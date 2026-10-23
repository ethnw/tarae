//! Docked groups (panes P3 step 6, `target/sb/panes-P3-plan.md` §3.1–3.4, §3.7): a group — the tests', a task's, a
//! preflight's, `:procs`' — leaves the slot and docks beside the code, a leaf of the session's tree of its own
//! (`Shows::Group`): `C-w L` right of the code's column (left of the chat), `C-w H` left of it (right of the side
//! panel or the rail), `C-w K` above it in its column; `C-w J` brings it back to the slot. The keys of its members are
//! the ones they have in the slot (§3.3: `At::List`, `At::Slot` with `Session.hand`); the slot leaves it out of its
//! tabs (`slot_shown`) while it's docked in sight.
//!
//! Its place is kept per kind (`Group.place`) while its runs come and go: closed, its leaf takes no room (the code
//! has it) and the next run of its kind lands there. Its size is its kind's own (`Sizes.docked`, `C-w < > - +` from
//! a member, a drag of its line — `Border::Leaf`), automatic until set. Too narrow — the code would keep less than
//! `CODE_MIN` beside it — a group docked beside the code folds back into the slot's tabs for as long (§3.7: it isn't
//! moved; wider, it's back).

use crate::editor::Editor;
use crate::pane::{Group, GroupKind, Pane, Place, Shows, Side};
use crate::session::At;
use crate::sizes::{CODE_MIN, CODE_ROOM, Slot};
use crate::split::{Dir, PaneId, Share, Split};

/// A group docked beside the code is at least this wide (its edge cell in it) — narrower, it folds into the slot.
pub const DOCK_MIN: usize = 36;
/// …and above the code at least this tall (its top line and its bottom edge in it).
pub const DOCK_MIN_ROWS: usize = 5;

impl Editor {
    /// Kind `kind` has something to show: the tests' run, the task's (not a preflight's), a preflight's, `:procs`.
    pub fn kind_there(&self, kind: GroupKind) -> bool {
        let preflight = crate::grouplist::preflight(self).is_some();
        match kind {
            GroupKind::Tests => self.session.test_run.is_some(),
            GroupKind::Task => self.session.task_run.is_some() && !preflight,
            GroupKind::Preflight => self.session.task_run.is_some() && preflight,
            GroupKind::Procs => self.procs_up(),
            GroupKind::Git | GroupKind::Review => false,
        }
    }

    /// The group kind a slot panel shows now (the task's panel: a task's or a preflight's run).
    pub fn kind_of(&self, slot: Slot) -> Option<GroupKind> {
        match slot {
            Slot::Test => Some(GroupKind::Tests),
            Slot::Task if crate::grouplist::preflight(self).is_some() => Some(GroupKind::Preflight),
            Slot::Task => Some(GroupKind::Task),
            Slot::Procs => Some(GroupKind::Procs),
            Slot::Debug | Slot::Terminal | Slot::Plugin => None,
        }
    }

    /// Kind `kind`'s group is docked and in sight: docked, something to show, its leaf in the tree, and room for it on
    /// this screen — beside the code, by the frame's width; over it, the rows it gets (`Screen.over_folded`, as last
    /// drawn) — before the first frame, room.
    pub fn docked_in_sight(&self, kind: GroupKind) -> bool {
        let Some(g) = self.session.group(kind) else { return false };
        let Place::Docked(side) = g.place else { return false };
        if !self.kind_there(kind) || !self.dock_leaf(&g) {
            return false;
        }
        match (side.beside(), self.screen.width) {
            (true, Some(total)) => self.dock_widths(total)[kind.dock_index()] > 0,
            (true, None) => true,
            (false, _) => !self.screen.over_folded[kind.dock_index()],
        }
    }

    /// Slot panel `slot`'s group is docked in sight — the slot leaves it out (`slot_shown`, `slot_tabs`).
    pub fn slot_docked(&self, slot: Slot) -> bool {
        self.kind_of(slot).is_some_and(|k| self.docked_in_sight(k))
    }

