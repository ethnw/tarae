//! The run panels' output as documents: keys in sequence on a run with no process behind it (`Run::for_test`,
//! `TestRun::finished`), a frame after each.

use super::*;
use crate::config::Config;

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

/// What a run's output costs in memory, kept once (`runout::Output`, the panel's document holding its rope): what a
/// task panel's 20,000 lines add — the run, its document following it, frames drawn — against their text, as the
/// bytes malloc holds in use (macOS: what's kept, not the pages a freed copy leaves dirty) and the footprint. Run
/// alone (both are the process's). Measured against the code before (P2c step 1, d95b3f13: the lines a
/// `Vec<(bool, String)>`, the document a copy) by the same scene there — target/sb/panes/p2c2/perf.md.
#[test]
fn perf_run_output_memory() {
    if crate::test_results::unoptimized("rundoc::tests::perf_run_output_memory") {
        return;
    }
    let me = std::process::id();
    let footprint = || {
        let os = crate::procs::tree(me).into_iter().find(|o| o.pid == me).unwrap();
        os.footprint.unwrap_or(os.rss)
    };
    #[cfg(target_os = "macos")]
    let in_use = || {
        // SAFETY: a plain struct malloc fills, every zone's (a null zone)
        let mut s: libc::malloc_statistics_t = unsafe { std::mem::zeroed() };
        unsafe { libc::malloc_zone_statistics(std::ptr::null_mut(), &mut s) };
        s.size_in_use as u64
    };
    #[cfg(not(target_os = "macos"))]
    let in_use = || 0u64;
    let mb = |b: u64| b as f64 / (1 << 20) as f64;
    let mut ed = Editor::new(Config::default());
    ed.docs[0].text = ropey::Rope::from_str("fn main() {}\n");
    frame(&mut ed);
    let line = |i: usize| format!("test module_{i}::case_{i} ... ok");
    let text: usize = (0..20_000).map(|i| line(i).len() + 1).sum();
    let before = (in_use(), footprint());
    let t = crate::task::Task { name: "t".into(), cmd: "cargo test".into(), cwd: std::env::temp_dir() };
    let mut r = crate::task::Run::for_test(t);
    for i in 0..20_000 {
        r.push_line(i % 9 == 0, line(i), false);
    }
    let run = in_use();
    ed.session.task_run = Some(r);
    frame(&mut ed);
    frame(&mut ed);
    let lines = ed.session.outputs.doc(Slot::Task).map_or(0, |d| d.text.len_lines());
    let after = (in_use(), footprint());
    let more = |a: u64, b: u64| a.saturating_sub(b);
    eprintln!(
        "run output: 20,000 lines, {:.2} MB of text, its document {lines} lines — in use {:.2} MB more (the run \
         {:.2}, its panel {:.2}; {:.1}× the text), footprint {:.2} MB more",
        mb(text as u64),
        mb(more(after.0, before.0)),
        mb(more(run, before.0)),
        mb(more(after.0, run)),
        more(after.0, before.0) as f64 / text as f64,
        mb(more(after.1, before.1)),
    );
    assert!(lines > 20_000, "the panel's document follows the run");
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

/// Keys as `run` takes them (`<C-w>`, `<esc>`), a frame after each; the code's document stays current, never the
/// output's.
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

fn output(ed: &Editor, slot: Slot) -> &Document {
    ed.session.outputs.doc(slot).expect("a run panel's output")
}

/// The task output's cursor line, and what's selected.
fn at(ed: &Editor, slot: Slot) -> (usize, String) {
    let d = output(ed, slot);
    let r = d.selection().primary();
    (mv::line_of(&d.text, r.cursor(&d.text)), d.text.byte_slice(r.from()..r.to()).to_string())
}

fn said(ed: &Editor) -> String {
    ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default()
}

fn status(shown: &str) -> String {
    shown.lines().last().unwrap_or_default().to_string()
}

/// `C-w j` (or a click) gives the task panel's output the keys: normal mode on its document — `● normal · task`
/// on the status line, its pairs in the panel's head, the cursor on its last row in sight; the editor's moves and
/// selections on its lines, the code untouched; `esc` collapses a selection, then goes to the code; `q` closes the
/// ended run's panel.
#[test]
fn the_task_panels_keys_are_normal_mode_on_its_output() {
    let mut ed = task(40);
    let code = ed.doc().selection().clone();
    feed(&mut ed, "<C-w>j");
    assert_eq!(region(&ed), "output:task");
    let shown = frame(&mut ed);
    let s = status(&shown);
    assert!(s.contains("● normal · task") && s.contains("esc code") && !s.contains("i type"), "{s}");
    let head = shown.lines().find(|l| l.contains("y copy")).unwrap_or_default();
    assert!(head.contains("/ search") && head.contains("q close"), "{head}");
    assert_eq!(at(&ed, Slot::Task).0, 39, "on its last row");
    assert!(s.contains("40:1"), "the position is the output's, not the code's: {s}");
    feed(&mut ed, "kk");
    assert_eq!(at(&ed, Slot::Task).0, 37);
    feed(&mut ed, "x");
    assert_eq!(at(&ed, Slot::Task).1, "out 37\n", "its row selected");
    assert!(status(&frame(&mut ed)).contains("enter quote"));
    // (the editor's own: a word, a count, `f`, the line's end)
    feed(&mut ed, "<esc>3kghwe");
    assert_eq!(at(&ed, Slot::Task), (34, "34".into()), "3k, its start, w e: the number");
    assert_eq!(ed.doc().selection(), &code, "the code's selection untouched");
    feed(&mut ed, "x<esc>");
    assert_eq!((region(&ed).as_str(), at(&ed, Slot::Task).1.len()), ("output:task", 0), "esc: collapsed");
    feed(&mut ed, "<esc>");
    assert_eq!(region(&ed), "code");
    feed(&mut ed, "<C-w>jq");
    assert!(ed.session.task_run.is_none() && ed.session.outputs.doc(Slot::Task).is_none(), "q: closed");
    assert_eq!(region(&ed), "code");
}

/// Read-only: what would edit it — `d`, `c`, `p`, `i`, `o`'s neighbours, `u` — is put back and said, insert mode
/// never entered; `y` copies to `"` and the clipboard, `p` in the code pastes it there.
#[test]
fn it_refuses_edits_and_says_so_and_y_copies() {
    let mut ed = task(12);
    let before = output(&ed, Slot::Task).text.clone();
    for keys in ["xd", "i", "a", "xc", "P", "A", ">", "~"] {
        feed(&mut ed, "<C-w>j");
        feed(&mut ed, keys);
        assert_eq!(output(&ed, Slot::Task).text, before, "{keys}: put back");
        assert_eq!(said(&ed), "the task's output is read-only — esc code", "{keys}");
        assert_eq!(ed.mode, Mode::Normal, "{keys}: the code's mode");
        assert_eq!(region(&ed), "output:task", "{keys}: the keys stay");
        let o = ed.session.outputs.get(Slot::Task).unwrap();
        assert_eq!(o.keys.mode, Mode::Normal, "{keys}: never insert mode there");
        feed(&mut ed, "<esc><esc>");
    }
    // (nothing to undo: none of it stayed)
    feed(&mut ed, "<C-w>ju");
    assert_eq!(said(&ed), "already at oldest change");
    feed(&mut ed, "<esc>");
    feed(&mut ed, "<C-w>jkxy<esc><esc>");
    assert_eq!(ed.registers.get(&'"').cloned(), Some(vec!["out 10\n".to_string()]), "y: in \"");
    feed(&mut ed, "p");
    assert_eq!(ed.doc().text.to_string(), "fn main() {}\nout 10\n", "p in the code pastes it");
}

/// `enter` on a selection quotes it (`from="task"`, its lines' text, where they are of how many), the chat's input
/// with the keys; with nothing selected and no run of yours waiting, `enter` is the chat's input, nothing quoted.
/// `space >` quotes as `enter` does.
#[test]
fn enter_quotes_a_selection_and_without_one_goes_to_the_chat() {
    let mut ed = task(12);
    feed(&mut ed, "<C-w>jkxx<ret>");
    assert_eq!(region(&ed), "chat");
    let c = ed.session.chat.as_ref().unwrap();
    assert_eq!(c.quotes.len(), 1);
    let q = &c.quotes[0];
    assert_eq!((q.label.as_str(), q.text.as_str()), ("task output 2 lines", "out 10\nout 11"));
    let block = q.block();
    assert!(block.contains(r#"from="task" title="check" lines="11-12 of 12""#), "{block}");
    // (nothing selected: the chat's input)
    let mut ed = task(12);
    feed(&mut ed, "<C-w>j<ret>");
    assert_eq!(region(&ed), "chat");
    assert!(ed.session.chat.as_ref().unwrap().quotes.is_empty(), "nothing quoted");
    // (`space >`, as `enter`)
    let mut ed = task(12);
    feed(&mut ed, "<C-w>jx<space>>");
    let c = ed.session.chat.as_ref().unwrap();
    assert_eq!((region(&ed).as_str(), c.quotes.len()), ("chat", 1), "space >: quoted");
}

/// Your run waiting behind the panel's (a run under way there): the status line says `enter run now`, and `enter`
/// with nothing selected runs it — out of line, not the chat's input.
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

/// The tests' detail is the failure picked — its message, its first line in sight, its first line red; quoted with
/// the test's name (`from="test"`); `]x` is the editor's as it was on the panel: the next failure picked, the
/// document made again of its message, the keys still its; the rest of `]` acts on the output (`]g`: no changes in
/// it), the code untouched.
#[test]
fn the_tests_detail_is_the_failure_picked_and_follows_x() {
    let dir = crate::scratch::Scratch::new("rundoc-tests");
    let mut ed = tests(dir.path());
    feed(&mut ed, "<C-w>j");
    assert_eq!(region(&ed), "output:tests");
    assert!(status(&frame(&mut ed)).contains("● normal · tests"));
    assert!(ed.session.outputs.get(Slot::Test).unwrap().failure());
    assert_eq!(at(&ed, Slot::Test).0, 0, "the message's first line");
    feed(&mut ed, "jx<ret>");
    let q = ed.session.chat.as_ref().unwrap().quotes[0].clone();
    assert_eq!((q.label.as_str(), q.text.as_str()), ("failure 1 line", "  left: 3"));
    assert!(q.block().contains(r#"from="test" title="math::*" test="math::divides""#), "{}", q.block());
    ed.bottom_take_keys();
    assert_eq!(region(&ed), "output:tests");
    // (`]x` opens the failure's file in the code — the one picked, then the next: keys one by one, the code's
    // document changes)
    for k in ["]", "x", "]", "x"] {
        ed.handle_key(k.parse().unwrap());
        frame(&mut ed);
    }
    assert_eq!(ed.session.test_run.as_ref().unwrap().selected, 2, "]x: the next failure");
    assert_eq!(region(&ed), "output:tests", "the keys stay on the detail");
    assert_eq!(
        output(&ed, Slot::Test).text.to_string(),
        "rounds-down-message\n",
        "made again of its message"
    );
    let code = (ed.doc().id, ed.doc().selection().clone());
    for k in ["]", "g"] {
        ed.handle_key(k.parse().unwrap());
    }
    assert_eq!((ed.doc().id, ed.doc().selection().clone()), code, "]g: the code untouched");
    assert!(ed.pending.is_empty() && region(&ed) == "output:tests");
}

/// A preflight's: `● normal · preflight`, its step's heading receding (painted), `o` its step's output in a buffer
/// of its own (the keys to it), `K` says the run isn't Claude's; `tab` / `S-tab` the slot's other panel and back.
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
    let faint = crate::syntax::capture_id("ui.linenr");
    assert!(
        output(&ed, Slot::Task).paint.iter().any(|s| s == &(0, "── ".len(), faint)),
        "its heading recedes"
    );
    // (the tests', newer, in front)
    feed(&mut ed, "<C-w>j");
    assert_eq!((ed.slot(), region(&ed).as_str()), (Some(Slot::Test), "output:tests"));
    feed(&mut ed, "<tab>");
    assert_eq!(region(&ed), "output:preflight");
    assert!(status(&frame(&mut ed)).contains("● normal · preflight"));
    feed(&mut ed, "<S-tab>");
    assert_eq!(region(&ed), "output:tests");
    feed(&mut ed, "<tab>K");
    assert!(said(&ed).contains("this run isn't one"), "{}", said(&ed));
    feed(&mut ed, "o");
    assert_eq!(region(&ed), "code", "its output's buffer has the keys");
    assert!(ed.doc().text.to_string().contains("fmt-says-ok"), "{}", ed.doc().text);
}

/// `tab`/`S-tab` go round the slot's tabs from the `:procs` panel too.
#[test]
fn tab_goes_round_the_slot_from_procs_too() {
    let mut ed = task(3);
    feed(&mut ed, ":procs<ret>");
    assert_eq!((ed.slot(), region(&ed).as_str()), (Some(Slot::Procs), "slot:procs"));
    feed(&mut ed, "<tab>");
    assert_eq!((ed.slot(), region(&ed).as_str()), (Some(Slot::Task), "output:task"), "{}", said(&ed));
    feed(&mut ed, "<S-tab>");
    assert_eq!((ed.slot(), region(&ed).as_str()), (Some(Slot::Procs), "slot:procs"));
    feed(&mut ed, "<S-tab>");
    assert_eq!(ed.slot(), Some(Slot::Task), "round the other way");
    assert!(!said(&ed).contains("isn't"), "never unused: {}", said(&ed));
}

/// A click on a preflight step's chip: the output's keys, from that step's heading at the top, the cursor on it.
#[test]
fn a_step_chip_shows_its_heading_at_the_top() {
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
    let (y, x, _, _) =
        *ed.screen.task_steps.borrow().iter().find(|c| c.3 == 1).expect("the test step's chip");
    ed.handle_mouse(crate::event::Mouse {
        kind: crate::event::MouseKind::Down,
        x: x as u16,
        y: y as u16,
        alt: false,
    });
    frame(&mut ed);
    assert_eq!(region(&ed), "output:preflight");
    let d = output(&ed, Slot::Task);
    assert_eq!(d.text.line(d.top).to_string().trim_end(), "── test ──", "its heading at the top");
    assert_eq!(at(&ed, Slot::Task).0, d.top, "the cursor on it");
    assert!(frame(&mut ed).contains("test line 0"));
}

/// The run's output trimmed while the keys are in it (`Run::keep`): one change — what you read stays in sight
/// where it's still there, the document the run's lines again, the run's scroll its top.
#[test]
fn a_trimmed_run_keeps_what_you_read() {
    let mut ed = task(3100);
    feed(&mut ed, "<C-w>j3000G");
    let line = |ed: &Editor| {
        let d = output(ed, Slot::Task);
        d.text.line(at(ed, Slot::Task).0).to_string()
    };
    assert_eq!(line(&ed), "out 2999\n");
    let r = ed.session.task_run.as_mut().unwrap();
    r.keep(0);
    let kept = r.output.len();
    frame(&mut ed);
    let d = output(&ed, Slot::Task);
    assert_eq!(mv::last_line(&d.text) + 1, kept, "the run's lines again");
    assert_eq!(line(&ed), "out 2999\n", "the cursor on the line it was on");
    let trims = ed.session.task_run.as_ref().unwrap().trims();
    assert!(trims.count() > 0 && trims.held() == 0, "replayed, the run keeps none of them");
    let top = d.top;
    assert_eq!(
        ed.session.task_run.as_ref().unwrap().top(rows(&ed, Slot::Task)),
        top,
        "the run's scroll its top"
    );
    feed(&mut ed, "<esc>");
    assert_eq!(ed.session.task_run.as_ref().unwrap().top(rows(&ed, Slot::Task)), top, "left where you read");
}

/// Held still: what the run prints while the keys are in it comes below, the rows in sight as they were (`↓ output
/// after` in its head); `G` catches up, on the latest line. Its search is its own: the prompt it opens stands over
/// its word, `n` finds the next, the code's search untouched.
#[test]
fn it_holds_still_and_g_catches_up_and_its_search_is_its_own() {
    let mut ed = task(30);
    feed(&mut ed, "<C-w>j");
    let top = output(&ed, Slot::Task).top;
    ed.session.task_run.as_mut().unwrap().push_line(false, "came-later".into(), false);
    let shown = frame(&mut ed);
    assert!(!shown.contains("came-later") && shown.contains("↓ output after"), "{shown}");
    assert_eq!(output(&ed, Slot::Task).top, top, "held");
    feed(&mut ed, "G");
    let shown = frame(&mut ed);
    assert!(shown.contains("came-later") && !shown.contains("output after"), "{shown}");
    assert_eq!(at(&ed, Slot::Task).0, 30, "on the latest line");
    feed(&mut ed, "/out 1");
    let s = status(&frame(&mut ed));
    assert!(s.contains("● normal · task"), "{s}");
    feed(&mut ed, "<ret>");
    assert_eq!(at(&ed, Slot::Task), (1, "out 1".into()), "found, from the start round");
    assert!(status(&frame(&mut ed)).contains("/out 1"), "its count on the status line");
    feed(&mut ed, "n");
    assert_eq!(at(&ed, Slot::Task), (10, "out 1".into()), "n: the next");
    assert_eq!(ed.search, None, "the code's search untouched");
}

/// The tests' output follows its run past the run's cut (`TestRun::keep`, its oldest lines going): the document is
/// the run's lines after every batch, the cursor on the line it was on, and the run holds none of its trims once
/// the document has them.
#[test]
fn the_tests_output_follows_its_run_past_its_cut() {
    let mut ed = task(0);
    ed.session.task_run = None;
    let lines: Vec<String> = (0..4990).map(|i| format!("line {i}")).collect();
    let mut r = crate::testing::TestRun::finished("all", Vec::new(), lines);
    r.state = crate::testing::RunState::Running;
    ed.session.test_run = Some(r);
    frame(&mut ed);
    feed(&mut ed, "<C-w>j4900G");
    let text = |ed: &Editor| {
        let r = ed.session.test_run.as_ref().unwrap();
        r.output.iter().map(|(_, l)| format!("{l}\n")).collect::<String>()
    };
    for batch in 0..6 {
        let r = ed.session.test_run.as_mut().unwrap();
        for i in 0..20 {
            r.output.push(false, &format!("line {}", 4990 + batch * 20 + i));
        }
        r.keep();
        frame(&mut ed);
        assert_eq!(
            output(&ed, Slot::Test).text.to_string(),
            text(&ed),
            "the run's lines after batch {batch}"
        );
    }
    let d = output(&ed, Slot::Test);
    assert_eq!(d.text.line(at(&ed, Slot::Test).0).to_string(), "line 4899\n", "the line you were on");
    let trims = &ed.session.test_run.as_ref().unwrap().trims;
    assert!(trims.count() > 0 && trims.held() == 0, "cut, replayed, none kept");
}

/// Into a run still printing, at its end: the cursor lands above the scrolloff margin, so what it prints next and
/// the first key move no rows (it landed on the last line and `k` scrolled by the margin). An ended run's: its last.
#[test]
fn into_a_run_still_printing_the_first_key_moves_no_rows() {
    let mut ed = task(30);
    ed.session.task_run.as_mut().unwrap().state = crate::task::State::Running;
    frame(&mut ed);
    feed(&mut ed, "<C-w>j");
    let (top, line) = (output(&ed, Slot::Task).top, at(&ed, Slot::Task).0);
    assert!(line < 29, "above the margin, not on the last line: {line}");
    for i in 0..5 {
        ed.session.task_run.as_mut().unwrap().push_line(false, format!("came {i}"), false);
        frame(&mut ed);
    }
    feed(&mut ed, "k");
    assert_eq!((output(&ed, Slot::Task).top, at(&ed, Slot::Task).0), (top, line - 1), "no rows moved");
    let mut ed = task(30);
    feed(&mut ed, "<C-w>j");
    assert_eq!(at(&ed, Slot::Task).0, 29, "an ended run's last line");
}

/// `space` is the editor's menu, as from the code: `space ?` the palette over the output (the keys back on it when
/// it goes), its which-key the code's.
#[test]
fn space_is_the_editors_menu() {
    let mut ed = task(12);
    feed(&mut ed, "<C-w>j<space>");
    let shown = frame(&mut ed);
    for row in ["Find a command", "Chat with Claude"] {
        assert!(shown.contains(row), "{row}: {shown}");
    }
    feed(&mut ed, "?");
    assert!(ed.picker.is_some(), "the palette");
    feed(&mut ed, "<esc>");
    assert_eq!(region(&ed), "output:task", "the output under it");
}

/// The wheel over the task panel scrolls it without taking the keys (the end, following its run again); with the
/// keys, three rows of its document, the cursor kept in sight. A press on a row: the keys, the cursor there.
#[test]
fn the_wheel_scrolls_and_a_press_puts_the_cursor() {
    use crate::event::{Mouse, MouseKind as K};
    let mut ed = task(40);
    let (_, y, x, rows) = ed.screen.run_rows.get().expect("its rows recorded");
    let wheel = |ed: &mut Editor, kind| {
        ed.handle_mouse(Mouse { kind, x: x as u16 + 2, y: (y + 2) as u16, alt: false })
    };
    let top = |ed: &Editor| ed.session.task_run.as_ref().unwrap().top(rows);
    wheel(&mut ed, K::ScrollUp);
    frame(&mut ed);
    assert_eq!((region(&ed).as_str(), top(&ed)), ("code", 40 - rows - 3), "scrolled, the keys the code's");
    wheel(&mut ed, K::ScrollDown);
    assert_eq!(ed.session.task_run.as_ref().unwrap().scroll, None, "at the end: it follows again");
    // A press on a row: the keys, the cursor there — the rows under the pointer where they were
    ed.handle_mouse(Mouse { kind: K::Down, x: (x + 1) as u16, y: (y + 1) as u16, alt: false });
    assert_eq!(region(&ed), "output:task");
    let d = output(&ed, Slot::Task);
    assert_eq!(d.top, top(&ed), "the rows as they were");
    assert_eq!(d.selection().primary().cursor(&d.text), mv::line_start(&d.text, d.top + 1) + 1);
    let was = output(&ed, Slot::Task).top;
    wheel(&mut ed, K::ScrollUp);
    assert_eq!(output(&ed, Slot::Task).top, was - 3, "with the keys: its document, three rows");
    frame(&mut ed);
    let d = output(&ed, Slot::Task);
    let line = mv::line_of(&d.text, d.selection().primary().cursor(&d.text));
    assert!((d.top..d.top + rows).contains(&line), "the cursor in sight");
}

/// A key that shows the code another buffer from the output (`ga`, the one before) gives the code the keys — the
/// output's document never the code's, the code's pane showing that buffer.
#[test]
fn a_key_that_shows_the_code_another_buffer_gives_it_the_keys() {
    let dir = crate::scratch::Scratch::new("rundoc-ga");
    let mut ed = task(12);
    for name in ["a.rs", "b.rs"] {
        std::fs::write(dir.path().join(name), format!("// {name}\n")).unwrap();
        ed.open(&dir.path().join(name)).unwrap();
    }
    frame(&mut ed);
    let b = ed.doc().id;
    ed.bottom_take_keys();
    assert_eq!(region(&ed), "output:task");
    for k in ["g", "a"] {
        ed.handle_key(k.parse().unwrap());
    }
    frame(&mut ed);
    assert_eq!(region(&ed), "code", "the code has the keys");
    assert_ne!(ed.doc().id, b, "showing the buffer before");
    assert!(ed.doc().text.to_string().starts_with("// a.rs"), "{}", ed.doc().text);
    assert!(ed.docs.iter().all(|d| d.id != output(&ed, Slot::Task).id), "the output never a buffer");
    // (the pane's own record kept: what it showed before is the code's, never the output)
    let view = ed.session.views.iter().find(|v| v.id == ed.session.focus).unwrap();
    assert_eq!(view.last_doc, Some(b), "the buffer before it, b.rs");
    for k in ["g", "a"] {
        ed.handle_key(k.parse().unwrap());
    }
    assert_eq!(ed.doc().id, b, "ga again: back to b.rs");
}

/// The window keys from the output are the editor's, `space w` as `C-w`: a split is the code pane's (§2.6), the keys
/// to the new one — neither pane ever showing the output's document.
#[test]
fn the_window_keys_from_the_output_split_the_code() {
    let mut ed = task(12);
    let code = ed.doc().id;
    feed(&mut ed, "<C-w>j<space>wv");
    assert_eq!(ed.session.views.len(), 2, "split");
    assert!(ed.session.views.iter().all(|v| v.doc == code), "both the code's");
    assert_eq!(region(&ed), "code", "the keys to the new pane");
}

/// What it's drawn with: the code's drawer, its colours as the panel had them — a failing line red, stderr faint,
/// a passing one green — painted on its document.
#[test]
fn its_document_holds_the_runs_own_rope_and_a_tab_as_printed() {
    let mut ed = task(2000);
    let r = ed.session.task_run.as_mut().unwrap();
    r.push_line(false, "\tleft: 3".into(), false);
    frame(&mut ed);
    // (the same storage, not an equal copy: every chunk of the document's text is the run's own, at its address)
    let chunks = |t: &ropey::Rope| t.chunks().map(|c| (c.as_ptr(), c.len())).collect::<Vec<_>>();
    let run = chunks(ed.session.task_run.as_ref().unwrap().output.text());
    assert!(run.len() > 1, "a rope of several nodes");
    assert_eq!(chunks(&output(&ed, Slot::Task).text), run, "the run's rope, shared");
    // (held after more comes, and after a trim)
    let r = ed.session.task_run.as_mut().unwrap();
    r.push_line(false, "more".into(), false);
    r.keep(0);
    frame(&mut ed);
    let run = chunks(ed.session.task_run.as_ref().unwrap().output.text());
    assert_eq!(chunks(&output(&ed, Slot::Task).text), run, "still the run's own");
    let d = output(&ed, Slot::Task);
    let last = mv::last_line(&d.text);
    assert_eq!(d.text.line(last - 1).to_string(), "\tleft: 3\n", "a tab kept as printed");
    assert!(frame(&mut ed).contains("    left: 3"), "drawn as the code draws a tab");
}

#[test]
fn its_lines_are_coloured_as_the_panel_coloured_them() {
    let mut ed = task(0);
    let r = ed.session.task_run.as_mut().unwrap();
    for (err, l) in
        [(false, "test a ... ok"), (false, "error: it broke"), (true, "on stderr"), (false, "plain")]
    {
        r.push_line(err, l.into(), false);
    }
    frame(&mut ed);
    let d = output(&ed, Slot::Task);
    let painted = |c: &str| d.paint.iter().filter(|s| s.2 == crate::syntax::capture_id(c)).count();
    assert_eq!((painted("diff.plus"), painted("error"), painted("ui.linenr")), (1, 1, 1), "{:?}", d.paint);
}
