//! The one way tarae runs git. A repository is resolved once from a root tarae trusts — a session's root, a
//! checkout it made, the top git named for one — and kept; every git after runs on it with its git folder and
//! work tree given (`--git-dir`, `--work-tree`), never found again by discovery from where the call starts.
//! Discovery from a file's folder took a subfolder holding `HEAD`, `objects/` and `refs/` for a repository,
//! with that folder's `config` (`target/sb/gitescape.md`); a file's git (the gutter, blame, staging, `:diff`)
//! goes through `for_file` instead: the session's repository, or the submodule of it the file is in as the
//! repository records it. `-c safe.bareRepository=explicit` on every run is the second guard: a bare
//! repository only when named, also for git's own discovery (the resolving, a hook's git).
//!
//! Two forms of a run, for one reason: git exports what `--git-dir`/`--work-tree` name as `GIT_DIR` and
//! `GIT_WORK_TREE` to everything it starts. tarae's own git (`Repo::command`: reads, `NO_HOOKS` steps, the
//! rebase) runs no hooks, so it names the repository. The git-tab actions that run the user's own hooks —
//! commit, pull, push, fetch, switch, stash, branch work (`hooked`, `Repo::hooked_command`) — run from the
//! work tree's top with nothing named, as from a terminal: a hook that runs git in another repository (a nested
//! project's lint-staged, a submodule) would otherwise act on this one. There the top's own `.git` (a
//! protected name — nothing a run of Claude's makes) is what git finds first, `GIT_CEILING_DIRECTORIES` stops
//! the climb above the top, and a bare look-alike is refused.
//!
//! A file in no session's repository (`for_file_in`): its own repository by discovery from its folder — only
//! where no sandboxed run can write (`sandbox::write_places`, the policy's own list), the climb stopped at
//! those places; inside one, none (`NoGit::Writable`, said once). Runs not sandboxed (`llm.sandbox` off, no
//! sandbox here): a run can write anywhere and run git itself, so nothing is held back — discovery from the
//! file's folder wherever it is, still refusing a bare look-alike.
//!
//! Not here: the grammar download's git (`grammar.rs`) — a repository tarae makes itself in its data folder,
//! from a URL in the packs, outside any project.

use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

/// On every git tarae runs: a bare repository is used only when named.
const SAFE: [&str; 2] = ["-c", "safe.bareRepository=explicit"];

/// git's hooks turned off — for what tarae itself runs in a checkout, outside the sandbox (session making,
/// ending and bringing, a review's fetch, the rebase — `rebase::git_line`): a `core.hooksPath` inside a
/// checkout is a folder Claude can write (review, 2026-10-04). The user's own git-tab actions keep their hooks.
pub(crate) const NO_HOOKS: [&str; 2] = ["-c", "core.hooksPath=/dev/null"];

/// Submodules inside submodules followed at most this deep.
const DEPTH_MAX: usize = 8;

/// A repository git runs on, its folders as real paths.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Repo {
    /// The checkout's git folder (a linked checkout's own, under the common one).
    pub git_dir: PathBuf,
    /// The repository's git folder, shared by its checkouts.
    pub common_dir: PathBuf,
    /// The checkout's top — None: a git folder alone (`git_folder`).
    pub work_tree: Option<PathBuf>,
    /// Where its git runs: the root it was resolved from (the work tree, or a folder in it — a session started
    /// in a subfolder keeps `-- .` and `--relative` to that folder), else the git folder.
    pub dir: PathBuf,
}

/// Roots kept at most (the least lately used goes first).
const KEPT_MAX: usize = 64;

/// Each root's repository once resolved, by the root's real path, the last used at the end. A failure isn't
/// kept: a `git init` there later is seen.
struct Kept {
    cap: usize,
    entries: Vec<(PathBuf, Arc<Repo>)>,
}