    /// The leaf group `g` is laid out in: its own while it's docked in sight, else the slot's.
    pub fn group_host(&self, g: &Group) -> Option<PaneId> {
        match self.docked_in_sight(g.kind) {
            true => Some(g.host),
            false => self.session.leaf(Shows::Slot),
        }
    }

    /// The groups docked beside the code and in sight on a screen `total` wide: each kind's width there (its edge
    /// cell in it) — 0 for one not docked beside it, with nothing to show, or folded: the code would keep less than
    /// `CODE_MIN` beside it. Automatic, as much as leaves the code `CODE_ROOM`, up to 2/5 of the screen; yours
    /// (`Sizes.docked`) as much as leaves it `CODE_MIN`. In the kinds' order, each after the ones before it; two on one
    /// side are stacked in one column (`place`), as wide as the wider, its width taken off the room once.
    pub fn dock_widths(&self, total: usize) -> [usize; 4] {
        let mut out = [0; 4];
        let side_open = self.session.sidebar.is_some() && !self.review_rail_up();
        let w = crate::sizes::widths(total, self.chat_panel(total), side_open, self.sizes);
        let rail = if self.review_rail_up() { self.screen.rail_w } else { 0 };
        let mut left = total.saturating_sub(w.chat + w.side + rail);
        if w.chat >= total {
            return out;
        }
        // (each side's column as wide as taken so far: left, right)
        let mut column = [0; 2];
        for kind in GroupKind::ALL.into_iter().filter(|k| k.dockable()) {
            let Some(g) = self.session.group(kind) else { continue };
            let Place::Docked(side) = g.place else { continue };
            if !side.beside() || !self.kind_there(kind) || !self.dock_leaf(&g) {
                continue;
            }
            let col = &mut column[usize::from(side == Side::Right)];
            // (the room with this side's column given back: one stacked in it is at least as wide)
            let room = left + *col;
            let want = match self.sizes.docked[kind.dock_index()] {
                Some(k) => (k as usize).min(room.saturating_sub(CODE_MIN)),
                None => (total * 2 / 5).min(room.saturating_sub(CODE_ROOM)),
            }
            .max(*col);
            if want >= DOCK_MIN && room.saturating_sub(want) >= CODE_MIN {
                out[kind.dock_index()] = want;
                left = room - want;
                *col = want;
            }
        }
        out
    }

    /// A group docked above the code: its rows of `total` (the code's and the slot's — the path bar's are apart, over
    /// the code under it), the slot keeping `slot` and the code `BOTTOM_CODE` — yours (`Sizes.docked`) or a third of
    /// them (6–14).
    pub fn dock_rows(&self, kind: GroupKind, total: usize, slot: usize) -> usize {
        let max = total.saturating_sub(slot + crate::sizes::BOTTOM_CODE);
        let want = match self.sizes.docked[kind.dock_index()] {
            Some(k) => k as usize,
            None => (total / 3).clamp(6, 14),
        };
        let n = want.min(max);
        if n >= DOCK_MIN_ROWS { n } else { 0 }
    }

    /// Group `g`'s own leaf is in the tree (it's docked and its kind has something to show — `dock_sync`).
    pub fn dock_leaf(&self, g: &Group) -> bool {
        self.session.shows(g.host) == Some(Shows::Group(g.kind)) && self.session.split.holds(g.host)
    }

    /// Every event: a docked group's leaf is in the tree while its kind has something to show (§3.4: closed, the
    /// code takes the room; its next run lands on its side again) — out of it, the group's host is the slot's leaf,
    /// its place kept.
    pub fn dock_sync(&mut self) {
        for i in 0..self.session.groups.len() {
            let g = self.session.groups[i];
            let Place::Docked(side) = g.place else { continue };
            match (self.kind_there(g.kind), self.dock_leaf(&g)) {
                (true, false) => {
                    self.dock_in(g.kind, side);
                }
                (false, true) => self.dock_out(g.kind),
                _ => {}
            }
        }
    }

