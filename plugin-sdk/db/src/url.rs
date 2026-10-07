//! A connection URL — `postgres://app:pw@host:5433/shop?sslmode=require`, `mysql://…`, `redis://…/0`,
//! `rediss://…` — read by hand (the `url` crate would bring IDNA tables for nothing here). What goes wrong is said
//! without a word of the URL: it may hold a password.

use crate::conn::TlsMode;
use crate::driver::Kind;
use crate::secret::Secret;

#[derive(Debug, PartialEq)]
pub struct Url {
    pub kind: Kind,
    pub host: String,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub password: Option<Secret>,
    /// The path: a database's name (Redis: its number).
    pub database: Option<String>,
    /// `rediss`, or `sslmode`/`ssl-mode` in the query.
    pub tls: Option<TlsMode>,
}

pub fn parse(s: &str) -> Result<Url, String> {
    let (scheme, rest) = s.trim().split_once("://").ok_or("not a URL")?;
    let (kind, tls_scheme) =
        Kind::from_scheme(scheme).ok_or("not a database URL — postgres://, mysql:// or redis:// is")?;
    // The user and password run to the last `@`: a password may hold `/`, `?`, `#`, `@` or `:` unencoded, and one
    // misread as the host would go unflagged where a config must refuse it. (An `@` in the path or query is then
    // taken for the user's end — a URL that writes one should encode it.) The host runs to the first `/`, `?` or `#`
    // after that.
    let (userinfo, after) = match rest.rfind('@') {
        Some(at) => (Some(&rest[..at]), &rest[at + 1..]),
        None => (None, rest),
    };
    let host_end = after.find(['/', '?', '#']).unwrap_or(after.len());
    let (hostport, tail) = after.split_at(host_end);
    let tail = tail.split('#').next().unwrap_or("");
    let (path, query) = tail.split_once('?').unwrap_or((tail, ""));
    let path = path.strip_prefix('/').unwrap_or(path);
    let (user, password) = match userinfo {
        Some(u) => {
            let (name, pw) = u.split_once(':').map_or((u, None), |(n, p)| (n, Some(p)));
            let name = decode(name).ok_or("its user name isn't valid percent-encoding")?;
            let pw = pw.map(|p| decode(p).ok_or("its password isn't valid percent-encoding")).transpose()?;
            ((!name.is_empty()).then_some(name), pw.map(Secret::new))
        }
        None => (None, None),
    };
    let (host, port) = split_host_port(hostport)?;
    if host.is_empty() {
        return Err("no host in it".into());
    }
    if host.contains(',') {
        return Err("it names several hosts — one is read".into());
    }
    let database = decode(path).ok_or("its database name isn't valid percent-encoding")?;
    let mut tls = tls_scheme.then_some(TlsMode::Verify);
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        if matches!(k, "sslmode" | "ssl-mode" | "ssl_mode" | "tls") {
            tls = Some(TlsMode::from_name(&v.to_ascii_lowercase()).ok_or("its sslmode isn't one tarae knows")?);
        }
    }
    Ok(Url { kind, host, port, user, password, database: (!database.is_empty()).then_some(database), tls })
}

fn split_host_port(s: &str) -> Result<(String, Option<u16>), String> {
    let port = |p: &str| p.parse::<u16>().map_err(|_| "its port isn't a number".to_string());
    if let Some(rest) = s.strip_prefix('[') {
        let (host, after) = rest.split_once(']').ok_or("an IPv6 address without its `]`")?;
        let p = match after.strip_prefix(':') {
            Some(p) => Some(port(p)?),
            None if after.is_empty() => None,
            None => return Err("something after its IPv6 address".into()),
        };
        return Ok((host.to_string(), p));
    }
    match s.rsplit_once(':') {
        Some((h, p)) => Ok((h.to_string(), Some(port(p)?))),
        None => Ok((s.to_string(), None)),
    }
}

/// `%40` → `@`; `None` if it isn't valid (a `%` without two hex digits, or not UTF-8).
fn decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

#[cfg(test)]
pub fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_parts_of_a_url_percent_decoded() {
        let u = parse("postgresql://app:p%40ss%3Aw0rd@db.internal:5433/shop%20eu?sslmode=verify-full&x=1").unwrap();
        assert_eq!(u.kind, Kind::Postgres);
        assert_eq!((u.host.as_str(), u.port), ("db.internal", Some(5433)));
        assert_eq!(u.user.as_deref(), Some("app"));
        assert_eq!(u.password.as_ref().map(Secret::expose), Some("p@ss:w0rd"));
        assert_eq!(u.database.as_deref(), Some("shop eu"));
        assert_eq!(u.tls, Some(TlsMode::Verify));
        let u = parse("rediss://:pw@[::1]/2").unwrap();
        assert_eq!((u.kind, u.host.as_str(), u.port, u.tls), (Kind::Redis, "::1", None, Some(TlsMode::Verify)));
        assert_eq!((u.user, u.database.as_deref()), (None, Some("2")));
        let u = parse("mysql://root@localhost").unwrap();
        assert_eq!((u.kind, u.password, u.database), (Kind::Mysql, None, None));
        // An unencoded `@` in the password: the last one ends it
        assert_eq!(parse("redis://u:a@b@h:1").unwrap().password.unwrap().expose(), "a@b");
    }

    /// The password runs to the last `@`, whatever it holds — `/`, `?`, `#`, `@`, `:` unencoded — so the host is
    /// the host and a password is seen as one (a config must refuse it; read as the host it would slip through).
    #[test]
    fn a_password_may_hold_what_ends_a_host() {
        for pw in ["p/w", "p?w", "p#w", "p@w", "p:w", "1234?x", "a/b?c#d@e:f"] {
            let u = parse(&format!("postgres://app:{pw}@db.internal:5433/shop?sslmode=verify-full")).unwrap();
            assert_eq!(u.password.as_ref().map(Secret::expose), Some(pw), "{pw}");
            assert_eq!((u.user.as_deref(), u.host.as_str(), u.port), (Some("app"), "db.internal", Some(5433)), "{pw}");
            assert_eq!((u.database.as_deref(), u.tls), (Some("shop"), Some(TlsMode::Verify)), "{pw}");
        }
        let c = crate::config::read("/p/.tarae/db.toml", "/p", "[[connection]]\nname = \"a\"\nurl = \"postgres://app:1234?x@prod/db\"\n");
        assert!(c.unwrap().connections[0].problems[0].contains("holds a password"));
    }

    /// A URL that doesn't read may still hold a password: why it doesn't is said in tarae's words only.
    #[test]
    fn what_goes_wrong_never_quotes_the_url() {
        for bad in [
            "postgres://app:tarae-fake-pw@:5432/db",
            "postgres://app:tarae-fake-pw@h:port/db",
            "postgres://app:tarae-fake-pw@h1,h2/db",
            "postgres://app:tarae-fake-pw%zz@h/db",
            "http://app:tarae-fake-pw@h/db",
            "app:tarae-fake-pw@h/db",
            "postgres://app:tarae-fake-pw@h/db?sslmode=tarae-fake-pw",
            "postgres://app:tarae-fake-pw@[::1/db",
        ] {
            let why = parse(bad).expect_err(bad);
            assert!(!why.contains("tarae-fake"), "{why}");
        }
    }
}
