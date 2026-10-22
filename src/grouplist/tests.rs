//! The task's and the preflight's groups (panes P3 §2.2): keys in sequence on runs with no process behind them
//! (`Run::for_test`), a frame after each.

use crate::config::Config;
use crate::editor::Editor;
use crate::key::{Code, Key};
use crate::pane::GroupKind;
use crate::session::At;
use crate::sizes::Slot;

const W: u16 = 120;
const H: u16 = 34;

/// A frame, after the event's `focus_sync` as the loop runs it (the groups made, the slot's front).
fn frame(ed: &mut Editor) -> String {
    cells(ed, W).screen().contents()
}

/// A frame `w` wide, its cells.
fn cells(ed: &mut Editor, w: u16) -> vt100::Parser {
    ed.focus_sync();
    let mut bytes = Vec::new();
    crate::term::render(ed, &mut bytes, w, H).unwrap();
    let mut p = vt100::Parser::new(H, w, 0);
    p.process(&bytes);
    p
}

/// Keys as `run` takes them (`<C-w>`, `<S-tab>`), a frame after each.
fn feed(ed: &mut Editor, keys: &str) {
    feed_at(ed, keys, W);
}

/// `feed` on a screen `w` wide.
fn feed_at(ed: &mut Editor, keys: &str, w: u16) {
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let key: Key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        };
        ed.handle_key(key);
        cells(ed, w);
    }
}

fn region(ed: &Editor) -> String {
    ed.keys_to().region.name()
}

fn said(ed: &Editor) -> String {
    ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default()
}

/// The task output's cursor line.
fn cursor(ed: &Editor) -> usize {
    crate::rundoc::cursor_line(ed, Slot::Task).expect("the task panel's output")
}

/// The task panel's lines in sight, as the run's scroll shows them without the keys.
fn in_sight(ed: &Editor) -> std::ops::Range<usize> {
    let rows = crate::rundoc::rows(ed, Slot::Task);
    let top = ed.session.task_run.as_ref().unwrap().top(rows);
    top..top + rows
}

/// A place in `path`, line `line`, of `severity`.
fn place(path: &std::path::Path, line: usize, severity: u8, message: &str) -> crate::quickfix::Item {
    crate::quickfix::Item {
        path: path.to_path_buf(),
        line,
        col: 0,
        message: message.into(),
        severity,
        mark: None,
    }
}

/// An editor in `root` whose task panel shows a run that ended.
fn editor(root: &std::path::Path) -> Editor {
    editor_of(root, "check")
}

/// `editor`, the task named `name` (its tab's label).
fn editor_of(root: &std::path::Path, name: &str) -> Editor {
    let mut ed = Editor::new(Config::default());
    ed.session.root = root.to_path_buf();
    ed.docs[0].text = ropey::Rope::from_str("fn main() {}\n");
    let t = crate::task::Task { name: name.into(), cmd: "cargo check".into(), cwd: root.to_path_buf() };
    let mut r = crate::task::Run::for_test(t);
    r.state = crate::task::State::Passed;
    ed.session.task_run = Some(r);
    ed
}

/// `n` lines of filler, then a place's line (`src/x.rs:L: warning: ...`), recorded as the run records it.
fn print(ed: &mut Editor, filler: usize, it: crate::quickfix::Item) {
    let r = ed.session.task_run.as_mut().unwrap();
    for i in 0..filler {
        r.push_line(false, format!("filler {i}"), false);
    }
    r.push_line(true, format!("{}:{}: {}", it.path.display(), it.line + 1, it.message), false);
    r.place(it, None);
}

/// The task's places beside its output (§2.2, decision 12): one place is the output alone; two, a list — the errors
/// first — whose selection scrolls the output to the line that printed it and selects it (`tab` lands there);
/// the list grows as the run prints, its selection staying on its place; `enter` goes to the place in the code.
#[test]
fn a_tasks_places_list_beside_its_output_drives_it() {
    let dir = crate::scratch::Scratch::new("group-task");
    let (a, b) = (dir.path().join("a.rs"), dir.path().join("b.rs"));
    std::fs::write(&a, "fn a() {}\n".repeat(20)).unwrap();
    std::fs::write(&b, "fn b() {}\n".repeat(20)).unwrap();
    let mut ed = editor(dir.path());
    print(&mut ed, 3, place(&a, 4, 2, "unused variable"));
    frame(&mut ed);
    // One place: the output alone, no list to hop to
    assert!(ed.slot_group().is_none(), "one place: one pane");
    assert!(ed.screen.list(GroupKind::Task).is_none());
    ed.list_focus(GroupKind::Task);
    assert!(said(&ed).contains("fewer than two places"), "{}", said(&ed));
    assert_eq!(ed.session.at, At::Code);
    // Two — the error printed far below: a list, the error's row first
    print(&mut ed, 60, place(&b, 9, 1, "mismatched types"));
    let shown = frame(&mut ed);
    let g = ed.slot_group().expect("the task's group laid out");
    assert_eq!(g.kind, GroupKind::Task);
    let rows = list_rows(&shown);
    assert!(rows.len() == 2 && rows[0].contains("b.rs:10") && rows[1].contains("a.rs:5"), "{rows:?}");
    assert!(shown.contains("› places ") && shown.contains("┬─ output "), "{shown}");
    // The list's keys (`S-tab` from the output), on the place found first — the warning, second row
    feed(&mut ed, "<C-w>j");
    if region(&ed) != "list:task" {
        feed(&mut ed, "<S-tab>");
    }
    assert_eq!(region(&ed), "list:task");
    assert_eq!(ed.session.task_run.as_ref().unwrap().picked, 0);
    // `k`: the error — the output at the line that printed it (row 64), that line selected
    feed(&mut ed, "k");
    let r = ed.session.task_run.as_ref().unwrap();
    assert_eq!(r.picked, 1, "the error, by its place");
    let line = r.origin(1).line.unwrap();
    let printed = r.output.line(line).map(|l| l.to_string());
    assert_eq!(printed, Some(format!("{}:10: mismatched types", b.display())));
    assert_eq!(cursor(&ed), line, "that line selected");
    assert!(in_sight(&ed).contains(&line), "in sight: {:?} {line}", in_sight(&ed));
    assert!(!in_sight(&ed).contains(&0), "scrolled there");
    // `j`: the warning again, its line (near the top)
    feed(&mut ed, "j");
    let warning = ed.session.task_run.as_ref().unwrap().origin(0).line.unwrap();
    assert_eq!((cursor(&ed), in_sight(&ed).contains(&warning)), (warning, true));
    // The list grows while the run prints — an error, listed before the warning: the selection stays on its place
    print(&mut ed, 2, place(&a, 1, 1, "cannot find value"));
    let shown = frame(&mut ed);
    assert_eq!(ed.session.task_run.as_ref().unwrap().picked, 0, "kept by identity");
    let rows = list_rows(&shown);
    assert!(
        rows.len() == 3 && rows[2].contains("a.rs:5") && rows[2].starts_with('▎'),
        "moved, the bar with it: {rows:?}"
    );
    // `tab`: the output's keys, its cursor on that line
    feed(&mut ed, "<tab>");
    assert_eq!(region(&ed), "output:task");
    assert_eq!(cursor(&ed), warning, "the keys land on the place's line");
    // `S-tab` back, `k` to the error, `enter`: the place in the code, the code's keys
    feed(&mut ed, "<S-tab>");
    assert_eq!(region(&ed), "list:task");
    feed(&mut ed, "gg<ret>");
    assert_eq!(ed.session.at, At::Code);
    assert_eq!(ed.doc().path.as_deref(), Some(b.as_path()));
    assert_eq!(ed.cursor_line(), 9, "on its line");
}

