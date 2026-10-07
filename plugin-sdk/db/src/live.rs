//! The connection in use, live (db-plan.md §11 Next 1): once one is in use and its password is at hand, it's
//! opened and its driver's session signs in; the tab and the badge say how it stands. One that dropped, or that
//! couldn't be reached, is tried again after a pause that doubles (2 s … 60 s); one the server turned away (a wrong
//! password, a certificate that doesn't check out) isn't — trying again would only knock until the account locks.
//!
//! TLS as the connection says (the user's decisions, §11 Next 3): off; verified — against its `ca-file`, else the
//! public CAs compiled in; or, only when named `require-unverified`, encrypted with the certificate unchecked.

use std::collections::BTreeMap;
use std::sync::Arc;

use tarae_plugin::tls::{self, Config};

use crate::conn::{Connection, Password, TlsMode, is_loopback};
use crate::driver::wire::{Event, Params, Session};
use crate::secret::Secret;
use crate::tarae::plugin::types::Connect;
use crate::{Action, HostEvent};

/// A connection's id: this plus its attempt's number (an old attempt's late bytes are told apart).
pub const CONNECT: u64 = 1_000_000;
/// The pause before trying again.
pub const RETRY: u64 = 8;
/// The password file, read for an attempt.
pub const PASSWORD_FILE: u64 = 9;
/// The CA file, read for an attempt.
pub const CA_FILE: u64 = 10;
const PAUSE_MS: [u64; 6] = [2_000, 4_000, 8_000, 16_000, 32_000, 60_000];

#[derive(Clone, Debug, PartialEq)]
pub enum Status {
    /// Not tried: why (the plugin doesn't speak it yet, its password can't be had).
    Cannot(String),
    /// A file it needs (its password's, its CA's) being read.
    Reading,
    Connecting,
    SigningIn,
    Ready { version: String },
    /// Down: why — and whether it's tried again.
    Down { why: String, again: bool },
}

/// What an attempt draws on: the variables read at start, and fresh random bytes (drawn as a session starts; none
/// before tarae handed any entropy over).
pub struct Ctx<'a> {
    pub vars: &'a BTreeMap<String, String>,
    pub seed: &'a mut dyn FnMut() -> Option<[u8; 32]>,
}

pub struct Live {
    /// The connection, as it was when opened (found again changed, it's opened again).
    pub conn: Connection,
    pub status: Status,
    /// The open attempt's id (`CONNECT` + n), 0 when none is open.
    id: u64,
    attempts: u64,
    /// Failures in a row, for the pause.
    failures: usize,
    session: Option<Box<dyn Session>>,
    /// What the session starts with, held until `connected`.
    opening: Vec<Event>,
    /// For this attempt, once had: its password (none: it has none), its TLS (none: off).
    password: Option<Option<Secret>>,
    tls: Option<Option<Arc<Config>>>,
}

impl Live {
    pub fn new(conn: Connection) -> Live {
        Live {
            conn,
            status: Status::Connecting,
            id: 0,
            attempts: 0,
            failures: 0,
            session: None,
            opening: Vec::new(),
            password: None,
            tls: None,
        }
    }

    /// Opens it — or says why it can't. Its password and its CA are read afresh each time (a file may have
    /// changed since).
    pub fn open(&mut self, cx: &mut Ctx) -> Vec<Action> {
        (self.password, self.tls) = (None, None);
        self.step(cx)
    }