impl Kept {
    /// `root`'s repository while it still holds: its git folder there, and no `.git` of the root's own that
    /// isn't it (a folder first resolved inside an outer repository, `git init`ed since) — else dropped.
    fn get(&mut self, root: &Path) -> Option<Arc<Repo>> {
        let i = self.entries.iter().position(|(k, _)| k == root)?;
        let r = &self.entries[i].1;
        let own = std::fs::symlink_metadata(root.join(".git")).is_ok();
        if !r.git_dir.is_dir() || (own && r.work_tree.as_deref() != Some(root)) {
            self.entries.remove(i);
            return None;
        }
        let e = self.entries.remove(i);
        let r = e.1.clone();
        self.entries.push(e);
        Some(r)
    }

    fn put(&mut self, root: PathBuf, r: Arc<Repo>) {
        self.entries.retain(|(k, _)| *k != root);
        self.entries.push((root, r));
        let over = self.entries.len().saturating_sub(self.cap);
        self.entries.drain(..over);
    }
}

static REPOS: Mutex<Kept> = Mutex::new(Kept { cap: KEPT_MAX, entries: Vec::new() });

fn repos() -> std::sync::MutexGuard<'static, Kept> {
    REPOS.lock().unwrap_or_else(|e| e.into_inner())
}

/// The repository of `root` — a folder tarae trusts, never a file's folder (`for_file`) — resolved the first
/// time and kept (`Kept`). Runs git the first time: on a worker.
pub fn repo(root: &Path) -> Result<Arc<Repo>, String> {
    let real = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if let Some(r) = repos().get(&real) {
        return Ok(r);
    }
    let r = Arc::new(resolve_with(&real, None)?);
    repos().put(real, r.clone());
    Ok(r)
}

/// `repo` when it was resolved already — for the main loop, which doesn't run git: `root` as given (a
/// session's root is its real path), else not known yet (`repo` on a worker).
pub fn cached(root: &Path) -> Option<Arc<Repo>> {
    repos().get(root)
}

/// Tests: `root`'s repository not kept any more (the next `repo` resolves it again).
#[cfg(test)]
pub fn forget(root: &Path) {
    let real = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    repos().entries.retain(|(k, _)| *k != real);
}

/// The one discovery: from `root`, bare-looking folders refused — and with `ceilings`, never climbing into
/// one of them (`GIT_CEILING_DIRECTORIES`).
fn resolve_with(root: &Path, ceilings: Option<&std::ffi::OsStr>) -> Result<Repo, String> {
    let args = [
        "rev-parse",
        "--path-format=absolute",
        "--git-dir",
        "--git-common-dir",
        "--is-inside-work-tree",
        "--show-cdup",
    ];
    #[cfg(test)]
    not_in_this_repository(root, &args);
    let mut cmd = base(root);
    if let Some(c) = ceilings {
        cmd.env("GIT_CEILING_DIRECTORIES", c);
    }
    cmd.args(SAFE).args(args);
    let out = String::from_utf8_lossy(&finish(&cmd, None)?).into_owned();
    let mut lines = out.lines();
    let (Some(git_dir), Some(common), Some(inside)) = (lines.next(), lines.next(), lines.next()) else {
        return Err(format!("git named no repository for {}", root.display()));
    };
    let real = |p: PathBuf| std::fs::canonicalize(&p).unwrap_or(p);
    let (git_dir, common_dir, here) = (real(git_dir.into()), real(common.into()), real(root.to_path_buf()));
    // (`--show-cdup`: the way up from `root` to the top, empty at the top)
    let work_tree = (inside == "true").then(|| real(here.join(lines.next().unwrap_or_default())));
    let dir = match &work_tree {
        Some(top) if here.starts_with(top) => here,
        Some(top) => top.clone(),
        None => git_dir.clone(),
    };
    Ok(Repo { git_dir, common_dir, work_tree, dir })
}

/// git in `dir`: no prompt, no optional lock (a read — status, diff — never takes the index lock: a commit
/// or stage running then would fail on it).
fn base(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).env("GIT_TERMINAL_PROMPT", "0").env("GIT_OPTIONAL_LOCKS", "0");
    cmd
}

