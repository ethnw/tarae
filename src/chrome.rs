//! The panels' chrome — one set of rules for every panel (the layout pass, 2026-10-03: the user found the
//! padding, the gutters and the headers uneven on a real screen).
//!
//! - **Columns.** The side panel (or the review's rail) and the chat run the whole height, top to the status
//!   line; between them the code, and under the code the bottom slot. Every panel is a surface
//!   (`ui.popup`) on the ground (`ui.background`).
//! - **Gutter.** One cell of ground, `GAP`, between a panel and what's beside it — the same width and colour
//!   everywhere; a theme whose surfaces are the ground's colour gets a faint `│` there instead.
//! - **Padding.** Text starts `PAD` cells in from a panel's edge and stops `PAD_R` before its other edge —
//!   the code too: its numbers start `PAD` in (the sign column lives in that padding, so a server attaching
//!   doesn't move the text).
//! - **Header.** Every panel, and the code under the path bar, opens with a title row and a thin rule
//!   (`HEAD` rows); their tops line up on the screen's first row. (A screen under `RULE_MIN_ROWS` rows:
//!   the code's title goes without its rule.)
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
/// Ground between two regions side by side.
pub const GAP: usize = 1;
/// A header: the title row and its rule.
pub const HEAD: usize = 2;
/// Under this many screen rows the code's path bar goes without its rule (a small pane keeps the row for
/// code; the panels beside keep theirs).
pub const RULE_MIN_ROWS: u16 = 16;
/// The picker's margins over the editor area: columns, rows (a cell is about twice as tall as wide — the
/// two look the same).
pub const PICKER_MX: usize = 2;
pub const PICKER_MY: usize = 1;
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
    /// The review's file rail, or an edit's review in the buffer (its `y`/`n`).
    Review,
    Slot(Slot),
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
            Keys::Slot(Slot::Terminal) => "TERMINAL",
            Keys::Slot(Slot::Test) => "TESTS",
            Keys::Slot(Slot::Task) => "TASK",
            Keys::Slot(Slot::Procs) => "PROCS",
            Keys::Slot(Slot::Debug) => "DEBUG",
            Keys::Slot(Slot::Plugin) => "PANEL",
        })
    }

    /// Whether the code panes are what has them (they're drawn at full strength).
    pub fn code(self) -> bool {
        matches!(self, Keys::Code | Keys::Review)
    }
}

impl Editor {
    /// Where the keys go now.
    pub fn keys_at(&self) -> Keys {
        if self.picker.is_some() {
            return Keys::Picker;
        }
        if self.lane.chat.as_ref().is_some_and(|c| c.focused) {
            return Keys::Chat;
        }
        let rail = self.review_rail_up();
        if rail && self.lane.code_review.as_ref().is_some_and(|r| r.focus) {
            return Keys::Review;
        }
        if !rail && let Some(s) = self.lane.sidebar.as_ref().filter(|s| s.focus) {
            return Keys::Side(s.mode);
        }
        if self.bottom_has_keys()
            && let Some(s) = self.slot()
        {
            return Keys::Slot(s);
        }
        if self.lane.review.as_ref().is_some_and(|r| r.ready) {
            return Keys::Review;
        }
        Keys::Code
    }
}

// ── The bottom slot's tabs ───────────────────────────────────────────────

