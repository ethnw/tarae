use super::*;
use crate::sandbox::watchdog::Watchdog;
#[cfg(target_os = "linux")]
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::Ordering::Relaxed;

/// Each test here ends within this, or its watchdog ends the binary saying what it waited for.
const LIMIT: Duration = Duration::from_secs(20);

/// What the scans of `staged` copies see below the tree, as the test sets it.
#[derive(Default)]
struct Stage {
    /// The sockets held.
    held: Arc<Mutex<HashSet<u64>>>,
    /// A process there that can't be read.
    unreadable: Arc<std::sync::atomic::AtomicBool>,
    /// A socket kept while the next scan reads (once).
    mid: Arc<Mutex<Option<OwnedFd>>>,
}

impl Stage {
    fn hold(&self, inodes: &[u64]) {
        *self.held.lock().unwrap() = inodes.iter().copied().collect();
    }
}

/// Copies with no thread of their own whose `collect` — the real one — reads the `Stage` for what's below the tree.
fn staged() -> (Arc<Copies>, Stage) {
    let stage = Stage::default();
    let me: Arc<std::sync::OnceLock<Weak<Copies>>> = Arc::default();
    let (held, unreadable, mid, at) =
        (stage.held.clone(), stage.unreadable.clone(), stage.mid.clone(), me.clone());
    let c = Copies::looking(move |_| {
        if let Some(fd) = mid.lock().unwrap().take() {
            at.get().and_then(Weak::upgrade).unwrap().keep(&fd).unwrap();
        }
        let unreadable = unreadable.load(Relaxed);
        Look { held: held.lock().unwrap().clone(), read: 0, unreadable }
    });
    let _ = me.set(Arc::downgrade(&c));
    (c, stage)
}

/// A copy goes once `STRIKES` scans in a row found no process holding its socket: one scan that finds it held
/// starts the count again, and a copy kept while a scan runs isn't struck by that scan (its holder may not have it
/// yet). Through `collect` itself, what's below the tree stood in for.
#[test]
fn a_copy_goes_after_two_scans_that_find_it_unheld() {
    let _w = Watchdog::new("copies::a_copy_goes_after_two_scans_that_find_it_unheld", LIMIT);
    let (c, stage) = staged();
    let (a, b) = UnixStream::pair().unwrap();
    let (a, b): (OwnedFd, OwnedFd) = (a.into(), b.into());
    c.keep(&a).unwrap();
    c.keep(&b).unwrap();
    let (ia, ib) = (inode(&a).unwrap(), inode(&b).unwrap());
    assert_eq!(c.collect(), 0, "one strike each");
    stage.hold(&[ib]);
    assert_eq!(c.collect(), 1, "a: two in a row, gone; b: held, its count again");
    assert!(c.get(ia).is_none() && c.get(ib).is_some());
    stage.hold(&[]);
    assert_eq!(c.collect(), 0, "b: one strike");
    // (kept while a scan reads: not struck by it)
    let (d, _) = UnixStream::pair().unwrap();
    let d: OwnedFd = d.into();
    let id = inode(&d).unwrap();
    *stage.mid.lock().unwrap() = Some(d);
    assert_eq!(c.collect(), 1, "b goes; the one kept during the scan stays");
    assert!(c.get(id).is_some());
    // (and its count starts with the next scan: one strike there — not its second)
    assert_eq!(c.collect(), 0, "the new one: its first strike");
    assert!(c.get(id).is_some());
    assert_eq!(socket_inode("socket:[12345]"), Some(12345));
    assert_eq!((socket_inode("pipe:[1]"), socket_inode("/dev/null")), (None, None));
}

/// The scans are paced by what happens: `INTERVAL` once a copy's kept; twice as long after each scan that strikes
/// nothing, up to `QUIET_MAX` (a quiet run's copies cost a scan a second); back to `INTERVAL` after one that strikes
/// or drops a copy, and when the run does something (`stir`). Through `collect` itself.
#[test]
fn the_scans_slow_down_while_nothing_happens() {
    let _w = Watchdog::new("copies::the_scans_slow_down_while_nothing_happens", LIMIT);
    let (c, stage) = staged();
    let (a, _b) = UnixStream::pair().unwrap();
    let a: OwnedFd = a.into();
    c.keep(&a).unwrap();
    assert_eq!(c.interval(), INTERVAL, "a copy kept");
    stage.hold(&[inode(&a).unwrap()]);
    let scan = || {
        c.collect();
        c.interval()
    };
    let quiet: Vec<Duration> = (0..6).map(|_| scan()).collect();
    let ms = |n| Duration::from_millis(n);
    assert_eq!(quiet, [ms(100), ms(200), ms(400), ms(800), QUIET_MAX, QUIET_MAX], "held: nothing happens");
    c.stir();
    assert_eq!(c.interval(), INTERVAL, "the run did something");
    assert_eq!(scan(), ms(100));
    stage.hold(&[]);
    assert_eq!(scan(), INTERVAL, "a strike: soon again, for its second");
}

