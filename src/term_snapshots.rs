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
//! first key in `BG`/`FG` wins. Text a region without the keys draws faded (`chrome::FADE`) gets its key's
//! mark too — the fade itself is the rules' tests' to check (`the_panels_follow_one_set_of_rules`).
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

/// Languages whose grammars are always compiled into a test build (`core = true` and `dev = true` in their
/// packs' `[grammar.<name>]`).
const CORE: &[&str] = &["rust", "toml", "markdown"];

/// Fixture root for scenarios that need real files (picker listing and preview).
fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/snapshots/fixtures")
}

/// The `demo` fixture copied to a temp folder of `test`'s own, at the same path below it — for a picker that
/// lists files (with git, which no test runs in tarae's own repository): (that folder — the editor's root, so
/// paths read as they do from this repository's — and the copy).
fn demo_copy(test: &str) -> (PathBuf, PathBuf) {
    fn copy(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for e in std::fs::read_dir(from).unwrap().flatten() {
            let (f, t) = (e.path(), to.join(e.file_name()));
            if f.is_dir() { copy(&f, &t) } else { std::fs::copy(&f, &t).map(|_| ()).unwrap() }
        }
    }
    let base = crate::scratch::Scratch::new(&format!("shot-{test}")).into_path();
    let demo = base.join("src/snapshots/fixtures/demo");
    copy(&fixtures().join("demo"), &demo);
    (base, demo)
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
    /// Where `file` puts its files: the working directory, or `project()` (`in_project`).
    root: PathBuf,
}

/// A project folder whose name is always `tarae` — for scenarios that show the project's name (the side
/// panel's files tab), which the checkout's folder can't give: worktrees and other clones are named
/// otherwise. Real and empty (a chat or a task can start in it); the files shown in it never touch disk.
fn project() -> PathBuf {
    let dir = std::env::temp_dir().join("tarae-snapshots").join("tarae");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::canonicalize(dir).unwrap()
}

impl Shot {
    fn new(w: u16, h: u16) -> Self {
        // crossterm drops every color under NO_COLOR — which tarae's own test runner sets (`test_start`)
        crossterm::style::force_color_output(true);
        let mut ed = Editor::new(Config::default());
        assert_eq!(ed.theme.name, "meok", "snapshots are drawn with the dark built-in theme");
        ed.config.lsp.enabled = false;
        let mut shot = Shot { ed, w, h, root: std::env::current_dir().unwrap() };
        shot.frame();
        shot
    }

    /// The session rooted at `project()` from here on (its files too) — returned, for the side panel.
    fn in_project(&mut self) -> PathBuf {
        self.root = project();
        self.ed.session.root = self.root.clone();
        self.root.clone()
    }

    /// Shows `text` as the file `rel` (relative to the shot's root, so the path bar reads `tarae › …`)
    /// without touching disk, and waits for its syntax tree (core grammars only).
    fn file(&mut self, rel: &str, text: &str) -> DocId {
        let path = self.root.join(rel);
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

    /// The frame as the terminal gets it, whole: the cards seated as it's read (`Screen::present`).
    fn frame(&mut self) -> Vec<u8> {
        let buf = self.drawn();
        let edges = self.ed.screen.edges.borrow();
        crate::screen::Screen::new().present(&buf, self.w, self.h, &edges).to_vec()
    }

    /// The frame as `render` draws it, before `Screen::present` reads it.
    fn drawn(&mut self) -> Vec<u8> {
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

    /// With `TARAE_FRAMES_DIR` set, the frame in both built-in themes there, `<name>-meok.bin` and
    /// `<name>-hanji.bin` — for a new element's screenshots (`cargo xtask shot` draws them:
    /// `TARAE_SHOT_REPLAY`). Nothing without it.
    fn frames_both(&mut self, name: &str) {
        let Some(dir) = std::env::var_os("TARAE_FRAMES_DIR") else { return };
        std::fs::create_dir_all(&dir).unwrap();
        for theme in ["meok", "hanji"] {
            self.ed.theme = crate::theme::load(theme).unwrap();
            std::fs::write(Path::new(&dir).join(format!("{name}-{theme}.bin")), self.frame()).unwrap();
        }
        self.ed.theme = crate::theme::load("meok").unwrap();
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
            // (a region without the keys draws its text faded toward its background — `chrome::FADE`: the
            // same key's mark)
            let faded_from = |k: &(vt100::Color, char, &str)| match (k.0, c.fgcolor(), c.bgcolor()) {
                (vt100::Color::Rgb(r, g, b), vt100::Color::Rgb(fr, fg, fb), bg) => {
                    let bg = match (bg, base_bg) {
                        (vt100::Color::Rgb(r, g, b), _)
                        | (vt100::Color::Default, vt100::Color::Rgb(r, g, b)) => [r, g, b],
                        _ => return false,
                    };
                    crate::chrome::fade([r, g, b], bg, crate::chrome::FADE) == [fr, fg, fb]
                }
                _ => false,
            };
            let key = fg_keys
                .iter()
                .find(|k| k.0 == c.fgcolor())
                .or_else(|| fg_keys.iter().find(|k| faded_from(k)));
            let fm = match key {
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
        s.ed.session.git_branch = Some("main".into());
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
    let (base, root) = demo_copy("file-picker");
    s.ed.session.root = base;
    s.ed.open_picker(
        crate::picker::Picker::new("files in demo", Vec::new(), true).with_tree(root.clone(), None),
        Some(Box::new(move || Ok(crate::picker::file_items(&root)))),
    );
    s.until("file list", |ed| ed.picker.as_ref().is_some_and(|p| !p.loading));
    s.keys("main");
    let ready = |ed: &Editor| {
        let Some(p) = ed.picker.as_ref() else { return false };
        let Some((path, _)) = p.current().and_then(|i| i.action.preview_target()) else { return false };
        matches!(p.previews.get(path), Some(crate::picker::Preview::Ready(_)))
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

/// A run of Claude's that looks stuck (#181): how long it's been quiet and idle, what its threads wait in,
/// `y Stop it` · `w What it waits on` · `n Keep waiting`.
#[test]
fn snapshot_offer_stuck() {
    let mut s = Shot::new(100, 30);
    s.ed.offer_popups = true;
    s.file("src/demo.rs", DEMO_RS);
    let stuck = crate::stuck::Stuck {
        slot: crate::sizes::Slot::Task,
        run: 1,
        pid: 4242,
        asked: std::time::Instant::now(),
        quiet: std::time::Duration::from_secs(12 * 60 + 4),
        cpu: 0.0,
        at: 0,
        reach: None,
    };
    s.ed.push_offer(crate::offer::Offer::new(
        "Stuck? Claude's command".into(),
        "No output for 12 min, 0% CPU — Claude's command cargo test --target aarch64-unknown-linux-gnu in the \
         task panel. Claude's turn waits on it, and only you can stop it.\n\nWhat each of its threads waits in \
         is in its output now (w), kept if you stop it:\n    pid 4242 · cargo test --target \
         aarch64-unknown-linux-gnu · 1 thread\n      cargo — sleeping in do_wait · 0.84 s CPU\n    pid 4250 · \
         qemu-aarch64 target/debug/deps/tarae-1f2e · 3 threads\n      3 × qemu-aarch64 — sleeping in \
         futex_wait_queue · 0.02 s CPU\n    …",
        "Stopping…",
        String::new(),
        crate::offer::What::Stuck(Box::new(stuck)),
    ));
    s.check("offer_stuck");
}

/// frictions15 #3: a quiet run connected to a daemon outside its tree — `Quiet:`, not `Stuck?`, and where its
/// work may be going on, made by `stuck::card` itself.
#[test]
fn snapshot_offer_quiet() {
    let mut s = Shot::new(100, 30);
    s.ed.offer_popups = true;
    s.file("src/demo.rs", DEMO_RS);
    let stuck = crate::stuck::Stuck {
        slot: crate::sizes::Slot::Task,
        run: 1,
        pid: 4242,
        asked: std::time::Instant::now(),
        quiet: std::time::Duration::from_secs(14 * 60),
        cpu: 0.0,
        at: 0,
        reach: Some(crate::sandbox::ReachOut::Sockets("~/.colima/default/docker.sock".into())),
    };
    let lines = [
        "pid 4242 · cargo xtask linux test · 1 thread".to_string(),
        "  cargo — waiting · 0.31 s CPU".to_string(),
        "pid 4250 · docker run --rm tarae-linux cargo test · 9 threads".to_string(),
        "  9 × docker — waiting · 0.12 s CPU".to_string(),
    ];
    let label = "Claude's command cargo xtask linux test in the task panel";
    let peers = ["~/.colima/default/docker.sock".to_string()];
    s.ed.push_offer(crate::stuck::card(stuck, label, &lines, &peers));
    s.check("offer_quiet");
    s.frames_both("offer_quiet");
}

/// A tall card being read in its buffer (frictions15 #1): the bar over the status line says whose keys they
/// are — the card's after `C-q` (`esc goes back`), the code's with a view key pending (`C-q answers it`) —
/// where a hidden hold made `C-q` let go instead.
#[test]
fn snapshot_card_reading_bar() {
    let mut s = Shot::new(100, 24);
    s.ed.offer_popups = true;
    let script: String = (1..=60).map(|i| format!("echo step {i}\n")).collect();
    let what = crate::offer::What::Install {
        tool: "script".into(),
        run: vec!["true".into()],
        cwd: std::env::temp_dir(),
        then: crate::install::Then::Lsp,
    };
    let q = format!("{script}\nRuns the script Claude wrote this turn.");
    s.ed.push_offer(crate::offer::Offer::new(
        "Claude wants to run a script".into(),
        &q,
        "",
        String::new(),
        what,
    ));
    s.frame();
    let o = (s.ed.offers[0].id, s.ed.offers[0].rev);
    s.ed.offer_seen.set(Some((o.0, o.1, Instant::now() - crate::offer::SETTLE * 2)));
    s.keys("<C-q>vjj");
    s.check("card_reading_bar");
    s.frames_both("card_reading_bar");
    s.keys("<esc>z");
    s.frames_both("card_reading_bar_code");
}

/// The card in front says how many more wait and where (frictions15 #5) — one stacked here, one in a session
/// behind — and the key that lists them.
#[test]
fn snapshot_card_says_who_waits_behind_it() {
    let mut s = Shot::new(100, 30);
    s.ed.offer_popups = true;
    s.ed.session.name = "main".into();
    s.file("src/demo.rs", DEMO_RS);
    let card = |n: u64| {
        let what = crate::offer::What::Screen { call: crate::tools::Call::test(n) };
        crate::offer::Offer::new(
            "Claude asks to see the screen".into(),
            "As text, once.",
            "",
            String::new(),
            what,
        )
    };
    s.ed.push_offer(card(1));
    s.ed.push_offer(card(2));
    let back = s.ed.session_add(std::env::temp_dir());
    if let Some(l) = s.ed.sessions.iter_mut().find(|l| l.id == back) {
        l.name = "fix-login".into();
        s.ed.offer_seq += 1;
        let mut o = card(3);
        o.id = s.ed.offer_seq;
        l.parked.push(o);
    }
    s.check("card_says_who_waits_behind");
    s.frames_both("card_says_who_waits_behind");
}

/// Piece C: the host card with the network closed — a pack's registry asked, its others suggested (the yes grants
/// them), the caches warning, `y` · `a` · `s` · `n` — made by `hosts::card` itself; at 100 columns and narrow.
#[test]
fn snapshot_host_card_closed() {
    for (w, name) in [(100, "host_closed"), (72, "host_closed_narrow")] {
        let mut s = Shot::new(w, 30);
        s.ed.offer_popups = true;
        s.file("src/demo.rs", DEMO_RS);
        let a = crate::hosts::Ask {
            session: s.ed.session.proxy_id(),
            run: 1,
            host: "index.crates.io".into(),
            port: 443,
            process: "cargo".into(),
            label: "cargo build".into(),
            users: false,
            left: false,
            open: false,
            hosts: Default::default(),
            also: vec!["crates.io".into(), "*.crates.io".into()],
        };
        s.ed.push_offer(crate::hosts::card(a));
        s.check(name);
        s.frames_both(name);
    }
}

/// Piece H: Claude's `commit` card — the message at full strength, the branch and the files with their counts (a
/// protected name and a key file in the warning colour), the diff's start in its colours, what stays staged, a
/// partly staged file said; `y Commit` · `v See the diff` · `n Don't` — made by `committool::card` itself, at 100
/// columns and narrow.
#[test]
fn snapshot_commit_card() {
    use crate::committool::{Ask, Commit, File};
    let file = |shown: &str, lines, status, warn| File { shown: shown.into(), lines, status, warn };
    let ask = Ask {
        root: PathBuf::from("/p"),
        top: PathBuf::from("/p"),
        parent: Some("1a2b3c4d5e6f".into()),
        branch: Some("main".into()),
        tree: "0".repeat(40),
        message: "Parse the lockfile once per build\n\nThe second read was the slow half of every check.\n".into(),
        paths: vec!["src/lock.rs".into(), "src/check.rs".into(), "Cargo.toml".into()],
        files: vec![
            file("Cargo.toml", Some((1, 1)), 'M', Some(crate::committool::PROTECTED.0)),
            file("src/check.rs", Some((3, 9)), 'M', None),
            file("src/lock.rs", Some((24, 0)), 'A', None),
        ],
        patch: "diff --git a/src/check.rs b/src/check.rs\n--- a/src/check.rs\n+++ b/src/check.rs\n@@ -12,9 +12,3 @@ pub fn check(root: &Path) -> Result<()> {\n-    let text = std::fs::read_to_string(root.join(\"Cargo.lock\"))?;\n-    let lock = parse(&text)?;\n+    let lock = crate::lock::once(root)?;\n     for p in lock.packages() {\n-        let again = parse(&std::fs::read_to_string(root.join(\"Cargo.lock\"))?)?;\n-        verify(p, &again)?;\n+        verify(p, lock)?;\n     }\n".into(),
        staged: vec!["README.md".into()],
        partial: vec!["src/check.rs".into()],
        unsaved: Vec::new(),
    };
    for (w, name) in [(100, "commit_card"), (64, "commit_card_narrow")] {
        let mut s = Shot::new(w, 32);
        s.ed.offer_popups = true;
        s.file("src/demo.rs", DEMO_RS);
        s.ed.push_offer(crate::committool::card(Commit {
            call: crate::tools::Call::test(1),
            ask: ask.clone(),
        }));
        s.check(name);
        s.frames_both(name);
    }
}

/// Piece F: every card whose answer words changed, for its screenshots — only with `TARAE_FRAMES_DIR` set (both
/// themes there; `cargo xtask shot` draws them with `TARAE_SHOT_REPLAY`). The host card closed and the grant card
/// (the whole ladder, `A Always here`) are `snapshot_host_card_closed`'s and `snapshot_sandbox_blocked`'s frames.
#[test]
fn frames_of_the_answer_words() {
    if std::env::var_os("TARAE_FRAMES_DIR").is_none() {
        return;
    }
    frames_of_the_repository_card();
    let shot = |name: &str, setup: &dyn Fn(&mut Shot)| {
        let mut s = Shot::new(100, 30);
        s.ed.offer_popups = true;
        s.file("src/demo.rs", DEMO_RS);
        setup(&mut s);
        assert!(!s.ed.offers.is_empty(), "{name}: a card");
        s.frames_both(name);
    };
    let run = |s: &mut Shot, command: &str| {
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "run", "arguments": {"command": command}}});
        assert!(crate::tools::mcp(&mut s.ed, 0, "c1", &msg).is_none(), "it waits for the user");
    };
    // A manifest tool's card: what it changes under the command, `y Once · a This chat`
    shot("f_run_manifest", &|s| {
        s.ed.config.llm.sandbox = false;
        run(s, "cargo add serde --features derive");
    });
    // No sandbox here, asked each time: `y Once · s This session · n No`
    shot("f_no_sandbox", &|s| {
        s.ed.sandbox = crate::sandbox::Availability::Missing("not on this system".into());
        s.ed.session.no_sandbox = Some(crate::tools::NoSandbox::Ask);
        run(s, "cargo build");
        s.until("its card", |ed| !ed.offers.is_empty());
    });
    // The network open: `y Once … A Always here`
    shot("f_host_open", &|s| {
        let a = crate::hosts::Ask {
            session: s.ed.session.proxy_id(),
            run: 1,
            host: "api.example.com".into(),
            port: 443,
            process: "curl".into(),
            label: "curl -sS https://api.example.com/v1/health".into(),
            users: false,
            left: false,
            open: true,
            hosts: Default::default(),
            also: Vec::new(),
        };
        s.ed.push_offer(crate::hosts::card(a));
    });
    // The three trust cards: `y Trust · n Hold back`
    shot("f_project_trust", &|s| {
        let q = ".tarae.toml sets what runs commands as you:\n\n  task.release = ./scripts/release.sh\n\nHeld back, \
                 the rest of the file still applies.";
        let what = crate::offer::What::ProjectTrust { hash: [0; 32] };
        let o = crate::offer::Offer::new(crate::trust::TITLE.into(), q, "", "Project settings".into(), what);
        s.ed.push_offer(o.deciding(2..3));
    });
    shot("f_git_trust", &|s| {
        let ask = crate::gittrust::Ask {
            config: "/p/.git/config".into(),
            place: "/p".into(),
            digest: [0; 32],
            items: vec![("core.fsmonitor".into(), ".git/fsmonitor.sh".into(), "on every status")],
            files: Vec::new(),
            filters: false,
        };
        let root = s.ed.session.root.clone();
        s.ed.push_offer(ask.card(&root));
    });
    // The binary the build left: `n Not now`
    shot("f_restart_same", &|s| {
        let q = "It was written before main (1085db8) was committed, and the build left it as it was — \
                 restarting would run the same tarae.\n\neditor.restart-build and editor.restart-binary say what's \
                 built and run.";
        let what = crate::offer::What::RestartSame { id: 1 };
        let o = crate::offer::Offer::new(
            "~/.cargo/bin/tarae is the binary it was".into(),
            q,
            "Restarting…",
            "Not restarting".into(),
            what,
        );
        s.ed.push_offer(o);
    });
    // A hook changed since the base: what a yes keeps (with the frames of piece I below)
    shot("f_hooks", &|s| {
        let ask = crate::hooks::Ask {
            what: "commit".into(),
            base: "main".into(),
            hooks: vec![crate::hooks::Hook {
                rel: ".githooks/pre-commit".into(),
                kind: "changed",
                likely: true,
                text: Some("#!/bin/sh\ncargo fmt --check\n".into()),
                hash: [0; 32],
            }],
            top: s.ed.session.root.clone(),
        };
        s.ed.push_offer(ask.card());
    });
}

/// Piece I: the repository's card as its parts come — tarae's alone, Claude's joining, git's joining (each frame
/// for jitter: it grows from where it stands), wide and narrow.
fn frames_of_the_repository_card() {
    for (w, wide) in [(100, ""), (64, "_narrow")] {
        let mut s = Shot::new(w, 30);
        s.ed.offer_popups = true;
        s.file("src/demo.rs", DEMO_RS);
        let root = s.ed.session.root.clone();
        let both = Box::new((s.ed.config.clone(), s.ed.keymaps.clone()));
        let items = vec![("task.release".to_string(), "./scripts/release.sh".to_string())];
        let project = crate::trust::Project {
            file: root.join(".tarae.toml"),
            hash: [1; 32],
            items,
            state: crate::trust::State::Ask,
            with: both.clone(),
            without: both,
        };
        s.ed.project_trust_set(Some(project));
        s.frames_both(&format!("i_1_tarae{wide}"));
        let q = "Claude started with these held back — y gives them to it, the conversation going on.\n\n  \
                 .claude/settings.json: apiKeyHelper = ./scripts/key.sh\n  .claude/settings.json: env \
                 ANTHROPIC_BASE_URL (its value not shown)";
        let what = crate::offer::What::ClaudeSettings { hash: [2; 32] };
        let o =
            crate::offer::Offer::new(crate::claudetrust::TITLE.into(), q, "", "Claude settings".into(), what);
        s.ed.trust_offer(o.deciding(2..4));
        s.frame();
        s.frames_both(&format!("i_2_claude{wide}"));
        let ask = crate::gittrust::Ask {
            config: root.join(".git/config"),
            place: root.clone(),
            digest: [3; 32],
            items: vec![("core.fsmonitor".into(), ".git/fsmonitor.sh".into(), "on every status")],
            files: Vec::new(),
            filters: false,
        };
        s.ed.trust_offer(ask.card(&root));
        s.frame();
        assert_eq!(s.ed.offers.len(), 1, "one card");
        s.frames_both(&format!("i_3_git{wide}"));
    }
}

/// A command Claude wants to run outside the sandbox — the run card P3 keeps — shown whole (a commit message past
/// four lines too), its reason under it, `y Run` · `n No`.
#[test]
fn snapshot_offer_run() {
    let mut s = Shot::new(100, 30);
    s.ed.offer_popups = true;
    s.file("src/demo.rs", DEMO_RS);
    let command = "git commit -m 'Count words: the words in the text, counted\n\nSplits on whitespace, so \
                   \"a  b\" is two words, and keeps the counts in a map from word to how many times it came, \
                   printed after the text.\n\nThe tests cover the empty text and repeated words.'";
    let reason = "git commit runs the hooks, which sign with the key in your agent";
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "run", "arguments": {"command": command, "outside_sandbox": true, "reason": reason}}});
    assert!(crate::tools::mcp(&mut s.ed, 0, "c1", &msg).is_none(), "it waits for the user");
    s.check("offer_run");
}

/// A command with a pattern for the rest of the chat (#27): the card names it, `a This chat` beside `y`/`n` — with
/// no sandbox, today's gate (in the sandbox it runs, P3; piece 5 takes the patterns out).
#[test]
fn snapshot_offer_run_pattern() {
    let mut s = Shot::new(100, 30);
    s.ed.config.llm.sandbox = false;
    s.ed.offer_popups = true;
    s.file("src/demo.rs", DEMO_RS);
    let command = "cargo xtask shot /tmp/words.png ' l' '@?distinct words'";
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "run", "arguments": {"command": command}}});
    assert!(crate::tools::mcp(&mut s.ed, 0, "c1", &msg).is_none(), "it waits for the user");
    s.check("offer_run_pattern");
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
    s.until("review", |ed| ed.session.review.as_ref().is_some_and(|r| r.ready));
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
    assert!(s.ed.session.chat.as_ref().is_some_and(|c| c.msgs.is_empty()), "chat opened without a note");
    s.check("chat_empty");
}

/// Chat panel after Claude explored the project in follow mode: a folded thought, tool rows
/// (grep·glob·read, as Claude's own tools are named) and an answer whose `path:line` references are underlined links.
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
        ran: None,
        cache: Default::default(),
    };
    let look = |verb, what: &str, pattern, range: &str, link: Option<Link>| {
        Some(Look { verb, what: what.into(), pattern, range: range.into(), link })
    };
    let lib = std::path::PathBuf::from("/p/src/lib.rs");
    s.ed.session.chat.as_mut().unwrap().follow.on = true;
    s.ed.session.chat.as_mut().unwrap().msgs = vec![
        msg(Role::User, "Who calls word_counts?", "demo.rs · L5", None),
        msg(
            Role::Thought,
            "**Finding the callers**\n\nIt's public, so other modules may call it — search before reading.",
            "",
            None,
        ),
        msg(Role::Tool, "", "", look("grep", "word_counts\\(", true, "in src", None)),
        msg(Role::Tool, "", "", look("glob", "src/**/*.rs", true, "", None)),
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
        ran: None,
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
    s.ed.session.chat.as_mut().unwrap().msgs =
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
    assert!(s.ed.session.chat.as_ref().unwrap().busy());
    s.frame();
    assert_eq!(s.ed.screen.chat_x, None, "minimized: no docked panel");
    assert!(s.ed.screen.chat_mini.get().is_some(), "a floating card instead");
    s.keys("<esc> l");
    assert_eq!(s.ed.screen.chat_x, docked, "space l brings the panel back for this turn");
    s.ed.session.chat.as_mut().unwrap().unfolded = false;
    s.ed.session.chat.as_mut().unwrap().follow.on = false;
    s.frame();
    assert_eq!(s.ed.screen.chat_x, docked, "not following = docked");
}

/// A command Claude wants to run outside the sandbox, asked while it works in follow mode: the minimized chat
/// moves to the top right, out of the card's way, the card's buttons in sight.
#[test]
fn snapshot_follow_offer_run() {
    let mut s = Shot::new(100, 30);
    s.file("src/demo.rs", DEMO_RS);
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" lcommit it<ret>");
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": {"name": "run", "arguments": {"command": "git commit -am 'Count words'",
            "outside_sandbox": true, "reason": "the hooks sign with the key in your agent"}}});
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
            crate::notes::By::Author,
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

/// A thread too long for the room keeps its newest words — under the name of whoever says them (`¶ author`),
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
    assert!(card[0].starts_with("¶ author"), "{card:#?}");
    assert!(card[1].starts_with("… Sentence number"), "{card:#?}");
    assert!(card.iter().any(|r| r.contains("Sentence number 12")), "the newest stays: {card:#?}");
    assert!(card.last().unwrap().contains("space n"), "and the keys: {card:#?}");
}

/// Who wrote each note, where it's seen: the reviewer's finding (`¶ reviewer · not sent yet` on its card, its
/// line end `reviewer · …`, `r1` in the rail beside your `¶1`), your draft, the author's answer.
#[test]
fn snapshot_review_notes_by_their_author() {
    use crate::notes::By;
    let (base, now) = five_and_thirty_added();
    let mut s = review_of(112, 30, &base, &now);
    let path = s.ed.doc().path.clone().unwrap();
    let id =
        crate::notes::add(&mut s.ed, &path, 11..12, By::Reviewer, "This value is never checked before use.");
    crate::notes::add(&mut s.ed, &path, 22..23, By::You, "Name this better.");
    for n in &mut s.ed.session.notes.list {
        (n.thread[0].draft, n.waiting) = (true, false);
    }
    let n = s.ed.session.notes.list.iter_mut().find(|n| n.id == id).unwrap();
    n.thread
        .push(crate::notes::Entry { by: By::Author, text: "It is — two lines up.".into(), draft: false });
    s.ed.goto_line(11);
    let mut parser = vt100::Parser::new(s.h, s.w, 0);
    parser.process(&s.frame());
    let rows: Vec<String> = parser.screen().rows(0, s.w).collect();
    let has = |t: &str| rows.iter().any(|r| r.contains(t));
    assert!(has("¶ reviewer · not sent yet") && has("¶ author"), "the card: {rows:#?}");
    assert!(has("¶ reviewer · This value") && has("¶ draft · Name"), "the line ends: {rows:#?}");
    assert!(has("¶1 r1"), "the rail: yours and the reviewer's: {rows:#?}");
    s.check("review_notes_by_author");
}

