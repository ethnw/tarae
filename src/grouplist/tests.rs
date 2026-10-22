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
    ed.focus_sync();
    let mut bytes = Vec::new();
    crate::term::render(ed, &mut bytes, W, H).unwrap();
    let mut p = vt100::Parser::new(H, W, 0);
    p.process(&bytes);
    p.screen().contents()
}

/// Keys as `run` takes them (`<C-w>`, `<S-tab>`), a frame after each.
fn feed(ed: &mut Editor, keys: &str) {
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let key: Key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        };
        ed.handle_key(key);
        frame(ed);
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
    let mut ed = Editor::new(Config::default());
    ed.session.root = root.to_path_buf();
    ed.docs[0].text = ropey::Rope::from_str("fn main() {}\n");
    let t = crate::task::Task { name: "check".into(), cmd: "cargo check".into(), cwd: root.to_path_buf() };
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
    assert!(shown.contains("PLACES") && shown.contains("OUTPUT"), "{shown}");
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
    assert!(shown.contains("STEPS") && ed.screen.task_steps.borrow().is_empty(), "a list, no chip row");
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
