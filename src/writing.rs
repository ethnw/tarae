//! 글타래 S2 — live edits: what Claude writes into a file shown as it's written. Its tool call's input streams
//! (`input_json_delta`, read on the reader thread a piece at a time — `scan.rs`); once an item's `old_string` has
//! closed and is in the file once (searched on a worker, as `edittool` will look), the `new_string` so far is
//! drawn there as rows of their own (`Shown`, `OtherKind::Writing`): the lines it takes out washed, the ones it
//! writes on the proposed wash, its head bleeding (S1's 번짐). Nothing is applied or decided and no key is taken — when
//! the call is whole the review opens on the same rows (the same diff of the same bytes); Claude's write landing
//! unasked moves the buffer on and the rows go, follow's shown edit taking their place.
//!
//! Frames: the reader batches the input as it batches text (`llm::FLUSH`); what came is shown at the stream's own
//! pace (`Pace`, `reveal`) — a frame every `REVEAL_EVERY` only while there's text left to show.

pub mod scan;

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ropey::Rope;
pub use scan::{Field, Piece, Scan, Tool};

use crate::codeview::{Edit, Mark, OtherKind, Row};
use crate::document::DocId;
use crate::editor::Editor;

/// What of a tool call's input came since the last batch (the reader thread's) — `done` once its block ended.
pub struct Delta {
    pub call: String,
    pub tool: Tool,
    pub pieces: Vec<Piece>,
    pub done: bool,
}

/// The reader thread's (`llm::pump`): the tool calls whose input is streaming, by content block, and what
/// came of them that the next batch sends.
#[derive(Default)]
pub struct Reading {
    blocks: Vec<(u64, String, Tool, Scan)>,
    ready: Vec<Delta>,
}

impl Reading {
    /// A line of the stream — whether it was a drawn tool input's (its block's start, a piece of it, its end).
    pub fn line(&mut self, ev: &serde_json::Value) -> bool {
        if ev["type"] != "stream_event" {
            return false;
        }
        let e = &ev["event"];
        let index = e["index"].as_u64();
        match e["type"].as_str() {
            // (a message's blocks are numbered from 0)
            Some("message_start") => {
                self.blocks.clear();
                false
            }
            Some("content_block_start") => {
                let b = &e["content_block"];
                let (Some(i), Some(tool)) = (index, b["name"].as_str().and_then(Tool::of)) else {
                    return false;
                };
                if b["type"] != "tool_use" {
                    return false;
                }
                let id = b["id"].as_str().unwrap_or_default().to_string();
                self.blocks.push((i, id, tool, Scan::new(tool)));
                true
            }
            Some("content_block_delta") if e["delta"]["type"] == "input_json_delta" => {
                let Some(b) = self.blocks.iter_mut().find(|b| Some(b.0) == index) else { return false };
                b.3.feed(e["delta"]["partial_json"].as_str().unwrap_or_default());
                let (call, tool, pieces) = (b.1.clone(), b.2, b.3.take());
                self.add(call, tool, pieces, false);
                true
            }
            Some("content_block_stop") => {
                let Some(k) = self.blocks.iter().position(|b| Some(b.0) == index) else { return false };
                let (_, call, tool, mut scan) = self.blocks.remove(k);
                self.add(call, tool, scan.take(), true);
                true
            }
            _ => false,
        }
    }

    fn add(&mut self, call: String, tool: Tool, pieces: Vec<Piece>, done: bool) {
        if pieces.is_empty() && !done {
            return;
        }
        let Some(d) = self.ready.iter_mut().find(|d| d.call == call) else {
            return self.ready.push(Delta { call, tool, pieces, done });
        };
        d.done |= done;
        for p in pieces {
            // (a field's text carried on: one piece)
            if let (
                Some(Piece::Text { item, field, add, closed }),
                Piece::Text { item: i, field: f, add: more, closed: c },
            ) = (d.pieces.last_mut(), &p)
                && !*closed
                && (*item, *field) == (*i, *f)
            {
                add.push_str(more);
                *closed = *c;
                continue;
            }
            d.pieces.push(p);
        }
    }

    /// Whether there's something for the next batch to send.
    pub fn holds(&self) -> bool {
        !self.ready.is_empty()
    }

    /// What the next batch sends.
    pub fn take(&mut self) -> Vec<Delta> {
        std::mem::take(&mut self.ready)
    }
}

/// A chat's edits being written (`Chat.writing`): each call's items as far as they came, and where each goes.
pub struct Live {
    pub calls: Vec<Call>,
    /// What came is shown at the stream's pace (`reveal`) — off in tests (an animation: shown as it comes) but where
    /// a test sets it.
    pub paced: bool,
}

impl Default for Live {
    fn default() -> Live {
        Live { calls: Vec::new(), paced: !cfg!(test) }
    }
}

/// How often the reveal moves while there's written text left to show: 40 frames a second — at the measured
/// 300 characters a second (`cargo xtask stream --edit`), about 8 characters a frame.
pub const REVEAL_EVERY: Duration = Duration::from_millis(25);

/// Pieces closer than this are one: the reader sends a lump's first delta at once and the rest `llm::FLUSH` (25 ms)
/// after it — measured, claude streams an edit's input as 12 deltas at once (~190 characters) every ~640 ms.
const BURST: Duration = Duration::from_millis(40);