    /// The next thing an attempt needs: its password, its TLS, then the connection.
    fn step(&mut self, cx: &mut Ctx) -> Vec<Action> {
        if self.password.is_none() {
            match &self.conn.password {
                Password::File(path) => {
                    self.status = Status::Reading;
                    return vec![Action::ReadFile((PASSWORD_FILE, path.clone()))];
                }
                p => match password(p, cx.vars) {
                    Ok(pw) => self.password = Some(pw),
                    Err(why) => return self.cannot(why),
                },
            }
        }
        if self.tls.is_none() {
            let made = match (self.conn.tls(), &self.conn.ca_file) {
                (TlsMode::Off, _) => Ok(None),
                (TlsMode::Unverified, _) => tls::config_unverified().map(Some),
                (TlsMode::Verify, Some(path)) => {
                    self.status = Status::Reading;
                    return vec![Action::ReadFile((CA_FILE, path.clone()))];
                }
                (TlsMode::Verify, None) => tls::config_public(None).map(Some),
            };
            match made {
                Ok(t) => self.tls = Some(t),
                Err(why) => return self.cannot(format!("TLS: {why}")),
            }
        }
        let password = self.password.clone().flatten();
        self.connect(password, cx)
    }

    /// Stops what's open and what's waiting.
    pub fn close(&mut self) -> Vec<Action> {
        let mut out = vec![Action::Stop(RETRY)];
        if self.id != 0 {
            out.push(Action::Stop(self.id));
        }
        self.id = 0;
        self.session = None;
        out
    }

    /// An event that's this connection's (else `None`): what to do.
    pub fn event(&mut self, ev: &HostEvent, cx: &mut Ctx) -> Option<Vec<Action>> {
        let open = |id: &u64| *id == self.id && self.id != 0;
        Some(match ev {
            HostEvent::File((PASSWORD_FILE, bytes)) if self.status == Status::Reading => {
                let c = &self.conn;
                // (no database named: libpq's default, the user's name — the database the server will use too)
                let user = c.user.as_deref().unwrap_or("");
                let db = c.database.as_deref().unwrap_or(user);
                match crate::pgpass::lookup(&String::from_utf8_lossy(bytes), &c.host, c.port, db, user) {
                    Some(pw) => {
                        self.password = Some(Some(pw));
                        self.step(cx)
                    }
                    None => self.cannot(format!("no line of {} is for it", password_file(c))),
                }
            }
            HostEvent::Failed((PASSWORD_FILE, why)) if self.status == Status::Reading => {
                self.cannot(format!("{}: {why}", password_file(&self.conn)))
            }
            // (one holding no certificate would trust nothing — every server refused later, the reason a riddle)
            HostEvent::File((CA_FILE, pem)) if self.status == Status::Reading && !pem.windows(11).any(|w| w == b"CERTIFICATE") => {
                self.cannot("its ca-file: no certificate in it".into())
            }
            HostEvent::File((CA_FILE, pem)) if self.status == Status::Reading => match tls::config(pem, None) {
                Ok(config) => {
                    self.tls = Some(Some(config));
                    self.step(cx)
                }
                Err(why) => self.cannot(format!("its ca-file: {why}")),
            },
            HostEvent::Failed((CA_FILE, why)) if self.status == Status::Reading => self.cannot(format!("its ca-file: {why}")),
            HostEvent::Tick(RETRY) if matches!(self.status, Status::Down { again: true, .. }) => self.open(cx),
            HostEvent::Connected(id) if open(id) => {
                self.status = Status::SigningIn;
                let opening = std::mem::take(&mut self.opening);
                self.events(opening)
            }
            HostEvent::Data((id, bytes)) if open(id) => match self.session.as_mut() {
                Some(s) => {
                    let events = s.feed(bytes);
                    self.events(events)
                }
                None => Vec::new(),
            },
            HostEvent::Closed((id, why)) if open(id) => {
                let why = if why.is_empty() { "the server closed the connection".to_string() } else { why.clone() };
                self.down(why, true)
            }
            HostEvent::Failed((id, why)) if open(id) => self.down(why.clone(), true),
            // (an earlier attempt's: over already)
            HostEvent::Data((id, _)) | HostEvent::Closed((id, _)) | HostEvent::Connected(id) | HostEvent::Failed((id, _))
                if *id >= CONNECT =>
            {
                Vec::new()
            }
            _ => return None,
        })
    }

