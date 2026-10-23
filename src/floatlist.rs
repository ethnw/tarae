//! The floating lists (panes P3 steps 7a and 7c): the session list (`sessionpane.rs`) and `:messages`
//! (`messages.rs`) — the viewer's, not a session's (no member of `Session.groups`), each in a named frame over the
//! code and the chat (`Frame::LIST`: 3/5 wide, as tall as its rows up to half the screen, its top at 1/4). One list's
//! state, keys, search and mouse for both, so the two never drift apart; each kind gives its rows' ids, what its
//! search matches, its `enter` and its own letters.
//!
//! - Its keys are a list's (`key::list_move`), `/`·`?` and `n`·`N` its own search (its matches lit — never
//!   type-to-narrow: its letters are keys), `esc code` gives the keys back with the list staying in sight, `q`
//!   closes it; a press on a row selects it, the same row again opens it as `enter`.
//! - **Done once used** (user, 2026-10-23): a floating list closes as what it was opened for is done — the session
//!   list's `enter` switching (zellij's session manager, tmux's choose-tree), the messages' `enter` opening one —
//!   floating panes sit in the middle of the screen, so a list that stayed would cover the code (`used`). A docked
//!   one would stay; neither docks yet.

use crate::editor::Editor;
use crate::key::{Code, Key, ListMove};
use crate::pane::GroupKind;
use crate::session::At;
use crate::split::PaneId;

/// A floating list while it's up.
#[derive(Clone, Debug)]
pub struct FloatList {
    /// Its pane's id (`At::List`, the state's `pane#N`).
    pub list: PaneId,
    /// The row selected, by its id (a session's, a message's) — kept as rows come and go (one gone: the row there
    /// now).
    pub selected: u64,
    /// Its first row in sight, as last drawn.
    pub top: std::cell::Cell<usize>,
    /// Its own search, as every pane's (never the code's): the last pattern, its matches lit while `hl`.
    pub search: Option<String>,
    pub hl: bool,
    /// A `/`·`?` it opened is its until it closes: the row selected as it opened (`esc` goes back to it).
    pub prompt: Option<u64>,
}

/// The kinds that are floating lists.
pub const KINDS: [GroupKind; 2] = [GroupKind::Sessions, GroupKind::Messages];

/// Kind `kind` is a floating list of the viewer's.
pub fn is_list(kind: GroupKind) -> bool {
    KINDS.contains(&kind)
}

/// What a list is called where a key it doesn't take is said.
fn what(kind: GroupKind) -> &'static str {
    match kind {
        GroupKind::Messages => "the messages",
        _ => "the session list",
    }
}

impl Editor {
    /// Kind `kind`'s floating list, while it's up.
    pub fn float_list(&self, kind: GroupKind) -> Option<&FloatList> {
        match kind {
            GroupKind::Sessions => self.sessions_pane.as_ref(),
            GroupKind::Messages => self.messages_pane.as_ref(),
            _ => None,
        }
    }

    pub fn float_list_mut(&mut self, kind: GroupKind) -> Option<&mut FloatList> {
        self.float_list_place(kind)?.as_mut()
    }

    fn float_list_place(&mut self, kind: GroupKind) -> Option<&mut Option<FloatList>> {
        match kind {
            GroupKind::Sessions => Some(&mut self.sessions_pane),
            GroupKind::Messages => Some(&mut self.messages_pane),
            _ => None,
        }
    }

    /// Its rows' ids, in its order.
    pub fn float_list_ids(&self, kind: GroupKind) -> Vec<u64> {
        match kind {
            GroupKind::Sessions => self.sessions_in_order().iter().map(|l| l.id).collect(),
            GroupKind::Messages => self.message_ids(),
            _ => Vec::new(),
        }
    }

    /// Kind `kind`'s list up with the keys — opened, or up already — `select` the row in hand when given (else, opened,
    /// `first`).
    pub(crate) fn float_list_open(&mut self, kind: GroupKind, first: u64, select: Option<u64>) {
        let next = self.next_pane;
        let Some(place) = self.float_list_place(kind) else { return };
        let made = place.is_none();
        let p = place.get_or_insert_with(|| FloatList {
            list: next,
            selected: first,
            top: Default::default(),
            search: None,
            hl: false,
            prompt: None,
        });
        if let Some(id) = select {
            p.selected = id;
        }
        let list = p.list;
        if made {
            self.next_pane += 1;
        }
        self.session.at = At::List(list);
    }

