//! A check run on a commit's tree (#212, `run`'s `base`): "is this failure already there on HEAD, or mine?"
//! A session's Claude exported `git archive HEAD` into `/tmp` and its build was refused — cargo's build folder
//! for that copy was no checkout's own. Here tarae writes the commit's tree (`export`) into a folder of its own
//! per project — `<tarae's cache>/base/<project key>/tree`, beside the runs' own folders and out of every place
//! a run may write — when the run's turn comes (`tools::guard_then_go`), and the run's sandbox is made for that
//! tree as its project (`policy_from`): it writes there only, never the session's checkout, and its build tool's
//! own folder for that tree opens as any checkout's does. Its path stays the same, so the build folder is the same
//! from one run to the next. The tree goes when the run ends (`tools::run_done`); the build folder stays, for the
//! next one.
//!
//! Only the project's checks run there (`tools::allowed` — a build, the tests), one command, nothing piped: what
//! else a line does is judged against the session's checkout, and this isn't it. tarae's own git, no hooks, a
//! temporary index of its own beside the tree (`git read-tree`, `git checkout-index --prefix`): the session's
//! index and working tree aren't touched. Submodules come as empty folders.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// A run's commit: the rev Claude named, the tree it's written to, the commit it named once `export` found it,
/// and the tree's claim from then (`Claim`) — held while the run has it, through its removal after.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Base {
    pub rev: String,
    pub tree: PathBuf,
    pub sha: Option<String>,
    pub claim: Option<Arc<Claim>>,
}

/// The trees a run has (review of #212: the tree is the project's, not the session's — two sessions of one
/// checkout, or a run asked while the last one's tree is still being removed, wrote and removed each other's).
static CLAIMED: (Mutex<Vec<PathBuf>>, Condvar) = (Mutex::new(Vec::new()), Condvar::new());

/// How long writing a tree waits for another run's to let it go — the last one's removal, say — before it says
/// another run has it.
const CLAIM_WAIT: Duration = Duration::from_secs(30);

/// One run's hold on a tree — from before it's written until the run is over and the tree removed: let go when
/// the last of it is dropped, however the run ended (or never started).
#[derive(Debug, PartialEq, Eq)]
pub struct Claim {
    tree: PathBuf,
}

impl Claim {
    /// `tree`, once no other run has it — waiting at most `wait` (a worker's). Err: another run has it still.
    pub fn take(tree: &Path, wait: Duration) -> Result<Claim, String> {
        let (lock, freed) = &CLAIMED;
        let held = lock.lock().unwrap_or_else(|e| e.into_inner());
        let (mut held, timeout) = freed
            .wait_timeout_while(held, wait, |h| h.iter().any(|t| t == tree))
            .unwrap_or_else(|e| e.into_inner());
        if timeout.timed_out() && held.iter().any(|t| t == tree) {
            return Err(
                "another run on this project's commit tree is under way (this session's or another's) — ask \
                 again once it's done"
                    .into(),
            );
        }
        held.push(tree.to_path_buf());
        Ok(Claim { tree: tree.to_path_buf() })
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        let (lock, freed) = &CLAIMED;
        lock.lock().unwrap_or_else(|e| e.into_inner()).retain(|t| *t != self.tree);
        freed.notify_all();
    }
}

impl Base {
    /// How a report says it: `HEAD (be2fc5d)`.
    pub fn said(&self) -> String {
        match &self.sha {
            Some(sha) if !self.rev.starts_with(&sha[..sha.len().min(7)]) => {
                format!("{} ({})", self.rev, &sha[..sha.len().min(7)])
            }
            _ => self.rev.clone(),
        }
    }
}

/// The folder of the project at `root`'s commit trees: `base`'s (a test's), else tarae's cache's `base`.
pub fn folder_for(root: &Path, base: Option<&Path>) -> Option<PathBuf> {
    let base = base.map(Path::to_path_buf).or_else(|| crate::runtime::cache_dir().map(|c| c.join("base")))?;
    Some(base.join(crate::sandbox::project_key(root)))
}

/// Whether `rev` is one this takes: a name, a sha, `HEAD~2`, `main^` — not an option, no spaces or control
/// characters.
pub fn rev_ok(rev: &str) -> bool {
    !rev.is_empty() && !rev.starts_with('-') && !rev.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// `b.rev`'s tree, written fresh to `b.tree` from the repository of the checkout at `root` — what was there
/// before removed first (a link there removed as a link, nothing through it) — once no other run has the tree
/// (`Claim`). The commit's id and the claim, for the run to hold. Blocks (a worker's); on failure nothing is
/// left.
pub fn export(root: &Path, b: &Base) -> Result<(String, Arc<Claim>), String> {
    export_waiting(root, b, CLAIM_WAIT)
}

/// `export`, waiting at most `wait` for the tree.
fn export_waiting(root: &Path, b: &Base, wait: Duration) -> Result<(String, Arc<Claim>), String> {
    if !rev_ok(&b.rev) {
        return Err(format!("base {}: not a commit's name", b.rev));
    }
    let claim = Arc::new(Claim::take(&b.tree, wait).map_err(|e| format!("base {}: {e}", b.rev))?);
    export_claimed(root, b).map(|sha| (sha, claim))
}

/// `export` with the tree claimed.
fn export_claimed(root: &Path, b: &Base) -> Result<String, String> {
    let arg = format!("{}^{{commit}}", b.rev);
    let sha =
        crate::snapshot::git(root, &["rev-parse", "--verify", "--quiet", "--end-of-options", &arg], None)
            .map(|s| s.trim().to_string())
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("base {}: no such commit in this repository", b.rev))?;
    let folder = b.tree.parent().ok_or("no folder for the tree")?;
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(folder)
            .map_err(|e| format!("{}: {e}", folder.display()))?;
    }
    remove(&b.tree).map_err(|e| format!("{}: {e}", b.tree.display()))?;
    let index = folder.join("tree.index");
    let written = (|| {
        crate::snapshot::git(root, &["read-tree", &sha], Some(&index))?;
        let prefix = format!("--prefix={}/", b.tree.display());
        crate::snapshot::git(root, &["checkout-index", "--all", "--force", &prefix], Some(&index))
    })();
    let _ = std::fs::remove_file(&index);
    match written {
        Ok(_) if b.tree.is_dir() => Ok(sha),
        Ok(_) => {
            std::fs::create_dir(&b.tree).map_err(|e| format!("{}: {e}", b.tree.display()))?;
            Ok(sha)
        }
        Err(e) => {
            let _ = remove(&b.tree);
            Err(format!("base {}: its tree couldn't be written: {e}", b.rev))
        }
    }
}

/// `tree` gone — a folder with all in it (links in it removed as links, never gone through), a link or file
/// removed as itself; nothing there is fine.
pub fn remove(tree: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(tree) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(tree),
        Ok(_) => std::fs::remove_file(tree),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
pub(crate) mod tests;
