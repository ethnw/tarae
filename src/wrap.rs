//! Soft wrap — a line wider than the text area is shown as several screen rows, broken after a blank
//! where it can (else mid-word), continuation rows indented like the line. Shared by drawing (term.rs)
//! and `j`/`k` (rows, not lines). Positions are bytes; columns are display cells from the line start.

use ropey::Rope;

use unicode_width::UnicodeWidthStr;

use crate::graphemes::{self, cells};
use crate::movement::{self as mv, Direction};
use crate::selection::Range;

/// Lines longer than this aren't wrapped (drawn clipped, scrolled sideways) — wrapping reads the whole line.
pub const MAX_LINE: usize = 64 * 1024;

/// One screen row of a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Row {
    /// Byte position of the row's first char.
    pub start: usize,
    /// Display column (from the line start) of the row's first char · the column it stops before
    /// (`usize::MAX` on the last row).
    pub col: usize,
    pub end_col: usize,
    /// Screen cells before the text (continuation rows are indented like the line).
    pub x0: usize,
}

impl Row {
    pub(crate) fn whole(start: usize) -> Self {
        Row { start, col: 0, end_col: usize::MAX, x0: 0 }
    }
}

/// A document's rows as its surface laid them out (the chat's transcript, `chatdoc.rs`): `j`/`k`, the pages and the
/// `z` moves count these, never `rows`' own breaks — its layout wraps the text, the text stays one line a
/// paragraph. Every line has a row at least; starts are bytes of the document. Held in parts — the surface's
/// pieces, each its own `Arc` — so the parts before one laid out again stay as they were, shared (the chat's answer
/// being written: only its own rows are made again, whatever the conversation's length).
#[derive(Clone, Debug, Default)]
pub struct Given {
    parts: Vec<std::sync::Arc<Part>>,
    /// Where each part's rows and lines begin.
    rows0: Vec<usize>,
    lines0: Vec<usize>,
    /// Rows and lines in all.
    rows: usize,
    lines: usize,
}

/// Whole lines' rows (`Given`): every row, in order, and where each of its lines' rows begin in them (its lines
/// counted from its first).
#[derive(Debug, Default)]
pub struct Part {
    pub rows: Vec<Row>,
    pub first: Vec<usize>,
}

impl Part {
    fn of(&self, line: usize) -> &[Row] {
        let Some(&a) = self.first.get(line) else { return &[] };
        let b = self.first.get(line + 1).copied().unwrap_or(self.rows.len());
        &self.rows[a..b.max(a)]
    }
}

impl Given {
    /// Of one part.
    #[cfg(test)]
    pub fn new(rows: Vec<Row>, first: Vec<usize>) -> Given {
        let mut g = Given::default();
        g.push(std::sync::Arc::new(Part { rows, first }));
        g
    }

    /// Its parts, in order.
    pub fn parts(&self) -> &[std::sync::Arc<Part>] {
        &self.parts
    }

    /// Only its first `n` parts kept.
    pub fn keep(&mut self, n: usize) {
        if n >= self.parts.len() {
            return;
        }
        self.parts.truncate(n);
        self.rows0.truncate(n);
        self.lines0.truncate(n);
        (self.rows, self.lines) = match self.parts.last() {
            Some(p) => (self.rows0[n - 1] + p.rows.len(), self.lines0[n - 1] + p.first.len()),
            None => (0, 0),
        };
    }

    /// `part` after the others: its lines and rows the next.
    pub fn push(&mut self, part: std::sync::Arc<Part>) {
        self.rows0.push(self.rows);
        self.lines0.push(self.lines);
        self.rows += part.rows.len();
        self.lines += part.first.len();
        self.parts.push(part);
    }

    /// Its rows in all.
    pub fn len(&self) -> usize {
        self.rows
    }

    pub fn is_empty(&self) -> bool {
        self.rows == 0
    }

    /// Row `i`.
    pub fn get(&self, i: usize) -> Option<&Row> {
        let (p, k) = self.part_of(i)?;
        self.parts[p].rows.get(k)
    }

