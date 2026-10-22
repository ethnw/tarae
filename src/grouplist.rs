//! A linked group's list (panes P3 §2.1, `pane::Group`): one handler for every kind's. It moves as every list does
//! (`key::list_move`, the list's measures — its selection drives the detail beside it), `enter` is the kind's act,
//! `tab`/`S-tab` the next/previous member (then the slot's next tab — `slot_step`), `q` the group's one close (a
//! run's only once it has ended — `Editor::slot_close`), `esc` back to the code; `space`, `:` and `C-w` are the
//! editor's, anything else is said. The kind supplies its rows, its `enter`, the link and its words; the tests' is
//! built (`testing.rs`), the other kinds come with P3's steps 3–5 (until then none is laid out, so none takes keys).

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
            GroupKind::Git | GroupKind::Review => None,
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
        match kind {
            GroupKind::Tests => {
                let r = ed.session.test_run.as_ref()?;
                let rows = ed.screen.tests_list.get().map_or(0, |l| l.1);
                Some(List { n: r.cases.len(), selected: r.selected, top: r.list_top.get(), rows })
            }
            _ => None,
        }
    }

    /// The selection moved: kept, and the link — the detail follows (the tests': the failure picked, its own
    /// scroll again).
    fn set(ed: &mut Editor, kind: GroupKind, selected: usize, top: usize) {
        if kind == GroupKind::Tests
            && let Some(r) = ed.session.test_run.as_mut()
        {
            r.list_top.set(top);
            if r.selected != selected {
                r.scroll = None;
            }
            r.selected = selected;
        }
    }
}

impl Editor {
    /// Group `g` is laid out — its host in sight with something to list (the tests': the slot showing a run with
    /// results); with nothing to list its host shows the detail alone, one pane.
    pub fn group_laid(&self, g: &Group) -> bool {
        match g.kind {
            GroupKind::Tests => {
                self.tests_grouped() && self.session.leaf(crate::pane::Shows::Slot) == Some(g.host)
            }
            _ => false,
        }
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
        match kind {
            GroupKind::Tests => self.tests_list_focus(),
            _ => self.set_status(format!("{} isn't a list of its own yet", kind.what())),
        }
    }

    /// The keys to kind `kind`'s detail — in the slot, the slot's view mode on it.
    pub fn detail_focus(&mut self, kind: GroupKind) {
        if let Some(s) = kind.slot() {
            self.session.slot_front = Some(s);
            self.bottom_take_keys();
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
        self.session.groups.iter().filter(|g| g.host == host).find_map(|g| g.detail_pane().filter(laid))
    }
}

/// Whether kind `kind`'s list was drawn in the last frame.
pub fn drawn(ed: &Editor, kind: GroupKind) -> bool {
    match kind {
        GroupKind::Tests => ed.screen.tests_list.get().is_some(),
        _ => false,
    }
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

/// `enter` on a group's list: the kind's act.
fn open(ed: &mut Editor, kind: GroupKind) {
    if kind == GroupKind::Tests {
        ed.tests_list_open();
    }
}

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
        (Code::Tab, false, false) => ed.slot_step(1),
        (Code::BackTab, false, false) => ed.slot_step(-1),
        _ => ed.key_unused(key, g.kind.what(), "esc code"),
    }
    true
}
