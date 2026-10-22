//! The panels' chrome — one set of rules for every panel (the layout pass, 2026-10-03: the user found the
//! padding, the gutters and the headers uneven on a real screen).
//!
//! - **Columns.** The side panel (or the review's rail) and the chat run the whole height, top to the status
//!   line; between them the code, and under the code the bottom slot. Every panel sits on the ground
//!   (`ui.panel` over `ui.background`).
//! - **Gutter.** One thin line between regions side by side, in the gutter's whole cell (`GAP`): `│`, `├`/`┤`
//!   where the slot's top edge meets it, lit for the surface with the keys (`term::edge_style`). What's right of
//!   it starts on the next cell, so every panel's text is `PAD` from its line — the side panel, the slot and the
//!   chat alike. Against the screen's edge (a panel filling the screen) the panel takes the cell whole — no line
//!   against nothing.
//! - **Padding.** Text starts `PAD` cells in from a panel's edge and stops `PAD_R` before its other edge.
//!   The code has none of its own: its sign column is the first cell after the gutter (so a server attaching
//!   doesn't move the text), its numbers right after — they inset its text already — and its path bar and
//!   rule start where the numbers do (`SIGNS`).
//! - **Header.** Every panel, and the code under the path bar, opens with a title row and a thin rule
//!   (`HEAD` rows); their tops line up on the screen's first row. (A screen under `RULE_MIN_ROWS` rows:
//!   the code's title goes without its rule.) The bottom slot's header is the line where it meets the code
//!   above it (panes P3 §1, `SLOT_HEAD`) — one row, as a frame's top line names it.
//! - **Focus, quietly.** What has the keys draws its header rule in the accent; everything else keeps
//!   `FADE` of its text's contrast; the status line's label names where the keys go (`Keys::label`).
//! - **The bottom slot's tabs.** The slot's title row lists every panel that's there (`Editor::slot_tabs`),
//!   with a badge for how it's doing; only one shows. A run that starts while the slot has the keys stays a
//!   tab — it never takes them.

use std::time::Instant;

use crossterm::style::Color;

use crate::editor::Editor;
use crate::sizes::Slot;

/// Cells between a panel's edge and its text: before it, after it.
pub const PAD: usize = 2;
pub const PAD_R: usize = 1;
/// The code's cells before its numbers: the sign column, right after the gutter.
pub const SIGNS: usize = 1;
/// The gutter between two regions side by side: a cell, half of it ground.
pub const GAP: usize = 1;
/// A header: the title row and its rule.
pub const HEAD: usize = 2;

/// The bottom slot's header rows: one — the line where the slot meets what's above it carries its names, details
/// and keys (panes P3 §1).
pub const SLOT_HEAD: usize = 1;
/// Under this many screen rows the code's path bar goes without its rule (a small pane keeps the row for
/// code; the panels beside keep theirs).
pub const RULE_MIN_ROWS: u16 = 16;
/// The picker's margins over the editor area: columns, rows (a cell is about twice as tall as wide — the
/// two look the same).
pub const PICKER_MX: usize = 2;
pub const PICKER_MY: usize = 1;
/// A list card's rows at least — it's as tall as its rows and a row of air, not the screen (geultarae §2.3).
pub const LIST_MIN: usize = 12;
/// The chat's measure: its prose wraps at this many cells at most, the rest of a wide panel is margin (code
/// blocks, rules and the tool rows keep the panel's width — geultarae decision 7).
pub const MEASURE: usize = 72;
/// What an unfocused region keeps of its text's contrast against its background (WCAG ratio).
pub const FADE: f64 = 0.88;

// ── Contrast ─────────────────────────────────────────────────────────────

/// A colour's red, green, blue — only for true colours (a palette index is the terminal's to say).
pub fn rgb(c: Color) -> Option<[u8; 3]> {
    match c {
        Color::Rgb { r, g, b } => Some([r, g, b]),
        _ => None,
    }
}

