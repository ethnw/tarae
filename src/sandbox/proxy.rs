//! The egress proxy (`docs/design/sandbox-keeper.md` §The egress proxy, `target/sb/step5-plan.md`): one, threads in
//! tarae, for both systems' sandboxed runs. It talks only in typed, owned messages (D23): a run's connection (Linux:
//! the proxy's end of the supervisor's TCP pair) or a run's own listener and token (macOS) come in, each with the
//! run's rules as a read-only snapshot and the sender its refusals go out on; the user's answers come in from the
//! main loop, and its questions go out to it. Nothing mutable is shared with anyone — the proxy knows only what a
//! message carried.
//!
//! One dispatcher thread holds the proxy's own state: the runs it serves, the answers it was told, the questions
//! out, the tunnels counted (at most `Limits::per_session` a session and `Limits::total` in all — past either, 503).
//! Each connection is a thread of its own, start to end: its head (`parse::head`) within `head_time`, the token
//! (macOS), the host decided — the rules, else the user's answer, held up to `hold` —, its name looked up once and
//! every address checked, the connection made to the address checked (`egress::connect`), then for a tunnel the
//! ClientHello's server name (`parse::server_name`) within `hello_time`, which must be the CONNECT host, and the
//! relay (`relay::relay`, one thread with `poll`; a second ClientHello after a HelloRetryRequest names the same
//! server). Every refusal is said on the run's sender, as the supervisor's are.

pub mod egress;
pub mod parse;
pub mod relay;

use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use egress::{Egress, Refused, Resolve};
use parse::{Auth, Host, Request};

use super::Denial;

/// The op a proxy's refusal is said with in a run's report.
pub const OP: &str = "network-proxy";

/// The op of a refusal the user's answer would lift — the host isn't among the run's, and the user said no, didn't
/// answer in time, or had too many questions up (`:sandbox-blocked` offers that host).
pub const HOST_OP: &str = "network-host";

/// A run's connection, as the supervisor hands it (Linux): the proxy's end of the TCP pair whose other end is
/// in the run's place.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub struct Connection {
    pub fd: OwnedFd,
    pub run: u64,
    pub session: u64,
    /// What the report names as the one refused (the run's process).
    pub process: String,
    pub rules: Arc<Egress>,
    pub told: Sender<Denial>,
}

/// How far an answer of the user's reaches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The run that asked (`y`).
    Run,
    /// Every run of the session (`s`).
    Session,
    /// The project, from now on (`A`): the session's here, the main loop keeps it for the next runs' rules.
    Project,
    /// Refused, for the session (`n`).
    Refused,
}

/// What comes in.
#[cfg_attr(not(test), allow(dead_code))] // (each system sends its own runs')
pub enum ToProxy {
    /// Linux: a run's connection, from the supervisor.
    Connection(Connection),
    /// macOS: a run's own listener (`127.0.0.1:0`, its port the only one its profile opens) and the token its
    /// proxy URL carries as the password.
    RunStarts {
        run: u64,
        session: u64,
        process: String,
        listener: OwnedFd,
        token: String,
        rules: Arc<Egress>,
        told: Sender<Denial>,
    },
    /// The run ended (reaped): macOS's listener closes, its token is gone (`sandbox::after_run`); on both
    /// systems (the main loop, `hosts::run_ended`) a question asked for it goes to whoever still waits.
    RunEnded { run: u64 },
    /// The user's answer to an `Ask`, or a grant from what a run was refused (main loop). An allow takes the place
    /// of a refusal before it, and a refusal of an allow.
    Answer { session: u64, run: u64, host: String, port: u16, scope: Scope },
    /// The user took back an answer for the session or the project (`:sandbox-disallow`): the host is asked
    /// again — whatever the runs' rules answered when they were made.
    Taken { session: u64, host: String, port: u16 },
    /// The user let a run go again with the host (`:sandbox-blocked`'s `y`): the session's latest answer goes —
    /// a refusal no longer holds; a run's rules that name it let it through, the others ask.
    Lifted { session: u64, host: String, port: u16 },
    /// The session ended: its answers go.
    SessionEnded { session: u64 },
}

/// What goes out, to the main loop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FromProxy {
    /// A host the run's rules don't name: the user is asked (a card); the connection waits up to `Limits::hold`.
    /// `process`: what asked (the run's process, as its report names it).
    Ask { session: u64, run: u64, host: String, port: u16, process: String },
}

/// The user's latest answer for a host in a session — before what a run's rules carried from when they were made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Latest {
    Allowed,
    Refused,
    /// Taken back: asked again.
    Taken,
}

/// The proxy's limits.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// To a request head's end.
    pub head_time: Duration,
    /// To a tunnel's ClientHello's end (and a second one's after a HelloRetryRequest).
    pub hello_time: Duration,
    /// How long a connection waits on the user's answer.
    pub hold: Duration,
    /// To an address's answer.
    pub connect: Duration,
    /// Tunnels a session may have at once.
    pub per_session: usize,
    /// Tunnels in all.
    pub total: usize,
    /// Hosts a session may have waiting on the user's answer at once — one card each: past it, refused unasked
    /// (a run trying many hosts would stack cards).
    pub questions: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            head_time: Duration::from_secs(10),
            hello_time: Duration::from_secs(10),
            // (below cargo's 30 s HTTP timeout and git's low-speed limit: the tool is still waiting)
            hold: Duration::from_secs(20),
            connect: Duration::from_secs(10),
            per_session: 256,
            total: 1024,
            questions: 8,
        }
    }
}

