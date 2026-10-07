//! Compose files: the project's own databases — a service whose image a driver runs, published on this machine.
//! Compose's own rules are here (ports, environment, interpolation); who a service connects as is its driver's
//! (`Driver::compose_account`, from its `Service`). A value taken from the environment (`${DB_PASSWORD}`) comes from
//! a `.env` the plugin doesn't read — said so.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_yaml::Value;

use crate::conn::{Connection, Mark, Origin, Password};
use crate::driver::Kind;
use crate::secret::Secret;

#[derive(Deserialize)]
struct Compose {
    #[serde(default)]
    services: BTreeMap<String, Raw>,
}

#[derive(Deserialize)]
struct Raw {
    image: Option<String>,
    #[serde(default)]
    ports: Vec<Value>,
    #[serde(default)]
    environment: Value,
    #[serde(default)]
    command: Value,
}

/// A service as a driver reads it: its environment and its command's words, each value interpolated as compose does
/// — or why it can't be here.
pub struct Service {
    /// Its environment: `None` is `FOO:` or a bare `- FOO` — taken from the shell, which the plugin can't see.
    env: BTreeMap<String, Option<String>>,
    words: Vec<String>,
}

impl Service {
    #[cfg(test)]
    pub fn new(env: &[(&str, &str)], words: &[&str]) -> Service {
        Service {
            env: env.iter().map(|(k, v)| (k.to_string(), Some(v.to_string()))).collect(),
            words: words.iter().map(|w| w.to_string()).collect(),
        }
    }

    /// A variable's value: given, or why it isn't known here.
    pub fn value(&self, name: &str) -> Option<Result<String, String>> {
        self.env.get(name).map(|v| match v {
            Some(v) => interpolate(v).map_err(|u| u.said()),
            None => Err(format!("{name} comes from the shell — not read")),
        })
    }

    /// A variable's value, if known.
    pub fn known(&self, name: &str) -> Option<String> {
        self.value(name).and_then(Result::ok)
    }

    /// A switch an image reads (`MYSQL_ALLOW_EMPTY_PASSWORD`): on when set to something, off when unset or empty —
    /// or unknown (why), when its value isn't known here.
    pub fn flag(&self, name: &str) -> Option<Result<bool, String>> {
        self.value(name).map(|v| v.map(|v| !v.is_empty()))
    }

    /// The word after `flag` in its command (`--requirepass x`).
    pub fn word_after(&self, flag: &str) -> Option<Result<String, String>> {
        let i = self.words.iter().position(|w| w == flag)?;
        self.words.get(i + 1).map(|w| interpolate(w).map_err(|u| u.said()))
    }

    /// A variable as a password: known, unknown (why), or none given.
    pub fn password(&self, name: &str) -> Password {
        Service::as_password(self.value(name))
    }

    /// A value as a password: known, unknown (why), or none given.
    pub fn as_password(v: Option<Result<String, String>>) -> Password {
        match v {
            Some(Ok(p)) => Password::Given(Secret::new(p)),
            Some(Err(why)) => Password::Unknown(why),
            None => Password::None,
        }
    }
}

/// Why a value isn't known here. It may be a password: none of it is ever said — a variable's name only when the
/// whole value is that one reference.
#[derive(Debug, PartialEq)]
enum Unresolved {
    /// The value is `${NAME}` or `$NAME`, nothing else.
    Var(String),
    /// A variable inside other text — which may be a password with a `$` in it.
    Embedded,
    /// `${` with no `}`, or no name after `$`.
    Malformed,
}

impl Unresolved {
    fn said(&self) -> String {
        match self {
            Unresolved::Var(name) => format!("from ${{{name}}} — .env isn't read"),
            Unresolved::Embedded => "a variable in it — .env isn't read".into(),
            Unresolved::Malformed => "a malformed variable reference in it".into(),
        }
    }
}

