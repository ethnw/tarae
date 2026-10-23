//! The chat's transcript as a document (panes P2c, `target/sb/panes-plan.md` §2.1e): the conversation's text, a
//! read-only `Document` that follows what each frame lays out (`sync`) — one line a paragraph, as the code's lines
//! under soft wrap: the chat's layout wraps it into its rows, and those rows are the document's (`wrap::Given`), so
//! `j`/`k`, the pages and the `z` moves step by what's drawn, while search, `w` and `x` cross a wrap and a resize
//! moves no cursor.
//!
//! What the drawer adds is never text: the user's bar, a thought's bar, a note's `· `/`≈ `, a code well's edge, a
//! wrapped row's hanging indent (`ChatRow.skip`), a tool row's knot, a change's stamps, the writing caret
//! (`ChatRow.tail`), the bleeding head (a style). The air before a question or a run of tool rows is an empty line; the
//! frame's own rows — what a live run waits on, `thinking…`, the empty chat's words (`Of::Own`) — aren't in it,
//! never under the cursor. A message's lines are made again only when its rows are (`Msg.cache`: the `Arc` the
//! layout keeps) — the one being written, mostly — and replace only its own lines when their text changed.
//!
//! It's kept as the chat's own drawer's (`term::draw_chat` paints its rows' spans, the selections over them by the
//! rows' places here), not `draw_text`'s: its rows are markdown's spans.
//!
//! The chat's view mode is normal mode on it (`esc` from the input, `space >`, the wheel; `i` back, `esc` again to
//! the code): the keymap's chat table — normal mode whole, `[view]` and `[view.chat]` over it — run with the
//! document swapped in (`docpane::swap`); an edit put back and said. While it has the keys it holds still — what
//! comes is appended below what's in sight, said (`● normal · new text after`), `G` catches up; without them the chat
//! shows its newest rows, `Chat.scroll` up from the bottom. Its selection is where you were: the next view lands
//! there, in this conversation.

use std::sync::Arc;

use unicode_width::UnicodeWidthStr;

use crate::chat::{Chat, ChatRow};
use crate::docpane::{After, Pane, Which};
use crate::document::Document;
use crate::editor::{Editor, Mode};
use crate::key::{Code, Key};
use crate::keymap::MappableCommand;
use crate::movement as mv;
use crate::selection::{Range, Selection};
use crate::session::At;
use crate::transaction::{Assoc, Change, Transaction};
use crate::view::{Surface, Was};
use crate::wrap::{Given, Part, Row};

/// What a run of the chat's rows is (`Laid`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Of {
    /// Message `i`'s rows (`Chat.msgs`).
    Msg(usize),
    /// The air before a question or a run of tool rows — where a message is, not what it is: an empty line.
    Air,
    /// The frame's own: no text of the document.
    Own,
}

/// The chat's rows as a frame laid them out, in order: each message's kept rows, the air between them, the frame's
/// own (`term::chat_laid`, then `draw_chat`'s; `Screen.chat_laid`).
pub type Laid = Vec<(Of, Arc<[ChatRow]>)>;

/// A row's place in its piece's text: its line (from the piece's first), where its text starts on the line and
/// its display column there, the bytes of it drawn (a cut code row's `…` stands for more), the cells before it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Place {
    pub line: usize,
    pub from: usize,
    pub col: usize,
    pub len: usize,
    pub x0: usize,
}

/// A row's text as the document has it: its characters past `skip` and before `tail`, line breaks and tabs as
/// spaces (one byte each — the painter counts the same bytes in its spans), blanks at its end off; and what a cut
/// code row's `…` stands for (`rest`), the `…` itself not the text's.
fn body(r: &ChatRow) -> (String, Option<&str>) {
    let n: usize = r.spans.iter().map(|s| s.text.chars().count()).sum();
    let keep = n.saturating_sub(r.tail);
    let chars = r.spans.iter().flat_map(|s| s.text.chars()).take(keep).skip(r.skip);
    let mut s: String = chars.map(|c| if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c }).collect();
    // (a code well's padding after a cut row's `…` isn't the text either)
    s.truncate(s.trim_end().len());
    match r.rest.as_deref() {
        Some(rest) if s.ends_with('…') => {
            s.truncate(s.len() - '…'.len_utf8());
            (s, Some(rest))
        }
        _ => (s, None),
    }
}

/// The cells of `r`'s first `skip` characters — what the drawer put before its text.
fn lead(r: &ChatRow) -> usize {
    let s: String = r.spans.iter().flat_map(|s| s.text.chars()).take(r.skip).collect();
    s.width()
}

/// A piece's rows as the document's text — a row that goes on from the one before (`ChatRow.join`) on its line,
/// the space its break took put back — each line ending in a line break; and each row's place in it.
pub fn lines_of(rows: &[ChatRow]) -> (String, Vec<Place>) {
    let mut text = String::new();
    let mut at = Vec::with_capacity(rows.len());
    let (mut line, mut start) = (0, 0);
    for (k, r) in rows.iter().enumerate() {
        match r.join.filter(|_| k > 0) {
            Some(space) => text.push_str(if space { " " } else { "" }),
            None if k > 0 => {
                text.push('\n');
                (line, start) = (line + 1, text.len());
            }
            None => {}
        }
        let (shown, rest) = body(r);
        let from = text.len() - start;
        let col = text[start..].width();
        text.push_str(&shown);
        at.push(Place { line, from, col, len: shown.len(), x0: lead(r) });
        text.push_str(rest.unwrap_or_default());
    }
    if !rows.is_empty() {
        text.push('\n');
    }
    (text, at)
}

