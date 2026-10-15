//! Text cut to a number of cells — by display width, for whatever lays text out in cells: the terminal's
//! drawing (`term.rs`) and the model's layouts (`diagram::layout`), which don't know the terminal.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// The start of `s` that fits in `width` cells — bare: for code and output lines.
pub fn fit(s: &str, width: usize) -> String {
    let mut out = String::new();
    let mut w = 0;
    for ch in s.chars() {
        let cw = ch.width().unwrap_or(0);
        if w + cw > width {
            break;
        }
        w += cw;
        out.push(ch);
    }
    out
}

/// Truncate to width, ending with '…' if cut — nothing when there's no room at all (the `…` is a cell too).
pub fn fit_ellipsis(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = fit(s, width.saturating_sub(1));
    out.push('…');
    out
}
