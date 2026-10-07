//! Redis's wire without I/O (db-plan.md §12.3), RESP2 — as PostgreSQL's and MySQL's: fed bytes, answering with
//! events. Valkey speaks the same (INFERRED — not tested).
//!
//! Signing in: TLS from the first byte when asked (the SDK's `Tls`); `AUTH [user] password` — only over TLS or to
//! this machine; `HELLO 2` for the server's version (answered to any user — MEASURED, 7.4, `ro` too; refused by a
//! server before 6: the version is then unknown); `COMMAND INFO` of the plugin's reads, to narrow them to what this
//! server flags `readonly` and not `write` (a renamed command, a module's) — an ACL user without `@connection` is
//! refused it (MEASURED: `ro`), and then the list stands, the ACL the wall; `SELECT` the URL's database (refused to
//! `ro` too — MEASURED: an ACL user like it reads database 0 only).
//!
//! Only reads are sent (`commands.rs`): a script is a write, KEYS never goes — a pattern is walked with SCAN, to a
//! cap, said when reached.

use std::collections::{BTreeSet, VecDeque};

use redis_protocol::resp2::decode::decode;
use redis_protocol::resp2::types::OwnedFrame as Frame;
use tarae_plugin::tls::Tls;

use super::commands;
use crate::driver::wire::{Column, Event, Params, Refusal, Table};

/// SCAN's walk: keys looked at per call, calls at most — past them the count is "at least".
pub const SCAN_COUNT: u32 = 1000;
pub const SCAN_CALLS: u32 = 100;

enum State {
    /// The TLS handshake under way.
    Handshake,
    /// Signing in: the step whose answer is awaited.
    Setup(Setup),
    Idle,
    Busy(Job),
    Over,
}

#[derive(Clone, Copy)]
enum Setup {
    Auth,
    Hello,
    CommandInfo,
    Select,
}

enum Request {
    Command(Vec<String>),
    Keys(String),
}

/// A request under way.
enum Job {
    Command(String),
    /// One key, no pattern: is it there.
    Exists(String),
    Scan { pattern: String, calls: u32, matched: u64, first: Option<String> },
    Type { key: String, matched: u64, capped: bool },
    Ttl { key: String, matched: u64, capped: bool, kind: String },
}

pub struct Session {
    params: Params,
    state: State,
    tls: Option<Tls>,
    buf: Vec<u8>,
    setup: VecDeque<Setup>,
    version: String,
    /// The reads this server lets the plugin send (`commands::READS`, narrowed by `COMMAND INFO`).
    reads: BTreeSet<String>,
    queue: VecDeque<Request>,
}

impl Session {
    /// A session, and the bytes that open it (Redis waits for the client).
    pub fn start(params: Params, _seed: [u8; 32]) -> (Session, Vec<Event>) {
        let mut setup = VecDeque::new();
        if params.password.is_some() {
            setup.push_back(Setup::Auth);
        }
        setup.extend([Setup::Hello, Setup::CommandInfo]);
        let db = params.database.clone().filter(|d| !d.is_empty() && d != "0");
        if db.is_some() {
            setup.push_back(Setup::Select);
        }
        let tls = params.tls.clone();
        let mut s = Session {
            params,
            state: State::Over,
            tls: None,
            buf: Vec::new(),
            setup,
            version: String::new(),
            reads: commands::READS.iter().map(|r| r.to_string()).collect(),
            queue: VecDeque::new(),
        };
        if db.is_some_and(|d| d.parse::<u32>().is_err()) {
            s.state = State::Over;
            return (s, vec![Event::Failed("a Redis database is a number — the URL's path".into())]);
        }
        // AUTH sends the password as it is: only inside TLS or to this machine
        if s.params.password.is_some() && !s.params.may_send_password() {
            return (s, vec![Event::Failed("signing in would send the password in the clear off this machine — TLS for it (rediss://)".into())]);
        }
        let out = match tls {
            Some((config, name)) => match Tls::client(config, &name) {
                Ok(mut t) => {
                    let hello = t.outgoing();
                    (s.tls, s.state) = (Some(t), State::Handshake);
                    vec![Event::Send(hello)]
                }
                Err(why) => s.fail(format!("TLS: {why}")),
            },
            None => s.next_setup(),
        };
        (s, out)
    }