/// What a git run gave.
pub struct Ran {
    pub ok: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Runs `cmd` (a `Repo::command`) with no terminal of its own — ssh or gpg asking for a passphrase can't
/// take ours, it fails instead — and without forking the editor (`spawn::setsid`: a fork costs with the
/// editor's memory), `input` on its stdin, its stdin `/dev/null` else. On a worker.
pub fn ran(cmd: &Command, input: Option<&[u8]>) -> Result<Ran, String> {
    use std::io::{Read as _, Write as _};
    let said = |e: std::io::Error| format!("git: {e}");
    let (s, feed) = match input {
        Some(bytes) => {
            let (s, mut w) = crate::spawn::setsid_fed(cmd).map_err(said)?;
            let bytes = bytes.to_vec();
            // (from a thread of its own: a big answer can't block the asking)
            let feed = crate::event::thread("tarae-git-feed", move || {
                let _ = w.write_all(&bytes); // (then closed: the end of its input)
            });
            (s, Some(feed))
        }
        None => (crate::spawn::setsid(cmd).map_err(said)?, None),
    };
    // (stderr read beside stdout: a full pipe of warnings can't hold git up)
    let errs = s.stderr.map(|mut e| {
        crate::event::thread("tarae-git-err", move || {
            let mut v = Vec::new();
            let _ = e.read_to_end(&mut v);
            v
        })
    });
    let mut stdout = Vec::new();
    let read = s.stdout.map_or(Ok(0), |mut o| o.read_to_end(&mut stdout));
    let stderr = errs.and_then(|t| t.join().ok()).unwrap_or_default();
    let status = crate::spawn::wait(s.pid).map_err(said)?;
    if let Some(f) = feed {
        let _ = f.join();
    }
    read.map_err(said)?;
    Ok(Ran { ok: status.success(), stdout, stderr })
}

/// `ran`: its stdout, else the first line it said why.
pub fn finish(cmd: &Command, input: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let r = ran(cmd, input)?;
    if !r.ok {
        let err = String::from_utf8_lossy(&r.stderr);
        return Err(err.lines().find(|l| !l.trim().is_empty()).unwrap_or("git failed").to_string());
    }
    Ok(r.stdout)
}

impl Repo {
    /// A git folder named — the common one a checkout's `repo` gave, for what outlives the checkout (its
    /// removal, its branch's): no work tree, nothing discovered.
    pub fn git_folder(dir: &Path) -> Repo {
        let d = dir.to_path_buf();
        Repo { git_dir: d.clone(), common_dir: d.clone(), work_tree: None, dir: d }
    }

    /// git on this repository: its folders given, run in `dir` — to run with `ran` or `finish` (an
    /// environment variable added first, say).
    pub fn command(&self, args: &[&str]) -> Command {
        #[cfg(test)]
        not_in_this_repository(&self.dir, args);
        let mut cmd = base(&self.dir);
        cmd.arg("--git-dir").arg(&self.git_dir);
        if let Some(top) = &self.work_tree {
            cmd.arg("--work-tree").arg(top);
        }
        cmd.args(SAFE).args(args);
        cmd
    }

    /// git for an action that runs the user's own hooks (`hooked`): from the work tree's top, the repository
    /// found there — the top's own `.git` comes first — not named; the climb stopped at the top
    /// (`GIT_CEILING_DIRECTORIES` = its parent), a bare look-alike refused. No `GIT_DIR` or `GIT_WORK_TREE`
    /// from tarae's own environment either.
    pub fn hooked_command(&self, args: &[&str]) -> Result<Command, String> {
        let top = self.work_tree.as_deref().ok_or("not a checkout")?;
        #[cfg(test)]
        not_in_this_repository(top, args);
        let mut cmd = base(top);
        if let Some(parent) = top.parent() {
            cmd.env("GIT_CEILING_DIRECTORIES", parent);
        }
        cmd.env_remove("GIT_DIR").env_remove("GIT_WORK_TREE").args(SAFE).args(args);
        Ok(cmd)
    }

