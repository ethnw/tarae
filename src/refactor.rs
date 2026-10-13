//! Claude's refactoring — the editor's own, as tools for the chat, so one change across many places is one
//! call and one review instead of an Edit per place:
//!
//! - `rename` is `space r`'s: the file's language server is asked at a place (`textDocument/prepareRename`
//!   first when it has it, then `textDocument/rename`), and its WorkspaceEdit becomes each file's new text.
//!   A server starting or still loading isn't asked — said so; text matches aren't a rename (strangers with
//!   the same name, uses through imports missed). A rename that would make, move or delete files is refused.
//! - `replace` is `space /`'s replace: a regex (or plain text) over the project as the search reads it
//!   (`grep.rs` — `.gitignore`, hidden files, never `.git`), the replacement with `$1` groups; `preview`
//!   lists what it would change and changes nothing. More than `max_files` (`MAX_FILES`) files is refused
//!   unless Claude raises it.
//!
//! Both land the way Claude's Edits do (`land`): every file inside the project, or nothing (said why). Under
//! review at the end the files are written at once — on a worker, each only while it's still the text the
//! change was worked out against — kept for `space u` as unseen, and the turn is reviewed when it ends;
//! otherwise (and for a file with your unsaved edits, or one `llm.review-ask` names) each file comes up in
//! the in-buffer review, one after another (`next`, every event). The answer says per file what happened.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};

use ropey::Rope;
use serde_json::{Value, json};

use crate::document::DocId;
use crate::editor::Editor;
use crate::lsp::{ClientId, Encoding};
use crate::nav::{self, Spot};
use crate::tools::{Call, answer};

/// One file's change: its real path (and from the project's root), its text the change was worked out
/// against, its text after, and how many places it changes (a rename's edits, a replace's matches).
#[derive(Clone, Debug)]
pub struct FileChange {
    pub path: PathBuf,
    pub rel: String,
    pub before: Rope,
    pub after: Rope,
    pub places: usize,
}

/// What became of one file.
#[derive(Clone, Debug, PartialEq)]
enum Outcome {
    /// Written without a review now (review at the end).
    Written,
    Accepted,
    /// Some of its changes accepted, some not.
    Partly,
    /// Accepted into a buffer with the user's unsaved edits — on disk when they save.
    InBuffer,
    Declined,
    /// Not the text it was worked out against any more — left alone.
    Changed,
    Failed(String),
}

/// Where a landing with several files to review is (#75: a project-wide rename came as a card per change).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Stage {
    /// Not put to the user yet.
    Fresh,
    /// On its card — accept them all, one by one, or none.
    Asking,
    /// Accepted whole, being written.
    Writing,
    /// In the in-buffer review, a file at a time.
    OneByOne,
}

/// A rename or replace landing: its files in review one at a time, what became of each.
pub struct Landing {
    stage: Stage,
    call: Call,
    generation: u64,
    /// For the review's title and the answer (``rename `a` → `b` ``).
    what: String,
    /// What a place is called in the answer (`place`, `match`).
    noun: &'static str,
    /// Claude's words before the call (the review shows them).
    reason: String,
    pub(crate) review: VecDeque<FileChange>,
    /// How many went to review in all (`2 of 5` in the title).
    reviews: usize,
    done: Vec<(String, usize, Outcome)>,
    /// Said after the files (unsaved buffers that weren't searched).
    note: String,
    /// When it came — its place in the inbox (`inbox.rs`).
    since: std::time::Instant,
}

/// Lands `files` as Claude's change (see the module's doc) — `call` is answered once every file is done.
/// The files are inside the project already (checked where they were worked out, on a worker).
pub fn land(
    ed: &mut Editor,
    call: Call,
    what: String,
    noun: &'static str,
    files: Vec<FileChange>,
    note: String,
) {
    let Some(c) = ed.session.chat.as_ref() else { return answer(ed, call, Err("the chat is gone".into())) };
    let (generation, reason) = (c.generation(), crate::edits::reason(c));
    let files: Vec<FileChange> = files.into_iter().filter(|f| f.before != f.after).collect();
    if files.is_empty() {
        return answer(
            ed,
            call,
            Ok(format!("nothing to change — {what} leaves every file as it is{}", tail(&note))),
        );
    }
    let at_end = crate::edits::at_end(ed);
    let ask = ed.config.llm.review_ask.clone();
    // Your unsaved edits: disk isn't what you see — the usual review, and it stays in the buffer
    let dirty = |p: &Path| {
        ed.docs.iter().any(|d| d.path.as_deref() == Some(p) && d.is_modified() && !d.unsaved_is_claudes())
    };
    let (unseen, review): (Vec<_>, Vec<_>) = files
        .into_iter()
        .partition(|f| at_end && !dirty(&f.path) && !crate::edits::listed(Path::new(&f.rel), &ask));
    let l = Landing {
        stage: Stage::Fresh,
        call,
        generation,
        what,
        noun,
        reason,
        reviews: review.len(),
        review: review.into(),
        done: Vec::new(),
        note,
        since: std::time::Instant::now(),
    };
    if unseen.is_empty() {
        return queue(ed, l);
    }
    // What each was is kept before anything is written (a crash between still has it)
    let befores = unseen.iter().map(|f| (f.path.clone(), f.before.clone())).collect();
    crate::checkpoint::record_unseen_all(ed, befores, move |ed: &mut Editor| write(ed, l, unseen));
}

/// Review at the end: `unseen` written on a worker; open buffers without unsaved edits take the new text at
/// once (as the watch would a moment later); then the rest of the landing goes on.
fn write(ed: &mut Editor, mut l: Landing, unseen: Vec<FileChange>) {
    ed.events.jobs().spawn(move || {
        let written: Vec<(FileChange, Result<Option<crate::disk::Stamp>, Outcome>)> =
            unseen.into_iter().map(|f| (write_file(&f), f)).map(|(r, f)| (f, r)).collect();
        move |ed: &mut Editor| {
            for (f, r) in written {
                let outcome = match r {
                    Ok(stamp) => {
                        ed.disk_take(&f.path, &f.after, stamp);
                        ed.wrote(&f.path);
                        Outcome::Written
                    }
                    Err(o) => o,
                };
                l.done.push((f.rel, f.places, outcome));
            }
            queue(ed, l);
        }
    });
}

