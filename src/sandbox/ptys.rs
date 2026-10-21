//! A run's terminals where its session has no `devpts` of its own (3b, `target/sb/linux7-plan.md`: no namespaces,
//! served — `tier::serves_ptys`). There `/dev/pts` is this system's — every pty of the user's, other sessions'
//! too — so a run's Landlock rules grant none of it (`linux::confine`) but a pool tarae opened for that run before
//! it started (`Ptys::open`), granted by inode: a number freed and taken again is a new inode, never granted.
//! A pty the run makes from `/dev/ptmx` is swapped for one of the pool at its first `TIOCSPTLCK`, `TIOCGPTN` or
//! `TIOCGPTPEER` — every way to a pty's other end passes one, and seccomp hands those to the supervisor
//! (`supervisor::pty_ioctl`) — so the `/dev/pts/N` it then opens is one its rules grant.
//!
//! tarae keeps its own master of each, so a pty the run let go is still that inode: while the run holds one, what
//! its processes hold is looked at (`/proc/<pid>/fd`, the processes `below` its keeper) — one none of them holds is emptied,
//! set as new and locked, and goes back to the pool (`settle` — judged only by a look begun after it was handed);
//! one whose master the run closed while something of
//! it still holds the other end is hung up as without tarae — tarae closes its master too — and is gone. So a run
//! has `POOL` at a time; past them, every one held, it's refused, said.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// The terminals one run holds at a time — each a pty tarae holds while the run goes (`kernel.pty.max`: 4096).
pub const POOL: usize = 16;

/// How often what a run holds is looked at while it has one of the pool (the latest a pty it closed is hung up).
pub const WATCH_EVERY: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    /// Locked, set as new, nothing of the run's holding it.
    Free,
    /// Taken for a call, not yet in the run's place (no look takes it back meanwhile).
    Pending,
    /// In the run's place.
    Handed,
    /// Hung up, or failed to be set as new: tarae's master closed.
    Gone,
}

struct Entry {
    /// Its number: `/dev/pts/<n>`.
    n: u32,
    /// tarae's master — the run's is the same open file (`ADDFD`); while tarae holds it the pty is that inode.
    master: Option<OwnedFd>,
    /// Its other end, `O_PATH` (no terminal opened): the inode the rules grant — closed once they hold it
    /// (`granted`): a rule keeps its own reference to the inode, and tarae's master keeps the pty that inode.
    slave: Option<OwnedFd>,
    state: State,
    /// The looks at what the run holds begun when it was last handed (`Inner.looks`): only a look begun after
    /// that judges it (`settle`).
    handed: u64,
}

/// What the run's processes hold of one pty.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Held {
    pub master: bool,
    pub slave: bool,
}

/// A run's pool.
pub struct Ptys {
    inner: Mutex<Inner>,
    woken: Condvar,
}

struct Inner {
    entries: Vec<Entry>,
    /// A new pty's settings, its other end's (`tcgetattr`), what one going back to the pool is set to.
    defaults: Option<libc::termios>,
    ended: bool,
    /// The looks at what the run holds begun (`look`) — what a hand-over is stamped with (`Entry.handed`).
    looks: u64,
}

#[cfg(test)]
fn stat(fd: BorrowedFd) -> io::Result<libc::stat> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: a descriptor we hold; the struct for the call to fill.
    if unsafe { libc::fstat(fd.as_raw_fd(), st.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: filled by the call that succeeded.
    Ok(unsafe { st.assume_init() })
}

/// `r` of a libc call that says -1 and errno.
fn ok(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

/// Locks (`true`) or unlocks `master`'s pty: locked, its other end opens for no one.
fn lock(master: BorrowedFd, locked: bool) -> io::Result<()> {
    let v = libc::c_int::from(locked);
    // SAFETY: an int for the call to read, on a descriptor we hold.
    ok(unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCSPTLCK, &v) }).map(drop)
}