    fn connect(&mut self, password: Option<Secret>, cx: &mut Ctx) -> Vec<Action> {
        let c = &self.conn;
        let Some(user) = c.user.clone().or_else(|| c.kind.driver().default_user().map(String::from)) else {
            return self.cannot("no user named — `user` in its config".into());
        };
        // (the name its certificate must carry: the host as written — an IP address is checked as one)
        let tls = self.tls.clone().flatten().map(|config| (config, c.host.trim_start_matches('[').trim_end_matches(']').to_string()));
        let params = Params { user, database: c.database.clone(), password, tls, loopback: is_loopback(&c.host) };
        // A nonce or padding from fixed bytes would be known to anyone: no randomness, no sign-in — tried again
        let Some(seed) = (cx.seed)() else {
            return self.down("no randomness yet to sign in with".into(), true);
        };
        let Some((session, opening)) = c.kind.driver().session(params, seed) else {
            return self.cannot(format!("the plugin doesn't speak {} yet", c.kind.driver().short()));
        };
        (self.session, self.opening) = (Some(session), opening);
        self.attempts += 1;
        self.id = CONNECT + self.attempts;
        self.status = Status::Connecting;
        vec![Action::Connect(Connect { id: self.id, host: c.host.clone(), port: c.port as u64 })]
    }

    fn events(&mut self, events: Vec<Event>) -> Vec<Action> {
        let mut out = Vec::new();
        for e in events {
            match e {
                Event::Send(b) => out.push(Action::Send((self.id, b))),
                Event::Ready { version } => {
                    self.failures = 0;
                    self.status = Status::Ready { version };
                }
                // The server turned it away, or spoke what the session won't go on with: not tried again
                Event::Failed(why) => {
                    out.extend(self.down(why, false));
                    break;
                }
                Event::Described { .. } | Event::Rows { .. } | Event::Keys { .. } | Event::Refused(_) => {}
            }
        }
        out
    }

    fn down(&mut self, why: String, again: bool) -> Vec<Action> {
        let mut out = Vec::new();
        if self.id != 0 {
            out.push(Action::Stop(self.id));
        }
        self.id = 0;
        self.session = None;
        self.status = Status::Down { why, again };
        if again {
            out.push(Action::Timer((RETRY, PAUSE_MS[self.failures.min(PAUSE_MS.len() - 1)])));
            self.failures += 1;
        }
        out
    }

    fn cannot(&mut self, why: String) -> Vec<Action> {
        self.status = Status::Cannot(why);
        Vec::new()
    }
}

fn password_file(c: &Connection) -> &str {
    match &c.password {
        Password::File(p) => p,
        _ => "",
    }
}

/// Its password, from where the connection says (`None`: it has none) — a file's is read first, by `open`.
fn password(p: &Password, vars: &BTreeMap<String, String>) -> Result<Option<Secret>, String> {
    match p {
        Password::None => Ok(None),
        Password::Given(s) => Ok(Some(s.clone())),
        Password::Env(name) => vars.get(name).map(|v| Some(Secret::new(v.as_str()))).ok_or(format!("${name} isn't set")),
        Password::Ask => Err("its password is asked — tarae's masked prompt comes with plugin API 0.3".into()),
        Password::Unknown(_) | Password::File(_) => Err("its password is unknown".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::Origin;
    use crate::driver::Kind;

    /// No randomness, no sign-in: a SCRAM nonce or OAEP padding made of zeros would be known to anyone in the way.
    /// The attempt is said and tried again after a pause — nothing is opened meanwhile.
    #[test]
    fn no_randomness_no_sign_in() {
        let mut c = Connection::new("dev", Kind::Postgres, "127.0.0.1", Origin::Env("X".into()));
        c.user = Some("app".into());
        let mut l = Live::new(c);
        let out = l.open(&mut Ctx { vars: &BTreeMap::new(), seed: &mut || None });
        assert!(!out.iter().any(|a| matches!(a, Action::Connect(_))), "{out:?}");
        assert!(out.contains(&Action::Timer((RETRY, PAUSE_MS[0]))), "{out:?}");
        assert!(matches!(&l.status, Status::Down { why, again: true } if why.contains("randomness")), "{:?}", l.status);
    }
}
