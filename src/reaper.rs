//! tarae as the last parent of what its runs leave running (Linux). A process a run starts that leaves it — a
//! build tool's daemon forking away from its client (Gradle's, and the Kotlin daemon Gradle's starts) — is
//! re-parented to the nearest *child subreaper* above it instead of init (`PR_SET_CHILD_SUBREAPER`, `at_start`).
//! With tarae one, such a process stays tarae's descendant, so under Yama `ptrace_scope` 1 (Ubuntu's, Debian's
//! default) the supervisor can still take its sockets (`pidfd_getfd` needs a descendant) and serve its connects
//! after its run has ended — the supervisor serves until no process of the run is left
//! (`sandbox::supervisor::serve`). Measured: without it a reused Gradle daemon's test worker is refused
//! (`target/sb/subreaper.md`).
//!
//! What comes with it: tarae is the parent of every orphan of every process below it (the terminal's `nohup`s
//! too), and reaps them — on `procs`' look (`Reaper::round`). **Only those it adopted, never one it started**:
//! a child tarae started belongs to whoever holds its `Child` (an LSP client, the spare Claude, …), who may
//! wait or kill it much later — reaped under it, its pid could be reused and that kill reach another process.
//! Adopted = in a session neither tarae's nor its own (a child tarae starts is in one of those two), or seen
//! under another parent before (`Reaper::adopted`); the rest is left a zombie, never a wrong kill. One adopted
//! is held by a pidfd and reaped when two looks `GRACE` apart found it still unreaped.
//! Since each session has a keeper (`sandbox::keeper`), what a sandboxed run leaves is the keeper's — it is
//! their subreaper, and ends them with itself. tarae is the last parent of what a keeper that died left: told by
//! its environment (`sandbox::MARK` = the run's tag — kept by Gradle's daemon and passed to the Kotlin daemon,
//! measured) and ended (`sandbox::end_left_by_tag`, `sandbox::end_leftovers`, through `end`). Elsewhere (macOS)
//! none of this exists: no subreaper, nothing to do.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use crate::procs::Os;

/// How long an adopted zombie is left before it's reaped (a look that finds it and a later one).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub const GRACE: Duration = Duration::from_secs(10);

/// Makes tarae a child subreaper (process-wide, kept across `:restart`'s exec). A failure is logged: what runs
/// leave then goes to init, as before — served only where tarae may take a non-descendant's sockets.
pub fn at_start() {
    #[cfg(target_os = "linux")]
    // SAFETY: a plain prctl with integer arguments.
    if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
        let e = std::io::Error::last_os_error();
        crate::log::line("reaper", "warn", &format!("not a child subreaper: {e}"));
    }
}

/// Zombie children held for reaping, and what tells an adopted one.
#[derive(Default)]
pub struct Reaper {
    #[cfg(target_os = "linux")]
    held: Vec<Held>,
    /// The processes below tarae last seen under another parent (pid, when it started) — found tarae's
    /// child later, it was adopted.
    under: HashSet<(u32, u64)>,
}

/// One zombie child: its pidfd (that process — a reused pid isn't it), and when it was first found.
#[cfg(target_os = "linux")]
struct Held {
    pid: u32,
    started: u64,
    fd: std::os::fd::OwnedFd,
    since: Instant,
}

impl Reaper {
    /// The zombie children of `me` in `tree` (a look at tarae's tree, `me` in it) that tarae adopted (pid,
    /// when it started): in a session neither `me`'s nor its own — a child `me` starts is in one of the two,
    /// it can only `setsid` — or seen under another parent at a look before. A zombie child of tarae's own
    /// session, or leading its own, never seen under another parent can't be told from one tarae started: not
    /// adopted (left a zombie). Remembers who's under another parent now, for the next look.
    pub fn adopted(&mut self, tree: &[&Os], me: u32) -> Vec<(u32, u64)> {
        let mine = tree.iter().find(|o| o.pid == me).and_then(|o| o.sid);
        let out = tree
            .iter()
            .filter(|o| o.ppid == me && o.zombie)
            .filter(|o| {
                // (tarae's own session unknown: nothing is foreign)
                let foreign = mine.is_some_and(|m| o.sid.is_some_and(|s| s != m && s != o.pid));
                foreign || self.under.contains(&(o.pid, o.started))
            })
            .map(|o| (o.pid, o.started))
            .collect();
        // (one adopted stays known as such while it's there)
        let under = tree
            .iter()
            .filter(|o| o.pid != me && (o.ppid != me || self.under.contains(&(o.pid, o.started))))
            .map(|o| (o.pid, o.started))
            .collect();
        self.under = under;
        out
    }