/// `master`'s other end opened through it (`TIOCGPTPEER`: the same `devpts` the master's in) — never as tarae's
/// controlling terminal, close-on-exec; `flags`' access and `O_NONBLOCK` kept.
fn peer(master: BorrowedFd, flags: i32) -> io::Result<OwnedFd> {
    let flags =
        (flags & (libc::O_ACCMODE | libc::O_NONBLOCK | libc::O_PATH)) | libc::O_NOCTTY | libc::O_CLOEXEC;
    // SAFETY: a plain call on a descriptor we hold; what it makes is ours.
    let fd = ok(unsafe { libc::ioctl(master.as_raw_fd(), libc::TIOCGPTPEER, flags) })?;
    // SAFETY: just made.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

impl Entry {
    /// A pty: its master opened (`/dev/ptmx`, locked as it comes), its number, its other end's `O_PATH`.
    fn open() -> io::Result<Entry> {
        // SAFETY: plain calls on a descriptor this makes and owns; an int for the call to fill.
        let (master, n) = unsafe {
            let m = ok(libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC))?;
            let master = OwnedFd::from_raw_fd(m);
            let mut n: libc::c_uint = 0;
            ok(libc::ioctl(m, libc::TIOCGPTN, &mut n))?;
            (master, n)
        };
        let slave = peer(master.as_fd(), libc::O_PATH)?;
        Ok(Entry { n, master: Some(master), slave: Some(slave), state: State::Free, handed: 0 })
    }

    /// Back as a new one, for the next pty the run makes: what waits on either side emptied, its other end's
    /// settings `defaults` and no size, its master's flags and packet mode cleared — the run's master was this open
    /// file — and locked. Anything failing: gone (tarae's master closed).
    fn renew(&mut self, defaults: Option<&libc::termios>) {
        let Some(m) = self.master.as_ref().map(|m| m.as_fd()) else { return };
        let renewed = (|| -> io::Result<()> {
            lock(m, false)?;
            let s = peer(m, libc::O_RDWR | libc::O_NONBLOCK)?;
            let none: libc::winsize = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
            let off: libc::c_int = 0;
            // SAFETY: plain calls on descriptors we hold; structs and ints for them to read.
            unsafe {
                ok(libc::tcflush(s.as_raw_fd(), libc::TCIOFLUSH))?;
                if let Some(t) = defaults {
                    ok(libc::tcsetattr(s.as_raw_fd(), libc::TCSANOW, t))?;
                }
                ok(libc::ioctl(s.as_raw_fd(), libc::TIOCSWINSZ, &none))?;
                drop(s);
                ok(libc::tcflush(m.as_raw_fd(), libc::TCIOFLUSH))?;
                ok(libc::ioctl(m.as_raw_fd(), libc::TIOCPKT, &off))?;
                ok(libc::fcntl(m.as_raw_fd(), libc::F_SETFL, libc::O_RDWR))?;
            }
            lock(m, true)
        })();
        match renewed {
            Ok(()) => self.state = State::Free,
            Err(e) => {
                crate::log::line("sandbox", "warn", &format!("a run's pty {} not set as new: {e}", self.n));
                self.gone();
            }
        }
    }

    fn gone(&mut self) {
        self.master = None;
        self.state = State::Gone;
    }
}

impl Inner {
    fn entry(&mut self, n: u32) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.n == n && e.state != State::Gone)
    }
}