/// The gap to the next piece while only one has come (measured: an input's first deltas come singly ~50 ms apart,
/// the text's median gap is 48 ms).
const FIRST_GAP: Duration = Duration::from_millis(50);

/// The longest gap the pace goes by: a pause (Claude thinking mid-edit) doesn't slow what comes after it past this
/// (measured: the lumps' gaps 560–690 ms).
const GAP_MAX: Duration = Duration::from_millis(1000);

/// What's left to show once the text is whole shows within this.
pub const WHOLE: Duration = Duration::from_millis(150);

/// The gaps the pace is worked out from — their median, so one pause doesn't set it.
const GAPS: usize = 3;

/// How far an item's new text is shown, and the pace it goes at: pieces come in lumps, shown as typed at the stream's
/// own speed — what came by about when the next piece is due (the last few gaps' median after the last piece).
#[derive(Default)]
pub struct Pace {
    /// Bytes of the text shown (a character's boundary).
    pub shown: usize,
    /// When the last few pieces came (pieces within `BURST` are one).
    came: VecDeque<Instant>,
    /// The reveal's last move: where it goes on from.
    stepped: Option<Instant>,
    /// When the text came whole.
    whole: Option<Instant>,
}

impl Pace {
    /// A piece came at `now` onto a text `before` bytes long.
    pub fn came(&mut self, now: Instant, before: usize) {
        // (all of it was shown: the reveal goes on from now, not from its last move)
        if self.shown >= before {
            self.stepped = Some(now);
        }
        if self.came.back().is_none_or(|&b| now.saturating_duration_since(b) >= BURST) {
            self.came.push_back(now);
            if self.came.len() > GAPS + 1 {
                self.came.pop_front();
            }
        }
    }

    /// The text came whole at `now`.
    pub fn closed(&mut self, now: Instant) {
        self.whole.get_or_insert(now);
    }

    /// When all that came should be shown: the next piece's time, from the last few gaps — sooner once it's whole.
    fn due(&self) -> Option<Instant> {
        let last = *self.came.back()?;
        let mut gaps: Vec<Duration> = self
            .came
            .iter()
            .zip(self.came.iter().skip(1))
            .map(|(a, b)| b.saturating_duration_since(*a))
            .collect();
        crate::sorting::sort(&mut gaps);
        let gap = gaps.get(gaps.len() / 2).copied().unwrap_or(FIRST_GAP).clamp(REVEAL_EVERY, GAP_MAX);
        let due = last + gap;
        Some(self.whole.map_or(due, |w| due.min(w + WHOLE)))
    }

    /// The reveal moved on to `now` over `text` (what came so far): whether it moved. Evenly from its last move to
    /// when it's due; past that, all of it.
    pub fn step(&mut self, text: &str, now: Instant) -> bool {
        let total = text.len();
        if self.shown >= total {
            return false;
        }
        let from = self.stepped.unwrap_or(now);
        let to = match self.due() {
            Some(due) if due > now && due > from => {
                let part = now.saturating_duration_since(from).as_secs_f64() / (due - from).as_secs_f64();
                self.shown + ((total - self.shown) as f64 * part).ceil() as usize
            }
            _ => total,
        };
        self.stepped = Some(now);
        let to = cluster_end(text, to.min(total));
        let moved = to > self.shown;
        self.shown = to.max(self.shown);
        moved
    }

    /// Whether there's text left to show.
    pub fn behind(&self, text: &str) -> bool {
        self.shown < text.len()
    }
}

/// The first character's end in `text` at or after byte `at` — a grapheme cluster's (a character is one: never half
/// of one shown).
fn cluster_end(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at += 1;
    }
    let mut c = unicode_segmentation::GraphemeCursor::new(at, text.len(), true);
    match c.is_boundary(text, 0) {
        Ok(false) => c.next_boundary(text, 0).ok().flatten().unwrap_or(text.len()),
        _ => at,
    }
}

pub struct Call {
    pub id: String,
    pub tool: Tool,
    pub items: Vec<Item>,
    /// Its input is whole (the block ended).
    pub done: bool,
    /// The files a review opened on once its input was whole (real paths): an item's place found there later is
    /// the review's — never drawn, the review answered or not.
    pub reviewed: Vec<PathBuf>,
}

#[derive(Default)]
pub struct Item {
    pub file: Text,
    pub old: Text,
    pub new: Text,
    /// `move_to` or `delete`: not a change of text — nothing to draw.
    pub other: bool,
    pub place: Place,
    /// Follow led the view on it at its last piece (`sync`): watched as it was written to where it ends
    /// (`written_end`).
    pub followed: bool,
    /// Its text whole and drawn: `followed` stays as it was then (a `C-f` after, another call's piece, change nothing).
    ended: bool,
    /// How much of `new` is shown — at the stream's pace (`reveal`): the rows are laid out from that much.
    pub pace: Pace,
    /// Its block as last laid out, and what for: the buffer's version, the new text's length, whether whole —
    /// laid out again only when one of them moved (a finished item costs a batch nothing).
    drawn: Option<((u64, usize, bool), Arc<Block>)>,
}

#[derive(Default)]
pub struct Text {
    pub s: String,
    pub closed: bool,
}

#[derive(Default)]
pub enum Place {
    /// Not asked yet: its file or `old_string` still coming.
    #[default]
    Unknown,
    Looking,
    Found(Found),
    /// Not drawn: in no file of the project, in none or more than one place, or you edited what it replaces since
    /// (an edit elsewhere in the buffer keeps it, found again: `relocate`).
    Nowhere,
}

