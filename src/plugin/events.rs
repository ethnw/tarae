//! Events: plugins that subscribed are told what happens to buffers — opened, saved, closed, changed, shown
//! (now in front), and about to be saved. Never from the key or render path: an event only queues work on the plugin thread.
//!
//! - **One after another**: the subscribers of an event are called in turn, each on the buffer as the one
//!   before left it (its answer applied), so two plugins' edits don't refuse each other.
//! - **`changed`** waits for input to stop (`CHANGED_IDLE`), goes out at most once per buffer at a time, and
//!   not for a change a plugin made itself — no plugin and edit keep calling each other.
//! - **`before-save`** holds `:w` (and `:wq`, `:wa`) until the answers are in — up to `Events::wait`, then
//!   the file is saved as it is and late answers are dropped. Auto-save doesn't wait for plugins.
//! - **`closed`** goes to every subscriber at once, with the buffer as it was; there's nothing left to edit.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use super::abi::Event;
use super::host::Call;
use super::shown;
use crate::document::DocId;
use crate::editor::Editor;

/// How long `changed` waits for input to stop.
const CHANGED_IDLE: Duration = Duration::from_millis(300);

/// What runs once every subscriber of an event has answered.
type Then = Box<dyn FnOnce(&mut Editor) + Send>;

/// The editor's event bookkeeping.
pub(super) struct Events {
    /// (plugin, event) for every subscription, in plugin order.
    /// (plugin, event, the languages it's about — none: every buffer)
    pub subs: Vec<(String, Event, Vec<String>)>,
    /// `changed`: the idle timer's generation; per buffer, the version last reported, the version a
    /// plugin's own edit left (not reported back to anyone), and whether a report is out.
    idle: u64,
    reported: HashMap<DocId, u64>,
    pub by_plugin: HashMap<DocId, u64>,
    out: HashSet<DocId>,
    /// `before-save`: buffers waiting for their plugins (the save's number, and whether it's `:wq`), the
    /// buffers whose plugins are done (their next save goes straight on), and the last save's number.
    saving: HashMap<DocId, (u64, bool)>,
    presaved: HashSet<DocId>,
    saves: u64,
    /// How long a save waits for `before-save` answers.
    pub wait: Duration,
    /// The buffer in front, as `shown` last said it.
    front: Option<DocId>,
}

impl Default for Events {
    fn default() -> Events {
        Events {
            subs: Vec::new(),
            idle: 0,
            reported: HashMap::new(),
            by_plugin: HashMap::new(),
            out: HashSet::new(),
            saving: HashMap::new(),
            presaved: HashSet::new(),
            saves: 0,
            wait: Duration::from_secs(2),
            front: None,
        }
    }
}

impl Events {
    /// Whether a buffer was edited, by a person, since `changed` last reported it (never reported: since
    /// it was opened or saved).
    fn edited(&self, d: &crate::document::Document) -> bool {
        let v = d.version();
        let since = match self.reported.get(&d.id) {
            Some(&r) => r != v,
            None => d.is_modified(),
        };
        since && self.by_plugin.get(&d.id) != Some(&v)
    }
}

impl Editor {
    /// The plugins that want `e` about buffer `doc` (its language one they asked for, if they did).
    fn subscribers(&self, e: Event, doc: DocId) -> VecDeque<String> {
        let Some(p) = &self.plugins else { return VecDeque::new() };
        let lang = self.docs.iter().find(|d| d.id == doc).and_then(|d| d.lang_name());
        p.events
            .subs
            .iter()
            .filter(|(_, s, langs)| {
                *s == e && (langs.is_empty() || lang.is_some_and(|l| langs.iter().any(|x| x == l)))
            })
            .map(|(plugin, ..)| plugin.clone())
            .collect()
    }

    /// `shown`, when the buffer in front isn't the one it was (after every event: switching, a window's
    /// focus, opening — all of them).
    pub fn plugin_front(&mut self) {
        let Some(p) = &mut self.plugins else { return };
        let doc = self.docs.get(self.current).map(|d| d.id);
        if p.events.front == doc {
            return;
        }
        p.events.front = doc;
        if let Some(doc) = doc {
            self.plugin_event(Event::Shown, doc);
        }
    }