/// The user's verdict on a host, from the dispatcher.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Verdict {
    Go,
    No(String),
}

/// What the dispatcher reads: the messages, and its own connections' questions.
enum In {
    Msg(ToProxy),
    /// A connection whose setting doesn't name its host: the answers so far, else the user's (`id`: the waiter's;
    /// `answered`: its rules had the user's yes when they were made).
    Decide {
        session: u64,
        run: u64,
        host: String,
        port: u16,
        process: String,
        answered: bool,
        id: u64,
        reply: Sender<Verdict>,
        /// Its connection, to tell whether it's still there when its run ends.
        alive: Option<TcpStream>,
    },
    /// A connection that stopped waiting (its hold ran out): no longer a waiter.
    GaveUp {
        session: u64,
        host: String,
        port: u16,
        id: u64,
    },
    /// Test-only: how many connections wait on a question.
    #[cfg(test)]
    Waiting {
        session: u64,
        host: String,
        port: u16,
        reply: Sender<usize>,
    },
    /// A tunnel ended.
    Ended {
        session: u64,
    },
    /// A run's listener took a connection (macOS).
    Accepted {
        run: u64,
        stream: TcpStream,
    },
    /// A run's listener stopped taking connections for good (macOS): said on the run's sender.
    PortStopped {
        run: u64,
        why: String,
    },
}

/// The proxy: messages go in through it.
#[derive(Clone)]
pub struct Proxy {
    tx: Sender<In>,
}

impl Proxy {
    /// The proxy's dispatcher, started: names looked up by `resolve`, the user asked through `asks`.
    pub fn start(resolve: Resolve, asks: Sender<FromProxy>, limits: Limits) -> Proxy {
        let (tx, rx) = mpsc::channel();
        let state = State {
            shared: Shared { resolve, limits, tx: tx.clone() },
            asks,
            runs: HashMap::new(),
            per_session: HashMap::new(),
            total: 0,
            latest: HashMap::new(),
            run_allowed: HashSet::new(),
            waiting: HashMap::new(),
        };
        let _ = std::thread::Builder::new().name("tarae-proxy".into()).spawn(move || state.run(rx));
        Proxy { tx }
    }

    /// A message to the proxy — false when it's gone.
    pub fn send(&self, m: ToProxy) -> bool {
        self.tx.send(In::Msg(m)).is_ok()
    }

    /// Test-only: how many connections wait on the user's answer for `host:port` in `session`.
    #[cfg(test)]
    fn waiting(&self, session: u64, host: &str, port: u16) -> usize {
        let (reply, n) = mpsc::channel();
        let _ = self.tx.send(In::Waiting { session, host: host.into(), port, reply });
        n.recv().unwrap_or(0)
    }
}

/// What every connection's thread has of the proxy's.
#[derive(Clone)]
struct Shared {
    resolve: Resolve,
    limits: Limits,
    tx: Sender<In>,
}

/// A macOS run's: what each of its connections gets, and its listener's stop (dropped: the listener closes).
struct Run {
    session: u64,
    process: String,
    token: String,
    rules: Arc<Egress>,
    told: Sender<Denial>,
    _stop: io::PipeWriter,
}

/// The dispatcher's own state.
struct State {
    shared: Shared,
    asks: Sender<FromProxy>,
    runs: HashMap<u64, Run>,
    per_session: HashMap<u64, usize>,
    total: usize,
    /// (session, host, port): the user's latest answer for the session (or the project, or taken back).
    latest: HashMap<(u64, String, u16), Latest>,
    /// (session, run, host, port) let go for that run.
    run_allowed: HashSet<(u64, u64, String, u16)>,
    /// The questions out, each with the connections waiting on it.
    waiting: HashMap<(u64, String, u16), Question>,
}

/// A question out: the run it was asked for (the card names it), and the connections waiting on it.
struct Question {
    run: u64,
    waiters: Vec<Waiter>,
}

/// A connection waiting on the user's answer: its id, its run, what asked, where its verdict goes, and its
/// connection (a copy — whether it's still there).
struct Waiter {
    id: u64,
    run: u64,
    /// What asked (the card names it).
    process: String,
    reply: Sender<Verdict>,
    alive: Option<TcpStream>,
}

/// Whether a waiting connection is still there: its client hasn't closed it (peeked, never read — nor its flags
/// changed: its thread reads it after).
fn open(s: &TcpStream) -> bool {
    let mut b = [0u8; 1];
    // SAFETY: our socket, a buffer of its size.
    let n =
        unsafe { libc::recv(s.as_raw_fd(), b.as_mut_ptr().cast(), 1, libc::MSG_PEEK | libc::MSG_DONTWAIT) };
    n > 0
        || (n < 0
            && matches!(
                io::Error::last_os_error().kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ))
}

/// A connection, ready to be served.
struct Conn {
    stream: TcpStream,
    run: u64,
    session: u64,
    process: String,
    rules: Arc<Egress>,
    told: Sender<Denial>,
    /// macOS: the password its requests must carry.
    token: Option<String>,
}

