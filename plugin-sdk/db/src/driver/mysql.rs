//! MySQL and MariaDB (one wire). Step 1: their names and the compose images that run them, and how one says its
//! account — the official images' `MYSQL_*`/`MARIADB_*` (INFERRED until step 8's container).

use super::{Driver, Kind};
use crate::compose::Service;
use crate::conn::{Connection, Password};

pub struct Mysql;

impl Driver for Mysql {
    fn kind(&self) -> Kind {
        Kind::Mysql
    }

    fn short(&self) -> &'static str {
        "mysql"
    }

    fn schemes(&self) -> &'static [(&'static str, bool)] {
        &[("mysql", false), ("mariadb", false)]
    }

    fn default_port(&self) -> u16 {
        3306
    }

    fn images(&self) -> &'static [&'static str] {
        &["mysql", "mysql-server", "mariadb"]
    }

    /// The user the image made (`MYSQL_USER` + `MYSQL_PASSWORD`), else root — with no password, a random one, or
    /// the one given.
    fn compose_account(&self, c: &mut Connection, s: &Service) {
        let either = |a: &str, b: &str| s.value(a).or_else(|| s.value(b));
        c.database = either("MYSQL_DATABASE", "MARIADB_DATABASE").and_then(Result::ok);
        let user = either("MYSQL_USER", "MARIADB_USER").and_then(Result::ok);
        if let (Some(u), Some(pw)) = (user, either("MYSQL_PASSWORD", "MARIADB_PASSWORD")) {
            c.user = Some(u);
            c.password = Service::as_password(Some(pw));
            return;
        }
        c.user = Some("root".into());
        // (a switch whose value comes from the shell decides nothing here: the root password is then unknown)
        let flag = |a: &str, b: &str| s.flag(a).or_else(|| s.flag(b));
        let random = flag("MYSQL_RANDOM_ROOT_PASSWORD", "MARIADB_RANDOM_ROOT_PASSWORD");
        let empty = flag("MYSQL_ALLOW_EMPTY_PASSWORD", "MARIADB_ALLOW_EMPTY_ROOT_PASSWORD");
        c.password = match (random, empty) {
            (Some(Err(why)), _) | (_, Some(Err(why))) => Password::Unknown(why),
            (Some(Ok(true)), _) => Password::Unknown("a random root password, in the container's log".into()),
            (_, Some(Ok(true))) => Password::None,
            _ => Service::as_password(either("MYSQL_ROOT_PASSWORD", "MARIADB_ROOT_PASSWORD")),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conn::Origin;
    use crate::secret::Secret;

    fn account(env: &[(&str, &str)]) -> Connection {
        let mut c = Connection::new("db", Kind::Mysql, "localhost", Origin::Env("X".into()));
        Mysql.compose_account(&mut c, &Service::new(env, &[]));
        c
    }

    /// The images' own ways: their user if both name and password are given, else root — empty, random (only the
    /// container's log knows it), or given; `$$` is one `$`.
    #[test]
    fn a_compose_services_account_as_the_image_says_it() {
        let c = account(&[("MARIADB_USER", "shop"), ("MARIADB_PASSWORD", "${SHOP_PW}"), ("MARIADB_DATABASE", "shop")]);
        assert_eq!((c.user.as_deref(), c.database.as_deref()), (Some("shop"), Some("shop")));
        assert_eq!(c.password, Password::Unknown("from ${SHOP_PW} — .env isn't read".into()));
        assert_eq!(account(&[("MYSQL_ALLOW_EMPTY_PASSWORD", "yes")]).password, Password::None);
        assert!(matches!(account(&[("MYSQL_RANDOM_ROOT_PASSWORD", "yes")]).password, Password::Unknown(_)));
        let root = account(&[("MYSQL_USER", "app"), ("MYSQL_ROOT_PASSWORD", "$$abc")]);
        assert_eq!((root.user.as_deref(), root.password), (Some("root"), Password::Given(Secret::new("$abc"))), "a user with no password isn't it");
    }
}