    /// Kind `kind`'s leaf into the tree on `side` (its pane made, `Share::Auto`) — false where the tree has no such
    /// place.
    fn dock_in(&mut self, kind: GroupKind, side: Side) -> bool {
        let Some(mut g) = self.session.group(kind) else { return false };
        let host = match self.session.panes.iter().find(|p| p.shows == Shows::Group(kind)) {
            Some(p) => p.id,
            None => {
                let id = self.next_pane;
                self.next_pane += 1;
                self.session.panes.push(Pane { id, shows: Shows::Group(kind) });
                id
            }
        };
        self.session.split.remove(host);
        let there: Vec<PaneId> = self
            .session
            .groups
            .iter()
            .filter(|o| o.kind != kind && o.place == Place::Docked(side) && self.dock_leaf(o))
            .map(|o| o.host)
            .collect();
        let mut split = std::mem::replace(&mut self.session.split, Split::Leaf(0));
        let placed = place(&mut split, host, side, &|p| there.contains(&p), &self.session);
        self.session.split = split;
        if !placed {
            self.session.panes.retain(|p| p.id != host);
            return false;
        }
        g.host = host;
        g.place = Place::Docked(side);
        self.group_set(g);
        true
    }

    /// Kind `kind`'s leaf out of the tree, its pane gone, its group's host the slot's leaf — its place as it was.
    fn dock_out(&mut self, kind: GroupKind) {
        let Some(mut g) = self.session.group(kind) else { return };
        if self.dock_leaf(&g) {
            self.session.split.remove(g.host);
        }
        self.session.panes.retain(|p| p.shows != Shows::Group(kind));
        if let Some(slot) = self.session.leaf(Shows::Slot) {
            g.host = slot;
        }
        self.group_set(g);
    }

    /// Every event and frame: `Session.hand` is let go once the keys aren't in its group. Folded into the slot since
    /// it was last drawn (`Screen.docked`), the keys stay with it there — its panel in the slot's front, once — and the
    /// hand stays, inert while it's folded (`slot_in_hand`), its own again when it's back beside the code.
    pub fn hand_sync(&mut self) {
        let Some(kind) = self.session.hand else { return };
        let Some(g) = self.session.group(kind) else { return self.session.hand = None };
        let keys = self.session.is_at(At::Slot) || self.session.is_at(At::List(g.list));
        if !keys || !matches!(g.place, Place::Docked(_)) {
            return self.session.hand = None;
        }
        if !self.docked_in_sight(kind) && self.screen.docked.borrow().iter().any(|d| d.0 == kind) {
            self.session.slot_front = kind.slot();
        }
    }

    /// The group in hand when the keys go to slot panel `slot`: its kind's, docked and folded into the slot (in sight,
    /// it isn't the slot's).
    pub fn hand_of(&self, slot: Option<Slot>) -> Option<GroupKind> {
        let kind = self.kind_of(slot?)?;
        let g = self.session.group(kind)?;
        matches!(g.place, Place::Docked(_)).then_some(kind)
    }

    /// The pane the keys are in (`Session::at_pane`): a docked group's own leaf only while it's in sight — folded into
    /// the slot, its detail is the slot's there (`slot_in_hand`, `focused_area`).
    pub fn at_pane(&self) -> Option<PaneId> {
        self.session.at_pane(|k| self.docked_in_sight(k))
    }