/// The database services in a compose file (at `path`), as connections; why it can't be read otherwise — said
/// by line, never quoting it.
pub fn read(path: &str, text: &str) -> Result<Vec<Connection>, String> {
    let compose: Compose = serde_yaml::from_str(text).map_err(|e| match e.location() {
        Some(l) => format!("line {}: not a compose file the plugin can read", l.line()),
        None => "not a compose file the plugin can read".into(),
    })?;
    let mut out = Vec::new();
    for (name, raw) in compose.services {
        let Some(kind) = raw.image.as_deref().and_then(image_kind) else { continue };
        let origin = Origin::Compose { path: path.to_string(), service: name.clone(), line: service_line(text, &name) };
        let mut c = Connection::new(name.clone(), kind, "localhost", origin);
        // The project's own container, on this machine
        c.mark = Some(Mark::Dev);
        match published(&raw.ports, kind.default_port()) {
            Ok((host, port)) => {
                c.port = port;
                if let Some(h) = host {
                    c.host = h;
                }
            }
            Err(why) => c.problems.push(why),
        }
        let words = match &raw.command {
            Value::String(s) => s.split_whitespace().map(str::to_string).collect(),
            Value::Sequence(v) => v.iter().filter_map(scalar).collect(),
            _ => Vec::new(),
        };
        kind.driver().compose_account(&mut c, &Service { env: environment(&raw.environment), words });
        out.push(c);
    }
    Ok(out)
}

/// `docker.io/library/postgres:17-alpine`, `postgis/postgis@sha256:…` → the driver that runs it.
fn image_kind(image: &str) -> Option<Kind> {
    let last = image.rsplit('/').next()?;
    Kind::from_image(last.split(['@', ':']).next()?)
}

/// The host port (and address, if a particular one) the container's `target` is published on.
fn published(ports: &[Value], target: u16) -> Result<(Option<String>, u16), String> {
    let mut seen_unpublished = false;
    for p in ports {
        let (host_ip, host_port, container) = match p {
            Value::String(s) => {
                let s = interpolate(s).map_err(|u| format!("its port: {}", u.said()))?;
                let s = s.split('/').next().unwrap_or(&s);
                let parts: Vec<&str> = s.rsplitn(3, ':').collect();
                match parts.as_slice() {
                    [c] => (None, None, c.to_string()),
                    [c, h] => (None, Some(h.to_string()), c.to_string()),
                    [c, h, ip] => (Some(ip.trim_matches(['[', ']']).to_string()), Some(h.to_string()), c.to_string()),
                    _ => continue,
                }
            }
            Value::Number(n) => (None, None, n.to_string()),
            Value::Mapping(m) => {
                let get = |k: &str| m.get(k).and_then(scalar);
                let Some(c) = get("target") else { continue };
                (get("host_ip"), get("published"), c)
            }
            _ => continue,
        };
        if container.parse::<u16>() != Ok(target) {
            continue;
        }
        let Some(host_port) = host_port.filter(|h| !h.is_empty()) else {
            seen_unpublished = true;
            continue;
        };
        let host_port = interpolate(&host_port).map_err(|u| format!("its port: {}", u.said()))?;
        let port = host_port.parse::<u16>().map_err(|_| "its published port isn't one port (a range?)".to_string())?;
        let host = host_ip.filter(|ip| !matches!(ip.as_str(), "" | "0.0.0.0" | "::"));
        return Ok((host, port));
    }
    // (short: it's a note in a narrow tab)
    Err(if seen_unpublished {
        format!("port {target}: no fixed one published")
    } else {
        format!("port {target} isn't published")
    })
}

fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// `environment:` as a map or a list of `NAME=value` — `FOO:` and a bare `- FOO` take FOO from the shell (`None`),
/// they don't make it empty.
fn environment(v: &Value) -> BTreeMap<String, Option<String>> {
    match v {
        Value::Mapping(m) => m
            .iter()
            .filter_map(|(k, v)| Some((scalar(k)?, if v.is_null() { None } else { Some(scalar(v)?) })))
            .collect(),
        Value::Sequence(items) => items
            .iter()
            .filter_map(|i| {
                let s = scalar(i)?;
                Some(match s.split_once('=') {
                    Some((k, v)) => (k.to_string(), Some(v.to_string())),
                    None => (s, None),
                })
            })
            .collect(),
        _ => BTreeMap::new(),
    }
}

