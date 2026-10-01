//! Grapheme clusters — units seen as one char (`é` = e + ◌́, NFD Hangul = ᄒ+ᅡ+ᆫ, 👍🏽, 👨‍👩‍👧, `\r\n`).
//! Cursor motion, "char under the cursor", deletion and drawing follow this unit. Positions are byte offsets.
//!
//! Boundaries via `GraphemeCursor` over rope chunks — no copying whole lines (safe even for a 10 MB line).

use ropey::Rope;
use unicode_segmentation::{GraphemeCursor, GraphemeIncomplete};
use unicode_width::UnicodeWidthStr;

/// Next cluster boundary after byte `pos` (document end if at the end).
pub fn next_boundary(text: &Rope, pos: usize) -> usize {
    let len = text.len_bytes();
    if pos >= len {
        return len;
    }
    let (mut chunk, mut chunk_start, _, _) = text.chunk_at_byte(pos);
    if let Some(n) = ascii_cluster(&chunk.as_bytes()[pos - chunk_start..], pos + 1 == len) {
        return pos + n;
    }
    let mut gc = GraphemeCursor::new(pos, len, true);
    loop {
        match gc.next_boundary(chunk, chunk_start) {
            Ok(None) => return len,
            Ok(Some(b)) => return b,
            Err(GraphemeIncomplete::NextChunk) => {
                chunk_start += chunk.len();
                chunk = text.chunk_at_byte(chunk_start).0;
            }
            Err(GraphemeIncomplete::PreContext(n)) => {
                let ctx = text.chunk_at_byte(n - 1).0;
                gc.provide_context(ctx, n - ctx.len());
            }
            Err(_) => return next_char(text, pos),
        }
    }
}

/// Start of the cluster at byte `pos` (clamped to the text) — for positions kept across a wholesale text
/// change (undo, reload), which may now fall mid-character.
pub fn snap(text: &Rope, pos: usize) -> usize {
    let pos = text.char_to_byte(text.byte_to_char(pos.min(text.len_bytes())));
    if pos < text.len_bytes() { prev_boundary(text, next_boundary(text, pos)) } else { pos }
}

/// Previous cluster boundary before byte `pos` (0 if at the start).
pub fn prev_boundary(text: &Rope, pos: usize) -> usize {
    let pos = pos.min(text.len_bytes());
    if pos == 0 {
        return 0;
    }
    let (mut chunk, mut chunk_start, _, _) = text.chunk_at_byte(pos);
    // An ASCII char after another ASCII char (or at the start) is a cluster of its own, but for CR LF
    let i = pos - chunk_start;
    if i >= 2 || (i == 1 && pos == 1) {
        let b = chunk.as_bytes();
        let (last, before) = (b[i - 1], if pos == 1 { 0 } else { b[i - 2] });
        if last.is_ascii() && before.is_ascii() && !(before == b'\r' && last == b'\n') {
            return pos - 1;
        }
    }
    let mut gc = GraphemeCursor::new(pos, text.len_bytes(), true);
    loop {
        match gc.prev_boundary(chunk, chunk_start) {
            Ok(None) => return 0,
            Ok(Some(b)) => return b,
            Err(GraphemeIncomplete::PrevChunk) => {
                let (c, s, _, _) = text.chunk_at_byte(chunk_start - 1);
                chunk = c;
                chunk_start = s;
            }
            Err(GraphemeIncomplete::PreContext(n)) => {
                let ctx = text.chunk_at_byte(n - 1).0;
                gc.provide_context(ctx, n - ctx.len());
            }
            Err(_) => return prev_char(text, pos),
        }
    }
}

/// One codepoint step (for places that look at char kinds, like word motion and char find).
pub fn next_char(text: &Rope, pos: usize) -> usize {
    if pos >= text.len_bytes() { text.len_bytes() } else { pos + char_at(text, pos).len_utf8() }
}

