//! Screen snapshot ("golden") tests — one frame from `render`, fed through a vt100 emulator, compared with
//! `src/snapshots/<name>.txt`. Catches layout and color-hierarchy regressions without eyeballing
//! screenshots. After an intended change: `TARAE_BLESS=1 cargo test snapshot_` rewrites the files
//! (then read the diff before committing).
//!
//! Snapshot format — four sections; every screen row is prefixed with its two-digit row number, trailing
//! blanks are trimmed (so editors that strip whitespace don't break the files):
//!
//! ```text
//! # <name> · <w>x<h> · theme meok · cursor <row>:<col>     ("cursor hidden" when not shown)
//! ## text
//! 00 <the row's characters — a wide char appears once, though it covers two cells>
//! ## bg · surface per cell, blank = ui.background
//! 00 <one mark per cell: a letter from the legend, 1–9 = other colors numbered by first appearance
//!     (blends — diff/tint backgrounds), ~ = never painted (terminal default), R = reverse video>
//! ## fg · visible glyphs only, blank = any unlisted color, UPPERCASE = bold (B = bold, unlisted color)
//! 00 <one mark per cell>
//! ## legend
//! bg: c=ui.cursorline.primary  p=ui.popup  1=other …   (only the marks that appear, theme key per mark)
//! fg: a=ui.accent  d=ui.virtual  B=bold …
//! ```
//!
//! The marks name theme keys rather than RGB values, so tweaking a palette color doesn't churn every file,
//! while a surface or accent moving to the wrong cells does show up. When two keys share a color the
//! first key in `BG`/`FG` wins.
//!
//! Determinism: fixed theme (meok), no language servers or git unless faked, only the built-in (core)
//! grammars, toasts frozen at full opacity, and every worker job the frame depends on is awaited
//! (`Shot::until`) — no sleeps.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ropey::Rope;
use serde_json::json;

use super::{Ui, card_style, render};
use crate::config::Config;
use crate::document::DocId;
use crate::editor::Editor;
use crate::event::Event;
use crate::key::{Code, Key};

/// Background marks: (mark, theme key).
const BG: &[(char, &str)] = &[
    ('c', "ui.cursorline.primary"),
    ('p', "ui.popup"),
    ('P', "ui.menu.selected"),
    ('v', "ui.selection"),
    ('V', "ui.selection.primary"),
    ('A', "ui.cursor.primary"),
    ('k', "ui.cursor"),
    ('i', "ui.cursor.primary.insert"),
    ('s', "ui.cursor.primary.select"),
];

/// Foreground marks: (mark, theme key) — lowercase; bold prints the uppercase.
const FG: &[(char, &str)] = &[
    ('a', "ui.accent"),
    ('t', "ui.text.focus"),
    ('d', "ui.virtual"),
    ('f', "ui.linenr"),
    ('e', "error"),
    ('w', "warning"),
    ('g', "diff.plus"),
];

/// Languages whose grammars are always compiled into a test build (`core = true` and `dev = true` in
/// languages.toml).
const CORE: &[&str] = &["rust", "toml", "markdown"];

/// Fixture root for scenarios that need real files (picker listing and preview).
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/snapshots/fixtures")
}

/// A small Rust file (doc comment, loop, string, multi-byte text) — the scenarios' default document.
const DEMO_RS: &str = r#"use std::collections::HashMap;

/// Counts how often each word appears.
///
/// Words are split on whitespace; case is kept.
pub fn word_counts(text: &str) -> HashMap<&str, usize> {
    let mut counts = HashMap::new();
    for word in text.split_whitespace() {
        *counts.entry(word).or_insert(0) += 1;
    }
    counts
}

fn main() {
    let text = "the quick brown fox jumps over the lazy dog";
    let counts = word_counts(text);
    println!("{} distinct words", counts.len()); // 타래
}
"#;

/// One editor at a fixed screen size, driven like the real loop: every event goes through
/// `handle_event` and is followed by a frame (so `viewport`/`view` are what the next key sees).
struct Shot {
    ed: Editor,
    w: u16,
    h: u16,
}

impl Shot {
    fn new(w: u16, h: u16) -> Self {
        // crossterm drops every color under NO_COLOR — which tarae's own test runner sets (`test_start`)
        crossterm::style::force_color_output(true);
        let mut ed = Editor::new(Config::default());
        assert_eq!(ed.theme.name, "meok", "snapshots are drawn with the dark built-in theme");
        ed.config.lsp.enabled = false;
        let mut shot = Shot { ed, w, h };
        shot.frame();
        shot
    }

    /// Shows `text` as the file `rel` (relative to the working directory, so the path bar reads
    /// `tarae › …`) without touching disk, and waits for its syntax tree (core grammars only).
    fn file(&mut self, rel: &str, text: &str) -> DocId {
        let path = std::env::current_dir().unwrap().join(rel);
        let doc = self.ed.doc_mut();
        doc.text = Rope::from_str(text);
        doc.path = Some(path.clone());
        let id = doc.id;
        // Only the built-in (core) grammars — others depend on what this machine has downloaded
        if crate::syntax::detect(&path).is_some_and(|l| CORE.contains(&l.name.as_str())) {
            self.ed.attach_syntax(id);
            self.until("syntax tree", |ed| {
                ed.doc().syntax.as_ref().is_some_and(|s| s.tree.is_some() && !s.dirty && !s.in_flight)
            });
        }
        self.frame();
        id
    }

    /// Keys like the editor tests' `feed`: plain chars, `<name>` for named keys (`<ret>`, `<C-w>`).
    fn keys(&mut self, keys: &str) {
        let mut chars = keys.chars();
        while let Some(c) = chars.next() {
            let key = if c == '<' {
                chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap()
            } else {
                Key::plain(Code::Char(c))
            };
            self.ed.handle_event(Event::Key(key));
            self.frame();
        }
    }

    /// Applies job results until `done` holds (each followed by a frame, like the event loop).
    fn until(&mut self, what: &str, done: impl Fn(&Editor) -> bool) {
        let end = Instant::now() + Duration::from_secs(10);
        while !done(&self.ed) {
            let left = end.saturating_duration_since(Instant::now());
            let ev =
                self.ed.events.recv_timeout(left).unwrap_or_else(|| panic!("timed out waiting for {what}"));
            self.ed.handle_event(ev);
            self.frame();
        }
    }

    /// Waits for reparses after edits (syntax dirty or in flight).
    fn settle(&mut self) {
        self.until("reparse", |ed| {
            ed.docs.iter().all(|d| d.syntax.as_ref().is_none_or(|s| !s.dirty && !s.in_flight))
        });
    }

    fn frame(&mut self) -> Vec<u8> {
        let mut buf = Vec::new();
        render(&mut self.ed, &mut buf, self.w, self.h).unwrap();
        buf
    }

    /// Draws the final frame and compares it with `src/snapshots/<name>.txt`.
    fn check(&mut self, name: &str) {
        let bytes = self.frame();
        let text = snapshot(&self.ed, name, &bytes, self.w, self.h);
        compare(name, &text);
    }
}

// ── Screen → text ─────────────────────────────────────────────────────────

fn vt_color(c: Option<crossterm::style::Color>) -> Option<vt100::Color> {
    use crossterm::style::Color as C;
    use vt100::Color::{Default, Idx, Rgb};
    Some(match c? {
        C::Reset => Default,
        C::Rgb { r, g, b } => Rgb(r, g, b),
        C::AnsiValue(i) => Idx(i),
        C::Black => Idx(0),
        C::DarkRed => Idx(1),
        C::DarkGreen => Idx(2),
        C::DarkYellow => Idx(3),
        C::DarkBlue => Idx(4),
        C::DarkMagenta => Idx(5),
        C::DarkCyan => Idx(6),
        C::Grey => Idx(7),
        C::DarkGrey => Idx(8),
        C::Red => Idx(9),
        C::Green => Idx(10),
        C::Yellow => Idx(11),
        C::Blue => Idx(12),
        C::Magenta => Idx(13),
        C::Cyan => Idx(14),
        C::White => Idx(15),
    })
}

