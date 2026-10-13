//! A run's changes, undoable (card model P5, `docs/design/cards.md` §3.4): before each run of Claude's — a
//! check or its tests too, not a command that only reads — the checkout's working tree is taken as a git tree; when the run ends it's taken again, and
//! what changed between the two — deletions included — joins the turn's checkpoint, as written unseen: the
//! turn ends in its review, and `space u` takes it back (`checkpoint.rs`).
//!
//! A tree is `git add -A` into tarae's own index for the checkout (`INDEX` in its git folder, first made from
//! a copy of the real one), then `write-tree`. The real index is never touched and no hook runs. The index is
//! kept between snapshots, so only files whose stat changed since are hashed again — untracked ones too (a
//! fresh copy of the real index hashes every untracked file every time). Ignored files aren't in a tree:
//! build output isn't kept, and a run's change to an ignored file isn't taken back — one it deleted is named
//! in Claude's report (`ignored`), no more (card model P3 decides the rest). More than `NEW_MAX` new
//! files at once aren't either (a tree of 200k untracked files: 91 s and 800 MB the first time). On workers,
//! unsandboxed — tarae's own git, not Claude's. "Since your last y" (`sinceyes.rs`) takes its trees here too.

use std::io::{BufRead as _, Read as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// tarae's index for the checkout's snapshots, in its git folder.
const INDEX: &str = "tarae-snapshot.index";

/// A file larger than this (either side) is kept by its blob, not as text.
const TEXT_MAX: u64 = 1 << 20;

/// A temporary index's name is unique per process with this.
static INDEX_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One snapshot per index at a time (two at once: git refuses the second its lock).
static LOCKS: Mutex<Vec<(PathBuf, Arc<Mutex<()>>)>> = Mutex::new(Vec::new());

/// git as tarae runs it here: on `root`'s repository (`gitrepo`), in `root`, no hooks, the index `index` if
/// given.
pub(crate) fn git(root: &Path, args: &[&str], index: Option<&Path>) -> Result<String, String> {
    git_bytes(root, args, index).map(|b| String::from_utf8_lossy(&b).into_owned())
}

fn command(root: &Path, args: &[&str], index: Option<&Path>) -> Result<std::process::Command, String> {
    let all: Vec<&str> = crate::gitrepo::NO_HOOKS.iter().chain(args).copied().collect();
    let mut cmd = crate::gitrepo::repo(root)?.command(&all);
    if let Some(i) = index {
        cmd.env("GIT_INDEX_FILE", i);
    }
    Ok(cmd)
}

fn git_bytes(root: &Path, args: &[&str], index: Option<&Path>) -> Result<Vec<u8>, String> {
    crate::gitrepo::finish(&command(root, args, index)?, None)
}

/// The project at `root`'s working tree as a git tree (what `git add -A` would stage, ignored files left
/// out) — staged into tarae's own index (see the module's doc). Err says why not.
pub fn tree(root: &Path) -> Result<String, String> {
    snap(root).map(|t| t.id)
}

/// A tree taken: its id, and how many new files it left out (more than `NEW_MAX` at once — then it has the
/// files it had before, as they are now, and none of the new ones).
#[derive(Clone, Debug, PartialEq)]
pub struct Tree {
    pub id: String,
    pub left_out: usize,
}

/// New files a snapshot keeps at most at once (not ignored, not in tarae's index yet). Measured 2026-10-05
/// (`target/sb/p5-checkpoint.md`): 200k untracked files took 91 s and 800 MB of git's objects the first
/// time — past this a snapshot keeps what it had (`add -u`: stats, ~0.4 s at 200k) and says so.
const NEW_MAX: usize = 5_000;

/// The checkout's git folder (`gitrepo` keeps it once resolved: a process saved per snapshot).
fn git_dir(root: &Path) -> Result<PathBuf, String> {
    crate::gitrepo::repo(root)
        .map(|r| r.git_dir.clone())
        .map_err(|_| "not a git checkout, so what changed can't be told".to_string())
}

/// `tree`, with how many new files it left out.
pub fn snap(root: &Path) -> Result<Tree, String> {
    let dir = git_dir(root)?;
    let index = dir.join(INDEX);
    let lock = {
        let mut locks = LOCKS.lock().unwrap_or_else(|e| e.into_inner());
        match locks.iter().find(|(p, _)| *p == index) {
            Some((_, l)) => l.clone(),
            None => {
                let l = Arc::new(Mutex::new(()));
                locks.push((index.clone(), l.clone()));
                l
            }
        }
    };
    let _one = lock.lock().unwrap_or_else(|e| e.into_inner());
    if !index.exists() {
        seed(&dir, &index)?;
    }
    match stage(root, &index) {
        Ok(id) => Ok(id),
        // (another tarae's snapshot holds it: one of its own in the temp folder, as before there was one kept)
        Err(e) if e.contains(".lock") => {
            let seq = INDEX_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let temp = std::env::temp_dir().join(format!("tarae-snap-{}-{seq}.index", std::process::id()));
            let id = seed(&dir, &temp).and_then(|_| stage(root, &temp));
            std::fs::remove_file(&temp).ok();
            id
        }
        // (ours unreadable — a git that changed its format, a cut write: made again once)
        Err(_) => {
            std::fs::remove_file(&index).ok();
            seed(&dir, &index)?;
            stage(root, &index)
        }
    }
}

/// `to` as a copy of the checkout's own index (none yet: nothing to copy) — only what changed since it is
/// hashed by the first snapshot.
fn seed(git_dir: &Path, to: &Path) -> Result<(), String> {
    match std::fs::copy(git_dir.join("index"), to) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!("git's index: {e}")),
        _ => Ok(()),
    }
}