/// A high-risk change, graded by a pack's pattern: the rail says how many are left to look at and marks the
/// file `▲2` — taller than the pane, no definitions in it, it's two pieces (cardburden 2) — its first line says
/// which piece, why and the key; the reviewer's row its findings (one on the high change) and its verdict's
/// first line. `space v h` on each piece once all of it was on screen: looked at — faint everywhere.
#[test]
fn snapshot_review_risk_and_the_reviewer() {
    use crate::notes::By;
    let base: String = (1..=5).map(|i| format!("let line_{i} = {i};\n")).collect();
    let added: String = (1..=30)
        .map(|i| {
            if i == 7 {
                "let pid = unsafe { getpid() };\n".to_string()
            } else {
                format!("let added_{i} = {i};\n")
            }
        })
        .collect();
    let mut s = review_of(112, 30, &base, &format!("{base}{added}"));
    let path = s.ed.doc().path.clone().unwrap();
    let r = s.ed.session.code_review.as_mut().unwrap();
    r.set_rules(crate::risk::Rules::with(&[("rust", &["rust"], &[r"\bunsafe\b"], &[])], &[], &[]));
    let (of, root) = (r.started, r.root.clone());
    let id = crate::notes::add(&mut s.ed, &path, 11..12, By::Reviewer, "unsafe without a SAFETY comment.");
    s.ed.session.reviewer = Some(crate::reviewer::Reviewer::for_test(
        of,
        root,
        crate::reviewer::State::Done,
        vec![id],
        "One risk: the unsafe call has no SAFETY comment.\nThe rest reads fine.",
    ));
    s.ed.goto_line(3);
    let mut parser = vt100::Parser::new(s.h, s.w, 0);
    parser.process(&s.frame());
    let rows: Vec<String> = parser.screen().rows(0, s.w).collect();
    let has = |rows: &[String], t: &str| rows.iter().any(|r| r.contains(t));
    assert!(has(&rows, "▲ 2 high to look at"), "{rows:#?}");
    assert!(has(&rows, "demo.rs  ▲2 +30") || has(&rows, "▲2 +30"), "the file's mark: {rows:#?}");
    assert!(has(&rows, "reviewer  1 finding ▲1"), "{rows:#?}");
    assert!(has(&rows, "“One risk: the unsafe"), "{rows:#?}");
    assert!(has(&rows, "▲ high · piece 1/2 · unsafe (rust pack) · space v h"), "the line end: {rows:#?}");
    s.check("review_risk");
    // Its 30 added lines are two pieces: the key on one takes it only once all of it was on screen — the
    // first piece's foot is below the pane, at first
    s.ed.goto_line(6);
    crate::codereview::hunk_seen(&mut s.ed);
    assert_eq!(s.ed.session.looked.len(), 0, "not all of it was on screen");
    s.ed.goto_line(34);
    s.frame();
    s.ed.goto_line(6);
    s.frame();
    crate::codereview::hunk_seen(&mut s.ed);
    let left = |s: &Shot| s.ed.session.code_review.as_ref().map_or(0, |r| r.high_unseen(&s.ed).len());
    assert_eq!(left(&s), 1, "the first piece, all of it drawn: looked at");
    s.ed.goto_line(34);
    crate::codereview::hunk_seen(&mut s.ed);
    // (back at the first piece's mark)
    s.ed.goto_line(3);
    let mut parser = vt100::Parser::new(s.h, s.w, 0);
    parser.process(&s.frame());
    let rows: Vec<String> = parser.screen().rows(0, s.w).collect();
    assert!(has(&rows, "▲ all looked at"), "{rows:#?}");
    assert!(has(&rows, "▲ looked at · piece 1/2 · unsafe (rust pack)"), "{rows:#?}");
}

/// #213: a driver reads the reviewer as `space v r` shows it — the verdict whole (the state has its first line
/// only), each finding with its place, whether it's on a high-risk change, answered, and its thread — over the
/// socket (`{"review": …}`, after a frame like any snapshot). A session with no reviewer says so; another
/// session's is asked by its id.
#[test]
fn the_reviewers_verdict_and_findings_go_to_a_driver_whole() {
    use crate::notes::By;
    let base: String = (1..=5).map(|i| format!("let line_{i} = {i};\n")).collect();
    let added = "let pid = unsafe { getpid() };\nlet quiet = 1;\n";
    let mut s = review_of(112, 30, &base, &format!("{base}{added}"));
    let path = s.ed.doc().path.clone().unwrap();
    let r = s.ed.session.code_review.as_mut().unwrap();
    r.set_rules(crate::risk::Rules::with(&[("rust", &["rust"], &[r"\bunsafe\b"], &[])], &[], &[]));
    let (of, root) = (r.started, r.root.clone());
    let high = crate::notes::add(&mut s.ed, &path, 5..6, By::Reviewer, "unsafe without a SAFETY comment.");
    // (on a line the review didn't change: no grade)
    let low =
        crate::notes::add(&mut s.ed, &path, 1..2, By::Reviewer, "A name that says nothing.\nRename it.");
    let n = s.ed.session.notes.list.iter_mut().find(|n| n.id == low).unwrap();
    n.thread.push(crate::notes::Entry { by: By::You, text: "Kept on purpose.".into(), draft: false });
    let verdict = "One risk: the unsafe call has no SAFETY comment.\nThe rest reads fine.";
    s.ed.session.reviewer = Some(crate::reviewer::Reviewer::for_test(
        of,
        root,
        crate::reviewer::State::Done,
        vec![high, low],
        verdict,
    ));
    let other = s.ed.session_add(std::env::temp_dir().join("tarae-review-report-other"));

    let (tx, rx) = std::sync::mpsc::channel();
    let until = std::time::Instant::now() + Duration::from_secs(10);
    for json in [true, false] {
        let what = crate::uistate::Asked::Review(String::new());
        s.ed.ui_asks.push(crate::uistate::Ask { after_input: 0, json, reply: tx.clone(), until, what });
    }
    s.frame();
    let v: serde_json::Value =
        serde_json::from_str(&rx.try_recv().expect("answered with the frame")).unwrap();
    let r = &v["review"];
    assert_eq!(r["verdict"], verdict, "whole");
    assert_eq!(r["reviewer"]["state"], "done");
    let f = r["findings"].as_array().unwrap();
    assert_eq!(f.len(), 2);
    assert_eq!(
        (f[0]["path"].as_str(), &f[0]["line"], &f[0]["high"]),
        (Some("src/demo.rs"), &json!(6), &json!(true))
    );
    assert_eq!(f[0]["end"], json!(6), "one line");
    assert_eq!((&f[0]["answered"], &f[0]["thread"][0]["by"]), (&json!(false), &json!("reviewer")));
    assert_eq!((&f[1]["high"], &f[1]["answered"]), (&json!(false), &json!(true)));
    assert_eq!(f[1]["thread"][1], json!({ "by": "you", "text": "Kept on purpose." }));
    let text = rx.try_recv().expect("the text form too");
    assert!(text.contains("The rest reads fine."), "{text}");
    assert!(text.contains("- src/demo.rs:6 ▲ high\n  reviewer: unsafe without"), "{text}");
    assert!(
        text.contains(
            "- src/demo.rs:2 · answered\n  reviewer: A name that says nothing.\n    Rename it.\n  you: Kept"
        ),
        "{text}"
    );
    // #214, the read half: the high-risk changes no one has looked at, with their lines — read, never marked
    let u = r["high_unseen"].as_array().expect("its review is open");
    assert_eq!(u.len(), 1, "{u:?}");
    assert_eq!(
        (u[0]["path"].as_str(), &u[0]["line"], &u[0]["end"]),
        (Some("src/demo.rs"), &json!(6), &json!(7))
    );
    assert!(u[0]["why"].as_str().unwrap().starts_with("unsafe"), "{}", u[0]["why"]);
    assert_eq!(u[0]["text"], json!(["let pid = unsafe { getpid() };", "let quiet = 1;"]));
    assert!(
        text.contains(
            "1 high-risk change not looked at (a person's space v h marks them):\n- src/demo.rs:6–7 unsafe"
        ),
        "{text}"
    );
    assert!(text.contains("\n    let pid = unsafe { getpid() };"), "{text}");
    // (a person's key took it: none left)
    s.ed.goto_line(6);
    s.frame();
    crate::codereview::hunk_seen(&mut s.ed);
    let after = s.ed.review_report("").unwrap();
    assert_eq!(after.unseen.as_ref().map(Vec::len), Some(0));
    assert!(after.text().contains("no high-risk change left to look at"), "{}", after.text());
    let none = s.ed.review_report(&format!("s{other}")).unwrap();
    assert!(none.unseen.is_none(), "no review open there");
    assert!(none.reviewer.is_none() && none.text().contains("none yet"), "{}", none.text());
    assert!(s.ed.review_report("nope").is_err());
}

/// #195: while the reviewer reads, the rail says how long and how far — on its row where the rail is wide
/// enough for the longest it gets, else on the row under it (the rail's width never changes in a review, so
/// neither does where it is).
#[test]
fn the_rail_says_how_long_the_reviewer_has_read() {
    use crate::reviewer::{Reviewer, State};
    let (base, now) = five_and_thirty_added();
    let mut s = review_of(112, 30, &base, &now);
    let r = s.ed.session.code_review.as_ref().unwrap();
    let mut rv = Reviewer::for_test(r.started, r.root.clone(), State::Reading, Vec::new(), "");
    rv.started -= Duration::from_secs(80);
    rv.reads = 14;
    s.ed.session.reviewer = Some(rv);
    let mut parser = vt100::Parser::new(s.h, s.w, 0);
    parser.process(&s.frame());
    let rows: Vec<String> = parser.screen().rows(0, s.w).collect();
    let at =
        rows.iter().position(|r| r.contains("reviewer  reading…")).unwrap_or_else(|| panic!("{rows:#?}"));
    assert!(rows[at + 1].trim_start().starts_with("1m 20s · 14 reads"), "{rows:#?}");
    // Under a minute, no reads yet: seconds only
    let rv = s.ed.session.reviewer.as_mut().unwrap();
    (rv.started, rv.reads) = (Instant::now() - Duration::from_secs(7), 0);
    let mut parser = vt100::Parser::new(s.h, s.w, 0);
    parser.process(&s.frame());
    let rows: Vec<String> = parser.screen().rows(0, s.w).collect();
    assert!(rows[at + 1].trim_start().starts_with("7s "), "{rows:#?}");
    assert!(!rows[at + 1].contains("read"), "{rows:#?}");
    // A frame as each second turns while the rail shows it — none with the rail out of sight, or once done
    let next = s.ed.tick_wanted().expect("a frame for its time");
    assert!(next <= Duration::from_millis(1002), "{next:?}");
    s.ed.session.code_review.as_mut().unwrap().wide = true;
    s.frame();
    assert_eq!(s.ed.tick_wanted(), None, "the rail out of sight (space v z)");
    s.ed.session.code_review.as_mut().unwrap().wide = false;
    s.ed.session.reviewer.as_mut().unwrap().state = State::Done;
    s.frame();
    assert_eq!(s.ed.tick_wanted(), None, "done");
    // A rail wide enough: all on one row
    let mut s = review_of(160, 30, &base, &now);
    let r = s.ed.session.code_review.as_mut().unwrap();
    r.rail_need.take();
    r.rail_need.get_or_init(|| 44);
    let mut rv = Reviewer::for_test(r.started, r.root.clone(), State::Reading, Vec::new(), "");
    rv.started -= Duration::from_secs(80);
    rv.reads = 14;
    s.ed.session.reviewer = Some(rv);
    let mut parser = vt100::Parser::new(s.h, s.w, 0);
    parser.process(&s.frame());
    let rows: Vec<String> = parser.screen().rows(0, s.w).collect();
    assert!(rows.iter().any(|r| r.contains("reviewer  reading… 1m 20s · 14 reads")), "{rows:#?}");
}

/// Three sessions on a `w`×16 screen, the chat open in front: `main` (in front, its reviewer reading),
/// `fix-login` (its chat working, its reviewer reading), `docs` (its reviewer ended behind, not looked at).
/// Named here, not after the checkout's folder (the tests run in any checkout).
fn sessions_reviewing(w: u16) -> Shot {
    use crate::reviewer::{Reviewer, State};
    let mut s = Shot::new(w, 16);
    let root = s.in_project();
    let reviewer = |root: &std::path::Path, state| {
        Reviewer::for_test(Instant::now(), root.to_path_buf(), state, Vec::new(), "")
    };
    (s.ed.session.name, s.ed.session.git_branch) = ("main".into(), None);
    s.ed.session.reviewer = Some(reviewer(&root, State::Reading));
    let mut chat = crate::chat::Chat::new();
    chat.focused = false;
    s.ed.session.chat = Some(chat);
    // (behind, named so: their folders beside the project's)
    let behind = |ed: &mut Editor, name: &str| -> PathBuf {
        let dir = root.with_file_name(name);
        std::fs::create_dir_all(&dir).unwrap();
        let id = ed.session_add(dir.clone());
        ed.sessions.iter_mut().find(|l| l.id == id).unwrap().name = name.into();
        dir
    };
    let dir = behind(&mut s.ed, "fix-login");
    let l = s.ed.sessions.last_mut().unwrap();
    let mut chat = crate::chat::Chat::new();
    chat.state = crate::chat::State::Thinking;
    l.chat = Some(chat);
    l.reviewer = Some(reviewer(&dir, State::Reading));
    let dir = behind(&mut s.ed, "docs");
    let mut done = reviewer(&dir, State::Done);
    done.unseen = true;
    s.ed.sessions.last_mut().unwrap().reviewer = Some(done);
    s
}

/// frictions26 S1: a session's end card when what `session.keep` names is kept — the kept line decided, its own
/// CLAUDE.md copied beside it — and when nothing can be (over the size): said in the warning colour, with what
/// goes with the checkout.
#[test]
fn snapshot_session_end_keeps_its_notes() {
    use crate::sessionkeep::{Folder, Plan};
    let to = PathBuf::from("/private/tmp/tarae/target/sb/sessions/fix-login");
    let ending = |keep: Plan| crate::worktree::Ending {
        root: "/private/tmp/sessions/fix-login".into(),
        common: "/private/tmp/tarae/.git".into(),
        name: "fix-login".into(),
        branch: Some("fix-login".into()),
        tip: "a1b2c3d4e5f60718".into(),
        base: "main".into(),
        changed: Vec::new(),
        ahead: vec![
            ("a1b2c3d".into(), "Parse hours too".into()),
            ("9f8e7d6".into(), "Say the hours in the summary".into()),
        ],
        lost: Vec::new(),
        entries: vec!["target/sb".into()],
        keep,
    };
    let folder = Folder { rel: "target/sb".into(), files: 14, bytes: 220 << 10, to: to.clone() };
    let instructions_to = Some(to.join("instructions"));
    let differ = vec!["CLAUDE.md".to_string()];
    let main = Some(PathBuf::from("/private/tmp/tarae"));
    let kept = Plan {
        folders: vec![folder.clone()],
        differ: differ.clone(),
        instructions_to: instructions_to.clone(),
        not: None,
        main: main.clone(),
    };
    let mut s = Shot::new(100, 30);
    s.ed.offer_popups = true;
    s.file("src/demo.rs", DEMO_RS);
    s.ed.push_offer(ending(kept).card());
    s.check("session_end_kept");
    s.frames_both("session_end_kept");
    s.ed.offers.clear();
    let big = Folder { files: 2400, bytes: 310 << 20, ..folder };
    let not = Some("over 64 MB".to_string());
    s.ed.push_offer(ending(Plan { folders: vec![big], differ, instructions_to, not, main }).card());
    s.check("session_end_not_kept");
    s.frames_both("session_end_not_kept");
}

/// frictions26 S2: a session behind with two cards waiting — its tab counts them (`▲2 waits`) — and, minutes on,
/// the one warning that names it.
#[test]
fn snapshot_sessions_cards_waiting_behind() {
    let mut s = Shot::new(120, 30);
    let root = s.in_project();
    s.file("src/demo.rs", DEMO_RS);
    (s.ed.session.name, s.ed.session.git_branch) = ("main".into(), None);
    let mut chat = crate::chat::Chat::new();
    chat.focused = false;
    s.ed.session.chat = Some(chat);
    let dir = root.with_file_name("fix-login");
    std::fs::create_dir_all(&dir).unwrap();
    let id = s.ed.session_add(dir);
    let ago = crate::session::NUDGE_AFTER + Duration::from_secs(60);
    let card = |ed: &mut Editor, title: &str, what: crate::offer::What| {
        let mut o = crate::offer::Offer::new(title.into(), "…", "", String::new(), what);
        (ed.offer_seq, o.since) = (ed.offer_seq + 1, Instant::now() - ago);
        o.id = ed.offer_seq;
        let l = ed.sessions.iter_mut().find(|l| l.id == id).unwrap();
        (l.name, l.git_branch) = ("fix-login".into(), None);
        l.parked.push(o);
        ed.offer_seq
    };
    let first = card(
        &mut s.ed,
        "Claude changes 3 files",
        crate::offer::What::ChangeSet { generation: 999, ops: false },
    );
    card(
        &mut s.ed,
        "Claude asks to see the screen",
        crate::offer::What::Screen { call: crate::tools::Call::test(7) },
    );
    s.ed.card_nudge(id, first);
    // (frozen at full opacity: fully faded in, seconds before the fade-out)
    let fresh = Instant::now().checked_sub(Duration::from_millis(500)).unwrap();
    for t in &mut s.ed.toasts {
        (t.at, t.shown) = (fresh, true);
    }
    let mut parser = vt100::Parser::new(s.h, s.w, 0);
    parser.process(&s.frame());
    let rows: Vec<String> = parser.screen().rows(0, s.w).collect();
    assert!(rows.iter().any(|r| r.contains("main │ fix-login ▲2 waits │ +")), "{rows:#?}");
    s.check("sessions_cards_waiting_behind");
    s.frames_both("sessions_cards_waiting_behind");
}

/// frictions26 S4: the user stopped a turn whose command still runs — the light card offering to stop it, what it
/// is, where, for how long, and why it matters.
#[test]
fn snapshot_offer_left_running() {
    let mut s = Shot::new(100, 30);
    s.ed.offer_popups = true;
    s.file("src/demo.rs", DEMO_RS);
    // (half a second past 7:12 — the clock reads the same however long the frame takes)
    let started = Instant::now() - Duration::from_millis(432_500);
    let label = "grep -rn TODO src".to_string();
    crate::tools::left_running(
        &mut s.ed,
        vec![crate::tools::Left { slot: crate::queued::Slot::Task, started, label }],
    );
    s.check("offer_left_running");
    s.frames_both("offer_left_running");
}

/// frictions26 S4: the next turn's command waits in line behind the run a stopped turn left — the chat's live row
/// says behind what, whose, and how it stops; the task panel lists it waiting.
#[test]
fn snapshot_chat_run_queued_behind_a_stopped_turns() {
    use crate::chat::{Msg, Role};
    let mut s = Shot::new(120, 30);
    s.file("src/demo.rs", DEMO_RS);
    let cwd = std::env::temp_dir();
    let task = crate::task::Task {
        name: "grep -rn TODO src".into(),
        cmd: "grep -rn TODO src".into(),
        cwd: cwd.clone(),
    };
    let mut r = crate::task::Run::for_test(task);
    (r.state, r.by_claude) = (crate::task::State::Running, true);
    r.started = Instant::now() - Duration::from_millis(432_500);
    r.last_output = Instant::now();
    s.ed.session.task_run = Some(r);
    let mut chat = crate::chat::Chat::new();
    (chat.state, chat.unfolded, chat.focused) = (crate::chat::State::Thinking, true, true);
    chat.msgs.push(Msg::new(Role::User, "now run the tests", ""));
    let mut m = Msg::new(Role::Tool, "", "");
    m.look = Some(crate::chat::Look {
        verb: "run",
        what: "cargo test -p tarae".into(),
        pattern: true,
        range: String::new(),
        link: None,
    });
    chat.msgs.push(m);
    let generation = chat.generation();
    s.ed.session.chat = Some(chat);
    let call = crate::tools::Call::of(generation, "r1", json!(1));
    let job = crate::tools::Job::Command("cargo test -p tarae".into(), cwd);
    let who = crate::tools::For::Claude(call);
    crate::queued::push(&mut s.ed, crate::queued::What::Guarded { who, job, ride: Default::default() });
    let mut parser = vt100::Parser::new(s.h, s.w, 0);
    parser.process(&s.frame());
    let all = parser.screen().contents();
    assert!(all.contains("queued 1st, behind a stopped turn's run"), "{all}");
    // (the name cut to fit, the time and the key whole)
    assert!(all.contains("  grep -rn TODO ") && all.contains(" · 7:12  space m c stop it"), "{all}");
    s.check("chat_run_queued_behind_a_stopped_turns");
    s.frames_both("chat_run_queued_behind_a_stopped_turns");
}

/// The chat's session tabs have a row of air under them — the layout's, not the first message's: scrolled to the
/// top, the first message's chip is two rows under the tabs; at the bottom of a long chat the row under them is
/// still blank, the last message still in sight and the input where one session has it. One session: no tabs,
/// no gap — the header's rule is its edge, the chip right under it.
#[test]
fn the_chats_session_tabs_have_a_row_of_air_under_them() {
    use crate::chat::{Msg, Role};
    let mut s = Shot::new(120, 24);
    let mut chat = crate::chat::Chat::new();
    chat.focused = false;
    chat.msgs.push(Msg::new(Role::User, "Parse hours too.", "lib.rs"));
    for i in 0..30 {
        chat.msgs.push(Msg::new(Role::Assistant, format!("answer line {i}"), ""));
    }
    s.ed.session.chat = Some(chat);
    let rows = |s: &mut Shot, scroll: usize| {
        s.ed.session.chat.as_mut().unwrap().scroll = scroll;
        let mut p = vt100::Parser::new(s.h, s.w, 0);
        p.process(&s.frame());
        p.screen().rows(0, s.w).collect::<Vec<String>>()
    };
    let at = |rows: &[String], text: &str| rows.iter().position(|r| r.contains(text));
    // (the chat's part of a row: from the column `col` on)
    let col = |row: &str, text: &str| row[..row.find(text).unwrap()].chars().count();
    let from = |row: &str, col: usize| row.chars().skip(col).collect::<String>();
    let one = rows(&mut s, 1000);
    let title = at(&one, "claude").expect("the chat's title");
    assert_eq!(at(&one, "lib.rs"), Some(title + 2), "one session: the chip under the rule\n{one:#?}");
    let input = at(&one, "Ask about").expect("the input");
    s.ed.session_add(std::env::temp_dir().join("tarae-chat-air-other"));
    let top = rows(&mut s, 1000);
    let tabs = at(&top, " │ +").expect("the sessions' tabs");
    let x = col(&top[title], "claude");
    assert_eq!(at(&top, "lib.rs"), Some(tabs + 2), "the chip a row below the tabs\n{top:#?}");
    assert_eq!(from(&top[tabs + 1], x).trim(), "", "the row under the tabs\n{top:#?}");
    assert_eq!(at(&top, "Ask about"), Some(input), "the input stays\n{top:#?}");
    s.frames_both("chat_tabs_air_top");
    rows(&mut s, 8);
    s.frames_both("chat_tabs_air_scrolled");
    let bottom = rows(&mut s, 0);
    assert_eq!(from(&bottom[tabs + 1], x).trim(), "", "scrolled: the row under the tabs\n{bottom:#?}");
    let last = at(&bottom, "answer line 29").expect("the last message in sight");
    assert!(last < input, "{bottom:#?}");
    assert_eq!(at(&bottom, "Ask about"), Some(input), "{bottom:#?}");
    // An empty chat's start text taller than its room (16 rows): from its first line, two rows under the tabs —
    // what doesn't fit is its end (the suggestions, the model's key), never its heading
    let mut e = Shot::new(120, 16);
    let mut chat = crate::chat::Chat::new();
    chat.focused = false;
    e.ed.session.chat = Some(chat);
    e.ed.session_add(std::env::temp_dir().join("tarae-chat-air-other"));
    let empty = rows(&mut e, 0);
    let tabs = at(&empty, " │ +").expect("the sessions' tabs");
    assert_eq!(at(&empty, "Ask Claude"), Some(tabs + 2), "the start text's first line\n{empty:#?}");
    assert_eq!(at(&empty, "It reads the whole project,"), Some(tabs + 4), "{empty:#?}");
    assert!(at(&empty, "starting from").is_some_and(|r| r < at(&empty, "Ask about").unwrap()), "{empty:#?}");
}

/// #195: the reviewer reading, where the sessions are — its tab (`¶ reviewing`, beside the chat's `●`), its
/// row in the list (`space W`) — and one that ended behind, not looked at (`•`, `¶ reviewer done`): news,
/// not `▲ waiting` on the status line.
#[test]
fn snapshot_sessions_with_the_reviewer_reading() {
    let mut s = sessions_reviewing(160);
    let mut parser = vt100::Parser::new(s.h, s.w, 0);
    parser.process(&s.frame());
    let row = parser.screen().rows(0, s.w).find(|r| r.contains(" │ +")).unwrap_or_default();
    assert!(row.contains("main ¶ reviewing │ fix-login ● ¶ reviewing │ docs • │ +"), "{row}");
    let status = parser.screen().rows(0, s.w).last().unwrap_or_default();
    assert!(!status.contains("waiting"), "{status}");
    s.check("sessions_reviewing_tabs");
    s.ed.session_list();
    let mut parser = vt100::Parser::new(s.h, s.w, 0);
    parser.process(&s.frame());
    let rows: Vec<String> = parser.screen().rows(0, s.w).collect();
    // (the list's rows — not the tabs', beside it)
    let row = |name: &str| {
        rows.iter().find(|r| r.contains(name) && !r.contains(" │ ")).cloned().unwrap_or_default()
    };
    assert!(row("fix-login").contains("working") && row("fix-login").contains("¶ reviewing"), "{rows:#?}");
    assert!(row("docs").contains("¶ reviewer done"), "{rows:#?}");
    s.check("sessions_reviewing_list");
}

/// #195: where the tabs don't fit — the dropdown: the front's reviewer as its `¶` beside the name, counted in
/// the room the name leaves (a long name is cut, the `▸` stays whole), and `docs`' reviewer news in `•1`.
#[test]
fn snapshot_sessions_dropdown_with_the_reviewer_reading() {
    let mut s = sessions_reviewing(100);
    s.ed.session.name = "main-with-a-long-feature-name".into();
    let mut parser = vt100::Parser::new(s.h, s.w, 0);
    parser.process(&s.frame());
    let row = parser.screen().rows(0, s.w).find(|r| r.contains(" ▾ ")).unwrap_or_default();
    assert!(row.contains("◂ main-with-a-long") && row.contains("… ¶ ▾ 1/3 •1 ▸"), "{row}");
    s.check("sessions_reviewing_dropdown");
}

/// `space v q` with a high-risk change no one has looked at: the end card names it in the warning colour, and
/// it has no yes (`Offer.held`) — `h` goes to it (#196), `n` keeps reviewing.
#[test]
fn snapshot_review_end_held_by_a_high_change() {
    let base: String = (1..=5).map(|i| format!("let line_{i} = {i};\n")).collect();
    let now = format!("{base}let pid = unsafe {{ getpid() }};\n");
    let mut s = review_of(112, 30, &base, &now);
    let r = s.ed.session.code_review.as_mut().unwrap();
    r.set_rules(crate::risk::Rules::with(&[("rust", &["rust"], &[r"\bunsafe\b"], &[])], &[], &[]));
    crate::codereview::close_asked(&mut s.ed);
    let o = s.ed.offers.first().expect("the end card");
    assert_eq!(o.answers(false), [('h', "Go to them"), ('n', "Keep reviewing")]);
    assert!(o.question.contains("1 high-risk change not looked at"), "{}", o.question);
    s.check("review_end_held");
}

