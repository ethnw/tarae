//! A code view's rows: what a place that draws a file's code shows, screen row by screen row — a line of the
//! document (or one wrapped part of it), a folded doc comment's row, text that isn't the document's (a removed
//! line, a line Claude proposes), or the edit review's change header. Screen rows ≠ document lines: drawing,
//! the mouse and the moves go through these.

use std::borrow::Cow;
use std::ops::Range;

use ropey::Rope;

use crate::config::{CursorShape, LineNumber};
use crate::document::Document;
use crate::editor::{Editor, Mode};
use crate::git::{Hunk, HunkKind};
use crate::syntax::Syntax;

/// A folded doc comment row's text — shared: the edit review folds the whole file each frame from its cache.
pub type Spans = std::sync::Arc<[crate::markdown::Span]>;

/// One screen row of a code view.
#[derive(Clone, Debug)]
pub enum Row {
    /// A line of the document — `part` when it soft-wraps: which of its rows this is; `edit` when it's one of the
    /// lines a change of the edit review takes out or keeps.
    Text { line: usize, part: Option<Part>, edit: Option<Edit> },
    /// One row of a folded doc comment block (a rendered piece; `Editor::doc_block_folded` decides it).
    Doc {
        /// Document line range of the block [start, end].
        start: usize,
        end: usize,
        indent: usize,
        /// Which row within the block.
        index: usize,
        spans: Spans,
        /// Example code row (background one shade deeper).
        code: bool,
    },
    /// Text that isn't the document's, shown above line `at` (`part` when it soft-wraps, as a line does).
    Other { at: usize, text: String, kind: OtherKind, part: Option<Part> },
    /// The edit review's head of change `hunk` (its first line `at`) — drawn by the review (`draw_review`).
    Header { hunk: usize, at: usize },
}

/// What a row of text that isn't the document's is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OtherKind {
    /// A line the code review's base had here and the text no longer does (`base`: which, for the base's
    /// colours), or one a shown edit replaced.
    Gone { base: Option<usize> },
    /// A line of a change of the edit review: one it proposes — or, when the document no longer holds what the
    /// change replaces (edited while the review waits), one it takes out or keeps, from the text it was asked on.
    Edit(Edit),
}

/// A row of change `hunk` of the edit review, and what the change does to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edit {
    pub hunk: usize,
    pub mark: Mark,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// A line it proposes — on the added wash while it's undecided.
    Added { washed: bool },
    /// A line it takes out.
    Taken,
    /// A line inside it that stays.
    Kept,
}

/// The rows of a line that isn't the document's at `width` cells (one, unwrapped, without a width).
pub fn other_parts(text: &str, width: Option<usize>, tab: usize) -> Vec<Option<Part>> {
    let Some(w) = width else { return vec![None] };
    parts(crate::wrap::rows(&Rope::from_str(text), 0, w, tab))
}

/// The rows of document line `line` at `width` cells (one, unwrapped, without a width).
pub fn text_parts(text: &Rope, line: usize, width: Option<usize>, tab: usize) -> Vec<Option<Part>> {
    let Some(w) = width else { return vec![None] };
    parts(crate::wrap::rows(text, line, w, tab))
}

fn parts(rows: Vec<crate::wrap::Row>) -> Vec<Option<Part>> {
    if rows.len() == 1 {
        return vec![None];
    }
    let n = rows.len();
    rows.into_iter().enumerate().map(|(index, row)| Some(Part { row, index, last: index + 1 == n })).collect()
}

/// One screen row of a soft-wrapped line (`index` from 0; `last` = the row the line ends on).
#[derive(Clone, Copy, Debug)]
pub struct Part {
    pub row: crate::wrap::Row,
    pub index: usize,
    pub last: bool,
}

