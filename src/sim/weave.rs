//! The woven view, simulated (collab C2b, `weave.rs`): the home checkout with `a.rs` open and its woven view in
//! front, a second checkout `fix` — main typing line 3 of its buffer one way or another (unsaved, racing the merge on
//! its worker), fix saving its line 3, time passing, fix's workspace ending (maybe mid-merge) and coming back, frames
//! 80 and 160 wide, `enter` on a line of the view (racing fix's end and its file changing). After every step the view's
//! text is its merge's and never unsaved, and a view whose sources moved has a merge coming; once it's still (its merge
//! of what's there now, none under way): its sides are open sessions only, the one whose line is changed is a side,
//! and there's a conflict exactly when main and fix changed line 3 to different texts. An `enter` either says why it
//! stays or lands in an open session's own `a.rs`, on a line it has.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::Editor;

const SEEDS: &[u64] = &[0, 1, 2];

const INPUTS: usize = 50;

const LIMIT: Duration = Duration::from_secs(120);

const LINE: [&str; 3] = ["line 3", "line 3, one way", "line 3, the other way"];

#[test]
fn the_woven_view_follows_the_buffers_and_the_workspaces() {
    super::seeds("the_woven_view_follows_the_buffers_and_the_workspaces", SEEDS, run);
}

struct World {
    fix: PathBuf,
    /// fix's session — None while it's closed.
    session: Option<crate::event::SessionId>,
    /// The woven view's document and main's own `a.rs` buffer.
    view: crate::document::DocId,
    buffer: crate::document::DocId,
    /// Line 3 of `a.rs` as each has it (`LINE`'s index): main's buffer, fix's disk.
    lines: [usize; 2],
    /// main's session, and what the last `enter` did wrong (None: nothing).
    main: crate::event::SessionId,
    enter: Option<String>,
}

fn text(line: usize) -> String {
    (1..=12).map(|n| if n == 3 { format!("{}\n", LINE[line]) } else { format!("line {n}\n") }).collect()
}

