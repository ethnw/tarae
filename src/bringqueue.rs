//! The bring queue (frictions29 24 a, as frictions30's plan decided): several sessions ready at once, brought into
//! their base one after another on its tip — each rebased onto it when it moved (`rebase_check`, as the rebase card's
//! `y`), its quick preflight run there, then brought (fast-forward only, as the bring card's `y`); the next starts
//! once one lands. One `y` for the queue (`What::BringQueue`). A conflict, a failed or declined preflight, changes
//! not in a commit, a fast-forward refused twice — that one is out, said, and the rest go on; its own card (the
//! rebase's, the held one's) stays as a single bring has it.
//!
//! `space W`: `m` marks the row in hand (again: unmarks) — the order marked is the order brought — and `B` asks to
//! bring the marked ones. The queue is the editor's, not a session's: one at a time, whatever's in front.

use std::collections::VecDeque;

use crate::editor::Editor;
use crate::event::SessionId;

/// What `space W` marked and the queue going, if one is.
#[derive(Default)]
pub struct Queue {
    /// Marked to bring, in the order marked.
    pub marks: Vec<SessionId>,
    pub run: Option<Run>,
    /// Tests: the rebase and the preflight it would start as tasks are left to a simulation (which models no
    /// task) — it says how each ended (`bring_queue_rebased`, `rebase_preflight_done`).
    #[cfg(test)]
    pub stand_in: bool,
}

/// A queue going: those still to come, the one under way and where it is, and what became of the rest.
pub struct Run {
    pub left: VecDeque<SessionId>,
    pub now: Option<(SessionId, Stage)>,
    /// Brought: (name, what it said — `3 commits, now at 1a2b3c4`).
    pub brought: Vec<(String, String)>,
    /// Not brought: (name, why).
    pub out: Vec<(String, String)>,
    /// The one under way was refused its fast-forward once (its base moved under it): looked at again.
    pub again: bool,
    pub total: usize,
    /// Each one's name as the card said it — one whose session ended is still said by it.
    pub names: Vec<(SessionId, String)>,
}

/// Where the one under way is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Its checkout looked at (on a worker): moved, ready, a rebase there.
    Looking,
    /// Rebased onto its base's tip, in its task panel.
    Rebasing,
    /// Its quick preflight runs (or waits on its card).
    Preflight,
    /// Brought in (on a worker).
    Bringing,
}

impl Stage {
    fn said(self) -> &'static str {
        match self {
            Stage::Looking => "looking",
            Stage::Rebasing => "rebasing",
            Stage::Preflight => "preflight",
            Stage::Bringing => "bringing",
        }
    }
}

/// What the card asks about: the sessions, in the order they come.
#[derive(Clone, Debug)]
pub struct Ask {
    pub ids: Vec<SessionId>,
    pub names: Vec<String>,
    /// Their bases, each once (`main`).
    pub bases: Vec<String>,
}

/// `1st`, `2nd`, `3rd`, `4th` …
fn nth(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// The card for `ask`.
pub fn card(ask: Ask) -> crate::offer::Offer {
    let n = ask.names.len();
    let base = match ask.bases.as_slice() {
        [one] => one.clone(),
        _ => "their bases".into(),
    };
    let tip = match ask.bases.as_slice() {
        [one] => format!("{one}'s tip"),
        _ => "its base's tip".into(),
    };
    // (in the order they come: `a, b, then c`)
    let order = match ask.names.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{}, then {last}", rest.join(", ")),
        None => String::new(),
    };
    let q = format!(
        "Bring {n} sessions into {base}, one after another? {order}.\n\nEach in turn: rebased onto {tip} if it \
         moved, its quick preflight run there, then brought — the next starts once it lands. A conflict or a \
         failed preflight stops only that one; the rest go on.",
    );
    crate::offer::Offer::new(
        format!("Bring {n} into {base}"),
        &q,
        "Bringing them in turn…",
        "Couldn't start the bring queue".into(),
        crate::offer::What::BringQueue(Box::new(ask)),
    )
}