fn stage(root: &Path, index: &Path) -> Result<Tree, String> {
    // (new to this index: none past `NEW_MAX` hashed and written)
    let new = git_bytes(root, &["ls-files", "-z", "--others", "--exclude-standard", "--", "."], Some(index))?;
    let new = new.split(|&b| b == 0).filter(|f| !f.is_empty()).count();
    let all = new <= NEW_MAX;
    // (a save's temp file isn't the run's: left out)
    git(root, &["add", if all { "-A" } else { "-u" }, "--", ".", crate::disk::NOT_TEMP], Some(index))?;
    let id = git(root, &["write-tree"], Some(index))?.trim().to_string();
    Ok(Tree { id, left_out: if all { 0 } else { new } })
}

/// One side of a file a run changed: its blob, and its text when both sides are text (`changes`).
#[derive(Clone, Debug, PartialEq)]
pub struct Blob {
    pub id: String,
    pub text: Option<String>,
}

/// A file a run changed (None: no file on that side).
#[derive(Clone, Debug, PartialEq)]
pub struct Change {
    pub path: PathBuf,
    pub before: Option<Blob>,
    pub after: Option<Blob>,
}

impl Change {
    /// Both sides are text (or no file): taken back as text — the user's edits since kept (`checkpoint::step`).
    pub fn text(&self) -> bool {
        [&self.before, &self.after].iter().all(|s| s.as_ref().is_none_or(|b| b.text.is_some()))
    }
}

/// What changed in the project at `root` from tree `then` to `now`: the files (absolute paths), and those it
/// can't take back, named (a symlink, a submodule, only the mode changed).
pub fn changes(root: &Path, then: &str, now: &str) -> Result<(Vec<Change>, Vec<String>), String> {
    let mut files: Vec<(String, Option<String>, Option<String>)> = Vec::new();
    let mut skipped = Vec::new();
    for Entry { path, modes, ids: (a, b) } in entries(root, then, now)? {
        let regular = |m: &str| m == "000000" || m.starts_with("100");
        if !regular(&modes.0) || !regular(&modes.1) {
            skipped.push(format!("{path} (a link or a submodule)"));
            continue;
        }
        if a == b {
            skipped.push(format!("{path} (its mode)"));
            continue;
        }
        files.push((path, a, b));
    }
    let ids: Vec<&str> = files.iter().flat_map(|(_, a, b)| [a, b]).flatten().map(String::as_str).collect();
    let texts = texts(root, &ids)?;
    let blob = |id: &Option<String>| {
        id.as_ref().map(|id| Blob {
            id: id.clone(),
            text: texts.iter().find(|(i, _)| i == id).map(|(_, t)| t.clone()),
        })
    };
    let changes = files
        .iter()
        .map(|(path, a, b)| {
            let mut c = Change { path: root.join(path), before: blob(a), after: blob(b) };
            // (one side text, the other not: both kept as blobs)
            if !c.text() {
                for s in [&mut c.before, &mut c.after].into_iter().flatten() {
                    s.text = None;
                }
            }
            c
        })
        .collect();
    Ok((changes, skipped))
}

