//! The `commit` tool (sandbox UX piece H, user decision 2026-10-16): Claude asks to commit the files it names
//! with a message; a card shows the message and the diff of exactly those paths, and only the user's `y`
//! commits them. Never a push, an amend, or anything not named.
//!
//! - Asked (a worker's): the paths judged first — inside the project and the checkout by their folders' real
//!   paths (a link itself is committed as a link), files only (a folder, git's own folder, another repository
//!   inside, an ignored untracked file, a path with no change from HEAD each refuse the whole call, named) — and
//!   nothing under way that a commit would cut short (a rebase, a merge, a cherry-pick or revert, a conflicted
//!   index). Then the commit's tree is built in a temporary index of tarae's in the git folder (where
//!   `snapshot.rs` keeps its own; removed after): HEAD's tree, the named paths as they are on disk over it
//!   (new and deleted too). The card's diff is that tree against HEAD, both sides git's objects — what `y`
//!   commits is what the card showed, whatever changes on disk meanwhile. The blobs are written as the tree is
//!   built: on `n` they're unreferenced, and git's gc prunes them.
//! - `y` (a worker's): HEAD and its branch still as the card said, nothing under way since, then `commit-tree`
//!   (the user's identity as git finds it, the message as given — `stripspace`'d, no trailer; signed when the
//!   user's `commit.gpgSign` says so — `commit-tree` doesn't read it — and a failed signing is git's error, on
//!   the toast and to Claude, nothing committed) and `update-ref` from the card's HEAD (a compare-and-swap).
//!   The real index then takes the named paths' new entries and nothing else: what else the user staged stays
//!   staged, as it was.
//! - Every git here is tarae's own (`gitrepo::Repo::command`: the repository's commands held until trusted) with
//!   `NO_HOOKS` and literal pathspecs, plumbing only — no `git commit`, so no template, editor, hook or status.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::editor::Editor;
use crate::offer::{Offer, What};
use crate::tools::Call;

/// Paths one call names at most.
const PATHS_MAX: usize = 200;
/// The card's diff lines (the rest: `v`) — each cut to the card's width where it's drawn, never wrapped.
const PREVIEW: usize = 6;
/// The card's lines of the message's body (the rest: `v`).
const BODY_SHOWN: usize = 4;
/// The card's file rows at most.
const FILES_SHOWN: usize = 12;
/// Bytes of the whole diff kept for `v`.
const PATCH_MAX: usize = 1 << 20;

/// A temporary index's name is unique per process with this.
static INDEX_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Before every git here: no hooks — and in tests no signing, whatever the user's own config says (a global
/// `commit.gpgSign` must never sign a test's commit or start pinentry).
#[cfg(not(test))]
const BEFORE: [&str; 2] = crate::gitrepo::NO_HOOKS;
#[cfg(test)]
const BEFORE: [&str; 4] = ["-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false"];

pub fn schema() -> Value {
    json!({
        "name": "commit",
        "description": "Commit the files you name, with a message — the user sees the message and the diff of exactly \
    those paths on a card and says yes or no. Only those paths go in, as they are on disk (new and deleted files \
    too); whatever else is staged stays staged. No hooks run; nothing is pushed or amended. Files only, inside the \
    checkout.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "message": { "type": "string", "description": "The commit message: a subject line, then a blank line and the body if any" },
                "paths": { "type": "array", "items": { "type": "string" }, "description": "The files to commit, from the project root" }
            },
            "required": ["message", "paths"]
        }
    })
}

/// A file row's warning, and what the card says of it once under the rows.
pub const PROTECTED: (&str, &str) =
    ("protected", "Protected: a build or a tool runs it later, outside the sandbox.");
pub const KEY: (&str, &str) = ("a key file", "A key file: what's in it goes into the history.");
pub const HOOK: (&str, &str) = (
    "a git hook",
    "A git hook: runs as you, outside the sandbox, on commit, push, pull … — once committed, no card asks.",
);