pub struct Found {
    /// Its real path.
    pub path: PathBuf,
    /// What it replaces, in the text it was found in.
    pub from: usize,
    pub to: usize,
    /// The open buffer it was found in, at that version — None: read from the disk.
    pub seen: Option<(DocId, u64)>,
    /// The buffer it's drawn in, at the version it holds what it replaces.
    pub bound: Option<(DocId, u64)>,
    /// What it replaces as the buffer has it (bound): found again after an edit elsewhere (`relocate`).
    pub held: String,
    /// The lines it replaces (first, last).
    pub lines: (usize, usize),
    /// A name Claude's runs can't write (the review warns of it, a row under its title).
    pub protected: bool,
}

impl Item {
    /// Its place can be looked for: a file and what it replaces (all of it for Write).
    fn askable(&self, tool: Tool) -> bool {
        self.file.closed && !self.other && (tool == Tool::Write || self.old.closed)
    }
}

/// An item's rows in the buffer drawn with it (`Document.writing`, by `term::rows_pane`): for the version the
/// rows hold, each block over the lines it replaces.
pub struct Shown {
    pub version: u64,
    pub blocks: Vec<Arc<Block>>,
}

/// One item drawn: lines `first..=last` shown as `rows` — the document's own lines it keeps or takes out
/// (`Row::Text`, a `Taken` mark), the ones it writes (`OtherKind::Writing`), as the review lays out its change.
pub struct Block {
    pub first: usize,
    pub last: usize,
    pub rows: Vec<Row>,
    /// The row it's writing now (None once its text is whole) — kept in sight (`term::scroll_to`).
    pub head: Option<usize>,
    /// A name Claude's runs can't write — the review's warning row, counted where it will open.
    pub protected: bool,
    /// Its rows' screen rows as last counted, and for what (soft-wrap width, tab width): `counts[k]` the screen
    /// rows before row `k`, its last the block's — counted once a width, not on every frame and key (a Write over
    /// a big file is thousands of rows).
    counts: std::sync::Mutex<Option<(Measure, Arc<Vec<usize>>)>>,
    /// The counts of the rows it shares with the block laid out before it (`after`), not counted again.
    seed: std::sync::Mutex<Option<(Measure, Vec<usize>)>>,
}

/// What a block's rows are counted at: the soft-wrap width (None: no wrap), the tab width.
type Measure = (Option<usize>, usize);

impl Block {
    /// The screen rows before each of its rows, at `wrap` (`counts`), its document `text` the one it was laid out
    /// for.
    pub fn counts(&self, text: &Rope, wrap: Option<usize>, tab: usize) -> Arc<Vec<usize>> {
        let mut kept = self.counts.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((key, counts)) = kept.as_ref()
            && *key == (wrap, tab)
        {
            return counts.clone();
        }
        let mut counts = Vec::with_capacity(self.rows.len() + 1);
        // (the rows it shares with the block it was laid out after, counted then: as the head moves only the end
        // of a big Write is counted again)
        let seed = self.seed.lock().unwrap_or_else(|e| e.into_inner()).take();
        match seed.filter(|(key, _)| *key == (wrap, tab)) {
            Some((_, prefix)) => counts.extend_from_slice(&prefix),
            None => counts.push(0),
        }
        let mut n = counts.last().copied().unwrap_or(0);
        for row in &self.rows[counts.len() - 1..] {
            n += parts_of(row, text, wrap, tab).len();
            counts.push(n);
        }
        let counts = Arc::new(counts);
        *kept = Some(((wrap, tab), counts.clone()));
        counts
    }

    /// Its screen rows from the `skip`th on, at most `max` of them — each row wrapped as the pane wraps a line, only
    /// those drawn worked out.
    pub fn screen_rows(
        &self,
        text: &Rope,
        (wrap, tab): (Option<usize>, usize),
        skip: usize,
        max: usize,
    ) -> Vec<Row> {
        let counts = self.counts(text, wrap, tab);
        let k = counts.partition_point(|&c| c <= skip).saturating_sub(1);
        let mut out = Vec::with_capacity(max.min(self.rows.len()));
        let mut drop = skip - counts[k];
        for row in self.rows.iter().skip(k) {
            for p in parts_of(row, text, wrap, tab) {
                if out.len() == max {
                    return out;
                }
                match drop {
                    0 => out.push(row.with_part(p)),
                    _ => drop -= 1,
                }
            }
        }
        out
    }
}

impl Block {
    /// The line it rewrites — the first it takes out; for one that only adds lines, the line its new rows stand on
    /// (they're drawn above it): where follow puts the cursor (`follow::writing_at`), the change's first line once
    /// it lands, so the cursor doesn't move then.
    pub fn place(&self) -> usize {
        let taken = self.rows.iter().find_map(|r| match r {
            Row::Text { line, edit: Some(Edit { mark: Mark::Taken, .. }), .. } => Some(*line),
            _ => None,
        });
        let added = || self.rows.iter().find_map(|r| matches!(r, Row::Other { .. }).then(|| r.line()));
        taken.or_else(added).unwrap_or(self.first)
    }