    /// Tells the subscribers of `e` about buffer `doc` (opened, saved).
    pub fn plugin_event(&mut self, e: Event, doc: DocId) {
        let subs = self.subscribers(e, doc);
        if !subs.is_empty() {
            self.plugin_chain(e, doc, subs, None, None);
        }
    }

    /// Calls the next plugin in `rest` about `doc`, applies its answer, and goes on with the others — then
    /// `then`. A `before-save` chain stops if its save (`save`) has already gone on without it.
    fn plugin_chain(
        &mut self,
        e: Event,
        doc: DocId,
        mut rest: VecDeque<String>,
        save: Option<u64>,
        then: Option<Then>,
    ) {
        let Some(plugin) = rest.pop_front() else {
            if let Some(then) = then {
                then(self);
            }
            return;
        };
        let Some((input, seen)) = self.plugin_input(doc, "") else {
            // The buffer is gone: nothing more to tell
            if let Some(then) = then {
                then(self);
            }
            return;
        };
        let events = self.events.sender();
        let who = plugin.clone();
        let reply = Box::new(move |result: Result<Vec<super::Action>, String>| {
            let apply = move |ed: &mut Editor| {
                if let Some(n) = save
                    && ed.plugins.as_ref().and_then(|p| p.events.saving.get(&doc)).map(|s| s.0) != Some(n)
                {
                    return; // saved without waiting any longer
                }
                match result {
                    Ok(actions) => {
                        // Only I/O asked for, nothing said: not drawn (a buffer brought to the front, again)
                        let quiet = actions.iter().all(super::Action::unseen);
                        let said = (ed.status.clone(), ed.toasts.len());
                        ed.plugin_apply(&who, &seen, actions);
                        if quiet && said == (ed.status.clone(), ed.toasts.len()) {
                            ed.unchanged = true;
                        }
                    }
                    Err(err) => ed.set_error(format!("{who}: {}", shown(&err))),
                }
                ed.plugin_chain(e, doc, rest, save, then);
            };
            let _ = events.send(crate::event::Event::Job(Box::new(apply)));
        });
        let sent = self.plugins.as_ref().map(|p| p.host.call(&plugin, Call::Event(e), input, reply));
        if let Some(Err(err)) = sent {
            self.set_error(format!("{plugin}: {err}"));
        }
    }

    /// Buffer `doc` is about to close: its subscribers see it as it is, all at once.
    pub fn plugin_closing(&mut self, doc: DocId) {
        // A buffer a plugin showed: it's told at once (so it stops following a log)
        let views: Vec<(String, u64)> = self
            .plugins
            .as_ref()
            .map(|p| p.views.iter().filter(|(_, d)| **d == doc).map(|(k, _)| k.clone()).collect())
            .unwrap_or_default();
        for (plugin, id) in views {
            if let Some(p) = &mut self.plugins {
                p.views.remove(&(plugin.clone(), id));
            }
            self.plugin_events(
                &plugin,
                vec![super::abi::HostEvent::Failed(id, "its buffer is closed".into())],
            );
        }
        let subs = self.subscribers(Event::Closed, doc);
        if let Some(p) = &mut self.plugins {
            p.status.retain(|(_, d), _| *d != doc);
            let ev = &mut p.events;
            ev.reported.remove(&doc);
            ev.by_plugin.remove(&doc);
            ev.out.remove(&doc);
        }
        let Some((input, seen)) = self.plugin_input(doc, "") else { return };
        for plugin in subs {
            let events = self.events.sender();
            let (who, seen) = (plugin.clone(), seen.clone());
            let reply = Box::new(move |result: Result<Vec<super::Action>, String>| {
                let apply = move |ed: &mut Editor| match result {
                    Ok(actions) => ed.plugin_apply(&who, &seen, actions),
                    Err(err) => ed.set_error(format!("{who}: {}", shown(&err))),
                };
                let _ = events.send(crate::event::Event::Job(Box::new(apply)));
            });
            if let Some(p) = &self.plugins {
                let _ = p.host.call(&plugin, Call::Event(Event::Closed), input.clone(), reply);
            }
        }
    }