/// `f.after` over the file — only while it's still `f.before` (else it changed since the change was worked
/// out: left alone). On a worker.
fn write_file(f: &FileChange) -> Result<Option<crate::disk::Stamp>, Outcome> {
    let now = std::fs::read(&f.path).map_err(|e| Outcome::Failed(e.to_string()))?;
    if !String::from_utf8(now).is_ok_and(|s| f.before == s.as_str()) {
        return Err(Outcome::Changed);
    }
    crate::disk::write_atomic(&f.path, |w| f.after.write_to(w))
        .map_err(|e| Outcome::Failed(format!("{e:#}")))?;
    Ok(crate::disk::stamp(&f.path))
}

/// Into the chat's queue of landings (answered at once when the chat it came from is gone).
fn queue(ed: &mut Editor, l: Landing) {
    match ed.session.chat.as_mut().filter(|c| c.generation() == l.generation) {
        Some(c) => c.edits.sets.push_back(l),
        None => return answer(ed, l.call.clone(), Ok(report(&l))),
    }
    next(ed);
}

/// Every event: once no review is up, the front landing's next file in review — or, its files all done,
/// its answer.
pub fn next(ed: &mut Editor) {
    if ed.session.review.is_some() {
        return;
    }
    let background = ed.background();
    let Some(c) = ed.session.chat.as_mut() else { return };
    let Some(l) = c.edits.sets.front_mut() else { return };
    // (a session behind: its files wait for their review until it's in front)
    if background && !l.review.is_empty() {
        return;
    }
    // Several files: one card for them all first (#75)
    match l.stage {
        Stage::Asking | Stage::Writing => return,
        Stage::Fresh if l.review.len() > 1 => {
            l.stage = Stage::Asking;
            let o = card(l);
            return ed.push_offer(o);
        }
        Stage::Fresh | Stage::OneByOne => {}
    }
    match l.review.pop_front() {
        Some(f) => {
            let n = l.reviews - l.review.len();
            let title = match l.reviews {
                1 => l.what.clone(),
                of => format!("{} · {n} of {of}", l.what),
            };
            let reason = l.reason.clone();
            show(ed, f, title, reason);
        }
        None => {
            let l = c.edits.sets.pop_front().expect("the front one");
            answer(ed, l.call.clone(), Ok(report(&l)));
        }
    }
}

impl Landing {
    /// How many of its files wait for their review, not on its card.
    pub(crate) fn waiting(&self) -> usize {
        if self.stage == Stage::Asking { 0 } else { self.review.len() }
    }

    /// What it changes (``rename `a` → `b` ``) · when it came.
    pub(crate) fn what(&self) -> (&str, std::time::Instant) {
        (&self.what, self.since)
    }
}

/// The card's file list shows this many files at most (the rest: `… N more`; `v` shows them all).
const CARD_FILES: usize = 8;

/// The card for a landing's files in review: how many places in how many files, which — `y` accepts them
/// all, `r` reviews them one by one, `v` shows every change, `n` declines them all.
fn card(l: &Landing) -> crate::offer::Offer {
    let places: usize = l.review.iter().map(|f| f.places).sum();
    let mut files: Vec<&FileChange> = l.review.iter().collect();
    crate::sorting::by(&mut files, |a, b| a.rel.cmp(&b.rel));
    // The change itself first, at full strength (in the title it was cut); then where, counts in a column
    let mut q =
        format!("{}\n\n{} in {}:", l.what, nav::count(places, l.noun), nav::count(files.len(), "file"));
    let shown = &files[..files.len().min(CARD_FILES)];
    let name_w = shown.iter().map(|f| f.rel.chars().count()).max().unwrap_or(0);
    let num_w = shown.iter().map(|f| f.places.to_string().len()).max().unwrap_or(0);
    for f in shown {
        let pad = " ".repeat(name_w - f.rel.chars().count());
        q.push_str(&format!("\n  {}{pad}  {:>num_w$}", f.rel, f.places));
    }
    if files.len() > CARD_FILES {
        q.push_str(&format!("\n  … {} more — v shows every change", files.len() - CARD_FILES));
    }
    crate::offer::Offer::new(
        format!("Claude changes {}", nav::count(files.len(), "file")),
        &q,
        "Writing…",
        "Claude's change".into(),
        crate::offer::What::ChangeSet { generation: l.generation },
    )
}

/// The front landing, while its card is up (`generation`: the chat it came from).
fn asking(ed: &mut Editor, generation: u64) -> Option<&mut Landing> {
    let c = ed.session.chat.as_mut()?;
    c.edits.sets.front_mut().filter(|l| l.generation == generation && l.stage == Stage::Asking)
}

/// `y` on the card: every file written at once (on a worker, each only while still the text the change
/// was worked out against — kept for `space u` first); a file with your unsaved edits still comes up in
/// the in-buffer review.
pub fn accept_all(ed: &mut Editor, id: u64, generation: u64) {
    ed.offer_finished(id, Ok(()));
    let dirty: Vec<PathBuf> = ed
        .docs
        .iter()
        .filter(|d| d.is_modified() && !d.unsaved_is_claudes())
        .filter_map(|d| d.path.clone())
        .collect();
    let Some(l) = asking(ed, generation) else { return };
    let (later, now): (Vec<FileChange>, Vec<FileChange>) =
        l.review.drain(..).partition(|f| dirty.contains(&f.path));
    l.reviews = later.len();
    l.review = later.into();
    l.stage = Stage::Writing;
    let files = now.iter().map(|f| (f.path.clone(), f.before.clone(), f.after.clone())).collect();
    crate::checkpoint::record_all(ed, files, move |ed: &mut Editor| {
        ed.events.jobs().spawn(move || {
            let written: Vec<_> = now.into_iter().map(|f| (write_file(&f), f)).collect();
            move |ed: &mut Editor| {
                let mut done = Vec::new();
                for (r, f) in written {
                    let outcome = match r {
                        Ok(stamp) => {
                            ed.disk_take(&f.path, &f.after, stamp);
                            ed.wrote(&f.path);
                            Outcome::Accepted
                        }
                        Err(o) => o,
                    };
                    done.push((f.rel, f.places, outcome));
                }
                let Some(c) = ed.session.chat.as_mut() else { return };
                if let Some(l) = c.edits.sets.front_mut().filter(|l| l.generation == generation) {
                    l.done.extend(done);
                    l.stage = Stage::OneByOne;
                }
                next(ed);
            }
        });
    });
}