    /// The part row `i` is in, and which of its rows it is (an empty part never: the next starts where it does).
    pub fn part_of(&self, i: usize) -> Option<(usize, usize)> {
        if i >= self.rows {
            return None;
        }
        let p = self.rows0.partition_point(|&a| a <= i) - 1;
        Some((p, i - self.rows0[p]))
    }

    /// Line `line`'s rows (one whole row past the last line: there's none).
    pub fn of(&self, line: usize) -> &[Row] {
        if line >= self.lines {
            return &[];
        }
        let p = self.lines0.partition_point(|&a| a <= line) - 1;
        self.parts[p].of(line - self.lines0[p])
    }

    /// Row `k` of `line`, counted from the first row.
    pub fn index(&self, line: usize, k: usize) -> usize {
        if line >= self.lines {
            return self.rows;
        }
        let p = self.lines0.partition_point(|&a| a <= line) - 1;
        let (part, l) = (&self.parts[p], line - self.lines0[p]);
        self.rows0[p] + part.first[l] + k.min(part.of(l).len().saturating_sub(1))
    }

    /// The line row `i` is of, and which of its rows it is (past the last row: its last line's).
    pub fn place(&self, i: usize) -> (usize, usize) {
        let Some((p, k)) = self.part_of(i) else {
            let line = self.lines.saturating_sub(1);
            let first = if self.lines == 0 { 0 } else { self.index(line, 0) };
            return (line, i.saturating_sub(first));
        };
        let part = &self.parts[p];
        let l = part.first.partition_point(|&a| a <= k).saturating_sub(1);
        (self.lines0[p] + l, k - part.first.get(l).copied().unwrap_or(0))
    }
}

/// `line`'s rows: the ones `given` (a surface's own layout), else wrapped at `width` cells.
pub fn line_rows(text: &Rope, given: Option<&Given>, line: usize, width: usize, tab: usize) -> Vec<Row> {
    match given.map(|g| g.of(line)).filter(|r| !r.is_empty()) {
        Some(r) => r.to_vec(),
        None => rows(text, line, width, tab),
    }
}

/// The rows of `line` at `width` cells.
pub fn rows(text: &Rope, line: usize, width: usize, tab: usize) -> Vec<Row> {
    let (start, end) = (mv::line_start(text, line), mv::line_end(text, line));
    if end - start > MAX_LINE || width < 8 {
        return vec![Row::whole(start)];
    }
    let slice = text.byte_slice(start..end);
    // Without a tab no char is wider than its bytes (ASCII 1 cell a byte; others ≥ 2 bytes, ≤ 2 cells), so a
    // line with fewer bytes than the row has cells fits — most code lines, told without segmenting them
    if end - start < width && !slice.bytes().any(|b| b == b'\t') {
        return vec![Row::whole(start)];
    }
    let s: std::borrow::Cow<str> = slice.into();
    let cs: Vec<_> = cells(&s, start, tab).collect();
    // Continuation indent = the line's own (+ a list or quote marker, so the text hangs under the text),
    // unless it would leave too little room
    let lead = s.len() - s.trim_start().len();
    let indent = cs.iter().take_while(|c| c.bytes.end <= lead).map(|c| c.width).sum::<usize>()
        + marker(&s[lead..]).width();
    let indent = if indent <= width / 2 { indent } else { 0 };
    let mut out = Vec::new();
    let (mut row_start, mut row_col, mut x0) = (start, 0, 0);
    // Where the row could end: after a blank (start of the next word) — (position, column)
    let mut brk: Option<(usize, usize)> = None;
    let mut i = 0;
    while i < cs.len() {
        let c = &cs[i];
        // Text leaves the row's last cell free — a blank may hang there, so a word that ends at the edge
        // doesn't get pushed down by the space after it
        let blank = s[c.bytes.clone()].trim().is_empty();
        let room = width - x0 - usize::from(!blank);
        if c.col + c.width - row_col > room && c.col > row_col {
            let (pos, col) = match brk.filter(|&(_, col)| col > row_col) {
                Some(b) => b,
                None => (c.pos, c.col),
            };
            out.push(Row { start: row_start, col: row_col, end_col: col, x0 });
            (row_start, row_col, x0, brk) = (pos, col, indent, None);
            // Re-read from the new row's first char (the word carried over may need more rows)
            i = cs.partition_point(|c| c.pos < pos);
            continue;
        }
        if blank {
            brk = Some((c.pos + c.len, c.col + c.width));
        }
        i += 1;
    }
    out.push(Row { start: row_start, col: row_col, end_col: usize::MAX, x0 });
    out
}

