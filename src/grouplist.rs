//! A linked group's list (panes P3 §2.1, `pane::Group`): one handler for every kind's. It moves as every list does
//! (`key::list_move`, the list's measures — its selection drives the detail beside it), `enter` is the kind's act,
//! `tab`/`S-tab` the next/previous member (then the slot's next tab — `slot_step`), `q` the group's one close (a
//! run's only once it has ended — `Editor::slot_close`), `esc` back to the code; `space`, `:` and `C-w` are the
//! editor's, anything else is said. The kind supplies its rows, its `enter`, the link and its words: the tests'
//! (`testing.rs`), the task's places and a preflight's steps (§2.2: the one output document beside them, scrolled to
//! the place's line or the step's heading), `:procs`' table (its detail what the row selected is — `procs::detail`;
//! its own `s` stop, `r` restart, `l` log, `enter` the detail); git and the review come with P3's step 5 (until then
//! neither is laid out, so neither takes keys).

use crate::editor::Editor;
use crate::key::{Code, Key, ListMove};
use crate::pane::{Group, GroupKind};
use crate::session::At;
use crate::sizes::Slot;

impl GroupKind {
    /// The slot's panel it's shown in while the slot hosts it.
    pub fn slot(self) -> Option<Slot> {
        match self {
            GroupKind::Tests => Some(Slot::Test),
            GroupKind::Task | GroupKind::Preflight => Some(Slot::Task),
            GroupKind::Procs => Some(Slot::Procs),
            GroupKind::Git | GroupKind::Review | GroupKind::Sessions => None,
        }
    }

    /// The list, in what's said about a key it has no use for.
    fn what(self) -> &'static str {
        match self {
            GroupKind::Tests => "the tests' list",
            GroupKind::Task => "the task's places",
            GroupKind::Preflight => "the preflight's steps",
            GroupKind::Procs => "the processes",
            GroupKind::Git => "the git tab",
            GroupKind::Review => "the review's files",
            GroupKind::Sessions => "the session list",
        }
    }
}

/// A kind's list as the keys move it: its rows, the one selected, the first in sight and the rows in sight (as
/// last drawn).
struct List {
    n: usize,
    selected: usize,
    top: usize,
    rows: usize,
}

impl List {
    fn of(ed: &Editor, kind: GroupKind) -> Option<Self> {
        let rows = ed.screen.list(kind).map_or(0, |l| l.1);
        match kind {
            GroupKind::Tests => {
                let r = ed.session.test_run.as_ref()?;
                Some(List { n: r.cases.len(), selected: r.selected, top: r.list_top.get(), rows })
            }
            GroupKind::Task => {
                let r = ed.session.task_run.as_ref()?;
                let order = r.place_rows();
                let selected = order.iter().position(|&i| i == r.picked).unwrap_or(0);
                Some(List { n: order.len(), selected, top: r.list_top.get(), rows })
            }
            GroupKind::Preflight => {
                let p = preflight(ed)?;
                Some(List { n: p.steps.len(), selected: p.selected, top: p.list_top.get(), rows })
            }
            GroupKind::Procs => {
                let p = ed.procs.panel.as_ref()?;
                let rows = ed.screen.procs_panel.get().map_or(0, |a| a.shown);
                Some(List { n: ed.procs.rows.len(), selected: p.selected, top: p.scroll.get(), rows })
            }
            _ => None,
        }
    }

    /// The selection moved to row `selected`: kept, and the link — the detail follows (the tests': the failure
    /// picked, its own scroll again; the task's: the output at the place's line; a preflight's: at the step's
    /// heading).
    fn set(ed: &mut Editor, kind: GroupKind, selected: usize, top: usize) {
        match kind {
            GroupKind::Tests => {
                let Some(r) = ed.session.test_run.as_mut() else { return };
                r.list_top.set(top);
                if r.selected != selected {
                    r.scroll = None;
                }
                r.selected = selected;
            }
            GroupKind::Task => {
                let Some(r) = ed.session.task_run.as_mut() else { return };
                r.list_top.set(top);
                // (by identity: the place, whatever row the list's growing puts it on)
                let Some(&i) = r.place_rows().get(selected) else { return };
                if r.picked != i {
                    r.picked = i;
                    link(ed, kind);
                }
            }
            GroupKind::Preflight => {
                let Some(p) = ed.session.preflight.as_mut() else { return };
                p.list_top.set(top);
                if p.selected != selected && selected < p.steps.len() {
                    p.selected = selected;
                    link(ed, kind);
                }
            }
            // (the detail follows at once — `rundoc::sync`, every event; a gone row the selection leaves goes)
            GroupKind::Procs => {
                let Some(p) = ed.procs.panel.as_mut() else { return };
                let was = p.selected;
                let gone = ed.procs.rows.get(was).is_some_and(|r| r.gone.is_some());
                let (mut selected, mut top) = (selected, top);
                // (the rows below it a row up: the selection and the view stay on theirs)
                if gone && selected != was {
                    ed.procs.rows.remove(was);
                    selected -= usize::from(selected > was);
                    top -= usize::from(top > was);
                }
                p.scroll.set(top);
                p.selected = selected.min(ed.procs.rows.len().saturating_sub(1));
            }
            _ => {}
        }
    }
}

