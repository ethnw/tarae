//! collab C2b — the merged view (`target/sb/collab-plan.md` §3.2): a file as every workspace's changes merge onto
//! the base's tip, in memory, unsaved changes too — a read-only document in a code pane, each change washed and
//! barred in its participant's colour (C1's `ui.participant.N`, its name's prefix in the sign column: never the colour
//! alone), each conflict its parts one after another (the base's lines, then each side's) — drawn side by side where
//! the pane is wide, as a folded block is (`doc_block_folded`'s one rule opens it stacked).
//!
//! Worked out on a worker (`ask`): the tip's text of the file read once per tip, each workspace's text — its open
//! buffer, else its disk text, else C2's snapshot — its hunks against its merge base with the tip (`imara-diff`,
//! histogram), a side behind the tip laid on it first (`rebase`: where its change meets the tip's own, a conflict
//! whose first part is the tip's lines), and every side's hunks laid on the tip in one pass (`lay`): hunks of two
//! sides that meet or touch are a conflict, the same change twice is one. One merge at a time per view, the last
//! one shown meanwhile; a buffer's version, a save, a snapshot or a HEAD moving asks again.
//!
//! Claude hears of it only where its own change meets another's, in the answer of the `edit` or `read` of that
//! file (`told`, as C2's) — never the merged text, never another side's lines.

use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::document::DocId;
use crate::editor::Editor;
use crate::event::SessionId;

#[cfg(test)]
mod tests;

/// A file's text read for the merge at most (larger: the view says so, nothing merged).
pub const TEXT_MAX: usize = 1 << 20;

/// Why a side is left out, as the path bar says it (`docs left out: too large`).
const TOO_LARGE: &str = "too large";
const BINARY: &str = "binary";
const NOT_PLAIN: &str = "not a plain file";

/// A side by side column's width at least (narrower: the conflict stacked).
pub const COLUMN_MIN: usize = 28;

/// What a merge was worked out from: each workspace's buffer (its id and version) or its last save (`Session.wrote`),
/// and its tree as C2 last took it (a run's changes, a HEAD moving) — one moving asks again.
type Key = Vec<(SessionId, Option<(DocId, u64)>, Option<std::time::Instant>, Option<String>)>;

/// Where the cursor goes once the first merge lands: the first conflict with that session (C2's `≈`), else none.
#[derive(Clone, Debug, PartialEq)]
pub enum Land {
    Conflict(Option<crate::overlap::Other>),
}

/// The merged view of a file, on its document (`Document.merged`).
pub struct Shown {
    /// The file from a checkout's top — the same path in each.
    pub rel: String,
    /// The base's branch (None until known: the home checkout's, read with the merge), and its tip as said (the
    /// session whose checkout has it, its colour — else the branch).
    pub branch: Option<String>,
    pub tip_name: (String, Option<u8>),
    pub sides: Vec<Side>,
    /// Each side's shortest telling prefix among the open sessions (`presence::prefixes`) — the sign column's.
    pub prefixes: Vec<String>,
    /// Workspaces of the repository with no change to it.
    pub unchanged: usize,
    /// Sessions left out, and why: their file isn't a plain file reached through no link (`read_beneath`), is too
    /// large or binary, or can't be read.
    pub left: Vec<(String, &'static str)>,
    pub merge: Arc<Merge>,
    /// The text it was set to, and its version then: an edit is put back to it.
    text: ropey::Rope,
    version: u64,
    of: Option<Key>,
    pub asking: bool,
    /// The tip and its text of the file — read once per tip.
    tip: Option<(String, Arc<String>)>,
    /// Why nothing is merged (no repository, no base, too large) — said on its header.
    pub said: Option<String>,
    pub land: Option<Land>,
    /// The code's width it was last laid out at: whether its conflicts show side by side (`columns`).
    pub cols: std::cell::Cell<usize>,
    /// Merges landed (tests count them).
    pub merges: u64,
}

/// A side of the merge: a workspace's text of the file, its session's name and colour.
#[derive(Clone, Debug, PartialEq)]
pub struct Side {
    pub session: SessionId,
    pub name: String,
    pub colour: u8,
    /// Commits its HEAD is behind the base's tip (None: not known).
    pub behind: Option<usize>,
    /// Its text differs from what it was merged from: unsaved, in a buffer.
    pub unsaved: bool,
}

/// A clean change of the merged text: its lines there (empty: a removal, above `lines.start`), whose (`side`, and
/// the sides that made the same change — `with`), and how many of the tip's lines it replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub side: usize,
    pub with: Vec<usize>,
    pub lines: Range<usize>,
    pub removed: usize,
}

/// A conflict in the merged text: its parts one after another — the tip's lines (`base`), then each side's version
/// (`parts`: the side, its lines in the merged text, its own lines in its text). `tip`: the first part is the tip's
/// own change, which a side behind it meets (the base's branch, not the merge base, is that part's name).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub base: Range<usize>,
    pub parts: Vec<Part>,
    pub tip: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Part {
    pub side: usize,
    pub lines: Range<usize>,
    pub own: Range<usize>,
}

impl Conflict {
    /// All of its lines in the merged text.
    pub fn lines(&self) -> Range<usize> {
        self.base.start..self.parts.last().map_or(self.base.end, |p| p.lines.end)
    }