    /// Its stdout as it came.
    pub fn output(&self, args: &[&str], input: Option<&str>) -> Result<Vec<u8>, String> {
        finish(&self.command(args), input.map(str::as_bytes))
    }

    /// Its stdout as text.
    pub fn run(&self, args: &[&str], input: Option<&str>) -> Result<String, String> {
        self.output(args, input).map(|b| String::from_utf8_lossy(&b).into_owned())
    }

    /// `run` with `NO_HOOKS`.
    pub fn run_no_hooks(&self, args: &[&str]) -> Result<String, String> {
        let all: Vec<&str> = NO_HOOKS.iter().chain(args).copied().collect();
        self.run(&all, None)
    }

    /// The start of a shell line that runs git on it (the rebase's, in the task panel).
    pub fn shell_line(&self) -> String {
        let q = |p: &Path| crate::packs::sh_quote(&p.to_string_lossy());
        let mut line = format!("git --git-dir {}", q(&self.git_dir));
        if let Some(top) = &self.work_tree {
            line += &format!(" --work-tree {}", q(top));
        }
        line + " " + &SAFE.join(" ")
    }
}

/// git on `root`'s repository (`repo`) — the call most of tarae makes.
pub(crate) fn git(root: &Path, args: &[&str], input: Option<&str>) -> Result<String, String> {
    repo(root)?.run(args, input)
}

/// git for a git-tab action that runs the user's own hooks — commit, amend, push, pull, fetch, switch, stash,
/// branch work (`hooks::with`'s): `Repo::hooked_command` — the hooks get no `GIT_DIR`/`GIT_WORK_TREE` (see the
/// module's doc).
pub(crate) fn hooked(root: &Path, args: &[&str]) -> Result<String, String> {
    let cmd = repo(root)?.hooked_command(args)?;
    finish(&cmd, None).map(|b| String::from_utf8_lossy(&b).into_owned())
}

/// `git` with `NO_HOOKS`.
pub(crate) fn git_no_hooks(root: &Path, args: &[&str]) -> Result<String, String> {
    repo(root)?.run_no_hooks(args)
}

/// The repository a file's git runs on, and the file's path from its work tree (git runs at the top): `root`'s
/// (a session's root, never the file's folder), or the submodule of it the file is in. None: outside its
/// work tree, or no repository.
pub fn for_file(root: &Path, path: &Path) -> Option<(Arc<Repo>, String)> {
    let r = repo(root).ok()?;
    let top = r.work_tree.clone()?;
    let real;
    let rel = match path.strip_prefix(&top) {
        Ok(rel) => rel,
        Err(_) => {
            real = std::fs::canonicalize(path).ok()?;
            real.strip_prefix(&top).ok()?
        }
    };
    let at_top = if r.dir == top { r.clone() } else { Arc::new(Repo { dir: top, ..(*r).clone() }) };
    within(at_top, rel, 0)
}

/// Why a file gets no git.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoGit {
    /// In no repository tarae reads.
    None,
    /// In no session's repository, where a sandboxed run can write: git isn't read there. `looks`: something
    /// there passes for a repository (a `.git`; `HEAD`, `objects/` and `refs/`) — the user is told why.
    Writable { looks: bool },
}

/// What's said — the status line, once per file (the gutter), or as the error of what was asked — when git
/// isn't read for a file (`NoGit::Writable`).
pub const NOT_READ: &str =
    "not in this session's repository — tarae doesn't read a git folder from where commands can write";

impl NoGit {
    /// Why, in words.
    pub fn said(self) -> &'static str {
        match self {
            NoGit::None => "not in a git repository",
            NoGit::Writable { .. } => NOT_READ,
        }
    }
}

