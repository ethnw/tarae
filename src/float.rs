//! Floating groups (panes P3 step 7, `target/sb/panes-P3-plan.md` §3.1–3.3): a group — the tests', a task's, a
//! preflight's, `:procs`' — leaves the slot or its docked side and floats over the docked panes, in a frame of its own
//! with its name in the top line (D3, `term::draw_floating`): `C-w e` floats the group in hand and docks a floating one
//! back where it came from (`Group.back`); `C-w H J K L` dock it on that side, `C-w J` the slot. The whole screen is
//! its frame (user, 2026-10-23), never the code area: first placed centred, 3/5 × 1/2 of it (`Frame::CENTRED`: its top
//! at 1/4 of the screen's height, every floating pane's — a list's too, `Frame::LIST`), then
//! where you put it — `C-w m` and `h j k l` move it (`Handler::Move`), `C-w < > - +` size it, `C-w =` puts it back
//! centred; the mouse drags its top line (off the names) and sizes it by its edges. Its frame is kept as thousandths of
//! the screen (`Frame`), clamped onto it every frame: a resize keeps it in sight.
//!
//! Its host is a pane of its own out of the tree (`Shows::Group`, `float_groups_sync`) — its members laid out in its
//! rect as the slot lays them out, their keys the ones they have there (§3.3: `At::List`, `At::Slot` with
//! `Session.hand`). Several float in a stack: the one with the keys on top (`Group.raised`), the others in their order.
//! A hop `C-w h/j/k/l` from one goes by its rect's centre; `C-w w` goes round them after the docked panes.

use crate::editor::Editor;
use crate::pane::{Frame, Group, GroupKind, Pane, Place, Shows};
use crate::split::{PaneId, Rect};

/// `C-w m`'s keys move a floating pane this many columns or rows a press (§3.2).
pub const MOVE_COLS: usize = 4;
pub const MOVE_ROWS: usize = 2;

/// What a press on a floating frame took hold of (`Editor::float_grip`): its top line off the names — it moves, the
/// press this far from its top-left corner —, or its edges — they size it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grip {
    Move(usize, usize),
    Size { left: bool, right: bool, bottom: bool },
}

impl Editor {
    /// Kind `kind`'s group floats in sight: floating, something to show, its own pane made (`float_groups_sync`).
    pub fn floating_in_sight(&self, kind: GroupKind) -> bool {
        if crate::floatlist::is_list(kind) {
            return self.float_list(kind).is_some();
        }
        let Some(g) = self.session.group(kind) else { return false };
        matches!(g.place, Place::Floating(_)) && self.kind_there(kind) && self.float_host(&g)
    }

    /// Kind `kind`'s group is in sight in a place of its own, out of the slot: docked with room, or floating — the
    /// slot leaves it out (`slot_shown`), its keys are its own pane's (`slot_in_hand`).
    pub fn out_of_slot(&self, kind: GroupKind) -> bool {
        self.docked_in_sight(kind) || self.floating_in_sight(kind)
    }

    /// Group `g`'s host is its own pane, out of the tree.
    fn float_host(&self, g: &Group) -> bool {
        self.session.shows(g.host) == Some(Shows::Group(g.kind)) && !self.session.split.holds(g.host)
    }

    /// The floating groups in sight, bottom first (`Group.raised`).
    pub fn floats_up(&self) -> Vec<Group> {
        let mut v: Vec<Group> =
            self.session.groups.iter().filter(|g| self.floating_in_sight(g.kind)).copied().collect();
        crate::sorting::by_key(&mut v, |g| g.raised);
        v
    }

    /// Kind `kind`'s own pane (`Shows::Group`): the one it has, else made.
    pub(crate) fn group_pane(&mut self, kind: GroupKind) -> PaneId {
        if let Some(p) = self.session.panes.iter().find(|p| p.shows == Shows::Group(kind)) {
            return p.id;
        }
        let id = self.next_pane;
        self.next_pane += 1;
        self.session.panes.push(Pane { id, shows: Shows::Group(kind) });
        id
    }

    /// Every event: a floating group's host is its own pane, out of the tree — made when it floats, after a restart
    /// (whose file kept the slot's leaf as its host).
    pub fn float_groups_sync(&mut self) {
        for i in 0..self.session.groups.len() {
            let g = self.session.groups[i];
            if !matches!(g.place, Place::Floating(_)) || self.float_host(&g) {
                continue;
            }
            let host = self.group_pane(g.kind);
            self.session.split.remove(host);
            self.session.groups[i].host = host;
        }
    }