    /// Runs a read — refused, nothing sent, unless it's one (`commands::refusal`) this server flags as one.
    // (step 4's key hints and step 6's runs are what call these)
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn command(&mut self, words: &[&str]) -> Vec<Event> {
        let why = commands::refusal(words).or_else(|| {
            let name = commands::name(words);
            (!self.reads.contains(&name)).then(|| format!("{} isn't flagged read-only by this server", name.to_ascii_uppercase()))
        });
        if let Some(why) = why {
            return vec![Event::Refused(Refusal { code: String::new(), sqlstate: None, message: why, position: None })];
        }
        self.queue.push_back(Request::Command(words.iter().map(|w| w.to_string()).collect()));
        self.next()
    }

    /// How many keys match a pattern, and one's type and time to live — walked with SCAN (never KEYS), to its cap.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn keys(&mut self, pattern: &str) -> Vec<Event> {
        self.queue.push_back(Request::Keys(pattern.to_string()));
        self.next()
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn idle(&self) -> bool {
        matches!(self.state, State::Idle) && self.queue.is_empty()
    }

    /// Sends `words` as they are — the tests' own setup (seeding keys) and their look at what an ACL refuses.
    #[cfg(test)]
    pub fn raw(&mut self, words: &[&str]) -> Vec<Event> {
        self.queue.push_back(Request::Command(words.iter().map(|w| w.to_string()).collect()));
        self.next()
    }

