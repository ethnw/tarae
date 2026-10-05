//! The inbox: everything waiting for the user's answer, in every session, in the order it came — Claude's
//! run and read cards, its edits waiting for their review (`Change 2 of 5`; a protected file like
//! `.tarae.toml` always asks), change sets, and tarae's own cards (trust, a rebase, a download). Built on the
//! UI state's cards (`Editor::ui_cards`, parked ones included), so the state and the inbox can't disagree;
//! the state lists it in its order (`UiState.inbox`).
//!
//! **A picker, not a panel** (`space I`, `:inbox`, a click on the status line's `▲3 waiting`): what waits
//! comes and goes within minutes and is nothing most of the day — a panel would hold its room for an empty
//! list, where the status line's count is what stays in sight; the list is a moment's choice, typed to
//! narrow by session or kind. `enter` goes to the one in hand — its session to the front, its card up with
//! the keys — and it's answered there, on its card, as ever: never from the list.
//!
//! `C-q` takes the same order: to the one that has waited longest, in any session (#90, #94) — once it's the
//! card up, `C-q` gives it the keys. While anything waits, `C-q` never says nothing does. Claude's runs gone
//! quiet are listed after everything that asks for an answer (#171), and so is a reviewer that ended in a
//! session behind (#195) — news to read there, gone once that session is in front.

use std::time::Instant;

use serde_json::{Value, json};

use crate::editor::Editor;
use crate::event::SessionId;
use crate::sizes::Slot;
use crate::uistate::{CardState, session_id};

/// One thing waiting for the user.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// `card#12` · `edit#s1:review` (an edit in its review) · `edit#s1:<request>` (one waiting for it) ·
    /// `read#s1:<request>` (a read waiting for its card) · `set#s1:<n>` (a change set's files, one by one) ·
    /// `run#…` (Claude's run gone quiet) · `reviewer#s1` (its reviewer ended while it was behind).
    pub id: String,
    /// Its session, as the state names it (`s1`).
    pub session: String,
    /// A card's kind (`run`, `read`, `project-trust` …), `edit`, `read`, `change-set`, `silent-run`,
    /// `reviewer`.
    pub kind: &'static str,
    /// What it asks: a card's title and what's decided; an edit's change and file; a reviewer's end, what it
    /// found and where to read it, or why it stopped.
    pub asks: String,
    pub waited_ms: u64,
    /// Where it is: a card `front` · `stacked` · `parked`; an edit `review` (up in its session) · `queued`;
    /// a run `running`; a reviewer `ended`.
    pub up: &'static str,
}

impl Entry {
    /// Its session's id.
    pub fn session_id(&self) -> SessionId {
        self.session[1..].parse().unwrap_or(0)
    }

    /// Whether it asks for an answer — a card, an edit, a read, a change set — rather than being news to look
    /// at: Claude's run gone quiet (#157), a reviewer that ended behind (#195). Only those that ask are
    /// `▲ waiting` (the status line, a card's count); the rest are said apart, calm.
    pub fn asks_answer(&self) -> bool {
        !matches!(self.kind, "silent-run" | "reviewer")
    }

    /// Its card's id, if it's a card.
    pub fn card(&self) -> Option<u64> {
        self.id.strip_prefix("card#")?.parse().ok()
    }

    pub fn json(&self) -> Value {
        json!({
            "id": self.id, "session": self.session, "kind": self.kind, "asks": self.asks,
            "waited_ms": self.waited_ms, "up": self.up,
        })
    }
}

/// Where `C-q` goes, unheld: the inbox's first.
#[derive(Clone, Debug, PartialEq)]
pub enum Next {
    /// The card up in front: it takes the keys.
    Card(u64),
    /// The front session's edit review: its own `C-q` brings the keys back to it.
    Review,
    /// Elsewhere: another session, a card stacked behind the front one, an edit waiting its turn.
    Go(Box<Entry>),
    /// Nothing waits, and this session behind ended its turn unseen — the first of them to end (its tab's
    /// `done`). Not an entry of the inbox (nothing there asks for an answer): a tail after it.
    Done(SessionId),
    /// Nothing waits, no turn ended unseen.
    None,
}

/// How long it has waited, as a row's hint says it: `12 s`, `4 min`, `2 h`.
fn waited(ms: u64) -> String {
    match ms / 1000 {
        s if s < 60 => format!("{s} s"),
        s if s < 3600 => format!("{} min", s / 60),
        s => format!("{} h", s / 3600),
    }
}

