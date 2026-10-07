//! A tunnel's two ends relayed by one thread (`poll`), with the one look the proxy takes inside a TLS tunnel: when
//! the server answers the first ClientHello with a TLS 1.3 HelloRetryRequest, the client sends a second one (RFC
//! 8446 §4.1.2), and it must name the server the first did — else the tunnel closes. The server's first
//! handshake bytes and that second ClientHello are held until they're judged; nothing else of the stream is read.

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
    let sides = [client, server];
    for s in &sides {
        if s.set_nonblocking(true).is_err() {
            return End::Done;
        }
    }
    let look = if name.is_some() { Look::Server(Vec::new()) } else { Look::Off };
    let plain = body.is_some();
    let mut f = Flow { look, body, pending: [Vec::new(), Vec::new()], name, hello_time, dropped: 0 };
    let end = pump(&sides, &mut f, first);
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

/// The relay's loop, until both sides are done or the proxy closes it.
fn pump(sides: &[TcpStream; 2], f: &mut Flow, first: &[u8]) -> End {
    // eof[i]: side i read to its end (the client of a plain request: once its body is whole); shut[i]: side i's
    // writing shut down; hup[i]: side i hung up — left out of `poll` while it isn't read, so a full other side
    // can't spin the loop
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
            let events =
                if read { libc::POLLIN } else { 0 } | if pending[i].is_empty() { 0 } else { libc::POLLOUT };
            let fd = if hup[i] && !read { -1 } else { sides[i].as_raw_fd() };
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
