//! Sizes you set (U1): the chat's and the side panel's widths and the bottom slot's height. Global, not per
//! project: kept as columns and rows in `$state/sizes.json`, read at startup (so across `:restart` too). A
//! screen too small for a kept size shows less of it — clamped here, never written back — and a bigger one
//! gets it again. Unset, each is automatic: the chat 2/5 of the screen (36–96 columns — U3), the side panel
//! 32 and more on a wide screen, the bottom slot what its panel wants (a task's output — U2).
//!
//! `C-w <`/`>` narrow/widen the focused one (the chat, the side panel; from the code: the chat if it's open,
//! else the side panel), `C-w -`/`+` the bottom slot, `C-w =` makes them all automatic again. A split of the
//! code's you made sizes its panes instead (panes P2b, §5.4): from a code pane with another beside it that way,
//! `C-w < > - +` and a drag of the divider between them set that pane's share in the tree (`Share::Cells` — the
//! session's, not these global three); `C-w =` makes them even again.
//!
//! Beside each other (140 columns: side panel 32 + chat 56 left the code 52): an automatic chat narrows to
//! leave the code `CODE_ROOM` (down to `CHAT_MIN`); if the code would still have less than `CODE_MIN`, the side
//! panel gives way, whole (it comes back when there's room — its keys go back to the code meanwhile).
//! Beside the code review's rail an automatic chat narrows further, to leave the code `REVIEW_CODE_ROOM`
//! (`chat_beside`).
//!
//! `C-w z` fills the screen with what has the keys — the code (its pane alone), the bottom slot, the chat, the
//! side panel — and back (`Zoom`); it ends when the keys go elsewhere. The bottom slot takes the keys with
//! `C-w j` or a click: they're its panel's view mode (`view.rs` — `q` closes it, Esc or `C-w k` back to the
//! code).

use crate::session::At;

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

/// The chat's width bounds — automatic up to `CHAT_AUTO_MAX` (U3), set by you up to what leaves the code
/// `CHAT_FULL_CODE` (below that the chat takes the whole width, as on a narrow screen).
pub const CHAT_MIN: usize = 36;
pub const CHAT_AUTO_MAX: usize = 96;
pub const CHAT_FULL_CODE: usize = 40;
/// The side panel's: automatic `SIDE_AUTO` (+ up to `SIDE_MORE` on a wide screen, the code keeping 100).
pub const SIDE_MIN: usize = 20;
pub const SIDE_MAX: usize = 72;
pub const SIDE_AUTO: usize = 32;
pub const SIDE_MORE: usize = 12;
/// The code beside them: an automatic chat narrows to leave it this much; under `CODE_MIN` the side panel
/// gives way.
pub const CODE_ROOM: usize = 60;
pub const CODE_MIN: usize = 48;
/// The code beside the review's rail: an automatic chat narrows to leave it this much (`chat_beside`).
pub const REVIEW_CODE_ROOM: usize = 100;
/// The bottom slot: at least this many rows, and the code keeps `BOTTOM_CODE` above it.
pub const BOTTOM_MIN: usize = 3;
pub const BOTTOM_CODE: usize = 4;
/// How a size's toast ends (the next one replaces it — `say_size`).
const SIZE_TAIL: &str = " · C-w = automatic";
/// One `C-w <`/`>` · `C-w -`/`+`.
pub const STEP_COLS: usize = 4;
pub const STEP_ROWS: usize = 2;

/// What you set (None: automatic).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sizes {
    pub chat: Option<u16>,
    pub side: Option<u16>,
    pub bottom: Option<u16>,
    /// A docked group's, per kind (`GroupKind::dock_index`): its width beside the code, its rows above it.
    pub docked: [Option<u16>; 4],
    /// The session list's frame (`sessionpane.rs`) — where you put it, the viewer's (None: centred).
    pub sessions: Option<crate::pane::Frame>,
    /// `:messages`' frame, as the session list's.
    pub messages: Option<crate::pane::Frame>,
}

/// The dockable kinds by `GroupKind::dock_index`, as `sizes.json` names them.
const DOCKED: [crate::pane::GroupKind; 4] = [
    crate::pane::GroupKind::Tests,
    crate::pane::GroupKind::Task,
    crate::pane::GroupKind::Preflight,
    crate::pane::GroupKind::Procs,
];

impl Sizes {
    pub fn path() -> Option<PathBuf> {
        crate::config::state_dir().map(|d| d.join("sizes.json"))
    }

    /// Read from `path` (missing or unreadable: all automatic).
    pub fn read(path: Option<&Path>) -> Sizes {
        let Some(v) = path
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            return Sizes::default();
        };
        let n = |v: &Value| v.as_u64().and_then(|n| u16::try_from(n).ok());
        let docked = DOCKED.map(|k| n(&v["docked"][k.name()]));
        let (sessions, messages) =
            (crate::pane::Frame::of_json(&v["sessions"]), crate::pane::Frame::of_json(&v["messages"]));
        Sizes {
            chat: n(&v["chat"]),
            side: n(&v["side"]),
            bottom: n(&v["bottom"]),
            docked,
            sessions,
            messages,
        }
    }

    pub fn to_json(self) -> String {
        let docked: serde_json::Map<String, Value> = DOCKED
            .iter()
            .filter_map(|k| Some((k.name().to_string(), json!(self.docked[k.dock_index()]?))))
            .collect();
        let (sessions, messages) =
            (self.sessions.map(crate::pane::Frame::json), self.messages.map(crate::pane::Frame::json));
        json!({ "chat": self.chat, "side": self.side, "bottom": self.bottom, "docked": docked, "sessions": sessions,
                "messages": messages })
        .to_string()
    }
}

