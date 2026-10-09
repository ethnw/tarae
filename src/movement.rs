//! Pure motion functions — (text, range) → range. Unaware of modes and editor state.
//!
//! Positions are bytes (always on char boundaries). Horizontal motion is per grapheme cluster, vertical
//! by visual column (tabs and wide chars). Char-kind checks (word motion, char find) are per codepoint.

use ropey::Rope;

use crate::graphemes::{self, char_at, next_char, prev_char};
use crate::selection::Range;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Forward,
    Backward,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CharClass {
    Eol,
    Whitespace,
    Word,
    Punct,
}

pub fn char_class(c: char) -> CharClass {
    match c {
        '\n' | '\r' => CharClass::Eol,
        c if c.is_whitespace() => CharClass::Whitespace,
        c if c.is_alphanumeric() || c == '_' => CharClass::Word,
        _ => CharClass::Punct,
    }
}

/// Class of the char at `pos`. A char extending a cluster (combining mark, ZWJ, skin tone) takes the class
/// of the cluster's first char — so word bounds never split a cluster (`cafe◌́` is one word).
pub fn class_at(text: &Rope, pos: usize) -> CharClass {
    let c = char_at(text, pos);
    if c.is_ascii() {
        return char_class(c); // only `\n` of `\r\n` extends a cluster, and it's Eol either way
    }
    let start = graphemes::prev_boundary(text, next_char(text, pos));
    char_class(if start < pos { char_at(text, start) } else { c })
}

/// Clamp a position past the document end to the start of the last char (0 for an empty document).
fn clamp_char(text: &Rope, pos: usize) -> usize {
    let len = text.len_bytes();
    if pos >= len { prev_char(text, len) } else { pos }
}

// ── Line helpers ─────────────────────────────────────────────────────────────

/// Last "real" line. The empty ghost line after a trailing newline doesn't count.
pub fn last_line(text: &Rope) -> usize {
    let n = text.len_lines();
    if n > 1 && text.line(n - 1).len_bytes() == 0 { n - 2 } else { n - 1 }
}

// (LTO copies it into many callers; −66 KB with node_slice, line_end, line_full_end — measured 2026-10-09)
#[inline(never)]
pub fn line_of(text: &Rope, pos: usize) -> usize {
    text.byte_to_line(pos.min(text.len_bytes()))
}

pub fn line_start(text: &Rope, line: usize) -> usize {
    text.line_to_byte(line)
}

/// End of the line content (excluding newline) = position of the newline char.
// (LTO copies it into many callers; −66 KB with node_slice, line_of, line_full_end — measured 2026-10-09)
#[inline(never)]
pub fn line_end(text: &Rope, line: usize) -> usize {
    let (start, mut end) = (line_start(text, line), line_full_end(text, line));
    // Most calls (every row of every frame): the line ends in its chunk
    let (chunk, at, _, _) = text.chunk_at_byte(end.saturating_sub(1));
    let byte = |i: usize| if i >= at { chunk.as_bytes()[i - at] } else { text.byte(i) };
    if end > start && byte(end - 1) == b'\n' {
        end -= 1;
        if end > start && byte(end - 1) == b'\r' {
            end -= 1;
        }
    }
    end
}

/// Line end including newline = start of the next line.
// (LTO copies it into many callers; −66 KB with node_slice, line_of, line_end — measured 2026-10-09)
#[inline(never)]
pub fn line_full_end(text: &Rope, line: usize) -> usize {
    text.line_to_byte((line + 1).min(text.len_lines()))
}

pub fn first_non_whitespace(text: &Rope, line: usize) -> usize {
    let (start, end) = (line_start(text, line), line_end(text, line));
    let mut pos = start;
    for c in text.byte_slice(start..end).chars() {
        if !c.is_whitespace() {
            break;
        }
        pos += c.len_utf8();
    }
    pos
}

/// Leading indentation string of the line.
pub fn indent_of(text: &Rope, line: usize) -> String {
    let start = line_start(text, line);
    text.byte_slice(start..first_non_whitespace(text, line)).to_string()
}