fn ms(since: Instant, now: Instant) -> u64 {
    now.saturating_duration_since(since).as_millis().min(u64::MAX as u128) as u64
}

/// A session's runs of Claude's that have printed nothing for `waits::SILENT` or more: (id as the state
/// names it, the slot it's in, its label, when it last printed, `no output for 27:04`).
fn silent_runs(l: &crate::session::Session, now: Instant) -> Vec<(String, Slot, String, Instant, String)> {
    let mut out = Vec::new();
    if let Some(r) = l.task_run.as_ref().filter(|r| r.by_claude && r.state == crate::task::State::Running)
        && let Some(said) = r.wait.silent(r.last_output, now)
    {
        let label = crate::task::traced(&r.task.cmd);
        out.push((format!("run#{}", r.trace.id()), Slot::Task, label, r.last_output, said));
    }
    if let Some(r) =
        l.test_run.as_ref().filter(|r| r.by_claude && r.state == crate::testing::RunState::Running)
        && let Some(said) = r.wait.silent(r.last_output, now)
    {
        let label = format!("tests {}", r.target.label);
        out.push((format!("run#{}", r.trace.id()), Slot::Test, label, r.last_output, said));
    }
    out
}

impl Editor {
    /// What waits for the user, oldest first.
    pub fn inbox(&self) -> Vec<Entry> {
        self.inbox_from(&self.ui_cards())
    }

