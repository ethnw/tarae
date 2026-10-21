//! A card that comes while keys are coming in waits for them to go quiet (friction 60), simulated: keys at random
//! gaps (`j`, `k`, `y`, `C-q`), light cards coming between them — some asked by the key just before (the user's
//! own, which never wait) — a second session switched to and back (a waiting card parked, then up again), frames,
//! and the user taking their time, among the quiet-keys wake's job.
//!
//! The scenario keeps its own model of what should wait, from what it did alone: a card waits when it came within
//! `KEYS_QUIET` of a key, not asked by that key, until the keys have been quiet that long. Each `y` is judged by
//! it — one that reaches a card that should wait must leave it unanswered, one that reaches a card that shouldn't
//! must answer it (light cards: `y` ends what never began, the card goes). After every step a card that waits,
//! up or parked, has its wake under way (else it would wait until the next key).

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use super::{Choices, Sim};
use crate::editor::Editor;
use crate::key::{Code, Key};
use crate::offer::{KEYS_QUIET, Offer, What};

/// (7: a wake whose frame must be drawn; 10: a waiting card parked by a session switch, the wake's re-arm)
const SEEDS: &[u64] = &[0, 1, 2, 7, 10];

const INPUTS: usize = 50;

const LIMIT: Duration = Duration::from_secs(60);

#[test]
fn a_card_waits_for_the_keys_to_go_quiet() {
    super::seeds("a_card_waits_for_the_keys_to_go_quiet", SEEDS, run);
}

/// What the scenario knows from its own steps: when its last key went, which cards should wait, the two sessions.
#[derive(Default)]
struct Model {
    last_key: Option<Instant>,
    waits: BTreeMap<u64, bool>,
    sessions: (crate::event::SessionId, crate::event::SessionId),
}

impl Model {
    /// The keys quiet `KEYS_QUIET` since the last: nothing waits any more.
    fn quiet_sync(&mut self) {
        if self.last_key.is_none_or(|t| crate::clock::since(t) >= KEYS_QUIET) {
            self.waits.values_mut().for_each(|w| *w = false);
        }
    }

    fn should_wait(&self, id: u64) -> bool {
        self.waits.get(&id).copied().unwrap_or(false)
    }
}