/// A scan that couldn't read a process below the tree strikes nothing — that process may hold any copy's socket (a
/// non-dumpable one's `fd` is refused): copies wait, never go early; once it's readable (or gone) they're collected
/// as before. A process that ended meanwhile (`ENOENT`, `ESRCH`) holds nothing; any other failure is unreadable.
#[test]
fn a_process_that_cant_be_read_keeps_every_copy() {
    let _w = Watchdog::new("copies::a_process_that_cant_be_read_keeps_every_copy", LIMIT);
    let (c, stage) = staged();
    let (a, _b) = UnixStream::pair().unwrap();
    let a: OwnedFd = a.into();
    c.keep(&a).unwrap();
    stage.unreadable.store(true, Relaxed);
    for n in 0..5 {
        assert_eq!(c.collect(), 0, "scan {n}: someone unread may hold it");
    }
    assert!(c.get(inode(&a).unwrap()).is_some());
    stage.unreadable.store(false, Relaxed);
    assert_eq!((c.collect(), c.collect()), (0, 1), "readable again: two strikes, gone");
    let e = |n| io::Error::from_raw_os_error(n);
    assert_eq!([failed(&e(libc::ENOENT)), failed(&e(libc::ESRCH))], [Failed::Gone, Failed::Gone]);
    assert_eq!([failed(&e(libc::EACCES)), failed(&e(libc::EPERM))], [Failed::Unreadable, Failed::Unreadable]);
}

/// A bind that finds its port taken waits only where a copy just went (`bind_again`): one the scanner let go a moment
/// before counts as much as `collect_now`'s own (the race: the scanner took it, `collect_now` found none, and the bind
/// wasn't tried again); none gone, the port is someone else's — no wait, the bind not tried again; still taken past
/// `RELEASE_WAIT`, its error.
#[test]
fn a_bind_waits_only_where_a_copy_just_went() {
    let _w = Watchdog::new("copies::a_bind_waits_only_where_a_copy_just_went", LIMIT);
    let (c, _nothing_held) = staged();
    let tries = std::cell::Cell::new(0);
    let taken_once = || {
        tries.set(tries.get() + 1);
        if tries.get() == 1 { Err("taken") } else { Ok(tries.get()) }
    };
    assert_eq!(c.bind_again(taken_once, |_| true), None, "no copy went: not tried");
    assert_eq!(tries.get(), 0);
    let (a, _b) = UnixStream::pair().unwrap();
    c.keep(&OwnedFd::from(a)).unwrap();
    assert_eq!((c.collect(), c.collect()), (0, 1), "the scanner let it go");
    assert_eq!(c.bind_again(taken_once, |_| true), Some(Ok(2)), "tried again, and again");
    let since = Instant::now();
    assert_eq!(c.bind_again(|| Err::<(), _>("taken"), |_| true), Some(Err("taken")));
    assert!(since.elapsed() >= RELEASE_WAIT, "{:?}", since.elapsed());
}

/// `fd` sent on `via` with one byte (`SCM_RIGHTS`) — left in the peer's queue, unread.
#[cfg(target_os = "linux")]
fn send_fd(via: &OwnedFd, fd: &OwnedFd) {
    let mut byte = *b"F";
    let mut iov = libc::iovec { iov_base: byte.as_mut_ptr().cast(), iov_len: 1 };
    let mut control = [0u64; 4];
    // SAFETY: a zeroed header pointed at buffers of ours, the one control message within its buffer.
    let sent = unsafe {
        let mut m: libc::msghdr = std::mem::zeroed();
        (m.msg_iov, m.msg_iovlen) = (&raw mut iov, 1);
        m.msg_control = control.as_mut_ptr().cast();
        m.msg_controllen = libc::CMSG_SPACE(4) as _;
        let c = libc::CMSG_FIRSTHDR(&m);
        ((*c).cmsg_level, (*c).cmsg_type, (*c).cmsg_len) =
            (libc::SOL_SOCKET, libc::SCM_RIGHTS, libc::CMSG_LEN(4) as _);
        std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<i32>(), fd.as_raw_fd());
        libc::sendmsg(via.as_raw_fd(), &m, 0)
    };
    assert_eq!(sent, 1, "{}", io::Error::last_os_error());
}

