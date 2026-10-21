//! Turn checkpoints — what Claude changed in one chat turn, across files, undone (and redone) as one.
//!
//! - Every edit of Claude's that the user accepts (`edits.rs`) is recorded against the turn: per file, the
//!   text before Claude's first change this turn and after its latest. The turn ending makes a checkpoint
//!   (the chat says so, with `C-z`).
//! - `space u` (`C-z` in the chat) · `:undo-turn` takes the latest turn still applied back; `space U` ·
//!   `:redo-turn` brings the latest undone one back. Per file it's a three-way step over what the file is now: Claude's changes go,
//!   the user's own edits since stay; where the user edited on or next to one of Claude's changes, that
//!   change stays too, and the toast says how many.
//! - Each file changes as one transaction (`u` there takes it back); a file that had no unsaved edits is saved
//!   again, so the disk — what Claude, tests and git read — matches. A file the turn created goes (deleted,
//!   its buffer closed) when nothing else is in it; redo writes it again.
//! - Claude is told on the next message (its memory still has its edits).
//! - Review at the end (`edits.rs`): edits Claude makes unseen keep only the text before; what it is after is
//!   read when the turn ends (`gather`), and the turn opens as a review (`codereview::start_turn`).
//! - A command of Claude's that isn't a check: what it changed in the checkout joins the turn the same way
//!   (`record_run`, `snapshot.rs`) — a file it deleted comes back with undo, one that isn't text by its git
//!   blob.
//! - Kept on disk per project (`$XDG_STATE_HOME/tarae/turns/<project hash>/` — the newest `KEEP`, and the
//!   turn in flight), written on a worker at each change: a new tarae (`:restart`, a crash) still has them. A
//!   turn cut short comes back as one of its own, its files as they are now.
//! - Kept turns come back as their questions and paths only (`Files::Kept`, #218: 26 sessions' texts held
//!   525 MB): `index.json` says them, each turn's texts are a file of their own (`<id>.json`, written once),
//!   read on a worker when a step or a review needs them (`with_texts`) — and let go again when their session
//!   goes behind (`shed`). The one file per project an earlier tarae kept (`<project hash>.json`, the texts in
//!   it) is read back and moved over; one it writes after this one's (a `:restart` into it) beside the
//!   folder joins it the same way — it never sees the folder, so never writes over it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ropey::Rope;
use serde_json::{Value, json};

use crate::editor::Editor;
use crate::transaction::{Change, Transaction};

/// One file a turn changed.
#[derive(Clone, Debug, Default)]
pub struct Touched {
    pub path: PathBuf,
    /// Before Claude's first change this turn — None: the turn created the file.
    pub before: Option<Rope>,
    /// After its latest.
    pub after: Rope,
    /// Written without the user seeing it (review at the end, `edits.rs`; a run's change, `snapshot.rs`) —
    /// `after` is read from disk when the turn ends, and the turn is reviewed then.
    pub unseen: bool,
    /// After it there's no file (a run deleted it): undo writes it again, redo deletes it.
    pub gone: bool,
    /// Not text (a run's change): what it was and is as git's blobs — taken back by writing a blob out.
    pub bytes: Option<Bytes>,
}

/// A file a run changed that isn't text, by its git blobs (None: no file).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Bytes {
    pub before: Option<String>,
    pub after: Option<String>,
}

/// One turn's changes.
#[derive(Clone, Debug)]
pub struct Checkpoint {
    /// What the user asked (its first line).
    pub question: String,
    pub files: Files,
    pub undone: bool,
    /// Its changes have been looked at: each was reviewed as it came, or the user ended its review (`space
    /// v q`), or a git review showing all its files — not one written unseen whose review a later turn's
    /// replaced (`space v t` says so).
    pub seen: bool,
    /// Its texts' file among the kept ones (`<id>.json`).
    pub id: u64,
    /// Its texts were read back from that file (`with_texts`, an earlier tarae's moved over): they go again
    /// when its session goes behind (`shed`). A turn made since tarae started keeps its own.
    pub read: bool,
    /// HEAD's moves in its runs — the files they brought aren't its own (said at its review's head).
    pub head: Vec<crate::snapshot::HeadMove>,
}

/// A turn's files: with their texts, or as kept — paths only, the texts in the turn's file.
#[derive(Clone, Debug)]
pub enum Files {
    Here(Vec<Touched>),
    Kept(Vec<Kept>),
}

/// A file of a turn kept on disk, without its texts.
#[derive(Clone, Debug, PartialEq)]
pub struct Kept {
    pub path: PathBuf,
    pub unseen: bool,
    pub gone: bool,
    pub bytes: Option<Bytes>,
}

impl Files {
    /// The files with their texts — None: they're on disk only (`with_texts`).
    pub fn here(&self) -> Option<&[Touched]> {
        match self {
            Files::Here(v) => Some(v),
            Files::Kept(_) => None,
        }
    }

    pub fn paths(&self) -> Vec<&Path> {
        match self {
            Files::Here(v) => v.iter().map(|t| t.path.as_path()).collect(),
            Files::Kept(v) => v.iter().map(|k| k.path.as_path()).collect(),
        }
    }