/// Written on a worker: the last one asked for wins (an older write that finishes later is skipped) — counted
/// per file.
pub fn save(path: &Path, sizes: Sizes, seq: u64) {
    use std::sync::Mutex;
    static LAST: Mutex<Vec<(PathBuf, u64)>> = Mutex::new(Vec::new());
    let Ok(mut last) = LAST.lock() else { return };
    match last.iter_mut().find(|(p, _)| p == path) {
        Some((_, n)) if seq < *n => return,
        Some((_, n)) => *n = seq,
        None => last.push((path.to_path_buf(), seq)),
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, sizes.to_json()).is_ok() {
        let _ = std::fs::rename(&tmp, path);
    }
}

/// The chat's and the side panel's widths on a screen `total` columns wide.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Widths {
    pub chat: usize,
    pub side: usize,
}

/// The chat's width beside a panel `side` columns wide that keeps its own width — the code review's rail
/// (frictions25 R6: beside it the chat kept its 2/5 and the code read ~45 columns, wrapping): as beside the side
/// panel, an automatic chat narrows to leave the code its room — a review's, `REVIEW_CODE_ROOM`: reading the
/// code is what a review is for — down to `CHAT_MIN`; one you set stays, and so does a chat taking the whole
/// width.
pub fn chat_beside(total: usize, side: usize, kept: Sizes) -> usize {
    let chat = widths(total, true, false, kept).chat;
    if kept.chat.is_some() || chat >= total {
        return chat;
    }
    total.saturating_sub(side + REVIEW_CODE_ROOM).max(CHAT_MIN).min(chat)
}

/// How wide the chat (`chat_open`: shown as a panel) and the side panel (`side_open`) are — kept sizes
/// clamped to the screen, automatic ones as the module says.
pub fn widths(total: usize, chat_open: bool, side_open: bool, kept: Sizes) -> Widths {
    let mut chat = match (chat_open, kept.chat) {
        (false, _) => 0,
        (true, Some(k)) => (k as usize).clamp(CHAT_MIN, total.saturating_sub(CHAT_FULL_CODE).max(CHAT_MIN)),
        (true, None) => (total * 2 / 5).clamp(CHAT_MIN, CHAT_AUTO_MAX),
    };
    // Too narrow for the chat and code side by side: the chat takes it all
    if chat_open && total < chat + CHAT_FULL_CODE {
        return Widths { chat: total, side: 0 };
    }
    if !side_open {
        return Widths { chat, side: 0 };
    }
    let auto_side = |edit: usize| SIDE_AUTO + (edit.saturating_sub(SIDE_AUTO + 100) / 2).min(SIDE_MORE);
    let side_of = |chat: usize| match kept.side {
        Some(k) => (k as usize).clamp(SIDE_MIN, SIDE_MAX),
        None => auto_side(total - chat),
    };
    let mut side = side_of(chat);
    // An automatic chat makes room for the code beside the panel
    if chat_open && kept.chat.is_none() && total < chat + side + CODE_ROOM {
        chat = total.saturating_sub(side + CODE_ROOM).max(CHAT_MIN).min(chat);
        side = side_of(chat);
    }
    // At its width it would leave the code less than CODE_MIN: it gives way (not squeezed), the chat as it
    // would be without it
    if total < chat + side + CODE_MIN {
        return widths(total, chat_open, false, kept);
    }
    Widths { chat, side }
}

/// The bottom slot's height in `total` rows (code and slot): a kept size clamped, else what its panel wants.
pub fn bottom(total: usize, kept: Option<u16>, natural: usize) -> usize {
    let max = total.saturating_sub(BOTTOM_CODE);
    match kept {
        Some(k) if max >= BOTTOM_MIN => (k as usize).clamp(BOTTOM_MIN, max),
        Some(_) => 0,
        None => natural,
    }
}

/// A border the mouse drags: the chat's left edge, the side panel's right edge, the bottom slot's top row, the
/// divider right of a code pane with another beside it (the pane's id).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Border {
    Chat,
    Side,
    Bottom,
    Divider(crate::split::PaneId),
    /// A docked group's edge (P3 step 6), by its kind: its width, or its rows over the code.
    Leaf(crate::pane::GroupKind),
}

/// A code pane keeps at least this many columns and rows when its split is sized.
pub const PANE_MIN_COLS: usize = 12;
pub const PANE_MIN_ROWS: usize = 3;

/// What `C-w <`/`>` sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Which {
    Chat,
    Side,
}

/// What `C-w z` fills the screen with: a pane — a code pane alone, the bottom slot, the chat, the side panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Zoom(pub crate::split::PaneId);

/// What the bottom slot shows — one at a time: `:procs` while it's open (asked for just now), the debugger,
/// then the terminal (when shown), a plugin's panel, and the test run or task started last.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Procs,
    Debug,
    Terminal,
    Plugin,
    Test,
    Task,
}

impl crate::editor::Editor {
    /// Whether `q` closes the test or task panel now: only once its run has ended — one under way (or a rebase
    /// stopped at its conflicts) is stopped by its own `space x c` / `space m c`, never by a `q` meant for a pager.
    pub fn q_closes(&self, slot: Slot) -> bool {
        use crate::task::State;
        match slot {
            Slot::Test => {
                self.session.test_run.as_ref().is_some_and(|r| r.state != crate::testing::RunState::Running)
            }
            Slot::Task => self
                .session
                .task_run
                .as_ref()
                .is_some_and(|r| matches!(r.state, State::Passed | State::Failed(_))),
            Slot::Procs => self.procs_up(),
            _ => false,
        }
    }