    /// `C-w t` from group `kind`'s member: its members turned as `C-w t` turns a split — side by side ↔ over and under,
    /// from what the last frame showed (stacked: its choice turned, over and under at first); kept for its kind
    /// (`Group.laid`, across `:restart`), `C-w =` automatic again. A choice with no room is stacked until there is —
    /// said.
    pub fn group_turn(&mut self, kind: GroupKind) {
        use crate::pane::{Laid, Shape};
        let Some(mut g) = self.session.group(kind) else { return };
        let shown = self.screen.shapes.iter().find(|s| s.0 == kind).map(|s| s.1);
        let (to, shape) = match (shown, g.laid) {
            (Some(Shape::Beside), _) | (Some(Shape::Stacked) | None, Laid::Beside | Laid::Auto) => {
                (Laid::Over, Shape::Over)
            }
            _ => (Laid::Beside, Shape::Beside),
        };
        g.laid = to;
        self.group_set(g);
        let fits = crate::term::group_shape_now(self, &g).is_none_or(|s| s == shape);
        let said = match fits {
            true => format!("{} {} — C-w t turns it back, C-w = automatic", kind.name(), shape.words()),
            false => format!("{} {} when there's room — stacked until then", kind.name(), shape.words()),
        };
        self.set_status(said);
    }

    /// The group a docking key acts on: the one whose list or detail has the keys — in the slot (with nothing to list
    /// yet, its panel's output alone is the group's detail) or docked.
    pub fn group_in_hand(&self) -> Option<GroupKind> {
        if let At::List(id) = self.session.docked() {
            return self.session.groups.iter().find(|g| g.list == id).map(|g| g.kind);
        }
        if !self.session.is_at(At::Slot) {
            return None;
        }
        self.slot().and_then(|s| self.kind_of(s))
    }

    /// `C-w H J K L` (`swap_view_*`) from a group's member: docked on that side — left, above, right; below is the slot
    /// (`C-w J`). False: the keys aren't in a group (the code's panes trade places, `swap_view`).
    pub fn dock_key(&mut self, dx: i32, dy: i32) -> bool {
        let Some(kind) = self.group_in_hand() else {
            // (the slot's other panels stay in it: said)
            if self.bottom_has_keys() || self.session.is_at(At::Program) {
                let what = match self.slot() {
                    Some(Slot::Terminal) => {
                        "the terminal stays in the slot — a terminal pane comes with the terminal work"
                    }
                    _ => "this panel stays in the slot — the tests, a task, a preflight and :procs dock",
                };
                self.set_status(what);
                return true;
            }
            return false;
        };
        let side = match (dx, dy) {
            (-1, _) => Side::Left,
            (1, _) => Side::Right,
            (_, -1) => Side::Top,
            _ => {
                self.undock(kind);
                return true;
            }
        };
        self.dock(kind, side);
        true
    }

    /// Kind `kind`'s group docked on `side` (§3.1), the keys staying with its member that had them; the slot shows
    /// its next tab. Already there: said.
    pub fn dock(&mut self, kind: GroupKind, side: Side) {
        let Some(g) = self.group_made(kind) else { return };
        if g.place == Place::Docked(side) && self.dock_leaf(&g) {
            return self.set_status(format!(
                "{} docked {} already — C-w J back to the slot",
                kind.name(),
                side.name()
            ));
        }
        let keys = self.group_keys(&g);
        if !self.dock_in(kind, side) {
            return self.set_status("no place for it beside the code in this layout");
        }
        let Some(g) = self.session.group(kind) else { return };
        self.keys_back(&g, keys);
        let room = self.docked_in_sight(kind) || self.screen.width.is_none();
        let said = match room {
            true => format!("{} docked {} — C-w J back to the slot", kind.name(), side.name()),
            false => {
                format!("{} docked {} — in the slot until the screen is wider", kind.name(), side.name())
            }
        };
        self.set_status(said);
    }

    /// Kind `kind`'s group back in the slot, in front, the keys staying with its member; its leaf goes. In the slot
    /// already: said.
    pub fn undock(&mut self, kind: GroupKind) {
        let Some(mut g) = self.session.group(kind) else { return };
        if g.place == Place::Slot {
            return self.set_status(format!("{} is in the slot — C-w H K L dock it", kind.name()));
        }
        let keys = self.group_keys(&g);
        self.dock_out(kind);
        let Some(slot) = self.session.leaf(Shows::Slot) else { return };
        g.host = slot;
        g.place = Place::Slot;
        self.group_set(g);
        // (in front of the slot: the member's keys put it there — `hand_to`)
        self.keys_back(&g, keys);
        self.set_status(format!("{} back in the slot", kind.name()));
    }

