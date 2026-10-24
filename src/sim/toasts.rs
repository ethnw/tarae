//! A toast's time meets the focus (P3 step 7c, `toast.rs`), simulated: messages said (routine ones and errors), `C-w w`
//! and presses focusing the card, its keys (`y`, `j`, `k`, `x`, `q`, `esc`, the window prefix), a press elsewhere, a
//! session switch, frames, ticks and the user taking their time — a toast's reading time running out as it's focused,
//! a message coming while it's focused or while it fades after a let-go.
//!
//! The scenario keeps its own model of the focus from what it did alone: `C-w w` from the code focuses the card while
//! it's drawn as a card and not fading, a press on it while it's drawn (fading too: it comes back held); `q`, `esc`, the
//! window prefix, a press elsewhere and a switch let it go. After every step: the editor's focus is the model's; a
//! focused card's messages are all up (held — their time stops) and it asks for no wake (`toast_wake`); a card let go
//! asks for one wake, at its fade's end, and once it's gone nothing is asked for.

use std::cell::RefCell;
use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::Editor;
use crate::key::Key;

const SEEDS: &[u64] = &[0, 1, 2, 7, 10];

const INPUTS: usize = 60;

const LIMIT: Duration = Duration::from_secs(60);

#[test]
fn a_toasts_time_meets_the_focus() {
    super::seeds("a_toasts_time_meets_the_focus", SEEDS, run);
}

/// What the scenario knows from its own steps: whether it focused the card, and the two sessions.
#[derive(Default)]
struct Model {
    focused: bool,
    sessions: (crate::event::SessionId, crate::event::SessionId),
    /// A tick served past a let-go's fade left the card standing, or a wake asked (the next check says it).
    stuck: Option<String>,
}