/// What a place shows — the conditions its rows are built and drawn by, decided once where the place is drawn
/// (`Shows::pane`) rather than read from the editor here and there.
pub struct Shows {
    /// Has the code's keys: the cursor line's number, jump labels, the line-end diagnostic counted as shown; a
    /// pane without them is toned down.
    pub focused: bool,
    /// The document's selection is shown: its ranges and cursors, the cursor line's number and guides marked,
    /// jump labels — not in the edit review, which has no cursor.
    pub cursor: bool,
    /// `Editor::editing_in(focused)` — the fold rule's input.
    pub editing: bool,
    /// Sideways scroll (display columns).
    pub left: usize,
    /// Where the change marks come from.
    pub changes: Changes,
    pub numbers: LineNumber,
    /// The primary cursor painted as a cell (focused, not INSERT, a block cursor); without the keys it's painted
    /// as the others are.
    pub primary: bool,
    /// The search's hits — only on the document searched (`SearchHits` are its offsets).
    pub search: bool,
    /// The line on the cursor line's band: the cursor's (`editor.cursorline`, focused only), or a preview's item's.
    pub cursorline: Option<usize>,
    /// Line-end diagnostics (`editor.inline-diagnostics`).
    pub messages: bool,
    /// Inlay hints and indent guides — off while scrolled sideways.
    pub inlay: bool,
    pub guides: bool,
    /// The cursor line's blame at its end (focused, not INSERT).
    pub blame: bool,
}

/// Where a code view's change marks come from — the one source of its signs, washes, removed rows, the
/// scrollbar's marks and `]g`/`[g`: a view shows one diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Changes {
    /// The index's (the git gutter).
    Index,
    /// A code review's or `:diff`'s base: added lines washed, removed ones rows of their own.
    Base,
    /// An edit of Claude's that follow or replay shows: its lines washed, the ones it replaced above them.
    Shown,
    /// The edit review's: its rows carry their own marks (`Edit`) — nothing from the document's diffs.
    Edit,
}

impl Changes {
    /// Where `doc`'s marks come from: a review base over a shown edit over the index.
    pub fn of(doc: &Document) -> Changes {
        match (&doc.review_base, &doc.shown) {
            (Some(_), _) => Changes::Base,
            (None, Some(_)) => Changes::Shown,
            (None, None) => Changes::Index,
        }
    }

    /// Removed lines are rows of their own (not the index's `▁` under the line before), changed lines washed.
    pub fn rows(self) -> bool {
        self != Changes::Index
    }

