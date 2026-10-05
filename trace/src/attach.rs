//! What every probe shares on the Rust side: loading its BPF object, choosing what's traced (`Target` →
//! `bpf/select.bpf.h`'s maps), keeping the traced processes' maps so their stacks can be named after they've
//! exited, and reading counters and stacks (`bpf/stacks.bpf.h`) back. A probe (`offcpu`, `syscalls`,
//! `ptywrite`) is its object plus what it reads at the end — the `Probe` trait.
//!
//! Pids are the tool's own pid namespace's (a container's, or the host's). The maps of a process are read
//! when the BPF side marks it seen (its `seen` map, polled) and every `REREAD` after.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use libbpf_rs::{Link, Map, MapCore, MapFlags, Object, ObjectBuilder};

use crate::maps::{self, Region};
use crate::procs;
use crate::symbols::Symbols;

pub const COMM_LEN: usize = 16;
/// Frames per stack (`MAX_DEPTH` in stacks.bpf.h).
const MAX_DEPTH: usize = 64;
/// A stack id below this is the error `bpf_get_stack` gave instead.
const NO_STACK: u64 = 4096;
/// How often the maps of the processes seen are read again (libraries loaded since).
const REREAD: Duration = Duration::from_secs(1);
/// How often `run_until` looks for new processes and asks whether to stop.
pub const POLL: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub enum Error {
    /// No CAP_BPF/CAP_PERFMON (or a seccomp/LSM refusal) — a test skips on it.
    NotPermitted(String),
    Other(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::NotPermitted(s) => {
                write!(f, "not permitted — run it as root (sudo) or with CAP_BPF and CAP_PERFMON: {s}")
            }
            Error::Other(s) => f.write_str(s),
        }
    }
}

pub fn bpf(what: &'static str) -> impl FnOnce(libbpf_rs::Error) -> Error {
    move |e| {
        let s = format!("{what}: {e}");
        if s.contains("Operation not permitted") || s.contains("ermission denied") {
            Error::NotPermitted(s)
        } else {
            Error::Other(s)
        }
    }
}

/// What to trace. Pids are in the tool's pid namespace.
#[derive(Clone, Debug, Default)]
pub struct Target {
    pub pids: Vec<u32>,
    /// Every process whose group leader has this comm (1–15 bytes).
    pub comm: Option<String>,
    /// The processes the traced ones start too — those running at attach and every later fork.
    pub children: bool,
}

/// A probe: attached for a target, polled while it runs, read when it ends.
pub trait Probe: Sized {
    /// Its name on the command line and in its output's file names.
    const NAME: &'static str;
    type Report: crate::Output;
    fn attach(target: &Target) -> Result<Self, Error>;
    fn attached(&mut self) -> &mut Attached;
    /// Detaches and reads what was measured.
    fn finish(self, name: &str) -> Result<Self::Report, Error>;

    /// Traces `pid` too — a command started after the attach.
    fn add(&mut self, pid: u32) -> Result<(), Error> {
        self.attached().add(pid)
    }

    /// Polls every `POLL` until `done` says to stop.
    fn run_until(&mut self, done: impl FnMut() -> bool) {
        self.attached().run_until(done)
    }

    /// Keeps the callers' user stacks of these syscalls (`--stacks`) — the syscall probe's; the others say so.
    fn keep_stacks_for(&mut self, syscalls: &[String]) -> Result<(), Error> {
        match syscalls {
            [] => Ok(()),
            _ => Err(Error::Other(format!("--stacks is the syscalls probe's, not {}'s", Self::NAME))),
        }
    }
}

pub fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
}

pub fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(b[at..at + 8].try_into().unwrap())
}