    /// Kind `kind`'s docked group as last drawn: its side and its leaf's rect.
    pub fn dock_rect(&self, kind: GroupKind) -> Option<(Side, crate::split::Rect)> {
        let Place::Docked(side) = self.session.group(kind)?.place else { return None };
        let r = self.screen.docked.borrow().iter().find(|d| d.0 == kind)?.1;
        Some((side, r))
    }

    /// The docked group whose edge is at screen cell (`x`, `y`), as last drawn (`term::dock_edge`) — docked over the
    /// code, the code's top line under it (its bottom: `Layout::docked` gives its last row to its panel).
    pub fn dock_edge_at(&self, x: usize, y: usize) -> Option<GroupKind> {
        let docked = self.screen.docked.borrow().iter().map(|d| d.0).collect::<Vec<_>>();
        docked.into_iter().find(|&kind| {
            let Some((side, r)) = self.dock_rect(kind) else { return false };
            let rows = (r.y..r.y + r.h).contains(&y);
            match side {
                Side::Right => x == r.x && rows,
                Side::Left => x + 1 == r.x + r.w && rows,
                Side::Top => y == r.y + r.h && (r.x..r.x + r.w).contains(&x),
            }
        })
    }

    /// Kind `kind`'s docked size set to `n` — its width beside the code (`DOCK_MIN` at least, the code keeping
    /// `CODE_MIN`), its rows over it (`DOCK_MIN_ROWS`, the code keeping `BOTTOM_CODE`); kept (`sizes.json`) — for the
    /// group stacked with it too (one column, or one row over the code: sized as one). The size it got.
    pub fn dock_size(&mut self, kind: GroupKind, n: usize) -> usize {
        let Some((side, r)) = self.dock_rect(kind) else { return n };
        let s = &self.screen;
        // (the docked groups drawn on this side in r's column (beside) or row (over the code): r's and its stacked one)
        let docked: Vec<(GroupKind, Side, crate::split::Rect)> = (s.docked.borrow().clone().into_iter())
            .filter_map(|(k, r)| Some((k, self.dock_rect(k)?.0, r)))
            .collect();
        let (min, max) = match side.beside() {
            true => {
                let total = s.width.unwrap_or(r.w);
                // (each column once, its stacked halves sharing it)
                let mut columns: Vec<(usize, usize)> = Vec::new();
                for c in docked.iter().filter(|d| d.1.beside()).map(|d| (d.2.x, d.2.w)) {
                    if !columns.contains(&c) {
                        columns.push(c);
                    }
                }
                let beside: usize = columns.iter().map(|c| c.1).sum();
                let code = total.saturating_sub(s.chat_w + s.side_w + s.rail_w + beside);
                (DOCK_MIN, r.w + code.saturating_sub(CODE_MIN))
            }
            false => {
                // (the code's rows: `body_h` leaves out the group's own and the path bar's already)
                let code = s.body_h.saturating_sub(s.bottom_h);
                (DOCK_MIN_ROWS, r.h + code.saturating_sub(crate::sizes::BOTTOM_CODE))
            }
        };
        let n = n.clamp(min, max.max(min));
        let one = |d: &crate::split::Rect| if side.beside() { d.x == r.x } else { d.y == r.y };
        let kinds = docked.iter().filter(|d| d.1 == side && one(&d.2)).map(|d| d.0);
        for k in kinds.chain([kind]) {
            self.sizes.docked[k.dock_index()] = Some(n as u16);
        }
        n
    }

