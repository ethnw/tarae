//! A watcher of the UI state through cards that come where nothing on screen changes, simulated (watchlate: a
//! watcher heard of a grant card 20 minutes late — `CARD WAITS 1178s` — because watchers were told only after a
//! frame, and a card parked in a session behind draws none). Two sessions, a watcher connected; at random Claude's
//! work behind or in front brings a card (a job, its result in that session), a card behind goes, a key, a switch
//! of sessions, the user taking their time (a card's nudge comes) — every event handled as the main loop does, a
//! frame only when one is due. After every step: every card up or parked was heard as `card-up`, and nothing a
//! watcher would be told waits for a later frame.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use serde_json::Value;

use super::{Choices, Sim};
use crate::editor::Editor;
use crate::event::{Event, SessionId};
use crate::key::{Code, Key};
use crate::offer::{Offer, What};
use crate::uistate::watch;

/// The seeds every `cargo test` runs, each twice (no git, no process: a run is quick).
const SEEDS: &[u64] = &[0, 1, 2, 3];

/// The scenario's own steps a run.
const INPUTS: usize = 60;

/// A run's real time at most.
const LIMIT: Duration = crate::test_results::PATIENCE;

#[test]
fn a_watcher_hears_a_card_that_draws_nothing() {
    super::seeds("a_watcher_hears_a_card_that_draws_nothing", SEEDS, run);
}

/// What the watcher heard: its lines as they came, and the cards it was told are up.
struct Heard {
    rx: Receiver<String>,
    lines: Vec<Value>,
    up: BTreeSet<String>,
}

impl Heard {
    fn take(&mut self) {
        for l in self.rx.try_iter() {
            let v: Value = serde_json::from_str(&l).unwrap();
            if v["event"] == "card-up" {
                self.up.insert(v["id"].as_str().unwrap_or_default().to_string());
            }
            self.lines.push(v);
        }
    }
}

/// One run of the scenario; its trace.
fn run(choices: Choices) -> Vec<String> {
    let dir = crate::scratch::Scratch::new("sim-watchers");
    let (front, behind) = (dir.join("front"), dir.join("behind"));
    std::fs::create_dir_all(&front).unwrap();
    std::fs::create_dir_all(&behind).unwrap();
    let mut sim = Sim::new("a_watcher_hears_a_card_that_draws_nothing", choices, LIMIT);
    sim.frames = Some((112, 30));
    let mut ed = Editor::new(crate::config::Config::default());
    (ed.session.root, ed.session.name) = (std::fs::canonicalize(&front).unwrap(), "front".into());
    let other = ed.session_open(&behind).unwrap();
    ed.session_switch(0);
    crate::term::render(&mut ed, &mut Vec::new(), 112, 30).unwrap();
    let (tx, rx) = std::sync::mpsc::sync_channel(watch::UNREAD);
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    ed.ui_watch_add(watch::Watcher { tx, kinds: Vec::new(), overflowed: flag });
    let heard = RefCell::new(Heard { rx, lines: Vec::new(), up: BTreeSet::new() });
    let ids = [0, other];
    let check = |ed: &Editor| told(ed, &mut heard.borrow_mut());
    let print = |ed: &Editor| {
        let mut heard = heard.borrow_mut();
        heard.take();
        print(ed, &heard)
    };
    sim.walk(&mut ed, INPUTS, |sim, ed| input(sim, ed, &ids), check, print);
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

/// A card of Claude's (one holding its call, as a grant card does) — the `n`th.
fn card(n: u64) -> Offer {
    let what = What::Screen { call: crate::tools::Call::test(n) };
    Offer::new(format!("Claude asks {n}"), "to see the screen", "", String::new(), what)
}

/// One of the scenario's steps, said.
fn input(sim: &mut Sim, ed: &mut Editor, ids: &[SessionId]) -> String {
    let c = &mut sim.choices;
    // (by arm: a card from work behind 4, from work in front 2, a card behind goes 2, `n` 2, a switch 2, time 2)
    let arm = c.weighted(&[4, 2, 2, 2, 2, 2]);
    let n = sim.steps as u64 + 1;
    // (Claude's work in a session: a job whose result brings the card, in that session — started from an event of
    // that session's, as a run's end starts the next)
    let work =
        move |ed: &mut Editor| ed.events.jobs().spawn(move || move |ed: &mut Editor| ed.push_offer(card(n)));
    let behind = || *ids.iter().find(|id| **id != ed.session.id).unwrap();
    let (ev, said) = match arm {
        0 => (Event::For(behind(), Box::new(work)), format!("work behind will ask {n}")),
        1 => (Event::Job(Box::new(work)), format!("work in front will ask {n}")),
        // (its call answered or dropped as the turn ended, in the session behind)
        2 => {
            let gone = |ed: &mut Editor| {
                if let Some(o) = ed.session.parked.pop() {
                    ed.card_how(o.id, "dropped as the turn ended");
                }
            };
            (Event::For(behind(), Box::new(gone)), "a card behind goes".into())
        }
        3 => (Event::Key(Key::plain(Code::Char('n'))), "n".into()),
        4 => {
            let to = behind();
            (
                Event::Job(Box::new(move |ed: &mut Editor| _ = ed.session_switch(to))),
                format!("session {to} in front"),
            )
        }
        _ => {
            let d = *c.pick(&[
                Duration::from_millis(300),
                Duration::from_millis(1000),
                crate::session::NUDGE_AFTER,
            ]);
            sim.wait(d);
            return format!("{} ms pass", d.as_millis());
        }
    };
    sim.batch(ed, ev);
    said
}

/// Every card up or parked was told as it came, and nothing waits to be told.
fn told(ed: &Editor, heard: &mut Heard) -> Result<(), String> {
    heard.take();
    let parked = std::iter::once(&ed.session).chain(&ed.sessions).flat_map(|l| &l.parked);
    if let Some(o) = ed.offers.iter().chain(parked).find(|o| !heard.up.contains(&format!("card#{}", o.id))) {
        return Err(format!("card#{} ({}) is waiting, and the watcher wasn't told", o.id, o.title));
    }
    match watch::owed(ed) {
        Some(owed) => Err(format!("a watcher waits for a frame to hear: {owed}")),
        None => Ok(()),
    }
}

/// What a trace line ends with: the session in front, the cards up and parked, the watcher's last line.
fn print(ed: &Editor, heard: &Heard) -> String {
    let ids = |os: &[Offer]| os.iter().map(|o| format!("#{}", o.id)).collect::<Vec<_>>().join(" ");
    let parked: Vec<String> = std::iter::once(&ed.session)
        .chain(&ed.sessions)
        .filter(|l| !l.parked.is_empty())
        .map(|l| format!("{}: {}", l.id, ids(&l.parked)))
        .collect();
    let last = heard.lines.last().map(|v| v["said"].as_str().unwrap_or_default().to_string());
    format!(
        "front {} · up [{}] · parked [{}] · frames {} · heard {} {last:?}",
        ed.session.id,
        ids(&ed.offers),
        parked.join("; "),
        ed.frames,
        heard.lines.len()
    )
}