/// A link to the output's first line holds when the keys come (`tab`): a place printed on the run's first line, a
/// preflight's first heading — the cursor stays there, not on the last row in sight (a fresh document's cursor is on
/// line 0 too, so the line alone can't say it was placed).
#[test]
fn a_link_to_the_first_line_holds_when_the_keys_come() {
    let dir = crate::scratch::Scratch::new("group-line0");
    let a = dir.path().join("a.rs");
    std::fs::write(&a, "fn a() {}\n").unwrap();
    // A place on the run's first line, another far below
    let mut ed = editor(dir.path());
    print(&mut ed, 0, place(&a, 0, 2, "first"));
    print(&mut ed, 60, place(&a, 0, 2, "later"));
    frame(&mut ed);
    ed.list_focus(GroupKind::Task);
    feed(&mut ed, "jk");
    assert_eq!(ed.session.task_run.as_ref().unwrap().picked, 0);
    assert_eq!(cursor(&ed), 0, "the link: its line, the first");
    feed(&mut ed, "<tab>");
    assert_eq!(region(&ed), "output:task");
    assert_eq!(cursor(&ed), 0, "the keys land on it");
    // A preflight's first step: its heading is the output's first line
    let step = |n: &str| crate::preflight::Step {
        name: n.into(),
        cmd: String::new(),
        note: None,
        from: String::new(),
        full: false,
    };
    let mut ed = editor(dir.path());
    let p = crate::preflight::Preflight::new(
        0,
        &[step("fmt"), step("test")],
        crate::preflight::Set::Full,
        "k".into(),
    );
    ed.session.preflight = Some(p);
    let r = ed.session.task_run.as_mut().unwrap();
    for name in ["fmt", "test"] {
        r.push_line(false, format!("── {name} ──"), true);
        (0..40).for_each(|i| r.push_line(false, format!("{name} {i}"), false));
    }
    ed.session
        .preflight
        .as_mut()
        .unwrap()
        .steps
        .iter_mut()
        .for_each(|s| s.state = crate::preflight::State::Passed);
    frame(&mut ed);
    ed.list_focus(GroupKind::Preflight);
    feed(&mut ed, "jk<tab>");
    assert_eq!(region(&ed), "output:preflight");
    assert_eq!(cursor(&ed), 0, "on `── fmt ──`");
}

/// A press on a row of the task's places: the list's keys, that place picked, the output at its line.
#[test]
fn a_click_on_a_places_row_picks_it() {
    let dir = crate::scratch::Scratch::new("group-click");
    let a = dir.path().join("a.rs");
    std::fs::write(&a, "fn a() {}\n").unwrap();
    let mut ed = editor(dir.path());
    print(&mut ed, 2, place(&a, 0, 2, "first"));
    print(&mut ed, 60, place(&a, 4, 2, "second"));
    frame(&mut ed);
    assert_eq!(ed.session.at, At::Code);
    let (y, rows, first) = ed.screen.list(GroupKind::Task).expect("the places' list drawn");
    assert!(rows >= 2 && first == 0);
    let g = ed.session.group(GroupKind::Task).unwrap();
    let list = ed.screen.leaves.iter().find(|l| l.0 == g.list).unwrap().1;
    let click = crate::event::Mouse {
        kind: crate::event::MouseKind::Down,
        x: (list.x + 4) as u16,
        y: (y + 1) as u16,
        alt: false,
    };
    ed.handle_mouse(click);
    frame(&mut ed);
    assert_eq!(region(&ed), "list:task");
    let r = ed.session.task_run.as_ref().unwrap();
    assert_eq!(r.picked, 1, "the second row's place");
    assert_eq!(cursor(&ed), r.origin(1).line.unwrap(), "the output at its line");
}

