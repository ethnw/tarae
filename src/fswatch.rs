//! Waking up when files change: inotify on Linux, kqueue on macOS, a timer anywhere else. An event only
//! says "look now" — callers still decide by stat, as they did when they polled.
//!
//! A watcher is a few syscalls, so callers make a fresh one each round, *before* they stat: a change made
//! while they look still wakes the wait, and a folder created or replaced meanwhile is simply watched the
//! next round. A network file system (NFS, SMB, FUSE…) doesn't tell the kernel about other machines'
//! writes, so there — and wherever a watch can't be set up — a wait lasts only as long as the old poll.

use std::ffi::{OsStr, OsString};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a wait lasts when events can be trusted — only a backstop for one we didn't foresee.
const BACKSTOP: Duration = Duration::from_secs(5);
/// …and when they can't: the poll interval from before watching.
const POLL: Duration = Duration::from_millis(500);
/// A burst of writes (a truncate-then-write save, a checkout) is over after this much quiet…
const QUIET: Duration = Duration::from_millis(50);
/// …or after this long — a file written to non-stop (a log) is looked at twice a second, as before.
const BURST_MAX: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Woke {
    /// Something watched may have changed (a burst of writes has settled).
    Changed,
    TimedOut,
    /// The waker was used — for good: every later wait says so at once.
    Cancelled,
}

/// Ends a watcher's wait from another thread: a pipe the wait listens on too. (If a pipe can't be had, the
/// wait just runs to its end.)
#[derive(Clone)]
pub struct Waker(Option<Arc<Pipe>>);

struct Pipe {
    read: OwnedFd,
    write: OwnedFd,
}

impl Waker {
    pub fn new() -> Waker {
        let mut fds = [0; 2];
        // (atomically close-on-exec where the OS can: another thread may be spawning a program meanwhile)
        #[cfg(any(target_os = "linux", target_os = "android"))]
        let made = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } == 0;
        #[cfg(not(any(target_os = "linux", target_os = "android")))]
        let made = unsafe { libc::pipe(fds.as_mut_ptr()) } == 0
            && fds.iter().all(|&fd| unsafe {
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) == 0
                    && libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK) == 0
            });
        if !made {
            return Waker(None);
        }
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        Waker(Some(Arc::new(Pipe { read, write })))
    }

    pub fn wake(&self) {
        if let Some(p) = &self.0 {
            // Never read back: the pipe stays readable, so the watcher stays cancelled
            unsafe { libc::write(p.write.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
        }
    }

    fn fd(&self) -> Option<RawFd> {
        self.0.as_ref().map(|p| p.read.as_raw_fd())
    }
}

pub struct Watcher {
    sys: Option<sys::Sys>,
    /// Some target can't be watched, or sits on a network file system: wait only as long as the old poll.
    polling: bool,
    waker: Waker,
}

impl Watcher {
    pub fn new() -> Watcher {
        Watcher::with_waker(Waker::new())
    }

    pub fn with_waker(waker: Waker) -> Watcher {
        let sys = sys::Sys::new(waker.fd());
        Watcher { polling: sys.is_none(), sys, waker }
    }

    /// Watch a file: written, replaced, created, or deleted — and a symlink's target as well.
    pub fn file(&mut self, path: &Path) {
        self.target(path, false);
        if path.is_symlink()
            && let Ok(real) = path.canonicalize()
        {
            self.target(&real, false);
        }
    }

    /// Watch a folder's files: added, removed, renamed, or written.
    pub fn folder(&mut self, dir: &Path) {
        self.target(dir, true);
    }

    fn target(&mut self, path: &Path, folder: bool) {
        let Some(sys) = self.sys.as_mut() else { return };
        let home = if folder { path.to_path_buf() } else { parent(path) };
        // A folder that doesn't exist yet: watch the closest one that does, for the next folder down
        let (mut dir, mut name) =
            (home.clone(), if folder { None } else { path.file_name().map(OsString::from) });
        while !dir.is_dir() {
            name = dir.file_name().map(OsString::from);
            if !dir.pop() {
                self.polling = true;
                return;
            }
        }
        if !sys.dir(&dir, name.as_deref()) || !is_local(&dir) {
            self.polling = true;
            return;
        }
        if dir != home {
            return;
        }
        // kqueue says only that a folder's list changed — writes in place are the files' own events
        let files: Vec<PathBuf> = match folder {
            false => vec![path.to_path_buf()],
            true if sys::PER_FILE => {
                std::fs::read_dir(&dir).into_iter().flatten().flatten().map(|e| e.path()).collect()
            }
            true => Vec::new(),
        };
        for f in files {
            if !sys.file(&f) {
                self.polling = true;
            }
        }
    }

    /// Until something watched may have changed (then until its burst of writes settles), the backstop
    /// passes, or the waker is used.
    pub fn wait(&mut self) -> Woke {
        let first = self.wait_for(if self.polling { POLL } else { BACKSTOP });
        if first != Woke::Changed {
            return first;
        }
        let end = Instant::now() + BURST_MAX;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Woke::Changed;
            }
            match self.wait_for(QUIET.min(left)) {
                Woke::Changed => {}
                Woke::TimedOut => return Woke::Changed,
                Woke::Cancelled => return Woke::Cancelled,
            }
        }
    }

    fn wait_for(&mut self, timeout: Duration) -> Woke {
        match self.sys.as_mut() {
            Some(sys) => sys.wait(timeout),
            None => match self.waker.fd() {
                // No events here: the timer, and the waker
                Some(fd) => match poll_in(&[fd], timeout) {
                    Some(_) => Woke::Cancelled,
                    None => Woke::TimedOut,
                },
                None => {
                    std::thread::sleep(timeout);
                    Woke::TimedOut
                }
            },
        }
    }
}

