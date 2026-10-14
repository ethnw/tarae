//! `:procs` — every process tarae started, what it's for and what it costs, a way to stop, restart or read
//! about each, and tarae's own checks on them.
//!
//! - **What's there** comes from the OS, not from bookkeeping: tarae's descendants (`tree` — libproc's child
//!   lists on macOS, `/proc` on Linux), each read for its CPU time, resident memory and (macOS) physical
//!   footprint. What each one is *for* comes from the editor (`Editor::procs_known` — its language servers,
//!   the debugger, runs, the terminal, Claude, plugins' programs); one it doesn't know is listed by name,
//!   under its parent. What a session started stays its own after its run is closed or replaced
//!   (`Spawned`, "left running"), and a child of tarae's nothing names goes under the session whose
//!   checkout it works in (#100). tarae itself is the first row. Claude asks the same with its `procs` tool.
//! - **Sampling** is a thread of its own (`start`): every `QUIET_EVERY` while `:procs` is closed, every
//!   `OPEN_EVERY` while it's open. A sample goes to the main loop, which draws nothing for it unless the panel
//!   is up or the status line's mark changed — an idle tarae stays idle (measured: `perf_e2e_procs_sampling`).
//! - **Checks** (`Procs::check`, each sample): busy for minutes, memory far above what that program usually
//!   takes (`$state/procs.json` — its first minutes' peak, over its last runs), a server left running with no
//!   file of it open (stopped), a child tarae doesn't know, one left unreaped; and, as it happens, a server
//!   exiting again and again (`procs_server_exited` — held: not started again until `r`). Each is recorded
//!   with what tarae did (`Procs.checks`, under the list, and the log); while one holds, the status line
//!   says so (`Procs::mark`) — nothing otherwise.

pub mod sockets;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::time::{Duration, Instant, SystemTime};

use crate::editor::{Editor, Mode};
use crate::event::{Event, SessionId};
use crate::key::{Code, Key};
use crate::lsp::ClientId;

/// How often tarae looks while `:procs` is closed (`TARAE_PROCS_EVERY` ms stands in, for measurements).
pub const QUIET_EVERY: Duration = Duration::from_secs(30);
/// …and while it's open.
pub const OPEN_EVERY: Duration = Duration::from_secs(1);
/// The most processes a sample reads (a fork bomb under a task mustn't make the sample one).
const MAX_PROCS: usize = 512;
/// Busy: at least this much of a core, every sample for `BUSY_FOR`.
const BUSY_PCT: f32 = 80.0;
const BUSY_FOR: Duration = Duration::from_secs(5 * 60);
/// A process's usual memory = the most it took in its first `SETTLE` (its last `USUAL_KEPT` runs' median);
/// far above = `MEMORY_TIMES` that and `MEMORY_OVER` more.
const SETTLE: Duration = Duration::from_secs(5 * 60);
const USUAL_KEPT: usize = 8;
const MEMORY_TIMES: u64 = 3;
const MEMORY_OVER: u64 = 512 << 20;
/// A server exiting `CRASHES` times within `CRASH_WINDOW` is held.
const CRASHES: usize = 3;
const CRASH_WINDOW: Duration = Duration::from_secs(10 * 60);
/// A server with no file of it open past its own stop (`IDLE_STOP`) and this much more was left running.
const LEFT_GRACE: Duration = Duration::from_secs(2 * 60);
/// A child of tarae's it doesn't know, running this long.
const STRAY_AFTER: Duration = Duration::from_secs(10 * 60);
/// Self-checks kept (since tarae started — `:procs` shows them, newest first) · how long one that ended
/// stays in the list (the log keeps it — an hours-old ended one read as something wrong now).
const CHECKS_KEPT: usize = 100;
pub const ENDED_KEPT: Duration = Duration::from_secs(30 * 60);

// ── What the OS says ─────────────────────────────────────────────────────

/// One process as the OS reports it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Os {
    pub pid: u32,
    pub ppid: u32,
    pub name: String,
    /// When it started, in seconds since the epoch (with the pid, which process this is — pids are reused).
    pub started: u64,
    /// CPU time so far (user + system).
    pub cpu: Duration,
    pub rss: u64,
    /// Physical footprint (macOS — what Activity Monitor shows); None elsewhere.
    pub footprint: Option<u64>,
    /// It ended and nobody waited for it.
    pub zombie: bool,
    /// Its working folder — read for tarae's own children only (`Sampler::sample`): one the editor can't
    /// name goes under the session whose checkout it works in (#100).
    pub cwd: Option<PathBuf>,
    /// The run of Claude's that left it running, by its environment (Linux: tarae's own children only —
    /// `sandbox::left_by`): the run's tag, and its project when tarae knows it.
    pub left: Option<(String, Option<PathBuf>)>,
    /// Its session (Linux): a child of tarae's in another than tarae's own is one a program it ran left — tarae
    /// is its last parent (`reaper`), it didn't start it.
    pub sid: Option<u32>,
}

/// A C string in a fixed buffer.
#[cfg(target_os = "macos")]
fn c_str(b: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = b.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Mach time units → nanoseconds (not 1:1 on Apple silicon).
#[cfg(target_os = "macos")]
#[allow(deprecated)] // (libc's mach_timebase_info — the one call the mach2 crate would be added for)
fn timebase() -> (u128, u128) {
    static TB: std::sync::OnceLock<(u128, u128)> = std::sync::OnceLock::new();
    *TB.get_or_init(|| {
        let mut tb = libc::mach_timebase_info { numer: 0, denom: 0 };
        // SAFETY: fills in the struct we own.
        unsafe { libc::mach_timebase_info(&mut tb) };
        (tb.numer.max(1) as u128, tb.denom.max(1) as u128)
    })
}

/// Process `pid` now — None if it's gone (or not ours to read).
#[cfg(target_os = "macos")]
pub fn read(pid: u32) -> Option<Os> {
    // SAFETY: plain out-structs of the sizes given; the calls only fill them in.
    unsafe {
        let mut bsd: libc::proc_bsdinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        if libc::proc_pidinfo(pid as i32, libc::PROC_PIDTBSDINFO, 0, (&raw mut bsd).cast(), size) != size {
            return None;
        }
        let mut ri: libc::rusage_info_v2 = std::mem::zeroed();
        let ok = libc::proc_pid_rusage(pid as i32, libc::RUSAGE_INFO_V2, (&raw mut ri).cast()) == 0;
        let (numer, denom) = timebase();
        let ticks = if ok { (ri.ri_user_time + ri.ri_system_time) as u128 } else { 0 };
        let name = match c_str(&bsd.pbi_name) {
            n if n.is_empty() => c_str(&bsd.pbi_comm),
            n => n,
        };
        Some(Os {
            pid,
            ppid: bsd.pbi_ppid,
            name,
            started: bsd.pbi_start_tvsec,
            cpu: Duration::from_nanos((ticks * numer / denom) as u64),
            rss: if ok { ri.ri_resident_size } else { 0 },
            footprint: ok.then_some(ri.ri_phys_footprint),
            zombie: bsd.pbi_status == libc::SZOMB,
            cwd: None,
            left: None,
            sid: None,
        })
    }
}

/// `pid`'s children.
#[cfg(target_os = "macos")]
pub fn children(pid: u32) -> Vec<u32> {
    let mut buf: Vec<libc::pid_t> = vec![0; 64];
    loop {
        let bytes = (buf.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        buf.iter_mut().for_each(|p| *p = 0);
        // SAFETY: the buffer is `bytes` long; the call writes pids into it.
        let n = unsafe { libc::proc_listchildpids(pid as i32, buf.as_mut_ptr().cast(), bytes) };
        if n <= 0 {
            return Vec::new();
        }
        // (a full buffer may have left some out: a bigger one)
        if n as usize >= buf.len() && buf.len() < 1 << 16 {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        return buf.iter().take(n as usize).filter(|&&p| p > 0).map(|&p| p as u32).collect();
    }
}

/// `/proc/<pid>/stat` read (fields after the name's closing parenthesis — the name may hold spaces).
/// `hz`: clock ticks a second · `page`: bytes a page · `boot`: when the machine started (epoch seconds).
#[cfg(any(target_os = "linux", test))]
fn parse_stat(pid: u32, stat: &str, hz: u64, page: u64, boot: u64) -> Option<Os> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?.to_string();
    let f: Vec<&str> = stat.get(close + 1..)?.split_whitespace().collect();
    let num = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok());
    let ticks = num(11)? + num(12)?;
    Some(Os {
        pid,
        ppid: num(1)? as u32,
        name,
        started: boot + num(19)? / hz.max(1),
        cpu: Duration::from_nanos(ticks.saturating_mul(1_000_000_000) / hz.max(1)),
        rss: num(21)? * page,
        footprint: None,
        zombie: f.first() == Some(&"Z"),
        cwd: None,
        left: None,
        sid: num(3).map(|s| s as u32),
    })
}

#[cfg(target_os = "linux")]
pub fn read(pid: u32) -> Option<Os> {
    static CONST: std::sync::OnceLock<(u64, u64, u64)> = std::sync::OnceLock::new();
    let (hz, page, boot) = *CONST.get_or_init(|| {
        // SAFETY: sysconf only reads constants.
        let (hz, page) = unsafe { (libc::sysconf(libc::_SC_CLK_TCK), libc::sysconf(libc::_SC_PAGESIZE)) };
        let boot = std::fs::read_to_string("/proc/stat")
            .ok()
            .and_then(|s| s.lines().find_map(|l| l.strip_prefix("btime ")?.trim().parse().ok()))
            .unwrap_or(0);
        (hz.max(1) as u64, page.max(1) as u64, boot)
    });
    parse_stat(pid, &std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?, hz, page, boot)
}

/// `pid`'s children: its threads' `children` lists, or (a kernel without them) every process's parent.
#[cfg(target_os = "linux")]
pub fn children(pid: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let mut listed = false;
    for t in std::fs::read_dir(format!("/proc/{pid}/task")).into_iter().flatten().flatten() {
        if let Ok(s) = std::fs::read_to_string(t.path().join("children")) {
            listed = true;
            out.extend(s.split_whitespace().filter_map(|p| p.parse::<u32>().ok()));
        }
    }
    if !listed {
        for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
            let Some(p) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
            if read(p).is_some_and(|o| o.ppid == pid) {
                out.push(p);
            }
        }
    }
    crate::sorting::sort(&mut out);
    out.dedup();
    out
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn read(_pid: u32) -> Option<Os> {
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn children(_pid: u32) -> Vec<u32> {
    Vec::new()
}

/// `root` and everything under it, parents before their children (at most `MAX_PROCS`). A pid listed under
/// one parent but found under another by the time it's read was reused — left out.
pub fn tree(root: u32) -> Vec<Os> {
    let mut out = Vec::new();
    let mut todo = vec![(root, None)];
    while let Some((pid, parent)) = todo.pop() {
        if out.len() >= MAX_PROCS {
            break;
        }
        let Some(os) = read(pid) else { continue };
        if parent.is_some_and(|p| p != os.ppid) {
            continue;
        }
        if !os.zombie {
            todo.extend(children(pid).into_iter().rev().map(|c| (c, Some(pid))));
        }
        out.push(os);
    }
    out
}

/// Process `pid`'s command line, its words joined by spaces (`KERN_PROCARGS2`: argc, the executable's path,
/// NULs, then the words) — None if it's gone or not ours to read.
#[cfg(target_os = "macos")]
pub fn command(pid: u32) -> Option<String> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid as libc::c_int];
    let mut size: libc::size_t = 0;
    let null = std::ptr::null_mut();
    // SAFETY: a size query (no buffer), then a read into a buffer of that size; both only fill in.
    unsafe {
        if libc::sysctl(mib.as_mut_ptr(), 3, null, &mut size, null, 0) != 0 || size < 4 {
            return None;
        }
        let mut buf = vec![0u8; size];
        if libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, null, 0) != 0 {
            return None;
        }
        buf.truncate(size);
        let argc = i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?).max(0) as usize;
        let rest = &buf[4..];
        let rest = &rest[rest.iter().position(|&b| b == 0)?..];
        let rest = &rest[rest.iter().position(|&b| b != 0)?..];
        let words: Vec<String> =
            rest.split(|&b| b == 0).take(argc).map(|w| String::from_utf8_lossy(w).into_owned()).collect();
        (!words.is_empty()).then(|| words.join(" "))
    }
}

/// Process `pid`'s working folder.
#[cfg(target_os = "macos")]
pub fn cwd(pid: u32) -> Option<PathBuf> {
    // SAFETY: a plain out-struct of the size given; the call only fills it in.
    unsafe {
        let mut vi: libc::proc_vnodepathinfo = std::mem::zeroed();
        let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
        if libc::proc_pidinfo(pid as i32, libc::PROC_PIDVNODEPATHINFO, 0, (&raw mut vi).cast(), size) != size
        {
            return None;
        }
        let flat: Vec<libc::c_char> = vi.pvi_cdir.vip_path.iter().flatten().copied().collect();
        Some(PathBuf::from(c_str(&flat))).filter(|p| !p.as_os_str().is_empty())
    }
}

#[cfg(target_os = "linux")]
pub fn command(pid: u32) -> Option<String> {
    let b = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    let words: Vec<String> = b
        .split(|&c| c == 0)
        .filter(|w| !w.is_empty())
        .map(|w| String::from_utf8_lossy(w).into_owned())
        .collect();
    (!words.is_empty()).then(|| words.join(" "))
}

#[cfg(target_os = "linux")]
pub fn cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// Every process there is now (those not ours to read come along; reading them says nothing).
#[cfg(target_os = "linux")]
pub fn all() -> Vec<u32> {
    let entries = std::fs::read_dir("/proc").into_iter().flatten().flatten();
    entries.filter_map(|e| e.file_name().to_str()?.parse().ok()).collect()
}

