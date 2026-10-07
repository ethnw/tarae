//! PostgreSQL. Step 1: its names, the compose images that run it and how one says its account (the official
//! image's `POSTGRES_*` — INFERRED until step 2's container), libpq's variables (VERIFIED `pg-libpq-envars`).

pub mod wire;

use std::collections::BTreeMap;

use super::{Driver, Kind};
use crate::compose::Service;
use crate::conn::{Connection, Origin, Password, TlsMode};

pub struct Postgres;

impl Driver for Postgres {
    fn kind(&self) -> Kind {
        Kind::Postgres
    }

    fn short(&self) -> &'static str {
        "pg"
    }

    fn schemes(&self) -> &'static [(&'static str, bool)] {
        &[("postgres", false), ("postgresql", false)]
    }

    fn default_port(&self) -> u16 {
        5432
    }

    fn images(&self) -> &'static [&'static str] {
        &["postgres", "postgresql", "postgis", "timescaledb"]
    }

    fn compose_account(&self, c: &mut Connection, s: &Service) {
        let user = s.known("POSTGRES_USER").unwrap_or_else(|| "postgres".into());
        c.database = Some(s.known("POSTGRES_DB").unwrap_or_else(|| user.clone()));
        c.user = Some(user);
        c.password = match s.flag("POSTGRES_PASSWORD_FILE") {
            Some(Ok(true)) => Password::Unknown("in a file inside the container".into()),
            Some(Err(why)) => Password::Unknown(why),
            _ if s.known("POSTGRES_HOST_AUTH_METHOD").as_deref() == Some("trust") => Password::None,
            _ => s.password("POSTGRES_PASSWORD"),
        };
    }

    fn env_names(&self) -> &'static [&'static str] {
        &["PGHOST", "PGPORT", "PGDATABASE", "PGUSER", "PGPASSWORD", "PGSSLMODE"]
    }

    /// libpq's: a connection when `PGHOST` or `PGDATABASE` says where (`PGHOST` unset is a local socket for libpq —
    /// here, TCP to localhost).
    fn env_connections(&self, vars: &BTreeMap<String, String>) -> (Vec<Connection>, Vec<String>) {
        let pg = |n: &str| vars.get(n).filter(|v| !v.is_empty());
        if pg("PGHOST").is_none() && pg("PGDATABASE").is_none() {
            return (Vec::new(), Vec::new());
        }
        let host = pg("PGHOST").cloned().unwrap_or_else(|| "localhost".into());
        let mut c = Connection::new("pg", Kind::Postgres, host, Origin::Env("PGHOST".into()));
        if let Some(p) = pg("PGPORT") {
            match p.parse() {
                Ok(p) => c.port = p,
                Err(_) => c.problems.push("$PGPORT isn't a port".into()),
            }
        }
        (c.user, c.database) = (pg("PGUSER").cloned(), pg("PGDATABASE").cloned());
        if let Some(m) = pg("PGSSLMODE") {
            match TlsMode::from_name(m) {
                Some(t) => c.tls = Some(t),
                None => c.problems.push("$PGSSLMODE isn't a mode the plugin knows".into()),
            }
        }
        if pg("PGHOST").is_some_and(|h| h.starts_with('/')) {
            c.problems.push("$PGHOST is a socket's folder — the plugin connects over TCP only".into());
        }
        (vec![c], Vec::new())
    }

    fn password_var(&self) -> Option<&'static str> {
        Some("PGPASSWORD")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::secret::Secret;

    fn account(env: &[(&str, &str)]) -> Connection {
        let mut c = Connection::new("db", Kind::Postgres, "localhost", Origin::Env("X".into()));
        Postgres.compose_account(&mut c, &Service::new(env, &[]));
        c
    }

    /// The official image's own ways: `POSTGRES_DB` defaults to the user, `trust` asks no password even where one
    /// is set, a password in a file stays the container's.
    #[test]
    fn a_compose_services_account_as_the_image_says_it() {
        let c = account(&[("POSTGRES_USER", "app"), ("POSTGRES_PASSWORD", "tarae-fake-pg")]);
        assert_eq!((c.user.as_deref(), c.database.as_deref()), (Some("app"), Some("app")));
        assert_eq!(c.password, Password::Given(Secret::new("tarae-fake-pg")));
        let trust = account(&[("POSTGRES_HOST_AUTH_METHOD", "trust"), ("POSTGRES_PASSWORD", "tarae-fake-pg")]);
        assert_eq!(trust.password, Password::None);
        assert!(matches!(account(&[("POSTGRES_PASSWORD_FILE", "/run/secrets/pw")]).password, Password::Unknown(_)));
        assert_eq!(account(&[]).user.as_deref(), Some("postgres"));
    }

    #[test]
    fn libpqs_variables_name_a_connection() {
        let vars: BTreeMap<String, String> =
            [("PGHOST", "/var/run/postgresql"), ("PGPORT", "x"), ("PGDATABASE", "shop")].iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        let (cs, _) = Postgres.env_connections(&vars);
        assert_eq!(cs[0].database.as_deref(), Some("shop"));
        assert_eq!(cs[0].problems, ["$PGPORT isn't a port", "$PGHOST is a socket's folder — the plugin connects over TCP only"]);
        assert!(Postgres.env_connections(&BTreeMap::new()).0.is_empty(), "nothing set, nothing named");
    }
}