/// `now` under code review against `base` on a `w`×`h` screen, the rail of files up (as `snapshot_review`).
fn review_of(w: u16, h: u16, base: &str, now: &str) -> Shot {
    use crate::codereview::{Base, CodeReview, File};
    let mut s = Shot::new(w, h);
    // (the session's label is the checkout folder's name — `tarae`, as `project()`'s, wherever the tests run)
    (s.ed.session.name, s.ed.session.git_branch) = ("tarae".into(), None);
    s.file("src/demo.rs", now);
    let cwd = std::env::current_dir().unwrap();
    let file = File {
        rel: "src/demo.rs".into(),
        path: cwd.join("src/demo.rs"),
        added: now.lines().count() - base.lines().count(),
        removed: 0,
        new: false,
        gone: false,
        binary: false,
    };
    s.ed.session.code_review = Some(CodeReview::for_test(Base::Head, cwd, vec![file]));
    let doc = s.ed.doc_mut();
    doc.review_base = Some(Rope::from_str(base));
    doc.git_base = Some(std::sync::Arc::new(base.to_string()));
    doc.git_hunks = crate::git::hunks(base, now);
    doc.git_version = doc.version();
    s.frame();
    s
}

/// Five lines, then thirty added — under review, the added ones on the wash.
fn five_and_thirty_added() -> (String, String) {
    let base: String = (1..=5).map(|i| format!("let line_{i} = {i};\n")).collect();
    let added: String = (1..=30).map(|i| format!("let added_{i} = {i};\n")).collect();
    let now = format!("{base}{added}");
    (base, now)
}

/// A card's top and bottom rows (`rect`, as `editor.screen` has it) are its half-block edges, and each
/// edge cell's outer half has the background `without` (the same screen with no card) shows in that cell —
/// what's under the card. Those backgrounds.
fn edges_seated(
    with: &vt100::Parser,
    without: &vt100::Parser,
    (x, y, w, h): (usize, usize, usize, usize),
    surface: vt100::Color,
) -> Vec<vt100::Color> {
    let mut under = Vec::new();
    for row in [y, y + h - 1] {
        for col in x..x + w {
            let c = with.screen().cell(row as u16, col as u16).unwrap();
            assert!(
                ["▄", "▀"].contains(&c.contents()) && c.fgcolor() == surface,
                "{row},{col}: {:?} isn't the card's edge",
                c.contents()
            );
            let was = without.screen().cell(row as u16, col as u16).unwrap().bgcolor();
            assert_eq!(c.bgcolor(), was, "{row},{col}: the edge's outer half on what's under it");
            under.push(was);
        }
    }
    under
}

/// A card's half-row edges sit on what's under them — no strip of ground between the card and a review's
/// wash, nor the code.
#[test]
fn card_edges_sit_on_what_is_under_them() {
    // Over a review's wash: a note on an added line, its card on the added lines below
    let (base, now) = five_and_thirty_added();
    let mut s = review_of(112, 30, &base, &now);
    let (ground, surface, _) = chrome_colors(&s.ed);
    let path = s.ed.doc().path.clone().unwrap();
    crate::notes::add(&mut s.ed, &path, 11..12, crate::notes::By::Author, "A remark about this added line.");
    s.ed.goto_line(11);
    let with = cells(&s.frame(), s.w, s.h);
    let rect = s.ed.screen.note_card.get().expect("the note card");
    s.ed.session.notes.list.clear();
    let without = cells(&s.frame(), s.w, s.h);
    let under = edges_seated(&with, &without, rect, surface);
    // (the pane's padding, left of the wash, is ground — per column)
    let washed = under.iter().filter(|&&c| c != ground).count();
    assert!(washed > under.len() / 2, "the wash is under it: {under:?}");
    // Over the code: hover docs
    let mut s = Shot::new(100, 24);
    s.file("src/demo.rs", DEMO_RS);
    s.ed.goto_line(5);
    s.ed.popup = Some(crate::markdown::render("Counts how often each word appears.", &s.ed.theme, None));
    let with = cells(&s.frame(), s.w, s.h);
    let rect = s.ed.screen.popup.get().expect("the hover card");
    s.ed.popup = None;
    let without = cells(&s.frame(), s.w, s.h);
    edges_seated(&with, &without, rect, surface);
}

/// Seated through the main loop's diffs too: its `Screen`, which showed the frames before, sends what makes
/// the screen a whole seated frame makes — as a card comes, moves over the wash, and goes.
#[test]
fn card_edges_stay_seated_through_diffs() {
    let (base, now) = five_and_thirty_added();
    let mut s = review_of(112, 30, &base, &now);
    let path = s.ed.doc().path.clone().unwrap();
    for line in [11, 20] {
        crate::notes::add(&mut s.ed, &path, line..line + 1, crate::notes::By::Author, "A remark about this.");
    }
    let mut screen = crate::screen::Screen::new();
    let mut through_diffs = vt100::Parser::new(s.h, s.w, 0);
    let mut cards = Vec::new();
    for line in [3, 11, 20, 11, 3] {
        s.ed.goto_line(line);
        let drawn = s.drawn();
        through_diffs.process(screen.present(&drawn, s.w, s.h, &s.ed.screen.edges.borrow()));
        cards.push(s.ed.screen.note_card.get().map(|r| r.1));
        let whole = cells(&s.frame(), s.w, s.h);
        for (y, x) in (0..s.h).flat_map(|y| (0..s.w).map(move |x| (y, x))) {
            let (a, b) = (through_diffs.screen().cell(y, x).unwrap(), whole.screen().cell(y, x).unwrap());
            assert_eq!(
                (a.contents(), a.fgcolor(), a.bgcolor()),
                (b.contents(), b.fgcolor(), b.bgcolor()),
                "line {line}, cell {y},{x}"
            );
        }
    }
    // (by its top row)
    let [none, a, b, a2, none2] = cards[..] else { unreachable!() };
    assert!(
        none.is_none() && none2.is_none() && a.is_some() && a == a2 && b > a,
        "came, moved, went: {cards:?}"
    );
}

/// A note's card stays inside the code of the pane it's about — never over the review's rail of files
/// (it once cut their names), nor into the pane beside.
#[test]
fn a_note_card_stays_in_its_pane() {
    let long = "A long remark that runs on well past the width of any card, so the card is as wide as \
                cards get and has to be kept inside the pane rather than wherever it would fit.";
    let inside = |s: &mut Shot, what: &str| {
        let path = s.ed.doc().path.clone().unwrap();
        crate::notes::add(&mut s.ed, &path, 11..12, crate::notes::By::Author, long);
        s.ed.goto_line(11);
        s.frame();
        let (x, _, w, _) = s.ed.screen.note_card.get().unwrap_or_else(|| panic!("{what}: the note card"));
        let focused = s.ed.session.focus;
        let pane = s.ed.screen.panes.iter().find(|p| p.0 == focused).expect("the focused pane").1;
        assert!(
            pane.x <= x && x + w <= pane.x + pane.w,
            "{what}: the card {x}..{} out of its pane {}..{}",
            x + w,
            pane.x,
            pane.x + pane.w
        );
    };
    // Beside the review's rail
    let (base, now) = five_and_thirty_added();
    let mut s = review_of(100, 30, &base, &now);
    assert!(s.ed.screen.panes[0].1.x > 0, "the rail is up");
    inside(&mut s, "review");
    // The left of two panes side by side
    let mut s = Shot::new(140, 30);
    s.file("src/demo.rs", &now);
    s.keys("<C-w>v<C-w>h");
    assert_eq!(s.ed.screen.panes.len(), 2, "two panes");
    inside(&mut s, "split");
}

/// A card over a panel — the prompt's card standing on the status line over the task panel in the bottom
/// slot, both `ui.popup`-coloured — is a tone up from it, so it stands apart from the panel as it does from
/// the ground, in the light theme too.
#[test]
fn a_card_over_a_panel_stands_apart_from_it() {
    for light in [false, true] {
        let mut s = Shot::new(112, 24);
        if light {
            s.ed.theme = crate::theme::load("hanji").unwrap();
        }
        s.file("src/demo.rs", DEMO_RS);
        let cwd = std::env::current_dir().unwrap();
        s.ed.task_start(crate::task::Task { name: "check".into(), cmd: "echo done".into(), cwd });
        s.until("task", |ed| ed.session.task_run.as_ref().is_some_and(|r| r.took.is_some()));
        s.ed.toasts.clear();
        s.keys(":w");
        let (ground, surface, _) = chrome_colors(&s.ed);
        let p = cells(&s.frame(), s.w, s.h);
        let rows: Vec<String> = p.screen().rows(0, s.w).collect();
        let y = rows.iter().rposition(|r| r.contains(":w")).expect("the prompt");
        let slot = s.ed.screen.text_top + s.ed.screen.body_h - s.ed.screen.bottom_h;
        assert!(s.ed.screen.bottom_h > 0 && y >= slot, "the task panel is up, the prompt over it");
        let bg = |x: usize| p.screen().cell(y as u16, x as u16).unwrap().bgcolor();
        let x = s.ed.screen.prompt_card.get().expect("the prompt's card").0 + 1;
        assert!(bg(x) != surface && bg(x) != ground, "light {light}: the card over the panel {:?}", bg(x));
        // (beside the card, the panel's own surface is still there)
        assert_eq!(bg(s.w as usize - 3), surface, "light {light}: the panel");
    }
}

/// #18: a message of yours with line breaks is drawn line by line, blank lines kept — one span with its
/// breaks in it reached the terminal as raw LFs (rows overlapped, paragraphs lost their starts).
#[test]
fn a_users_message_keeps_its_lines() {
    use crate::chat::{Msg, Role};
    let mut s = Shot::new(112, 30);
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" l");
    let c = s.ed.session.chat.as_mut().unwrap();
    c.msgs.push(Msg::new(Role::User, "First paragraph here.\n\nSecond paragraph\r\nthird line", ""));
    let mut parser = vt100::Parser::new(s.h, s.w, 0);
    parser.process(&s.frame());
    let rows: Vec<String> = parser.screen().rows(0, s.w).collect();
    let at = rows.iter().position(|r| r.contains("▎ First paragraph here.")).expect("its first line");
    let next: Vec<&str> = rows[at + 1..at + 4].iter().map(|r| r.trim()).collect();
    assert!(next[0].ends_with('▎'), "the blank line, kept: {rows:#?}");
    assert!(next[1].ends_with("▎ Second paragraph"), "{rows:#?}");
    assert!(next[2].ends_with("▎ third line"), "{rows:#?}");
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
    let c = s.ed.session.chat.as_mut().unwrap();
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
        let c = s.ed.session.chat.as_mut().unwrap();
        c.follow.on = true;
        c.msgs.push(Msg::new(Role::Thought, "Reading where it starts.", ""));
        c.follow.gaze = Some(crate::follow::Gaze::new(look, Some(path.clone()), Some(lines)));
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
            crate::notes::add(&mut s.ed, &path, 39..40, crate::notes::By::Author, long);
        }
        s.ed.goto_line(39);
        let c = s.ed.session.chat.as_mut().unwrap();
        c.msgs.push(Msg {
            role: Role::Thought,
            text: "Thinking hard about it for a while.\n".repeat(6),
            chip: String::new(),
            look: None,
            call: String::new(),
            attached: None,
            ran: None,
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
            ran: None,
            cache: Default::default(),
        });
        c.follow.gaze = Some(crate::follow::Gaze::new(look, Some(path.clone()), Some(39..60)));
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
    s.ed.session.code_review = Some(CodeReview::for_test(Base::Head, cwd.clone(), files));
    let doc = s.ed.doc_mut();
    doc.review_base = Some(Rope::from_str(DEMO_RS));
    doc.git_base = Some(std::sync::Arc::new(DEMO_RS.to_string()));
    doc.git_hunks = crate::git::hunks(DEMO_RS, &now);
    doc.git_version = doc.version();
    let path = doc.path.clone().unwrap();
    s.keys(":9<ret> vm");
    crate::notes::add(&mut s.ed, &path, 8..9, crate::notes::By::You, "Why split this into two lines?");
    s.ed.session.notes.list[0].thread[0].draft = true;
    s.ed.session.notes.list[0].waiting = false;
    s.keys("gg");
    s.check("review");
}

/// #134: `space n` in a review file — from the code, from the rail, read wide — opens the comment's input,
/// said as one (`comment on line N:`, where outside a review it's `ask claude:`), and enter keeps it as a
/// draft on that line.
#[test]
fn space_n_in_a_review_opens_a_comment() {
    use crate::codereview::{Base, CodeReview, File};
    let screen = |s: &mut Shot| {
        let mut vt = vt100::Parser::new(s.h, s.w, 0);
        vt.process(&s.frame());
        vt.screen().contents()
    };
    for keys in ["<space>n", "<space>e<space>n", "<space>vz<space>n"] {
        let mut s = Shot::new(112, 30);
        s.file("src/demo.rs", DEMO_RS);
        let cwd = std::env::current_dir().unwrap();
        let files = vec![File {
            rel: "src/demo.rs".into(),
            path: cwd.join("src/demo.rs"),
            added: 1,
            removed: 0,
            new: false,
            gone: false,
            binary: false,
        }];
        s.ed.session.code_review = Some(CodeReview::for_test(Base::Head, cwd.clone(), files));
        s.keys(":6<ret>");
        s.keys(keys);
        let text = screen(&mut s);
        assert!(text.contains("comment on line 6: "), "{keys}\n{text}");
        s.keys("why?<ret>");
        let n = s.ed.session.notes.list.last().expect("a note");
        assert!(n.thread[0].draft && n.lines == (5..6), "{keys}: a draft on its line");
    }
    // Outside a review: a question to Claude, as before
    let mut s = Shot::new(112, 30);
    s.file("src/demo.rs", DEMO_RS);
    s.keys("<space>n");
    assert!(screen(&mut s).contains("ask claude: "));
}

/// #135: the rail's `m` marks the file on its row — a deleted binary file too, whose buffer shows no lines of
/// code (it marked the file the code showed, or nothing: the review never reached N of N).
#[test]
fn the_rail_marks_a_deleted_binary_file_viewed() {
    use crate::codereview::{Base, CodeReview, File};
    let mut s = Shot::new(112, 30);
    // (a folder of its own: the binary file's buffer asks git about it — never tarae's own repository)
    let cwd = s.in_project();
    s.file("src/demo.rs", DEMO_RS);
    let files = vec![
        File {
            rel: "assets/logo.png".into(),
            path: cwd.join("assets/logo.png"),
            added: 0,
            removed: 0,
            new: false,
            gone: true,
            binary: true,
        },
        File {
            rel: "src/demo.rs".into(),
            path: cwd.join("src/demo.rs"),
            added: 0,
            removed: 0,
            new: false,
            gone: false,
            binary: false,
        },
    ];
    s.ed.session.code_review = Some(CodeReview::for_test(Base::Head, cwd.clone(), files));
    let doc = s.ed.doc_mut();
    doc.review_base = Some(Rope::from_str(DEMO_RS));
    doc.git_base = Some(std::sync::Arc::new(DEMO_RS.to_string()));
    s.keys("<space>e");
    let viewed = |s: &Shot| {
        let r = s.ed.session.code_review.as_ref().unwrap();
        r.files.iter().filter(|f| r.viewed(&s.ed, f)).map(|f| f.rel.clone()).collect::<Vec<_>>()
    };
    let order = s.ed.session.code_review.as_ref().unwrap().rail_order();
    let png = order.iter().position(|&i| i == 0).unwrap();
    s.keys("gg");
    for _ in 0..png {
        s.keys("j");
    }
    assert_eq!(s.ed.session.code_review.as_ref().unwrap().selected, 0, "on the binary file's row");
    s.keys("m");
    assert_eq!(viewed(&s), ["assets/logo.png"], "{:?}", s.ed.messages.last());
    s.keys("gg");
    for _ in 0..order.iter().position(|&i| i == 1).unwrap() {
        s.keys("j");
    }
    s.keys("m");
    s.until("demo.rs's mark", |ed| {
        let r = ed.session.code_review.as_ref().unwrap();
        r.files.iter().all(|f| r.viewed(ed, f))
    });
    let mut vt = vt100::Parser::new(30, 112, 0);
    vt.process(&s.frame());
    assert!(vt.screen().contents().contains("2 of 2 viewed"), "{}", vt.screen().contents());
}

/// #145: a turn's review lists Claude's temp files outside the checkout beside `src/` — the rail says they
/// aren't in it (its head counts them, each row ends `outside` instead of `new`): they won't be committed.
#[test]
fn the_rail_says_which_files_are_outside_the_checkout() {
    use crate::codereview::{Base, CodeReview, File};
    let mut s = Shot::new(112, 30);
    s.file("src/demo.rs", DEMO_RS);
    let cwd = std::env::current_dir().unwrap();
    let file = |path: PathBuf, rel: String| File {
        rel,
        path,
        added: 2,
        removed: 0,
        new: true,
        gone: false,
        binary: false,
    };
    let outside = PathBuf::from("/tmp/x/setup.sh");
    let files = vec![
        file(cwd.join("src/demo.rs"), "src/demo.rs".into()),
        file(outside.clone(), outside.display().to_string()),
    ];
    let base = Base::Turn { at: 0, question: "set it up".into() };
    s.ed.session.code_review = Some(CodeReview::for_test(base, cwd.clone(), files));
    let mut vt = vt100::Parser::new(30, 112, 0);
    vt.process(&s.frame());
    let rows: Vec<String> = vt.screen().rows(0, 112).collect();
    let all = rows.join("\n");
    assert!(all.contains("1 outside, not committed "), "the head says it, whole\n{all}");
    let row =
        |name: &str| rows.iter().find(|r| r.contains(&format!("◦ {name}"))).cloned().unwrap_or_default();
    assert!(row("setup.sh").contains(" +2 outside"), "{}\n{all}", row("setup.sh"));
    assert!(row("demo.rs").contains(" +2 new") && !row("demo.rs").contains("outside"), "{all}");
}

