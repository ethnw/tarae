//! View moves (helix's `z` mode, `Z` = sticky until Esc): `zz`/`zc` put the cursor line mid-screen, `zt` at the
//! top, `zb` at the bottom, `zj`/`zk` scroll a line (the cursor stays unless the view leaves it). `gt` `gc`
//! `gb` do the opposite: the cursor goes to the top / middle / bottom of what's on screen. Rows, not lines,
//! when the document soft-wraps.

use crate::codeview::Measure;
use crate::editor::Editor;
use crate::movement as mv;
use crate::selection::Selection;

/// Where to put the cursor line (`z` mode) or the cursor (`g` + t c b).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    Top,
    Center,
    Bottom,
}

impl Editor {
    /// Screen rows the focused pane's lines near `line` take, as it lays them out (`codeview::Measure`): wrapped
    /// rows, the removed rows a code review shows above a line, a folded doc comment's rows.
    fn measure(&self, line: usize) -> Measure<'_> {
        let doc = self.doc();
        let wrap = self.wraps(doc).then_some(self.viewport.1);
        Measure::new(self, doc, self.editing_in(true), (wrap, self.viewport.1), line, self.viewport.0.max(1))
    }

    fn scrolloff(&self) -> usize {
        self.config.scrolloff.min(self.viewport.0.saturating_sub(1) / 2)
    }

    /// The top line and its rows above the screen that put `line` (its first row) `above` rows down the screen:
    /// a line above that doesn't fit whole shows its last rows (a wrapped line's, a folded doc comment's — never
    /// some of the removed rows above a line); never a top inside a folded block past its first line.
    fn top_for(m: &Measure, line: usize, above: usize) -> (usize, usize) {
        let mut top = line;
        let mut rows = 0;
        while top > 0 && rows < above {
            let n = m.rows(top - 1);
            if rows + n > above {
                let skip = n - (above - rows);
                if skip <= m.skippable(top - 1) {
                    return (m.shown_as(top - 1), skip);
                }
                break;
            }
            top -= 1;
            rows += n;
        }
        match m.folded(top) {
            Some((a, b, _)) if top > a => (b + 1, 0),
            _ => (top, 0),
        }
    }

    /// The view starts at `top`, `skip` of its rows above the screen.
    fn set_top_rows(&mut self, (top, skip): (usize, usize)) {
        let doc = self.doc_mut();
        doc.set_top(top);
        doc.top_rows = (skip > 0).then_some((top, skip));
    }

    /// `zz` `zt` `zb` — scroll so the cursor line sits there (the scrolloff margin still applies).
    pub fn align_view(&mut self, at: Place) {
        let rows = self.viewport.0.max(1);
        let m = self.measure(self.cursor_line());
        let line = m.shown_as(self.cursor_line());
        let above = match at {
            Place::Top => self.scrolloff(),
            Place::Center => rows.saturating_sub(m.rows(line)) / 2,
            Place::Bottom => rows.saturating_sub(m.rows(line) + self.scrolloff()),
        };
        let top = Self::top_for(&m, line, above);
        drop(m);
        self.set_top_rows(top);
    }

    /// Puts `lines` in sight as a whole (a follow or replay stop — the cursor on its first line): left as
    /// it is when it's all on screen already; centred as a block when it fits (equal room above and
    /// below); taller than the screen, its first line near the top with `RANGE_CONTEXT` lines above (or the
    /// scrolloff margin, if wider).
    pub fn show_range(&mut self, lines: std::ops::Range<usize>) {
        const RANGE_CONTEXT: usize = 3;
        let rows = self.viewport.0.max(1);
        let so = self.scrolloff();
        let doc = self.doc();
        let last = mv::last_line(&doc.text);
        let m = self.measure(lines.start.min(last));
        let start = m.shown_as(lines.start.min(last));
        let end = lines.end.min(last + 1).max(start + 1);
        let top = doc.top;
        // Rows of lines `a..b`, counted no further than a screen and a row
        let rows_in = |a: usize, b: usize| {
            let mut n = 0;
            for l in a..b {
                n += m.rows(l);
                if n > rows {
                    break;
                }
            }
            n
        };
        let first_seen = start >= top && (top == 0 || start >= top + so);
        if first_seen && rows_in(top, end) <= rows {
            return;
        }
        let tall = rows_in(start, end);
        let above = if tall <= rows { ((rows - tall) / 2).max(so) } else { RANGE_CONTEXT.max(so) };
        let top = Self::top_for(&m, start, above);
        drop(m);
        self.set_top_rows(top);
    }

    /// An edit being written over `line` (`writing.rs`): the view so its first row is `above` rows down — where
    /// the review will show it (`term::review_lead`); its head is kept in sight from there (`term::scroll_to_cursor`).
    pub fn align_writing(&mut self, line: usize, above: usize) {
        let last = mv::last_line(&self.doc().text);
        let m = self.measure(line.min(last));
        let start = m.shown_as(line.min(last));
        let top = Self::top_for(&m, start, above.min(self.viewport.0.saturating_sub(1)));
        drop(m);
        self.set_top_rows(top);
    }

    /// First and last lines fully on screen, inside the scrolloff margin (a folded doc comment's first line for
    /// one inside it).
    fn inner_lines(&self) -> (usize, usize) {
        let doc = self.doc();
        let (top, last) = (doc.top, mv::last_line(&doc.text));
        let (rows, so) = (self.viewport.0.max(1), self.scrolloff());
        let m = self.measure(top);
        let first = if top == 0 { 0 } else { (top + so).min(last) };
        let (mut l, mut used) = (top, 0);
        while l < last && used + m.rows(l) + m.rows(l + 1) <= rows {
            used += m.rows(l);
            l += 1;
        }
        let bottom = if l >= last { last } else { l.saturating_sub(so).max(first) };
        (m.shown_as(first), m.shown_as(bottom))
    }

    /// Put the primary cursor on `line`, same column (select mode: extend).
    fn cursor_to_line(&mut self, line: usize, extend: bool) {
        let tab = self.config.tab_width;
        let doc = self.doc_mut();
        let r = doc.selection().primary();
        let col = mv::cursor_col(&doc.text, r, tab);
        let pos = mv::pos_at_col(&doc.text, line, col, tab);
        let r = r.put_cursor(&doc.text, pos, extend);
        doc.set_selection(Selection::single(r));
    }

    /// `zj` / `zk` — the view moves `delta` lines; the cursor only moves if the view would leave it.
    pub fn scroll_view(&mut self, delta: isize, extend: bool) {
        let last = mv::last_line(&self.doc().text);
        let top = (self.doc().top as isize + delta).clamp(0, last as isize) as usize;
        // (a folded doc comment is one line: the view steps over it)
        let top = match self.measure(top).folded(top) {
            Some((a, b, _)) if top > a => {
                if delta > 0 {
                    (b + 1).min(last)
                } else {
                    a
                }
            }
            _ => top,
        };
        self.doc_mut().set_top(top);
        let (first, bottom) = self.inner_lines();
        let line = self.cursor_line();
        if line < first {
            self.cursor_to_line(first, extend);
        } else if line > bottom {
            self.cursor_to_line(bottom, extend);
        }
    }

    /// `gt` `gc` `gb` — the cursor to the top / middle / bottom line on screen.
    pub fn goto_window(&mut self, at: Place, extend: bool) {
        let (first, bottom) = self.inner_lines();
        let line = match at {
            Place::Top => first,
            Place::Bottom => bottom,
            Place::Center => first + (bottom - first) / 2,
        };
        self.cursor_to_line(line, extend);
    }
}