/// A piece of the document: what it was made of (the layout's own rows — the same `Arc` is the same rows), its
/// bytes and lines, its rows' places, and its part of `Document.rows` (`given`).
struct Piece {
    of: Of,
    rows: Arc<[ChatRow]>,
    len: usize,
    lines: usize,
    at: Vec<Place>,
    part: Option<Arc<Part>>,
}

impl Piece {
    fn new(of: Of, rows: &Arc<[ChatRow]>) -> (Piece, String) {
        let (text, at) = lines_of(rows);
        let lines = at.last().map_or(0, |a| a.line + 1);
        (Piece { of, rows: rows.clone(), len: text.len(), lines, at, part: None }, text)
    }

    /// Whether `rows` laid out as `of` are this piece as it is: the same rows (`Arc`) — or air, always the one
    /// empty row whichever `Arc` a frame made for it (`term::chat_laid`).
    fn is(&self, of: Of, rows: &Arc<[ChatRow]>) -> bool {
        self.of == of && (of == Of::Air || Arc::ptr_eq(&self.rows, rows))
    }
}

/// The transcript: its document pane, the pieces it was made of (their rows `Document.rows`, a part each), and its
/// view mode's state.
pub struct Transcript {
    pub pane: Pane,
    pieces: Vec<Piece>,
    /// Its view mode is on (`esc` from the input, `space >`, the wheel) — the keys its while the chat has them.
    pub keyed: bool,
    /// What the conversation held when the view came or caught up (`seen`) — another now: something came after.
    seen: u64,
    /// Entered by the wheel: where the keys were.
    by_wheel: Option<Was>,
    /// Its cursor was placed in this conversation: the selection is where you were.
    visited: bool,
}

impl Transcript {
    fn new(id: crate::document::DocId) -> Transcript {
        let mut doc = Document::from_str(id, "");
        doc.wrap = Some(true);
        doc.throwaway = true;
        doc.rows = Some(Arc::new(Given::default()));
        Transcript {
            pane: Pane::new(doc),
            pieces: Vec::new(),
            keyed: false,
            seen: 0,
            by_wheel: None,
            visited: false,
        }
    }

    /// Its rows (the document's, frame rows not counted).
    pub fn rows(&self) -> usize {
        self.pane.doc_in().and_then(|d| d.rows.as_deref()).map_or(0, Given::len)
    }

    /// Row `i` as drawn: its line, where its text starts in the document, the bytes of it drawn, the cells before
    /// it, and what the cell after its text stands for (`Shown.end`).
    pub fn row(&self, i: usize) -> Option<Shown> {
        let d = self.pane.doc_in()?;
        let g = d.rows.as_deref()?;
        let r = g.get(i)?;
        let (line, k) = g.place(i);
        // (its drawn length: its piece's row's — a part each, in order; an empty conversation's row, none)
        let len = g.part_of(i).and_then(|(p, k)| self.pieces.get(p)?.at.get(k)).map_or(0, |a| a.len);
        let drawn_end = r.start + len;
        let end = match g.of(line).get(k + 1) {
            // (a row the layout broke at a space: the space its break took)
            Some(next) => (next.start > drawn_end).then_some(drawn_end),
            // (its line's last: the line break — a cut code row's `…` stands for more, none drawn)
            None => (drawn_end == mv::line_end(&d.text, line)).then_some(drawn_end),
        };
        Some(Shown { line, start: r.start, len, x0: r.x0, end })
    }

    /// The row the view starts at (its top line's rows above the screen counted).
    pub fn top_row(&self) -> usize {
        let Some(d) = self.pane.doc_in() else { return 0 };
        d.rows.as_deref().map_or(0, |g| g.index(d.top, d.top_skip()))
    }

    /// The message whose words the document's `line` is (the air before one: the message before it).
    fn msg_at(&self, line: usize) -> Option<usize> {
        let (mut base, mut last) = (0, None);
        for p in &self.pieces {
            if let Of::Msg(i) = p.of {
                if line < base + p.lines {
                    return Some(i);
                }
                last = Some(i);
            } else if line < base + p.lines {
                return last;
            }
            base += p.lines;
        }
        last
    }

    /// The row of message `i`'s row `k` (its last, past its end).
    fn row_of_msg(&self, i: usize, k: usize) -> Option<usize> {
        let mut at = 0;
        for p in &self.pieces {
            if p.of == Of::Msg(i) {
                return Some(at + k.min(p.at.len().saturating_sub(1)));
            }
            at += p.at.len();
        }
        None
    }
}