    /// `q` in a list or output panel with tarae's keys, and `C-w q` there: it closes — the terminal's hides as
    /// `space T` hides it (the shell runs on), a test's or task's once its run has ended (else said, the run
    /// untouched), `:procs`.
    pub fn slot_close(&mut self, slot: Slot) {
        match slot {
            Slot::Terminal => {
                if let Some(t) = self.session.terminal.as_mut() {
                    t.visible = false;
                }
                self.session.leave(At::Program);
                self.session.leave(At::Slot);
            }
            // A rebase stopped at its conflicts isn't running: it waits on them, and its panel with it — `space m c`
            // isn't the way out there (it drops the run the rebase is), `:rebase-abort` is
            Slot::Task
                if let Some(crate::task::State::Stopped(why)) =
                    self.session.task_run.as_ref().map(|r| &r.state) =>
            {
                let why = why.clone();
                self.set_status(format!(
                    "the rebase waits at {why} — its panel stays until it ends (:rebase-abort ends it)"
                ));
            }
            Slot::Test | Slot::Task if !self.q_closes(slot) => {
                let (what, stop) = match slot {
                    Slot::Test => ("the tests are", "space x c"),
                    _ => ("the task is", "space m c"),
                };
                self.set_status(format!("{what} still running — {stop} stops and closes"));
            }
            Slot::Test => {
                self.test_close();
                self.session.leave(At::Slot);
            }
            Slot::Task => {
                self.task_close();
                self.session.leave(At::Slot);
            }
            Slot::Procs => self.procs_close(),
            Slot::Debug | Slot::Plugin => {}
        }
    }

    /// `C-w q`: what has the keys closes, as its own `q` closes it — the side panel (a plugin's tab in it too:
    /// its `q` is tarae's, the side panel goes and the tab stays the plugin's), the review's rail (the review
    /// ends, asked first), `:procs`, the test, task and terminal panels. False: the code has them (its pane
    /// closes). Not the chat (closing it ends its conversation's process: a slip of two keys mustn't cut a turn
    /// off — `space L` does it) nor a plugin's bottom panel (its `q` is the plugin's, and only the plugin closes
    /// it — tarae has no event to tell it so): said.
    pub fn close_focused(&mut self) -> bool {
        use crate::uistate::Region;
        match self.keys_to().region {
            Region::Side(_) => self.session.sidebar = None,
            Region::Rail => crate::codereview::close_asked(self),
            Region::Slot(Slot::Plugin) => self.set_status("a plugin's panel closes by its own keys"),
            Region::Slot(s) => self.slot_close(s),
            Region::List(k) => crate::grouplist::close(self, k),
            Region::View(crate::view::Surface::Terminal) => self.slot_close(Slot::Terminal),
            Region::Output(r) => self.slot_close(r.slot()),
            Region::Chat | Region::Unfold | Region::View(crate::view::Surface::Chat) => {
                self.set_status("C-w q closes panels — space L closes the chat")
            }
            _ => return false,
        }
        true
    }
}

impl crate::editor::Editor {
    /// What `C-w <`/`>` acts on: the chat or the side panel, whichever has the keys — from the code, the
    /// chat if it's shown as a panel, else the side panel.
    fn sized(&self) -> Option<Which> {
        let chat = self.screen.chat_w > 0 && self.session.chat.as_ref().is_some_and(|c| !c.compact());
        let side = self.session.sidebar.is_some() && !self.review_rail_up();
        if chat && self.session.is_at(At::Chat) {
            return Some(Which::Chat);
        }
        if side && self.session.is_at(At::Side) {
            return Some(Which::Side);
        }
        if chat { Some(Which::Chat) } else { side.then_some(Which::Side) }
    }

    /// How wide the chat can be on this screen: beside the side panel no wider than leaves the code
    /// `CODE_MIN` (else the panel would go), alone what leaves it `CHAT_FULL_CODE`.
    fn chat_max(&self, total: usize) -> usize {
        match self.screen.side_w {
            0 => total.saturating_sub(CHAT_FULL_CODE),
            side => total.saturating_sub(side + CODE_MIN),
        }
    }

    /// How wide the side panel can be: what leaves the code `CODE_MIN` beside the chat.
    fn side_max(&self, total: usize) -> usize {
        total.saturating_sub(self.screen.chat_w + CODE_MIN).min(SIDE_MAX)
    }

    /// How tall the bottom slot can be: the code keeps `BOTTOM_CODE`.
    fn bottom_max(&self) -> usize {
        self.screen.body_h.saturating_sub(BOTTOM_CODE).max(BOTTOM_MIN)
    }

    /// The border at screen cell (`x`, `y`) — as last drawn: the gutter left of the chat, the one right of
    /// the side panel (both the whole height — `chrome.rs`), the bottom panel's top row between them (not
    /// its tabs — a click there brings that tab in front).
    pub fn border_at(&self, x: usize, y: usize) -> Option<Border> {
        // (one thing fills the screen: no borders)
        if self.zoomed().is_some() {
            return None;
        }
        let s = &self.screen;
        let total = s.width?;
        let end = s.text_top + s.body_h;
        let bottom_y = end - s.bottom_h;
        if s.chat_w > 0 && s.chat_w < total && x == total - s.chat_w && y < end {
            return Some(Border::Chat);
        }
        if s.side_w > 0 && x + 1 == s.side_w && y < end {
            return Some(Border::Side);
        }
        let tab = || {
            s.slot_tabs.borrow().iter().any(|t| t.0 == y && (t.1..t.2).contains(&x))
                || s.member_names.borrow().iter().any(|t| t.0 == y && (t.1..t.2).contains(&x))
        };
        // (a docked group's edge: its line toward the code)
        if let Some(kind) = self.dock_edge_at(x, y) {
            return Some(Border::Leaf(kind));
        }
        if s.bottom_h > 0
            && y == bottom_y
            && x >= s.slot_x
            && x < s.slot_end().min(total - s.chat_w)
            && !tab()
        {
            return Some(Border::Bottom);
        }
        // (between two code panes side by side: the cell after the left one's last, the right one starting past it)
        let code = |id: &crate::split::PaneId| self.session.views.iter().any(|v| v.id == *id);
        let left = s.leaves.iter().filter(|l| code(&l.0)).find(|(_, r)| {
            r.x + r.w == x
                && (r.y..r.y + r.h).contains(&y)
                && s.leaves.iter().any(|(id, b)| code(id) && b.x == x + 1 && (b.y..b.y + b.h).contains(&y))
        });
        left.map(|l| Border::Divider(l.0))
    }