    /// Laid out after `before` (the same item, the same version, more text): the counts of the rows they share at
    /// the start are `before`'s, as it last counted them.
    fn after(self, before: &Block) -> Block {
        let same = |a: &Row, b: &Row| match (a, b) {
            (Row::Text { line: x, edit: e, .. }, Row::Text { line: y, edit: f, .. }) => x == y && e == f,
            (Row::Other { at: x, text: s, kind: k, .. }, Row::Other { at: y, text: t, kind: l, .. }) => {
                x == y && s == t && k == l
            }
            _ => false,
        };
        let shared = self.rows.iter().zip(&before.rows).take_while(|(a, b)| same(a, b)).count();
        if let Some((key, counts)) = before.counts.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
            *self.seed.lock().unwrap_or_else(|e| e.into_inner()) = Some((*key, counts[..=shared].to_vec()));
        }
        self
    }
}

/// A row's soft-wrapped parts, as the pane lays out a line or a text that isn't the document's.
fn parts_of(row: &Row, text: &Rope, wrap: Option<usize>, tab: usize) -> Vec<Option<crate::codeview::Part>> {
    match row {
        Row::Text { line, .. } => crate::codeview::text_parts(text, *line, wrap, tab),
        Row::Other { text, .. } => crate::codeview::other_parts(text, wrap, tab),
        _ => vec![None],
    }
}

/// What's being written in `doc` as it is now (rows laid out for another version aren't drawn).
pub fn shown(doc: &crate::document::Document) -> Option<&Shown> {
    doc.writing.as_deref().filter(|s| s.version == doc.version())
}

impl Shown {
    /// The block holding line `l`.
    pub fn at(&self, l: usize) -> Option<&Block> {
        let i = self.blocks.partition_point(|b| b.last < l);
        self.blocks.get(i).map(|b| &**b).filter(|b| b.first <= l)
    }
}

/// Past this many bytes (what it replaces and what it writes) the rows aren't diffed: the lines it replaces taken
/// out, then the ones it writes — the review shows its own diff.
const DIFF_MAX: usize = 256 * 1024;

/// … and while it's still being written, laid out again every batch on the main loop: past this the diff waits for
/// the whole text (a Write over a big file — its rows the plain layout until then).
const DIFF_WRITING_MAX: usize = 32 * 1024;

/// Pieces of chat `generation`'s edit call: kept, its places looked for once they can be, its rows drawn.
pub fn on_delta(ed: &mut Editor, d: Delta) {
    let Some(c) = ed.session.chat.as_mut() else { return };
    let live = &mut c.writing;
    let k = match live.calls.iter().position(|c| c.id == d.call) {
        Some(k) => k,
        None => {
            let call = Call {
                id: d.call.clone(),
                tool: d.tool,
                items: Vec::new(),
                done: false,
                reviewed: Vec::new(),
            };
            live.calls.push(call);
            live.calls.len() - 1
        }
    };
    let (paced, now) = (live.paced, crate::clock::now());
    let call = &mut live.calls[k];
    call.done |= d.done;
    for p in d.pieces {
        let (Piece::Text { item, .. } | Piece::Flag { item, .. }) = p;
        // (an item past `edittool`'s most isn't one it will make)
        if item >= crate::edittool::ITEMS_MAX {
            continue;
        }
        if call.items.len() <= item {
            call.items.resize_with(item + 1, Item::default);
        }
        let it = &mut call.items[item];
        match p {
            Piece::Text { field, add, closed, .. } => {
                let t = match field {
                    Field::File => &mut it.file,
                    Field::Old => &mut it.old,
                    Field::New => &mut it.new,
                    Field::Other => {
                        it.other = true;
                        continue;
                    }
                    Field::All => continue,
                };
                let before = t.s.len();
                t.s.push_str(&add);
                t.closed |= closed;
                // (the text it writes: shown at the stream's pace — from when each piece came)
                if matches!(field, Field::New) {
                    if !add.is_empty() {
                        it.pace.came(now, before);
                    }
                    if closed {
                        it.pace.closed(now);
                    }
                    if !paced {
                        it.pace.shown = it.new.s.len();
                    }
                }
            }
            Piece::Flag { field: Field::Other, on: true, .. } => it.other = true,
            Piece::Flag { .. } => {}
        }
    }
    let tool = call.tool;
    let ask: Vec<(usize, String, String)> = call
        .items
        .iter_mut()
        .enumerate()
        .filter(|(_, it)| matches!(it.place, Place::Unknown) && it.askable(tool))
        .map(|(i, it)| {
            it.place = Place::Looking;
            (i, it.file.s.clone(), it.old.s.clone())
        })
        .collect();
    for (i, file, old) in ask {
        look(ed, d.call.clone(), i, tool, file, old);
    }
    sync(ed);
}

/// The first and last lines of `from..to` in `whole`: the last is its last byte's (`to - 1`, a byte that may sit
/// inside a character — a region ending in `—` — so the newlines before it are counted as bytes, never by
/// slicing the text there; a `'\n'` is one byte and never inside another character).
fn lines_of(whole: &str, from: usize, to: usize) -> (usize, usize) {
    let line = |b: usize| whole.as_bytes()[..b].iter().filter(|&&c| c == b'\n').count();
    (line(from), line(to.saturating_sub(1).max(from)))
}

