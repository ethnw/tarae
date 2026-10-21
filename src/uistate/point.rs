//! Pointing (`docs/design/ui-state.md`, phase 3 (c)): the user shows Claude an element — "why did this come
//! up?" about the card on screen. `C-o` in the chat (`space @` in the code, `:chat-point`) puts a two-letter
//! label, in the `gw` look, on every element on screen that has an id: the cards' title rows, the toasts,
//! the side panel's and the review's rows, the bottom slot's tabs, the path bar. Typing a label (or clicking
//! the element) puts its id in the chat's input as a chip; the message carries `<pointed id=…>` with what
//! the element is (`claude::pointed`), and it's given to Claude for the turn.
//!
//! While labels are up every key is pointing's (`handle_key`'s first layer): letters pick, Esc stops,
//! anything else is said — nothing goes through to what's under the labels.

use crate::editor::Editor;
use crate::key::{Code, Key};
use crate::labels::{ALPHABET, Labels};

/// An element as drawn (besides the panels' rows, which `Screen.sidebar`, `review_rail` and `slot_tabs`
/// have): a card's title row, a toast's first row, the path bar — the cells a click on it takes (`x`, `y`,
/// `w`), and where its label goes (`at`: over a margin, never over its words).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Spot {
    pub of: Of,
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub at: (usize, usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Of {
    Card(u64),
    Toast(u64),
    Pane(crate::split::ViewId),
}

/// An element with a label: its id, the cells a click on it takes, and where its label goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub id: String,
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub at: (usize, usize),
}

/// Labels up: the elements in label order (`Labels::label(i)`), and the first letter typed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pointing {
    pub targets: Vec<Target>,
    pub typed: Option<char>,
}

impl Editor {
    /// Every element on screen with an id, top to bottom then left to right — as the last frame drew them.
    pub fn point_targets(&self) -> Vec<Target> {
        let s = &self.screen;
        let mut out: Vec<Target> = Vec::new();
        for spot in s.spots.borrow().iter() {
            let id = match spot.of {
                Of::Card(n) => format!("card#{n}"),
                Of::Toast(n) => format!("toast#{n}"),
                Of::Pane(v) => format!("pane#{v}"),
            };
            out.push(Target { id, x: spot.x, y: spot.y, w: spot.w, at: spot.at });
        }
        // (a picker covers the side panel and the rail — their rows aren't in sight)
        let covered = s.picker_list.is_some();
        if let Some(sb) = self.session.sidebar.as_ref().filter(|_| s.side_w > 0 && !covered) {
            let tab = super::side_tab(sb.mode);
            let lines = sb.lines();
            for &(y, i) in s.sidebar.borrow().iter() {
                if let Some(l) = lines.get(i).filter(|l| l.target != crate::sidebar::Target::None) {
                    let id = super::panels::side_id(self, tab, l);
                    out.push(Target { id, x: 0, y, w: s.side_w, at: (0, y) });
                }
            }
        }
        if let Some(r) = self.session.code_review.as_ref().filter(|_| s.rail_w > 0 && !covered) {
            for &(y, i) in s.review_rail.borrow().iter() {
                if let Some(f) = r.files.get(i) {
                    out.push(Target { id: format!("review:{}", f.rel), x: 0, y, w: s.rail_w, at: (0, y) });
                }
            }
        }
        for &(y, a, b, slot) in s.slot_tabs.borrow().iter() {
            let id = match slot {
                crate::sizes::Slot::Task => {
                    self.session.task_run.as_ref().map(|r| format!("run#{}", r.trace.id()))
                }
                crate::sizes::Slot::Test => {
                    self.session.test_run.as_ref().map(|r| format!("run#{}", r.trace.id()))
                }
                other => Some(super::slot_name(other).to_string()),
            };
            // (its label on the rule under the tab, where its word starts — none hidden)
            if let Some(id) = id {
                out.push(Target { id, x: a, y, w: b - a, at: (a, y + 1) });
            }
        }
        crate::sorting::by_key(&mut out, |t| (t.at.1, t.at.0));
        out.dedup_by(|a, b| a.id == b.id);
        out.truncate(ALPHABET.len() * ALPHABET.len());
        out
    }

    /// The element whose cells hold (`x`, `y`) — a click while pointing.
    fn point_at(&self, x: usize, y: usize) -> Option<String> {
        let p = self.pointing.as_ref()?;
        p.targets.iter().find(|t| t.y == y && (t.x..t.x + t.w.max(2)).contains(&x)).map(|t| t.id.clone())
    }
}

/// `C-o` in the chat, `space @`, `:chat-point`: labels on what's on screen.
pub fn start(ed: &mut Editor) {
    let targets = ed.point_targets();
    if targets.is_empty() {
        return ed.set_status(
            "nothing on screen to point at — cards, toasts, panel rows and the path bar have labels",
        );
    }
    ed.pointing = Some(Pointing { targets, typed: None });
}

/// A key while labels are up — all of them are pointing's. True if pointing took it.
pub fn key(ed: &mut Editor, key: Key) -> bool {
    let Some(p) = ed.pointing.as_mut() else { return false };
    match (key.code, key.ctrl || key.alt) {
        (Code::Esc, _) => {
            ed.pointing = None;
        }
        (Code::Char(c), false) if c.is_ascii_lowercase() => match p.typed {
            None if (0..p.targets.len()).any(|i| Labels::label(i).0 == c) => p.typed = Some(c),
            Some(a) => match (0..p.targets.len()).find(|&i| Labels::label(i) == (a, c)) {
                Some(i) => {
                    let id = p.targets[i].id.clone();
                    pick(ed, id);
                }
                None => {
                    p.typed = None;
                    ed.set_status(format!("no label {a}{c} — type one shown, or esc"));
                }
            },
            None => ed.set_status(format!("no label starts with {c} — type one shown, or esc")),
        },
        _ => ed.set_status(format!("{key}: pointing takes a label's two letters — esc stop pointing")),
    }
    true
}

/// A click while labels are up: the element under it is picked; anywhere else, pointing stops. True if
/// pointing took it.
pub fn click(ed: &mut Editor, x: usize, y: usize) -> bool {
    if ed.pointing.is_none() {
        return false;
    }
    match ed.point_at(x, y) {
        Some(id) => pick(ed, id),
        None => ed.pointing = None,
    }
    true
}

/// `id` pointed at: a chip in the chat's input (the chat opened, with the keys) — the next message carries it;
/// one carrying a file named like a key Claude may not read asks first (`reads::point`).
fn pick(ed: &mut Editor, id: String) {
    ed.pointing = None;
    if ed.session.chat.is_none() {
        crate::chat::open(ed);
    }
    let Some(c) = ed.session.chat.as_mut() else { return };
    c.focused = true;
    c.unfolded = true;
    if !c.pointed.contains(&id) {
        c.pointed.push(id.clone());
        c.with.push(crate::chat::With::Pointed);
    }
    crate::reads::point(ed, &id);
}
