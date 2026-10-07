//! MySQL's wire without I/O (db-plan.md §12.2): a `Session` is fed the bytes that came and answers with events,
//! as PostgreSQL's does. MariaDB speaks the same handshake (INFERRED — no MariaDB server tested it).
//!
//! Signing in: the server's HandshakeV10 → (TLS asked: the SSL request, then the SDK's `Tls` carries everything) →
//! HandshakeResponse41 → an auth switch or more data → OK. `mysql_native_password` and `caching_sha2_password`;
//! the latter's full authentication sends the password **only inside TLS, or RSA-encrypted to this machine** under
//! the key the server sends when asked (MySQL's own `--get-server-public-key`) — off this machine without TLS it's
//! refused, never asked for over the open network (the user's decision, 2026-10-07). RSA-OAEP's random bytes are
//! the caller's, so a captured exchange replays byte for byte.
//!
//! Capabilities never asked: `LOCAL_FILES` (a server asking for a file of ours anyway is stopped talking to),
//! `MULTI_STATEMENTS`, `MULTI_RESULTS` — one statement per query. Read-only by default: `SET SESSION TRANSACTION
//! READ ONLY` once signed in, and each statement in `START TRANSACTION READ ONLY` … `ROLLBACK` (a statement can
//! lift the session's setting, not the started transaction's). Refused before sending (`sql.rs`), read both with
//! and without backslash escapes: `INTO OUTFILE`/`DUMPFILE` — a file on the server that read-only doesn't stop —, a
//! `SET` of `sql_mode` (it would change where strings end), and `PREPARE`/`EXECUTE`/`CALL` (SQL the plugin can't
//! read).

use std::collections::VecDeque;

use rand_chacha::ChaCha20Rng;
use rand_core::SeedableRng;
use rsa::pkcs8::DecodePublicKey;
use rsa::{Oaep, RsaPublicKey};
use sha1::Sha1;
use sha2::{Digest, Sha256};
use tarae_plugin::tls::Tls;

use super::sql;
use crate::driver::wire::{Column, Event, Params, Refusal, Table};

// Capabilities (VERIFIED `myp-group__group__cs__capabilities__flags`)
const LONG_PASSWORD: u32 = 1;
const CONNECT_WITH_DB: u32 = 8;
const PROTOCOL_41: u32 = 512;
const SSL: u32 = 2048;
const TRANSACTIONS: u32 = 8192;
const SECURE_CONNECTION: u32 = 32768;
const PLUGIN_AUTH: u32 = 1 << 19;
const PLUGIN_AUTH_LENENC_CLIENT_DATA: u32 = 1 << 21;
const DEPRECATE_EOF: u32 = 1 << 24;

/// A packet's most bytes; one this long goes on in the next.
const CONTINUED: usize = 0xff_ffff;
/// The largest packet we take: 16 MB (what we tell the server).
const MAX_PACKET: u32 = 1 << 24;
/// utf8mb4_general_ci — every MySQL and MariaDB knows it.
const UTF8MB4: u8 = 45;
const COM_QUERY: u8 = 0x03;
const COM_STMT_PREPARE: u8 = 0x16;
const COM_STMT_CLOSE: u8 = 0x19;

const NATIVE: &str = "mysql_native_password";
const CACHING_SHA2: &str = "caching_sha2_password";