    /// Its parts as (first line, last line + 1, the side — None: the tip's).
    pub fn spans(&self) -> impl Iterator<Item = (Range<usize>, Option<usize>)> + '_ {
        std::iter::once((self.base.clone(), None))
            .chain(self.parts.iter().map(|p| (p.lines.clone(), Some(p.side))))
    }
}

/// What a merge made: the merged text, its clean changes and its conflicts, in order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Merge {
    pub text: String,
    pub changes: Vec<Change>,
    pub conflicts: Vec<Conflict>,
}

impl Merge {
    /// The conflict holding merged line `l`.
    pub fn conflict_at(&self, l: usize) -> Option<usize> {
        self.conflicts.iter().position(|c| c.lines().contains(&l))
    }
}

/// A side's input to `lay`: its text, and — when its merge base's text of the file isn't the tip's — that text, to
/// lay its change on the tip first (`rebase`).
pub struct Input {
    pub text: String,
    pub merge_base: Option<String>,
}

/// A hunk of one side against the tip: the tip's lines it replaces and its own (lines of its text).
#[derive(Clone, Debug)]
struct Hunk {
    side: usize,
    base: Range<u32>,
    own: Range<u32>,
    /// It meets the tip's own change (a side behind the tip): a conflict even alone.
    tip: bool,
}

/// The document's merged view, if it is one.
pub fn of(doc: &crate::document::Document) -> Option<&Shown> {
    doc.merged.as_deref()
}

/// Whether conflict `c` of `doc` shows side by side at `cols` cells of code: each part a column of `COLUMN_MIN` at
/// least, and closed by the one rule (`Editor::doc_block_closed` — a selection wider than a character over it, or
/// INSERT/SELECT, opens it stacked).
pub fn columns(
    ed: &Editor,
    doc: &crate::document::Document,
    c: &Conflict,
    cols: usize,
    editing: bool,
) -> bool {
    let n = c.parts.len() + 1;
    let l = c.lines();
    cols >= n * COLUMN_MIN + n - 1 && !l.is_empty() && ed.doc_block_closed(doc, editing, (l.start, l.end - 1))
}

/// A column's width when `n` parts share `cols` cells (a `│` between two).
pub fn column_w(cols: usize, n: usize) -> usize {
    cols.saturating_sub(n - 1) / n.max(1)
}

/// The rows a side by side conflict takes: its head, then its longest part's lines.
pub fn column_rows(c: &Conflict) -> usize {
    1 + c.spans().map(|(r, _)| r.len()).max().unwrap_or(0)
}

/// The heads above line `l`, stacked: (the conflict, which part — 0 the tip's).
pub fn heads_at(m: &Merge, l: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for (i, c) in m.conflicts.iter().enumerate() {
        for (k, (r, _)) in c.spans().enumerate() {
            if r.start == l {
                out.push((i, k));
            }
        }
    }
    out
}

impl Shown {
    /// A part's name and colour: the tip's (`base`, or the branch where a side behind meets the tip's change), else
    /// its side's.
    pub fn part_name(&self, c: &Conflict, side: Option<usize>) -> (String, Option<u8>) {
        match side.and_then(|s| self.sides.get(s)) {
            Some(s) => (s.name.clone(), Some(s.colour)),
            None if c.tip => self.tip_name.clone(),
            None => ("base".into(), None),
        }
    }
}

/// How merged line `l` is marked: whose it is (its colour — None: a conflict's base part, or a branch no session
/// has), and on a change's first line that side's prefix (the sign column's: its name, never the colour alone).
/// None: a line no one changed.
pub fn mark(m: &Shown, l: usize) -> Option<(Option<u8>, Option<&str>)> {
    if let Some(c) = m.merge.changes.iter().find(|c| c.lines.contains(&l)) {
        let first = (c.lines.start == l).then(|| m.prefixes.get(c.side).map(String::as_str)).flatten();
        return Some((m.sides.get(c.side).map(|s| s.colour), first));
    }
    let c = m.merge.conflicts.iter().find(|c| c.lines().contains(&l))?;
    let (_, side) = c.spans().find(|(r, _)| r.contains(&l))?;
    Some((m.part_name(c, side).1, None))
}

/// Whose change holds merged line `l` — the name its first line says while the cursor is on it (`‹fix›`, C1's tag
/// rule: only while it says something new): (that first line, the name, its colour).
pub fn tag_at(m: &Shown, cursor: usize) -> Option<(usize, String, Option<u8>)> {
    if let Some(c) = m.merge.changes.iter().find(|c| c.lines.contains(&cursor)) {
        let s = m.sides.get(c.side)?;
        let mut name = s.name.clone();
        for w in &c.with {
            name += &format!(" · {}", m.sides.get(*w)?.name);
        }
        return Some((c.lines.start, name, Some(s.colour)));
    }
    None
}

/// The sign column's width for `doc`: a merged view's longest prefix, else one cell.
pub fn signs(doc: &crate::document::Document) -> usize {
    use unicode_width::UnicodeWidthStr;
    of(doc).map_or(1, |m| m.prefixes.iter().map(|p| p.width()).max().unwrap_or(1).clamp(1, 3))
}

/// The sessions, the front one first.
fn sessions(ed: &Editor) -> impl Iterator<Item = &crate::session::Session> {
    std::iter::once(&ed.session).chain(&ed.sessions)
}

