//! Notes — Claude's comments pinned to lines of code, each a thread you answer in place.
//!
//! - Claude writes them with the `note` tool of the chat process (an MCP server living in tarae — the chat
//!   answers `mcp_message` control requests, `chat.rs`). `reply_to` answers a thread instead.
//! - Each entry says who wrote it (`By`): the author (the chat's Claude), you, or the reviewer (a second
//!   Claude, `reviewer.rs` — its findings wait like your drafts, `finding`).
//! - A note sits on a document mark (it follows edits); a file that isn't open keeps the line numbers and
//!   gets its mark when it opens (`attach`, every event). The line end shows `¶ first words +N`, and with
//!   the cursor on the lines a card shows the whole thread (`term::note_lines`).
//! - `space n`: reply to the note here, or ask Claude about this line (a new note starting with you).
//!   The message goes to the chat with the lines and the thread; Claude answers with `reply_to` — if it
//!   answers in plain text, that text goes into the thread. `]n`/`[n` walk notes, `space N` lists them,
//!   `:note-close` drops the one here.
//! - Kept per session, under its checkout, in the restore file (`restore.rs`, written on quit — `to_json`, read
//!   as the session is made — `restore_in`). A note remembers its first line's text: if the file changed while
//!   tarae was closed, it re-anchors on the nearest line with that text when the file opens.

use std::ops::Range;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::document::DocId;
use crate::editor::Editor;
use crate::movement as mv;
use crate::selection::{Range as SelRange, Selection};

/// Who wrote an entry: the chat's Claude (the author of the changes) · you · the reviewer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    /// The chat's Claude — what it changed is what's reviewed.
    Author,
    You,
    /// The reviewer (`reviewer.rs`) — a second Claude, apart from the chat's.
    Reviewer,
}

impl By {
    /// How it's shown: on the note's card, in the notes list and the review rail.
    pub fn label(self) -> &'static str {
        match self {
            By::Author => "author",
            By::You => "you",
            By::Reviewer => "reviewer",
        }
    }

    /// How a thread line names its author for Claude, and in the restore file (the author's `claude`, as
    /// files written before there was a reviewer have it).
    pub fn word(self) -> &'static str {
        match self {
            By::Author => "claude",
            By::You => "you",
            By::Reviewer => "reviewer",
        }
    }

    fn from_word(w: &str) -> By {
        match w {
            "you" => By::You,
            "reviewer" => By::Reviewer,
            _ => By::Author,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Entry {
    pub by: By,
    pub text: String,
    /// Written during a review — goes to Claude with the rest of it (`codereview::send`).
    pub draft: bool,
}

#[derive(Clone, Debug)]
pub struct Note {
    pub id: u64,
    pub path: PathBuf,
    /// The mark in the open document (follows edits).
    pub at: Option<(DocId, u64)>,
    /// 0-based lines, as of the last event.
    pub lines: Range<usize>,
    pub thread: Vec<Entry>,
    /// You wrote last and Claude hasn't answered yet.
    pub waiting: bool,
    /// Restored from last time: the first line's text then (re-anchors once, when the file opens).
    pub anchor: Option<String>,
    /// Its lines' text as it was pinned — a SHA-256 of it (`lines_digest`): a draft whose lines changed since
    /// is offered to close at a turn's end (#197). None until known (a file not open: read on a worker).
    pub pinned: Option<[u8; 32]>,
}

impl Note {
    /// The first thing said — what the line end shows.
    pub fn gist(&self) -> &str {
        self.thread.first().map_or("", |e| e.text.lines().find(|l| !l.trim().is_empty()).unwrap_or(""))
    }
}

#[derive(Default)]
pub struct Notes {
    pub list: Vec<Note>,
    next: u64,
    /// The notes the chat turn in flight is about (one: its answer lands there if Claude doesn't use the tool).
    pub turn: Vec<u64>,
    /// The note shown when several share a line — the one just pinned, or the one `]n`/`[n` stepped to.
    pub focus: Option<u64>,
    /// Drafts the stale card asked about (`stale_card`) — not asked again.
    stale_asked: Vec<u64>,
}

/// The `note` tool as the MCP server lists it.
pub fn tool_schema() -> Value {
    json!({
        "name": "note",
        "description": "Pin a short note to lines of code in the user's editor — it shows beside that code and \
    the user can reply there. Use it for observations tied to specific lines (a bug, a risk, a question, an idea); \
    one or two sentences each, a few per answer at most. To answer the user on an existing note, pass reply_to \
    with its id (path and line aren't needed then).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path, absolute or relative to the project root" },
                "line": { "type": "integer", "description": "First line, 1-based" },
                "end_line": { "type": "integer", "description": "Last line, 1-based (optional)" },
                "text": { "type": "string", "description": "The note — Markdown, one or two sentences" },
                "reply_to": { "type": "integer", "description": "Id of the note you're answering" }
            },
            "required": ["text"]
        }
    })
}

fn find(ed: &Editor, id: u64) -> Option<&Note> {
    ed.session.notes.list.iter().find(|n| n.id == id)
}

fn find_mut(ed: &mut Editor, id: u64) -> Option<&mut Note> {
    ed.session.notes.list.iter_mut().find(|n| n.id == id)
}

/// A new note on `lines` of `path`.
pub fn add(ed: &mut Editor, path: &Path, lines: Range<usize>, by: By, text: &str) -> u64 {
    ed.session.notes.next += 1;
    let id = ed.session.notes.next;
    ed.session.notes.list.push(Note {
        id,
        path: path.to_path_buf(),
        at: None,
        lines: lines.clone(),
        thread: vec![Entry { by, text: text.trim().to_string(), draft: false }],
        waiting: by == By::You,
        anchor: None,
        pinned: None,
    });
    ed.session.notes.focus = Some(id);
    attach(ed);
    // (a file not open — a finding of the reviewer's: its lines as they are now, read on a worker)
    if find(ed, id).is_some_and(|n| n.pinned.is_none()) {
        let path = path.to_path_buf();
        ed.events.jobs().spawn(move || {
            let text = std::fs::read_to_string(&path).ok().map(|t| ropey::Rope::from_str(&t));
            let pinned = text.and_then(|t| lines_digest(&t, &lines));
            move |ed: &mut Editor| {
                if let Some(n) = find_mut(ed, id).filter(|n| n.pinned.is_none()) {
                    n.pinned = pinned;
                }
                ed.unchanged = true;
            }
        });
    }
    id
}

/// What a note keeps of its lines' text to tell a change (`Note.pinned`): a SHA-256 of them. None past the
/// end.
fn lines_digest(text: &ropey::Rope, lines: &Range<usize>) -> Option<[u8; 32]> {
    let n = text.len_lines();
    if lines.start >= n {
        return None;
    }
    let end = lines.end.clamp(lines.start + 1, n);
    let s = text.slice(text.line_to_char(lines.start)..text.line_to_char(end)).to_string();
    Some(crate::sha256::digest(s.as_bytes()))
}

fn hex(d: &[u8; 32]) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

fn of_hex(s: &str) -> Option<[u8; 32]> {
    let bytes: Option<Vec<u8>> = (0..s.len())
        .step_by(2)
        .map(|i| s.get(i..i + 2).and_then(|b| u8::from_str_radix(b, 16).ok()))
        .collect();
    bytes?.try_into().ok()
}

/// Every event: open documents get marks for their notes, and every note's lines follow its mark.
pub fn attach(ed: &mut Editor) {
    let Editor { session, docs, .. } = ed;
    attach_in(&mut session.notes, docs);
}

