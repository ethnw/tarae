//! A tunnel's two ends relayed by one thread (`poll`), with the one look the proxy takes inside a TLS tunnel: when
//! the server answers the first ClientHello with a TLS 1.3 HelloRetryRequest, the client sends a second one (RFC
//! 8446 §4.1.2), and it must name the server the first did — else the tunnel closes. The server's first
//! handshake bytes and that second ClientHello are held until they're judged; nothing else of the stream is read.
//!
//! Linux (step 6a, `target/sb/step6a-plan.md`): once nothing is left to judge and nothing waits in tarae, the
//! rest goes through a pipe each way by `splice` — the kernel moves it (R3: the direct rate, ≈ 2.0–2.5 GB/s, at
//! ≈ 0.4 CPU-s/GiB against the pump's ≈ 1 GB/s at ≈ 1). A TLS tunnel splices both ways once its look is off. Plain
//! http splices only the server's answer, once the request's body is whole: the client's side stays on the pump —
//! every byte of it counted against the body's framing, so one request in, one out holds and what follows it is
//! never read (an upload, a big PUT, stays pumped: decided 2026-10-07). macOS keeps the pump.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use super::parse;

/// The random a ServerHello carries when it's a HelloRetryRequest (RFC 8446 §4.1.3: SHA-256 of
/// "HelloRetryRequest").
const HRR_RANDOM: [u8; 32] = [
    0xcf, 0x21, 0xad, 0x74, 0xe5, 0x9a, 0x61, 0x11, 0xbe, 0x1d, 0x8c, 0x02, 0x1e, 0x65, 0xb8, 0x91, 0xc2,
    0xa2, 0x11, 0x16, 0x7a, 0xbb, 0x8c, 0x5e, 0x07, 0x9e, 0x09, 0xe2, 0xc8, 0xa8, 0x33, 0x9c,
];
/// What one read takes.
const CHUNK: usize = 16 * 1024;
/// The most bytes waiting for one side before the other isn't read any more.
const PENDING_MAX: usize = 64 * 1024;
/// A ServerHello's first bytes: record header, handshake header, version, random.
const SERVER_HELLO_HEAD: usize = 5 + 4 + 2 + 32;
/// A ChangeCipherSpec record a TLS 1.3 client may send before its second ClientHello (RFC 8446 §D.4).
const CCS: [u8; 6] = [20, 3, 3, 0, 1, 1];

/// How a relay ended.
#[derive(Debug, PartialEq, Eq)]
pub enum End {
    /// Both sides done.
    Done,
    /// Done — and this many bytes the client sent after its plain request were dropped (at least: what came
    /// after the request was no longer read).
    Dropped(usize),
    /// The proxy closed it: why, as the report says it.
    Closed(String),
}

/// What's held back to be judged.
enum Look {
    /// The server's first bytes, until they say whether they're a HelloRetryRequest.
    Server(Vec<u8>),
    /// After one: the client's second ClientHello, until its name is read — by the deadline.
    Client(Vec<u8>, Instant),
    Off,
}

/// Relays `client` and `server` until both are done, `first` — the client's bytes already read — going first.
/// `name` — a TLS tunnel's server name, from the first ClientHello — has a second ClientHello after a
/// HelloRetryRequest checked against it, within `hello_time`. `body` — a plain request's — makes it one request
/// in, one out: exactly that body goes on, then the server's side is half-closed and the client no longer read;
/// what the client sent after its request (one pipelined behind it) is dropped, and said (`End::Dropped`).
pub fn relay(
    client: TcpStream,
    server: TcpStream,
    name: Option<&str>,
    hello_time: Duration,
    first: &[u8],
    body: Option<parse::Body>,
) -> End {
    #[cfg(target_os = "linux")]
    let pipes = splice::pipes;
    #[cfg(not(target_os = "linux"))]
    let pipes = || Err(io::Error::from(io::ErrorKind::Unsupported));
    relay_with(client, server, name, hello_time, first, body, pipes)
}