/// `r` on the card: each file in the in-buffer review, one after another.
pub fn one_by_one(ed: &mut Editor, id: u64, generation: u64) {
    ed.offer_finished(id, Ok(()));
    if let Some(l) = asking(ed, generation) {
        l.stage = Stage::OneByOne;
    }
    next(ed);
}

/// `n` on the card (it's gone already): every file left as it was.
pub fn decline_all(ed: &mut Editor, generation: u64) {
    if let Some(l) = asking(ed, generation) {
        let declined: Vec<_> = l.review.drain(..).map(|f| (f.rel, f.places, Outcome::Declined)).collect();
        l.done.extend(declined);
        l.stage = Stage::OneByOne;
    }
    next(ed);
}

/// `v` on the card: every change, file by file, in a buffer of its own (worked out on a worker) — it
/// closes with the card.
pub fn view_all(ed: &mut Editor, generation: u64) {
    let Some(l) = asking(ed, generation) else {
        return ed.offer_refused("its changes are gone (its chat ended) — n closes the card");
    };
    let title = format!("{}Claude changes {}", crate::offer::CARD_VIEW, nav::count(l.review.len(), "file"));
    let mut files: Vec<FileChange> = l.review.iter().cloned().collect();
    crate::sorting::by(&mut files, |a, b| a.rel.cmp(&b.rel));
    ed.events.jobs().spawn(move || {
        let text = changes_text(&files);
        move |ed: &mut Editor| {
            if asking(ed, generation).is_none() {
                return;
            }
            let old: Vec<DocId> =
                ed.docs.iter().filter(|d| d.title.as_deref() == Some(&title)).map(|d| d.id).collect();
            for d in old {
                ed.close_doc(d);
            }
            ed.open_virtual(&title, &text);
            if let Some(c) = ed.session.chat.as_mut() {
                c.focused = false;
            }
        }
    });
}

/// Every change of `files`, as `v` shows them: each file's name and count, then each change's line and its
/// old (`-`) and new (`+`) lines.
fn changes_text(files: &[FileChange]) -> String {
    let mut out = String::new();
    for f in files {
        out.push_str(&format!("── {} ({})\n", f.rel, f.places));
        let before = f.before.to_string();
        for (from, _, old, new) in crate::editdiff::line_changes(&before, &f.after.to_string()) {
            let line = before[..from].matches('\n').count() + 1;
            out.push_str(&format!("\n  line {line}\n"));
            for l in old.lines() {
                out.push_str(&format!("  - {l}\n"));
            }
            for l in new.lines() {
                out.push_str(&format!("  + {l}\n"));
            }
        }
        out.push('\n');
    }
    out
}

/// What became of a file of the front landing.
fn done_one(ed: &mut Editor, f: &FileChange, outcome: Outcome) {
    if let Some(l) = ed.session.chat.as_mut().and_then(|c| c.edits.sets.front_mut()) {
        l.done.push((f.rel.clone(), f.places, outcome));
    }
}

/// One file in the in-buffer review (y/n per change, as an Edit's) — if it's still what the change was
/// worked out against.
fn show(ed: &mut Editor, f: FileChange, title: String, reason: String) {
    if let Err(e) = ed.open(&f.path) {
        return done_one(ed, &f, Outcome::Failed(format!("{e:#}")));
    }
    let doc = ed.doc();
    if doc.loading {
        return done_one(ed, &f, Outcome::Failed("still loading in the editor".into()));
    }
    if doc.text != f.before {
        return done_one(ed, &f, Outcome::Changed);
    }
    let dirty = doc.is_modified() && !doc.unsaved_is_claudes();
    let doc_id = doc.id;
    let changes = crate::editdiff::line_changes(&f.before.to_string(), &f.after.to_string());
    // The keys go to the review; the file is the user's now (follow won't close it as a preview)
    let from_chat = ed.session.chat.as_ref().is_some_and(|c| c.focused);
    if let Some(c) = &mut ed.session.chat {
        c.focused = false;
        c.follow.keep(doc_id);
    }
    crate::llm::review_with(
        ed,
        doc_id,
        &title,
        changes,
        Some(Box::new(move |ed: &mut Editor, applied: bool| reviewed(ed, f, doc_id, dirty, applied))),
    );
    if let Some(r) = &mut ed.session.review {
        r.from_chat = from_chat;
        r.reason = reason;
        // (its turn in the chat is the step)
        r.own_turn = false;
        let g = r.generation;
        if let Some(c) = &mut ed.session.chat {
            c.edits.review = Some(g);
        }
    }
    crate::follow::settle(ed);
}

/// A file's review ended: kept for `space u` and saved (unless the user has unsaved edits there).
fn reviewed(ed: &mut Editor, f: FileChange, doc_id: DocId, dirty: bool, applied: bool) {
    if let Some(c) = &mut ed.session.chat {
        c.edits.review = None;
    }
    let now = ed.docs.iter().find(|d| d.id == doc_id).map(|d| d.text.clone());
    let outcome = match now {
        _ if !applied => Outcome::Declined,
        None => Outcome::Failed("its buffer was closed".into()),
        Some(now) if now == f.before => Outcome::Declined,
        Some(now) => {
            crate::checkpoint::record(ed, doc_id, Some(f.before.clone()));
            if dirty {
                Outcome::InBuffer
            } else {
                match ed.save_in_background(doc_id, false, crate::saver::Then::Auto(" · Claude's change")) {
                    Err(e) => Outcome::Failed(format!("not saved: {e}")),
                    Ok(()) if now == f.after => Outcome::Accepted,
                    Ok(()) => Outcome::Partly,
                }
            }
        }
    };
    done_one(ed, &f, outcome);
    crate::follow::settle(ed);
}

/// The chat turn ended: landings still going are dropped (nobody waits for their answers; a review still
/// up was closed by `edits::turn_over`).
pub fn turn_over(ed: &mut Editor) {
    if let Some(c) = ed.session.chat.as_mut() {
        c.edits.sets.clear();
    }
    // (and a card for one: nobody waits for its answer)
    ed.drop_cards(|w| matches!(w, crate::offer::What::ChangeSet { .. }));
}

