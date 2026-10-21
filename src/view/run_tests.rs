//! View mode on the run panels — the task's output, a preflight's, the tests' detail: keys in sequence on a run
//! with no process behind it (`Run::for_test`, `TestRun::finished`), a frame after each.

use super::*;
use crate::config::Config;
use crate::sizes::Slot;

const W: u16 = 120;
const H: u16 = 34;

/// An editor whose task panel shows a run that printed `lines` lines (`out 0` …) and ended; a stand-in claude (it
/// reads nothing) for the chat the keys may go to.
fn task(lines: usize) -> Editor {
    let mut ed = Editor::new(Config::default());
    ed.docs[0].text = ropey::Rope::from_str("fn main() {}\n");
    (ed.config.llm.command, ed.config.llm.args) = ("sh".into(), vec!["-c".into(), "cat > /dev/null".into()]);
    let t =
        crate::task::Task { name: "check".into(), cmd: "cargo check".into(), cwd: ed.session.root.clone() };
    let mut r = crate::task::Run::for_test(t);
    for i in 0..lines {
        r.push_line(i % 7 == 3, format!("out {i}"), false);
    }
    r.state = crate::task::State::Passed;
    ed.session.task_run = Some(r);
    frame(&mut ed);
    ed
}

/// Two failed tests and a passing one, the first failure picked: its message is the detail; where they failed, a
/// file in `dir`.
fn tests(dir: &std::path::Path) -> Editor {
    use crate::test_results::{Case, Status};
    let mut ed = task(0);
    ed.session.task_run = None;
    std::fs::write(dir.join("math.rs"), "fn a() {}\n".repeat(20)).unwrap();
    // (as the document's path is kept: a real path — `]x` tells it's at the failure by it)
    let file = std::fs::canonicalize(dir.join("math.rs")).unwrap();
    let case = |name: &str, status, message: &[&str], line| Case {
        name: name.into(),
        status,
        secs: None,
        message: message.iter().map(|m| m.to_string()).collect(),
        at: Some((file.clone(), line)),
    };
    let cases = vec![
        case("math::adds", Status::Passed, &[], 1),
        case("math::divides", Status::Failed, &["assertion failed: 7 / 2", "  left: 3", " right: 4"], 3),
        case("math::rounds", Status::Failed, &["rounds-down-message"], 9),
    ];
    let mut r = crate::testing::TestRun::finished("math::*", cases, vec!["running 3 tests".into()]);
    r.selected = 1;
    ed.session.test_run = Some(r);
    frame(&mut ed);
    ed
}

fn frame(ed: &mut Editor) -> String {
    let mut bytes = Vec::new();
    crate::term::render(ed, &mut bytes, W, H).unwrap();
    let mut p = vt100::Parser::new(H, W, 0);
    p.process(&bytes);
    p.screen().contents()
}

/// Keys as `run` takes them (`<C-w>`, `<esc>`), a frame after each; the code's document stays current.
fn feed(ed: &mut Editor, keys: &str) {
    let code = (ed.doc().id, ed.docs.len());
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let key: Key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        };
        ed.handle_key(key);
        // (`o` opens a buffer of its own: the one key that changes the pane's document)
        if c != 'o' {
            assert_eq!((ed.doc().id, ed.docs.len()), code, "after {key}: the pane's document is current");
        }
        frame(ed);
    }
}

fn region(ed: &Editor) -> String {
    ed.keys_to().region.name()
}

fn view(ed: &Editor) -> &View {
    ed.session.view.as_ref().filter(|v| v.surface.run()).expect("a run panel's view mode")
}

/// The view's cursor line, and what's selected.
fn at(ed: &Editor) -> (usize, String) {
    let d = view(ed).doc();
    let r = d.selection().primary();
    (mv::line_of(&d.text, r.cursor(&d.text)), d.text.byte_slice(r.from()..r.to()).to_string())
}

fn said(ed: &Editor) -> String {
    ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default()
}

fn status(shown: &str) -> String {
    shown.lines().last().unwrap_or_default().to_string()
}