    /// One round, after a look at tarae's tree (`me` in it): the zombie children tarae adopted (`adopted`) are
    /// held, and one found again `GRACE` after it was first (so in a later look) is reaped through its pidfd
    /// (`waitid(P_PIDFD)` — a waiter of its own already took it: nothing). The pids reaped.
    pub fn round(&mut self, tree: &[&Os], me: u32, now: Instant) -> Vec<u32> {
        let zombies = self.adopted(tree, me);
        #[cfg(target_os = "linux")]
        {
            use std::os::fd::{AsRawFd, FromRawFd};
            // (one not found again was reaped by its own waiter)
            self.held.retain(|h| zombies.contains(&(h.pid, h.started)));
            for &(pid, started) in &zombies {
                if self.held.iter().any(|h| h.pid == pid && h.started == started) {
                    continue;
                }
                // SAFETY: pidfd_open of a pid; a descriptor it gives is ours.
                let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
                if fd >= 0 {
                    // SAFETY: just made.
                    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) };
                    self.held.push(Held { pid, started, fd, since: now });
                }
            }
            let mut reaped = Vec::new();
            self.held.retain(|h| {
                if now.saturating_duration_since(h.since) < GRACE {
                    return true;
                }
                if waited(h.fd.as_raw_fd(), false) {
                    reaped.push(h.pid);
                }
                false
            });
            reaped
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (zombies, now);
            Vec::new()
        }
    }
}

/// Reaps the child behind pidfd `fd` — `block`: waits for it to end. Whether it was reaped here.
#[cfg(target_os = "linux")]
fn waited(fd: std::os::fd::RawFd, block: bool) -> bool {
    // SAFETY: a zeroed siginfo the call fills; the pidfd is the caller's.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | if block { 0 } else { libc::WNOHANG };
    // SAFETY: as above.
    let r = unsafe { libc::waitid(libc::P_PIDFD, fd as libc::id_t, &mut info, flags) };
    // (WNOHANG with nothing ended yet: 0 and no pid filled in)
    // SAFETY: filled in by a successful waitid.
    r == 0 && unsafe { info.si_pid() } != 0
}

/// The tag `pid`'s environment carries (`sandbox::MARK`: the run that started it — or, a daemon that took a
/// later build's environment, the run its children came from) — None: none, or not ours to read.
pub fn tag_of(pid: u32) -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
        let key = format!("{}=", crate::sandbox::MARK);
        env.split(|&b| b == 0)
            .find_map(|kv| kv.strip_prefix(key.as_bytes()))
            .map(|v| String::from_utf8_lossy(v).into_owned())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Ends processes: TERM to each (its whole group when it leads one — a daemon that called `setsid` takes the
/// workers it started with it), KILL to what's left after `wait`, then reaped (tarae's children). Through
/// pidfds taken first, so a pid reused meanwhile is never signalled; never an unbounded wait. The pids that
/// ended.
pub fn end(pids: &[u32], wait: Duration) -> Vec<u32> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let send = |fd: &OwnedFd, sig: i32| {
            // SAFETY: pidfd_send_signal on a pidfd we hold, no siginfo.
            unsafe { libc::syscall(libc::SYS_pidfd_send_signal, fd.as_raw_fd(), sig, 0usize, 0) == 0 }
        };
        let held: Vec<(u32, OwnedFd)> = pids
            .iter()
            .filter_map(|&pid| {
                // SAFETY: as in `Reaper::round`.
                let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid as libc::pid_t, 0) };
                // SAFETY: just made.
                (fd >= 0).then(|| (pid, unsafe { OwnedFd::from_raw_fd(fd as i32) }))
            })
            .collect();
        for (pid, fd) in &held {
            // SAFETY: getpgid of a pid we hold a pidfd on (alive, or a zombie of ours: not reused).
            if unsafe { libc::getpgid(*pid as libc::pid_t) } == *pid as libc::pid_t {
                // SAFETY: the group it leads.
                unsafe { libc::kill(-(*pid as libc::pid_t), libc::SIGTERM) };
            } else {
                send(fd, libc::SIGTERM);
            }
        }
        let dead = |fd: &OwnedFd| {
            let mut p = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one pollfd; a pidfd is readable once its process has ended.
            unsafe { libc::poll(&mut p, 1, 0) == 1 }
        };
        let all_dead = |until: Instant| {
            while Instant::now() < until && !held.iter().all(|(_, fd)| dead(fd)) {
                std::thread::sleep(Duration::from_millis(20));
            }
        };
        all_dead(Instant::now() + wait);
        for (_, fd) in &held {
            if !dead(fd) {
                send(fd, libc::SIGKILL);
            }
        }
        // (KILL ends it at once — but never waited on unbounded: one somehow still there is left)
        all_dead(Instant::now() + Duration::from_millis(500));
        held.iter()
            .filter(|(_, fd)| {
                let ended = dead(fd);
                // (reaped here when it's tarae's child; another's parent reaps its own)
                waited(fd.as_raw_fd(), false);
                ended
            })
            .map(|(pid, _)| *pid)
            .collect()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pids, wait);
        Vec::new()
    }
}