/// A commit asked for, worked out: what the card shows and what `y` commits.
#[derive(Clone, Debug)]
pub struct Ask {
    pub root: PathBuf,
    /// The checkout's top (real).
    pub top: PathBuf,
    /// HEAD as the card was made (None: an unborn branch — the first commit).
    pub parent: Option<String>,
    /// HEAD's branch (None: detached).
    pub branch: Option<String>,
    /// The commit's tree: HEAD's with the named paths as they were on disk.
    pub tree: String,
    /// As git's `stripspace` leaves it.
    pub message: String,
    /// The named paths, from the top.
    pub paths: Vec<String>,
    pub files: Vec<File>,
    /// The whole diff (`v`), cut at `PATCH_MAX`.
    pub patch: String,
    /// What else is staged (from the top) — it stays staged.
    pub staged: Vec<String>,
    /// Named paths with a staged part of their own: the whole file goes in.
    pub partial: Vec<String>,
    /// Named paths with unsaved changes in a buffer: what's on disk goes in.
    pub unsaved: Vec<String>,
}

/// A file of the commit, as its card row says it.
#[derive(Clone, Debug, PartialEq)]
pub struct File {
    /// Its path as the card names it (from the project's root).
    pub shown: String,
    /// Lines added and removed (None: not text).
    pub lines: Option<(usize, usize)>,
    /// `A` new · `D` deleted · `M` changed · `T` its type.
    pub status: char,
    /// What yes means for it, in the warning colour: a protected name, a key file, a git hook (`PROTECTED`,
    /// `KEY`, `HOOK`).
    pub warn: Option<&'static str>,
}

/// The card's commit: the call it answers and what it commits.
#[derive(Clone, Debug)]
pub struct Commit {
    pub call: Call,
    pub ask: Ask,
}

/// What `y` made.
#[derive(Clone, Debug, PartialEq)]
pub struct Done {
    pub id: String,
    pub short: String,
    /// The index couldn't take the new entries (its lock held, say): git's words — the commit is made.
    pub index: Option<String>,
}

/// A call: its words checked now, the rest on a worker — a refusal is answered at once, else a card.
pub fn call(ed: &mut Editor, call: Call, args: &Value) -> Result<(), String> {
    let message = args["message"].as_str().ok_or("message: the commit message, a string")?.to_string();
    let paths: Vec<String> = args["paths"]
        .as_array()
        .ok_or("paths: the files to commit, as a list — [\"src/a.rs\"]")?
        .iter()
        .map(|p| p.as_str().map(str::to_string).ok_or("paths: each a string"))
        .collect::<Result<_, _>>()?;
    if paths.is_empty() {
        return Err("paths: name the files to commit".into());
    }
    if paths.len() > PATHS_MAX {
        return Err(format!("paths: {} at most in one commit", PATHS_MAX));
    }
    // (one at a time — the user answers one diff)
    if ed.session_cards().any(|o| matches!(o.what, What::Commit(_))) {
        return Err("a commit is already waiting for the user's answer — wait for that one".into());
    }
    let root = ed.session.root.clone();
    let unsaved: Vec<PathBuf> =
        ed.docs.iter().filter(|d| d.is_modified()).filter_map(|d| d.path.clone()).collect();
    ed.events.jobs().spawn(move || {
        let r = prepare(&root, &message, &paths, &unsaved);
        move |ed: &mut Editor| match r {
            Ok(ask) => card_up(ed, Commit { call, ask }),
            Err(e) => crate::tools::answer(ed, call, Err(e)),
        }
    });
    Ok(())
}

fn card_up(ed: &mut Editor, c: Commit) {
    // (another call's card came up meanwhile)
    if ed.session_cards().any(|o| matches!(o.what, What::Commit(_))) {
        let e = "a commit is already waiting for the user's answer — wait for that one";
        return crate::tools::answer(ed, c.call, Err(e.into()));
    }
    let o = card(c);
    ed.push_offer(o);
}