/// Many removed lines above the cursor: the view scrolls by screen rows, so the cursor stays on screen.
#[test]
fn review_keeps_the_cursor_on_screen_below_a_big_deletion() {
    use crate::codereview::{Base, CodeReview};
    let mut s = Shot::new(100, 20);
    let base: String = (0..60).map(|i| format!("line {i}\n")).collect();
    let now: String = (0..60).filter(|i| !(10..50).contains(i)).map(|i| format!("line {i}\n")).collect();
    s.file("src/demo.rs", &now);
    s.ed.session.code_review =
        Some(CodeReview::for_test(Base::Head, std::env::current_dir().unwrap(), Vec::new()));
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

/// Two doc comments; the second one's first line changes and a line joins it (`DOCS_NOW`).
const DOCS_BASE: &str = r#"/// Counts the words in `text`.
///
/// Splits on **whitespace**, so `a  b` is two words.
pub fn count(text: &str) -> usize {
    text.split_whitespace().count()
}

/// The longest word.
pub fn longest(text: &str) -> Option<&str> {
    text.split_whitespace().max_by_key(|w| w.len())
}

fn main() {
    let text = "to be or not to be";
    println!("{} words, the longest {:?}", count(text), longest(text));
}
"#;

fn docs_now() -> String {
    DOCS_BASE.replace(
        "/// The longest word.\n",
        "/// The longest word, if there is one.\n/// Ties go to the first of them.\n",
    )
}

/// The screen as text, rows only (for asserting on what's drawn).
fn screen_text(s: &mut Shot) -> String {
    let bytes = s.frame();
    let mut parser = vt100::Parser::new(s.h, s.w, 0);
    parser.process(&bytes);
    parser.screen().contents()
}

/// Under a code review an unchanged doc comment renders as it does outside one; one holding a change stays
/// raw, line for line, its removed row above the line that replaced it. A click on the rendered block lands on
/// its first line, `j` steps over it, `]g` lands in the change.
#[test]
fn snapshot_review_doc_comments() {
    let mut s = review_of(100, 24, DOCS_BASE, &docs_now());
    s.ed.goto_line(13);
    s.frame();
    let shown = screen_text(&mut s);
    assert!(!shown.contains("/// Counts") && shown.contains("Counts the words in text."), "{shown}");
    assert!(shown.contains("/// The longest word, if there is one."), "the changed block stays raw: {shown}");
    assert!(shown.contains("/// The longest word."), "its removed row: {shown}");
    let folded = |s: &Shot| {
        s.ed.view.iter().filter(|r| matches!(r, crate::codeview::Row::Doc { start: 0, .. })).count()
    };
    assert!(folded(&s) > 0, "{:?}", s.ed.view);
    assert!(
        s.ed.view.iter().any(|r| matches!(r, crate::codeview::Row::Other { at: 7, .. })),
        "both kinds of rows in one view: {:?}",
        s.ed.view
    );
    s.check("review_doc_comments");
    s.frames_both("review_doc_comments");
    // A click on the rendered block's last row: its first line
    let (_, rect, gutter, rows) = s.ed.screen.panes[0].clone();
    let row = rows.iter().rposition(|r| r.0 == 0).expect("the block's rows");
    assert_eq!(rows[row + 1].0, 3, "the line after the block right below it: {rows:?}");
    let click = |s: &mut Shot, kind, y: usize| {
        let x = rect.x + gutter + 4;
        s.ed.handle_event(Event::Mouse(crate::event::Mouse { kind, x: x as u16, y: y as u16, alt: false }));
        s.frame();
    };
    click(&mut s, crate::event::MouseKind::Down, rect.y + row);
    click(&mut s, crate::event::MouseKind::Up, rect.y + row);
    assert_eq!(s.ed.cursor_line(), 0, "on the block");
    // `j` past it, `k` back onto it
    s.keys("j");
    assert_eq!(s.ed.cursor_line(), 3, "stepped over the block");
    s.keys("k");
    assert_eq!(s.ed.cursor_line(), 0);
    // Into the raw block line by line, and `]g` lands on the change's line
    s.ed.goto_line(6);
    s.keys("j");
    assert_eq!(s.ed.cursor_line(), 7, "a raw block takes j line by line");
    s.keys("j");
    assert_eq!(s.ed.cursor_line(), 8);
    s.ed.goto_line(0);
    s.keys("]g");
    let doc = s.ed.doc();
    assert_eq!(doc.text.byte_to_line(doc.selection().primary().from()), 7, "the change selected");
    assert!(s.ed.screen.panes[0].3.iter().any(|r| r.0 == 7), "on screen");
}

/// A file whose first lines are a doc comment that folds to more rows than it has lines (one long line flowed),
/// then a function a line.
fn tall_doc_block(s: &mut Shot) {
    s.ed.config.soft_wrap = "never".into();
    let mut text = format!("/// {}\n/// More.\n/// End.\nfn z() {{}}\n", "word ".repeat(60));
    for i in 0..30 {
        text.push_str(&format!("fn a{i}() {{}}\n"));
    }
    s.file("src/p.rs", &text);
}

/// Scrolling counts a folded doc comment's rows as they're drawn, not its lines: `j` below a block that folds
/// to more rows than lines keeps the cursor on screen (it went below the pane's end, the block counted a row a
/// line).
#[test]
fn scrolling_counts_a_folded_blocks_rows() {
    use crate::codeview::Row;
    let mut s = Shot::new(50, 12);
    tall_doc_block(&mut s);
    let doc_rows = s.ed.view.iter().filter(|r| matches!(r, Row::Doc { .. })).count();
    assert!(doc_rows > 3, "folds to more rows than lines: {:?}", s.ed.view);
    for _ in 0..12 {
        s.keys("j");
        let line = s.ed.cursor_line();
        let drawn = s.ed.view.iter().any(|r| matches!(r, Row::Text { line: l, .. } if *l == line));
        assert!(drawn, "line {line} on screen (top {}): {:?}", s.ed.doc().top, s.ed.view);
    }
}

/// `zb` and `zj` count a folded doc comment's rows as drawn: `zb` under it puts the cursor's line at the bottom
/// with the block's last rows above it (it went below the screen, the block counted a row a line), `zj` steps
/// over it whole.
#[test]
fn view_moves_step_over_a_folded_block() {
    use crate::codeview::Row;
    let mut s = Shot::new(50, 12);
    tall_doc_block(&mut s);
    s.ed.goto_line(4);
    s.frame();
    for c in "zb".chars() {
        s.ed.handle_event(Event::Key(Key::plain(Code::Char(c))));
    }
    s.frame();
    let rows = s.ed.viewport.0;
    let so = s.ed.config.scrolloff.min((rows - 1) / 2);
    let at = s.ed.view.iter().position(|r| matches!(r, Row::Text { line: 4, .. }));
    assert_eq!(at, Some(rows - 1 - so), "at the bottom: {:?}", s.ed.view);
    assert!(matches!(s.ed.view[0], Row::Doc { index: 1.., .. }), "the block's last rows: {:?}", s.ed.view);
    // `zj` from the block: over it, to the line after
    s.ed.goto_line(20);
    s.ed.doc_mut().set_top(0);
    for c in "zj".chars() {
        s.ed.handle_event(Event::Key(Key::plain(Code::Char(c))));
    }
    assert_eq!(s.ed.doc().top, 3, "one step over the block");
}

/// In a soft-wrapped file `zt` `zz` `zb` and a follow stop count rows, not lines: a wrapped line above that
/// doesn't fit whole shows its last rows, so the cursor's line lands on the row asked for.
#[test]
fn view_moves_start_partway_into_a_wrapped_line() {
    use crate::codeview::Row;
    let mut s = Shot::new(50, 13);
    (s.ed.config.soft_wrap, s.ed.config.scrolloff) = ("always".into(), 1);
    // (every line wraps to four rows: 10 rows high, none of the places is a whole number of lines down)
    let text: String = (0..40).map(|i| format!("{}\n", format!("l{i:02} ").repeat(40))).collect();
    s.file("notes.txt", &text);
    let line = 20;
    // (where the line's first row is, its rows, whether the view starts partway into a line)
    let at = |s: &Shot| {
        let of = |r: &Row| matches!(r, Row::Text { line: l, .. } if *l == line);
        let partway = matches!(s.ed.view[0], Row::Text { part: Some(p), .. } if p.index > 0);
        (s.ed.view.iter().position(of), s.ed.view.iter().filter(|r| of(r)).count(), partway)
    };
    s.ed.goto_line(line);
    for keys in ["zt", "zz", "zb"] {
        s.ed.doc_mut().set_top(line);
        s.frame();
        for c in keys.chars() {
            s.ed.handle_event(Event::Key(Key::plain(Code::Char(c))));
        }
        s.frame();
        let (rows, (first, n, partway)) = (s.ed.viewport.0, at(&s));
        let above = match keys {
            "zt" => 1,
            "zz" => (rows - n) / 2,
            _ => rows - n - 1,
        };
        assert_eq!((first, partway), (Some(above), true), "{keys} ({n} rows a line): {:?}", s.ed.view);
    }
    // A follow stop: the line centred
    s.ed.doc_mut().set_top(0);
    s.ed.show_range(line..line + 1);
    s.frame();
    let (rows, (first, n, partway)) = (s.ed.viewport.0, at(&s));
    assert_eq!((first, partway), (Some((rows - n) / 2), true), "a follow stop: {:?}", s.ed.view);
}

/// A preview whose first line falls inside a folded doc comment starts at the block's first row — it started on
/// a row inside it, the block cut at the top — or past the block when the item's line wouldn't show then.
#[test]
fn a_preview_never_starts_inside_a_folded_block() {
    use crate::codeview::Row;
    let mut s = Shot::new(50, 12);
    tall_doc_block(&mut s);
    let doc = s.ed.doc();
    // (opens at line 6: the preview starts a third of its rows up, at line 2 — inside the block)
    let rows = |n: usize| super::rows_preview(&s.ed, doc, Some("rust"), (Some(6), Some(6), 0), n, 40);
    let tall = rows(30);
    assert!(matches!(tall[0], Row::Doc { start: 0, index: 0, .. }), "the block whole: {tall:?}");
    let short = rows(12);
    assert!(matches!(short[0], Row::Text { line: 3, .. }), "past it: {short:?}");
    assert!(short.iter().any(|r| matches!(r, Row::Text { line: 6, .. })), "the item's line: {short:?}");
}

/// A picker's item on a file open in a buffer is previewed from the buffer: nothing read from disk for it.
#[test]
fn a_preview_of_an_open_file_reads_no_disk() {
    let mut s = Shot::new(100, 30);
    s.file("src/open.rs", "fn open() {}\n");
    let path = s.ed.doc().path.clone().unwrap();
    let item = |action| crate::picker::Item {
        label: "src/open.rs".into(),
        action,
        hint: String::new(),
        glyph: None,
    };
    let picker = crate::picker::Picker::new("t", vec![item(crate::picker::Action::Open(path.clone()))], true);
    s.ed.open_picker(picker, None);
    s.frame();
    let shown = screen_text(&mut s);
    let p = s.ed.picker.as_ref().unwrap();
    assert!(p.previews.is_empty(), "read: {:?}", p.previews.keys().collect::<Vec<_>>());
    assert!(shown.contains("fn open() {}"), "the buffer's text: {shown}");
}

/// `gw` labels only the words on rows drawn: a wrapped line cut at the pane's end has none past it.
#[test]
fn jump_labels_only_on_the_rows_drawn() {
    use crate::codeview::Row;
    let mut s = Shot::new(40, 8);
    let words: String = (0..80).map(|i| format!("w{i} ")).collect();
    s.file("notes.md", &format!("top\n{words}\n"));
    s.keys("gw");
    let doc = s.ed.doc();
    let labels = s.ed.jump_labels.as_ref().expect("labels up");
    let drawn = |at: usize| {
        let line = doc.text.byte_to_line(at);
        let col = crate::movement::visual_col(&doc.text, at, 4);
        s.ed.view.iter().any(|r| match r {
            Row::Text { line: l, part: Some(p), .. } => *l == line && p.row.col <= col && col < p.row.end_col,
            Row::Text { line: l, part: None, .. } => *l == line,
            _ => false,
        })
    };
    let off: Vec<String> = labels
        .targets
        .iter()
        .filter(|t| !drawn(t.0))
        .map(|t| doc.text.byte_slice(t.0..t.1).to_string())
        .collect();
    assert!(!labels.targets.is_empty() && off.is_empty(), "labelled off screen: {off:?}");
}

/// A click on a row whose text isn't the line's lands on one place of it, wherever it's clicked: a removed line
/// the start of the line it went above, a folded doc comment where the comment starts.
#[test]
fn a_click_on_a_removed_or_folded_row() {
    use crate::codeview::Row;
    let mut s = review_of(100, 24, DOCS_BASE, &docs_now());
    s.ed.goto_line(13);
    s.frame();
    let (_, rect, gutter, _) = s.ed.screen.panes[0].clone();
    let click = |s: &mut Shot, at: &dyn Fn(&Row) -> bool| {
        let row = s.ed.view.iter().position(at).expect("the row");
        for kind in [crate::event::MouseKind::Down, crate::event::MouseKind::Up] {
            let (x, y) = ((rect.x + gutter + 6) as u16, (rect.y + row) as u16);
            s.ed.handle_event(Event::Mouse(crate::event::Mouse { kind, x, y, alt: false }));
        }
        s.frame();
        let doc = s.ed.doc();
        doc.selection().primary().cursor(&doc.text)
    };
    let start = |s: &Shot, l: usize| s.ed.doc().text.line_to_byte(l);
    let removed = click(&mut s, &|r| matches!(r, Row::Other { at: 7, .. }));
    assert_eq!(removed, start(&s, 7), "the start of the line it went above");
    let folded = click(&mut s, &|r| matches!(r, Row::Doc { start: 0, index: 1, .. }));
    assert_eq!(folded, start(&s, 0), "where the comment starts");
}

/// A folded doc comment indented with tabs is drawn at its code's column (it was drawn a cell a tab, left of
/// it), and a click on it lands where the comment starts, past the tabs (it landed inside them).
#[test]
fn a_tab_indented_folded_block_at_its_column() {
    use crate::codeview::Row;
    let mut s = Shot::new(80, 12);
    s.file("src/t.rs", "impl S {\n\t/// Does it.\n\t/// Really.\n\tfn f() {}\n}\n");
    s.ed.goto_line(3);
    let shown = screen_text(&mut s);
    // (the column, in cells, `needle` starts at on its row)
    let col = |needle: &str| {
        let l = shown.lines().find(|l| l.contains(needle)).expect(needle);
        l[..l.find(needle).unwrap()].chars().count()
    };
    assert_eq!(col("▎ Does it."), col("fn f() {}"), "under its code: {shown}");
    let (_, rect, gutter, _) = s.ed.screen.panes[0].clone();
    let row = s.ed.view.iter().position(|r| matches!(r, Row::Doc { start: 1, .. })).expect("folded");
    for kind in [crate::event::MouseKind::Down, crate::event::MouseKind::Up] {
        let (x, y) = ((rect.x + gutter + 6) as u16, (rect.y + row) as u16);
        s.ed.handle_event(Event::Mouse(crate::event::Mouse { kind, x, y, alt: false }));
    }
    let doc = s.ed.doc();
    assert_eq!(doc.selection().primary().cursor(&doc.text), doc.text.line_to_byte(1) + 1, "past the tab");
}

/// A click in Claude's edit review on a change's row (its head, a line it proposes) takes that change in hand;
/// on a row of no change (a folded doc comment) it moves nothing.
#[test]
fn a_click_in_the_edit_review_takes_a_change_in_hand() {
    let mut s = Shot::new(100, 30);
    let id = s.file("src/words.rs", DOCS_BASE);
    let text = Rope::from_str(DOCS_BASE);
    let hunk = |line: usize, new: &str| {
        let (from, to) = (text.line_to_byte(line), text.line_to_byte(line + 1));
        let old = text.byte_slice(from..to).to_string();
        let (decision, conflict) = (crate::llm::Decision::Undecided, false);
        crate::llm::Hunk { from, to, old, new: new.into(), decision, conflict }
    };
    let hunks =
        vec![hunk(4, "    text.split(' ').count()\n"), hunk(13, "    let text = \"proposed here\";\n")];
    let mut rev = crate::llm::Review::new(1, id, "two", hunks);
    (rev.ready, rev.follow) = (true, false);
    s.ed.session.review = Some(rev);
    // (the row showing `needle`, on the screen as it is now)
    let click = |s: &mut Shot, needle: &str| {
        let shown = screen_text(s);
        let y = shown.lines().position(|l| l.contains(needle)).expect(needle);
        for kind in [crate::event::MouseKind::Down, crate::event::MouseKind::Up] {
            s.ed.handle_event(Event::Mouse(crate::event::Mouse { kind, x: 20, y: y as u16, alt: false }));
        }
        s.frame();
        s.ed.session.review.as_ref().unwrap().current
    };
    assert_eq!(click(&mut s, "proposed here"), 1, "the line it proposes");
    // (after change 1 is in hand: a click that took change 0 would show)
    assert_eq!(click(&mut s, "Counts the words in text."), 1, "a folded doc comment: no change");
    assert_eq!(click(&mut s, "Change 1 of 2"), 0, "its head");
}

/// The first lines of the focused pane's folded doc comments (its view, as drawn).
fn folded(s: &Shot) -> Vec<usize> {
    let starts = s.ed.view.iter().filter_map(|r| match r {
        crate::codeview::Row::Doc { start, index: 0, .. } => Some(*start),
        _ => None,
    });
    starts.collect()
}

/// The mode rule, key by key: in NORMAL a block opens only under a selection wider than a character (a line
/// selected above it isn't on it) and `j`/`k` step over it; in INSERT and SELECT any selection on it opens it,
/// and a block away from the cursor stays folded; a prompt or a picker taking the keys changes nothing.
#[test]
fn doc_comments_follow_the_mode() {
    use crate::editor::Mode;
    let mut s = Shot::new(100, 24);
    s.file("src/x.rs", "fn a() {}\n/// One.\n/// Two.\nfn b() {}\n/// Three.\nfn c() {}\n");
    assert_eq!(folded(&s), [1, 4], "NORMAL, the cursor above");
    s.keys("x");
    assert_eq!(folded(&s), [1, 4], "NORMAL: the line above selected isn't the block's");
    s.keys(";j");
    assert_eq!((s.ed.cursor_line(), folded(&s)), (1, vec![1, 4]), "NORMAL: a cursor on it leaves it folded");
    s.keys("j");
    assert_eq!(s.ed.cursor_line(), 3, "NORMAL: j steps over it");
    s.keys("k");
    assert_eq!(s.ed.cursor_line(), 1, "NORMAL: k lands on its first line");
    s.keys("x");
    assert_eq!(folded(&s), [4], "NORMAL: a selection wider than a character opens it");
    s.keys(";i");
    assert_eq!((s.ed.mode, folded(&s)), (Mode::Insert, vec![4]), "INSERT: the cursor on it opens it");
    s.keys("<esc>gg");
    s.keys("i");
    assert_eq!(folded(&s), [1, 4], "INSERT elsewhere: the others stay folded");
    s.keys("<esc>jv");
    assert_eq!((s.ed.mode, folded(&s)), (Mode::Select, vec![4]), "SELECT: on it opens it");
    s.keys("j");
    assert_eq!(s.ed.cursor_line(), 2, "SELECT: j goes line by line through it (raw)");
    s.keys(":");
    assert!(s.ed.prompt.is_some());
    assert_eq!(folded(&s), [4], "a prompt with the keys: as the mode under it");
    s.keys("<esc> ?");
    assert!(s.ed.picker.is_some());
    assert_eq!(folded(&s), [4], "a picker with the keys: as the mode under it");
    s.keys("<esc><esc>");
    assert_eq!((s.ed.mode, folded(&s)), (Mode::Normal, vec![4]), "NORMAL: what SELECT selected stays, wide");
    s.keys(";");
    assert_eq!(folded(&s), [1, 4]);
}

/// Two panes on one file: the one being edited (focused, INSERT) shows the block raw; the other keeps
/// NORMAL's rule and shows it folded.
#[test]
fn doc_comments_in_split_panes() {
    let mut s = Shot::new(140, 24);
    s.file("src/words.rs", DOCS_BASE);
    s.keys("<C-w>vggi");
    assert_eq!(s.ed.session.views.len(), 2);
    let shown = screen_text(&mut s);
    let n = |t: &str| shown.matches(t).count();
    assert_eq!(
        (n("/// Counts"), n("Counts the words in text.")),
        (1, 1),
        "raw where edited, folded beside: {shown}"
    );
    s.frames_both("split_doc_comments");
}

/// Two panes on two files, a.txt without the keys and b.txt with them, then `keys`: the background of each of
/// a.txt's characters as its pane draws them.
fn split_on_two_files(keys: &str) -> (Shot, Vec<(char, vt100::Color)>) {
    const A: &str = "abcdefghijkl";
    let mut s = Shot::new(100, 12);
    s.file("a.txt", &format!("{A}\n"));
    s.keys("<C-w>v:new<ret>");
    s.file("b.txt", "xxxxxxxx foo\n");
    s.keys(keys);
    assert_eq!(s.ed.session.views.len(), 2);
    let p = cells(&s.frame(), s.w, s.h);
    let rows: Vec<String> = p.screen().rows(0, s.w).collect();
    let y = rows.iter().position(|r| r.contains(A)).expect("a.txt's line");
    let x = rows[y].find(A).unwrap();
    let row = A.chars().enumerate();
    let bgs = row.map(|(i, c)| (c, p.screen().cell(y as u16, (x + i) as u16).unwrap().bgcolor())).collect();
    (s, bgs)
}

/// A search is the searched document's: a pane on another file shows none of its hits (it did, at the same
/// offsets).
#[test]
fn search_hits_wash_only_the_searched_document() {
    let (_, bgs) = split_on_two_files("/foo<ret>");
    // (b.txt's "foo" is at 9..12 — a.txt's "jkl")
    let (plain, hit) = (bgs[5].1, bgs[9].1);
    assert_eq!(hit, plain, "a.txt has no hit: {bgs:?}");
}

/// The cell `dx` cells after where `needle` starts on the frame's first row that holds it (`dx` < 0: before).
fn cell_by<'p>(p: &'p vt100::Parser, w: u16, needle: &str, dx: isize) -> &'p vt100::Cell {
    let rows: Vec<String> = p.screen().rows(0, w).collect();
    let y = rows.iter().position(|r| r.contains(needle)).unwrap_or_else(|| panic!("{needle:?}: {rows:#?}"));
    let x = rows[y][..rows[y].find(needle).unwrap()].chars().count() as isize + dx;
    p.screen().cell(y as u16, x as u16).unwrap()
}

/// An edit follow shows is the one diff its view shows: the gutter marks its changes, not the index's (it
/// marked both: the index's bars, the edit's washes and rows).
#[test]
fn a_shown_edit_is_the_one_diff_its_view_shows() {
    let mut s = Shot::new(100, 12);
    s.file("a.txt", "one\ntwo\nthree\nfour\nfive\n");
    let d = s.ed.doc_mut();
    let (added, deleted) = (crate::git::HunkKind::Added, crate::git::HunkKind::Deleted);
    d.git_hunks = vec![
        crate::git::Hunk { kind: added, lines: 0..1, removed: 0, base: 0..0 },
        crate::git::Hunk { kind: deleted, lines: 2..2, removed: 1, base: 1..2 },
    ];
    let edit = crate::replay::Hunk { lines: 3..4, removed: vec!["FOUR".into()], added: vec!["four".into()] };
    d.shown = Some(vec![edit]);
    let p = cells(&s.frame(), s.w, s.h);
    let bar = |word: &str| cell_by(&p, s.w, word, -1).contents();
    assert_eq!([bar("one"), bar("two"), bar("four")], [" ", " ", "▎"], "the edit's bar only");
}

/// The cursor line doesn't hide a change's wash: on it, the wash a step stronger (it was the plain cursor
/// line's).
#[test]
fn a_change_on_the_cursor_line_keeps_its_wash() {
    let mut s = Shot::new(100, 12);
    s.file("a.txt", "one\nadded here\nadded too\nthree\n");
    let d = s.ed.doc_mut();
    d.review_base = Some(Rope::from_str("one\nthree\n"));
    d.git_hunks =
        vec![crate::git::Hunk { kind: crate::git::HunkKind::Added, lines: 1..3, removed: 0, base: 1..1 }];
    d.set_selection(crate::selection::Selection::point(4));
    let p = cells(&s.frame(), s.w, s.h);
    let bg = |word: &str| cell_by(&p, s.w, word, 6).bgcolor();
    let cursorline = vt_color(s.ed.theme.get("ui.cursorline.primary").bg).unwrap();
    let (here, too, plain) = (bg("added here"), bg("added too"), bg("three"));
    assert!(here != cursorline && here != too && too != plain, "{here:?} {too:?} {plain:?} {cursorline:?}");
}

/// A folded doc comment's row carries its lines' signs: the most severe diagnostic among them, and the git
/// mark of their changes' own kind (it had no sign, and its git bar was always "changed").
#[test]
fn a_folded_doc_comment_carries_its_lines_signs() {
    const SRC: &str = "/// Adds two numbers.\n///\n/// Wraps on overflow.\nfn add() {}\n";
    let mut s = Shot::new(100, 12);
    s.ed.config.line_number = crate::config::LineNumber::Absolute;
    s.file("src/add.rs", SRC);
    let d = s.ed.doc_mut();
    d.set_selection(crate::selection::Selection::point(SRC.find("fn").unwrap()));
    let at = SRC.find("overflow").unwrap();
    let diag = crate::lsp::Diagnostic {
        from: at,
        to: at + 8,
        severity: 1,
        message: "x".into(),
        raw: json!({}),
        source: 0,
    };
    d.lsp.diagnostics = vec![diag];
    d.git_hunks =
        vec![crate::git::Hunk { kind: crate::git::HunkKind::Added, lines: 2..3, removed: 0, base: 2..2 }];
    let p = cells(&s.frame(), s.w, s.h);
    let rows: Vec<String> = p.screen().rows(0, s.w).collect();
    assert!(!rows.iter().any(|r| r.contains("///")), "folded: {rows:#?}");
    // (the row: sign, number, git mark, then the block's rendered text)
    let row = rows.iter().position(|r| r.contains("Adds two")).unwrap();
    let x = rows[row].chars().position(|c| c == '1').unwrap() as u16;
    let cell = |x: u16| p.screen().cell(row as u16, x).unwrap();
    let sign = (0..x).rev().map(cell).find(|c| !c.contents().trim().is_empty()).expect("a sign");
    let mark = cell(x + 1);
    let fg = |k: &str| vt_color(s.ed.theme.get(k).fg);
    assert_eq!((sign.contents(), Some(sign.fgcolor())), ("▎", fg("error")), "the error's bar");
    assert_eq!((mark.contents(), Some(mark.fgcolor())), ("▎", fg("diff.plus")), "added, not changed");
}

/// `:diff` of `text` (shown as `rel`) against `base`: the frame once `done` holds of it, or when no event has
/// come for 10 s (the base's parse is a worker's).
fn diff_frame(
    s: &mut Shot,
    rel: &str,
    text: &str,
    base: &str,
    done: impl Fn(&vt100::Parser) -> bool,
) -> vt100::Parser {
    let id = s.file(rel, text);
    let version = s.ed.doc().version();
    s.ed.diff_show(id, version, base.into(), "base".into(), crate::git::hunks(base, text));
    loop {
        let p = cells(&s.frame(), s.w, s.h);
        if done(&p) {
            return p;
        }
        match s.ed.events.recv_timeout(Duration::from_secs(10)) {
            Some(ev) => s.ed.handle_event(ev),
            None => return p,
        }
    }
}

/// A removed line soft-wraps as the lines do (it was cut at the pane's edge).
#[test]
fn a_removed_line_wraps_as_the_lines_do() {
    let mut s = Shot::new(60, 12);
    let long = format!("{}END", "word ".repeat(20));
    let p = diff_frame(&mut s, "a.txt", "keep\nnew\n", &format!("keep\n{long}\n"), |_| true);
    assert!(s.ed.doc().wrap.is_none() && s.ed.wraps(s.ed.doc()), "a file with no language wraps");
    let rows: Vec<String> = p.screen().rows(0, s.w).collect();
    assert!(rows.iter().any(|r| r.contains("END")), "the whole removed line: {rows:#?}");
    let marks: Vec<bool> = rows.iter().filter(|r| r.contains("word")).map(|r| r.contains('−')).collect();
    assert_eq!(marks, [true, false], "two rows, `−` on the first: {rows:#?}");
}

/// The cursor stays in sight below a removed line that wraps: the view counts its rows as drawn.
#[test]
fn the_cursor_stays_in_sight_below_a_wrapped_removed_line() {
    let mut s = Shot::new(60, 9);
    s.ed.config.scrolloff = 0;
    let text = "a\nb\nc\nd\ne\nf\n";
    let base = format!("a\nb\nc\nd\ne\n{}END\nf\n", "word ".repeat(30));
    diff_frame(&mut s, "a.txt", text, &base, |_| true);
    // (from the top: the view scrolls down to the cursor, counting the removed rows above its line)
    s.keys("ggge");
    s.ed.toasts.clear();
    let rows: Vec<String> = cells(&s.frame(), s.w, s.h).screen().rows(0, s.w).collect();
    assert!(rows.iter().any(|r| r.contains("END")), "the removed line's last row: {rows:#?}");
    assert!(rows.iter().any(|r| r.trim_end().ends_with(" f")), "the cursor's line: {rows:#?}");
}

/// A removed line's tabs go to the document's tab stops (each was `tab_width` spaces wherever it stood).
#[test]
fn a_removed_lines_tabs_take_the_tab_stops() {
    let mut s = Shot::new(60, 12);
    s.ed.config.tab_width = 4;
    let p = diff_frame(&mut s, "a.txt", "keep\nend\n", "keep\nab\tx\nend\n", |_| true);
    let rows: Vec<String> = p.screen().rows(0, s.w).collect();
    assert!(rows.iter().any(|r| r.contains("ab  x")), "x at column 4: {rows:#?}");
}

/// A removed line is coloured from the base parsed whole — inside a block comment it's a comment (alone it
/// read as code).
#[test]
fn a_removed_line_takes_its_colours_from_the_whole_base() {
    let mut s = Shot::new(60, 12);
    let comment = vt_color(s.ed.theme.get("comment").fg);
    let fg = |p: &vt100::Parser| Some(cell_by(p, 60, "removed inside", 0).fgcolor());
    let (base, text) = ("/*\nremoved inside\n*/\nfn f() {}\n", "/*\n*/\nfn f() {}\n");
    let p = diff_frame(&mut s, "src/c.rs", text, base, |p| fg(p) == comment);
    assert_eq!(fg(&p), comment, "the base's block comment");
}

/// The base's parse is a frame only when its removed rows are on screen — else nothing's drawn for it.
#[test]
fn the_bases_parse_is_a_frame_only_in_sight() {
    for seen in [true, false] {
        let mut s = Shot::new(60, 12);
        let (base, text) = ("/*\nremoved inside\n*/\nfn f() {}\n", "/*\n*/\nfn f() {}\n");
        let id = s.file("src/c.rs", text);
        let version = s.ed.doc().version();
        s.ed.diff_show(id, version, base.into(), "base".into(), crate::git::hunks(base, text));
        match seen {
            true => drop(s.frame()),
            false => s.ed.screen.panes.clear(),
        }
        let ev = s.ed.events.recv_timeout(Duration::from_secs(10)).expect("the parse");
        s.ed.handle_event(ev);
        assert!(s.ed.doc().base_syntax.is_some(), "parsed");
        assert_eq!(s.ed.unchanged, !seen, "a frame only when seen ({seen})");
    }
}

/// A review's base parse follows sight as the document's own trees do: gone with them from a file not looked at
/// for a while (it was kept for every reviewed file), parsed again once it's shown.
#[test]
fn the_bases_parse_follows_sight() {
    let mut s = Shot::new(60, 12);
    let (base, text) = ("/*\nremoved inside\n*/\nfn f() {}\n", "/*\n*/\nfn f() {}\n");
    let id = s.file("src/c.rs", text);
    let version = s.ed.doc().version();
    s.ed.diff_show(id, version, base.into(), "base".into(), crate::git::hunks(base, text));
    s.until("the parse", |ed| ed.doc().base_syntax.is_some());
    // Other buffers looked at since, ten minutes on: its trees go, the base's with them
    let start = Instant::now();
    for k in 1..=8 {
        s.keys(":new<ret>");
        s.ed.trees_follow_sight(start + Duration::from_secs(600 + k));
    }
    let d = s.ed.docs.iter().find(|d| d.id == id).unwrap();
    assert!(d.syntax.as_ref().is_some_and(|s| s.dormant), "its trees went");
    assert!(d.base_syntax.is_none() && !d.base_parsing, "the base's too");
    // Shown again: parsed again
    while s.ed.doc().id != id {
        s.ed.cycle_buffer(1);
    }
    s.ed.trees_follow_sight(start + Duration::from_secs(700));
    s.until("the parse again", |ed| ed.doc().base_syntax.is_some());
}

/// A review's base parse that lands after its file's trees went is let go, as they were (it was kept until the
/// file was shown again), and asked again once it is.
#[test]
fn a_base_parse_landing_after_the_trees_went_is_let_go() {
    let mut s = Shot::new(60, 12);
    let (base, text) = ("/*\nremoved inside\n*/\nfn f() {}\n", "/*\n*/\nfn f() {}\n");
    let id = s.file("src/c.rs", text);
    let version = s.ed.doc().version();
    s.ed.diff_show(id, version, base.into(), "base".into(), crate::git::hunks(base, text));
    // (its trees go while the parse is out: nothing applies its result meanwhile)
    let start = Instant::now();
    for k in 1..=8 {
        s.keys(":new<ret>");
        s.ed.trees_follow_sight(start + Duration::from_secs(600 + k));
    }
    let parsing = |ed: &Editor| ed.docs.iter().find(|d| d.id == id).unwrap().base_parsing;
    assert!(parsing(&s.ed), "the parse still out");
    s.until("the parse", |ed| !parsing(ed));
    let d = s.ed.docs.iter().find(|d| d.id == id).unwrap();
    assert!(d.syntax.as_ref().is_some_and(|s| s.dormant), "its trees went");
    assert!(d.base_syntax.is_none(), "the base's let go");
    while s.ed.doc().id != id {
        s.ed.cycle_buffer(1);
    }
    s.ed.trees_follow_sight(start + Duration::from_secs(700));
    s.until("the parse again", |ed| ed.doc().base_syntax.is_some());
}

/// A pane without the keys paints its primary cursor as it paints the others (it painted none).
#[test]
fn a_pane_without_the_keys_shows_its_primary_cursor() {
    let (s, bgs) = split_on_two_files("");
    let cursor = vt_color(s.ed.theme.get("ui.cursor").bg).unwrap();
    assert_eq!((bgs[0].1, bgs[1].1 == cursor), (cursor, false), "the cursor on `a`: {bgs:?}");
}

/// Scrolled sideways: a folded block is flowed to the width — it stays folded (unshifted), `j` steps over it.
#[test]
fn doc_comments_under_sideways_scroll() {
    let mut s = Shot::new(80, 24);
    s.ed.config.soft_wrap = "never".into();
    let long = format!("    let text = \"{}\";", "to be or not to be ".repeat(8));
    s.file("src/words.rs", &DOCS_BASE.replace("    let text = \"to be or not to be\";", &long));
    s.ed.goto_line(13);
    s.keys("gl");
    let d = s.ed.doc();
    assert!(d.left > 0, "scrolled sideways: {} at {:?}", d.left, d.selection().primary());
    assert_eq!(folded(&s), [0, 7]);
    assert_eq!(s.ed.folded_doc_block_at(1), Some((0, 2)), "j/k step over it as drawn");
    let shown = screen_text(&mut s);
    assert!(shown.contains("▎ Counts the words in text."), "unshifted: {shown}");
    s.frames_both("sideways_doc_comments");
}

/// Changes shown in the buffer: a block a `:diff` hunk, an edit follow shows or a merge conflict touches stays
/// raw; the one away from them renders.
#[test]
fn doc_comments_by_changes_shown() {
    let now = docs_now();
    // `:diff` (a review base, no code review)
    let mut s = Shot::new(100, 24);
    s.file("src/words.rs", &now);
    let doc = s.ed.doc_mut();
    doc.review_base = Some(Rope::from_str(DOCS_BASE));
    doc.git_hunks = crate::git::hunks(DOCS_BASE, &now);
    doc.git_version = doc.version();
    s.frame();
    assert_eq!(folded(&s), [0], ":diff");
    s.frames_both("diff_doc_comments");
    // An edit of Claude's that follow shows
    let mut s = Shot::new(100, 24);
    s.file("src/words.rs", &now);
    s.ed.doc_mut().shown = Some(vec![crate::replay::Hunk {
        lines: 7..9,
        removed: vec!["/// The longest word.".into()],
        added: now.lines().skip(7).take(2).map(String::from).collect(),
    }]);
    s.frame();
    assert_eq!(folded(&s), [0], "follow");
    // A merge conflict
    let mut s = Shot::new(100, 24);
    let marked = "<<<<<<< ours\n/// The longest word.\n=======\n/// The longest one.\n>>>>>>> theirs\n";
    s.file("src/words.rs", &DOCS_BASE.replace("/// The longest word.\n", marked));
    let doc = s.ed.doc_mut();
    (doc.conflicts, doc.conflicts_of) = (crate::conflict::find(&doc.text), doc.version());
    assert_eq!(doc.conflicts.len(), 1);
    s.frame();
    assert_eq!(folded(&s), [0], "a conflict");
    s.frames_both("conflict_doc_comments");
}

