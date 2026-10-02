//! Code review — walk every change against a base over the real buffers, leave comments, and send them to
//! Claude in one go (`space v`, `:review [branch]`).
//!
//! - Base: the uncommitted changes (against HEAD), or this branch since it left another (`git merge-base`; a
//!   remote branch is fetched first). Changed files = `git diff --numstat` against it + untracked files.
//! - Each file opens as the real buffer (every motion, LSP and edit still work) with the base as its diff
//!   base: the gutter, `]g`/`[g` (which go on into the next file) and the hunk card follow it, removed lines
//!   show in place (`ViewRow::Removed`), added ones on a green wash. A rail on the left lists the files.
//! - Comments are notes; while a review is open (or has drafts) `space n` writes a draft, and `space v s`
//!   sends them all in one message with their diffs — Claude answers each on its note, its edits come as the
//!   usual review.
//! - `space v m` marks a file viewed (a later change to it takes the mark back); `space v q` ends the review
//!   and puts the index back as the diff base.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::document::DocId;
use crate::editor::Editor;
use crate::key::{Code, Key};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Base {
    /// The uncommitted changes.
    Head,
    /// This branch since it left `name` (at `commit`, their merge base).
    Branch { name: String, commit: String },
    /// What Claude changed in a turn — checkpoint `at` (`checkpoint.rs`), each file against its text before
    /// the turn. No git.
    Turn { at: usize, question: String },
}

impl Base {
    /// The git revision the files are diffed against (None: a turn's own texts).
    fn rev(&self) -> Option<&str> {
        match self {
            Base::Head => Some("HEAD"),
            Base::Branch { commit, .. } => Some(commit),
            Base::Turn { .. } => None,
        }
    }

