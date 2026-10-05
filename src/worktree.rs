//! Sessions' checkouts: work beside the rest — a branch of its own in a checkout of its own (a git
//! worktree), opened as a session (`session.rs`). `space W n` · `:session-new <name> [<base>]` makes one:
//! the name is the branch, made from the front session's HEAD (`C-b` in the prompt picks another base); a
//! card says where its checkout goes and what it costs on disk before anything is made. `:session-end`
//! removes one — at once when nothing would be lost (no changes, merged into its base), else a card says
//! what would be. `:session-bring` moves its base up to it, fast-forward only — a base that moved since is
//! rebased onto first, after a card (`rebase.rs`). The list (`space W`). The
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
use crate::gitmenu::{git, git_no_hooks};
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
    /// Asked at a session's end where its checkout built — None in tests (they set a stand-in: never the
    /// real cargo home).
    pub cargo: Option<CargoAsk>,
}

/// How a session's end asks cargo where its checkout built (`cargo metadata`'s `build_directory`): the
/// program, and the cargo home — a build dir is only ever removed from its `build/`.
#[derive(Clone, Debug)]
pub struct CargoAsk {
    pub program: PathBuf,
    pub home: PathBuf,
}

impl CargoAsk {
    /// `cargo`, and `$CARGO_HOME` (else `~/.cargo`) — what `{cargo-cache-home}` in a build-dir is.
    pub fn real() -> Option<CargoAsk> {
        let home = std::env::var_os("CARGO_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))?;
        Some(CargoAsk { program: "cargo".into(), home })
    }
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
/// which reads every cargo config as it is; `program`: `cargo`, a stand-in in tests), as cargo prints them
/// — a configured one with its `..` and links (`real_dirs` for comparing) — None when it doesn't say within
/// `CARGO_WAIT`.
fn cargo_dirs(program: &Path, top: &Path) -> Option<(PathBuf, PathBuf)> {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    let mut child = Command::new(program)
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
    let target = PathBuf::from(v["target_directory"].as_str()?);
    let build = v["build_directory"].as_str().map_or_else(|| target.clone(), PathBuf::from);
    Some((target, build))
}

/// Cargo's `build.build-dir` as the config says it for `top` — its `CARGO_BUILD_BUILD_DIR` (`env`), else the
/// nearest `.cargo/config.toml` (or `config`) from `top` up, else `cargo_home`'s — unresolved, its
/// `{workspace-path-hash}` and the like as written. None when nothing sets it.
fn build_dir_setting(top: &Path, cargo_home: Option<&Path>, env: Option<String>) -> Option<String> {
    if let Some(v) = env.filter(|v| !v.is_empty()) {
        return Some(v);
    }
    let read = |dir: &Path| {
        ["config.toml", "config"].iter().find_map(|f| {
            let text = std::fs::read_to_string(dir.join(f)).ok()?;
            let v: toml::Value = toml::from_str(&text).ok()?;
            v.get("build")?.get("build-dir")?.as_str().map(str::to_string)
        })
    };
    top.ancestors().find_map(|d| read(&d.join(".cargo"))).or_else(|| read(cargo_home?))
}

/// Whether a build-dir setting puts each checkout's builds apart (`{workspace-path-hash}`,
/// `{workspace-root}` in it) — then a new checkout builds cold in its own.
fn per_checkout(setting: &str) -> bool {
    setting.contains("{workspace-path-hash}") || setting.contains("{workspace-root}")
}

/// `cargo_dirs`' answer by real paths (as far as they're there) — for comparing, never for removing.
fn real_dirs((target, build): (PathBuf, PathBuf)) -> (PathBuf, PathBuf) {
    let real = |p: PathBuf| std::fs::canonicalize(&p).unwrap_or(p);
    (real(target), real(build))
}

// ── Git ──────────────────────────────────────────────────────────────────────

/// The commit `rev` names (full sha).
pub(crate) fn sha(dir: &Path, rev: &str) -> Result<String, String> {
    git(dir, &["rev-parse", "--verify", "-q", rev], None).map(|s| s.trim().to_string())
}

/// The repository's git folder (shared by all its worktrees), real path.
pub fn common_dir(dir: &Path) -> Result<PathBuf, String> {
    let out = git(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"], None)?;
    let p = PathBuf::from(out.trim());
    Ok(std::fs::canonicalize(&p).unwrap_or(p))
}

/// Whether `a` is in `b`'s history.
pub(crate) fn is_ancestor(dir: &Path, a: &str, b: &str) -> bool {
    git(dir, &["merge-base", "--is-ancestor", a, b], None).is_ok()
}

pub(crate) fn count(dir: &Path, range: &str) -> usize {
    git(dir, &["rev-list", "--count", range], None).ok().and_then(|n| n.trim().parse().ok()).unwrap_or(0)
}

pub(crate) fn commits(n: usize) -> String {
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
    /// Cargo's build-dir outside the checkout is one per checkout (`per_checkout` — #111: the card said
    /// shared): this one's size, what the new checkout's first build makes again from cold.
    pub build_own: Option<(u64, bool)>,
    /// The instruction files git doesn't have here (`instructions`) — copied into its checkout, so its
    /// Claude starts with the project's rules.
    pub copies: Vec<String>,
    /// The other bases `b` on its card steps to (as `base`, `base_sha`): the repository's main branch and
    /// the front session's, whichever isn't `base` (#85: three sessions from main meant going to main
    /// before each).
    pub others: Vec<(String, String)>,
}

/// The instruction files a session's checkout gets from this one when git doesn't carry them (#17: CLAUDE.md
/// is ignored here, and a session's Claude started without the project's rules) — `.claude/settings*.json`
/// too, never the rest of `.claude/`.
const INSTRUCTIONS: [&str; 3] = ["CLAUDE.md", "CLAUDE.local.md", "AGENTS.md"];

/// An instruction file bigger than this isn't one — not copied.
const INSTRUCTIONS_MAX: u64 = 1 << 20;

/// The instruction files in `top` that git doesn't track (ignored or untracked), small and plain files —
/// relative paths, in `INSTRUCTIONS`' order, then `.claude/`'s settings by name. On a worker.
fn instructions(top: &Path) -> Vec<String> {
    let mut settings: Vec<String> = std::fs::read_dir(top.join(".claude"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| n.starts_with("settings") && n.ends_with(".json"))
        .map(|n| format!(".claude/{n}"))
        .collect();
    settings.sort();
    let found: Vec<String> = INSTRUCTIONS
        .iter()
        .map(|n| n.to_string())
        .chain(settings)
        .filter(|rel| {
            std::fs::metadata(top.join(rel)).is_ok_and(|m| m.is_file() && m.len() <= INSTRUCTIONS_MAX)
        })
        .collect();
    if found.is_empty() {
        return found;
    }
    // (tracked ones come with the checkout)
    let mut args = vec!["ls-files", "-z", "--"];
    args.extend(found.iter().map(String::as_str));
    let tracked = git(top, &args, None).unwrap_or_default();
    let tracked: Vec<&str> = tracked.split('\0').collect();
    found.into_iter().filter(|rel| !tracked.contains(&rel.as_str())).collect()
}

/// `files` (relative) from `top` into the new checkout `path` — never over a file that's there. What
/// couldn't be copied, said.
fn copy_instructions(top: &Path, path: &Path, files: &[String]) -> Vec<String> {
    let mut failed = Vec::new();
    for rel in files {
        let to = path.join(rel);
        if to.exists() {
            continue;
        }
        let copied = to
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|_| std::fs::copy(top.join(rel), &to).map(|_| ()));
        if let Err(e) = copied {
            failed.push(format!("{rel}: {e}"));
        }
    }
    failed
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
            // (per checkout: its own, beside this one's — from cold, #111)
            Some((_, build)) if shared(build) && self.build_own.is_some() => {
                let beside = build.parent().map(crate::allow::tilde).unwrap_or_default();
                q += &format!(
                    "\nCargo builds it in a build-dir of its own in {beside}: its first build starts cold"
                );
                match self.build_own {
                    Some(s) if s.0 > 0 => q += &format!(" — this checkout's is {}.", size_text(s)),
                    _ => q += ".",
                }
            }
            Some((_, build)) if shared(build) => {
                q += &format!(
                    "\nCargo's build-dir is shared ({}): sessions reuse its builds, one build at a time.",
                    crate::allow::tilde(build)
                )
            }
            _ => {}
        }
        if !self.copies.is_empty() {
            q += &format!("\n\nCopied into it (not in git here):\n  {}", self.copies.join("  "));
        }
        if let Some((b, s)) = self.others.first() {
            let said = if b == s { s[..7].to_string() } else { format!("{b} ({})", &s[..7]) };
            q += &format!("\n\nb — from {said} instead");
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
    let (base, base_sha, others) = match base {
        Some(b) if b.starts_with('-') => return Err(format!("{b:?} isn't a commit")),
        Some(b) => {
            let s = sha(&top, &format!("{b}^{{commit}}")).map_err(|_| format!("there's no {b} here"))?;
            // (a branch is kept by name — `:session-bring` moves it; anything else is the commit)
            let named = sha(&top, &format!("refs/heads/{b}")).is_ok();
            (if named { b.to_string() } else { s.clone() }, s, Vec::new())
        }
        // The repository's main branch first, the front session's after it (`b` steps)
        None => {
            let s = sha(&top, "HEAD")?;
            let front = (current_branch(&top).unwrap_or_else(|| s.clone()), s);
            let mut bases: Vec<(String, String)> = main_branch(&top)
                .and_then(|m| Some((m.clone(), sha(&top, &format!("refs/heads/{m}")).ok()?)))
                .into_iter()
                .collect();
            if !bases.iter().any(|b| b.0 == front.0) {
                bases.push(front);
            }
            let (base, base_sha) = bases.remove(0);
            (base, base_sha, bases)
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
    let cargo = top.join("Cargo.toml").is_file().then(|| cargo_dirs(Path::new("cargo"), &top)).flatten();
    let cargo = cargo.map(real_dirs);
    // (a build-dir outside it that's per checkout: the new one builds cold — this one's size says how much)
    let home = CargoAsk::real().map(|c| c.home);
    let own = build_dir_setting(&top, home.as_deref(), std::env::var("CARGO_BUILD_BUILD_DIR").ok())
        .is_some_and(|s| per_checkout(&s));
    let build_own = match &cargo {
        Some((_, build)) if own && !build.starts_with(&top) => Some(disk_size(build, until)),
        _ => None,
    };
    let copies = instructions(&top);
    Ok(Making { top, branch, base, base_sha, path, files, big, cargo, build_own, copies, others })
}

/// The repository's main branch, a local one: the remote's default (`origin/HEAD`), else `main` or
/// `master`, else the branch of its main checkout (the first `git worktree list` gives).
fn main_branch(top: &Path) -> Option<String> {
    let local = |b: &str| sha(top, &format!("refs/heads/{b}")).is_ok();
    let remote = git(top, &["symbolic-ref", "--quiet", "--short", "refs/remotes/origin/HEAD"], None)
        .ok()
        .and_then(|r| r.trim().strip_prefix("origin/").map(str::to_string));
    if let Some(b) = remote.into_iter().chain(["main".to_string(), "master".to_string()]).find(|b| local(b)) {
        return Some(b);
    }
    let list = git(top, &["worktree", "list", "--porcelain"], None).ok()?;
    crate::gitpanel::parse_worktrees(&list).into_iter().next()?.1
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
    crate::sorting::by_key(&mut found, |p| !p.ends_with('/'));
    let mut big: Vec<(String, (u64, bool))> = found
        .into_iter()
        .map(|p| (p.to_string(), disk_size(&top.join(p), until)))
        .filter(|(_, s)| s.0 >= BIG)
        .collect();
    crate::sorting::by_key(&mut big, |(_, s)| std::cmp::Reverse(s.0));
    big.truncate(BIG_SHOWN);
    big
}

/// `git worktree add -b` for the card's plan (on a worker), its instruction files copied in — the
/// checkout's real path, and what couldn't be copied.
fn make(m: &Making) -> Result<(PathBuf, Vec<String>), String> {
    if let Some(parent) = m.path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", crate::allow::tilde(parent)))?;
    }
    if m.path.exists() {
        return Err(format!("{} is there already", crate::allow::tilde(&m.path)));
    }
    let path = m.path.to_str().ok_or("a checkout path that isn't UTF-8")?;
    git_no_hooks(&m.top, &["worktree", "add", "-q", "-b", &m.branch, path, &m.base_sha])?;
    let failed = copy_instructions(&m.top, &m.path, &m.copies);
    Ok((std::fs::canonicalize(&m.path).unwrap_or_else(|_| m.path.clone()), failed))
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
    if crate::rebase::under_way(root).is_some() {
        return Err(
            "its checkout is in the middle of a rebase — :rebase-continue or :rebase-abort first".into()
        );
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

/// What a session's end did with its checkout's cargo build dir.
#[derive(Debug, PartialEq)]
pub enum BuildDir {
    /// Nothing to do: not asked, not a cargo project, never built, or inside the checkout (gone with it).
    Nothing,
    /// Removed — what it took.
    Freed((u64, bool)),
    /// Left where it is: why, said whole (`its build dir … stays: …`).
    Left(String),
    /// Removing it failed partway — why.
    Failed(String),
}

/// A build dir to remove: the cargo home's `build/` (its real path) and the two folders below it
/// (`{workspace-path-hash}`'s `xx/yyyy…`).
#[derive(Debug, PartialEq)]
pub struct Doomed {
    builds: PathBuf,
    rel: PathBuf,
}

impl Doomed {
    /// Still as it was checked — `build/` its own real path, each folder below it a folder, not a link —
    /// its path then. Asked again right before removing (a link put there since would be followed).
    fn still(&self) -> Result<PathBuf, String> {
        if std::fs::canonicalize(&self.builds).ok().as_deref() != Some(self.builds.as_path()) {
            return Err(format!("{} isn't where it was", crate::allow::tilde(&self.builds)));
        }
        let mut p = self.builds.clone();
        for c in self.rel.components() {
            p.push(c);
            // (`symlink_metadata`: a link is a link here, never what it points to)
            match std::fs::symlink_metadata(&p) {
                Ok(m) if m.is_dir() => {}
                Ok(_) => return Err(format!("{} is a link, not a folder", crate::allow::tilde(&p))),
                Err(e) => return Err(format!("{}: {e}", crate::allow::tilde(&p))),
            }
        }
        Ok(p)
    }
}

/// Why a checkout's own `.cargo/config.toml` (or `.cargo/config`) has its say on where it builds — it
/// sets `build-dir` (however it's written: parsed, not searched), or can't be read — None when it has none.
/// What cargo prints for a checkout is the checkout's to choose (its Claude can write that file); a build
/// dir it chose is never removed.
fn own_build_dir(root: &Path) -> Option<String> {
    for name in [".cargo/config.toml", ".cargo/config"] {
        let path = root.join(name);
        if std::fs::symlink_metadata(&path).is_err() {
            continue;
        }
        let Ok(table) = std::fs::read_to_string(&path)
            .map_err(|_| ())
            .and_then(|t| toml::from_str::<toml::Table>(&t).map_err(|_| ()))
        else {
            return Some(format!("its {name} can't be read"));
        };
        if table.get("build").and_then(|b| b.get("build-dir")).is_some() {
            return Some(format!("its own {name} sets build-dir"));
        }
    }
    None
}

/// Whether the ended checkout's build dir goes, before its checkout does (cargo has to read its
/// `Cargo.toml`). `ours`: what cargo printed for the checkout at `root` — which the checkout chooses, so
/// it goes only when it's plainly the cargo home's `build/xx/yyyy`: absolute, no `.` or `..`, exactly two
/// folders below `build/`, no link at any level (`Doomed::still`), not set by the checkout's own config
/// (`own_build_dir`); not its target dir nor holding it; and no other checkout of the repository builds
/// there (`others`, asked only then: each one that's a cargo project and what cargo said there).
/// `Ok` = what to remove once the checkout is gone (`remove_build` checks it again).
fn build_dir_fate(
    root: &Path,
    ours: Option<(PathBuf, PathBuf)>,
    home: &Path,
    others: impl FnOnce() -> Vec<(PathBuf, Option<(PathBuf, PathBuf)>)>,
) -> Result<Doomed, BuildDir> {
    use std::path::Component;
    let Some((target, build)) = ours else {
        return Err(BuildDir::Left("cargo didn't say where it built: no build dir removed".into()));
    };
    let stays = |why: String| {
        Err(BuildDir::Left(format!("its build dir {} stays: {why}", crate::allow::tilde(&build))))
    };
    if !build.is_absolute()
        || build.components().any(|c| matches!(c, Component::CurDir | Component::ParentDir))
    {
        return stays("its path has `.` or `..` in it".into());
    }
    if build.starts_with(root) || std::fs::symlink_metadata(&build).is_err() {
        return Err(BuildDir::Nothing);
    }
    if let Some(why) = own_build_dir(root) {
        return stays(why);
    }
    let Ok(builds) = std::fs::canonicalize(home.join("build")) else {
        return stays(format!("there's no {}", crate::allow::tilde(&home.join("build"))));
    };
    // (`{cargo-cache-home}` as cargo spelled it, or its real path)
    let Some(rel) = build.strip_prefix(home.join("build")).or_else(|_| build.strip_prefix(&builds)).ok()
    else {
        return stays(format!("it isn't in {}", crate::allow::tilde(&builds)));
    };
    if rel.components().count() != 2 {
        return stays(format!(
            "it isn't a checkout's own folder in {} (xx/yyyy)",
            crate::allow::tilde(&builds)
        ));
    }
    let doomed = Doomed { builds, rel: rel.to_path_buf() };
    let real = match doomed.still() {
        Ok(p) => p,
        Err(why) => return stays(why),
    };
    // (one inside the other either way: removing one takes some of the other)
    let overlap = |a: &Path, b: &Path| a.starts_with(b) || b.starts_with(a);
    if overlap(&real, &std::fs::canonicalize(&target).unwrap_or(target)) {
        return stays("cargo's target dir is there".into());
    }
    for (other, said) in others() {
        match said.map(real_dirs) {
            None => return stays(format!("cargo didn't say where {} builds", crate::allow::tilde(&other))),
            Some((t, b)) if overlap(&real, &b) || overlap(&real, &t) => {
                return stays(format!("{} builds there too", crate::allow::tilde(&other)));
            }
            Some(_) => {}
        }
    }
    Ok(doomed)
}

/// The doomed build dir removed — checked again first (`Doomed::still`); links inside it are removed, never
/// followed (`remove_dir_all`, `disk_size`).
fn remove_build(d: &Doomed) -> BuildDir {
    let path = match d.still() {
        Ok(p) => p,
        Err(why) => {
            let at = crate::allow::tilde(&d.builds.join(&d.rel));
            return BuildDir::Left(format!("its build dir {at} stays: {why}"));
        }
    };
    let size = disk_size(&path, Instant::now() + MEASURE_FOR);
    match std::fs::remove_dir_all(&path) {
        Ok(()) => BuildDir::Freed(size),
        Err(why) => BuildDir::Failed(format!("{}: {why}", crate::allow::tilde(&path))),
    }
}

/// The repository's other checkouts that are cargo projects (`git worktree list` — the main one, every
/// session's, ones made outside tarae) and where cargo says each builds.
fn other_builds(e: &Ending, program: &Path) -> Vec<(PathBuf, Option<(PathBuf, PathBuf)>)> {
    let list = git(&e.common, &["worktree", "list", "--porcelain"], None).unwrap_or_default();
    crate::gitpanel::parse_worktrees(&list)
        .into_iter()
        .map(|(p, _)| std::fs::canonicalize(&p).unwrap_or(p))
        .filter(|p| *p != e.root && p.join("Cargo.toml").is_file())
        .map(|p| {
            let said = cargo_dirs(program, &p);
            (p, said)
        })
        .collect()
}

/// Its checkout removed (and its branch, `both`, if it's still where the checks saw it) — what that freed,
/// and then its build dir when nothing else builds there (`build_dir_fate`; `cargo`: how to ask).
fn remove(e: &Ending, both: bool, cargo: Option<&CargoAsk>) -> Result<((u64, bool), BuildDir), String> {
    let branch = e.branch.as_deref().filter(|_| both);
    if let Some(b) = branch
        && sha(&e.common, &format!("refs/heads/{b}")).ok().as_deref() != Some(e.tip.as_str())
    {
        return Err(format!("{b} moved since the card was shown — nothing removed"));
    }
    let fate = match cargo {
        Some(c) if e.root.join("Cargo.toml").is_file() => {
            build_dir_fate(&e.root, cargo_dirs(&c.program, &e.root), &c.home, || other_builds(e, &c.program))
        }
        _ => Err(BuildDir::Nothing),
    };
    let freed = disk_size(&e.root, Instant::now() + MEASURE_FOR);
    let root = e.root.to_str().ok_or("a checkout path that isn't UTF-8")?;
    // (with changes, git asks for --force — the card said they go; without, a change made since stops it)
    let mut args = vec!["worktree", "remove"];
    if !e.changed.is_empty() {
        args.push("--force");
    }
    args.push(root);
    git_no_hooks(&e.common, &args)?;
    if let Some(b) = branch {
        git_no_hooks(&e.common, &["branch", "-D", "--", b])?;
    }
    let build = match fate {
        Ok(doomed) => remove_build(&doomed),
        Err(said) => said,
    };
    Ok((freed, build))
}

// ── Bringing one into its base ───────────────────────────────────────────────

pub struct Brought {
    pub branch: String,
    pub base: String,
    pub count: usize,
    pub tip: String,
}

/// What the checks before a bring found (on a worker).
enum Bringing {
    Brought(Brought),
    /// The base moved since: the rebase card.
    Moved(crate::rebase::Moved),
    /// On its base's tip: the bring card (`moved` 0).
    Ready(crate::rebase::Moved),
    /// A rebase is under way in its checkout (tarae's or not — taken up then).
    Rebasing(crate::rebase::Stop),
    Refused(String),
}

/// Its base moved since it left it, and it has commits of its own: a rebase first, not a bring — None when
/// it can be brought as it is (or git can't say: `bring` says why).
fn moved(root: &Path, name: &str, base: Option<String>, start: &Path) -> Option<crate::rebase::Moved> {
    let branch = current_branch(root)?;
    let base = base_or_start(base, start).ok()?;
    let base_tip = local_branch(root, &base).ok()?;
    let tip = sha(root, "HEAD").ok()?;
    let ahead = count(root, &format!("{base_tip}..{tip}"));
    if ahead == 0 || is_ancestor(root, &base_tip, &tip) {
        return None;
    }
    let moved = count(root, &format!("{tip}..{base_tip}"));
    Some(crate::rebase::Moved { root: root.to_path_buf(), name: name.into(), branch, base, moved, ahead })
}

/// It can be brought as it is (its base not moved since): how far ahead — for the bring card. What `bring`
/// would refuse first is said now. Nothing ahead but behind: `ahead` 0 — moved up to its base instead
/// (`rebase::up_card`, #186), unless it has changes (`unsaved`: the editor's buffers).
fn ready(
    root: &Path,
    name: &str,
    base: Option<String>,
    start: &Path,
    unsaved: &[PathBuf],
) -> Result<crate::rebase::Moved, String> {
    let branch = current_branch(root).ok_or("its checkout is on no branch — nothing to bring")?;
    let base = base_or_start(base, start)?;
    let base_tip = local_branch(root, &base)?;
    let ahead = count(root, &format!("{base_tip}..HEAD"));
    let behind = if ahead == 0 { count(root, &format!("HEAD..{base_tip}")) } else { 0 };
    if ahead == 0 && behind > 0 {
        let why = crate::rebase::held_up(root, unsaved);
        let nothing = format!("nothing to bring — {branch} has no commits past {base}");
        return match why {
            Some(why) => {
                Err(format!("{nothing}, and it can't move up to {base}'s {}: {why}", commits(behind)))
            }
            None => Ok(crate::rebase::Moved {
                root: root.into(),
                name: name.into(),
                branch,
                base,
                moved: behind,
                ahead,
            }),
        };
    }
    if ahead == 0 {
        return Err(format!("nothing to bring — {branch} has no commits past {base}"));
    }
    Ok(crate::rebase::Moved { root: root.to_path_buf(), name: name.into(), branch, base, moved: 0, ahead })
}

/// Its base moved up to its branch — fast-forward only, in the checkout that has the base (refused if that
/// one has changes, saved or not, in the files it brings), else the ref alone (on a worker).
fn bring(root: &Path, base: Option<String>, start: &Path, unsaved: &[PathBuf]) -> Result<Brought, String> {
    let branch = current_branch(root).ok_or("its checkout is on no branch — nothing to bring")?;
    let base = base_or_start(base, start)?;
    let base_tip = local_branch(root, &base)?;
    let tip = sha(root, "HEAD")?;
    if tip == base_tip || count(root, &format!("{base_tip}..{tip}")) == 0 {
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
            git_no_hooks(&at, &["merge", "--ff-only", "-q", &tip])?;
        }
        None => {
            let msg = format!("tarae: bring {branch} into {base}");
            git_no_hooks(root, &["update-ref", "-m", &msg, &format!("refs/heads/{base}"), &tip, &base_tip])?;
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
    /// A rebase under way in its checkout, as it says it (`rebasing 2/5`).
    pub rebasing: Option<String>,
    /// Its files that came again from disk while it was behind (`Session.reloaded`, #6).
    pub reloaded: usize,
    /// Its Claude's cache (`warm until 14:32` · `cold` · `cache not reported`) and what its turns cost so far
    /// (`usage.rs`) — None: nothing reported.
    pub claude: Option<(String, String)>,
}

impl Shown {
    /// `claude` for a session's usage at `now_ms`.
    pub fn claude_of(u: &crate::usage::Usage, now_ms: u64) -> Option<(String, String)> {
        use crate::usage::Cache;
        if !u.reported() {
            return None;
        }
        let cache = match u.cache(now_ms) {
            Cache::Warm { left_ms, .. } => format!("● warm until {}", crate::usage::until(now_ms + left_ms)),
            Cache::Cold { .. } => "◦ cold".into(),
            // (a session whose chat began a new conversation, or a CLI that doesn't say)
            Cache::NotReported if u.conv.is_none() => String::new(),
            Cache::NotReported => "cache not reported".into(),
        };
        let t = u.totals();
        let total = if t.turns == 0 { String::new() } else { t.api_said() };
        Some((cache, total))
    }
}

/// The list's rows: the name, a glyph for what its Claude does (`●` working, `▲` waiting for you, `◦` a
/// tool call unanswered too long — `stuck? symbols 42s`; a turn ended behind says `done` in words), and the
/// rest in columns — branch, `↑ahead ↓behind`, what's changed, files reloaded behind (`• 2 reloaded`), what
/// it's doing, its Claude's cache and cost so far, CPU and memory — each column as wide as its widest and
/// empty ones left out, so the hints (right-aligned) line up row under row.
pub fn items(sessions: &[Shown]) -> Vec<crate::picker::Item> {
    use crate::session::Doing;
    use unicode_width::UnicodeWidthStr as _;
    let cols: Vec<[String; 8]> = sessions
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
                (Doing::Done, _) => "done — not looked at".to_string(),
                (d @ Doing::Stuck { .. }, _) => format!("stuck? {}", d.said()),
                (d @ Doing::Queued { .. }, _) => format!("working · {}", d.said()),
                (Doing::Working, _) => "working".to_string(),
                (Doing::Idle, true) => "here".to_string(),
                (Doing::Idle, false) => String::new(),
            };
            let doing = match (&l.rebasing, doing.is_empty()) {
                (Some(r), true) => r.clone(),
                (Some(r), false) => format!("{r} · {doing}"),
                (None, _) => doing,
            };
            let usage = l
                .usage
                .map(|(cpu, bytes)| format!("{cpu:>3.0}%  {:>6}", crate::procs::mem(bytes)))
                .unwrap_or_default();
            let reloaded =
                if l.reloaded > 0 { format!("• {} reloaded", l.reloaded) } else { String::new() };
            let (cache, cost) = l.claude.clone().unwrap_or_default();
            [branch, moved.join(" "), changed, reloaded, doing, cache, cost, usage]
        })
        .collect();
    let widths: Vec<usize> = (0..8).map(|c| cols.iter().map(|r| r[c].width()).max().unwrap_or(0)).collect();
    // (counts right-aligned, words left)
    let right = [false, true, true, true, false, false, true, true];
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
                Doing::Queued { .. } => ("●", "ui.virtual"),
                Doing::Done | Doing::Idle => (" ", "ui.linenr"),
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

/// Back from `:restart`'s state, the front one in front — before its files open. The names of those whose
/// checkout is gone (the restart's toast says them).
pub fn restore(ed: &mut Editor, v: &Value) -> Vec<String> {
    let sessions = kept_list(v);
    if sessions.is_empty() {
        return Vec::new();
    }
    let (here, gone): (Vec<Kept>, Vec<Kept>) = sessions.into_iter().partition(|k| k.root.is_dir());
    ed.sessions_back(here, gone, v["front"].as_str().map(PathBuf::from), true)
}

impl Editor {
    /// Every session, the first opened first.
    pub fn sessions_in_order(&self) -> Vec<&crate::session::Session> {
        let mut all: Vec<&crate::session::Session> =
            std::iter::once(&self.session).chain(&self.sessions).collect();
        crate::sorting::by_key(&mut all, |l| l.id);
        all
    }

    pub(crate) fn session_by_id(&mut self, id: SessionId) -> Option<&mut crate::session::Session> {
        std::iter::once(&mut self.session).chain(&mut self.sessions).find(|l| l.id == id)
    }

    /// The checkout tarae started in (session 0's).
    fn start_root(&self) -> PathBuf {
        self.sessions_in_order().first().map(|l| l.root.clone()).unwrap_or_default()
    }

    /// Its branch, as its watch last read it — a session's base when it has none of its own.
    pub(crate) fn start_branch(&self) -> Option<String> {
        self.sessions_in_order().first().and_then(|l| l.git_branch.clone())
    }

    /// The session `which` names (its number or name), else the one in front.
    pub(crate) fn session_named(&self, which: &str) -> Result<(SessionId, String), String> {
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
        self.events.shared_jobs().background(move || {
            if let Ok(common) = common_dir(&start) {
                save(&dir, &common, s);
            }
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
                    let gone = ed.sessions_back(here, gone, front, take);
                    if !gone.is_empty() {
                        ed.set_warning(format!(
                            "{} — {} removed outside tarae; dropped",
                            gone.join(", "),
                            if gone.len() == 1 { "its checkout was" } else { "their checkouts were" }
                        ));
                    }
                }
                None => ed.session_store.restored = true,
            }
        });
    }

    /// Kept sessions back: session 0's name and base onto it, the others behind (not twice); `front` in front
    /// when `take_front`; the list written again without the gone ones — their names, for the caller to say.
    pub fn sessions_back(
        &mut self,
        here: Vec<Kept>,
        gone: Vec<Kept>,
        front: Option<PathBuf>,
        take_front: bool,
    ) -> Vec<String> {
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
        if changed {
            self.sessions_keep();
        }
        gone.into_iter().map(|k| k.name).collect()
    }

    // ── Making ───────────────────────────────────────────────────────────────

    /// `space W n` — the name of a new session, made from the repository's main branch — its card says, and
    /// `b` there steps to the front session's branch (#85); `C-b` here picks any.
    pub fn session_prompt(&mut self) {
        self.open_prompt(crate::editor::PromptKind::Session, "");
        if let Some(p) = self.prompt.as_mut() {
            p.label = Some("new session from the main branch (C-b another base):".into());
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
        let said = format!("Looking at what {name} would take…");
        let work = move || making(&root, &name, base.as_deref(), &setting);
        self.step(self.events.shared_jobs(), "session", said, work, |ed, found| match found {
            Ok(m) => ed.push_offer(m.card()),
            Err(e) => ed.set_error(format!("new session: {e}")),
        });
    }

    /// `y` on the create card: the worktree made (a worker), then its session in front, its chat open.
    pub fn session_make(&mut self, id: u64, m: Making) {
        self.events.shared_jobs().spawn(move || {
            let made = make(&m);
            move |ed: &mut Editor| match made {
                Ok((root, failed)) => {
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
                    if !failed.is_empty() {
                        ed.set_warning(format!("not copied into it: {}", failed.join(", ")));
                    }
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
        if let Some(r) = session.rebase.as_ref() {
            return Err(format!("end {name}: {}", r.why(&name)));
        }
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
        let said = format!("Looking at {name} before it ends…");
        let n = name.clone();
        let work = move || ending(n, &root, base, &start);
        self.step(self.events.shared_jobs(), "session", said, work, move |ed, found| match found {
            Ok(e) if e.quiet() => ed.session_remove(e, false, None),
            Ok(e) => ed.push_offer(e.card()),
            Err(e) => ed.set_error(format!("end {name}: {e}")),
        });
        Ok(())
    }

    /// Its session closes (what it ran stops), then its checkout is removed on a worker — and its branch too
    /// when `both`, and its cargo build dir when only it built there (`build_dir_fate`). `card`: the end
    /// card's id (it waves meanwhile, and says a failure). A failure opens the session again.
    pub fn session_remove(&mut self, e: Ending, both: bool, card: Option<u64>) {
        let kept = self.session_close(&e.root);
        if card.is_none() {
            self.set_step("session", format!("Ending {}…", e.name));
        }
        let cargo = self.session_store.cargo.clone();
        self.events.shared_jobs().spawn(move || {
            let result = remove(&e, both, cargo.as_ref());
            move |ed: &mut Editor| {
                ed.step_done("session");
                match result {
                    Ok((mut freed, build)) => {
                        if let Some(id) = card {
                            ed.offer_finished(id, Ok(()));
                        }
                        // (its notes and turns go with its checkout — a session made there later starts
                        // without them)
                        ed.restore.set_notes(&e.root, Vec::new());
                        crate::checkpoint::forget(ed, e.root.clone());
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
                        let (mut of_it, mut left) = (String::new(), String::new());
                        match &build {
                            BuildDir::Freed(b) => {
                                freed = (freed.0 + b.0, freed.1 && b.1);
                                of_it = format!(", {} of it its build dir", size_text(*b));
                            }
                            BuildDir::Left(why) => left = format!(" · {why}"),
                            BuildDir::Nothing | BuildDir::Failed(_) => {}
                        }
                        ed.set_success(format!(
                            "Ended {} — {} freed{of_it}{branch}{left}",
                            e.name,
                            size_text(freed)
                        ));
                        if let BuildDir::Failed(why) = build {
                            ed.set_warning(format!("{}'s build dir isn't all removed — {why}", e.name));
                        }
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
        let said = format!("Staging {}'s changes…", e.name);
        let root = e.root.clone();
        let work = move || {
            git_no_hooks(&root, &["add", "--all"])
                .and_then(|_| crate::gitpanel::status(&root, None).map(crate::gitpanel::Status::with_rows))
        };
        // (its session's: the status and the message are that session's, wherever you are when they land)
        self.step(self.events.jobs(), "session", said, work, move |ed, read| match read {
            Ok(st) => {
                ed.session.git_status = Some(st);
                ed.session.commit_then_end = true;
                ed.git_commit_open(false, true);
            }
            Err(why) => ed.set_error(format!("commit {}: {why}", e.name)),
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
        // What it has now is what opening it again in this run brings back (`session_kept_back`): its notes
        // where the restore reads them (with its files still open, for their anchors), its turns written
        let notes = crate::notes::to_json(&session.notes, &self.docs);
        self.restore.set_notes(&session.root, notes);
        crate::checkpoint::keep_of(self, &session);
        let shown_elsewhere = |ed: &Editor, d| {
            std::iter::once(&ed.session).chain(&ed.sessions).any(|l| l.views.iter().any(|v| v.doc == d))
        };
        // (a file outside it that only it had open — `~/.cargo/config.toml` — goes too, with its server:
        // #141; your unsaved edits keep it, in front)
        let only_its = |ed: &Editor, d: &crate::document::Document| {
            session.docs.contains(&d.id)
                && !std::iter::once(&ed.session).chain(&ed.sessions).any(|l| l.docs.contains(&d.id))
                && !shown_elsewhere(ed, d.id)
        };
        let adopt: Vec<crate::document::DocId> =
            self.docs.iter().filter(|d| only_its(self, d) && d.is_modified()).map(|d| d.id).collect();
        for d in adopt {
            self.session_keeps(d);
        }
        let gone: Vec<crate::document::DocId> = self
            .docs
            .iter()
            .filter(|d| match &d.path {
                Some(p) => p.starts_with(&session.root) || only_its(self, d),
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
        let now_ms = crate::usage::now_ms();
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
                rebasing: l.rebase.as_ref().map(crate::rebase::Rebase::said),
                reloaded: l.reloaded,
                claude: Shown::claude_of(&l.usage, now_ms),
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

    /// `:session-bring [<n|name>]`, `b` in the list — asked on the bring card first (`y` brings · `p` the
    /// quick preflight, then brings: #155, it brought at once). The base moved since: a card offers to rebase
    /// it first (`rebase.rs`). Not while a rebase is under way there.
    pub fn session_bring(&mut self, which: &str) -> Result<(), String> {
        self.bringing(which, false)
    }

    /// The bring card's `y` (or its preflight passed): its base moved up to its branch, fast-forward only (a
    /// worker).
    pub(crate) fn session_bring_now(&mut self, which: &str) -> Result<(), String> {
        self.bringing(which, true)
    }

    fn bringing(&mut self, which: &str, now: bool) -> Result<(), String> {
        let (id, name) = self.session_named(which)?;
        if id == 0 {
            return Err(format!("{name} is the checkout tarae started in — bring a session into it instead"));
        }
        let session =
            std::iter::once(&self.session).chain(&self.sessions).find(|l| l.id == id).expect("named");
        // (a rebase stopped there: its card, to go on or abort — #175)
        if session.rebase.is_some() {
            return self.rebase_offer(id).map_err(|e| format!("bring {name}: {e}"));
        }
        let (root, base, start) = (session.root.clone(), session.base.clone(), self.start_root());
        let unsaved: Vec<PathBuf> =
            self.docs.iter().filter(|d| d.is_modified()).filter_map(|d| d.path.clone()).collect();
        let said = format!("Bringing {name} in…");
        let who = name.clone();
        let work = move || match crate::rebase::under_way(&root) {
            Some(stop) => Bringing::Rebasing(stop),
            None => match moved(&root, &who, base.clone(), &start) {
                Some(m) => Bringing::Moved(m),
                None if !now => {
                    ready(&root, &who, base, &start, &unsaved).map_or_else(Bringing::Refused, Bringing::Ready)
                }
                None => {
                    bring(&root, base, &start, &unsaved).map_or_else(Bringing::Refused, Bringing::Brought)
                }
            },
        };
        self.step(self.events.shared_jobs(), "session", said, work, move |ed, result| {
            match result {
                Bringing::Brought(b) => ed.set_success(format!(
                    "Brought {} of {} into {} — now at {}",
                    commits(b.count),
                    b.branch,
                    b.base,
                    b.tip
                )),
                Bringing::Moved(m) => ed.push_offer(m.card()),
                // (nothing of its own, behind: moved up instead — #186)
                Bringing::Ready(m) if m.ahead == 0 => ed.push_offer(m.up_card()),
                Bringing::Ready(m) => {
                    let mut quick = None;
                    ed.with_session(id, |ed| quick = ed.preflight_quick());
                    ed.push_offer(m.bring_card(quick, false));
                }
                // (one tarae didn't start: taken up, so continue and abort know it — and its card up)
                Bringing::Rebasing(stop) => {
                    ed.with_session(id, |ed| {
                        if ed.session.rebase.is_none() {
                            ed.rebase_adopt(stop);
                        }
                    });
                    if let Err(e) = ed.rebase_offer(id) {
                        ed.set_error(format!("bring {name}: {e}"));
                    }
                }
                Bringing::Refused(e) => ed.set_error(format!("bring {name}: {e}")),
            }
            ed.git_refresh();
            ed.git_reload_bases();
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
        assert_eq!(p.label.as_deref(), Some("new session from the main branch (C-b another base):"));
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

    /// #17: the instruction files git doesn't have here (CLAUDE.md, CLAUDE.local.md, AGENTS.md,
    /// `.claude/settings*.json`) go into a new session's checkout, as its card says — not one git tracks
    /// (the checkout has its own), not a big one, never the rest of `.claude/` or `target/`.
    #[test]
    fn a_new_session_gets_the_projects_instruction_files() {
        let (repo, dir) = place("instructions");
        std::fs::write(repo.join(".gitignore"), "target/\nCLAUDE.md\nCLAUDE.local.md\n.claude/\n").unwrap();
        std::fs::write(repo.join("AGENTS.md"), "as committed\n").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-q", "-m", "agents"]);
        let files = [
            ("CLAUDE.md", "# rules\n"),
            ("AGENTS.md", "edited, not committed\n"),
            (".claude/settings.json", "{}\n"),
            (".claude/settings.local.json", "{\"a\": 1}\n"),
            (".claude/commands/x.md", "not this\n"),
            (".claude/other.json", "not this\n"),
            ("target/notes.md", "not this\n"),
        ];
        for (rel, text) in files {
            std::fs::create_dir_all(repo.join(rel).parent().unwrap()).unwrap();
            std::fs::write(repo.join(rel), text).unwrap();
        }
        std::fs::write(repo.join("CLAUDE.local.md"), "x".repeat(INSTRUCTIONS_MAX as usize + 1)).unwrap();
        let mut ed = editor(&repo, &dir);
        feed(&mut ed, ":session-new rules<ret>");
        settle_until(&mut ed, |ed| !ed.offers.is_empty() || says(ed, "new session:"));
        let q = ed.offers.first().map(|o| o.question.clone()).unwrap_or_else(|| said(&ed));
        assert!(
            q.ends_with(
                "Copied into it (not in git here):\n  CLAUDE.md  .claude/settings.json  \
                 .claude/settings.local.json"
            ),
            "{q}"
        );
        ed.offer_accept();
        settle_until(&mut ed, |ed| !ed.sessions.is_empty());
        let wt = ed.session.root.clone();
        let read = |rel: &str| std::fs::read_to_string(wt.join(rel)).ok();
        assert_eq!(read("CLAUDE.md").as_deref(), Some("# rules\n"));
        assert_eq!(read(".claude/settings.json").as_deref(), Some("{}\n"));
        assert_eq!(read(".claude/settings.local.json").as_deref(), Some("{\"a\": 1}\n"));
        assert_eq!(read("AGENTS.md").as_deref(), Some("as committed\n"), "tracked: the checkout's own");
        for rel in ["CLAUDE.local.md", ".claude/commands/x.md", ".claude/other.json", "target/notes.md"] {
            assert!(read(rel).is_none(), "{rel} not copied");
        }
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

    /// #85: from a session in front on its own branch, a new one is made from the repository's main branch by
    /// default — `b` on its card steps to the front session's branch and back, the card saying which.
    #[test]
    fn a_new_session_is_made_from_main_and_b_steps_to_the_front_ones_branch() {
        let (repo, dir) = place("bases");
        let mut ed = editor(&repo, &dir);
        let first = new_session(&mut ed, "first");
        git_in(&first, &["commit", "-q", "--allow-empty", "-m", "on first only"]);
        let (main, on_first) =
            (git_in(&repo, &["rev-parse", "main"]), git_in(&first, &["rev-parse", "HEAD"]));
        assert_ne!(main, on_first);
        // From `first` in front: main by default, first offered
        assert_eq!(ed.session.root, first);
        feed(&mut ed, ":session-new second<ret>");
        settle_until(&mut ed, |ed| !ed.offers.is_empty() || says(ed, "new session:"));
        let q = |ed: &Editor| ed.offers.first().map(|o| o.question.clone()).unwrap_or_else(|| said(ed));
        assert!(q(&ed).starts_with(&format!("Branch second from main ({}).", &main[..7])), "{}", q(&ed));
        assert!(q(&ed).ends_with(&format!("b — from first ({}) instead", &on_first[..7])), "{}", q(&ed));
        assert!(card_in_sight(&mut ed).contains(&'b'), "its button");
        feed(&mut ed, "b");
        assert!(q(&ed).starts_with(&format!("Branch second from first ({}).", &on_first[..7])), "{}", q(&ed));
        assert!(q(&ed).ends_with(&format!("b — from main ({}) instead", &main[..7])), "{}", q(&ed));
        feed(&mut ed, "b");
        assert!(q(&ed).starts_with("Branch second from main"), "{}", q(&ed));
        // Made from main
        card_in_sight(&mut ed);
        feed(&mut ed, "y");
        settle_until(&mut ed, |ed| ed.sessions.len() == 2);
        assert_eq!(git_in(&repo, &["rev-parse", "second"]), main);
        // From the checkout on main: nothing else to offer
        ed.session.chat = None;
        ed.session_switch(0);
        feed(&mut ed, ":session-new third<ret>");
        settle_until(&mut ed, |ed| !ed.offers.is_empty() || says(ed, "new session:"));
        assert!(!q(&ed).contains("b — from"), "{}", q(&ed));
        drop(ed);
        for wt in [first, dir.join("wt").join(repo_folder(&repo.join(".git"))).join("second")] {
            git_in(&repo, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
        }
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

    /// An ended session's notes and turns go with its checkout: a session made later at the same path, in
    /// the same run, starts without them.
    #[test]
    fn an_ended_sessions_notes_and_turns_go_with_it() {
        use crate::checkpoint::{Checkpoint, Touched};
        let (repo, dir) = place("forget");
        let store = dir.join("turns");
        let mut ed = editor(&repo, &dir);
        ed.turns_dir = Some(store.clone());
        let wt = new_session(&mut ed, "gone");
        settle_until(&mut ed, |ed| ed.session.checkpoints.dir.is_some());
        crate::notes::add(&mut ed, &wt.join("a.txt"), 0..1, crate::notes::By::Claude, "a note");
        let (before, after) = (Some(ropey::Rope::from_str("one\n")), ropey::Rope::from_str("two\n"));
        let t = Touched { path: wt.join("a.txt"), before, after, ..Default::default() };
        ed.session.checkpoints.list.push(Checkpoint::new("a turn".into(), vec![t]));
        crate::checkpoint::keep(&ed);
        let kept = |n: usize| crate::checkpoint::load(&store, &wt).list.len() == n;
        let wait = |what: &str, done: &dyn Fn() -> bool| {
            let end = Instant::now() + Duration::from_secs(5);
            while !done() {
                assert!(Instant::now() < end, "{what}");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        wait("its turn kept", &|| kept(1));
        feed(&mut ed, ":session-end<ret>");
        settle_until(&mut ed, |ed| says(ed, "Ended gone") || says(ed, "end gone:"));
        assert!(!wt.exists(), "{}", said(&ed));
        wait("its turns forgotten", &|| kept(0));
        // Another checkout at the same path, a session for it
        git_in(&repo, &["worktree", "add", "-q", wt.to_str().unwrap(), "gone"]);
        let id = ed.session_open(&wt).unwrap();
        settle_until(&mut ed, |ed| ed.session.id == id && ed.session.checkpoints.dir.is_some());
        assert!(ed.session.notes.list.is_empty(), "no note back: {:?}", ed.session.notes.list);
        assert!(ed.session.checkpoints.list.is_empty(), "no turn back");
        git_in(&repo, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The card up titled `title`.
    fn card<'a>(ed: &'a Editor, title: &str) -> Option<&'a crate::offer::Offer> {
        ed.offers.iter().find(|o| o.title == title)
    }

    /// A stand-in cargo in `dir` and a cargo home of its own (`dir/cargo-home`) — never the real ones. Its
    /// `metadata` says the target dir is the checkout's `target/` and the build dir is what the checkout's
    /// `.fake-build` holds. (Both files are ignored by the repository's `info/exclude`, which every checkout
    /// of it shares: a session with them is still one with nothing to lose.)
    fn fake_cargo(repo: &Path, dir: &Path) -> CargoAsk {
        std::fs::write(repo.join(".git/info/exclude"), "Cargo.toml\n.fake-build\n").unwrap();
        CargoAsk { program: fake_cargo_program(dir), home: dir.join("cargo-home") }
    }

    /// The stand-in cargo itself: it prints `.fake-build` as it is, `..` and links and all (as cargo prints
    /// a configured build-dir).
    fn fake_cargo_program(dir: &Path) -> PathBuf {
        let program = dir.join("fake-cargo");
        let script = "#!/bin/sh\nprintf '{\"target_directory\":\"%s/target\",\"build_directory\":\"%s\"}' \
                      \"$(pwd -P)\" \"$(cat .fake-build)\"\n";
        std::fs::write(&program, script).unwrap();
        std::fs::set_permissions(&program, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        program
    }

    /// Review: what cargo says is the checkout's to choose (its own `.cargo/config.toml`, which its Claude
    /// can write; a link in the cargo home's `build/`, which sandboxed runs can make) — so a build dir
    /// goes only when it's plainly the cargo home's `build/xx/yyyy`: no `..`, no link at any level, two
    /// folders below `build/`, not set by the checkout's own config. Each case leaves its marker. Through
    /// what `:session-end` runs: the stand-in cargo, the fate, the removal.
    #[test]
    fn a_build_dir_that_isnt_plainly_the_cargo_homes_build_xx_yyyy_is_never_removed() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("tarae-build-escape-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        let program = fake_cargo_program(&dir);
        let (home, root) = (dir.join("cargo-home"), dir.join("wt/x"));
        let builds = home.join("build");
        for d in [&builds.join("ab"), &root.join(".cargo")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(root.join("Cargo.toml"), "[package]\n").unwrap();
        let marked = |d: &Path| {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("marker"), "keep me").unwrap();
            d.join("marker")
        };
        symlink(dir.join("outside-mid"), builds.join("ln")).unwrap();
        symlink(dir.join("outside-end"), builds.join("ab/ln")).unwrap();
        let cases = [
            // `..` out of the build folder — the checkout's own config can say that (`../../Documents`)
            ("`..` escape", builds.join("../../outside-dotdot"), marked(&dir.join("outside-dotdot"))),
            ("`..` back in", builds.join("ab/../ab/dd"), marked(&builds.join("ab/dd"))),
            ("a link in the middle", builds.join("ln/victim"), marked(&dir.join("outside-mid/victim"))),
            ("a link at the end", builds.join("ab/ln"), marked(&dir.join("outside-end"))),
            ("one level deep", builds.join("one"), marked(&builds.join("one"))),
            ("three levels deep", builds.join("ab/cd/ef"), marked(&builds.join("ab/cd/ef"))),
            ("the build folder", builds.clone(), marked(&builds)),
            ("the cargo home itself", home.clone(), marked(&home)),
        ];
        let mut removed = Vec::new();
        let mut run = |name: &str, printed: &Path, marker: &Path| {
            std::fs::write(root.join(".fake-build"), printed.display().to_string()).unwrap();
            // (and the fate on its own, given the path as printed — it mustn't count on anyone cleaning it)
            let raw =
                build_dir_fate(&root, Some((root.join("target"), printed.to_path_buf())), &home, Vec::new);
            if let Ok(doomed) = &raw {
                let _ = remove_build(doomed);
            }
            if !marker.is_file() {
                removed.push(format!("{name}, as printed ({})", printed.display()));
                return;
            }
            let fate = build_dir_fate(&root, cargo_dirs(&program, &root), &home, Vec::new);
            if let Ok(doomed) = &fate {
                let _ = remove_build(doomed);
            }
            if !marker.is_file() {
                removed.push(format!("{name} ({})", printed.display()));
            } else if !matches!(&fate, Err(BuildDir::Left(_))) {
                removed.push(format!("{name}: not said ({fate:?})"));
            }
        };
        for (name, printed, marker) in &cases {
            run(name, printed, marker);
        }
        // Set by the checkout's own config — even in the right shape — and a config that can't be read
        let own = marked(&builds.join("ab/own"));
        for (name, config) in [
            ("its own config", "[build]\nbuild-dir = \"anywhere\"\n"),
            ("its own config, dotted and escaped", "build.\"build\\u002Ddir\" = \"anywhere\"\n"),
            ("its own config unreadable", "[build\n"),
        ] {
            std::fs::write(root.join(".cargo/config.toml"), config).unwrap();
            run(name, &builds.join("ab/own"), &own);
        }
        std::fs::remove_file(root.join(".cargo/config.toml")).unwrap();
        assert!(removed.is_empty(), "removed, or kept without a word:\n  {}", removed.join("\n  "));
        // And plainly its own: goes
        let fine = marked(&builds.join("ab/fine"));
        std::fs::write(root.join(".fake-build"), builds.join("ab/fine").display().to_string()).unwrap();
        let fate = build_dir_fate(&root, cargo_dirs(&program, &root), &home, Vec::new);
        let doomed = fate.unwrap_or_else(|f| panic!("its own: {f:?}"));
        assert!(matches!(remove_build(&doomed), BuildDir::Freed(_)) && !fine.exists());
        // Checked, then a link put in the middle before the removal: checked again, nothing removed
        marked(&builds.join("sw/ap"));
        std::fs::write(root.join(".fake-build"), builds.join("sw/ap").display().to_string()).unwrap();
        let doomed = build_dir_fate(&root, cargo_dirs(&program, &root), &home, Vec::new).unwrap();
        let victim = marked(&dir.join("outside-swap/ap"));
        std::fs::rename(builds.join("sw"), dir.join("moved-sw")).unwrap();
        symlink(dir.join("outside-swap"), builds.join("sw")).unwrap();
        let said = remove_build(&doomed);
        assert!(matches!(&said, BuildDir::Left(w) if w.contains("is a link")), "{said:?}");
        assert!(victim.is_file(), "what the link points to stays");
        assert!(dir.join("moved-sw/ap/marker").is_file(), "the folder checked, moved aside, stays too");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `checkout` is a cargo project that built into `build` (a marker of `mb` MB there).
    fn builds_in(checkout: &Path, build: &Path, mb: usize) {
        std::fs::write(checkout.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(checkout.join(".fake-build"), build.display().to_string()).unwrap();
        std::fs::create_dir_all(build.join("debug")).unwrap();
        std::fs::write(build.join("debug/marker"), vec![1u8; mb << 20]).unwrap();
    }

    /// `:session-end` asks cargo (on its worker, before the checkout goes) where the session built: its own
    /// build dir in the cargo home's `build/` goes, its size said within what was freed — the main
    /// checkout's beside it stays. One the main checkout builds in too stays, and the end note says why.
    /// One that can't all be removed is a warning; the session still ends.
    #[test]
    fn a_sessions_own_build_dir_goes_with_it() {
        let (repo, dir) = place("builddir");
        let cargo = fake_cargo(&repo, &dir);
        let builds = dir.join("cargo-home/build");
        let main_build = builds.join("aa/main");
        builds_in(&repo, &main_build, 1);
        let mut ed = editor(&repo, &dir);
        ed.session_store.cargo = Some(cargo);
        let ended = |ed: &mut Editor, name: &str| {
            feed(ed, ":session-end<ret>");
            settle_until(ed, |ed| says(ed, &format!("Ended {name}")) || says(ed, &format!("end {name}:")));
            ed.messages.iter().map(|m| m.2.clone()).find(|m| m.contains(&format!("Ended {name}"))).unwrap()
        };
        // Its own: removed, counted
        let wt = new_session(&mut ed, "own");
        let own = builds.join("bb/own");
        builds_in(&wt, &own, 2);
        let toast = ended(&mut ed, "own");
        assert!(toast.ends_with(", 2 MB of it its build dir · branch own kept"), "{toast}");
        let mb: f64 = toast.split(" — ").nth(1).unwrap().split(' ').next().unwrap().parse().unwrap();
        assert!(mb >= 2.0, "the build dir counted in what was freed: {toast}");
        assert!(!wt.exists() && !own.exists(), "its checkout and its build dir are gone");
        assert!(builds.join("bb").is_dir(), "only its own folder");
        assert!(main_build.join("debug/marker").is_file(), "the main checkout's build dir stays");
        // Shared with the main checkout: left, and why
        let wt = new_session(&mut ed, "shared");
        builds_in(&wt, &main_build, 1);
        let toast = ended(&mut ed, "shared");
        let why = format!(
            "· its build dir {} stays: {} builds there too",
            crate::allow::tilde(&main_build),
            crate::allow::tilde(&repo)
        );
        assert!(toast.ends_with(&why), "{toast}");
        assert!(!wt.exists() && main_build.join("debug/marker").is_file());
        // Can't be removed whole (a folder in it without write permission): a warning, the end goes on.
        // (Root removes it anyway — a Linux container; CI's runner and macOS aren't root.)
        // SAFETY: geteuid only reads.
        if unsafe { libc::geteuid() } == 0 {
            eprintln!("a_sessions_own_build_dir_goes_with_it: as root a 0o555 folder is removed — not tried");
            std::fs::remove_dir_all(&dir).ok();
            return;
        }
        let wt = new_session(&mut ed, "stuck");
        let stuck = builds.join("cc/stuck");
        builds_in(&wt, &stuck, 1);
        let locked = stuck.join("debug");
        std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o555)).unwrap();
        let toast = ended(&mut ed, "stuck");
        std::fs::set_permissions(&locked, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        assert!(toast.ends_with(" freed · branch stuck kept"), "{toast}");
        let warned = ed.messages.iter().any(|m| {
            m.1 == crate::editor::ToastKind::Warning
                && m.2.starts_with("stuck's build dir isn't all removed — ")
        });
        assert!(warned, "a warning says it: {}", said(&ed));
        assert!(!wt.exists() && (ed.session.id, ed.sessions.len()) == (0, 0), "it ended all the same");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #141: an ended session's files close — those in its checkout and one outside it that only it had open
    /// — and their servers stop soon after; a file outside it another session has open stays.
    #[test]
    fn an_ended_sessions_servers_stop_with_it() {
        let (repo, dir) = place("servers");
        std::fs::write(repo.join(".git/info/exclude"), "notes.md\n").unwrap();
        for d in ["elsewhere", "shared"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
            std::fs::write(dir.join(d).join("config.toml"), "x = 1\n").unwrap();
        }
        let mut ed = editor(&repo, &dir);
        let cfg = "[lsp.mdfake]\ncommand = \"sh\"\nargs = [\"-c\", \"cat > /dev/null\"]\n\
                   [lsp.tomlfake]\ncommand = \"sh\"\nargs = [\"-c\", \"cat > /dev/null\"]\n\
                   [lang.markdown]\nlsp = [\"mdfake\"]\n[lang.toml]\nlsp = [\"tomlfake\"]\n";
        let (config, w) = crate::config::parse(cfg);
        assert!(w.is_empty(), "{w:?}");
        ed.config.lsp = config.editor.lsp;
        let shared = dir.join("shared/config.toml");
        ed.open(&shared).unwrap();
        let wt = new_session(&mut ed, "docs");
        std::fs::write(wt.join("notes.md"), "# notes\n").unwrap();
        let server = |ed: &mut Editor, p: &Path| {
            ed.open(p).unwrap();
            ed.doc().lsp.client.expect("its server")
        };
        let md = server(&mut ed, &wt.join("notes.md"));
        let toml = server(&mut ed, &dir.join("elsewhere/config.toml"));
        let kept = server(&mut ed, &shared);
        feed(&mut ed, ":session-end<ret>");
        settle_until(&mut ed, |ed| says(ed, "Ended docs") || says(ed, "end docs:"));
        assert!(says(&ed, "Ended docs"), "{}", said(&ed));
        settle_until(&mut ed, |ed| ed.client(md).is_none() && ed.client(toml).is_none());
        let open = |p: &Path| ed.docs.iter().any(|d| d.path.as_deref() == Some(p));
        assert!(!open(&dir.join("elsewhere/config.toml")), "the file outside only it had open closed");
        assert!(open(&shared) && ed.client(kept).is_some(), "one another session has open stays");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Each check that keeps a build dir — and the other checkouts asked only once the ended one's own
    /// checks pass.
    #[test]
    fn a_build_dir_goes_only_from_the_cargo_homes_build_folder_and_only_when_no_one_else_builds_there() {
        let dir = std::env::temp_dir().join(format!("tarae-build-fate-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        let (root, home) = (dir.join("wt/x"), dir.join("cargo-home"));
        let builds = home.join("build");
        let (own, elsewhere) = (builds.join("ab/cdef"), dir.join("elsewhere/build"));
        for d in [&root, &own, &elsewhere, &root.join("target")] {
            std::fs::create_dir_all(d).unwrap();
        }
        let target = root.join("target");
        let asked = std::cell::Cell::new(false);
        let none = || {
            asked.set(true);
            Vec::new()
        };
        let fate = |ours: Option<(PathBuf, PathBuf)>, others: Vec<(PathBuf, Option<(PathBuf, PathBuf)>)>| {
            build_dir_fate(&root, ours, &home, || others).map(|d| d.builds.join(d.rel))
        };
        let left = |r: Result<PathBuf, BuildDir>| match r {
            Err(BuildDir::Left(why)) => why,
            r => panic!("left expected: {r:?}"),
        };
        // Its own, no one else's: goes
        let goes = build_dir_fate(&root, Some((target.clone(), own.clone())), &home, none);
        assert_eq!(goes.map(|d| d.builds.join(d.rel)), Ok(own.clone()));
        assert!(asked.take(), "the others asked");
        // Inside the checkout, or never built: nothing to do here
        assert_eq!(fate(Some((target.clone(), target.clone())), vec![]), Err(BuildDir::Nothing));
        assert_eq!(fate(Some((target.clone(), builds.join("ab/never"))), vec![]), Err(BuildDir::Nothing));
        // Each refusal before the others are asked
        let refused = [
            (None, "cargo didn't say where it built"),
            (Some((target.clone(), elsewhere.clone())), "stays: it isn't in "),
            (Some((target.clone(), builds.clone())), "stays: it isn't a checkout's own folder in "),
            (Some((own.clone(), own.clone())), "stays: cargo's target dir is there"),
            (Some((own.join("target"), own.clone())), "stays: cargo's target dir is there"),
        ];
        for (ours, why) in refused {
            let said = left(build_dir_fate(&root, ours.clone(), &home, none).map(|d| d.builds.join(d.rel)));
            assert!(said.contains(why), "{ours:?}: {said}");
            assert!(!asked.take(), "{ours:?}: the others not asked");
        }
        // Another checkout building there, in a folder of it, or around it — or one cargo didn't answer for
        let main = dir.join("main");
        let ours = || Some((target.clone(), own.clone()));
        let other = |b: PathBuf| vec![(main.clone(), Some((main.join("target"), b)))];
        for b in [own.clone(), own.join("sub"), builds.join("ab")] {
            let said = left(fate(ours(), other(b.clone())));
            assert!(
                said.ends_with(&format!("{} builds there too", crate::allow::tilde(&main))),
                "{b:?}: {said}"
            );
        }
        let said = left(fate(ours(), vec![(main.clone(), None)]));
        assert!(
            said.ends_with(&format!("cargo didn't say where {} builds", crate::allow::tilde(&main))),
            "{said}"
        );
        // A neighbour in build/ is no one's business
        assert_eq!(fate(ours(), other(builds.join("ab/other"))), Ok(own.clone()));
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

    /// `:session-bring` (or `b` in the list) on a session its base hasn't moved from: the bring card, as after
    /// a rebase (#155: it brought at once) — `y` brings it.
    fn bring_asked(ed: &mut Editor, branch: &str, base: &str) {
        let title = format!("Bring {branch} into {base}");
        feed(ed, ":session-bring<ret>");
        settle_until(ed, |ed| card(ed, &title).is_some() || says(ed, &format!("bring {branch}:")));
        assert_eq!(card_in_sight(ed), ['y', 'p', 'n'], "{}", said(ed));
        feed(ed, "y");
    }

    /// Bringing in: main moves up to the session's branch in main's checkout (its files follow); main moved
    /// meanwhile: a card offers to rebase first (with how far — `n` leaves it all); refused when main's
    /// checkout has changes in a file it brings; a base no checkout has moves as a ref alone.
    #[test]
    fn a_session_is_brought_into_its_base_fast_forward_only() {
        let (repo, dir) = place("bring");
        let mut ed = editor(&repo, &dir);
        // Fast-forward: main's checkout follows — asked first (#155), `n` leaves it, `b` in the list asks too
        let wt = new_session(&mut ed, "feat");
        std::fs::write(wt.join("a.txt"), "feat\n").unwrap();
        git_in(&wt, &["commit", "-qam", "feat"]);
        let tip = git_in(&wt, &["rev-parse", "HEAD"]);
        let main_was = git_in(&repo, &["rev-parse", "main"]);
        ed.session_list();
        let row = |ed: &Editor| ed.picker.as_ref().and_then(|p| p.current()).map(|i| i.label.clone());
        for _ in 0..3 {
            if !row(&ed).is_some_and(|l| l.contains("feat")) {
                feed(&mut ed, "j");
            }
        }
        assert!(row(&ed).is_some_and(|l| l.contains("feat")), "{:?}", row(&ed));
        feed(&mut ed, "b");
        settle_until(&mut ed, |ed| card(ed, "Bring feat into main").is_some() || says(ed, "bring feat:"));
        let q = &card(&ed, "Bring feat into main").unwrap().question;
        assert!(q.starts_with("1 commit on main's tip now.\n\nBring it into main"), "{q}");
        assert_eq!(card_in_sight(&mut ed), ['y', 'p', 'n']);
        feed(&mut ed, "n");
        assert_eq!(git_in(&repo, &["rev-parse", "main"]), main_was, "nothing brought yet");
        bring_asked(&mut ed, "feat", "main");
        settle_until(&mut ed, |ed| says(ed, "Brought") || says(ed, "bring feat:"));
        assert!(
            says(&ed, &format!("Brought 1 commit of feat into main — now at {}", &tip[..7])),
            "{}",
            said(&ed)
        );
        assert_eq!(git_in(&repo, &["rev-parse", "main"]), tip);
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "feat\n", "its checkout followed");
        // main moved since: the card offers a rebase first, with the counts — `n` leaves it all
        let wt = new_session(&mut ed, "late");
        git_in(&wt, &["commit", "-q", "--allow-empty", "-m", "late"]);
        let late = git_in(&wt, &["rev-parse", "HEAD"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "main moves"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "and again"]);
        let main = git_in(&repo, &["rev-parse", "main"]);
        feed(&mut ed, ":session-bring<ret>");
        settle_until(&mut ed, |ed| card(ed, "Bring late: main moved").is_some() || says(ed, "bring late:"));
        let q = &card(&ed, "Bring late: main moved").unwrap().question;
        assert!(
            q.starts_with(
                "Rebase late onto main first? main moved 2 commits since late left it; late is 1 commit ahead."
            ),
            "{q}"
        );
        assert_eq!(card_in_sight(&mut ed), ['y', 'n']);
        feed(&mut ed, "n");
        assert!(says(&ed, "Not rebased — late as it is, nothing brought"), "{}", said(&ed));
        assert_eq!(git_in(&repo, &["rev-parse", "main"]), main);
        assert_eq!(git_in(&wt, &["rev-parse", "HEAD"]), late);
        // A change in main's checkout to a file it brings: refused, nothing moves
        let wt = new_session(&mut ed, "edit");
        std::fs::write(wt.join("a.txt"), "edit\n").unwrap();
        git_in(&wt, &["commit", "-qam", "edit"]);
        std::fs::write(repo.join("a.txt"), "mine, unsaved in git\n").unwrap();
        bring_asked(&mut ed, "edit", "main");
        settle_until(&mut ed, |ed| says(ed, "bring edit:"));
        assert!(says(&ed, "main's checkout has changes in a file it brings (a.txt)"), "{}", said(&ed));
        assert_eq!(git_in(&repo, &["rev-parse", "main"]), main);
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "mine, unsaved in git\n");
        git_in(&repo, &["checkout", "-q", "--", "a.txt"]);
        // A base no checkout has: the ref alone moves
        git_in(&repo, &["branch", "side"]);
        let wt = new_session(&mut ed, "topic side");
        // (with hooks a Claude could write, in main's checkout: `update-ref` runs none of them)
        let marks = dir.join("hooks-ran");
        plant_hooks(&repo, &marks);
        let abs = repo.join(".githooks");
        git_in(&repo, &["config", "core.hooksPath", abs.to_str().unwrap()]);
        git_in(&wt, &["commit", "-q", "--allow-empty", "-m", "topic"]);
        marked_by_hand(&marks);
        let tip = git_in(&wt, &["rev-parse", "HEAD"]);
        bring_asked(&mut ed, "topic", "side");
        settle_until(&mut ed, |ed| says(ed, "of topic into side") || says(ed, "bring topic:"));
        assert_eq!(git_in(&repo, &["rev-parse", "side"]), tip, "{}", said(&ed));
        assert!(!marks.exists(), "a hook ran: {}", std::fs::read_to_string(&marks).unwrap_or_default());
        // Nothing to bring
        feed(&mut ed, ":session-bring<ret>");
        settle_until(&mut ed, |ed| says(ed, "nothing to bring"));
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #186: a session with nothing of its own, behind its base — `:session-bring` offers to move it up (a
    /// fast-forward, its card says so) and `y` does; with changes in its checkout it says why it can't.
    #[test]
    fn a_clean_session_behind_its_base_is_moved_up_to_it() {
        let (repo, dir) = place("behind");
        let mut ed = editor(&repo, &dir);
        let wt = new_session(&mut ed, "behind");
        std::fs::write(repo.join("c.txt"), "c\n").unwrap();
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-qm", "main moves"]);
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "and again"]);
        let main = git_in(&repo, &["rev-parse", "main"]);
        feed(&mut ed, ":session-bring<ret>");
        let title = "Move behind up to main";
        settle_until(&mut ed, |ed| card(ed, title).is_some() || says(ed, "bring behind:"));
        let q = &card(&ed, title).unwrap_or_else(|| panic!("{}", said(&ed))).question;
        assert!(
            q.starts_with("behind has no commits past main — nothing to bring. It's 2 commits behind: move it up to main's tip?"),
            "{q}"
        );
        assert!(q.contains("A fast-forward"), "{q}");
        assert_eq!(card_in_sight(&mut ed), ['y', 'n']);
        feed(&mut ed, "y");
        settle_until(&mut ed, |ed| says(ed, "Moved behind up to main") || says(ed, "move behind up:"));
        assert!(says(&ed, &format!("Moved behind up to main — now at {}", &main[..7])), "{}", said(&ed));
        assert_eq!(git_in(&wt, &["rev-parse", "HEAD"]), main);
        assert_eq!(std::fs::read_to_string(wt.join("c.txt")).unwrap(), "c\n", "its checkout followed");
        // Changes in its checkout: said why not, nothing moves
        git_in(&repo, &["commit", "-q", "--allow-empty", "-m", "main again"]);
        std::fs::write(wt.join("a.txt"), "mine\n").unwrap();
        feed(&mut ed, ":session-bring<ret>");
        settle_until(&mut ed, |ed| says(ed, "bring behind:") || card(ed, title).is_some());
        assert!(
            says(
                &ed,
                "can't move up to main's 1 commit: it has changes in a.txt — commit or stash them first"
            ),
            "{}",
            said(&ed)
        );
        assert_eq!(git_in(&wt, &["rev-parse", "HEAD"]), main);
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A Claude stand-in for the sessions made after it: each message is logged to `log`, then it does
    /// `act` (a shell line, in the session's checkout) and ends its turn.
    fn claude_does(ed: &mut Editor, log: &Path, act: &str) {
        let result = json!({"type": "result", "result": "Resolved."});
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do case \"$line\" in '{{\"message\"'*) printf '%s\\n' \"$line\" >> '{}'; \
                 {act} printf '%s\\n' '{result}';; esac; done",
                log.display()
            ),
        ];
    }

    /// A session `feat` with two commits — the first changes `a.txt`'s second line, the second adds `b.txt`
    /// — and main moved since (its own second line of `a.txt`, unless `clean`). Returns its checkout and its
    /// tip.
    fn moved_apart(ed: &mut Editor, repo: &Path, clean: bool) -> (PathBuf, String) {
        let wt = new_session(ed, "feat");
        std::fs::write(wt.join("a.txt"), "one\nfeat\n").unwrap();
        git_in(&wt, &["commit", "-qam", "feat a"]);
        std::fs::write(wt.join("b.txt"), "b\n").unwrap();
        git_in(&wt, &["add", "-A"]);
        git_in(&wt, &["commit", "-qm", "add b"]);
        match clean {
            true => std::fs::write(repo.join("c.txt"), "c\n").unwrap(),
            false => std::fs::write(repo.join("a.txt"), "one\nmain\n").unwrap(),
        }
        git_in(repo, &["add", "-A"]);
        git_in(repo, &["commit", "-qm", "main moves"]);
        (wt.clone(), git_in(&wt, &["rev-parse", "HEAD"]))
    }

    /// `:session-bring`, the rebase card up and answered `y` — the rebase under way.
    fn rebase_asked(ed: &mut Editor) {
        feed(ed, ":session-bring<ret>");
        settle_until(ed, |ed| card(ed, "Bring feat: main moved").is_some() || says(ed, "bring feat:"));
        assert_eq!(card_in_sight(ed), ['y', 'n'], "{}", said(ed));
        feed(ed, "y");
        assert!(ed.session.rebase.is_some(), "under way: {}", said(ed));
    }

    /// Roadmap #3, clean: main moved, so `:session-bring` offers a rebase first; `y` runs `git rebase main`
    /// in the session's checkout (tarae's own run, in the task panel); its commits land on main's tip, and the
    /// bring card offers to bring them now — `y` does.
    #[test]
    fn a_moved_base_is_rebased_onto_then_brought() {
        let (repo, dir) = place("rebase-clean");
        let mut ed = editor(&repo, &dir);
        let (wt, _) = moved_apart(&mut ed, &repo, true);
        rebase_asked(&mut ed);
        let task = ed.session.task_run.as_ref().map(|r| (r.task.name.clone(), r.task.cmd.clone()));
        let cmd = "git -c core.hooksPath=/dev/null -c advice.mergeConflict=false rebase 'main'";
        assert_eq!(task, Some(("rebase onto main".into(), cmd.into())), "in the task panel, with no hooks");
        settle_until(&mut ed, |ed| card(ed, "Rebased feat onto main").is_some() || says(ed, "rebase feat:"));
        let main = git_in(&repo, &["rev-parse", "main"]);
        assert_eq!(git_in(&wt, &["merge-base", "main", "HEAD"]), main, "on main's tip");
        assert_eq!(git_in(&wt, &["log", "--format=%s", "main..HEAD"]), "add b\nfeat a");
        assert!(ed.session.rebase.is_none() && crate::rebase::under_way(&wt).is_none());
        assert!(says(&ed, "Rebased feat onto main — 2 commits on top, now at "), "{}", said(&ed));
        let q = &card(&ed, "Rebased feat onto main").unwrap().question;
        assert!(q.starts_with("2 commits on main's tip now.\n\nBring them into main"), "{q}");
        assert_eq!(card_in_sight(&mut ed), ['y', 'p', 'n']);
        feed(&mut ed, "y");
        settle_until(&mut ed, |ed| says(ed, "Brought") || says(ed, "bring feat:"));
        assert!(says(&ed, "Brought 2 commits of feat into main"), "{}", said(&ed));
        assert_eq!(git_in(&repo, &["rev-parse", "main"]), git_in(&wt, &["rev-parse", "HEAD"]));
        assert_eq!(std::fs::read_to_string(repo.join("b.txt")).unwrap(), "b\n", "main's checkout followed");
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The bring card's `p`, when the project has two preflight sets: the card names the quick one's steps,
    /// `p` runs only those in the session's checkout (the full ones are for before publishing — a stand-in
    /// that would leave a mark), and the bring follows once they pass.
    #[test]
    fn a_rebased_session_is_brought_after_its_quick_preflight() {
        let (repo, dir) = place("rebase-quick");
        let mut ed = editor(&repo, &dir);
        let mark = dir.join("full-ran");
        let step = |name: &str, cmd: String, full: bool| crate::preflight::Step {
            name: name.into(),
            cmd,
            note: None,
            from: ".tarae.toml".into(),
            full,
        };
        ed.config.preflight = vec![
            step("fmt", "echo formatted".into(), false),
            step("perf", format!("touch '{}'", mark.display()), true),
        ];
        let (wt, _) = moved_apart(&mut ed, &repo, true);
        rebase_asked(&mut ed);
        settle_until(&mut ed, |ed| card(ed, "Rebased feat onto main").is_some() || says(ed, "rebase feat:"));
        let q = card(&ed, "Rebased feat onto main").unwrap().question.clone();
        assert!(q.contains("once the quick preflight passes?"), "{q}");
        assert!(q.contains("Quick: fmt — :preflight-full runs the rest, before publishing."), "{q}");
        assert_eq!(card_in_sight(&mut ed), ['y', 'p', 'n']);
        feed(&mut ed, "p");
        let run = ed.session.task_run.as_ref().map(|r| (r.task.name.clone(), r.task.cwd.clone()));
        assert_eq!(run, Some(("quick preflight".into(), wt.clone())), "{}", said(&ed));
        settle_until(&mut ed, |ed| says(ed, "Brought") || says(ed, "not brought") || says(ed, "bring feat:"));
        assert!(says(&ed, "Brought 2 commits of feat into main"), "{}", said(&ed));
        assert!(!mark.exists(), "the full set's step ran");
        assert_eq!(git_in(&repo, &["rev-parse", "main"]), git_in(&wt, &["rev-parse", "HEAD"]));
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Roadmap #3, a conflict: the rebase stops at commit 1 of 2; the session's own Claude is asked in its
    /// chat (the file, both sides, no git, no commit); its turn ends with the markers gone, so the review
    /// shows the file against the commit being replayed and the card offers to go on. Meanwhile the session
    /// says `rebasing 1/2`, and bring, end and `:restart` wait for it. `y` stages it and continues — to the end.
    #[test]
    fn a_conflict_goes_to_the_sessions_claude_and_the_rebase_goes_on() {
        let (repo, dir) = place("rebase-conflict");
        let (log, answer) = (dir.join("claude.log"), dir.join("answer.txt"));
        std::fs::write(&answer, "one\nmain\nfeat\n").unwrap();
        let mut ed = editor(&repo, &dir);
        claude_does(&mut ed, &log, &format!("cp '{}' a.txt;", answer.display()));
        let (wt, _) = moved_apart(&mut ed, &repo, false);
        rebase_asked(&mut ed);
        settle_until(&mut ed, |ed| {
            card(ed, "feat: conflicts resolved").is_some() || says(ed, "rebase feat:")
        });
        assert!(ed.session.code_review.is_some(), "the card comes once the review is open");
        // The panel: stopped at its conflict, not failed
        let state = ed.session.task_run.as_ref().map(|r| r.state.clone());
        assert_eq!(state, Some(crate::task::State::Stopped("1 conflict".into())));
        // Claude was asked: the file, both sides, what not to do
        let heard = std::fs::read_to_string(&log).unwrap();
        for part in [
            "Rebasing feat onto main stopped at commit 1 of 2 (",
            " feat a): git couldn't merge a.txt.",
            "Don't run git and don't commit",
            "<conflicted>\\na.txt\\n</conflicted>",
            "+feat",
            "+main",
        ] {
            assert!(heard.contains(part), "{part:?} in {heard}");
        }
        let r = ed.session.rebase.as_ref().expect("under way");
        assert_eq!(
            (r.stage, r.step, r.stop.files.clone()),
            (crate::rebase::Stage::Resolved, Some((1, 2)), vec!["a.txt".to_string()])
        );
        // The review: the resolved file against the commit being replayed
        let review = ed.session.code_review.as_ref().expect("the review");
        assert!(
            matches!(&review.base, crate::codereview::Base::Replay { label, .. } if label.ends_with(" feat a")),
            "{:?}",
            review.base
        );
        assert_eq!(review.files.iter().map(|f| f.rel.as_str()).collect::<Vec<_>>(), ["a.txt"]);
        let q = &card(&ed, "feat: conflicts resolved").unwrap().question;
        assert!(q.starts_with("a.txt resolved for commit 1 of 2 ("), "{q}");
        // (#175: unmerged in git until `y` — the card says so)
        assert_eq!(git_in(&wt, &["status", "--porcelain", "a.txt"]), "UU a.txt");
        assert!(
            q.ends_with(
                " feat a). The review shows it against that commit — not staged yet: y stages it and goes \
                 on.\n\nX aborts the rebase: feat goes back as it was."
            ),
            "{q}"
        );
        // It says so — the tabs over its chat, its row in the list
        let mut out = Vec::new();
        crate::term::render(&mut ed, &mut out, 112, 30).unwrap();
        let mut vt = vt100::Parser::new(30, 112, 0);
        vt.process(&out);
        let shown = vt.screen().contents();
        assert!(shown.contains("feat rebasing 1/2"), "{shown}");
        assert!(shown.contains("stopped: 1 conflict") && !shown.contains("failed"), "{shown}");
        assert!(ed.session_items()[1].hint.contains("rebasing 1/2"), "{}", ed.session_items()[1].hint);
        // What would get in its way waits for it — bringing it puts its card up again (one, not two)
        feed(&mut ed, ":session-bring<ret>");
        assert!(!says(&ed, "bring feat:"), "{}", said(&ed));
        let resolved = |o: &&crate::offer::Offer| matches!(o.what, crate::offer::What::RebaseResolved { .. });
        assert_eq!(ed.offers.iter().filter(resolved).count(), 1);
        assert!(card(&ed, "feat: conflicts resolved").is_some());
        feed(&mut ed, ":session-end<ret>");
        assert!(says(&ed, "end feat: feat is rebasing 1/2"), "{}", said(&ed));
        let restart = crate::restart::start(&mut ed, false).unwrap_err();
        assert_eq!(restart, "can't restart: feat is rebasing 1/2 — :rebase-continue or :rebase-abort first");
        // y: staged, and on to the end
        assert_eq!(card_in_sight(&mut ed), ['y', 'X', 'n']);
        let a = ed
            .docs
            .iter()
            .find(|d| d.path.as_deref() == Some(wt.join("a.txt").as_path()))
            .expect("in review");
        ed.blame.of = Some((a.id, a.version()));
        feed(&mut ed, "y");
        settle_until(&mut ed, |ed| card(ed, "Rebased feat onto main").is_some() || says(ed, "rebase feat:"));
        assert!(ed.blame.of.is_none(), "its blame is asked again — the lines are committed now");
        assert_eq!(git_in(&wt, &["log", "--format=%s", "main..HEAD"]), "add b\nfeat a", "{}", said(&ed));
        assert_eq!(
            git_in(&wt, &["show", "HEAD~1:a.txt"]),
            "one\nmain\nfeat",
            "Claude's resolution, committed"
        );
        assert_eq!(git_in(&wt, &["log", "-1", "--format=%s", "HEAD~1"]), "feat a", "its message kept");
        assert!(ed.session.rebase.is_none() && ed.session.code_review.is_none());
        assert!(card(&ed, "feat: conflicts resolved").is_none());
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #175: the rebase stopped in `feat`, its card answered `n`, and the user went to another session (as an
    /// inbox jump does): `:session-bring feat` from there puts its card up in front — saying the file isn't
    /// staged, `y` going on — and `:rebase-continue` typed there goes on in feat, not "no rebase under way" in
    /// the session in front.
    #[test]
    fn a_stopped_rebase_is_gone_on_with_from_another_session() {
        let (repo, dir) = place("rebase-elsewhere");
        let (log, answer) = (dir.join("claude.log"), dir.join("answer.txt"));
        std::fs::write(&answer, "one\nmain\nfeat\n").unwrap();
        let mut ed = editor(&repo, &dir);
        claude_does(&mut ed, &log, &format!("cp '{}' a.txt;", answer.display()));
        let (wt, _) = moved_apart(&mut ed, &repo, false);
        let feat = ed.session.id;
        rebase_asked(&mut ed);
        settle_until(&mut ed, |ed| {
            card(ed, "feat: conflicts resolved").is_some() || says(ed, "rebase feat:")
        });
        card_in_sight(&mut ed);
        feed(&mut ed, "n");
        assert!(says(&ed, "the rebase waits"), "{}", said(&ed));
        assert!(ed.session_switch(0));
        // Bringing feat from here: its card, in front
        feed(&mut ed, ":session-bring feat<ret>");
        assert!(!says(&ed, "bring feat:"), "{}", said(&ed));
        let q = card(&ed, "feat: conflicts resolved").expect("its card, here").question.clone();
        assert!(q.contains("not staged yet: y stages it and goes on"), "{q}");
        assert_eq!(card_in_sight(&mut ed), ['y', 'X', 'n']);
        feed(&mut ed, "n");
        // Typed here: feat's rebase goes on, the front staying where it is
        feed(&mut ed, ":rebase-continue<ret>");
        let rebasing = |ed: &Editor| {
            std::iter::once(&ed.session).chain(&ed.sessions).any(|l| l.id == feat && l.rebase.is_some())
        };
        settle_until(&mut ed, |ed| !rebasing(ed) || says(ed, "rebase: ") || says(ed, "no rebase under way"));
        assert!(!says(&ed, "no rebase under way"), "{}", said(&ed));
        assert_eq!(ed.session.id, 0, "the front stays");
        assert!(says(&ed, "feat's rebase goes on — its task panel shows it"), "{}", said(&ed));
        assert_eq!(git_in(&wt, &["log", "--format=%s", "main..HEAD"]), "add b\nfeat a", "{}", said(&ed));
        assert_eq!(git_in(&wt, &["show", "HEAD~1:a.txt"]), "one\nmain\nfeat", "Claude's resolution, staged");
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #129: resolving, Claude changed another file too (a snapshot blessed again) — it's in the review beside
    /// the conflicted one, the card names it, and `y` stages it with them (left unstaged, git refused to go
    /// on). One changed after the review isn't staged unseen: named, and the next `:rebase-continue` takes it.
    #[test]
    fn what_else_a_resolution_changed_is_reviewed_and_staged() {
        let (repo, dir) = place("rebase-also");
        let (log, answer) = (dir.join("claude.log"), dir.join("answer.txt"));
        std::fs::write(&answer, "one\nmain\nfeat\n").unwrap();
        let mut ed = editor(&repo, &dir);
        claude_does(
            &mut ed,
            &log,
            &format!("cp '{}' a.txt; printf 'target/\\nsnap/\\n' > .gitignore;", answer.display()),
        );
        let (wt, _) = moved_apart(&mut ed, &repo, false);
        rebase_asked(&mut ed);
        settle_until(&mut ed, |ed| {
            card(ed, "feat: conflicts resolved").is_some() || says(ed, "rebase feat:")
        });
        let review = ed.session.code_review.as_ref().expect("the review");
        let mut files: Vec<&str> = review.files.iter().map(|f| f.rel.as_str()).collect();
        files.sort_unstable();
        assert_eq!(files, [".gitignore", "a.txt"], "what else it changed is in the review");
        let q = &card(&ed, "feat: conflicts resolved").unwrap().question;
        assert!(q.contains(" It also changed .gitignore — staged with them. The review shows them"), "{q}");
        // Changed after the review: named, not staged unseen
        std::fs::write(wt.join("late.txt"), "late\n").unwrap();
        card_in_sight(&mut ed);
        feed(&mut ed, "y");
        settle_until(&mut ed, |ed| says(ed, "not going on") || says(ed, "Rebased feat"));
        assert!(
            says(&ed, "not going on — late.txt changed too and is unstaged: look at it (space g), then"),
            "{}",
            said(&ed)
        );
        assert!(crate::rebase::under_way(&wt).is_some(), "still stopped");
        feed(&mut ed, ":rebase-continue<ret>");
        settle_until(&mut ed, |ed| card(ed, "Rebased feat onto main").is_some() || says(ed, "rebase feat:"));
        assert_eq!(git_in(&wt, &["log", "--format=%s", "main..HEAD"]), "add b\nfeat a", "{}", said(&ed));
        assert_eq!(
            git_in(&wt, &["show", "HEAD~1:.gitignore"]),
            "target/\nsnap/",
            "staged with the resolution"
        );
        assert_eq!(git_in(&wt, &["show", "HEAD~1:late.txt"]), "late", "the next continue took it");
        assert_eq!(git_in(&wt, &["status", "--porcelain"]), "", "nothing left unstaged");
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Claude's turn ends with markers left: the files are named, nothing goes on — `:rebase-continue`
    /// won't either; `:rebase-abort` puts the branch back where it was.
    #[test]
    fn markers_left_are_named_and_abort_goes_back() {
        let (repo, dir) = place("rebase-abort");
        let log = dir.join("claude.log");
        let mut ed = editor(&repo, &dir);
        claude_does(&mut ed, &log, "");
        let (wt, tip) = moved_apart(&mut ed, &repo, false);
        // Open from before the rebase, saved: its buffer may lag the disk (a shot showed it) — the disk's
        // text is what's checked
        feed(&mut ed, ":o a.txt<ret>");
        rebase_asked(&mut ed);
        settle_until(&mut ed, |ed| says(ed, "conflict markers still in") || says(ed, "rebase feat:"));
        assert!(
            says(&ed, "conflict markers still in a.txt — ask Claude again, or fix them and :rebase-continue"),
            "{}",
            said(&ed)
        );
        assert!(card(&ed, "feat: conflicts resolved").is_none() && ed.session.code_review.is_none());
        feed(&mut ed, ":rebase-continue<ret>");
        settle_until(&mut ed, |ed| says(ed, "not going on"));
        assert!(says(&ed, "conflict markers still in a.txt — not going on"));
        assert!(crate::rebase::under_way(&wt).is_some(), "still stopped");
        feed(&mut ed, ":rebase-abort<ret>");
        settle_until(&mut ed, |ed| says(ed, "Rebase aborted") || says(ed, "rebase feat:"));
        assert!(says(&ed, "Rebase aborted — feat is back where it was"), "{}", said(&ed));
        assert_eq!(git_in(&wt, &["rev-parse", "HEAD"]), tip);
        assert_eq!(std::fs::read_to_string(wt.join("a.txt")).unwrap(), "one\nfeat\n");
        assert!(ed.session.rebase.is_none() && crate::rebase::under_way(&wt).is_none());
        // None under way now
        feed(&mut ed, ":rebase-abort<ret>");
        settle_until(&mut ed, |ed| says(ed, "no rebase under way"));
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Hooks a Claude could write — a `core.hooksPath` inside the checkout `wt` — written as hooks that leave
    /// a mark in the test's own folder: one per hook making, rebasing, bringing or ending a session may run.
    fn plant_hooks(wt: &Path, marks: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        git_in(wt, &["config", "core.hooksPath", ".githooks"]);
        // (out of git: a commit of the test's own doesn't carry them into the other checkout)
        let common = PathBuf::from(git_in(wt, &["rev-parse", "--path-format=absolute", "--git-common-dir"]));
        std::fs::create_dir_all(common.join("info")).unwrap();
        std::fs::write(common.join("info/exclude"), ".githooks/\n").unwrap();
        std::fs::create_dir_all(wt.join(".githooks")).unwrap();
        for hook in [
            "pre-rebase",
            "post-checkout",
            "post-rewrite",
            "reference-transaction",
            "pre-commit",
            "prepare-commit-msg",
            "commit-msg",
            "post-commit",
            "post-merge",
        ] {
            let path = wt.join(".githooks").join(hook);
            std::fs::write(&path, format!("#!/bin/sh\necho {hook} >> '{}'\n", marks.display())).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// The test's own git ran the stand-in hooks (they work): what they marked, and the marks cleared.
    fn marked_by_hand(marks: &Path) -> String {
        let ran = std::fs::read_to_string(marks).unwrap_or_default();
        assert!(ran.contains("pre-commit"), "the stand-ins run for git run as usual: {ran}");
        std::fs::remove_file(marks).unwrap();
        ran
    }

    /// Review 2026-10-04: what tarae's git does in a checkout runs outside the sandbox, so hooks under a
    /// `core.hooksPath` a Claude can write (in main's checkout and in the session's) never run for it —
    /// making the session (`worktree add`), the rebase, its continue after a conflict, the bring (`merge
    /// --ff-only` in main's checkout), an abort, and the end that deletes the branch too.
    #[test]
    fn hooks_in_the_checkout_never_run_in_taraes_rebase() {
        let (repo, dir) = place("rebase-hooks");
        let (log, answer, marks) = (dir.join("claude.log"), dir.join("answer.txt"), dir.join("hooks-ran"));
        std::fs::write(&answer, "one\nmain\nfeat\n").unwrap();
        let mut ed = editor(&repo, &dir);
        claude_does(&mut ed, &log, &format!("cp '{}' a.txt;", answer.display()));
        plant_hooks(&repo, &marks);
        let (wt, _) = moved_apart(&mut ed, &repo, false);
        // (main's commit in `moved_apart` ran them; making the session didn't)
        let ran = marked_by_hand(&marks);
        assert!(!ran.contains("post-checkout"), "worktree add ran a hook: {ran}");
        plant_hooks(&wt, &marks);
        std::fs::write(wt.join("d.txt"), "d\n").unwrap();
        git_in(&wt, &["add", "d.txt"]);
        git_in(&wt, &["commit", "-qm", "hooked"]);
        marked_by_hand(&marks);
        // The rebase stops at its conflict; Claude resolves it; it goes on to the end; it's brought
        rebase_asked(&mut ed);
        settle_until(&mut ed, |ed| {
            card(ed, "feat: conflicts resolved").is_some() || says(ed, "rebase feat:")
        });
        card_in_sight(&mut ed);
        feed(&mut ed, "y");
        settle_until(&mut ed, |ed| card(ed, "Rebased feat onto main").is_some() || says(ed, "rebase feat:"));
        assert_eq!(
            git_in(&wt, &["log", "--format=%s", "main..HEAD"]),
            "hooked\nadd b\nfeat a",
            "{}",
            said(&ed)
        );
        card_in_sight(&mut ed);
        feed(&mut ed, "y");
        settle_until(&mut ed, |ed| says(ed, "Brought") || says(ed, "bring feat:"));
        assert_eq!(
            git_in(&repo, &["rev-parse", "main"]),
            git_in(&wt, &["rev-parse", "HEAD"]),
            "{}",
            said(&ed)
        );
        assert!(!marks.exists(), "a hook ran: {}", std::fs::read_to_string(&marks).unwrap_or_default());
        // An abort: both change a.txt's second line again
        std::fs::write(wt.join("a.txt"), "one\nfeat two\n").unwrap();
        git_in(&wt, &["commit", "-qam", "feat two"]);
        std::fs::write(repo.join("a.txt"), "one\nmain again\n").unwrap();
        git_in(&repo, &["commit", "-qam", "main moves again"]);
        marked_by_hand(&marks);
        ed.offers.clear();
        rebase_asked(&mut ed);
        settle_until(&mut ed, |ed| {
            card(ed, "feat: conflicts resolved").is_some() || says(ed, "rebase feat:")
        });
        feed(&mut ed, ":rebase-abort<ret>");
        settle_until(&mut ed, |ed| says(ed, "Rebase aborted") || says(ed, "rebase feat:"));
        // The end, its branch deleted too — run from the git folder, where a relative hooksPath means a folder
        // in `.git`: an absolute one into main's checkout
        let abs = repo.join(".githooks");
        git_in(&repo, &["config", "core.hooksPath", abs.to_str().unwrap()]);
        ed.offers.clear();
        feed(&mut ed, ":session-end<ret>");
        settle_until(&mut ed, |ed| end_card(ed).is_some() || says(ed, "end feat:"));
        card_in_sight(&mut ed);
        feed(&mut ed, "D");
        settle_until(&mut ed, |ed| says(ed, "Ended feat") || says(ed, "end feat:"));
        assert!(!wt.exists() && !has_branch(&repo, "feat"), "{}", said(&ed));
        assert!(!marks.exists(), "a hook ran: {}", std::fs::read_to_string(&marks).unwrap_or_default());
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Review 2026-10-04: the user's own git in a session's checkout whose `core.hooksPath` is inside it. A
    /// hook committed and as the base has it runs as before; one changed since the base, or untracked, is
    /// asked about first — named and shown, nothing run yet — `n` goes on without hooks, `y` runs them.
    #[test]
    fn the_users_git_asks_before_hooks_a_claude_could_have_written() {
        use std::os::unix::fs::PermissionsExt as _;
        let (repo, dir) = place("user-hooks");
        let marks = dir.join("hooks-ran");
        let hook = |at: &Path, name: &str, says: &str| {
            let p = at.join(".githooks").join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, format!("#!/bin/sh\necho {says} >> '{}'\n", marks.display())).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let ran = || std::fs::read_to_string(&marks).unwrap_or_default();
        // main has its pre-commit committed (the test's own commit runs it)
        git_in(&repo, &["config", "core.hooksPath", ".githooks"]);
        hook(&repo, "pre-commit", "committed");
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-qm", "hooks"]);
        std::fs::remove_file(&marks).unwrap();
        let mut ed = editor(&repo, &dir);
        let wt = new_session(&mut ed, "feat");
        let amended = |ed: &Editor| ed.messages.iter().filter(|m| m.2.starts_with("Amended")).count();
        let amend = |ed: &mut Editor, n: &str| {
            std::fs::write(wt.join("a.txt"), format!("{n}\n")).unwrap();
            git_in(&wt, &["add", "a.txt"]);
            feed(ed, ":git-amend<ret>");
        };
        // As its base has it: runs, no card
        amend(&mut ed, "1");
        settle_until(&mut ed, |ed| amended(ed) == 1 || says(ed, "amend:"));
        assert!(ed.offers.is_empty() && ran() == "committed\n", "{} — {}", ran(), said(&ed));
        std::fs::remove_file(&marks).unwrap();
        // Changed since the base: the card names and shows it, and nothing has run
        hook(&wt, "pre-commit", "changed");
        let head = git_in(&wt, &["rev-parse", "HEAD"]);
        amend(&mut ed, "2");
        settle_until(&mut ed, |ed| card(ed, "Hooks before the amend").is_some() || says(ed, "amend:"));
        let q = card(&ed, "Hooks before the amend").unwrap().question.clone();
        assert!(
            q.starts_with(
                "This hook isn't as main has it:\n  .githooks/pre-commit — changed · this amend runs it\nIt runs as you"
            ),
            "{q}"
        );
        assert!(q.contains(".githooks/pre-commit:\n#!/bin/sh\necho changed >> "), "shown: {q}");
        assert!(
            git_in(&wt, &["rev-parse", "HEAD"]) == head && ran().is_empty(),
            "nothing ran before the answer"
        );
        assert_eq!(card_in_sight(&mut ed), ['y', 'n']);
        feed(&mut ed, "n");
        settle_until(&mut ed, |ed| amended(ed) == 2 || says(ed, "amend:"));
        assert!(git_in(&wt, &["rev-parse", "HEAD"]) != head, "amended: {}", said(&ed));
        assert_eq!(ran(), "", "n: without hooks");
        // An untracked one too — both named; y runs them
        hook(&wt, "commit-msg", "untracked");
        amend(&mut ed, "3");
        settle_until(&mut ed, |ed| card(ed, "Hooks before the amend").is_some() || says(ed, "amend:"));
        let q = card(&ed, "Hooks before the amend").unwrap().question.clone();
        assert!(
            q.contains(
                "  .githooks/commit-msg — untracked · this amend runs it\n  .githooks/pre-commit — changed · this amend runs it"
            ),
            "{q}"
        );
        // (taller than the room beside the chat: read whole first — `v`, to its end — as a script's card)
        card_in_sight(&mut ed);
        if !ed.offer_seen_whole() {
            feed(&mut ed, "v");
            let end = ed.end_key();
            feed(&mut ed, &end);
            card_in_sight(&mut ed);
        }
        feed(&mut ed, "y");
        settle_until(&mut ed, |ed| amended(ed) == 3 || says(ed, "amend:"));
        assert_eq!(ran(), "changed\nuntracked\n", "y: they ran — {}", said(&ed));
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Review 2026-10-04: what a pull runs depends on config — merging (pre-merge-commit, commit-msg …) or
    /// rebasing (pre-rebase, post-rewrite …) — so every file in the hooks folder is asked about, not a list by
    /// action: hooks no pull list names bring the card up, unmarked as "this pull runs it"; `n` pulls anyway.
    #[test]
    fn a_pull_asks_about_every_hook_whatever_config_runs() {
        use std::os::unix::fs::PermissionsExt as _;
        let (repo, dir) = place("pull-hooks");
        let origin = dir.join("origin");
        let out =
            std::process::Command::new("git").args(["clone", "-q"]).arg(&repo).arg(&origin).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        git_in(&repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git_in(&repo, &["fetch", "-q", "origin"]);
        git_in(&repo, &["branch", "-q", "-u", "origin/main"]);
        let marks = dir.join("hooks-ran");
        let hook = |name: &str| {
            let p = repo.join(".githooks").join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, format!("#!/bin/sh\necho {name} >> '{}'\n", marks.display())).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        };
        let up = |msg: &str| {
            git_in(
                &origin,
                &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", msg],
            );
            git_in(&origin, &["rev-parse", "HEAD"])
        };
        git_in(&repo, &["config", "core.hooksPath", ".githooks"]);
        let mut ed = editor(&repo, &dir);
        ed.git_refresh();
        settle_until(&mut ed, |ed| ed.session.git_status.is_some());
        let pulled = |ed: &Editor| ed.messages.iter().filter(|m| m.2.starts_with("Pulled")).count();
        for (k, (config, name)) in
            [("false", "pre-merge-commit"), ("true", "pre-rebase")].into_iter().enumerate()
        {
            git_in(&repo, &["config", "pull.rebase", config]);
            hook(name);
            let tip = up(&format!("up {k}"));
            ed.git_push_pull(false);
            settle_until(&mut ed, |ed| card(ed, "Hooks before the pull").is_some() || says(ed, "pull:"));
            let q = card(&ed, "Hooks before the pull").unwrap().question.clone();
            let row = format!("  .githooks/{name} — untracked\n");
            assert!(q.contains(&row), "{name} asked about, not marked as the pull's own: {q}");
            card_in_sight(&mut ed);
            if !ed.offer_seen_whole() {
                feed(&mut ed, "v");
                let end = ed.end_key();
                feed(&mut ed, &end);
                card_in_sight(&mut ed);
            }
            feed(&mut ed, "n");
            settle_until(&mut ed, |ed| pulled(ed) == k + 1 || says(ed, "pull:"));
            assert_eq!(git_in(&repo, &["rev-parse", "HEAD"]), tip, "pulled: {}", said(&ed));
            assert!(!marks.exists(), "a hook ran: {}", std::fs::read_to_string(&marks).unwrap_or_default());
        }
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A review against a remote branch fetches it first — tarae's own step, so with no hooks either.
    #[test]
    fn a_reviews_fetch_runs_no_hooks() {
        let (repo, dir) = place("review-fetch");
        let origin = dir.join("origin");
        let out =
            std::process::Command::new("git").args(["clone", "-q"]).arg(&repo).arg(&origin).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        git_in(&repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git_in(&repo, &["fetch", "-q", "origin"]);
        git_in(
            &origin,
            &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "up"],
        );
        let up = git_in(&origin, &["rev-parse", "HEAD"]);
        let marks = dir.join("hooks-ran");
        plant_hooks(&repo, &marks);
        let mut ed = editor(&repo, &dir);
        feed(&mut ed, ":review origin/main<ret>");
        // (past its step's own "gathering…": no changes, an error, or the review)
        let after = |ed: &Editor| ed.messages.iter().filter(|m| m.2.starts_with("review:")).count() > 1;
        settle_until(&mut ed, |ed| after(ed) || ed.session.code_review.is_some());
        assert_eq!(git_in(&repo, &["rev-parse", "origin/main"]), up, "fetched: {}", said(&ed));
        assert!(!marks.exists(), "a hook ran: {}", std::fs::read_to_string(&marks).unwrap_or_default());
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The rebase's git closed while it runs (`space m c`): what git left is read — the session doesn't say
    /// `rebasing` for good with nothing running — and the rebase can still be ended.
    #[test]
    fn the_rebases_git_closed_mid_run_is_read_as_left() {
        let (repo, dir) = place("rebase-closed");
        let mut ed = editor(&repo, &dir);
        claude_does(&mut ed, &dir.join("claude.log"), "");
        let (wt, tip) = moved_apart(&mut ed, &repo, false);
        rebase_asked(&mut ed);
        settle_until(&mut ed, |ed| says(ed, "conflict markers still in") || says(ed, "rebase feat:"));
        feed(&mut ed, ":rebase-abort<ret>");
        assert!(matches!(ed.session.rebase.as_ref().unwrap().stage, crate::rebase::Stage::Running { .. }));
        ed.task_close();
        settle_until(&mut ed, |ed| ed.session.rebase.as_ref().is_none_or(|r| r.stopped()));
        assert!(says(&ed, "the rebase's git was stopped — it's read as git left it"), "{}", said(&ed));
        if ed.session.rebase.is_some() {
            feed(&mut ed, ":rebase-abort<ret>");
            settle_until(&mut ed, |ed| says(ed, "Rebase aborted") || says(ed, "rebase feat:"));
        }
        assert_eq!(git_in(&wt, &["rev-parse", "HEAD"]), tip, "{}", said(&ed));
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A rebase tarae didn't start (by hand in the session's terminal, or from before a restart): bring
    /// takes it up and puts up its card (#175: it only said to type `:rebase-continue`) — the file unmerged,
    /// `y` going on, `X` aborting it.
    #[test]
    fn a_rebase_by_hand_is_taken_up() {
        let (repo, dir) = place("rebase-hand");
        let mut ed = editor(&repo, &dir);
        let (wt, tip) = moved_apart(&mut ed, &repo, false);
        let out =
            std::process::Command::new("git").arg("-C").arg(&wt).args(["rebase", "main"]).output().unwrap();
        assert!(!out.status.success(), "stopped at the conflict");
        feed(&mut ed, ":session-bring<ret>");
        settle_until(&mut ed, |ed| card(ed, "feat: rebasing 1/2").is_some() || says(ed, "bring feat:"));
        let q = &card(&ed, "feat: rebasing 1/2").unwrap().question;
        assert!(q.starts_with("Rebasing feat onto main stopped at commit 1 of 2 ("), "{q}");
        assert!(
            q.ends_with(
                " feat a). a.txt is unmerged: y stages it once no conflict markers are left, and goes on.\n\n\
                 X aborts the rebase: feat goes back as it was."
            ),
            "{q}"
        );
        let r = ed.session.rebase.as_ref().expect("taken up");
        assert_eq!((r.branch.as_str(), r.stop.files.clone()), ("feat", vec!["a.txt".to_string()]));
        assert_eq!(card_in_sight(&mut ed), ['y', 'X', 'n']);
        feed(&mut ed, "X");
        settle_until(&mut ed, |ed| says(ed, "Rebase aborted") || says(ed, "rebase feat:"));
        assert_eq!(git_in(&wt, &["rev-parse", "HEAD"]), tip, "{}", said(&ed));
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
        let blocks = snapshot["sessions"]["sessions"].as_array_mut().unwrap();
        let block = blocks.iter_mut().find(|b| b["root"] == json!(one)).unwrap();
        (block["docs"], block["current"]) = (json!([{ "path": one.join("b.txt") }]), json!(0));
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

    /// A session ended and opened again in the same run (an end that failed opens it again) comes back as it
    /// was when it ended — not as the startup file had it: a note removed since stays removed, its turns
    /// come back once.
    #[test]
    fn a_session_opened_again_is_as_it_ended() {
        use crate::checkpoint::{Checkpoint, Touched};
        let dir = std::env::temp_dir().join(format!("tarae-sessions-again-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("back")).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        let (back, store) = (dir.join("back"), dir.join("turns"));
        let file = back.join("a.txt");
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let mut ed = editor(&dir, &dir);
        ed.turns_dir = Some(store.clone());
        // At startup the restore file had two notes for it
        let note = |line: usize, text: &str| json!({ "path": file, "lines": [line, line + 1], "thread": [["claude", text, false]] });
        let notes = json!({ back.to_str().unwrap(): [note(0, "keep me"), note(1, "remove me")] });
        ed.restore = crate::restore::State::from_json(json!({ "notes": notes }));
        let id = ed.session_add(back.clone());
        let turns_back = |ed: &mut Editor, id| {
            settle_until(ed, |ed| {
                std::iter::once(&ed.session)
                    .chain(&ed.sessions)
                    .any(|l| l.id == id && l.checkpoints.dir.is_some())
            })
        };
        turns_back(&mut ed, id);
        // A turn (not written yet), and one note removed
        ed.with_session(id, |ed| {
            let (before, after) = (Some(ropey::Rope::from_str("one\n")), ropey::Rope::from_str("two\n"));
            let t = Touched { path: file.clone(), before, after, ..Default::default() };
            ed.session.checkpoints.list.push(Checkpoint::new("a turn".into(), vec![t]));
            ed.session.notes.list.retain(|n| n.gist() != "remove me");
        });
        // Ended: what it had is written as it goes — then back (as a failed end brings it back)
        let kept = ed.session_close(&back).expect("closed");
        assert!(ed.sessions.is_empty());
        let end = Instant::now() + Duration::from_secs(5);
        while crate::checkpoint::load(&store, &back).list.is_empty() {
            assert!(Instant::now() < end, "its turns written as it ended");
            std::thread::sleep(Duration::from_millis(10));
        }
        ed.sessions_back(vec![kept], Vec::new(), None, false);
        let id = ed.sessions[0].id;
        turns_back(&mut ed, id);
        let l = &ed.sessions[0];
        let gists: Vec<&str> = l.notes.list.iter().map(|n| n.gist()).collect();
        assert_eq!(gists, ["keep me"], "the removed note stays removed");
        let turns: Vec<&str> = l.checkpoints.list.iter().map(|c| c.question.as_str()).collect();
        assert_eq!(turns, ["a turn"], "once");
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
            rebasing: None,
            reloaded: 0,
            claude: None,
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
        // Its Claude's cache and cost so far: a column each, the costs right-aligned
        let claude = |cache: &str, cost: &str| Some((cache.to_string(), cost.to_string()));
        let rows = items(&[
            Shown {
                claude: claude("● warm until 14:32", "$0.020"),
                ..shown(0, "tarae", None, None, Doing::Idle, None)
            },
            Shown {
                claude: claude("◦ cold", "$1.23"), ..shown(1, "fix", None, None, Doing::Working, None)
            },
            shown(2, "parser", None, None, Doing::Idle, None),
        ]);
        let hints: Vec<&str> = rows.iter().map(|i| i.hint.as_str()).collect();
        assert_eq!(
            hints,
            [
                "here     ● warm until 14:32  $0.020",
                &format!("working  ◦ cold{}$1.23", " ".repeat(15)),
                &" ".repeat(35),
            ]
        );
    }

    /// The column from a session's usage: its cache, warm until when (by the clock — true however long
    /// nothing redraws) or cold, and its total — a session whose Claude reported nothing has none.
    #[test]
    fn the_list_says_each_sessions_cache_and_cost() {
        use crate::usage::{Counts, Seen, Spent, Ttl, Usage};
        let mut u = Usage::default();
        assert_eq!(Shown::claude_of(&u, 0), None);
        u.fresh("c");
        let usage = serde_json::json!({ "cache_creation_input_tokens": 9,
                                        "cache_creation": { "ephemeral_5m_input_tokens": 9 } });
        u.requesting(1_000);
        u.seen(Seen {
            id: Some("m".into()),
            counts: Counts::of(&usage),
            ttl: Ttl::of(&usage),
            ..Seen::default()
        });
        u.spent(Spent { conversation: "c".into(), total_cost: Some(0.02), at_ms: 2_000, ..Spent::default() });
        // (warm until five minutes after the request went out, whenever it's asked)
        let at = format!("● warm until {}", crate::usage::until(1_000 + Ttl::Min5.ms()));
        // (the cost said as what it is: the API price — #161)
        let cost = "≈ API $0.020".to_string();
        assert_eq!(Shown::claude_of(&u, 61_000), Some((at.clone(), cost.clone())));
        assert_eq!(Shown::claude_of(&u, 290_000), Some((at, cost.clone())));
        assert_eq!(Shown::claude_of(&u, 1_000 + Ttl::Min5.ms()), Some(("◦ cold".into(), cost)));
    }

    /// #111: a build-dir set per checkout (`{workspace-path-hash}`) — found where cargo looks: its variable,
    /// the nearest `.cargo/config.toml` up from the checkout, then the cargo home's — makes the card say the
    /// new checkout builds cold in its own, and what this one's weighs; a fixed one is still said shared.
    #[test]
    fn the_new_session_card_says_where_its_builds_go() {
        let dir = std::env::temp_dir().join(format!("tarae-builddir-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let (top, home) = (dir.join("work/proj"), dir.join("cargo-home"));
        std::fs::create_dir_all(top.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join("work/.cargo")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let hashed = "[build]\nbuild-dir = \"{cargo-cache-home}/build/{workspace-path-hash}\"\n";
        std::fs::write(home.join("config.toml"), hashed).unwrap();
        let at = |env: Option<&str>| build_dir_setting(&top.join("sub"), Some(&home), env.map(String::from));
        assert_eq!(at(None).as_deref(), Some("{cargo-cache-home}/build/{workspace-path-hash}"), "the home's");
        std::fs::write(dir.join("work/.cargo/config.toml"), "[build]\nbuild-dir = \"/b/shared\"\n").unwrap();
        assert_eq!(at(None).as_deref(), Some("/b/shared"), "the nearest config wins");
        assert_eq!(
            at(Some("{workspace-root}/b")).as_deref(),
            Some("{workspace-root}/b"),
            "the variable first"
        );
        assert!(
            per_checkout("{cargo-cache-home}/build/{workspace-path-hash}")
                && per_checkout("{workspace-root}/x")
        );
        assert!(!per_checkout("/b/shared"));
        // The card's words
        let making = |build_own| Making {
            top: top.clone(),
            branch: "fix".into(),
            base: "main".into(),
            base_sha: "a".repeat(40),
            path: dir.join("checkouts/fix"),
            files: (1 << 20, true),
            big: Vec::new(),
            cargo: Some((top.join("target"), home.join("build/1a/2b3c"))),
            build_own,
            copies: Vec::new(),
            others: Vec::new(),
        };
        let q =
            making(Some((3 << 30, true))).card().question.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(q.contains("Cargo builds it in a build-dir of its own in"), "{q}");
        assert!(
            q.contains("its first build starts cold — this checkout's is 3.0 GB") && !q.contains("shared"),
            "{q}"
        );
        let q = making(Some((0, true))).card().question.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(q.contains("its first build starts cold.") && !q.contains("0 B"), "none built here yet: {q}");
        let q = making(None).card().question.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(q.contains("build-dir is shared") && !q.contains("cold"), "{q}");
        std::fs::remove_dir_all(&dir).ok();
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
