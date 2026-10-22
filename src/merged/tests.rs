//! The merged view (`merged.rs`): the merge itself, and three checkouts of a fixture repository merged in memory —
//! what's shown, in whose colour, what keys do there, and what Claude hears.

use std::process::Command;
use std::time::Duration;

use serde_json::json;

use super::*;
use crate::config::Config;
use crate::key::{Code, Key};
use crate::scratch::Scratch;

/// Twelve lines: `line 1` … `line 12`.
fn twelve() -> String {
    (1..=12).map(|n| format!("line {n}\n")).collect()
}

/// `twelve` with line `n` (from 1) as `text`.
fn with(n: usize, text: &str) -> String {
    (1..=12).map(|k| if k == n { format!("{text}\n") } else { format!("line {k}\n") }).collect()
}

fn side(text: String) -> Input {
    Input { text, merge_base: None }
}

/// Changes far apart merge clean, each its side's; the same change twice is one, both named; two sides on one line
/// are a conflict — the tip's line, then each side's, one after another.
#[test]
fn far_apart_merge_clean_and_one_line_two_ways_is_a_conflict() {
    let m = lay(&twelve(), &[side(with(2, "two, a")), side(with(10, "ten, b"))]);
    assert_eq!(m.text, with(2, "two, a").replace("line 10", "ten, b"));
    assert!(m.conflicts.is_empty());
    let lines: Vec<(usize, Range<usize>)> = m.changes.iter().map(|c| (c.side, c.lines.clone())).collect();
    assert_eq!(lines, [(0, 1..2), (1, 9..10)]);
    // (the same change by both: one, its first side's, the other with it)
    let m = lay(&twelve(), &[side(with(4, "four")), side(with(4, "four"))]);
    assert!(m.conflicts.is_empty());
    assert_eq!((m.changes[0].side, m.changes[0].with.clone()), (0, vec![1]));
    // Two ways: the tip's line, then a's, then b's
    let m = lay(&twelve(), &[side(with(5, "five, a")), side(with(5, "five, b"))]);
    assert!(m.changes.is_empty());
    let c = &m.conflicts[0];
    assert_eq!(c.base, 4..5);
    let parts: Vec<(usize, Range<usize>, Range<usize>)> =
        c.parts.iter().map(|p| (p.side, p.lines.clone(), p.own.clone())).collect();
    assert_eq!(parts, [(0, 5..6, 4..5), (1, 6..7, 4..5)]);
    let text: Vec<&str> = m.text.lines().collect();
    assert_eq!(&text[3..8], ["line 4", "line 5", "five, a", "five, b", "line 6"]);
    assert_eq!(m.text.lines().count(), 14);
    // (touching lines conflict too, as git's merge: a's line 5 and b's line 6)
    let m = lay(&twelve(), &[side(with(5, "five, a")), side(with(6, "six, b"))]);
    assert_eq!(m.conflicts.len(), 1, "{m:?}");
}

/// One side's two hunks with another's between them in one group (a's line 5 and 7, b's 6, touching): a's version
/// is one part — never two columns of the same side.
#[test]
fn a_sides_hunks_apart_in_one_conflict_are_one_part() {
    let a = with(5, "five, a").replace("line 7", "seven, a");
    let m = lay(&twelve(), &[side(a), side(with(6, "six, b"))]);
    assert_eq!(m.conflicts.len(), 1, "{m:?}");
    let parts: Vec<(usize, Range<usize>)> =
        m.conflicts[0].parts.iter().map(|p| (p.side, p.own.clone())).collect();
    assert_eq!(parts, [(0, 4..7), (1, 4..7)], "{m:?}");
    let text: Vec<&str> = m.text.lines().collect();
    assert_eq!(&text[4..7], ["line 5", "line 6", "line 7"], "the tip's part");
    assert_eq!(&text[7..10], ["five, a", "line 6", "seven, a"], "a's, whole");
    assert_eq!(&text[10..13], ["line 5", "six, b", "line 7"], "b's");
}