    /// The keys to kind `kind`'s list, up (a press on it, `C-w w`).
    pub fn float_list_focus(&mut self, kind: GroupKind) {
        if let Some(p) = self.float_list(kind) {
            self.session.at = At::List(p.list);
        }
    }

    /// Kind `kind`'s list has the keys.
    pub fn float_list_keys(&self, kind: GroupKind) -> bool {
        self.float_list(kind).is_some_and(|p| self.session.docked() == At::List(p.list))
    }

    /// The floating list with the keys, if one has them.
    pub fn float_list_in_hand(&self) -> Option<GroupKind> {
        KINDS.into_iter().find(|&k| self.float_list_keys(k))
    }

    /// The floating list whose pane is `id`.
    pub fn float_list_of(&self, id: PaneId) -> Option<GroupKind> {
        KINDS.into_iter().find(|&k| self.float_list(k).is_some_and(|p| p.list == id))
    }

    /// `q` on a list: it goes, the keys to the code.
    pub fn float_list_close(&mut self, kind: GroupKind) {
        if let Some(p) = self.float_list_place(kind).and_then(Option::take) {
            self.session.leave(At::List(p.list));
        }
        self.float_moving = self.float_moving.filter(|&k| k != kind);
    }

    /// The list's keys go back to the code — it stays in sight.
    pub(crate) fn float_list_leave(&mut self, kind: GroupKind) {
        if let Some(p) = self.float_list(kind) {
            let list = p.list;
            self.session.leave(At::List(list));
        }
    }

    /// Done once used (user, 2026-10-23): what a list was opened for is done — floating, it closes; docked it would
    /// stay, its keys back to the code (neither list docks yet).
    pub(crate) fn float_list_used(&mut self, kind: GroupKind) {
        self.float_list_close(kind);
    }

    /// The row the list's selection is on (by its id, else its first row).
    pub fn float_list_here(&self, kind: GroupKind) -> Option<u64> {
        let p = self.float_list(kind)?;
        let ids = self.float_list_ids(kind);
        ids.contains(&p.selected).then_some(p.selected).or_else(|| ids.first().copied())
    }

    /// The selection on row `i` (in the list's order), `top` its first row in sight.
    pub(crate) fn float_list_select(&mut self, kind: GroupKind, i: usize, top: usize) {
        let id = self.float_list_ids(kind).get(i).copied();
        if let (Some(p), Some(id)) = (self.float_list_mut(kind), id) {
            p.selected = id;
            p.top.set(top);
        }
    }

    /// A press on row `i` of the list: the keys its, that row selected — the same row again within `DOUBLE_CLICK`
    /// opens it, as `enter`.
    pub fn float_list_click(&mut self, kind: GroupKind, i: usize) {
        let Some(p) = self.float_list(kind) else { return };
        let top = p.top.get();
        self.float_list_focus(kind);
        self.float_list_select(kind, i, top);
        if self.clicked_again(kind.name(), i) {
            self.float_list_enter(kind);
        }
    }

    /// `enter` on the list: what its kind does with the row in hand.
    fn float_list_enter(&mut self, kind: GroupKind) {
        match kind {
            GroupKind::Messages => self.messages_enter(),
            _ => self.sessions_enter(),
        }
    }

    /// The list's frame: the one you set (moved or sized, kept with the sizes), else a list's (`Frame::LIST`) — `h` 0
    /// while it's as tall as its rows.
    pub fn float_list_frame(&self, kind: GroupKind) -> Option<crate::pane::Frame> {
        self.float_list(kind)?;
        let kept = match kind {
            GroupKind::Messages => self.sizes.messages,
            _ => self.sizes.sessions,
        };
        Some(kept.unwrap_or(crate::pane::Frame::LIST))
    }

    /// The list's cells on a screen `w` wide with `h` rows above the status line (`Frame::rect_rows`).
    pub fn float_list_rect(&self, kind: GroupKind, w: usize, h: usize) -> Option<crate::split::Rect> {
        Some(self.float_list_frame(kind)?.rect_rows(w, h, self.float_list_ids(kind).len()))
    }

    /// The list's kept frame (`Sizes`): set, or back to a list's (None).
    pub(crate) fn float_list_frame_set(&mut self, kind: GroupKind, f: Option<crate::pane::Frame>) {
        match kind {
            GroupKind::Messages => self.sizes.messages = f,
            _ => self.sizes.sessions = f,
        }
    }

