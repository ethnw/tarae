//! tarae's own copy of every socket a run has, where it can't take a run's descriptors (6f′,
//! `target/sb/step6fprime-design.md`: `pidfd_getfd` refused — Docker's default profile). The supervisor makes every
//! socket the run gets (`seccomp::Mode::Copied`: its `socket`, `socketpair` and `accept`), keeps a copy here by
//! inode, and finds the run's descriptor `n` by the inode `/proc/<tid>/fd/<n>` names (M8: a readlink needs only
//! `PTRACE_MODE_READ`).
//!
//! A copy is a second reference to the run's socket: while tarae holds it, the run's `close()` sends no FIN and a
//! bound port stays bound (M8's caveat). So copies are collected (6f′ G2 (b)): while any is kept, the sockets the
//! processes below the run's keeper hold are read (`/proc/<pid>/fd`), and a copy none of them held in `STRIKES` scans
//! in a row goes.
//!
//! Never early (the review of 6f′):
//! - A socket a served `sendmsg` passed (`SCM_RIGHTS`) is held by no process while it waits in the receiver's queue,
//!   however long the receiver takes (Python's forkserver passes its sockets so). Its copy is kept while the socket
//!   it went on has anything unread (`in_flight`: that socket's `SIOCOUTQ` — what it sent that the peer hasn't
//!   read; the kernel frees it when the receiver reads the message, or drops it), up to `IN_FLIGHT_MAX`.
//! - A scan that can't read a process's descriptors (one that made itself non-dumpable — ssh-agent, gpg-agent — or
//!   whose children can't be listed) strikes nothing: its copies wait until it's gone or readable. A process that
//!   ended meanwhile (`ENOENT`) holds nothing.
//!
//! Scans are paced by what the run does: `INTERVAL` after a call of the run's tarae served, a copy kept, or a
//! scan that struck or dropped one; doubling to `QUIET_MAX` while nothing happens (measured: a scan of a Gradle
//! daemon's ≈ 450 descriptors took 6–11 ms — every `INTERVAL` that was 10–22% of a core, idle too). A close's FIN
//! comes two scans late — ≈ 100 ms while the run is busy, up to `QUIET_MAX` and one interval after a quiet stretch;
//! a port's release lazily: a bind that finds its port taken has the copies collected first (`collect_now`), then
//! tries again — for up to `RELEASE_WAIT` where a copy just went, as a process tarae is starting holds every one of
//! tarae's descriptors until its exec is through (`bind_again`).

use std::collections::{HashMap, HashSet};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// How soon the copies are collected again after something happened (`Copies::stir`, a copy kept, one struck).
pub const INTERVAL: Duration = Duration::from_millis(50);

/// The longest between two scans while nothing happens (doubling from `INTERVAL`): what a quiet run's copies cost,
/// a scan a second (`measure_copies_scan`, the design note).
pub const QUIET_MAX: Duration = Duration::from_secs(1);

/// Scans in a row that find no process holding a copy's socket before it goes.
pub const STRIKES: u8 = 2;

/// How long a bind that found its port taken waits for a copy's socket to let it go (`bind_again`). The socket goes
/// once the last reference to it does — and tarae's copy isn't always the last: a process tarae starts holds every
/// descriptor tarae has from its fork until its exec is through (closing them is part of the exec), copies too.
/// Measured under the test suite's load (2026-10-10): the port free 63 µs after the copy went.
pub const RELEASE_WAIT: Duration = Duration::from_millis(200);

/// The longest a passed socket's copy is kept for being in flight (`in_flight`) — past it, collected as any: the
/// socket it went on may have more unread than that message (a receiver reading slowly, or never).
pub const IN_FLIGHT_MAX: Duration = Duration::from_secs(30);

/// The copies of one run's sockets (its supervisor's), and how they're collected.
pub struct Copies {
    inner: Mutex<Inner>,
    /// The process whose descendants hold a run's sockets: its keeper (a run's processes are its children, and what
    /// one leaves is reparented to it, a child subreaper). Its other runs' processes are below it too: a socket one
    /// of them holds is kept — a copy kept longer, never one dropped early.
    tree: u32,
    /// A thread collects them every `INTERVAL` (tests that count the scans themselves have none).
    scanned: bool,
    /// What a scan reads: `holders` (tests stand in for it).
    look: Box<dyn Fn(u32) -> Look + Send + Sync>,
}

/// What one scan found below the tree.
#[derive(Default)]
pub struct Look {
    /// The socket inodes its processes hold.
    pub held: HashSet<u64>,
    /// Descriptors read.
    pub read: u64,
    /// A process there whose descriptors (or children) couldn't be read: it may hold any of them.
    pub unreadable: bool,
}