/// Relative luminance (WCAG 2).
pub fn luminance(c: [u8; 3]) -> f64 {
    let lin = |v: u8| {
        let v = v as f64 / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

/// Contrast ratio between two colours (1 = none, 21 = black on white).
pub fn contrast(a: [u8; 3], b: [u8; 3]) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

fn mix(a: [u8; 3], b: [u8; 3], t: f64) -> [u8; 3] {
    let m = |x: u8, y: u8| (x as f64 + (y as f64 - x as f64) * t).round() as u8;
    [m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2])]
}

/// `fg` moved toward `bg` until it keeps `keep` of its contrast with it — the same step in every theme
/// (dark text on paper moves less than light text on ink for the same loss).
pub fn fade(fg: [u8; 3], bg: [u8; 3], keep: f64) -> [u8; 3] {
    let want = 1.0 + (contrast(fg, bg) - 1.0) * keep;
    let (mut lo, mut hi) = (0.0, 1.0);
    for _ in 0..16 {
        let t = (lo + hi) / 2.0;
        if contrast(mix(fg, bg, t), bg) > want { lo = t } else { hi = t }
    }
    mix(fg, bg, lo)
}

// ── Where the keys go ────────────────────────────────────────────────────

/// What has the keys — the region drawn at full strength, the one the status line names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keys {
    Code,
    Picker,
    Chat,
    Side(crate::sidebar::Mode),
    /// The code review's file rail.
    Review,
    /// Claude's change in review in the buffer (its `y`/`n`) — a word of its own, so `review` means one thing.
    Change,
    /// The bottom slot with tarae's keys (the terminal's are its view mode's).
    Slot(Slot),
    /// The terminal's program: every key goes to it.
    Program,
    /// A card given the keys with `C-q` (#78: the status line still said CHAT).
    Card,
    /// Labels up for pointing at an element: letters pick one (`uistate/point.rs`).
    Point,
    /// A surface's view mode (`view.rs`).
    View(crate::view::Surface),
    /// A run panel's output, normal mode on its document (`rundoc.rs`) — the status line says its mode and name.
    Output(crate::rundoc::Run),
    /// A linked group's list in the slot (`grouplist.rs`) — named as its header names it: a preflight's steps
    /// `preflight`, not the task panel's word.
    List(crate::pane::GroupKind),
}

impl Keys {
    /// The status line's label for it (`None`: the code's own mode — NORMAL, INSERT, SELECT).
    pub fn label(self) -> Option<&'static str> {
        use crate::sidebar::Mode as S;
        Some(match self {
            Keys::Code => return None,
            Keys::Picker => "PICKER",
            Keys::Chat => "CHAT",
            Keys::Side(S::Files) => "FILES",
            Keys::Side(S::Outline) => "OUTLINE",
            Keys::Side(S::List) => "LIST",
            Keys::Side(S::Git) => "GIT",
            Keys::Side(S::Plugin) => "PANEL",
            Keys::Review => "REVIEW",
            Keys::Change => "CHANGE",
            Keys::Slot(Slot::Terminal) | Keys::Program => "TERMINAL",
            Keys::Slot(Slot::Test) => "TESTS",
            Keys::Slot(Slot::Task) => "TASK",
            Keys::Slot(Slot::Procs) => "PROCS",
            Keys::Slot(Slot::Debug) => "DEBUG",
            Keys::Slot(Slot::Plugin) => "PANEL",
            Keys::Card => "CARD",
            Keys::Point => "POINT",
            Keys::View(crate::view::Surface::Chat) => "CHAT",
            Keys::View(crate::view::Surface::Terminal) => "VIEW · TERMINAL",
            Keys::Output(crate::rundoc::Run::Task) => "TASK",
            Keys::Output(crate::rundoc::Run::Preflight) => "PREFLIGHT",
            Keys::Output(crate::rundoc::Run::Tests) => "TESTS",
            Keys::Output(crate::rundoc::Run::Procs) => "PROCS",
            Keys::List(k) => match k {
                crate::pane::GroupKind::Tests => "TESTS",
                crate::pane::GroupKind::Task => "TASK",
                crate::pane::GroupKind::Preflight => "PREFLIGHT",
                crate::pane::GroupKind::Procs => "PROCS",
                crate::pane::GroupKind::Git => "GIT",
                crate::pane::GroupKind::Review => "REVIEW",
            },
        })
    }

    /// The chat's, as it's drawn: its input or its view mode has them.
    pub fn chat(self) -> bool {
        matches!(self, Keys::Chat | Keys::View(crate::view::Surface::Chat))
    }

    /// Whether the code panes are what has them (they're drawn at full strength).
    pub fn code(self) -> bool {
        matches!(self, Keys::Code | Keys::Review | Keys::Change)
    }

    /// The key hint the status line puts after the label (key, what it does). A view mode's — the terminal's
    /// with tarae's keys — are the status line's own, each key as bound.
    pub fn hint(self) -> Option<(&'static str, &'static str)> {
        match self {
            Keys::Point => Some(("esc", "stop pointing")),
            _ => None,
        }
    }
}