    pub fn len(&self) -> usize {
        match self {
            Files::Here(v) => v.len(),
            Files::Kept(v) => v.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Without their texts.
    fn kept(&self) -> Vec<Kept> {
        match self {
            Files::Here(v) => v
                .iter()
                .map(|t| Kept {
                    path: t.path.clone(),
                    unseen: t.unseen,
                    gone: t.gone,
                    bytes: t.bytes.clone(),
                })
                .collect(),
            Files::Kept(v) => v.clone(),
        }
    }
}

impl Touched {
    /// It's a change (a file changed and changed back isn't).
    fn changed(&self) -> bool {
        match (&self.bytes, &self.before) {
            (Some(b), _) => b.before != b.after,
            (None, None) => true,
            (None, Some(b)) => *b != self.after || self.gone,
        }
    }
}

impl Checkpoint {
    pub fn new(question: String, files: Vec<Touched>) -> Self {
        let seen = !files.iter().any(|t| t.unseen);
        let (files, id) = (Files::Here(files), new_id());
        Checkpoint { question, files, undone: false, seen, id, read: false, head: Vec::new() }
    }
}

/// A turn's id: when it was made (ns), one past the last given when that's later — never the same twice.
fn new_id() -> u64 {
    static LAST: AtomicU64 = AtomicU64::new(0);
    let now = crate::chats::seq().max(1);
    let next = |l: u64| l.max(now - 1) + 1;
    next(LAST.try_update(Ordering::Relaxed, Ordering::Relaxed, |l| Some(next(l))).unwrap_or_else(|l| l))
}

#[derive(Default)]
pub struct Checkpoints {
    pub list: Vec<Checkpoint>,
    /// The turn in flight.
    pub turn: Vec<Touched>,
    /// HEAD's moves in the turn in flight's runs (`Checkpoint.head`).
    pub head: Vec<crate::snapshot::HeadMove>,
    /// Where a move or copy of Claude's put a file it wrote unseen this turn, when that place held something
    /// that isn't text before (no `before` to keep for it in `turn`): named like the unseen ones all the same
    /// (`tools::moved`). Gone with the turn.
    pub carried: Vec<PathBuf>,
    /// What Claude hears with the next message (it undid or redid a turn).
    pub tell: Vec<String>,
    /// The turn that just ended wrote files unseen, being read (`gather`): its checkpoint comes after.
    pub gathering: bool,
    /// Where they're kept (the state dir's `turns/`; None: not kept — tests).
    pub dir: Option<PathBuf>,
    /// How many of `list`, from its start, came back as kept (`restore_from`) — not the turn tarae ended in
    /// the middle of, put after them, nor those made since: a turn's review carried over `:restart` counts
    /// back from the last of these (`codereview::reopen_turn`).
    pub restored: usize,
    /// Its kept turns are being read back (`restore_from`): tarae's memory isn't judged meanwhile
    /// (`Editor::restoring`).
    pub restoring: bool,
    /// A turn's texts are being read (`with_texts`): a step waits.
    pub reading: bool,
}

/// An accepted edit of Claude's landed in document `id`: `before` was its text just before (None = no such
/// file), the buffer holds the text after.
pub fn record(ed: &mut Editor, id: crate::document::DocId, before: Option<Rope>) {
    let Some((path, after)) =
        ed.docs.iter().find(|d| d.id == id).and_then(|d| Some((d.path.clone()?, d.text.clone())))
    else {
        return;
    };
    let turn = &mut ed.session.checkpoints.turn;
    match turn.iter_mut().find(|t| t.path == path) {
        Some(t) => t.after = after,
        None => {
            turn.push(Touched { path, before, after, ..Default::default() });
            keep(ed);
        }
    }
}

/// An edit of Claude's let in unasked (review at the end) to `path`, which was `before` (None = no such
/// file) — kept the first time this turn; what it is after is read when the turn ends. `then` (claude
/// told it may write) runs once that's on disk: a crash before claude writes still has what it was.
pub fn record_unseen(
    ed: &mut Editor,
    path: PathBuf,
    before: Option<Rope>,
    then: impl FnOnce(&mut Editor) + Send + 'static,
) {
    let turn = &mut ed.session.checkpoints.turn;
    match turn.iter_mut().find(|t| t.path == path) {
        Some(t) if t.unseen => then(ed),
        Some(t) => {
            t.unseen = true;
            keep_then(ed, then);
        }
        None => {
            let after = before.clone().unwrap_or_default();
            turn.push(Touched { path, before, after, unseen: true, ..Default::default() });
            keep_then(ed, then);
        }
    }
}

/// `record_unseen` for many files at once (a rename, replace or edit of Claude's, `refactor.rs`): each
/// `(path, before)` (None = no such file) — `then` once what each was is on disk, kept once for them all.
pub fn record_unseen_all(
    ed: &mut Editor,
    files: Vec<(PathBuf, Option<Rope>)>,
    then: impl FnOnce(&mut Editor) + Send + 'static,
) {
    let turn = &mut ed.session.checkpoints.turn;
    let mut new = false;
    for (path, before) in files {
        match turn.iter_mut().find(|t| t.path == path) {
            Some(t) if t.unseen => {}
            Some(t) => (t.unseen, new) = (true, true),
            None => {
                let after = before.clone().unwrap_or_default();
                turn.push(Touched { path, before, after, unseen: true, ..Default::default() });
                new = true;
            }
        }
    }
    match new {
        true => keep_then(ed, then),
        false => then(ed),
    }
}

/// Accepted changes of Claude's to many files at once (a change set accepted whole on its card,
/// `refactor.rs`): each `(path, before, after)` — before None: no such file (one it makes); after None: no
/// file after (deleted, or moved from there: undo writes it again) — `then` once what each was is on disk,
/// before any is written.
pub fn record_all(
    ed: &mut Editor,
    files: Vec<(PathBuf, Option<Rope>, Option<Rope>)>,
    then: impl FnOnce(&mut Editor) + Send + 'static,
) {
    let turn = &mut ed.session.checkpoints.turn;
    for (path, before, after) in files {
        let gone = after.is_none();
        let after = after.unwrap_or_default();
        match turn.iter_mut().find(|t| t.path == path) {
            Some(t) => (t.after, t.gone) = (after, gone),
            None => turn.push(Touched { path, before, after, gone, ..Default::default() }),
        }
    }
    keep_then(ed, then);
}

/// What a run of Claude's changed in the checkout (`snapshot.rs`) joins the turn, as written unseen: each
/// file's text before this turn kept the first time (one the turn touched already keeps its own), a deleted
/// one marked gone, one that isn't text kept by its blobs. What they are after is read when the turn ends.
pub fn record_run(ed: &mut Editor, changes: Vec<crate::snapshot::Change>) {
    add_run(&mut ed.session.checkpoints.turn, changes);
    keep(ed);
}

/// `record_run` when the turn it ran in is over (stopped meanwhile): a checkpoint of its own, called
/// `question`, with HEAD's move in it said (`head`). The files row (`checkpoint::finish`).
pub fn record_run_alone(
    ed: &mut Editor,
    question: &str,
    changes: Vec<crate::snapshot::Change>,
    head: Vec<crate::snapshot::HeadMove>,
) -> Option<String> {
    let mut files = Vec::new();
    add_run(&mut files, changes);
    finish(ed, question, files, head)
}

fn add_run(turn: &mut Vec<Touched>, changes: Vec<crate::snapshot::Change>) {
    for c in changes {
        let text = |b: &Option<crate::snapshot::Blob>| {
            b.as_ref().and_then(|b| b.text.as_deref()).map(Rope::from_str).unwrap_or_default()
        };
        let blobs = Bytes {
            before: c.before.as_ref().map(|b| b.id.clone()),
            after: c.after.as_ref().map(|b| b.id.clone()),
        };
        let (after, gone, is_text) = (text(&c.after), c.after.is_none(), c.text());
        match turn.iter_mut().find(|t| t.path == c.path) {
            Some(t) => {
                (t.unseen, t.gone, t.after) = (true, gone, after);
                // (its blob before stays this turn's first; one that was text before the run: from the run's)
                match (&mut t.bytes, is_text) {
                    (Some(b), _) => b.after = blobs.after,
                    (None, false) => t.bytes = Some(blobs),
                    (None, true) => {}
                }
            }
            None => {
                let before = c.before.as_ref().map(|_| text(&c.before));
                let bytes = (!is_text).then_some(blobs);
                turn.push(Touched { path: c.path, before, after, unseen: true, gone, bytes });
            }
        }
    }
}

/// A change of its own accepted in document `id` (a `space i` answer): one step.
pub fn single(ed: &mut Editor, id: crate::document::DocId, before: Rope, what: &str) {
    let Some((path, after)) =
        ed.docs.iter().find(|d| d.id == id).and_then(|d| Some((d.path.clone()?, d.text.clone())))
    else {
        return;
    };
    if before == after {
        return;
    }
    let question = what.lines().next().unwrap_or_default().to_string();
    let files = vec![Touched { path, before: Some(before), after, ..Default::default() }];
    ed.session.checkpoints.list.push(Checkpoint::new(question, files));
    keep(ed);
}

/// The chat turn ended: its changes become a checkpoint (a chat row says so). The files it changed, as
/// the row names them (`lib.rs, sub.rs` · `4 files`), if any.
pub fn turn_over(ed: &mut Editor, question: &str) -> Option<String> {
    ed.session.checkpoints.carried.clear();
    let files = std::mem::take(&mut ed.session.checkpoints.turn);
    let head = std::mem::take(&mut ed.session.checkpoints.head);
    finish(ed, question, files, head)
}

/// The chat turn ended having written files unseen (review at the end): what they are now is read on a
/// worker, then the checkpoint is made and `chat::turn_gathered` takes it on. False: nothing unseen —
/// `turn_over` it is.
pub fn gather(ed: &mut Editor, question: &str) -> bool {
    if !ed.session.checkpoints.turn.iter().any(|t| t.unseen) {
        return false;
    }
    ed.session.checkpoints.carried.clear();
    let files = std::mem::take(&mut ed.session.checkpoints.turn);
    let head = std::mem::take(&mut ed.session.checkpoints.head);
    ed.session.checkpoints.gathering = true;
    let question = question.to_string();
    let paths: Vec<PathBuf> = files.iter().filter(|t| t.unseen).map(|t| t.path.clone()).collect();
    ed.events.jobs().spawn(move || {
        // (gone: nothing in it now · unreadable: as it was kept) — with its fingerprint, for open buffers
        let now: Vec<(Option<Rope>, Option<crate::disk::Stamp>, bool)> = paths
            .iter()
            .map(|p| {
                let stamp = crate::disk::stamp(p);
                let (text, gone) = match std::fs::read(p) {
                    Ok(b) => (String::from_utf8(b).ok().map(|s| Rope::from_str(&s)), false),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Some(Rope::new()), true),
                    Err(_) => (None, false),
                };
                (text, stamp, gone)
            })
            .collect();
        move |ed: &mut Editor| {
            let mut files = files;
            for (p, (now, stamp, gone)) in paths.iter().zip(now) {
                let Some(now) = now else { continue };
                // An open buffer shows what Claude wrote before the review diffs it
                ed.disk_take(p, &now, stamp);
                // (one that isn't text keeps its blobs: `after` says nothing of it)
                if let Some(t) = files.iter_mut().find(|t| &t.path == p && t.bytes.is_none()) {
                    (t.after, t.gone) = (now, gone);
                }
            }
            ed.session.checkpoints.gathering = false;
            let n = finish(ed, &question, files, head);
            crate::chat::turn_gathered(ed, n);
        }
    });
    true
}

/// The turn's `files` as a checkpoint, with HEAD's moves in it said (`head`) — the files row (`lib.rs,
/// sub.rs` · `4 files`), None if none changed.
fn finish(
    ed: &mut Editor,
    question: &str,
    files: Vec<Touched>,
    head: Vec<crate::snapshot::HeadMove>,
) -> Option<String> {
    // (a file changed and changed back isn't a change)
    let files: Vec<Touched> = files.into_iter().filter(Touched::changed).collect();
    if files.is_empty() {
        // (the turn kept as in flight goes)
        keep(ed);
        return None;
    }
    let names: Vec<String> = files
        .iter()
        .map(|f| f.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
        .collect();
    let n = if names.len() <= 2 { names.join(", ") } else { format!("{} files", names.len()) };
    let question = question.lines().next().unwrap_or_default().to_string();
    ed.session.checkpoints.list.push(Checkpoint { head, ..Checkpoint::new(question, files) });
    keep(ed);
    Some(n)
}

/// Whether there's a turn to undo (the chat's `C-z` hint).
pub fn can_undo(ed: &Editor) -> bool {
    ed.session.checkpoints.list.iter().any(|c| !c.undone)
}

/// `C-z` · `:undo-turn` (`undo`) · `:redo-turn`.
pub fn step(ed: &mut Editor, undo: bool) -> Result<(), String> {
    if !ed.session.checkpoints.turn.is_empty() || ed.session.chat.as_ref().is_some_and(|c| c.busy()) {
        return Err("Claude is still working — stop it (C-c in the chat) or wait for the turn to end".into());
    }
    if ed.session.checkpoints.gathering {
        return Err("reading what Claude wrote this turn — a moment".into());
    }
    if ed.session.checkpoints.reading {
        return Err("reading that turn's changes — a moment".into());
    }
    let list = &ed.session.checkpoints.list;
    let i = if undo {
        list.iter().rposition(|c| !c.undone).ok_or("no turn of Claude's changed files — nothing to undo")?
    } else {
        list.iter().rposition(|c| c.undone).ok_or("no undone turn to bring back")?
    };
    // (kept on disk only: read first, then this step again — the turn it finds then)
    let Some(files) = list[i].files.here().map(<[Touched]>::to_vec) else {
        with_texts(ed, i, move |ed, _| {
            if let Err(e) = step(ed, undo) {
                ed.set_warning(e);
            }
        });
        return Ok(());
    };
    let question = list[i].question.clone();
    let mut changed = Vec::new();
    let mut kept = 0;
    let mut problems = Vec::new();
    let mut blobs = Vec::new();
    for f in &files {
        // (not text: its blob written out on a worker — said there if it can't be)
        if let Some(b) = &f.bytes {
            let (expect, put) = if undo { (&b.after, &b.before) } else { (&b.before, &b.after) };
            blobs.push((f.path.clone(), expect.clone(), put.clone()));
            changed.push(crate::chat::shown(&ed.session.root, &f.path));
            continue;
        }
        let after = (!f.gone).then_some(&f.after);
        let (from, to) = if undo { (after, f.before.as_ref()) } else { (f.before.as_ref(), after) };
        match file(ed, &f.path, from, to) {
            Ok((true, k)) => {
                changed.push(crate::chat::shown(&ed.session.root, &f.path));
                kept += k;
            }
            Ok((false, k)) => kept += k,
            Err(e) => problems.push(e),
        }
    }
    ed.session.checkpoints.list[i].undone = undo;
    keep(ed);
    put_blobs(ed, blobs);
    let what = if undo { "undid" } else { "brought back" };
    let files = match changed.as_slice() {
        [one] => one.clone(),
        many => format!("{} files", many.len()),
    };
    let mut msg = format!("{what} “{}” — {files}{}", quote(&question), if undo { " as before" } else { "" });
    if kept > 0 {
        msg.push_str(&format!(" · {kept} change(s) left as they are: you edited there since"));
    }
    ed.session.checkpoints.tell.push(format!(
        "The user {} your changes from the turn \"{}\" in {} — {}.",
        if undo { "undid" } else { "brought back" },
        question,
        if changed.is_empty() { "no file".to_string() } else { changed.join(", ") },
        if undo { "those files are as they were before that turn" } else { "they're applied again" },
    ));
    // The chat has the question right above: its row says only what happened
    if let Some(c) = &mut ed.session.chat {
        let row = match (undo, changed.as_slice()) {
            (_, []) => format!("{what} the turn — nothing to change"),
            (true, _) => format!("undid the turn ({files}) · space U brings it back"),
            (false, _) => format!("brought the turn back ({files})"),
        };
        c.msgs.push(crate::chat::Msg::new(crate::chat::Role::Note, row, ""));
    }
    if problems.is_empty() {
        ed.set_success(format!("{msg}{}", if undo { " · space U brings it back" } else { "" }));
    } else {
        ed.set_warning(format!("{msg} · {}", problems.join("; ")));
    }
    Ok(())
}

/// Files that aren't text, each `(path, what it should be now, what it goes to)` as git's blobs (None: no
/// file) — written out on a worker; one changed since is left as it is, and said.
fn put_blobs(ed: &mut Editor, blobs: Vec<(PathBuf, Option<String>, Option<String>)>) {
    if blobs.is_empty() {
        return;
    }
    let root = ed.session.root.clone();
    ed.events.jobs().spawn(move || {
        let problems: Vec<String> = blobs
            .iter()
            .filter_map(|(p, now, to)| crate::snapshot::put(&root, p, now.as_deref(), to.as_deref()).err())
            .collect();
        move |ed: &mut Editor| {
            if !problems.is_empty() {
                ed.set_warning(format!("not taken back: {}", problems.join("; ")));
            }
        }
    });
}

/// The question, short enough for a toast (`src/sub.rs 새 파일에 sub(a…`).
fn quote(question: &str) -> String {
    const MAX: usize = 24;
    let q: String = question.chars().take(MAX).collect();
    if question.chars().count() > MAX { format!("{}…", q.trim_end()) } else { q }
}

/// One file from `from` to `to` over what it is now (None = no file). (changed, changes kept).
pub(crate) fn file(
    ed: &mut Editor,
    path: &Path,
    from: Option<&Rope>,
    to: Option<&Rope>,
) -> Result<(bool, usize), String> {
    let shown = crate::chat::shown(&ed.session.root, path);
    let open = ed.docs.iter().any(|d| d.path.as_deref() == Some(path));
    if !open && !path.exists() && from.is_some() {
        return Err(format!("{shown} is gone"));
    }
    ed.open(path).map_err(|e| format!("{shown}: {e:#}"))?;
    if ed.doc().loading {
        return Err(format!("{shown} is still loading"));
    }
    let empty = Rope::new();
    let now = ed.doc().text.to_string();
    let (target, kept) = rebase(&from.unwrap_or(&empty).to_string(), &to.unwrap_or(&empty).to_string(), &now);
    // In the project, written through no link from its root (`beneath.rs`): a run of Claude's may have swapped
    // a folder for a link to outside since the turn. A file outside it (accepted on its review) by its path.
    let root = Some(ed.session.root.clone()).filter(|r| crate::paths::written_inside(r, path));
    // A file the turn made, with nothing else in it now: it goes
    if to.is_none() && kept == 0 && target.is_empty() {
        let id = ed.doc().id;
        match &root {
            Some(r) => remove_beneath(r, path),
            None => std::fs::remove_file(path),
        }
        .map_err(|e| format!("{shown}: {e}"))?;
        ed.close_doc(id);
        return Ok((true, 0));
    }
    if target == now {
        return Ok((false, kept));
    }
    let clean = !ed.doc().is_modified();
    let changes: Vec<Change> = crate::editdiff::line_changes(&now, &target)
        .into_iter()
        .map(|(from, to, _, insert)| Change { from, to, insert })
        .collect();
    let tx = Transaction::new(changes);
    ed.with_group(|cx| {
        let doc = cx.editor.doc_mut();
        let sel = tx.map_selection(doc.selection());
        doc.apply_with(&tx, sel);
        // (a turn stepped back on your key — not your edit)
        doc.own.not_yours();
    });
    // The disk matches again (unsaved edits of the user's stay unsaved)
    if clean {
        let id = ed.doc().id;
        ed.flush_saves(); // (written here — a background save of it finishes first)
        match root {
            // (a folder a run deleted made again on the way)
            Some(r) => {
                let doc = ed.doc_mut();
                let at = doc.check_save(false).map_err(|e| format!("{shown}: {e:#}"))?;
                let stamp = crate::saver::write_beneath(&r, &at, &doc.text, &|| {})?;
                let version = doc.version();
                doc.saved_at(version, stamp);
            }
            None => {
                // (a file a run deleted, with its folder: the folder first)
                if let Some(dir) = path.parent().filter(|d| !d.exists()) {
                    std::fs::create_dir_all(dir).map_err(|e| format!("{shown}: {e}"))?;
                }
                ed.doc_mut().save().map_err(|e| format!("{shown}: {e:#}"))?;
            }
        }
        ed.lsp_did_save(id);
        ed.undo_persist(id);
    }
    Ok((true, kept))
}

/// `path` removed from its folder, opened from `root`'s real path through no link (`beneath.rs`) — a link
/// in the way is refused, never gone through.
fn remove_beneath(root: &Path, path: &Path) -> std::io::Result<()> {
    let base = crate::beneath::Dir::root(root)?;
    let rel = path.strip_prefix(&base.at).map_err(|_| std::io::Error::other("not in the project"))?;
    let name = rel.file_name().ok_or_else(|| std::io::Error::other("no file named"))?;
    base.folder(rel.parent().unwrap_or(Path::new("")), false)?.remove(name)
}

// ── Kept on disk ────────────────────────────────────────────────────────────

/// Checkpoints kept per project (the oldest go).
const KEEP: usize = 10;

/// The question of a turn tarae ended in the middle of, when it didn't say.
const CUT_SHORT: &str = "a turn cut short";

pub fn default_dir() -> Option<PathBuf> {
    Some(crate::config::state_dir()?.join("turns"))
}

/// A project's turns as kept: the checkpoints, and the turn in flight with its question.
#[derive(Debug, Default)]
pub struct Stored {
    pub list: Vec<Checkpoint>,
    pub turn: Vec<Touched>,
    /// The turn in flight's HEAD moves (`Checkpoints.head`) — a turn cut short keeps them (review finding).
    pub head: Vec<crate::snapshot::HeadMove>,
    pub question: String,
    /// When it was taken (`chats::seq`) — a slower save of an older one doesn't replace it.
    pub seq: u64,
}

/// Saves read, compare and write the file one at a time.
static SAVING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Holds every save back while it's kept (tests: what waits for one).
#[cfg(test)]
pub fn hold_saves() -> std::sync::MutexGuard<'static, ()> {
    SAVING.lock().unwrap_or_else(|e| e.into_inner())
}

fn touched_json(t: &Touched) -> Value {
    let bytes = t.bytes.as_ref().map(|b| json!({ "before": b.before, "after": b.after }));
    json!({ "path": t.path, "before": t.before.as_ref().map(Rope::to_string), "after": t.after.to_string(),
            "unseen": t.unseen, "gone": t.gone, "bytes": bytes })
}

fn bytes_of(v: &Value) -> Option<Bytes> {
    let blob = |b: &Value| b.as_str().map(String::from);
    v.is_object().then(|| Bytes { before: blob(&v["before"]), after: blob(&v["after"]) })
}

fn touched_of(v: &Value) -> Option<Touched> {
    Some(Touched {
        path: PathBuf::from(v["path"].as_str()?),
        before: v["before"].as_str().map(Rope::from_str),
        after: Rope::from_str(v["after"].as_str()?),
        unseen: v["unseen"].as_bool().unwrap_or(false),
        gone: v["gone"].as_bool().unwrap_or(false),
        bytes: bytes_of(&v["bytes"]),
    })
}

fn files_of(v: &Value) -> Vec<Touched> {
    v.as_array().into_iter().flatten().filter_map(touched_of).collect()
}

fn kept_json(k: &Kept) -> Value {
    let bytes = k.bytes.as_ref().map(|b| json!({ "before": b.before, "after": b.after }));
    json!({ "path": k.path, "unseen": k.unseen, "gone": k.gone, "bytes": bytes })
}

fn kept_of(v: &Value) -> Option<Kept> {
    Some(Kept {
        path: PathBuf::from(v["path"].as_str()?),
        unseen: v["unseen"].as_bool().unwrap_or(false),
        gone: v["gone"].as_bool().unwrap_or(false),
        bytes: bytes_of(&v["bytes"]),
    })
}

/// The folder the project at `root`'s turns are kept in (`<dir>/<project hash>/`).
fn folder(dir: &Path, root: &Path) -> PathBuf {
    crate::chats::file(dir, root).with_extension("")
}

/// The file an earlier tarae kept the project's turns in, texts and all (`<dir>/<project hash>.json`).
fn old_file(dir: &Path, root: &Path) -> PathBuf {
    crate::chats::file(dir, root)
}

/// The index of the project's kept turns — in the folder, so an earlier tarae (`old_file`) never sees it.
fn index_file(dir: &Path, root: &Path) -> PathBuf {
    folder(dir, root).join("index.json")
}

/// `files`' texts (as kept) from their turn's file — Err: whether they're gone for good (no such file, or it
/// isn't their texts: written whole or not at all, `write_atomic`), and why. Any other failure to read it
/// (too many files open, a permission) passes: the turn stays, asked again later.
fn read_texts(path: &Path, files: &[Kept]) -> Result<Vec<Touched>, (bool, String)> {
    let body = std::fs::read_to_string(path)
        .map_err(|e| (e.kind() == std::io::ErrorKind::NotFound, e.to_string()))?;
    let v: Value = serde_json::from_str(&body).map_err(|e| (true, e.to_string()))?;
    drop(body);
    let texts =
        v.as_array().filter(|t| t.len() == files.len()).ok_or((true, "not its files' texts".to_string()))?;
    files
        .iter()
        .zip(texts)
        .map(|(k, t)| {
            Ok(Touched {
                path: k.path.clone(),
                before: t["before"].as_str().map(Rope::from_str),
                after: Rope::from_str(
                    t["after"].as_str().ok_or((true, "a file without its text after".to_string()))?,
                ),
                unseen: k.unseen,
                gone: k.gone,
                bytes: k.bytes.clone(),
            })
        })
        .collect()
}

/// The index's `seq` — it starts the file (`save`), so only its first bytes are read. 0: no index, or not one.
fn seq_of(index: &Path) -> u64 {
    use std::io::Read as _;
    let mut head = Vec::new();
    let Ok(f) = std::fs::File::open(index) else { return 0 };
    let _ = f.take(40).read_to_end(&mut head);
    let head = String::from_utf8_lossy(&head);
    let digits: String =
        head.strip_prefix("{\"seq\":").unwrap_or_default().chars().take_while(char::is_ascii_digit).collect();
    digits.parse().unwrap_or(0)
}

/// The index: its turns as kept (paths only), the turn in flight (texts and all).
fn read_index(index: &Path) -> Stored {
    let Ok(body) = std::fs::read_to_string(index) else { return Stored::default() };
    let Ok(v) = serde_json::from_str::<Value>(&body) else { return Stored::default() };
    let list = v["list"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| {
            Some(Checkpoint {
                question: c["question"].as_str().unwrap_or_default().to_string(),
                files: Files::Kept(c["files"].as_array()?.iter().filter_map(kept_of).collect()),
                undone: c["undone"].as_bool().unwrap_or(false),
                seen: c["seen"].as_bool().unwrap_or(true),
                id: c["id"].as_u64()?,
                read: false,
                head: heads_of(&c["head"]),
            })
        })
        .filter(|c| !c.files.is_empty())
        .collect();
    Stored {
        list,
        turn: files_of(&v["turn"]),
        head: heads_of(&v["head"]),
        question: v["question"].as_str().unwrap_or_default().to_string(),
        seq: v["seq"].as_u64().unwrap_or(0),
    }
}

/// HEAD moves as kept (`HeadMove::to_json`) — one unreadable is left out.
fn heads_of(v: &Value) -> Vec<crate::snapshot::HeadMove> {
    v.as_array().into_iter().flatten().filter_map(crate::snapshot::HeadMove::from_json).collect()
}

/// An earlier tarae's file (`old_file`): its turns with their texts (nothing if unreadable).
fn read_old(body: &str) -> Stored {
    let Ok(v) = serde_json::from_str::<Value>(body) else { return Stored::default() };
    let list = v["list"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| Checkpoint {
            question: c["question"].as_str().unwrap_or_default().to_string(),
            files: Files::Here(files_of(&c["files"])),
            undone: c["undone"].as_bool().unwrap_or(false),
            // (kept before this was: nothing to say about it)
            seen: c["seen"].as_bool().unwrap_or(true),
            id: new_id(),
            read: false,
            head: Vec::new(),
        })
        .filter(|c| !c.files.is_empty())
        .collect();
    Stored {
        list,
        turn: files_of(&v["turn"]),
        head: Vec::new(),
        question: v["question"].as_str().unwrap_or_default().to_string(),
        seq: v["seq"].as_u64().unwrap_or(0),
    }
}

/// The project at `root`'s turns kept in `dir`: the index's (paths only), then an earlier tarae's file's
/// (texts and all) — the turn in flight the later one's; the newest `KEEP` (nothing if none, or unreadable).
#[cfg(test)]
pub fn load(dir: &Path, root: &Path) -> Stored {
    load_both(dir, root).0
}

/// `load`, and whether an earlier tarae's file was there.
fn load_both(dir: &Path, root: &Path) -> (Stored, bool) {
    let index = read_index(&index_file(dir, root));
    let Ok(body) = std::fs::read_to_string(old_file(dir, root)) else { return (index, false) };
    let old = read_old(&body);
    drop(body);
    let mut list = index.list;
    list.extend(old.list);
    let n = list.len();
    list.drain(..n.saturating_sub(KEEP));
    let (turn, head, question, seq) = match old.seq > index.seq {
        true => (old.turn, old.head, old.question, old.seq),
        false => (index.turn, index.head, index.question, index.seq),
    };
    (Stored { list, turn, head, question, seq }, true)
}

/// `load` with every turn's texts read too (tests: what's kept, whole).
#[cfg(test)]
pub fn load_whole(dir: &Path, root: &Path) -> Stored {
    let mut s = load(dir, root);
    for c in &mut s.list {
        // (one a save removed meanwhile stays kept)
        if let Files::Kept(k) = &c.files
            && let Ok(texts) = read_texts(&folder(dir, root).join(format!("{}.json", c.id)), k)
        {
            c.files = Files::Here(texts);
        }
    }
    s
}

/// Written — unless what's there was taken later (on a worker): each turn's texts a file of its own, written
/// once (a kept turn's is there already), then the index, then the files of turns it doesn't name any more
/// go. False: not written (a later one was, or the disk said no).
fn save(dir: &Path, root: &Path, s: Stored) -> bool {
    let _one = SAVING.lock().unwrap_or_else(|e| e.into_inner());
    let (at, index) = (folder(dir, root), index_file(dir, root));
    if seq_of(&index) > s.seq || std::fs::create_dir_all(&at).is_err() {
        return false;
    }
    let write = |path: &Path, body: String| {
        crate::disk::write_atomic(path, |w| std::io::Write::write_all(w, body.as_bytes())).is_ok()
    };
    for c in &s.list {
        let Files::Here(files) = &c.files else { continue };
        let path = at.join(format!("{}.json", c.id));
        if std::fs::symlink_metadata(&path).is_ok() {
            continue;
        }
        let texts: Vec<Value> = files
            .iter()
            .map(
                |t| json!({ "before": t.before.as_ref().map(Rope::to_string), "after": t.after.to_string() }),
            )
            .collect();
        if !write(&path, Value::Array(texts).to_string()) {
            return false;
        }
    }
    let list: Vec<Value> = s
        .list
        .iter()
        .map(|c| {
            let files: Vec<Value> = c.files.kept().iter().map(kept_json).collect();
            let head: Vec<Value> = c.head.iter().map(crate::snapshot::HeadMove::to_json).collect();
            let (id, q, seen) = (c.id, &c.question, c.seen);
            json!({ "id": id, "question": q, "files": files, "undone": c.undone, "seen": seen, "head": head })
        })
        .collect();
    let turn: Vec<Value> = s.turn.iter().map(touched_json).collect();
    let head: Vec<Value> = s.head.iter().map(crate::snapshot::HeadMove::to_json).collect();
    let rest =
        json!({ "root": root, "list": list, "turn": turn, "head": head, "question": s.question }).to_string();
    // (`seq` first: a save reads only it — `seq_of`)
    if !write(&index, format!("{{\"seq\":{},{}", s.seq, &rest[1..])) {
        return false;
    }
    // The texts of turns it doesn't name (all under `SAVING`: an older save that comes after this one writes
    // nothing — `seq_of` above — so it never removes a newer turn's texts, nor names one removed)
    let named: std::collections::HashSet<String> = s.list.iter().map(|c| format!("{}.json", c.id)).collect();
    for e in std::fs::read_dir(&at).into_iter().flatten().flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        // (`<id>.json`: not the index, nor a file being written)
        let a_turns = name
            .strip_suffix(".json")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
        if a_turns && !named.contains(&name) {
            let _ = std::fs::remove_file(e.path());
        }
    }
    true
}