/// The preflight whose run the task panel shows.
pub fn preflight(ed: &Editor) -> Option<&crate::preflight::Preflight> {
    let r = ed.session.task_run.as_ref()?;
    ed.session.preflight.as_ref().filter(|p| p.generation == r.generation())
}

/// Kind `kind` has something to list beside its detail (else its host shows the detail alone, one pane): the tests'
/// results; the task's places, two or more; a preflight's steps, two or more.
pub fn listed(ed: &Editor, kind: GroupKind) -> bool {
    match kind {
        GroupKind::Tests => ed.session.test_run.as_ref().is_some_and(|r| !r.cases.is_empty()),
        GroupKind::Task => {
            preflight(ed).is_none() && ed.session.task_run.as_ref().is_some_and(|r| r.places.len() > 1)
        }
        GroupKind::Preflight => preflight(ed).is_some_and(|p| p.steps.len() > 1),
        GroupKind::Procs => ed.procs_up(),
        _ => false,
    }
}

/// The link (§2.1): the detail shows what the list's selection is — the task's output with the place's line in
/// sight and selected, a preflight's from the step's heading at the top. Nothing slow: the document is there.
fn link(ed: &mut Editor, kind: GroupKind) {
    match kind {
        GroupKind::Task => {
            let Some(r) = ed.session.task_run.as_ref() else { return };
            match r.origin(r.picked).line {
                Some(line) => crate::rundoc::show_line(ed, line, false),
                None => ed.set_status("that place's line was cut from the output — enter goes to the place"),
            }
        }
        GroupKind::Preflight => {
            let Some((p, r)) = preflight(ed).zip(ed.session.task_run.as_ref()) else { return };
            match p.heading_at(p.selected, &r.output) {
                Some(at) => crate::rundoc::show_line(ed, at, true),
                None => ed.set_status(format!("{} hasn't printed anything yet", p.steps[p.selected].name)),
            }
        }
        _ => {}
    }
}

/// A press on row `row` of kind `kind`'s list: the keys its, that row selected.
pub fn click(ed: &mut Editor, kind: GroupKind, row: usize) {
    ed.list_focus(kind);
    if let Some(list) = List::of(ed, kind).filter(|l| row < l.n) {
        List::set(ed, kind, row, list.top);
    }
}

/// The wheel over kind `kind`'s list: its view `by` rows, the selection staying unless it would leave it, then the
/// nearest — as `C-e`/`C-y` move it, through `List::set` (the link; `:procs`' gone row let go).
pub fn wheel(ed: &mut Editor, kind: GroupKind, by: isize) {
    let Some(l) = List::of(ed, kind) else { return };
    let (top, at) = crate::key::in_sight(by, l.top, l.selected, l.n, l.rows, 0);
    List::set(ed, kind, at, top);
}

/// `o` on a preflight's output beside its steps: the step in view (the cursor's) selected in the list, the keys
/// there — false when the panel isn't a preflight's group.
pub fn step_in_view(ed: &mut Editor) -> bool {
    if !ed.session.groups.iter().any(|g| g.kind == GroupKind::Preflight && ed.group_laid(g)) {
        return false;
    }
    let at = crate::rundoc::cursor_line(ed, Slot::Task);
    let k = preflight(ed).zip(ed.session.task_run.as_ref()).and_then(|(p, r)| p.step_at(at?, &r.output));
    let Some(k) = k else {
        ed.set_status("no step has printed anything yet");
        return true;
    };
    ed.list_focus(GroupKind::Preflight);
    if let Some(p) = ed.session.preflight.as_mut() {
        p.selected = k;
    }
    true
}

impl Editor {
    /// Group `g` is laid out — its host in sight with something to list (the tests': the slot showing a run with
    /// results); with nothing to list its host shows the detail alone, one pane.
    pub fn group_laid(&self, g: &Group) -> bool {
        match g.kind {
            GroupKind::Tests | GroupKind::Task | GroupKind::Preflight | GroupKind::Procs => {
                // (docked in sight: its own leaf; else the slot's, its panel the one in front there)
                let there = self.out_of_slot(g.kind)
                    || (g.kind.slot().is_some_and(|s| self.slot_shown() == Some(s))
                        && self.session.leaf(crate::pane::Shows::Slot).is_some());
                there && listed(self, g.kind)
            }
            _ => false,
        }
    }