/// A NUL-padded comm.
pub fn comm(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

/// The `settings` map's one value (`struct sel_config`): the comm, the pid namespace's device and inode,
/// whether children are followed.
fn config(target: &Target) -> Result<Vec<u8>, Error> {
    let name = target.comm.as_deref().unwrap_or("");
    if name.len() >= COMM_LEN || target.comm.is_some() && name.is_empty() {
        return Err(Error::Other(format!("a comm is 1–15 bytes: {name:?}")));
    }
    let ns =
        std::fs::metadata("/proc/self/ns/pid").map_err(|e| Error::Other(format!("pid namespace: {e}")))?;
    let mut c = vec![0u8; COMM_LEN];
    c[..name.len()].copy_from_slice(name.as_bytes());
    c.extend(ns.dev().to_ne_bytes());
    c.extend(ns.ino().to_ne_bytes());
    c.extend(u32::from(target.children).to_ne_bytes());
    c.extend(0u32.to_ne_bytes());
    Ok(c)
}

/// What's kept of a traced process to name its stacks once it's gone.
#[derive(Default)]
struct Proc {
    /// Its binary (`/proc/<pid>/exe`) — frames in it may be the program's own.
    exe: Option<PathBuf>,
    regions: Vec<Region>,
}

/// Reads `tgid`'s maps over what was kept of it (gone: what was kept stays).
fn read_maps(kept: &mut HashMap<u32, Proc>, tgid: u32) {
    if let Ok(text) = std::fs::read_to_string(format!("/proc/{tgid}/maps")) {
        let p = kept.entry(tgid).or_default();
        p.regions = maps::merge(&p.regions, maps::parse(&text));
        if let Ok(exe) = std::fs::read_link(format!("/proc/{tgid}/exe")) {
            p.exe = Some(exe);
        }
    }
}

/// CLOCK_MONOTONIC in ns — `bpf_ktime_get_ns`'s clock.
pub fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: fills the timespec given.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// A probe's object, attached.
pub struct Attached {
    pub obj: Object,
    links: Vec<Link>,
    kept: HashMap<u32, Proc>,
    /// Processes from `seen` already read once.
    known: HashSet<u32>,
    t0: Instant,
    read_at: Instant,
}

impl Attached {
    /// Loads `object` and attaches its programs for `target`; with `children`, the processes its pids have
    /// started so far are seeded too. An empty target traces nothing until `add`.
    pub fn attach(object: &[u8], target: &Target) -> Result<Attached, Error> {
        let cfg = config(target)?;
        let mut builder = ObjectBuilder::default();
        let open = builder.open_memory(object).map_err(bpf("opening the BPF object"))?;
        let obj = open.load().map_err(bpf("loading the BPF object"))?;
        let mut a = Attached {
            obj,
            links: Vec::new(),
            kept: HashMap::new(),
            known: HashSet::new(),
            t0: Instant::now(),
            read_at: Instant::now(),
        };
        a.map("settings")?.update(&0u32.to_ne_bytes(), &cfg, MapFlags::ANY).map_err(bpf("settings"))?;
        let mut seeds = target.pids.clone();
        if target.children {
            let all = procs::all();
            for &p in &target.pids {
                seeds.extend(procs::descendants(&all, p));
            }
        }
        for p in seeds {
            a.add(p)?;
        }
        for p in a.obj.progs_mut() {
            // (the program is said: a kernel whose function is missing or inlined fails here)
            let what = format!("attaching {}", p.name().to_string_lossy());
            a.links.push(p.attach().map_err(|e| match bpf("attaching")(e) {
                Error::Other(s) => Error::Other(s.replacen("attaching", &what, 1)),
                e => e,
            })?);
        }
        a.t0 = Instant::now();
        Ok(a)
    }

    pub fn map(&self, name: &str) -> Result<Map<'_>, Error> {
        self.obj.maps().find(|m| m.name() == name).ok_or_else(|| Error::Other(format!("no map {name}")))
    }

    /// Traces `pid` (in the tool's namespace) too — its maps read now: a process that lives a few ms (a
    /// burst of calls, then exit) would be gone by the next poll, its stacks left unnamed. Read again when
    /// the BPF side first sees it (not marked `known` here): a process added right after its exec hasn't
    /// mapped its libraries yet (measured on x86_64: the fixture's libc frames came out unnamed).
    pub fn add(&mut self, pid: u32) -> Result<(), Error> {
        self.map("seeds")?.update(&pid.to_ne_bytes(), &[1], MapFlags::ANY).map_err(bpf("seeds"))?;
        read_maps(&mut self.kept, pid);
        Ok(())
    }

    /// Reads the maps of the processes seen since the last poll — and of all of them every `REREAD`.
    pub fn poll(&mut self) {
        let Ok(seen) = self.map("seen") else { return };
        let new: Vec<u32> = seen
            .keys()
            .filter(|k| k.len() == 4)
            .map(|k| u32_at(&k, 0))
            .filter(|p| !self.known.contains(p))
            .collect();
        for tgid in new {
            self.known.insert(tgid);
            read_maps(&mut self.kept, tgid);
        }
        if self.read_at.elapsed() >= REREAD {
            for tgid in self.kept.keys().copied().collect::<Vec<_>>() {
                read_maps(&mut self.kept, tgid);
            }
            self.read_at = Instant::now();
        }
    }

    /// Polls every `POLL` until `done` says to stop.
    pub fn run_until(&mut self, mut done: impl FnMut() -> bool) {
        loop {
            std::thread::sleep(POLL);
            self.poll();
            if done() {
                break;
            }
        }
    }

    /// Detaches — the totals stop moving while they're read: the seconds traced, and the clock then.
    pub fn detach(&mut self) -> (f64, u64) {
        let secs = self.t0.elapsed().as_secs_f64();
        self.links.clear();
        let now = monotonic_ns();
        self.poll();
        (secs, now)
    }

    /// An array map of u64 counters, `n` of them (0 for one it doesn't have).
    pub fn counters(&self, name: &str, n: u32) -> Vec<u64> {
        let Ok(m) = self.map(name) else { return vec![0; n as usize] };
        (0..n)
            .map(|i| m.lookup(&i.to_ne_bytes(), MapFlags::ANY).ok().flatten().map_or(0, |v| u64_at(&v, 0)))
            .collect()
    }

    /// The probe's own losses (`errors`, labelled in its order), the shared ones (`tracked` and `stacks`
    /// full), and the switches of the comm in other pid namespaces.
    pub fn losses(&self, own: &[&str]) -> (BTreeMap<String, u64>, u64) {
        let mut lost: BTreeMap<String, u64> = own
            .iter()
            .zip(self.counters("errors", own.len() as u32))
            .map(|(l, n)| (l.to_string(), n))
            .collect();
        let sel = self.counters("sel_errors", 2);
        lost.insert("tracked table full".into(), sel[0]);
        if self.map("stack_errors").is_ok() {
            lost.insert("stack table full".into(), self.counters("stack_errors", 1)[0]);
        }
        (lost, sel[1])
    }

    /// Names stacks (by their ids in `stacks`) for the probe's report.
    pub fn namer(&self) -> Namer<'_> {
        Namer { a: self, stacks: self.map("stacks").ok(), addrs: HashMap::new(), symbols: Symbols::new() }
    }
}

