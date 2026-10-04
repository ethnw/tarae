//! Sessions' checkouts: work beside the rest — a branch of its own in a checkout of its own (a git
//! worktree), opened as a session (`session.rs`). `space W n` · `:session-new <name> [<base>]` makes one:
//! the name is the branch, made from the front session's HEAD (`C-b` in the prompt picks another base); a
//! card says where its checkout goes and what it costs on disk before anything is made. `:session-end`
//! removes one — at once when nothing would be lost (no changes, merged into its base), else a card says
//! what would be. `:session-bring` moves its base up to it, fast-forward only. The list (`space W`). The
//! sessions are kept per repository (`$state/sessions/<hash of its git folder>.json`) and come back at
//! startup and after `:restart`.
//!
//! Git runs on workers whose results land in the session in front (`Queue::shared_jobs`) — never swapped
//! into the session they're about, which may be gone by then.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::editor::Editor;
use crate::event::SessionId;
use crate::gitmenu::git;
use crate::gitpanel::{branch_name, commit_rows, current_branch, only_on, parse_log, top_of};

/// How long sizes on disk are measured for, at most (then the card says "over").
const MEASURE_FOR: Duration = Duration::from_millis(1500);
/// An ignored folder this big is on the create card · at most this many of them.
const BIG: u64 = 20 << 20;
const BIG_SHOWN: usize = 3;
/// How long `cargo metadata` gets to say where builds go.
const CARGO_WAIT: Duration = Duration::from_secs(3);
/// Changed files listed on the end card (the rest counted).
const FILES_SHOWN: usize = 6;

#[derive(Default)]
pub struct SessionStore {
    /// Where the sessions are kept (`$state/sessions`) — None in tests (they set a temp folder).
    pub dir: Option<PathBuf>,
    /// The kept sessions are back (or there were none): from then on a change is written.
    pub restored: bool,
    /// A key was pressed since startup — sessions coming back then stay behind (the front is where you are).
    pub keyed: bool,
    /// What git last said of each session for the list (read when it opens — shown meanwhile).
    pub facts: std::collections::HashMap<SessionId, Facts>,
}

/// A session as it's kept: its checkout, name, branch and base (`Session`'s).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Kept {
    pub root: PathBuf,
    pub name: String,
    pub branch: Option<String>,
    pub base: Option<String>,
}

impl Kept {
    pub fn of(l: &crate::session::Session) -> Kept {
        Kept { root: l.root.clone(), name: l.name.clone(), branch: l.branch.clone(), base: l.base.clone() }
    }

    fn json(&self) -> Value {
        json!({ "root": self.root, "name": self.name, "branch": self.branch, "base": self.base })
    }

    fn from_json(v: &Value) -> Option<Kept> {
        Some(Kept {
            root: PathBuf::from(v["root"].as_str()?),
            name: v["name"].as_str()?.to_string(),
            branch: v["branch"].as_str().map(str::to_string),
            base: v["base"].as_str().map(str::to_string),
        })
    }
}

// ── Disk ─────────────────────────────────────────────────────────────────────

/// What `p` takes on disk (allocated blocks; links not followed) — and whether all of it was counted
/// before `until`.
pub fn disk_size(p: &Path, until: Instant) -> (u64, bool) {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(m) = std::fs::symlink_metadata(p) else { return (0, true) };
    let mut total = m.blocks() * 512;
    if !m.is_dir() {
        return (total, true);
    }
    let mut stack = vec![p.to_path_buf()];
    let mut seen = 0u32;
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen.is_multiple_of(256) && Instant::now() >= until {
                return (total, false);
            }
            // (`DirEntry::metadata` doesn't follow a link)
            let Ok(m) = e.metadata() else { continue };
            total += m.blocks() * 512;
            if m.is_dir() {
                stack.push(e.path());
            }
        }
    }
    (total, true)
}

/// `2.3 GB` · `over 1.2 GB` when counting stopped first.
fn size_text((bytes, whole): (u64, bool)) -> String {
    match whole {
        true => crate::procs::mem(bytes),
        false => format!("over {}", crate::procs::mem(bytes)),
    }
}

/// Where cargo puts builds for the project in `top`: its target folder and build-dir (`cargo metadata`,
/// which reads every cargo config as it is) — None when it doesn't say within `CARGO_WAIT`.
fn cargo_dirs(top: &Path) -> Option<(PathBuf, PathBuf)> {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new("cargo")
        .args(["metadata", "--no-deps", "--offline", "--format-version", "1"])
        .current_dir(top)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut out = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut s = Vec::new();
        let _ = out.read_to_end(&mut s);
        s
    });
    let end = Instant::now() + CARGO_WAIT;
    loop {
        match child.try_wait() {
            Ok(Some(s)) if s.success() => break,
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let v: Value = serde_json::from_slice(&reader.join().ok()?).ok()?;
    let dir = |k: &str| v[k].as_str().map(|d| std::fs::canonicalize(d).unwrap_or_else(|_| PathBuf::from(d)));
    let target = dir("target_directory")?;
    let build = dir("build_directory").unwrap_or_else(|| target.clone());
    Some((target, build))
}

// ── Git ──────────────────────────────────────────────────────────────────────

/// The commit `rev` names (full sha).
fn sha(dir: &Path, rev: &str) -> Result<String, String> {
    git(dir, &["rev-parse", "--verify", "-q", rev], None).map(|s| s.trim().to_string())
}

/// The repository's git folder (shared by all its worktrees), real path.
pub fn common_dir(dir: &Path) -> Result<PathBuf, String> {
    let out = git(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"], None)?;
    let p = PathBuf::from(out.trim());
    Ok(std::fs::canonicalize(&p).unwrap_or(p))
}

/// Whether `a` is in `b`'s history.
fn is_ancestor(dir: &Path, a: &str, b: &str) -> bool {
    git(dir, &["merge-base", "--is-ancestor", a, b], None).is_ok()
}

fn count(dir: &Path, range: &str) -> usize {
    git(dir, &["rev-list", "--count", range], None).ok().and_then(|n| n.trim().parse().ok()).unwrap_or(0)
}

fn commits(n: usize) -> String {
    if n == 1 { "1 commit".into() } else { format!("{n} commits") }
}

/// A local branch's name as given, else an error saying it isn't one.
fn local_branch(dir: &Path, b: &str) -> Result<String, String> {
    if b.starts_with('-') {
        return Err(format!("{b:?} isn't a branch"));
    }
    sha(dir, &format!("refs/heads/{b}")).map_err(|_| format!("{b} isn't a branch here"))
}

/// The base a session compares with: its own, else the branch of the checkout tarae started in.
fn base_or_start(base: Option<String>, start: &Path) -> Result<String, String> {
    base.or_else(|| current_branch(start))
        .ok_or_else(|| "no base to compare with — the checkout tarae started in is on no branch".into())
}

// ── Where checkouts go ───────────────────────────────────────────────────────

/// `session.worktrees` ("" = `$XDG_DATA_HOME/tarae/worktrees`, `~/` from home) + a folder for the
/// repository whose git folder is `common`.
pub fn worktrees_dir(setting: &str, common: &Path) -> Result<PathBuf, String> {
    let s = setting.trim();
    let base = if s.is_empty() {
        crate::runtime::data_dir()
            .ok_or("no folder for sessions' checkouts — set session.worktrees")?
            .join("worktrees")
    } else if let Some(rest) = s.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME").ok_or("no HOME for session.worktrees' ~")?).join(rest)
    } else {
        PathBuf::from(s)
    };
    if !base.is_absolute() {
        return Err(format!("session.worktrees is {s:?} — a full path or ~/… is needed"));
    }
    Ok(base.join(repo_folder(common)))
}

/// `tarae-1a2b3c4d`: the repository's folder name and a hash of its git folder — two clones named alike
/// keep apart.
fn repo_folder(common: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let named = if common.file_name() == Some(".git".as_ref()) { common.parent() } else { Some(common) };
    let name = named
        .and_then(Path::file_name)
        .map_or_else(|| "repo".to_string(), |n| n.to_string_lossy().trim_end_matches(".git").to_string());
    let h = common
        .as_os_str()
        .as_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3));
    format!("{name}-{:08x}", h as u32)
}

// ── Making one ───────────────────────────────────────────────────────────────

/// What the create card shows, and `y` makes.
#[derive(Clone, Debug)]
pub struct Making {
    /// The checkout it's made from (its top).
    pub top: PathBuf,
    pub branch: String,
    /// The base as stored and said: a branch, or the commit when the checkout was detached · its commit.
    pub base: String,
    pub base_sha: String,
    /// Where its checkout goes.
    pub path: PathBuf,
    /// The checkout's own files on disk · the big ignored folders here (not copied), biggest first ·
    /// where cargo builds (target folder, build-dir), if it's a cargo project.
    pub files: (u64, bool),
    pub big: Vec<(String, (u64, bool))>,
    pub cargo: Option<(PathBuf, PathBuf)>,
}

impl Making {
    /// `main (a1b2c3d)` · `a1b2c3d` for a commit.
    fn base_said(&self) -> String {
        match self.base == self.base_sha {
            true => self.base_sha[..7].to_string(),
            false => format!("{} ({})", self.base, &self.base_sha[..7]),
        }
    }

