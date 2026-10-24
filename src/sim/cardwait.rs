//! A card takes the keys only once focused (P3 §3.8), simulated: keys (`j`, `k`, `y`, `esc`, `C-q`), light cards
//! coming between them — some asked by the key just before (the user's own, focused as they come), some rewritten
//! as they're up (asked again: a new `rev`) — a press on the card, a second session switched to and back (a card
//! parked, then up again), frames, and the user taking their time.
//!
//! The scenario keeps its own model of which card is focused — by id and `rev`, from what it did alone: `C-q` or a
//! press focuses the front (`C-q` again, `esc`, a switch, the card's text changing let go), your own card coming to
//! the front is focused, and an answer hands the keys on only while going through them (the answered card not
//! yours) or to one of yours — and only at a frame that draws it: `y y` in one batch (a repeat, a yank typed on) never
//! answers the next card unseen (the review of 7b). Each `y` is judged by it — one that reaches a card the model
//! hasn't focused must leave it up (the code's yank, or refused and said on it), one that reaches the focused card
//! must answer it (light cards: `y` ends what never began, the card goes). After every step the editor's focus is
//! the model's.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::Editor;
use crate::key::Key;
use crate::offer::{Offer, What};

const SEEDS: &[u64] = &[0, 1, 2, 7, 10];

const INPUTS: usize = 60;

const LIMIT: Duration = Duration::from_secs(60);

#[test]
fn a_card_takes_the_keys_only_once_focused() {
    super::seeds("a_card_takes_the_keys_only_once_focused", SEEDS, run);
}

/// What the scenario knows from its own steps: the card it focused (id, rev), the one an answer handed the keys on to
/// (focused at the next frame that draws it), the frames seen, the two sessions.
#[derive(Default)]
struct Model {
    focused: Option<(u64, u64)>,
    next: Option<(u64, u64)>,
    frames: u64,
    sessions: (crate::event::SessionId, crate::event::SessionId),
}

impl Model {
    /// A frame drawn since: the card handed on, in front as it was, is focused.
    fn sync(&mut self, ed: &Editor) {
        if ed.frames != self.frames {
            self.frames = ed.frames;
            if self.next.is_some() && front(ed).map(|f| (f.0, f.1)) == self.next {
                self.focused = self.next.take();
            }
        }
    }
}