impl State {
    fn run(mut self, rx: Receiver<In>) {
        while let Ok(m) = rx.recv() {
            match m {
                In::Msg(ToProxy::Connection(c)) => {
                    let conn = Conn {
                        stream: TcpStream::from(c.fd),
                        run: c.run,
                        session: c.session,
                        process: c.process,
                        rules: c.rules,
                        told: c.told,
                        token: None,
                    };
                    self.admit(conn);
                }
                In::Msg(ToProxy::RunStarts { run, session, process, listener, token, rules, told }) => {
                    let Ok((stop_r, stop_w)) = io::pipe() else { continue };
                    let tx = self.shared.tx.clone();
                    let spawned = std::thread::Builder::new()
                        .name("tarae-proxy-run".into())
                        .spawn(move || accept(run, listener, stop_r, tx));
                    if spawned.is_ok() {
                        self.runs.insert(run, Run { session, process, token, rules, told, _stop: stop_w });
                    }
                }
                In::Msg(ToProxy::RunEnded { run }) => self.run_ended(run),
                In::Msg(ToProxy::Answer { session, run, host, port, scope }) => {
                    self.answer(session, run, host, port, scope)
                }
                In::Msg(ToProxy::Taken { session, host, port }) => {
                    self.latest.insert((session, host, port), Latest::Taken);
                }
                In::Msg(ToProxy::Lifted { session, host, port }) => {
                    self.latest.remove(&(session, host, port));
                }
                In::Msg(ToProxy::SessionEnded { session }) => {
                    self.latest.retain(|(s, ..), _| *s != session);
                    self.run_allowed.retain(|(s, ..)| *s != session);
                    self.waiting.retain(|(s, ..), _| *s != session);
                }
                In::Decide { session, run, host, port, process, answered, id, reply, alive } => {
                    self.decide((session, host, port), answered, Waiter { id, run, process, reply, alive })
                }
                #[cfg(test)]
                In::Waiting { session, host, port, reply } => {
                    let _ =
                        reply.send(self.waiting.get(&(session, host, port)).map_or(0, |q| q.waiters.len()));
                }
                In::GaveUp { session, host, port, id } => {
                    let key = (session, host, port);
                    if let Some(q) = self.waiting.get_mut(&key) {
                        q.waiters.retain(|w| w.id != id);
                        if q.waiters.is_empty() {
                            self.waiting.remove(&key);
                        }
                    }
                }
                In::Ended { session } => {
                    self.total = self.total.saturating_sub(1);
                    if let Some(n) = self.per_session.get_mut(&session) {
                        *n = n.saturating_sub(1);
                    }
                }
                In::PortStopped { run, why } => {
                    let Some(r) = self.runs.get(&run) else { continue };
                    let target = format!("its port to tarae's proxy stopped taking connections: {why}");
                    crate::log::line("sandbox", "warning", &format!("run {run}: {target}"));
                    let _ = r.told.send(Denial { process: r.process.clone(), op: OP.into(), target });
                }
                In::Accepted { run, stream } => {
                    let Some(r) = self.runs.get(&run) else { continue };
                    let conn = Conn {
                        stream,
                        run,
                        session: r.session,
                        process: r.process.clone(),
                        rules: r.rules.clone(),
                        told: r.told.clone(),
                        token: Some(r.token.clone()),
                    };
                    self.admit(conn);
                }
            }
        }
    }

    /// A connection counted and served on a thread of its own — or, past a cap, answered 503 and said.
    fn admit(&mut self, c: Conn) {
        let limits = self.shared.limits;
        let mine = self.per_session.get(&c.session).copied().unwrap_or(0);
        if mine >= limits.per_session || self.total >= limits.total {
            let why = match mine >= limits.per_session {
                true => format!("past {} connections at once for the session", limits.per_session),
                false => format!("past {} connections at once in all", limits.total),
            };
            // (a fresh connection's send buffer is empty: this doesn't wait)
            let _ = c.stream.set_nonblocking(true);
            answer(&c.stream, 503, &why, "");
            tell(&c, &why);
            return;
        }
        *self.per_session.entry(c.session).or_default() += 1;
        self.total += 1;
        let shared = self.shared.clone();
        let session = c.session;
        let spawned = std::thread::Builder::new()
            .name("tarae-proxy-conn".into())
            .stack_size(256 << 10)
            .spawn(move || {
                let tx = shared.tx.clone();
                serve(c, &shared);
                let _ = tx.send(In::Ended { session });
            });
        if spawned.is_err() {
            self.total -= 1;
            if let Some(n) = self.per_session.get_mut(&session) {
                *n -= 1;
            }
        }
    }

    /// The answers so far for a host a connection's setting doesn't name — the user's latest for the session,
    /// the run's own, then what its rules carried (`answered`) — else the user asked, once a host, at most
    /// `Limits::questions` hosts of a session at once.
    fn decide(&mut self, key: (u64, String, u16), answered: bool, waiter: Waiter) {
        let (session, run) = (key.0, waiter.run);
        let latest = self.latest.get(&key).copied();
        let ran = self.run_allowed.contains(&(session, run, key.1.clone(), key.2));
        let verdict = match latest {
            Some(Latest::Refused) => Some(Verdict::No("the user refused it for this session".into())),
            Some(Latest::Allowed) => Some(Verdict::Go),
            _ if ran => Some(Verdict::Go),
            None if answered => Some(Verdict::Go),
            _ => None,
        };
        if let Some(v) = verdict {
            let _ = waiter.reply.send(v);
            return;
        }
        if !self.waiting.contains_key(&key) {
            let max = self.shared.limits.questions;
            if self.waiting.keys().filter(|(s, ..)| *s == session).count() >= max {
                let why = format!("{max} hosts already wait for the user's answer");
                let _ = waiter.reply.send(Verdict::No(why));
                return;
            }
            let (host, port, process) = (key.1.clone(), key.2, waiter.process.clone());
            let _ = self.asks.send(FromProxy::Ask { session, run, host, port, process });
        }
        self.waiting.entry(key).or_insert_with(|| Question { run, waiters: Vec::new() }).waiters.push(waiter);
    }

