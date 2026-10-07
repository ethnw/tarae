//! Path patterns as tarae's are: `*` within a name, `**` any folders (none too).

/// Whether `path` (absolute) is one `pattern` (absolute) covers.
pub fn matches(pattern: &str, path: &str) -> bool {
    let parts = |s: &str| s.split('/').filter(|p| !p.is_empty()).map(str::to_string).collect::<Vec<_>>();
    fn go(p: &[String], n: &[String]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some((first, rest)) if first == "**" => (0..=n.len()).any(|k| go(rest, &n[k..])),
            Some((first, rest)) => n.split_first().is_some_and(|(name, more)| name_matches(first, name) && go(rest, more)),
        }
    }
    go(&parts(pattern), &parts(path))
}

/// A name against a pattern with `*` (any run of characters) and `?` (one).
fn name_matches(pat: &str, name: &str) -> bool {
    let (p, n): (Vec<char>, Vec<char>) = (pat.chars().collect(), name.chars().collect());
    fn go(p: &[char], n: &[char]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some(('*', rest)) => (0..=n.len()).any(|k| go(rest, &n[k..])),
            Some((c, rest)) => n.split_first().is_some_and(|(m, more)| (*c == '?' || c == m) && go(rest, more)),
        }
    }
    go(&p, &n)
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn stars_within_a_name_double_stars_across_folders() {
        assert!(matches("/p/migrations/**", "/p/migrations/0042_orders.sql"));
        assert!(matches("/p/migrations/**", "/p/migrations/pg/0042.sql"));
        assert!(matches("/p/**/*.sql", "/p/a.sql"), "** is no folders too");
        assert!(matches("/p/**/*compose*.y*ml", "/p/deploy/docker-compose.override.yml"));
        assert!(!matches("/p/migrations/*", "/p/migrations/pg/0042.sql"), "* stays within a name");
        assert!(!matches("/p/src/**", "/p/srcs/a.rs"));
    }
}
