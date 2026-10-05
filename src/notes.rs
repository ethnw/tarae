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
        lines,
        thread: vec![Entry { by, text: text.trim().to_string(), draft: false }],
        waiting: by == By::You,
        anchor: None,
    });
    ed.session.notes.focus = Some(id);
    attach(ed);
    id
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
            let thread: Vec<Value> = n
                .thread
                .iter()
                .map(|e| json!([e.by.word(), e.text, e.draft]))
                .collect();
            json!({ "path": n.path, "lines": [n.lines.start, n.lines.end], "anchor": anchor, "thread": thread })
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
    // (and the lines looked at in its reviews — `risk.rs`)
    s.looked.extend(restore.looked(&s.root));
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
    ed.session.notes.list.retain(|n| n.id != id);
    ed.set_status(format!("note #{id} closed"));
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

/// `space N` — every note, newest first.
pub fn picker(ed: &mut Editor) {
    use crate::picker::{Action, Item};
    if ed.session.notes.list.is_empty() {
        return ed
            .set_status("no notes yet — Claude pins them while it explores, or space n asks about a line");
    }
    let items: Vec<Item> = ed
        .session
        .notes
        .list
        .iter()
        .rev()
        .map(|n| Item {
            label: format!(
                "{}:{}  {}",
                crate::chat::shown(&ed.session.root, &n.path),
                n.lines.start + 1,
                n.gist()
            ),
            action: Action::Goto { path: n.path.clone(), line: n.lines.start, col: 0 },
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
        .collect();
    ed.open_picker(crate::picker::Picker::new("notes", items, false), None);
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