    /// Bytes that came from the server.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        let came = if let Some(t) = &mut self.tls {
            if let Err(why) = t.feed(bytes) {
                return self.fail(why);
            }
            let plain = t.read();
            if matches!(self.state, State::Handshake) {
                out.push(Event::Send(t.outgoing()));
                if !t.handshaking() {
                    out.extend(self.next_setup());
                }
            }
            plain
        } else {
            bytes.to_vec()
        };
        self.buf.extend_from_slice(&came);
        // A frame ends with `\n`: none came, none was completed — a big reply in small pieces isn't parsed again
        // from its start on every piece
        if !came.contains(&b'\n') {
            out.retain(|e| !matches!(e, Event::Send(b) if b.is_empty()));
            return out;
        }
        while !matches!(self.state, State::Over | State::Handshake) {
            match decode(&self.buf) {
                Ok(Some((frame, n))) => {
                    self.buf.drain(..n);
                    out.extend(self.reply(frame));
                }
                Ok(None) => break,
                Err(_) => out.extend(self.fail("the server sent what isn't RESP".into())),
            }
        }
        out.retain(|e| !matches!(e, Event::Send(b) if b.is_empty()));
        out
    }

    fn reply(&mut self, f: Frame) -> Vec<Event> {
        match std::mem::replace(&mut self.state, State::Over) {
            State::Setup(step) => self.setup_reply(step, f),
            State::Busy(job) => self.job_reply(job, f),
            State::Idle | State::Handshake | State::Over => self.fail("the server sent a reply nothing asked for".into()),
        }
    }

    // ── Signing in ──

    fn next_setup(&mut self) -> Vec<Event> {
        let Some(step) = self.setup.pop_front() else {
            self.state = State::Idle;
            let mut out = vec![Event::Ready { version: self.version.clone() }];
            out.extend(self.next());
            return out;
        };
        let p = &self.params;
        let words: Vec<String> = match step {
            Setup::Auth => {
                let pw = p.password.as_ref().map(|s| s.expose().to_string()).unwrap_or_default();
                // (`default` alone: what a server without ACLs — before 6 — understands)
                if p.user == "default" { vec!["AUTH".into(), pw] } else { vec!["AUTH".into(), p.user.clone(), pw] }
            }
            Setup::Hello => vec!["HELLO".into(), "2".into()],
            Setup::CommandInfo => {
                let mut w = vec!["COMMAND".to_string(), "INFO".to_string()];
                w.extend(commands::READS.iter().map(|r| r.to_string()));
                w
            }
            Setup::Select => vec!["SELECT".into(), p.database.clone().unwrap_or_default()],
        };
        self.state = State::Setup(step);
        self.send(&words)
    }

    fn setup_reply(&mut self, step: Setup, f: Frame) -> Vec<Event> {
        match (step, f) {
            (Setup::Auth | Setup::Select, Frame::Error(e)) => self.fail(e),
            // Refused HELLO or COMMAND (an ACL user without `@connection`, a server before 6): the version unknown,
            // the plugin's own list stands
            (Setup::Hello | Setup::CommandInfo, Frame::Error(_)) => self.next_setup(),
            (Setup::Hello, Frame::Array(kv)) => {
                let text = |f: &Frame| match f {
                    Frame::BulkString(b) | Frame::SimpleString(b) => String::from_utf8_lossy(b).into_owned(),
                    _ => String::new(),
                };
                if let Some(v) = kv.chunks(2).find(|p| text(&p[0]) == "version").and_then(|p| p.get(1)) {
                    self.version = text(v);
                }
                self.next_setup()
            }
            (Setup::CommandInfo, Frame::Array(entries)) => {
                for (name, entry) in commands::READS.iter().zip(&entries) {
                    if !flagged_read_only(entry) {
                        self.reads.remove(*name);
                    }
                }
                self.next_setup()
            }
            (Setup::Auth | Setup::Select, _) => self.next_setup(),
            _ => self.fail("the server answered signing in with something else".into()),
        }
    }

    // ── Requests ──

    fn next(&mut self) -> Vec<Event> {
        if !matches!(self.state, State::Idle) {
            return Vec::new();
        }
        let Some(r) = self.queue.pop_front() else { return Vec::new() };
        match r {
            Request::Command(words) => {
                self.state = State::Busy(Job::Command(words.first().cloned().unwrap_or_default()));
                self.send(&words)
            }
            // A name with no pattern in it is one key: asked after, not searched for
            Request::Keys(pattern) if !pattern.contains(['*', '?', '[', '\\']) => {
                self.state = State::Busy(Job::Exists(pattern.clone()));
                self.send(&["EXISTS".into(), pattern])
            }
            Request::Keys(pattern) => self.scan(Job::Scan { pattern, calls: 0, matched: 0, first: None }, "0"),
        }
    }

    fn scan(&mut self, job: Job, cursor: &str) -> Vec<Event> {
        let Job::Scan { pattern, .. } = &job else { unreachable!() };
        let words = ["SCAN".into(), cursor.into(), "MATCH".into(), pattern.clone(), "COUNT".into(), SCAN_COUNT.to_string()];
        self.state = State::Busy(job);
        self.send(&words)
    }

    fn job_reply(&mut self, job: Job, f: Frame) -> Vec<Event> {
        if let Frame::Error(e) = &f {
            self.state = State::Idle;
            let mut out = vec![Event::Refused(refusal(e))];
            out.extend(self.next());
            return out;
        }
        let answer = match job {
            Job::Command(name) => {
                let rows: Vec<Vec<Option<String>>> = match f {
                    Frame::Array(items) => items.iter().map(|i| vec![text(i)]).collect(),
                    other => vec![vec![text(&other)]],
                };
                let column = Column { name: name.to_ascii_lowercase(), type_id: 0, table: Table::None };
                let tag = format!("{} {}", name.to_ascii_uppercase(), rows.len());
                Event::Rows { columns: vec![column], rows, tag }
            }
            Job::Exists(key) => match f {
                Frame::Integer(1) => return self.ask_type(key, 1, false),
                _ => Event::Keys { matched: 0, capped: false, sample: None },
            },
            Job::Scan { pattern, calls, mut matched, mut first } => {
                let Frame::Array(parts) = f else { return self.fail("SCAN's answer doesn't read".into()) };
                let (Some(cursor), Some(Frame::Array(keys))) = (parts.first().and_then(text), parts.get(1)) else {
                    return self.fail("SCAN's answer doesn't read".into());
                };
                matched += keys.len() as u64;
                if first.is_none() {
                    first = keys.first().and_then(text);
                }
                let calls = calls + 1;
                let done = cursor == "0";
                if !done && calls < SCAN_CALLS {
                    return self.scan(Job::Scan { pattern, calls, matched, first }, &cursor);
                }
                match first {
                    Some(key) => return self.ask_type(key, matched, !done),
                    None => Event::Keys { matched, capped: !done, sample: None },
                }
            }
            Job::Type { key, matched, capped } => {
                let kind = text(&f).unwrap_or_default();
                self.state = State::Busy(Job::Ttl { key: key.clone(), matched, capped, kind });
                return self.send(&["PTTL".into(), key]);
            }
            Job::Ttl { key, matched, capped, kind } => {
                // (-1: it doesn't expire; -2: gone since)
                let ttl = match f {
                    Frame::Integer(ms) if ms >= 0 => Some(ms),
                    _ => None,
                };
                Event::Keys { matched, capped, sample: Some((key, kind, ttl)) }
            }
        };
        self.state = State::Idle;
        let mut out = vec![answer];
        out.extend(self.next());
        out
    }

    fn ask_type(&mut self, key: String, matched: u64, capped: bool) -> Vec<Event> {
        self.state = State::Busy(Job::Type { key: key.clone(), matched, capped });
        self.send(&["TYPE".into(), key])
    }

    // ── Bytes out ──

    /// A command as RESP's array of bulk strings, through TLS when there's TLS.
    fn send(&mut self, words: &[String]) -> Vec<Event> {
        let mut b = format!("*{}\r\n", words.len()).into_bytes();
        for w in words {
            b.extend(format!("${}\r\n", w.len()).as_bytes());
            b.extend(w.as_bytes());
            b.extend(b"\r\n");
        }
        match &mut self.tls {
            Some(t) => match t.write(&b) {
                Ok(()) => vec![Event::Send(t.outgoing())],
                Err(why) => self.fail(why),
            },
            None => vec![Event::Send(b)],
        }
    }

    fn fail(&mut self, why: String) -> Vec<Event> {
        self.state = State::Over;
        vec![Event::Failed(why)]
    }
}