enum State {
    /// Waiting for the server's handshake.
    Greeting,
    /// The TLS handshake under way; our response waits for it.
    Handshake,
    /// Our response sent: signing in with this plugin.
    Auth(&'static str),
    /// caching_sha2's full authentication: the server's RSA key asked for.
    Key,
    /// Signed in; `SET SESSION TRANSACTION READ ONLY` sent.
    Starting,
    Idle,
    Busy(Box<Busy>),
    Over,
}

/// A request, as asked.
enum Request {
    Query(String),
    Describe(String),
    Kill(u32),
    /// As it is (tests' setup only — `raw`).
    #[cfg_attr(not(test), allow(dead_code))]
    Raw(String),
}

/// One command on the wire, and how its answer counts.
enum Cmd {
    /// Ours (`START TRANSACTION READ ONLY`, `ROLLBACK`): an OK expected; refused, the statement it guards isn't run.
    Guard(String),
    /// The statement asked: its answer is the request's.
    Statement(String),
    Prepare(String),
}

/// A request under way: its commands still to send, the one in flight, what's coming back, its answer once known.
struct Busy {
    cmd: Cmd,
    steps: VecDeque<Cmd>,
    reading: Reading,
    columns: Vec<Column>,
    params: Vec<u32>,
    rows: Vec<Vec<Option<String>>>,
    answer: Option<Event>,
}

/// Where in a command's answer.
enum Reading {
    First,
    /// Column definitions still to come (then an EOF, without `DEPRECATE_EOF`).
    Columns(usize),
    ColumnsEof,
    Rows,
    /// A prepared statement's: its id; parameter, then column definitions (each set ending in an EOF without
    /// `DEPRECATE_EOF`) still to come.
    Prepared { id: u32, params: usize, columns: usize, eof_after_params: bool, eof_after_columns: bool },
}

pub struct Session {
    params: Params,
    /// The caller's random bytes: RSA-OAEP's padding draws from them.
    seed: [u8; 32],
    state: State,
    tls: Option<Tls>,
    /// Plaintext from the server not yet parsed.
    buf: Vec<u8>,
    /// The next packet's sequence number.
    seq: u8,
    caps: u32,
    nonce: Vec<u8>,
    plugin: String,
    version: String,
    connection_id: u32,
    queue: VecDeque<Request>,
}

impl Session {
    /// A session — MySQL speaks first, so nothing to send yet.
    pub fn start(params: Params, seed: [u8; 32]) -> (Session, Vec<Event>) {
        let s = Session {
            params,
            seed,
            state: State::Greeting,
            tls: None,
            buf: Vec::new(),
            seq: 0,
            caps: 0,
            nonce: Vec::new(),
            plugin: String::new(),
            version: String::new(),
            connection_id: 0,
            queue: VecDeque::new(),
        };
        (s, Vec::new())
    }

    /// Runs `sql` in a read-only transaction (its rows come back as text) — unless `sql::refusal` has it.
    // (step 4's verdicts and step 6's runs are what call these)
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn query(&mut self, sql: &str) -> Vec<Event> {
        if let Some(message) = sql::refusal(sql) {
            return vec![Event::Refused(Refusal { code: String::new(), sqlstate: None, message: message.into(), position: None })];
        }
        self.queue.push_back(Request::Query(sql.to_string()));
        self.next()
    }

    /// Describes `sql` without running it: prepared, its parameters and columns read, closed — never executed.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn describe(&mut self, sql: &str) -> Vec<Event> {
        self.queue.push_back(Request::Describe(sql.to_string()));
        self.next()
    }