/// Where item `i` of `call` goes, worked out on a worker: its file in the project (not one named like a key
/// Claude can't read), its text as the user has it, `old` in it once.
fn look(ed: &mut Editor, call: String, i: usize, tool: Tool, file: String, old: String) {
    let root = crate::nav::project_root(ed);
    let keys = crate::reads::Keys::of(ed);
    let open: Vec<(PathBuf, DocId, u64, Rope)> = ed
        .docs
        .iter()
        .filter(|d| !d.loading)
        .filter_map(|d| Some((d.path.clone()?, d.id, d.version(), d.text.clone())))
        .collect();
    let generation = ed.session.chat.as_ref().map(crate::chat::Chat::generation);
    ed.events.jobs().spawn(move || {
        let found = (|| {
            let real_root = std::fs::canonicalize(&root).ok()?;
            let (path, _, protected) = crate::edittool::place_in(&root, &keys, &real_root, &file).ok()?;
            let (text, seen) = match open.into_iter().find(|o| o.0 == path) {
                Some((_, id, version, text)) => (Cow::Rope(text), Some((id, version))),
                None => (Cow::Disk(String::from_utf8(std::fs::read(&path).ok()?).ok()?), None),
            };
            let whole = text.string();
            let (from, to) = match tool {
                Tool::Write => (0, whole.len()),
                _ => crate::edittool::one_place(&whole, &old)?,
            };
            let lines = lines_of(&whole, from, to);
            Some(Found { path, from, to, seen, bound: None, held: String::new(), lines, protected })
        })();
        move |ed: &mut Editor| {
            let Some(c) = ed.session.chat.as_mut().filter(|c| Some(c.generation()) == generation) else {
                return;
            };
            let Some(call) = c.writing.calls.iter_mut().find(|c| c.id == call) else { return };
            // (a review opened on that file once the call was whole: the change is the review's, drawn or not)
            let found = found.filter(|f| !call.reviewed.contains(&f.path));
            let Some(it) = call.items.get_mut(i) else { return };
            if !matches!(it.place, Place::Looking) {
                return;
            }
            let at = found.as_ref().map(|f| (f.path.clone(), f.lines));
            it.place = found.map_or(Place::Nowhere, Place::Found);
            // Follow takes the editor there — the file opened for it, if it isn't (`follow::writing`)
            if let Some((p, (a, b))) = at
                && !ed.docs.iter().any(|d| d.path.as_ref() == Some(&p))
            {
                crate::follow::writing(ed, &p, a..b + 1);
            }
            sync(ed);
        }
    });
}

/// A text as `look` read it: an open buffer's or the disk's.
enum Cow {
    Rope(Rope),
    Disk(String),
}

impl Cow {
    fn string(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Cow::Rope(r) => std::borrow::Cow::Owned(r.to_string()),
            Cow::Disk(s) => std::borrow::Cow::Borrowed(s),
        }
    }
}