/// The answer: how many places in how many files changed, then per file what became of it.
fn report(l: &Landing) -> String {
    let changed =
        |o: &Outcome| matches!(o, Outcome::Written | Outcome::Accepted | Outcome::Partly | Outcome::InBuffer);
    let files = l.done.iter().filter(|d| changed(&d.2)).count();
    let places: usize = l.done.iter().filter(|d| changed(&d.2)).map(|d| d.1).sum();
    let mut out = match files {
        0 => format!("nothing changed — {}", l.what),
        _ => format!("{} in {} — {}", nav::count(places, l.noun), nav::count(files, "file"), l.what),
    };
    // (by path, as the review's rail lists them — not in the server's order)
    let mut done: Vec<&(String, usize, Outcome)> = l.done.iter().collect();
    crate::sorting::by(&mut done, |a, b| a.0.cmp(&b.0));
    let group = |o: fn(&Outcome) -> bool| -> Vec<String> {
        done.iter().filter(|d| o(&d.2)).map(|(rel, n, _)| format!("{rel} ({n})")).collect()
    };
    let mut line = |head: &str, files: Vec<String>, tail: &str| {
        if !files.is_empty() {
            out.push_str(&format!("\n{head}: {}{tail}", files.join(", ")));
        }
    };
    line(
        "written",
        group(|o| *o == Outcome::Written),
        " — the user reviews your turn when it ends (space u takes it back)",
    );
    line("accepted by the user", group(|o| *o == Outcome::Accepted), "");
    line("accepted in part", group(|o| *o == Outcome::Partly), " — Read them again before editing them");
    line(
        "accepted into the user's open buffer, which has unsaved edits",
        group(|o| *o == Outcome::InBuffer),
        " — on disk when they save; treat it as done",
    );
    line("declined by the user, left as they were", group(|o| *o == Outcome::Declined), "");
    line(
        "changed since this was worked out, left alone",
        group(|o| *o == Outcome::Changed),
        " — call again for them",
    );
    for (rel, _, o) in done {
        if let Outcome::Failed(why) = o {
            out.push_str(&format!("\nnot changed: {rel} — {why}"));
        }
    }
    if files > 0 && files < l.done.len() {
        out.push_str(
            "\nSome files weren't changed, so the code may not be consistent — ask the user before going on.",
        );
    }
    out + &tail(&l.note)
}

/// A note after an answer, on its own line.
fn tail(note: &str) -> String {
    if note.is_empty() { String::new() } else { format!("\n{note}") }
}

// ── rename ──────────────────────────────────────────────────────────────────

pub fn rename_schema() -> Value {
    json!({
        "name": "rename",
        "description": "Rename a function, type, method, field, variable or module everywhere it's used — the \
    language server's rename (the editor's space r): every use, through imports and aliases, no same-named \
    strangers. One call instead of an Edit per place; the change lands as your edits do (the user reviews it). \
    Give the file and line where the name is written (its declaration or any use) and the name, or a column \
    on that line, and the new name. The answer lists the files and places changed, or why the server refused. \
    While the language server is starting or loading it can't rename — try again later; for plain text \
    across files, replace.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "A file where the name is written, relative to the project root" },
                "line": { "type": "integer", "description": "The line it's on, 1-based — left out: its first appearance in the file" },
                "symbol": { "type": "string", "description": "The name as written in the code" },
                "column": { "type": "integer", "description": "Its column on that line, 1-based (characters) — instead of the name" },
                "new_name": { "type": "string", "description": "The new name" }
            },
            "required": ["path", "new_name"]
        }
    })
}

/// How long a server has to answer a rename (a big one takes a while).
const RENAME_WAIT: std::time::Duration =
    std::time::Duration::from_millis(if cfg!(test) { 2_000 } else { 60_000 });

/// A `rename` asked of a server.
pub struct RenameAsk {
    call: Call,
    word: String,
    new_name: String,
    /// `path:line`, as said in the answer.
    at: String,
    doc: DocId,
    byte: usize,
    server: String,
    cid: ClientId,
    /// Open documents' versions when it was asked — an edit for one changed since isn't applied.
    versions: Vec<(DocId, u64)>,
}

/// The tool: the file checked on a worker (in the project), then the server asked (`rename_at`). Err =
/// answered now.
pub fn rename(ed: &mut Editor, call: Call, args: &Value) -> Result<(), String> {
    let new_name =
        args["new_name"].as_str().map(str::trim).filter(|n| !n.is_empty()).ok_or("new_name is required")?;
    if new_name.chars().any(char::is_whitespace) {
        return Err(format!("new_name is one name — not `{new_name}`"));
    }
    let new_name = new_name.to_string();
    let root = nav::project_root(ed);
    let spot = nav::spot_of(&root, args)?;
    let keys = crate::reads::Keys::of(ed);
    ed.events.jobs().spawn(move || {
        let file = nav::file_in(&root, &spot.path, &keys);
        move |ed: &mut Editor| match file {
            Ok(path) => rename_at(ed, call, Spot { path, ..spot }, new_name),
            Err(e) => answer(ed, call, Err(e)),
        }
    });
    Ok(())
}

fn rename_at(ed: &mut Editor, call: Call, spot: Spot, new_name: String) {
    let shown = nav::rel(&nav::project_root(ed), &spot.path);
    let id = match nav::buffer_for(ed, &spot.path) {
        Ok(id) => id,
        Err(e) => return answer(ed, call, Err(format!("{shown}: {e}"))),
    };
    let Some(doc) = ed.docs.iter().find(|d| d.id == id) else { return };
    if doc.loading {
        return answer(
            ed,
            call,
            Err(format!("{shown} is still loading in the editor — try again in a moment")),
        );
    }
    let (line, col, word) = match nav::place_in(&doc.text, &spot, &shown) {
        Ok(at) => at,
        Err(e) => return answer(ed, call, Err(e)),
    };
    if word == new_name {
        return answer(ed, call, Err(format!("`{word}` is already its name")));
    }
    let byte = doc.text.line_to_byte(line) + col;
    let versions = ed.docs.iter().map(|d| (d.id, d.version())).collect();
    let n = nav::next_ask(ed);
    let a = RenameAsk {
        call,
        word,
        new_name,
        at: format!("{shown}:{}", line + 1),
        doc: id,
        byte,
        server: String::new(),
        cid: Default::default(),
        versions,
    };
    send(ed, n, a, false);
}

/// Why it can't be renamed now — said, never guessed with text.
fn not_now(why: &str, later: bool) -> String {
    match later {
        true => format!(
            "{why} — not renamed; try again once it's ready (tarae renames only through the language server, \
             never by text matches)"
        ),
        false => format!("{why} — not renamed (for plain text across files, replace)"),
    }
}

