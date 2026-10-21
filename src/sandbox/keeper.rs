//! A session's keeper (`docs/design/sandbox-keeper.md` §The keeper): a small process per session, `tarae
//! --sandbox-keeper`, that forks each sandboxed run of its session, reaps it and says how it ended, and is the
//! child subreaper of what the runs leave — so what a run leaves running is the session's process tree, not a
//! tag (`env -i` can't shed it), and ends with it. Where user namespaces work (`tier.rs`) it's born in the
//! session's own — user, ipc, mount, pid where a fresh `/proc` mounts (then it's their PID 1), net where the
//! policy opens nothing — with the user's own ids (D5), its own `/proc`, `/dev/pts` and `/dev/shm` (D10), `lo`
//! up, and every capability dropped before its exec; without, a plain fork server (no namespaces).
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
    /// tarae's own number for it, never reused (a pid is): what its runs' sockets are known by where sessions
    /// share a loopback (`owners::Owner.keeper`), let go when it's gone.
    pub serial: u64,
    pub root: PathBuf,
    /// The namespaces it was born with.
    pub ns: Ns,
    /// Its own device places (`DEVICE_HANDLES`), as it handed them with `Ready`: what its runs' rules grant
    /// the devices on when it's in a mount namespace of its own (`linux::confine`).
    pub devices: Vec<(&'static str, OwnedFd)>,
    /// The families whose address its `lo` was to carry (`Families`) but couldn't take — said with it (`said`).
    pub lo_missing: Families,
    chan: OwnedFd,
    pidfd: OwnedFd,
    state: Mutex<State>,
    changed: Condvar,
}

#[derive(Default)]
struct State {
    /// Why it's gone — None while it's there.
    gone: Option<String>,
    /// tarae asked it to end (`end`): its going isn't a death.
    ending: bool,
    /// It didn't end in time and tarae killed it (`ended_by`): what its runs left may outlive it.
    killed: bool,
    /// A keeper of another policy took its place: no run more, and it ends once its last one has.
    superseded: bool,
    /// Its policy's key (`sandbox::policy_key`) — None: a probe's, any key takes it over.
    key: Option<String>,
    next: u64,
    runs: HashMap<u64, Slot>,
    /// Tests (M3): what it said it uses.
    #[cfg(test)]
    usage: Option<(u64, u64)>,
    /// Tests: what its held runs' pidfds' fdinfo reads as, instead of `/proc`'s.
    #[cfg(test)]
    fdinfo: Option<&'static str>,
    /// Tests: how long tarae waits before it numbers a held run (a slow reader thread, made sure).
    #[cfg(test)]
    number_after: Option<std::time::Duration>,
    /// Tests: how its runs ended, as it said (`Exited`), whether tarae still waited on them or not.
    #[cfg(test)]
    exits: Vec<i32>,
}

#[derive(Default)]
struct Slot {
    /// `Started`'s pid and pidfd — or why it didn't start.
    started: Option<Result<(u32, Arc<OwnedFd>), String>>,
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

/// A keeper for the probe (`tier.rs`): born with `ns`, kept by nobody but the caller.
pub(super) fn born_for_probe(root: &Path, ns: Ns) -> io::Result<Arc<Keeper>> {
    Keeper::born_lo(root, None, ns, Families::here(), false)
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
    /// (`Families::here`).
    fn born(root: &Path, key: Option<&str>, ns: Ns) -> io::Result<Arc<Keeper>> {
        Keeper::born_with(root, key, ns, Families::here())
    }

    /// `born`, its `lo` carrying `families`' addresses.
    fn born_with(root: &Path, key: Option<&str>, ns: Ns, families: Families) -> io::Result<Arc<Keeper>> {
        Keeper::born_lo(root, key, ns, families, false)
    }

    /// `born_with` — tests: the mirrors put on a device that isn't there (`off_lo`).
    fn born_lo(
        root: &Path,
        key: Option<&str>,
        ns: Ns,
        families: Families,
        off_lo: bool,
    ) -> io::Result<Arc<Keeper>> {
        let exe = exe()?;
        let (ours, theirs) = seqpacket()?;
        let null = open_null(libc::O_RDWR)?;
        let birth = Birth::new(families, off_lo)?;
        let mut lo_missing = Families::default();
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
                birth.child(ns);
                for fd in 0..3 {
                    libc::dup2(null.as_raw_fd(), fd);
                }
                if theirs.as_raw_fd() == CHANNEL_FD {
                    libc::fcntl(CHANNEL_FD, libc::F_SETFD, 0);
                } else {
                    libc::dup2(theirs.as_raw_fd(), CHANNEL_FD);
                }
                // (the limit tarae started with: the keeper's runs inherit it — `nofile`)
                crate::nofile::restore();
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
        let mut had = ns;
        let ready = (|| {
            // (what it's in, read from its own links — not the flags asked: its words and its runs' rules are what it
            // has, and one outside what it asked for is no keeper. Read while it waits for `go`: a copy of tarae
            // still, its namespaces its clone's. Once it runs it isn't dumpable — in the session's user namespace its
            // links stay tarae's to read (tarae owns that namespace), but in tarae's own (none of the session's: C)
            // they're closed to a tarae without CAP_SYS_PTRACE there, as on any desktop)
            had = Ns::of_pid(pid as u32, ns.shm)?;
            if had != ns {
                return Err(io::Error::other(format!(
                    "a keeper asked to be apart from tarae in {} is apart in {}",
                    ns.apart_in(),
                    had.apart_in()
                )));
            }
            if ns.user {
                id_maps(pid as i32)?;
            }
            // SAFETY: one byte written from a buffer we own, on our end of the go pipe.
            if unsafe { libc::write(go.as_raw_fd(), [1u8].as_ptr().cast(), 1) } != 1 {
                return Err(BirthError { step: BirthStep::Go, errno: errno() }.into());
            }
            said_ready(ours.as_fd(), told, &mut lo_missing, &crate::log::Session::of(root))
        })();
        let (ready, devices) = match ready {
            Ok(devices) => (Ok(()), devices),
            Err(e) => (Err(e), Vec::new()),
        };
        let ns = had;
        static SERIALS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let k = Arc::new(Keeper {
            pid: pid as u32,
            serial: SERIALS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            root: root.to_path_buf(),
            ns,
            devices,
            lo_missing,
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
        k.log().line(
            "sandbox",
            "info",
            &format!("a keeper for {}: pid {}, {}", root.display(), k.pid, k.said()),
        );
        Ok(k)
    }

    /// Whose its log lines are: the session of its checkout (`log::Session::of`).
    pub fn log(&self) -> crate::log::Session {
        crate::log::Session::of(&self.root)
    }

    /// Its tier and namespaces in words (`:procs`, the log): `own (user net ipc pid mnt)`, and a mirror its `lo`
    /// couldn't take (`lo_missing`). Not `tier::get`'s: the probe's own keeper says this while the probe is under way.
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
        format!("{tier} ({}{lo})", self.ns.said())
    }

    /// Reads what the keeper says, until its channel ends.
    fn read(self: Arc<Self>) {
        let mut buf = Vec::new();
        loop {
            match channel::recv(self.chan.as_fd(), &mut buf) {
                Ok(Some((Ok(msg), fds))) => self.heard(msg, fds),
                Ok(Some((Err(e), _))) => self.log().line("sandbox", "warn", &format!("its keeper: {e}")),
                Ok(None) | Err(_) => return self.gone("its channel closed".into()),
            }
        }
    }

    fn heard(self: &Arc<Self>, msg: Msg, mut fds: Vec<OwnedFd>) {
        let mut s = lock(&self.state);
        match msg {
            Msg::Started { id, pid } => {
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
                    _ => self.log().line(
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
                self.log().line("sandbox", "info", &format!("keeper {} reaped {n} left by runs", self.pid));
            }
            #[cfg(test)]
            Msg::Usage { rss_kb, pss_kb } => s.usage = Some((rss_kb, pss_kb)),
            other => self.log().line("sandbox", "warn", &format!("its keeper said {other:?}")),
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
        #[cfg(test)]
        if let Some(d) = s.number_after {
            std::thread::sleep(d);
        }
        pid_of(pidfd)
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
        self.gone(how);
    }

    /// The keeper is gone (`why`): its runs' waits answer that, what they were is ended — failing closed —
    /// and, not asked to end, it died: counted (D11) and said. Died, or killed for not ending in time: what its
    /// runs left (tarae's now, the subreaper) is ended by their tag, and what they held of this machine's
    /// loopback is no session's only as each socket goes (`owners::Owners::let_go`).
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
        // (what its runs had of this machine's loopback stays its session's while anything of theirs may still hold
        // it: ended by the keeper itself, gone now; else — it died, or didn't end in time and was killed — as each
        // socket goes, once what they left was ended, below. Rows are by keeper: one taking its place keeps its own)
        let left_running = !asked || s.killed;
        if !left_running {
            super::owners::OWNERS.ended(self.serial);
        }
        s.gone = Some(why);
        drop(s);
        self.changed.notify_all();
        for (pid, pidfd) in &runs {
            kill_run(*pid, pidfd);
        }
        if !asked {
            self.log().line(
                "sandbox",
                "warn",
                &format!("{said} (pid {}, {})", self.pid, self.root.display()),
            );
        }
        if left_running {
            let (root, serial) = (self.root.clone(), self.serial);
            crate::event::thread("tarae-kleft", move || {
                super::end_left_by_tag(&root);
                let left = super::owners::OWNERS.let_go(serial, super::owners::live, std::thread::sleep);
                if left > 0 {
                    let why = format!(
                        "{left} sockets of what a dead keeper's runs left ({}) still held after {} min: kept as its \
                         session's, no longer asked after",
                        root.display(),
                        super::owners::LET_GO_FOR.as_secs() / 60
                    );
                    crate::log::line("sandbox", "info", &why);
                }
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
                // (what its runs left may outlive it now: `gone` ends it by tag)
                s.killed = true;
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
    /// handoff socket's child end) and waits for its start (`STARTED_WITHIN`): its pid and pidfd. `hold`, a held
    /// run's end of that socket: its byte goes once its pid is known (`heard`).
    fn run(
        &self,
        mut req: channel::Run,
        fds: &[BorrowedFd],
        hold: Option<Arc<OwnedFd>>,
    ) -> io::Result<(u32, Arc<OwnedFd>)> {
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
        if let Err(e) = channel::send(self.chan.as_fd(), &Msg::Run(Box::new(req)), fds) {
            lock(&self.state).runs.remove(&id);
            return Err(e);
        }
        let deadline = Instant::now() + STARTED_WITHIN;
        let mut s = lock(&self.state);
        loop {
            if let Some(started) = s.runs.get(&id).and_then(|r| r.started.clone()) {
                if started.is_err() {
                    s.runs.remove(&id);
                }
                return started.map_err(io::Error::other);
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
}

impl Keeper {
    /// Starts `cmd` — its program, arguments, environment and folder, as `spawn::setsid` takes them: stdin
    /// `/dev/null`, stdout and stderr pipes — through the keeper, confined to the Landlock ruleset `rules`, its
    /// process named `comm` before it confines itself (`landlock::thread_name` — what Landlock's audit names
    /// beside its pid: the caller tells its watch first, `sandbox::spawn`). Its sockets go as `net` says — the
    /// answer `rules` were made for (`linux::confine`'s `Confine.net`): `Own`, its internet sockets are its own
    /// (`seccomp::Mode::OwnNetwork`); `Plain`, it has none; `Served` and `Routed` are `start_served`'s. `rules` are
    /// made by hand (the tier probe's, tests'): a `Confine`'s go only through `start_confined` or `start_served`,
    /// which take the files it hides by mounts with them (outside tests a `Confine` gives its rules to nothing else).
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
        let asks = Asks { no_userns: super::tier::runs_refuse_userns(), ..Asks::default() };
        self.start_with(cmd, comm, rules, net, asks).map(|(s, _)| s)
    }

    /// `start` with the rules `c` made — and the files named like keys it hides by mounts (`linux::Covers`, 7d): a
    /// run of tarae's starts here or through `start_served`, never with `c`'s rules alone (they leave those
    /// files' folders whole, for the mounts to hide them).
    pub fn start_confined(
        &self,
        cmd: &std::process::Command,
        comm: &str,
        c: &super::linux::Confine,
    ) -> io::Result<Started> {
        if matches!(c.net, RunNet::Served | RunNet::Routed) {
            return Err(io::Error::other("a served run starts with its supervisor (start_served)"));
        }
        let (rules, covers) = c.rules_and_covers();
        let asks = Asks { no_userns: super::tier::runs_refuse_userns(), ptys: false, covers: Some(covers) };
        self.start_with(cmd, comm, rules, c.net, asks).map(|(s, _)| s)
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
        self.start_with(cmd, comm, rules, net, Asks { no_userns, ..Asks::default() }).map(|(s, _)| s)
    }

    /// `start`, served or not; with a handoff socket when served or held (a keeper in a PID namespace — its
    /// run waits for tarae to know its pid): tarae's end of it comes back. What's asked of its child besides its
    /// rules: `asks`.
    fn start_with(
        &self,
        cmd: &std::process::Command,
        comm: &str,
        rules: BorrowedFd,
        net: RunNet,
        asks: Asks<'_>,
    ) -> io::Result<(Started, Option<Arc<OwnedFd>>)> {
        let Asks { no_userns, ptys, covers } = asks;
        // (only a keeper in a user namespace can give its run a mount namespace of its own)
        let covers = covers.filter(|c| !c.keys.is_empty() || !c.seals.is_empty());
        // (the break-once: the mounts skipped, the rules left whole — 7d's key tests fail where keys are mounted over)
        #[cfg(test)]
        let covers = covers.filter(|_| std::env::var_os("TARAE_TEST_NO_COVERS").is_none());
        if covers.is_some() && !self.ns.user {
            return Err(io::Error::other("files hidden by mounts, but its keeper has no user namespace"));
        }
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
            ptys,
            shut: covers
                .filter(|c| !c.keys.is_empty())
                .and_then(|c| c.shut.as_ref())
                .map(|s| channel::Covered { dev: s.dev, ino: s.ino, path: bytes(s.path.as_os_str()) }),
            covers: covers
                .iter()
                .flat_map(|c| &c.keys)
                .map(|k| channel::Covered { dev: k.dev, ino: k.ino, path: bytes(k.path.as_os_str()) })
                .collect(),
            seals: covers
                .iter()
                .flat_map(|c| &c.seals)
                .map(|s| channel::Covered { dev: s.dev, ino: s.ino, path: bytes(s.path.as_os_str()) })
                .collect(),
            pins: covers
                .iter()
                .flat_map(|c| &c.pins)
                .map(|s| channel::Covered { dev: s.dev, ino: s.ino, path: bytes(s.path.as_os_str()) })
                .collect(),
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
        let (pid, _pidfd) = self.run(req, &fds, hold)?;
        let spawned = crate::spawn::Spawned { pid, stdout: Some(out_r), stderr: Some(err_r) };
        Ok((Started { spawned }, ours))
    }
}

/// What a run's child is asked besides its rules (`Keeper::start_with`): its filter refuses new user namespaces
/// (`no_userns`, `tier::runs_refuse_userns`); its way to a pty's other end goes to its supervisor (`ptys`: served, a
/// pool granted in its rules — 3b); it hides these files named like keys by mounts first (`covers`, 7d).
#[derive(Clone, Copy, Default)]
struct Asks<'a> {
    no_userns: bool,
    ptys: bool,
    covers: Option<&'a super::linux::Covers>,
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
    /// refusals out on `refused`), then the run with the child's end of a handoff pair; once its filter is made,
    /// its listener comes on that pair (`SCM_RIGHTS`, `hand_over` — no `pidfd_getfd`, which Docker's default profile
    /// refuses), read on a thread of its own (the main loop doesn't wait for it), handed to the supervisor, and
    /// one byte said back: the run goes on to its exec. Anything failing there closes the pair, and the run
    /// fails at its handoff. Its rules, how its sockets go (`Served`, or `Routed`: its own in the session's netns)
    /// and its ptys (3b) are `c`'s.
    pub fn start_served(
        &self,
        cmd: &std::process::Command,
        comm: &str,
        c: &super::linux::Confine,
        policy: Arc<super::Policy>,
        refused: std::sync::mpsc::Sender<super::Denial>,
    ) -> io::Result<Started> {
        let net = c.net;
        if !matches!(net, RunNet::Served | RunNet::Routed) {
            return Err(io::Error::other("a run not served starts on its own (start)"));
        }
        let tree = (self.pid, self.serial);
        let to = super::supervisor::serve(policy, net, refused, tree, c.ptys.clone())?;
        let no_userns = super::tier::runs_refuse_userns();
        let (rules, covers) = c.rules_and_covers();
        let asks = Asks { no_userns, ptys: c.ptys.is_some(), covers: Some(covers) };
        let (started, ours) = self.start_with(cmd, comm, rules, net, asks)?;
        let ours = ours.ok_or_else(|| io::Error::other("a served run without its handoff"))?;
        crate::event::thread("tarae-handoff", move || {
            let Ok(listener) = recv_listener(&ours) else { return };
            if to.send(listener).is_err() {
                return;
            }
            // SAFETY: a send of one byte on our end of the pair.
            unsafe { libc::send(ours.as_raw_fd(), [1u8].as_ptr().cast(), 1, libc::MSG_NOSIGNAL) };
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
}

/// What a birth's child says on its birth pipe of a mirror it couldn't make (`Families`): IPv4's, IPv6's —
/// codes past `BirthStep`'s, as (code, errno); the birth goes on (`said_ready` takes the family off).
const LO_NOTES: [u8; 2] = [0x80, 0x81];
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

/// The address the session's `lo` carries beside loopback where tarae's network has IPv4 (`Families`, `lo:1`, a
/// /32): a documentation address, never anyone's — where a run's proxy variables point too (`route::PROXY`: a
/// connect there is the proxy's, made by the supervisor).
pub const SESSION_V4: std::net::Ipv4Addr = std::net::Ipv4Addr::new(192, 0, 2, 1);
/// `SESSION_V4`'s IPv6 twin (a /128 on `lo`), where tarae's network has IPv6 beyond loopback and link-local.
pub const SESSION_V6: std::net::Ipv6Addr = std::net::Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1);

impl Birth {
    fn new(families: Families, off_lo: bool) -> io::Result<Birth> {
        let pipe = || -> io::Result<(OwnedFd, OwnedFd)> {
            let (r, w) = io::pipe()?;
            Ok((high(r.into())?, high(w.into())?))
        };
        let last_cap = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(63);
        Ok(Birth { go: pipe()?, told: pipe()?, last_cap, families, off_lo })
    }

    /// tarae's ends, the child's closed: the go pipe's writer, the birth pipe's reader.
    fn parents(self) -> (OwnedFd, OwnedFd) {
        (self.go.1, self.told.0)
    }

    /// Says how a step the birth goes on without went (`LO_NOTES`; an errno) on the
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
    ///
    /// # Safety
    /// The birth's child's: raw calls only (a copy of a multithreaded process).
    unsafe fn child(&self, ns: Ns) {
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
                return;
            }
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
            }
            if let Err(e) = capabilities_dropped(self.last_cap) {
                self.failed(BirthStep::Caps, e);
            }
        }
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
/// is put in `missing` and said in the log.
fn said_ready(
    chan: BorrowedFd,
    told: OwnedFd,
    missing: &mut Families,
    log: &crate::log::Session,
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
            if n == 5 && LO_NOTES.contains(&b[0]) {
                let (family, address, lookups) = match b[0] == LO_NOTES[0] {
                    true => (&mut missing.v4, SESSION_V4.to_string(), "IPv4"),
                    false => (&mut missing.v6, SESSION_V6.to_string(), "IPv6"),
                };
                *family = true;
                log.line(
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
                            _ => log.line(
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
    let mut k = Kept { chan, signals, children: HashMap::new(), reaped: 0 };
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
                Ok(Some((Ok(Msg::Run(run)), fds))) => k.start(*run, fds),
                Ok(Some((Ok(Msg::Signal { id, sig }), _))) => k.signal(id, sig),
                Ok(Some((Ok(Msg::AskUsage), _))) => {
                    let (rss_kb, pss_kb) = own_usage();
                    let _ = k.say(&Msg::Usage { rss_kb, pss_kb }, &[]);
                }
                // (a keeper's own kinds, or a malformed record: refused, and it goes on)
                Ok(Some((Ok(_), _))) | Ok(Some((Err(_), _))) => {
                    let _ = k.say(&Msg::Failed { id: 0, errno: libc::EINVAL, step: Step::Request }, &[]);
                }
            }
        }
    }
}

/// The keeper's state.
struct Kept {
    chan: OwnedFd,
    signals: OwnedFd,
    children: HashMap<u64, Child>,
    /// Orphans reaped since `Reaped` last said so.
    reaped: u32,
}

impl Kept {
    fn say(&self, msg: &Msg, fds: &[BorrowedFd]) -> io::Result<()> {
        channel::send(self.chan.as_fd(), msg, fds)
    }

    /// Starts run `run` (its child: `child`, forked), then says `Started` with its pidfd — or `Failed`.
    fn start(&mut self, run: channel::Run, fds: Vec<OwnedFd>) {
        let id = run.id;
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
        // SAFETY: the child is `child` — raw calls on what was made before (`Exec`).
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
            self.children.insert(id, Child { pid, error: None });
            return failed(self, Step::Fork, e);
        }
        // SAFETY: just made, ours.
        let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd as RawFd) };
        drop(error_w);
        drop(fds);
        self.children.insert(id, Child { pid, error: Some(error_r) });
        let started = Msg::Started { id, pid: pid as u32 };
        let _ = self.say(&started, &[pidfd.as_fd()]);
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
                    self.children.remove(&id);
                    let _ = self.say(&Msg::Exited { id, status }, &[]);
                }
                None => self.reaped += 1,
            }
        }
        if self.reaped > 0 {
            let _ = self.say(&Msg::Reaped { n: self.reaped }, &[]);
            self.reaped = 0;
        }
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
        self.reap();
        std::process::exit(0)
    }
}

/// `capset`'s header, version 3 (two data blocks of effective, permitted, inheritable).
#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

/// Every capability gone, for good: the bounding set (an exec as root would take them back from it — this
/// needs `CAP_SETPCAP`, which only a root keeper has), the ambient set, then effective, permitted and
/// inheritable. tarae run as a user holds none of the others, and can't empty its bounding set: that stays this
/// system's, inert under the keeper's `NoNewPrivs` (an exec of a setuid or file-capability program gains nothing
/// from it); as root in a container its runs would hold root's. (A keeper born in namespaces has dropped them
/// all already: `capabilities_dropped`.)
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

/// A run's child, after the keeper's fork: its channel closed, the files named like keys hidden in a
/// mount namespace of its own where it's asked to (`cover`, 7d), dumpable again (finding 1: tarae reads a
/// served run's memory through `/proc/<tid>/mem`, which a non-dumpable process refuses without a namespace of
/// tarae's), a session of its own, its stdio, its folder, its name (what Landlock's audit names), every signal
/// unblocked, its Landlock rules, its seccomp filter — served, put on by a helper thread and its listener sent to
/// tarae (`hand_over`), one byte back — then the exec. Held, it waits first for its byte, sent once tarae knows its
/// pid: one that failed before it could be gone, reaped, when tarae read its pidfd — its start an error ("it ended
/// before it started") where its own failure, on its stderr, was the answer. A failure on the way is written on `error` (step, errno) and in its stderr,
/// and it exits 127.
///
/// Nothing here allocates or frees: after a fork of a process that may have more than one thread (a test's keeper
/// has libtest's thread beside it) only async-signal-safe calls are. Its strings and its seccomp program come made (`Exec`); a failure is said in fixed bytes
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
        #[cfg(test)]
        if exec.leak {
            libc::open(exec.cwd.as_ptr(), libc::O_PATH | libc::O_DIRECTORY);
        }
        libc::setsid();
        for (from, to) in [(stdin, 0), (stdout, 1), (stderr, 2)] {
            if libc::dup2(from.as_raw_fd(), to) < 0 {
                fail(Step::Request, errno());
            }
        }
        // (held: its byte before anything that can fail — the served one comes only after `hand_over`, so this is it)
        if run.held {
            let Some(handoff) = fds.get(4) else { fail(Step::Request, libc::EINVAL) };
            if let Err(e) = byte_from(handoff.as_raw_fd()) {
                fail(Step::Handoff, e);
            }
        }
        if libc::chdir(exec.cwd.as_ptr()) != 0 {
            fail(Step::Chdir, errno());
        }
        // (7d: before dumpable — a new user namespace's capabilities make it undumpable again; after its stdio, which
        // says why it failed)
        if let Some(c) = &exec.covers
            && let Err(e) = cover(c)
        {
            fail(Step::Covers, e);
        }
        libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0);
        libc::prctl(libc::PR_SET_NAME, exec.comm.as_ptr());
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        libc::pthread_sigmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
    }
    if let Err(e) = crate::landlock::restrict_self(ruleset.as_fd()) {
        fail(Step::Landlock, e.raw_os_error().unwrap_or(libc::EIO));
    }
    let handoff = fds.get(4);
    // (served: the filter on a helper thread, its listener sent to tarae before the filter is this thread's —
    // `hand_over`; else on this thread)
    let listener = match (run.served(), handoff) {
        (true, Some(h)) => match hand_over(exec, h.as_raw_fd()) {
            Ok(l) => Some(l),
            Err((step, e)) => fail(step, e),
        },
        (true, None) => fail(Step::Request, libc::EINVAL),
        (false, _) => match exec.filter.apply() {
            Ok(l) => l,
            Err(e) => fail(Step::Seccomp, e.raw_os_error().unwrap_or(libc::EIO)),
        },
    };
    // (served: a byte once tarae has its listener)
    if run.served() {
        let Some(handoff) = handoff else { fail(Step::Request, libc::EINVAL) };
        if let Err(e) = byte_from(handoff.as_raw_fd()) {
            fail(Step::Handoff, e);
        }
    }
    drop(listener);
    // (nothing but its stdio goes past the exec — every descriptor of the keeper's and tarae's comes close-on-exec,
    // and this holds it whatever comes later: one opened before its own mount namespace would reach past the mounts)
    // SAFETY: a plain call on this process's own descriptors.
    if unsafe { libc::syscall(libc::SYS_close_range, 3u32, u32::MAX, libc::CLOSE_RANGE_CLOEXEC) } != 0 {
        fail(Step::Exec, errno());
    }
    // SAFETY: the program, argv and envp are NUL-terminated arrays of strings `exec` holds.
    unsafe { libc::execve(exec.program.as_ptr(), exec.argv_p.as_ptr(), exec.envp_p.as_ptr()) };
    fail(Step::Exec, errno())
}