fn snapshot(ed: &Editor, name: &str, bytes: &[u8], w: u16, h: u16) -> String {
    let mut parser = vt100::Parser::new(h, w, 0);
    parser.process(bytes);
    let screen = parser.screen();
    let ui = Ui::new(ed);
    let t = &ed.theme;
    let base_bg = vt_color(ui.base.bg).unwrap_or_default();
    let mut bg_keys: Vec<(vt100::Color, char, &str)> = Vec::new();
    for &(m, key) in BG {
        let bg =
            if key == "ui.popup" { card_style(&ui, ed, key).bg } else { t.try_get(key).and_then(|s| s.bg) };
        if let Some(c) = vt_color(bg).filter(|c| *c != base_bg && !bg_keys.iter().any(|k| k.0 == *c)) {
            bg_keys.push((c, m, key));
        }
    }
    let mut fg_keys: Vec<(vt100::Color, char, &str)> = Vec::new();
    for &(m, key) in FG {
        let fg = if key == "ui.accent" { ui.accent.fg } else { t.try_get(key).and_then(|s| s.fg) };
        if let Some(c) = vt_color(fg).filter(|c| !fg_keys.iter().any(|k| k.0 == *c)) {
            fg_keys.push((c, m, key));
        }
    }
    let mut others: Vec<vt100::Color> = Vec::new();
    let (mut used_bg, mut used_fg) = (Vec::new(), Vec::new());
    let (mut text, mut bgs, mut fgs) = (Vec::new(), Vec::new(), Vec::new());
    for row in 0..h {
        let (mut tl, mut bl, mut fl) = (String::new(), String::new(), String::new());
        for col in 0..w {
            let Some(cell) = screen.cell(row, col) else { continue };
            // A wide char's second cell: text already printed; style marks repeat the first cell's
            let lead = if cell.is_wide_continuation() && col > 0 { screen.cell(row, col - 1) } else { None };
            let c = lead.unwrap_or(cell);
            if lead.is_none() {
                tl.push_str(if cell.has_contents() { cell.contents() } else { " " });
            }
            let bm = match c.bgcolor() {
                _ if c.inverse() => 'R',
                vt100::Color::Default if base_bg != vt100::Color::Default => '~',
                bg if bg == base_bg => ' ',
                bg => match bg_keys.iter().find(|k| k.0 == bg) {
                    Some(k) => k.1,
                    None => {
                        let i = others.iter().position(|o| *o == bg).unwrap_or_else(|| {
                            others.push(bg);
                            others.len() - 1
                        });
                        char::from_digit(i as u32 + 1, 10).unwrap_or('+')
                    }
                },
            };
            if bm != ' ' && !used_bg.contains(&bm) {
                used_bg.push(bm);
            }
            bl.push(bm);
            let visible = c.has_contents() && !c.contents().trim().is_empty();
            let fm = match fg_keys.iter().find(|k| k.0 == c.fgcolor()) {
                _ if !visible => ' ',
                Some(k) if c.bold() => k.1.to_ascii_uppercase(),
                Some(k) => k.1,
                None if c.bold() => 'B',
                None => ' ',
            };
            if fm != ' ' && !used_fg.contains(&fm.to_ascii_lowercase()) {
                used_fg.push(fm.to_ascii_lowercase());
            }
            fl.push(fm);
        }
        text.push(tl);
        bgs.push(bl);
        fgs.push(fl);
    }
    let (cy, cx) = screen.cursor_position();
    let cursor = if screen.hide_cursor() { "cursor hidden".to_string() } else { format!("cursor {cy}:{cx}") };
    let mut out = format!("# {name} · {w}x{h} · theme {} · {cursor}\n", t.name);
    let section = |out: &mut String, title: &str, rows: &[String]| {
        out.push_str(title);
        out.push('\n');
        for (i, r) in rows.iter().enumerate() {
            out.push_str(format!("{i:02} {r}").trim_end());
            out.push('\n');
        }
    };
    section(&mut out, "## text", &text);
    section(&mut out, "## bg · surface per cell, blank = ui.background", &bgs);
    section(&mut out, "## fg · visible glyphs only, blank = unlisted color, UPPERCASE = bold", &fgs);
    // Legend in table order (then digits, then the specials) — independent of where marks first appear
    let rank = |keys: &[(vt100::Color, char, &str)], m: char| {
        keys.iter().position(|k| k.1 == m).unwrap_or(keys.len() + "123456789+~RB".find(m).unwrap_or(99))
    };
    used_bg.sort_by_key(|m| rank(&bg_keys, *m));
    used_fg.sort_by_key(|m| rank(&fg_keys, if *m == 'b' { 'B' } else { *m }));
    let bg_names: Vec<String> = used_bg
        .iter()
        .map(|m| match bg_keys.iter().find(|k| k.1 == *m) {
            Some(k) => format!("{m}={}", k.2),
            None if *m == '~' => "~=unpainted".into(),
            None if *m == 'R' => "R=reverse".into(),
            None => format!("{m}=other"),
        })
        .collect();
    let fg_names: Vec<String> = used_fg
        .iter()
        .map(|m| match fg_keys.iter().find(|k| k.1 == *m) {
            Some(k) => format!("{m}={}", k.2),
            None => "B=bold".into(),
        })
        .collect();
    out.push_str(&format!("## legend\nbg: {}\nfg: {}\n", bg_names.join("  "), fg_names.join("  ")));
    out
}

// ── Golden files ──────────────────────────────────────────────────────────

fn compare(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/snapshots").join(format!("{name}.txt"));
    if std::env::var_os("TARAE_BLESS").is_some_and(|v| v == "1") {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, actual).unwrap();
        return;
    }
    // The test's own name (one test may check several files)
    let thread = std::thread::current();
    let test = thread.name().and_then(|n| n.rsplit("::").next()).unwrap_or("snapshot_");
    let hint = format!("TARAE_BLESS=1 cargo test {test}");
    let Ok(expected) = std::fs::read_to_string(&path) else {
        panic!("no snapshot {} yet — create it with `{hint}`\n\n{actual}", path.display());
    };
    let expected = expected.replace("\r\n", "\n");
    if expected != actual {
        panic!(
            "snapshot {name} differs ({}) — `-` expected, `+` actual:\n\n{}\nIf the change is intended: `{hint}`",
            path.display(),
            line_diff(&expected, actual)
        );
    }
}

