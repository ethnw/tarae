//! Watching the UI state (phase 2): a driver connected with `{"watch":[…]}` gets an event a line as things
//! change, instead of asking again and again. After each frame, a small summary — where the keys go, the
//! cards (id, rev, where they are), what each session is doing, the toasts — is compared with the last one
//! sent, and what changed goes out (`keys`, `card-up`, `session`, `toast`); what a diff can't tell is said
//! where it happens (`card-gone` and how, `turn`, `run` — every step of a run's trace). Each carries
//! `frame`, `input` and `said`, one line in plain words.
//!
//! Nobody watching: one branch a frame (and the gone ring's few ids). A watcher never wakes tarae or makes
//! it draw — events go out after frames drawn anyway. A watcher that doesn't read is let go after `UNREAD`
//! events, told so on its last line.
//!
//! The gone ring (`Editor::ui_gone`): the last `GONE_KEPT` cards that went, and how — answered `y`, `n`,
//! dropped as the turn ended, lost, done — so an id asked about after it went still says something.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};

use serde_json::{Value, json};

use crate::editor::Editor;

/// Events a watcher may leave unread before it's let go.
pub const UNREAD: usize = 256;
/// Cards the gone ring remembers.
pub const GONE_KEPT: usize = 200;

/// A connection watching: where its lines go, which kinds it asked for (none: all), and the flag its
/// connection reads once it's let go for not reading.
pub struct Watcher {
    pub tx: SyncSender<String>,
    pub kinds: Vec<String>,
    pub overflowed: Arc<AtomicBool>,
}

