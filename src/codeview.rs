//! A code view's rows: what a place that draws a file's code shows, screen row by screen row — a line of the
//! document (or one wrapped part of it), a folded doc comment's row, or text that isn't the document's (a
//! removed line). Screen rows ≠ document lines: drawing, the mouse and the moves go through these.

use std::borrow::Cow;
use std::ops::Range;

use ropey::Rope;

use crate::config::{CursorShape, LineNumber};
use crate::document::Document;
use crate::editor::{Editor, Mode};
use crate::git::{Hunk, HunkKind};
use crate::syntax::Syntax;

/// One screen row of a code view.
#[derive(Clone, Debug)]
pub enum Row {
    /// A line of the document — `part` when it soft-wraps: which of its rows this is.
    Text { line: usize, part: Option<Part> },
    /// One row of a folded doc comment block (a rendered piece; `Editor::doc_block_folded` decides it).
    Doc {
        /// Document line range of the block [start, end].
        start: usize,
        end: usize,
        indent: usize,
        /// Which row within the block.
        index: usize,
        spans: Vec<crate::markdown::Span>,
        /// Example code row (background one shade deeper).
        code: bool,
    },
    /// Text that isn't the document's: a line the code review's base had here and the text no longer does, or
    /// one a shown edit replaced (shown above line `at`).
    Other { at: usize, text: String },
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
    /// The cursor line's band (`editor.cursorline`, focused only).
    pub cursorline: bool,
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
            (Changes::Shown, None) => Cow::Borrowed(&[]),
            _ => Cow::Borrowed(&doc.git_hunks),
        }
    }

    /// Whether `line` is on a change's wash (an added or changed line, where removed lines are rows).
    pub fn washed(self, hunks: &[Hunk], line: usize) -> bool {
        self.rows() && hunks.iter().any(|h| h.kind != HunkKind::Deleted && h.lines.contains(&line))
    }

    /// The removed lines that go above lines `from..=to`: (line they go above, text).
    pub fn removed(self, doc: &Document, from: usize, to: usize) -> Vec<(usize, String)> {
        let starts = |h: &Range<usize>| (from..=to).contains(&h.start);
        match (self, &doc.review_base, &doc.shown) {
            (Changes::Base, Some(base), _) => doc
                .git_hunks
                .iter()
                .filter(|h| h.removed > 0 && starts(&h.lines))
                .flat_map(|h| h.base.clone().filter(|&b| b < base.len_lines()).map(|b| (h.lines.start, b)))
                .map(|(at, b)| (at, base.line(b).to_string().trim_end_matches(['\n', '\r']).to_string()))
                .collect(),
            (Changes::Shown, _, Some(shown)) => shown
                .iter()
                .filter(|h| starts(&h.lines))
                .flat_map(|h| h.removed.iter().map(|t| (h.lines.start, t.clone())))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// How many removed rows go above `line`.
    pub fn rows_above(self, doc: &Document, line: usize) -> usize {
        match (self, &doc.shown) {
            (Changes::Base, _) => {
                doc.git_hunks.iter().filter(|h| h.lines.start == line).map(|h| h.removed).sum()
            }
            (Changes::Shown, Some(shown)) => {
                shown.iter().filter(|h| h.lines.start == line).map(|h| h.removed.len()).sum()
            }
            _ => 0,
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

impl Shows {
    /// A pane of the editor showing `doc`.
    pub fn pane(editor: &Editor, doc: &Document, focused: bool, left: usize) -> Shows {
        let not_insert = editor.mode != Mode::Insert;
        let config = &editor.config;
        Shows {
            focused,
            editing: editor.editing_in(focused),
            left,
            changes: Changes::of(doc),
            numbers: config.line_number,
            primary: focused && not_insert && crate::term::cursor_shape(editor) == CursorShape::Block,
            search: editor.search_hits.as_ref().is_some_and(|h| h.doc == doc.id),
            cursorline: focused && config.cursorline,
            messages: config.inline_diagnostics,
            inlay: config.inlay_hints && left == 0,
            guides: config.indent_guides && left == 0,
            blame: focused && not_insert,
        }
    }
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
            Row::Other { at, .. } => *at,
        }
    }

    pub fn last_line(&self) -> usize {
        match self {
            Row::Text { line, .. } => *line,
            Row::Doc { end, .. } => *end,
            // Not a line of the text: it doesn't widen the per-line tables
            Row::Other { at, .. } => at.saturating_sub(1),
        }
    }

    /// For the mouse: (doc line, display column at the row's first text cell, cells before the text,
    /// column the row stops before).
    pub fn origin(&self, left: usize) -> (usize, usize, usize, usize) {
        match self {
            Row::Text { line, part: Some(p) } => (*line, p.row.col, p.row.x0, p.row.end_col),
            // (a folded block doesn't scroll sideways)
            Row::Doc { start, .. } => (*start, 0, 0, usize::MAX),
            _ => (self.line(), left, 0, usize::MAX),
        }
    }
}
