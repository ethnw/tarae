//! What the terminal shows, cell by cell — so a frame writes only what changed.
//!
//! `render` still draws the whole screen, as escapes, into a buffer. Here that frame is read back into a grid
//! of cells — tarae's own few escapes: cursor moves, SGR, erase to the line's end, cursor shape and
//! visibility — and compared with the grid the terminal shows. What goes out is made from the grid: in each
//! row, the stretch from a changed cell to the last changed one. Typing, a key is a handful of cells rather
//! than a 24 KB screen (estimated from recorded sessions: 13% of the bytes typing, 5% saving, 30% in a picker).
//!
//! A frame with anything else (another escape, a control character, text past the edge, a cluster too long
//! to keep) goes out as it is, and the next one redraws everything. So does the first frame, a resize, focus
//! coming back, `:redraw`, and input resting after a write in parts — which is how the screen heals when
//! something else wrote to the terminal, the terminal measured a character differently, or left something
//! of its own behind (Terminal.app: a cursor drawn past its cell).

use crate::graphemes::cells_from;

/// A color as SGR gives it — default, one of 256, or RGB — as 4 bytes, so cells compare as plain bytes
/// (comparing every cell of two screens is a good part of a frame's cost).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Color([u8; 4]);

impl Color {
    const DEFAULT: Color = Color([0; 4]);

    fn idx(n: u8) -> Color {
        Color([1, n, 0, 0])
    }

    fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color([2, r, g, b])
    }
}

const BOLD: u8 = 1;
const DIM: u8 = 2;
const ITALIC: u8 = 4;
const REVERSE: u8 = 8;
const CROSSED: u8 = 16;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Style {
    fg: Color,
    bg: Color,
    /// Underline color (`58`)
    ul: Color,
    attrs: u8,
    /// `4:n` — 0 none, 1 single, 3 curly…
    underline: u8,
}

const PLAIN: Style =
    Style { fg: Color::DEFAULT, bg: Color::DEFAULT, ul: Color::DEFAULT, attrs: 0, underline: 0 };

/// The longest cluster a cell keeps — a longer one (a family emoji and such) sends its frame out as it is.
const TEXT: usize = 14;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Cell {
    text: [u8; TEXT],
    len: u8,
    /// 1 or 2 — 0 for the right half of a wide character
    width: u8,
    style: Style,
}

impl Cell {
    fn blank(style: Style) -> Cell {
        let mut text = [0; TEXT];
        text[0] = b' ';
        Cell { text, len: 1, width: 1, style }
    }

    fn ascii(b: u8, style: Style) -> Cell {
        let mut text = [0; TEXT];
        text[0] = b;
        Cell { text, len: 1, width: 1, style }
    }