fn parent(path: &Path) -> PathBuf {
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

/// poll(2) for input on `fds` — the first ready one's index, or None when `timeout` passes.
fn poll_in(fds: &[RawFd], timeout: Duration) -> Option<usize> {
    let end = Instant::now() + timeout;
    let mut pfds: Vec<libc::pollfd> =
        fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    loop {
        let left = end.saturating_duration_since(Instant::now());
        let ms = left.as_micros().div_ceil(1000).min(i32::MAX as u128) as i32;
        let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, ms) };
        if n > 0 {
            return pfds.iter().position(|p| p.revents != 0);
        }
        if n == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            return None;
        }
    }
}

pub fn statfs(dir: &Path) -> Option<libc::statfs> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statfs(c.as_ptr(), &mut st) } == 0).then_some(st)
}

/// Whether the kernel hears of every write to files in `dir` (not so on a network file system).
#[cfg(target_os = "macos")]
fn is_local(dir: &Path) -> bool {
    statfs(dir).is_none_or(|st| st.f_flags & libc::MNT_LOCAL as u32 != 0)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn is_local(dir: &Path) -> bool {
    /// statfs's `f_type` of file systems that other machines (or a VM's host) write to.
    const REMOTE: [u32; 12] = [
        0x6969,      // NFS
        0x517B,      // SMB
        0xFF53_4D42, // CIFS
        0xFE53_4D42, // SMB2
        0x6573_5546, // FUSE (sshfs, virtiofs)
        0x0102_1997, // 9p (VM shares, WSL's drives)
        0x00C3_6400, // Ceph
        0x5346_414F, // AFS
        0x7375_7245, // Coda
        0x564C,      // NCP
        0x0BD0_0BD0, // Lustre
        0x4750_4653, // GPFS
    ];
    statfs(dir).is_none_or(|st| !REMOTE.contains(&(st.f_type as u32)))
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android")))]
fn is_local(dir: &Path) -> bool {
    let _ = statfs;
    let _ = dir;
    true
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod sys {
    use super::*;
    use std::collections::HashMap;
    use std::os::unix::ffi::OsStrExt;

    /// Files are seen through their folder's watch.
    pub const PER_FILE: bool = false;

    const MASK: u32 = libc::IN_MODIFY
        | libc::IN_ATTRIB
        | libc::IN_CLOSE_WRITE
        | libc::IN_CREATE
        | libc::IN_DELETE
        | libc::IN_MOVED_FROM
        | libc::IN_MOVED_TO
        | libc::IN_DELETE_SELF
        | libc::IN_MOVE_SELF
        | libc::IN_ONLYDIR;
    /// The watch itself is gone or events were lost: whatever it was, look.
    const LOOK: u32 = libc::IN_Q_OVERFLOW | libc::IN_IGNORED | libc::IN_DELETE_SELF | libc::IN_MOVE_SELF;

    pub struct Sys {
        fd: OwnedFd,
        wake: Option<RawFd>,
        /// Watch → the names in it that matter (None: all of them).
        dirs: HashMap<i32, Option<Vec<OsString>>>,
    }

    impl Sys {
        pub fn new(wake: Option<RawFd>) -> Option<Sys> {
            let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
            (fd >= 0).then(|| Sys { fd: unsafe { OwnedFd::from_raw_fd(fd) }, wake, dirs: HashMap::new() })
        }

        pub fn dir(&mut self, dir: &Path, name: Option<&OsStr>) -> bool {
            let Ok(c) = std::ffi::CString::new(dir.as_os_str().as_bytes()) else { return false };
            let wd = unsafe { libc::inotify_add_watch(self.fd.as_raw_fd(), c.as_ptr(), MASK) };
            if wd < 0 {
                return false;
            }
            // The same folder again (two files in it) is the same watch: the names add up
            let names = self.dirs.entry(wd).or_insert_with(|| Some(Vec::new()));
            match (names.as_mut(), name) {
                (Some(v), Some(n)) => v.push(n.to_os_string()),
                (_, None) => *names = None,
                (None, Some(_)) => {}
            }
            true
        }

        pub fn file(&mut self, _: &Path) -> bool {
            true
        }

        pub fn wait(&mut self, timeout: Duration) -> Woke {
            let end = Instant::now() + timeout;
            loop {
                let fds = [self.fd.as_raw_fd(), self.wake.unwrap_or(-1)]; // (poll skips a negative fd)
                match poll_in(&fds, end.saturating_duration_since(Instant::now())) {
                    None => return Woke::TimedOut,
                    Some(1) => return Woke::Cancelled,
                    Some(_) if self.read() => return Woke::Changed,
                    Some(_) => {} // only other files in the folders
                }
            }
        }

        /// Read what's queued — whether any of it is about something watched.
        fn read(&mut self) -> bool {
            const HEAD: usize = std::mem::size_of::<libc::inotify_event>();
            let mut buf = [0u8; 4096];
            let mut hit = false;
            loop {
                let n = unsafe { libc::read(self.fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    return hit;
                }
                let mut at = 0;
                while at + HEAD <= n as usize {
                    let ev: libc::inotify_event =
                        unsafe { std::ptr::read_unaligned(buf[at..].as_ptr().cast()) };
                    let name = &buf[at + HEAD..(at + HEAD + ev.len as usize).min(n as usize)];
                    let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
                    at += HEAD + ev.len as usize;
                    hit |= ev.mask & LOOK != 0
                        || match self.dirs.get(&ev.wd) {
                            Some(Some(names)) => names.iter().any(|w| w.as_bytes() == name),
                            _ => true,
                        };
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
mod sys {
    use super::*;
    use std::os::unix::ffi::OsStrExt;

    /// A folder's event says only that its list changed: files are watched one by one, for writes in place.
    pub const PER_FILE: bool = true;
    /// Descriptors one watcher may hold — past this, poll. (macOS gives a process 256 by default.)
    const MAX_FDS: usize = 64;

    pub struct Sys {
        kq: OwnedFd,
        fds: Vec<OwnedFd>,
        /// Folders watched already (two open files in one folder: one descriptor).
        dirs: Vec<PathBuf>,
    }

    impl Sys {
        pub fn new(wake: Option<RawFd>) -> Option<Sys> {
            let kq = unsafe { libc::kqueue() };
            if kq < 0 {
                return None;
            }
            let sys = Sys { kq: unsafe { OwnedFd::from_raw_fd(kq) }, fds: Vec::new(), dirs: Vec::new() };
            if let Some(w) = wake {
                sys.add(w, libc::EVFILT_READ, 0, 0).then_some(())?;
            }
            Some(sys)
        }

        fn add(&self, fd: RawFd, filter: i16, flags: u16, fflags: u32) -> bool {
            let mut ev: libc::kevent = unsafe { std::mem::zeroed() };
            ev.ident = fd as libc::uintptr_t;
            ev.filter = filter;
            ev.flags = libc::EV_ADD | flags;
            ev.fflags = fflags;
            unsafe {
                libc::kevent(self.kq.as_raw_fd(), &ev, 1, std::ptr::null_mut(), 0, std::ptr::null()) == 0
            }
        }

        /// Open `path` only to hear about it (O_EVTONLY: doesn't keep a volume from unmounting).
        fn vnode(&mut self, path: &Path, fflags: u32) -> std::io::Result<()> {
            if self.fds.len() >= MAX_FDS {
                return Err(std::io::ErrorKind::QuotaExceeded.into());
            }
            let c = std::ffi::CString::new(path.as_os_str().as_bytes())?;
            let fd = unsafe { libc::open(c.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let fd = unsafe { OwnedFd::from_raw_fd(fd) };
            let fflags = fflags | libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE;
            if !self.add(fd.as_raw_fd(), libc::EVFILT_VNODE, libc::EV_CLEAR, fflags) {
                return Err(std::io::Error::last_os_error());
            }
            self.fds.push(fd);
            Ok(())
        }

        pub fn dir(&mut self, dir: &Path, _name: Option<&OsStr>) -> bool {
            if self.dirs.iter().any(|d| d == dir) {
                return true;
            }
            self.dirs.push(dir.to_path_buf());
            self.vnode(dir, libc::NOTE_WRITE).is_ok()
        }

        pub fn file(&mut self, path: &Path) -> bool {
            match self.vnode(path, libc::NOTE_WRITE | libc::NOTE_EXTEND | libc::NOTE_ATTRIB | libc::NOTE_LINK)
            {
                Ok(()) => true,
                // Not there (yet): its folder's watch sees it arrive
                Err(e) => e.kind() == std::io::ErrorKind::NotFound,
            }
        }

        pub fn wait(&mut self, timeout: Duration) -> Woke {
            let end = Instant::now() + timeout;
            loop {
                let left = end.saturating_duration_since(Instant::now());
                let ts = libc::timespec { tv_sec: left.as_secs() as _, tv_nsec: left.subsec_nanos() as _ };
                let mut evs: [libc::kevent; 8] = unsafe { std::mem::zeroed() };
                let n = unsafe {
                    libc::kevent(
                        self.kq.as_raw_fd(),
                        std::ptr::null(),
                        0,
                        evs.as_mut_ptr(),
                        evs.len() as _,
                        &ts,
                    )
                };
                if n > 0 {
                    // Only the waker's pipe is read-filtered
                    let cancelled = evs[..n as usize].iter().any(|e| e.filter == libc::EVFILT_READ);
                    return if cancelled { Woke::Cancelled } else { Woke::Changed };
                }
                if n == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
                    return Woke::TimedOut;
                }
            }
        }
    }
}

/// No file events here: every watcher is a timer.
#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
mod sys {
    use super::*;

    pub const PER_FILE: bool = false;

    pub struct Sys;

    impl Sys {
        pub fn new(_: Option<RawFd>) -> Option<Sys> {
            None
        }
        pub fn dir(&mut self, _: &Path, _: Option<&OsStr>) -> bool {
            false
        }
        pub fn file(&mut self, _: &Path) -> bool {
            false
        }
        pub fn wait(&mut self, _: Duration) -> Woke {
            Woke::TimedOut
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// How long an event that comes at once may take to arrive — generous for a slow runner, yet under
    /// `BACKSTOP` (a wait that ran out isn't one an event ended).
    const SOON: Duration = Duration::from_secs(3);

    fn tmp(name: &str) -> PathBuf {
        crate::scratch::Scratch::new(&format!("fswatch-{name}")).into_path()
    }

    fn watching(f: impl FnOnce(&mut Watcher)) -> Watcher {
        let mut w = Watcher::new();
        f(&mut w);
        assert!(
            !w.polling || cfg!(not(any(target_os = "linux", target_os = "macos"))),
            "events expected here"
        );
        w
    }

    #[test]
    fn written_in_place_replaced_and_deleted() {
        let dir = tmp("file");
        let path = dir.join("a.toml");
        std::fs::write(&path, "a").unwrap();
        // In place (append — no rename, no new inode)
        let mut w = watching(|w| w.file(&path));
        use std::io::Write as _;
        std::fs::OpenOptions::new().append(true).open(&path).unwrap().write_all(b"b").unwrap();
        assert_eq!(w.wait_for(SOON), Woke::Changed);
        // Replaced by a rename (how tarae and most editors save)
        let mut w = watching(|w| w.file(&path));
        std::fs::write(dir.join("tmp"), "c").unwrap();
        std::fs::rename(dir.join("tmp"), &path).unwrap();
        assert_eq!(w.wait_for(SOON), Woke::Changed);
        // Deleted
        let mut w = watching(|w| w.file(&path));
        std::fs::remove_file(&path).unwrap();
        assert_eq!(w.wait_for(SOON), Woke::Changed);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn created_under_folders_not_there_yet() {
        let dir = tmp("missing");
        let path = dir.join("sub/deeper/a.toml");
        let mut w = watching(|w| w.file(&path));
        std::fs::create_dir(dir.join("sub")).unwrap();
        assert_eq!(w.wait_for(SOON), Woke::Changed, "the first folder down appears");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_folders_files_added_and_written() {
        let dir = tmp("folder");
        std::fs::write(dir.join("a.wasm"), "a").unwrap();
        let mut w = watching(|w| w.folder(&dir));
        std::fs::write(dir.join("b.wasm"), "b").unwrap();
        assert_eq!(w.wait_for(SOON), Woke::Changed);
        let mut w = watching(|w| w.folder(&dir));
        use std::io::Write as _;
        std::fs::OpenOptions::new().append(true).open(dir.join("a.wasm")).unwrap().write_all(b"!").unwrap();
        assert_eq!(w.wait_for(SOON), Woke::Changed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Linux names the file: a neighbour's churn (git's index next to HEAD) doesn't wake anyone. HEAD is made
    /// without a descriptor (`mknod`): one written with `std::fs::write` says its close (`IN_CLOSE_WRITE`) only
    /// when the last copy of the descriptor goes, and a child another test forked holds one till its exec — under
    /// a full run that close came after the watch started, and woke it as HEAD's.
    #[cfg(target_os = "linux")]
    #[test]
    fn neighbours_dont_wake() {
        let dir = tmp("neighbour");
        let path = dir.join("HEAD");
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a plain call with a path of ours.
        assert_eq!(unsafe { libc::mknod(c.as_ptr(), libc::S_IFREG | 0o644, 0) }, 0, "{path:?}");
        let mut w = watching(|w| w.file(&path));
        std::fs::write(dir.join("index.lock"), "x").unwrap();
        std::fs::rename(dir.join("index.lock"), dir.join("index")).unwrap();
        assert_eq!(w.wait_for(Duration::from_millis(200)), Woke::TimedOut);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn waker_ends_the_wait_for_good() {
        let dir = tmp("waker");
        let waker = Waker::new();
        let mut w = Watcher::with_waker(waker.clone());
        w.file(&dir.join("a"));
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            waker.wake();
        });
        let start = Instant::now();
        assert_eq!(w.wait(), Woke::Cancelled);
        assert!(start.elapsed() < SOON);
        assert_eq!(w.wait(), Woke::Cancelled);
        t.join().unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Ten writes `GAP` apart are one burst: the one wake comes after the last of them — it took them all in.
    /// Then, once the burst's late events are drained (an event can reach the queue after the wake — seen
    /// on CI's macOS), nothing more wakes: an event is an edge, not a level. The gap is spun, not slept: CI's
    /// macOS stretched a 5 ms sleep to 25 ms (`QUIET`/2) in every try. A try where the writer still stalled
    /// for half of `QUIET` or more (preempted on a busy runner) wasn't one burst — tried again.
    #[test]
    fn a_burst_of_writes_is_one_wake() {
        const GAP: Duration = Duration::from_millis(2);
        const TRIES: u32 = 10;
        let dir = tmp("burst");
        let path = dir.join("a.log");
        for attempt in 1.. {
            std::fs::write(&path, "").unwrap();
            let mut w = watching(|w| w.file(&path));
            let p = path.clone();
            let t = std::thread::spawn(move || {
                use std::io::Write as _;
                let mut f = std::fs::OpenOptions::new().append(true).open(&p).unwrap();
                let (mut last, mut gap) = (Instant::now(), Duration::ZERO);
                for i in 0..10 {
                    f.write_all(b"line\n").unwrap();
                    if i > 0 {
                        gap = gap.max(last.elapsed());
                    }
                    last = Instant::now();
                    while last.elapsed() < GAP {
                        std::hint::spin_loop();
                    }
                }
                (last, gap)
            });
            assert_eq!(w.wait(), Woke::Changed);
            let woke = Instant::now();
            let (last, gap) = t.join().unwrap();
            if gap >= QUIET / 2 {
                assert!(
                    attempt < TRIES,
                    "the writer stalled ({gap:?} between writes) in each of {TRIES} tries"
                );
                continue;
            }
            assert!(woke >= last, "woke {:?} before the last write — the burst was split", last - woke);
            let drained = Instant::now() + BURST_MAX;
            while Instant::now() < drained && w.wait_for(QUIET) == Woke::Changed {}
            assert_eq!(
                w.wait_for(Duration::from_millis(200)),
                Woke::TimedOut,
                "nothing more after the burst"
            );
            break;
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
