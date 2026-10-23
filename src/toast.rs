//! The notification card as a pane (panes P3 step 7c, plan §3.9 — the user's decision 14, 2026-10-22): a toast is
//! a note in the floating layer, by its id (`toast#N`), placed as it always was (`term::toast_place`), and — on 7b's
//! rule — it takes keys only once focused: a press on it, or `C-w w`'s round (after a waiting card: panes → the card
//! → the toast → round again). Never `C-q` (the inbox is what waits for you; news doesn't), never a hop.
//!
//! - **Framed** as every float that can take the keys (user, 2026-10-23): its air rows are its frame's lines, the faint
//!   wash, named `messages`, on the ground. **Focused** the lines lit, the same rect (nothing moves); its messages held
//!   (their time stops: they stay while it has the keys, and so do ones that join it meanwhile); its keys in the
//!   frame's bottom line, as many whole pairs as its width holds, the most useful first: `enter` its home —
//!   `:messages` with its message selected —, `esc` (`q`) lets it go, `y` copies it, `j`/`k` between its messages;
//!   every other key said. The window prefix gives the keys back and goes on (as from a card).
//! - **`:messages` opened lets the toasts go** (user, 2026-10-23: the same words never twice), however it's opened;
//!   what's said while it's up is a row of it where the frame shows that row (`messages::rows_drawn`), else a toast
//!   as usual — an error never only on the status line — and a toast again once it closes.
//! - **A press on it focuses it**, fading too (you pressed it: back, held); with a prompt open the press is the
//!   prompt's, said.
//! - **Losing the focus fades it out** — whatever took it (`q`, `esc`, `C-w`, a press elsewhere, a card or a picker
//!   taking the keys, a session switch): over `LETGO`, its own short fade (honest motion: the frame after the let-go
//!   at half strength, gone at the next tick, nothing drawn after). A message that comes meanwhile starts a card of
//!   its own.
//! - Unfocused, as it was: its reading time, an error waiting for a person's key after `ERROR_MIN`, `Toast::waiting`
//!   under a picker, the narrow fold to the status line.

use crate::editor::Editor;
use crate::key::{Code, Key};
use std::time::Duration;

/// How long a toast that loses the focus takes to fade out — its own, shorter than the timeout's (`TOAST_FADE`).
pub const LETGO: Duration = Duration::from_millis(100);

/// A focused card's messages are held this long — for as long as it has the keys (its let-go ends it).
pub(crate) const HELD: Duration = Duration::from_secs(24 * 3600);

/// The focused card: its newest message when it got the keys (`toast#N`, its pane's id in the state) and the
/// message in hand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Focus {
    pub id: u64,
    pub selected: u64,
}

impl Editor {
    /// The notification card is up as a card (not folded into the status line), not fading out after a let-go — it
    /// can take the keys.
    pub fn toast_up(&self) -> bool {
        self.screen.toast_card.get().is_some() && !self.toasts.is_empty() && self.toast_letgo.is_none()
    }

    /// The messages on the card, top to bottom: the newest, then the ones before it the card shows a row each.
    pub fn toast_shown(&self) -> Vec<u64> {
        let toasts = self.card_toasts();
        let newest = toasts.last().map(|t| t.id);
        let older = toasts.iter().rev().skip(1).take(crate::term::TOAST_OLDER).map(|t| t.id);
        newest.into_iter().chain(older).collect()
    }

    /// The card takes the keys (a press on it, `C-w w`): its messages held, the newest in hand — one fading after a
    /// let-go too (pressed: it comes back held, the fade stopped). Only one floating pane has them: a card that had
    /// them lets go.
    pub fn toast_focus_take(&mut self) {
        let drawn = self.screen.toast_card.get().is_some();
        let Some(newest) = self.toasts.last().map(|t| t.id).filter(|_| drawn) else { return };
        self.toast_letgo = None;
        self.unchanged = false;
        self.card_focus = None;
        for t in &mut self.toasts {
            t.life = t.age() + HELD;
        }
        self.toast_focus = Some(Focus { id: newest, selected: newest });
        self.focus_sync();
    }

    /// The focused card lets go of the keys: it fades out over `LETGO`, then goes.
    pub fn toast_let_go(&mut self) {
        if self.toast_focus.take().is_none() {
            return;
        }
        for t in &mut self.toasts {
            t.life = t.age() + LETGO;
        }
        self.toast_letgo = Some(crate::clock::now());
        self.unchanged = false;
        self.schedule_tick();
    }