/// A picker's preview shows doc comments as the editor would in NORMAL — but the block the item opens at stays
/// raw (going there selects it).
#[test]
fn doc_comments_in_previews() {
    use crate::picker::{Action, Item, Picker};
    let mut s = Shot::new(110, 30);
    let doc = s.file("src/words.rs", DOCS_BASE);
    s.ed.goto_line(13);
    let item = |label: &str, index, line| Item {
        label: label.into(),
        action: Action::Jump { index, doc, line },
        hint: String::new(),
        glyph: None,
    };
    let items = vec![item("in the first doc comment", 0, 1), item("at count", 1, 3)];
    s.ed.open_picker(Picker::new("places", items, false), None);
    s.frame();
    let shown = screen_text(&mut s);
    assert!(shown.contains("/// Counts") && shown.contains("▎ The longest word."), "{shown}");
    s.check("preview_doc_comments");
    s.frames_both("preview_doc_comments");
    s.keys("<C-n>");
    let shown = screen_text(&mut s);
    assert!(!shown.contains("/// Counts") && shown.contains("▎ Counts the words in text."), "{shown}");
}

/// A picker's code preview is the file as the editor draws it (`Shows::preview`): a buffer's diagnostic — its
/// sign and its line-end message — and numbers as wide as a pane's (`num_w`: a short file's two digits, not
/// four). So for an item that names the buffer (a jump) and one that names its file (the diagnostics picker's,
/// a search's — the file is open: its buffer is what enter opens; this one isn't even on disk).
#[test]
fn a_code_preview_is_drawn_as_the_editor_draws_the_file() {
    let mut s = Shot::new(110, 30);
    let doc = s.file("src/words.rs", DOCS_BASE);
    let path = s.root.join("src/words.rs");
    for (k, action) in [
        crate::picker::Action::Jump { index: 0, doc, line: 4 },
        crate::picker::Action::Goto { path: path.clone(), line: 4, col: 0 },
    ]
    .into_iter()
    .enumerate()
    {
        preview_draws_the_buffer(&mut s, doc, action, k);
    }
}

fn preview_draws_the_buffer(s: &mut Shot, doc: DocId, action: crate::picker::Action, k: usize) {
    use crate::picker::{Item, Picker};
    let at = DOCS_BASE.find("text.split_whitespace().count()").unwrap();
    let d = s.ed.docs.iter_mut().find(|d| d.id == doc).unwrap();
    d.lsp.diagnostics = vec![crate::lsp::Diagnostic {
        from: at,
        to: at + 4,
        severity: 1,
        message: "no such text".into(),
        raw: json!({}),
        source: 0,
    }];
    let item = Item { label: "count's body".into(), action, hint: String::new(), glyph: None };
    s.ed.open_picker(Picker::new("places", vec![item], false), None);
    let p = cells(&s.frame(), s.w, s.h);
    let shown = p.screen().contents();
    assert!(shown.contains("no such text"), "item {k}, its line-end message: {shown}");
    // (the line: its sign, its number in two cells, the change mark's cell, then its four spaces of indent —
    // from the preview's edge, where its title starts)
    let line = "text.split_whitespace().count()";
    assert_eq!(cell_by(&p, s.w, line, -6).contents(), "5", "{shown}");
    assert_eq!(cell_by(&p, s.w, line, -8).contents(), "▎", "its sign: {shown}");
    let x_of = |needle: &str| {
        let rows: Vec<String> = p.screen().rows(0, s.w).collect();
        let row = rows.iter().find(|r| r.contains(needle)).unwrap();
        row[..row.find(needle).unwrap()].chars().count()
    };
    assert_eq!(x_of(line) - x_of("src/words.rs"), 4 + 4, "a pane's gutter, then the indent: {shown}");
}

/// Claude's edit under review: an unchanged doc comment renders, the one the edit changes stays raw.
#[test]
fn snapshot_edit_review_doc_comments() {
    let mut s = Shot::new(100, 24);
    let id = s.file("src/words.rs", DOCS_BASE);
    let text = Rope::from_str(DOCS_BASE);
    let (from, to) = (text.line_to_byte(7), text.line_to_byte(8));
    let hunk = crate::llm::Hunk {
        from,
        to,
        old: text.byte_slice(from..to).to_string(),
        new: "/// The longest word, if there is one.\n/// Ties go to the first of them.\n".into(),
        decision: crate::llm::Decision::Undecided,
        conflict: false,
    };
    let mut rev = crate::llm::Review::new(1, id, "say what it does on a tie", vec![hunk]);
    // (from the top: following, the change sits a quarter down and the first block's rows go above)
    (rev.ready, rev.follow) = (true, false);
    s.ed.session.review = Some(rev);
    let shown = screen_text(&mut s);
    assert!(!shown.contains("/// Counts") && shown.contains("Counts the words in text."), "{shown}");
    assert!(shown.contains("/// The longest word.") && shown.contains("/// Ties go"), "raw: {shown}");
    s.check("edit_review_doc_comments");
    s.frames_both("edit_review_doc_comments");
}

/// Claude's edit under review shows the file as the editor does (#9): a breakpoint in the sign column, a
/// diagnostic at its line's end, indent guides on its document's rows — and a proposed line's tab at the
/// document's tab stops (#5: it was four spaces wherever it stood). With `TARAE_FRAMES_DIR` set, the frame in
/// both themes (`target/sb/shots-step6.sh` draws them).
#[test]
fn the_edit_review_shows_the_file_as_the_editor_does() {
    let mut s = Shot::new(100, 24);
    let text = "/// Runs it, **once**: `go` then `done`.\nfn main() {\n    if ready {\n        go();\n    }\n    let a = 1;\n    \
                done();\n}\n";
    let id = s.file("src/demo.rs", text);
    let path = s.root.join("src/demo.rs");
    s.ed.breakpoints.entry(path).or_default().insert(2, Default::default());
    let rope = Rope::from_str(text);
    let at = |line: usize| rope.line_to_byte(line);
    s.ed.doc_mut().lsp.diagnostics.push(crate::lsp::Diagnostic {
        from: at(6) + 4,
        to: at(6) + 10,
        severity: 2,
        message: "never read".into(),
        raw: serde_json::Value::Null,
        source: Default::default(),
    });
    let hunk = crate::llm::Hunk {
        from: at(5),
        to: at(6),
        old: "    let a = 1;\n".into(),
        new: "    let a\t= 1;\n".into(),
        decision: crate::llm::Decision::Undecided,
        conflict: false,
    };
    let mut rev = crate::llm::Review::new(1, id, "tab it", vec![hunk]);
    (rev.ready, rev.follow) = (true, false);
    s.ed.session.review = Some(rev);
    let shown = screen_text(&mut s);
    let row = |has: &str| shown.lines().find(|l| l.contains(has)).unwrap_or_default().to_string();
    let mut missing = Vec::new();
    if !row("if ready").starts_with("● 3") {
        missing.push("the breakpoint's ● before line 3's number");
    }
    if !row("done();").contains("▲ never read") {
        missing.push("the diagnostic at its line's end");
    }
    if !row("go();").contains("│   go();") {
        missing.push("the indent guide");
    }
    if !shown.contains("let a   = 1;") || shown.contains("let a    = 1;") {
        missing.push("the proposed line's tab at the tab stop");
    }
    assert!(missing.is_empty(), "missing: {missing:?}\n{shown}");
    s.frames_both("edit_review_file");
}

/// The edit review has no cursor: a selection and several cursors on the rows it shows paint nothing, and no
/// line's number is marked as the cursor's — the frame is the one drawn with the cursor off screen.
#[test]
fn the_edit_review_shows_none_of_the_documents_selection() {
    use crate::selection::{Range, Selection};
    let text: String = (0..30).map(|i| format!("line {i}\n")).collect();
    let frame = |sel: Selection| {
        let mut s = Shot::new(100, 24);
        let id = s.file("a.txt", &text);
        let rope = Rope::from_str(&text);
        let (from, to) = (rope.line_to_byte(10), rope.line_to_byte(11));
        let hunk = crate::llm::Hunk {
            from,
            to,
            old: "line 10\n".into(),
            new: "line ten\n".into(),
            decision: crate::llm::Decision::Undecided,
            conflict: false,
        };
        let mut rev = crate::llm::Review::new(1, id, "spell it", vec![hunk]);
        (rev.ready, rev.follow) = (true, false);
        s.ed.session.review = Some(rev);
        s.ed.doc_mut().set_selection(sel);
        let p = cells(&s.frame(), s.w, s.h);
        // (the status line says where the cursor is: left out)
        let mut all = Vec::new();
        for y in 0..s.h - 1 {
            for x in 0..s.w {
                let c = p.screen().cell(y, x).unwrap();
                all.push((y, x, c.contents().to_string(), c.fgcolor(), c.bgcolor(), c.bold(), c.inverse()));
            }
        }
        (all, p.screen().contents())
    };
    // Lines 2–5: a range, a wider one (the primary), a character, a cursor — against one cursor on line 29
    let at = |line: usize, col: usize| line * 7 + col;
    let ranges =
        vec![Range::new(at(2, 0), at(2, 4)), Range::new(at(3, 1), at(4, 3)), Range::new(at(5, 2), at(5, 3))];
    let many = Selection::new([ranges, vec![Range::point(at(5, 5))]].concat(), 1);
    let (with, shown) = frame(many);
    let (without, _) = frame(Selection::point(text.len() - 3));
    assert!(shown.contains("line 3") && shown.contains("line ten"), "{shown}");
    let differ: Vec<_> = with.iter().zip(&without).filter(|(a, b)| a != b).map(|(a, _)| (a.0, a.1)).collect();
    assert!(differ.is_empty(), "cells differ at {differ:?}\n{shown}");
}

/// A control character in a row that isn't the document's is shown as `�`, as in a line of the document — a code
/// review's removed row and an edit review's proposed one (they went to the screen as is, and it blanked them).
#[test]
fn a_control_character_in_a_row_not_the_documents_shows_as_a_replacement() {
    let mut s = Shot::new(100, 12);
    s.file("a.txt", "one\nthree\n");
    let d = s.ed.doc_mut();
    d.review_base = Some(Rope::from_str("one\nt\u{7}wo\nthree\n"));
    d.git_hunks =
        vec![crate::git::Hunk { kind: crate::git::HunkKind::Deleted, lines: 1..1, removed: 1, base: 1..2 }];
    // (read from the frame's bytes: vt100 drops a `�` it's given)
    let has = |frame: &[u8], s: &str| frame.windows(s.len()).any(|w| w == s.as_bytes());
    let removed = s.frame();

    let mut s = Shot::new(100, 12);
    let id = s.file("b.txt", "one\ntwo\n");
    let hunk = crate::llm::Hunk {
        from: 4,
        to: 8,
        old: "two\n".into(),
        new: "t\u{7}wo!\n".into(),
        decision: crate::llm::Decision::Undecided,
        conflict: false,
    };
    let mut rev = crate::llm::Review::new(1, id, "bell it", vec![hunk]);
    (rev.ready, rev.follow) = (true, false);
    s.ed.session.review = Some(rev);
    let proposed = s.frame();
    let text = |f: &[u8]| String::from_utf8_lossy(f).into_owned();
    assert!(has(&removed, "t\u{fffd}wo"), "the removed row: {}", text(&removed));
    assert!(has(&proposed, "t\u{fffd}wo!"), "the proposed row: {}", text(&proposed));
}

/// Frictions 13, #1: a turn's review says it's the last turn, and under its question what it leaves out —
/// `9 more differ from HEAD · space v v` on one row where the rail is wide enough, the key on its own row
/// where it isn't; the files and the key hints still in sight under it.
#[test]
fn a_turns_rail_says_what_else_differs_from_head() {
    let (base, now) = five_and_thirty_added();
    for (need, rows_want) in [(None, 2), (Some(44), 1)] {
        let mut s = review_of(160, 30, &base, &now);
        let r = s.ed.session.code_review.as_mut().unwrap();
        r.base = crate::codereview::Base::Turn { at: 0, question: "fix it".into() };
        r.beyond = Some((9, "space v v".into()));
        // (measured again: `review_of` measured it for a review of HEAD)
        r.rail_need.take();
        if let Some(n) = need {
            r.rail_need.get_or_init(|| n);
        }
        let mut parser = vt100::Parser::new(s.h, s.w, 0);
        parser.process(&s.frame());
        let rows: Vec<String> = parser.screen().rows(0, s.w).collect();
        assert!(rows[0].contains("review") && rows[0].contains("last turn"), "{rows:#?}");
        assert!(rows[2].contains("“fix it”"), "{rows:#?}");
        match rows_want {
            1 => assert!(rows[3].contains("9 more differ from HEAD · space v v"), "{rows:#?}"),
            _ => {
                assert!(
                    rows[3].contains("9 more differ from HEAD") && !rows[3].contains("space"),
                    "{rows:#?}"
                );
                assert!(rows[4].trim_start().starts_with("space v v "), "{rows:#?}");
            }
        }
        assert!(rows[3 + rows_want].contains("1 file"), "the counts under it: {rows:#?}");
        assert!(rows.iter().any(|r| r.contains("demo.rs")), "{rows:#?}");
        assert!(rows.iter().any(|r| r.contains("space u") && r.contains("undo the whole turn")), "{rows:#?}");
    }
}

/// Frictions 13, #2: the rail's counts are every line, and what moved is said beside them, faint — on the
/// file's row (the rail measured wide enough for it) and, each move once, on the counts' row.
#[test]
fn the_rail_says_what_moved() {
    let (base, now) = five_and_thirty_added();
    let mut s = review_of(160, 30, &base, &now);
    let r = s.ed.session.code_review.as_mut().unwrap();
    let path = r.files[0].path.clone();
    r.moved.insert(path.clone(), 30);
    // (graded as the review's worker grades it: a behaviour change, its `•` measured with the rest)
    let behaviour = crate::risk::Graded {
        grade: crate::risk::Grade::Behaviour,
        why: String::new(),
        at: 5,
        lines: 30,
        keys: Vec::new(),
        removed: 0,
        added: 0,
        mode: false,
        piece: None,
    };
    r.grades.insert(path, std::sync::Arc::from(vec![behaviour]));
    r.rail_need.take();
    let mut parser = vt100::Parser::new(s.h, s.w, 0);
    parser.process(&s.frame());
    let rows: Vec<String> = parser.screen().rows(0, s.w).collect();
    assert!(rows.iter().any(|r| r.contains("◦ demo.rs • +30 · 30 moved")), "the name whole: {rows:#?}");
    assert!(rows.iter().any(|r| r.contains("1 file  +30 · 15 moved")), "{rows:#?}");
    let row = rows.iter().find(|r| r.contains("◦ demo.rs")).unwrap();
    let screen = parser.screen();
    let y = rows.iter().position(|r| r == row).unwrap();
    let x = row.find("30 moved").map(|b| row[..b].chars().count()).unwrap();
    let (moved, count) =
        (screen.cell(y as u16, x as u16).unwrap(), screen.cell(y as u16, (x - 6) as u16).unwrap());
    assert_ne!(moved.fgcolor(), count.fgcolor(), "faint, not the count's colour");
}

/// Review of frictions 13, #2: the rail measures the grade it draws — the open document's diff as it is now,
/// not what the review opened with — so a `▲1` found since doesn't cut the name.
#[test]
fn the_rail_measures_the_grade_it_draws() {
    let (base, now) = five_and_thirty_added();
    let now = format!("{now}let pid = unsafe {{ getpid() }};\n");
    let mut s = review_of(160, 30, &base, &now);
    let r = s.ed.session.code_review.as_mut().unwrap();
    assert!(r.grades.is_empty(), "the review opened with no grade");
    r.set_rules(crate::risk::Rules::with(&[("rust", &["rust"], &[r"\bunsafe\b"], &[])], &[], &[]));
    r.moved.insert(r.files[0].path.clone(), 30000);
    r.rail_need.take();
    let mut parser = vt100::Parser::new(s.h, s.w, 0);
    parser.process(&s.frame());
    let rows: Vec<String> = parser.screen().rows(0, s.w).collect();
    // (its 31-line change taller than the pane: two pieces to look at — cardburden 2)
    assert!(rows.iter().any(|r| r.contains("◦ demo.rs ▲2 +31 · 30000 moved")), "the name whole: {rows:#?}");
}

/// Frictions 13, #3: where a click on the rail's first file row opened nothing — a 188×57 screen, five
/// sessions, a turn's review of 28 files in folders three deep, the reviewer's and the high-risk rows, the
/// chat open and focused. A click on each file row's name opens that file; the first one's too, at the cell
/// the user clicked (column 12, row 8 — 1-based).
#[test]
fn a_click_on_the_rails_file_names_opens_them_in_a_full_layout() {
    use crate::codereview::{Base, CodeReview, File};
    use crate::event::{Mouse, MouseKind};
    let scratch = crate::scratch::Scratch::new("rail-full");
    let dir = scratch.to_path_buf();
    let mut rels: Vec<String> = ["c-sharp", "go", "java", "python", "rust", "zig"]
        .iter()
        .map(|l| format!("runtime/packs/{l}/pack.toml"))
        .collect();
    rels.extend((0..22).map(|i| format!("src/f{i:02}.rs")));
    let files: Vec<File> = rels
        .iter()
        .map(|rel| {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "let x = 1;\nlet y = 2;\n").unwrap();
            File { path, rel: rel.clone(), added: 2, removed: 1, new: false, gone: false, binary: false }
        })
        .collect();
    let mut s = Shot::new(188, 57);
    s.ed.session.root = dir.clone();
    s.ed.open(&dir.join("src/f05.rs")).unwrap();
    let base = Base::Turn { at: 0, question: "fix the frictions".into() };
    let mut r = CodeReview::for_test(base, dir.clone(), files);
    for rel in ["src/f03.rs", "src/f09.rs"] {
        let high = crate::risk::Graded {
            grade: crate::risk::Grade::High,
            why: "unsafe (rust pack)".into(),
            at: 0,
            lines: 1,
            keys: vec![1],
            removed: 0,
            added: 1,
            mode: false,
            piece: None,
        };
        r.grades.insert(dir.join(rel), std::sync::Arc::from(vec![high]));
    }
    let (of, root) = (r.started, r.root.clone());
    s.ed.session.code_review = Some(r);
    s.ed.session.reviewer = Some(crate::reviewer::Reviewer::for_test(
        of,
        root,
        crate::reviewer::State::Done,
        Vec::new(),
        "Two risks.",
    ));
    let mut chat = crate::chat::Chat::new();
    chat.focused = true;
    s.ed.session.chat = Some(chat);
    for name in ["b", "c", "d", "e"] {
        let other = dir.join(format!("session-{name}"));
        std::fs::create_dir_all(&other).unwrap();
        s.ed.session_add(other);
    }
    let screen = |s: &mut Shot| {
        let mut p = vt100::Parser::new(s.h, s.w, 0);
        p.process(&s.frame());
        p.screen().rows(0, s.w).collect::<Vec<String>>()
    };
    let rows = screen(&mut s);
    assert!(rows.iter().any(|r| r.contains("│ │ ◦ pack.toml")), "folders three deep: {rows:#?}");
    let rail = s.ed.screen.review_rail.borrow().clone();
    assert_eq!(rail.len(), 28, "every file in sight: {rows:#?}");
    let opened = |s: &Shot| s.ed.doc().path.clone();
    // The cell the user clicked: the first file row, on its name
    let (y, i) = rail[0];
    s.ed.handle_mouse(Mouse { kind: MouseKind::Down, x: 11, y: y as u16, alt: false });
    s.ed.handle_mouse(Mouse { kind: MouseKind::Up, x: 11, y: y as u16, alt: false });
    let want = s.ed.session.code_review.as_ref().unwrap().files[i].path.clone();
    assert_eq!(opened(&s), Some(want), "the first row's file: {rows:#?}");
    // Every row, on its name
    for (y, i) in rail {
        let rows = screen(&mut s);
        let f = s.ed.session.code_review.as_ref().unwrap().files[i].clone();
        let name = f.rel.rsplit('/').next().unwrap();
        let x = rows[y].find(name).map(|b| rows[y][..b].chars().count()).expect("its name");
        s.ed.handle_mouse(Mouse { kind: MouseKind::Down, x: x as u16 + 1, y: y as u16, alt: false });
        s.ed.handle_mouse(Mouse { kind: MouseKind::Up, x: x as u16 + 1, y: y as u16, alt: false });
        assert_eq!(opened(&s).as_deref(), Some(f.path.as_path()), "row {y}: {}", rows[y]);
    }
    std::fs::remove_dir_all(&dir).ok();
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
    s.ed.session.code_review = Some(CodeReview::for_test(Base::Head, dir.clone(), files));
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

/// frictions25 R1/R2: netgrant's review — its 50 files (3248e87f's) in folders, 14 with lines moved, scrolled
/// into the middle — at two sizes: every row drawn with a file's name clicks that file; then the head grows
/// (the reviewer's row and its verdict, a file changed since viewed — a row the rail used to leave uncounted)
/// and each file stays on its row, the keys whole at the bottom: a click on the row aimed at before opens the
/// same file, and `space v m` marks that one.
#[test]
fn every_rail_row_clicks_the_file_drawn_on_it_while_its_head_grows() {
    use crate::codereview::{Base, CodeReview, File};
    use crate::event::{Mouse, MouseKind};
    let scratch = crate::scratch::Scratch::new("rail-netgrant");
    let dir = scratch.to_path_buf();
    let rels = "compat/go.toml compat/java-gradle.toml compat/java-maven.toml compat/node-npm.toml \
        compat/node-pnpm.toml compat/python-pip.toml compat/python-uv.toml compat/rust.toml docs/design/cards.md \
        docs/threat-model.md runtime/packs/go/pack.toml runtime/packs/javascript/pack.toml \
        runtime/packs/python/pack.toml runtime/packs/rust/pack.toml src/allow.rs src/blocked.rs src/caps.rs \
        src/cards.rs src/chat.rs src/compat.rs src/config.rs src/editor.rs src/hosts.rs src/hosts/tests.rs \
        src/offer.rs src/sandbox.rs src/sandbox/golden.rs src/sandbox/guarantees.rs src/sandbox/keeper.rs \
        src/sandbox/linux.rs src/sandbox/policy.rs src/sandbox/proxy.rs src/sandbox/proxy/egress.rs \
        src/sandbox/proxy/macos_tests.rs src/sandbox/proxy/tests.rs src/sandboxpack.rs src/sandboxpack/tests.rs \
        src/session.rs src/snapshots/host_closed.txt src/snapshots/host_closed_narrow.txt \
        src/snapshots/sandbox_blocked_card.txt src/snapshots/sandbox_landlock.txt src/snapshots/sandbox_policy.txt \
        src/snapshots/sandbox_report.txt src/snapshots/ui_state.json src/term.rs src/term_snapshots.rs \
        src/tools.rs src/uistate/tests.rs xtask/src/compat.rs";
    let files: Vec<File> = rels
        .split_whitespace()
        .map(|rel| {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "x\n").unwrap();
            File { path, rel: rel.into(), added: 120, removed: 340, new: false, gone: false, binary: false }
        })
        .collect();
    assert_eq!(files.len(), 50);
    for (w, h) in [(160, 30), (200, 40)] {
        let mut s = Shot::new(w, h);
        s.ed.session.root = dir.clone();
        s.ed.open(&files[0].path).unwrap();
        let mut r = CodeReview::for_test(Base::Head, dir.clone(), files.clone());
        for f in files.iter().step_by(3).take(14) {
            r.moved.insert(f.path.clone(), 123);
        }
        let (of, root) = (r.started, r.root.clone());
        s.ed.session.code_review = Some(r);
        // (in the middle: src/compat.rs's neighbours, hosts/ among them)
        crate::codereview::show(&mut s.ed, 21, false);
        let screen = |s: &mut Shot| -> Vec<String> {
            let mut p = vt100::Parser::new(s.h, s.w, 0);
            p.process(&s.frame());
            p.screen().rows(0, s.w).collect()
        };
        let rail = |s: &Shot| s.ed.screen.review_rail.borrow().clone();
        // Each row the rail clicks to a file shows that file's name (cut from the left, its end stays)
        let check = |rows: &[String], rail: &[(usize, usize)], at: &str| {
            assert!(rail.len() > 10, "{at}: rows of files\n{}", rows.join("\n"));
            for &(y, i) in rail {
                let drawn = rows[y].split("◦ ").nth(1).and_then(|t| t.split(' ').next()).unwrap_or_default();
                let drawn = drawn.trim_start_matches('…');
                assert!(
                    !drawn.is_empty() && files[i].rel.ends_with(drawn),
                    "{w}×{h} {at}: row {y} clicks {}\n{}",
                    files[i].rel,
                    rows.join("\n")
                );
            }
            // (the keys whole at the bottom, over the status line)
            assert!(rows[h as usize - 2].contains("q end"), "{w}×{h} {at}: the keys\n{}", rows.join("\n"));
        };
        let rows = screen(&mut s);
        let before = rail(&s);
        check(&rows, &before, "as it opened");
        // The head grows by three rows: the reviewer done, its verdict, and a file changed since viewed
        s.ed.session.reviewer = Some(crate::reviewer::Reviewer::for_test(
            of,
            root.clone(),
            crate::reviewer::State::Done,
            Vec::new(),
            "Two risks.",
        ));
        let changed = files[5].path.clone();
        crate::codereview::mark_for_test(&mut s.ed, &changed);
        let rows = screen(&mut s);
        let after = rail(&s);
        check(&rows, &after, "its head grown");
        assert!(rows.iter().any(|r| r.contains("changed since viewed")), "{}", rows.join("\n"));
        // The row aimed at as it opened still holds its file
        let (y, i) = before[before.len() / 2];
        assert!(after.contains(&(y, i)), "{w}×{h}: row {y} kept {}: {before:?} → {after:?}", files[i].rel);
        s.ed.handle_mouse(Mouse { kind: MouseKind::Down, x: 12, y: y as u16, alt: false });
        s.ed.handle_mouse(Mouse { kind: MouseKind::Up, x: 12, y: y as u16, alt: false });
        assert_eq!(s.ed.doc().path.as_deref(), Some(files[i].path.as_path()));
        screen(&mut s);
        assert!(rail(&s).contains(&(y, i)), "opened, it stays where it was clicked");
        s.keys("<space>vm");
        let viewed = |s: &Shot, k: usize| {
            let r = s.ed.session.code_review.as_ref().unwrap();
            r.viewed(&s.ed, &r.files[k])
        };
        assert!(viewed(&s, i), "{w}×{h}: space v m marked the file clicked, {}", files[i].rel);
        // (R3: the head says how many are left)
        let rows = screen(&mut s);
        assert!(rows.iter().any(|r| r.contains("1 of 50 viewed · 49 unviewed")), "{}", rows.join("\n"));
        // …and the head shrinks again: still there
        s.ed.session.reviewer = None;
        screen(&mut s);
        assert!(rail(&s).contains(&(y, i)), "{w}×{h}: the head shrunk, row {y} kept {}", files[i].rel);
    }
}