    pub fn card(&self) -> crate::offer::Offer {
        // (the folder and where it is apart: a path wraps only at its space, never mid-name)
        let (folder, under) = match (self.path.file_name(), self.path.parent()) {
            (Some(f), Some(p)) => (f.to_string_lossy().into_owned(), crate::allow::tilde(p)),
            _ => (crate::allow::tilde(&self.path), String::new()),
        };
        let mut q = format!(
            "Branch {} from {}.\nIts checkout, {folder}/, goes in\n  {under}\n\nOn disk: {} of files.",
            self.branch,
            self.base_said(),
            size_text(self.files)
        );
        match self.big.is_empty() {
            true => q += " Nothing big is ignored here.",
            false => q += " Ignored here, so not copied:",
        }
        let top = &self.top;
        let shared = |d: &Path| !d.starts_with(top);
        // Sizes right-aligned in a column, each folder after its size
        let sizes: Vec<String> = self.big.iter().map(|(_, s)| size_text(*s)).collect();
        let w = sizes.iter().map(String::len).max().unwrap_or(0);
        for ((p, _), size) in self.big.iter().zip(&sizes) {
            let builds = self
                .cargo
                .as_ref()
                .is_some_and(|(t, b)| top.join(p.trim_end_matches('/')) == *t && !shared(b));
            let note = if builds { " — a session builds its own" } else { "" };
            q += &format!("\n  {size:>w$}  {p}{note}");
        }
        match &self.cargo {
            Some((target, _)) if shared(target) => {
                q += &format!(
                    "\nCargo builds into {} (your config): shared, one build at a time.",
                    crate::allow::tilde(target)
                )
            }
            Some((_, build)) if shared(build) => {
                q += &format!(
                    "\nCargo's build-dir is shared ({}): sessions reuse its builds, one build at a time.",
                    crate::allow::tilde(build)
                )
            }
            _ => {}
        }
        crate::offer::Offer::new(
            format!("New session: {}", self.branch),
            &q,
            "Making its checkout…",
            "Couldn't make the session".into(),
            crate::offer::What::SessionNew(Box::new(self.clone())),
        )
    }
}

/// Looked at before the card: the name as a branch (new), the base, where the checkout goes and what it
/// costs (on a worker).
pub fn making(root: &Path, name: &str, base: Option<&str>, setting: &str) -> Result<Making, String> {
    let top = top_of(root)?;
    let branch = branch_name(&top, name.trim())?;
    if sha(&top, &format!("refs/heads/{branch}")).is_ok() {
        let list = git(&top, &["worktree", "list", "--porcelain"], None).unwrap_or_default();
        let held = crate::gitpanel::parse_worktrees(&list)
            .into_iter()
            .find(|(_, b)| b.as_deref() == Some(branch.as_str()));
        return Err(match held {
            Some((at, _)) => format!(
                "there's a branch {branch} already, checked out in {} — pick another name",
                crate::gitpanel::worktree_shown(&top, &std::fs::canonicalize(&at).unwrap_or(at.clone()))
            ),
            None => format!("there's a branch {branch} already — pick another name"),
        });
    }
    let (base, base_sha) = match base {
        Some(b) if b.starts_with('-') => return Err(format!("{b:?} isn't a commit")),
        Some(b) => {
            let s = sha(&top, &format!("{b}^{{commit}}")).map_err(|_| format!("there's no {b} here"))?;
            // (a branch is kept by name — `:session-bring` moves it; anything else is the commit)
            let named = sha(&top, &format!("refs/heads/{b}")).is_ok();
            (if named { b.to_string() } else { s.clone() }, s)
        }
        None => {
            let s = sha(&top, "HEAD")?;
            (current_branch(&top).unwrap_or_else(|| s.clone()), s)
        }
    };
    let common = common_dir(&top)?;
    let path = worktrees_dir(setting, &common)?.join(branch.replace('/', "-"));
    if path.exists() {
        return Err(format!("{} is there already", crate::allow::tilde(&path)));
    }
    let until = Instant::now() + MEASURE_FOR;
    let big = big_ignored(&top, until);
    let files = tracked_size(&top, until);
    // (where cargo builds, from the user's cargo config as it is)
    let cargo = top.join("Cargo.toml").is_file().then(|| cargo_dirs(&top)).flatten();
    Ok(Making { top, branch, base, base_sha, path, files, big, cargo })
}

/// The tracked files' size on disk — what a new checkout writes.
fn tracked_size(top: &Path, until: Instant) -> (u64, bool) {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(out) = git(top, &["ls-files", "-z"], None) else { return (0, false) };
    let mut total = 0;
    for (i, f) in out.split('\0').filter(|f| !f.is_empty()).enumerate() {
        if i % 256 == 255 && Instant::now() >= until {
            return (total, false);
        }
        total += std::fs::symlink_metadata(top.join(f)).map_or(0, |m| m.blocks() * 512);
    }
    (total, true)
}

/// What a session doesn't get from this checkout: its big ignored folders (build output, installed
/// packages), biggest first.
fn big_ignored(top: &Path, until: Instant) -> Vec<(String, (u64, bool))> {
    let out =
        git(top, &["ls-files", "-z", "--others", "--ignored", "--exclude-standard", "--directory"], None)
            .unwrap_or_default();
    let mut found: Vec<&str> = out.split('\0').filter(|p| !p.is_empty()).collect();
    // (folders first: they hold the bulk, and the time is shared)
    found.sort_by_key(|p| !p.ends_with('/'));
    let mut big: Vec<(String, (u64, bool))> = found
        .into_iter()
        .map(|p| (p.to_string(), disk_size(&top.join(p), until)))
        .filter(|(_, s)| s.0 >= BIG)
        .collect();
    big.sort_by_key(|(_, s)| std::cmp::Reverse(s.0));
    big.truncate(BIG_SHOWN);
    big
}

/// `git worktree add -b` for the card's plan (on a worker) — the checkout's real path.
fn make(m: &Making) -> Result<PathBuf, String> {
    if let Some(parent) = m.path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", crate::allow::tilde(parent)))?;
    }
    if m.path.exists() {
        return Err(format!("{} is there already", crate::allow::tilde(&m.path)));
    }
    let path = m.path.to_str().ok_or("a checkout path that isn't UTF-8")?;
    git(&m.top, &["worktree", "add", "-q", "-b", &m.branch, path, &m.base_sha], None)?;
    Ok(std::fs::canonicalize(&m.path).unwrap_or_else(|_| m.path.clone()))
}

// ── Ending one ───────────────────────────────────────────────────────────────

/// What the checks found about a session to end.
#[derive(Clone, Debug)]
pub struct Ending {
    pub root: PathBuf,
    pub common: PathBuf,
    pub name: String,
    /// The branch checked out there now (None: detached) · the sha it's at.
    pub branch: Option<String>,
    pub tip: String,
    pub base: String,
    /// `git status --porcelain` rows (ignored files aren't in it).
    pub changed: Vec<String>,
    /// Commits not in the base · commits on no other branch, remote's branch or tag (gone with the branch).
    pub ahead: Vec<(String, String)>,
    pub lost: Vec<(String, String)>,
}

impl Ending {
    /// Nothing would be lost: no changes, every commit in the base.
    pub fn quiet(&self) -> bool {
        self.changed.is_empty() && self.ahead.is_empty()
    }

    /// Work in no commit would go with its checkout (no reflog brings it back): the card has no `y` —
    /// committing first (`c`, on a branch) leads, removing takes `R`/`D`.
    pub fn dirty(&self) -> bool {
        !self.changed.is_empty()
    }

    pub fn card(&self) -> crate::offer::Offer {
        let mut lines: Vec<String> = Vec::new();
        let mut decided = 0..0;
        let n = self.changed.len();
        if n > 0 {
            lines.push(format!(
                "{}, in no commit yet:",
                if n == 1 { "1 changed file".to_string() } else { format!("{n} changed files") }
            ));
            decided.start = lines.len();
            lines.extend(self.changed.iter().take(FILES_SHOWN).map(|c| format!("  {}", changed_row(c))));
            if n > FILES_SHOWN {
                lines.push(format!("  … and {} more", n - FILES_SHOWN));
            }
            decided.end = lines.len();
        }
        if !self.ahead.is_empty() {
            if decided.is_empty() {
                decided.start = lines.len();
            }
            lines.push(format!("{} not in {}:", commits(self.ahead.len()), self.base));
            lines.extend(commit_rows(&self.ahead).lines().filter(|l| !l.is_empty()).map(str::to_string));
            decided.end = lines.len();
            lines.push("A squash merge on a forge looks unmerged here.".into());
        }
        // What committing first does, then what removing loses — in the warning color where it's for good
        let mut tail: Vec<(String, bool)> = Vec::new();
        if let (true, Some(b)) = (self.dirty(), &self.branch) {
            tail.push((format!("Committing first: Claude drafts it, :w commits on {b}."), false));
        }
        if self.dirty() {
            tail.push(("Removing throws them away — no reflog keeps them.".into(), true));
        }
        match (&self.branch, self.lost.len()) {
            (Some(b), 0) => tail
                .push((format!("Removing both deletes {b} too — its commits are on other branches."), false)),
            (Some(b), lost) => tail.push((
                format!(
                    "Removing both deletes {b} too, and {} on no other branch with it — git's reflog keeps \
                     them a while.",
                    commits(lost)
                ),
                true,
            )),
            (None, _) if !self.ahead.is_empty() => {
                tail.push(("Its checkout is on no branch: those commits go with it.".into(), true))
            }
            (None, _) => {}
        }
        if !tail.is_empty() {
            lines.push(String::new());
        }
        let at = lines.len();
        let warned: Vec<usize> = tail.iter().enumerate().filter(|(_, t)| t.1).map(|(i, _)| at + i).collect();
        // (one range: the warned lines are the last ones, or one alone)
        let warning = warned.first().zip(warned.last()).map(|(a, b)| *a..*b + 1);
        lines.extend(tail.into_iter().map(|t| t.0));
        let mut o = crate::offer::Offer::new(
            format!("End {}?", self.name),
            &lines.join("\n"),
            "Removing its checkout…",
            format!("Couldn't end {}", self.name),
            crate::offer::What::SessionEnd(Box::new(self.clone())),
        )
        .deciding(decided);
        if let Some(w) = warning {
            o = o.warning(w);
        }
        o
    }
}