/// What the diff compares, frame to frame.
#[derive(Clone, Debug, Default, PartialEq)]
struct Summary {
    keys: (String, String, String, bool, bool),
    /// (id, rev, up, session, its `asks`, the line `said` gives it)
    cards: Vec<(String, u64, &'static str, String, String, String)>,
    /// (id, name, doing, its reviewer's)
    sessions: Vec<(String, String, String, Option<&'static str>)>,
    /// (session id, its name, its reviewer's last run) — a `reviewer` event when it starts, ends or stops.
    reviewers: Vec<(String, String, super::ReviewerState)>,
    /// (id, kind, text)
    toasts: Vec<(String, &'static str, String)>,
}

/// The editor's watchers, what was last sent, and what was said since (explicit events, before their frame).
#[derive(Default)]
pub struct Watch {
    pub watchers: Vec<Watcher>,
    last: Option<Summary>,
    pending: Vec<Value>,
}

/// A card that went: its id, title, how, and when — and whose session it was (Claude's level lists only its
/// own).
#[derive(Clone, Debug, PartialEq)]
pub struct Gone {
    pub id: String,
    pub title: String,
    pub how: String,
    pub at: std::time::Instant,
    pub session: String,
}

impl Watch {
    pub fn watching(&self) -> bool {
        !self.watchers.is_empty()
    }
}

fn summary(s: &super::UiState) -> Summary {
    let k = &s.keys;
    Summary {
        keys: (k.to.clone(), k.region.name(), k.takes.name().to_string(), k.drawn, k.held),
        cards: s
            .cards
            .iter()
            .map(|c| {
                let answers: Vec<String> =
                    c.answers.iter().map(|(k, d)| format!("{k} {}", d.to_lowercase())).collect();
                // (whole: what a driver is asked to approve, all of it — frictions15)
                let line = format!("{} — {}", c.asks, answers.join(" · "));
                (c.id.clone(), c.rev, c.up, c.session.clone(), c.asks.clone(), line)
            })
            .collect(),
        sessions: s
            .sessions
            .iter()
            .map(|x| (x.id.clone(), x.name.clone(), x.doing.clone(), x.reviewer))
            .collect(),
        reviewers: s
            .sessions
            .iter()
            .filter_map(|x| Some((x.id.clone(), x.name.clone(), x.reviewer_run.clone()?)))
            .collect(),
        toasts: s.toasts.iter().map(|t| (t.id.clone(), t.kind, t.text.clone())).collect(),
    }
}

/// The events between two summaries.
fn diff(old: &Summary, new: &Summary) -> Vec<Value> {
    let mut out = Vec::new();
    if old.keys != new.keys {
        let (to, region, takes, drawn, held) = &new.keys;
        let mut said = format!("keys → {} ({takes})", super::keys_place(to));
        if !drawn {
            said += " — not on screen";
        }
        out.push(json!({ "event": "keys", "to": to, "region": region, "takes": takes, "drawn": drawn,
                         "held": held, "was": old.keys.0, "said": said }));
    }
    for (id, rev, up, session, asks, line) in &new.cards {
        let before = old.cards.iter().find(|c| &c.0 == id);
        if before.is_none_or(|b| b.1 != *rev || b.2 != *up) {
            let said = match before {
                None => format!("{id} up ({up}, {session}): {line}"),
                Some(_) => format!("{id} now {up}: {line}"),
            };
            out.push(json!({ "event": "card-up", "id": id, "rev": rev, "up": up, "session": session,
                             "asks": asks, "said": said }));
        }
    }
    // (its reviewer said after what its chat does: `idle · reviewing`)
    let shown = |doing: &str, reviewer: Option<&str>| match reviewer {
        Some(r) => format!("{doing} · {r}"),
        None => doing.to_string(),
    };
    for (id, name, doing, reviewer) in &new.sessions {
        let before = old.sessions.iter().find(|s| &s.0 == id);
        if before.is_none_or(|b| b.2 != *doing || b.3 != *reviewer) {
            let now = shown(doing, *reviewer);
            let said = match before {
                Some(b) => format!("{id} {name}: {} → {now}", shown(&b.2, b.3)),
                None => format!("{id} {name}: {now}"),
            };
            let was = before.map(|b| b.2.as_str());
            out.push(json!({ "event": "session", "id": id, "doing": doing, "was": was, "reviewer": reviewer,
                             "said": said }));
        }
    }
    // (its state only: every read or finding would be an event — the end carries the counts)
    for (id, name, r) in &new.reviewers {
        let before = old.reviewers.iter().find(|x| &x.0 == id).map(|x| x.2.state);
        if before != Some(r.state) {
            let mut ev = r.json();
            ev["event"] = json!("reviewer");
            ev["session"] = json!(id);
            ev["was"] = json!(before);
            ev["said"] = json!(format!("{id} {name}: reviewer {}", r.text()));
            out.push(ev);
        }
    }
    for (id, kind, text) in &new.toasts {
        if !old.toasts.iter().any(|t| &t.0 == id) {
            let first = text.lines().next().unwrap_or_default();
            out.push(json!({ "event": "toast", "id": id, "kind": kind, "text": text,
                             "said": format!("{kind}: {first}") }));
        }
    }
    out
}

/// The kind a watcher names an event by.
fn kind_of(event: &str) -> &str {
    match event {
        "card-up" | "card-gone" => "cards",
        "session" => "sessions",
        "turn" => "turns",
        "run" => "runs",
        "toast" => "toasts",
        other => other,
    }
}

impl Editor {
    /// What card `id` went by, said where it went (`y`, `n`, dropped, lost) — the gone ring takes it when the
    /// card leaves (`cards_sync`).
    pub(crate) fn card_how(&mut self, id: u64, how: impl Into<String>) {
        self.ui_card_how.insert(id, how.into());
    }

    /// An event only the code that made it can tell (`turn`): kept for the next frame's watchers.
    pub(crate) fn ui_said(&mut self, ev: Value) {
        if self.ui_watch.watching() {
            self.ui_watch.pending.push(ev);
        }
    }

    /// Every frame: the cards that went since the last one go to the gone ring, with how (said where they
    /// went, else just `gone`) — and to watchers as `card-gone`.
    fn cards_sync(&mut self) {
        let front = self.session.id;
        let all = || {
            std::iter::once(&self.session)
                .chain(&self.sessions)
                .flat_map(|l| l.parked.iter().map(move |o| (o, l.id)))
                .chain(self.offers.iter().map(|o| (o, front)))
                .map(|(o, l)| (o.id, o.title.as_str(), l))
        };
        let same = self.ui_cards_seen.len() == all().count()
            && all().all(|(id, ..)| self.ui_cards_seen.iter().any(|s| s.0 == id));
        if same {
            return;
        }
        let now: Vec<(u64, String, crate::event::SessionId)> =
            all().map(|(id, t, l)| (id, t.to_string(), l)).collect();
        let went: Vec<(u64, String, crate::event::SessionId)> =
            self.ui_cards_seen.iter().filter(|s| !now.iter().any(|n| n.0 == s.0)).cloned().collect();
        self.ui_cards_seen = now;
        for (id, title, session) in went {
            let how = self.ui_card_how.remove(&id).unwrap_or_else(|| "gone".into());
            let said = format!("card#{id} went: {how} — {title}");
            self.ui_said(
                json!({ "event": "card-gone", "id": format!("card#{id}"), "how": how, "said": said }),
            );
            self.ui_gone.push_back(Gone {
                id: format!("card#{id}"),
                title,
                how,
                at: std::time::Instant::now(),
                session: super::session_id(session),
            });
            if self.ui_gone.len() > GONE_KEPT {
                self.ui_gone.pop_front();
            }
        }
        // (a card's how-it-went note outlives no card: one that came and went between two frames is let go)
        let ids: Vec<u64> = self.ui_cards_seen.iter().map(|s| s.0).collect();
        self.ui_card_how.retain(|id, _| ids.contains(id));
    }

    /// A connection watches (`uisocket.rs`): from the frame after this on, it's told what changes.
    pub fn ui_watch_add(&mut self, w: Watcher) {
        if self.ui_watch.watchers.is_empty() {
            self.ui_watch.last = Some(summary(&self.ui_state()));
        }
        self.ui_watch.watchers.push(w);
        crate::trace::watched(true);
    }
}

/// A turn of session `ed.session`'s Claude began or ended — for watchers.
pub fn turn(ed: &mut Editor, what: &str, calls: usize, took: Option<std::time::Duration>) {
    if !ed.ui_watch.watching() {
        return;
    }
    let (id, name) = (super::session_id(ed.session.id), ed.session.label().to_string());
    let said = match (what, took) {
        ("ended", Some(t)) => {
            format!("{id} {name}: Claude's turn ended — {calls} calls, {:.1}s", t.as_secs_f32())
        }
        _ => format!("{id} {name}: Claude's turn {what}"),
    };
    let took = took.map(|t| t.as_millis() as u64);
    ed.ui_said(json!({ "event": "turn", "session": id, "what": what, "calls": calls, "took_ms": took,
                       "said": said }));
}

/// Session `ed.session`'s usage changed — a turn's result came (`turn`), its cache went cold (`cold`), or
/// its Claude compacted the conversation (`compacted`: the context dropped) — for watchers. Its own kind,
/// said where it happens rather than diffed frame to frame: the seconds left change every frame, and a
/// watcher of what sessions are doing needn't hear what they cost.
pub fn usage(ed: &mut Editor, what: &str) {
    if !ed.ui_watch.watching() {
        return;
    }
    let Some(u) = super::UsageState::of(&ed.session.usage, crate::usage::now_ms()) else { return };
    let (id, name) = (super::session_id(ed.session.id), ed.session.label().to_string());
    let said = match (what, &ed.session.compacted.last) {
        ("cold", _) => format!("{id} {name}: Claude's cache went cold"),
        ("compacted", Some(k)) => format!("{id} {name}: Claude compacted its conversation ({})", k.how()),
        _ => format!("{id} {name}: {}", u.text()),
    };
    let mut ev = u.json();
    ev["event"] = json!("usage");
    ev["session"] = json!(id);
    ev["what"] = json!(what);
    ev["said"] = json!(said);
    ed.ui_said(ev);
}

/// After each frame (`uistate::after_frame`): the gone ring; then, while someone watches, what changed.
pub fn after_frame(ed: &mut Editor) {
    ed.cards_sync();
    if !ed.ui_watch.watching() {
        return;
    }
    let state = ed.ui_state();
    let now = summary(&state);
    let mut events = match ed.ui_watch.last.as_ref() {
        Some(old) => diff(old, &now),
        None => Vec::new(),
    };
    ed.ui_watch.last = Some(now);
    // (what was said where it happened goes first — a card's going before what's up after it)
    let mut said = std::mem::take(&mut ed.ui_watch.pending);
    for s in crate::trace::steps_taken() {
        let took = s.since.as_millis() as u64;
        said.push(json!({
            "event": "run", "id": format!("run#{}", s.id), "what": s.what, "step": s.step, "ended": s.ended,
            "since_ms": took, "said": format!("run#{} {} — {}", s.id, s.what, s.step),
        }));
    }
    said.append(&mut events);
    if said.is_empty() {
        return;
    }
    for ev in &mut said {
        ev["frame"] = json!(state.frame);
        ev["input"] = json!(state.input);
    }
    ed.ui_watch.watchers.retain(|w| {
        for ev in &said {
            let kind = kind_of(ev["event"].as_str().unwrap_or_default());
            if !w.kinds.is_empty() && !w.kinds.iter().any(|k| k == kind) {
                continue;
            }
            match w.tx.try_send(ev.to_string()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    w.overflowed.store(true, Ordering::Relaxed);
                    return false;
                }
                Err(TrySendError::Disconnected(_)) => return false,
            }
        }
        true
    });
    if !ed.ui_watch.watching() {
        ed.ui_watch.last = None;
        crate::trace::watched(false);
    }
}

/// The gone ring's newest, for the state (`gone`).
pub fn recent(gone: &VecDeque<Gone>, n: usize) -> Vec<Gone> {
    gone.iter().rev().take(n).cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #195: a session's reviewer starting and ending to read is a `session` event, its chat's `doing` as it
    /// was — `s0 author: idle → idle · reviewing`.
    #[test]
    fn a_reviewer_reading_is_a_session_event() {
        let at = |reviewer| Summary {
            sessions: vec![("s0".into(), "author".into(), "idle".into(), reviewer)],
            ..Summary::default()
        };
        let e = diff(&at(None), &at(Some("reviewing")));
        assert_eq!(e.len(), 1, "{e:?}");
        assert_eq!(
            (&e[0]["doing"], &e[0]["was"], &e[0]["reviewer"]),
            (&json!("idle"), &json!("idle"), &json!("reviewing"))
        );
        assert_eq!(e[0]["said"], "s0 author: idle → idle · reviewing");
        let e = diff(&at(Some("reviewing")), &at(None));
        assert_eq!(e[0]["said"], "s0 author: idle · reviewing → idle");
        assert!(diff(&at(Some("reviewing")), &at(Some("reviewing"))).is_empty());
    }
}