/// A socket passed in a message is held by no process while the message waits in the receiver's queue: its copy
/// stays (`in_flight`) however many scans find it unheld, until the receiver has read what the socket it went on
/// sent (`SIOCOUTQ` back to 0) — then it's collected as any — or until `IN_FLIGHT_MAX`, unread or not. Real sockets:
/// the queue is the kernel's.
#[cfg(target_os = "linux")]
#[test]
fn a_passed_socket_is_kept_while_in_flight() {
    let w = Watchdog::new("copies::a_passed_socket_is_kept_while_in_flight", LIMIT);
    let (c, _nothing_held) = staged();
    let (via, peer) = UnixStream::pair().unwrap();
    let via: OwnedFd = via.into();
    let passed = |c: &Arc<Copies>| {
        let (x, _) = UnixStream::pair().unwrap();
        let x: OwnedFd = x.into();
        c.keep(&x).unwrap();
        send_fd(&via, &x);
        inode(&x).unwrap()
    };
    let ino = passed(&c);
    c.in_flight(ino, &via);
    for n in 0..4 {
        assert_eq!(c.collect(), 0, "scan {n}: in the receiver's queue, held by no process — kept");
    }
    w.waiting_for("the receiver's read");
    (&peer).read_exact(&mut [0u8]).unwrap();
    assert_eq!((c.collect(), c.collect()), (0, 1), "read: out of flight, then two strikes");
    // (the bound: in flight that long, collected as any though still unread)
    let ino = passed(&c);
    c.in_flight_ago(ino, &via, IN_FLIGHT_MAX);
    assert_eq!((c.collect(), c.collect()), (0, 1), "past IN_FLIGHT_MAX");
    let _ = (&peer).write_all(b"x");
}

/// A child of `tree` (this test's process) holding `fd` as its stdin until it's killed.
/// A tree of the test's own, as a keeper and its run: a shell (the tree — not read) whose child `sleep` holds a
/// socket as its stdin. Not this test's process: other tests' keepers run below that, non-dumpable — unreadable
/// under Docker's default profile, and a scan that can't read one strikes nothing.
#[cfg(target_os = "linux")]
struct Held {
    sh: std::process::Child,
    sleep: u32,
}

#[cfg(target_os = "linux")]
impl Held {
    fn new(fd: &OwnedFd) -> Held {
        let stdin = std::process::Stdio::from(fd.try_clone().unwrap());
        // (a command after it: the shell doesn't exec the sleep, it stays its parent)
        let sh =
            std::process::Command::new("sh").args(["-c", "sleep 30; exit 0"]).stdin(stdin).spawn().unwrap();
        let by = Instant::now() + Duration::from_secs(5);
        let sleep = loop {
            if let Some(&pid) = below(sh.id()).0.first() {
                break pid;
            }
            assert!(Instant::now() < by, "the shell's sleep never came");
            std::thread::sleep(Duration::from_millis(5));
        };
        Held { sh, sleep }
    }

    fn tree(&self) -> u32 {
        self.sh.id()
    }

    /// The sleep killed and the shell's end waited for: nobody holds the socket but tarae's copy.
    fn end(mut self) {
        // SAFETY: a signal to our shell's child.
        unsafe { libc::kill(self.sleep as libc::pid_t, libc::SIGKILL) };
        self.sh.wait().unwrap();
    }
}

/// The late FIN (M8's caveat, pinned): while tarae holds its copy, the end of a socket the run held isn't seen by
/// its peer — the run's process gone, its peer reads nothing yet — until the copies are collected: one scan that
/// finds it unheld isn't enough, the second lets it go and the peer sees the end. Below the tree: the tree's child;
/// the tree itself isn't read.
#[cfg(target_os = "linux")]
#[test]
fn a_closed_sockets_end_comes_when_its_copy_is_collected() {
    let w = Watchdog::new("copies::a_closed_sockets_end_comes_when_its_copy_is_collected", LIMIT);
    let (ours, theirs) = UnixStream::pair().unwrap();
    let theirs: OwnedFd = theirs.into();
    w.waiting_for("the tree's start");
    let held = Held::new(&theirs);
    let c = Copies::unscanned(held.tree());
    c.keep(&theirs).unwrap();
    drop(theirs);
    w.waiting_for("a scan of the tree");
    assert_eq!(c.collect(), 0, "held by the child");
    w.waiting_for("the tree's end");
    held.end();
    ours.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let mut b = [0u8; 1];
    w.waiting_for("a read of the peer's end (200 ms)");
    let read = (&ours).read(&mut b);
    assert!(read.is_err(), "the copy holds it open: no end yet ({read:?})");
    w.waiting_for("two scans");
    assert_eq!(c.collect(), 0, "one strike");
    assert_eq!(c.collect(), 1, "two: gone");
    w.waiting_for("the peer's end, once the copy's gone");
    assert_eq!((&ours).read(&mut b).unwrap(), 0, "the end, once the copy's gone");
}

