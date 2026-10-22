//! Overlaps, simulated (collab C2, `overlap.rs`): two checkouts of one repository, each saving line 3 of `a.rs` its
//! own way (or putting it back) and `b.rs` far apart, while time passes — the quiet, the round on its worker landing
//! among the steps, a save racing it — and `fix`'s workspace ending, maybe while its snapshot merges, and coming back.
//! After every step: an overlap names open sessions only; a tree that moved has a round coming; a workspace ended
//! has its folder removed and never made again; and once all is still and taken, there's an overlap on `a.rs` exactly when the two changed
//! line 3 to different texts — and never on `b.rs`. A file put where fix's folder goes (its takes fail, are tried
//! again a few times, then wait for fix's next change) and taken away again: a round never stops coming for a tree
//! that moved, and what's said holds again from fix's first save after.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::Editor;

const SEEDS: &[u64] = &[0, 1, 2];

const INPUTS: usize = 50;

const LIMIT: Duration = Duration::from_secs(120);

const LINE: [&str; 3] = ["line 3", "line 3, one way", "line 3, the other way"];

#[test]
fn overlaps_follow_the_trees_and_the_workspaces() {
    super::seeds("overlaps_follow_the_trees_and_the_workspaces", SEEDS, run);
}

struct World {
    main: PathBuf,
    fix: PathBuf,
    /// fix's session — None while it's closed — and its folder as it was last open.
    session: Option<crate::event::SessionId>,
    folder: Option<PathBuf>,
    /// That folder seen removed since fix ended (its removal is a job of its own: a later step).
    gone: Cell<bool>,
    /// Line 3 of `a.rs` as each has it (`LINE`'s index): main's, fix's.
    lines: [usize; 2],
    /// A file where fix's folder goes (its takes fail) — and fix not taken since it went: what's said may be of
    /// its tree before (until its next save).
    blocked: bool,
    stale: bool,
}

fn text(line: usize, far: bool) -> String {
    (1..=12)
        .map(|n| match n {
            3 => format!("{}\n", LINE[line]),
            12 if far => "line 12, far below\n".into(),
            n => format!("line {n}\n"),
        })
        .collect()
}