/// `tab` and `S-tab` go round the slot's groups member by member (P3 step 4: onto a group going forward its list,
/// going back its detail) — the task's places and the tests' results, both tabs.
#[test]
fn tab_and_s_tab_go_round_each_groups_members() {
    let dir = crate::scratch::Scratch::new("group-round");
    let a = dir.path().join("a.rs");
    std::fs::write(&a, "fn a() {}\n").unwrap();
    let mut ed = editor(dir.path());
    print(&mut ed, 1, place(&a, 0, 1, "one"));
    print(&mut ed, 1, place(&a, 0, 2, "two"));
    let case = crate::test_results::Case {
        name: "adds".into(),
        status: crate::test_results::Status::Passed,
        secs: None,
        message: Vec::new(),
        at: None,
    };
    let run = crate::testing::TestRun::finished("t", vec![case.clone(), case], vec!["ok".into()]);
    ed.session.test_run = Some(run);
    frame(&mut ed);
    ed.list_focus(GroupKind::Task);
    frame(&mut ed);
    let mut round = vec![region(&ed)];
    for _ in 0..4 {
        feed(&mut ed, "<tab>");
        round.push(region(&ed));
    }
    assert_eq!(round, ["list:task", "output:task", "list:tests", "output:tests", "list:task"], "forward");
    let mut back = Vec::new();
    for _ in 0..4 {
        feed(&mut ed, "<S-tab>");
        back.push(region(&ed));
    }
    assert_eq!(
        back,
        ["output:tests", "list:tests", "output:task", "list:task"],
        "back: onto a group, its detail"
    );
}

/// The status line's word with a list's keys names the surface as its header does (`● preflight` on a preflight's
/// steps, not the task panel's word; `● task`, `● tests`).
#[test]
fn a_lists_status_word_is_its_headers() {
    let dir = crate::scratch::Scratch::new("group-word");
    let a = dir.path().join("a.rs");
    std::fs::write(&a, "fn a() {}\n").unwrap();
    let status = |shown: &str| shown.lines().last().unwrap_or_default().trim().to_string();
    // The task's places
    let mut ed = editor(dir.path());
    print(&mut ed, 1, place(&a, 0, 1, "one"));
    print(&mut ed, 1, place(&a, 0, 2, "two"));
    frame(&mut ed);
    ed.list_focus(GroupKind::Task);
    let shown = frame(&mut ed);
    assert_eq!(region(&ed), "list:task");
    assert!(status(&shown).starts_with("● task"), "{}", status(&shown));
    // A preflight's steps
    let step = |n: &str| crate::preflight::Step {
        name: n.into(),
        cmd: String::new(),
        note: None,
        from: String::new(),
        full: false,
    };
    let mut ed = editor(dir.path());
    let p = crate::preflight::Preflight::new(
        0,
        &[step("fmt"), step("test")],
        crate::preflight::Set::Full,
        "k".into(),
    );
    ed.session.preflight = Some(p);
    frame(&mut ed);
    ed.list_focus(GroupKind::Preflight);
    let shown = frame(&mut ed);
    assert_eq!(region(&ed), "list:preflight");
    assert!(status(&shown).starts_with("● preflight"), "{}", status(&shown));
    // The tests' results
    let mut ed = editor(dir.path());
    let case = crate::test_results::Case {
        name: "adds".into(),
        status: crate::test_results::Status::Passed,
        secs: None,
        message: Vec::new(),
        at: None,
    };
    let run = crate::testing::TestRun::finished("t", vec![case.clone(), case], vec!["ok".into()]);
    ed.session.test_run = Some(run);
    frame(&mut ed);
    ed.list_focus(GroupKind::Tests);
    let shown = frame(&mut ed);
    assert_eq!(region(&ed), "list:tests");
    assert!(status(&shown).starts_with("● tests"), "{}", status(&shown));
}

/// The list's rows in a frame: the slot's rows with a place's mark, cut at the list's line.
fn list_rows(shown: &str) -> Vec<String> {
    let rows = shown.lines().filter(|l| l.chars().take(4).any(|c| c == '▲' || c == '◦'));
    rows.map(|l| l.chars().take_while(|&c| c != '│').collect::<String>().trim_end().to_string()).collect()
}

/// A trim (`Run::keep`) moves a place's line with what stays; a line cut takes it — the list says `cut` and the
/// link says why the output can't show it.
#[test]
fn a_places_line_follows_a_trim_or_is_said_cut() {
    let dir = crate::scratch::Scratch::new("group-trim");
    let a = dir.path().join("a.rs");
    std::fs::write(&a, "fn a() {}\n").unwrap();
    let mut ed = editor(dir.path());
    // (a place in the step's head — kept; one in its middle — cut; one at its end — moved up)
    print(&mut ed, 10, place(&a, 0, 1, "head"));
    print(&mut ed, 1000, place(&a, 0, 1, "middle"));
    print(&mut ed, 4000, place(&a, 0, 1, "end"));
    let r = ed.session.task_run.as_mut().unwrap();
    let before = (r.origin(0).line, r.origin(2).line.unwrap());
    r.keep(0);
    let text = |r: &crate::task::Run, i: usize| {
        r.origin(i).line.and_then(|l| r.output.line(l)).map(|l| l.to_string()).unwrap_or_default()
    };
    assert_eq!(r.origin(0).line, before.0, "before the cut: where it was");
    assert_eq!(r.origin(1).line, None, "in what went: cut");
    assert!(r.origin(2).line.unwrap() < before.1, "after it: moved up");
    assert!(text(r, 2).ends_with(": end") && text(r, 0).ends_with(": head"), "{:?}", text(r, 2));
    let shown = frame(&mut ed);
    let rows = list_rows(&shown);
    assert!(rows.iter().any(|l| l.contains("middle") && l.ends_with("cut")), "{rows:?}");
    assert!(rows.iter().filter(|l| l.ends_with("cut")).count() == 1, "{rows:?}");
    ed.list_focus(GroupKind::Task);
    feed(&mut ed, "j");
    assert_eq!(ed.session.task_run.as_ref().unwrap().picked, 1);
    assert!(said(&ed).contains("cut from the output"), "{}", said(&ed));
}