/// frictions25 R6, fixed: the chat narrows beside the review's rail only where the rail is then drawn — at 120
/// columns it isn't (the code would have 84, under its 90), so the chat keeps the width it has without a review
/// (it lost 12 columns for nothing, and its rows wrapped: e2e `follow_shows_each_edit_and_replays_the_turn`);
/// at 160 the chat gives way to the rail and the code.
#[test]
fn the_chat_narrows_for_the_review_rail_only_where_the_rail_is_drawn() {
    use crate::codereview::{Base, CodeReview, File};
    let scratch = crate::scratch::Scratch::new("rail-chat-width");
    let dir = scratch.to_path_buf();
    let files: Vec<File> = ["src/a.rs", "src/b.rs"]
        .iter()
        .map(|rel| {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "x\n").unwrap();
            File { path, rel: rel.to_string(), added: 1, removed: 0, new: false, gone: false, binary: false }
        })
        .collect();
    for (w, review_chat, rail) in [(120, 48, false), (160, crate::sizes::CHAT_MIN, true)] {
        let mut s = Shot::new(w, 30);
        s.ed.session.root = dir.clone();
        s.ed.config.llm.command = "sh".into();
        s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        crate::chat::open(&mut s.ed);
        s.ed.session.chat.as_mut().unwrap().focused = false;
        s.ed.open(&files[0].path).unwrap();
        s.frame();
        let alone = s.ed.screen.chat_w;
        assert_eq!(alone, crate::sizes::widths(w as usize, true, false, Default::default()).chat);
        s.ed.session.code_review = Some(CodeReview::for_test(Base::Head, dir.clone(), files.clone()));
        s.frame();
        assert_eq!(
            s.ed.screen.chat_w, review_chat,
            "{w} columns: the chat beside the review (alone: {alone})"
        );
        assert_eq!(s.ed.screen.rail_w > 0, rail, "{w} columns: the rail drawn");
    }
}

/// #179: a turn's review of 80 files in folders (~78 met), beside the chat, at heights from small to tall:
/// the rail's rows stay inside it — its counts (`N of 80 viewed`) at the top and its keys at the bottom
/// always in sight, the files between them counted above and below (`↑ a more` · shown · `↓ b more` = 80);
/// the file in hand brought into view, anywhere in the list, without the head or the keys moving.
#[test]
fn review_rail_with_80_files_scrolls_and_keeps_its_counts() {
    use crate::codereview::{Base, CodeReview, File};
    let scratch = crate::scratch::Scratch::new("rail-80");
    let dir = scratch.to_path_buf();
    let rel = |i: usize| format!("src/m{}/part{}/f{i:02}.rs", i / 10, i % 3);
    let mut files: Vec<File> = (0..79)
        .map(|i| {
            let rel = rel(i);
            std::fs::create_dir_all(dir.join(&rel).parent().unwrap()).unwrap();
            std::fs::write(dir.join(&rel), "x\n").unwrap();
            File { path: dir.join(&rel), rel, added: 3, removed: 1, new: false, gone: false, binary: false }
        })
        .collect();
    // (one of Claude's temp files outside the checkout: a row more in the head)
    let out = PathBuf::from("/tmp/tarae-rail-80/f79.rs");
    files.push(File {
        path: out.clone(),
        rel: out.display().to_string(),
        added: 2,
        removed: 0,
        new: true,
        gone: false,
        binary: false,
    });
    for (w, h, keys) in [(160, 16, false), (160, 20, true), (160, 30, false), (200, 50, true)] {
        let mut s = Shot::new(w, h);
        s.ed.session.root = dir.clone();
        s.ed.config.llm.command = "sh".into();
        s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        crate::chat::open(&mut s.ed);
        s.ed.session.chat.as_mut().unwrap().focused = false;
        s.ed.open(&dir.join(rel(0))).unwrap();
        let turn = Base::Turn { at: 0, question: "split the modules".into() };
        s.ed.session.code_review = Some(CodeReview::for_test(turn, dir.clone(), files.clone()));
        // (the rail with the keys: its own hints — `]F file` last)
        if keys {
            crate::codereview::rail_focus(&mut s.ed);
        }
        let last_key = if keys { "]F file" } else { "q keep" };
        let rows = |s: &mut Shot| -> Vec<String> {
            let mut p = vt100::Parser::new(s.h, s.w, 0);
            p.process(&s.frame());
            p.screen().rows(0, s.w).collect()
        };
        let check = |rows: &[String], at: &str| {
            let all = rows.join("\n");
            let row_of = |t: &str| rows.iter().position(|r| r.contains(t));
            let counts = row_of("of 80 viewed").unwrap_or_else(|| panic!("{at}: the counts in sight\n{all}"));
            assert!(row_of("80 files").is_some_and(|r| r < counts), "{at}: the head\n{all}");
            // The keys at the bottom of the rail, right over the status line, whole
            let keys = row_of(last_key).unwrap_or_else(|| panic!("{at}: the keys in sight\n{all}"));
            assert_eq!(keys, h as usize - 2, "{at}: the last key row ends the rail\n{all}");
            let n = |arrow: &str| {
                rows.iter()
                    .find_map(|r| r.split(&format!("{arrow} ")).nth(1)?.split(" more").next()?.parse().ok())
                    .unwrap_or(0usize)
            };
            let shown: Vec<usize> = rows
                .iter()
                .enumerate()
                .filter(|(_, r)| r.contains("◦ f") || r.contains("● f"))
                .map(|(y, _)| y)
                .collect();
            assert!(!shown.is_empty(), "{at}: some files\n{all}");
            assert!(
                shown.iter().all(|&y| y > counts && y < keys - 4),
                "{at}: files between head and keys\n{all}"
            );
            assert_eq!(n("↑") + shown.len() + n("↓"), 80, "{at}: every file counted\n{all}");
            (n("↑"), n("↓"))
        };
        let (up, down) = check(&rows(&mut s), &format!("{h} rows, the first file"));
        assert_eq!(up, 0, "{h} rows: from the top");
        assert!(down > 0, "{h} rows: more below");
        // The last file, then one in the middle: in view (the row in hand, `▎`), the head and keys where they
        // were — shown from the code, or moved to with the rail's own keys while it has them
        let order = s.ed.session.code_review.as_ref().unwrap().rail_order();
        for at_ in [order.len() - 1, 41] {
            let i = order[at_];
            match keys {
                false => crate::codereview::show(&mut s.ed, i, false),
                true if at_ == order.len() - 1 => s.keys("G"),
                true => {
                    s.keys("gg");
                    (0..at_).for_each(|_| s.keys("j"));
                }
            }
            let r = rows(&mut s);
            let at = format!("{h} rows, file {i}");
            let (_, down) = check(&r, &at);
            let name =
                std::path::Path::new(&files[i].rel).file_name().unwrap().to_string_lossy().into_owned();
            assert!(
                r.iter().any(|row| row.starts_with('▎') && row.contains(&format!("◦ {name}"))),
                "{at}: in view, in hand\n{}",
                r.join("\n")
            );
            if at_ == order.len() - 1 {
                assert_eq!(down, 0, "{at}: nothing below the last");
            }
        }
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// #73, #67: a turn's review with folders, the chat open beside it: the wheel scrolls the rail, a click on
/// `↓ N more` / `↑ N more` scrolls it a page; a click on a file's `◦` opens it and marks it viewed — a
/// second click right after (the file still opening) leaves it marked, and `space v m` still toggles.
#[test]
fn review_rail_scrolls_by_click_and_marks_by_click() {
    use crate::codereview::{Base, CodeReview, File};
    use crate::event::{Mouse, MouseKind};
    let scratch = crate::scratch::Scratch::new("rail-marks");
    let dir = scratch.to_path_buf();
    let files: Vec<File> = (0..30)
        .map(|i| {
            let rel = format!("src/m{}/f{i:02}.txt", i / 5);
            std::fs::create_dir_all(dir.join(&rel).parent().unwrap()).unwrap();
            std::fs::write(dir.join(&rel), "x\n").unwrap();
            File { path: dir.join(&rel), rel, added: 1, removed: 0, new: true, gone: false, binary: false }
        })
        .collect();
    let mut s = Shot::new(160, 24);
    s.ed.session.root = dir.clone();
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    crate::chat::open(&mut s.ed);
    s.ed.open(&dir.join("src/m0/f00.txt")).unwrap();
    let turn = Base::Turn { at: 0, question: "tidy the modules".into() };
    s.ed.session.code_review = Some(CodeReview::for_test(turn, dir.clone(), files));
    s.frame();
    let rail = |s: &Shot| s.ed.screen.review_rail.borrow().clone();
    let more = |s: &Shot| s.ed.screen.review_rail_more.borrow().clone();
    let at = |s: &mut Shot, kind, x: usize, y: usize| {
        s.ed.handle_event(Event::Mouse(Mouse { kind, x: x as u16, y: y as u16, alt: false }));
        s.frame();
    };
    let before = rail(&s);
    assert!(before.len() < 30 && !before.is_empty(), "{before:?}");
    // The wheel
    at(&mut s, MouseKind::ScrollDown, 4, before[0].0);
    assert!(rail(&s)[0].1 > before[0].1, "the wheel scrolled it: {:?}", rail(&s));
    at(&mut s, MouseKind::ScrollUp, 4, before[0].0);
    assert_eq!(rail(&s), before);
    // `↓ N more`: a page down; then `↑ N more`: back
    let (down, by) = *more(&s).iter().find(|m| m.1 > 0).expect("a ↓ row");
    at(&mut s, MouseKind::Down, 4, down);
    assert_eq!(s.ed.session.code_review.as_ref().unwrap().scroll, by as usize, "a page");
    assert_ne!(rail(&s), before);
    let (up, _) = *more(&s).iter().find(|m| m.1 < 0).expect("an ↑ row");
    at(&mut s, MouseKind::Down, 4, up);
    assert_eq!(rail(&s), before);
    // A click on a file's mark: opened and viewed, a second click right after keeps it so
    let (y, i, x) = s.ed.screen.review_rail_marks.borrow().iter().copied().rfind(|m| m.1 != 0).unwrap();
    at(&mut s, MouseKind::Down, x, y);
    at(&mut s, MouseKind::Down, x, y);
    let path = dir.join(format!("src/m{}/f{i:02}.txt", i / 5));
    assert_eq!(s.ed.doc().path.as_deref(), Some(path.as_path()), "opened");
    s.until("its base", |ed| ed.doc().review_base.is_some());
    let viewed = |s: &Shot| {
        let r = s.ed.session.code_review.as_ref().unwrap();
        r.files.iter().filter(|f| r.viewed(&s.ed, f)).count()
    };
    assert_eq!(viewed(&s), 1, "marked once, not toggled back");
    // A click on its name opens it, nothing more; `space v m` takes the mark back
    s.ed.session.chat.as_mut().unwrap().focused = false;
    s.keys(" vm");
    assert_eq!(viewed(&s), 0, "space v m toggles");
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
/// its row marked; folded folders say how many files they hold; the project's name at the right.
#[test]
fn snapshot_side_panel_files() {
    let mut s = Shot::new(112, 24);
    let root = s.in_project();
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
    let mut sb = crate::sidebar::Sidebar::with_files(root, rels.iter().map(|r| r.to_string()).collect());
    sb.focus = false;
    s.ed.session.sidebar = Some(sb);
    s.keys("j");
    s.check("side_panel_files");
}

/// The outline tab, focused: definitions with their kind glyphs, the selection on the accent bar.
#[test]
fn snapshot_side_panel_outline() {
    let mut s = Shot::new(112, 24);
    s.file("src/demo.rs", DEMO_RS);
    s.ed.sidebar_show(crate::sidebar::Mode::Outline);
    s.until("outline", |ed| ed.session.sidebar.as_ref().is_some_and(|sb| !sb.lines().is_empty()));
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
    s.ed.session.git_status = Some(st.with_rows());
    s.ed.session.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, cwd));
    s.keys("j");
    s.check("side_panel_git");
}

/// #139: the commit draft's card (git tab `C` — Claude writing, streamed) stands in the code's room: right
/// of the side panel or the review's rail, above the bottom slot, its text whole — placed by the screen's
/// edge, it ran into a side panel widened by hand (`C-w >`), and the task's toast covered its end.
#[test]
fn the_draft_card_stays_in_the_codes_room() {
    use crate::codereview::{Base, CodeReview, File};
    // (screen, a review's rail or the side panel kept this wide)
    for (w, h, side) in [(160, 30, None), (184, 30, None), (140, 30, Some(72)), (150, 26, Some(64))] {
        let mut s = Shot::new(w, h);
        let doc = s.file("src/demo.rs", DEMO_RS);
        let cwd = std::env::current_dir().unwrap();
        let file = |rel: &str| File {
            rel: rel.into(),
            path: cwd.join(rel),
            added: 1,
            removed: 0,
            new: false,
            gone: false,
            binary: false,
        };
        match side {
            None => {
                let files =
                    vec![file("src/demo.rs"), file("src/a_rather_long_name_for_a_module_of_the_review.rs")];
                s.ed.session.code_review = Some(CodeReview::for_test(Base::Head, cwd.clone(), files));
            }
            Some(k) => {
                s.ed.sizes.side = Some(k);
                s.in_project();
                s.keys("<space>e");
                s.keys("<C-w>l");
            }
        }
        s.ed.task_start(crate::task::Task { name: "check".into(), cmd: "echo built".into(), cwd });
        s.until("task", |ed| ed.session.task_run.as_ref().is_some_and(|r| r.took.is_some()));
        // (a claude that hears the prompt and says nothing yet: the card says it's thinking — the chat open
        // beside the code, as in #139)
        s.ed.config.llm.command = "sh".into();
        s.ed.config.llm.args = vec!["-c".into(), "read -r line; sleep 5".into()];
        if side.is_none() {
            crate::chat::open(&mut s.ed);
            s.ed.session.chat.as_mut().unwrap().focused = false;
        }
        crate::llm::draft(&mut s.ed, doc, "a commit message", "draft it", 0, 0).unwrap();
        let mut vt = vt100::Parser::new(h, w, 0);
        vt.process(&s.frame());
        let rows: Vec<String> = vt.screen().rows(0, w).collect();
        let (y, row) = rows
            .iter()
            .enumerate()
            .find(|(_, r)| r.contains("claude thinking"))
            .unwrap_or_else(|| panic!("{w}×{h}: no card\n{}", rows.join("\n")));
        let x =
            row.find("claude thinking").map(|b| unicode_width::UnicodeWidthStr::width(&row[..b])).unwrap();
        let sc = &s.ed.screen;
        let all = rows.join("\n");
        // (the card's left edge: two cells of margin and the wave before the words)
        let (left, edge) = (sc.rail_w.max(sc.side_w), x - 6);
        assert!(left > 0 && sc.bottom_h > 0, "{w}×{h}: the panel and the slot are up\n{all}");
        assert!(edge > left, "{w}×{h}: right of the panel ({left}) — at {edge}\n{all}");
        assert!(y + 2 <= (h as usize) - 1 - sc.bottom_h, "{w}×{h}: above the slot — at row {y}\n{all}");
        // (the task's toast keeps off it: its instruction and its time are whole)
        assert!(row.contains("“a commit message”") && row.trim_end().ends_with('s'), "{w}×{h}: {row}\n{all}");
        assert!(!all.lines().nth(y).unwrap().contains("check — all good"), "{w}×{h}\n{all}");
        crate::llm::cancel(&mut s.ed);
    }
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
    s.until("task", |ed| ed.session.task_run.as_ref().is_some_and(|r| r.took.is_some()));
    s.ed.session.task_run.as_mut().unwrap().took = Some(Duration::from_millis(1830));
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
    let mut s = Shot::new(100, 30);
    blocked_npm_run(&mut s);
    // (the run's sandbox, as `tools::go_now` keeps it: the list's state line — 9e)
    s.ed.session.sandbox = Some(crate::sandbox::guarantees::Guarantees::seatbelt(false, false));
    let dump = |s: &mut Shot, name: &str| {
        if let Some(dir) = std::env::var_os("TARAE_FRAMES_DIR") {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(Path::new(&dir).join(format!("{name}.bin")), s.frame()).unwrap();
        }
    };
    s.check("sandbox_blocked_panel");
    dump(&mut s, "sandbox_blocked_panel");
    s.keys(" ms");
    s.until("the list", |ed| ed.picker.is_some());
    s.check("sandbox_blocked_list");
    dump(&mut s, "sandbox_blocked_list");
    s.frames_both("sandbox_blocked_list");
    s.keys("<ret>");
    s.check("sandbox_blocked_card");
    dump(&mut s, "sandbox_blocked_card");
    s.frames_both("sandbox_blocked_card");
}

/// 9f: what was blocked, then the AppArmor profile's step — its row last, after running it outside; the header's
/// hint over them, no note of nothing blocked. Keys: Enter on the step's row copies the whole step and says so; on
/// a blocked row it still puts up that grant's card. At 120 and 60 columns; with `TARAE_FRAMES_DIR`, both themes.
#[cfg(target_os = "macos")]
#[test]
fn snapshot_sandbox_blocked_with_step() {
    for (w, h) in [(120, 30), (60, 24)] {
        let mut s = Shot::new(w, h);
        blocked_npm_run(&mut s);
        s.ed.session.sandbox = Some(host_restricted());
        s.keys(" ms");
        s.until("the list", |ed| ed.picker.is_some());
        let p = s.ed.picker.as_ref().unwrap();
        let labels: Vec<String> = p.items().iter().map(|i| i.label.clone()).collect();
        assert_eq!(
            labels[labels.len() - 2..],
            ["run it outside the sandbox, once", "copy the step for own namespaces"],
            "{labels:?}"
        );
        // (something was blocked: "blocked nothing" is never said over its rows)
        let note = p.head.as_ref().and_then(|h| h.note.clone());
        assert_eq!(note, None, "no note over what was blocked");
        let name = format!("sandbox_blocked_step_{w}");
        s.check(&name);
        s.frames_both(&name);
        // (the step's row, last: Enter copies the step — no card)
        let step = s.ed.session.sandbox.as_ref().and_then(|g| g.hint.as_ref()?.step.clone()).unwrap();
        s.keys(&"<down>".repeat(labels.len() - 1));
        let at = s.ed.picker.as_ref().and_then(|p| p.current()).map(|i| i.label.clone());
        assert_eq!(at.as_deref(), labels.last().map(String::as_str), "on the step's row");
        s.keys("<ret>");
        let said = "copied the step for own namespaces — run it in a terminal, then restart tarae";
        s.until("the copy", |ed| ed.toasts.iter().any(|t| t.text == said));
        assert!(crate::clipboard::written(&step) && s.ed.offers.is_empty(), "{step}");
        // (and the blocked rows above it still act: Enter on the first puts its grant's card up)
        s.keys(" ms");
        s.until("the list", |ed| ed.picker.is_some());
        s.keys("<ret>");
        let titles: Vec<&str> = s.ed.offers.iter().map(|o| o.title.as_str()).collect();
        assert_eq!(titles, [crate::blocked::GRANT_TITLE], "the first row's card");
    }
}

/// A failed `npm test` of Claude's the sandbox blocked a write in npm's cache and a port in, as its panel has it
/// (macOS: the fixed folder is in `/private/tmp` so the paths read the same every run).
#[cfg(target_os = "macos")]
/// cardauto 7: `:granted` — the defaults first, each saying why, then the answers under how long they last, newest
/// first, with when — and the chat title's faint `› N granted` (`granted_chip`). Full, narrow, nothing past the
/// defaults; both themes with `TARAE_FRAMES_DIR` (`frames_both`). The list is made from a `Caps` given here, not
/// found (the toolchains found are this machine's); macOS only, as `snapshot_sandbox_blocked`: its places are in
/// `/private/tmp` (none under home — `~` is this machine's), so they read the same every run.
#[cfg(target_os = "macos")]
#[test]
fn snapshot_granted() {
    use crate::allow::{Allowed, Grant};
    use crate::caps::{Caps, Entry, Source, Why};
    let at = |p: &str| PathBuf::from(format!("/private/tmp/tarae-shot-granted/{p}"));
    let now = 1_792_000_000;
    let answer = |a: &mut Allowed, g: Grant, ago: u64| {
        a.add(&g);
        a.granted.insert(g, now - ago);
    };
    let (mut kept, mut session, mut chat) = (Allowed::default(), Allowed::default(), Allowed::default());
    answer(&mut kept, Grant::Write(at("npm-cache")), 3 * 86_400);
    answer(&mut kept, Grant::Host("registry.npmjs.org".into(), 443), 5 * 3600);
    answer(&mut session, Grant::Port(5432), 2 * 3600);
    answer(&mut chat, Grant::Read(at("notes")), 12 * 60);
    answer(&mut chat, Grant::Terminals, 20);
    let hosts = vec![("db.example.com".to_string(), 5432)];
    let caps = |answers: [&Allowed; 3]| {
        let mut c = Caps::granted(&at("tarae"), answers[0], answers[1], answers[2]);
        let d = |what: PathBuf, why| Entry { what, source: Source::Default(why) };
        c.read.extend([
            d(at("tarae-review"), Why::OtherCheckout),
            d("/private/tmp".into(), Why::Temp),
            d(at("rustup/toolchains/1.99.0/lib/rustlib/src"), Why::Toolchain),
            d(at("cargo/registry/src"), Why::Toolchain),
        ]);
        c.outside = true;
        c
    };
    let scene = |w: u16, h: u16, answers: [&Allowed; 3], hosts: &[(String, u16)]| {
        let mut s = Shot::new(w, h);
        s.file("src/demo.rs", DEMO_RS);
        let mut c = crate::chat::Chat::new();
        c.focused = false;
        s.ed.session.chat = Some(c);
        s.ed.session.caps = Some((1, std::sync::Arc::new(caps(answers))));
        s.ed.session.hosts = hosts.to_vec();
        s
    };
    // Piece E: the trust answers, a plugin, the preflight told, the no-sandbox `s`, other checkouts' count
    let trusted = |kind, file: &str, ago: u64, versions| crate::trust::Trusted {
        kind,
        place: crate::trust::Place::Repo { common: at("tarae/.git"), file: file.into() },
        at: now - ago,
        versions,
    };
    use crate::trust::Kind;
    let more = crate::granted::More {
        no_sandbox: true,
        repository: true,
        trusted: vec![
            trusted(Kind::Tarae, ".tarae.toml", 2 * 86_400, 2),
            trusted(Kind::Claude, ".claude/settings.json", 86_400, 1),
            trusted(Kind::Git, ".git/config", 5 * 3600, 1),
            trusted(Kind::Hook, ".githooks/pre-commit", 3600, 1),
            trusted(Kind::Hook, ".githooks/pre-push", 3600, 1),
        ],
        plugins: vec![("k8s".into(), "run kubectl, read ~/.kube/config, reach 2 hosts".into())],
        preflight: vec!["0".repeat(64)],
        elsewhere: 4,
    };
    let full = [&kept, &session, &chat];
    let mut s = scene(100, 30, full, &hosts);
    s.check("granted_chip");
    s.frames_both("granted_chip");
    s.ed.granted_open(crate::granted::items(&caps(full), full, &hosts, &more, now), false);
    s.check("granted_full");
    s.frames_both("granted_full");
    let mut s = scene(100, 50, full, &hosts);
    s.ed.granted_open(crate::granted::items(&caps(full), full, &hosts, &more, now), false);
    s.keys("G");
    s.check("granted_full_end");
    s.frames_both("granted_full_end");
    let mut s = scene(60, 24, full, &hosts);
    s.ed.granted_open(crate::granted::items(&caps(full), full, &hosts, &more, now), false);
    s.check("granted_narrow");
    s.frames_both("granted_narrow");
    s.keys("G");
    s.check("granted_narrow_end");
    s.frames_both("granted_narrow_end");
    let none = Allowed::default();
    let mut s = scene(100, 30, [&none; 3], &[]);
    let nothing = crate::granted::More::default();
    s.ed.granted_open(crate::granted::items(&caps([&none; 3]), [&none; 3], &[], &nothing, now), false);
    s.check("granted_empty");
    s.frames_both("granted_empty");
}

#[cfg(target_os = "macos")]
fn blocked_npm_run(s: &mut Shot) {
    let cache = PathBuf::from("/private/tmp/tarae-shot-blocked/npm-cache");
    std::fs::create_dir_all(&cache).unwrap();
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
    r.guard = Some(crate::sandbox::Guard::In(Box::new(policy)));
    r.output = vec![(true, "npm ERR! code EPERM".into()), (true, "npm ERR! syscall open".into())];
    s.ed.session.task_run = Some(r);
}

/// Tier offline in a container under Docker's default profile (no user namespaces, `pidfd_getfd` refused, Landlock
/// ABI 4): the fallback's why, and how to get the calls served.
fn offline_docker_default() -> crate::sandbox::guarantees::Guarantees {
    use crate::sandbox::guarantees::{Found, Guarantees, Ns, PIDFD_REFUSED};
    Guarantees::landlock(Found {
        ns: Ns::NONE,
        served: Err(PIDFD_REFUSED.into()),
        abi: 4,
        ptys: None,
        lo_missing: Vec::new(),
        short: Some(
            "a seccomp filter tarae runs under refuses new namespaces — Docker's default profile does".into(),
        ),
        brief: Some("a seccomp filter blocks namespaces".into()),
        hint: None,
        network: false,
    })
}

/// Tier host on Ubuntu with its restriction on and no profile on tarae (24.04, ABI 4; served): the profile's hint (9f).
fn host_restricted() -> crate::sandbox::guarantees::Guarantees {
    use crate::sandbox::guarantees::{Found, Guarantees, Hint, Ns, Tier};
    let place = crate::sandbox::apparmor::Place {
        path: crate::sandbox::apparmor::PACKAGED.into(),
        root_owned: Ok(()),
        packaged: true,
    };
    Guarantees::landlock(Found {
        ns: Ns::NONE,
        served: Ok(()),
        abi: 4,
        ptys: Some(4),
        lo_missing: Vec::new(),
        short: Some("no user namespaces: Ubuntu's restriction".into()),
        brief: Some("AppArmor blocks user namespaces".into()),
        hint: Some(Hint::of(Tier::Host, &place)),
        network: false,
    })
}

/// 9e: the sandbox panel's state line (`space m s`) — the session's last run's sandbox as key hints over the list,
/// a fallback's why faint under it, and (9f) where tarae's AppArmor profile would give commands namespaces of their own, that and where its
/// step is, on rows of its own; at 60 columns the least important parts folded (`Guarantees::FOLD`); before any
/// sandboxed run, said so in its place. Nothing blocked: the list says so where its first row would be. With
/// `TARAE_FRAMES_DIR` set, each in both themes (`target/sb/shots-9e.sh` draws them).
#[test]
fn snapshot_sandbox_panel_tier() {
    use crate::sandbox::guarantees::{Found, Guarantees, Ns};
    let top = Guarantees::landlock(Found {
        ns: Ns::session(true, true),
        served: Ok(()),
        abi: 8,
        ptys: None,
        lo_missing: Vec::new(),
        short: None,
        brief: None,
        hint: None,
        network: true,
    });
    for (w, h) in [(120, 30), (60, 24)] {
        let scenes = [
            ("fallback", Some(offline_docker_default())),
            ("hint", Some(host_restricted())),
            ("top", Some(top.clone())),
            ("empty", None),
        ];
        for (name, g) in scenes {
            let mut s = Shot::new(w, h);
            s.file("src/demo.rs", DEMO_RS);
            s.ed.session.sandbox = g;
            s.keys(" ms");
            assert!(
                s.ed.picker.as_ref().is_some_and(|p| p.kind == "blocked"),
                "nothing blocked opens the panel too"
            );
            let name = format!("sandbox_panel_tier_{name}_{w}");
            s.check(&name);
            s.frames_both(&name);
        }
    }
}

/// 9e: where the state line doesn't fit, its parts go in `Guarantees::FOLD`'s order — Landlock's ABI, the network, the
/// loopback, then what's shared, which first keeps fewer items whole (`processes, signals +2`), never a word cut; the
/// tier always stays.
#[test]
fn the_state_line_folds_its_least_important_part_first() {
    use crate::sandbox::guarantees::Guarantees;
    let pairs = host_restricted().panel_pairs();
    let keys = |w: usize| -> Vec<&str> {
        super::fold_hints(&pairs, Guarantees::FOLD, w).iter().map(|p| p.0).collect()
    };
    assert_eq!(keys(1000), ["tier", "Landlock", "loopback", "network", "shared"]);
    // (narrower and narrower: the order the parts go in)
    let mut gone: Vec<&str> = Vec::new();
    for w in (0..200).rev() {
        let now = keys(w);
        let went: Vec<&str> =
            pairs.iter().map(|p| p.0).filter(|k| !now.contains(k) && !gone.contains(k)).collect();
        gone.extend(went);
    }
    assert_eq!(gone, ["Landlock", "network", "loopback", "shared"]);
    // (a long part that doesn't fit doesn't take the less important short ones along: the ABI and the network stay
    // without the loopback)
    assert_eq!(keys(100), ["tier", "Landlock", "network", "shared"]);
    // (what's shared keeps fewer items before it goes — each whole — and what's left fits)
    let at_34 = super::fold_hints(&pairs, Guarantees::FOLD, 34);
    let shared = &at_34.iter().find(|p| p.0 == "shared").expect("shared, fewer").1;
    let all = &pairs.iter().find(|p| p.0 == "shared").unwrap().1;
    let (kept, more) = shared.rsplit_once(" +").expect("how many more");
    assert!(kept.split(", ").all(|i| all.split(", ").any(|a| a == i)), "{at_34:?}");
    assert_eq!(kept.split(", ").count() + more.parse::<usize>().unwrap(), all.split(", ").count());
    let wide: usize =
        at_34.iter().map(|(k, m)| k.len() + 1 + m.chars().count()).sum::<usize>() + 2 * (at_34.len() - 1);
    assert!(wide <= 34, "{at_34:?}");
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
        s.ed.session.task_run = Some(r);
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
    s.ed.session.test_run = Some(run);
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

/// A run where no test ran: the header says so (its tab's badge in the warning colour), and the first row
/// under the rule says why above the runner's own "ok".
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
    s.ed.session.test_run = Some(run);
    let mut p = vt100::Parser::new(s.h, s.w, 0);
    p.process(&s.frame());
    let text = p.screen().contents();
    let (y0, _, _) = s.ed.screen.test_panel.get().expect("the panel is drawn");
    let rows: Vec<&str> = text.lines().skip(y0).collect();
    assert!(rows[0].contains("tests ▲") && rows[0].contains("orphan::forgotten  no tests"), "{text}");
    assert!(rows[1].trim().chars().all(|c| c == '─'), "the header's rule: {text}");
    assert!(rows[2].contains("no test ran — nothing matched it"), "{text}");
    assert!(rows[3..].iter().any(|r| r.contains("test result: ok. 0 passed")), "the output under it: {text}");
}

/// The terminal panel under the code with a program's colored output, the keys in it.
#[test]
fn snapshot_terminal_panel() {
    if crate::sandbox::no_pty_here("term::snapshot_tests::snapshot_terminal_panel") {
        return;
    }
    let mut s = Shot::new(100, 24);
    s.file("src/demo.rs", DEMO_RS);
    let script = "printf '\\033[1;32m$\\033[0m cargo test\\r\\nrunning 3 tests\\r\\ntest words ... \\033[32mok\\033[0m\\r\\n\
                  test empty ... \\033[31mFAILED\\033[0m\\r\\n'; sleep 30";
    s.ed.terminal_run(vec!["/bin/sh".into(), "-c".into(), script.into()], Vec::new());
    // All of it — the line break after the last line too (it can come in a read of its own)
    s.until("output", |ed| {
        ed.session.terminal.as_ref().is_some_and(|t| {
            let screen = t.parser.screen();
            screen.contents().contains("FAILED") && screen.cursor_position() == (4, 0)
        })
    });
    s.check("terminal_panel");
    // A drag over its first line (the screen row under the slot's tabs and rule; its text starts at the
    // padding, two columns in): selected on the selection's color, copied on release
    let first = s.ed.screen.terminal_rows.expect("the terminal's rows").0 + crate::chrome::HEAD;
    let y = first as u16;
    let mouse = |s: &mut Shot, kind, x| {
        s.ed.handle_event(Event::Mouse(crate::event::Mouse { kind, x, y, alt: false }));
        s.frame()
    };
    use crate::event::MouseKind as K;
    mouse(&mut s, K::Down, 2);
    mouse(&mut s, K::Drag, 13);
    let bytes = mouse(&mut s, K::Up, 13);
    assert_eq!(s.ed.session.terminal.as_ref().unwrap().selected_text(), "$ cargo test");
    s.until("the copy", |ed| ed.toasts.iter().any(|t| t.text == "copied 12 characters"));
    let shot = snapshot(&s.ed, "t", &bytes, s.w, s.h);
    let bg_row = shot.lines().skip_while(|l| !l.starts_with("## bg")).nth(first + 1).unwrap();
    assert_eq!(&bg_row[4..18], "pVVVVVVVVVVVVp", "the selected cells: {bg_row:?}");
    // A drag begun in the code isn't the terminal's when it crosses it — nor is its cursor moved by it
    let at = |s: &mut Shot, kind, x, y| {
        s.ed.handle_event(Event::Mouse(crate::event::Mouse { kind, x, y, alt: false }));
        s.frame();
    };
    at(&mut s, K::Down, 10, 3);
    assert!(
        s.ed.session.terminal.as_ref().unwrap().selection.is_none()
            && !s.ed.session.terminal.as_ref().unwrap().focus
    );
    at(&mut s, K::Drag, 10, y + 1);
    at(&mut s, K::Up, 10, y + 1);
    assert!(s.ed.session.terminal.as_ref().unwrap().selection.is_none(), "not a terminal selection");
    // A drag begun in it (its second line) goes on outside it (clamped to its edge) and ends there
    at(&mut s, K::Down, 2, y + 1);
    at(&mut s, K::Drag, 31, 3);
    assert_eq!(
        s.ed.session.terminal.as_ref().unwrap().selected(),
        Some(((0, 29), (1, 0))),
        "clamped to its first row"
    );
    at(&mut s, K::Up, 31, 3);
    assert!(!s.ed.session.terminal.as_ref().unwrap().selecting, "released outside — done");
    let cursor = s.ed.doc().selection().primary();
    at(&mut s, K::Drag, 40, 5);
    assert_eq!(
        s.ed.doc().selection().primary(),
        cursor,
        "a stray drag after it doesn't move the code's cursor"
    );
    // A release that never came: the next press anywhere ends that selection
    at(&mut s, K::Down, 2, 17);
    at(&mut s, K::Drag, 7, 17);
    at(&mut s, K::Down, 10, 3);
    assert!(!s.ed.session.terminal.as_ref().unwrap().selecting);
    s.ed.session.terminal = None; // hangs it up
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
    let c = s.ed.session.chat.as_mut().unwrap();
    c.msgs = vec![Msg::new(Role::User, "how?", ""), Msg::new(Role::Assistant, answer, "")];
    c.follow.on = true;
    s.frame();
    // As a turn leaves it: at the bottom (the last place shows), read from here on
    let c = s.ed.session.chat.as_mut().unwrap();
    c.follow.read_at = Some(c.scroll);
    let at = |s: &Shot| {
        (
            s.ed.doc().path.clone().unwrap(),
            s.ed.doc().text.byte_to_line(s.ed.doc().selection().primary().head),
        )
    };
    let cwd = std::env::current_dir().unwrap();
    s.ed.session.chat.as_mut().unwrap().scroll = 1000; // to the top
    s.frame();
    assert_eq!(at(&s), (cwd.join("src/filetree.rs"), 29), "the top of the answer");
    s.ed.session.chat.as_mut().unwrap().scroll = 0;
    s.frame();
    assert_eq!(at(&s), (cwd.join("src/sidebar.rs"), 59), "its end");
}

/// A name too long for the side panel keeps its end there; selected with the keys in the panel, it shows
/// whole — over the code, on its row.
#[test]
fn a_long_name_shows_whole_when_selected() {
    let mut s = Shot::new(112, 16);
    let root = s.in_project();
    s.file("src/demo.rs", DEMO_RS);
    let long = "src/components/user_profile_settings_view_controller.tsx";
    let mut sb = crate::sidebar::Sidebar::with_files(root, vec![long.to_string(), "src/demo.rs".into()]);
    sb.set_open(["src", "src/components"], true);
    sb.focus = false;
    s.ed.session.sidebar = Some(sb);
    let text = |s: &mut Shot| {
        let bytes = s.frame();
        snapshot(&s.ed, "t", &bytes, s.w, s.h)
    };
    let unfocused = text(&mut s);
    assert!(
        unfocused.contains("user_profile…") && unfocused.contains("ler.tsx"),
        "the end stays: {unfocused}"
    );
    let row =
        s.ed.session
            .sidebar
            .as_ref()
            .unwrap()
            .lines()
            .iter()
            .position(|l| l.label.ends_with(".tsx"))
            .unwrap();
    let sb = s.ed.session.sidebar.as_mut().unwrap();
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
    let mut frames = vec![s.drawn()];
    let mut step = |s: &mut Shot, keys: &str| {
        s.keys(keys);
        frames.push(s.drawn());
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
    let (base, root) = demo_copy("long-title");
    s.ed.session.root = base;
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

// ── The panels' chrome (`chrome.rs`) ─────────────────────────────────────

/// A frame as cells.
fn cells(bytes: &[u8], w: u16, h: u16) -> vt100::Parser {
    let mut p = vt100::Parser::new(h, w, 0);
    p.process(bytes);
    p
}

/// What the colors are: the ground, the panels' surface, the accent.
fn chrome_colors(ed: &Editor) -> (vt100::Color, vt100::Color, vt100::Color) {
    let ui = Ui::new(ed);
    let surface = card_style(&ui, ed, "ui.popup").bg;
    (vt_color(ui.base.bg).unwrap(), vt_color(surface).unwrap(), vt_color(ui.accent.fg).unwrap())
}

/// The side panel, the code with a task under it, the chat — one set of rules: every title on the screen's
/// first row with a thin rule under it, text `PAD` in from each panel's edge, the code's numbers (and its
/// path bar) right after its sign column, half a cell of ground between regions — the gutter's half block,
/// the panel's half in its own colour, faded or not — the same everywhere, and the rule of what has the keys
/// in the accent, following them from the code to the chat, the side panel and the bottom slot, the status
/// line naming each; what hasn't them keeps `FADE` of its text's contrast.
#[test]
fn the_panels_follow_one_set_of_rules() {
    use crate::chrome::{GAP, HEAD, PAD, PAD_R, SIGNS};
    let (w, h) = (150u16, 36u16);
    let mut s = Shot::new(w, h);
    let cwd = s.in_project();
    s.file("src/demo.rs", DEMO_RS);
    let mut sb =
        crate::sidebar::Sidebar::with_files(cwd.clone(), vec!["src/demo.rs".into(), "README.md".into()]);
    sb.focus = false;
    s.ed.session.sidebar = Some(sb);
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" l<esc>");
    s.ed.task_start(crate::task::Task { name: "check".into(), cmd: "echo all good".into(), cwd });
    s.until("task", |ed| ed.session.task_run.as_ref().is_some_and(|r| r.took.is_some()));
    s.ed.toasts.clear();
    let (ground, surface, accent) = chrome_colors(&s.ed);
    let p = cells(&s.frame(), w, h);
    let at = |y: usize, x: usize| p.screen().cell(y as u16, x as u16).unwrap().clone();
    let blank = |y: usize, x: usize| at(y, x).contents().trim().is_empty();
    let sc = &s.ed.screen;
    let (side, chat, slot_x) = (sc.side_w, sc.chat_x.unwrap(), sc.slot_x);
    let end = sc.text_top + sc.body_h;
    let slot_y = end - sc.bottom_h;
    assert!(side > 0 && sc.bottom_h > 0 && slot_x == side, "the slot under the code only");
    // Half a cell of ground between regions, the whole height: a gutter's left half is what's left of it —
    // a panel's surface (unfaded, though neither panel has the keys), or the ground beside the code — its
    // right half ground, what's right of it from the next cell
    let half = |y: usize, x: usize| {
        let c = at(y, x);
        (c.contents().to_string(), c.fgcolor(), c.bgcolor())
    };
    for y in 0..end {
        assert_eq!(half(y, side - GAP), ("▌".into(), surface, ground), "row {y}: the side panel's gutter");
        let chat_gutter = half(y, chat);
        match y < slot_y {
            true => assert_eq!((chat_gutter.0, chat_gutter.2), (" ".into(), ground), "row {y}: by the code"),
            false => assert_eq!(chat_gutter, ("▌".into(), surface, ground), "row {y}: by the slot"),
        }
        assert_eq!(at(y, side - GAP - 1).bgcolor(), surface, "row {y}: the side panel up to its gutter");
        assert_eq!(at(y, chat + GAP).bgcolor(), surface, "row {y}: the chat from its gutter");
    }
    for y in slot_y..end {
        assert_eq!(at(y, slot_x).bgcolor(), surface, "row {y}: the slot right after the side's gutter");
        assert_eq!(at(y, chat - 1).bgcolor(), surface, "row {y}: …up to the chat's");
    }
    // Every title on the first row, its text at the padding; the slot's tabs too
    let first = |y: usize, a: usize, b: usize| (a..b).find(|&x| !blank(y, x));
    assert_eq!(first(0, 0, side), Some(PAD), "the side panel's title");
    assert_eq!(first(0, side, chat), Some(side + SIGNS), "the code's path bar, where its numbers start");
    assert_eq!(first(0, chat + GAP, w as usize), Some(chat + GAP + PAD), "the chat's title");
    assert_eq!(first(slot_y, slot_x, chat), Some(slot_x + PAD), "the slot's tabs");
    // …a thin rule under each, from padding to padding (the code's from its numbers)
    let regions = [
        (1, 0, side - GAP, PAD),
        (1, side, chat, SIGNS),
        (1, chat + GAP, w as usize, PAD),
        (slot_y + 1, slot_x, chat, PAD),
    ];
    for (y, a, b, pad) in regions {
        let line: String = (a + pad..b - PAD_R).map(|x| at(y, x).contents().to_string()).collect();
        assert!(line.chars().all(|c| c == '─'), "row {y} from {a}: {line:?}");
        assert!(blank(y, a + pad - 1) && blank(y, b - 1), "row {y} from {a}: inside the padding");
    }
    // The bodies' text: the code's numbers right after its sign column (two-digit ones there, none before),
    // the chat's words and the task's output at the padding
    assert_eq!(sc.text_top, HEAD);
    let starts: Vec<usize> = (HEAD..slot_y).filter_map(|y| first(y, side, chat)).collect();
    assert_eq!(starts.iter().min(), Some(&(side + SIGNS)), "the numbers right after the sign column");
    let row_with = |t: &str| (0..end).find(|&y| p.screen().rows(0, w).nth(y).is_some_and(|r| r.contains(t)));
    let ask = row_with("Ask Claude").expect("the chat's empty state");
    assert_eq!(first(ask, chat + GAP, w as usize), Some(chat + GAP + PAD));
    assert_eq!(first(slot_y + HEAD, slot_x, chat), Some(slot_x + PAD), "the task's output");
    // What has the keys: its rule in the accent, the status line's label naming it
    let rules = [(1, PAD), (1, side + SIGNS), (1, chat + GAP + PAD), (slot_y + 1, slot_x + PAD)];
    let keys = |s: &mut Shot| -> (Vec<bool>, String) {
        let p = cells(&s.frame(), w, h);
        let lit =
            rules.iter().map(|&(y, x)| p.screen().cell(y as u16, x as u16).unwrap().fgcolor() == accent);
        let status = p.screen().rows(0, w).nth(end).unwrap_or_default();
        (lit.collect(), status.split('▌').next().unwrap_or_default().trim().to_string())
    };
    assert_eq!(keys(&mut s), (vec![false, true, false, false], "▐ NORMAL".into()), "the code");
    s.keys(" l");
    assert_eq!(keys(&mut s), (vec![false, false, true, false], "▐ CHAT".into()), "the chat");
    s.keys("<esc><C-w>e");
    assert_eq!(keys(&mut s), (vec![true, false, false, false], "▐ FILES".into()), "the side panel");
    s.keys("<esc><C-w>j");
    assert_eq!(keys(&mut s), (vec![false, false, false, true], "▐ TASK".into()), "the bottom slot");
    // …and what hasn't them, faded: the chat's title, the code's file name (the slot has the keys)
    let ui = Ui::new(&s.ed);
    let p = cells(&s.frame(), w, h);
    let fg = |y: usize, x: usize| match p.screen().cell(y as u16, x as u16).unwrap().fgcolor() {
        vt100::Color::Rgb(r, g, b) => [r, g, b],
        c => panic!("{c:?}"),
    };
    let rgb = |c: Option<crossterm::style::Color>| c.and_then(crate::chrome::rgb).unwrap();
    let (ground_rgb, surface_rgb) = (rgb(ui.base.bg), rgb(card_style(&ui, &s.ed, "ui.popup").bg));
    let dim = rgb(ui.virt.fg);
    let fade = |c, bg| crate::chrome::fade(c, bg, crate::chrome::FADE);
    assert_eq!(fg(0, chat + GAP + PAD), fade(dim, surface_rgb), "the chat's title, faded");
    let name = p.screen().rows(0, w).next().unwrap().find("demo.rs").unwrap();
    let focus = rgb(s.ed.theme.try_get("ui.text.focus").and_then(|s| s.fg));
    assert_eq!(fg(0, name), fade(focus, ground_rgb), "the code's file name, faded");
    assert_ne!(fade(focus, ground_rgb), focus);
}

/// Every panel's text starts the same distance in from its visible edge — `PAD` cells from where its
/// surface shows: the side panel's (from the screen's edge), the bottom slot's (past the side panel's
/// gutter) and the chat's, beside the code and beside the slot alike. Measured in half cells: a half block
/// shows two colours.
#[test]
fn every_panel_starts_its_text_the_same_distance_in() {
    use crate::chrome::{GAP, PAD};
    let (w, h) = (150u16, 36u16);
    let mut s = Shot::new(w, h);
    let cwd = s.in_project();
    s.file("src/demo.rs", DEMO_RS);
    let rels = vec!["src/demo.rs".into(), "README.md".into()];
    let mut sb = crate::sidebar::Sidebar::with_files(cwd.clone(), rels);
    sb.focus = false;
    s.ed.session.sidebar = Some(sb);
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" l<esc>");
    s.ed.task_start(crate::task::Task { name: "check".into(), cmd: "echo all good".into(), cwd });
    s.until("task", |ed| ed.session.task_run.as_ref().is_some_and(|r| r.took.is_some()));
    s.ed.toasts.clear();
    let (_, surface, _) = chrome_colors(&s.ed);
    let p = cells(&s.frame(), w, h);
    let sc = &s.ed.screen;
    let (side, chat, slot_y, last) =
        (sc.side_w, sc.chat_x.unwrap(), sc.text_top + sc.body_h - sc.bottom_h, sc.text_top + sc.body_h - 1);
    assert!(slot_y < last, "the chat's last row is beside the slot");
    // A half cell's colour: half blocks split their cell
    let surface_at = |y: usize, half: usize| {
        let c = p.screen().cell(y as u16, (half / 2) as u16).unwrap();
        let (left, right) = match c.contents() {
            "▌" => (c.fgcolor(), c.bgcolor()),
            "▐" => (c.bgcolor(), c.fgcolor()),
            _ => (c.bgcolor(), c.bgcolor()),
        };
        (if half.is_multiple_of(2) { left } else { right }) == surface
    };
    // From half cell `from` on: where the panel's surface shows, then its first text → the inset, in halves
    let inset = |y: usize, from: usize| {
        let edge = (from..2 * w as usize).find(|&k| surface_at(y, k)).expect("a surface");
        let text = (edge.div_ceil(2)..w as usize)
            .find(|&x| {
                let t = p.screen().cell(y as u16, x as u16).unwrap().contents();
                !t.trim().is_empty() && t != "▌" && t != "▐"
            })
            .expect("text");
        2 * text - edge
    };
    // (the slot's and the chat's from their gutter's right half — its left half is what's left of it)
    let insets = [
        ("the side panel", inset(0, 0)),
        ("the slot", inset(slot_y, 2 * (side - GAP) + 1)),
        ("the chat by the code", inset(0, 2 * chat + 1)),
        ("the chat by the slot", inset(last, 2 * chat + 1)),
    ];
    for (what, halves) in insets {
        assert_eq!(halves, 2 * PAD, "{what}: its text {halves} half cells in");
    }
    // A picker over the editing area hides the slot: no sliver of it left in the chat's gutter
    let ground = chrome_colors(&s.ed).0;
    s.keys(" f");
    let p = cells(&s.frame(), w, h);
    let c = p.screen().cell(last as u16, chat as u16).unwrap();
    assert_eq!((c.contents(), c.bgcolor()), (" ", ground), "the chat's gutter beside the picker");
}

/// The gutters at their edges: the code's sign column is the first cell after the side panel's gutter; a
/// border held shows the panel's half in the accent; a card's edge over a gutter is seated on the panel's
/// half (its colour — not the ground, which would bite into the panel's edge); a panel filling the screen
/// takes its gutter's cell whole (no ground against nothing); a theme whose panels are the ground's colour
/// gets a `│` there instead, and nothing at the screen's edge.
#[test]
fn the_gutters_hold_at_their_edges() {
    use crate::sizes::Border;
    let (w, h) = (120u16, 30u16);
    let mut s = Shot::new(w, h);
    let cwd = s.in_project();
    s.file("src/demo.rs", DEMO_RS);
    s.ed.doc_mut().lsp.diagnostics = vec![crate::lsp::Diagnostic {
        from: 0,
        to: 3,
        severity: 1,
        message: "unresolved import".into(),
        raw: json!({}),
        source: 0,
    }];
    let rels = ["Cargo.toml", "README.md", "src/demo.rs"].map(String::from).to_vec();
    let mut sb = crate::sidebar::Sidebar::with_files(cwd, rels);
    sb.focus = false;
    s.ed.session.sidebar = Some(sb);
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" l<esc>");
    let (ground, surface, accent) = chrome_colors(&s.ed);
    let (side, chat, top, end) =
        (s.ed.screen.side_w, s.ed.screen.chat_x.unwrap(), s.ed.screen.text_top, s.ed.screen.body_h);
    let cell = |s: &mut Shot, y: usize, x: usize| {
        let c = cells(&s.frame(), w, h).screen().cell(y as u16, x as u16).unwrap().clone();
        (c.contents().to_string(), c.fgcolor(), c.bgcolor())
    };
    // The sign column right after the gutter: the error's bar there, nothing between
    assert_eq!(cell(&mut s, top, side).0, "▎", "the sign column, first after the gutter");
    // A border held: the panel's half in the accent
    s.ed.dragging = Some(Border::Side);
    assert_eq!(cell(&mut s, 5, side - 1), ("▌".into(), accent, ground), "the side panel's border, held");
    s.ed.dragging = Some(Border::Chat);
    assert_eq!(cell(&mut s, 5, chat), ("▐".into(), accent, ground), "the chat's border, held");
    s.ed.dragging = None;
    // A card over the side panel and its gutter — a completion list wider than the code beside the chat,
    // pushed left over them: its edges there seated on the panel's half
    let item =
        |label: &str| json!({ "label": label, "detail": "fn(&str) -> Result<HashMap<String, usize>>" });
    let items =
        json!([item("a_name_long_enough_to_fill_its_column_wide"), item("another_name_as_long_as_that")]);
    let mut list = crate::completion::Completion::new(0, s.ed.doc().id, 0, items);
    list.filter("");
    s.ed.completion = Some(list);
    let edges: Vec<_> =
        (0..end).map(|y| cell(&mut s, y, side - 1)).filter(|c| c.0 == "▄" || c.0 == "▀").collect();
    assert!(!edges.is_empty(), "the list's edge crosses the gutter");
    for c in edges {
        assert_eq!(c.2, surface, "a card's edge over the gutter: {c:?}");
    }
    s.ed.completion = None;
    // The chat filling the screen: its gutter's cell whole, the screen's edge no ground
    s.keys(" l<C-w>z");
    for y in 0..end {
        let (glyph, _, bg) = cell(&mut s, y, 0);
        assert_eq!((glyph, bg), (" ".into(), surface), "row {y}, zoomed");
    }
    s.keys("<C-w>z<esc>");
    // Panels the ground's colour: a `│` between regions, nothing at the screen's edge
    let bg = s.ed.theme.get("ui.background").bg;
    let popup = s.ed.theme.get("ui.popup");
    s.ed.theme.set("ui.popup", crate::theme::Style { bg, ..popup });
    for y in [0, 5, end - 1] {
        assert_eq!(cell(&mut s, y, side - 1).0, "│", "row {y}: the side panel's gutter, no surfaces");
        assert_eq!(cell(&mut s, y, chat).0, "│", "row {y}: the chat's gutter, no surfaces");
    }
    s.keys(" l<C-w>z");
    assert_eq!(cell(&mut s, 5, 0).0, " ", "zoomed, no surfaces: nothing at the screen's edge");
}