#[cfg(test)]
mod tests {
    use crate::config::Config;
    use crate::editor::Editor;

    /// A stop is placed by its whole range: one that fits is centred as a block, a taller one starts near
    /// the top with three lines above it, and one already on screen doesn't move the view.
    #[test]
    fn a_stop_is_placed_by_its_whole_range() {
        let mut ed = Editor::new(Config::default());
        ed.docs[0].text =
            ropey::Rope::from_str(&(0..1000).map(|i| format!("line {i}\n")).collect::<String>());
        ed.viewport = (40, 80);
        ed.config.scrolloff = 3;
        // 20 lines in 40 rows: 10 above, 10 below
        ed.show_range(500..520);
        assert_eq!(ed.doc().top, 490, "centred as a block");
        // Already all on screen: left alone
        ed.show_range(495..515);
        assert_eq!(ed.doc().top, 490, "on screen — no scroll");
        ed.show_range(493..530);
        assert_eq!(ed.doc().top, 490, "the whole screen's worth, still on it");
        // Taller than the screen: its first line three rows below the top
        ed.show_range(600..830);
        assert_eq!(ed.doc().top, 597, "starts 3 rows below the top");
        // Partly on screen (its end below): placed again
        ed.show_range(630..660);
        assert_eq!(ed.doc().top, 625, "centred: (40 - 30) / 2 above");
        // The scrolloff margin wins when it's wider than the context
        ed.config.scrolloff = 5;
        ed.show_range(100..400);
        assert_eq!(ed.doc().top, 95);
    }
}