/// `C-w j` (or a click) gives the task panel the keys as its view mode — `● view · task` on the status line and
/// `● view` in its head, the view's pairs in it, the cursor on the last row in sight; Helix's moves on its rows,
/// the code untouched; `esc` collapses a selection, then goes to the code; `q` closes the ended run's panel.
#[test]
fn the_task_panels_keys_are_its_view_mode() {
    let mut ed = task(40);
    let code = ed.doc().selection().clone();
    feed(&mut ed, "<C-w>j");
    assert_eq!(region(&ed), "view:task");
    let shown = frame(&mut ed);
    let s = status(&shown);
    assert!(s.contains("● view · task") && s.contains("esc code") && !s.contains("i type"), "{s}");
    let head = shown.lines().find(|l| l.contains("y copy")).unwrap_or_default();
    assert!(head.contains("● view") && head.contains("/ search") && head.contains("q close"), "{head}");
    assert_eq!(at(&ed).0, 39, "on its last row");
    feed(&mut ed, "kk");
    assert_eq!(at(&ed).0, 37);
    feed(&mut ed, "x");
    assert_eq!(at(&ed).1, "out 37\n", "its row selected");
    assert!(status(&frame(&mut ed)).contains("enter quote"));
    assert_eq!(ed.doc().selection(), &code, "the code's selection untouched");
    feed(&mut ed, "<esc>");
    assert_eq!((region(&ed).as_str(), at(&ed).1.len()), ("view:task", 0), "esc: the selection collapses");
    feed(&mut ed, "<esc>");
    assert_eq!(region(&ed), "code");
    assert!(ed.session.view.is_none());
    feed(&mut ed, "<C-w>jq");
    assert!(ed.session.task_run.is_none(), "q: the ended run's panel closed");
    assert_eq!(region(&ed), "code");
}