/// Compose's interpolation with defaults taken: `${X:-d}` and `${X-d}` → `d`; `$$` is `$`. A variable with no
/// default would come from the environment or `.env`, which the plugin doesn't read — `Unresolved`, which names it
/// only when it's the whole value.
fn interpolate(s: &str) -> Result<String, Unresolved> {
    let valid = |n: &str| n.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        if let Some(a) = after.strip_prefix('$') {
            out.push('$');
            rest = a;
            continue;
        }
        let (name, end) = if let Some(body) = after.strip_prefix('{') {
            let Some(close) = body.find('}') else { return Err(Unresolved::Malformed) };
            let inner = &body[..close];
            // Compose's forms, right after the name: `${V}`, `${V:-d}` `${V-d}` (a default: what we take),
            // `${V:?err}` `${V?err}`, `${V:+alt}` `${V+alt}` (all the shell's to decide)
            let name_len = inner.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(inner.len());
            let (name, op) = inner.split_at(name_len);
            let default = op.strip_prefix(":-").or_else(|| op.strip_prefix('-'));
            if let (Some(d), true) = (default, valid(name)) {
                out.push_str(d);
                rest = &body[close + 1..];
                continue;
            }
            if !(op.is_empty() || [":?", "?", ":+", "+"].iter().any(|o| op.starts_with(o))) {
                return Err(Unresolved::Malformed);
            }
            (name, &body[close + 1..])
        } else {
            let len = after.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).unwrap_or(after.len());
            (&after[..len], &after[len..])
        };
        if !valid(name) {
            return Err(Unresolved::Malformed);
        }
        // Named only when it's all there is: in other text it may be a password's `$`
        let whole = out.is_empty() && end.is_empty();
        return Err(if whole { Unresolved::Var(name.to_string()) } else { Unresolved::Embedded });
    }
    out.push_str(rest);
    Ok(out)
}