fn run(choices: Choices) -> Vec<String> {
    let mut sim = Sim::new("overlaps_follow_the_trees_and_the_workspaces", choices, LIMIT);
    let dir = crate::scratch::Scratch::new("sim-overlap");
    let main = dir.join("main");
    std::fs::create_dir_all(&main).unwrap();
    let main = std::fs::canonicalize(main).unwrap();
    std::fs::write(main.join("a.rs"), text(0, false)).unwrap();
    std::fs::write(main.join("b.rs"), text(0, false)).unwrap();
    for args in [&["init", "-q", "-b", "main"][..], &["add", "-A"], &["commit", "-q", "-m", "base"]] {
        super::sessions::git(&main, &[&["-c", "user.name=t", "-c", "user.email=t@t"][..], args].concat());
    }
    super::sessions::git(&main, &["worktree", "add", "-q", "../fix"]);
    let fix = std::fs::canonicalize(dir.join("fix")).unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = main.clone();
    ed.session.name = "main".into();
    ed.session.git_branch = Some("main".into());
    ed.overlap.dir = Some(dir.join("overlap"));
    ed.open(&main.join("a.rs")).unwrap();
    let world = RefCell::new(World {
        main,
        fix,
        session: None,
        folder: None,
        gone: Cell::new(false),
        lines: [0, 0],
        blocked: false,
        stale: false,
    });
    open(&mut ed, &mut world.borrow_mut());
    let check = |ed: &Editor| held(ed, &world.borrow());
    sim.walk(&mut ed, INPUTS, |sim, ed| step(sim, ed, &mut world.borrow_mut()), check, print);
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

/// fix's session, behind, its base `main`.
fn open(ed: &mut Editor, w: &mut World) {
    let id = ed.session_add(w.fix.clone());
    if let Some(l) = ed.session_by_id(id) {
        l.name = "fix".into();
        l.base = Some("main".into());
    }
    w.session = Some(id);
    w.gone.set(false);
    // (taken anew as it opens, unless its folder's way is blocked)
    w.stale = w.blocked;
}

fn step(sim: &mut Sim, ed: &mut Editor, w: &mut World) -> String {
    let c = &mut sim.choices;
    // (by arm: fix saves a.rs 3, main saves a.rs 3, fix saves b.rs far below 1, time 4, fix ends 1, back 1, a frame 1,
    // fix's folder in the way or cleared 1)
    match c.weighted(&[3, 3, 1, 4, 1, 1, 1, 1]) {
        k @ (0 | 1) => {
            let line = *c.pick(&[0, 1, 2]);
            let (root, who) = if k == 0 { (w.fix.clone(), "fix") } else { (w.main.clone(), "main") };
            if k == 0 && w.session.is_none() {
                return "(fix is closed)".into();
            }
            w.lines[1 - k] = line;
            w.stale &= !(k == 0 && !w.blocked);
            std::fs::write(root.join("a.rs"), text(line, false)).unwrap();
            ed.wrote(&root.join("a.rs"));
            format!("{who} saves a.rs: {:?}", LINE[line])
        }
        2 => {
            if w.session.is_none() {
                return "(fix is closed)".into();
            }
            w.stale &= w.blocked;
            std::fs::write(w.fix.join("b.rs"), text(0, true)).unwrap();
            ed.wrote(&w.fix.join("b.rs"));
            "fix saves b.rs, far below".into()
        }
        3 => {
            let ms = *c.pick(&[100, 1500, 3500]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
        4 => {
            let Some(id) = w.session.take() else { return "(fix is closed)".into() };
            w.folder = ed.overlap.folder(id);
            let _ = ed.session_close(&w.fix.clone());
            "fix's workspace ends".into()
        }
        5 => {
            if w.session.is_some() {
                return "(fix is open)".into();
            }
            open(ed, w);
            "fix's workspace again".into()
        }
        // (a stand-in for a take that fails a while: a file in the test's own folder where fix's goes)
        6 => {
            let Some(f) = w.session.and_then(|id| ed.overlap.folder(id)) else {
                return "(fix is closed)".into();
            };
            match w.blocked {
                false => {
                    let _ = std::fs::remove_dir_all(&f);
                    std::fs::create_dir_all(f.parent().unwrap()).unwrap();
                    std::fs::write(&f, "in the way").unwrap();
                    (w.blocked, w.stale) = (true, true);
                    "a file where fix's folder goes".into()
                }
                true => {
                    std::fs::remove_file(&f).unwrap();
                    w.blocked = false;
                    "fix's folder's way clear".into()
                }
            }
        }
        _ => {
            let width = *c.pick(&[80, 160]);
            crate::term::render(ed, &mut Vec::new(), width, 30).unwrap();
            format!("a frame, {width} wide")
        }
    }
}

fn held(ed: &Editor, w: &World) -> Result<(), String> {
    let open: Vec<_> = std::iter::once(&ed.session).chain(&ed.sessions).map(|l| l.id).collect();
    for o in &ed.overlap.now {
        let b = match &o.b {
            crate::overlap::Other::Session(b) => Some(*b),
            crate::overlap::Other::Branch(_) => None,
        };
        if !open.contains(&o.a) || b.is_some_and(|b| !open.contains(&b)) {
            return Err(format!("an overlap of a session closed: {o:?}"));
        }
        if o.path == "b.rs" {
            return Err(format!("b.rs's changes are far apart, yet: {o:?}"));
        }
    }
    if let Some(why) = ed.overlap.unarmed(ed) {
        return Err(why);
    }
    // (an ended workspace's folder: once removed — its removal waited for a round writing there, if one was — never
    // made again by a round of before)
    if w.session.is_none()
        && let Some(f) = w.folder.as_ref()
    {
        match f.exists() {
            false => w.gone.set(true),
            true if w.gone.get() => {
                return Err(format!("fix's folder came back after it ended: {}", f.display()));
            }
            true => {}
        }
    }
    if w.session.is_some() && !w.stale && ed.overlap.settled(ed) {
        let [m, f] = w.lines;
        let conflicts = m != 0 && f != 0 && m != f;
        let said = ed.overlap.now.iter().any(|o| o.path == "a.rs");
        if said != conflicts {
            return Err(format!("settled, main {:?} and fix {:?}: overlap said {said}", LINE[m], LINE[f]));
        }
    }
    Ok(())
}

fn print(ed: &Editor) -> String {
    let now: Vec<String> = ed.overlap.now.iter().map(|o| format!("{} {:?}", o.path, o.lines)).collect();
    format!(
        "{} sessions · {} rounds · overlaps [{}]",
        ed.sessions.len() + 1,
        ed.overlap.rounds,
        now.join(", ")
    )
}