/// git on the checkout at `top` (the repository `root`'s), run at its top: tarae's own, `BEFORE`, literal
/// pathspecs, the index `index` if given.
fn git(
    root: &Path,
    top: &Path,
    args: &[&str],
    index: Option<&Path>,
    input: Option<&[u8]>,
) -> Result<Vec<u8>, String> {
    let all: Vec<&str> = BEFORE.iter().chain(args).copied().collect();
    let mut cmd = crate::gitrepo::repo(root)?.command(&all);
    cmd.current_dir(top).env("GIT_LITERAL_PATHSPECS", "1").env_remove("GIT_INDEX_FILE");
    if let Some(i) = index {
        cmd.env("GIT_INDEX_FILE", i);
    }
    crate::gitrepo::finish(&cmd, input)
}

fn text(b: Vec<u8>) -> String {
    String::from_utf8_lossy(&b).into_owned()
}

/// NUL-separated fields, empty ones left out.
fn fields(b: &[u8]) -> Vec<String> {
    b.split(|&c| c == 0).filter(|f| !f.is_empty()).map(|f| String::from_utf8_lossy(f).into_owned()).collect()
}

/// HEAD's commit (None: an unborn branch).
fn head(root: &Path, top: &Path) -> Option<String> {
    let id = git(root, top, &["rev-parse", "--verify", "-q", "HEAD^{commit}"], None, None).ok()?;
    Some(text(id).trim().to_string()).filter(|id| !id.is_empty())
}

/// HEAD's branch (None: detached).
fn branch(root: &Path, top: &Path) -> Option<String> {
    let b = git(root, top, &["symbolic-ref", "-q", "--short", "HEAD"], None, None).ok()?;
    Some(text(b).trim().to_string()).filter(|b| !b.is_empty())
}

/// Why a commit can't be made now: a rebase, a merge, a cherry-pick or revert under way, a conflicted index.
fn under_way(root: &Path, top: &Path, git_dir: &Path) -> Result<(), String> {
    if crate::rebase::under_way(root).is_some() {
        return Err(
            "a rebase is under way here — a commit now would land in the middle of it; nothing committed"
                .into(),
        );
    }
    for (file, what) in
        [("MERGE_HEAD", "a merge"), ("CHERRY_PICK_HEAD", "a cherry-pick"), ("REVERT_HEAD", "a revert")]
    {
        if git_dir.join(file).exists() {
            return Err(format!(
                "{what} is under way here — its commit is the user's to make; nothing committed"
            ));
        }
    }
    let conflicted = fields(&git(root, top, &["ls-files", "-u", "-z"], None, None)?);
    if !conflicted.is_empty() {
        return Err("the index has conflicts — they're the user's to resolve first; nothing committed".into());
    }
    Ok(())
}

/// `raw` (as Claude named it, from the root) as the path git takes, from the top — Err: why not.
fn path_of(root: &Path, top: &Path, raw: &str) -> Result<String, String> {
    if raw.trim().is_empty() {
        return Err("paths: an empty one".into());
    }
    let full = root.join(raw);
    let (Some(folder), Some(name)) = (full.parent(), full.file_name()) else {
        return Err(format!("{raw}: not a file's path"));
    };
    // (its folder by its real path; the entry itself not followed — a link is committed as a link)
    let folder = crate::paths::inside(root, folder).ok_or_else(|| format!("{raw}: outside the project"))?;
    let at = folder.join(name);
    let rel = at.strip_prefix(top).map_err(|_| format!("{raw}: outside the checkout"))?;
    if rel.iter().any(|p| p.eq_ignore_ascii_case(".git")) {
        return Err(format!("{raw}: in git's own folder — not committed"));
    }
    if std::fs::symlink_metadata(&at).is_ok_and(|m| m.is_dir()) {
        return Err(format!("{raw} is a folder — name the files in it"));
    }
    for up in rel.ancestors().skip(1).filter(|a| !a.as_os_str().is_empty()) {
        if std::fs::symlink_metadata(top.join(up).join(".git")).is_ok() {
            return Err(format!("{raw} is in another repository ({}) — commit it there", up.display()));
        }
    }
    rel.to_str().map(str::to_string).ok_or_else(|| format!("{raw}: a name git can't be given here"))
}