/// Asks the prepare (`prepared` false, when the server has it) or the rename — the answer goes to
/// `prepared`/`renamed`; a server silent past `RENAME_WAIT` (from the first question) is said so.
fn send(ed: &mut Editor, n: u64, mut a: RenameAsk, prepared: bool) {
    match ed.ask_rename(a.doc, a.byte, n, &a.new_name, prepared) {
        Err((why, later)) => answer(ed, a.call, Err(not_now(&why, later))),
        Ok((cid, server, _)) => {
            (a.cid, a.server) = (cid, server);
            ed.lsp.tool_renames.insert(n, a);
            if prepared {
                return;
            }
            ed.events.jobs().spawn(move || {
                std::thread::sleep(RENAME_WAIT);
                move |ed: &mut Editor| {
                    if let Some(a) = ed.lsp.tool_renames.remove(&n) {
                        let why = format!("{} didn't answer within {}", a.server, nav::secs(RENAME_WAIT));
                        answer(ed, a.call, Err(not_now(&why, true)));
                    }
                }
            });
        }
    }
}

/// A server that ended a piece of work this recently may be between two (rust-analyzer: fetching the
/// workspace, then indexing it) — nothing or an error from it then is its loading, not a refusal.
const SETTLE: std::time::Duration = std::time::Duration::from_millis(if cfg!(test) { 100 } else { 2_000 });

/// Whether server `cid` is at work, or was moments ago (`SETTLE`), or is only just up.
fn at_work(ed: &Editor, cid: ClientId) -> bool {
    let recent = |t: &std::time::Instant| t.elapsed() < SETTLE;
    ed.client(cid).is_some_and(|c| !c.progress.is_empty() || c.ready_at.as_ref().is_some_and(recent))
        || ed.lsp.work_ended.get(&cid).is_some_and(recent)
}

/// The server's word on whether the name there can be renamed: no (null, an error) is the answer — unless
/// it's loading; yes, and the rename is asked.
pub fn prepared(ed: &mut Editor, n: u64, result: Result<&Value, &Value>) {
    let Some(a) = ed.lsp.tool_renames.remove(&n) else { return };
    let refused = |why: &str| format!("{} refused to rename `{}` at {}: {why}", a.server, a.word, a.at);
    let busy = at_work(ed, a.cid);
    let e = match result {
        Err(e) if loading(e) || busy => not_now(&format!("{} is still loading the project", a.server), true),
        Ok(v) if v.is_null() && busy => not_now(&format!("{} is still loading the project", a.server), true),
        Err(e) => refused(e["message"].as_str().unwrap_or("its prepareRename failed")),
        Ok(v) if v.is_null() || v["defaultBehavior"] == json!(false) => refused(
            "nothing it can rename there (prepareRename) — give the line where the name is declared or used",
        ),
        Ok(_) => return send(ed, n, a, true),
    };
    answer(ed, a.call, Err(e));
}

/// "Content modified" / "server cancelled": the project changed under the question — it's loading.
fn loading(e: &Value) -> bool {
    matches!(e["code"].as_i64(), Some(-32801 | -32802))
}

/// The server's rename: its edit made into each file's new text (on a worker), then landed.
pub fn renamed(ed: &mut Editor, n: u64, result: Result<&Value, &Value>) {
    let Some(a) = ed.lsp.tool_renames.remove(&n) else { return };
    let busy = at_work(ed, a.cid);
    let edit = match result {
        Err(e) if loading(e) || busy => {
            return answer(
                ed,
                a.call,
                Err(not_now(&format!("{} is still loading the project", a.server), true)),
            );
        }
        Ok(v) if v.is_null() && busy => {
            return answer(
                ed,
                a.call,
                Err(not_now(&format!("{} is still loading the project", a.server), true)),
            );
        }
        Err(e) => {
            let why = e["message"].as_str().unwrap_or("the rename failed");
            return answer(
                ed,
                a.call,
                Err(format!("{} refused to rename `{}` at {}: {why}", a.server, a.word, a.at)),
            );
        }
        Ok(v) if v.is_null() => {
            return answer(ed, a.call, Err(format!("{} found nothing to rename at {}", a.server, a.at)));
        }
        Ok(v) => v,
    };
    let per = match edits_by_file(ed, &a, edit) {
        Ok(p) => p,
        Err(e) => return answer(ed, a.call, Err(e)),
    };
    let enc = ed.encoding_of(a.cid);
    let open: HashMap<PathBuf, Rope> = ed
        .docs
        .iter()
        .filter_map(|d| Some((d.path.clone()?, d.text.clone())))
        .filter(|(p, _)| per.iter().any(|(q, _)| q == p))
        .collect();
    let (root, keys) = (nav::project_root(ed), crate::reads::Keys::of(ed));
    let what = format!("rename `{}` → `{}`", a.word, a.new_name);
    let call = a.call;
    ed.events.jobs().spawn(move || {
        let files = file_changes(&root, per, &open, enc, &keys);
        move |ed: &mut Editor| match files {
            Ok(files) => land(ed, call, what, "place", files, String::new()),
            Err(e) => answer(ed, call, Err(e)),
        }
    });
}