    /// `C-w e` (§3.2, zellij's "embed or float"): the group in hand floats — from the slot or its docked side, the keys
    /// staying with its member —; a floating one docks back where it came from. The slot's other panels stay in it,
    /// said. One meaning (user, 2026-10-23): where nothing in hand can float — the code, the chat, the side panel —
    /// it says so (the side panel's keys are its own, `space e`, `space g`…).
    pub fn float_key(&mut self) {
        if let Some(k) = self.float_list_in_hand() {
            return self
                .set_status(format!("the {} list floats — C-w m moves it, q closes it", list_name(k)));
        }
        let Some(kind) = self.group_in_hand() else {
            if self.bottom_has_keys() || self.session.is_at(crate::session::At::Program) {
                return self.set_status(
                    "this panel stays in the slot — the tests, a task, a preflight and :procs float",
                );
            }
            return self
                .set_status("nothing here floats — C-w e floats the tests, a task, a preflight, :procs");
        };
        match self.session.group(kind).map(|g| g.place) {
            Some(Place::Floating(_)) => self.unfloat(kind),
            _ => self.float(kind),
        }
    }

    /// Kind `kind`'s group floating, on top, the keys staying with its member that had them; the slot shows its next
    /// tab, a docked side gives its room back to the code.
    pub fn float(&mut self, kind: GroupKind) {
        let Some(mut g) = self.group_made(kind) else { return };
        let keys = self.group_keys(&g);
        if self.dock_leaf(&g) {
            self.session.split.remove(g.host);
        }
        g.back = match g.place {
            Place::Floating(_) => g.back,
            p => p,
        };
        g.place = Place::Floating(Frame::CENTRED);
        g.raised = self.float_top() + 1;
        self.group_set(g);
        self.float_groups_sync();
        let Some(g) = self.session.group(kind) else { return };
        self.keys_back(&g, keys);
        self.set_status(format!("{} floating — C-w m moves it, C-w e docks it back", kind.name()));
    }

    /// Kind `kind`'s floating group docked back where it came from (`Group.back`): its side, else the slot.
    fn unfloat(&mut self, kind: GroupKind) {
        let Some(g) = self.session.group(kind) else { return };
        match g.back {
            Place::Docked(side) => self.dock(kind, side),
            _ => self.undock(kind),
        }
    }

    /// The highest stacking turn among the floating groups.
    fn float_top(&self) -> u32 {
        self.session.groups.iter().map(|g| g.raised).max().unwrap_or(0)
    }

    /// Every event: a floating group whose member has the keys is on top of the others (§3.3), the rest in their order.
    pub fn float_raise(&mut self) {
        let Some(kind) = self.session.hand.filter(|&k| self.floating_in_sight(k)) else { return };
        let top = self.float_top();
        if let Some(g) = self.session.groups.iter_mut().find(|g| g.kind == kind)
            && (g.raised < top || top == 0)
        {
            g.raised = top + 1;
        }
    }

    /// The screen floating panes are placed on: its width and the rows above the status line (as last drawn —
    /// before the first frame, 80 × 24's).
    pub fn float_screen(&self) -> (usize, usize) {
        (self.screen.width.unwrap_or(80), self.screen.height.unwrap_or(23))
    }

    /// Kind `kind`'s floating frame's cells on this screen.
    pub fn float_rect(&self, kind: GroupKind) -> Option<Rect> {
        let (w, h) = self.float_screen();
        match kind {
            k if crate::floatlist::is_list(k) => self.float_list_rect(k, w, h),
            _ => Some(self.session.group(kind)?.place.frame()?.rect(w, h)),
        }
    }

    /// Kind `kind`'s floating frame set to the cells `r` — onto the screen, at least its least size (`Frame::rect`).
    fn float_set(&mut self, kind: GroupKind, r: Rect) {
        let (w, h) = self.float_screen();
        let f = Frame::of_rect(r, w, h);
        // (a floating list's is the viewer's, kept with the sizes — as tall as its rows still unless its height
        // changed)
        if crate::floatlist::is_list(kind) {
            let rows = self.float_list_frame(kind).is_some_and(|f| f.h == 0)
                && self.float_rect(kind).is_some_and(|was| was.h == r.h);
            self.float_list_frame_set(kind, Some(if rows { Frame { h: 0, ..f } } else { f }));
            return;
        }
        if let Some(g) = self.session.groups.iter_mut().find(|g| g.kind == kind)
            && matches!(g.place, Place::Floating(_))
        {
            g.place = Place::Floating(f);
        }
    }

    /// The keys to floating pane `kind` (a press on it): the session list's, a group's list where it has one, else its
    /// detail.
    pub fn float_focus(&mut self, kind: GroupKind) {
        match kind {
            k if crate::floatlist::is_list(k) => self.float_list_focus(k),
            k if crate::grouplist::listed(self, k) => self.list_focus(k),
            k => self.detail_focus(k),
        }
    }

