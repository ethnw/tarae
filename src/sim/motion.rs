//! A flash's clock (`motion.rs`), simulated: edits, undo and redo, `.`, yanks, frames, ticks and time passing in any
//! order. After every step: a flash drawn is on the document as it is (its ranges inside the text); the wake asked is
//! on a flash's own clock (`born + k·TICK`, 2 ms past) and never further than a step away, or now for one that's over;
//! a tick served lets every flash past its time go; with none left nothing is asked — every wake ends. A two-change
//! edit review answered presses its first head's stamp (§5), on the same clock; a turn's file's one change taken
//! back presses its `u` once its diff lands (a job), and no reset is left waiting on a diff already landed.

use std::cell::Cell;
use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::{Editor, TICK};
use crate::key::Key;
use crate::motion::{What, shown_for};

const SEEDS: &[u64] = &[0, 1, 2, 7, 10];

const INPUTS: usize = 60;

const LIMIT: Duration = Duration::from_secs(60);

#[test]
fn a_flash_wakes_on_its_own_steps_and_ends() {
    super::seeds("a_flash_wakes_on_its_own_steps_and_ends", SEEDS, run);
}

fn run(choices: Choices) -> Vec<String> {
    let mut sim = Sim::new("a_flash_wakes_on_its_own_steps_and_ends", choices, LIMIT);
    sim.frames = Some((80, 16));
    let mut ed = Editor::new(crate::config::Config::default());
    ed.docs[0].text = ropey::Rope::from_str("alpha beta\ngamma delta\nepsilon\nzeta eta\n");
    ed.motion_drawn = true;
    // (a tick served with a flash past its time left it standing: the next check says so)
    let stuck = Cell::new(None::<String>);
    let check = |ed: &Editor| held(ed, &stuck);
    sim.walk(&mut ed, INPUTS, |sim, ed| step(sim, ed, &stuck), check, print);
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

fn keys(sim: &Sim, ed: &mut Editor, keys: &[&str]) {
    for k in keys {
        sim.batch(ed, crate::event::Event::Key(k.parse::<Key>().unwrap()));
    }
}

fn step(sim: &mut Sim, ed: &mut Editor, stuck: &Cell<Option<String>>) -> String {
    let c = &mut sim.choices;
    // (by arm: an edit 3, undo 3, redo 2, `.` 2, a yank 2, a frame 1, a tick 4, time passing 4, a review answered 2,
    // a turn's change taken back 2)
    match c.weighted(&[3, 3, 2, 2, 2, 1, 4, 4, 2, 2]) {
        0 => {
            let at = *c.pick(&["k", "j", "C", "w"]);
            keys(sim, ed, &[at, "i", "X", "esc"]);
            format!("{at} then an insert")
        }
        1 => {
            keys(sim, ed, &["u"]);
            "undo".into()
        }
        2 => {
            keys(sim, ed, &["U"]);
            "redo".into()
        }
        3 => {
            keys(sim, ed, &["."]);
            "repeat".into()
        }
        4 => {
            keys(sim, ed, &["x", "y"]);
            "a yank".into()
        }
        5 => {
            crate::term::render(ed, &mut Vec::new(), 80, 16).unwrap();
            "a frame".into()
        }
        6 => {
            // (as the main loop's wake: its tick, then a frame — one past a flash's time lets it go)
            let over = ed.motions.iter().any(|m| crate::clock::since(m.born) >= shown_for(m.life));
            ed.schedule_tick();
            crate::term::render(ed, &mut Vec::new(), 80, 16).unwrap();
            if let Some(m) = ed.motions.iter().find(|m| crate::clock::since(m.born) >= shown_for(m.life)) {
                stuck.set(Some(format!("a tick left a flash {:?} old", crate::clock::since(m.born))));
            } else if over && ed.unchanged {
                stuck.set(Some("a flash let go, and the frame said nothing changed".into()));
            }
            "a tick".into()
        }
        7 => {
            let ms = *c.pick(&[30, 100, 102, 150, 250, 1000]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
        8 => {
            // (§5: a two-change review, its first change answered — its head's stamp pressed — a frame, the second
            // answered: the review ends, the press runs out on its own clock)
            let first = *c.pick(&["y", "n"]);
            let doc = ed.doc();
            let (id, n) = (doc.id, doc.text.len_bytes());
            let (a, b) = (doc.text.byte_slice(0..1).to_string(), doc.text.byte_slice(n - 1..n).to_string());
            let changes = vec![(0, 1, a, "Q".into()), (n - 1, n, b, "Z\n".into())];
            crate::llm::review_with(ed, id, "a review", changes, None);
            keys(sim, ed, &[first]);
            crate::term::render(ed, &mut Vec::new(), 80, 16).unwrap();
            keys(sim, ed, &["n"]);
            format!("a review: {first}, a frame, n")
        }
        _ => {
            // (§5: a turn's review of the file, its first line its one change; `space v u` on it takes it back — the
            // `u` pressed once the diff lands, a job delivered at the simulation's pace)
            turn_review(ed);
            keys(sim, ed, &["g", "g", "space", "v", "u"]);
            "a turn's one change taken back".into()
        }
    }
}

/// A turn's review of the document (a path of its own), against its text with the first line another: that line its
/// one change.
fn turn_review(ed: &mut Editor) {
    use crate::codereview::{Base, CodeReview, File};
    let path = std::path::PathBuf::from("/nowhere/sim/a.rs");
    let doc = ed.doc_mut();
    doc.path = Some(path.clone());
    let text = doc.text.to_string();
    let base = format!("base\n{}", text.split_once('\n').map_or("", |(_, rest)| rest));
    doc.review_base = Some(ropey::Rope::from_str(&base));
    doc.git_base = Some(std::sync::Arc::new(base.clone()));
    doc.git_hunks = crate::git::hunks(&base, &text);
    doc.git_version = doc.version();
    let id = doc.id;
    let file =
        File { rel: "a.rs".into(), path, added: 1, removed: 1, new: false, gone: false, binary: false };
    let turn = Base::Turn { at: 0, question: "do it".into() };
    let mut r = CodeReview::for_test(turn, "/nowhere/sim".into(), vec![file]);
    r.docs.push(id);
    ed.session.code_review = Some(r);
    // (its diff set here, as a diff landing)
    ed.diff_landed(id, ed.doc().version());
}

/// A flash on the document as it is lies inside its text; the wake is on a flash's own steps, at most a step away (now
/// for one over); none asked with none left.
fn held(ed: &Editor, stuck: &Cell<Option<String>>) -> Result<(), String> {
    if let Some(s) = stuck.take() {
        return Err(s);
    }
    let n = ed.doc().text.len_bytes();
    for m in &ed.motions {
        let What::Flash { doc, version, ranges, .. } = &m.what else { continue };
        if (*doc, *version) == (ed.doc().id, ed.doc().version())
            && ranges.iter().any(|r| r.end > n || r.is_empty())
        {
            return Err(format!("a flash's ranges {ranges:?} outside a text of {n} bytes"));
        }
    }
    if let Some((id, at)) = ed.press_on_diff
        && let Some(d) = ed.docs.iter().find(|d| d.id == id)
        && d.git_version != u64::MAX
        && d.git_version >= at
    {
        return Err(format!("a reset's press waits on version {at}, its diff landed at {}", d.git_version));
    }
    let wake = ed.motion_wake();
    if ed.motions.is_empty() {
        return match wake {
            Some(w) => Err(format!("no flash left, a wake asked in {w:?}")),
            None => Ok(()),
        };
    }
    let Some(w) = wake else { return Err("a flash under way, and no wake asked".into()) };
    let slack = Duration::from_millis(2);
    let on_a_step = ed.motions.iter().any(|m| {
        let age = crate::clock::since(m.born);
        let over = age >= shown_for(m.life);
        (over && w.is_zero())
            || (!over && (age + w).saturating_sub(slack).as_nanos().is_multiple_of(TICK.as_nanos()))
    });
    if !on_a_step || w > TICK + slack {
        return Err(format!("a wake in {w:?}, on no flash's own step"));
    }
    Ok(())
}

fn print(ed: &Editor) -> String {
    let ages: Vec<u128> = ed.motions.iter().map(|m| crate::clock::since(m.born).as_millis()).collect();
    format!("motions {ages:?} ms old · wake {:?}", ed.motion_wake())
}
