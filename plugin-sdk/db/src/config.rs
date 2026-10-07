//! `.tarae/db.toml`: the project's connections, by name — and where each one's password comes from, never the
//! password itself (the file gets committed). `.tarae/db.local.toml`, the same, is yours: git-ignored, it wins by
//! name — and still holds no password (a file in the project is readable by every tool that works there, Claude's
//! too). What's wrong in either is said by line, never quoting it.
//!
//! ```toml
//! [[connection]]
//! name = "dev"
//! url = "postgres://app@localhost:5433/shop"   # or kind, host, port, database, user
//! password-env = "PGPASSWORD"                   # or password-file = "~/.pgpass", or ask = true
//! paths = ["migrations/**", "src/db/**"]        # the files it's for (from the project's folder)
//! production = true                             # or dev = true (unmarked: production unless on this machine)
//! tls = "verify"                                # off · verify (the default off this machine) · require-unverified
//! ca-file = "certs/db-ca.pem"                   # verify against this CA (from the project's folder, or ~/…, /…)
//! ```                                           #   — none: the public CAs

use serde::Deserialize;
use toml::Spanned;

use crate::conn::{Connection, Mark, Origin, Password, TlsMode};
use crate::driver::{Kind, env_names};

/// The password files it may read.
pub const PASSWORD_FILES: [&str; 1] = ["~/.pgpass"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    connection: Vec<Spanned<Entry>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "kebab-case")]
struct Entry {
    name: String,
    kind: Option<String>,
    url: Option<String>,
    url_env: Option<String>,
    host: Option<String>,
    port: Option<u16>,
    database: Option<String>,
    user: Option<String>,
    /// Refused: taken in only to say so (any type, so its value is never in an error).
    password: Option<toml::Value>,
    password_env: Option<String>,
    password_file: Option<String>,
    #[serde(default)]
    ask: bool,
    tls: Option<String>,
    ca_file: Option<String>,
    #[serde(default)]
    production: bool,
    #[serde(default)]
    dev: bool,
    #[serde(default)]
    paths: Vec<String>,
}

/// What the file says: its connections (one that can't be used is still listed, with why), or why none can be
/// read.
pub struct Read {
    pub connections: Vec<Connection>,
    /// Connections named by a variable (`url-env`): (the connection's index, the variable) — filled in once
    /// it's read.
    pub url_env: Vec<(usize, String)>,
    /// Every variable a connection names with `url-env`, used or not: it's that connection's, not one more of the
    /// environment's.
    pub named: Vec<String>,
}