#[derive(Default)]
struct Inner {
    held: HashMap<u64, Copy>,
    /// Scans begun so far: a copy kept during a scan isn't struck by it (its holder may not have it yet).
    scans: u64,
    /// A thread collects them (none while there's nothing to collect).
    collecting: bool,
    /// Its next wait (`INTERVAL` … `QUIET_MAX`).
    interval: Duration,
    /// When a scan last let a copy go (the scanner's or `collect_now`'s).
    went: Option<Instant>,
}

struct Copy {
    fd: Arc<OwnedFd>,
    born: u64,
    unseen: u8,
    in_flight: Option<InFlight>,
}

/// A passed socket's way: the socket the message carrying it went on (tarae's own descriptor of it), and when.
struct InFlight {
    via: OwnedFd,
    since: Instant,
}

impl Copies {
    pub fn new(tree: u32) -> Arc<Copies> {
        Copies::made(tree, true, Box::new(holders))
    }

    fn made(tree: u32, scanned: bool, look: Box<dyn Fn(u32) -> Look + Send + Sync>) -> Arc<Copies> {
        let inner = Mutex::new(Inner { interval: INTERVAL, ..Inner::default() });
        Arc::new(Copies { inner, tree, scanned, look })
    }

    /// Tests: copies only `collect` collects — no thread of their own.
    #[cfg(test)]
    pub fn unscanned(tree: u32) -> Arc<Copies> {
        Copies::made(tree, false, Box::new(holders))
    }

    /// Tests: copies `collect` collects with `look` standing in for what's below the tree.
    #[cfg(test)]
    pub fn looking(look: impl Fn(u32) -> Look + Send + Sync + 'static) -> Arc<Copies> {
        Copies::made(0, false, Box::new(look))
    }

    /// Something happened (a call of the run's tarae served): the next scan comes after `INTERVAL`.
    pub fn stir(&self) {
        self.lock().interval = INTERVAL;
    }

    /// The wait before the next scan.
    pub fn interval(&self) -> Duration {
        self.lock().interval
    }

    /// After a scan: back to `INTERVAL` where it struck or dropped a copy (`changed`), else twice as long, up to
    /// `QUIET_MAX`.
    fn pace(&self, changed: bool) {
        let mut i = self.lock();
        i.interval = if changed { INTERVAL } else { (i.interval * 2).min(QUIET_MAX) };
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A copy of `fd` (the same open socket) kept by its inode — before the run holds it, so a call of the run's
    /// on it finds it — and collected from now on.
    pub fn keep(self: &Arc<Self>, fd: &OwnedFd) -> io::Result<()> {
        let ino = inode(fd)?;
        let copy = Arc::new(fd.try_clone()?);
        let start = {
            let mut i = self.lock();
            let born = i.scans;
            i.held.insert(ino, Copy { fd: copy, born, unseen: 0, in_flight: None });
            i.interval = INTERVAL;
            self.scanned && !std::mem::replace(&mut i.collecting, true)
        };
        if start {
            let weak = Arc::downgrade(self);
            crate::event::thread("tarae-copies", move || collecting(&weak));
        }
        Ok(())
    }

    /// The copy of the socket with inode `ino`, as a descriptor of tarae's own — None: tarae has none.
    pub fn get(&self, ino: u64) -> Option<OwnedFd> {
        let copy = self.lock().held.get(&ino).map(|c| c.fd.clone())?;
        copy.try_clone().ok()
    }

    /// The socket with inode `ino` was passed in a message sent on `via` (a served `sendmsg`, after it went): its
    /// copy is kept while `via` has anything its peer hasn't read — the message waiting in the receiver's queue,
    /// held by no process — up to `IN_FLIGHT_MAX` (`settle`). Nothing where tarae keeps no copy of it.
    pub fn in_flight(&self, ino: u64, via: &OwnedFd) {
        self.in_flight_since(ino, via, Instant::now());
    }

    fn in_flight_since(&self, ino: u64, via: &OwnedFd, since: Instant) {
        let Ok(via) = via.try_clone() else { return };
        if let Some(c) = self.lock().held.get_mut(&ino) {
            c.in_flight = Some(InFlight { via, since });
        }
    }

    /// Tests: `in_flight` as if `ago` before now.
    #[cfg(test)]
    pub fn in_flight_ago(&self, ino: u64, via: &OwnedFd, ago: Duration) {
        self.in_flight_since(ino, via, Instant::now() - ago);
    }

    /// How many copies are kept.
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.lock().held.len()
    }