/// A WorkspaceEdit's text edits by file, in order — Err for one that makes, moves or deletes files, or one
/// for an open document changed since it was asked.
fn edits_by_file(ed: &Editor, a: &RenameAsk, edit: &Value) -> Result<Vec<(PathBuf, Vec<Value>)>, String> {
    let path_of = |u: &Value| -> Result<PathBuf, String> {
        let u = u.as_str().ok_or("the server's edit names no file")?;
        crate::lsp::path_from_uri(u).ok_or_else(|| format!("the server's edit names {u}, not a file"))
    };
    // (several entries for one file chain on: kept apart, applied in turn — `file_changes`)
    let mut per: Vec<(PathBuf, Vec<Value>)> = Vec::new();
    let mut add =
        |path: PathBuf, edits: &Value| per.push((path, edits.as_array().cloned().unwrap_or_default()));
    // Both present → documentChanges only (the spec)
    let doc_changes = edit["documentChanges"].as_array();
    if let Some(map) = edit["changes"].as_object().filter(|_| doc_changes.is_none()) {
        for (u, edits) in map {
            add(path_of(&json!(u))?, edits);
        }
    }
    let mut ops = Vec::new();
    for dc in doc_changes.into_iter().flatten() {
        if let Some(kind) = dc["kind"].as_str() {
            let uri = |k: &str| path_of(&dc[k]).map(|p| nav::rel(&ed.session.root, &p)).unwrap_or_default();
            ops.push(match kind {
                "rename" => format!("moves {} to {}", uri("oldUri"), uri("newUri")),
                "create" => format!("makes {}", uri("uri")),
                _ => format!("deletes {}", uri("uri")),
            });
            continue;
        }
        let path = path_of(&dc["textDocument"]["uri"])?;
        // A versioned edit against what the server has of the open document
        if let Some(v) = dc["textDocument"]["version"].as_i64()
            && let Some(doc) = ed.open_doc_at(&path)
            && let Some(l) = doc.lsp.link(a.cid)
            && (i64::from(l.version) != v || l.behind())
        {
            return Err(format!(
                "{} changed since {} worked out the rename — call rename again",
                nav::rel(&ed.session.root, &path),
                a.server
            ));
        }
        add(path, &dc["edits"]);
    }
    if !ops.is_empty() {
        return Err(format!(
            "{}'s rename of `{}` also {} — this tool only changes text in files; not renamed. Move the file \
             first (run git mv), then rename",
            a.server,
            a.word,
            ops.join(", ")
        ));
    }
    // An open document changed while the server worked: its offsets are stale
    for (id, v) in &a.versions {
        if let Some(d) = ed.docs.iter().find(|d| d.id == *id)
            && d.version() != *v
            && d.path.as_ref().is_some_and(|p| per.iter().any(|(q, _)| q == p))
        {
            let name = d.display_name(&ed.session.root);
            return Err(format!(
                "{name} changed while {} worked out the rename — call rename again",
                a.server
            ));
        }
    }
    Ok(per)
}

/// Each file's text before (the open buffer's, else disk) and after `per`'s edits — on a worker. Err: a file
/// outside the project (the whole change is refused, said which), or one that can't be read — or one named
/// like a key Claude hasn't been let read (`reads::Keys`; refused whole too: its change would show its text).
fn file_changes(
    root: &Path,
    per: Vec<(PathBuf, Vec<Value>)>,
    open: &HashMap<PathBuf, Rope>,
    enc: Encoding,
    keys: &crate::reads::Keys,
) -> Result<Vec<FileChange>, String> {
    let real_root = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
    let mut out: Vec<FileChange> = Vec::new();
    let mut outside = Vec::new();
    for (path, edits) in per {
        let Some(real) = crate::paths::inside(&real_root, &path) else {
            outside.push(path.display().to_string());
            continue;
        };
        if keys.shut(&real) {
            let left_out = crate::reads::key_left_out(&nav::rel(&real_root, &real));
            return Err(format!("this change reaches a file named like a key — refused whole: {left_out}"));
        }
        if let Some(f) = out.iter_mut().find(|f| f.path == real) {
            f.after = crate::editdiff::apply_edits(&f.after, &edits, enc);
            f.places += edits.len();
            continue;
        }
        let before = match open.get(&path).or_else(|| open.get(&real)) {
            Some(t) => t.clone(),
            None => crate::document::read_file(&real)
                .map_err(|e| format!("{}: {e:#}", nav::rel(&real_root, &real)))?,
        };
        let after = crate::editdiff::apply_edits(&before, &edits, enc);
        let rel = nav::rel(&real_root, &real);
        out.push(FileChange { path: real, rel, before, after, places: edits.len() });
    }
    if !outside.is_empty() {
        return Err(format!(
            "this change reaches {} outside the project ({}) — refused whole: tarae's tools change files inside \
             it only",
            nav::count(outside.len(), "file"),
            outside.join(", ")
        ));
    }
    Ok(out)
}

// ── replace ─────────────────────────────────────────────────────────────────

pub fn replace_schema() -> Value {
    json!({
        "name": "replace",
        "description": "Replace text across the project's files — the editor's project replace (space /): a \
    regex (Rust syntax, a line at a time) or plain text with literal, and the replacement ($1 or ${name} for \
    a regex's groups; ${1}x when letters follow). Searches as the search tool does: .gitignore honored, \
    hidden files in, never .git, inside the project only; narrow with path or glob. preview: true lists what \
    it would change (capped) and changes nothing — use it first. Applied, it's one change set the user \
    reviews, as your edits are. More than max_files files (200) is refused unless you raise max_files. Case \
    is as written unless you say otherwise. For renaming a symbol, rename is better (the language server's).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "A regex — plain text with literal" },
                "replacement": { "type": "string", "description": "What each match becomes ($1, ${name}: a regex's groups); empty deletes it" },
                "literal": { "type": "boolean", "description": "The pattern and the replacement are plain text" },
                "case": { "type": "string", "enum": ["sensitive", "insensitive", "smart"], "description": "sensitive (default) · insensitive · smart: any case when the pattern is all lowercase" },
                "path": { "type": "string", "description": "A folder or file in the project, relative to its root — the whole project if left out" },
                "glob": { "type": "string", "description": "Only files matching these globs, space-separated (`*.rs`, `src/**/*.{ts,tsx}`, `!*_test.go`)" },
                "preview": { "type": "boolean", "description": "List what it would change, change nothing" },
                "max_files": { "type": "integer", "description": "Files it may change at most (200 if left out)" }
            },
            "required": ["pattern", "replacement"]
        }
    })
}

/// Files a replace may change unless Claude raises `max_files`.
pub const MAX_FILES: usize = 200;

/// A preview's changed lines shown at most (each file's count is always said).
const PREVIEW_LINES: usize = 60;

/// What a replace asks.
#[derive(Clone, Debug)]
struct Replace {
    pattern: String,
    replacement: String,
    literal: bool,
    case: crate::grep::Case,
    globs: Vec<String>,
    path: Option<String>,
    preview: bool,
    max_files: usize,
}