/// A `git status --porcelain` row in words: `new      notes.md` · `changed  src/a.rs` · `deleted  b.rs`.
fn changed_row(row: &str) -> String {
    let (code, path) = (row.get(..2).unwrap_or_default(), row.get(3..).unwrap_or(row));
    let what = match code {
        "??" => "new",
        c if c.contains('D') => "deleted",
        c if c.contains('R') => "renamed",
        c if c.contains('A') => "new",
        c if c.contains('U') => "conflict",
        _ => "changed",
    };
    format!("{what:<8} {path}")
}

/// The checks before ending (on a worker): what's changed, what's not in the base, what only its branch has.
fn ending(name: String, root: &Path, base: Option<String>, start: &Path) -> Result<Ending, String> {
    let top = top_of(root)?;
    if top != root {
        return Err(format!("{} isn't a checkout's top any more", crate::allow::tilde(root)));
    }
    let common = common_dir(root)?;
    let status = git(root, &["status", "--porcelain", "--untracked-files=all"], None)?;
    let changed: Vec<String> = status.lines().filter(|l| !l.trim().is_empty()).map(str::to_string).collect();
    let branch = current_branch(root);
    let tip = sha(root, "HEAD")?;
    let base = base_or_start(base, start)?;
    sha(root, &format!("{base}^{{commit}}")).map_err(|_| format!("its base {base} isn't here any more"))?;
    let ahead = parse_log(&git(root, &["log", "--format=%h%x1f%s", &format!("{base}..HEAD")], None)?);
    let lost = match &branch {
        Some(b) => only_on(root, b)?,
        None => Vec::new(),
    };
    Ok(Ending { root: root.to_path_buf(), common, name, branch, tip, base, changed, ahead, lost })
}

/// Its checkout removed (and its branch, `both`, if it's still where the checks saw it) — what that freed.
fn remove(e: &Ending, both: bool) -> Result<(u64, bool), String> {
    let branch = e.branch.as_deref().filter(|_| both);
    if let Some(b) = branch
        && sha(&e.common, &format!("refs/heads/{b}")).ok().as_deref() != Some(e.tip.as_str())
    {
        return Err(format!("{b} moved since the card was shown — nothing removed"));
    }
    let freed = disk_size(&e.root, Instant::now() + MEASURE_FOR);
    let root = e.root.to_str().ok_or("a checkout path that isn't UTF-8")?;
    // (with changes, git asks for --force — the card said they go; without, a change made since stops it)
    let mut args = vec!["worktree", "remove"];
    if !e.changed.is_empty() {
        args.push("--force");
    }
    args.push(root);
    git(&e.common, &args, None)?;
    if let Some(b) = branch {
        git(&e.common, &["branch", "-D", "--", b], None)?;
    }
    Ok(freed)
}

// ── Bringing one into its base ───────────────────────────────────────────────

pub struct Brought {
    pub branch: String,
    pub base: String,
    pub count: usize,
    pub tip: String,
}

/// Its base moved up to its branch — fast-forward only, in the checkout that has the base (refused if that
/// one has changes, saved or not, in the files it brings), else the ref alone (on a worker).
fn bring(root: &Path, base: Option<String>, start: &Path, unsaved: &[PathBuf]) -> Result<Brought, String> {
    let branch = current_branch(root).ok_or("its checkout is on no branch — nothing to bring")?;
    let base = base_or_start(base, start)?;
    let base_tip = local_branch(root, &base)?;
    let tip = sha(root, "HEAD")?;
    if tip == base_tip {
        return Err(format!("nothing to bring — {branch} has no commits past {base}"));
    }
    if !is_ancestor(root, &base_tip, &tip) {
        let moved = count(root, &format!("{tip}..{base_tip}"));
        return Err(format!("{base} moved {} since — rebase {branch} on it first", commits(moved)));
    }
    let n = count(root, &format!("{base_tip}..{tip}"));
    let list = git(root, &["worktree", "list", "--porcelain"], None)?;
    let holder = crate::gitpanel::parse_worktrees(&list)
        .into_iter()
        .find(|(_, b)| b.as_deref() == Some(base.as_str()))
        .map(|(p, _)| std::fs::canonicalize(&p).unwrap_or(p));
    match holder {
        Some(at) => {
            let touched: Vec<String> = git(root, &["diff", "--name-only", "-z", &base_tip, &tip], None)?
                .split('\0')
                .filter(|f| !f.is_empty())
                .map(str::to_string)
                .collect();
            let status = git(&at, &["status", "--porcelain", "-z", "--untracked-files=all"], None)?;
            let mut held: Vec<String> = status
                .split('\0')
                .filter_map(|e| e.get(3..))
                .filter(|f| touched.iter().any(|t| t == f))
                .map(str::to_string)
                .collect();
            let unsaved_too: Vec<String> = touched
                .iter()
                .filter(|t| unsaved.contains(&at.join(t)) && !held.contains(t))
                .cloned()
                .collect();
            held.extend(unsaved_too);
            if !held.is_empty() {
                let some: Vec<&str> = held.iter().take(3).map(String::as_str).collect();
                return Err(format!(
                    "{base}'s checkout has changes in {} it brings ({}{}) — commit, stash or save them first",
                    if held.len() == 1 { "a file".to_string() } else { format!("{} files", held.len()) },
                    some.join(", "),
                    if held.len() > 3 { ", …" } else { "" }
                ));
            }
            git(&at, &["merge", "--ff-only", "-q", &tip], None)?;
        }
        None => {
            let msg = format!("tarae: bring {branch} into {base}");
            git(root, &["update-ref", "-m", &msg, &format!("refs/heads/{base}"), &tip, &base_tip], None)?;
        }
    }
    Ok(Brought { branch, base, count: n, tip: tip[..7].to_string() })
}

// ── The list ─────────────────────────────────────────────────────────────────

/// What git says of a session for its row in the list.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Facts {
    /// Commits past its base · its base's commits it hasn't (none for the checkout tarae started in).
    pub ahead: usize,
    pub behind: usize,
    /// Files changed and not in a commit (new ones too).
    pub changed: usize,
}

/// What git says of the checkout in `root` against its base (on a worker) — None outside git.
fn facts(root: &Path, base: Option<String>, start: &Path) -> Option<Facts> {
    let status = git(root, &["status", "--porcelain", "--untracked-files=all"], None).ok()?;
    let changed = status.lines().filter(|l| !l.trim().is_empty()).count();
    let counts = base_or_start(base, start)
        .ok()
        .filter(|_| root != start)
        .and_then(|b| git(root, &["rev-list", "--left-right", "--count", &format!("HEAD...{b}")], None).ok());
    let mut n = counts.as_deref().unwrap_or("").split_whitespace().map(|x| x.parse().unwrap_or(0));
    Some(Facts { ahead: n.next().unwrap_or(0), behind: n.next().unwrap_or(0), changed })
}

/// A session as its row in the list says it.
pub struct Shown {
    pub id: SessionId,
    pub name: String,
    /// The branch checked out there (said when it isn't the name).
    pub branch: Option<String>,
    pub facts: Option<Facts>,
    pub doing: crate::session::Doing,
    pub front: bool,
    /// CPU % and memory of its processes, summed (none seen yet: None).
    pub usage: Option<(f32, u64)>,
}

/// The list's rows: the name, a glyph for what its Claude does (`●` working, `▲` waiting for you, `◦` a
/// tool call unanswered too long — `stuck? symbols 42s`), and the
/// rest in columns — branch, `↑ahead ↓behind`, what's changed, what it's doing, CPU and memory — each column
/// as wide as its widest and empty ones left out, so the hints (right-aligned) line up row under row.
pub fn items(sessions: &[Shown]) -> Vec<crate::picker::Item> {
    use crate::session::Doing;
    use unicode_width::UnicodeWidthStr as _;
    let cols: Vec<[String; 5]> = sessions
        .iter()
        .map(|l| {
            let branch = l.branch.clone().filter(|b| *b != l.name).unwrap_or_default();
            let f = l.facts.clone().unwrap_or_default();
            let ahead = (f.ahead > 0).then(|| format!("↑{}", f.ahead));
            let behind = (f.behind > 0).then(|| format!("↓{}", f.behind));
            let moved: Vec<String> = ahead.into_iter().chain(behind).collect();
            let changed = if f.changed > 0 { format!("{} changed", f.changed) } else { String::new() };
            let doing = match (l.doing, l.front) {
                (Doing::Waiting, _) => "waits for you".to_string(),
                (d @ Doing::Stuck { .. }, _) => format!("stuck? {}", d.said()),
                (Doing::Working, _) => "working".to_string(),
                (Doing::Idle, true) => "here".to_string(),
                (Doing::Idle, false) => String::new(),
            };
            let usage = l
                .usage
                .map(|(cpu, bytes)| format!("{cpu:>3.0}%  {:>6}", crate::procs::mem(bytes)))
                .unwrap_or_default();
            [branch, moved.join(" "), changed, doing, usage]
        })
        .collect();
    let widths: Vec<usize> = (0..5).map(|c| cols.iter().map(|r| r[c].width()).max().unwrap_or(0)).collect();
    // (counts right-aligned, words left)
    let right = [false, true, true, false, true];
    sessions
        .iter()
        .zip(cols)
        .map(|(l, row)| {
            let parts: Vec<String> = row
                .iter()
                .enumerate()
                .filter(|&(c, _)| widths[c] > 0)
                .map(|(c, s)| {
                    let pad = " ".repeat(widths[c] - s.width());
                    if right[c] { format!("{pad}{s}") } else { format!("{s}{pad}") }
                })
                .collect();
            let glyph = match l.doing {
                Doing::Working => ("●", "ui.accent"),
                Doing::Waiting => ("▲", "warning"),
                Doing::Stuck { .. } => ("◦", "warning"),
                Doing::Idle => (" ", "ui.linenr"),
            };
            crate::picker::Item {
                label: l.name.clone(),
                action: crate::picker::Action::Session(l.id),
                hint: parts.join("  "),
                glyph: Some(glyph),
            }
        })
        .collect()
}