/// One path that differs between two trees (or commits): its modes and blobs then and now (None: not there).
struct Entry {
    path: String,
    modes: (String, String),
    ids: (Option<String>, Option<String>),
}

/// What differs from tree `then` to `now` in the project at `root` (`diff-tree`, paths from `root`).
fn entries(root: &Path, then: &str, now: &str) -> Result<Vec<Entry>, String> {
    let raw = git_bytes(root, &["diff-tree", "-r", "-z", "--no-renames", "--relative", then, now], None)?;
    let mut fields = raw.split(|&b| b == 0).filter(|f| !f.is_empty());
    let mut out = Vec::new();
    // (`:100644 100644 <old> <new> M` then the path)
    while let (Some(head), Some(path)) = (fields.next(), fields.next()) {
        let head = String::from_utf8_lossy(head);
        let [mode_a, mode_b, a, b, _status] = head.trim_start_matches(':').split(' ').collect::<Vec<_>>()[..]
        else {
            continue;
        };
        let side = |id: &str| (!id.bytes().all(|c| c == b'0')).then(|| id.to_string());
        out.push(Entry {
            path: String::from_utf8_lossy(path).into_owned(),
            modes: (mode_a.to_string(), mode_b.to_string()),
            ids: (side(a), side(b)),
        });
    }
    Ok(out)
}

/// The commit HEAD names in the checkout at `root` — None: none yet (an unborn branch), or git can't say.
fn head(root: &Path) -> Option<String> {
    let id = git(root, &["rev-parse", "--verify", "-q", "HEAD^{commit}"], None).ok()?;
    Some(id.trim().to_string()).filter(|id| !id.is_empty())
}

/// The commits the checkout's refs name (branches, remote branches, tags) — None: git can't say.
fn tips(root: &Path) -> Option<Vec<String>> {
    let out = git(root, &["for-each-ref", "--format=%(objectname)"], None).ok()?;
    Some(out.lines().map(str::to_string).collect())
}

/// HEAD moved while a run ran (a merge, a pull, a checkout, a reset, a commit): from which commit to which,
/// whether it was a pure fast-forward (`came`: every commit between already there before the run — the only
/// move whose files leave the turn), how many files that left out (#225: a `git merge --ff-only main` was
/// "changed 90 files" in the turn's review), and how many commits the run made itself (`own`). Any other move
/// keeps every file the turn's, as before #225, and is only said (user decision 2026-10-14: small and safe).
#[derive(Clone, Debug, PartialEq)]
pub struct HeadMove {
    pub from: String,
    pub to: String,
    /// `to`'s short form (`rev-parse --short`).
    pub short: String,
    /// A pure fast-forward: forward, every commit between one a ref had before the run.
    pub came: bool,
    /// Forward from where it was (`from` is `to`'s ancestor).
    pub forward: bool,
    /// The files that came with a pure fast-forward — left out of the turn.
    pub files: usize,
    /// The commits the run made (none a ref had before it).
    pub own: usize,
}

impl HeadMove {
    /// `HEAD moved: fast-forward to 1a2b3c4` (its files left out) — every other move keeps its files:
    /// `HEAD moved: a commit made by this run — its files stay in this turn` · `HEAD moved to 1a2b3c4 — not a
    /// fast-forward; its files stay in this turn` · `HEAD moved to 1a2b3c4 — its files stay in this turn`
    /// (forward, but the refs before couldn't be read).
    pub fn said(&self) -> String {
        let short = &self.short;
        match self.own {
            _ if self.came => format!("HEAD moved: fast-forward to {short}"),
            0 if !self.forward => {
                format!("HEAD moved to {short} — not a fast-forward; its files stay in this turn")
            }
            0 => format!("HEAD moved to {short} — its files stay in this turn"),
            1 => "HEAD moved: a commit made by this run — its files stay in this turn".to_string(),
            n => format!("HEAD moved: {n} commits made by this run — its files stay in this turn"),
        }
    }