/// The tool: the pattern checked now (a typo is answered at once), searched and worked out on a worker,
/// then previewed or landed.
pub fn replace(ed: &mut Editor, call: Call, args: &Value) -> Result<(), String> {
    use crate::grep::Case;
    let s = |k: &str| args[k].as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let pattern = args["pattern"].as_str().filter(|p| !p.is_empty()).ok_or("pattern is required")?;
    let replacement =
        args["replacement"].as_str().ok_or("replacement is required (an empty one deletes what matches)")?;
    let case = match s("case").as_deref() {
        None | Some("sensitive") => Case::Sensitive,
        Some("insensitive") => Case::Insensitive,
        Some("smart") => Case::Smart,
        Some(c) => return Err(format!("case is sensitive, insensitive or smart — not {c}")),
    };
    let q = Replace {
        pattern: pattern.to_string(),
        replacement: replacement.to_string(),
        literal: args["literal"].as_bool().unwrap_or(false),
        case,
        globs: s("glob").map(|g| g.split_whitespace().map(str::to_string).collect()).unwrap_or_default(),
        path: s("path"),
        preview: args["preview"].as_bool().unwrap_or(false),
        max_files: args["max_files"].as_u64().map_or(MAX_FILES, |m| m as usize),
    };
    let re = regex_of(&q)?;
    let root = nav::project_root(ed);
    let open: HashMap<PathBuf, Rope> =
        ed.docs.iter().filter_map(|d| Some((d.path.clone()?, d.text.clone()))).collect();
    let unsaved: Vec<PathBuf> =
        ed.docs.iter().filter(|d| d.is_modified()).filter_map(|d| d.path.clone()).collect();
    let keys = crate::reads::Keys::of(ed);
    ed.events.jobs().spawn(move || {
        let r = plan(&root, &q, &re, &open, &unsaved, &keys);
        move |ed: &mut Editor| match r {
            Ok(Planned::Preview(text)) => answer(ed, call, Ok(text)),
            Ok(Planned::Apply(files, note)) => {
                let what = format!("replace `{}` → `{}`", q.pattern, q.replacement);
                land(ed, call, what, "match", files, note)
            }
            Err(e) => answer(ed, call, Err(e)),
        }
    });
    Ok(())
}

/// The regex a replace matches with — the search's reading of the pattern and its case.
fn regex_of(q: &Replace) -> Result<regex::Regex, String> {
    use crate::grep::Case;
    let insensitive = match q.case {
        Case::Smart => crate::grep::smart_insensitive(&q.pattern, q.literal),
        Case::Sensitive => false,
        Case::Insensitive => true,
    };
    let pattern = if q.literal { regex::escape(&q.pattern) } else { q.pattern.clone() };
    regex::RegexBuilder::new(&pattern)
        .case_insensitive(insensitive)
        .build()
        .map_err(|e| format!("not a regex: {}", crate::grep::why(&e.to_string())))
}

enum Planned {
    Preview(String),
    Apply(Vec<FileChange>, String),
}

/// On a worker: the files with matches (the project search), each one's text after — an open buffer's text
/// is what's replaced in — and the preview, or the change set (refused past `max_files`). Not a file named
/// like a key Claude hasn't been let read (`reads::Keys`), nor counted — as the search (`nav::run_search`).
fn plan(
    root: &Path,
    q: &Replace,
    re: &regex::Regex,
    open: &HashMap<PathBuf, Rope>,
    unsaved: &[PathBuf],
    keys: &crate::reads::Keys,
) -> Result<Planned, String> {
    let root = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
    let scope = match &q.path {
        Some(p) => {
            crate::paths::existing_inside(&root, &root.join(p), p, "replace changes files inside it only")?
        }
        None => root.clone(),
    };
    let query = crate::grep::Query {
        pattern: &q.pattern,
        literal: q.literal,
        case: q.case,
        word: false,
        globs: &q.globs,
        context: 0,
        mode: crate::grep::Mode::Lines,
        types: &[],
        multiline: false,
        exclude: "",
        judged: false,
    };
    // Per file, its matching lines (from 0)
    let mut found: Vec<(String, Vec<usize>)> = Vec::new();
    let warn = crate::grep::search(&root, std::slice::from_ref(&scope), &query, &|| false, &mut |f| {
        if !keys.shut(&root.join(&f.rel)) {
            found.push((f.rel, f.rows.iter().filter(|r| r.hit).map(|r| r.line - 1).collect()));
        }
        true
    })?;
    crate::sorting::sort(&mut found);
    // An open buffer: all of its lines (its text isn't what the search read)
    let targets: Vec<(PathBuf, Vec<usize>)> = found
        .into_iter()
        .map(|(rel, lines)| {
            let path = root.join(&rel);
            let lines = match open.get(&path) {
                Some(t) => (0..t.len_lines()).collect(),
                None => lines,
            };
            (path, lines)
        })
        .collect();
    let planned = crate::replace::plan_with(re, &q.replacement, !q.literal, &targets, open);
    let files: Vec<FileChange> = planned
        .into_iter()
        .map(|(path, before, edits, count)| {
            let after = crate::editdiff::apply_edits(&before, &edits, Encoding::Utf8);
            FileChange { rel: nav::rel(&root, &path), path, before, after, places: count }
        })
        .collect();
    let mut note = Vec::new();
    let unsaved: Vec<String> = unsaved
        .iter()
        .filter(|p| p.starts_with(&scope) && !files.iter().any(|f| &f.path == *p))
        .map(|p| nav::rel(&root, p))
        .collect();
    if !unsaved.is_empty() {
        note.push(format!(
            "(unsaved edits in {} weren't searched — only what's on disk)",
            unsaved.join(", ")
        ));
    }
    if let Some(w) = warn {
        note.push(format!("(not searched — {w})"));
    }
    let note = note.join("\n");
    if files.is_empty() {
        let within = if q.globs.is_empty() {
            String::new()
        } else {
            format!(" in files matching {}", q.globs.join(" "))
        };
        return Ok(Planned::Preview(format!(
            "no matches for `{}`{within} — nothing to replace{}",
            q.pattern,
            tail(&note)
        )));
    }
    let over = files.len() > q.max_files;
    if q.preview {
        return Ok(Planned::Preview(preview(q, &files, over) + &tail(&note)));
    }
    if over {
        return Err(format!(
            "this replace would change {} — more than max_files ({}). Narrow it with path or glob, or give \
             max_files: {} if you mean all of them (preview: true lists them first)",
            nav::count(files.len(), "file"),
            q.max_files,
            files.len()
        ));
    }
    Ok(Planned::Apply(files, note))
}

