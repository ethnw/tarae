//! A `~/.pgpass` read as libpq reads it: `hostname:port:database:username:password`, each of the first four a value
//! or `*`, `\` before a `:` or `\` taken literally, `#` lines aside, the first line that matches the one (VERIFIED
//! `pg-libpq-pgpass`). The password, de-escaped, ends at its first unescaped `:` — what follows is ignored (libpq's
//! `passwordFromFile`, from its source as recalled — INFERRED). Its password never leaves here but as a `Secret`.

use crate::secret::Secret;

/// The password for this host, port, database and user — from the first line matching them. (A connection that
/// names no database is matched as libpq's: its database is the user's name.)
pub fn lookup(file: &str, host: &str, port: u16, database: &str, user: &str) -> Option<Secret> {
    let port = port.to_string();
    file.lines().filter(|l| !l.starts_with('#')).find_map(|line| {
        let f = fields(line);
        let [h, p, d, u, pw, ..] = f.as_slice() else { return None };
        let fits = |field: &str, value: &str| field == "*" || field == value;
        (fits(h, host) && fits(p, &port) && fits(d, database) && fits(u, user)).then(|| Secret::new(pw.as_str()))
    })
}

/// A line's fields: split at every unescaped `:`, `\:` and `\\` kept as the character.
fn fields(line: &str) -> Vec<String> {
    let mut out = vec![String::new()];
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => out.last_mut().unwrap().extend(chars.next()),
            ':' => out.push(String::new()),
            c => out.last_mut().unwrap().push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// libpq's rules, each: `*` matches any value, the first matching line wins (so the specific goes first), `\`
    /// lets a field hold `:` or `\`, and the password ends at its first unescaped `:` (an escaped one is kept).
    /// (A `#` line is skipped as libpq does — though its host, starting with `#`, could never match anyway.)
    #[test]
    fn a_pgpass_line_matches_as_libpq_reads_it() {
        let file = "localhost:5433:shop:app:tarae-fake-specific\n\
                    *:*:*:app:tarae-fake-wild\n\
                    db\\:x:5432:*:o\\\\k:tarae-fake-escaped\n\
                    h:5432:*:cut:tarae:fake:end\n\
                    h:5432:*:kept:tarae\\:fake\\\\x\n";
        let pw = |host, port, db, user| lookup(file, host, port, db, user).map(|s| s.expose().to_string());
        assert_eq!(pw("localhost", 5433, "shop", "app").as_deref(), Some("tarae-fake-specific"), "the first that matches");
        assert_eq!(pw("localhost", 5432, "shop", "app").as_deref(), Some("tarae-fake-wild"));
        assert_eq!(pw("db:x", 5432, "any", "o\\k").as_deref(), Some("tarae-fake-escaped"));
        assert_eq!(pw("h", 5432, "any", "cut").as_deref(), Some("tarae"), "to its first unescaped `:`");
        assert_eq!(pw("h", 5432, "any", "kept").as_deref(), Some("tarae:fake\\x"), "de-escaped");
        assert_eq!(pw("other", 5432, "shop", "nobody"), None);
    }
}