    /// `/`·`?` on the list: its own search's prompt, from the row in hand.
    fn float_list_search_open(&mut self, kind: GroupKind, reverse: bool) {
        let here = self.float_list_here(kind);
        if let Some(p) = self.float_list_mut(kind) {
            p.prompt = here;
        }
        self.open_prompt(crate::editor::PromptKind::Search { reverse }, "");
    }

    /// The row `pat` matches next after `from`'s (before it, `reverse`), round the list's end (said). Typing: only
    /// the selection moves, an unfinished pattern leaves it; entered: kept as the list's search, lit, and a pattern
    /// that isn't one or matches nothing said.
    fn float_list_search(&mut self, kind: GroupKind, pat: &str, reverse: bool, from: u64, typing: bool) {
        let re = match crate::search::compile(pat) {
            Ok(re) => re,
            Err(e) if !typing => return self.set_error(e),
            Err(_) => return,
        };
        if !typing && let Some(p) = self.float_list_mut(kind) {
            (p.search, p.hl) = (Some(pat.to_string()), true);
        }
        let ids = self.float_list_ids(kind);
        let hits = matched(self, kind, &re);
        let at = ids.iter().position(|&id| id == from).unwrap_or(0);
        let n = ids.len();
        let order: Vec<usize> = match reverse {
            false => (1..=n).map(|k| (at + k) % n).collect(),
            true => (1..=n).map(|k| (at + n - k) % n).collect(),
        };
        let Some(&i) = order.iter().find(|&&i| hits.get(i).copied().unwrap_or(false)) else {
            if !typing {
                let none = match kind {
                    GroupKind::Messages => "no message",
                    _ => "no session",
                };
                self.set_error(format!("{none} matches /{pat}/"));
            }
            return;
        };
        if let Some(p) = self.float_list_mut(kind) {
            p.selected = ids[i];
        }
        if !typing && if reverse { i >= at } else { i <= at } {
            self.set_status("search wrapped around");
        }
    }

    /// `n`·`N` on the list: its search's next row (the other way).
    fn float_list_search_next(&mut self, kind: GroupKind, reverse: bool) {
        let (Some(pat), Some(here)) =
            (self.float_list(kind).and_then(|p| p.search.clone()), self.float_list_here(kind))
        else {
            return self.set_status("no search in the list yet — / searches it");
        };
        self.float_list_search(kind, &pat, reverse, here, false);
    }
}

/// Which of the list's rows `re` matches, in its order.
fn matched(ed: &Editor, kind: GroupKind, re: &regex::Regex) -> Vec<bool> {
    match kind {
        GroupKind::Messages => crate::messages::rows(ed).iter().map(|r| re.is_match(&r.text)).collect(),
        _ => ed.sessions_rows().iter().map(|r| crate::sessionpane::matches(r, re)).collect(),
    }
}

/// The pattern kind `kind`'s list lights: the one its `/` is typing, else its last search while lit.
pub fn lit(ed: &Editor, kind: GroupKind) -> Option<regex::Regex> {
    let p = ed.float_list(kind)?;
    let typed = ed.prompt.as_ref().filter(|_| p.prompt.is_some()).map(|t| t.text.clone());
    let pat = match typed {
        Some(t) => Some(t).filter(|t| !t.is_empty()),
        None => p.search.clone().filter(|_| p.hl),
    }?;
    crate::search::compile(&pat).ok()
}

/// A list's own `/`·`?` is up (the code's search prompt isn't the code's).
pub fn searching(ed: &Editor) -> bool {
    KINDS.into_iter().any(|k| ed.float_list(k).is_some_and(|p| p.prompt.is_some()))
}

