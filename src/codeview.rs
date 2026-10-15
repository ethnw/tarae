//! A code view's rows: what a place that draws a file's code shows, screen row by screen row — a line of the
//! document (or one wrapped part of it), a folded doc comment's row, or text that isn't the document's (a
//! removed line). Screen rows ≠ document lines: drawing, the mouse and the moves go through these.

use ropey::Rope;

use crate::config::{CursorShape, LineNumber};
use crate::document::Document;
use crate::editor::{Editor, Mode};
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
    /// The primary cursor painted as a cell (focused, not INSERT, a block cursor).
    pub primary: bool,
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

/// Where a code view's change marks come from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Changes {
    /// The index's (the git gutter).
    Index,
    /// A code review's or `:diff`'s base: added lines washed, removed ones rows of their own.
    Base,
    /// An edit of Claude's that follow or replay shows: its lines washed, the ones it replaced above them.
    Shown,
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
            changes: match (&doc.review_base, &doc.shown) {
                (Some(_), _) => Changes::Base,
                (None, Some(_)) => Changes::Shown,
                (None, None) => Changes::Index,
            },
            numbers: config.line_number,
            primary: focused && not_insert && crate::term::cursor_shape(editor) == CursorShape::Block,
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
