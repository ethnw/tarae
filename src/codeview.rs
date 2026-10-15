//! A code view's rows: what a place that draws a file's code shows, screen row by screen row — a line of the
//! document (or one wrapped part of it), a folded doc comment's row, or text that isn't the document's (a
//! removed line). Screen rows ≠ document lines: drawing, the mouse and the moves go through these.

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