    /// Which move, in a few words: `fast-forward` · `made by this run` · `not a fast-forward` · `moved`.
    pub fn kind(&self) -> &'static str {
        match self.own {
            _ if self.came => "fast-forward",
            0 if !self.forward => "not a fast-forward",
            0 => "moved",
            _ => "made by this run",
        }
    }

    /// As a turn keeps it (`checkpoint.rs`).
    pub fn to_json(&self) -> serde_json::Value {
        let (from, to, short) = (&self.from, &self.to, &self.short);
        let (came, forward, files, own) = (self.came, self.forward, self.files, self.own);
        serde_json::json!({ "from": from, "to": to, "short": short, "came": came, "forward": forward,
            "files": files, "own": own })
    }

    pub fn from_json(v: &serde_json::Value) -> Option<HeadMove> {
        let s = |k: &str| v[k].as_str().map(str::to_string);
        Some(HeadMove {
            from: s("from")?,
            to: s("to")?,
            short: s("short")?,
            came: v["came"].as_bool()?,
            forward: v["forward"].as_bool()?,
            files: v["files"].as_u64()? as usize,
            own: v["own"].as_u64()? as usize,
        })
    }
}

/// HEAD's move from `from` to `to` at `root`. Only a pure fast-forward — `to` a descendant of `from`, and
/// every commit between one a ref had before the run (`tips`: `Before.refs`) — takes files out of `files`:
/// one whose blobs then and now are the two commits' came with it; one the run changed over what it brought
/// stays, from what it brought (`space u` takes back the run's part — the file stays as the new HEAD has it).
/// Every other move keeps every file (user decision 2026-10-14, after review findings): a commit the run made
/// (an edit committed, a merge, a rebase — Claude's work, or a mix no blob can split), a checkout or reset
/// backward or sideways, a fast-forward the refs before can't vouch for (`tips` unknown).
fn head_moved(
    root: &Path,
    from: &str,
    to: &str,
    tips: Option<&[String]>,
    files: &mut Vec<Change>,
) -> Result<HeadMove, String> {
    let forward = git(root, &["merge-base", "--is-ancestor", from, to], None).is_ok();
    // (the commits between no ref had before the run — the run's own; None: can't tell)
    let own = match tips {
        Some(tips) => {
            let not: String = tips.iter().map(|t| format!("^{t}\n")).collect();
            let args = ["rev-list", "--stdin", to, &format!("^{from}")];
            let out = crate::gitrepo::finish(&command(root, &args, None)?, Some(not.as_bytes()))?;
            Some(String::from_utf8_lossy(&out).lines().count())
        }
        None => None,
    };
    let came = forward && own == Some(0);
    let moved = if came { entries(root, from, to)? } else { Vec::new() };
    let id = |b: &Option<Blob>| b.as_ref().map(|b| b.id.clone());
    let of = |c: &Change| moved.iter().find(|e| root.join(&e.path) == c.path && e.ids.0 == id(&c.before));
    let before = files.len();
    files.retain(|c| of(c).is_none_or(|e| e.ids.1 != id(&c.after)));
    let brought = before - files.len();
    // (the run's change over the move's: its before is the move's — both sides' texts read again, as `changes`)
    let over: Vec<(usize, Option<String>)> =
        files.iter().enumerate().filter_map(|(i, c)| Some((i, of(c)?.ids.1.clone()))).collect();
    let ids: Vec<String> =
        over.iter().flat_map(|(i, b)| [b.clone(), id(&files[*i].after)]).flatten().collect();
    let texts = texts(root, &ids.iter().map(String::as_str).collect::<Vec<_>>())?;
    let blob = |id: String| Blob { text: texts.iter().find(|(i, _)| *i == id).map(|(_, t)| t.clone()), id };
    for (i, b) in over {
        let c = &mut files[i];
        (c.before, c.after) = (b.map(blob), id(&c.after).map(blob));
        if !c.text() {
            for s in [&mut c.before, &mut c.after].into_iter().flatten() {
                s.text = None;
            }
        }
    }
    let short = git(root, &["rev-parse", "--short", to], None)
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| to.chars().take(7).collect());
    let (from, to, own) = (from.into(), to.into(), own.unwrap_or(0));
    Ok(HeadMove { from, to, short, came, forward, files: brought, own })
}