/// Whether a bind's error says its port is taken.
#[cfg(target_os = "linux")]
fn taken(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::AddrInUse
}

/// The port's release (pinned): a UDP port the run bound stays bound while tarae holds the copy — a bind of it
/// fails `EADDRINUSE` — until it's collected; a bind that finds it taken (`bind_again`) has the copies collected at
/// once, the scans' two strikes in one go, and binds.
#[cfg(target_os = "linux")]
#[test]
fn a_closed_sockets_port_is_free_once_collected_now() {
    let w = Watchdog::new("copies::a_closed_sockets_port_is_free_once_collected_now", LIMIT);
    let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let at = s.local_addr().unwrap();
    let s: OwnedFd = s.into();
    w.waiting_for("the tree's start and end");
    let held = Held::new(&s);
    let c = Copies::unscanned(held.tree());
    c.keep(&s).unwrap();
    drop(s);
    held.end();
    let again = std::net::UdpSocket::bind(at);
    assert_eq!(again.err().map(|e| e.kind()), Some(io::ErrorKind::AddrInUse), "the copy holds the port");
    w.waiting_for("bind_again");
    let bound = c.bind_again(|| std::net::UdpSocket::bind(at), taken);
    assert!(bound.as_ref().is_some_and(Result::is_ok), "free once collected: {bound:?}");
    assert_eq!(c.len(), 0);
}

/// The flake that found it (2026-10-10, under the suite's load): the copy collected, the port still taken a moment —
/// the socket held by a process forked meanwhile (any process tarae, here the test binary, starts holds every one of
/// its descriptors until its exec is through), free 63 µs later. A bind waits for that (`RELEASE_WAIT`): here a
/// process of the test's own, outside the tree, holds the socket 50 ms after its copy went — the bind gets the port
/// once it lets go, well within the wait.
#[cfg(target_os = "linux")]
#[test]
fn a_bind_waits_for_a_process_still_holding_a_copys_socket() {
    let w = Watchdog::new("copies::a_bind_waits_for_a_process_still_holding_a_copys_socket", LIMIT);
    let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let at = s.local_addr().unwrap();
    let s: OwnedFd = s.into();
    w.waiting_for("the tree's start and end");
    let held = Held::new(&s);
    let c = Copies::unscanned(held.tree());
    c.keep(&s).unwrap();
    held.end();
    w.waiting_for("the late holder's start");
    let stdin = std::process::Stdio::from(s);
    let mut late = std::process::Command::new("sleep").arg("0.05").stdin(stdin).spawn().unwrap();
    let since = Instant::now();
    w.waiting_for("bind_again (≤ RELEASE_WAIT)");
    let bound = c.bind_again(|| std::net::UdpSocket::bind(at), taken);
    let took = since.elapsed();
    late.wait().unwrap();
    assert!(
        bound.as_ref().is_some_and(Result::is_ok),
        "bound once the late holder let go: {bound:?} after {took:?}"
    );
    assert!(took >= Duration::from_millis(20), "it waited for the holder: {took:?}");
}

/// The scanner, on its own: a copy whose holders are gone goes within `STRIKES` intervals (and a scan's time), the
/// peer then seeing the end — not before the first interval.
#[cfg(target_os = "linux")]
#[test]
fn the_scans_let_a_closed_socket_go_within_two_intervals() {
    let w = Watchdog::new("copies::the_scans_let_a_closed_socket_go_within_two_intervals", LIMIT);
    let (ours, theirs) = UnixStream::pair().unwrap();
    let theirs: OwnedFd = theirs.into();
    w.waiting_for("the tree's start and end");
    let held = Held::new(&theirs);
    let c = Copies::new(held.tree());
    c.keep(&theirs).unwrap();
    drop(theirs);
    held.end();
    let gone = Instant::now();
    ours.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut b = [0u8; 1];
    w.waiting_for("the peer's end, by the scanner (5 s)");
    assert_eq!((&ours).read(&mut b).unwrap(), 0, "the end comes");
    let took = gone.elapsed();
    let most = INTERVAL * u32::from(STRIKES + 1) + Duration::from_millis(200);
    assert!(took < most, "within {most:?}: {took:?}");
    let _ = (&ours).write_all(b"x");
    assert_eq!(c.len(), 0);
}
