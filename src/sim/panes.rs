//! The layout kept, simulated (panes P2b, plan §5.1): the window keys that change a checkout's panes — `C-w v`/`s`,
//! `q`, `o`, `H`/`L`, `t`, `w`, `>`, `=` — and another file opened in a pane, frames, the user taking their time;
//! each change written by a worker (`layout_keep`), the writes run in any order. After every step: the file holds a
//! layout the editor asked for, never one older than what it held before; once quiet, the newest.
//!
//! P3 step 6: the tests group docked and back (`C-w H`/`L`/`K`/`J` from its list) racing its run's end and a new run
//! — the place is part of the layout written, so the newest place wins the file. After every step a docked group's
//! leaf is in the tree exactly while its run is there, once; the slot's tabs never list a group docked in sight; the
//! keys are a docked group's (`Session.hand`) only while they're in it.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use serde_json::Value;

use super::{Choices, Sim};
use crate::editor::Editor;
use crate::event::Event;
use crate::key::{Code, Key};

const SEEDS: &[u64] = &[0, 1, 2];

const INPUTS: usize = 50;

const LIMIT: Duration = Duration::from_secs(60);

#[test]
fn the_newest_layout_wins() {
    super::seeds("the_newest_layout_wins", SEEDS, run);
}

/// The layouts the editor asked to have written, by their number, and the newest the file was seen holding.
#[derive(Default)]
struct World {
    asked: RefCell<Vec<(u64, Value)>>,
    read: Cell<u64>,
}