/// Tests: each relay's spliced bytes by its client's peer — the test's own end of it (`spliced_by`): None while
/// it runs, its count once it ended. One entry an address: a relay starting there takes out what an earlier one
/// on that port left.
#[cfg(test)]
static SPLICED: std::sync::Mutex<Vec<(std::net::SocketAddr, Option<usize>)>> =
    std::sync::Mutex::new(Vec::new());

/// Tests: a relay whose client's peer is `peer` started — an earlier relay's leftover there goes.
#[cfg(test)]
pub(super) fn started(peer: std::net::SocketAddr) {
    let mut list = SPLICED.lock().unwrap_or_else(|e| e.into_inner());
    list.retain(|(a, _)| *a != peer);
    list.push((peer, None));
}

/// Tests: that relay ended, having spliced `n` bytes.
#[cfg(test)]
pub(super) fn ended(peer: std::net::SocketAddr, n: usize) {
    let mut list = SPLICED.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(e) = list.iter_mut().find(|(a, _)| *a == peer) {
        e.1 = Some(n);
    }
}

/// Tests: the bytes the relay whose client is the test's `end` (its local address) moved through pipes — that
/// relay's alone — once it ended (within 10 s; None: it didn't), taken out. Elsewhere than Linux, 0.
#[cfg(test)]
pub fn spliced_by(end: std::net::SocketAddr) -> Option<usize> {
    let by = Instant::now() + Duration::from_secs(10);
    loop {
        let mut list = SPLICED.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(i) = list.iter().position(|(a, n)| *a == end && n.is_some()) {
            return list.remove(i).1;
        }
        drop(list);
        if Instant::now() > by {
            return None;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// `relay`, the pipes to splice through made by `pipes` (an Err: it stays on the pump).
pub(super) fn relay_with(
    client: TcpStream,
    server: TcpStream,
    name: Option<&str>,
    hello_time: Duration,
    first: &[u8],
    body: Option<parse::Body>,
    pipes: fn() -> io::Result<Pipes>,
) -> End {
    let sides = [client, server];
    // (taken now: a socket shut down both ways has no peer to say by its end — macOS)
    #[cfg(test)]
    let peer = sides[0].peer_addr();
    #[cfg(test)]
    if let Ok(peer) = peer {
        started(peer);
    }
    for s in &sides {
        if s.set_nonblocking(true).is_err() {
            return End::Done;
        }
    }
    let look = if name.is_some() { Look::Server(Vec::new()) } else { Look::Off };
    let plain = body.is_some();
    let mut f = Flow { look, body, pending: [Vec::new(), Vec::new()], name, hello_time, dropped: 0 };
    let mut spliced = 0;
    let end = pump(&sides, &mut f, first, pipes, &mut spliced);
    #[cfg(test)]
    if let Ok(peer) = peer {
        ended(peer, spliced);
    }
    if end != End::Done || !plain {
        return end;
    }
    // (what came after the request, never read, is dropped too)
    let mut b = [0u8; 1];
    if matches!(sides[0].peek(&mut b), Ok(1..)) {
        f.dropped += 1;
    }
    match f.dropped {
        0 => End::Done,
        n => End::Dropped(n),
    }
}

/// A pipe a direction's bytes are spliced through (`pipes[i]`: from side i to the other), and how much it holds.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) struct Pipe {
    r: std::os::fd::OwnedFd,
    w: std::os::fd::OwnedFd,
    held: usize,
}

pub(super) type Pipes = [Pipe; 2];

/// Tests (Linux): a tunnel's pipes, as `relay` makes them — the measurement's spliced side.
#[cfg(all(test, target_os = "linux"))]
pub(super) use splice::pipes;

/// Tests (Linux): pipes of one page each (`F_SETPIPE_SZ`) — full after a few small segments, as a pipe of a user
/// past `pipe-user-pages-soft` is.
#[cfg(all(test, target_os = "linux"))]
pub(super) fn one_page_pipes() -> io::Result<Pipes> {
    let p = splice::pipes()?;
    for x in &p {
        // SAFETY: a pipe of ours; F_SETPIPE_SZ takes an int.
        if unsafe { libc::fcntl(x.w.as_raw_fd(), libc::F_SETPIPE_SZ, 4096) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(p)
}

/// The relay's loop, until both sides are done or the proxy closes it — handing over to `splice::run` once
/// nothing is left to judge or waits in tarae (Linux; `pipes` an Err: it pumps on), which counts what it moves
/// in `spliced`.
fn pump(
    sides: &[TcpStream; 2],
    f: &mut Flow,
    first: &[u8],
    pipes: fn() -> io::Result<Pipes>,
    spliced: &mut usize,
) -> End {
    #[cfg(not(target_os = "linux"))]
    let _ = (pipes, spliced);
    #[cfg(target_os = "linux")]
    let mut pipes = Some(pipes);
    // eof[i]: side i read to its end (the client of a plain request: once its body is whole); shut[i]: side i's
    // writing shut down; hup[i]: side i hung up — left out of `poll` while it's neither read nor written to, so a
    // full other side can't spin the loop. (macOS says `POLLHUP` for a side polled for reading once its peer only
    // half-closed, and it sticks here: what waits for that side still goes — it was dropped before. Polled only
    // for writing, `poll` waits until it takes bytes — no spin: `a_half_closed_slow_reader_doesnt_spin_the_relay`)
    let (mut eof, mut shut, mut hup) = ([false; 2], [false; 2], [false; 2]);
    let mut buf = vec![0u8; CHUNK];
    if let Some(why) = feed(f, 0, first, false) {
        return End::Closed(why);
    }
    let whole = |f: &Flow| f.body.as_ref().is_some_and(parse::Body::done);
    eof[0] |= whole(f);
    loop {
        let (look, pending) = (&f.look, &f.pending);
        let held = match look {
            Look::Server(b) | Look::Client(b, _) => b.len(),
            Look::Off => 0,
        };
        let mut fds: [libc::pollfd; 2] = std::array::from_fn(|i| {
            let other = 1 - i;
            let read = !eof[i] && pending[other].len() + held < PENDING_MAX;
            let write = !pending[i].is_empty();
            let events = if read { libc::POLLIN } else { 0 } | if write { libc::POLLOUT } else { 0 };
            let fd = if hup[i] && !read && !write { -1 } else { sides[i].as_raw_fd() };
            libc::pollfd { fd, events, revents: 0 }
        });
        let timeout = match look {
            Look::Client(_, by) => {
                by.saturating_duration_since(Instant::now()).as_millis().min(i32::MAX as u128) as i32
            }
            _ => -1,
        };
        if fds.iter().all(|f| f.events == 0 || f.fd < 0) {
            return End::Done;
        }
        // SAFETY: two pollfds of ours, their count.
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
        if r < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return End::Done;
        }
        if r == 0 && matches!(f.look, Look::Client(..)) {
            return End::Closed(format!(
                "no second ClientHello within {} s of the HelloRetryRequest",
                f.hello_time.as_secs()
            ));
        }
        if fds.iter().any(|f| f.revents & (libc::POLLERR | libc::POLLNVAL) != 0) {
            return End::Done;
        }
        for i in 0..2 {
            hup[i] |= fds[i].revents & libc::POLLHUP != 0;
            let ready = libc::POLLIN | libc::POLLHUP;
            if eof[i] || fds[i].revents & ready == 0 || fds[i].events & libc::POLLIN == 0 {
                continue;
            }
            let got = match (&sides[i]).read(&mut buf) {
                Ok(n) => n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => {
                    continue;
                }
                Err(_) => 0,
            };
            if got == 0 {
                eof[i] = true;
            }
            let bytes = &buf[..got];
            if let Some(why) = feed(f, i, bytes, got == 0) {
                return End::Closed(why);
            }
            // (a plain request's body whole: the client isn't read any more, the server's side half-closes)
            eof[0] |= whole(f);
        }
        for (side, pending) in sides.iter().zip(&mut f.pending) {
            if pending.is_empty() {
                continue;
            }
            match (&*side).write(pending) {
                Ok(n) => {
                    pending.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock || e.kind() == io::ErrorKind::Interrupted => {
                }
                // (its reader gone: nothing more goes either way)
                Err(_) => return End::Done,
            }
        }
        let holding = !matches!(f.look, Look::Off);
        for i in 0..2 {
            let other = 1 - i;
            if eof[other] && f.pending[i].is_empty() && !shut[i] && !holding {
                let _ = sides[i].shutdown(Shutdown::Write);
                shut[i] = true;
            }
        }
        if eof == [true, true] && shut == [true, true] {
            return End::Done;
        }
        // Nothing left to judge, nothing waiting in tarae (what was read went first, in order) — and for plain
        // http, its request whole, its client no longer read: the rest is spliced (`eof` says which sides still
        // read: a plain request's client never again)
        #[cfg(target_os = "linux")]
        if matches!(f.look, Look::Off)
            && f.pending.iter().all(Vec::is_empty)
            && (f.body.is_none() || eof[0])
            && let Some(make) = pipes.take()
        {
            match make() {
                Ok(p) => return splice::run(sides, p, [eof, shut, hup], spliced),
                Err(e) => splice::unavailable(&e),
            }
        }
    }
}

/// Where a relay's bytes go: what's held to be judged, a plain request's body followed, what waits for each side.
struct Flow<'a> {
    look: Look,
    body: Option<parse::Body>,
    /// pending[i]: what goes to side i.
    pending: [Vec<u8>; 2],
    name: Option<&'a str>,
    hello_time: Duration,
    /// The client's bytes after its plain request's body, dropped.
    dropped: usize,
}

/// Bytes read from side `from` (0 the client, 1 the server; `end`: it read to its end) put where they go — held
/// while `look` judges them, a client's past its request's `body` dropped. Some: why the tunnel closes.
fn feed(f: &mut Flow, from: usize, bytes: &[u8], end: bool) -> Option<String> {
    let Flow { look, body, pending, name, hello_time, dropped } = f;
    let (name, hello_time) = (*name, *hello_time);
    let to = 1 - from;
    let next = match look {
        Look::Server(b) if from == 1 => {
            b.extend_from_slice(bytes);
            match server_said(b, end) {
                Said::More => return None,
                Said::Retry => Look::Client(Vec::new(), Instant::now() + hello_time),
                Said::Other => Look::Off,
                Said::Cut => return Some("a ServerHello cut shorter than its random".into()),
            }
        }
        Look::Client(b, _) if from == 0 => {
            b.extend_from_slice(bytes);
            // (a ChangeCipherSpec may come first, for middleboxes: passed as it is)
            let mut rest = &b[..];
            while rest.starts_with(&CCS) {
                rest = &rest[CCS.len()..];
            }
            let first = name.unwrap_or_default();
            match parse::server_name(rest) {
                Ok(None) if !end => return None,
                Ok(None) => return Some("it ended before its second ClientHello was whole".into()),
                Ok(Some(n)) if n == first => Look::Off,
                Ok(Some(n)) => return Some(format!("its second ClientHello names {n}, not {first}")),
                Err(e) => return Some(format!("its second ClientHello: {}", e.said())),
            }
        }
        _ => {
            let n = match body.as_mut().filter(|_| from == 0) {
                Some(b) => match b.take(bytes) {
                    Ok(n) => n,
                    Err(why) => return Some(format!("its request's body: {why}")),
                },
                None => bytes.len(),
            };
            pending[to].extend_from_slice(&bytes[..n]);
            *dropped += bytes.len() - n;
            return None;
        }
    };
    // What was held goes on as it came
    if let Look::Server(b) | Look::Client(b, _) = look {
        pending[to].append(b);
    }
    *look = next;
    None
}

/// The rest of a tunnel by `splice` (Linux): each direction through its own pipe, the kernel moving the bytes —
/// tarae never reads them. The pump's rules as they were: a side read while it isn't at its end and its pipe has
/// room; a side read to its end (`splice` gave 0) whose pipe is empty has the other's writing shut down; done once
/// both are. A pipe's room isn't its bytes: it holds 16 slots, one per small segment a socket gives it (two pages
/// once the user is past `pipe-user-pages-soft`), so it can be full well under 64 KiB — a splice into it that
/// says "not now" while it holds bytes leaves that side unread until the pipe has given some out (else `poll`
/// says the socket is ready, the splice says not now, and a slow far side spins the loop).
#[cfg(target_os = "linux")]
mod splice {
    use std::io;
    use std::net::{Shutdown, TcpStream};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    use super::{End, Pipe, Pipes};

    /// What one pipe holds at most (Linux's default size: no `F_SETPIPE_SZ`) — in bytes; it can be full sooner
    /// (`full`).
    const PIPE_CAP: usize = 64 * 1024;

    /// A pipe each way.
    pub fn pipes() -> io::Result<Pipes> {
        Ok([pipe()?, pipe()?])
    }

    fn pipe() -> io::Result<Pipe> {
        let mut fds = [0; 2];
        // SAFETY: a buffer of two descriptors; both are ours once it returns 0.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: just made, ours alone.
        let (r, w) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        Ok(Pipe { r, w, held: 0 })
    }

    /// Pipes that couldn't be made (descriptors run short: `EMFILE`) — the tunnel is pumped, never refused; said in
    /// the log once.
    pub fn unavailable(e: &io::Error) {
        static SAID: std::sync::Once = std::sync::Once::new();
        let msg = format!("a tunnel's pipes couldn't be made ({e}): pumped instead");
        SAID.call_once(|| crate::log::line("sandbox", "warning", &msg));
    }

    /// One `splice` of at most `len` bytes, neither end waited on: Some(n) moved (0: the source's end), None: not
    /// now (`EAGAIN`, `EINTR`), Err: it failed.
    fn once(from: i32, to: i32, len: usize) -> io::Result<Option<usize>> {
        let flags = libc::SPLICE_F_MOVE | libc::SPLICE_F_NONBLOCK;
        // SAFETY: descriptors of ours, no offsets (sockets and pipes have none).
        let n = unsafe { libc::splice(from, std::ptr::null_mut(), to, std::ptr::null_mut(), len, flags) };
        if n >= 0 {
            return Ok(Some(n as usize));
        }
        let e = io::Error::last_os_error();
        match e.kind() {
            io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted => Ok(None),
            _ => Err(e),
        }
    }

    /// SIGPIPE held on this thread: `splice` has no `MSG_NOSIGNAL`, so one into a socket whose peer reset would
    /// raise it — tarae lives through that only while the signal is ignored, and a restart's window resets it
    /// (`restart::Signals`). Held, the splice fails `EPIPE` instead. Left held: the thread is the connection's and
    /// ends with it, a pending SIGPIPE with it.
    fn hold_sigpipe() {
        // SAFETY: a signal set of ours, filled and passed for this thread's own mask.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGPIPE);
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        }
    }

    /// The splice loop — `eof`, `shut`, `hup` as the pump left them; what it moves to the sides counted in `spliced`.
    pub fn run(sides: &[TcpStream; 2], mut pipes: Pipes, state: [[bool; 2]; 3], spliced: &mut usize) -> End {
        hold_sigpipe();
        let [mut eof, mut shut, mut hup] = state;
        // full[i]: pipe i said "not now" while it held bytes — side i unread until it gives some out
        let mut full = [false; 2];
        loop {
            let mut fds: [libc::pollfd; 2] = std::array::from_fn(|i| {
                let read = !eof[i] && !full[i] && pipes[i].held < PIPE_CAP;
                let write = pipes[1 - i].held > 0;
                let events = if read { libc::POLLIN } else { 0 } | if write { libc::POLLOUT } else { 0 };
                let fd = if hup[i] && !read && !write { -1 } else { sides[i].as_raw_fd() };
                libc::pollfd { fd, events, revents: 0 }
            });
            if fds.iter().all(|f| f.events == 0 || f.fd < 0) {
                return End::Done;
            }
            // SAFETY: two pollfds of ours, their count.
            let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
            if r < 0 {
                if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return End::Done;
            }
            if fds.iter().any(|f| f.revents & (libc::POLLERR | libc::POLLNVAL) != 0) {
                return End::Done;
            }
            for i in 0..2 {
                hup[i] |= fds[i].revents & libc::POLLHUP != 0;
                let ready = libc::POLLIN | libc::POLLHUP;
                if eof[i] || fds[i].revents & ready == 0 || fds[i].events & libc::POLLIN == 0 {
                    continue;
                }
                let room = PIPE_CAP - pipes[i].held;
                // (a read that fails is its end, as the pump takes it)
                match once(sides[i].as_raw_fd(), pipes[i].w.as_raw_fd(), room) {
                    Ok(Some(0)) | Err(_) => eof[i] = true,
                    Ok(Some(n)) => pipes[i].held += n,
                    // (the socket was ready: holding bytes, it's the pipe that's full — empty, the socket's own)
                    Ok(None) => full[i] = pipes[i].held > 0,
                }
            }
            for (i, side) in sides.iter().enumerate() {
                let from = 1 - i;
                if pipes[from].held == 0 {
                    continue;
                }
                match once(pipes[from].r.as_raw_fd(), side.as_raw_fd(), pipes[from].held) {
                    Ok(Some(n)) => {
                        pipes[from].held -= n;
                        full[from] &= n == 0;
                        *spliced += n;
                    }
                    Ok(None) => {}
                    // (its reader gone: nothing more goes either way)
                    Err(_) => return End::Done,
                }
            }
            for i in 0..2 {
                let other = 1 - i;
                if eof[other] && pipes[other].held == 0 && !shut[i] {
                    let _ = sides[i].shutdown(Shutdown::Write);
                    shut[i] = true;
                }
            }
            if eof == [true, true] && shut == [true, true] {
                return End::Done;
            }
        }
    }
}

/// What the server's first bytes say.
enum Said {
    More,
    /// A HelloRetryRequest.
    Retry,
    /// Anything else (a ServerHello, an alert, not TLS): nothing more to look at.
    Other,
    /// A handshake record too short to hold a ServerHello's random: fails closed.
    Cut,
}

fn server_said(b: &[u8], end: bool) -> Said {
    match b {
        [] if !end => Said::More,
        [t, ..] if *t != 22 => Said::Other,
        [_, _, _, hi, lo, rest @ ..] => {
            let len = usize::from(u16::from_be_bytes([*hi, *lo]));
            match rest.first() {
                Some(&kind) if kind != 2 => Said::Other,
                _ if len < SERVER_HELLO_HEAD - 5 => Said::Cut,
                _ if b.len() < SERVER_HELLO_HEAD && !end => Said::More,
                _ if b.len() < SERVER_HELLO_HEAD => Said::Other,
                _ if b[11..SERVER_HELLO_HEAD] == HRR_RANDOM => Said::Retry,
                _ => Said::Other,
            }
        }
        _ if !end => Said::More,
        _ => Said::Other,
    }
}