    /// A split of yours around the code pane `id` along `dir`: its child holding the pane, that child's size along
    /// `dir` as last laid out, and the room it and its siblings that aren't a surface's own share (None: no other
    /// code pane beside it that way — the surfaces' sizes are sized instead).
    fn own_split(&self, id: crate::split::PaneId, dir: crate::split::Dir) -> Option<(usize, usize, usize)> {
        use crate::split::{Dir, Share, Split};
        let mut tree = self.session.split.clone();
        let (kids, i) = tree.parent_along(id, dir)?;
        let along = |s: &Split| {
            let rects: Vec<crate::split::Rect> =
                self.screen.leaves.iter().filter(|l| s.holds(l.0)).map(|l| l.1).collect();
            let (from, to) = match dir {
                Dir::Vertical => (rects.iter().map(|r| r.x).min()?, rects.iter().map(|r| r.x + r.w).max()?),
                Dir::Horizontal => (rects.iter().map(|r| r.y).min()?, rects.iter().map(|r| r.y + r.h).max()?),
            };
            Some(to - from)
        };
        let others: Vec<usize> = (0..kids.len()).filter(|&j| j != i && kids[j].0 != Share::Auto).collect();
        if others.is_empty() {
            return None;
        }
        let now = along(&kids[i].1)?;
        let room = now + others.iter().filter_map(|&j| along(&kids[j].1)).sum::<usize>();
        Some((i, now, room.saturating_sub(others.len() * Self::pane_min(dir))))
    }

    fn pane_min(dir: crate::split::Dir) -> usize {
        match dir {
            crate::split::Dir::Vertical => PANE_MIN_COLS,
            crate::split::Dir::Horizontal => PANE_MIN_ROWS,
        }
    }

    /// The code pane `id`'s child in its split along `dir` set to `n` cells (within its room) — said, the tree kept
    /// with the session.
    fn pane_size(
        &mut self,
        id: crate::split::PaneId,
        dir: crate::split::Dir,
        n: usize,
        room: usize,
    ) -> usize {
        let n = n.clamp(Self::pane_min(dir), room.max(Self::pane_min(dir)));
        if let Some((kids, i)) = self.session.split.parent_along(id, dir) {
            kids[i].0 = crate::split::Share::Cells(n as u16);
        }
        n
    }

    /// `C-w < > - +` from a code pane with another beside it that way: that pane `STEP_COLS`/`STEP_ROWS` bigger or
    /// smaller. False: no split of yours that way — the surfaces' sizes are the keys'.
    fn size_own(&mut self, dir: crate::split::Dir, more: bool) -> bool {
        let id = self.session.focus;
        if !self.session.is_at(At::Code) {
            return false;
        }
        let Some((_, now, room)) = self.own_split(id, dir) else { return false };
        let step = if dir == crate::split::Dir::Vertical { STEP_COLS } else { STEP_ROWS };
        let to = if more { now + step } else { now.saturating_sub(step) };
        let n = self.pane_size(id, dir, to, room);
        let unit = if dir == crate::split::Dir::Vertical { "columns" } else { "rows" };
        self.say_size(format!("pane {n} {unit}"));
        true
    }

    /// A border dragged to (`x`, `y`): the size it makes, within what this screen allows (kept when the
    /// press is let go — `border_done`).
    pub fn border_drag(&mut self, b: Border, x: usize, y: usize) {
        let Some(total) = self.screen.width else { return };
        let s = &self.screen;
        // (nothing said while it moves — the edge, in the accent, is there to see; letting go says it)
        match b {
            Border::Chat => {
                let cols = total.saturating_sub(x).clamp(CHAT_MIN, self.chat_max(total).max(CHAT_MIN));
                self.sizes.chat = Some(cols as u16);
            }
            Border::Side => {
                let cols = (x + 1).clamp(SIDE_MIN, self.side_max(total).max(SIDE_MIN));
                self.sizes.side = Some(cols as u16);
            }
            Border::Bottom => {
                let rows = (s.text_top + s.body_h).saturating_sub(y).clamp(BOTTOM_MIN, self.bottom_max());
                self.sizes.bottom = Some(rows as u16);
            }
            // (a docked group's edge: the far side of it stays)
            Border::Leaf(kind) => {
                if let Some((side, r)) = self.dock_rect(kind) {
                    let n = match side {
                        crate::pane::Side::Right => (r.x + r.w).saturating_sub(x),
                        crate::pane::Side::Left => (x + 1).saturating_sub(r.x),
                        // (its edge the code's line under its last row)
                        crate::pane::Side::Top => y.saturating_sub(r.y),
                    };
                    self.dock_size(kind, n);
                }
            }
            // (the divider at column `x`: the left pane's child ends there)
            Border::Divider(id) => {
                let dir = crate::split::Dir::Vertical;
                let Some((_, now, room)) = self.own_split(id, dir) else { return };
                let Some(r) = s.leaves.iter().find(|l| l.0 == id).map(|l| l.1) else { return };
                // (the child's left edge: its width now back from the pane's right edge)
                let start = (r.x + r.w).saturating_sub(now);
                self.pane_size(id, dir, x.saturating_sub(start), room);
            }
        }
    }

    /// The press let go: what it set is kept, and said.
    pub fn border_done(&mut self, b: Border) {
        self.sizes_keep();
        let (what, n, unit) = match b {
            Border::Chat => ("chat", self.sizes.chat, "columns"),
            Border::Side => ("side panel", self.sizes.side, "columns"),
            Border::Bottom => ("bottom panel", self.sizes.bottom, "rows"),
            Border::Leaf(kind) => {
                let unit = match self.session.group(kind).map(|g| g.place) {
                    Some(crate::pane::Place::Docked(crate::pane::Side::Top)) => "rows",
                    _ => "columns",
                };
                (kind.name(), self.sizes.docked[kind.dock_index()], unit)
            }
            Border::Divider(id) => {
                let mut tree = self.session.split.clone();
                let n = match tree.parent_along(id, crate::split::Dir::Vertical) {
                    Some((kids, i)) => match kids[i].0 {
                        crate::split::Share::Cells(n) => Some(n),
                        _ => None,
                    },
                    None => None,
                };
                ("pane", n, "columns")
            }
        };
        if let Some(n) = n {
            self.say_size(format!("{what} {n} {unit}"));
        }
    }