    /// Run `run` ended (reaped — the main loop says so on both systems; macOS's port closes with it): what it
    /// was let reach alone goes, and so does a connection of its that closed with it. A question asked for it
    /// goes to whoever still waits — another run's connection, else one of a process it left running (Linux: a
    /// daemon keeps its run) — asked again: none waits out its hold with no card.
    fn run_ended(&mut self, run: u64) {
        self.runs.remove(&run);
        self.run_allowed.retain(|(_, r, ..)| *r != run);
        let asks = &self.asks;
        self.waiting.retain(|(session, host, port), q| {
            q.waiters.retain(|w| {
                let live = w.run != run || w.alive.as_ref().is_some_and(open);
                if !live {
                    let _ = w.reply.send(Verdict::No("its run ended".into()));
                }
                live
            });
            if q.run == run
                && let Some(next) = q.waiters.iter().find(|w| w.run != run).or(q.waiters.first())
            {
                q.run = next.run;
                let (host, process) = (host.clone(), next.process.clone());
                let _ = asks.send(FromProxy::Ask {
                    session: *session,
                    run: next.run,
                    host,
                    port: *port,
                    process,
                });
            }
            !q.waiters.is_empty()
        });
    }

    /// The user's answer: kept as far as it reaches, and the connections waiting on it let go or refused.
    fn answer(&mut self, session: u64, run: u64, host: String, port: u16, scope: Scope) {
        let key = (session, host, port);
        let verdict = match scope {
            Scope::Refused => Verdict::No("the user refused it".into()),
            _ => Verdict::Go,
        };
        match scope {
            Scope::Run => {
                self.run_allowed.insert((session, run, key.1.clone(), port));
            }
            Scope::Session | Scope::Project => {
                self.latest.insert(key.clone(), Latest::Allowed);
            }
            Scope::Refused => {
                self.latest.insert(key.clone(), Latest::Refused);
            }
        }
        let Some(q) = self.waiting.remove(&key) else { return };
        let (now, rest): (Vec<_>, Vec<_>) =
            q.waiters.into_iter().partition(|w| scope != Scope::Run || w.run == run);
        for w in now {
            let _ = w.reply.send(verdict.clone());
        }
        // (an answer for one run leaves the others' connections waiting: the user is asked for the next one's)
        if let Some(next) = rest.first() {
            let (run, process) = (next.run, next.process.clone());
            let _ = self.asks.send(FromProxy::Ask { session, run, host: key.1.clone(), port, process });
            self.waiting.insert(key, Question { run, waiters: rest });
        }
    }
}

/// A run's listener's connections (macOS), each to the dispatcher, until its stop's other end is dropped. A port
/// that stops for good (an accept error that isn't one connection's or a passing want of descriptors) is said on
/// the run's sender (`In::PortStopped`).
fn accept(run: u64, listener: OwnedFd, stop: io::PipeReader, tx: Sender<In>) {
    let listener = match port_listener(listener) {
        Ok(l) => l,
        Err(e) => {
            let _ = tx.send(In::PortStopped { run, why: e.to_string() });
            return;
        }
    };
    let stopped = accepting(
        &listener,
        &stop,
        || listener.accept().map(|(s, _)| s),
        |stream| tx.send(In::Accepted { run, stream }).is_ok(),
    );
    if let Some(e) = stopped {
        let _ = tx.send(In::PortStopped { run, why: e.to_string() });
    }
}

/// A run's listener as its accept loop holds it: non-blocking — a connection gone between `poll` and `accept`
/// would leave a blocking accept waiting past the stop, and the run's port open after its end.
fn port_listener(fd: OwnedFd) -> io::Result<std::net::TcpListener> {
    let listener = std::net::TcpListener::from(fd);
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// The accept loop of a run's port: `next` (the listener's accept) whenever `poll` says it's ready, each
/// connection made blocking and given to `took` (false: the proxy's gone, the loop ends), until `stop` is
/// readable — or an error that stops it for good, returned (`after_accept_error`).
fn accepting(
    listener: &std::net::TcpListener,
    stop: &io::PipeReader,
    mut next: impl FnMut() -> io::Result<TcpStream>,
    mut took: impl FnMut(TcpStream) -> bool,
) -> Option<io::Error> {
    let mut fds = [
        libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 },
        libc::pollfd { fd: stop.as_raw_fd(), events: libc::POLLIN, revents: 0 },
    ];
    loop {
        // SAFETY: two pollfds of ours, their count.
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if r < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if r < 0 || fds[1].revents != 0 {
            return None;
        }
        if fds[0].revents != 0 {
            match next() {
                Ok(stream) => {
                    // (BSD's accepted sockets take the listener's O_NONBLOCK; `serve` reads with timeouts)
                    if stream.set_nonblocking(false).is_err() {
                        continue;
                    }
                    if !took(stream) {
                        return None;
                    }
                }
                Err(e) => match after_accept_error(&e) {
                    Some(pause) => std::thread::sleep(pause),
                    None => return Some(e),
                },
            }
        }
    }
}