/// A side behind a tip that added lines above its conflict: its part's own lines are its own text's (what Claude is
/// told), not the laid text's.
#[test]
fn a_behind_sides_own_lines_are_its_own_texts() {
    let tip = format!("new 1\nnew 2\n{}", with(9, "nine, tip"));
    let m = lay(&tip, &[Input { text: with(9, "nine, fix"), merge_base: Some(twelve()) }]);
    let c = &m.conflicts[0];
    assert!(c.tip);
    assert_eq!(c.parts[0].own, 8..9, "its line 9, not the laid text's 11: {m:?}");
}

/// A side's last line without its newline, mid-conflict: a line of its own, never joined to the next part's.
#[test]
fn a_part_without_its_last_newline_stays_a_line_of_its_own() {
    let base = "a\nb\nc";
    let m = lay(base, &[side("a\nb\nc, one".into()), side("a\nb\nc, two".into())]);
    let text: Vec<&str> = m.text.lines().collect();
    assert_eq!(text, ["a", "b", "c", "c, one", "c, two"]);
}

/// A side behind the tip: its change laid on the tip first — far from the tip's own change, only its own is a change
/// of the merge; on the tip's own line, a conflict whose first part is the tip's line.
#[test]
fn a_side_behind_the_tip_is_laid_on_it_first() {
    let tip = with(3, "three, tip");
    let behind = |text: String| Input { text, merge_base: Some(twelve()) };
    let m = lay(&tip, &[behind(with(9, "nine, fix"))]);
    assert_eq!(m.text, tip.replace("line 9", "nine, fix"), "the tip's change kept, its own laid on it");
    assert_eq!((m.changes.len(), m.changes[0].lines.clone()), (1, 8..9));
    let m = lay(&tip, &[behind(with(3, "three, fix"))]);
    assert!(m.changes.is_empty(), "{m:?}");
    let c = &m.conflicts[0];
    assert!(c.tip, "its first part the tip's change");
    let text: Vec<&str> = m.text.lines().collect();
    assert_eq!(&text[2..4], ["three, tip", "three, fix"]);
    // (the tip's change made the same way by it: nothing of its own there)
    let m = lay(&tip, &[behind(tip.clone())]);
    assert!(m.changes.is_empty() && m.conflicts.is_empty(), "{m:?}");
}

/// A measurement, not a check (`cargo perf`): the in-memory merge of a big file — 20 000 lines (term.rs's size), three
/// sides each with a dozen changes far apart and two meeting on one line — as the worker lays it on each buffer's move.
#[test]
fn perf_merge_a_big_file() {
    if crate::test_results::unoptimized("merged::tests::perf_merge_a_big_file") {
        return;
    }
    use std::io::Write as _;
    const LINES: usize = 20_000;
    let base: String = (0..LINES).map(|n| format!("    let x{n} = compute({n}, \"{n}\");\n")).collect();
    let side = |k: usize| -> String {
        let mut lines: Vec<String> = base.lines().map(|l| format!("{l}\n")).collect();
        for h in 0..12 {
            let at = (h * 1_600 + k * 500) % LINES;
            lines[at] = format!("    let x{at} = side{k}({at});\n");
        }
        // (the two first meet on one line)
        if k < 2 {
            lines[10_001] = format!("    let x10001 = both{k}();\n");
        }
        lines.concat()
    };
    let inputs: Vec<Input> = (0..3).map(|k| Input { text: side(k), merge_base: None }).collect();
    let mut ms = Vec::new();
    for _ in 0..7 {
        let t = std::time::Instant::now();
        let m = lay(&base, &inputs);
        ms.push(t.elapsed().as_secs_f64() * 1e3);
        assert_eq!(m.conflicts.len(), 1);
        assert_eq!(m.changes.len(), 36);
    }
    ms.sort_by(f64::total_cmp);
    let line = format!("PERF merged_big_file_ms {:.2} noise=2\n", ms[ms.len() / 2]);
    let _ = std::io::stderr().write_all(line.as_bytes());
}