    /// The floating group the keys are in, if they're in one.
    pub fn float_in_hand(&self) -> Option<GroupKind> {
        if let Some(k) = self.float_list_in_hand() {
            return Some(k);
        }
        self.group_in_hand().filter(|&k| self.floating_in_sight(k))
    }

    /// `C-w m` (§3.2): the floating pane in hand moves by `h j k l` and the arrows, `MOVE_COLS`/`MOVE_ROWS` a press,
    /// until `enter`, `esc` or `C-w m` again (`Handler::Move`). Not on a floating pane: said.
    pub fn float_move_toggle(&mut self) {
        if self.float_moving.take().is_some() {
            return self.float_moved_said();
        }
        let Some(kind) = self.float_in_hand() else {
            return self.set_status("C-w m moves a floating pane — C-w e floats the tests, a task, :procs");
        };
        self.float_moving = Some(kind);
    }

    /// A key while `C-w m` moves a floating pane — every key is its: the moves, `enter`/`esc`/`m` end it, the rest said
    /// (no silent state change). False: nothing moves.
    pub fn float_move_key(&mut self, key: crate::key::Key) -> bool {
        use crate::key::Code;
        let Some(kind) = self.float_moving else { return false };
        // (the pane gone — its run closed, docked by a click: the mode with it)
        let Some(r) = self.float_rect(kind).filter(|_| self.floating_in_sight(kind)) else {
            self.float_moving = None;
            return false;
        };
        // (the window prefix goes on: `C-w m` again ends it, `C-w` hops)
        if key.ctrl && key.code == Code::Char('w') {
            return false;
        }
        let (dx, dy): (isize, isize) = match (key.code, key.ctrl || key.alt) {
            (Code::Char('h') | Code::Left, false) => (-1, 0),
            (Code::Char('l') | Code::Right, false) => (1, 0),
            (Code::Char('k') | Code::Up, false) => (0, -1),
            (Code::Char('j') | Code::Down, false) => (0, 1),
            (Code::Enter | Code::Esc | Code::Char('m'), false) => {
                self.float_moving = None;
                self.float_moved_said();
                return true;
            }
            _ => {
                self.key_unused(key, "moving a floating pane", "h j k l move it, enter done");
                return true;
            }
        };
        let (sw, sh) = self.float_screen();
        let x = r.x.saturating_add_signed(dx * MOVE_COLS as isize).min(sw.saturating_sub(r.w));
        let y = r.y.saturating_add_signed(dy * MOVE_ROWS as isize).min(sh.saturating_sub(r.h));
        self.float_set(kind, Rect { x, y, ..r });
        true
    }

    /// Every key and event (`focus_sync`): `C-w m` ends with its pane — gone from sight, silently (its run closed, docked)
    /// — or once the keys leave it (a hop, a click elsewhere): kept where it is and said, so `h j k l` and `esc` are the
    /// pane's with the keys again.
    pub(crate) fn float_move_sync(&mut self) {
        let Some(kind) = self.float_moving else { return };
        if !self.floating_in_sight(kind) {
            self.float_moving = None;
        } else if self.float_in_hand() != Some(kind) {
            self.float_moving = None;
            self.float_keep();
            self.set_status("moving ended — the keys left it; C-w m moves it again");
        }
    }

    /// Where the move left it, said — and kept with the layout (the session list's with the sizes).
    fn float_moved_said(&mut self) {
        self.float_keep();
        self.set_status("moved — C-w m moves it again, C-w = back to the middle");
    }

    /// `C-w < > - +` from a floating group's member: it narrower or wider by `STEP_COLS`, shorter or taller by
    /// `STEP_ROWS` — its left and top edges staying, within the screen. False: the keys aren't in a floating group.
    pub fn float_size(&mut self, dir: crate::split::Dir, more: bool) -> bool {
        let Some(kind) = self.float_in_hand() else { return false };
        let Some(r) = self.float_rect(kind) else { return false };
        let (sw, sh) = self.float_screen();
        let r = match dir {
            crate::split::Dir::Vertical => {
                let step = crate::sizes::STEP_COLS;
                let w = if more { r.w + step } else { r.w.saturating_sub(step) };
                // (wider than there's room right of it: it moves left)
                let w = w.clamp(crate::pane::FLOAT_MIN_W.min(sw), sw);
                Rect { w, x: r.x.min(sw - w), ..r }
            }
            crate::split::Dir::Horizontal => {
                let step = crate::sizes::STEP_ROWS;
                let h = if more { r.h + step } else { r.h.saturating_sub(step) };
                let h = h.clamp(crate::pane::FLOAT_MIN_H.min(sh), sh);
                // (a list as short as its rows: no shorter — never taller for `-`)
                if !more && h >= r.h {
                    self.say_size(format!("{} as short as it goes — C-w = back", kind.name()));
                    return true;
                }
                Rect { h, y: r.y.min(sh - h), ..r }
            }
        };
        self.float_set(kind, r);
        self.float_keep();
        self.say_size(format!("{} {} × {}", kind.name(), r.w, r.h));
        true
    }