/// A row of the transcript as it's drawn (`Transcript::row`): its line, where its text starts in the document and
/// the bytes of it drawn, the cells the drawer put before it, and the byte the cell after its text stands for —
/// its line's break, or the space a wrap took — where a cursor there is drawn (None: the next row starts right
/// there).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shown {
    pub line: usize,
    pub start: usize,
    pub len: usize,
    pub x0: usize,
    pub end: Option<usize>,
}

/// Before a frame draws the chat (`term::render_frame`): its transcript follows `laid`, the conversation as this
/// frame laid it out — each message whose rows were laid out again, its lines replaced when their text changed;
/// from the first piece that isn't what it was (another conversation, a message gone), the rest made again. One
/// transaction: selections and the view's top follow it. Never inside a key's command (its document swapped in).
pub fn sync(ed: &mut Editor, laid: &Laid) {
    if ed.session.chat.as_ref().is_some_and(|c| c.transcript.is_none()) {
        let t = Transcript::new(ed.alloc_id());
        ed.session.chat.as_mut().expect("a chat, above").transcript = Some(t);
    }
    if let Some(t) = ed.session.chat.as_mut().and_then(|c| c.transcript.as_mut()) {
        follow(t, laid);
    }
}

/// `t` made to hold `laid`'s pieces (`sync`).
fn follow(t: &mut Transcript, laid: &Laid) {
    let Some(d) = t.pane.doc_in_mut() else { return };
    let new: Vec<(Of, &Arc<[ChatRow]>)> =
        laid.iter().filter(|(of, _)| *of != Of::Own).map(|(of, rows)| (*of, rows)).collect();
    // (the pieces as they were up to the first laid out anew stay where they are — a frame while Claude writes looks
    // at the answer being written, not the conversation's every piece again)
    let same_to = t.pieces.iter().zip(&new).take_while(|(p, (of, rows))| p.is(*of, rows)).count();
    if same_to == t.pieces.len() && same_to == new.len() {
        return;
    }
    // (the pieces of the same kinds in the same places are each looked at; past the first that differs, made anew)
    let kept =
        same_to + t.pieces[same_to..].iter().zip(&new[same_to..]).take_while(|(p, n)| p.of == n.0).count();
    let mut changes = Vec::new();
    let mut at: usize = t.pieces[..same_to].iter().map(|p| p.len).sum();
    let rest = t.pieces.split_off(same_to);
    let mut pieces = std::mem::take(&mut t.pieces);
    for (i, old) in rest.into_iter().enumerate().map(|(j, old)| (same_to + j, old)) {
        if i >= kept {
            let end = d.text.len_bytes();
            let text: String = new[kept..].iter().map(|(of, rows)| piece(&mut pieces, *of, rows)).collect();
            changes.push(Change { from: at, to: end, insert: text });
            break;
        }
        if old.is(new[i].0, new[i].1) {
            at += old.len;
            pieces.push(old);
            continue;
        }
        let (p, text) = Piece::new(new[i].0, new[i].1);
        let was = d.text.byte_slice(at..at + old.len).to_string();
        if was != text {
            // (only what differs: a message being written grows at its end — the cursor and the view on its
            // earlier lines stay where they are)
            let (pre, suf) = same_ends(&was, &text);
            let insert = text[pre..text.len() - suf].to_string();
            changes.push(Change { from: at + pre, to: at + old.len - suf, insert });
        }
        at += old.len;
        pieces.push(p);
    }
    // (more pieces than it had: what came after, at its end)
    if pieces.len() < new.len() {
        let text: String =
            new[pieces.len()..].iter().map(|(of, rows)| piece(&mut pieces, *of, rows)).collect();
        changes.push(Change::insert(d.text.len_bytes(), text));
    }
    if !changes.is_empty() {
        let top = mv::line_start(&d.text, d.top.min(mv::last_line(&d.text)));
        let skip = d.top_skip();
        let tx = Transaction::new(changes);
        let top = tx.map_pos(top, Assoc::Before);
        // (what arrives never takes a selection along: its start and its last character's start stay before the
        // text that comes there — a cursor on the line break of the paragraph being written keeps its cell, the
        // words going on under and after it; `x` on the last line keeps that line when a message comes after it)
        let ends: Vec<(usize, Option<usize>, bool)> = d
            .selection()
            .ranges()
            .iter()
            .map(|r| {
                let last = (r.to() > r.from())
                    .then(|| tx.map_pos(crate::graphemes::prev_boundary(&d.text, r.to()), Assoc::Before));
                (tx.map_pos(r.from(), Assoc::Before), last, r.head < r.anchor)
            })
            .collect();
        let primary = d.selection().primary_index();
        d.apply(&tx);
        let ranges = ends
            .into_iter()
            .map(|(from, last, back)| {
                let to = last.map_or(from, |l| crate::graphemes::next_boundary(&d.text, l).max(from));
                if back { Range::new(to, from) } else { Range::new(from, to) }
            })
            .collect();
        d.set_selection(Selection::new(ranges, primary));
        let line = mv::line_of(&d.text, top);
        d.set_top(line);
        d.top_rows = (skip > 0).then_some((line, skip));
    }
    t.pieces = pieces;
    given(d, &mut t.pieces);
}