/// Lines the range spans (start line, end line). A 0-width range is the cursor line.
pub fn line_span(text: &Rope, r: Range) -> (usize, usize) {
    let r = r.min_width_1(text);
    // to-1 may be mid-char, but that's fine for asking the line number.
    let last = if r.to() > r.from() { r.to() - 1 } else { r.from() };
    (line_of(text, r.from()), line_of(text, last))
}

// ── Char and line motion ─────────────────────────────────────────────────────

pub fn move_horizontally(text: &Rope, r: Range, dir: Direction, count: usize, extend: bool) -> Range {
    let mut pos = r.cursor(text);
    for _ in 0..count {
        pos = match dir {
            Direction::Backward => graphemes::prev_boundary(text, pos),
            Direction::Forward => graphemes::next_boundary(text, pos),
        };
    }
    r.put_cursor(text, pos, extend)
}

/// Which screen cell `pos` is at within its line (accounting for tabs, wide chars, clusters).
pub fn visual_col(text: &Rope, pos: usize, tab: usize) -> usize {
    let start = line_start(text, line_of(text, pos));
    let s: std::borrow::Cow<str> = text.byte_slice(start..pos.max(start)).into();
    graphemes::seek(&s, 0, tab, usize::MAX).1
}

pub fn cursor_col(text: &Rope, r: Range, tab: usize) -> usize {
    visual_col(text, r.cursor(text), tab)
}

/// Char position covering screen cell `col` on line `line` (line end if the line is shorter).
pub fn pos_at_col(text: &Rope, line: usize, col: usize, tab: usize) -> usize {
    let (start, end) = (line_start(text, line), line_end(text, line));
    // Don't copy a very long line wholesale — cut the window end at a char boundary.
    let (sc, ec) = (text.byte_to_char(start), text.byte_to_char(end));
    let window = text.char_to_byte(ec.min(sc + col + 256));
    let s: std::borrow::Cow<str> = text.byte_slice(start..window).into();
    match graphemes::seek(&s, 0, tab, col) {
        (b, _) if b < s.len() => start + b,
        _ => window,
    }
}

/// Vertical motion — `col` is the visual column to keep (sticky column held across consecutive j/k).
pub fn move_vertically(
    text: &Rope,
    r: Range,
    dir: Direction,
    count: usize,
    extend: bool,
    col: usize,
    tab: usize,
) -> Range {
    let line = line_of(text, r.cursor(text));
    let target = match dir {
        Direction::Backward => line.saturating_sub(count),
        Direction::Forward => (line + count).min(last_line(text).max(line)),
    };
    r.put_cursor(text, pos_at_col(text, target, col, tab), extend)
}

// ── Word motion (helix semantics: motion is selection) ───────────────────────

/// `w` — select up to the next word start (current word + trailing whitespace).
pub fn next_word_start(text: &Rope, r: Range, count: usize, extend: bool) -> Range {
    let len = text.len_bytes();
    let mut out = r;
    for _ in 0..count {
        if len == 0 {
            break;
        }
        let mut p = clamp_char(text, out.cursor(text));
        let nx = next_char(text, p);
        if nx >= len {
            break;
        }
        if class_at(text, p) != class_at(text, nx) {
            p = nx;
        }
        while p < len && class_at(text, p) == CharClass::Eol {
            p = next_char(text, p);
        }
        if p >= len {
            break;
        }
        let anchor = p;
        let c = class_at(text, p);
        if c != CharClass::Whitespace {
            while p < len && class_at(text, p) == c {
                p = next_char(text, p);
            }
        }
        while p < len && class_at(text, p) == CharClass::Whitespace {
            p = next_char(text, p);
        }
        out = Range::new(if extend { out.anchor } else { anchor }, p);
    }
    out
}

/// `e` — select up to the next word end (leading whitespace + word).
pub fn next_word_end(text: &Rope, r: Range, count: usize, extend: bool) -> Range {
    let len = text.len_bytes();
    let mut out = r;
    for _ in 0..count {
        if len == 0 {
            break;
        }
        let mut p = clamp_char(text, out.cursor(text));
        let nx = next_char(text, p);
        if nx >= len {
            break;
        }
        if class_at(text, p) != class_at(text, nx) {
            p = nx;
        }
        while p < len && class_at(text, p) == CharClass::Eol {
            p = next_char(text, p);
        }
        if p >= len {
            break;
        }
        let anchor = p;
        while p < len && class_at(text, p) == CharClass::Whitespace {
            p = next_char(text, p);
        }
        if p < len {
            let c = class_at(text, p);
            while p < len && class_at(text, p) == c {
                p = next_char(text, p);
            }
        }
        out = Range::new(if extend { out.anchor } else { anchor }, p);
    }
    out
}