/// A key while a list's own `/`·`?` is up — false: the prompt isn't a list's. `esc` goes back to the row it opened on,
/// `enter` searches (empty: its last pattern again), typing moves the selection to the next match as it goes.
pub fn prompt_key(ed: &mut Editor, key: Key) -> bool {
    use crate::editor::PromptKind;
    let Some((kind, from)) =
        KINDS.into_iter().find_map(|k| ed.float_list(k).and_then(|p| p.prompt).map(|from| (k, from)))
    else {
        return false;
    };
    let Some(PromptKind::Search { reverse }) = ed.prompt.as_ref().map(|p| p.kind) else {
        // (another prompt now: the list's went)
        if let Some(p) = ed.float_list_mut(kind) {
            p.prompt = None;
        }
        return false;
    };
    let empty = ed.prompt.as_ref().is_some_and(|p| p.text.is_empty());
    let cancel = key.code == Code::Esc
        || (key.ctrl && key.code == Code::Char('c'))
        || (key.code == Code::Backspace && empty);
    if cancel {
        ed.prompt = None;
        if let Some(p) = ed.float_list_mut(kind) {
            (p.prompt, p.selected) = (None, from);
        }
        return true;
    }
    if crate::editor::prompt_submits(key) {
        let text = ed.prompt.take().map(|p| p.text).unwrap_or_default();
        let last = ed.float_list_mut(kind).and_then(|p| {
            p.prompt = None;
            p.search.clone()
        });
        let pat = if text.is_empty() { last.unwrap_or_default() } else { text };
        if pat.is_empty() {
            return true;
        }
        ed.float_list_search(kind, &pat, reverse, from, false);
        return true;
    }
    ed.handle_prompt_key(key);
    let typed = ed.prompt.as_ref().map(|p| p.text.clone()).unwrap_or_default();
    match typed.is_empty() {
        true => {
            if let Some(p) = ed.float_list_mut(kind) {
                p.selected = from;
            }
        }
        false => ed.float_list_search(kind, &typed, reverse, from, true),
    }
    true
}

/// The wheel over a list: its view three rows, the selection staying unless it would leave it (as `C-e`/`C-y`).
pub fn wheel(ed: &mut Editor, kind: GroupKind, up: bool) {
    let ids = ed.float_list_ids(kind);
    let here = ed.float_list_here(kind).and_then(|id| ids.iter().position(|&x| x == id)).unwrap_or(0);
    let top = top_was(ed, kind);
    let (top, at) =
        crate::key::in_sight(if up { -3 } else { 3 }, top, here, ids.len(), rows_in_sight(ed, kind), 0);
    ed.float_list_select(kind, at, top);
}

/// The list's first row in sight, as last drawn.
fn top_was(ed: &Editor, kind: GroupKind) -> usize {
    ed.float_list(kind).map_or(0, |p| p.top.get())
}

/// The list's rows in sight, as last drawn (none before).
fn rows_in_sight(ed: &Editor, kind: GroupKind) -> usize {
    ed.screen.list(kind).map_or(0, |l| l.1)
}

/// A key while a floating list has the keys — false when none has them.
pub fn key(ed: &mut Editor, key: Key) -> bool {
    if ed.mode != crate::editor::Mode::Normal || !ed.pending.is_empty() || ed.prompt.is_some() {
        return false;
    }
    let Some(kind) = ed.float_list_in_hand() else { return false };
    let ids = ed.float_list_ids(kind);
    let here = ed.float_list_here(kind).and_then(|id| ids.iter().position(|&x| x == id)).unwrap_or(0);
    let top = top_was(ed, kind);
    let rows = rows_in_sight(ed, kind);
    if let Some(m) = crate::key::list_move(key, rows) {
        let (selected, top) = match m {
            ListMove::Scroll(by) => {
                let (top, at) = crate::key::in_sight(by, top, here, ids.len(), rows, 0);
                (at, top)
            }
            m => (m.from(here, ids.len()), top),
        };
        // (at its end, or one row: said — no silent key)
        if (selected, top) == (here, top_was(ed, kind)) {
            ed.key_unused(key, what(kind), "it's at its end");
        }
        ed.float_list_select(kind, selected, top);
        return true;
    }
    match (key.code, key.ctrl, key.alt) {
        (Code::Char(' ' | ':'), false, false) | (Code::Char('w'), true, false) => return false,
        // (`esc` as the code's: its search's matches unlit too)
        (Code::Esc, ..) => {
            if let Some(p) = ed.float_list_mut(kind) {
                p.hl = false;
            }
            ed.float_list_leave(kind);
        }
        (Code::Char('q'), false, false) => ed.float_list_close(kind),
        (Code::Enter, false, false) => ed.float_list_enter(kind),
        // Its own search, as every pane's (user, 2026-10-23: never type-to-narrow — its letters are keys)
        (Code::Char(c @ ('/' | '?')), false, false) => ed.float_list_search_open(kind, c == '?'),
        (Code::Char(c @ ('n' | 'N')), false, false) => ed.float_list_search_next(kind, c == 'N'),
        (Code::Char(c), false, false) => match kind {
            GroupKind::Messages => ed.messages_key(c),
            _ => ed.session_list_key(c),
        },
        _ => ed.key_unused(key, what(kind), "esc code"),
    }
    true
}

#[cfg(test)]
mod tests;