/// The blobs among `ids` that are text (UTF-8, no NUL, at most `TEXT_MAX`), with their text — sizes first
/// (`--batch-check`), then only the small ones' contents (`--batch`), each one `cat-file`.
fn texts(root: &Path, ids: &[&str]) -> Result<Vec<(String, String)>, String> {
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let sizes = batch(root, "--batch-check", ids)?;
    let mut small = Vec::new();
    for line in String::from_utf8_lossy(&sizes).lines() {
        // (`<id> blob <size>` · `<id> missing`)
        let w: Vec<&str> = line.split(' ').collect();
        if let [id, "blob", size] = w[..]
            && size.parse::<u64>().is_ok_and(|s| s <= TEXT_MAX)
            && !small.contains(&id.to_string())
        {
            small.push(id.to_string());
        }
    }
    if small.is_empty() {
        return Ok(Vec::new());
    }
    let ids: Vec<&str> = small.iter().map(String::as_str).collect();
    let out = batch(root, "--batch", &ids)?;
    let mut r = std::io::Cursor::new(out);
    let mut got = Vec::new();
    let mut head = String::new();
    loop {
        head.clear();
        if r.read_line(&mut head).map_err(|e| e.to_string())? == 0 {
            break;
        }
        let w: Vec<&str> = head.trim_end().split(' ').collect();
        let [id, _, size] = w[..] else { continue };
        let size: usize = size.parse().map_err(|_| format!("git cat-file said {head:?}"))?;
        let mut body = vec![0; size + 1];
        r.read_exact(&mut body).map_err(|e| e.to_string())?;
        body.pop();
        if !body.contains(&0)
            && let Ok(text) = String::from_utf8(body)
        {
            got.push((id.to_string(), text));
        }
    }
    Ok(got)
}

/// `git cat-file <mode>` fed `ids` (`gitrepo::ran` writes them from a thread of its own: a big answer can't
/// block the asking).
fn batch(root: &Path, mode: &str, ids: &[&str]) -> Result<Vec<u8>, String> {
    let input: String = ids.iter().map(|i| format!("{i}\n")).collect();
    let ran = crate::gitrepo::ran(&command(root, &["cat-file", mode], None)?, Some(input.as_bytes()))?;
    if !ran.ok {
        return Err(format!("git cat-file {mode} failed"));
    }
    Ok(ran.stdout)
}

/// Blob `id` written out at `path` (its folder made) — None: `path` removed. Only while `path` is still
/// `expect` (a blob id; None: no file): a file changed since is left as it is (Err says so).
pub fn put(root: &Path, path: &Path, expect: Option<&str>, id: Option<&str>) -> Result<(), String> {
    let shown = crate::chat::shown(root, path);
    let now = match path.exists() {
        false => None,
        true => {
            let p = path.to_string_lossy();
            // (as `add` hashed it: through the checkout's filters)
            Some(git(root, &["hash-object", "--", &p], None)?.trim().to_string())
        }
    };
    if now.as_deref() != expect {
        return Err(format!("{shown} changed since — left as it is"));
    }
    match id {
        None => std::fs::remove_file(path).map_err(|e| format!("{shown}: {e}")),
        Some(id) => {
            let bytes = git_bytes(root, &["cat-file", "blob", id], None)?;
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir).map_err(|e| format!("{shown}: {e}"))?;
            }
            std::fs::write(path, bytes).map_err(|e| format!("{shown}: {e}"))
        }
    }
}

/// The checkout as a run found it (`tools::go`): its tree, or why there's none — and the files the editor
/// wrote while it ran (`Editor::wrote`: the user's saves, Claude's own edits), which aren't the run's.
#[derive(Clone, Debug)]
pub struct Before {
    pub root: PathBuf,
    pub tree: Result<Tree, String>,
    pub wrote: Vec<PathBuf>,
    /// The ignored files and folders there were (`ignored`) — None: git couldn't say.
    pub ignored: Option<Vec<String>>,
    /// The commit HEAD named (`head`).
    pub head: Option<String>,
    /// The commits every ref named (`tips`) — what a move of HEAD can have come from (`head_moved`). None:
    /// git couldn't say.
    pub refs: Option<Vec<String>>,
}