/// Where the one cursor on screen is: where the keys go (user decision 2026-10-20 — as Vim and tmux keep one,
/// and an editor without focus draws no caret). Everywhere else nothing of a cursor: the code keeps only its
/// cursor-line wash, the input no caret; selections keep their colours (a selection isn't a cursor).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorIn {
    /// The focused pane's (its primary block, its other cursors, the terminal's own cursor on it).
    Code,
    /// The chat's input caret.
    Input,
    /// A view mode's cursors (`view.rs`), painted on its rows — a run panel's output's, by the code's drawer.
    View,
    /// The terminal's program's cursor.
    Program,
    /// A prompt's caret (`:`, `/` — a view's search too).
    Prompt,
    /// A picker's query caret.
    Picker,
}

impl Editor {
    /// Where the cursor is drawn: `keys_to`'s region — a card's answers taken in normal mode aren't where the keys
    /// are (what's under it is), a card given them (`C-q`) has none; panels, lists, an edit's review and labels have
    /// none.
    pub fn cursor_in(&self) -> Option<CursorIn> {
        use crate::uistate::Region as R;
        let k = self.keys_to();
        let region = match k.region {
            R::Card(_) if !k.held => k.under.unwrap_or(R::Code),
            r => r,
        };
        Some(match region {
            R::Code => CursorIn::Code,
            R::Chat => CursorIn::Input,
            R::View(_) | R::Output(_) => CursorIn::View,
            R::Program => CursorIn::Program,
            R::Prompt => CursorIn::Prompt,
            R::Picker => CursorIn::Picker,
            _ => return None,
        })
    }

    /// A key the panel with the keys has no use for: said, not swallowed in silence — the routing matrix
    /// found the side panel, the review's file list, `:procs`, the bottom slot and an edit's review taking
    /// any key and doing nothing (`uistate::tests::silent`). `out`: what to do instead.
    pub fn key_unused(&mut self, key: crate::key::Key, place: &str, out: &str) {
        self.set_status(format!("{key} does nothing in {place} — {out}"));
    }

    /// Where the keys go now — `keys_to`'s region (`uistate.rs`, `handle_key`'s chain as data). A card's
    /// answers taken in normal mode aren't where the keys are (the code's mode is): only one held with
    /// `C-q`. A prompt is the code's (its line is the status line's neighbour).
    pub fn keys_at(&self) -> Keys {
        use crate::uistate::Region as R;
        let k = self.keys_to();
        let region = match k.region {
            R::Card(_) if !k.held => k.under.unwrap_or(R::Code),
            r => r,
        };
        match region {
            R::Picker => Keys::Picker,
            R::Card(_) => Keys::Card,
            R::Chat | R::Unfold => Keys::Chat,
            R::Rail => Keys::Review,
            R::EditReview => Keys::Change,
            R::Side(m) => Keys::Side(m),
            R::Slot(s) => Keys::Slot(s),
            R::List(k) => match (k.slot(), k) {
                (Some(_), _) => Keys::List(k),
                (None, crate::pane::GroupKind::Git) => Keys::Side(crate::sidebar::Mode::Git),
                (None, _) => Keys::Review,
            },
            R::Program => Keys::Program,
            R::Point => Keys::Point,
            R::View(s) => Keys::View(s),
            R::Output(r) => Keys::Output(r),
            R::Code | R::Prompt | R::NextCard | R::Popup => Keys::Code,
        }
    }
}