/// The preview: how many matches in how many files, each file's count, and its changed lines (old, then
/// new) up to `PREVIEW_LINES`.
fn preview(q: &Replace, files: &[FileChange], over: bool) -> String {
    let matches: usize = files.iter().map(|f| f.places).sum();
    let mut out = format!(
        "{} in {} — preview, nothing changed: `{}` → `{}`",
        nav::count(matches, "match"),
        nav::count(files.len(), "file"),
        q.pattern,
        q.replacement
    );
    let mut shown = 0;
    let mut rest: Vec<String> = Vec::new();
    for f in files {
        if shown >= PREVIEW_LINES {
            rest.push(format!("{} ({})", f.rel, f.places));
            continue;
        }
        out.push_str(&format!("\n{} ({})", f.rel, f.places));
        for (from, _, old, new) in crate::editdiff::line_changes(&f.before.to_string(), &f.after.to_string())
        {
            let line = f.before.byte_to_line(from) + 1;
            let (old, new): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
            for (i, (o, n)) in old.iter().zip(&new).enumerate() {
                if shown >= PREVIEW_LINES {
                    break;
                }
                out.push_str(&format!(
                    "\n  {}: {}\n  {}→ {}",
                    line + i,
                    nav::clip(o.trim_end(), 200),
                    " ".repeat((line + i).to_string().len()),
                    nav::clip(n.trim_end(), 200)
                ));
                shown += 1;
            }
        }
    }
    if !rest.is_empty() {
        out.push_str(&format!("\n… {} more not shown: {}", nav::count(rest.len(), "file"), rest.join(", ")));
    }
    if over {
        out.push_str(&format!(
            "\nApplying it would change {} — more than max_files ({}): give max_files: {} to mean it",
            nav::count(files.len(), "file"),
            q.max_files,
            files.len()
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 7c: a file in the project named like a key isn't Claude's to change through these tools until it was let
    /// read it (`reads::Keys`) — its change would show its text. A replace leaves it out, uncounted (its
    /// preview of a match only there says what one matching nothing does), and changes it once opened; a
    /// rename that reaches it is refused whole, naming it.
    #[test]
    fn a_key_named_file_is_out_of_replace_and_rename_until_opened() {
        let dir = std::env::temp_dir().join(format!("tarae-refactor-keys-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let env = dir.join(".env");
        std::fs::write(&env, "alpha=tarae-fake-token\n").unwrap();
        std::fs::write(dir.join("a.rs"), "let alpha = 1;\n").unwrap();
        let q = |pattern: &str, preview: bool| Replace {
            pattern: pattern.into(),
            replacement: "beta".into(),
            literal: true,
            case: crate::grep::Case::Sensitive,
            globs: Vec::new(),
            path: None,
            preview,
            max_files: MAX_FILES,
        };
        let shut = crate::reads::Keys::none_opened(&dir);
        let open = shut.clone().opening(env.clone());
        let none = HashMap::new();
        let plan_of =
            |q: Replace, keys: &crate::reads::Keys| plan(&dir, &q, &regex_of(&q).unwrap(), &none, &[], keys);
        let changed = |p: Result<Planned, String>| match p {
            Ok(Planned::Apply(files, _)) => files.iter().map(|f| f.rel.clone()).collect::<Vec<_>>(),
            Ok(Planned::Preview(t)) | Err(t) => panic!("{t}"),
        };
        assert_eq!(changed(plan_of(q("alpha", false), &shut)), ["a.rs"]);
        assert_eq!(changed(plan_of(q("alpha", false), &open)), [".env", "a.rs"], "opened");
        let preview = |p: Result<Planned, String>| match p {
            Ok(Planned::Preview(t)) => t,
            _ => panic!("a preview"),
        };
        let only_there = preview(plan_of(q("fake-token", true), &shut));
        assert_eq!(only_there.replace("fake-token", "nowhere"), preview(plan_of(q("nowhere", true), &shut)));
        let edit = json!([{"range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}},
            "newText": "beta"}]);
        let per = vec![
            (dir.join("a.rs"), edit.as_array().unwrap().clone()),
            (env.clone(), edit.as_array().unwrap().clone()),
        ];
        let e = file_changes(&dir, per.clone(), &none, Encoding::Utf8, &shut).err().unwrap();
        assert!(e.contains("refused whole") && e.contains(".env is named like a key"), "{e}");
        assert_eq!(file_changes(&dir, per, &none, Encoding::Utf8, &open).map(|f| f.len()), Ok(2), "opened");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A literal replace puts its text in as written (`$1` too) and matches its pattern's characters only;
    /// a regex's `.` is any character and `$1` a group; case is as written unless asked; an open buffer's
    /// text is what's replaced in; a broken regex is said.
    #[test]
    fn a_replace_reads_its_pattern_and_replacement_as_asked() {
        use crate::grep::Case;
        let dir = std::env::temp_dir().join(format!("tarae-refactor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "a.b axb A.B\n").unwrap();
        std::fs::write(dir.join("open.txt"), "on disk\n").unwrap();
        let q = |pattern: &str, replacement: &str, literal: bool, case: Case| Replace {
            pattern: pattern.into(),
            replacement: replacement.into(),
            literal,
            case,
            globs: vec!["f.txt".into()],
            path: None,
            preview: false,
            max_files: MAX_FILES,
        };
        let after = |q: Replace, open: &HashMap<PathBuf, Rope>| match plan(
            &dir,
            &q,
            &regex_of(&q).unwrap(),
            open,
            &[],
            &crate::reads::Keys::none_opened(&dir),
        ) {
            Ok(Planned::Apply(files, _)) => {
                files.iter().map(|f| f.after.to_string()).collect::<Vec<_>>().join("|")
            }
            Ok(Planned::Preview(t)) => t,
            Err(e) => e,
        };
        let none = HashMap::new();
        assert_eq!(after(q("a.b", "$1 ok", true, Case::Sensitive), &none), "$1 ok axb A.B\n");
        assert_eq!(after(q("a.b", "$1 ok", true, Case::Insensitive), &none), "$1 ok axb $1 ok\n");
        assert_eq!(after(q("(a).b", "<$1>", false, Case::Sensitive), &none), "<a> <a> A.B\n");
        assert_eq!(after(q("a.b", "x", false, Case::Smart), &none), "x x x\n", "smart: all lowercase");
        let e = regex_of(&q("(", "x", false, Case::Sensitive)).unwrap_err();
        assert!(e.starts_with("not a regex: "), "{e}");
        // An open buffer: its text, not the disk's
        let mut open = HashMap::new();
        open.insert(dir.join("open.txt"), Rope::from_str("in the buffer a.b\n"));
        let mut b = q("a.b", "c", true, Case::Sensitive);
        b.globs = vec!["open.txt".into()];
        assert!(after(b.clone(), &none).starts_with("no matches"), "the disk has none");
        std::fs::write(dir.join("open.txt"), "a.b on disk\n").unwrap();
        assert_eq!(after(b, &open), "in the buffer c\n");
        std::fs::remove_dir_all(&dir).ok();
    }
}