/// The rows each buffer draws, from the items as they stand: an item found is bound to the buffer that holds
/// what it replaces (follow taken there the first time), unbound for good once that buffer changed or a review
/// opened on it; each buffer's blocks laid out again.
pub fn sync(ed: &mut Editor) {
    let review = ed.session.review.as_ref().map(|r| r.doc_id);
    let mut bound_now: Vec<(PathBuf, usize, usize)> = Vec::new();
    let mut want: Vec<(DocId, Arc<Block>)> = Vec::new();
    let others: Vec<DocId> = ed.sessions.iter().flat_map(|l| l.docs.iter().copied()).collect();
    if let Some(c) = ed.session.chat.as_mut() {
        for call in &mut c.writing.calls {
            for it in &mut call.items {
                let Place::Found(f) = &mut it.place else { continue };
                if f.bound.is_none() {
                    let doc = ed
                        .docs
                        .iter()
                        .find(|d| d.path.as_ref() == Some(&f.path) && !d.loading && !others.contains(&d.id));
                    let holds = doc.filter(|d| match f.seen {
                        Some(seen) => seen == (d.id, d.version()),
                        // (read from the disk: the buffer opened since, unchanged, holds it there)
                        None => {
                            !d.is_modified()
                                && match call.tool {
                                    Tool::Write => d.text.len_bytes() == f.to,
                                    _ => d
                                        .text
                                        .get_byte_slice(f.from..f.to)
                                        .is_some_and(|s| s == it.old.s.as_str()),
                                }
                        }
                    });
                    if let Some(d) = holds {
                        f.bound = Some((d.id, d.version()));
                        f.held = d.text.byte_slice(f.from..f.to).to_string();
                        bound_now.push((f.path.clone(), f.lines.0, f.lines.1));
                    }
                }
                let Some((id, mut version)) = f.bound else { continue };
                let doc = ed.docs.iter().find(|d| d.id == id);
                let Some(doc) = doc.filter(|_| review != Some(id)) else {
                    it.place = Place::Nowhere;
                    continue;
                };
                // The buffer changed: an edit elsewhere keeps the rows where what they replace is now, one inside
                // it (a Write's is the whole file) ends them
                if doc.version() != version {
                    // (what it writes is there now: the change landed — Claude's write unasked, its file reloaded;
                    // what it replaced may still be read inside it, an edit that adds lines)
                    let end = f.from + it.new.s.len();
                    let landed = it.new.closed
                        && doc.text.get_byte_slice(f.from..end).is_some_and(|s| s == it.new.s.as_str());
                    let found = relocate(&doc.text, f).filter(|_| call.tool != Tool::Write && !landed);
                    let Some((from, to)) = found else {
                        it.place = Place::Nowhere;
                        continue;
                    };
                    let line = |b: usize| doc.text.byte_to_line(b);
                    (f.from, f.to, f.lines) = (from, to, (line(from), line(to.saturating_sub(1).max(from))));
                    version = doc.version();
                    f.bound = Some((id, version));
                }
                // (as much of it as is shown: whole once all of it is)
                let shown = &it.new.s[..it.pace.shown.min(it.new.s.len())];
                let whole = it.new.closed && shown.len() == it.new.s.len();
                let key = (version, shown.len(), whole);
                let b = match &it.drawn {
                    Some((k, b)) if *k == key => b.clone(),
                    before => {
                        let mut b = block(&doc.text, (f.from, f.to), shown, whole);
                        b.protected = f.protected;
                        let b = match before.as_ref().filter(|(k, _)| k.0 == version) {
                            Some((_, before)) => b.after(before),
                            None => b,
                        };
                        let b = Arc::new(b);
                        it.drawn = Some((key, b.clone()));
                        b
                    }
                };
                want.push((id, b));
            }
        }
    }
    // (the session's own buffers only: another's rows are its chat's)
    for d in ed.docs.iter_mut().filter(|d| !others.contains(&d.id)) {
        let mut blocks: Vec<Arc<Block>> = Vec::new();
        for (_, b) in want.extract_if(.., |w| w.0 == d.id) {
            blocks.push(b);
        }
        crate::sorting::by_key(&mut blocks, |b| b.first);
        // (two items over the same lines: the first drawn)
        blocks.dedup_by(|b, a| b.first <= a.last);
        let shown = (!blocks.is_empty()).then(|| Arc::new(Shown { version: d.version(), blocks }));
        if shown.is_some() || d.writing.is_some() {
            d.writing = shown;
        }
    }
    for (path, first, last) in bound_now {
        crate::follow::writing(ed, &path, first..last + 1);
    }
    // (a block elsewhere whose jump waited for PACE, first: the cursor goes there with it)
    crate::follow::writing_due(ed);
    // What's written in the document in front (the block being written, else the first): follow puts the cursor on the
    // line it rewrites — the editor taken onto its block first when the line being written went past the screen's
    // end (its block begun on screen, the cursor elsewhere)
    let block = shown(ed.doc()).and_then(|s| {
        let b = s.blocks.iter().find(|b| b.head.is_some()).or(s.blocks.first())?;
        Some((b.first..b.last + 1, b.place()))
    });
    if let (Some((lines, line)), Some(path)) = (block, ed.doc().path.clone()) {
        let past = past_sight(ed).is_some();
        crate::follow::writing_at(ed, &path, lines, line, past);
    }
    // Whether follow leads the view on what's drawn in front now — kept from the first time an item is drawn whole:
    // whether it was watched to its end (`written_end`)
    let (front, leads) = (ed.doc().id, crate::follow::view_line(ed).is_some());
    if let Some(c) = ed.session.chat.as_mut() {
        for it in c.writing.calls.iter_mut().flat_map(|c| &mut c.items).filter(|it| !it.ended) {
            let bound = match &it.place {
                Place::Found(f) => f.bound.map(|b| b.0),
                _ => None,
            };
            it.followed = leads && bound == Some(front);
            it.ended = it.new.closed && !it.pace.behind(&it.new.s) && bound.is_some();
        }
    }
}

/// The reveal moved on to now — after every event, a frame's tick (`reveal_wake`) among them: each item's text shown
/// as far as its pace has come, its rows laid out again where that moved (follow's caret and view with them) or where
/// its buffer changed since (an edit landed mid-reveal: its rows go, and the frames).
pub fn reveal(ed: &mut Editor) {
    let Some(c) = ed.session.chat.as_ref().filter(|c| c.writing.paced && !c.writing.calls.is_empty()) else {
        return;
    };
    let changed = c.writing.calls.iter().flat_map(|c| &c.items).any(|it| match &it.place {
        Place::Found(Found { bound: Some((id, v)), .. }) => {
            ed.docs.iter().any(|d| d.id == *id && d.version() != *v)
        }
        _ => false,
    });
    // (`editor.motion = "none"`: shown as it comes — what's left, at once)
    let still = ed.motion() == crate::motion::Motion::None;
    let Some(c) = ed.session.chat.as_mut() else { return };
    let now = crate::clock::now();
    let mut moved = changed;
    for it in c.writing.calls.iter_mut().flat_map(|c| &mut c.items) {
        moved |= match still {
            true => std::mem::replace(&mut it.pace.shown, it.new.s.len()) != it.new.s.len(),
            false => it.pace.step(&it.new.s, now),
        };
    }
    if moved {
        sync(ed);
        ed.unchanged = false;
    }
}

/// When the reveal moves next while an item drawn has text left to show — None: nothing left, no frame.
pub fn reveal_wake(ed: &Editor) -> Option<Duration> {
    let c =
        ed.session.chat.as_ref().filter(|c| c.writing.paced && ed.motion() != crate::motion::Motion::None)?;
    c.writing
        .calls
        .iter()
        .flat_map(|c| &c.items)
        .filter(|it| it.pace.behind(&it.new.s) && matches!(&it.place, Place::Found(f) if f.bound.is_some()))
        .map(|it| {
            let since = it.pace.stepped.map_or(Duration::ZERO, crate::clock::since);
            REVEAL_EVERY.saturating_sub(since).max(Duration::from_millis(1))
        })
        .min()
}