/// `attach` for one session's notes.
fn attach_in(notes: &mut Notes, docs: &mut [crate::document::Document]) {
    for n in &mut notes.list {
        match n
            .at
            .and_then(|(d, m)| docs.iter().find(|doc| doc.id == d).and_then(|doc| Some((doc, doc.mark(m)?))))
        {
            Some((doc, sel)) => {
                let r = sel.primary();
                let (a, b) = (mv::line_of(&doc.text, r.anchor), mv::line_of(&doc.text, r.head));
                n.lines = a.min(b)..a.max(b) + 1;
            }
            None => {
                n.at = None;
                if let Some(doc) =
                    docs.iter_mut().find(|d| d.path.as_deref() == Some(n.path.as_path()) && !d.loading)
                {
                    // Saved last time: follow its first line if the file moved it meanwhile
                    if let Some(want) = n.anchor.take() {
                        let shift = nearest_line(&doc.text, n.lines.start, &want);
                        n.lines = n.lines.start.saturating_add_signed(shift)
                            ..n.lines.end.saturating_add_signed(shift);
                    }
                    // (the file may have shrunk: both ends within it, the first not after the last)
                    let last = mv::last_line(&doc.text);
                    let a = n.lines.start.min(last);
                    let b = n.lines.end.saturating_sub(1).max(a).min(last);
                    let sel = Selection::single(SelRange::new(
                        mv::line_start(&doc.text, a),
                        mv::line_start(&doc.text, b),
                    ));
                    n.at = Some((doc.id, doc.add_mark(sel)));
                    n.lines = a..b + 1;
                    // (its lines as pinned, when not known yet — a note restored from an earlier build)
                    if n.pinned.is_none() {
                        n.pinned = lines_digest(&doc.text, &n.lines);
                    }
                }
            }
        }
    }
}

/// Offset from `line` to the nearest line (within 500) whose text is `want`, trimmed — 0 if none.
fn nearest_line(text: &ropey::Rope, line: usize, want: &str) -> isize {
    let is = |l: usize| l < text.len_lines() && text.line(l).to_string().trim() == want;
    if is(line) {
        return 0;
    }
    for d in 1..=500isize {
        for off in [-d, d] {
            if line.checked_add_signed(off).is_some_and(is) {
                return off;
            }
        }
    }
    0
}

/// A session's notes as saved in the restore file: path, lines, the first line's text, the thread.
pub fn to_json(notes: &Notes, docs: &[crate::document::Document]) -> Vec<Value> {
    notes
        .list
        .iter()
        .map(|n| {
            let anchor = n.anchor.clone().or_else(|| {
                let doc = docs.iter().find(|d| d.path.as_deref() == Some(n.path.as_path()))?;
                (n.lines.start < doc.text.len_lines())
                    .then(|| doc.text.line(n.lines.start).to_string().trim().to_string())
            });
            let thread: Vec<Value> = n.thread.iter().map(|e| json!([e.by.word(), e.text, e.draft])).collect();
            let pinned = n.pinned.as_ref().map(hex);
            json!({
                "path": n.path, "lines": [n.lines.start, n.lines.end], "anchor": anchor, "thread": thread,
                "pinned": pinned,
            })
        })
        .collect()
}

/// Session `id`'s notes from last time, kept for its checkout — session 0's at start, each other one's as
/// it's made (`session_add`: back at a start or a restart, or opened again).
pub fn restore_in(ed: &mut Editor, id: crate::event::SessionId) {
    let Editor { session, sessions, docs, restore, .. } = ed;
    let Some(s) = std::iter::once(session).chain(sessions.iter_mut()).find(|l| l.id == id) else { return };
    let saved = restore.notes(&s.root);
    restore_from(&mut s.notes, docs, saved);
    // (and the lines looked at in its reviews — `risk.rs` — with the added ones' text)
    s.looked.extend(restore.looked(&s.root));
    for (rel, lines) in restore.looked_lines(&s.root) {
        let kept = s.looked_lines.entry(rel).or_default();
        for l in lines {
            if !kept.iter().any(|k| k.added == l.added) {
                kept.push(l);
            }
        }
    }
    // (each once: restored again in the same run — a session ended and opened again — adds none twice)
    for (rel, orders) in restore.looked_orders(&s.root) {
        let kept = s.looked_orders.entry(rel).or_default();
        for o in orders {
            if !kept.contains(&o) {
                kept.push(o);
            }
        }
    }
}

fn restore_from(notes: &mut Notes, docs: &mut [crate::document::Document], saved: Vec<Value>) {
    for v in saved {
        let (Some(path), Some(lines)) = (v["path"].as_str(), v["lines"].as_array()) else { continue };
        let (Some(a), Some(b)) =
            (lines.first().and_then(Value::as_u64), lines.get(1).and_then(Value::as_u64))
        else {
            continue;
        };
        let thread: Vec<Entry> = v["thread"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|e| {
                let by = By::from_word(e[0].as_str().unwrap_or_default());
                Some(Entry { by, text: e[1].as_str()?.to_string(), draft: e[2] == true })
            })
            .collect();
        if thread.is_empty() || !Path::new(path).is_file() {
            continue;
        }
        notes.next += 1;
        notes.list.push(Note {
            id: notes.next,
            path: PathBuf::from(path),
            at: None,
            lines: a as usize..(b as usize).max(a as usize + 1),
            thread,
            waiting: false,
            anchor: v["anchor"].as_str().map(str::to_string),
            pinned: v["pinned"].as_str().and_then(of_hex),
        });
    }
    attach_in(notes, docs);
}

/// Marks notes point at (the jump list's pruning keeps them).
pub fn marks(ed: &Editor) -> Vec<(DocId, u64)> {
    ed.session.notes.list.iter().filter_map(|n| n.at).collect()
}

/// Notes over the cursor line in the current document, in (first line, age) order.
pub fn all_here(ed: &Editor) -> Vec<&Note> {
    let Some(path) = ed.doc().path.as_deref() else { return Vec::new() };
    let line = ed.cursor_line();
    let mut v: Vec<&Note> =
        ed.session.notes.list.iter().filter(|n| n.path == path && n.lines.contains(&line)).collect();
    crate::sorting::by_key(&mut v, |n| (n.lines.start, n.id));
    v
}

/// The note under the cursor: the focused one (just pinned, or stepped to) if it's here, else the first —
/// so `]n` reads a crowded line in order.
pub fn here(ed: &Editor) -> Option<&Note> {
    let v = all_here(ed);
    v.iter().find(|n| Some(n.id) == ed.session.notes.focus).or(v.first()).copied()
}

// ── The tool ────────────────────────────────────────────────────────────────

/// `note` called by Claude → the text it gets back (Err = a tool error it can correct).
pub fn tool_call(ed: &mut Editor, args: &Value) -> Result<String, String> {
    let text = args["text"].as_str().map(str::trim).filter(|t| !t.is_empty()).ok_or("text is required")?;
    if let Some(id) = args["reply_to"].as_u64() {
        let n = find_mut(ed, id).ok_or(format!("there's no note #{id}"))?;
        n.thread.push(Entry { by: By::Author, text: text.to_string(), draft: false });
        n.waiting = false;
        return Ok(format!("replied on note #{id}"));
    }
    let (path, lines) = placed(&ed.session.root, args)?;
    let line = lines.start;
    let id = add(ed, &path, lines, By::Author, text);
    Ok(format!("note #{id} pinned at {}:{}", crate::chat::shown(&ed.session.root, &path), line + 1))
}