/// `enter` on a selection quotes it (`from="task"`, its rows' text), the chat's input with the keys; with nothing
/// selected and no run of yours waiting, `enter` is the chat's input, nothing quoted. `i` says the panel takes no
/// typing.
#[test]
fn enter_quotes_a_selection_and_without_one_goes_to_the_chat() {
    let mut ed = task(12);
    feed(&mut ed, "<C-w>ji");
    assert_eq!(said(&ed), "the task panel takes no typing — esc to the code");
    feed(&mut ed, "kxx<ret>");
    assert_eq!(region(&ed), "chat");
    let c = ed.session.chat.as_ref().unwrap();
    assert_eq!(c.quotes.len(), 1);
    let q = &c.quotes[0];
    assert_eq!((q.label.as_str(), q.text.as_str()), ("task output 2 lines", "out 10\nout 11"));
    let block = q.block();
    assert!(block.contains(r#"from="task" title="check" lines="11-12 of 12""#), "{block}");
    assert!(ed.session.view.is_none(), "the view left");
    // (nothing selected: the chat's input)
    let mut ed = task(12);
    feed(&mut ed, "<C-w>j<ret>");
    assert_eq!(region(&ed), "chat");
    assert!(ed.session.chat.as_ref().unwrap().quotes.is_empty(), "nothing quoted");
}

/// Your run waiting behind the panel's (a run under way there): the status line says `enter run now`, and `enter`
/// with nothing selected runs it — out of line, not the chat's input. (No sandbox: tests of Claude's runs in line
/// skip themselves inside one.)
#[test]
fn enter_runs_your_waiting_run_now() {
    let mut ed = task(12);
    ed.session.task_run.as_mut().unwrap().state = crate::task::State::Running;
    let mine = crate::task::Task { name: "mine".into(), cmd: "true".into(), cwd: ed.session.root.clone() };
    crate::queued::push(&mut ed, crate::queued::What::Task(mine));
    assert_eq!(ed.session.runs_next.len(), 1, "it waits");
    feed(&mut ed, "<C-w>j");
    assert!(status(&frame(&mut ed)).contains("enter run now"));
    feed(&mut ed, "<ret>");
    assert!(ed.session.runs_next.is_empty(), "out of line: it runs");
    assert_ne!(region(&ed), "chat", "not the chat's input");
}

/// The tests' view is the failure picked — its message, its first line in sight; quoted with the test's name
/// (`from="test"`); `]x` is the editor's as it was on the panel: the next failure picked, the view made again of
/// its message.
#[test]
fn the_tests_view_is_the_failure_picked_and_follows_x() {
    let dir = crate::scratch::Scratch::new("run-view-tests");
    let mut ed = tests(dir.path());
    feed(&mut ed, "<C-w>j");
    assert_eq!(region(&ed), "view:tests");
    assert!(status(&frame(&mut ed)).contains("● view · tests"));
    assert!(view(&ed).failure());
    assert_eq!(at(&ed).0, 0, "the message's first line");
    feed(&mut ed, "jx<ret>");
    let q = ed.session.chat.as_ref().unwrap().quotes[0].clone();
    assert_eq!((q.label.as_str(), q.text.as_str()), ("failure 1 line", "  left: 3"));
    assert!(q.block().contains(r#"from="test" title="math::*" test="math::divides""#), "{}", q.block());
    // (`]x` opens the failure's file in the code: keys one by one, the pane's document changes)
    ed.bottom_take_keys();
    assert_eq!(region(&ed), "view:tests");
    for k in ["]", "x", "]", "x"] {
        ed.handle_key(k.parse().unwrap());
        let took = crate::uistate::TOOK.get();
        assert!(
            took != Some(crate::uistate::Handler::RunView),
            "{k}: the editor's, not the view's: {}",
            said(&ed)
        );
        frame(&mut ed);
    }
    assert_eq!(ed.session.test_run.as_ref().unwrap().selected, 2, "]x: the next failure");
    // (the rest of `]`'s act on the code out of sight: said, the code as it was)
    let code = (ed.doc().id, ed.doc().selection().clone());
    for k in ["]", "g"] {
        ed.handle_key(k.parse().unwrap());
    }
    assert!(said(&ed).starts_with("] g isn't in view mode"), "{}", said(&ed));
    assert_eq!((ed.doc().id, ed.doc().selection().clone()), code, "]g: the code untouched");
    assert!(ed.pending.is_empty() && region(&ed) == "view:tests");
    assert_eq!(region(&ed), "view:tests");
    assert_eq!(view(&ed).doc().text.to_string(), "rounds-down-message\n", "made again of its message");
}

/// A preflight's: `● view · preflight`, `o` its step's output in a buffer of its own (the keys to it), `K` says
/// the run isn't Claude's; `tab` / `S-tab` the slot's other panel's view mode and back.
#[test]
fn a_preflights_o_and_k_and_tab_to_the_tests() {
    let mut ed = task(0);
    let r = ed.session.task_run.as_mut().unwrap();
    r.push_line(false, "── fmt ──".into(), true);
    r.push_line(false, "fmt-says-ok".into(), false);
    let steps = [crate::preflight::Step {
        name: "fmt".into(),
        cmd: String::new(),
        note: None,
        from: String::new(),
        full: false,
    }];
    let mut p = crate::preflight::Preflight::new(0, &steps, crate::preflight::Set::Full, "k".into());
    p.steps[0].state = crate::preflight::State::Passed;
    ed.session.preflight = Some(p);
    ed.session.test_run = Some(crate::testing::TestRun::finished("t", Vec::new(), vec!["ok".into()]));
    frame(&mut ed);
    // (the tests', newer, in front)
    feed(&mut ed, "<C-w>j");
    assert_eq!((ed.slot(), region(&ed).as_str()), (Some(Slot::Test), "view:tests"));
    feed(&mut ed, "<tab>");
    assert_eq!(region(&ed), "view:preflight");
    assert!(status(&frame(&mut ed)).contains("● view · preflight"));
    feed(&mut ed, "<S-tab>");
    assert_eq!(region(&ed), "view:tests");
    feed(&mut ed, "<tab>K");
    assert!(said(&ed).contains("this run isn't one"), "{}", said(&ed));
    feed(&mut ed, "o");
    assert_eq!(region(&ed), "code", "its output's buffer has the keys");
    assert!(ed.doc().text.to_string().contains("fmt-says-ok"), "{}", ed.doc().text);
}

/// view P3's leftover: `tab`/`S-tab` go round the slot's tabs from the `:procs` panel too — there, they were
/// "unused" while the run views' and the terminal's took them.
#[test]
fn tab_goes_round_the_slot_from_procs_too() {
    let mut ed = task(3);
    feed(&mut ed, ":procs<ret>");
    frame(&mut ed);
    assert_eq!((ed.slot(), region(&ed).as_str()), (Some(Slot::Procs), "slot:procs"));
    feed(&mut ed, "<tab>");
    frame(&mut ed);
    assert_eq!((ed.slot(), region(&ed).as_str()), (Some(Slot::Task), "view:task"), "{}", said(&ed));
    feed(&mut ed, "<S-tab>");
    frame(&mut ed);
    assert_eq!((ed.slot(), region(&ed).as_str()), (Some(Slot::Procs), "slot:procs"));
    feed(&mut ed, "<S-tab>");
    assert_eq!(ed.slot(), Some(Slot::Task), "round the other way");
    assert!(!said(&ed).contains("isn't"), "never unused: {}", said(&ed));
}

/// A click on a preflight step's chip with its view up: the view made again from that step's heading, at the top.
#[test]
fn a_step_chip_makes_the_view_again_at_its_heading() {
    let mut ed = task(0);
    let r = ed.session.task_run.as_mut().unwrap();
    for name in ["fmt", "test", "clippy"] {
        r.push_line(false, format!("── {name} ──"), true);
        for i in 0..30 {
            r.push_line(false, format!("{name} line {i}"), false);
        }
    }
    let steps: Vec<crate::preflight::Step> = ["fmt", "test", "clippy"]
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
    ed.session.preflight = Some(p);
    frame(&mut ed);
    feed(&mut ed, "<C-w>j");
    let before = view(&ed).doc().id;
    let (y, x, _, _) =
        *ed.screen.task_steps.borrow().iter().find(|c| c.3 == 1).expect("the test step's chip");
    ed.handle_mouse(crate::event::Mouse {
        kind: crate::event::MouseKind::Down,
        x: x as u16,
        y: y as u16,
        alt: false,
    });
    frame(&mut ed);
    assert_eq!(region(&ed), "view:preflight");
    let d = view(&ed).doc();
    assert_ne!(d.id, before, "made again");
    assert_eq!(d.text.line(d.top).to_string().trim_end(), "── test ──", "its heading at the top");
    assert!(frame(&mut ed).contains("test line 0"));
}

/// The run's output trimmed while it's viewed (`Run::keep`): its lines are no longer the view's, so the view's top
/// stops being its scroll — the run keeps `keep`'s own correction, through keys and leaving.
#[test]
fn a_trimmed_run_keeps_its_own_scroll() {
    let mut ed = task(3100);
    feed(&mut ed, "<C-w>j1500G");
    let top = view(&ed).doc().top;
    assert_eq!(ed.session.task_run.as_ref().unwrap().scroll, Some(top), "untrimmed: the view's top");
    let r = ed.session.task_run.as_mut().unwrap();
    r.keep(0);
    let corrected = r.scroll;
    assert_ne!(corrected, Some(top), "keep moved it with the lines it took out");
    feed(&mut ed, "j");
    assert_eq!(ed.session.task_run.as_ref().unwrap().scroll, corrected, "a key leaves it");
    feed(&mut ed, "<esc>");
    assert_eq!(region(&ed), "code");
    assert_eq!(ed.session.task_run.as_ref().unwrap().scroll, corrected, "so does leaving");
}

/// Held still: what the run prints while viewed waits below (`● view · output after`), the rows in sight as they
/// were; `G` catches up, on the latest line. The search prompt it opens stands over its word, not the code's mode.
#[test]
fn it_holds_still_and_g_catches_up_and_its_search_says_its_word() {
    let mut ed = task(30);
    feed(&mut ed, "<C-w>j");
    ed.session.task_run.as_mut().unwrap().push_line(false, "came-later".into(), false);
    let shown = frame(&mut ed);
    assert!(!shown.contains("came-later") && shown.contains("● view · output after"), "{shown}");
    feed(&mut ed, "G");
    let shown = frame(&mut ed);
    assert!(shown.contains("came-later") && !shown.contains("output after"), "{shown}");
    assert_eq!(at(&ed).0, 30, "on the latest line");
    feed(&mut ed, "/out 1");
    let s = status(&frame(&mut ed));
    assert!(s.contains("● view · task") && !s.contains("normal"), "{s}");
    feed(&mut ed, "<ret>");
    assert_eq!(at(&ed).1, "out 1", "found in the view");
    assert_eq!(ed.search, None, "the code's search untouched");
}

/// `space` in a run panel's view: view mode's own menu (`space >` quotes the selection; `space ?` the palette over
/// it, the view kept under; a key the code's menu has — `space m` — said).
#[test]
fn space_is_view_modes_own_menu() {
    let mut ed = task(12);
    feed(&mut ed, "<C-w>j<space>");
    let shown = frame(&mut ed);
    for row in ["Quote or view chat", "Find a command", "Chat with Claude", "What waits for your answer"] {
        assert!(shown.contains(row), "{row}: {shown}");
    }
    feed(&mut ed, "m");
    assert!(said(&ed).starts_with("space m isn't in view mode — esc to the code"), "{}", said(&ed));
    feed(&mut ed, "<space>?");
    assert!(ed.picker.is_some(), "the palette");
    feed(&mut ed, "<esc>");
    assert_eq!(region(&ed), "view:task", "the view under it");
    feed(&mut ed, "x<space>>");
    let c = ed.session.chat.as_ref().unwrap();
    assert_eq!((region(&ed).as_str(), c.quotes.len()), ("chat", 1), "space >: quoted");
}

/// The wheel up over the task panel enters its view mode at that row, the keys its; back down to the end, nothing
/// selected, the keys go back to the code. `C-e`/`C-y` move its view a row, as every output panel's.
#[test]
fn the_wheel_enters_its_view_and_leaves_at_the_end() {
    use crate::event::{Mouse, MouseKind as K};
    let mut ed = task(40);
    let (_, y, x, rows) = ed.screen.run_rows.get().expect("its rows recorded");
    let wheel = |ed: &mut Editor, kind| {
        ed.handle_mouse(Mouse { kind, x: x as u16 + 2, y: (y + 2) as u16, alt: false })
    };
    wheel(&mut ed, K::ScrollUp);
    frame(&mut ed);
    assert_eq!(region(&ed), "view:task");
    assert_eq!(view(&ed).by_wheel, Some(Was::Code));
    let top = view(&ed).doc().top;
    let n = view(&ed).rows();
    assert_eq!(
        top,
        40 - rows - 3,
        "scrolled as the wheel took it: {n} lines, {rows} rows, {:?}",
        ed.screen.run_rows.get()
    );
    feed(&mut ed, "<C-e>");
    assert_eq!(view(&ed).doc().top, top + 1);
    feed(&mut ed, "<C-y>");
    wheel(&mut ed, K::ScrollDown);
    frame(&mut ed);
    assert_eq!(region(&ed), "code", "back at the end: the keys where they were");
    // A press on a row: the view's cursor there
    ed.handle_mouse(Mouse { kind: K::Down, x: (x + 1) as u16, y: (y + 1) as u16, alt: false });
    assert_eq!(region(&ed), "view:task");
    assert_eq!(view(&ed).doc().selection().primary().cursor(&view(&ed).doc().text), {
        let d = view(&ed).doc();
        mv::line_start(&d.text, d.top + 1) + 1
    });
}