/// Every process there is now.
#[cfg(target_os = "macos")]
pub fn all() -> Vec<u32> {
    let mut buf: Vec<libc::pid_t> = vec![0; 1024];
    loop {
        let bytes = (buf.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
        // SAFETY: the buffer is `bytes` long; the call writes pids into it.
        let n = unsafe { libc::proc_listallpids(buf.as_mut_ptr().cast(), bytes) };
        if n <= 0 {
            return Vec::new();
        }
        // (a full buffer may have left some out: a bigger one)
        if n as usize >= buf.len() && buf.len() < 1 << 20 {
            buf.resize(buf.len() * 4, 0);
            continue;
        }
        return buf.iter().take(n as usize).filter(|&&p| p > 0).map(|&p| p as u32).collect();
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn all() -> Vec<u32> {
    Vec::new()
}

/// The files process `pid` has open, where it's ours to read (Linux: `/proc/<pid>/fd`) — elsewhere none
/// (macOS's call for it isn't in `libc`).
pub fn open_files(pid: u32) -> Vec<PathBuf> {
    #[cfg(target_os = "linux")]
    {
        let fds = std::fs::read_dir(format!("/proc/{pid}/fd")).into_iter().flatten().flatten();
        fds.filter_map(|e| std::fs::read_link(e.path()).ok()).collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        Vec::new()
    }
}

/// One thread of a process, as an unprivileged reader is told it: what it waits in (`stuck.rs`, #181).
#[derive(Clone, Debug, PartialEq)]
pub struct Thread {
    pub name: String,
    /// Its state in words: `sleeping`, `running`, `in uninterruptible wait` …
    pub state: String,
    /// The kernel function it sleeps in (Linux's `wchan`) — None where it isn't told.
    pub wchan: Option<String>,
    /// CPU time so far (user + system).
    pub cpu: Duration,
}

/// The most threads read of one process.
const THREADS_MAX: usize = 256;

/// A thread's `/proc/<pid>/task/<tid>/stat` and `wchan` read (`hz`: clock ticks a second).
#[cfg(any(target_os = "linux", test))]
fn parse_thread(stat: &str, wchan: Option<&str>, hz: u64) -> Option<Thread> {
    let name = stat.get(stat.find('(')? + 1..stat.rfind(')')?)?.to_string();
    let f: Vec<&str> = stat.get(stat.rfind(')')? + 1..)?.split_whitespace().collect();
    let num = |i: usize| f.get(i).and_then(|v| v.parse::<u64>().ok());
    let state = match *f.first()? {
        "R" => "running",
        "S" => "sleeping",
        "D" => "in uninterruptible wait",
        "T" => "stopped",
        "t" => "stopped by a tracer",
        "Z" => "ended",
        "I" => "idle",
        "X" => "dead",
        other => other,
    };
    let ticks = num(11)? + num(12)?;
    Some(Thread {
        name,
        state: state.into(),
        // (`0`: not sleeping, or the kernel hides its addresses — kptr_restrict)
        wchan: wchan.map(str::trim).filter(|w| !w.is_empty() && *w != "0").map(str::to_string),
        cpu: Duration::from_nanos(ticks.saturating_mul(1_000_000_000) / hz.max(1)),
    })
}

/// Process `pid`'s threads, at most `THREADS_MAX` — empty when it's gone or not ours to read.
#[cfg(target_os = "linux")]
pub fn threads(pid: u32) -> Vec<Thread> {
    // SAFETY: sysconf only reads a constant.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) }.max(1) as u64;
    let dir = PathBuf::from(format!("/proc/{pid}/task"));
    let mut tids: Vec<u64> = std::fs::read_dir(&dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .collect();
    crate::sorting::sort(&mut tids);
    tids.iter()
        .take(THREADS_MAX)
        .filter_map(|tid| {
            let t = dir.join(tid.to_string());
            let stat = std::fs::read_to_string(t.join("stat")).ok()?;
            parse_thread(&stat, std::fs::read_to_string(t.join("wchan")).ok().as_deref(), hz)
        })
        .collect()
}

/// Process `pid`'s threads, at most `THREADS_MAX` — empty when it's gone or not ours to read. macOS tells
/// an unprivileged reader each thread's state and CPU time, not where it waits.
#[cfg(target_os = "macos")]
pub fn threads(pid: u32) -> Vec<Thread> {
    // (libproc's flavour listing a process's thread handles — not in the libc crate)
    const PROC_PIDLISTTHREADS: libc::c_int = 6;
    let mut ids = vec![0u64; THREADS_MAX];
    let bytes = (ids.len() * std::mem::size_of::<u64>()) as libc::c_int;
    // SAFETY: the buffer is `bytes` long; the call writes thread handles into it.
    let n = unsafe { libc::proc_pidinfo(pid as i32, PROC_PIDLISTTHREADS, 0, ids.as_mut_ptr().cast(), bytes) };
    let count = (n.max(0) as usize / std::mem::size_of::<u64>()).min(ids.len());
    ids[..count]
        .iter()
        .filter_map(|&id| {
            // SAFETY: a plain out-struct of the size given; the call only fills it in.
            let ti = unsafe {
                let mut ti: libc::proc_threadinfo = std::mem::zeroed();
                let size = std::mem::size_of::<libc::proc_threadinfo>() as libc::c_int;
                let got =
                    libc::proc_pidinfo(pid as i32, libc::PROC_PIDTHREADINFO, id, (&raw mut ti).cast(), size);
                (got == size).then_some(ti)
            }?;
            let state = match ti.pth_run_state {
                libc::TH_STATE_RUNNING => "running",
                libc::TH_STATE_STOPPED => "stopped",
                libc::TH_STATE_WAITING if ti.pth_flags & libc::TH_FLAGS_IDLE != 0 => "idle",
                libc::TH_STATE_WAITING => "waiting",
                libc::TH_STATE_UNINTERRUPTIBLE => "in uninterruptible wait",
                libc::TH_STATE_HALTED => "halted",
                _ => "in an unknown state",
            };
            Some(Thread {
                name: c_str(&ti.pth_name),
                state: state.into(),
                wchan: None,
                // (nanoseconds — thread_basic_info's, unlike the task's Mach ticks)
                cpu: Duration::from_nanos(ti.pth_user_time.saturating_add(ti.pth_system_time)),
            })
        })
        .collect()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn threads(_pid: u32) -> Vec<Thread> {
    Vec::new()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn command(_pid: u32) -> Option<String> {
    None
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn cwd(_pid: u32) -> Option<PathBuf> {
    None
}

// ── Samples ──────────────────────────────────────────────────────────────

/// One look at the tree: each process with its share of a core since the look before (None: its first).
pub struct Sample {
    pub procs: Vec<(Os, Option<f32>)>,
    /// What the look cost (the sampling thread's time for it).
    pub took: Duration,
    /// What `$state/procs.json` held (the thread's first sample only).
    usual: Option<Usual>,
    /// The file systems tarae writes to, as this look found them (`volumes`).
    pub free: Vec<Volume>,
}

impl Sample {
    /// A look of these processes, as the sampling thread sends one (tests).
    #[cfg(test)]
    pub fn of(procs: Vec<(Os, Option<f32>)>) -> Sample {
        Sample { procs, took: Duration::ZERO, usual: None, free: Vec::new() }
    }
}

// ── Free space (#219) ────────────────────────────────────────────────────────────────────

/// A file system tarae writes to: where it was looked at from, what's free of it and its whole size (bytes).
#[derive(Clone, Debug, PartialEq)]
pub struct Volume {
    pub at: PathBuf,
    pub free: u64,
    pub total: u64,
}

/// Free below this (or a tenth of a small disk) marks the status line.
const DISK_LOW: u64 = 10 << 30;
/// Free below this (or a fiftieth of a small disk) is said on a card that waits for a key — builds and runs
/// start failing soon (#219: 117 MB left, a session stopped mid-turn and nothing said why).
const DISK_FLOOR: u64 = 2 << 30;

/// Where tarae and what it runs write: the home folder (the tools' build folders — `~/.cargo/build`), tarae's
/// data (sessions' checkouts) and cache (run folders), the temp folder.
fn disk_places() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    home.into_iter()
        .chain(crate::runtime::data_dir())
        .chain(crate::runtime::cache_dir())
        .chain([std::env::temp_dir()])
        .collect()
}

/// The file systems `places` are on, one each (by device) — a place not there yet counts by its nearest
/// folder that is.
pub fn volumes(places: &[PathBuf]) -> Vec<Volume> {
    use std::os::unix::fs::MetadataExt as _;
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for p in places {
        let Some(at) = p.ancestors().find(|a| a.exists()) else { continue };
        let (Ok(m), Some(st)) = (std::fs::metadata(at), crate::fswatch::statfs(at)) else { continue };
        if seen.contains(&m.dev()) {
            continue;
        }
        seen.push(m.dev());
        #[allow(clippy::unnecessary_cast)] // (the field types differ between systems)
        let block = st.f_bsize as u64;
        #[allow(clippy::unnecessary_cast)]
        let (free, total) = (st.f_bavail as u64 * block, st.f_blocks as u64 * block);
        out.push(Volume { at: at.to_path_buf(), free, total });
    }
    out
}

/// What free space says: below `DISK_LOW` the status line's mark, below `DISK_FLOOR` a card too (`procs_found`).
pub fn disk_check(vols: &[Volume]) -> Vec<Finding> {
    let mut out = Vec::new();
    for v in vols.iter().filter(|v| v.total > 0) {
        let (low, floor) = (DISK_LOW.min(v.total / 10), DISK_FLOOR.min(v.total / 50));
        let shown = crate::sandbox::home_shown(&v.at.display().to_string());
        let what = format!("{} free of {} on {shown}", mem(v.free), mem(v.total));
        let did = "nothing — space W shows each session's build folder; x drops an idle one's";
        for (key, under) in [("disk-low", low), ("disk-full", floor)] {
            if v.free < under {
                out.push(Finding {
                    key: format!("{key}:{}", v.at.display()),
                    pid: 0,
                    what: what.clone(),
                    short: format!("disk {} free", mem(v.free)),
                    did: did.into(),
                    act: Act::Nothing,
                });
            }
        }
    }
    out
}

/// Each process's CPU time at the look before — for its share since.
#[derive(Default)]
pub struct Sampler {
    prev: HashMap<(u32, u64), Duration>,
    at: Option<Instant>,
}

impl Sampler {
    pub fn sample(&mut self, root: u32) -> Sample {
        let t0 = Instant::now();
        let wall = self.at.map(|a| t0.duration_since(a)).filter(|w| *w >= Duration::from_millis(50));
        let procs: Vec<(Os, Option<f32>)> = tree(root)
            .into_iter()
            .map(|mut os| {
                if os.ppid == root {
                    os.cwd = cwd(os.pid);
                    os.left = crate::sandbox::left_by(os.pid);
                }
                let before = self.prev.get(&(os.pid, os.started));
                let pct = wall
                    .zip(before)
                    .map(|(w, c)| os.cpu.saturating_sub(*c).as_secs_f32() / w.as_secs_f32() * 100.0);
                (os, pct)
            })
            .collect();
        self.prev = procs.iter().map(|(o, _)| ((o.pid, o.started), o.cpu)).collect();
        self.at = Some(t0);
        Sample { procs, took: t0.elapsed(), usual: None, free: Vec::new() }
    }
}

/// What the editor tells the sampling thread.
pub enum Wake {
    /// `:procs` opened (look now, then every second) or closed.
    Open(bool),
    /// Look now (an action changed what's there).
    Now,
}

fn quiet_every() -> Duration {
    std::env::var("TARAE_PROCS_EVERY")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(QUIET_EVERY, |ms| Duration::from_millis(ms.max(10)))
}

/// One look at tarae's tree — and the children tarae adopted (tarae is the last parent of what its programs
/// leave — `reaper`), found ended in two looks `reaper::GRACE` apart, reaped; those are gone from the look.
pub fn look(s: &mut Sampler, reaper: &mut crate::reaper::Reaper, now: Instant) -> Sample {
    let me = std::process::id();
    let mut sample = s.sample(me);
    let tree: Vec<&Os> = sample.procs.iter().map(|(o, _)| o).collect();
    let reaped = reaper.round(&tree, me, now);
    if !reaped.is_empty() {
        crate::log::line("procs", "info", &format!("reaped what nobody waited for: {reaped:?}"));
        sample.procs.retain(|(o, _)| !reaped.contains(&o.pid));
    }
    sample
}

/// The sampling thread: a look now, then one every `QUIET_EVERY` (`OPEN_EVERY` while `:procs` is open), each
/// sent to the main loop. It ends with the editor.
pub fn start(events: crate::event::Tx, usual_path: Option<PathBuf>) -> Sender<Wake> {
    let (tx, rx) = channel();
    let run = move || {
        let (mut s, mut reaper) = (Sampler::default(), crate::reaper::Reaper::default());
        let (quiet, mut open) = (quiet_every(), false);
        let mut usual = Some(usual_path.as_deref().map(load_usual).unwrap_or_default());
        let places = disk_places();
        // (screenshots and drivers: every volume this free, in bytes — the mark and the card without a full disk)
        let free_as = std::env::var("TARAE_DISK_FREE").ok().and_then(|v| v.parse::<u64>().ok());
        loop {
            let mut sample = look(&mut s, &mut reaper, Instant::now());
            sample.usual = usual.take();
            sample.free = volumes(&places);
            if let Some(free) = free_as {
                sample.free.iter_mut().for_each(|v| v.free = free.min(v.total));
            }
            let job = move |ed: &mut Editor| ed.procs_sampled(sample);
            if events.send(Event::Job(Box::new(job))).is_err() {
                return;
            }
            match wait(&rx, if open { OPEN_EVERY } else { quiet }, &mut open) {
                Some(()) => {}
                None => return,
            }
        }
    };
    let _ = std::thread::Builder::new().name("tarae-procs".into()).spawn(run);
    tx
}

/// Until the next look is due — or sooner, opened or asked. None: the editor is gone.
fn wait(rx: &Receiver<Wake>, every: Duration, open: &mut bool) -> Option<()> {
    let due = Instant::now() + every;
    loop {
        match rx.recv_timeout(due.saturating_duration_since(Instant::now())) {
            Ok(Wake::Open(o)) => {
                let opened = o && !*open;
                *open = o;
                if opened {
                    return Some(());
                }
            }
            Ok(Wake::Now) => return Some(()),
            Err(RecvTimeoutError::Timeout) => return Some(()),
            Err(RecvTimeoutError::Disconnected) => return None,
        }
    }
}

// ── What each one is for ─────────────────────────────────────────────────

/// What a process is to tarae.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    Tarae,
    Lsp(ClientId),
    /// A server held after exiting again and again — no process; `r` starts it.
    Held(String, PathBuf),
    Debug,
    /// What a debug attach started before it (a port-forward).
    Helper,
    Task,
    Test,
    Terminal,
    Chat,
    /// `space i`'s claude, answering.
    Ask,
    /// A session's reviewer, reading (`reviewer.rs`).
    Reviewer,
    /// The claude kept started for the next `space i`.
    Spare,
    /// A plugin's program: (plugin, its id for it).
    Plugin(String, u64),
    /// Not one tarae knows (or under one it does).
    Other,
    /// A session's heading (no process): what's under it, summed.
    Session(SessionId),
    /// A run waiting in its session's line (`queued.rs`) — no process yet; `s` takes it out.
    Queued(u64),
}

impl Kind {
    /// The list's order of tarae's children.
    fn order(&self) -> u8 {
        match self {
            Kind::Tarae => 0,
            Kind::Lsp(_) | Kind::Held(..) => 1,
            Kind::Chat | Kind::Ask | Kind::Reviewer | Kind::Spare => 2,
            Kind::Debug | Kind::Helper => 3,
            Kind::Task | Kind::Queued(_) => 4,
            Kind::Test => 5,
            Kind::Terminal => 6,
            Kind::Plugin(..) => 7,
            Kind::Other => 8,
            Kind::Session(_) => 9,
        }
    }

    /// What the user's own programs run under: busy or big there is their business, not a fault.
    fn users(&self) -> bool {
        matches!(self, Kind::Task | Kind::Test | Kind::Terminal | Kind::Debug)
    }

    /// Whether `r` means something for it.
    pub fn restarts(&self) -> bool {
        matches!(self, Kind::Lsp(_) | Kind::Held(..) | Kind::Task | Kind::Test | Kind::Chat)
    }
}

/// A process the editor knows: what it's for.
pub struct Known {
    pub pid: u32,
    pub kind: Kind,
    /// Its name in the list (the server's, the task's).
    pub title: String,
    /// What for: a root, a command, a file.
    pub what: String,
    /// The session it's for — None: shared (and with one session, every one).
    pub session: Option<SessionId>,
    /// When it started (epoch seconds), when that's what tells it from a later process given its pid — a
    /// remembered one's (`Spawned`); None: the editor holds it now.
    pub at: Option<u64>,
}

impl Known {
    /// Whether it's process `os`.
    fn is(&self, os: &Os) -> bool {
        self.pid == os.pid && self.at.is_none_or(|a| os.started.abs_diff(a) <= 2)
    }
}

/// A process a session started (a run of Claude's, a task, tests — `Session.spawned`): kept while the OS
/// still shows it, so it stays its session's after its run is closed or replaced (#100: a program of Claude's
/// run that outlived its panel was listed as nobody's, its session showing nothing).
#[derive(Clone, Debug)]
pub struct Spawned {
    pub pid: u32,
    /// When it was started, epoch seconds (with the pid, which process — pids are reused).
    pub at: u64,
    pub title: String,
    pub what: String,
}

impl Editor {
    /// Process `pid` was just started in the session in `ed.session`, for `title` (`what`: its command).
    pub(crate) fn procs_spawned(&mut self, pid: u32, title: &str, what: &str) {
        let at = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        self.session.spawned.push(Spawned { pid, at, title: title.to_string(), what: what.to_string() });
    }
}

/// A session as `:procs` heads its processes: its name as its tab says it, its checkout.
#[derive(Clone, Debug)]
pub struct Group {
    pub id: SessionId,
    pub name: String,
    pub root: PathBuf,
}

/// A row of the list.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// 0 for a held server (no process).
    pub pid: u32,
    /// Its parent (0 with no process).
    pub ppid: u32,
    started: u64,
    /// Under tarae's child (0) — its children 1, theirs 2.
    pub depth: usize,
    pub kind: Kind,
    /// The kind of tarae's child it's under (its own, for one).
    under: Kind,
    pub title: String,
    pub what: String,
    /// The program's own name (what its usual memory is kept under).
    program: String,
    pub up: Option<Duration>,
    pub cpu: Option<f32>,
    pub rss: u64,
    pub footprint: Option<u64>,
    pub zombie: bool,
    /// A check holds about it now.
    pub flagged: bool,
    /// The session it's under (its heading's, for a heading) — None: shared.
    pub session: Option<SessionId>,
    /// Left by a program tarae ran, tarae only its last parent (`reaper`): not a stray of tarae's own.
    pub left: bool,
}

impl Row {
    fn key(&self) -> (u32, u64) {
        (self.pid, self.started)
    }

    /// Whether it's the row `other` was, a look later: the same process, or — for one with none (a held
    /// server, a session's heading) — the same thing.
    fn same(&self, other: &Row) -> bool {
        match self.pid {
            0 => self.kind == other.kind,
            _ => self.key() == other.key(),
        }
    }

    /// The memory it's judged by: its footprint, else what's resident.
    pub fn memory(&self) -> u64 {
        self.footprint.unwrap_or(self.rss)
    }
}

/// The list: tarae first, then its children in `Kind::order` (each with what's under it, depth first) — the
/// held servers after the running ones. With sessions (`groups` — none while there's one), what's shared
/// stays on top (plugins, `space i`'s claude, ones tarae doesn't know), then each session under a heading
/// that sums its processes' CPU and memory: its chat, servers (a held one by its root), terminal, tasks and
/// tests. `now`: epoch seconds.
pub fn rows(
    sample: &Sample,
    known: &[Known],
    me: u32,
    held: &[Held],
    groups: &[Group],
    now: u64,
) -> Vec<Row> {
    let by_pid: HashMap<u32, &Os> = sample.procs.iter().map(|(o, _)| (o.pid, o)).collect();
    let pct: HashMap<u32, Option<f32>> = sample.procs.iter().map(|(o, p)| (o.pid, *p)).collect();
    let mut kids: HashMap<u32, Vec<u32>> = HashMap::new();
    for (o, _) in &sample.procs {
        if o.pid != me {
            kids.entry(o.ppid).or_default().push(o.pid);
        }
    }
    let row = |os: &Os, depth: usize, kind: Kind, under: Kind, title: String, what: String, session| Row {
        pid: os.pid,
        ppid: os.ppid,
        started: os.started,
        depth,
        kind,
        under,
        title,
        what,
        program: os.name.clone(),
        up: (os.started > 0).then(|| Duration::from_secs(now.saturating_sub(os.started))),
        cpu: pct.get(&os.pid).copied().flatten(),
        rss: os.rss,
        footprint: os.footprint,
        zombie: os.zombie,
        flagged: false,
        session,
        left: false,
    };
    // No process: a held server, a session's heading
    let bare = |kind: Kind, under: Kind, title: &str, what: String, session| Row {
        pid: 0,
        ppid: 0,
        started: 0,
        depth: 0,
        kind,
        under,
        title: title.to_string(),
        what,
        program: title.to_string(),
        up: None,
        cpu: None,
        rss: 0,
        footprint: None,
        zombie: false,
        flagged: false,
        session,
        left: false,
    };
    let mut out = Vec::new();
    if let Some(os) = by_pid.get(&me) {
        out.push(row(os, 0, Kind::Tarae, Kind::Tarae, "tarae".into(), "this editor".into(), None));
    }
    // (a session that isn't among `groups` is no session here)
    let grouped = |l: Option<SessionId>| l.filter(|l| groups.iter().any(|g| g.id == *l));
    // (one the editor can't name: the session whose checkout it works in, the innermost — #100)
    let working_in = |os: &Os| {
        let dir = os.cwd.as_ref()?;
        groups
            .iter()
            .filter(|g| dir.starts_with(&g.root))
            .max_by_key(|g| g.root.as_os_str().len())
            .map(|g| g.id)
    };
    // (what a run of Claude's left running: the session whose checkout the run was in, the innermost)
    let run_in = |root: &Path| {
        groups
            .iter()
            .filter(|g| root.starts_with(&g.root))
            .max_by_key(|g| g.root.as_os_str().len())
            .map(|g| g.id)
    };
    let mine = by_pid.get(&me).and_then(|o| o.sid);
    let mut top: Vec<(Kind, String, String, &Os, Option<SessionId>)> = kids
        .get(&me)
        .into_iter()
        .flatten()
        .filter_map(|pid| by_pid.get(pid))
        .map(|os| match (known.iter().find(|k| k.is(os)), &os.left) {
            (Some(k), _) => (k.kind.clone(), k.title.clone(), k.what.clone(), *os, grouped(k.session)),
            (None, Some((_, root))) => {
                let what = match root {
                    Some(r) => format!("left by a run of Claude's in {}", crate::allow::tilde(r)),
                    None => "left by a run of Claude's before tarae restarted".into(),
                };
                let session = root.as_deref().and_then(run_in).or_else(|| working_in(os));
                (Kind::Other, format!("{} · left by a run", os.name), what, *os, session)
            }
            (None, None) => (Kind::Other, os.name.clone(), String::new(), *os, working_in(os)),
        })
        .collect();
    // (tarae only its last parent: a run's, or in a session not tarae's — a program it ran left it)
    let adopted = |os: &Os| os.left.is_some() || os.sid.is_some_and(|s| Some(s) != mine);
    crate::sorting::by(&mut top, |a, b| (a.0.order(), &a.1, a.3.pid).cmp(&(b.0.order(), &b.1, b.3.pid)));
    // A held server goes with the session its root is in (the innermost)
    let held_in = |h: &Held| {
        groups
            .iter()
            .filter(|g| h.root.starts_with(&g.root))
            .max_by_key(|g| g.root.as_os_str().len())
            .map(|g| g.id)
    };
    for session in std::iter::once(None).chain(groups.iter().map(|g| Some(g.id))) {
        let start = out.len();
        if let Some(g) = groups.iter().find(|g| Some(g.id) == session) {
            out.push(bare(
                Kind::Session(g.id),
                Kind::Session(g.id),
                &g.name,
                crate::allow::tilde(&g.root),
                session,
            ));
        }
        for (kind, title, what, os, _) in top.iter().filter(|t| t.4 == session) {
            let left = *kind == Kind::Other && adopted(os);
            out.push(Row {
                left,
                ..row(os, 0, kind.clone(), kind.clone(), title.clone(), what.clone(), session)
            });
            // What it started, depth first (a known one under it keeps its own name — a plugin's shell's child)
            let mut todo: Vec<(u32, usize)> =
                kids.get(&os.pid).into_iter().flatten().rev().map(|&p| (p, 1)).collect();
            while let Some((pid, depth)) = todo.pop() {
                let Some(o) = by_pid.get(&pid) else { continue };
                let (k, t, w) = match known.iter().find(|k| k.is(o)) {
                    Some(k) => (k.kind.clone(), k.title.clone(), k.what.clone()),
                    None => (Kind::Other, o.name.clone(), String::new()),
                };
                out.push(row(o, depth, k, kind.clone(), t, w, session));
                todo.extend(kids.get(&pid).into_iter().flatten().rev().map(|&p| (p, depth + 1)));
            }
        }
        // Its held servers after its running ones (past its heading)
        let first = start + usize::from(session.is_some());
        let after = out[first..]
            .iter()
            .position(|r| r.depth == 0 && r.kind.order() > 1)
            .map_or(out.len(), |i| first + i);
        let mine = held.iter().filter(|h| held_in(h) == session).map(|h| Row {
            flagged: true,
            ..bare(
                Kind::Held(h.name.clone(), h.root.clone()),
                Kind::Lsp(0),
                &h.name,
                crate::allow::tilde(&h.root),
                session,
            )
        });
        out.splice(after..after, mine.collect::<Vec<_>>());
        // The heading: what's under it, summed
        if session.is_some() {
            let (head, under) = out.split_at_mut(start + 1);
            let (h, live) = (&mut head[start], under.iter().filter(|r| r.pid != 0));
            h.cpu = live.clone().filter_map(|r| r.cpu).reduce(|a, b| a + b);
            h.rss = live.clone().map(|r| r.rss).sum();
            h.footprint = live.clone().any(|r| r.footprint.is_some()).then(|| live.map(Row::memory).sum());
        }
    }
    out
}

/// The runs waiting in line (`Editor::procs_queued`: session, id, what, where in line) into the list, each
/// after its session's processes (with `groups`, under its heading) — no process, so no pid, CPU or
/// memory (a run waiting was nowhere to be seen, and taken for done — twice).
pub fn with_queued(rows: &mut Vec<Row>, queued: &[(SessionId, u64, String, String)], groups: &[Group]) {
    for (session, id, title, what) in queued {
        let session = groups.iter().any(|g| g.id == *session).then_some(*session);
        let at = rows.iter().rposition(|r| r.session == session).map_or(rows.len(), |i| i + 1);
        let row = Row {
            pid: 0,
            ppid: 0,
            started: 0,
            depth: 0,
            kind: Kind::Queued(*id),
            under: Kind::Task,
            title: title.clone(),
            what: what.clone(),
            program: String::new(),
            up: None,
            cpu: None,
            rss: 0,
            footprint: None,
            zombie: false,
            flagged: false,
            session,
            left: false,
        };
        rows.insert(at, row);
    }
}

/// CPU % and memory of the processes `pids` among tarae's children, each with what it started — None when
/// none of them is in the list.
pub fn summed(rows: &[Row], pids: &[u32]) -> Option<(f32, u64)> {
    let mut sum: Option<(f32, u64)> = None;
    let mut inside = false;
    for r in rows {
        if r.depth == 0 {
            inside = r.kind != Kind::Tarae && pids.contains(&r.pid);
        }
        if inside {
            let (cpu, mem) = sum.get_or_insert((0.0, 0));
            (*cpu, *mem) = (*cpu + r.cpu.unwrap_or(0.0), *mem + r.memory());
        }
    }
    sum
}

// ── Claude's tool ────────────────────────────────────────────────────────

/// How long the tool's CPU shares are measured over (two looks this far apart).
const TOOL_WINDOW: Duration = Duration::from_millis(300);
/// A command line longer than this is cut (a server's long classpath).
const COMMAND_MAX: usize = 400;

pub fn tool_schema() -> serde_json::Value {
    serde_json::json!({
        "name": "procs",
        "description": "The processes the user's editor runs for this session, as its :procs panel shows them — \
    language servers, your own chat process, commands and tests running, the terminal, what each started — with \
    pid, parent, age, CPU (share of a core over the last 0.3 s), memory (physical footprint, which counts \
    compressed pages that RSS hides; resident beside it), command line and working folder. Read-only, no card: \
    use it instead of ps, pgrep or lsof through run. all: every session's, and what they share.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "all": { "type": "boolean", "description": "Every session's processes, not only this session's" }
            }
        }
    })
}

/// A `procs` call (#97: Claude ran ps, pgrep and lsof through `run`, a card each): two looks
/// `TOOL_WINDOW` apart on a worker, the rows `:procs` makes of them, each process's command line and
/// folder — answered with `report`.
pub fn tool_call(ed: &mut Editor, call: crate::tools::Call, args: &serde_json::Value) {
    let all = args["all"].as_bool() == Some(true);
    let (known, groups, held) = (ed.procs_known(), ed.procs_groups(), ed.procs.held.clone());
    let queued = ed.procs_queued();
    let (me, name) = (ed.session.id, ed.session.label().to_string());
    ed.events.jobs().spawn(move || {
        let mut s = Sampler::default();
        s.sample(std::process::id());
        std::thread::sleep(TOOL_WINDOW);
        let sample = s.sample(std::process::id());
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let mut rows = rows(&sample, &known, std::process::id(), &held, &groups, now);
        with_queued(&mut rows, &queued, &groups);
        let mine = |r: &Row| all || groups.is_empty() || r.session == Some(me);
        let shown: Vec<Row> = rows.into_iter().filter(mine).collect();
        let extra =
            shown.iter().filter(|r| r.pid > 0).map(|r| (r.pid, (command(r.pid), cwd(r.pid)))).collect();
        let scope = match (all, groups.is_empty()) {
            (false, false) => format!("session {name}"),
            _ => "every session".to_string(),
        };
        let text = report(&shown, &extra, &scope);
        move |ed: &mut Editor| crate::tools::answer(ed, call, Ok(text))
    });
}

/// What the tool answers: a line on what's measured, then each row as `:procs` lists it — a session's
/// heading with its sums; a process indented under what started it, with its pid, parent, age, CPU,
/// memory (footprint, resident beside it), command line and folder (`extra`); a held server said so.
pub fn report(rows: &[Row], extra: &HashMap<u32, (Option<String>, Option<PathBuf>)>, scope: &str) -> String {
    let footprint = rows.iter().any(|r| r.footprint.is_some());
    let memory = match footprint {
        true => {
            "memory = physical footprint (what Activity Monitor shows — RSS leaves compressed pages out), resident beside it"
        }
        false => "memory = resident (no footprint on this system)",
    };
    let window = TOOL_WINDOW.as_secs_f32();
    let mut out = format!(
        "The editor's processes for {scope}, as :procs lists them — cpu = share of a core over the last {window:.1} s; {memory}.\n"
    );
    if rows.is_empty() {
        out.push_str("\nNone running.\n");
        return out;
    }
    let mem = |r: &Row| match r.footprint {
        Some(f) => format!("{} (resident {})", self::mem(f), self::mem(r.rss)),
        None => self::mem(r.rss),
    };
    let cpu = |r: &Row| r.cpu.map_or("—".to_string(), |c| format!("{c:.1}%"));
    for r in rows {
        let pad = "  ".repeat(r.depth);
        let head = match r.what.is_empty() {
            true => r.title.clone(),
            false => format!("{} — {}", r.title, r.what),
        };
        match (&r.kind, r.pid) {
            (Kind::Session(_), _) => {
                out.push_str(&format!("\n── {head}: cpu {} · memory {}\n", cpu(r), mem(r)));
            }
            (Kind::Queued(_), _) => out.push_str(&format!(
                "\n{pad}{head}: queued, not started — it runs when the one before it ends\n"
            )),
            (_, 0) => out.push_str(&format!(
                "\n{pad}{head}: held — it exited again and again; not started until r on it in :procs\n"
            )),
            _ => {
                let up = r.up.map_or("—".to_string(), age);
                let ended = if r.zombie { " · ended, not reaped" } else { "" };
                out.push_str(&format!(
                    "\n{pad}{head}\n{pad}  pid {} · parent {} · up {up} · cpu {} · memory {}{ended}\n",
                    r.pid,
                    r.ppid,
                    cpu(r),
                    mem(r)
                ));
                let (cmd, dir) = extra.get(&r.pid).cloned().unwrap_or_default();
                if let Some(cmd) = cmd {
                    let cut = match cmd.chars().count() > COMMAND_MAX {
                        true => format!("{}…", cmd.chars().take(COMMAND_MAX).collect::<String>()),
                        false => cmd,
                    };
                    out.push_str(&format!("{pad}  command: {cut}\n"));
                }
                if let Some(dir) = dir {
                    out.push_str(&format!("{pad}  folder: {}\n", crate::allow::tilde(&dir)));
                }
            }
        }
    }
    out
}

// ── Checks ───────────────────────────────────────────────────────────────

/// A language server as the checks see it.
pub struct LspFact {
    pub cid: ClientId,
    pub pid: Option<u32>,
    pub name: String,
    /// A file open uses it.
    pub serving: bool,
    /// Its oldest work in progress: (since, what).
    pub busy: Option<(Instant, String)>,
}

/// What a check makes tarae do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Act {
    Nothing,
    StopServer(ClientId),
}

/// Something abnormal now.
#[derive(Clone, Debug, PartialEq)]
pub struct Finding {
    /// One per thing (the same thing found again isn't another).
    pub key: String,
    pub pid: u32,
    /// What it is, in a sentence · the status line's few words · what tarae did about it.
    pub what: String,
    pub short: String,
    pub did: String,
    pub act: Act,
}

/// A self-check, as recorded: what was abnormal, when, what tarae did about it, when it ended.
#[derive(Clone, Debug)]
pub struct Check {
    key: String,
    pub what: String,
    pub short: String,
    pub did: String,
    pub at: SystemTime,
    pub ended: Option<SystemTime>,
}

/// A server that exited again and again, not started again until `r`.
#[derive(Clone, Debug, PartialEq)]
pub struct Held {
    pub name: String,
    pub root: PathBuf,
    what: String,
}

/// Programs' usual memory: their last runs' first-minutes peaks (bytes), newest last.
pub type Usual = HashMap<String, Vec<u64>>;

fn load_usual(path: &Path) -> Usual {
    std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// The median of `program`'s kept peaks.
fn usual_of(usual: &Usual, program: &str) -> Option<u64> {
    let mut v = usual.get(program)?.clone();
    crate::sorting::sort(&mut v);
    v.get(v.len() / 2).copied()
}

/// The panel's state while `:procs` is open.
pub struct Panel {
    pub selected: usize,
    /// The first row in sight (drawing keeps the selected one in sight).
    pub scroll: std::cell::Cell<usize>,
    pub focus: bool,
    /// The tallest it's been (`term::procs_panel_h` — it doesn't shrink while it's open).
    pub tall: std::cell::Cell<usize>,
}

/// The panel as drawn last frame: its first screen line and its last, its first row's line, how many rows
/// show, the first row shown.
#[derive(Clone, Copy, Debug)]
pub struct Area {
    pub top: usize,
    pub bottom: usize,
    pub first: usize,
    pub shown: usize,
    pub scroll: usize,
}

/// tarae's processes: the last look, the checks' memory, the panel.
#[derive(Default)]
pub struct Procs {
    /// To the sampling thread (None in tests — a look is then a one-off job).
    wake: Option<Sender<Wake>>,
    pub panel: Option<Panel>,
    pub rows: Vec<Row>,
    /// What the last look cost · when it was (the UI state says how long ago — #143).
    pub took: Option<Duration>,
    pub looked: Option<Instant>,
    pub checks: Vec<Check>,
    /// cpu ≥ `BUSY_PCT` since · the most it took so far in its first `SETTLE` (and whether that's kept) ·
    /// servers serving no file since · zombies seen last look.
    busy: HashMap<(u32, u64), Instant>,
    peak: HashMap<(u32, u64), (u64, bool)>,
    unused: HashMap<ClientId, Instant>,
    zombies: HashSet<u32>,
    pub usual: Usual,
    usual_path: Option<PathBuf>,
    /// Sessions' kept turns are being read back (`Editor::restoring`): tarae's own memory is neither said nor
    /// learned meanwhile — a restore's reading isn't what tarae usually takes (#218: "705 MB, usually 96 MB").
    pub restoring: bool,
    /// Servers' exits (name, root, when) · the ones held.
    exits: Vec<(String, PathBuf, Instant)>,
    pub held: Vec<Held>,
    /// The mark the status line last had (a look that changes it is drawn).
    marked: Option<String>,
    /// The file systems tarae writes to, at the last look (`volumes`).
    pub free: Vec<Volume>,
}

/// `412 KB` · `48 MB` · `1.4 GB`.
pub fn mem(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= (1u64 << 30) as f64 {
        format!("{:.1} GB", b / (1u64 << 30) as f64)
    } else if b >= (1u64 << 20) as f64 {
        format!("{:.0} MB", b / (1u64 << 20) as f64)
    } else {
        format!("{:.0} KB", b / 1024.0)
    }
}

/// `42s` · `12m` · `3h 05m` · `2d 04h`.
pub fn age(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h {:02}m", s / 3600, s % 3600 / 60),
        _ => format!("{}d {:02}h", s / 86400, s % 86400 / 3600),
    }
}

impl Procs {
    /// What's abnormal in this look (`now`: when it was taken) — and the checks' memory brought up to it.
    /// true in `.1`: a program's usual memory was learned (to be kept).
    pub fn check(&mut self, rows: &[Row], lsp: &[LspFact], now: Instant) -> (Vec<Finding>, bool) {
        let mut out = Vec::new();
        let mut learned = false;
        let seen: HashSet<(u32, u64)> = rows.iter().map(Row::key).collect();
        self.busy.retain(|k, _| seen.contains(k));
        self.peak.retain(|k, _| seen.contains(k));
        // A server's work in progress for minutes (its own words say what)
        for f in lsp {
            if let Some((since, doing)) = &f.busy
                && now.saturating_duration_since(*since) >= BUSY_FOR
            {
                let long = age(now.saturating_duration_since(*since));
                out.push(Finding {
                    key: format!("busy:server:{}", f.cid),
                    pid: f.pid.unwrap_or(0),
                    what: format!("{} busy for {long} ({doing})", f.name),
                    short: format!("{} busy {long}", f.name),
                    did: "nothing yet — r restarts it".into(),
                    act: Act::Nothing,
                });
            }
        }
        for r in rows.iter().filter(|r| r.pid != 0) {
            let ours = !r.under.users();
            // Busy: most of a core, every look for minutes
            if r.cpu.is_some_and(|c| c >= BUSY_PCT) {
                self.busy.entry(r.key()).or_insert(now);
            } else {
                self.busy.remove(&r.key());
            }
            if ours
                && let Some(since) = self.busy.get(&r.key())
                && now.saturating_duration_since(*since) >= BUSY_FOR
                && !out.iter().any(|f| f.pid == r.pid)
            {
                let long = age(now.saturating_duration_since(*since));
                out.push(Finding {
                    key: format!("busy:{}:{}", r.pid, r.started),
                    pid: r.pid,
                    what: format!("{} at {:.0}% CPU for {long}", r.title, r.cpu.unwrap_or(0.0)),
                    short: format!("{} busy {long}", r.title),
                    did: if r.kind.restarts() {
                        "nothing yet — r restarts it"
                    } else {
                        "nothing yet — s stops it"
                    }
                    .into(),
                    act: Act::Nothing,
                });
            }
            // Memory: its first minutes' peak is learned; far above its program's usual is said
            let m = r.memory();
            if self.restoring && r.kind == Kind::Tarae {
                continue;
            }
            match r.up {
                Some(up) if up < SETTLE => {
                    let p = self.peak.entry(r.key()).or_insert((0, false));
                    p.0 = p.0.max(m);
                }
                Some(_) => {
                    if let Some(p) = self.peak.get_mut(&r.key()).filter(|p| !p.1 && p.0 > 0) {
                        p.1 = true;
                        let v = self.usual.entry(r.program.clone()).or_default();
                        v.push(p.0);
                        let n = v.len();
                        v.drain(..n.saturating_sub(USUAL_KEPT));
                        learned = true;
                    }
                }
                None => {}
            }
            if ours
                && let Some(u) = usual_of(&self.usual, &r.program)
                && m >= u.saturating_mul(MEMORY_TIMES)
                && m >= u.saturating_add(MEMORY_OVER)
            {
                let did = match r.kind {
                    Kind::Tarae => "nothing — :restart starts it afresh",
                    ref k if k.restarts() => "nothing yet — r restarts it",
                    _ => "nothing yet — s stops it",
                };
                out.push(Finding {
                    key: format!("memory:{}:{}", r.pid, r.started),
                    pid: r.pid,
                    what: format!("{} at {} — usually {}", r.title, mem(m), mem(u)),
                    short: format!("{} {}", r.title, mem(m)),
                    did: did.into(),
                    act: Act::Nothing,
                });
            }
            // A child of tarae's it doesn't know, running long; one nobody waited for
            if r.depth == 0
                && r.kind == Kind::Other
                && !r.left
                && !r.zombie
                && r.up.is_some_and(|u| u >= STRAY_AFTER)
            {
                out.push(Finding {
                    key: format!("stray:{}:{}", r.pid, r.started),
                    pid: r.pid,
                    what: format!(
                        "{} (pid {}), started by tarae but not one it knows, running {}",
                        r.title,
                        r.pid,
                        age(r.up.unwrap_or_default())
                    ),
                    short: format!("{} running", r.title),
                    did: "nothing — s stops it".into(),
                    act: Act::Nothing,
                });
            }
            if r.depth == 0 && r.zombie && self.zombies.contains(&r.pid) {
                out.push(Finding {
                    key: format!("zombie:{}:{}", r.pid, r.started),
                    pid: r.pid,
                    what: format!("{} (pid {}) ended, and tarae didn't wait for it", r.title, r.pid),
                    short: format!("{} not reaped", r.title),
                    did: if cfg!(target_os = "linux") {
                        "nothing yet — reaped once nobody has for a while"
                    } else {
                        "nothing — it goes when tarae does"
                    }
                    .into(),
                    act: Act::Nothing,
                });
            }
        }
        self.zombies = rows.iter().filter(|r| r.depth == 0 && r.zombie).map(|r| r.pid).collect();
        // A server with no file of it open, past its own stop
        let left = crate::lsp_editor::IDLE_STOP + LEFT_GRACE;
        self.unused.retain(|cid, _| lsp.iter().any(|f| f.cid == *cid));
        for f in lsp {
            if f.serving {
                self.unused.remove(&f.cid);
                continue;
            }
            let since = *self.unused.entry(f.cid).or_insert(now);
            if now.saturating_duration_since(since) >= left {
                out.push(Finding {
                    key: format!("left:{}", f.cid),
                    pid: f.pid.unwrap_or(0),
                    what: format!(
                        "{} kept running with no file of it open for {}",
                        f.name,
                        age(now.saturating_duration_since(since))
                    ),
                    short: format!("{} left running", f.name),
                    did: "stopped it".into(),
                    act: Act::StopServer(f.cid),
                });
            }
        }
        for h in &self.held {
            out.push(Finding {
                key: format!("loop:{}:{}", h.name, h.root.display()),
                pid: 0,
                what: h.what.clone(),
                short: format!("{} keeps exiting", h.name),
                did: "not started again — r in :procs starts it".into(),
                act: Act::Nothing,
            });
        }
        (out, learned)
    }

    /// The checks recorded as `found` says (a new one is logged; one no longer found has ended). The new ones.
    pub fn record(&mut self, found: &[Finding], at: SystemTime) -> Vec<Finding> {
        let mut new = Vec::new();
        for f in found {
            if !self.checks.iter().any(|c| c.key == f.key && c.ended.is_none()) {
                crate::log::line("procs", "check", &format!("{} — {}", f.what, f.did));
                self.checks.push(Check {
                    key: f.key.clone(),
                    what: f.what.clone(),
                    short: f.short.clone(),
                    did: f.did.clone(),
                    at,
                    ended: None,
                });
                new.push(f.clone());
            } else if let Some(c) = self.checks.iter_mut().find(|c| c.key == f.key && c.ended.is_none()) {
                // (still so: its words follow — "busy 6m" → "busy 7m")
                c.what.clone_from(&f.what);
                c.short.clone_from(&f.short);
            }
        }
        for c in self.checks.iter_mut().filter(|c| c.ended.is_none()) {
            if !found.iter().any(|f| f.key == c.key) {
                crate::log::line("procs", "check", &format!("ended: {}", c.what));
                c.ended = Some(at);
            }
        }
        // (ended a while ago: gone from the list — the log has it)
        self.checks.retain(|c| c.ended.is_none_or(|e| at.duration_since(e).unwrap_or_default() < ENDED_KEPT));
        let n = self.checks.len();
        if n > CHECKS_KEPT {
            let mut over = n - CHECKS_KEPT;
            self.checks.retain(|c| {
                let drop = over > 0 && c.ended.is_some();
                over -= usize::from(drop);
                !drop
            });
        }
        new
    }

    /// What the status line says: the newest check that still holds.
    pub fn mark(&self) -> Option<&Check> {
        self.checks.iter().rev().find(|c| c.ended.is_none())
    }

    /// The checks that hold now, oldest first: what each found, and what tarae did.
    pub fn holding(&self) -> Vec<String> {
        self.checks.iter().filter(|c| c.ended.is_none()).map(|c| format!("{} — {}", c.what, c.did)).collect()
    }

    /// A server exited: the third time within `CRASH_WINDOW`, it's held. The check, if it was.
    fn exited(&mut self, name: &str, root: &Path, now: Instant) -> Option<Finding> {
        self.exits.retain(|e| now.saturating_duration_since(e.2) < CRASH_WINDOW);
        self.exits.push((name.to_string(), root.to_path_buf(), now));
        let n = self.exits.iter().filter(|e| e.0 == name && e.1 == root).count();
        if n < CRASHES || self.is_held(name, root) {
            return None;
        }
        let what =
            format!("{name} for {} exited {n} times in {}", crate::allow::tilde(root), age(CRASH_WINDOW));
        self.held.push(Held { name: name.to_string(), root: root.to_path_buf(), what: what.clone() });
        Some(Finding {
            key: format!("loop:{name}:{}", root.display()),
            pid: 0,
            what,
            short: format!("{name} keeps exiting"),
            did: "not started again — r in :procs starts it".into(),
            act: Act::Nothing,
        })
    }

    pub fn is_held(&self, name: &str, root: &Path) -> bool {
        self.held.iter().any(|h| h.name == name && h.root == root)
    }
}

// ── The editor's side ────────────────────────────────────────────────────

impl Editor {
    /// The sampling thread, from the event loop (not in tests).
    pub fn procs_start_sampling(&mut self) {
        let path = crate::config::state_dir().map(|d| d.join("procs.json"));
        self.procs.usual_path.clone_from(&path);
        self.procs.wake = Some(start(self.events.shared_sender(), path));
    }

    /// Whether the panel is up (it takes the bottom slot first).
    pub fn procs_up(&self) -> bool {
        self.procs.panel.is_some()
    }

    /// `:procs` — the panel, with the keys, looked at every second while it's open; again: closed.
    pub fn procs_toggle(&mut self) {
        if self.procs.panel.as_ref().is_some_and(|p| p.focus) {
            return self.procs_close();
        }
        match self.procs.panel.as_mut() {
            Some(p) => p.focus = true,
            None => {
                self.procs.panel = Some(Panel {
                    selected: 0,
                    scroll: Default::default(),
                    focus: true,
                    tall: Default::default(),
                });
                self.procs_wake(Wake::Open(true));
            }
        }
        self.procs_took_keys();
    }

    pub fn procs_close(&mut self) {
        self.procs.panel = None;
        self.procs_wake(Wake::Open(false));
    }

    /// The panel took the keys: nothing else keeps them.
    fn procs_took_keys(&mut self) {
        if let Some(c) = self.session.chat.as_mut() {
            c.focused = false;
        }
        if let Some(s) = self.session.sidebar.as_mut() {
            s.focus = false;
        }
        if let Some(t) = self.session.terminal.as_mut() {
            t.focus = false;
        }
        self.bottom_focus = false;
        self.plugin_panel_unfocus();
    }

    /// Whether the panel has the keys.
    pub fn procs_focused(&self) -> bool {
        self.procs.panel.as_ref().is_some_and(|p| p.focus)
    }

    /// The panel gives the keys back (the code was clicked, another place took them).
    pub fn procs_unfocus(&mut self) {
        if let Some(p) = self.procs.panel.as_mut() {
            p.focus = false;
        }
    }

    /// A click on the panel's row `row` (None: its header): the keys, that row selected.
    pub fn procs_click(&mut self, row: Option<usize>) {
        let n = self.procs.rows.len();
        let Some(p) = self.procs.panel.as_mut() else { return };
        p.focus = true;
        if let Some(r) = row.filter(|&r| r < n) {
            p.selected = r;
        }
        self.procs_took_keys();
    }

    pub(crate) fn procs_wake(&mut self, w: Wake) {
        let open = matches!(w, Wake::Open(true) | Wake::Now);
        if let Some(tx) = &self.procs.wake {
            let _ = tx.send(w);
            return;
        }
        // No thread (tests): a one-off look on a worker
        if open {
            self.events.jobs().spawn(|| {
                let s = Sampler::default().sample(std::process::id());
                move |ed: &mut Editor| ed.procs_sampled(s)
            });
        }
    }

    /// The processes the editor knows, what each is for, and — with sessions — whose: a session's chat,
    /// terminal, task and tests, and the servers rooted in its checkout.
    pub fn procs_known(&self) -> Vec<Known> {
        let mut out = Vec::new();
        let mut add =
            |pid: Option<u32>, kind: Kind, title: &str, what: String, session: Option<SessionId>| {
                if let Some(pid) = pid.filter(|&p| p > 0) {
                    out.push(Known { pid, kind, title: title.to_string(), what, session, at: None });
                }
            };
        let sessions = !self.sessions.is_empty();
        for c in &self.lsp.clients {
            // (a shared server — several checkouts, or one being swapped — is no one session's)
            match self.share_procs(c) {
                Some(what) => add(c.pid(), Kind::Lsp(c.id), &c.name, what, None),
                None => {
                    let session = self.session_of(&c.root).filter(|_| sessions);
                    add(c.pid(), Kind::Lsp(c.id), &c.name, crate::allow::tilde(&c.root), session);
                }
            }
        }
        if let Some(d) = &self.dap {
            let (adapter, helper) = d.pids();
            add(adapter, Kind::Debug, "debugger", d.program.clone(), None);
            add(helper, Kind::Helper, "debug helper", d.program.clone(), None);
        }
        for l in std::iter::once(&self.session).chain(&self.sessions) {
            let session = sessions.then_some(l.id);
            if let Some(r) = &l.task_run {
                let title = if r.by_claude { "claude's command" } else { r.task.name.as_str() };
                add(r.pid(), Kind::Task, title, r.task.cmd.clone(), session);
                // (its sandbox's `log stream`, watching what it blocks — a stray child otherwise, #E16)
                let watch = r.guard.as_ref().and_then(|g| g.watch()).and_then(|w| w.pid());
                add(
                    watch,
                    Kind::Helper,
                    "sandbox watch",
                    format!("what the sandbox blocks: {}", r.task.cmd),
                    session,
                );
            }
            if let Some(r) = &l.test_run {
                add(r.pid(), Kind::Test, "tests", r.target.label.clone(), session);
                let watch = r.guard.as_ref().and_then(|g| g.watch()).and_then(|w| w.pid());
                add(
                    watch,
                    Kind::Helper,
                    "sandbox watch",
                    format!("what the sandbox blocks: tests {}", r.target.label),
                    session,
                );
            }
            if let Some(t) = &l.terminal {
                add(t.pid(), Kind::Terminal, "terminal", t.title.clone(), session);
            }
            if let Some(c) = &l.chat {
                add(c.pid(), Kind::Chat, "claude", "the chat".into(), session);
            }
            if let Some(r) = &l.reviewer {
                add(r.pid(), Kind::Reviewer, "claude", format!("the reviewer · {}", r.label), session);
            }
        }
        // (a watch held elsewhere — its run gone, not yet started, or never to: still tarae's, still known)
        for w in crate::sandbox::live_watches() {
            if let Some(pid) = w.pid().filter(|p| !out.iter().any(|k| k.pid == *p)) {
                out.push(Known {
                    pid,
                    kind: Kind::Helper,
                    title: "sandbox watch".into(),
                    what: "what the sandbox blocks — its run not in a panel".into(),
                    session: None,
                    at: None,
                });
            }
        }
        // (each session's keeper: its sandboxed runs, and what they left, under it — `sandbox::keeper`)
        #[cfg(target_os = "linux")]
        for k in crate::sandbox::keeper::of(None).into_iter().filter(|k| !k.is_gone()) {
            let of = self.session_of(&k.root);
            let g = std::iter::once(&self.session)
                .chain(&self.sessions)
                .find(|s| of == Some(s.id))
                .and_then(|s| s.sandbox.as_ref());
            out.push(Known {
                pid: k.pid,
                kind: Kind::Helper,
                title: "sandbox keeper".into(),
                what: keeper_what(g, || k.said(), &k.root),
                session: of.filter(|_| sessions),
                at: None,
            });
        }
        // (a session's end's own command — its session gone by then)
        for (pid, cmd) in crate::ended::running() {
            out.push(Known {
                pid,
                kind: Kind::Helper,
                title: "session.ended".into(),
                what: cmd,
                session: None,
                at: None,
            });
        }
        let mut add =
            |pid: Option<u32>, kind: Kind, title: &str, what: String, session: Option<SessionId>| {
                if let Some(pid) = pid.filter(|&p| p > 0) {
                    out.push(Known { pid, kind, title: title.to_string(), what, session, at: None });
                }
            };
        add(self.llm_pid, Kind::Ask, "claude", "space i".into(), None);
        add(self.llm_warm.pid(), Kind::Spare, "claude", "ready for space i".into(), None);
        for (plugin, id, pid, program) in self.plugin_pids() {
            add(Some(pid), Kind::Plugin(plugin.clone(), id), &program, format!("plugin {plugin}"), None);
        }
        // What a session started that outlived its run (closed, replaced): still its session's (#100)
        for l in std::iter::once(&self.session).chain(&self.sessions) {
            let session = sessions.then_some(l.id);
            for s in l.spawned.iter() {
                if out.iter().any(|k| k.pid == s.pid) {
                    continue;
                }
                out.push(Known {
                    pid: s.pid,
                    kind: Kind::Other,
                    title: format!("{} · left running", s.title),
                    what: s.what.clone(),
                    session,
                    at: Some(s.at),
                });
            }
        }
        out
    }

    /// The sessions as `:procs` heads them, in the tabs' order — none while there's one.
    fn procs_groups(&self) -> Vec<Group> {
        if self.sessions.is_empty() {
            return Vec::new();
        }
        let group = |l: &crate::session::Session| Group {
            id: l.id,
            name: l.label().to_string(),
            root: l.root.clone(),
        };
        self.sessions_in_order().into_iter().map(group).collect()
    }

    fn procs_lsp_facts(&self) -> Vec<LspFact> {
        self.lsp
            .clients
            .iter()
            .map(|c| LspFact {
                cid: c.id,
                pid: c.pid(),
                name: c.name.clone(),
                serving: self.docs.iter().any(|d| d.lsp.clients().any(|x| x == c.id)),
                busy: c
                    .progress
                    .values()
                    .filter_map(|p| Some((p.began?, p.title.clone())))
                    .min_by_key(|(at, _)| *at),
            })
            .collect()
    }

    /// A look arrived: the list, the checks — and what they do. Nothing is drawn for it unless the panel is
    /// up or the status line's mark changed.
    pub fn procs_sampled(&mut self, s: Sample) {
        if let Some(u) = s.usual.clone() {
            // (kept ones first; what this run of tarae learned before they came stays)
            for (k, v) in u {
                self.procs.usual.entry(k).or_insert(v);
            }
        }
        let now = SystemTime::now();
        let secs = now.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        // (what a session started and is gone: forgotten — a later process given its pid isn't it)
        let alive = |sp: &Spawned| {
            s.procs.iter().any(|(o, _)| o.pid == sp.pid && o.started.abs_diff(sp.at) <= 2 && !o.zombie)
        };
        for l in std::iter::once(&mut self.session).chain(&mut self.sessions) {
            l.spawned.retain(alive);
        }
        let groups = self.procs_groups();
        let mut rows = rows(&s, &self.procs_known(), std::process::id(), &self.procs.held, &groups, secs);
        with_queued(&mut rows, &self.procs_queued(), &groups);
        self.procs.restoring = self.restoring();
        let (mut found, learned) = self.procs.check(&rows, &self.procs_lsp_facts(), Instant::now());
        found.extend(disk_check(&s.free));
        self.procs.free = s.free.clone();
        // (a run of Claude's printing nothing at next to no CPU — `stuck.rs`)
        crate::stuck::look(self, &rows, Instant::now());
        let flagged: HashSet<u32> = found.iter().map(|f| f.pid).filter(|&p| p > 0).collect();
        let held_before = self.procs.holding();
        self.procs_found(found, now);
        if self.ui_watch.watching() {
            self.procs_said(&rows, &held_before);
        }
        for r in &mut rows {
            r.flagged |= flagged.contains(&r.pid);
        }
        // The selection stays on its process (its heading, its held server) as rows come and go
        let was = self.procs.panel.as_ref().and_then(|p| self.procs.rows.get(p.selected));
        let now = was.and_then(|w| rows.iter().position(|r| w.same(r)));
        if let Some(p) = self.procs.panel.as_mut() {
            p.selected = now.unwrap_or(p.selected).min(rows.len().saturating_sub(1));
        }
        self.procs.rows = rows;
        self.procs.took = Some(s.took);
        self.procs.looked = Some(Instant::now());
        if learned {
            self.procs_keep_usual();
        }
        let up = self.procs_up();
        self.unchanged = !up && !self.procs_mark_changed();
    }

    /// For watchers (`--watch procs`, #143): what a look found changed since the one before — processes
    /// started and ended, checks that came or went. Said only when something did: a look is every
    /// `QUIET_EVERY` (each second while `:procs` is open), and most find nothing new.
    fn procs_said(&mut self, rows: &[Row], held_before: &[String]) {
        let name = |r: &Row| match r.what.is_empty() {
            true => format!("{} ({})", r.title, r.pid),
            false => format!("{} ({}) — {}", r.title, r.pid, r.what),
        };
        let live =
            |rs: &[Row]| -> Vec<(u32, u64)> { rs.iter().filter(|r| r.pid != 0).map(Row::key).collect() };
        let (was, now) = (live(&self.procs.rows), live(rows));
        // (the first look has nothing before it: everything would be "started")
        if self.procs.looked.is_none() {
            return;
        }
        let started: Vec<String> =
            rows.iter().filter(|r| r.pid != 0 && !was.contains(&r.key())).map(name).collect();
        let ended: Vec<String> =
            self.procs.rows.iter().filter(|r| r.pid != 0 && !now.contains(&r.key())).map(name).collect();
        let holding = self.procs.holding();
        let found: Vec<&String> = holding.iter().filter(|c| !held_before.contains(c)).collect();
        let over: Vec<&String> = held_before.iter().filter(|c| !holding.contains(c)).collect();
        if started.is_empty() && ended.is_empty() && found.is_empty() && over.is_empty() {
            return;
        }
        let mut said = Vec::new();
        for (word, list) in [("started", &started), ("ended", &ended)] {
            if !list.is_empty() {
                said.push(format!("{word}: {}", list.join(", ")));
            }
        }
        said.extend(found.iter().map(|c| format!("▲ {c}")));
        said.extend(over.iter().map(|c| format!("no longer: {c}")));
        self.ui_said(serde_json::json!({ "event": "procs", "started": started, "ended": ended,
            "found": found, "over": over, "said": format!("procs — {}", said.join(" · ")) }));
    }

    /// The status line's mark isn't what was drawn last.
    fn procs_mark_changed(&mut self) -> bool {
        let now = self.procs.mark().map(|c| c.short.clone());
        let was = std::mem::replace(&mut self.procs.marked, now.clone());
        was != now
    }

    /// Checks recorded; what the new ones say to do, done.
    fn procs_found(&mut self, found: Vec<Finding>, at: SystemTime) {
        for f in self.procs.record(&found, at) {
            // (once each time it gets there: a card that waits for a key, not only the status line's mark)
            if f.key.starts_with("disk-full:") {
                // (what matters first: under a newer toast it's one row, cut; whole in a narrow card's 3 rows)
                self.set_error(format!(
                    "Disk almost full: {}. space W lists build folders; x drops an idle one",
                    f.what
                ));
            }
            if let Act::StopServer(cid) = f.act {
                let log = self.server_log(cid);
                if let Some(name) = self.lsp_stopped(cid) {
                    log.line(&name, "stop", &format!("{name} stopped by tarae's check: {}", f.what));
                }
            }
        }
    }

    /// A server exited (`lsp_exited`): the third time in a while, it's held — the next file of its project
    /// doesn't start it again until `r` in `:procs`.
    pub fn procs_server_exited(&mut self, name: &str, root: &Path) {
        if let Some(f) = self.procs.exited(name, root, Instant::now()) {
            // (said where it shows, not only at the next look)
            let found: Vec<Finding> = self
                .procs
                .checks
                .iter()
                .filter(|c| c.ended.is_none())
                .map(|c| Finding {
                    key: c.key.clone(),
                    pid: 0,
                    what: c.what.clone(),
                    short: c.short.clone(),
                    did: c.did.clone(),
                    act: Act::Nothing,
                })
                .chain(std::iter::once(f))
                .collect();
            self.procs_found(found, SystemTime::now());
            self.procs_wake(Wake::Now);
        }
    }

    /// Whether `name` for `root` is held (`lsp_spawn` doesn't start it).
    pub fn procs_held(&self, name: &str, root: &Path) -> bool {
        self.procs.is_held(name, root)
    }

    fn procs_selected(&self) -> Option<Row> {
        let p = self.procs.panel.as_ref()?;
        self.procs.rows.get(p.selected).cloned()
    }

    /// What `r` is called in a toast: from a session behind, its name first (`fix-login · claude`).
    fn procs_whose(&self, r: &Row, what: &str) -> String {
        let behind = r.session.filter(|&id| id != self.session.id);
        match behind.and_then(|id| self.sessions.iter().find(|l| l.id == id)) {
            Some(l) => format!("{} · {what}", l.label()),
            None => what.to_string(),
        }
    }

    /// `f` in the session `r` is under (swapped in if it's behind — `with_session`), the one in front for a
    /// shared row.
    fn procs_in(&mut self, r: &Row, f: impl FnOnce(&mut Editor)) {
        self.with_session(r.session.unwrap_or(self.session.id), f);
    }

    /// `s` on the selected row: it stops (the way its owner stops it — a run keeps its output).
    fn procs_stop(&mut self) {
        let Some(r) = self.procs_selected() else {
            return self.set_status("no row selected — nothing to stop");
        };
        let gone = |ed: &mut Editor, what: &str, then: Option<&str>| {
            ed.set_status(stopped(&ed.procs_whose(&r, what), then));
            ed.procs_wake(Wake::Now);
        };
        match r.kind {
            Kind::Tarae => self.set_status("that's tarae itself — :q quits it"),
            Kind::Held(..) => self.set_status(format!("{} isn't running — r starts it", r.title)),
            Kind::Session(_) => self.set_status(format!("{} is a session — :session-end ends it", r.title)),
            Kind::Queued(id) => {
                self.procs_in(&r, |ed| {
                    crate::queued::leave(ed, id, "the user took it out of the line in :procs")
                });
                self.procs_wake(Wake::Now);
            }
            Kind::Lsp(cid) => {
                let log = self.server_log(cid);
                if let Some(name) = self.lsp_stopped(cid) {
                    log.line(&name, "stop", &format!("{name} stopped from :procs"));
                    gone(self, &name, Some(SERVER_AGAIN));
                }
            }
            // (a sandbox's watch is a helper too — it stops alone, killed and reaped by its holder)
            Kind::Helper
                if let Some(w) =
                    crate::sandbox::live_watches().into_iter().find(|w| w.pid() == Some(r.pid)) =>
            {
                w.finish();
                gone(self, "the sandbox watch", None);
            }
            Kind::Debug | Kind::Helper => {
                self.dap_terminate();
                self.procs_wake(Wake::Now);
            }
            Kind::Task | Kind::Test | Kind::Plugin(..) => {
                crate::dap::term_group(r.pid);
                gone(self, &r.title, None);
            }
            Kind::Terminal => {
                // SAFETY: a hangup to the terminal's process group (`setsid`), which tarae started.
                unsafe { libc::kill(-(r.pid as i32), libc::SIGHUP) };
                gone(self, "the terminal", None);
            }
            Kind::Chat => {
                self.procs_in(&r, |ed| {
                    if let Some(c) = ed.session.chat.as_mut() {
                        c.stop_process();
                    }
                });
                gone(self, "claude", Some(CHAT_AGAIN));
            }
            Kind::Ask => {
                crate::llm::kill(r.pid);
                gone(self, "space i's claude", None);
            }
            Kind::Reviewer => {
                let pid = r.pid;
                self.procs_in(&r, |ed| crate::reviewer::stopped_by_procs(ed, pid));
                gone(self, "the reviewer", None);
            }
            Kind::Spare => {
                self.llm_warm = Default::default();
                gone(self, "the spare claude", None);
            }
            Kind::Other => {
                // (still the process the list showed: the same start, under tarae)
                stop_unknown(r.pid, r.started);
                gone(self, &format!("{} (pid {})", r.title, r.pid), None);
            }
        }
    }
}

/// A sandbox keeper's row: its session's last run's state line (`Guarantees::line` — the sandbox panel's words, 9e), else
/// its own tier and namespaces (`said`: `Keeper::said`) until that run's are known; then its checkout.
#[cfg(any(target_os = "linux", test))]
fn keeper_what(
    g: Option<&crate::sandbox::guarantees::Guarantees>,
    said: impl FnOnce() -> String,
    root: &Path,
) -> String {
    let state = g.map_or_else(said, |g| g.line());
    format!("the session's keeper · {state} · {}", crate::allow::tilde(root))
}

/// A process tarae started but doesn't know, stopped (`s`): SIGTERM while it's still the one listed (its
/// `started`) — and, tarae's own child, waited for on a thread of its own: nothing here holds it, so nothing
/// else would, and it'd stay a zombie until tarae exits (seen: a stray `log stream`, `<defunct>`).
fn stop_unknown(pid: u32, started: u64) {
    let Some(o) = read(pid).filter(|o| o.started == started) else { return };
    // SAFETY: SIGTERM to a descendant of tarae's, as read a moment ago.
    unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    if o.ppid == std::process::id() {
        crate::event::thread("tarae-reap", move || crate::spawn::wait_once(pid));
    }
}

impl Editor {
    /// `r` on the selected row: started again, where that means something.
    fn procs_restart(&mut self) {
        let Some(r) = self.procs_selected() else {
            return self.set_status("no row selected — nothing to restart");
        };
        match r.kind {
            Kind::Lsp(cid) => {
                let docs: Vec<_> =
                    self.docs.iter().filter(|d| d.lsp.clients().any(|c| c == cid)).map(|d| d.id).collect();
                let log = self.server_log(cid);
                let Some(name) = self.lsp_stopped(cid) else { return };
                log.line(&name, "stop", &format!("{name} restarted from :procs"));
                if docs.is_empty() {
                    self.set_status(format!("{name} stopped — no file uses it, so it starts with the next"));
                } else {
                    for id in docs {
                        self.attach_lsp(id);
                    }
                    self.set_success(format!("{name} restarted"));
                }
            }
            Kind::Held(name, root) => {
                self.procs.held.retain(|h| !(h.name == name && h.root == root));
                self.procs.exits.retain(|e| !(e.0 == name && e.1 == root));
                let docs: Vec<_> = self
                    .docs
                    .iter()
                    .filter(|d| d.path.as_deref().is_some_and(|p| p.starts_with(&root)))
                    .map(|d| d.id)
                    .collect();
                for id in docs {
                    self.attach_lsp(id);
                }
                self.set_success(format!("{name} let start again"));
            }
            Kind::Task => self.procs_in(&r, |ed| {
                match ed.session.task_run.as_ref().filter(|t| !t.by_claude && !t.install) {
                    Some(t) => {
                        let task = t.task.clone();
                        ed.task_start(task);
                    }
                    None => ed.set_status("Claude's command — Claude runs it again if it needs to"),
                }
            }),
            Kind::Test => self.procs_in(&r, Editor::test_rerun),
            Kind::Chat => self.procs_in(&r, crate::chat::restart_process),
            Kind::Session(_) => {
                return self.set_status(format!("{} is a session — r restarts a process under it", r.title));
            }
            _ => return self.set_status(format!("{} has no restart — s stops it", r.title)),
        }
        self.procs_wake(Wake::Now);
    }

    /// `l` on the selected row: what it said — a run's or the terminal's panel, the chat (another session's
    /// in that session, brought to the front), else tarae's log at its last line; on a session's heading,
    /// that session to the front.
    fn procs_log(&mut self) {
        let Some(r) = self.procs_selected() else { return };
        if let Kind::Session(id) = r.kind {
            self.session_switch(id);
            return;
        }
        if matches!(r.under, Kind::Task | Kind::Test | Kind::Terminal | Kind::Chat)
            && let Some(id) = r.session
        {
            self.session_switch(id);
        }
        match r.under {
            Kind::Task | Kind::Test => {
                self.procs_close();
                self.bottom_take_keys();
            }
            Kind::Terminal => {
                self.procs_close();
                self.terminal_focus();
            }
            Kind::Chat => crate::chat::open(self),
            _ => {
                let Some(path) = crate::log::path().filter(|p| p.exists()) else {
                    return self.set_status("no log yet");
                };
                // Its last line there (a plugin's program logs under the plugin's name)
                let source = match &r.kind {
                    Kind::Plugin(p, _) => p.clone(),
                    Kind::Lsp(_) | Kind::Held(..) => r.title.clone(),
                    _ => "procs".to_string(),
                };
                self.events.jobs().spawn(move || {
                    let text = std::fs::read_to_string(&path).unwrap_or_default();
                    let line = last_line_of(&text, &source);
                    move |ed: &mut Editor| {
                        if let Err(e) = ed.open_at(&path, line, 0) {
                            ed.set_error(format!("{e:#}"));
                        }
                    }
                });
            }
        }
    }

    /// The usual memory kept (worker).
    fn procs_keep_usual(&mut self) {
        let Some(path) = self.procs.usual_path.clone() else { return };
        let usual = self.procs.usual.clone();
        self.events.jobs().background(move || {
            if let Ok(s) = serde_json::to_string(&usual) {
                let tmp = path.with_extension("json.tmp");
                if std::fs::write(&tmp, s).is_ok() {
                    let _ = std::fs::rename(&tmp, &path);
                }
            }
        });
    }
}

/// What comes after a stopped server and the chat's claude: how they come back.
const SERVER_AGAIN: &str = "the next file of its project starts it again";
const CHAT_AGAIN: &str = "the chat's next message starts it again";

/// `s`'s word: `taplo stopped`, and how it comes back after it — `taplo stopped · the next file …`.
fn stopped(what: &str, then: Option<&str>) -> String {
    match then {
        Some(then) => format!("{what} stopped · {then}"),
        None => format!("{what} stopped"),
    }
}

/// The last line of the log from `source` (its column after the time) — else its end.
fn last_line_of(text: &str, source: &str) -> usize {
    let lines: Vec<&str> = text.lines().collect();
    lines
        .iter()
        .rposition(|l| l.get(10..).is_some_and(|rest| rest.split_whitespace().next() == Some(source)))
        .unwrap_or(lines.len().saturating_sub(1))
}

/// The panel's keys, while it has them: `j`/`k` (`g`/`G`, `C-d`/`C-u`) through the rows, `s` stop, `r`
/// restart, `l` its log, `q` close, Esc back to the code (it stays, looking every second). The space menu,
/// `:` and `C-w` stay the editor's. true if taken.
pub fn key(ed: &mut Editor, key: Key) -> bool {
    if ed.mode != Mode::Normal || !ed.pending.is_empty() || ed.prompt.is_some() {
        return false;
    }
    let n = ed.procs.rows.len();
    let Some(p) = ed.procs.panel.as_mut().filter(|p| p.focus) else { return false };
    if let Some(m) = crate::key::list_move(key) {
        p.selected = m.from(p.selected, n);
        return true;
    }
    match (key.code, key.ctrl, key.alt) {
        (Code::Char(' ' | ':'), false, false) | (Code::Char('w'), true, false) => return false,
        (Code::Esc, ..) => p.focus = false,
        (Code::Char('q'), false, false) => ed.procs_close(),
        (Code::Char('s'), false, false) => ed.procs_stop(),
        (Code::Char('r'), false, false) => ed.procs_restart(),
        (Code::Char('l'), false, false) => ed.procs_log(),
        _ => ed.key_unused(key, ":procs", "esc gives the keys to the code"),
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1 << 20;

    /// 9e: a keeper's row says its session's sandbox in the words the panel's state line has (`Guarantees::line`),
    /// its own `said` only before the session's first run.
    #[test]
    fn a_keepers_row_says_its_sessions_sandbox_as_the_panel_does() {
        use crate::sandbox::guarantees::{Found, Guarantees, Ns};
        let g = Guarantees::landlock(Found {
            ns: Ns::session(true, true),
            served: Ok(()),
            copies: false,
            abi: 8,
            ptys: None,
            tproxy: Ok(()),
            lo_missing: Vec::new(),
            short: None,
            brief: None,
            hint: None,
            network: false,
        });
        let root = Path::new("/p");
        let said = || "A+ (user net ipc pid mnt)".to_string();
        assert_eq!(keeper_what(Some(&g), said, root), format!("the session's keeper · {} · /p", g.line()),);
        assert!(g.line().starts_with("tier A+ · Landlock ABI 8"), "{}", g.line());
        assert_eq!(keeper_what(None, said, root), "the session's keeper · A+ (user net ipc pid mnt) · /p");
    }

    fn os(pid: u32, ppid: u32, name: &str) -> Os {
        Os { pid, ppid, name: name.into(), started: 1_000, rss: 10 * MB, ..Default::default() }
    }

    fn sample(procs: Vec<Os>) -> Sample {
        Sample {
            procs: procs.into_iter().map(|o| (o, Some(1.0))).collect(),
            took: Duration::ZERO,
            usual: None,
            free: Vec::new(),
        }
    }

    /// A row as the checks see one: tarae's child (`under` its own kind), `cpu`% of a core, `mb` of memory,
    /// up `up` seconds.
    fn row(pid: u32, kind: Kind, cpu: f32, mb: u64, up: u64) -> Row {
        Row {
            pid,
            ppid: 1,
            started: 1,
            depth: 0,
            under: kind.clone(),
            kind,
            title: format!("p{pid}"),
            what: String::new(),
            program: format!("prog{pid}"),
            up: Some(Duration::from_secs(up)),
            cpu: Some(cpu),
            rss: mb * MB,
            footprint: Some(mb * MB),
            zombie: false,
            flagged: false,
            session: None,
            left: false,
        }
    }

    /// #143: what `:procs` lists is in the UI state (JSON and text) as of its last look, and a watcher of
    /// `procs` hears what a look found changed — a process started, one ended — and nothing when nothing did.
    #[test]
    fn procs_are_in_the_state_and_a_watcher_hears_what_changed() {
        let mut ed = Editor::new(crate::config::Config::default());
        let me = std::process::id();
        // (started now: one started long ago and not known is a stray, flagged)
        let secs = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs();
        let os = |pid, ppid, name| Os { started: secs, ..os(pid, ppid, name) };
        assert!(ed.ui_state().procs.rows.is_empty(), "no look yet");
        ed.procs_sampled(sample(vec![os(me, 1, "tarae"), os(50_001, me, "sleep")]));
        let s = ed.ui_state();
        let rows: Vec<(u32, &str, &str)> =
            s.procs.rows.iter().map(|r| (r.pid, r.kind, r.title.as_str())).collect();
        assert_eq!(rows, [(me, "tarae", "tarae"), (50_001, "other", "sleep")]);
        let json = s.json();
        assert_eq!(json["procs"]["rows"][1]["pid"], 50_001);
        assert_eq!(json["procs"]["rows"][1]["memory"], 10 * MB);
        assert!(json["procs"]["looked_ms"].is_u64(), "{}", json["procs"]);
        let text = s.text();
        assert!(text.contains("procs: 2 processes · 20 MB"), "{text}");
        assert!(text.contains("\n  50001 sleep · cpu 1.0% · 10 MB\n"), "{text}");
        // A watcher of procs: the next look's changes, said once
        let (tx, rx) = std::sync::mpsc::sync_channel(crate::uistate::watch::UNREAD);
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        ed.ui_watch_add(crate::uistate::watch::Watcher { tx, kinds: vec!["procs".into()], overflowed: flag });
        let gone = Os { zombie: true, ..os(50_002, me, "cargo") };
        ed.procs_sampled(sample(vec![os(me, 1, "tarae"), gone.clone()]));
        crate::term::render(&mut ed, &mut Vec::new(), 120, 40).unwrap();
        let e: Vec<serde_json::Value> = rx.try_iter().map(|l| serde_json::from_str(&l).unwrap()).collect();
        assert_eq!(e.len(), 1, "{e:?}");
        assert_eq!(
            (e[0]["started"][0].as_str(), e[0]["ended"][0].as_str()),
            (Some("cargo (50002)"), Some("sleep (50001)"))
        );
        assert_eq!(e[0]["said"], "procs — started: cargo (50002) · ended: sleep (50001)");
        assert!(ed.ui_state().text().contains("50002 cargo · cpu 1.0% · 10 MB · ended, not reaped"));
        // Still unreaped at the next look: the check that found it, said
        ed.procs_sampled(sample(vec![os(me, 1, "tarae"), gone.clone()]));
        crate::term::render(&mut ed, &mut Vec::new(), 120, 40).unwrap();
        let e: Vec<serde_json::Value> = rx.try_iter().map(|l| serde_json::from_str(&l).unwrap()).collect();
        assert_eq!(e.len(), 1, "{e:?}");
        assert!(e[0]["found"][0].as_str().unwrap().starts_with("cargo (pid 50002) ended"), "{e:?}");
        // Nothing new: nothing said
        ed.procs_sampled(sample(vec![os(me, 1, "tarae"), gone]));
        crate::term::render(&mut ed, &mut Vec::new(), 120, 40).unwrap();
        let e: Vec<String> = rx.try_iter().collect();
        assert!(e.is_empty(), "{e:?}");
    }

    /// `/proc/<pid>/stat`: the fields after the name — which may hold spaces and parentheses — its parent,
    /// CPU ticks, start and resident pages; `Z` is a zombie.
    #[test]
    fn proc_stat_is_read_past_the_name() {
        let stat = "4242 (rust (an) alyzer) S 77 4242 4242 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 9 0 \
                    6000 123456789 2560 18446744073709551615";
        let o = parse_stat(4242, stat, 100, 4096, 1_700_000_000).expect("read");
        assert_eq!((o.ppid, o.name.as_str(), o.zombie), (77, "rust (an) alyzer", false));
        assert_eq!(o.cpu, Duration::from_secs(3), "250 + 50 ticks at 100 a second");
        assert_eq!(o.started, 1_700_000_060, "boot + 6000 ticks");
        assert_eq!(o.rss, 2560 * 4096);
        let z = stat.replacen(") S ", ") Z ", 1);
        assert!(parse_stat(4242, &z, 100, 4096, 0).expect("read").zombie);
        assert_eq!(parse_stat(1, "1 (x", 100, 4096, 0), None, "cut short: nothing made up");
    }

    /// A thread's `stat` and `wchan` (#181): its name, its state in words, where it sleeps — `0` (not
    /// sleeping, or addresses hidden) is no place — and its CPU time.
    #[test]
    fn a_threads_state_and_wait_channel_are_read() {
        let stat = "4243 (tokio-rt (w)) D 77 4242 4242 0 -1 4194560 100 0 0 0 30 20 0 0 20 0 9 0 6000 1 1 1";
        let t = parse_thread(stat, Some("folio_wait_bit\n"), 100).expect("read");
        assert_eq!(t.name, "tokio-rt (w)");
        assert_eq!(t.state, "in uninterruptible wait");
        assert_eq!(t.wchan.as_deref(), Some("folio_wait_bit"));
        assert_eq!(t.cpu, Duration::from_millis(500));
        let s = parse_thread(&stat.replacen(") D ", ") S ", 1), Some("0"), 100).expect("read");
        assert_eq!((s.state.as_str(), s.wchan), ("sleeping", None));
        assert_eq!(parse_thread("1 (x", None, 100), None, "cut short: nothing made up");
    }

    /// A look sees tarae's (here: the test's) child and what it started, with real CPU time — a busy loop's
    /// is about its wall time (Mach ticks aren't nanoseconds on Apple silicon) — and real memory. (The
    /// children are the test's own stand-ins, stopped at its end.)
    #[test]
    fn a_look_sees_children_their_cpu_and_memory() {
        use std::os::unix::process::CommandExt;
        let me = std::process::id();
        let spin = std::process::Command::new("sh")
            .args(["-c", "while :; do :; done"])
            .process_group(0)
            .spawn()
            .expect("sh");
        let nest = std::process::Command::new("sh")
            .args(["-c", "sleep 30 & wait"])
            .process_group(0)
            .spawn()
            .expect("sh");
        let mut s = Sampler::default();
        s.sample(me);
        std::thread::sleep(Duration::from_millis(400));
        let look = s.sample(me);
        let find = |pid: u32| look.procs.iter().find(|(o, _)| o.pid == pid).cloned();
        let (spun, pct) = find(spin.id()).expect("the child is listed");
        let nested = look.procs.iter().any(|(o, _)| o.ppid == nest.id() && o.name.contains("sleep"));
        for c in [&spin, &nest] {
            // SAFETY: stops the process groups of this test's own stand-ins.
            unsafe { libc::kill(-(c.id() as i32), libc::SIGKILL) };
        }
        let (mut spin, mut nest) = (spin, nest);
        let _ = (spin.wait(), nest.wait());
        assert_eq!(spun.ppid, me);
        assert!(nested, "what a child started is listed too");
        assert!(
            spun.cpu > Duration::from_millis(50) && spun.cpu < Duration::from_secs(5),
            "a busy loop for ~0.4 s took {:?} of CPU",
            spun.cpu
        );
        assert!(pct.is_some_and(|p| p > 10.0 && p < 150.0), "its share of a core: {pct:?}");
        assert!(spun.rss > 0, "resident memory read");
        assert_eq!(spun.footprint.is_some(), cfg!(target_os = "macos"), "footprint where there is one");
        let tarae = look.procs.first().expect("itself first");
        assert_eq!(tarae.0.pid, me);
        assert!(look.took < Duration::from_millis(500), "a look took {:?}", look.took);
    }

    /// A process's command line and working folder, read from the OS (a stand-in child of the test's own,
    /// in its temp folder, stopped at its end). Read until they're what it was started with, a second at
    /// most: right after the spawn it can still be on its way into `sleep` (on Linux its command line read
    /// as nothing then).
    #[test]
    fn a_processs_command_line_and_folder_are_read() {
        let dir = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("tarae-procs-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut child =
            std::process::Command::new("sleep").arg("30").current_dir(&dir).spawn().expect("sleep");
        let until = |want: &dyn Fn() -> bool| {
            let end = std::time::Instant::now() + Duration::from_secs(1);
            while !want() && std::time::Instant::now() < end {
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        until(&|| command(child.id()).as_deref() == Some("sleep 30"));
        until(&|| cwd(child.id()).as_ref() == Some(&dir));
        let (cmd, folder) = (command(child.id()), cwd(child.id()));
        let _ = child.kill();
        let _ = child.wait();
        std::fs::remove_dir_all(&dir).ok();
        assert_eq!(cmd.as_deref(), Some("sleep 30"));
        assert_eq!(folder, Some(dir));
        assert_eq!(command(u32::MAX - 1), None, "no such process: nothing made up");
    }

    /// #97: Claude's `procs` answers what `:procs` shows, no card — this session's processes (a command
    /// running in it: pid, parent, age, CPU, footprint and resident memory, command line, folder), not
    /// another session's unless `all` asks.
    #[test]
    fn claudes_procs_tool_says_what_procs_shows() {
        use serde_json::json;
        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("tarae-procs-tool-{}", std::process::id()));
        let (here, there) = (base.join("here"), base.join("there"));
        for d in [&here, &there] {
            std::fs::create_dir_all(d).unwrap();
        }
        let mut ed = Editor::new(crate::config::Config::default());
        (ed.session.root, ed.session.name) = (here.clone(), "here".into());
        let task = |name: &str, dir: &Path| crate::task::Task {
            name: name.into(),
            cmd: "sleep 20".into(),
            cwd: dir.into(),
        };
        ed.task_start(task("sleeps-here", &here));
        let other = ed.session_open(&there).unwrap();
        ed.session.name = "there".into();
        ed.task_start(task("sleeps-there", &there));
        assert!(ed.session_switch(0));
        let ask = |ed: &mut Editor, n: u64, args: serde_json::Value| -> String {
            let msg = json!({"jsonrpc": "2.0", "id": n, "method": "tools/call",
                "params": {"name": "procs", "arguments": args}});
            assert!(crate::tools::mcp(ed, u64::MAX, "c", &msg).is_none(), "answered from its worker");
            let end = Instant::now() + Duration::from_secs(10);
            loop {
                let got = crate::tools::ANSWERED
                    .with(|a| a.borrow().iter().find(|(id, _)| *id == json!(n)).cloned());
                if let Some((_, r)) = got {
                    return r.expect("an answer");
                }
                assert!(Instant::now() < end, "procs answered");
                if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                    ed.handle_event(ev);
                }
            }
        };
        let mine = ask(&mut ed, 970, json!({}));
        assert!(ed.offers.is_empty(), "no card");
        assert!(mine.starts_with("The editor's processes for session here"), "{mine}");
        assert!(mine.contains("sleeps-here — sleep 20"), "{mine}");
        // (`sh -c` with one command runs it in its own place)
        assert!(mine.contains("command: sleep 20") || mine.contains("command: sh -c sleep 20"), "{mine}");
        assert!(mine.contains(&format!("folder: {}", crate::allow::tilde(&here))), "{mine}");
        let pid = ed.session.task_run.as_ref().and_then(|r| r.pid()).unwrap();
        assert!(mine.contains(&format!("pid {pid} · parent {}", std::process::id())), "{mine}");
        assert!(mine.contains("· up ") && mine.contains("· cpu "), "{mine}");
        if cfg!(target_os = "macos") {
            assert!(mine.contains("(resident ") && mine.contains("physical footprint"), "{mine}");
        }
        assert!(!mine.contains("sleeps-there"), "another session's isn't this one's: {mine}");
        let every = ask(&mut ed, 971, json!({ "all": true }));
        assert!(every.starts_with("The editor's processes for every session"), "{every}");
        assert!(every.contains("── here") && every.contains("── there"), "headed by session: {every}");
        assert!(every.contains("sleeps-here") && every.contains("sleeps-there"), "{every}");
        ed.session.task_run = None;
        ed.with_session(other, |ed| ed.session.task_run = None);
        std::fs::remove_dir_all(&base).ok();
    }

    /// #100 as met: a run of Claude's in a session behind whose program outlives its panel (closed — the
    /// program shrugs off the SIGTERM) is still that session's, by name, in `procs` and `:procs`. (The
    /// stand-in ignores TERM and sleeps in the test's temp folder; it's killed at the end.)
    #[test]
    fn a_claude_run_left_running_stays_under_its_session() {
        use serde_json::json;
        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("tarae-procs-left-{}", std::process::id()));
        let (here, there) = (base.join("here"), base.join("there"));
        for d in [&here, &there] {
            std::fs::create_dir_all(d).unwrap();
        }
        let mut ed = Editor::new(crate::config::Config::default());
        (ed.session.root, ed.session.name) = (here.clone(), "here".into());
        let other = ed.session_open(&there).unwrap();
        ed.session.name = "there".into();
        let task = crate::task::Task {
            name: "swap".into(),
            cmd: "trap '' TERM; sleep 30".into(),
            cwd: there.clone(),
        };
        assert!(
            ed.task_start_for(task, crate::tools::For::User("swap".into()), None, Default::default()).is_ok()
        );
        let pid = ed.session.task_run.as_ref().and_then(|r| r.pid()).unwrap();
        // (its `trap` in place before the TERM comes)
        let end = Instant::now() + Duration::from_secs(5);
        while !tree(pid).iter().any(|o| o.name == "sleep") {
            assert!(Instant::now() < end, "the stand-in sleeps");
            std::thread::sleep(Duration::from_millis(20));
        }
        // Its panel closed: the run is gone, its program isn't
        ed.task_close();
        assert!(ed.session.task_run.is_none());
        assert!(ed.session_switch(0));
        std::thread::sleep(Duration::from_millis(200));
        assert!(read(pid).is_some_and(|o| !o.zombie), "still running");
        let msg = json!({"jsonrpc": "2.0", "id": 990, "method": "tools/call",
            "params": {"name": "procs", "arguments": {"all": true}}});
        assert!(crate::tools::mcp(&mut ed, u64::MAX, "c", &msg).is_none());
        let end = Instant::now() + Duration::from_secs(10);
        let text = loop {
            let got =
                crate::tools::ANSWERED.with(|a| a.borrow().iter().find(|(id, _)| *id == json!(990)).cloned());
            if let Some((_, r)) = got {
                break r.unwrap();
            }
            assert!(Instant::now() < end, "procs answered");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        };
        // SAFETY: stops the process group of this test's own stand-in.
        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        let at = text.find("── there").unwrap_or_else(|| panic!("there's heading: {text}"));
        let named = text.find("claude's command · left running").unwrap_or_else(|| panic!("named: {text}"));
        assert!(named > at, "under its session: {text}");
        assert!(text[named..].contains(&format!("pid {pid}")), "{text}");
        let _ = other;
        std::fs::remove_dir_all(&base).ok();
    }

    /// A sandbox watch held outside any run (its run gone, or never started) is still one tarae knows — not
    /// a stray child flagged ten minutes on.
    #[test]
    fn a_watch_held_outside_any_run_is_known() {
        let ed = Editor::new(crate::config::Config::default());
        let watch = crate::sandbox::Watch::spawned_for_test("sleep", "5");
        let wpid = watch.pid().expect("its child");
        let known = ed.procs_known();
        let k = known.iter().find(|k| k.pid == wpid);
        assert!(k.is_some_and(|k| k.kind == Kind::Helper && k.title == "sandbox watch"), "not known");
        watch.finish();
    }

    /// `s` on a child tarae doesn't know stops it and reaps it — nothing else holds it to (seen: a stray
    /// `log stream` stopped from `:procs`, `<defunct>` under tarae until it quit). The stand-in is the test's
    /// own `sleep`, its handle let go.
    #[test]
    fn a_stray_child_stopped_is_reaped() {
        let pid = std::process::Command::new("sleep").arg("60").spawn().unwrap().id();
        let started = read(pid).expect("running").started;
        stop_unknown(pid, started);
        // (signal 0: a zombie answers it — `read` may not see one)
        // SAFETY: signal 0 sends nothing.
        let there = || unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
        let end = Instant::now() + Duration::from_secs(5);
        while there() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!there(), "the stray ({pid}) is still there, {:?}", read(pid).map(|o| o.zombie));
    }

    /// A session's share (the session list's column): its children's CPU and memory with what they started,
    /// not tarae's own row nor another session's; none of its processes listed — nothing said.
    #[test]
    fn a_sessions_processes_are_summed_with_what_they_started() {
        let deeper = |mut r: Row, depth| {
            r.depth = depth;
            r
        };
        let rows = vec![
            row(1, Kind::Tarae, 3.0, 100, 9),
            row(10, Kind::Chat, 2.0, 200, 9),
            deeper(row(11, Kind::Other, 1.5, 50, 9), 1),
            deeper(row(12, Kind::Other, 0.5, 25, 9), 2),
            row(20, Kind::Terminal, 40.0, 30, 9),
            row(30, Kind::Lsp(1), 1.0, 400, 9),
        ];
        assert_eq!(summed(&rows, &[10, 30]), Some((5.0, 675 * MB)), "the chat with its two, and the server");
        assert_eq!(summed(&rows, &[20]), Some((40.0, 30 * MB)), "the terminal alone");
        assert_eq!(summed(&rows, &[1]), None, "tarae itself is no session's");
        assert_eq!(summed(&rows, &[99]), None);
    }

    /// The list: tarae first, its children by kind — what each is for from the editor — each with what it
    /// started under it; one the editor doesn't know by its own name; a held server with the running ones,
    /// with no process.
    #[test]
    fn rows_say_what_each_is_for_and_nest_the_rest() {
        let s = sample(vec![
            os(1, 0, "tarae"),
            os(5, 1, "bash"),
            os(2, 1, "rust-analyzer"),
            os(3, 2, "rust-analyzer-proc-macro-srv"),
            os(4, 1, "git"),
        ]);
        let known = [
            Known {
                pid: 2,
                kind: Kind::Lsp(7),
                title: "rust-analyzer".into(),
                what: "~/w".into(),
                session: None,
                at: None,
            },
            Known {
                pid: 5,
                kind: Kind::Terminal,
                title: "terminal".into(),
                what: "fish".into(),
                session: None,
                at: None,
            },
        ];
        let held = [Held { name: "pyright".into(), root: "/w".into(), what: String::new() }];
        let rows = rows(&s, &known, 1, &held, &[], 1_060);
        let got: Vec<(u32, usize, &str, &Kind)> =
            rows.iter().map(|r| (r.pid, r.depth, r.title.as_str(), &r.kind)).collect();
        assert_eq!(
            got,
            [
                (1, 0, "tarae", &Kind::Tarae),
                (2, 0, "rust-analyzer", &Kind::Lsp(7)),
                (3, 1, "rust-analyzer-proc-macro-srv", &Kind::Other),
                (0, 0, "pyright", &Kind::Held("pyright".into(), "/w".into())),
                (5, 0, "terminal", &Kind::Terminal),
                (4, 0, "git", &Kind::Other),
            ]
        );
        assert_eq!(rows[2].under, Kind::Lsp(7), "a child is judged as what it's under");
        assert_eq!(rows[1].up, Some(Duration::from_secs(60)));
    }

    /// Two sessions: what's shared stays on top (tarae, the spare claude, one tarae doesn't know); then each
    /// session under its heading, the heading summing CPU and memory of everything under it (what a server
    /// started too) — its server first, a held server by its root, then its chat and terminal.
    #[test]
    fn rows_group_by_session_under_headings_that_sum_them() {
        let with = |mut o: Os, cpu: f32, mb: u64| {
            o.rss = mb * MB;
            (o, Some(cpu))
        };
        let s = Sample {
            procs: vec![
                with(os(1, 0, "tarae"), 3.0, 100),
                with(os(2, 1, "claude"), 0.5, 300),
                with(os(3, 1, "rust-analyzer"), 2.0, 900),
                with(os(4, 3, "rust-analyzer-proc-macro-srv"), 0.5, 20),
                with(os(5, 1, "claude"), 1.0, 200),
                with(os(6, 1, "fish"), 40.0, 30),
                with(os(7, 1, "git"), 0.0, 5),
            ],
            took: Duration::ZERO,
            usual: None,
            free: Vec::new(),
        };
        let k = |pid, kind, title: &str, session| Known {
            pid,
            kind,
            title: title.into(),
            what: String::new(),
            session,
            at: None,
        };
        let known = [
            k(2, Kind::Spare, "claude", None),
            k(3, Kind::Lsp(7), "rust-analyzer", Some(0)),
            k(5, Kind::Chat, "claude", Some(1)),
            k(6, Kind::Terminal, "terminal", Some(1)),
        ];
        let held = [Held { name: "pyright".into(), root: "/wt/fix-login/py".into(), what: String::new() }];
        let groups = [
            Group { id: 0, name: "main".into(), root: "/w".into() },
            Group { id: 1, name: "fix-login".into(), root: "/wt/fix-login".into() },
        ];
        let rows = rows(&s, &known, 1, &held, &groups, 1_060);
        let got: Vec<(u32, usize, &str, Option<SessionId>)> =
            rows.iter().map(|r| (r.pid, r.depth, r.title.as_str(), r.session)).collect();
        assert_eq!(
            got,
            [
                (1, 0, "tarae", None),
                (2, 0, "claude", None),
                (7, 0, "git", None),
                (0, 0, "main", Some(0)),
                (3, 0, "rust-analyzer", Some(0)),
                (4, 1, "rust-analyzer-proc-macro-srv", Some(0)),
                (0, 0, "fix-login", Some(1)),
                (0, 0, "pyright", Some(1)),
                (5, 0, "claude", Some(1)),
                (6, 0, "terminal", Some(1)),
            ]
        );
        let sums = |at: usize| (rows[at].kind.clone(), rows[at].cpu, rows[at].memory());
        assert_eq!(sums(3), (Kind::Session(0), Some(2.5), 920 * MB), "the server with what it started");
        assert_eq!(sums(6), (Kind::Session(1), Some(41.0), 230 * MB), "the held server has no process");
        assert!(matches!(rows[7].kind, Kind::Held(..)) && rows[7].flagged);
        // The panel's totals and a session list's share don't count a heading twice
        assert_eq!(summed(&rows, &[5, 6]), Some((41.0, 230 * MB)));
        // A heading keeps its row a look later, as a process does
        assert!(rows[6].same(&rows[6].clone()) && !rows[6].same(&rows[3]) && !rows[6].same(&rows[7]));
    }

    /// #100: what a session started stays under it after its run is gone — remembered with when it started
    /// (a later process given the same pid isn't it) — and a child of tarae's nothing names goes under the
    /// session whose checkout it works in; elsewhere, it's shared.
    #[test]
    fn what_a_session_started_stays_its_own() {
        let at = |mut o: Os, dir: &str| {
            o.cwd = Some(dir.into());
            o
        };
        let s = sample(vec![
            os(1, 0, "tarae"),
            os(8, 1, "ra-swap-tool"),
            at(os(9, 1, "swap-helper"), "/wt/ra-swap/tools"),
            at(os(10, 1, "elsewhere"), "/tmp"),
            os(11, 1, "sleep"),
        ]);
        let remembered = |pid, started| Known {
            pid,
            kind: Kind::Other,
            title: "claude's command · left running".into(),
            what: "ra-swap-tool swap-on".into(),
            session: Some(1),
            at: Some(started),
        };
        // (pid 11's start doesn't match what was remembered: another process now)
        let known = [remembered(8, 1_001), remembered(11, 500)];
        let groups = [
            Group { id: 0, name: "main".into(), root: "/w".into() },
            Group { id: 1, name: "ra-swap".into(), root: "/wt/ra-swap".into() },
        ];
        let rows = rows(&s, &known, 1, &[], &groups, 1_060);
        let got: Vec<(u32, &str, Option<SessionId>)> =
            rows.iter().map(|r| (r.pid, r.title.as_str(), r.session)).collect();
        assert_eq!(
            got,
            [
                (1, "tarae", None),
                (10, "elsewhere", None),
                (11, "sleep", None),
                (0, "main", Some(0)),
                (0, "ra-swap", Some(1)),
                (8, "claude's command · left running", Some(1)),
                (9, "swap-helper", Some(1)),
            ]
        );
    }

    /// Busy: most of a core at every look for `BUSY_FOR` — not sooner, not for the user's own runs (a
    /// build is busy by design), and not once it rests; a server's own work in progress counts too.
    #[test]
    fn busy_for_minutes_is_found_but_not_in_the_users_runs() {
        let mut p = Procs::default();
        let t0 = Instant::now();
        let looks = |p: &mut Procs, cpu: f32, at: Duration| {
            let rows = [row(2, Kind::Lsp(1), cpu, 100, 600), row(3, Kind::Task, cpu, 100, 600)];
            p.check(&rows, &[], t0 + at).0
        };
        let mut at = Duration::ZERO;
        while at < BUSY_FOR {
            assert!(looks(&mut p, 95.0, at).is_empty(), "nothing at {at:?}");
            at += QUIET_EVERY;
        }
        let found = looks(&mut p, 95.0, at);
        assert_eq!(
            found.iter().map(|f| f.pid).collect::<Vec<_>>(),
            [2],
            "the server, not the task: {found:?}"
        );
        assert_eq!(found[0].short, "p2 busy 5m");
        assert!(looks(&mut p, 3.0, at + QUIET_EVERY).is_empty(), "it rested");
        assert!(looks(&mut p, 95.0, at + 2 * QUIET_EVERY).is_empty(), "busy again: the time starts over");
        // Its work in progress, for minutes
        let lsp = |since: Instant| LspFact {
            cid: 1,
            pid: Some(2),
            name: "ra".into(),
            serving: true,
            busy: Some((since, "Indexing".into())),
        };
        let now = t0 + Duration::from_secs(3600);
        assert!(p.check(&[], &[lsp(now - BUSY_FOR / 2)], now).0.is_empty());
        let found = p.check(&[], &[lsp(now - BUSY_FOR)], now).0;
        assert_eq!(found.iter().map(|f| f.what.as_str()).collect::<Vec<_>>(), ["ra busy for 5m (Indexing)"]);
    }

    /// Memory: a program's usual is the most it took in its first minutes (kept over its runs); far above it
    /// — three times and half a GB more — is found; a run of the user's isn't judged.
    #[test]
    fn memory_far_above_its_usual_is_found() {
        let mut p = Procs::default();
        let now = Instant::now();
        let early = SETTLE.as_secs() - 60;
        let late = SETTLE.as_secs() + 60;
        // Its first minutes: 300 MB at most; then it settles — learned
        p.check(&[row(2, Kind::Lsp(1), 1.0, 300, early)], &[], now);
        p.check(&[row(2, Kind::Lsp(1), 1.0, 200, early + 30)], &[], now);
        let (found, learned) = p.check(&[row(2, Kind::Lsp(1), 1.0, 250, late)], &[], now);
        assert!(found.is_empty() && learned);
        assert_eq!(p.usual.get("prog2"), Some(&vec![300 * MB]));
        // Twice that: not far above; four times and over half a GB more: found
        assert!(p.check(&[row(2, Kind::Lsp(1), 1.0, 600, late)], &[], now).0.is_empty());
        let found = p.check(&[row(2, Kind::Lsp(1), 1.0, 1300, late)], &[], now).0;
        assert_eq!(
            found.iter().map(|f| f.what.as_str()).collect::<Vec<_>>(),
            ["p2 at 1.3 GB — usually 300 MB"]
        );
        // Small programs: three times isn't enough without the half GB
        p.usual.insert("prog4".into(), vec![10 * MB]);
        assert!(p.check(&[row(4, Kind::Lsp(2), 1.0, 100, late)], &[], now).0.is_empty());
        // The user's run: not judged
        p.usual.insert("prog3".into(), vec![10 * MB]);
        assert!(p.check(&[row(3, Kind::Task, 1.0, 4000, late)], &[], now).0.is_empty());
    }

    /// #218: while sessions' kept turns are read back (`restoring`), tarae's own memory is neither said nor
    /// learned — a restore's reading isn't what tarae usually takes ("705 MB, usually 96 MB" at a restart);
    /// once they're back it's judged again. Another program is judged meanwhile all the same.
    #[test]
    fn taraes_memory_waits_for_a_restore() {
        let mut p = Procs::default();
        let now = Instant::now();
        let (early, late) = (SETTLE.as_secs() - 60, SETTLE.as_secs() + 60);
        p.usual.insert("prog1".into(), vec![96 * MB]);
        p.usual.insert("prog2".into(), vec![10 * MB]);
        p.restoring = true;
        let rows = [row(1, Kind::Tarae, 1.0, 705, early), row(2, Kind::Lsp(1), 1.0, 4000, late)];
        let found = p.check(&rows, &[], now).0;
        assert_eq!(found.iter().map(|f| f.pid).collect::<Vec<_>>(), [2], "the server, not tarae: {found:?}");
        // (nor was the restore's peak learned as its first minutes')
        p.restoring = false;
        let (found, learned) = p.check(&[row(1, Kind::Tarae, 1.0, 120, late)], &[], now);
        assert!(found.is_empty() && !learned, "{found:?}");
        assert_eq!(p.usual.get("prog1"), Some(&vec![96 * MB]));
        let found = p.check(&[row(1, Kind::Tarae, 1.0, 705, late)], &[], now).0;
        assert_eq!(found.iter().map(|f| f.pid).collect::<Vec<_>>(), [1], "back: judged again");
    }

    /// A server with no file of it open past its own stop is stopped by the check; one that exits three
    /// times in a while is held; a child tarae doesn't know, running long, and one left unreaped are said.
    /// Each is recorded once, ends when it's no longer so, and the status line's mark is the newest still on.
    #[test]
    fn the_other_checks_and_how_they_are_recorded() {
        let mut p = Procs::default();
        let t0 = Instant::now();
        let fact = |serving| LspFact { cid: 9, pid: Some(2), name: "ra".into(), serving, busy: None };
        assert!(p.check(&[], &[fact(false)], t0).0.is_empty());
        let late = t0 + crate::lsp_editor::IDLE_STOP + LEFT_GRACE;
        let found = p.check(&[], &[fact(false)], late).0;
        assert_eq!(found.iter().map(|f| &f.act).collect::<Vec<_>>(), [&Act::StopServer(9)]);
        assert!(p.check(&[], &[fact(true)], late).0.is_empty(), "a file of it open again: fine");
        // Exits: the third within the window holds it
        let root = Path::new("/w");
        assert!(p.exited("ra", root, t0).is_none() && p.exited("ra", root, t0).is_none());
        let held = p.exited("ra", root, t0).expect("the third");
        assert!(p.is_held("ra", root) && !p.is_held("ra", Path::new("/other")));
        assert_eq!(held.short, "ra keeps exiting");
        // A stranger running long; a zombie seen twice
        let mut stray = row(5, Kind::Other, 0.0, 1, STRAY_AFTER.as_secs());
        stray.title = "sleep".into();
        let mut zombie = row(6, Kind::Other, 0.0, 0, 1);
        zombie.zombie = true;
        let rows = [stray, zombie];
        let first: Vec<String> = p.check(&rows, &[], late).0.iter().map(|f| f.key.clone()).collect();
        assert_eq!(first, ["stray:5:1", "loop:ra:/w"], "a zombie once may be on its way to being reaped");
        let found = p.check(&rows, &[], late).0;
        let keys: Vec<&str> = found.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["stray:5:1", "zombie:6:1", "loop:ra:/w"]);
        // Recorded once each; the mark is the newest still on; gone, ended
        let at = SystemTime::now();
        assert_eq!(p.record(&found, at).len(), 3);
        assert!(p.record(&found, at).is_empty(), "the same things again aren't new");
        assert_eq!(p.mark().map(|c| c.short.as_str()), Some("ra keeps exiting"));
        p.record(&found[..1], at);
        assert_eq!(p.checks.iter().filter(|c| c.ended.is_some()).count(), 2);
        assert_eq!(p.mark().map(|c| c.short.as_str()), Some("sleep running"));
        p.record(&[], at);
        assert!(p.mark().is_none(), "nothing abnormal: no mark");
        // E16: ended ones leave the list after a while (the log keeps them); one that holds stays
        assert_eq!(p.checks.len(), 3);
        p.record(&found[..1], at + ENDED_KEPT - Duration::from_secs(1));
        assert_eq!(p.checks.len(), 4, "ended a moment ago: still listed (and one on again)");
        p.record(&found[..1], at + ENDED_KEPT);
        let left: Vec<(&str, bool)> =
            p.checks.iter().map(|c| (c.short.as_str(), c.ended.is_none())).collect();
        assert_eq!(left, [("sleep running", true)], "the ones ended {}m ago gone", ENDED_KEPT.as_secs() / 60);
    }

    /// What a run of Claude's left running (its environment names the run — `sandbox::left_by`) is listed as
    /// that, under the session whose checkout the run was in, not a stray of tarae's; nor is a child in a
    /// session not tarae's (the terminal's `nohup`, tarae only its last parent — `reaper`). One of tarae's own
    /// session that nothing names still is.
    #[test]
    fn what_a_run_left_goes_under_its_session_and_isnt_a_stray() {
        let sid = |mut o: Os, s: u32| {
            o.sid = Some(s);
            o
        };
        let mut gradle = sid(os(5, 1, "java"), 77);
        gradle.left = Some(("tarae-1-4.".into(), Some("/wt/fix/app".into())));
        let mut before = sid(os(8, 1, "kotlin"), 78);
        before.left = Some(("tarae-1-2.".into(), None));
        let s = sample(vec![
            sid(os(1, 0, "tarae"), 1),
            gradle,
            sid(os(6, 1, "node"), 88),
            sid(os(7, 1, "log"), 1),
            before,
        ]);
        let groups = [
            Group { id: 0, name: "main".into(), root: "/w".into() },
            Group { id: 1, name: "fix".into(), root: "/wt/fix".into() },
        ];
        let rows = rows(&s, &[], 1, &[], &groups, 1_000 + STRAY_AFTER.as_secs() + 60);
        let r = |pid| rows.iter().find(|r| r.pid == pid).unwrap();
        assert_eq!((r(5).title.as_str(), r(5).session), ("java · left by a run", Some(1)));
        assert_eq!(r(5).what, "left by a run of Claude's in /wt/fix/app");
        assert_eq!(r(8).what, "left by a run of Claude's before tarae restarted");
        let found = Procs::default().check(&rows, &[], Instant::now()).0;
        let keys: Vec<&str> = found.iter().map(|f| f.key.as_str()).collect();
        assert_eq!(keys, ["stray:7:1000"], "only tarae's own");
    }

    /// Linux, in a process of its own (`TARAE_REAP_SCENE`; it makes itself a child subreaper): the looks reap
    /// what tarae adopted — an orphan of a program in a session of its own, an orphan of tarae's session seen
    /// under its parent before — once a second look finds it `reaper::GRACE` after the first; not before. A
    /// child it started and holds that ended (in tarae's session, or `setsid`) is never reaped, however long
    /// unwaited: still a zombie (its pid not free for another process), its owner's kill reaches only it, and
    /// its wait gets its exit status; nor is a waiter within the grace robbed. Prints `reap: first=<all
    /// listed> before=<all listed> orphan=<reaped at GRACE> seen=<there at GRACE, reaped after> held=<its
    /// owner's exit code after a kill> setsid=<the same> late=<the late waiter's>`.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore]
    fn reap_scene() {
        use std::process::{Command, Stdio};
        let Some(dir) = std::env::var_os("TARAE_REAP_SCENE").map(PathBuf::from) else { return };
        crate::reaper::at_start();
        let until = |what: &dyn Fn() -> bool| {
            let end = Instant::now() + Duration::from_secs(5);
            while !what() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        let zombie = |pid: u32| read(pid).is_some_and(|o| o.zombie);
        let pid_in = |f: &Path| {
            until(&|| std::fs::read_to_string(f).is_ok_and(|t| t.ends_with('\n')));
            std::fs::read_to_string(f).unwrap().trim().parse::<u32>().unwrap()
        };
        let sh = |c: &str| {
            let mut cmd = Command::new("sh");
            cmd.args(["-c", c]);
            cmd
        };
        // Children it started and holds, ended at once: a plain one (tarae's session) and a `setsid` one
        let mut held = sh("exit 1").spawn().unwrap();
        let held_sid = crate::spawn::setsid(&sh("exit 2")).unwrap().pid;
        // An orphan a program in a session of its own left (a run's, the terminal's)
        let file = dir.join("orphan");
        let prog =
            crate::spawn::setsid(&sh(&format!("(sleep 0.2 & echo $! > '{}')", file.display()))).unwrap();
        crate::spawn::wait(prog.pid).unwrap();
        let orphan = pid_in(&file);
        // One in tarae's session, under its parent still at the first look
        let file = dir.join("seen");
        let mut mid = sh(&format!("sleep 1 & echo $! > '{}'; read x", file.display()))
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let seen = pid_in(&file);
        let mut late = sh("exit 3").spawn().unwrap();
        until(&|| [held.id(), held_sid, orphan, late.id()].iter().all(|&p| zombie(p)));
        let (mut s, mut r) = (Sampler::default(), crate::reaper::Reaper::default());
        let listed = |s: &Sample, pid: u32| s.procs.iter().any(|(o, _)| o.pid == pid);
        let all = [held.id(), held_sid, orphan, seen, late.id()];
        let t0 = Instant::now();
        let one = look(&mut s, &mut r, t0);
        let first = all.iter().all(|&p| listed(&one, p));
        drop(mid.stdin.take());
        mid.wait().unwrap();
        until(&|| zombie(seen));
        let half = look(&mut s, &mut r, t0 + crate::reaper::GRACE / 2);
        let before = all.iter().all(|&p| listed(&half, p));
        let code = late.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-2);
        let there = |pid: u32| Path::new(&format!("/proc/{pid}")).exists();
        let at = look(&mut s, &mut r, t0 + crate::reaper::GRACE);
        let orphan_reaped = !listed(&at, orphan) && !there(orphan);
        let seen_kept = listed(&at, seen) && there(seen);
        let after = look(&mut s, &mut r, t0 + crate::reaper::GRACE * 3);
        let seen_reaped = !listed(&after, seen) && !there(seen);
        // (the held ones: still zombies, so the pid is still theirs — the kill reaches only them)
        let owned = |pid: u32| zombie(pid) && listed(&after, pid);
        let held_code = match owned(held.id()) && held.kill().is_ok() {
            true => held.wait().map(|s| s.code().unwrap_or(-1)).unwrap_or(-2).to_string(),
            false => "reaped".into(),
        };
        let sid_code = match owned(held_sid) {
            true => crate::spawn::wait(held_sid).map(|s| s.code().unwrap_or(-1)).unwrap_or(-2).to_string(),
            false => "reaped".into(),
        };
        let yes = |b: bool| if b { "yes" } else { "no" };
        println!(
            "reap: first={} before={} orphan={} seen={} held={held_code} setsid={sid_code} late={code}",
            yes(first),
            yes(before),
            yes(orphan_reaped),
            yes(seen_kept && seen_reaped),
        );
    }

    /// Which zombie children the reaper takes for adopted (`reaper::Reaper::adopted`): one in a session
    /// neither tarae's nor its own, or seen under another parent at a look before (and still, at the looks
    /// after); never one of tarae's session or leading its own that wasn't — a child tarae started and holds
    /// (an LSP server that crashed, the spare Claude) — nor anything while tarae's own session is unknown.
    #[test]
    fn the_reaper_takes_only_what_tarae_adopted() {
        let p = |pid: u32, ppid: u32, sid: u32, zombie: bool| {
            let mut o = os(pid, ppid, "p");
            (o.sid, o.zombie) = (Some(sid), zombie);
            o
        };
        let mut r = crate::reaper::Reaper::default();
        let me = p(1, 0, 1, false);
        // tarae's own: a crashed server (its session), a crashed setsid one; a run's orphan; a terminal's
        // child still under its shell
        let (server, adapter, orphan) = (p(2, 1, 1, true), p(3, 1, 3, true), p(4, 1, 9, true));
        let (shell, under) = (p(5, 1, 5, false), p(6, 5, 1, false));
        let tree = [&me, &server, &adapter, &orphan, &shell, &under];
        assert_eq!(r.adopted(&tree, 1), [(4, 1_000)]);
        // (its parent gone, the one under it is tarae's — in tarae's session, but seen under another before)
        let adopted = p(6, 1, 1, true);
        let tree = [&me, &server, &adapter, &orphan, &adopted];
        assert_eq!(r.adopted(&tree, 1), [(4, 1_000), (6, 1_000)]);
        assert_eq!(r.adopted(&tree, 1), [(4, 1_000), (6, 1_000)], "still known at the look after");
        // A new reaper never saw it under another parent: like tarae's own, left
        assert_eq!(crate::reaper::Reaper::default().adopted(&tree, 1), [(4, 1_000)]);
        // tarae's session unknown: nothing is
        let me_unknown = os(1, 0, "tarae");
        assert!(crate::reaper::Reaper::default().adopted(&[&me_unknown, &orphan, &server], 1).is_empty());
    }

    /// `reap_scene`, run.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_looks_reap_what_tarae_adopted_and_never_what_it_holds() {
        let dir = std::env::temp_dir().join(format!("tarae-reap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "procs::tests::reap_scene", "--ignored", "--nocapture", "--test-threads=1"])
            .env("TARAE_REAP_SCENE", &dir)
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).ok();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let said = text.lines().find_map(|l| l.split_once("reap: ").map(|(_, s)| s.to_string()));
        let err = String::from_utf8_lossy(&out.stderr);
        let want = "first=yes before=yes orphan=yes seen=yes held=1 setsid=2 late=3";
        assert_eq!(said.as_deref(), Some(want), "{text}\n{err}");
    }

    /// E16: the sandbox's own `log stream` beside a run of Claude's is a process tarae knows — not a stray
    /// child flagged ten minutes on. (A stand-in watch: `sleep`, as its child.)
    #[test]
    fn the_sandbox_watch_is_known() {
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        let dir = std::env::temp_dir();
        ed.task_start(crate::task::Task { name: "slow".into(), cmd: "sleep 5".into(), cwd: dir });
        let watch = crate::sandbox::Watch::spawned_for_test("sleep", "5");
        let wpid = watch.pid().expect("its child");
        let policy = crate::sandbox::Policy::for_project(&crate::sandbox::Env::default());
        ed.session.task_run.as_mut().unwrap().guard =
            Some(crate::sandbox::Guard::In(Box::new(crate::sandbox::Policy {
                watch: Some(watch),
                ..policy
            })));
        let known = ed.procs_known();
        let k = known.iter().find(|k| k.pid == wpid).expect("the watch is known");
        assert_eq!(
            (k.title.as_str(), k.what.as_str()),
            ("sandbox watch", "what the sandbox blocks: sleep 5")
        );
        ed.task_close();
    }

    /// A project with a stand-in language server (`cat`, which reads what it's sent).
    fn with_server(name: &str) -> (Editor, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tarae-procs-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"t\"\n").unwrap();
        let cfg = "[lsp.fake]\ncommand = \"sh\"\nargs = [\"-c\", \"cat > /dev/null\"]\n[lang.rust]\nlsp = [\"fake\"]\n";
        let (config, w) = crate::config::parse(cfg);
        assert!(w.is_empty(), "{w:?}");
        let mut ed = Editor::new(config);
        ed.open(&dir.join("main.rs")).unwrap();
        (ed, std::fs::canonicalize(&dir).unwrap())
    }

    /// Events until `done` (or 5 s pass).
    fn settle(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !done(ed) && Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
    }

    fn screen(ed: &mut Editor, w: u16, h: u16) -> Vec<String> {
        let mut bytes = Vec::new();
        crate::term::render(ed, &mut bytes, w, h).unwrap();
        let mut vt = vt100::Parser::new(h, w, 0);
        vt.process(&bytes);
        vt.screen().rows(0, w).collect()
    }

    fn keys(ed: &mut Editor, keys: &str) {
        for k in keys.split_whitespace() {
            ed.handle_key(k.parse().unwrap());
        }
    }

    /// #219: free space as the checks take it — below 10 GB (a tenth of a small disk) the status line's mark,
    /// below 2 GB a card that waits for a key, once each time it gets there; room again and both end.
    #[test]
    fn a_disk_running_full_is_marked_then_said_on_a_card_once() {
        const GB: u64 = 1 << 30;
        let v = |free| Volume { at: PathBuf::from("/Volumes/x"), free, total: 500 * GB };
        assert!(disk_check(&[v(50 * GB)]).is_empty());
        let low = disk_check(&[v(8 * GB)]);
        assert_eq!(low.iter().map(|f| f.key.as_str()).collect::<Vec<_>>(), ["disk-low:/Volumes/x"]);
        assert_eq!(low[0].short, "disk 8.0 GB free");
        assert_eq!(disk_check(&[v(GB)]).len(), 2, "low and at the floor");
        let small = Volume { at: PathBuf::from("/s"), free: 5 * GB, total: 32 * GB };
        assert!(disk_check(&[small]).is_empty(), "a small disk: a tenth of it");
        // Through the looks: the mark, the card once per crossing
        let mut ed = Editor::new(crate::config::Config::default());
        let look = |ed: &mut Editor, free| {
            let mut s = Sample::of(Vec::new());
            s.free = vec![v(free)];
            ed.procs_sampled(s);
        };
        let cards = |ed: &Editor| {
            ed.messages
                .iter()
                .filter(|m| m.1 == crate::editor::ToastKind::Error && m.2.contains("almost full"))
                .count()
        };
        look(&mut ed, 8 * GB);
        assert_eq!(ed.procs.mark().map(|c| c.short.as_str()), Some("disk 8.0 GB free"));
        assert_eq!(cards(&ed), 0);
        look(&mut ed, GB);
        look(&mut ed, GB);
        assert_eq!(cards(&ed), 1, "once while it stays there");
        assert_eq!(ed.procs.mark().map(|c| c.short.as_str()), Some("disk 1.0 GB free"));
        look(&mut ed, 50 * GB);
        assert!(ed.procs.mark().is_none());
        look(&mut ed, GB);
        assert_eq!(cards(&ed), 2, "again when it gets there again");
        // The real look: the temp folder's file system, a place not there yet by its folder
        let tmp = std::env::temp_dir();
        let vols = volumes(&[tmp.clone(), tmp.join("not-there-yet/x")]);
        assert_eq!(vols.len(), 1, "one file system, once");
        assert!(vols[0].total > 0 && vols[0].free <= vols[0].total, "{vols:?}");
    }

    /// `:procs` lists tarae and its language server — what it's for, its numbers — in the bottom slot;
    /// `s` on the server's row stops it (its process reaped). (The server is the test's stand-in.)
    #[test]
    fn procs_lists_the_server_and_s_stops_it() {
        let (mut ed, root) = with_server("stop");
        let cid = ed.doc().lsp.client.expect("attached");
        let pid = ed.client(cid).and_then(crate::lsp::Client::pid).expect("its process");
        crate::typed::execute(&mut ed, "procs").unwrap();
        settle(&mut ed, |ed| ed.procs.rows.iter().any(|r| r.kind == Kind::Lsp(cid)));
        let rows: Vec<(&Kind, &str)> =
            ed.procs.rows.iter().take(2).map(|r| (&r.kind, r.title.as_str())).collect();
        assert_eq!(rows, [(&Kind::Tarae, "tarae"), (&Kind::Lsp(cid), "fake")]);
        assert_eq!(ed.procs.rows[1].what, crate::allow::tilde(&root));
        keys(&mut ed, "j");
        let shown = screen(&mut ed, 140, 30).join("\n");
        for s in [
            "PROCS",
            "PID",
            "PROCESS",
            "FOR",
            "CPU",
            "RESIDENT",
            "SELF-CHECKS",
            "none now",
            "s stop",
            "r restart",
        ] {
            assert!(shown.contains(s), "{s:?} in\n{shown}");
        }
        let line = shown.lines().find(|l| l.contains(" fake ")).expect("the server's row");
        assert!(line.contains(&pid.to_string()) && line.contains('▎'), "selected: {line}");
        keys(&mut ed, "s");
        assert!(ed.client(cid).is_none(), "stopped");
        // Said as it happened: what stopped first, then how it comes back
        let said = ed.toasts.last().map(|t| t.text.as_str());
        assert_eq!(said, Some("fake stopped · the next file of its project starts it again"));
        assert_eq!(
            stopped("claude", Some(CHAT_AGAIN)),
            "claude stopped · the chat's next message starts it again"
        );
        assert_eq!(stopped("the terminal", None), "the terminal stopped");
        let end = Instant::now() + Duration::from_secs(5);
        // SAFETY: signal 0 only asks whether the process is there.
        while unsafe { libc::kill(pid as i32, 0) } == 0 && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_ne!(unsafe { libc::kill(pid as i32, 0) }, 0, "its process is gone");
        keys(&mut ed, "q");
        assert!(!ed.procs_up(), "q closes it");
        std::fs::remove_dir_all(root).ok();
    }

    /// #131: `s` on a task whose program left a child behind that ignores the stop and holds its output:
    /// the task's own process is reaped at once — not a zombie until that child goes — and the run ends
    /// soon after (`DRAIN_AFTER_EXIT`). (Every process here is the test's own stand-in; the child left
    /// behind is a `sleep` it ends itself.)
    #[test]
    fn s_on_a_task_reaps_it_whoever_holds_its_output() {
        let dir = std::env::temp_dir().join(format!("tarae-procs-reap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ed = Editor::new(crate::config::Config::default());
        // (the child says its pid once it ignores TERM — said by the parent, a loaded runner's TERM came
        // before the trap and it died: a Linux flake)
        let cmd = "sh -c 'trap \"\" TERM; echo $$ > left.pid; exec sleep 8' & echo started; wait".to_string();
        ed.task_start(crate::task::Task { name: "holder".into(), cmd, cwd: dir.clone() });
        let pid = ed.session.task_run.as_ref().and_then(|r| r.pid()).expect("its process");
        let left = dir.join("left.pid");
        settle(&mut ed, |_| std::fs::read_to_string(&left).is_ok_and(|s| s.ends_with('\n')));
        let child: i32 = std::fs::read_to_string(&left).unwrap().trim().parse().unwrap();
        crate::typed::execute(&mut ed, "procs").unwrap();
        settle(&mut ed, |ed| ed.procs.rows.iter().any(|r| r.kind == Kind::Task));
        let at = ed.procs.rows.iter().position(|r| r.kind == Kind::Task).expect("its row");
        ed.procs.panel.as_mut().unwrap().selected = at;
        keys(&mut ed, "s");
        let gone = |pid: i32| {
            // SAFETY: signal 0 only asks whether the process is there (a zombie still is).
            unsafe { libc::kill(pid, 0) != 0 }
        };
        let end = Instant::now() + Duration::from_secs(2);
        while !gone(pid as i32) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        let reaped = gone(pid as i32);
        let still = !gone(child);
        settle(&mut ed, |ed| {
            ed.session.task_run.as_ref().is_some_and(|r| r.state != crate::task::State::Running)
        });
        let ended = ed.session.task_run.as_ref().is_some_and(|r| r.state != crate::task::State::Running);
        // SAFETY: the `sleep` this test's own command left behind (its pid from its own file).
        unsafe { libc::kill(child, libc::SIGKILL) };
        std::fs::remove_dir_all(&dir).ok();
        assert!(still, "the child left behind still held its output while it was stopped");
        assert!(reaped, "the task's process reaped, not a zombie while that child lives");
        assert!(ended, "the run ended without waiting for that child");
    }

    /// Two sessions, each with its checkout's server and a chat (the test's stand-ins): `:procs` heads
    /// each — its name, its processes under it, their numbers summed on the heading. `j` walks across the
    /// groups; `s` on the heading only says what it is; `s` on the session behind's server and chat stops
    /// those (said with its session's name), not the front's; `l` on a heading brings that session to the
    /// front.
    #[test]
    fn procs_heads_each_session_and_acts_on_the_right_ones() {
        let (mut ed, root) = with_server("groups");
        (ed.session.root, ed.session.name) = (root.clone(), "base".into());
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        let front = ed.doc().lsp.client.expect("attached");
        crate::chat::open(&mut ed);
        let second = root.with_file_name(format!("{}-second", root.file_name().unwrap().to_string_lossy()));
        std::fs::create_dir_all(&second).unwrap();
        std::fs::write(second.join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(second.join("Cargo.toml"), "[package]\nname = \"t\"\n").unwrap();
        let id = ed.session_open(&second).unwrap();
        ed.session.name = "second".into();
        ed.open(&ed.session.root.join("main.rs")).unwrap();
        let behind = ed.doc().lsp.client.expect("its own server");
        assert_ne!(front, behind);
        crate::chat::open(&mut ed);
        ed.session_switch(0);
        let chat_of = |ed: &Editor, l: SessionId| {
            let session = std::iter::once(&ed.session).chain(&ed.sessions).find(|x| x.id == l).unwrap();
            session.chat.as_ref().and_then(crate::chat::Chat::pid)
        };
        let (chat0, chat1) = (chat_of(&ed, 0).expect("its claude"), chat_of(&ed, id).expect("its claude"));
        crate::typed::execute(&mut ed, "procs").unwrap();
        let listed = |ed: &Editor, c| ed.procs.rows.iter().position(|r| r.kind == Kind::Lsp(c));
        let has = |ed: &Editor, pid| ed.procs.rows.iter().any(|r| r.pid == pid);
        settle(&mut ed, |ed| {
            listed(ed, front).is_some() && listed(ed, behind).is_some() && has(ed, chat0) && has(ed, chat1)
        });
        let session_of =
            |ed: &Editor, pid| ed.procs.rows.iter().find(|r| r.pid == pid).and_then(|r| r.session);
        assert_eq!(
            (session_of(&ed, chat0), session_of(&ed, chat1)),
            (Some(0), Some(id)),
            "each chat its own"
        );
        let at = |ed: &Editor, k: Kind| ed.procs.rows.iter().position(|r| r.kind == k).expect("listed");
        let (head0, head1) = (at(&ed, Kind::Session(0)), at(&ed, Kind::Session(id)));
        let (s0, s1) = (at(&ed, Kind::Lsp(front)), at(&ed, Kind::Lsp(behind)));
        assert!(head0 < s0 && s0 < head1 && head1 < s1, "each server under its heading: {:?}", ed.procs.rows);
        assert_eq!(ed.procs.rows[head1].title, "second");
        let h = &ed.procs.rows[head1];
        let under: Vec<&Row> = ed.procs.rows[head1 + 1..].iter().filter(|r| r.session == Some(id)).collect();
        let sum = |f: fn(&Row) -> u64| under.iter().map(|r| f(r)).sum::<u64>();
        assert_eq!(
            (h.rss, h.memory()),
            (sum(|r| r.rss), sum(Row::memory)),
            "the heading sums what's under it"
        );
        let summed = mem(h.memory());
        // (on its heading, so it's in sight — other tests' children are listed on top too)
        keys(&mut ed, &"j ".repeat(head1));
        let shown = screen(&mut ed, 140, 40).join("\n");
        // (its checkout's path names it — not the chat's session tabs, not the server's row)
        let head = shown.lines().find(|l| l.contains("-second") && !l.contains("fake")).expect("its heading");
        assert!(head.contains("second  "), "its name from the pid column: {head}");
        assert!(head.contains(&summed), "summed on screen: {head}");
        // Across the groups: on the heading, then the server behind
        keys(&mut ed, "s");
        assert_eq!(
            ed.status.as_ref().map(|s| s.0.as_str()),
            Some("second is a session — :session-end ends it")
        );
        keys(&mut ed, &"j ".repeat(s1 - head1));
        keys(&mut ed, "s");
        assert!(
            ed.client(behind).is_none() && ed.client(front).is_some(),
            "the one behind stopped, not the front's"
        );
        let said = ed.toasts.last().map(|t| t.text.as_str());
        assert_eq!(said, Some("second · fake stopped · the next file of its project starts it again"));
        // Its chat: that session's claude stops, the front's goes on
        let c1 = ed.procs.rows.iter().position(|r| r.pid == chat1).expect("its row");
        keys(&mut ed, "g");
        keys(&mut ed, &"j ".repeat(c1));
        keys(&mut ed, "s");
        settle(&mut ed, |ed| chat_of(ed, id).is_none());
        assert_eq!((chat_of(&ed, 0), chat_of(&ed, id)), (Some(chat0), None), "the one behind stopped");
        let said = ed.toasts.last().map(|t| t.text.as_str());
        assert_eq!(said, Some("second · claude stopped · the chat's next message starts it again"));
        // `l` on a heading: that session to the front (its row now — what the processes started since, a
        // chat's `cat`, can have come in above it)
        let head1 = at(&ed, Kind::Session(id));
        keys(&mut ed, "g");
        keys(&mut ed, &"j ".repeat(head1));
        keys(&mut ed, "l");
        assert_eq!(ed.session.id, id, "brought to the front");
        drop(ed);
        std::fs::remove_dir_all(root).ok();
        std::fs::remove_dir_all(second).ok();
    }

    /// A server exiting a third time is held: the status line says so (and nothing did before), the next
    /// file of its project doesn't start it; `r` on its row in `:procs` lets it start, and it does.
    #[test]
    fn a_server_that_keeps_exiting_is_held_until_r() {
        let (mut ed, root) = with_server("held");
        // (the status line: the screen's last row)
        let status = |ed: &mut Editor| screen(ed, 140, 30)[29].clone();
        assert!(!status(&mut ed).contains('▲'), "no mark while all is well: {}", status(&mut ed));
        for _ in 0..3 {
            let cid = ed.doc().lsp.client.expect("attached");
            let pid = ed.client(cid).and_then(crate::lsp::Client::pid).expect("its process");
            // SAFETY: a signal to this test's stand-in server.
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            settle(&mut ed, |ed| ed.client(cid).is_none());
            let id = ed.doc().id;
            ed.close_doc(id);
            ed.open(&root.join("main.rs")).unwrap();
        }
        assert!(ed.doc().lsp.client.is_none(), "held: not started again");
        assert!(status(&mut ed).contains("▲ fake keeps exiting"), "{}", status(&mut ed));
        crate::typed::execute(&mut ed, "procs").unwrap();
        settle(&mut ed, |ed| ed.procs.rows.iter().any(|r| matches!(r.kind, Kind::Held(..))));
        let at = ed.procs.rows.iter().position(|r| matches!(r.kind, Kind::Held(..))).expect("its row");
        keys(&mut ed, &"j ".repeat(at));
        keys(&mut ed, "r");
        assert!(ed.doc().lsp.client.is_some(), "started again");
        // (its own check: the test binary's other children — another test's `sh` not waited for yet — are checks
        // of `:procs` too, and the tests run side by side)
        let ended = |ed: &Editor| {
            ed.procs.checks.iter().any(|c| c.what.contains("exited 3 times") && c.ended.is_some())
        };
        settle(&mut ed, |ed| ended(ed));
        assert!(ended(&ed), "the check ended, and is recorded — still holding: {:?}", ed.procs.holding());
        std::fs::remove_dir_all(root).ok();
    }

    /// The sampling thread's waits: closed, the slow pace; opened, a look at once; asked, at once; the editor
    /// gone, it ends.
    #[test]
    fn the_sampler_waits_its_pace_and_wakes_when_asked() {
        let (tx, rx) = channel();
        let mut open = false;
        let t = Instant::now();
        assert_eq!(wait(&rx, Duration::from_millis(60), &mut open), Some(()));
        assert!(t.elapsed() >= Duration::from_millis(60), "its pace");
        tx.send(Wake::Open(true)).unwrap();
        let t = Instant::now();
        assert_eq!(wait(&rx, Duration::from_secs(30), &mut open), Some(()));
        // (at once, not after the 30 s pace — the bound only has to outlast a slow runner)
        assert!(open && t.elapsed() < Duration::from_secs(10), "opened: at once");
        tx.send(Wake::Now).unwrap();
        assert_eq!(wait(&rx, Duration::from_secs(30), &mut open), Some(()));
        drop(tx);
        assert_eq!(wait(&rx, Duration::from_secs(30), &mut open), None, "the editor is gone");
    }
}