/// A list item or quote marker at the start of a line's text (`- `, `* `, `+ `, `12. `, `3) `, `> `).
fn marker(s: &str) -> &str {
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    let n = match s.as_bytes() {
        [b'-' | b'*' | b'+' | b'>', b' ', ..] => 2,
        _ if (1..10).contains(&digits)
            && matches!(s.as_bytes().get(digits..digits + 2), Some([b'.' | b')', b' '])) =>
        {
            digits + 2
        }
        _ => 0,
    };
    &s[..n]
}

/// Which of `rows` holds `pos` (the last row whose start is at or before it).
pub fn row_of(rows: &[Row], pos: usize) -> usize {
    rows.partition_point(|r| r.start <= pos).saturating_sub(1)
}

/// Screen x of `pos` within its row.
pub fn x_of(text: &Rope, given: Option<&Given>, pos: usize, width: usize, tab: usize) -> usize {
    let line = mv::line_of(text, pos);
    let rows = line_rows(text, given, line, width, tab);
    let r = rows[row_of(&rows, pos)];
    r.x0 + mv::visual_col(text, pos, tab).saturating_sub(r.col)
}

/// The char of row `k` under screen x (the row's last char if x is past it).
pub fn pos_in_row(text: &Rope, line: usize, rows: &[Row], k: usize, x: usize, tab: usize) -> usize {
    let r = rows[k];
    let pos = mv::pos_at_col(text, line, r.col + x.saturating_sub(r.x0), tab);
    match rows.get(k + 1) {
        Some(next) if pos >= next.start => graphemes::prev_boundary(text, next.start).max(r.start),
        _ => pos,
    }
}

