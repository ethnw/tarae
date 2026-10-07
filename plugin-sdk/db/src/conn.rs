//! A connection as found — where to, as whom, where its password comes from (never the password shown), and
//! whether it's production: marked so, or a host that isn't this machine and isn't marked `dev`.

use crate::driver::Kind;
use crate::secret::Secret;

/// Where a connection was found.
#[derive(Clone, Debug, PartialEq)]
pub enum Origin {
    /// A config file (`.tarae/db.toml`, `.tarae/db.local.toml` — its path), at a line (from 0).
    Config { path: String, line: u64 },
    /// A compose file (its path), the service, at its line (from 0) if it was found.
    Compose { path: String, service: String, line: Option<u64> },
    /// An environment variable (its name).
    Env(String),
}

impl Origin {
    pub fn said(&self) -> String {
        match self {
            Origin::Config { path, .. } => format!(".tarae/{}", file_name(path)),
            Origin::Compose { path, service, .. } => format!("{} · {service}", file_name(path)),
            Origin::Env(name) => format!("${name}"),
        }
    }

    /// Where it's written: (path, line), for `enter` in the side tab.
    pub fn place(&self) -> Option<(String, u64)> {
        match self {
            Origin::Config { path, line } => Some((path.clone(), *line)),
            Origin::Compose { path, line: Some(line), .. } => Some((path.clone(), *line)),
            _ => None,
        }
    }
}

pub fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

/// Where the password comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Password {
    /// None asked for (a compose service that trusts every connection; Redis without `requirepass`).
    None,
    /// Known already: a compose file's dev value, an environment variable's.
    Given(Secret),
    /// Read from this environment variable when connecting.
    Env(String),
    /// Looked up in this password file (`~/.pgpass`) when connecting.
    File(String),
    /// Asked for when connecting.
    Ask,
    /// Can't be known here — why.
    Unknown(String),
}