    /// `C-w < > - +` from a docked group's member: its size `STEP_COLS`/`STEP_ROWS` more or less — `< >` beside the
    /// code, `- +` over it (the other pair said). False: the keys aren't in a docked group.
    pub fn size_dock(&mut self, dir: Dir, more: bool) -> bool {
        let Some(kind) = self.slot_in_hand().and(self.session.hand) else { return false };
        let Some((side, r)) = self.dock_rect(kind) else { return false };
        let beside = side.beside();
        if beside != (dir == Dir::Vertical) {
            let keys = if beside { "C-w < >" } else { "C-w - +" };
            self.set_status(format!("{} docked {} — {keys} size it", kind.name(), side.name()));
            return true;
        }
        let (now, step, unit) = match beside {
            true => (r.w, crate::sizes::STEP_COLS, "columns"),
            false => (r.h, crate::sizes::STEP_ROWS, "rows"),
        };
        let to = if more { now + step } else { now.saturating_sub(step) };
        let n = self.dock_size(kind, to);
        self.sizes_keep();
        self.say_size(format!("{} {n} {unit}", kind.name()));
        true
    }

    /// The keys to run panel `slot`'s output: a docked group's detail, else the slot's (`bottom_take_keys`).
    pub fn panel_take_keys(&mut self, slot: Slot) {
        match self.kind_of(slot).filter(|&k| self.docked_in_sight(k)) {
            Some(kind) => self.detail_focus(kind),
            None => self.bottom_take_keys(),
        }
    }

    /// Which of group `g`'s members has the keys.
    fn group_keys(&self, g: &Group) -> Option<crate::pane::Member> {
        if self.session.is_at(At::List(g.list)) {
            return Some(crate::pane::Member::List);
        }
        (self.session.is_at(At::Slot) && self.slot().and_then(|s| self.kind_of(s)) == Some(g.kind))
            .then_some(crate::pane::Member::Detail)
    }

    /// The keys back to group `g`'s member `keys` had, where it is now.
    fn keys_back(&mut self, g: &Group, keys: Option<crate::pane::Member>) {
        match keys {
            Some(crate::pane::Member::List) => self.list_focus(g.kind),
            Some(crate::pane::Member::Detail) => self.detail_focus(g.kind),
            None => {}
        }
    }

    fn group_set(&mut self, g: Group) {
        if let Some(at) = self.session.groups.iter_mut().find(|x| x.kind == g.kind) {
            *at = g;
        }
    }

    /// A docked group's own size on the tree's `Auto` share (`term::Surfaces`): kind `kind`'s width beside the code
    /// on a screen `total` wide, or its rows above it.
    pub fn dock_auto(&self, kind: GroupKind, dir: Dir, total: usize, rows: usize, slot: usize) -> usize {
        if !self.docked_in_sight(kind) && self.screen.width.is_some() {
            // (folded or nothing to show — the frame's own width said it before this was asked, `render_frame`)
            return 0;
        }
        match dir {
            Dir::Vertical => self.dock_widths(total)[kind.dock_index()],
            Dir::Horizontal => self.dock_rows(kind, rows, slot),
        }
    }
}

#[cfg(test)]
mod tests;

/// Leaf `host` into `split` on `side` (`Share::Auto`): beside the chat's leaf or the left column's, or over the code's
/// column — a group docked there already, the two stacked in a node of their own. False: no such place in this tree.
fn place(
    split: &mut Split,
    host: PaneId,
    side: Side,
    docked: &impl Fn(PaneId) -> bool,
    s: &crate::session::Session,
) -> bool {
    let leaf = |shows: Shows| s.leaf(shows);
    match side {
        Side::Right => {
            let chat = leaf(Shows::Chat);
            beside(split, host, &|id| Some(id) == chat, false, docked)
        }
        Side::Left => {
            let (a, b) = (leaf(Shows::Side), leaf(Shows::Rail));
            beside(split, host, &|id| Some(id) == a || Some(id) == b, true, docked)
        }
        Side::Top => over_column(split, host, s, docked),
    }
}