fn run(choices: Choices) -> Vec<String> {
    let dir = crate::scratch::Scratch::new("sim-cardwait");
    let mut sim = Sim::new("a_card_takes_the_keys_only_once_focused", choices, LIMIT);
    sim.frames = Some((100, 24));
    let mut ed = Editor::new(crate::config::Config::default());
    let main = ed.session.id;
    let other = ed.session_open(dir.path()).unwrap();
    ed.session_switch(main);
    let model = RefCell::new(Model { sessions: (main, other), ..Model::default() });
    let wrong = Cell::new(None);
    let check = |ed: &Editor| held(ed, &model.borrow(), &wrong);
    sim.walk(&mut ed, INPUTS, |sim, ed| input(sim, ed, &model, &wrong), check, print);
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

/// The front card up: (id, rev, yours).
fn front(ed: &Editor) -> Option<(u64, u64, bool)> {
    ed.offers.first().map(|o| (o.id, o.rev, o.yours))
}

/// Key `k` — a batch of its own, a frame after it when one is due (`framed`), else the first of a batch the next key
/// ends — judged and followed by the model.
fn key(
    sim: &Sim,
    ed: &mut Editor,
    model: &RefCell<Model>,
    wrong: &Cell<Option<String>>,
    k: &str,
    framed: bool,
) {
    let before = front(ed);
    let session = ed.session.id;
    let event = crate::event::Event::Key(k.parse::<Key>().unwrap());
    match framed {
        true => sim.batch(ed, event),
        false => ed.handle_event(event),
    }
    let mut m = model.borrow_mut();
    let had = before.is_some_and(|f| m.focused == Some((f.0, f.1)));
    match k {
        "y" => {
            let Some(f) = before else { return };
            let answered = front(ed).map(|o| o.0) != Some(f.0);
            match (had, answered) {
                (false, true) => wrong.set(Some(format!("y answered card#{}, never focused or drawn", f.0))),
                (true, false) => wrong.set(Some(format!("y left card#{}, focused, up", f.0))),
                _ => {}
            }
            // (answered: on to the next only while going through them, or to one of yours — at the frame that draws
            // it)
            if answered {
                m.focused = None;
                m.next = front(ed).filter(|n| !f.2 || n.2).map(|n| (n.0, n.1));
            }
        }
        "esc" if had => m.focused = None,
        // (focused: let go; else to this session's card, or — none here — the next up anywhere)
        "C-q" if had => m.focused = None,
        "C-q" => {
            let here = before.is_some();
            m.focused = match front(ed) {
                Some(f) if here || ed.session.id != session => Some((f.0, f.1)),
                _ => None,
            };
        }
        _ => {}
    }
    m.sync(ed);
}

fn input(sim: &mut Sim, ed: &mut Editor, model: &RefCell<Model>, wrong: &Cell<Option<String>>) -> String {
    let said = step(sim, ed, model, wrong);
    model.borrow_mut().sync(ed);
    said
}

fn step(sim: &mut Sim, ed: &mut Editor, model: &RefCell<Model>, wrong: &Cell<Option<String>>) -> String {
    let c = &mut sim.choices;
    // (by arm: a key 6, a card 3, a card rewritten 1, a press 1, a session switch 1, a frame 1, time passing 1,
    // `y y` in one batch 2)
    match c.weighted(&[6, 3, 1, 1, 1, 1, 1, 2]) {
        0 => {
            let k = *c.pick(&["j", "k", "y", "y", "y", "esc", "C-q", "C-q"]);
            key(sim, ed, model, wrong, k, true);
            format!("key {k}")
        }
        7 => {
            // (a repeat, or a yank typed on after an answer: both keys before any frame)
            key(sim, ed, model, wrong, "y", false);
            key(sim, ed, model, wrong, "y", true);
            "keys y y in one batch".into()
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
            let id = ed.offer_seq;
            // (yours, come to the front: focused as it came)
            if let Some(f) = front(ed).filter(|f| yours && f.0 == id) {
                model.borrow_mut().focused = Some((f.0, f.1));
            }
            format!("card#{id} comes{}", if yours { ", asked by the last key" } else { "" })
        }
        2 => {
            // (the front asked again from elsewhere: its text new, its `rev` up)
            let Some(title) = ed.offers.first().map(|o| o.title.clone()) else {
                return "nothing to rewrite".into();
            };
            let what = What::ReviewEnd { session: 0, started: crate::clock::now() };
            ed.push_offer(Offer::new(title, "3 files not viewed.", "", String::new(), what));
            format!("card#{} rewritten", ed.offers[0].id)
        }
        3 => {
            // (a press on the card in sight: focused)
            let Some((x, y, _, _)) = ed.screen.offer_card else { return "no card to press".into() };
            let (x, y) = (x + 2, y + 1);
            // (a back card's row brings it to the front, focused; an answer is a key's to judge, not pressed here)
            let button =
                ed.screen.offer_buttons.iter().find(|b| b.0 == y && (b.1..b.2).contains(&x)).map(|b| b.3);
            if button.is_some_and(|k| !k.is_ascii_digit()) {
                return "no press: an answer there".into();
            }
            let press = crate::event::Mouse {
                kind: crate::event::MouseKind::Down,
                x: x as u16,
                y: y as u16,
                alt: false,
                ctrl: false,
            };
            let before = front(ed);
            sim.batch(ed, crate::event::Event::Mouse(press));
            let f = if button.is_some() { front(ed) } else { before };
            if let Some(f) = f {
                model.borrow_mut().focused = Some((f.0, f.1));
            }
            format!("a press on the card{}", if button.is_some() { "'s back row" } else { "" })
        }
        4 => {
            let (main, other) = model.borrow().sessions;
            let to = if ed.session.id == main { other } else { main };
            ed.session_switch(to);
            (model.borrow_mut().focused, model.borrow_mut().next) = (None, None);
            // (as the key that switches would: a frame after it)
            crate::term::render(ed, &mut Vec::new(), 100, 24).unwrap();
            format!("to session {to}")
        }
        5 => {
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

/// No `y` judged wrong by the model; the editor's focus is the model's — the front card has the keys exactly when the
/// model focused it as it says now — and the keys' state says the one waiting otherwise.
fn held(ed: &Editor, m: &Model, wrong: &Cell<Option<String>>) -> Result<(), String> {
    if let Some(why) = wrong.take() {
        return Err(why);
    }
    let focused = front(ed).is_some_and(|f| m.focused == Some((f.0, f.1)));
    if ed.card_has_keys() != focused {
        return Err(format!("the card has the keys: {}, the model says {focused}", ed.card_has_keys()));
    }
    let waiting = ed.keys_to().waiting;
    if waiting.is_some() != (front(ed).is_some() && !focused) {
        return Err(format!("keys.waiting {waiting:?} — the model's focus {:?}", m.focused));
    }
    Ok(())
}

fn print(ed: &Editor) -> String {
    let parked = ed.session.parked.len() + ed.sessions.iter().map(|l| l.parked.len()).sum::<usize>();
    format!("cards {} · parked {parked} · card has the keys {}", ed.offers.len(), ed.card_has_keys())
}