    fn text(&self) -> &[u8] {
        &self.text[..self.len as usize]
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
struct Grid {
    w: usize,
    h: usize,
    cells: Vec<Cell>,
    /// Where the cursor shows — None: hidden
    cursor: Option<(usize, usize)>,
    /// `CSI n SP q`
    shape: u8,
    /// Kept only while a frame with cards is read (`seat`): per cell, the background it had before a half
    /// block (`▄`/`▀`) was drawn over it
    under: Option<Vec<Option<Color>>>,
}

impl Grid {
    fn new(w: usize, h: usize) -> Grid {
        Grid { w, h, cells: vec![Cell::blank(PLAIN); w * h], cursor: None, shape: 0, under: None }
    }

    fn row(&self, y: usize) -> &[Cell] {
        &self.cells[y * self.w..(y + 1) * self.w]
    }

    /// Put `cell` at (x, y) as a terminal would (xterm's way): a wide character it cuts in half leaves a
    /// blank half, in the style being written.
    fn put(&mut self, x: usize, y: usize, cell: Cell) {
        let i = y * self.w + x;
        if self.cells[i].width == 0 && x > 0 {
            self.cells[i - 1] = Cell::blank(cell.style);
        }
        if self.cells[i].width == 2 && x + 1 < self.w {
            self.cells[i + 1] = Cell::blank(cell.style);
        }
        // (the same half block drawn again keeps what was under the first)
        if let Some(under) = self.under.as_mut()
            && HALVES.contains(&cell.text())
            && self.cells[i] != cell
        {
            under[i] = Some(self.cells[i].style.bg);
        }
        self.cells[i] = cell;
        if cell.width == 2 {
            if self.cells[i + 1].width == 2 && x + 2 < self.w {
                self.cells[i + 2] = Cell::blank(cell.style);
            }
            self.cells[i + 1] = Cell { len: 0, width: 0, ..cell };
        }
    }

    /// Draw `frame` over what's there. None: it has something this doesn't read (the grid is then partly
    /// drawn — the caller throws it away).
    fn apply(&mut self, frame: &[u8]) -> Option<()> {
        let (mut pos, mut style, mut visible) = (None::<(usize, usize)>, PLAIN, self.cursor.is_some());
        let mut i = 0;
        while i < frame.len() {
            let b = frame[i];
            if b == 0x1b {
                if frame.get(i + 1) != Some(&b'[') {
                    return None;
                }
                // CSI: parameters 0x30–0x3F, intermediates 0x20–0x2F, a final byte
                let p = i + 2;
                let mut j = p;
                while j < frame.len() && (0x30..=0x3f).contains(&frame[j]) {
                    j += 1;
                }
                let params = &frame[p..j];
                let k = j;
                while j < frame.len() && (0x20..=0x2f).contains(&frame[j]) {
                    j += 1;
                }
                let inter = &frame[k..j];
                let fin = *frame.get(j)?;
                i = j + 1;
                match (fin, inter, params) {
                    (b'H', b"", _) => {
                        // `row;col`, either left out for 1 — anything else (`?`, a third one) isn't ours
                        let mut rc = [1u32; 2];
                        for (k, p) in params.split(|&c| c == b';').enumerate() {
                            if !p.is_empty() {
                                *rc.get_mut(k)? = num(p)?;
                            } else if k >= 2 {
                                return None;
                            }
                        }
                        let (x, y) = (rc[1].max(1) as usize - 1, rc[0].max(1) as usize - 1);
                        if x >= self.w || y >= self.h {
                            return None;
                        }
                        pos = Some((x, y));
                    }
                    (b'K', b"", b"" | b"0") => {
                        let (x, y) = pos?;
                        // (reversed or underlined: terminals differ)
                        if style.attrs & (REVERSE | CROSSED) != 0 || style.underline != 0 {
                            return None;
                        }
                        // (right after the last column there's nothing left to erase — xterm would take the
                        // last cell, but what was drawn is meant to stay, and it's this grid that goes out)
                        // The erased cells take the background (BCE), nothing else
                        let blank = Cell::blank(Style { bg: style.bg, ..PLAIN });
                        for x in x..self.w {
                            self.put(x, y, blank);
                        }
                    }
                    (b'm', b"", _) => sgr(&mut style, params)?,
                    (b'h' | b'l', b"", b"?25") => visible = fin == b'h',
                    (b'h' | b'l', b"", b"?2026") => {}
                    (b'q', b" ", _) => self.shape = num(params)?.min(255) as u8,
                    _ => return None,
                }
                continue;
            }
            if b < 0x20 || b == 0x7f {
                return None;
            }
            // Text up to the next escape
            let end = frame[i..].iter().position(|&c| c < 0x20 || c == 0x7f).map_or(frame.len(), |n| i + n);
            let (mut x, y) = pos?;
            // Plain ASCII is a cell a byte — unless a non-ASCII byte follows, which may combine with it
            while i < end && frame[i] < 0x80 && frame[i + 1..].first().is_none_or(|&n| n < 0x80) {
                if x >= self.w {
                    return None;
                }
                self.put(x, y, Cell::ascii(frame[i], style));
                (x, i) = (x + 1, i + 1);
            }
            let text = std::str::from_utf8(&frame[i..end]).ok()?;
            for c in cells_from(text, 0, x, 8) {
                let g = &text[c.bytes.clone()];
                if !(1..=2).contains(&c.width)
                    || x + c.width > self.w
                    || g.len() > TEXT
                    || g.chars().any(char::is_control)
                {
                    return None;
                }
                let mut cell = Cell { text: [0; TEXT], len: g.len() as u8, width: c.width as u8, style };
                cell.text[..g.len()].copy_from_slice(g.as_bytes());
                self.put(x, y, cell);
                x += c.width;
            }
            pos = Some((x, y)); // (x = w: at the edge — more text would wrap, and is refused above)
            i = end;
        }
        self.cursor = if visible { Some(pos.filter(|&(x, _)| x < self.w)?) } else { None };
        Some(())
    }
}

/// A decimal parameter (empty: None).
fn num(p: &[u8]) -> Option<u32> {
    if p.is_empty() || p.len() > 9 || !p.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(p.iter().fold(0, |n, &d| n * 10 + (d - b'0') as u32))
}

/// Apply `CSI params m` to `style`. None: a parameter this doesn't know.
fn sgr(style: &mut Style, params: &[u8]) -> Option<()> {
    // (on the stack — a frame has a few hundred of these)
    let mut all: [&[u8]; 32] = [&[]; 32];
    let mut n = 0;
    for p in params.split(|&c| c == b';') {
        *all.get_mut(n)? = p;
        n += 1;
    }
    let ps = &all[..n];
    let mut i = 0;
    while i < ps.len() {
        let p = ps[i];
        // Colon subparameters: `4:3` (curly underline), `58:2::r:g:b`
        if p.contains(&b':') {
            let subs: Vec<Option<u32>> = p.split(|&c| c == b':').map(num).collect();
            match subs[..] {
                [Some(4), Some(n)] => style.underline = n.min(255) as u8,
                [Some(58), Some(5), Some(n)] => style.ul = Color::idx(n.min(255) as u8),
                [Some(58), Some(2), None, Some(r), Some(g), Some(b)]
                | [Some(58), Some(2), Some(r), Some(g), Some(b)] => {
                    style.ul = Color::rgb(r.min(255) as u8, g.min(255) as u8, b.min(255) as u8)
                }
                _ => return None,
            }
            i += 1;
            continue;
        }
        let n = if p.is_empty() { 0 } else { num(p)? };
        match n {
            0 => *style = PLAIN,
            1 => style.attrs |= BOLD,
            2 => style.attrs |= DIM,
            3 => style.attrs |= ITALIC,
            4 => style.underline = 1,
            7 => style.attrs |= REVERSE,
            9 => style.attrs |= CROSSED,
            22 => style.attrs &= !(BOLD | DIM),
            23 => style.attrs &= !ITALIC,
            24 => style.underline = 0,
            27 => style.attrs &= !REVERSE,
            29 => style.attrs &= !CROSSED,
            30..=37 => style.fg = Color::idx(n as u8 - 30),
            90..=97 => style.fg = Color::idx(n as u8 - 90 + 8),
            40..=47 => style.bg = Color::idx(n as u8 - 40),
            100..=107 => style.bg = Color::idx(n as u8 - 100 + 8),
            39 => style.fg = Color::DEFAULT,
            49 => style.bg = Color::DEFAULT,
            59 => style.ul = Color::DEFAULT,
            38 | 48 | 58 => {
                let at = |k: usize| ps.get(i + k).and_then(|p| num(p)).map(|v| v.min(255) as u8);
                let (c, used) = match at(1)? {
                    5 => (Color::idx(at(2)?), 3),
                    2 => (Color::rgb(at(2)?, at(3)?, at(4)?), 5),
                    _ => return None,
                };
                match n {
                    38 => style.fg = c,
                    48 => style.bg = c,
                    _ => style.ul = c,
                }
                i += used;
                continue;
            }
            _ => return None,
        }
        i += 1;
    }
    Some(())
}

/// Changed cells closer than this are written together — a jump (`CSI y;x H`) costs about as much as
/// writing the few same cells between.
const GAP: usize = 8;

/// What turns a terminal showing `prev` into `next` (`all`: the terminal's content isn't known — every cell).
fn diff(prev: &Grid, next: &Grid, all: bool, out: &mut Vec<u8>) {
    let w = next.w;
    let mut wrote = false;
    for y in 0..next.h {
        let (a, b) = (prev.row(y), next.row(y));
        let mut x = 0;
        while x < w {
            if !all && a[x] == b[x] {
                x += 1;
                continue;
            }
            let mut end = x;
            let mut same = 0;
            let mut k = x + 1;
            while k < w {
                if all || a[k] != b[k] {
                    (end, same) = (k, 0);
                } else {
                    same += 1;
                    if same >= GAP {
                        break;
                    }
                }
                k += 1;
            }
            // Whole wide characters, as either side has them: never write half of one
            let mut s = x;
            while s > 0 && (a[s].width == 0 || b[s].width == 0) {
                s -= 1;
            }
            let mut e = end;
            while e + 1 < w && (a[e + 1].width == 0 || b[e + 1].width == 0) {
                e += 1;
            }
            if !wrote {
                out.extend_from_slice(b"\x1b[?2026h\x1b[?25l");
                wrote = true;
            }
            move_to(out, s, y);
            let mut cur = None;
            for c in &b[s..=e] {
                if c.width == 0 {
                    continue;
                }
                if cur != Some(c.style) {
                    put_style(out, c.style);
                    cur = Some(c.style);
                }
                out.extend_from_slice(c.text());
            }
            x = e + 1;
        }
    }
    if !wrote && !all && prev.cursor == next.cursor && (next.cursor.is_none() || prev.shape == next.shape) {
        return;
    }
    if !wrote {
        out.extend_from_slice(b"\x1b[?2026h");
    }
    match next.cursor {
        Some((x, y)) => {
            move_to(out, x, y);
            out.extend_from_slice(b"\x1b[");
            push_num(out, next.shape as usize);
            out.extend_from_slice(b" q\x1b[?25h");
        }
        None if !wrote => out.extend_from_slice(b"\x1b[?25l"),
        None => {} // (hidden since the start of this write)
    }
    out.extend_from_slice(b"\x1b[0m\x1b[?2026l");
}

fn push_num(out: &mut Vec<u8>, n: usize) {
    let mut d = [0u8; 20];
    let mut i = d.len();
    let mut n = n;
    loop {
        i -= 1;
        d[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend_from_slice(&d[i..]);
}

fn move_to(out: &mut Vec<u8>, x: usize, y: usize) {
    out.extend_from_slice(b"\x1b[");
    push_num(out, y + 1);
    out.push(b';');
    push_num(out, x + 1);
    out.push(b'H');
}

/// One sequence from a reset, as `term::apply` writes it — the underline's color and a curly underline apart.
fn put_style(out: &mut Vec<u8>, s: Style) {
    let color = |out: &mut Vec<u8>, on: &[u8], c: Color| match c.0 {
        [0, ..] => {}
        [1, n, ..] => {
            out.extend_from_slice(on);
            out.extend_from_slice(b";5;");
            push_num(out, n as usize);
        }
        [_, r, g, b] => {
            out.extend_from_slice(on);
            out.extend_from_slice(b";2");
            for v in [r, g, b] {
                out.push(b';');
                push_num(out, v as usize);
            }
        }
    };
    out.extend_from_slice(b"\x1b[0");
    color(out, b";38", s.fg);
    color(out, b";48", s.bg);
    for (bit, code) in [(BOLD, b";1"), (DIM, b";2"), (ITALIC, b";3"), (REVERSE, b";7"), (CROSSED, b";9")] {
        if s.attrs & bit != 0 {
            out.extend_from_slice(code);
        }
    }
    if s.underline == 1 {
        out.extend_from_slice(b";4");
    }
    out.push(b'm');
    if s.ul != Color::DEFAULT {
        out.extend_from_slice(b"\x1b[");
        color(out, b"58", s.ul);
        out.push(b'm');
    }
    if s.underline > 1 {
        out.extend_from_slice(b"\x1b[4:");
        push_num(out, s.underline as usize);
        out.push(b'm');
    }
}

/// The terminal's side of the event loop: frames in, what to write out.
pub struct Screen {
    /// What the terminal shows — as far as `known` says
    shown: Grid,
    next: Grid,
    /// `shown` is what the terminal shows (not so at first, after a resize, focus back, `:redraw`, or a
    /// frame written as it is)
    known: bool,
    /// The last write was only what changed — the terminal hasn't had the whole screen since
    patched: bool,
    /// The last frame, to pass over one just like it without reading it (none after `invalidate`)
    last: Vec<u8>,
    out: Vec<u8>,
    /// `Grid.under`'s cells, kept from frame to frame
    under: Vec<Option<Color>>,
    warned: bool,
}

impl Screen {
    pub fn new() -> Screen {
        Screen {
            shown: Grid::new(0, 0),
            next: Grid::new(0, 0),
            known: false,
            patched: false,
            last: Vec::new(),
            out: Vec::new(),
            under: Vec::new(),
            warned: false,
        }
    }

    /// The terminal may show something else than the last frame: the next one redraws everything.
    pub fn invalidate(&mut self) {
        self.known = false;
        self.last.clear();
    }

    /// What to write for `frame`, drawn for a `w`×`h` terminal: what changed — everything after
    /// `invalidate` — or the frame itself when it can't be read. Empty when nothing changed. Its cards'
    /// `edges` are seated on what's under them as it's read (`Grid::seat` — no second reading).
    pub fn present<'a>(&'a mut self, frame: &'a [u8], w: u16, h: u16, edges: &[Edge]) -> &'a [u8] {
        let (w, h) = (w as usize, h as usize);
        if (w, h) != (self.shown.w, self.shown.h) {
            self.shown = Grid::new(w, h);
            self.invalidate();
        }
        if !self.last.is_empty() && frame == self.last {
            return &[];
        }
        self.last.clear();
        self.last.extend_from_slice(frame);
        self.next.clone_from(&self.shown);
        if !edges.is_empty() {
            let mut under = std::mem::take(&mut self.under);
            under.clear();
            under.resize(w * h, None);
            self.next.under = Some(under);
        }
        let read = w > 0 && h > 0 && self.next.apply(frame).is_some();
        if let Some(under) = self.next.under.take() {
            if read {
                self.next.seat(&under, edges);
            }
            self.under = under;
        }
        if !read {
            if !self.warned {
                self.warned = true;
                crate::log::line(
                    "tarae",
                    "info",
                    "screen: a frame with an escape this can't read — drawn whole",
                );
            }
            self.known = false;
            self.patched = false;
            return frame;
        }
        self.out.clear();
        diff(&self.shown, &self.next, !self.known, &mut self.out);
        if !self.out.is_empty() {
            self.patched = self.known;
        }
        std::mem::swap(&mut self.shown, &mut self.next);
        self.known = true;
        &self.out
    }

    /// Whether only parts of the screen went out since it last went whole — see `event_loop`'s resting redraw.
    pub fn patched(&self) -> bool {
        self.patched
    }
}

/// The half blocks a card's top and bottom edges are drawn with (`term::card_edge`).
const HALVES: [&[u8]; 2] = ["▄".as_bytes(), "▀".as_bytes()];

/// A card's edge row: `width` cells from (`x`, `y`), drawn in half blocks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edge {
    pub x: usize,
    pub y: usize,
    pub width: usize,
}

impl Grid {
    /// Seats the cards on what's under them: each edge cell still showing its half block gets as background
    /// — the half outside the card — the one that cell had before the edge was drawn over it (`under`: the
    /// code, a cursor line, a review's wash, a panel, another card).
    fn seat(&mut self, under: &[Option<Color>], edges: &[Edge]) {
        for e in edges.iter().filter(|e| e.y < self.h) {
            for x in e.x..(e.x + e.width).min(self.w) {
                let i = e.y * self.w + x;
                if let Some(bg) = under[i]
                    && HALVES.contains(&self.cells[i].text())
                {
                    self.cells[i].style.bg = bg;
                }
            }
        }
    }
}

/// A whole frame as the text it shows (`w`×`h` cells, a row a line, trailing blanks and rows dropped) — what
/// goes to Claude for "the screen" (`chat::attach_screen`). None: a frame this doesn't read.
pub fn text(frame: &[u8], w: u16, h: u16) -> Option<String> {
    text_hiding(frame, w, h, None)
}

/// A part of the screen left out of its text: rows `rows` (first, last), from the left edge up to column
/// `cols_end`, blanked — `note` on its first row says so.
pub struct Hidden {
    pub rows: (usize, usize),
    pub cols_end: usize,
    pub note: String,
}

/// `text`, with `hide` left out (the terminal panel, when `llm.screen-terminal` is off).
pub fn text_hiding(frame: &[u8], w: u16, h: u16, hide: Option<&Hidden>) -> Option<String> {
    let mut g = Grid::new(w as usize, h as usize);
    g.apply(frame)?;
    let mut rows: Vec<String> = (0..g.h)
        .map(|y| {
            let hid = hide.filter(|d| (d.rows.0..=d.rows.1).contains(&y));
            let mut row: Vec<u8> = Vec::new();
            if let Some(d) = hid {
                let note: String =
                    if y == d.rows.0 { d.note.chars().take(d.cols_end).collect() } else { String::new() };
                row.extend(format!("{note:<width$}", width = d.cols_end).bytes());
            }
            // (cells by column: a wide one takes two, its second has no width)
            let from = hid.map_or(0, |d| d.cols_end);
            let cells = g.row(y).iter().enumerate().skip(from).filter(|(_, c)| c.width > 0);
            row.extend(cells.flat_map(|(_, c)| c.text().to_vec()));
            String::from_utf8_lossy(&row).trim_end().to_string()
        })
        .collect();
    while rows.last().is_some_and(String::is_empty) {
        rows.pop();
    }
    Some(rows.join("\n"))
}

/// Replaying what `Screen` writes gives, frame after frame, the screen each whole frame draws over the ones
/// before it — checked cell by cell with a terminal emulator (vt100).
#[cfg(test)]
pub fn assert_replays(w: u16, h: u16, frames: impl IntoIterator<Item = Vec<u8>>) {
    fn cells(p: &vt100::Parser) -> Vec<String> {
        let s = p.screen();
        let (h, w) = s.size();
        let mut v = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let Some(c) = s.cell(y, x) else { continue };
                let text = if c.contents().is_empty() { " ".to_string() } else { c.contents().to_string() };
                // A blank's color and weight don't show
                let ink = text != " " || c.inverse();
                v.push(format!(
                    "{y},{x} {text:?} {:?} {:?} b{} i{} u{} r{} w{}",
                    if ink { c.fgcolor() } else { vt100::Color::Default },
                    c.bgcolor(),
                    ink && c.bold(),
                    ink && c.italic(),
                    c.underline(),
                    c.inverse(),
                    c.is_wide()
                ));
            }
        }
        // (where a hidden cursor is doesn't show)
        v.push(format!("cursor {:?}", (!s.hide_cursor()).then(|| s.cursor_position())));
        v
    }
    let mut screen = Screen::new();
    let mut through_diffs = vt100::Parser::new(h, w, 0);
    let mut whole = vt100::Parser::new(h, w, 0);
    for (n, f) in frames.into_iter().enumerate() {
        let out = screen.present(&f, w, h, &[]).to_vec();
        assert!(out != f, "frame {n} wasn't read — went out as it is");
        through_diffs.process(&out);
        whole.process(&f);
        let (a, b) = (cells(&through_diffs), cells(&whole));
        if let Some(i) = (0..a.len()).find(|&i| a[i] != b[i]) {
            panic!("frame {n}: through diffs {} — whole frames {}", a[i], b[i]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(w: usize, h: usize, frame: &[u8]) -> Grid {
        let mut g = Grid::new(w, h);
        g.apply(frame).expect("readable");
        g
    }

    fn row_text(g: &Grid, y: usize) -> String {
        g.row(y)
            .iter()
            .filter(|c| c.width > 0)
            .map(|c| String::from_utf8_lossy(c.text()).into_owned())
            .collect()
    }

    #[test]
    fn reads_moves_styles_erase_and_the_cursor() {
        let g = grid(
            10,
            2,
            b"\x1b[?2026h\x1b[?25l\x1b[1;1H\x1b[0;38;2;1;2;3;48;5;8;1mab\x1b[0;49mc\x1b[K\x1b[2;4H\x1b[58;2;9;9;9m\x1b[4:3mx\x1b[2;2H\x1b[6 q\x1b[?25h\x1b[0m\x1b[0m\x1b[?2026l",
        );
        assert_eq!(row_text(&g, 0), "abc       ");
        assert_eq!(
            g.row(0)[0].style,
            Style { fg: Color::rgb(1, 2, 3), bg: Color::idx(8), attrs: BOLD, ..PLAIN }
        );
        assert_eq!(g.row(0)[2].style, PLAIN);
        assert_eq!(g.row(1)[3].style, Style { ul: Color::rgb(9, 9, 9), underline: 3, ..PLAIN });
        assert_eq!((g.cursor, g.shape), (Some((1, 1)), 6));
    }

    #[test]
    fn what_it_cant_read_is_refused() {
        for frame in [
            &b"\x1b[1;1Habc\x1b[2J"[..], // another escape
            b"\x1b[1;1Ha\tb",            // a control character
            b"abc",                      // text before any move
            b"\x1b[1;4Habcd",            // past the edge
            b"\x1b[1;5H\xea\xb0\x80",    // a wide character in the last column
            b"\x1b]8;;x\x1b\\",          // OSC
            b"\x1b[1;1H\x1b[38;7m",      // an unknown color form
            b"\x1b[?5;3Hx",              // a private move
            b"\x1b[1;2;3Hx",             // a third parameter
        ] {
            assert!(Grid::new(5, 2).apply(frame).is_none(), "{:?}", String::from_utf8_lossy(frame));
        }
    }

    #[test]
    fn wide_characters_cut_in_half_leave_blank_halves() {
        let mut g = grid(6, 1, "\x1b[1;1H가나다".as_bytes());
        assert_eq!(row_text(&g, 0), "가나다");
        g.apply(b"\x1b[1;2Hx").unwrap(); // over 가's right half
        assert_eq!(row_text(&g, 0), " x나다");
        g.apply(b"\x1b[1;5Hy").unwrap(); // over 다's left half
        assert_eq!(row_text(&g, 0), " x나y ");
    }

    fn replays(w: u16, h: u16, frames: &[&[u8]]) {
        assert_replays(w, h, frames.iter().map(|f| f.to_vec()));
    }

    #[test]
    fn diffs_replay_to_the_same_screen() {
        let a = "\x1b[?25l\x1b[1;1H\x1b[0;38;5;3mfn main() {\x1b[K\x1b[2;1H\x1b[0m    let 가 = 1;\x1b[K\x1b[3;1H}\x1b[K\x1b[2;5H\x1b[2 q\x1b[?25h\x1b[0m";
        let b = "\x1b[?25l\x1b[1;1H\x1b[0;38;5;3mfn main() {\x1b[K\x1b[2;1H\x1b[0m    let 가나 = 12;\x1b[K\x1b[3;1H}\x1b[K\x1b[2;9H\x1b[6 q\x1b[?25h\x1b[0m";
        let c = "\x1b[?25l\x1b[1;1H\x1b[0;38;5;3mfn main() {\x1b[K\x1b[2;1H\x1b[0m    let 가나 = 12;\x1b[K\x1b[3;1H}\x1b[K\x1b[2;3H\x1b[0;7m popup \x1b[0m";
        replays(30, 4, &[a.as_bytes(), b.as_bytes(), b.as_bytes(), c.as_bytes(), a.as_bytes()]);
    }

    #[test]
    fn a_frame_like_the_last_writes_nothing_and_a_small_change_little() {
        let mut s = Screen::new();
        let row = |t: &str| {
            format!("\x1b[?25l\x1b[1;1H\x1b[0m{t:<60}\x1b[2;1H{:<60}\x1b[1;1H\x1b[?25h", "second line")
        };
        let first = s.present(row("hello").as_bytes(), 60, 2, &[]).len();
        assert!(first > 120, "the first frame is drawn whole");
        assert!(s.present(row("hello").as_bytes(), 60, 2, &[]).is_empty());
        let small = s.present(row("hellO").as_bytes(), 60, 2, &[]).to_vec();
        assert!(small.len() * 2 < first, "{} {:?}", small.len(), String::from_utf8_lossy(&small));
        assert!(s.patched(), "only a part went out");
        s.invalidate();
        assert!(s.present(row("hellO").as_bytes(), 60, 2, &[]).len() > 120, "redrawn whole");
        assert!(!s.patched());
    }

    #[test]
    fn an_unreadable_frame_goes_out_as_it_is_and_the_next_is_whole() {
        let mut s = Screen::new();
        let good = b"\x1b[1;1Habc".as_slice();
        s.present(good, 10, 1, &[]);
        let odd = b"\x1b[1;1Habd\x1b[5n".as_slice();
        assert_eq!(s.present(odd, 10, 1, &[]), odd);
        assert!(s.present(good, 10, 1, &[]).len() > 10, "everything again");
    }
}