/// How far from where it was the text an item replaces is looked for once its buffer changed (`relocate`).
const RELOCATE_WINDOW: usize = 64 * 1024;

/// Where the text an item replaces is in its buffer after an edit (the review's rule for its hunks,
/// `llm::relocate`, within `RELOCATE_WINDOW` of where it was): still there, else its nearest copy — None: gone,
/// the edit was inside it.
fn relocate(text: &Rope, f: &Found) -> Option<(usize, usize)> {
    let snap = |b: usize| text.char_to_byte(text.byte_to_char(b.min(text.len_bytes())));
    if f.to <= text.len_bytes()
        && snap(f.from) == f.from
        && snap(f.to) == f.to
        && text.byte_slice(f.from..f.to) == f.held.as_str()
    {
        return Some((f.from, f.to));
    }
    if f.held.is_empty() {
        return None;
    }
    let (a, b) = (snap(f.from.saturating_sub(RELOCATE_WINDOW)), snap(f.to + RELOCATE_WINDOW));
    let near = text.byte_slice(a..b).to_string();
    let at = near.match_indices(f.held.as_str()).map(|(i, _)| a + i).min_by_key(|i| i.abs_diff(f.from))?;
    Some((at, at + f.held.len()))
}

/// The lines of the block being written in the focused pane when its head is below the screen while its first
/// row is on it — rows counted as the pane lays them out (`codeview::Measure`); None with the cursor on it (or on the
/// line right under it, where the new rows of one that only adds lines stand).
fn past_sight(ed: &Editor) -> Option<std::ops::Range<usize>> {
    let doc = ed.doc();
    let b = shown(doc)?.blocks.iter().find(|b| b.head.is_some())?;
    let cursor = crate::movement::line_of(&doc.text, doc.selection().primary().head);
    let (rows, cols) = ed.viewport;
    if (b.first..=b.last + 1).contains(&cursor) || rows == 0 || b.first < doc.top {
        return None;
    }
    let wrap = ed.wraps(doc).then_some(cols);
    let m = crate::codeview::Measure::new(ed, doc, ed.editing_in(true), (wrap, cols), b.first, rows);
    let above = (doc.top..b.first).map(|l| m.rows(l)).sum::<usize>().saturating_sub(doc.top_skip());
    (above < rows && above + m.removed(b.first) + m.head(b.first) >= rows).then_some(b.first..b.last + 1)
}

/// Whether Claude is still writing a change over `lines` of `path` (an item of a call under way, its place there):
/// a jump to it that waited is still worth taking (`follow::writing_due`).
pub fn being_written(ed: &Editor, path: &std::path::Path, lines: &std::ops::Range<usize>) -> bool {
    let Some(c) = ed.session.chat.as_ref() else { return false };
    c.writing.calls.iter().flat_map(|c| &c.items).any(|it| match &it.place {
        Place::Found(f) => f.path == path && f.lines.0 < lines.end && lines.start <= f.lines.1,
        _ => false,
    })
}

/// Where call `call`'s writing ended in `after` — `path`'s text once it landed, `before` the text it landed on — when
/// it was drawn in the document in front and follow led the view on its last item to its last piece
/// (`Item.followed`): the last line that item wrote, its place in `before` moved by the items above it; one that
/// doesn't read there (matched loosely, the buffer changed since) the edit's last changed line. None: nobody watched it
/// written there (`follow::wrote` takes the view to its first line). A review's rows aren't the writing's
/// (`reviewed`).
pub fn written_end(
    ed: &Editor,
    call: &str,
    path: &std::path::Path,
    before: &Rope,
    after: &Rope,
) -> Option<usize> {
    let c = ed.session.chat.as_ref()?;
    let doc = ed.doc();
    let call = c.writing.calls.iter().find(|k| k.id == call).filter(|_| doc.path.as_deref() == Some(path))?;
    // (its items drawn there, in the order they were written)
    let drawn: Vec<(&Found, &str, bool)> = call
        .items
        .iter()
        .filter_map(|it| match &it.place {
            Place::Found(f) if f.bound.is_some_and(|b| b.0 == doc.id) => {
                Some((f, it.new.s.as_str(), it.followed))
            }
            _ => None,
        })
        .collect();
    let &(last, new, followed) = drawn.last()?;
    if !followed {
        return None;
    }
    let at = || {
        let read = |f: &Found| before.get_byte_slice(f.from..f.to).is_some_and(|s| s == f.held.as_str());
        if !drawn.iter().all(|(f, ..)| read(f)) {
            return None;
        }
        let shift: isize = drawn
            .iter()
            .filter(|(f, ..)| f.from < last.from)
            .map(|(f, new, _)| new.len() as isize - (f.to - f.from) as isize)
            .sum();
        let start = last.from.checked_add_signed(shift)?;
        let end = start + new.len();
        // (its last byte's line: the line it ends on, not the one after its newline)
        (after.get_byte_slice(start..end)? == new)
            .then(|| after.byte_to_line(end.saturating_sub(1).max(start)))
    };
    Some(at().unwrap_or_else(|| crate::follow::last_difference(before, after)))
}

/// The line of `doc` whose end Claude's caret is at when the row it's writing is one it keeps (a piece ended right
/// at that line's end) — drawn after the line's last character, as after a written row's (`term::draw_text`): the
/// caret never misses a frame while the writing goes on.
pub fn caret_line(doc: &crate::document::Document) -> Option<usize> {
    shown(doc)?.blocks.iter().find_map(|b| match b.rows.get(b.head?)? {
        Row::Text { line, .. } => Some(*line),
        _ => None,
    })
}