/// Unified-style line diff (LCS) with two lines of context around changes.
fn line_diff(a: &str, b: &str) -> String {
    let (a, b): (Vec<&str>, Vec<&str>) = (a.lines().collect(), b.lines().collect());
    let (n, m) = (a.len(), b.len());
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if a[i] == b[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    let mut ops: Vec<(char, &str)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n || j < m {
        if i < n && j < m && a[i] == b[j] {
            ops.push((' ', a[i]));
            (i, j) = (i + 1, j + 1);
        } else if i < n && (j == m || lcs[i + 1][j] >= lcs[i][j + 1]) {
            ops.push(('-', a[i]));
            i += 1;
        } else {
            ops.push(('+', b[j]));
            j += 1;
        }
    }
    let near = |k: usize| ops[k.saturating_sub(2)..(k + 3).min(ops.len())].iter().any(|o| o.0 != ' ');
    let mut out = String::new();
    let mut skipped = false;
    for (k, (op, line)) in ops.iter().enumerate() {
        if near(k) {
            out.push_str(&format!("{op} {line}\n"));
            skipped = false;
        } else if !skipped {
            out.push_str("  …\n");
            skipped = true;
        }
    }
    out
}

// ── Scenarios ─────────────────────────────────────────────────────────────

/// Normal editing: line numbers, cursorline, folded doc comment, path bar with the enclosing function.
#[test]
fn snapshot_editing() {
    let mut s = Shot::new(100, 30);
    s.file("src/demo.rs", DEMO_RS);
    s.keys("8ggww");
    s.check("editing");
}

/// Same at 50x15 — path bar and status line fold, text is clipped.
#[test]
fn snapshot_editing_narrow() {
    let mut s = Shot::new(50, 15);
    s.file("src/demo.rs", DEMO_RS);
    s.keys("8ggww");
    s.check("editing_narrow");
}

/// Status line with every segment, at four widths — least important segments drop first.
#[test]
fn snapshot_statusline_widths() {
    for (name, w) in [
        ("statusline_wide", 120),
        ("statusline_medium", 56),
        ("statusline_tight", 50),
        ("statusline_narrow", 30),
    ] {
        let mut s = Shot::new(w, 6);
        s.file("src/demo.rs", DEMO_RS);
        s.ed.git_branch = Some("main".into());
        let diag = |from, to, severity, message: &str| crate::lsp::Diagnostic {
            from,
            to,
            severity,
            message: message.into(),
            raw: json!({}),
            source: 0,
        };
        let use_at = DEMO_RS.find("HashMap;").unwrap();
        s.ed.doc_mut().lsp.diagnostics =
            vec![diag(use_at, use_at + 7, 1, "unresolved import"), diag(4, 7, 2, "unused import")];
        s.keys("C");
        s.check(name);
    }
}

/// Cursor on an underlined error whose detail is on its second line (+ a hint overlapping it) → the card by
/// the cursor with both in full, `code` in syntax colors.
fn diagnostic_card_shot() -> Shot {
    let mut s = Shot::new(90, 24);
    s.file("src/demo.rs", DEMO_RS);
    let at = DEMO_RS.find("word_counts(text);").unwrap();
    let diag = |from, severity, message: &str, raw| crate::lsp::Diagnostic {
        from,
        to: from + 11,
        severity,
        message: message.into(),
        raw,
        source: 0,
    };
    s.ed.doc_mut().lsp.diagnostics = vec![
        diag(
            at,
            1,
            "mismatched types\nexpected `HashMap<&str, u32>`, found `HashMap<&str, usize>`",
            json!({ "source": "rustc", "code": "E0308" }),
        ),
        diag(at - 9, 4, "expected due to this", json!({ "source": "rustc", "code": "E0308" })),
    ];
    s.keys("16Gfw");
    s
}

#[test]
fn snapshot_diagnostic_card() {
    diagnostic_card_shot().check("diagnostic_card");
}

/// The card only on an underline · only when the line end can't show it all · not in insert mode · Esc
/// hides it until the cursor leaves the line.
#[test]
fn diagnostic_card_shows_only_what_the_line_end_cannot() {
    let mut s = diagnostic_card_shot();
    let shown = |s: &mut Shot| {
        let bytes = s.frame();
        snapshot(&s.ed, "card", &bytes, s.w, s.h).contains("found HashMap")
    };
    assert!(shown(&mut s));
    s.keys("<esc>");
    assert!(!shown(&mut s), "Esc hides it");
    s.keys("l");
    assert!(!shown(&mut s), "still hidden on the same line");
    s.keys("jk");
    assert!(shown(&mut s), "back after leaving the line");
    s.keys("i");
    assert!(!shown(&mut s), "not while typing");
    s.keys("<esc>gh");
    assert!(!shown(&mut s), "same line, off the underline");
    s.keys("fw");
    assert!(shown(&mut s), "back on the underline");
    // One short single-line message fits at the line end — no card
    let at = DEMO_RS.find("word_counts(text);").unwrap();
    s.ed.doc_mut().lsp.diagnostics = vec![crate::lsp::Diagnostic {
        from: at,
        to: at + 11,
        severity: 2,
        message: "unused result".into(),
        raw: json!({ "source": "rustc" }),
        source: 0,
    }];
    let bytes = s.frame();
    let t = snapshot(&s.ed, "card", &bytes, s.w, s.h);
    assert!(!t.contains("  rustc"), "{}", &t[..t.find("## bg").unwrap()]);
    // The same message on a line too long for it → card
    s.ed.config.inline_diagnostics = false;
    let bytes = s.frame();
    assert!(snapshot(&s.ed, "card", &bytes, s.w, s.h).contains("  rustc"));
}

/// Soft wrap in prose: word-boundary rows, list items hanging under their text, continuation rows
/// without numbers, the cursor line's band across all its rows.
#[test]
fn snapshot_soft_wrap() {
    let mut s = Shot::new(60, 14);
    s.file(
        "notes.md",
        "# Notes\n\nLong lines in prose wrap at word boundaries instead of running off the right edge.\n\n\
         - A list item long enough to need a second row keeps its text aligned.\n- Short item\n",
    );
    s.keys("jj");
    s.check("soft_wrap");
}

/// Start screen (no file).
#[test]
fn snapshot_welcome() {
    let mut s = Shot::new(100, 30);
    s.check("welcome");
}

/// Which-key card after `space`.
#[test]
fn snapshot_which_key() {
    let mut s = Shot::new(100, 30);
    s.file("src/demo.rs", DEMO_RS);
    s.keys(" ");
    s.check("which_key");
}

/// Command palette (`space ?`) with a query typed — matched chars in accent, key hints dimmed.
#[test]
fn snapshot_command_palette() {
    let mut s = Shot::new(100, 30);
    s.file("src/demo.rs", DEMO_RS);
    s.keys(" ?split");
    s.check("command_palette");
}

/// File picker over the fixture folder, filtered — the matches under their folders — with the selected
/// file's preview (worker-filled).
#[test]
fn snapshot_file_picker() {
    let mut s = Shot::new(100, 30);
    let root = fixtures().join("demo");
    s.ed.open_picker(
        crate::picker::Picker::new("files in demo", Vec::new(), true).with_tree(root.clone(), None),
        Some(Box::new(move || Ok(crate::picker::file_items(&root)))),
    );
    s.until("file list", |ed| ed.picker.as_ref().is_some_and(|p| !p.loading));
    s.keys("main");
    let ready = |ed: &Editor| {
        let Some(p) = ed.picker.as_ref() else { return false };
        let Some((path, _)) = p.current().and_then(|i| i.action.preview_target()) else { return false };
        matches!(p.previews.get(path), Some(crate::picker::Preview::Ready { .. }))
    };
    s.until("preview", ready);
    s.check("file_picker");
}

/// Vertical split (`C-w v`) — two panes of the same file; the focused (right) one jumps to the end and
/// scrolls, the other keeps its own scroll.
#[test]
fn snapshot_split_vertical() {
    let mut s = Shot::new(100, 16);
    s.file("src/demo.rs", DEMO_RS);
    s.keys("<C-w>vG");
    s.check("split_vertical");
}

/// Doc comments render as markdown (bold, code, list, heading, code block) while the cursor is elsewhere.
#[test]
fn snapshot_doc_comment_folded() {
    const SRC: &str = r#"//! Tiny geometry helpers.

/// A point on the **plane**.
///
/// - `x` grows to the right
/// - `y` grows downward
///
/// # Examples
///
/// ```
/// let p = Point { x: 1, y: 2 };
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

impl Point {
    /// Distance to `other`, _squared_ — cheap to compare.
    pub fn dist2(self, other: Point) -> i32 {
        let (dx, dy) = (self.x - other.x, self.y - other.y);
        dx * dx + dy * dy
    }
}
"#;
    let mut s = Shot::new(100, 30);
    s.file("src/point.rs", SRC);
    s.keys("22gg");
    s.check("doc_comment_folded");
}

/// Error toast, frozen at full opacity (between fade-in and fade-out).
#[test]
fn snapshot_toast_error() {
    let mut s = Shot::new(100, 30);
    s.file("src/demo.rs", DEMO_RS);
    s.ed.set_error("Couldn't save demo.rs: permission denied");
    s.ed.set_warning("demo.rs changed on disk");
    // Half a second on the card: fully faded in, seconds before the fade-out starts (errors live 7 s,
    // warnings 6 s)
    let fresh = Instant::now().checked_sub(Duration::from_millis(500)).unwrap();
    for t in &mut s.ed.toasts {
        (t.at, t.shown) = (fresh, true);
    }
    assert_eq!(s.ed.toast_alpha(), 1.0);
    s.check("toast_error");
}

/// Grammar download offer card (offers are off in tests — on just here; the card only, no download).
#[test]
fn snapshot_offer_grammar() {
    let mut s = Shot::new(100, 30);
    s.ed.offer_popups = true;
    s.file("app.py", "def greet(name):\n    return f\"hello, {name}\"\n\n\nprint(greet(\"tarae\"))\n");
    s.ed.offer_grammar(crate::syntax::spec("python").unwrap());
    assert_eq!(s.ed.offers.len(), 1);
    s.check("offer_grammar");
}

/// A plugin's panel with the keys: its name in the accent, its keys, the selected row marked, its status.
#[test]
fn snapshot_plugin_panel() {
    let mut s = Shot::new(100, 30);
    s.file("app.yaml", "kind: Deployment\nmetadata:\n  name: web\n");
    let dir = std::env::temp_dir().join(format!("tarae-shot-plugins-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("demo.wasm"), include_bytes!("../plugin-sdk/demo.wasm")).unwrap();
    s.ed.plugins_sync(crate::plugin::scan(&dir));
    s.until("the plugin", |ed| ed.plugin_command("table").is_some());
    s.ed.offers.clear(); // (its approval card — another shot)
    s.ed.plugin_run("table", "").unwrap();
    s.until("its panel", |ed| ed.plugins.as_ref().is_some_and(|p| p.panel.is_some()));
    s.keys("jj");
    s.check("plugin_panel");
    std::fs::remove_dir_all(dir).ok();
}

/// Offer to install a missing language server — its command in sight, and `N Never`.
#[test]
fn snapshot_offer_install() {
    let mut s = Shot::new(100, 30);
    s.ed.offer_popups = true;
    s.file("main.go", "package main\n\nfunc main() {\n\tprintln(\"hi\")\n}\n");
    let run: Vec<String> = ["go", "install", "golang.org/x/tools/gopls@latest"].map(String::from).to_vec();
    s.ed.push_offer(crate::offer::Offer::new(
        "Go's language server".into(),
        &format!("gopls isn't installed. Install it with:\n{}", crate::install::shown(&run)),
        "Installing gopls…",
        "Couldn't install gopls".into(),
        crate::offer::What::Install {
            tool: "gopls".into(),
            run,
            cwd: std::env::temp_dir(),
            then: crate::install::Then::Lsp,
        },
    ));
    s.check("offer_install");
}

/// A command Claude wants to run — shown whole (a commit message past four lines too), `y Run` · `n No`.
#[test]
fn snapshot_offer_run() {
    let mut s = Shot::new(100, 30);
    s.ed.offer_popups = true;
    s.file("src/demo.rs", DEMO_RS);
    let command = "git commit -m 'Count words: the words in the text, counted\n\nSplits on whitespace, so \
                   \"a  b\" is two words, and keeps the counts in a map from word to how many times it came, \
                   printed after the text.\n\nThe tests cover the empty text and repeated words.'";
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "run", "arguments": {"command": command}}});
    assert!(crate::tools::mcp(&mut s.ed, 0, "c1", &msg).is_none(), "it waits for the user");
    s.check("offer_run");
}

/// `:ask` review — the answer shown as an inline diff inside the buffer (fake claude).
#[test]
fn snapshot_ask_review() {
    let mut s = Shot::new(100, 30);
    s.file("src/demo.rs", DEMO_RS);
    let reply = "<r i=\"1\">    println!(\"{} distinct words in {:?}\", counts.len(), text);\n</r>";
    let json = json!({"type": "result", "is_error": false, "result": reply}).to_string();
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), format!("read -r line; printf '%s\\n' '{json}'")];
    s.keys("17ggx i");
    s.keys("show the text too<ret>");
    s.until("review", |ed| ed.review.as_ref().is_some_and(|r| r.ready));
    s.ed.toasts.clear(); // "claude answered in 0.0s" — wall-clock time
    s.check("ask_review");
}

/// Chat panel, empty state (a fake claude that just waits).
#[test]
fn snapshot_chat_empty() {
    let mut s = Shot::new(100, 30);
    s.file("src/demo.rs", DEMO_RS);
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" l");
    assert!(s.ed.chat.as_ref().is_some_and(|c| c.msgs.is_empty()), "chat opened without a note");
    s.check("chat_empty");
}

/// Chat panel after Claude explored the project in follow mode: a folded thought, tool rows
/// (search·find·read) and an answer whose `path:line` references are underlined links.
#[test]
fn snapshot_chat_explore() {
    use crate::chat::{Link, Look, Msg, Role};
    let mut s = Shot::new(100, 30);
    s.file("src/demo.rs", DEMO_RS);
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" l");
    let msg = |role, text: &str, chip: &str, look: Option<Look>| Msg {
        role,
        text: text.into(),
        chip: chip.into(),
        look,
        call: String::new(),
        attached: None,
        cache: Default::default(),
    };
    let look = |verb, what: &str, pattern, range: &str, link: Option<Link>| {
        Some(Look { verb, what: what.into(), pattern, range: range.into(), link })
    };
    let lib = std::path::PathBuf::from("/p/src/lib.rs");
    s.ed.chat.as_mut().unwrap().follow.on = true;
    s.ed.chat.as_mut().unwrap().msgs = vec![
        msg(Role::User, "Who calls word_counts?", "demo.rs · L5", None),
        msg(
            Role::Thought,
            "**Finding the callers**\n\nIt's public, so other modules may call it — search before reading.",
            "",
            None,
        ),
        msg(Role::Tool, "", "", look("search", "word_counts\\(", true, "in src", None)),
        msg(Role::Tool, "", "", look("find", "src/**/*.rs", true, "", None)),
        msg(
            Role::Tool,
            "",
            "",
            look("read", "src/lib.rs", false, "L1–40", Some(Link { path: lib, line: 0 })),
        ),
        msg(
            Role::Assistant,
            "Only `main` does, at src/demo.rs:15. `src/lib.rs:12` has a copy that splits on commas.",
            "",
            None,
        ),
        msg(Role::Note, "2.4s", "", None),
    ];
    s.check("chat_explore");
}