/// The commit asked for, worked out (a worker's): the paths judged, the tree built, the diff, what the card says.
pub fn prepare(root: &Path, message: &str, paths: &[String], unsaved: &[PathBuf]) -> Result<Ask, String> {
    let repo =
        crate::gitrepo::repo(root).map_err(|_| "not a git checkout — nothing to commit to".to_string())?;
    let top = repo.work_tree.clone().ok_or("a bare repository — nothing to commit from")?;
    let top = std::fs::canonicalize(&top).unwrap_or(top);
    let git_dir = repo.git_dir.clone();
    under_way(root, &top, &git_dir)?;
    let message = text(git(root, &top, &["stripspace"], None, Some(message.as_bytes()))?);
    if message.trim().is_empty() {
        return Err("message: empty — a subject line at least".into());
    }
    let mut rels: Vec<String> = Vec::new();
    for raw in paths {
        let rel = path_of(root, &top, raw)?;
        if !rels.contains(&rel) {
            rels.push(rel);
        }
    }
    let parent = head(root, &top);
    let shown = |rel: &str| crate::chat::shown(root, &top.join(rel));
    let mut pathspec: Vec<&str> = vec!["--"];
    pathspec.extend(rels.iter().map(String::as_str));
    // (what HEAD has of them: one in neither HEAD nor on disk is no file)
    let in_head = |tree: &str| -> Result<Vec<(String, String)>, String> {
        let mut args = vec!["ls-tree", "-r", "-z", tree];
        args.extend(&pathspec);
        Ok(fields(&git(root, &top, &args, None, None)?)
            .iter()
            .filter_map(|e| e.split_once('\t').map(|(head, p)| (p.to_string(), head.to_string())))
            .collect())
    };
    let was = match &parent {
        Some(p) => in_head(p)?,
        None => Vec::new(),
    };
    for rel in &rels {
        if std::fs::symlink_metadata(top.join(rel)).is_err() && !was.iter().any(|(p, _)| p == rel) {
            return Err(format!("{}: no such file, here or in HEAD", shown(rel)));
        }
    }
    let mut args = vec!["ls-files", "-z", "--others", "--ignored", "--exclude-standard"];
    args.extend(&pathspec);
    if let Some(p) = fields(&git(root, &top, &args, None, None)?).first() {
        return Err(format!("{}: ignored (.gitignore) — not committed", shown(p)));
    }
    let tree = build(root, &top, &git_dir, parent.as_deref(), &rels)?;
    let base = match &parent {
        Some(p) => p.clone(),
        None => text(git(root, &top, &["hash-object", "-t", "tree", "--stdin"], None, Some(b""))?)
            .trim()
            .to_string(),
    };
    let status = fields(&git(
        root,
        &top,
        &["diff-tree", "-r", "-z", "--no-renames", "--name-status", &base, &tree],
        None,
        None,
    )?);
    let status: Vec<(char, String)> =
        status.chunks(2).filter_map(|c| Some((c.first()?.chars().next()?, c.get(1)?.clone()))).collect();
    for rel in &rels {
        if !status.iter().any(|(_, p)| p == rel) {
            return Err(format!("{}: no change from HEAD — nothing of it to commit", shown(rel)));
        }
    }
    let numstat = text(git(
        root,
        &top,
        &["diff-tree", "-r", "-z", "--no-renames", "--numstat", &base, &tree],
        None,
        None,
    )?);
    // (`added\tremoved\tpath\0` — `-\t-\t` not text)
    let lines_of = |rel: &str| {
        numstat
            .split('\0')
            .find_map(|e| {
                let mut f = e.splitn(3, '\t');
                let (a, r, p) = (f.next()?, f.next()?, f.next()?);
                (p == rel).then(|| a.parse().ok().zip(r.parse().ok()))
            })
            .flatten()
    };
    // The checkout's own hooks folder: the one git uses, and the one the repository's own config names while
    // that waits for its trust card (a `y` there makes it the one) — a hook committed runs as the user from
    // then on, with no hooks card: it's "as the last commit has it" (auditfix2 F11)
    let named = repo.local.found.iter().filter(|(k, _)| k == "core.hookspath").map(|(_, v)| {
        let p = top.join(v);
        std::fs::canonicalize(&p).unwrap_or(p)
    });
    let hooks: Vec<PathBuf> = crate::hooks::folder(&top)
        .map(|(_, d)| d)
        .into_iter()
        .chain(named.filter(|d| d.starts_with(&top) && !d.starts_with(&git_dir)))
        .collect();
    let files = status
        .iter()
        .map(|(s, rel)| {
            let path = top.join(rel);
            let warn = if crate::edits::protected(root, &path) {
                Some(PROTECTED.0)
            } else if crate::sandbox::key_named_in(root, &path) {
                Some(KEY.0)
            } else if hooks.iter().any(|d| path.starts_with(d)) {
                Some(HOOK.0)
            } else {
                None
            };
            File { shown: shown(rel), lines: lines_of(rel), status: *s, warn }
        })
        .collect();
    let patch = git(
        root,
        &top,
        &[
            "diff-tree",
            "-r",
            "-p",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            "--no-renames",
            &base,
            &tree,
        ],
        None,
        None,
    )?;
    let mut patch = text(patch);
    if patch.len() > PATCH_MAX {
        let mut cut = PATCH_MAX;
        while !patch.is_char_boundary(cut) {
            cut -= 1;
        }
        patch.truncate(cut);
        patch.push_str("\n… (cut here — `git diff --cached` after the commit shows the rest)\n");
    }
    // What else is staged: the index against HEAD (blob ids only — no stat, no refresh)
    let staged: Vec<String> = match &parent {
        Some(p) => fields(&git(root, &top, &["diff-index", "--cached", "--name-only", "-z", p], None, None)?),
        None => fields(&git(root, &top, &["ls-files", "-z"], None, None)?),
    };
    let staged: Vec<String> = staged.into_iter().filter(|p| !rels.contains(p)).collect();
    // A named path whose staged blob is neither HEAD's nor the commit's: part of it was staged
    let mut args = vec!["ls-files", "-s", "-z"];
    args.extend(&pathspec);
    let index: Vec<(String, String)> = fields(&git(root, &top, &args, None, None)?)
        .iter()
        .filter_map(|e| e.split_once('\t').map(|(head, p)| (p.to_string(), blob(head))))
        .collect();
    let now = in_head(&tree)?;
    let blob_in =
        |list: &[(String, String)], rel: &str| list.iter().find(|(p, _)| p == rel).map(|(_, h)| blob(h));
    let partial = rels
        .iter()
        .filter(|rel| {
            let staged = index.iter().find(|(p, _)| p == *rel).map(|(_, b)| b.clone());
            staged.is_some() && staged != blob_in(&was, rel) && staged != blob_in(&now, rel)
        })
        .map(|rel| shown(rel))
        .collect();
    let unsaved = rels
        .iter()
        .filter(|rel| {
            let path = top.join(rel);
            unsaved.iter().any(|u| *u == path || std::fs::canonicalize(u).is_ok_and(|u| u == path))
        })
        .map(|rel| shown(rel))
        .collect();
    Ok(Ask {
        root: root.to_path_buf(),
        top: top.clone(),
        branch: branch(root, &top),
        parent,
        tree,
        message,
        paths: rels,
        files,
        patch,
        staged,
        partial,
        unsaved,
    })
}

