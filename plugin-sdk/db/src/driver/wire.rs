//! What every database's wire says, in one shape (db-plan.md §12.1): a session is fed the bytes that came and
//! answers with these events, so the plugin's plumbing — connecting, sending, what the tab and the badge say — is
//! written once for all of them.

use std::sync::Arc;

use tarae_plugin::tls::Config;

use crate::secret::Secret;

/// What a session needs to sign in.
pub struct Params {
    pub user: String,
    pub database: Option<String>,
    pub password: Option<Secret>,
    /// TLS: the client's configuration and the name the server's certificate must carry.
    pub tls: Option<(Arc<Config>, String)>,
    /// The server is this machine.
    pub loopback: bool,
}

impl Params {
    /// Whether a password — or what gives it away (MD5's answer, a cleartext exchange) — may cross: only over TLS
    /// or to this machine, never across a network in the clear (§4.1).
    pub fn may_send_password(&self) -> bool {
        self.tls.is_some() || self.loopback
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Column {
    pub name: String,
    /// The database's own type id (PostgreSQL's type OID, MySQL's type byte).
    pub type_id: u32,
    pub table: Table,
}

/// The table a column comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Table {
    /// None: an expression.
    None,
    /// PostgreSQL's: a `pg_class` OID, named by asking.
    Oid(u32),
    /// MySQL's: named in the column's definition (the table itself, not an alias).
    Named { schema: String, name: String },
}

/// A statement the server refused.
#[derive(Clone, Debug, PartialEq)]
pub struct Refusal {
    /// The database's own code (PostgreSQL: the SQLSTATE; MySQL: its error number).
    pub code: String,
    /// The SQLSTATE where the database gives one — the same across databases (25006: a write in a read-only
    /// transaction, PostgreSQL's and MySQL's).
    pub sqlstate: Option<String>,
    pub message: String,
    /// Where in the statement, in characters from 1.
    pub position: Option<u32>,
}

#[derive(Debug, PartialEq)]
pub enum Event {
    /// Bytes to send on the connection.
    Send(Vec<u8>),
    /// Signed in, ready for statements: the server's version.
    Ready { version: String },
    /// A statement described, not run: its parameters' types, its columns (none: it returns no rows).
    Described { params: Vec<u32>, columns: Vec<Column> },
    /// A statement's answer: its columns, its rows as text (NULL: none), what the server said it did (`SELECT 2`).
    Rows { columns: Vec<Column>, rows: Vec<Vec<Option<String>>>, tag: String },
    /// A key pattern looked up (Redis): how many keys match — at least, when the walk stopped at its cap — and one
    /// matching key's type and time to live (ms; none: it doesn't expire).
    Keys { matched: u64, capped: bool, sample: Option<(String, String, Option<i64>)> },
    /// The server refused a statement; the session goes on.
    Refused(Refusal),
    /// The connection is over — why, in words that hold no secret.
    Failed(String),
}

/// A database's session, as the plugin drives it.
pub trait Session {
    /// Bytes that came from the server.
    fn feed(&mut self, bytes: &[u8]) -> Vec<Event>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::{mysql, postgres};

    fn said(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::Failed(w) => Some(w.clone()),
                Event::Refused(r) => Some(format!("{r:?}")),
                _ => None,
            })
            .collect()
    }

    fn pg(tag: u8, body: &[u8]) -> Vec<u8> {
        [&[tag][..], &((body.len() + 4) as u32).to_be_bytes(), body].concat()
    }

    fn my(seq: u8, payload: &[u8]) -> Vec<u8> {
        [&(payload.len() as u32).to_le_bytes()[..3], &[seq], payload].concat()
    }

    /// The words a failure is said in are the plugin's or the server's — never the password, whatever it holds:
    /// driven through each way signing in can fail in both wires (refused off this machine, a plugin or mechanism
    /// not spoken, a server's refusal, a proof that doesn't match, a key that doesn't read), with step 1's passwords.
    #[test]
    fn no_failure_quotes_the_password() {
        let greeting = {
            let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/driver/mysql/captured");
            let server = std::fs::read(dir.join("signin-fast.server.raw")).unwrap();
            let len = server[0] as usize | (server[1] as usize) << 8;
            server[4..4 + len].to_vec()
        };
        for secret in crate::tests::fake_secrets(40) {
            let params = |loopback| Params {
                user: "app".into(),
                database: Some("shop".into()),
                password: Some(Secret::new(secret.as_str())),
                tls: None,
                loopback,
            };
            let mut out = Vec::new();
            // PostgreSQL: a password in plain text or MD5 off this machine; SCRAM refused, a nonce not ours, a
            // server proof that doesn't match; the server's own refusal
            for (loopback, answers) in [
                (false, vec![pg(b'R', &[0, 0, 0, 3])]),
                (false, vec![pg(b'R', &[0, 0, 0, 5, 1, 2, 3, 4])]),
                (true, vec![pg(b'R', b"\0\0\0\x0aSCRAM-SHA-256\0\0"), pg(b'R', b"\0\0\0\x0br=not-ours,s=c2FsdA==,i=1")]),
                (true, vec![pg(b'R', b"\0\0\0\x0aSCRAM-SHA-256\0\0"), pg(b'R', b"\0\0\0\x0ce=invalid-proof")]),
                (true, vec![pg(b'E', b"SFATAL\0C28P01\0Mpassword authentication failed for user \"app\"\0\0")]),
            ] {
                let (mut s, _) = postgres::wire::Session::start(params(loopback), [7; 18]);
                for a in answers {
                    out.extend(s.feed(&a));
                }
            }
            // MySQL: a plugin not spoken, full authentication off this machine, a key that doesn't read, the
            // server's refusal
            for (loopback, answers) in [
                (true, vec![my(2, b"\xfemysql_clear_password\0")]),
                (false, vec![my(2, &[0x01, 0x04])]),
                (true, vec![my(2, &[0x01, 0x04]), my(4, b"\x01-----BEGIN PUBLIC KEY-----\nnot a key\n")]),
                (true, vec![my(2, b"\xff\x15\x04#28000Access denied for user 'app'@'localhost' (using password: YES)")]),
            ] {
                let (mut s, _) = mysql::wire::Session::start(params(loopback), [7; 32]);
                out.extend(s.feed(&my(0, &greeting)));
                for a in answers {
                    out.extend(s.feed(&a));
                }
            }
            // Redis: AUTH off this machine without TLS, the server's refusal
            let redis = |loopback| Params { database: None, ..params(loopback) };
            out.extend(crate::driver::redis::wire::Session::start(redis(false), [0; 32]).1);
            let (mut s, _) = crate::driver::redis::wire::Session::start(redis(true), [0; 32]);
            out.extend(s.feed(b"-WRONGPASS invalid username-password pair or user is disabled.\r\n"));
            let said = said(&out);
            assert!(said.len() >= 11, "each way failed — else this proves nothing: {said:?}");
            for w in said {
                assert!(!w.contains(&secret), "{secret:?} in {w:?}");
            }
        }
    }
}