/// An answer with tables in a 60-cell chat: one that fits (aligned columns, a rule under the header), one
/// too wide (cells wrapping in their columns, a blank line between rows) and one stacked row by row (its
/// header names dim, not faint — they say what each value is).
#[test]
fn snapshot_chat_table() {
    use crate::chat::{Msg, Role};
    let mut s = Shot::new(150, 44);
    s.file("src/demo.rs", DEMO_RS);
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" l");
    let msg = |role, text: &str| Msg {
        role,
        text: text.into(),
        chip: String::new(),
        look: None,
        call: String::new(),
        attached: None,
        cache: Default::default(),
    };
    let answer = "Two ways to draw a cell that doesn't fit:\n\n\
                  | 방식 | 장점 | 단점 |\n|---|---|---|\n\
                  | 셀 안에서 줄바꿈 | 내용이 전부 보이고 열 정렬이 유지된다 | 행 높이가 들쭉날쭉해진다 |\n\
                  | 잘라내기(`…`) | 한 행 = 한 줄, 훑어보기 좋다 | 잘린 내용은 볼 방법이 없다 |\n\n\
                  The work, roughly:\n\n\
                  | 부분 | 줄 수 | 비고 |\n|---|---:|:---:|\n| `markdown.rs` 표 파싱 | ~120 | **핵심** |\n\
                  | `wrap` 배치 | ~80 | |\n| 테스트 | ~150 | 스냅샷 1 |\n\n\
                  And a wide one, row by row:\n\n\
                  | 설정 | 기본값 | 범위 | 설명 | 예시 | 비고 |\n|---|---|---|---|---|---|\n\
                  | `chat.width` | `0.4` | `0.2–0.6` | 채팅 패널이 차지하는 화면 비율 | `:set chat.width 0.5` | 좁으면 전체 화면 |";
    s.ed.chat.as_mut().unwrap().msgs =
        vec![msg(Role::User, "표는 어떻게 그릴까?"), msg(Role::Assistant, answer), msg(Role::Note, "3.1s")];
    s.check("chat_table");
}

/// While Claude works in follow mode the panel is minimized to a floating card (the code gets the whole
/// width); `space l` brings it back for the turn, and without follow it stays docked.
#[test]
fn chat_minimizes_while_following() {
    let mut s = Shot::new(100, 30);
    s.file("src/demo.rs", DEMO_RS);
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" l");
    s.frame();
    let docked = s.ed.screen.chat_x;
    assert!(docked.is_some());
    s.keys("why?<ret>");
    assert!(s.ed.chat.as_ref().unwrap().busy());
    s.frame();
    assert_eq!(s.ed.screen.chat_x, None, "minimized: no docked panel");
    assert!(s.ed.screen.chat_mini.get().is_some(), "a floating card instead");
    s.keys("<esc> l");
    assert_eq!(s.ed.screen.chat_x, docked, "space l brings the panel back for this turn");
    s.ed.chat.as_mut().unwrap().unfolded = false;
    s.ed.chat.as_mut().unwrap().follow.on = false;
    s.frame();
    assert_eq!(s.ed.screen.chat_x, docked, "not following = docked");
}

/// A command Claude wants to run, asked while it works in follow mode: the minimized chat moves to the top
/// right, out of the card's way, the card's buttons in sight.
#[test]
fn snapshot_follow_offer_run() {
    let mut s = Shot::new(100, 30);
    s.file("src/demo.rs", DEMO_RS);
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" lcommit it<ret>");
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "run", "arguments": {"command": "git commit -am 'Count words'"}}});
    assert!(crate::tools::mcp(&mut s.ed, 0, "c1", &msg).is_none());
    s.frame();
    let mini = s.ed.screen.chat_mini.get().expect("the minimized chat shows");
    for &(row, from, to, _) in &s.ed.screen.offer_buttons {
        let covers = mini.1 <= row && row < mini.1 + mini.3 && mini.0 < to && from < mini.0 + mini.2;
        assert!(!covers, "the chat {mini:?} is under the card's buttons (row {row})");
    }
    s.check("follow_offer_run");
}

/// A note on several lines keeps them in sight: its card goes under (or over) all of them — the code it's
/// about stays readable, wherever the note sits on screen.
#[test]
fn a_note_card_leaves_its_lines_in_sight() {
    for (first, cursor) in [(4, 4), (4, 7), (20, 20), (60, 62), (74, 79)] {
        let mut s = Shot::new(100, 30);
        let text: String = (1..=80).map(|i| format!("let line_{i} = {i};\n")).collect();
        s.file("src/demo.rs", &text);
        let path = s.ed.doc().path.clone().unwrap();
        s.ed.goto_line(cursor);
        // Where the lines are before any card covers them
        let mut parser = vt100::Parser::new(s.h, s.w, 0);
        parser.process(&s.frame());
        let rows: Vec<String> = parser.screen().rows(0, s.w).collect();
        crate::notes::add(
            &mut s.ed,
            &path,
            first..first + 6,
            crate::notes::By::Claude,
            "Six lines, one remark.",
        );
        s.frame();
        let (_, y, _, h) = s.ed.screen.note_card.get().unwrap_or_else(|| panic!("a card at {first}"));
        for k in first + 1..=first + 6 {
            let r = rows.iter().position(|r| r.contains(&format!("let line_{k} = "))).expect("on screen");
            assert!(!(y..y + h).contains(&r), "note at {first}: line {k} (row {r}) under the card {y}+{h}");
        }
    }
}

/// A thread too long for the room keeps its newest words — under the name of whoever says them (`¶ claude`),
/// the cut words going on with `…`.
#[test]
fn a_cut_note_keeps_who_is_talking() {
    let mut s = Shot::new(90, 16);
    let text: String = (1..=40).map(|i| format!("let line_{i} = {i};\n")).collect();
    s.file("src/demo.rs", &text);
    let path = s.ed.doc().path.clone().unwrap();
    let long: String = (1..=12).map(|i| format!("Sentence number {i} of a long answer.\n")).collect();
    let id = crate::notes::add(&mut s.ed, &path, 5..6, crate::notes::By::You, "why?");
    crate::notes::tool_call(&mut s.ed, &serde_json::json!({"reply_to": id, "text": long})).unwrap();
    s.ed.goto_line(5);
    let bytes = s.frame();
    let (_, y, _, h) = s.ed.screen.note_card.get().expect("a card");
    let mut parser = vt100::Parser::new(s.h, s.w, 0);
    parser.process(&bytes);
    let rows: Vec<String> = parser.screen().rows(0, s.w).collect();
    let card: Vec<&str> = rows[y..y + h]
        .iter()
        .map(|r| r.trim())
        .filter(|r| !r.is_empty() && !r.starts_with(['▄', '▀']))
        .collect();
    assert!(card[0].starts_with("¶ claude"), "{card:#?}");
    assert!(card[1].starts_with("… Sentence number"), "{card:#?}");
    assert!(card.iter().any(|r| r.contains("Sentence number 12")), "the newest stays: {card:#?}");
    assert!(card.last().unwrap().contains("space n"), "and the keys: {card:#?}");
}

/// The thought card is the turn's words as a log: a new thought goes under the last one (thinking comes in
/// bursts — a lone "I should" must not wipe the whole thought before it).
#[test]
fn the_thought_card_keeps_the_turns_thinking() {
    use crate::chat::{Msg, Role};
    let mut s = Shot::new(112, 30);
    let text: String = (1..=80).map(|i| format!("let line_{i} = {i};\n")).collect();
    s.file("src/demo.rs", &text);
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" lhow?<ret>");
    let c = s.ed.chat.as_mut().unwrap();
    c.follow.on = true;
    c.msgs.push(Msg::new(Role::Thought, "The user asks how the list is filtered, so I read the picker.", ""));
    c.msgs.push(Msg::new(Role::Assistant, "Let me check the worker side.", ""));
    c.msgs.push(Msg::new(Role::Thought, "I should", ""));
    let bytes = s.frame();
    let (x, y, w, h) = s.ed.screen.thought_card.get().expect("the card shows");
    let mut parser = vt100::Parser::new(s.h, s.w, 0);
    parser.process(&bytes);
    let card: Vec<String> = (y..y + h)
        .map(|r| parser.screen().contents_between(r as u16, x as u16, r as u16, (x + w) as u16))
        .collect();
    let all = card.join("\n");
    let at = |needle: &str| all.find(needle).unwrap_or_else(|| panic!("{needle:?} in {card:#?}"));
    assert!(at("read the picker") < at("Let me check") && at("Let me check") < at("I should"), "{card:#?}");
}

/// Follow mode is one card in one place (U4): the minimized chat and Claude's words are the same card, at the
/// bottom right of the code — it stays there as Claude reads elsewhere, and goes to the top right only while
/// the lines it reads run under it.
#[test]
fn follow_is_one_card_in_one_place() {
    use crate::chat::{Look, Msg, Role};
    let mut s = Shot::new(112, 30);
    // Lines 18–25 long enough to reach the right edge; the rest short
    let text: String = (1..=80)
        .map(|i| match i {
            18..=25 => format!("let line_{i} = \"{}\";\n", "x".repeat(90)),
            _ => format!("let line_{i} = {i};\n"),
        })
        .collect();
    s.file("src/demo.rs", &text);
    let path = s.ed.doc().path.clone().unwrap();
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" lwhere?<ret>");
    let gaze = |s: &mut Shot, lines: std::ops::Range<usize>| {
        let look = Look {
            verb: "read",
            what: "src/demo.rs".into(),
            pattern: false,
            range: format!("L{}–{}", lines.start + 1, lines.end),
            link: None,
        };
        let c = s.ed.chat.as_mut().unwrap();
        c.follow.on = true;
        c.msgs.push(Msg::new(Role::Thought, "Reading where it starts.", ""));
        c.follow.gaze = Some(crate::follow::Gaze {
            look,
            path: Some(path.clone()),
            lines: Some(lines),
            why: None,
            step: None,
        });
        s.frame();
        let card = s.ed.screen.thought_card.get().expect("the card shows");
        assert_eq!(s.ed.screen.chat_mini.get(), Some(card), "the minimized chat is this card");
        card
    };
    // (its corner — the card grows with the words, up from the bottom)
    let corner = |r: (usize, usize, usize, usize)| (r.0 + r.2, r.1 + r.3);
    let first = gaze(&mut s, 1..5);
    let pane = s.ed.screen.panes[0].1;
    assert_eq!(corner(first), (pane.x + pane.w - 1, pane.y + pane.h), "bottom right");
    assert_eq!(corner(gaze(&mut s, 8..12)), corner(first), "Claude reads elsewhere: the card stays put");
    // The lines read run under it: the top right, out of their way
    let moved = gaze(&mut s, 17..25);
    assert_eq!((moved.0, moved.1), (first.0, pane.y), "{moved:?}");
}