/// What a run changed (`Before::since`): the files, those it can't take back (named — `changes`), how many
/// new files weren't kept, before or after it (`NEW_MAX`), the ignored files and folders it deleted —
/// named only: no tree has them, so nothing takes them back — and the protected names it made
/// (`protected_made`) — and HEAD's move, with the files it brought left out of `files` (`HeadMove`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Found {
    pub files: Vec<Change>,
    pub skipped: Vec<String>,
    pub left_out: usize,
    pub ignored_gone: Vec<String>,
    pub protected: Vec<String>,
    pub head: Option<HeadMove>,
}

/// The protected names (`sandbox::protected_in`: not below `target/` or `node_modules/`) a run made where
/// none was, from the project's root: files new in the tree, and ignored files and folders new beside them
/// (`.envrc` and `.tarae.local.toml` are often ignored). Seatbelt refuses them by name; Landlock can't (a
/// path that isn't there has no rule) — so they're said before the next run (`tools::ask_made`).
fn protected_made(root: &Path, files: &[Change], ignored_new: &[String]) -> Vec<String> {
    let new_files =
        files.iter().filter(|c| c.before.is_none()).filter_map(|c| c.path.strip_prefix(root).ok());
    let ignored = ignored_new.iter().map(|p| Path::new(p.trim_end_matches('/')));
    new_files
        .chain(ignored)
        .filter(|rel| crate::sandbox::protected_in(root, &root.join(rel)))
        .map(|rel| rel.to_string_lossy().into_owned())
        .collect()
}

/// The project's ignored files and folders, from its root (`.env`, `target/`): an ignored folder is one
/// entry, never walked (`--directory`) — a file deleted inside one isn't seen, the folder going is.
pub fn ignored(root: &Path) -> Result<Vec<String>, String> {
    let args = ["ls-files", "-z", "--others", "--ignored", "--exclude-standard", "--directory", "--", "."];
    let out = git_bytes(root, &args, None)?;
    Ok(out
        .split(|&b| b == 0)
        .filter(|f| !f.is_empty())
        .map(|f| String::from_utf8_lossy(f).into_owned())
        .collect())
}

impl Before {
    /// Taken now (on a worker).
    pub fn take(root: &Path) -> Before {
        let ignored = ignored(root).ok();
        let (head, refs) = (head(root), tips(root));
        Before { root: root.to_path_buf(), tree: snap(root), wrote: Vec::new(), ignored, head, refs }
    }

    /// What the run changed: the checkout taken again and compared (on a worker) — Err: it can't tell.
    pub fn since(&self) -> Result<Found, String> {
        let then = self.tree.as_ref().map_err(String::clone)?;
        let now = snap(&self.root)?;
        let left_out = then.left_out.max(now.left_out);
        let (ignored_gone, ignored_new) = match (&self.ignored, ignored(&self.root)) {
            (Some(was), Ok(is)) => {
                let set = |v: &[String]| v.iter().cloned().collect::<std::collections::HashSet<String>>();
                let (was_set, is_set) = (set(was), set(&is));
                let gone =
                    was.iter().filter(|p| !is_set.contains(*p) && !self.root.join(p).exists()).cloned();
                (gone.collect(), is.into_iter().filter(|p| !was_set.contains(p)).collect())
            }
            _ => (Vec::new(), Vec::new()),
        };
        let (mut files, skipped) = match then.id == now.id {
            true => (Vec::new(), Vec::new()),
            false => changes(&self.root, &then.id, &now.id)?,
        };
        files.retain(|c| !self.wrote.contains(&c.path));
        let ignored_new: Vec<String> = ignored_new
            .into_iter()
            .filter(|p| !self.wrote.contains(&self.root.join(p.trim_end_matches('/'))))
            .collect();
        // (a protected name HEAD's move brought is asked about all the same: it runs later outside the sandbox)
        let protected = protected_made(&self.root, &files, &ignored_new);
        // (HEAD moved: what came with it isn't the run's — said instead; can't tell — all of it stays)
        let head = match (&self.head, head(&self.root)) {
            (Some(from), Some(to)) if *from != to => {
                head_moved(&self.root, from, &to, self.refs.as_deref(), &mut files).ok()
            }
            _ => None,
        };
        Ok(Found { files, skipped, left_out, ignored_gone, protected, head })
    }
}

#[cfg(test)]
mod tests;