// ── Kept ─────────────────────────────────────────────────────────────────────

static SAVING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The sessions kept for a repository: theirs, which was in front, the save's number.
#[derive(Default)]
pub struct Stored {
    pub sessions: Vec<Kept>,
    pub front: Option<PathBuf>,
    pub seq: u64,
}

/// The folder they're kept in (in the state folder) · what earlier builds called it, and the list in a file
/// and in `:restart`'s state — still read, so nothing kept is lost on upgrade.
const DIR: &str = "sessions";
const OLD_NAME: &str = "lanes";

pub fn default_dir() -> Option<PathBuf> {
    Some(crate::config::state_dir()?.join(DIR))
}

pub fn load(dir: &Path, common: &Path) -> Stored {
    let read = |d: &Path| std::fs::read_to_string(crate::chats::file(d, common)).ok();
    // (one an earlier build kept, while this folder has none of its own)
    let old = || read(&dir.with_file_name(OLD_NAME)).filter(|_| dir.ends_with(DIR));
    let Some(v) = read(dir).or_else(old).and_then(|b| serde_json::from_str::<Value>(&b).ok()) else {
        return Stored::default();
    };
    Stored {
        sessions: kept_list(&v),
        front: v["front"].as_str().map(PathBuf::from),
        seq: v["seq"].as_u64().unwrap_or(0),
    }
}

/// The kept sessions in `v` (a file's, `:restart`'s state) — under their old name if an earlier build wrote it.
fn kept_list(v: &Value) -> Vec<Kept> {
    let list = v.get("sessions").or_else(|| v.get(OLD_NAME));
    list.and_then(Value::as_array).into_iter().flatten().filter_map(Kept::from_json).collect()
}

/// Written — unless what's there was taken later (on a worker).
fn save(dir: &Path, common: &Path, s: Stored) {
    let _one = SAVING.lock().unwrap_or_else(|e| e.into_inner());
    if load(dir, common).seq > s.seq {
        return;
    }
    let sessions: Vec<Value> = s.sessions.iter().map(Kept::json).collect();
    let body = json!({ "repo": common, "seq": s.seq, "front": s.front, "sessions": sessions }).to_string();
    let _ = std::fs::create_dir_all(dir);
    let _ = crate::disk::write_atomic(&crate::chats::file(dir, common), |w| {
        std::io::Write::write_all(w, body.as_bytes())
    });
}

/// The kept sessions still there (their checkout's top where it was) · the ones gone.
fn still_there(sessions: Vec<Kept>) -> (Vec<Kept>, Vec<Kept>) {
    sessions.into_iter().partition(|k| k.root.is_dir() && top_of(&k.root).is_ok_and(|t| t == k.root))
}

/// The sessions for `:restart`'s state (`restart.rs`): theirs and which is in front.
pub fn snapshot(ed: &Editor) -> Value {
    let sessions: Vec<Value> = ed.sessions_in_order().iter().map(|l| Kept::of(l).json()).collect();
    json!({ "sessions": sessions, "front": ed.session.root })
}

/// Back from `:restart`'s state, the front one in front — before its files open.
pub fn restore(ed: &mut Editor, v: &Value) {
    let sessions = kept_list(v);
    if sessions.is_empty() {
        return;
    }
    let (here, gone): (Vec<Kept>, Vec<Kept>) = sessions.into_iter().partition(|k| k.root.is_dir());
    ed.sessions_back(here, gone, v["front"].as_str().map(PathBuf::from), true);
}

impl Editor {
    /// Every session, the first opened first.
    pub fn sessions_in_order(&self) -> Vec<&crate::session::Session> {
        let mut all: Vec<&crate::session::Session> =
            std::iter::once(&self.session).chain(&self.sessions).collect();
        all.sort_by_key(|l| l.id);
        all
    }

    pub(crate) fn session_by_id(&mut self, id: SessionId) -> Option<&mut crate::session::Session> {
        std::iter::once(&mut self.session).chain(&mut self.sessions).find(|l| l.id == id)
    }

    /// The checkout tarae started in (session 0's).
    fn start_root(&self) -> PathBuf {
        self.sessions_in_order().first().map(|l| l.root.clone()).unwrap_or_default()
    }

    /// The session `which` names (its number or name), else the one in front.
    fn session_named(&self, which: &str) -> Result<(SessionId, String), String> {
        let which = which.trim();
        let all = self.sessions_in_order();
        let found = match which {
            "" => Some(&self.session),
            w => all.iter().copied().find(|l| l.name == w || l.id.to_string() == w),
        };
        found.map(|l| (l.id, l.name.clone())).ok_or_else(|| {
            let names: Vec<String> = all.iter().map(|l| format!("{} {}", l.id, l.name)).collect();
            format!("no session {which} — {}", names.join(" · "))
        })
    }

    /// The sessions as they are now, kept for this repository (written on a worker).
    pub fn sessions_keep(&self) {
        let Some(dir) = self.session_store.dir.clone().filter(|_| self.session_store.restored) else {
            return;
        };
        let start = self.start_root();
        let s = Stored {
            sessions: self.sessions_in_order().into_iter().map(Kept::of).collect(),
            front: Some(self.session.root.clone()),
            seq: crate::chats::seq(),
        };
        self.events.shared_jobs().spawn(move || {
            if let Ok(common) = common_dir(&start) {
                save(&dir, &common, s);
            }
            |ed: &mut Editor| ed.unchanged = true
        });
    }

    /// At startup: this repository's kept sessions come back behind (read on a worker); the one that was in
    /// front comes to the front too, unless a key was pressed meanwhile. Ones whose checkout is gone are
    /// dropped, said once.
    pub fn sessions_restore(&mut self) {
        let Some(dir) = self.session_store.dir.clone() else {
            self.session_store.restored = true;
            return;
        };
        let start = self.start_root();
        self.events.shared_jobs().spawn(move || {
            let found = common_dir(&start).ok().map(|common| {
                let s = load(&dir, &common);
                let (here, gone) = still_there(s.sessions);
                (here, gone, s.front)
            });
            move |ed: &mut Editor| match found {
                Some((here, gone, front)) => {
                    let take = !ed.session_store.keyed;
                    ed.sessions_back(here, gone, front, take);
                }
                None => ed.session_store.restored = true,
            }
        });
    }

    /// Kept sessions back: session 0's name and base onto it, the others behind (not twice); `front` in front
    /// when `take_front`; the gone ones said, and the list written again without them.
    pub fn sessions_back(
        &mut self,
        here: Vec<Kept>,
        gone: Vec<Kept>,
        front: Option<PathBuf>,
        take_front: bool,
    ) {
        let start = self.start_root();
        let mut changed = !gone.is_empty();
        for k in here {
            let id = match std::iter::once(&self.session).chain(&self.sessions).find(|l| l.root == k.root) {
                Some(l) => l.id,
                None => {
                    changed = true;
                    self.session_add(k.root.clone())
                }
            };
            if let Some(l) = self.session_by_id(id) {
                (l.name, l.branch, l.base) = (k.name, k.branch, k.base);
            }
        }
        self.session_store.restored = true;
        if take_front
            && let Some(f) = front.filter(|f| *f != self.session.root && *f != start)
            && let Some(id) = self.sessions.iter().find(|l| l.root == f).map(|l| l.id)
        {
            self.session_switch(id);
        }
        if !gone.is_empty() {
            let names: Vec<&str> = gone.iter().map(|k| k.name.as_str()).collect();
            self.set_warning(format!(
                "{} — {} removed outside tarae; dropped",
                names.join(", "),
                if gone.len() == 1 { "its checkout was" } else { "their checkouts were" }
            ));
        }
        if changed {
            self.sessions_keep();
        }
    }

    // ── Making ───────────────────────────────────────────────────────────────

    /// `space W n` — the name of a new session, made from the front session's HEAD (said; `C-b` another base).
    pub fn session_prompt(&mut self) {
        let base = self.session.git_branch.clone().unwrap_or_else(|| "HEAD".into());
        self.open_prompt(crate::editor::PromptKind::Session, "");
        if let Some(p) = self.prompt.as_mut() {
            p.label = Some(format!("new session from {base} (C-b another base):"));
        }
    }

    /// `C-b` in the session prompt: the local branches to make it from — Enter goes on to the card.
    pub fn session_base_picker(&mut self, name: String) {
        let name = name.trim().to_string();
        if name.is_empty() {
            self.session_prompt();
            return self.set_status("its name first — then C-b picks what it's made from");
        }
        let root = self.session.root.clone();
        let picker = crate::picker::Picker::new(format!("{name} from"), Vec::new(), false).without_preview();
        self.open_picker(
            picker,
            Some(Box::new(move || {
                let out = git(
                    &root,
                    &["branch", "--sort=-committerdate", "--format=%(refname:short)%1f%(contents:subject)"],
                    None,
                )?;
                Ok(out
                    .lines()
                    .filter_map(|l| l.split_once('\x1f'))
                    .map(|(b, subject)| crate::picker::Item {
                        label: b.to_string(),
                        action: crate::picker::Action::Typed(format!("session-new {name} {b}")),
                        hint: subject.to_string(),
                        glyph: None,
                    })
                    .collect())
            })),
        );
    }

