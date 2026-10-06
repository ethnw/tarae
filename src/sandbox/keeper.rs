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
    Keeper::born(root, None, ns)
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
    /// failed before is the error (`BirthError` inside it).
    fn born(root: &Path, key: Option<&str>, ns: Ns) -> io::Result<Arc<Keeper>> {
        let exe = exe()?;
        let (ours, theirs) = seqpacket()?;
        let null = open_null(libc::O_RDWR)?;
        let birth = Birth::new()?;
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
            said_ready(ours.as_fd(), told)
        })();
        let (ready, devices) = match ready {
            Ok(devices) => (Ok(()), devices),
            Err(e) => (Err(e), Vec::new()),
        };
        let k = Arc::new(Keeper {
            pid: pid as u32,
            root: root.to_path_buf(),
            ns,
            devices,
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
        crate::log::line(
            "sandbox",
            "info",
            &format!("a keeper for {}: pid {}, {}", root.display(), k.pid, k.said()),
        );
        Ok(k)
    }

    /// Its tier and namespaces in words (`:procs`, the log): `A+ (user net ipc pid mnt)`, and why its network
    /// is this machine's where its policy opens one (D-a).
    pub fn said(&self) -> String {
        let served = super::supervisor::unavailable().is_none();
        let tier = super::tier::Tier::of(self.ns, served).said();
        match self.ns.user && !self.ns.net {
            true => {
                format!("{tier} ({}; network shared: its policy opens it, until routing)", self.ns.said())
            }
            false => format!("{tier} ({})", self.ns.said()),
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
        s.gone = Some(why);
        drop(s);
        self.changed.notify_all();
        for (pid, pidfd) in &runs {
            kill_run(*pid, pidfd);
        }
        if !asked {
            lock(&DEATHS).push((self.root.clone(), Instant::now()));
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
        if let Err(e) = channel::send(self.chan.as_fd(), &Msg::Run(req), fds) {
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
    pub pidfd: Arc<OwnedFd>,
}

impl Keeper {
    /// Starts `cmd` — its program, arguments, environment and folder, as `spawn::setsid` takes them: stdin
    /// `/dev/null`, stdout and stderr pipes — through the keeper, confined to the Landlock ruleset `rules`, its
    /// process named `comm` before it confines itself (`landlock::thread_name` — what Landlock's audit names
    /// beside its pid: the caller tells its watch first, `sandbox::spawn`). Its sockets go as `net` says — the
    /// answer `rules` were made for (`linux::confine`'s `Confine.net`): `Own`, its internet sockets are its own
    /// (`seccomp::Mode::OwnNetwork`); `Plain`, it has none; `Served` is `start_served`'s.
    pub fn start(
        &self,
        cmd: &std::process::Command,
        comm: &str,
        rules: BorrowedFd,
        net: RunNet,
    ) -> io::Result<Started> {
        if net == RunNet::Served {
            return Err(io::Error::other("a served run starts with its supervisor (start_served)"));
        }
        self.start_with(cmd, comm, rules, net).map(|(s, _)| s)
    }

    /// `start`, served or not; with a handoff socket when served or held (a keeper in a PID namespace — its
    /// run waits for tarae to know its pid): tarae's end of it comes back.
    fn start_with(
        &self,
        cmd: &std::process::Command,
        comm: &str,
        rules: BorrowedFd,
        net: RunNet,
    ) -> io::Result<(Started, Option<Arc<OwnedFd>>)> {
        use std::os::unix::ffi::OsStrExt;
        // (its own sockets only inside the network namespace that holds them to `lo`)
        if net == RunNet::Own && self.ns.run_net(false) != RunNet::Own {
            return Err(io::Error::other("a run's own network, but its keeper has no network namespace"));
        }
        let served = net == RunNet::Served;
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
            served,
            held: self.ns.pid,
            own_net: net == RunNet::Own,
        };
        let stdin = open_null(libc::O_RDONLY)?;
        let (out_r, out_w) = io::pipe()?;
        let (err_r, err_w) = io::pipe()?;
        let handoff = match served || req.held {
            true => Some(stream_pair()?),
            false => None,
        };
        let mut fds = vec![rules, stdin.as_fd(), out_w.as_fd(), err_w.as_fd()];
        fds.extend(handoff.as_ref().map(|(_, theirs)| theirs.as_fd()));
        let ours = handoff.as_ref().map(|(ours, _)| ours.clone());
        let hold = ours.clone().filter(|_| req.held);
        let (pid, pidfd) = self.run(req, &fds, hold)?;
        let spawned = crate::spawn::Spawned { pid, stdout: Some(out_r), stderr: Some(err_r) };
        Ok((Started { spawned, pidfd }, ours))
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
    /// fails at its handoff.
    pub fn start_served(
        &self,
        cmd: &std::process::Command,
        comm: &str,
        rules: BorrowedFd,
        policy: Arc<super::Policy>,
        refused: std::sync::mpsc::Sender<super::Denial>,
    ) -> io::Result<Started> {
        let to = super::supervisor::serve(policy, refused)?;
        let (started, ours) = self.start_with(cmd, comm, rules, RunNet::Served)?;
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
}

/// `struct ifreq` as `SIOCGIFFLAGS`/`SIOCSIFFLAGS` take it: a name, the flags, the rest of its 40 bytes.
#[repr(C)]
struct IfFlags {
    name: [u8; 16],
    flags: libc::c_short,
    rest: [u8; 22],
}

impl Birth {
    fn new() -> io::Result<Birth> {
        let pipe = || -> io::Result<(OwnedFd, OwnedFd)> {
            let (r, w) = io::pipe()?;
            Ok((high(r.into())?, high(w.into())?))
        };
        let last_cap = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(63);
        Ok(Birth { go: pipe()?, told: pipe()?, last_cap })
    }

    /// tarae's ends, the child's closed: the go pipe's writer, the birth pipe's reader.
    fn parents(self) -> (OwnedFd, OwnedFd) {
        (self.go.1, self.told.0)
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
                libc::close(s);
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
/// (another name or a second one is left out, said in the log).
fn said_ready(chan: BorrowedFd, told: OwnedFd) -> io::Result<Vec<(&'static str, OwnedFd)>> {
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
            if n == 5 {
                let step = BirthStep::from_code(b[0]).unwrap_or(BirthStep::Exec);
                let errno = i32::from_le_bytes([b[1], b[2], b[3], b[4]]);
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
                Ok(Some((Ok(Msg::Run(run)), fds))) => k.start(run, fds),
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

    /// Forks run `run` (its child: `child`), then says `Started` with its pidfd — or `Failed`.
    fn start(&mut self, run: channel::Run, fds: Vec<OwnedFd>) {
        let id = run.id;
        let failed = |k: &Kept, step, errno| {
            let _ = k.say(&Msg::Failed { id, errno, step }, &[]);
        };
        let mut pipe = [0; 2];
        // SAFETY: a two-int array for the call to fill.
        if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return failed(self, Step::Fork, errno());
        }
        // SAFETY: both just made, ours.
        let (error_r, error_w) = unsafe { (OwnedFd::from_raw_fd(pipe[0]), OwnedFd::from_raw_fd(pipe[1])) };
        // SAFETY: the keeper is single-threaded (in tarae's binary; a test's keeper has libtest's idle main
        // thread beside it), so the child may run ordinary code before its exec.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            child(&run, &fds, self.chan.as_raw_fd(), error_w.as_raw_fd());
        }
        drop(error_w);
        drop(fds);
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
        self.children.insert(id, Child { pid, error: Some(error_r) });
        let _ = self.say(&Msg::Started { id, pid: pid as u32 }, &[pidfd.as_fd()]);
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

/// A run's child, after the keeper's fork: its channel closed, dumpable again (finding 1: tarae takes its
/// listener with `pidfd_getfd`, which a non-dumpable process refuses without a namespace of tarae's), a session
/// of its own, its stdio, its folder, its name (what Landlock's audit names), every signal unblocked, its
/// Landlock rules, its seccomp filter — served, its listener's number handed to tarae and one byte back; held,
/// one byte once tarae knows its pid — then the exec. A failure on the way is written on `error` (step, errno)
/// and in its stderr, and it exits 127.
fn child(run: &channel::Run, fds: &[OwnedFd], chan: RawFd, error: RawFd) -> ! {
    let fail = |step: Step, errno: i32| -> ! {
        let mut b = [step.code(); 5];
        b[1..].copy_from_slice(&errno.to_le_bytes());
        let line = format!(
            "tarae's sandbox couldn't start it — {}: {}\n",
            step.said(),
            io::Error::from_raw_os_error(errno)
        );
        // SAFETY: writes of buffers we own, then the end of this process.
        unsafe {
            libc::write(error, b.as_ptr().cast(), b.len());
            libc::write(2, line.as_ptr().cast(), line.len());
            libc::_exit(127)
        }
    };
    let c = |s: &[u8]| CString::new(s).unwrap_or_default();
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
        if libc::chdir(c(&run.cwd).as_ptr()) != 0 {
            fail(Step::Chdir, errno());
        }
        libc::prctl(libc::PR_SET_NAME, c(&run.comm).as_ptr());
        let mut none: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut none);
        libc::pthread_sigmask(libc::SIG_SETMASK, &none, std::ptr::null_mut());
    }
    if let Err(e) = crate::landlock::restrict_self(ruleset.as_fd()) {
        fail(Step::Landlock, e.raw_os_error().unwrap_or(libc::EIO));
    }
    let mode = match (run.served, run.own_net) {
        (true, _) => crate::seccomp::Mode::Served,
        (false, true) => crate::seccomp::Mode::OwnNetwork,
        (false, false) => crate::seccomp::Mode::Plain,
    };
    let listener = match crate::seccomp::apply(mode) {
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
    let bytes = run.served as usize + run.held as usize;
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
    let argv: Vec<CString> = run.argv.iter().map(|a| c(a)).collect();
    let envp: Vec<CString> = run.envp.iter().map(|e| c(e)).collect();
    let ptrs = |v: &[CString]| -> Vec<*const libc::c_char> {
        v.iter().map(|s| s.as_ptr()).chain(std::iter::once(std::ptr::null())).collect()
    };
    let (argv_p, envp_p) = (ptrs(&argv), ptrs(&envp));
    // SAFETY: the program, argv and envp are NUL-terminated arrays of strings held above.
    unsafe { libc::execve(c(&run.program).as_ptr(), argv_p.as_ptr(), envp_p.as_ptr()) };
    fail(Step::Exec, errno())
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
        assert_eq!(said, "served refused · told network-outbound remote:127.0.0.1:9");
    }

    /// `tarae_takes_the_listener_of_a_keeper_child`'s scene, as an ordinary user: a project whose policy opens
    /// one port of a listener here, a served run of a keeper's that connects to it and to port 9. Prints
    /// `scene: <what the run said> · told <the refusal's op and target>`.
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
            if ! { : 4<>/dev/tcp/127.0.0.1/9; } 2>/dev/null; then printf ' refused'; fi";
        cmd.args(["-c", script]).env("P", port.to_string()).current_dir(&root);
        let s = k.start_served(&cmd, &crate::landlock::thread_name(), c.rules_fd(), Arc::new(p), tx).unwrap();
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
        let s = k.start(&cmd, &crate::landlock::thread_name(), rules, net).unwrap();
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
            other => format!("no helper {other}"),
        };
        println!("helper: {said}");
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

    /// D-a: a policy that opens a port, this machine's ports or the network gets no network namespace until
    /// routing (its runs' sockets would be the session's, where nothing listens and nothing goes out), and the
    /// keeper says so; one that opens nothing gets the session's own, not this process's. A keeper of the other
    /// kind is superseded.
    #[test]
    fn an_open_policys_keeper_has_no_netns_yet() {
        use super::super::tier;
        if !namespaced("an_open_policys_keeper_has_no_netns_yet") {
            return;
        }
        let (closed, _, r1) = project_in("netns-closed", Vec::new(), None, &[]);
        let (open, _, r2) = project_in("netns-open", vec![9], None, &[]);
        let (closed_ns, open_ns) = (tier::for_policy(&closed), tier::for_policy(&open));
        assert_eq!((closed_ns.net, open_ns.net), (ns().net, false));
        assert!(tier::opens(&super::super::Policy { localhost: true, ..closed.clone() }));
        assert!(tier::opens(&super::super::Policy { network: true, ..closed.clone() }));
        let root = std::env::temp_dir().join(format!("tarae-keeper-netns-{}", std::process::id()));
        let net = |pid: u32| std::fs::read_link(format!("/proc/{pid}/ns/net")).unwrap();
        let a = ready(&root, Some("closed"), closed_ns).unwrap();
        assert_ne!(net(a.pid), net(std::process::id()), "the session's own");
        let b = ready(&root, Some("open"), open_ns).unwrap();
        assert_ne!(a.pid, b.pid, "another keeper");
        assert_eq!(net(b.pid), net(std::process::id()), "this machine's, until routing");
        assert!(b.said().contains("network shared"), "{}", b.said());
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
        for (net, handled) in [(RunNet::Plain, nets), (RunNet::Served, nets), (RunNet::Own, false)] {
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
        assert_eq!((c.net, c.handled_net() != 0), (RunNet::Served, nets), "served");
        let (_, c, root) = project_served("netrights-own", vec![], Some(&k), &[], unserved());
        assert_eq!((c.net, c.handled_net()), (RunNet::Own, 0), "plain, in its own network namespace");
        let said = helper_through(&k, c.rules_fd(), c.net, "loopback", &[], Some(&root));
        assert!(said.starts_with("own=ok "), "its own sockets, under its own rules: {said}");
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }
}