/// Where a new note goes: `path` (relative to `root`) and its lines, 0-based.
fn placed(root: &Path, args: &Value) -> Result<(PathBuf, Range<usize>), String> {
    let path =
        args["path"].as_str().filter(|p| !p.is_empty()).ok_or("path and line are required for a new note")?;
    let path = crate::chat::resolve(root, path);
    let path = std::fs::canonicalize(&path).unwrap_or(path);
    if !path.is_file() {
        return Err(format!("{} isn't a file", path.display()));
    }
    let line = args["line"].as_u64().ok_or("path and line are required for a new note")?.max(1) as usize - 1;
    let end = args["end_line"].as_u64().map_or(line, |e| (e.max(1) as usize - 1).max(line));
    Ok((path, line..end + 1))
}

/// The most a finding of the reviewer's says (it's read by you, then maybe sent to the author).
const FINDING_MAX: usize = 1_000;

/// The reviewer's `note` (`reviewer.rs`) → the new note's id and what it's told. A finding only: a file
/// inside `root` (the checkout it reviews), no `reply_to` (it sees no thread). It waits like your drafts —
/// for `space v s` to take it to the author with them, or `:note-close` to drop it.
pub fn finding(ed: &mut Editor, root: &Path, args: &Value) -> Result<(u64, String), String> {
    if args.get("reply_to").is_some_and(|r| !r.is_null()) {
        return Err("the reviewer pins new findings only — reply_to isn't one of its arguments".into());
    }
    let text = args["text"].as_str().map(str::trim).filter(|t| !t.is_empty()).ok_or("text is required")?;
    let text: String = match text.char_indices().nth(FINDING_MAX) {
        Some((cut, _)) => format!("{}…", &text[..cut]),
        None => text.to_string(),
    };
    let (path, lines) = placed(root, args)?;
    let shown = crate::chat::shown(root, &path);
    crate::paths::existing_inside(root, &path, &shown, "the reviewer pins findings in the checkout only")?;
    let line = lines.start;
    let id = add(ed, &path, lines, By::Reviewer, &text);
    if let Some(n) = find_mut(ed, id) {
        n.thread[0].draft = true;
    }
    Ok((id, format!("finding #{id} pinned at {shown}:{}", line + 1)))
}

/// The chat turn ended: notes still waiting stop waiting — a single one gets Claude's plain answer (it didn't
/// use `reply_to`); a review's are answered in the chat.
pub fn turn_done(ed: &mut Editor, answer: Option<String>) {
    let ids = std::mem::take(&mut ed.session.notes.turn);
    let single = ids.len() == 1;
    for id in ids {
        let Some(n) = find_mut(ed, id) else { continue };
        if !n.waiting {
            continue;
        }
        n.waiting = false;
        if single && let Some(a) = answer.as_ref().map(|a| a.trim().to_string()).filter(|a| !a.is_empty()) {
            n.thread.push(Entry { by: By::Author, text: a, draft: false });
        }
    }
    stale_check(ed);
}

/// Whether a draft's lines are still as they were pinned: at `lines` in `text`, or moved within 500 lines
/// (a file not open has no mark to follow lines added above).
fn as_pinned(text: &ropey::Rope, lines: &Range<usize>, pinned: &[u8; 32]) -> bool {
    let at = |off: isize| {
        let (Some(a), Some(b)) = (lines.start.checked_add_signed(off), lines.end.checked_add_signed(off))
        else {
            return false;
        };
        lines_digest(text, &(a..b)).as_ref() == Some(pinned)
    };
    at(0) || (1..=500isize).any(|d| at(-d) || at(d))
}

/// A chat turn ended: the drafts — your comments, the reviewer's findings, waiting for `space v s` — whose
/// lines changed since they were pinned (fixed, maybe, by the author asked in the chat — #197) are offered to
/// close, on a card (`stale_card`); each is asked about once. Compared on a worker: an open file's text as
/// it is (its notes follow their marks), a file not open read from disk.
pub fn stale_check(ed: &mut Editor) {
    type Wanted = (u64, PathBuf, Range<usize>, [u8; 32], Option<(ropey::Rope, bool)>);
    let (notes, docs) = (&ed.session.notes, &ed.docs);
    let wanted: Vec<Wanted> = notes
        .list
        .iter()
        .filter(|n| n.thread.iter().any(|e| e.draft) && !notes.stale_asked.contains(&n.id))
        .filter_map(|n| {
            let pinned = n.pinned?;
            let doc = docs.iter().find(|d| !d.loading && d.path.as_deref() == Some(n.path.as_path()));
            Some((
                n.id,
                n.path.clone(),
                n.lines.clone(),
                pinned,
                doc.map(|d| (d.text.clone(), n.at.is_some())),
            ))
        })
        .collect();
    if wanted.is_empty() {
        return;
    }
    ed.events.jobs().spawn(move || {
        let stale: Vec<u64> = wanted
            .into_iter()
            .filter(|(_, path, lines, pinned, open)| match open {
                // (on its mark: where its lines are now)
                Some((text, true)) => lines_digest(text, lines).as_ref() != Some(pinned),
                Some((text, false)) => !as_pinned(text, lines, pinned),
                None => std::fs::read_to_string(path)
                    .map_or(true, |t| !as_pinned(&ropey::Rope::from_str(&t), lines, pinned)),
            })
            .map(|w| w.0)
            .collect();
        move |ed: &mut Editor| stale_card(ed, stale)
    });
}

/// The drafts a card names at most.
const STALE_SHOWN: usize = 6;

/// The card for drafts `ids` whose lines changed since they were pinned: `y` closes them, `r` lists them one
/// by one, `n` keeps them. One card a session (cardburden 6 — a card each turn's end, before): a later turn's
/// drafts join the card up, its list grown in place — read again before its `y` counts (`rev`).
fn stale_card(ed: &mut Editor, ids: Vec<u64>) {
    let notes = &ed.session.notes;
    let new: Vec<u64> = notes
        .list
        .iter()
        .filter(|n| ids.contains(&n.id) && is_draft(n) && !notes.stale_asked.contains(&n.id))
        .map(|n| n.id)
        .collect();
    if new.is_empty() {
        ed.unchanged = true;
        return;
    }
    ed.session.notes.stale_asked.extend(&new);
    let up = stale_ids(ed).unwrap_or_default();
    let all: Vec<u64> =
        up.iter().chain(&new).copied().filter(|&id| find(ed, id).is_some_and(is_draft)).collect();
    let (title, q, decided) = stale_text(ed, &all);
    match stale_offer(ed) {
        Some(o) => {
            (o.title, o.question, o.decided) = (title, q, Some(decided));
            o.what = crate::offer::What::NotesStale { ids: all };
            o.rev += 1;
        }
        None => {
            let what = crate::offer::What::NotesStale { ids: all };
            let o = crate::offer::Offer::new(title, &q, "", String::new(), what).deciding(decided);
            ed.push_offer(o);
        }
    }
}

/// Whether note `n` holds a draft — it waits for `space v s`.
fn is_draft(n: &Note) -> bool {
    n.thread.iter().any(|e| e.draft)
}