/// git in the test's own repository: its own name, no signing, no hooks, none of the machine's config.
fn git(dir: &Path, args: &[&str]) {
    crate::gitrepo::not_in_this_repository(dir, args);
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
        .args(["-c", "init.defaultBranch=main", "-c", "core.hooksPath=/dev/null"])
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

struct Scene {
    ed: Editor,
    /// The repository's first checkout (the home session's, `main`) and two more, `fix` (in front) and `docs`.
    main: PathBuf,
    fix: PathBuf,
    docs: PathBuf,
    ids: [SessionId; 3],
    dir: Scratch,
}

/// A repository (`a.rs` of twelve lines committed) and two more checkouts of it, each its session: you in `fix` with
/// its `a.rs` open, `fix` and `docs` based on `main`.
fn scene(name: &str) -> Scene {
    let dir = Scratch::new(name);
    let main = dir.join("main");
    std::fs::create_dir_all(&main).unwrap();
    let main = std::fs::canonicalize(main).unwrap();
    std::fs::write(main.join("a.rs"), twelve()).unwrap();
    std::fs::write(main.join("b.rs"), twelve()).unwrap();
    git(&main, &["init", "-q"]);
    git(&main, &["add", "-A"]);
    git(&main, &["commit", "-qm", "first"]);
    git(&main, &["worktree", "add", "-q", "../fix"]);
    git(&main, &["worktree", "add", "-q", "../docs"]);
    let fix = std::fs::canonicalize(dir.join("fix")).unwrap();
    let docs = std::fs::canonicalize(dir.join("docs")).unwrap();
    let mut ed = Editor::new(Config::default());
    ed.session.root = main.clone();
    ed.session.name = "main".into();
    ed.session.git_branch = Some("main".into());
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    let mut ids = [0; 3];
    for (k, (root, name)) in [(&fix, "fix"), (&docs, "docs")].into_iter().enumerate() {
        let id = ed.session_add(root.clone());
        let l = ed.session_by_id(id).unwrap();
        l.name = name.into();
        l.base = Some("main".into());
        l.git_branch = Some(name.into());
        ids[k + 1] = id;
    }
    assert!(ed.session_switch(ids[1]));
    ed.open(&fix.join("a.rs")).unwrap();
    Scene { ed, main, fix, docs, ids, dir }
}

/// The front buffer's line `n` (from 1) made `text`, unsaved.
fn type_line(ed: &mut Editor, n: usize, text: &str) {
    let old = ed.doc().text.clone();
    let mut lines: Vec<String> = old.lines().map(|l| l.to_string()).collect();
    lines[n - 1] = format!("{text}\n");
    let new = ropey::Rope::from_str(&lines.concat());
    let c = crate::disk::minimal_change(&old, &new).unwrap();
    ed.doc_mut().apply(&crate::transaction::Transaction::new(vec![c]));
}

/// `path`'s line `n` (from 1) made `text` on disk, said as a save.
fn save_line(ed: &mut Editor, path: &Path, n: usize, text: &str) {
    let now = std::fs::read_to_string(path).unwrap();
    let lines: Vec<String> =
        now.lines().enumerate().map(|(i, l)| if i + 1 == n { text.into() } else { l.into() }).collect();
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
    ed.wrote(path);
}

/// The front document's merged view.
fn shown(ed: &Editor) -> &Shown {
    of(ed.doc()).expect("a merged view in front")
}

/// Events until the merged view in front has had `n` merges land and none is under way.
fn merged_to(ed: &mut Editor, n: u64) {
    sync(ed);
    let end = std::time::Instant::now() + Duration::from_secs(20);
    while shown(ed).merges < n || shown(ed).asking {
        let left = end.saturating_duration_since(std::time::Instant::now());
        let ev = ed.events.recv_timeout(left).unwrap_or_else(|| panic!("merge {n}: no event in time"));
        ed.handle_event(ev);
    }
}

fn press(ed: &mut Editor, keys: &str) {
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let k: Key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        };
        ed.handle_event(crate::event::Event::Key(k));
    }
}

/// The frame, `w` wide.
fn frame(ed: &mut Editor, w: u16) -> vt100::Parser {
    crossterm::style::force_color_output(true);
    let mut bytes = Vec::new();
    crate::term::render(ed, &mut bytes, w, 30).unwrap();
    let mut p = vt100::Parser::new(30, w, 0);
    p.process(&bytes);
    p
}

fn rows(p: &vt100::Parser) -> Vec<String> {
    let (_, w) = p.screen().size();
    p.screen().rows(0, w).collect()
}

