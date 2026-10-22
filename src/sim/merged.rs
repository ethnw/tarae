//! The merged view, simulated (collab C2b, `merged.rs`): the home checkout with `a.rs` open and its merged view in
//! front, a second checkout `fix` — main typing line 3 of its buffer one way or another (unsaved, racing the merge on
//! its worker), fix saving its line 3, time passing, fix's workspace ending (maybe mid-merge) and coming back, frames
//! 80 and 160 wide. After every step the view's text is its merge's and never unsaved, and a view whose sources
//! moved has a merge coming; once it's still (its merge of
//! what's there now, none under way): its sides are open sessions only, the one whose line is changed is a side, and
//! there's a conflict exactly when main and fix changed line 3 to different texts.

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
fn the_merged_view_follows_the_buffers_and_the_workspaces() {
    super::seeds("the_merged_view_follows_the_buffers_and_the_workspaces", SEEDS, run);
}

struct World {
    fix: PathBuf,
    /// fix's session — None while it's closed.
    session: Option<crate::event::SessionId>,
    /// The merged view's document and main's own `a.rs` buffer.
    view: crate::document::DocId,
    buffer: crate::document::DocId,
    /// Line 3 of `a.rs` as each has it (`LINE`'s index): main's buffer, fix's disk.
    lines: [usize; 2],
}

fn text(line: usize) -> String {
    (1..=12).map(|n| if n == 3 { format!("{}\n", LINE[line]) } else { format!("line {n}\n") }).collect()
}

fn run(choices: Choices) -> Vec<String> {
    let mut sim = Sim::new("the_merged_view_follows_the_buffers_and_the_workspaces", choices, LIMIT);
    let dir = crate::scratch::Scratch::new("sim-merged");
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
    crate::merged::open(&mut ed, None, None).unwrap();
    let view = ed.doc().id;
    let world = RefCell::new(World { fix, session: None, view, buffer, lines: [0, 0] });
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
    // (by arm: main types line 3 in its buffer 3, fix saves its line 3 3, time 4, fix ends 1, back 1, a frame 1)
    match c.weighted(&[3, 3, 4, 1, 1, 1]) {
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
        _ => {
            let width = *c.pick(&[80, 160]);
            crate::term::render(ed, &mut Vec::new(), width, 30).unwrap();
            format!("a frame, {width} wide")
        }
    }
}

fn held(ed: &Editor, w: &World) -> Result<(), String> {
    let Some(d) = ed.docs.iter().find(|d| d.id == w.view) else { return Err("the merged view went".into()) };
    let Some(m) = crate::merged::of(d) else { return Err("no longer a merged view".into()) };
    if d.is_modified() {
        return Err("the merged view is unsaved".into());
    }
    if m.merges > 0 && d.text != m.merge.text.as_str() {
        return Err("the view's text isn't its merge's".into());
    }
    // (every step ends in an event: a view whose sources moved has its merge asked by then)
    if !crate::merged::settled(ed, w.view) {
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
    let Some(m) = ed.docs.iter().find_map(crate::merged::of) else { return "no view".into() };
    let sides: Vec<&str> = m.sides.iter().map(|s| s.name.as_str()).collect();
    format!(
        "{} merges · sides {sides:?} · {} conflicts{}",
        m.merges,
        m.merge.conflicts.len(),
        if m.asking { " · merging" } else { "" }
    )
}