/// The base's branch the workspaces land on: the front one's (its own, else the home checkout's branch), as C2's.
fn branch(ed: &Editor) -> Option<String> {
    let home = sessions(ed).find(|l| l.id == 0).and_then(|l| l.git_branch.clone());
    match ed.session.id {
        0 => home,
        _ => ed.session.base.clone().or(home),
    }
}

/// `:merged`, `space v j`: the merged view of `path` (the current file's when None) — the one open already, else a
/// new read-only document; `land`: where its cursor goes once merged.
pub fn open(ed: &mut Editor, path: Option<PathBuf>, land: Option<Land>) -> Result<(), String> {
    let path = match path {
        Some(p) => p,
        None => match (ed.doc().path.clone(), of(ed.doc())) {
            (_, Some(m)) => ed.session.root.join(&m.rel),
            (Some(p), None) => p,
            (None, None) => return Err("no file here to merge — open one".into()),
        },
    };
    let rel = match path.strip_prefix(&ed.session.root) {
        Ok(r) if !r.as_os_str().is_empty() => r.to_string_lossy().into_owned(),
        _ => return Err(format!("{} isn't in this workspace", path.display())),
    };
    if crate::sandbox::key_named_in(&ed.session.root, &path) {
        return Err(format!("{rel} is named like a key — it isn't merged"));
    }
    // (not known yet — a session just opened or switched to, its branch not read yet: the merge reads it)
    let branch = branch(ed);
    let mine: Vec<DocId> = ed.session.docs.clone();
    if let Some(i) = ed.docs.iter().position(|d| mine.contains(&d.id) && of(d).is_some_and(|m| m.rel == rel))
    {
        ed.current = i;
        if let Some(m) = ed.docs[i].merged.as_mut() {
            m.land = land.or(m.land.take());
        }
        place(ed);
        return Ok(());
    }
    let tip_name = match &branch {
        Some(b) => crate::overlap::named_branch(ed, b),
        None => (String::new(), None),
    };
    let id = ed.open_virtual(&format!("merged · {rel}"), "");
    let doc = ed.docs.iter_mut().find(|d| d.id == id).expect("just opened");
    doc.merged = Some(Box::new(Shown {
        rel,
        branch,
        tip_name,
        sides: Vec::new(),
        prefixes: Vec::new(),
        unchanged: 0,
        left: Vec::new(),
        merge: Arc::new(Merge::default()),
        text: ropey::Rope::new(),
        version: doc.version(),
        of: None,
        asking: false,
        tip: None,
        said: None,
        land,
        cols: std::cell::Cell::new(0),
        merges: 0,
    }));
    if let Some(spec) = crate::syntax::detect(&path) {
        ed.load_language(id, spec);
    }
    sync(ed);
    Ok(())
}

/// Every event: what an edit did to a merged view put back (it's read-only — said), and the views in sight merged
/// again where what they were merged from moved (one at a time each: the last merge shown meanwhile).
pub fn sync(ed: &mut Editor) {
    if !ed.docs.iter().any(|d| d.merged.is_some()) {
        return;
    }
    read_only(ed);
    let mut ids: Vec<DocId> = ed.session.views.iter().map(|v| v.doc).collect();
    ids.push(ed.doc().id);
    ids.dedup();
    for id in ids {
        let Some(doc) = ed.docs.iter().find(|d| d.id == id) else { continue };
        let Some(m) = of(doc) else { continue };
        if m.asking {
            continue;
        }
        let key = key(ed, &m.rel);
        if m.of.as_ref() != Some(&key) {
            ask(ed, id, key);
        }
    }
}

/// A merged view's text as merged.rs set it: an edit's put back (and insert mode left), said where you look.
fn read_only(ed: &mut Editor) {
    let current = ed.doc().id;
    let mut said = false;
    for d in ed.docs.iter_mut() {
        let version = d.version();
        let Some(m) = d.merged.as_mut() else { continue };
        if version == m.version {
            continue;
        }
        let text = m.text.clone();
        crate::disk::reload_text(d, &text);
        d.drop_history();
        let version = d.version();
        if let Some(m) = d.merged.as_mut() {
            m.version = version;
        }
        said |= d.id == current;
    }
    let typing = ed.mode == crate::editor::Mode::Insert && of(ed.doc()).is_some();
    if typing {
        ed.mode = crate::editor::Mode::Normal;
        ed.insert_rec = None;
    }
    if said || typing {
        ed.set_status("the merged view is read-only — edit in your workspace");
    }
}

/// What `rel`'s merge is worked out from now.
fn key(ed: &Editor, rel: &str) -> Key {
    sessions(ed)
        .map(|l| {
            let path = l.root.join(rel);
            let doc =
                ed.docs.iter().find(|d| d.path.as_deref() == Some(path.as_path()) && d.merged.is_none());
            let snap = crate::overlap::snapshot_commit(ed, l.id);
            (l.id, doc.map(|d| (d.id, d.version())), l.wrote, snap)
        })
        .collect()
}

/// Whether view `id`'s merge is of what's there now, none under way (the simulation's "still").
#[cfg(test)]
pub fn settled(ed: &Editor, id: DocId) -> bool {
    let Some(m) = ed.docs.iter().find(|d| d.id == id).and_then(of) else { return false };
    !m.asking && current(ed, m)
}