/// `b` — select backward to the previous word start.
pub fn prev_word_start(text: &Rope, r: Range, count: usize, extend: bool) -> Range {
    let mut out = r;
    for _ in 0..count {
        if text.len_bytes() == 0 {
            break;
        }
        let mut p = clamp_char(text, out.cursor(text));
        if p == 0 {
            break;
        }
        let pv = prev_char(text, p);
        if class_at(text, p) != class_at(text, pv) {
            p = pv;
        }
        while p > 0 && class_at(text, p) == CharClass::Eol {
            p = prev_char(text, p);
        }
        let anchor = graphemes::next_boundary(text, p);
        while p > 0 && class_at(text, p) == CharClass::Whitespace {
            p = prev_char(text, p);
        }
        let c = class_at(text, p);
        while p > 0 && class_at(text, prev_char(text, p)) == c {
            p = prev_char(text, p);
        }
        out = Range::new(if extend { out.anchor } else { anchor }, p);
    }
    out
}

// ── Char find (f t F T) ──────────────────────────────────────────────────────

/// Up to (inclusive) the `count`th `ch`, or just before it (till). Unchanged if not found.
/// till searches from one char further — so repeating (`A-.`) doesn't get stuck in place (as helix does).
pub fn find_char(
    text: &Rope,
    r: Range,
    ch: char,
    forward: bool,
    inclusive: bool,
    count: usize,
    extend: bool,
) -> Range {
    let len = text.len_bytes();
    if len == 0 {
        return r;
    }
    let cur = clamp_char(text, r.cursor(text));
    let found = if forward {
        let mut start = next_char(text, cur);
        if !inclusive {
            start = next_char(text, start);
        }
        if start >= len {
            return r;
        }
        find_bytes(text, start, len, ch, count, true)
    } else {
        // Exclusive upper end of the search: F starts at the char before the cursor, T one before.
        let mut end = cur;
        if !inclusive {
            if end == 0 {
                return r;
            }
            end = prev_char(text, end);
        }
        find_bytes(text, 0, end, ch, count, false)
    };
    // Find by codepoint, but the selection end on a grapheme boundary — don't split 👍 off 👍🏽.
    let Some(p) = found else { return r };
    let target = match (forward, inclusive) {
        (_, true) => p,
        (true, false) => graphemes::prev_boundary(text, p),
        (false, false) => graphemes::next_boundary(text, p),
    };
    if extend {
        r.put_cursor(text, target, true)
    } else if forward {
        Range::new(cur, graphemes::next_boundary(text, target))
    } else {
        Range::new(graphemes::next_boundary(text, cur), target)
    }
}

/// The `count`th `ch` in `from..to` (forward from `from`, or back from `to`) — its UTF-8 bytes looked for in
/// the rope's chunks with `memchr`'s SIMD search: a miss read the rest of the document char by char (31 ms on
/// 200k lines). A char's bytes can't match inside another char's, and a chunk never splits a char.
fn find_bytes(text: &Rope, from: usize, to: usize, ch: char, count: usize, forward: bool) -> Option<usize> {
    // (on char boundaries — the slices below would panic mid-char, where reading chars never did)
    let snap = |b: usize| text.char_to_byte(text.byte_to_char(b.min(text.len_bytes())));
    let (from, to) = (snap(from), snap(to));
    let mut buf = [0u8; 4];
    let needle = ch.encode_utf8(&mut buf).as_bytes();
    let mut hits = 0;
    let mut hit = || {
        hits += 1;
        hits == count
    };
    if forward {
        let (chunks, mut base, _, _) = text.byte_slice(from..to).chunks_at_byte(0);
        base += from;
        for chunk in chunks {
            if let Some(i) = memchr::memmem::find_iter(chunk.as_bytes(), needle).find(|_| hit()) {
                return Some(base + i);
            }
            base += chunk.len();
        }
    } else {
        let slice = text.byte_slice(from..to);
        let (mut chunks, _, _, _) = slice.chunks_at_byte(slice.len_bytes());
        let mut end = to;
        while let Some(chunk) = chunks.prev() {
            let base = end - chunk.len();
            if let Some(i) = memchr::memmem::rfind_iter(chunk.as_bytes(), needle).find(|_| hit()) {
                return Some(base + i);
            }
            end = base;
        }
    }
    None
}