impl Editor {
    /// `m` in the list: session `id` marked to bring (again: unmarked) — said, with what `B` does.
    pub(crate) fn bring_mark(&mut self, id: SessionId) {
        if id == 0 {
            return self.set_status("that's the checkout tarae started in — sessions are brought into it");
        }
        let name = self.bring_name(id);
        let marks = &mut self.bring_queue.marks;
        match marks.iter().position(|m| *m == id) {
            Some(i) => {
                marks.remove(i);
            }
            None => marks.push(id),
        }
        let n = marks.len();
        let said = match (marks.contains(&id), n) {
            (false, 0) => format!("{name} unmarked — nothing marked to bring"),
            (false, _) => format!("{name} unmarked — {n} marked: B brings them in turn"),
            (true, 1) => format!("{name} marked to bring — m on another, then B brings them in turn"),
            (true, _) => format!("{name} marked to bring {} — B brings the {n} in turn", nth(n)),
        };
        self.set_status(said);
        // (its row says its place at once)
        self.session_list_sync();
    }

    /// `B` in the list: the marked ones asked about on one card — one marked is a bring of its own.
    pub(crate) fn bring_queue_ask(&mut self) -> Result<(), String> {
        if self.bring_queue.run.is_some() {
            return Err("a bring queue is under way — the list says where".into());
        }
        let live =
            |ed: &Editor, id: SessionId| std::iter::once(&ed.session).chain(&ed.sessions).any(|l| l.id == id);
        let marks: Vec<SessionId> =
            self.bring_queue.marks.iter().copied().filter(|id| live(self, *id)).collect();
        self.bring_queue.marks.clone_from(&marks);
        match marks.as_slice() {
            [] => Err("nothing marked — m marks the sessions to bring in turn, then B".into()),
            [one] => {
                self.bring_queue.marks.clear();
                self.session_bring(&one.to_string())
            }
            _ => {
                let start = self.start_root();
                let all = std::iter::once(&self.session).chain(&self.sessions);
                let mut bases: Vec<String> = Vec::new();
                let mut names = Vec::new();
                for id in &marks {
                    let Some(l) = all.clone().find(|l| l.id == *id) else { continue };
                    names.push(l.label().to_string());
                    let base = l.base.clone().unwrap_or_else(|| start_branch(self, &start));
                    if !bases.contains(&base) {
                        bases.push(base);
                    }
                }
                self.push_offer(card(Ask { ids: marks, names, bases }));
                Ok(())
            }
        }
    }

    /// `y` on the card: the queue starts, the marks it took cleared.
    pub(crate) fn bring_queue_start(&mut self, ask: Ask) {
        if self.bring_queue.run.is_some() {
            return self.set_error("a bring queue is under way — one at a time");
        }
        self.bring_queue.marks.retain(|m| !ask.ids.contains(m));
        let total = ask.ids.len();
        let names = ask.ids.iter().copied().zip(ask.names.iter().cloned()).collect();
        let left = ask.ids.into_iter().collect();
        let run = Run { left, now: None, brought: Vec::new(), out: Vec::new(), again: false, total, names };
        self.bring_queue.run = Some(run);
        crate::log::line("session", "bring-queue", &format!("started: {}", ask.names.join(", ")));
        self.bring_queue_next();
    }

    /// The next one looked at — or, none left, the queue over and said.
    fn bring_queue_next(&mut self) {
        loop {
            let Some(run) = self.bring_queue.run.as_mut() else { return };
            let Some(id) = run.left.pop_front() else { return self.bring_queue_end() };
            run.now = Some((id, Stage::Looking));
            run.again = false;
            match self.bringing_queued(id) {
                Ok(()) => return,
                Err(e) => {
                    let name = self.bring_name(id);
                    if let Some(run) = self.bring_queue.run.as_mut() {
                        run.out.push((name, e));
                        run.now = None;
                    }
                }
            }
        }
    }

    /// Every one gone through: said — a success when each was brought, else a warning naming who wasn't.
    fn bring_queue_end(&mut self) {
        let Some(run) = self.bring_queue.run.take() else { return };
        let brought: Vec<&str> = run.brought.iter().map(|(n, _)| n.as_str()).collect();
        let out: Vec<String> = run.out.iter().map(|(n, why)| format!("{n} ({why})")).collect();
        crate::log::line("session", "bring-queue", &format!("over: brought {brought:?}, not {out:?}"));
        let brought_said = match brought.as_slice() {
            [] => "none brought".to_string(),
            b => format!("brought {}", b.join(", ")),
        };
        match out.is_empty() {
            true => self.set_success(format!("Bring queue done — {brought_said}")),
            false => self
                .set_warning(format!("Bring queue done — {brought_said} · not brought: {}", out.join("; "))),
        }
    }