/// The line (from 0) a service is defined on: `  name:` under `services:`.
fn service_line(text: &str, name: &str) -> Option<u64> {
    let mut in_services = false;
    for (i, line) in text.lines().enumerate() {
        let body = line.split(" #").next().unwrap_or(line).trim_end();
        if !line.starts_with([' ', '\t']) {
            in_services = body == "services:";
            continue;
        }
        let key = body.trim_start().trim_end_matches(':');
        if in_services && body.ends_with(':') && key.trim_matches(['"', '\'']) == name {
            return Some(i as u64);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::TlsMode;

    const COMPOSE: &str = r#"
services:
  web:
    image: nginx:1.27
    ports: ["8080:80"]
  db:
    image: docker.io/library/postgres:17-alpine
    environment:
      POSTGRES_USER: app
      POSTGRES_PASSWORD: tarae-fake-pg
    ports:
      - "127.0.0.1:5433:5432"
  mysql:
    image: mysql:8.4
    environment:
      - MYSQL_ROOT_PASSWORD=tarae-fake-root
      - MYSQL_DATABASE=shop
    ports:
      - target: 3306
        published: "${MYSQL_PORT:-3307}"
  cache: # the sessions
    image: "redis:7-alpine"
    command: ["redis-server", "--requirepass", "tarae-fake-redis"]
    ports: [6379]
  maria:
    image: mariadb:11
    environment: { MARIADB_USER: shop, MARIADB_PASSWORD: "${SHOP_PW}", MARIADB_ROOT_PASSWORD: x }
    ports: ["3308:3306/tcp"]
"#;

    fn by_name<'a>(cs: &'a [Connection], name: &str) -> &'a Connection {
        cs.iter().find(|c| c.name == name).unwrap_or_else(|| panic!("no {name}"))
    }

    /// A service is a database by its image (any registry, folder or tag), reached where its container port is
    /// published on this machine — and its account from what its driver reads in it.
    #[test]
    fn compose_service_found_by_image_and_loopback_port() {
        let cs = read("/p/compose.yaml", COMPOSE).unwrap();
        assert_eq!(cs.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["cache", "db", "maria", "mysql"]);
        let db = by_name(&cs, "db");
        assert_eq!(db.shown(), "postgres://app@127.0.0.1:5433/app", "POSTGRES_DB defaults to the user");
        assert_eq!(db.password, Password::Given(Secret::new("tarae-fake-pg")));
        assert_eq!((db.production(), db.tls()), (false, TlsMode::Off));
        assert_eq!(db.origin.place(), Some(("/p/compose.yaml".into(), 5)));
        let mysql = by_name(&cs, "mysql");
        assert_eq!(mysql.shown(), "mysql://root@localhost:3307/shop", "the interpolation's default");
        assert_eq!(mysql.password, Password::Given(Secret::new("tarae-fake-root")));
        let cache = by_name(&cs, "cache");
        assert_eq!(cache.password, Password::Given(Secret::new("tarae-fake-redis")));
        assert_eq!(cache.origin.place(), Some(("/p/compose.yaml".into(), 20)), "a comment after the key");
        assert_eq!(cache.problems, ["port 6379: no fixed one published"]);
        let maria = by_name(&cs, "maria");
        assert_eq!(maria.shown(), "mysql://shop@localhost:3308");
        assert_eq!(maria.password, Password::Unknown("from ${SHOP_PW} — .env isn't read".into()));
    }

    /// A value that may be a password is never said, not even a piece of it: an unclosed `${` is "malformed", a
    /// variable inside other text (a password with a `$`) is "a variable", and a variable is named only when it's
    /// the whole value — `${SHOP_PW}` is a reference, `ab${cdef` and `pa$sword` are passwords.
    #[test]
    fn an_unknown_variable_never_echoes_the_value() {
        let said = |v: &str| interpolate(v).unwrap_err().said();
        for (value, piece) in [("ab${cdef", "cdef"), ("${cdef", "cdef"), ("pa$sword", "sword"), ("pa${SS}word", "SS"), ("x${9z}", "9z")] {
            let s = said(value);
            assert!(!s.contains(piece), "{value:?} said {s:?}");
        }
        assert_eq!(said("${SHOP_PW}"), "from ${SHOP_PW} — .env isn't read");
        assert_eq!(said("$SHOP_PW"), "from ${SHOP_PW} — .env isn't read");
        assert_eq!(interpolate("${A:-x}y$$"), Ok("xy$".into()));
    }

    /// `FOO:` and a bare `- FOO` mean "take FOO from the shell" — a value the plugin can't see, never a known empty
    /// one: no password is claimed known (and empty), and MySQL's random or empty root isn't assumed from a flag
    /// nobody can read. `- FOO=` is the one that's empty.
    #[test]
    fn a_variable_left_to_the_shell_is_unknown_not_empty() {
        let pw = |env: &str, image: &str| {
            let text = format!("services:\n  d:\n    image: {image}\n    ports: [\"1:{}\"]\n    environment:\n{env}", if image == "postgres" { 5432 } else { 3306 });
            read("/p/c.yml", &text).unwrap().remove(0).password
        };
        for env in ["      POSTGRES_PASSWORD:\n", "      - POSTGRES_PASSWORD\n"] {
            assert!(matches!(pw(env, "postgres"), Password::Unknown(w) if w.contains("shell")), "{env:?}");
        }
        assert_eq!(pw("      - POSTGRES_PASSWORD=\n", "postgres"), Password::Given(Secret::new("")), "said empty, it is");
        let mysql = pw("      MYSQL_RANDOM_ROOT_PASSWORD:\n      MYSQL_ROOT_PASSWORD: tarae-fake-root\n", "mysql");
        assert!(matches!(&mysql, Password::Unknown(w) if w.contains("shell")), "{mysql:?}");
        let mysql = pw("      - MYSQL_ALLOW_EMPTY_PASSWORD\n", "mysql");
        assert!(matches!(&mysql, Password::Unknown(w) if w.contains("shell")), "not \"no password\": {mysql:?}");
    }

    /// Compose's own forms, read exactly: only `${V:-d}` and `${V-d}` give a value here; `${V:?err}`, `${V?err}`,
    /// `${V:+alt}`, `${V+alt}` depend on the shell — so a `-` inside an error message is never taken for a default,
    /// nor the words after it for a password.
    #[test]
    fn compose_interpolation_forms_are_read_exactly() {
        assert_eq!(interpolate("${X:-a-b}"), Ok("a-b".into()));
        assert_eq!(interpolate("${X-d}"), Ok("d".into()));
        for unknown in ["${PG_PW:?must-be-set}", "${PG_PW?must-be-set}", "${PG_PW:+a-b}", "${PG_PW+a-b}", "${PG_PW}"] {
            assert_eq!(interpolate(unknown), Err(Unresolved::Var("PG_PW".into())), "{unknown}");
        }
        assert_eq!(interpolate("${9X:-d}"), Err(Unresolved::Malformed));
        assert_eq!(interpolate("${X*d}"), Err(Unresolved::Malformed));
        let text = "services:\n  d:\n    image: postgres\n    ports: [\"1:5432\"]\n    environment:\n      POSTGRES_PASSWORD: \"${PG_PW:?must-be-set}\"\n";
        assert_eq!(read("/p/c.yml", text).unwrap()[0].password, Password::Unknown("from ${PG_PW} — .env isn't read".into()));
    }

    /// serde_yaml's messages can quote the file — a password in it among others: only the line goes out.
    #[test]
    fn a_compose_file_that_doesnt_read_is_said_by_line_only() {
        let why = read("/p/c.yml", "services:\n  db:\n    image: postgres\n    environment: [POSTGRES_PASSWORD=tarae-fake-pg\n").unwrap_err();
        assert!(why.starts_with("line ") && !why.contains("tarae-fake"), "{why}");
        let why = read("/p/c.yml", "services: [\"tarae-fake-pg\"]\n").unwrap_err();
        assert!(!why.contains("tarae-fake"), "{why}");
    }
}