/// Crowded screens: with Claude at work in follow mode and a note under the cursor, the note card, the
/// follow card and a notification never overlap — at any size (a card with no room is skipped; a
/// notification with no room goes into the status line).
#[test]
fn floating_cards_never_overlap() {
    use crate::chat::{Look, Msg, Role};
    for (w, h) in [(112, 30), (100, 30), (80, 24), (72, 20), (140, 45)] {
        let mut s = Shot::new(w, h);
        let text: String =
            (1..=80).map(|i| format!("let line_{i} = {i}; // some code on line {i}\n")).collect();
        s.file("src/demo.rs", &text);
        s.ed.config.llm.command = "sh".into();
        s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        s.keys(" lwhy?<ret>");
        let path = s.ed.doc().path.clone().unwrap();
        let long = "A longer note that wraps over a few rows so the card is tall — the kind Claude writes when \
                    something needs explaining; it goes on for a while to fill the card.";
        for _ in 0..3 {
            crate::notes::add(&mut s.ed, &path, 39..40, crate::notes::By::Claude, long);
        }
        s.ed.goto_line(39);
        let c = s.ed.chat.as_mut().unwrap();
        c.msgs.push(Msg {
            role: Role::Thought,
            text: "Thinking hard about it for a while.\n".repeat(6),
            chip: String::new(),
            look: None,
            call: String::new(),
            attached: None,
            cache: Default::default(),
        });
        let look = Look {
            verb: "read",
            what: "src/demo.rs".into(),
            pattern: false,
            range: "L40–60".into(),
            link: None,
        };
        c.msgs.push(Msg {
            role: Role::Tool,
            text: String::new(),
            chip: String::new(),
            look: Some(look.clone()),
            call: String::new(),
            attached: None,
            cache: Default::default(),
        });
        c.follow.gaze = Some(crate::follow::Gaze {
            look,
            path: Some(path.clone()),
            lines: Some(39..60),
            why: None,
            step: None,
        });
        s.ed.set_status("Saved src/demo.rs · 80 lines");
        let bytes = s.frame();
        // (the minimized chat and the thought card are one card — U4)
        assert_eq!(s.ed.screen.chat_mini.get(), s.ed.screen.thought_card.get(), "{w}x{h}: one follow card");
        let cards: Vec<_> = [&s.ed.screen.note_card, &s.ed.screen.thought_card, &s.ed.screen.toast_card]
            .iter()
            .filter_map(|c| c.get())
            .collect();
        if s.ed.screen.toast_card.get().is_none() {
            let mut vt = vt100::Parser::new(h, w, 0);
            vt.process(&bytes);
            let status =
                vt.screen().contents().lines().nth(usize::from(h) - 2).unwrap_or_default().to_string();
            assert!(
                status.contains("Saved src/demo.rs"),
                "{w}x{h}: no card → the status line has it: {status}"
            );
        }
        assert!(s.ed.screen.note_card.get().is_some(), "{w}x{h}: the note card shows");
        if h >= 30 {
            assert_eq!(
                cards.len(),
                3,
                "{w}x{h}: all three fit — note {:?} follow {:?} toast {:?}",
                s.ed.screen.note_card.get(),
                s.ed.screen.thought_card.get(),
                s.ed.screen.toast_card.get()
            );
        }
        for (i, a) in cards.iter().enumerate() {
            for b in &cards[i + 1..] {
                let hit = a.0 < b.0 + b.2 && b.0 < a.0 + a.2 && a.1 < b.1 + b.3 && b.1 < a.1 + a.3;
                assert!(!hit, "{w}x{h}: {a:?} overlaps {b:?}");
            }
        }
    }
}

/// A code review over a file: the rail of changed files on the left (current on the accent bar, drafts
/// counted), removed lines in place with their old numbers, changed and added lines on a green wash, and a
/// draft comment at its line end.
#[test]
fn snapshot_review() {
    use crate::codereview::{Base, CodeReview, File};
    let mut s = Shot::new(112, 30);
    let now = DEMO_RS
        .replace(
            "    let mut counts = HashMap::new();\n",
            "    let mut counts: HashMap<&str, usize> = HashMap::new();\n",
        )
        .replace(
            "        *counts.entry(word).or_insert(0) += 1;\n",
            "        let n = counts.entry(word).or_insert(0);\n        *n += 1;\n",
        )
        .replace("    println!(\"{} distinct words\", counts.len()); // 타래\n", "");
    s.file("src/demo.rs", &now);
    let cwd = std::env::current_dir().unwrap();
    let file = |rel: &str, added, removed, new| File {
        rel: rel.into(),
        path: cwd.join(rel),
        added,
        removed,
        new,
        gone: false,
        binary: false,
    };
    let files = vec![
        file("src/demo.rs", 3, 3, false),
        file("src/lib.rs", 40, 0, true),
        file("README.md", 2, 1, false),
    ];
    s.ed.code_review = Some(CodeReview::for_test(Base::Head, cwd.clone(), files));
    let doc = s.ed.doc_mut();
    doc.review_base = Some(Rope::from_str(DEMO_RS));
    doc.git_base = Some(std::sync::Arc::new(DEMO_RS.to_string()));
    doc.git_hunks = crate::git::hunks(DEMO_RS, &now);
    doc.git_version = doc.version();
    let path = doc.path.clone().unwrap();
    s.keys(":9<ret> vm");
    crate::notes::add(&mut s.ed, &path, 8..9, crate::notes::By::You, "Why split this into two lines?");
    s.ed.notes.list[0].thread[0].draft = true;
    s.ed.notes.list[0].waiting = false;
    s.keys("gg");
    s.check("review");
}

/// Many removed lines above the cursor: the view scrolls by screen rows, so the cursor stays on screen.
#[test]
fn review_keeps_the_cursor_on_screen_below_a_big_deletion() {
    use crate::codereview::{Base, CodeReview};
    let mut s = Shot::new(100, 20);
    let base: String = (0..60).map(|i| format!("line {i}\n")).collect();
    let now: String = (0..60).filter(|i| !(10..50).contains(i)).map(|i| format!("line {i}\n")).collect();
    s.file("src/demo.rs", &now);
    s.ed.code_review = Some(CodeReview::for_test(Base::Head, std::env::current_dir().unwrap(), Vec::new()));
    let doc = s.ed.doc_mut();
    doc.review_base = Some(Rope::from_str(&base));
    doc.git_hunks = crate::git::hunks(&base, &now);
    doc.git_version = doc.version();
    for target in [11, 19, 0, 10] {
        s.ed.goto_line(target);
        s.frame();
        let rows = &s.ed.screen.panes[0].3;
        assert!(rows.iter().any(|r| r.0 == target), "line {target} on screen: {rows:?}");
    }
}