/// The bytes `a` and `b` share at their start, and then at their end (not overlapping) — on character boundaries.
fn same_ends(a: &str, b: &str) -> (usize, usize) {
    let pre: usize = a.chars().zip(b.chars()).take_while(|(x, y)| x == y).map(|(x, _)| x.len_utf8()).sum();
    let (a, b) = (&a[pre..], &b[pre..]);
    let suf =
        a.chars().rev().zip(b.chars().rev()).take_while(|(x, y)| x == y).map(|(x, _)| x.len_utf8()).sum();
    (pre, suf)
}

/// A piece made of `rows` put in `pieces`: its text.
fn piece(pieces: &mut Vec<Piece>, of: Of, rows: &Arc<[ChatRow]>) -> String {
    let (p, text) = Piece::new(of, rows);
    pieces.push(p);
    text
}

/// `d`'s rows from its pieces (`Document.rows`), a part each: the parts of the pieces before the first one laid out
/// anew kept as they were, shared (nothing before them changed, so their bytes didn't move); from it on made again
/// — a frame while Claude writes makes the rows of the answer being written, not the conversation's.
fn given(d: &mut Document, pieces: &mut [Piece]) {
    let mut g = d.rows.take().map(Arc::unwrap_or_clone).unwrap_or_default();
    let (mut kept, mut line) = (0, 0);
    for (i, p) in pieces.iter().enumerate() {
        let same = p.part.as_ref().is_some_and(|part| g.parts().get(i).is_some_and(|q| Arc::ptr_eq(q, part)));
        if !same {
            break;
        }
        (kept, line) = (kept + 1, line + p.lines);
    }
    g.keep(kept);
    for p in &mut pieces[kept..] {
        let mut part = Part::default();
        let mut start = 0;
        for (k, a) in p.at.iter().enumerate() {
            if part.first.len() <= a.line {
                part.first.push(part.rows.len());
                start = mv::line_start(&d.text, line + a.line);
            }
            let end_col = match p.at.get(k + 1) {
                Some(n) if n.line == a.line => n.col,
                _ => usize::MAX,
            };
            part.rows.push(Row { start: start + a.from, col: a.col, end_col, x0: a.x0 });
        }
        let part = Arc::new(part);
        p.part = Some(part.clone());
        g.push(part);
        line += p.lines;
    }
    // (an empty conversation: its one empty line, a row of nothing)
    if g.is_empty() {
        g.push(Arc::new(Part { rows: vec![Row::whole(0)], first: vec![0] }));
    }
    d.rows = Some(Arc::new(g));
}

/// `d`'s cursor row in sight in `rows` rows, the scrolloff margin `so` kept — by the rows it's drawn in.
pub fn in_sight(d: &mut Document, rows: usize, so: usize) {
    let Some(g) = d.rows.clone() else { return };
    let rows = rows.max(1);
    let so = so.min(rows.saturating_sub(1) / 2);
    let cursor = d.selection().primary().cursor(&d.text);
    let line = mv::line_of(&d.text, cursor);
    let at = g.index(line, crate::wrap::row_of(g.of(line), cursor));
    let mut top = g.index(d.top, d.top_skip());
    if at < top + so {
        top = at.saturating_sub(so);
    } else if at + so >= top + rows {
        top = (at + so + 1).saturating_sub(rows);
    }
    let top = top.min(g.len().saturating_sub(rows));
    let (line, k) = g.place(top);
    d.set_top(line);
    d.top_rows = (k > 0).then_some((line, k));
}

// ── Its view mode ────────────────────────────────────────────────────────────────────────────────────────

/// The chat's transcript.
pub fn transcript(ed: &Editor) -> Option<&Transcript> {
    ed.session.chat.as_ref()?.transcript.as_ref()
}

fn transcript_mut(ed: &mut Editor) -> Option<&mut Transcript> {
    ed.session.chat.as_mut()?.transcript.as_mut()
}

/// Its view mode is on (it has the keys whenever the chat has them).
pub fn keyed(ed: &Editor) -> bool {
    transcript(ed).is_some_and(|t| t.keyed)
}

/// The chat's view mode has the keys now: on, the chat's keys, the chat unfolded.
pub fn has_keys(ed: &Editor) -> bool {
    keyed(ed) && ed.session.is_at(At::Chat) && ed.session.chat.as_ref().is_some_and(|c| !c.compact())
}

/// What the conversation holds now, as one number: what came changes it.
pub fn seen(c: &Chat) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    c.msgs.len().hash(&mut h);
    if let Some(m) = c.msgs.last() {
        (m.version(), m.text.len()).hash(&mut h);
    }
    (c.state as u8).hash(&mut h);
    h.finish()
}

/// Something came since the view came or caught up (`● normal · new text after`).
pub fn came(ed: &Editor) -> bool {
    let Some(c) = ed.session.chat.as_ref() else { return false };
    c.transcript.as_ref().is_some_and(|t| t.keyed && t.seen != seen(c))
}