/// What a failed accept means for a run's port: go on — at once for what was one connection's (it reset before
/// it was taken: `ECONNABORTED`) or nothing (`EINTR`, `EWOULDBLOCK`); after a pause while descriptors or buffers
/// run short (`EMFILE`, `ENFILE`, `ENOBUFS`, `ENOMEM` — the connection waits in the queue meanwhile) — or stop
/// for good (None: the listener itself is wrong).
fn after_accept_error(e: &io::Error) -> Option<Duration> {
    use io::ErrorKind::{ConnectionAborted, ConnectionReset, Interrupted, WouldBlock};
    match e.kind() {
        Interrupted | WouldBlock | ConnectionAborted | ConnectionReset => Some(Duration::ZERO),
        _ if matches!(e.raw_os_error(), Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)) => {
            Some(Duration::from_millis(100))
        }
        _ => None,
    }
}

/// A refusal said on the run's sender, as the supervisor says its own.
fn tell(c: &Conn, target: &str) {
    tell_op(c, OP, target);
}

fn tell_op(c: &Conn, op: &str, target: &str) {
    let _ = c.told.send(Denial { process: c.process.clone(), op: op.into(), target: target.into() });
}

/// An HTTP answer (`extra`: header lines, each ending CR LF), its body the why; the connection closes after.
fn answer(mut s: &TcpStream, status: u16, why: &str, extra: &str) {
    let reason = match status {
        400 => "Bad Request",
        403 => "Forbidden",
        405 => "Method Not Allowed",
        407 => "Proxy Authentication Required",
        408 => "Request Timeout",
        431 => "Request Header Fields Too Large",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Error",
    };
    let body = format!("tarae's proxy: {why}\n");
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\n{extra}Content-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = s.write_all(format!("{head}{body}").as_bytes());
}

/// Whether two tokens are the same, in time that doesn't depend on where they differ: every byte compared.
fn same_token(given: &[u8], token: &[u8]) -> bool {
    let diff = given.iter().zip(token).fold(0u8, |acc, (a, b)| acc | (a ^ b));
    std::hint::black_box(diff) == 0 && given.len() == token.len()
}

/// Bytes read from `s` into `buf` until `done` says enough (Some), within `by`. Err: the time ran out (true)
/// or the client ended or failed (false).
fn read_until<T>(
    mut s: &TcpStream,
    buf: &mut Vec<u8>,
    by: Instant,
    mut done: impl FnMut(&[u8]) -> Option<T>,
) -> Result<T, bool> {
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(t) = done(buf) {
            return Ok(t);
        }
        let left = by.saturating_duration_since(Instant::now());
        if left.is_zero() || s.set_read_timeout(Some(left)).is_err() {
            return Err(true);
        }
        match s.read(&mut chunk) {
            Ok(0) => return Err(false),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut) => {
                return Err(true);
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err(false),
        }
    }
}

/// One connection, start to end.
fn serve(c: Conn, shared: &Shared) {
    let limits = shared.limits;
    let s = &c.stream;
    let mut buf = Vec::new();
    let by = Instant::now() + limits.head_time;
    let h = match read_until(s, &mut buf, by, |b| match parse::head(b) {
        Ok(None) => None,
        r => Some(r),
    }) {
        Ok(Ok(Some(h))) => h,
        Ok(Ok(None)) => return,
        Ok(Err(r)) => {
            answer(s, r.status(), &r.said(), "");
            return tell(&c, &r.said());
        }
        Err(true) => {
            let why = format!("no whole request head within {} s", limits.head_time.as_secs_f32());
            answer(s, 408, &why, "");
            return tell(&c, &why);
        }
        Err(false) => return,
    };
    if let Some(token) = &c.token {
        let (host, port) = match &h.request {
            Request::Connect { host, port } | Request::Plain { host, port, .. } => (host, port),
        };
        let at = match host {
            Host::Name(n) => format!("{n}:{port}"),
            Host::Addr(a) => std::net::SocketAddr::new(*a, *port).to_string(),
        };
        let wrong = match &h.auth {
            Auth::Basic { password, .. } if same_token(password, token.as_bytes()) => None,
            Auth::None => Some(None),
            _ => Some(Some(format!("{at}: a wrong proxy password — not its run's (HTTPS_PROXY)"))),
        };
        if let Some(said) = wrong {
            answer(
                s,
                407,
                "this proxy takes its run's password",
                "Proxy-Authenticate: Basic realm=\"tarae\"\r\n",
            );
            if let Some(why) = said {
                tell(&c, &why);
            }
            return;
        }
    }
    let (host, port, default) = match &h.request {
        Request::Connect { host, port } => (host, *port, 443),
        Request::Plain { host, port, .. } => (host, *port, 80),
    };
    let name = match host {
        Host::Name(n) => n.clone(),
        Host::Addr(a) => {
            let why = format!("{a}:{port}: an address, not a name — rules name hosts");
            answer(s, 403, &why, "");
            return tell(&c, &why);
        }
    };
    let refuse = |r: Refused| {
        let why = format!("{name}:{port}: {}", r.why);
        answer(s, r.status, &why, "");
        tell(&c, &why);
    };
    // (a refusal the user's answer would lift: its own op — `:sandbox-blocked` offers the host)
    let refuse_host = |why: String| {
        let why = format!("{name}:{port}: {why}");
        answer(s, 403, &why, "");
        tell_op(&c, HOST_OP, &why);
    };
    if !c.rules.allows(&name, port, default) {
        static WAITERS: AtomicU64 = AtomicU64::new(0);
        let id = WAITERS.fetch_add(1, Ordering::Relaxed);
        let (reply, verdict) = mpsc::channel();
        let (session, host, process) = (c.session, name.clone(), c.process.clone());
        let answered = c.rules.answered(&name, port);
        let alive = c.stream.try_clone().ok();
        let ask =
            In::Decide { session, run: c.run, host: host.clone(), port, process, answered, id, reply, alive };
        if shared.tx.send(ask).is_err() {
            return;
        }
        match verdict.recv_timeout(limits.hold) {
            Ok(Verdict::Go) => {}
            Ok(Verdict::No(why)) => return refuse_host(why),
            Err(_) => {
                // (no longer waiting: the next connection to it asks again)
                let _ = shared.tx.send(In::GaveUp { session, host, port, id });
                return refuse_host(format!(
                    "not among the hosts it may reach (llm.sandbox-hosts); the user was asked and didn't answer \
                     within {} s",
                    limits.hold.as_secs_f32()
                ));
            }
        }
    }
    let up = match egress::connect(&c.rules, &shared.resolve, &name, port, limits.connect) {
        Ok(up) => up,
        Err(r) => return refuse(r),
    };
    let rest = buf.split_off(h.len);
    let (sni, first, body) = match h.request {
        // (only the head and its body: what follows them isn't this request's — `relay` drops it)
        Request::Plain { head, body, .. } => {
            if relay_send(&up, &head).is_err() {
                return;
            }
            (None, rest, Some(parse::Body::new(body)))
        }
        Request::Connect { host, .. } => {
            let _ = (&c.stream).write_all(b"HTTP/1.1 200 Connection established\r\n\r\n");
            let mut hello = rest;
            let by = Instant::now() + limits.hello_time;
            let said = match read_until(s, &mut hello, by, |b| match parse::server_name(b) {
                Ok(None) => None,
                r => Some(r),
            }) {
                Ok(Ok(Some(n))) if parse::same_host(&host, &n) => Ok(n),
                Ok(Ok(Some(n))) => Err(format!("its ClientHello names {n}, not the host it asked for")),
                Ok(Ok(None)) => return,
                Ok(Err(e)) => Err(e.said()),
                Err(true) => {
                    Err(format!("no whole ClientHello within {} s", limits.hello_time.as_secs_f32()))
                }
                Err(false) => return,
            };
            match said {
                Ok(n) => (Some(n), hello, None),
                Err(why) => return tell(&c, &format!("{name}:{port}: {why} — the tunnel closed")),
            }
        }
    };
    let _ = c.stream.set_read_timeout(None);
    let client = match c.stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    };
    match relay::relay(client, up, sni.as_deref(), limits.hello_time, &first, body) {
        relay::End::Done => {}
        relay::End::Dropped(n) => tell(
            &c,
            &format!(
                "{name}:{port}: {n} bytes sent after its request were dropped — one request per connection"
            ),
        ),
        relay::End::Closed(why) => tell(&c, &format!("{name}:{port}: {why} — the tunnel closed")),
    }
}

