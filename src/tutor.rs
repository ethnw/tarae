//! `:tutor` drawn as text: `tutor.md`'s few marks — `#` a heading, `` `key` ``, `**word**` — are taken out of the
//! text and kept as its colours (`Document.paint`, following edits), so no markdown shows and every line reads,
//! and is practised on, as it's drawn (Helix's tutor is plain text too). An exercise's `→` is painted as a list's
//! mark. A line starting `@zellij ` is there only inside zellij, one starting `@plain ` only outside it — where
//! zellij takes a key the tutor names its twin (`zellij.rs`).

use crate::syntax::capture_id;

/// The widest line a reader meets (80 columns, less the sign column and the line numbers).
#[cfg(test)]
const WIDTH: usize = 72;

/// The tutorial's text and its colours (byte spans, global capture ids) — inside zellij (the twins said), zellij
/// taking `letters` with `C-` (`editor.zellij-keys`).
pub fn text(zellij: bool, letters: &[char]) -> (String, Vec<(usize, usize, u32)>) {
    from(include_str!("tutor.md"), zellij, letters)
}

fn from(src: &str, zellij: bool, letters: &[char]) -> (String, Vec<(usize, usize, u32)>) {
    // (`@zellij-q`/`@plain-q`: whether zellij takes `C-q` — the cards' and the inbox's way to them)
    let q = zellij && letters.contains(&'q');
    let keys = crate::zellij::said(letters, true);
    let (heading, key, strong, mark) = (
        capture_id("markup.heading"),
        capture_id("markup.raw"),
        capture_id("markup.bold"),
        capture_id("markup.list"),
    );
    let (mut text, mut paint) = (String::with_capacity(src.len()), Vec::new());
    for line in src.split_inclusive('\n') {
        // (the mark, then a space — or the line's end, for an empty line)
        let only = |m: &str| {
            line.strip_prefix(m).and_then(|l| l.strip_prefix(' ').or(l.starts_with('\n').then_some(l)))
        };
        let line = match (only("@zellij"), only("@plain"), only("@zellij-q"), only("@plain-q")) {
            (Some(l), ..) if zellij => l,
            (_, Some(l), ..) if !zellij => l,
            (_, _, Some(l), _) if q => l,
            (.., Some(l)) if !q => l,
            (None, None, None, None) => line,
            _ => continue,
        };
        // (the keys zellij takes, as the setting names them)
        let with_keys: String;
        let line = match line.contains("{zellij-keys}") {
            true => {
                with_keys = line.replace("{zellij-keys}", &keys);
                with_keys.as_str()
            }
            false => line,
        };
        if let Some(h) = line.strip_prefix("# ").or_else(|| line.strip_prefix("## ")) {
            let body = h.trim_end_matches('\n');
            paint.push((text.len(), text.len() + body.len(), heading));
            text.push_str(h);
            continue;
        }
        // (a span open: where it started and what paints it)
        let mut open: Option<(usize, u32)> = None;
        let mut rest = line;
        while let Some(c) = rest.chars().next() {
            let (toggle, by) = match c {
                '`' => (Some(key), 1),
                '*' if rest.starts_with("**") => (Some(strong), 2),
                _ => (None, c.len_utf8()),
            };
            match (toggle, open) {
                (Some(t), Some((at, o))) if t == o => {
                    paint.push((at, text.len(), o));
                    open = None;
                }
                (Some(t), None) => open = Some((text.len(), t)),
                _ => {
                    if c == '→' && open.is_none() && line[..line.len() - rest.len()].trim().is_empty() {
                        paint.push((text.len(), text.len() + c.len_utf8(), mark));
                    }
                    text.push_str(&rest[..by]);
                }
            }
            rest = &rest[by..];
        }
    }
    (text, paint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn the_tutor_shows_no_markdown_and_fits_80_columns() {
        for zellij in [false, true] {
            reads_whole(zellij);
        }
    }

    fn reads_whole(zellij: bool) {
        let (text, paint) = text(zellij, &crate::zellij::DEFAULT_KEYS);
        assert!(!text.contains("@zellij") && !text.contains("@plain"), "a line's mark shows");
        for (i, line) in text.lines().enumerate() {
            let n = i + 1;
            assert!(!line.contains('`') && !line.contains("**"), "line {n} shows a mark: {line}");
            assert!(!line.starts_with('#'), "line {n} shows a heading's mark: {line}");
            assert!(line.width() <= WIDTH, "line {n} is {} wide: {line}", line.width());
        }
        // Every span is on the text it paints: the keys, the headings, an exercise's arrow
        let of = |cap: &str| -> Vec<&str> {
            let id = capture_id(cap);
            paint.iter().filter(|s| s.2 == id).map(|s| &text[s.0..s.1]).collect()
        };
        let keys = of("markup.raw");
        for k in ["space e", "esc", "C-\\", "C-q", "space I", "space W", "space v v", "y", "n", "q"] {
            assert!(keys.contains(&k), "{k} isn't painted as a key: {keys:?}");
        }
        // Inside zellij, `C-q` is named only as what zellij takes — its twins are the keys to press
        let says = |s: &str| text.contains(s);
        assert_eq!(says("C-g locks zellij"), zellij, "the zellij section");
        assert_eq!(says("C-q hands it your keys"), !zellij);
        if zellij {
            assert!(keys.contains(&"esc esc") && keys.contains(&"C-g"), "{keys:?}");
        }
        let headings = of("markup.heading");
        assert_eq!(headings[0], "tarae tutorial");
        for h in ["Panels", "The terminal", "Cards", "Reviewing", "Sessions", "The inbox"] {
            assert!(headings.iter().any(|l| l.contains(h)), "no section on {h}: {headings:?}");
        }
        assert!(of("markup.list").iter().all(|a| *a == "→") && of("markup.list").len() > 5);
        assert_eq!(of("markup.bold"), ["normal", "insert", "selects"]);
    }

    #[test]
    fn marks_come_out_and_their_spans_stay() {
        let src =
            "## 2  Moving\n  `h` ← a **word**, `gg` top\n@plain   → do it\n@zellij   → do it in zellij\n";
        let d = &crate::zellij::DEFAULT_KEYS;
        let (text, paint) = from(src, false, d);
        assert_eq!(text, "2  Moving\n  h ← a word, gg top\n  → do it\n");
        assert_eq!(from(src, true, d).0, "2  Moving\n  h ← a word, gg top\n  → do it in zellij\n");
        // (the keys zellij takes, from the setting; the `C-q` lines only where it takes `C-q`)
        let keys = "@zellij {zellij-keys}\n@zellij-q zellij takes C-q\n@plain-q C-q\n";
        assert_eq!(from(keys, true, &['g', 'q']).0, "C-g C-q\nzellij takes C-q\n");
        assert_eq!(from(keys, true, &['g']).0, "C-g\nC-q\n");
        assert_eq!(from(keys, false, &['g', 'q']).0, "C-q\n");
        let got: Vec<&str> = paint.iter().map(|s| &text[s.0..s.1]).collect();
        assert_eq!(got, ["2  Moving", "h", "word", "gg", "→"]);
    }
}