    /// A size said in a toast — one at a time: the one before it goes (a few presses of `C-w >` are one
    /// message that changes, not a stack of them).
    pub(crate) fn say_size(&mut self, msg: String) {
        self.toasts.retain(|t| !t.text.ends_with(SIZE_TAIL));
        self.set_status(format!("{msg}{SIZE_TAIL}"));
    }

    /// `C-w >` / `C-w <` — the chat or the side panel `STEP_COLS` wider/narrower, from its width on screen;
    /// as far as leaves the code its room. Kept (global) — `C-w =` makes it automatic again.
    pub fn size_width(&mut self, wider: bool) {
        if self.float_size(crate::split::Dir::Vertical, wider)
            || self.size_own(crate::split::Dir::Vertical, wider)
            || self.size_dock(crate::split::Dir::Vertical, wider)
        {
            return;
        }
        let Some(total) = self.screen.width else { return };
        let Some(which) = self.sized() else {
            return self.set_status("nothing to widen — the chat (space l) and the side panel (space e) are");
        };
        let (chat, side) = (self.screen.chat_w, self.screen.side_w);
        let step = |now: usize, min: usize, max: usize| {
            let to = if wider { now + STEP_COLS } else { now.saturating_sub(STEP_COLS) };
            to.clamp(min, max.max(min))
        };
        // From the size you set (as much of it as this screen shows — two presses before a frame count twice),
        // else from what's on screen
        let from =
            |kept: Option<u16>, shown: usize, max: usize| kept.map_or(shown, |k| (k as usize).min(max));
        let (what, cols) = match which {
            Which::Chat => {
                let max = self.chat_max(total);
                let cols = step(from(self.sizes.chat, chat, max), CHAT_MIN, max);
                self.sizes.chat = Some(cols as u16);
                ("chat", cols)
            }
            Which::Side => {
                // (folded away: from the size it would have)
                let max = self.side_max(total);
                let shown = if side > 0 { side } else { SIDE_AUTO };
                let cols = step(from(self.sizes.side, shown, max), SIDE_MIN, max);
                self.sizes.side = Some(cols as u16);
                ("side panel", cols)
            }
        };
        self.sizes_keep();
        self.say_size(format!("{what} {cols} columns"));
    }

    /// `C-w +` / `C-w -` — the bottom slot `STEP_ROWS` taller/shorter (the code keeping `BOTTOM_CODE`).
    pub fn size_height(&mut self, taller: bool) {
        if self.float_size(crate::split::Dir::Horizontal, taller)
            || self.size_own(crate::split::Dir::Horizontal, taller)
            || self.size_dock(crate::split::Dir::Horizontal, taller)
        {
            return;
        }
        let now = self.screen.bottom_h;
        if now == 0 {
            return self.set_status("no panel at the bottom — tests, tasks and the terminal show there");
        }
        let max = self.bottom_max();
        let now = self.sizes.bottom.map_or(now, |k| (k as usize).min(max));
        let rows =
            if taller { now + STEP_ROWS } else { now.saturating_sub(STEP_ROWS) }.clamp(BOTTOM_MIN, max);
        self.sizes.bottom = Some(rows as u16);
        self.sizes_keep();
        self.say_size(format!("bottom panel {rows} rows"));
    }

    /// The side panel is open but folded away: on this screen (as last drawn) it would leave the code too
    /// little (`widths`). Not before the first frame, not while a review's rail holds its place.
    pub fn side_folded(&self) -> bool {
        let Some(total) = self.screen.width else { return false };
        let chat_open = self.chat_panel(total);
        self.session.sidebar.is_some()
            && !self.review_rail_up()
            && widths(total, chat_open, true, self.sizes).side == 0
    }

    /// The chat is a panel on a screen `total` wide: open, not minimized — and where it would take the whole
    /// width (too narrow for it beside the code), only while it has the keys. Out of the chat (a card
    /// answered, Esc, an edit's review) they went to a code pane with no room on screen: `keys → pane#29`,
    /// typing landing unseen. What has the keys fills a narrow screen, the code too; `space l` brings the chat.
    /// A card that took them (`C-q` — #201: in 50 columns its keys went to a card nobody could see) has them,
    /// not the chat: the code is there, with the card on it; Esc or its answer brings the chat back.
    pub fn chat_panel(&self, total: usize) -> bool {
        if !self.session.chat.as_ref().is_some_and(|c| !c.compact()) {
            return false;
        }
        (self.session.is_at(At::Chat) && !self.card_has_keys())
            || widths(total, true, false, self.sizes).chat < total
    }

    /// `C-w =` — every size automatic again, and how each group lays its members (`C-w t`'s).
    pub fn sizes_auto(&mut self) {
        // (from a floating pane: it alone, back in the middle)
        if self.float_auto() {
            return;
        }
        self.sizes =
            Sizes { sessions: self.sizes.sessions, messages: self.sizes.messages, ..Sizes::default() };
        self.session.split.even();
        for g in &mut self.session.groups {
            g.laid = crate::pane::Laid::Auto;
        }
        self.sizes_keep();
        self.toasts.retain(|t| !t.text.ends_with(SIZE_TAIL));
        self.set_status("sizes automatic again");
    }

    /// The run panel in hand: a docked group's while one of its members has the keys (`slot_in_hand`), else what
    /// the bottom slot shows (`slot_shown`) — what `At::Slot`'s keys act on.
    pub fn slot(&self) -> Option<Slot> {
        self.slot_in_hand().or_else(|| self.slot_shown())
    }