/// A preflight's steps beside its one output (§2.2, decision 5): a step selected puts its heading at the top; `enter`
/// goes to its first place, else the output's keys at its heading; `o` on the output selects the step in view, `o`
/// on the list opens its step whole in a buffer; one step is the output alone, its chip row as before.
#[test]
fn a_preflights_steps_list_beside_its_output_drives_it() {
    let dir = crate::scratch::Scratch::new("group-preflight");
    let a = dir.path().join("a.rs");
    std::fs::write(&a, "fn a() {}\n".repeat(10)).unwrap();
    let mut ed = editor(dir.path());
    let names = ["fmt", "clippy", "test"];
    let steps: Vec<crate::preflight::Step> = names
        .iter()
        .map(|n| crate::preflight::Step {
            name: n.to_string(),
            cmd: String::new(),
            note: None,
            from: String::new(),
            full: false,
        })
        .collect();
    let mut p = crate::preflight::Preflight::new(0, &steps, crate::preflight::Set::Full, "k".into());
    p.steps.iter_mut().for_each(|s| s.state = crate::preflight::State::Passed);
    p.steps[2].state = crate::preflight::State::Failed(101);
    ed.session.preflight = Some(p);
    let r = ed.session.task_run.as_mut().unwrap();
    for (k, name) in names.iter().enumerate() {
        r.push_line(false, format!("── {name} ──"), true);
        for i in 0..30 {
            r.push_line(false, format!("{name} line {i}"), false);
        }
        // (clippy's one place)
        if k == 1 {
            r.push_line(true, format!("{}:7: warning: unused", a.display()), false);
            r.place(place(&a, 6, 2, "unused"), Some(1));
        }
    }
    frame(&mut ed);
    let g = ed.slot_group().expect("the preflight's group");
    assert_eq!(g.kind, GroupKind::Preflight);
    ed.list_focus(GroupKind::Preflight);
    let shown = frame(&mut ed);
    assert!(shown.contains("› steps ") && ed.screen.task_steps.borrow().is_empty(), "a list, no chip row");
    // `j`: clippy — its heading at the top of the output
    feed(&mut ed, "j");
    let heading = |ed: &Editor| {
        let d = ed.session.outputs.doc(Slot::Task).unwrap();
        let top = ed.session.task_run.as_ref().unwrap().top(crate::rundoc::rows(ed, Slot::Task));
        d.text.line(top).to_string().trim_end().to_string()
    };
    assert_eq!(heading(&ed), "── clippy ──");
    // `enter`: its first place, in the code
    feed(&mut ed, "<ret>");
    assert_eq!((ed.session.at, ed.doc().path.as_deref(), ed.cursor_line()), (At::Code, Some(a.as_path()), 6));
    // fmt has none: `enter` gives the output the keys at its heading
    ed.list_focus(GroupKind::Preflight);
    feed(&mut ed, "k<ret>");
    assert_eq!(region(&ed), "output:preflight");
    let d = ed.session.outputs.doc(Slot::Task).unwrap();
    assert_eq!(d.text.line(cursor(&ed)).to_string().trim_end(), "── fmt ──");
    // `o` on the output: the step in view selected in the list, the list's keys
    feed(&mut ed, "G");
    feed(&mut ed, "o");
    assert_eq!(region(&ed), "list:preflight");
    assert_eq!(ed.session.preflight.as_ref().unwrap().selected, 2, "test, at the output's end");
    // `o` on the list: that step whole, in a buffer
    feed(&mut ed, "o");
    assert!(ed.doc().title.as_deref().is_some_and(|t| t.ends_with("· test")), "{}", said(&ed));
    // One step: the output alone, its chip row
    let mut ed = editor(dir.path());
    let one = crate::preflight::Preflight::new(0, &steps[..1], crate::preflight::Set::Full, "k".into());
    ed.session.preflight = Some(one);
    ed.session.task_run.as_mut().unwrap().push_line(false, "── fmt ──".into(), true);
    frame(&mut ed);
    assert!(ed.slot_group().is_none(), "one step: one pane");
    assert!(!ed.screen.task_steps.borrow().is_empty(), "its chip");
}

/// A group of each kind in the slot, its list's two rows first: the task's places (`two` an error, listed first),
/// a preflight's steps (`fmt`, `test`), the tests' results (a pass, a failure in `src/lib.rs` picked).
fn each_kind(root: &std::path::Path) -> Vec<(Editor, GroupKind, &'static str, &'static str, &'static str)> {
    let a = root.join("a.rs");
    let mut task = editor(root);
    print(&mut task, 3, place(&a, 0, 2, "one"));
    print(&mut task, 3, place(&a, 2, 1, "two"));
    let step = |n: &str| crate::preflight::Step {
        name: n.into(),
        cmd: String::new(),
        note: None,
        from: String::new(),
        full: false,
    };
    let mut preflight = editor_of(root, "preflight");
    let mut p = crate::preflight::Preflight::new(
        0,
        &[step("fmt"), step("test")],
        crate::preflight::Set::Full,
        "k".into(),
    );
    p.steps.iter_mut().for_each(|s| s.state = crate::preflight::State::Passed);
    preflight.session.preflight = Some(p);
    let r = preflight.session.task_run.as_mut().unwrap();
    for name in ["fmt", "test"] {
        r.push_line(false, format!("── {name} ──"), true);
        (0..5).for_each(|i| r.push_line(false, format!("{name} {i}"), false));
    }
    let mut tests = editor(root);
    let case = |name: &str, status, at| crate::test_results::Case {
        name: name.into(),
        status,
        secs: None,
        message: vec!["assertion failed".into()],
        at,
    };
    use crate::test_results::Status;
    // (names as long as the demo's: the list wide enough for its name and the counts)
    let cases = vec![
        case("tests::length_sums_the_knots", Status::Passed, None),
        case("tests::a_tied_knot_lengthens_it", Status::Failed, Some(("src/lib.rs".into(), 30))),
    ];
    let mut run = crate::testing::TestRun::finished("t", cases, vec!["ok".into()]);
    run.selected = 1;
    tests.session.test_run = Some(run);
    tests.session.task_run = None;
    vec![
        (task, GroupKind::Task, "places", "output", "a.rs:3"),
        (preflight, GroupKind::Preflight, "steps", "output", "fmt"),
        (tests, GroupKind::Tests, "results", "failure", "length_sums_the_knots"),
    ]
}