/// The blob of an entry's head (`ls-tree`'s `mode type id`, `ls-files -s`'s `mode id stage`).
fn blob(head: &str) -> String {
    head.split(' ')
        .find(|w| w.len() >= 40 && w.bytes().all(|b| b.is_ascii_hexdigit()))
        .unwrap_or_default()
        .to_string()
}

/// The commit's tree: `parent`'s with `rels` as they are on disk over it — in a temporary index of tarae's in
/// the git folder (as `snapshot.rs` keeps its own), removed after; the user's index never read or written.
fn build(
    root: &Path,
    top: &Path,
    git_dir: &Path,
    parent: Option<&str>,
    rels: &[String],
) -> Result<String, String> {
    let seq = INDEX_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let index = git_dir.join(format!("tarae-commit-{}-{seq}.index", std::process::id()));
    let built = (|| {
        let read: &[&str] = match parent {
            Some(p) => &["read-tree", p],
            None => &["read-tree", "--empty"],
        };
        git(root, top, read, Some(&index), None)?;
        let list: Vec<u8> = rels.iter().flat_map(|r| r.bytes().chain([0])).collect();
        git(root, top, &["update-index", "--add", "--remove", "-z", "--stdin"], Some(&index), Some(&list))?;
        Ok(text(git(root, top, &["write-tree"], Some(&index), None)?).trim().to_string())
    })();
    std::fs::remove_file(&index).ok();
    built
}