fn run(choices: Choices) -> Vec<String> {
    let dir = crate::scratch::Scratch::new("sim-cardwait");
    let mut sim = Sim::new("a_card_waits_for_the_keys_to_go_quiet", choices, LIMIT);
    sim.frames = Some((100, 24));
    let mut ed = Editor::new(crate::config::Config::default());
    ed.keys_hold = true;
    let main = ed.session.id;
    let other = ed.session_open(dir.path()).unwrap();
    ed.session_switch(main);
    let model = RefCell::new(Model { sessions: (main, other), ..Model::default() });
    let wrong = Cell::new(None);
    let check = |ed: &Editor| held(ed, &wrong);
    sim.walk(&mut ed, INPUTS, |sim, ed| input(sim, ed, &model, &wrong), check, print);
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

/// A key the scenario sends — a batch of its own, as the main loop takes it, a frame after when one is due — its
/// model following.
fn send(sim: &Sim, ed: &mut Editor, model: &RefCell<Model>, key: Key) {
    model.borrow_mut().quiet_sync();
    sim.batch(ed, crate::event::Event::Key(key));
    model.borrow_mut().last_key = Some(crate::clock::now());
}

fn input(sim: &mut Sim, ed: &mut Editor, model: &RefCell<Model>, wrong: &Cell<Option<String>>) -> String {
    let c = &mut sim.choices;
    // (by arm: a key 6, a card 3, a session switch 1, a frame 1, time passing 2)
    match c.weighted(&[6, 3, 1, 1, 2]) {
        0 => {
            let k = *c.pick(&["j", "k", "y", "y", "y", "C-q"]);
            model.borrow_mut().quiet_sync();
            let front = ed.offers.first().map(|o| o.id);
            // (given the keys by `C-q`, its answers are its own)
            let given = ed.card_keys;
            send(sim, ed, model, k.parse::<Key>().unwrap());
            if let Some(id) = front.filter(|_| k == "y") {
                let answered = ed.offers.first().map(|o| o.id) != front;
                let should_wait = !given && model.borrow().should_wait(id);
                if should_wait && answered {
                    wrong.set(Some(format!("y answered card#{id}, which should have waited for the keys")));
                } else if !should_wait && !answered {
                    wrong.set(Some(format!("y left card#{id} up, which should have had the keys")));
                }
            }
            if ed.card_keys && k != "C-q" {
                // (given the keys, then answered or let go: back to the code)
                send(sim, ed, model, Key::plain(Code::Esc));
            }
            format!("key {k}")
        }
        1 => {
            // (asked by the key just before — the user's own — or come from elsewhere)
            let yours = *c.pick(&[false, false, true]);
            let what = What::ReviewEnd { session: 0, started: crate::clock::now() };
            let was = ed.events.set_asked(yours.then_some(ed.input_seq));
            // (a title of its own: one with the same title would be merged into the card up)
            let title = format!("End review {}?", ed.offer_seq + 1);
            ed.push_offer(Offer::new(title, "2 files not viewed.", "", String::new(), what));
            ed.events.set_asked(was);
            let mut m = model.borrow_mut();
            let coming = m.last_key.is_some_and(|t| crate::clock::since(t) < KEYS_QUIET);
            m.waits.insert(ed.offer_seq, !yours && coming);
            format!(
                "card#{} comes{} · should wait {}",
                ed.offer_seq,
                if yours { ", asked by the last key" } else { "" },
                !yours && coming
            )
        }
        2 => {
            let (main, other) = model.borrow().sessions;
            let to = if ed.session.id == main { other } else { main };
            ed.session_switch(to);
            // (as the key that switches would: a frame after it)
            crate::term::render(ed, &mut Vec::new(), 100, 24).unwrap();
            format!("to session {to}")
        }
        3 => {
            crate::term::render(ed, &mut Vec::new(), 100, 24).unwrap();
            "a frame".into()
        }
        _ => {
            let ms = *c.pick(&[50, 300, 1200]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
    }
}

/// No `y` judged wrong by the model; a card that waits, up or parked, has its wake under way; with no wake under
/// way, the screen doesn't say a card waits when none does (a wake whose frame was never drawn — the review of
/// friction 60: a result taken for a session behind).
fn held(ed: &Editor, wrong: &Cell<Option<String>>) -> Result<(), String> {
    if let Some(why) = wrong.take() {
        return Err(why);
    }
    let status = ed
        .last_frame
        .as_ref()
        .and_then(|f| crate::screen::text(f.0.as_slice(), 100, 24))
        .and_then(|t| t.lines().last().map(str::to_string))
        .unwrap_or_default();
    if !ed.keys_waking && !ed.card_waits_quiet() && status.contains("waiting · C-q") {
        return Err(format!("the screen says a card waits, none does: {}", status.trim()));
    }
    // (read here, not through `cards_typing`: the check mustn't share what it checks)
    let parked = std::iter::once(&ed.session).chain(&ed.sessions).flat_map(|l| &l.parked);
    if ed.offers.iter().chain(parked).any(|o| o.typing) && !ed.keys_waking {
        return Err("a card waits for the keys with no wake under way".into());
    }
    Ok(())
}

fn print(ed: &Editor) -> String {
    let waiting = ed.offers.iter().filter(|o| o.typing).count();
    let parked = ed.session.parked.len() + ed.sessions.iter().map(|l| l.parked.len()).sum::<usize>();
    format!(
        "cards {} · parked {parked} · waiting {waiting} · card has the keys {}",
        ed.offers.len(),
        ed.card_has_keys()
    )
}