/// Over and under (user, 2026-10-23): each kind docked right of the code (tall, 64 wide) lays its list over its detail
/// — the list's first row under the group's line, the detail's own top line after the list's rows naming it, the
/// detail under that; `:procs`' table and its detail too.
#[test]
fn every_kind_lays_over_and_under_docked_tall() {
    let dir = crate::scratch::Scratch::new("group-over");
    std::fs::write(dir.path().join("a.rs"), "fn a() {}\n".repeat(5)).unwrap();
    let mut all = each_kind(dir.path());
    let mut procs = editor(dir.path());
    procs.session.task_run = None;
    procs.procs_toggle();
    all.push((procs, GroupKind::Procs, "processes", "detail", "PID"));
    for (mut ed, kind, _, detail, first) in all {
        cells(&mut ed, 160);
        ed.dock(kind, crate::pane::Side::Right);
        cells(&mut ed, 160);
        let p = cells(&mut ed, 160);
        let g = ed.session.group(kind).unwrap();
        let leaf = |id| ed.screen.leaves.iter().find(|l| l.0 == id).map(|l| l.1).unwrap_or_default();
        let (l, d) = (leaf(g.list), leaf(g.detail_pane().unwrap()));
        assert!(l.w > 0 && (l.x, l.w) == (d.x, d.w) && d.y == l.y + l.h + 1, "{kind:?}: {l:?} {d:?}");
        let line: String = row(&p, (d.y - 1) as u16).into_iter().skip(l.x).collect();
        assert!(line.starts_with(&format!("─ {detail} ")), "{kind:?}: its top line: {line}");
        let top: String = row(&p, l.y as u16).into_iter().skip(l.x).collect();
        assert!(top.contains(first), "{kind:?}: the list's first row: {top}");
    }
}

/// A frame's row `y`, a char a cell.
fn row(p: &vt100::Parser, y: u16) -> Vec<char> {
    let w = p.screen().size().1;
    (0..w)
        .map(|x| p.screen().cell(y, x).map_or(' ', |c| c.contents().chars().next().unwrap_or(' ')))
        .collect()
}

/// The column `name` starts at in `row`, after column `from`.
fn column(row: &[char], name: &str, from: usize) -> Option<usize> {
    let n: Vec<char> = name.chars().collect();
    (from..row.len().saturating_sub(n.len())).find(|&x| row[x..x + n.len()] == n[..])
}

/// The slot's line (the row naming the group's members after ` › `) and, laid side by side, the column of the line
/// between its members.
fn slot_line(ed: &Editor, p: &vt100::Parser, kind: GroupKind) -> (u16, Option<usize>) {
    let h = p.screen().size().0;
    let y = (0..h)
        .find(|&y| row(p, y)[0] == '─' && column(&row(p, y), " › ", 0).is_some())
        .expect("the slot's line");
    let g = ed.session.group(kind).unwrap();
    let leaf = |id| ed.screen.leaves.iter().find(|l| l.0 == id).map(|l| l.1);
    let split = leaf(g.list).zip(g.detail_pane().and_then(leaf)).map(|(l, _)| l.x + l.w);
    (y, split)
}

/// Panes P3 step 3b: each member of a group names itself in the line over its own column — the list's after the
/// tab's name and ` › `, the detail's after the `┬` — and the caps title rows are gone: the row under the line is the
/// list's first.
#[test]
fn a_groups_members_are_named_over_their_own_columns() {
    let dir = crate::scratch::Scratch::new("group-names");
    std::fs::write(dir.path().join("a.rs"), "fn a() {}\n".repeat(5)).unwrap();
    for (mut ed, kind, list, detail, first) in each_kind(dir.path()) {
        let p = cells(&mut ed, W);
        assert_eq!(ed.slot_group().map(|g| g.kind), Some(kind));
        let (y, split) = slot_line(&ed, &p, kind);
        let split = split.expect("side by side at 120");
        let line = row(&p, y);
        let at = |name: &str| {
            column(&line, name, 0).unwrap_or_else(|| panic!("{name}: {}", String::from_iter(&line)))
        };
        assert_eq!(line[split], '┬', "{}", String::from_iter(&line));
        assert!(at(&format!("› {list} ")) < split, "{list} over the list");
        assert_eq!(at(&format!("┬─ {detail} ")), split, "{detail} over the detail");
        // (the failure's place after its name)
        if kind == GroupKind::Tests {
            assert!(at("src/lib.rs:31") > split, "{}", String::from_iter(&line));
        }
        let under = String::from_iter(row(&p, y + 1));
        assert!(under[..under.find('│').unwrap()].contains(first), "the list's first row: {under:?}");
        let all = p.screen().contents();
        for title in ["PLACES", "STEPS", "TESTS", "FAILURE", "OUTPUT"] {
            assert!(!all.contains(title), "{title}: {all}");
        }
    }
}

/// Step 3b, decision 3 amended: the member with the keys has its stretch of the line lit and its name strong, the
/// other's in the wash; the `┬` and the `│` under it lit while either has them (they edge both, as a code pane's
/// divider); the code's keys — nothing lit.
#[test]
fn only_the_member_with_the_keys_is_lit() {
    let dir = crate::scratch::Scratch::new("group-lit");
    std::fs::write(dir.path().join("a.rs"), "fn a() {}\n".repeat(5)).unwrap();
    for (mut ed, kind, list, detail, _) in each_kind(dir.path()) {
        // (colours drawn though tarae's test runner sets NO_COLOR; meok's accent and wash)
        crossterm::style::force_color_output(true);
        ed.theme = crate::theme::load("meok").unwrap();
        let p = cells(&mut ed, W);
        let (y, split) = slot_line(&ed, &p, kind);
        let s = split.unwrap() as u16;
        let fg = |p: &vt100::Parser, y: u16, x: u16| p.screen().cell(y, x).unwrap().fgcolor();
        let name = |p: &vt100::Parser, name: &str| {
            let x = column(&row(p, y), name, 0).unwrap() as u16;
            p.screen().cell(y, x).map(|c| (c.fgcolor(), c.bold())).unwrap()
        };
        // (the list's stretch · the detail's · the `┬` · the `│` under it)
        let look = |p: &vt100::Parser| [fg(p, y, 0), fg(p, y, s + 1), fg(p, y, s), fg(p, y + 1, s)];
        let [wash, ..] = look(&p);
        assert_eq!(look(&p), [wash; 4], "{kind:?}, the code's keys: nothing lit");
        assert_eq!(name(&p, list), name(&p, detail), "both names dim");
        // The list's keys
        ed.list_focus(kind);
        let p = cells(&mut ed, W);
        let lit = fg(&p, y, 0);
        assert_ne!(lit, wash, "{kind:?}: the list's stretch lit");
        assert_eq!(look(&p), [lit, wash, lit, lit], "{kind:?}, the list's keys");
        // (the names faded while the code had the keys — compared within a frame)
        let (strong, dim) = (name(&p, list), name(&p, detail));
        assert_ne!(strong, dim, "the list's name strong, the detail's dim");
        // `tab`: the detail's
        feed(&mut ed, "<tab>");
        assert_eq!(ed.session.at, At::Slot, "{kind:?}");
        let p = cells(&mut ed, W);
        assert_eq!(look(&p), [wash, lit, lit, lit], "{kind:?}, the detail's keys");
        assert_eq!((name(&p, list), name(&p, detail)), (dim, strong));
        // `esc`: the code's again
        feed(&mut ed, "<esc>");
        assert_eq!(ed.session.at, At::Code);
        assert_eq!(look(&cells(&mut ed, W)), [wash; 4], "{kind:?}: nothing lit again");
    }
}