    /// `:session-new <name> [<base>]` — looked at on a worker, then the card (nothing is made before `y`).
    pub fn session_new(&mut self, name: &str, base: Option<&str>) {
        let name = name.trim().to_string();
        if name.is_empty() {
            return self.set_error("usage: :session-new <name> [<base>] — the name is its branch");
        }
        let (root, base) = (self.session.root.clone(), base.map(str::to_string));
        let setting = self.config.session_worktrees.clone();
        self.set_step("session", format!("Looking at what {name} would take…"));
        self.events.shared_jobs().spawn(move || {
            let found = making(&root, &name, base.as_deref(), &setting);
            move |ed: &mut Editor| {
                ed.step_done("session");
                match found {
                    Ok(m) => ed.push_offer(m.card()),
                    Err(e) => ed.set_error(format!("new session: {e}")),
                }
            }
        });
    }

    /// `y` on the create card: the worktree made (a worker), then its session in front, its chat open.
    pub fn session_make(&mut self, id: u64, m: Making) {
        self.events.shared_jobs().spawn(move || {
            let made = make(&m);
            move |ed: &mut Editor| match made {
                Ok(root) => {
                    ed.offer_finished(id, Ok(()));
                    let session = ed.session_add(root);
                    if let Some(l) = ed.session_by_id(session) {
                        (l.name, l.branch, l.base) =
                            (m.branch.clone(), Some(m.branch.clone()), Some(m.base.clone()));
                    }
                    ed.session_switch(session);
                    crate::chat::open(ed);
                    if let Some(c) = ed.session.chat.as_mut() {
                        c.focused = true;
                    }
                    ed.set_success(format!("Session {} — from {}", m.branch, m.base_said()));
                }
                Err(e) => ed.offer_finished(id, Err(e)),
            }
        });
    }

    // ── Ending ───────────────────────────────────────────────────────────────

    /// `:session-end [<n|name>]` — the checks on a worker; nothing to lose: removed at once (its branch
    /// kept), else the card. Never the checkout tarae started in; not while files of it are unsaved or its
    /// Claude is answering.
    pub fn session_end(&mut self, which: &str) -> Result<(), String> {
        let (id, name) = self.session_named(which)?;
        if id == 0 {
            return Err(format!("{name} is the checkout tarae started in — it stays"));
        }
        let session =
            std::iter::once(&self.session).chain(&self.sessions).find(|l| l.id == id).expect("named");
        let root = session.root.clone();
        if session.chat.as_ref().is_some_and(crate::chat::Chat::busy) {
            return Err(format!("{name}'s Claude is answering — stop it (C-c) or let it finish first"));
        }
        let unsaved: Vec<String> = self
            .docs
            .iter()
            .filter(|d| d.is_modified() && d.path.as_ref().is_some_and(|p| p.starts_with(&root)))
            .map(|d| d.display_name(&root))
            .collect();
        if !unsaved.is_empty() {
            return Err(format!("unsaved changes in {} — save or close them first", unsaved.join(", ")));
        }
        let (base, start) = (session.base.clone(), self.start_root());
        self.set_step("session", format!("Looking at {name} before it ends…"));
        self.events.shared_jobs().spawn(move || {
            let found = ending(name.clone(), &root, base, &start);
            move |ed: &mut Editor| {
                ed.step_done("session");
                match found {
                    Ok(e) if e.quiet() => ed.session_remove(e, false, None),
                    Ok(e) => ed.push_offer(e.card()),
                    Err(e) => ed.set_error(format!("end {name}: {e}")),
                }
            }
        });
        Ok(())
    }

    /// Its session closes (what it ran stops), then its checkout is removed on a worker — and its branch too
    /// when `both`. `card`: the end card's id (it waves meanwhile, and says a failure). A failure opens the
    /// session again.
    pub fn session_remove(&mut self, e: Ending, both: bool, card: Option<u64>) {
        let kept = self.session_close(&e.root);
        if card.is_none() {
            self.set_step("session", format!("Ending {}…", e.name));
        }
        self.events.shared_jobs().spawn(move || {
            let result = remove(&e, both);
            move |ed: &mut Editor| {
                ed.step_done("session");
                match result {
                    Ok(freed) => {
                        if let Some(id) = card {
                            ed.offer_finished(id, Ok(()));
                        }
                        let branch = match (&e.branch, both) {
                            (Some(b), false) => format!(" · branch {b} kept"),
                            (Some(b), true) => {
                                format!(
                                    " · branch {b} deleted — `git branch {b} {}` brings it back",
                                    &e.tip[..7]
                                )
                            }
                            (None, _) => String::new(),
                        };
                        ed.set_success(format!("Ended {} — {} freed{branch}", e.name, size_text(freed)));
                    }
                    Err(why) => {
                        if let Some(k) = kept.filter(|k| k.root.is_dir()) {
                            ed.sessions_back(vec![k], Vec::new(), None, false);
                        }
                        match card {
                            Some(id) => ed.offer_finished(id, Err(why)),
                            None => ed.set_error(format!("end {}: {why}", e.name)),
                        }
                    }
                }
            }
        });
    }

    /// `c` on the end card: its changes go into a commit on its branch first — its session to the front, all
    /// of them staged, the message buffer open with Claude's draft (read it, `:w` commits). Once the commit
    /// is in, the end is asked again (`Committing.then_end`); `:q` in the message keeps the session, its
    /// changes staged.
    pub fn session_commit_first(&mut self, e: Ending) {
        let Some(id) =
            std::iter::once(&self.session).chain(&self.sessions).find(|l| l.root == e.root).map(|l| l.id)
        else {
            return self.set_error(format!("{} isn't open any more", e.name));
        };
        self.session_switch(id);
        self.set_step("session", format!("Staging {}'s changes…", e.name));
        let root = e.root.clone();
        // (its session's: the status and the message are that session's, wherever you are when they land)
        self.events.jobs().spawn(move || {
            let read = git(&root, &["add", "--all"], None)
                .and_then(|_| crate::gitpanel::status(&root).map(crate::gitpanel::Status::with_rows));
            move |ed: &mut Editor| {
                ed.step_done("session");
                match read {
                    Ok(st) => {
                        ed.session.git_status = Some(st);
                        ed.session.commit_then_end = true;
                        ed.git_commit_open(false, true);
                    }
                    Err(why) => ed.set_error(format!("commit {}: {why}", e.name)),
                }
            }
        });
    }

    /// Session `id` closes: the one tarae started in comes to the front first if it was there; what it ran
    /// stops with it (its Claude, terminal, tests and tasks are dropped), its files close (their servers stop
    /// once nothing of theirs is open). What it was, to open it again.
    fn session_close(&mut self, root: &Path) -> Option<Kept> {
        // (by its checkout: one opened again after a failed end has another id)
        let id = std::iter::once(&self.session).chain(&self.sessions).find(|l| l.root == root)?.id;
        if id == 0 {
            return None;
        }
        if self.session.id == id {
            self.session_switch(0);
        }
        let i = self.sessions.iter().position(|l| l.id == id)?;
        let session = self.sessions.remove(i);
        let kept = Kept::of(&session);
        let shown_elsewhere = |ed: &Editor, d| {
            std::iter::once(&ed.session).chain(&ed.sessions).any(|l| l.views.iter().any(|v| v.doc == d))
        };
        let gone: Vec<crate::document::DocId> = self
            .docs
            .iter()
            .filter(|d| match &d.path {
                Some(p) => p.starts_with(&session.root),
                // (its blank scratch — not one another session shows)
                None => {
                    !d.is_modified()
                        && session.views.iter().any(|v| v.doc == d.id)
                        && !shown_elsewhere(self, d.id)
                }
            })
            .map(|d| d.id)
            .collect();
        drop(session);
        for d in gone {
            self.close_doc(d);
        }
        self.sessions_keep();
        Some(kept)
    }

    // ── The list ─────────────────────────────────────────────────────────────

    /// `space W` — the sessions, one row each: what its Claude does, its branch against its base, what's
    /// changed, what its processes use. Enter brings one to the front, a digit the one on that row; `d` ends
    /// it, `b` brings it into its base, `n` makes a new one. Git's part is read on a worker (the last read
    /// shown meanwhile); the processes are looked at once.
    pub fn session_list(&mut self) {
        let mut p = crate::picker::Picker::new("sessions", self.session_items(), false).without_preview();
        p.keys = true;
        let front = self.session.id;
        p.select_where(|a| *a == crate::picker::Action::Session(front));
        self.open_picker(p, None);
        self.procs_wake(crate::procs::Wake::Now);
        let start = self.start_root();
        let asks: Vec<(SessionId, PathBuf, Option<String>)> =
            self.sessions_in_order().iter().map(|l| (l.id, l.root.clone(), l.base.clone())).collect();
        self.events.shared_jobs().spawn(move || {
            let found: Vec<(SessionId, Option<Facts>)> =
                asks.into_iter().map(|(id, root, base)| (id, facts(&root, base, &start))).collect();
            move |ed: &mut Editor| {
                for (id, f) in found {
                    match f {
                        Some(f) => ed.session_store.facts.insert(id, f),
                        None => ed.session_store.facts.remove(&id),
                    };
                }
            }
        });
    }

    /// The list's rows as things are now.
    fn session_items(&self) -> Vec<crate::picker::Item> {
        let shown: Vec<Shown> = self
            .sessions_in_order()
            .into_iter()
            .map(|l| Shown {
                id: l.id,
                name: l.label().to_string(),
                branch: l.git_branch.clone().or_else(|| l.branch.clone()),
                facts: self.session_store.facts.get(&l.id).cloned(),
                doing: self.doing(l),
                front: l.id == self.session.id,
                usage: crate::procs::summed(&self.procs.rows, &self.session_pids(l)),
            })
            .collect();
        items(&shown)
    }

    /// Every event: the list, while it's open, says what's so now (the row in hand stays).
    pub(crate) fn session_list_sync(&mut self) {
        if !self.picker.as_ref().is_some_and(|p| p.keys) {
            return;
        }
        let items = self.session_items();
        if let Some(p) = self.picker.as_mut()
            && p.items().iter().map(|i| (&i.hint, i.glyph)).ne(items.iter().map(|i| (&i.hint, i.glyph)))
        {
            p.set_found(items);
        }
    }