/// The chat's input says whether it has the keys: its border in the accent and the caret in it while it
/// does; without them a message half-written is a draft, said at the border's end, the caret elsewhere.
#[test]
fn the_chat_input_says_where_the_keys_are() {
    let (w, h) = (120u16, 30u16);
    let mut s = Shot::new(w, h);
    s.file("src/demo.rs", DEMO_RS);
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" lhalf a thought");
    let (_, _, accent) = chrome_colors(&s.ed);
    let chat = s.ed.screen.chat_x.unwrap();
    let look = |s: &mut Shot| {
        let p = cells(&s.frame(), w, h);
        let rows: Vec<String> = p.screen().rows(0, w).collect();
        let input = rows.iter().position(|r| r.contains("› half a thought")).expect("the input");
        let border = input - 1;
        let at = |x: usize| p.screen().cell(border as u16, x as u16).unwrap().clone();
        let lit = at(chat + 3).fgcolor() == accent;
        let caret = (!p.screen().hide_cursor()).then(|| p.screen().cursor_position());
        (rows[border].clone(), lit, caret, input)
    };
    let (border, lit, caret, input) = look(&mut s);
    assert!(lit && !border.contains("draft"), "with the keys: {border}");
    assert_eq!(caret.map(|c| c.0 as usize), Some(input), "the caret in it");
    s.keys("<esc>");
    let (border, lit, caret, input) = look(&mut s);
    assert!(!lit && border.trim_end().ends_with("draft ──"), "without them: {border}");
    assert_ne!(caret.map(|c| c.0 as usize), Some(input), "the caret back in the code");
}