/// The turns as they are now, kept (the texts are written out on a worker).
pub(crate) fn keep(ed: &Editor) {
    keep_of(ed, &ed.session);
}

/// `keep` for session `l` — one that's ending (out of the editor already) too.
pub(crate) fn keep_of(ed: &Editor, l: &crate::session::Session) {
    if let Some(save) = saving(l) {
        ed.events.jobs().background(save);
    }
}

/// Tests: session `l`'s turns written now, on this thread (`keep_of` writes on a worker, unseen by events).
#[cfg(test)]
pub(crate) fn keep_now(l: &crate::session::Session) {
    if let Some(save) = saving(l) {
        save();
    }
}

/// The turns kept for the checkout in `root` forgotten (its session ended, the checkout removed) — written
/// over with none, later than any save of its still on its way (on a worker).
pub(crate) fn forget(ed: &Editor, root: PathBuf) {
    let Some(dir) = ed.turns_dir.clone() else { return };
    let none = Stored { seq: crate::chats::seq(), ..Default::default() };
    ed.events.jobs().background(move || {
        // (an earlier tarae's file too: else the next start would bring its turns back)
        if save(&dir, &root, none) {
            let _ = std::fs::remove_file(old_file(&dir, &root));
        }
    });
}

/// `keep`, then `then` on the main loop once it's written (at once when nothing is kept — tests, or
/// before `restore` has them back).
fn keep_then(ed: &mut Editor, then: impl FnOnce(&mut Editor) + Send + 'static) {
    let Some(save) = saving(&ed.session) else { return then(ed) };
    ed.events.jobs().spawn(move || {
        save();
        then
    });
}