    /// A frame moved or sized: kept — a group's with its layout, the session list's with the sizes.
    fn float_keep(&mut self) {
        self.layout_keep();
        self.sizes_keep();
    }

    /// `C-w =` from a floating group's member: back in the middle, 3/5 × 1/2 of the screen. False: not in one.
    pub fn float_auto(&mut self) -> bool {
        let Some(kind) = self.float_in_hand() else { return false };
        match kind {
            k if crate::floatlist::is_list(k) => self.float_list_frame_set(k, None),
            _ => {
                if let Some(g) = self.session.groups.iter_mut().find(|g| g.kind == kind) {
                    g.place = Place::Floating(Frame::CENTRED);
                    g.laid = crate::pane::Laid::Auto;
                }
            }
        }
        self.float_keep();
        self.say_size(format!("{} in the middle", kind.name()));
        true
    }

    /// What a press at (`x`, `y`) takes hold of on the topmost floating frame there, as last drawn: its top line off the
    /// names (it moves), its edges and bottom line (they size it) — None inside it, or on no frame.
    pub fn float_grip(&self, x: usize, y: usize) -> Option<(GroupKind, Grip)> {
        let (kind, r) = self.float_at(x, y)?;
        let names = self.screen.member_names.borrow().iter().any(|t| t.0 == y && (t.1..t.2).contains(&x))
            || self.screen.slot_tabs.borrow().iter().any(|t| t.0 == y && (t.1..t.2).contains(&x));
        let (left, right, bottom) = (x == r.x, x + 1 == r.x + r.w, y + 1 == r.y + r.h);
        match () {
            _ if y == r.y && !names && !left && !right => Some((kind, Grip::Move(x - r.x, 0))),
            _ if left || right || bottom => Some((kind, Grip::Size { left, right, bottom })),
            _ => None,
        }
    }

    /// The topmost floating pane drawn at (`x`, `y`) in the last frame: its kind and rect.
    pub fn float_at(&self, x: usize, y: usize) -> Option<(GroupKind, Rect)> {
        let drawn = self.screen.floating.borrow();
        drawn
            .iter()
            .rev()
            .find(|f| (f.1.x..f.1.x + f.1.w).contains(&x) && (f.1.y..f.1.y + f.1.h).contains(&y))
            .copied()
    }

    /// A floating frame dragged by `grip` to (`x`, `y`): moved with the press, or sized from the edges held — the
    /// others staying (kept when it's let go: `float_drag_done`).
    pub fn float_drag(&mut self, kind: GroupKind, grip: Grip, x: usize, y: usize) {
        let Some(r) = self.float_rect(kind) else { return };
        let (sw, sh) = self.float_screen();
        let r = match grip {
            Grip::Move(dx, dy) => Rect {
                x: x.saturating_sub(dx).min(sw.saturating_sub(r.w)),
                y: y.saturating_sub(dy).min(sh.saturating_sub(r.h)),
                ..r
            },
            Grip::Size { left, right, bottom } => {
                let (min_w, min_h) = (crate::pane::FLOAT_MIN_W.min(sw), crate::pane::FLOAT_MIN_H.min(sh));
                let mut r = r;
                if right {
                    r.w = (x + 1).saturating_sub(r.x).clamp(min_w, sw - r.x);
                }
                if left {
                    let end = r.x + r.w;
                    let x = x.min(end.saturating_sub(min_w));
                    (r.x, r.w) = (x, end - x);
                }
                if bottom {
                    r.h = (y + 1).saturating_sub(r.y).clamp(min_h, sh - r.y);
                }
                r
            }
        };
        self.float_set(kind, r);
    }

    /// The press let go: where it is kept, and said.
    pub fn float_drag_done(&mut self, kind: GroupKind, grip: Grip) {
        self.float_keep();
        if let (Grip::Size { .. }, Some(r)) = (grip, self.float_rect(kind)) {
            self.say_size(format!("{} {} × {}", kind.name(), r.w, r.h));
        }
    }
}

/// A floating list's name where it's said (`the session list floats`).
fn list_name(kind: GroupKind) -> &'static str {
    match kind {
        GroupKind::Messages => "messages",
        _ => "session",
    }
}

#[cfg(test)]
mod tests;