/// The picker floats over the editor area — the chat beside it stays whole — with the same margin all round
/// (`PICKER_MX` columns, `PICKER_MY` rows: they look alike) and only the ground in it.
#[test]
fn the_picker_floats_with_equal_margins() {
    use crate::chrome::{PICKER_MX, PICKER_MY};
    let (w, h) = (150u16, 36u16);
    let mut s = Shot::new(w, h);
    s.file("src/demo.rs", DEMO_RS);
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" l<esc>");
    let (_, root) = demo_copy("floats");
    s.ed.open_picker(
        crate::picker::Picker::new("files", Vec::new(), true).with_tree(root.clone(), None),
        Some(Box::new(move || Ok(crate::picker::file_items(&root)))),
    );
    s.until("file list", |ed| ed.picker.as_ref().is_some_and(|p| !p.loading));
    let (ground, surface, _) = chrome_colors(&s.ed);
    let p = cells(&s.frame(), w, h);
    let at = |y: usize, x: usize| p.screen().cell(y as u16, x as u16).unwrap().clone();
    let chat = s.ed.screen.chat_x.unwrap();
    let end = s.ed.screen.text_top + s.ed.screen.body_h;
    // The card: its surface and its half-row edges
    let card = |y, x| {
        let c = at(y, x);
        c.bgcolor() == surface || (["▄", "▀"].contains(&c.contents()) && c.fgcolor() == surface)
    };
    let rows: Vec<usize> = (0..end).filter(|&y| (0..chat).any(|x| card(y, x))).collect();
    let cols: Vec<usize> = (0..chat).filter(|&x| (0..end).any(|y| card(y, x))).collect();
    let (top, bottom) = (rows[0], end - 1 - rows[rows.len() - 1]);
    let (left, right) = (cols[0], chat - 1 - cols[cols.len() - 1]);
    assert_eq!((left, right, top, bottom), (PICKER_MX, PICKER_MX, PICKER_MY, PICKER_MY));
    for (y, x) in (0..end).flat_map(|y| (0..chat).map(move |x| (y, x))) {
        if !rows.contains(&y) || !cols.contains(&x) {
            assert_eq!(at(y, x).bgcolor(), ground, "the margin at {y},{x}");
        }
    }
    // The chat whole beside it: its title, its rule, its input
    let screen: Vec<String> = p.screen().rows(0, w).collect();
    let beside = |r: &str| r.chars().skip(chat).collect::<String>();
    assert!(beside(&screen[0]).contains("claude") && beside(&screen[1]).contains("────"), "{screen:#?}");
    assert!(screen.iter().any(|r| r.contains("› Ask about demo.rs")), "{screen:#?}");
    assert!(screen[end].contains("▐ PICKER ▌"));
}

/// The bottom slot's panels are tabs on its title row: a task started while the terminal has the keys is a
/// tab — its badge running, then failed — and never takes the slot or the keys; a click on its tab brings it
/// in front (the keys staying in the slot), `C-w t` and `tab` step through them.
#[test]
fn the_bottom_slot_shows_its_panels_as_tabs() {
    use crate::sizes::Slot;
    if crate::sandbox::no_pty_here("term::snapshot_tests::the_bottom_slot_shows_its_panels_as_tabs") {
        return;
    }
    let (w, h) = (120u16, 30u16);
    let mut s = Shot::new(w, h);
    s.file("src/demo.rs", DEMO_RS);
    let script = "printf 'hi\\r\\n'; sleep 30";
    s.ed.terminal_run(vec!["/bin/sh".into(), "-c".into(), script.into()], Vec::new());
    s.until("the terminal", |ed| {
        ed.session.terminal.as_ref().is_some_and(|t| t.parser.screen().contents().contains("hi"))
    });
    s.ed.terminal_focus();
    let typing = |ed: &Editor| ed.session.terminal.as_ref().is_some_and(|t| t.focus);
    assert!(typing(&s.ed));
    // A task started meanwhile (harmless: a pause and a failure) — a tab, running
    let cwd = std::env::temp_dir();
    s.ed.task_start(crate::task::Task { name: "check".into(), cmd: "sleep 0.3; exit 1".into(), cwd });
    let tabs = |s: &mut Shot| {
        let p = cells(&s.frame(), w, h);
        let y = s.ed.screen.slot_tabs.borrow().first().map(|t| t.0).expect("the tabs");
        let row = p.screen().rows(0, w).nth(y).unwrap();
        let spots = s.ed.screen.slot_tabs.borrow().clone();
        (row, spots, p)
    };
    let (row, ..) = tabs(&mut s);
    assert!(
        row.trim_start().starts_with("terminal   check ") && row.chars().any(|c| "▂▃▄▅▆▇".contains(c)),
        "{row}"
    );
    assert_eq!((s.ed.slot(), typing(&s.ed)), (Some(Slot::Terminal), true), "the terminal keeps it all");
    // It fails behind the terminal: its tab says so, in the error colour — nothing else moves
    s.until("the task", |ed| ed.session.task_run.as_ref().is_some_and(|r| r.took.is_some()));
    s.ed.toasts.clear();
    let (row, spots, p) = tabs(&mut s);
    let at = row.find("check ▲").map(|b| row[..b].chars().count() + "check ".len()).expect(&row);
    let y = spots[0].0;
    let error = vt_color(Ui::new(&s.ed).error.fg).unwrap();
    assert_eq!(p.screen().cell(y as u16, at as u16).unwrap().fgcolor(), error, "{row}");
    assert_eq!((s.ed.slot(), typing(&s.ed)), (Some(Slot::Terminal), true), "never takes the keys");
    // A click on its tab: in front, the keys still the slot's (tarae's there — not the shell's)
    let &(y, x, ..) = spots.iter().find(|t| t.3 == Slot::Task).unwrap();
    s.ed.handle_event(Event::Mouse(crate::event::Mouse {
        kind: crate::event::MouseKind::Down,
        x: x as u16,
        y: y as u16,
        alt: false,
    }));
    s.frame();
    assert_eq!((s.ed.slot(), s.ed.bottom_focus, typing(&s.ed)), (Some(Slot::Task), true, false));
    // `C-w t`: the next tab, round to the first; `tab` while the slot has the keys too
    s.keys("<C-w>t");
    assert_eq!(s.ed.slot(), Some(Slot::Terminal));
    s.keys("<tab>");
    assert_eq!(s.ed.slot(), Some(Slot::Task));
    assert!(s.ed.bottom_focus, "the keys stayed in the slot");
    s.ed.session.terminal = None; // hangs it up
}

// ── 글타래 P1: what the scenes showed cut, covered or misplaced ──────────────────────────────────

/// The frame's rows as text, `(x, y, w, h)` of it (cells past the screen's edge aren't asked).
fn rows_of(s: &mut Shot, (x, y, w, h): (usize, usize, usize, usize)) -> Vec<String> {
    let bytes = s.frame();
    let mut parser = vt100::Parser::new(s.h, s.w, 0);
    parser.process(&bytes);
    let end = (x + w).min(s.w as usize) as u16;
    (y..y + h).map(|r| parser.screen().contents_between(r as u16, x as u16, r as u16, end)).collect()
}

/// The follow card's log cut to its newest rows says so on a row of its own (`…`): put in front of a row
/// already as wide as the card, it pushed that row's end out of it (`in the same cal`, `l.` gone).
#[test]
fn a_cut_follow_log_loses_no_words() {
    use crate::chat::{Msg, Role};
    let mut s = Shot::new(112, 30);
    s.file("src/demo.rs", DEMO_RS);
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" lwhy does insert clone the name?<ret>");
    let said = "The name is cloned because insert takes the key by value, and first moves into the map in the \
                same call. Keying by the name alone means a second strand with the same name replaces the \
                first, so the entry API keeps the strand that came first and drops the one that repeats it.";
    let c = s.ed.session.chat.as_mut().unwrap();
    c.follow.on = true;
    c.msgs.push(Msg::new(Role::Assistant, said, ""));
    s.frame();
    let card = s.ed.screen.thought_card.get().expect("the card shows");
    let rows: Vec<String> = rows_of(&mut s, card).iter().map(|r| r.trim().to_string()).collect();
    let from = rows.iter().position(|r| r.starts_with('…')).unwrap_or_else(|| panic!("cut: {rows:#?}"));
    let shown: Vec<&str> = rows[from..]
        .iter()
        .take_while(|r| !r.is_empty() && !r.starts_with('▀'))
        .flat_map(|r| r.trim_start_matches('…').split_whitespace())
        .collect();
    let words: Vec<&str> = said.split_whitespace().collect();
    assert!(!shown.is_empty() && words.ends_with(&shown), "the answer's last words, whole: {rows:#?}");
}

/// A card's title is whole when the code has the room: at 80 columns the run-outside card's was cut at its key
/// word (`outside the sandb…`) — the card takes the width its title needs, up to the code's.
#[test]
fn a_cards_title_is_whole_at_80_columns() {
    let mut s = Shot::new(80, 24);
    s.ed.offer_popups = true;
    s.file("src/demo.rs", DEMO_RS);
    let args =
        json!({"command": "cargo test --quiet", "outside_sandbox": true, "reason": "it needs the registry"});
    let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "run", "arguments": args}});
    assert!(crate::tools::mcp(&mut s.ed, 0, "c1", &msg).is_none(), "it waits for the user");
    let screen = rows_of(&mut s, (0, 0, 80, 24)).join("\n");
    assert!(screen.contains(crate::tools::OUTSIDE_RUN_TITLE), "{screen}");
    // (and with a second card waiting: its count shares the title's row — the card widens for both)
    let msg = json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "run", "arguments": args}});
    assert!(crate::tools::mcp(&mut s.ed, 0, "c2", &msg).is_none(), "it waits for the user");
    let screen = rows_of(&mut s, (0, 0, 80, 24)).join("\n");
    assert!(screen.contains(crate::tools::OUTSIDE_RUN_TITLE) && screen.contains("1/2"), "{screen}");
}

/// Only one row of the follow card's log fits (a short pane): its cut is still said — `…` in front of the
/// newest row's end, the row cut on its left so nothing runs past the card.
#[test]
fn a_one_row_follow_log_still_says_its_cut() {
    use crate::chat::{Msg, Role};
    use unicode_width::UnicodeWidthStr;
    let mut s = Shot::new(112, 14);
    s.file("src/demo.rs", DEMO_RS);
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" lwhy does insert clone the name?<ret>");
    let said = "The name is cloned because insert takes the key by value, and first moves into the map in the \
                same call. Keying by the name alone means a second strand with the same name replaces it.";
    let c = s.ed.session.chat.as_mut().unwrap();
    c.follow.on = true;
    c.msgs.push(Msg::new(Role::Assistant, said, ""));
    s.frame();
    let card = s.ed.screen.thought_card.get().expect("the card shows");
    let rows: Vec<String> = rows_of(&mut s, card).iter().map(|r| r.trim().to_string()).collect();
    let at =
        rows.iter().position(|r| r.starts_with('…')).unwrap_or_else(|| panic!("its cut said: {rows:#?}"));
    let shown: Vec<&str> = rows[at].trim_start_matches('…').split_whitespace().collect();
    let tail = said.split_whitespace().last().unwrap();
    assert!(shown.last() == Some(&tail), "the newest row's end, whole: {rows:#?}");
    let inner = card.2 - 4;
    let text = rows.iter().filter(|r| !r.starts_with(['▄', '▀']));
    assert!(text.clone().all(|r| r.width() <= inner), "nothing past the card: {rows:#?}");
}

/// The path bar never starts or ends on a `›`, nor shows two together: a part dropped for room takes its
/// separator with it (beside the chat at 80 columns: `› src › main.rs`).
#[test]
fn the_path_bar_has_no_separator_alone() {
    for w in 24..64 {
        let mut s = Shot::new(w, 10);
        s.file("src/demo.rs", DEMO_RS);
        let bar = rows_of(&mut s, (0, 0, w as usize, 1)).remove(0);
        let bar = bar.trim();
        assert!(!bar.starts_with('›') && !bar.ends_with('›') && !bar.contains("›  ›"), "{w} cols: {bar:?}");
    }
}

/// An edit review's reason is whole sentences — the last ones that fit its two rows — not a cut that starts
/// mid-sentence behind a `…` and runs past the code's width.
#[test]
fn an_edit_reviews_reason_is_whole_sentences() {
    use unicode_width::UnicodeWidthStr;
    let mut s = Shot::new(100, 30);
    s.file("src/demo.rs", DEMO_RS);
    let reply = "<r i=\"1\">    println!(\"{} distinct words in {:?}\", counts.len(), text);\n</r>";
    let json = json!({"type": "result", "is_error": false, "result": reply}).to_string();
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), format!("read -r line; printf '%s\\n' '{json}'")];
    s.keys("17ggx i");
    s.keys("show the text too<ret>");
    s.until("review", |ed| ed.session.review.as_ref().is_some_and(|r| r.ready));
    s.ed.toasts.clear();
    let reason = "Keying by the name alone means a second strand with the same name replaces the first one in \
                  the map. The entry API keeps the one that came first. I will switch it so a repeat keeps the \
                  first strand, and since nothing else reads the map, no other line has to change.";
    s.ed.session.review.as_mut().unwrap().reason = reason.into();
    let top = s.ed.screen.review_rows.borrow().first().map_or(4, |r| r.0);
    // (the title's row, then the reason's: up to the first change's head)
    let rows = rows_of(&mut s, (0, 3, 100, top.saturating_sub(3)));
    let shown: Vec<&str> = rows
        .iter()
        .take_while(|r| !r.trim_start().chars().next().is_some_and(|c| c.is_ascii_digit()))
        .flat_map(|r| r.split_whitespace())
        .collect();
    let words: Vec<&str> = reason.split_whitespace().collect();
    // (`…` says sentences went before them)
    assert_eq!(shown.first(), Some(&"…"), "the cut said: {rows:#?}");
    let shown = &shown[1..];
    assert!(!shown.is_empty() && words.ends_with(shown), "whole words, the reason's last: {rows:#?}");
    assert!(words[words.len() - shown.len() - 1].ends_with('.'), "a sentence's start: {rows:#?}");
    // A last sentence longer than the two rows alone: its end, `…` in front, no row past the code's width (one-
    // letter words: wrapped at the full width, its first row fills it — the mark would push it past)
    let long = format!("Short one. {}word end.", "a ".repeat(200));
    s.ed.session.review.as_mut().unwrap().reason = long.clone();
    let top = s.ed.screen.review_rows.borrow().first().map_or(4, |r| r.0);
    let rows = rows_of(&mut s, (0, 3, 100, top.saturating_sub(3)));
    let reason_rows: Vec<&String> = rows
        .iter()
        .take_while(|r| !r.trim_start().chars().next().is_some_and(|c| c.is_ascii_digit()))
        .filter(|r| r.contains(" a a ") || r.contains("word end."))
        .collect();
    assert!(reason_rows.first().is_some_and(|r| r.trim_start().starts_with('…')), "{rows:#?}");
    assert!(reason_rows.last().is_some_and(|r| r.trim_end().ends_with("end.")), "{rows:#?}");
    assert!(reason_rows.iter().all(|r| r.trim_end().width() <= 100 - 4), "{rows:#?}");
}

/// The git tab opens on its first file, not on the `Changes` heading over it (the heading still takes `s` for
/// the whole section, a `k` away) — when the file in front isn't among the changes.
#[test]
fn the_git_tab_opens_on_a_file() {
    use crate::sidebar::{Mode, Sidebar, Target};
    let mut s = Shot::new(112, 24);
    // (rooted in the snapshots' own folder: a sync that reads the status again runs git there — never in tarae's)
    let root = s.in_project();
    s.file("src/demo.rs", DEMO_RS);
    let mut st = crate::gitpanel::parse_v2(
        "# branch.head main\0 1 .M N... 100644 100644 100644 0 0 src/thread.rs\0? src/knot.rs\0",
    );
    st.top = root.clone();
    st.generation = 1;
    s.ed.session.git_status = Some(st.with_rows());
    s.ed.session.sidebar = Some(Sidebar::new(Mode::Git, root));
    s.ed.sidebar_sync();
    s.frame();
    let sb = s.ed.session.sidebar.as_ref().unwrap();
    let on = &sb.lines()[sb.selected].target;
    assert!(matches!(on, Target::Change { file: true, .. }), "on {on:?}");
}

/// The git tab, shown again: back from another tab it starts on its first file again; refocused (`space g`
/// after `esc`) it keeps the row you were on — the `Changes` heading too, when that's what you picked.
#[test]
fn the_git_tab_reopened_starts_fresh_or_keeps_your_row() {
    use crate::sidebar::{Mode, Target};
    let mut s = Shot::new(112, 24);
    // (rooted in the snapshots' own folder: the files tab looks for its repository — never tarae's)
    let root = s.in_project();
    s.file("src/demo.rs", DEMO_RS);
    // (row 4, the `Staged` heading: a kept index lands off the files)
    let mut st = crate::gitpanel::parse_v2(
        "# branch.head main\0 1 .M N... 100644 100644 100644 0 0 src/thread.rs\0? src/knot.rs\0\
         1 M. N... 100644 100644 100644 0 0 docs/a.md\0 1 M. N... 100644 100644 100644 0 0 docs/b.md\0",
    );
    st.top = root;
    st.generation = 1;
    s.ed.session.git_status = Some(st.with_rows());
    let on = |s: &Shot| {
        let sb = s.ed.session.sidebar.as_ref().unwrap();
        sb.lines()[sb.selected].target.clone()
    };
    s.ed.sidebar_show(Mode::Git);
    s.frame();
    assert!(matches!(on(&s), Target::Change { file: true, .. }), "opened: {:?}", on(&s));
    // (up to the heading, the keys back to the code, the tab again: your row)
    s.keys("gg<esc>");
    assert!(matches!(on(&s), Target::Change { file: false, .. }), "on the heading: {:?}", on(&s));
    s.ed.sidebar_show(Mode::Git);
    s.frame();
    assert!(matches!(on(&s), Target::Change { file: false, .. }), "kept: {:?}", on(&s));
    // (the files tab, a row far down, then git again: its first file)
    s.ed.sidebar_show(Mode::Files);
    s.frame();
    s.ed.session.sidebar.as_mut().unwrap().selected = 4;
    s.ed.sidebar_show(Mode::Git);
    s.frame();
    assert!(matches!(on(&s), Target::Change { file: true, .. }), "back from files: {:?}", on(&s));
    // (Claude's link into the git tab from the files tab: its rows read again, not the files' row kept)
    s.ed.sidebar_show(Mode::Files);
    s.frame();
    s.ed.session.sidebar.as_mut().unwrap().selected = 4;
    s.ed.ui_jump("git:src/knot.rs");
    s.frame();
    assert!(matches!(on(&s), Target::Change { file: true, .. }), "a link in: {:?}", on(&s));
    // A tab built while it hadn't the keys (`:restart`, a link) and never had them: on a file too
    s.ed.session.sidebar = None;
    let mut sb = crate::sidebar::Sidebar::new(Mode::Git, s.root.clone());
    sb.focus = false;
    s.ed.session.sidebar = Some(sb);
    s.ed.sidebar_sync();
    s.ed.sidebar_show(Mode::Git);
    s.frame();
    assert!(matches!(on(&s), Target::Change { file: true, .. }), "built unfocused: {:?}", on(&s));
}

/// `:granted`'s cut rows read the same wherever the list is scrolled: what sets a row apart is worked out
/// against the whole list, not the rows on screen.
#[cfg(target_os = "macos")]
#[test]
fn granted_rows_cut_the_same_when_scrolled() {
    use crate::allow::Allowed;
    use crate::caps::{Caps, Entry, Source, Why};
    let at = |p: &str| PathBuf::from(format!("/private/tmp/tarae-shot-granted/{p}"));
    let names =
        ["stable-aarch64-apple-darwin", "nightly-aarch64-apple-darwin", "1.90-aarch64-apple-darwin", "esp"];
    let none = Allowed::default();
    let made = || {
        let mut caps = Caps::granted(&at("tarae"), &none, &none, &none);
        caps.read.extend(names.iter().map(|n| Entry {
            what: at(&format!("rustup/toolchains/{n}/lib/rustlib/src")),
            source: Source::Default(Why::Toolchain),
        }));
        caps.read.push(Entry { what: at("cargo/registry/src"), source: Source::Default(Why::Toolchain) });
        caps
    };
    let caps = made();
    let more = crate::granted::More {
        no_sandbox: false,
        repository: false,
        trusted: Vec::new(),
        plugins: Vec::new(),
        preflight: Vec::new(),
        elsewhere: 0,
    };
    let bare = |r: &String| r.trim_start_matches(['▎', ' ']).to_string();
    let mut compared = 0;
    // (screens short enough that the list scrolls, each showing a different few of its rows)
    for h in 8..=12u16 {
        let mut s = Shot::new(80, h);
        s.file("src/demo.rs", DEMO_RS);
        s.ed.session.caps = Some((1, std::sync::Arc::new(made())));
        s.ed.granted_open(
            crate::granted::items(&caps, [&none, &none, &none], &[], &more, 1_792_000_000),
            false,
        );
        let shown = |s: &mut Shot| -> Vec<String> {
            let rows = rows_of(s, (0, 0, 80, h as usize));
            rows.into_iter().filter(|r| r.contains("rustlib")).map(|r| r.trim().to_string()).collect()
        };
        let mut frames = vec![shown(&mut s)];
        for _ in 0..8 {
            s.keys("j");
            frames.push(shown(&mut s));
        }
        // (alone on screen beside the other places, a toolchain's row lost its name: `…apple-darwin/lib/…`)
        for row in frames.iter().flatten() {
            assert!(names.iter().any(|n| row.contains(n)), "{h} rows: {row:?} names its toolchain");
            compared += 1;
        }
        for n in names {
            let seen: Vec<String> =
                frames.iter().filter_map(|f| f.iter().find(|r| r.contains(n)).map(bare)).collect();
            assert!(seen.windows(2).all(|p| p[0] == p[1]), "{h} rows, {n}: {seen:#?}");
        }
    }
    assert!(compared > 0, "rows on screen");
}

/// Where a toast goes is decided once for its card: a routine one shown as a card stays a card when a longer one
/// joins it and makes it too wide for the narrow code — it doesn't jump to the status line mid-read.
#[test]
fn a_toasts_place_is_kept_for_its_card() {
    let mut s = Shot::new(80, 16);
    let long: String = (1..=30).map(|i| format!("let line_{i} = {};\n", "x".repeat(40))).collect();
    s.file("src/demo.rs", &long);
    s.keys("8gg");
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" l<esc>");
    s.ed.set_success("Saved a.rs");
    s.frame();
    assert!(s.ed.screen.toast_card.get().is_some(), "short: a card");
    s.ed.set_success("Saved src/sidebar.rs · 1204 lines, and its server told");
    s.frame();
    assert!(s.ed.screen.toast_card.get().is_some(), "the same card, where it was");
    // (its first message said again: a new id in its place — still the same card)
    s.ed.set_success("Saved a.rs");
    s.frame();
    assert!(s.ed.screen.toast_card.get().is_some(), "said again: still the card");
}

/// The chat beside the code at 80 columns, the code 48 wide.
fn narrow_code() -> Shot {
    let mut s = Shot::new(80, 16);
    let long: String = (1..=30).map(|i| format!("let line_{i} = {};\n", "x".repeat(40))).collect();
    s.file("src/demo.rs", &long);
    s.keys("8gg");
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" l<esc>");
    s
}

/// Routine messages said together over narrow code (a disk-watch round's two reloads) keep their card: folded,
/// the status line had room for the last one only, and the first showed nowhere. One that joins a message
/// already folded is counted there (`+1 earlier`).
#[test]
fn routine_messages_said_together_keep_their_card() {
    let mut s = narrow_code();
    s.ed.set_status("a.rs reloaded from disk");
    s.ed.set_status("b.rs reloaded from disk");
    s.frame();
    assert!(s.ed.screen.toast_card.get().is_some(), "both on a card");
    let screen = rows_of(&mut s, (0, 0, 80, 16)).join("\n");
    assert!(screen.contains("a.rs reloaded") && screen.contains("b.rs reloaded"), "{screen}");
    // (one alone folds; another joining it is counted)
    let mut s = narrow_code();
    s.ed.set_success("Saved main.rs · Claude's change");
    s.frame();
    assert_eq!(s.ed.screen.toast_card.get(), None, "folded");
    s.ed.set_status("b.rs reloaded from disk");
    let status = rows_of(&mut s, (0, 15, 80, 1)).remove(0);
    assert!(status.contains("b.rs reloaded") && status.contains("+1 earlier"), "{status:?}");
}

/// A routine message folded to the status line gives way to what the status line already says there — the
/// diagnostics' count, the mode, the position — cut to the room they leave (it took everything but the mode
/// and the position for its whole life).
#[test]
fn a_folded_routine_message_gives_way_to_the_diagnostics_count() {
    let mut s = narrow_code();
    s.ed.doc_mut().lsp.diagnostics = vec![crate::lsp::Diagnostic {
        from: 0,
        to: 3,
        severity: 1,
        message: "mismatched types".into(),
        raw: json!({ "source": "rustc" }),
        source: 0,
    }];
    let before = rows_of(&mut s, (0, 15, 80, 1)).remove(0);
    let sign =
        before.chars().find(|c| "▲●■✖".contains(*c)).unwrap_or_else(|| panic!("its count: {before:?}"));
    s.ed.set_success("Saved src/sidebar.rs · 1204 lines, its server told and its tests queued to run");
    let after = rows_of(&mut s, (0, 15, 80, 1)).remove(0);
    assert!(after.contains(sign) && after.contains("Saved src"), "{before:?} → {after:?}");
}

/// Code too narrow for a toast to keep off it (the card over half its width, code under both its places — 40
/// columns beside the chat): a routine message is said on the status line instead; an error keeps its card.
#[test]
fn a_narrow_codes_routine_toast_goes_to_the_status_line() {
    let mut s = Shot::new(80, 16);
    let long: String = (1..=30).map(|i| format!("let line_{i} = {};\n", "x".repeat(40))).collect();
    s.file("src/demo.rs", &long);
    s.keys("8gg");
    // (the chat beside it: the code keeps 48 columns)
    s.ed.config.llm.command = "sh".into();
    s.ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    s.keys(" l<esc>");
    s.ed.set_success("Saved main.rs · Claude's change");
    s.frame();
    assert_eq!(s.ed.screen.toast_card.get(), None, "no card over the code");
    let status = rows_of(&mut s, (0, 15, 80, 1)).remove(0);
    assert!(status.contains("Saved main.rs"), "said on the status line: {status:?}");
    s.ed.set_error("no such command: 'frobnicate'");
    s.frame();
    assert!(s.ed.screen.toast_card.get().is_some(), "an error keeps its card");
}

/// `:granted`'s paths under one folder keep what sets each apart when cut: five toolchains' sources read
/// `~/.rustup/toolchai…apple-darwin/lib/rustlib/src` alike, the toolchain's name cut out of every one.
#[cfg(target_os = "macos")]
#[test]
fn granted_rows_cut_keep_what_differs() {
    use crate::allow::Allowed;
    use crate::caps::{Caps, Entry, Source, Why};
    let at = |p: &str| PathBuf::from(format!("/private/tmp/tarae-shot-granted/{p}"));
    let names =
        ["stable-aarch64-apple-darwin", "nightly-aarch64-apple-darwin", "1.90-aarch64-apple-darwin", "esp"];
    let none = Allowed::default();
    let made = || {
        let mut caps = Caps::granted(&at("tarae"), &none, &none, &none);
        caps.read.extend(names.iter().map(|n| Entry {
            what: at(&format!("rustup/toolchains/{n}/lib/rustlib/src")),
            source: Source::Default(Why::Toolchain),
        }));
        caps
    };
    let caps = made();
    let mut s = Shot::new(80, 24);
    s.file("src/demo.rs", DEMO_RS);
    s.ed.session.caps = Some((1, std::sync::Arc::new(made())));
    let more = crate::granted::More {
        no_sandbox: false,
        repository: false,
        trusted: Vec::new(),
        plugins: Vec::new(),
        preflight: Vec::new(),
        elsewhere: 0,
    };
    s.ed.granted_open(crate::granted::items(&caps, [&none, &none, &none], &[], &more, 1_792_000_000), false);
    let rows: Vec<String> =
        rows_of(&mut s, (0, 0, 80, 24)).into_iter().filter(|r| r.contains("toolchain")).collect();
    assert_eq!(rows.len(), names.len(), "{rows:#?}");
    for n in names {
        assert!(rows.iter().any(|r| r.contains(n)), "{n} in {rows:#?}");
    }
}