impl crate::driver::wire::Session for Session {
    fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        Session::feed(self, bytes)
    }
}

/// A `COMMAND INFO` entry's flags say `readonly` and not `write` (none: the server doesn't have it by that name).
fn flagged_read_only(entry: &Frame) -> bool {
    let Frame::Array(parts) = entry else { return false };
    let Some(Frame::Array(flags)) = parts.get(2) else { return false };
    let has = |f: &str| flags.iter().any(|x| text(x).as_deref() == Some(f));
    has("readonly") && !has("write")
}

fn text(f: &Frame) -> Option<String> {
    match f {
        Frame::BulkString(b) | Frame::SimpleString(b) => Some(String::from_utf8_lossy(b).into_owned()),
        Frame::Integer(i) => Some(i.to_string()),
        Frame::Error(e) => Some(e.clone()),
        Frame::Array(items) => Some(items.iter().map(|i| text(i).unwrap_or_default()).collect::<Vec<_>>().join(" ")),
        Frame::Null => None,
    }
}

/// An error reply: its first word is its code (`NOPERM`, `WRONGTYPE`); a cluster's redirect is said for what it is.
fn refusal(e: &str) -> Refusal {
    let code = e.split_whitespace().next().unwrap_or("").to_string();
    let message = match code.as_str() {
        "MOVED" | "ASK" => "the key lives on another node of a cluster — the plugin speaks to one server".to_string(),
        _ => e.to_string(),
    };
    Refusal { code, sqlstate: None, message, position: None }
}

#[cfg(test)]
mod tests;