    /// A docked group's panel while its list or its detail has the keys (P3 §3.3: where a group is doesn't change
    /// what its keys do — `At::List`, `At::Slot` as in the slot).
    pub fn slot_in_hand(&self) -> Option<Slot> {
        let kind = self.session.hand?;
        let g = self.session.group(kind)?;
        let keys = self.session.is_at(At::Slot) || self.session.is_at(At::List(g.list));
        keys.then_some(()).filter(|_| self.out_of_slot(kind)).and(kind.slot())
    }

    /// What the bottom slot shows, if anything: the panel put in front (`chrome.rs` — a tab, one just
    /// opened), else the first there of `:procs`, the debugger, the terminal, a plugin's panel, the test run
    /// or task started last — never one whose group is docked in sight (P3 §3.1: it's a pane of its own).
    pub fn slot_shown(&self) -> Option<Slot> {
        let free = |s: &Slot| !self.slot_docked(*s);
        if let Some(s) = self.slot_front().filter(free) {
            return Some(s);
        }
        if self.procs_up() && free(&Slot::Procs) {
            return Some(Slot::Procs);
        }
        if self.dap.is_some() {
            return Some(Slot::Debug);
        }
        if self.session.terminal.as_ref().is_some_and(|t| t.visible) {
            return Some(Slot::Terminal);
        }
        if self.plugins.as_ref().is_some_and(|p| p.panel.is_some()) {
            return Some(Slot::Plugin);
        }
        let test = self.session.test_run.as_ref().filter(|_| free(&Slot::Test));
        let task = self.session.task_run.as_ref().filter(|_| free(&Slot::Task));
        match (test, task) {
            (Some(t), Some(k)) if k.started > t.started => Some(Slot::Task),
            (Some(_), _) => Some(Slot::Test),
            (None, Some(_)) => Some(Slot::Task),
            (None, None) => None,
        }
    }

    /// Whether the bottom slot has the keys — tarae's (`At::Slot`), or the terminal's program, or a
    /// plugin panel's.
    pub fn bottom_has_keys(&self) -> bool {
        // (a docked group's are its own pane's, beside the code)
        if self.slot_in_hand().is_some() {
            return false;
        }
        // (a group's list in the slot is the slot's: the tests', a task's places, a preflight's steps, `:procs`')
        let listing = || self.slot_group().is_some_and(|g| self.session.is_at(At::List(g.list)));
        match self.slot() {
            Some(Slot::Terminal) => self.session.is_at(At::Slot) || self.session.is_at(At::Program),
            Some(Slot::Plugin) => self.plugin_panel_focused(),
            Some(Slot::Procs | Slot::Test | Slot::Task) => self.session.is_at(At::Slot) || listing(),
            Some(Slot::Debug) | None => false,
        }
    }

    /// `C-w j` below the lowest pane, or a click on the panel: the keys to the bottom slot (on the terminal,
    /// tarae's — `i` then types in it). The debugger's panel takes none (its keys work from the code).
    pub fn bottom_take_keys(&mut self) {
        self.session.hand = self.hand_of(self.slot_shown());
        match self.slot_shown() {
            None => self.set_status("nothing at the bottom — tests, tasks and the terminal show there"),
            Some(Slot::Debug) => {
                self.set_status("the debug panel follows the code — its keys work from there")
            }
            Some(Slot::Plugin) => self.plugin_panel_click(None),
            Some(Slot::Procs) => self.procs_click(None),
            Some(_) => {
                self.session.at = At::Slot;
                // (the terminal's: its view mode, at once — what has the keys is said before the next key; a run
                // panel's output, its cursor landed in sight)
                crate::view::sync(self);
                crate::rundoc::sync(self);
            }
        }
    }

    /// What has the keys, as `C-w z` would fill the screen with it — None: the review's file list (it keeps
    /// its width).
    pub fn focused_area(&self) -> Option<Zoom> {
        use crate::pane::Shows;
        let s = &self.session;
        let shows = if s.is_at(At::Chat) && s.chat.as_ref().is_some_and(|c| !c.compact()) {
            Shows::Chat
        } else if self.review_rail_up() && s.is_at(At::Rail) && s.code_review.is_some() {
            return None;
        } else if !self.review_rail_up() && s.is_at(At::Side) && s.sidebar.is_some() {
            Shows::Side
        } else if let Some(g) = self.slot_in_hand().and(s.hand).and_then(|k| s.group(k)) {
            return Some(Zoom(g.host));
        } else if let Some(p) = self.float_list_in_hand().and_then(|k| self.float_list(k)) {
            return Some(Zoom(p.list));
        } else if self.bottom_has_keys() {
            Shows::Slot
        } else {
            Shows::Code
        };
        s.leaf(shows).map(Zoom)
    }

    /// What fills the screen now: `C-w z`'s, while it still has the keys.
    pub fn zoomed(&self) -> Option<Zoom> {
        self.session.zoom.filter(|z| Some(*z) == self.focused_area())
    }

    /// What the pane filling the screen shows (`zoomed`'s).
    pub fn zoomed_shows(&self) -> Option<crate::pane::Shows> {
        self.zoomed().and_then(|z| self.session.shows(z.0))
    }

    /// `C-w z` — what has the keys fills the screen; again, back as it was.
    pub fn zoom_toggle(&mut self) {
        if self.zoomed().is_some() {
            self.session.zoom = None;
            return;
        }
        let Some(z) = self.focused_area() else {
            return self.set_status("the review's file list keeps its width — C-w z from the code");
        };
        let s = &self.screen;
        let alone = self.session.views.len() == 1
            && s.chat_w == 0
            && s.side_w == 0
            && s.bottom_h == 0
            && !(self.session.code_review.is_some() && self.review_rail_up());
        if self.session.shows(z.0) == Some(crate::pane::Shows::Code) && alone {
            return self.set_status("the code has the whole screen already");
        }
        self.session.zoom = Some(z);
    }

