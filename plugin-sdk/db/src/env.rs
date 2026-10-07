//! Connections your environment names (the variables tarae started with — only those the plugin may read):
//! `DATABASE_URL`, whatever database its scheme names, and each driver's own (`Driver::env_connections`) — with the
//! variable a driver takes as the password for what has none of its own (`Driver::password_var`).

use std::collections::BTreeMap;

use crate::conn::{Connection, Origin, Password};
use crate::driver::{self, Kind};
use crate::secret::Secret;

/// The connection a URL in variable `var` names — or why it can't be used (in tarae's words: the URL may hold a
/// password).
pub fn from_url(var: &str, url: &str) -> Result<Connection, String> {
    let u = crate::url::parse(url).map_err(|why| format!("${var}: {why}"))?;
    let mut c = Connection::new(var.to_ascii_lowercase(), u.kind, u.host, Origin::Env(var.to_string()));
    if let Some(p) = u.port {
        c.port = p;
    }
    (c.user, c.database, c.tls) = (u.user, u.database, u.tls);
    c.password = u.password.map_or(Password::None, Password::Given);
    Ok(c)
}

/// What the variables (name → value, unset ones left out) name: connections, and why some can't be used.
pub fn connections(vars: &BTreeMap<String, String>) -> (Vec<Connection>, Vec<String>) {
    let (mut out, mut problems) = (Vec::new(), Vec::new());
    if let Some(url) = vars.get("DATABASE_URL").filter(|v| !v.is_empty()) {
        match from_url("DATABASE_URL", url) {
            Ok(c) => out.push(c),
            Err(why) => problems.push(why),
        }
    }
    for d in driver::ALL {
        let (cs, why) = d.env_connections(vars);
        out.extend(cs);
        problems.extend(why);
    }
    for c in &mut out {
        let fallback = Kind::driver(c.kind).password_var().and_then(|v| vars.get(v));
        if let (Password::None, Some(pw)) = (&c.password, fallback) {
            c.password = Password::Given(Secret::new(pw.clone()));
        }
    }
    (out, problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vars(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn urls_and_libpqs_variables_each_a_connection() {
        let (cs, problems) = connections(&vars(&[
            ("DATABASE_URL", "postgres://app@db.internal/shop"),
            ("REDIS_URL", "redis://localhost:6380/1"),
            ("PGHOST", "localhost"),
            ("PGPORT", "5433"),
            ("PGUSER", "me"),
            ("PGPASSWORD", "tarae-fake-pg"),
            ("REDISCLI_AUTH", "tarae-fake-redis"),
        ]));
        assert!(problems.is_empty(), "{problems:?}");
        let shown: Vec<String> = cs.iter().map(|c| format!("{} {}", c.name, c.shown())).collect();
        assert_eq!(shown, ["database_url postgres://app@db.internal:5432/shop", "pg postgres://me@localhost:5433", "redis_url redis://localhost:6380/1"]);
        assert!(cs[0].production(), "a remote host nobody marked");
        assert_eq!(cs[0].password, Password::Given(Secret::new("tarae-fake-pg")), "PGPASSWORD for what has none");
        assert_eq!(cs[2].password, Password::Given(Secret::new("tarae-fake-redis")));
    }

    #[test]
    fn a_url_that_doesnt_read_is_a_problem_said_without_it() {
        let (cs, problems) = connections(&vars(&[("DATABASE_URL", "postgres://app:tarae-fake-pw@h:x/db")]));
        assert!(cs.is_empty());
        assert_eq!(problems, ["$DATABASE_URL: its port isn't a number"]);
    }
}
