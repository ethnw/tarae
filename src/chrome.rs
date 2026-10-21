//! The panels' chrome — one set of rules for every panel (the layout pass, 2026-10-03: the user found the
//! padding, the gutters and the headers uneven on a real screen).
//!
//! - **Columns.** The side panel (or the review's rail) and the chat run the whole height, top to the status
//!   line; between them the code, and under the code the bottom slot. Every panel is a surface
//!   (`ui.popup`) on the ground (`ui.background`).
//! - **Gutter.** Half a cell of ground between regions side by side: the gutter's cell, `GAP`, is split down
//!   the middle — its left half is what's left of it (the side panel's or the review rail's surface, the
//!   bottom slot's beside the chat, the ground beside the code), its right half ground, and what's right of
//!   it starts on the next cell. So every panel's surface starts on a cell and its text is `PAD` from where
//!   the surface shows — the side panel, the slot and the chat alike. Against the screen's edge (a panel
//!   filling the screen) the panel takes the cell whole — no ground against nothing. A theme whose surfaces
//!   are the ground's colour gets a faint `│` there instead (the same middle of the cell; nothing at the
//!   edge). A card edge over a gutter's cell is seated on its left half (`screen::Grid::put`).
//! - **Padding.** Text starts `PAD` cells in from a panel's edge and stops `PAD_R` before its other edge.
//!   The code has none of its own: its sign column is the first cell after the gutter (so a server attaching
//!   doesn't move the text), its numbers right after — they inset its text already — and its path bar and
//!   rule start where the numbers do (`SIGNS`).
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
/// The code's cells before its numbers: the sign column, right after the gutter.
pub const SIGNS: usize = 1;
/// The gutter between two regions side by side: a cell, half of it ground.
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
            Keys::View(crate::view::Surface::Chat) => "VIEW · CHAT",
            Keys::View(crate::view::Surface::Terminal) => "VIEW · TERMINAL",
            Keys::View(crate::view::Surface::Task) => "VIEW · TASK",
            Keys::View(crate::view::Surface::Preflight) => "VIEW · PREFLIGHT",
            Keys::View(crate::view::Surface::Tests) => "VIEW · TESTS",
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
    /// A view mode's cursors (`view.rs`), painted on its rows.
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
            R::View(_) => CursorIn::View,
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
            R::Program => Keys::Program,
            R::Point => Keys::Point,
            R::View(s) => Keys::View(s),
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

    /// The panel you put in front (a tab clicked, `C-w t`, one just opened), while it's there.
    pub(crate) fn slot_front(&self) -> Option<Slot> {
        self.session.slot_front.filter(|&s| self.slot_there(s, true))
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
        assert_eq!(Keys::Change.label(), Some("CHANGE"));
    }
}