/// Participant `colour`'s colour at full strength, as the frame paints it.
fn participant(ed: &Editor, colour: u8) -> vt100::Color {
    let scope = format!("ui.participant.{}", colour as usize % crate::session::COLOURS + 1);
    match ed.theme.try_get(&scope).and_then(|s| s.fg) {
        Some(crossterm::style::Color::Rgb { r, g, b }) => vt100::Color::Rgb(r, g, b),
        c => panic!("{scope}: {c:?}"),
    }
}

/// The cell starting `word` on the first row holding it.
fn cell_of<'a>(p: &'a vt100::Parser, word: &str) -> &'a vt100::Cell {
    let rows = rows(p);
    let y = rows.iter().position(|r| r.contains(word)).unwrap_or_else(|| panic!("{word:?} in {rows:#?}"));
    let x = rows[y][..rows[y].find(word).unwrap()].chars().count();
    p.screen().cell(y as u16, x as u16).unwrap()
}

/// Three workspaces changing one file far apart — `main` saved, `fix` unsaved in its buffer, `docs` saved: the merged
/// view holds all three, each change its session's, its first line's prefix in the sign column in its colour, the
/// path bar naming them (`fix unsaved`); saved, `unsaved` goes — nothing in any `.git` written.
#[test]
fn three_workspaces_merge_clean_each_change_in_its_colour() {
    let Scene { mut ed, main, fix, docs, ids, dir } = scene("merged-clean");
    // (a session on another repository, its `a.rs` changed: never a side)
    let other = dir.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let other = std::fs::canonicalize(other).unwrap();
    std::fs::write(other.join("a.rs"), twelve()).unwrap();
    git(&other, &["init", "-q"]);
    git(&other, &["add", "-A"]);
    git(&other, &["commit", "-qm", "elsewhere"]);
    std::fs::write(other.join("a.rs"), with(8, "eight, elsewhere")).unwrap();
    let id = ed.session_add(other.clone());
    ed.session_by_id(id).unwrap().name = "other".into();
    let before = files(&main.join(".git"));
    save_line(&mut ed, &main.join("a.rs"), 2, "two, main");
    save_line(&mut ed, &docs.join("a.rs"), 11, "eleven, docs");
    type_line(&mut ed, 6, "six, fix");
    crate::typed::execute(&mut ed, "merged").unwrap();
    merged_to(&mut ed, 1);
    let m = shown(&ed);
    assert_eq!(m.rel, "a.rs");
    let names: Vec<(&str, bool)> = m.sides.iter().map(|s| (s.name.as_str(), s.unsaved)).collect();
    assert_eq!(names, [("fix", true), ("main", false), ("docs", false)]);
    assert!(m.merge.conflicts.is_empty());
    let text = ed.doc().text.to_string();
    for l in ["two, main", "six, fix", "eleven, docs"] {
        assert!(text.contains(l), "{l} in {text}");
    }
    let whose: Vec<(usize, &str)> =
        m.merge.changes.iter().map(|c| (c.lines.start, m.sides[c.side].name.as_str())).collect();
    assert_eq!(whose, [(1, "main"), (5, "fix"), (10, "docs")]);
    assert_eq!(m.prefixes, ["f", "m", "d"]);
    let p = frame(&mut ed, 120);
    let r = rows(&p);
    let line = |t: &str| r.iter().find(|l| l.contains(t)).unwrap_or_else(|| panic!("{t} in {r:#?}")).clone();
    assert!(line("six, fix").trim_start().starts_with('f'), "{}", line("six, fix"));
    assert!(line("two, main").trim_start().starts_with('m'));
    assert!(line("merged · a.rs").contains("fix unsaved · main · docs"), "{}", line("merged · a.rs"));
    let colours = |ids: &[SessionId]| -> Vec<u8> {
        ids.iter().map(|id| ed.sessions_in_order().iter().find(|l| l.id == *id).unwrap().colour).collect()
    };
    let c = colours(&ids);
    let prefix = |p: &vt100::Parser, row: &str| {
        let rows = rows(p);
        let y = rows.iter().position(|l| l.contains(row)).unwrap();
        let x = rows[y].len() - rows[y].trim_start().len();
        p.screen().cell(y as u16, x as u16).unwrap().fgcolor()
    };
    assert_eq!(prefix(&p, "six, fix"), participant(&ed, c[1]));
    assert_eq!(prefix(&p, "two, main"), participant(&ed, c[0]));
    assert_eq!(prefix(&p, "eleven, docs"), participant(&ed, c[2]));
    assert_eq!(cell_of(&p, "fix unsaved").fgcolor(), participant(&ed, c[1]), "its name in its colour");
    // The cursor on a change: whose, at its first line's end
    press(&mut ed, "6G");
    let p = frame(&mut ed, 120);
    let r = rows(&p);
    assert!(r.iter().any(|l| l.contains("six, fix") && l.contains("‹fix›")), "{r:#?}");
    assert!(!r.iter().any(|l| l.contains("‹main›")), "only where the cursor is");
    // Saved: no longer unsaved (merged again on the save)
    let n = shown(&ed).merges;
    let a = ed.docs.iter().position(|d| d.path.as_deref() == Some(fix.join("a.rs").as_path())).unwrap();
    let save = std::fs::write(fix.join("a.rs"), ed.docs[a].text.to_string());
    save.unwrap();
    ed.docs[a].mark_saved();
    ed.wrote(&fix.join("a.rs"));
    merged_to(&mut ed, n + 1);
    assert!(!shown(&ed).sides[0].unsaved);
    assert_eq!(files(&main.join(".git")), before, "nothing written in the repository's git folder");
}