/// `host` into the side-by-side node holding a leaf `anchor` says: before the first such (`after` false — the chat),
/// or after the last (the left column) — or stacked with a group docked there already.
fn beside(
    split: &mut Split,
    host: PaneId,
    anchor: &impl Fn(PaneId) -> bool,
    after: bool,
    docked: &impl Fn(PaneId) -> bool,
) -> bool {
    let Split::Node(dir, kids) = split else { return false };
    let at = kids.iter().position(|k| matches!(k.1, Split::Leaf(id) if anchor(id)));
    if *dir == Dir::Vertical
        && let Some(i) = at
    {
        let i = if after {
            kids.iter().rposition(|k| matches!(k.1, Split::Leaf(id) if anchor(id))).unwrap_or(i) + 1
        } else {
            i
        };
        // (one docked there already, next to it: the two stacked)
        let near = if after { i } else { i.wrapping_sub(1) };
        if let Some(k) = kids.get_mut(near)
            && k.1.leaves().into_iter().all(docked)
        {
            stack(&mut k.1, host, Dir::Horizontal);
            return true;
        }
        kids.insert(i, (Share::Auto, Split::Leaf(host)));
        return true;
    }
    kids.iter_mut().any(|k| beside(&mut k.1, host, anchor, after, docked))
}

/// `host` over the code's column (the smallest node holding the code's panes and the slot): the column wrapped in a
/// stacked node, `host` first — or beside a group docked there already.
fn over_column(
    split: &mut Split,
    host: PaneId,
    s: &crate::session::Session,
    docked: &impl Fn(PaneId) -> bool,
) -> bool {
    let mut want: Vec<PaneId> = s.views.iter().map(|v| v.id).collect();
    want.extend(s.leaf(Shows::Slot));
    let holds = |n: &Split| want.iter().all(|&id| n.holds(id));
    // (a column docked over already: `H[(Auto, docked), (Even, column)]` — the new one beside the one there)
    fn walk(
        n: &mut Split,
        host: PaneId,
        holds: &impl Fn(&Split) -> bool,
        docked: &impl Fn(PaneId) -> bool,
    ) -> bool {
        let Split::Node(dir, kids) = n else { return false };
        if *dir == Dir::Horizontal
            && kids.len() == 2
            && kids[0].0 == Share::Auto
            && kids[0].1.leaves().into_iter().all(docked)
            && holds(&kids[1].1)
        {
            stack(&mut kids[0].1, host, Dir::Vertical);
            return true;
        }
        let Some(k) = kids.iter_mut().find(|k| holds(&k.1)) else { return false };
        // (its child holding them all is the column when none of that one's own children does)
        let inner = match &k.1 {
            Split::Node(_, ks) => ks.iter().any(|c| holds(&c.1)),
            Split::Leaf(_) => false,
        };
        if inner {
            return walk(&mut k.1, host, holds, docked);
        }
        let column = std::mem::replace(&mut k.1, Split::Leaf(host));
        k.1 = Split::Node(Dir::Horizontal, vec![(Share::Auto, Split::Leaf(host)), (Share::Even, column)]);
        true
    }
    if !holds(split) {
        return false;
    }
    if walk(split, host, &holds, docked) {
        return true;
    }
    // (the column is the whole tree)
    let column = std::mem::replace(split, Split::Leaf(host));
    *split = Split::Node(Dir::Horizontal, vec![(Share::Auto, Split::Leaf(host)), (Share::Even, column)]);
    true
}

/// `host` stacked with what's at `n` (a docked group, or a node of them) going `dir`.
fn stack(n: &mut Split, host: PaneId, dir: Dir) {
    match n {
        Split::Node(d, kids) if *d == dir => kids.push((Share::Even, Split::Leaf(host))),
        _ => {
            let was = std::mem::replace(n, Split::Leaf(host));
            *n = Split::Node(dir, vec![(Share::Even, was), (Share::Even, Split::Leaf(host))]);
        }
    }
}