    /// Every key and event: the keys are still where `Session.at` says — what it names is still there (the
    /// chat open, the terminal shown and running, the slot showing a panel that takes them), else they're the
    /// code's; `C-w z` ends once what it filled the screen with doesn't have them.
    pub fn focus_sync(&mut self) {
        self.slot_sync();
        self.groups_sync();
        self.dock_sync();
        // (a floating group's own pane, out of the tree; the one in hand on top; `C-w m` ends with its pane)
        self.float_groups_sync();
        self.float_raise();
        // (a docked group folded into the slot puts its panel in front first, while the keys are still in it — judged
        // against an older front, its list or output wasn't there and they fell to the code)
        self.hand_sync();
        let s = &self.session;
        let there = match s.docked() {
            At::Code | At::Float(_) => true,
            At::Chat => s.chat.is_some(),
            At::Program => s.terminal.as_ref().is_some_and(|t| t.visible && t.exited.is_none()),
            At::Slot => matches!(self.slot(), Some(Slot::Terminal | Slot::Test | Slot::Task | Slot::Procs)),
            At::List(id) => {
                s.groups.iter().any(|g| g.list == id && self.group_laid(g))
                    || self.float_list_of(id).is_some()
            }
            At::Plugin => self.plugins.as_ref().is_some_and(|p| p.panel.is_some()),
            At::Side => s.sidebar.is_some(),
            At::Rail => s.code_review.is_some(),
        };
        if !there {
            self.session.leave(self.session.docked());
            // (the keys gone from its group: the hand let go)
            self.hand_sync();
        }
        // (`C-w m` ends with its pane, or once the keys leave it — a hop, a click elsewhere)
        self.float_move_sync();
        self.groups_front();
        // `:procs`' panel gives the slot to the terminal given the keys (`C-\`), which it covered
        if self.procs_up() && self.session.program_has_keys() {
            self.procs_close();
        }
        // (a view whose surface lost the keys ends; the terminal given tarae's keys is in its view mode; the run
        // panels' documents follow their runs, the keys coming or going taken in)
        crate::view::sync(self);
        crate::rundoc::sync(self);
        if self.session.zoom.is_some() && self.zoomed().is_none() {
            self.session.zoom = None;
        }
        // (a focused toast lets go once something else takes the keys, or goes with its card)
        self.toast_sync();
        // (a card, the picker, a prompt, which-key: the keys' while up, the docked pane's again once gone)
        self.float_sync();
    }

    /// Writes the sizes (worker — the newest write wins).
    pub fn sizes_keep(&mut self) {
        let Some(path) = self.sizes_path.clone() else { return };
        self.sizes_seq += 1;
        let (sizes, seq) = (self.sizes, self.sizes_seq);
        self.events.jobs().background(move || save(&path, sizes, seq));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTO: Sizes =
        Sizes { chat: None, side: None, bottom: None, docked: [None; 4], sessions: None, messages: None };

    /// Panes P2a: where the keys are is one value (`Session.at`) — once what it names is gone (the chat closed,
    /// the side panel closed, the terminal hidden, the slot showing nothing that takes them) the next event
    /// gives them to the code; a pane still there keeps them; `C-w z` ends with them.
    #[test]
    fn the_keys_go_to_the_code_once_what_had_them_is_gone() {
        use crate::session::At;
        let dir = crate::scratch::Scratch::new("keys-gone");
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        ed.session.root = dir.path().to_path_buf();
        use crate::key::{Code, Key};
        let key = |ed: &mut crate::editor::Editor| ed.handle_key(Key::plain(Code::Char('l')));
        ed.session.chat = Some(crate::chat::Chat::new());
        ed.session.at = At::Chat;
        ed.focus_sync();
        assert_eq!(ed.session.at, At::Chat);
        ed.session.chat = None;
        ed.focus_sync();
        assert_eq!(ed.session.at, At::Code, "the chat closed");
        ed.sidebar_show(crate::sidebar::Mode::Files);
        assert_eq!(ed.session.at, At::Side);
        key(&mut ed);
        assert_eq!(ed.session.at, At::Side, "a pane still there keeps them");
        ed.session.sidebar = None;
        key(&mut ed);
        assert_eq!(ed.session.at, At::Code, "the side panel closed");
        let _sent = crate::terminal::stand_in(&mut ed, 10, 80, 100);
        ed.session.zoom = ed.focused_area();
        assert_eq!((ed.session.at, ed.zoomed_shows()), (At::Program, Some(crate::pane::Shows::Slot)));
        ed.session.terminal.as_mut().unwrap().visible = false;
        ed.focus_sync();
        assert_eq!((ed.session.at, ed.session.zoom), (At::Code, None), "hidden: the code's, the zoom over");
        ed.session.at = At::Slot;
        ed.focus_sync();
        assert_eq!(ed.session.at, At::Code, "nothing in the slot takes them");
    }

    /// cardburden 4b: `K` in the task panel's view mode says where Claude's command in it ran and why — its
    /// run's record, as a click on its chat row's mark does; its hint on the panel's header. A run that isn't
    /// Claude's says so. (The run is the test's own `sleep`, stopped at the end.)
    #[test]
    fn k_in_the_task_panel_says_where_claudes_command_ran() {
        let scratch = crate::scratch::Scratch::new("k-ran");
        let dir = std::fs::canonicalize(scratch.path()).unwrap();
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        let task = crate::task::Task { name: "t".into(), cmd: "sleep 30".into(), cwd: dir.clone() };
        assert!(
            ed.task_start_for(task, crate::tools::For::User("t".into()), None, Default::default()).is_ok()
        );
        ed.bottom_take_keys();
        let k: crate::key::Key = "K".parse().unwrap();
        let said = |ed: &crate::editor::Editor| ed.messages.last().map(|m| m.2.clone()).unwrap_or_default();
        ed.handle_key(k);
        assert_eq!(crate::uistate::TOOK.get(), Some(crate::uistate::Handler::Output));
        assert!(said(&ed).contains("this run isn't one"), "{}", said(&ed));
        let r = ed.session.task_run.as_mut().unwrap();
        r.by_claude = true;
        r.ran = Some(crate::chat::Ran::of(&crate::sandbox::Guard::Outside, Some(7), None, Vec::new()));
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 140, 30).unwrap();
        let mut parser = vt100::Parser::new(30, 140, 0);
        parser.process(&bytes);
        assert!(parser.screen().contents().contains("K where it ran"), "its hint");
        ed.handle_key(k);
        assert_eq!(
            said(&ed),
            "sleep 30 — ran outside\nOutside the sandbox, with all your permissions — said yes to. From card#7."
        );
        ed.session.task_run = None;
    }