/// The file at `path` (in the project at `root`), its `text`.
pub fn read(path: &str, root: &str, text: &str) -> Result<Read, String> {
    let name = format!(".tarae/{}", crate::conn::file_name(path));
    // Why a password can't be in it: the project's is committed; yours is readable where you work
    let no_password = if path.ends_with("db.local.toml") {
        "a file in the project is readable by every tool working there"
    } else {
        "this file gets committed"
    };
    let file: File = toml::from_str(text).map_err(|e| {
        let line = e.span().map(|s| line_of(text, s.start) + 1);
        let at = line.map(|l| format!("line {l}: ")).unwrap_or_default();
        // Only what names a key: the rest of toml's messages can quote a value
        let msg = e.message();
        if msg.starts_with("unknown field") || msg.starts_with("missing field") || msg.starts_with("duplicate key") {
            format!("{at}{}", msg.split(", expected").next().unwrap_or(msg))
        } else {
            format!("{at}not what {name} takes there")
        }
    })?;
    let mut read = Read { connections: Vec::new(), url_env: Vec::new(), named: Vec::new() };
    // (a config can name only the variables the plugin may read: its `permissions()`)
    let names = env_names();
    let may_read = |var: &str| names.contains(&var);
    let not_readable = |var: &str| format!("${var} isn't one the plugin may read: {}", names.join(", "));
    for spanned in file.connection {
        let line = line_of(text, spanned.span().start) as u64;
        let e = spanned.into_inner();
        let origin = Origin::Config { path: path.to_string(), line };
        let mut problems = Vec::new();
        // What else says where to — beside `url-env`, a conflict: said, not settled by picking one
        let also_where: Vec<&str> = [("url", e.url.is_some()), ("host", e.host.is_some()), ("port", e.port.is_some()),
            ("database", e.database.is_some()), ("user", e.user.is_some())]
            .iter()
            .filter(|(_, set)| *set)
            .map(|(k, _)| *k)
            .collect();
        // (its kind and port are set below, once known)
        let mut c = Connection::new(&e.name, crate::driver::ALL[0].kind(), "localhost", origin);
        let mut kind = e.kind.as_deref().map(|k| Kind::from_name(k).ok_or(()));
        if let Some(url) = &e.url {
            match crate::url::parse(url) {
                Ok(u) if u.password.is_some() => {
                    problems.push(format!("its url holds a password — {no_password}: password-env, password-file or ask"));
                    kind.get_or_insert(Ok(u.kind));
                }
                Ok(u) => {
                    kind.get_or_insert(Ok(u.kind));
                    (c.host, c.user, c.database, c.tls) = (u.host, u.user, u.database, u.tls);
                    if let Some(p) = u.port {
                        c.port = p;
                    }
                }
                Err(why) => problems.push(format!("its url: {why}")),
            }
        }
        match kind {
            Some(Ok(k)) => {
                c.kind = k;
                if e.port.is_none() && e.url.is_none() {
                    c.port = k.default_port();
                }
            }
            Some(Err(())) => problems.push("its kind isn't postgres, mysql or redis".into()),
            None if e.url_env.is_some() => {}
            None => problems.push("no kind — postgres, mysql or redis (or a url)".into()),
        }
        if let Some(h) = e.host {
            c.host = h;
        }
        if let Some(p) = e.port {
            c.port = p;
        }
        c.database = e.database.or(c.database);
        c.user = e.user.or(c.user);
        if e.password.is_some() {
            problems.push(format!("a password in {name}: {no_password} — password-env, password-file or ask"));
        }
        c.password = match (e.password_env, e.password_file, e.ask) {
            (Some(var), None, false) if may_read(&var) => Password::Env(var),
            (Some(var), None, false) => {
                problems.push(not_readable(&var));
                Password::Unknown(format!("${var} can't be read"))
            }
            (None, Some(f), false) if PASSWORD_FILES.contains(&f.as_str()) => Password::File(f),
            (None, Some(f), false) => {
                problems.push(format!("{f} isn't a password file the plugin may read: {}", PASSWORD_FILES.join(", ")));
                Password::Unknown(format!("{f} can't be read"))
            }
            (None, None, true) => Password::Ask,
            (None, None, false) => Password::None,
            _ => {
                problems.push("password-env, password-file and ask: one of them".into());
                Password::Unknown("more than one way given".into())
            }
        };
        if let Some(t) = e.tls {
            match TlsMode::from_name(&t) {
                Some(m) => c.tls = Some(m),
                None => problems.push("its tls isn't off, verify or require-unverified".into()),
            }
        }
        if let Some(ca) = e.ca_file {
            // A CA checks the server's certificate: with TLS off, or asked not to check, it would do nothing — said,
            // not left to look as if it did
            if matches!(c.tls, Some(TlsMode::Off | TlsMode::Unverified)) {
                problems.push("ca-file goes with tls = \"verify\" — it checks nothing otherwise".into());
            }
            let absolute = ca.starts_with('/') || ca.starts_with("~/");
            c.ca_file = Some(if absolute { ca } else { format!("{}/{}", root.trim_end_matches('/'), ca.trim_start_matches("./")) });
        }
        c.mark = match (e.production, e.dev) {
            (true, true) => {
                problems.push("production and dev both — taken as production".into());
                Some(Mark::Production)
            }
            (true, false) => Some(Mark::Production),
            (false, true) => Some(Mark::Dev),
            (false, false) => None,
        };
        c.paths = e.paths.iter().map(|p| format!("{}/{}", root.trim_end_matches('/'), p.trim_start_matches("./"))).collect();
        if let Some(var) = e.url_env {
            read.named.push(var.clone());
            if !may_read(&var) {
                problems.push(not_readable(&var));
            } else if !also_where.is_empty() {
                problems.push(format!("url-env ${var} and {} both say where to — one of them", also_where.join(", ")));
            } else {
                read.url_env.push((read.connections.len(), var));
            }
        }
        c.problems = problems;
        read.connections.push(c);
    }
    Ok(read)
}