fn run(choices: Choices) -> Vec<String> {
    let dir = crate::scratch::Scratch::new("sim-toasts");
    let mut sim = Sim::new("a_toasts_time_meets_the_focus", choices, LIMIT);
    sim.frames = Some((100, 24));
    let mut ed = Editor::new(crate::config::Config::default());
    ed.docs[0].text = ropey::Rope::from_str("one\ntwo\n");
    let main = ed.session.id;
    let other = ed.session_open(dir.path()).unwrap();
    ed.session_switch(main);
    let model = RefCell::new(Model { sessions: (main, other), ..Model::default() });
    let check = |ed: &Editor| held(ed, &model.borrow());
    sim.walk(&mut ed, INPUTS, |sim, ed| step(sim, ed, &model), check, print);
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

fn key(sim: &Sim, ed: &mut Editor, k: &str) {
    sim.batch(ed, crate::event::Event::Key(k.parse::<Key>().unwrap()));
}

/// The card is drawn as a card: what a press reaches — and `C-w w` while it isn't fading.
fn drawn(ed: &Editor) -> bool {
    ed.screen.toast_card.get().is_some() && !ed.toasts.is_empty()
}

fn up(ed: &Editor) -> bool {
    drawn(ed) && ed.toast_letgo.is_none()
}

fn step(sim: &mut Sim, ed: &mut Editor, model: &RefCell<Model>) -> String {
    let c = &mut sim.choices;
    // (by arm: a message 3, an error 1, C-w w 2, a press on the card 1, a key 4, a press elsewhere 1, a switch 1, a
    // frame 1, a tick 2, time passing 3)
    match c.weighted(&[3, 1, 2, 1, 4, 1, 1, 1, 2, 3]) {
        0 => {
            let n = ed.said + 1;
            ed.set_status(format!("saved file {n}"));
            format!("message {n}")
        }
        1 => {
            ed.set_error("it broke");
            "an error".into()
        }
        2 => {
            // (from the code — under which-key —, nothing else floating: the round's last stop is the toast, as the
            // frame after `C-w` draws it)
            let was = model.borrow().focused;
            key(sim, ed, "C-w");
            let reach = up(ed) && ed.session.docked() == crate::session::At::Code && ed.offers.is_empty();
            key(sim, ed, "w");
            // (focused: the prefix let it go, the round goes on round the panes)
            model.borrow_mut().focused = !was && reach;
            "C-w w".into()
        }
        3 => {
            let Some((x, y, ..)) = ed.screen.toast_card.get().filter(|_| !ed.toasts.is_empty()) else {
                return "no card to press".into();
            };
            let reach = drawn(ed) || model.borrow().focused;
            let press = crate::event::Mouse {
                kind: crate::event::MouseKind::Down,
                x: x as u16 + 2,
                y: y as u16 + 1,
                alt: false,
                ctrl: false,
            };
            sim.batch(ed, crate::event::Event::Mouse(press));
            if reach {
                model.borrow_mut().focused = true;
            }
            "a press on the card".into()
        }
        4 => {
            let k = *c.pick(&["y", "j", "k", "x", "q", "esc"]);
            key(sim, ed, k);
            if matches!(k, "q" | "esc") {
                model.borrow_mut().focused = false;
            }
            format!("key {k}")
        }
        5 => {
            let press = crate::event::Mouse {
                kind: crate::event::MouseKind::Down,
                x: 1,
                y: 1,
                alt: false,
                ctrl: false,
            };
            sim.batch(ed, crate::event::Event::Mouse(press));
            // (unless that's on the card itself)
            let on = ed
                .screen
                .toast_card
                .get()
                .is_some_and(|(x, y, w, h)| (x..x + w).contains(&1) && (y..y + h).contains(&1));
            if !on {
                model.borrow_mut().focused = false;
            }
            "a press on the code".into()
        }
        6 => {
            let (main, other) = model.borrow().sessions;
            let to = if ed.session.id == main { other } else { main };
            ed.session_switch(to);
            model.borrow_mut().focused = false;
            crate::term::render(ed, &mut Vec::new(), 100, 24).unwrap();
            format!("to session {to}")
        }
        7 => {
            crate::term::render(ed, &mut Vec::new(), 100, 24).unwrap();
            "a frame".into()
        }
        8 => {
            // (as the main loop's wake: its tick, then a frame — one past a let-go's fade ends it: the card gone, nothing
            // asked for after)
            let over = ed.toast_letgo.is_some_and(|at| crate::clock::since(at) >= crate::toast::LETGO);
            ed.schedule_tick();
            ed.focus_sync();
            crate::term::render(ed, &mut Vec::new(), 100, 24).unwrap();
            if over && (!ed.toasts.is_empty() || ed.toast_wake().is_some()) {
                let (n, wake) = (ed.toasts.len(), ed.toast_wake());
                model.borrow_mut().stuck =
                    Some(format!("past its fade, a tick left {n} toasts, a wake in {wake:?}"));
            }
            "a tick".into()
        }
        _ => {
            let ms = *c.pick(&[50, 120, 400, 5000, 70_000]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
    }
}

/// The editor's focus is the model's; focused, every message on the card is up and no wake is asked for; let go, one
/// wake at its fade's end (its frame there, none before it); that wake served, the card gone and none asked.
fn held(ed: &Editor, m: &Model) -> Result<(), String> {
    if let Some(stuck) = &m.stuck {
        return Err(stuck.clone());
    }
    let focused = ed.toast_focus.is_some();
    // (the card can go from under the model: folded away with nothing on it — its messages put away)
    if focused != (m.focused && !ed.toasts.is_empty()) {
        return Err(format!("the toast has the keys: {focused}, the model says {}", m.focused));
    }
    if focused {
        if let Some(t) = ed.toasts.iter().find(|t| !t.alive()) {
            return Err(format!("toast#{} ran out while the card had the keys", t.id));
        }
        if ed.toast_letgo.is_some() {
            return Err("focused and fading at once".into());
        }
        if let Some(wake) = ed.toast_wake() {
            return Err(format!("focused, it's held — yet a wake is asked in {wake:?}"));
        }
    }
    if let Some(at) = ed.toast_letgo {
        let wake = ed.toast_wake();
        if ed.toasts.is_empty() {
            if let Some(wake) = wake {
                return Err(format!("its fade is over and the card gone — yet a wake is asked in {wake:?}"));
            }
        } else {
            // (at the fade's end, give or take the wake's 2 ms past it: the let-go frame, then gone)
            let left = crate::toast::LETGO.saturating_sub(crate::clock::since(at));
            let slack = Duration::from_millis(5);
            match wake {
                None => return Err("let go, it fades — but no wake is asked to end it".into()),
                Some(w) if w < left || w > left + slack => {
                    return Err(format!("let go with {left:?} of its fade left, a wake is asked in {w:?}"));
                }
                Some(_) => {}
            }
        }
    }
    Ok(())
}

fn print(ed: &Editor) -> String {
    let letgo = ed.toast_letgo.map(|at| crate::clock::since(at).as_millis());
    format!("toasts {} · focused {} · let go {letgo:?} ms ago", ed.toasts.len(), ed.toast_focus.is_some())
}