/// Whether `m`'s last merge is of what's there now: no buffer, save or snapshot of the file moved since it was asked.
fn current(ed: &Editor, m: &Shown) -> bool {
    m.of.as_ref() == Some(&key(ed, &m.rel))
}

/// A workspace as the worker reads it.
struct In {
    id: SessionId,
    name: String,
    colour: u8,
    root: PathBuf,
    /// Its open buffer's text, and whether it's unsaved.
    buffer: Option<(ropey::Rope, bool)>,
    snap: Option<crate::overlap::SnapRead>,
}

/// What a worker made of a merge.
struct Worked {
    branch: String,
    tip: Option<(String, Arc<String>)>,
    sides: Vec<Side>,
    unchanged: usize,
    /// Sessions whose file couldn't be merged (a link, a FIFO, too large, binary): left out of the merge, and why.
    left: Vec<(String, &'static str)>,
    merge: Merge,
}

/// The merge worked out on a worker, landed on view `id` (`landed`).
fn ask(ed: &mut Editor, id: DocId, key: Key) {
    let Some(m) = ed.docs.iter_mut().find(|d| d.id == id).and_then(|d| d.merged.as_mut()) else { return };
    m.asking = true;
    let (rel, branch, tip) = (m.rel.clone(), m.branch.clone(), m.tip.clone());
    let top = ed.session.root.clone();
    let home = sessions(ed).find(|l| l.id == 0).map_or_else(|| top.clone(), |l| l.root.clone());
    let ins: Vec<In> = sessions(ed)
        .map(|l| {
            let path = l.root.join(&rel);
            let doc =
                ed.docs.iter().find(|d| d.path.as_deref() == Some(path.as_path()) && d.merged.is_none());
            In {
                id: l.id,
                name: l.label().to_string(),
                colour: l.colour,
                root: l.root.clone(),
                buffer: doc.map(|d| (d.text.clone(), d.is_modified())),
                snap: crate::overlap::snapshot_reader(ed, l.id),
            }
        })
        .collect();
    ed.events.shared_jobs().spawn(move || {
        let started = crate::clock::now();
        let r = work(&top, (&rel, &home), branch, tip, ins);
        let line = match &r {
            Ok(w) => format!(
                "{rel}: {} sides, {} conflicts — {} ms",
                w.sides.len(),
                w.merge.conflicts.len(),
                crate::clock::since(started).as_millis()
            ),
            Err(e) => format!("{rel}: {e}"),
        };
        crate::log::line("merged", "info", &line);
        move |ed: &mut Editor| landed(ed, id, key, r)
    });
}

/// git for the merge: no hooks, and never a fetch for a blob this clone doesn't have.
fn git(root: &Path, args: &[&str]) -> Result<String, String> {
    let all: Vec<&str> = crate::gitrepo::NO_HOOKS.iter().copied().chain(args.iter().copied()).collect();
    let mut cmd = crate::gitrepo::repo(root)?.command(&all);
    cmd.env(crate::gitrepo::NO_LAZY_FETCH.0, crate::gitrepo::NO_LAZY_FETCH.1);
    crate::gitrepo::finish(&cmd, None).map(|b| String::from_utf8_lossy(&b).trim_end().to_string())
}

/// `rev:rel`'s text (missing there: empty) — an error past `TEXT_MAX`.
fn blob(root: &Path, rev: &str, rel: &str) -> Result<String, String> {
    if git(root, &["cat-file", "-e", &format!("{rev}:{rel}")]).is_err() {
        return Ok(String::new());
    }
    let size: usize =
        git(root, &["cat-file", "-s", &format!("{rev}:{rel}")])?.parse().map_err(|_| "no size")?;
    if size > TEXT_MAX {
        return Err("too large to merge here".into());
    }
    let all: Vec<&str> = crate::gitrepo::NO_HOOKS.iter().copied().chain(["cat-file", "blob"]).collect();
    let mut cmd = crate::gitrepo::repo(root)?.command(&all);
    cmd.arg(format!("{rev}:{rel}"));
    cmd.env(crate::gitrepo::NO_LAZY_FETCH.0, crate::gitrepo::NO_LAZY_FETCH.1);
    let b = crate::gitrepo::finish(&cmd, None)?;
    // (the base's own text: nothing to merge onto — said for the whole view)
    text_of(b).map_err(|why| match why {
        TOO_LARGE => "too large to merge here".to_string(),
        _ => "binary — nothing to merge by lines".to_string(),
    })
}

/// Bytes as a text the merge takes — else why not: past `TEXT_MAX`, or a NUL in it (binary).
fn text_of(b: Vec<u8>) -> Result<String, &'static str> {
    if b.len() > TEXT_MAX {
        return Err(TOO_LARGE);
    }
    if b.contains(&0) {
        return Err(BINARY);
    }
    Ok(String::from_utf8_lossy(&b).into_owned())
}

