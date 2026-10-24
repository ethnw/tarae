//! Shuttle keys, simulated (`shuttle.rs`): a program's steps reach the main loop as jobs, and land when the
//! seed says — among the person's keys, a card asked again (its rev bumped) or another put in front, the time
//! the person takes, the shuttle asking to go on, and its process ending (a hello after it connects anew). Each
//! step is written when it's sent, from what the shuttle read then: where the keys go, the front card and its
//! rev. After every step: a card a shuttle's key answered was the one, at the rev, its step was written for; no
//! step landed while the person's key had paused it, nor from a shuttle that had ended; a pause went only after
//! `QUIET` of the person idle; and the person's key always landed.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use super::{Choices, Sim};
use crate::editor::Editor;
use crate::key::{Code, Key};
use crate::offer::{Offer, What};
use crate::shuttle::{Input, Level, QUIET, Step};

const SEEDS: &[u64] = &[0, 1, 2, 3];

const INPUTS: usize = 60;

const LIMIT: Duration = crate::test_results::PATIENCE;

#[test]
fn shuttle_steps_among_the_persons_keys() {
    super::seeds("shuttle_steps_among_the_persons_keys", SEEDS, run);
}

/// What the scenario saw go wrong — said by the jobs as they land, failed by the check after the step.
type Wrong = Arc<Mutex<Vec<String>>>;

fn run(choices: Choices) -> Vec<String> {
    let mut sim = Sim::new("shuttle_steps_among_the_persons_keys", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    ed.docs[0].text = ropey::Rope::from_str(&"line\n".repeat(40));
    crate::shuttle::connect_for_test(&mut ed, "sim", Level::Light);
    let wrong: Wrong = Arc::default();
    let check = |_: &Editor| match wrong.lock().unwrap().first() {
        Some(w) => Err(w.clone()),
        None => Ok(()),
    };
    sim.walk(&mut ed, INPUTS, |sim, ed| input(sim, ed, &wrong), check, print);
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

/// The front card, and its rev.
fn front(ed: &Editor) -> Option<(u64, u64)> {
    ed.offers.first().map(|o| (o.id, o.rev))
}

fn input(sim: &mut Sim, ed: &mut Editor, wrong: &Wrong) -> String {
    let c = &mut sim.choices;
    // (by arm: a card asked (again) 3, a step queued 4, the person's key 2, time passing 2, a resume queued 2,
    // its process ending 1, a frame 1)
    match c.weighted(&[3, 4, 2, 2, 2, 1, 1]) {
        0 => {
            // (asked again under its title: the same card, its rev bumped; another's put in front)
            let title = *c.pick(&["End the review?", "End the other review?"]);
            let what = What::ReviewEnd { session: 0, started: std::time::Instant::now() };
            ed.push_offer(Offer::new(title.into(), "2 files not viewed.", "", String::new(), what));
            format!("card {title:?} asked · front {:?}", front(ed))
        }
        1 => {
            // Written from what it reads now; landing whenever the seed says
            let Some(id) = ed.shuttles.on.as_ref().map(|s| s.id) else { return "(no shuttle)".into() };
            let to = ed.keys_to().to;
            let mut expect = json!({ "keys": { "to": to } });
            let read = front(ed).filter(|_| to.starts_with("card#"));
            if let Some((card, rev)) = read {
                expect[format!("card#{card}")] = json!({ "rev": rev });
            }
            let k = *c.pick(&["y", "n", "j"]);
            let step = Step { id, input: Input::Keys(vec![k.parse().unwrap()], k.into()), expect };
            let wrong = wrong.clone();
            let job = move |ed: &mut Editor| landed(ed, step, read, &wrong);
            ed.events.shared_sender().send(crate::event::Event::Job(Box::new(job))).unwrap();
            format!("step {k} → {to} queued")
        }
        2 => {
            let input = ed.input_seq;
            ed.handle_event(crate::event::Event::Key(Key::plain(Code::Char('l'))));
            if ed.input_seq != input + 1 {
                wrong.lock().unwrap().push("the person's key didn't land".into());
            }
            if ed.shuttles.on.as_ref().is_some_and(|s| s.paused.is_none()) {
                wrong.lock().unwrap().push("the person's key didn't pause the shuttle".into());
            }
            "the person's l".into()
        }
        3 => {
            let ms = *c.pick(&[300, 2000, 6000]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
        4 => {
            let Some(id) = ed.shuttles.on.as_ref().map(|s| s.id) else { return "(no shuttle)".into() };
            let wrong = wrong.clone();
            let job = move |ed: &mut Editor| {
                let idle = ed.shuttles.person_idle();
                let said = crate::shuttle::resume(ed, id);
                if said["shuttle"] == "resumed" && idle.is_some_and(|d| d < QUIET) {
                    wrong.lock().unwrap().push(format!("resumed after {idle:?} of the person idle"));
                }
            };
            ed.events.shared_sender().send(crate::event::Event::Job(Box::new(job))).unwrap();
            "resume queued".into()
        }
        5 => {
            // (its process ends — the watch's job; the program, started again, says hello and is granted anew)
            let of = ed.shuttles.on.as_ref().map(|s| s.of);
            let job = move |ed: &mut Editor| {
                if let Some(of) = of {
                    crate::shuttle::process_ended(ed, of);
                }
                crate::shuttle::connect_for_test(ed, "sim", Level::Light);
            };
            ed.events.shared_sender().send(crate::event::Event::Job(Box::new(job))).unwrap();
            "its process ends (queued)".into()
        }
        6 => {
            crate::term::render(ed, &mut Vec::new(), 100, 30).unwrap();
            "a frame".into()
        }
        _ => unreachable!("one arm a weight"),
    }
}

/// The step lands: what the editor was as it landed, then the step, then what its answer says against that.
fn landed(ed: &mut Editor, step: Step, read: Option<(u64, u64)>, wrong: &Wrong) {
    let (paused, on) =
        (ed.shuttles.on.as_ref().map(|s| s.paused.is_some()), ed.shuttles.on.as_ref().map(|s| s.id));
    let (at, id) = (front(ed), step.id);
    let (said, _): (Value, bool) = crate::shuttle::run(ed, step);
    let mut w = wrong.lock().unwrap();
    if said.get("landed").is_none() {
        return;
    }
    if paused == Some(true) {
        w.push(format!("a step landed while paused: {said}"));
    }
    if on != Some(id) {
        w.push(format!("a step of shuttle {id} landed with {on:?} connected: {said}"));
    }
    // (a card answered: the one at the rev it read)
    if said["took"] == "card" && at != read {
        w.push(format!("it answered {at:?}, written for {read:?}: {said}"));
    }
}

fn print(ed: &Editor) -> String {
    let s = ed.shuttles.on.as_ref().map(|s| (s.id, s.paused.is_some()));
    format!(
        "cards {:?} · shuttle {s:?} · line {}",
        ed.offers.iter().map(|o| (o.id, o.rev)).collect::<Vec<_>>(),
        ed.cursor_line()
    )
}