// ── Line selection ───────────────────────────────────────────────────────────

/// `x` — select the whole line; if already whole lines, extend down one line at a time.
pub fn extend_line_below(text: &Rope, r: Range, count: usize) -> Range {
    let (start_line, end_line) = line_span(text, r);
    let start = line_start(text, start_line);
    let r1 = r.min_width_1(text);
    let full = r1.from() == start && r1.to() == line_full_end(text, end_line);
    let grow = if full { count } else { count - 1 };
    let end_line = (end_line + grow).min(last_line(text).max(end_line));
    Range::new(start, line_full_end(text, end_line))
}

/// `extend_line_above` — select the whole line; if already whole lines, extend upward (backward).
pub fn extend_line_above(text: &Rope, r: Range, count: usize) -> Range {
    let (start_line, end_line) = line_span(text, r);
    let end = line_full_end(text, end_line);
    let r1 = r.min_width_1(text);
    let full = r1.from() == line_start(text, start_line) && r1.to() == end;
    let grow = if full { count } else { count - 1 };
    Range::new(end, line_start(text, start_line.saturating_sub(grow)))
}

/// `X` — widen to the bounds of the spanned lines (direction kept).
pub fn extend_to_line_bounds(text: &Rope, r: Range) -> Range {
    let (start_line, end_line) = line_span(text, r);
    let (from, to) = (line_start(text, start_line), line_full_end(text, end_line));
    if r.is_forward() { Range::new(from, to) } else { Range::new(to, from) }
}