/// Another workspace's file as a link to a file outside (a stand-in secret in the test's own folder), a FIFO, a file
/// past `TEXT_MAX`, a binary one: the link never gone through, the FIFO never waited on, the others not taken — that
/// session left out and said on the path bar with why, every other side still merged (one copy can't blank the view).
#[test]
fn another_workspaces_link_fifo_or_huge_file_is_never_read() {
    let Scene { mut ed, main, docs, dir, .. } = scene("merged-link");
    std::fs::write(dir.join("secret.txt"), "SECRET-STAND-IN\n").unwrap();
    save_line(&mut ed, &main.join("a.rs"), 2, "two, main");
    std::fs::remove_file(docs.join("a.rs")).unwrap();
    std::os::unix::fs::symlink(dir.join("secret.txt"), docs.join("a.rs")).unwrap();
    ed.wrote(&docs.join("a.rs"));
    crate::typed::execute(&mut ed, "merged").unwrap();
    merged_to(&mut ed, 1);
    let m = shown(&ed);
    assert!(!ed.doc().text.to_string().contains("SECRET"), "a link never gone through");
    assert_eq!(m.left, [("docs".to_string(), "not a plain file")]);
    assert_eq!(m.sides.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["main"]);
    let r = rows(&frame(&mut ed, 160));
    assert!(r[0].contains("docs left out: not a plain file"), "{}", r[0]);
    // A FIFO where its file goes: refused at once — the merge lands, nothing waits
    std::fs::remove_file(docs.join("a.rs")).unwrap();
    let fifo = std::ffi::CString::new(docs.join("a.rs").into_os_string().into_encoded_bytes()).unwrap();
    // SAFETY: a path of the test's own folder, live for the call.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    ed.wrote(&docs.join("a.rs"));
    let n = shown(&ed).merges;
    merged_to(&mut ed, n + 1);
    assert_eq!(shown(&ed).left, [("docs".to_string(), "not a plain file")], "a FIFO left out");
    // Past `TEXT_MAX`, then binary: that side left out, said with why — main's change still merged
    let still = |ed: &Editor, why: &'static str| {
        let m = shown(ed);
        assert_eq!((m.said.as_deref(), m.left.clone()), (None, vec![("docs".to_string(), why)]));
        assert_eq!(m.sides.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["main"], "the rest merged");
        assert!(ed.doc().text.to_string().contains("two, main"));
    };
    std::fs::remove_file(docs.join("a.rs")).unwrap();
    std::fs::write(docs.join("a.rs"), "x".repeat(TEXT_MAX + 1)).unwrap();
    ed.wrote(&docs.join("a.rs"));
    merged_to(&mut ed, n + 2);
    still(&ed, "too large");
    let r = rows(&frame(&mut ed, 160));
    assert!(r[0].contains("docs left out: too large"), "{}", r[0]);
    std::fs::write(docs.join("a.rs"), with(4, "four\0docs")).unwrap();
    ed.wrote(&docs.join("a.rs"));
    merged_to(&mut ed, n + 3);
    still(&ed, "binary");
}

