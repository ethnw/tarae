//! Where a path really leads, and whether that's inside a folder (the project) — one answer for the checks
//! that keep Claude's reads, edits, searches, replaces and runs in the project. Real paths: links followed,
//! `..` gone through (macOS `/tmp` is `/private/tmp`).

use std::path::{Path, PathBuf};

/// Where `path` really leads — its deepest existing folder resolved — if that's inside `root` (resolved
/// itself). For a path that may not exist yet (an edit making a file).
pub fn inside(root: &Path, path: &Path) -> Option<PathBuf> {
    let root = std::fs::canonicalize(root).ok()?;
    real(path).filter(|real| real.starts_with(&root))
}

/// Where `path` really leads: its deepest existing folder resolved (links followed, `..` gone through), the
/// names below it as they are — None if one of those is `.`/`..` (it would climb unseen).
pub fn real(path: &Path) -> Option<PathBuf> {
    let mut rest = Vec::new();
    let mut at = path.to_path_buf();
    let real = loop {
        if let Ok(real) = std::fs::canonicalize(&at) {
            break real;
        }
        rest.push(at.file_name()?.to_os_string());
        at = at.parent()?.to_path_buf();
    };
    // What doesn't exist yet: plain names only (a `..` there would climb out unseen — `file_name` already
    // refuses a path ending in one, above; this holds whatever a path's parts turn out to be)
    let mut real = real;
    for name in rest.iter().rev() {
        if name == ".." || name == "." {
            return None;
        }
        real.push(name);
    }
    Some(real)
}

/// `path` — which must be there — by its real path, if that's inside `root`; else why, naming it as `shown`:
/// `shown: <error>` when it isn't there, `shown is outside the project — <why>` when it leads out.
pub fn existing_inside(root: &Path, path: &Path, shown: &str, why: &str) -> Result<PathBuf, String> {
    let real = std::fs::canonicalize(path).map_err(|e| format!("{shown}: {e}"))?;
    let root = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
    if !real.starts_with(&root) {
        return Err(format!("{shown} is outside the project — {why}"));
    }
    Ok(real)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Inside means where the path really leads — links followed and `..` gone through — whether it's there
    /// yet (`inside`) or must be (`existing_inside`, which says why not).
    #[test]
    fn inside_is_where_a_path_really_leads() {
        let dir = std::env::temp_dir().join(format!("tarae-paths-inside-{}", std::process::id()));
        let outside = std::env::temp_dir().join(format!("tarae-paths-outside-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(dir.join("src/main.rs"), "").unwrap();
        std::fs::write(outside.join("x.rs"), "").unwrap();
        let root = std::fs::canonicalize(&dir).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("away")).ok();
        let at = |p: &str| inside(&root, &root.join(p));
        assert_eq!(at("src/main.rs"), Some(root.join("src/main.rs")));
        assert_eq!(
            at("src/new/deeper/file.rs"),
            Some(root.join("src/new/deeper/file.rs")),
            "folders to come"
        );
        assert_eq!(at("src/../src/lib.rs"), Some(root.join("src/lib.rs")));
        assert_eq!(at("src/new/../../../x.rs"), None, "a `..` below what's there");
        assert_eq!(at("../elsewhere.rs"), None);
        assert_eq!(at("away/x.rs"), None, "a link out of the project");
        assert_eq!(inside(&root, Path::new("/etc/hosts")), None);
        // (the root as given: /tmp → /private/tmp on macOS)
        assert_eq!(inside(&root, &dir.join("src/main.rs")), Some(root.join("src/main.rs")));
        // Must be there
        let there = |p: &str| existing_inside(&dir, &dir.join(p), p, "looked inside it only");
        assert_eq!(there("src/main.rs"), Ok(root.join("src/main.rs")));
        assert_eq!(
            there("away/x.rs"),
            Err("away/x.rs is outside the project — looked inside it only".into())
        );
        assert!(there("src/none.rs").is_err_and(|e| e.starts_with("src/none.rs: ")), "not there");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&outside).ok();
    }
}