/// Writing session `l`'s turns as they are now (for a worker) — None when they aren't kept.
fn saving(l: &crate::session::Session) -> Option<impl FnOnce() + Send + 'static> {
    let dir = l.checkpoints.dir.clone()?;
    let root = l.root.clone();
    let list = &l.checkpoints.list;
    let question = l
        .chat
        .as_ref()
        .and_then(|c| c.msgs.iter().rev().find(|m| m.role == crate::chat::Role::User))
        .map(|m| m.text.lines().next().unwrap_or_default().to_string())
        .unwrap_or_default();
    let s = Stored {
        list: list[list.len().saturating_sub(KEEP)..].to_vec(),
        turn: l.checkpoints.turn.clone(),
        head: l.checkpoints.head.clone(),
        question,
        seq: crate::chats::seq(),
    };
    Some(move || {
        save(&dir, &root, s);
    })
}

/// Turn `at`'s texts in memory, then `then(ed, at)` — at once when they are; else read from its file on a
/// worker first, the turn found again by its id (gone meanwhile: nothing more). Its file gone for good
/// (`read_texts`): said, and the turn left out — nothing of it can be stepped or shown; one that can't be read
/// just now: said, the turn kept as it is.
pub fn with_texts(ed: &mut Editor, at: usize, then: impl FnOnce(&mut Editor, usize) + Send + 'static) {
    let cps = &ed.session.checkpoints;
    let Some(cp) = cps.list.get(at) else { return };
    let Files::Kept(files) = &cp.files else { return then(ed, at) };
    if cps.reading {
        return ed.set_status("reading that turn's changes — a moment");
    }
    // (a kept turn came from `dir`: it's set as they come back)
    let Some(dir) = cps.dir.clone() else { return };
    let (id, files) = (cp.id, files.clone());
    let path = folder(&dir, &ed.session.root).join(format!("{id}.json"));
    ed.session.checkpoints.reading = true;
    ed.events.jobs().spawn(move || {
        let texts = read_texts(&path, &files);
        move |ed: &mut Editor| {
            let cps = &mut ed.session.checkpoints;
            cps.reading = false;
            let Some(at) = cps.list.iter().position(|c| c.id == id) else { return };
            match texts {
                Ok(texts) => {
                    let c = &mut cps.list[at];
                    if matches!(c.files, Files::Kept(_)) {
                        (c.files, c.read) = (Files::Here(texts), true);
                    }
                    then(ed, at);
                }
                Err((false, e)) => {
                    let question = quote(&cps.list[at].question);
                    ed.set_warning(format!(
                        "couldn't read the turn “{question}”'s changes just now ({e}) — try again"
                    ));
                }
                Err((true, e)) => {
                    // (a review on it — its texts are held while one is open, so none is: closed all the same)
                    let on_it = |ed: &Editor| {
                        matches!(ed.session.code_review.as_ref().map(|r| &r.base),
                            Some(crate::codereview::Base::Turn { at: open, .. }) if *open == at)
                    };
                    if on_it(ed) {
                        crate::codereview::close(ed);
                    }
                    let cps = &mut ed.session.checkpoints;
                    let question = cps.list.remove(at).question;
                    cps.restored -= usize::from(at < cps.restored);
                    if let Some(crate::codereview::Base::Turn { at: open, .. }) =
                        ed.session.code_review.as_mut().map(|r| &mut r.base)
                        && *open > at
                    {
                        *open -= 1;
                    }
                    keep(ed);
                    ed.set_warning(format!(
                        "the turn “{}” isn't kept any more ({e}) — left out",
                        quote(&question)
                    ));
                }
            }
        }
    });
}