/// `C` — copy a same-shaped range `delta` lines away (by visual column — the same spot on screen even if
/// char widths differ per line). None if out of bounds.
pub fn copy_on_line(text: &Rope, r: Range, delta: isize, tab: usize) -> Option<Range> {
    let map = |pos: usize| -> Option<usize> {
        let target = line_of(text, pos) as isize + delta;
        if target < 0 || target > last_line(text) as isize {
            return None;
        }
        Some(pos_at_col(text, target as usize, visual_col(text, pos, tab), tab))
    };
    if r.is_empty() {
        return map(r.head).map(Range::point);
    }
    // Move the end bound by visual column too — so one wide char (2 cells) covers two ASCII chars.
    let first = map(r.from())?;
    let end = map(r.to())?.max(graphemes::next_boundary(text, first));
    Some(if r.is_forward() { Range::new(first, end) } else { Range::new(end, first) })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `visual_col` and `pos_at_col` (16-byte blocks) give what walking the cells gives — at every position of
    /// random multi-line text spanning several rope chunks.
    #[test]
    fn columns_match_walking_the_cells() {
        use crate::graphemes::cells;
        for seed in 1..8 {
            let t = Rope::from_str(&crate::graphemes::tests::random_text(seed, 1500, true));
            let mut pos = 0;
            while pos <= t.len_bytes() {
                let start = line_start(&t, line_of(&t, pos));
                let s: String = t.byte_slice(start..pos).into();
                let want = cells(&s, start, 4).last().map_or(0, |c| c.col + c.width);
                assert_eq!(visual_col(&t, pos, 4), want, "at {pos}");
                if pos == t.len_bytes() {
                    break;
                }
                pos = next_char(&t, pos);
            }
            for line in 0..last_line(&t).min(40) {
                let (start, end) = (line_start(&t, line), line_end(&t, line));
                let s: String = t.byte_slice(start..end).into();
                for col in 0..visual_col(&t, end, 4) + 3 {
                    let want = cells(&s, start, 4).find(|c| c.col + c.width > col).map_or(end, |c| c.pos);
                    assert_eq!(pos_at_col(&t, line, col, 4), want, "line {line} col {col}");
                }
            }
        }
    }

    /// Ends inside a multi-byte char (none should come, but a crash is the wrong answer) are taken from that
    /// char's start.
    #[test]
    fn find_bytes_from_inside_a_char() {
        let t = Rope::from_str("a한b한c");
        assert_eq!(find_bytes(&t, 0, 3, '한', 1, false), None); // (up to inside the first 한: before it)
        assert_eq!(find_bytes(&t, 2, t.len_bytes(), 'b', 1, true), Some(4));
    }

    /// `f`/`t`/`F`/`T` by bytes in chunks find what reading chars one by one found.
    #[test]
    fn find_char_matches_reading_chars() {
        fn reference(
            text: &Rope,
            from: usize,
            to: usize,
            ch: char,
            count: usize,
            forward: bool,
        ) -> Option<usize> {
            let mut hits = 0;
            if forward {
                let mut pos = from;
                for c in text.byte_slice(from..to).chars() {
                    if c == ch {
                        hits += 1;
                        if hits == count {
                            return Some(pos);
                        }
                    }
                    pos += c.len_utf8();
                }
            } else {
                let mut it = text.byte_slice(from..to).chars_at(text.byte_slice(from..to).len_chars());
                let mut pos = to;
                while let Some(c) = it.prev() {
                    pos -= c.len_utf8();
                    if c == ch {
                        hits += 1;
                        if hits == count {
                            return Some(pos);
                        }
                    }
                }
            }
            None
        }
        for seed in 1..6 {
            let t = Rope::from_str(&crate::graphemes::tests::random_text(seed, 3000, true));
            let len = t.len_bytes();
            let mut from = 0;
            while from < len {
                for ch in ['a', '(', '한', '\u{301}', '\n', 'Q'] {
                    for count in 1..4 {
                        assert_eq!(
                            find_bytes(&t, from, len, ch, count, true),
                            reference(&t, from, len, ch, count, true)
                        );
                        assert_eq!(
                            find_bytes(&t, 0, from, ch, count, false),
                            reference(&t, 0, from, ch, count, false)
                        );
                    }
                }
                for _ in 0..37 {
                    from = next_char(&t, from).min(len);
                }
            }
        }
    }

    fn rope(s: &str) -> Rope {
        Rope::from_str(s)
    }

    #[test]
    fn lines() {
        let t = rope("ab\n\ncd\n");
        assert_eq!(last_line(&t), 2);
        assert_eq!(line_end(&t, 0), 2);
        assert_eq!(line_full_end(&t, 0), 3);
        assert_eq!(line_end(&t, 1), 3);
        assert_eq!(line_full_end(&t, 2), 7);
        assert_eq!(last_line(&rope("ab")), 0);
        assert_eq!(last_line(&rope("")), 0);
        assert_eq!(line_end(&rope("a\r\nb"), 0), 1);
        // Bytes: "한\n" = 3 + 1
        assert_eq!(line_end(&rope("한\n글"), 0), 3);
        assert_eq!(line_start(&rope("한\n글"), 1), 4);
    }

    #[test]
    fn word_motions() {
        let t = rope("foo bar.baz\nqux");
        let w = |r| next_word_start(&t, r, 1, false);
        let r = w(Range::point(0));
        assert_eq!(r, Range::new(0, 4)); // "foo "
        let r = w(r);
        assert_eq!(r, Range::new(4, 7)); // "bar"
        let r = w(r);
        assert_eq!(r, Range::new(7, 8)); // "."
        let r = w(r);
        assert_eq!(r, Range::new(8, 11)); // "baz"
        let r = w(r);
        assert_eq!(r, Range::new(12, 15)); // "qux" across the newline

        let e = next_word_end(&t, Range::point(0), 1, false);
        assert_eq!(e, Range::new(0, 3));
        let e = next_word_end(&t, e, 1, false);
        assert_eq!(e, Range::new(3, 7)); // " bar"

        let b = prev_word_start(&t, Range::point(4), 1, false);
        assert_eq!(b, Range::new(4, 0)); // "foo "
        let b = prev_word_start(&t, Range::point(5), 1, false);
        assert_eq!(b, Range::new(6, 4)); // "ba"
        let b = prev_word_start(&t, Range::point(12), 1, false);
        assert_eq!(b, Range::new(11, 8)); // "baz" across the newline (newline excluded)
    }

    #[test]
    fn hangul_words_in_bytes() {
        let t = rope("타래 편집기");
        let r = next_word_start(&t, Range::point(0), 1, false);
        assert_eq!(r, Range::new(0, 7)); // "타래 " = 6 + 1 bytes
        assert_eq!(next_word_start(&t, r, 1, false), Range::new(7, 16)); // "편집기"
        assert_eq!(prev_word_start(&t, Range::point(10), 1, false), Range::new(13, 7)); // "편집"
    }

    #[test]
    fn word_bounds_keep_clusters_whole() {
        // NFD é = e + U+0301 (3..6) — the mark belongs to the word, not a punctuation run
        let t = rope("cafe\u{301} x");
        assert_eq!(next_word_start(&t, Range::point(0), 1, false), Range::new(0, 7));
        assert_eq!(next_word_end(&t, Range::point(0), 1, false), Range::new(0, 6));
        assert_eq!(prev_word_start(&t, Range::point(3), 1, false), Range::new(6, 0));
    }

    #[test]
    fn vertical_clamps_to_line_end() {
        let t = rope("abcdef\nab\nabcdef");
        let r = move_vertically(&t, Range::point(5), Direction::Forward, 1, false, 5, 4);
        assert_eq!(r, Range::point(9)); // on the short line's newline
        let r = move_vertically(&t, r, Direction::Forward, 5, false, 5, 4);
        assert_eq!(r, Range::point(15)); // sticky column 5 kept
    }

    #[test]
    fn visual_columns_with_wide_chars_and_tabs() {
        let t = rope("한글자\n\tab\nabcdef");
        assert_eq!(visual_col(&t, 3, 4), 2); // before '글' (3 B) = 2 cells
        assert_eq!(visual_col(&t, 11, 4), 4); // 'a' after the tab
        assert_eq!(pos_at_col(&t, 2, 2, 4), 16); // 'c'
        assert_eq!(pos_at_col(&t, 0, 3, 4), 3); // cell 3 is the right half of '글'
    }

    #[test]
    fn find_char_multibyte() {
        let t = rope("가,나,다");
        // '가'(3) ','(1) '나'(3) ','(1) '다'(3)
        assert_eq!(find_char(&t, Range::point(0), ',', true, true, 2, false), Range::new(0, 8));
        assert_eq!(find_char(&t, Range::point(8), ',', false, true, 1, false), Range::new(11, 7));
        assert_eq!(find_char(&t, Range::point(0), '다', true, false, 1, false), Range::new(0, 8));
        // Finding by a cluster's first codepoint still selects the whole cluster (a 1 + 👍 4 + 🏽 4)
        let t = rope("a👍🏽b");
        assert_eq!(find_char(&t, Range::point(0), '👍', true, true, 1, false), Range::new(0, 9));
    }

    #[test]
    fn line_selection() {
        let t = rope("a\nb\nc\n");
        let r = extend_line_below(&t, Range::point(0), 1);
        assert_eq!(r, Range::new(0, 2));
        let r = extend_line_below(&t, r, 1);
        assert_eq!(r, Range::new(0, 4));
        let r = extend_line_below(&t, r, 5);
        assert_eq!(r, Range::new(0, 6)); // doesn't go to the ghost line
        let r = extend_line_above(&t, Range::point(4), 1);
        assert_eq!(r, Range::new(6, 4));
        let r = extend_line_above(&t, r, 1);
        assert_eq!(r, Range::new(6, 2));
    }

    #[test]
    fn copy_on_next_line() {
        let t = rope("abc\nab\nabc");
        assert_eq!(copy_on_line(&t, Range::point(2), 1, 4), Some(Range::point(6)));
        assert_eq!(copy_on_line(&t, Range::point(2), 3, 4), None);
        assert_eq!(copy_on_line(&t, Range::new(0, 2), 2, 4), Some(Range::new(7, 9)));
        // Wide-char line → ASCII line: same spot on screen ('글' = cells 2–3 → 'c','d')
        let t = rope("한글\nabcd");
        assert_eq!(copy_on_line(&t, Range::new(3, 6), 1, 4), Some(Range::new(9, 11)));
    }
}
