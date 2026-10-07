//! The Driver boundary (db-plan.md §10): everything one database knows is one module behind `Driver`, so a
//! driver could become a plugin of its own sharing this crate — the rest of the plugin names no database. This is
//! the only list of them.
//!
//! What the trait holds is what the plugin does today — finding connections. Each later step adds its part, with
//! the code and the tests behind it: connect and authenticate, check or describe a statement or a name, run it page
//! by page, read-only enforcement and whether something is a write, the side tab's objects.

mod mysql;
mod postgres;
mod redis;

use std::collections::BTreeMap;

use crate::compose::Service;
use crate::conn::Connection;

pub trait Driver: Sync {
    fn kind(&self) -> Kind;
    /// Its name where room is short (the side tab): `pg`.
    fn short(&self) -> &'static str;
    /// Its names — the first is how it's shown (`postgres://…`) — as a config's `kind` and URL schemes say them, each
    /// with whether that scheme asks for TLS (`rediss`).
    fn schemes(&self) -> &'static [(&'static str, bool)];
    fn default_port(&self) -> u16;
    /// The compose images that run it, by name (registry, folders and tag taken off).
    fn images(&self) -> &'static [&'static str];
    /// A compose service running one of its images: the account (user, database, password) it says.
    fn compose_account(&self, c: &mut Connection, service: &Service);
    /// The variables of its own it reads (they're in the plugin's `permissions()`).
    fn env_names(&self) -> &'static [&'static str] {
        &[]
    }
    /// The connections its own variables name, and why some of them can't be used.
    fn env_connections(&self, _vars: &BTreeMap<String, String>) -> (Vec<Connection>, Vec<String>) {
        (Vec::new(), Vec::new())
    }
    /// The variable whose value is the password for one of its connections that has none (`PGPASSWORD`).
    fn password_var(&self) -> Option<&'static str> {
        None
    }
}

/// Which database: an id, its knowledge in `driver()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Postgres,
    Mysql,
    Redis,
}

pub const ALL: [&dyn Driver; 3] = [&postgres::Postgres, &mysql::Mysql, &redis::Redis];

impl Kind {
    pub fn driver(self) -> &'static dyn Driver {
        match self {
            Kind::Postgres => &postgres::Postgres,
            Kind::Mysql => &mysql::Mysql,
            Kind::Redis => &redis::Redis,
        }
    }

    /// The kind a URL's scheme names, and whether the scheme itself asks for TLS.
    pub fn from_scheme(scheme: &str) -> Option<(Kind, bool)> {
        let s = scheme.to_ascii_lowercase();
        ALL.iter().find_map(|d| d.schemes().iter().find(|(n, _)| *n == s).map(|&(_, tls)| (d.kind(), tls)))
    }

    /// As a config's `kind` names it.
    pub fn from_name(name: &str) -> Option<Kind> {
        Kind::from_scheme(name).map(|(k, _)| k)
    }

    /// A compose image's name.
    pub fn from_image(name: &str) -> Option<Kind> {
        ALL.iter().find(|d| d.images().contains(&name)).map(|d| d.kind())
    }

    pub fn scheme(self) -> &'static str {
        self.driver().schemes()[0].0
    }

    pub fn default_port(self) -> u16 {
        self.driver().default_port()
    }
}

/// Every variable the plugin may read: `DATABASE_URL` (any of them), then each driver's own.
pub fn env_names() -> Vec<&'static str> {
    let mut names = vec!["DATABASE_URL"];
    names.extend(ALL.iter().flat_map(|d| d.env_names().iter().copied()));
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name is one driver's: a scheme, an image or a variable two drivers claimed would go to whichever is listed
    /// first, quietly. And each driver is the one its kind finds.
    #[test]
    fn every_name_belongs_to_one_driver() {
        let (mut schemes, mut images, mut vars) = (Vec::new(), Vec::new(), env_names());
        for d in ALL {
            assert_eq!(d.kind().driver().kind(), d.kind(), "{:?}", d.kind());
            schemes.extend(d.schemes().iter().map(|(n, _)| *n));
            images.extend(d.images());
        }
        for (what, names) in [("scheme", &mut schemes), ("image", &mut images), ("variable", &mut vars)] {
            let all = names.len();
            names.sort();
            names.dedup();
            assert_eq!(names.len(), all, "a {what} claimed twice: {names:?}");
        }
    }
}
