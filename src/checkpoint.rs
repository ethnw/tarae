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
//! - Kept on disk per project (`$XDG_STATE_HOME/tarae/turns/<project hash>.json` — the newest `KEEP`, and
//!   the turn in flight), written on a worker at each change: a new tarae (`:restart`, a crash) still has
//!   them. A turn cut short comes back as one of its own, its files as they are now.

use std::path::{Path, PathBuf};

use ropey::Rope;
use serde_json::{Value, json};

use crate::editor::Editor;
use crate::transaction::{Change, Transaction};

/// One file a turn changed.
#[derive(Clone, Debug)]
pub struct Touched {
    pub path: PathBuf,
    /// Before Claude's first change this turn — None: the turn created the file.
    pub before: Option<Rope>,
    /// After its latest.
    pub after: Rope,
    /// Written without the user seeing it (review at the end, `edits.rs`) — `after` is read from disk when
    /// the turn ends, and the turn is reviewed then.
    pub unseen: bool,
}

/// One turn's changes.
#[derive(Clone, Debug)]
pub struct Checkpoint {
    /// What the user asked (its first line).
    pub question: String,
    pub files: Vec<Touched>,
    pub undone: bool,
    /// Its changes have been looked at: each was reviewed as it came, or the user ended its review (`space
    /// v q`), or a git review showing all its files — not one written unseen whose review a later turn's
    /// replaced (`space v t` says so).
    pub seen: bool,
}

impl Checkpoint {
    pub fn new(question: String, files: Vec<Touched>) -> Self {
        let seen = !files.iter().any(|t| t.unseen);
        Checkpoint { question, files, undone: false, seen }
    }
}

#[derive(Default)]
pub struct Checkpoints {
    pub list: Vec<Checkpoint>,
    /// The turn in flight.
    pub turn: Vec<Touched>,
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
            turn.push(Touched { path, before, after, unseen: false });
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
            turn.push(Touched { path, before, after, unseen: true });
            keep_then(ed, then);
        }
    }
}

/// `record_unseen` for many files at once (a rename or replace of Claude's, `refactor.rs`): `then` once
/// what each was is on disk — kept once for them all.
pub fn record_unseen_all(
    ed: &mut Editor,
    files: Vec<(PathBuf, Rope)>,
    then: impl FnOnce(&mut Editor) + Send + 'static,
) {
    let turn = &mut ed.session.checkpoints.turn;
    let mut new = false;
    for (path, before) in files {
        match turn.iter_mut().find(|t| t.path == path) {
            Some(t) if t.unseen => {}
            Some(t) => (t.unseen, new) = (true, true),
            None => {
                turn.push(Touched { path, before: Some(before.clone()), after: before, unseen: true });
                new = true;
            }
        }
    }
    match new {
        true => keep_then(ed, then),
        false => then(ed),
    }
}

/// Accepted changes of Claude's to many files at once (a rename or replace accepted whole on its card,
/// `refactor.rs`): each `(path, before, after)` — `then` once what each was is on disk, before any is written.
pub fn record_all(
    ed: &mut Editor,
    files: Vec<(PathBuf, Rope, Rope)>,
    then: impl FnOnce(&mut Editor) + Send + 'static,
) {
    let turn = &mut ed.session.checkpoints.turn;
    for (path, before, after) in files {
        match turn.iter_mut().find(|t| t.path == path) {
            Some(t) => t.after = after,
            None => turn.push(Touched { path, before: Some(before), after, unseen: false }),
        }
    }
    keep_then(ed, then);
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
    let files = vec![Touched { path, before: Some(before), after, unseen: false }];
    ed.session.checkpoints.list.push(Checkpoint::new(question, files));
    keep(ed);
}

