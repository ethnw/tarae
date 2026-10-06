//! A session's keeper (`docs/design/sandbox-keeper.md` §The keeper) — here without namespaces (tiers C/D; the
//! namespaces are step 3): a small process per session, `tarae --sandbox-keeper`, that forks each sandboxed run
//! of its session, reaps it and says how it ended, and is the child subreaper of what the runs leave — so what a
//! run leaves running is the session's process tree, not a tag (`env -i` can't shed it), and ends with it.
//!
//! tarae's side: `ready` (on the worker that makes a run's policy — `sandbox::confined`) finds the session's
//! keeper or makes one (`born`: `clone3(CLONE_PIDFD)`, raw calls until the exec), `Keeper::run` asks it for a
//! run and waits for `Started` (the main loop's wait: at most `STARTED_WITHIN`, as it waited for `posix_spawn`
//! before), and the two places every run is waited on and stopped — `spawn::wait` and `dap::term_group` — ask
//! here first (`wait`, `signal`): a keeper's run isn't tarae's child, and only its reaper can signal it without
//! a reused pid's race. Its channel is `channel.rs`'s protocol; a thread reads it, another waits on its pidfd.
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
/// D11: a session's keeper that ended this many times within `DEATHS_WITHIN` isn't made again.
const DEATHS_MAX: usize = 2;
const DEATHS_WITHIN: Duration = Duration::from_secs(600);

// ── tarae's side ─────────────────────────────────────────────────────────────

/// A session's keeper, as tarae holds it.
pub struct Keeper {
    pub pid: u32,
    pub root: PathBuf,
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
}