// ── The bottom slot's tabs ───────────────────────────────────────────────

/// How a tab's panel is doing: under way (the wave), done well (`●`), failed (`▲` in the error colour),
/// wants a look (`▲` in the warning colour), stopped part-way for you (`◦` in the warning colour).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    None,
    Running,
    Passed,
    Failed,
    Warning,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tab {
    pub slot: Slot,
    pub label: String,
    pub badge: Badge,
}

/// The tabs' order — fixed, so a tab never moves when another comes or goes.
const ORDER: [Slot; 6] = [Slot::Terminal, Slot::Task, Slot::Test, Slot::Debug, Slot::Plugin, Slot::Procs];

impl Editor {
    /// Whether `s` has something to show in the slot (a hidden terminal is a tab, not shown).
    pub(crate) fn slot_there(&self, s: Slot, shown: bool) -> bool {
        match s {
            Slot::Procs => self.procs_up(),
            Slot::Debug => self.dap.is_some(),
            Slot::Terminal => self.session.terminal.as_ref().is_some_and(|t| t.visible || !shown),
            Slot::Plugin => self.plugins.as_ref().is_some_and(|p| p.panel.is_some()),
            Slot::Test => self.session.test_run.is_some(),
            Slot::Task => self.session.task_run.is_some(),
        }
    }

    /// The panel you put in front (a tab clicked, `tab` in its view mode, one just opened), while it's there.
    pub(crate) fn slot_front(&self) -> Option<Slot> {
        self.session.slot_front.filter(|&s| self.slot_there(s, true))
    }

    /// Every panel the slot has, in `ORDER`, with how it's doing — the slot's title row; a group docked in sight is
    /// a pane of its own, not among them (P3 §3.1).
    pub fn slot_tabs(&self) -> Vec<Tab> {
        let mut tabs = self.panel_tabs();
        tabs.retain(|t| !self.slot_docked(t.slot));
        tabs
    }

    /// Every panel there is, docked or in the slot, in `ORDER`, with how it's doing.
    pub fn panel_tabs(&self) -> Vec<Tab> {
        use crate::dap::State as D;
        ORDER
            .iter()
            .filter(|&&s| self.slot_there(s, false))
            .map(|&slot| {
                let (label, badge) = match slot {
                    Slot::Terminal => {
                        let t = self.session.terminal.as_ref();
                        let badge = match t.and_then(|t| t.exited) {
                            Some(0) | None => Badge::None,
                            Some(_) => Badge::Failed,
                        };
                        ("terminal".to_string(), badge)
                    }
                    Slot::Task => {
                        let r = self.session.task_run.as_ref().expect("there");
                        let badge = match r.state {
                            crate::task::State::Running => Badge::Running,
                            crate::task::State::Passed if r.found == (0, 0) => Badge::Passed,
                            crate::task::State::Passed => Badge::Warning,
                            crate::task::State::Failed(_) => Badge::Failed,
                            crate::task::State::Stopped(_) => Badge::Stopped,
                        };
                        (r.task.name.clone(), badge)
                    }
                    Slot::Test => {
                        use crate::testing::RunState as R;
                        let r = self.session.test_run.as_ref().expect("there");
                        let badge = match r.state {
                            R::Running => Badge::Running,
                            R::Passed => Badge::Passed,
                            R::Failed(_) => Badge::Failed,
                            R::NoTests => Badge::Warning,
                        };
                        ("tests".to_string(), badge)
                    }
                    Slot::Debug => {
                        let badge = match self.dap.as_ref().map(|d| &d.state) {
                            Some(D::Building | D::Starting | D::Running) => Badge::Running,
                            Some(D::Stopped { .. }) => Badge::Warning,
                            _ => Badge::None,
                        };
                        ("debug".to_string(), badge)
                    }
                    Slot::Plugin => {
                        let name =
                            self.plugins.as_ref().and_then(|p| p.panel.as_ref()).map(|p| p.plugin.clone());
                        (name.unwrap_or_default().to_lowercase(), Badge::None)
                    }
                    Slot::Procs => {
                        let badge = if self.procs.mark().is_some() { Badge::Warning } else { Badge::None };
                        ("procs".to_string(), badge)
                    }
                };
                Tab { slot, label, badge }
            })
            .collect()
    }