/// The list's `v` on a session: the file where its change would conflict with yours (C2's overlap), at that
/// session's conflict — not the first one — and on a view open already, the cursor taken there too; the hint says it.
#[test]
fn the_lists_v_opens_the_overlaps_file_at_that_sessions_conflict() {
    let Scene { mut ed, main, docs, ids, dir: _dir, .. } = scene("merged-list-v");
    save_line(&mut ed, &docs.join("a.rs"), 3, "three, docs");
    save_line(&mut ed, &main.join("a.rs"), 9, "nine, main");
    type_line(&mut ed, 3, "three, fix");
    type_line(&mut ed, 9, "nine, fix");
    // (C2's overlap of fix and main in a.rs, as a round would have found it)
    let main_side = crate::overlap::Other::Session(ids[0]);
    ed.overlap.now.push(crate::overlap::Overlap {
        a: ids[1],
        b: main_side,
        path: "a.rs".into(),
        lines: Some(8..9),
        other_lines: Some(8..9),
    });
    let to_row = |ed: &mut Editor, id: SessionId| {
        press(ed, " W");
        for _ in 0..4 {
            let p = ed.picker.as_ref().unwrap();
            if matches!(p.current().map(|i| &i.action), Some(crate::picker::Action::Session(s)) if *s == id) {
                break;
            }
            press(ed, "j");
        }
    };
    to_row(&mut ed, ids[0]);
    let p = frame(&mut ed, 160);
    assert!(rows(&p)[29].contains("v merged"), "the hint: {}", rows(&p)[29]);
    press(&mut ed, "v");
    merged_to(&mut ed, 1);
    let m = shown(&ed);
    assert_eq!((m.rel.as_str(), m.merge.conflicts.len()), ("a.rs", 2));
    let with_main = m.merge.conflicts[1].clone();
    assert!(with_main.parts.iter().any(|p| m.sides[p.side].name == "main"));
    assert_eq!(ed.cursor_line(), with_main.base.start, "at main's conflict, not the first");
    // Open already, the cursor elsewhere: taken to it again
    press(&mut ed, "gg");
    let view = ed.doc().id;
    to_row(&mut ed, ids[0]);
    press(&mut ed, "v");
    assert_eq!(ed.doc().id, view, "the view open already");
    assert_eq!(ed.cursor_line(), with_main.base.start);
}

/// Every file under `dir`, by its path from there, with its size.
fn files(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut out = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let m = e.metadata().unwrap();
            match m.is_dir() {
                true => todo.push(e.path()),
                false => out.push((e.path().strip_prefix(dir).unwrap().to_path_buf(), m.len())),
            }
        }
    }
    out.sort();
    out
}

