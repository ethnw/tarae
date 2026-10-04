//! Sizes you set (U1): the chat's and the side panel's widths and the bottom slot's height. Global, not per
//! project: kept as columns and rows in `$state/sizes.json`, read at startup (so across `:restart` too). A
//! screen too small for a kept size shows less of it — clamped here, never written back — and a bigger one
//! gets it again. Unset, each is automatic: the chat 2/5 of the screen (36–96 columns — U3), the side panel
//! 32 and more on a wide screen, the bottom slot what its panel wants (a task's output — U2).
//!
//! `C-w <`/`>` narrow/widen the focused one (the chat, the side panel; from the code: the chat if it's open,
//! else the side panel), `C-w -`/`+` the bottom slot, `C-w =` makes them all automatic again.
//!
//! Beside each other (140 columns: side panel 32 + chat 56 left the code 52): an automatic chat narrows to
//! leave the code `CODE_ROOM` (down to `CHAT_MIN`); if the code would still have less than `CODE_MIN`, the side
//! panel gives way, whole (it comes back when there's room — its keys go back to the code meanwhile).
//!
//! `C-w z` fills the screen with what has the keys — the code (its pane alone), the bottom slot, the chat, the
//! side panel — and back (`Zoom`); it ends when the keys go elsewhere. The bottom slot takes the keys with
//! `C-w j` or a click (`bottom_key`: they scroll it; Esc or `C-w k` back to the code).

use crate::key::{Code, Key};

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
}

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
        let n = |k: &str| v[k].as_u64().and_then(|n| u16::try_from(n).ok());
        Sizes { chat: n("chat"), side: n("side"), bottom: n("bottom") }
    }

    pub fn to_json(self) -> String {
        json!({ "chat": self.chat, "side": self.side, "bottom": self.bottom }).to_string()
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

/// A border the mouse drags: the chat's left edge, the side panel's right edge, the bottom slot's top row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Border {
    Chat,
    Side,
    Bottom,
}

/// What `C-w <`/`>` sizes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Which {
    Chat,
    Side,
}

/// What `C-w z` fills the screen with: the code (that pane alone), the bottom slot, the chat, the side panel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zoom {
    Code(crate::split::ViewId),
    Bottom,
    Chat,
    Side,
}

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

/// Keys while the bottom slot has them (test and task output, the terminal's scrollback): `j`/`k` a line,
/// `C-d`/`C-u` half the panel, `g`/`G` the start/end; on the terminal `i` (Enter) types in it; Esc back to
/// the code. The space menu, `:`, `[`/`]` and `C-w` stay the editor's; other keys do nothing here (the code
/// may not be in sight). False = not the slot's.
pub fn bottom_key(ed: &mut crate::editor::Editor, key: Key) -> bool {
    if !ed.bottom_focus
        || ed.mode != crate::editor::Mode::Normal
        || !ed.pending.is_empty()
        || ed.prompt.is_some()
    {
        return false;
    }
    let Some(slot) = ed.slot().filter(|s| matches!(s, Slot::Terminal | Slot::Test | Slot::Task)) else {
        ed.bottom_focus = false;
        return false;
    };
    let rows = ed.slot_rows(slot);
    let half = (rows / 2).max(1) as isize;
    let by = match (key.code, key.ctrl) {
        (Code::Char(' ' | ':' | '[' | ']'), false) | (Code::Char('w'), true) => return false,
        (Code::Esc, _) => {
            ed.bottom_focus = false;
            return true;
        }
        (Code::Char('i' | 'a') | Code::Enter, false) if slot == Slot::Terminal => {
            if let Some(t) = ed.terminal.as_mut().filter(|t| t.exited.is_none()) {
                t.focus = true;
                ed.bottom_focus = false;
            }
            return true;
        }
        (Code::Char('j') | Code::Down, false) | (Code::Char('e'), true) => 1,
        (Code::Char('k') | Code::Up, false) | (Code::Char('y'), true) => -1,
        (Code::Char('d'), true) | (Code::PageDown, _) => half,
        (Code::Char('u'), true) | (Code::PageUp, _) => -half,
        (Code::Char('g') | Code::Home, false) => isize::MIN / 2,
        (Code::Char('G') | Code::End, false) => isize::MAX / 2,
        _ => return true,
    };
    match slot {
        Slot::Test => {
            if let Some(r) = ed.test_run.as_mut() {
                r.scroll_by(by, rows);
            }
        }
        Slot::Task => {
            if let Some(r) = ed.task_run.as_mut() {
                r.scroll_by(by, rows);
            }
        }
        // (back is up)
        _ => ed.terminal_scroll_by(-by),
    }
    true
}

