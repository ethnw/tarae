//! A session's keeper (`docs/design/sandbox-keeper.md` §The keeper): a small process per session, `tarae
//! --sandbox-keeper`, that forks each sandboxed run of its session, reaps it and says how it ended, and is the
//! child subreaper of what the runs leave — so what a run leaves running is the session's process tree, not a
//! tag (`env -i` can't shed it), and ends with it. Where user namespaces work (`tier.rs`) it's born in the
//! session's own — user, ipc, mount, pid where a fresh `/proc` mounts (then it's their PID 1), net where the
//! policy opens nothing — with the user's own ids (D5), its own `/proc`, `/dev/pts` and `/dev/shm` (D10), `lo`
//! up, and every capability dropped before its exec; without, a plain fork server (tiers C/D).
//!
//! tarae's side: `ready` (on the worker that makes a run's policy — `sandbox::confined`) finds the session's
//! keeper or makes one (`born`: `clone3(CLONE_PIDFD | namespaces)`, tarae writing its id maps, raw calls until
//! the exec), `Keeper::run` asks it for a run and waits for `Started` (the main loop's wait: at most
//! `STARTED_WITHIN`, as it waited for `posix_spawn` before), and the two places every run is waited on and
//! stopped — `spawn::wait` and `dap::term_group` — ask here first (`wait`, `signal`): a keeper's run isn't
//! tarae's child, and only its reaper can signal it without a reused pid's race. Its channel is `channel.rs`'s
//! protocol; a thread reads it, another waits on its pidfd. A run's pid is tarae's number: in a PID namespace
//! the keeper says its own, so tarae reads its own from the run's pidfd while the run waits for it (`held`).
//!
//! The keeper's side (`main`) is single-threaded: non-dumpable, `no_new_privs`, a child subreaper, a session
//! of its own, every signal at its default and `SIGCHLD` blocked from its birth (read through a `signalfd`, so
//! no handler or default ever runs), its environment empty, holding only its channel and `/dev/null` as stdio.
//! It holds no Landlock or seccomp of its own: nothing of tarae's is in it to take, no run can trace it or read
//! its `/proc` (non-dumpable, and Landlock's ptrace rule), from ABI 6 none can signal it (Landlock's scope), and
//! below 6 a run that kills it only fails closed — its runs fail, what they left ends, tarae says so.

use std::collections::HashMap;
use std::ffi::CString;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use super::channel::{self, Msg, Step};
use super::tier::{BirthError, BirthStep, Ns, RunNet};

/// The flag tarae's `main` starts a keeper with.
pub const FLAG: &str = "--sandbox-keeper";
/// The keeper's end of its channel, as the keeper has it.
const CHANNEL_FD: RawFd = 3;
/// Where the birth leaves the raw ICMP socket it made for the session's network (`Birth::child`), for the keeper to
/// hand tarae with `Planted` (`birth_raw`).
const RAW_FD: RawFd = 4;
/// How long a new keeper has to say it's there.
const READY_WITHIN: Duration = Duration::from_secs(2);
/// How long a run's start may take — the main loop waits for it.
const STARTED_WITHIN: Duration = Duration::from_secs(1);
/// What the keeper gives what it ends for TERM, before KILL.
const LEFT_TERM: Duration = Duration::from_secs(1);
/// How long a keeper told to end has (its `Kept::end_all`'s rounds of TERM and KILL, `LEFT_TERM` each, and a
/// second more) before KILL.
const END_WITHIN: Duration = Duration::from_secs(5);
/// The device places a keeper hands tarae with `Ready` (its own, in its mount namespace): its `devpts`, its
/// `ptmx` there and at `/dev/ptmx` where that's a node bound over (not a link), its `/dev/shm`.
pub const DEVICE_HANDLES: [&str; 4] = ["/dev/pts", "/dev/pts/ptmx", "/dev/ptmx", "/dev/shm"];
/// D11: a session's keeper that ended this many times within `DEATHS_WITHIN` isn't made again.
const DEATHS_MAX: usize = 2;
const DEATHS_WITHIN: Duration = Duration::from_secs(600);

// ── tarae's side ─────────────────────────────────────────────────────────────

/// A session's keeper, as tarae holds it.
pub struct Keeper {
    pub pid: u32,
    pub root: PathBuf,
    /// The namespaces it was born with.
    pub ns: Ns,
    /// Its own device places (`DEVICE_HANDLES`), as it handed them with `Ready`: what its runs' rules grant
    /// the devices on when it's in a mount namespace of its own (`linux::confine`).
    pub devices: Vec<(&'static str, OwnedFd)>,
    /// The families whose address its `lo` was to carry (`Families`) but couldn't take — said with it (`said`).
    pub lo_missing: Families,
    /// Its session's network (`redirect`), where it was born with one: let go when it's gone — the owned table
    /// with tarae's socket (P4), and its netns with the sockets that hold it.
    pub network: Mutex<Option<super::redirect::Made>>,
    /// 6g: where its runs' own cgroups are, or why they have none — None: not asked (no TPROXY network, or
    /// `tier::Probed.cgroups` says why for every keeper).
    pub runs_in: Option<Result<RunsIn, String>>,
    /// Tests: what its birth's read-only step came to (`read_only`).
    #[cfg(test)]
    cgroupfs: Result<(), String>,
    chan: OwnedFd,
    pidfd: OwnedFd,
    state: Mutex<State>,
    changed: Condvar,
}

/// A keeper's scope (6g): each of its runs starts in a cgroup of its own right below it.
#[derive(Clone, Debug)]
pub struct RunsIn {
    /// The scope's cgroup, as tarae's namespace — the machine's — names it.
    pub scope: String,
    /// The level a `socket cgroupv2` rule names a run's cgroup at (`cgroup::run_level`).
    pub level: u32,
    source: super::cgroup::Source,
}

#[derive(Default)]
struct State {
    /// Why it's gone — None while it's there.
    gone: Option<String>,
    /// tarae asked it to end (`end`): its going isn't a death.
    ending: bool,
    /// A keeper of another policy took its place: no run more, and it ends once its last one has.
    superseded: bool,
    /// Its policy's key (`sandbox::policy_key`) — None: a probe's, any key takes it over.
    key: Option<String>,
    next: u64,
    runs: HashMap<u64, Slot>,
    /// `Planted` as it came — its kinds, its errno, its sockets — for `plant`, which asked.
    planted: Option<(Vec<u8>, i32, Vec<OwnedFd>)>,
    /// Tests (M3): what it said it uses.
    #[cfg(test)]
    usage: Option<(u64, u64)>,
    /// Tests: what its held runs' pidfds' fdinfo reads as, instead of `/proc`'s.
    #[cfg(test)]
    fdinfo: Option<&'static str>,
    /// Tests: how its runs ended, as it said (`Exited`), whether tarae still waited on them or not.
    #[cfg(test)]
    exits: Vec<i32>,
}

#[derive(Default)]
struct Slot {
    /// `Started`'s pid and pidfd — or why it didn't start.
    started: Option<Result<(u32, Arc<OwnedFd>), String>>,
    /// The id of its own cgroup (6g) — 0: it's in its keeper's.
    cgroup: u64,
    /// Its `wait` status — or why it has none (the keeper ended first).
    exited: Option<Result<i32, String>>,
    /// A held run's end of its handoff socket: a byte on it once its pid is known, and it goes on to its exec.
    hold: Option<Arc<OwnedFd>>,
}

/// Every keeper there is (a session's, and ones superseded still ending their runs). Held only to look or
/// change: `of` is the main loop's (`:procs`).
static KEEPERS: Mutex<Vec<Arc<Keeper>>> = Mutex::new(Vec::new());
/// Held through `ready` — a birth too (≤ `READY_WITHIN`): two runs' policies made at once get one keeper,
/// workers waiting on it, never `KEEPERS`'s readers.
static BIRTHS: Mutex<()> = Mutex::new(());
/// Tests: a birth for this root says it began and takes a second more (`a_birth_under_way_holds_up_no_reader`).
#[cfg(test)]
static SLOW_BIRTH: Mutex<Option<(PathBuf, std::sync::mpsc::Sender<()>)>> = Mutex::new(None);
/// When each session's keeper died (not asked to end).
static DEATHS: Mutex<Vec<(PathBuf, Instant)>> = Mutex::new(Vec::new());
/// The runs of every keeper by pid (tarae's numbering): its keeper and its id.
static RUNS: Mutex<Vec<(u32, Arc<Keeper>, u64)>> = Mutex::new(Vec::new());

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The keeper of the session at `root` for a policy keyed `key` (None: any — a probe's), born with `ns`
/// (`tier::for_policy`): the one there, else a new one. One of another key — or born with other namespaces —
/// is superseded: what its runs left ends now, and it ends after its last run (D14). Two deaths within
/// `DEATHS_WITHIN` and none is made (D11). A birth that fails fails the run, saying why — never another tier
/// (D-b: the probe's, until tarae starts again). Blocks (a worker's).
pub fn ready(root: &Path, key: Option<&str>, ns: Ns) -> Result<Arc<Keeper>, String> {
    let _births = lock(&BIRTHS);
    let current = {
        let mut keepers = lock(&KEEPERS);
        keepers.retain(|k| lock(&k.state).gone.is_none());
        keepers.iter().find(|k| k.root == root && !lock(&k.state).superseded).cloned()
    };
    if let Some(k) = current {
        let mut s = lock(&k.state);
        match (&s.key, key) {
            (_, None) => return Ok(k.clone()),
            (None, Some(new)) if k.ns == ns => {
                s.key = Some(new.to_string());
                return Ok(k.clone());
            }
            (Some(was), Some(new)) if was == new && k.ns == ns => return Ok(k.clone()),
            _ => {
                s.superseded = true;
                let idle = s.runs.values().all(|r| r.exited.is_some() || r.started.is_none());
                drop(s);
                let k = k.clone();
                crate::event::thread("tarae-kend", move || match idle {
                    true => k.end(),
                    false => {
                        crate::reaper::end(&k.leftovers(), LEFT_TERM);
                    }
                });
            }
        }
    }
    let now = Instant::now();
    let died =
        lock(&DEATHS).iter().filter(|(r, t)| r == root && now.duration_since(*t) < DEATHS_WITHIN).count();
    if died >= DEATHS_MAX {
        return Err(format!(
            "the session's keeper ended {died} times within {} minutes — tarae stopped making it",
            DEATHS_WITHIN.as_secs() / 60
        ));
    }
    #[cfg(test)]
    let slow = lock(&SLOW_BIRTH).as_ref().filter(|(r, _)| r == root).map(|(_, tx)| tx.clone());
    #[cfg(test)]
    if let Some(began) = slow {
        let _ = began.send(());
        std::thread::sleep(Duration::from_secs(1));
    }
    let k = Keeper::born(root, key, ns).map_err(|e| format!("the session's keeper couldn't start: {e}"))?;
    lock(&KEEPERS).push(k.clone());
    Ok(k)
}

/// A keeper for the probe (`tier.rs`): born with `ns` — and the session's network, `redirect` (the probe can't ask
/// itself whether there is one) — kept by nobody but the caller.
pub(super) fn born_for_probe(root: &Path, ns: Ns, redirect: bool) -> io::Result<Arc<Keeper>> {
    let network = (redirect && ns.netns()).then_some(NetworkFor::Session);
    Keeper::born_lo(root, None, ns, Families::here(), false, network, None)
}

/// The network a keeper born with `ns` gets: the session's, where it has a netns and the probe's landed (`probed`,
/// D-b: decided once — `tier::Probed.redirect`) — else none (until 6f its runs go out by the swap).
pub(super) fn network_for(ns: Ns, probed: &Result<(), String>) -> Option<NetworkFor> {
    (ns.netns() && probed.is_ok()).then_some(NetworkFor::Session)
}

/// What network a keeper's birth makes: the session's — for the families its `lo` carries, known once it's ready
/// (`redirect::Network::session`) — or, tests, one given whole.
pub(super) enum NetworkFor {
    Session,
    #[cfg(test)]
    Given(super::redirect::Network),
}

/// What a keeper's `Planted` (`kinds`, `errno`) gives for a `Plant` of `families`: each socket's kind, exactly
/// `redirect::Kind::wanted`'s in its order — and the raw ICMP socket besides, once, where the birth made one — else
/// why not, as an errno: the keeper's own, or `EPROTO` (a kind missing, one extra, one twice). tarae never takes a
/// socket for a kind it wasn't asked as.
fn planted_kinds(families: Families, kinds: &[u8], errno: i32) -> Result<Vec<super::redirect::Kind>, i32> {
    use super::redirect::Kind;
    if errno != 0 {
        return Err(errno);
    }
    let got: Vec<Kind> =
        kinds.iter().map(|k| Kind::from_code(*k).ok_or(libc::EPROTO)).collect::<Result<_, _>>()?;
    let asked: Vec<Kind> = got.iter().copied().filter(|k| *k != Kind::Icmp4).collect();
    let raw = got.len() - asked.len();
    if asked != Kind::wanted(families.v4, families.v6) || raw > 1 {
        return Err(libc::EPROTO);
    }
    Ok(got)
}

/// The keepers of the session at `root` (None: every session's).
pub fn of(root: Option<&Path>) -> Vec<Arc<Keeper>> {
    lock(&KEEPERS).iter().filter(|k| root.is_none_or(|r| k.root == r)).cloned().collect()
}

/// Ends `keepers` together (`Keeper::end` of each — all told first, then one deadline for all): tarae's quit
/// with many sessions waits `END_WITHIN` at most, not that many times over. A worker's, or tarae's end.
pub fn end_together(keepers: &[Arc<Keeper>]) {
    for k in keepers {
        k.ask_end();
    }
    let deadline = Instant::now() + END_WITHIN;
    for k in keepers {
        k.ended_by(deadline);
    }
}

/// What `pid` is to a keeper: its keeper and its id — None: not a keeper's run.
fn run_of(pid: u32) -> Option<(Arc<Keeper>, u64)> {
    lock(&RUNS).iter().rev().find(|(p, ..)| *p == pid).map(|(_, k, id)| (k.clone(), *id))
}

/// `spawn::wait` for a keeper's run: how it ended (an error: the keeper ended before it said) — None: `pid`
/// isn't one (it's waited for as tarae's own child).
pub fn wait(pid: u32) -> Option<io::Result<std::process::ExitStatus>> {
    use std::os::unix::process::ExitStatusExt;
    let (k, id) = run_of(pid)?;
    let mut s = lock(&k.state);
    loop {
        if let Some(how) = s.runs.get(&id).and_then(|r| r.exited.clone()) {
            s.runs.remove(&id);
            drop(s);
            lock(&RUNS).retain(|(p, _, i)| !(*p == pid && *i == id));
            return Some(how.map(ExitStatusExt::from_raw).map_err(io::Error::other));
        }
        if !s.runs.contains_key(&id) {
            return Some(Err(io::Error::other("waited for twice")));
        }
        s = k.changed.wait(s).unwrap_or_else(|e| e.into_inner());
    }
}

/// `dap::term_group` for a keeper's run: `sig` to its process group, by its keeper — only while it hasn't
/// reaped it (after, nothing: the group may be another's). False: `pid` isn't a keeper's run.
pub fn signal(pid: u32, sig: i32) -> bool {
    let Some((k, id)) = run_of(pid) else { return false };
    let reaped = lock(&k.state).runs.get(&id).is_none_or(|r| r.exited.is_some());
    if !reaped {
        let _ = channel::send(k.chan.as_fd(), &Msg::Signal { id, sig }, &[]);
    }
    true
}

impl Keeper {
    /// A new keeper for the session at `root`, in the namespaces `ns` says: `clone3` (its pidfd from the start),
    /// its id maps written by tarae (D5), then the child's raw calls — with namespaces its mounts, `lo` and
    /// every capability dropped (`Birth::child`); stdio to `/dev/null`, its channel to fd 3, `SIGCHLD` blocked
    /// (so through the exec: no thread of the new process takes it before its `signalfd` does) — then the exec
    /// of tarae's own binary, with an empty environment. Ready once it says so (`READY_WITHIN`); a step that
    /// failed before is the error (`BirthError` inside it). Its `lo` mirrors tarae's network's families
    /// (`Families::here`). With a network namespace, where the probe's network landed (`tier::Probed.redirect`), it
    /// gets the session's network before it's ready (`redirect`): its sockets planted, checked and sent on — any
    /// refusal its birth's error, the keeper and its netns thrown away.
    fn born(root: &Path, key: Option<&str>, ns: Ns) -> io::Result<Arc<Keeper>> {
        Keeper::born_with(root, key, ns, Families::here())
    }

    /// `born`, its `lo` carrying `families`' addresses. With the session's network, a scope of its own where this
    /// tarae's keepers get one (6g, `tier::Probed.cgroups`).
    fn born_with(root: &Path, key: Option<&str>, ns: Ns, families: Families) -> io::Result<Arc<Keeper>> {
        let probed = super::tier::get();
        let network = network_for(ns, &probed.redirect);
        let cgroups = network.as_ref().and(probed.cgroups.clone().ok());
        Keeper::born_lo(root, key, ns, families, false, network, cgroups)
    }

    /// `born_with` — tests: the mirrors put on a device that isn't there (`off_lo`), the network another one
    /// (None: none), the scope from where tests say (`cgroups`, None: none).
    fn born_lo(
        root: &Path,
        key: Option<&str>,
        ns: Ns,
        families: Families,
        off_lo: bool,
        network: Option<NetworkFor>,
        cgroups: Option<super::cgroup::Source>,
    ) -> io::Result<Arc<Keeper>> {
        let exe = exe()?;
        let (ours, theirs) = seqpacket()?;
        let null = open_null(libc::O_RDWR)?;
        let birth = Birth::new(families, off_lo, network.is_some() && ns.netns())?;
        #[cfg(test)]
        let birth = {
            let seam = lock(&CGROUPFS_SEAMS).iter().find(|(r, _)| r == root).map(|(_, s)| *s);
            Birth { seam, ..birth }
        };
        let mut lo_missing = Families::default();
        let mut cgroupfs = None;
        let argv: Vec<CString> = keeper_argv();
        let argv_p: Vec<*const libc::c_char> =
            argv.iter().map(|a| a.as_ptr()).chain(std::iter::once(std::ptr::null())).collect();
        let envs: Vec<CString> = keeper_env();
        let envp: Vec<*const libc::c_char> =
            envs.iter().map(|e| e.as_ptr()).chain(std::iter::once(std::ptr::null())).collect();
        let empty = c"";
        let mut pidfd: libc::c_int = -1;
        // SAFETY: in the child — a copy of a multithreaded process — only raw calls on descriptors and buffers
        // made above, then the exec or `_exit`.
        let pid = unsafe { clone_pidfd(&mut pidfd, ns.clone_flags()) };
        if pid == 0 {
            // SAFETY: as above — async-signal-safe calls only.
            unsafe {
                let raw = birth.child(ns);
                for fd in 0..3 {
                    libc::dup2(null.as_raw_fd(), fd);
                }
                if theirs.as_raw_fd() == CHANNEL_FD {
                    libc::fcntl(CHANNEL_FD, libc::F_SETFD, 0);
                } else {
                    libc::dup2(theirs.as_raw_fd(), CHANNEL_FD);
                }
                // (after the channel's: whatever sat at 4 — its old place among them — isn't needed past here)
                if raw >= 0 {
                    libc::dup2(raw, RAW_FD);
                    libc::close(raw);
                }
                let mut mask: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut mask);
                libc::sigaddset(&mut mask, libc::SIGCHLD);
                libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut());
                libc::syscall(
                    libc::SYS_execveat,
                    exe.as_raw_fd(),
                    empty.as_ptr(),
                    argv_p.as_ptr(),
                    envp.as_ptr(),
                    libc::AT_EMPTY_PATH,
                );
                birth.failed(BirthStep::Exec, errno());
            }
        }
        if pid < 0 {
            return Err(BirthError { step: BirthStep::Clone, errno: errno() }.into());
        }
        // SAFETY: the pidfd `clone3` made for us.
        let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
        drop(theirs);
        let (go, told) = birth.parents();
        let ready = (|| {
            if ns.user {
                id_maps(pid as i32)?;
            }
            // SAFETY: one byte written from a buffer we own, on our end of the go pipe.
            if unsafe { libc::write(go.as_raw_fd(), [1u8].as_ptr().cast(), 1) } != 1 {
                return Err(BirthError { step: BirthStep::Go, errno: errno() }.into());
            }
            said_ready(ours.as_fd(), told, &mut lo_missing, &mut cgroupfs)
        })();
        let (ready, devices) = match ready {
            Ok(devices) => (Ok(()), devices),
            Err(e) => (Err(e), Vec::new()),
        };
        // (6g: in a scope of its own before any run, its folder handed over — or why not, its runs in its cgroup;
        // only where its birth made /sys/fs/cgroup read-only: else a run could move itself between the scope's
        // cgroups, all the user's)
        let runs_in = cgroups.filter(|_| ready.is_ok()).map(|source| -> Result<RunsIn, String> {
            read_only(ns, cgroupfs)
                .map_err(|why| format!("{why} — a run could move itself between cgroups"))?;
            let scope = super::cgroup::place(&source, pid as u32)?;
            channel::send(ours.as_fd(), &Msg::Cgroup, &[scope.dir.as_fd()])
                .map_err(|e| format!("handing the keeper its scope: {e}"))?;
            Ok(RunsIn { scope: scope.path, level: scope.level, source })
        });
        let k = Arc::new(Keeper {
            pid: pid as u32,
            root: root.to_path_buf(),
            ns,
            devices,
            lo_missing,
            network: Mutex::new(None),
            runs_in,
            #[cfg(test)]
            cgroupfs: read_only(ns, cgroupfs),
            chan: ours,
            pidfd,
            state: Mutex::new(State { key: key.map(str::to_string), ..State::default() }),
            changed: Condvar::new(),
        });
        let waiting = k.clone();
        crate::event::thread("tarae-kwait", move || waiting.wait_gone());
        if let Err(e) = ready {
            // SAFETY: a signal to the keeper we just made (its pidfd: no other process).
            unsafe { libc::syscall(libc::SYS_pidfd_send_signal, k.pidfd.as_raw_fd(), libc::SIGKILL, 0, 0) };
            lock(&k.state).ending = true;
            return Err(e);
        }
        let reading = k.clone();
        crate::event::thread("tarae-keeper", move || reading.read());
        if let Some(network) = network {
            let carried =
                Families { v4: families.v4 && !k.lo_missing.v4, v6: families.v6 && !k.lo_missing.v6 };
            let network = match network {
                NetworkFor::Session => super::redirect::Network::session(carried.v4, carried.v6),
                #[cfg(test)]
                NetworkFor::Given(network) => network,
            };
            let made = k.plant(carried).and_then(|handed| {
                let session = super::redirect::netns_of_pid(k.pid).map_err(|e| BirthError {
                    step: BirthStep::Netns,
                    errno: e.raw_os_error().unwrap_or(libc::EIO),
                })?;
                super::redirect::make(session, handed, &network)
            });
            match made {
                Ok(made) => *lock(&k.network) = Some(made),
                // (never a retry: a half-made network is never a session's — `redirect.rs`)
                Err(e) => {
                    crate::log::line(
                        "sandbox",
                        "warn",
                        &format!(
                            "a keeper for {}: its network failed ({e}) — keeper {} killed, its netns with it",
                            root.display(),
                            k.pid
                        ),
                    );
                    k.thrown_away();
                    return Err(e.into());
                }
            }
        }
        let runs = match &k.runs_in {
            None => String::new(),
            Some(Ok(r)) => format!("; runs in cgroups of their own in {} (level {})", r.scope, r.level),
            Some(Err(why)) => format!("; runs in its cgroup — no scope of its own: {why}"),
        };
        crate::log::line(
            "sandbox",
            "info",
            &format!("a keeper for {}: pid {}, {}{runs}", root.display(), k.pid, k.said()),
        );
        Ok(k)
    }

    /// Its tier and namespaces in words (`:procs`, the log): `A+ (user net ipc pid mnt)`, and why its network
    /// is this machine's (D-a: served, this kernel can't swap a socket in), and a mirror its `lo` couldn't take
    /// (`lo_missing`). Not `tier::get`'s: the probe's own keeper says this while the probe is under way.
    pub fn said(&self) -> String {
        let served = super::supervisor::unavailable().is_none();
        let tier = super::tier::Tier::of(self.ns, served).said();
        let missing: Vec<&str> = [(self.lo_missing.v4, "IPv4"), (self.lo_missing.v6, "IPv6")]
            .into_iter()
            .filter_map(|(m, f)| m.then_some(f))
            .collect();
        let lo = match missing.is_empty() {
            true => String::new(),
            false => format!(
                "; its lo without an {} address: lookups of that family alone find nothing",
                missing.join(" or ")
            ),
        };
        match (self.ns.user && !self.ns.net && served, super::supervisor::swap_unavailable()) {
            (true, Some(why)) => {
                format!("{tier} ({}; network shared: its policy opens it — {why})", self.ns.said())
            }
            _ => format!("{tier} ({}{lo})", self.ns.said()),
        }
    }

    /// Reads what the keeper says, until its channel ends.
    fn read(self: Arc<Self>) {
        let mut buf = Vec::new();
        loop {
            match channel::recv(self.chan.as_fd(), &mut buf) {
                Ok(Some((Ok(msg), fds))) => self.heard(msg, fds),
                Ok(Some((Err(e), _))) => crate::log::line("sandbox", "warn", &format!("its keeper: {e}")),
                Ok(None) | Err(_) => return self.gone("its channel closed".into()),
            }
        }
    }

    fn heard(self: &Arc<Self>, msg: Msg, mut fds: Vec<OwnedFd>) {
        let mut s = lock(&self.state);
        match msg {
            Msg::Started { id, pid, cgroup, missed } => {
                // (D5: a run that couldn't have its own cgroup started in its keeper's — said, not refused)
                if let Some((step, errno)) = missed {
                    crate::log::line(
                        "sandbox",
                        "info",
                        &format!(
                            "a run (keeper {}) started in its keeper's cgroup — {}: {}",
                            self.pid,
                            step.said(),
                            io::Error::from_raw_os_error(errno)
                        ),
                    );
                }
                if let Some(slot) = s.runs.get_mut(&id) {
                    slot.cgroup = cgroup;
                }
                let pidfd = fds.pop();
                // (in a PID namespace its number is the keeper's: tarae's own from its pidfd, read while it
                // waits for its byte)
                let number = match (&pidfd, self.ns.pid) {
                    (Some(fd), true) => self.number(&s, fd),
                    _ => Ok(pid),
                };
                match (s.runs.get_mut(&id), pidfd) {
                    (Some(slot), Some(pidfd)) => match number {
                        Ok(pid) => {
                            slot.started = Some(Ok((pid, Arc::new(pidfd))));
                            lock(&RUNS).push((pid, self.clone(), id));
                            if let Some(hold) = slot.hold.take() {
                                // SAFETY: one byte from a buffer we own, on our end of its handoff socket.
                                unsafe {
                                    libc::send(hold.as_raw_fd(), [1u8].as_ptr().cast(), 1, libc::MSG_NOSIGNAL)
                                };
                            }
                        }
                        // (one tarae can't number it couldn't wait on or end: its byte never goes, it's killed
                        // where it waits — gone already, it failed: its `Failed` or `Exited` says so)
                        Err(why) => {
                            // SAFETY: KILL through the run's pidfd, ours (no other process can be it).
                            unsafe {
                                libc::syscall(
                                    libc::SYS_pidfd_send_signal,
                                    pidfd.as_raw_fd(),
                                    libc::SIGKILL,
                                    0,
                                    0,
                                )
                            };
                            if let Some(why) = why {
                                slot.started = Some(Err(why));
                            }
                        }
                    },
                    // (one whose start tarae gave up on: it goes)
                    _ => {
                        let _ =
                            channel::send(self.chan.as_fd(), &Msg::Signal { id, sig: libc::SIGKILL }, &[]);
                    }
                }
            }
            Msg::Failed { id, errno, step } => {
                let why = format!("{}: {}", step.said(), io::Error::from_raw_os_error(errno));
                match s.runs.get_mut(&id) {
                    Some(slot) if slot.started.is_none() => slot.started = Some(Err(why)),
                    _ => crate::log::line(
                        "sandbox",
                        "info",
                        &format!("a run (keeper {}) failed at {why}", self.pid),
                    ),
                }
            }
            Msg::Exited { id, status } => {
                #[cfg(test)]
                s.exits.push(status);
                if let Some(slot) = s.runs.get_mut(&id) {
                    slot.exited = Some(Ok(status));
                    if slot.started.is_none() {
                        slot.started = Some(Err(format!("it ended before it started ({status})")));
                    }
                }
                let idle = s.runs.values().all(|r| r.exited.is_some());
                if s.superseded && idle && !s.ending {
                    let k = self.clone();
                    crate::event::thread("tarae-kend", move || k.end());
                }
            }
            Msg::Reaped { n } => {
                crate::log::line("sandbox", "info", &format!("keeper {} reaped {n} left by runs", self.pid));
            }
            #[cfg(test)]
            Msg::Usage { rss_kb, pss_kb } => s.usage = Some((rss_kb, pss_kb)),
            // (handed before any run, or not taken: a socket of the session's network comes from its keeper
            // before a run could be in it — P5)
            Msg::Planted { kinds, errno } if s.next == 0 => s.planted = Some((kinds, errno, fds)),
            Msg::Planted { .. } => crate::log::line(
                "sandbox",
                "warn",
                &format!("keeper {} handed sockets after a run — refused", self.pid),
            ),
            other => crate::log::line("sandbox", "warn", &format!("its keeper said {other:?}")),
        }
        drop(s);
        self.changed.notify_all();
    }

    /// tarae's number for a held run, from its pidfd (`pid_of`; in tests what `State.fdinfo` says, if set).
    fn number(&self, #[allow(unused_variables)] s: &State, pidfd: &OwnedFd) -> Result<u32, Option<String>> {
        #[cfg(test)]
        if let Some(info) = s.fdinfo {
            return pid_in_fdinfo(info);
        }
        pid_of(pidfd)
    }

    /// The keeper killed (`SIGKILL` through its pidfd: no other process can be it) — its going no death (`ending`),
    /// its netns gone with the last socket in it.
    fn thrown_away(&self) {
        lock(&self.state).ending = true;
        // SAFETY: a signal through the pidfd of the keeper we made.
        unsafe { libc::syscall(libc::SYS_pidfd_send_signal, self.pidfd.as_raw_fd(), libc::SIGKILL, 0, 0) };
    }

    /// Asks the keeper for the sockets its session's network is made with (`Msg::Plant`: its `lo` carrying
    /// `families`' addresses) and waits for them (`READY_WITHIN`): each its kind, exactly `redirect::Kind::wanted`'s
    /// (and the birth's raw ICMP socket, the first time, where it made one), or the birth's error (`Planted`, with
    /// the keeper's errno). Before any run: `heard` takes none after.
    pub(super) fn plant(
        &self,
        families: Families,
    ) -> Result<Vec<(super::redirect::Kind, OwnedFd)>, BirthError> {
        let failed = |errno| BirthError { step: BirthStep::Planted, errno };
        lock(&self.state).planted = None;
        let ask = Msg::Plant { v4: families.v4, v6: families.v6 };
        channel::send(self.chan.as_fd(), &ask, &[])
            .map_err(|e| failed(e.raw_os_error().unwrap_or(libc::EIO)))?;
        let deadline = Instant::now() + READY_WITHIN;
        let mut s = lock(&self.state);
        let (kinds, errno, fds) = loop {
            if let Some(p) = s.planted.take() {
                break p;
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match (s.gone.is_some(), left.is_zero()) {
                (true, _) => return Err(failed(libc::ECHILD)),
                (_, true) => return Err(failed(libc::ETIMEDOUT)),
                _ => s = self.changed.wait_timeout(s, left).unwrap_or_else(|e| e.into_inner()).0,
            }
        };
        drop(s);
        let got = planted_kinds(families, &kinds, errno).map_err(failed)?;
        Ok(got.into_iter().zip(fds).collect())
    }

    /// Waits for the keeper's process to end (and reaps it).
    fn wait_gone(self: Arc<Self>) {
        // SAFETY: a zeroed siginfo for the call to fill; the pidfd is ours.
        let how = unsafe {
            let mut info: libc::siginfo_t = std::mem::zeroed();
            loop {
                let r = libc::waitid(
                    libc::P_PIDFD,
                    self.pidfd.as_raw_fd() as libc::id_t,
                    &mut info,
                    libc::WEXITED,
                );
                if r == 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                    break;
                }
            }
            match info.si_code {
                libc::CLD_EXITED => format!("it exited ({})", info.si_status()),
                _ => format!("it was ended by signal {}", info.si_status()),
            }
        };
        // (its scope, once no process of its is in it)
        if let Some(Ok(r)) = &self.runs_in {
            super::cgroup::let_go(&r.source, &r.scope);
        }
        self.gone(how);
    }

    /// The keeper is gone (`why`): its runs' waits answer that, what they were is ended — failing closed —
    /// and, not asked to end, it died: counted (D11), said, and what its runs left (tarae's now, the
    /// subreaper) ended by their tag.
    fn gone(&self, why: String) {
        let mut s = lock(&self.state);
        if s.gone.is_some() {
            return;
        }
        let asked = s.ending;
        let said = format!("the session's keeper ended: {why}");
        let mut runs = Vec::new();
        for slot in s.runs.values_mut() {
            if slot.started.is_none() {
                slot.started = Some(Err(said.clone()));
            }
            if slot.exited.is_none() {
                slot.exited = Some(Err(said.clone()));
                if let Some(Ok(run)) = &slot.started {
                    runs.push(run.clone());
                }
            }
        }
        // (a death counted before it's seen gone: the next `ready` can't come between and miss it — D11)
        if !asked {
            lock(&DEATHS).push((self.root.clone(), Instant::now()));
        }
        s.gone = Some(why);
        drop(s);
        self.changed.notify_all();
        // (its owned table goes with tarae's socket, its netns with the last socket in it)
        drop(lock(&self.network).take());
        for (pid, pidfd) in &runs {
            kill_run(*pid, pidfd);
        }
        if !asked {
            crate::log::line(
                "sandbox",
                "warn",
                &format!("{said} (pid {}, {})", self.pid, self.root.display()),
            );
            let root = self.root.clone();
            crate::event::thread("tarae-kleft", move || {
                super::end_left_by_tag(&root);
            });
        }
    }

    /// Ends the keeper — every run and what they left, then itself — and waits for it (≤ `END_WITHIN`; a
    /// worker's).
    pub fn end(&self) {
        self.ask_end();
        self.ended_by(Instant::now() + END_WITHIN);
    }

    /// Tells the keeper to end (`end` without the wait).
    fn ask_end(&self) {
        lock(&self.state).ending = true;
        let _ = channel::send(self.chan.as_fd(), &Msg::End, &[]);
    }

    /// Waits for the keeper to be gone until `deadline` — then KILL to it.
    fn ended_by(&self, deadline: Instant) {
        let mut s = lock(&self.state);
        while s.gone.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                drop(s);
                // SAFETY: KILL to the keeper by its pidfd (no other process can be it).
                unsafe {
                    libc::syscall(libc::SYS_pidfd_send_signal, self.pidfd.as_raw_fd(), libc::SIGKILL, 0, 0)
                };
                return;
            }
            s = self.changed.wait_timeout(s, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// Whether the keeper is gone.
    pub fn is_gone(&self) -> bool {
        lock(&self.state).gone.is_some()
    }

    /// What its runs left running: its children that aren't a run going.
    pub fn leftovers(&self) -> Vec<u32> {
        let runs: Vec<u32> = lock(&self.state)
            .runs
            .values()
            .filter(|r| r.exited.is_none())
            .filter_map(|r| r.started.as_ref()?.as_ref().ok().map(|(p, _)| *p))
            .collect();
        crate::procs::children(self.pid).into_iter().filter(|p| !runs.contains(p)).collect()
    }

    /// Tests (M3): what the keeper itself uses, as it says (its `smaps_rollup` isn't readable to tarae:
    /// non-dumpable — `:procs` shows what's resident, from its `stat`): (Rss, Pss) in KB.
    #[cfg(test)]
    pub fn usage(&self) -> Option<(u64, u64)> {
        lock(&self.state).usage = None;
        channel::send(self.chan.as_fd(), &Msg::AskUsage, &[]).ok()?;
        let deadline = Instant::now() + STARTED_WITHIN;
        let mut s = lock(&self.state);
        while s.usage.is_none() && s.gone.is_none() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            s = self.changed.wait_timeout(s, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        s.usage
    }

    /// Asks for run `req` with its descriptors (the ruleset, stdin, stdout, stderr — and served or held, the
    /// handoff socket's child end) and waits for its start (`STARTED_WITHIN`): its pid, pidfd and own cgroup's id
    /// (0: none). `hold`, a held run's end of that socket: its byte goes once its pid is known (`heard`).
    fn run(
        &self,
        mut req: channel::Run,
        fds: &[BorrowedFd],
        hold: Option<Arc<OwnedFd>>,
    ) -> io::Result<(u32, Arc<OwnedFd>, u64)> {
        let id = {
            let mut s = lock(&self.state);
            if let Some(why) = &s.gone {
                return Err(io::Error::other(format!("the session's keeper ended: {why}")));
            }
            if s.superseded {
                return Err(io::Error::other("the session's keeper was replaced"));
            }
            s.next += 1;
            let id = s.next;
            s.runs.insert(id, Slot { hold, ..Slot::default() });
            id
        };
        req.id = id;
        if let Err(e) = channel::send(self.chan.as_fd(), &Msg::Run(req), fds) {
            lock(&self.state).runs.remove(&id);
            return Err(e);
        }
        let deadline = Instant::now() + STARTED_WITHIN;
        let mut s = lock(&self.state);
        loop {
            if let Some((started, cgroup)) =
                s.runs.get(&id).and_then(|r| Some((r.started.clone()?, r.cgroup)))
            {
                if started.is_err() {
                    s.runs.remove(&id);
                }
                return started.map(|(pid, pidfd)| (pid, pidfd, cgroup)).map_err(io::Error::other);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                s.runs.remove(&id);
                return Err(io::Error::other(format!(
                    "the session's keeper didn't start it within {} s",
                    STARTED_WITHIN.as_secs()
                )));
            }
            s = self.changed.wait_timeout(s, left).unwrap_or_else(|e| e.into_inner()).0;
        }
    }
}

/// A run the keeper started (`Keeper::start`).
pub struct Started {
    pub spawned: crate::spawn::Spawned,
    pub pidfd: Arc<OwnedFd>,
    /// The id of the cgroup of its own it started in (6g) — None: its keeper's. Its rule's level is its keeper's
    /// (`RunsIn.level`). Nothing reads it before 6d but tests.
    #[cfg_attr(not(test), allow(dead_code))]
    pub cgroup: Option<u64>,
}

impl Keeper {
    /// Starts `cmd` — its program, arguments, environment and folder, as `spawn::setsid` takes them: stdin
    /// `/dev/null`, stdout and stderr pipes — through the keeper, confined to the Landlock ruleset `rules`, its
    /// process named `comm` before it confines itself (`landlock::thread_name` — what Landlock's audit names
    /// beside its pid: the caller tells its watch first, `sandbox::spawn`). Its sockets go as `net` says — the
    /// answer `rules` were made for (`linux::confine`'s `Confine.net`): `Own`, its internet sockets are its own
    /// (`seccomp::Mode::OwnNetwork`); `Plain`, it has none; `Served` and `Routed` are `start_served`'s.
    pub fn start(
        &self,
        cmd: &std::process::Command,
        comm: &str,
        rules: BorrowedFd,
        net: RunNet,
    ) -> io::Result<Started> {
        if matches!(net, RunNet::Served | RunNet::Routed) {
            return Err(io::Error::other("a served run starts with its supervisor (start_served)"));
        }
        let no_userns = super::tier::runs_refuse_userns();
        self.start_with(cmd, comm, rules, net, no_userns).map(|(s, _)| s)
    }

    /// Tests: `start`, new user namespaces refused (`no_userns`) as asked rather than as this system decides.
    #[cfg(test)]
    pub fn start_userns(
        &self,
        cmd: &std::process::Command,
        comm: &str,
        rules: BorrowedFd,
        net: RunNet,
        no_userns: bool,
    ) -> io::Result<Started> {
        self.start_with(cmd, comm, rules, net, no_userns).map(|(s, _)| s)
    }

    /// `start`, served or not; with a handoff socket when served or held (a keeper in a PID namespace — its
    /// run waits for tarae to know its pid): tarae's end of it comes back. `no_userns`: its filter refuses new
    /// user namespaces (`tier::runs_refuse_userns`).
    fn start_with(
        &self,
        cmd: &std::process::Command,
        comm: &str,
        rules: BorrowedFd,
        net: RunNet,
        no_userns: bool,
    ) -> io::Result<(Started, Option<Arc<OwnedFd>>)> {
        use std::os::unix::ffi::OsStrExt;
        // (its own sockets only inside the network namespace that holds them to `lo`)
        if matches!(net, RunNet::Own | RunNet::Routed) && !self.ns.netns() {
            return Err(io::Error::other("a run's own sockets, but its keeper has no network namespace"));
        }
        let env = crate::spawn::environment(cmd);
        let cwd = match cmd.get_current_dir() {
            Some(d) => std::path::absolute(d)?,
            None => std::env::current_dir()?,
        };
        let path_var = env.iter().find(|(k, _)| k == "PATH").map(|(_, v)| v.as_os_str());
        let program = crate::spawn::resolve(cmd.get_program(), Some(&cwd), path_var)?;
        let bytes = |s: &std::ffi::OsStr| s.as_bytes().to_vec();
        let argv = std::iter::once(cmd.get_program()).chain(cmd.get_args()).map(bytes).collect();
        let envp = env
            .iter()
            .map(|(k, v)| {
                let mut kv = bytes(k);
                kv.push(b'=');
                kv.extend(v.as_bytes());
                kv
            })
            .collect();
        let req = channel::Run {
            id: 0,
            program: bytes(program.as_os_str()),
            argv,
            envp,
            cwd: bytes(cwd.as_os_str()),
            comm: comm.as_bytes().to_vec(),
            net,
            held: self.ns.pid,
            no_userns,
        };
        let stdin = open_null(libc::O_RDONLY)?;
        let (out_r, out_w) = io::pipe()?;
        let (err_r, err_w) = io::pipe()?;
        let handoff = match req.served() || req.held {
            true => Some(stream_pair()?),
            false => None,
        };
        let mut fds = vec![rules, stdin.as_fd(), out_w.as_fd(), err_w.as_fd()];
        fds.extend(handoff.as_ref().map(|(_, theirs)| theirs.as_fd()));
        let ours = handoff.as_ref().map(|(ours, _)| ours.clone());
        let hold = ours.clone().filter(|_| req.held);
        let (pid, pidfd, cgroup) = self.run(req, &fds, hold)?;
        let spawned = crate::spawn::Spawned { pid, stdout: Some(out_r), stderr: Some(err_r) };
        Ok((Started { spawned, pidfd, cgroup: (cgroup != 0).then_some(cgroup) }, ours))
    }
}

/// A `SOCK_STREAM` pair, close-on-exec: tarae's end (shared by who writes on it) and the child's.
fn stream_pair() -> io::Result<(Arc<OwnedFd>, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: a two-int array for the call to fill.
    if unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) }
        != 0
    {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both just made, ours.
    Ok(unsafe { (Arc::new(OwnedFd::from_raw_fd(fds[0])), OwnedFd::from_raw_fd(fds[1])) })
}

impl Keeper {
    /// `start`, served: the run's supervisor started first (`supervisor::serve`: `policy` as it is now, its
    /// refusals out on `refused`), then the run with the child's end of a handoff pair; once its filter is on,
    /// its listener's number comes on that pair, tarae takes the listener from the run's process
    /// (`pidfd_getfd`, on a thread of its own — the main loop doesn't wait for it), hands it to the supervisor
    /// and says one byte back: the run goes on to its exec. Anything failing there closes the pair, and the run
    /// fails at its handoff. `net`: `Served`, or `Routed` (its own sockets in the session's netns).
    pub fn start_served(
        &self,
        cmd: &std::process::Command,
        comm: &str,
        rules: BorrowedFd,
        net: RunNet,
        policy: Arc<super::Policy>,
        refused: std::sync::mpsc::Sender<super::Denial>,
    ) -> io::Result<Started> {
        if !matches!(net, RunNet::Served | RunNet::Routed) {
            return Err(io::Error::other("a run not served starts on its own (start)"));
        }
        // (6d: where its session has the TPROXY network, beyond is the run's own socket marked as its run's)
        let marks = lock(&self.network).as_ref().map(|n| n.marks.clone());
        let to = super::supervisor::serve(policy, net, refused, marks)?;
        let (started, ours) = self.start_with(cmd, comm, rules, net, super::tier::runs_refuse_userns())?;
        let ours = ours.ok_or_else(|| io::Error::other("a served run without its handoff"))?;
        let pidfd = started.pidfd.clone();
        crate::event::thread("tarae-handoff", move || {
            let mut n = [0u8; 4];
            // SAFETY: a read into a buffer of its size, a send of one byte, on our end of the pair.
            unsafe {
                if libc::read(ours.as_raw_fd(), n.as_mut_ptr().cast(), 4) != 4 {
                    return;
                }
                let Ok(listener) = super::supervisor::pidfd_getfd(&pidfd, i32::from_le_bytes(n)) else {
                    return;
                };
                if to.send(listener).is_err() {
                    return;
                }
                libc::send(ours.as_raw_fd(), [1u8].as_ptr().cast(), 1, libc::MSG_NOSIGNAL);
            }
        });
        Ok(started)
    }
}

impl std::fmt::Debug for Keeper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Keeper(pid {}, {})", self.pid, self.root.display())
    }
}

/// KILL to a dead keeper's run `pid` through `pidfd` (its keeper's, from `Started`), and to the group it leads
/// only while that pidfd says it's there: its number is it then, and the group it leads (a run's `setsid`) is
/// its own. Never by number alone — the run is no longer tarae's to reap first (it went to the nearest subreaper,
/// tarae or init, whose reaping frees its number), as `reaper::end`.
fn kill_run(pid: u32, pidfd: &OwnedFd) {
    if matches!(readable(pidfd.as_fd(), Duration::ZERO), Ok(false))
        // SAFETY: getpgid of a pid whose pidfd says it's alive (not reused).
        && unsafe { libc::getpgid(pid as libc::pid_t) } == pid as libc::pid_t
    {
        // SAFETY: the group it leads, as above.
        unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
    }
    // SAFETY: KILL through a pidfd we hold (no other process can be it).
    unsafe { libc::syscall(libc::SYS_pidfd_send_signal, pidfd.as_raw_fd(), libc::SIGKILL, 0, 0) };
}

/// `struct clone_args` (Linux 5.3; `CLONE_ARGS_SIZE_VER2`).
#[repr(C)]
#[derive(Default)]
struct CloneArgs {
    flags: u64,
    pidfd: u64,
    child_tid: u64,
    parent_tid: u64,
    exit_signal: u64,
    stack: u64,
    stack_size: u64,
    tls: u64,
    set_tid: u64,
    set_tid_size: u64,
    cgroup: u64,
}

/// A child with its pidfd from the start (in `pidfd`), in the namespaces `ns_flags` makes (`Ns::clone_flags`):
/// `clone3(CLONE_PIDFD | …)` — or, where a filter answers `clone3` with `ENOSYS` (Docker's default profile
/// does, so that libcs fall back), `clone(2)` with the same flags (Linux 5.2). 0 in the child, its pid in the
/// parent, -1 and errno on failure.
///
/// # Safety
/// The child is a copy of the calling (multithreaded) process: until its exec it may make raw calls only.
unsafe fn clone_pidfd(pidfd: &mut libc::c_int, ns_flags: u64) -> libc::c_long {
    let args = CloneArgs {
        flags: libc::CLONE_PIDFD as u64 | ns_flags,
        pidfd: (pidfd as *mut libc::c_int) as u64,
        exit_signal: libc::SIGCHLD as u64,
        ..CloneArgs::default()
    };
    // SAFETY: a live `clone_args` of its size; the caller keeps the child to raw calls.
    unsafe {
        let r = libc::syscall(libc::SYS_clone3, &raw const args, std::mem::size_of::<CloneArgs>());
        if r != -1 || io::Error::last_os_error().raw_os_error() != Some(libc::ENOSYS) {
            return r;
        }
        // (flags, new stack: none — the child goes on on a copy of this one, as a fork's —, where the pidfd goes)
        let flags = (libc::CLONE_PIDFD | libc::SIGCHLD) as libc::c_ulong | ns_flags as libc::c_ulong;
        libc::syscall(libc::SYS_clone, flags, 0usize, pidfd as *mut libc::c_int, 0usize, 0usize)
    }
}

/// What a birth's child needs, made before the clone — it makes raw calls only: its go pipe (tarae's byte once
/// the id maps are written) and its birth pipe (what failed before its exec, as `(step, errno)`; its end closed
/// by the exec: it's there), both above the descriptors the child moves into 0–3; and this kernel's highest
/// capability.
struct Birth {
    go: (OwnedFd, OwnedFd),
    told: (OwnedFd, OwnedFd),
    last_cap: libc::c_ulong,
    /// The address families the session's `lo` mirrors (with a network namespace).
    families: Families,
    /// Tests: the mirrors put on a device that isn't there, to see them fail.
    off_lo: bool,
    /// The session's network is made (`redirect`): a raw ICMP socket is made in its netns too, while the child still
    /// has the capability that takes (`CAP_NET_RAW`) — `Taken` answers refused datagrams on it — and its
    /// `tcp_fwmark_accept` set (6d: `BirthStep::Fwmark`).
    icmp: bool,
    /// Tests: what its read-only step does instead (`CgroupfsSeam`, by the keeper's root).
    #[cfg(test)]
    seam: Option<CgroupfsSeam>,
}

/// What a birth's child says on its birth pipe of a mirror it couldn't make (`Families`): IPv4's, IPv6's —
/// codes past `BirthStep`'s, as (code, errno); the birth goes on (`said_ready` takes the family off).
const LO_NOTES: [u8; 2] = [0x80, 0x81];
/// What a birth's child says of its read-only step (`cgroupfs_read_only`), every time it has a mount namespace: 0
/// made read-only, `NOTHING_THERE`, `NOT_A_MOUNT`, else the errno it couldn't for. The birth goes on whatever it
/// says (the step is 6g's second lock — Landlock never grants the hierarchy — and only 6g needs it): a keeper whose
/// step didn't make it read-only gets no scope (`read_only`).
const CGROUPFS_NOTE: u8 = 0x82;
/// `/sys/fs/cgroup` isn't there in its mount namespace (checked: `statx` says so).
const NOTHING_THERE: i32 = -1;
/// `/sys/fs/cgroup` is there but no mount's root (checked: `STATX_ATTR_MOUNT_ROOT`, or its device its parent's).
const NOT_A_MOUNT: i32 = -2;

/// Tests: what a birth's read-only step does instead of its calls — fails with this errno, or finds no
/// `mount_setattr` (answered `ENOSYS`: the pre-5.12 way, forced).
#[cfg(test)]
#[derive(Clone, Copy, Debug)]
enum CgroupfsSeam {
    Fail(i32),
    NoSetattr,
}

/// Tests: the seams asked for, by the keeper's root — one test's never reaches another's keeper.
#[cfg(test)]
static CGROUPFS_SEAMS: Mutex<Vec<(PathBuf, CgroupfsSeam)>> = Mutex::new(Vec::new());

/// Whether a keeper whose birth said `said` (`CGROUPFS_NOTE`; None: nothing — no mount namespace of its own, `ns`)
/// has `/sys/fs/cgroup` read-only in its namespace — else why not, for its log line: only such a keeper gets a
/// scope (6g, the review's second pass). Pure.
fn read_only(ns: Ns, said: Option<i32>) -> Result<(), String> {
    match (ns.user, said) {
        (false, _) => Err("it has no mount namespace of its own to make /sys/fs/cgroup read-only in".into()),
        (true, Some(0)) => Ok(()),
        (true, Some(NOTHING_THERE)) => Err("nothing is at /sys/fs/cgroup in its mount namespace".into()),
        (true, Some(NOT_A_MOUNT)) => Err("/sys/fs/cgroup isn't a mount point in its mount namespace".into()),
        (true, Some(e)) => Err(format!(
            "/sys/fs/cgroup couldn't be made read-only there: {}",
            io::Error::from_raw_os_error(e)
        )),
        (true, None) => Err("its birth said nothing of /sys/fs/cgroup".into()),
    }
}

/// The address families tarae's own network has — what glibc's `AI_ADDRCONFIG` counts: an IPv4 address but
/// loopback, an IPv6 one but loopback and link-local. The session's `lo` carries a documentation address of each
/// (`SESSION_V4`, `SESSION_V6`), so a lookup inside answers as it would outside (M5). Decided at each birth.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Families {
    pub v4: bool,
    pub v6: bool,
}

impl Families {
    /// Those `addrs` have.
    pub fn of(addrs: &[std::net::IpAddr]) -> Families {
        let v4 = addrs.iter().any(|a| matches!(a, std::net::IpAddr::V4(v4) if !v4.is_loopback()));
        let v6 = addrs.iter().any(
            |a| matches!(a, std::net::IpAddr::V6(v6) if !v6.is_loopback() && !v6.is_unicast_link_local()),
        );
        Families { v4, v6 }
    }

    /// tarae's own network's.
    pub fn here() -> Families {
        Families::of(&addresses().unwrap_or_default())
    }
}

/// The addresses of tarae's own network namespace, every interface's (`getifaddrs`).
pub fn addresses() -> io::Result<Vec<std::net::IpAddr>> {
    super::proxy::egress::addresses()
}

/// `struct in6_ifreq` as `SIOCSIFADDR` takes it on an IPv6 socket: the address, its prefix, the device's index.
#[repr(C)]
struct In6Ifreq {
    addr: libc::in6_addr,
    prefixlen: u32,
    ifindex: libc::c_int,
}

/// The loopback device's index in every network namespace (the kernel's `LOOPBACK_IFINDEX`).
const LOOPBACK_INDEX: libc::c_int = 1;

/// `struct ifreq` as `SIOCGIFFLAGS`/`SIOCSIFFLAGS` take it: a name, the flags, the rest of its 40 bytes.
#[repr(C)]
struct IfFlags {
    name: [u8; 16],
    flags: libc::c_short,
    rest: [u8; 22],
}

/// `struct ifreq` as `SIOCSIFADDR`/`SIOCSIFNETMASK` take it: a name, an IPv4 address, the rest of its 40 bytes.
#[repr(C)]
struct IfAddr {
    name: [u8; 16],
    addr: libc::sockaddr_in,
    rest: [u8; 8],
}

/// The addresses the session's `lo` carries beside loopback (`redirect.rs` says why — and its planted sockets are at
/// them).
pub use super::redirect::{SESSION_V4, SESSION_V6};

impl Birth {
    fn new(families: Families, off_lo: bool, icmp: bool) -> io::Result<Birth> {
        let pipe = || -> io::Result<(OwnedFd, OwnedFd)> {
            let (r, w) = io::pipe()?;
            Ok((high(r.into())?, high(w.into())?))
        };
        let last_cap = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(63);
        Ok(Birth {
            go: pipe()?,
            told: pipe()?,
            last_cap,
            families,
            off_lo,
            icmp,
            #[cfg(test)]
            seam: None,
        })
    }

    /// tarae's ends, the child's closed: the go pipe's writer, the birth pipe's reader.
    fn parents(self) -> (OwnedFd, OwnedFd) {
        (self.go.1, self.told.0)
    }

    /// Says how a step the birth goes on without went (`LO_NOTES`, `CGROUPFS_NOTE`; an errno or its own code) on the
    /// birth pipe; the child goes on.
    ///
    /// # Safety
    /// The birth's child's: raw calls only.
    unsafe fn note(&self, code: u8, errno: i32) {
        let mut m = [code; 5];
        m[1..].copy_from_slice(&errno.to_le_bytes());
        // SAFETY: a write of a buffer we own.
        unsafe { libc::write(self.told.1.as_raw_fd(), m.as_ptr().cast(), m.len()) };
    }

    /// Says what failed (`step`, `errno`) on the birth pipe, and ends the child.
    ///
    /// # Safety
    /// The birth's child's: raw calls only.
    unsafe fn failed(&self, step: BirthStep, errno: i32) -> ! {
        let mut m = [step.code(); 5];
        m[1..].copy_from_slice(&errno.to_le_bytes());
        // SAFETY: a write of a buffer we own, then the end of this process.
        unsafe {
            libc::write(self.told.1.as_raw_fd(), m.as_ptr().cast(), m.len());
            libc::_exit(127)
        }
    }

    /// The child, before the rest of its birth: waits for tarae's go (its id maps written; its end gone —
    /// tarae gave up — it exits), then, with namespaces: its mounts slaved to the system's (nothing it mounts
    /// goes out), a fresh `/proc` with a PID namespace, its own `devpts` with its `ptmx` over `/dev/ptmx`, its
    /// own `/dev/shm` (D10), `lo` up, and every capability dropped — each checked; a failure said and it exits.
    /// With the session's network (`icmp`), a raw ICMP socket made in its netns before the drop: where it is (above
    /// the descriptors the child moves, not close-on-exec), else -1 — a socket that can't be made is done without —
    /// and its netns's `tcp_fwmark_accept` set, which it can't be born without (6d).
    ///
    /// # Safety
    /// The birth's child's: raw calls only (a copy of a multithreaded process).
    unsafe fn child(&self, ns: Ns) -> libc::c_int {
        let null = std::ptr::null::<libc::c_char>();
        // SAFETY: raw calls on this process's own descriptors, static strings and buffers on its stack.
        unsafe {
            libc::close(self.go.1.as_raw_fd());
            libc::close(self.told.0.as_raw_fd());
            let mut byte = 0u8;
            loop {
                match libc::read(self.go.0.as_raw_fd(), (&raw mut byte).cast(), 1) {
                    1 => break,
                    0 => libc::_exit(127),
                    _ if errno() == libc::EINTR => continue,
                    _ => self.failed(BirthStep::Go, errno()),
                }
            }
            libc::close(self.go.0.as_raw_fd());
            if !ns.user {
                return -1;
            }
            let mut raw = -1;
            let mount = |src: *const libc::c_char,
                         at: &std::ffi::CStr,
                         fs: *const libc::c_char,
                         flags: libc::c_ulong,
                         data: *const libc::c_void| {
                libc::mount(src, at.as_ptr(), fs, flags, data) == 0
            };
            if !mount(null, c"/", null, libc::MS_REC | libc::MS_SLAVE, std::ptr::null()) {
                self.failed(BirthStep::Propagation, errno());
            }
            let (nosuid, nodev, noexec) = (libc::MS_NOSUID, libc::MS_NODEV, libc::MS_NOEXEC);
            if ns.pid
                && !mount(
                    c"proc".as_ptr(),
                    c"/proc",
                    c"proc".as_ptr(),
                    nosuid | nodev | noexec,
                    std::ptr::null(),
                )
            {
                self.failed(BirthStep::Proc, errno());
            }
            let pts = c"newinstance,ptmxmode=0666,mode=0620";
            if !mount(
                c"devpts".as_ptr(),
                c"/dev/pts",
                c"devpts".as_ptr(),
                nosuid | noexec,
                pts.as_ptr().cast(),
            ) {
                self.failed(BirthStep::Devpts, errno());
            }
            if !mount(c"/dev/pts/ptmx".as_ptr(), c"/dev/ptmx", null, libc::MS_BIND, std::ptr::null()) {
                self.failed(BirthStep::Ptmx, errno());
            }
            let shm = c"mode=1777";
            if ns.shm
                && !mount(
                    c"tmpfs".as_ptr(),
                    c"/dev/shm",
                    c"tmpfs".as_ptr(),
                    nosuid | nodev,
                    shm.as_ptr().cast(),
                )
            {
                self.failed(BirthStep::Shm, errno());
            }
            // (said, never failed for: only 6g needs it, and only 6g is done without — `read_only`)
            #[cfg(test)]
            let (fail, setattr) = match self.seam {
                Some(CgroupfsSeam::Fail(e)) => (Some(e), true),
                Some(CgroupfsSeam::NoSetattr) => (None, false),
                None => (None, true),
            };
            #[cfg(not(test))]
            let (fail, setattr) = (None, true);
            self.note(CGROUPFS_NOTE, cgroupfs_read_only(fail, setattr));
            if ns.net {
                let s = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
                let mut ifr = IfFlags { name: [0; 16], flags: 0, rest: [0; 22] };
                ifr.name[..2].copy_from_slice(b"lo");
                if s < 0 || libc::ioctl(s, libc::SIOCGIFFLAGS as _, &raw mut ifr) != 0 {
                    self.failed(BirthStep::Lo, errno());
                }
                ifr.flags |= libc::IFF_UP as libc::c_short;
                if libc::ioctl(s, libc::SIOCSIFFLAGS as _, &raw mut ifr) != 0 {
                    self.failed(BirthStep::Lo, errno());
                }
                // The families tarae's network has, mirrored (`Families`): `SESSION_V4` as `lo:1`, a /32;
                // `SESSION_V6`, a /128 — what an `AI_ADDRCONFIG` lookup asks before it asks anything else. Only
                // lookups need them: one that fails is said (`LO_NOTES`) and the birth goes on without it
                if self.families.v4 {
                    let mut ifa = IfAddr { name: [0; 16], addr: std::mem::zeroed(), rest: [0; 8] };
                    let dev: &[u8] = if self.off_lo { b"nolo:1" } else { b"lo:1" };
                    ifa.name[..dev.len()].copy_from_slice(dev);
                    ifa.addr.sin_family = libc::AF_INET as libc::sa_family_t;
                    ifa.addr.sin_addr.s_addr = u32::from_ne_bytes(SESSION_V4.octets());
                    if libc::ioctl(s, libc::SIOCSIFADDR as _, &raw mut ifa) != 0 {
                        self.note(LO_NOTES[0], errno());
                    } else {
                        ifa.addr.sin_addr.s_addr = u32::MAX;
                        if libc::ioctl(s, libc::SIOCSIFNETMASK as _, &raw mut ifa) != 0 {
                            self.note(LO_NOTES[0], errno());
                        }
                    }
                }
                libc::close(s);
                if self.families.v6 {
                    let s6 = libc::socket(libc::AF_INET6, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
                    let mut r = In6Ifreq {
                        addr: libc::in6_addr { s6_addr: SESSION_V6.octets() },
                        prefixlen: 128,
                        ifindex: if self.off_lo { 0x7fff } else { LOOPBACK_INDEX },
                    };
                    if s6 < 0 || libc::ioctl(s6, libc::SIOCSIFADDR as _, &raw mut r) != 0 {
                        self.note(LO_NOTES[1], errno());
                    }
                    if s6 >= 0 {
                        libc::close(s6);
                    }
                }
                if self.icmp {
                    let r = libc::socket(libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_ICMP);
                    if r >= 0 {
                        raw = libc::fcntl(r, libc::F_DUPFD, 10);
                        libc::close(r);
                    }
                    // 6d: a connection keeps its mark where the planted socket accepts it — its run's, tarae's to
                    // give (`marks.rs`); this netns's own setting, written while the child still has the
                    // capability over it
                    let at = c"/proc/sys/net/ipv4/tcp_fwmark_accept";
                    let f = libc::open(at.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                    if f < 0 || libc::write(f, b"1".as_ptr().cast(), 1) != 1 {
                        self.failed(BirthStep::Fwmark, errno());
                    }
                    libc::close(f);
                }
            }
            if let Err(e) = capabilities_dropped(self.last_cap) {
                self.failed(BirthStep::Caps, e);
            }
            raw
        }
    }
}

/// `struct mount_attr` (Linux 5.12): what `mount_setattr` sets and clears.
#[repr(C)]
struct MountAttr {
    attr_set: u64,
    attr_clr: u64,
    propagation: u64,
    userns_fd: u64,
}

const MOUNT_ATTR_RDONLY: u64 = 1;

/// `statx`'s attribute: the path is the root of a mount (Linux 5.8).
const STATX_ATTR_MOUNT_ROOT: u64 = 0x2000;

/// `/sys/fs/cgroup` and what's mounted under it read-only in the birth's mount namespace (6g, the review's second
/// finding): every cgroup there is the user's — the runs' own uid — under `Delegate=yes`, so with it writable a run
/// could move itself into another run's cgroup (6d's rules would count it that run's), into or out of its keeper's
/// scope, or change one. The keeper itself writes none by path: its runs' cgroups are made and entered through the
/// scope's folder tarae opened in its own namespace, writable there. What it says (`CGROUPFS_NOTE`): 0, made
/// read-only; `NOTHING_THERE` or `NOT_A_MOUNT`, each as checked (`statx`), never guessed from an error; else the
/// errno it couldn't for (`fail`: tests say one instead of trying).
///
/// `mount_setattr` only adds read-only, leaving what the mount has locked as it is. Without it (before 5.12 —
/// `setattr` false: tests force that way), a bind remount, which must keep every flag a mount from a more privileged
/// namespace has locked or be refused (`EPERM`): nosuid, nodev, noexec and the atime ones — noatime, nodiratime,
/// relatime, else strictatime — each as `statvfs` says it has them (said, not left to the kernel's keeping atime on a
/// remount that names none).
///
/// # Safety
/// The birth's child's: raw calls only.
unsafe fn cgroupfs_read_only(fail: Option<i32>, setattr: bool) -> i32 {
    let at = c"/sys/fs/cgroup";
    // SAFETY: raw calls with live buffers of their sizes and NUL-terminated paths.
    unsafe {
        let mut stx: libc::statx = std::mem::zeroed();
        let how = libc::AT_SYMLINK_NOFOLLOW | libc::AT_NO_AUTOMOUNT;
        if libc::statx(libc::AT_FDCWD, at.as_ptr(), how, libc::STATX_BASIC_STATS, &mut stx) != 0 {
            return if errno() == libc::ENOENT { NOTHING_THERE } else { errno() };
        }
        let root = match stx.stx_attributes_mask & STATX_ATTR_MOUNT_ROOT != 0 {
            true => stx.stx_attributes & STATX_ATTR_MOUNT_ROOT != 0,
            // (before 5.8: a mount's root is on another device than its parent)
            false => {
                let mut parent: libc::statx = std::mem::zeroed();
                let up = libc::statx(
                    libc::AT_FDCWD,
                    c"/sys/fs".as_ptr(),
                    how,
                    libc::STATX_BASIC_STATS,
                    &mut parent,
                );
                up != 0
                    || (stx.stx_dev_major, stx.stx_dev_minor) != (parent.stx_dev_major, parent.stx_dev_minor)
            }
        };
        if !root {
            return NOT_A_MOUNT;
        }
        if let Some(e) = fail {
            return e;
        }
        if setattr {
            let attr = MountAttr { attr_set: MOUNT_ATTR_RDONLY, attr_clr: 0, propagation: 0, userns_fd: 0 };
            let r = libc::syscall(
                libc::SYS_mount_setattr,
                libc::AT_FDCWD,
                at.as_ptr(),
                libc::AT_RECURSIVE,
                &raw const attr,
                std::mem::size_of::<MountAttr>(),
            );
            if r == 0 {
                return 0;
            }
            if errno() != libc::ENOSYS {
                return errno();
            }
        }
        let mut st: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(at.as_ptr(), &mut st) != 0 {
            return errno();
        }
        let mut kept = 0;
        for (st_flag, ms) in [
            (libc::ST_NOSUID, libc::MS_NOSUID),
            (libc::ST_NODEV, libc::MS_NODEV),
            (libc::ST_NOEXEC, libc::MS_NOEXEC),
            (libc::ST_NOATIME, libc::MS_NOATIME),
            (libc::ST_NODIRATIME, libc::MS_NODIRATIME),
            (libc::ST_RELATIME, libc::MS_RELATIME),
        ] {
            if st.f_flag & st_flag != 0 {
                kept |= ms;
            }
        }
        // (neither noatime nor relatime: strictatime, said as such)
        if st.f_flag & (libc::ST_NOATIME | libc::ST_RELATIME) == 0 {
            kept |= libc::MS_STRICTATIME;
        }
        let flags = libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | kept;
        let null = std::ptr::null::<libc::c_char>();
        if libc::mount(null, at.as_ptr(), null, flags, std::ptr::null()) == 0 { 0 } else { errno() }
    }
}

/// `fd` moved up past the descriptors a birth's child moves into 0–3 (close-on-exec).
fn high(fd: OwnedFd) -> io::Result<OwnedFd> {
    // SAFETY: a dup of a descriptor we own; the new one is ours.
    let up = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
    if up < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just made.
    Ok(unsafe { OwnedFd::from_raw_fd(up) })
}

/// The new keeper's id maps, written by tarae (its parent, the user namespace's owner): the user's own ids,
/// each to itself (D5) — `setgroups` denied first, as an unprivileged map of a group needs.
fn id_maps(pid: i32) -> Result<(), BirthError> {
    // SAFETY: plain calls with no arguments.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let maps = [
        ("setgroups", "deny".to_string()),
        ("uid_map", format!("{uid} {uid} 1")),
        ("gid_map", format!("{gid} {gid} 1")),
    ];
    for (file, map) in maps {
        std::fs::write(format!("/proc/{pid}/{file}"), map)
            .map_err(|e| BirthError { step: BirthStep::Map, errno: e.raw_os_error().unwrap_or(libc::EIO) })?;
    }
    Ok(())
}

/// Waits for a new keeper's `Ready` on `chan` (`READY_WITHIN`) — or what its birth pipe `told` says failed
/// first (its end, the exec, says nothing). Its device handles: those of `DEVICE_HANDLES`' places, each once
/// (another name or a second one is left out, said in the log). A mirror its child couldn't make (`LO_NOTES`)
/// is put in `missing` and said in the log; what its read-only step said (`CGROUPFS_NOTE`), in `cgroupfs`.
fn said_ready(
    chan: BorrowedFd,
    told: OwnedFd,
    missing: &mut Families,
    cgroupfs: &mut Option<i32>,
) -> io::Result<Vec<(&'static str, OwnedFd)>> {
    let deadline = Instant::now() + READY_WITHIN;
    let mut told = Some(told);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::other("it didn't say it was there"));
        }
        let mut polled = vec![libc::pollfd { fd: chan.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
        if let Some(t) = &told {
            polled.push(libc::pollfd { fd: t.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        }
        // SAFETY: the pollfds, live for the call.
        let r =
            unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, left.as_millis() as i32) };
        if r < 0 && errno() != libc::EINTR {
            return Err(io::Error::last_os_error());
        }
        if let (Some(t), Some(p)) = (&told, polled.get(1))
            && p.revents != 0
        {
            let mut b = [0u8; 5];
            // SAFETY: a read into a buffer of its size.
            let n = unsafe { libc::read(t.as_raw_fd(), b.as_mut_ptr().cast(), b.len()) };
            let errno = i32::from_le_bytes([b[1], b[2], b[3], b[4]]);
            if n == 5 && b[0] == CGROUPFS_NOTE {
                *cgroupfs = Some(errno);
                continue;
            }
            if n == 5 && LO_NOTES.contains(&b[0]) {
                let (family, address, lookups) = match b[0] == LO_NOTES[0] {
                    true => (&mut missing.v4, SESSION_V4.to_string(), "IPv4"),
                    false => (&mut missing.v6, SESSION_V6.to_string(), "IPv6"),
                };
                *family = true;
                crate::log::line(
                    "sandbox",
                    "warn",
                    &format!(
                        "a keeper's lo couldn't take {address} ({}): its runs' {lookups}-only name lookups with \
                         AI_ADDRCONFIG find nothing",
                        io::Error::from_raw_os_error(errno)
                    ),
                );
                continue;
            }
            if n == 5 {
                let step = BirthStep::from_code(b[0]).unwrap_or(BirthStep::Exec);
                return Err(BirthError { step, errno }.into());
            }
            told = None;
            continue;
        }
        if polled[0].revents != 0 {
            return match channel::recv(chan, &mut Vec::new())? {
                Some((Ok(Msg::Ready { places, .. }), fds)) => {
                    let mut kept: Vec<(&'static str, OwnedFd)> = Vec::new();
                    for (place, fd) in places.iter().zip(fds) {
                        match DEVICE_HANDLES.iter().find(|d| d.as_bytes() == place.as_slice()) {
                            Some(d) if !kept.iter().any(|(k, _)| k == d) => kept.push((*d, fd)),
                            _ => crate::log::line(
                                "sandbox",
                                "warn",
                                &format!("a keeper handed {} — left out", String::from_utf8_lossy(place)),
                            ),
                        }
                    }
                    Ok(kept)
                }
                Some((msg, _)) => Err(io::Error::other(format!("it said {msg:?} first"))),
                None => Err(io::Error::other("it ended at once")),
            };
        }
    }
}

/// Tarae's number for the process `pidfd` is of (its fdinfo's `Pid:`) — else `Err(None)`: it's gone,
/// `Err(Some(why))`: tarae can't number it (`pid_in_fdinfo`, or no fdinfo to read).
fn pid_of(pidfd: &OwnedFd) -> Result<u32, Option<String>> {
    let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", pidfd.as_raw_fd()))
        .map_err(|e| Some(format!("its pidfd's fdinfo: {e}")))?;
    pid_in_fdinfo(&info)
}

/// A pidfd's fdinfo's `Pid:` — `Err(None)` at `-1` (the process is gone), `Err(Some(why))` for none tarae
/// can use (`0`: not in this `/proc`'s PID namespace; none at all).
fn pid_in_fdinfo(info: &str) -> Result<u32, Option<String>> {
    let pid = info.lines().find_map(|l| l.strip_prefix("Pid:")).and_then(|p| p.trim().parse::<i64>().ok());
    match pid {
        Some(-1) => Err(None),
        Some(p) if p > 0 => u32::try_from(p).map_err(|_| Some(format!("its pidfd says pid {p}"))),
        Some(_) => Err(Some("its pid isn't in tarae's PID namespace (its pidfd says 0)".into())),
        None => Err(Some("its pidfd's fdinfo has no pid".into())),
    }
}

/// tarae's own binary, opened once (`O_PATH`, above the descriptors the birth moves): what a keeper is
/// executed from — the binary running, whatever has replaced it on disk since.
fn exe() -> io::Result<BorrowedFd<'static>> {
    static EXE: std::sync::OnceLock<Result<OwnedFd, String>> = std::sync::OnceLock::new();
    let fd = EXE.get_or_init(|| {
        let c = c"/proc/self/exe";
        // SAFETY: a path string for the call; a descriptor it makes is ours, moved up past the low ones.
        unsafe {
            let fd = libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC);
            if fd < 0 {
                return Err(io::Error::last_os_error().to_string());
            }
            let high = libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 10);
            libc::close(fd);
            if high < 0 {
                return Err(io::Error::last_os_error().to_string());
            }
            Ok(OwnedFd::from_raw_fd(high))
        }
    });
    match fd {
        Ok(fd) => Ok(fd.as_fd()),
        Err(e) => Err(io::Error::other(format!("tarae's own binary: {e}"))),
    }
}

/// The keeper's arguments: `tarae --sandbox-keeper` — in tests the test binary, run on `keeper_process`.
fn keeper_argv() -> Vec<CString> {
    let args: &[&str] = match cfg!(test) {
        false => &["tarae", FLAG],
        true => &[
            "tarae-test",
            "--exact",
            "sandbox::keeper::tests::keeper_process",
            "--ignored",
            "--test-threads=1",
            "--quiet",
        ],
    };
    args.iter().map(|a| CString::new(*a).unwrap_or_default()).collect()
}

/// The keeper's environment: none (in tests, what tells `keeper_process` it's one).
fn keeper_env() -> Vec<CString> {
    match cfg!(test) {
        false => Vec::new(),
        true => vec![c"TARAE_KEEPER=1".to_owned()],
    }
}

/// A `SOCK_SEQPACKET` pair, close-on-exec.
fn seqpacket() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0; 2];
    // SAFETY: a two-int array for the call to fill.
    let r = unsafe {
        libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr())
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both just made, ours.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// `/dev/null`, close-on-exec.
pub(super) fn open_null(flags: libc::c_int) -> io::Result<OwnedFd> {
    // SAFETY: a path string for the call; a descriptor it makes is ours.
    let fd = unsafe { libc::open(c"/dev/null".as_ptr(), flags | libc::O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just made.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Whether `fd` turns readable within `wait`.
fn readable(fd: BorrowedFd, wait: Duration) -> io::Result<bool> {
    let mut p = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    let ms = wait.as_millis().min(i32::MAX as u128) as i32;
    loop {
        // SAFETY: one pollfd, live for the call.
        let r = unsafe { libc::poll(&mut p, 1, ms) };
        if r >= 0 {
            return Ok(r > 0);
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

// ── the keeper's side ────────────────────────────────────────────────────────

/// A run the keeper started: its process, and the pipe its child says a failure on before its exec (open until
/// it's read to its end: the exec closed it, or a failure came).
struct Child {
    pid: i32,
    error: Option<OwnedFd>,
    /// Its own cgroup's name in the keeper's scope (6g), removed once it's reaped.
    cgroup: Option<CString>,
}

/// The keeper (`tarae --sandbox-keeper`, first thing in `main`): its loop over its channel, its children's
/// failure pipes and `SIGCHLD`, until its channel ends or says `End` — then it ends every run and what they
/// left, and exits.
pub fn main() -> ! {
    // SAFETY: fd 3 is the channel its birth left there (`Keeper::born`); the rest are plain calls on this
    // process, before anything else runs in it.
    let (chan, signals) = unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0);
        drop_capabilities();
        libc::fcntl(CHANNEL_FD, libc::F_SETFD, libc::FD_CLOEXEC);
        libc::setsid();
        // (what tarae had ignored — SIGPIPE, std's — is every run's default again)
        for sig in 1..32 {
            if sig != libc::SIGKILL && sig != libc::SIGSTOP {
                libc::signal(sig, libc::SIG_DFL);
            }
        }
        // SIGCHLD blocked before its signalfd (and from the birth on): no handler or default ever runs for it
        let mut mask: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGCHLD);
        libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
        let sfd = libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK);
        if sfd < 0 {
            libc::_exit(1);
        }
        (OwnedFd::from_raw_fd(CHANNEL_FD), OwnedFd::from_raw_fd(sfd))
    };
    let mut k = Kept {
        chan,
        signals,
        children: HashMap::new(),
        reaped: 0,
        scope: None,
        clone3: true,
        ran: false,
        emptied: Vec::new(),
    };
    // (handed with the first `Planted`, never to a run: close-on-exec from here)
    let mut raw = birth_raw();
    // SAFETY: getpid has no preconditions.
    let me = unsafe { libc::getpid() } as u32;
    // Its device places, as its own mount namespace has them, opened now — before any run exists — and handed
    // to tarae with `Ready`: what tarae grants a run on (a rule on what they sit on isn't reached). A link
    // (`/dev/ptmx` → `pts/ptmx` in a container) isn't opened: what it points to comes as itself.
    let handles: Vec<(&str, OwnedFd)> = DEVICE_HANDLES
        .iter()
        .filter_map(|p| crate::landlock::open_unlinked(Path::new(p), libc::O_PATH).ok().map(|fd| (*p, fd)))
        .collect();
    let places = handles.iter().map(|(p, _)| p.as_bytes().to_vec()).collect();
    let fds: Vec<BorrowedFd> = handles.iter().map(|(_, fd)| fd.as_fd()).collect();
    if k.say(&Msg::Ready { pid: me, places }, &fds).is_err() {
        std::process::exit(1);
    }
    drop(fds);
    drop(handles);
    let mut buf = Vec::new();
    loop {
        let errors: Vec<(u64, RawFd)> =
            k.children.iter().filter_map(|(id, c)| c.error.as_ref().map(|e| (*id, e.as_raw_fd()))).collect();
        let mut polled: Vec<libc::pollfd> = [k.chan.as_raw_fd(), k.signals.as_raw_fd()]
            .into_iter()
            .chain(errors.iter().map(|e| e.1))
            .map(|fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 })
            .collect();
        // SAFETY: the pollfds, live for the call.
        let r = unsafe { libc::poll(polled.as_mut_ptr(), polled.len() as libc::nfds_t, -1) };
        if r < 0 {
            continue;
        }
        // A child's failure first: it comes before its end
        for (i, (id, _)) in errors.iter().enumerate() {
            if polled[2 + i].revents != 0 {
                k.failure(*id);
            }
        }
        if polled[1].revents != 0 {
            k.reap();
        }
        if polled[0].revents != 0 {
            match channel::recv(k.chan.as_fd(), &mut buf) {
                Ok(Some((Ok(Msg::End), _))) | Ok(None) | Err(_) => k.end_all(me),
                Ok(Some((Ok(Msg::Run(run)), fds))) => k.start(run, fds),
                Ok(Some((Ok(Msg::Signal { id, sig }), _))) => k.signal(id, sig),
                Ok(Some((Ok(Msg::AskUsage), _))) => {
                    let (rss_kb, pss_kb) = own_usage();
                    let _ = k.say(&Msg::Usage { rss_kb, pss_kb }, &[]);
                }
                // The sockets the session's network is made with, made here — in its netns — each in
                // `Kind::wanted`'s order, until one fails (its errno said with those made)
                Ok(Some((Ok(Msg::Plant { v4, v6 }), _))) => {
                    let mut made: Vec<(super::redirect::Kind, OwnedFd)> = Vec::new();
                    let mut errno = 0;
                    for kind in super::redirect::Kind::wanted(v4, v6) {
                        match super::redirect::open(kind) {
                            Ok(fd) => made.push((kind, fd)),
                            Err(e) => {
                                errno = e.raw_os_error().unwrap_or(libc::EIO);
                                break;
                            }
                        }
                    }
                    if errno == 0
                        && let Some(r) = raw.take()
                    {
                        made.push((super::redirect::Kind::Icmp4, r));
                    }
                    let kinds = made.iter().map(|(kind, _)| kind.code()).collect();
                    let fds: Vec<BorrowedFd> = made.iter().map(|(_, fd)| fd.as_fd()).collect();
                    let _ = k.say(&Msg::Planted { kinds, errno }, &fds);
                }
                // (6g: its scope, taken before any run only — each run's own cgroup made there from now on)
                Ok(Some((Ok(Msg::Cgroup), mut fds))) if !k.ran && k.scope.is_none() => k.scope = fds.pop(),
                // (a keeper's own kinds, or a malformed record: refused, and it goes on)
                Ok(Some((Ok(_), _))) | Ok(Some((Err(_), _))) => {
                    let _ = k.say(&Msg::Failed { id: 0, errno: libc::EINVAL, step: Step::Request }, &[]);
                }
            }
        }
    }
}

/// The raw ICMP socket its birth left at `RAW_FD` (`Birth::child`), made close-on-exec — None: nothing there, or what
/// is there isn't one (left as it is).
fn birth_raw() -> Option<OwnedFd> {
    let opt = |name: i32| {
        let (mut v, mut len) = (0i32, size_of::<i32>() as libc::socklen_t);
        // SAFETY: an int and its length, ours for the call, on a descriptor that may not be a socket (then -1).
        let r = unsafe { libc::getsockopt(RAW_FD, libc::SOL_SOCKET, name, (&raw mut v).cast(), &mut len) };
        (r == 0).then_some(v)
    };
    let raw = opt(libc::SO_DOMAIN) == Some(libc::AF_INET)
        && opt(libc::SO_TYPE) == Some(libc::SOCK_RAW)
        && opt(libc::SO_PROTOCOL) == Some(libc::IPPROTO_ICMP);
    if !raw {
        return None;
    }
    // SAFETY: the descriptor checked above is this process's, its birth's to hand on: ours from here.
    unsafe {
        libc::fcntl(RAW_FD, libc::F_SETFD, libc::FD_CLOEXEC);
        Some(OwnedFd::from_raw_fd(RAW_FD))
    }
}

/// The keeper's state.
struct Kept {
    chan: OwnedFd,
    signals: OwnedFd,
    children: HashMap<u64, Child>,
    /// Orphans reaped since `Reaped` last said so.
    reaped: u32,
    /// 6g: its scope's folder (`Msg::Cgroup`), where each run's own cgroup is made.
    scope: Option<OwnedFd>,
    /// Whether `clone3` is still worth asking — not once it was answered `ENOSYS`.
    clone3: bool,
    /// A run was asked: its scope is taken no more.
    ran: bool,
    /// The cgroups of runs reaped, still to remove: one a run's leftovers are still in (`EBUSY`) goes once the
    /// last of them is reaped (`reap`), or at the end (`end_all`).
    emptied: Vec<CString>,
}

/// `CLONE_INTO_CGROUP` (Linux 5.7): the child starts in the cgroup `clone_args.cgroup` names, never in its parent's.
const CLONE_INTO_CGROUP: u64 = 0x2_0000_0000;

impl Kept {
    fn say(&self, msg: &Msg, fds: &[BorrowedFd]) -> io::Result<()> {
        channel::send(self.chan.as_fd(), msg, fds)
    }

    /// Starts run `run` (its child: `child`), then says `Started` with its pidfd — or `Failed`. With a scope (6g),
    /// in a cgroup of its own made there: `clone3(CLONE_INTO_CGROUP)`, so nothing of the run ever runs in the
    /// keeper's (no `cgroup.procs` write after the fact, and its tail — spike6 Q4). That folder or that call failing,
    /// it's forked as without a scope, in the keeper's cgroup — `Started` says why (D5: said, never refused).
    fn start(&mut self, run: channel::Run, fds: Vec<OwnedFd>) {
        let id = run.id;
        self.ran = true;
        let failed = |k: &Kept, step, errno| {
            let _ = k.say(&Msg::Failed { id, errno, step }, &[]);
        };
        let exec = match Exec::of(&run) {
            Ok(exec) => exec,
            Err(e) => return failed(self, Step::Seccomp, e.raw_os_error().unwrap_or(libc::EIO)),
        };
        let mut pipe = [0; 2];
        // SAFETY: a two-int array for the call to fill.
        if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return failed(self, Step::Fork, errno());
        }
        // SAFETY: both just made, ours.
        let (error_r, error_w) = unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
        // (a scope, but `clone3` answered missing before: not asked again — no folder made for nothing — and each
        // run said to start in the keeper's cgroup, its own not tried)
        let mut missed = (self.scope.is_some() && !self.clone3).then_some((Step::Cgroup, libc::ENOSYS));
        let mut started: Option<(libc::pid_t, OwnedFd, u64, CString)> = None;
        if let Some(scope) = self.scope.as_ref().filter(|_| self.clone3) {
            match run_cgroup(scope.as_fd(), id) {
                Ok((name, dir, cgroup)) => {
                    let mut pidfd: libc::c_int = -1;
                    // SAFETY: the child is `child`, which allocates nothing and makes raw calls only on what was made
                    // before (`Exec`) — `clone_into`'s contract.
                    let pid = unsafe { clone_into(&mut pidfd, dir.as_raw_fd()) };
                    if pid == 0 {
                        child(&run, &exec, &fds, self.chan.as_raw_fd(), error_w.as_raw_fd());
                    }
                    if pid > 0 {
                        // SAFETY: the pidfd `clone3` made for us.
                        let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
                        started = Some((pid as libc::pid_t, pidfd, cgroup, name));
                    } else {
                        let e = errno();
                        // (refused by a filter, or no such call: not asked again)
                        self.clone3 &= e != libc::ENOSYS;
                        remove_cgroup(scope.as_fd(), &name);
                        missed = Some((Step::Fork, e));
                    }
                }
                Err(e) => missed = Some((Step::Cgroup, e)),
            }
        }
        let (pid, pidfd, cgroup, name) = match started {
            Some((pid, pidfd, cgroup, name)) => (pid, pidfd, cgroup, Some(name)),
            None => {
                // SAFETY: the child is `child` — raw calls on what was made before (`Exec`), as after a clone.
                let pid = unsafe { libc::fork() };
                if pid == 0 {
                    child(&run, &exec, &fds, self.chan.as_raw_fd(), error_w.as_raw_fd());
                }
                if pid < 0 {
                    return failed(self, Step::Fork, errno());
                }
                // SAFETY: plain call on our own child (not reaped yet: the pid is it).
                let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0u32) };
                if pidfd < 0 {
                    let e = errno();
                    // SAFETY: our own unreaped child.
                    unsafe { libc::kill(pid, libc::SIGKILL) };
                    self.children.insert(id, Child { pid, error: None, cgroup: None });
                    return failed(self, Step::Fork, e);
                }
                // SAFETY: just made, ours.
                (pid, unsafe { OwnedFd::from_raw_fd(pidfd as RawFd) }, 0, None)
            }
        };
        drop(error_w);
        drop(fds);
        self.children.insert(id, Child { pid, error: Some(error_r), cgroup: name });
        let started = Msg::Started { id, pid: pid as u32, cgroup, missed };
        let _ = self.say(&started, &[pidfd.as_fd()]);
    }

    /// Removes the cgroups of runs reaped (`emptied`) — keeping those still holding what a run left.
    fn empty_cgroups(&mut self) {
        let Some(scope) = &self.scope else { return };
        // (kept only while something is in it: one gone already is let go)
        self.emptied.retain(|name| {
            // SAFETY: a removal by name in the scope's folder, ours.
            let r = unsafe { libc::unlinkat(scope.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
            r != 0 && errno() == libc::EBUSY
        });
    }

    /// Reads run `id`'s failure pipe: a failure (step, errno) is said; its end (the exec closed it) closes it.
    fn failure(&mut self, id: u64) {
        let Some(c) = self.children.get_mut(&id) else { return };
        let Some(fd) = c.error.take() else { return };
        let mut b = [0u8; 5];
        // SAFETY: a buffer of its size for the call.
        let n = unsafe { libc::read(fd.as_raw_fd(), b.as_mut_ptr().cast(), b.len()) };
        if n == 5 {
            let step = channel::Step::from_code(b[0]).unwrap_or(Step::Exec);
            let errno = i32::from_le_bytes([b[1], b[2], b[3], b[4]]);
            let _ = self.say(&Msg::Failed { id, errno, step }, &[]);
        }
    }

    /// Every child that ended: a run's end said (`Exited`), an orphan's counted (`Reaped`).
    fn reap(&mut self) {
        let mut info = [0u8; 128];
        // SAFETY: the signalfd's records drained into a buffer of their size.
        while unsafe { libc::read(self.signals.as_raw_fd(), info.as_mut_ptr().cast(), info.len()) } > 0 {}
        loop {
            let mut status = 0;
            // SAFETY: an int for the call to fill.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
            if pid <= 0 {
                break;
            }
            match self.children.iter().find(|(_, c)| c.pid == pid).map(|(id, _)| *id) {
                Some(id) => {
                    // (a failure said before the end)
                    if self.children.get(&id).is_some_and(|c| c.error.is_some()) {
                        self.failure(id);
                    }
                    if let Some(name) = self.children.remove(&id).and_then(|c| c.cgroup) {
                        self.emptied.push(name);
                    }
                    let _ = self.say(&Msg::Exited { id, status }, &[]);
                }
                None => self.reaped += 1,
            }
        }
        if self.reaped > 0 {
            let _ = self.say(&Msg::Reaped { n: self.reaped }, &[]);
            self.reaped = 0;
        }
        // (a run's cgroup goes with its last process: the run's own, else the last thing it left)
        self.empty_cgroups();
    }

    /// `sig` to run `id`'s group (and itself) — only while unreaped: after, its pid may be another's.
    fn signal(&self, id: u64, sig: i32) {
        if let Some(c) = self.children.get(&id) {
            // SAFETY: signals to our own unreaped child and the group it leads.
            unsafe {
                libc::kill(-c.pid, sig);
                libc::kill(c.pid, sig);
            }
        }
    }

    /// Ends every run and what they left — TERM to each child and its group, KILL after `LEFT_TERM`, again
    /// while children are left (an orphan comes here once its parent is gone) — then exits.
    fn end_all(&mut self, me: u32) -> ! {
        let deadline = Instant::now() + LEFT_TERM * 3;
        while Instant::now() < deadline {
            let kids = crate::procs::children(me);
            if kids.is_empty() {
                break;
            }
            for (sig, wait) in [(libc::SIGTERM, LEFT_TERM), (libc::SIGKILL, LEFT_TERM)] {
                for &p in &crate::procs::children(me) {
                    // SAFETY: signals to our own children (unreaped: their pids are them) and their groups.
                    unsafe {
                        libc::kill(-(p as i32), sig);
                        libc::kill(p as i32, sig);
                    }
                }
                let until = Instant::now() + wait;
                while Instant::now() < until && !crate::procs::children(me).is_empty() {
                    let _ = readable(self.signals.as_fd(), Duration::from_millis(20));
                    self.reap();
                }
                if crate::procs::children(me).is_empty() {
                    break;
                }
            }
        }
        // (and with it every run's cgroup it could remove: `reap`'s last round)
        self.reap();
        std::process::exit(0)
    }
}

/// Run `id`'s own cgroup, made in the keeper's scope: its name there, its folder (for `clone3`) and its id (the
/// folder's inode: a cgroup's id on a 64-bit kernel) — or the errno that stopped it.
fn run_cgroup(scope: BorrowedFd, id: u64) -> Result<(CString, OwnedFd, u64), i32> {
    let name = CString::new(format!("run-{id}")).map_err(|_| libc::EINVAL)?;
    // SAFETY: a folder made and opened by name in the scope's folder, ours; a stat into a zeroed buffer.
    unsafe {
        if libc::mkdirat(scope.as_raw_fd(), name.as_ptr(), 0o755) != 0 {
            return Err(errno());
        }
        let fd = libc::openat(
            scope.as_raw_fd(),
            name.as_ptr(),
            libc::O_DIRECTORY | libc::O_RDONLY | libc::O_CLOEXEC,
        );
        if fd < 0 {
            let e = errno();
            remove_cgroup(scope, &name);
            return Err(e);
        }
        let dir = OwnedFd::from_raw_fd(fd);
        let mut st: libc::stat = std::mem::zeroed();
        if libc::fstat(dir.as_raw_fd(), &mut st) != 0 {
            let e = errno();
            remove_cgroup(scope, &name);
            return Err(e);
        }
        Ok((name, dir, st.st_ino))
    }
}

/// Removes the cgroup `name` in `scope` (one no process is in).
fn remove_cgroup(scope: BorrowedFd, name: &CString) {
    // SAFETY: a removal by name in the scope's folder, ours.
    unsafe { libc::unlinkat(scope.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
}

/// A child in the cgroup whose folder is `cgroup`, with its pidfd from the start (in `pidfd`): `clone3(CLONE_PIDFD |
/// CLONE_INTO_CGROUP)` — 0 in the child, its pid in the parent, -1 and errno on failure (`ENOSYS`: no such call, or
/// a filter refused it). Without a new stack, as a fork.
///
/// # Safety
/// The child is a copy of the calling process without fork's bookkeeping — glibc's atfork handlers don't run (its
/// malloc locks aren't reset: one another thread held at the clone stays held), its cached thread id is the
/// parent's: until its exec it must allocate and free nothing and make no call that needs a thread id (`raise`,
/// `pthread_kill`) — raw calls on what was made before the clone only, as `child` does.
unsafe fn clone_into(pidfd: &mut libc::c_int, cgroup: RawFd) -> libc::c_long {
    let args = CloneArgs {
        flags: libc::CLONE_PIDFD as u64 | CLONE_INTO_CGROUP,
        pidfd: (pidfd as *mut libc::c_int) as u64,
        exit_signal: libc::SIGCHLD as u64,
        cgroup: cgroup as u64,
        ..CloneArgs::default()
    };
    // SAFETY: a live `clone_args` of its size; the caller keeps the child as said.
    unsafe { libc::syscall(libc::SYS_clone3, &raw const args, std::mem::size_of::<CloneArgs>()) }
}

/// `capset`'s header, version 3 (two data blocks of effective, permitted, inheritable).
#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

/// Every capability gone, for good: the bounding set (an exec as root would take them back from it — this
/// needs `CAP_SETPCAP`, which only a root keeper has), the ambient set, then effective, permitted and
/// inheritable. tarae run as a user has none to drop; as root in a container its runs would hold root's. (A
/// keeper born in namespaces has dropped them all already: `capabilities_dropped`.)
fn drop_capabilities() {
    let h = CapHeader { version: 0x2008_0522, pid: 0 };
    let none = [0u32; 6];
    // SAFETY: plain prctls with integer arguments; capset with version 3's header and its two data blocks.
    unsafe {
        for cap in 0..64 {
            libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0);
        }
        libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0);
        libc::syscall(libc::SYS_capset, &raw const h, none.as_ptr());
    }
}

/// A birth's child in its user namespace (where it holds every capability, `CAP_SETPCAP` too) drops them all
/// before its exec: each bounding-set bit up to `last` (an exec as the namespace's root would take them back
/// from it), checked gone; the ambient set; effective, permitted and inheritable. Err: an errno. Raw calls only.
///
/// # Safety
/// Plain calls on this process; a birth's child may make them.
unsafe fn capabilities_dropped(last: libc::c_ulong) -> Result<(), i32> {
    let h = CapHeader { version: 0x2008_0522, pid: 0 };
    let none = [0u32; 6];
    // SAFETY: as above.
    unsafe {
        for cap in 0..=last {
            libc::prctl(libc::PR_CAPBSET_DROP, cap, 0, 0, 0);
        }
        for cap in 0..=last {
            if libc::prctl(libc::PR_CAPBSET_READ, cap, 0, 0, 0) == 1 {
                return Err(libc::EPERM);
            }
        }
        if libc::prctl(libc::PR_CAP_AMBIENT, libc::PR_CAP_AMBIENT_CLEAR_ALL, 0, 0, 0) != 0
            || libc::syscall(libc::SYS_capset, &raw const h, none.as_ptr()) != 0
        {
            return Err(errno());
        }
    }
    Ok(())
}

fn errno() -> i32 {
    io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO)
}

/// The keeper's own Rss and Pss (KB), from its `smaps_rollup`.
fn own_usage() -> (u64, u64) {
    let text = std::fs::read_to_string("/proc/self/smaps_rollup").unwrap_or_default();
    let kb = |name: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
            .unwrap_or(0)
    };
    (kb("Rss:"), kb("Pss:"))
}

/// A run's child, after the keeper's `clone3` or fork: its channel closed, dumpable again (finding 1: tarae takes
/// its listener with `pidfd_getfd`, which a non-dumpable process refuses without a namespace of tarae's), a session
/// of its own, its stdio, its folder, its name (what Landlock's audit names), every signal unblocked, its Landlock
/// rules, its seccomp filter — served, its listener's number handed to tarae and one byte back; held, one byte once
/// tarae knows its pid — then the exec. A failure on the way is written on `error` (step, errno) and in its stderr,
/// and it exits 127.
///
/// Nothing here allocates or frees: a raw `clone3`'s child has none of fork's bookkeeping — glibc's malloc locks
/// aren't reset in it, so one another thread held at the clone (a test's keeper has libtest's thread beside it)
/// would never be let go. Its strings and its seccomp program come made (`Exec`); a failure is said in fixed bytes
/// and its errno's digits (tarae says the errno's words, from `error`); every other call is a raw one.
/// (`a_runs_child_allocates_nothing_before_its_exec` holds it.)
fn child(run: &channel::Run, exec: &Exec, fds: &[OwnedFd], chan: RawFd, error: RawFd) -> ! {
    #[cfg(test)]
    tests::NO_ALLOC.store(true, std::sync::atomic::Ordering::Relaxed);
    let fail = |step: Step, errno: i32| -> ! {
        let mut b = [step.code(); 5];
        b[1..].copy_from_slice(&errno.to_le_bytes());
        // (the errno's digits, right to left, in a buffer of the stack's)
        let mut digits = [0u8; 12];
        let (mut n, mut at) = (errno.unsigned_abs(), digits.len());
        loop {
            at -= 1;
            digits[at] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        let parts: [&[u8]; 4] = [
            "tarae's sandbox couldn't start it — ".as_bytes(),
            step.said().as_bytes(),
            b": errno ",
            &digits[at..],
        ];
        // SAFETY: writes of buffers we own, then the end of this process.
        unsafe {
            libc::write(error, b.as_ptr().cast(), b.len());
            for p in parts {
                libc::write(2, p.as_ptr().cast(), p.len());
            }
            libc::write(2, b"\n".as_ptr().cast(), 1);
            libc::_exit(127)
        }
    };
    let (ruleset, stdin, stdout, stderr) = (&fds[0], &fds[1], &fds[2], &fds[3]);
    // SAFETY: plain calls on this process and descriptors it holds.
    unsafe {
        libc::close(chan);
        libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0);
        libc::setsid();
        for (from, to) in [(stdin, 0), (stdout, 1), (stderr, 2)] {
            if libc::dup2(from.as_raw_fd(), to) < 0 {
                fail(Step::Request, errno());
            }
        }
        if libc::chdir(exec.cwd.as_ptr()) != 0 {
            fail(Step::Chdir, errno());
        }
        libc::prctl(libc::PR_SET_NAME, exec.comm.as_ptr());
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        libc::pthread_sigmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
    }
    if let Err(e) = crate::landlock::restrict_self(ruleset.as_fd()) {
        fail(Step::Landlock, e.raw_os_error().unwrap_or(libc::EIO));
    }
    let listener = match exec.filter.apply() {
        Ok(l) => l,
        Err(e) => fail(Step::Seccomp, e.raw_os_error().unwrap_or(libc::EIO)),
    };
    let handoff = fds.get(4);
    if let (Some(l), Some(handoff)) = (&listener, handoff) {
        let n = (l.as_raw_fd() as u32).to_le_bytes();
        // SAFETY: a write of a buffer we own on the handoff socket (not a call the filter hands to tarae: no
        // address).
        if unsafe { libc::write(handoff.as_raw_fd(), n.as_ptr().cast(), n.len()) } != 4 {
            fail(Step::Handoff, errno());
        }
    }
    // (a byte once tarae has its listener, served; one once tarae knows its pid, held — in either order)
    let bytes = run.served() as usize + run.held as usize;
    if bytes > 0 {
        let Some(handoff) = handoff else { fail(Step::Request, libc::EINVAL) };
        let mut back = [0u8; 2];
        let mut got = 0;
        while got < bytes {
            // SAFETY: a read into a buffer we own, within it.
            let n = unsafe { libc::read(handoff.as_raw_fd(), back[got..].as_mut_ptr().cast(), bytes - got) };
            match n {
                n if n > 0 => got += n as usize,
                0 => fail(Step::Handoff, libc::EPIPE),
                _ if errno() == libc::EINTR => {}
                _ => fail(Step::Handoff, errno()),
            }
        }
    }
    drop(listener);
    // SAFETY: the program, argv and envp are NUL-terminated arrays of strings `exec` holds.
    unsafe { libc::execve(exec.program.as_ptr(), exec.argv_p.as_ptr(), exec.envp_p.as_ptr()) };
    fail(Step::Exec, errno())
}

/// What a run's child needs made — its strings as `chdir`, `prctl` and `execve` take them, its seccomp program —
/// made by the keeper before the run's process is: the child allocates nothing (`child`).
struct Exec {
    program: CString,
    cwd: CString,
    comm: CString,
    /// `argv_p` and `envp_p` point into these (their heap buffers: moving the `Vec`s moves none).
    _argv: Vec<CString>,
    _envp: Vec<CString>,
    argv_p: Vec<*const libc::c_char>,
    envp_p: Vec<*const libc::c_char>,
    /// Its filter, for its `net` (`keeper::mode`'s), made; the child only puts it on.
    filter: crate::seccomp::Filter,
}

impl Exec {
    /// `run`'s (its strings hold no NUL: `channel::decode` refused any) — Err: no filter for this machine.
    fn of(run: &channel::Run) -> io::Result<Exec> {
        let mode = match run.net {
            RunNet::Served => crate::seccomp::Mode::Served,
            RunNet::Routed => crate::seccomp::Mode::Routed,
            RunNet::Own => crate::seccomp::Mode::OwnNetwork,
            RunNet::Plain => crate::seccomp::Mode::Plain,
        };
        let filter = crate::seccomp::Filter::new(mode, run.no_userns)?;
        let c = |s: &[u8]| CString::new(s).unwrap_or_default();
        let argv: Vec<CString> = run.argv.iter().map(|a| c(a)).collect();
        let envp: Vec<CString> = run.envp.iter().map(|e| c(e)).collect();
        let ptrs = |v: &[CString]| -> Vec<*const libc::c_char> {
            v.iter().map(|s| s.as_ptr()).chain(std::iter::once(std::ptr::null())).collect()
        };
        let (argv_p, envp_p) = (ptrs(&argv), ptrs(&envp));
        Ok(Exec {
            program: c(&run.program),
            cwd: c(&run.cwd),
            comm: c(&run.comm),
            _argv: argv,
            _envp: envp,
            argv_p,
            envp_p,
            filter,
        })
    }
}

/// Tests: tarae's number for the process a run of `k`'s calls `inner` (what `$!` or `$$` says inside a PID
/// namespace) — the descendant of `k` whose `NSpid` ends with it; `inner` itself without one. Waits a second
/// for it (it must be alive: a gone one has no `NSpid` to find).
#[cfg(test)]
pub(crate) fn outer_pid(k: &Keeper, inner: u32) -> u32 {
    if !k.ns.pid {
        return inner;
    }
    let below = |mut p: u32| {
        for _ in 0..64 {
            match crate::procs::read(p).map(|o| o.ppid) {
                Some(pp) if pp == k.pid => return true,
                Some(pp) if pp > 1 => p = pp,
                _ => return false,
            }
        }
        false
    };
    let end = Instant::now() + Duration::from_secs(1);
    loop {
        for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Some(p) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
            let status = std::fs::read_to_string(format!("/proc/{p}/status")).unwrap_or_default();
            let nspid: Vec<&str> = status
                .lines()
                .find_map(|l| l.strip_prefix("NSpid:"))
                .map(|v| v.split_whitespace().collect())
                .unwrap_or_default();
            if nspid.len() > 1 && nspid.last() == Some(&inner.to_string().as_str()) && below(p) {
                return p;
            }
        }
        assert!(Instant::now() < end, "no process {inner} below keeper {}", k.pid);
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Tests: the keeper's number as its runs see it (their `$PPID`, a `stat`'s parent) — 1 in its PID namespace.
#[cfg(test)]
pub(crate) fn seen_by_runs(k: &Keeper) -> u32 {
    if k.ns.pid { 1 } else { k.pid }
}

#[cfg(test)]
mod tests {
    use super::super::handed::{received_fds, send_fds};
    use super::*;

    /// The keeper, in tests: the test binary run again on this (`keeper_argv`), with what tells it it's one.
    #[test]
    #[ignore = "a keeper: started by Keeper::born in tests"]
    fn keeper_process() {
        if std::env::var_os("TARAE_KEEPER").is_some() {
            main();
        }
    }

    /// The namespaces this system gives a keeper (`tier::get`): every test's keeper is born in them, so each
    /// test runs in the tier there is.
    fn ns() -> Ns {
        super::super::tier::get().ns
    }

    /// A keeper of a test's own, for a root no other test uses, in this system's namespaces.
    fn keeper(test: &str) -> Arc<Keeper> {
        keeper_in(test, ns())
    }

    /// `keeper`, born with `ns`.
    fn keeper_in(test: &str, ns: Ns) -> Arc<Keeper> {
        let root = std::env::temp_dir().join(format!("tarae-keeper-{}-{test}", std::process::id()));
        Keeper::born(&root, None, ns).expect("a keeper starts")
    }

    /// Whether the keeper is gone within `wait` — tarae told, and its process ended and reaped.
    fn gone_within(k: &Keeper, wait: Duration) -> bool {
        let end = Instant::now() + wait;
        let gone = || k.is_gone() && !std::path::Path::new(&format!("/proc/{}", k.pid)).exists();
        while !gone() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(10));
        }
        gone()
    }

    /// tarae's end of the channel closed (as at its quit, restart or crash: close-on-exec), the keeper ends
    /// within a second — it reads EOF as `End`.
    #[test]
    fn the_keeper_ends_with_its_channel() {
        let k = keeper("channel");
        assert!(!k.is_gone(), "there");
        // SAFETY: shuts our own end of the channel both ways (the descriptor stays ours, unused after).
        unsafe { libc::shutdown(k.chan.as_raw_fd(), libc::SHUT_RDWR) };
        assert!(gone_within(&k, Duration::from_secs(1) + LEFT_TERM), "ended with its channel, and reaped");
    }

    /// A ruleset granting everything (what these tests check isn't Landlock's) — None after saying the test skips
    /// itself where there's no Landlock to make one with.
    fn open_rules(test: &str) -> Option<crate::landlock::Ruleset> {
        let abi = match crate::landlock::abi() {
            Ok(n) if n >= 2 => n,
            _ => {
                let line = format!(
                    "{}sandbox::keeper::tests::{test} — no Landlock here\n",
                    crate::test_results::SELF_SKIP
                );
                let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
                return None;
            }
        };
        let mut r = crate::landlock::Ruleset::new(abi).unwrap();
        r.allow(Path::new("/"), crate::landlock::ALL).unwrap();
        Some(r)
    }

    /// `sh -c script` through `k` (plain: no supervisor): its output to its end, and how it ended (`spawn::wait`
    /// — through the keeper).
    fn sh_through(
        k: &Keeper,
        rules: &crate::landlock::Ruleset,
        script: &str,
    ) -> (String, std::process::ExitStatus) {
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", script]);
        let s = k.start(&cmd, &crate::landlock::thread_name(), rules.fd(), RunNet::Plain).unwrap();
        let mut out = String::new();
        io::Read::read_to_string(&mut s.spawned.stdout.unwrap(), &mut out).unwrap();
        (out, crate::spawn::wait(s.spawned.pid).unwrap())
    }

    /// A run holds its stdio and nothing else of the keeper's or tarae's: `/proc/self/fd` lists 0, 1, 2 and the
    /// lister's own.
    #[test]
    fn a_command_holds_only_its_stdio() {
        let Some(rules) = open_rules("a_command_holds_only_its_stdio") else { return };
        let k = keeper("stdio");
        let (out, status) = sh_through(&k, &rules, "exec ls -1 /proc/self/fd");
        assert!(status.success(), "{out}");
        assert_eq!(out, "0\n1\n2\n3\n", "stdio and ls's own folder only");
        k.end();
    }

    /// A run holds no capability — none effective, permitted, inheritable, ambient, nor in its bounding set (a
    /// root keeper's dropped, `drop_capabilities`; a user's has none).
    #[test]
    fn a_command_holds_no_capability() {
        let Some(rules) = open_rules("a_command_holds_no_capability") else { return };
        let k = keeper("caps");
        let (out, status) =
            sh_through(&k, &rules, "exec grep -E '^Cap(Inh|Prm|Eff|Bnd|Amb)' /proc/self/status");
        assert!(status.success(), "{out}");
        let held: Vec<&str> = out.lines().filter(|l| !l.ends_with("0000000000000000")).collect();
        assert_eq!((out.lines().count(), held), (5, Vec::<&str>::new()), "{out}");
        k.end();
    }

    /// How a run ended comes from its keeper (`spawn::wait`: it isn't tarae's child), and stopping it
    /// (`dap::term_group`) is the keeper's signal to its group: the run and a child of its own both go, the run
    /// ended by that TERM.
    #[test]
    fn exit_status_and_cancel_go_through_the_keeper() {
        use std::io::BufRead;
        use std::os::unix::process::ExitStatusExt;
        let Some(rules) = open_rules("exit_status_and_cancel_go_through_the_keeper") else { return };
        let k = keeper("cancel");
        let (_, status) = sh_through(&k, &rules, "exit 3");
        assert_eq!(status.code(), Some(3), "its exit status");
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "sleep 30 & echo $!; wait"]);
        let s = k.start(&cmd, &crate::landlock::thread_name(), rules.fd(), RunNet::Plain).unwrap();
        let mut lines = std::io::BufReader::new(s.spawned.stdout.unwrap()).lines();
        let child = outer_pid(&k, lines.next().unwrap().unwrap().trim().parse().unwrap());
        let pid = s.spawned.pid;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(crate::spawn::wait(pid)));
        crate::dap::term_group(pid);
        let status = rx.recv_timeout(Duration::from_secs(5)).expect("it ended once stopped").unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM), "ended by the stop");
        let end = Instant::now() + Duration::from_secs(3);
        while std::path::Path::new(&format!("/proc/{child}")).exists() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!std::path::Path::new(&format!("/proc/{child}")).exists(), "its own child went with it");
        k.end();
    }

    /// A served run of a keeper's: tarae takes its listener from the run's process (`pidfd_getfd` — which the
    /// run's being dumpable again allows, finding 1), and its supervisor serves it — a port the policy opens
    /// reached, another refused, that refusal out through `Refused`. As an ordinary user (`served_scene`): root
    /// with `CAP_SYS_PTRACE` would take it either way.
    #[test]
    fn tarae_takes_the_listener_of_a_keeper_child() {
        if let Some(why) = super::super::supervisor::unavailable() {
            let line = format!(
                "{}sandbox::keeper::tests::tarae_takes_the_listener_of_a_keeper_child — {why}\n",
                crate::test_results::SELF_SKIP
            );
            let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
            return;
        }
        let said = scene("served_scene", &[]);
        assert_eq!(said, "served refused · told network-outbound remote:192.0.2.1:9");
    }

    /// `tarae_takes_the_listener_of_a_keeper_child`'s scene, as an ordinary user: a project whose policy opens
    /// one port of a listener here, a served run of a keeper's that connects to it and beyond this machine (a
    /// documentation address, the network closed: refused in every tier — a closed port of this machine's is
    /// the session's own in its netns). Prints `scene: <what the run said> · told <the refusal's op and
    /// target>`.
    #[test]
    #[ignore = "run by tarae_takes_the_listener_of_a_keeper_child in a process of its own"]
    fn served_scene() {
        if std::env::var_os(SCENE).is_none() {
            return;
        }
        ordinary_user();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let root = std::env::temp_dir().join(format!("tarae-keeper-served-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let env = super::super::Env {
            root: root.clone(),
            tmp: Some(super::super::profile_path(&std::env::temp_dir())),
            ports: vec![port],
            run_env: super::super::run_env_of(&super::super::Here::now(), &crate::llm::LlmConfig::default()),
            ..Default::default()
        };
        let p = super::super::Policy::for_project(&env);
        let abi = crate::landlock::abi().unwrap();
        let k = keeper_in("served", super::super::tier::for_policy(&p));
        let c = super::super::linux::confine(&p, abi, Err("not watched".into()), false, Some(&k), Ok(()))
            .unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut cmd = std::process::Command::new("bash");
        let script = "if : 3<>/dev/tcp/127.0.0.1/$P; then printf served; fi
            if ! { : 4<>/dev/tcp/192.0.2.1/9; } 2>/dev/null; then printf ' refused'; fi";
        cmd.args(["-c", script]).env("P", port.to_string()).current_dir(&root);
        let name = crate::landlock::thread_name();
        let s = k.start_served(&cmd, &name, c.rules_fd(), c.net, Arc::new(p), tx).unwrap();
        let mut out = String::new();
        io::Read::read_to_string(&mut s.spawned.stdout.unwrap(), &mut out).unwrap();
        let mut err = String::new();
        io::Read::read_to_string(&mut s.spawned.stderr.unwrap(), &mut err).unwrap();
        crate::spawn::wait(s.spawned.pid).unwrap();
        let told = rx.recv_timeout(Duration::from_secs(5)).map(|d| format!("{} {}", d.op, d.target));
        let err: Vec<&str> = err.lines().collect();
        println!("scene: {out}{} · told {}", err.join(" / "), told.unwrap_or_else(|e| e.to_string()));
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// What a run leaves running is its keeper's — not a tag's: a run starts `env -i setsid sleep` (no tag, a
    /// session of its own) and ends; the sleep's parent is the keeper (its child subreaper), it's among the
    /// keeper's leftovers, and the keeper's end ends it.
    #[test]
    fn leftovers_are_the_keepers_children() {
        let Some(rules) = open_rules("leftovers_are_the_keepers_children") else { return };
        let k = keeper("leftovers");
        let (out, status) = sh_through(&k, &rules, "env -i setsid sleep 30 >/dev/null 2>&1 & echo $!");
        assert!(status.success(), "{out}");
        let sleep = outer_pid(&k, out.trim().parse().unwrap());
        let parent = || crate::procs::read(sleep).map(|o| o.ppid);
        let end = Instant::now() + Duration::from_secs(3);
        while parent() != Some(k.pid) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(parent(), Some(k.pid), "the keeper's child, its run gone");
        assert_eq!(k.leftovers(), [sleep], "what its runs left");
        k.end();
        assert!(!std::path::Path::new(&format!("/proc/{sleep}")).exists(), "ended with its keeper");
    }

    /// A keeper killed under a run fails closed: the run's wait says the keeper ended (it isn't left waiting),
    /// the run is ended too, and the next run's policy makes a new keeper, whose run goes. A second death within
    /// `DEATHS_WITHIN` and none is made again (D11): the run doesn't start, saying why — never unsandboxed.
    #[test]
    fn a_killed_keeper_fails_closed_and_is_remade() {
        let Some(rules) = open_rules("a_killed_keeper_fails_closed_and_is_remade") else { return };
        let root = std::env::temp_dir().join(format!("tarae-keeper-killed-{}", std::process::id()));
        let kill = |k: &Keeper| {
            // SAFETY: KILL to the keeper of the test's own root.
            unsafe { libc::kill(k.pid as i32, libc::SIGKILL) };
            assert!(gone_within(k, Duration::from_secs(2)), "it's gone");
        };
        let k = ready(&root, Some("a key"), ns()).unwrap();
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");
        let run =
            k.start(&cmd, &crate::landlock::thread_name(), rules.fd(), RunNet::Plain).unwrap().spawned.pid;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || tx.send(crate::spawn::wait(run).map_err(|e| e.to_string())));
        kill(&k);
        let waited = rx.recv_timeout(Duration::from_secs(2)).expect("its wait answered");
        assert!(waited.as_ref().is_err_and(|e| e.contains("the session's keeper ended")), "{waited:?}");
        let end = Instant::now() + Duration::from_secs(3);
        while crate::procs::read(run).is_some_and(|o| !o.zombie) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(crate::procs::read(run).is_none_or(|o| o.zombie), "the run ended with it");
        let again = ready(&root, Some("a key"), ns()).expect("made again");
        assert_ne!(again.pid, k.pid);
        let (_, status) = sh_through(&again, &rules, "exit 0");
        assert!(status.success(), "a run of the new one goes");
        kill(&again);
        let refused = ready(&root, Some("a key"), ns()).expect_err("not made a third time");
        assert!(refused.contains("tarae stopped making it"), "{refused}");
    }

    /// A dead keeper's run is killed through the pidfd tarae holds, never by its number alone: with that pidfd's
    /// process gone, a live process (here in a group of its own) given as its number — as a number reused would
    /// be — is left alone; given its own pidfd, it's killed.
    #[test]
    fn a_dead_keepers_run_is_killed_by_its_pidfd_only() {
        use std::os::unix::process::CommandExt;
        let pidfd = |pid: u32| {
            // SAFETY: a pidfd of our own unreaped child, owned below.
            let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0u32) };
            assert!(fd >= 0, "{}", io::Error::last_os_error());
            // SAFETY: just made.
            unsafe { OwnedFd::from_raw_fd(fd as RawFd) }
        };
        let mut gone = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let gone_fd = pidfd(gone.id());
        gone.kill().unwrap();
        gone.wait().unwrap();
        let mut other = std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap();
        let other_fd = pidfd(other.id());
        kill_run(other.id(), &gone_fd);
        assert!(!readable(other_fd.as_fd(), Duration::from_millis(300)).unwrap(), "left alone");
        kill_run(other.id(), &other_fd);
        assert!(readable(other_fd.as_fd(), Duration::from_secs(5)).unwrap(), "killed by its own pidfd");
        other.wait().unwrap();
    }

    /// A keeper's birth (≤ `READY_WITHIN`, here made a second longer) holds up nothing that reads the keepers —
    /// `of`, which `:procs` and `end_leftovers` ask: it answers while the birth is under way on a worker, and
    /// finds that keeper once born.
    #[test]
    fn a_birth_under_way_holds_up_no_reader() {
        let root = std::env::temp_dir().join(format!("tarae-keeper-slow-{}", std::process::id()));
        let (tx, began) = std::sync::mpsc::channel();
        *lock(&SLOW_BIRTH) = Some((root.clone(), tx));
        let r = root.clone();
        let birth = std::thread::spawn(move || ready(&r, Some("a key"), ns()));
        began.recv_timeout(Duration::from_secs(5)).expect("its birth began");
        let at = Instant::now();
        let _ = of(None);
        let took = at.elapsed();
        *lock(&SLOW_BIRTH) = None;
        let k = birth.join().unwrap().expect("born");
        assert!(took < Duration::from_millis(300), "`of` waited {took:?} on a birth");
        assert!(of(Some(&root)).iter().any(|o| Arc::ptr_eq(o, &k)), "found once born");
        k.end();
    }

    /// Keepers told to end together end in the time one takes (`end_together`, tarae's quit): two, each with a
    /// run that ignores TERM (so each waits `LEFT_TERM` before its KILL), end within one `LEFT_TERM` bound, not
    /// two.
    #[test]
    fn keepers_end_together_not_one_after_another() {
        use std::io::BufRead;
        let Some(rules) = open_rules("keepers_end_together_not_one_after_another") else { return };
        let keepers: Vec<Arc<Keeper>> = (0..2).map(|i| keeper(&format!("together-{i}"))).collect();
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "trap '' TERM; echo ready; exec sleep 30"]);
        let mut outs = Vec::new();
        for k in &keepers {
            let s = k.start(&cmd, &crate::landlock::thread_name(), rules.fd(), RunNet::Plain).unwrap();
            let mut lines = std::io::BufReader::new(s.spawned.stdout.unwrap()).lines();
            assert_eq!(lines.next().unwrap().unwrap(), "ready", "TERM ignored from now");
            outs.push(lines);
        }
        let at = Instant::now();
        end_together(&keepers);
        let took = at.elapsed();
        assert!(keepers.iter().all(|k| k.is_gone()), "all ended");
        assert!(took >= LEFT_TERM, "each waited for TERM first: {took:?}");
        assert!(took < LEFT_TERM * 2, "together, not one after another: {took:?}");
    }

    /// D14: a run of another policy key gets a new keeper, and the old one is superseded — what its runs left
    /// ends now, a run still going is kept (it's asked no more runs), and it ends after that last run. One
    /// superseded while idle ends at once.
    #[test]
    fn a_key_change_supersedes_the_keeper() {
        use std::io::BufRead;
        let Some(rules) = open_rules("a_key_change_supersedes_the_keeper") else { return };
        let root = std::env::temp_dir().join(format!("tarae-keeper-superseded-{}", std::process::id()));
        let old = ready(&root, Some("key 1"), ns()).unwrap();
        let (out, status) = sh_through(&old, &rules, "setsid sleep 30 >/dev/null 2>&1 & echo $!");
        assert!(status.success(), "{out}");
        let left = outer_pid(&old, out.trim().parse().unwrap());
        let end = Instant::now() + Duration::from_secs(3);
        while crate::procs::read(left).map(|o| o.ppid) != Some(old.pid) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(old.leftovers(), [left], "left by its run");
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo going; exec sleep 30"]);
        let going = old.start(&cmd, &crate::landlock::thread_name(), rules.fd(), RunNet::Plain).unwrap();
        let mut lines = std::io::BufReader::new(going.spawned.stdout.unwrap()).lines();
        assert_eq!(lines.next().unwrap().unwrap(), "going");
        let new = ready(&root, Some("key 2"), ns()).unwrap();
        assert_ne!(new.pid, old.pid, "a new keeper for the new key");
        let ended = |pid: u32, within: Duration| {
            let end = Instant::now() + within;
            while crate::procs::read(pid).is_some_and(|o| !o.zombie) && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(20));
            }
            crate::procs::read(pid).is_none_or(|o| o.zombie)
        };
        assert!(ended(left, LEFT_TERM * 3), "what its runs left ended now");
        assert!(!old.is_gone(), "kept for its run going");
        let refused = old.start(&cmd, &crate::landlock::thread_name(), rules.fd(), RunNet::Plain).err();
        assert!(refused.is_some_and(|e| e.to_string().contains("replaced")), "no run more");
        crate::dap::term_group(going.spawned.pid);
        crate::spawn::wait(going.spawned.pid).unwrap();
        assert!(gone_within(&old, END_WITHIN), "ended after its last run");
        let third = ready(&root, Some("key 3"), ns()).unwrap();
        assert!(gone_within(&new, END_WITHIN), "an idle one superseded ends at once");
        third.end();
    }

    /// Where `clone3` answers `ENOSYS` (Docker's default filter does, so libcs fall back), a keeper is born
    /// through `clone(2)` (`clone_pidfd`): `clone3` refused so in a process of its own (`clone3_refused_scene`, a
    /// seccomp filter of its own on the thread that makes the keeper), the keeper still says it's there.
    #[test]
    fn a_keeper_is_born_where_clone3_is_refused() {
        assert_eq!(scene("clone3_refused_scene", &[]), "clone3=38 born=yes");
    }

    /// `a_keeper_is_born_where_clone3_is_refused`'s scene: this thread's `clone3` answered `ENOSYS` (a filter of
    /// its own — the whole process is the scene's), then a keeper born. Prints `scene: clone3=<errno> born=…`.
    #[test]
    #[ignore = "run by a_keeper_is_born_where_clone3_is_refused in a process of its own"]
    fn clone3_refused_scene() {
        if std::env::var_os(SCENE).is_none() {
            return;
        }
        let op = |code: u32, k: u32, jt: u8, jf: u8| libc::sock_filter { code: code as u16, jt, jf, k };
        let ret = libc::BPF_RET | libc::BPF_K;
        // (the syscall's number at offset 0 of `seccomp_data`: clone3 → ENOSYS, the rest allowed)
        let filter = [
            op(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0, 0, 0),
            op(libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K, libc::SYS_clone3 as u32, 0, 1),
            op(ret, libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32, 0, 0),
            op(ret, libc::SECCOMP_RET_ALLOW, 0, 0),
        ];
        let prog = libc::sock_fprog { len: filter.len() as u16, filter: filter.as_ptr().cast_mut() };
        // SAFETY: plain prctls on this thread; the program is live for the call.
        unsafe {
            assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
            let r = libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &raw const prog);
            assert_eq!(r, 0, "{}", io::Error::last_os_error());
        }
        // SAFETY: a call that fails before it does anything (no arguments it reads).
        let clone3 = unsafe { libc::syscall(libc::SYS_clone3, std::ptr::null::<CloneArgs>(), 0usize) };
        let errno = if clone3 < 0 { errno() } else { 0 };
        let born = match Keeper::born(Path::new("/tarae-keeper-clone3"), None, ns()) {
            Ok(k) => {
                k.end();
                "yes".to_string()
            }
            Err(e) => e.to_string(),
        };
        println!("scene: clone3={errno} born={born}");
    }

    /// A keeper says what it uses (`AskUsage` → `Usage`): its own Rss and Pss, which nobody else can read.
    #[test]
    fn a_keeper_says_what_it_uses() {
        let k = keeper("usage");
        let (rss, pss) = k.usage().expect("it answered");
        assert!(rss > 0 && pss > 0 && pss <= rss, "Rss {rss} KB, Pss {pss} KB");
        k.end();
    }

    /// `:procs` names each session's keeper (CLAUDE.md: a new kind of child process gets a line there).
    #[test]
    fn the_sessions_keeper_is_known() {
        let root = std::env::temp_dir().join(format!("tarae-keeper-known-{}", std::process::id()));
        let k = ready(&root, Some("a key"), ns()).unwrap();
        let ed = crate::editor::Editor::new(crate::config::Config::default());
        let known = ed.procs_known();
        let line = known.iter().find(|n| n.pid == k.pid).expect("a line for it");
        assert_eq!(line.title, "sandbox keeper");
        assert!(line.what.starts_with("the session's keeper · "), "{}", line.what);
        crate::sandbox::end_leftovers(Some(&root));
        let known = ed.procs_known();
        assert!(!known.iter().any(|n| n.pid == k.pid), "gone once it ended");
    }

    /// A project's policy with these ports open and these (empty) files, and its Landlock rules for `keeper`'s
    /// runs (`linux::confine`), served as this system serves them: (the policy, the rules, the project).
    fn project_in(
        name: &str,
        ports: Vec<u16>,
        keeper: Option<&Keeper>,
        files: &[&str],
    ) -> (super::super::Policy, super::super::linux::Confine, PathBuf) {
        project_served(name, ports, keeper, files, super::super::linux::served_here())
    }

    /// `project_in`, served or not as `serve` says.
    fn project_served(
        name: &str,
        ports: Vec<u16>,
        keeper: Option<&Keeper>,
        files: &[&str],
        serve: Result<(), String>,
    ) -> (super::super::Policy, super::super::linux::Confine, PathBuf) {
        let root = std::env::temp_dir().join(format!("tarae-keeper-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        for f in files {
            std::fs::write(root.join(f), "").unwrap();
        }
        let env = super::super::Env {
            root: root.clone(),
            tmp: Some(super::super::profile_path(&std::env::temp_dir())),
            ports,
            run_env: super::super::run_env_of(&super::super::Here::now(), &crate::llm::LlmConfig::default()),
            ..Default::default()
        };
        let p = super::super::Policy::for_project(&env);
        let abi = crate::landlock::abi().unwrap();
        let c =
            super::super::linux::confine(&p, abi, Err("not watched".into()), false, keeper, serve).unwrap();
        (p, c, root)
    }

    /// The median of `n` times `f` took, in ms.
    fn median_ms(n: usize, mut f: impl FnMut()) -> f64 {
        let mut t: Vec<f64> = (0..n)
            .map(|_| {
                let at = Instant::now();
                f();
                at.elapsed().as_secs_f64() * 1000.0
            })
            .collect();
        t.sort_by(f64::total_cmp);
        t[n / 2]
    }

    /// This process's resident memory, as its status says it.
    fn rss() -> String {
        let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        s.lines().find_map(|l| l.strip_prefix("VmRSS:")).map(|v| v.trim().to_string()).unwrap_or_default()
    }

    /// The namespaces to measure in, A/B on one kernel: none, and this system's when it has any.
    fn measured_in() -> Vec<Ns> {
        let mut each = vec![Ns::NONE];
        each.extend(Some(ns()).filter(|n| n.user));
        each
    }

    /// M1 (`docs/design/sandbox-keeper.md` §Measured): a sandboxed `true`, from asking to its exit status,
    /// median of 50 — through the keeper, plain and served, without namespaces and in this system's (the
    /// confining thread and `posix_spawn` it replaced were measured beside it before they went: the doc has
    /// both).
    #[test]
    fn perf_a_run_through_the_keeper() {
        if crate::test_results::unoptimized("sandbox::keeper::tests::perf_a_run_through_the_keeper") {
            return;
        }
        if open_rules("perf_a_run_through_the_keeper").is_none() {
            return;
        }
        let served = super::super::supervisor::unavailable().is_none();
        let mut said = Vec::new();
        for ns in measured_in() {
            let k = keeper_in("perf", ns);
            let (p, c, root) = project_in("perf", Vec::new(), Some(&k), &[]);
            let (_, plain, _) =
                project_served("perf", Vec::new(), Some(&k), &[], Err("measured plain".into()));
            let mut cmd = std::process::Command::new("true");
            cmd.current_dir(&root);
            let policy = Arc::new(p);
            let (tx, _rx) = std::sync::mpsc::channel();
            let keeper_plain = median_ms(50, || {
                let s = k.start(&cmd, &crate::landlock::thread_name(), plain.rules_fd(), plain.net).unwrap();
                crate::spawn::wait(s.spawned.pid).unwrap();
            });
            let keeper_served = served.then(|| {
                median_ms(50, || {
                    let s = k
                        .start_served(
                            &cmd,
                            &crate::landlock::thread_name(),
                            c.rules_fd(),
                            c.net,
                            policy.clone(),
                            tx.clone(),
                        )
                        .unwrap();
                    crate::spawn::wait(s.spawned.pid).unwrap();
                })
            });
            let ms = |m: Option<f64>| m.map_or("not served here".into(), |m| format!("{m:.2} ms"));
            said.push(format!("{}: plain {keeper_plain:.2} ms, served {}", k.said(), ms(keeper_served)));
            k.end();
            std::fs::remove_dir_all(&root).ok();
        }
        let line = format!(
            "M1, `true` asked to its status, median of 50 (this process {}) — {}\n",
            rss(),
            said.join(" · ")
        );
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
    }

    /// `/proc/meminfo`'s `MemAvailable` and `Slab`, in KB.
    fn meminfo() -> (i64, i64) {
        let m = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let kb = |name: &str| -> i64 {
            m.lines()
                .find_map(|l| l.strip_prefix(name))
                .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
                .unwrap_or(0)
        };
        (kb("MemAvailable:"), kb("Slab:"))
    }

    /// A keeper's birth to `Ready`, median of 20, and an idle keeper's own Rss and Pss (its `Usage`) with 1, 4
    /// and 8 at once (M3) — without namespaces and in this system's; with 8, what the system's `MemAvailable`
    /// and `Slab` moved by (the kernel's share — namespaces, a mount table's copy — which Pss doesn't count; a
    /// rough bound: anything else on the machine moves them too).
    #[test]
    fn perf_a_keepers_birth_and_memory() {
        if crate::test_results::unoptimized("sandbox::keeper::tests::perf_a_keepers_birth_and_memory") {
            return;
        }
        let root = std::env::temp_dir().join(format!("tarae-keeper-birth-{}", std::process::id()));
        let mut lines = Vec::new();
        for ns in measured_in() {
            let mut born = Vec::new();
            let birth = median_ms(20, || born.push(Keeper::born(&root, None, ns).unwrap()));
            for k in born.drain(..) {
                k.end();
            }
            let mut said = Vec::new();
            for n in [1, 4, 8] {
                let before = meminfo();
                let ks: Vec<Arc<Keeper>> = (0..n).map(|_| Keeper::born(&root, None, ns).unwrap()).collect();
                std::thread::sleep(Duration::from_millis(200));
                let after = meminfo();
                let used: Vec<(u64, u64)> = ks.iter().filter_map(|k| k.usage()).collect();
                let (rss, pss) = used.iter().fold((0, 0), |a, u| (a.0 + u.0, a.1 + u.1));
                let system = match n {
                    8 => {
                        format!(
                            " (MemAvailable {:+} KB, Slab {:+} KB)",
                            after.0 - before.0,
                            after.1 - before.1
                        )
                    }
                    _ => String::new(),
                };
                said.push(format!("{n}: Rss {rss} KB, Pss {pss} KB in all{system}"));
                for k in ks {
                    k.end();
                }
            }
            lines.push(format!(
                "{}: birth {birth:.2} ms · idle keepers' own use — {}",
                ns.said(),
                said.join(" · ")
            ));
        }
        let line = format!(
            "a keeper's birth to Ready, median of 20 (this process {}) — {}\n",
            rss(),
            lines.join(" ‖ ")
        );
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
    }

    /// The keeper is non-dumpable: its `/proc/<pid>/environ`, and where its descriptors point, are refused to
    /// a process of its own user — a dumpable one's aren't (Landlock alone would hide the difference from a
    /// run). As an ordinary user (`dumpable_scene`): as root, capabilities decide first.
    #[test]
    fn the_keeper_is_not_dumpable() {
        let said = scene("dumpable_scene", &[]);
        let eacces = libc::EACCES;
        assert_eq!(said, format!("keeper environ={eacces} fd={eacces} · dumpable environ=ok fd=ok"));
    }

    /// `the_keeper_is_not_dumpable`'s scene, in a process of its own as an ordinary user (`ordinary_user`):
    /// what reading a keeper's `/proc/<pid>/environ` and `fd/0` came to, and a dumpable process's of the same
    /// user. Prints `scene: keeper environ=<ok|errno> fd=… · dumpable environ=… fd=…`.
    #[test]
    #[ignore = "run by the_keeper_is_not_dumpable in a process of its own"]
    fn dumpable_scene() {
        if std::env::var_os(SCENE).is_none() {
            return;
        }
        ordinary_user();
        // (a keeper with no namespaces: in a user namespace this process, its owner, holds every capability and
        // reads it anyway — the flag is the same in every tier; there the runs inside are kept off by having no
        // capability, and Landlock's ptrace rule)
        let k = keeper_in("dumpable", Ns::NONE);
        let mut mine = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        let read = |pid: u32| {
            let environ = tried(std::fs::read(format!("/proc/{pid}/environ")));
            let fd = tried(std::fs::read_link(format!("/proc/{pid}/fd/0")));
            format!("environ={environ} fd={fd}")
        };
        println!("scene: keeper {} · dumpable {}", read(k.pid), read(mine.id()));
        k.end();
        mine.kill().ok();
        mine.wait().ok();
    }

    /// What tells a scene it was started by its test.
    const SCENE: &str = "TARAE_KEEPER_SCENE";

    /// The test binary run on the ignored scene `name` (`env` set besides): what it printed after `scene: `.
    fn scene(name: &str, env: &[(&str, &str)]) -> String {
        said_by(name, scene_cmd(name, env))
    }

    /// The command `scene` runs.
    fn scene_cmd(name: &str, env: &[(&str, &str)]) -> std::process::Command {
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", &format!("sandbox::keeper::tests::{name}"), "--ignored", "--nocapture"])
            .args(["--test-threads=1"])
            .env(SCENE, "1")
            .envs(env.iter().copied());
        cmd
    }

    /// What `cmd` — scene `name`'s — printed after `scene: `.
    fn said_by(name: &str, mut cmd: std::process::Command) -> String {
        let out = cmd.output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let err = String::from_utf8_lossy(&out.stderr);
        text.lines()
            .find_map(|l| l.split_once("scene: ").map(|(_, s)| s.to_string()))
            .unwrap_or_else(|| panic!("{name} said nothing:\n{text}\n{err}"))
    }

    /// As root (a container's tests), this process becomes `nobody` — every thread (glibc's `setresuid`), every
    /// capability gone: an ordinary user, as tarae runs. A scene's only: it changes the whole process.
    fn ordinary_user() {
        ordinary_user_as(65534);
    }

    /// `ordinary_user`, as uid and gid `id` (1000 for the tier scenes: inside a user namespace an unmapped id
    /// shows as 65534, so with 65534 a working map couldn't be told from none). Dumpable again after: changing
    /// its ids made it non-dumpable, which a process started as that user isn't — and a non-dumpable tarae's
    /// keeper has its `/proc` files root's, its id maps unwritable (`tier::why` says so).
    fn ordinary_user_as(id: u32) {
        // SAFETY: plain calls on this process (a scene's own).
        unsafe {
            if libc::geteuid() == 0 {
                assert_eq!(libc::setgroups(0, std::ptr::null()), 0, "setgroups");
                assert_eq!(libc::setresgid(id, id, id), 0, "setresgid");
                assert_eq!(libc::setresuid(id, id, id), 0, "setresuid");
                assert_eq!(libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0), 0, "dumpable");
            }
        }
    }

    /// An attempt as a scene says it: `ok`, or its errno.
    fn tried<T>(r: io::Result<T>) -> String {
        match r {
            Ok(_) => "ok".into(),
            Err(e) => e.raw_os_error().map_or_else(|| e.to_string(), |n| n.to_string()),
        }
    }

    // ── the session's namespaces ───────────────────────────────────────────────────────────────────────

    /// Whether this system gives a keeper the session's namespaces — else says the test skips itself, with the
    /// probe's reason (on a machine without them that line is the proof the reason is right). A machine said to
    /// give them (`TARAE_EXPECT_TIER`, as `the_probe_here` checks) fails instead: a birth broken into tier C
    /// isn't a skip there.
    fn namespaced(test: &str) -> bool {
        if ns().user {
            return true;
        }
        let why = super::super::tier::get().short.clone().unwrap_or_default();
        if let Ok(want) = std::env::var("TARAE_EXPECT_TIER").map(|w| w.trim().to_string())
            && ["A+", "A", "B+", "B"].contains(&want.as_str())
        {
            panic!("{test}: tier {want} expected here, the probe found none — {why}");
        }
        let line = format!(
            "{}sandbox::keeper::tests::{test} — no namespaces here: {why}\n",
            crate::test_results::SELF_SKIP
        );
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
        false
    }

    /// What tells the test binary it's a run's program (`helper`), and which.
    const HELPER: &str = "TARAE_KEEPER_HELPER";

    /// The test binary as a run of `k`'s under `rules` (made for `net`), in `cwd`, on `helper` doing `what` (with
    /// `env` set): what it printed after `helper: `.
    fn helper_through(
        k: &Keeper,
        rules: BorrowedFd,
        net: RunNet,
        what: &str,
        env: &[(&str, &str)],
        cwd: Option<&Path>,
    ) -> String {
        let cmd = helper_cmd(what, env, cwd);
        helper_said(k.start(&cmd, &crate::landlock::thread_name(), rules, net).unwrap())
    }

    /// The test binary on `helper` doing `what` (with `env` set, in `cwd`).
    fn helper_cmd(what: &str, env: &[(&str, &str)], cwd: Option<&Path>) -> std::process::Command {
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "sandbox::keeper::tests::helper",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER, what)
        .envs(env.iter().copied());
        if let Some(d) = cwd {
            cmd.current_dir(d);
        }
        cmd
    }

    /// What the helper run `s` printed after `helper: `, once it ended.
    fn helper_said(s: Started) -> String {
        let (mut out, mut err) = (String::new(), String::new());
        io::Read::read_to_string(&mut s.spawned.stdout.unwrap(), &mut out).unwrap();
        io::Read::read_to_string(&mut s.spawned.stderr.unwrap(), &mut err).unwrap();
        crate::spawn::wait(s.spawned.pid).unwrap();
        out.lines()
            .find_map(|l| l.split_once("helper: ").map(|(_, s)| s.to_string()))
            .unwrap_or_else(|| panic!("the helper said nothing:\n{out}\n{err}"))
    }

    /// A run's program in these tests: the test binary, doing what `HELPER` says — `pty`: a pty opened, its
    /// device and what `/dev/pts` lists (`dev=<st_dev> listed=<entries>`); `mounts`: a pty, a file made and
    /// removed in `/dev/shm`, `/proc/self/status` read, `$TARAE_PROTECTED` opened to write (each `ok` or its
    /// errno); `loopback`: a listener of its own on `127.0.0.1` reached, and `127.0.0.1:$TARAE_PORT`.
    #[test]
    #[ignore = "a run's program in the keeper's tests (helper_through)"]
    fn helper() {
        use std::os::unix::fs::MetadataExt;
        let Ok(what) = std::env::var(HELPER) else { return };
        let said = match what.as_str() {
            "pty" => match pty_opened() {
                Ok((_m, _s, path)) => {
                    let dev = std::fs::metadata(&path).map(|m| m.dev()).unwrap_or(0);
                    let mut listed: Vec<String> = std::fs::read_dir("/dev/pts")
                        .into_iter()
                        .flatten()
                        .flatten()
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect();
                    listed.sort();
                    format!("dev={dev} listed={}", listed.join(","))
                }
                Err(e) => format!("pty={}", tried::<()>(Err(e))),
            },
            "mounts" => {
                let shm = format!("/dev/shm/tarae-helper-{}", std::process::id());
                let made = std::fs::write(&shm, "x").and_then(|()| std::fs::remove_file(&shm));
                let protected = std::env::var("TARAE_PROTECTED").unwrap_or_default();
                format!(
                    "pty={} shm={} proc={} protected={}",
                    tried(pty_opened()),
                    tried(made),
                    tried(std::fs::read_to_string("/proc/self/status")),
                    tried(std::fs::OpenOptions::new().write(true).open(protected)),
                )
            }
            "loopback" => {
                let own = (|| {
                    let l = std::net::TcpListener::bind("127.0.0.1:0")?;
                    std::net::TcpStream::connect(l.local_addr()?).map(drop)
                })();
                let port: u16 = std::env::var("TARAE_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(9);
                format!(
                    "own={} tarae's={}",
                    tried(own),
                    tried(std::net::TcpStream::connect(("127.0.0.1", port)))
                )
            }
            "listen" => return helper_listen(),
            "connect" => helper_connect(&std::env::var("TARAE_TO").unwrap_or_default()),
            "origin" => helper_origin(&std::env::var("TARAE_TO").unwrap_or_default()),
            "swap" => helper_swap(&std::env::var("TARAE_TO").unwrap_or_default()),
            "proxied" => helper_proxied(&std::env::var("TARAE_TO").unwrap_or_default()),
            "bound" => helper_bound(&std::env::var("TARAE_TO").unwrap_or_default()),
            "swaplisten" => helper_swap_listen(&std::env::var("TARAE_TO").unwrap_or_default()),
            "racelo" => helper_race_lo(&std::env::var("TARAE_TO").unwrap_or_default()),
            "foreign" => helper_foreign(),
            "protocols" => helper_protocols(),
            "udp" => helper_udp(&std::env::var("TARAE_TO").unwrap_or_default()),
            "within" => helper_within(&std::env::var("TARAE_TO").unwrap_or_default()),
            "cgroupwrites" => helper_cgroup_writes(),
            // (`within` once `$TARAE_GO` is there — what the test wanted in place first is: 5 s at most)
            "within_when" => {
                let go = std::env::var_os("TARAE_GO").unwrap_or_default();
                let by = Instant::now() + Duration::from_secs(5);
                while !Path::new(&go).exists() && Instant::now() < by {
                    std::thread::sleep(Duration::from_millis(10));
                }
                helper_within(&std::env::var("TARAE_TO").unwrap_or_default())
            }
            "asked" => helper_asked(&std::env::var("TARAE_TO").unwrap_or_default()),
            "named" => helper_named(&std::env::var("TARAE_NAME").unwrap_or_default()),
            "searched" => helper_searched(&std::env::var("TARAE_TO").unwrap_or_default()),
            // (6e: a run's own DNS server at the session's 127.0.0.1:53 — `ready` once bound, then one question
            // answered NOTIMP, its ID and question echoed: an answer only it gives)
            "dnsserver" => {
                let s = match std::net::UdpSocket::bind("127.0.0.1:53") {
                    Ok(s) => s,
                    Err(e) => return println!("helper: bind={}", tried::<()>(Err(e))),
                };
                println!("helper: ready");
                s.set_read_timeout(Some(Duration::from_secs(10))).ok();
                let mut b = [0u8; 512];
                let Ok((n, from)) = s.recv_from(&mut b) else { return println!("helper: nothing asked") };
                let mut a = b[..n].to_vec();
                if a.len() >= 4 {
                    (a[2], a[3]) = (0x80 | (a[2] & 0x79), 0x84);
                }
                format!("answered={}", tried(s.send_to(&a, from)))
            }
            "ask" => helper_ask(&std::env::var("TARAE_TO").unwrap_or_default()),
            // (6d: a TCP socket made in the run's network and its number said — then held, unconnected, until
            // `$TARAE_GO` is there, 5 s at most, for the test to take it)
            "socket" => {
                // SAFETY: a plain call; the socket is this process's until it ends.
                let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
                println!("helper: fd={fd}");
                let go = std::env::var_os("TARAE_GO").unwrap_or_default();
                let by = Instant::now() + Duration::from_secs(5);
                while !Path::new(&go).exists() && Instant::now() < by {
                    std::thread::sleep(Duration::from_millis(10));
                }
                return;
            }
            "lookup_timed" => {
                let name = std::env::var("TARAE_NAME").unwrap_or_default();
                let t0 = Instant::now();
                let found = lookup_in(&name, libc::AF_INET);
                format!("lookup={found} ms={}", t0.elapsed().as_millis())
            }
            "datagram" => {
                let to = std::env::var("TARAE_TO").unwrap_or_default();
                let sent = std::net::UdpSocket::bind("0.0.0.0:0").and_then(|s| s.send_to(b"x", to.as_str()));
                format!("datagram={}", tried(sent))
            }
            "nlchanges" => helper_nl_changes(),
            "userns" => helper_userns(),
            "setns" => helper_setns(&std::env::var("TARAE_NS").unwrap_or_default()),
            "lookup4" => format!("v4={} lookup4={}", ipv4_addresses().join(","), lookup4_own_name()),
            "lookup6" => {
                let name = std::env::var("TARAE_NAME").unwrap_or_default();
                format!("v6={} lookup6={}", ipv6_addresses().join(","), lookup_in(&name, libc::AF_INET6))
            }
            other => format!("no helper {other}"),
        };
        println!("helper: {said}");
    }

    /// `userns`: a user namespace made three ways — `unshare` (in a fork: a process with threads can't), `clone` and
    /// `clone3` — each `capable` (a network namespace made inside it: what Ubuntu's restriction takes away),
    /// `capless <errno>`, or the errno it was refused with: `unshare=… clone=… clone3=…`.
    fn helper_userns() -> String {
        /// In the child each way made: a network namespace inside, its errno (0: made) on `w`, then the end.
        unsafe fn inside(w: i32) -> ! {
            // SAFETY: raw calls only — this is a copy of a process with threads.
            unsafe {
                let r = libc::unshare(libc::CLONE_NEWNET);
                let e: i32 = if r == 0 { 0 } else { *libc::__errno_location() };
                libc::write(w, (&raw const e).cast(), 4);
                libc::_exit(0)
            }
        }
        let way = |make: &dyn Fn() -> libc::c_long, w: i32, r: i32| -> String {
            let pid = make();
            if pid == 0 {
                // SAFETY: the new child's own end.
                unsafe { inside(w) }
            }
            if pid < 0 {
                return io::Error::last_os_error().raw_os_error().unwrap_or(0).to_string();
            }
            let mut e = -1i32;
            // SAFETY: a read into an int we own; the child reaped.
            unsafe {
                libc::read(r, (&raw mut e).cast(), 4);
                libc::waitpid(pid as i32, std::ptr::null_mut(), 0);
            }
            match e {
                0 => "capable".into(),
                e => format!("capless {e}"),
            }
        };
        let (r, w) = io::pipe().unwrap();
        let (r, w) = (std::os::fd::AsRawFd::as_raw_fd(&r), std::os::fd::AsRawFd::as_raw_fd(&w));
        // SAFETY: each a plain call making a child (stack 0: a copy, as a fork's) — `inside` is all it runs.
        let unshare = way(
            &|| unsafe {
                let pid = libc::fork();
                if pid == 0 {
                    if libc::unshare(libc::CLONE_NEWUSER) != 0 {
                        let e = -*libc::__errno_location();
                        libc::write(w, (&raw const e).cast(), 4);
                        libc::_exit(0);
                    }
                    inside(w)
                }
                pid as libc::c_long
            },
            w,
            r,
        );
        // (unshare's own refusal comes back negative from its child — said as the errno, as the others)
        let unshare = match unshare.strip_prefix("capless -") {
            Some(errno) => errno.to_string(),
            None => unshare,
        };
        let user = libc::CLONE_NEWUSER as u64;
        let flags = user | libc::SIGCHLD as u64;
        // SAFETY: as above.
        let clone = way(&|| unsafe { libc::syscall(libc::SYS_clone, flags, 0, 0, 0, 0) }, w, r);
        let mut args = [0u64; 11]; // struct clone_args: flags, …, exit_signal
        (args[0], args[4]) = (user, libc::SIGCHLD as u64);
        let size = std::mem::size_of_val(&args);
        // SAFETY: as above; the struct lives through the call.
        let clone3 = way(&|| unsafe { libc::syscall(libc::SYS_clone3, args.as_ptr(), size) }, w, r);
        format!("unshare={unshare} clone={clone} clone3={clone3}")
    }

    /// Q4: where tarae runs under its AppArmor profile with Ubuntu's restriction on, a run makes no user namespace —
    /// not by `unshare`, `clone` nor `clone3` — though the profile, whose label the run carries (an exec keeps it,
    /// t5), would let it. The control: the same run without the refusal makes one by each, capable — the profile is
    /// what gives it, so the refusal is what stops it. Elsewhere the test says why it skips; a machine said to hold
    /// the profile (`TARAE_EXPECT_PROFILE=1`: the VM's run under it) fails instead.
    #[test]
    fn a_run_makes_no_user_namespace_under_the_profile() {
        let test = "a_run_makes_no_user_namespace_under_the_profile";
        if !profiled(test) {
            return;
        }
        let Some(rules) = open_rules(test) else { return };
        let k = keeper("userns");
        let through = |no_userns| {
            let cmd = helper_cmd("userns", &[], None);
            let comm = crate::landlock::thread_name();
            helper_said(k.start_userns(&cmd, &comm, rules.fd(), RunNet::Plain, no_userns).unwrap())
        };
        let (refused, control) = (through(true), through(false));
        k.end();
        let (eperm, enosys) = (libc::EPERM, libc::ENOSYS);
        assert_eq!(refused, format!("unshare={eperm} clone={eperm} clone3={enosys}"), "refused, every way");
        assert_eq!(
            control, "unshare=capable clone=capable clone3=capable",
            "the profile lends it: {refused}"
        );
    }

    /// `setns`: the user namespace at `ns` (a `/proc/<pid>/ns/user`) joined — in a fork: a process with threads
    /// can't change its user namespace — and a network namespace made inside it (what a capable one allows):
    /// `setns=<ok|errno> net=<ok|errno|->`.
    fn helper_setns(ns: &str) -> String {
        let path = std::ffi::CString::new(ns).unwrap_or_default();
        let (r, w) = io::pipe().unwrap();
        let (r, w) = (std::os::fd::AsRawFd::as_raw_fd(&r), std::os::fd::AsRawFd::as_raw_fd(&w));
        // SAFETY: the child makes raw calls only (on the path and buffer made above), then ends; the parent reads
        // into an array it owns and reaps the child.
        unsafe {
            let pid = libc::fork();
            if pid == 0 {
                let errno = || *libc::__errno_location();
                let fd = libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
                let got: [i32; 2] = if fd < 0 || libc::setns(fd, libc::CLONE_NEWUSER) != 0 {
                    [errno(), -1]
                } else {
                    [0, if libc::unshare(libc::CLONE_NEWNET) == 0 { 0 } else { errno() }]
                };
                libc::write(w, got.as_ptr().cast(), 8);
                libc::_exit(0);
            }
            let mut got = [-1i32; 2];
            libc::read(r, got.as_mut_ptr().cast(), 8);
            libc::waitpid(pid, std::ptr::null_mut(), 0);
            let said = |e: i32| match e {
                0 => "ok".to_string(),
                -1 => "-".to_string(),
                e => e.to_string(),
            };
            format!("setns={} net={}", said(got[0]), said(got[1]))
        }
    }

    /// The review's finding (2026-10-08), under the profile: a user namespace is refused joined as it's refused
    /// made. The user's own tool makes a capable one and stays in it (`unshare -U sleep`, started as the user's
    /// task is: `Acts::User`); a process of the user's kind joins it (`setns` through its `/proc/<pid>/ns/user`) and
    /// makes a network namespace there — the control: the profile lends it, joining works. A run of Claude's
    /// outside the sandbox trying the same is refused `setns` itself (`EPERM`), so Q4 isn't got round through a
    /// namespace the terminal's podman made.
    #[test]
    fn claudes_run_joins_no_user_namespace_the_users_tool_made() {
        if !profiled("claudes_run_joins_no_user_namespace_the_users_tool_made") {
            return;
        }
        let mut hold = std::process::Command::new("unshare");
        hold.args(["-U", "sleep", "60"]);
        let holder = crate::spawn::setsid_as(&hold, crate::spawn::Acts::User).expect("unshare -U starts");
        let ns = format!("/proc/{}/ns/user", holder.pid);
        let ours = std::fs::read_link("/proc/self/ns/user").unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while std::fs::read_link(&ns).is_ok_and(|n| n == ours) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        let theirs = std::fs::read_link(&ns).ok();
        let joined = |acts| {
            let cmd = helper_cmd("setns", &[("TARAE_NS", ns.as_str())], None);
            let s = crate::spawn::setsid_as(&cmd, acts).expect("the joiner starts");
            let mut out = String::new();
            io::Read::read_to_string(&mut s.stdout.unwrap(), &mut out).unwrap();
            crate::spawn::wait(s.pid).unwrap();
            out.lines().find_map(|l| l.split_once("helper: ").map(|(_, s)| s.trim().to_string()))
        };
        let (users, claudes) = (joined(crate::spawn::Acts::User), joined(crate::spawn::Acts::Claude));
        // SAFETY: the group the holder leads (`setsid`), ours.
        unsafe { libc::kill(-(holder.pid as libc::pid_t), libc::SIGKILL) };
        let _ = crate::spawn::wait(holder.pid);
        assert!(theirs.is_some_and(|t| t != ours), "the holder made its own user namespace");
        assert_eq!(users.as_deref(), Some("setns=ok net=ok"), "the control: the user's kind joins, capable");
        assert_eq!(claudes, Some(format!("setns={} net=-", libc::EPERM)), "Claude's run is refused setns");
    }

    /// `tier::profiled`, for a test of this module.
    fn profiled(test: &str) -> bool {
        super::super::tier::profiled(&format!("sandbox::keeper::tests::{test}"))
    }

    /// The user's call (2026-10-08), for real under the profile: outside the sandbox, a run of Claude's (started
    /// as `task_spawn` starts it: `sandbox::spawn_as` without a policy, `Acts::Claude` — the editor's own paths:
    /// `tools::tests::claudes_commands_outside_the_sandbox_carry_the_filter`) and what a plugin starts (by
    /// `plugin_spawn_now`'s own `plugin_process`) make no user namespace, by any of the three calls. The control:
    /// the terminal's shell — the user's own, started as `terminal_run` starts it — makes a capable one: the
    /// profile lends it where the user said it should, so the filter is what keeps the others.
    #[test]
    fn outside_the_sandbox_claude_and_plugins_make_no_user_namespace_the_terminal_does() {
        if !profiled("outside_the_sandbox_claude_and_plugins_make_no_user_namespace_the_terminal_does") {
            return;
        }
        let said = |out: &str| {
            let line = out.lines().find_map(|l| l.split_once("helper: ").map(|(_, s)| s.trim().to_string()));
            line.unwrap_or_else(|| panic!("the helper said nothing:\n{out}"))
        };
        // Claude's run, outside the sandbox
        let s = crate::sandbox::spawn_as(None, &helper_cmd("userns", &[], None), crate::spawn::Acts::Claude)
            .expect("Claude's run starts");
        let mut out = String::new();
        io::Read::read_to_string(&mut s.stdout.unwrap(), &mut out).unwrap();
        crate::spawn::wait(s.pid).unwrap();
        let claudes = said(&out);
        // What a plugin starts: this binary by its name on PATH (`--installed`: /usr/bin/tarae), as a plugin's
        // `spawn` names a program, the helper's variable one it may read
        let exe = std::env::current_exe().unwrap();
        let name = exe.file_name().unwrap().to_string_lossy().into_owned();
        let args: Vec<String> = ["--exact", "sandbox::keeper::tests::helper", "--ignored", "--nocapture"]
            .iter()
            .map(|a| a.to_string())
            .chain(["--test-threads=1".into()])
            .collect();
        let env = [(HELPER.to_string(), "userns".to_string())];
        let declared = [crate::plugin::PluginPermission::Env(HELPER.into())];
        let child = crate::plugin::plugin_process(&name, &args, &env, &declared)
            .unwrap_or_else(|e| panic!("the plugin's program starts ({name} on PATH): {e}"));
        let out = child.wait_with_output().unwrap();
        let plugins = said(&String::from_utf8_lossy(&out.stdout));
        // The terminal's shell
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        let argv: Vec<String> = std::iter::once(exe.display().to_string()).chain(args).collect();
        ed.terminal_run(argv, vec![(HELPER.into(), "userns".into())]);
        let end = Instant::now() + Duration::from_secs(20);
        let screen = |ed: &crate::editor::Editor| {
            ed.session.terminal.as_ref().map(|t| t.parser.screen().contents()).unwrap_or_default()
        };
        while ed.session.terminal.as_ref().is_none_or(|t| t.exited.is_none()) && Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(100)) {
                ed.handle_event(ev);
            }
        }
        let terminals = said(&screen(&ed));
        let refused = format!("unshare={} clone={} clone3={}", libc::EPERM, libc::EPERM, libc::ENOSYS);
        assert_eq!(claudes, refused, "Claude's run outside the sandbox");
        assert_eq!(plugins, refused, "what a plugin starts");
        assert_eq!(
            terminals, "unshare=capable clone=capable clone3=capable",
            "the terminal's shell keeps them"
        );
    }

    /// `listen`: a listener of its own on `127.0.0.1:$TARAE_PORT` (0: any), its port said, then one connection
    /// served — a line read, `got <it>` written back.
    fn helper_listen() {
        let port: u16 = std::env::var("TARAE_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(0);
        let l = match std::net::TcpListener::bind(("127.0.0.1", port)) {
            Ok(l) => l,
            Err(e) => return println!("helper: listen={}", tried::<()>(Err(e))),
        };
        println!("helper: port {}", l.local_addr().map(|a| a.port()).unwrap_or(0));
        if let Ok((s, _)) = l.accept() {
            let mut line = String::new();
            let mut r = std::io::BufReader::new(&s);
            let _ = io::BufRead::read_line(&mut r, &mut line);
            let _ = io::Write::write_all(&mut &s, format!("got {line}").as_bytes());
        }
        println!("helper: served");
    }

    /// `cgroupwrites` (6g): each way a run could move itself or change a cgroup, tried — its own cgroup's
    /// `cgroup.procs`, a sibling run's (`$TARAE_SIBLING`), its keeper's scope's (`$TARAE_SCOPE`), the one above the
    /// scope's (moving out of it), a folder made in the scope, a file of its own cgroup (`cgroup.max.descendants`):
    /// `own=… sibling=… scope=… out=… mkdir=… file=…`, each `ok` or its errno.
    fn helper_cgroup_writes() -> String {
        let at = |c: &str| Path::new("/sys/fs/cgroup").join(c.trim_start_matches('/'));
        let own = std::fs::read_to_string("/proc/self/cgroup").unwrap_or_default();
        let own = own.lines().find_map(|l| l.strip_prefix("0::")).unwrap_or_default().to_string();
        let scope = std::env::var("TARAE_SCOPE").unwrap_or_default();
        let sibling = std::env::var("TARAE_SIBLING").unwrap_or_default();
        let me = std::process::id().to_string();
        let write = |p: PathBuf, v: &str| {
            tried(
                std::fs::OpenOptions::new()
                    .write(true)
                    .open(p)
                    .and_then(|mut f| io::Write::write_all(&mut f, v.as_bytes())),
            )
        };
        let out = Path::new(&scope).parent().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        format!(
            "own={} sibling={} scope={} out={} mkdir={} file={}",
            write(at(&own).join("cgroup.procs"), &me),
            write(at(&sibling).join("cgroup.procs"), &me),
            write(at(&scope).join("cgroup.procs"), &me),
            write(at(&out).join("cgroup.procs"), &me),
            tried(std::fs::create_dir(at(&scope).join("made-by-a-run"))),
            write(at(&own).join("cgroup.max.descendants"), "0"),
        )
    }

    /// `connect`: `$TARAE_TO` reached, `hello` written, its answer read: `reached <answer>`, or the errno.
    fn helper_connect(to: &str) -> String {
        let s = match std::net::TcpStream::connect(to) {
            Ok(s) => s,
            Err(e) => return tried::<()>(Err(e)),
        };
        let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        let _ = io::Write::write_all(&mut &s, b"hello\n");
        let mut line = String::new();
        let _ = io::BufRead::read_line(&mut std::io::BufReader::new(&s), &mut line);
        format!("reached {}", line.trim())
    }

    /// `origin` (6d): a plain request for `$TARAE_HOST` sent as a client sends it to its server (origin form) on a
    /// TCP socket of its own connected to `$TARAE_TO` — the run trying `SO_MARK` on it before (`setmark`: its errno,
    /// 0 had it worked) and clearing it after (`clear`) — and what it says: `peer=<its peer> reply=<the first line
    /// back> setmark=<errno> mark=<even|odd|0> clear=<errno>`, or the connect's errno.
    fn helper_origin(to: &str) -> String {
        use std::io::{BufRead, Write};
        let sa = sockaddr(to);
        let host = std::env::var("TARAE_HOST").unwrap_or_default();
        let mark = |fd: i32| {
            let (mut v, mut len) = (0u32, 4 as libc::socklen_t);
            // SAFETY: an int and its length, ours for the call.
            unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_MARK, (&raw mut v).cast(), &mut len) };
            v
        };
        let set = |fd: i32, v: u32| {
            // SAFETY: an int, ours for the call.
            let r =
                unsafe { libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_MARK, (&raw const v).cast(), 4) };
            if r == 0 { 0 } else { errno_now() }
        };
        // SAFETY: plain calls on a socket this makes; an address of its size.
        let (fd, setmark, r) = unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            let setmark = set(fd, 0x40);
            let r =
                libc::connect(fd, (&raw const sa).cast(), size_of::<libc::sockaddr_in>() as libc::socklen_t);
            (fd, setmark, if r == 0 { 0 } else { errno_now() })
        };
        // SAFETY: the socket made above, ours from here.
        let s = unsafe { std::net::TcpStream::from_raw_fd(fd) };
        if r != 0 {
            return r.to_string();
        }
        let m = match mark(fd) {
            0 => "0",
            m if m & 1 == 0 => "even",
            _ => "odd",
        };
        let clear = set(fd, 0);
        let peer = s.peer_addr().map(|a| a.to_string()).unwrap_or_default();
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let _ = (&s).write_all(format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes());
        let mut reply = String::new();
        let _ = std::io::BufReader::new(&s).read_line(&mut reply);
        format!("peer={peer} reply={} setmark={setmark} mark={m} clear={clear}", reply.trim_end())
    }

    /// `within`: `$TARAE_TO` reached within 2 s, and what it does with a connection that says nothing — `reached
    /// closed` (it closed it), `reached reset`, `reached <data>`, `reached open` (still open 2 s on) — or the errno,
    /// `timed out`.
    fn helper_within(to: &str) -> String {
        let Ok(to) = to.parse::<std::net::SocketAddr>() else { return format!("not an address: {to}") };
        let s = match std::net::TcpStream::connect_timeout(&to, Duration::from_secs(2)) {
            Ok(s) => s,
            Err(e) if e.kind() == io::ErrorKind::TimedOut => return "timed out".into(),
            // (a reset quick enough to land before the connect is seen through: reached all the same)
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => return "reached reset".into(),
            Err(e) => return tried::<()>(Err(e)),
        };
        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
        let mut b = [0u8; 64];
        match io::Read::read(&mut &s, &mut b) {
            Ok(0) => "reached closed".into(),
            Ok(n) => format!("reached {}", String::from_utf8_lossy(&b[..n]).trim()),
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => "reached reset".into(),
            Err(_) => "reached open".into(),
        }
    }

    /// `named` (6e): `name` looked up as a tool does (`getaddrinfo` — glibc's resolver, its `resolv.conf`, and
    /// `$LOCALDOMAIN`/`$RES_OPTIONS` where set), its first address connected to at port 80, and a plain request for
    /// `name` sent there: `addr=<the address> reply=<the first line back>`, or `lookup=<why>`, or the connect's errno.
    fn helper_named(name: &str) -> String {
        use std::io::{BufRead, Write};
        use std::net::ToSocketAddrs;
        let first = match (name, 80).to_socket_addrs() {
            Ok(mut a) => a.next(),
            Err(e) => return format!("lookup={e}"),
        };
        let Some(at) = first else { return "lookup=none".into() };
        let s = match std::net::TcpStream::connect_timeout(&at, Duration::from_secs(5)) {
            Ok(s) => s,
            Err(e) => return format!("addr={} connect={}", at.ip(), tried::<()>(Err(e))),
        };
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let _ = (&s).write_all(format!("GET / HTTP/1.1\r\nHost: {name}\r\n\r\n").as_bytes());
        let mut reply = String::new();
        let _ = std::io::BufReader::new(&s).read_line(&mut reply);
        format!("addr={} reply={}", at.ip(), reply.trim_end())
    }

    /// `ask` (6e): a DNS question for `$TARAE_NAME`'s `$TARAE_TYPE` record (default A) sent to `to` on a connected
    /// UDP socket — which takes an answer only from the address and port it asked — or over TCP (`$TARAE_TCP` set):
    /// `rcode=<n> answers=<n> addr=<the first A/AAAA's>`, or what failed.
    fn helper_ask(to: &str) -> String {
        let name = std::env::var("TARAE_NAME").unwrap_or_default();
        let qtype: u16 = std::env::var("TARAE_TYPE").ok().and_then(|t| t.parse().ok()).unwrap_or(1);
        let tcp = std::env::var_os("TARAE_TCP").is_some();
        ask_raw(to, &name, qtype, tcp).unwrap_or_else(|e| e)
    }

    /// `searched` (6e, E6): `$TARAE_NAME` looked up as glibc does under `ndots:5` with `$TARAE_SEARCH` its search
    /// domain (M7: every glibc client did) — the candidate `<name>.<search>` asked first of `$TARAE_TO`, the name as
    /// written only if that was NXDOMAIN — and the address taken connected to at port 80, a plain request for the name
    /// sent: `candidate=<rcode> addr=<the address> reply=<the first line back>`.
    fn helper_searched(to: &str) -> String {
        use std::io::{BufRead, Write};
        let name = std::env::var("TARAE_NAME").unwrap_or_default();
        let search = std::env::var("TARAE_SEARCH").unwrap_or_default();
        let candidate = ask_raw(to, &format!("{name}.{search}"), 1, false);
        let first = |said: &str| said.strip_prefix("rcode=0 answers=1 addr=").map(str::to_string);
        let (code, addr) = match candidate {
            Ok(said) => match first(&said) {
                Some(a) => ("0".to_string(), Some(a)),
                None => (said.split(' ').next().unwrap_or("").replace("rcode=", ""), None),
            },
            Err(e) => return e,
        };
        let addr = match addr {
            Some(a) => a,
            None => match ask_raw(to, &name, 1, false).map(|s| first(&s)) {
                Ok(Some(a)) => a,
                Ok(None) => return format!("candidate={code} name=unanswered"),
                Err(e) => return e,
            },
        };
        let Ok(ip) = addr.parse::<std::net::IpAddr>() else { return format!("not an address: {addr}") };
        let s = match std::net::TcpStream::connect_timeout(&(ip, 80).into(), Duration::from_secs(5)) {
            Ok(s) => s,
            Err(e) => return format!("candidate={code} addr={addr} connect={}", tried::<()>(Err(e))),
        };
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let _ = (&s).write_all(format!("GET / HTTP/1.1\r\nHost: {name}\r\n\r\n").as_bytes());
        let mut reply = String::new();
        let _ = std::io::BufReader::new(&s).read_line(&mut reply);
        format!("candidate={code} addr={addr} reply={}", reply.trim_end())
    }

    /// A DNS question for `name`'s `qtype` record sent to `to` (over TCP where `tcp`): `rcode=<n> answers=<n>
    /// addr=<the first A/AAAA's>` — Err: what failed, said.
    fn ask_raw(to: &str, name: &str, qtype: u16, tcp: bool) -> Result<String, String> {
        use std::io::{Read, Write};
        let mut q = vec![0x6e, 0x6e, 0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0];
        for l in name.split('.').filter(|l| !l.is_empty()) {
            q.push(l.len() as u8);
            q.extend_from_slice(l.as_bytes());
        }
        q.push(0);
        q.extend(qtype.to_be_bytes());
        q.extend([0, 1]);
        let wait = Some(Duration::from_secs(3));
        let failed = |e: io::Error| tried::<()>(Err(e));
        let a = if tcp {
            let Ok(to) = to.parse::<std::net::SocketAddr>() else {
                return Err(format!("not an address: {to}"));
            };
            let mut s = std::net::TcpStream::connect_timeout(&to, Duration::from_secs(3)).map_err(failed)?;
            s.set_read_timeout(wait).ok();
            let mut framed = (q.len() as u16).to_be_bytes().to_vec();
            framed.extend(&q);
            let mut len = [0u8; 2];
            s.write_all(&framed).and_then(|()| s.read_exact(&mut len)).map_err(failed)?;
            let mut a = vec![0u8; usize::from(u16::from_be_bytes(len))];
            s.read_exact(&mut a).map_err(failed)?;
            a
        } else {
            let any = if to.starts_with('[') { "[::]:0" } else { "0.0.0.0:0" };
            let s = std::net::UdpSocket::bind(any).and_then(|s| s.connect(to).map(|()| s)).map_err(failed)?;
            s.set_read_timeout(wait).ok();
            let mut b = [0u8; 512];
            let n = s.send(&q).and_then(|_| s.recv(&mut b)).map_err(failed)?;
            b[..n].to_vec()
        };
        if a.len() < 12 || a[..2] != q[..2] {
            return Err(format!("not its answer ({} bytes)", a.len()));
        }
        let answers = u16::from_be_bytes([a[6], a[7]]);
        // (the first record's data: past the question — echoed — its name pointer, type, class, TTL and length)
        let rdata = a.get(q.len() + 12..).unwrap_or_default();
        let addr = match rdata.len() {
            4 => std::net::Ipv4Addr::from(<[u8; 4]>::try_from(rdata).unwrap()).to_string(),
            16 => std::net::Ipv6Addr::from(<[u8; 16]>::try_from(rdata).unwrap()).to_string(),
            _ => "none".into(),
        };
        Ok(format!("rcode={} answers={answers} addr={addr}", a[3] & 0x0f))
    }

    /// `asked`: a datagram to `$TARAE_TO` on a connected UDP socket — as glibc's resolver sends its questions — and
    /// its answer waited for 2 s: `answered`, or the errno (`EAGAIN`: none came) and how long that took, `ms=<n>`.
    fn helper_asked(to: &str) -> String {
        let s = match std::net::UdpSocket::bind("0.0.0.0:0").and_then(|s| s.connect(to).map(|()| s)) {
            Ok(s) => s,
            Err(e) => return tried::<()>(Err(e)),
        };
        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
        let t0 = Instant::now();
        let mut b = [0u8; 512];
        let got = s.send(b"\x12\x34\x01\x00\x00\x01\x00\x00\x00\x00\x00\x00").and_then(|_| s.recv(&mut b));
        match got {
            Ok(_) => "answered".into(),
            Err(e) => format!("{} ms={}", tried::<()>(Err(e)), t0.elapsed().as_millis()),
        }
    }

    /// `nlchanges`: what changing the session's network takes, tried by a run on its own routing socket, each sent
    /// to the kernel's address as ip sends it — the policy rule tarae added deleted (`RTM_DELRULE`, table 100), the
    /// main table's default route deleted and added again (`RTM_DELROUTE`, `RTM_NEWROUTE`), and a netfilter socket
    /// (what nf_tables changes take): each `ok` or its errno.
    fn helper_nl_changes() -> String {
        // SAFETY: plain call; a descriptor made is ours.
        let fd = unsafe {
            libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::NETLINK_ROUTE)
        };
        if fd < 0 {
            return format!("route={}", errno_now());
        }
        // SAFETY: just made.
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        let send = |kind: u16, flags: i32, body: &[u8]| -> String {
            let mut m = Vec::new();
            m.extend((16 + body.len() as u32).to_ne_bytes());
            m.extend(kind.to_ne_bytes());
            m.extend(((libc::NLM_F_REQUEST | libc::NLM_F_ACK | flags) as u16).to_ne_bytes());
            m.extend(1u32.to_ne_bytes());
            m.extend(0u32.to_ne_bytes());
            m.extend(body);
            // SAFETY: all zeros is the kernel's address; a send of our buffer on our socket.
            unsafe {
                let mut to: libc::sockaddr_nl = std::mem::zeroed();
                to.nl_family = libc::AF_NETLINK as libc::sa_family_t;
                let n = libc::sendto(
                    fd.as_raw_fd(),
                    m.as_ptr().cast(),
                    m.len(),
                    0,
                    (&raw const to).cast(),
                    size_of::<libc::sockaddr_nl>() as libc::socklen_t,
                );
                if n < 0 { errno_now().to_string() } else { "ok".into() }
            }
        };
        // `struct fib_rule_hdr`: AF_INET, table 100, FR_ACT_TO_TBL; `struct rtmsg`: AF_INET, main, unicast
        let rule = [libc::AF_INET as u8, 0, 0, 0, 100, 0, 0, 1, 0, 0, 0, 0];
        let route = [libc::AF_INET as u8, 0, 0, 0, 254, 0, 0, 1, 0, 0, 0, 0];
        let delrule = send(libc::RTM_DELRULE, 0, &rule);
        let delroute = send(libc::RTM_DELROUTE, 0, &route);
        let newroute = send(libc::RTM_NEWROUTE, libc::NLM_F_CREATE, &route);
        // SAFETY: plain call; a descriptor it makes is closed at once.
        let nf = unsafe {
            libc::socket(libc::AF_NETLINK, libc::SOCK_RAW | libc::SOCK_CLOEXEC, libc::NETLINK_NETFILTER)
        };
        let netfilter = match nf {
            n if n < 0 => errno_now().to_string(),
            n => {
                // SAFETY: ours, just made.
                unsafe { libc::close(n) };
                "ok".into()
            }
        };
        format!("delrule={delrule} delroute={delroute} newroute={newroute} netfilter={netfilter}")
    }

    /// This process's netns's IPv4 addresses (`getifaddrs`), as `a.b.c.d`.
    fn ipv4_addresses() -> Vec<String> {
        let mut out = Vec::new();
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: the list getifaddrs makes, walked while it lives, freed after.
        unsafe {
            if libc::getifaddrs(&mut list) != 0 {
                return out;
            }
            let mut at = list;
            while !at.is_null() {
                let a = (*at).ifa_addr;
                if !a.is_null() && i32::from((*a).sa_family) == libc::AF_INET {
                    let sin = &*(a as *const libc::sockaddr_in);
                    out.push(std::net::Ipv4Addr::from(sin.sin_addr.s_addr.to_ne_bytes()).to_string());
                }
                at = (*at).ifa_next;
            }
            libc::freeifaddrs(list);
        }
        out
    }

    /// This process's netns's IPv6 addresses (`getifaddrs`), sorted.
    fn ipv6_addresses() -> Vec<String> {
        let mut out = Vec::new();
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        // SAFETY: the list getifaddrs makes, walked while it lives, freed after.
        unsafe {
            if libc::getifaddrs(&mut list) != 0 {
                return out;
            }
            let mut at = list;
            while !at.is_null() {
                let a = (*at).ifa_addr;
                if !a.is_null() && i32::from((*a).sa_family) == libc::AF_INET6 {
                    let sin6 = &*(a as *const libc::sockaddr_in6);
                    out.push(std::net::Ipv6Addr::from(sin6.sin6_addr.s6_addr).to_string());
                }
                at = (*at).ifa_next;
            }
            libc::freeifaddrs(list);
        }
        out.sort();
        out
    }

    /// This machine's own name looked up IPv4-only with `AI_ADDRCONFIG` (as `getent ahostsv4` asks — answered from
    /// `/etc/hosts`, no network): how many addresses, or `getaddrinfo <its error>`.
    fn lookup4_own_name() -> String {
        let mut name = [0u8; 256];
        // SAFETY: a buffer of its size for the call; NUL-terminated by it (one byte kept for it).
        if unsafe { libc::gethostname(name.as_mut_ptr().cast(), name.len() - 1) } != 0 {
            return "no hostname".into();
        }
        let end = name.iter().position(|&b| b == 0).unwrap_or(0);
        lookup_in(&String::from_utf8_lossy(&name[..end]), libc::AF_INET)
    }

    /// `name` looked up in `family` only, with `AI_ADDRCONFIG`: how many addresses, or `getaddrinfo <its error>`.
    fn lookup_in(name: &str, family: i32) -> String {
        let Ok(name) = std::ffi::CString::new(name) else { return "a bad name".into() };
        // SAFETY: an all-integer/pointer struct, zeroed then set.
        let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
        (hints.ai_family, hints.ai_flags, hints.ai_socktype) =
            (family, libc::AI_ADDRCONFIG, libc::SOCK_STREAM);
        let mut res: *mut libc::addrinfo = std::ptr::null_mut();
        // SAFETY: a NUL-terminated name, the hints and an out pointer, live for the call; the list freed after.
        let r = unsafe { libc::getaddrinfo(name.as_ptr().cast(), std::ptr::null(), &hints, &mut res) };
        if r != 0 {
            return format!("getaddrinfo {r}");
        }
        let (mut n, mut at) = (0, res);
        while !at.is_null() {
            n += 1;
            // SAFETY: a node of the list getaddrinfo made.
            at = unsafe { (*at).ai_next };
        }
        // SAFETY: the list getaddrinfo made.
        unsafe { libc::freeaddrinfo(res) };
        n.to_string()
    }

    /// `$TARAE_TO` (`a.b.c.d:port`) as a `sockaddr_in`.
    fn sockaddr(to: &str) -> libc::sockaddr_in {
        let a: std::net::SocketAddrV4 = to.parse().expect("an IPv4 address and port");
        // SAFETY: an all-integer struct, zeroed then set.
        let mut sa: libc::sockaddr_in = unsafe { std::mem::zeroed() };
        sa.sin_family = libc::AF_INET as libc::sa_family_t;
        sa.sin_port = a.port().to_be();
        sa.sin_addr.s_addr = u32::from_ne_bytes(a.ip().octets());
        sa
    }

    fn errno_now() -> i32 {
        io::Error::last_os_error().raw_os_error().unwrap_or(0)
    }

    /// `swap`: a non-blocking, close-on-exec TCP socket with `TCP_NODELAY`, `SO_KEEPALIVE` and a 7 s
    /// `SO_RCVTIMEO` set, connected to `$TARAE_TO`: what `connect` said, then — after its connection is up — what
    /// the socket at that number holds (`connect=<errno> nonblock= cloexec= nodelay= keepalive= rcvtimeo=
    /// error= dev=<the device it's bound to>`).
    fn helper_swap(to: &str) -> String {
        let sa = sockaddr(to);
        // SAFETY: plain calls on a socket this makes; buffers of their sizes, live for each call.
        unsafe {
            let fd =
                libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0);
            let one = 1i32;
            let int = std::mem::size_of::<i32>() as libc::socklen_t;
            libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, (&raw const one).cast(), int);
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_KEEPALIVE, (&raw const one).cast(), int);
            let tv = libc::timeval { tv_sec: 7, tv_usec: 0 };
            let tv_len = std::mem::size_of::<libc::timeval>() as libc::socklen_t;
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, (&raw const tv).cast(), tv_len);
            let sa_len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            let r = libc::connect(fd, (&raw const sa).cast(), sa_len);
            let said = if r == 0 { 0 } else { errno_now() };
            let mut pfd = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
            libc::poll(&mut pfd, 1, 5000);
            let get = |level, name| {
                let (mut v, mut len) = (0i32, int);
                libc::getsockopt(fd, level, name, (&raw mut v).cast(), &mut len);
                v
            };
            let mut tv = libc::timeval { tv_sec: 0, tv_usec: 0 };
            let mut len = tv_len;
            libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, (&raw mut tv).cast(), &mut len);
            let mut dev = [0u8; libc::IFNAMSIZ];
            let mut dev_len = dev.len() as libc::socklen_t;
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_BINDTODEVICE,
                dev.as_mut_ptr().cast(),
                &mut dev_len,
            );
            let dev = String::from_utf8_lossy(&dev[..dev_len as usize]).trim_end_matches('\0').to_string();
            format!(
                "connect={said} nonblock={} cloexec={} nodelay={} keepalive={} rcvtimeo={} error={} dev={dev}",
                (libc::fcntl(fd, libc::F_GETFL) & libc::O_NONBLOCK != 0) as u8,
                (libc::fcntl(fd, libc::F_GETFD) & libc::FD_CLOEXEC != 0) as u8,
                get(libc::IPPROTO_TCP, libc::TCP_NODELAY),
                get(libc::SOL_SOCKET, libc::SO_KEEPALIVE),
                tv.tv_sec,
                get(libc::SOL_SOCKET, libc::SO_ERROR),
            )
        }
    }

    /// `proxied`: a non-blocking, close-on-exec TCP socket with `TCP_NODELAY` set, connected to `$TARAE_TO` (the
    /// proxy's address), then — once it's up — what the socket at that number is, and a plain request for
    /// `http://$TARAE_HOST/` sent through it, its answer's first line read (`connect=<errno> family= stream=
    /// nodelay= nonblock= peer=<its address's ip> reply=<line>`).
    fn helper_proxied(to: &str) -> String {
        use std::io::{BufRead, Write};
        let sa = sockaddr(to);
        let host = std::env::var("TARAE_HOST").unwrap_or_default();
        // SAFETY: plain calls on a socket this makes; buffers of their sizes, live for each call.
        let (said, facts, fd) = unsafe {
            let fd =
                libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0);
            let one = 1i32;
            let int = std::mem::size_of::<i32>() as libc::socklen_t;
            libc::setsockopt(fd, libc::IPPROTO_TCP, libc::TCP_NODELAY, (&raw const one).cast(), int);
            let sa_len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            let r = libc::connect(fd, (&raw const sa).cast(), sa_len);
            let said = if r == 0 { 0 } else { errno_now() };
            let mut pfd = libc::pollfd { fd, events: libc::POLLOUT, revents: 0 };
            libc::poll(&mut pfd, 1, 5000);
            let get = |level, name| {
                let (mut v, mut len) = (0i32, int);
                libc::getsockopt(fd, level, name, (&raw mut v).cast(), &mut len);
                v
            };
            let mut peer: libc::sockaddr_in = std::mem::zeroed();
            let mut len = sa_len;
            libc::getpeername(fd, (&raw mut peer).cast(), &mut len);
            let facts = format!(
                "family={} stream={} nodelay={} nonblock={} peer={}",
                get(libc::SOL_SOCKET, libc::SO_DOMAIN),
                (get(libc::SOL_SOCKET, libc::SO_TYPE) == libc::SOCK_STREAM) as u8,
                get(libc::IPPROTO_TCP, libc::TCP_NODELAY),
                (libc::fcntl(fd, libc::F_GETFL) & libc::O_NONBLOCK != 0) as u8,
                std::net::Ipv4Addr::from(u32::from_be(peer.sin_addr.s_addr)),
            );
            libc::fcntl(fd, libc::F_SETFL, libc::fcntl(fd, libc::F_GETFL) & !libc::O_NONBLOCK);
            (said, facts, fd)
        };
        // SAFETY: the socket made above, ours from here.
        let s = unsafe { std::net::TcpStream::from_raw_fd(fd) };
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let _ = (&s).write_all(format!("GET http://{host}/ HTTP/1.1\r\n\r\n").as_bytes());
        let mut reply = String::new();
        let _ = std::io::BufReader::new(&s).read_line(&mut reply);
        format!("connect={said} {facts} reply={}", reply.trim_end())
    }

    /// `bound`: a TCP socket bound to `0.0.0.0:$TARAE_PORT` first (0: the kernel's pick), then connected to
    /// `$TARAE_TO`: `bound=<errno>`.
    fn helper_bound(to: &str) -> String {
        let sa = sockaddr(to);
        let port = std::env::var("TARAE_PORT").unwrap_or_else(|_| "0".into());
        let any = sockaddr(&format!("0.0.0.0:{port}"));
        let sa_len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        // SAFETY: plain calls on a socket this makes, with addresses of their sizes.
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if libc::bind(fd, (&raw const any).cast(), sa_len) != 0 {
                return format!("bind={}", errno_now());
            }
            let r = libc::connect(fd, (&raw const sa).cast(), sa_len);
            format!("bound={}", if r == 0 { 0 } else { errno_now() })
        }
    }

    /// `swaplisten`: a TCP socket connected to `$TARAE_TO` (a closed port: refused — the socket unbound again;
    /// with `$TARAE_UNSPEC`, the connection undone by `connect(AF_UNSPEC)`), then listened on: `connect=<errno>
    /// [unspec=<errno>] listen=<errno>` (0: done).
    fn helper_swap_listen(to: &str) -> String {
        let sa = sockaddr(to);
        let sa_len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        let ok = |r: i32| if r == 0 { 0 } else { errno_now() };
        // SAFETY: plain calls on a socket this makes, with addresses of their sizes.
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            let connect = ok(libc::connect(fd, (&raw const sa).cast(), sa_len));
            let unspec = match std::env::var_os("TARAE_UNSPEC") {
                Some(_) => {
                    let mut none: libc::sockaddr_in = std::mem::zeroed();
                    none.sin_family = libc::AF_UNSPEC as libc::sa_family_t;
                    format!(" unspec={}", ok(libc::connect(fd, (&raw const none).cast(), sa_len)))
                }
                None => String::new(),
            };
            let listen = ok(libc::listen(fd, 1));
            format!("connect={connect}{unspec} listen={listen}")
        }
    }

    /// `racelo`: one TCP socket, a thread connecting it to `$TARAE_TO` (beyond: swapped, the supervisor pausing
    /// once tarae's socket is in place — `Policy::swap_pause`) while this one, 200 ms in, connects the same number
    /// to `$TARAE_LO` (this machine's loopback, its port not opened): `lo=<errno> out=<errno>` (0: connected).
    fn helper_race_lo(to: &str) -> String {
        let (out_at, lo_at) = (sockaddr(to), sockaddr(&std::env::var("TARAE_LO").unwrap_or_default()));
        let len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        let ok = |r: i32| if r == 0 { 0 } else { errno_now() };
        // SAFETY: a socket this makes, connected from two threads with addresses of their sizes.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        // SAFETY: as above.
        let out =
            std::thread::spawn(move || ok(unsafe { libc::connect(fd, (&raw const out_at).cast(), len) }));
        std::thread::sleep(std::time::Duration::from_millis(200));
        // SAFETY: as above.
        let lo = ok(unsafe { libc::connect(fd, (&raw const lo_at).cast(), len) });
        format!("lo={lo} out={}", out.join().unwrap_or(-1))
    }

    /// `foreign`: three sockets taken over the unix socket `$TARAE_SOCK` (made outside the session: two TCP, one
    /// unix stream), the first connected to `$TARAE_LO`, the second listened on, the third connected to the
    /// abstract name `$TARAE_NAME`: `connect=<errno> listen=<errno> abstract=<errno>` (0: done).
    fn helper_foreign() -> String {
        let var = |n: &str| std::env::var(n).unwrap_or_default();
        let s = match std::os::unix::net::UnixStream::connect(var("TARAE_SOCK")) {
            Ok(s) => s,
            Err(e) => return format!("pass={}", tried::<()>(Err(e))),
        };
        let fds = received_fds(&s);
        if fds.len() != 3 {
            return format!("fds={}", fds.len());
        }
        let lo = sockaddr(&var("TARAE_LO"));
        let name = var("TARAE_NAME");
        let ok = |r: i32| if r == 0 { 0 } else { errno_now() };
        // SAFETY: plain calls on the descriptors taken, with addresses of their sizes (an all-integer
        // `sockaddr_un`, zeroed then set: a NUL, then the name — abstract).
        unsafe {
            let len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            let connect = ok(libc::connect(fds[0].as_raw_fd(), (&raw const lo).cast(), len));
            let listen = ok(libc::listen(fds[1].as_raw_fd(), 1));
            let mut un: libc::sockaddr_un = std::mem::zeroed();
            un.sun_family = libc::AF_UNIX as libc::sa_family_t;
            for (i, b) in name.bytes().enumerate() {
                un.sun_path[1 + i] = b as libc::c_char;
            }
            let un_len = (std::mem::size_of::<libc::sa_family_t>() + 1 + name.len()) as libc::socklen_t;
            let abs = ok(libc::connect(fds[2].as_raw_fd(), (&raw const un).cast(), un_len));
            format!("connect={connect} listen={listen} abstract={abs}")
        }
    }

    /// `protocols`: an IPv4 socket of each (kind, protocol) — TCP's and UDP's by 0, SCTP, MPTCP, UDP-Lite, ICMP's
    /// datagram one: each `0` (made) or its errno, space-separated.
    fn helper_protocols() -> String {
        let (stream, dgram) = (libc::SOCK_STREAM, libc::SOCK_DGRAM);
        let each = [
            (stream, 0),
            (dgram, 0),
            (stream, libc::IPPROTO_SCTP),
            (stream, libc::IPPROTO_MPTCP),
            (dgram, libc::IPPROTO_UDPLITE),
            (dgram, libc::IPPROTO_ICMP),
        ];
        let said: Vec<String> = each
            .iter()
            .map(|&(kind, protocol)| {
                // SAFETY: plain call; a descriptor it makes is closed at once.
                let fd = unsafe { libc::socket(libc::AF_INET, kind | libc::SOCK_CLOEXEC, protocol) };
                if fd < 0 {
                    return errno_now().to_string();
                }
                // SAFETY: ours, just made.
                unsafe { libc::close(fd) };
                "0".into()
            })
            .collect();
        said.join(" ")
    }

    /// `udp`: a datagram sent to `$TARAE_TO` from a socket of its own it never bound (as a resolver does), an
    /// answer waited for 3 s: `answer=<it>`, or the errno.
    fn helper_udp(to: &str) -> String {
        let sa = sockaddr(to);
        let sa_len = std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
        // SAFETY: plain calls on a socket this makes, with buffers and addresses of their sizes.
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
            let q = b"query";
            if libc::sendto(fd, q.as_ptr().cast(), q.len(), 0, (&raw const sa).cast(), sa_len) < 0 {
                return format!("send={}", errno_now());
            }
            let tv = libc::timeval { tv_sec: 3, tv_usec: 0 };
            let tv_len = std::mem::size_of::<libc::timeval>() as libc::socklen_t;
            libc::setsockopt(fd, libc::SOL_SOCKET, libc::SO_RCVTIMEO, (&raw const tv).cast(), tv_len);
            let mut buf = [0u8; 64];
            let n = libc::recv(fd, buf.as_mut_ptr().cast(), buf.len(), 0);
            if n < 0 {
                return format!("recv={}", errno_now());
            }
            format!("answer={}", String::from_utf8_lossy(&buf[..n as usize]))
        }
    }

    /// A pty opened (`/dev/ptmx`): its master, its slave, the slave's path.
    fn pty_opened() -> io::Result<(OwnedFd, OwnedFd, String)> {
        // SAFETY: plain calls on descriptors this makes and owns; a name buffer of its size.
        unsafe {
            let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
            if m < 0 {
                return Err(io::Error::last_os_error());
            }
            let m = OwnedFd::from_raw_fd(m);
            let mut name = [0 as libc::c_char; 64];
            if libc::grantpt(m.as_raw_fd()) != 0
                || libc::unlockpt(m.as_raw_fd()) != 0
                || libc::ptsname_r(m.as_raw_fd(), name.as_mut_ptr(), name.len()) != 0
            {
                return Err(io::Error::last_os_error());
            }
            let s = libc::open(name.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
            if s < 0 {
                return Err(io::Error::last_os_error());
            }
            let path = std::ffi::CStr::from_ptr(name.as_ptr()).to_string_lossy().into_owned();
            Ok((m, OwnedFd::from_raw_fd(s), path))
        }
    }

    /// D5: inside the session's user namespace a run is the user tarae is — its uid and gid tarae's own, each
    /// mapped to itself (not to root), `setgroups` denied. As uid 1000 (`ids_scene`).
    #[test]
    fn a_run_keeps_the_users_own_ids() {
        if !namespaced("a_run_keeps_the_users_own_ids") {
            return;
        }
        let said = scene("ids_scene", &[]);
        let (me, run) = said.split_once(" run=").expect("its two halves");
        let id = me.strip_prefix("me=").unwrap().split_once('/').unwrap();
        let (u, g) = (id.0, id.1);
        assert_ne!(u, "65534", "an id a map can be told from no map with");
        assert_eq!(run, format!("{u}/{g} {u} {u} 1 {g} {g} 1 deny"), "{said}");
    }

    /// `a_run_keeps_the_users_own_ids`'s scene, as uid and gid 1000: `scene: me=<uid>/<gid> run=<its id -u>/<id
    /// -g> <uid_map> <gid_map> <setgroups>`.
    #[test]
    #[ignore = "run by a_run_keeps_the_users_own_ids in a process of its own"]
    fn ids_scene() {
        if std::env::var_os(SCENE).is_none() {
            return;
        }
        ordinary_user_as(1000);
        let Some(rules) = open_rules("ids_scene") else { return };
        let k = keeper("ids");
        let script = "echo $(id -u)/$(id -g) $(cat /proc/self/uid_map) $(cat /proc/self/gid_map) \
                      $(cat /proc/self/setgroups)";
        let (out, status) = sh_through(&k, &rules, script);
        assert!(status.success(), "{out}");
        // SAFETY: plain calls with no arguments.
        let (u, g) = unsafe { (libc::getuid(), libc::getgid()) };
        println!("scene: me={u}/{g} run={}", out.trim());
        k.end();
    }

    /// D12: where a fresh `/proc` can't be mounted — here a `tmpfs` over `/proc/sys`, made in a user and mount
    /// namespace of the scene's own, so locked in the keeper's copy as Docker's masks are (`masked_proc_scene`)
    /// — the probe finds no PID namespace (never the host's `/proc` under one), saying why, and a run's `NSpid`
    /// holds one number.
    #[test]
    fn no_pid_namespace_without_its_own_proc() {
        use std::os::unix::process::CommandExt;
        if !namespaced("no_pid_namespace_without_its_own_proc") {
            return;
        }
        let mut cmd = scene_cmd("masked_proc_scene", &[]);
        // SAFETY: plain calls with no arguments.
        let (u, g) = unsafe { (libc::geteuid(), libc::getegid()) };
        let maps: Vec<(&'static std::ffi::CStr, Vec<u8>)> = vec![
            (c"/proc/self/setgroups", b"deny".to_vec()),
            (c"/proc/self/uid_map", format!("{u} {u} 1").into_bytes()),
            (c"/proc/self/gid_map", format!("{g} {g} 1").into_bytes()),
        ];
        // SAFETY: in the forked child before its exec: raw calls only, on strings made before the fork.
        unsafe {
            cmd.pre_exec(move || {
                let null = std::ptr::null::<libc::c_char>();
                if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                    return Err(io::Error::last_os_error());
                }
                for (path, map) in &maps {
                    let fd = libc::open(path.as_ptr(), libc::O_WRONLY);
                    if fd < 0 || libc::write(fd, map.as_ptr().cast(), map.len()) != map.len() as isize {
                        return Err(io::Error::last_os_error());
                    }
                    libc::close(fd);
                }
                if libc::mount(null, c"/".as_ptr(), null, libc::MS_REC | libc::MS_PRIVATE, std::ptr::null())
                    != 0
                    || libc::mount(
                        c"tmpfs".as_ptr(),
                        c"/proc/sys".as_ptr(),
                        c"tmpfs".as_ptr(),
                        0,
                        std::ptr::null(),
                    ) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let said = said_by("masked_proc_scene", cmd);
        assert!(said.contains("(user net ipc mnt) nspid=1 — "), "{said}");
        assert!(said.contains("a fresh /proc can't be mounted here"), "{said}");
    }

    /// `no_pid_namespace_without_its_own_proc`'s scene, in the user and mount namespace its test made:
    /// `scene: <tier> (<namespaces>) nspid=<how many numbers a run's NSpid holds> — <why>`.
    #[test]
    #[ignore = "run by no_pid_namespace_without_its_own_proc in a process of its own"]
    fn masked_proc_scene() {
        if std::env::var_os(SCENE).is_none() {
            return;
        }
        let p = super::super::tier::get();
        let Some(rules) = open_rules("masked_proc_scene") else { return };
        let k = keeper("masked");
        let (out, _) = sh_through(&k, &rules, "grep NSpid /proc/self/status");
        let n = out.split_whitespace().count().saturating_sub(1);
        let why = p.short.clone().unwrap_or_default();
        println!("scene: {} ({}) nspid={n} — {why}", p.tier().said(), p.ns.said());
        k.end();
    }

    /// A run sees only its session: in its `/proc` the keeper (1) and its own, in a numbering of their own —
    /// not this process; in `/proc/net/dev` only `lo` (a policy that opens nothing: the session's own
    /// network); in `/proc/sysvipc/shm` not the segment this process made (the session's own IPC).
    #[test]
    fn a_run_sees_only_its_sessions_processes() {
        if !namespaced("a_run_sees_only_its_sessions_processes") {
            return;
        }
        let Some(rules) = open_rules("a_run_sees_only_its_sessions_processes") else { return };
        let k = keeper("sees");
        // SAFETY: a plain call; the segment is removed below.
        let shm = unsafe { libc::shmget(libc::IPC_PRIVATE, 4096, libc::IPC_CREAT | 0o600) };
        assert!(shm >= 0, "{}", io::Error::last_os_error());
        let listed =
            |text: &str| text.lines().skip(1).any(|l| l.split_whitespace().nth(1) == Some(&*shm.to_string()));
        let ours = std::fs::read_to_string("/proc/sysvipc/shm").unwrap();
        let script = r#"ls /proc | grep -E '^[0-9]+$' | tr '\n' ' '; echo
            tail -n +3 /proc/net/dev | cut -d: -f1 | tr -d ' ' | tr '\n' ' '; echo
            cat /proc/sysvipc/shm"#;
        let (out, status) = sh_through(&k, &rules, script);
        // SAFETY: removes the segment made above.
        unsafe { libc::shmctl(shm, libc::IPC_RMID, std::ptr::null_mut()) };
        assert!(status.success(), "{out}");
        assert!(listed(&ours), "this process sees its own segment: {ours}");
        let mut lines = out.lines();
        let pids: Vec<u32> = lines.next().unwrap().split_whitespace().map(|p| p.parse().unwrap()).collect();
        let devs = lines.next().unwrap().trim().to_string();
        let ipc = lines.collect::<Vec<_>>().join("\n");
        if k.ns.pid {
            assert!(pids.contains(&1), "the keeper, PID 1: {pids:?}");
            assert!(!pids.contains(&std::process::id()), "not this process: {pids:?}");
            assert!(pids.iter().all(|p| *p < 64), "a numbering of the session's own: {pids:?}");
        }
        if k.ns.net {
            assert_eq!(devs, "lo", "only its loopback");
        }
        assert!(!listed(&ipc), "not this process's segment: {ipc}");
        k.end();
    }

    /// F2: no process outside its session gives a run its environment. With the PID namespace (A+) the run
    /// can't even name one: nothing it sees carries the stand-in's markers. Without it (A: Docker's masked
    /// `/proc`) it sees the stand-in and reads its command line, never its environment (Landlock keeps a
    /// ptrace-mode read in the domain — `landlock::tests::a_confined_run_reads_no_environment_outside_it`).
    /// The stand-in is the test's own, with a harmless marker in its arguments and one in its environment;
    /// the run looks through every process it sees, and must find its own markers (arguments and
    /// environment) — else a run that reads nothing would pass. Each result said.
    #[test]
    fn a_run_reads_no_details_outside_its_session() {
        if !namespaced("a_run_reads_no_details_outside_its_session") {
            return;
        }
        let Some(rules) = open_rules("a_run_reads_no_details_outside_its_session") else { return };
        let mut target = crate::landlock::stand_in("session-arg-marker", "session-env-marker");
        let k = keeper("details");
        // (the markers split in the script: its own command line mustn't carry them; the shell runs the loop
        // again as itself with the control's markers in its environment and arguments, both in place
        // before the loop reads — no child that may not have started yet)
        let script = r#"s='n=0 a=0 e=0 oa=0 oe=0
            for d in /proc/[0-9]*; do
                n=$((n+1))
                v=$(tr "\0" " " < "$d/cmdline" 2>/dev/null)
                case "$v" in *"session-arg""-marker"*) a=$((a+1)) ;; esac
                case "$v" in *"run-arg""-marker"*) oa=$((oa+1)) ;; esac
                v=$(tr "\0" " " < "$d/environ" 2>/dev/null)
                case "$v" in *"session-env""-marker"*) e=$((e+1)) ;; esac
                case "$v" in *"run-env""-marker"*) oe=$((oe+1)) ;; esac
            done
            echo "$n $a $e $oa $oe"'
            export TARAE_F2_RUN="run-env""-marker"
            exec sh -c "$s" sh "run-arg""-marker""#;
        let (out, status) = sh_through(&k, &rules, script);
        target.kill().ok();
        target.wait().ok();
        let pid_ns = k.ns.pid;
        k.end();
        assert!(status.success(), "{out}");
        let n: Vec<usize> = out.split_whitespace().map(|w| w.parse().unwrap()).collect();
        let (seen, args, env, own_args, own_env) = (n[0], n[1], n[2], n[3], n[4]);
        let tier = if pid_ns { "A+ (its own PID namespace)" } else { "A (no PID namespace)" };
        let line = format!(
            "tier {tier}: the run sees {seen} processes — its own markers read in {own_args} (arguments) and \
             {own_env} (environment), the stand-in's arguments in {args}, its environment in {env}\n"
        );
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
        // The control: the loop finds the run's own markers, so a 0 for the stand-in's is a refusal or a
        // process out of sight, not a loop that reads nothing (in A+ both of the stand-in's are 0).
        assert!(own_args >= 1 && own_env >= 1, "the run doesn't find its own markers: {line}");
        assert_eq!(env, 0, "an environment read outside the session: {line}");
        assert_eq!(args, usize::from(!pid_ns), "{line}");
    }

    /// D10 under Landlock: the session's own `/dev/pts` and `/dev/shm` sit over this system's, where a rule
    /// isn't reached through them (measured: `target/sb/step3-notes.md`), so a run's rules grant the devices on
    /// the keeper's own places too (the handles it gave with `Ready` — `linux::session_devices`). Under a
    /// project's real rules (`linux::confine`) a pty, a file made and removed in `/dev/shm` and `/proc/self`
    /// read all work — and a protected file is still refused: denied, not hidden.
    #[test]
    fn the_session_mounts_keep_landlocks_rules() {
        if !namespaced("the_session_mounts_keep_landlocks_rules")
            || open_rules("the_session_mounts_keep_landlocks_rules").is_none()
        {
            return;
        }
        let k = keeper("mounts");
        let plain = Err("not served".into());
        let (_p, c, root) = project_served("mounts", Vec::new(), Some(&k), &["Cargo.toml"], plain);
        let granted: Vec<String> = c.granted().iter().map(|(p, _)| p.display().to_string()).collect();
        for own in ["session:/dev/pts", "session:/dev/pts/ptmx", "session:/dev/shm"] {
            assert!(granted.iter().any(|g| g == own), "{own} granted: {granted:?}");
        }
        let protected = root.join("Cargo.toml");
        let env = [("TARAE_PROTECTED", protected.to_str().unwrap())];
        let said = helper_through(&k, c.rules_fd(), c.net, "mounts", &env, Some(&root));
        assert_eq!(said, format!("pty=ok shm=ok proc=ok protected={}", libc::EACCES));
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// A project in `/dev/shm` gets no `/dev/shm` of its own for its session (a private one would hide it from
    /// its runs): its keeper is born without that mount and says so; a run there writes in the project, and
    /// this process sees what it wrote.
    #[test]
    fn a_project_in_dev_shm_shares_dev_shm() {
        if !namespaced("a_project_in_dev_shm_shares_dev_shm") {
            return;
        }
        let Ok(abi) = crate::landlock::abi() else { return };
        let root = std::path::PathBuf::from(format!("/dev/shm/tarae-keeper-shm-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let env = super::super::Env {
            root: root.clone(),
            tmp: Some(super::super::profile_path(&std::env::temp_dir())),
            run_env: super::super::run_env_of(&super::super::Here::now(), &crate::llm::LlmConfig::default()),
            ..Default::default()
        };
        let p = super::super::Policy::for_project(&env);
        let wanted = super::super::tier::for_policy(&p);
        assert!(super::super::tier::in_shm(&p) && !wanted.shm && wanted.user, "{wanted:?}");
        let k = keeper_in("shm", wanted);
        assert!(k.said().contains("/dev/shm shared"), "{}", k.said());
        let plain = Err("not served".into());
        let c =
            super::super::linux::confine(&p, abi, Err("not watched".into()), false, Some(&k), plain).unwrap();
        let granted: Vec<String> = c.granted().iter().map(|(p, _)| p.display().to_string()).collect();
        assert!(
            !granted.iter().any(|g| g == "session:/dev/shm"),
            "this system's, not granted whole: {granted:?}"
        );
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "printf made > made"]).current_dir(&root);
        let s = k.start(&cmd, &crate::landlock::thread_name(), c.rules_fd(), c.net).unwrap();
        let status = crate::spawn::wait(s.spawned.pid).unwrap();
        assert!(status.success() && std::fs::read_to_string(root.join("made")).is_ok_and(|m| m == "made"));
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// D10: a run's pty comes from the session's own `devpts` — another instance than this process's (its device
    /// differs), which lists only `ptmx` and the run's own, not the two this process holds.
    #[test]
    fn a_pty_comes_from_the_sessions_devpts() {
        use std::os::unix::fs::MetadataExt;
        if !namespaced("a_pty_comes_from_the_sessions_devpts") {
            return;
        }
        let Some(rules) = open_rules("a_pty_comes_from_the_sessions_devpts") else { return };
        let _mine = (pty_opened().expect("a pty of this process's"), pty_opened().expect("and another"));
        let ours = std::fs::metadata("/dev/pts").unwrap().dev();
        let ours_listed = std::fs::read_dir("/dev/pts").unwrap().count();
        assert!(ours_listed >= 3, "ptmx and this process's two");
        let k = keeper("pty");
        let said = helper_through(&k, rules.fd(), RunNet::Plain, "pty", &[], None);
        let (dev, listed) = said.strip_prefix("dev=").and_then(|s| s.split_once(" listed=")).expect(&said);
        assert_ne!(dev.parse::<u64>().unwrap(), ours, "another devpts: {said}");
        assert_eq!(listed.split(',').count(), 2, "ptmx and its own: {said}");
        k.end();
    }

    /// A run can't end or stop its keeper by a signal: in its PID namespace the keeper is PID 1, which takes no
    /// signal it has no handler for; without one, from Landlock ABI 6 the signal scope keeps a run off it. TERM,
    /// INT, HUP, KILL and STOP to its parent (the keeper) — and the keeper still runs the next.
    #[test]
    fn a_command_cant_signal_the_keeper() {
        let Some(rules) = open_rules("a_command_cant_signal_the_keeper") else { return };
        let k = keeper("signal");
        let abi = crate::landlock::abi().unwrap_or(0);
        if !k.ns.pid && abi < 6 {
            let line = format!(
                "{}sandbox::keeper::tests::a_command_cant_signal_the_keeper — no PID namespace here and Landlock ABI \
                 {abi}: a run can signal it; it fails closed (a_killed_keeper_fails_closed_and_is_remade)\n",
                crate::test_results::SELF_SKIP
            );
            let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
            k.end();
            return;
        }
        let script = "for s in TERM INT HUP KILL STOP; do kill -s $s $PPID 2>&1; done; echo sent";
        let (out, _) = sh_through(&k, &rules, script);
        assert!(out.trim_end().ends_with("sent"), "{out}");
        std::thread::sleep(Duration::from_millis(200));
        assert!(!k.is_gone(), "the keeper's there: {out}");
        let (_, status) = sh_through(&k, &rules, "exit 0");
        assert!(status.success(), "and runs the next");
        k.end();
    }

    /// A run's pid is tarae's number for it, from its pidfd's fdinfo: its parent, as this process sees it, is
    /// the keeper; inside a PID namespace the keeper's own number for it is another.
    #[test]
    fn a_runs_pid_comes_from_its_pidfd() {
        use std::io::BufRead;
        let info = "pos:\t0\nflags:\t02000002\nmnt_id:\t15\nino:\t1061\nPid:\t4242\nNSpid:\t4242\t7\n";
        assert_eq!(pid_in_fdinfo(info), Ok(4242));
        assert_eq!(pid_in_fdinfo("Pid:\t-1\nNSpid:\t-1\n"), Err(None), "gone");
        assert!(matches!(pid_in_fdinfo("Pid:\t0\n"), Err(Some(_))), "not in this PID namespace");
        assert!(matches!(pid_in_fdinfo("flags:\t0\n"), Err(Some(_))));
        let Some(rules) = open_rules("a_runs_pid_comes_from_its_pidfd") else { return };
        let k = keeper("pidfd");
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo $$; exec sleep 30"]);
        let s = k.start(&cmd, &crate::landlock::thread_name(), rules.fd(), RunNet::Plain).unwrap();
        let mut lines = std::io::BufReader::new(s.spawned.stdout.unwrap()).lines();
        let inner: u32 = lines.next().unwrap().unwrap().trim().parse().unwrap();
        let pid = s.spawned.pid;
        assert_eq!(
            crate::procs::read(pid).map(|o| o.ppid),
            Some(k.pid),
            "tarae's number: the keeper's child"
        );
        match k.ns.pid {
            true => assert_ne!(inner, pid, "inside, another number"),
            false => assert_eq!(inner, pid),
        }
        crate::dap::term_group(pid);
        crate::spawn::wait(pid).unwrap();
        k.end();
    }

    /// A held run tarae can't number (its pidfd says `Pid: 0` — a `/proc` of another PID namespace) never goes
    /// on to its exec: no byte, KILLed where it waits, and its start fails saying why — not a run left going
    /// that tarae could neither wait on nor end.
    #[test]
    fn a_held_run_tarae_cant_number_never_goes_on() {
        if !namespaced("a_held_run_tarae_cant_number_never_goes_on") {
            return;
        }
        let Some(rules) = open_rules("a_held_run_tarae_cant_number_never_goes_on") else { return };
        let k = keeper("unnumbered");
        if !k.ns.pid {
            let line = format!(
                "{}sandbox::keeper::tests::a_held_run_tarae_cant_number_never_goes_on — no PID namespace here: \
                 its runs aren't held\n",
                crate::test_results::SELF_SKIP
            );
            let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
            k.end();
            return;
        }
        lock(&k.state).fdinfo = Some("Pid:\t0\n");
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "exit 3"]);
        let e = k
            .start(&cmd, &crate::landlock::thread_name(), rules.fd(), RunNet::Plain)
            .err()
            .expect("its start fails");
        assert!(e.to_string().contains("PID namespace"), "{e}");
        let deadline = Instant::now() + STARTED_WITHIN;
        let mut s = lock(&k.state);
        while s.exits.is_empty() && Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            s = k.changed.wait_timeout(s, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        let ended = s.exits.clone();
        drop(s);
        assert!(
            matches!(ended[..], [st] if libc::WIFSIGNALED(st) && libc::WTERMSIG(st) == libc::SIGKILL),
            "killed before its exec, not ended by its program: {ended:?}"
        );
        k.end();
    }

    /// A policy that opens a port, this machine's ports or the network gets the session's network namespace as
    /// one that opens nothing does — its calls routed out of it (step 4) — where tarae can put a socket of its
    /// own in a run's place (`supervisor::swap_unavailable`); where it can't, none (D-a), and the keeper says
    /// why. A keeper of the other key is superseded.
    #[test]
    fn an_opened_policys_keeper_has_its_netns_where_the_swap_works() {
        use super::super::tier;
        if !namespaced("an_opened_policys_keeper_has_its_netns_where_the_swap_works") {
            return;
        }
        let swaps = super::super::supervisor::swap_unavailable().is_none();
        let (closed, _, r1) = project_in("netns-closed", Vec::new(), None, &[]);
        let (open, _, r2) = project_in("netns-open", vec![9], None, &[]);
        let (closed_ns, open_ns) = (tier::for_policy(&closed), tier::for_policy(&open));
        let served = super::super::supervisor::unavailable().is_none();
        assert_eq!(
            (closed_ns.net, open_ns.net),
            (ns().net, ns().net && (swaps || !served)),
            "swaps: {swaps}"
        );
        assert!(tier::opens(&super::super::Policy { localhost: true, ..closed.clone() }));
        assert!(tier::opens(&super::super::Policy { network: true, ..closed.clone() }));
        let root = std::env::temp_dir().join(format!("tarae-keeper-netns-{}", std::process::id()));
        let net = |pid: u32| std::fs::read_link(format!("/proc/{pid}/ns/net")).unwrap();
        let a = ready(&root, Some("closed"), closed_ns).unwrap();
        assert_ne!(net(a.pid), net(std::process::id()), "the session's own");
        let b = ready(&root, Some("open"), open_ns).unwrap();
        assert_ne!(a.pid, b.pid, "another keeper");
        match open_ns.net {
            true => assert_ne!(net(b.pid), net(std::process::id()), "the session's own, routed"),
            false => {
                assert_eq!(net(b.pid), net(std::process::id()), "this machine's: no swap here");
                assert!(b.said().contains("network shared"), "{}", b.said());
            }
        }
        assert!(gone_within(&a, END_WITHIN), "the idle one superseded ends");
        b.end();
        for r in [r1, r2] {
            std::fs::remove_dir_all(r).ok();
        }
    }

    /// Finding 2's measurement (step 3): which object a rule must be on for a run to write in the session's
    /// own `/dev/shm` and open a pty of its own `devpts` — this process's `/dev/shm`/`/dev/pts` (the mounts the
    /// session's sit on), `/dev`, or the session's own (opened through `/proc/<keeper>/root`). Every ruleset
    /// reads and runs everywhere besides. Prints `measure: …` lines.
    #[test]
    #[ignore = "a measurement: run by hand (step 3, finding 2)"]
    fn measure_where_a_rule_holds_over_the_session_mounts() {
        use std::os::unix::fs::MetadataExt;
        if !namespaced("measure_where_a_rule_holds_over_the_session_mounts") {
            return;
        }
        let abi = crate::landlock::abi().unwrap();
        let k = keeper("rules-measure");
        let theirs = format!("/proc/{}/root", k.pid);
        let id = |p: &str| {
            std::fs::metadata(p).map(|m| format!("{}:{}", m.dev(), m.ino())).unwrap_or_else(|e| e.to_string())
        };
        println!(
            "measure: ABI {abi} · /dev/shm here {} · the session's {} · /dev/pts here {} · the session's {}",
            id("/dev/shm"),
            id(&format!("{theirs}/dev/shm")),
            id("/dev/pts"),
            id(&format!("{theirs}/dev/pts"))
        );
        let info = std::fs::read_to_string(format!("/proc/{}/mountinfo", k.pid)).unwrap_or_default();
        for l in
            info.lines().filter(|l| [" /dev/shm ", " /dev/pts ", " /dev/ptmx "].iter().any(|m| l.contains(m)))
        {
            println!("measure: the session's mountinfo: {l}");
        }
        #[repr(C, packed)]
        struct Beneath {
            allowed_access: u64,
            parent_fd: i32,
        }
        let read = crate::landlock::EXECUTE | crate::landlock::READ_FILE | crate::landlock::READ_DIR;
        let rules_on = |place: Option<&str>| {
            let mut r = crate::landlock::Ruleset::new(abi).unwrap();
            r.allow(Path::new("/"), read).unwrap();
            if let Some(p) = place {
                // (opened as it is — `/proc/<pid>/root` is a magic link `Ruleset::allow` won't follow)
                let at = std::fs::File::open(p).unwrap();
                let rule = Beneath {
                    allowed_access: crate::landlock::handled(abi) & !crate::landlock::UNGRANTED,
                    parent_fd: at.as_raw_fd(),
                };
                // SAFETY: both descriptors open for the call; the kernel only reads the rule.
                let n = unsafe {
                    libc::syscall(libc::SYS_landlock_add_rule, r.fd().as_raw_fd(), 1, &raw const rule, 0u32)
                };
                assert_eq!(n, 0, "{p}: {}", io::Error::last_os_error());
            }
            r
        };
        let shm = ": > /dev/shm/tarae-m.$$ && rm /dev/shm/tarae-m.$$ && echo ok";
        let (shm_here, shm_theirs) = (format!("{theirs}/dev/shm"), format!("{theirs}/dev/pts"));
        let places: [(&str, Option<&str>); 5] = [
            ("no rule", None),
            ("/dev/shm and /dev/pts here", Some("HERE")),
            ("/dev here", Some("/dev")),
            ("the session's own", Some("THEIRS")),
            ("/", Some("/")),
        ];
        for (name, place) in places {
            let (shm_rules, pty_rules) = match place {
                Some("HERE") => (rules_on(Some("/dev/shm")), rules_on(Some("/dev/pts"))),
                Some("THEIRS") => (rules_on(Some(&shm_here)), rules_on(Some(&shm_theirs))),
                p => (rules_on(p), rules_on(p)),
            };
            let (out, _) = sh_through(&k, &shm_rules, &format!("{shm} 2>&1"));
            let pty = helper_through(&k, pty_rules.fd(), RunNet::Plain, "pty", &[], None);
            println!("measure: rule on {name}: shm {} · pty {pty}", out.trim());
        }
        k.end();
    }

    /// Not served, inside the session's own network namespace a run's internet sockets are its own
    /// (`seccomp::Mode::OwnNetwork`, Landlock's TCP and UDP rights unhandled — `Ruleset::new_own_network`): it
    /// listens on its own `lo` and reaches itself; a listener of this process's on this machine's loopback isn't
    /// there for it.
    #[test]
    fn in_plain_mode_a_run_reaches_only_its_own_loopback() {
        if !namespaced("in_plain_mode_a_run_reaches_only_its_own_loopback") {
            return;
        }
        let Ok(abi) = crate::landlock::abi() else { return };
        let mut rules = crate::landlock::Ruleset::new_own_network(abi).unwrap();
        rules.allow(Path::new("/"), crate::landlock::ALL).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port().to_string();
        let k = keeper("loopback");
        let said = helper_through(&k, rules.fd(), RunNet::Own, "loopback", &[("TARAE_PORT", &port)], None);
        assert_eq!(said, format!("own=ok tarae's={}", libc::ECONNREFUSED));
        k.end();
    }

    /// How a run's sockets go is decided once, by `linux::confine` (`Confine.net`), and its rules are made for
    /// that answer: a keeper without a network namespace, not served → `Plain`, TCP and UDP handled; served →
    /// `Served`, handled; not served in the session's own → `Own`, left to the run — and a run started on that
    /// answer (its seccomp mode) under those rules reaches its own `lo`.
    #[test]
    fn confine_decides_a_runs_network_once() {
        let test = "confine_decides_a_runs_network_once";
        if open_rules(test).is_none() {
            return;
        }
        // (below ABI 4 Landlock has no network rights: only the decision is there to check)
        let abi = crate::landlock::abi().unwrap();
        let nets = crate::landlock::handled_net(abi) != 0;
        for (net, handled) in
            [(RunNet::Plain, nets), (RunNet::Served, nets), (RunNet::Routed, nets), (RunNet::Own, false)]
        {
            let r = super::super::linux::ruleset_for(net, abi).unwrap();
            assert_eq!(r.net != 0, handled, "{net:?}'s rules");
        }
        let unserved = || Err::<(), String>("not served".into());
        let none = keeper_in("netrights-none", Ns::NONE);
        let (_, c, _) = project_served("netrights-none", vec![], Some(&none), &[], unserved());
        assert_eq!((c.net, c.handled_net() != 0), (RunNet::Plain, nets), "no network namespace");
        none.end();
        if !namespaced(test) {
            return;
        }
        let k = keeper("netrights");
        assert!(k.ns.net, "a policy opening nothing gets the session's network: {}", k.said());
        let (_, c, _) = project_served("netrights-served", vec![], Some(&k), &[], Ok(()));
        assert_eq!((c.net, c.handled_net() != 0), (RunNet::Routed, nets), "served: routed");
        let (_, c, root) = project_served("netrights-own", vec![], Some(&k), &[], unserved());
        assert_eq!((c.net, c.handled_net()), (RunNet::Own, 0), "plain, in its own network namespace");
        let said = helper_through(&k, c.rules_fd(), c.net, "loopback", &[], Some(&root));
        assert!(said.starts_with("own=ok "), "its own sockets, under its own rules: {said}");
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    // ── step 4: the run's own sockets in the session, routed; the swap ─────────────────────────────────────

    /// Whether this system's runs are routed — served, in the session's network namespace — else says the test
    /// skips itself, and why.
    fn routed_here(test: &str) -> bool {
        if !namespaced(test) {
            return false;
        }
        match super::super::supervisor::unavailable() {
            None => true,
            Some(why) => {
                let line = format!(
                    "{}sandbox::keeper::tests::{test} — not served here: {why}\n",
                    crate::test_results::SELF_SKIP
                );
                let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
                false
            }
        }
    }

    /// A project's policy, `change`d (what it opens), and its rules for `k`'s runs, served — routed.
    fn routed_project(
        name: &str,
        k: &Keeper,
        change: impl FnOnce(&mut super::super::Policy),
    ) -> (Arc<super::super::Policy>, super::super::linux::Confine, PathBuf) {
        let (mut p, c, root) = project_served(name, Vec::new(), Some(k), &[], Ok(()));
        assert_eq!(c.net, RunNet::Routed, "{}", k.said());
        change(&mut p);
        (Arc::new(p), c, root)
    }

    /// The test binary on `helper` doing `what` (with `env` set), a run of `k`'s served under `p` with `c`'s
    /// rules, in `root`: its output's lines as they come, its pid, and its refusals.
    fn served_helper(
        k: &Keeper,
        (p, c, root): (&Arc<super::super::Policy>, &super::super::linux::Confine, &Path),
        what: &str,
        env: &[(&str, &str)],
    ) -> (impl Iterator<Item = String> + use<>, u32, std::sync::mpsc::Receiver<super::super::Denial>) {
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "sandbox::keeper::tests::helper",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(HELPER, what)
        .envs(env.iter().copied())
        .current_dir(root);
        let (tx, rx) = std::sync::mpsc::channel();
        let name = crate::landlock::thread_name();
        let s = k.start_served(&cmd, &name, c.rules_fd(), c.net, p.clone(), tx).unwrap();
        let pid = s.spawned.pid;
        let lines =
            io::BufRead::lines(std::io::BufReader::new(s.spawned.stdout.unwrap())).map_while(Result::ok);
        (lines, pid, rx)
    }

    /// What a helper said next (`helper: …`), its other lines skipped.
    fn said_next(lines: &mut impl Iterator<Item = String>) -> String {
        lines
            .find_map(|l| l.split_once("helper: ").map(|(_, s)| s.to_string()))
            .unwrap_or_else(|| "nothing".into())
    }

    /// A helper's one answer, waited for to its end — and the refusals said meanwhile.
    fn served_said(
        k: &Keeper,
        run: (&Arc<super::super::Policy>, &super::super::linux::Confine, &Path),
        what: &str,
        env: &[(&str, &str)],
    ) -> (String, Vec<super::super::Denial>) {
        let (mut lines, pid, refused) = served_helper(k, run, what, env);
        let said = said_next(&mut lines);
        lines.for_each(drop);
        crate::spawn::wait(pid).unwrap();
        (said, refused.try_iter().collect())
    }

    /// This machine's address beyond its loopback — the "outside" a run reaches out to here, a listener of the
    /// test's own on it, beyond this machine for a policy made with no own addresses (`own_addresses: Some([])`:
    /// a target truly beyond would send packets off the machine, which no test does) — None where there's none
    /// (a connected UDP socket's own address: no packet is sent).
    fn outside() -> Option<std::net::Ipv4Addr> {
        let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        s.connect("192.0.2.1:9").ok()?;
        match s.local_addr().ok()?.ip() {
            std::net::IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_unspecified() => Some(v4),
            _ => None,
        }
    }

    /// A listener of the test's on `at` that answers one connection's line with `answer`: its address, and
    /// where whoever it served is said.
    fn answering(
        at: &str,
        answer: &'static str,
    ) -> (std::net::SocketAddr, std::sync::mpsc::Receiver<std::net::SocketAddr>) {
        let l = std::net::TcpListener::bind(at).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let addr = l.local_addr().unwrap();
        std::thread::spawn(move || {
            if let Ok((s, peer)) = l.accept() {
                let _ = tx.send(peer);
                let mut line = String::new();
                let _ = io::BufRead::read_line(&mut std::io::BufReader::new(&s), &mut line);
                let _ = io::Write::write_all(&mut &s, format!("{answer}\n").as_bytes());
            }
        });
        (addr, rx)
    }

    /// Two runs of one session reach each other on its loopback: the first leaves a listener, the second
    /// connects there — the policy closed, and with this machine's every port opened (the session's own
    /// listener first, D2).
    #[test]
    fn two_runs_share_the_sessions_loopback() {
        if !routed_here("two_runs_share_the_sessions_loopback") {
            return;
        }
        for localhost in [false, true] {
            let k = keeper(&format!("share-{localhost}"));
            let (p, c, root) = routed_project(&format!("share-{localhost}"), &k, |p| p.localhost = localhost);
            let (mut one, first, _) = served_helper(&k, (&p, &c, &root), "listen", &[]);
            let port = said_next(&mut one);
            let to = format!("127.0.0.1:{}", port.strip_prefix("port ").unwrap_or_else(|| panic!("{port}")));
            let (said, _) = served_said(&k, (&p, &c, &root), "connect", &[("TARAE_TO", &to)]);
            assert_eq!(said, "reached got hello", "localhost {localhost}");
            one.for_each(drop);
            crate::spawn::wait(first).unwrap();
            k.end();
            std::fs::remove_dir_all(&root).ok();
        }
    }

    /// A listener of the user's on this machine's loopback isn't reached from a session whose policy opens no
    /// port: the run's connect stays in the session, whose loopback refuses it; nothing arrives here.
    #[test]
    fn the_users_loopback_isnt_reached() {
        if !routed_here("the_users_loopback_isnt_reached") {
            return;
        }
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.set_nonblocking(true).unwrap();
        let to = l.local_addr().unwrap().to_string();
        let k = keeper("users-loopback");
        let (p, c, root) = routed_project("users-loopback", &k, |_| {});
        let (said, refused) = served_said(&k, (&p, &c, &root), "connect", &[("TARAE_TO", &to)]);
        assert_eq!(said, libc::ECONNREFUSED.to_string(), "the session's own loopback answers");
        assert!(refused.is_empty(), "no rule refused it: {refused:?}");
        assert!(l.accept().is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock), "nothing arrived here");
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// D2: a port `llm.sandbox-ports` opens reaches this machine's listener on it — unless the session listens
    /// on that port itself: then the session's answers.
    #[test]
    fn an_opened_port_reaches_this_machine() {
        if !routed_here("an_opened_port_reaches_this_machine") {
            return;
        }
        let (at, served) = answering("127.0.0.1:0", "machine hello");
        let k = keeper("opened-port");
        let (p, c, root) = routed_project("opened-port", &k, |p| p.ports = vec![at.port()]);
        let to = at.to_string();
        let (said, _) = served_said(&k, (&p, &c, &root), "connect", &[("TARAE_TO", &to)]);
        assert_eq!(said, "reached machine hello");
        assert!(served.recv_timeout(Duration::from_secs(1)).is_ok());
        let port = at.port().to_string();
        let (mut one, first, _) = served_helper(&k, (&p, &c, &root), "listen", &[("TARAE_PORT", &port)]);
        assert_eq!(said_next(&mut one), format!("port {port}"), "the same port, in the session");
        let (said, _) = served_said(&k, (&p, &c, &root), "connect", &[("TARAE_TO", &to)]);
        assert_eq!(said, "reached got hello", "the session's own listener first");
        one.for_each(drop);
        crate::spawn::wait(first).unwrap();
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// The swap: a non-blocking connect out of the session (here to this machine's own address beyond loopback,
    /// its port opened — the route that swaps since the network open goes through the proxy, step 5) is made on
    /// a socket of tarae's put in the run's place — `EINPROGRESS` as the run's own would answer, and what it set
    /// carried: non-blocking, close-on-exec, `TCP_NODELAY`, `SO_KEEPALIVE`, `SO_RCVTIMEO`; its connection arrives.
    #[test]
    fn a_swapped_socket_keeps_nonblocking() {
        if !routed_here("a_swapped_socket_keeps_nonblocking") {
            return;
        }
        let Some(ip) = outside() else { return };
        let (at, served) = answering(&format!("{ip}:0"), "out hello");
        let k = keeper("swap");
        let own = |p: &mut super::super::Policy| {
            (p.ports, p.own_addresses) = (vec![at.port()], Some(vec![ip.into()]))
        };
        let (p, c, root) = routed_project("swap", &k, own);
        let (said, refused) = served_said(&k, (&p, &c, &root), "swap", &[("TARAE_TO", &at.to_string())]);
        assert_eq!(said, swapped_said(), "{refused:?}");
        assert!(served.recv_timeout(Duration::from_secs(5)).is_ok(), "its connection arrived");
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// What `helper_swap` says of a socket swapped out: `EINPROGRESS`, everything it set carried, bound to no
    /// device.
    fn swapped_said() -> String {
        let n = libc::EINPROGRESS;
        format!("connect={n} nonblock=1 cloexec=1 nodelay=1 keepalive=1 rcvtimeo=7 error=0 dev=")
    }

    /// Step 5: with the network open a run's connect to the proxy's address holds a real TCP socket of its own
    /// family — every option a client sets on it works (a unix socket in its place broke Python's `TCP_NODELAY`,
    /// M5) and its `O_NONBLOCK` is kept — and what it sends goes through tarae's proxy to the host it names, in
    /// tier C (D-e) and the session's netns alike. The outside is a listener of the test's on this machine's own
    /// address, named for the proxy (`proxy::TEST_NAMES`), beyond this machine for a policy made with no own
    /// addresses.
    #[test]
    fn a_proxied_connect_holds_a_tcp_socket() {
        let Some(tiers) = served_tiers("a_proxied_connect_holds_a_tcp_socket") else { return };
        let Some(ip) = outside() else { return };
        for (tier, ns) in tiers {
            let (at, served) = answering(&format!("{ip}:0"), "out hello");
            let host = format!("outside-{tier}.test");
            super::super::proxy::TEST_NAMES.lock().unwrap().push((host.clone(), at));
            let name = format!("proxied-{tier}");
            let k = keeper_in(&name, ns);
            let (p, c, root) = project_served(&name, Vec::new(), Some(&k), &[], Ok(()));
            // (the network open to any host — `llm.sandbox-hosts = "any"`, as `policy_in` makes it)
            let egress = super::super::proxy::egress::Egress { any: true, ..Default::default() };
            let p = Arc::new(super::super::Policy {
                network: true,
                own_addresses: Some(Vec::new()),
                egress,
                ..p
            });
            let env = [
                ("TARAE_TO", super::super::route::PROXY.to_string()),
                ("TARAE_HOST", format!("{host}:{}", at.port())),
            ];
            let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let (said, refused) = served_said(&k, (&p, &c, &root), "proxied", &env);
            assert_eq!(
                said, "connect=0 family=2 stream=1 nodelay=1 nonblock=1 peer=127.0.0.1 reply=out hello",
                "{tier}: {refused:?}"
            );
            assert!(served.recv_timeout(Duration::from_secs(5)).is_ok(), "{tier}: it arrived outside");
            k.end();
            std::fs::remove_dir_all(&root).ok();
        }
    }

    /// Step 5e: a host a run's rules don't name (`llm.sandbox-hosts = []`) is held at the proxy — reached through
    /// the supervisor's TCP pair as macOS's runs reach it through their ports — and the main loop is asked, for
    /// that run; the session's answer lets it through, and the session's next run reaches the host unasked. The
    /// test stands in for the main loop (`proxy::TEST_ASKS`). In tier C and the netns alike.
    #[test]
    fn an_unknown_host_waits_for_the_answer() {
        use super::super::proxy::{self, FromProxy, Scope, ToProxy};
        let Some(tiers) = served_tiers("an_unknown_host_waits_for_the_answer") else { return };
        let Some(ip) = outside() else { return };
        for (tier, ns) in tiers {
            // (the outside answers two requests: the first run's, then the next's)
            let l = std::net::TcpListener::bind((ip, 0)).unwrap();
            let at = l.local_addr().unwrap();
            let (tx, served) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                for s in l.incoming().take(2).flatten() {
                    let mut line = String::new();
                    let _ = io::BufRead::read_line(&mut std::io::BufReader::new(&s), &mut line);
                    let _ = io::Write::write_all(&mut &s, b"out hello\n");
                    let _ = tx.send(());
                }
            });
            let host = format!("ask-{tier}.test");
            proxy::TEST_NAMES.lock().unwrap().push((host.clone(), at));
            let name = format!("asked-{tier}");
            let k = keeper_in(&name, ns);
            let (p, c, root) = project_served(&name, Vec::new(), Some(&k), &[], Ok(()));
            let session = proxy::session_of(&p.root);
            let (asks, asked) = std::sync::mpsc::channel();
            proxy::TEST_ASKS.lock().unwrap().push((session, asks));
            // (two runs of the session, ids of their own; their rules name no host)
            let listed = |run_id| {
                let egress = proxy::egress::Egress::default();
                let own = Some(Vec::new());
                Arc::new(super::super::Policy {
                    network: true,
                    own_addresses: own,
                    egress,
                    run_id,
                    ..p.clone()
                })
            };
            let (first, second) = (listed(5_000_001), listed(5_000_002));
            // The main loop, stood in: the session's answer to the first question
            let answer = std::thread::spawn(move || {
                let ask = asked.recv_timeout(Duration::from_secs(10)).ok()?;
                let FromProxy::Ask { run, host, port, .. } = ask.clone();
                proxy::get().send(ToProxy::Answer { session, run, host, port, scope: Scope::Session });
                Some((ask, asked))
            });
            let env = [
                ("TARAE_TO", super::super::route::PROXY.to_string()),
                ("TARAE_HOST", format!("{host}:{}", at.port())),
            ];
            let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let reached = "connect=0 family=2 stream=1 nodelay=1 nonblock=1 peer=127.0.0.1 reply=out hello";
            let (said, refused) = served_said(&k, (&first, &c, &root), "proxied", &env);
            assert_eq!(said, reached, "{tier}: the held request went on — {refused:?}");
            let (ask, asked) =
                answer.join().unwrap().unwrap_or_else(|| panic!("{tier}: the main loop asked"));
            assert!(
                matches!(&ask, FromProxy::Ask { run: 5_000_001, host: h, process, .. } if *h == host && !process.is_empty()),
                "{tier}: {ask:?}"
            );
            let (said, refused) = served_said(&k, (&second, &c, &root), "proxied", &env);
            assert_eq!(said, reached, "{tier}: the session's next run — {refused:?}");
            assert!(asked.try_recv().is_err(), "{tier}: not asked again");
            for n in ["first", "second"] {
                assert!(
                    served.recv_timeout(Duration::from_secs(5)).is_ok(),
                    "{tier}: the {n} arrived outside"
                );
            }
            k.end();
            std::fs::remove_dir_all(&root).ok();
        }
    }

    /// Step 5: with the network through the proxy, a connect beyond this machine that doesn't go through it — a
    /// tool that ignores `HTTPS_PROXY` — is refused (`EPERM`, `network-direct`) and said in words that name the
    /// way out; nothing arrives outside. A datagram out is refused the same way. In tier C and the netns alike —
    /// but where the session's network takes what goes beyond (6d, TPROXY), the connect isn't the supervisor's to
    /// refuse: it reaches a planted socket, the proxy's to decide by the host it names (here none: still nothing
    /// arrives); the datagram is refused as before (UDP: 6e's).
    #[test]
    fn a_direct_connect_is_refused_with_the_proxy() {
        let Some(tiers) = served_tiers("a_direct_connect_is_refused_with_the_proxy") else { return };
        let Some(ip) = outside() else { return };
        for (tier, ns) in tiers {
            let l = std::net::TcpListener::bind((ip, 0)).unwrap();
            l.set_nonblocking(true).unwrap();
            let udp = std::net::UdpSocket::bind((ip, 0)).unwrap();
            udp.set_nonblocking(true).unwrap();
            let name = format!("direct-{tier}");
            let k = keeper_in(&name, ns);
            let redirected = lock(&k.network).is_some();
            let (p, c, root) = project_served(&name, Vec::new(), Some(&k), &[], Ok(()));
            let p = Arc::new(super::super::Policy { network: true, own_addresses: Some(Vec::new()), ..p });
            let to = l.local_addr().unwrap().to_string();
            if redirected {
                let (said, refused) = served_said(&k, (&p, &c, &root), "within", &[("TARAE_TO", &to)]);
                assert_eq!(
                    said, "reached open",
                    "{tier}: on a planted socket, waiting for a host — {refused:?}"
                );
                assert!(!refused.iter().any(|d| d.op == "network-direct"), "{tier}: {refused:?}");
            } else {
                let (said, refused) = served_said(&k, (&p, &c, &root), "connect", &[("TARAE_TO", &to)]);
                assert_eq!(said, libc::EPERM.to_string(), "{tier}: {refused:?}");
                let d = refused
                    .iter()
                    .find(|d| d.op == "network-direct")
                    .unwrap_or_else(|| panic!("{refused:?}"));
                assert!(
                    d.plain().contains(&format!("{to} directly")) && d.plain().contains("HTTPS_PROXY"),
                    "{}",
                    d.plain()
                );
            }
            let udp_to = udp.local_addr().unwrap().to_string();
            let (said, refused) = served_said(&k, (&p, &c, &root), "udp", &[("TARAE_TO", &udp_to)]);
            assert!(!said.starts_with("answer="), "{tier}: {said}");
            assert!(refused.iter().any(|d| d.op == "network-direct"), "{tier}: {refused:?}");
            assert!(
                l.accept().is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock),
                "{tier}: nothing arrived"
            );
            assert!(udp.recv_from(&mut [0u8; 8]).is_err(), "{tier}: no datagram arrived");
            k.end();
            std::fs::remove_dir_all(&root).ok();
        }
    }

    /// Where no socket of tarae's can take a run's place (a kernel before 5.14 — D-a: tier C; forced here, every
    /// kernel tried has it), the network open reaches nothing beyond this machine's loopback: a connect to the
    /// proxy is refused (`EPERM`, `network-outbound`) and said with why, and the report says loopback only; an
    /// opened port on loopback — tier C's own socket, no swap — is still reached.
    #[test]
    fn without_the_swap_nothing_goes_beyond_loopback() {
        if super::super::supervisor::unavailable().is_some() || open_rules("without_the_swap").is_none() {
            return;
        }
        let (lo_at, lo_served) = answering("127.0.0.1:0", "machine hello");
        let k = keeper_in("no-swap", Ns::NONE);
        let (p, c, root) = project_served("no-swap", Vec::new(), Some(&k), &[], Ok(()));
        let why = "this kernel can't put a socket in a run's place (a test's stand-in)";
        let p = Arc::new(super::super::Policy {
            network: true,
            ports: vec![lo_at.port()],
            own_addresses: Some(Vec::new()),
            test_swap_missing: Some(why.into()),
            ..p
        });
        let to = super::super::route::PROXY.to_string();
        let (said, refused) = served_said(&k, (&p, &c, &root), "connect", &[("TARAE_TO", &to)]);
        let (lo_said, _) = served_said(&k, (&p, &c, &root), "connect", &[("TARAE_TO", &lo_at.to_string())]);
        assert_eq!((said, lo_said), (libc::EPERM.to_string(), "reached machine hello".into()), "{refused:?}");
        let d = refused.iter().find(|d| d.op == "network-outbound").unwrap_or_else(|| panic!("{refused:?}"));
        assert!(d.plain().starts_with(&format!("connecting to {to} — {why}")), "{}", d.plain());
        assert_eq!(d.port_here(), None, "beyond this machine: no port to open");
        assert!(lo_served.recv_timeout(Duration::from_secs(5)).is_ok());
        let made = super::super::Policy { confine: Some(Ok(c.clone())), ..(*p).clone() };
        let report = super::super::Guard::In(Box::new(made)).report(&[]);
        let report = report.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            report.contains(&format!(
                "connect to nothing but this machine's loopback, its ports {} only (llm.sandbox-ports) — {why}, so \
                 nothing goes beyond it, llm.sandbox-network notwithstanding",
                lo_at.port()
            )),
            "{report}"
        );
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// A socket the run bound to a port of its own isn't swapped: its source address can't leave the session —
    /// refused with `EADDRNOTAVAIL`, said as such; nothing arrives outside. One bound to every address and a port
    /// the kernel picks (`0.0.0.0:0`, as Rust's and Java's datagram sockets are) goes out as an unbound one.
    #[test]
    fn a_bound_socket_isnt_swapped() {
        if !routed_here("a_bound_socket_isnt_swapped") {
            return;
        }
        let Some(ip) = outside() else { return };
        let l = std::net::TcpListener::bind((ip, 0)).unwrap();
        l.set_nonblocking(true).unwrap();
        let to = l.local_addr().unwrap().to_string();
        let k = keeper("bound");
        let port = l.local_addr().unwrap().port();
        // (this machine's own address, its port opened: the route that swaps — step 5)
        let (p, c, root) =
            routed_project("bound", &k, |p| (p.ports, p.own_addresses) = (vec![port], Some(vec![ip.into()])));
        let env = [("TARAE_TO", to.as_str()), ("TARAE_PORT", "41999")];
        let (said, refused) = served_said(&k, (&p, &c, &root), "bound", &env);
        assert_eq!(said, format!("bound={}", libc::EADDRNOTAVAIL));
        let d = refused.iter().find(|d| d.op == "network-bound").unwrap_or_else(|| panic!("{refused:?}"));
        assert!(d.plain().contains("bound itself"), "{}", d.plain());
        assert!(l.accept().is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock), "nothing arrived outside");
        let env = [("TARAE_TO", to.as_str()), ("TARAE_PORT", "0")];
        let (said, refused) = served_said(&k, (&p, &c, &root), "bound", &env);
        assert_eq!(said, "bound=0", "every address, the kernel's port: {refused:?}");
        l.set_nonblocking(false).unwrap();
        assert!(l.accept().is_ok(), "it arrived");
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// Step 5c (Q4, closing what D-c opened): with the network through the proxy a resolver `resolv.conf` names
    /// (a stand-in of the test's on this machine's loopback, on its own port) isn't reached — names are the
    /// proxy's to look up, and an open port 53 would be a way out no host rule sees. The datagram is refused,
    /// said as a name lookup, and nothing arrives; in tier C and the session's netns alike. (Measured first: no
    /// tool going through the proxy looked a name up itself — `measure_through_the_proxy`.)
    #[test]
    fn under_the_proxy_a_resolver_isnt_reached() {
        let Some(tiers) = served_tiers("under_the_proxy_a_resolver_isnt_reached") else { return };
        for (tier, ns) in tiers {
            let resolver = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            resolver.set_nonblocking(true).unwrap();
            let at = resolver.local_addr().unwrap();
            let name = format!("resolver-{tier}");
            let k = keeper_in(&name, ns);
            let (p, c, root) = project_served(&name, Vec::new(), Some(&k), &[], Ok(()));
            let p = Arc::new(super::super::Policy { network: true, resolvers: vec![at], ..p });
            let (said, refused) = served_said(&k, (&p, &c, &root), "udp", &[("TARAE_TO", &at.to_string())]);
            assert!(!said.starts_with("answer="), "{tier}: {said}");
            let d =
                refused.iter().find(|d| d.op == "network-direct").unwrap_or_else(|| panic!("{refused:?}"));
            assert!(d.plain().contains("a name lookup"), "{tier}: {}", d.plain());
            assert!(resolver.recv_from(&mut [0u8; 64]).is_err(), "{tier}: nothing reached the resolver");
            k.end();
            std::fs::remove_dir_all(&root).ok();
        }
    }

    /// D-d: with the network open, an address never reached (multicast here — the kernel would refuse a TCP
    /// connect there itself, so nothing goes out even broken) is refused before any connect, said.
    #[test]
    fn the_never_reached_are_refused() {
        if !routed_here("the_never_reached_are_refused") {
            return;
        }
        let k = keeper("never");
        let (p, c, root) = routed_project("never", &k, |p| p.network = true);
        let (said, refused) = served_said(&k, (&p, &c, &root), "connect", &[("TARAE_TO", "224.0.0.1:9")]);
        assert_eq!(said, libc::EPERM.to_string());
        assert!(refused.iter().any(|d| d.op == "network-never"), "{refused:?}");
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// The served tiers here, each run's keeper born in them: tier C's (no namespaces), and the session's netns
    /// where this system gives one — None where nothing's served.
    fn served_tiers(test: &str) -> Option<Vec<(&'static str, Ns)>> {
        if super::super::supervisor::unavailable().is_some() || open_rules(test).is_none() {
            return None;
        }
        let mut tiers = vec![("c", Ns::NONE)];
        if ns().net {
            tiers.push(("netns", ns()));
        }
        Some(tiers)
    }

    /// This machine's own address beyond loopback (a container's, a LAN's, Docker's bridge's) is this machine,
    /// not beyond it: with the network open a listener of the test's there isn't reached — refused, said, nothing
    /// arrives — until its port is opened (`llm.sandbox-ports`); in tier C and the session's netns alike. The own
    /// addresses as a policy reads them when it's made (`Here::now`).
    #[test]
    fn this_machines_own_address_is_reached_only_where_opened() {
        let Some(tiers) = served_tiers("this_machines_own_address_is_reached_only_where_opened") else {
            return;
        };
        let Some(ip) = outside() else { return };
        let own = super::super::Here::now().addresses;
        assert!(own.as_ref().is_some_and(|a| a.contains(&ip.into())), "{ip} among {own:?}");
        for (tier, ns) in tiers {
            let name = format!("own-{tier}");
            let k = keeper_in(&name, ns);
            let (p, c, root) = project_served(&name, Vec::new(), Some(&k), &[], Ok(()));
            let p = super::super::Policy { own_addresses: own.clone(), ..p };
            let (at, served) = answering(&format!("{ip}:0"), "machine hello");
            let to = at.to_string();
            let open = Arc::new(super::super::Policy { network: true, ..p.clone() });
            let (said, refused) = served_said(&k, (&open, &c, &root), "connect", &[("TARAE_TO", &to)]);
            assert_eq!(said, libc::EPERM.to_string(), "{tier}: {refused:?}");
            assert!(
                refused.iter().any(|d| d.op == "network-outbound" && d.target.contains(&to)),
                "{tier}: {refused:?}"
            );
            assert!(served.recv_timeout(Duration::from_millis(300)).is_err(), "{tier}: nothing arrived");
            let opened = Arc::new(super::super::Policy { network: true, ports: vec![at.port()], ..p });
            let (said, refused) = served_said(&k, (&opened, &c, &root), "connect", &[("TARAE_TO", &to)]);
            assert_eq!(said, "reached machine hello", "{tier}: opened: {refused:?}");
            k.end();
            std::fs::remove_dir_all(&root).ok();
        }
    }

    /// A socket tarae put out of the session isn't listened on: once its connect beyond is refused (a closed port
    /// beyond — `outside`), or once a connection made is undone (`connect(AF_UNSPEC)`), it's unbound again and a
    /// `listen` would bind it to every address of this machine — refused with `EINVAL` and said; in tier C and the
    /// session's netns alike. (A run's own listener: `two_runs_share_the_sessions_loopback`.)
    #[test]
    fn a_socket_put_out_of_the_session_isnt_listened_on() {
        let Some(tiers) = served_tiers("a_socket_put_out_of_the_session_isnt_listened_on") else { return };
        let Some(ip) = outside() else { return };
        for (tier, ns) in tiers {
            let name = format!("swaplisten-{tier}");
            let k = keeper_in(&name, ns);
            let (p, c, root) = project_served(&name, Vec::new(), Some(&k), &[], Ok(()));
            let closed = std::net::TcpListener::bind((ip, 0)).unwrap();
            let to = closed.local_addr().unwrap().to_string();
            drop(closed);
            let (at, served) = answering(&format!("{ip}:0"), "out hello");
            // (this machine's own address, both ports opened: the route that swaps — step 5)
            let ports = vec![to.rsplit_once(':').unwrap().1.parse().unwrap(), at.port()];
            let p = Arc::new(super::super::Policy { ports, own_addresses: Some(vec![ip.into()]), ..p });
            let (failed, refused) = served_said(&k, (&p, &c, &root), "swaplisten", &[("TARAE_TO", &to)]);
            let env = [("TARAE_TO", at.to_string()), ("TARAE_UNSPEC", "1".into())];
            let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
            let (undone, refused_after) = served_said(&k, (&p, &c, &root), "swaplisten", &env);
            // (both before either is judged: a break shows each)
            let (e, inval) = (libc::ECONNREFUSED, libc::EINVAL);
            assert_eq!(
                (failed, undone),
                (format!("connect={e} listen={inval}"), format!("connect=0 unspec=0 listen={inval}")),
                "{tier}: {refused:?} {refused_after:?}"
            );
            assert!(served.recv_timeout(Duration::from_secs(5)).is_ok(), "{tier}: its connection arrived");
            for r in [refused, refused_after] {
                assert!(r.iter().any(|d| d.op == "network-bind"), "{tier}: {r:?}");
            }
            k.end();
            std::fs::remove_dir_all(&root).ok();
        }
    }

    /// The swap's window: tarae's socket is known as put out of the session before the run holds it. Another thread
    /// of the run's connecting the same number to this machine's loopback the moment it's there (a port not
    /// opened: the session's) is refused — not made on tarae's socket in tarae's namespace, where it would reach
    /// the user's listener. The supervisor pauses in that window (`swap_pause`), so the second call lands in it
    /// every time.
    #[test]
    fn a_swapped_socket_is_taraes_before_the_run_holds_it() {
        if !routed_here("a_swapped_socket_is_taraes_before_the_run_holds_it") {
            return;
        }
        let Some(ip) = outside() else { return };
        let (at, served) = answering(&format!("{ip}:0"), "out hello");
        let users = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        users.set_nonblocking(true).unwrap();
        let k = keeper("swaprace");
        // (this machine's own address, its port opened: the route that swaps — step 5)
        let (p, c, root) = routed_project("swaprace", &k, |p| {
            (p.ports, p.own_addresses) = (vec![at.port()], Some(vec![ip.into()]));
            p.swap_pause = Some(Duration::from_secs(1));
        });
        let (to, lo) = (at.to_string(), users.local_addr().unwrap().to_string());
        let (said, refused) =
            served_said(&k, (&p, &c, &root), "racelo", &[("TARAE_TO", &to), ("TARAE_LO", &lo)]);
        assert_eq!(said, format!("lo={} out=0", libc::EADDRNOTAVAIL), "{refused:?}");
        assert!(
            users.accept().is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock),
            "the user's listener: nothing"
        );
        assert!(served.recv_timeout(Duration::from_secs(5)).is_ok(), "the swapped connect arrived");
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// A socket made outside the session's netns and handed to a run (over a unix socket in the project, by a process
    /// of the test's standing for one outside) is no socket of the session's: a connect on it to this machine's
    /// loopback (a port not opened), a listen on it, and a connect to an abstract name of this machine's are refused
    /// and said — nothing reaches the test's listeners.
    #[test]
    fn a_socket_from_outside_the_session_is_refused() {
        if !routed_here("a_socket_from_outside_the_session_is_refused") {
            return;
        }
        use std::os::linux::net::SocketAddrExt as _;
        let k = keeper("foreign");
        let (p, c, root) = routed_project("foreign", &k, |_| {});
        let users = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        users.set_nonblocking(true).unwrap();
        let name = format!("tarae-foreign-{}", std::process::id());
        let named = std::os::unix::net::SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let abstract_l = std::os::unix::net::UnixListener::bind_addr(&named).unwrap();
        abstract_l.set_nonblocking(true).unwrap();
        let pass = root.join("pass.sock");
        let l = std::os::unix::net::UnixListener::bind(&pass).unwrap();
        let handing = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            // SAFETY: plain calls; the descriptors made are ours.
            let made: Vec<OwnedFd> = [libc::AF_INET, libc::AF_INET, libc::AF_UNIX]
                .map(|family| unsafe {
                    OwnedFd::from_raw_fd(libc::socket(family, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0))
                })
                .into();
            send_fds(&s, &made.iter().map(|f| f.as_fd()).collect::<Vec<_>>());
            // (held until the run is done with them)
            let _ = io::Read::read(&mut &s, &mut [0u8; 1]);
        });
        let (lo, sock) = (users.local_addr().unwrap().to_string(), pass.display().to_string());
        let env = [("TARAE_SOCK", sock.as_str()), ("TARAE_LO", lo.as_str()), ("TARAE_NAME", name.as_str())];
        let (said, refused) = served_said(&k, (&p, &c, &root), "foreign", &env);
        let e = libc::EPERM;
        assert_eq!(said, format!("connect={e} listen={e} abstract={e}"), "{refused:?}");
        assert!(
            users.accept().is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock),
            "the user's listener: nothing"
        );
        assert!(
            abstract_l.accept().is_err_and(|e| e.kind() == io::ErrorKind::WouldBlock),
            "the abstract one: nothing"
        );
        let outside = refused.iter().filter(|d| d.target.contains("from outside the session")).count();
        assert_eq!(outside, 2, "the connect and the listen said so: {refused:?}");
        handing.join().unwrap();
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// In the session's netns a run makes its own internet sockets — TCP's and UDP's only, as tier C's tarae makes
    /// them: SCTP, MPTCP, UDP-Lite and ICMP's, whose options and messages name addresses tarae never sees, refused.
    #[test]
    fn a_run_makes_tcp_and_udp_sockets_only() {
        if !routed_here("a_run_makes_tcp_and_udp_sockets_only") {
            return;
        }
        let k = keeper("protocols");
        let (p, c, root) = routed_project("protocols", &k, |p| p.network = true);
        let (said, _) = served_said(&k, (&p, &c, &root), "protocols", &[]);
        let e = libc::EPERM;
        assert_eq!(said, format!("0 0 {e} {e} {e} {e}"), "TCP, UDP; SCTP, MPTCP, UDP-Lite, ICMP");
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// A mirror the birth can't make (`lo`'s addresses — only lookups need them) leaves the keeper born without
    /// it, said: its `lo` carries loopback only, its runs go on, an IPv6-only lookup finds nothing.
    #[test]
    fn a_mirror_that_fails_leaves_the_birth_going() {
        let test = "a_mirror_that_fails_leaves_the_birth_going";
        if !namespaced(test) || !ns().net {
            return;
        }
        let Some(rules) = open_rules(test) else { return };
        let root = std::env::temp_dir().join(format!("tarae-keeper-{}-offlo", std::process::id()));
        let both = Families { v4: true, v6: true };
        let k = Keeper::born_lo(
            &root,
            None,
            ns(),
            both,
            true,
            network_for(ns(), &super::super::tier::get().redirect),
            None,
        )
        .expect("born, without its mirrors");
        assert_eq!(k.lo_missing, both, "neither carried");
        assert!(k.said().contains("its lo without an IPv4 or IPv6 address"), "{}", k.said());
        let said =
            helper_through(&k, rules.fd(), RunNet::Own, "lookup6", &[("TARAE_NAME", "ip6-localhost")], None);
        assert!(said.starts_with("v6=::1 lookup6=getaddrinfo"), "{said}");
        let said = helper_through(&k, rules.fd(), RunNet::Own, "lookup4", &[], None);
        assert!(said.starts_with("v4=127.0.0.1 "), "{said}");
        k.end();
        let k = Keeper::born_lo(
            &root,
            None,
            ns(),
            both,
            false,
            network_for(ns(), &super::super::tier::get().redirect),
            None,
        )
        .expect("a keeper starts");
        assert_eq!(k.lo_missing, Families::default(), "both carried where they can be");
        assert!(!k.said().contains("its lo"), "{}", k.said());
        k.end();
    }

    /// The second lock under the routing: a run under `Routed`'s rules whose `connect` the filter let through
    /// (seccomp's `OwnNetwork` standing in) is refused by Landlock itself (ABI ≥ 4), even to its own listener.
    #[test]
    fn a_routed_runs_own_connect_is_refused_by_the_kernel() {
        let test = "a_routed_runs_own_connect_is_refused_by_the_kernel";
        if !namespaced(test) {
            return;
        }
        let abi = crate::landlock::abi().unwrap_or(0);
        if crate::landlock::handled_net(abi) == 0 {
            let line = format!(
                "{}sandbox::keeper::tests::{test} — this kernel's Landlock is ABI {abi}: TCP rights came with 4\n",
                crate::test_results::SELF_SKIP
            );
            let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
            return;
        }
        let mut rules = super::super::linux::ruleset_for(RunNet::Routed, abi).unwrap();
        rules.allow(Path::new("/"), crate::landlock::ALL).unwrap();
        let k = keeper("routed-kernel");
        let said = helper_through(&k, rules.fd(), RunNet::Own, "loopback", &[], None);
        assert!(said.starts_with(&format!("own={} ", libc::EACCES)), "{said}");
        k.end();
    }

    /// The families a session's `lo` mirrors are what glibc's `AI_ADDRCONFIG` counts: IPv4 but loopback, IPv6 but
    /// loopback and link-local (a unique-local one counts).
    #[test]
    fn the_families_are_what_glibc_counts() {
        let of = |addrs: &[&str]| {
            let addrs: Vec<std::net::IpAddr> = addrs.iter().map(|a| a.parse().unwrap()).collect();
            let f = Families::of(&addrs);
            (f.v4, f.v6)
        };
        assert_eq!(of(&["127.0.0.1", "::1", "fe80::1"]), (false, false), "loopback and link-local only");
        assert_eq!(of(&["127.0.0.1", "172.17.0.2", "::1", "fe80::42"]), (true, false), "a container's");
        assert_eq!(of(&["::1", "2600:1f18::5"]), (false, true), "IPv6 only");
        assert_eq!(of(&["10.0.0.2", "fd00::1"]), (true, true), "unique-local counts");
        assert_eq!(of(&[]), (false, false));
    }

    /// M5: an IPv4-only lookup with `AI_ADDRCONFIG` inside the session's netns finds an address — where tarae's
    /// own network has IPv4, `lo` carries `SESSION_V4` beside loopback, so glibc reads "IPv4 here". This machine's
    /// own name, from `/etc/hosts`: where this process finds it that way, a run does too.
    #[test]
    fn an_ipv4_lookup_finds_an_address_in_the_session() {
        if !namespaced("an_ipv4_lookup_finds_an_address_in_the_session") {
            return;
        }
        let Some(rules) = open_rules("an_ipv4_lookup_finds_an_address_in_the_session") else { return };
        assert!(Families::here().v4, "this machine has IPv4 beyond loopback");
        let k = keeper("lookup4");
        assert!(k.ns.netns(), "{}", k.said());
        let said = helper_through(&k, rules.fd(), RunNet::Own, "lookup4", &[], None);
        let (v4, lookup) = said.split_once(" lookup4=").unwrap_or_else(|| panic!("{said}"));
        let here = lookup4_own_name();
        match here.parse::<u32>().is_ok_and(|n| n > 0) {
            true => assert!(
                lookup.parse::<u32>().is_ok_and(|n| n > 0),
                "found here ({here}), not in the session: {said}"
            ),
            false => {
                let line = format!(
                    "{}sandbox::keeper::tests::an_ipv4_lookup_finds_an_address_in_the_session — this machine's \
                     name isn't found IPv4-only here ({here}): only lo's addresses checked\n",
                    crate::test_results::SELF_SKIP
                );
                let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
            }
        }
        assert_eq!(v4, format!("v4=127.0.0.1,{SESSION_V4}"), "lo's, and the session's own");
        k.end();
    }

    /// The IPv6 half: where tarae's network has IPv6 beyond loopback and link-local, `lo` carries `SESSION_V6`, and
    /// an IPv6-only lookup with `AI_ADDRCONFIG` (`ip6-localhost`, from `/etc/hosts`) answers inside; without it,
    /// glibc reads "no IPv6 here" and finds nothing. The family forced at the birth (`born_with`): these machines'
    /// containers have no IPv6 of their own; the choice from tarae's own addresses is `the_families_are_what_glibc_counts`.
    #[test]
    fn an_ipv6_lookup_finds_an_address_where_tarae_has_ipv6() {
        let test = "an_ipv6_lookup_finds_an_address_where_tarae_has_ipv6";
        if !namespaced(test) {
            return;
        }
        let Some(rules) = open_rules(test) else { return };
        let root = std::env::temp_dir().join(format!("tarae-keeper-{}-lookup6", std::process::id()));
        let mut said = Vec::new();
        for v6 in [true, false] {
            let k = Keeper::born_with(&root, None, ns(), Families { v4: true, v6 }).expect("a keeper starts");
            let env = [("TARAE_NAME", "ip6-localhost")];
            said.push(helper_through(&k, rules.fd(), RunNet::Own, "lookup6", &env, None));
            k.end();
        }
        assert_eq!(said[0], format!("v6={SESSION_V6},::1 lookup6=1"), "with IPv6 mirrored");
        assert!(said[1].starts_with("v6=::1 lookup6=getaddrinfo"), "without: {}", said[1]);
    }

    /// The probe finds tarae can put a socket of its own in a run's place (every kernel here is past 5.14) —
    /// where it serves at all.
    #[test]
    fn the_swap_is_probed() {
        if super::super::supervisor::unavailable().is_some() {
            return;
        }
        assert_eq!(super::super::supervisor::swap_unavailable(), None);
    }

    // ── step 6c: the session's network, made at its keeper's birth ─────────────────────────────────────────

    /// Whether this system's keepers are born with the session's network (a netns, and the probe's landed) — else
    /// says the test skips itself, and why. A machine said to give it (`TARAE_EXPECT_NETWORK=tproxy`) fails instead:
    /// a network broken there isn't a skip.
    fn network_here(test: &str) -> bool {
        if !namespaced(test) {
            return false;
        }
        match (&super::super::tier::get().redirect, ns().netns()) {
            (Ok(()), true) => true,
            (r, _) => {
                let why = r.as_ref().err().cloned().unwrap_or_else(|| "no network namespace".into());
                if std::env::var("TARAE_EXPECT_NETWORK").is_ok_and(|n| n == "tproxy") {
                    panic!("{test}: TPROXY expected here (TARAE_EXPECT_NETWORK), none — {why}");
                }
                let line = format!(
                    "{}sandbox::keeper::tests::{test} — no TPROXY here: {why}\n",
                    crate::test_results::SELF_SKIP
                );
                let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
                false
            }
        }
    }

    /// The session's network as tarae reads it back over its own sockets: nf_tables' generation, each chain's
    /// rule count, the routes and the policy rules (v4 and v6) — as text, to compare.
    fn network_state(k: &Keeper) -> String {
        use tarae_netlink::{Family, nft, route};
        let mut net = lock(&k.network);
        let made = net.as_mut().expect("its network");
        let by = || Instant::now() + Duration::from_secs(2);
        let generation = nft::generation(&mut made.nf, by()).unwrap();
        let rules = nft::rules(&mut made.nf, by()).unwrap();
        let count = |chain: &str| rules.iter().filter(|r| r.table == "tarae" && r.chain == chain).count();
        let mut said = format!("generation {generation}, out {}, pre {}", count("out"), count("pre"));
        for f in [Family::V4, Family::V6] {
            said += &format!("\n{f:?} routes {:?}", route::routes(&mut made.route, f, by()).unwrap());
            said += &format!("\n{f:?} rules {:?}", route::rules(&mut made.route, f, by()).unwrap());
        }
        said
    }

    /// A keeper is born with the session's network before any run: its owned table holds the session's ruleset
    /// (4 rules leaving and 5 coming in for both families, and 2 and 6 more for each its `lo` carries — 8 and 17
    /// with both: 6e's DNS over TCP one more each way, its own servers' two), its routes and policy rule are there, and
    /// a run's own connect
    /// beyond the session — on its own
    /// socket, no supervisor — lands on a planted socket, which refuses it until 6d (accepted, reset, counted).
    #[test]
    fn the_session_has_its_network_before_any_run() {
        let test = "the_session_has_its_network_before_any_run";
        if !network_here(test) {
            return;
        }
        let k = keeper("network");
        let here = Families::here();
        let carried = [here.v4 && !k.lo_missing.v4, here.v6 && !k.lo_missing.v6].map(usize::from);
        let (out, pre) = (4 + 2 * (carried[0] + carried[1]), 5 + 6 * (carried[0] + carried[1]));
        let state = network_state(&k);
        assert!(
            state.starts_with("generation ") && state.contains(&format!(", out {out}, pre {pre}")),
            "{state}"
        );
        assert!(state.contains("table: 100") && state.contains("fwmark: Some(1)"), "{state}");
        let (_, c, root) = project_served("network-own", vec![], Some(&k), &[], Err("not served".into()));
        assert_eq!(c.net, RunNet::Own);
        let said = helper_through(
            &k,
            c.rules_fd(),
            c.net,
            "within",
            &[("TARAE_TO", "203.0.113.7:443")],
            Some(&root),
        );
        assert_eq!(said, "reached reset", "taken to the planted socket, refused there");
        let taken = lock(&k.network).as_ref().map(|m| (m.taken.count(), m.taken.last()));
        assert_eq!(
            taken,
            Some((1, Some("203.0.113.7:443".parse().unwrap()))),
            "counted, its destination kept"
        );
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// A UDP datagram a run sends beyond the session lands on the planted UDP socket with where it was going
    /// (`RECVORIGDSTADDR`): what 6e answers DNS with. (IPv4: these machines' containers have no IPv6 of their own.)
    #[test]
    fn a_datagram_beyond_lands_with_its_original_destination() {
        let test = "a_datagram_beyond_lands_with_its_original_destination";
        if !network_here(test) {
            return;
        }
        let k = keeper("datagram");
        let (_, c, root) = project_served("datagram-own", vec![], Some(&k), &[], Err("not served".into()));
        let said = helper_through(
            &k,
            c.rules_fd(),
            c.net,
            "datagram",
            &[("TARAE_TO", "203.0.113.7:5000")],
            Some(&root),
        );
        assert_eq!(said, "datagram=ok");
        let seen = || lock(&k.network).as_ref().map(|m| (m.taken.count(), m.taken.last()));
        let by = Instant::now() + Duration::from_secs(1);
        while seen().is_some_and(|(n, _)| n == 0) && Instant::now() < by {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            seen(),
            Some((1, Some("203.0.113.7:5000".parse().unwrap()))),
            "taken, its destination read"
        );
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    // ── step 6d: accept, attribute, check ──────────────────────────────────────────────────────────────────

    /// A served run's policy in a session with the TPROXY network, `change`d: the network open to any host, this
    /// machine's own addresses none (the test's outside, on one of them, is beyond), run `run`.
    fn redirected_project(
        name: &str,
        k: &Keeper,
        run: u64,
        change: impl FnOnce(&mut super::super::Policy),
    ) -> (Arc<super::super::Policy>, super::super::linux::Confine, PathBuf) {
        let (p, c, root) = project_served(name, Vec::new(), Some(k), &[], Ok(()));
        let egress = super::super::proxy::egress::Egress { any: true, ..Default::default() };
        let mut p =
            super::super::Policy { network: true, own_addresses: Some(Vec::new()), egress, run_id: run, ..p };
        change(&mut p);
        (Arc::new(p), c, root)
    }

    /// P2 and P3 for TCP (6d): a served run's connect beyond the session goes on its own socket — its peer the
    /// address it named, never a pair's or a swap's — marked by its supervisor as its run's, and the planted socket
    /// hands it to the egress proxy, which reaches the host its request names; what the outside sees comes from
    /// tarae's network, not the session's. The run can neither mark its own socket nor clear the mark tarae gave
    /// (`EPERM`: M12), and that mark leaves bit 0 to the output chain (M12: with it, IPv4 took the destination as its
    /// source — this connect would fail). Nothing lands refused on the planted socket.
    #[test]
    fn a_runs_connection_beyond_reaches_its_host_through_the_planted_socket() {
        let test = "a_runs_connection_beyond_reaches_its_host_through_the_planted_socket";
        if !network_here(test) || !routed_here(test) {
            return;
        }
        let Some(ip) = outside() else { return };
        let (at, served) = answering(&format!("{ip}:0"), "out hello");
        super::super::proxy::TEST_NAMES.lock().unwrap().push(("planted-6d.test".into(), at));
        let k = keeper("planted-6d");
        let (p, c, root) = redirected_project("planted-6d", &k, 6_000_001, |_| {});
        let env = [("TARAE_TO", "203.0.113.7:80"), ("TARAE_HOST", "planted-6d.test")];
        let (said, refused) = served_said(&k, (&p, &c, &root), "origin", &env);
        let e = libc::EPERM;
        let expected = format!("peer=203.0.113.7:80 reply=out hello setmark={e} mark=even clear={e}");
        assert_eq!(said, expected, "{refused:?}");
        let peer = served.recv_timeout(Duration::from_secs(5)).expect("it arrived outside");
        assert_eq!(peer.ip(), std::net::IpAddr::V4(ip), "from tarae's network, not the session's");
        assert_eq!(lock(&k.network).as_ref().map(|m| m.taken.count()), Some(0), "nothing refused there");
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// A run's mark outlives it (what it left running connects as its run), but not what's kept for it (6d): once no
    /// process of the run is served, its refusals' sender is gone — the receiving end disconnected, so its
    /// `tarae-refused` thread (`sandbox::told`, `for d in rx`) ends, and its policy with it — while the session's
    /// marks still name the run. (A mark held the sender itself: every run's thread lived for the session's life.)
    #[test]
    fn a_runs_refusals_end_with_it_though_its_mark_is_kept() {
        let test = "a_runs_refusals_end_with_it_though_its_mark_is_kept";
        if !network_here(test) || !routed_here(test) {
            return;
        }
        let Some(ip) = outside() else { return };
        let (at, _served) = answering(&format!("{ip}:0"), "out hello");
        super::super::proxy::TEST_NAMES.lock().unwrap().push(("ended-6d.test".into(), at));
        let k = keeper("ended-6d");
        let run = 6_000_031;
        let (p, c, root) = redirected_project("ended-6d", &k, run, |_| {});
        let env = [("TARAE_TO", "203.0.113.7:80"), ("TARAE_HOST", "ended-6d.test")];
        let (mut lines, pid, refused) = served_helper(&k, (&p, &c, &root), "origin", &env);
        let said = said_next(&mut lines);
        lines.for_each(drop);
        crate::spawn::wait(pid).unwrap();
        assert!(said.contains("reply=out hello"), "{said}");
        let ended = refused.recv_timeout(Duration::from_secs(5));
        assert!(
            matches!(ended, Err(std::sync::mpsc::RecvTimeoutError::Disconnected)),
            "its sender gone with the run: {ended:?}"
        );
        let kept = lock(&k.network).as_ref().map(|m| m.marks.of_run(run).len());
        assert_eq!(kept, Some(1), "its mark kept");
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// A connection carrying the mark of a run that's over — nothing of it served, its refusals' sender gone — is
    /// reset on the planted socket and said in the log, never handed to the proxy: no one would hear its refusal, and
    /// no process of that run could have made it. Only tarae can mark a socket, so the test does what tarae's side
    /// can: it takes a socket an unserved run made in the session's network (`pidfd_getfd`, as the supervisor does),
    /// gives it the ended run's mark (`SO_MARK`, `CAP_NET_ADMIN` over the session's netns) and asks for a host that
    /// run's rules allow (any) — whose outside answers if the connection gets through.
    #[test]
    fn a_connection_marked_for_a_run_thats_over_is_reset() {
        use std::io::{BufRead, Write};
        let test = "a_connection_marked_for_a_run_thats_over_is_reset";
        if !network_here(test) || !routed_here(test) {
            return;
        }
        let Some(ip) = outside() else { return };
        let (own_at, _own) = answering(&format!("{ip}:0"), "out hello");
        let (late_at, late) = answering(&format!("{ip}:0"), "late hello");
        for (name, at) in [("over-6d.test", own_at), ("late-6d.test", late_at)] {
            super::super::proxy::TEST_NAMES.lock().unwrap().push((name.into(), at));
        }
        let captured = crate::log::capture();
        let k = keeper("over-6d");
        let run = 6_000_041;
        let (p, c, root) = redirected_project("over-6d", &k, run, |_| {});
        // (the run's own connection gives its mark; then the run ends, and its sender with it)
        let env = [("TARAE_TO", "203.0.113.7:80"), ("TARAE_HOST", "over-6d.test")];
        let (mut lines, pid, refused) = served_helper(&k, (&p, &c, &root), "origin", &env);
        let said = said_next(&mut lines);
        lines.for_each(drop);
        crate::spawn::wait(pid).unwrap();
        assert!(said.contains("reply=out hello"), "{said}");
        let ended = refused.recv_timeout(Duration::from_secs(5));
        assert!(matches!(ended, Err(std::sync::mpsc::RecvTimeoutError::Disconnected)), "{ended:?}");
        let marks = lock(&k.network).as_ref().map(|m| m.marks.of_run(run)).unwrap_or_default();
        let [(mark, _)] = marks[..] else { panic!("one mark for the run: {marks:?}") };
        // (a socket in the session's network, from a run tarae doesn't serve: unmarked, unconnected)
        let (_, own, own_root) =
            project_served("over-6d-own", vec![], Some(&k), &[], Err("not served".into()));
        let go = own_root.join("go");
        let cmd = helper_cmd("socket", &[("TARAE_GO", go.to_str().unwrap())], Some(&own_root));
        let s = k.start(&cmd, &crate::landlock::thread_name(), own.rules_fd(), own.net).unwrap();
        let held = s.spawned.pid;
        let mut out = io::BufReader::new(s.spawned.stdout.unwrap()).lines().map_while(Result::ok);
        let n: i32 =
            said_next(&mut out).strip_prefix("fd=").and_then(|n| n.parse().ok()).expect("its socket");
        // SAFETY: a plain call for a process of our own runs'.
        let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, held, 0) };
        assert!(pidfd >= 0, "pidfd_open: {}", io::Error::last_os_error());
        // SAFETY: the descriptor just opened, ours from here.
        let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd as i32) };
        let sock = super::super::supervisor::pidfd_getfd(&pidfd, n).expect("the run's socket taken");
        let sa = sockaddr("203.0.113.7:80");
        // SAFETY: an int, then an address of its size, on the socket we hold.
        let (set, connected) = unsafe {
            let set = libc::setsockopt(
                sock.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_MARK,
                (&raw const mark).cast(),
                4,
            );
            let len = size_of::<libc::sockaddr_in>() as libc::socklen_t;
            (set, libc::connect(sock.as_raw_fd(), (&raw const sa).cast(), len))
        };
        assert_eq!(
            (set, connected),
            (0, 0),
            "marked {mark:#x} and connected: {}",
            io::Error::last_os_error()
        );
        std::fs::write(&go, "").unwrap();
        crate::spawn::wait(held).unwrap();
        let s = std::net::TcpStream::from(sock);
        s.set_read_timeout(Some(Duration::from_secs(5))).ok();
        // (a reset that lands before the request is the write's error, and the read then sees the end)
        let wrote = (&s).write_all(b"GET / HTTP/1.1\r\nHost: late-6d.test\r\n\r\n");
        let mut reply = String::new();
        let got = match wrote.and_then(|()| io::BufReader::new(&s).read_line(&mut reply)) {
            Err(e) if e.kind() == io::ErrorKind::ConnectionReset => "reset".to_string(),
            Ok(0) => "closed".into(),
            Ok(_) => reply.trim().to_string(),
            Err(e) => e.to_string(),
        };
        assert_eq!(got, "reset", "the outside reached: {:?}", late.try_recv());
        assert!(late.try_recv().is_err(), "the outside not reached");
        let log = captured.lines().join("\n");
        let why = format!("run {run}'s ");
        assert!(log.contains(&why) && log.contains("the run is over and nothing of it is served"), "{log}");
        k.end();
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&own_root).ok();
    }

    /// Each question is its run's (6e, `SO_RCVMARK`): a served run's question to port 53 — the session's to answer —
    /// goes on its own socket, marked by its supervisor, and the DNS socket reads the mark back with it. Two runs of
    /// one session each ask for a TXT record (REFUSED: Q3), and each refusal is said in its own run's report, never
    /// the other's. On a kernel that reads no datagram's mark (before 5.19, D5 — here E5's stand-in, the sockets made
    /// without asking), the question is the session's: answered as one, said in no run's report, and the session's
    /// start says why once.
    #[test]
    fn each_question_is_its_runs() {
        let test = "each_question_is_its_runs";
        if !network_here(test) || !routed_here(test) {
            return;
        }
        let captured = crate::log::capture();
        // (run `run` asks for `txt-<run>.test`'s TXT record: REFUSED — what it said, and its report)
        let txt = |k: &Keeper, run: u64| {
            let p = redirected_project(&format!("question-6e-{run}"), k, run, |_| {});
            let name = format!("txt-{run}.test");
            let env = [("TARAE_TO", "203.0.113.7:53"), ("TARAE_NAME", name.as_str()), ("TARAE_TYPE", "16")];
            let said = served_said(k, (&p.0, &p.1, &p.2), "ask", &env);
            std::fs::remove_dir_all(&p.2).ok();
            said
        };
        let named = |report: &[super::super::Denial], run: u64| {
            report.iter().any(|d| d.target.starts_with(&format!("txt-{run}.test (TXT): ")))
        };
        let k = keeper("question-6e");
        let ((said1, report1), (said2, report2)) = (txt(&k, 6_000_051), txt(&k, 6_000_053));
        k.end();
        assert!(said1.starts_with("rcode=5 ") && said2.starts_with("rcode=5 "), "REFUSED: {said1}; {said2}");
        assert!(named(&report1, 6_000_051) && !named(&report1, 6_000_053), "its own: {report1:?}");
        assert!(named(&report2, 6_000_053) && !named(&report2, 6_000_051), "its own: {report2:?}");
        // (E5's stand-in: the planted UDP sockets made as before 5.19)
        let here = Families::here();
        let network = super::super::redirect::Network {
            no_rcvmark: true,
            ..super::super::redirect::Network::session(here.v4, here.v6)
        };
        let root = std::env::temp_dir().join(format!("tarae-keeper-{}-question-d5", std::process::id()));
        let given = Some(NetworkFor::Given(network));
        let k = Keeper::born_lo(&root, None, ns(), here, false, given, None).expect("a keeper starts");
        let (said, report) = txt(&k, 6_000_052);
        k.end();
        assert!(said.starts_with("rcode=5 "), "answered as the session's: {said}");
        assert!(!named(&report, 6_000_052), "no run's to say it in: {report:?}");
        let lines = captured.lines().join("\n");
        assert!(
            lines.contains("read no datagram's mark (Protocol not available"),
            "said at the start: {lines}"
        );
    }

    /// A name asked in the session reaches its host (6e): a tool's lookup (`getaddrinfo`, glibc, the resolver its
    /// `resolv.conf` names — whichever: every question to a port 53 is the session's) gets an address of the session's
    /// names, and its connection there names the host before any byte — the proxy reaches it by name, from tarae's
    /// network. The name is one the run's rules name, so tarae looked it up for real first (E1, the test's stand-in
    /// resolver).
    #[test]
    fn a_name_asked_in_the_session_reaches_its_host() {
        let test = "a_name_asked_in_the_session_reaches_its_host";
        if !network_here(test) || !routed_here(test) {
            return;
        }
        let Some(ip) = outside() else { return };
        let (at, served) = answering(&format!("{ip}:0"), "out hello");
        super::super::proxy::TEST_NAMES.lock().unwrap().push(("named-6e.test".into(), at));
        let k = keeper("named-6e");
        let host = super::super::proxy::egress::Pattern::parse("named-6e.test").unwrap();
        let only = |p: &mut super::super::Policy| {
            p.egress = super::super::proxy::egress::Egress { hosts: vec![host], ..Default::default() }
        };
        let (p, c, root) = redirected_project("named-6e", &k, 6_000_061, only);
        let (said, refused) = served_said(&k, (&p, &c, &root), "named", &[("TARAE_NAME", "named-6e.test")]);
        let addr: Option<std::net::IpAddr> =
            said.strip_prefix("addr=").and_then(|s| s.split(' ').next()?.parse().ok());
        assert!(
            addr.is_some_and(super::super::names::in_pool),
            "an address of the session's: {said} {refused:?}"
        );
        assert!(said.ends_with("reply=out hello"), "reached: {said}");
        let peer = served.recv_timeout(Duration::from_secs(5)).expect("it arrived outside");
        assert_eq!(peer.ip(), std::net::IpAddr::V4(ip), "from tarae's network");
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// 6e′: a run has no proxy URL exactly where its keeper is born with the session's network — the decision made
    /// before there's a keeper (`tier::redirects`, the probe's) is the keeper's own (`network_for`). A run's policy
    /// made on the real path (`sandbox::policy`: its keeper and Landlock rules made) with the network open: its
    /// keeper has the network, and its environment no proxy variable.
    #[test]
    fn a_run_has_no_proxy_variable_where_its_keeper_has_the_network() {
        let test = "a_run_has_no_proxy_variable_where_its_keeper_has_the_network";
        if !network_here(test) {
            return;
        }
        let root = std::env::temp_dir().join(format!("tarae-keeper-{}-unproxied-6e2", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let llm = crate::llm::LlmConfig { sandbox_network: true, ..Default::default() };
        let p = super::super::policy(&root, &llm, &[]).unwrap();
        let Some(Ok(c)) = &p.confine else {
            panic!("no sandbox: {:?}", p.confine.as_ref().map(|r| r.as_ref().err()))
        };
        let k = c.keeper.clone().expect("its keeper");
        let has = lock(&k.network).is_some();
        let cmd = super::super::command(Some(&p), "true", &[]);
        let vars: Vec<String> = cmd
            .get_envs()
            .filter(|(n, v)| v.is_some() && n.to_string_lossy().to_ascii_lowercase().contains("proxy"))
            .map(|(n, _)| n.to_string_lossy().into_owned())
            .collect();
        k.end();
        std::fs::remove_dir_all(&root).ok();
        assert!(has, "its keeper has the session's network");
        assert!(p.redirected && p.proxy_url().is_none() && vars.is_empty(), "{} {vars:?}", p.redirected);
    }

    /// An answer comes from the address and port the question went to (6e) — what a connected UDP socket takes, and
    /// nothing else: the loopback stub (`127.0.0.53`), GCP's (`169.254.169.254`), a LAN's (`10.0.0.2`), each answered
    /// from the session's names by one DNS socket (`IP_PKTINFO`'s address, its port 53); and over TCP to the stub
    /// (R6e's rules take it before loopback's return). None of them is reached: they're documentation stand-ins, and
    /// the answers are the session's own addresses.
    #[test]
    fn an_answer_comes_from_the_address_asked() {
        let test = "an_answer_comes_from_the_address_asked";
        if !network_here(test) || !routed_here(test) {
            return;
        }
        let k = keeper("asked-6e");
        // (rules naming no host: the name gets its address unlooked-up — E1's lookups are another test's)
        let none = |p: &mut super::super::Policy| p.egress = super::super::proxy::egress::Egress::default();
        let (p, c, root) = redirected_project("asked-6e", &k, 6_000_071, none);
        let ask = |to: &str, tcp: bool| {
            let mut env = vec![("TARAE_TO", to), ("TARAE_NAME", "asked-6e.test")];
            if tcp {
                env.push(("TARAE_TCP", "1"));
            }
            served_said(&k, (&p, &c, &root), "ask", &env).0
        };
        let given = lock(&k.network).as_ref().unwrap().names.names.give("asked-6e.test").v4;
        let want = format!("rcode=0 answers=1 addr={given}");
        let captured = crate::log::capture();
        for to in ["127.0.0.53:53", "169.254.169.254:53", "10.0.0.2:53"] {
            let got = ask(to, false);
            let said = |l: &String| l.contains("names") || l.contains("supervisor") || l.contains("planted");
            let log: Vec<String> = captured.lines().into_iter().filter(said).collect();
            assert_eq!(got, want, "UDP to {to}: {log:#?}");
        }
        assert_eq!(ask("127.0.0.53:53", true), want, "TCP to the stub");
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// E6 (b): under `ndots:5` with a search domain, a tool asks the search list's candidate first
    /// (`github.com.<search>`) — not named by the rules, so NXDOMAIN without a lookup — and then the name as written:
    /// `github.com` reaches github.com, never `github.com.<search>`. The run asks as glibc does under `ndots:5`
    /// (`searched`: M7 saw every glibc client ask so); tarae reads the search domain from its network's `search` (as
    /// `resolv.conf` gives both in a real session). (The run's glibc took neither `LOCALDOMAIN` nor `RES_OPTIONS`
    /// from its environment here — set, not in secure mode — so the order is the helper's, said plainly.)
    #[test]
    fn under_ndots_a_name_as_written_wins_over_its_search_candidate() {
        let test = "under_ndots_a_name_as_written_wins_over_its_search_candidate";
        if !network_here(test) || !routed_here(test) {
            return;
        }
        let Some(ip) = outside() else { return };
        let (at, _served) = answering(&format!("{ip}:0"), "out hello");
        super::super::proxy::TEST_NAMES.lock().unwrap().push(("github.com".into(), at));
        let here = Families::here();
        let network = super::super::redirect::Network {
            search: vec!["other-6e.example".into()],
            ..super::super::redirect::Network::session(here.v4, here.v6)
        };
        let root = std::env::temp_dir().join(format!("tarae-keeper-{}-ndots-6e", std::process::id()));
        let given = Some(NetworkFor::Given(network));
        let k = Keeper::born_lo(&root, None, ns(), here, false, given, None).expect("a keeper starts");
        let host = super::super::proxy::egress::Pattern::parse("github.com").unwrap();
        let only = |p: &mut super::super::Policy| {
            p.egress = super::super::proxy::egress::Egress { hosts: vec![host], ..Default::default() }
        };
        let (p, c, project) = redirected_project("ndots-6e", &k, 6_000_081, only);
        let env =
            [("TARAE_TO", "10.0.0.2:53"), ("TARAE_NAME", "github.com"), ("TARAE_SEARCH", "other-6e.example")];
        let (said, refused) = served_said(&k, (&p, &c, &project), "searched", &env);
        assert!(said.starts_with("candidate=3 "), "the candidate NXDOMAIN: {said} {refused:?}");
        assert!(said.ends_with("reply=out hello"), "github.com reached: {said} {refused:?}");
        let names = lock(&k.network).as_ref().unwrap().names.clone();
        let ip_said: Option<std::net::IpAddr> =
            said.split(" addr=").nth(1).and_then(|s| s.split(' ').next()?.parse().ok());
        assert_eq!(
            ip_said.and_then(|ip| names.names.name_of(ip)).as_deref(),
            Some("github.com"),
            "its address github.com's"
        );
        k.end();
        std::fs::remove_dir_all(&project).ok();
    }

    /// Where the session answers names, a server of the session's own and a port the user opened still come first
    /// (6e review): (a) a run serving DNS at the session's `127.0.0.1:53` — its bind is the supervisor's, tarae's —
    /// answers another run's question there, never tarae (prerouting delivers a question a session's own socket is
    /// bound for to it; the route leaves it the session's); (b) a run whose policy opens port 53 reaches this
    /// machine's own server at `127.0.0.1:53` (the test's, in its container) as before 6e. Each answer is one only
    /// that server gives: NOTIMP (4) from the run's, SERVFAIL (2) from this machine's; tarae's would be NOERROR.
    #[test]
    fn a_sessions_own_server_and_an_opened_port_53_come_first() {
        let test = "a_sessions_own_server_and_an_opened_port_53_come_first";
        if !network_here(test) || !routed_here(test) {
            return;
        }
        let k = keeper("own-dns-6e");
        let (p, c, root) = redirected_project("own-dns-6e", &k, 6_000_091, |_| {});
        let (mut lines, pid, refused) = served_helper(&k, (&p, &c, &root), "dnsserver", &[]);
        assert_eq!(
            said_next(&mut lines),
            "ready",
            "its bind at 53: {:?}",
            refused.try_iter().collect::<Vec<_>>()
        );
        let env = [("TARAE_TO", "127.0.0.1:53"), ("TARAE_NAME", "own-6e.test")];
        let (asked, _) = served_said(&k, (&p, &c, &root), "ask", &env);
        assert_eq!(asked, "rcode=4 answers=0 addr=none", "the run's own server answered");
        assert_eq!(said_next(&mut lines), "answered=ok");
        lines.for_each(drop);
        crate::spawn::wait(pid).unwrap();
        // (b) this machine's own server, the port opened
        let this_machine = match std::net::UdpSocket::bind("127.0.0.1:53") {
            Ok(s) => s,
            Err(e) => {
                let line = format!(
                    "{}sandbox::keeper::tests::{test} — (b) left out: this machine's 127.0.0.1:53 can't be bound \
                     here ({e})\n",
                    crate::test_results::SELF_SKIP
                );
                let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
                k.end();
                return;
            }
        };
        std::thread::spawn(move || {
            let mut b = [0u8; 512];
            if let Ok((n, from)) = this_machine.recv_from(&mut b) {
                let mut a = b[..n].to_vec();
                if a.len() >= 4 {
                    (a[2], a[3]) = (0x80 | (a[2] & 0x79), 0x82);
                }
                let _ = this_machine.send_to(&a, from);
            }
        });
        let opened = |p: &mut super::super::Policy| p.ports = vec![53];
        let (p, c, root2) = redirected_project("opened-dns-6e", &k, 6_000_092, opened);
        let (asked, _) = served_said(&k, (&p, &c, &root2), "ask", &env);
        assert_eq!(asked, "rcode=2 answers=0 addr=none", "this machine's own server answered");
        k.end();
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&root2).ok();
    }

    /// E2: a question with no mark tarae gave — from a run tarae doesn't serve — is answered REFUSED at once (its
    /// lookup ends as fast as a refused one) and said in the log, never answered as anyone's.
    #[test]
    fn an_unmarked_question_is_refused() {
        let test = "an_unmarked_question_is_refused";
        if !network_here(test) || !routed_here(test) {
            return;
        }
        let captured = crate::log::capture();
        let k = keeper("unmarked-q-6e");
        let (_, c, root) = project_served("unmarked-q-6e", vec![], Some(&k), &[], Err("not served".into()));
        let env = [("TARAE_TO", "203.0.113.53:53"), ("TARAE_NAME", "unmarked-6e.test")];
        let said = helper_through(&k, c.rules_fd(), c.net, "ask", &env, Some(&root));
        assert_eq!(said, "rcode=5 answers=0 addr=none", "REFUSED");
        let lines = captured.lines().join("\n");
        assert!(
            lines.contains("a question for 203.0.113.53:53 from ") && lines.contains("answered REFUSED"),
            "{lines}"
        );
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// Each connection is its own run's (6d): two runs of one session at once, of one program, each connecting
    /// beyond — one to a host the user refused for the session, one to a host its own rules name — and each is
    /// judged by its own run's rules, its refusal in its own run's report, never the other's: the planted socket
    /// tells them apart by the mark each run's supervisor gave.
    #[test]
    fn each_connection_is_its_runs() {
        use super::super::proxy::{self, Scope, ToProxy};
        let test = "each_connection_is_its_runs";
        if !network_here(test) || !routed_here(test) {
            return;
        }
        let Some(ip) = outside() else { return };
        let (at, _served) = answering(&format!("{ip}:0"), "out hello");
        proxy::TEST_NAMES.lock().unwrap().push(("reached-6d.test".into(), at));
        let k = keeper("runs-6d");
        let (refused_run, reached_run) = (6_000_011, 6_000_012);
        // (each run its own rules — the one naming no host, the other naming the host it reaches — so a connection
        // taken for the other run's meets rules that aren't its own; the refused host is the user's answer, below)
        let none = |p: &mut super::super::Policy| p.egress = proxy::egress::Egress::default();
        let (p1, c, root) = redirected_project("runs-6d", &k, refused_run, none);
        let host = proxy::egress::Pattern::parse("reached-6d.test").unwrap();
        let egress = proxy::egress::Egress { hosts: vec![host], ..Default::default() };
        let p2 = Arc::new(super::super::Policy { run_id: reached_run, egress, ..(*p1).clone() });
        let session = proxy::session_of(&p1.root);
        let (host, port, scope) = ("refused-6d.test".to_string(), 80, Scope::Refused);
        proxy::get().send(ToProxy::Answer { session, run: refused_run, host, port, scope });
        let to = |host: &'static str| [("TARAE_TO", "203.0.113.7:80"), ("TARAE_HOST", host)];
        let (mut one, first, refused1) =
            served_helper(&k, (&p1, &c, &root), "origin", &to("refused-6d.test"));
        let (mut two, second, refused2) =
            served_helper(&k, (&p2, &c, &root), "origin", &to("reached-6d.test"));
        let (said1, said2) = (said_next(&mut one), said_next(&mut two));
        one.for_each(drop);
        two.for_each(drop);
        crate::spawn::wait(first).unwrap();
        crate::spawn::wait(second).unwrap();
        let (refused1, refused2): (Vec<_>, Vec<_>) =
            (refused1.try_iter().collect(), refused2.try_iter().collect());
        assert!(said1.contains("reply=HTTP/1.1 403 Forbidden"), "{said1}");
        assert!(said2.contains("reply=out hello"), "{said2}");
        assert!(
            refused1.iter().any(|d| d.op == proxy::HOST_OP && d.target.starts_with("refused-6d.test:80: ")),
            "the refused run's own report: {refused1:?}"
        );
        assert!(refused2.is_empty(), "nothing in the other run's: {refused2:?}");
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// What lands on the planted socket but isn't a run's connection tarae marked is refused there — reset, and said
    /// in the log with why: a connection a served run made to the planted address itself (the output chain leaves the
    /// session's own addresses alone: it reaches the listener as any local one), and one from a run tarae doesn't
    /// serve, its socket never marked by a supervisor — even with a served run's mark given in the session before.
    /// (A session each: a planted socket says its first refusal and counts the rest — `SAID_EVERY`.)
    #[test]
    fn what_tarae_didnt_mark_is_refused_on_the_planted_socket() {
        let test = "what_tarae_didnt_mark_is_refused_on_the_planted_socket";
        if !network_here(test) || !routed_here(test) {
            return;
        }
        let captured = crate::log::capture();
        let k = keeper("itself-6d");
        let (p, c, root) = redirected_project("itself-6d", &k, 6_000_021, |_| {});
        let planted = SESSION_V4.to_string() + ":15001";
        let (said, _) = served_said(&k, (&p, &c, &root), "within", &[("TARAE_TO", &planted)]);
        assert_eq!(said, "reached reset", "the planted address itself");
        k.end();
        std::fs::remove_dir_all(&root).ok();
        let k = keeper("unmarked-6d");
        let (p, c, root) = redirected_project("unmarked-6d", &k, 6_000_022, |_| {});
        // (a served run's mark given in the session first: an unmarked connection still finds no owner)
        let env = [("TARAE_TO", "203.0.113.7:80"), ("TARAE_HOST", "nowhere-6d.test")];
        served_said(&k, (&p, &c, &root), "origin", &env);
        let (_, own, own_root) =
            project_served("unmarked-6d-own", vec![], Some(&k), &[], Err("not served".into()));
        let said = helper_through(
            &k,
            own.rules_fd(),
            own.net,
            "within",
            &[("TARAE_TO", "203.0.113.7:443")],
            Some(&own_root),
        );
        assert_eq!(said, "reached reset", "a run tarae doesn't serve");
        let lines = captured.lines().join("\n");
        assert!(lines.contains("a connection to the planted socket itself; reset"), "{lines}");
        assert!(lines.contains("for 203.0.113.7:443 (its mark 0x1 isn't one tarae gave"), "{lines}");
        k.end();
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&own_root).ok();
    }

    // ── step 6g: a cgroup per run ──────────────────────────────────────────────────────────────────────────

    /// Set first thing in a run's child (`child`): from then on the test binary's allocator ends that process
    /// with `ALLOCATED` at any allocation or free, before it touches malloc's locks.
    pub(super) static NO_ALLOC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    /// The exit status of a run's child that allocated before its exec.
    const ALLOCATED: i32 = 121;

    /// The test binary's allocator: the system's, watched — `NO_ALLOC` set, any call ends the process.
    struct Watched;

    impl Watched {
        fn check() {
            if NO_ALLOC.load(std::sync::atomic::Ordering::Relaxed) {
                // SAFETY: the end of this process (a run's child), nothing allocated.
                unsafe { libc::_exit(ALLOCATED) }
            }
        }
    }

    // SAFETY: the system's allocator, a check before each call.
    unsafe impl std::alloc::GlobalAlloc for Watched {
        unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
            Watched::check();
            // SAFETY: as the caller's contract.
            unsafe { std::alloc::System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
            Watched::check();
            // SAFETY: as the caller's contract.
            unsafe { std::alloc::System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
            Watched::check();
            // SAFETY: as the caller's contract.
            unsafe { std::alloc::System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, new: usize) -> *mut u8 {
            Watched::check();
            // SAFETY: as the caller's contract.
            unsafe { std::alloc::System.realloc(ptr, layout, new) }
        }
    }

    #[global_allocator]
    static ALLOC: Watched = Watched;

    /// The review's first finding: a run's child allocates and frees nothing between the keeper's `clone3` (or fork)
    /// and its exec — a raw `clone3` doesn't reset glibc's malloc locks, so in a keeper with another thread (a test's)
    /// an allocation there could wait for good. Shown deterministically rather than by catching a thread holding
    /// malloc's lock across the clone (a race a test can only hope to hit): the test binary's allocator ends a child
    /// that allocates (`NO_ALLOC`, exit 121) — every run of every test here would show it; this one goes through the
    /// way out too (a folder that isn't there: `fail`'s words, made of fixed bytes and the errno's digits), through
    /// `clone3` where the keeper has a scope (`--cgroup`), else the fork.
    #[test]
    fn a_runs_child_allocates_nothing_before_its_exec() {
        let Some(rules) = open_rules("a_runs_child_allocates_nothing_before_its_exec") else { return };
        let k = keeper("noalloc");
        let (out, status) = sh_through(&k, &rules, "echo ran");
        assert_eq!((status.code(), out.as_str()), (Some(0), "ran\n"), "the child allocated: {ALLOCATED}?");
        let missing = std::env::temp_dir().join(format!("tarae-noalloc-{}-missing", std::process::id()));
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "true"]).current_dir(&missing);
        let s = k.start(&cmd, &crate::landlock::thread_name(), rules.fd(), RunNet::Plain).unwrap();
        let mut err = String::new();
        io::Read::read_to_string(&mut s.spawned.stderr.unwrap(), &mut err).unwrap();
        let status = crate::spawn::wait(s.spawned.pid).unwrap();
        assert_eq!(status.code(), Some(127), "its way out, not an allocation ({ALLOCATED}): {err}");
        let said = format!("tarae's sandbox couldn't start it — its folder: errno {}\n", libc::ENOENT);
        assert_eq!(err, said);
        let way = if matches!(k.runs_in, Some(Ok(_))) { "clone3 into its cgroup" } else { "fork" };
        let line = format!("a run's child, by {way}: nothing allocated before its exec\n");
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
        k.end();
    }

    /// Whether this system's keepers get a scope of their own, each run a cgroup of its own (6g: with the session's
    /// network, where `tier::Probed.cgroups` says so — in tests only a folder `cargo xtask linux --cgroup` gives) —
    /// else says the test skips itself, and why. A machine said to give them (`TARAE_EXPECT_CGROUP`, which `--cgroup`
    /// sets) fails instead.
    fn cgroups_here(test: &str) -> bool {
        if !network_here(test) {
            return false;
        }
        match &super::super::tier::get().cgroups {
            Ok(_) => true,
            Err(why) => {
                if std::env::var_os("TARAE_EXPECT_CGROUP").is_some() {
                    panic!("{test}: a cgroup per run expected here (TARAE_EXPECT_CGROUP), none — {why}");
                }
                let line = format!(
                    "{}sandbox::keeper::tests::{test} — no cgroup per run here: {why}\n",
                    crate::test_results::SELF_SKIP
                );
                let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
                false
            }
        }
    }

    /// `k`'s scope, as it must have one here.
    fn scope_of(k: &Keeper) -> RunsIn {
        match &k.runs_in {
            Some(Ok(r)) => r.clone(),
            other => panic!("a keeper without its scope: {other:?}"),
        }
    }

    /// A run's own cgroup's folder, as tarae sees the hierarchy.
    fn folder(cgroup: &str) -> PathBuf {
        Path::new("/sys/fs/cgroup").join(cgroup.trim_start_matches('/'))
    }

    /// 6g: each run starts in a cgroup of its own right below its keeper's scope — not the keeper's, not another
    /// run's — and what it forks is in its run's (a child inherits it: what a run leaves stays its run's, as 6d's
    /// rules will see it — spike6 Q2). `Started` says that cgroup's id: its folder's inode, what a rule compares.
    #[test]
    fn each_run_is_in_its_own_cgroup() {
        use std::io::BufRead;
        use std::os::unix::fs::MetadataExt;
        let test = "each_run_is_in_its_own_cgroup";
        if !cgroups_here(test) {
            return;
        }
        let Some(rules) = open_rules(test) else { return };
        let k = keeper("cgroups");
        let scope = scope_of(&k).scope;
        assert_eq!(super::super::cgroup::cgroup_of(k.pid), Ok(scope.clone()), "the keeper is in its scope");
        // (the run's shell's cgroup and its child's — sed's own — said, then it waits in its cgroup)
        let script = "sed -n 's/^0:://p' /proc/$$/cgroup /proc/self/cgroup; exec sleep 30";
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", script]);
        let runs: Vec<(Started, Vec<String>)> = (0..2)
            .map(|_| {
                let mut s =
                    k.start(&cmd, &crate::landlock::thread_name(), rules.fd(), RunNet::Plain).unwrap();
                let lines = std::io::BufReader::new(s.spawned.stdout.take().unwrap());
                let said = lines.lines().take(2).map(Result::unwrap).collect();
                (s, said)
            })
            .collect();
        let own: Vec<&String> = runs.iter().map(|(_, said)| &said[0]).collect();
        assert_ne!(own[0], own[1], "each its own");
        for (s, said) in &runs {
            assert!(said[0].starts_with(&format!("{scope}/run-")), "right below the scope: {said:?}");
            assert_eq!(said[1], said[0], "its child in its cgroup");
            let ino = std::fs::metadata(folder(&said[0])).unwrap().ino();
            assert_eq!(s.cgroup, Some(ino), "Started says its id");
        }
        for (s, _) in runs {
            crate::dap::term_group(s.spawned.pid);
            crate::spawn::wait(s.spawned.pid).unwrap();
        }
        k.end();
    }

    /// 6g, the real path (by hand — `target/sb/profile6h/m11-live.sh`: a machine with the user's systemd manager, as
    /// the user, outside a container — whose cgroup the manager couldn't move a keeper out of): a keeper is put in a
    /// scope of its own by the user's manager over the user bus (`StartTransientUnit`, `Delegate=yes`) — under
    /// `user@<uid>.service`, the user's — its run starts in a cgroup of its own right below it, and the scope is gone
    /// once the keeper is (`CollectMode=inactive-or-failed`). In the session's user and mount namespaces (no network,
    /// no PID one): a scope goes only to a keeper whose birth made `/sys/fs/cgroup` read-only in a mount namespace of
    /// its own — so on Ubuntu with its restriction, only as `/usr/bin/tarae` under tarae's profile. How long the birth
    /// took, said.
    #[test]
    #[ignore = "the user's own systemd manager: by hand on a VM (target/sb/profile6h/m11-live.sh)"]
    fn a_keeper_gets_its_own_delegated_scope() {
        use std::os::unix::fs::MetadataExt;
        let bus = super::super::cgroup::user_bus(
            std::env::var("DBUS_SESSION_BUS_ADDRESS").ok().as_deref(),
            std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).as_deref(),
            |p| p.exists(),
        )
        .expect("a user bus");
        let Some(rules) = open_rules("a_keeper_gets_its_own_delegated_scope") else { return };
        let root = std::env::temp_dir().join(format!("tarae-keeper-{}-scope", std::process::id()));
        let source = super::super::cgroup::Source::UserBus(bus);
        let t0 = Instant::now();
        let k = Keeper::born_lo(
            &root,
            None,
            Ns::session(false, false),
            Families::here(),
            false,
            None,
            Some(source),
        )
        .expect("a keeper starts");
        let born = t0.elapsed();
        let scope = scope_of(&k).scope;
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        assert!(scope.contains(&format!("/user@{uid}.service/")) && scope.ends_with(".scope"), "{scope}");
        assert_eq!(super::super::cgroup::cgroup_of(k.pid), Ok(scope.clone()), "the keeper is in it");
        assert_eq!(std::fs::metadata(folder(&scope)).unwrap().uid(), uid, "delegated: the user's");
        let (out, status) = sh_through(&k, &rules, "sed -n 's/^0:://p' /proc/self/cgroup");
        assert!(status.success(), "{out}");
        assert_eq!(out.trim(), format!("{scope}/run-1"), "its run in a cgroup of its own");
        k.end();
        assert!(gone_soon(&folder(&scope)), "the scope went with its keeper");
        let line = format!("a keeper in {scope}: born in {} ms with its scope\n", born.as_millis());
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
    }

    /// The folder the tests' scopes go in (`TARAE_TEST_CGROUP`), where this system gives a keeper the session's
    /// namespaces too — a scope goes only to one with a mount namespace of its own — else says the test skips itself,
    /// and why (`TARAE_EXPECT_CGROUP`: fails instead).
    fn given_folder(test: &str) -> Option<PathBuf> {
        if !namespaced(test) {
            return None;
        }
        match std::env::var_os("TARAE_TEST_CGROUP") {
            Some(folder) => Some(folder.into()),
            None if std::env::var_os("TARAE_EXPECT_CGROUP").is_some() => {
                panic!("{test}: a scope's folder expected (TARAE_EXPECT_CGROUP)")
            }
            None => {
                let line = format!(
                    "{}sandbox::keeper::tests::{test} — no folder for a scope (TARAE_TEST_CGROUP)\n",
                    crate::test_results::SELF_SKIP
                );
                let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
                None
            }
        }
    }

    /// The review's second pass, its first finding: a birth whose read-only step fails (here told to, `EPERM`) is a
    /// birth all the same — the step is 6g's second lock, not the keeper's — the keeper there, with no scope (its runs
    /// in its own cgroup), why said on its line. Before, it failed every keeper's birth, 6g or not.
    #[test]
    fn a_birth_goes_on_where_cgroupfs_cant_be_made_read_only() {
        let test = "a_birth_goes_on_where_cgroupfs_cant_be_made_read_only";
        let Some(given) = given_folder(test) else { return };
        let Some(rules) = open_rules(test) else { return };
        let root = std::env::temp_dir().join(format!("tarae-keeper-{}-ro-fails", std::process::id()));
        lock(&CGROUPFS_SEAMS).push((root.clone(), CgroupfsSeam::Fail(libc::EPERM)));
        let captured = crate::log::capture();
        let source = super::super::cgroup::Source::Given(given.clone());
        let born = Keeper::born_lo(&root, None, ns(), Families::here(), false, None, Some(source));
        lock(&CGROUPFS_SEAMS).retain(|(r, _)| *r != root);
        let k = born.expect("born all the same");
        let why = match &k.runs_in {
            Some(Err(why)) => why.clone(),
            other => panic!("a scope where /sys/fs/cgroup wasn't read-only: {other:?}"),
        };
        let eperm = io::Error::from_raw_os_error(libc::EPERM);
        assert!(why.contains(&format!("couldn't be made read-only there: {eperm}")), "{why}");
        assert!(!given.join(format!("keeper-{}", k.pid)).exists(), "never put in one");
        let (out, _) = sh_through(&k, &rules, "sed -n 's/^0:://p' /proc/self/cgroup");
        let keepers = super::super::cgroup::cgroup_of(k.pid).unwrap();
        assert_eq!(out.trim(), keepers, "its run in its keeper's cgroup");
        let said = format!("a keeper for {}: pid {}", root.display(), k.pid);
        let line = captured.lines().into_iter().find(|l| l.contains(&said)).expect("its line");
        assert!(line.contains("runs in its cgroup — no scope of its own: ") && line.contains(&why), "{line}");
        k.end();
    }

    /// The review's second pass, its third finding: a scope goes only to a keeper whose birth made `/sys/fs/cgroup`
    /// read-only in a mount namespace of its own. One born without namespaces (tier C's) has none to do it in: its runs
    /// could move themselves between the scope's cgroups, all the user's — so it gets no scope (never even asked for
    /// one), said; its runs in its own cgroup.
    #[test]
    fn a_scope_goes_only_to_a_keeper_whose_cgroupfs_is_read_only() {
        let test = "a_scope_goes_only_to_a_keeper_whose_cgroupfs_is_read_only";
        let Some(given) = given_folder(test) else { return };
        let root = std::env::temp_dir().join(format!("tarae-keeper-{}-no-mntns", std::process::id()));
        let source = super::super::cgroup::Source::Given(given.clone());
        let k = Keeper::born_lo(&root, None, Ns::NONE, Families::here(), false, None, Some(source))
            .expect("born");
        match &k.runs_in {
            Some(Err(why)) => assert!(why.contains("no mount namespace of its own"), "{why}"),
            other => panic!("a scope for a keeper without a mount namespace: {other:?}"),
        }
        assert!(!given.join(format!("keeper-{}", k.pid)).exists(), "never put in one");
        k.end();
    }

    /// The review's second pass, its second finding: without `mount_setattr` (answered `ENOSYS` — the pre-5.12 way,
    /// forced), the read-only step's bind remount keeps every flag the mount has locked — here a tmpfs over
    /// `/sys/fs/cgroup` mounted nosuid, nodev, noexec, noatime in the scene's own user and mount namespaces, so locked
    /// for the keeper's (a namespace below): a remount losing one is refused. In a process of its own
    /// (`cgroupfs_fallback_scene`); what a run of the keeper's sees: the mount read-only with its flags, a write
    /// `EROFS`.
    #[test]
    fn without_mount_setattr_the_remount_keeps_the_mounts_flags() {
        use std::os::unix::process::CommandExt;
        if !namespaced("without_mount_setattr_the_remount_keeps_the_mounts_flags") {
            return;
        }
        let mut cmd = scene_cmd("cgroupfs_fallback_scene", &[]);
        // SAFETY: plain calls with no arguments.
        let (u, g) = unsafe { (libc::geteuid(), libc::getegid()) };
        let maps: Vec<(&'static std::ffi::CStr, Vec<u8>)> = vec![
            (c"/proc/self/setgroups", b"deny".to_vec()),
            (c"/proc/self/uid_map", format!("{u} {u} 1").into_bytes()),
            (c"/proc/self/gid_map", format!("{g} {g} 1").into_bytes()),
        ];
        // SAFETY: in the forked child before its exec: raw calls only, on strings made before the fork.
        unsafe {
            cmd.pre_exec(move || {
                let null = std::ptr::null::<libc::c_char>();
                if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                    return Err(io::Error::last_os_error());
                }
                for (path, map) in &maps {
                    let fd = libc::open(path.as_ptr(), libc::O_WRONLY);
                    if fd < 0 || libc::write(fd, map.as_ptr().cast(), map.len()) != map.len() as isize {
                        return Err(io::Error::last_os_error());
                    }
                    libc::close(fd);
                }
                let flags = libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC | libc::MS_NOATIME;
                if libc::mount(null, c"/".as_ptr(), null, libc::MS_REC | libc::MS_PRIVATE, std::ptr::null())
                    != 0
                    || libc::mount(
                        c"tmpfs".as_ptr(),
                        c"/sys/fs/cgroup".as_ptr(),
                        c"tmpfs".as_ptr(),
                        flags,
                        std::ptr::null(),
                    ) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let want = "read-only=ok opts=ro,nosuid,nodev,noexec,noatime write=Read-only file system";
        assert_eq!(said_by("cgroupfs_fallback_scene", cmd), want);
    }

    /// `without_mount_setattr_the_remount_keeps_the_mounts_flags`' scene, in the user and mount namespaces its test
    /// made (`/sys/fs/cgroup` a tmpfs there): a keeper born in namespaces of its own with `mount_setattr` answered
    /// missing (`CgroupfsSeam::NoSetattr`), and a run of its: `scene: read-only=<ok or why> opts=<the mount's, as the
    /// run sees it> write=<a file made there>`.
    #[test]
    #[ignore = "run by without_mount_setattr_the_remount_keeps_the_mounts_flags in a process of its own"]
    fn cgroupfs_fallback_scene() {
        if std::env::var_os(SCENE).is_none() {
            return;
        }
        let rules = open_rules("cgroupfs_fallback_scene").expect("Landlock");
        let root = PathBuf::from("/tarae-keeper-cgroupfs-fallback");
        lock(&CGROUPFS_SEAMS).push((root.clone(), CgroupfsSeam::NoSetattr));
        let k = Keeper::born_lo(&root, None, Ns::session(false, false), Families::here(), false, None, None)
            .expect("born");
        let read_only = match &k.cgroupfs {
            Ok(()) => "ok".to_string(),
            Err(why) => why.clone(),
        };
        let script = "awk '$5 == \"/sys/fs/cgroup\" { print $6 }' /proc/self/mountinfo";
        let (opts, _) = sh_through(&k, &rules, script);
        // (the shell's own words for a file it can't make — `…: Read-only file system`, the errno's text)
        let (made, _) = sh_through(&k, &rules, "( : > /sys/fs/cgroup/made-by-a-run ) 2>&1 && echo ok");
        let write = made.trim().rsplit(": ").next().unwrap_or_default().to_string();
        k.end();
        // (the tmpfs is mounted over the hierarchy: the last line at that place is the mount the run sees)
        let top = opts.trim().lines().last().unwrap_or_default();
        println!("scene: read-only={read_only} opts={top} write={write}");
    }

    /// The keeper's lines in `captured` saying a run started in its keeper's cgroup: what kept it from its own.
    fn missed_said(captured: &crate::log::Capture, k: &Keeper) -> Vec<String> {
        let mark = format!("a run (keeper {}) started in its keeper's cgroup — ", k.pid);
        captured.lines().iter().filter_map(|l| l.split_once(&mark).map(|(_, why)| why.to_string())).collect()
    }

    /// The review's third finding, D5's "said, never refused": a run whose own cgroup can't be made (here the scope
    /// takes no more — `cgroup.max.descendants` 0, so `mkdirat` is `EAGAIN`) starts all the same, in its keeper's
    /// cgroup; `Started` has no id of its own and why is said.
    #[test]
    fn a_run_without_its_own_cgroup_starts_in_its_keepers_said() {
        let test = "a_run_without_its_own_cgroup_starts_in_its_keepers_said";
        if !cgroups_here(test) {
            return;
        }
        let Some(rules) = open_rules(test) else { return };
        let k = keeper("cgroup-full");
        let scope = scope_of(&k).scope;
        let max = folder(&scope).join("cgroup.max.descendants");
        std::fs::write(&max, "0").unwrap();
        let captured = crate::log::capture();
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "sed -n 's/^0:://p' /proc/self/cgroup"]);
        let started = k.start(&cmd, &crate::landlock::thread_name(), rules.fd(), RunNet::Plain);
        std::fs::write(&max, "max").unwrap();
        let s = started.expect("started all the same");
        assert_eq!(s.cgroup, None, "no cgroup of its own");
        let mut out = String::new();
        io::Read::read_to_string(&mut s.spawned.stdout.unwrap(), &mut out).unwrap();
        assert!(crate::spawn::wait(s.spawned.pid).unwrap().success());
        assert_eq!(out.trim(), scope, "in its keeper's: sed, its child, is");
        let why = io::Error::from_raw_os_error(libc::EAGAIN);
        assert_eq!(missed_said(&captured, &k), [format!("its own cgroup: {why}")]);
        k.end();
    }

    /// The review's third finding: where `clone3` answers `ENOSYS` (a filter's — the scene's own), a keeper with a
    /// scope says its first run started in its cgroup for that, and asks it no more — no folder made for nothing —
    /// each later run said not to have tried (`its own cgroup`, not `starting it`).
    #[test]
    fn clone3_missing_is_asked_once() {
        if given_folder("clone3_missing_is_asked_once").is_none() {
            return;
        }
        let no = io::Error::from_raw_os_error(libc::ENOSYS);
        let want = format!("run1=starting it: {no} run2=its own cgroup: {no} run3=its own cgroup: {no}");
        assert_eq!(scene("clone3_missing_scene", &[]), want);
    }

    /// `clone3_missing_is_asked_once`'s scene: this thread's `clone3` answered `ENOSYS` (a filter of its own — the
    /// whole process is the scene's, the keeper born from it inherits it), a keeper in the session's user and mount
    /// namespaces (where a scope may go: `/sys/fs/cgroup` read-only there) put in a scope of its own in
    /// `TARAE_TEST_CGROUP`, three runs. Prints `scene: run1=… run2=… run3=…`, what each said.
    #[test]
    #[ignore = "run by clone3_missing_is_asked_once in a process of its own"]
    fn clone3_missing_scene() {
        if std::env::var_os(SCENE).is_none() {
            return;
        }
        let given = std::env::var_os("TARAE_TEST_CGROUP").unwrap();
        let rules = open_rules("clone3_missing_scene").expect("Landlock");
        let op = |code: u32, k: u32, jt: u8, jf: u8| libc::sock_filter { code: code as u16, jt, jf, k };
        let ret = libc::BPF_RET | libc::BPF_K;
        let filter = [
            op(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0, 0, 0),
            op(libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K, libc::SYS_clone3 as u32, 0, 1),
            op(ret, libc::SECCOMP_RET_ERRNO | libc::ENOSYS as u32, 0, 0),
            op(ret, libc::SECCOMP_RET_ALLOW, 0, 0),
        ];
        let prog = libc::sock_fprog { len: filter.len() as u16, filter: filter.as_ptr().cast_mut() };
        // SAFETY: plain prctls on this thread; the program is live for the call.
        unsafe {
            assert_eq!(libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), 0);
            let r = libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &raw const prog);
            assert_eq!(r, 0, "{}", io::Error::last_os_error());
        }
        let captured = crate::log::capture();
        let root = Path::new("/tarae-keeper-clone3-missing");
        let source = super::super::cgroup::Source::Given(given.into());
        let ns = Ns::session(false, false);
        let k = Keeper::born_lo(root, None, ns, Families::here(), false, None, Some(source)).expect("born");
        for _ in 0..3 {
            let (_, status) = sh_through(&k, &rules, "true");
            assert!(status.success());
        }
        let said = missed_said(&captured, &k);
        k.end();
        let said: Vec<String> = said.iter().enumerate().map(|(i, w)| format!("run{}={w}", i + 1)).collect();
        println!("scene: {}", said.join(" "));
    }

    /// The review's third finding: a keeper takes its scope once, and only before any run — a second one sent (a
    /// folder elsewhere), or one sent after a run, is refused (said) and its runs never go there. Else a scope could
    /// be swapped under a session whose runs 6d's rules already name.
    #[test]
    fn a_scope_is_taken_once_and_before_any_run() {
        let test = "a_scope_is_taken_once_and_before_any_run";
        if !cgroups_here(test) {
            return;
        }
        let Some(rules) = open_rules(test) else { return };
        let given = std::env::var("TARAE_TEST_CGROUP").unwrap();
        let decoy = Path::new(&given).join(format!("decoy-{}", std::process::id()));
        std::fs::create_dir(&decoy).unwrap();
        let hand = |k: &Keeper| {
            let dir = std::fs::File::open(&decoy).unwrap();
            channel::send(k.chan.as_fd(), &Msg::Cgroup, &[dir.as_fd()]).unwrap();
        };
        let cgroup_of_a_run = |k: &Keeper| {
            let (out, status) = sh_through(k, &rules, "sed -n 's/^0:://p' /proc/self/cgroup");
            assert!(status.success(), "{out}");
            out.trim().to_string()
        };
        let captured = crate::log::capture();
        let refused = |k: &Keeper| {
            let said = format!("a run (keeper {}) failed at the request", k.pid);
            captured.lines().iter().filter(|l| l.contains(&said)).count()
        };
        // A second scope, before any run
        let k = keeper("scope-twice");
        let scope = scope_of(&k).scope;
        hand(&k);
        assert!(cgroup_of_a_run(&k).starts_with(&format!("{scope}/run-")), "its first scope");
        assert_eq!(refused(&k), 1, "the second refused, said");
        k.end();
        // A scope after a run
        let root = std::env::temp_dir().join(format!("tarae-keeper-{}-scope-late", std::process::id()));
        let k = Keeper::born_lo(&root, None, ns(), Families::here(), false, None, None).expect("born");
        let before = cgroup_of_a_run(&k);
        hand(&k);
        assert_eq!(cgroup_of_a_run(&k), before, "still its keeper's");
        assert_eq!(refused(&k), 1, "the late one refused, said");
        k.end();
        std::fs::remove_dir(&decoy).ok();
    }

    /// `cgroupwrites` as a run of `k`'s under `rules` (made for `net`), beside a sibling run waiting in its own
    /// cgroup: what it said. Anything it made in the scope (a write let through) is removed after.
    fn cgroup_writes(k: &Keeper, rules: BorrowedFd, net: RunNet, cwd: &Path) -> String {
        let scope = scope_of(k).scope;
        let mut wait = std::process::Command::new("sh");
        wait.args(["-c", "exec sleep 30"]);
        let sibling = k.start(&wait, &crate::landlock::thread_name(), rules, net).unwrap();
        let theirs = super::super::cgroup::cgroup_of(sibling.spawned.pid).unwrap();
        assert!(theirs.starts_with(&format!("{scope}/run-")), "a sibling in its own: {theirs}");
        let env = [("TARAE_SCOPE", scope.as_str()), ("TARAE_SIBLING", theirs.as_str())];
        let said = helper_through(k, rules, net, "cgroupwrites", &env, Some(cwd));
        crate::dap::term_group(sibling.spawned.pid);
        crate::spawn::wait(sibling.spawned.pid).unwrap();
        let _ = std::fs::remove_dir(folder(&scope).join("made-by-a-run"));
        said
    }

    /// The review's second finding: a run moves no process between cgroups and changes none — not into a sibling
    /// run's (6d's rules would count its connections that run's), not into its keeper's scope nor out of it (the
    /// session's default, or the user's own `app.slice`), not making one in the scope, not writing its own cgroup's
    /// files — though every one of them is the user's, the runs' own uid (`Delegate=yes`). With a run's own rules
    /// (the project's): each refused, said which way.
    #[test]
    fn a_run_moves_nothing_between_cgroups_nor_changes_one() {
        let test = "a_run_moves_nothing_between_cgroups_nor_changes_one";
        if !cgroups_here(test) {
            return;
        }
        let k = keeper("cgroup-writes");
        let (_, c, root) = project_served("cgroup-writes", vec![], Some(&k), &[], Err("not served".into()));
        let said = cgroup_writes(&k, c.rules_fd(), c.net, &root);
        let line = format!("a run's writes to cgroups, under its own rules: {said}\n");
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
        assert!(said.starts_with("own=") && !said.contains("=ok"), "{said}");
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// The same, with Landlock out of the way (rules granting every write): the session's mount namespace holds it
    /// alone — `/sys/fs/cgroup` is read-only there (`Birth::child`), each write `EROFS`. Two locks: a run's rules
    /// never grant the hierarchy, and a grant that did (a setting, a card) meets the read-only mount.
    #[test]
    fn the_sessions_cgroup_hierarchy_is_read_only() {
        let test = "the_sessions_cgroup_hierarchy_is_read_only";
        if !cgroups_here(test) {
            return;
        }
        let Some(rules) = open_rules(test) else { return };
        let k = keeper("cgroup-ro");
        let root = std::env::temp_dir();
        let said = cgroup_writes(&k, rules.fd(), RunNet::Plain, &root);
        let e = libc::EROFS;
        let want = format!("own={e} sibling={e} scope={e} out={e} mkdir={e} file={e}");
        assert_eq!(said, want, "every write refused by the mount, Landlock granting it");
        k.end();
    }

    /// Whether `path` is gone within 2 s.
    fn gone_soon(path: &Path) -> bool {
        let by = Instant::now() + Duration::from_secs(2);
        while path.exists() && Instant::now() < by {
            std::thread::sleep(Duration::from_millis(10));
        }
        !path.exists()
    }

    /// 6g: a run's cgroup goes with its last process — right after the run's reap when it left nothing; while what it
    /// left runs, it stays (that process is in it), and it goes once the keeper reaps that one too. A keeper's
    /// scope never fills up with the folders of runs long gone.
    #[test]
    fn a_runs_cgroup_goes_with_its_last_process() {
        let test = "a_runs_cgroup_goes_with_its_last_process";
        if !cgroups_here(test) {
            return;
        }
        let Some(rules) = open_rules(test) else { return };
        let k = keeper("cgroup-gone");
        let run = |n: u64| folder(&format!("{}/run-{n}", scope_of(&k).scope));
        let (_, status) = sh_through(&k, &rules, "true");
        assert!(status.success());
        assert!(gone_soon(&run(1)), "removed after its reap");
        let (_, status) = sh_through(&k, &rules, "sleep 30 >/dev/null 2>&1 &");
        assert!(status.success());
        assert!(run(2).exists(), "kept while what it left runs in it");
        let left = k.leftovers();
        assert_eq!(left.len(), 1, "the sleep it left: {left:?}");
        crate::reaper::end(&left, LEFT_TERM);
        assert!(gone_soon(&run(2)), "removed once the keeper reaped what it left");
        k.end();
    }

    /// 6g, for 6d: a rule at a run's level (`RunsIn.level`) naming its cgroup's id holds that run's connections and no
    /// other's — the level counted right on this kernel (the machine's root on Ubuntu's 6.8, the sender's namespace's
    /// on 6.1 and 7.3: one and the same where tarae is in the machine's namespace). With `socket cgroupv2 level L <A>
    /// drop` in the session's output chain, run A's connect beyond never reaches tarae; run B's, at the same moment,
    /// lands on the planted socket. (IPv4: these machines' containers have no IPv6 of their own.)
    #[test]
    fn the_level_matches_on_this_kernel() {
        use tarae_netlink::nft::{self, Expr, Reg, SocketKey, Transaction, Verdict};
        let test = "the_level_matches_on_this_kernel";
        if !cgroups_here(test) {
            return;
        }
        let k = keeper("level");
        let level = scope_of(&k).level;
        let (_, c, root) = project_served("level-own", vec![], Some(&k), &[], Err("not served".into()));
        let go = root.join("go");
        let env = [("TARAE_TO", "203.0.113.7:443"), ("TARAE_GO", go.to_str().unwrap())];
        let start = || {
            let cmd = helper_cmd("within_when", &env, Some(&root));
            k.start(&cmd, &crate::landlock::thread_name(), c.rules_fd(), c.net).unwrap()
        };
        let (a, b) = (start(), start());
        let id = a.cgroup.expect("run A in a cgroup of its own");
        let mut t = Transaction::new();
        t.rule(
            "tarae",
            "out",
            &[
                Expr::Socket { key: SocketKey::Cgroupv2 { level }, dreg: Reg::R1 },
                Expr::Cmp { sreg: Reg::R1, data: nft::cgroup(id) },
                Expr::Verdict(Verdict::Drop),
            ],
        );
        let sent = lock(&k.network)
            .as_mut()
            .expect("its network")
            .nf
            .apply(&t.end(), Instant::now() + Duration::from_secs(2));
        sent.expect("the rule taken");
        std::fs::write(&go, "").unwrap();
        let (said_a, said_b) = (helper_said(a), helper_said(b));
        assert_eq!(said_b, "reached reset", "B's lands on the planted socket");
        assert!(!said_a.starts_with("reached"), "A's held by the rule at level {level}: {said_a}");
        let taken = lock(&k.network).as_ref().map(|m| m.taken.count());
        assert_eq!(taken, Some(1), "only B's reached tarae");
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// P5's "before any run": sockets a keeper hands once a run was asked of it aren't taken — they could be a
    /// run's — so a later ask goes unanswered (refused, said in the log) and fails at its time limit.
    #[test]
    fn sockets_handed_after_a_run_are_refused() {
        let test = "sockets_handed_after_a_run_are_refused";
        if !network_here(test) {
            return;
        }
        let Some(rules) = open_rules(test) else { return };
        let k = keeper("after-a-run");
        assert!(k.plant(Families::default()).is_ok(), "before any run: taken");
        let (_, status) = sh_through(&k, &rules, "true");
        assert!(status.success());
        match k.plant(Families::default()) {
            Err(e) => assert_eq!((e.step, e.errno), (BirthStep::Planted, libc::ETIMEDOUT), "{e}"),
            Ok(_) => panic!("sockets taken after a run"),
        }
        k.end();
    }

    /// P4: without tarae's netfilter socket the session reaches nothing — the owned table goes with it (tarae's
    /// crash stood in for by closing it): a run's own connect beyond, taken to the planted socket while the table is
    /// there, isn't once it's gone.
    #[test]
    fn without_taraes_netlink_socket_the_session_reaches_nothing() {
        let test = "without_taraes_netlink_socket_the_session_reaches_nothing";
        if !network_here(test) {
            return;
        }
        let k = keeper("p4");
        let (_, c, root) = project_served("p4-own", vec![], Some(&k), &[], Err("not served".into()));
        let beyond = [("TARAE_TO", "203.0.113.7:443")];
        let said = helper_through(&k, c.rules_fd(), c.net, "within", &beyond, Some(&root));
        assert_eq!(said, "reached reset", "taken while the table is there");
        let super::super::redirect::Made { nf, taken, route, .. } =
            lock(&k.network).take().expect("its network");
        drop(nf);
        // (the table goes as its socket is released: asked again until it has, within a second)
        let by = Instant::now() + Duration::from_secs(1);
        let said = loop {
            let said = helper_through(&k, c.rules_fd(), c.net, "within", &beyond, Some(&root));
            if said != "reached reset" || Instant::now() > by {
                break said;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        assert_ne!(said, "reached reset", "taken with tarae's socket gone");
        assert_eq!(taken.count(), 1, "nothing more reached the planted socket");
        drop((route, taken));
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// P5: tarae sends netlink only on sockets of the session's netns — the one its keeper (its runs) is in. Another
    /// keeper's sockets, of a netns tarae owns too (so the lookup answers), handed in the session's keeper's place,
    /// are refused before anything is sent: that netns has no table and no route after. Its own keeper's are taken.
    #[test]
    fn a_socket_of_another_netns_is_refused_before_anything_is_sent() {
        use tarae_netlink::{Family, Protocol, Socket, nft, route};
        let test = "a_socket_of_another_netns_is_refused_before_anything_is_sent";
        if !network_here(test) {
            return;
        }
        let session = keeper("p5-session");
        let other_root = std::env::temp_dir().join(format!("tarae-keeper-{}-p5-other", std::process::id()));
        let other = Keeper::born_lo(&other_root, None, ns(), Families::here(), false, None, None)
            .expect("born, no network");
        let carried = Families {
            v4: Families::here().v4 && !other.lo_missing.v4,
            v6: Families::here().v6 && !other.lo_missing.v6,
        };
        let handed = other.plant(carried).expect("its sockets");
        let theirs = super::super::redirect::netns_of_pid(other.pid).unwrap();
        let ours = super::super::redirect::netns_of_pid(session.pid).unwrap();
        assert_ne!(theirs, ours, "two sessions, two netns");
        let made = super::super::redirect::make(
            ours,
            handed,
            &super::super::redirect::Network::session(carried.v4, carried.v6),
        );
        // (what's in the other netns now, read over its own fresh sockets)
        // (its netlink sockets only: no planted ones to bind where the first set's may still be)
        let mut look = other.plant(Families::default()).expect("its netlink sockets");
        let by = || Instant::now() + Duration::from_secs(2);
        let fd = |k: super::super::redirect::Kind, h: &mut Vec<(super::super::redirect::Kind, OwnedFd)>| {
            let at = h.iter().position(|(kind, _)| *kind == k).unwrap();
            h.remove(at).1
        };
        let mut nf =
            Socket::from_fd(fd(super::super::redirect::Kind::Netfilter, &mut look), Protocol::Netfilter)
                .unwrap();
        let mut rt =
            Socket::from_fd(fd(super::super::redirect::Kind::Route, &mut look), Protocol::Route).unwrap();
        assert!(nft::tables(&mut nf, by()).unwrap().is_empty(), "no table landed there");
        let routes = route::routes(&mut rt, Family::V4, by()).unwrap();
        assert!(!routes.iter().any(|r| r.table == 100), "no route landed there: {routes:?}");
        match made {
            Err(e) => assert_eq!((e.step, e.errno), (BirthStep::Netns, libc::EPERM), "{e}"),
            Ok(_) => panic!("another netns's sockets taken as the session's"),
        }
        drop((look, nf, rt));
        let own = super::super::redirect::make(
            theirs,
            look_again(&other, carried),
            &super::super::redirect::Network::session(carried.v4, carried.v6),
        );
        assert!(own.is_ok(), "its own keeper's sockets are the session's: {:?}", own.err());
        drop(own);
        other.end();
        session.end();
    }

    /// `plant`, again: a fresh set of the keeper's sockets (it has run nothing).
    fn look_again(k: &Keeper, carried: Families) -> Vec<(super::super::redirect::Kind, OwnedFd)> {
        k.plant(carried).expect("its sockets")
    }

    /// P1, for the birth's state: nothing a run does changes the session's rules or routes. A served run's own
    /// routing socket deleting the policy rule and the main table's default route, adding one, and asking for a
    /// netfilter socket — each refused — and the generation, the rule counts, the routes and the policy rules read
    /// back the same. (tarae makes a served run's netlink sends, with its own capability over the netns: what
    /// nlguard lets through lands.)
    #[test]
    fn a_run_changes_none_of_the_sessions_rules_or_routes() {
        let test = "a_run_changes_none_of_the_sessions_rules_or_routes";
        if !network_here(test) || !routed_here(test) {
            return;
        }
        let k = keeper("p1");
        let before = network_state(&k);
        let (p, c, root) = routed_project("p1", &k, |_| {});
        let (said, refused) = served_said(&k, (&p, &c, &root), "nlchanges", &[]);
        let each: HashMap<&str, &str> = said.split(' ').filter_map(|w| w.split_once('=')).collect();
        let eperm = libc::EPERM.to_string();
        for change in ["delrule", "delroute", "newroute"] {
            assert_eq!(each.get(change), Some(&eperm.as_str()), "{change}: {said} — {refused:?}");
        }
        assert!(each.get("netfilter").is_some_and(|n| *n != "ok"), "{said}");
        assert_eq!(network_state(&k), before, "the session's network as it was");
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// A refusal while making the session's network fails the birth, said with its step and errno — the keeper
    /// and its netns thrown away (the keeper gone within a second), never a session with half its network: a route
    /// on a device that isn't there (`ENODEV` at the routes), a rule for a chain that isn't there (`ENOENT` at the
    /// rules).
    #[test]
    fn the_birth_says_which_part_failed() {
        use tarae_netlink::nft::{Expr, Transaction, Verdict};
        use tarae_netlink::route::{Changes, RouteKind};
        use tarae_netlink::{Family, route};
        let test = "the_birth_says_which_part_failed";
        if !network_here(test) {
            return;
        }
        let root = std::env::temp_dir().join(format!("tarae-keeper-{}-failed", std::process::id()));
        let mut no_device = Changes::new();
        no_device.default_route(Family::V4, route::MAIN, RouteKind::Unicast, 4242);
        let routes = super::super::redirect::Network {
            routes: vec![no_device.end()],
            ..super::super::redirect::Network::session(true, true)
        };
        let mut no_chain = Transaction::new();
        no_chain.table("tarae", true);
        no_chain.rule("tarae", "nosuch", &[Expr::Verdict(Verdict::Accept)]);
        let rules = super::super::redirect::Network {
            rules: no_chain.end(),
            ..super::super::redirect::Network::session(true, true)
        };
        for (network, step, errno) in
            [(routes, BirthStep::Routes, libc::ENODEV), (rules, BirthStep::Rules, libc::ENOENT)]
        {
            let captured = crate::log::capture();
            let given = Some(NetworkFor::Given(network));
            let e = match Keeper::born_lo(&root, None, ns(), Families::here(), false, given, None) {
                Ok(k) => {
                    k.end();
                    panic!("born with half its network ({step:?})");
                }
                Err(e) => e,
            };
            assert_eq!(BirthError::of(&e), Some(BirthError { step, errno }), "{e}");
            assert!(e.to_string().starts_with(step.said()), "said with its step: {e}");
            // (and thrown away: the keeper — the last thing in its netns but tarae's sockets, gone with the error —
            // gone within a second)
            let ours = format!("a keeper for {}: its network failed", root.display());
            let said = captured.lines().into_iter().find(|l| l.contains(&ours)).expect("its failure said");
            let pid: u32 = said
                .split(" — keeper ")
                .nth(1)
                .and_then(|r| r.split(' ').next()?.parse().ok())
                .unwrap_or_else(|| panic!("its pid: {said}"));
            let by = Instant::now() + Duration::from_secs(1);
            while Path::new(&format!("/proc/{pid}")).exists() && Instant::now() < by {
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(!Path::new(&format!("/proc/{pid}")).exists(), "keeper {pid} still there ({step:?})");
        }
    }

    /// The interim until 6d and 6e (`redirect.rs`): a run tarae doesn't serve, beyond the session — where it used to
    /// get `ENETUNREACH` at once — is refused as quickly and as plainly as by a closed port, never left to its own
    /// time limits: its connect reset (`ECONNRESET` on its first read), a datagram it asks on a connected UDP socket
    /// answered port unreachable (`ECONNREFUSED`), and so its name lookup gives up at once — glibc asks on connected
    /// sockets (`RES_OPTIONS` keeps one that would wait to 2 s). Since 6e an unserved run's question is answered
    /// REFUSED (E2: its mark isn't one tarae gave) — even `asked`'s header alone, whose ID and flags can be read — and
    /// its lookup ends as fast.
    #[test]
    fn an_unserved_runs_way_out_fails_fast() {
        let test = "an_unserved_runs_way_out_fails_fast";
        if !network_here(test) {
            return;
        }
        let k = keeper("unserved");
        let (_, c, root) = project_served("unserved-own", vec![], Some(&k), &[], Err("not served".into()));
        let run = |what: &str, env: &[(&str, &str)]| {
            helper_through(&k, c.rules_fd(), c.net, what, env, Some(&root))
        };
        let ms = |said: &str| {
            said.rsplit_once("ms=").and_then(|(_, n)| n.parse::<u128>().ok()).unwrap_or(u128::MAX)
        };
        assert_eq!(run("within", &[("TARAE_TO", "203.0.113.7:443")]), "reached reset", "the connect");
        let t0 = Instant::now();
        let asked = run("asked", &[("TARAE_TO", "203.0.113.53:53")]);
        assert!(asked == "answered" && t0.elapsed() < Duration::from_secs(1), "the datagram: {asked}");
        let env = [("TARAE_NAME", "tarae-6c.example"), ("RES_OPTIONS", "timeout:2 attempts:1")];
        let lookup = run("lookup_timed", &env);
        assert!(lookup.starts_with("lookup=getaddrinfo ") && ms(&lookup) < 1000, "the lookup: {lookup}");
        eprintln!("{test}: {asked}; {lookup}");
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// A family the session's `lo` doesn't carry gets no routes and no rules: a run's connect beyond in that family
    /// fails at once, as before 6c (`ENETUNREACH`: no route) — never its SYN meeting the final drop and waiting out
    /// TCP's time limit. A session born as for a network without IPv6 (`born_with`); IPv4, carried, still lands on
    /// its planted socket.
    #[test]
    fn a_family_not_carried_fails_at_once_as_before() {
        let test = "a_family_not_carried_fails_at_once_as_before";
        if !network_here(test) {
            return;
        }
        let root = std::env::temp_dir().join(format!("tarae-keeper-{}-v6off", std::process::id()));
        let k =
            Keeper::born_with(&root, None, ns(), Families { v4: true, v6: false }).expect("a keeper starts");
        let (_, c, project) = project_served("v6off-own", vec![], Some(&k), &[], Err("not served".into()));
        let run =
            |to: &str| helper_through(&k, c.rules_fd(), c.net, "within", &[("TARAE_TO", to)], Some(&project));
        assert_eq!(run("[2001:db8::7]:443"), libc::ENETUNREACH.to_string(), "IPv6, not carried: no route");
        assert_eq!(run("203.0.113.7:443"), "reached reset", "IPv4, carried: taken");
        k.end();
        std::fs::remove_dir_all(&project).ok();
    }

    /// P2's second line, the ruleset's last rule: what no rule took — neither TCP nor UDP — is dropped. Its stand-in
    /// sender is the raw ICMP socket the birth made in the session's netns (runs make TCP and UDP sockets only —
    /// `a_run_makes_tcp_and_udp_sockets_only`), marked as the output chain marks what leaves (`SO_MARK`: tarae has
    /// `CAP_NET_ADMIN` over the netns), so routing takes what it sends as this machine's (table 100): an echo to the
    /// session's own address arrives — the sender works — and one beyond the session meets the drop: it never
    /// arrives, where without the rule it would be delivered here as this machine's own. (Unmarked, it would die
    /// anyway: the netns forwards nothing, and `ip_error` says nothing then — so only the marked one shows the rule.)
    #[test]
    fn what_isnt_tcp_or_udp_meets_the_final_drop() {
        let test = "what_isnt_tcp_or_udp_meets_the_final_drop";
        if !network_here(test) {
            return;
        }
        let k = keeper("drop");
        let icmp =
            lock(&k.network).as_ref().and_then(|m| m.icmp.clone()).expect("the birth's raw ICMP socket");
        let mark = |m: u32| {
            // SAFETY: an int, ours for the call, on a socket we hold.
            unsafe {
                libc::setsockopt(icmp.as_raw_fd(), libc::SOL_SOCKET, libc::SO_MARK, (&raw const m).cast(), 4)
            }
        };
        assert_eq!(mark(1), 0, "{}", io::Error::last_os_error());
        // (it only sends as made — `the_raw_icmp_socket_only_sends`: given every type while this test watches)
        super::super::redirect::icmp_filter(icmp.as_fd(), 0).unwrap();
        let beyond = std::net::Ipv4Addr::new(203, 0, 113, 7);
        assert!(
            echo(icmp.as_fd(), SESSION_V4, 0x6c01),
            "to the session's own address it arrives: the sender works"
        );
        assert!(!echo(icmp.as_fd(), beyond, 0x6c02), "beyond the session, untaken: dropped");
        mark(0);
        super::super::redirect::icmp_filter(icmp.as_fd(), u32::MAX).unwrap();
        k.end();
    }

    /// An ICMP echo to `to` (its `id`) on raw socket `fd`, and whether it got anywhere within a second: delivered in
    /// this netns (a raw ICMP socket sees every ICMP packet delivered here — the request itself, as it arrives), its
    /// reply, or an error quoting it. (Taken as this machine's beyond the session, the request is delivered but its
    /// reply can't leave: its source, `to`, is no local address once unmarked — so the arrival is what shows it.)
    fn echo(fd: BorrowedFd, to: std::net::Ipv4Addr, id: u16) -> bool {
        send_echo(fd, to, id);
        let ours = |icmp: &[u8]| icmp.len() >= 6 && icmp[4..6] == id.to_be_bytes();
        let by = Instant::now() + Duration::from_secs(1);
        let mut b = [0u8; 2048];
        while let Some(left) = by.checked_duration_since(Instant::now()) {
            if !readable(fd, left).unwrap_or(false) {
                break;
            }
            // SAFETY: a receive into a buffer of its size, on a socket we hold.
            let n = unsafe { libc::recv(fd.as_raw_fd(), b.as_mut_ptr().cast(), b.len(), libc::MSG_DONTWAIT) };
            let Ok(n) = usize::try_from(n) else { continue };
            let packet = &b[..n];
            let Some(icmp) = packet.get(usize::from(packet.first().unwrap_or(&0) & 0xf) * 4..) else {
                continue;
            };
            let quoted = icmp.get(8..).and_then(|inner| inner.get(usize::from(inner.first()? & 0xf) * 4..));
            match icmp.first() {
                Some(0 | 8) if ours(icmp) => return true,
                Some(3 | 11) if quoted.is_some_and(|q| q.first() == Some(&8) && ours(q)) => return true,
                _ => {}
            }
        }
        false
    }

    /// An ICMP echo request to `to` (its `id`) sent on raw socket `fd`.
    fn send_echo(fd: BorrowedFd, to: std::net::Ipv4Addr, id: u16) {
        let mut p = vec![8u8, 0, 0, 0];
        p.extend(id.to_be_bytes());
        p.extend(1u16.to_be_bytes());
        p.extend(b"tarae 6c");
        let sum = super::super::redirect::checksum(&p);
        p[2..4].copy_from_slice(&sum.to_be_bytes());
        let to = std::net::SocketAddr::new(to.into(), 0);
        let (sa, len) = super::super::redirect::socket_address(to);
        // SAFETY: a send of our buffer to an address of ours, on a socket we hold.
        let n = unsafe {
            libc::sendto(fd.as_raw_fd(), p.as_ptr().cast(), p.len(), 0, (&raw const sa).cast(), len)
        };
        assert_eq!(n, p.len() as isize, "the echo: {}", io::Error::last_os_error());
    }

    /// The bytes waiting on socket `fd` (`FIONREAD`: a raw socket's next packet's).
    fn queued(fd: BorrowedFd) -> i32 {
        let mut n: libc::c_int = 0;
        // SAFETY: an int, ours for the call, on a socket we hold.
        let r = unsafe { libc::ioctl(fd.as_raw_fd(), libc::FIONREAD, &mut n) };
        assert_eq!(r, 0, "FIONREAD: {}", io::Error::last_os_error());
        n
    }

    /// The birth's raw ICMP socket only sends: a raw socket gets a copy of every ICMP packet delivered in its netns,
    /// and nothing reads this one, so every type it can be given is filtered (`ICMP_FILTER`) — its queue would fill
    /// and stay full. After an echo to the session's own address, nothing waits on it; with the filter lifted, the
    /// same echo's request and reply do (the control: the echo is delivered).
    #[test]
    fn the_raw_icmp_socket_only_sends() {
        let test = "the_raw_icmp_socket_only_sends";
        if !network_here(test) {
            return;
        }
        let k = keeper("raw-sends");
        let icmp =
            lock(&k.network).as_ref().and_then(|m| m.icmp.clone()).expect("the birth's raw ICMP socket");
        send_echo(icmp.as_fd(), SESSION_V4, 0x6c11);
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(queued(icmp.as_fd()), 0, "as made: nothing kept");
        super::super::redirect::icmp_filter(icmp.as_fd(), 0).unwrap();
        send_echo(icmp.as_fd(), SESSION_V4, 0x6c12);
        let by = Instant::now() + Duration::from_secs(1);
        while queued(icmp.as_fd()) == 0 && Instant::now() < by {
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(queued(icmp.as_fd()) > 0, "unfiltered, the echo's copies would wait there");
        k.end();
    }

    /// tarae takes a keeper's sockets only as the kinds it asked: exactly `Kind::wanted`'s, in order, and the raw
    /// ICMP socket besides at most once. A kind missing, one extra, one twice, one unknown — `EPROTO`; the keeper's
    /// own failure — its errno. Each fails the birth at `Planted` (`plant`).
    #[test]
    fn a_keepers_sockets_are_taken_only_as_the_kinds_asked() {
        use super::super::redirect::Kind;
        let v4 = Families { v4: true, v6: false };
        let codes = |k: &[Kind]| k.iter().map(|k| k.code()).collect::<Vec<u8>>();
        let asked = [Kind::Route, Kind::Netfilter, Kind::Tcp4, Kind::Udp4, Kind::Dns4];
        assert_eq!(planted_kinds(v4, &codes(&asked), 0), Ok(asked.to_vec()));
        let with_raw = [&asked[..], &[Kind::Icmp4]].concat();
        assert_eq!(planted_kinds(v4, &codes(&with_raw), 0), Ok(with_raw.clone()), "the raw socket besides");
        assert_eq!(
            planted_kinds(v4, &codes(&asked), libc::EADDRNOTAVAIL),
            Err(libc::EADDRNOTAVAIL),
            "its errno"
        );
        let wrong: [(&str, Vec<u8>); 6] = [
            ("one missing", codes(&asked[..3])),
            ("one extra", codes(&[&asked[..], &[Kind::Tcp6]].concat())),
            ("one twice", codes(&[&asked[..], &[Kind::Udp4]].concat())),
            ("out of order", codes(&[Kind::Netfilter, Kind::Route, Kind::Tcp4, Kind::Udp4, Kind::Dns4])),
            ("the raw socket twice", codes(&[&with_raw[..], &[Kind::Icmp4]].concat())),
            ("a kind that isn't one", [codes(&asked), vec![9]].concat()),
        ];
        for (what, kinds) in wrong {
            assert_eq!(planted_kinds(v4, &kinds, 0), Err(libc::EPROTO), "{what}");
        }
    }

    /// The keeper's own failure to make a socket fails the birth at `Planted` with its errno: a keeper whose `lo`
    /// couldn't take the session's IPv4 address (`born_lo`'s `off_lo`) asked for IPv4's planted sockets — their bind
    /// refused (`EADDRNOTAVAIL`), nothing taken.
    #[test]
    fn a_socket_the_keeper_cant_make_fails_its_plant() {
        let test = "a_socket_the_keeper_cant_make_fails_its_plant";
        if !namespaced(test) || !ns().netns() {
            return;
        }
        let root = std::env::temp_dir().join(format!("tarae-keeper-{}-badplant", std::process::id()));
        let k = Keeper::born_lo(&root, None, ns(), Families { v4: true, v6: false }, true, None, None)
            .expect("born");
        assert!(k.lo_missing.v4, "its lo without the session's IPv4 address");
        match k.plant(Families { v4: true, v6: false }) {
            Err(e) => assert_eq!((e.step, e.errno), (BirthStep::Planted, libc::EADDRNOTAVAIL), "{e}"),
            Ok(_) => panic!("sockets taken that couldn't be made"),
        }
        k.end();
    }

    /// A keeper gets the session's network only where it has a netns of its own and the probe's network landed
    /// (D-b): none without a netns (tier C; a policy that opens something where tarae can't swap; `net` without a user
    /// namespace makes none), none where the probe's batch didn't land — whatever the other says.
    #[test]
    fn a_keeper_gets_the_network_only_with_a_netns_and_the_probes_yes() {
        let (yes, no): (Result<(), String>, Result<(), String>) = (Ok(()), Err("no NFT_TPROXY here".into()));
        assert!(network_for(Ns::session(true, true), &yes).is_some(), "A+");
        assert!(network_for(Ns::session(true, false), &yes).is_some(), "A: no PID namespace, its netns");
        assert!(network_for(Ns::NONE, &yes).is_none(), "no namespaces");
        assert!(network_for(Ns::session(false, true), &yes).is_none(), "the session's, without its netns");
        assert!(network_for(Ns { net: true, ..Ns::NONE }, &yes).is_none(), "net without user makes none");
        assert!(network_for(Ns::session(true, true), &no).is_none(), "the probe's didn't land");
    }
}