impl Ptys {
    /// Up to `k` ptys, locked — fewer where the system gives no more (said in the log): past them the run is
    /// refused one. A new one's settings read from the first.
    pub fn open(k: usize) -> Ptys {
        let mut entries = Vec::with_capacity(k);
        while entries.len() < k {
            match Entry::open() {
                Ok(e) => entries.push(e),
                Err(e) => {
                    let said = format!("a run's terminals: {} of {k} opened ({e})", entries.len());
                    crate::log::line("sandbox", "warn", &said);
                    break;
                }
            }
        }
        let defaults = entries.first().and_then(|e: &Entry| {
            let m = e.master.as_ref()?.as_fd();
            let read = (|| -> io::Result<libc::termios> {
                lock(m, false)?;
                let s = peer(m, libc::O_RDWR | libc::O_NONBLOCK)?;
                let mut t = std::mem::MaybeUninit::<libc::termios>::uninit();
                // SAFETY: a descriptor we hold; the struct for the call to fill, read only once filled.
                let t = unsafe {
                    ok(libc::tcgetattr(s.as_raw_fd(), t.as_mut_ptr()))?;
                    t.assume_init()
                };
                drop(s);
                lock(m, true)?;
                Ok(t)
            })();
            read.ok()
        });
        Ptys { inner: Mutex::new(Inner { entries, defaults, ended: false, looks: 0 }), woken: Condvar::new() }
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// `f` on each pty's other end (`O_PATH`) and its number — what `linux::confine` grants (before `granted`).
    pub fn each(&self, mut f: impl FnMut(BorrowedFd<'_>, u32) -> io::Result<()>) -> io::Result<()> {
        self.inner()
            .entries
            .iter()
            .filter_map(|e| Some((e.slave.as_ref()?, e.n)))
            .try_for_each(|(s, n)| f(s.as_fd(), n))
    }

    /// The pool's pty numbers (a test's look at what's open).
    #[cfg(test)]
    pub fn numbers(&self) -> Vec<u32> {
        self.inner().entries.iter().map(|e| e.n).collect()
    }

    /// The rules hold every pty's other end: their `O_PATH`s closed — half the pool's descriptors (a Landlock
    /// rule keeps its own reference to the inode it was given; tarae's master keeps the pty that inode).
    pub fn granted(&self) {
        for e in &mut self.inner().entries {
            e.slave = None;
        }
    }

    /// Whether pty `n` is one of the pool's (not gone) — the run's descriptor of it is then tarae's master's open
    /// file, put there before. One back in the pool that the run still had (passed in a message while it was
    /// looked at) is the run's again.
    pub fn known(&self, n: u32) -> bool {
        let mut inner = self.inner();
        let looks = inner.looks;
        let Some(e) = inner.entry(n) else { return false };
        if e.state == State::Free {
            (e.state, e.handed) = (State::Handed, looks);
            if let Some(m) = &e.master {
                let _ = lock(m.as_fd(), false);
            }
        }
        drop(inner);
        self.woken.notify_all();
        true
    }

    /// A pty of the pool for a call of the run's, unlocked (as a pty is once asked to be): one free, else — every
    /// one handed — once what the processes below `tree` hold was looked at and those none holds went back
    /// (`settle`). None: every one held (or what they hold couldn't be read).
    pub fn take(&self, tree: u32) -> Option<u32> {
        if let Some(n) = self.take_free() {
            return Some(n);
        }
        let (began, held) = self.look(tree);
        self.settle(began, &held?);
        self.take_free()
    }

    /// One look at what the processes below `tree` hold (`holders`, read with no lock held): the looks begun, this
    /// one counted — what `settle` judges a pty by — and what it saw.
    fn look(&self, tree: u32) -> (u64, Option<HashMap<u32, Held>>) {
        let began = self.begin();
        (began, holders(tree))
    }

    /// A look begins: the looks begun, this one counted.
    fn begin(&self) -> u64 {
        let mut inner = self.inner();
        inner.looks += 1;
        inner.looks
    }

    fn take_free(&self) -> Option<u32> {
        let mut inner = self.inner();
        let e = inner.entries.iter_mut().find(|e| e.state == State::Free)?;
        let m = e.master.as_ref()?.as_fd();
        if lock(m, false).is_err() {
            e.gone();
            return None;
        }
        e.state = State::Pending;
        Some(e.n)
    }

    /// `f` on tarae's master of pty `n` — None: no such pty of the pool.
    pub fn with_master<R>(&self, n: u32, f: impl FnOnce(&OwnedFd) -> R) -> Option<R> {
        let mut inner = self.inner();
        inner.entry(n).and_then(|e| e.master.as_ref()).map(f)
    }

    /// Pty `n` taken (`take`) is in the run's place now: looked at from here on.
    pub fn handed_over(&self, n: u32) {
        let mut inner = self.inner();
        let looks = inner.looks;
        if let Some(e) = inner.entry(n) {
            (e.state, e.handed) = (State::Handed, looks);
        }
        drop(inner);
        self.woken.notify_all();
    }

    /// Pty `n` taken (`take`) never reached the run: back to the pool.
    pub fn give_back(&self, n: u32) {
        let mut inner = self.inner();
        let defaults = inner.defaults;
        if let Some(e) = inner.entry(n) {
            e.renew(defaults.as_ref());
        }
    }

    /// Pty `n`'s other end opened as `TIOCGPTPEER` would.
    pub fn peer(&self, n: u32, flags: i32) -> io::Result<OwnedFd> {
        let unknown = || io::Error::from_raw_os_error(libc::EIO);
        self.with_master(n, |m| peer(m.as_fd(), flags & !libc::O_PATH)).unwrap_or_else(|| Err(unknown()))
    }

    /// Pty `n` unlocked, as the run asked.
    pub fn unlock(&self, n: u32) -> io::Result<()> {
        self.with_master(n, |m| lock(m.as_fd(), false))
            .unwrap_or_else(|| Err(io::Error::from_raw_os_error(libc::EIO)))
    }

    /// Each pty handed before look `began` (`look`) by what the run held of it then (`held`, by number): its master
    /// — still the run's; neither end — back to the pool, new; only the other end — hung up (tarae's master closed:
    /// what holds that end reads its end, as without tarae) and gone. One handed since that look began isn't in
    /// what it saw — `/proc` was read before it was there — so it isn't judged: taken back, the run would hold it and
    /// be handed it again, past `POOL`. How many went back.
    pub fn settle(&self, began: u64, held: &HashMap<u32, Held>) -> usize {
        let mut inner = self.inner();
        let defaults = inner.defaults;
        let mut back = 0;
        for e in inner.entries.iter_mut().filter(|e| e.state == State::Handed && e.handed < began) {
            match held.get(&e.n).copied().unwrap_or_default() {
                Held { master: true, .. } => {}
                Held { slave: true, .. } => e.gone(),
                Held { .. } => {
                    e.renew(defaults.as_ref());
                    back += usize::from(e.state == State::Free);
                }
            }
        }
        back
    }

    /// Looks at what the run's processes (below `tree`) hold every `WATCH_EVERY` while it has a pty of the pool
    /// (`settle`), on a thread of its own, until `end`.
    pub fn watch(self: &Arc<Self>, tree: u32) {
        let ptys = self.clone();
        let spawned = std::thread::Builder::new().name("tarae-ptys".into()).spawn(move || {
            loop {
                let inner = ptys.inner();
                let mut inner = ptys
                    .woken
                    .wait_while(inner, |i| !i.ended && !i.entries.iter().any(|e| e.state == State::Handed))
                    .unwrap_or_else(|e| e.into_inner());
                if !inner.ended {
                    inner = ptys.woken.wait_timeout(inner, WATCH_EVERY).unwrap_or_else(|e| e.into_inner()).0;
                }
                if inner.ended {
                    return;
                }
                drop(inner);
                if let (began, Some(held)) = ptys.look(tree) {
                    ptys.settle(began, &held);
                }
            }
        });
        if let Err(e) = spawned {
            crate::log::line("sandbox", "warn", &format!("a run's ptys aren't watched: {e}"));
        }
    }

    /// The run is over: every pty closed (tarae's masters), the watch ended.
    pub fn end(&self) {
        let mut inner = self.inner();
        inner.ended = true;
        inner.entries.iter_mut().for_each(Entry::gone);
        drop(inner);
        self.woken.notify_all();
    }
}

/// The pty number a run's descriptor is (`/proc/<pid>/fdinfo/<n>`'s `tty-index:`, a master's and a slave's alike).
pub fn tty_index(fdinfo: &str) -> Option<u32> {
    fdinfo.lines().find_map(|l| l.strip_prefix("tty-index:")).and_then(|v| v.trim().parse().ok())
}

/// What the processes below `tree` hold of ptys, by number (`below`, each one's `/proc/<pid>/fd`: a
/// master names `ptmx`, its number from `fdinfo`; the other end names `/dev/pts/N`) — None where not every one
/// could be read (then nothing is taken back).
pub fn holders(tree: u32) -> Option<HashMap<u32, Held>> {
    let (pids, complete) = below(tree);
    if !complete {
        return None;
    }
    let mut held: HashMap<u32, Held> = HashMap::new();
    for pid in pids {
        let dir = match std::fs::read_dir(format!("/proc/{pid}/fd")) {
            Ok(dir) => dir,
            // (a process that ended meanwhile holds nothing)
            Err(e) if matches!(e.raw_os_error(), Some(libc::ENOENT | libc::ESRCH)) => continue,
            Err(_) => return None,
        };
        for e in dir.flatten() {
            let Ok(link) = std::fs::read_link(e.path()) else { continue };
            // (a master's number is its `fdinfo`'s; the other end's `fdinfo` has none — its number is its name, in
            // the `devpts` tarae's masters are in: there's no other where ptys are served)
            match link.to_str() {
                Some("/dev/ptmx" | "/dev/pts/ptmx") => {
                    let fd = e.file_name();
                    // (as bytes, as the supervisor reads it: only the number's line is the kernel's to word)
                    let info = std::fs::read(format!("/proc/{pid}/fdinfo/{}", fd.to_string_lossy()));
                    if let Some(n) = info.ok().and_then(|i| tty_index(&String::from_utf8_lossy(&i))) {
                        held.entry(n).or_default().master = true;
                    }
                }
                Some(l) => {
                    if let Some(n) = l.strip_prefix("/dev/pts/").and_then(|n| n.parse::<u32>().ok()) {
                        held.entry(n).or_default().slave = true;
                    }
                }
                None => {}
            }
        }
    }
    Some(held)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The pool's ptys are each one of their own, locked until taken, their other end the inode `/dev/pts/<n>`
    /// names (what the rules grant). One the run let go goes back as that same pty, new and locked; one whose other
    /// end is still held when its master isn't is hung up — its other end reads its end — and gone.
    #[test]
    fn a_pool_pty_goes_back_as_itself_or_is_hung_up() {
        let p = Ptys::open(2);
        let mut seen = Vec::new();
        p.each(|fd, n| {
            let named = std::fs::metadata(format!("/dev/pts/{n}"));
            seen.push((n, stat(fd).unwrap().st_ino, named.map(|m| std::os::unix::fs::MetadataExt::ino(&m))));
            Ok(())
        })
        .unwrap();
        if seen.len() < 2 {
            eprintln!(
                "a_pool_pty_goes_back_as_itself_or_is_hung_up: skipped — this system gave {} ptys",
                seen.len()
            );
            return;
        }
        for (n, ino, named) in &seen {
            assert_eq!(named.as_ref().ok(), Some(ino), "/dev/pts/{n}");
        }
        assert!(p.peer(seen[0].0, libc::O_RDWR).is_err(), "locked until taken");
        let a = p.take_free().unwrap();
        p.handed_over(a);
        let end = p.peer(a, libc::O_RDWR).expect("taken: unlocked");
        // SAFETY: a few bytes from a buffer of ours to a descriptor we hold.
        unsafe { libc::write(end.as_raw_fd(), b"left\n".as_ptr().cast(), 5) };
        drop(end);
        assert_eq!(p.settle(p.begin(), &HashMap::new()), 1, "nothing holds it: back");
        assert!(p.peer(a, libc::O_RDWR).is_err(), "back, and locked");
        assert_eq!(p.take_free(), Some(a), "the same pty again");
        let mut pending = [0u8; 8];
        let left = p.with_master(a, |m| {
            let mut fl = libc::pollfd { fd: m.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: a struct and a buffer of ours, a descriptor we hold.
            unsafe {
                libc::poll(&mut fl, 1, 0) > 0 && libc::read(m.as_raw_fd(), pending.as_mut_ptr().cast(), 8) > 0
            }
        });
        assert_eq!(left, Some(false), "what was left there emptied");
        p.handed_over(a);
        let end = p.peer(a, libc::O_RDWR | libc::O_NONBLOCK).unwrap();
        let held = HashMap::from([(a, Held { master: false, slave: true })]);
        assert_eq!(p.settle(p.begin(), &held), 0);
        hung_up(end.as_fd()).expect("its other end hung up");
        assert!(!p.known(a), "gone");
        let b = p.take_free().unwrap();
        assert_ne!(b, a);
        assert!(p.take_free().is_none(), "two of two");
        p.end();
    }

    /// The race a look runs (`look`: `/proc` read with no lock held): it began before a pty was handed over, so what
    /// it saw doesn't have that pty — yet the run holds it. Judged by that look, it'd go back to the pool and be
    /// handed again: two of the run's descriptors on one pty, past `POOL`. So only a
    /// look begun after the hand-over judges it — that one, seeing it held, keeps it; one seeing it let go takes it
    /// back. A pty taken again while a look ran (`known`) is the same.
    #[test]
    fn a_look_begun_before_a_hand_over_doesnt_take_that_pty_back() {
        let p = Ptys::open(1);
        let Some(a) = p.take_free() else {
            return eprintln!(
                "a_look_begun_before_a_hand_over_doesnt_take_that_pty_back: skipped — no pty here"
            );
        };
        let began = p.begin();
        p.handed_over(a);
        assert_eq!(p.settle(began, &HashMap::new()), 0, "the look began before it was the run's");
        assert_eq!(p.take_free(), None, "still the run's: not handed again");
        let held = HashMap::from([(a, Held { master: true, slave: false })]);
        assert_eq!(p.settle(p.begin(), &held), 0, "a later look that sees it held keeps it");
        assert_eq!(p.settle(p.begin(), &HashMap::new()), 1, "one that sees it let go takes it back");
        let began = p.begin();
        assert!(p.known(a), "back in the pool, the run's again");
        assert_eq!(p.settle(began, &HashMap::new()), 0, "taken again after the look began");
        assert_eq!(p.take_free(), None);
        p.end();
    }

    /// Waits until `end`, a pty's other end, reads its end — hung up —, at most `HUNG_WAIT` (then why not). The
    /// hang-up comes when tarae's master's last copy closes: a process another thread is spawning holds a copy of
    /// every descriptor until it execs, so dropping the master may hang the pty up only a moment later.
    fn hung_up(end: BorrowedFd) -> Result<(), String> {
        const HUNG_WAIT: Duration = Duration::from_secs(10);
        let mut fl = libc::pollfd { fd: end.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: a struct of ours, a descriptor we hold.
        let ready = unsafe { libc::poll(&mut fl, 1, HUNG_WAIT.as_millis() as libc::c_int) };
        match ready {
            0 => return Err(format!("not hung up within {HUNG_WAIT:?}")),
            ..0 => return Err(format!("poll: {}", io::Error::last_os_error())),
            _ => {}
        }
        let mut b = [0u8; 1];
        // SAFETY: a buffer of ours, a descriptor we hold.
        let r = unsafe { libc::read(end.as_raw_fd(), b.as_mut_ptr().cast(), 1) };
        let e = io::Error::last_os_error();
        match r == 0 || e.raw_os_error() == Some(libc::EIO) {
            true => Ok(()),
            false => Err(format!("read {r}: {e}")),
        }
    }

    #[test]
    fn a_descriptors_pty_number_is_read_from_its_fdinfo() {
        assert_eq!(tty_index("pos:\t0\nflags:\t02\nmnt_id:\t25\nino:\t3\ntty-index:\t7\n"), Some(7));
        assert_eq!(tty_index("pos:\t0\nflags:\t02\n"), None);
    }

    /// What a run's pool costs it before it starts (3b's design guessed < 0.3 ms): `POOL` ptys opened and each
    /// granted (where this kernel has Landlock), then let go — medians of 20.
    #[test]
    fn perf_a_runs_pty_pool() {
        if crate::test_results::unoptimized("sandbox::ptys::tests::perf_a_runs_pty_pool") {
            return;
        }
        let abi = crate::landlock::abi().ok();
        let (mut made, mut gone) = (Vec::new(), Vec::new());
        let mut opened = 0;
        for _ in 0..20 {
            let at = std::time::Instant::now();
            let p = Ptys::open(POOL);
            if let Some(mut r) = abi.and_then(|a| crate::landlock::Ruleset::new(a).ok()) {
                let path = std::path::Path::new("/dev/pts/n");
                p.each(|fd, _| r.allow_at(fd, path, crate::landlock::READ_FILE).map(drop)).unwrap();
            }
            made.push(at.elapsed().as_secs_f64() * 1000.0);
            opened = p.inner().entries.len();
            let at = std::time::Instant::now();
            p.end();
            drop(p);
            gone.push(at.elapsed().as_secs_f64() * 1000.0);
        }
        let median = |mut t: Vec<f64>| {
            t.sort_by(f64::total_cmp);
            t[t.len() / 2]
        };
        let line = format!(
            "a run's pty pool ({opened} of {POOL}, Landlock {}): made and granted {:.3} ms, let go {:.3} ms — medians \
             of 20\n",
            abi.map_or("none".into(), |a| format!("ABI {a}")),
            median(made),
            median(gone)
        );
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    }

    /// A master's number is its `fdinfo`'s `tty-index:` — the kernel's, for a pty of the test's own, is the one
    /// its other end's name says; and any `fdinfo`, mutated 20,000 times, is read without a panic, a number only
    /// from a `tty-index:` line's start.
    #[test]
    fn a_ptys_number_is_its_fdinfos() {
        use crate::mutate::{self, Rng};
        // SAFETY: a new descriptor, owned at once.
        let fd = unsafe { libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) };
        assert!(fd >= 0, "{}", std::io::Error::last_os_error());
        // SAFETY: just made, ours alone.
        let master = unsafe { OwnedFd::from_raw_fd(fd) };
        let mut name = [0 as libc::c_char; 64];
        // SAFETY: room for the name and its NUL.
        assert_eq!(unsafe { libc::ptsname_r(master.as_raw_fd(), name.as_mut_ptr(), name.len()) }, 0);
        // SAFETY: NUL-terminated by the call.
        let name = unsafe { std::ffi::CStr::from_ptr(name.as_ptr()) }.to_string_lossy().into_owned();
        let n: u32 = name.strip_prefix("/dev/pts/").unwrap().parse().unwrap();
        let info = std::fs::read_to_string(format!("/proc/self/fdinfo/{}", master.as_raw_fd())).unwrap();
        assert_eq!(tty_index(&info), Some(n), "{name}: {info}");
        let words = ["tty-index:", "\t", "\n", " ", "flags:", "4294967296"];
        let mut rng = Rng(0x5eed_0add_7e55_0108);
        for _ in 0..20_000 {
            let mut t = info.clone();
            mutate::text(&mut rng, &mut t, &words);
            if let Some(got) = tty_index(&t) {
                let line = t.lines().find(|l| l.starts_with("tty-index:"));
                assert!(line.is_some_and(|l| l.contains(&got.to_string())), "{got} of {t:?}");
            }
        }
    }
}