impl crate::editor::Editor {
    /// What `C-w <`/`>` acts on: the chat or the side panel, whichever has the keys — from the code, the
    /// chat if it's shown as a panel, else the side panel.
    fn sized(&self) -> Option<Which> {
        let chat = self.screen.chat_w > 0 && self.chat.as_ref().is_some_and(|c| !c.compact());
        let side = self.sidebar.is_some() && !self.review_rail_up();
        if chat && self.chat.as_ref().is_some_and(|c| c.focused) {
            return Some(Which::Chat);
        }
        if side && self.sidebar.as_ref().is_some_and(|s| s.focus) {
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

    /// The border at screen cell (`x`, `y`) — as last drawn: the chat's edge column (`▐`), the side panel's
    /// (`▌`), the bottom panel's top row (its title) left of the chat.
    pub fn border_at(&self, x: usize, y: usize) -> Option<Border> {
        // (one thing fills the screen: no borders)
        if self.zoomed().is_some() {
            return None;
        }
        let s = &self.screen;
        let total = s.width?;
        let (top, end) = (s.text_top, s.text_top + s.body_h);
        let bottom_y = end - s.bottom_h;
        if s.chat_w > 0 && s.chat_w < total && x == total - s.chat_w && (top..end).contains(&y) {
            return Some(Border::Chat);
        }
        if s.side_w > 0 && x + 1 == s.side_w && (top..bottom_y).contains(&y) {
            return Some(Border::Side);
        }
        if s.bottom_h > 0 && y == bottom_y && x < total - s.chat_w {
            return Some(Border::Bottom);
        }
        None
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
        }
    }

    /// The press let go: what it set is kept, and said.
    pub fn border_done(&mut self, b: Border) {
        self.sizes_keep();
        let (what, n, unit) = match b {
            Border::Chat => ("chat", self.sizes.chat, "columns"),
            Border::Side => ("side panel", self.sizes.side, "columns"),
            Border::Bottom => ("bottom panel", self.sizes.bottom, "rows"),
        };
        if let Some(n) = n {
            self.say_size(format!("{what} {n} {unit}"));
        }
    }

    /// A size said in a toast — one at a time: the one before it goes (a few presses of `C-w >` are one
    /// message that changes, not a stack of them).
    fn say_size(&mut self, msg: String) {
        self.toasts.retain(|t| !t.text.ends_with(SIZE_TAIL));
        self.set_status(format!("{msg}{SIZE_TAIL}"));
    }

    /// `C-w >` / `C-w <` — the chat or the side panel `STEP_COLS` wider/narrower, from its width on screen;
    /// as far as leaves the code its room. Kept (global) — `C-w =` makes it automatic again.
    pub fn size_width(&mut self, wider: bool) {
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
        let chat_open = self.chat.as_ref().is_some_and(|c| !c.compact());
        self.sidebar.is_some()
            && !self.review_rail_up()
            && widths(total, chat_open, true, self.sizes).side == 0
    }

    /// `C-w =` — every size automatic again.
    pub fn sizes_auto(&mut self) {
        self.sizes = Sizes::default();
        self.sizes_keep();
        self.toasts.retain(|t| !t.text.ends_with(SIZE_TAIL));
        self.set_status("sizes automatic again");
    }

    /// What the bottom slot shows, if anything (the order `term::draw` follows).
    pub fn slot(&self) -> Option<Slot> {
        if self.procs_up() {
            return Some(Slot::Procs);
        }
        if self.dap.is_some() {
            return Some(Slot::Debug);
        }
        if self.terminal.as_ref().is_some_and(|t| t.visible) {
            return Some(Slot::Terminal);
        }
        if self.plugins.as_ref().is_some_and(|p| p.panel.is_some()) {
            return Some(Slot::Plugin);
        }
        match (&self.test_run, &self.task_run) {
            (Some(t), Some(k)) if k.started > t.started => Some(Slot::Task),
            (Some(_), _) => Some(Slot::Test),
            (None, Some(_)) => Some(Slot::Task),
            (None, None) => None,
        }
    }

    /// The rows of output the slot's panel showed last frame (before one: 10).
    fn slot_rows(&self, slot: Slot) -> usize {
        let s = &self.screen;
        match slot {
            Slot::Test => s.test_panel.get().map(|p| p.2),
            Slot::Task => s.task_panel.get().map(|p| p.2),
            _ => s.terminal_size.get().map(|(r, _)| r as usize),
        }
        .unwrap_or(10)
        .max(1)
    }

    /// Whether the bottom slot has the keys — tarae's (`bottom_focus`), or the terminal's program, or a
    /// plugin panel's.
    pub fn bottom_has_keys(&self) -> bool {
        match self.slot() {
            Some(Slot::Terminal) => self.bottom_focus || self.terminal.as_ref().is_some_and(|t| t.focus),
            Some(Slot::Plugin) => self.plugin_panel_focused(),
            Some(Slot::Procs) => self.procs_focused(),
            Some(Slot::Test | Slot::Task) => self.bottom_focus,
            Some(Slot::Debug) | None => false,
        }
    }

    /// `C-w j` below the lowest pane, or a click on the panel: the keys to the bottom slot (on the terminal,
    /// tarae's — `i` then types in it). The debugger's panel takes none (its keys work from the code).
    pub fn bottom_take_keys(&mut self) {
        match self.slot() {
            None => self.set_status("nothing at the bottom — tests, tasks and the terminal show there"),
            Some(Slot::Debug) => {
                self.set_status("the debug panel follows the code — its keys work from there")
            }
            Some(Slot::Plugin) => self.plugin_panel_click(None),
            Some(Slot::Procs) => self.procs_click(None),
            Some(_) => {
                if let Some(c) = self.chat.as_mut() {
                    c.focused = false;
                }
                if let Some(s) = self.sidebar.as_mut() {
                    s.focus = false;
                }
                if let Some(r) = self.code_review.as_mut() {
                    r.focus = false;
                }
                if let Some(t) = self.terminal.as_mut() {
                    t.focus = false;
                }
                self.bottom_focus = true;
            }
        }
    }

    /// What has the keys, as `C-w z` would fill the screen with it — None: the review's file list (it keeps
    /// its width).
    pub fn focused_area(&self) -> Option<Zoom> {
        if self.chat.as_ref().is_some_and(|c| c.focused && !c.compact()) {
            return Some(Zoom::Chat);
        }
        let rail = self.review_rail_up();
        if rail && self.code_review.as_ref().is_some_and(|r| r.focus) {
            return None;
        }
        if !rail && self.sidebar.as_ref().is_some_and(|s| s.focus) {
            return Some(Zoom::Side);
        }
        if self.bottom_has_keys() {
            return Some(Zoom::Bottom);
        }
        Some(Zoom::Code(self.focus))
    }

    /// What fills the screen now: `C-w z`'s, while it still has the keys.
    pub fn zoomed(&self) -> Option<Zoom> {
        self.zoom.filter(|z| Some(*z) == self.focused_area())
    }

    /// `C-w z` — what has the keys fills the screen; again, back as it was.
    pub fn zoom_toggle(&mut self) {
        if self.zoomed().is_some() {
            self.zoom = None;
            return;
        }
        let Some(z) = self.focused_area() else {
            return self.set_status("the review's file list keeps its width — C-w z from the code");
        };
        let s = &self.screen;
        let alone = self.views.len() == 1
            && s.chat_w == 0
            && s.side_w == 0
            && s.bottom_h == 0
            && !(self.code_review.is_some() && self.review_rail_up());
        if matches!(z, Zoom::Code(_)) && alone {
            return self.set_status("the code has the whole screen already");
        }
        self.zoom = Some(z);
    }

    /// Every key and event: the bottom slot's keys go when something else took them (or it emptied), and
    /// `C-w z` ends once what it filled the screen with doesn't have them.
    pub fn focus_sync(&mut self) {
        if self.bottom_focus {
            let program = self.terminal.as_ref().is_some_and(|t| t.focus && t.visible);
            let other = self.chat.as_ref().is_some_and(|c| c.focused)
                || self.sidebar.as_ref().is_some_and(|s| s.focus)
                || self.code_review.as_ref().is_some_and(|r| r.focus);
            let slot = matches!(self.slot(), Some(Slot::Terminal | Slot::Test | Slot::Task));
            if program || other || !slot {
                self.bottom_focus = false;
            }
        }
        // `:procs`' panel: the same, when another place took the keys — and it gives the slot to the terminal
        // given them (`C-\`), which it covered
        if self.procs_up() && self.terminal.as_ref().is_some_and(|t| t.focus && t.visible) {
            self.procs_close();
        }
        if self.procs_focused()
            && (self.chat.as_ref().is_some_and(|c| c.focused)
                || self.sidebar.as_ref().is_some_and(|s| s.focus)
                || self.code_review.as_ref().is_some_and(|r| r.focus))
        {
            self.procs_unfocus();
        }
        if self.zoom.is_some() && self.zoomed().is_none() {
            self.zoom = None;
        }
    }

    /// Writes the sizes (worker — the newest write wins).
    pub fn sizes_keep(&mut self) {
        let Some(path) = self.sizes_path.clone() else { return };
        self.sizes_seq += 1;
        let (sizes, seq) = (self.sizes, self.sizes_seq);
        self.events.jobs().spawn(move || {
            save(&path, sizes, seq);
            |_: &mut crate::editor::Editor| {}
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const AUTO: Sizes = Sizes { chat: None, side: None, bottom: None };

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

    /// A kept size is shown as it is where it fits, clamped where it doesn't — the kept value itself isn't
    /// touched (it's the caller's, unchanged), so a bigger screen gets it whole again.
    #[test]
    fn kept_widths_are_clamped_not_changed() {
        let kept = Sizes { chat: Some(90), side: Some(40), bottom: None };
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
        let odd = Sizes { chat: Some(5), side: Some(500), bottom: None };
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
        let dir = std::env::temp_dir().join(format!("tarae-sizes-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let path = dir.join("sizes.json");
        assert_eq!(Sizes::read(Some(&path)), Sizes::default(), "missing: automatic");
        let s = Sizes { chat: Some(70), side: None, bottom: Some(12) };
        save(&path, s, 1);
        assert_eq!(Sizes::read(Some(&path)), s);
        save(&path, Sizes::default(), 0);
        assert_eq!(Sizes::read(Some(&path)), s, "an older write that comes late is skipped");
        std::fs::write(&path, "not json").unwrap();
        assert_eq!(Sizes::read(Some(&path)), Sizes::default());
        std::fs::remove_dir_all(&dir).ok();
    }
}