/// `y`: the card's tree committed on the card's HEAD (a worker's) — Err: why not, nothing committed.
pub fn commit(a: &Ask) -> Result<Done, String> {
    let (root, top) = (a.root.as_path(), a.top.as_path());
    let repo = crate::gitrepo::repo(root)?;
    under_way(root, top, &repo.git_dir)?;
    if branch(root, top) != a.branch {
        let on = branch(root, top).unwrap_or_else(|| "a detached HEAD".into());
        return Err(format!("HEAD is on {on} now, not where the card said — nothing committed"));
    }
    if head(root, top) != a.parent {
        return Err("HEAD moved since the card — nothing committed; ask again".into());
    }
    // (`commit-tree` doesn't read `commit.gpgSign`: the user's setting followed by hand)
    let sign = git(root, top, &["config", "--type=bool", "--get", "commit.gpgSign"], None, None)
        .is_ok_and(|s| text(s).trim() == "true");
    let mut args = vec!["commit-tree", a.tree.as_str()];
    if let Some(p) = &a.parent {
        args.extend(["-p", p.as_str()]);
    }
    args.push(if sign { "-S" } else { "--no-gpg-sign" });
    let id = text(git(root, top, &args, None, Some(a.message.as_bytes()))?).trim().to_string();
    let subject = a.message.lines().next().unwrap_or_default();
    let line = match &a.parent {
        Some(p) => format!("update HEAD {id} {p}\n"),
        None => format!("create HEAD {id}\n"),
    };
    let reflog = format!("commit (claude): {subject}");
    git(root, top, &["update-ref", "-m", &reflog, "--stdin"], None, Some(line.as_bytes()))?;
    // The index: the named paths' new entries, nothing else touched
    let mut args = vec!["ls-tree", "-r", "-z", id.as_str(), "--"];
    args.extend(a.paths.iter().map(String::as_str));
    let entries = fields(&git(root, top, &args, None, None)?);
    let zero = "0".repeat(id.len());
    let mut info = Vec::new();
    for rel in &a.paths {
        let entry = entries.iter().find_map(|e| e.split_once('\t').filter(|(_, p)| p == rel));
        let head = match entry {
            // (`mode blob id` — `--index-info` takes `mode id`)
            Some((head, _)) => {
                let w: Vec<&str> = head.split(' ').collect();
                format!("{} {}", w.first().unwrap_or(&""), w.get(2).unwrap_or(&""))
            }
            None => format!("0 {zero}"),
        };
        info.extend(format!("{head}\t{rel}").bytes().chain([0]));
    }
    let index = git(root, top, &["update-index", "-z", "--index-info"], None, Some(&info)).err();
    let short = git(root, top, &["rev-parse", "--short", &id], None, None)
        .map(|s| text(s).trim().to_string())
        .unwrap_or_else(|_| id.chars().take(7).collect());
    Ok(Done { id, short, index })
}