    /// Every event: the task's and the preflight's groups, made in the slot's leaf once a run of theirs is there,
    /// `:procs`' once it's open (the tests' is the session's from its start) — their ids from `next_pane`, kept on a
    /// new run.
    pub fn groups_sync(&mut self) {
        if self.procs_up() {
            self.group_made(GroupKind::Procs);
        }
        let Some(r) = self.session.task_run.as_ref() else { return };
        let kind = match self.session.preflight.as_ref().is_some_and(|p| p.generation == r.generation()) {
            true => GroupKind::Preflight,
            false => GroupKind::Task,
        };
        self.group_made(kind);
    }

    /// Kind `kind`'s group in the slot's leaf, made if it isn't yet — None when the session has no slot.
    pub fn group_made(&mut self, kind: GroupKind) -> Option<Group> {
        if let Some(g) = self.session.group(kind) {
            return Some(g);
        }
        let host = self.session.leaf(crate::pane::Shows::Slot)?;
        let g = Group::new(kind, host, self.next_pane);
        self.session.groups.push(g);
        self.next_pane += Group::ids(kind);
        Some(g)
    }

    /// Every event: the member of a group that has the keys is its front (`Group.front`) — the one in sight while it's
    /// stacked, kept once the keys leave.
    pub fn groups_front(&mut self) {
        let detail = self.slot_group().filter(|_| self.session.is_at(At::Slot)).map(|g| g.kind);
        let at = self.session.docked();
        for g in &mut self.session.groups {
            if at == At::List(g.list) {
                g.front = crate::pane::Member::List;
            } else if detail == Some(g.kind) {
                g.front = crate::pane::Member::Detail;
            }
        }
    }

    /// The other member of a stacked group (§2.3: one member open, the other named in its line) when pane `id` is the
    /// one open in `leaves` (the last frame's) — and which way it lies as side by side (the list `-1`, left; the detail
    /// `1`). None while the group is side by side or `id` is no member.
    pub fn stacked_other(
        &self,
        id: crate::split::PaneId,
        leaves: &[(crate::split::PaneId, crate::split::Rect)],
    ) -> Option<(crate::split::PaneId, i32)> {
        let laid = |p: crate::split::PaneId| leaves.iter().any(|l| l.0 == p);
        self.session.groups.iter().filter(|g| self.group_laid(g)).find_map(|g| {
            let detail = g.detail_pane()?;
            let (other, way) = match id {
                _ if id == g.list => (detail, 1),
                _ if id == detail => (g.list, -1),
                _ => return None,
            };
            (laid(id) && !laid(other)).then_some((other, way))
        })
    }

    /// The group the slot lays out now, its panel in front.
    pub fn slot_group(&self) -> Option<Group> {
        let slot = self.slot()?;
        self.session.groups.iter().find(|g| g.kind.slot() == Some(slot) && self.group_laid(g)).copied()
    }

    /// The group whose list has the keys (normal mode, the group laid out).
    pub fn list_focused(&self) -> Option<Group> {
        let At::List(id) = self.session.docked() else { return None };
        self.session.groups.iter().find(|g| g.list == id && self.group_laid(g)).copied()
    }

    /// The keys to kind `kind`'s list (`C-w h` from its detail, `S-tab`, a click on its rows).
    pub fn list_focus(&mut self, kind: GroupKind) {
        let why = match kind {
            GroupKind::Tests => "no results to list yet — the output is all the panel has",
            GroupKind::Task => "fewer than two places to list — the output is all the panel has",
            GroupKind::Preflight => "one step — the output is all the panel has",
            GroupKind::Procs => ":procs isn't open — :procs opens it",
            _ => return self.set_status(format!("{} isn't a list of its own yet", kind.what())),
        };
        if !listed(self, kind) {
            return self.set_status(why);
        }
        let Some(g) = self.group_made(kind) else { return };
        self.hand_to(kind);
        self.plugin_panel_unfocus();
        self.session.at = At::List(g.list);
        crate::rundoc::sync(self);
    }

    /// The keys to kind `kind`'s detail — in the slot or docked, normal mode on its document (`rundoc.rs`).
    pub fn detail_focus(&mut self, kind: GroupKind) {
        if kind.slot().is_some() {
            self.hand_to(kind);
            self.plugin_panel_unfocus();
            self.session.at = At::Slot;
            crate::rundoc::sync(self);
        }
    }

    /// The keys go to a member of kind `kind`'s group: docked, it's in hand (`Session.hand` — folded into the slot
    /// too, for when it's back); in the slot, its panel comes to the slot's front.
    fn hand_to(&mut self, kind: GroupKind) {
        let apart = self.session.group(kind).is_some_and(|g| g.place != crate::pane::Place::Slot);
        self.session.hand = apart.then_some(kind);
        if !self.out_of_slot(kind) {
            self.session.slot_front = kind.slot();
        }
    }