    /// A letter in the list: `j`/`k` move, a digit brings that row's session to the front, `n` makes a new
    /// one, `d` ends the one in hand, `b` brings it into its base.
    pub(crate) fn session_list_key(&mut self, c: char) {
        use crate::picker::Action;
        let Some(p) = self.picker.as_mut() else { return };
        let here = match p.current().map(|i| &i.action) {
            Some(Action::Session(id)) => Some(*id),
            _ => None,
        };
        match c {
            'j' => p.move_by(1),
            'k' => p.move_by(-1),
            '1'..='9' => {
                let row = c as usize - '1' as usize;
                if let Some(Action::Session(id)) = p.items().get(row).map(|i| i.action.clone()) {
                    self.picker = None;
                    self.session_switch(id);
                }
            }
            'n' => {
                self.picker = None;
                self.session_prompt();
            }
            'd' | 'b' => {
                let Some(id) = here else { return };
                self.picker = None;
                let which = id.to_string();
                let done = if c == 'd' { self.session_end(&which) } else { self.session_bring(&which) };
                if let Err(e) = done {
                    self.set_error(e);
                }
            }
            _ => {}
        }
    }

    // ── Bringing ─────────────────────────────────────────────────────────────

    /// `:session-bring [<n|name>]` — its base moved up to its branch, fast-forward only (a worker).
    pub fn session_bring(&mut self, which: &str) -> Result<(), String> {
        let (id, name) = self.session_named(which)?;
        if id == 0 {
            return Err(format!("{name} is the checkout tarae started in — bring a session into it instead"));
        }
        let session =
            std::iter::once(&self.session).chain(&self.sessions).find(|l| l.id == id).expect("named");
        let (root, base, start) = (session.root.clone(), session.base.clone(), self.start_root());
        let unsaved: Vec<PathBuf> =
            self.docs.iter().filter(|d| d.is_modified()).filter_map(|d| d.path.clone()).collect();
        self.set_step("session", format!("Bringing {name} in…"));
        self.events.shared_jobs().spawn(move || {
            let result = bring(&root, base, &start, &unsaved);
            move |ed: &mut Editor| {
                ed.step_done("session");
                match result {
                    Ok(b) => ed.set_success(format!(
                        "Brought {} of {} into {} — now at {}",
                        commits(b.count),
                        b.branch,
                        b.base,
                        b.tip
                    )),
                    Err(e) => ed.set_error(format!("bring {name}: {e}")),
                }
                ed.git_refresh();
                ed.git_reload_bases();
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn has_branch(repo: &Path, b: &str) -> bool {
        let mut git = std::process::Command::new("git");
        git.arg("-C").arg(repo).args(["rev-parse", "--verify", "-q", &format!("refs/heads/{b}")]);
        git.output().unwrap().status.success()
    }

    /// A temp folder of the test's own (real path): a repository on `main` (`a.txt`, `target/` ignored) and
    /// a folder for sessions' checkouts beside it.
    fn place(name: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tarae-sessions-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let repo = dir.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
            &["config", "commit.gpgsign", "false"],
            &["add", "-A"],
            &["commit", "-q", "-m", "base"],
        ] {
            git_in(&repo, args);
        }
        let dir = std::fs::canonicalize(dir).unwrap();
        (dir.join("proj"), dir)
    }

    /// An editor in `repo` (on main), sessions' checkouts under `dir/wt`, a chat that's a quiet stand-in.
    fn editor(repo: &Path, dir: &Path) -> Editor {
        let mut ed = Editor::new(Config::default());
        (ed.session.root, ed.session.name) = (repo.to_path_buf(), "proj".into());
        ed.session.git_branch = Some("main".into());
        ed.config.session_worktrees = dir.join("wt").display().to_string();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        ed
    }

    fn feed(ed: &mut Editor, keys: &str) {
        let mut chars = keys.chars();
        while let Some(c) = chars.next() {
            let key: crate::key::Key = if c == '<' {
                chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap()
            } else {
                crate::key::Key::plain(crate::key::Code::Char(c))
            };
            ed.handle_key(key);
        }
    }

    fn settle_until(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !done(ed) {
            let left = end.saturating_duration_since(Instant::now());
            let ev = ed.events.recv_timeout(left).unwrap_or_else(|| panic!("timed out: {}", said(ed)));
            ed.handle_event(ev);
        }
    }

    /// Everything said since the editor started, newest last.
    fn said(ed: &Editor) -> String {
        ed.messages.iter().map(|m| m.2.as_str()).collect::<Vec<_>>().join(" | ")
    }

    fn says(ed: &Editor, needle: &str) -> bool {
        ed.messages.iter().any(|m| m.2.contains(needle))
    }

    /// `:session-new name [base]` from the checkout tarae started in, its card answered `y` — the session
    /// in front (its chat's input left without the keys, so the next `:` reaches the editor).
    fn new_session(ed: &mut Editor, line: &str) -> PathBuf {
        ed.session_switch(0);
        let cards = ed.offers.len();
        feed(ed, &format!(":session-new {line}<ret>"));
        settle_until(ed, |ed| ed.offers.len() > cards || says(ed, "new session:"));
        assert!(ed.offers.len() > cards, "{}", said(ed));
        let sessions = ed.sessions.len();
        ed.offer_accept();
        settle_until(ed, |ed| ed.sessions.len() > sessions);
        if let Some(c) = ed.session.chat.as_mut() {
            c.focused = false;
        }
        ed.session.root.clone()
    }

    fn end_card(ed: &Editor) -> Option<&Ending> {
        ed.offers.iter().find_map(|o| match &o.what {
            crate::offer::What::SessionEnd(e) => Some(&**e),
            _ => None,
        })
    }

    /// `space W n`: the prompt says what it's made from; the card says where its checkout goes and what's
    /// big and ignored here — and nothing is made before `y`. Then: its branch from main's tip, its session in
    /// front with the chat open and focused, its name and base kept on the session. A name taken (here, a
    /// branch checked out in another worktree) or not a branch name is refused.
    #[test]
    fn a_new_session_is_a_branch_in_its_own_checkout_after_its_card() {
        let (repo, dir) = place("new");
        std::fs::create_dir_all(repo.join("target")).unwrap();
        std::fs::write(repo.join("target/build.bin"), vec![7u8; 21 << 20]).unwrap();
        let mut ed = editor(&repo, &dir);
        feed(&mut ed, "<space>Wn");
        let p = ed.prompt.as_ref().expect("the name's prompt");
        assert_eq!(p.label.as_deref(), Some("new session from main (C-b another base):"));
        feed(&mut ed, "fix-login<ret>");
        settle_until(&mut ed, |ed| !ed.offers.is_empty() || says(ed, "new session:"));
        let o = ed.offers.first().unwrap_or_else(|| panic!("the card: {}", said(&ed)));
        assert_eq!(o.title, "New session: fix-login");
        let main = git_in(&repo, &["rev-parse", "main"]);
        let under = worktrees_dir(&ed.config.session_worktrees, &repo.join(".git")).unwrap();
        let wt = under.join("fix-login");
        let starts = format!(
            "Branch fix-login from main ({}).\nIts checkout, fix-login/, goes in\n  {}\n",
            &main[..7],
            crate::allow::tilde(&under)
        );
        assert!(o.question.starts_with(&starts), "where it goes: {}", o.question);
        assert!(o.question.contains("Ignored here, so not copied:\n  21 MB  target/"), "{}", o.question);
        assert!(!has_branch(&repo, "fix-login") && !wt.exists(), "nothing made before y");
        ed.offer_accept();
        settle_until(&mut ed, |ed| !ed.sessions.is_empty());
        assert_eq!(ed.session.root, std::fs::canonicalize(&wt).unwrap(), "its session in front");
        assert_eq!(git_in(&wt, &["symbolic-ref", "--short", "HEAD"]), "fix-login");
        assert_eq!(git_in(&repo, &["rev-parse", "fix-login"]), main);
        assert_eq!(
            (ed.session.name.as_str(), ed.session.branch.as_deref(), ed.session.base.as_deref()),
            ("fix-login", Some("fix-login"), Some("main"))
        );
        assert!(ed.session.chat.as_ref().is_some_and(|c| c.focused), "its chat open, with the keys");
        assert!(says(&ed, &format!("Session fix-login — from main ({})", &main[..7])), "{}", said(&ed));
        assert!(ed.offers.is_empty());
        // Taken names: a branch there is (checked out in the session's checkout), and one git refuses
        ed.session.chat.as_mut().unwrap().focused = false;
        feed(&mut ed, ":session-switch 0<ret>:session-new fix-login<ret>");
        settle_until(&mut ed, |ed| says(ed, "new session: there's a branch fix-login already"));
        assert!(says(&ed, "already, checked out in "), "says where: {}", said(&ed));
        feed(&mut ed, ":session-new a..b<ret>");
        settle_until(&mut ed, |ed| says(ed, "isn't a branch name"));
        assert!(ed.offers.is_empty() && ed.sessions.len() == 1);
        drop(ed);
        git_in(&repo, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `C-b` in the prompt: the branches to make it from; picking one goes on to the card, made from it.
    #[test]
    fn another_base_is_picked_from_the_branches() {
        let (repo, dir) = place("base");
        git_in(&repo, &["branch", "side"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "on main only"]);
        let mut ed = editor(&repo, &dir);
        feed(&mut ed, "<space>Wntopic<C-b>");
        settle_until(&mut ed, |ed| ed.picker.as_ref().is_some_and(|p| !p.loading));
        feed(&mut ed, "side<ret>");
        settle_until(&mut ed, |ed| !ed.offers.is_empty() || says(ed, "new session:"));
        let side = git_in(&repo, &["rev-parse", "side"]);
        let o = ed.offers.first().unwrap_or_else(|| panic!("{}", said(&ed)));
        assert!(
            o.question.starts_with(&format!("Branch topic from side ({}).", &side[..7])),
            "{}",
            o.question
        );
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Nothing to lose — no changes (the ignored `target/` isn't one: git status says nothing with it
    /// there), every commit in main — ends at once: its checkout removed, what that freed said, its branch
    /// kept, its Claude stopped, its files closed, the checkout tarae started in in front.
    #[test]
    fn a_clean_merged_session_ends_at_once_and_keeps_its_branch() {
        let (repo, dir) = place("quiet");
        let pid_file = dir.join("claude.pid");
        let mut ed = editor(&repo, &dir);
        ed.config.llm.args =
            vec!["-c".into(), format!("echo $$ > '{}'; cat > /dev/null", pid_file.display())];
        let wt = new_session(&mut ed, "done");
        std::fs::create_dir_all(wt.join("target")).unwrap();
        std::fs::write(wt.join("target/out.bin"), vec![1u8; 3 << 20]).unwrap();
        assert_eq!(git_in(&wt, &["status", "--porcelain"]), "", "target/ is ignored: no change");
        feed(&mut ed, ":o a.txt<ret>");
        assert_eq!(ed.doc().path.as_deref(), Some(wt.join("a.txt").as_path()));
        let end = Instant::now() + Duration::from_secs(5);
        while !pid_file.exists() {
            assert!(Instant::now() < end, "its Claude started");
            std::thread::sleep(Duration::from_millis(10));
        }
        let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        feed(&mut ed, ":session-end<ret>");
        settle_until(&mut ed, |ed| says(ed, "Ended done") || says(ed, "end done:"));
        assert!(ed.offers.is_empty(), "no card");
        assert!(!wt.exists(), "its checkout is gone");
        assert!(has_branch(&repo, "done"), "its branch is kept");
        let toast = ed.messages.iter().map(|m| m.2.clone()).find(|m| m.contains("Ended done")).unwrap();
        assert!(toast.ends_with(" freed · branch done kept"), "{toast}");
        let mb: f64 = toast.split(" — ").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
        assert!(mb >= 3.0, "target/'s 3 MB counted in what was freed: {toast}");
        assert!((ed.session.id, ed.sessions.len()) == (0, 0), "the start's session in front, alone");
        assert!(
            !ed.docs.iter().any(|d| d.path.as_ref().is_some_and(|p| p.starts_with(&wt))),
            "its file closed"
        );
        let end = Instant::now() + Duration::from_secs(3);
        // SAFETY: signal 0 only asks whether the process is there.
        while unsafe { libc::kill(pid, 0) } == 0 {
            assert!(Instant::now() < end, "its Claude ({pid}) stopped");
            std::thread::sleep(Duration::from_millis(20));
        }
        // The checkout tarae started in stays
        feed(&mut ed, ":session-end<ret>");
        assert!(says(&ed, "is the checkout tarae started in — it stays"), "{}", said(&ed));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The card drawn and in sight long enough (`offer::SETTLE`) for its keys to answer it.
    fn card_in_sight(ed: &mut Editor) -> Vec<char> {
        let mut out = Vec::new();
        crate::term::render(ed, &mut out, 112, 30).unwrap();
        std::thread::sleep(crate::offer::SETTLE + Duration::from_millis(50));
        crate::term::render(ed, &mut out, 112, 30).unwrap();
        ed.screen.offer_buttons.iter().map(|b| b.3).collect()
    }

    /// Something to lose: the card says what — the changed files, the commits not in main (squash merges
    /// look unmerged), what deleting the branch too would take. Work in no commit: no `y` (a reflex `y`
    /// would throw away what no reflog keeps) — `c` commits it on the branch first, Claude drafting the
    /// message, and then the end is asked again; `R` removes the checkout anyway; `n` keeps it all. With
    /// only commits to lose, `y` removes the checkout and keeps the branch; `D` deletes both.
    #[test]
    fn a_session_with_something_to_lose_asks_first() {
        let (repo, dir) = place("ask");
        let mut ed = editor(&repo, &dir);
        let wt = new_session(&mut ed, "wip");
        std::fs::write(wt.join("a.txt"), "one\ntwo\n").unwrap();
        git_in(&wt, &["commit", "-qam", "half done"]);
        std::fs::write(wt.join("notes.md"), "todo\n").unwrap();
        feed(&mut ed, ":session-end<ret>");
        settle_until(&mut ed, |ed| end_card(ed).is_some() || says(ed, "end wip:"));
        let o = ed.offers.first().unwrap();
        assert_eq!(o.title, "End wip?");
        for part in [
            "1 changed file, in no commit yet:\n  new      notes.md\n",
            "1 commit not in main:\n  ",
            " half done\nA squash merge on a forge looks unmerged here.",
            "Committing first: Claude drafts it, :w commits on wip.",
            "Removing throws them away — no reflog keeps them.",
            "Removing both deletes wip too, and 1 commit on no other branch with it",
        ] {
            assert!(o.question.contains(part), "{part:?} in {}", o.question);
        }
        assert_eq!(o.decided_lines(), 1..4, "the file and the commits are what's decided");
        assert_eq!(o.warning, Some(7..9), "what removing loses, in the warning color");
        assert_eq!(card_in_sight(&mut ed), ['c', 'R', 'D', 'n'], "no y");
        // A reflex y: nothing goes
        feed(&mut ed, "y");
        assert!(end_card(&ed).is_some() && !ed.offers[0].installing(), "still asking: {}", said(&ed));
        assert!(says(&ed, "its changes are in no commit — c commits them first, R removes them anyway"));
        feed(&mut ed, "n");
        assert!(says(&ed, "Kept wip as it is") && wt.exists() && ed.sessions.len() == 1);
        // c: committed on wip (Claude's draft, read and accepted, `:w`), then asked again — with only
        // commits to lose now, y removes the checkout and keeps the branch
        let reply = json!({"type": "result", "result": "Add the notes"});
        ed.config.llm.args = vec!["-c".into(), format!("read -r line; printf '%s\\n' '{reply}'")];
        ed.session_switch(0);
        feed(&mut ed, ":session-end wip<ret>");
        settle_until(&mut ed, |ed| end_card(ed).is_some());
        card_in_sight(&mut ed);
        feed(&mut ed, "c");
        assert_eq!(ed.session.root, wt, "to its session");
        settle_until(&mut ed, |ed| ed.session.review.as_ref().is_some_and(|r| r.ready));
        assert!(ed.session.committing.as_ref().is_some_and(|c| c.then_end));
        feed(&mut ed, "a:w<ret>");
        settle_until(&mut ed, |ed| end_card(ed).is_some_and(|e| !e.dirty()));
        assert_eq!(git_in(&wt, &["log", "-1", "--format=%s"]), "Add the notes");
        assert_eq!(git_in(&wt, &["status", "--porcelain"]), "", "all of it in the commit");
        assert_eq!(card_in_sight(&mut ed), ['y', 'D', 'n']);
        assert!(ed.offers[0].question.starts_with("2 commits not in main:"), "{}", ed.offers[0].question);
        feed(&mut ed, "y");
        settle_until(&mut ed, |ed| says(ed, "Ended wip"));
        assert!(!wt.exists() && has_branch(&repo, "wip"), "{}", said(&ed));
        assert!(said(&ed).contains("freed · branch wip kept"));
        assert_eq!(
            git_in(&repo, &["log", "-1", "--format=%s", "wip"]),
            "Add the notes",
            "kept on the branch"
        );
        // R: work in no commit, removed anyway — the branch stays
        let wt = new_session(&mut ed, "scrap");
        std::fs::write(wt.join("scratch.txt"), "x\n").unwrap();
        feed(&mut ed, ":session-end<ret>");
        settle_until(&mut ed, |ed| end_card(ed).is_some());
        card_in_sight(&mut ed);
        feed(&mut ed, "R");
        settle_until(&mut ed, |ed| says(ed, "Ended scrap"));
        assert!(!wt.exists() && has_branch(&repo, "scrap"), "{}", said(&ed));
        // D: both go
        let wt = new_session(&mut ed, "drop");
        git_in(&wt, &["commit", "-q", "--allow-empty", "-m", "throwaway"]);
        let tip = git_in(&wt, &["rev-parse", "HEAD"]);
        feed(&mut ed, ":session-end drop<ret>");
        settle_until(&mut ed, |ed| end_card(ed).is_some());
        card_in_sight(&mut ed);
        feed(&mut ed, "D");
        settle_until(&mut ed, |ed| says(ed, "Ended drop"));
        assert!(!wt.exists() && !has_branch(&repo, "drop"));
        assert!(says(&ed, &format!("branch drop deleted — `git branch drop {}` brings it back", &tip[..7])));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Bringing in: main moves up to the session's branch in main's checkout (its files follow); refused
    /// when main moved meanwhile (with how far), or when main's checkout has changes in a file it brings;
    /// a base no checkout has moves as a ref alone.
    #[test]
    fn a_session_is_brought_into_its_base_fast_forward_only() {
        let (repo, dir) = place("bring");
        let mut ed = editor(&repo, &dir);
        // Fast-forward: main's checkout follows
        let wt = new_session(&mut ed, "feat");
        std::fs::write(wt.join("a.txt"), "feat\n").unwrap();
        git_in(&wt, &["commit", "-qam", "feat"]);
        let tip = git_in(&wt, &["rev-parse", "HEAD"]);
        feed(&mut ed, ":session-bring<ret>");
        settle_until(&mut ed, |ed| says(ed, "Brought") || says(ed, "bring feat:"));
        assert!(
            says(&ed, &format!("Brought 1 commit of feat into main — now at {}", &tip[..7])),
            "{}",
            said(&ed)
        );
        assert_eq!(git_in(&repo, &["rev-parse", "main"]), tip);
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "feat\n", "its checkout followed");
        // main moved since: refused, with the count
        let wt = new_session(&mut ed, "late");
        git_in(&wt, &["commit", "-q", "--allow-empty", "-m", "late"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "main moves"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "and again"]);
        let main = git_in(&repo, &["rev-parse", "main"]);
        feed(&mut ed, ":session-bring<ret>");
        settle_until(&mut ed, |ed| says(ed, "bring late:"));
        assert!(says(&ed, "main moved 2 commits since — rebase late on it first"), "{}", said(&ed));
        assert_eq!(git_in(&repo, &["rev-parse", "main"]), main);
        // A change in main's checkout to a file it brings: refused, nothing moves
        let wt = new_session(&mut ed, "edit");
        std::fs::write(wt.join("a.txt"), "edit\n").unwrap();
        git_in(&wt, &["commit", "-qam", "edit"]);
        std::fs::write(repo.join("a.txt"), "mine, unsaved in git\n").unwrap();
        feed(&mut ed, ":session-bring<ret>");
        settle_until(&mut ed, |ed| says(ed, "bring edit:"));
        assert!(says(&ed, "main's checkout has changes in a file it brings (a.txt)"), "{}", said(&ed));
        assert_eq!(git_in(&repo, &["rev-parse", "main"]), main);
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "mine, unsaved in git\n");
        git_in(&repo, &["checkout", "-q", "--", "a.txt"]);
        // A base no checkout has: the ref alone moves
        git_in(&repo, &["branch", "side"]);
        let wt = new_session(&mut ed, "topic side");
        git_in(&wt, &["commit", "-q", "--allow-empty", "-m", "topic"]);
        let tip = git_in(&wt, &["rev-parse", "HEAD"]);
        feed(&mut ed, ":session-bring<ret>");
        settle_until(&mut ed, |ed| says(ed, "of topic into side") || says(ed, "bring topic:"));
        assert_eq!(git_in(&repo, &["rev-parse", "side"]), tip, "{}", said(&ed));
        // Nothing to bring
        feed(&mut ed, ":session-bring<ret>");
        settle_until(&mut ed, |ed| says(ed, "nothing to bring"));
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The sessions are kept per repository: a new start brings them back behind, the one that was in front
    /// in front (no key pressed yet); a checkout removed outside tarae is dropped and said once. `:restart`'s
    /// state carries them too, and its files open in the session that was in front.
    #[test]
    fn sessions_come_back_at_startup_and_after_a_restart() {
        let (repo, dir) = place("kept");
        let store = dir.join("sessions");
        let mut ed = editor(&repo, &dir);
        (ed.session_store.dir, ed.session_store.restored) = (Some(store.clone()), true);
        let one = new_session(&mut ed, "one");
        let two = new_session(&mut ed, "two");
        feed(&mut ed, ":session-switch 1<ret>");
        let common = common_dir(&repo).unwrap();
        let written = |n: usize, front: &Path| {
            let s = load(&store, &common);
            s.sessions.len() == n && s.front.as_deref() == Some(front)
        };
        let end = Instant::now() + Duration::from_secs(5);
        while !written(3, &one) {
            assert!(Instant::now() < end, "kept");
            std::thread::sleep(Duration::from_millis(10));
        }
        let snapshot = crate::restart::snapshot(&mut ed);
        drop(ed);
        // A new start: back behind, `one` in front
        let start = |keyed: bool| {
            let mut ed = editor(&repo, &dir);
            ed.session_store.dir = Some(store.clone());
            ed.session_store.keyed = keyed;
            ed.sessions_restore();
            settle_until(&mut ed, |ed| ed.session_store.restored);
            ed
        };
        let ed = start(false);
        assert_eq!(ed.session.root, one, "the one in front");
        let names: Vec<(&str, Option<&str>)> =
            ed.sessions_in_order().iter().map(|l| (l.name.as_str(), l.base.as_deref())).collect();
        assert_eq!(names, [("proj", None), ("one", Some("main")), ("two", Some("main"))]);
        drop(ed);
        let ed = start(true);
        assert_eq!((ed.session.id, ed.sessions.len()), (0, 2), "a key pressed: they stay behind");
        drop(ed);
        // :restart: the sessions, and the files in the one in front
        std::fs::write(one.join("b.txt"), "b\n").unwrap();
        let mut snapshot = snapshot;
        snapshot["docs"] = json!([{ "path": one.join("b.txt") }]);
        snapshot["current"] = json!(0);
        let mut ed = editor(&repo, &dir);
        crate::restart::restore(&mut ed, &snapshot);
        assert_eq!(ed.session.root, one);
        assert_eq!(ed.doc().path.as_deref(), Some(one.join("b.txt").as_path()), "its file, in its session");
        assert_eq!(ed.sessions.len(), 2);
        drop(ed);
        // Removed outside: dropped, said once
        git_in(&repo, &["worktree", "remove", "--force", two.to_str().unwrap()]);
        let ed = start(false);
        assert_eq!(ed.sessions_in_order().len(), 2, "proj and one");
        let told = |ed: &Editor| ed.messages.iter().filter(|m| m.2.contains("removed outside tarae")).count();
        assert_eq!(told(&ed), 1, "{}", said(&ed));
        assert!(says(&ed, "two — its checkout was removed outside tarae; dropped"));
        let end = Instant::now() + Duration::from_secs(5);
        while load(&store, &common).sessions.len() != 2 {
            assert!(Instant::now() < end, "kept without it");
            std::thread::sleep(Duration::from_millis(10));
        }
        drop(ed);
        let ed = start(false);
        assert_eq!(told(&ed), 0, "said once");
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn checkouts_go_in_a_folder_per_repository() {
        let common = Path::new("/src/tarae/.git");
        let d = worktrees_dir("/wt", common).unwrap();
        assert!(
            d.starts_with("/wt") && d.file_name().unwrap().to_string_lossy().starts_with("tarae-"),
            "{d:?}"
        );
        assert_ne!(d, worktrees_dir("/wt", Path::new("/other/tarae/.git")).unwrap(), "two clones keep apart");
        assert_eq!(repo_folder(Path::new("/srv/proj.git")).split('-').next(), Some("proj"), "a bare one");
        assert!(worktrees_dir("wt", common).unwrap_err().contains("a full path"));
    }

    /// The list's rows: the glyph says what its Claude does; the rest in columns as wide as their widest
    /// (counts right-aligned, empty columns left out), so every hint is as wide as the others — right-aligned
    /// in the list, they line up row under row. A session's branch is its name, so it isn't said twice.
    #[test]
    fn the_list_lines_its_columns_up() {
        use crate::session::Doing;
        let shown = |id, name: &str, branch: Option<&str>, f: Option<Facts>, doing, usage| Shown {
            id,
            name: name.into(),
            branch: branch.map(str::to_string),
            facts: f,
            doing,
            front: id == 0,
            usage,
        };
        let f = |ahead, behind, changed| Some(Facts { ahead, behind, changed });
        let rows = items(&[
            shown(0, "tarae", Some("main"), f(0, 0, 0), Doing::Idle, Some((3.0, 40 << 20))),
            shown(1, "fix-login", Some("fix-login"), f(12, 1, 3), Doing::Working, Some((104.0, 1 << 30))),
            shown(2, "parser", Some("parser"), None, Doing::Waiting, None),
        ]);
        let hints: Vec<&str> = rows.iter().map(|i| i.hint.as_str()).collect();
        assert_eq!(
            hints,
            [
                "main                     here             3%   40 MB",
                "      ↑12 ↓1  3 changed  working        104%  1.0 GB",
                "                         waits for you              ",
            ]
        );
        let glyphs: Vec<&str> = rows.iter().map(|i| i.glyph.unwrap().0).collect();
        assert_eq!(glyphs, [" ", "●", "▲"]);
        assert_eq!(rows[2].action, crate::picker::Action::Session(2));
    }

    #[test]
    fn sizes_say_when_counting_stopped() {
        assert_eq!(size_text((3 << 30, true)), "3.0 GB");
        assert_eq!(size_text((48 << 20, false)), "over 48 MB");
    }

    /// What earlier builds kept — `$state/lanes/<hash>.json`, its list under `"lanes"`, and `:restart`'s state
    /// the same — still comes back; once this build has written its own file, that one is read.
    #[test]
    fn sessions_kept_by_earlier_builds_come_back() {
        let state = std::env::temp_dir().join(format!("tarae-kept-old-{}", std::process::id()));
        std::fs::remove_dir_all(&state).ok();
        let (dir, common) = (state.join(DIR), Path::new("/repo/.git"));
        let kept = |name: &str| json!({ "root": format!("/wt/{name}"), "name": name, "branch": name });
        let old = json!({ "repo": common, "seq": 3, "front": "/wt/a", OLD_NAME: [kept("a")] });
        std::fs::create_dir_all(state.join(OLD_NAME)).unwrap();
        std::fs::write(crate::chats::file(&state.join(OLD_NAME), common), old.to_string()).unwrap();
        let s = load(&dir, common);
        assert_eq!(s.sessions.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(), ["a"]);
        assert_eq!((s.front.as_deref(), s.seq), (Some(Path::new("/wt/a")), 3));
        save(
            &dir,
            common,
            Stored { sessions: vec![Kept::from_json(&kept("b")).unwrap()], front: None, seq: 4 },
        );
        assert_eq!(load(&dir, common).sessions[0].name, "b", "its own file wins");
        // (a folder not named as the state's isn't paired with an old one)
        assert!(load(&state.join("elsewhere"), common).sessions.is_empty());
        assert_eq!(kept_list(&json!({ OLD_NAME: [kept("c")] }))[0].name, "c", ":restart's state");
        std::fs::remove_dir_all(&state).ok();
    }
}