fn relay_send(mut up: &TcpStream, bytes: &[u8]) -> io::Result<()> {
    up.write_all(bytes)
}

/// What a run's environment gets while its network goes through the proxy: every proxy variable naming `url` —
/// Linux: `route::PROXY`, whose connects the supervisor hands to the proxy, so no credential is needed; macOS:
/// the run's own port with its token (`RunPort::url`) — and this machine's loopback left direct (a test's own
/// server). In place of the user's own (`runenv`).
pub fn run_env(url: &str) -> Vec<(&'static str, String)> {
    let direct = "localhost,127.0.0.1,::1".to_string();
    let mut env: Vec<(&str, String)> =
        ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"]
            .into_iter()
            .map(|k| (k, url.to_string()))
            .collect();
    env.extend([("NO_PROXY", direct.clone()), ("no_proxy", direct)]);
    env
}

/// What git gets on top (macOS): `http.proxyAuthMethod=basic` as the setting after the `count` the run's own
/// environment already holds (`GIT_CONFIG_COUNT`) — git's default (`anyauth`) sends the password only after a
/// 407 (M1), one round trip more on every connection.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub fn git_basic(count: usize) -> [(String, String); 3] {
    [
        ("GIT_CONFIG_COUNT".into(), (count + 1).to_string()),
        (format!("GIT_CONFIG_KEY_{count}"), "http.proxyAuthMethod".into()),
        (format!("GIT_CONFIG_VALUE_{count}"), "basic".into()),
    ]
}

/// A session as the proxy knows it: its project's root resolved as a run's policy resolves it
/// (`sandbox::profile_path` — a path through a link is its real one), hashed. The one way both sides name it: the
/// policy's (`Policy.session`, on its worker) and the main loop's (`Session::proxy_id`, once per root). It looks at
/// the disk: never on the key path.
pub fn session_of(root: &std::path::Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    super::profile_path(root).hash(&mut h);
    h.finish()
}

/// A macOS run's own way to the proxy (§macOS flow): a listener on `127.0.0.1:0` — the one port beyond the
/// opened ones its profile opens, so another session's run can't reach it (M3d) — and the token its proxy URL
/// carries as the password, 32 bytes from `getentropy` in hex: what can reach the port but not read the run's
/// environment (outside Seatbelt) still can't use it. Made with the run's policy; the listener goes to the proxy
/// once, at spawn (`take`) — no copy of the policy keeps the port taking connections once the run is gone.
#[cfg_attr(target_os = "linux", allow(dead_code))]
pub struct RunPort {
    /// The run, as the proxy knows it (`RunStarts`, `RunEnded`).
    pub run: u64,
    pub port: u16,
    token: String,
    listener: std::sync::Mutex<Option<OwnedFd>>,
    /// This machine's own addresses beyond loopback, read with the policy (`egress::addresses`) — None where
    /// they couldn't be: the proxy then reaches no address at all.
    pub own_addresses: Option<Vec<std::net::IpAddr>>,
}