/// The worker's merge: the tip's text (`tip` kept when it's the same tip), each workspace of the repository's text
/// — its buffer, else its disk text, else C2's snapshot — and its merge base's where that isn't the tip, laid on
/// the tip (`lay`). A workspace with no change of the file is counted, not a side.
fn work(
    top: &Path,
    (rel, home): (&str, &Path),
    branch: Option<String>,
    tip: Option<(String, Arc<String>)>,
    ins: Vec<In>,
) -> Result<Worked, String> {
    let common = crate::worktree::common_dir(top).map_err(|_| "not in a git repository".to_string())?;
    // (the home checkout's branch, when the main loop hasn't read it yet)
    let branch = match branch {
        Some(b) => b,
        None => git(home, &["symbolic-ref", "--quiet", "--short", "HEAD"])
            .map_err(|_| "no base branch to merge onto — the home checkout's HEAD is detached".to_string())?,
    };
    let at = git(top, &["rev-parse", "--verify", "--quiet", &format!("{branch}^{{commit}}")])
        .map_err(|_| format!("no branch {branch} to merge onto"))?;
    let tip = match tip {
        Some((t, text)) if t == at => (t, text),
        _ => (at.clone(), Arc::new(blob(top, &at, rel)?)),
    };
    let (mut sides, mut inputs, mut unchanged, mut left) = (Vec::new(), Vec::new(), 0, Vec::new());
    for w in ins {
        if crate::worktree::common_dir(&w.root).ok().as_ref() != Some(&common) {
            continue;
        }
        let Ok(head) = git(&w.root, &["rev-parse", "--verify", "--quiet", "HEAD^{commit}"]) else { continue };
        let mb = git(&w.root, &["merge-base", &head, &at]).unwrap_or_else(|_| at.clone());
        let behind = git(&w.root, &["rev-list", "--count", &format!("{head}..{at}")])
            .ok()
            .and_then(|n| n.parse().ok());
        let path = w.root.join(rel);
        // (a file named like a key: never read — whose ever it is)
        if crate::sandbox::key_named_in(&w.root, &path) {
            continue;
        }
        let (text, unsaved) = match w.buffer {
            Some((t, unsaved)) => (t.to_string(), unsaved),
            // (what another workspace's run could plant there — a link, a FIFO, a huge or binary file: never gone
            // through, waited on or read whole; that side left out and said, the others merged — one copy can't
            // blank the view)
            None => match read_beneath(&w.root, rel) {
                Read::Text(b) => match text_of(b) {
                    Ok(t) => (t, false),
                    Err(why) => {
                        left.push((w.name, why));
                        continue;
                    }
                },
                Read::Gone => (String::new(), false),
                Read::TooLarge => {
                    left.push((w.name, TOO_LARGE));
                    continue;
                }
                Read::Refused(why) => {
                    crate::log::line("merged", "info", &why);
                    left.push((w.name, NOT_PLAIN));
                    continue;
                }
                Read::Failed(e) => match w.snap.as_ref().and_then(|s| s.read(rel)) {
                    Some(t) => (t, false),
                    None => {
                        crate::log::line("merged", "info", &format!("{}: {e}", path.display()));
                        left.push((w.name, "unreadable"));
                        continue;
                    }
                },
            },
        };
        let merge_base = match mb == at {
            true => None,
            false => Some(blob(&w.root, &mb, rel)?),
        };
        if text == *merge_base.as_ref().unwrap_or(&tip.1) {
            unchanged += 1;
            continue;
        }
        sides.push(Side { session: w.id, name: w.name, colour: w.colour, behind, unsaved });
        inputs.push(Input { text, merge_base });
    }
    let merge = lay(&tip.1, &inputs);
    Ok(Worked { branch, tip: Some(tip), sides, unchanged, left, merge })
}

/// What reading a workspace's file found.
enum Read {
    Text(Vec<u8>),
    /// Not there (or a folder on its way isn't): removed in that tree.
    Gone,
    TooLarge,
    /// Not a plain file reached through no link — never read.
    Refused(String),
    /// It couldn't be read (a permission, an I/O error): C2's snapshot may stand in.
    Failed(String),
}

/// File `rel` of the checkout at `root`, read as a change set writes (`beneath.rs`): every folder on its way and
/// the file opened through no link from the checkout's real path, only a plain file, its size taken from the open
/// descriptor before a byte is read (`TEXT_MAX`).
fn read_beneath(root: &Path, rel: &str) -> Read {
    let rel = Path::new(rel);
    let (Some(name), parent) = (rel.file_name(), rel.parent().unwrap_or(Path::new(""))) else {
        return Read::Refused(format!("{}: not a file's path", rel.display()));
    };
    let dir = match crate::beneath::Dir::root(root).and_then(|d| d.folder(parent, false)) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Read::Gone,
        Err(e) if e.kind() == std::io::ErrorKind::NotADirectory => return Read::Gone,
        Err(e) => return Read::Refused(e.to_string()),
    };
    match dir.kind(name) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Read::Gone,
        Err(e) => return Read::Failed(e.to_string()),
        Ok(crate::beneath::Kind::File) => {}
        Ok(k) => {
            return Read::Refused(format!(
                "{}: a {k:?}, not a plain file — not read",
                dir.at.join(name).display()
            ));
        }
    }
    match dir.read_at_most(name, TEXT_MAX) {
        Ok(Some(b)) => Read::Text(b),
        Ok(None) => Read::Gone,
        Err(e) if e.kind() == std::io::ErrorKind::FileTooLarge => Read::TooLarge,
        // (swapped since its kind was looked at — a link, a FIFO now: refused all the same)
        Err(e) if e.kind() == std::io::ErrorKind::Other => Read::Refused(e.to_string()),
        Err(e) => Read::Failed(e.to_string()),
    }
}

