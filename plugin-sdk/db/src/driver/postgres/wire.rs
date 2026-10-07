//! PostgreSQL's wire without I/O: a `Session` is fed the bytes that came and answers with events — bytes to send,
//! signed in, a statement described or answered, refused, the connection over. The plugin's plumbing carries the
//! bytes; tests feed transcripts captured from a real server (`captured/`).
//!
//! Signing in: `SSLRequest` first when TLS is asked (the SDK's `Tls` then carries everything), the startup message
//! (with `default_transaction_read_only=on`), then trust, a plain-text password (only over TLS or to this machine),
//! MD5, or SCRAM-SHA-256 — ours, so the nonce is the caller's to give (db-plan.md §11).

use std::collections::{BTreeMap, VecDeque};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::BytesMut;
use fallible_iterator::FallibleIterator;
use hmac::{Hmac, Mac};
use postgres_protocol::authentication::md5_hash;
use postgres_protocol::message::backend::{ErrorResponseBody, Message};
use postgres_protocol::message::frontend;
use sha2::{Digest, Sha256};
use tarae_plugin::tls::Tls;

use crate::driver::wire::{Column, Event, Params, Refusal, Table};
use crate::secret::Secret;

/// The most SCRAM iterations computed in one call: step 0 measured ~21 000 fitting the interpreter's fuel
/// (~85 000 the JIT's); PostgreSQL's default is 4096. More is refused, said why.
pub const SCRAM_MAX_ITERATIONS: u32 = 16_384;

enum State {
    /// `SSLRequest` sent: one byte answers it.
    SslAnswer,
    /// The TLS handshake under way.
    Handshake,
    /// The startup message sent: authenticating.
    Auth,
    /// SCRAM under way.
    Scram(Scram),
    /// Signed in, the server's parameters coming, then `ReadyForQuery`.
    Starting,
    Idle,
    Busy(Answer),
    Over,
}

/// What's being put together for the statement in flight.
enum Answer {
    Describe { params: Vec<u32>, columns: Vec<Column> },
    Query { columns: Vec<Column>, rows: Vec<Vec<Option<String>>>, tag: String },
    /// Refused: what's left is waited out to `ReadyForQuery`.
    Refused,
}

enum Request {
    Describe(String),
    Query(String),
}

pub struct Session {
    params: Params,
    /// The nonce's bytes for SCRAM (18, the caller's).
    nonce: [u8; 18],
    state: State,
    tls: Option<Tls>,
    /// Plaintext from the server not yet parsed.
    buf: BytesMut,
    server: BTreeMap<String, String>,
    /// (process id, secret key) — what a cancel needs.
    backend_key: Option<(i32, i32)>,
    queue: VecDeque<Request>,
}

impl Session {
    /// A session, and the bytes that open it.
    pub fn start(params: Params, nonce: [u8; 18]) -> (Session, Vec<Event>) {
        let tls = params.tls.is_some();
        let mut s = Session {
            params,
            nonce,
            state: State::Over,
            tls: None,
            buf: BytesMut::new(),
            server: BTreeMap::new(),
            backend_key: None,
            queue: VecDeque::new(),
        };
        if tls {
            let mut b = BytesMut::new();
            frontend::ssl_request(&mut b);
            s.state = State::SslAnswer;
            return (s, vec![Event::Send(b.to_vec())]);
        }
        let out = s.startup();
        (s, out)
    }

    /// Describes `sql` without running it (Parse + Describe + Sync — never Execute).
    // (step 4's verdicts and step 6's runs are what call these four)
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn describe(&mut self, sql: &str) -> Vec<Event> {
        self.queue.push_back(Request::Describe(sql.to_string()));
        self.next()
    }