/// The rows and columns the chat shows its conversation in, as the last frame drew it.
fn viewport(ed: &Editor) -> (usize, usize) {
    ed.screen.chat_view.get().map_or((20, 60), |a| (a.2.max(1), a.1.max(1)))
}

/// The chat's chords — the screen, pointing, the last code block, a new or earlier conversation, models, the
/// turn's undo, stop, follow's hops: the chat's in its view mode too (§3.4), where normal mode binds them to no
/// more than a move (`C-d` `C-u` `C-f` `C-b` page there; the chat's table leaves the rest out — `Keymaps`).
pub const CHAT_CHORDS: &[char] = &['s', 'o', 'y', 'r', 'l', 'p', 't', 'z', 'c', ']', 'g'];

/// `key` is one of the chat's chords (`C-s`).
pub fn chord(key: Key) -> bool {
    key.ctrl && !key.alt && matches!(key.code, Code::Char(c) if CHAT_CHORDS.contains(&c))
}

/// `key` is one of the chat's chords that its view mode leaves to it: bound to nothing in its table.
pub fn chat_chord(ed: &Editor, key: Key) -> bool {
    let Some(t) = transcript(ed).filter(|t| t.keyed) else { return false };
    chord(key) && ed.keymaps.view(Surface::Chat, t.pane.keys.mode).search(&[key]).is_none()
}

/// Whether `key` goes on from its view mode with nothing under way: `C-w` (or `A-C-w`) and `:`, the editor's from
/// every pane.
fn passes_key(key: Key) -> bool {
    (key.ctrl && key.code == Code::Char('w')) || key.plain_char() == Some(':')
}

/// The keys its view mode with nothing under way lets go on down the chain (`keys_to`'s layer): `C-w`, `:`, and
/// the chat's chords.
pub fn passes(ed: &Editor) -> Vec<Key> {
    let ctrl = |c: char| Key { code: Code::Char(c), ctrl: true, alt: false, shift: false };
    let mut v = vec![ctrl('w'), Key { alt: true, ..ctrl('w') }, Key::plain(Code::Char(':'))];
    v.extend(CHAT_CHORDS.iter().map(|&c| ctrl(c)).filter(|&k| chat_chord(ed, k)));
    v
}

/// `esc` from the chat's input, `space >` from anywhere: its view mode, landed where you were in this
/// conversation, else on the last answer's first row, else the last row. Nothing to read — an empty
/// conversation — the input's `esc` goes to the code (as it did), `space >` to the input, said. The minimized
/// chat keeps its `esc` (to the code); `space >` brings it back. A chat not drawn yet (closed, minimized) is
/// viewed once the frame lays its rows out (`wanted`).
pub fn view(ed: &mut Editor) {
    let from_input = ed.session.is_at(At::Chat);
    let Some(c) = ed.session.chat.as_mut() else {
        crate::chat::open(ed);
        return want(ed);
    };
    if c.compact() {
        match from_input {
            true => ed.session.at = At::Code,
            false => {
                c.unfolded = true;
                want(ed);
            }
        }
        return;
    }
    if c.msgs.is_empty() {
        ed.session.at = if from_input { At::Code } else { At::Chat };
        if !from_input {
            ed.set_status("nothing to view yet — the conversation is empty: type a question");
        }
        return;
    }
    // (`space >` from elsewhere with the chat out of sight: once a frame lays it out; the input's `esc` has it in
    // sight — its transcript what the last frame laid; none laid yet, nothing to read: to the code)
    match (from_input, c.transcript.is_some()) {
        (true, false) => return ed.session.at = At::Code,
        (false, false) => return want(ed),
        (false, true) if ed.screen.chat_view.get().is_none() => return want(ed),
        _ => {}
    }
    enter(ed, None, None);
}

/// Its view mode asked for before its rows were laid out: made once a frame has (`wanted`).
fn want(ed: &mut Editor) {
    if let Some(c) = ed.session.chat.as_mut() {
        c.view_wanted = true;
        ed.session.at = At::Chat;
    }
}

/// After a frame (`term::render`): the view asked for before the chat was drawn, made now — true when it was
/// (the frame is drawn again with it).
pub fn wanted(ed: &mut Editor) -> bool {
    let chat_keys = ed.session.is_at(At::Chat);
    let Some(c) = ed.session.chat.as_mut().filter(|c| c.view_wanted) else { return false };
    c.view_wanted = false;
    if !chat_keys || ed.screen.chat_view.get().is_none() || c.transcript.is_none() {
        return false;
    }
    // (opened on an empty conversation: the input keeps the keys, said)
    if c.msgs.is_empty() {
        ed.set_status("nothing to view yet — the conversation is empty: type a question");
        return true;
    }
    view(ed);
    true
}

/// The last answer: the first of Claude's after your last question, else Claude's last.
fn last_answer(c: &Chat) -> Option<usize> {
    use crate::chat::Role;
    let from = c.msgs.iter().rposition(|m| m.role == Role::User).map_or(0, |i| i + 1);
    (from..c.msgs.len())
        .find(|&i| c.msgs[i].role == Role::Assistant)
        .or_else(|| c.msgs.iter().rposition(|m| m.role == Role::Assistant))
}