/// The card: the message (what's decided), where and which files — a protected name, a key file or a git hook
/// in the warning colour — the diff's start, what stays staged, what the user should know of their own changes.
pub fn card(c: Commit) -> Offer {
    let a = &c.ask;
    let mut q: Vec<String> = Vec::new();
    let mut warnings = Vec::new();
    let mut message = a.message.trim_end().lines();
    q.push(message.next().unwrap_or_default().to_string());
    let body: Vec<&str> = message.skip_while(|l| l.trim().is_empty()).collect();
    q.extend(body.iter().take(BODY_SHOWN).map(|l| l.to_string()));
    if body.len() > BODY_SHOWN {
        q.push(format!("… {} — v shows it whole", crate::nav::count(body.len() - BODY_SHOWN, "more line")));
    }
    q.push(String::new());
    let files = crate::nav::count(a.files.len(), "file");
    q.push(match (&a.branch, &a.parent) {
        (Some(b), Some(_)) => format!("On {b}:"),
        (Some(b), None) => format!("The first commit on {b}:"),
        (None, _) => "On a detached HEAD:".into(),
    });
    let tag = |f: &File| match (f.status, f.lines) {
        ('A', Some((n, _))) => format!("new +{n}"),
        ('D', Some((_, n))) => format!("deleted −{n}"),
        ('A', None) => "new".into(),
        ('D', None) => "deleted".into(),
        (_, None) => "not text".into(),
        (_, Some((0, r))) => format!("−{r}"),
        (_, Some((n, 0))) => format!("+{n}"),
        (_, Some((n, r))) => format!("+{n} −{r}"),
    };
    let shown = &a.files[..a.files.len().min(FILES_SHOWN)];
    let width = |s: &str| s.chars().count();
    let name_w = shown.iter().map(|f| width(&f.shown)).max().unwrap_or(0);
    let tag_w = shown.iter().map(|f| width(&tag(f))).max().unwrap_or(0);
    for f in shown {
        let name = format!("{}{}", f.shown, " ".repeat(name_w - width(&f.shown)));
        let t = tag(f);
        let t = format!("{}{t}", " ".repeat(tag_w - width(&t)));
        match f.warn {
            Some(w) => {
                warnings.push(q.len()..q.len() + 1);
                q.push(format!("  {name}  {t}  {w}"));
            }
            None => q.push(format!("  {name}  {t}").trim_end().to_string()),
        }
    }
    if a.files.len() > FILES_SHOWN {
        q.push(format!("  … {} more — v shows every one", a.files.len() - FILES_SHOWN));
        // (a warned file past the rows still warns)
        let past: Vec<&str> =
            a.files[FILES_SHOWN..].iter().filter(|f| f.warn.is_some()).map(|f| f.shown.as_str()).collect();
        if !past.is_empty() {
            warnings.push(q.len()..q.len() + 1);
            q.push(format!("  among them {}: protected, a key file or a git hook", past.join(", ")));
        }
    }
    // What a warned row means, once each
    for (word, said) in [PROTECTED, KEY, HOOK] {
        if a.files.iter().any(|f| f.warn == Some(word)) {
            warnings.push(q.len()..q.len() + 1);
            q.push(said.into());
        }
    }
    // The diff's start: its changed lines, `@@` heads faint (no file headers — the rows above name them)
    let lines: Vec<&str> = a
        .patch
        .lines()
        .filter(|l| {
            (l.starts_with('+') && !l.starts_with("+++"))
                || (l.starts_with('-') && !l.starts_with("---"))
                || l.starts_with("@@")
        })
        .collect();
    let mut diff = None;
    if !lines.is_empty() {
        q.push(String::new());
        let start = q.len();
        q.extend(lines.iter().take(PREVIEW).map(|l| l.to_string()));
        diff = Some(start..q.len());
        if lines.len() > PREVIEW {
            q.push(format!("… {} — v shows it all", crate::nav::count(lines.len() - PREVIEW, "more line")));
        }
    }
    q.push(String::new());
    if !a.staged.is_empty() {
        let names: Vec<&str> = a.staged.iter().take(3).map(String::as_str).collect();
        let more = match a.staged.len() {
            n if n > 3 => format!(" and {} more", n - 3),
            _ => String::new(),
        };
        q.push(format!("Staged and left out: {}{more}", names.join(", ")));
    }
    for p in &a.partial {
        warnings.push(q.len()..q.len() + 1);
        q.push(format!("{p}: part of it was staged — the whole file goes in."));
    }
    for p in &a.unsaved {
        warnings.push(q.len()..q.len() + 1);
        q.push(format!("{p} has unsaved changes — what's on disk goes in."));
    }
    q.push("No hooks run; nothing is pushed.".into());
    let what = a.paths.join(" ");
    let title = format!("Claude commits {files}");
    let asked =
        crate::cards::Asked::new(crate::cards::Why::Commit, crate::cards::Request::Commit).about(&what);
    // (the subject at full strength; the body under it faint)
    let mut o =
        Offer::new(title, &q.join("\n"), "Committing…", "Couldn't commit".into(), What::Commit(Box::new(c)))
            .deciding(0..1)
            .asked(asked);
    o.warnings = warnings;
    o.diff = diff;
    o
}