    /// The inbox from the state's cards (`ui_state` lists both from one call) and every session's edits,
    /// change sets and reads waiting. What asks for an answer comes first, oldest first; quiet runs and
    /// reviewers that ended behind after it, however long ago — a slow run is only maybe stuck, a reviewer's
    /// end is news to read, a card surely waits (#171).
    /// The order is by when each came, not by how long each has waited as read (whole milliseconds, each at
    /// its own moment: two cards come within one swapped between `keys_to`'s read and `handle_key`'s — the
    /// routing test, now and then); things that came at the same instant keep the order they came in.
    pub(crate) fn inbox_from(&self, cards: &[CardState]) -> Vec<Entry> {
        let now = Instant::now();
        // (`seq`: a card's id counts up as cards come — the tie-break)
        let mut v: Vec<(Entry, Instant, u64)> = cards
            .iter()
            .filter(|c| c.state != "installing")
            .map(|c| {
                let e = Entry {
                    id: c.id.clone(),
                    session: c.session.clone(),
                    kind: c.kind,
                    asks: c.asks.clone(),
                    waited_ms: ms(c.since, now),
                    up: c.up,
                };
                let seq = e.card().unwrap_or(0);
                (e, c.since, seq)
            })
            .collect();
        for l in std::iter::once(&self.session).chain(&self.sessions) {
            let s = session_id(l.id);
            let entry = |id: String, kind, asks: String, since, up| {
                (Entry { id, session: s.clone(), kind, asks, waited_ms: ms(since, now), up }, since, u64::MAX)
            };
            if let Some(r) = l.review.as_ref().filter(|r| r.ready) {
                let file = self.docs.iter().find(|d| d.id == r.doc_id).map(|d| d.display_name(&l.root));
                let change = format!("Change {} of {}", r.current + 1, r.hunks.len());
                let asks = match file {
                    Some(f) => format!("{change} · {f}"),
                    None => change,
                };
                v.push(entry(format!("edit#{s}:review"), "edit", asks, r.asked, "review"));
            }
            // Claude's run printing nothing for minutes: something to look at — its turn waits on it, and
            // only the user can stop it (#157). Waited since it went quiet.
            for (id, _, label, last, said) in silent_runs(l, now) {
                v.push(entry(id, "silent-run", format!("{label} · {said}"), last, "running"));
            }
            // Its reviewer ended while it was behind (#195): news, not a question — what it found, where to
            // read it; gone once the session is in front
            if let Some((said, at)) = crate::reviewer::news(l) {
                v.push(entry(format!("reviewer#{s}"), "reviewer", said, at, "ended"));
            }
            let Some(c) = l.chat.as_ref() else { continue };
            for p in &c.edits.queue {
                let path = crate::chat::resolve(&l.root, p.input["file_path"].as_str().unwrap_or_default());
                let asks = format!("Claude's edit to {}", crate::chat::shown(&l.root, &path));
                v.push(entry(format!("edit#{s}:{}", p.request_id), "edit", asks, p.since, "queued"));
            }
            for p in &c.reads {
                let at = p.look.at.as_deref().map_or_else(|| p.look.given.clone(), crate::allow::tilde);
                let asks = format!("Claude's {} of {at}", p.look.tool);
                v.push(entry(format!("read#{s}:{}", p.request_id), "read", asks, p.since, "queued"));
            }
            for (i, set) in c.edits.sets.iter().enumerate().filter(|(_, set)| set.waiting() > 0) {
                let (what, since) = set.what();
                let n = set.waiting();
                let asks = format!("{what} · {n} file{} to review", if n == 1 { "" } else { "s" });
                v.push(entry(format!("set#{s}:{i}"), "change-set", asks, since, "queued"));
            }
        }
        let quiet = |e: &Entry| !e.asks_answer();
        v.sort_by(|a, b| (quiet(&a.0).cmp(&quiet(&b.0))).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
        v.into_iter().map(|(e, ..)| e).collect()
    }

    /// When a run of Claude's in any session next goes silent (`waits::SILENT`) — a frame then, for the
    /// status line's count and the inbox (nothing else may draw: the session behind, Claude idle there).
    pub(crate) fn silent_wake(&self) -> Option<std::time::Duration> {
        let now = Instant::now();
        let mut next: Option<std::time::Duration> = None;
        for l in std::iter::once(&self.session).chain(&self.sessions) {
            let task = l.task_run.as_ref().filter(|r| r.by_claude && r.state == crate::task::State::Running);
            let test =
                l.test_run.as_ref().filter(|r| r.by_claude && r.state == crate::testing::RunState::Running);
            let ins = task
                .and_then(|r| r.wait.silent_in(r.last_output, now))
                .into_iter()
                .chain(test.and_then(|r| r.wait.silent_in(r.last_output, now)));
            for d in ins {
                next = Some(next.map_or(d, |n| n.min(d)));
            }
        }
        next.map(|d| d + std::time::Duration::from_millis(5))
    }

    /// Where `C-q` goes now, unheld — the inbox's first, and what that is from here.
    pub fn next_up(&self) -> Next {
        let Some(e) = self.inbox().into_iter().next() else {
            return self.turns_ended().first().map_or(Next::None, |&id| Next::Done(id));
        };
        let here = e.session_id() == self.session.id && !self.background();
        match e.card() {
            Some(id) if here && self.offers.first().is_some_and(|o| o.id == id && !o.installing()) => {
                Next::Card(id)
            }
            None if here && e.up == "review" => Next::Review,
            _ => Next::Go(Box::new(e)),
        }
    }

    /// `C-q` with no card of its to take it: to the one that has waited longest — nothing waiting, to the
    /// session whose turn ended first, unseen. False: nothing waits, no turn ended.
    pub(crate) fn inbox_next(&mut self) -> bool {
        match self.next_up() {
            Next::None => false,
            Next::Go(e) => {
                self.inbox_to(&e);
                true
            }
            Next::Done(id) => {
                let Some(l) = self.sessions.iter().find(|l| l.id == id) else { return false };
                let ago = l.turn_end.as_ref().map_or(0, |t| ms(t.0, Instant::now()));
                let said = format!("{} · its turn ended {} ago", l.label(), waited(ago));
                self.session_switch(id);
                self.set_status(said);
                true
            }
            // (the card up, the review in front: theirs — `card_key`, `llm::review_key`)
            Next::Card(_) | Next::Review => true,
        }
    }

    /// To entry `e`: its session to the front, its card up front (stacked behind another: brought forward),
    /// an edit's review up with the keys, or the next of its queue shown. What was being typed there waits
    /// (`session_switch` leaves insert mode, a prompt, a picker as it always does).
    fn inbox_to(&mut self, e: &Entry) {
        let id = e.session_id();
        if id != self.session.id && !self.session_switch(id) {
            return;
        }
        if let Some(card) = e.card() {
            if let Some(i) = self.offers.iter().position(|o| o.id == card) {
                self.offer_to_front(i);
            }
            return;
        }
        // A reviewer that ended: there now — coming said it (`reviewer::session_in`)
        if e.kind == "reviewer" {
            return;
        }
        // A silent run: its panel in front with the keys — stopping it is the user's call there
        if e.kind == "silent-run" {
            let Some((_, slot, _, _, said)) =
                silent_runs(&self.session, Instant::now()).into_iter().find(|r| r.0 == e.id)
            else {
                return self.set_status("that run isn't quiet any more — it printed, or ended");
            };
            self.slot_show(slot);
            self.bottom_take_keys();
            let stop = if slot == Slot::Task { "space m c" } else { "space x c" };
            return self.set_status(format!("Claude's run: {said} — {stop} stops it, Claude hears why"));
        }
        // An edit or a read: what holds it back here goes (insert mode, a prompt — its review waits for
        // you to stop typing), then the queues go on
        if self.prompt.is_some() || self.mode != crate::editor::Mode::Normal {
            self.handle_key(crate::key::Key::plain(crate::key::Code::Esc));
            self.mode = crate::editor::Mode::Normal;
        }
        crate::edits::next(self);
        crate::refactor::next(self);
        if let Some(r) = self.session.review.as_mut() {
            r.away = false;
            if let Some(c) = self.session.chat.as_mut() {
                c.focused = false;
            }
        }
        // (one behind another of its kind: it comes once that one is answered — said)
        if self.inbox().iter().any(|x| x.id == e.id && x.up == "queued") {
            self.set_status("it comes once the one up there is answered");
        }
    }

    /// `enter` in the inbox: to the entry, and its card takes the keys — answered there. Gone meanwhile
    /// (answered elsewhere, its turn ended): said, and the list follows.
    pub(crate) fn inbox_go(&mut self, id: &str) {
        let Some(e) = self.inbox().into_iter().find(|e| e.id == id) else {
            return self.set_status("that one isn't waiting any more — answered, or its turn ended");
        };
        self.inbox_to(&e);
        if e.card().is_some_and(|c| self.offers.first().is_some_and(|o| o.id == c)) {
            self.card_keys = true;
        }
    }

    /// The inbox's rows: session (with more than one), what it asks · kind and how long it has waited.
    fn inbox_items(&self) -> Vec<crate::picker::Item> {
        let names: Vec<(String, String)> = std::iter::once(&self.session)
            .chain(&self.sessions)
            .map(|l| (session_id(l.id), l.label().to_string()))
            .collect();
        let many = !self.sessions.is_empty();
        self.inbox()
            .into_iter()
            .map(|e| {
                let name = names.iter().find(|n| n.0 == e.session).map_or("", |n| n.1.as_str());
                let label = if many { format!("{name} · {}", e.asks) } else { e.asks.clone() };
                let claudes = matches!(
                    e.kind,
                    "run" | "read" | "no-sandbox" | "screen" | "edit" | "change-set" | "silent-run"
                );
                // (a reviewer's end: its own glyph, as its findings have it — news, not a question)
                let glyph = match e.kind {
                    "reviewer" => ("¶", "type"),
                    _ => ("▲", if claudes { "warning" } else { "ui.accent" }),
                };
                crate::picker::Item {
                    label,
                    hint: format!("{} · {}", e.kind, waited(e.waited_ms)),
                    action: crate::picker::Action::Inbox(e.id),
                    glyph: Some(glyph),
                }
            })
            .collect()
    }

    /// `space I` · `:inbox` · a click on the status line's count — the list, oldest first, as wide as the
    /// picker (what each asks is the row; the whole of it is its card, one `enter` away). Nothing waiting:
    /// said.
    pub fn inbox_open(&mut self) {
        let items = self.inbox_items();
        if items.is_empty() {
            return self.set_status("nothing waits for you — no card, no edit, in any session");
        }
        let p = crate::picker::Picker::new(INBOX, items, false).without_preview();
        self.open_picker(p, None);
    }

    /// Every event: the inbox, while it's open, lists what waits now (the row in hand stays).
    pub(crate) fn inbox_sync(&mut self) {
        if !self.picker.as_ref().is_some_and(|p| p.title == INBOX) {
            return;
        }
        let items = self.inbox_items();
        let Some(p) = self.picker.as_mut() else { return };
        if p.items().iter().map(|i| &i.action).ne(items.iter().map(|i| &i.action)) {
            if items.is_empty() {
                self.picker = None;
                return self.set_status("nothing waits for you any more");
            }
            p.set_found(items);
        }
    }
}

/// The inbox picker's title — its key hints and its sync go by it.
pub const INBOX: &str = "waiting for you";

#[cfg(test)]
mod tests;