    /// Every key and event: a focused card holds the keys only while it's up and nothing else took them — a card
    /// focused, a picker or a prompt opened (it lets go and fades); gone (folded into the status line, its messages
    /// put away), the focus with it. The message in hand is one on the card.
    pub fn toast_sync(&mut self) {
        let Some(f) = self.toast_focus else { return };
        if self.toasts.is_empty() {
            self.toast_focus = None;
            return;
        }
        if self.card_has_keys() || self.picker.is_some() || self.prompt.is_some() {
            return self.toast_let_go();
        }
        let shown = self.toast_shown();
        if !shown.contains(&f.selected) {
            let selected = shown.first().copied().unwrap_or(f.id);
            self.toast_focus = Some(Focus { selected, ..f });
        }
    }

    /// A press on the card: focused, the message under it in hand; else the card takes the keys.
    pub fn toast_press(&mut self, y: usize) {
        if self.toast_focus.is_none() {
            return self.toast_focus_take();
        }
        let rows = self.screen.toast_rows.borrow().clone();
        let under = rows.iter().rev().find(|r| r.0 <= y).or(rows.first()).map(|r| r.1);
        if let (Some(f), Some(id)) = (self.toast_focus.as_mut(), under) {
            f.selected = id;
        }
    }

    /// The card gone at once, focused or not, fading or not (`:messages` opened: its words are there).
    pub fn toasts_put_away(&mut self) {
        if !self.toasts.is_empty() {
            self.unchanged = false;
        }
        self.toasts.clear();
        (self.toast_focus, self.toast_letgo) = (None, None);
        self.screen.toast_narrow.set(None);
    }

    /// The text of the message in hand on the focused card.
    fn toast_selected(&self) -> Option<&crate::editor::Toast> {
        let f = self.toast_focus?;
        self.toasts.iter().find(|t| t.id == f.selected)
    }
}

/// A key while the card has the keys — false: it hasn't them (or the window prefix: they go back, the prefix goes on).
pub fn key(ed: &mut Editor, key: Key) -> bool {
    let Some(f) = ed.toast_focus else { return false };
    // The window prefix: the keys back where they were, the prefix going on from there — a hop out is one key
    if ed.pending.is_empty() && ed.keymaps.window_prefix() == Some(key) {
        ed.toast_let_go();
        ed.float_left = Some(crate::pane::Floating::Toast(f.id));
        return false;
    }
    ed.pending.clear();
    let shown = ed.toast_shown();
    let at = shown.iter().position(|&id| id == f.selected).unwrap_or(0);
    let step = |ed: &mut Editor, to: Option<usize>| match to.and_then(|i| shown.get(i)) {
        Some(&selected) => ed.toast_focus = Some(crate::toast::Focus { selected, ..f }),
        None => ed.key_unused(key, "the message", "it's at its end"),
    };
    match (key.code, key.ctrl || key.alt) {
        (Code::Esc | Code::Char('q'), false) => ed.toast_let_go(),
        (Code::Enter, false) => {
            ed.toast_let_go();
            ed.messages_open(Some(f.selected));
        }
        (Code::Char('y'), false) => {
            if let Some(text) = ed.toast_selected().map(|t| t.text.clone()) {
                crate::clipboard::copy(&ed.events.jobs(), text, "the message".into());
            }
        }
        (Code::Char('j') | Code::Down, false) => step(ed, Some(at + 1)),
        (Code::Char('k') | Code::Up, false) => step(ed, at.checked_sub(1)),
        _ => ed.key_unused(key, "the message", "enter messages, y copies, esc close"),
    }
    true
}

/// The way out, said on every focused card — the card is never narrower than it (`term::draw_toasts`).
pub const ESC: (&str, &str) = ("esc", "close");

/// The focused card's keys for its frame's bottom line, the most useful first — the line takes them in order, whole,
/// one that doesn't fit skipped for the next that does (`term::draw_toasts`): `enter messages`, `esc close`, `y copy`,
/// `j k move` (with more than one message on it).
pub fn hints(ed: &Editor) -> Vec<(&'static str, &'static str)> {
    let mut v = vec![("enter", "messages"), ESC, ("y", "copy")];
    if ed.toast_shown().len() > 1 {
        v.push(("j k", "move"));
    }
    v
}

#[cfg(test)]
mod tests;