    pub fn label(&self) -> String {
        match self {
            Base::Head => "uncommitted".into(),
            Base::Branch { name, .. } => format!("since {name}"),
            Base::Turn { .. } => "Claude's turn".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct File {
    /// Relative to the repository root.
    pub rel: String,
    pub path: PathBuf,
    pub added: usize,
    pub removed: usize,
    /// Not in the base (new or untracked) · gone from the working tree · binary (no lines to show).
    pub new: bool,
    pub gone: bool,
    pub binary: bool,
}

pub struct CodeReview {
    pub base: Base,
    pub root: PathBuf,
    pub files: Vec<File>,
    /// rel → the document version it was marked viewed at (a later change takes the mark back).
    viewed: HashMap<String, (DocId, u64)>,
    /// Documents diffed against the review's base (back to the index when it ends).
    pub docs: Vec<DocId>,
    /// Once this document's diff arrives, select its first (or, going back, last) change.
    land: Option<(DocId, bool)>,
    /// The rail has the keys (`space e` during a review) · the file selected there.
    pub focus: bool,
    pub selected: usize,
    /// The rail's first file row shown (the wheel moves it) · the file it last scrolled to (`fit`).
    pub scroll: usize,
    shown: Option<usize>,
    /// The git tab is in front of the rail (`space g` during a review; `space g`/Esc there gives the
    /// column back — `Editor::review_rail_up`).
    pub behind: bool,
    /// The columns the rail's rows need (`term::review_rail_w` — measured once: the files don't change).
    pub rail_need: std::cell::OnceCell<usize>,
}

impl CodeReview {
    /// A review over `files` (tests and snapshots — no git).
    #[cfg(test)]
    pub fn for_test(base: Base, root: PathBuf, files: Vec<File>) -> CodeReview {
        CodeReview {
            base,
            root,
            files,
            viewed: HashMap::new(),
            docs: Vec::new(),
            land: None,
            focus: false,
            selected: 0,
            scroll: 0,
            shown: None,
            behind: false,
            rail_need: Default::default(),
        }
    }

    pub fn viewed(&self, ed: &Editor, f: &File) -> bool {
        self.viewed
            .get(&f.rel)
            .is_some_and(|&(id, v)| ed.docs.iter().find(|d| d.id == id).is_none_or(|d| d.version() == v))
    }

    /// The rail's rows: the files under their folders.
    pub fn rail_rows(&self) -> Vec<crate::filetree::Row> {
        crate::filetree::Tree::build(
            self.files.iter().enumerate().map(|(i, f)| (i, f.rel.as_str(), f.rel.len())),
            false,
        )
        .rows(None)
    }

    /// The files in the rail's order (under their folders).
    pub fn rail_order(&self) -> Vec<usize> {
        self.rail_rows().into_iter().filter_map(|t| t.item).collect()
    }

    /// The file the rail marks: its selection while it has the keys, else the file being read.
    pub fn marked(&self, ed: &Editor) -> Option<usize> {
        if self.focus { Some(self.selected) } else { self.current(ed) }
    }

    /// Before drawing `room` rows of the rail: scroll only when the marked file changed and is out of
    /// sight — just into view while the rail has the keys, else centered — so a click doesn't slide the
    /// list under the pointer, and the wheel's scroll stays.
    pub fn fit(&mut self, marked: Option<usize>, room: usize) {
        let rows = self.rail_rows();
        if marked != self.shown {
            self.shown = marked;
            if let Some(at) = marked.and_then(|m| rows.iter().position(|t| t.item == Some(m)))
                && !(self.scroll..self.scroll + room).contains(&at)
            {
                self.scroll = match (self.focus, at < self.scroll) {
                    (true, true) => at,
                    (true, false) => at + 1 - room,
                    (false, _) => at.saturating_sub(room / 2),
                };
            }
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(room));
    }

    /// Which file the current document is.
    pub fn current(&self, ed: &Editor) -> Option<usize> {
        let doc = ed.doc();
        match &doc.path {
            Some(p) => self.files.iter().position(|f| &f.path == p),
            None => {
                doc.title.as_ref().and_then(|t| self.files.iter().position(|f| f.gone && t == &gone_title(f)))
            }
        }
    }
}

fn gone_title(f: &File) -> String {
    format!("{} (deleted)", f.rel)
}

// ── git (worker threads) ──────────────────────────────────────────────────────

/// (the one way git runs — in a session of its own, so the fetch below can't ask for a password on the
/// terminal)
fn git(dir: &Path, args: &[&str]) -> Result<String, String> {
    crate::gitmenu::git(dir, args, None)
}

/// `git diff --numstat -z` → (added, removed, path, binary).
pub fn parse_numstat(out: &str) -> Vec<(usize, usize, String, bool)> {
    out.split('\0')
        .filter_map(|rec| {
            let mut it = rec.splitn(3, '\t');
            let (a, r, p) = (it.next()?, it.next()?, it.next()?.trim_start_matches('\n'));
            if p.is_empty() {
                return None;
            }
            let binary = a == "-" && r == "-";
            Some((a.parse().unwrap_or(0), r.parse().unwrap_or(0), p.to_string(), binary))
        })
        .collect()
}

/// The changed files against `base`, sorted by path.
fn changed(root: &Path, base: &Base) -> Result<Vec<File>, String> {
    let rev = base.rev().ok_or("not a git review")?;
    // (`diff-index`: `diff` would refresh the index — `diff_text`)
    let numstat = git(root, &["diff-index", "--numstat", "-z", "--no-renames", rev])?;
    let mut files: Vec<File> = parse_numstat(&numstat)
        .into_iter()
        .map(|(added, removed, rel, binary)| {
            let path = root.join(&rel);
            let gone = !path.exists();
            File { new: false, gone, binary, added, removed, rel, path }
        })
        .collect();
    // New in the base's eyes: added with nothing removed and not in the base tree
    let in_base: std::collections::HashSet<String> = git(root, &["ls-tree", "-r", "--name-only", "-z", rev])?
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect();
    for f in &mut files {
        f.new = !in_base.contains(&f.rel);
    }
    for rel in git(root, &["ls-files", "--others", "--exclude-standard", "-z"])?
        .split('\0')
        .filter(|s| !s.is_empty())
    {
        let path = root.join(rel);
        let text = std::fs::read(&path).unwrap_or_default();
        let binary = text.contains(&0);
        let added = if binary { 0 } else { text.iter().filter(|&&b| b == b'\n').count() };
        files.push(File { rel: rel.to_string(), path, added, removed: 0, new: true, gone: false, binary });
    }
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    files.dedup_by(|a, b| a.rel == b.rel);
    Ok(files)
}

/// The working tree's patch against `rev`. (`diff-index`, not `diff`: over a file whose stat changed, `diff`
/// refreshes the index — taking its lock whatever `GIT_OPTIONAL_LOCKS` says; the output is the same)
fn diff_text(root: &Path, rev: &str) -> String {
    git(root, &["diff-index", "-p", "--no-color", "--no-ext-diff", rev]).unwrap_or_default()
}

// ── Starting and ending ───────────────────────────────────────────────────────

/// `space v v` / `:review [branch]`: the changes against HEAD, or since `branch` (fetched first if remote).
pub fn start(ed: &mut Editor, branch: Option<String>) {
    start_in(ed, std::env::current_dir().unwrap_or_default(), branch);
}

/// `start` for the repository around `cwd`.
pub fn start_in(ed: &mut Editor, cwd: PathBuf, branch: Option<String>) {
    ed.set_status(match &branch {
        Some(b) => format!("review: gathering changes since {b}…"),
        None => "review: gathering changes…".into(),
    });
    ed.events.jobs().spawn(move || {
        let found = (|| -> Result<(PathBuf, Base, Vec<File>), String> {
            let root = PathBuf::from(git(&cwd, &["rev-parse", "--show-toplevel"])?.trim());
            let base = match branch {
                None => Base::Head,
                Some(name) => {
                    // origin/main → bring origin's main up to date (offline: the last fetch will do)
                    let remotes = git(&root, &["remote"]).unwrap_or_default();
                    if let Some((remote, rest)) = name.split_once('/')
                        && remotes.lines().any(|r| r == remote)
                    {
                        let _ = git(&root, &["fetch", "--quiet", remote, rest]);
                    }
                    let commit = git(&root, &["merge-base", "HEAD", &name])
                        .map_err(|e| format!("{name}: {e}"))?
                        .trim()
                        .to_string();
                    Base::Branch { name, commit }
                }
            };
            let files = changed(&root, &base)?;
            Ok((root, base, files))
        })();
        move |ed: &mut Editor| match found {
            Err(e) => ed.set_error(format!("review: {e}")),
            Ok((root, base, files)) => open(ed, root, base, files),
        }
    });
}

/// `space v t` · `:review-turn`: what Claude changed in its latest turn (not taken back), as a review.
pub fn review_turn(ed: &mut Editor) {
    match ed.checkpoints.list.iter().rposition(|c| !c.undone) {
        Some(at) => start_turn(ed, at),
        None => ed.set_status("no turn of Claude's changed files — nothing to review"),
    }
}

/// Checkpoint `at` as a review (the end of a turn under review at the end, `space v t`): its files as they
/// are now, each against its text before the turn — counted on a worker.
pub fn start_turn(ed: &mut Editor, at: usize) {
    let Some(cp) = ed.checkpoints.list.get(at) else { return };
    let base = Base::Turn { at, question: cp.question.clone() };
    let files: Vec<(PathBuf, Option<ropey::Rope>)> =
        cp.files.iter().map(|t| (t.path.clone(), t.before.clone())).collect();
    let root = ed.chat.as_ref().and_then(|c| c.edits.at_end.clone()).unwrap_or_else(|| {
        let cwd = std::env::current_dir().unwrap_or_default();
        std::fs::canonicalize(&cwd).unwrap_or(cwd)
    });
    ed.events.jobs().spawn(move || {
        let files: Vec<File> = files
            .into_iter()
            .map(|(path, before)| {
                let now = std::fs::read_to_string(&path);
                let gone = !path.exists();
                let hunks = crate::git::hunks(
                    &before.as_ref().map(|b| b.to_string()).unwrap_or_default(),
                    now.as_deref().unwrap_or_default(),
                );
                let rel = path.strip_prefix(&root).unwrap_or(&path).display().to_string();
                File {
                    added: hunks.iter().map(|h| h.lines.len()).sum(),
                    removed: hunks.iter().map(|h| h.base.len()).sum(),
                    new: before.is_none(),
                    gone,
                    // (there but not text)
                    binary: !gone && now.is_err(),
                    rel,
                    path,
                }
            })
            .collect();
        move |ed: &mut Editor| open(ed, root, base, files)
    });
}

/// The review of `files` against `base` begins (any open one ends).
fn open(ed: &mut Editor, root: PathBuf, base: Base, mut files: Vec<File>) {
    if files.is_empty() {
        return ed.set_status(format!("review: no changes {}", base.label()));
    }
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    close(ed);
    let n = files.len();
    // Start on the file being looked at, if it's among the changes (else the first)
    let here = ed.doc().path.clone();
    let first = files.iter().position(|f| here.as_ref() == Some(&f.path)).unwrap_or(0);
    let turn = matches!(base, Base::Turn { .. });
    ed.code_review = Some(CodeReview {
        base,
        root,
        files,
        viewed: HashMap::new(),
        docs: Vec::new(),
        land: None,
        focus: false,
        selected: 0,
        scroll: 0,
        shown: None,
        behind: false,
        rail_need: Default::default(),
    });
    let s = if n == 1 { "" } else { "s" };
    ed.set_status(match turn {
        true => format!("review: {n} file{s} Claude changed — ]g walks the changes"),
        false => format!("review: {n} file{s} changed — ]g walks the changes"),
    });
    show(ed, first, false);
    if std::mem::take(&mut ed.review_ask) {
        claude_reviews(ed);
    }
}

/// `space v c` · `:review-claude [branch]` — Claude reviews the changes: a note on the lines of each finding,
/// a verdict in the chat. Opens the review first (against HEAD, or since `branch`) if it isn't.
pub fn ask_claude(ed: &mut Editor, branch: Option<String>) {
    if ed.code_review.is_none() || branch.is_some() {
        ed.review_ask = true;
        return start(ed, branch);
    }
    claude_reviews(ed);
}

/// Most bytes of the diff Claude is sent to review (it reads the rest itself).
const REVIEW_DIFF_MAX: usize = 150_000;

const REVIEW_ASK: &str = "Review my changes above as a careful reviewer would. Find what's wrong or risky: \
bugs, cases that break, missing handling, missing tests — read the code around them where you need to. Pin each \
finding to its lines with the note tool (the exact lines; one or two sentences on what's wrong and why it \
matters), the most important first, a handful at most — no style nits. Don't change any files. Then give a \
short verdict here.";

/// The review's diff (untracked files as whole additions) to Claude, on a worker, then the ask.
fn claude_reviews(ed: &mut Editor) {
    let Some(r) = ed.code_review.as_ref() else { return };
    if ed.chat.as_ref().is_some_and(|c| c.busy()) {
        return ed.set_status("Claude is still answering — ask for its review once it's done");
    }
    let (root, rev, label) = (r.root.clone(), r.base.rev().map(str::to_string), r.base.label());
    let untracked: Vec<File> = r.files.iter().filter(|f| f.new && !f.binary && !f.gone).cloned().collect();
    // A turn's: each file against its text before the turn
    let turn: Vec<(String, PathBuf, Option<ropey::Rope>)> = match &r.base {
        Base::Turn { at, .. } => {
            let before = |p: &PathBuf| {
                ed.checkpoints.list.get(*at)?.files.iter().find(|t| &t.path == p)?.before.clone()
            };
            r.files.iter().map(|f| (f.rel.clone(), f.path.clone(), before(&f.path))).collect()
        }
        _ => Vec::new(),
    };
    let n = r.files.len();
    ed.set_status("Claude is reading the changes…");
    ed.events.jobs().spawn(move || {
        let mut diff = match &rev {
            Some(rev) => diff_text(&root, rev),
            None => turn_diff(&turn),
        };
        for f in untracked.iter().filter(|_| rev.is_some()) {
            if diff.contains(&format!("b/{}\n", f.rel)) {
                continue;
            }
            let text = std::fs::read_to_string(&f.path).unwrap_or_default();
            diff.push_str(&format!("--- /dev/null\n+++ b/{}\n", f.rel));
            for l in text.lines() {
                diff.push('+');
                diff.push_str(l);
                diff.push('\n');
            }
        }
        let mut cut = REVIEW_DIFF_MAX.min(diff.len());
        while !diff.is_char_boundary(cut) {
            cut -= 1;
        }
        let more = diff.len() - cut;
        diff.truncate(cut);
        if more > 0 {
            diff.push_str(&format!("\n… ({more} more bytes — read those files yourself)\n"));
        }
        move |ed: &mut Editor| {
            let about = format!("<changes of=\"{label}\">\n<diff>\n{diff}</diff>\n</changes>");
            let chip = format!("Claude reviews · {n} file{}", if n == 1 { "" } else { "s" });
            if ed.chat.is_none() {
                crate::chat::open(ed);
                if let Some(c) = &mut ed.chat {
                    c.focused = false;
                    c.unfolded = false;
                }
            }
            if !crate::chat::submit(ed, REVIEW_ASK, Some(about), Some(chip)) {
                ed.set_warning("couldn't reach claude — the chat panel says why");
            }
        }
    });
}

/// A turn's files (rel, path, text before) as a unified diff against what they are on disk now.
fn turn_diff(files: &[(String, PathBuf, Option<ropey::Rope>)]) -> String {
    let mut out = String::new();
    for (rel, path, before) in files {
        let base = before.as_ref().map(|b| b.to_string()).unwrap_or_default();
        let now = ropey::Rope::from_str(&std::fs::read_to_string(path).unwrap_or_default());
        for (k, h) in crate::git::hunks(&base, &now.to_string()).iter().enumerate() {
            let p = crate::gitmenu::patch(rel, &base, &now, h);
            // (the file's header once)
            let body = if k == 0 { p.as_str() } else { p.splitn(4, '\n').nth(3).unwrap_or_default() };
            out.push_str(body);
        }
    }
    out
}

/// `space v q`: the review ends — documents go back to the index as their diff base (drafts stay).
pub fn close(ed: &mut Editor) {
    let Some(r) = ed.code_review.take() else { return };
    for id in r.docs {
        if let Some(d) = ed.docs.iter_mut().find(|d| d.id == id) {
            d.review_base = None;
            d.git_base = None;
            d.git_hunks.clear();
            d.diff_of = None;
        }
        ed.git_load_base(id);
    }
}

// ── Files ─────────────────────────────────────────────────────────────────────

/// Opens file `i` of the review with its base; `at_end` = land on its last change (walking back).
pub fn show(ed: &mut Editor, i: usize, at_end: bool) {
    let Some(r) = &ed.code_review else { return };
    let Some(f) = r.files.get(i).cloned() else { return };
    let from = match &r.base {
        Base::Turn { at, .. } => From::Text(
            ed.checkpoints
                .list
                .get(*at)
                .and_then(|c| c.files.iter().find(|t| t.path == f.path))
                .and_then(|t| t.before.clone()),
        ),
        base => From::Git(r.root.clone(), base.rev().unwrap_or("HEAD").to_string()),
    };
    if f.binary {
        return ed.set_status(format!("review: {} is binary", f.rel));
    }
    ed.push_jump();
    let id = if f.gone {
        // Nothing on disk: an empty buffer whose every base line shows as removed
        let title = gone_title(&f);
        match ed.docs.iter().position(|d| d.title.as_deref() == Some(title.as_str())) {
            Some(k) => {
                ed.current = k;
                ed.docs[k].id
            }
            None => ed.open_virtual(&title, ""),
        }
    } else {
        if let Err(e) = ed.open(&f.path) {
            return ed.set_error(format!("{e:#}"));
        }
        ed.doc().id
    };
    if let Some(r) = &mut ed.code_review {
        if !r.docs.contains(&id) {
            r.docs.push(id);
        }
        r.land = Some((id, at_end));
    }
    // Seen before in this review: its base is there already — land now (the diff keeps itself current)
    if ed.docs.iter().any(|d| d.id == id && d.review_base.is_some() && d.diff_of.is_none()) {
        return land(ed);
    }
    load_base(ed, id, from, f);
}

/// Where a file's base text comes from: git (the root, the revision) · the text itself (a turn's — None: the
/// file didn't exist).
enum From {
    Git(PathBuf, String),
    Text(Option<ropey::Rope>),
}

/// The file's base text (worker) → its diff base; the first diff comes with it.
fn load_base(ed: &mut Editor, id: DocId, from: From, f: File) {
    let Some(doc) = ed.docs.iter().find(|d| d.id == id) else { return };
    // A big file still loading has a placeholder for text (and keeps its version when the text comes in):
    // its diff is worked out once it's there
    let (text, version) = (doc.text.clone(), (!doc.loading).then(|| doc.version()));
    ed.events.jobs().spawn(move || {
        let base = match from {
            From::Text(t) => t.map(|t| t.to_string()).unwrap_or_default(),
            From::Git(..) if f.new => String::new(),
            From::Git(root, rev) => git(&root, &["show", &format!("{rev}:{}", f.rel)]).unwrap_or_default(),
        };
        let hunks = version.map(|_| crate::git::hunks(&base, &text.to_string()));
        move |ed: &mut Editor| {
            let Some(d) = ed.docs.iter_mut().find(|d| d.id == id) else { return };
            d.review_base = Some(ropey::Rope::from_str(&base));
            d.git_base = Some(Arc::new(base));
            d.diff_of = None; // the review's base now, not a `:diff`'s
            if let (Some(version), Some(hunks)) = (version, hunks)
                && d.version() == version
            {
                d.git_hunks = hunks;
                d.git_version = version;
            } else {
                d.git_version = u64::MAX;
            }
            land(ed);
        }
    });
}

/// A file just shown: its first (last) change selected and centered, once its diff is there.
pub fn land(ed: &mut Editor) {
    let Some((id, at_end)) = ed.code_review.as_ref().and_then(|r| r.land) else { return };
    let Some(d) = ed.docs.iter().find(|d| d.id == id) else { return };
    if d.git_version != d.version() || ed.doc().id != id {
        return;
    }
    if let Some(r) = &mut ed.code_review {
        r.land = None;
    }
    let target = if at_end { d.git_hunks.last() } else { d.git_hunks.first() }.cloned();
    if let Some(h) = target {
        ed.select_hunk(&h);
        // The cursor on its first line — a change taller than the screen (a new file) shows from its top
        let r = ed.doc().selection().primary();
        let back = crate::selection::Range::new(r.to(), r.from());
        ed.doc_mut().set_selection(crate::selection::Selection::single(back));
        ed.align_view(crate::viewalign::Place::Center);
    }
}

/// `]g`/`[g` past the last (first) change of a file: on to the next (previous) file of the review. True if
/// it moved.
pub fn next_file(ed: &mut Editor, forward: bool) -> bool {
    let Some(r) = &ed.code_review else { return false };
    let n = r.files.len();
    let Some(i) = r.current(ed) else { return false };
    let step = |k: usize| if forward { (k + 1) % n } else { (k + n - 1) % n };
    // Skip binary files (nothing to show)
    let mut j = step(i);
    while r.files[j].binary && j != i {
        j = step(j);
    }
    if j == i && n > 1 {
        return false;
    }
    show(ed, j, !forward);
    true
}

/// `space v m`: marks the current file viewed (again: unmarks).
pub fn toggle_viewed(ed: &mut Editor) {
    let Some(i) = ed.code_review.as_ref().and_then(|r| r.current(ed)) else {
        return ed.set_status("no review here — space v v starts one");
    };
    let (id, version) = (ed.doc().id, ed.doc().version());
    let r = ed.code_review.as_mut().expect("checked");
    let rel = r.files[i].rel.clone();
    let now = if r.viewed.get(&rel) == Some(&(id, version)) {
        r.viewed.remove(&rel);
        false
    } else {
        r.viewed.insert(rel.clone(), (id, version));
        true
    };
    let done = r.files.iter().filter(|f| r.viewed.contains_key(&f.rel)).count();
    let n = r.files.len();
    ed.note(format!("{rel} {} — {done} of {n} viewed", if now { "viewed" } else { "not viewed" }));
}

// ── The rail's keys ───────────────────────────────────────────────────────────

/// `space e` during a review: the keys to the rail (on the file being read), or back to the code. With the
/// git tab in front: the rail back in its place, with the keys.
pub fn rail_focus(ed: &mut Editor) {
    let current = ed.code_review.as_ref().and_then(|r| r.current(ed));
    let up = ed.review_rail_up();
    let Some(r) = ed.code_review.as_mut() else { return };
    if !up {
        r.behind = false;
        r.focus = false;
        if let Some(s) = ed.sidebar.as_mut() {
            s.focus = false;
        }
    }
    r.focus = !r.focus;
    if r.focus {
        r.selected = current.or_else(|| r.rail_order().first().copied()).unwrap_or(0);
    }
}

/// Keys while the rail has them: `j`/`k` move over the files, Enter (`l`) opens one, `m` marks it viewed
/// and goes on to the next, Esc (`q`) back to the code; `space` and `:` stay the editor's. False = not the
/// rail's.
pub fn rail_key(ed: &mut Editor, key: Key) -> bool {
    if ed.mode != crate::editor::Mode::Normal || !ed.pending.is_empty() || ed.prompt.is_some() {
        return false;
    }
    if !ed.review_rail_up() {
        return false;
    }
    let Some(r) = ed.code_review.as_mut().filter(|r| r.focus) else { return false };
    let order = r.rail_order();
    let at = order.iter().position(|&i| i == r.selected).unwrap_or(0);
    let pick = |k: usize| order.get(k.min(order.len().saturating_sub(1))).copied();
    match (key.code, key.ctrl) {
        (Code::Char(' ') | Code::Char(':'), false) => return false,
        (Code::Char('j') | Code::Down, false) | (Code::Char('n'), true) => {
            r.selected = pick(at + 1).unwrap_or(r.selected)
        }
        (Code::Char('k') | Code::Up, false) | (Code::Char('p'), true) => {
            r.selected = pick(at.saturating_sub(1)).unwrap_or(r.selected)
        }
        (Code::Char('g') | Code::Home, false) => r.selected = pick(0).unwrap_or(r.selected),
        (Code::Char('G') | Code::End, false) => r.selected = pick(usize::MAX).unwrap_or(r.selected),
        (Code::Enter | Code::Char('l') | Code::Right, false) => {
            let i = r.selected;
            r.focus = false;
            show(ed, i, false);
        }
        (Code::Char('m'), false) => {
            let i = r.selected;
            show(ed, i, false);
            toggle_viewed(ed);
            if let Some(r) = ed.code_review.as_mut() {
                r.selected = pick(at + 1).unwrap_or(i);
            }
        }
        (Code::Esc | Code::Char('q'), false) => r.focus = false,
        _ => {}
    }
    true
}

impl Editor {
    /// Whether the review's rail has the left column: a review is open and the git tab isn't in front of
    /// it (closed, the tab gives the column back).
    pub fn review_rail_up(&self) -> bool {
        self.code_review.as_ref().is_some_and(|r| !r.behind || self.sidebar.is_none())
    }
}

// ── Sending ───────────────────────────────────────────────────────────────────

/// Whether `space n` writes drafts: a review is open, or drafts are waiting to go.
pub fn drafting(ed: &Editor) -> bool {
    ed.code_review.is_some() || ed.notes.list.iter().any(|n| n.thread.iter().any(|e| e.draft))
}

/// The lines around a note as a diff against the review's base: context, `-` removed, `+` added.
pub fn excerpt(doc: &crate::document::Document, lines: &std::ops::Range<usize>) -> String {
    let text = &doc.text;
    let last = crate::movement::last_line(text);
    let (from, to) = (lines.start.saturating_sub(3), (lines.end + 2).min(last));
    let line = |r: &ropey::Rope, l: usize| r.line(l).to_string().trim_end_matches(['\n', '\r']).to_string();
    let mut out = String::new();
    for l in from..=to {
        if let Some(base) = &doc.review_base {
            for h in doc.git_hunks.iter().filter(|h| h.removed > 0 && h.lines.start == l) {
                for b in h.base.clone().filter(|&b| b < base.len_lines()) {
                    out.push_str(&format!("-{}\n", line(base, b)));
                }
            }
        }
        let added =
            doc.git_hunks.iter().any(|h| h.kind != crate::git::HunkKind::Deleted && h.lines.contains(&l));
        out.push(if added { '+' } else { ' ' });
        out.push_str(&line(text, l));
        out.push('\n');
    }
    out
}

/// `space v s`: every draft to Claude in one message — each with its lines as a diff and the thread so far.
pub fn send(ed: &mut Editor) {
    let drafts: Vec<u64> =
        ed.notes.list.iter().filter(|n| n.thread.iter().any(|e| e.draft)).map(|n| n.id).collect();
    if drafts.is_empty() {
        return ed.set_status("no drafts — space n on a line writes one");
    }
    if ed.chat.as_ref().is_some_and(|c| c.busy()) {
        return ed.set_status("Claude is still answering — send the review once it's done");
    }
    let label = ed.code_review.as_ref().map_or_else(|| "changes".to_string(), |r| r.base.label());
    let mut body = format!("<review of=\"{label}\" comments=\"{}\">\n", drafts.len());
    let mut files = std::collections::BTreeSet::new();
    for id in &drafts {
        let Some(n) = ed.notes.list.iter().find(|n| n.id == *id) else { continue };
        files.insert(n.path.clone());
        let diff = ed
            .docs
            .iter()
            .find(|d| d.path.as_deref() == Some(n.path.as_path()))
            .map(|d| excerpt(d, &n.lines))
            .unwrap_or_default();
        body.push_str(&format!(
            "<comment note=\"{}\" path=\"{}\" lines=\"{}-{}\">\n<diff>\n{diff}</diff>\n",
            n.id,
            crate::chat::shown(&n.path),
            n.lines.start + 1,
            n.lines.end
        ));
        for e in &n.thread {
            body.push_str(if e.by == crate::notes::By::Claude { "claude: " } else { "you: " });
            body.push_str(&e.text);
            body.push('\n');
        }
        body.push_str("</comment>\n");
    }
    body.push_str("</review>");
    let (k, f) = (drafts.len(), files.len());
    let question = format!(
        "My review: {k} comment{} on {f} file{}. Address each — change the code where it calls for it, and answer \
         every comment on its note.",
        if k == 1 { "" } else { "s" },
        if f == 1 { "" } else { "s" }
    );
    let chip = format!(
        "review · {k} comment{} · {f} file{}",
        if k == 1 { "" } else { "s" },
        if f == 1 { "" } else { "s" }
    );
    if ed.chat.is_none() {
        crate::chat::open(ed);
        if let Some(c) = &mut ed.chat {
            c.focused = false;
            c.unfolded = false;
        }
    }
    if !crate::chat::submit(ed, &question, Some(body), Some(chip)) {
        return ed.set_warning("couldn't reach claude — the chat panel says why (the drafts are kept)");
    }
    for n in ed.notes.list.iter_mut().filter(|n| drafts.contains(&n.id)) {
        for e in &mut n.thread {
            e.draft = false;
        }
        n.waiting = true;
    }
    ed.notes.turn = drafts;
    ed.set_success(format!("review sent — {k} comment{}", if k == 1 { "" } else { "s" }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gathering a review's changes and its diff for Claude never writes the index: `git diff` over a file
    /// whose stat changed but not its text refreshes it (lock and all — measured: `core.trustctime=false`
    /// and `core.checkStat=minimal` don't stop it); `diff-index` leaves it and doesn't list that file.
    #[test]
    fn a_reviews_changes_leave_the_index() {
        let dir = std::env::temp_dir().join(format!("tarae-review-index-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let run = |args: &[&str]| {
            let id = ["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"];
            let ok = std::process::Command::new("git").args(id).args(args).current_dir(&dir).output();
            assert!(ok.unwrap().status.success(), "git {args:?}");
        };
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.join("b.txt"), "two\n").unwrap();
        run(&["init", "-q"]);
        run(&["add", "a.txt", "b.txt"]);
        run(&["commit", "-qm", "x"]);
        std::fs::write(dir.join("b.txt"), "two\nthree\n").unwrap();
        // The same text, an hour older: its stat no longer matches the index's (git counts whole seconds)
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options().write(true).open(dir.join("a.txt")).unwrap().set_modified(hour_ago).unwrap();
        let before = std::fs::read(dir.join(".git/index")).unwrap();
        let files = changed(&dir, &Base::Head).unwrap();
        let rows: Vec<_> = files.iter().map(|f| (f.rel.as_str(), f.added, f.removed)).collect();
        assert_eq!(rows, [("b.txt", 1, 0)], "only the file whose text changed");
        let diff = diff_text(&dir, "HEAD");
        assert!(diff.contains("+three") && !diff.contains("a.txt"), "{diff}");
        assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), before, "the index was rewritten");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn numstat_records() {
        let out = "3\t1\tsrc/a.rs\0-\t-\tlogo.png\x000\t12\tgone.rs\0";
        assert_eq!(
            parse_numstat(out),
            [
                (3, 1, "src/a.rs".into(), false),
                (0, 0, "logo.png".into(), true),
                (0, 12, "gone.rs".into(), false)
            ]
        );
    }
}