/// A byte from tarae on `handoff`, in a run's child (a raw read: nothing allocates) — else the errno (`EPIPE`: tarae's
/// end closed).
fn byte_from(handoff: RawFd) -> Result<(), i32> {
    let mut back = [0u8; 1];
    loop {
        // SAFETY: a read into a buffer we own, within it.
        match unsafe { libc::read(handoff, back.as_mut_ptr().cast(), 1) } {
            1 => return Ok(()),
            0 => return Err(libc::EPIPE),
            _ if errno() == libc::EINTR => {}
            _ => return Err(errno()),
        }
    }
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
    /// The stack of the thread that puts a served filter on and makes its listener (`hand_over`).
    stack: Box<[u8]>,
    /// What it hides by mounts first (7d) — None: nothing.
    covers: Option<Covering>,
    /// Tests: its child opens its folder without close-on-exec first thing, before its own mount namespace (its
    /// environment holds `TARAE_TEST_LEAK_FD=1`) — what `close_range` must keep from the program.
    #[cfg(test)]
    leak: bool,
}

/// What a run's child needs to hide the files named like keys (7d, `linux::Covers`; `cover`) and seal the project's
/// protected entries (B9): the shut file (with keys only), each file and what it was (device, inode), each sealed
/// entry and what it was, its own user namespace's id maps (the keeper's ids, the user's — D5) and the last
/// capability to drop.
struct Covering {
    shut: Option<(CString, u64, u64)>,
    keys: Vec<(CString, u64, u64)>,
    seals: Vec<(CString, u64, u64)>,
    pins: Vec<(CString, u64, u64)>,
    uid_map: CString,
    gid_map: CString,
    last_cap: libc::c_ulong,
}

impl Covering {
    fn of(run: &channel::Run) -> Option<Covering> {
        if run.covers.is_empty() && run.seals.is_empty() {
            return None;
        }
        let c = |s: &[u8]| CString::new(s).unwrap_or_default();
        // (keys need the shut file — `channel::valid_run` holds a run to it)
        let shut = match (&run.shut, run.covers.is_empty()) {
            (Some(s), false) => Some((c(&s.path), s.dev, s.ino)),
            (None, true) => None,
            _ => return None,
        };
        // SAFETY: plain calls.
        let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
        let last_cap = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(63);
        Some(Covering {
            shut,
            keys: run.covers.iter().map(|k| (c(&k.path), k.dev, k.ino)).collect(),
            seals: run.seals.iter().map(|s| (c(&s.path), s.dev, s.ino)).collect(),
            pins: run.pins.iter().map(|s| (c(&s.path), s.dev, s.ino)).collect(),
            uid_map: c(format!("{uid} {uid} 1\n").as_bytes()),
            gid_map: c(format!("{gid} {gid} 1\n").as_bytes()),
            last_cap,
        })
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

/// `mount_setattr`'s: read-only, no set-user-ID, devices nor programs run from it (Linux 5.12).
const MOUNT_ATTR_RDONLY: u64 = 0x01;
const MOUNT_ATTR_NOSUID: u64 = 0x02;
const MOUNT_ATTR_NODEV: u64 = 0x04;
const MOUNT_ATTR_NOEXEC: u64 = 0x08;
/// `open_tree`'s: a copy of the mount, detached (Linux 5.2); `move_mount`'s: from and onto descriptors themselves.
const OPEN_TREE_CLONE: libc::c_uint = 1;
const OPEN_TREE_CLOEXEC: libc::c_uint = libc::O_CLOEXEC as libc::c_uint;
const MOVE_MOUNT_F_EMPTY_PATH: libc::c_uint = 0x04;
const MOVE_MOUNT_T_EMPTY_PATH: libc::c_uint = 0x40;

/// `open_tree`'s and `mount_setattr`'s: the whole tree under it, its mounts too.
const AT_RECURSIVE: libc::c_uint = 0x8000;

/// B9: the protected entry at `path` (a real path) bound read-only over itself in the run's mount namespace —
/// opened through no link and checked to be what tarae found (device, inode; a file or a folder, else `ESTALE`), a
/// copy of its mount there (its whole tree), read-only, no set-user-ID nor devices — programs still run from it
/// (`./gradlew`) — moved onto what it opened. Written, it's refused (EROFS); renamed or removed, refused (EBUSY: a
/// mount point); linked, refused (EXDEV). Not `read_only`: a folder on the way to one (`Covers::pins`), bound over
/// itself as it is — a mount point, so renamed or removed, refused (EBUSY), and written as before. Err: an errno.
///
/// # Safety
/// A run's child's, in its own user and mount namespace (`cover`): raw calls on `path`.
unsafe fn seal(path: &std::ffi::CStr, dev: u64, ino: u64, read_only: bool) -> Result<(), i32> {
    // SAFETY: raw calls on this process, `path` and buffers on the stack; each descriptor closed.
    unsafe {
        let mut how: libc::open_how = std::mem::zeroed();
        how.flags = (libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64;
        how.resolve = libc::RESOLVE_NO_SYMLINKS | libc::RESOLVE_NO_MAGICLINKS;
        let size = std::mem::size_of::<libc::open_how>();
        let at = libc::syscall(libc::SYS_openat2, libc::AT_FDCWD, path.as_ptr(), &raw const how, size);
        if at < 0 {
            return Err(errno());
        }
        let at = at as libc::c_int;
        let mut st: libc::stat = std::mem::zeroed();
        let same = libc::fstat(at, &mut st) == 0
            && (st.st_mode & libc::S_IFMT == libc::S_IFDIR
                || read_only && st.st_mode & libc::S_IFMT == libc::S_IFREG)
            && (st.st_dev as u64, st.st_ino as u64) == (dev, ino);
        if !same {
            libc::close(at);
            return Err(libc::ESTALE);
        }
        let empty = c"".as_ptr();
        let flags = OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC | AT_RECURSIVE | libc::AT_EMPTY_PATH as libc::c_uint;
        let tree = libc::syscall(libc::SYS_open_tree, at, empty, flags);
        if tree < 0 {
            let e = errno();
            libc::close(at);
            return Err(e);
        }
        let tree = tree as libc::c_int;
        let attr = MountAttr {
            attr_set: MOUNT_ATTR_RDONLY | MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV,
            attr_clr: 0,
            propagation: 0,
            userns_fd: 0,
        };
        let size = std::mem::size_of::<MountAttr>();
        let set_flags = libc::AT_EMPTY_PATH as libc::c_uint | AT_RECURSIVE;
        let set = !read_only
            || libc::syscall(libc::SYS_mount_setattr, tree, empty, set_flags, &raw const attr, size) == 0;
        let done = set
            && libc::syscall(
                libc::SYS_move_mount,
                tree,
                empty,
                at,
                empty,
                MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH,
            ) == 0;
        let e = errno();
        libc::close(tree);
        libc::close(at);
        match done {
            true => Ok(()),
            false => Err(e),
        }
    }
}

/// 7d: a run's child hides `c`'s files named like keys — and seals the project's protected entries (`seal`, B9)
/// — in a user and mount namespace of its own (`unshare`: it
/// holds every capability there, the keeper none), its ids mapped to themselves; each key opened through no link
/// (`openat2`) and checked to be the file tarae found (device, inode; a regular file) — moved or swapped since, it
/// fails (`ESTALE`); a copy of the shut file's mount (opened through no link, the file tarae checked — else
/// `ESTALE` too), read-only, no set-user-ID, devices nor programs, moved onto
/// what it opened (so onto that file wherever it went since); then every capability dropped for good. The
/// mounts in it, made in this namespace, aren't locked; what takes them back from a run is that it has no
/// capability, and Landlock refuses a confined process every mount and unmount; in a namespace a run makes, they
/// come locked. Err: an errno. Raw calls only, nothing allocated (`child`).
///
/// # Safety
/// A run's child's: raw calls on what `c` holds.
unsafe fn cover(c: &Covering) -> Result<(), i32> {
    // SAFETY: raw calls on this process, `c`'s strings and buffers on the stack; each descriptor closed.
    unsafe {
        if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
            return Err(errno());
        }
        // (its new capabilities made it undumpable, which gives its `/proc/self` to root: its maps couldn't be
        // written — EACCES, measured)
        libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0);
        let maps: [(&std::ffi::CStr, &[u8]); 3] = [
            (c"/proc/self/setgroups", b"deny"),
            (c"/proc/self/uid_map", c.uid_map.as_bytes()),
            (c"/proc/self/gid_map", c.gid_map.as_bytes()),
        ];
        for (file, text) in maps {
            let fd = libc::open(file.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
            if fd < 0 {
                return Err(errno());
            }
            let n = libc::write(fd, text.as_ptr().cast(), text.len());
            let e = errno();
            libc::close(fd);
            if n != text.len() as isize {
                return Err(e);
            }
        }
        // (B9: each protected entry bound read-only over itself — a copy of its own mount at it, its whole tree, so
        // the project around it is one rule; first, so a key inside one is covered in what the run sees. Before
        // them the folders on the way, each bound over itself, outermost first: they can't be moved away with one)
        for (path, dev, ino) in &c.pins {
            seal(path, *dev, *ino, false)?;
        }
        for (path, dev, ino) in &c.seals {
            seal(path, *dev, *ino, true)?;
        }
        for (path, dev, ino) in &c.keys {
            let mut how: libc::open_how = std::mem::zeroed();
            how.flags = (libc::O_PATH | libc::O_CLOEXEC | libc::O_NOFOLLOW) as u64;
            how.resolve = libc::RESOLVE_NO_SYMLINKS | libc::RESOLVE_NO_MAGICLINKS;
            let size = std::mem::size_of::<libc::open_how>();
            let key = libc::syscall(libc::SYS_openat2, libc::AT_FDCWD, path.as_ptr(), &raw const how, size);
            if key < 0 {
                return Err(errno());
            }
            let key = key as libc::c_int;
            let mut st: libc::stat = std::mem::zeroed();
            let same = libc::fstat(key, &mut st) == 0
                && st.st_mode & libc::S_IFMT == libc::S_IFREG
                && (st.st_dev as u64, st.st_ino as u64) == (*dev, *ino);
            if !same {
                libc::close(key);
                return Err(libc::ESTALE);
            }
            // (the shut file through no link, and only the one tarae checked — swapped since, it fails as a key does)
            let flags = OPEN_TREE_CLONE | OPEN_TREE_CLOEXEC | libc::AT_SYMLINK_NOFOLLOW as libc::c_uint;
            let Some((shut_at, shut_dev, shut_ino)) = &c.shut else {
                libc::close(key);
                return Err(libc::EINVAL);
            };
            let tree = libc::syscall(libc::SYS_open_tree, libc::AT_FDCWD, shut_at.as_ptr(), flags);
            if tree < 0 {
                let e = errno();
                libc::close(key);
                return Err(e);
            }
            let tree = tree as libc::c_int;
            let mut st: libc::stat = std::mem::zeroed();
            let same = libc::fstat(tree, &mut st) == 0
                && st.st_mode & libc::S_IFMT == libc::S_IFREG
                && (st.st_dev as u64, st.st_ino as u64) == (*shut_dev, *shut_ino);
            if !same {
                libc::close(tree);
                libc::close(key);
                return Err(libc::ESTALE);
            }
            let shut = MOUNT_ATTR_RDONLY | MOUNT_ATTR_NOSUID | MOUNT_ATTR_NODEV | MOUNT_ATTR_NOEXEC;
            let attr = MountAttr { attr_set: shut, attr_clr: 0, propagation: 0, userns_fd: 0 };
            let empty = c"".as_ptr();
            let size = std::mem::size_of::<MountAttr>();
            let set = libc::syscall(
                libc::SYS_mount_setattr,
                tree,
                empty,
                libc::AT_EMPTY_PATH,
                &raw const attr,
                size,
            ) == 0;
            if !set {
                let e = errno();
                libc::close(tree);
                libc::close(key);
                return Err(e);
            }
            let done = libc::syscall(
                libc::SYS_move_mount,
                tree,
                empty,
                key,
                empty,
                MOVE_MOUNT_F_EMPTY_PATH | MOVE_MOUNT_T_EMPTY_PATH,
            ) == 0;
            let e = errno();
            libc::close(tree);
            libc::close(key);
            if !done {
                return Err(e);
            }
        }
        capabilities_dropped(c.last_cap)
    }
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
        let filter = crate::seccomp::Filter::new(mode, run.no_userns, run.ptys)?;
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
            stack: vec![0u8; HELPER_STACK].into_boxed_slice(),
            covers: Covering::of(run),
            #[cfg(test)]
            leak: run.envp.iter().any(|e| e.as_slice() == b"TARAE_TEST_LEAK_FD=1"),
        })
    }
}

/// The helper thread's stack (`hand_over`): it puts the filter on and makes two calls.
const HELPER_STACK: usize = 64 << 10;

/// What the helper thread says the main one (`hand_over`): its listener's number, or `-errno` — `WAITING` until then.
static HELPER_LISTENER: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(WAITING);
/// 0: before the handoff; 1: the listener is tarae's, sync; 2: synced (`HELPER_SYNCED` says how).
static HELPER_STEP: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);
/// `TSYNC`'s answer: 0, or `-errno` (a thread it couldn't sync: `-ESRCH`).
static HELPER_SYNCED: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
const WAITING: i32 = i32::MIN;

/// A served run's filter and listener, its listener handed to tarae with no `pidfd_getfd` (6f′, G1 (b) — Docker's
/// default profile refuses that call; measured in M8b on 6.1, 6.8 and 7.3, `target/sb/m8b/`). Once the filter is on,
/// a `sendmsg` goes to the listener, which nobody reads before tarae has it — so the filter goes on a helper thread of
/// the child (`helper`) first: the main thread, still unfiltered, sends the listener on the handoff socket
/// (`SCM_RIGHTS`), and the helper then syncs it onto its filter (`SECCOMP_FILTER_FLAG_TSYNC` with an allow-all filter
/// on top: the main thread gets the helper's whole chain, `no_new_privs` with it). The child's exec ends the helper.
/// No allocation: the stack is `Exec`'s, the rest is raw calls and atomics (a raw clone's child — `child`).
fn hand_over(exec: &Exec, handoff: RawFd) -> Result<OwnedFd, (Step, i32)> {
    use std::sync::atomic::Ordering::SeqCst;
    HELPER_LISTENER.store(WAITING, SeqCst);
    HELPER_STEP.store(0, SeqCst);
    let top = (exec.stack.as_ptr() as usize + exec.stack.len()) & !15;
    let flags = libc::CLONE_VM
        | libc::CLONE_FS
        | libc::CLONE_FILES
        | libc::CLONE_SIGHAND
        | libc::CLONE_THREAD
        | libc::CLONE_SYSVSEM;
    // SAFETY: a thread on a stack of its own that outlives it (`Exec` lives until the exec, which ends the thread);
    // its argument is `exec`, read only. Without `CLONE_SETTLS` it shares this thread's TLS: it calls nothing that
    // touches it but `errno` (which this thread doesn't read meanwhile).
    let r = unsafe {
        libc::clone(helper, top as *mut libc::c_void, flags, (exec as *const Exec).cast_mut().cast())
    };
    if r < 0 {
        return Err((Step::Seccomp, errno()));
    }
    let l = loop {
        match HELPER_LISTENER.load(SeqCst) {
            WAITING => {
                // SAFETY: plain call.
                unsafe { libc::sched_yield() };
            }
            l => break l,
        }
    };
    if l < 0 {
        return Err((Step::Seccomp, -l));
    }
    // SAFETY: the helper made it, in the descriptors both threads share; ours from here.
    let listener = unsafe { OwnedFd::from_raw_fd(l) };
    if !send_listener(handoff, l) {
        return Err((Step::Handoff, errno()));
    }
    HELPER_STEP.store(1, SeqCst);
    while HELPER_STEP.load(SeqCst) != 2 {
        // SAFETY: plain call.
        unsafe { libc::sched_yield() };
    }
    match HELPER_SYNCED.load(SeqCst) {
        0 => Ok(listener),
        e => Err((Step::Seccomp, -e)),
    }
}

/// The helper thread (`hand_over`): the filter put on itself and its listener said, then — once that's tarae's — the
/// main thread synced onto it.
extern "C" fn helper(arg: *mut libc::c_void) -> libc::c_int {
    use std::sync::atomic::Ordering::SeqCst;
    // SAFETY: `hand_over`'s `Exec`, alive until the exec.
    let exec = unsafe { &*arg.cast::<Exec>() };
    let l = match exec.filter.apply() {
        Ok(Some(l)) => std::os::fd::IntoRawFd::into_raw_fd(l),
        Ok(None) => -libc::EINVAL,
        Err(e) => -e.raw_os_error().unwrap_or(libc::EIO),
    };
    HELPER_LISTENER.store(l, SeqCst);
    if l < 0 {
        return 0;
    }
    while HELPER_STEP.load(SeqCst) != 1 {
        // SAFETY: plain call.
        unsafe { libc::sched_yield() };
    }
    let mut allow = [libc::sock_filter {
        code: (libc::BPF_RET | libc::BPF_K) as u16,
        jt: 0,
        jf: 0,
        k: libc::SECCOMP_RET_ALLOW,
    }];
    let prog = libc::sock_fprog { len: 1, filter: allow.as_mut_ptr() };
    // SAFETY: a program that outlives the call; every thread of the process synced onto this one's filters.
    let r = unsafe {
        libc::syscall(
            libc::SYS_seccomp,
            libc::SECCOMP_SET_MODE_FILTER,
            libc::SECCOMP_FILTER_FLAG_TSYNC,
            &raw const prog,
        )
    };
    let synced = match r {
        0 => 0,
        r if r > 0 => -libc::ESRCH,
        _ => -errno(),
    };
    HELPER_SYNCED.store(synced, SeqCst);
    HELPER_STEP.store(2, SeqCst);
    0
}

/// The listener `l` sent on `handoff` with one byte (`SCM_RIGHTS`) — the child's half of the handoff; nothing
/// allocated. False: not sent (`errno` says why).
fn send_listener(handoff: RawFd, l: RawFd) -> bool {
    let mut byte = *b"L";
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    let mut control = [0u64; 4];
    // SAFETY: a zeroed header pointed at buffers of ours, the one control message written within its buffer
    // (`CMSG_SPACE` of one descriptor: 24 bytes, of 32).
    unsafe {
        let mut m: libc::msghdr = std::mem::zeroed();
        m.msg_iov = &raw mut iov;
        m.msg_iovlen = 1;
        m.msg_control = control.as_mut_ptr().cast();
        m.msg_controllen = libc::CMSG_SPACE(4) as _;
        let c = libc::CMSG_FIRSTHDR(&m);
        (*c).cmsg_level = libc::SOL_SOCKET;
        (*c).cmsg_type = libc::SCM_RIGHTS;
        (*c).cmsg_len = libc::CMSG_LEN(4) as _;
        std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<RawFd>(), l);
        loop {
            match libc::sendmsg(handoff, &m, libc::MSG_NOSIGNAL) {
                1 => return true,
                _ if errno() == libc::EINTR => {}
                _ => return false,
            }
        }
    }
}