/// The rows of an item that replaces bytes `from..to` of `text` with `new` (`closed`: whole) — the review's own
/// layout of a change (`llm::Review::rows`): its whole lines, the line diff of what they were and what they
/// become, a line kept as the document's row, one taken out marked so, one written its own row. While it's
/// written the head line has no rest of the line after it, and the lines past the head are taken out until it
/// writes them again.
pub fn block(text: &Rope, (from, to): (usize, usize), new: &str, closed: bool) -> Block {
    let last = crate::movement::last_line(text);
    let line_of = |b: usize| text.byte_to_line(b.min(text.len_bytes()));
    let first = line_of(from);
    let lt = if to > from { line_of(to - 1) } else { first };
    let ls = text.line_to_byte(first);
    let le = text.line_to_byte((lt + 1).min(last + 1)).max(to);
    let (pre, post) = (text.byte_slice(ls..from.max(ls)).to_string(), text.byte_slice(to..le).to_string());
    let old = format!("{pre}{}{post}", text.byte_slice(from..to));
    let new = if closed { format!("{pre}{new}{post}") } else { format!("{pre}{new}") };
    let written = new.lines().count();
    let max = if closed { DIFF_MAX } else { DIFF_WRITING_MAX };
    let diff: Vec<(char, &str)> = if old.len() + new.len() <= max {
        crate::llm::line_diff(&old, &new)
    } else {
        old.lines().map(|l| ('-', l)).chain(new.lines().map(|l| ('+', l))).collect()
    };
    let (mut rows, mut head) = (Vec::with_capacity(diff.len()), None);
    // (the document line the next old line is, how many new lines so far)
    let (mut old_no, mut new_no) = (first, 0);
    for (kind, line) in diff {
        if kind != '-' {
            new_no += 1;
        }
        let here = !closed && kind != '-' && new_no == written;
        if here {
            head = Some(rows.len());
        }
        rows.push(match kind {
            '+' => Row::Other {
                at: old_no,
                text: line.to_string(),
                kind: OtherKind::Writing { head: here },
                part: None,
            },
            k => {
                let edit = (k == '-').then_some(Edit { hunk: 0, mark: Mark::Taken });
                old_no += 1;
                Row::Text { line: old_no - 1, part: None, edit }
            }
        });
    }
    Block {
        first,
        last: lt.max(first),
        rows,
        head,
        protected: false,
        counts: Default::default(),
        seed: Default::default(),
    }
}

/// A review opened on buffer `doc`: what was being written there is the review's now — drawn, or still looked for
/// by a call that's whole (its place found later is the review's too: `Call.reviewed`).
pub fn reviewed(ed: &mut Editor, doc: DocId) {
    let path = ed.docs.iter().find(|d| d.id == doc).and_then(|d| d.path.clone());
    let Some(c) = ed.session.chat.as_mut() else { return };
    for call in &mut c.writing.calls {
        if let Some(p) = path.as_ref().filter(|_| call.done) {
            call.reviewed.push(p.clone());
        }
        for it in &mut call.items {
            if let Place::Found(f) = &it.place
                && (f.bound.is_some_and(|b| b.0 == doc) || call.done && path.as_ref() == Some(&f.path))
            {
                it.place = Place::Nowhere;
            }
        }
    }
    if let Some(d) = ed.docs.iter_mut().find(|d| d.id == doc) {
        d.writing = None;
    }
}

/// Call `id` answered (its result came): its rows go.
pub fn answered(ed: &mut Editor, id: &str) {
    let Some(c) = ed.session.chat.as_mut() else { return };
    let before = c.writing.calls.len();
    c.writing.calls.retain(|c| c.id != id);
    if c.writing.calls.len() != before {
        sync(ed);
    }
}

/// The turn is over, the chat closed or its process replaced: nothing is being written — in this session's
/// buffers (another session's rows are its own chat's).
pub fn over(ed: &mut Editor) {
    if let Some(c) = ed.session.chat.as_mut() {
        c.writing.calls.clear();
    }
    let others: Vec<DocId> = ed.sessions.iter().flat_map(|l| l.docs.iter().copied()).collect();
    for d in ed.docs.iter_mut().filter(|d| !others.contains(&d.id)) {
        d.writing = None;
    }
}

/// What a chat is writing, for the state and Claude's level (`uistate::WritingState`).
pub fn listed(
    chat: Option<&crate::chat::Chat>,
    docs: &[crate::document::Document],
) -> Vec<crate::uistate::WritingState> {
    let Some(c) = chat else { return Vec::new() };
    c.writing
        .calls
        .iter()
        .flat_map(|call| call.items.iter().filter(|it| !it.other && it.file.closed).map(move |it| (call, it)))
        .map(|(call, it)| {
            let lines = match &it.place {
                Place::Found(Found { bound: Some((id, _)), lines, .. }) => docs
                    .iter()
                    .find(|d| d.id == *id)
                    .and_then(|d| shown(d)?.at(lines.0))
                    .map(|b| (b.first + 1, b.last + 1)),
                _ => None,
            };
            crate::uistate::WritingState {
                call: call.id.clone(),
                file: it.file.s.clone(),
                lines,
                whole: it.new.closed,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests;