    /// One scan: the sockets the processes below `tree` hold read (`look`), each copy kept before it began that
    /// none holds — and not in flight — struck, at `STRIKES` gone; and the next one paced (`pace`). How many went.
    pub fn collect(&self) -> usize {
        let began = {
            let mut i = self.lock();
            i.scans += 1;
            i.scans
        };
        let at = Instant::now();
        let look = (self.look)(self.tree);
        let (gone, struck) = self.settle(began, &look);
        self.pace(struck);
        SCANS.add(at.elapsed(), look.read);
        gone
    }

    /// `collect` twice at once (a bind that found its port taken: a closed socket's copy goes now, not in two
    /// intervals). How many went.
    pub fn collect_now(&self) -> usize {
        self.collect() + self.collect()
    }

    /// A bind that found its port taken (`taken`: its error says so), where a copy may be what holds it: the copies
    /// collected at once (`collect_now`); then, where a copy went just now — in that collection, or the scanner's
    /// within `RELEASE_WAIT` (it may have taken the copy a moment before) — `bind` tried again until the port isn't
    /// taken or `RELEASE_WAIT` is past (a process tarae is starting may hold the socket a moment longer). Its last
    /// answer; None where no copy went (the port is someone else's: no wait).
    pub fn bind_again<T, E>(
        &self,
        mut bind: impl FnMut() -> Result<T, E>,
        taken: impl Fn(&E) -> bool,
    ) -> Option<Result<T, E>> {
        let gone = self.collect_now();
        let lately = self.lock().went.is_some_and(|at| at.elapsed() < RELEASE_WAIT);
        if gone == 0 && !lately {
            return None;
        }
        let by = Instant::now() + RELEASE_WAIT;
        loop {
            let r = bind();
            match &r {
                Err(e) if taken(e) && Instant::now() < by => std::thread::sleep(Duration::from_millis(1)),
                _ => return Some(r),
            }
        }
    }

    /// The copies struck by scan `began` that found `look`: how many went, and whether any was struck (or went). A
    /// copy is held where `look` saw it held, where it couldn't read everything (`Look::unreadable`), and while its
    /// socket is in flight (`in_flight`).
    fn settle(&self, began: u64, look: &Look) -> (usize, bool) {
        let mut i = self.lock();
        let before = i.held.len();
        let mut struck = false;
        i.held.retain(|ino, c| {
            if c.born >= began {
                return true;
            }
            if let Some(f) = &c.in_flight {
                if f.since.elapsed() < IN_FLIGHT_MAX && unread(&f.via) {
                    c.unseen = 0;
                    return true;
                }
                c.in_flight = None;
            }
            c.unseen = if look.unreadable || look.held.contains(ino) { 0 } else { c.unseen + 1 };
            struck |= c.unseen > 0;
            c.unseen < STRIKES
        });
        let gone = before - i.held.len();
        if gone > 0 {
            i.went = Some(Instant::now());
        }
        (gone, struck)
    }
}

/// Whether socket `via` has sent anything its peer hasn't read yet (`SIOCOUTQ`: for a unix socket, the memory of its
/// messages still queued at the receiver — freed when read, or dropped). Not on another system (copies are Linux's).
fn unread(via: &OwnedFd) -> bool {
    #[cfg(target_os = "linux")]
    {
        let mut n: libc::c_int = 0;
        // SAFETY: an int the call fills, on our own descriptor. (`SIOCOUTQ` is `TIOCOUTQ`'s number.)
        let r = unsafe { libc::ioctl(via.as_raw_fd(), libc::TIOCOUTQ, &mut n) };
        r == 0 && n > 0
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = via;
        false
    }
}

/// A run's copies collected, each wait as `Copies::interval` says, until there's none left (or no supervisor: the
/// run's gone).
fn collecting(copies: &Weak<Copies>) {
    loop {
        let Some(wait) = copies.upgrade().map(|c| c.interval()) else { return };
        std::thread::sleep(wait);
        let Some(c) = copies.upgrade() else { return };
        c.collect();
        let mut i = c.lock();
        if i.held.is_empty() {
            i.collecting = false;
            return;
        }
    }
}

/// The inode of a socket.
pub fn inode(fd: &OwnedFd) -> io::Result<u64> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: a descriptor of ours and a stat buffer the call fills.
    if unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: filled by the successful fstat above.
    Ok(unsafe { st.assume_init() }.st_ino)
}

