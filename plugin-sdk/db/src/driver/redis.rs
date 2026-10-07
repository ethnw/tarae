//! Redis (and Valkey: one wire). Step 1: its names (`rediss` asks for TLS), the compose images that run it and
//! their password (`--requirepass`, or bitnami's `REDIS_PASSWORD` — INFERRED until step 9's container),
//! `REDIS_URL`, and `REDISCLI_AUTH` (VERIFIED `redis-cli`) for one that has none.

use std::collections::BTreeMap;

use super::{Driver, Kind};
use crate::compose::Service;
use crate::conn::Connection;

pub struct Redis;

impl Driver for Redis {
    fn kind(&self) -> Kind {
        Kind::Redis
    }

    fn short(&self) -> &'static str {
        "redis"
    }

    fn schemes(&self) -> &'static [(&'static str, bool)] {
        &[("redis", false), ("rediss", true)]
    }

    fn default_port(&self) -> u16 {
        6379
    }

    fn images(&self) -> &'static [&'static str] {
        &["redis", "redis-stack", "redis-stack-server", "valkey"]
    }

    fn compose_account(&self, c: &mut Connection, s: &Service) {
        c.password = Service::as_password(s.word_after("--requirepass").or_else(|| s.value("REDIS_PASSWORD")));
    }

    fn env_names(&self) -> &'static [&'static str] {
        &["REDIS_URL", "REDISCLI_AUTH"]
    }

    fn env_connections(&self, vars: &BTreeMap<String, String>) -> (Vec<Connection>, Vec<String>) {
        match vars.get("REDIS_URL").filter(|v| !v.is_empty()).map(|u| crate::env::from_url("REDIS_URL", u)) {
            Some(Ok(c)) => (vec![c], Vec::new()),
            Some(Err(why)) => (Vec::new(), vec![why]),
            None => (Vec::new(), Vec::new()),
        }
    }

    fn password_var(&self) -> Option<&'static str> {
        Some("REDISCLI_AUTH")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::{Origin, Password};
    use crate::secret::Secret;

    #[test]
    fn the_password_is_requirepass_s_or_the_images_variable() {
        let pw = |env: &[(&str, &str)], words: &[&str]| {
            let mut c = Connection::new("c", Kind::Redis, "localhost", Origin::Env("X".into()));
            Redis.compose_account(&mut c, &Service::new(env, words));
            c.password
        };
        assert_eq!(pw(&[], &["redis-server", "--requirepass", "tarae-fake-redis"]), Password::Given(Secret::new("tarae-fake-redis")));
        assert_eq!(pw(&[("REDIS_PASSWORD", "tarae-fake-b")], &[]), Password::Given(Secret::new("tarae-fake-b")));
        assert_eq!(pw(&[], &["redis-server"]), Password::None);
    }
}