/// Two workspaces on one line: a conflict — side by side where the code is wide (the tip's line, then each side's,
/// each in its column under its name), stacked where it's narrow (a head over each part); a selection wider than a
/// character over it opens it stacked, as a folded block; `]m`/`[m` go to it, `]g` steps the changes.
#[test]
fn a_conflict_shows_side_by_side_wide_and_stacked_narrow() {
    let Scene { mut ed, main, ids, dir: _dir, .. } = scene("merged-conflict");
    save_line(&mut ed, &main.join("a.rs"), 5, "five, main");
    type_line(&mut ed, 5, "five, fix");
    type_line(&mut ed, 10, "ten, fix");
    crate::typed::execute(&mut ed, "merged").unwrap();
    merged_to(&mut ed, 1);
    let m = shown(&ed);
    assert_eq!(m.merge.conflicts.len(), 1);
    let c = m.merge.conflicts[0].clone();
    let parts: Vec<&str> = c.parts.iter().map(|p| m.sides[p.side].name.as_str()).collect();
    assert_eq!(parts, ["fix", "main"]);
    // Wide: one head row, then a row of columns
    let p = frame(&mut ed, 160);
    let r = rows(&p);
    let head = r.iter().find(|l| l.contains("conflict 1 of 1")).unwrap_or_else(|| panic!("{r:#?}"));
    assert!(head.contains("base") && head.contains("┬─ fix") && head.contains("┬─ main"), "{head}");
    let cols = r.iter().find(|l| l.contains("five, fix")).unwrap();
    assert!(cols.contains("line 5") && cols.contains("│") && cols.contains("five, main"), "{cols}");
    assert!(head.trim_start().starts_with('≈'), "{head}");
    let colour = ed.sessions_in_order().iter().find(|l| l.id == ids[0]).unwrap().colour;
    assert_eq!(cell_of(&p, "─ main").fgcolor(), cell_of(&p, "─ fix").fgcolor(), "the rules faint");
    let name = {
        let rows = rows(&p);
        let y = rows.iter().position(|l| l.contains("┬─ main")).unwrap();
        let x = rows[y][..rows[y].find("┬─ main").unwrap()].chars().count() + 3;
        p.screen().cell(y as u16, x as u16).unwrap().fgcolor()
    };
    assert_eq!(name, participant(&ed, colour), "main's name in its colour");
    // Narrow: stacked — a head over each part, each part's lines their own
    let p = frame(&mut ed, 80);
    let r = rows(&p);
    let at = |t: &str| r.iter().position(|l| l.contains(t)).unwrap_or_else(|| panic!("{t} in {r:#?}"));
    assert!(at("conflict 1 of 1 · base") < at("line 5"));
    assert!(at("line 5") < at("─ fix") && at("─ fix") < at("five, fix"));
    assert!(at("five, fix") < at("─ main") && at("─ main") < at("five, main"));
    // Keys: `]m` to the conflict, `]g` on to the change after it, `[m` back
    press(&mut ed, "gg]m");
    assert_eq!(ed.cursor_line(), c.base.start);
    press(&mut ed, "]g");
    let from = ed.doc().text.byte_to_line(ed.doc().selection().primary().from());
    let line = ed.doc().text.line(from).to_string();
    assert!(line.contains("ten, fix"), "{line:?} at {from}: {:?}", hunks(shown(&ed)));
    press(&mut ed, "[m");
    assert_eq!(ed.cursor_line(), c.base.start);
    // Wide, `x` over it: opened stacked, as a folded block
    press(&mut ed, "x");
    let r = rows(&frame(&mut ed, 160));
    assert!(r.iter().any(|l| l.contains("conflict 1 of 1 · base")), "{r:#?}");
    assert!(!r.iter().any(|l| l.contains("┬─ fix")), "{r:#?}");
}

/// The home checkout in front with its branch not read yet (a session just switched to): `:merged` isn't refused —
/// the merge reads the branch from its HEAD, and the view is named by it once it lands.
#[test]
fn a_branch_not_read_yet_is_read_with_the_merge() {
    let Scene { mut ed, main, docs, ids, dir: _dir, .. } = scene("merged-branch");
    assert!(ed.session_switch(ids[0]));
    ed.session.git_branch = None;
    ed.open(&main.join("a.rs")).unwrap();
    save_line(&mut ed, &docs.join("a.rs"), 4, "four, docs");
    crate::typed::execute(&mut ed, "merged").unwrap();
    merged_to(&mut ed, 1);
    let m = shown(&ed);
    assert_eq!((m.said.as_deref(), m.branch.as_deref()), (None, Some("main")));
    assert_eq!(m.sides.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["docs"]);
}

/// The merged view is read-only: an edit put back and insert mode left, said where you look; the buffer of your own
/// workspace untouched.
#[test]
fn the_merged_view_is_read_only() {
    let Scene { mut ed, main, fix, dir: _dir, .. } = scene("merged-readonly");
    save_line(&mut ed, &main.join("a.rs"), 2, "two, main");
    crate::typed::execute(&mut ed, "merged").unwrap();
    merged_to(&mut ed, 1);
    let text = ed.doc().text.to_string();
    press(&mut ed, "ggdd");
    assert_eq!(ed.doc().text.to_string(), text, "put back");
    assert!(ed.messages.iter().any(|m| m.2.contains("read-only")), "said");
    press(&mut ed, "ihello");
    assert_eq!(ed.mode, crate::editor::Mode::Normal, "insert mode left");
    assert_eq!(ed.doc().text.to_string(), text);
    assert!(!ed.doc().is_modified(), "nothing to save");
    let own = ed.docs.iter().find(|d| d.path.as_deref() == Some(fix.join("a.rs").as_path())).unwrap();
    assert_eq!(own.text.to_string(), twelve(), "your own buffer untouched");
}

