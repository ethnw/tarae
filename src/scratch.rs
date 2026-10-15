//! A test's own scratch folder (f22 p). A path made of a name and the process id is one anyone on the machine
//! can guess, and a test that cleared it first (`remove_dir_all`) would delete what another user, another run or
//! a planted link put there. `Scratch::new` makes a folder that's new — `mkdir`, never `create_dir_all` over
//! what's there: 0700, and the next number when its name is taken (`EEXIST`, a link too) — so it removes only
//! what it made, when it's dropped.

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

/// A folder of the test's own under the temp folder, removed (with what's in it) when dropped.
#[derive(Debug)]
pub struct Scratch {
    path: PathBuf,
}

/// Names tried at most before giving up (each taken one is another's).
const TRIES: usize = 1000;

impl Scratch {
    /// `tarae-<name>-<pid>-<n>` made new in the temp folder, by its real path (macOS: `/private/var/…`).
    pub fn new(name: &str) -> Scratch {
        Scratch::in_dir(&std::env::temp_dir(), name)
    }

    /// `new` in `dir` (a test's own temp folder or a stand-in for one).
    pub fn in_dir(dir: &Path, name: &str) -> Scratch {
        for n in 0..TRIES {
            let path = dir.join(format!("tarae-{name}-{}-{n}", std::process::id()));
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => {
                    let path = std::fs::canonicalize(&path).unwrap_or(path);
                    return Scratch { path };
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("a scratch folder in {}: {e}", dir.display()),
            }
        }
        panic!("no free scratch folder name in {} after {TRIES}", dir.display())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Its path, no longer removed when dropped — for a fixture that removes it itself (its own `Drop`), or a
    /// helper whose caller does: made new all the same.
    pub fn into_path(self) -> PathBuf {
        let me = std::mem::ManuallyDrop::new(self);
        me.path.clone()
    }

    /// A path in it.
    pub fn join(&self, rel: impl AsRef<Path>) -> PathBuf {
        self.path.join(rel)
    }
}

impl std::ops::Deref for Scratch {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<Path> for Scratch {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.path).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Made new, 0700, by its real path, each its own; a name taken — a folder another made there, a link — is
    /// passed over and left as it was; dropped, only what it made goes.
    #[test]
    fn a_scratch_folder_is_new_its_own_and_goes_when_dropped() {
        let outer = Scratch::new("scratch-outer");
        // (the first names taken: a folder of another's with a file in it, a link to it)
        let pid = std::process::id();
        let (theirs, link) = (outer.join(format!("tarae-t-{pid}-0")), outer.join(format!("tarae-t-{pid}-1")));
        std::fs::create_dir(&theirs).unwrap();
        std::fs::write(theirs.join("keep"), "theirs").unwrap();
        std::os::unix::fs::symlink(&theirs, &link).unwrap();
        let a = Scratch::in_dir(&outer, "t");
        let b = Scratch::in_dir(&outer, "t");
        assert_eq!(a.path(), outer.join(format!("tarae-t-{pid}-2")), "the taken ones passed over");
        assert_eq!(b.path(), outer.join(format!("tarae-t-{pid}-3")), "each its own");
        assert!(a.is_dir() && !a.is_symlink());
        let mode = std::fs::metadata(&a).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        std::fs::write(a.join("x"), "mine").unwrap();
        let (a_path, b_path) = (a.to_path_buf(), b.to_path_buf());
        drop((a, b));
        assert!(!a_path.exists() && !b_path.exists(), "gone when dropped");
        assert_eq!(std::fs::read_to_string(theirs.join("keep")).unwrap(), "theirs", "never another's");
        assert!(link.is_symlink());
    }
}