fn run(choices: Choices) -> Vec<String> {
    let mut sim = Sim::new("the_woven_view_follows_the_buffers_and_the_workspaces", choices, LIMIT);
    let dir = crate::scratch::Scratch::new("sim-weave");
    let main = dir.join("main");
    std::fs::create_dir_all(&main).unwrap();
    let main = std::fs::canonicalize(main).unwrap();
    std::fs::write(main.join("a.rs"), text(0)).unwrap();
    for args in [&["init", "-q", "-b", "main"][..], &["add", "-A"], &["commit", "-q", "-m", "base"]] {
        super::sessions::git(&main, &[&["-c", "user.name=t", "-c", "user.email=t@t"][..], args].concat());
    }
    super::sessions::git(&main, &["worktree", "add", "-q", "../fix"]);
    let fix = std::fs::canonicalize(dir.join("fix")).unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = main.clone();
    ed.session.name = "main".into();
    ed.session.git_branch = Some("main".into());
    ed.open(&main.join("a.rs")).unwrap();
    let buffer = ed.doc().id;
    crate::weave::open(&mut ed, None, None).unwrap();
    let view = ed.doc().id;
    let main = ed.session.id;
    let world = RefCell::new(World { fix, session: None, view, buffer, lines: [0, 0], main, enter: None });
    open(&mut ed, &mut world.borrow_mut());
    let check = |ed: &Editor| held(ed, &world.borrow());
    sim.walk(&mut ed, INPUTS, |sim, ed| step(sim, ed, &mut world.borrow_mut()), check, print);
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

/// fix's session, behind, its base `main` — its line 3 as its disk has it.
fn open(ed: &mut Editor, w: &mut World) {
    let id = ed.session_add(w.fix.clone());
    if let Some(l) = ed.session_by_id(id) {
        l.name = "fix".into();
        l.base = Some("main".into());
    }
    w.session = Some(id);
    let now = std::fs::read_to_string(w.fix.join("a.rs")).unwrap_or_default();
    w.lines[1] = LINE.iter().rposition(|l| now.lines().nth(2) == Some(*l)).unwrap_or(0);
}

fn step(sim: &mut Sim, ed: &mut Editor, w: &mut World) -> String {
    let c = &mut sim.choices;
    // (by arm: main types line 3 in its buffer 3, fix saves its line 3 3, time 4, fix ends 1, back 1, a frame 1,
    // enter on a line 2)
    match c.weighted(&[3, 3, 4, 1, 1, 1, 2]) {
        0 => {
            let line = *c.pick(&[0, 1, 2]);
            w.lines[0] = line;
            let Some(d) = ed.docs.iter_mut().find(|d| d.id == w.buffer) else { return "(no buffer)".into() };
            let old = d.text.clone();
            if let Some(change) = crate::disk::minimal_change(&old, &ropey::Rope::from_str(&text(line))) {
                d.apply(&crate::transaction::Transaction::new(vec![change]));
            }
            ed.handle_event(crate::event::Event::Resize);
            format!("main types line 3: {:?}", LINE[line])
        }
        1 => {
            if w.session.is_none() {
                return "(fix is closed)".into();
            }
            let line = *c.pick(&[0, 1, 2]);
            w.lines[1] = line;
            std::fs::write(w.fix.join("a.rs"), text(line)).unwrap();
            ed.wrote(&w.fix.join("a.rs"));
            ed.handle_event(crate::event::Event::Resize);
            format!("fix saves line 3: {:?}", LINE[line])
        }
        2 => {
            let ms = *c.pick(&[5, 100, 1500]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
        3 => {
            if w.session.take().is_none() {
                return "(fix is closed)".into();
            }
            let _ = ed.session_close(&w.fix.clone());
            ed.handle_event(crate::event::Event::Resize);
            "fix's workspace ends".into()
        }
        4 => {
            if w.session.is_some() {
                return "(fix is open)".into();
            }
            open(ed, w);
            ed.handle_event(crate::event::Event::Resize);
            "fix's workspace again".into()
        }
        5 => {
            let width = *c.pick(&[80, 160]);
            crate::term::render(ed, &mut Vec::new(), width, 30).unwrap();
            format!("a frame, {width} wide")
        }
        _ => {
            // (lines 3–5 — the changed one, a conflict's parts — most of the time)
            let n = ed.doc().text.len_lines().max(1);
            let line = match c.below(4) {
                k @ 0..=2 => 2 + k,
                _ => c.below(n),
            };
            ed.goto_line(line);
            let said = ed.said;
            ed.handle_event(crate::event::Event::Key(crate::key::Key::plain(crate::key::Code::Enter)));
            w.enter = went(ed, w, said);
            let to = match ed.doc().id == w.view {
                true => "stays".to_string(),
                false => format!("{}'s a.rs:{}", ed.session.label(), ed.cursor_line() + 1),
            };
            // (back in main's workspace with the view in front — fix's buffer of it closed: its disk stays the truth)
            if ed.session.id != w.main {
                ed.close_current(true).ok();
                ed.session_switch(w.main);
            }
            if let Some(i) = ed.docs.iter().position(|d| d.id == w.view) {
                ed.current = i;
            }
            ed.handle_event(crate::event::Event::Resize);
            format!("enter on line {} → {to}", line + 1)
        }
    }
}

/// What an `enter` on the view did wrong: stayed without a word, or landed outside an open session's `a.rs` or past
/// its lines.
fn went(ed: &Editor, w: &World, said: u64) -> Option<String> {
    if ed.doc().id == w.view {
        return (ed.said == said).then(|| "enter stayed, unsaid".to_string());
    }
    let want = ed.session.root.join("a.rs");
    if ed.doc().path.as_deref() != Some(want.as_path()) {
        return Some(format!("enter landed in {:?}", ed.doc().path));
    }
    (ed.cursor_line() > crate::movement::last_line(&ed.doc().text))
        .then(|| "enter past the file's end".into())
}

fn held(ed: &Editor, w: &World) -> Result<(), String> {
    if let Some(e) = &w.enter {
        return Err(e.clone());
    }
    let Some(d) = ed.docs.iter().find(|d| d.id == w.view) else { return Err("the woven view went".into()) };
    let Some(m) = crate::weave::of(d) else { return Err("no longer a woven view".into()) };
    if d.is_modified() {
        return Err("the woven view is unsaved".into());
    }
    if m.merges > 0 && d.text != m.merge.text.as_str() {
        return Err("the view's text isn't its merge's".into());
    }
    // (every step ends in an event: a view whose sources moved has its merge asked by then)
    if !crate::weave::settled(ed, w.view) {
        return match m.asking {
            true => Ok(()),
            false => Err("what it's merged from moved, and no merge is coming".into()),
        };
    }
    let open: Vec<_> = std::iter::once(&ed.session).chain(&ed.sessions).map(|l| l.id).collect();
    if let Some(s) = m.sides.iter().find(|s| !open.contains(&s.session)) {
        return Err(format!("still, a side of a session closed: {}", s.name));
    }
    let [main, fix] = w.lines;
    let fix = if w.session.is_some() { fix } else { 0 };
    let conflict = main != 0 && fix != 0 && main != fix;
    if m.merge.conflicts.is_empty() == conflict {
        return Err(format!(
            "still, main {:?} and fix {:?}: {} conflicts",
            LINE[main],
            LINE[fix],
            m.merge.conflicts.len()
        ));
    }
    let named = |n: &str| m.sides.iter().any(|s| s.name == n);
    if (main != 0) != named("main") || (fix != 0) != named("fix") {
        let sides: Vec<&str> = m.sides.iter().map(|s| s.name.as_str()).collect();
        return Err(format!("still, main {:?} and fix {:?}: sides {sides:?}", LINE[main], LINE[fix]));
    }
    Ok(())
}

fn print(ed: &Editor) -> String {
    let Some(m) = ed.docs.iter().find_map(crate::weave::of) else { return "no view".into() };
    let sides: Vec<&str> = m.sides.iter().map(|s| s.name.as_str()).collect();
    format!(
        "{} merges · sides {sides:?} · {} conflicts{}",
        m.merges,
        m.merge.conflicts.len(),
        if m.asking { " · weaving" } else { "" }
    )
}