    /// The one under way (`id`) is out of the queue — `why` said — and the next goes on.
    pub(crate) fn bring_queue_out(&mut self, id: SessionId, why: String) {
        let Some(run) = self.bring_queue.run.as_mut().filter(|r| r.now.is_some_and(|(n, _)| n == id)) else {
            return;
        };
        run.now = None;
        let name = self.bring_name(id);
        let left = self.bring_queue.run.as_ref().map_or(0, |r| r.left.len());
        let next = if left == 0 { String::new() } else { " — the next goes on".into() };
        self.set_warning(format!("Bring queue: {name} not brought — {why}{next}"));
        if let Some(run) = self.bring_queue.run.as_mut() {
            run.out.push((name, why));
        }
        self.bring_queue_next();
    }

    /// The stage session `id` is at in the queue, if it's the one under way.
    pub(crate) fn bring_queue_at(&self, id: SessionId) -> Option<Stage> {
        self.bring_queue.run.as_ref()?.now.filter(|(n, _)| *n == id).map(|(_, s)| s)
    }

    fn bring_queue_stage(&mut self, id: SessionId, stage: Stage) {
        if let Some(run) = self.bring_queue.run.as_mut()
            && let Some(now) = run.now.as_mut().filter(|(n, _)| *n == id)
        {
            now.1 = stage;
        }
    }

    /// Whether session `id` is in the queue going (under way or still to come).
    pub(crate) fn bring_queued(&self, id: SessionId) -> bool {
        self.bring_queue
            .run
            .as_ref()
            .is_some_and(|r| r.left.contains(&id) || r.now.is_some_and(|(n, _)| n == id))
    }

    /// What the list says of session `id`: marked (`bring 2nd`), or where it is in the queue going.
    pub(crate) fn bring_said(&self, id: SessionId) -> Option<String> {
        if let Some(run) = self.bring_queue.run.as_ref() {
            if let Some((_, stage)) = run.now.filter(|(n, _)| *n == id) {
                let at = run.brought.len() + run.out.len() + 1;
                return Some(format!("bring queue {at}/{} · {}", run.total, stage.said()));
            }
            if let Some(i) = run.left.iter().position(|n| *n == id) {
                return Some(match i {
                    0 => "bring queue · next".to_string(),
                    i => format!("bring queue · {}", nth(i + 1)),
                });
            }
        }
        let i = self.bring_queue.marks.iter().position(|m| *m == id)?;
        Some(format!("bring {}", nth(i + 1)))
    }

    fn bring_queue_stands_in(&self) -> bool {
        #[cfg(test)]
        return self.bring_queue.stand_in;
        #[cfg(not(test))]
        false
    }

    fn bring_name(&self, id: SessionId) -> String {
        let all = std::iter::once(&self.session).chain(&self.sessions);
        let queued = || {
            let run = self.bring_queue.run.as_ref()?;
            run.names.iter().find(|(n, _)| *n == id).map(|(_, name)| name.clone())
        };
        match all.into_iter().find(|l| l.id == id) {
            Some(l) => l.label().to_string(),
            None => queued().unwrap_or_else(|| format!("session {id}")),
        }
    }

    // ── What the bring's steps say back ──

    /// The checkout of the one under way looked at (`bringing_queued`'s worker): what comes next for it.
    pub(crate) fn bring_queue_looked(&mut self, id: SessionId, found: crate::worktree::Bringing) {
        use crate::worktree::Bringing;
        if self.bring_queue_at(id) != Some(Stage::Looking) {
            return;
        }
        match found {
            // (its base moved: rebased onto its tip in its own task panel — the rebase card's `y`, unasked)
            Bringing::Moved(m) => {
                self.bring_queue_stage(id, Stage::Rebasing);
                if !self.bring_queue_stands_in() {
                    self.with_session(id, |ed| ed.rebase_check(m, false));
                }
            }
            Bringing::Ready(m) if m.ahead == 0 => {
                self.bring_queue_out(id, format!("nothing to bring — no commits past {}", m.base));
            }
            Bringing::Ready(_) => {
                self.bring_queue_stage(id, Stage::Preflight);
                let mut failed = None;
                let stands_in = self.bring_queue_stands_in();
                self.with_session(id, |ed| {
                    ed.session.bring_after_preflight = true;
                    if stands_in {
                        return;
                    }
                    if let Err(e) = ed.preflight_start(crate::preflight::Set::Quick) {
                        ed.session.bring_after_preflight = false;
                        failed = Some(e);
                    }
                });
                if let Some(e) = failed {
                    self.bring_queue_out(id, format!("its preflight didn't start: {e}"));
                }
            }
            // (one tarae didn't start: taken up, its card up — as a single bring has it)
            Bringing::Rebasing(stop) => {
                self.with_session(id, |ed| {
                    if ed.session.rebase.is_none() {
                        ed.rebase_adopt(stop);
                    }
                });
                let _ = self.rebase_offer(id);
                self.bring_queue_out(id, "a rebase is under way there — its card".into());
            }
            Bringing::Refused(e) => self.bring_queue_out(id, e),
            Bringing::Brought(_) => {}
        }
    }