impl Password {
    /// What the side tab says of it, in little room (an unknown one's why goes on a line of its own).
    pub fn said(&self) -> String {
        match self {
            Password::None => "no password".into(),
            Password::Given(_) => "password known".into(),
            Password::Env(name) => format!("password: ${name}"),
            Password::File(path) => format!("password: {path}"),
            Password::Ask => "password asked".into(),
            Password::Unknown(_) => "password unknown".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TlsMode {
    Off,
    /// Encrypted and the server's certificate checked — against public roots or a CA file the config names.
    Verify,
    /// Encrypted, the certificate not checked: only when the config says so in so many words.
    Unverified,
}

impl TlsMode {
    /// As a config's `tls` or a URL's `sslmode` says it (libpq's names too). libpq's `require` encrypts without
    /// checking the certificate; ours checks it — a stricter reading on purpose (the user's decision, 2026-10-07):
    /// unverified encryption is had only by naming it, `require-unverified`.
    pub fn from_name(name: &str) -> Option<TlsMode> {
        Some(match name {
            "off" | "disable" | "false" => TlsMode::Off,
            "require" | "required" | "true" => TlsMode::Verify,
            "verify" | "verify-ca" | "verify-full" | "verify_identity" | "verify_ca" => TlsMode::Verify,
            "require-unverified" => TlsMode::Unverified,
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    Production,
    Dev,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Connection {
    pub name: String,
    pub kind: Kind,
    pub host: String,
    pub port: u16,
    pub database: Option<String>,
    pub user: Option<String>,
    pub password: Password,
    /// As asked; `None` = the default (`tls()`).
    pub tls: Option<TlsMode>,
    /// The CA its server's certificate is checked against (a PEM file); none: the public CAs.
    pub ca_file: Option<String>,
    pub mark: Option<Mark>,
    pub origin: Origin,
    /// The files it's for (absolute globs: `*` a name, `**` any folders) — `.tarae/db.toml`'s `paths`.
    pub paths: Vec<String>,
    /// What's wrong with it, said under it.
    pub problems: Vec<String>,
}

impl Connection {
    pub fn new(name: impl Into<String>, kind: Kind, host: impl Into<String>, origin: Origin) -> Connection {
        Connection {
            name: name.into(),
            kind,
            host: host.into(),
            port: kind.default_port(),
            database: None,
            user: None,
            password: Password::None,
            tls: None,
            ca_file: None,
            mark: None,
            origin,
            paths: Vec::new(),
            problems: Vec::new(),
        }
    }

    /// Production unless marked `dev` — or not marked and on this machine. A host elsewhere that nobody marked
    /// is production (the user's decision, 2026-10-07): the safe guess is the one that guards.
    pub fn production(&self) -> bool {
        match self.mark {
            Some(Mark::Production) => true,
            Some(Mark::Dev) => false,
            None => !is_loopback(&self.host),
        }
    }

    /// TLS as it'll be: what was asked, else off on this machine and verified elsewhere.
    pub fn tls(&self) -> TlsMode {
        self.tls.unwrap_or(if is_loopback(&self.host) { TlsMode::Off } else { TlsMode::Verify })
    }

    /// `postgres://app@localhost:5433/shop` — never a password, whatever it has.
    pub fn shown(&self) -> String {
        format!("{}://{}", self.kind.scheme(), self.target())
    }

    /// `app@localhost:5433/shop`: as whom, where.
    pub fn target(&self) -> String {
        let user = self.user.as_deref().map(|u| format!("{u}@")).unwrap_or_default();
        let host = if self.host.contains(':') { format!("[{}]", self.host) } else { self.host.clone() };
        let db = self.database.as_deref().map(|d| format!("/{d}")).unwrap_or_default();
        format!("{user}{host}:{}{db}", self.port)
    }

    /// Whether it's for the file at `path` (its `paths`).
    pub fn covers(&self, path: &str) -> bool {
        self.paths.iter().any(|p| crate::glob::matches(p, path))
    }
}

/// This machine: `localhost` (and names under it), 127.0.0.0/8, ::1.
pub fn is_loopback(host: &str) -> bool {
    let h = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    h == "localhost"
        || h.ends_with(".localhost")
        || h == "::1"
        || h.parse::<std::net::Ipv4Addr>().is_ok_and(|ip| ip.is_loopback())
        || h.parse::<std::net::Ipv6Addr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The user's rule (2026-10-07): a host that isn't this machine is production until someone marks it `dev`
    /// — a guess the other way would let a write or Claude through to a database nobody vouched for.
    #[test]
    fn an_unmarked_remote_host_is_production() {
        let c = |host: &str, mark| Connection { mark, ..Connection::new("x", Kind::Postgres, host, Origin::Env("X".into())) };
        for local in ["localhost", "LOCALHOST", "db.localhost", "127.0.0.1", "127.3.4.5", "::1", "[::1]"] {
            assert!(!c(local, None).production(), "{local} is this machine");
        }
        for remote in ["10.0.0.5", "192.168.1.2", "db.internal", "shop.abc123.eu-west-1.rds.amazonaws.com", "::2"] {
            assert!(c(remote, None).production(), "{remote} isn't marked: production");
            assert!(!c(remote, Some(Mark::Dev)).production(), "{remote} marked dev");
        }
        assert!(c("localhost", Some(Mark::Production)).production(), "a mark wins on this machine too");
    }

    /// Off this machine TLS is verified unless asked otherwise — a password over a network in the clear, or to a
    /// server nobody checked, is the default nobody should get by accident (the user's decision, 2026-10-07).
    #[test]
    fn tls_is_verified_off_this_machine_unless_asked_otherwise() {
        let mut c = Connection::new("x", Kind::Postgres, "db.internal", Origin::Env("X".into()));
        assert_eq!(c.tls(), TlsMode::Verify);
        c.tls = Some(TlsMode::Off);
        assert_eq!(c.tls(), TlsMode::Off);
        assert_eq!(Connection::new("x", Kind::Redis, "127.0.0.1", Origin::Env("X".into())).tls(), TlsMode::Off);
    }

    /// Every name says what it does: libpq's `require` (which doesn't check the certificate) is read as verify —
    /// stricter on purpose; encryption without the check only by naming it.
    #[test]
    fn each_tls_name_says_what_it_does() {
        for off in ["off", "disable", "false"] {
            assert_eq!(TlsMode::from_name(off), Some(TlsMode::Off), "{off}");
        }
        for verify in ["require", "required", "true", "verify", "verify-ca", "verify-full", "verify_identity", "verify_ca"] {
            assert_eq!(TlsMode::from_name(verify), Some(TlsMode::Verify), "{verify}");
        }
        assert_eq!(TlsMode::from_name("require-unverified"), Some(TlsMode::Unverified));
        for unknown in ["prefer", "allow", "unverified", ""] {
            assert_eq!(TlsMode::from_name(unknown), None, "{unknown}");
        }
    }

    #[test]
    fn shown_names_where_and_as_whom_never_the_password() {
        let mut c = Connection::new("x", Kind::Postgres, "::1", Origin::Env("X".into()));
        (c.port, c.user, c.database) = (5433, Some("app".into()), Some("shop".into()));
        c.password = Password::Given(Secret::new("tarae-fake-pw"));
        assert_eq!(c.shown(), "postgres://app@[::1]:5433/shop");
        assert!(!format!("{c:?}").contains("tarae-fake-pw"));
    }
}