/// The review's rail with more files than rows: a click opens the file under it without the list sliding
/// (the next click lands where it's aimed), the wheel scrolls it, and a file shown from elsewhere (`]g`)
/// scrolls into view.
#[test]
fn review_rail_scrolls_by_itself_only_to_show_the_file() {
    use crate::codereview::{Base, CodeReview, File};
    use crate::event::{Mouse, MouseKind};
    let dir = std::env::temp_dir().join(format!("tarae-rail-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let dir = std::fs::canonicalize(dir).unwrap();
    let files: Vec<File> = (0..30)
        .map(|i| {
            let rel = format!("f{i:02}.txt");
            std::fs::write(dir.join(&rel), "x\n").unwrap();
            File { path: dir.join(&rel), rel, added: 1, removed: 0, new: true, gone: false, binary: false }
        })
        .collect();
    let mut s = Shot::new(100, 24);
    s.ed.open(&dir.join("f00.txt")).unwrap();
    s.ed.code_review = Some(CodeReview::for_test(Base::Head, dir.clone(), files));
    s.frame();
    let rail = |s: &Shot| s.ed.screen.review_rail.borrow().clone();
    let click = |s: &mut Shot, kind, y: usize| {
        s.ed.handle_mouse(Mouse { kind, x: 4, y: y as u16, alt: false });
        s.frame();
    };
    let screen = |s: &mut Shot| {
        let mut p = vt100::Parser::new(s.h, s.w, 0);
        p.process(&s.frame());
        p.screen().contents()
    };
    let before = rail(&s);
    assert!(before.len() < 30, "more files than rows: {before:?}");
    // The files out of sight are counted
    let text = screen(&mut s);
    assert!(text.contains(&format!("↓ {} more", 30 - before.len())) && !text.contains('↑'), "{text}");
    // Clicks on the last row, then on the first: each opens the file it was on
    let (last, first) = (*before.last().unwrap(), before[0]);
    click(&mut s, MouseKind::Down, last.0);
    assert_eq!(s.ed.doc().path.as_deref(), Some(dir.join(format!("f{:02}.txt", last.1)).as_path()));
    assert_eq!(rail(&s), before, "the list stays where it was");
    click(&mut s, MouseKind::Down, first.0);
    assert_eq!(s.ed.doc().path.as_deref(), Some(dir.join(format!("f{:02}.txt", first.1)).as_path()));
    // The wheel scrolls it (and leaves the code alone)
    click(&mut s, MouseKind::ScrollDown, first.0);
    assert_eq!(rail(&s)[0].1, first.1 + 3, "three files down");
    assert!(screen(&mut s).contains("↑ 3 more"));
    click(&mut s, MouseKind::ScrollUp, first.0);
    assert_eq!(rail(&s), before);
    for _ in 0..20 {
        click(&mut s, MouseKind::ScrollDown, first.0);
    }
    assert_eq!(rail(&s).last().unwrap().1, 29, "no further than the last file");
    assert!(!screen(&mut s).contains('↓'), "nothing below the last file");
    // Another file shown from elsewhere comes into view
    crate::codereview::show(&mut s.ed, 5, false);
    s.frame();
    assert!(rail(&s).iter().any(|r| r.1 == 5), "{:?}", rail(&s));
    std::fs::remove_dir_all(&dir).ok();
}

/// Completion card with the selected item's docs — the list comes from a fake server (`cat`), the
/// response is injected with `on_lsp_message`.
#[test]
fn snapshot_lsp_completion() {
    let log = std::env::temp_dir().join(format!("tarae-snapshot-lsp-{}.log", std::process::id()));
    let cfg = format!(
        "[lsp.fake]\ncommand = \"sh\"\nargs = [\"-c\", \"cat > '{}'\"]\n[lang.rust]\nlsp = [\"fake\"]\n",
        log.display()
    );
    let (config, warnings) = crate::config::parse(&cfg);
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut s = Shot::new(100, 30);
    let lsp = config.editor.lsp;
    s.ed.config.lsp = lsp;
    let id = s.file("src/demo.rs", "fn main() {\n    let v = vec![3, 1, 2];\n    \n}\n");
    s.ed.attach_lsp(id);
    let cid = s.ed.doc().lsp.client.expect("fake server attached");
    let caps = json!({ "positionEncoding": "utf-8", "completionProvider": { "triggerCharacters": ["."] } });
    s.ed.on_lsp_message(cid, json!({ "id": 0, "result": { "capabilities": caps } }));
    s.keys("jjAv.");
    let id =
        s.ed.lsp
            .pending
            .iter()
            .filter(|(_, p)| matches!(p.kind, crate::lsp_editor::Kind::Completion))
            .map(|((_, id), _)| *id)
            .max()
            .expect("completion requested");
    let item = |n: u8, label: &str, detail: &str| json!({ "label": label, "kind": 2, "detail": detail, "sortText": n.to_string() });
    let mut push = item(0, "push", "fn(&mut self, value: i32)");
    push["documentation"] = json!({ "kind": "markdown", "value": "Appends an element to the back of a collection.\n\n# Panics\n\nPanics if the new capacity exceeds `isize::MAX` bytes." });
    let items = json!({ "isIncomplete": false, "items": [
        push,
        item(1, "pop", "fn(&mut self) -> Option<i32>"),
        item(2, "len", "fn(&self) -> usize"),
        item(3, "iter", "fn(&self) -> Iter<'_, i32>"),
        item(4, "capacity", "fn(&self) -> usize"),
    ]});
    s.ed.on_lsp_message(cid, json!({ "id": id, "result": items }));
    s.frame();
    s.keys("<tab>");
    s.settle();
    assert!(s.ed.completion.as_ref().is_some_and(|c| c.docs.is_some()), "docs shown for the selected item");
    // "fake ready" expires on the wall clock — a slow machine still has it; the snapshot is about completion
    s.ed.toasts.clear();
    s.check("lsp_completion");
    let _ = std::fs::remove_file(&log);
}

/// The side panel's files tab beside the code, following the editor: the current file's folders open,
/// its row marked; folded folders say how many files they hold.
#[test]
fn snapshot_side_panel_files() {
    let mut s = Shot::new(112, 24);
    s.file("src/demo.rs", DEMO_RS);
    let rels = [
        "Cargo.toml",
        "README.md",
        "docs/features.md",
        "docs/configuration.md",
        "src/demo.rs",
        "src/lib.rs",
        "src/ui/card.rs",
        "src/ui/theme.rs",
        "tests/e2e.rs",
    ];
    let root = std::env::current_dir().unwrap();
    let mut sb = crate::sidebar::Sidebar::with_files(root, rels.iter().map(|r| r.to_string()).collect());
    sb.focus = false;
    s.ed.sidebar = Some(sb);
    s.keys("j");
    s.check("side_panel_files");
}

/// The outline tab, focused: definitions with their kind glyphs, the selection on the accent bar.
#[test]
fn snapshot_side_panel_outline() {
    let mut s = Shot::new(112, 24);
    s.file("src/demo.rs", DEMO_RS);
    s.ed.sidebar_show(crate::sidebar::Mode::Outline);
    s.until("outline", |ed| ed.sidebar.as_ref().is_some_and(|sb| !sb.lines().is_empty()));
    s.keys("j");
    s.check("side_panel_outline");
}

/// The list tab: a build's places under their files, severity glyphs, the current one marked.
#[test]
fn snapshot_side_panel_list() {
    let mut s = Shot::new(112, 24);
    let id = s.file("src/demo.rs", DEMO_RS);
    let cwd = std::env::current_dir().unwrap();
    let item = |rel: &str, line, message: &str, severity| crate::quickfix::Item {
        path: cwd.join(rel),
        line,
        col: 4,
        message: message.into(),
        severity,
        mark: None,
    };
    s.ed.qlist_set(
        "cargo check",
        vec![
            item("src/demo.rs", 5, "unused variable: `counts`", 2),
            item("src/demo.rs", 13, "mismatched types", 1),
            item("src/ui/card.rs", 40, "cannot find value `width` in this scope", 1),
        ],
    );
    s.ed.toasts.clear();
    s.ed.sidebar_show(crate::sidebar::Mode::List);
    s.keys("<ret>");
    assert_eq!(s.ed.doc().id, id);
    // The item's message toast, frozen at full opacity (a slow machine would catch it fading in)
    let fresh = Instant::now().checked_sub(Duration::from_millis(500)).unwrap();
    for t in &mut s.ed.toasts {
        (t.at, t.shown) = (fresh, true);
    }
    s.check("side_panel_list");
}

/// The git tab: changes and staged files as sections, status letters in the gutter's colors, line counts.
#[test]
fn snapshot_side_panel_git() {
    let mut s = Shot::new(112, 24);
    s.file("src/demo.rs", DEMO_RS);
    let cwd = std::env::current_dir().unwrap();
    let mut st = crate::gitpanel::parse_v2(
        "# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -0\0\
         1 .M N... 100644 100644 100644 0 0 src/demo.rs\0\
         1 M. N... 100644 100644 100644 0 0 README.md\0\
         1 .D N... 100644 100644 000000 0 0 src/old.rs\0\
         ? src/ui/card.rs\0",
    );
    for (e, (a, r)) in st.unstaged.iter_mut().zip([(3, 1), (0, 12), (40, 0)]) {
        (e.added, e.removed) = (Some(a), Some(r));
    }
    (st.staged[0].added, st.staged[0].removed) = (Some(2), Some(0));
    st.top = cwd.clone();
    st.generation = 1;
    s.ed.git_status = Some(st.with_rows());
    s.ed.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, cwd));
    s.keys("j");
    s.check("side_panel_git");
}

/// The task panel after a failed build: its output, the places it found counted, how long it took.
#[test]
fn snapshot_task_panel() {
    let mut s = Shot::new(100, 24);
    s.file("src/demo.rs", DEMO_RS);
    let cwd = std::env::current_dir().unwrap();
    s.ed.task_start(crate::task::Task {
        name: "check".into(),
        cmd: "echo '   Compiling demo v0.1.0'; echo 'error[E0308]: mismatched types'; \
              echo ' --> src/snapshots/fixtures/demo/src/main.rs:1:4'; echo 'error: could not compile demo'; \
              exit 101"
            .into(),
        cwd,
    });
    s.until("task", |ed| ed.task_run.as_ref().is_some_and(|r| r.took.is_some()));
    s.ed.task_run.as_mut().unwrap().took = Some(Duration::from_millis(1830));
    s.ed.toasts.clear();
    s.check("task_panel");
}

/// What the sandbox blocked in Claude's command, acted on: the panel's row with its key, the list of what
/// could open (folders widest last, a port, running it outside), and a folder's card. macOS only — the
/// fixed folder is in `/private/tmp` so the paths read the same every run. With `TARAE_FRAMES_DIR` set,
/// each frame is written there too (`cargo xtask shot` draws them: `TARAE_SHOT_REPLAY`).
#[cfg(target_os = "macos")]
#[test]
fn snapshot_sandbox_blocked() {
    let cache = PathBuf::from("/private/tmp/tarae-shot-blocked/npm-cache");
    std::fs::create_dir_all(&cache).unwrap();
    let mut s = Shot::new(100, 30);
    s.file("src/demo.rs", DEMO_RS);
    let watch = crate::sandbox::Watch::quiet("t.");
    let d = |op: &str, target: &str| crate::sandbox::Denial {
        process: "npm".into(),
        op: op.into(),
        target: target.into(),
    };
    watch.record(d("file-write-create", "/private/tmp/tarae-shot-blocked/npm-cache/_cacache/index-v5"));
    watch.record(d("network-outbound", "remote:*:5432"));
    let policy = crate::sandbox::Policy {
        watch: Some(watch),
        ..crate::sandbox::Policy::for_project(&crate::sandbox::Env::default())
    };
    let task = crate::task::Task { name: "npm test".into(), cmd: "npm test".into(), cwd: "/p".into() };
    let mut r = crate::task::Run::for_test(task);
    r.state = crate::task::State::Failed(Some(1));
    r.took = Some(Duration::from_millis(2410));
    r.by_claude = true;
    r.guard = Some(crate::sandbox::Guard::In(policy));
    r.output = vec![(true, "npm ERR! code EPERM".into()), (true, "npm ERR! syscall open".into())];
    s.ed.task_run = Some(r);
    let dump = |s: &mut Shot, name: &str| {
        if let Some(dir) = std::env::var_os("TARAE_FRAMES_DIR") {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(Path::new(&dir).join(format!("{name}.bin")), s.frame()).unwrap();
        }
    };
    s.check("sandbox_blocked_panel");
    dump(&mut s, "sandbox_blocked_panel");
    s.keys(" ms");
    s.check("sandbox_blocked_list");
    dump(&mut s, "sandbox_blocked_list");
    s.keys("<ret>");
    s.check("sandbox_blocked_card");
    dump(&mut s, "sandbox_blocked_card");
}