/// The stale card's title, its text and the lines saying what it closes (its list), for drafts `ids`.
fn stale_text(ed: &Editor, ids: &[u64]) -> (String, String, Range<usize>) {
    let stale: Vec<&Note> = ids.iter().filter_map(|&id| find(ed, id)).collect();
    let k = stale.len();
    let mut q = String::from("Their lines changed since they were pinned — fixed in the chat, maybe:\n");
    for n in stale.iter().take(STALE_SHOWN) {
        let by = n.thread.first().map_or(By::Author, |e| e.by).label();
        let at = format!("{}:{}", crate::chat::shown(&ed.session.root, &n.path), n.lines.start + 1);
        q.push_str(&format!("\n  {at} · {by} · {}", n.gist()));
    }
    if k > STALE_SHOWN {
        q.push_str(&format!("\n  … {} more — r lists them all", k - STALE_SHOWN));
    }
    let listed = q.split('\n').count();
    q.push_str("\n\nKept, space v s still sends them.");
    (format!("Close {} on changed lines?", crate::nav::count(k, "draft")), q, 0..listed)
}

/// Where this session's stale card is: up in front (`offers`), or parked while the session is behind.
fn stale_offer(ed: &mut Editor) -> Option<&mut crate::offer::Offer> {
    let is = |o: &crate::offer::Offer| matches!(o.what, crate::offer::What::NotesStale { .. });
    match ed.background() {
        true => ed.session.parked.iter_mut().find(|o| is(o)),
        false => ed.offers.iter_mut().find(|o| is(o)),
    }
}

/// The drafts this session's stale card lists — None: no card up.
fn stale_ids(ed: &mut Editor) -> Option<Vec<u64>> {
    match &stale_offer(ed)?.what {
        crate::offer::What::NotesStale { ids } => Some(ids.clone()),
        _ => None,
    }
}