/// `j`/`k` over screen rows: `n` rows in `dir` keeping screen x `x` (sticky across moves) — the rows `given`, else
/// wrapped at `width`.
#[allow(clippy::too_many_arguments)]
pub fn move_rows(
    text: &Rope,
    given: Option<&Given>,
    r: Range,
    dir: Direction,
    n: usize,
    extend: bool,
    x: usize,
    width: usize,
    tab: usize,
) -> Range {
    let rows = |line| line_rows(text, given, line, width, tab);
    let head = r.cursor(text);
    let last = mv::last_line(text);
    let mut line = mv::line_of(text, head);
    let mut rs = rows(line);
    let mut k = row_of(&rs, head);
    for _ in 0..n {
        match dir {
            Direction::Forward if k + 1 < rs.len() => k += 1,
            Direction::Forward if line < last => {
                line += 1;
                rs = rows(line);
                k = 0;
            }
            Direction::Backward if k > 0 => k -= 1,
            Direction::Backward if line > 0 => {
                line -= 1;
                rs = rows(line);
                k = rs.len() - 1;
            }
            _ => break,
        }
    }
    r.put_cursor(text, pos_in_row(text, line, &rs, k, x, tab), extend)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shown(src: &str, width: usize) -> Vec<String> {
        let t = Rope::from_str(src);
        let rs = rows(&t, 0, width, 4);
        let end = mv::line_end(&t, 0);
        rs.iter()
            .enumerate()
            .map(|(k, r)| {
                let to = rs.get(k + 1).map_or(end, |n| n.start);
                format!("{}{}", " ".repeat(r.x0), t.byte_slice(r.start..to))
            })
            .collect()
    }

    #[test]
    fn breaks_after_blanks_and_mid_word_only_when_it_must() {
        assert_eq!(shown("the quick brown fox", 10), ["the quick ", "brown fox"]);
        assert_eq!(shown("abcdefghijklmnop", 9), ["abcdefgh", "ijklmnop"], "the last cell is for a blank");
        assert_eq!(shown("short", 10), ["short"]);
    }

    #[test]
    fn continuation_rows_keep_the_indent() {
        assert_eq!(shown("    one two three four", 12), ["    one two ", "    three ", "    four"]);
    }

    #[test]
    fn list_items_hang_under_their_text() {
        assert_eq!(shown("- one two three", 10), ["- one two ", "  three"]);
        assert_eq!(shown("12. one two three", 12), ["12. one two ", "    three"]);
        assert_eq!(shown("-x one two three", 11), ["-x one two ", "three"], "not a marker");
    }

    #[test]
    fn wide_chars_never_split() {
        let rows = shown("가나다라마바", 9); // 2 cells each — 4 per 9-cell row
        assert_eq!(rows, ["가나다라", "마바"]);
    }

    #[test]
    fn rows_moves_keep_the_screen_x() {
        let t = Rope::from_str("aaaa bbbb cccc\nzz\n");
        let r = move_rows(&t, None, Range::point(1), Direction::Forward, 1, false, 1, 8, 4);
        assert_eq!(r, Range::point(6), "next row of the same line");
        let r = move_rows(&t, None, r, Direction::Forward, 2, false, 1, 8, 4);
        assert_eq!(r, Range::point(16), "then into the next line");
        let r = move_rows(&t, None, Range::point(16), Direction::Backward, 1, false, 7, 8, 4);
        assert_eq!(r, Range::point(14), "x past the last row's end: the line end, like j/k");
        let r = move_rows(&t, None, Range::point(16), Direction::Backward, 2, false, 7, 8, 4);
        assert_eq!(r, Range::point(9), "x past a middle row's end: its last char");
        assert_eq!(x_of(&t, None, 6, 8, 4), 1);
    }

    /// Rows a surface gave: `j`/`k` step by them — a row the width alone would never break — its cells before the
    /// text kept out of the screen x.
    #[test]
    fn given_rows_are_the_ones_moved_over() {
        let t = Rope::from_str("aaaa bbbb\nzz\n");
        let row = |start, col, end_col, x0| Row { start, col, end_col, x0 };
        let g = Given::new(
            vec![row(0, 0, 5, 2), row(5, 5, usize::MAX, 4), row(10, 0, usize::MAX, 2)],
            vec![0, 2],
        );
        assert_eq!((g.of(0).len(), g.of(1).len(), g.index(1, 0), g.place(1)), (2, 1, 2, (0, 1)));
        // (the same rows in two parts, an empty one between: the same answers)
        let mut parts = Given::default();
        for (rows, first) in [(vec![row(0, 0, 5, 2), row(5, 5, usize::MAX, 4)], vec![0]), (vec![], vec![])] {
            parts.push(std::sync::Arc::new(Part { rows, first }));
        }
        parts.push(std::sync::Arc::new(Part { rows: vec![row(10, 0, usize::MAX, 2)], first: vec![0] }));
        for i in 0..4 {
            assert_eq!((parts.get(i), parts.place(i)), (g.get(i), g.place(i)), "row {i}");
        }
        for l in 0..3 {
            assert_eq!((parts.of(l), parts.index(l, 1)), (g.of(l), g.index(l, 1)), "line {l}");
        }
        parts.keep(1);
        assert_eq!((parts.len(), parts.of(1).len(), parts.index(1, 0)), (2, 0, 2), "its first part kept");
        let r = move_rows(&t, Some(&g), Range::point(1), Direction::Forward, 1, false, 3, 80, 4);
        assert_eq!(
            r,
            Range::point(5),
            "its second row, though 80 cells hold the line: x 3 is the row's first"
        );
        let r = move_rows(&t, Some(&g), r, Direction::Forward, 1, false, 3, 80, 4);
        assert_eq!(r, Range::point(11), "the next line's row");
        assert_eq!(x_of(&t, Some(&g), 6, 80, 4), 5, "its cells before the text, then its column in the row");
    }
}
