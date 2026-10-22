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