/// Step 3b, step 1's fold order: side by side at 80 the members' names stand and the run's details fold before them —
/// the time, then the counts (the tab's `▲` already says how it went), the words last.
#[test]
fn the_names_stand_and_the_details_fold_side_by_side() {
    let dir = crate::scratch::Scratch::new("group-folds");
    std::fs::write(dir.path().join("a.rs"), "fn a() {}\n".repeat(5)).unwrap();
    for (mut ed, kind, list, detail, _) in each_kind(dir.path()) {
        if let Some(r) = ed.session.task_run.as_mut() {
            (r.state, r.found) = (crate::task::State::Failed(Some(101)), (0, 2));
        }
        if let Some(r) = ed.session.test_run.as_mut() {
            r.target.label = "all tests".into();
        }
        let p = cells(&mut ed, 80);
        let (y, split) = slot_line(&ed, &p, kind);
        let line = String::from_iter(row(&p, y));
        assert!(split.is_some(), "{kind:?}: side by side at 80: {line}");
        let words = if kind == GroupKind::Tests { "all tests" } else { "failed" };
        assert!(line.contains(&format!("› {list} ── {words} ")), "{kind:?}: the name and the words: {line}");
        assert!(line.contains(&format!("┬─ {detail} ")), "{line}");
        // (the task's name is short: its counts still fit)
        let folded = !line.contains("▲ 2") && !line.contains("● 1");
        assert!(folded || kind == GroupKind::Task, "{kind:?}: the counts folded: {line}");
    }
}

/// The keys that step through the failures, `]x next  [x prev`, are said where they act: in the failure's stretch of
/// the line beside its place and which of how many (`src/lib.rs:31 1/2`), one piece that folds whole — so they
/// outlast the panel's keys, which fold first.
#[test]
fn the_failures_keys_are_said_beside_the_failure() {
    let dir = crate::scratch::Scratch::new("group-fails");
    let mut ed = editor(dir.path());
    ed.session.task_run = None;
    use crate::test_results::Status;
    let case = |name: &str, status, line| crate::test_results::Case {
        name: name.into(),
        status,
        secs: None,
        message: vec!["assertion failed".into()],
        at: (status == Status::Failed).then(|| ("src/lib.rs".into(), line)),
    };
    let cases = vec![
        case("tests::length_sums_the_knots", Status::Passed, 0),
        case("tests::a_tied_knot_lengthens_it", Status::Failed, 30),
        case("tests::a_cut_knot_shortens_it", Status::Failed, 40),
    ];
    let mut run = crate::testing::TestRun::finished("t", cases, vec!["ok".into()]);
    run.selected = 1;
    ed.session.test_run = Some(run);
    for w in [100, 120] {
        let p = cells(&mut ed, w);
        let (y, split) = slot_line(&ed, &p, GroupKind::Tests);
        let line = String::from_iter(row(&p, y));
        let at = line
            .find("─ failure ── src/lib.rs:31 1/2  ]x next  [x prev ─")
            .map(|b| line[..b].chars().count());
        assert!(at.is_some_and(|x| Some(x) > split), "{w}: beside the failure: {line}");
    }
}

/// `editor_of(root, "preflight")` with its preflight under way: fmt passed in 6ms, clippy running for 13.9s, test to
/// come — the thread's clock held still (`clock::hold`), so every frame reads the same time.
fn running_preflight(root: &std::path::Path) -> Editor {
    use crate::preflight::State;
    let step = |n: &str| crate::preflight::Step {
        name: n.into(),
        cmd: String::new(),
        note: None,
        from: String::new(),
        full: false,
    };
    let mut ed = editor_of(root, "preflight");
    let steps = [step("fmt"), step("clippy"), step("test")];
    let mut p = crate::preflight::Preflight::new(0, &steps, crate::preflight::Set::Full, "k".into());
    p.steps[0].state = State::Passed;
    p.steps[0].took = Some(std::time::Duration::from_millis(6));
    p.steps[1].state = State::Running;
    p.steps[1].started = Some(crate::clock::hold() - std::time::Duration::from_millis(13_920));
    ed.session.preflight = Some(p);
    ed.session.task_run.as_mut().unwrap().state = crate::task::State::Running;
    ed
}

/// The bottom slot's height in a frame: from its line to the status line.
fn slot_h(ed: &Editor, p: &vt100::Parser, kind: GroupKind) -> usize {
    (H - 1 - slot_line(ed, p, kind).0) as usize
}