/// Claude's command in the task panel: no `space m m again` — that runs the user's task, not this one.
#[test]
fn claudes_command_offers_no_again() {
    let hints = |by_claude: bool, found: (usize, usize)| {
        let mut s = Shot::new(100, 24);
        s.file("src/demo.rs", DEMO_RS);
        let task = crate::task::Task { name: "cargo build".into(), cmd: "true".into(), cwd: "/p".into() };
        let mut r = crate::task::Run::for_test(task);
        (r.by_claude, r.found, r.took) = (by_claude, found, Some(Duration::from_millis(40)));
        s.ed.task_run = Some(r);
        let mut vt = vt100::Parser::new(24, 100, 0);
        vt.process(&s.frame());
        let row = vt
            .screen()
            .contents()
            .lines()
            .find(|l| l.contains("cargo build"))
            .unwrap_or_default()
            .to_string();
        assert!(row.contains("space m c"), "the panel's header: {row}");
        row
    };
    assert!(hints(false, (0, 0)).contains("space m m"), "the user's task: again");
    assert!(!hints(true, (0, 0)).contains("space m m"));
    let found = hints(true, (2, 0));
    assert!(found.contains("]q") && !found.contains("space m m"), "{found}");
}

/// The test panel after a run with a long failure, the wheel partway down its message: the list, the
/// failure's window, the scroll thumb on the card's edge.
#[test]
fn snapshot_test_panel_scrolled() {
    use crate::test_results::{Case, Status};
    let mut s = Shot::new(100, 24);
    s.file("src/demo.rs", DEMO_RS);
    let at = Some((std::env::current_dir().unwrap().join("src/demo.rs"), 91));
    let mut message = vec![
        "assertion `left == right` failed: snapshot differs".to_string(),
        "  left: \"3 distinct words\"".into(),
        " right: \"8 distinct words\"".into(),
    ];
    message.extend((0..30).map(|i| format!("  - {i:02} the quick brown fox, row {i}")));
    let case = |name: &str, status, message: Vec<String>, at| Case {
        name: name.into(),
        status,
        secs: Some(0.02),
        message,
        at,
    };
    let cases = vec![
        case("tests::word_counts_splits", Status::Passed, Vec::new(), None),
        case("tests::word_counts_distinct", Status::Failed, message.clone(), at.clone()),
        case("tests::word_counts_empty", Status::Failed, vec!["panicked: empty".into()], at),
    ];
    let mut run = crate::testing::TestRun::finished("tests", cases, message);
    run.target.cwd = std::env::current_dir().unwrap();
    run.selected = 1;
    run.took = Some(Duration::from_millis(1240));
    s.ed.test_run = Some(run);
    s.frame();
    let (y0, _, _) = s.ed.screen.test_panel.get().expect("the panel is drawn");
    for _ in 0..4 {
        s.ed.handle_mouse(crate::event::Mouse {
            kind: crate::event::MouseKind::ScrollDown,
            x: 60,
            y: (y0 + 3) as u16,
            alt: false,
        });
    }
    s.ed.toasts.clear();
    s.check("test_panel_scrolled");
}

/// A run where no test ran: the pill says so, and the first row says why above the runner's own "ok".
#[test]
fn test_panel_says_when_no_test_ran() {
    let mut s = Shot::new(100, 24);
    s.file("src/demo.rs", DEMO_RS);
    let output = vec![
        "running 0 tests".to_string(),
        String::new(),
        "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 1 filtered out".into(),
    ];
    let mut run = crate::testing::TestRun::finished("orphan::forgotten", Vec::new(), output);
    run.state = crate::testing::RunState::NoTests;
    s.ed.test_run = Some(run);
    let mut p = vt100::Parser::new(s.h, s.w, 0);
    p.process(&s.frame());
    let text = p.screen().contents();
    let (y0, _, _) = s.ed.screen.test_panel.get().expect("the panel is drawn");
    let rows: Vec<&str> = text.lines().skip(y0).collect();
    assert!(rows[0].contains("NO TESTS") && rows[0].contains("orphan::forgotten"), "{text}");
    assert!(rows[1].contains("no test ran — nothing matched it"), "{text}");
    assert!(rows[2..].iter().any(|r| r.contains("test result: ok. 0 passed")), "the output under it: {text}");
}

/// The terminal panel under the code with a program's colored output, the keys in it.
#[test]
fn snapshot_terminal_panel() {
    let mut s = Shot::new(100, 24);
    s.file("src/demo.rs", DEMO_RS);
    let script = "printf '\\033[1;32m$\\033[0m cargo test\\r\\nrunning 3 tests\\r\\ntest words ... \\033[32mok\\033[0m\\r\\n\
                  test empty ... \\033[31mFAILED\\033[0m\\r\\n'; sleep 30";
    s.ed.terminal_run(vec!["/bin/sh".into(), "-c".into(), script.into()], Vec::new());
    // All of it — the line break after the last line too (it can come in a read of its own)
    s.until("output", |ed| {
        ed.terminal.as_ref().is_some_and(|t| {
            let screen = t.parser.screen();
            screen.contents().contains("FAILED") && screen.cursor_position() == (4, 0)
        })
    });
    s.check("terminal_panel");
    // A drag over its first line (screen row 15; its text starts a column in): selected on the selection's
    // color, copied on release
    let mouse = |s: &mut Shot, kind, x| {
        s.ed.handle_event(Event::Mouse(crate::event::Mouse { kind, x, y: 15, alt: false }));
        s.frame()
    };
    use crate::event::MouseKind as K;
    mouse(&mut s, K::Down, 1);
    mouse(&mut s, K::Drag, 12);
    let bytes = mouse(&mut s, K::Up, 12);
    assert_eq!(s.ed.terminal.as_ref().unwrap().selected_text(), "$ cargo test");
    assert!(s.ed.toasts.iter().any(|t| t.text == "copied 12 characters"));
    let shot = snapshot(&s.ed, "t", &bytes, s.w, s.h);
    let bg_row = shot.lines().skip_while(|l| !l.starts_with("## bg")).nth(16).unwrap();
    assert_eq!(&bg_row[3..17], "pVVVVVVVVVVVVp", "the selected cells: {bg_row:?}");
    // A drag begun in the code isn't the terminal's when it crosses it — nor is its cursor moved by it
    let at = |s: &mut Shot, kind, x, y| {
        s.ed.handle_event(Event::Mouse(crate::event::Mouse { kind, x, y, alt: false }));
        s.frame();
    };
    at(&mut s, K::Down, 10, 3);
    assert!(s.ed.terminal.as_ref().unwrap().selection.is_none() && !s.ed.terminal.as_ref().unwrap().focus);
    at(&mut s, K::Drag, 10, 16);
    at(&mut s, K::Up, 10, 16);
    assert!(s.ed.terminal.as_ref().unwrap().selection.is_none(), "not a terminal selection");
    // A drag begun in it goes on outside it (clamped to its edge) and ends there
    at(&mut s, K::Down, 1, 16);
    at(&mut s, K::Drag, 30, 3);
    assert_eq!(
        s.ed.terminal.as_ref().unwrap().selected(),
        Some(((0, 29), (1, 0))),
        "clamped to its first row"
    );
    at(&mut s, K::Up, 30, 3);
    assert!(!s.ed.terminal.as_ref().unwrap().selecting, "released outside — done");
    let cursor = s.ed.doc().selection().primary();
    at(&mut s, K::Drag, 40, 5);
    assert_eq!(
        s.ed.doc().selection().primary(),
        cursor,
        "a stray drag after it doesn't move the code's cursor"
    );
    // A release that never came: the next press anywhere ends that selection
    at(&mut s, K::Down, 1, 16);
    at(&mut s, K::Drag, 6, 16);
    at(&mut s, K::Down, 10, 3);
    assert!(!s.ed.terminal.as_ref().unwrap().selecting);
    s.ed.terminal = None; // hangs it up
}

/// A merge conflict: ours on the green wash, theirs on the changed one, the card with the keys at the
/// cursor.
#[test]
fn snapshot_conflict_card() {
    let mut s = Shot::new(100, 24);
    let text = DEMO_RS.replace(
        "    let text = \"the quick brown fox jumps over the lazy dog\";\n",
        "<<<<<<< HEAD\n    let text = \"the quick brown fox\";\n=======\n    let text = \"jumps over the lazy dog\";\n>>>>>>> feature\n",
    );
    s.file("src/demo.rs", &text);
    s.keys("]m");
    s.check("conflict_card");
}