fn run(choices: Choices) -> Vec<String> {
    let mut sim = Sim::new("the_newest_layout_wins", choices, LIMIT);
    let dir = crate::scratch::Scratch::new("sim-panes");
    let root = dir.to_path_buf();
    for f in ["a.txt", "b.txt"] {
        std::fs::write(root.join(f), format!("{f}\n")).unwrap();
    }
    let file = root.join("layouts.json");
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = root.clone();
    ed.layouts_path = Some(file.clone());
    ed.open(&root.join("a.txt")).unwrap();
    ed.session.test_run = Some(tests());
    crate::term::render(&mut ed, &mut Vec::new(), 120, 40).unwrap();
    let world = World::default();
    let key = root.to_str().unwrap().to_string();
    let check = |ed: &Editor| held(ed, &world, &file, &key);
    sim.walk(&mut ed, INPUTS, |sim, ed| step(sim, ed, &world, &key), check, print);
    // Quiet: the newest asked for is what's there
    let newest = world.asked.borrow().last().map(|a| a.1.clone());
    let kept = crate::layouts::read(Some(&file)).get(&key).cloned();
    assert_eq!(kept, newest, "once quiet, the file holds the newest layout");
    // (…and its place for the tests group, the editor's now)
    if let Some(kept) = kept {
        let place = ed.session.group(crate::pane::GroupKind::Tests).map(|g| g.place.name());
        let tests =
            kept["groups"].as_array().and_then(|gs| gs.iter().find(|g| g["kind"] == "tests").cloned());
        assert_eq!(tests.map(|g| g["place"].clone()), place.map(Value::from), "the newest place wins");
    }
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

/// The layout the editor asked to have written last, if it's a new ask — after each event the scenario sends (one
/// step sends several) and after every step (a result landing is an event too).
fn record(ed: &Editor, world: &World, key: &str) {
    let last = world.asked.borrow().last().map_or(0, |a| a.0);
    if ed.layouts_seq != last
        && let Some(l) = ed.layouts.get(key)
    {
        world.asked.borrow_mut().push((ed.layouts_seq, l.clone()));
    }
}

fn keys(sim: &Sim, ed: &mut Editor, world: &World, key: &str, keys: &str) {
    let mut chars = keys.chars();
    while let Some(ch) = chars.next() {
        let k: Key = match ch {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            ch => Key::plain(Code::Char(ch)),
        };
        sim.batch(ed, Event::Key(k));
        record(ed, world, key);
    }
}

/// A finished run of two tests, one failed: its group lays out a list.
fn tests() -> crate::testing::TestRun {
    use crate::test_results::{Case, Status};
    let case =
        |name: &str, status| Case { name: name.into(), status, secs: None, message: Vec::new(), at: None };
    let cases = vec![case("adds", Status::Passed), case("subtracts", Status::Failed)];
    crate::testing::TestRun::finished("math", cases, vec!["test result: FAILED".into()])
}

fn step(sim: &mut Sim, ed: &mut Editor, world: &World, key: &str) -> String {
    // (by arm: a window key 6, another file in the pane 2, a frame 2, time passing 1, a docking key from the tests'
    // list 3, the run ending or a new one 2)
    match sim.choices.weighted(&[6, 2, 2, 1, 3, 2]) {
        0 => {
            let k = *sim.choices.pick(&[
                "<C-w>v", "<C-w>s", "<C-w>q", "<C-w>o", "<C-w>H", "<C-w>L", "<C-w>t", "<C-w>w", "<C-w>>",
                "<C-w>=",
            ]);
            keys(sim, ed, world, key, &format!("<esc>{k}"));
            format!("keys {k}")
        }
        1 => {
            let f = *sim.choices.pick(&["a.txt", "b.txt"]);
            let path = ed.session.root.join(f);
            keys(sim, ed, world, key, &format!("<esc>:open {}<ret>", path.display()));
            format!("{f} opened in the pane")
        }
        2 => {
            let w = *sim.choices.pick(&[60, 100, 160]);
            crate::term::render(ed, &mut Vec::new(), w, 40).unwrap();
            format!("a frame, {w} columns")
        }
        4 => {
            let k = *sim.choices.pick(&["<C-w>L", "<C-w>H", "<C-w>K", "<C-w>J", "j", "<tab>"]);
            ed.list_focus(crate::pane::GroupKind::Tests);
            keys(sim, ed, world, key, k);
            format!("the tests' list, {k}")
        }
        5 => match ed.session.test_run.take() {
            Some(_) => {
                sim.batch(ed, Event::Resize);
                record(ed, world, key);
                "the run closed".into()
            }
            None => {
                ed.session.test_run = Some(tests());
                sim.batch(ed, Event::Resize);
                record(ed, world, key);
                "a new run".into()
            }
        },
        _ => {
            let ms = *sim.choices.pick(&[100, 3000]);
            sim.wait(Duration::from_millis(ms));
            "time passes".into()
        }
    }
}

/// The file holds a layout asked for (each write a job of a later step than its ask), never older than one it held
/// before.
fn held(ed: &Editor, world: &World, file: &std::path::Path, key: &str) -> Result<(), String> {
    record(ed, world, key);
    docked_held(ed)?;
    let Some(kept) = crate::layouts::read(Some(file)).get(key).cloned() else { return Ok(()) };
    let asked = world.asked.borrow();
    let seqs: Vec<u64> = asked.iter().filter(|a| a.1 == kept).map(|a| a.0).collect();
    let Some(&newest) = seqs.last() else {
        return Err(format!("the file holds a layout never asked for: {kept}"));
    };
    // (the same layout asked again later — `C-w v` then `C-w q` — is several numbers: the file is at the earliest of
    // them no older than what it held before; only when all are older did it go back)
    let read = world.read.get();
    match seqs.iter().find(|&&s| s >= read) {
        Some(&seq) => world.read.set(seq),
        None => return Err(format!("the file went back to layout {newest} after {read}")),
    }
    Ok(())
}

/// A docked group's leaf is in the tree exactly while its run is there, once, its pane with it; the slot's tabs never
/// list it while it's docked in sight; `Session.hand` names it only while the keys are in it.
fn docked_held(ed: &Editor) -> Result<(), String> {
    use crate::pane::{GroupKind, Place, Shows};
    let g = ed.session.group(GroupKind::Tests).ok_or("no tests group")?;
    let leaves = ed.session.split.leaves();
    let hosts: Vec<_> =
        ed.session.panes.iter().filter(|p| p.shows == Shows::Group(GroupKind::Tests)).collect();
    let in_tree = hosts.iter().filter(|p| leaves.contains(&p.id)).count();
    let want = usize::from(matches!(g.place, Place::Docked(_)) && ed.session.test_run.is_some());
    if in_tree != want || hosts.len() != want {
        return Err(format!(
            "{} leaves and {} panes of the docked group, want {want} ({:?})",
            in_tree,
            hosts.len(),
            g.place
        ));
    }
    if ed.docked_in_sight(GroupKind::Tests)
        && ed.slot_tabs().iter().any(|t| t.slot == crate::sizes::Slot::Test)
    {
        return Err("the slot's tabs list the group docked in sight".into());
    }
    let keys =
        ed.session.is_at(crate::session::At::Slot) || ed.session.is_at(crate::session::At::List(g.list));
    if ed.session.hand.is_some() && !keys {
        return Err(format!("the hand {:?} with the keys at {:?}", ed.session.hand, ed.session.at));
    }
    Ok(())
}

fn print(ed: &Editor) -> String {
    let place = ed.session.group(crate::pane::GroupKind::Tests).map(|g| g.place.name()).unwrap_or("-");
    format!("{} code panes · tests {place} · layout {}", ed.session.views.len(), ed.layouts_seq)
}
