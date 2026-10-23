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
//! `C-q` takes the same order, but for what's in front: the session in front's own first (#198), then the one
//! that has waited longest, in any session (#90, #94) — once it's the card up, `C-q` gives it the keys (a
//! rebase's card put away gets them as it comes back: it was put away, not new). While
//! anything waits, `C-q` never says nothing does. Claude's runs gone
//! quiet are listed after everything that asks for an answer (#171), and so is a reviewer that ended in a
//! session behind (#195) — news to read there, gone once that session is in front.

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::editor::Editor;
use crate::event::SessionId;
use crate::sizes::Slot;
use crate::uistate::{CardState, session_id};

/// What `space I` and `C-q` say when nothing waits — one sentence for the one empty state.
pub const NOTHING: &str = "nothing waits for you — no card, no edit, in any session";

/// One thing waiting for the user.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// `card#12` · `edit#s1:review` (an edit in its review) · `edit#s1:<request>` (one waiting for it) ·
    /// `read#s1:<request>` (a read waiting for its card) · `set#s1:<n>` (a change set's files, one by one) ·
    /// `apply#s1` (the user's own change set's files, one by one) ·
    /// `run#…` (Claude's run gone quiet) · `reviewer#s1` (its reviewer ended while it was behind) ·
    /// `rebase#s1` (its rebase resolved, the card put away).
    pub id: String,
    /// Its session, as the state names it (`s1`).
    pub session: String,
    /// A card's kind (`run`, `read`, `project-trust` …), `edit`, `read`, `change-set`, `silent-run`,
    /// `reviewer`, `rebase`.
    pub kind: &'static str,
    /// What it asks: a card's title and what's decided; an edit's change and file; a reviewer's end, what it
    /// found and where to read it, or why it stopped.
    pub asks: String,
    pub waited_ms: u64,
    /// Where it is: a card `front` · `stacked` · `parked`; an edit `review` (up in its session) · `queued`;
    /// a run `running`; a reviewer `ended`; a rebase's card `put-away`.
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

/// The inbox's order — the one place it's decided: the card the user's own command just made while it's in
/// front (`yours`, #205 — user decision 2026-10-13: `C-q` and `enter` go to it, Claude's stays stacked), then
/// what asks for an answer before what doesn't, oldest first, then the order they came in (an entry, how long
/// it has waited — each on its own clock, a card's on logic's and the rest's on the real one, so the two never
/// meet as instants — its card's id).
fn in_line(a: &(Entry, Duration, u64), b: &(Entry, Duration, u64), yours: Option<u64>) -> std::cmp::Ordering {
    let first = |e: &Entry| yours.is_none() || e.card() != yours;
    let quiet = |e: &Entry| !e.asks_answer();
    (first(&a.0).cmp(&first(&b.0)))
        .then(quiet(&a.0).cmp(&quiet(&b.0)))
        .then(b.1.cmp(&a.1))
        .then(a.2.cmp(&b.2))
}

fn ms(since: Instant, now: Instant) -> u64 {
    now.saturating_duration_since(since).as_millis().min(u64::MAX as u128) as u64
}

/// A session's runs of Claude's that have printed nothing for `waits::SILENT` or more, and aren't busy at it
/// (`Waiting::worth_a_look`): (id as the state names it, the slot it's in, its label, when it last printed,
/// `no output for 27:04`, and `· its output goes through tail` when a filter holds it).
fn silent_runs(l: &crate::session::Session, now: Instant) -> Vec<(String, Slot, String, Instant, String)> {
    let mut out = Vec::new();
    if let Some(r) = l.task_run.as_ref().filter(|r| r.by_claude && r.state == crate::task::State::Running)
        && r.wait.worth_a_look(r.started, r.last_output, now)
        && let Some(said) = r.wait.silent(r.last_output, now)
    {
        let label = crate::task::traced(&r.task.cmd);
        let held = crate::readonly::filtered_by(&r.task.cmd)
            .map(|f| format!(" · its output goes through {f}"))
            .unwrap_or_default();
        let stop = busy_stop(&r.wait, r.last_output, now, Slot::Task);
        out.push((format!("run#{}", r.trace.id()), Slot::Task, label, r.last_output, said + &held + &stop));
    }
    if let Some(r) =
        l.test_run.as_ref().filter(|r| r.by_claude && r.state == crate::testing::RunState::Running)
        && r.wait.worth_a_look(r.started, r.last_output, now)
        && let Some(said) = r.wait.silent(r.last_output, now)
    {
        let label = format!("tests {}", r.target.label);
        let stop = busy_stop(&r.wait, r.last_output, now, Slot::Test);
        out.push((format!("run#{}", r.trace.id()), Slot::Test, label, r.last_output, said + &stop));
    }
    out
}

/// The key that stops a run in `slot` (its panel's).
pub fn stop_key(slot: Slot) -> &'static str {
    match slot {
        Slot::Test => "space x c",
        _ => "space m c",
    }
}

/// ` — space m c stops it` for a run busy at nothing it shows (`Waiting::busy`, frictions 53), else nothing.
fn busy_stop(w: &crate::waits::Waiting, last: Instant, now: Instant, slot: Slot) -> String {
    w.busy(last, now).map(|_| format!(" — {} stops it", stop_key(slot))).unwrap_or_default()
}