/// `:diff` against another file: removed lines in place, changed and added ones on the green wash.
#[test]
fn snapshot_diff_view() {
    let mut s = Shot::new(100, 24);
    let dir = std::env::temp_dir().join(format!("tarae-snapshot-diff-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let old = dir.join("demo.old.rs");
    std::fs::write(&old, DEMO_RS).unwrap();
    let now = DEMO_RS
        .replace("    counts\n}", "    counts.retain(|_, n| *n > 1);\n    counts\n}")
        .replace("    println!(\"{} distinct words\", counts.len()); // 타래\n", "");
    s.file("src/demo.rs", &now);
    s.keys(&format!(":diff {}<ret>", old.display()));
    s.until("diff", |ed| ed.doc().diff_of.is_some());
    s.ed.toasts.clear();
    std::fs::remove_dir_all(&dir).ok();
    s.check("diff_view");
}

/// The list's places show their message at the line end (like a diagnostic, where none speaks already)
/// and move with the lines when you edit above them.
#[test]
fn list_messages_at_line_ends_follow_edits() {
    let mut s = Shot::new(100, 20);
    s.file("src/demo.rs", DEMO_RS);
    let path = s.ed.doc().path.clone().unwrap();
    let item = crate::quickfix::Item {
        path,
        line: 13,
        col: 0,
        message: "main never returns an error".into(),
        severity: 2,
        mark: None,
    };
    s.ed.qlist_set("check", vec![item]);
    let row_of = |s: &mut Shot| {
        let bytes = s.frame();
        let text = snapshot(&s.ed, "t", &bytes, s.w, s.h);
        text.lines().find(|l| l.contains("▲ main never returns")).map(|l| l[..2].to_string())
    };
    let before = row_of(&mut s).expect("the message at the line end");
    s.keys("ggOinserted<esc>");
    let after = row_of(&mut s).expect("still shown");
    assert_eq!(after.parse::<u32>().unwrap(), before.parse::<u32>().unwrap() + 1, "moved down with its line");
}

/// The completion's docs card left of its list (no room right): the cell between them is blank — not a
/// letter of the code under it.
#[test]
fn completion_docs_left_of_the_list_leave_no_letter_between() {
    let mut s = Shot::new(100, 24);
    let line = "x".repeat(96);
    let text = format!(
        "fn main() {{\n    let v = 1;\n    {} v.\n{}\n",
        "y".repeat(76),
        format!("{line}\n").repeat(12)
    );
    s.file("src/demo.rs", &text);
    s.keys("jjA");
    let items = json!([{ "label": "push", "kind": 2, "documentation": "Appends an element to the back." }]);
    let mut c = crate::completion::Completion::new(
        1,
        s.ed.doc().id,
        s.ed.doc().selection().primary().head,
        items.clone(),
    );
    c.filter("");
    c.selected = Some(0);
    let md = crate::completion::doc_markdown(&items[0], "rust");
    c.docs = Some((0, crate::markdown::render(&md, &s.ed.theme, Some("rust"))));
    s.ed.completion = Some(c);
    // Far right: the list goes right, the docs to its left
    s.ed.doc_mut().set_selection(crate::selection::Selection::point(text.find("v.").unwrap() + 2));
    let bytes = s.frame();
    let shot = snapshot(&s.ed, "t", &bytes, s.w, s.h);
    let rows: Vec<&str> = shot.lines().skip(2).take(24).map(|l| l.get(3..).unwrap_or("")).collect();
    let list = rows.iter().position(|r| r.contains("push")).expect("the list");
    let x = rows[list].find("push").unwrap();
    assert!(rows[list][..x].contains("Appends"), "docs on the left: {:?}", rows[list]);
    // No `x` of the code sits between the cards — on their half-block edge rows either
    for r in [rows[list - 1], rows[list], rows[list + 1]] {
        let edge = |c: char| c == '▄' || c == '▀';
        let (Some(a), Some(b)) = (r.find(edge), r.rfind(edge)) else { continue };
        assert!(!r[a..b].contains('x'), "{r:?}");
    }
}

/// The cursor line's blame shows at its end in normal mode — not while typing there.
#[test]
fn blame_hides_while_typing() {
    let mut s = Shot::new(100, 12);
    s.file("src/demo.rs", DEMO_RS);
    s.ed.config.git_blame = true;
    let blame = crate::gitmenu::Blame {
        author: "eth".into(),
        time: 0,
        summary: "wind the first thread".into(),
        uncommitted: false,
    };
    s.ed.blame.lines = vec![Some(blame); DEMO_RS.lines().count()];
    s.ed.blame.of = Some((s.ed.doc().id, s.ed.doc().version()));
    let shows = |s: &mut Shot| {
        let bytes = s.frame();
        snapshot(&s.ed, "t", &bytes, s.w, s.h).contains("wind the first thread")
    };
    assert!(shows(&mut s), "normal mode");
    s.keys("i");
    assert!(!shows(&mut s), "typing");
    s.keys("<esc>");
    assert!(shows(&mut s), "back");
}

/// After a turn, reading the answer: scroll the chat and the editor shows the first place it now shows.
#[test]
fn reading_the_answer_moves_the_editor() {
    use crate::chat::{Msg, Role};
    let mut s = Shot::new(100, 30);
    s.file("src/demo.rs", DEMO_RS);
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" l");
    let filler = "A line of the answer that says something.\n\n".repeat(30);
    let answer = format!(
        "The tree is built in src/filetree.rs:30.\n\n{filler}The side panel keeps it at src/sidebar.rs:60."
    );
    let c = s.ed.chat.as_mut().unwrap();
    c.msgs = vec![Msg::new(Role::User, "how?", ""), Msg::new(Role::Assistant, answer, "")];
    c.follow.on = true;
    s.frame();
    // As a turn leaves it: at the bottom (the last place shows), read from here on
    let c = s.ed.chat.as_mut().unwrap();
    c.follow.read_at = Some(c.scroll);
    let at = |s: &Shot| {
        (
            s.ed.doc().path.clone().unwrap(),
            s.ed.doc().text.byte_to_line(s.ed.doc().selection().primary().head),
        )
    };
    let cwd = std::env::current_dir().unwrap();
    s.ed.chat.as_mut().unwrap().scroll = 1000; // to the top
    s.frame();
    assert_eq!(at(&s), (cwd.join("src/filetree.rs"), 29), "the top of the answer");
    s.ed.chat.as_mut().unwrap().scroll = 0;
    s.frame();
    assert_eq!(at(&s), (cwd.join("src/sidebar.rs"), 59), "its end");
}

/// A name too long for the side panel keeps its end there; selected with the keys in the panel, it shows
/// whole — over the code, on its row.
#[test]
fn a_long_name_shows_whole_when_selected() {
    let mut s = Shot::new(112, 16);
    s.file("src/demo.rs", DEMO_RS);
    let long = "src/components/user_profile_settings_view_controller.tsx";
    let root = std::env::current_dir().unwrap();
    let mut sb = crate::sidebar::Sidebar::with_files(root, vec![long.to_string(), "src/demo.rs".into()]);
    sb.set_open(["src", "src/components"], true);
    sb.focus = false;
    s.ed.sidebar = Some(sb);
    let text = |s: &mut Shot| {
        let bytes = s.frame();
        snapshot(&s.ed, "t", &bytes, s.w, s.h)
    };
    let unfocused = text(&mut s);
    assert!(
        unfocused.contains("user_profile…") && unfocused.contains("ler.tsx"),
        "the end stays: {unfocused}"
    );
    let row = s.ed.sidebar.as_ref().unwrap().lines().iter().position(|l| l.label.ends_with(".tsx")).unwrap();
    let sb = s.ed.sidebar.as_mut().unwrap();
    sb.focus = true;
    sb.selected = row;
    assert!(text(&mut s).contains("user_profile_settings_view_controller.tsx"), "whole when selected");
}

/// What the terminal gets — only what changed (`screen.rs`) — replayed over what it got before, is the screen
/// each whole frame draws: syntax colors, cards over text, wide characters cut by them, the cursor.
#[test]
fn screen_diffs_replay_real_frames() {
    let mut s = Shot::new(100, 24);
    s.file("src/demo.rs", DEMO_RS);
    let mut frames = vec![s.frame()];
    let mut step = |s: &mut Shot, keys: &str| {
        s.keys(keys);
        frames.push(s.frame());
    };
    for keys in
        ["jjw", "iabc<esc>", " ", "<esc>", " ?", "open", "<esc>", ":se", "<esc>", "<C-d>", "<C-u>", "ddu"]
    {
        step(&mut s, keys);
    }
    // Wide characters, an emoji, a combining accent — and a card over them
    step(&mut s, "ggO가나다라 마바사 😀 e\u{301} 한글<esc>");
    for keys in ["yyp", "p", "gg", " ", "<esc>", "ll", "x", "u", " ?", "<esc>"] {
        step(&mut s, keys);
    }
    crate::screen::assert_replays(100, 24, frames);
}

/// `:redraw` asks the main loop for a whole frame.
#[test]
fn redraw_asks_for_a_whole_frame() {
    let mut s = Shot::new(40, 6);
    s.keys(":redraw<ret>");
    assert!(s.ed.redraw);
}

/// A long title (the folder's name) gives way to the query: what's typed shows, and the title is cut.
#[test]
fn picker_query_shows_past_a_long_title() {
    let mut s = Shot::new(100, 20);
    let root = fixtures().join("demo");
    let title = "files in a-folder-whose-name-is-as-long-as-a-worktree-ebpf-performance-7e75ab";
    s.ed.open_picker(
        crate::picker::Picker::new(title, Vec::new(), true).with_tree(root.clone(), None),
        Some(Box::new(move || Ok(crate::picker::file_items(&root)))),
    );
    s.until("file list", |ed| ed.picker.as_ref().is_some_and(|p| !p.loading));
    s.keys("main");
    let bytes = s.frame();
    let screen = snapshot(&s.ed, "t", &bytes, s.w, s.h);
    let input = screen.lines().find(|l| l.contains(" › ")).expect("input row").to_string();
    assert!(input.contains(" › main "), "the query shows: {input}");
    assert!(input.contains("files in a-folder") && input.contains('…'), "the title is cut: {input}");
    assert!(input.contains('/'), "the count stays: {input}");
}

/// A long line scrolled far right draws from the view's edge, not the line's head — the same frame, with its
/// colors (syntax), the cursor, and wide characters cut by the edge, as drawing it from the head.
#[test]
fn a_far_scrolled_line_draws_the_same_from_the_edge() {
    let mut s = Shot::new(80, 8);
    s.ed.config.soft_wrap = "never".into(); // (scrolled sideways, not wrapped)
    let line: String = (0..400).map(|i| format!("let x{i} = \"타래\"; ")).collect();
    s.file("src/long.rs", &format!("fn main() {{\n    {line}\n}}\n"));
    s.keys("jgl");
    let both = |s: &mut Shot| {
        let fast = s.frame();
        super::SKIP_SEEK.set(true);
        let slow = s.frame();
        super::SKIP_SEEK.set(false);
        (fast, slow)
    };
    for _ in 0..12 {
        let (fast, slow) = both(&mut s);
        assert!(fast == slow, "differs at {:?}", s.ed.doc().selection().primary());
        s.keys("hhhhhhb");
    }
}