    /// Stops what another session of the same account runs (`KILL QUERY`, by its connection id) — a cancel is
    /// this, sent on a second session.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn kill_query(&mut self, connection_id: u32) -> Vec<Event> {
        self.queue.push_back(Request::Kill(connection_id));
        self.next()
    }

    /// This session's connection id, as the server said it (what a cancel names).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn connection_id(&self) -> u32 {
        self.connection_id
    }

    #[cfg_attr(not(test), allow(dead_code))]
    pub fn idle(&self) -> bool {
        matches!(self.state, State::Idle) && self.queue.is_empty()
    }

    /// Runs `sql` in the read-only transaction without the plugin's own refusal — for the live test that shows
    /// why the refusal is there.
    #[cfg(test)]
    pub fn query_unrefused(&mut self, sql: &str) -> Vec<Event> {
        self.queue.push_back(Request::Query(sql.to_string()));
        self.next()
    }

    /// Runs `sql` as it is — no refusal, no transaction of ours: the tests' own setup (`FLUSH PRIVILEGES`).
    #[cfg(test)]
    pub fn raw(&mut self, sql: &str) -> Vec<Event> {
        self.queue.push_back(Request::Raw(sql.to_string()));
        self.next()
    }

    /// Bytes that came from the server.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        if let Some(t) = &mut self.tls {
            if let Err(why) = t.feed(bytes) {
                return self.fail(why);
            }
            let plain = t.read();
            if matches!(self.state, State::Handshake) {
                out.push(Event::Send(t.outgoing()));
                if !t.handshaking() {
                    out.extend(self.respond());
                }
            }
            self.buf.extend_from_slice(&plain);
        } else {
            self.buf.extend_from_slice(bytes);
        }
        while !matches!(self.state, State::Over | State::Handshake) {
            match self.packet() {
                Ok(Some(p)) => out.extend(self.packet_came(p)),
                Ok(None) => break,
                Err(why) => out.extend(self.fail(why)),
            }
        }
        out.retain(|e| !matches!(e, Event::Send(b) if b.is_empty()));
        out
    }

    /// One whole packet from what came (a 16 MB − 1 one goes on in the next, joined), its sequence checked.
    fn packet(&mut self) -> Result<Option<Vec<u8>>, String> {
        let (mut at, mut payload, mut seq) = (0, Vec::new(), self.seq);
        loop {
            let Some(head) = self.buf.get(at..at + 4) else { return Ok(None) };
            let len = head[0] as usize | (head[1] as usize) << 8 | (head[2] as usize) << 16;
            if head[3] != seq {
                return Err("the server's packets came out of order".into());
            }
            let Some(body) = self.buf.get(at + 4..at + 4 + len) else { return Ok(None) };
            payload.extend_from_slice(body);
            (at, seq) = (at + 4 + len, seq.wrapping_add(1));
            if len < CONTINUED {
                break;
            }
        }
        self.buf.drain(..at);
        self.seq = seq;
        Ok(Some(payload))
    }

    fn packet_came(&mut self, p: Vec<u8>) -> Vec<Event> {
        match std::mem::replace(&mut self.state, State::Over) {
            State::Greeting => self.greeting(&p),
            State::Auth(plugin) => self.auth(plugin, &p),
            State::Key => match p.split_first() {
                Some((0x01, pem)) => self.rsa(pem),
                _ => self.fail("the server didn't send its key for caching_sha2's full authentication".into()),
            },
            State::Starting => match p.first() {
                Some(0x00) => {
                    self.state = State::Idle;
                    let mut out = vec![Event::Ready { version: self.version.clone() }];
                    out.extend(self.next());
                    out
                }
                Some(0xff) => {
                    let e = error(&p);
                    self.fail(format!("read-only couldn't be set: {} ({})", e.message, e.code))
                }
                _ => self.fail("the server answered setting read-only with something else".into()),
            },
            State::Busy(b) => self.answer(b, &p),
            State::Idle | State::Handshake | State::Over => self.fail("the server sent a packet nothing asked for".into()),
        }
    }

    // ── Signing in ──

    fn greeting(&mut self, p: &[u8]) -> Vec<Event> {
        if p.first() == Some(&0xff) {
            if self.params.tls.is_some() {
                // Before TLS, anyone in the way may have written it: never shown (as PostgreSQL's, CVE-2024-10977)
                return self.fail("the server refused before TLS — what it said isn't shown, it came before TLS".into());
            }
            let e = error(p);
            return self.fail(format!("{} ({})", e.message, e.code));
        }
        let Some(h) = Handshake::read(p) else { return self.fail("the server's greeting isn't MySQL's protocol 10".into()) };
        (self.version, self.connection_id, self.nonce, self.caps, self.plugin) = (h.version, h.connection_id, h.nonce, h.caps, h.plugin);
        let needed = PROTOCOL_41 | SECURE_CONNECTION | PLUGIN_AUTH;
        if self.caps & needed != needed {
            return self.fail("the server is older than the protocol the plugin speaks (4.1, with auth plugins)".into());
        }
        let Some((config, name)) = self.params.tls.clone() else { return self.respond() };
        // TLS asked: a server that doesn't offer it is never carried on with in the clear (the bit may have been
        // taken out on the way)
        if self.caps & SSL == 0 {
            return self.fail("the server doesn't offer TLS — `tls = \"off\"` for one on this machine".into());
        }
        // Bytes after the greeting, before TLS: someone in the way getting words in ahead of the encryption, to be read
        // as if they came inside it (PostgreSQL's CVE-2021-23222 has this shape) — not taken
        if !self.buf.is_empty() {
            return self.fail("the server sent more before TLS than its greeting — not taken, the connection is closed".into());
        }
        let mut request = self.client_flags().to_le_bytes().to_vec();
        request.extend(MAX_PACKET.to_le_bytes());
        request.push(UTF8MB4);
        request.extend([0; 23]);
        let ssl_request = self.frame(&request);
        match Tls::client(config, &name) {
            Ok(mut t) => {
                let hello = t.outgoing();
                self.tls = Some(t);
                self.state = State::Handshake;
                vec![Event::Send(ssl_request), Event::Send(hello)]
            }
            Err(why) => self.fail(format!("TLS: {why}")),
        }
    }

    fn client_flags(&self) -> u32 {
        let mut f = LONG_PASSWORD | PROTOCOL_41 | TRANSACTIONS | SECURE_CONNECTION | PLUGIN_AUTH;
        f |= self.caps & (PLUGIN_AUTH_LENENC_CLIENT_DATA | DEPRECATE_EOF);
        if self.params.database.is_some() {
            f |= CONNECT_WITH_DB & self.caps;
        }
        if self.params.tls.is_some() {
            f |= SSL;
        }
        f
    }

    /// HandshakeResponse41 — answered with the server's plugin if the plugin speaks it, else caching_sha2's (the
    /// server then switches us to the account's).
    fn respond(&mut self) -> Vec<Event> {
        let flags = self.client_flags();
        let plugin = if self.plugin == NATIVE { NATIVE } else { CACHING_SHA2 };
        let answer = self.scramble(plugin);
        let mut r = flags.to_le_bytes().to_vec();
        r.extend(MAX_PACKET.to_le_bytes());
        r.push(UTF8MB4);
        r.extend([0; 23]);
        r.extend(self.params.user.as_bytes());
        r.push(0);
        if flags & PLUGIN_AUTH_LENENC_CLIENT_DATA != 0 {
            put_lenenc(&mut r, answer.len() as u64);
        } else {
            r.push(answer.len() as u8);
        }
        r.extend(&answer);
        if flags & CONNECT_WITH_DB != 0
            && let Some(db) = &self.params.database
        {
            r.extend(db.as_bytes());
            r.push(0);
        }
        r.extend(plugin.as_bytes());
        r.push(0);
        self.state = State::Auth(plugin);
        let framed = self.frame(&r);
        self.out(framed)
    }

    /// The plugin's answer to the server's nonce (none without a password).
    fn scramble(&self, plugin: &str) -> Vec<u8> {
        let Some(pw) = &self.params.password else { return Vec::new() };
        if pw.expose().is_empty() {
            return Vec::new();
        }
        let pw = pw.expose().as_bytes();
        let xor = |a: &[u8], b: &[u8]| a.iter().zip(b).map(|(x, y)| x ^ y).collect::<Vec<u8>>();
        if plugin == NATIVE {
            // SHA1(pw) XOR SHA1(nonce + SHA1(SHA1(pw)))
            let stage1 = Sha1::digest(pw);
            let stage2 = Sha1::digest(stage1);
            xor(&stage1, &Sha1::new().chain_update(&self.nonce).chain_update(stage2).finalize())
        } else {
            // SHA256(pw) XOR SHA256(SHA256(SHA256(pw)) + nonce) (VERIFIED `myp-page_caching_sha2_authentication_exchanges`)
            let stage1 = Sha256::digest(pw);
            let stage2 = Sha256::digest(stage1);
            xor(&stage1, &Sha256::new().chain_update(stage2).chain_update(&self.nonce).finalize())
        }
    }

    fn auth(&mut self, plugin: &'static str, p: &[u8]) -> Vec<Event> {
        match p.first() {
            Some(0x00) => self.signed_in(),
            Some(0xff) => {
                let e = error(p);
                self.fail(format!("{} ({})", e.message, e.code))
            }
            // An auth switch: another plugin, a fresh nonce
            Some(0xfe) => {
                let rest = &p[1..];
                let end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
                let name = String::from_utf8_lossy(&rest[..end]).into_owned();
                let data = rest.get(end + 1..).unwrap_or(&[]);
                let plugin = match name.as_str() {
                    NATIVE => NATIVE,
                    CACHING_SHA2 => CACHING_SHA2,
                    // (`mysql_clear_password` would send it as it is; others the plugin doesn't speak)
                    other => return self.fail(format!("the server asks to sign in with {other}, which the plugin doesn't do")),
                };
                self.nonce = data.strip_suffix(&[0]).unwrap_or(data).to_vec();
                let answer = self.scramble(plugin);
                self.state = State::Auth(plugin);
                let framed = self.frame(&answer);
                self.out(framed)
            }
            // caching_sha2's word: fast authentication done (an OK follows), or the full one needed
            Some(0x01) if plugin == CACHING_SHA2 => match p.get(1) {
                Some(0x03) => {
                    self.state = State::Auth(plugin);
                    Vec::new()
                }
                Some(0x04) => self.full_auth(),
                _ => self.fail("the server sent caching_sha2 a word it doesn't have".into()),
            },
            _ => self.fail("the server answered signing in with something else".into()),
        }
    }

    /// caching_sha2's full authentication: the password inside TLS; to this machine, RSA under the key the server
    /// sends; else not at all.
    fn full_auth(&mut self) -> Vec<Event> {
        if self.tls.is_some() {
            let mut pw = self.params.password.as_ref().map(|p| p.expose().as_bytes().to_vec()).unwrap_or_default();
            pw.push(0);
            self.state = State::Auth(CACHING_SHA2);
            let framed = self.frame(&pw);
            return self.out(framed);
        }
        if !self.params.loopback {
            return self.fail(
                "full authentication needs TLS here — without it the password would cross the network (sign in once \
                 over TLS: the server remembers it)"
                    .into(),
            );
        }
        self.state = State::Key;
        let framed = self.frame(&[0x02]);
        self.out(framed)
    }

    /// The password (NUL-ended), XORed with the nonce, RSA-OAEP(SHA-1)-encrypted under the server's key.
    fn rsa(&mut self, pem: &[u8]) -> Vec<Event> {
        let Ok(key) = std::str::from_utf8(pem).map_err(|_| ()).and_then(|k| RsaPublicKey::from_public_key_pem(k).map_err(|_| ())) else {
            return self.fail("the server's RSA key doesn't read".into());
        };
        let mut pw = self.params.password.as_ref().map(|p| p.expose().as_bytes().to_vec()).unwrap_or_default();
        pw.push(0);
        if !self.nonce.is_empty() {
            for (i, b) in pw.iter_mut().enumerate() {
                *b ^= self.nonce[i % self.nonce.len()];
            }
        }
        let mut rng = ChaCha20Rng::from_seed(self.seed);
        match key.encrypt(&mut rng, Oaep::new::<Sha1>(), &pw) {
            Ok(sealed) => {
                self.state = State::Auth(CACHING_SHA2);
                let framed = self.frame(&sealed);
                self.out(framed)
            }
            Err(_) => self.fail("the password doesn't fit the server's RSA key".into()),
        }
    }

    fn signed_in(&mut self) -> Vec<Event> {
        self.state = State::Starting;
        self.command(COM_QUERY, b"SET SESSION TRANSACTION READ ONLY")
    }

    // ── Statements ──

    /// The next request waiting, if the session is free for it.
    fn next(&mut self) -> Vec<Event> {
        if !matches!(self.state, State::Idle) {
            return Vec::new();
        }
        let Some(r) = self.queue.pop_front() else { return Vec::new() };
        let steps: VecDeque<Cmd> = match r {
            Request::Query(sql) => [
                Cmd::Guard("START TRANSACTION READ ONLY".into()),
                Cmd::Statement(sql),
                Cmd::Guard("ROLLBACK".into()),
            ]
            .into(),
            Request::Describe(sql) => [Cmd::Prepare(sql)].into(),
            Request::Kill(id) => [Cmd::Statement(format!("KILL QUERY {id}"))].into(),
            Request::Raw(sql) => [Cmd::Statement(sql)].into(),
        };
        let b = Box::new(Busy {
            cmd: Cmd::Guard(String::new()),
            steps,
            reading: Reading::First,
            columns: Vec::new(),
            params: Vec::new(),
            rows: Vec::new(),
            answer: None,
        });
        self.step(b)
    }

    /// Sends the request's next command, or — none left — gives its answer and goes on.
    fn step(&mut self, mut b: Box<Busy>) -> Vec<Event> {
        let Some(cmd) = b.steps.pop_front() else {
            self.state = State::Idle;
            let mut out: Vec<Event> = b.answer.into_iter().collect();
            out.extend(self.next());
            return out;
        };
        let (code, text) = match &cmd {
            Cmd::Guard(s) | Cmd::Statement(s) => (COM_QUERY, s.clone()),
            Cmd::Prepare(s) => (COM_STMT_PREPARE, s.clone()),
        };
        (b.cmd, b.reading) = (cmd, Reading::First);
        (b.columns, b.params, b.rows) = (Vec::new(), Vec::new(), Vec::new());
        self.state = State::Busy(b);
        self.command(code, text.as_bytes())
    }

    fn answer(&mut self, mut b: Box<Busy>, p: &[u8]) -> Vec<Event> {
        let deprecate_eof = self.client_flags() & DEPRECATE_EOF != 0;
        match std::mem::replace(&mut b.reading, Reading::First) {
            Reading::First => match (p.first(), &b.cmd) {
                (Some(0xff), cmd) => {
                    let e = error(p);
                    match cmd {
                        // The guard refused: the statement isn't run (its ROLLBACK still is)
                        Cmd::Guard(_) if b.answer.is_none() => {
                            b.steps.retain(|c| matches!(c, Cmd::Guard(_)));
                            b.answer = Some(Event::Refused(e));
                        }
                        Cmd::Guard(_) => {}
                        Cmd::Statement(_) | Cmd::Prepare(_) => b.answer = Some(Event::Refused(e)),
                    }
                    self.step(b)
                }
                (Some(0x00), Cmd::Prepare(_)) => {
                    let mut r = Reader(&p[1..]);
                    let (Some(id), Some(columns), Some(params)) = (r.u32(), r.u16(), r.u16()) else {
                        return self.fail("the server's answer to a prepare doesn't read".into());
                    };
                    let (params, columns) = (params as usize, columns as usize);
                    b.reading = Reading::Prepared {
                        id,
                        params,
                        columns,
                        eof_after_params: params > 0 && !deprecate_eof,
                        eof_after_columns: columns > 0 && !deprecate_eof,
                    };
                    self.prepared(b)
                }
                (Some(0x00), cmd) => {
                    if matches!(cmd, Cmd::Statement(_)) {
                        let affected = Reader(&p[1..]).lenenc().unwrap_or(0);
                        b.answer = Some(Event::Rows { columns: Vec::new(), rows: Vec::new(), tag: format!("{affected} rows affected") });
                    }
                    self.step(b)
                }
                // LOAD DATA LOCAL's request for a file of ours — never asked for: a server asking anyway isn't talked to
                (Some(0xfb), _) => self.fail("the server asked for a file from this machine (LOAD DATA LOCAL) — refused, connection closed".into()),
                (Some(_), Cmd::Statement(_)) => match Reader(p).lenenc() {
                    Some(n) if n > 0 => {
                        b.reading = Reading::Columns(n as usize);
                        self.go(b)
                    }
                    _ => self.fail("the server's result doesn't read".into()),
                },
                _ => self.fail("the server answered with something else".into()),
            },
            Reading::Columns(n) => {
                let Some(c) = column(p) else { return self.fail("a column definition doesn't read".into()) };
                b.columns.push(c);
                b.reading = match n - 1 {
                    0 if deprecate_eof => Reading::Rows,
                    0 => Reading::ColumnsEof,
                    left => Reading::Columns(left),
                };
                self.go(b)
            }
            Reading::ColumnsEof => {
                b.reading = Reading::Rows;
                self.go(b)
            }
            Reading::Rows => match p.first() {
                // The end: an EOF/OK whose header is 0xFE (a row starting so is 16 MB long, never this short)
                Some(0xfe) if p.len() < CONTINUED => {
                    let n = b.rows.len();
                    let (columns, rows) = (std::mem::take(&mut b.columns), std::mem::take(&mut b.rows));
                    b.answer = Some(Event::Rows { columns, rows, tag: format!("{n} rows") });
                    self.step(b)
                }
                Some(0xff) => {
                    b.answer = Some(Event::Refused(error(p)));
                    self.step(b)
                }
                _ => {
                    let mut r = Reader(p);
                    let mut row = Vec::with_capacity(b.columns.len());
                    for _ in 0..b.columns.len() {
                        match r.cell() {
                            Some(cell) => row.push(cell),
                            None => return self.fail("the server sent a row that doesn't read".into()),
                        }
                    }
                    b.rows.push(row);
                    b.reading = Reading::Rows;
                    self.go(b)
                }
            },
            prepared @ Reading::Prepared { .. } => {
                b.reading = prepared;
                self.prepared_packet(b, p)
            }
        }
    }

    /// A prepared statement's definitions, one packet at a time.
    fn prepared_packet(&mut self, mut b: Box<Busy>, p: &[u8]) -> Vec<Event> {
        let Reading::Prepared { id, params, columns, eof_after_params, eof_after_columns } = b.reading else { unreachable!() };
        if params > 0 {
            let Some(c) = column(p) else { return self.fail("a parameter definition doesn't read".into()) };
            b.params.push(c.type_id);
            b.reading = Reading::Prepared { id, params: params - 1, columns, eof_after_params, eof_after_columns };
        } else if eof_after_params {
            b.reading = Reading::Prepared { id, params, columns, eof_after_params: false, eof_after_columns };
        } else if columns > 0 {
            let Some(c) = column(p) else { return self.fail("a column definition doesn't read".into()) };
            b.columns.push(c);
            b.reading = Reading::Prepared { id, params, columns: columns - 1, eof_after_params, eof_after_columns };
        } else {
            b.reading = Reading::Prepared { id, params, columns, eof_after_params, eof_after_columns: false };
        }
        self.prepared(b)
    }

    /// Everything of the prepared statement read: it's closed (no answer comes) — never executed.
    fn prepared(&mut self, mut b: Box<Busy>) -> Vec<Event> {
        let Reading::Prepared { id, params: 0, columns: 0, eof_after_params: false, eof_after_columns: false } = b.reading else {
            return self.go(b);
        };
        let (params, columns) = (std::mem::take(&mut b.params), std::mem::take(&mut b.columns));
        b.answer = Some(Event::Described { params, columns });
        let mut close = vec![COM_STMT_CLOSE];
        close.extend(id.to_le_bytes());
        self.seq = 0;
        let framed = self.frame(&close);
        let mut out = self.out(framed);
        out.extend(self.step(b));
        out
    }

    fn go(&mut self, b: Box<Busy>) -> Vec<Event> {
        self.state = State::Busy(b);
        Vec::new()
    }

    // ── Bytes out ──

    /// A command: its sequence starts over.
    fn command(&mut self, code: u8, body: &[u8]) -> Vec<Event> {
        self.seq = 0;
        let mut payload = Vec::with_capacity(1 + body.len());
        payload.push(code);
        payload.extend_from_slice(body);
        let framed = self.frame(&payload);
        self.out(framed)
    }

    /// A payload in packets: 16 MB − 1 at most each (one exactly that long followed by a shorter, maybe empty, one).
    fn frame(&mut self, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(payload.len() + 4);
        let mut chunks = payload.chunks(CONTINUED).peekable();
        let mut last = 0;
        while let Some(c) = chunks.next() {
            out.extend(&(c.len() as u32).to_le_bytes()[..3]);
            out.push(self.seq);
            out.extend(c);
            self.seq = self.seq.wrapping_add(1);
            last = c.len();
            if chunks.peek().is_none() {
                break;
            }
        }
        if payload.is_empty() || last == CONTINUED {
            out.extend([0, 0, 0, self.seq]);
            self.seq = self.seq.wrapping_add(1);
        }
        out
    }

    /// Plaintext out — through TLS when there's TLS.
    fn out(&mut self, plain: Vec<u8>) -> Vec<Event> {
        match &mut self.tls {
            Some(t) => match t.write(&plain) {
                Ok(()) => vec![Event::Send(t.outgoing())],
                Err(why) => self.fail(why),
            },
            None => vec![Event::Send(plain)],
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

/// The server's HandshakeV10 (VERIFIED `myp-page_protocol_connection_phase_packets_protocol_handshake_v10`).
struct Handshake {
    version: String,
    connection_id: u32,
    caps: u32,
    nonce: Vec<u8>,
    plugin: String,
}

impl Handshake {
    fn read(p: &[u8]) -> Option<Handshake> {
        let mut r = Reader(p);
        if r.u8()? != 10 {
            return None;
        }
        let version = String::from_utf8_lossy(r.nul()?).into_owned();
        let connection_id = r.u32()?;
        let mut nonce = r.take(8)?.to_vec();
        r.u8()?;
        let low = r.u16()? as u32;
        let (_charset, _status) = (r.u8()?, r.u16()?);
        let caps = low | (r.u16()? as u32) << 16;
        let auth_len = r.u8()? as usize;
        r.take(10)?;
        // Part 2: max(13, its length − 8), NUL-ended — the nonce is 20 bytes in all
        let part2 = r.take(13.max(auth_len.saturating_sub(8)))?;
        nonce.extend(part2.strip_suffix(&[0]).unwrap_or(part2));
        let plugin = if caps & PLUGIN_AUTH != 0 { String::from_utf8_lossy(r.nul().unwrap_or(&[])).into_owned() } else { String::new() };
        Some(Handshake { version, connection_id, caps, nonce, plugin })
    }
}

/// ERR_Packet: 0xFF, its number, `#` and the SQLSTATE, the message (VERIFIED `myp-page_protocol_basic_err_packet`).
fn error(p: &[u8]) -> Refusal {
    let code = p.get(1..3).map_or(0, |c| u16::from_le_bytes([c[0], c[1]]));
    let (sqlstate, message) = match p.get(3) {
        Some(b'#') => (p.get(4..9).map(|s| String::from_utf8_lossy(s).into_owned()), p.get(9..).unwrap_or(&[])),
        _ => (None, p.get(3..).unwrap_or(&[])),
    };
    Refusal { code: code.to_string(), sqlstate, message: String::from_utf8_lossy(message).into_owned(), position: None }
}

/// ColumnDefinition41: catalog, schema, table, org_table, name, org_name, then the fixed part (charset, length,
/// type, flags, decimals) — VERIFIED `myp-page_protocol_com_query_response_text_resultset_column_definition`.
fn column(p: &[u8]) -> Option<Column> {
    let mut r = Reader(p);
    let mut text = || r.lenenc_bytes().map(|b| String::from_utf8_lossy(b).into_owned());
    let (_catalog, schema, _table, org_table, name, _org_name) = (text()?, text()?, text()?, text()?, text()?, text()?);
    r.lenenc()?;
    let (_charset, _length, type_id) = (r.u16()?, r.u32()?, r.u8()?);
    let table = if org_table.is_empty() { Table::None } else { Table::Named { schema, name: org_table } };
    Some(Column { name, type_id: type_id as u32, table })
}

fn put_lenenc(out: &mut Vec<u8>, n: u64) {
    match n {
        0..0xfb => out.push(n as u8),
        0xfb..0x1_0000 => {
            out.push(0xfc);
            out.extend(&(n as u16).to_le_bytes());
        }
        0x1_0000..0x100_0000 => {
            out.push(0xfd);
            out.extend(&(n as u32).to_le_bytes()[..3]);
        }
        _ => {
            out.push(0xfe);
            out.extend(n.to_le_bytes());
        }
    }
}

/// Reads a packet front to back; `None` when what's asked for isn't there.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let (head, rest) = (self.0.get(..n)?, self.0.get(n..)?);
        self.0 = rest;
        Some(head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }

    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self) -> Option<u32> {
        self.take(4).map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn nul(&mut self) -> Option<&'a [u8]> {
        let end = self.0.iter().position(|&c| c == 0)?;
        let s = self.take(end)?;
        self.take(1)?;
        Some(s)
    }

    /// A length-encoded integer (VERIFIED `myp-page_protocol_basic_dt_integers`).
    fn lenenc(&mut self) -> Option<u64> {
        Some(match self.u8()? {
            n @ 0..=0xfa => n as u64,
            // (0xFB is a row's NULL — `cell` — and 0xFF an error: neither is a length)
            0xfb | 0xff => return None,
            0xfc => self.u16()? as u64,
            0xfd => {
                let b = self.take(3)?;
                u32::from_le_bytes([b[0], b[1], b[2], 0]) as u64
            }
            0xfe => u64::from_le_bytes(self.take(8)?.try_into().ok()?),
        })
    }

    fn lenenc_bytes(&mut self) -> Option<&'a [u8]> {
        let n = usize::try_from(self.lenenc()?).ok()?;
        self.take(n)
    }

    /// A row's cell: `0xFB` is NULL, else a length-encoded string.
    fn cell(&mut self) -> Option<Option<String>> {
        if self.0.first() == Some(&0xfb) {
            self.take(1)?;
            return Some(None);
        }
        self.lenenc_bytes().map(|b| Some(String::from_utf8_lossy(b).into_owned()))
    }
}

#[cfg(test)]
mod tests;