    /// The detail of a group hosted by leaf `host` that's laid out in `leaves` (the last frame's): where the host's
    /// keys are on screen — the slot's on the tests' are its detail's.
    pub fn hosted_detail(
        &self,
        host: crate::split::PaneId,
        leaves: &[(crate::split::PaneId, crate::split::Rect)],
    ) -> Option<crate::split::PaneId> {
        let laid = |id: &crate::split::PaneId| leaves.iter().any(|l| l.0 == *id);
        self.session
            .groups
            .iter()
            .filter(|g| self.group_host(g) == Some(host))
            .find_map(|g| g.detail_pane().filter(laid))
    }
}

/// Whether kind `kind`'s list was drawn in the last frame.
pub fn drawn(ed: &Editor, kind: GroupKind) -> bool {
    ed.screen.list(kind).is_some()
}

/// `q` on a group: its one close — the slot's panel (a run's once it has ended), the side panel's git tab, the
/// review (asked when files are unviewed).
pub fn close(ed: &mut Editor, kind: GroupKind) {
    match kind {
        GroupKind::Git => ed.session.sidebar = None,
        GroupKind::Review => crate::codereview::close_asked(ed),
        k => {
            if let Some(s) = k.slot() {
                ed.slot_close(s);
            }
        }
    }
}

/// `enter` on a group's list: the kind's act — the test's place, the task's place, a preflight step's first place
/// (else its heading, the output with the keys there), the process's detail.
fn open(ed: &mut Editor, kind: GroupKind) {
    let place = match kind {
        GroupKind::Tests => return ed.tests_list_open(),
        GroupKind::Procs => return ed.detail_focus(kind),
        GroupKind::Task => ed.session.task_run.as_ref().and_then(|r| r.places.get(r.picked)).cloned(),
        GroupKind::Preflight => {
            let Some((p, r)) = preflight(ed).zip(ed.session.task_run.as_ref()) else { return };
            let first = (0..r.places.len()).find(|&i| r.origin(i).step == Some(p.selected));
            match first {
                Some(i) => r.places.get(i).cloned(),
                None => {
                    let name = p.steps[p.selected].name.clone();
                    return crate::rundoc::step(ed, &name);
                }
            }
        }
        _ => return,
    };
    let Some(it) = place else { return };
    ed.session.at = At::Code;
    if let Err(e) = ed.open_at(&it.path, it.line, it.col) {
        ed.set_error(format!("{e:#}"));
    }
}

/// `o` on a group's list: a preflight's selected step's output whole, in a buffer of its own (`:preflight-output`).
fn output(ed: &mut Editor, kind: GroupKind) -> bool {
    let Some(name) =
        preflight(ed).filter(|_| kind == GroupKind::Preflight).map(|p| p.steps[p.selected].name.clone())
    else {
        return false;
    };
    if let Err(e) = ed.preflight_output(&name) {
        ed.set_status(e);
    }
    true
}

#[cfg(test)]
mod tests;

/// A key while a group's list has the keys (the module's doc) — false when it isn't one of its.
pub fn key(ed: &mut Editor, key: Key) -> bool {
    if ed.mode != crate::editor::Mode::Normal || !ed.pending.is_empty() || ed.prompt.is_some() {
        return false;
    }
    let Some(g) = ed.list_focused() else { return false };
    let Some(list) = List::of(ed, g.kind) else { return false };
    if let Some(m) = crate::key::list_move(key, list.rows) {
        let (selected, top) = match m {
            // (`C-e`/`C-y`: the view, the selection staying unless it would leave it)
            ListMove::Scroll(by) => {
                let (top, at) = crate::key::in_sight(by, list.top, list.selected, list.n, list.rows, 0);
                (at, top)
            }
            m => (m.from(list.selected, list.n), list.top),
        };
        List::set(ed, g.kind, selected, top);
        return true;
    }
    match (key.code, key.ctrl, key.alt) {
        (Code::Char(' ' | ':'), false, false) | (Code::Char('w'), true, false) => return false,
        (Code::Esc, ..) => ed.session.at = At::Code,
        (Code::Char('q'), false, false) => close(ed, g.kind),
        (Code::Enter, false, false) => open(ed, g.kind),
        (Code::Char('o'), false, false) if output(ed, g.kind) => {}
        (Code::Char(c @ ('s' | 'r' | 'l')), false, false) if g.kind == GroupKind::Procs => ed.procs_act(c),
        (Code::Tab, false, false) => ed.slot_step(1),
        (Code::BackTab, false, false) => ed.slot_step(-1),
        _ => ed.key_unused(key, g.kind.what(), "esc code"),
    }
    true
}