/// Where a sandboxed run may write (`sandbox::write_places`, each session's) — None: runs aren't sandboxed (one
/// can write anywhere and run git itself, so nothing is held back: a file's own repository is found from its
/// folder). Asked on a worker, only for a file in no session's repository.
pub type Writes = Arc<dyn Fn() -> Option<Vec<PathBuf>> + Send + Sync>;

/// Where a file's git is looked for — made on the main loop (`Editor::git_places`), used on a worker.
#[derive(Clone)]
pub struct Places {
    /// The sessions' roots, the one whose checkout holds the file first.
    pub roots: Vec<PathBuf>,
    pub writes: Writes,
}

impl Places {
    /// Tests: `root`'s repository — and runs not sandboxed.
    #[cfg(test)]
    pub fn only(root: &Path) -> Places {
        Places { roots: vec![root.to_path_buf()], writes: Arc::new(|| None) }
    }
}

/// The repository a file's git runs on, and the file's path from its top (`for_file`): a session's — else its
/// own, found from its folder (a bare look-alike refused), only when no sandboxed run can write that folder
/// (`Places::writes`) nor where its repository keeps its git folder; the climb stops at those places
/// (`GIT_CEILING_DIRECTORIES`). Runs not sandboxed: found from its folder wherever it is. On a worker.
pub fn for_file_in(places: &Places, path: &Path) -> Result<(Arc<Repo>, String), NoGit> {
    if let Some(found) = places.roots.iter().find_map(|root| for_file(root, path)) {
        return Ok(found);
    }
    let real = std::fs::canonicalize(path)
        .or_else(|_| {
            let (dir, name) = (path.parent().ok_or(())?, path.file_name().ok_or(())?);
            std::fs::canonicalize(dir).map(|d| d.join(name)).map_err(|_| ())
        })
        .map_err(|_| NoGit::None)?;
    let folder = real.parent().ok_or(NoGit::None)?;
    // (runs not sandboxed: one can write anywhere and run git itself — holding git back protects nothing)
    let Some(writes) = (places.writes)() else {
        return within_outside(&real, folder, &[]);
    };
    if let Some(w) = writes.iter().find(|w| folder.starts_with(w)) {
        return Err(NoGit::Writable { looks: looks_like_git(folder, w) });
    }
    within_outside(&real, folder, &writes)
}

/// `real` (in `folder`) in the repository around it, found from there (`outside`) — not one whose git folder
/// is in `writes`.
fn within_outside(real: &Path, folder: &Path, writes: &[PathBuf]) -> Result<(Arc<Repo>, String), NoGit> {
    let repo = outside(folder, writes).ok_or(NoGit::None)?;
    let writable = |p: &Path| writes.iter().any(|w| p.starts_with(w));
    if writable(&repo.git_dir) || writable(&repo.common_dir) {
        return Err(NoGit::Writable { looks: true });
    }
    let top = repo.work_tree.clone().ok_or(NoGit::None)?;
    let rel = real.strip_prefix(&top).map_err(|_| NoGit::None)?;
    let at_top = Arc::new(Repo { dir: top.clone(), ..(*repo).clone() });
    within(at_top, rel, 0).ok_or(NoGit::None)
}

/// The repository around `folder` (outside every write place), kept as a root's is (`Kept`); the climb
/// stopped at `writes` — none of them is looked in.
fn outside(folder: &Path, writes: &[PathBuf]) -> Option<Arc<Repo>> {
    if let Some(r) = repos().get(folder) {
        return Some(r);
    }
    let ceilings = std::env::join_paths(writes.iter().filter(|w| w.is_absolute())).ok()?;
    let r = Arc::new(resolve_with(folder, (!writes.is_empty()).then_some(ceilings.as_os_str())).ok()?);
    repos().put(folder.to_path_buf(), r.clone());
    Some(r)
}

/// Whether something from `folder` up to `top` passes for a git repository: a `.git`, or a git folder's own
/// layout (`HEAD`, `objects/`, `refs/`) — files only looked at, no git run.
fn looks_like_git(folder: &Path, top: &Path) -> bool {
    folder.ancestors().take_while(|d| d.starts_with(top)).any(|d| {
        std::fs::symlink_metadata(d.join(".git")).is_ok()
            || (d.join("HEAD").is_file() && d.join("objects").is_dir() && d.join("refs").is_dir())
    })
}