    /// A key (or paste) went in: if a buffer changed, `changed` goes out once input stops.
    pub fn plugin_input_went_in(&mut self) {
        let Some(p) = &mut self.plugins else { return };
        if !p.events.subs.iter().any(|(_, e, _)| *e == Event::Changed) {
            return;
        }
        let ev = &p.events;
        let moved = self.docs.iter().any(|d| ev.edited(d));
        if !moved {
            return;
        }
        let p = self.plugins.as_mut().expect("checked above");
        p.events.idle += 1;
        let idle = p.events.idle;
        self.events.jobs().spawn(move || {
            std::thread::sleep(CHANGED_IDLE);
            move |ed: &mut Editor| {
                if ed.plugins.as_ref().is_some_and(|p| p.events.idle == idle) {
                    ed.plugin_changed_now();
                }
            }
        });
    }

    /// Input stopped: `changed` for every buffer edited since it was last reported (by a person, not a
    /// plugin), unless one is still out.
    fn plugin_changed_now(&mut self) {
        let Some(p) = &mut self.plugins else { return };
        let mut due = Vec::new();
        for d in &self.docs {
            let ev = &mut p.events;
            if d.loading || ev.out.contains(&d.id) || !ev.edited(d) {
                continue;
            }
            ev.out.insert(d.id);
            ev.reported.insert(d.id, d.version());
            due.push(d.id);
        }
        for doc in due {
            let subs = self.subscribers(Event::Changed, doc);
            let then: Then = Box::new(move |ed: &mut Editor| {
                if let Some(p) = &mut ed.plugins {
                    p.events.out.remove(&doc);
                }
                // Edited again while it was out: another round once input stops
                ed.plugin_input_went_in();
            });
            self.plugin_chain(Event::Changed, doc, subs, None, Some(then));
        }
    }

    /// `:w` and friends, before anything else: if plugins want `before-save`, ask them and save once they've
    /// answered (true — the caller doesn't save). A buffer they're done with goes straight on (false).
    pub(crate) fn plugins_before_save(&mut self, doc: DocId, quit: bool) -> bool {
        let subs = self.subscribers(Event::BeforeSave, doc);
        let Some(p) = &mut self.plugins else { return false };
        if p.events.presaved.remove(&doc) || subs.is_empty() {
            return false;
        }
        let Some(d) = self.docs.iter().find(|d| d.id == doc) else { return false };
        if d.loading || d.path.is_none() {
            return false;
        }
        let p = self.plugins.as_mut().expect("checked above");
        p.events.saves += 1;
        let (n, wait) = (p.events.saves, p.events.wait);
        p.events.saving.insert(doc, (n, quit));
        self.note("plugins before saving…");
        let then: Then = Box::new(move |ed: &mut Editor| ed.plugins_saved_before(doc, n, None));
        self.plugin_chain(Event::BeforeSave, doc, subs, Some(n), Some(then));
        self.events.jobs().spawn(move || {
            std::thread::sleep(wait);
            move |ed: &mut Editor| {
                ed.plugins_saved_before(doc, n, Some("the plugins took too long before saving — saved as is"))
            }
        });
        true
    }

    /// The `before-save` answers are in (or `why` not): on with the save — formatting, then writing.
    fn plugins_saved_before(&mut self, doc: DocId, n: u64, why: Option<&str>) {
        let Some(p) = &mut self.plugins else { return };
        // Already on its way (the answers came, or the wait ran out) — or a later save's
        let Some(&(m, quit)) = p.events.saving.get(&doc) else { return };
        if m != n {
            return;
        }
        p.events.saving.remove(&doc);
        p.events.presaved.insert(doc);
        if let Some(why) = why {
            self.set_warning(why.to_string());
        }
        if !self.format_then_save_doc(doc, quit) {
            self.format_save_done(doc, quit, None);
        }
    }

    /// Whether a save is waiting for `before-save` answers.
    pub(crate) fn plugins_saving(&self) -> bool {
        self.plugins.as_ref().is_some_and(|p| !p.events.saving.is_empty())
    }

    /// What plugins put in the status line for the buffer in front, in plugin order.
    pub fn plugin_status(&self) -> Vec<&str> {
        let Some(p) = &self.plugins else { return Vec::new() };
        let doc = self.doc().id;
        p.status.iter().filter(|((_, d), _)| *d == doc).map(|(_, text)| text.as_str()).collect()
    }
}