/// `v`: the card's text, then the whole diff and every file.
pub fn view_text(a: &Ask) -> String {
    let files: Vec<String> = a.files.iter().map(|f| format!("  {} {}", f.status, f.shown)).collect();
    format!(
        "\n── the message\n\n{}\n── {}\n\n{}\n\n── the diff\n\n{}",
        a.message,
        crate::nav::count(a.files.len(), "file"),
        files.join("\n"),
        a.patch
    )
}

/// The user's answer on card `id`: `y` commits on a worker (the card says it's committing until then), `n`
/// tells Claude no.
pub fn answer(ed: &mut Editor, id: u64, c: Commit, yes: bool) {
    if !yes {
        ed.set_status("Not committed — nothing changed");
        return crate::tools::answer(ed, c.call, Err("the user said no — nothing was committed".into()));
    }
    let Commit { call, ask } = c;
    ed.events.jobs().spawn(move || {
        let r = commit(&ask);
        move |ed: &mut Editor| {
            ed.offer_finished(id, Ok(()));
            let subject = ask.message.lines().next().unwrap_or_default().to_string();
            let on = ask.branch.clone().unwrap_or_else(|| "a detached HEAD".into());
            let files = crate::nav::count(ask.paths.len(), "file");
            match r {
                Ok(done) => {
                    let index = match &done.index {
                        Some(e) => format!(" The index couldn't take its new entries ({e}): `git status` shows them as staged changes back to the old ones."),
                        None => String::new(),
                    };
                    match &done.index {
                        Some(_) => ed.set_warning(format!("Committed {} on {on} — the index wasn't brought along", done.short)),
                        None => ed.set_success(format!("Committed {} on {on} — {subject}", done.short)),
                    }
                    let said = format!("Committed {} on {on}: {subject} ({files}). Not pushed; nothing else staged went in.{index}", done.short);
                    crate::tools::answer(ed, call, Ok(said));
                }
                Err(e) => {
                    ed.set_error(format!("commit: {e}"));
                    crate::tools::answer(ed, call, Err(format!("not committed: {e}")));
                }
            }
            ed.git_refresh();
            ed.git_reload_bases();
        }
    });
}

/// The chat turn ended: a card still asking goes; its call needs no answer.
pub fn turn_over(ed: &mut Editor) {
    ed.drop_cards(|w| matches!(w, What::Commit(_)));
}

#[cfg(test)]
mod tests;