/// Panes polish: a stacked group is as tall as the member in sight — its list its rows, while the output grows
/// hidden behind it (the task panel's "as tall as its output" is the output's rule); the output its own height, back as
/// tall as it was last seen though it would want less now (`Run.tall`); side by side, the output's rule whichever
/// member is in front.
#[test]
fn a_stacked_groups_height_follows_the_member_in_sight() {
    const N: u16 = 60;
    let dir = crate::scratch::Scratch::new("group-height");
    let mut ed = running_preflight(dir.path());
    let print = |ed: &mut Editor, n: usize| {
        let r = ed.session.task_run.as_mut().unwrap();
        (0..n).for_each(|i| r.push_line(false, format!("line {i}"), false));
    };
    print(&mut ed, 2);
    let h = |ed: &mut Editor, w: u16| {
        let p = cells(ed, w);
        slot_h(ed, &p, GroupKind::Preflight)
    };
    let list = crate::chrome::SLOT_HEAD + 3;
    assert_eq!(h(&mut ed, N), list, "the list's three rows under the line");
    print(&mut ed, 40);
    assert_eq!(h(&mut ed, N), list, "the output grew hidden: still the list's rows");
    // The output in sight: as tall as it is (up to half the room)
    ed.list_focus(GroupKind::Preflight);
    feed_at(&mut ed, "<tab>", N);
    let seen = h(&mut ed, N);
    assert!(seen > list + 8, "the output's height: {seen}");
    // Back to the list: its rows; the output, now two lines, again as tall as it was seen
    feed_at(&mut ed, "<C-w>h", N);
    assert_eq!(region(&ed), "list:preflight");
    assert_eq!(h(&mut ed, N), list);
    ed.session.task_run.as_mut().unwrap().output = (0..2).map(|i| (false, format!("line {i}"))).collect();
    feed_at(&mut ed, "<C-w>l", N);
    assert_eq!(h(&mut ed, N), seen, "as tall as it was seen, not two lines' height");
    // Side by side, the list with the keys: the output's rule
    feed_at(&mut ed, "<C-w>h", W);
    assert_eq!(region(&ed), "list:preflight");
    assert_eq!(h(&mut ed, W), seen, "both in sight: the output's height");
}

/// Panes polish: every list row's mark is one cell — the running step's too, the wave's one cell — so its name starts
/// where the others' do and its time is whole, right-aligned with theirs (the three-cell wave pushed it two cells and
/// cut `13.9s` to `13.9`). Side by side and stacked.
#[test]
fn the_running_steps_row_keeps_its_columns() {
    let dir = crate::scratch::Scratch::new("group-mark");
    for w in [W, 60] {
        let mut ed = running_preflight(dir.path());
        // (side by side the panel is as tall as the output: enough of it for the three rows)
        let r = ed.session.task_run.as_mut().unwrap();
        (0..6).for_each(|i| r.push_line(false, format!("line {i}"), false));
        ed.list_focus(GroupKind::Preflight);
        let p = cells(&mut ed, w);
        let shown = p.screen().contents();
        // (each step's row, cut at the line between the members)
        let row = |name: &str| {
            let head = |l: &str| l.chars().skip(1).take(8 + name.len()).collect::<String>();
            let l = shown.lines().find(|l| head(l).contains(&format!(" {name}")));
            l.map(|l| l.chars().take_while(|&c| c != '│').collect::<String>()).expect(name)
        };
        let (fmt, clippy, test) = (row("fmt"), row("clippy"), row("test"));
        let col = |r: &str, name: &str| r.find(name).map(|b| r[..b].chars().count());
        assert_eq!(
            col(&fmt, "fmt"),
            col(&clippy, "clippy"),
            "{w}: the names in one column:\n{fmt}\n{clippy}"
        );
        assert_eq!(col(&fmt, "fmt"), col(&test, "test"), "{w}");
        let mark = clippy.chars().nth(col(&clippy, "clippy").unwrap() - 2).unwrap();
        assert!("▂▃▄▅▆▇".contains(mark), "{w}: the wave's one cell: {clippy}");
        assert!(
            clippy.trim_end().ends_with(" 13.9s") && fmt.trim_end().ends_with(" 6ms"),
            "{w}:\n{fmt}\n{clippy}"
        );
        let end = |r: &str| r.trim_end().chars().count();
        assert_eq!(end(&fmt), end(&clippy), "{w}: the times right-aligned");
    }
}

/// A run's time is written one way (`term::run_duration`), a task's, a preflight's and the tests' alike: tenths
/// under a minute, then minutes and seconds, then hours and minutes — never wider than `RUN_TIME_W` (7) under ten
/// hours.
#[test]
fn a_runs_time_is_written_one_way() {
    use std::time::Duration;
    let ms = |n: u64| crate::term::run_duration(Duration::from_millis(n));
    let said = [412, 8_340, 59_960, 432_500, 1_000_000, 3_599_000, 5_400_000, 35_999_000].map(ms);
    assert_eq!(said, ["412ms", "8.3s", "1m 00s", "7m 12s", "16m 40s", "59m 59s", "1h 30m", "9h 59m"]);
    assert!(said.iter().all(|s| s.len() <= 7), "{said:?}");
}