/// `rel` in `repo`'s work tree — down into a submodule on the way when there's one.
fn within(repo: Arc<Repo>, rel: &Path, depth: usize) -> Option<(Arc<Repo>, String)> {
    let top = repo.work_tree.clone()?;
    let parts: Vec<&std::ffi::OsStr> = rel
        .components()
        .map(|c| match c {
            Component::Normal(p) => Some(p),
            _ => None,
        })
        .collect::<Option<_>>()?;
    if depth < DEPTH_MAX {
        let mut at = top;
        for i in 0..parts.len().saturating_sub(1) {
            at.push(parts[i]);
            // (a folder with a `.git` in it: asked of the index — a stat each on the way down otherwise)
            if std::fs::symlink_metadata(at.join(".git")).is_ok() {
                let here: PathBuf = parts[..=i].iter().collect();
                if let Some(sub) = submodule(&repo, &at, here.to_str()?) {
                    let rest: PathBuf = parts[i + 1..].iter().collect();
                    return within(Arc::new(sub), &rest, depth + 1);
                }
            }
        }
    }
    Some((repo, rel.to_str()?.to_string()))
}

/// The submodule checked out at `at` (`rel` from `repo`'s top), as `repo` records it: a gitlink in its index
/// there, and a git folder under its own `modules/` — the one the checkout's `.git` names, taken only there.
fn submodule(repo: &Repo, at: &Path, rel: &str) -> Option<Repo> {
    let spec = format!(":(top,literal){rel}");
    let listed = repo.run(&["ls-files", "--stage", "-z", "--", &spec], None).ok()?;
    // (`160000 <id> 0\t<path>`)
    let gitlink = listed
        .split('\0')
        .any(|e| e.starts_with("160000 ") && e.split_once('\t').is_some_and(|(_, p)| p == rel));
    if !gitlink {
        return None;
    }
    let named = std::fs::read_to_string(at.join(".git")).ok()?;
    let git_dir =
        std::fs::canonicalize(at.join(named.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim())).ok()?;
    let modules = std::fs::canonicalize(repo.common_dir.join("modules")).ok()?;
    if git_dir == modules || !git_dir.starts_with(&modules) || !git_dir.is_dir() {
        return None;
    }
    let top = std::fs::canonicalize(at).ok()?;
    Some(Repo { common_dir: git_dir.clone(), git_dir, work_tree: Some(top.clone()), dir: top })
}

/// Tests only: whether git run in `dir` would work on tarae's own repository (the nearest `.git` up from it
/// is this crate's).
#[cfg(test)]
fn in_this_repository(dir: &Path) -> bool {
    let own = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    let dir = dir.ancestors().find_map(|d| std::fs::canonicalize(d).ok()).unwrap_or_default();
    dir.ancestors().find(|d| d.join(".git").exists()) == Some(own.as_path())
}

/// Tests only: no test runs git in tarae's own repository — even a read can write there (`git diff`
/// refreshes the index), and a sandboxed run reports it as a denial. One that does stops the whole run,
/// naming the command and where it came from (past libtest's capture: an abort loses what it captured).
#[cfg(test)]
pub(crate) fn not_in_this_repository(dir: &Path, args: &[&str]) {
    if in_this_repository(dir) {
        // (a job's worker is named after the test it ran for — `event::run`)
        let test = std::thread::current().name().unwrap_or("a test").to_string();
        let line = format!(
            "{test} ran `git {}` in tarae's own repository ({}) — give it a repository of its own in its \
             temp folder\n{}\n",
            args.join(" "),
            dir.display(),
            std::backtrace::Backtrace::force_capture()
        );
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        std::process::abort();
    }
}

#[cfg(test)]
mod tests;