#[derive(Default)]
struct Slot {
    /// `Started`'s pid and pidfd — or why it didn't start.
    started: Option<Result<(u32, Arc<OwnedFd>), String>>,
    /// Its `wait` status — or why it has none (the keeper ended first).
    exited: Option<Result<i32, String>>,
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

/// The keeper of the session at `root` for a policy keyed `key` (None: any — a probe's): the one there, else a
/// new one. One of another key is superseded: what its runs left ends now, and it ends after its last run (D14).
/// Two deaths within `DEATHS_WITHIN` and none is made (D11). Blocks (a worker's).
pub fn ready(root: &Path, key: Option<&str>) -> Result<Arc<Keeper>, String> {
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
            (None, Some(new)) => {
                s.key = Some(new.to_string());
                return Ok(k.clone());
            }
            (Some(was), Some(new)) if was == new => return Ok(k.clone()),
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
    let k = Keeper::born(root, key).map_err(|e| format!("the session's keeper couldn't start: {e}"))?;
    lock(&KEEPERS).push(k.clone());
    Ok(k)
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
    /// A new keeper for the session at `root`: `clone3` (its pidfd from the start), the child's raw calls —
    /// stdio to `/dev/null`, its channel to fd 3, `SIGCHLD` blocked (so through the exec: no thread of the new
    /// process takes it before its `signalfd` does) — then the exec of tarae's own binary, with an empty
    /// environment. Ready once it says so (`READY_WITHIN`).
    fn born(root: &Path, key: Option<&str>) -> io::Result<Arc<Keeper>> {
        let exe = exe()?;
        let (ours, theirs) = seqpacket()?;
        let null = open_null(libc::O_RDWR)?;
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
        let pid = unsafe { clone_pidfd(&mut pidfd) };
        if pid == 0 {
            // SAFETY: as above — async-signal-safe calls only.
            unsafe {
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
                libc::_exit(127);
            }
        }
        if pid < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the pidfd `clone3` made for us.
        let pidfd = unsafe { OwnedFd::from_raw_fd(pidfd) };
        drop(theirs);
        let ready = (|| {
            if !readable(ours.as_fd(), READY_WITHIN)? {
                return Err(io::Error::other("it didn't say it was there"));
            }
            match channel::recv(ours.as_fd(), &mut Vec::new())? {
                Some((Ok(Msg::Ready { .. }), _)) => Ok(()),
                Some((msg, _)) => Err(io::Error::other(format!("it said {msg:?} first"))),
                None => Err(io::Error::other("it ended at once")),
            }
        })();
        let k = Arc::new(Keeper {
            pid: pid as u32,
            root: root.to_path_buf(),
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
        crate::log::line("sandbox", "info", &format!("a keeper for {}: pid {}", root.display(), k.pid));
        Ok(k)
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
            Msg::Started { id, pid } => match (s.runs.get_mut(&id), fds.pop()) {
                (Some(slot), Some(pidfd)) => {
                    slot.started = Some(Ok((pid, Arc::new(pidfd))));
                    lock(&RUNS).push((pid, self.clone(), id));
                }
                // (one whose start tarae gave up on: it goes)
                _ => {
                    let _ = channel::send(self.chan.as_fd(), &Msg::Signal { id, sig: libc::SIGKILL }, &[]);
                }
            },
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
                if let Some(slot) = s.runs.get_mut(&id) {
                    slot.exited = Some(Ok(status));
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

    /// Asks for run `req` with its descriptors (the ruleset, stdin, stdout, stderr — and served, the handoff
    /// socket's child end) and waits for its start (`STARTED_WITHIN`): its pid and pidfd.
    pub fn run(&self, mut req: channel::Run, fds: &[BorrowedFd]) -> io::Result<(u32, Arc<OwnedFd>)> {
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
            s.runs.insert(id, Slot::default());
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
    /// beside its pid: the caller tells its watch first, `sandbox::spawn`); with `handoff` (the child's end of a
    /// socket pair), served: its listener's number comes back on it.
    pub fn start(
        &self,
        cmd: &std::process::Command,
        comm: &str,
        rules: BorrowedFd,
        handoff: Option<&OwnedFd>,
    ) -> io::Result<Started> {
        use std::os::unix::ffi::OsStrExt;
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
            served: handoff.is_some(),
        };
        let stdin = open_null(libc::O_RDONLY)?;
        let (out_r, out_w) = io::pipe()?;
        let (err_r, err_w) = io::pipe()?;
        let mut fds = vec![rules, stdin.as_fd(), out_w.as_fd(), err_w.as_fd()];
        fds.extend(handoff.map(|h| h.as_fd()));
        let (pid, pidfd) = self.run(req, &fds)?;
        let spawned = crate::spawn::Spawned { pid, stdout: Some(out_r), stderr: Some(err_r) };
        Ok(Started { spawned, pidfd })
    }
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
        let mut fds = [0; 2];
        // SAFETY: a two-int array for the call to fill.
        if unsafe {
            libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr())
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: both just made, ours.
        let (ours, theirs) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let started = self.start(cmd, comm, rules, Some(&theirs))?;
        drop(theirs);
        let pidfd = started.pidfd.clone();
        crate::event::thread("tarae-handoff", move || {
            let mut n = [0u8; 4];
            // SAFETY: a read into a buffer of its size, a write of one byte, on our end of the pair.
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
                libc::write(ours.as_raw_fd(), [1u8].as_ptr().cast(), 1);
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

/// A child with its pidfd from the start (in `pidfd`): `clone3(CLONE_PIDFD)` — or, where a filter answers
/// `clone3` with `ENOSYS` (Docker's default profile does, so that libcs fall back), `clone(2)` with the same
/// flag (Linux 5.2). 0 in the child, its pid in the parent, -1 and errno on failure.
///
/// # Safety
/// The child is a copy of the calling (multithreaded) process: until its exec it may make raw calls only.
unsafe fn clone_pidfd(pidfd: &mut libc::c_int) -> libc::c_long {
    let args = CloneArgs {
        flags: libc::CLONE_PIDFD as u64,
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
        let flags = (libc::CLONE_PIDFD | libc::SIGCHLD) as libc::c_ulong;
        libc::syscall(libc::SYS_clone, flags, 0usize, pidfd as *mut libc::c_int, 0usize, 0usize)
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
    if k.say(&Msg::Ready { pid: me }, &[]).is_err() {
        std::process::exit(1);
    }
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

/// Every capability gone, for good: the bounding set (an exec as root would take them back from it — this
/// needs `CAP_SETPCAP`, which only a root keeper has), the ambient set, then effective, permitted and
/// inheritable. tarae run as a user has none to drop; as root in a container its runs would hold root's.
fn drop_capabilities() {
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    let h = Header { version: 0x2008_0522, pid: 0 };
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
/// Landlock rules, its seccomp filter — served, its listener's number handed to tarae and one byte back — then
/// the exec. A failure on the way is written on `error` (step, errno) and in its stderr, and it exits 127.
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
    let mode = if run.served { crate::seccomp::Mode::Served } else { crate::seccomp::Mode::Plain };
    let listener = match crate::seccomp::apply(mode) {
        Ok(l) => l,
        Err(e) => fail(Step::Seccomp, e.raw_os_error().unwrap_or(libc::EIO)),
    };
    if let (Some(l), Some(handoff)) = (&listener, fds.get(4)) {
        let n = (l.as_raw_fd() as u32).to_le_bytes();
        let mut back = [0u8];
        // SAFETY: a write and a read of buffers we own on the handoff socket (neither is a call the filter
        // hands to tarae: no address).
        let ok = unsafe {
            libc::write(handoff.as_raw_fd(), n.as_ptr().cast(), n.len()) == 4
                && libc::read(handoff.as_raw_fd(), back.as_mut_ptr().cast(), 1) == 1
        };
        if !ok {
            fail(Step::Handoff, errno());
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

    /// A keeper of a test's own, for a root no other test uses.
    fn keeper(test: &str) -> Arc<Keeper> {
        let root = std::env::temp_dir().join(format!("tarae-keeper-{}-{test}", std::process::id()));
        Keeper::born(&root, None).expect("a keeper starts")
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
        let s = k.start(&cmd, &crate::landlock::thread_name(), rules.fd(), None).unwrap();
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
        let s = k.start(&cmd, &crate::landlock::thread_name(), rules.fd(), None).unwrap();
        let mut lines = std::io::BufReader::new(s.spawned.stdout.unwrap()).lines();
        let child: i32 = lines.next().unwrap().unwrap().trim().parse().unwrap();
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
        let c = super::super::linux::confine(&p, abi, Err("not watched".into()), false).unwrap();
        let k = keeper("served");
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
        let sleep: u32 = out.trim().parse().unwrap();
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
        let k = ready(&root, Some("a key")).unwrap();
        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("30");
        let run = k.start(&cmd, &crate::landlock::thread_name(), rules.fd(), None).unwrap().spawned.pid;
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
        let again = ready(&root, Some("a key")).expect("made again");
        assert_ne!(again.pid, k.pid);
        let (_, status) = sh_through(&again, &rules, "exit 0");
        assert!(status.success(), "a run of the new one goes");
        kill(&again);
        let refused = ready(&root, Some("a key")).expect_err("not made a third time");
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
        let birth = std::thread::spawn(move || ready(&r, Some("a key")));
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
            let s = k.start(&cmd, &crate::landlock::thread_name(), rules.fd(), None).unwrap();
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
        let old = ready(&root, Some("key 1")).unwrap();
        let (out, status) = sh_through(&old, &rules, "setsid sleep 30 >/dev/null 2>&1 & echo $!");
        assert!(status.success(), "{out}");
        let left: u32 = out.trim().parse().unwrap();
        let end = Instant::now() + Duration::from_secs(3);
        while crate::procs::read(left).map(|o| o.ppid) != Some(old.pid) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(old.leftovers(), [left], "left by its run");
        let mut cmd = std::process::Command::new("sh");
        cmd.args(["-c", "echo going; exec sleep 30"]);
        let going = old.start(&cmd, &crate::landlock::thread_name(), rules.fd(), None).unwrap();
        let mut lines = std::io::BufReader::new(going.spawned.stdout.unwrap()).lines();
        assert_eq!(lines.next().unwrap().unwrap(), "going");
        let new = ready(&root, Some("key 2")).unwrap();
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
        let refused = old.start(&cmd, &crate::landlock::thread_name(), rules.fd(), None).err();
        assert!(refused.is_some_and(|e| e.to_string().contains("replaced")), "no run more");
        crate::dap::term_group(going.spawned.pid);
        crate::spawn::wait(going.spawned.pid).unwrap();
        assert!(gone_within(&old, END_WITHIN), "ended after its last run");
        let third = ready(&root, Some("key 3")).unwrap();
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
        let born = match Keeper::born(Path::new("/tarae-keeper-clone3"), None) {
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
        let k = ready(&root, Some("a key")).unwrap();
        let ed = crate::editor::Editor::new(crate::config::Config::default());
        let known = ed.procs_known();
        let line = known.iter().find(|n| n.pid == k.pid).expect("a line for it");
        assert_eq!(line.title, "sandbox keeper");
        assert!(line.what.starts_with("the session's keeper · "), "{}", line.what);
        crate::sandbox::end_leftovers(Some(&root));
        let known = ed.procs_known();
        assert!(!known.iter().any(|n| n.pid == k.pid), "gone once it ended");
    }

    /// A project's policy with these ports open, and its Landlock rules (`linux::confine`): (the policy, the
    /// rules, the project).
    fn project(name: &str, ports: Vec<u16>) -> (super::super::Policy, super::super::linux::Confine, PathBuf) {
        let root = std::env::temp_dir().join(format!("tarae-keeper-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let env = super::super::Env {
            root: root.clone(),
            tmp: Some(super::super::profile_path(&std::env::temp_dir())),
            ports,
            run_env: super::super::run_env_of(&super::super::Here::now(), &crate::llm::LlmConfig::default()),
            ..Default::default()
        };
        let p = super::super::Policy::for_project(&env);
        let abi = crate::landlock::abi().unwrap();
        let c = super::super::linux::confine(&p, abi, Err("not watched".into()), false).unwrap();
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

    /// M1 (`docs/design/sandbox-keeper.md` §Measured): a sandboxed `true`, from asking to its exit status,
    /// median of 50 — through the keeper, plain and served (the confining thread and `posix_spawn` it replaced
    /// were measured beside it before they went: the doc has both).
    #[test]
    fn perf_a_run_through_the_keeper() {
        if crate::test_results::unoptimized("sandbox::keeper::tests::perf_a_run_through_the_keeper") {
            return;
        }
        if open_rules("perf_a_run_through_the_keeper").is_none() {
            return;
        }
        let (p, c, root) = project("perf", Vec::new());
        let k = keeper("perf");
        let mut cmd = std::process::Command::new("true");
        cmd.current_dir(&root);
        let served = super::super::supervisor::unavailable().is_none();
        let policy = Arc::new(p);
        let (tx, _rx) = std::sync::mpsc::channel();
        let keeper_plain = median_ms(50, || {
            let s = k.start(&cmd, &crate::landlock::thread_name(), c.rules_fd(), None).unwrap();
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
        let line = format!(
            "M1, `true` asked to its status, median of 50 (this process {}): keeper plain {keeper_plain:.2} ms, \
             served {}\n",
            rss(),
            ms(keeper_served),
        );
        let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
        k.end();
        std::fs::remove_dir_all(&root).ok();
    }

    /// A keeper's birth to `Ready`, median of 20, and an idle keeper's own Rss and Pss (its `Usage`) with 1, 4
    /// and 8 at once (M3).
    #[test]
    fn perf_a_keepers_birth_and_memory() {
        if crate::test_results::unoptimized("sandbox::keeper::tests::perf_a_keepers_birth_and_memory") {
            return;
        }
        let root = std::env::temp_dir().join(format!("tarae-keeper-birth-{}", std::process::id()));
        let mut born = Vec::new();
        let birth = median_ms(20, || born.push(Keeper::born(&root, None).unwrap()));
        for k in born.drain(..) {
            k.end();
        }
        let mut said = Vec::new();
        for n in [1, 4, 8] {
            let ks: Vec<Arc<Keeper>> = (0..n).map(|_| Keeper::born(&root, None).unwrap()).collect();
            std::thread::sleep(Duration::from_millis(200));
            let used: Vec<(u64, u64)> = ks.iter().filter_map(|k| k.usage()).collect();
            let (rss, pss) = used.iter().fold((0, 0), |a, u| (a.0 + u.0, a.1 + u.1));
            said.push(format!("{n}: Rss {rss} KB, Pss {pss} KB in all"));
            for k in ks {
                k.end();
            }
        }
        let line = format!(
            "a keeper's birth to Ready, median of 20 (this process {}): {birth:.2} ms · idle keepers' own use — {}\n",
            rss(),
            said.join(" · ")
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
        let k = keeper("dumpable");
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
        let exe = std::env::current_exe().unwrap();
        let out = std::process::Command::new(exe)
            .args(["--exact", &format!("sandbox::keeper::tests::{name}"), "--ignored", "--nocapture"])
            .args(["--test-threads=1"])
            .env(SCENE, "1")
            .envs(env.iter().copied())
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let err = String::from_utf8_lossy(&out.stderr);
        text.lines()
            .find_map(|l| l.split_once("scene: ").map(|(_, s)| s.to_string()))
            .unwrap_or_else(|| panic!("{name} said nothing:\n{text}\n{err}"))
    }

    /// As root (a container's tests), this process becomes `nobody` — every thread (glibc's `setresuid`), every
    /// capability gone: an ordinary user, as tarae runs. A scene's only: it changes the whole process.
    fn ordinary_user() {
        // SAFETY: plain calls on this process (a scene's own).
        unsafe {
            if libc::geteuid() == 0 {
                assert_eq!(libc::setgroups(0, std::ptr::null()), 0, "setgroups");
                assert_eq!(libc::setresgid(65534, 65534, 65534), 0, "setresgid");
                assert_eq!(libc::setresuid(65534, 65534, 65534), 0, "setresuid");
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
}