    /// The changes, in order: the index's or the base's (`git_hunks` — the document's git base is the review's),
    /// or the shown edit's (`base` empty there: what it removed is the edit's own text).
    pub fn hunks(self, doc: &Document) -> Cow<'_, [Hunk]> {
        match (self, &doc.shown) {
            (Changes::Shown, Some(shown)) => Cow::Owned(
                shown
                    .iter()
                    .map(|h| Hunk {
                        kind: match (h.lines.is_empty(), h.removed.is_empty()) {
                            (true, _) => HunkKind::Deleted,
                            (false, true) => HunkKind::Added,
                            (false, false) => HunkKind::Modified,
                        },
                        lines: h.lines.clone(),
                        removed: h.removed.len(),
                        base: 0..0,
                    })
                    .collect(),
            ),
            (Changes::Shown, None) | (Changes::Edit, _) => Cow::Borrowed(&[]),
            _ => Cow::Borrowed(&doc.git_hunks),
        }
    }

    /// Whether `line` is on a change's wash (an added or changed line, where removed lines are rows).
    pub fn washed(self, hunks: &[Hunk], line: usize) -> bool {
        self.rows() && hunks.iter().any(|h| h.kind != HunkKind::Deleted && h.lines.contains(&line))
    }

    /// The removed lines that go above lines `from..=to`: (line they go above, text, the base's line it was).
    pub fn removed(self, doc: &Document, from: usize, to: usize) -> Vec<(usize, String, Option<usize>)> {
        let starts = |h: &Range<usize>| (from..=to).contains(&h.start);
        match (self, &doc.review_base, &doc.shown) {
            (Changes::Base, Some(base), _) => doc
                .git_hunks
                .iter()
                .filter(|h| h.removed > 0 && starts(&h.lines))
                .flat_map(|h| h.base.clone().filter(|&b| b < base.len_lines()).map(|b| (h.lines.start, b)))
                .map(|(at, b)| {
                    (at, base.line(b).to_string().trim_end_matches(['\n', '\r']).to_string(), Some(b))
                })
                .collect(),
            (Changes::Shown, _, Some(shown)) => shown
                .iter()
                .filter(|h| starts(&h.lines))
                .flat_map(|h| h.removed.iter().map(|t| (h.lines.start, t.clone(), None)))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// How many removed rows go above `line` — each soft-wrapped at `width`, as the lines are.
    pub fn rows_above(self, doc: &Document, line: usize, width: Option<usize>, tab: usize) -> usize {
        let n = match (self, &doc.shown) {
            (Changes::Base, _) => {
                doc.git_hunks.iter().filter(|h| h.lines.start == line).map(|h| h.removed).sum()
            }
            (Changes::Shown, Some(shown)) => {
                shown.iter().filter(|h| h.lines.start == line).map(|h| h.removed.len()).sum()
            }
            _ => 0,
        };
        match (n, width) {
            (0, _) | (_, None) => n,
            _ => self.removed(doc, line, line).iter().map(|r| other_parts(&r.1, width, tab).len()).sum(),
        }
    }

    /// What `near` reads: the lines of the changes it looks at (rows folded by it are kept while they're equal).
    pub fn near_key(self, doc: &Document) -> Vec<Range<usize>> {
        match (self, &doc.shown) {
            (Changes::Base, _) => doc.git_hunks.iter().map(|h| h.lines.clone()).collect(),
            (Changes::Shown, Some(shown)) => shown.iter().map(|h| h.lines.clone()).collect(),
            _ => Vec::new(),
        }
    }

    /// Whether a change shown as rows and washes is in or right next to lines `a..=b`: a changed or added line
    /// among them or on either side, or removed rows inside them or right against them.
    pub fn near(self, doc: &Document, a: usize, b: usize) -> bool {
        // (a removal's `lines` is empty: its rows go above `lines.start`)
        let near = |l: &Range<usize>| l.start <= b + 1 && l.end >= a;
        match (self, &doc.shown) {
            (Changes::Base, _) => doc.git_hunks.iter().any(|h| near(&h.lines)),
            (Changes::Shown, Some(shown)) => shown.iter().any(|h| near(&h.lines)),
            _ => false,
        }
    }
}

impl Editor {
    /// Parses document `id`'s review base whole on a worker (`Document.base_syntax`) — a frame only when its
    /// removed rows are on screen.
    pub fn base_parse(&mut self, id: crate::document::DocId) {
        let Some(d) = self.docs.iter().find(|d| d.id == id) else { return };
        let (Some(base), Some(lang)) = (d.review_base.clone(), d.syntax.as_ref().map(|s| s.lang.clone()))
        else {
            return;
        };
        let seq = d.base_seq;
        self.events.jobs().spawn(move || {
            let syn = crate::syntax::Syntax::parsed(lang, &base);
            move |ed: &mut Editor| {
                let Some(d) = ed.docs.iter_mut().find(|d| d.id == id && d.base_seq == seq) else {
                    crate::event::drop_later(syn);
                    ed.unchanged = true;
                    return;
                };
                d.base_syntax = Some(syn);
                // (the removed rows' lines: in a pane of the last frame?)
                let gone = |l: usize| d.git_hunks.iter().any(|h| h.removed > 0 && h.lines.start == l);
                let views = &ed.session.views;
                let seen = ed.screen.panes.iter().any(|(view, _, _, rows)| {
                    views.iter().any(|v| v.id == *view && v.doc == id) && rows.iter().any(|r| gone(r.0))
                });
                ed.unchanged = !seen;
            }
        });
    }
}

impl Shows {
    /// A pane of the editor showing `doc`.
    pub fn pane(editor: &Editor, doc: &Document, focused: bool, left: usize) -> Shows {
        let not_insert = editor.mode != Mode::Insert;
        let config = &editor.config;
        Shows {
            focused,
            cursor: true,
            editing: editor.editing_in(focused),
            left,
            changes: Changes::of(doc),
            numbers: config.line_number,
            primary: focused && not_insert && crate::term::cursor_shape(editor) == CursorShape::Block,
            search: editor.search_hits.as_ref().is_some_and(|h| h.doc == doc.id),
            cursorline: (focused && config.cursorline).then(|| {
                let primary = crate::term::cursor_idx(editor.mode, doc.selection().primary(), &doc.text);
                crate::movement::line_of(&doc.text, primary)
            }),
            messages: config.inline_diagnostics,
            inlay: config.inlay_hints && left == 0,
            guides: config.indent_guides && left == 0,
            blame: focused && not_insert,
        }
    }

    /// Claude's edit review: the whole file as in the editor — diagnostics, indent guides, inlay hints,
    /// breakpoints on its document's rows — its change marks the edit's own (`Changes::Edit`), scrolled sideways
    /// by `left` when it doesn't wrap. Kept apart from a pane: absolute numbers and no cursor line, no selection,
    /// no search's hits, no blame — the review has no cursor (its keys answer changes, `j`/`k` scroll), and a
    /// number relative to a cursor that isn't there would read as distances from nothing.
    pub fn edit_review(editor: &Editor, left: usize) -> Shows {
        let config = &editor.config;
        Shows {
            focused: true,
            cursor: false,
            editing: false,
            left,
            changes: Changes::Edit,
            numbers: LineNumber::Absolute,
            primary: false,
            search: false,
            cursorline: None,
            messages: config.inline_diagnostics,
            inlay: config.inlay_hints && left == 0,
            guides: config.indent_guides && left == 0,
            blame: false,
        }
    }

    /// A picker's code preview: the file as the editor shows it — diagnostics, their line-end messages, signs,
    /// indent guides, its change marks as the index's are drawn (never removed lines as rows: the preview is
    /// laid out by lines from where the item opens). Kept apart from a pane: absolute numbers and no selection
    /// (no cursor there to count from); a band on `item`, the line the item opens at, even with
    /// `editor.cursorline` off — it marks where enter goes, not a cursor; no inlay hints — they're asked for
    /// the lines a pane shows, so a preview elsewhere in the file would have them on some lines and not others.
    /// Its lines always wrap (`term::rows_preview`): a preview is narrow and doesn't scroll sideways.
    pub fn preview(editor: &Editor, item: Option<usize>) -> Shows {
        let config = &editor.config;
        Shows {
            focused: true,
            cursor: false,
            editing: false,
            left: 0,
            changes: Changes::Index,
            numbers: LineNumber::Absolute,
            primary: false,
            search: false,
            cursorline: item,
            messages: config.inline_diagnostics,
            inlay: false,
            guides: config.indent_guides,
            blame: false,
        }
    }
}

/// Everything `Editor::doc_block_closed` reads but the block (`Editor::closed_key`): rows it folded are kept
/// while this is equal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClosedKey {
    /// The document and its version (its text).
    pub doc: (crate::document::DocId, u64),
    pub editing: bool,
    /// Its selection's ranges, `from..to`.
    pub selection: Vec<(usize, usize)>,
    /// The changes shown (`Changes::near_key`) and the merge conflicts' lines.
    pub changes: Vec<Range<usize>>,
    pub conflicts: Vec<(usize, usize)>,
}