#[cfg_attr(target_os = "linux", allow(dead_code))]
impl RunPort {
    /// For run `run` (`Policy.run_id`).
    pub fn new(run: u64) -> io::Result<Arc<RunPort>> {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        let mut raw = [0u8; 32];
        // SAFETY: a buffer of ours, its length (≤ 256, getentropy's most).
        if unsafe { libc::getentropy(raw.as_mut_ptr().cast(), raw.len()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let token = raw.iter().map(|b| format!("{b:02x}")).collect();
        Ok(Arc::new(RunPort {
            run,
            port,
            token,
            listener: std::sync::Mutex::new(Some(listener.into())),
            // (loopback is `route::this_machine`'s: the own addresses are the others — a LAN's, a bridge's)
            own_addresses: egress::addresses()
                .ok()
                .map(|a| a.into_iter().filter(|ip| !ip.to_canonical().is_loopback()).collect()),
        }))
    }

    /// The URL its run's proxy variables name: the token as the password (`run:` the user — git and pip send
    /// nothing with a user alone, M1), the address as a literal (a JVM's default socket reaches `127.0.0.1`
    /// where Seatbelt's `localhost` doesn't match `::ffff:127.0.0.1` — M3d).
    pub fn url(&self) -> String {
        format!("http://run:{}@127.0.0.1:{}", self.token, self.port)
    }

    #[cfg(test)]
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The listener, once: the spawn's, for the proxy.
    pub fn take(&self) -> Option<OwnedFd> {
        self.listener.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Its run started (`sandbox::spawn`): the listener to the proxy, with what each of its connections gets —
    /// the session (its project's), the rules (its policy's hosts and answers; this machine's own addresses as
    /// read with the policy), where its refusals are said (`told`), and what they name (`process`).
    pub fn started(&self, policy: &super::Policy, process: String, told: Sender<Denial>) {
        let Some(listener) = self.take() else { return };
        #[cfg(all(test, not(target_os = "linux")))]
        let beyond = TEST_BEYOND.lock().unwrap_or_else(|e| e.into_inner());
        let rules = Egress {
            own_addresses: self.own_addresses.clone(),
            #[cfg(all(test, not(target_os = "linux")))]
            beyond: beyond.iter().filter(|(r, _)| *r == policy.root).map(|(_, ip)| *ip).collect(),
            ..policy.egress.clone()
        };
        let session = policy.session;
        let token = self.token.clone();
        let (run, rules) = (self.run, Arc::new(rules));
        get().send(ToProxy::RunStarts { run, session, process, listener, token, rules, told });
    }

    /// Its run ended (reaped — `sandbox::after_run`): its port closes, its token is gone.
    pub fn ended(run: u64) {
        get().send(ToProxy::RunEnded { run });
    }
}

/// Its port only: a policy's `Debug` is its key (`policy_key`) and goes in logs — never the token.
impl std::fmt::Debug for RunPort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunPort").field("port", &self.port).finish_non_exhaustive()
    }
}

impl PartialEq for RunPort {
    fn eq(&self, other: &RunPort) -> bool {
        self.port == other.port && self.token == other.token
    }
}

impl Eq for RunPort {}

/// Where the proxy's questions go (`listen`).
static ASKS: std::sync::OnceLock<Sender<FromProxy>> = std::sync::OnceLock::new();

static PROXY: std::sync::OnceLock<Proxy> = std::sync::OnceLock::new();

/// The proxy, if a run started it — the main loop's answers go to it, never starting it.
#[cfg_attr(test, allow(dead_code))]
pub fn running() -> Option<&'static Proxy> {
    PROXY.get()
}

/// The main loop takes the proxy's questions on `asks` — its only asker (`hosts.rs`), set once at start, before
/// any run. Unset (tests that don't), a question has no reader: its connection waits out its hold and is refused.
pub fn listen(asks: Sender<FromProxy>) {
    let _ = ASKS.set(asks);
}

/// The proxy of this tarae, started on first use — never on the main loop's key path (Linux: a supervisor's
/// thread; macOS: a run's spawn, whose first one starts a thread). Names are looked up by the system's
/// resolver; the user is asked through `listen`'s sender.
pub fn get() -> &'static Proxy {
    PROXY.get_or_init(|| {
        #[cfg(not(test))]
        let asks = ASKS.get().cloned().unwrap_or_else(|| mpsc::channel().0);
        // (tests: each question to the sender its session's test registered — `TEST_ASKS`)
        #[cfg(test)]
        let asks = {
            let _ = &ASKS;
            let (asks, rx) = mpsc::channel::<FromProxy>();
            crate::event::thread("tarae-test-asks", move || {
                for a in rx {
                    let FromProxy::Ask { session, .. } = &a;
                    let to = TEST_ASKS.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some((_, tx)) = to.iter().find(|(s, _)| s == session) {
                        let _ = tx.send(a);
                    }
                }
            });
            asks
        };
        let system = egress::system();
        #[cfg(test)]
        let resolve: Resolve = Arc::new(move |host: &str, port| {
            let names = TEST_NAMES.lock().unwrap_or_else(|e| e.into_inner());
            match names.iter().find(|(n, _)| n == host) {
                Some((_, at)) => Ok(vec![*at]),
                None => system(host, port),
            }
        });
        #[cfg(not(test))]
        let resolve = system;
        Proxy::start(resolve, asks, Limits::default())
    })
}