    /// A tab clicked, or stepped to: its panel in front — with the keys if the slot had them (on the
    /// terminal, tarae's: `i` types in it).
    pub fn slot_show(&mut self, s: Slot) {
        if !self.slot_there(s, false) {
            return;
        }
        // (a docked group's name in its line: the keys to the member it had last)
        if self.slot_docked(s)
            && let Some(g) = self.kind_of(s).and_then(|k| self.session.group(k))
        {
            return match (g.front, crate::grouplist::listed(self, g.kind)) {
                (crate::pane::Member::List, true) => self.list_focus(g.kind),
                _ => self.detail_focus(g.kind),
            };
        }
        let keys = self.bottom_has_keys();
        if s == Slot::Terminal
            && let Some(t) = self.session.terminal.as_mut()
        {
            t.visible = true;
        }
        self.session.slot_front = Some(s);
        if s != Slot::Terminal {
            self.session.leave(crate::session::At::Program);
        }
        if s != Slot::Procs {
            self.procs_unfocus();
        }
        if s != Slot::Plugin {
            self.plugin_panel_unfocus();
        }
        if keys {
            self.bottom_take_keys();
        }
    }

    /// `tab` / `S-tab` while the slot has the keys: the next tab in front, or the one before. On a group (§2.1c: the
    /// tests', a task's places, a preflight's steps, `:procs`) its members come first, in order — `tab` from the list
    /// to its detail, from the detail on to the next tab, which starts at its first member, the list; `S-tab` the
    /// other way, onto a group at its last member, the detail.
    pub fn slot_step(&mut self, by: isize) {
        // (a docked group: its own members only — the slot's tabs are the slot's)
        if self.slot_in_hand().is_some() {
            if let Some(g) = self.slot_group()
                && let Some(detail) = g.detail_pane()
            {
                let on_list = self.session.is_at(crate::session::At::List(g.list));
                return self.focus_pane(if on_list { detail } else { g.list });
            }
            return self.set_status("its output is all it has yet — C-w J back to the slot");
        }
        let tabs = self.slot_tabs();
        if tabs.is_empty() {
            return self.set_status("nothing at the bottom — tests, tasks and the terminal show there");
        }
        if let Some(g) = self.slot_group()
            && let Some(detail) = g.detail_pane()
        {
            let on_list = self.session.is_at(crate::session::At::List(g.list));
            let on_detail = !on_list && self.session.is_at(crate::session::At::Slot);
            match (on_list, on_detail, by > 0) {
                (true, _, true) => return self.focus_pane(detail),
                (_, true, false) => return self.focus_pane(g.list),
                // (the one tab there is: round its two members)
                (true, _, false) | (_, true, true) if tabs.len() == 1 => {
                    return self.focus_pane(if on_list { detail } else { g.list });
                }
                _ => {}
            }
        }
        let at = self.slot().and_then(|s| tabs.iter().position(|t| t.slot == s));
        let n = tabs.len() as isize;
        let next = match at {
            Some(i) => (i as isize + by).rem_euclid(n) as usize,
            None => 0,
        };
        self.slot_show(tabs[next].slot);
        // (onto a group going forward: its first member, the list; going back, its last, the detail)
        if self.bottom_has_keys()
            && let Some(g) = self.slot_group()
        {
            match (by > 0, g.detail_pane()) {
                (true, _) => self.focus_pane(g.list),
                (false, Some(detail)) => self.focus_pane(detail),
                (false, None) => {}
            }
        }
    }