/// Panes polish: a run under way is said once in its header — the wave by its name (the badge); the details' place
/// the time it has run, a fixed width (`run_time`) past 1000 s and past an hour too, which becomes its whole when it
/// ends. The tests' header the same. The clock held still: a frame reads one time.
#[test]
fn a_running_runs_header_says_its_wave_once_and_its_time() {
    use std::time::Duration;
    let dir = crate::scratch::Scratch::new("group-header");
    let now = crate::clock::hold();
    let ago = |ms: u64| now - Duration::from_millis(ms);
    let waves = |l: &str| l.chars().filter(|c| "▂▃▄▅▆▇".contains(*c)).count();
    let line = |ed: &mut Editor, name: &str| {
        let shown = cells(ed, W).screen().contents();
        shown.lines().find(|l| l.starts_with(&format!("─ {name} "))).map(str::to_string).expect(name)
    };
    // (where what's said after the time starts)
    let end = |l: &str, t: &str| {
        let b = l.find(t)? + t.len();
        Some(l[..b].chars().count() + l[b..].chars().take_while(|&c| c == ' ').count())
    };
    let mut ed = editor(dir.path());
    let r = ed.session.task_run.as_mut().unwrap();
    r.state = crate::task::State::Running;
    r.took = None;
    r.started = ago(2_520);
    r.push_line(false, "building".into(), false);
    let l = line(&mut ed, "check");
    assert_eq!(waves(&l), 3, "one wave: {l}");
    assert!(l.contains(" 2.5s ─"), "nothing after it: the line runs on, no gap: {l}");
    // Quiet past `waits::SILENT`: said after the time, which keeps its width from then on
    ed.session.task_run.as_mut().unwrap().last_output = std::time::Instant::now() - Duration::from_secs(200);
    let l = line(&mut ed, "check");
    let at = end(&l, " 2.5s").unwrap_or_else(|| panic!("its time so far: {l}"));
    assert_eq!(l.chars().skip(at).take(9).collect::<String>(), "no output", "{l}");
    // (past 1000 s and past an hour: minutes, then hours — the same width)
    for (ms, said) in [(12_320, " 12.3s"), (1_234_000, " 20m 34s"), (5_400_000, " 1h 30m")] {
        ed.session.task_run.as_mut().unwrap().started = ago(ms);
        let l = line(&mut ed, "check");
        assert_eq!((waves(&l), end(&l, said)), (3, Some(at)), "counting, in its place: {l}");
    }
    let r = ed.session.task_run.as_mut().unwrap();
    r.state = crate::task::State::Passed;
    r.took = Some(Duration::from_millis(5_412_000));
    let l = line(&mut ed, "check");
    assert!(waves(&l) == 0 && l.contains(" 1h 30m"), "ended: its whole time, the same way: {l}");
    // The tests' run
    let mut ed = editor(dir.path());
    ed.session.task_run = None;
    let mut run = crate::testing::TestRun::finished("t", Vec::new(), vec!["ok".into()]);
    run.state = crate::testing::RunState::Running;
    run.took = None;
    run.started = ago(3_020);
    ed.session.test_run = Some(run);
    let l = line(&mut ed, "tests");
    assert!(waves(&l) == 3 && l.contains(" 3.0s ─"), "the tests' header: {l}");
    let r = ed.session.test_run.as_mut().unwrap();
    (r.state, r.took) = (crate::testing::RunState::Passed, Some(Duration::from_millis(432_500)));
    let l = line(&mut ed, "tests");
    assert!(l.contains(" 7m 12s"), "the tests' whole time, the same way: {l}");
}

/// §2.3: narrower than the list's and the detail's least, a group is stacked — one member in sight, the slot's
/// width, both named in the line (`places · output`, no `┬`); `tab` brings the other; the one that had the keys last
/// stays in sight once they go (`Group.front`); a click on a member's name gives it the keys.
#[test]
fn a_narrow_group_is_stacked() {
    const N: u16 = 60;
    let dir = crate::scratch::Scratch::new("group-tabs");
    let a = dir.path().join("a.rs");
    std::fs::write(&a, "fn a() {}\n".repeat(5)).unwrap();
    let mut ed = editor(dir.path());
    print(&mut ed, 3, place(&a, 0, 2, "one"));
    print(&mut ed, 3, place(&a, 2, 1, "two"));
    let p = cells(&mut ed, N);
    let (y, split) = slot_line(&ed, &p, GroupKind::Task);
    let line = String::from_iter(row(&p, y));
    assert!(split.is_none() && line.contains("› places · output ") && !line.contains('┬'), "{line}");
    // The list in sight (its first time), the slot's width; the output not drawn
    let g = ed.session.group(GroupKind::Task).unwrap();
    let list = ed.screen.leaves.iter().find(|l| l.0 == g.list).map(|l| l.1).expect("the list laid out");
    assert_eq!(list.w, N as usize);
    assert!(!ed.screen.leaves.iter().any(|l| Some(l.0) == g.detail_pane()), "the output no pane on screen");
    let all = p.screen().contents();
    assert!(all.contains("a.rs:3") && !all.contains("filler"), "{all}");
    // The list's keys: the whole line lit (one pane's), the list's name strong
    crossterm::style::force_color_output(true);
    ed.theme = crate::theme::load("meok").unwrap();
    let fg = |p: &vt100::Parser, x: u16| p.screen().cell(y, x).unwrap().fgcolor();
    let wash = fg(&cells(&mut ed, N), 0);
    ed.list_focus(GroupKind::Task);
    let p = cells(&mut ed, N);
    assert!(fg(&p, 0) != wash && fg(&p, N - 1) == fg(&p, 0), "lit end to end");
    // `C-w l` / `C-w h`: the other member opened, as side by side they'd hop to it — each is a pane, whichever layout
    feed_at(&mut ed, "<C-w>l", N);
    assert_eq!(ed.session.at, At::Slot, "C-w l: the output");
    assert!(cells(&mut ed, N).screen().contents().contains("filler"), "opened");
    feed_at(&mut ed, "<C-w>l", N);
    assert_eq!(ed.session.at, At::Slot, "nothing further right");
    feed_at(&mut ed, "<C-w>h", N);
    assert_eq!(region(&ed), "list:task", "C-w h: the list");
    assert!(cells(&mut ed, N).screen().contents().contains("a.rs:3"), "opened");
    // `tab` from the list: the output in sight, the list not
    feed_at(&mut ed, "<tab>", N);
    assert_eq!(ed.session.at, At::Slot);
    let all = cells(&mut ed, N).screen().contents();
    assert!(all.contains("filler") && !all.contains("a.rs:3 "), "{all}");
    // `esc`: the code's keys, the output still in sight
    feed_at(&mut ed, "<esc>", N);
    assert_eq!(ed.session.at, At::Code);
    assert!(cells(&mut ed, N).screen().contents().contains("filler"), "kept in sight");
    assert_eq!(ed.session.group(GroupKind::Task).unwrap().front, crate::pane::Member::Detail);
    // A click on `places`: the list's keys, in sight
    let spot =
        ed.screen.member_names.borrow().iter().find(|s| s.3 == g.list).copied().expect("its name's spot");
    let click = crate::event::Mouse {
        kind: crate::event::MouseKind::Down,
        x: spot.1 as u16,
        y: spot.0 as u16,
        alt: false,
    };
    ed.handle_mouse(click);
    let all = cells(&mut ed, N).screen().contents();
    assert_eq!(region(&ed), "list:task");
    assert!(all.contains("a.rs:3") && !all.contains("filler"), "{all}");
}
