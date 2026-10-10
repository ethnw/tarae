//! File names that look like keys and tokens (`.env`, `*.pem`, `id_rsa` …), as data —
//! `runtime/secret-names.txt`: never copied for recovery (`recovery.rs`), never read or found through a
//! plugin's `project` permission (`plugin/perm.rs`, `plugin/io.rs`), closed to Claude's commands inside the
//! project on macOS (`sandbox::key_regexes`). Its `!` lines are exceptions (`.env.example`, `*.pub`).

use std::path::Path;

const LIST: &str = include_str!("../runtime/secret-names.txt");

/// The data's lines: `#` lines and blank ones left out.
fn lines() -> impl Iterator<Item = &'static str> {
    LIST.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#'))
}

/// The patterns: one a line, but the exceptions.
pub fn patterns() -> impl Iterator<Item = &'static str> {
    lines().filter(|l| !l.starts_with('!'))
}

/// The exceptions: the `!` lines, without it — a name one matches isn't secret, whatever `patterns` say.
pub fn exceptions() -> impl Iterator<Item = &'static str> {
    lines().filter_map(|l| l.strip_prefix('!'))
}

/// Whether file name `name` looks like a key's or a token's, in any case.
pub fn is_secret(name: &str) -> bool {
    let name = name.to_lowercase();
    patterns().any(|p| wild(p, &name)) && !exceptions().any(|p| wild(p, &name))
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
    /// uppercase pattern would match nothing), one `*` at most, no folders — the exceptions too, and each of
    /// those takes back a name a pattern gives (one that doesn't is a typo).
    #[test]
    fn every_pattern_can_match() {
        let all: Vec<&str> = patterns().collect();
        assert!(all.len() > 10, "{all:?}");
        let except: Vec<&str> = exceptions().collect();
        assert!(except.len() >= 3, "{except:?}");
        for p in all.iter().chain(&except) {
            assert_eq!(*p, p.to_lowercase(), "{p}: lowercase");
            assert!(p.matches('*').count() <= 1 && !p.contains('/') && !p.starts_with('!'), "{p}");
        }
        // (an exception's `*` filled with a pattern's own text: `*.pub` with `id_rsa*`'s is `id_rsa.pub`)
        for e in except {
            let examples = all.iter().map(|p| e.replace('*', &p.replace('*', "")));
            let taken = examples.filter(|x| wild(e, x)).any(|x| all.iter().any(|p| wild(p, &x)));
            assert!(taken, "{e}: no pattern gives a name it takes back");
        }
    }

    /// Keys and tokens by their names, in any case; code that only shares a part of one isn't; nor are the
    /// exceptions — templates of a `.env` and public keys.
    #[test]
    fn names_like_keys_and_tokens_are_secret() {
        for n in
            [".env", ".env.local", "prod.env", "server.PEM", "id_ed25519", "id_rsa", ".npmrc", "credentials"]
        {
            assert!(is_secret(n), "{n}");
        }
        for n in ["env.rs", "main.rs", "id_utils.py", "keys.rs", "credentials.rs", "README.md"] {
            assert!(!is_secret(n), "{n}");
        }
        for n in [".env.example", ".ENV.Sample", "id_rsa.pub", "id_ed25519.pub", "deploy.pub"] {
            assert!(!is_secret(n), "{n}: an exception");
        }
    }
}