pub fn prev_char(text: &Rope, pos: usize) -> usize {
    let c = text.byte_to_char(pos.min(text.len_bytes()));
    if c == 0 { 0 } else { text.char_to_byte(c - 1) }
}

/// The char starting at `pos` (a char boundary).
pub fn char_at(text: &Rope, pos: usize) -> char {
    text.char(text.byte_to_char(pos))
}

/// Screen cells a cluster takes. Tab runs to the next tab stop, newline is one cell (for selection display).
/// Multi-codepoint emoji (ZWJ, skin tone) are drawn two cells wide by terminals, so cap at 2.
pub fn cluster_width(g: &str, col: usize, tab: usize) -> usize {
    match g {
        "\t" => tab - col % tab,
        "\n" | "\r\n" => 1,
        "\r" => 0,
        _ => {
            let mut chars = g.chars();
            let first = chars.next().unwrap_or(' ');
            if chars.next().is_none() {
                if first.is_control() { 1 } else { g.width() }
            } else {
                g.width().min(2)
            }
        }
    }
}

/// A line (or its head) split into screen cells. `pos` is the document byte position, `len` the cluster
/// byte length, `bytes` the range within the given string `s`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cell {
    pub pos: usize,
    pub len: usize,
    pub col: usize,
    pub width: usize,
    pub bytes: std::ops::Range<usize>,
}

/// `s` is the text from the line start, `first` the document position of its first byte. Lazy — a caller
/// that stops at the screen edge doesn't segment the rest.
pub fn cells(s: &str, first: usize, tab: usize) -> impl Iterator<Item = Cell> + '_ {
    cells_from(s, first, 0, tab)
}

/// `cells` of a piece of a line that starts at display column `col` (tab stops count from the line start).
pub fn cells_from(s: &str, first: usize, col: usize, tab: usize) -> impl Iterator<Item = Cell> + '_ {
    let (mut b, mut col) = (0, col);
    std::iter::from_fn(move || {
        let rest = &s.as_bytes()[b..];
        let len = match ascii_cluster(rest, rest.len() == 1) {
            Some(n) => n,
            None if rest.is_empty() => return None,
            None => {
                GraphemeCursor::new(b, s.len(), true).next_boundary(s, 0).ok().flatten().unwrap_or(s.len())
                    - b
            }
        };
        let width = match rest[0] {
            b'\t' if len == 1 => tab - col % tab,
            b'\r' if len == 1 => 0,
            c if c.is_ascii() => 1,
            _ => cluster_width(&s[b..b + len], col, tab),
        };
        let cell = Cell { pos: first + b, len, col, width, bytes: b..b + len };
        (b, col) = (b + len, col + width);
        Some(cell)
    })
}

/// Where screen column `target` falls in `s` — a piece of a line that starts at column `col` — as the byte
/// and column of the cell holding it (`s.len()` and the end column when `s` ends first; `usize::MAX` walks
/// all of `s`). What `cells_from(..).find(|c| c.col + c.width > target)` gives, without a cell per char:
/// printable ASCII goes 16 bytes at a time (a check the compiler turns into SIMD compares) — a 5 MB
/// minified line took 20 ms a key cell by cell. A block is only skipped when the byte after it is ASCII too:
/// a non-ASCII one could make a cluster with the block's last char.
pub fn seek(s: &str, col: usize, tab: usize, target: usize) -> (usize, usize) {
    let b = s.as_bytes();
    let (mut i, mut col) = (0, col);
    loop {
        while let Some(block) = b.get(i..i + 16)
            && col + 16 <= target
            && printable(block)
            && b.get(i + 16).is_none_or(u8::is_ascii)
        {
            (i, col) = (i + 16, col + 16);
        }
        let Some(c) = cells_from(&s[i..], 0, col, tab).next() else { return (i, col) };
        if c.col + c.width > target {
            return (i, col);
        }
        (i, col) = (i + c.len, col + c.width);
    }
}