/// What a descriptor's link names (`/proc/<pid>/fd/<n>`): a socket's inode (`socket:[123]`), else None.
pub fn socket_inode(link: &str) -> Option<u64> {
    link.strip_prefix("socket:[")?.strip_suffix(']')?.parse().ok()
}

/// What a failed read of a process's `/proc` says: `Gone` — it ended meanwhile (`ENOENT`, `ESRCH`), it holds
/// nothing; else `Unreadable` — it may hold anything (a non-dumpable process's `fd` is refused, `EACCES`).
#[derive(Debug, PartialEq)]
enum Failed {
    Gone,
    Unreadable,
}

fn failed(e: &io::Error) -> Failed {
    match e.raw_os_error() {
        Some(libc::ENOENT | libc::ESRCH) => Failed::Gone,
        _ => Failed::Unreadable,
    }
}

/// What the processes below `tree` hold (`Look`).
fn holders(tree: u32) -> Look {
    let (pids, complete) = below(tree);
    let mut look = Look { unreadable: !complete, ..Look::default() };
    for pid in pids {
        let dir = match std::fs::read_dir(format!("/proc/{pid}/fd")) {
            Ok(dir) => dir,
            Err(e) => {
                look.unreadable |= failed(&e) == Failed::Unreadable;
                continue;
            }
        };
        for e in dir.flatten() {
            look.read += 1;
            match std::fs::read_link(e.path()) {
                Ok(l) => look.held.extend(socket_inode(&l.to_string_lossy())),
                // (a descriptor closed meanwhile holds nothing)
                Err(e) => look.unreadable |= failed(&e) == Failed::Unreadable,
            }
        }
    }
    look
}

/// The processes below `tree` (not itself), and whether every one's children could be read: by each thread's
/// `children` (`/proc/<pid>/task/<tid>/children`) — or, where the kernel has none of those for `tree`
/// (`CONFIG_PROC_CHILDREN`), by every process's parent.
fn below(tree: u32) -> (Vec<u32>, bool) {
    let children = |pid: u32| -> io::Result<Vec<u32>> {
        let mut out = Vec::new();
        for t in std::fs::read_dir(format!("/proc/{pid}/task"))?.flatten() {
            match std::fs::read_to_string(t.path().join("children")) {
                Ok(list) => out.extend(list.split_whitespace().filter_map(|p| p.parse::<u32>().ok())),
                // (a thread that ended meanwhile)
                Err(e) if failed(&e) == Failed::Gone => {}
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    };
    let Ok(first) = children(tree) else { return (by_parent(tree), true) };
    let (mut out, mut next, mut complete) = (Vec::new(), first, true);
    while let Some(pid) = next.pop() {
        out.push(pid);
        match children(pid) {
            Ok(c) => next.extend(c),
            Err(e) => complete &= failed(&e) == Failed::Gone,
        }
    }
    (out, complete)
}

/// `below` from every process's parent (`/proc/<pid>/stat`'s fourth field).
fn by_parent(tree: u32) -> Vec<u32> {
    let mut parents: HashMap<u32, Vec<u32>> = HashMap::new();
    for e in std::fs::read_dir("/proc").into_iter().flatten().flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { continue };
        // (after the name's closing parenthesis: state, then the parent)
        let ppid = stat.rsplit_once(')').and_then(|(_, r)| r.split_whitespace().nth(1)?.parse::<u32>().ok());
        if let Some(ppid) = ppid {
            parents.entry(ppid).or_default().push(pid);
        }
    }
    let (mut out, mut next) = (Vec::new(), parents.get(&tree).cloned().unwrap_or_default());
    while let Some(pid) = next.pop() {
        out.push(pid);
        next.extend(parents.get(&pid).cloned().unwrap_or_default());
    }
    out
}

/// Every scan's cost so far (a measurement reads it before and after: `measure_copies_scan`).
pub struct Scans {
    pub count: AtomicU64,
    pub nanos: AtomicU64,
    pub longest: AtomicU64,
    pub descriptors: AtomicU64,
}

impl Scans {
    fn add(&self, took: Duration, read: u64) {
        let ns = u64::try_from(took.as_nanos()).unwrap_or(u64::MAX);
        self.count.fetch_add(1, Relaxed);
        self.nanos.fetch_add(ns, Relaxed);
        self.longest.fetch_max(ns, Relaxed);
        self.descriptors.fetch_add(read, Relaxed);
    }
}

pub static SCANS: Scans = Scans {
    count: AtomicU64::new(0),
    nanos: AtomicU64::new(0),
    longest: AtomicU64::new(0),
    descriptors: AtomicU64::new(0),
};

#[cfg(test)]
mod tests;