/// The line (from 0) a byte offset is on.
pub fn line_of(text: &str, at: usize) -> usize {
    text.as_bytes()[..at.min(text.len())].iter().filter(|&&b| b == b'\n').count()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(text: &str) -> Connection {
        read("/p/.tarae/db.toml", "/p", text).unwrap().connections.remove(0)
    }

    /// The file is the project's and gets committed: a password in it — as a key or inside a url — is refused,
    /// said on the connection, and never taken in.
    #[test]
    fn a_password_in_the_config_is_refused() {
        let c = one("[[connection]]\nname = \"dev\"\nkind = \"postgres\"\npassword = \"tarae-fake-pw\"\n");
        assert!(c.problems.iter().any(|p| p.contains("gets committed")), "{:?}", c.problems);
        assert_eq!(c.password, Password::None);
        let c = one("[[connection]]\nname = \"dev\"\nurl = \"postgres://app:tarae-fake-pw@localhost/shop\"\n");
        assert!(c.problems.iter().any(|p| p.contains("holds a password")), "{:?}", c.problems);
        assert_eq!((c.user.as_deref(), c.database.as_deref()), (None, None), "nothing of that url is taken in");
        assert!(!format!("{c:?}").contains("tarae-fake"));
    }

    /// Your own file isn't committed — but it's in the project, where every tool working there can read it,
    /// Claude's too: no password in it either, and the reason said is its own.
    #[test]
    fn a_password_in_a_personal_config_is_refused_too() {
        let text = "[[connection]]\nname = \"mine\"\nkind = \"redis\"\npassword = \"tarae-fake-pw\"\n";
        let c = read("/p/.tarae/db.local.toml", "/p", text).unwrap().connections.remove(0);
        assert_eq!(c.problems, ["a password in .tarae/db.local.toml: a file in the project is readable by every tool working there — password-env, password-file or ask"]);
        assert_eq!((c.password, c.origin.said()), (Password::None, ".tarae/db.local.toml".to_string()));
    }

    #[test]
    fn a_connection_from_its_parts_or_its_url() {
        let text = "[[connection]]\nname = \"dev\"\nurl = \"postgres://app@localhost:5433/shop\"\npassword-env = \"PGPASSWORD\"\npaths = [\"migrations/**\"]\n\n[[connection]]\nname = \"cache\"\nkind = \"redis\"\nhost = \"cache.internal\"\nask = true\ndev = true\n";
        let r = read("/p/.tarae/db.toml", "/p", text).unwrap();
        let (dev, cache) = (&r.connections[0], &r.connections[1]);
        assert_eq!(dev.shown(), "postgres://app@localhost:5433/shop");
        assert_eq!(dev.password, Password::Env("PGPASSWORD".into()));
        assert!(dev.covers("/p/migrations/0001.sql") && !dev.covers("/p/src/main.rs"));
        assert_eq!(dev.origin.place(), Some(("/p/.tarae/db.toml".into(), 0)));
        assert_eq!(cache.shown(), "redis://cache.internal:6379");
        assert_eq!((cache.password.clone(), cache.production()), (Password::Ask, false));
        assert_eq!(cache.origin.place(), Some(("/p/.tarae/db.toml".into(), 6)));
        assert!(dev.problems.is_empty() && cache.problems.is_empty());
    }

    /// `url-env` and a `host`, `port`, `database`, `user` or `url` of its own both say where to: a conflict said on
    /// the connection, the variable not applied — never one silently over the other.
    #[test]
    fn url_env_beside_parts_of_its_own_is_a_conflict_said() {
        let r = read("/p/.tarae/db.toml", "/p", "[[connection]]\nname = \"a\"\nurl-env = \"DATABASE_URL\"\nport = 6432\ndatabase = \"x\"\n").unwrap();
        assert_eq!(r.connections[0].problems, ["url-env $DATABASE_URL and port, database both say where to — one of them"]);
        assert!(r.url_env.is_empty(), "not applied");
        assert_eq!(r.named, ["DATABASE_URL"], "still that connection's, not the environment's");
        let r = read("/p/.tarae/db.toml", "/p", "[[connection]]\nname = \"a\"\nurl-env = \"DATABASE_URL\"\ntls = \"verify\"\n").unwrap();
        assert!(r.connections[0].problems.is_empty() && r.url_env.len() == 1, "tls isn't where to");
    }

    /// A CA file checks the server's certificate: from the project's folder unless written from `/` or `~/` — and
    /// beside `tls = "off"` or `require-unverified`, where it would check nothing, that's said.
    #[test]
    fn a_ca_file_is_the_projects_and_goes_with_verify() {
        let pg = "[[connection]]\nname = \"a\"\nkind = \"postgres\"\n";
        let c = one(&format!("{pg}ca-file = \"./certs/ca.pem\"\n"));
        assert_eq!((c.ca_file.as_deref(), c.problems.len()), (Some("/p/certs/ca.pem"), 0));
        assert_eq!(one(&format!("{pg}ca-file = \"~/ca.pem\"\n")).ca_file.as_deref(), Some("~/ca.pem"));
        for tls in ["off", "require-unverified"] {
            let c = one(&format!("{pg}tls = \"{tls}\"\nca-file = \"ca.pem\"\n"));
            assert_eq!(c.problems, ["ca-file goes with tls = \"verify\" — it checks nothing otherwise"], "{tls}");
        }
    }

    /// The plugin reads only the variables and files you approved for it: a config naming another is told so,
    /// not quietly given nothing.
    #[test]
    fn only_variables_and_files_the_plugin_may_read() {
        let c = one("[[connection]]\nname = \"a\"\nkind = \"mysql\"\npassword-env = \"APP_DB_PASSWORD\"\n");
        assert!(c.problems[0].contains("$APP_DB_PASSWORD isn't one the plugin may read"), "{:?}", c.problems);
        let c = one("[[connection]]\nname = \"a\"\nkind = \"mysql\"\npassword-file = \"secrets/db.txt\"\n");
        assert!(c.problems[0].contains("isn't a password file"), "{:?}", c.problems);
    }

    /// toml's own messages quote the line they're about — which may be a password: only a key's name and the
    /// line number go out.
    #[test]
    fn a_parse_error_never_quotes_the_file() {
        for text in [
            "[[connection]]\nname = \"a\"\nport = \"tarae-fake-pw\"\n",
            "[[connection]]\nname = \"a\"\npasword = \"tarae-fake-pw\"\n",
            "[[connection]]\nname = \"a\"\npassword-env = tarae-fake-pw\n",
            "[[connection]]\nname = \"tarae-fake-pw\n",
        ] {
            let why = read("/p/.tarae/db.toml", "/p", text).err().expect(text);
            assert!(!why.contains("tarae-fake"), "{why}");
            assert!(why.starts_with("line "), "{why}");
        }
        let why = read("/p/.tarae/db.toml", "/p", "[[connection]]\nname = \"a\"\npasword = \"x\"\n").err().unwrap();
        assert_eq!(why, "line 3: unknown field `pasword`");
    }
}