/// A merge landed on view `id`: its text set (the selection mapped over the change), its sides, changes and
/// conflicts — nothing drawn when nothing changed; the cursor where it was asked to land.
fn landed(ed: &mut Editor, id: DocId, key: Key, r: Result<Worked, String>) {
    let names: Vec<String> = sessions(ed).map(|l| l.label().to_string()).collect();
    let named = r.as_ref().ok().map(|w| crate::overlap::named_branch(ed, &w.branch));
    let Some(d) = ed.docs.iter_mut().find(|d| d.id == id) else {
        ed.unchanged = true;
        return;
    };
    let Some(m) = d.merged.as_mut() else { return };
    m.asking = false;
    m.of = Some(key);
    if let (Ok(w), Some(named)) = (&r, named) {
        m.branch = Some(w.branch.clone());
        m.tip_name = named;
    }
    m.merges += 1;
    let w = match r {
        Ok(w) => w,
        Err(e) => {
            if m.said.as_deref() == Some(e.as_str()) {
                ed.unchanged = true;
            }
            m.said = Some(e);
            return;
        }
    };
    let same = m.said.is_none()
        && m.sides == w.sides
        && *m.merge == w.merge
        && m.unchanged == w.unchanged
        && m.left == w.left;
    m.left = w.left;
    m.said = None;
    m.tip = w.tip;
    let mut all = names;
    for s in &w.sides {
        if !all.contains(&s.name) {
            all.push(s.name.clone());
        }
    }
    let short = crate::presence::prefixes(&all);
    m.prefixes =
        w.sides.iter().map(|s| short[all.iter().position(|n| *n == s.name).unwrap_or(0)].clone()).collect();
    m.sides = w.sides;
    m.unchanged = w.unchanged;
    let text = ropey::Rope::from_str(&w.merge.text);
    m.merge = Arc::new(w.merge);
    m.text = text.clone();
    // (its first merge: from its top — the cursor of an empty view would map past what came in)
    let first = d.text.len_bytes() == 0;
    crate::disk::reload_text(d, &text);
    if first {
        d.set_selection(crate::selection::Selection::point(0));
        (d.top, d.top_rows) = (0, None);
    }
    d.drop_history();
    let version = d.version();
    if let Some(m) = d.merged.as_mut() {
        m.version = version;
    }
    let landing = d.merged.as_mut().and_then(|m| m.land.take());
    if same && landing.is_none() {
        ed.unchanged = true;
        return;
    }
    if landing.is_some() {
        place_in(ed, id, landing);
    }
}

/// The current view's cursor where it was asked to land, once it has a merge.
fn place(ed: &mut Editor) {
    let id = ed.doc().id;
    let land = ed.doc_mut().merged.as_mut().filter(|m| m.of.is_some()).and_then(|m| m.land.take());
    if land.is_some() {
        place_in(ed, id, land);
    }
}

fn place_in(ed: &mut Editor, id: DocId, land: Option<Land>) {
    let Some(Land::Conflict(with)) = land else { return };
    if ed.doc().id != id {
        return;
    }
    let Some(m) = of(ed.doc()) else { return };
    let theirs = |c: &&Conflict| match &with {
        Some(crate::overlap::Other::Session(s)) => {
            c.parts.iter().any(|p| m.sides.get(p.side).is_some_and(|x| x.session == *s))
        }
        Some(crate::overlap::Other::Branch(_)) => c.tip,
        None => true,
    };
    let Some(line) =
        m.merge.conflicts.iter().find(theirs).or(m.merge.conflicts.first()).map(|c| c.base.start)
    else {
        return;
    };
    ed.goto_line(line);
}

/// `]m`/`[m` on a merged view: to the next (previous) conflict — its lines selected. None: not a merged view.
pub fn step(ed: &mut Editor, forward: bool) -> Option<()> {
    let m = of(ed.doc())?;
    let line = ed.cursor_line();
    let spans: Vec<Range<usize>> = m.merge.conflicts.iter().map(Conflict::lines).collect();
    let none = spans.is_empty();
    let target = match forward {
        true => spans.iter().find(|r| r.start > line),
        false => spans.iter().rev().find(|r| r.start < line && !r.contains(&line)),
    }
    .cloned();
    match target {
        Some(r) => ed.goto_line(r.start),
        None => ed.set_status(if none { "no conflicts — it merges clean" } else { "no more conflicts" }),
    }
    Some(())
}

/// `]g`/`[g` on a merged view: its changes and conflicts as hunks, in order (`codeview::Changes::Merged`).
pub fn hunks(m: &Shown) -> Vec<crate::git::Hunk> {
    use crate::git::{Hunk, HunkKind};
    let mut out: Vec<Hunk> = m
        .merge
        .changes
        .iter()
        .map(|c| Hunk {
            kind: match (c.lines.is_empty(), c.removed) {
                (true, _) => HunkKind::Deleted,
                (false, 0) => HunkKind::Added,
                _ => HunkKind::Modified,
            },
            lines: c.lines.clone(),
            removed: c.removed,
            base: 0..0,
        })
        .chain(m.merge.conflicts.iter().map(|c| Hunk {
            kind: HunkKind::Modified,
            lines: c.lines(),
            removed: c.base.len(),
            base: 0..0,
        }))
        .collect();
    crate::sorting::by_key(&mut out, |h| h.lines.start);
    out
}