    /// Automatic: the chat 2/5 up to 96 (U3); at 140 with the side panel, the chat narrows so the code keeps
    /// 60; narrower still, the panel gives way; a narrow screen is all chat.
    #[test]
    fn automatic_widths() {
        assert_eq!(widths(140, true, false, AUTO), Widths { chat: 56, side: 0 });
        assert_eq!(widths(200, true, false, AUTO), Widths { chat: 80, side: 0 });
        assert_eq!(widths(300, true, false, AUTO), Widths { chat: 96, side: 0 }, "the cap: 96");
        // 140, both: the chat gives 4 columns so the code keeps 60 (it had 52)
        let w = widths(140, true, true, AUTO);
        assert_eq!(w, Widths { chat: 48, side: 32 });
        assert_eq!(140 - w.chat - w.side, CODE_ROOM);
        // 120: the chat at its least, the code 52 — the panel stays (48 is its line)
        assert_eq!(widths(120, true, true, AUTO), Widths { chat: 36, side: 32 });
        // 110: the code would have 42 — the panel gives way
        assert_eq!(widths(110, true, true, AUTO), Widths { chat: 44, side: 0 });
        assert_eq!(
            widths(70, true, true, AUTO),
            Widths { chat: 70, side: 0 },
            "narrow: the chat takes it all"
        );
        assert_eq!(widths(200, false, true, AUTO), Widths { chat: 0, side: 32 + 12 });
    }

    /// frictions25 R6: beside the review's rail (36 columns here) an automatic chat narrows to leave the code a
    /// review's room (100), down to its least — at 160 the code had 60 beside a 64-column chat; on a wide
    /// screen it's the chat's 2/5 again; a width you set, or a chat taking the whole width, stays.
    #[test]
    fn beside_the_review_rail_the_chat_leaves_the_code_its_room() {
        assert_eq!(widths(160, true, false, AUTO).chat, 64, "as it was beside the rail");
        assert_eq!(chat_beside(160, 36, AUTO), CHAT_MIN, "160: the code 88");
        assert_eq!(chat_beside(140, 36, AUTO), CHAT_MIN);
        assert_eq!(chat_beside(200, 36, AUTO), 64, "200: the code keeps 100");
        assert_eq!(chat_beside(260, 36, AUTO), CHAT_AUTO_MAX, "wide: no wider than without the rail");
        let kept = Sizes {
            chat: Some(70),
            side: None,
            bottom: None,
            docked: [None; 4],
            sessions: None,
            messages: None,
        };
        assert_eq!(chat_beside(160, 36, kept), 70, "yours stays");
        assert_eq!(chat_beside(70, 36, AUTO), 70, "narrow: the chat takes it all, as without the rail");
    }

    /// A kept size is shown as it is where it fits, clamped where it doesn't — the kept value itself isn't
    /// touched (it's the caller's, unchanged), so a bigger screen gets it whole again.
    #[test]
    fn kept_widths_are_clamped_not_changed() {
        let kept = Sizes {
            chat: Some(90),
            side: Some(40),
            bottom: None,
            docked: [None; 4],
            sessions: None,
            messages: None,
        };
        assert_eq!(widths(200, true, true, kept), Widths { chat: 90, side: 40 });
        // 150: the chat keeps its 90; the panel's 40 would leave the code 20 — it gives way
        assert_eq!(widths(150, true, true, kept), Widths { chat: 90, side: 0 });
        // 120: the chat as wide as leaves the code 40
        assert_eq!(widths(120, true, false, kept), Widths { chat: 80, side: 0 });
        assert_eq!(kept.chat, Some(90), "kept as it was");
        assert_eq!(
            widths(200, true, true, kept),
            Widths { chat: 90, side: 40 },
            "the bigger screen: whole again"
        );
        // Out of bounds kept values (an edited file): within them
        let odd = Sizes {
            chat: Some(5),
            side: Some(500),
            bottom: None,
            docked: [None; 4],
            sessions: None,
            messages: None,
        };
        assert_eq!(widths(240, true, true, odd), Widths { chat: CHAT_MIN, side: SIDE_MAX });
    }

    #[test]
    fn bottom_rows() {
        assert_eq!(bottom(36, None, 9), 9, "automatic: what the panel wants");
        assert_eq!(bottom(36, Some(20), 9), 20, "yours wins");
        assert_eq!(bottom(20, Some(30), 9), 16, "clamped: the code keeps 4");
        assert_eq!(bottom(36, Some(1), 9), BOTTOM_MIN);
        assert_eq!(bottom(5, Some(10), 9), 0, "no room at all");
    }

    /// Kept across runs: read back as written; missing or odd files are automatic.
    #[test]
    fn kept_in_a_file() {
        let dir = crate::scratch::Scratch::new("sizes");
        let path = dir.join("sizes.json");
        assert_eq!(Sizes::read(Some(&path)), Sizes::default(), "missing: automatic");
        let s = Sizes {
            chat: Some(70),
            side: None,
            bottom: Some(12),
            docked: [None; 4],
            sessions: None,
            messages: None,
        };
        save(&path, s, 1);
        assert_eq!(Sizes::read(Some(&path)), s);
        save(&path, Sizes::default(), 0);
        assert_eq!(Sizes::read(Some(&path)), s, "an older write that comes late is skipped");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Sizes::read(Some(&path)), Sizes::default());
        std::fs::remove_dir_all(&dir).ok();
    }
}