    /// Every event: a panel that's new in the slot comes to the front — a test or a task only while the slot
    /// doesn't have the keys (it stays a tab, its badge saying how it goes; nothing hidden takes them). And
    /// the terminal's program never has the keys while it's not the one in front.
    pub fn slot_sync(&mut self) {
        let now: Vec<(Slot, Option<Instant>)> = ORDER
            .iter()
            .filter(|&&s| self.slot_there(s, true))
            .map(|&s| {
                let at = match s {
                    Slot::Test => self.session.test_run.as_ref().map(|r| r.started),
                    Slot::Task => self.session.task_run.as_ref().map(|r| r.started),
                    _ => None,
                };
                (s, at)
            })
            .collect();
        let new: Vec<Slot> =
            now.iter().filter(|n| !self.session.slot_seen.contains(n)).map(|n| n.0).collect();
        if !new.is_empty() {
            let keys = self.bottom_has_keys();
            // (several at once: the last in `ORDER` — the rarer, asked-for ones)
            if let Some(&s) = new.iter().rev().find(|s| !(keys && matches!(s, Slot::Test | Slot::Task))) {
                self.session.slot_front = Some(s);
            }
        }
        self.session.slot_seen = now;
        if self.slot() != Some(Slot::Terminal) {
            self.session.leave(crate::session::At::Program);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fade loses the same share of contrast in both built-in themes — light text on ink and dark text
    /// on paper — and stays readable; the colours read from the themes themselves. Measured (WCAG ratio,
    /// before → after, 2026-10-21's palettes): meok text on ink 12.80 → 11.42, on its surface 11.17 → 9.98, dim
    /// 4.38 → 4.02, faint 2.71 → 2.51; hanji text on paper 11.36 → 10.20, on its surface 10.20 → 9.16, dim
    /// 3.95 → 3.63, faint 2.64 → 2.45 — 11–12% of the contrast above 1 each.
    #[test]
    fn the_fade_keeps_the_same_share_of_contrast_in_both_themes() {
        let rgb = |c: Option<crossterm::style::Color>| match c {
            Some(crossterm::style::Color::Rgb { r, g, b }) => [r, g, b],
            other => panic!("a built-in theme's colour is a true colour: {other:?}"),
        };
        // (the text stays above these after the fade — WCAG AA for body text is 4.5)
        for (name, text_after) in [("meok", 10.0), ("hanji", 9.0)] {
            let t = crate::theme::load(name).unwrap();
            let (fg, bg) = (|k: &str| rgb(t.get(k).fg), |k: &str| rgb(t.get(k).bg));
            let (ground, surface) = (bg("ui.background"), bg("ui.popup"));
            let cases = [
                ("text", fg("ui.text"), ground),
                ("dim", fg("ui.virtual"), ground),
                ("faint", fg("ui.linenr"), ground),
                ("text on its surface", fg("ui.text"), surface),
            ];
            for (what, fg, bg) in cases {
                let (before, after) = (contrast(fg, bg), contrast(fade(fg, bg, FADE), bg));
                let lost = 1.0 - (after - 1.0) / (before - 1.0);
                assert!((0.10..=0.15).contains(&lost), "{name} {what}: {before:.2} → {after:.2} ({lost:.3})");
            }
            let after = contrast(fade(fg("ui.text"), ground, FADE), ground);
            assert!(after > text_after, "{name}: text after the fade {after:.2}");
        }
    }

    #[test]
    fn the_labels_say_where_the_keys_go() {
        assert_eq!(Keys::Code.label(), None);
        assert_eq!(Keys::Chat.label(), Some("CHAT"));
        assert_eq!(Keys::Side(crate::sidebar::Mode::Git).label(), Some("GIT"));
        assert_eq!(Keys::Slot(Slot::Terminal).label(), Some("TERMINAL"));
        assert_eq!(Keys::Picker.label(), Some("PICKER"));
        assert_eq!(Keys::Review.label(), Some("REVIEW"));
        assert_eq!(Keys::Change.label(), Some("CHANGE"));
    }
}
