//! The disk watch reading another program's writes, simulated (frictions29 90 a): an open file, and at random a
//! write that begins (truncated, its first line) or ends (the rest), a disk round asked (a focus regain's — its own
//! stat finds what changed; no waiting round, whose wait is the OS's), the user taking their time, a frame — the
//! round's settle a sleep on the simulated clock among them. After every step: the buffer holds a write's first
//! half only where the file stayed half-done for `disk::SETTLE` at least (a writer that stopped there).

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::{Choices, Sim};
use crate::editor::Editor;

const SEEDS: &[u64] = &[0, 1, 2];

const INPUTS: usize = 40;

const LIMIT: Duration = Duration::from_secs(60);

#[test]
fn a_write_is_read_once_it_settles() {
    super::seeds("a_write_is_read_once_it_settles", SEEDS, run);
}

/// Each write's first half: its text, when it was made, and when the rest came (None: not yet).
type Halves = RefCell<Vec<(String, Instant, Option<Instant>)>>;

fn run(choices: Choices) -> Vec<String> {
    let dir = crate::scratch::Scratch::new("sim-diskwatch");
    let path = dir.join("a.txt");
    std::fs::write(&path, "before\n").unwrap();
    let mut sim = Sim::new("a_write_is_read_once_it_settles", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    ed.open(&path).unwrap();
    let halves = Halves::default();
    sim.walk(
        &mut ed,
        INPUTS,
        |sim, ed| input(sim, ed, &path, &halves),
        |ed| held(ed, &halves),
        |ed| {
            format!("buffer {:?} · file {:?}", ed.doc().text.to_string(), std::fs::read_to_string(&path).ok())
        },
    );
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

fn input(sim: &mut Sim, ed: &mut Editor, path: &PathBuf, halves: &Halves) -> String {
    let c = &mut sim.choices;
    let under_way = halves.borrow().last().is_some_and(|h| h.2.is_none());
    // (by arm: a write begins 2, it ends 2, a round 3, time 2, a frame 1)
    match c.weighted(&[2, 2, 3, 2, 1]) {
        0 if !under_way => {
            let n = halves.borrow().len();
            let half = format!("one {n}\n");
            std::fs::write(path, &half).unwrap();
            halves.borrow_mut().push((half, crate::clock::now(), None));
            format!("write {n} begins")
        }
        1 if under_way => {
            let mut h = halves.borrow_mut();
            let last = h.last_mut().expect("under way");
            std::fs::write(path, format!("{}two\n", last.0)).unwrap();
            last.2 = Some(crate::clock::now());
            "the write ends".into()
        }
        0 | 1 => "(nothing to write)".into(),
        2 => {
            ed.on_focus(true);
            // (this round only: its landing carries on no waiting round — the OS's wait, not modelled)
            ed.disk_poll_gen += 1;
            "a disk round".into()
        }
        3 => {
            let ms = *c.pick(&[10, 30, 60, 200]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
        _ => {
            crate::term::render(ed, &mut Vec::new(), 100, 30).unwrap();
            "a frame".into()
        }
    }
}

/// The buffer holds a write's first half only if the file stayed that way for `SETTLE` at least.
fn held(ed: &Editor, halves: &Halves) -> Result<(), String> {
    let text = ed.doc().text.to_string();
    let now = crate::clock::now();
    for (half, from, to) in halves.borrow().iter().filter(|h| h.0 == text) {
        let lasted = to.unwrap_or(now).saturating_duration_since(*from);
        if lasted < crate::disk::SETTLE {
            return Err(format!("read half-done: {half:?}, the file that way for {lasted:?}"));
        }
    }
    Ok(())
}