impl Editor {
    /// What waits for the user, oldest first — but the card the user's own command made, while in front.
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
        // (a card's time is on logic's clock, `clock`; the rest's are still the real one's — a review's, a run's
        // last output, a reviewer's, a rebase's, Claude's edits and reads — each read against its own)
        let (now, real) = (crate::clock::now(), Instant::now());
        // (`seq`: a card's id counts up as cards come — the tie-break)
        let mut v: Vec<(Entry, Duration, u64)> = cards
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
                (e, now.saturating_duration_since(c.since), seq)
            })
            .collect();
        for l in std::iter::once(&self.session).chain(&self.sessions) {
            let s = session_id(l.id);
            let entry = |id: String, kind, asks: String, since, up| {
                (
                    Entry { id, session: s.clone(), kind, asks, waited_ms: ms(since, real), up },
                    real.saturating_duration_since(since),
                    u64::MAX,
                )
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
            for (id, _, label, last, said) in silent_runs(l, real) {
                v.push(entry(id, "silent-run", format!("{label} · {said}"), last, "running"));
            }
            // Its reviewer ended while it was behind (#195): news, not a question — what it found, where to
            // read it; gone once the session is in front
            if let Some((said, at)) = crate::reviewer::news(l) {
                v.push(entry(format!("reviewer#{s}"), "reviewer", said, at, "ended"));
            }
            // A rebase that waits on you, its card put away (`n`) — resolved, or stopped with nothing asked of
            // its Claude (one taken up after a restart): `enter` and `C-q` bring its card back. Waited since
            // it began to wait (`Rebase::waits_on_you`: git's own time for one tarae didn't start)
            if let Some(r) = l.rebase.as_ref()
                && let Some(since) = r.waits_on_you()
                && !cards.iter().any(|c| c.kind == "rebase-resolved" && c.session == s)
            {
                let asks = match r.stage {
                    crate::rebase::Stage::Resolved => {
                        format!("{}: conflicts resolved — y continues the rebase, X aborts it", l.label())
                    }
                    _ => format!("{}: {} stopped — y goes on, X aborts it", l.label(), r.said()),
                };
                v.push(entry(format!("rebase#{s}"), "rebase", asks, since, "put-away"));
            }
            // The user's own change set (`:session-apply`), its files waiting for their review — on logic's
            // clock, as a card's
            if let Some(set) = l.applying.as_ref().filter(|set| set.waiting() > 0) {
                let ((what, since), n) = (set.what(), set.waiting());
                let asks = format!("{what} · {n} file{} to review", if n == 1 { "" } else { "s" });
                let id = format!("apply#{s}");
                let e = Entry {
                    id,
                    session: s.clone(),
                    kind: "change-set",
                    asks,
                    waited_ms: ms(since, now),
                    up: "queued",
                };
                v.push((e, now.saturating_duration_since(since), u64::MAX));
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

        // (the front card here, when the user's own command made it)
        let yours = self
            .offers
            .first()
            .filter(|o| o.rank() == crate::offer::Rank::Yours && !o.installing())
            .map(|o| o.id);
        crate::sorting::by(&mut v, |a, b| in_line(a, b, yours));
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

    /// Where `C-q` goes now, unheld — this session's own first (#198: an older one elsewhere waits while you
    /// answer what's here), else the inbox's first; and what that is from here.
    pub fn next_up(&self) -> Next {
        let inbox = self.inbox();
        let front = !self.background();
        let own = inbox.iter().position(|e| front && e.asks_answer() && e.session_id() == self.session.id);
        let Some(e) = inbox.into_iter().nth(own.unwrap_or(0)) else {
            return self.turns_ended().first().map_or(Next::None, |&id| Next::Done(id));
        };
        let here = e.session_id() == self.session.id && front;
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
                // (a card there takes the keys at once, as `enter` in the inbox gives them — frictions29 19: there,
                // that session's chat kept them, and the `n` meant for the card was typed into it)
                if e.card().is_some_and(|c| self.offers.first().is_some_and(|o| o.id == c && !o.installing()))
                {
                    self.card_keys = true;
                }
                true
            }
            Next::Done(id) => {
                let Some(l) = self.sessions.iter().find(|l| l.id == id) else { return false };
                let ago = l.turn_end.as_ref().map_or(0, |t| ms(t.0, Instant::now()));
                let ended = crate::term::ago(Duration::from_millis(ago), false);
                let said = format!("{} · its turn ended {ended}", l.label());
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
        // A rebase's card put away: up again, in front, with the keys — from `enter` and `C-q` alike (it was
        // put away, not answered: going to it is going to answer it)
        if e.kind == "rebase" {
            if let Err(why) = self.rebase_offer(id) {
                return self.set_status(why);
            }
            let mine = |o: &crate::offer::Offer| matches!(o.what, crate::offer::What::RebaseResolved { .. });
            if let Some(i) = self.offers.iter().position(mine) {
                self.offer_to_front(i);
                self.card_keys = true;
            }
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
            self.session.leave(crate::session::At::Chat);
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
                // (one row: cut as the one-line forms cut it — the whole is its card, one `enter` away)
                let asks = crate::uistate::asks_line(&e.asks);
                let label = if many { format!("{name} · {asks}") } else { asks };
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
                    // (its time a column: the rows line up whatever each has waited)
                    hint: format!(
                        "{} · {}",
                        e.kind,
                        crate::term::time_column(Duration::from_millis(e.waited_ms))
                    ),
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
            return self.set_status(NOTHING);
        }
        let p = crate::picker::Picker::of("inbox", INBOX, items, false).without_preview();
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