/// The edit review's rows, folded (`term::review_rows`), kept on its `Review` while what they're built from holds
/// — built afresh each frame they took ~200 µs on a 12k-line file (`perf_review_folded_on_a_big_file`).
pub struct Kept {
    pub key: KeptKey,
    pub rows: std::sync::Arc<Vec<Row>>,
}

/// What the edit review's rows are built from: the fold rule's inputs (the document's version among them), each
/// change's place and answer, the doc comments' layout.
#[derive(PartialEq)]
pub struct KeptKey {
    pub closed: ClosedKey,
    pub hunks: Vec<(usize, usize, crate::llm::Decision, bool)>,
    pub folds: Option<crate::doccomment::FoldsKey>,
}

/// The edit review's rows (`Review::rows`, the whole file) with each doc comment block laid out in `folds`
/// shown folded — the block's lines one after another as plain rows of their own, the rows on either side
/// plain too (a change's head or line inside it or right next to it keeps it raw: a diff is read against the
/// text it changes), and `closed` (the one rule, `Editor::doc_block_closed`) saying so.
pub fn fold(
    rows: Vec<Row>,
    folds: &crate::doccomment::Folds,
    closed: impl Fn(usize, usize) -> bool,
) -> Vec<Row> {
    let plain = |r: Option<&Row>| r.is_none_or(|r| matches!(r, Row::Text { edit: None, .. }));
    let line = |r: &Row| match r {
        Row::Text { line, edit: None, part: None } => Some(*line),
        _ => None,
    };
    let mut out = Vec::with_capacity(rows.len());
    // (rows' lines climb: the blocks are walked beside them, not looked up for each row)
    let mut next = folds.blocks.iter().peekable();
    let mut i = 0;
    while i < rows.len() {
        let block = line(&rows[i]).and_then(|a| {
            while next.next_if(|b| b.start < a).is_some() {}
            next.peek().copied().filter(|b| b.start == a)
        });
        let Some(&crate::doccomment::Folded { start: a, end: b, indent, rows: ref laid }) = block else {
            out.push(rows[i].clone());
            i += 1;
            continue;
        };
        // The block's lines, as long as they're rows of their own one after another (a change breaks them)
        let mut j = i + 1;
        while j < rows.len() && a + j - i <= b && line(&rows[j]) == Some(a + j - i) {
            j += 1;
        }
        let whole = a + j - i - 1 == b;
        if !whole
            || !plain(i.checked_sub(1).and_then(|k| rows.get(k)))
            || !plain(rows.get(j))
            || !closed(a, b)
        {
            out.extend_from_slice(&rows[i..j]);
            i = j;
            continue;
        }
        out.extend(laid.iter().enumerate().map(|(index, (spans, code))| Row::Doc {
            start: a,
            end: b,
            indent,
            index,
            spans: spans.clone(),
            code: *code,
        }));
        i = j;
    }
    out
}

