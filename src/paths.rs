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

/// Whether `path`, as written (a document's real path when opened), is in `root` — as written or by its real
/// path — with no link followed now: a folder swapped for a link since doesn't take it out (a write there goes
/// through `beneath.rs`, which refuses the link).
pub fn written_inside(root: &Path, path: &Path) -> bool {
    path.starts_with(root) || std::fs::canonicalize(root).is_ok_and(|r| path.starts_with(r))
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

/// A file opened to read, and where it really is — as the descriptor says, not the path again.
pub struct Opened {
    pub file: std::fs::File,
    /// The opened file's own path (`F_GETPATH` / `/proc/self/fd`): links, `..` and any change since gone.
    pub real: PathBuf,
    pub meta: std::fs::Metadata,
}

/// `path` opened to read first, then judged by what was opened: a path judged and then read again can be
/// changed in between — a folder on it swapped for a link by a run that writes the project — and the read
/// follows it where the judge never looked. So tarae's own reads for Claude (`read`, `search`) open, take the
/// descriptor's real path, judge that, and read from that descriptor. Non-blocking (a FIFO doesn't wait for a
/// writer) and no controlling terminal; a folder opens too (`meta` says so).
pub fn open_read(path: &Path) -> std::io::Result<Opened> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY | libc::O_CLOEXEC)
        .open(path)?;
    let meta = file.metadata()?;
    let real = fd_path(&file)?;
    Ok(Opened { file, real, meta })
}

/// Where an open descriptor's file is.
#[cfg(target_os = "macos")]
fn fd_path(file: &std::fs::File) -> std::io::Result<PathBuf> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::io::AsRawFd;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes (with its NUL) into the buffer given.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETPATH, buf.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&buf[..end])))
}

#[cfg(not(target_os = "macos"))]
fn fd_path(file: &std::fs::File) -> std::io::Result<PathBuf> {
    use std::os::unix::io::AsRawFd;
    std::fs::read_link(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What was opened, by its descriptor: a link's target, the folder's real path (macOS `/tmp` is
    /// `/private/tmp`), and a FIFO opened without waiting for a writer.
    #[test]
    fn what_was_opened_says_where_it_is() {
        let scratch = crate::scratch::Scratch::new("paths-opened");
        let dir = scratch.to_path_buf();
        std::fs::create_dir_all(dir.join("in")).unwrap();
        std::fs::write(dir.join("in/a.txt"), "a").unwrap();
        std::os::unix::fs::symlink(dir.join("in"), dir.join("link")).unwrap();
        let real = std::fs::canonicalize(dir.join("in/a.txt")).unwrap();
        let o = open_read(&dir.join("link/a.txt")).unwrap();
        assert_eq!((o.real, o.meta.is_file()), (real, true), "where the link led");
        assert!(open_read(&dir.join("in")).unwrap().meta.is_dir());
        let fifo = std::ffi::CString::new(dir.join("fifo").to_str().unwrap()).unwrap();
        // SAFETY: a path made above, a mode.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(!open_read(&dir.join("fifo")).unwrap().meta.is_file(), "opened, not waited on");
        std::fs::remove_dir_all(&dir).unwrap();
    }

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