/// Its view mode, the keys its: the rows in sight as the chat showed them, the cursor on the row `row` (the
/// wheel's, under the pointer), else where you were, else the last answer's first row (its first is the air above
/// it), else the last. One view at a time: the terminal's ends.
fn enter(ed: &mut Editor, row: Option<usize>, by_wheel: Option<Was>) {
    come(ed, row, by_wheel);
    in_sight_now(ed);
}

/// `enter`'s view, the cursor landed, the rows in sight not moved for it yet.
fn come(ed: &mut Editor, row: Option<usize>, by_wheel: Option<Was>) {
    crate::view::leave(ed);
    let shown = ed.screen.chat_view.get().map_or(0, |a| a.3);
    let Some(c) = ed.session.chat.as_mut() else { return };
    let now = seen(c);
    let answer = last_answer(c);
    let Some(t) = c.transcript.as_mut() else { return };
    let last = t.rows().saturating_sub(1);
    let land = match row {
        Some(r) => Some(r.min(last)),
        None if t.visited => None,
        None => Some(answer.and_then(|i| t.row_of_msg(i, 1)).unwrap_or(last)),
    };
    (t.keyed, t.visited, t.seen, t.by_wheel) = (true, true, now, by_wheel);
    let Some(d) = t.pane.doc_in_mut() else { return };
    let Some(g) = d.rows.clone() else { return };
    let (line, k) = g.place(shown.min(last));
    d.set_top(line);
    d.top_rows = (k > 0).then_some((line, k));
    if let Some(r) = land.and_then(|r| g.get(r)) {
        d.set_selection(Selection::point(r.start));
    }
    ed.session.at = At::Chat;
}

/// A press on the conversation's row `row` (counted as `Screen.chat_view`'s) at column `col` of its text: its view
/// mode, the keys its, the cursor on that character, as a click in the code or a run's output puts it — on a bar
/// or a hanging indent, the row's first; past its text, the cell after it (its line break, the space a wrap took),
/// else its last. In sight already: the rows under the pointer don't move. A row of the frame's own (what a run
/// waits on) or an empty conversation: false, nothing placed.
pub fn click(ed: &mut Editor, row: usize, col: usize) -> bool {
    if ed.session.chat.as_ref().is_none_or(|c| c.msgs.is_empty()) {
        return false;
    }
    let Some(s) = transcript(ed).filter(|t| row < t.rows()).and_then(|t| t.row(row)) else { return false };
    if !has_keys(ed) {
        come(ed, Some(row), None);
    }
    let Some(d) = transcript_mut(ed).and_then(|t| t.pane.doc_in_mut()) else { return false };
    let text = d.text.byte_slice(s.start..s.start + s.len).to_string();
    // (the character whose cells hold the column — display width, a wide one's second cell its own)
    let col = col.saturating_sub(s.x0);
    let (mut w, mut at) = (0, None);
    for (b, g) in unicode_segmentation::UnicodeSegmentation::grapheme_indices(text.as_str(), true) {
        w += g.width();
        if w > col {
            at = Some(s.start + b);
            break;
        }
    }
    let end = s.start + s.len;
    let pos = at.or(s.end).unwrap_or_else(|| crate::graphemes::prev_boundary(&d.text, end).max(s.start));
    d.set_selection(Selection::point(crate::graphemes::snap(&d.text, pos)));
    true
}

/// After a frame (`term::render`, as `rundoc::resized`): the chat drawn at another size while its view has the
/// keys — the cursor kept in sight now, not at the next key. True when the rows in sight moved (drawn again).
pub fn resized(ed: &mut Editor) -> bool {
    if !has_keys(ed) {
        return false;
    }
    let size = viewport(ed);
    let Some(before) = transcript(ed).filter(|t| t.pane.size != Some(size)).map(Transcript::top_row) else {
        return false;
    };
    in_sight_now(ed);
    transcript(ed).map(Transcript::top_row) != Some(before)
}

/// The cursor's row in sight in the rows the chat shows, the scrolloff margin kept.
fn in_sight_now(ed: &mut Editor) {
    let size = viewport(ed);
    let so = ed.config.scrolloff;
    let Some(t) = transcript_mut(ed) else { return };
    t.pane.size = Some(size);
    if let Some(d) = t.pane.doc_in_mut() {
        in_sight(d, size.0, so);
    }
}

/// Ends its view mode: where you were kept (its selection, down to its cursor — the next view lands there), the
/// chat showing the rows you read — `Chat.scroll` from the bottom, what came while it held below them; a prompt
/// of its goes. The keys stay where they are — the caller says where they go.
pub fn leave(ed: &mut Editor) {
    let (area, _) = viewport(ed);
    // (the chat's scroll counts from the live bottom: every row the last frame laid out, its own included)
    let total: usize = ed.screen.chat_laid.borrow().iter().map(|(_, r)| r.len()).sum();
    let Some(c) = ed.session.chat.as_mut() else { return };
    let Some(t) = c.transcript.as_mut().filter(|t| t.keyed && t.pane.doc_in().is_some()) else { return };
    t.keyed = false;
    t.by_wheel = None;
    c.scroll = total.max(t.rows()).saturating_sub(t.top_row() + area);
    if t.pane.dropped() {
        ed.prompt = None;
    }
}

