//! The woven view (`weave.rs`): the merge itself, and three checkouts of a fixture repository merged in memory —
//! what's shown, in whose colour, and what keys do there.

use std::process::Command;
use std::time::Duration;

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
    if crate::test_results::unoptimized("weave::tests::perf_merge_a_big_file") {
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

/// The front document's woven view.
fn shown(ed: &Editor) -> &Shown {
    of(ed.doc()).expect("a woven view in front")
}

/// Events until the woven view in front has had `n` merges land and none is under way.
fn woven_to(ed: &mut Editor, n: u64) {
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
    let Scene { mut ed, main, fix, docs, ids, dir } = scene("weave-clean");
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
    crate::typed::execute(&mut ed, "weave").unwrap();
    woven_to(&mut ed, 1);
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
    assert!(line("woven · a.rs").contains("fix unsaved · main · docs"), "{}", line("woven · a.rs"));
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
    woven_to(&mut ed, n + 1);
    assert!(!shown(&ed).sides[0].unsaved);
    assert_eq!(files(&main.join(".git")), before, "nothing written in the repository's git folder");
}

/// Another workspace's file as a link to a file outside (a stand-in secret in the test's own folder), a FIFO, a file
/// past `TEXT_MAX`, a binary one: the link never gone through, the FIFO never waited on, the others not taken — that
/// session left out and said on the path bar with why, every other side still merged (one copy can't blank the view).
#[test]
fn another_workspaces_link_fifo_or_huge_file_is_never_read() {
    let Scene { mut ed, main, docs, dir, .. } = scene("weave-link");
    std::fs::write(dir.join("secret.txt"), "SECRET-STAND-IN\n").unwrap();
    save_line(&mut ed, &main.join("a.rs"), 2, "two, main");
    std::fs::remove_file(docs.join("a.rs")).unwrap();
    std::os::unix::fs::symlink(dir.join("secret.txt"), docs.join("a.rs")).unwrap();
    ed.wrote(&docs.join("a.rs"));
    crate::typed::execute(&mut ed, "weave").unwrap();
    woven_to(&mut ed, 1);
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
    woven_to(&mut ed, n + 1);
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
    woven_to(&mut ed, n + 2);
    still(&ed, "too large");
    let r = rows(&frame(&mut ed, 160));
    assert!(r[0].contains("docs left out: too large"), "{}", r[0]);
    std::fs::write(docs.join("a.rs"), with(4, "four\0docs")).unwrap();
    ed.wrote(&docs.join("a.rs"));
    woven_to(&mut ed, n + 3);
    still(&ed, "binary");
}

/// The list's `v` on a session: the file where its change would conflict with yours (C2's overlap), at that
/// session's conflict — not the first one — and on a view open already, the cursor taken there too; the hint says it.
#[test]
fn the_lists_v_opens_the_overlaps_file_at_that_sessions_conflict() {
    let Scene { mut ed, main, docs, ids, dir: _dir, .. } = scene("weave-list-v");
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
        // (from its first row: a list's moves stop at its ends)
        press(ed, " Wg");
        for _ in 0..4 {
            if ed.sessions_here() == Some(id) {
                break;
            }
            press(ed, "j");
        }
    };
    to_row(&mut ed, ids[0]);
    let p = frame(&mut ed, 160);
    assert!(rows(&p).iter().any(|r| r.contains("v woven")), "the hint: {:#?}", rows(&p));
    press(&mut ed, "v");
    woven_to(&mut ed, 1);
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
    let Scene { mut ed, main, ids, dir: _dir, .. } = scene("weave-conflict");
    save_line(&mut ed, &main.join("a.rs"), 5, "five, main");
    type_line(&mut ed, 5, "five, fix");
    type_line(&mut ed, 10, "ten, fix");
    crate::typed::execute(&mut ed, "weave").unwrap();
    woven_to(&mut ed, 1);
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

/// The home checkout in front with its branch not read yet (a session just switched to): `:weave` isn't refused —
/// the merge reads the branch from its HEAD, and the view is named by it once it lands.
#[test]
fn a_branch_not_read_yet_is_read_with_the_merge() {
    let Scene { mut ed, main, docs, ids, dir: _dir, .. } = scene("weave-branch");
    assert!(ed.session_switch(ids[0]));
    ed.session.git_branch = None;
    ed.open(&main.join("a.rs")).unwrap();
    save_line(&mut ed, &docs.join("a.rs"), 4, "four, docs");
    crate::typed::execute(&mut ed, "weave").unwrap();
    woven_to(&mut ed, 1);
    let m = shown(&ed);
    assert_eq!((m.said.as_deref(), m.branch.as_deref()), (None, Some("main")));
    assert_eq!(m.sides.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["docs"]);
}

/// The woven view is read-only: an edit put back and insert mode left, said where you look; the buffer of your own
/// workspace untouched.
#[test]
fn the_woven_view_is_read_only() {
    let Scene { mut ed, main, fix, dir: _dir, .. } = scene("weave-readonly");
    save_line(&mut ed, &main.join("a.rs"), 2, "two, main");
    crate::typed::execute(&mut ed, "weave").unwrap();
    woven_to(&mut ed, 1);
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

/// `space v w` opens the current file's woven view, and from it again the same one; the session list's `v` on a
/// session opens the file it changed.
#[test]
fn space_v_w_and_the_lists_v_open_it() {
    let Scene { mut ed, docs, ids, dir: _dir, .. } = scene("weave-open");
    press(&mut ed, " vw");
    assert_eq!(shown(&ed).rel, "a.rs");
    let id = ed.doc().id;
    press(&mut ed, " vw");
    assert_eq!(ed.doc().id, id, "the one open already");
    // (a file named like a key: never merged — nobody's copy of it read)
    let e = crate::typed::execute(&mut ed, "weave server.pem").unwrap_err();
    assert!(e.contains("named like a key"), "{e}");
    woven_to(&mut ed, 1);
    assert!(shown(&ed).sides.is_empty(), "no session changed it");
    // (the list's `v`: docs's file — where it is, else the first it changed; none yet: said)
    let _ = docs;
    press(&mut ed, " Wg");
    let on = |ed: &Editor| ed.sessions_here() == Some(ids[2]);
    for _ in 0..4 {
        if !on(&ed) {
            press(&mut ed, "j");
        }
    }
    assert!(on(&ed), "docs's row in hand");
    press(&mut ed, "v");
    assert!(ed.messages.iter().any(|m| m.2.contains("nothing of it to weave yet")), "{:?}", ed.messages);
}

/// The woven view of `view` in front again, in fix's workspace (after an `enter` took you elsewhere).
fn back(ed: &mut Editor, fix: SessionId, view: crate::document::DocId) {
    if ed.session.id != fix {
        assert!(ed.session_switch(fix));
    }
    ed.current = ed.docs.iter().position(|d| d.id == view).expect("the view");
}

/// Where `enter` took you: the session, the file, the line.
fn went(ed: &Editor) -> (SessionId, PathBuf, usize) {
    (ed.session.id, ed.doc().path.clone().unwrap_or_default(), ed.cursor_line())
}

/// `enter` on a change's line: that session's workspace, its file at that line of its own text (not the woven view's
/// numbers — another's lines above it there); on a line nobody changed it says so and nothing moves; the path bar says
/// `enter go there`, the buffer list `woven ·`.
#[test]
fn enter_on_a_change_goes_to_its_workspace_at_its_own_line() {
    let Scene { mut ed, main, docs, ids, dir: _dir, .. } = scene("weave-enter");
    std::fs::write(docs.join("a.rs"), format!("new 1\nnew 2\n{}", twelve())).unwrap();
    ed.wrote(&docs.join("a.rs"));
    save_line(&mut ed, &main.join("a.rs"), 6, "six, main");
    crate::typed::execute(&mut ed, "weave").unwrap();
    woven_to(&mut ed, 1);
    let view = ed.doc().id;
    let r = rows(&frame(&mut ed, 160));
    let bar = r.iter().find(|l| l.contains("enter go there")).unwrap_or_else(|| panic!("{r:#?}"));
    assert!(bar.contains("woven · a.rs"), "{bar}");
    let at = ed.doc().text.to_string().lines().position(|l| l == "six, main").unwrap();
    assert_eq!(at, 7, "two of docs's lines above it here");
    ed.goto_line(at + 2);
    press(&mut ed, "<ret>");
    assert_eq!((ed.session.id, ed.doc().id), (ids[1], view), "nothing moved");
    assert!(ed.messages.iter().any(|m| m.2.contains("nobody changed this line")), "{:?}", ed.messages);
    ed.goto_line(at);
    press(&mut ed, "<ret>");
    assert_eq!(went(&ed), (ids[0], main.join("a.rs"), 5), "main's a.rs, its own line 6");
    // docs's: its second added line
    back(&mut ed, ids[1], view);
    ed.goto_line(1);
    press(&mut ed, "<ret>");
    assert_eq!(went(&ed), (ids[2], docs.join("a.rs"), 1));
}

/// A conflict: `enter` on a stacked part's line goes to that part's workspace at its own line, on the tip's part to the
/// checkout that has the base's branch at the tip's line — none has it: said; side by side, the column clicked is where
/// it goes (its wash a step stronger); a side whose workspace ended meanwhile: said, nothing moves.
#[test]
fn enter_on_a_conflicts_part_goes_to_whose_it_is() {
    let Scene { mut ed, main, docs, ids, dir: _dir, .. } = scene("weave-enter-part");
    // (docs's own line above it too: the parts' lines and the tip's are each their own text's)
    std::fs::write(docs.join("a.rs"), format!("new 1\n{}", twelve().replace("line 5\n", "five, docs\n")))
        .unwrap();
    ed.wrote(&docs.join("a.rs"));
    type_line(&mut ed, 5, "five, fix");
    crate::typed::execute(&mut ed, "weave").unwrap();
    woven_to(&mut ed, 1);
    let view = ed.doc().id;
    let c = shown(&ed).merge.conflicts[0].clone();
    let part = |ed: &Editor, name: &str| {
        let m = shown(ed);
        c.parts.iter().find(|p| m.sides[p.side].name == name).unwrap().lines.start
    };
    // Stacked: docs's part, then the tip's
    frame(&mut ed, 80);
    ed.goto_line(part(&ed, "docs"));
    press(&mut ed, "<ret>");
    assert_eq!(went(&ed), (ids[2], docs.join("a.rs"), 5));
    back(&mut ed, ids[1], view);
    ed.goto_line(c.base.start);
    assert_eq!(c.base.start, 5, "docs's line above it here");
    press(&mut ed, "<ret>");
    assert_eq!(
        went(&ed),
        (ids[0], main.join("a.rs"), 4),
        "the base's branch is main's checkout, its own line"
    );
    // Side by side: a click on docs's column picks it
    back(&mut ed, ids[1], view);
    ed.goto_line(c.base.start);
    let p = frame(&mut ed, 160);
    let before = cell_of(&p, "five, docs").bgcolor();
    let r = rows(&p);
    let y = r.iter().position(|l| l.contains("five, docs")).unwrap_or_else(|| panic!("{r:#?}"));
    let x = r[y][..r[y].find("five, docs").unwrap()].chars().count();
    for kind in [crate::event::MouseKind::Down, crate::event::MouseKind::Up] {
        let m = crate::event::Mouse { kind, x: x as u16, y: y as u16, alt: false, ctrl: false };
        ed.handle_event(crate::event::Event::Mouse(m));
    }
    assert_eq!(ed.cursor_line(), part(&ed, "docs"), "the cursor on its line");
    let p = frame(&mut ed, 160);
    assert_ne!(cell_of(&p, "five, docs").bgcolor(), before, "its wash a step stronger");
    assert!(rows(&p).iter().any(|l| l.contains("┬─ docs")), "still side by side");
    press(&mut ed, "<ret>");
    assert_eq!(went(&ed), (ids[2], docs.join("a.rs"), 5));
    // The tip's part where no checkout has the branch: said
    back(&mut ed, ids[1], view);
    ed.session_by_id(ids[0]).unwrap().git_branch = Some("elsewhere".into());
    ed.goto_line(c.base.start);
    press(&mut ed, "<ret>");
    assert_eq!((ed.session.id, ed.doc().id), (ids[1], view));
    assert!(ed.messages.iter().any(|m| m.2.contains("no workspace here has main")), "{:?}", ed.messages);
    // docs's workspace ended meanwhile (its side still in the last weave): said
    let _ = ed.session_close(&docs);
    ed.goto_line(part(&ed, "docs"));
    press(&mut ed, "<ret>");
    assert_eq!((ed.session.id, ed.doc().id), (ids[1], view));
    assert!(ed.messages.iter().any(|m| m.2.contains("docs's workspace has ended")), "{:?}", ed.messages);
}

/// A change that only takes lines out: a head where they went — whose (its prefix in the sign column, its name), how
/// many — on no line of its own; `]g` lands under it, `enter` there goes to that workspace where the lines were.
#[test]
fn a_removal_shows_its_head_and_enter_goes_there() {
    let Scene { mut ed, main, ids, dir: _dir, .. } = scene("weave-gone");
    let text: String = (1..=12).filter(|n| !(7..=8).contains(n)).map(|n| format!("line {n}\n")).collect();
    std::fs::write(main.join("a.rs"), text).unwrap();
    ed.wrote(&main.join("a.rs"));
    type_line(&mut ed, 2, "two, fix");
    crate::typed::execute(&mut ed, "weave").unwrap();
    woven_to(&mut ed, 1);
    for w in [80, 160] {
        let r = rows(&frame(&mut ed, w));
        let head =
            r.iter().position(|l| l.contains("main · 2 lines removed")).unwrap_or_else(|| panic!("{r:#?}"));
        assert!(r[head - 1].contains("line 6") && r[head + 1].contains("line 9"), "{r:#?}");
        let (sign, rule) = r[head].trim_start().split_once("─ main").unwrap();
        assert_eq!(sign.trim(), "m", "its prefix in the sign column, no number: {}", r[head]);
        assert!(rule.ends_with('─'), "{}", r[head]);
    }
    let p = frame(&mut ed, 80);
    let colour = ed.sessions_in_order().iter().find(|l| l.id == ids[0]).unwrap().colour;
    assert_eq!(cell_of(&p, "main · 2").fgcolor(), participant(&ed, colour), "its name in its colour");
    // Its row counted where the view scrolls: from the top of a short pane down past it, the cursor's line in sight
    let short = |ed: &mut Editor| {
        let mut bytes = Vec::new();
        crate::term::render(ed, &mut bytes, 80, 9).unwrap();
        let mut p = vt100::Parser::new(9, 80, 0);
        p.process(&bytes);
        rows(&p)
    };
    // (no margin: the line just in sight, its head's row above it counted)
    ed.config.scrolloff = 0;
    press(&mut ed, "gg");
    short(&mut ed);
    ed.goto_line(7);
    let r = short(&mut ed);
    assert!(r.iter().any(|l| l.contains("line 10")), "{r:#?}");
    press(&mut ed, "gg]g]g");
    assert_eq!(ed.cursor_line(), 6, "under the head: line 9's");
    press(&mut ed, "<ret>");
    assert_eq!(went(&ed), (ids[0], main.join("a.rs"), 6), "main's a.rs where the lines were");
}

/// The gutter's number on the row whose text ends `text` (none: no such row, or no number there).
fn number_at(r: &[String], text: &str) -> Option<usize> {
    let row = r.iter().find(|l| l.trim_end().ends_with(&format!(" {text}")))?;
    let before = &row[..row.trim_end().len() - text.len()];
    before.split_whitespace().last()?.trim_start_matches(|c: char| !c.is_ascii_digit()).parse().ok()
}

/// A woven view's numbers are where each line is, not its rows: a conflict's three parts all 5 (the tip's, fix's and
/// main's own line), the line after it 6 as everywhere; side by side each side's column says its own at its head; the
/// status line counts so; `:6` and `6G` go to the tip's line 6.
#[test]
fn a_woven_views_numbers_are_where_each_line_is() {
    let Scene { mut ed, main, dir: _dir, .. } = scene("weave-numbers");
    save_line(&mut ed, &main.join("a.rs"), 5, "five, main");
    type_line(&mut ed, 5, "five, fix");
    crate::typed::execute(&mut ed, "weave").unwrap();
    woven_to(&mut ed, 1);
    let r = rows(&frame(&mut ed, 80));
    let n = |t: &str| number_at(&r, t);
    assert_eq!((n("line 5"), n("five, fix"), n("five, main")), (Some(5), Some(5), Some(5)), "{r:#?}");
    assert_eq!((n("line 4"), n("line 6"), n("line 12")), (Some(4), Some(6), Some(12)), "{r:#?}");
    let at = |ed: &Editor, t: &str| ed.doc().text.to_string().lines().position(|l| l == t).unwrap();
    ed.goto_line(at(&ed, "five, main"));
    let r = rows(&frame(&mut ed, 80));
    assert!(r[29].contains(" 5:1"), "the status line: {}", r[29]);
    // Side by side: the tip's number in the gutter, each side's at its column's head
    let r = rows(&frame(&mut ed, 160));
    let head = r.iter().find(|l| l.contains("conflict 1 of 1")).unwrap();
    assert!(head.trim_start().starts_with("≈ 5 ─ conflict"), "the tip's number in the gutter: {head}");
    assert!(head.contains("fix 5 ─") && head.contains("main 5 ─"), "{head}");
    assert_eq!(number_at(&r, "line 6"), Some(6), "{r:#?}");
    // `:6`, `6G`: the tip's line 6 — past the conflict, not the view's sixth row
    crate::typed::execute(&mut ed, "6").unwrap();
    assert_eq!(ed.cursor_line(), at(&ed, "line 6"));
    press(&mut ed, "gg6G");
    assert_eq!(ed.cursor_line(), at(&ed, "line 6"));
    crate::typed::execute(&mut ed, "5").unwrap();
    assert_eq!(ed.cursor_line(), at(&ed, "line 5"), "the tip's line 5: the conflict's first part");
}

/// A side behind the tip: its change laid on the tip, numbered by its own lines (one fewer than the tip's there — the
/// tip has a line above it docs hasn't), the tip's lines by the tip's.
#[test]
fn a_side_behinds_change_is_numbered_by_its_own_lines() {
    let Scene { mut ed, main, docs, dir: _dir, .. } = scene("weave-numbers-behind");
    std::fs::write(main.join("a.rs"), format!("new 0\n{}", twelve())).unwrap();
    git(&main, &["commit", "-qam", "a line above"]);
    ed.wrote(&main.join("a.rs"));
    save_line(&mut ed, &docs.join("a.rs"), 9, "nine, docs");
    crate::typed::execute(&mut ed, "weave").unwrap();
    woven_to(&mut ed, 1);
    assert_eq!(shown(&ed).sides.iter().find(|s| s.name == "docs").and_then(|s| s.behind), Some(1));
    let r = rows(&frame(&mut ed, 80));
    let n = |t: &str| number_at(&r, t);
    assert_eq!((n("line 8"), n("nine, docs"), n("line 10")), (Some(9), Some(9), Some(11)), "{r:#?}");
}

/// A conflict part with no lines (a side took them out) shows side by side as an empty column: a click on it is where
/// `enter` goes — that side's workspace where the lines were — while the cursor stays where the click put it. A
/// ctrl+click there is the same pick (no `gd`: the column is what it says).
#[test]
fn an_empty_column_is_picked_by_its_click() {
    let Scene { mut ed, docs, ids, dir: _dir, .. } = scene("weave-enter-empty");
    std::fs::write(docs.join("a.rs"), twelve().replace("line 5\n", "")).unwrap();
    ed.wrote(&docs.join("a.rs"));
    type_line(&mut ed, 5, "five, fix");
    crate::typed::execute(&mut ed, "weave").unwrap();
    woven_to(&mut ed, 1);
    let view = ed.doc().id;
    let c = shown(&ed).merge.conflicts[0].clone();
    let p = frame(&mut ed, 160);
    let r = rows(&p);
    let y = r.iter().position(|l| l.contains("┬─ docs")).unwrap_or_else(|| panic!("{r:#?}"));
    let x = r[y][..r[y].find("┬─ docs").unwrap()].chars().count() + 4;
    for kind in [crate::event::MouseKind::Down, crate::event::MouseKind::Up] {
        let m = crate::event::Mouse { kind, x: x as u16, y: (y + 1) as u16, alt: false, ctrl: true };
        ed.handle_event(crate::event::Event::Mouse(m));
    }
    assert_eq!(ed.cursor_line(), c.base.start, "no line of its own: the conflict's first");
    assert_eq!(ed.doc().id, view, "ctrl+click on a column: a pick, nothing gone to");
    press(&mut ed, "<ret>");
    assert_eq!(went(&ed), (ids[2], docs.join("a.rs"), 4), "docs's a.rs where line 5 was");
    // Used, the pick is gone: the tip's part again
    back(&mut ed, ids[1], view);
    press(&mut ed, "<ret>");
    assert_eq!(ed.session.id, ids[0], "the tip's part: main's checkout");
}

/// A click, `x` the cell and `y` the row.
fn click(ed: &mut Editor, x: usize, y: usize) {
    for kind in [crate::event::MouseKind::Down, crate::event::MouseKind::Up] {
        let m = crate::event::Mouse { kind, x: x as u16, y: y as u16, alt: false, ctrl: false };
        ed.handle_event(crate::event::Event::Mouse(m));
    }
}

/// `enter` opens a workspace's file only as the weave reads it: a link planted at the tip's checkout (its copy left
/// out of the weave) or swapped in at a session's since the weave is refused and said — nothing moves, the link's
/// target in no buffer; and a buffer whose file became a link isn't that side's text.
#[test]
fn enter_never_goes_through_a_link() {
    let Scene { mut ed, main, fix, docs, ids, dir } = scene("weave-enter-link");
    std::fs::write(dir.join("secret.txt"), "SECRET-STAND-IN\n").unwrap();
    let link = |path: &Path| {
        std::fs::remove_file(path).unwrap();
        std::os::unix::fs::symlink(dir.join("secret.txt"), path).unwrap();
    };
    save_line(&mut ed, &docs.join("a.rs"), 5, "five, docs");
    type_line(&mut ed, 5, "five, fix");
    link(&main.join("a.rs"));
    ed.wrote(&main.join("a.rs"));
    crate::typed::execute(&mut ed, "weave").unwrap();
    woven_to(&mut ed, 1);
    let view = ed.doc().id;
    assert_eq!(shown(&ed).left, [("main".to_string(), "not a plain file")]);
    let c = shown(&ed).merge.conflicts[0].clone();
    let refused = |ed: &Editor, n: usize, what: &str| {
        assert_eq!((ed.session.id, ed.doc().id), (ids[1], view), "{what}: nothing moved");
        let said = ed.messages.iter().filter(|m| m.2.contains("isn't a plain file — not opened")).count();
        assert_eq!(said, n, "{what}: said {:?}", ed.messages);
        assert!(!ed.docs.iter().any(|d| d.text.to_string().contains("SECRET")), "{what}: in no buffer");
    };
    // The tip's part: main's checkout, where a.rs is a link
    ed.goto_line(c.base.start);
    press(&mut ed, "<ret>");
    refused(&ed, 1, "the tip's part");
    // docs's part, its file a link since the weave (nothing said of it)
    link(&docs.join("a.rs"));
    let docs_part = c.parts.iter().find(|p| shown(&ed).sides[p.side].name == "docs").unwrap().lines.start;
    ed.goto_line(docs_part);
    press(&mut ed, "<ret>");
    refused(&ed, 2, "swapped in since");
    // fix's buffer (unsaved), its file a link now: left out as its disk copy would be
    link(&fix.join("a.rs"));
    ed.wrote(&fix.join("a.rs"));
    let n = shown(&ed).merges;
    woven_to(&mut ed, n + 1);
    let m = shown(&ed);
    assert!(m.left.contains(&("fix".to_string(), "not a plain file")), "{:?}", m.left);
    assert!(!m.sides.iter().any(|s| s.name == "fix"), "its buffer not that side's text");
    assert!(!ed.doc().text.to_string().contains("SECRET"));
}

/// The view starting inside a side by side conflict (its head and first row above the screen): a click on a column is
/// that row of the part — counted from the conflict's head, not from the first row in sight — and `enter` goes to that
/// line of its file.
#[test]
fn a_click_in_a_conflict_the_view_starts_inside_is_its_own_row() {
    let Scene { mut ed, main, fix, ids, dir: _dir, .. } = scene("weave-click-scrolled");
    for (n, w) in [(5, "five"), (6, "six"), (7, "seven")] {
        save_line(&mut ed, &main.join("a.rs"), n, &format!("{w}, main"));
        type_line(&mut ed, n, &format!("{w}, fix"));
    }
    crate::typed::execute(&mut ed, "weave").unwrap();
    woven_to(&mut ed, 1);
    let c = shown(&ed).merge.conflicts[0].clone();
    // (the cursor on the line after it, in sight; the view from the conflict's third row)
    ed.config.scrolloff = 0;
    ed.goto_line(c.lines().end);
    let doc = ed.doc_mut();
    (doc.top, doc.top_rows) = (c.base.start, Some((c.base.start, 2)));
    let r = rows(&frame(&mut ed, 160));
    assert!(!r.iter().any(|l| l.contains("five, fix") || l.contains("conflict 1 of 1")), "inside it: {r:#?}");
    let y = r.iter().position(|l| l.contains("six, fix")).unwrap_or_else(|| panic!("{r:#?}"));
    click(&mut ed, r[y][..r[y].find("six, fix").unwrap()].chars().count(), y);
    let six = ed.doc().text.to_string().lines().position(|l| l == "six, fix").unwrap();
    assert_eq!(ed.cursor_line(), six, "the row clicked, not the part's first");
    press(&mut ed, "<ret>");
    assert_eq!(went(&ed), (ids[1], fix.join("a.rs"), 5), "fix's own line 6");
}