/// `space v j` opens the current file's merged view, and from it again the same one; the session list's `v` on a
/// session opens the file it changed.
#[test]
fn space_v_j_and_the_lists_v_open_it() {
    let Scene { mut ed, docs, ids, dir: _dir, .. } = scene("merged-open");
    press(&mut ed, " vj");
    assert_eq!(shown(&ed).rel, "a.rs");
    let id = ed.doc().id;
    press(&mut ed, " vj");
    assert_eq!(ed.doc().id, id, "the one open already");
    // (a file named like a key: never merged — nobody's copy of it read)
    let e = crate::typed::execute(&mut ed, "merged server.pem").unwrap_err();
    assert!(e.contains("named like a key"), "{e}");
    merged_to(&mut ed, 1);
    assert!(shown(&ed).sides.is_empty(), "no session changed it");
    // (the list's `v`: docs's file — where it is, else the first it changed; none yet: said)
    let _ = docs;
    press(&mut ed, " W");
    let on = |ed: &Editor| {
        let p = ed.picker.as_ref().unwrap();
        matches!(p.current().map(|i| &i.action), Some(crate::picker::Action::Session(s)) if *s == ids[2])
    };
    for _ in 0..4 {
        if !on(&ed) {
            press(&mut ed, "j");
        }
    }
    assert!(on(&ed), "docs's row in hand");
    press(&mut ed, "v");
    assert!(ed.messages.iter().any(|m| m.2.contains("nothing of it to merge yet")), "{:?}", ed.messages);
}

/// Claude's `read` of a file in its workspace whose merge was worked out: a line where its change meets another's —
/// its own lines, the other's name — nothing where it merges clean (C2's line held back too).
#[test]
fn claude_hears_where_its_change_meets_another_only() {
    let Scene { mut ed, main, fix, dir: _dir, .. } = scene("merged-told");
    let mut c = crate::chat::Chat::new();
    let _ = crate::chat::stand_in(&mut c);
    ed.session.chat = Some(c);
    save_line(&mut ed, &main.join("a.rs"), 5, "five, main");
    type_line(&mut ed, 5, "five, fix");
    crate::typed::execute(&mut ed, "merged").unwrap();
    merged_to(&mut ed, 1);
    assert_eq!(
        told(&ed, &fix.join("a.rs")),
        Some(vec!["a.rs: merged with main's change: conflict at L5".to_string()])
    );
    assert_eq!(told(&ed, &fix.join("b.rs")), None, "no merge of it worked out");
    assert_eq!(
        crate::overlap::told(&ed, &fix.join("a.rs")),
        ["a.rs: merged with main's change: conflict at L5"]
    );
    // Clean: nothing
    let a = ed.docs.iter().position(|d| d.path.as_deref() == Some(fix.join("a.rs").as_path())).unwrap();
    let n = shown(&ed).merges;
    let merged = ed.current;
    ed.current = a;
    type_line(&mut ed, 5, "line 5");
    type_line(&mut ed, 9, "nine, fix");
    ed.current = merged;
    // (its own edit just resolved it: the merge from before isn't told — C2's line stands, none here)
    assert_eq!(told(&ed, &fix.join("a.rs")), None, "a merge from before the edit");
    assert!(crate::overlap::told(&ed, &fix.join("a.rs")).is_empty());
    merged_to(&mut ed, n + 1);
    assert!(shown(&ed).merge.conflicts.is_empty());
    assert_eq!(told(&ed, &fix.join("a.rs")), Some(Vec::new()));
    let answer = |ed: &mut Editor, n: u64| {
        let end = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let got = crate::tools::ANSWERED
                .with(|a| a.borrow().iter().find(|(i, _)| *i == json!(n)).map(|(_, r)| r.clone()));
            if let Some(r) = got {
                return r.unwrap_or_else(|e| e);
            }
            assert!(std::time::Instant::now() < end, "call {n} answered");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
    };
    crate::readtool::call(&mut ed, crate::tools::Call::test(81), &json!({"files": ["a.rs"]})).unwrap();
    let r = answer(&mut ed, 81);
    assert!(!r.contains("merged with") && !r.contains("conflict"), "{r}");
}
