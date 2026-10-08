//! File names that look like keys and tokens (`.env`, `*.pem`, `id_rsa` …), as data —
//! `runtime/secret-names.txt`: never copied for recovery (`recovery.rs`), never read or found through a
//! plugin's `project` permission (`plugin/perm.rs`, `plugin/io.rs`).

use std::path::Path;

const LIST: &str = include_str!("../runtime/secret-names.txt");

/// The patterns: one a line, `#` lines and blank ones left out.
pub fn patterns() -> impl Iterator<Item = &'static str> {
    LIST.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'))
}

/// Whether file name `name` looks like a key's or a token's, in any case.
pub fn is_secret(name: &str) -> bool {
    let name = name.to_lowercase();
    patterns().any(|p| wild(p, &name))
}

/// Whether `path`'s file name does (`is_secret`).
pub fn named_secret(path: &Path) -> bool {
    path.file_name().is_some_and(|n| is_secret(&n.to_string_lossy()))
}

/// `pat` (one `*` at most) matches all of `name`.
pub fn wild(pat: &str, name: &str) -> bool {
    match pat.split_once('*') {
        Some((a, b)) => name.len() >= a.len() + b.len() && name.starts_with(a) && name.ends_with(b),
        None => pat == name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The data reads as patterns a match can use: lowercase (names are lowered before matching — an
    /// uppercase pattern would match nothing), one `*` at most, no folders.
    #[test]
    fn every_pattern_can_match() {
        let all: Vec<&str> = patterns().collect();
        assert!(all.len() > 10, "{all:?}");
        for p in all {
            assert_eq!(p, p.to_lowercase(), "{p}: lowercase");
            assert!(p.matches('*').count() <= 1 && !p.contains('/'), "{p}");
        }
    }

    /// Keys and tokens by their names, in any case; code that only shares a part of one isn't.
    #[test]
    fn names_like_keys_and_tokens_are_secret() {
        for n in [
            ".env",
            ".env.local",
            "prod.env",
            "server.PEM",
            "id_ed25519",
            "id_rsa.pub",
            ".npmrc",
            "credentials",
        ] {
            assert!(is_secret(n), "{n}");
        }
        for n in ["env.rs", "main.rs", "id_utils.py", "keys.rs", "credentials.rs", "README.md"] {
            assert!(!is_secret(n), "{n}");
        }
    }
}