/// tarae's half of the handoff: the listener a served run's child sent (`send_listener`) — close-on-exec.
fn recv_listener(ours: &OwnedFd) -> io::Result<OwnedFd> {
    let mut byte = [0u8];
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    let mut control = [0u64; 4];
    // SAFETY: a zeroed header pointed at buffers of ours; the control data read only within what the kernel wrote,
    // its one descriptor taken once.
    unsafe {
        let mut m: libc::msghdr = std::mem::zeroed();
        m.msg_iov = &raw mut iov;
        m.msg_iovlen = 1;
        m.msg_control = control.as_mut_ptr().cast();
        m.msg_controllen = std::mem::size_of_val(&control) as _;
        let n = loop {
            let n = libc::recvmsg(ours.as_raw_fd(), &mut m, libc::MSG_CMSG_CLOEXEC);
            if n >= 0 || errno() != libc::EINTR {
                break n;
            }
        };
        if n != 1 {
            return Err(io::Error::other("the run's child sent no listener"));
        }
        let c = libc::CMSG_FIRSTHDR(&m);
        if c.is_null() || (*c).cmsg_level != libc::SOL_SOCKET || (*c).cmsg_type != libc::SCM_RIGHTS {
            return Err(io::Error::other("the run's child's message carried no descriptor"));
        }
        Ok(OwnedFd::from_raw_fd(std::ptr::read_unaligned(libc::CMSG_DATA(c).cast::<RawFd>())))
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

    /// A run holds no capability — none effective, permitted, inheritable nor ambient — and gains none by an exec
    /// (`NoNewPrivs`). Its bounding set is empty too where its keeper could empty it: root's (`drop_capabilities`)
    /// or one born in namespaces (`capabilities_dropped`). A user's keeper without namespaces (tier host as a user)
    /// can't (`CAP_SETPCAP`): the bounding set stays this system's, inert under `NoNewPrivs` — a setuid or
    /// file-capability program gets nothing from it.
    #[test]
    fn a_command_holds_no_capability() {
        let Some(rules) = open_rules("a_command_holds_no_capability") else { return };
        let k = keeper("caps");
        let (out, status) = sh_through(
            &k,
            &rules,
            "exec grep -E '^(Cap(Inh|Prm|Eff|Bnd|Amb)|NoNewPrivs):' /proc/self/status",
        );
        k.end();
        assert!(status.success(), "{out}");
        // SAFETY: a plain query.
        let bounding_kept = unsafe { libc::geteuid() } != 0 && !ns().user;
        let held: Vec<&str> = out
            .lines()
            .filter(|l| l.starts_with("Cap") && !l.ends_with("0000000000000000"))
            .filter(|l| !(bounding_kept && l.starts_with("CapBnd:")))
            .collect();
        assert_eq!((out.lines().count(), held), (6, Vec::<&str>::new()), "{out}");
        assert!(out.lines().any(|l| l == "NoNewPrivs:\t1"), "no capability from an exec: {out}");
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
        let s = k.start_served(&cmd, &name, &c, Arc::new(p), tx).unwrap();
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

    /// A tarae session for the keeper's lifetime tests: an editor on a project of its own, the sandbox on, and
    /// Claude's runs going tarae's whole way (`tools::guard_then_go`: the policy made on a worker, its session's
    /// keeper found or made — `sandbox::confined`), in surroundings of the test's own (tarae's cache in `base`);
    /// its chat's claude a stand-in.
    struct Lived {
        ed: crate::editor::Editor,
        root: PathBuf,
    }

    impl Lived {
        /// A session on `base/name` (`base`: out of tarae's repository — a run takes its snapshot with git — and
        /// out of the temp folders, which every run writes).
        fn new(base: &Path, name: &str) -> Lived {
            let root = base.join(name);
            std::fs::create_dir_all(&root).unwrap();
            let mut ed = crate::editor::Editor::new(crate::config::Config::default());
            (ed.config.llm.sandbox, ed.sandbox) = (true, super::super::Availability::Ready);
            // (a stand-in claude: it reads what it's sent and says nothing)
            ed.config.llm.command = "sh".into();
            ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
            ed.session.root = root.clone();
            crate::chat::open(&mut ed);
            ed.session.chat.as_mut().unwrap().conversation = Some(format!("{name}-1"));
            let cache = base.join("cache");
            ed.sandbox_here = Some(Arc::new(move || {
                let mut here = super::super::Here::now();
                here.env.insert("XDG_CACHE_HOME".into(), cache.clone().into());
                here
            }));
            Lived { ed, root }
        }

        /// `command` run as Claude's in this session — a call of its chat's turn under way —, to its end: its
        /// answer.
        fn run(&mut self, command: &str) -> String {
            use crate::tools::{ANSWERED, Call, For, Job};
            // (calls of every session's are answered into one list: an id of their own)
            static CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let n = CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let id = serde_json::json!(n);
            let c = self.ed.session.chat.as_mut().unwrap();
            c.state = crate::chat::State::Thinking;
            let call = Call::of(c.generation(), &format!("r{n}"), id.clone());
            let job = Job::Command(command.into(), self.root.clone());
            crate::tools::guard_then_go(&mut self.ed, For::Claude(call), job, Default::default());
            let end = Instant::now() + Duration::from_secs(30);
            loop {
                if let Some((_, answer)) =
                    ANSWERED.with(|a| a.borrow().iter().find(|(i, _)| *i == id).cloned())
                {
                    return answer.unwrap_or_else(|e| e);
                }
                if Instant::now() >= end {
                    let toasts: Vec<_> = self.ed.toasts.iter().map(|t| t.text.clone()).collect();
                    let (cards, task) = (self.ed.offers.len(), self.ed.session.task_run.is_some());
                    panic!("{command}: no answer — toasts {toasts:?}, {cards} cards, a task run: {task}");
                }
                if let Some(ev) = self.ed.events.recv_timeout(Duration::from_millis(20)) {
                    self.ed.handle_event(ev);
                }
            }
        }

        /// The session's keeper — the one taking its runs.
        fn keeper(&self) -> Arc<Keeper> {
            let current: Vec<_> =
                of(Some(&self.root)).into_iter().filter(|k| !lock(&k.state).superseded).collect();
            assert_eq!(current.len(), 1, "one keeper takes the session's runs");
            current[0].clone()
        }

        /// A daemon left by a run of this session's: a run that starts one in the background and ends, then the
        /// daemon under the session's keeper.
        fn daemon(&mut self) -> u32 {
            let said = self.run("setsid sleep 60 >/dev/null 2>&1 &");
            let k = self.keeper();
            let end = Instant::now() + Duration::from_secs(5);
            loop {
                match k.leftovers()[..] {
                    [pid] => return pid,
                    _ => assert!(Instant::now() < end, "a daemon under the keeper: {said}"),
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn end(self) {
            super::super::end_leftovers(Some(&self.root));
        }
    }

    /// Whether `pid` is alive (there, and no zombie).
    fn alive(pid: u32) -> bool {
        crate::procs::read(pid).is_some_and(|o| !o.zombie)
    }

    /// The folder `Lived` sessions of `test` live in, made empty.
    fn lived_base(test: &str) -> PathBuf {
        // (the caller's to remove, as before)
        crate::scratch::Scratch::in_dir(std::path::Path::new("/var/tmp"), test).into_path()
    }

    /// D13: the boundary is the session, not the conversation — a new conversation in the chat (`C-l`:
    /// `chat::reset`) goes on with the session's keeper, and a daemon a run of the last conversation left is
    /// alive after the new one's first run. Another session's runs between (another project, so another
    /// policy key) leave it alone too.
    #[test]
    fn a_new_conversation_keeps_the_keeper() {
        let test = "a_new_conversation_keeps_the_keeper";
        if open_rules(test).is_none() {
            return;
        }
        let base = lived_base(test);
        let (mut mine, mut theirs) = (Lived::new(&base, "mine"), Lived::new(&base, "theirs"));
        let daemon = mine.daemon();
        let k = mine.keeper();
        let their_daemon = theirs.daemon();
        assert_ne!(theirs.keeper().pid, k.pid, "each session its own keeper");
        crate::chat::reset(&mut mine.ed);
        assert!(mine.ed.session.chat.as_ref().unwrap().conversation.is_none(), "a new conversation");
        let said = mine.run("true");
        assert_eq!(mine.keeper().pid, k.pid, "the same keeper: {said}");
        assert!(!k.is_gone());
        assert!(alive(daemon), "the last conversation's daemon goes on");
        assert_eq!(k.leftovers(), [daemon]);
        assert!(alive(their_daemon), "the other session's too");
        mine.end();
        theirs.end();
        std::fs::remove_dir_all(&base).ok();
    }

    /// D14: a session's policy key changed (a setting — here `llm.sandbox-write` — between two runs), its next
    /// run gets a new keeper, and the old one's daemons end with it: a daemon keeps the rights it started with,
    /// narrower or wider. Another session's keeper and daemon go on.
    #[test]
    fn a_key_change_ends_the_keepers_daemons() {
        let test = "a_key_change_ends_the_keepers_daemons";
        if open_rules(test).is_none() {
            return;
        }
        let base = lived_base(test);
        let (mut mine, mut theirs) = (Lived::new(&base, "mine"), Lived::new(&base, "theirs"));
        let daemon = mine.daemon();
        let old = mine.keeper();
        let their_daemon = theirs.daemon();
        let their_keeper = theirs.keeper();
        let granted = base.join("granted");
        std::fs::create_dir_all(&granted).unwrap();
        mine.ed.config.llm.sandbox_write.push(granted.display().to_string());
        let said = mine.run("true");
        let new = mine.keeper();
        assert_ne!(new.pid, old.pid, "a new keeper for the new key: {said}");
        assert!(gone_within(&old, END_WITHIN), "the old keeper ends");
        let end = Instant::now() + LEFT_TERM * 3;
        while alive(daemon) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(daemon), "its daemon ended with it");
        assert!(!their_keeper.is_gone() && alive(their_daemon), "the other session's go on");
        assert_eq!(theirs.keeper().pid, their_keeper.pid);
        mine.end();
        theirs.end();
        std::fs::remove_dir_all(&base).ok();
    }

    /// D13 gives way to D14 (the user's decision 2026-10-10): a new conversation goes on with the keeper only
    /// where the last one granted nothing. A write granted for the last conversation (`a` on its card:
    /// `Chat.allowed`) is in the policy key; the new one (`C-l`) grants it no more, so its first run gets a new
    /// keeper and the last conversation's daemons end — a daemon mustn't keep a right no longer granted.
    #[test]
    fn a_new_conversation_after_a_grant_ends_the_keepers_daemons() {
        let test = "a_new_conversation_after_a_grant_ends_the_keepers_daemons";
        if open_rules(test).is_none() {
            return;
        }
        let base = lived_base(test);
        let mut mine = Lived::new(&base, "mine");
        let granted = base.join("granted");
        std::fs::create_dir_all(&granted).unwrap();
        mine.ed.session.chat.as_mut().unwrap().allowed.write.push(granted);
        let daemon = mine.daemon();
        let old = mine.keeper();
        crate::chat::reset(&mut mine.ed);
        assert!(
            mine.ed.session.chat.as_ref().unwrap().allowed.write.is_empty(),
            "the grant was the last one's"
        );
        let said = mine.run("true");
        assert_ne!(mine.keeper().pid, old.pid, "a new keeper: {said}");
        assert!(gone_within(&old, END_WITHIN), "the old keeper ends");
        let end = Instant::now() + LEFT_TERM * 3;
        while alive(daemon) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(daemon), "the last conversation's daemon ended with it");
        mine.end();
        std::fs::remove_dir_all(&base).ok();
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
        let (p, root) = project_policy(name, ports, files);
        let abi = crate::landlock::abi().unwrap();
        let c =
            super::super::linux::confine(&p, abi, Err("not watched".into()), false, keeper, serve).unwrap();
        (p, c, root)
    }

    /// A project's policy with these ports open and these (empty) files: (the policy, the project).
    fn project_policy(name: &str, ports: Vec<u16>, files: &[&str]) -> (super::super::Policy, PathBuf) {
        project_policy_with(name, ports, files, |_| {})
    }

    /// `project_policy`, what it's made from `change`d.
    fn project_policy_with(
        name: &str,
        ports: Vec<u16>,
        files: &[&str],
        change: impl FnOnce(&mut super::super::Env),
    ) -> (super::super::Policy, PathBuf) {
        let root = std::env::temp_dir().join(format!("tarae-keeper-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        for f in files {
            std::fs::write(root.join(f), "").unwrap();
        }
        (project_policy_at(&root, ports, change), root)
    }

    /// The policy of the project at `root` (made, its real path) with these ports open, what it's made from
    /// `change`d.
    fn project_policy_at(
        root: &Path,
        ports: Vec<u16>,
        change: impl FnOnce(&mut super::super::Env),
    ) -> super::super::Policy {
        let mut env = super::super::Env {
            root: root.to_path_buf(),
            tmp: Some(super::super::profile_path(&std::env::temp_dir())),
            ports,
            run_env: super::super::run_env_of(&super::super::Here::now(), &crate::llm::LlmConfig::default()),
            ..Default::default()
        };
        change(&mut env);
        super::super::Policy::for_project(&env)
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
                let s = k.start_confined(&cmd, &crate::landlock::thread_name(), &plain).unwrap();
                crate::spawn::wait(s.spawned.pid).unwrap();
            });
            let keeper_served = served.then(|| {
                median_ms(50, || {
                    let s = k
                        .start_served(&cmd, &crate::landlock::thread_name(), &c, policy.clone(), tx.clone())
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

    /// What `perf_keepers_memory_per_session` reads at one moment: each keeper's own (Rss, Pss) as it says it
    /// (`usage`), this process's (`own_usage`), its threads by name and its descriptors by kind.
    struct Seen {
        keepers: Vec<(u64, u64)>,
        mine: (u64, u64),
        threads: std::collections::BTreeMap<String, i64>,
        fds: std::collections::BTreeMap<String, i64>,
    }

    impl Seen {
        fn now(keepers: &[Arc<Keeper>]) -> Seen {
            let mut threads = std::collections::BTreeMap::new();
            for t in std::fs::read_dir("/proc/self/task").into_iter().flatten().flatten() {
                if let Ok(name) = std::fs::read_to_string(t.path().join("comm")) {
                    *threads.entry(name.trim().to_string()).or_default() += 1;
                }
            }
            // (a socket, a pipe, an anonymous inode by its name — `[pidfd]`, `[eventfd]`… —, a pty's end — the
            // pool's, 3b —, else a path)
            let mut fds = std::collections::BTreeMap::new();
            for fd in std::fs::read_dir("/proc/self/fd").into_iter().flatten().flatten() {
                let Ok(to) = std::fs::read_link(fd.path()) else { continue };
                let to = to.to_string_lossy();
                let kind = match to.split_once(':') {
                    Some(("anon_inode", name)) => name.to_string(),
                    Some((kind @ ("socket" | "pipe"), _)) => kind.to_string(),
                    _ if to.starts_with("/dev/pts/") || to == "/dev/ptmx" => "pty".to_string(),
                    _ => "path".to_string(),
                };
                *fds.entry(kind).or_default() += 1;
            }
            let keepers = keepers.iter().map(|k| k.usage().expect("a keeper says what it uses")).collect();
            Seen { keepers, mine: own_usage(), threads, fds }
        }

        /// The keepers' Pss: in all, and each one's lowest and highest — MB.
        fn keepers_pss(&self) -> String {
            let each = self.keepers.iter().map(|u| u.1);
            let (lo, hi) = (each.clone().min().unwrap_or(0), each.clone().max().unwrap_or(0));
            let mb = |kb: u64| kb as f64 / 1024.0;
            format!("{:.1} ({:.1}–{:.1})", mb(each.sum()), mb(lo), mb(hi))
        }

        /// This process's Pss / Rss grown since `then`, per session — KB.
        fn mine_since(&self, then: &Seen, n: usize) -> String {
            let per = |now: u64, was: u64| (now as i64 - was as i64) / n as i64;
            format!("{:+} / {:+}", per(self.mine.1, then.mine.1), per(self.mine.0, then.mine.0))
        }

        /// The threads more (or fewer) than `then`, by name.
        fn threads_since(&self, then: &Seen) -> String {
            Self::more(&self.threads, &then.threads)
        }

        /// The descriptors more (or fewer) than `then`, by kind.
        fn fds_since(&self, then: &Seen) -> String {
            Self::more(&self.fds, &then.fds)
        }

        /// What `now` counts more (or fewer) of than `then`, by name.
        fn more(
            now: &std::collections::BTreeMap<String, i64>,
            then: &std::collections::BTreeMap<String, i64>,
        ) -> String {
            let mut names: Vec<&String> = now.keys().chain(then.keys()).collect();
            names.sort();
            names.dedup();
            let more: Vec<String> = names
                .into_iter()
                .filter_map(|name| {
                    let d = now.get(name).unwrap_or(&0) - then.get(name).unwrap_or(&0);
                    (d != 0).then(|| format!("{name} {d:+}"))
                })
                .collect();
            if more.is_empty() { "none".into() } else { more.join(", ") }
        }

        /// The descriptors open, in all.
        fn fds_open(&self) -> i64 {
            self.fds.values().sum()
        }
    }

    /// This process's limit on open descriptors: (soft, hard).
    fn fd_limit() -> (u64, u64) {
        let mut l = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: reads this process's limit into a struct of ours.
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut l) };
        (l.rlim_cur, l.rlim_max)
    }

    /// Step 8 (`docs/design/sandbox-keeper.md` §Measured): memory with 1, 8, 16 and 32 sessions, in each tier
    /// served here (`served_tiers`). Each session is a keeper of its own, left idle; then each keeps one idle run,
    /// served, holding a connection through the proxy to a stand-in outside (`proxyhold`) — what a session whose
    /// daemon went out once holds: its supervisor, the proxy's thread for the connection and its relay. A table
    /// row per tier and count: the keepers' own Pss (in all, each one's lowest–highest), idle and with their run;
    /// this process's (tarae's stand-in) Pss / Rss grown per session, for the keepers and then for the runs; its
    /// threads more, by name, and its descriptors more, by kind. Where a run can't be made (descriptors run out),
    /// the row says after how many, and the tier's larger counts aren't tried. The runs' own processes (the test
    /// binary) are neither's. A measurement, no budget (the user, 2026-10-10). Run in a process of its own with the
    /// descriptor limit raised as tarae's `main` raises it (`nofile::raise` changes the process) — not raised with
    /// `TARAE_TEST_NO_RAISE` set, as before it was.
    #[test]
    fn perf_keepers_memory_per_session() {
        if crate::test_results::unoptimized("sandbox::keeper::tests::perf_keepers_memory_per_session") {
            return;
        }
        let status = scene_cmd("keepers_memory_scene", &[]).status().unwrap();
        assert!(status.success(), "the scene failed: {status}");
    }

    #[test]
    #[ignore = "run by perf_keepers_memory_per_session, in a process of its own"]
    fn keepers_memory_scene() {
        if std::env::var_os(SCENE).is_none() {
            return;
        }
        if std::env::var_os("TARAE_TEST_NO_RAISE").is_none()
            && let Some(said) = crate::nofile::raise()
        {
            println!("{said}");
        }
        let Some(tiers) = served_tiers("perf_keepers_memory_per_session") else { return };
        let Some(ip) = outside() else { return };
        // (the outside: each connection's first line answered, then kept open — the run's relay stays, idle)
        let l = std::net::TcpListener::bind((ip, 0)).unwrap();
        let at = l.local_addr().unwrap();
        let kept = Arc::new(Mutex::new(Vec::new()));
        let keep = kept.clone();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                s.set_read_timeout(Some(Duration::from_secs(5))).ok();
                let mut line = String::new();
                let _ = io::BufRead::read_line(&mut std::io::BufReader::new(&s), &mut line);
                let _ = io::Write::write_all(&mut &s, b"out hello\n");
                lock(&keep).push(s);
            }
        });
        let host = "kept-open.test";
        super::super::proxy::TEST_NAMES.lock().unwrap().push((host.into(), at));
        let env = [
            ("TARAE_TO", super::super::route::PROXY.to_string()),
            ("TARAE_HOST", format!("{host}:{}", at.port())),
        ];
        let env: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
        // (long enough for what starts a run — the handoff, a connect's own supervisor thread — to have ended)
        const IDLE: Duration = Duration::from_secs(1);
        let (soft, hard) = fd_limit();
        let mut rows = vec![
            format!("descriptors: at most {soft} open (hard limit {hard})"),
            "| tier | sessions | keepers' Pss, MB: idle | … with a run each | tarae's Pss / Rss, KB per session: \
             keepers | … runs | tarae's threads more | tarae's descriptors more (open in all) |"
                .to_string(),
            "|---|---|---|---|---|---|---|---|".to_string(),
        ];
        let abi = crate::landlock::abi().unwrap();
        'tiers: for (tier, ns) in tiers {
            for n in [1, 8, 16, 32] {
                let before = Seen::now(&[]);
                let ks: Vec<Arc<Keeper>> =
                    (0..n).map(|i| keeper_for(&format!("mem-{tier}-{n}-{i}"), ns)).collect();
                std::thread::sleep(IDLE);
                let idle = Seen::now(&ks);
                let mut runs = Vec::new();
                let mut stopped = None;
                for (i, k) in ks.iter().enumerate() {
                    let (p, root) = project_policy(&format!("mem-{tier}-{n}-{i}"), Vec::new(), &[]);
                    let c = match super::super::linux::confine(
                        &p,
                        abi,
                        Err("not watched".into()),
                        false,
                        Some(k),
                        Ok(()),
                    ) {
                        Ok(c) => c,
                        Err(e) => {
                            stopped = Some(format!("session {}'s rules: {e}", i + 1));
                            std::fs::remove_dir_all(&root).ok();
                            break;
                        }
                    };
                    let egress = super::super::proxy::egress::Egress { any: true, ..Default::default() };
                    let p = Arc::new(super::super::Policy {
                        network: true,
                        own_addresses: Some(Vec::new()),
                        egress,
                        ..p
                    });
                    let (mut lines, pid, refused) = served_helper(k, (&p, &c, &root), "proxyhold", &env);
                    let said = said_next(&mut lines);
                    assert!(said.ends_with("reply=out hello"), "{tier} {n}: {said} {:?}", refused.try_recv());
                    runs.push((lines, pid, refused, c, root));
                }
                std::thread::sleep(IDLE);
                let held = Seen::now(&ks);
                rows.push(match &stopped {
                    None => format!(
                        "| {tier} | {n} | {} | {} | {} | {} | keepers: {} · runs: {} | keepers: {} · runs: {} ({}) |",
                        idle.keepers_pss(),
                        held.keepers_pss(),
                        idle.mine_since(&before, n),
                        held.mine_since(&idle, n),
                        idle.threads_since(&before),
                        held.threads_since(&idle),
                        idle.fds_since(&before),
                        held.fds_since(&idle),
                        held.fds_open(),
                    ),
                    Some(why) => format!(
                        "| {tier} | {n} | {} | stopped after {} runs — {why} | {} | | | keepers: {} · runs: {} ({}) |",
                        idle.keepers_pss(),
                        runs.len(),
                        idle.mine_since(&before, n),
                        idle.fds_since(&before),
                        held.fds_since(&idle),
                        held.fds_open(),
                    ),
                });
                for (.., root) in &runs {
                    std::fs::write(root.join("done"), "").unwrap();
                }
                for (lines, pid, _, _, root) in runs {
                    lines.for_each(drop);
                    crate::spawn::wait(pid).unwrap();
                    std::fs::remove_dir_all(&root).ok();
                }
                for k in ks {
                    k.end();
                }
                lock(&kept).clear();
                if stopped.is_some() {
                    continue 'tiers;
                }
            }
        }
        let line = format!("keepers' memory per session (step 8)\n{}\n", rows.join("\n"));
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

    /// A keeper is born with the descriptor limit tarae started with (`nofile::restore` before its exec), so its runs
    /// have that one, not tarae's raised one — no shim needed for them. A scene of its own (tarae's raise changes the
    /// process), started with a soft limit of 256.
    #[test]
    fn a_keepers_runs_get_the_descriptor_limit_tarae_started_with() {
        use std::os::unix::process::CommandExt as _;
        if open_rules("a_keepers_runs_get_the_descriptor_limit_tarae_started_with").is_none() {
            return;
        }
        let mut cmd = scene_cmd("keeper_limit_scene", &[]);
        // SAFETY: getrlimit/setrlimit only, between fork and exec.
        unsafe {
            cmd.pre_exec(|| {
                let mut l = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                libc::getrlimit(libc::RLIMIT_NOFILE, &mut l);
                l.rlim_cur = 256;
                libc::setrlimit(libc::RLIMIT_NOFILE, &l);
                Ok(())
            });
        }
        let said = said_by("keeper_limit_scene", cmd);
        if said == "nothing to raise" {
            eprintln!(
                "a_keepers_runs_get_the_descriptor_limit_tarae_started_with: the hard limit is 256 here"
            );
            return;
        }
        assert_eq!(said, "raised, its run has 256");
    }

    #[test]
    #[ignore = "run by a_keepers_runs_get_the_descriptor_limit_tarae_started_with, in a process of its own"]
    fn keeper_limit_scene() {
        if std::env::var_os(SCENE).is_none() {
            return;
        }
        if crate::nofile::raise().is_none() || crate::nofile::original() != Some(256) {
            println!("scene: nothing to raise");
            return;
        }
        let k = keeper_in("nofile", Ns::NONE);
        let (_, plain, root) = project_served("nofile", Vec::new(), Some(&k), &[], Err("plain".into()));
        let mut cmd = std::process::Command::new("/bin/sh");
        cmd.args(["-c", "ulimit -Sn"]).current_dir(&root);
        let s = k.start_confined(&cmd, &crate::landlock::thread_name(), &plain).unwrap();
        let mut out = String::new();
        io::Read::read_to_string(&mut s.spawned.stdout.unwrap(), &mut out).unwrap();
        crate::spawn::wait(s.spawned.pid).unwrap();
        k.end();
        std::fs::remove_dir_all(&root).ok();
        println!("scene: raised, its run has {}", out.trim());
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
    /// give them (`TARAE_EXPECT_TIER`, as `the_probe_here` checks) fails instead: a birth broken into tier host
    /// isn't a skip there.
    fn namespaced(test: &str) -> bool {
        if ns().user {
            return true;
        }
        let why = super::super::tier::get().short.clone().unwrap_or_default();
        if let Ok(want) = std::env::var("TARAE_EXPECT_TIER").map(|w| w.trim().to_string())
            && want == "own"
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

    /// 9.0: a keeper's namespaces are read from its own links at its birth (`Ns::of_pid`), and a birth not in what it
    /// asked for is refused — so a keeper born is what it asked, and what's checked here is its links read on their
    /// own, after: apart from this process in each of `/proc/<pid>/ns/{user,ipc,mnt,net,pid}` exactly where its `ns`
    /// says, for a keeper of no namespaces of the session's and one of this system's; asked for one its birth doesn't
    /// make (a network namespace without the user one: `clone_flags` makes none), it isn't born at all rather than said
    /// to have it.
    #[test]
    fn a_keeper_is_in_the_namespaces_it_is_said_in() {
        let test = "a_keeper_is_in_the_namespaces_it_is_said_in";
        let link = |pid: u32, name: &str| std::fs::read_link(format!("/proc/{pid}/ns/{name}"));
        let me = std::process::id();
        let mut asked = vec![("links-none", Ns::NONE)];
        if ns().user {
            asked.push(("links-here", ns()));
        }
        for (name, asked) in asked {
            let k = keeper_in(name, asked);
            let said = k.said();
            match link(k.pid, "user") {
                // (a running keeper isn't dumpable: one in tarae's own user namespace has its links closed to a tarae
                // without CAP_SYS_PTRACE there — any host's; one in the session's stays readable, tarae owns it. A part
                // of the test not done, said — not a skip of the test: the rest of it ran)
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied && !k.ns.user => {
                    let line = format!(
                        "sandbox::keeper::tests::{test}: in part — a keeper of no namespaces of the session's has its \
                         links closed to this process without CAP_SYS_PTRACE ({e}): not compared; the rest ran\n"
                    );
                    let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
                }
                _ => {
                    let apart = |n: &str| link(k.pid, n).unwrap() != link(me, n).unwrap();
                    assert_eq!(
                        ["user", "ipc", "mnt", "net", "pid"].map(apart),
                        [k.ns.user, k.ns.user, k.ns.user, k.ns.net, k.ns.pid],
                        "apart where it says: {said}"
                    );
                }
            }
            k.end();
        }
        let root = std::env::temp_dir().join(format!("tarae-keeper-{}-links-net", std::process::id()));
        let asked = Ns { net: true, ..Ns::NONE };
        let Err(e) = Keeper::born(&root, None, asked) else { panic!("no keeper born outside what it asked") };
        assert!(e.to_string().contains("asked to be apart from tarae in net is apart in none"), "{e}");
    }

    /// Whether this system's keepers have a network namespace of the session's own — the session's namespaces, served
    /// or not — else says the test skips
    /// itself, with the probe's reason. A machine said to give tier own (`TARAE_EXPECT_TIER=own`) fails instead.
    fn own_network(test: &str) -> bool {
        if !namespaced(test) {
            return false;
        }
        if ns().netns() {
            return true;
        }
        let why = super::super::tier::get().short.clone().unwrap_or_default();
        if std::env::var("TARAE_EXPECT_TIER").is_ok_and(|t| t.trim() == "own") {
            panic!("{test}: a network namespace expected here (TARAE_EXPECT_TIER=own), none — {why}");
        }
        let line = format!(
            "{}sandbox::keeper::tests::{test} — no network namespace of the session's own here: {why}\n",
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
            "ptys" => ptys_tried(),
            "files" => files_tried(),
            "pty-refusals" => pty_refusals_tried(),
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
            "swap" => helper_swap(&std::env::var("TARAE_TO").unwrap_or_default()),
            "proxied" => helper_proxied(&std::env::var("TARAE_TO").unwrap_or_default()),
            // (step 8: `proxied`, its connection then kept open, idle, until `done` is in its folder — 120 s at most)
            "proxyhold" => {
                let (said, _kept) = proxied(&std::env::var("TARAE_TO").unwrap_or_default());
                println!("helper: {said}");
                let by = Instant::now() + Duration::from_secs(120);
                while !Path::new("done").exists() && Instant::now() < by {
                    std::thread::sleep(Duration::from_millis(50));
                }
                return;
            }
            "bound" => helper_bound(&std::env::var("TARAE_TO").unwrap_or_default()),
            "swaplisten" => helper_swap_listen(&std::env::var("TARAE_TO").unwrap_or_default()),
            "racelo" => helper_race_lo(&std::env::var("TARAE_TO").unwrap_or_default()),
            "foreign" => helper_foreign(),
            "protocols" => helper_protocols(),
            "udp" => helper_udp(&std::env::var("TARAE_TO").unwrap_or_default()),
            "reach" => helper_reach(&std::env::var("TARAE_TO").unwrap_or_default()),
            // (step 7: another session's run holding what it holds until `done` is in its folder — 30 s at most)
            // (step 8: `hold` started as its child, what that holds said, then gone — the child left running below
            // the keeper, as a daemon a build starts is)
            "leaves" => {
                use std::os::unix::process::CommandExt as _;
                // (a process group of its own, as `setsid sleep &` has it: the run's end ends its group; none of
                // the run's output open in it — the run's reader would wait on it)
                let mut left = std::process::Command::new(std::env::current_exe().unwrap())
                    .args(std::env::args().skip(1))
                    .env(HELPER, "hold")
                    .process_group(0)
                    .stdin(std::process::Stdio::null())
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::null())
                    .spawn()
                    .unwrap();
                let out = io::BufReader::new(left.stdout.take().unwrap());
                let said = io::BufRead::lines(out)
                    .map_while(Result::ok)
                    .find_map(|l| l.split_once("helper: ").map(|(_, s)| s.to_string()))
                    .expect("the one left said nothing");
                let said = format!("left={} {said}", left.id());
                // (never waited here: it's left running, and the keeper — its subreaper once this is gone — reaps it)
                std::mem::forget(left);
                said
            }
            // (step 8: a UDP socket made and left unbound until `swept` is in its folder, then bound by its first send
            // — its port said as `held` says one — and answering as `held`'s do, marked, until `done`; 30 s at most)
            "unbound" => {
                use std::os::fd::FromRawFd as _;
                // SAFETY: plain call; the descriptor is this process's own from here.
                let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
                assert!(fd >= 0, "{}", io::Error::last_os_error());
                // SAFETY: just made, owned by nothing else.
                let s = unsafe { std::net::UdpSocket::from_raw_fd(fd) };
                println!("helper: made");
                let by = Instant::now() + Duration::from_secs(30);
                while !Path::new("swept").exists() && Instant::now() < by {
                    std::thread::sleep(Duration::from_millis(10));
                }
                // (to a port nothing listens on: what matters is the port the kernel gives it)
                let sent = s.send_to(b"x", "127.0.0.1:9");
                let port = s.local_addr().map(|a| a.port()).unwrap_or(0);
                println!("helper: tag=theirs udp={port} sent={}", tried(sent));
                let _ = s.set_read_timeout(Some(Duration::from_millis(100)));
                let mut b = [0u8; 64];
                while !Path::new("done").exists() && Instant::now() < by {
                    if let Ok((_, from)) = s.recv_from(&mut b) {
                        let _ = std::fs::write("reached-udp", "");
                        let _ = s.send_to(apart_marker("theirs-sock").as_bytes(), from);
                    }
                }
                return;
            }
            // (step 8: a TCP listener and a UDP socket at 127.0.0.1 with `SO_REUSEADDR` and `SO_REUSEPORT` — a port
            // another socket of the same user may join — and a TCP socket bound so, not listening (`late`), their
            // ports said; `late` listening once `listen` is in its folder, said (`listened=`); held until `done`; 30 s
            // at most)
            "reuses" => {
                use super::super::owners::tests::{bound, bound_only, listening, port_of};
                let (tcp, udp) =
                    (bound(true, "127.0.0.1:0", true, None), bound(false, "127.0.0.1:0", true, None));
                let (tcp, udp) = (tcp.unwrap(), udp.unwrap());
                let late = bound_only(true, "127.0.0.1:0", true, None).unwrap();
                println!("helper: tcp={} udp={} late={}", port_of(&tcp), port_of(&udp), port_of(&late));
                let by = Instant::now() + Duration::from_secs(30);
                let mut listened = false;
                while !Path::new("done").exists() && Instant::now() < by {
                    if !listened && Path::new("listen").exists() {
                        println!("helper: listened={}", tried(listening(&late)));
                        listened = true;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                return;
            }
            // (step 8: binds with `SO_REUSEADDR` and `SO_REUSEPORT` onto `reuses`'s ports — `$TARAE_TO`'s `tcp=` and
            // `udp=` — at its address, every IPv4 address, every address (`[::]`, dual-stack) and `[::1]`, each closed
            // again; and to port 0 with that port the only one the kernel may pick (`-picked`): each `ok` or its errno;
            // `v6`: whether IPv6 is here. With `$TARAE_LATE`, first `late=`'s port bound so, said (`bound-late=`), and
            // made to listen once `go` is in its folder: `late=` among the rest)
            "binds" => {
                use super::super::owners::tests::{bound, bound_only, listening};
                let to = std::env::var("TARAE_TO").unwrap_or_default();
                let late = match std::env::var_os("TARAE_LATE") {
                    Some(_) => {
                        let port = to.split_whitespace().find_map(|w| w.strip_prefix("late=")).unwrap();
                        let s = bound_only(true, &format!("127.0.0.1:{port}"), true, None);
                        println!(
                            "helper: bound-late={}",
                            tried(s.as_ref().map(drop).map_err(|e| io::Error::new(e.kind(), e.to_string())))
                        );
                        let by = Instant::now() + Duration::from_secs(30);
                        while !Path::new("go").exists() && Instant::now() < by {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        format!(" late={}", tried(s.and_then(|s| listening(&s))))
                    }
                    None => String::new(),
                };
                let to: std::collections::HashMap<&str, &str> =
                    to.split_whitespace().filter_map(|w| w.split_once('=')).collect();
                let mut said = vec![format!("v6={}", std::net::UdpSocket::bind("[::1]:0").is_ok())];
                for (proto, tcp) in [("tcp", true), ("udp", false)] {
                    let port: u16 = to[proto].parse().unwrap();
                    for (what, ip) in
                        [("", "127.0.0.1"), ("-any", "0.0.0.0"), ("6-any", "[::]"), ("6", "[::1]")]
                    {
                        let r = bound(tcp, &format!("{ip}:{port}"), true, None);
                        said.push(format!("{proto}{what}={}", tried(r)));
                    }
                    let r = bound(tcp, "127.0.0.1:0", true, Some(port));
                    said.push(format!("{proto}-picked={}", tried(r)));
                }
                format!("{}{late}", said.join(" "))
            }
            // (step 8: a UDP socket connected to 127.0.0.1:$TARAE_PORT, a port nothing is bound to, said (`aimed=`),
            // sending on it with plain `send()` — never served — every 10 ms until `done`; 30 s at most)
            "aims" => {
                let port: u16 = std::env::var("TARAE_PORT").unwrap().parse().unwrap();
                let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                println!("helper: aimed={port} connected={}", tried(s.connect(("127.0.0.1", port))));
                let by = Instant::now() + Duration::from_secs(30);
                while !Path::new("done").exists() && Instant::now() < by {
                    let _ = s.send(apart_marker("theirs-sent").as_bytes());
                    std::thread::sleep(Duration::from_millis(10));
                }
                return;
            }
            // (step 8: UDP binds at `$TARAE_TO`'s `aimed=` port — at 127.0.0.1 (`named`), every address (`any`), port 0
            // with that port the only one the kernel may pick (`picked`), and an unbound socket so narrowed sending
            // (`sent`: its bind the kernel's, on the send) — each `ok` or its errno; `got`: whether any that bound
            // there took in a datagram within 300 ms)
            "bindsat" => {
                use super::super::owners::tests::bound_only;
                use std::os::fd::FromRawFd as _;
                let to = std::env::var("TARAE_TO").unwrap_or_default();
                let port: u16 =
                    to.split_whitespace().find_map(|w| w.strip_prefix("aimed=")).unwrap().parse().unwrap();
                let took = |s: &std::net::UdpSocket| {
                    let _ = s.set_read_timeout(Some(Duration::from_millis(300)));
                    s.recv(&mut [0u8; 64]).is_ok()
                };
                let mut said = Vec::new();
                // (each tried, read from and let go before the next: the port free again)
                for (what, at, range) in [
                    ("named", format!("127.0.0.1:{port}"), None),
                    ("any", format!("0.0.0.0:{port}"), None),
                    ("picked", "127.0.0.1:0".to_string(), Some(port)),
                ] {
                    match bound_only(false, &at, false, range) {
                        Ok(fd) => {
                            let s = std::net::UdpSocket::from(fd);
                            let on = s.local_addr().is_ok_and(|a| a.port() == port);
                            said.push(format!("{what}=ok"));
                            if on && took(&s) {
                                said.push(format!("{what}-got=yes"));
                            }
                        }
                        Err(e) => said.push(format!("{what}={}", tried::<()>(Err(e)))),
                    }
                }
                // SAFETY: plain calls; the descriptor is this process's own from here.
                let s = unsafe {
                    let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
                    let range = u32::from(port) | (u32::from(port) << 16);
                    let size = std::mem::size_of::<u32>() as libc::socklen_t;
                    libc::setsockopt(fd, libc::SOL_IP, 51, (&raw const range).cast(), size);
                    std::net::UdpSocket::from_raw_fd(fd)
                };
                let sent = s.send_to(b"x", "127.0.0.1:9");
                let at = s.local_addr().map(|a| a.port()).unwrap_or(0);
                said.push(format!("sent={}", tried(sent)));
                if at == port && took(&s) {
                    said.push("sent-got=yes".into());
                }
                said.join(" ")
            }
            // (M8.3: 2 000 UDP sends to a socket of its own on 127.0.0.1 — each addressed, served —, as many to
            // 192.0.2.1 (TEST-NET-1: beyond this machine, refused where the network is closed — served all the same),
            // then as many on the first socket connected to its own, never served: each's mean in µs)
            "sendloop" => {
                const N: u32 = 2000;
                let rx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                let to = rx.local_addr().unwrap();
                let tx = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                let timed = |send: &dyn Fn() -> io::Result<usize>, must: bool| {
                    let at = Instant::now();
                    for _ in 0..N {
                        let r = send();
                        assert!(!must || r.is_ok(), "{r:?}");
                    }
                    at.elapsed().as_secs_f64() * 1e6 / f64::from(N)
                };
                let served = timed(&|| tx.send_to(b"x", to), true);
                let away = timed(&|| tx.send_to(b"x", "192.0.2.1:9"), false);
                tx.connect(to).unwrap();
                let plain = timed(&|| tx.send(b"x"), true);
                format!("loopback={served:.1} away={away:.1} plain={plain:.1}")
            }
            "hold" => {
                let h = held("theirs");
                println!("helper: {}", h.said);
                let by = Instant::now() + Duration::from_secs(30);
                while !Path::new("done").exists() && Instant::now() < by {
                    std::thread::sleep(Duration::from_millis(10));
                }
                return (h.undo)();
            }
            "sockets" => {
                let own = held("mine");
                let theirs = helper_try(&std::env::var("TARAE_TO").unwrap_or_default());
                let mine = helper_try(&own.said);
                (own.undo)();
                let mine: String = mine.split_whitespace().map(|w| format!(" own-{w}")).collect();
                format!("{theirs}{mine}")
            }
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
        proxied(to).0
    }

    /// `helper_proxied`'s words, and its connection — still open.
    fn proxied(to: &str) -> (String, std::net::TcpStream) {
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
        (format!("connect={said} {facts} reply={}", reply.trim_end()), s)
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

    /// The `ptys` helper (3b): each way a run reaches a pty, `ok` or its errno — `own` (one made as libc's
    /// `openpty` does: `/dev/ptmx`, unlocked, named, its `/dev/pts/N` opened), `peer` (its other end through
    /// `TIOCGPTPEER`), `reopen` (`own`'s other end again through `/proc/self/fd`), `read`/`write`
    /// (`$TARAE_TO`: a pty of another's), `fresh` (own's closed, a master made and — before any call that tarae
    /// sees — its number read from `fdinfo` and opened: whether its number is `own`'s, `reused` says), `then`
    /// (that master numbered and opened as `own` was).
    fn ptys_tried() -> String {
        use std::os::unix::fs::OpenOptionsExt;
        let open = |path: &str, read: bool, write: bool| {
            let mut o = std::fs::OpenOptions::new();
            o.read(read).write(write).custom_flags(libc::O_NOCTTY).open(path)
        };
        let own = pty_opened();
        let number = |path: &str| path.rsplit('/').next().and_then(|n| n.parse::<u32>().ok());
        let own_n = own.as_ref().ok().and_then(|(_, _, p)| number(p));
        let reopen = match &own {
            Ok((_, s, _)) => tried(open(&format!("/proc/self/fd/{}", s.as_raw_fd()), true, true)),
            Err(_) => "unmade".into(),
        };
        // SAFETY: plain calls on descriptors this makes and owns.
        let peer = unsafe {
            let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
            let m = (m >= 0).then(|| OwnedFd::from_raw_fd(m));
            match m {
                Some(m) if libc::unlockpt(m.as_raw_fd()) == 0 => {
                    let s = libc::ioctl(m.as_raw_fd(), libc::TIOCGPTPEER, libc::O_RDWR | libc::O_NOCTTY);
                    tried((s >= 0).then(|| OwnedFd::from_raw_fd(s)).ok_or_else(io::Error::last_os_error))
                }
                _ => tried::<()>(Err(io::Error::last_os_error())),
            }
        };
        let other = std::env::var("TARAE_TO").unwrap_or_default();
        let (read, write) = (tried(open(&other, true, false)), tried(open(&other, false, true)));
        let own = tried(own.map(drop));
        // SAFETY: as above.
        let (fresh, reused, then) = unsafe {
            let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
            let m = (m >= 0).then(|| OwnedFd::from_raw_fd(m));
            let info =
                m.as_ref().map(|m| std::fs::read_to_string(format!("/proc/self/fdinfo/{}", m.as_raw_fd())));
            let n = info.and_then(|i| super::super::ptys::tty_index(&i.ok()?));
            let fresh = match n {
                Some(n) => tried(open(&format!("/dev/pts/{n}"), true, true)),
                None => "unmade".into(),
            };
            let mut name = [0 as libc::c_char; 64];
            let then = match &m {
                Some(m)
                    if libc::unlockpt(m.as_raw_fd()) == 0
                        && libc::ptsname_r(m.as_raw_fd(), name.as_mut_ptr(), name.len()) == 0 =>
                {
                    tried(open(&std::ffi::CStr::from_ptr(name.as_ptr()).to_string_lossy(), true, true))
                }
                _ => tried::<()>(Err(io::Error::last_os_error())),
            };
            (fresh, n.is_some() && n == own_n, then)
        };
        format!(
            "own={own} peer={peer} reopen={reopen} read={read} write={write} fresh={fresh} reused={reused} then={then}"
        )
    }

    /// The `pty-refusals` helper (3b): the supervisor's own refusals, each `ok` or its errno — `nonmaster` (a pty
    /// call on `/dev/null`), `relock` (a master made, then locked again), `seq` (of 100 ptys made and closed one
    /// after another, how many opened), `held` (ptys then made and kept until one is refused — how many), `past`
    /// (that refusal's errno), `hangup` (a shell reading a pty's other end ends once the run closes its master —
    /// `hung` if not within 3 s).
    fn pty_refusals_tried() -> String {
        let null = std::fs::File::open("/dev/null").expect("/dev/null");
        let mut n: libc::c_uint = 0;
        // SAFETY: plain calls on descriptors this makes and owns; ints for the calls to read and fill.
        let (nonmaster, relock) = unsafe {
            let r = libc::ioctl(null.as_raw_fd(), libc::TIOCGPTN, &mut n);
            let nonmaster = tried((r == 0).then_some(()).ok_or_else(io::Error::last_os_error));
            let m = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
            let m = (m >= 0).then(|| OwnedFd::from_raw_fd(m));
            let lock: libc::c_int = 1;
            let relock = match &m {
                Some(m) => {
                    let r = libc::ioctl(m.as_raw_fd(), libc::TIOCSPTLCK, &lock);
                    tried((r == 0).then_some(()).ok_or_else(io::Error::last_os_error))
                }
                None => "unmade".into(),
            };
            (nonmaster, relock)
        };
        let seq = (0..100).filter(|_| pty_opened().is_ok()).count();
        let mut held = Vec::new();
        let past = loop {
            match pty_opened() {
                Ok(pty) => held.push(pty),
                Err(e) => break tried::<()>(Err(e)),
            }
            if held.len() > 2 * super::super::ptys::POOL {
                break "never".into();
            }
        };
        let kept = held.len();
        drop(held);
        // (a shell reading the other end; its master closed — tarae's too, once it sees the run's go)
        let hangup = match pty_opened() {
            Ok((m, s, _)) => {
                let sh = std::process::Command::new("sh")
                    .args(["-c", "read x"])
                    .stdin(std::process::Stdio::from(s))
                    .stdout(std::process::Stdio::null())
                    .spawn();
                drop(m);
                match sh {
                    Ok(mut sh) => {
                        let until = Instant::now() + Duration::from_secs(3);
                        while sh.try_wait().ok().flatten().is_none() && Instant::now() < until {
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        match sh.try_wait() {
                            Ok(Some(_)) => "ok".to_string(),
                            _ => {
                                let _ = sh.kill();
                                let _ = sh.wait();
                                "hung".into()
                            }
                        }
                    }
                    Err(e) => tried::<()>(Err(e)),
                }
            }
            Err(e) => tried::<()>(Err(e)),
        };
        format!("nonmaster={nonmaster} relock={relock} seq={seq} held={kept} past={past} hangup={hangup}")
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
        // (its network namespace as this system gives it: what's checked is the PID one)
        let without_pid = Ns::session(ns().net, false).said();
        assert!(said.contains(&format!("({without_pid}) nspid=1 — ")), "{said}");
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

    /// F2: no process outside its session gives a run its environment. With the PID namespace the run
    /// can't even name one: nothing it sees carries the stand-in's markers. Without it (Docker's masked
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
        let tier = if pid_ns { "own (its own PID namespace)" } else { "own (no PID namespace)" };
        let line = format!(
            "tier {tier}: the run sees {seen} processes — its own markers read in {own_args} (arguments) and \
             {own_env} (environment), the stand-in's arguments in {args}, its environment in {env}\n"
        );
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
        // The control: the loop finds the run's own markers, so a 0 for the stand-in's is a refusal or a
        // process out of sight, not a loop that reads nothing (with a PID namespace both of the stand-in's are 0).
        assert!(own_args >= 1 && own_env >= 1, "the run doesn't find its own markers: {line}");
        assert_eq!(env, 0, "an environment read outside the session: {line}");
        assert_eq!(args, usize::from(!pid_ns), "{line}");
    }

    /// The other session's stand-in's markers: in its arguments (how a run finds it) and its environment. Made
    /// up where they're used, so no command line or environment here carries them whole.
    fn apart_marker(what: &str) -> String {
        format!("apart-{what}-{}", "marker")
    }

    /// `reach`: another session's run, `$TARAE_TO` (its pid as tarae sees it) — named by that number if it is
    /// the one carrying `apart_marker("arg")` where this run looks, else found by the marker among what it sees,
    /// else out of its sight. Then each attempt on it, `ok` or its errno: its command line and environment read
    /// (`ok` only with the marker in it), `setpriority` to 7, `PTRACE_SEIZE` (ended with this process: no
    /// stop), `SIGCONT`. Out of sight, whether the number names anything here (`kill 0`'s errno, `own` where
    /// it's one of its own session's). `own=N`: the processes the look found carrying this helper's own
    /// argument — its control: a look that reads nothing finds none.
    fn helper_reach(to: &str) -> String {
        let marked = |pid: &str, marker: &str| {
            std::fs::read(format!("/proc/{pid}/cmdline"))
                .is_ok_and(|c| String::from_utf8_lossy(&c).contains(marker))
        };
        let pids: Vec<String> = std::fs::read_dir("/proc")
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
            .collect();
        let own = pids.iter().filter(|p| marked(p, "sandbox::keeper::tests::helper")).count();
        let arg = apart_marker("arg");
        let seen = match marked(to, &arg) {
            true => Some(to.to_string()),
            false => pids.into_iter().find(|p| marked(p, &arg)),
        };
        let Some(t) = seen else {
            let number = match Path::new(&format!("/proc/{to}")).exists() {
                true => "own".to_string(),
                // SAFETY: signal 0 only asks.
                false => tried(io_result(unsafe { libc::kill(to.parse().unwrap_or(0), 0) })),
            };
            return format!("seen=no number={number} own={own}");
        };
        let read = |file: &str, marker: &str| match std::fs::read(format!("/proc/{t}/{file}")) {
            Ok(c) if String::from_utf8_lossy(&c).contains(marker) => "ok".to_string(),
            Ok(_) => "unmarked".to_string(),
            Err(e) => tried::<()>(Err(e)),
        };
        let pid: libc::pid_t = t.parse().unwrap();
        // SAFETY: plain calls on a pid; a seized tracee is let go when this process ends, never stopped.
        let (prio, ptrace, kill) = unsafe {
            (
                libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, 7),
                libc::ptrace(libc::PTRACE_SEIZE, pid, 0, 0) as libc::c_int,
                libc::kill(pid, libc::SIGCONT),
            )
        };
        format!(
            "seen=yes cmdline={} environ={} setpriority={} ptrace={} kill={} own={own}",
            read("cmdline", &arg),
            read("environ", &apart_marker("env")),
            tried(io_result(prio)),
            tried(io_result(ptrace)),
            tried(io_result(kill)),
        )
    }

    /// A libc call's `-1` as its errno.
    fn io_result(r: libc::c_int) -> io::Result<()> {
        if r == -1 { Err(io::Error::last_os_error()) } else { Ok(()) }
    }

    /// What one session's run got trying another session's.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Reach {
        /// Out of its sight (its session's own PID namespace): no number of its names it — nothing to try.
        Unnamed,
        /// Named, and the kernel refused it.
        Refused,
        /// Named, and it went through: shared between sessions here — known, and the report must say it.
        Shared,
        /// Never there: the other session's run can't make one here, so there's nothing of a session's to reach.
        Unmade,
    }

    /// Step 7: two sessions' keepers side by side, a run of each — from one, everything a process can do to
    /// another of the same user: kill/signal, ptrace attach, its `/proc/<pid>/{cmdline,environ}` read,
    /// `setpriority`. Each tier's expectation is the table: with its own PID namespace a run names nothing of the
    /// other session; without one (no fresh `/proc`, or no namespaces) the other run is in sight and Landlock keeps ptrace and the environment
    /// (a ptrace-mode read) in the run's own domain, signals from ABI 6 — what goes through is shared, pinned as
    /// shared and said (the user's decision 2026-10-10). The stand-in is the test's own; what a `setpriority` it
    /// was let do is checked on it from here, and it's alive after.
    #[test]
    fn two_sessions_cant_reach_each_others_processes() {
        use Reach::*;
        let test = "two_sessions_cant_reach_each_others_processes";
        let Some(rules) = open_rules(test) else { return };
        let abi = crate::landlock::abi().unwrap();
        let (mine, theirs) = (keeper("apart-mine"), keeper("apart-theirs"));
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "sleep 60; :", &apart_marker("arg")]).env("TARAE_APART", apart_marker("env"));
        let target = theirs.start(&cmd, &crate::landlock::thread_name(), rules.fd(), RunNet::Plain).unwrap();
        let pid = target.spawned.pid;
        let stat = || std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
        let nice =
            || stat().rsplit_once(')').and_then(|(_, s)| s.split_whitespace().nth(16).map(String::from));
        let by = Instant::now() + Duration::from_secs(5);
        let cmdline = || std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        while !String::from_utf8_lossy(&cmdline()).contains(&apart_marker("arg")) && Instant::now() < by {
            std::thread::sleep(Duration::from_millis(10));
        }
        let before = nice();
        let said = helper_through(
            &mine,
            rules.fd(),
            RunNet::Plain,
            "reach",
            &[("TARAE_TO", &pid.to_string())],
            None,
        );
        let after = nice();
        // SAFETY: signal 0 only asks; then the stand-in's group (`setsid`: it and its `sleep`) is ended.
        let alive = unsafe { libc::kill(pid as libc::pid_t, 0) } == 0;
        unsafe { libc::kill(-(pid as libc::pid_t), libc::SIGKILL) };
        crate::spawn::wait(pid).ok();
        let pid_ns = mine.ns.pid;
        let probed = super::super::tier::get();
        let (user_ns, tier) = (mine.ns.user, probed.tier().said());
        let why = probed.short.as_deref().map(|w| format!(" — {w}")).unwrap_or_default();
        // What the report says a run of this session's can't be kept from (its guarantees' notes, said with the
        // conversation's first run), its rules made with its keeper as a project's run's are
        let plain = Err("not served".into());
        let (mut p, c, _root) = project_served("apart", Vec::new(), Some(&mine), &[], plain);
        let g = c.guarantees.clone().expect("a keeper's run has its guarantees");
        p.confine = Some(Ok(c));
        // (judged on the notes alone — the sentence names every part, kept or shared)
        let notes = g.shared_said();
        let report = crate::sandbox::Guard::In(Box::new(p)).report(&[]);
        assert!(report.contains(&notes), "the conversation's first report carries the notes: {report}");
        mine.end();
        theirs.end();
        let got: std::collections::HashMap<&str, &str> =
            said.split_whitespace().filter_map(|w| w.split_once('=')).collect();
        let line = format!("tier {tier}{why} (Landlock ABI {abi}): {said}\n");
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
        assert!(before.is_some() && alive, "the stand-in ran and outlived the attempts: {before:?} {line}");
        assert!(got["own"] != "0", "the look finds the run's own helper — else it reads nothing: {line}");
        let verdict = |what: &str| match (got["seen"], got.get(what).copied()) {
            ("no", _) => Unnamed,
            (_, Some("ok")) => Shared,
            (_, Some(n)) if n.bytes().all(|b| b.is_ascii_digit()) => Refused,
            (_, other) => panic!("{what}: {other:?} — {line}"),
        };
        // From ABI 6 Landlock keeps a run's signals to its own processes (its scope); below, they're shared
        let signal = if abi >= 6 { Refused } else { Shared };
        // (attempt, with its own PID namespace, with the session's namespaces but that one, with none)
        let table = [
            ("cmdline", Unnamed, Shared, Shared),
            ("environ", Unnamed, Refused, Refused),
            ("ptrace", Unnamed, Refused, Refused),
            ("kill", Unnamed, signal, signal),
            ("setpriority", Unnamed, Shared, Shared),
        ];
        // Where the report says each that's shared
        let said_of = |what: &str| match what {
            "kill" => notes.contains("can't keep a command's signals to its own processes"),
            "cmdline" => {
                notes.contains("another session's commands") && notes.contains("their command lines")
            }
            "setpriority" => notes.contains("another session's commands") && notes.contains("their priority"),
            _ => false,
        };
        // …and nothing says it where it isn't so
        assert_eq!(notes.contains("another session's commands"), !pid_ns, "{notes}");
        for (what, a_plus, a, c_d) in table {
            let want = match (pid_ns, user_ns) {
                (true, _) => a_plus,
                (false, true) => a,
                (false, false) => c_d,
            };
            assert_eq!(verdict(what), want, "{what}: {line}");
            if want == Shared {
                assert!(
                    said_of(what),
                    "{what} is shared between sessions here, and the report doesn't say so: {line}"
                );
            }
        }
        // 9a: what its guarantees say shared is what was found shared — the processes in sight (their command lines,
        // their priority) and signals
        use super::super::guarantees::{Part, reached_by_any, said_as_found};
        let reached = |what: &str| (verdict(what) != Unnamed).then(|| verdict(what) == Shared);
        let processes = reached_by_any(&[reached("cmdline"), reached("setpriority")]);
        let found = [(Part::Processes, processes), (Part::Signals, reached("kill"))];
        let wrong = said_as_found(&g, &found);
        assert!(wrong.is_empty(), "{wrong:#?}\n{}\n{line}", g.line());
        // What the kernel let it do is what happened to the stand-in
        let moved = Some("7".to_string());
        assert_eq!(
            after == moved && before != moved,
            verdict("setpriority") == Shared,
            "{before:?} → {after:?}: {line}"
        );
        if !pid_ns {
            assert_eq!(got["seen"], "yes", "in sight without a PID namespace of its own: {line}");
        }
    }

    /// What a session's run holds for another session's to try (`held`): `said` names each as `helper_try`
    /// reads it — `what=<where>`, or `what=no<errno>` where it couldn't be made here; `undo` takes away what
    /// would outlive the run (the segment, the files).
    struct Held {
        said: String,
        undo: Box<dyn FnOnce()>,
        _pty: Option<(OwnedFd, OwnedFd, String)>,
    }

    /// `tag`'s things, made by this process: TCP listeners on `127.0.0.1` and `::1`, UDP sockets there answering
    /// every datagram, an abstract unix listener, one at `apart-<tag>.sock` in its folder — each answering with
    /// `apart_marker("<tag>-sock")` (IPv6's `unmade` where this system has none) —,
    /// a SysV shared memory segment at a key of its own and a file in `/dev/shm`, both holding
    /// `apart_marker("<tag>-mem")`, and a pty (its path, its `devpts` and its device number).
    fn held(tag: &str) -> Held {
        use std::os::linux::net::SocketAddrExt as _;
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::net::{SocketAddr, UnixListener};
        // (each connection accepted, each datagram received, marked in its folder: `reached-<what>` — what came
        // in, whether or not its answer got back)
        fn answering<L: Send + 'static, S: io::Write + 'static>(
            l: L,
            accept: fn(&L) -> io::Result<S>,
            answer: String,
            mark: PathBuf,
        ) {
            std::thread::spawn(move || {
                while let Ok(mut s) = accept(&l) {
                    let _ = std::fs::write(&mark, "");
                    let _ = io::Write::write_all(&mut s, answer.as_bytes());
                }
            });
        }
        let here = std::env::current_dir().unwrap();
        let mark = |what: &str| here.join(format!("reached-{what}"));
        let no = |e: io::Error| format!("no{}", tried::<()>(Err(e)));
        let answer = apart_marker(&format!("{tag}-sock"));
        let pid = std::process::id();
        let tcp = |at: &str, what: &str| match std::net::TcpListener::bind(at) {
            Ok(l) => {
                let port = l.local_addr().unwrap().port();
                answering(l, |l| l.accept().map(|(s, _)| s), answer.clone(), mark(what));
                port.to_string()
            }
            Err(e) => no(e),
        };
        let (tcp, tcp6) = (tcp("127.0.0.1:0", "tcp"), tcp("[::1]:0", "tcp6"));
        let udp = |at: &str, what: &str| match std::net::UdpSocket::bind(at) {
            Ok(s) => {
                let port = s.local_addr().unwrap().port();
                let (answer, mark) = (answer.clone(), mark(what));
                std::thread::spawn(move || {
                    let mut b = [0u8; 64];
                    while let Ok((_, from)) = s.recv_from(&mut b) {
                        let _ = std::fs::write(&mark, "");
                        let _ = s.send_to(answer.as_bytes(), from);
                    }
                });
                port.to_string()
            }
            Err(e) => no(e),
        };
        let (udp, udp6) = (udp("127.0.0.1:0", "udp"), udp("[::1]:0", "udp6"));
        let name = format!("tarae-apart-{tag}-{pid}");
        let abs =
            match SocketAddr::from_abstract_name(name.as_bytes()).and_then(|a| UnixListener::bind_addr(&a)) {
                Ok(l) => {
                    answering(l, |l| l.accept().map(|(s, _)| s), answer.clone(), mark("abstract"));
                    name
                }
                Err(e) => no(e),
            };
        let sock = here.join(format!("apart-{tag}.sock"));
        let path = match UnixListener::bind(&sock) {
            Ok(l) => {
                answering(l, |l| l.accept().map(|(s, _)| s), answer, mark("path"));
                sock.display().to_string()
            }
            Err(e) => no(e),
        };
        let mem = apart_marker(&format!("{tag}-mem"));
        let key = 0x7a40_0000 | ((tag.len() as i32) << 16) | (pid as i32 & 0xffff);
        // SAFETY: a segment of its own (4096 bytes, the marker far less), written while attached here; `undo`
        // removes it.
        let id = unsafe { libc::shmget(key, 4096, libc::IPC_CREAT | libc::IPC_EXCL | 0o600) };
        let ipc = match id {
            -1 => no(io::Error::last_os_error()),
            id => {
                // SAFETY: as above.
                unsafe {
                    let at = libc::shmat(id, std::ptr::null(), 0);
                    assert!(at as isize != -1, "{}", io::Error::last_os_error());
                    std::ptr::copy_nonoverlapping(mem.as_ptr(), at.cast::<u8>(), mem.len());
                    libc::shmdt(at);
                }
                format!("{key}:{id}")
            }
        };
        let file = PathBuf::from(format!("/dev/shm/tarae-apart-{tag}-{pid}"));
        let shm = match std::fs::write(&file, &mem).and_then(|()| std::fs::metadata(&file)) {
            Ok(m) => format!("{}:{}:{}", file.display(), m.dev(), m.ino()),
            Err(e) => no(e),
        };
        let (pty, opened) = match pty_opened() {
            Ok(p) => {
                let m = std::fs::metadata(&p.2).unwrap();
                (format!("{}:{}:{}", p.2, m.dev(), m.rdev()), Some(p))
            }
            Err(e) => (no(e), None),
        };
        let undo = move || {
            if id != -1 {
                // SAFETY: removes the segment made above.
                unsafe { libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut()) };
            }
            std::fs::remove_file(&file).ok();
            std::fs::remove_file(&sock).ok();
        };
        Held {
            said: format!(
                "tag={tag} tcp={tcp} tcp6={tcp6} udp={udp} udp6={udp6} abstract={abs} path={path} ipc={ipc} \
                 shm={shm} pty={pty}"
            ),
            undo: Box::new(undo),
            _pty: opened,
        }
    }

    /// Each of what `said` names (`held`'s) tried from here: `ok` when it's reached — the holder's marker read
    /// back (a socket's answer, the segment's or the file's text), or a pty opened —, `unseen` where nothing of
    /// the holder's is at that name here (nothing there, or something else: another namespace's own), `unmade`
    /// where the holder couldn't make it, else the errno it was refused with.
    fn helper_try(said: &str) -> String {
        use std::os::linux::net::SocketAddrExt as _;
        use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
        use std::os::unix::net::{SocketAddr, UnixStream};
        /// The path in `v` (`<path>:<a>:<b>`) when what's there is what the holder made (`id` of it is
        /// `(a, b)`), else what was found instead.
        fn placed(v: &str, id: fn(&std::fs::Metadata) -> (u64, u64)) -> Result<&str, String> {
            let mut it = v.rsplitn(3, ':');
            let (b, a, path) = (it.next(), it.next(), it.next().unwrap_or_default());
            let want = (a.and_then(|a| a.parse().ok()), b.and_then(|b| b.parse().ok()));
            match std::fs::metadata(path) {
                Ok(m) if (Some(id(&m).0), Some(id(&m).1)) == want => Ok(path),
                Ok(_) => Err("unseen".into()),
                Err(e) if e.raw_os_error() == Some(libc::ENOENT) => Err("unseen".into()),
                Err(e) => Err(tried::<()>(Err(e))),
            }
        }
        let got: std::collections::HashMap<&str, &str> =
            said.split_whitespace().filter_map(|w| w.split_once('=')).collect();
        let tag = got.get("tag").copied().unwrap_or_default();
        let judged = |text: &[u8], what: &str| {
            let text = String::from_utf8_lossy(text);
            match text.contains(&apart_marker(&format!("{tag}-{what}"))) {
                true => "ok",
                false if text.contains("apart-") => "unseen",
                false => "unmarked",
            }
            .to_string()
        };
        let wait = Duration::from_secs(3);
        let read = |s: io::Result<Box<dyn io::Read>>| match s {
            Ok(mut s) => {
                let mut b = Vec::new();
                let _ = io::Read::read_to_end(&mut s, &mut b);
                judged(&b, "sock")
            }
            // (nothing listening at that name here)
            Err(e) if matches!(e.raw_os_error(), Some(libc::ECONNREFUSED | libc::ENOENT)) => "unseen".into(),
            Err(e) => tried::<()>(Err(e)),
        };
        let unix = |s: io::Result<UnixStream>| {
            s.and_then(|s| s.set_read_timeout(Some(wait)).map(|()| Box::new(s) as Box<dyn io::Read>))
        };
        let each = |what: &str, f: &dyn Fn(&str) -> String| match got.get(what) {
            Some(v) if !v.starts_with("no") => format!("{what}={}", f(v)),
            _ => format!("{what}=unmade"),
        };
        let stream = |ip: std::net::IpAddr, port: &str| {
            let to = std::net::SocketAddr::new(ip, port.parse().unwrap_or(9));
            read(
                std::net::TcpStream::connect_timeout(&to, wait)
                    .and_then(|s| s.set_read_timeout(Some(wait)).map(|()| Box::new(s) as Box<dyn io::Read>)),
            )
        };
        // (from a socket bound to every address and a port the kernel picks: one bound to an address stays in its
        // session — `supervisor::swapped_for`; nothing back within a second, or refused: nothing of the holder's)
        let datagram = |ip: std::net::IpAddr, port: &str| {
            let from = if ip.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
            let s = match std::net::UdpSocket::bind(from) {
                Ok(s) => s,
                Err(e) => return tried::<()>(Err(e)),
            };
            let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
            let gone = |e: &io::Error| {
                e.raw_os_error() == Some(libc::ECONNREFUSED)
                    || matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
            };
            let to = std::net::SocketAddr::new(ip, port.parse().unwrap_or(9));
            let mut b = [0u8; 128];
            match s.send_to(b"x", to).and_then(|_| s.recv_from(&mut b)) {
                Ok((n, _)) => judged(&b[..n], "sock"),
                Err(e) if gone(&e) => "unseen".into(),
                Err(e) => tried::<()>(Err(e)),
            }
        };
        let (v4, v6) = (std::net::Ipv4Addr::LOCALHOST.into(), std::net::Ipv6Addr::LOCALHOST.into());
        // (the same ports at every address, `0.0.0.0`: the kernel sends it to loopback — `owners::listener_at`)
        let any = std::net::Ipv4Addr::UNSPECIFIED.into();
        let at_any = |what: &str, f: &dyn Fn(&str) -> String| match got.get(what) {
            Some(v) if !v.starts_with("no") => format!("{what}-any={}", f(v)),
            _ => format!("{what}-any=unmade"),
        };
        [
            each("tcp", &|port| stream(v4, port)),
            each("tcp6", &|port| stream(v6, port)),
            each("udp", &|port| datagram(v4, port)),
            each("udp6", &|port| datagram(v6, port)),
            at_any("tcp", &|port| stream(any, port)),
            at_any("udp", &|port| datagram(any, port)),
            each("abstract", &|name| {
                read(unix(
                    SocketAddr::from_abstract_name(name.as_bytes())
                        .and_then(|a| UnixStream::connect_addr(&a)),
                ))
            }),
            each("path", &|path| read(unix(UnixStream::connect(path)))),
            each("ipc", &|v| {
                let (key, id) = v.split_once(':').unwrap();
                let (key, id): (i32, i32) = (key.parse().unwrap(), id.parse().unwrap());
                // SAFETY: plain calls; a segment attached read-only, 64 bytes of it read (a segment is a page at
                // least), detached.
                unsafe {
                    let by_key = libc::shmget(key, 0, 0);
                    let at = libc::shmat(
                        if by_key >= 0 { by_key } else { id },
                        std::ptr::null(),
                        libc::SHM_RDONLY,
                    );
                    if at as isize == -1 {
                        let e = io::Error::last_os_error();
                        return match e.raw_os_error() {
                            Some(libc::EINVAL | libc::EIDRM) => "unseen".into(),
                            _ => tried::<()>(Err(e)),
                        };
                    }
                    let r = judged(std::slice::from_raw_parts(at.cast::<u8>(), 64), "mem");
                    libc::shmdt(at);
                    r
                }
            }),
            each("shm", &|v| match placed(v, |m| (m.dev(), m.ino())) {
                Ok(p) => std::fs::read(p).map_or_else(|e| tried::<()>(Err(e)), |b| judged(&b, "mem")),
                Err(found) => found,
            }),
            each("pty", &|v| match placed(v, |m| (m.dev(), m.rdev())) {
                Ok(p) => tried(
                    std::fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOCTTY).open(p),
                ),
                Err(found) => found,
            }),
        ]
        .join(" ")
    }

    /// A session for step 7's tests: its project, its keeper — born in the namespaces this system gives the
    /// project's policy — and its runs' rules as `linux::confine` makes them, served as this system serves.
    struct Apart {
        policy: super::super::Policy,
        keeper: Arc<Keeper>,
        confine: super::super::linux::Confine,
        root: PathBuf,
    }

    impl Apart {
        fn new(name: &str) -> Apart {
            let (policy, root) = project_policy(name, Vec::new(), &[]);
            Apart::of(name, policy, root)
        }

        /// A session for the checkout at `root` (real) with what a run of its gets to write but its project, as
        /// `policy_from` gives it, in `home` (real, standing for one): its own folder (`RunDir`) in tarae's
        /// cache's `runs`, where every session's are, a kept run home of its project's and the home a daemon of
        /// its policy lives from (`with_kept`) — each made, as a run that needs it would. Reads confined to
        /// `home` but for the repository's other checkouts (`allow::checkouts`), as a run's are.
        fn writing(name: &str, root: &Path, home: &Path) -> Apart {
            let cache = home.join(".cache/tarae");
            let (runs, homes) = (cache.join("runs"), cache.join("run-homes"));
            let key = super::super::project_key(root);
            let env = super::super::Env {
                root: root.to_path_buf(),
                tmp: Some(super::super::profile_path(&std::env::temp_dir())),
                runs: Some(runs.clone()),
                kept_homes: vec![("kept".into(), homes.join("kept").join(&key))],
                daemon_homes: vec![("daemon".into(), homes.join("daemon").join(&key))],
                reads: super::super::Reads {
                    home: Some(home.to_path_buf()),
                    allow: crate::allow::checkouts(root),
                    ..Default::default()
                },
                run_env: super::super::run_env_of(
                    &super::super::Here::now(),
                    &crate::llm::LlmConfig::default(),
                ),
                ..Default::default()
            };
            // (a folder of its own: the checkout's top holds `.git`, granted entry by entry — `linux::confine`)
            std::fs::create_dir_all(root.join("src")).unwrap();
            let mut policy = super::super::Policy::for_project(&env);
            super::super::with_kept(&mut policy, &env);
            policy.run = Some(super::super::RunDir::new_in(&runs).unwrap());
            for k in &policy.kept {
                std::fs::create_dir_all(&k.home).unwrap();
            }
            Apart::of(name, policy, root.to_path_buf())
        }

        fn of(name: &str, policy: super::super::Policy, root: PathBuf) -> Apart {
            let keeper = keeper_in(name, super::super::tier::for_policy(&policy));
            let (abi, served) = (crate::landlock::abi().unwrap(), super::super::linux::served_here());
            let confine = super::super::linux::confine(
                &policy,
                abi,
                Err("not watched".into()),
                false,
                Some(&keeper),
                served,
            )
            .unwrap();
            Apart { policy, keeper, confine, root }
        }

        /// The test binary on `helper` doing `what` (with `env` set) as a run of this session's, in its project.
        fn helper(&self, what: &str, env: &[(&str, &str)]) -> Started {
            // (what the supervisor refuses is said on a channel no one reads here)
            self.helper_told(what, env).0
        }

        /// `helper`, and what its supervisor refused it (served; else nothing ever comes).
        fn helper_told(
            &self,
            what: &str,
            env: &[(&str, &str)],
        ) -> (Started, std::sync::mpsc::Receiver<super::super::Denial>) {
            let cmd = helper_cmd(what, env, Some(&self.root));
            let (c, name) = (&self.confine, crate::landlock::thread_name());
            let (told, refused) = std::sync::mpsc::channel();
            let started = match c.net {
                RunNet::Served | RunNet::Routed => {
                    let p = Arc::new(self.policy.clone());
                    self.keeper.start_served(&cmd, &name, c, p, told)
                }
                RunNet::Own | RunNet::Plain => self.keeper.start_confined(&cmd, &name, c),
            };
            (started.unwrap(), refused)
        }

        /// Where a run of this session's writes, but the temp folders (`writing`'s): what, the folder.
        fn writes(&self) -> Vec<(&'static str, PathBuf)> {
            let kept = |daemon| self.policy.kept.iter().find(|k| k.daemon == daemon).unwrap().home.clone();
            let run = self.policy.run.as_ref().unwrap().path().to_path_buf();
            let checkout = self.root.join("src");
            vec![("checkout", checkout), ("run", run), ("kept", kept(false)), ("daemon", kept(true))]
        }

        fn end(self) {
            self.keeper.end();
            std::fs::remove_dir_all(&self.root).ok();
        }
    }

    /// Step 7: two sessions side by side — from a run of one at what a run of the other holds (`held`):
    /// loopback TCP listeners and UDP sockets, IPv4 and IPv6, an abstract unix listener, a unix socket in its
    /// project, a SysV shared memory segment, a file in `/dev/shm`, a pty. Each is the session's own where its
    /// keeper has the namespace — the network (its `lo`, its abstract names), IPC and its own `/dev/shm` and
    /// `devpts` (tier own) —; a unix socket by path is the supervisor's to refuse outside the project, in every tier
    /// served. Without them (no namespaces) what goes through is shared, pinned as shared and said (the user's decision
    /// 2026-10-10) — but the loopback: the trying session opens every port of this machine's
    /// (`llm.sandbox-localhost`), and where sessions share that loopback the other session's sockets on it are
    /// refused (step 8, D18) — `ECONNREFUSED`, what a loopback of the session's own with nothing there says, and
    /// the report says whose port it was. The trying run reaches its own of each first: a try that reaches
    /// nothing would pass.
    #[test]
    fn two_sessions_cant_reach_each_others_sockets() {
        use Reach::*;
        let test = "two_sessions_cant_reach_each_others_sockets";
        if open_rules(test).is_none() {
            return;
        }
        let (mut open, root) = project_policy("socks-mine", Vec::new(), &[]);
        open.localhost = true;
        let (mine, theirs) = (Apart::of("socks-mine", open, root), Apart::new("socks-theirs"));
        let mut hold = theirs.helper("hold", &[]);
        let mut out = io::BufReader::new(hold.spawned.stdout.take().unwrap());
        let held = loop {
            let mut line = String::new();
            assert!(io::BufRead::read_line(&mut out, &mut line).unwrap() > 0, "the holder said nothing");
            if let Some((_, s)) = line.split_once("helper: ") {
                break s.trim().to_string();
            }
        };
        let (tried, refused) = mine.helper_told("sockets", &[("TARAE_TO", &held)]);
        let said = helper_said(tried);
        let refused: Vec<super::super::Denial> = refused.try_iter().collect();
        std::fs::write(theirs.root.join("done"), "").unwrap();
        let mut rest = String::new();
        io::Read::read_to_string(&mut out, &mut rest).unwrap();
        io::Read::read_to_string(&mut hold.spawned.stderr.take().unwrap(), &mut rest).unwrap();
        crate::spawn::wait(hold.spawned.pid).unwrap();
        // (what came in to the holder's sockets: its marks)
        let reached: Vec<String> = std::fs::read_dir(&theirs.root)
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.strip_prefix("reached-").map(String::from))
            .collect();
        let (ns, net, tier) = (mine.keeper.ns, mine.confine.net, mine.confine.tier());
        let ptys = mine.confine.ptys.is_some();
        let g = mine.confine.guarantees.clone().expect("a keeper's run has its guarantees");
        // What the report says a run of this session's can't be kept from (its guarantees' notes, said with the
        // conversation's first run)
        let mut p = mine.policy.clone();
        p.confine = Some(Ok(mine.confine.clone()));
        // (judged on the notes alone — the sentence names every part, kept or shared)
        let notes = g.shared_said();
        let report = crate::sandbox::Guard::In(Box::new(p)).report(&[]);
        assert!(report.contains(&notes), "the conversation's first report carries the notes: {report}");
        mine.end();
        theirs.end();
        let got: std::collections::HashMap<&str, &str> =
            said.split_whitespace().filter_map(|w| w.split_once('=')).collect();
        let refusals: Vec<String> = refused.iter().map(|d| d.reported()).collect();
        let line = format!(
            "tier {tier} ({ns:?}, {net:?}): held {held}\n  tried: {said}\n  came in: {reached:?}\n  refused: \
             {refusals:#?}\n  notes:{notes}\n"
        );
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
        let verdict = |what: &str| match got[what] {
            "ok" => Shared,
            "unseen" => Unnamed,
            "unmade" => Unmade,
            n if n.bytes().all(|b| b.is_ascii_digit()) => Refused,
            other => panic!("{what}: {other} — {line}"),
        };
        // Without a supervisor a run has no socket but netlink
        let sockets = net != RunNet::Plain;
        // (IPv6 judged only where `::1` is there: this system's loopback — a container's often has none, so neither
        // session makes one — or a session's own netns, whose `lo` has its own whatever tarae's network carries)
        let v6 = got["own-tcp6"] != "unmade";
        if !v6 {
            eprintln!("{test}: IPv6 not judged — not carried here");
        }
        let loopback =
            [("tcp", true), ("tcp6", v6), ("udp", true), ("udp6", v6), ("tcp-any", true), ("udp-any", true)];
        // The control: the trying run reaches its own — its loopback sockets (its ports all opened) and an
        // abstract name only in its session's network
        for (what, want) in [
            ("tcp", sockets),
            ("tcp6", sockets && v6),
            ("udp", sockets),
            ("udp6", sockets && v6),
            ("tcp-any", sockets),
            ("udp-any", sockets),
            ("abstract", sockets && ns.net),
            ("path", sockets),
            ("ipc", true),
            ("shm", true),
            ("pty", true),
        ] {
            let own = got[&*format!("own-{what}")];
            assert_eq!(own == "ok", want, "its own {what}: {line}");
        }
        // 3b: without a `devpts` of the session's own a served run opens only the ptys it makes (`ptys.rs`)
        assert_eq!(ptys, ns.serves_ptys(net), "{line}");
        let pty = if ptys { Refused } else { Shared };
        // (attempt, in the session's own namespace for it, without one — the loopback refused as if nothing
        // were there, step 8: below, said)
        let table = [
            ("tcp", ns.net, Unnamed, Unnamed),
            ("tcp6", ns.net, Unnamed, Unnamed),
            ("udp", ns.net, Unnamed, Unnamed),
            ("udp6", ns.net, Unnamed, Unnamed),
            ("tcp-any", ns.net, Unnamed, Unnamed),
            ("udp-any", ns.net, Unnamed, Unnamed),
            ("abstract", ns.net, Unnamed, Unmade),
            ("path", false, Refused, Refused),
            ("ipc", ns.user, Unnamed, Shared),
            ("shm", ns.shm, Unnamed, Shared),
            ("pty", ns.user, Unnamed, pty),
        ];
        // Where the report says each that's shared
        let said_of = |what: &str| {
            notes.contains("other sessions' commands")
                && match what {
                    "ipc" => notes.contains("System V IPC"),
                    "shm" => notes.contains("/dev/shm"),
                    // (a run reaches every pty here, not only the other session's: the report says so)
                    "pty" => notes.contains("terminals (/dev/pts)") && notes.contains("the user's own too"),
                    _ => false,
                }
        };
        let mut wrong = Vec::new();
        for (what, own, in_own, without) in table {
            if !loopback.iter().all(|(w, judged)| w != &what || *judged) {
                continue;
            }
            let want = match (sockets, what) {
                (false, "tcp" | "tcp6" | "udp" | "udp6" | "tcp-any" | "udp-any" | "abstract" | "path") => {
                    Unmade
                }
                _ if own => in_own,
                _ => without,
            };
            if verdict(what) != want {
                wrong.push(format!("{what}: {:?}, not {want:?}", verdict(what)));
            }
            if want == Shared && !said_of(what) {
                wrong.push(format!("{what} is shared between sessions here, and the report doesn't say so"));
            }
        }
        // Step 8: nothing of the trying run's came in to the other session's loopback sockets (`held`'s marks:
        // a datagram arrives whether or not its answer gets back); where sessions share this machine's loopback
        // each was refused by tarae, said by its port — and only there (a loopback of its own has nothing of theirs)
        for (what, _) in loopback.iter().filter(|(_, made)| *made) {
            if reached.iter().any(|r| r == what) {
                wrong.push(format!("{what}: the other session's socket took it in"));
            }
        }
        let theirs_at: std::collections::HashMap<&str, &str> =
            held.split_whitespace().filter_map(|w| w.split_once('=')).collect();
        let shared = sockets && !ns.net;
        for (what, _) in loopback.iter().filter(|(_, made)| *made) {
            // (to every address: said by the same port, the socket's)
            let port = format!(":{}", theirs_at[what.trim_end_matches("-any")]);
            let said = refused.iter().any(|d| {
                d.op == super::super::SESSION_OP
                    && d.target.ends_with(&port)
                    && d.plain().contains("is another tarae session's")
            });
            if said != shared {
                wrong.push(format!("{what}: refused as another session's {said}, where shared {shared}"));
            }
        }
        // 9a: what its guarantees say shared is what was found shared — System V IPC, /dev/shm, the terminals, the
        // loopback (any of its sockets reached)
        use super::super::guarantees::{Part, said_as_found};
        let reached = |what: &str| match verdict(what) {
            Unnamed | Unmade => None,
            v => Some(v == Shared),
        };
        let judged = loopback.iter().filter(|(_, j)| *j);
        let lo = sockets.then(|| judged.clone().any(|(what, _)| verdict(what) == Shared));
        let found = [
            (Part::Ipc, reached("ipc")),
            (Part::Shm, reached("shm")),
            (Part::Ptys, reached("pty")),
            (Part::Loopback, lo),
        ];
        wrong.extend(said_as_found(&g, &found));
        assert!(wrong.is_empty(), "{wrong:#?}\n{}\n{line}", g.line());
        // …and nothing says it where it isn't so — the ptys kept apart said as such
        assert_eq!(notes.contains("other sessions' commands"), !(ns.user && ns.shm), "{line}");
        assert_eq!(notes.contains("the user's own too"), !ns.user && !ptys, "{line}");
        assert_eq!(notes.contains("it opens only those it makes"), ptys, "{line}");
    }

    /// Step 8: a session's runs reach its own loopback sockets, where sessions share this machine's loopback as
    /// where they don't — one that a run left running (a daemon: Gradle's) after that run ended, from a later
    /// run: the rule is by session, never by run. Its ports all opened (`llm.sandbox-localhost`).
    #[test]
    fn a_session_reaches_its_own_listener() {
        let test = "a_session_reaches_its_own_listener";
        if open_rules(test).is_none() {
            return;
        }
        let (mut open, root) = project_policy("own-listener", Vec::new(), &[]);
        open.localhost = true;
        let s = Apart::of("own-listener", open, root);
        if s.confine.net == RunNet::Plain {
            s.end();
            eprintln!("{test}: skipped — no sockets without a supervisor");
            return;
        }
        // (the run that left it is over: its supervisor serves what it left)
        let left = helper_said(s.helper("leaves", &[]));
        let (tried, refused) = s.helper_told("sockets", &[("TARAE_TO", &left)]);
        let said = helper_said(tried);
        let refused: Vec<String> = refused.try_iter().map(|d| d.reported()).collect();
        std::fs::write(s.root.join("done"), "").unwrap();
        let tier = s.confine.tier();
        s.end();
        let got: std::collections::HashMap<&str, &str> =
            said.split_whitespace().filter_map(|w| w.split_once('=')).collect();
        let line = format!("tier {tier}: left {left}\n  tried: {said}\n  refused: {refused:#?}\n");
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
        // (IPv6 only where `::1` is there, as in `two_sessions_cant_reach_each_others_sockets`)
        let v6 = got["own-tcp6"] != "unmade";
        let tried =
            [("tcp", true), ("tcp6", v6), ("udp", true), ("udp6", v6), ("tcp-any", true), ("udp-any", true)];
        for (what, judged) in tried {
            if judged {
                assert_eq!(got[what], "ok", "{what}: {line}");
            }
        }
        assert!(refused.iter().all(|r| !r.contains("network-session")), "{line}");
    }

    /// Step 8: where sessions share this machine's loopback, a socket a sweep let go — made, and left unbound past
    /// two of its session's sweeps — is its session's again once a send binds it (`supervisor::destination` touches
    /// it): another session's datagram to it is refused and said, and nothing comes in. Both sessions open every
    /// port (theirs: its first send goes out). Elsewhere a session's loopback is its own: nothing to sweep.
    #[test]
    fn a_socket_a_sweep_let_go_is_its_sessions_again_once_it_binds() {
        let test = "a_socket_a_sweep_let_go_is_its_sessions_again_once_it_binds";
        if open_rules(test).is_none() {
            return;
        }
        let opened = |name: &str| {
            let (mut p, root) = project_policy(name, Vec::new(), &[]);
            p.localhost = true;
            Apart::of(name, p, root)
        };
        let (mine, theirs) = (opened("swept-mine"), opened("swept-theirs"));
        if mine.confine.net == RunNet::Plain || mine.keeper.ns.net {
            let tier = mine.confine.tier();
            mine.end();
            theirs.end();
            eprintln!("{test}: skipped — tier {tier}: no loopback shared between sessions");
            return;
        }
        let owners = &super::super::owners::OWNERS;
        let mut hold = theirs.helper("unbound", &[]);
        let mut out = io::BufReader::new(hold.spawned.stdout.take().unwrap());
        let said = |out: &mut io::BufReader<_>| loop {
            let mut line = String::new();
            assert!(io::BufRead::read_line(out, &mut line).unwrap() > 0, "the holder said nothing");
            if let Some((_, s)) = line.split_once("helper: ") {
                break s.trim().to_string();
            }
        };
        assert_eq!(said(&mut out), "made");
        let session = theirs.policy.session;
        let made = owners.of(session);
        // (twice: a sweep keeps what was touched since the one before)
        let live = super::super::owners::live().expect("tarae's tables");
        owners.swept(session, &live);
        owners.swept(session, &live);
        let swept = owners.of(session);
        std::fs::write(theirs.root.join("swept"), "").unwrap();
        let held = said(&mut out);
        let (tried, refused) = mine.helper_told("sockets", &[("TARAE_TO", &held)]);
        let tried = helper_said(tried);
        let refused: Vec<super::super::Denial> = refused.try_iter().collect();
        std::fs::write(theirs.root.join("done"), "").unwrap();
        io::Read::read_to_end(&mut out, &mut Vec::new()).unwrap();
        io::Read::read_to_end(&mut hold.spawned.stderr.take().unwrap(), &mut Vec::new()).unwrap();
        crate::spawn::wait(hold.spawned.pid).unwrap();
        let reached = theirs.root.join("reached-udp").exists();
        mine.end();
        theirs.end();
        let got: std::collections::HashMap<&str, &str> =
            tried.split_whitespace().filter_map(|w| w.split_once('=')).collect();
        let theirs_at: std::collections::HashMap<&str, &str> =
            held.split_whitespace().filter_map(|w| w.split_once('=')).collect();
        let refusals: Vec<String> = refused.iter().map(|d| d.reported()).collect();
        let line = format!(
            "rows {made} made, {swept} once swept twice; held {held}\n  tried: {tried}\n  came in: {reached}\n  \
             refused: {refusals:#?}\n"
        );
        assert!(made >= 1 && swept == 0, "the unbound socket wasn't let go — nothing tested: {line}");
        assert_eq!(theirs_at["sent"], "ok", "its first send: {line}");
        assert!(!reached, "the other session's datagram came in: {line}");
        assert_eq!(got["udp"], "unseen", "{line}");
        let port = format!(":{}", theirs_at["udp"]);
        let said = refused.iter().any(|d| d.op == super::super::SESSION_OP && d.target.ends_with(&port));
        assert!(said, "refused as another session's, said: {line}");
    }

    /// Step 8: a loopback listener of no session's — this test's, standing for the user's own — is reached on a
    /// port the policy opens (`llm.sandbox-ports`), and stays refused on one it doesn't, in every tier: the rule
    /// refuses another session's socket, never one it knows nothing of.
    #[test]
    fn the_users_listener_is_reached_where_opened() {
        let test = "the_users_listener_is_reached_where_opened";
        if open_rules(test).is_none() {
            return;
        }
        let (opened, closed) = (
            std::net::TcpListener::bind("127.0.0.1:0").unwrap(),
            std::net::TcpListener::bind("127.0.0.1:0").unwrap(),
        );
        let port = |l: &std::net::TcpListener| l.local_addr().unwrap().port();
        let (policy, root) = project_policy("users-listener", vec![port(&opened)], &[]);
        let s = Apart::of("users-listener", policy, root);
        if s.confine.net == RunNet::Plain {
            s.end();
            eprintln!("{test}: skipped — no sockets without a supervisor");
            return;
        }
        let tried = |l: &std::net::TcpListener| {
            let said = helper_said(s.helper("loopback", &[("TARAE_PORT", &port(l).to_string())]));
            said.split_once("tarae's=").map(|(_, r)| r.to_string()).unwrap_or(said)
        };
        let (to_opened, to_closed) = (tried(&opened), tried(&closed));
        let tier = s.confine.tier();
        s.end();
        assert_eq!(to_opened, "ok", "tier {tier}: the opened port");
        assert_ne!(to_closed, "ok", "tier {tier}: the closed port");
    }

    /// Step 8: where sessions share this machine's loopback the sockets a session's runs make are known as its
    /// (`owners`), and when its keeper ends none of them is any more — another session's stay. Without (a
    /// session's own netns) nothing is known: nothing to keep apart.
    #[test]
    fn an_ended_sessions_sockets_leave_the_table() {
        let test = "an_ended_sessions_sockets_leave_the_table";
        if open_rules(test).is_none() {
            return;
        }
        let owners = &super::super::owners::OWNERS;
        let (one, two) = (Apart::new("ended-one"), Apart::new("ended-two"));
        let shared = one.confine.net != RunNet::Plain && !one.keeper.ns.net;
        let holds = |s: &Apart| {
            let mut hold = s.helper("hold", &[]);
            let mut out = io::BufReader::new(hold.spawned.stdout.take().unwrap());
            let mut line = String::new();
            while !line.contains("helper: ") {
                line.clear();
                assert!(io::BufRead::read_line(&mut out, &mut line).unwrap() > 0, "the holder said nothing");
            }
            (hold, out)
        };
        let (h1, h2) = (holds(&one), holds(&two));
        let sessions = (one.policy.session, two.policy.session);
        let rows = |(a, b): (u64, u64)| (owners.of(a), owners.of(b));
        let before = rows(sessions);
        for (s, (mut h, mut out)) in [(&one, h1), (&two, h2)] {
            std::fs::write(s.root.join("done"), "").unwrap();
            io::Read::read_to_end(&mut out, &mut Vec::new()).unwrap();
            io::Read::read_to_end(&mut h.spawned.stderr.take().unwrap(), &mut Vec::new()).unwrap();
            crate::spawn::wait(h.spawned.pid).unwrap();
        }
        one.end();
        let after = rows(sessions);
        two.end();
        let gone = rows(sessions);
        let line = format!("rows (one, two): {before:?} held, {after:?} once one ended, {gone:?} once both");
        if shared {
            // (each holder made its TCP and UDP listeners, IPv6's too where there is IPv6)
            assert!(before.0 >= 2 && before.1 >= 2, "{line}");
            assert_eq!(after, (0, before.1), "{line}");
        } else {
            assert_eq!(before, (0, 0), "{line}");
        }
        assert_eq!(gone, (0, 0), "{line}");
    }

    /// Step 8: where sessions share this machine's loopback, a bind onto a port another session's socket holds —
    /// a TCP listener, a UDP socket, both with `SO_REUSEADDR` and `SO_REUSEPORT`, which the kernel lets another
    /// socket of the same user join — at an address that overlaps it (its own, every IPv4 address, IPv6's every
    /// address) is refused, `EADDRINUSE`, said by its address; `[::1]` beside its `127.0.0.1` isn't. A UDP bind to
    /// port 0 isn't given that port either (`supervisor::alone`: the kernel would). A TCP socket bound beside the
    /// holder's before either listened is refused at its listen, the holder's listening first. A run of the holder's
    /// own session makes the same binds — the control: the kernel takes them — but the port picked: never a taken one.
    #[test]
    fn a_bind_onto_another_sessions_port_is_refused() {
        let test = "a_bind_onto_another_sessions_port_is_refused";
        if open_rules(test).is_none() {
            return;
        }
        let (mine, theirs) = (Apart::new("binds-mine"), Apart::new("binds-theirs"));
        if mine.confine.net == RunNet::Plain || mine.keeper.ns.net {
            let tier = mine.confine.tier();
            mine.end();
            theirs.end();
            eprintln!("{test}: skipped — tier {tier}: no loopback shared between sessions");
            return;
        }
        // (what a run says next after `helper: `)
        fn next_said(out: &mut impl io::BufRead) -> String {
            loop {
                let mut line = String::new();
                assert!(out.read_line(&mut line).unwrap() > 0, "the run said nothing more");
                if let Some((_, s)) = line.split_once("helper: ") {
                    break s.trim().to_string();
                }
            }
        }
        let mut hold = theirs.helper("reuses", &[]);
        let mut out = io::BufReader::new(hold.spawned.stdout.take().unwrap());
        let held = next_said(&mut out);
        // Another session's binds — and its `late` one: a TCP socket bound beside the holder's before either
        // listens (the kernel lists a TCP socket only once it listens: no bind can tell), the holder's listening
        // first, then this one
        let (mut late, refused) = mine.helper_told("binds", &[("TARAE_TO", &held), ("TARAE_LATE", "1")]);
        let mut late_out = io::BufReader::new(late.spawned.stdout.take().unwrap());
        let bound_late = next_said(&mut late_out);
        std::fs::write(theirs.root.join("listen"), "").unwrap();
        let listened = next_said(&mut out);
        std::fs::write(mine.root.join("go"), "").unwrap();
        let by_mine = next_said(&mut late_out);
        io::Read::read_to_end(&mut late.spawned.stderr.take().unwrap(), &mut Vec::new()).unwrap();
        crate::spawn::wait(late.spawned.pid).unwrap();
        let refused_mine: Vec<super::super::Denial> = refused.try_iter().collect();
        let (started, refused) = theirs.helper_told("binds", &[("TARAE_TO", &held)]);
        let by_theirs = helper_said(started);
        let refused_theirs: Vec<super::super::Denial> = refused.try_iter().collect();
        std::fs::write(theirs.root.join("done"), "").unwrap();
        io::Read::read_to_end(&mut out, &mut Vec::new()).unwrap();
        io::Read::read_to_end(&mut hold.spawned.stderr.take().unwrap(), &mut Vec::new()).unwrap();
        crate::spawn::wait(hold.spawned.pid).unwrap();
        let tier = mine.confine.tier();
        mine.end();
        theirs.end();
        let said = |d: &[super::super::Denial]| d.iter().map(|d| d.reported()).collect::<Vec<String>>();
        let line = format!(
            "tier {tier}: held {held}\n  another session's: {bound_late}, then the holder {listened}; {by_mine}\n  \
             refused: {:#?}\n  its own session's: {by_theirs}\n  refused: {:#?}\n",
            said(&refused_mine),
            said(&refused_theirs)
        );
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
        let got = |s: &str| -> std::collections::HashMap<String, String> {
            s.split_whitespace()
                .filter_map(|w| w.split_once('='))
                .map(|(k, v)| (k.into(), v.into()))
                .collect()
        };
        let (mine_got, theirs_got) = (got(&by_mine), got(&by_theirs));
        let v6 = mine_got["v6"] == "true";
        let in_use = libc::EADDRINUSE.to_string();
        let mut wrong = Vec::new();
        for proto in ["tcp", "udp"] {
            // (what, overlaps the holder's 127.0.0.1, judged)
            for (what, overlaps, judged) in
                [("", true, true), ("-any", true, true), ("6-any", true, v6), ("6", false, v6)]
            {
                if !judged {
                    continue;
                }
                let key = format!("{proto}{what}");
                let want_mine = if overlaps { &in_use } else { "ok" };
                if mine_got[&key] != want_mine {
                    wrong.push(format!("another session's {key}: {}, not {want_mine}", mine_got[&key]));
                }
                if theirs_got[&key] != "ok" {
                    wrong.push(format!("its own session's {key}: {}, not ok", theirs_got[&key]));
                }
            }
            // (a port picked: never the taken one, whoever's — where the kernel can be made to pick only it)
            for (whose, got) in [("another session's", &mine_got), ("its own session's", &theirs_got)] {
                let picked = &got[&format!("{proto}-picked")];
                if picked != &libc::ENOPROTOOPT.to_string() && picked != &in_use {
                    wrong.push(format!("{whose} {proto} port 0 given the taken port: {picked}"));
                }
            }
        }
        // (the late one: bound beside it, the holder listening first — the kernel lets both —, then refused)
        if (bound_late.as_str(), listened.as_str()) != ("bound-late=ok", "listened=ok") {
            wrong.push(format!("the late one: {bound_late}, the holder {listened} — nothing tested"));
        }
        if mine_got["late"] != in_use {
            wrong.push(format!("another session's late listen: {}, not {in_use}", mine_got["late"]));
        }
        let theirs_at: std::collections::HashMap<&str, &str> =
            held.split_whitespace().filter_map(|w| w.split_once('=')).collect();
        for proto in ["tcp", "udp", "late"] {
            let at = format!("local:127.0.0.1:{}", theirs_at[proto]);
            let refused = |d: &super::super::Denial| {
                d.op == super::super::SESSION_OP
                    && d.target == at
                    && d.plain().contains("is another tarae session's")
            };
            if !refused_mine.iter().any(refused) {
                wrong.push(format!("{proto}: the bind at {at} not said as another session's"));
            }
        }
        if refused_theirs.iter().any(|d| d.op == super::super::SESSION_OP) {
            wrong.push("its own session's binds said as another's".into());
        }
        assert!(wrong.is_empty(), "{wrong:#?}\n{line}");
    }

    /// Step 8: where sessions share this machine's loopback, a UDP socket of one session's connected to a loopback
    /// port nothing is bound to sends there with plain `send()`, which tarae never serves — so the port is that
    /// session's to bind, not another's: another session's bind there (at that address, at every address), its port 0
    /// narrowed to it (`IP_LOCAL_PORT_RANGE`: `supervisor::picked` lets the kernel's pick go and refuses) and a first
    /// send's bind so narrowed are refused, `EADDRINUSE`, said, and nothing comes in. The control: a run of the
    /// sender's own session makes the same binds and takes in its datagrams — what the refusal keeps from the other.
    #[test]
    fn a_port_another_sessions_udp_socket_sends_to_is_kept_from_a_bind() {
        let test = "a_port_another_sessions_udp_socket_sends_to_is_kept_from_a_bind";
        if open_rules(test).is_none() {
            return;
        }
        let opened = |name: &str| {
            let (mut p, root) = project_policy(name, Vec::new(), &[]);
            p.localhost = true;
            Apart::of(name, p, root)
        };
        let (mine, theirs) = (opened("aimed-mine"), opened("aimed-theirs"));
        if mine.confine.net == RunNet::Plain || mine.keeper.ns.net {
            let tier = mine.confine.tier();
            mine.end();
            theirs.end();
            eprintln!("{test}: skipped — tier {tier}: no loopback shared between sessions");
            return;
        }
        // (a port free, found outside any run)
        let free = std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let mut aims = theirs.helper("aims", &[("TARAE_PORT", &free.to_string())]);
        let mut out = io::BufReader::new(aims.spawned.stdout.take().unwrap());
        let aimed = loop {
            let mut line = String::new();
            assert!(io::BufRead::read_line(&mut out, &mut line).unwrap() > 0, "the sender said nothing");
            if let Some((_, s)) = line.split_once("helper: ") {
                break s.trim().to_string();
            }
        };
        let binds = |s: &Apart| {
            let (started, refused) = s.helper_told("bindsat", &[("TARAE_TO", &aimed)]);
            (helper_said(started), refused.try_iter().collect::<Vec<super::super::Denial>>())
        };
        let ((by_mine, refused), (by_theirs, _)) = (binds(&mine), binds(&theirs));
        std::fs::write(theirs.root.join("done"), "").unwrap();
        io::Read::read_to_end(&mut out, &mut Vec::new()).unwrap();
        io::Read::read_to_end(&mut aims.spawned.stderr.take().unwrap(), &mut Vec::new()).unwrap();
        crate::spawn::wait(aims.spawned.pid).unwrap();
        let tier = mine.confine.tier();
        mine.end();
        theirs.end();
        let refusals: Vec<String> = refused.iter().map(|d| d.reported()).collect();
        let line = format!(
            "tier {tier}: {aimed}\n  another session's: {by_mine}\n  refused: {refusals:#?}\n  its own session's: \
             {by_theirs}\n"
        );
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
        let got = |s: &str| -> std::collections::HashMap<String, String> {
            s.split_whitespace()
                .filter_map(|w| w.split_once('='))
                .map(|(k, v)| (k.into(), v.into()))
                .collect()
        };
        let (mine_got, theirs_got, at) = (got(&by_mine), got(&by_theirs), got(&aimed));
        assert_eq!(at["connected"], "ok", "the sender connected: {line}");
        // (port 0 narrowed only where the kernel has `IP_LOCAL_PORT_RANGE`)
        let narrowed = mine_got["picked"] != libc::ENOPROTOOPT.to_string();
        let tried: &[&str] = if narrowed { &["named", "any", "picked", "sent"] } else { &["named", "any"] };
        let in_use = libc::EADDRINUSE.to_string();
        let mut wrong = Vec::new();
        for what in tried {
            if mine_got[*what] != in_use {
                wrong.push(format!("another session's {what}: {}, not {in_use}", mine_got[*what]));
            }
            if mine_got.contains_key(&format!("{what}-got")) {
                wrong.push(format!("another session's {what} took in the sender's datagrams"));
            }
            if theirs_got[*what] != "ok" || !theirs_got.contains_key(&format!("{what}-got")) {
                wrong.push(format!(
                    "its own session's {what}: {} — the control didn't take them in",
                    theirs_got[*what]
                ));
            }
        }
        let port = format!(":{}", at["aimed"]);
        let said =
            refused.iter().filter(|d| d.op == super::super::SESSION_OP && d.target.ends_with(&port)).count();
        if said < tried.len() {
            wrong.push(format!("{said} of {} said as another session's", tried.len()));
        }
        assert!(wrong.is_empty(), "{wrong:#?}\n{line}");
    }

    /// M8.3: what step 8 adds to a served call — a UDP send to an opened loopback port, each one served: the
    /// lookup of the port's sockets (`supervisor::anothers_at`) and the socket's checks — against the same session
    /// without it (`supervisor::APART_OFF`), 2 000 sends each, five times; a send that's never served (on a connected
    /// socket) beside. Where sessions share this machine's loopback.
    #[test]
    fn perf_a_served_loopback_send() {
        if crate::test_results::unoptimized("sandbox::keeper::tests::perf_a_served_loopback_send") {
            return;
        }
        let test = "perf_a_served_loopback_send";
        if open_rules(test).is_none() {
            return;
        }
        let (mut p, root) = project_policy("sendloop", Vec::new(), &[]);
        p.localhost = true;
        let s = Apart::of("sendloop", p, root);
        if s.confine.net == RunNet::Plain || s.keeper.ns.net {
            s.end();
            eprintln!("{test}: skipped — no loopback shared between sessions here");
            return;
        }
        let off = &super::super::supervisor::APART_OFF;
        let mut said = Vec::new();
        for round in 0..10 {
            let without = round % 2 == 1;
            off.store(if without { s.policy.session } else { 0 }, std::sync::atomic::Ordering::Relaxed);
            said.push(format!(
                "{}: {}",
                if without { "without" } else { "with" },
                helper_said(s.helper("sendloop", &[]))
            ));
        }
        off.store(0, std::sync::atomic::Ordering::Relaxed);
        let tier = s.confine.tier();
        s.end();
        let line = format!("M8.3, tier {tier}, µs a send — {}\n", said.join(" · "));
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
    }

    /// Step 8: a session's keeper killed unasked (it died), what its runs left running — a listener a build's
    /// daemon holds, here one ending by tag doesn't reach (these tests' runs carry none tarae knows) — stays that
    /// session's while it holds its sockets: another session's call to it is refused, said, and nothing comes in;
    /// once it's gone, so are its rows. Where sessions share this machine's loopback, and the keeper has no PID
    /// namespace (with one, what it left dies with it).
    #[test]
    fn a_dead_keepers_leftover_listener_stays_its_sessions() {
        leftover_stays_its_sessions("a_dead_keepers_leftover_listener_stays_its_sessions", false);
    }

    /// Step 8: as `a_dead_keepers_leftover_listener_stays_its_sessions`, the keeper asked to end and killed for not
    /// ending in time (`ended_by` past its deadline — here one already past, the end asked but never sent: it
    /// couldn't end what its runs left) — asked, yet what its runs left may outlive it (`State.killed`).
    #[test]
    fn a_killed_keepers_leftover_listener_stays_its_sessions() {
        leftover_stays_its_sessions("a_killed_keepers_leftover_listener_stays_its_sessions", true);
    }

    /// `a_dead_keepers_leftover_listener_stays_its_sessions`, the keeper killed unasked — or, `overran`, asked to end
    /// and killed past its deadline.
    fn leftover_stays_its_sessions(test: &str, overran: bool) {
        if open_rules(test).is_none() {
            return;
        }
        let tag = if overran { "killed" } else { "dead" };
        let (mine_name, theirs_name) = (format!("leftover-{tag}-mine"), format!("leftover-{tag}-theirs"));
        let (mut open, root) = project_policy(&mine_name, Vec::new(), &[]);
        open.localhost = true;
        let (mine, theirs) = (Apart::of(&mine_name, open, root), Apart::new(&theirs_name));
        if mine.confine.net == RunNet::Plain || mine.keeper.ns.net || theirs.keeper.ns.pid {
            let tier = mine.confine.tier();
            mine.end();
            theirs.end();
            eprintln!(
                "{test}: skipped — tier {tier}: no loopback shared, or what a keeper leaves dies with it"
            );
            return;
        }
        let owners = &super::super::owners::OWNERS;
        let session = theirs.policy.session;
        let left = helper_said(theirs.helper("leaves", &[]));
        if overran {
            // (asked to end, and its time past: `ended_by` kills it — the end never sent, it ended nothing itself)
            lock(&theirs.keeper.state).ending = true;
            theirs.keeper.ended_by(Instant::now());
        } else {
            // SAFETY: KILL to the test's own keeper, by its pid (still ours: unreaped until `wait_gone`).
            unsafe { libc::kill(theirs.keeper.pid as i32, libc::SIGKILL) };
        }
        assert!(gone_within(&theirs.keeper, Duration::from_secs(5)), "the keeper ended");
        let rows_dead = owners.of(session);
        let (tried, refused) = mine.helper_told("sockets", &[("TARAE_TO", &left)]);
        let tried = helper_said(tried);
        let refused: Vec<super::super::Denial> = refused.try_iter().collect();
        std::fs::write(theirs.root.join("done"), "").unwrap();
        // (its rows go as its sockets do: asked again from 100 ms, doubling)
        let by = Instant::now() + Duration::from_secs(15);
        while owners.of(session) > 0 && Instant::now() < by {
            std::thread::sleep(Duration::from_millis(50));
        }
        let rows_after = owners.of(session);
        let reached: Vec<String> = std::fs::read_dir(&theirs.root)
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.strip_prefix("reached-").map(String::from))
            .collect();
        mine.end();
        theirs.end();
        let got: std::collections::HashMap<&str, &str> =
            tried.split_whitespace().filter_map(|w| w.split_once('=')).collect();
        let theirs_at: std::collections::HashMap<&str, &str> =
            left.split_whitespace().filter_map(|w| w.split_once('=')).collect();
        let refusals: Vec<String> = refused.iter().map(|d| d.reported()).collect();
        let line = format!(
            "rows {rows_dead} once the keeper died, {rows_after} once what it left ended; left {left}\n  tried: \
             {tried}\n  came in: {reached:?}\n  refused: {refusals:#?}\n"
        );
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
        assert!(rows_dead >= 2, "what it left isn't its session's: {line}");
        for what in ["tcp", "udp"] {
            assert_ne!(got[what], "ok", "{what}: {line}");
            assert!(!reached.iter().any(|r| r == what), "{what} came in: {line}");
            let port = format!(":{}", theirs_at[what]);
            let said = refused.iter().any(|d| d.op == super::super::SESSION_OP && d.target.ends_with(&port));
            assert!(said, "{what}: refused as another session's, said: {line}");
        }
        assert_eq!(rows_after, 0, "{line}");
    }

    /// Step 7: two sessions side by side, files — from a run of one into each place a run of the other writes
    /// but the temp folders (shared, D16): its checkout, its run's own folder (in the same `runs` as this one's,
    /// as every session's are), its project's kept run home, the home a daemon of its policy lives from — in
    /// each a file made and the one there written. Landlock grants a run its own session's places only, in every
    /// tier alike (namespaces keep nothing of files here), so each is refused. The trying run writes its own of
    /// each first: a try that writes nothing would pass. The repository's other checkouts are read by design
    /// (D16) — pinned as shared, with a folder beside them in home refused: reads are confined there, so the
    /// read is the policy's grant, not an open home.
    #[test]
    fn two_sessions_cant_write_each_others_files() {
        let test = "two_sessions_cant_write_each_others_files";
        if open_rules(test).is_none() {
            return;
        }
        // (under this crate's `target/`, standing for home: outside the temp folders, where every run writes)
        let home = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("target/tarae-apart-files-{}", std::process::id()));
        std::fs::remove_dir_all(&home).ok();
        std::fs::create_dir_all(&home).unwrap();
        let home = std::fs::canonicalize(&home).unwrap();
        let (main, wt) = crate::allow::tests::repo_with_worktree(&home);
        let elsewhere = home.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let (mine, theirs) =
            (Apart::writing("files-mine", &main, &home), Apart::writing("files-theirs", &wt, &home));
        let (own, others) = (mine.writes(), theirs.writes());
        for (_, dir) in own.iter().chain(&others) {
            std::fs::write(dir.join("there"), "").unwrap();
        }
        std::fs::write(elsewhere.join("there"), "").unwrap();
        let listed = |l: &[(&str, PathBuf)]| -> String {
            l.iter().map(|(w, d)| format!("{w}={}\n", d.display())).collect()
        };
        let reads = [("checkout", wt.join("src/there")), ("elsewhere", elsewhere.join("there"))];
        let said = helper_said(mine.helper(
            "files",
            &[("TARAE_OWN", &listed(&own)), ("TARAE_TO", &listed(&others)), ("TARAE_READS", &listed(&reads))],
        ));
        // What landed in the other session's places, seen from here
        let landed: Vec<PathBuf> = others
            .iter()
            .flat_map(|(_, d)| std::fs::read_dir(d).into_iter().flatten().flatten())
            .map(|e| e.path())
            .filter(|f| match f.file_name().and_then(|n| n.to_str()) {
                Some("there") => std::fs::metadata(f).is_ok_and(|m| m.len() > 0),
                _ => true,
            })
            .collect();
        let (ns, net, tier) = (mine.keeper.ns, mine.confine.net, mine.confine.tier());
        let g = mine.confine.guarantees.clone().expect("a keeper's run has its guarantees");
        mine.end();
        theirs.end();
        std::fs::remove_dir_all(&home).ok();
        let got: std::collections::HashMap<&str, &str> =
            said.split_whitespace().filter_map(|w| w.split_once('=')).collect();
        let line = format!("tier {tier} ({ns:?}, {net:?}): {said}\n");
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
        let mut wrong = Vec::new();
        // (emptying a file by its path: refused from ABI 3, below it reached — another session's files too)
        let abi = crate::landlock::abi().unwrap();
        let truncate = if abi >= 3 { "13" } else { "ok" };
        for (what, _) in &own {
            for (how, refused) in [("make", "13"), ("write", "13"), ("truncate", truncate)] {
                if got[&*format!("own-{what}-{how}")] != "ok" {
                    wrong.push(format!("its own {what}: {how} not ok — the try reaches nothing"));
                }
                // (Landlock's refusal: EACCES)
                let theirs = got[&*format!("{what}-{how}")];
                if theirs != refused {
                    wrong.push(format!("the other session's {what}: {how} {theirs}, not {refused}"));
                }
            }
        }
        if got["checkout-read"] != "ok" {
            wrong.push("the repository's other checkout isn't read (D16: shared)".into());
        }
        if got["elsewhere-read"] != "13" {
            wrong
                .push("a folder in home that's no checkout is read: the checkout's read pins nothing".into());
        }
        // 9a: its guarantees say its files shared exactly where one of the other session's was made, written or emptied
        let tries = ["make", "write", "truncate"];
        let written =
            others.iter().any(|(what, _)| tries.iter().any(|h| got[&*format!("{what}-{h}")] == "ok"));
        let found = [(super::super::guarantees::Part::Files, Some(written || !landed.is_empty()))];
        wrong.extend(super::super::guarantees::said_as_found(&g, &found));
        assert!(wrong.is_empty() && landed.is_empty(), "{wrong:#?}\nlanded: {landed:?}\n{line}");
    }

    /// The `files` helper: in each `$TARAE_OWN` folder, then each `$TARAE_TO` one (`what=folder` lines), a file
    /// made, `there` appended to and emptied by its path; each `$TARAE_READS` file read — `[own-]what-make=…
    /// what-write=… what-truncate=… what-read=…`.
    fn files_tried() -> String {
        let list = |var: &str| {
            let v = std::env::var(var).unwrap_or_default();
            v.lines()
                .filter_map(|l| l.split_once('='))
                .map(|(w, p)| (w.to_string(), PathBuf::from(p)))
                .collect::<Vec<_>>()
        };
        let mut said = Vec::new();
        for (pre, var) in [("own-", "TARAE_OWN"), ("", "TARAE_TO")] {
            for (what, dir) in list(var) {
                let made = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(dir.join(format!("made-{}", std::process::id())));
                let written = std::fs::OpenOptions::new()
                    .append(true)
                    .open(dir.join("there"))
                    .and_then(|mut f| io::Write::write_all(&mut f, b"x"));
                // (emptied by its path, no file opened to write: Landlock's truncating rule, from ABI 3)
                let there = std::os::unix::ffi::OsStringExt::into_vec(dir.join("there").into_os_string());
                let there = std::ffi::CString::new(there).unwrap();
                // SAFETY: a plain call on a path; the file is the test's own.
                let emptied = io_result(unsafe { libc::truncate(there.as_ptr(), 0) });
                said.push(format!(
                    "{pre}{what}-make={} {pre}{what}-write={} {pre}{what}-truncate={}",
                    tried(made),
                    tried(written),
                    tried(emptied)
                ));
            }
        }
        for (what, file) in list("TARAE_READS") {
            said.push(format!("{what}-read={}", tried(std::fs::read(file))));
        }
        said.join(" ")
    }

    /// 3b: without a `devpts` of the session's own (C) a run reaches the ptys it makes — as libc's
    /// `openpty` makes them (the session-mounts helper's pty), through `TIOCGPTPEER`, again through `/proc/self/fd`
    /// — and no other: a terminal of the user's is neither read (what's typed there) nor written, and a master it
    /// made whose number it read without asking tarae (`fdinfo`: no call tarae sees) is refused its other end —
    /// that pty isn't one the rules grant (Landlock grants inodes), nor ever has a pool pty's number (tarae holds
    /// those). The same master asked for its number as programs do is then the pool's, and opens. All that with
    /// none of the pool's `O_PATH` other ends left open in tarae once the rules are made (`Ptys::granted`).
    #[test]
    fn a_run_opens_only_the_ptys_it_makes() {
        let test = "a_run_opens_only_the_ptys_it_makes";
        if open_rules(test).is_none() {
            return;
        }
        let s = Apart::new("ptys");
        if s.confine.ptys.is_none() {
            let why = if s.keeper.ns.user { "its session has a devpts of its own" } else { "not served" };
            eprintln!("{test}: skipped — tier {}: {why}", s.confine.tier());
            s.end();
            return;
        }
        // (the rules made, tarae keeps no other end of the pool's — half its descriptors, `Ptys::granted` — and the
        // run below still opens its own: a rule holds its inode)
        let pool = s.confine.ptys.as_ref().unwrap();
        let mut ends = 0;
        pool.each(|_, _| {
            ends += 1;
            Ok(())
        })
        .unwrap();
        let ours: Vec<String> = pool.numbers().iter().map(|n| format!("/dev/pts/{n}")).collect();
        let open = std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .flatten()
            .filter_map(|e| std::fs::read_link(e.path()).ok())
            .filter(|to| ours.iter().any(|o| to.as_os_str() == o.as_str()))
            .count();
        assert_eq!((ends, open), (0, 0), "no O_PATH other end of the pool's left open");
        let (_m, _s, other) = pty_opened().expect("a pty of this process's: another terminal of the user's");
        let said = helper_said(s.helper("ptys", &[("TARAE_TO", &other)]));
        let tier = s.confine.tier();
        s.end();
        let _ = io::Write::write_all(&mut io::stderr(), format!("tier {tier}: {said}\n").as_bytes());
        let refused = libc::EACCES;
        // (`reused`: tarae holds the pool's masters, so a fresh pty never gets a pool pty's number while the run goes)
        let want = format!(
            "own=ok peer=ok reopen=ok read={refused} write={refused} fresh={refused} reused=false then=ok"
        );
        assert_eq!(said, want);
    }

    /// 3b, the pool at a time: a pty call on a descriptor that isn't a master is the kernel's `ENOTTY`; locking a
    /// pool pty again is `EINVAL`; ptys made and closed one after another go back to the pool — 100 all open; held
    /// at once, the one past `POOL` is `EACCES`, and the refusal is said (`pty-make`, in plain words) once; a pty
    /// whose master the run closed is hung up for what still reads its other end, as without tarae (whose own
    /// master would hold it up).
    #[test]
    fn a_run_holds_its_ptys_at_a_time() {
        let test = "a_run_holds_its_ptys_at_a_time";
        if open_rules(test).is_none() {
            return;
        }
        let s = Apart::new("pty-refusals");
        if s.confine.ptys.is_none() {
            eprintln!("{test}: skipped — tier {}: its ptys aren't tarae's to serve", s.confine.tier());
            s.end();
            return;
        }
        let (started, refused) = s.helper_told("pty-refusals", &[]);
        let said = helper_said(started);
        s.end();
        let pool = super::super::ptys::POOL;
        let want = format!(
            "nonmaster={} relock={} seq=100 held={pool} past={} hangup=ok",
            libc::ENOTTY,
            libc::EINVAL,
            libc::EACCES
        );
        assert_eq!(said, want);
        let told: Vec<String> = refused.try_iter().map(|d| format!("{} {}", d.op, d.plain())).collect();
        let once = format!("pty-make making more terminals than a run holds at a time ({pool})");
        assert_eq!(told, [once], "the one refusal, said");
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
        let s = k.start_confined(&cmd, &crate::landlock::thread_name(), &c).unwrap();
        let status = crate::spawn::wait(s.spawned.pid).unwrap();
        assert!(status.success() && std::fs::read_to_string(root.join("made")).is_ok_and(|m| m == "made"));
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// 9a: a run's guarantees are what its own keeper and supervisor decide for it — never the session's first run's.
    /// In every tier: runs on the session's keeper served, then not (no supervisor for that run), then served again
    /// each say how their sockets go out (the network's way: tarae's proxy — then no socket). Where
    /// the session has namespaces, also its keepers: a policy that writes in `/dev/shm` (its keeper born without a
    /// `/dev/shm` of its own, D14: another keeper for another policy) between two that don't, each says theirs; without
    /// namespaces `/dev/shm` is this system's for every keeper (said, not judged; a machine said to give them fails).
    #[test]
    fn the_guarantees_are_the_runs() {
        let test = "the_guarantees_are_the_runs";
        if open_rules(test).is_none() {
            return;
        }
        use super::super::guarantees::{Part, Way};
        let (outside, root) = project_policy("guarantees", Vec::new(), &[]);
        // (both folders gone when the test ends, failed or not)
        struct Gone(PathBuf);
        impl Drop for Gone {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).ok();
            }
        }
        let _root = Gone(root.clone());
        let shm = PathBuf::from(format!("/dev/shm/tarae-guarantees-{}", std::process::id()));
        let _shm = Gone(shm.clone());
        std::fs::create_dir_all(&shm).unwrap();
        let write = [outside.write.clone(), vec![std::fs::canonicalize(&shm).unwrap()]].concat();
        let inside = super::super::Policy { write, ..outside.clone() };
        let said = |p: &super::super::Policy| {
            let run = super::super::RunDir::new().ok();
            let p = super::super::confined(super::super::Policy { run, ..p.clone() });
            let c = p.confine.as_ref().unwrap().as_ref().unwrap_or_else(|e| panic!("{e}"));
            let g = c.guarantees.as_ref().expect("a keeper's run has its guarantees");
            (g.kept(Part::Shm), g.line(), c.keeper.clone().expect("its keeper"))
        };
        let got = [said(&outside), said(&inside), said(&outside)];
        // (the session's keeper now: the last policy's)
        let k = got[2].2.clone();
        let abi = crate::landlock::abi().unwrap();
        let way = |serve: Result<(), String>| {
            let c = super::super::linux::confine(
                &outside,
                abi,
                Err("not watched".into()),
                false,
                Some(&k),
                serve,
            )
            .unwrap();
            let g = c.guarantees.expect("a keeper's run has its guarantees");
            (g.way, g.line(), g.brief)
        };
        let served = super::super::linux::served_here();
        let ways = [way(served.clone()), way(Err("no supervisor for this run".into())), way(served.clone())];
        for k in of(Some(&root)) {
            k.end();
        }
        let lines: Vec<&String> = got.iter().map(|g| &g.1).chain(ways.iter().map(|w| &w.1)).collect();
        if served.is_ok() {
            let w = ways.each_ref().map(|w| w.0);
            assert!(w[0] != Way::NoSocket && w[1] == Way::NoSocket && w[2] == w[0], "{w:?}\n{lines:#?}");
        } else {
            eprintln!("{test}: the network's way not judged — tarae serves no run here");
        }
        // (9e: `confine` hands the probe's few words on — each run's brief starts with them, as its why with `short`)
        match &super::super::tier::get().brief {
            Some(b) => {
                let briefs = ways.each_ref().map(|w| w.2.clone());
                assert!(
                    briefs.iter().all(|x| x.as_deref().is_some_and(|x| x.starts_with(b.as_str()))),
                    "{b}: {briefs:?}"
                );
            }
            None => eprintln!("{test}: the probe's brief not judged — every namespace here, nothing to say"),
        }
        match super::super::tier::get().ns.user {
            true => {
                let shm = got.each_ref().map(|g| g.0);
                assert_eq!(shm, [Some(true), Some(false), Some(true)], "{lines:#?}")
            }
            false => {
                let why =
                    "no namespaces here: /dev/shm is this system's for both, nothing of a keeper's to judge";
                if let Ok(want) = std::env::var("TARAE_EXPECT_TIER")
                    && want.trim() == "own"
                {
                    panic!("{test}: tier {want} expected here — {why}");
                }
                let line =
                    format!("{}sandbox::keeper::tests::{test} — {why}\n", crate::test_results::SELF_SKIP);
                let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
            }
        }
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
    /// one that opens nothing does — its calls routed out of it (step 4): a socket of tarae's can always take a
    /// run's place (6f: `SETFD` is older than the 5.14 floor, so D-a's "no netns" is gone). A keeper of the
    /// other key is superseded.
    #[test]
    fn an_opened_policys_keeper_has_its_netns() {
        use super::super::tier;
        if !own_network("an_opened_policys_keeper_has_its_netns") {
            return;
        }
        let (closed, _, r1) = project_in("netns-closed", Vec::new(), None, &[]);
        let (open, _, r2) = project_in("netns-open", vec![9], None, &[]);
        let open = super::super::Policy { network: true, localhost: true, ..open };
        let (closed_ns, open_ns) = (tier::for_policy(&closed), tier::for_policy(&open));
        assert_eq!((closed_ns.net, open_ns.net), (ns().net, ns().net));
        let root = std::env::temp_dir().join(format!("tarae-keeper-netns-{}", std::process::id()));
        let net = |pid: u32| std::fs::read_link(format!("/proc/{pid}/ns/net")).unwrap();
        let a = ready(&root, Some("closed"), closed_ns).unwrap();
        assert_ne!(net(a.pid), net(std::process::id()), "the session's own");
        let b = ready(&root, Some("open"), open_ns).unwrap();
        assert_ne!(a.pid, b.pid, "another keeper");
        assert_ne!(net(b.pid), net(std::process::id()), "the session's own, routed");
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
        if !own_network("in_plain_mode_a_run_reaches_only_its_own_loopback") {
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
        if !own_network(test) {
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
        if !own_network(test) {
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
        let s = k.start_served(&cmd, &name, c, p.clone(), tx).unwrap();
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
    /// tier host (D-e) and the session's netns alike. The outside is a listener of the test's on this
    /// machine's own address, named for the proxy (`proxy::TEST_NAMES`), beyond this machine for a policy made with
    /// no own addresses.
    #[test]
    fn a_proxied_connect_holds_a_tcp_socket() {
        let Some(tiers) = served_tiers("a_proxied_connect_holds_a_tcp_socket") else { return };
        let Some(ip) = outside() else { return };
        for (tier, ns) in tiers {
            let (at, served) = answering(&format!("{ip}:0"), "out hello");
            let host = format!("outside-{tier}.test");
            super::super::proxy::TEST_NAMES.lock().unwrap().push((host.clone(), at));
            let name = format!("proxied-{tier}");
            let k = keeper_for(&name, ns);
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
    /// test stands in for the main loop (`proxy::TEST_ASKS`). In tier host and the session's netns alike.
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
            let k = keeper_for(&name, ns);
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

    /// Piece C: the network closed (`llm.sandbox-network` off), a run's connection beyond still reaches tarae's
    /// proxy — the closed policy goes beyond through it with an empty list, by the proxy variable's address — and
    /// waits there on the main loop's answer: `y` lets this run's request through alone; the next run is asked
    /// again, and a no refuses it, nothing more arriving outside. The test stands in for the main loop
    /// (`proxy::TEST_ASKS`). In tier host and the session's netns alike.
    #[test]
    fn with_the_network_closed_a_connection_waits_on_the_card() {
        use super::super::proxy::{self, FromProxy, Scope, ToProxy};
        let test = "with_the_network_closed_a_connection_waits_on_the_card";
        let Some(tiers) = served_tiers(test) else { return };
        let Some(ip) = outside() else { return };
        for (i, (tier, ns)) in tiers.into_iter().enumerate() {
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
            let host = format!("closed-{tier}.test");
            proxy::TEST_NAMES.lock().unwrap().push((host.clone(), at));
            let name = format!("closed-{tier}");
            let k = keeper_for(&name, ns);
            let closed = |e: &mut super::super::Env| e.asks_hosts = true;
            let (p, root) = project_policy_with(&name, Vec::new(), &[], closed);
            assert!(
                p.network && p.asks_hosts && !p.egress.any && p.egress.hosts.is_empty(),
                "{tier}: beyond through the proxy, each host asked, an empty list"
            );
            assert!(p.proxy_url().is_some(), "{tier}: the proxy variable");
            let abi = crate::landlock::abi().unwrap();
            let c = super::super::linux::confine(&p, abi, Err("not watched".into()), false, Some(&k), Ok(()))
                .unwrap();
            let session = proxy::session_of(&p.root);
            let (asks, asked) = std::sync::mpsc::channel();
            proxy::TEST_ASKS.lock().unwrap().push((session, asks));
            let of_run = |run_id| {
                Arc::new(super::super::Policy { own_addresses: Some(Vec::new()), run_id, ..p.clone() })
            };
            let base = 5_100_001 + 10 * i as u64;
            let (first, second) = (of_run(base), of_run(base + 1));
            // The main loop, stood in: `y` (this run) to the first question, `n` to the next
            let main = std::thread::spawn(move || {
                let mut seen = Vec::new();
                for scope in [Scope::Run, Scope::Refused] {
                    let Ok(a) = asked.recv_timeout(Duration::from_secs(10)) else { break };
                    let FromProxy::Ask { run, host, port, .. } = a.clone();
                    proxy::get().send(ToProxy::Answer { session, run, host, port, scope });
                    seen.push(a);
                }
                seen
            });
            let (to, named) = (super::super::route::PROXY.to_string(), format!("{host}:{}", at.port()));
            let reach = |p: &Arc<super::super::Policy>| {
                let env = [("TARAE_TO", to.as_str()), ("TARAE_HOST", named.as_str())];
                served_said(&k, (p, &c, &root), "proxied", &env)
            };
            let (said, refused) = reach(&first);
            assert!(
                said.ends_with("reply=out hello"),
                "{tier}: `y` — the held request went on: {said} {refused:?}"
            );
            assert!(served.recv_timeout(Duration::from_secs(5)).is_ok(), "{tier}: it arrived outside");
            let (said, refused) = reach(&second);
            assert!(!said.ends_with("reply=out hello"), "{tier}: `n` — refused: {said} {refused:?}");
            let seen = main.join().unwrap();
            let runs: Vec<(u64, &str)> = seen
                .iter()
                .map(|a| {
                    let FromProxy::Ask { run, host, .. } = a;
                    (*run, host.as_str())
                })
                .collect();
            assert_eq!(runs, [(base, host.as_str()), (base + 1, host.as_str())], "{tier}: each run asked");
            assert!(served.recv_timeout(Duration::from_millis(500)).is_err(), "{tier}: nothing more outside");
            k.end();
            std::fs::remove_dir_all(&root).ok();
        }
    }

    /// Step 5: with the network through the proxy, a connect beyond this machine that doesn't go through it — a
    /// tool that ignores `HTTPS_PROXY` — is refused (`EPERM`, `network-direct`) and said in words that name the
    /// way out; nothing arrives outside. A datagram out is refused the same way. In tier host and the netns alike.
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
            let k = keeper_for(&name, ns);
            let (p, c, root) = project_served(&name, Vec::new(), Some(&k), &[], Ok(()));
            let p = Arc::new(super::super::Policy { network: true, own_addresses: Some(Vec::new()), ..p });
            let to = l.local_addr().unwrap().to_string();
            let (said, refused) = served_said(&k, (&p, &c, &root), "connect", &[("TARAE_TO", &to)]);
            assert_eq!(said, libc::EPERM.to_string(), "{tier}: {refused:?}");
            let d =
                refused.iter().find(|d| d.op == "network-direct").unwrap_or_else(|| panic!("{refused:?}"));
            assert!(
                d.plain().contains(&format!("{to} directly")) && d.plain().contains("HTTPS_PROXY"),
                "{}",
                d.plain()
            );
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
    /// said as a name lookup, and nothing arrives; in tier host and the session's netns alike. (Measured first: no
    /// tool going through the proxy looked a name up itself — `measure_through_the_proxy`.)
    #[test]
    fn under_the_proxy_a_resolver_isnt_reached() {
        let Some(tiers) = served_tiers("under_the_proxy_a_resolver_isnt_reached") else { return };
        for (tier, ns) in tiers {
            let resolver = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            resolver.set_nonblocking(true).unwrap();
            let at = resolver.local_addr().unwrap();
            let name = format!("resolver-{tier}");
            let k = keeper_for(&name, ns);
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

    /// The served tiers here, each run's keeper born in them (`keeper_for`): tier host's (no namespaces); the
    /// session's netns where this system gives one, whose runs go out by the proxy's variable and the pair (6f's F3).
    /// None where nothing's served.
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

    /// A keeper for `test`, born with `ns`.
    fn keeper_for(test: &str, ns: Ns) -> Arc<Keeper> {
        let root = std::env::temp_dir().join(format!("tarae-keeper-{}-{test}", std::process::id()));
        Keeper::born_lo(&root, None, ns, Families::here(), false).expect("a keeper starts")
    }

    /// This machine's own address beyond loopback (a container's, a LAN's, Docker's bridge's) is this machine,
    /// not beyond it: with the network open a listener of the test's there isn't reached — refused, said, nothing
    /// arrives — until its port is opened (`llm.sandbox-ports`); in tier host and the session's netns alike. The own
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
            let k = keeper_for(&name, ns);
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
    /// `listen` would bind it to every address of this machine — refused with `EINVAL` and said; in tier host and the
    /// session's netns alike. (A run's own listener: `two_runs_share_the_sessions_loopback`.)
    #[test]
    fn a_socket_put_out_of_the_session_isnt_listened_on() {
        let Some(tiers) = served_tiers("a_socket_put_out_of_the_session_isnt_listened_on") else {
            return;
        };
        let Some(ip) = outside() else { return };
        for (tier, ns) in tiers {
            let name = format!("swaplisten-{tier}");
            let k = keeper_for(&name, ns);
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

    /// In the session's netns a run makes its own internet sockets — TCP's and UDP's only, as tier host's tarae makes
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
        if !own_network(test) {
            return;
        }
        let Some(rules) = open_rules(test) else { return };
        let root = std::env::temp_dir().join(format!("tarae-keeper-{}-offlo", std::process::id()));
        let both = Families { v4: true, v6: true };
        let k = Keeper::born_lo(&root, None, ns(), both, true).expect("born, without its mirrors");
        assert_eq!(k.lo_missing, both, "neither carried");
        assert!(k.said().contains("its lo without an IPv4 or IPv6 address"), "{}", k.said());
        let said =
            helper_through(&k, rules.fd(), RunNet::Own, "lookup6", &[("TARAE_NAME", "ip6-localhost")], None);
        assert!(said.starts_with("v6=::1 lookup6=getaddrinfo"), "{said}");
        let said = helper_through(&k, rules.fd(), RunNet::Own, "lookup4", &[], None);
        assert!(said.starts_with("v4=127.0.0.1 "), "{said}");
        k.end();
        let k = Keeper::born_lo(&root, None, ns(), both, false).expect("a keeper starts");
        assert_eq!(k.lo_missing, Families::default(), "both carried where they can be");
        assert!(!k.said().contains("its lo"), "{}", k.said());
        k.end();
    }

    /// The second lock under the routing: a run under `Routed`'s rules whose `connect` the filter let through
    /// (seccomp's `OwnNetwork` standing in) is refused by Landlock itself (ABI ≥ 4), even to its own listener.
    #[test]
    fn a_routed_runs_own_connect_is_refused_by_the_kernel() {
        let test = "a_routed_runs_own_connect_is_refused_by_the_kernel";
        if !own_network(test) {
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
        if !own_network("an_ipv4_lookup_finds_an_address_in_the_session") {
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
        if !own_network(test) {
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

    /// The review's first finding: a run's child allocates and frees nothing between the keeper's fork and its exec
    /// — in a keeper with another thread (a test's) only async-signal-safe calls are safe there. Shown
    /// deterministically rather than by catching a thread holding malloc's lock across the fork (a race a test can
    /// only hope to hit): the test binary's allocator ends a child that allocates (`NO_ALLOC`, exit 121) — every run
    /// of every test here would show it; this one goes through the way out too (a folder that isn't there: `fail`'s
    /// words, made of fixed bytes and the errno's digits).
    #[test]
    fn a_runs_child_allocates_nothing_before_its_exec() {
        let Some(rules) = open_rules("a_runs_child_allocates_nothing_before_its_exec") else { return };
        let k = keeper("noalloc");
        let (out, status) = sh_through(&k, &rules, "echo ran");
        assert_eq!((status.code(), out.as_str()), (Some(0), "ran\n"), "the child allocated: {ALLOCATED}?");
        let missing = std::env::temp_dir().join(format!("tarae-noalloc-{}-missing", std::process::id()));
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "true"]).current_dir(&missing);
        // (tarae slow to number it, held: its failing first had it gone, reaped, before its pid was read — its
        // start then "it ended before it started"; it waits for its byte first)
        lock(&k.state).number_after = Some(std::time::Duration::from_millis(200));
        let s = k.start(&cmd, &crate::landlock::thread_name(), rules.fd(), RunNet::Plain).unwrap();
        let mut err = String::new();
        io::Read::read_to_string(&mut s.spawned.stderr.unwrap(), &mut err).unwrap();
        let status = crate::spawn::wait(s.spawned.pid).unwrap();
        assert_eq!(status.code(), Some(127), "its way out, not an allocation ({ALLOCATED}): {err}");
        let said = format!("tarae's sandbox couldn't start it — its folder: errno {}\n", libc::ENOENT);
        assert_eq!(err, said);
        k.end();
    }
}