/// Session `l` goes behind: the texts read back from disk go again (their files still have them) — not the
/// turn its review is open on (that reads them as it shows), nor one past the newest `KEEP` (the next save
/// removes its file).
pub(crate) fn shed(l: &mut crate::session::Session) {
    let open = match l.code_review.as_ref().map(|r| &r.base) {
        Some(crate::codereview::Base::Turn { at, .. }) => Some(*at),
        _ => None,
    };
    let from = l.checkpoints.list.len().saturating_sub(KEEP);
    for (i, c) in l.checkpoints.list.iter_mut().enumerate().skip(from) {
        if c.read && Some(i) != open {
            (c.files, c.read) = (Files::Kept(c.files.kept()), false);
        }
    }
}

/// `restore_from` the front session's own `dir` (tests set one; nothing is kept until they're back).
#[cfg(test)]
pub fn restore(ed: &mut Editor) {
    let Some(dir) = ed.session.checkpoints.dir.take() else { return };
    ed.session.checkpoints.restoring = true;
    restore_from(ed, dir, ed.session.root.clone());
}

/// As a session is made (`Editor::session_kept_back`): the kept turns of the checkout in `root` come back
/// from `dir` (read on a worker), before any made since, into the session the editor's results are for now
/// — its `dir` set once they're back (a save before would write over them). They come back as kept (paths
/// only); an earlier tarae's file's are kept as this one keeps them first, and the file goes. A turn tarae
/// ended in the middle of becomes a checkpoint of its own, its files as they are now.
pub fn restore_from(ed: &Editor, dir: PathBuf, root: PathBuf) {
    ed.events.jobs().spawn(move || {
        let back = dir.clone();
        let (mut s, old) = load_both(&dir, &root);
        if old {
            let list = s.list.clone();
            let (turn, head, question) = (s.turn.clone(), s.head.clone(), s.question.clone());
            let moved = Stored { list, turn, head, question, seq: crate::chats::seq() };
            // (not written: they stay held, texts and all — and the file stays)
            if save(&dir, &root, moved) {
                let _ = std::fs::remove_file(old_file(&dir, &root));
                for c in &mut s.list {
                    c.files = Files::Kept(c.files.kept());
                }
            }
        }
        let mut cut = std::mem::take(&mut s.turn);
        for t in cut.iter_mut().filter(|t| t.bytes.is_none()) {
            // (gone: nothing in it now · unreadable: as it was kept)
            match std::fs::read(&t.path) {
                Ok(b) => {
                    if let Ok(now) = String::from_utf8(b) {
                        (t.after, t.gone) = (Rope::from_str(&now), false);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => (t.after, t.gone) = (Rope::new(), true),
                Err(_) => {}
            }
        }
        cut.retain(Touched::changed);
        let kept = s.list.len();
        if !cut.is_empty() {
            let question = if s.question.is_empty() { CUT_SHORT.to_string() } else { s.question.clone() };
            let head = std::mem::take(&mut s.head);
            s.list.push(Checkpoint { head, ..Checkpoint::new(question, cut) });
        }
        move |ed: &mut Editor| {
            ed.session.checkpoints.dir = Some(back);
            ed.session.checkpoints.restored = kept;
            ed.session.checkpoints.restoring = false;
            let n = s.list.len();
            if n > 0 {
                let since = std::mem::take(&mut ed.session.checkpoints.list);
                ed.session.checkpoints.list = s.list.into_iter().chain(since).collect();
                // (a turn's review open already points past the ones put before it)
                if let Some(crate::codereview::Base::Turn { at, .. }) =
                    ed.session.code_review.as_mut().map(|r| &mut r.base)
                {
                    *at += n;
                }
            }
            // (a turn's review `:restart` carried: open again now its turn is back)
            if let Some(v) = ed.session.turn_review_back.take() {
                crate::codereview::reopen_turn(ed, &v);
            }
            keep(ed);
        }
    });
}

/// `base` taken to `theirs`, over `ours` (= `base` with the user's edits): the lines `base` → `theirs`
/// changes, except where `ours` changed on or next to them — those stay as `ours` has them. (text, how many
/// were left).
pub fn rebase(base: &str, theirs: &str, ours: &str) -> (String, usize) {
    if base == ours {
        return (theirs.to_string(), 0);
    }
    let mine = crate::editdiff::line_changes(base, ours);
    let wanted = crate::editdiff::line_changes(base, theirs);
    // Touching counts: two edits on neighboring lines are one place
    let near =
        |a: &(usize, usize, String, String), b: &(usize, usize, String, String)| a.0 <= b.1 && b.0 <= a.1;
    let (take, left): (Vec<_>, Vec<_>) = wanted.into_iter().partition(|w| !mine.iter().any(|m| near(w, m)));
    // Both sets are disjoint on `base` now: apply them in order
    let mut all: Vec<(usize, usize, String, String)> = mine.into_iter().chain(take).collect();
    crate::sorting::by_key(&mut all, |c| (c.0, c.1));
    let mut out = String::with_capacity(ours.len());
    let mut at = 0;
    for (from, to, _, insert) in all {
        out.push_str(&base[at..from]);
        out.push_str(&insert);
        at = to;
    }
    out.push_str(&base[at..]);
    (out, left.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rebase_keeps_the_users_edits_and_what_they_touched() {
        let before = "a\nb\nc\nd\ne\nf\ng\n";
        let after = "a\nB\nc\nd\ne\nF\ng\n"; // Claude changed b and f
        // Untouched since: straight back
        assert_eq!(rebase(after, before, after), (before.to_string(), 0));
        // The user edited d (away from both): d stays theirs, b and f go back
        let ours = "a\nB\nc\nD!\ne\nF\ng\n";
        assert_eq!(rebase(after, before, ours), ("a\nb\nc\nD!\ne\nf\ng\n".to_string(), 0));
        // The user edited the line after B: that change of Claude's stays
        let ours = "a\nB\nC!\nd\ne\nF\ng\n";
        assert_eq!(rebase(after, before, ours), ("a\nB\nC!\nd\ne\nf\ng\n".to_string(), 1));
        // Lines added by the user at the end
        let ours = "a\nB\nc\nd\ne\nF\ng\nh\n";
        assert_eq!(rebase(after, before, ours), ("a\nb\nc\nd\ne\nf\ng\nh\n".to_string(), 0));
        // A created file emptied back; the user's own addition keeps it
        assert_eq!(rebase("new\n", "", "new\n"), (String::new(), 0));
        // (the user's line right after them touches them: all stays)
        assert_eq!(rebase("x\ny\nz\n", "", "x\ny\nz\nmine\n"), ("x\ny\nz\nmine\n".to_string(), 1));
    }

    /// #218: what restoring sessions' turns costs — each session's kept turns (`KEEP` of them, every file
    /// a turn touched with its whole text before and after) read as a restore reads them (`load`: the index,
    /// paths only — an earlier tarae's one file was read whole, 525 MB held for these), 26 sessions four at a
    /// time as the workers take them. The process's footprint before, at its peak while loading, with the
    /// turns held, and once they're dropped. Stand-in turns: `TARAE_TURN_FILES` files of `TARAE_TURN_KB` KB
    /// each (3 × 150 KB).
    #[test]
    fn perf_restored_turns_memory() {
        if crate::test_results::unoptimized("checkpoint::tests::perf_restored_turns_memory") {
            return;
        }
        use super::*;
        let env = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
        let (files, kb, sessions) = (env("TARAE_TURN_FILES", 3), env("TARAE_TURN_KB", 150), 26);
        let scratch = crate::scratch::Scratch::new("perf-turns");
        let dir = scratch.to_path_buf();
        let text = |seed: usize| -> Rope {
            let line =
                format!("    let x{seed} = compute(a, b, c); // a line of a source file, as long as most\n");
            Rope::from_str(&line.repeat(kb * 1024 / line.len()))
        };
        let roots: Vec<PathBuf> = (0..sessions).map(|i| PathBuf::from(format!("/checkouts/s{i}"))).collect();
        for root in &roots {
            let list = (0..KEEP)
                .map(|t| {
                    let touched = (0..files)
                        .map(|f| Touched {
                            path: root.join(format!("src/f{f}.rs")),
                            before: Some(text(t * 10 + f)),
                            after: text(t * 10 + f + 1),
                            ..Default::default()
                        })
                        .collect();
                    Checkpoint::new(format!("turn {t}"), touched)
                })
                .collect();
            save(&dir, root, Stored { list, seq: 1, ..Default::default() });
        }
        // (each project's folder: its index and its turns' texts)
        let on_disk: u64 = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .flat_map(|e| std::fs::read_dir(e.path()).into_iter().flatten().flatten())
            .map(|e| e.metadata().unwrap().len())
            .sum();
        let me = std::process::id();
        let footprint = || {
            let os = crate::procs::tree(me).into_iter().find(|o| o.pid == me).unwrap();
            os.footprint.unwrap_or(os.rss)
        };
        let mb = |b: u64| b as f64 / (1 << 20) as f64;
        let before = footprint();
        let (stop, peak) = (std::sync::atomic::AtomicBool::new(false), std::sync::atomic::AtomicU64::new(0));
        let held: Vec<Stored> = std::thread::scope(|s| {
            crate::event::scoped(s, "tarae-peak", || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    peak.fetch_max(footprint(), std::sync::atomic::Ordering::Relaxed);
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
            });
            let mut held = Vec::new();
            for four in roots.chunks(4) {
                let loads: Vec<_> =
                    four.iter().map(|r| crate::event::scoped(s, "tarae-load", || load(&dir, r))).collect();
                held.extend(loads.into_iter().map(|l| l.join().unwrap()));
            }
            stop.store(true, std::sync::atomic::Ordering::Relaxed);
            held
        });
        let holding = footprint();
        let turns: usize = held.iter().map(|s| s.list.len()).sum();
        drop(held);
        let after = footprint();
        let peak = peak.into_inner().max(holding);
        eprintln!(
            "restored turns: {sessions} sessions × {KEEP} turns × {files} files × {kb} KB ({turns} turns, {:.0} MB \
             on disk) — footprint before {:.0} MB, peak {:.0} MB, holding {:.0} MB, dropped {:.0} MB",
            mb(on_disk),
            mb(before),
            mb(peak),
            mb(holding),
            mb(after)
        );
        assert_eq!(turns, sessions * KEEP);
    }

    /// frictions19: `space u` putting a file back in the project writes through its folders by descriptor —
    /// `sub/` swapped for a link to outside since the turn (what a sandboxed run can do) and the file there
    /// is untouched, the step says why; a file the turn made is removed the same way, never the one the
    /// link leads to. (By its real path the file was "outside" now, and written by path through the link.)
    #[test]
    fn putting_a_file_back_never_goes_through_a_link_swapped_in() {
        use super::*;
        let base = crate::scratch::Scratch::new("undo-beneath");
        let (dir, outside) = (base.join("p"), base.join("outside"));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let (dir, outside) = (std::fs::canonicalize(&dir).unwrap(), std::fs::canonicalize(&outside).unwrap());
        let mut ed = Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        for f in ["a.txt", "new.txt"] {
            std::fs::write(dir.join("sub").join(f), "after\n").unwrap();
            std::fs::write(outside.join(f), "after\n").unwrap();
            ed.open(&dir.join("sub").join(f)).unwrap();
        }
        std::fs::rename(dir.join("sub"), dir.join("sub.aside")).unwrap();
        std::os::unix::fs::symlink(&outside, dir.join("sub")).unwrap();
        let (before, after) = (Rope::from_str("before\n"), Rope::from_str("after\n"));
        // (undoing: from the turn's text back to what was before it — none, for the file it made)
        // (opened again it's where the link leads now: refused as not the project's, or as the link it is)
        let e = file(&mut ed, &dir.join("sub/a.txt"), Some(&after), Some(&before)).unwrap_err();
        assert!(e.contains("a link") || e.contains("not in"), "{e}");
        let e = file(&mut ed, &dir.join("sub/new.txt"), Some(&after), None).unwrap_err();
        assert!(e.contains("a link"), "{e}");
        for f in ["a.txt", "new.txt"] {
            assert_eq!(
                std::fs::read_to_string(outside.join(f)).unwrap(),
                "after\n",
                "{f}: never through the link"
            );
        }
    }

    /// Events handled until `until` (5 s at most).
    fn settle(ed: &mut Editor, what: &str, until: impl Fn(&Editor) -> bool) {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !until(ed) {
            assert!(std::time::Instant::now() < end, "{what}");
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(10)) {
                ed.handle_event(ev);
            }
        }
    }

    /// A temp folder of the test's own (real path), empty — the caller's to remove.
    fn temp(name: &str) -> PathBuf {
        crate::scratch::Scratch::new(name).into_path()
    }

    /// A turn that made `root`'s `a.txt` "two\n" from "one\n".
    fn one_to_two(root: &Path, question: &str) -> Checkpoint {
        let (before, after) = (Some(Rope::from_str("one\n")), Rope::from_str("two\n"));
        Checkpoint::new(
            question.into(),
            vec![Touched { path: root.join("a.txt"), before, after, ..Default::default() }],
        )
    }

    /// What an earlier tarae kept for `root` (its one file, every turn's texts in it — its `save`, as it was).
    fn write_old(store: &Path, root: &Path, questions: &[&str], seq: u64) {
        let list: Vec<Value> = questions
            .iter()
            .map(|q| {
                let Files::Here(files) = one_to_two(root, q).files else { unreachable!() };
                let files: Vec<Value> = files.iter().map(touched_json).collect();
                json!({ "question": q, "files": files, "undone": false, "seen": true })
            })
            .collect();
        std::fs::create_dir_all(store).unwrap();
        let body = json!({ "root": root, "seq": seq, "list": list, "turn": [], "question": "" });
        std::fs::write(old_file(store, root), body.to_string()).unwrap();
    }

    /// #218: kept turns come back as their paths only, in every session — the one in front and those behind
    /// — and `space u` / `space U` still take each back and bring it again across the start, its texts read
    /// from its own file as the step asks. A session going behind lets the texts it read go again (their
    /// file still has them); the one coming in front keeps what it read.
    #[test]
    fn every_sessions_turn_undoes_across_a_start_its_texts_read_as_asked() {
        use super::*;
        let dir = temp("turns-every");
        let names = ["front", "b1", "b2"];
        let store = dir.join("turns");
        for n in names {
            std::fs::create_dir_all(dir.join(n)).unwrap();
            std::fs::write(dir.join(n).join("a.txt"), "two\n").unwrap();
            let list = vec![one_to_two(&dir.join(n), &format!("turn in {n}"))];
            assert!(save(
                &store,
                &dir.join(n),
                Stored { list, seq: crate::chats::seq(), ..Default::default() }
            ));
        }
        // A new tarae: the session in front and two behind, each with its kept turn
        let mut ed = Editor::new(crate::config::Config::default());
        (ed.session.root, ed.turns_dir) = (dir.join("front"), Some(store.clone()));
        ed.session_kept_back(ed.session.id);
        let ids = [ed.session.id, ed.session_add(dir.join("b1")), ed.session_add(dir.join("b2"))];
        assert!(ed.restoring(), "reading them back");
        let turns = |ed: &Editor, id| {
            let l = std::iter::once(&ed.session).chain(&ed.sessions).find(|l| l.id == id).unwrap();
            l.checkpoints.list.clone()
        };
        settle(&mut ed, "every session's turn back", |ed| {
            !ed.restoring() && ids.iter().all(|&id| turns(ed, id).len() == 1)
        });
        let held = |ed: &Editor, id| turns(ed, id)[0].files.here().is_some();
        for id in ids {
            assert!(!held(&ed, id), "paths only: {:?}", turns(&ed, id));
        }
        for (id, n) in ids.into_iter().zip(names) {
            let a = dir.join(n).join("a.txt");
            let now = |want: &'static str| {
                let a = a.clone();
                move |_: &Editor| std::fs::read_to_string(&a).unwrap() == want
            };
            ed.with_session(id, |ed| step(ed, true).unwrap());
            settle(&mut ed, &format!("{n}: undone"), now("one\n"));
            ed.with_session(id, |ed| step(ed, false).unwrap());
            settle(&mut ed, &format!("{n}: brought back"), now("two\n"));
            assert!(held(&ed, id), "{n}: read once, held while it's where it was");
        }
        // The front one goes behind: what it read goes; b1, in front now, keeps its own
        assert!(ed.session_switch(ids[1]));
        assert!(!held(&ed, ids[0]) && held(&ed, ids[1]));
        // …and is read again when asked
        ed.with_session(ids[0], |ed| step(ed, true).unwrap());
        let a = dir.join("front/a.txt");
        settle(&mut ed, "front: undone again", |_| std::fs::read_to_string(&a).unwrap() == "one\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An earlier tarae kept a project's turns in one file, texts and all: they come back, kept as this tarae
    /// keeps them (each turn's texts a file of their own, the index), and the file goes. A `:restart` into an
    /// earlier tarae then: it reads only that one file — its reader (`read_old`, that tarae's `load`) on this
    /// index reads no turn and doesn't fail — and never sees the folder, so never writes over it; the file it
    /// writes beside it joins the index's turns at the next start, after them.
    #[test]
    fn an_earlier_taraes_turns_move_over_and_it_never_writes_over_ours() {
        use super::*;
        let dir = temp("turns-earlier");
        let (root, store) = (dir.join("p"), dir.join("turns"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        write_old(&store, &root, &["first", "second"], crate::chats::seq());
        let start = |root: &Path| {
            let mut ed = Editor::new(crate::config::Config::default());
            (ed.session.root, ed.turns_dir) = (root.to_path_buf(), Some(store.clone()));
            ed.session_kept_back(ed.session.id);
            settle(&mut ed, "its turns back", |ed| !ed.restoring());
            ed
        };
        let questions = |l: &[Checkpoint]| l.iter().map(|c| c.question.clone()).collect::<Vec<_>>();
        let mut ed = start(&root);
        assert_eq!(questions(&ed.session.checkpoints.list), ["first", "second"]);
        assert!(
            ed.session.checkpoints.list.iter().all(|c| c.files.here().is_none()),
            "kept as this tarae keeps them"
        );
        assert!(!old_file(&store, &root).exists(), "the earlier file went");
        let whole = load_whole(&store, &root);
        let texts = |c: &Checkpoint| {
            c.files.here().map(|f| (f[0].before.as_ref().map(Rope::to_string), f[0].after.to_string()))
        };
        assert!(
            whole.list.iter().all(|c| texts(c) == Some((Some("one\n".into()), "two\n".into()))),
            "{whole:?}"
        );
        step(&mut ed, true).unwrap();
        settle(&mut ed, "undone", |_| std::fs::read_to_string(root.join("a.txt")).unwrap() == "one\n");
        let index = index_file(&store, &root);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !load(&store, &root).list.last().is_some_and(|c| c.undone) {
            assert!(std::time::Instant::now() < end, "the undo kept");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        drop(ed);
        // An earlier tarae reading this index as its file: no turn, no failure
        let ours = std::fs::read_to_string(&index).unwrap();
        let read = read_old(&ours);
        assert!(read.list.is_empty() && read.turn.is_empty(), "{read:?}");
        // …it keeps a turn of its own in its file: ours is as it was
        write_old(&store, &root, &["third"], crate::chats::seq());
        assert_eq!(std::fs::read_to_string(&index).unwrap(), ours, "never written over");
        // Back in this tarae: its turn after ours, moved over too
        let ed = start(&root);
        assert_eq!(questions(&ed.session.checkpoints.list), ["first", "second", "third"]);
        assert_eq!(
            ed.session.checkpoints.list.iter().map(|c| c.undone).collect::<Vec<_>>(),
            [false, true, false]
        );
        assert!(!old_file(&store, &root).exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// HEAD's moves are kept with their turn (#225, review finding): a checkpoint's come back as they were, and
    /// the turn in flight's go with it when tarae ends mid-turn — its checkpoint, made at the next start, still
    /// says what came with HEAD (the files it left out stay out).
    #[test]
    fn head_moves_are_kept_with_their_turn_and_a_turn_cut_short() {
        let dir = temp("turns-head");
        let (root, store) = (dir.join("p"), dir.join("turns"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        let h = |short: &str, files: usize| crate::snapshot::HeadMove {
            from: "1".repeat(40),
            to: "2".repeat(40),
            short: short.into(),
            came: true,
            forward: true,
            files,
            own: 0,
        };
        let (done, flying) = (h("1a2b3c4", 90), h("5d6e7f8", 3));
        let list = vec![Checkpoint { head: vec![done.clone()], ..one_to_two(&root, "q") }];
        let t = Touched {
            path: root.join("a.txt"),
            before: Some(Rope::from_str("one\n")),
            after: Rope::from_str("two\n"),
            unseen: true,
            ..Default::default()
        };
        let (turn, head, question) = (vec![t], vec![flying.clone()], "mid-turn".to_string());
        let seq = crate::chats::seq();
        assert!(save(&store, &root, Stored { list, turn, head, question, seq }));
        // Read back as kept
        let s = load(&store, &root);
        assert_eq!((&s.list[0].head, &s.head), (&vec![done.clone()], &vec![flying.clone()]));
        // A start: the turn cut short is a checkpoint, its HEAD move with it
        let mut ed = Editor::new(crate::config::Config::default());
        (ed.session.root, ed.turns_dir) = (root.clone(), Some(store.clone()));
        ed.session_kept_back(ed.session.id);
        settle(&mut ed, "its turns back", |ed| !ed.restoring());
        let list = &ed.session.checkpoints.list;
        let got: Vec<_> = list.iter().map(|c| (c.question.as_str(), c.head.clone())).collect();
        assert_eq!(got, [("q", vec![done]), ("mid-turn", vec![flying])]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `space v t` on a turn kept from before the start: its texts read first, then its review opens on them —
    /// the file against its text before the turn — and they stay while that review is open, its session
    /// behind or not (the review's diff and its files read them as they're shown).
    #[test]
    fn a_kept_turns_review_reads_its_texts_first() {
        let dir = temp("turns-review");
        let (root, store) = (dir.join("p"), dir.join("turns"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        let list = vec![one_to_two(&root, "q")];
        assert!(save(&store, &root, Stored { list, seq: crate::chats::seq(), ..Default::default() }));
        let mut ed = Editor::new(crate::config::Config::default());
        (ed.session.root, ed.turns_dir) = (root.clone(), Some(store.clone()));
        ed.session_kept_back(ed.session.id);
        settle(&mut ed, "its turn back", |ed| !ed.restoring());
        assert!(ed.session.checkpoints.list[0].files.here().is_none());
        crate::codereview::review_turn(&mut ed);
        settle(&mut ed, "its review open", |ed| ed.session.code_review.is_some());
        let r = ed.session.code_review.as_ref().unwrap();
        let files: Vec<_> = r.files.iter().map(|f| (f.rel.as_str(), f.added, f.removed, f.new)).collect();
        assert_eq!(files, [("a.txt", 1, 1, false)]);
        shed(&mut ed.session);
        assert!(ed.session.checkpoints.list[0].files.here().is_some(), "held while its review is open");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A turn's texts that can't be read just now (here a folder where its file is — a stand-in for too many
    /// files open, as root too) leave the turn and its file as they are, said; its file gone for good leaves
    /// the turn out, said, and the next undo takes the turn before it back.
    #[test]
    fn a_turn_whose_texts_cant_be_read_now_stays_and_one_gone_is_left_out() {
        let dir = temp("turns-unread");
        let (root, store) = (dir.join("p"), dir.join("turns"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), "two\n").unwrap();
        let (first, second) = (one_to_two(&root, "first"), one_to_two(&root, "second"));
        let texts = folder(&store, &root).join(format!("{}.json", second.id));
        let list = vec![first, second];
        assert!(save(&store, &root, Stored { list, seq: crate::chats::seq(), ..Default::default() }));
        let mut ed = Editor::new(crate::config::Config::default());
        (ed.session.root, ed.turns_dir) = (root.clone(), Some(store.clone()));
        ed.session_kept_back(ed.session.id);
        settle(&mut ed, "its turns back", |ed| !ed.restoring());
        let questions =
            |ed: &Editor| ed.session.checkpoints.list.iter().map(|c| c.question.clone()).collect::<Vec<_>>();
        let said = |ed: &Editor| ed.toasts.iter().map(|t| t.text.clone()).collect::<Vec<_>>().join(" | ");
        // Can't be read just now
        std::fs::remove_file(&texts).unwrap();
        std::fs::create_dir(&texts).unwrap();
        step(&mut ed, true).unwrap();
        settle(&mut ed, "read", |ed| !ed.session.checkpoints.reading);
        assert_eq!(questions(&ed), ["first", "second"], "{}", said(&ed));
        assert!(said(&ed).contains("couldn't read the turn “second”'s changes just now"), "{}", said(&ed));
        assert!(texts.is_dir(), "left as it is");
        // Gone for good
        std::fs::remove_dir(&texts).unwrap();
        step(&mut ed, true).unwrap();
        settle(&mut ed, "read again", |ed| !ed.session.checkpoints.reading);
        assert_eq!(questions(&ed), ["first"]);
        assert!(said(&ed).contains("the turn “second” isn't kept any more"), "{}", said(&ed));
        step(&mut ed, true).unwrap();
        let a = root.join("a.txt");
        settle(&mut ed, "the turn before undone", |_| std::fs::read_to_string(&a).unwrap() == "one\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A save reads only the index's `seq` (it starts the file — an index whose rest isn't JSON still says it):
    /// an older save writes nothing; each turn's texts are written once, and those of a turn the index
    /// doesn't name any more go; forgetting leaves no texts, and no earlier tarae's file.
    #[test]
    fn a_save_reads_only_seq_and_keeps_only_the_named_turns_texts() {
        use super::*;
        let dir = temp("turns-seq");
        let (root, store) = (dir.join("p"), dir.join("turns"));
        let (a, b) = (one_to_two(&root, "a"), one_to_two(&root, "b"));
        let (at, index) = (folder(&store, &root), index_file(&store, &root));
        let texts = |c: &Checkpoint| at.join(format!("{}.json", c.id));
        let seq = crate::chats::seq();
        let stored = |list: Vec<Checkpoint>, seq| Stored { list, seq, ..Default::default() };
        assert!(save(&store, &root, stored(vec![a.clone(), b.clone()], seq)));
        assert_eq!(seq_of(&index), seq);
        assert!(texts(&a).is_file() && texts(&b).is_file());
        assert!(!save(&store, &root, stored(vec![a.clone()], seq - 1)), "an older one");
        assert!(texts(&b).is_file());
        std::fs::write(&index, format!("{{\"seq\":{},not json", seq + 1)).unwrap();
        assert_eq!(seq_of(&index), seq + 1, "only its head read");
        assert!(!save(&store, &root, stored(vec![a.clone()], seq)));
        // A later one without b: b's texts go, a's stay as written
        let a_was = std::fs::metadata(texts(&a)).unwrap().modified().unwrap();
        assert!(save(&store, &root, stored(vec![a.clone()], seq + 2)));
        assert!(!texts(&b).exists());
        assert_eq!(std::fs::metadata(texts(&a)).unwrap().modified().unwrap(), a_was, "written once");
        // Forgotten: nothing left of either kind
        write_old(&store, &root, &["old"], 1);
        let mut ed = Editor::new(crate::config::Config::default());
        ed.turns_dir = Some(store.clone());
        forget(&ed, root.clone());
        settle(&mut ed, "forgotten", |_| !texts(&a).exists() && !old_file(&store, &root).exists());
        assert!(load(&store, &root).list.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Wave 3, gap 2: a session behind (back at a start, or opened again) kept its turns nowhere it read —
    /// its first turn saved a list of one over the ones kept for its checkout. Its kept turns come back into
    /// it first, and a turn made meanwhile goes after them, on disk too.
    #[test]
    fn a_session_behind_keeps_its_turns_across_a_start() {
        use super::*;
        let scratch = crate::scratch::Scratch::new("turns-behind");
        let dir = scratch.to_path_buf();
        for p in ["front", "back"] {
            std::fs::create_dir_all(dir.join(p)).unwrap();
        }
        let (front, back, store) = (dir.join("front"), dir.join("back"), dir.join("turns"));
        let turn = |q: &str| {
            let path = back.join("a.txt");
            let (before, after) = (Some(Rope::from_str("one\n")), Rope::from_str("two\n"));
            Checkpoint::new(q.into(), vec![Touched { path, before, after, ..Default::default() }])
        };
        // Kept by an earlier tarae for the checkout behind
        let earlier = Stored {
            list: vec![turn("first"), turn("second")],
            seq: crate::chats::seq(),
            ..Default::default()
        };
        save(&store, &back, earlier);
        let mut ed = Editor::new(crate::config::Config::default());
        (ed.session.root, ed.turns_dir) = (front, Some(store.clone()));
        let id = ed.session_add(back.clone());
        // A turn there right away, before anything else came
        ed.with_session(id, |ed| {
            ed.session.checkpoints.list.push(turn("third"));
            keep(ed);
        });
        let questions = |l: &[Checkpoint]| l.iter().map(|c| c.question.clone()).collect::<Vec<_>>();
        let want = ["first", "second", "third"];
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let here = questions(&ed.session_by_id(id).unwrap().checkpoints.list);
            let kept = questions(&load(&store, &back).list);
            if here == want && kept == want {
                break;
            }
            assert!(std::time::Instant::now() < end, "in the session {here:?} · kept {kept:?}");
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(10)) {
                ed.handle_event(ev);
            }
        }
    }
}