/// Tests: names the proxy `get` gives looks up here first — a listener of the test's standing for the outside
/// (Linux: on this machine's own address; macOS: on loopback, taken for beyond by `TEST_BEYOND`) — a name, as
/// the proxy takes no address.
#[cfg(test)]
pub static TEST_NAMES: std::sync::Mutex<Vec<(String, std::net::SocketAddr)>> =
    std::sync::Mutex::new(Vec::new());

/// Tests: where the proxy `get` gives asks about a session's hosts — the test standing in for the main loop.
#[cfg(test)]
pub static TEST_ASKS: std::sync::Mutex<Vec<(u64, Sender<FromProxy>)>> = std::sync::Mutex::new(Vec::new());

/// Tests (macOS): loopback taken for beyond in the rules of the runs of the project at the root given
/// (`Egress::beyond`) — the test's own listeners stand for the outside.
#[cfg(all(test, not(target_os = "linux")))]
pub static TEST_BEYOND: std::sync::Mutex<Vec<(std::path::PathBuf, std::net::IpAddr)>> =
    std::sync::Mutex::new(Vec::new());

/// A connected loopback TCP pair for a run's connection to the proxy (Linux, `supervisor::proxied`): the run's
/// end — a socket of `domain`, readied by `ready` (what the run set on its own) before it connects — and the
/// proxy's, accepted on a listener made for this pair alone and closed at once. An accepted peer that isn't the
/// run's end (another connect to the listener in that window) is closed, and the next one taken. `between`:
/// called with the listener's address before the run's end connects (tests stand an intruder in the window).
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn pair(
    domain: i32,
    ready: impl FnOnce(&OwnedFd) -> io::Result<()>,
    between: impl FnOnce(std::net::SocketAddr),
) -> io::Result<(OwnedFd, TcpStream)> {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener};
    use std::os::fd::FromRawFd;
    let six = domain == libc::AF_INET6;
    let listener = match six {
        true => TcpListener::bind((Ipv6Addr::LOCALHOST, 0))
            .or_else(|_| TcpListener::bind((Ipv4Addr::LOCALHOST, 0))),
        false => TcpListener::bind((Ipv4Addr::LOCALHOST, 0)),
    }?;
    let at = listener.local_addr()?;
    // (an IPv6 socket reaches an IPv4 listener at its mapped address)
    let to = match (six, at.ip()) {
        (true, IpAddr::V4(v4)) => SocketAddr::new(IpAddr::V6(v4.to_ipv6_mapped()), at.port()),
        _ => at,
    };
    #[cfg(target_os = "linux")]
    let kind = libc::SOCK_STREAM | libc::SOCK_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    let kind = libc::SOCK_STREAM;
    // SAFETY: plain call; a descriptor it makes is ours.
    let s = unsafe { libc::socket(domain, kind, libc::IPPROTO_TCP) };
    if s < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just made.
    let run = unsafe { OwnedFd::from_raw_fd(s) };
    #[cfg(not(target_os = "linux"))]
    // SAFETY: a flag set on our own descriptor.
    unsafe {
        libc::fcntl(run.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }
    ready(&run)?;
    between(at);
    let (sa, len) = raw_addr(to);
    // SAFETY: our socket and an address of ours, live for the call.
    if unsafe { libc::connect(run.as_raw_fd(), (&raw const sa).cast(), len) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mine = TcpStream::from(run.try_clone()?).local_addr()?;
    let canonical = |a: SocketAddr| SocketAddr::new(a.ip().to_canonical(), a.port());
    // (the run's end is in the listener's queue once its connect returned: it comes within a few accepts)
    for _ in 0..8 {
        let (proxy, peer) = listener.accept()?;
        if canonical(peer) == canonical(mine) {
            return Ok((run, proxy));
        }
        crate::log::line(
            "sandbox",
            "warning",
            &format!("a connection not the run's took the proxy's pair: {peer}"),
        );
    }
    Err(io::Error::other("the run's end never came to the pair's listener"))
}

/// `a` as the kernel takes it.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn raw_addr(a: std::net::SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    // SAFETY: an all-zero sockaddr_storage is valid; each arm writes the family's own struct, which fits in it.
    unsafe {
        let mut ss: libc::sockaddr_storage = std::mem::zeroed();
        match a {
            std::net::SocketAddr::V4(v4) => {
                let sin = &mut *(&raw mut ss).cast::<libc::sockaddr_in>();
                sin.sin_family = libc::AF_INET as libc::sa_family_t;
                sin.sin_port = v4.port().to_be();
                sin.sin_addr.s_addr = u32::from(*v4.ip()).to_be();
                #[cfg(target_os = "macos")]
                {
                    sin.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
                }
                (ss, std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t)
            }
            std::net::SocketAddr::V6(v6) => {
                let sin6 = &mut *(&raw mut ss).cast::<libc::sockaddr_in6>();
                sin6.sin6_family = libc::AF_INET6 as libc::sa_family_t;
                sin6.sin6_port = v6.port().to_be();
                sin6.sin6_addr.s6_addr = v6.ip().octets();
                #[cfg(target_os = "macos")]
                {
                    sin6.sin6_len = std::mem::size_of::<libc::sockaddr_in6>() as u8;
                }
                (ss, std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t)
            }
        }
    }
}

#[cfg(test)]
mod tests;
// (macOS's runs through the real path: a run's port, its profile, its environment, its spawn and end)
#[cfg(all(test, target_os = "macos"))]
mod macos_tests;
// (step 5d's measurements, Q4 and Q3b — `#[ignore]`d, by hand outside a sandbox)
#[cfg(all(test, target_os = "macos"))]
mod macos_measure;