/// The drafts the stale card in front lists — each says `stale ·` while it's up (its line end, its card, the
/// review rail's count). Empty: none up.
pub fn stale_shown(ed: &Editor) -> &[u64] {
    ed.offers
        .iter()
        .find_map(|o| match &o.what {
            crate::offer::What::NotesStale { ids } => Some(ids.as_slice()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Every event: the stale card follows its drafts — one closed or sent since (`C-x` in its list, `space v s`,
/// `:note-close`) leaves its list; none left, the card goes. Fewer, its `y` closes less than was read: not to
/// be read again.
pub fn stale_sync(ed: &mut Editor) {
    let Some(ids) = stale_ids(ed) else { return };
    let left: Vec<u64> = ids.iter().copied().filter(|&id| find(ed, id).is_some_and(is_draft)).collect();
    if left.len() == ids.len() {
        return;
    }
    if left.is_empty() {
        let is = |o: &crate::offer::Offer| matches!(o.what, crate::offer::What::NotesStale { .. });
        let gone = match ed.background() {
            true => ed.session.parked.iter().find(|o| is(o)).map(|o| o.id),
            false => ed.offers.iter().find(|o| is(o)).map(|o| o.id),
        };
        if let Some(id) = gone {
            ed.card_how(id, "its drafts closed or sent");
            ed.session.parked.retain(|o| o.id != id);
            ed.offer_finished(id, Ok(()));
        }
        return;
    }
    let (title, q, decided) = stale_text(ed, &left);
    if let Some(o) = stale_offer(ed) {
        (o.title, o.question, o.decided) = (title, q, Some(decided));
        o.what = crate::offer::What::NotesStale { ids: left };
    }
}

/// The stale drafts list's title (`r` on the card): the notes list of just those, `C-x` closing one.
pub const STALE: &str = "notes · stale";

/// Whether a picker titled `title` is a list of notes (the notes list, or the stale card's).
pub fn is_list(title: &str) -> bool {
    title == NOTES || title == STALE
}

/// `r` on the stale card: its drafts in the notes list, alone — `enter` goes to one, `C-x` closes it (the
/// card's list follows, `stale_sync`). No key of the code's changes meaning; the card stays under it.
pub fn stale_list(ed: &mut Editor) {
    let items = list_items(ed, STALE);
    ed.open_picker(crate::picker::Picker::new(STALE, items, false), None);
}

/// The rows of the list titled `title`: every note, or the stale card's drafts.
pub fn list_items(ed: &Editor, title: &str) -> Vec<crate::picker::Item> {
    let mut items = picker_items(ed);
    if title == STALE {
        let shown = stale_shown(ed);
        items.retain(|i| matches!(i.action, crate::picker::Action::Note { id, .. } if shown.contains(&id)));
    }
    items
}

/// The stale drafts card's answer: `y` closes notes `ids` (those still there), `n` keeps them.
pub fn stale_answer(ed: &mut Editor, card: u64, ids: Vec<u64>, yes: bool) {
    ed.offer_finished(card, Ok(()));
    if !yes {
        return ed.set_status("kept — space v s still sends them, :note-close drops one");
    }
    let before = ed.session.notes.list.len();
    ed.session.notes.list.retain(|n| !ids.contains(&n.id));
    let k = before - ed.session.notes.list.len();
    ed.set_status(format!("{} closed", crate::nav::count(k, "draft")));
}

/// Every note `by` began closed — every note, `None` — said: `:notes-clear [who]`, the notes list's `A-x`
/// (#197). When some hold your unsent drafts (gone with them), a card asks first (`What::NotesClose`) — the
/// notes as they are now, one added later isn't among them. Whether it asked.
pub fn close_by(ed: &mut Editor, by: Option<By>) -> bool {
    let whose = |n: &Note| by.is_none() || n.thread.first().map(|e| e.by) == by;
    let ids: Vec<u64> = ed.session.notes.list.iter().filter(|n| whose(n)).map(|n| n.id).collect();
    let drafts: Vec<&Note> = ed
        .session
        .notes
        .list
        .iter()
        .filter(|n| whose(n) && n.thread.iter().any(|e| e.by == By::You && e.draft))
        .collect();
    if drafts.is_empty() {
        close_ids(ed, by, &ids);
        return false;
    }
    let k = drafts.len();
    let mut q = format!(
        "{} your unsent drafts — closed, they're gone:\n",
        if k == 1 { "One holds" } else { "These hold" }
    );
    for n in drafts.iter().take(STALE_SHOWN) {
        let at = format!("{}:{}", crate::chat::shown(&ed.session.root, &n.path), n.lines.start + 1);
        q.push_str(&format!("\n  {at} · {}", n.gist()));
    }
    if k > STALE_SHOWN {
        q.push_str(&format!("\n  … {} more", k - STALE_SHOWN));
    }
    q.push_str("\n\nKept, space v s still sends them; C-x in the notes list closes one.");
    let title = match by {
        Some(by) => format!("Close the {}'s {}?", by.label(), crate::nav::count(ids.len(), "note")),
        None => format!("Close all {}?", crate::nav::count(ids.len(), "note")),
    };
    let what = crate::offer::What::NotesClose { by, ids };
    ed.push_offer(crate::offer::Offer::new(title, &q, "", String::new(), what));
    true
}

/// The card's answer (`What::NotesClose`): `y` closes notes `ids` (those still there), `n` keeps them.
pub fn close_by_answer(ed: &mut Editor, card: u64, by: Option<By>, ids: Vec<u64>, yes: bool) {
    ed.offer_finished(card, Ok(()));
    if !yes {
        return ed.set_status("kept — space v s still sends your drafts, C-x in the notes list closes one");
    }
    close_ids(ed, by, &ids);
}

/// Notes `ids` closed, said as whose they were.
fn close_ids(ed: &mut Editor, by: Option<By>, ids: &[u64]) {
    let before = ed.session.notes.list.len();
    ed.session.notes.list.retain(|n| !ids.contains(&n.id));
    let n = before - ed.session.notes.list.len();
    ed.set_status(match by {
        Some(by) => format!("{n} of the {}'s notes closed", by.label()),
        None => format!("{} closed", crate::nav::count(n, "note")),
    });
}

/// Note `id` closed, said.
pub fn close(ed: &mut Editor, id: u64) {
    ed.session.notes.list.retain(|n| n.id != id);
    ed.set_status(format!("note #{id} closed"));
}

/// The notes list's title — its keys and hints go by it.
pub const NOTES: &str = "notes";

/// A note's row in the notes list: where it is and what it says first.
fn row_label(ed: &Editor, n: &Note) -> String {
    format!("{}:{}  {}", crate::chat::shown(&ed.session.root, &n.path), n.lines.start + 1, n.gist())
}

/// The note on a row of the notes list (`C-x`, `A-x`): by the id the row carries — two notes saying the
/// same on the same line are two rows — if it's still there.
pub fn of_row(ed: &Editor, item: &crate::picker::Item) -> Option<u64> {
    let crate::picker::Action::Note { id, .. } = item.action else { return None };
    find(ed, id).map(|n| n.id)
}

/// The author who began note `id`.
pub fn began(ed: &Editor, id: u64) -> Option<By> {
    find(ed, id).and_then(|n| n.thread.first()).map(|e| e.by)
}

/// Who `word` names, as `:notes-clear` takes it: `reviewer`, `author` (or `claude`), `you`.
pub fn by_word(word: &str) -> Option<By> {
    match word {
        "reviewer" => Some(By::Reviewer),
        "author" | "claude" => Some(By::Author),
        "you" => Some(By::You),
        _ => None,
    }
}

// ── You ───────────────────────────────────────────────────────────────────

/// `space n`: reply to the note here, or start one (asking Claude about the line or selection).
pub fn start(ed: &mut Editor) {
    let target = here(ed).map(|n| n.id);
    // On a walk-through's stop: it holds still while you ask
    crate::follow::hold(ed);
    // In a review a new one is a comment, kept as a draft — said so, on its line (#134: "ask claude:" wasn't
    // taken for the review's comment input the rail's `space n comment` promised)
    let comment = (target.is_none() && crate::codereview::drafting(ed))
        .then(|| format!("comment on line {}:", ed.cursor_line() + 1));
    ed.open_prompt(crate::editor::PromptKind::Note(target), "");
    if let Some(p) = ed.prompt.as_mut().filter(|_| comment.is_some()) {
        p.label = comment;
    }
}

/// The prompt was sent.
pub fn submit(ed: &mut Editor, target: Option<u64>, text: &str) {
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    if ed.session.chat.as_ref().is_some_and(|c| c.busy()) && !crate::codereview::drafting(ed) {
        return ed.set_status("Claude is still answering — reply once it's done (C-c in the chat stops it)");
    }
    // During a review it's a draft: it goes with the rest (`space v s`), nothing is sent now
    let draft = crate::codereview::drafting(ed);
    let fresh = target.filter(|&id| find(ed, id).is_some()).is_none();
    // A question at a walk-through's stop is about the stop: its lines, and what Claude said they do
    let stop = (fresh && !draft).then(|| crate::follow::stop_here(ed)).flatten();
    let id = match target.filter(|&id| find(ed, id).is_some()) {
        Some(id) => {
            let n = find_mut(ed, id).expect("checked");
            n.thread.push(Entry { by: By::You, text: text.to_string(), draft });
            n.waiting = !draft;
            id
        }
        None => {
            let Some(path) = ed.doc().path.clone() else {
                return ed.set_status("save the file first — notes live in files");
            };
            let doc = ed.doc();
            let r = doc.selection().primary();
            let (a, b) = (
                mv::line_of(&doc.text, r.from()),
                mv::line_of(&doc.text, r.to().saturating_sub(1).max(r.from())),
            );
            let lines = match &stop {
                Some((_, lines, _)) if a == b => lines.clone(),
                _ => a..b + 1,
            };
            let id = add(ed, &path, lines, By::You, text);
            if draft && let Some(n) = find_mut(ed, id) {
                n.waiting = false;
                n.thread[0].draft = true;
            }
            id
        }
    };
    if draft {
        let k = ed.session.notes.list.iter().filter(|n| n.thread.iter().any(|e| e.draft)).count();
        return ed.note(format!("draft saved — {k} in this review · space v s sends them"));
    }
    let n = find(ed, id).expect("just made");
    let chip = format!(
        "¶ #{id} · {} L{}",
        n.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default(),
        n.lines.start + 1
    );
    let mut about = message(ed, n);
    if let Some((_, _, why)) = &stop {
        let at = about.len() - "</note>".len();
        about.insert_str(at, &format!("(a stop of your walk-through — you said: {why})\n"));
    }
    // The chat panel carries the conversation — opened in the background if closed; the keys stay here
    if ed.session.chat.is_none() {
        crate::chat::open(ed);
        if let Some(c) = &mut ed.session.chat {
            c.focused = false;
            c.unfolded = false;
        }
    }
    if crate::chat::submit(ed, text, Some(about), Some(chip)) {
        ed.session.notes.turn = vec![id];
        return;
    }
    // Claude never saw it — take it back (the chat panel says why)
    if fresh {
        ed.session.notes.list.retain(|n| n.id != id);
    } else if let Some(n) = find_mut(ed, id) {
        n.thread.pop();
        n.waiting = false;
    }
    ed.set_warning("couldn't reach claude — the chat panel says why");
}

/// What goes to Claude with a reply: where the note is, the code there and the thread so far.
fn message(ed: &Editor, n: &Note) -> String {
    let code = ed
        .docs
        .iter()
        .find(|d| d.path.as_deref() == Some(n.path.as_path()))
        .map(|d| {
            let last = mv::last_line(&d.text);
            let (a, b) = (n.lines.start.min(last), n.lines.end.saturating_sub(1).min(last));
            d.text
                .slice(d.text.line_to_char(a)..d.text.line_to_char((b + 1).min(d.text.len_lines())))
                .to_string()
        })
        .unwrap_or_default();
    let mut s = format!(
        "<note id=\"{}\" path=\"{}\" lines=\"{}-{}\">\n<code>\n{}</code>\n",
        n.id,
        crate::chat::shown(&ed.session.root, &n.path),
        n.lines.start + 1,
        n.lines.end,
        code
    );
    // The thread up to (not including) the message being sent
    for e in &n.thread[..n.thread.len().saturating_sub(1)] {
        s.push_str(e.by.word());
        s.push_str(": ");
        s.push_str(&e.text);
        s.push('\n');
    }
    s.push_str("</note>");
    s
}

/// `:note-close` — drop the note here.
pub fn close_here(ed: &mut Editor) -> Result<(), String> {
    let id = here(ed).map(|n| n.id).ok_or("no note on this line")?;
    close(ed, id);
    Ok(())
}

/// `]n`/`[n` — the next/previous note in (file, line, age) order across every file, wrapping around; notes
/// sharing a line are visited one by one (the card shows the one stepped to).
pub fn step(ed: &mut Editor, forward: bool) {
    let key = |n: &Note| (n.path.clone(), n.lines.start, n.id);
    let path = ed.doc().path.clone().unwrap_or_default();
    // From the note shown here, else from the cursor line
    let here = here(ed).map(key).unwrap_or((path, ed.cursor_line(), 0));
    let mut all: Vec<(PathBuf, usize, u64)> = ed.session.notes.list.iter().map(key).collect();
    all.sort();
    let target = if forward {
        all.iter().find(|t| **t > here).or(all.first())
    } else {
        all.iter().rev().find(|t| **t < here).or(all.last())
    };
    match target.cloned() {
        Some((p, l, id)) => {
            ed.session.notes.focus = Some(id);
            if let Err(e) = ed.open_at(&p, l, 0) {
                ed.set_error(format!("{e:#}"));
            }
        }
        None => {
            ed.set_status("no notes yet — Claude pins them while it explores, or space n asks about a line")
        }
    }
}

/// `space N` — every note, newest first. `C-x` closes the note on its row, `A-x` every note its author began
/// (#197).
pub fn picker(ed: &mut Editor) {
    if ed.session.notes.list.is_empty() {
        return ed
            .set_status("no notes yet — Claude pins them while it explores, or space n asks about a line");
    }
    let items = picker_items(ed);
    ed.open_picker(crate::picker::Picker::new(NOTES, items, false), None);
}

/// The notes list's rows, newest first.
pub fn picker_items(ed: &Editor) -> Vec<crate::picker::Item> {
    use crate::picker::{Action, Item};
    ed.session
        .notes
        .list
        .iter()
        .rev()
        .map(|n| Item {
            label: row_label(ed, n),
            action: Action::Note { id: n.id, path: n.path.clone(), line: n.lines.start },
            // (who began it, and how many answered)
            hint: {
                let by = n.thread.first().map_or(By::Author, |e| e.by).label();
                match n.thread.len() {
                    1 => by.to_string(),
                    2 => format!("{by} · +1 reply"),
                    k => format!("{by} · +{} replies", k - 1),
                }
            },
            glyph: Some(match n.thread.first().map(|e| e.by) {
                Some(By::Reviewer) => ("¶", "type"),
                _ => ("¶", "ui.accent"),
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> Editor {
        Editor::new(crate::config::Config::default())
    }

    /// Each entry says who wrote it — the chat's `note` the author, the reviewer's `finding` the reviewer
    /// (waiting like a draft), `space n` you — and keeps it through the restore file; a file written before
    /// there was a reviewer (`claude`) reads as the author's.
    #[test]
    fn notes_carry_their_author() {
        let dir = std::env::temp_dir().join(format!("tarae-notes-by-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let f = dir.join("a.rs");
        std::fs::write(&f, "one\ntwo\nthree\n").unwrap();
        let mut ed = editor();
        ed.open(&f).unwrap();
        tool_call(&mut ed, &json!({"path": f, "line": 1, "text": "the author's"})).unwrap();
        let (found, _) =
            finding(&mut ed, &dir, &json!({"path": "a.rs", "line": 2, "text": "the reviewer's"})).unwrap();
        // (`space n` on line 3: with a finding waiting it's a draft too)
        ed.goto_line(2);
        submit(&mut ed, None, "mine");
        let by: Vec<(By, bool)> =
            ed.session.notes.list.iter().map(|n| (n.thread[0].by, n.thread[0].draft)).collect();
        assert_eq!(by, [(By::Author, false), (By::Reviewer, true), (By::You, true)]);
        assert_eq!(ed.session.notes.list[1].id, found);
        let mut saved = to_json(&ed.session.notes, &ed.docs);
        let words: Vec<&str> = saved.iter().map(|n| n["thread"][0][0].as_str().unwrap()).collect();
        assert_eq!(words, ["claude", "reviewer", "you"]);
        saved.push(json!({"path": f, "lines": [0, 1], "thread": [["claude", "from an older tarae"]]}));
        let mut ed = editor();
        restore_from(&mut ed.session.notes, &mut ed.docs, saved);
        let by: Vec<(By, bool)> =
            ed.session.notes.list.iter().map(|n| (n.thread[0].by, n.thread[0].draft)).collect();
        assert_eq!(by, [(By::Author, false), (By::Reviewer, true), (By::You, true), (By::Author, false)]);
        // The reviewer pins findings in the checkout only, and answers no thread
        let outside = std::env::temp_dir().join(format!("tarae-notes-by-out-{}.rs", std::process::id()));
        std::fs::write(&outside, "x\n").unwrap();
        assert!(finding(&mut ed, &dir, &json!({"path": outside, "line": 1, "text": "out"})).is_err());
        assert!(finding(&mut ed, &dir, &json!({"reply_to": 1, "text": "re"})).is_err());
        std::fs::remove_file(&outside).ok();
        std::fs::remove_dir_all(&dir).ok();
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

    /// Events handled until `done` (a worker's result), at most 5 s.
    fn settle_until(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done(ed) {
            let left = end.saturating_duration_since(std::time::Instant::now());
            let ev = ed.events.recv_timeout(left).expect("event before timeout");
            ed.handle_event(ev);
        }
    }

    /// Events handled for `ms` — what a worker would bring back by then.
    fn settle_for(ed: &mut Editor, ms: u64) {
        let end = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        while let Some(ev) = ed.events.recv_timeout(end.saturating_duration_since(std::time::Instant::now()))
        {
            ed.handle_event(ev);
        }
    }

    fn gists(ed: &Editor) -> Vec<&str> {
        ed.session.notes.list.iter().map(Note::gist).collect()
    }

    /// The front card drawn and in sight past its settling time, then `keys` (a weighty card takes them only
    /// so — the stale card is one since cardburden 6).
    fn answer(ed: &mut Editor, keys: &str) {
        crate::term::render(ed, &mut Vec::new(), 120, 40).unwrap();
        let o = (ed.offers[0].id, ed.offers[0].rev);
        ed.offer_seen.set(Some((o.0, o.1, std::time::Instant::now() - crate::offer::SETTLE)));
        feed(ed, keys);
    }

    /// #197: the author fixed what the reviewer found (asked in the chat, not with `space v s`) — the
    /// findings stayed drafts. Now a turn's end offers to close the drafts whose lines changed since they
    /// were pinned: a card naming them, `y` closes them; `n` keeps them, not asked about again. A draft whose
    /// lines are as they were, and a note that isn't a draft, stay unasked — and a finding in a file not open
    /// counts its lines as the file had them then.
    #[test]
    fn drafts_on_lines_changed_since_are_offered_to_close() {
        let dir = std::env::temp_dir().join(format!("tarae-notes-stale-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let (f, g) = (dir.join("a.rs"), dir.join("b.rs"));
        std::fs::write(&f, "one\ntwo\nthree\nfour\n").unwrap();
        std::fs::write(&g, "alpha\nbeta\n").unwrap();
        let mut ed = editor();
        ed.open(&f).unwrap();
        tool_call(&mut ed, &json!({"path": f, "line": 1, "text": "the author's"})).unwrap();
        let find = |ed: &mut Editor, path: &str, line: u64, text: &str| {
            finding(ed, &dir, &json!({"path": path, "line": line, "text": text})).unwrap().0
        };
        find(&mut ed, "a.rs", 2, "two is wrong");
        find(&mut ed, "a.rs", 4, "four too");
        // (b.rs isn't open: its lines are read as the finding is pinned)
        let beta = find(&mut ed, "b.rs", 2, "beta");
        settle_until(&mut ed, |ed| ed.session.notes.list.iter().any(|n| n.id == beta && n.pinned.is_some()));
        // The author fixes line 2 in the buffer, and b.rs's line on disk
        ed.goto_line(1);
        feed(&mut ed, "Afixed<esc>");
        std::fs::write(&g, "alpha\nbeta fixed\n").unwrap();
        turn_done(&mut ed, None);
        settle_until(&mut ed, |ed| !ed.offers.is_empty());
        let o = &ed.offers[0];
        assert!(o.title.contains("2 drafts"), "{}", o.title);
        assert!(o.question.contains("a.rs:2") && o.question.contains("b.rs:2"), "{}", o.question);
        assert!(!o.question.contains("a.rs:4"), "its lines are as they were: {}", o.question);
        answer(&mut ed, "y");
        assert_eq!(gists(&ed), ["the author's", "four too"]);
        // Nothing changed since: nothing asked
        turn_done(&mut ed, None);
        settle_for(&mut ed, 300);
        assert!(ed.offers.is_empty());
        // Line 4 changes: asked; `n` keeps it, and it isn't asked again
        ed.goto_line(3);
        feed(&mut ed, "Afixed<esc>");
        turn_done(&mut ed, None);
        settle_until(&mut ed, |ed| !ed.offers.is_empty());
        answer(&mut ed, "n");
        assert_eq!(gists(&ed), ["the author's", "four too"]);
        turn_done(&mut ed, None);
        settle_for(&mut ed, 300);
        assert!(ed.offers.is_empty(), "asked once");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// cardburden 6: the stale card was pushed anew at every turn's end. Now one card a session: a later
    /// turn's stale drafts join it in place — the same card, its list grown, and a `y` before the new list has
    /// been in sight isn't taken. While it's up each of its drafts says `stale ·` at its line end and on its
    /// card. `r` lists just them (the notes list, no key of the code's changed); `C-x` there closes one and the
    /// card's list follows; `y` closes what's left. (The files are the test's own.)
    #[test]
    fn one_stale_card_grows_in_place_and_lists_its_drafts() {
        let dir = std::env::temp_dir().join(format!("tarae-notes-stale-one-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let f = dir.join("a.rs");
        std::fs::write(&f, "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let mut ed = editor();
        ed.open(&f).unwrap();
        for (line, text) in [(2, "two is wrong"), (3, "three too"), (5, "five")] {
            finding(&mut ed, &dir, &json!({"path": "a.rs", "line": line, "text": text})).unwrap();
        }
        // (the author's fix in the turn — made past the card up, which has the keys — then the turn's end)
        let fix = |ed: &mut Editor, line: usize| {
            let up = std::mem::take(&mut ed.offers);
            ed.goto_line(line);
            feed(ed, "Afixed<esc>");
            ed.offers = up;
            turn_done(ed, None);
        };
        fix(&mut ed, 1);
        settle_until(&mut ed, |ed| !ed.offers.is_empty());
        let (card, rev) = (ed.offers[0].id, ed.offers[0].rev);
        assert_eq!(ed.offers[0].title, "Close 1 draft on changed lines?");
        // Its draft says so: at its line end, and on its card (the cursor on its line)
        ed.goto_line(1);
        let rows = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 120, 40).unwrap();
            let mut parser = vt100::Parser::new(40, 120, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        let screen = rows(&mut ed);
        assert!(screen.contains("¶ stale · two is wrong"), "{screen}");
        assert!(screen.contains("stale · its lines changed since it was pinned"), "{screen}");
        assert!(!screen.contains("stale · three too"), "{screen}");
        // A later turn: the same card, grown — its `y` waits for the new list to be seen
        answer(&mut ed, "");
        fix(&mut ed, 2);
        settle_until(&mut ed, |ed| ed.offers.iter().any(|o| o.title.contains("2 drafts")));
        assert_eq!(ed.offers.len(), 1, "one card");
        assert_eq!(ed.offers[0].id, card, "the same card");
        assert!(ed.offers[0].rev > rev, "to be read again");
        feed(&mut ed, "y");
        assert_eq!(
            gists(&ed),
            ["two is wrong", "three too", "five"],
            "not taken before the new list was seen"
        );
        // `r`: just its drafts, listed; `C-x` closes one — the card's list follows
        answer(&mut ed, "r");
        let p = ed.picker.as_ref().expect("its list");
        assert_eq!(p.title, STALE);
        let listed: Vec<&str> = p.items().iter().map(|i| i.label.as_str()).collect();
        assert_eq!(listed.len(), 2, "{listed:?}");
        assert!(listed.iter().all(|l| !l.contains("five")), "{listed:?}");
        feed(&mut ed, "<C-x>");
        assert_eq!(gists(&ed), ["two is wrong", "five"]);
        assert_eq!(ed.picker.as_ref().map(|p| p.items().len()), Some(1), "the list follows");
        assert_eq!(ed.offers[0].title, "Close 1 draft on changed lines?", "so does the card");
        feed(&mut ed, "<esc>");
        answer(&mut ed, "y");
        assert_eq!(gists(&ed), ["five"]);
        assert!(ed.offers.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The review of 6: a session behind keeps its stale card parked — a later turn's drafts grow that card
    /// there, in place (not a second one, nothing put up in front), and once its drafts are closed it goes from
    /// there too (`stale_sync` behind). (The files are the test's own.)
    #[test]
    fn a_stale_card_behind_grows_and_goes_where_it_is_parked() {
        let dir = std::env::temp_dir().join(format!("tarae-notes-stale-behind-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("back")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let back_root = dir.join("back");
        std::fs::write(back_root.join("a.rs"), "one\ntwo\nthree\n").unwrap();
        let mut ed = editor();
        ed.session.root = dir.clone();
        let back = ed.session_add(back_root.clone());
        let mut ids = Vec::new();
        ed.with_session(back, |ed| {
            for line in [1, 2] {
                let args = json!({"path": "a.rs", "line": line, "text": format!("found {line}")});
                ids.push(finding(ed, &back_root, &args).unwrap().0);
            }
        });
        let parked = |ed: &Editor| -> Vec<(u64, u64, Vec<u64>)> {
            let l = ed.sessions.iter().find(|l| l.id == back).expect("behind");
            l.parked
                .iter()
                .filter_map(|o| match &o.what {
                    crate::offer::What::NotesStale { ids } => Some((o.id, o.rev, ids.clone())),
                    _ => None,
                })
                .collect()
        };
        ed.with_session(back, |ed| {
            assert!(ed.background());
            stale_card(ed, vec![ids[0]]);
        });
        let first = parked(&ed);
        assert_eq!(first.len(), 1, "parked behind");
        assert!(ed.offers.is_empty(), "nothing in front");
        // A later turn behind: the same card, grown, to be read again
        ed.with_session(back, |ed| stale_card(ed, vec![ids[1]]));
        let grown = parked(&ed);
        assert_eq!(grown.len(), 1, "one card");
        assert_eq!(grown[0].0, first[0].0, "the same card");
        assert_eq!(grown[0].2, ids, "its list grown");
        assert!(grown[0].1 > first[0].1);
        assert!(ed.offers.is_empty(), "nothing in front");
        // Its drafts closed, still behind: it goes
        ed.with_session(back, |ed| {
            close(ed, ids[0]);
            stale_sync(ed);
        });
        assert_eq!(parked(&ed)[0].2, [ids[1]], "its list follows");
        ed.with_session(back, |ed| {
            close(ed, ids[1]);
            stale_sync(ed);
        });
        assert!(parked(&ed).is_empty(), "gone from where it was parked");
        assert!(ed.offers.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #197: closing notes by who wrote them — in the notes list (`space N`) `C-x` closes the note on its row,
    /// `A-x` every note its author began (the reviewer's findings, all at once); `:notes-clear reviewer`
    /// does the same from the command line, `:notes-clear` closes every note.
    #[test]
    fn notes_close_by_row_or_by_author() {
        let dir = std::env::temp_dir().join(format!("tarae-notes-close-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let f = dir.join("a.rs");
        std::fs::write(&f, "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let mut ed = editor();
        ed.open(&f).unwrap();
        tool_call(&mut ed, &json!({"path": f, "line": 1, "text": "the author's"})).unwrap();
        finding(&mut ed, &dir, &json!({"path": "a.rs", "line": 2, "text": "found one"})).unwrap();
        finding(&mut ed, &dir, &json!({"path": "a.rs", "line": 3, "text": "found two"})).unwrap();
        ed.goto_line(3);
        submit(&mut ed, None, "mine");
        tool_call(&mut ed, &json!({"path": f, "line": 5, "text": "the author's again"})).unwrap();
        feed(&mut ed, "<space>Nfound two<A-x>");
        assert_eq!(gists(&ed), ["the author's", "mine", "the author's again"], "the reviewer's, all");
        let rows = ed.picker.as_ref().map(|p| (p.items().len(), p.shown().count()));
        assert_eq!(rows, Some((3, 0)), "the list follows (narrowed to `found two`: none left)");
        assert!(ed.toasts.iter().any(|t| t.text.contains("2 of the reviewer's notes closed")), "said");
        feed(&mut ed, "<esc><space>Nmine<C-x>");
        assert_eq!(gists(&ed), ["the author's", "the author's again"]);
        feed(&mut ed, "<esc>");
        crate::typed::execute(&mut ed, "notes-clear author").unwrap();
        assert!(ed.session.notes.list.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Closing by author when some of those notes hold your unsent drafts (a reply on a finding, a comment of
    /// your own): a card names them first — `n` keeps every one, `y` closes them; from the notes list the
    /// card takes the keys (the list goes). A bare `:notes-clear` asks the same. None of yours among them:
    /// closed at once, as before.
    #[test]
    fn closing_by_author_asks_first_over_your_unsent_drafts() {
        let dir = std::env::temp_dir().join(format!("tarae-notes-drafts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let f = dir.join("a.rs");
        std::fs::write(&f, "one\ntwo\nthree\nfour\nfive\n").unwrap();
        let mut ed = editor();
        ed.open(&f).unwrap();
        let one = finding(&mut ed, &dir, &json!({"path": "a.rs", "line": 2, "text": "found one"})).unwrap().0;
        finding(&mut ed, &dir, &json!({"path": "a.rs", "line": 3, "text": "found two"})).unwrap();
        ed.goto_line(3);
        submit(&mut ed, None, "mine");
        tool_call(&mut ed, &json!({"path": f, "line": 5, "text": "the author's"})).unwrap();
        // (your draft reply on a finding, and your own comment a draft — as a review writes them)
        let draft = |text: &str| Entry { by: By::You, text: text.into(), draft: true };
        let list = &mut ed.session.notes.list;
        list.iter_mut().find(|n| n.id == one).unwrap().thread.push(draft("my reply"));
        list.iter_mut().find(|n| n.gist() == "mine").unwrap().thread[0].draft = true;
        // The command line: asked, `n` keeps them
        crate::typed::execute(&mut ed, "notes-clear reviewer").unwrap();
        let o = ed.offers.first().expect("the card");
        assert!(o.title.contains("the reviewer's 2 notes"), "{}", o.title);
        assert!(
            o.question.contains("a.rs:2 · found one") && !o.question.contains("found two"),
            "{}",
            o.question
        );
        feed(&mut ed, "n");
        assert_eq!(gists(&ed), ["found one", "found two", "mine", "the author's"], "kept");
        // A bare `:notes-clear` (every note): asked too, naming each that holds a draft; `n` keeps them all
        crate::typed::execute(&mut ed, "notes-clear").unwrap();
        let o = ed.offers.first().expect("the card");
        assert_eq!(o.title, "Close all 4 notes?");
        assert!(
            o.question.contains("a.rs:2 · found one")
                && o.question.contains("a.rs:4 · mine")
                && !o.question.contains("the author's"),
            "{}",
            o.question
        );
        feed(&mut ed, "n");
        assert!(ed.offers.is_empty());
        assert_eq!(gists(&ed), ["found one", "found two", "mine", "the author's"], "kept, every one");
        // The notes list's `A-x`: the card has the keys, `y` closes them
        feed(&mut ed, "<space>Nfound two<A-x>");
        assert!(ed.picker.is_none() && !ed.offers.is_empty(), "the card, not the list");
        feed(&mut ed, "y");
        assert_eq!(gists(&ed), ["mine", "the author's"]);
        // Yours: asked too
        crate::typed::execute(&mut ed, "notes-clear you").unwrap();
        assert!(ed.offers.first().is_some_and(|o| o.question.contains("a.rs:4 · mine")));
        feed(&mut ed, "y");
        assert_eq!(gists(&ed), ["the author's"]);
        // None of yours: at once, bare too
        crate::typed::execute(&mut ed, "notes-clear").unwrap();
        assert!(ed.offers.is_empty() && ed.session.notes.list.is_empty());
        assert!(ed.toasts.iter().any(|t| t.text.contains("1 note closed")), "said");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The notes list's `C-x` closes the note on its row by its id: two notes saying the same on the same
    /// line are two rows, each its own.
    #[test]
    fn a_row_closes_its_own_note_when_two_say_the_same() {
        let dir = std::env::temp_dir().join(format!("tarae-notes-same-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let f = dir.join("a.rs");
        std::fs::write(&f, "one\ntwo\n").unwrap();
        let mut ed = editor();
        ed.open(&f).unwrap();
        tool_call(&mut ed, &json!({"path": f, "line": 2, "text": "same"})).unwrap();
        tool_call(&mut ed, &json!({"path": f, "line": 2, "text": "same"})).unwrap();
        let ids = |ed: &Editor| ed.session.notes.list.iter().map(|n| n.id).collect::<Vec<_>>();
        let (older, newer) = (ids(&ed)[0], ids(&ed)[1]);
        // (newest first: the second row is the older note)
        feed(&mut ed, "<space>N<C-n><C-x>");
        assert_eq!(ids(&ed), [newer], "the row's own note");
        assert!(older != newer);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Saved on quit, back at start — and if the file was edited meanwhile, the note finds its line again;
    /// a note past the end of a shrunk file lands on the last line instead of crashing.
    #[test]
    fn notes_survive_a_restart_and_outside_edits() {
        let dir = std::env::temp_dir().join(format!("tarae-notes-save-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.rs");
        std::fs::write(&f, "a\nb\ntarget();\nc\n").unwrap();
        let f = std::fs::canonicalize(&f).unwrap();
        let mut ed = editor();
        ed.open(&f).unwrap();
        tool_call(&mut ed, &json!({"path": f, "line": 3, "text": "look here"})).unwrap();
        tool_call(&mut ed, &json!({"reply_to": 1, "text": "and here"})).unwrap();
        let mut saved = to_json(&ed.session.notes, &ed.docs);
        assert_eq!(saved[0]["anchor"], "target();");
        // Edited outside tarae: two lines above
        std::fs::write(&f, "x\ny\na\nb\ntarget();\nc\n").unwrap();
        saved.push(
            json!({"path": f, "lines": [50, 52], "anchor": "gone", "thread": [["claude", "past the end"]]}),
        );
        let mut ed = editor();
        restore_from(&mut ed.session.notes, &mut ed.docs, saved);
        assert_eq!(ed.session.notes.list.len(), 2);
        ed.open(&f).unwrap();
        attach(&mut ed);
        let n = &ed.session.notes.list[0];
        assert_eq!((n.lines.clone(), n.thread.len()), (4..5, 2), "re-anchored on its line's text");
        assert_eq!(ed.session.notes.list[1].lines, 5..6, "clamped to the last line");
        std::fs::remove_dir_all(&dir).ok();
    }
}