/// Ends its view mode and keeps nothing of it: the conversation is another (`C-l` a new one, `C-p` an earlier one)
/// — where you were was the one that went.
pub fn forget(ed: &mut Editor) {
    leave(ed);
    if let Some(t) = transcript_mut(ed) {
        t.visited = false;
    }
}

/// Every key and event (`view::sync`): its view mode ends once the chat lost the keys (a click elsewhere, the
/// chat closed or minimized, `C-w h`) — never inside a key's command (its document swapped in).
pub fn sync_keys(ed: &mut Editor) {
    if keyed(ed) && !has_keys(ed) {
        leave(ed);
    }
}

/// Caught up: the cursor on the latest line, at the bottom of what's in sight, what came taken in.
fn catch_up(ed: &mut Editor) {
    let (area, _) = viewport(ed);
    let Some(c) = ed.session.chat.as_mut() else { return };
    let now = seen(c);
    let Some(t) = c.transcript.as_mut() else { return };
    t.seen = now;
    let rows = t.rows();
    let Some(d) = t.pane.doc_in_mut() else { return };
    let Some(g) = d.rows.clone() else { return };
    let last = mv::last_line(&d.text);
    d.set_selection(Selection::point(mv::line_start(&d.text, last)));
    let (line, k) = g.place(rows.saturating_sub(area));
    d.set_top(line);
    d.top_rows = (k > 0).then_some((line, k));
    in_sight_now(ed);
}

/// Runs `f` with the transcript swapped in (`docpane::swap`): an edit or typing put back is said (it's
/// read-only), the cursor kept in sight after.
fn swap<R>(ed: &mut Editor, f: impl FnOnce(&mut Editor) -> R) -> Option<R> {
    let size = viewport(ed);
    let (r, done) = crate::docpane::swap(ed, Which::Chat, size, f)?;
    if done.refused() {
        let bound = |name, like| crate::view::bound(ed, Surface::Chat, name, like);
        let ways: Vec<String> = [("view_input", "i", "types"), ("view_leave", "esc", "code")]
            .iter()
            .filter_map(|(cmd, like, what)| Some(format!("{} {what}", bound(cmd, like)?)))
            .collect();
        let out = if ways.is_empty() { String::new() } else { format!(" — {}", ways.join(", ")) };
        ed.set_status(format!("the conversation is read-only{out}"));
    }
    in_sight_now(ed);
    Some(r)
}

/// A command a key of its view mode runs on the editor, the document put back first: `space l`, `space I`,
/// `space ?` (`view::EDITORS`), the window table's, a `:` line bound to a key — they act on the session, never on
/// the text in sight.
fn editors(ed: &Editor, c: &MappableCommand) -> bool {
    match c {
        MappableCommand::Static(s) => {
            crate::view::EDITORS.contains(&s.name) || ed.keymaps.in_window_table(s.name)
        }
        MappableCommand::Typed(_) => true,
    }
}

/// A key while its view mode has them: its prompt's (`/`), else the chat's table — normal mode's with `[view]`
/// and `[view.chat]` over it — on its document. `C-w`, `:`, the chat's chords and a sequence of the editor's under
/// way go on. Returns the handler that took it.
pub fn key(ed: &mut Editor, key: Key) -> Option<crate::uistate::Handler> {
    use crate::uistate::Handler;
    if !has_keys(ed) || !ed.pending.is_empty() {
        return None;
    }
    let t = transcript(ed)?;
    t.pane.doc_in()?;
    if ed.prompt.is_some() {
        if !t.pane.prompt {
            return None;
        }
        swap(ed, |ed| ed.handle_prompt_key(key));
        return Some(Handler::Prompt);
    }
    if t.pane.keys.idle() && (passes_key(key) || chat_chord(ed, key)) {
        return None;
    }
    swap(ed, |ed| {
        crate::docpane::key_in(
            ed,
            key,
            |ed, mode, keys| ed.keymaps.view_lookup(Surface::Chat, mode, keys),
            editors,
        )
    });
    after(ed);
    Some(Handler::ChatView)
}

/// What a command asked for, once its key is done.
fn after(ed: &mut Editor) {
    let Some(t) = transcript_mut(ed) else { return };
    let Some((a, deferred)) = t.pane.take_after() else { return };
    match a {
        After::Leave => {
            leave(ed);
            ed.session.leave(At::Chat);
        }
        // (the input: the keys stay the chat's)
        After::Input | After::Enter => leave(ed),
        After::Latest => catch_up(ed),
        After::Take => take(ed),
        After::ChatView => ed.set_status(
            "this is the conversation, read-only — select something, then enter or space > quotes it",
        ),
        After::Editor => ed.run(&deferred, None),
    }
}