/// What Claude's `edit` or `read` answer of `path` says of the merged view: where a merge of that file was worked
/// out with its own session's change in it, a line per conflict its change is in — `src/a.rs: merged with fix's
/// change: conflict at L30–34` (its own lines, the other's name) — nothing when it merges clean. Never the merged
/// text, never another side's lines. None: no merge of it worked out (C2's line stands).
pub fn told(ed: &Editor, path: &Path) -> Option<Vec<String>> {
    let me = ed.session.id;
    let rel = path.strip_prefix(&ed.session.root).ok()?.to_string_lossy().into_owned();
    if crate::sandbox::key_named_in(&ed.session.root, path) {
        return None;
    }
    let m = sessions(ed)
        .flat_map(|l| ed.docs.iter().filter(|d| l.docs.contains(&d.id)))
        .filter_map(of)
        // (only a merge of what's there now: one from before Claude's own edit could tell it of a conflict it just
        // resolved — C2's line stands meanwhile)
        .find(|m| m.rel == rel && m.said.is_none() && current(ed, m))?;
    let mine = m.sides.iter().position(|s| s.session == me)?;
    let mut out = Vec::new();
    for c in &m.merge.conflicts {
        let Some(p) = c.parts.iter().find(|p| p.side == mine) else { continue };
        let others: Vec<String> = c
            .spans()
            .filter(|(_, s)| *s != Some(mine))
            .filter(|(_, s)| s.is_some() || c.tip)
            .map(|(_, s)| crate::cut::fit_ellipsis(&m.part_name(c, s).0, 32))
            .collect();
        let who = match others.is_empty() {
            true => "another".to_string(),
            false => others.join(" and "),
        };
        let own = p.own.start..p.own.end.max(p.own.start + 1);
        out.push(format!(
            "{rel}: merged with {who}'s change: conflict at {}",
            crate::overlap::lines_said(&own)
        ));
    }
    Some(out)
}

/// `text` split in lines as the diff counts them (each with its `\n`).
fn lines(text: &str) -> Vec<&str> {
    text.split_inclusive('\n').collect()
}

/// `side`'s changes of `base`: (base lines, its lines).
fn diff(base: &str, side: &str) -> Vec<(Range<u32>, Range<u32>)> {
    use imara_diff::intern::InternedInput;
    use imara_diff::{Algorithm, diff};
    let input = InternedInput::new(base, side);
    let mut out = Vec::new();
    diff(Algorithm::Histogram, &input, |before: Range<u32>, after: Range<u32>| out.push((before, after)));
    out
}

/// Whether two hunks' base lines meet or touch (git conflicts on both).
fn meet(x: &Range<u32>, y: &Range<u32>) -> bool {
    x.start <= y.end && y.start <= x.end
}

/// A side's version of base lines `span` with its hunks there laid on them: (its text, its own first line there).
fn version<'a>(base: &[&'a str], own: &[&'a str], hunks: &[&Hunk], span: Range<u32>) -> (Vec<&'a str>, u32) {
    let mut out = Vec::new();
    let mut at = span.start;
    let mut first = None;
    for h in hunks {
        // (the base's lines before it are its own too — its own line there counted back from the hunk)
        first.get_or_insert(h.own.start - (h.base.start - at));
        out.extend(base[at as usize..h.base.start as usize].iter().copied());
        out.extend(own[h.own.start as usize..h.own.end as usize].iter().copied());
        at = h.base.end;
    }
    let start = first.unwrap_or(0);
    out.extend(base[at as usize..span.end as usize].iter().copied());
    (out, start)
}

/// `ours` (a side's text, made on `merge_base`) laid on `tip`: the text it would be on the tip, and the tip's lines
/// where its change meets the tip's own — its version there (the tip's change is the conflict's other part).
pub fn rebase(merge_base: &str, tip: &str, ours: &str) -> (String, Vec<Range<u32>>) {
    let (mb, t, o) = (lines(merge_base), lines(tip), lines(ours));
    let mut hunks: Vec<(u8, Range<u32>, Range<u32>)> = Vec::new();
    hunks.extend(diff(merge_base, tip).into_iter().map(|(b, x)| (0, b, x)));
    hunks.extend(diff(merge_base, ours).into_iter().map(|(b, x)| (1, b, x)));
    crate::sorting::by_key(&mut hunks, |h| (h.1.start, h.1.end, h.0));
    let (mut out, mut met) = (String::new(), Vec::new());
    let (mut at, mut tip_at) = (0u32, 0u32);
    let mut i = 0;
    while i < hunks.len() {
        let (mut span, mut j) = (hunks[i].1.clone(), i + 1);
        while j < hunks.len() && meet(&span, &hunks[j].1) {
            span.end = span.end.max(hunks[j].1.end);
            j += 1;
        }
        let group = &hunks[i..j];
        for l in &mb[at as usize..span.start as usize] {
            out.push_str(l);
        }
        tip_at += span.start - at;
        let side = |k: u8, own: &[&str]| -> Vec<String> {
            let mut v = Vec::new();
            let mut a = span.start;
            for h in group.iter().filter(|h| h.0 == k) {
                v.extend(mb[a as usize..h.1.start as usize].iter().map(|s| s.to_string()));
                v.extend(own[h.2.start as usize..h.2.end as usize].iter().map(|s| s.to_string()));
                a = h.1.end;
            }
            v.extend(mb[a as usize..span.end as usize].iter().map(|s| s.to_string()));
            v
        };
        let (theirs, mine) = (side(0, &t), side(1, &o));
        let both = group.iter().any(|h| h.0 == 0) && group.iter().any(|h| h.0 == 1);
        let tip_len = theirs.len() as u32;
        // (the same change both ways is the tip's: nothing of ours left there)
        let take = match (both, theirs == mine) {
            (true, false) => {
                met.push(tip_at..tip_at + tip_len);
                &mine
            }
            _ if group.iter().any(|h| h.0 == 1) => &mine,
            _ => &theirs,
        };
        for l in take {
            out.push_str(l);
        }
        tip_at += tip_len;
        at = span.end;
        i = j;
    }
    for l in &mb[at as usize..] {
        out.push_str(l);
    }
    (out, met)
}