/// All 16 bytes printable ASCII (a cell each) — no early exit, so it compiles to a few vector compares.
fn printable(block: &[u8]) -> bool {
    block.iter().fold(true, |all, &c| all & (c.wrapping_sub(0x20) < 0x5f))
}

/// Length of the cluster at the head of `rest` when that's plain ASCII: an ASCII char followed by another
/// (or ending the text — `at_end`) is a cluster of its own, but for CR LF. Most code never reaches the
/// Unicode tables this way. `None` when the next char isn't ASCII (it may extend this one) or `rest` is empty.
fn ascii_cluster(rest: &[u8], at_end: bool) -> Option<usize> {
    match *rest {
        [c, ..] if !c.is_ascii() => None,
        [b'\r', b'\n', ..] => Some(2),
        [_, n, ..] if n.is_ascii() => Some(1),
        [_] if at_end => Some(1),
        _ => None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Random text from pieces that stress cells: ASCII runs long enough for 16-byte blocks, tabs, CR LF,
    /// combining marks after ASCII, keycaps, ZWJ families, flags, Hangul, zero-width, brackets (xorshift, fixed
    /// seed — the same text every run).
    pub(crate) fn random_text(seed: u64, pieces: usize, lines: bool) -> String {
        const P: &[&str] = &[
            "abcdefghijklmnopqrstuvwxyz0123456789",
            "a",
            "b",
            " ",
            "\t",
            "\r\n",
            "e\u{301}",
            "한",
            "👍🏽",
            "👨\u{200d}👩\u{200d}👧",
            "🇰🇷",
            "🇰",
            "1\u{fe0f}\u{20e3}",
            "\u{200b}",
            "(",
            ")",
            "[",
            "]",
            "{",
            "}",
            "x",
            "yz",
            "é",
            "\u{301}",
            "~~~~~~~~~~~~~~~~~~",
            "\r",
        ];
        let mut x = seed.max(1);
        let mut out = String::new();
        for _ in 0..pieces {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let k = (x % (P.len() as u64 + u64::from(lines))) as usize;
            out.push_str(if k == P.len() { "\n" } else { P[k] });
        }
        out
    }

    /// `seek` lands where walking the cells one by one does — for every column, from any starting column:
    /// random text, and every alignment of a 16-byte block against clusters that start with an ASCII char
    /// (`a` + a spacing mark is one cell together, two apart — a block must not end inside it).
    #[test]
    fn seek_matches_walking_the_cells() {
        let edges = ["a\u{93f}", "1\u{fe0f}\u{20e3}", "e\u{301}", "🇰🇷", "a\u{200d}b", "\r\n"];
        let aligned = (0..34).flat_map(|k| edges.map(|e| format!("{}{e}{}", "a".repeat(k), "b".repeat(20))));
        let random = (1..60).map(|seed| random_text(seed, 120, false));
        for s in aligned.chain(random) {
            for col in [0, 3] {
                let cells: Vec<Cell> = cells_from(&s, 0, col, 8).collect();
                let total = cells.last().map_or(col, |c| c.col + c.width);
                assert_eq!(seek(&s, col, 8, usize::MAX), (s.len(), total), "{s:?}");
                for target in 0..total + 3 {
                    let want = cells
                        .iter()
                        .find(|c| c.col + c.width > target)
                        .map_or((s.len(), total), |c| (c.bytes.start, c.col));
                    assert_eq!(seek(&s, col, 8, target), want, "target {target} in {s:?}");
                }
            }
        }
    }

    #[test]
    fn boundaries_skip_whole_clusters() {
        let t = Rope::from_str("e\u{301}x👍🏽\r\n한");
        // Bytes: é(1+2) x(1) 👍🏽(4+4) \r\n(2) 한(3)
        let mut walk = vec![0];
        while *walk.last().unwrap() < t.len_bytes() {
            walk.push(next_boundary(&t, *walk.last().unwrap()));
        }
        assert_eq!(walk, vec![0, 3, 4, 12, 14, 17]);
        let mut back = vec![t.len_bytes()];
        while *back.last().unwrap() > 0 {
            back.push(prev_boundary(&t, *back.last().unwrap()));
        }
        assert_eq!(back, vec![17, 14, 12, 4, 3, 0]);
    }

    #[test]
    fn boundaries_across_rope_chunks() {
        // Many ZWJ family emoji (18 B) so some straddle chunk boundaries
        let family = "👨\u{200d}👩\u{200d}👧";
        let t = Rope::from_str(&family.repeat(700));
        let (mut pos, mut steps) = (0, 0);
        while pos < t.len_bytes() {
            let next = next_boundary(&t, pos);
            assert_eq!(next - pos, family.len(), "at {pos}");
            pos = next;
            steps += 1;
        }
        assert_eq!(steps, 700);
        assert_eq!(prev_boundary(&t, t.len_bytes()), t.len_bytes() - family.len());
    }

    #[test]
    fn cell_widths() {
        let s = "👨\u{200d}👩\u{200d}👧x\t한e\u{301}";
        let w: Vec<usize> = cells(s, 0, 4).map(|c| c.width).collect();
        assert_eq!(w, vec![2, 1, 1, 2, 1]); // tab is col 3 → 4
        let pos: Vec<usize> = cells(s, 10, 4).map(|c| c.pos).collect();
        assert_eq!(pos, vec![10, 28, 29, 30, 33]); // family emoji 18 B, x 1, tab 1, 한 3
    }

    /// A wrapped row read from its own start lays out like that part of the whole line — tab stops still
    /// count from the line start.
    #[test]
    fn cells_from_a_row_start_continue_the_line() {
        let s = "ab\tc한\td👍🏽e\t\tf";
        let whole: Vec<Cell> = cells(s, 100, 4).collect();
        for (i, c) in whole.iter().enumerate() {
            let k = c.bytes.start;
            let tail: Vec<Cell> = cells_from(&s[k..], 100 + k, c.col, 4)
                .map(|t| Cell { bytes: t.bytes.start + k..t.bytes.end + k, ..t })
                .collect();
            assert_eq!(tail, whole[i..], "from byte {k}");
        }
    }

    /// The ASCII shortcuts agree with the Unicode rules: ASCII next to a combining mark, CR LF, a lone CR, a
    /// ZWJ or skin tone after ASCII, regional indicators, Hangul jamo, controls — at every position.
    #[test]
    fn ascii_shortcuts_match_the_unicode_rules() {
        use unicode_segmentation::UnicodeSegmentation;
        let s = "ab\r\nc\re\u{301}f\u{200d}g🇰🇷h👍🏽\ti\u{1}ᄒ\u{1161}\u{11ab}j\r\r\n\n\u{600}k한 x\u{308}";
        let want: Vec<(usize, &str)> = s.grapheme_indices(true).collect();
        let got: Vec<(usize, &str)> = cells(s, 0, 4).map(|c| (c.bytes.start, &s[c.bytes])).collect();
        assert_eq!(got, want);
        let widths: Vec<usize> = cells(s, 0, 4).map(|c| c.width).collect();
        let mut col = 0;
        let slow: Vec<usize> = want
            .iter()
            .map(|(_, g)| {
                let w = cluster_width(g, col, 4);
                col += w;
                w
            })
            .collect();
        assert_eq!(widths, slow);
        let t = Rope::from_str(s);
        let starts: Vec<usize> = want.iter().map(|w| w.0).chain([s.len()]).collect();
        for pair in starts.windows(2) {
            assert_eq!(next_boundary(&t, pair[0]), pair[1], "next from {}", pair[0]);
            assert_eq!(prev_boundary(&t, pair[1]), pair[0], "prev from {}", pair[1]);
        }
    }
}