    /// Session `id` brought in (`said`: its commits and tip): the next goes on.
    pub(crate) fn bring_queue_landed(&mut self, id: SessionId, said: String) {
        if self.bring_queue_at(id).is_none() {
            return;
        }
        let name = self.bring_name(id);
        if let Some(run) = self.bring_queue.run.as_mut() {
            run.now = None;
            run.brought.push((name, said));
        }
        self.bring_queue_next();
    }

    /// Session `id`'s bring after its preflight didn't land (`found`): its base moved during the preflight
    /// (`Moved` — the check before the fast-forward) or under the fast-forward itself (`Refused`) — looked at once
    /// more, rebased again then; a rebase found there (its card up), or the base moved again, out.
    pub(crate) fn bring_queue_not_brought(&mut self, id: SessionId, found: crate::worktree::Bringing) {
        use crate::worktree::Bringing;
        if self.bring_queue_at(id) != Some(Stage::Bringing) {
            return;
        }
        let why = match found {
            Bringing::Brought(_) => return,
            Bringing::Moved(m) => format!("{} moved again while it was checked", m.base),
            Bringing::Refused(e) => e,
            Bringing::Ready(m) => format!("not brought into {}", m.base),
            Bringing::Rebasing(stop) => {
                self.with_session(id, |ed| {
                    if ed.session.rebase.is_none() {
                        ed.rebase_adopt(stop);
                    }
                });
                let _ = self.rebase_offer(id);
                return self.bring_queue_out(id, "a rebase is under way there — its card".into());
            }
        };
        let again = self.bring_queue.run.as_mut().is_some_and(|r| !std::mem::replace(&mut r.again, true));
        if again {
            self.bring_queue_stage(id, Stage::Looking);
            match self.bringing_queued(id) {
                Ok(()) => return,
                Err(e) => return self.bring_queue_out(id, e),
            }
        }
        self.bring_queue_out(id, why);
    }

    /// The front session's rebase is over (`done`: rebased onto its base's tip; else why not): the one under way
    /// looked at again (its preflight next), or out. True when the queue took it — no bring card then.
    pub(crate) fn bring_queue_rebased(&mut self, done: Result<(), String>) -> bool {
        let id = self.session.id;
        if self.bring_queue_at(id) != Some(Stage::Rebasing) {
            return false;
        }
        match done {
            Ok(()) => {
                self.bring_queue_stage(id, Stage::Looking);
                if let Err(e) = self.bringing_queued(id) {
                    self.bring_queue_out(id, e);
                }
            }
            Err(why) => self.bring_queue_out(id, why),
        }
        true
    }

    /// The front session's preflight for a bring ended (`passed`): brought next, or out.
    pub(crate) fn bring_queue_preflight(&mut self, passed: bool, title: &str) {
        let id = self.session.id;
        if self.bring_queue_at(id) != Some(Stage::Preflight) {
            return;
        }
        match passed {
            true => self.bring_queue_stage(id, Stage::Bringing),
            false => self.bring_queue_out(id, format!("its {title} didn't pass")),
        }
    }

    /// Every event: the one under way whose session ended, or whose preflight was dropped on the way (its card
    /// declined, it didn't start — `bring_after_preflight` cleared with no end said), is out.
    pub(crate) fn bring_queue_sync(&mut self) {
        let Some((id, stage)) = self.bring_queue.run.as_ref().and_then(|r| r.now) else { return };
        let session = std::iter::once(&self.session).chain(&self.sessions).find(|l| l.id == id);
        match session {
            None => self.bring_queue_out(id, "its session ended".into()),
            Some(l) if stage == Stage::Preflight && !l.bring_after_preflight => {
                self.bring_queue_out(id, "its preflight didn't run".into())
            }
            Some(_) => {}
        }
    }
}

/// The branch of the checkout tarae started in (a session's base when it names none).
fn start_branch(ed: &Editor, start: &std::path::Path) -> String {
    let main = std::iter::once(&ed.session).chain(&ed.sessions).find(|l| l.root == start);
    main.and_then(|l| l.git_branch.clone()).unwrap_or_else(|| "its base".into())
}