/// A side's laid text's lines as lines of its own text — None where the two are one (nothing laid on the tip).
fn own_map(laid: &str, side: &Input) -> Option<crate::presence::LineMap> {
    (laid != side.text).then(|| crate::presence::LineMap::of(laid, &side.text))
}

/// Every side laid on `tip` in one pass: each side's hunks against it (a side behind first laid on it — `rebase`),
/// the hunks of two sides that meet or touch a conflict, the same change by several one change.
pub fn lay(tip: &str, sides: &[Input]) -> Merge {
    let base = lines(tip);
    let laid: Vec<(String, Vec<Range<u32>>)> = sides
        .iter()
        .map(|s| match &s.merge_base {
            Some(mb) if mb != tip => rebase(mb, tip, &s.text),
            _ => (s.text.clone(), Vec::new()),
        })
        .collect();
    let mut hunks: Vec<Hunk> = Vec::new();
    for (i, (text, met)) in laid.iter().enumerate() {
        hunks.extend(diff(tip, text).into_iter().map(|(b, own)| {
            let tip = met.iter().any(|m| meet(m, &b));
            Hunk { side: i, base: b, own, tip }
        }));
    }
    crate::sorting::by_key(&mut hunks, |h| (h.base.start, h.base.end, h.side));
    let owns: Vec<Vec<&str>> = laid.iter().map(|t| lines(&t.0)).collect();
    let maps: Vec<std::cell::OnceCell<Option<crate::presence::LineMap>>> =
        laid.iter().map(|_| std::cell::OnceCell::new()).collect();
    let mut m = Merge::default();
    let mut n = 0usize;
    // (a line ends before the next one starts — a side's last line without its `\n` mid-conflict too)
    let push = |m: &mut Merge, n: &mut usize, l: &str| {
        if !m.text.is_empty() && !m.text.ends_with('\n') {
            m.text.push('\n');
        }
        m.text.push_str(l);
        *n += 1;
    };
    let mut at = 0u32;
    let mut i = 0;
    while i < hunks.len() {
        let (mut span, mut j) = (hunks[i].base.clone(), i + 1);
        while j < hunks.len() && meet(&span, &hunks[j].base) {
            span.end = span.end.max(hunks[j].base.end);
            j += 1;
        }
        let group: Vec<&Hunk> = hunks[i..j].iter().collect();
        for l in &base[at as usize..span.start as usize] {
            push(&mut m, &mut n, l);
        }
        // (each side once: a side's hunks needn't be neighbours in the group — another's between them)
        let mut by: Vec<usize> = group.iter().map(|h| h.side).collect();
        crate::sorting::by_key(&mut by, |s| *s);
        by.dedup();
        let versions: Vec<(usize, Vec<&str>, u32)> = by
            .iter()
            .map(|&s| {
                let of: Vec<&Hunk> = group.iter().copied().filter(|h| h.side == s).collect();
                let (v, own) = version(&base, &owns[s], &of, span.clone());
                (s, v, own)
            })
            .collect();
        let same = versions.windows(2).all(|w| w[0].1 == w[1].1);
        let tip_met = group.iter().any(|h| h.tip);
        if same && !tip_met {
            // One change (or the same by several): its lines, the first side's
            let start = n;
            for l in &versions[0].1 {
                push(&mut m, &mut n, l);
            }
            let with = versions[1..].iter().map(|v| v.0).collect();
            let removed = (span.end - span.start) as usize;
            m.changes.push(Change { side: versions[0].0, with, lines: start..n, removed });
        } else {
            let start = n;
            for l in &base[span.start as usize..span.end as usize] {
                push(&mut m, &mut n, l);
            }
            let base_lines = start..n;
            let mut parts = Vec::new();
            for (s, v, own) in &versions {
                let start = n;
                for l in v {
                    push(&mut m, &mut n, l);
                }
                // (in its own text: a side laid on the tip first has the tip's lines in its laid text — mapped back)
                let own = *own as usize..*own as usize + v.len();
                let own = match maps[*s].get_or_init(|| own_map(&laid[*s].0, &sides[*s])) {
                    Some(map) if own.is_empty() => {
                        let at = map.range(own.start..own.start + 1).start;
                        at..at
                    }
                    Some(map) => map.range(own),
                    None => own,
                };
                parts.push(Part { side: *s, lines: start..n, own });
            }
            m.conflicts.push(Conflict { base: base_lines, parts, tip: tip_met });
        }
        at = span.end;
        i = j;
    }
    for l in &base[at as usize..] {
        push(&mut m, &mut n, l);
    }
    m
}