pub struct Namer<'a> {
    a: &'a Attached,
    stacks: Option<Map<'a>>,
    addrs: HashMap<u64, Vec<u64>>,
    symbols: Symbols,
}

impl Namer<'_> {
    fn addrs(&mut self, id: u64) -> Result<Vec<u64>, String> {
        if id < NO_STACK {
            // EFAULT: no user stack (a kernel thread, a task between exec and its first instruction)
            return Err(format!("[no stack: errno {id}]"));
        }
        if let Some(a) = self.addrs.get(&id) {
            return Ok(a.clone());
        }
        let b = self.stacks.as_ref().and_then(|m| m.lookup(&id.to_ne_bytes(), MapFlags::ANY).ok().flatten());
        let Some(b) = b else { return Err("[stack lost: table full]".into()) };
        let a: Vec<u64> =
            (0..b.len().min(MAX_DEPTH * 8) / 8).map(|i| u64_at(&b, i * 8)).take_while(|&a| a != 0).collect();
        self.addrs.insert(id, a.clone());
        Ok(a)
    }

    /// A user stack of process `tgid`, root first, and which frames are its own code.
    pub fn user(&mut self, tgid: u32, id: u64) -> (Vec<String>, Vec<bool>) {
        match self.addrs(id) {
            Ok(a) => {
                let p = self.a.kept.get(&tgid);
                self.symbols.user(p.map(|p| p.regions.as_slice()), p.and_then(|p| p.exe.as_deref()), &a)
            }
            Err(why) => (vec![why], vec![false]),
        }
    }

    /// A kernel stack, root first.
    pub fn kernel(&mut self, id: u64) -> Vec<String> {
        match self.addrs(id) {
            Ok(a) => self.symbols.kernel(&a),
            Err(why) => vec![why],
        }
    }
}

/// The running kernel's release.
pub fn kernel() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default().trim().to_string()
}