/// `enter` (`space >`) on a selection: each range a quote over the chat's input (`quote.rs`) — the text, a
/// paragraph whole however it wrapped — the view left, the input with the keys.
fn take(ed: &mut Editor) {
    use crate::chat::Role;
    use crate::quote::{Taken, lines, text_of};
    let Some(c) = ed.session.chat.as_ref() else { return };
    let Some(t) = c.transcript.as_ref() else { return };
    let Some(d) = t.pane.doc_in() else { return };
    let quote = |r: &Range| {
        let (text, l0, l1) = text_of(&d.text, (r.from(), r.to()), &|_| (0, None), &|_| None);
        // (blank lines around it — the air between messages — aren't what was meant)
        let text = text.trim_start_matches('\n').trim_end().to_string();
        if text.trim().is_empty() {
            return None;
        }
        let n = lines(text.lines().count());
        // (whose words: one message's role, else the chat's)
        let role = |l: usize| t.msg_at(l).and_then(|m| Some(c.msgs.get(m)?.role));
        let (role, label) = match (role(l0), role(l1)) {
            (Some(a), Some(b)) if a == b => match a {
                Role::User => ("user", "your message"),
                Role::Assistant => ("assistant", "Claude's answer"),
                Role::Thought => ("thinking", "Claude's thinking"),
                Role::Tool => ("tool calls", "Claude's tool calls"),
                Role::Note => ("note", "the chat"),
            },
            _ => ("several", "the chat"),
        };
        Some(Taken {
            attrs: vec![("from", "chat".into()), ("role", role.into())],
            label: format!("{label} {n}"),
            text,
            redact: false,
            code: None,
        })
    };
    // (only the ranges wider than a character: a plain cursor beside them quotes nothing)
    let wide = |r: &&Range| crate::graphemes::next_boundary(&d.text, r.from()) < r.to();
    let taken: Vec<Taken> = d.selection().ranges().iter().filter(wide).filter_map(quote).collect();
    if taken.is_empty() {
        return ed.set_status("only blanks are selected — nothing to quote");
    }
    // (the keys the chat's input's: `quote::add` focuses it)
    leave(ed);
    crate::quote::add(ed, taken);
}

/// Whether `enter` quotes in its view mode now: something selected.
pub fn quotes(ed: &Editor) -> bool {
    has_keys(ed) && transcript(ed).and_then(|t| t.pane.doc_in()).is_some_and(crate::docpane::wide)
}

/// Before drawing: the matches its search shows — the pattern its `/` prompt is typing, else its last search
/// while shown.
pub fn refresh_search(ed: &mut Editor) {
    let typed = ed.prompt.as_ref().filter(|p| matches!(p.kind, crate::editor::PromptKind::Search { .. }));
    let typed = typed.map(|p| p.text.clone());
    if !has_keys(ed) {
        return;
    }
    if let Some(t) = transcript_mut(ed) {
        t.pane.refresh_search(typed);
    }
}

/// `esc` with its keys: its matches no longer shown (the code's stay as they are).
pub fn search_unshown(ed: &mut Editor) {
    if let Some(t) = transcript_mut(ed) {
        t.pane.keys.search_hl = false;
    }
}

/// The wheel over the conversation: up, its view mode at that point — the cursor on `row` (the row under the
/// pointer, of all the chat's), the keys its — then three rows; back down to the end, by the wheel, nothing
/// selected, the keys go back where they were. Without the keys, down scrolls the chat. An empty conversation:
/// nothing to read, nothing done.
pub fn wheel(ed: &mut Editor, row: Option<usize>, up: bool) {
    if ed.session.chat.as_ref().is_none_or(|c| c.msgs.is_empty() || c.transcript.is_none()) {
        return;
    }
    if !has_keys(ed) {
        if !up {
            if let Some(c) = ed.session.chat.as_mut() {
                c.scroll = c.scroll.saturating_sub(3);
            }
            return;
        }
        let was = crate::view::was(ed);
        enter(ed, row, Some(was));
    }
    ed.session.at = At::Chat;
    // (three rows — `scroll_view` counts the transcript's rows, a paragraph's each its own)
    swap(ed, |ed| ed.scroll_view(if up { -3 } else { 3 }, false));
    let (area, _) = viewport(ed);
    let Some(t) = transcript(ed) else { return };
    let at_end = t.top_row() + area >= t.rows();
    if !up
        && at_end
        && !t.pane.selects()
        && !t.pane.prompt
        && let Some(was) = t.by_wheel
    {
        leave(ed);
        crate::view::give_back(ed, was);
    }
}

/// The state's `view` while its view mode has the keys: the cursor's line, selections, whether more came since —
/// shapes, never text.
pub fn state(ed: &Editor) -> Option<serde_json::Value> {
    if !has_keys(ed) {
        return None;
    }
    let t = transcript(ed)?;
    let d = t.pane.doc_in()?;
    let line = mv::line_of(&d.text, d.selection().primary().cursor(&d.text));
    Some(serde_json::json!({
        "surface": Surface::Chat.name(),
        "line": line + 1,
        "lines": t.pane.lines(),
        "selections": d.selection().ranges().len(),
        "selects": t.pane.selects(),
        "mode": if t.pane.keys.mode == Mode::Select { "select" } else { "normal" },
        "held": came(ed),
    }))
}

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) mod view_tests;