/// How a tab's panel is doing: under way (the wave), done well (`●`), failed (`▲` in the error colour),
/// wants a look (`▲` in the warning colour).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    None,
    Running,
    Passed,
    Failed,
    Warning,
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
    fn slot_there(&self, s: Slot, shown: bool) -> bool {
        match s {
            Slot::Procs => self.procs_up(),
            Slot::Debug => self.dap.is_some(),
            Slot::Terminal => self.lane.terminal.as_ref().is_some_and(|t| t.visible || !shown),
            Slot::Plugin => self.plugins.as_ref().is_some_and(|p| p.panel.is_some()),
            Slot::Test => self.lane.test_run.is_some(),
            Slot::Task => self.lane.task_run.is_some(),
        }
    }

    /// The panel you put in front (a tab clicked, `C-w t`, one just opened), while it's there.
    pub(crate) fn slot_front(&self) -> Option<Slot> {
        self.lane.slot_front.filter(|&s| self.slot_there(s, true))
    }

    /// Every panel the slot has, in `ORDER`, with how it's doing — the slot's title row.
    pub fn slot_tabs(&self) -> Vec<Tab> {
        use crate::dap::State as D;
        ORDER
            .iter()
            .filter(|&&s| self.slot_there(s, false))
            .map(|&slot| {
                let (label, badge) = match slot {
                    Slot::Terminal => {
                        let t = self.lane.terminal.as_ref();
                        let badge = match t.and_then(|t| t.exited) {
                            Some(0) | None => Badge::None,
                            Some(_) => Badge::Failed,
                        };
                        ("terminal".to_string(), badge)
                    }
                    Slot::Task => {
                        let r = self.lane.task_run.as_ref().expect("there");
                        let badge = match r.state {
                            crate::task::State::Running => Badge::Running,
                            crate::task::State::Passed if r.found == (0, 0) => Badge::Passed,
                            crate::task::State::Passed => Badge::Warning,
                            crate::task::State::Failed(_) => Badge::Failed,
                        };
                        (r.task.name.clone(), badge)
                    }
                    Slot::Test => {
                        use crate::testing::RunState as R;
                        let r = self.lane.test_run.as_ref().expect("there");
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
        let keys = self.bottom_has_keys();
        if s == Slot::Terminal
            && let Some(t) = self.lane.terminal.as_mut()
        {
            t.visible = true;
        }
        self.lane.slot_front = Some(s);
        if let Some(t) = self.lane.terminal.as_mut().filter(|_| s != Slot::Terminal) {
            t.focus = false;
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

    /// `C-w t` (and `tab` / `S-tab` while the slot has the keys): the next tab in front, or the one before.
    pub fn slot_step(&mut self, by: isize) {
        let tabs = self.slot_tabs();
        if tabs.is_empty() {
            return self.set_status("nothing at the bottom — tests, tasks and the terminal show there");
        }
        let at = self.slot().and_then(|s| tabs.iter().position(|t| t.slot == s));
        let n = tabs.len() as isize;
        let next = match at {
            Some(i) => (i as isize + by).rem_euclid(n) as usize,
            None => 0,
        };
        self.slot_show(tabs[next].slot);
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
                    Slot::Test => self.lane.test_run.as_ref().map(|r| r.started),
                    Slot::Task => self.lane.task_run.as_ref().map(|r| r.started),
                    _ => None,
                };
                (s, at)
            })
            .collect();
        let new: Vec<Slot> = now.iter().filter(|n| !self.lane.slot_seen.contains(n)).map(|n| n.0).collect();
        if !new.is_empty() {
            let keys = self.bottom_has_keys();
            // (several at once: the last in `ORDER` — the rarer, asked-for ones)
            if let Some(&s) = new.iter().rev().find(|s| !(keys && matches!(s, Slot::Test | Slot::Task))) {
                self.lane.slot_front = Some(s);
            }
        }
        self.lane.slot_seen = now;
        if self.slot() != Some(Slot::Terminal)
            && let Some(t) = self.lane.terminal.as_mut().filter(|t| t.focus)
        {
            t.focus = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fade loses the same share of contrast in both built-in themes — light text on ink and dark text
    /// on paper — and stays readable. Measured (WCAG ratio, before → after): meok text on ink 12.41 →
    /// 11.08, on its surface 11.17 → 9.98, dim 4.25 → 3.90, faint 1.75 → 1.67; hanji text on paper 11.36 →
    /// 10.20, on its surface 10.20 → 9.16, dim 3.13 → 2.89, faint 1.54 → 1.48 — 11–12% of the contrast
    /// above 1 each.
    #[test]
    fn the_fade_keeps_the_same_share_of_contrast_in_both_themes() {
        let hex = |s: &str| {
            let v = u32::from_str_radix(s.trim_start_matches('#'), 16).unwrap();
            [(v >> 16) as u8, (v >> 8) as u8, v as u8]
        };
        // (meok: paper / dim / faint on ink and on surface; hanji: ink / dim / faint on paper and surface)
        let cases = [
            ("#d6d2c8", "#101217"),
            ("#747884", "#101217"),
            ("#3a3e49", "#101217"),
            ("#d6d2c8", "#1a1d24"),
            ("#34322d", "#f5f1e8"),
            ("#8d887c", "#f5f1e8"),
            ("#cbc4b4", "#f5f1e8"),
            ("#34322d", "#ebe5d8"),
        ];
        for (fg, bg) in cases {
            let (fg, bg) = (hex(fg), hex(bg));
            let (before, after) = (contrast(fg, bg), contrast(fade(fg, bg, FADE), bg));
            let lost = 1.0 - (after - 1.0) / (before - 1.0);
            assert!((0.10..=0.15).contains(&lost), "{fg:?} on {bg:?}: {before:.2} → {after:.2} ({lost:.3})");
        }
        // The text colours themselves: readable after (WCAG AA for body text is 4.5)
        assert!(contrast(fade(hex("#d6d2c8"), hex("#101217"), FADE), hex("#101217")) > 10.0);
        assert!(contrast(fade(hex("#34322d"), hex("#f5f1e8"), FADE), hex("#f5f1e8")) > 9.0);
    }

    #[test]
    fn the_labels_say_where_the_keys_go() {
        assert_eq!(Keys::Code.label(), None);
        assert_eq!(Keys::Chat.label(), Some("CHAT"));
        assert_eq!(Keys::Side(crate::sidebar::Mode::Git).label(), Some("GIT"));
        assert_eq!(Keys::Slot(Slot::Terminal).label(), Some("TERMINAL"));
        assert_eq!(Keys::Picker.label(), Some("PICKER"));
        assert_eq!(Keys::Review.label(), Some("REVIEW"));
    }
}