/// What the drawer reads text and colours from.
pub struct Source<'a> {
    pub doc: &'a Document,
    pub text: &'a Rope,
    /// The parse to colour by — only one with a tree.
    pub syntax: Option<&'a Syntax>,
}

impl<'a> Source<'a> {
    pub fn of(doc: &'a Document) -> Source<'a> {
        Source { doc, text: &doc.text, syntax: doc.syntax.as_ref().filter(|s| s.tree.is_some()) }
    }
}

impl Row {
    /// Document line this screen row points to (first line if folded — clicking goes there and unfolds it).
    pub fn line(&self) -> usize {
        match self {
            Row::Text { line, .. } => *line,
            Row::Doc { start, .. } => *start,
            Row::Other { at, .. } | Row::Header { at, .. } => *at,
        }
    }

    /// This row as its soft-wrapped part `p` (a line's or a text's that isn't the document's).
    pub fn with_part(&self, p: Option<Part>) -> Row {
        let mut row = self.clone();
        if let Row::Text { part, .. } | Row::Other { part, .. } = &mut row {
            *part = p;
        }
        row
    }

    /// The edit review's change this row is part of.
    pub fn hunk(&self) -> Option<usize> {
        match self {
            Row::Header { hunk, .. } => Some(*hunk),
            Row::Text { edit: Some(e), .. } | Row::Other { kind: OtherKind::Edit(e), .. } => Some(e.hunk),
            _ => None,
        }
    }

    pub fn last_line(&self) -> usize {
        match self {
            Row::Text { line, .. } => *line,
            Row::Doc { end, .. } => *end,
            // Not a line of the text: it doesn't widen the per-line tables
            Row::Other { at, .. } | Row::Header { at, .. } => at.saturating_sub(1),
        }
    }

    /// For the mouse: (doc line, display column at the row's first text cell, cells before the text,
    /// column the row stops before).
    pub fn origin(&self, left: usize) -> (usize, usize, usize, usize) {
        match self {
            Row::Text { line, part: Some(p), .. } | Row::Other { at: line, part: Some(p), .. } => {
                (*line, p.row.col, p.row.x0, p.row.end_col)
            }
            // (a folded block doesn't scroll sideways)
            Row::Doc { start, .. } => (*start, 0, 0, usize::MAX),
            _ => (self.line(), left, 0, usize::MAX),
        }
    }
}