/// The chat turn ended: its changes become a checkpoint (a chat row says so). The files it changed, as
/// the row names them (`lib.rs, sub.rs` · `4 files`), if any.
pub fn turn_over(ed: &mut Editor, question: &str) -> Option<String> {
    ed.session.checkpoints.carried.clear();
    let files = std::mem::take(&mut ed.session.checkpoints.turn);
    finish(ed, question, files)
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
    ed.session.checkpoints.gathering = true;
    let question = question.to_string();
    let paths: Vec<PathBuf> = files.iter().filter(|t| t.unseen).map(|t| t.path.clone()).collect();
    ed.events.jobs().spawn(move || {
        // (gone: nothing in it now · unreadable: as it was kept) — with its fingerprint, for open buffers
        let now: Vec<(Option<Rope>, Option<crate::disk::Stamp>)> = paths
            .iter()
            .map(|p| {
                let stamp = crate::disk::stamp(p);
                let text = match std::fs::read(p) {
                    Ok(b) => String::from_utf8(b).ok().map(|s| Rope::from_str(&s)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(Rope::new()),
                    Err(_) => None,
                };
                (text, stamp)
            })
            .collect();
        move |ed: &mut Editor| {
            let mut files = files;
            for (p, (now, stamp)) in paths.iter().zip(now) {
                let Some(now) = now else { continue };
                // An open buffer shows what Claude wrote before the review diffs it
                ed.disk_take(p, &now, stamp);
                if let Some(t) = files.iter_mut().find(|t| &t.path == p) {
                    t.after = now;
                }
            }
            ed.session.checkpoints.gathering = false;
            let n = finish(ed, &question, files);
            crate::chat::turn_gathered(ed, n);
        }
    });
    true
}

/// The turn's `files` as a checkpoint — the files row (`lib.rs, sub.rs` · `4 files`), None if none changed.
fn finish(ed: &mut Editor, question: &str, files: Vec<Touched>) -> Option<String> {
    // (a file changed and changed back isn't a change)
    let files: Vec<Touched> =
        files.into_iter().filter(|t| t.before.as_ref().is_none_or(|b| *b != t.after)).collect();
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
    ed.session.checkpoints.list.push(Checkpoint::new(question, files));
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
    let list = &ed.session.checkpoints.list;
    let i = if undo {
        list.iter().rposition(|c| !c.undone).ok_or("no turn of Claude's changed files — nothing to undo")?
    } else {
        list.iter().rposition(|c| c.undone).ok_or("no undone turn to bring back")?
    };
    let cp = list[i].clone();
    let mut changed = Vec::new();
    let mut kept = 0;
    let mut problems = Vec::new();
    for f in &cp.files {
        let (from, to) =
            if undo { (Some(&f.after), f.before.as_ref()) } else { (f.before.as_ref(), Some(&f.after)) };
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
    let what = if undo { "undid" } else { "brought back" };
    let files = match changed.as_slice() {
        [one] => one.clone(),
        many => format!("{} files", many.len()),
    };
    let mut msg =
        format!("{what} “{}” — {files}{}", quote(&cp.question), if undo { " as before" } else { "" });
    if kept > 0 {
        msg.push_str(&format!(" · {kept} change(s) left as they are: you edited there since"));
    }
    ed.session.checkpoints.tell.push(format!(
        "The user {} your changes from the turn \"{}\" in {} — {}.",
        if undo { "undid" } else { "brought back" },
        cp.question,
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

/// The question, short enough for a toast (`src/sub.rs 새 파일에 sub(a…`).
fn quote(question: &str) -> String {
    const MAX: usize = 24;
    let q: String = question.chars().take(MAX).collect();
    if question.chars().count() > MAX { format!("{}…", q.trim_end()) } else { q }
}

/// One file from `from` to `to` over what it is now (None = no file). (changed, changes kept).
fn file(
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
    // A file the turn made, with nothing else in it now: it goes
    if to.is_none() && kept == 0 && target.is_empty() {
        let id = ed.doc().id;
        std::fs::remove_file(path).map_err(|e| format!("{shown}: {e}"))?;
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
    });
    // The disk matches again (unsaved edits of the user's stay unsaved)
    if clean {
        let id = ed.doc().id;
        ed.flush_saves(); // (written here — a background save of it finishes first)
        ed.doc_mut().save().map_err(|e| format!("{shown}: {e:#}"))?;
        ed.lsp_did_save(id);
        ed.undo_persist(id);
    }
    Ok((true, kept))
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
    json!({ "path": t.path, "before": t.before.as_ref().map(Rope::to_string), "after": t.after.to_string(),
            "unseen": t.unseen })
}

fn touched_of(v: &Value) -> Option<Touched> {
    Some(Touched {
        path: PathBuf::from(v["path"].as_str()?),
        before: v["before"].as_str().map(Rope::from_str),
        after: Rope::from_str(v["after"].as_str()?),
        unseen: v["unseen"].as_bool().unwrap_or(false),
    })
}

fn files_of(v: &Value) -> Vec<Touched> {
    v.as_array().into_iter().flatten().filter_map(touched_of).collect()
}

/// The project at `root`'s turns kept in `dir` (nothing if none — or unreadable).
pub fn load(dir: &Path, root: &Path) -> Stored {
    let Ok(body) = std::fs::read_to_string(crate::chats::file(dir, root)) else { return Stored::default() };
    let Ok(v) = serde_json::from_str::<Value>(&body) else { return Stored::default() };
    let list = v["list"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|c| Checkpoint {
            question: c["question"].as_str().unwrap_or_default().to_string(),
            files: files_of(&c["files"]),
            undone: c["undone"].as_bool().unwrap_or(false),
            // (kept before this was: nothing to say about it)
            seen: c["seen"].as_bool().unwrap_or(true),
        })
        .filter(|c| !c.files.is_empty())
        .collect();
    Stored {
        list,
        turn: files_of(&v["turn"]),
        question: v["question"].as_str().unwrap_or_default().to_string(),
        seq: v["seq"].as_u64().unwrap_or(0),
    }
}

/// Written — unless what's there was taken later (on a worker).
fn save(dir: &Path, root: &Path, s: Stored) {
    let _one = SAVING.lock().unwrap_or_else(|e| e.into_inner());
    if load(dir, root).seq > s.seq {
        return;
    }
    let list: Vec<Value> = s
        .list
        .iter()
        .map(|c| {
            let files: Vec<Value> = c.files.iter().map(touched_json).collect();
            json!({ "question": c.question, "files": files, "undone": c.undone, "seen": c.seen })
        })
        .collect();
    let turn: Vec<Value> = s.turn.iter().map(touched_json).collect();
    let body =
        json!({ "root": root, "seq": s.seq, "list": list, "turn": turn, "question": s.question }).to_string();
    let _ = std::fs::create_dir_all(dir);
    let _ = crate::disk::write_atomic(&crate::chats::file(dir, root), |w| {
        std::io::Write::write_all(w, body.as_bytes())
    });
}

/// The turns as they are now, kept (the texts are written out on a worker).
pub(crate) fn keep(ed: &Editor) {
    if let Some(save) = saving(ed) {
        ed.events.jobs().background(save);
    }
}

/// `keep`, then `then` on the main loop once it's written (at once when nothing is kept — tests, or
/// before `restore` has them back).
fn keep_then(ed: &mut Editor, then: impl FnOnce(&mut Editor) + Send + 'static) {
    let Some(save) = saving(ed) else { return then(ed) };
    ed.events.jobs().spawn(move || {
        save();
        then
    });
}

/// Writing the turns as they are now (for a worker) — None when they aren't kept.
fn saving(ed: &Editor) -> Option<impl FnOnce() + Send + 'static> {
    let dir = ed.session.checkpoints.dir.clone()?;
    let root = ed.session.root.clone();
    let list = &ed.session.checkpoints.list;
    let question = ed
        .session
        .chat
        .as_ref()
        .and_then(|c| c.msgs.iter().rev().find(|m| m.role == crate::chat::Role::User))
        .map(|m| m.text.lines().next().unwrap_or_default().to_string())
        .unwrap_or_default();
    let s = Stored {
        list: list[list.len().saturating_sub(KEEP)..].to_vec(),
        turn: ed.session.checkpoints.turn.clone(),
        question,
        seq: crate::chats::seq(),
    };
    Some(move || save(&dir, &root, s))
}

/// At startup: the project's kept turns come back (read on a worker), before any made since. A turn
/// tarae ended in the middle of becomes a checkpoint of its own, its files as they are now.
pub fn restore(ed: &mut Editor) {
    // (nothing is kept until they're back — a save before would write over them)
    let Some(dir) = ed.session.checkpoints.dir.take() else { return };
    let root = ed.session.root.clone();
    ed.events.jobs().spawn(move || {
        let back = dir.clone();
        let mut s = load(&dir, &root);
        let mut cut = std::mem::take(&mut s.turn);
        for t in &mut cut {
            // (gone: nothing in it now · unreadable: as it was kept)
            match std::fs::read(&t.path) {
                Ok(b) => {
                    if let Ok(now) = String::from_utf8(b) {
                        t.after = Rope::from_str(&now);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => t.after = Rope::new(),
                Err(_) => {}
            }
        }
        cut.retain(|t| t.before.as_ref().is_none_or(|b| *b != t.after));
        if !cut.is_empty() {
            let question = if s.question.is_empty() { CUT_SHORT.to_string() } else { s.question.clone() };
            s.list.push(Checkpoint::new(question, cut));
        }
        move |ed: &mut Editor| {
            ed.session.checkpoints.dir = Some(back);
            let n = s.list.len();
            if n == 0 {
                return keep(ed);
            }
            let since = std::mem::take(&mut ed.session.checkpoints.list);
            ed.session.checkpoints.list = s.list.into_iter().chain(since).collect();
            // (a turn's review open already points past the ones put before it)
            if let Some(crate::codereview::Base::Turn { at, .. }) =
                ed.session.code_review.as_mut().map(|r| &mut r.base)
            {
                *at += n;
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
    use super::rebase;

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
}