    /// Runs `sql` (the simple protocol: its rows come back as text).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn query(&mut self, sql: &str) -> Vec<Event> {
        self.queue.push_back(Request::Query(sql.to_string()));
        self.next()
    }

    /// Signed in and nothing in flight.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn idle(&self) -> bool {
        matches!(self.state, State::Idle) && self.queue.is_empty()
    }

    /// The bytes that cancel what this session runs — sent on a connection of their own.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn cancel_request(&self) -> Option<Vec<u8>> {
        let (pid, key) = self.backend_key?;
        let mut b = BytesMut::new();
        frontend::cancel_request(pid, key, &mut b);
        Some(b.to_vec())
    }

    /// Bytes that came from the server.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        let mut bytes = bytes;
        if matches!(self.state, State::SslAnswer) {
            let Some((&answer, rest)) = bytes.split_first() else { return out };
            match (answer, self.params.tls.clone()) {
                (b'S', Some((config, name))) => match Tls::client(config, &name) {
                    Ok(mut t) => {
                        out.push(Event::Send(t.outgoing()));
                        self.tls = Some(t);
                        self.state = State::Handshake;
                    }
                    Err(why) => return self.fail(format!("TLS: {why}")),
                },
                (b'N', _) => return self.fail("the server doesn't offer TLS — `tls = \"off\"` for one on this machine".into()),
                // An error before TLS isn't the checked server's: never shown (CVE-2024-10977)
                _ => return self.fail("the server answered the TLS request with something else — not shown, it came before TLS".into()),
            }
            bytes = rest;
        }
        if let Some(t) = &mut self.tls {
            if let Err(why) = t.feed(bytes) {
                return self.fail(why);
            }
            let plain = t.read();
            if matches!(self.state, State::Handshake) {
                out.push(Event::Send(t.outgoing()));
                if !t.handshaking() {
                    out.extend(self.startup());
                }
            }
            self.buf.extend_from_slice(&plain);
        } else {
            self.buf.extend_from_slice(bytes);
        }
        loop {
            let msg = match Message::parse(&mut self.buf) {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(_) => {
                    out.extend(self.fail("the server sent what isn't PostgreSQL's wire".into()));
                    break;
                }
            };
            out.extend(self.message(msg));
            if matches!(self.state, State::Over) {
                break;
            }
        }
        out.retain(|e| !matches!(e, Event::Send(b) if b.is_empty()));
        out
    }

    fn message(&mut self, msg: Message) -> Vec<Event> {
        match (std::mem::replace(&mut self.state, State::Over), msg) {
            (State::Auth, Message::AuthenticationOk) => self.go(State::Starting),
            (State::Auth, Message::AuthenticationCleartextPassword) => {
                if !self.params.may_send_password() {
                    return self.fail("the server asks for the password in plain text, without TLS, off this machine — not sent".into());
                }
                let Some(pw) = self.params.password.clone() else { return self.no_password() };
                self.state = State::Auth;
                self.send(|b| frontend::password_message(pw.expose().as_bytes(), b))
            }
            (State::Auth, Message::AuthenticationMd5Password(body)) => {
                // Guarded as plain text is: in the clear off this machine, whoever is in the way picks the salt and
                // takes the answer away to crack
                if !self.params.may_send_password() {
                    return self.fail("the server asks for an MD5 password, without TLS, off this machine — not sent".into());
                }
                let Some(pw) = self.params.password.clone() else { return self.no_password() };
                let hash = md5_hash(self.params.user.as_bytes(), pw.expose().as_bytes(), body.salt());
                self.state = State::Auth;
                self.send(|b| frontend::password_message(hash.as_bytes(), b))
            }
            (State::Auth, Message::AuthenticationSasl(body)) => {
                let scram = body.mechanisms().any(|m| Ok(m == "SCRAM-SHA-256")).unwrap_or(false);
                if !scram {
                    return self.fail("the server asks for a SASL mechanism other than SCRAM-SHA-256".into());
                }
                let Some(pw) = self.params.password.clone() else { return self.no_password() };
                let s = Scram::new(&pw, &self.nonce);
                let first = s.client_first();
                self.state = State::Scram(s);
                self.send(|b| frontend::sasl_initial_response("SCRAM-SHA-256", first.as_bytes(), b))
            }
            (State::Scram(mut s), Message::AuthenticationSaslContinue(body)) => match s.server_first(body.data()) {
                Ok(fin) => {
                    self.state = State::Scram(s);
                    self.send(|b| frontend::sasl_response(fin.as_bytes(), b))
                }
                Err(why) => self.fail(why),
            },
            (State::Scram(s), Message::AuthenticationSaslFinal(body)) => match s.server_final(body.data()) {
                Ok(()) => {
                    self.state = State::Auth;
                    Vec::new()
                }
                Err(why) => self.fail(why),
            },
            (State::Auth | State::Scram(_) | State::Starting, Message::ErrorResponse(body)) => {
                let e = error(&body);
                self.fail(format!("{} ({})", e.message, e.code))
            }
            (state @ (State::Auth | State::Scram(_)), m) => {
                self.state = state;
                self.fail(format!("the server asked for a way of signing in the plugin doesn't speak ({})", kind(&m)))
            }
            (State::Starting, Message::ParameterStatus(body)) => {
                if let (Ok(k), Ok(v)) = (body.name(), body.value()) {
                    self.server.insert(k.to_string(), v.to_string());
                }
                self.go(State::Starting)
            }
            (State::Starting, Message::BackendKeyData(body)) => {
                self.backend_key = Some((body.process_id(), body.secret_key()));
                self.go(State::Starting)
            }
            (State::Starting, Message::ReadyForQuery(_)) => {
                let version = self.server.get("server_version").cloned().unwrap_or_default();
                self.state = State::Idle;
                let mut out = vec![Event::Ready { version }];
                out.extend(self.next());
                out
            }
            (State::Busy(a), m) => self.answer(a, m),
            (state, Message::NoticeResponse(_) | Message::ParameterStatus(_)) => self.go(state),
            (state, m) => {
                self.state = state;
                self.fail(format!("the server sent {} where it wasn't expected", kind(&m)))
            }
        }
    }

    fn answer(&mut self, a: Answer, m: Message) -> Vec<Event> {
        let next = |s: &mut Session, a| {
            s.state = State::Busy(a);
            Vec::new()
        };
        match (a, m) {
            (a, Message::ParseComplete | Message::BindComplete | Message::NoticeResponse(_) | Message::ParameterStatus(_)) => next(self, a),
            (Answer::Describe { columns, .. }, Message::ParameterDescription(body)) => {
                let params = body.parameters().collect().unwrap_or_default();
                next(self, Answer::Describe { params, columns })
            }
            (Answer::Describe { params, .. }, Message::RowDescription(body)) => {
                next(self, Answer::Describe { params, columns: columns(&body) })
            }
            (a @ Answer::Describe { .. }, Message::NoData) => next(self, a),
            (Answer::Query { rows, tag, .. }, Message::RowDescription(body)) => {
                next(self, Answer::Query { columns: columns(&body), rows, tag })
            }
            (Answer::Query { columns, mut rows, tag }, Message::DataRow(body)) => {
                let buf = body.buffer();
                let cells: Result<Vec<Option<String>>, _> =
                    body.ranges().map(|r| Ok(r.map(|r| String::from_utf8_lossy(&buf[r]).into_owned()))).collect();
                // A row that doesn't read is a stream that can't be trusted further — not an empty row
                let Ok(cells) = cells else {
                    return self.fail("the server sent a row that doesn't read".into());
                };
                rows.push(cells);
                next(self, Answer::Query { columns, rows, tag })
            }
            (Answer::Query { columns, rows, .. }, Message::CommandComplete(body)) => {
                let tag = body.tag().unwrap_or_default().to_string();
                next(self, Answer::Query { columns, rows, tag })
            }
            (a @ Answer::Query { .. }, Message::EmptyQueryResponse) => next(self, a),
            (_, Message::ErrorResponse(body)) => {
                self.state = State::Busy(Answer::Refused);
                vec![Event::Refused(error(&body))]
            }
            (a, Message::ReadyForQuery(_)) => {
                self.state = State::Idle;
                let mut out = match a {
                    Answer::Describe { params, columns } => vec![Event::Described { params, columns }],
                    Answer::Query { columns, rows, tag } => vec![Event::Rows { columns, rows, tag }],
                    Answer::Refused => Vec::new(),
                };
                out.extend(self.next());
                out
            }
            (Answer::Refused, _) => next(self, Answer::Refused),
            (a, m) => {
                self.state = State::Busy(a);
                self.fail(format!("the server sent {} where it wasn't expected", kind(&m)))
            }
        }
    }

    /// The next statement waiting, if the session is free for it.
    fn next(&mut self) -> Vec<Event> {
        if !matches!(self.state, State::Idle) {
            return Vec::new();
        }
        let Some(r) = self.queue.pop_front() else { return Vec::new() };
        let mut b = BytesMut::new();
        let (answer, written) = match r {
            Request::Describe(sql) => (
                Answer::Describe { params: Vec::new(), columns: Vec::new() },
                frontend::parse("", &sql, std::iter::empty(), &mut b).and_then(|()| {
                    frontend::describe(b'S', "", &mut b)?;
                    frontend::sync(&mut b);
                    Ok(())
                }),
            ),
            Request::Query(sql) => (
                Answer::Query { columns: Vec::new(), rows: Vec::new(), tag: String::new() },
                frontend::query(&sql, &mut b),
            ),
        };
        self.state = State::Busy(answer);
        if written.is_err() {
            // (a NUL in the statement: the wire can't carry it)
            self.state = State::Idle;
            let refusal = Refusal { code: String::new(), sqlstate: None, message: "the statement holds a NUL".into(), position: None };
            let mut out = vec![Event::Refused(refusal)];
            out.extend(self.next());
            return out;
        }
        self.out(b.to_vec())
    }

    fn startup(&mut self) -> Vec<Event> {
        let mut params = vec![("user", self.params.user.clone()), ("application_name", "tarae-db".to_string())];
        if let Some(db) = &self.params.database {
            params.push(("database", db.clone()));
        }
        params.extend([("client_encoding", "UTF8".to_string()), ("default_transaction_read_only", "on".to_string())]);
        let mut b = BytesMut::new();
        if frontend::startup_message(params.iter().map(|(k, v)| (*k, v.as_str())), &mut b).is_err() {
            return self.fail("the user or database name holds a NUL".into());
        }
        self.state = State::Auth;
        self.out(b.to_vec())
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

    fn send(&mut self, write: impl FnOnce(&mut BytesMut) -> std::io::Result<()>) -> Vec<Event> {
        let mut b = BytesMut::new();
        match write(&mut b) {
            Ok(()) => self.out(b.to_vec()),
            Err(_) => self.fail("a message the wire can't carry".into()),
        }
    }

    fn go(&mut self, state: State) -> Vec<Event> {
        self.state = state;
        Vec::new()
    }

    fn no_password(&mut self) -> Vec<Event> {
        self.fail("the server asks for a password and none is known for this connection".into())
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

fn columns(body: &postgres_protocol::message::backend::RowDescriptionBody) -> Vec<Column> {
    body.fields()
        .map(|f| {
            // (table OID 0: not a table's column — an expression)
            let table = if f.table_oid() == 0 { Table::None } else { Table::Oid(f.table_oid()) };
            Ok(Column { name: f.name().to_string(), type_id: f.type_oid(), table })
        })
        .collect()
        .unwrap_or_default()
}

fn error(body: &ErrorResponseBody) -> Refusal {
    let mut e = Refusal { code: String::new(), sqlstate: None, message: String::new(), position: None };
    let mut fields = body.fields();
    while let Ok(Some(f)) = fields.next() {
        let v = String::from_utf8_lossy(f.value_bytes()).into_owned();
        match f.type_() {
            b'C' => (e.code, e.sqlstate) = (v.clone(), Some(v)),
            b'M' => e.message = v,
            b'P' => e.position = v.parse().ok(),
            _ => {}
        }
    }
    e
}

/// A message's name, for what's said when it wasn't expected.
fn kind(m: &Message) -> &'static str {
    match m {
        Message::AuthenticationKerberosV5 => "Kerberos sign-in",
        Message::AuthenticationScmCredential => "SCM sign-in",
        Message::AuthenticationGss | Message::AuthenticationGssContinue(_) | Message::AuthenticationSspi => "GSS/SSPI sign-in",
        Message::CopyInResponse(_) | Message::CopyOutResponse(_) => "a COPY",
        _ => "a message",
    }
}

// ── SCRAM-SHA-256 (RFC 5802/7677), as PostgreSQL speaks it: no channel binding, the user name left empty ──

struct Scram {
    nonce: String,
    password: Vec<u8>,
    /// Once the server's first message came: the salted password and the whole exchange signed.
    salted: Option<[u8; 32]>,
    auth_message: String,
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut m = Hmac::<Sha256>::new_from_slice(key).expect("any key length");
    m.update(data);
    m.finalize().into_bytes().into()
}

impl Scram {
    fn new(password: &Secret, nonce: &[u8; 18]) -> Scram {
        // SASLprep, as PostgreSQL does — a password it can't normalize is used as it is
        let pw = password.expose();
        let password = stringprep::saslprep(pw).map(|p| p.into_owned()).unwrap_or_else(|_| pw.to_string()).into_bytes();
        Scram { nonce: STANDARD.encode(nonce), password, salted: None, auth_message: String::new() }
    }

    fn client_first(&self) -> String {
        format!("n,,n=,r={}", self.nonce)
    }

    /// The server's first message (its nonce, the salt, the iterations) → our proof.
    fn server_first(&mut self, data: &[u8]) -> Result<String, String> {
        let first = std::str::from_utf8(data).map_err(|_| "the server's SCRAM message isn't text")?;
        let field = |k: &str| first.split(',').find_map(|p| p.strip_prefix(k));
        let (Some(nonce), Some(salt), Some(i)) = (field("r="), field("s="), field("i=")) else {
            return Err("the server's SCRAM message lacks a part".into());
        };
        if !nonce.starts_with(&self.nonce) {
            return Err("the server's SCRAM nonce isn't ours".into());
        }
        let i: u32 = i.parse().map_err(|_| "the server's SCRAM iterations aren't a number")?;
        // RFC 5802: at least one — zero would make a proof of nothing, so a server (or anyone in the way) asking it
        // isn't answered
        if i < 1 {
            return Err("the server asks for no SCRAM iterations — at least one, RFC 5802 says".into());
        }
        if i > SCRAM_MAX_ITERATIONS {
            return Err(format!("the server asks for {i} SCRAM iterations — more than the plugin computes in one go ({SCRAM_MAX_ITERATIONS})"));
        }
        let salt = STANDARD.decode(salt).map_err(|_| "the server's SCRAM salt isn't base64")?;
        // PBKDF2-HMAC-SHA-256, one block
        let mut u = hmac(&self.password, &[salt.as_slice(), &[0, 0, 0, 1]].concat());
        let mut salted = u;
        for _ in 1..i {
            u = hmac(&self.password, &u);
            salted.iter_mut().zip(u).for_each(|(s, x)| *s ^= x);
        }
        let client_key = hmac(&salted, b"Client Key");
        let stored_key = Sha256::digest(client_key);
        let without_proof = format!("c=biws,r={nonce}");
        self.auth_message = format!("n=,r={},{first},{without_proof}", self.nonce);
        let signature = hmac(&stored_key, self.auth_message.as_bytes());
        let proof: Vec<u8> = client_key.iter().zip(signature).map(|(k, s)| k ^ s).collect();
        self.salted = Some(salted);
        Ok(format!("{without_proof},p={}", STANDARD.encode(proof)))
    }

    /// The server's last message: its own proof that it knows the password too.
    fn server_final(&self, data: &[u8]) -> Result<(), String> {
        let text = std::str::from_utf8(data).map_err(|_| "the server's SCRAM message isn't text")?;
        if let Some(e) = text.strip_prefix("e=") {
            return Err(format!("the server refused SCRAM: {e}"));
        }
        let salted = self.salted.ok_or("SCRAM ended before it began")?;
        let server_key = hmac(&salted, b"Server Key");
        let expected = STANDARD.encode(hmac(&server_key, self.auth_message.as_bytes()));
        if text.strip_prefix("v=") == Some(expected.as_str()) {
            Ok(())
        } else {
            Err("the server's SCRAM proof didn't match — it doesn't know this password: not the server it says it is?".into())
        }
    }
}

#[cfg(test)]
mod tests;
