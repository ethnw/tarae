//! Editor state + key dispatch. Knows nothing of the terminal — so it can be tested whole via key sequences.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::Result;

use crate::commands::{self, Context};
use crate::config::{Config, EditorConfig};
use crate::document::{DocId, Document, Snapshot};
use crate::event::{Event, Queue};
use crate::key::{Code, Key};
use crate::keymap::{Keymaps, Lookup, MappableCommand};
use crate::llm::{self, Warm};
use crate::lsp_editor::LspState;
use crate::movement as mv;
use crate::picker::{Action, Picker};
use crate::search;
use crate::selection::{Range, Selection};
use crate::session::At;
use crate::syntax;
use crate::theme::Theme;
use crate::typed;

/// Files larger than this are read in the background. Small files take only a few ms synchronously,
/// so async would only add flicker.
const ASYNC_LOAD_BYTES: u64 = 1 << 20;

/// Who opens a file (`Editor::open_as`): you, Claude's turn (`claudes_only`) or a code review (`review_only`) —
/// for the last two no grammar or server is offered until you take the file up.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Opener {
    You,
    Claude,
    Review,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    Select,
}

/// The last motion `A-.` repeats (a closure capturing the count, target char, etc.).
pub type Motion = Rc<dyn Fn(&mut Context)>;
/// A command that takes one character from the next key and runs.
pub type CharFn = fn(&mut Context, char);

/// Kind of single-line command-line input — same input box, only what Enter does differs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptKind {
    Command,
    Search {
        reverse: bool,
    },
    SelectRegex,
    Split,
    Keep {
        remove: bool,
    },
    Rename,
    /// Debugger: breakpoint condition / log message / watch expression.
    BreakCondition,
    LogMessage,
    Watch,
    /// `|` `A-|` `!` `A-!` `$` — a shell command for the selections (shell.rs).
    Shell(crate::shell::Pipe),
    /// `C-r` in the global search results — the replacement text.
    Replace,
    /// `space n`: a reply to note N, or (None) a question starting a note on this line.
    Note(Option<u64>),
    /// A plugin's line (its token; its label is the prompt's `label`).
    Plugin(u64),
    /// `space W o`: a new session's name (`worktree.rs`; the label says what it's made from).
    Session,
}

impl PromptKind {
    pub fn label(&self) -> &'static str {
        match self {
            PromptKind::Command => ":",
            PromptKind::Search { reverse: false } => "/",
            PromptKind::Search { reverse: true } => "?",
            PromptKind::SelectRegex => "select:",
            PromptKind::Split => "split:",
            PromptKind::Keep { remove: false } => "keep:",
            PromptKind::Keep { remove: true } => "remove:",
            PromptKind::Rename => "rename:",
            PromptKind::BreakCondition => "break when:",
            PromptKind::LogMessage => "log:",
            PromptKind::Watch => "watch:",
            PromptKind::Shell(p) => p.label(),
            PromptKind::Replace => "replace with:",
            PromptKind::Note(Some(_)) => "reply:",
            PromptKind::Note(None) => "ask claude:",
            PromptKind::Plugin(_) => ">",
            PromptKind::Session => "new session:",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Prompt {
    pub kind: PromptKind,
    /// Instead of the kind's own label (a plugin's).
    pub label: Option<String>,
    pub text: String,
    /// Cycling `:` completions with Tab: (input when cycling began, current candidate index). Typing ends it.
    pub cycle: Option<(String, usize)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    Info,
    Error,
}

/// A notice that pops up briefly at the top right (save, reload, claude answer …).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Clone, Debug)]
pub struct Toast {
    pub text: String,
    pub kind: ToastKind,
    pub at: std::time::Instant,
    /// How long it stays up: the time to read it (see `Toast::new`).
    pub life: std::time::Duration,
    /// A step under way (`Editor::set_step`) — up until its end is said (`step_done`).
    pub step: Option<&'static str>,
    /// It has been on the notification card. An error or warning that hasn't (a picker over it, no free
    /// corner) waits: its time doesn't run until it has been (U7, `Editor::toasts_drawn`).
    pub shown: bool,
    /// Which message it is — the count of messages said when it came (`toast#N` in the UI state).
    pub id: u64,
    /// The session it's news of, when that's one behind (said there, or about it) — Claude's level of the
    /// state leaves it out for any other session's Claude (other sessions are counts there).
    pub of: Option<crate::event::SessionId>,
    /// The person's alone: never at any Claude's level (an overlap — a Claude hears nothing of one, `overlap.rs`).
    pub persons: bool,
    /// Words painted in a theme scope (`""`: as the text), in order — each the first found after the one before:
    /// an overlap's two names in their participant colours and its `≈` in warning.
    pub paint: Vec<(String, &'static str)>,
    /// Said while `:messages` was up: only its row there if the frame shows that row, else a toast as usual —
    /// decided once, by the first frame that has it (`messages::rows_drawn`).
    pub row: bool,
}

/// Two clicks on the same picker row this close together are a double click (it opens the row).
const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(400);

/// The longest a step's toast stays up without its end being said.
const STEP_MAX: std::time::Duration = std::time::Duration::from_secs(120);

/// Messages `:messages` keeps (and the most a card holds).
const MESSAGES_KEPT: usize = 300;

/// An error stays up until a key is pressed — but at least this long, so a key already on its way doesn't
/// take it before it's read.
pub(crate) const ERROR_MIN: std::time::Duration = std::time::Duration::from_secs(4);

/// Whether `key` submits a prompt (the command line runs its line) — Enter, with any modifier. The one test:
/// `handle_prompt_key` goes by it, and so does a shuttle's guard on `:` lines (`shuttle.rs`).
pub(crate) fn prompt_submits(key: Key) -> bool {
    key.code == Code::Enter
}

#[cfg(test)]
thread_local! {
    /// The `frames owed` lines this thread's editors logged (`Editor::frame_due` — the log writes nothing in
    /// tests).
    pub static FRAMES_OWED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    /// A test's own step after every event's work, among the syncs (`handle_event`) — one that says, as
    /// #200's look at a card behind did, that the event changed nothing.
    pub static AFTER_EVENT: std::cell::Cell<Option<fn(&mut Editor)>> = const { std::cell::Cell::new(None) };
}
/// The fade-out at the end of a toast's life.
const TOAST_FADE: std::time::Duration = std::time::Duration::from_millis(400);
/// …and the fade-in when a card first shows.
const TOAST_FADE_IN: std::time::Duration = std::time::Duration::from_millis(150);
/// One animation frame (waves, fades, elapsed seconds) — 10 a second, only while something moves: what
/// other terminal tools wait at (measured 2026-10-03: Gemini CLI's spinner 80 ms, Claude Code a frame every
/// ~107 ms), a third fewer frames than the 70 ms it was (N6).
pub const TICK: std::time::Duration = std::time::Duration::from_millis(100);

/// The clock the waiting animation steps on: every wave on screen takes the same step, and a waiting tick
/// is set for the next step (`step_in`) — ticks timed from when the last one was handled drifted, and now
/// and then a wave skipped a step.
pub fn anim_clock() -> std::time::Instant {
    #[cfg(not(test))]
    {
        static CLOCK: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
        *CLOCK.get_or_init(std::time::Instant::now)
    }
    // Tests: a clock each (a test runs on a thread of its own) — a snapshot's wave is at its first step,
    // however long the run has been going
    #[cfg(test)]
    {
        thread_local!(static CLOCK: std::time::Instant = std::time::Instant::now());
        CLOCK.with(|c| *c)
    }
}

/// The waiting animation's step now (`anim_clock`, a `TICK` a step).
pub fn anim_step() -> u128 {
    anim_clock().elapsed().as_nanos() / TICK.as_nanos()
}

/// From `elapsed` on the animation's clock to just past its next step (2 ms past — the frame drawn then is
/// surely on it).
fn step_in(elapsed: std::time::Duration) -> std::time::Duration {
    let into = elapsed.as_nanos() % TICK.as_nanos();
    TICK - std::time::Duration::from_nanos(into as u64) + std::time::Duration::from_millis(2)
}

/// Documents that keep their parse trees however long ago they were seen (`trees_follow_sight`).
pub const TREES_RECENT: usize = 5;

/// Past the most recent few, how long a document keeps its trees after it was last on screen: 3 minutes
/// (`TARAE_TREES_UNSEEN` = seconds instead — measurements and tests that can't wait that long).
pub fn trees_unseen() -> std::time::Duration {
    static UNSEEN: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *UNSEEN.get_or_init(|| {
        let secs = std::env::var("TARAE_TREES_UNSEEN").ok().and_then(|s| s.parse().ok()).unwrap_or(180);
        std::time::Duration::from_secs(secs)
    })
}

/// The live search (`space /`) runs once typing pauses this long — a burst of keys is one search, not one
/// each (each key stops the one under way anyway).
pub const LIVE_PAUSE: std::time::Duration = std::time::Duration::from_millis(50);

impl Toast {
    /// Up for as long as it takes to read (1.2 s + 60 ms a character), within a range per kind — a long
    /// message isn't gone before its end. An error waits for a key (`Editor::handle_key`), up to a minute.
    pub fn new(kind: ToastKind, text: String) -> Self {
        let read = 1200 + 60 * text.chars().count() as u64;
        let ms = match kind {
            ToastKind::Success => read.clamp(2500, 8000),
            ToastKind::Info => read.clamp(3500, 10_000),
            ToastKind::Warning => read.clamp(6000, 15_000),
            ToastKind::Error => 60_000,
        };
        let life = std::time::Duration::from_millis(ms);
        Toast {
            text,
            kind,
            at: crate::clock::now(),
            life,
            step: None,
            shown: false,
            id: 0,
            of: None,
            persons: false,
            paint: Vec::new(),
            row: false,
        }
    }

    pub fn alive(&self) -> bool {
        self.age() < self.life
    }

    /// An error or warning no card has shown yet (U7) — its time doesn't run.
    pub fn waiting(&self) -> bool {
        !self.shown && matches!(self.kind, ToastKind::Error | ToastKind::Warning)
    }

    /// How long it has been up — none while it waits for a card (its time starts when one shows it).
    pub fn age(&self) -> std::time::Duration {
        if self.waiting() { std::time::Duration::ZERO } else { crate::clock::since(self.at) }
    }
}

/// The folder tarae was started in (`main` moves there for `tarae <folder>` before the editor starts),
/// links resolved — the editor's `root`. Read once, by `main` at startup (and by the tests' `Editor::new`):
/// everything after goes by `Editor::root()`, so a root of its own (a test's temp folder, later a session's
/// worktree) is honored everywhere.
pub fn started_in() -> PathBuf {
    let dir = std::env::current_dir().unwrap_or_default();
    std::fs::canonicalize(&dir).unwrap_or(dir)
}

impl Screen {
    /// The rows the last frame drew of document `doc` (`codeview::Row::origin`s) — of every pane that drew it; a
    /// pane switched to it since drew another's lines, not its.
    pub fn rows_of(&self, doc: DocId) -> impl Iterator<Item = &(usize, usize, usize, usize)> {
        self.panes.iter().zip(&self.pane_docs).filter(move |(_, d)| **d == doc).flat_map(|(p, _)| p.3.iter())
    }

    /// Kind `kind`'s list as the last frame drew it (`lists`).
    pub fn list(&self, kind: crate::pane::GroupKind) -> Option<(usize, usize, usize)> {
        self.lists.borrow().iter().find(|l| l.0 == kind).map(|l| l.1)
    }

    /// The column past the bottom slot's last — the chat's first (or a group docked right's), as last drawn.
    pub fn slot_end(&self) -> usize {
        match self.slot_w {
            0 => self.chat_x.unwrap_or(usize::MAX),
            w => self.slot_x + w,
        }
    }
}

/// A list as drawn (`Screen.lists`): its first row's screen row, the rows it has, the first of its rows in sight.
pub type ListDrawn = (usize, usize, usize);

/// Where each run panel drew its output this frame (`Screen.run_rows`) — one, or a docked group's beside the slot's.
#[derive(Clone, Debug, Default)]
pub struct RunRows(std::cell::RefCell<Vec<(crate::sizes::Slot, usize, usize, usize)>>);

impl RunRows {
    /// Panel `r.0`'s: (its panel, first screen row, the text's first column, rows).
    pub fn set(&self, r: Option<(crate::sizes::Slot, usize, usize, usize)>) {
        if let Some(r) = r {
            let mut all = self.0.borrow_mut();
            all.retain(|e| e.0 != r.0);
            all.push(r);
        }
    }

    /// The one recorded last.
    #[cfg(test)]
    pub fn get(&self) -> Option<(crate::sizes::Slot, usize, usize, usize)> {
        self.0.borrow().last().copied()
    }

    /// Panel `slot`'s.
    pub fn of(&self, slot: crate::sizes::Slot) -> Option<(crate::sizes::Slot, usize, usize, usize)> {
        self.0.borrow().iter().find(|e| e.0 == slot).copied()
    }
}

/// Screen layout of the last frame (term.rs fills it on every draw).
#[derive(Clone, Debug, Default)]
pub struct Screen {
    /// Per pane: (view, pane rect, line-number gutter width, per screen row `codeview::Row::origin`).
    #[allow(clippy::type_complexity)]
    pub panes: Vec<(crate::split::PaneId, crate::split::Rect, usize, Vec<(usize, usize, usize, usize)>)>,
    /// Every pane of the tree that had room, where it was laid out (panes P2b) — what `C-w h/j/k/l` hop between
    /// and the dividers are found from. Filling the screen, the one pane.
    pub leaves: Vec<(crate::split::PaneId, crate::split::Rect)>,
    /// How each group laid out was laid (side by side, over and under, stacked) — what `C-w t` turns.
    pub shapes: Vec<(crate::pane::GroupKind, crate::pane::Shape)>,
    /// The document each of `panes` drew (in their order) — its rows are that one's lines, whatever the pane
    /// shows since (`rows_of`).
    pub pane_docs: Vec<DocId>,
    /// The focused pane's edit being written (`writing.rs`): its rows of changes on screen and the one being
    /// written (screen rows) — the follow card keeps off them as off a review's changes.
    pub writing_rows: Vec<usize>,
    pub writing_head: Option<usize>,
    /// Column where the chat pane starts.
    pub chat_x: Option<usize>,
    /// The chat's rows as this frame laid them out (`chatdoc::Laid` — its transcript follows them) and where its
    /// conversation was drawn (`chat_view`: first screen row, width, rows, which of its rows is on the first).
    pub chat_laid: std::cell::RefCell<crate::chatdoc::Laid>,
    /// The cells a cursor was painted in this frame (x, y) — the code's block and others, a view's: with the
    /// terminal's own cursor, one place on screen (`Editor::cursor_in`; the test that counts them).
    pub cursors: std::cell::RefCell<Vec<(usize, usize)>>,
    pub chat_view: std::cell::Cell<Option<(usize, usize, usize, usize)>>,
    /// Where the run panel in front drew its output (the tests' detail): (its panel, first screen row, the text's
    /// first column, rows) — the mouse finds a row of its view there.
    pub run_rows: RunRows,
    /// …and its columns, each panel's: how wide its text is (`rundoc.rs` keeps the cursor's column in sight).
    pub run_cols: std::cell::RefCell<Vec<(crate::sizes::Slot, usize)>>,
    /// The groups docked in sight as drawn: their kind and their leaf's rect (P3 step 6) — the mouse finds them.
    pub docked: std::cell::RefCell<Vec<(crate::pane::GroupKind, crate::split::Rect)>>,
    /// The floating panes as drawn, bottom first: their kind (the session list's `Sessions`) and their frame's rect
    /// (P3 step 7) — the mouse finds the topmost, the code's cursor under one is hidden.
    pub floating: std::cell::RefCell<Vec<(crate::pane::GroupKind, crate::split::Rect)>>,
    /// The rows above the status line (None before the first frame): floating panes are placed on them and the width.
    pub height: Option<usize>,
    /// The groups docked over the code folded into the slot this frame, by `dock_index` — the code and the slot under
    /// them leave them too few rows (`term::over_fold`).
    pub over_folded: [bool; crate::pane::DOCKABLE],
    /// The bottom slot's width (from `slot_x`) — a group docked right of the code is past it.
    pub slot_w: usize,
    /// First row and row count of the picker list.
    pub picker_list: Option<(usize, usize)>,
    /// Grammar download offer buttons: (row, start col, end col, key to feed when clicked).
    pub offer_buttons: Vec<(usize, usize, usize, char)>,
    /// The front card as drawn, its back cards' rows too (x, y, width, height) — the UI state's `at`.
    pub offer_card: Option<(usize, usize, usize, usize)>,
    /// How many of the front card's lines its drawing left out (`↓ 2 more lines`; 0: drawn whole) — None
    /// when no card is drawn, or it stepped aside to a bar while its text is read (the UI state's `cut`).
    pub offer_cut: Option<usize>,
    /// Column where the picker's preview card starts (the wheel scrolls it there) · the cells it takes (x, y,
    /// width, height — to the code's right edge, its whole height: the screen sent blanks it over a file named
    /// like a key, `reads::key_panes`).
    pub picker_preview_x: Option<usize>,
    pub picker_preview: Option<(usize, usize, usize, usize)>,
    /// The picker list's card's columns (first, end) — its rows are `picker_list`'s: the screen sent blanks a
    /// row carrying a line of a file named like a key (`reads::key_panes`).
    pub picker_list_cols: Option<(usize, usize)>,
    /// The floating docs card this frame (x, y, width, height) — drawing fills it in; the wheel scrolls it.
    pub popup: std::cell::Cell<Option<(usize, usize, usize, usize)>>,
    /// Clickable spots in the chat pane: (row, start col, end col, where it points) — drawing fills them in.
    pub chat_links: std::cell::RefCell<Vec<(usize, usize, usize, crate::chat::Link)>>,
    /// The chat's rows of tool calls: (row, call id) — a click goes to that call's replay stop.
    pub chat_calls: std::cell::RefCell<Vec<(usize, String)>>,
    /// Where a command of Claude's ran, at its row's end: (row, start col, end col, its call id) — a click says
    /// why (`chat::said_ran`).
    pub chat_ran: std::cell::RefCell<Vec<(usize, usize, usize, String)>>,
    /// UI elements' ids in Claude's answers: (row, start col, end col, the id) — a click brings it into sight.
    pub chat_ids: std::cell::RefCell<Vec<(usize, usize, usize, String)>>,
    /// The minimized chat card (x, y, width, height) — a click on it brings the panel back.
    pub chat_mini: std::cell::Cell<Option<(usize, usize, usize, usize)>>,
    /// The sessions' tabs (the chat's top row) and the status line's chip: (row, start col, end col, what a
    /// click does).
    pub session_spots: std::cell::RefCell<Vec<(usize, usize, usize, crate::session::Spot)>>,
    /// The code review's file rail: (row, file index) — a click opens that file.
    pub review_rail: std::cell::RefCell<Vec<(usize, usize)>>,
    /// The rail's file rows' viewed marks: (row, file index, column) — a click there marks it viewed (#67) ·
    /// its `↑ N more` / `↓ N more` rows: (row, rows to scroll by) — a click scrolls it (#73).
    pub review_rail_marks: std::cell::RefCell<Vec<(usize, usize, usize)>>,
    pub review_rail_more: std::cell::RefCell<Vec<(usize, isize)>>,
    /// A preflight's step chips in the task panel: (row, from column, to column, step) — a click scrolls its
    /// output to that step's (#72).
    pub task_steps: std::cell::RefCell<Vec<(usize, usize, usize, usize)>>,
    /// The side panel's rows: (screen row, list row) — a click opens it.
    pub sidebar: std::cell::RefCell<Vec<(usize, usize)>>,
    /// The whole screen's width (None before the first frame — tests) · the chat's, the side panel's (0: not shown — folded away if it's open) and
    /// the bottom slot's (rows) as drawn last frame (`sizes.rs` starts a change from these).
    pub width: Option<usize>,
    pub chat_w: usize,
    pub side_w: usize,
    /// The code review's rail, as wide as drawn (0: not shown).
    pub rail_w: usize,
    pub bottom_h: usize,
    /// The rows of code and bottom slot together, from row `text_top`.
    pub body_h: usize,
    pub text_top: usize,
    /// The terminal panel: its rows on screen (first — its tabs —, last) when it shows · the size a
    /// terminal gets.
    pub terminal_rows: Option<(usize, usize)>,
    /// The bottom slot's first column · its tabs as drawn: (row, start col, end col, the panel).
    pub slot_x: usize,
    pub slot_tabs: std::cell::RefCell<Vec<(usize, usize, usize, crate::sizes::Slot)>>,
    /// The names of the group's members in the slot's line as drawn (panes P3 step 3b): (row, start col, end col, the
    /// member's pane) — a click gives it the keys.
    pub member_names: std::cell::RefCell<Vec<(usize, usize, usize, crate::split::PaneId)>>,
    pub terminal_size: std::cell::Cell<Option<(u16, u16)>>,
    /// The test panel: its rows (first, last) and how many of them the detail has — the wheel scrolls it.
    pub test_panel: std::cell::Cell<Option<(usize, usize, usize)>>,
    /// A linked group's list, as drawn beside its detail (`pane::Group`), by kind: its first row's screen row, the
    /// rows it has, the first of its rows in sight — a click picks a row, its keys move by its rows.
    pub lists: std::cell::RefCell<Vec<(crate::pane::GroupKind, ListDrawn)>>,
    /// The in-buffer review's change in hand: its first and last screen rows.
    pub review_change: std::cell::Cell<Option<(usize, usize)>>,
    /// The in-buffer review's changes: every screen row of one (its header, its lines) and which change — cards
    /// keep off them, a click takes that change in hand.
    pub review_rows: std::cell::RefCell<Vec<(usize, usize)>>,
    /// The task panel's: (first, last, rows of output).
    pub task_panel: std::cell::Cell<Option<(usize, usize, usize)>>,
    /// A plugin's panel: (its first screen line, its last, its first row's line, the first row shown).
    pub plugin_panel: std::cell::Cell<Option<(usize, usize, usize, usize)>>,
    /// `:procs`' panel as drawn (for the mouse).
    pub procs_panel: std::cell::Cell<Option<crate::procs::Area>>,
    /// The note card and the thought card this frame (x, y, width, height).
    pub note_card: std::cell::Cell<Option<(usize, usize, usize, usize)>>,
    pub thought_card: std::cell::Cell<Option<(usize, usize, usize, usize)>>,
    /// The notification card (x, y, width, height) — None when there's none, or it's folded into the status
    /// line.
    pub toast_card: std::cell::Cell<Option<(usize, usize, usize, usize)>>,
    /// How strong this frame drew the notification card, in percent (its fade-in and fade-out —
    /// `Editor::toast_alpha`, rounded down: 100 only once it's whole) — None with no card drawn.
    pub toast_strength: std::cell::Cell<Option<u8>>,
    /// For the notification card up: whether it's said on the status line, the code too narrow for it —
    /// decided once, as it first shows; None with no card (`draw_toasts`).
    pub toast_narrow: std::cell::Cell<Option<bool>>,
    /// This frame's routine message folded into the status line for narrow code — said there, outranking the
    /// rest (`draw_statusline`).
    pub toast_said: std::cell::Cell<bool>,
    /// The elements Claude can be told about, where they're drawn (a card's title row, a toast's, a panel
    /// row, the path bar, a slot tab) — pointing labels them (`uistate/point.rs`).
    pub spots: std::cell::RefCell<Vec<crate::uistate::point::Spot>>,
    /// The prompt's card (x, y, width, height) — a click on it is its own, not the code's under it.
    pub prompt_card: std::cell::Cell<Option<(usize, usize, usize, usize)>>,
    /// The front card wasn't drawn this frame only because a key sequence is under way (`C-w` of `C-w w`: a card
    /// steps aside for which-key) — still in sight for the round (`cycle_view`).
    pub offer_held: bool,
    /// The messages list's text room as last drawn (cells) — its `enter` opens a row cut to it whole.
    pub messages_room: std::cell::Cell<usize>,
    /// The notification card's messages as drawn, top to bottom: (screen row, toast id) — a press on one when focused
    /// selects it.
    pub toast_rows: std::cell::RefCell<Vec<(usize, u64)>>,
    /// The floating lists' rows on screen, for the state's panels (`messages:toast#N`).
    pub list_rows: std::cell::RefCell<Vec<(crate::pane::GroupKind, crate::uistate::panels::Row)>>,
    /// What an element drew of this frame, to tell whether the finished frame still shows it whole
    /// (`term::shown_whole`): its id, its cells (row, columns) and the bytes of the frame that drew them.
    pub owned: std::cell::RefCell<Vec<Owned>>,
}

/// An element's cells on a frame and the bytes that drew them (`Screen.owned`).
#[derive(Clone, Debug, Default)]
pub struct Owned {
    pub id: String,
    pub row: usize,
    pub cols: std::ops::Range<usize>,
    pub bytes: std::ops::Range<usize>,
}

/// One pane of a split: what it shows (a document) + its scroll. Cursor and selection are the document's.
#[derive(Clone, Debug)]
pub struct View {
    pub id: crate::split::PaneId,
    pub doc: DocId,
    pub top: usize,
    pub left: usize,
    /// The rows of its top line above it (`Document.top_rows`) — its own, as its `top`.
    pub top_rows: Option<(usize, usize)>,
    /// Spots to come back to (`C-o`/`C-i`) · the document shown before this one (`ga`).
    pub jumps: crate::jumplist::JumpList,
    pub last_doc: Option<DocId>,
}

impl View {
    pub fn new(id: crate::split::PaneId, doc: DocId) -> Self {
        Self { id, doc, top: 0, left: 0, top_rows: None, jumps: Default::default(), last_doc: None }
    }

    /// Shows another document in its place (the one it showed went): from its top — the old one's scroll, rows
    /// of its top line included, aren't the new one's.
    pub fn show(&mut self, doc: DocId) {
        (self.doc, self.top, self.left, self.top_rows) = (doc, 0, 0, None);
    }
}

/// Frames kept with what drew them (`Editor.frames_for`).
pub const FRAMES_FOR_KEPT: usize = 16;

/// What drew a frame (`Editor.frames_for`, the state's `frames_for`): each event of the batch before it that
/// changed something, or why one was drawn though none did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DrawnFor {
    /// Input or the terminal: `key`, `mouse`, `paste`, `focus`, `resize`, `hover`.
    Event(&'static str),
    /// A job's result, by where it was sent from (`Jobs::spawn`'s caller, `Tx::send`'s).
    Job(&'static std::panic::Location<'static>),
    /// A job's result that names itself (`Editor::drawn_for`), or the main loop's own: `a tick`, `a git watch
    /// round`, `the resting redraw`, `the first frame`.
    Named(&'static str),
    /// Every event said nothing changed, but what came no frame had shown (`Editor::frame_owed`).
    Owed(String),
}

impl DrawnFor {
    /// An event's own kind; None for a job's result (its sender says it as it's applied).
    pub fn of(ev: &Event) -> Option<DrawnFor> {
        Some(DrawnFor::Event(match ev {
            Event::Key(_) => "key",
            Event::Mouse(_) => "mouse",
            Event::Hover(..) => "hover",
            Event::Focus(_) => "focus",
            Event::Resize => "resize",
            Event::Paste(_) => "paste",
            Event::Job(_) | Event::For(..) => return None,
        }))
    }
}

impl std::fmt::Display for DrawnFor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DrawnFor::Event(what) | DrawnFor::Named(what) => f.write_str(what),
            DrawnFor::Job(at) => write!(f, "job {}:{}", at.file(), at.line()),
            DrawnFor::Owed(what) => write!(f, "owed: {what}"),
        }
    }
}

pub struct Editor {
    pub docs: Vec<Document>,
    pub current: usize,
    /// The session in front — its checkout, panes, chat, terminal, tests, tasks, git (`session.rs`).
    pub session: crate::session::Session,
    /// The sessions behind it (switching swaps one with `session`) · the last session id given · a result of
    /// a session behind is being applied (`with_session` — what would take the screen waits in that session).
    pub sessions: Vec<crate::session::Session>,
    pub(crate) next_session: crate::event::SessionId,
    /// Where every session is reading and writing, and what draws it in other checkouts (`presence.rs`).
    pub presence: crate::presence::State,
    /// Where two workspaces' changes would conflict, and what each was last taken as (`overlap.rs`).
    pub overlap: crate::overlap::State,
    /// The workspaces kept close to their base: its tips watched, its moves asked and made (`keepclose.rs`).
    pub keep_close: crate::keepclose::State,
    pub(crate) behind: bool,
    pub(crate) next_pane: crate::split::PaneId,
    pub mode: Mode,
    pub keymaps: Keymaps,
    pub config: EditorConfig,
    pub theme: Theme,
    /// Registers — name → per-range text. `"` is the default, `_` discards, `+` is the system clipboard.
    pub registers: HashMap<char, Vec<String>>,
    /// Register picked with `"x` for the next command.
    pub selected_register: Option<char>,
    /// Command awaiting one character from the next key (`f t F T r "`) + its count.
    pub on_next_char: Option<(CharFn, Option<usize>)>,
    /// First char of a two-char command (`mr x y`).
    pub char_arg: Option<char>,
    /// Recording a macro (register, keys). Replay = feeding the keys in again.
    pub recording: Option<(char, Vec<Key>)>,
    pub macros: HashMap<char, Vec<Key>>,
    /// Registers of the macros being replayed (a macro may replay another one, never itself).
    pub replaying: Vec<char>,
    /// `.`: the insert being recorded · the last finished one · replaying it now (repeat.rs).
    pub(crate) insert_rec: Option<crate::repeat::LastInsert>,
    pub(crate) last_insert: Option<crate::repeat::LastInsert>,
    pub(crate) repeating_insert: bool,
    /// `A-o` steps (doc, version, selection before, after) — `A-i` walks back while nothing else changed.
    pub(crate) expand_history: Vec<(DocId, u64, Selection, Selection)>,
    /// Replace across files: (pattern, listed matches per file) waiting for the replacement · the plan
    /// shown in the preview picker.
    pub(crate) replace_targets: Option<(String, crate::replace::Targets)>,
    pub(crate) replace_plan: Vec<crate::replace::FilePlan>,
    /// Blame (gitmenu.rs — `b` in the git tab).
    pub blame: crate::gitmenu::BlameState,
    /// Files told why they get no git (`gitrepo::NoGit::Writable`) — said once each, since tarae started.
    pub(crate) git_told: std::collections::HashSet<PathBuf>,
    /// Index reads for the gutter that came back (`git_load_base`) — what tests wait on.
    pub(crate) git_bases_read: u64,
    /// Rebase lines read on a worker that came back (`rebase::rebase_git`) — what tests wait on.
    pub(crate) rebase_lines_read: u64,
    /// Sessions marked in `space W` and the bring queue going (`bringqueue.rs`) — the editor's, not a session's.
    pub bring_queue: crate::bringqueue::Queue,
    /// `gw` labels on screen (labels.rs) — the next two keys pick one.
    pub jump_labels: Option<crate::labels::Labels>,
    /// Length of the key sequence behind the last command — to drop `Q` itself when recording stops.
    pub last_trigger_len: usize,
    /// Some while entering a command line.
    pub prompt: Option<Prompt>,
    /// Last search term (`n`/`N`, `*`).
    pub search: Option<String>,
    /// Whether to highlight all matches of the last search (on after a search, off with Esc in normal mode).
    pub search_hl: bool,
    pub search_hits: Option<search::SearchHits>,
    /// Position before the `/` preview moved the view (Esc returns there).
    pub search_origin: Option<usize>,
    pub status: Option<(String, Severity)>,
    pub should_quit: bool,
    /// Text area (rows, cols) — the terminal updates it every frame. Used by page motions.
    pub viewport: (usize, usize),
    pub last_motion: Option<Motion>,
    /// Visual column held across consecutive vertical moves (per range). Dropped on a non-vertical command.
    pub sticky: Option<Vec<usize>>,
    pub sticky_used: bool,
    /// Queue for keys and job results. Slow work goes through `events.jobs().spawn(..)`.
    pub events: Queue,
    /// Writes documents off the main loop (`saver.rs`).
    pub saver: crate::saver::Saver,
    /// Language servers and sent requests.
    pub lsp: LspState,
    /// Text floating near the cursor (hover). Any key closes it.
    pub popup: Option<Vec<crate::markdown::Line>>,
    /// How many lines the floating text is scrolled down (C-d / C-u).
    pub popup_scroll: usize,
    /// Insert-mode completion list.
    pub completion: Option<crate::completion::Completion>,
    /// Function signature floating above the cursor (on `(`·`,` in insert mode).
    pub signature: Option<crate::signature::Signature>,
    /// Open picker (files, buffers, search results).
    pub picker: Option<Picker>,
    /// The last picker closed (`space '`).
    last_picker: Option<Picker>,
    picker_ids: u64,
    /// The live search's latest key (N4) — a search under way for an earlier one stops (`grep::search`
    /// asks before each file and at each match).
    live_search: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub llm_warm: Warm,
    pub llm_generation: u64,
    /// The ask process awaiting an answer (killed on cancel).
    pub llm_pid: Option<u32>,
    /// `:usage`'s buffer and the session it's about — said again as its turns end (`usage.rs`).
    pub usage_view: Option<(DocId, crate::event::SessionId)>,
    /// (document, line, byte column) to put the cursor on once that document finishes loading.
    pub goto_after_load: Option<(DocId, usize, usize)>,
    /// Keys, clicks, drags, the wheel over the code and pastes so far — follow tells the user moving the
    /// cursor from a reload moving it.
    pub input_seq: u64,
    /// The person's own keys, clicks and pastes — the terminal's input, not a shuttle's steps (`shuttle.rs`): what
    /// only a person's look may do goes by it (follow's pause, a card's place among yours).
    pub person_seq: u64,
    /// A program typing over the UI socket, its grants, the step under way (`shuttle.rs`).
    pub shuttles: crate::shuttle::Shuttles,
    /// Debug run · breakpoints (kept without a debug run) · debug run generation (drops late messages).
    pub dap: Option<crate::dap::Dap>,
    pub breakpoints: crate::dap::Breakpoints,
    /// WebAssembly plugins: their thread and the commands they registered (`None` until there are some).
    pub plugins: Option<crate::plugin::Plugins>,
    /// The processes tarae started, their costs and its checks on them (`:procs`, `procs.rs`).
    pub procs: crate::procs::Procs,
    /// Lists used by `:` completion (folders, themes — filled by a worker thread) · folders being read.
    pub cmd_lists: crate::cmdline::Lists,
    /// Grammars already offered for download (since tarae started).
    pub grammar_hinted: std::collections::HashSet<String>,
    /// Offers asking to download (arrival order — the front one gets keys, offer.rs).
    pub offers: Vec<crate::offer::Offer>,
    pub offer_seq: u64,
    /// Git-tab actions waiting on their hooks card (`hooks.rs`): the card's id, the session, the action.
    pub hook_waits: Vec<crate::hooks::Wait>,
    /// The card (id, rev) last in sight whole — drawn uncut, or read to its end in its `v` buffer (a weighty
    /// `y` waits for it).
    pub offer_whole: std::cell::Cell<Option<(u64, u64)>>,
    /// The card (id, rev) a `y` came to before it was seen whole — its cut row says why in the warning
    /// color (#84: the toast alone was missed, the `y` seemed ignored).
    pub offer_early: std::cell::Cell<Option<(u64, u64)>>,
    /// The card (id, rev) whose whole text `v` opened, in this buffer — seen whole once its last line has
    /// been on screen (opening it isn't reading it: a line 50 would go unseen).
    pub offer_view: std::cell::Cell<Option<(u64, u64, DocId)>>,
    /// Every card's `v` buffer and the card it's of (E17: closed with that card, not with the last card of
    /// its title — one "Claude wants to run" parked in another session kept them all open).
    pub offer_views: Vec<(DocId, u64)>,
    /// Config files auto-save held back because they don't parse (`disk::unparsed_config`) — the version
    /// it was said for, so it's said once per change.
    pub auto_held: Vec<(DocId, u64)>,
    /// The texts of cards seen whole since tarae started (`offer::text_hash`): the same card again — a script run
    /// again unchanged — isn't to be read again.
    pub offer_read: std::cell::RefCell<std::collections::HashSet<u64>>,
    /// Whether to show offers (off in tests — so they don't swallow y/n keys).
    pub offer_popups: bool,
    /// 번짐: the head of text being written drawn as ink not yet dry (`term::bleed`) — off in tests, as the
    /// animation is (a test that looks at it turns it on).
    pub bleed_ink: bool,
    /// Motions on a clock under way (`motion.rs`): flashes, … — each let go once over (`motions_prune`).
    pub motions: Vec<crate::motion::Moving>,
    /// A hunk reset waiting on its diff (§5): (the document, the version it left) — a turn's file's `u` shows once
    /// that version's diff lands empty, and is pressed then (`Editor::diff_landed`).
    pub press_on_diff: Option<(crate::document::DocId, u64)>,
    /// Whether motions are drawn — off in tests, as the animation is (a test that looks at one turns it on).
    pub motion_drawn: bool,
    /// `TARAE_MOTION_AT`: every motion drawn at this age, none ending (shots — read once at start).
    pub motion_at: Option<std::time::Duration>,
    /// macOS's Reduce Motion is on (read once at start — `motion::reduce_motion`): `editor.motion = "full"`
    /// behaves as `calm`.
    pub reduce_motion: bool,
    /// Diagnostic card closed with Esc on this line (doc, line) — back once the cursor leaves the line.
    pub diag_card_hidden: Option<(DocId, usize)>,
    cmd_loading: std::collections::HashSet<std::path::PathBuf>,
    /// Debugger watch expressions (evaluated at every stop — outlive debug runs).
    pub watches: Vec<String>,
    pub dap_generation: u64,
    /// Run generations of the test pane, the build task and the terminal (a replaced one's late output is
    /// dropped) — counted here so they're unique across sessions.
    pub task_generation: u64,
    pub terminal_generation: u64,
    pub test_generation: u64,
    /// If the current debug run is a test, that test (after it ends, F5 = rerun it).
    pub dap_test: Option<crate::testing::Target>,
    /// What the current debugging started as — to start it again once a missing debugger is installed.
    pub dap_resume: Option<crate::offer::Resume>,
    /// Preparing a Java debug run (asking jdtls for the main class and classpath) — java.rs.
    pub java_debug: Option<crate::java::DebugPrep>,
    /// Screen layout of the last frame — for mapping mouse coordinates to text positions.
    pub screen: Screen,
    /// Screen rows of the last frame (folded doc comments make screen row ≠ doc line — used by the mouse).
    pub view: Vec<crate::codeview::Row>,
    /// Where a drag started (selecting with the mouse).
    mouse_anchor: Option<usize>,
    /// A ctrl+press in the code not dragged yet: its release is `gd` (a ctrl+drag selects, as a plain one).
    mouse_goto: bool,
    /// The mouse was pressed on the terminal's screen — its drag and release go there, wherever they are.
    mouse_in_terminal: bool,
    /// Recently opened files (start screen — open with number keys).
    pub recent: Vec<PathBuf>,
    /// What you ran from the palette, newest first — its order with nothing typed (`palette.rs`).
    pub palette_ran: Vec<String>,
    /// The restore file (last position per file, open files per folder — `restore.rs`).
    pub restore: crate::restore::State,
    /// Theme before the theme preview (Esc restores it).
    theme_before: Option<crate::theme::Theme>,
    /// The notification card: messages that came while it was up (a burst is one card, newest on top) —
    /// it goes when the last of them has had its time.
    pub toasts: Vec<Toast>,
    /// Every message since tarae started (`:messages`): when, kind, text.
    pub messages: Vec<(std::time::Instant, ToastKind, String)>,
    /// The disk watch round that's current — a newer one (an immediate check) takes over the watch.
    pub disk_poll_gen: u64,
    /// Git actions' reloads, said as one line a session once its actions are over (`disk::DiskBatch`).
    pub disk_batch: Vec<crate::disk::DiskBatch>,
    /// The disk round that waits for changes: what ends its wait (a newer round), and the files it watches.
    pub disk_round: Option<(crate::fswatch::Waker, Vec<(DocId, PathBuf)>)>,
    /// Set by a job result that changed nothing on screen (a disk round that found nothing) — when a batch
    /// of events is all such, the main loop doesn't draw it.
    pub unchanged: bool,
    /// When the last frame was drawn (`term::render`) and the input it drew (`input_seq` then) — what came
    /// after it is owed a frame (`frame_due`).
    pub frame_at: Option<std::time::Instant>,
    pub frame_input: u64,
    /// `:redraw` — the next frame draws the whole screen, not only what changed (the main loop takes it).
    pub redraw: bool,
    /// The last frame went out in parts: the whole screen goes again once input rests (`term::RESTING_REDRAW`) —
    /// the main loop sets it after each write; the state says it (`redraw_due`) for a driver waiting for quiet.
    pub redraw_due: bool,
    /// Frames that went to the terminal (a frame like the last writes nothing) — what a driver reading the
    /// terminal counts (each ends its synchronized update); the state's `frames_written`.
    pub frames_written: u64,
    /// What the event under way is, as its sender said it (`event::Tx::send`'s site, or the name a job gives
    /// itself — `drawn_for`) — for the frame it draws, should it change something.
    pub event_from: Option<DrawnFor>,
    /// What the events of the batch under way that changed something were (`term::handle_noting`) — the next
    /// frame's reasons.
    pub drawn_by: Vec<DrawnFor>,
    /// The last `FRAMES_FOR_KEPT` frames written, each its number (`frames_written` then) and what drew it — the
    /// state's `frames_for`: a late frame in an idle check says its cause the first time, never guessed.
    pub frames_for: std::collections::VecDeque<(u64, Vec<DrawnFor>)>,
    /// A git diff is running on a worker thread.
    pub git_diffing: bool,
    /// Whether to read HEAD contents when opening a file (off in tests — keeps event order stable;
    /// turn on when needed).
    pub git_auto: bool,
    /// Where the chat's conversations are kept (`chats.rs`) — None in tests (they set a temp dir).
    pub chats_dir: Option<std::path::PathBuf>,
    /// Where the copies of what went with messages are written (`chat::write_sent`): a folder of this tarae's own in
    /// the temp folder — None in tests (no copy; those that read one set a scratch folder).
    pub sent_dir: Option<std::path::PathBuf>,
    /// Where Claude's turns are kept (`checkpoint.rs`), every session's — None in tests (they set a temp dir).
    pub turns_dir: Option<std::path::PathBuf>,
    /// The sessions kept per repository, and whether they're back (`worktree.rs`).
    pub session_store: crate::worktree::SessionStore,
    /// The project file's held-back settings and the answer about them (`trust.rs`).
    pub project_trust: Option<crate::trust::Project>,
    /// Where answers about projects are kept — None in tests.
    pub trust_path: Option<std::path::PathBuf>,
    /// Where what's always allowed is kept (`allow.rs`) — None in tests (they set a temp file).
    pub allow_path: Option<std::path::PathBuf>,
    /// Kept answers taken back whose write to `allow_path` hasn't landed yet — or failed: then until you quit, or
    /// an `A` for that checkout writes it (`allow::add_for`) — each with the checkout it was taken back in, as
    /// `:sandbox-disallow` names it, and an id: a write ends the ones it carried (`taking_end`). A `Caps` made
    /// meanwhile leaves out its own checkout's (`caps::Basis.taking`).
    pub allow_taking: Vec<(u64, PathBuf, String)>,
    /// Writes to `allow_path` asked whose result hasn't come back (`allow::keep_queued`) — what's kept may be
    /// on disk before its result is applied (tests wait for none).
    pub allow_writing: usize,
    /// Writes to `trust_path` queued whose result hasn't come back (`trust::queue`).
    pub trust_writing: usize,
    /// The take-backs a write that landed ended, its result not applied yet — recorded by the write itself, so
    /// `:restart`'s state (made after the loop has ended) doesn't carry them (`restart::snapshot`).
    pub allow_landed: std::sync::Arc<std::sync::Mutex<Vec<u64>>>,
    /// The take-backs of trust this tarae gave, each its place and given (`trust::taking` is the process's) — what
    /// `:restart` carries of it, those still marked (`trust::taking_now`).
    pub trust_taking: Vec<(crate::trust::Place, u64)>,
    /// Where cargo's configs told about for their tokens are kept (`tokens::tell_cargo`) — None in tests.
    pub tokens_said: Option<std::path::PathBuf>,
    /// Those told about since tarae started (`~` paths).
    pub tokens_told: Vec<String>,
    /// Claude reads the temp folders unasked (`allow::defaults`) — off in tests: their projects, and what
    /// they read "outside" them, are in the temp folder.
    pub read_temp: bool,
    /// Claude reads outside home unasked where the capability model does (`reads::by_default`, D3) — off in
    /// tests, for the same reason.
    pub read_outside: bool,
    /// tarae runs inside zellij: a hinted key it takes is said by its twin (`zellij.rs`) — off in tests, which
    /// set it.
    pub zellij: bool,
    /// (Tests: the home a session's `Caps` are made for — `caps::Basis.home`; None: the process's.)
    #[cfg(test)]
    pub caps_home: Option<PathBuf>,
    /// What a read-only shellcheck may read beyond its line (`readonly::Shellcheck`): home's rc files and
    /// `SHELLCHECK_OPTS`, the process's — none in tests (they never read yours; a test sets its own).
    pub shellcheck: crate::readonly::Shellcheck,
    /// Where commit trees for `run`'s `base` go (`basetree::folder_for`): None — tarae's cache's; tests' a temp
    /// folder, never the user's cache.
    pub base_trees: Option<PathBuf>,
    /// The chat's claude starts once the project's Claude settings are read and, if they need it, answered
    /// (`claudetrust.rs`) — off in tests but their own: a worker's read before every chat test's claude.
    pub claude_settings_gate: bool,
    /// How old the claude it runs is, said once (`claudeupdate.rs`) — not looked at in tests but their own.
    pub claude_update: crate::claudeupdate::Told,
    /// Your own Claude settings file, read for what a held-back key goes back to (`claudetrust::Baseline`) —
    /// None in tests.
    pub claude_user_settings: Option<std::path::PathBuf>,
    /// tarae's environment as claude inherits it — None: the process's own; tests give their own (empty unless a
    /// test says), so none depends on where it runs. `claudetrust` reads only it for a held-back name;
    /// `llm::spawn` lays it over the process's (a stand-in claude still finds `sh`'s programs) and leaves out what
    /// `llm::claude_env` says of both; what the chat says of it (`llm::env_told`) reads only this.
    pub claude_env: Option<crate::llm::Env>,
    /// What claude's environment leaves out has been said (`llm::env_told`): once since tarae started.
    pub claude_env_told: bool,
    /// An API session's key helper, a test's stand-in (`billing::Editor::credential`) — None: the Keychain's.
    pub api_key_helper: Option<String>,
    /// How `billing::Editor::api_check` looks for the key, a test's stand-in (in tests one that finds nothing,
    /// never the Keychain) — None: `security`, attributes only.
    pub api_key_query: Option<Vec<String>>,
    /// What the last `api_check` found — None: not looked yet.
    pub api_ready: Option<Result<(), String>>,
    /// What tarae keeps of billing across starts — `a` on the limit card, the weakness said (`billing::Kept`).
    pub billing_kept: crate::billing::Kept,
    /// The plan's limits as any claude said them — the account's, not a session's (`limits.rs`).
    pub limits: crate::limits::Limits,
    /// The clock a run of Claude's starts by (`reads::run_started`) — `reads::file_clock` unless a test says.
    pub(crate) run_clock: Option<fn() -> std::time::SystemTime>,
    /// What a run's policy reads of tarae's surroundings (`sandbox::Here`, on its worker) — `Here::now` unless a
    /// test says (`tools::guard_then_go`).
    pub(crate) sandbox_here: Option<std::sync::Arc<dyn Fn() -> crate::sandbox::Here + Send + Sync>>,
    /// The user's config file, where a toggle that's kept is written (`space v a`) — None in tests (they
    /// set a temp file).
    pub config_file: Option<std::path::PathBuf>,
    /// The last frame drawn and its size — what the user sees (`chat::attach_screen`).
    pub last_frame: Option<(Vec<u8>, u16, u16)>,
    /// Bytes for the terminal itself, sent after the next frame (a copy through OSC 52 — `clipboard.rs`).
    pub term_out: Vec<u8>,
    /// Sizes you set — the chat's and side panel's widths, the bottom slot's height (`sizes.rs`), global and
    /// kept in `sizes_path` (None in tests: nothing read or written) · the latest write's number.
    pub sizes: crate::sizes::Sizes,
    pub sizes_path: Option<PathBuf>,
    pub sizes_seq: u64,
    /// Each checkout's layout as kept (`layouts.rs`) in `layouts_path` (None in tests) · how many it has asked to
    /// write (the writes' own numbers are the process's, `layouts::WRITES`).
    pub layouts: serde_json::Map<String, serde_json::Value>,
    pub layouts_path: Option<PathBuf>,
    pub layouts_seq: u64,
    /// What's counted of the features used and not saved yet (`features.rs`; its path None in tests).
    pub features: crate::features::Counts,
    /// The card record's folder (`cards.rs`, `$state/cards`) — None in tests: nothing written.
    pub cards_dir: Option<PathBuf>,
    /// The call-order record's folder (`calls.rs`, `$state/calls`) — None in tests: nothing written.
    pub calls_dir: Option<PathBuf>,
    /// The reviewers' line (`reviewer::pump`): its memory read under way · its retry pending while memory is
    /// critical · how memory is read (`share::room`; in tests normal, unless a test says).
    pub reviewer_pumping: bool,
    pub reviewer_retry: bool,
    /// How long a board's other angles wait for its first one's prompt to be cached (`reviewer::STAGGER`).
    pub reviewer_stagger: std::time::Duration,
    pub reviewer_room: crate::share::ReadRoom,
    /// The memory as the line last read it — what `reviewer::place_in_line` counts the room by between reads.
    pub reviewer_pressure: crate::share::Pressure,
    /// Tests: a reviewer's members start no process — a simulation's stand-ins, their events the scenario's
    /// (`sim/board.rs`: a process's reader threads aren't the simulation's).
    #[cfg(test)]
    pub reviewer_stand_in: bool,
    /// Tests: the chat's claude is no process — its stdin's lines go here (a simulation's stand-in: a process's
    /// reader threads aren't the simulation's).
    #[cfg(test)]
    pub claude_lines: Option<std::sync::mpsc::Sender<String>>,
    /// The review record's folder (`reviews.rs`, `$state/reviews`) — None in tests: nothing written.
    pub reviews_dir: Option<PathBuf>,
    /// The border the mouse is dragging (pressed on it, not let go yet).
    pub dragging: Option<crate::sizes::Border>,
    /// A floating frame the mouse holds (its top line: moving it; an edge: sizing it) until it's let go (`float.rs`).
    pub float_dragging: Option<(crate::pane::GroupKind, crate::float::Grip)>,
    /// `C-w m` under way: the floating pane its keys move (`Handler::Move`).
    pub float_moving: Option<crate::pane::GroupKind>,
    /// The session list, a floating pane while it's up (`space W`, `sessionpane.rs`) — the viewer's, across
    /// sessions: switching to one by other means keeps it in sight.
    pub sessions_pane: Option<crate::floatlist::FloatList>,
    /// `:messages`, a floating list while it's up (`messages.rs`) — the viewer's, as the session list.
    pub messages_pane: Option<crate::floatlist::FloatList>,
    /// The last click on a picker's row (picker, row, when) — the same row again within `DOUBLE_CLICK` opens it.
    pub picker_click: Option<(u64, usize, std::time::Instant)>,
    /// The last click on the git tab's or the review rail's row (which list, row, when) — `clicked_again`.
    pub list_click: Option<(&'static str, usize, std::time::Instant)>,
    /// The card that has the keys — focused (P3 §3.8): by `C-q` from wherever they were (#24), a click on it,
    /// `C-w w`, or as it came when your own key asked for it (`Offer.yours`) — by its id and `rev`: what it said
    /// when it got them (one rewritten, or another in front, has them only once focused itself). The chat's draft,
    /// insert mode, a review stay as they were; an answer, `esc` or `C-w` gives them back.
    pub card_focus: Option<(u64, u64)>,
    /// The card an answer handed the keys on to (id, `rev`): focused once a frame has drawn it as it is
    /// (`term::render_frame`) — a key typed on after the answer (`y y`, a repeat) never answers one not yet in sight.
    pub card_next: Option<(u64, u64)>,
    /// The front card as the last frame drew it (id, `rev`) — a weighty card's answers count only for it
    /// (`Stands.drawn`).
    pub offer_drawn: std::cell::Cell<Option<(u64, u64)>>,
    /// The floating pane a window prefix just took the keys from (a card's `card_key`, a toast's `toast_key`): `C-w w`
    /// from it goes on round the panes, not back to it.
    pub float_left: Option<crate::pane::Floating>,
    /// The notification card that has the keys (P3 §3.9, `toast.rs`): focused by a press on it or `C-w w`'s round —
    /// its time held while it has them, its keys on its frame's bottom line; let go, it fades out.
    pub toast_focus: Option<crate::toast::Focus>,
    /// When the focused card was let go — it fades out over `toast::LETGO`, then goes.
    pub toast_letgo: Option<std::time::Instant>,
    /// The terminal said its focus went elsewhere (CSI ?1004 — tmux passes it on only with `focus-events
    /// on`), and no input came since: the waiting animation rests (`tick_wanted`). A terminal that never
    /// says keeps it going.
    pub unfocused: bool,
    /// Claude's `screen` calls the user said yes to — answered with the next frame drawn (`seeing.rs`).
    pub screen_sends: Vec<crate::tools::Call>,
    /// Frames drawn since tarae started · snapshots of the UI state asked over its socket, answered after a
    /// frame (`uistate.rs`) · messages said so far (a toast's id).
    pub frames: u64,
    pub ui_asks: Vec<crate::uistate::Ask>,
    pub said: u64,
    /// Connections watching the UI state · the cards that went, and how (the gone ring) · the cards up or
    /// parked as of the last frame · how a card went, said where it went (`uistate/watch.rs`).
    pub ui_watch: crate::uistate::watch::Watch,
    pub ui_gone: std::collections::VecDeque<crate::uistate::watch::Gone>,
    pub(crate) ui_cards_seen: Vec<(u64, String, crate::event::SessionId)>,
    pub(crate) ui_card_how: HashMap<u64, String>,
    /// An element brought into sight from a link in Claude's answer, behind it the selected row's colour
    /// for a moment: (its id, since when) — `uistate/claude.rs`.
    pub ui_flash: Option<(String, std::time::Instant)>,
    /// Pointing at an element for the chat: its labels on screen (`uistate/point.rs`).
    pub pointing: Option<crate::uistate::point::Pointing>,
    /// The last key a card refused, and why: (its id, its rev, what it says) — on the card (`offer_refused`).
    pub offer_said: Option<(u64, u64, String)>,
    /// The keys of the command waiting for a character (`on_next_char`) — the status line shows them.
    pub(crate) char_for: String,
    /// Whether Claude's commands can be sandboxed here (`sandbox.rs`) — found with the first one.
    pub sandbox: crate::sandbox::Availability,
    /// `:restart` under way (`restart.rs`) — once `Ready`, main runs the new binary after the loop ends.
    pub restart: Option<crate::restart::Restart>,
    /// Projects whose pack-default preflight was shown on its card and run since tarae started (no card again).
    pub preflight_told: std::collections::HashSet<PathBuf>,
    /// Copies of the buffers with unsaved edits, and the ones a gone tarae left (`recovery.rs`). None in
    /// tests (a test gives one its own folder).
    pub recovery: Option<crate::recovery::Recovery>,
    pub idle_save_gen: u64,
    /// When the scheduled animation tick lands (None: none scheduled).
    ticking: Option<std::time::Instant>,
    /// Values `:set` at runtime — they win even when the config file is reloaded (the top layer).
    pub overrides: Vec<(String, toml::Value)>,
    pub pending: Vec<Key>,
    pub(crate) count: Option<usize>,
    /// Starting snapshot of the open edit group. Stays open throughout insert mode.
    undo_group: Option<(DocId, Snapshot)>,
    next_id: DocId,
}

impl Editor {
    /// An editor whose root is the folder the tests run in (`main` gives its own: `with_root`).
    #[cfg(test)]
    pub fn new(config: Config) -> Self {
        Self::with_root(config, started_in())
    }

    /// An editor for the project in `root` — taken as its real path (links resolved), which `run_cwd`, the
    /// sandbox and every `starts_with(root)` compare against; one that isn't there stays as given.
    pub fn with_root(config: Config, root: PathBuf) -> Self {
        let root = std::fs::canonicalize(&root).unwrap_or(root);
        let recovery = match cfg!(test) {
            true => None,
            false => crate::config::state_dir()
                .map(|d| crate::recovery::Recovery::new(d.join("recovery"), root.clone())),
        };
        let events = Queue::default();
        let saver = crate::saver::Saver::new(events.shared_sender());
        let turns_dir = if cfg!(test) { None } else { crate::checkpoint::default_dir() };
        let mut ed = Self {
            docs: Vec::new(),
            current: 0,
            session: crate::session::Session::new(0, root, 1),
            sessions: Vec::new(),
            next_session: 0,
            presence: Default::default(),
            overlap: Default::default(),
            keep_close: Default::default(),
            behind: false,
            next_pane: 1 + crate::pane::IDS,
            mode: Mode::Normal,
            keymaps: config.keymaps,
            config: config.editor,
            theme: config.theme,
            registers: HashMap::new(),
            selected_register: None,
            on_next_char: None,
            char_arg: None,
            recording: None,
            macros: HashMap::new(),
            replaying: Vec::new(),
            last_trigger_len: 0,
            prompt: None,
            search: None,
            search_hl: false,
            search_hits: None,
            search_origin: None,
            status: None,
            should_quit: false,
            viewport: (24, 80),
            last_motion: None,
            sticky: None,
            sticky_used: false,
            events,
            saver,
            picker: None,
            last_picker: None,
            picker_ids: 0,
            live_search: Default::default(),
            lsp: LspState::default(),
            popup: None,
            popup_scroll: 0,
            completion: None,
            signature: None,
            llm_warm: Warm::default(),
            llm_generation: 0,
            llm_pid: None,
            usage_view: None,
            goto_after_load: None,
            input_seq: 0,
            person_seq: 0,
            shuttles: Default::default(),
            dap: None,
            breakpoints: Default::default(),
            plugins: None,
            procs: Default::default(),
            cmd_lists: Default::default(),
            grammar_hinted: Default::default(),
            offers: Vec::new(),
            offer_whole: Default::default(),
            offer_early: Default::default(),
            offer_view: Default::default(),
            offer_views: Vec::new(),
            auto_held: Vec::new(),
            offer_read: Default::default(),
            offer_seq: 0,
            hook_waits: Vec::new(),
            offer_popups: !cfg!(test),
            bleed_ink: !cfg!(test),
            motions: Vec::new(),
            press_on_diff: None,
            motion_drawn: !cfg!(test),
            motion_at: if cfg!(test) { None } else { crate::motion::pinned_age() },
            reduce_motion: false,
            diag_card_hidden: None,
            cmd_loading: Default::default(),
            watches: Vec::new(),
            dap_generation: 0,
            java_debug: None,
            task_generation: 0,
            terminal_generation: 0,
            test_generation: 0,
            dap_test: None,
            dap_resume: None,
            ticking: None,
            toasts: Vec::new(),
            messages: Vec::new(),
            disk_poll_gen: 0,
            disk_batch: Vec::new(),
            disk_round: None,
            unchanged: false,
            frame_at: None,
            frame_input: 0,
            redraw: false,
            redraw_due: false,
            frames_written: 0,
            event_from: None,
            drawn_by: Vec::new(),
            frames_for: Default::default(),
            git_diffing: false,
            git_auto: !cfg!(test),
            chats_dir: if cfg!(test) { None } else { crate::chats::default_dir() },
            sent_dir: if cfg!(test) { None } else { Some(crate::chat::sent_dir()) },
            turns_dir,
            session_store: crate::worktree::SessionStore {
                dir: if cfg!(test) { None } else { crate::worktree::default_dir() },
                builds: if cfg!(test) { None } else { Some(crate::builds::Builds::real()) },
                ..Default::default()
            },
            project_trust: None,
            trust_path: if cfg!(test) { None } else { crate::trust::Store::path() },
            allow_path: if cfg!(test) { None } else { crate::allow::Store::path() },
            allow_taking: Vec::new(),
            allow_writing: 0,
            allow_landed: Default::default(),
            trust_writing: 0,
            trust_taking: Vec::new(),
            tokens_said: if cfg!(test) { None } else { crate::tokens::said_path() },
            tokens_told: Vec::new(),
            read_temp: !cfg!(test),
            read_outside: !cfg!(test),
            zellij: crate::zellij::inside(),
            #[cfg(test)]
            caps_home: None,
            shellcheck: if cfg!(test) {
                Default::default()
            } else {
                crate::readonly::Shellcheck::of_process()
            },
            base_trees: cfg!(test).then(|| std::env::temp_dir().join("tarae-test-base")),
            claude_settings_gate: !cfg!(test),
            claude_update: if cfg!(test) { Default::default() } else { crate::claudeupdate::Told::new() },
            claude_user_settings: match cfg!(test) {
                true => None,
                false => crate::claudetrust::user_settings(),
            },
            claude_env: if cfg!(test) { Some(Default::default()) } else { None },
            claude_env_told: false,
            api_key_helper: None,
            api_key_query: cfg!(test).then(|| ["sh", "-c", "exit 44"].map(String::from).to_vec()),
            api_ready: None,
            // (a few bytes, read before the first frame — a new session's billing hangs on it)
            billing_kept: if cfg!(test) {
                Default::default()
            } else {
                crate::billing::Kept::read(crate::billing::Kept::path())
            },
            limits: crate::limits::Limits::new(),
            run_clock: None,
            sandbox_here: None,
            config_file: if cfg!(test) { None } else { crate::config::config_path() },
            last_frame: None,
            term_out: Vec::new(),
            unfocused: false,
            // (a few bytes, read before the first frame — it's drawn at the sizes you left)
            sizes: if cfg!(test) {
                Default::default()
            } else {
                crate::sizes::Sizes::read(crate::sizes::Sizes::path().as_deref())
            },
            sizes_path: if cfg!(test) { None } else { crate::sizes::Sizes::path() },
            sizes_seq: 0,
            // (each checkout's panes as you left them — a small file, read before the first frame)
            layouts: if cfg!(test) {
                Default::default()
            } else {
                crate::layouts::read(crate::layouts::path().as_deref())
            },
            layouts_path: if cfg!(test) { None } else { crate::layouts::path() },
            layouts_seq: 0,
            cards_dir: if cfg!(test) {
                None
            } else {
                crate::config::state_dir().map(|d| d.join(crate::cards::DIR))
            },
            calls_dir: if cfg!(test) {
                None
            } else {
                crate::config::state_dir().map(|d| d.join(crate::calls::DIR))
            },
            reviewer_pumping: false,
            reviewer_retry: false,
            reviewer_stagger: crate::reviewer::STAGGER,
            reviewer_room: if cfg!(test) {
                || crate::share::Room { pressure: crate::share::Pressure::Normal, headroom: None }
            } else {
                crate::share::room
            },
            reviewer_pressure: crate::share::Pressure::Unknown,
            #[cfg(test)]
            reviewer_stand_in: false,
            #[cfg(test)]
            claude_lines: None,
            reviews_dir: if cfg!(test) {
                None
            } else {
                crate::config::state_dir().map(|d| d.join(crate::reviews::DIR))
            },
            features: crate::features::Counts::new(if cfg!(test) {
                None
            } else {
                crate::config::state_dir().map(|d| d.join(crate::features::FILE))
            }),
            dragging: None,
            float_dragging: None,
            float_moving: None,
            sessions_pane: None,
            messages_pane: None,
            picker_click: None,
            list_click: None,
            card_focus: None,
            card_next: None,
            offer_drawn: Default::default(),
            float_left: None,
            toast_focus: None,
            toast_letgo: None,
            screen_sends: Vec::new(),
            frames: 0,
            ui_asks: Vec::new(),
            said: 0,
            ui_watch: Default::default(),
            ui_gone: Default::default(),
            ui_cards_seen: Vec::new(),
            ui_card_how: HashMap::new(),
            ui_flash: None,
            pointing: None,
            offer_said: None,
            char_for: String::new(),
            sandbox: if cfg!(test) { crate::sandbox::Availability::Bypass } else { Default::default() },
            restart: None,
            preflight_told: Default::default(),
            recovery,
            idle_save_gen: 0,
            recent: Vec::new(),
            palette_ran: Vec::new(),
            restore: crate::restore::State::default(),
            theme_before: None,
            screen: Screen::default(),
            mouse_anchor: None,
            mouse_goto: false,
            mouse_in_terminal: false,
            view: Vec::new(),
            overrides: Vec::new(),
            pending: Vec::new(),
            count: None,
            undo_group: None,
            insert_rec: None,
            last_insert: None,
            repeating_insert: false,
            expand_history: Vec::new(),
            jump_labels: None,
            blame: Default::default(),
            git_told: Default::default(),
            git_bases_read: 0,
            rebase_lines_read: 0,
            bring_queue: Default::default(),
            replace_targets: None,
            replace_plan: Vec::new(),
            next_id: 0,
        };
        ed.new_scratch();
        ed.session.views.push(View::new(1, ed.docs[0].id));
        ed.session.billing = ed.new_session_billing();
        ed.api_check_kept();
        ed.project_trust_set(config.project);
        ed
    }

    pub fn doc(&self) -> &Document {
        &self.docs[self.current]
    }

    pub fn doc_mut(&mut self) -> &mut Document {
        &mut self.docs[self.current]
    }

    pub(crate) fn alloc_id(&mut self) -> DocId {
        self.next_id += 1;
        self.next_id
    }

    /// The slot of the empty scratch created at startup, if it can be handed over: the only buffer, or the
    /// current one when it's the session's own first (`session_open`) and nothing else was added in it since.
    fn replaceable_scratch(&self) -> Option<usize> {
        // (a web page is no blank: its text is a page's, read)
        let blank = |i: usize| {
            self.docs[i].path.is_none() && !self.docs[i].is_modified() && self.docs[i].page.is_none()
        };
        if self.docs.len() == 1 && blank(0) {
            return Some(0);
        }
        (self.session.scratch == Some(self.docs[self.current].id) && blank(self.current))
            .then_some(self.current)
    }

    /// Adds a document and makes it current — taking over the startup scratch's slot if that's all there
    /// is (panes showing the scratch then show the new document).
    fn add_doc(&mut self, doc: Document) {
        let slot = self.replaceable_scratch();
        self.session.scratch = None;
        let id = doc.id;
        if let Some(i) = slot {
            let old = self.docs[i].id;
            for v in self.session.views.iter_mut().filter(|v| v.doc == old) {
                v.show(id);
            }
            self.session.docs.retain(|&d| d != old);
            self.docs[i] = doc;
            self.current = i;
        } else {
            self.docs.push(doc);
            self.current = self.docs.len() - 1;
        }
        self.session_keeps(id);
    }

    /// A buffer with no file behind it (a deleted file in a review) — made current. Returns its id.
    pub fn open_virtual(&mut self, title: &str, text: &str) -> DocId {
        let id = self.alloc_id();
        let mut doc = Document::from_str(id, text);
        doc.title = Some(title.to_string());
        doc.throwaway = true;
        self.add_doc(doc);
        id
    }

    /// `:tutor` — hands-on tutorial (a disposable practice buffer, drawn as text: `tutor.rs`).
    pub fn open_tutor(&mut self) {
        let id = self.alloc_id();
        let (text, paint) = crate::tutor::text(self.twins_said(), self.zellij_letters());
        let mut doc = Document::from_str(id, &text);
        doc.title = Some("tutor".into());
        doc.throwaway = true;
        doc.paint = paint;
        self.add_doc(doc);
    }

    pub fn new_scratch(&mut self) {
        let id = self.alloc_id();
        self.docs.push(Document::from_str(id, ""));
        self.current = self.docs.len() - 1;
        self.session_keeps(id);
    }

    /// The project's folder (`root`).
    pub fn root(&self) -> &Path {
        &self.session.root
    }

    /// The checkout tarae started in (session 0's): the editor's config is read from there, whichever
    /// session is in front — after `:restart` that can be another checkout, its `.tarae.toml` another
    /// version (one without `[preflight]`: the packs' defaults ran in place of the project's).
    pub fn started_root(&self) -> &Path {
        let first = std::iter::once(&self.session).chain(&self.sessions).find(|l| l.id == 0);
        first.map_or(&self.session.root, |l| &l.root)
    }

    /// Where its config loads read your config file and the trust answers — none in tests.
    pub fn config_sources(&self) -> crate::config::Sources {
        crate::config::Sources { user: self.config_file.clone(), trust: self.trust_path.clone() }
    }

    /// What every tarae watches from its start (`term::event_loop`): its config files — `started_root`'s —
    /// and that checkout's HEAD, as session 0's (the other sessions' are watched as they open).
    pub fn watches_start(&mut self) {
        let root = self.started_root().to_path_buf();
        crate::config::watch(self.events.shared_sender(), root.clone(), self.config_sources());
        self.events.set_session(0);
        crate::git::watch(self.events.sender(), root);
        self.events.set_session(self.session.id);
    }

    /// `path` as the editor means it: a relative one is from the root, not from the process's folder.
    pub fn path_of(&self, path: &Path) -> PathBuf {
        if path.is_absolute() { path.to_path_buf() } else { self.session.root.join(path) }
    }

    /// `path` relative to the root, for showing (unchanged if outside).
    pub fn shown<'a>(&self, path: &'a Path) -> &'a Path {
        path.strip_prefix(&self.session.root).unwrap_or(path)
    }

    /// Opens a file (a relative path is from the root). If already open, switches to that buffer.
    pub fn open(&mut self, path: &Path) -> Result<()> {
        self.open_as(path, Opener::You)
    }

    /// `open` for Claude's turn (follow, replay): a file not open yet is `claudes_only` — no grammar or
    /// server offered for it until the user takes it up (#208).
    pub fn open_for_claude(&mut self, path: &Path) -> Result<()> {
        self.open_as(path, Opener::Claude)
    }

    /// `open` for the git tab's or the review rail's look at a file — one not open yet `review_only`: no grammar or
    /// server offered for it until the user takes it up (frictions25 R7) — in the session's one preview slot:
    /// one open already is only shown (a buffer of its own stays one), else it takes the place of the preview shown
    /// before, so passing over files doesn't grow the buffer list (`Document::preview`). A preview edited, under
    /// Claude's review, or with a review's viewed mark waiting for its base (it lands on that buffer) isn't closed:
    /// it's a buffer of its own from then on.
    ///
    /// The one it replaces keeps its place in the jump lists (`park_jumps`: `C-o` back to it opens it again, as the
    /// preview); one another pane shows isn't closed — it's that pane's, a buffer of its own from then on, said.
    pub fn open_preview(&mut self, path: &Path) -> Result<()> {
        self.open_preview_as(path, false)
    }

    /// `open_preview` for `C-o`/`C-i` travelling to a preview the next one took the place of: no jump recorded (it
    /// would cut the list's forward branch — `goto_jump` sets the pane's document itself).
    pub(crate) fn open_preview_travel(&mut self, path: &Path) -> Result<()> {
        self.open_preview_as(path, true)
    }

    fn open_preview_as(&mut self, path: &Path, travel: bool) -> Result<()> {
        let before = self.buffers().into_iter().map(|i| &self.docs[i]).find(|d| d.preview).map(|d| d.id);
        let was: Vec<DocId> = self.docs.iter().map(|d| d.id).collect();
        self.open_as(path, Opener::Review)?;
        let id = self.doc().id;
        // (the spot left behind a jump now, while its buffer is still there to mark — as any switch of file)
        if !travel {
            self.track_doc_switch();
        }
        // (one open already: only shown)
        if was.contains(&id) {
            return Ok(());
        }
        self.doc_mut().preview = true;
        let Some(old) = before else { return Ok(()) };
        let reviewed = self.session.review.as_ref().is_some_and(|r| r.doc_id == old);
        let waited = |d: &Document| {
            let r = self.session.code_review.as_ref();
            d.path.as_deref().is_some_and(|p| r.is_some_and(|r| r.waits(p)))
        };
        // (another pane shows it: that pane's — the focused one's document is the new preview by now)
        let focus = self.session.focus;
        let shown = self.session.views.iter().find(|v| v.doc == old && v.id != focus).map(|v| v.id);
        match self.docs.iter().find(|d| d.id == old) {
            Some(d) if shown.is_some() => {
                let name = d.display_name_short();
                self.preview_keep(old);
                let pane = shown.unwrap_or_default();
                self.set_status(format!("{name} stays open, a buffer of its own — pane#{pane} shows it"));
            }
            Some(d) if d.is_modified() || reviewed || waited(d) => self.preview_keep(old),
            Some(_) => {
                self.park_jumps(old);
                self.close_doc_as(old, true);
            }
            None => {}
        }
        Ok(())
    }

    /// Preview `id` becomes a buffer of its own (upright, kept, restored).
    pub fn preview_keep(&mut self, id: DocId) {
        if let Some(d) = self.docs.iter_mut().find(|d| d.id == id) {
            d.preview = false;
        }
    }

    /// Every event: a preview edited is a buffer of its own — moving, searching, `esc code` into it keep it one.
    fn previews_sync(&mut self) {
        for d in self.docs.iter_mut().filter(|d| d.preview && d.is_modified()) {
            d.preview = false;
        }
    }

    /// A click on row `row` of list `what` (the git tab's, the review rail's): whether it's the same row again within
    /// `DOUBLE_CLICK` — a double-click.
    pub(crate) fn clicked_again(&mut self, what: &'static str, row: usize) -> bool {
        let again = self
            .list_click
            .is_some_and(|(w, r, at)| (w, r) == (what, row) && crate::clock::since(at) < DOUBLE_CLICK);
        self.list_click = (!again).then(|| (what, row, crate::clock::now()));
        again
    }

    fn open_as(&mut self, path: &Path, by: Opener) -> Result<()> {
        let path = self.path_of(path);
        let path = std::fs::canonicalize(&path).or_else(|_| std::path::absolute(&path))?;
        if let Some(i) = self.docs.iter().position(|d| d.path.as_deref() == Some(path.as_path())) {
            self.current = i;
            self.session_keeps(self.docs[i].id);
            // (opened by you — `enter` on its row, `space f`, a jump: a preview is yours from now on)
            if by == Opener::You {
                self.docs[i].preview = false;
            }
            return Ok(());
        }
        let id = self.alloc_id();
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let mut doc = if size < ASYNC_LOAD_BYTES {
            Document::open(id, &path)?
        } else {
            // Big files load in the background — the first frame is instant regardless of file size.
            let p = path.clone();
            self.events.jobs().spawn(move || {
                let started = std::time::Instant::now();
                // Stamp before reading — a change during the read is caught by the next watch round
                let disk = crate::disk::stamp(&p);
                let result = crate::document::read_file(&p);
                let took = started.elapsed();
                move |ed: &mut Editor| {
                    if let Some(d) = ed.docs.iter_mut().find(|d| d.id == id) {
                        d.disk = disk;
                    }
                    ed.finish_loading(id, result, took)
                }
            });
            Document::placeholder(id, &path)
        };
        doc.claudes_only = by == Opener::Claude;
        doc.review_only = by == Opener::Review;
        self.add_doc(doc);
        self.restore.opened = true;
        self.restore_position(self.current);
        self.undo_restore(id);
        self.attach_syntax(id);
        self.attach_lsp(id);
        self.git_load_base(id);
        self.qlist_mark_open();
        // (a big file: once its text is in — `finish_loading`)
        if !self.docs.iter().any(|d| d.id == id && d.loading) {
            self.plugin_event(crate::plugin::PluginEvent::Opened, id);
        }
        self.recent = crate::recent::bump(&self.recent, &path);
        if !cfg!(test) {
            let list = self.recent.clone();
            self.events.jobs().background(move || crate::recent::save(&list));
        }
        Ok(())
    }

    fn finish_loading(&mut self, id: DocId, result: Result<ropey::Rope>, took: std::time::Duration) {
        let Some(i) = self.docs.iter().position(|d| d.id == id) else { return }; // closed while loading
        match result {
            Ok(text) => {
                let mb = text.len_bytes() as f64 / 1e6;
                self.docs[i].finish_loading(text);
                self.docs[i].git_version = u64::MAX; // its diff (git, `:diff`) is worked out on the text now there
                self.restore_position(i);
                // A jump that was waiting for the text (go-to while it loaded) wins over the saved spot
                if let Some((_, line, col)) = self.goto_after_load.take_if(|g| g.0 == id) {
                    place_cursor(&mut self.docs[i], line, col);
                    if i == self.current {
                        self.align_view(crate::viewalign::Place::Center);
                    }
                }
                self.attach_lsp(id);
                self.undo_restore(id);
                self.qlist_mark_open();
                self.set_status(format!(
                    "{} loaded — {mb:.1} MB in {}",
                    self.docs[i].display_name(&self.session.root),
                    crate::term::run_duration(took)
                ));
                self.plugin_event(crate::plugin::PluginEvent::Opened, id);
                self.recovery_loaded(id);
            }
            Err(e) => {
                self.close_doc(id);
                self.set_error(format!("{e:#}"));
            }
        }
    }

    /// Closing document `id` while a change to it is under review: asked on a card first (`y` declines the
    /// change and closes it, `n` keeps both) — #54: closed, the change was declined without a word. True:
    /// asked. Not `force` (`:bc!`), your own unsaved edits keep it open after `y` all the same.
    fn close_asks_review(&mut self, id: DocId, force: bool) -> bool {
        if self.session.review.as_ref().is_none_or(|r| r.doc_id != id) {
            return false;
        }
        let Some(d) = self.docs.iter().find(|d| d.id == id) else { return false };
        let name = d.display_name(&self.session.root);
        let closes = match !force && d.is_modified() && !d.throwaway {
            true => "— your unsaved edits keep the buffer open (:bc! discards them)",
            false => "and closes the buffer",
        };
        let card = crate::offer::Offer::new(
            format!("Close {name}?"),
            &format!(
                "A change to {name} waits for your answer.\n\ny declines it (Claude hears so) {closes} · n keeps both"
            ),
            "",
            String::new(),
            crate::offer::What::CloseReviewed { doc: id, force },
        );
        self.push_offer(card);
        // (asked for by you, over the review: its y and n are the card's, as after `C-q`)
        self.card_focus_front();
        true
    }

    /// `y` on that card: the change declined, the buffer closed — unless it has unsaved changes of yours and
    /// the close wasn't forced: then kept open, as `:bc` keeps it.
    pub(crate) fn close_reviewed(&mut self, id: DocId, force: bool) {
        if self.session.review.as_ref().is_some_and(|r| r.doc_id == id) {
            crate::llm::close_review(self);
        }
        // (a `:w` a moment ago may still be being written — it counts)
        if self.docs.iter().any(|d| d.id == id && d.saving > 0) {
            self.flush_saves();
        }
        let Some(d) = self.docs.iter().find(|d| d.id == id) else { return };
        if !force && d.is_modified() && !d.throwaway {
            let name = d.display_name(&self.session.root);
            return self.set_error(format!(
                "declined — unsaved changes in {name} keep it open (use :bc! to discard)"
            ));
        }
        self.close_doc(id);
    }

    pub fn close_current(&mut self, force: bool) -> Result<(), String> {
        if self.close_asks_review(self.doc().id, force) {
            return Ok(());
        }
        // (a `:w` a moment ago may still be being written — it counts)
        if self.doc().saving > 0 {
            self.flush_saves();
        }
        if !force && self.doc().is_modified() && !self.doc().throwaway {
            return Err(format!(
                "unsaved changes in {} (use :bc! to discard)",
                self.doc().display_name(&self.session.root)
            ));
        }
        self.close_doc(self.docs[self.current].id);
        Ok(())
    }

    /// Closes a document (without asking) — notifies servers; panes showing it switch to another document
    /// (the front session's buffer after it, else the one before; none left: a blank one).
    pub fn close_doc(&mut self, gone: DocId) {
        self.close_doc_as(gone, false)
    }

    /// `close_doc`; `parked`: a preview the next one replaced — its jumps stay where they are (`park_jumps`).
    fn close_doc_as(&mut self, gone: DocId, parked: bool) {
        let Some(i) = self.docs.iter().position(|d| d.id == gone) else { return };
        self.plugin_closing(gone);
        self.lsp_did_close(gone);
        self.qlist_doc_closing(gone);
        let next = match i == self.current {
            true => {
                let mine = self.buffers();
                let at = mine.iter().position(|&k| k == i);
                at.and_then(|a| mine.get(a + 1).or_else(|| mine.get(a.checked_sub(1)?)))
                    .map(|&k| self.docs[k].id)
            }
            false => Some(self.docs[self.current].id),
        };
        self.docs.remove(i);
        for l in std::iter::once(&mut self.session).chain(&mut self.sessions) {
            l.docs.retain(|&d| d != gone);
        }
        match next.and_then(|id| self.docs.iter().position(|d| d.id == id)) {
            Some(k) => self.current = k,
            None => self.new_scratch(),
        }
        let now = self.docs[self.current].id;
        for v in self.session.views.iter_mut() {
            if v.doc == gone {
                v.show(now);
            }
            if !parked {
                v.jumps.remove_doc(gone);
            }
            v.last_doc = v.last_doc.filter(|&d| d != gone);
        }
        // A review of a change to it, in any session, ends with it — Claude is told its buffer went (#149: one
        // left on a closed buffer drew nothing and kept the keys for 80 minutes)
        let reviewing: Vec<_> = std::iter::once(&self.session)
            .chain(&self.sessions)
            .filter(|l| l.review.as_ref().is_some_and(|r| r.doc_id == gone))
            .map(|l| l.id)
            .collect();
        for id in reviewing {
            self.with_session(id, |ed| {
                crate::llm::close_review(ed);
                ed.set_warning("the buffer of Claude's edit under review was closed — Claude was told");
            });
        }
        // A rebase's commit message being written (`m`), closed some way but `:w`/`:q`: its card back
        self.rebase_message_closed(gone);
    }

    // ── Splits ────────────────────────────────────────────────────────────

    fn view_mut(&mut self, id: crate::split::PaneId) -> Option<&mut View> {
        self.session.views.iter_mut().find(|v| v.id == id)
    }

    /// Every event: focused pane = the current document and its scroll (even if the document changed by
    /// another path — opening a file, switching buffers).
    pub fn sync_focus_view(&mut self) {
        self.track_doc_switch();
        let (doc, top, left, top_rows) = {
            let d = self.doc();
            (d.id, d.top, d.left, d.top_rows)
        };
        self.session_keeps(doc);
        let focus = self.session.focus;
        if let Some(v) = self.view_mut(focus) {
            (v.doc, v.top, v.left, v.top_rows) = (doc, top, left, top_rows);
        }
    }

    /// Moves focus to another pane: saves the current pane's scroll, then restores the new pane's
    /// document and scroll.
    pub fn focus_view(&mut self, id: crate::split::PaneId) {
        if id == self.session.focus {
            return;
        }
        self.sync_focus_view();
        let Some(v) = self.session.views.iter().find(|v| v.id == id).cloned() else { return };
        self.session.focus = id;
        if let Some(i) = self.docs.iter().position(|d| d.id == v.doc) {
            self.current = i;
            let d = &mut self.docs[i];
            (d.top, d.left, d.top_rows) = (v.top, v.left, v.top_rows);
        }
        self.completion = None;
        self.popup = None;
    }

    /// Splits the current pane — the new pane shows the same document at the same spot; focus moves to it. From
    /// another surface (§2.6: the chat, a run, the side panel) it's the code pane last focused that splits, the keys
    /// to the new one; from the terminal it's said — a second terminal comes with the terminal work (P1).
    pub fn split_view(&mut self, dir: crate::split::Dir) {
        let terminal = self.session.is_at(At::Program)
            || (self.session.is_at(At::Slot) && self.slot() == Some(crate::sizes::Slot::Terminal));
        if terminal {
            return self.set_status("one terminal for now — a second comes with the terminal work");
        }
        self.sync_focus_view();
        let id = self.next_pane;
        self.next_pane += 1;
        let Some(cur) = self.session.views.iter().find(|v| v.id == self.session.focus).cloned() else {
            return;
        };
        let views: Vec<_> = self.session.views.iter().map(|v| v.id).collect();
        self.session.split.split_among(self.session.focus, id, dir, &|p| views.contains(&p));
        self.session.views.push(View { id, ..cur });
        self.session.focus = id;
        if !self.session.is_at(At::Code) {
            self.focus_pane(id);
        }
    }

    /// Closes the current pane — false if it's the last one (then `:q` quits the editor).
    pub fn close_view(&mut self) -> bool {
        if self.session.views.len() <= 1 {
            return false;
        }
        let gone = self.session.focus;
        let order = self.code_leaves();
        let i = order.iter().position(|v| *v == gone).unwrap_or(0);
        self.session.split.remove(gone);
        self.session.views.retain(|v| v.id != gone);
        let next = self.code_leaves()[i.saturating_sub(1).min(self.session.views.len() - 1)];
        self.focus_view(next); // syncing the gone pane finds nothing
        true
    }

    /// The code's panes in the tree's order.
    pub fn code_leaves(&self) -> Vec<crate::split::PaneId> {
        let s = &self.session;
        s.split.leaves().into_iter().filter(|id| s.views.iter().any(|v| v.id == *id)).collect()
    }

    /// Keep only this pane among the code's — the other surfaces keep their places (§2.6 Q4).
    pub fn only_view(&mut self) {
        self.sync_focus_view();
        let focus = self.session.focus;
        for id in self.code_leaves().into_iter().filter(|&id| id != focus) {
            self.session.split.remove(id);
        }
        self.session.views.retain(|v| v.id == focus);
    }

    /// `C-w w`: the next pane, every kind (§2.6) — in the tree's order as the last frame laid them out with room
    /// (`Screen.leaves`: the side panel, the code's panes, the slot or the tests' two, the chat), landing where each
    /// takes the keys. Before a frame, the code's panes.
    pub fn cycle_view(&mut self, delta: isize) {
        let mut order: Vec<_> = self.screen.leaves.iter().map(|l| l.0).collect();
        if order.is_empty() {
            order = self.code_leaves();
        }
        // The floating panes in sight are the round's last stops (P3 §3.8–3.9): a card waiting, then the toast —
        // `C-w w` from the last pane focuses the card, from the card the toast (backwards the other way round); from
        // the one a window prefix just took the keys from the round goes on (never back to it)
        use crate::pane::Floating;
        let left = self.float_left.take();
        let card = (self.screen.offer_card.is_some() || self.screen.offer_held)
            && self.card_waits()
            && !matches!(left, Some(Floating::Card(l)) if self.offers.first().is_some_and(|o| o.id == l));
        let toast = self.toast_up() && !matches!(left, Some(Floating::Toast(_)));
        let from_card = matches!(left, Some(Floating::Card(_)));
        let from_toast = matches!(left, Some(Floating::Toast(_)));
        let edge = |end: Option<&crate::split::PaneId>| self.at_pane().is_some_and(|h| end == Some(&h));
        let (last, first) = (edge(order.last()) && left.is_none(), edge(order.first()) && left.is_none());
        match delta > 0 {
            true if last && card => return self.card_focus_front(),
            true if (last || from_card) && toast => return self.toast_focus_take(),
            false if first && toast => return self.toast_focus_take(),
            false if (first || from_toast) && card => return self.card_focus_front(),
            _ => {}
        }
        let mut here = self.at_pane().unwrap_or(self.session.focus);
        // (the slot's keys on a group: its detail's)
        if !order.contains(&here)
            && let Some(d) = self.hosted_detail(here, &self.screen.leaves)
        {
            here = d;
        }
        let i = order.iter().position(|v| *v == here).unwrap_or(0) as isize;
        let next = order[(i + delta).rem_euclid(order.len() as isize) as usize];
        if next == here {
            return self.set_status("one pane on screen — C-w v splits this one");
        }
        self.focus_pane(next);
    }

    /// `C-w H J K L` (Helix's `swap_view_*`): this code pane and the code pane that way trade places, the keys
    /// staying with this one. From a group's member it docks the group on that side, `C-w J` back to the slot
    /// (`dock_key`, P3 §3.2); the side panel and the chat keep their places — said.
    pub fn swap_view(&mut self, dx: i32, dy: i32) {
        if self.dock_key(dx, dy) {
            return;
        }
        if !self.session.is_at(At::Code) {
            return self.set_status(
                "the side panel and the chat keep their places — C-w H J K L moves the code's panes and docks a group",
            );
        }
        // (the last frame's panes: one closed since — `C-w o`, no frame between — is no pane to trade with)
        let live = |to: &crate::split::PaneId| {
            self.session.views.iter().any(|v| v.id == *to) && self.session.split.holds(*to)
        };
        match self.pane_toward(dx, dy).filter(live) {
            Some(to) => self.session.split.swap(self.session.focus, to),
            None => self.nothing_toward(dx, dy),
        }
    }

    /// `C-w t` (Helix's `transpose_view`): the split of the code's panes around the one last focused turned — side by
    /// side ↔ stacked; the panels around it stay.
    pub fn transpose_view(&mut self) {
        // (from a group's member: its members turned — side by side ↔ over and under, `dock::group_turn`)
        if let Some(kind) = self.group_in_hand() {
            return self.group_turn(kind);
        }
        let views: Vec<_> = self.session.views.iter().map(|v| v.id).collect();
        if !self.session.split.transpose(self.session.focus, &|id| views.contains(&id)) {
            self.set_status("one code pane — nothing to turn (C-w v splits it)");
        }
    }

    /// `C-w h/j/k/l`: to the pane that way, any kind (panes P2b, §2.6) — the nearest the tree laid out there in the
    /// last frame (`Screen.leaves`); toward the code it's the code pane last focused when that one is there (friction
    /// 88: `C-w h` from the chat's input went back to it). It lands where that surface takes the keys: a run panel's or
    /// the terminal's view mode, the chat's input, the side panel on the row for where the code is. Nothing that way
    /// is said.
    pub fn focus_dir(&mut self, dx: i32, dy: i32) {
        let Some(mut from) = self.at_pane() else { return self.nothing_toward(dx, dy) };
        let leaves = &self.screen.leaves;
        // (the slot's keys on a group: its detail's — the slot's leaf is its members on screen)
        if let Some(d) = self.hosted_detail(from, leaves) {
            from = d;
        }
        // (a stacked group's other member, the list left of its detail as side by side: opened, as `tab` opens it)
        if let Some((other, way)) = self.stacked_other(from, leaves)
            && way == dx.signum()
            && dy == 0
        {
            return self.focus_pane(other);
        }
        // (from what has no room on screen — the minimized chat's input — a hop goes back to the code)
        if self.screen.width.is_some() && from != self.session.focus && !leaves.iter().any(|l| l.0 == from) {
            return self.focus_pane(self.session.focus);
        }
        // (floating panes are over the docked, not beside them: a hop between docked ones never lands in one; from one,
        // to its own frame's other member, else from its frame's centre to the nearest docked pane that way — P3 §3.3)
        let floats: Vec<crate::split::Rect> = self.screen.floating.borrow().iter().map(|f| f.1).collect();
        let inside = |f: &crate::split::Rect, r: &crate::split::Rect| {
            f.x <= r.x && r.x + r.w <= f.x + f.w && f.y <= r.y && r.y + r.h <= f.y + f.h
        };
        let docked: Vec<_> =
            leaves.iter().filter(|l| !floats.iter().any(|f| inside(f, &l.1))).copied().collect();
        let own =
            leaves.iter().find(|l| l.0 == from).and_then(|l| floats.iter().rev().find(|f| inside(f, &l.1)));
        let to = match own {
            None => crate::split::neighbor_from(&docked, from, dx, dy, self.session.focus),
            Some(f) => {
                let mates: Vec<_> = leaves.iter().filter(|l| inside(f, &l.1)).copied().collect();
                crate::split::neighbor_from(&mates, from, dx, dy, self.session.focus).or_else(|| {
                    let centre = crate::split::Rect { x: f.x + f.w / 2, y: f.y + f.h / 2, w: 1, h: 1 };
                    let mut v = docked.clone();
                    v.push((from, centre));
                    // (none wholly that way: the docked pane under its centre — it floats over it)
                    let under = docked.iter().find(|l| inside(&l.1, &centre)).map(|l| l.0);
                    crate::split::neighbor_from(&v, from, dx, dy, self.session.focus).or(under)
                })
            }
        };
        match to {
            Some(to) => self.focus_pane(to),
            // (below the code with nothing in the slot: it says what shows there)
            None if dy > 0 && self.session.is_at(At::Code) => self.bottom_take_keys(),
            None => self.nothing_toward(dx, dy),
        }
    }

    /// The row of kind `kind`'s list at screen cell (x, y), as the list was last drawn (its pane in `Screen.leaves`).
    fn list_row(&self, kind: crate::pane::GroupKind, x: usize, y: usize) -> Option<usize> {
        let (y0, rows, first) = self.screen.list(kind)?;
        let id = self.session.group(kind)?.list;
        let list = self.screen.leaves.iter().find(|l| l.0 == id)?.1;
        ((list.x..list.x + list.w).contains(&x) && (y0..y0 + rows).contains(&y)).then(|| first + y - y0)
    }

    /// The keys to pane `id`, landing where its surface takes them.
    pub fn focus_pane(&mut self, id: crate::split::PaneId) {
        use crate::pane::Shows;
        // (a floating list, a pane of the viewer's)
        if let Some(k) = self.float_list_of(id) {
            return self.float_list_focus(k);
        }
        let Some(shows) = self.session.shows(id) else { return };
        if !matches!(shows, Shows::Slot | Shows::List(_) | Shows::Detail(_) | Shows::Group(_)) {
            self.plugin_panel_unfocus();
            self.procs_unfocus();
        }
        match shows {
            Shows::Code => {
                self.focus_view(id);
                self.session.at = At::Code;
            }
            Shows::Chat => self.session.at = At::Chat,
            Shows::Side => self.side_focus(),
            // (the rail as `space e` gives it the keys: on the file in hand)
            Shows::Rail => crate::codereview::rail_focus(self),
            Shows::Slot => self.bottom_take_keys(),
            Shows::List(k) => self.list_focus(k),
            // (a detail in the slot is the slot's view mode on it)
            Shows::Detail(k) => self.detail_focus(k),
            // (a docked group's leaf: the member it had last — its output alone while it lists nothing)
            Shows::Group(k) if self.floating_in_sight(k) => self.float_focus(k),
            Shows::Group(k) => {
                if let Some(s) = k.slot() {
                    self.slot_show(s);
                }
            }
        }
    }

    /// A `C-w h/j/k/l` with nothing that way: said, nothing moved.
    fn nothing_toward(&mut self, dx: i32, dy: i32) {
        let way = match (dx.signum(), dy.signum()) {
            (-1, _) => "to the left",
            (1, _) => "to the right",
            (_, -1) => "above",
            _ => "below",
        };
        self.set_status(format!("nothing {way}"));
    }

    /// The pane that way from the one in hand, as last drawn — none below: `C-w j` goes to the bottom slot (its
    /// header's `C-w j scroll` says so only then).
    pub fn pane_toward(&self, dx: i32, dy: i32) -> Option<crate::split::PaneId> {
        let panes: Vec<_> = self.screen.panes.iter().map(|(v, r, _, _)| (*v, *r)).collect();
        crate::split::neighbor(&panes, self.session.focus, dx, dy)
    }

    /// `focus_side_panel` (the palette's): to the side panel from wherever the keys are — or back to the code from
    /// it. A closed panel opens at the tab it last showed.
    pub fn side_toggle_focus(&mut self) {
        match self.session.sidebar.as_ref() {
            Some(_) if self.session.is_at(At::Side) => self.session.at = At::Code,
            Some(_) => self.side_focus(),
            None => self.sidebar_show(self.session.side_last),
        }
    }

    /// The keys to the open side panel, its row the one for where the code is.
    fn side_focus(&mut self) {
        self.session.leave(At::Chat);
        if self.session.sidebar.is_none() {
            return;
        }
        self.session.at = At::Side;
        self.sidebar_sync();
        let here = self.session.sidebar.as_ref().and_then(|s| s.here(self));
        if let (Some(s), Some(h)) = (self.session.sidebar.as_mut(), here) {
            s.selected = h;
        }
    }

    /// `:bn`/`:bp` — through the front session's buffers.
    pub fn cycle_buffer(&mut self, delta: isize) {
        let mine = self.buffers();
        let at = mine.iter().position(|&i| i == self.current).unwrap_or(0) as isize;
        self.current = mine[(at + delta).rem_euclid(mine.len() as isize) as usize];
    }

    /// Opens `path` with the cursor at (line, byte column) — a jump point is left behind.
    /// A big file still loading gets the spot once its text is there (`goto_after_load`).
    pub fn open_at(&mut self, path: &Path, line: usize, col: usize) -> Result<()> {
        self.push_jump();
        self.open(path)?;
        let i = self.current;
        if self.docs[i].loading {
            self.goto_after_load = Some((self.docs[i].id, line, col));
        } else {
            place_cursor(&mut self.docs[i], line, col);
        }
        Ok(())
    }

    pub fn goto_line(&mut self, line: usize) {
        self.push_jump();
        let doc = self.doc_mut();
        let line = line.min(mv::last_line(&doc.text));
        let pos = mv::line_start(&doc.text, line);
        doc.set_selection(Selection::point(pos));
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        self.report(ToastKind::Info, msg.into());
    }

    pub fn set_error(&mut self, msg: impl Into<String>) {
        self.report(ToastKind::Error, msg.into());
    }

    /// Success (save, apply) — green bar, short.
    pub fn set_success(&mut self, msg: impl Into<String>) {
        self.report(ToastKind::Success, msg.into());
    }

    /// Caution (changed on disk, conflict) — yellow bar, long.
    pub fn set_warning(&mut self, msg: impl Into<String>) {
        self.report(ToastKind::Warning, msg.into());
    }

    fn report(&mut self, kind: ToastKind, msg: String) {
        let msg = self.said_by_session(msg);
        self.report_as_is(kind, msg);
    }

    /// News of session `id` (its name in `msg` already), said from the front — a toast that's that session's
    /// (`Toast.of`), not the front's.
    pub(crate) fn said_for(&mut self, id: crate::event::SessionId, kind: ToastKind, msg: String) {
        self.report_as_is(kind, msg.clone());
        if let Some(t) = self.toasts.iter_mut().rev().find(|t| t.text == msg && t.kind == kind) {
            t.of = Some(id);
        }
    }

    fn report_as_is(&mut self, kind: ToastKind, msg: String) {
        let of = self.behind.then_some(self.session.id);
        self.recorded(kind, msg.clone());
        self.toast(kind, msg.clone(), of);
        let severity = if kind == ToastKind::Error { Severity::Error } else { Severity::Info };
        self.status = Some((msg, severity));
    }

    /// Into `:messages` and the log, no toast — what's of a session behind (its chip and list say it waits;
    /// the words are here when you look).
    pub fn recorded(&mut self, kind: ToastKind, msg: String) {
        let level = match kind {
            ToastKind::Error => "error",
            ToastKind::Warning => "warning",
            _ => "info",
        };
        crate::log::line("tarae", level, &msg);
        self.said += 1;
        self.messages.push((crate::clock::now(), kind, msg));
        if self.messages.len() > MESSAGES_KEPT {
            self.messages.remove(0);
        }
    }

    /// A step under way (`fetching…`): its toast stays up until `step_done(key)` says it's over (at most
    /// `STEP_MAX`) — then it goes, and what came of it shows on its own. The same key again replaces it.
    pub fn set_step(&mut self, key: &'static str, msg: impl Into<String>) {
        let msg = self.said_by_session(msg.into());
        self.step_done(key);
        self.report_as_is(ToastKind::Info, msg.clone());
        if let Some(t) = self.toasts.iter_mut().rev().find(|t| t.text == msg && t.kind == ToastKind::Info) {
            t.step = Some(key);
            t.life = STEP_MAX;
        }
    }

    /// Step `key` is over (done or failed): its toast goes.
    pub fn step_done(&mut self, key: &'static str) {
        self.toasts.retain(|t| t.step != Some(key));
    }

    /// A step that is one job: said (`set_step`) while `work` runs on `jobs`' worker, and over (`step_done`)
    /// before `then` gets its result — so the step never lingers under what came of it.
    #[track_caller]
    pub fn step<W, R, T>(
        &mut self,
        jobs: crate::event::Jobs,
        key: &'static str,
        msg: impl Into<String>,
        work: W,
        then: T,
    ) where
        W: FnOnce() -> R + Send + 'static,
        R: Send + 'static,
        T: FnOnce(&mut Editor, R) + Send + 'static,
    {
        self.set_step(key, msg);
        jobs.spawn(move || {
            let result = work();
            move |ed: &mut Editor| {
                ed.step_done(key);
                then(ed, result)
            }
        });
    }

    /// Record only, no toast — `status` is never drawn. Only for what the screen shows already (the
    /// diagnostic at the cursor, the selections just made); anything else the user has to see is a toast.
    pub fn note(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Severity::Info));
    }

    /// Words a flash says at the eye (`Editor::flash_saying`): kept in `:messages` and the log, and a note — their
    /// toast waits on the frame (`Editor::words_drawn`). The words as said.
    pub(crate) fn flashed_note(&mut self, kind: ToastKind, msg: String) -> String {
        let msg = self.said_by_session(msg);
        self.recorded(kind, msg.clone());
        self.note(msg.clone());
        msg
    }

    /// Words a flash was to say that no frame drew it saying: their toast after all (kept already).
    pub(crate) fn flash_unseen(&mut self, kind: ToastKind, msg: String) {
        let of = self.behind.then_some(self.session.id);
        self.toast(kind, msg, of);
    }

    /// `msg` as a toast of `kind` — `set_status`, `set_success`, … by its kind.
    pub(crate) fn say(&mut self, kind: ToastKind, msg: String) {
        self.report(kind, msg);
    }

    fn toast(&mut self, kind: ToastKind, text: String, of: Option<crate::event::SessionId>) {
        // (`:messages` up: said there, a row of it — never the same words twice — once the frame shows that row; else
        // a toast as usual, an error never only on the status line: `messages::rows_drawn`)
        let row = self.messages_pane.is_some();
        // The card is gone (all had their time), or fading out after it lost the focus (`toast.rs`) → this one
        // starts a new card
        let fading = self.toast_letgo.take().is_some();
        if fading || !self.toasts.iter().any(Toast::alive) {
            self.toasts.clear();
            self.screen.toast_narrow.set(None);
        }
        // (the card focused: what joins it is held with it)
        let held = |mut t: Toast, focused: bool| {
            if focused {
                t.life = crate::toast::HELD;
            }
            t
        };
        let focused = self.toast_focus.is_some();
        // The same message again: its time starts over, where it is (moving it would make the card jump)
        let id = self.said;
        if let Some(t) = self.toasts.iter_mut().find(|t| t.text == text && t.kind == kind) {
            *t = held(Toast { id, of, row, ..Toast::new(kind, text) }, focused);
            if let Some(f) = self.toast_focus.as_mut().filter(|f| f.selected == t.id) {
                f.selected = id;
            }
            return;
        }
        self.toasts.push(held(Toast { id, of, row, ..Toast::new(kind, text) }, focused));
        // (the card shows the newest few and counts the rest — all of them are in `:messages`)
        if self.toasts.len() > MESSAGES_KEPT {
            self.toasts.remove(0);
        }
    }

    /// After a frame (`term::render`): the card drawn, its messages have been seen; not drawn (a picker over
    /// it, no free corner — the status line carried the newest), an error or warning not seen yet waits
    /// (`Toast::waiting`), so it comes up as a card once it can (U7: never only on the status line). Its
    /// time starts when the card first shows it — and only then does it need frames (its fade-in): a
    /// waiting one asks for none, where restarting its time every frame drew 14 frames a second.
    pub fn toasts_drawn(&mut self, card: bool) {
        if !card {
            return;
        }
        let now = crate::clock::now();
        let mut started = false;
        for t in self.toasts.iter_mut().filter(|t| !t.shown) {
            if t.waiting() {
                t.at = now;
                started = true;
            }
            t.shown = true;
        }
        if started {
            self.schedule_tick();
        }
    }

    /// The messages on the notification card, oldest first — not one said while `:messages` is up that the frame
    /// hasn't found out of the list's sight yet (`Toast.row`, `messages::rows_drawn`).
    pub fn card_toasts(&self) -> Vec<&Toast> {
        self.toasts.iter().filter(|t| !t.row).collect()
    }

    /// The card's opacity 0..1: fades in with its first message (150 ms), out with the last one's time (400 ms).
    /// Where nothing moves on a clock (`editor.motion`) it's there whole until it goes.
    pub fn toast_alpha(&self) -> f32 {
        let toasts = self.card_toasts();
        let Some(oldest) = toasts.iter().map(|t| t.age()).max() else { return 0.0 };
        if !self.moving() {
            return 1.0;
        }
        // (let go of the keys: its own short fade, from half strength — `toast::LETGO`)
        if let Some(at) = self.toast_letgo {
            let left = 1.0 - crate::clock::since(at).as_secs_f32() / crate::toast::LETGO.as_secs_f32();
            return (0.5 * left).clamp(0.0, 0.5);
        }
        let left = toasts.iter().map(|t| t.life.saturating_sub(t.age())).max().unwrap_or_default();
        (oldest.as_secs_f32() / TOAST_FADE_IN.as_secs_f32())
            .min(left.as_secs_f32() / TOAST_FADE.as_secs_f32())
            .clamp(0.0, 1.0)
    }

    /// For the status line — pending count and keys.
    pub fn pending_display(&self) -> String {
        // (a view mode's own, while it has the keys)
        if let Some((_, k)) = crate::view::viewing(self).filter(|_| self.pending.is_empty()) {
            return k.pending_display();
        }
        if let Some((_, o)) = crate::rundoc::keyed(self).filter(|_| self.pending.is_empty()) {
            return o.keys.pending_display();
        }
        // (the chat's input's — the `j` of an `[insert]` sequence, held until the next key)
        if let Some(c) = self.session.chat.as_ref().filter(|c| {
            self.pending.is_empty()
                && self.session.is_at(crate::session::At::Chat)
                && !crate::chatdoc::keyed(self)
                && !c.input.pane.keys.pending.is_empty()
        }) {
            return c.input.pane.keys.pending_display();
        }
        let mut s = self.count.map(|c| c.to_string()).unwrap_or_default();
        for k in &self.pending {
            s.push_str(&k.to_string());
        }
        // (`f`, `r`, `"` … wait for a character: the next key is taken as one — said, it showed nothing)
        if self.on_next_char.is_some() {
            s.push_str(&self.char_for);
            s.push('…');
        }
        s
    }

    // ── Undo groups ───────────────────────────────────────────────────────

    fn open_undo_group(&mut self) {
        // Focus moved to another document mid-group (pane click in insert mode) — the group stays with its own
        if self.undo_group.as_ref().is_some_and(|(id, _)| *id != self.doc().id) {
            self.close_undo_group();
        }
        if self.undo_group.is_none() {
            let doc = self.doc();
            self.undo_group = Some((doc.id, doc.snapshot()));
        }
    }

    /// Insert mode `C-s`: what was typed so far becomes its own undo step; typing goes on in a new one.
    pub fn commit_undo_checkpoint(&mut self) {
        self.close_undo_group();
        self.open_undo_group();
    }

    /// This document's language (by its grammar, else by its path — also when the grammar isn't there).
    pub fn lang_spec(&self) -> Option<&'static syntax::LangSpec> {
        let doc = self.doc();
        doc.syntax
            .as_ref()
            .and_then(|s| syntax::spec(&s.lang.name))
            .or_else(|| doc.path.as_deref().and_then(syntax::detect))
    }

    /// Whether `doc`'s long lines wrap (`editor.soft-wrap`) — "prose" = Markdown, commit messages, and
    /// files with no language.
    /// A buffer's own choice (`z w`, `:soft-wrap`) goes first.
    pub fn wraps(&self, doc: &Document) -> bool {
        if let Some(on) = doc.wrap {
            return on;
        }
        match self.config.soft_wrap.as_str() {
            "always" => true,
            "never" => false,
            _ => crate::syntax::prose(doc.lang_name()),
        }
    }

    /// `z w` · `:soft-wrap [on|off]`: this buffer's long lines wrap, or scroll sideways — `on` None flips
    /// what it does now. Said, with what the setting would do (#156).
    pub fn soft_wrap_toggle(&mut self, on: Option<bool>) {
        let on = on.unwrap_or(!self.wraps(self.doc()));
        let doc = self.doc_mut();
        doc.wrap = Some(on);
        if on {
            doc.left = 0;
        }
        let said = if on { "long lines wrap" } else { "long lines scroll sideways" };
        self.set_status(format!("{said} in this buffer — editor.soft-wrap is {}", self.config.soft_wrap));
    }

    /// Insert-mode auto-pairs for this document — empty when turned off.
    pub fn pairs(&self) -> &'static [(char, char)] {
        // (the chat's input is prose: what's typed is what's there)
        if !self.config.auto_pairs || self.session.swapped == Some(crate::docpane::Which::Input) {
            return &[];
        }
        self.lang_spec().and_then(|s| s.auto_pairs.as_deref()).unwrap_or(syntax::DEFAULT_PAIRS)
    }

    fn close_undo_group(&mut self) {
        if let Some((id, snap)) = self.undo_group.take()
            && let Some(doc) = self.docs.iter_mut().find(|d| d.id == id)
        {
            doc.commit_undo(snap);
        }
    }

    /// Whether an edit group is open on buffer `id` (typing in it, in insert mode).
    pub(crate) fn editing(&self, id: DocId) -> bool {
        self.undo_group.as_ref().is_some_and(|(d, _)| *d == id)
    }

    /// undo/redo itself doesn't create an edit group.
    pub fn abandon_undo_group(&mut self) {
        self.undo_group = None;
    }

    // ── Dispatch ─────────────────────────────────────────────────────────

    pub fn handle_event(&mut self, ev: Event) {
        // A hover moves nothing: only what's under the mouse says (`presence::hovered` — a frame only when that
        // changed), none of the work every other event does below (a sweep sends dozens a second)
        if let Event::Hover(x, y) = ev {
            return crate::presence::hovered(self, x as usize, y as usize);
        }
        // Input means the terminal has focus, whatever it said last (a focus-in lost on the way)
        let input = matches!(ev, Event::Key(_) | Event::Mouse(_) | Event::Paste(_));
        let before = input.then(|| crate::edits::before_input(self));
        // (a shuttle's step hands its keys in through here too, one event each — `shuttle.rs`: they say nothing
        // of the terminal's focus, and what they ask for isn't the person's own)
        let person = input && self.shuttles.stepping.is_none();
        if person {
            self.unfocused = false;
        }
        // (what it asks for — now, or through jobs it starts — was asked by this input: the count it's about to
        // take, first thing in `handle_key`/`handle_paste`, at a click's press — #205)
        let asked = person.then(|| self.events.set_asked(Some(self.input_seq + 1)));
        match ev {
            Event::Key(k) => {
                self.handle_key(k);
                self.auto_save_idle();
                self.plugin_input_went_in();
            }
            Event::Mouse(m) => self.handle_mouse(m),
            Event::Hover(..) => {}
            Event::Focus(gained) => {
                self.unfocused = !gained;
                if !gained {
                    crate::presence::away(self);
                }
                self.on_focus(gained)
            }
            Event::Resize => {}
            Event::Paste(text) => {
                self.handle_paste(text);
                self.plugin_input_went_in();
            }
            Event::Job(apply) => apply(self),
            Event::For(session, apply) => self.for_session(session, apply),
        }
        if let Some(was) = asked {
            self.events.set_asked(was);
        }
        // Whether the event changed nothing on screen is the event's to say: what runs after every event below
        // can make it drawn, never not (#200: a look at a card behind said "nothing" for every key for 30 min)
        let unchanged = self.unchanged;
        if let Some(before) = before {
            crate::edits::taken_up(self, before);
        }
        self.sync_focus_view();
        self.focus_sync();
        self.layout_keep();
        self.schedule_parses();
        self.lsp_flush();
        self.inlay_schedule();
        self.git_schedule();
        self.blame_schedule();
        self.sidebar_sync();
        self.git_inline_sync();
        self.previews_sync();
        self.sessions_sync();
        crate::restart::sync(self);
        self.picker_filter();
        self.conflicts_sync();
        self.disk_sync();
        self.recovery_sync();
        self.saves_due(false);
        crate::writing::reveal(self);
        crate::follow::watch(self);
        crate::presence::sync(self);
        crate::overlap::sync(self);
        crate::keepclose::sync(self);
        crate::weave::sync(self);
        crate::web::open::sync(self);
        crate::notes::attach(self);
        crate::edits::next(self);
        crate::refactor::next(self);
        crate::codereview::land(self);
        self.offer_view_sync();
        self.offer_group_sync();
        crate::shuttle::asking_sync(self);
        crate::notes::stale_sync(self);
        crate::stuck::sync(self);
        self.bring_queue_sync();
        self.plugin_front();
        self.schedule_tick();
        #[cfg(test)]
        if let Some(step) = AFTER_EVENT.get() {
            step(self);
        }
        // (one of them can clear it — `schedule_tick` when toasts went: drawn whatever the event was)
        self.unchanged &= unchanged;
    }

    // ── Mouse ────────────────────────────────────────────────────────────

    /// Press = place cursor (alt adds one) · drag = select · wheel = 3 lines. Clicking the chat pane
    /// focuses it; in a picker, wheel = move selection, click = open.
    /// Pasted text (the outer terminal's bracketed paste): to the built-in terminal when it has the keys; typed
    /// into a picker, prompt, or the chat (one line — newlines as spaces; the chat keeps them); in insert mode
    /// into the buffer as one edit; in normal or select mode held in the register for `p`/`P` (a `:` line: the
    /// command line).
    pub fn handle_paste(&mut self, text: String) {
        let begun = crate::document::input_begin();
        self.handle_paste_now(text);
        self.typed(begun);
    }

    fn handle_paste_now(&mut self, text: String) {
        self.input_came();
        if self.picker.is_none() && self.prompt.is_none() && self.terminal_paste(&text) {
            return;
        }
        let text = text.replace("\r\n", "\n").replace('\r', "\n");
        // One line for a query or a prompt; the chat keeps its lines
        let line = || -> String {
            text.chars()
                .filter_map(
                    |c| if c == '\n' || c == '\t' { Some(' ') } else { (!c.is_control()).then_some(c) },
                )
                .collect()
        };
        if let Some(p) = self.picker.as_mut() {
            p.push_str(&line());
            self.workspace_symbols_requery();
            self.live_search();
            self.preview_theme();
            self.code_action_preview();
            return;
        }
        if self.prompt.is_some() {
            for c in line().chars() {
                self.handle_key(Key::plain(Code::Char(c)));
            }
            return;
        }
        // Straight into the chat's input — as keys, its letters could answer a review or an offer first
        if self.session.is_at(At::Chat)
            && let Some(c) = self.session.chat.as_mut()
        {
            let text: String = text.chars().filter(|&c| c == '\n' || !c.is_control()).collect();
            c.input.insert_str(&text);
            // (minimized, its input isn't on screen: it comes back with what was pasted — never text unseen)
            if c.compact() {
                c.unfolded = true;
                self.set_status("pasted into the chat — it was minimized; it's back");
            }
            return;
        }
        if self.session.review.is_some() {
            return self.set_status("finish reviewing the answer first");
        }
        // A `:` line pasted in normal mode is a command meant for the command line (2026-10-04: `:w` pasted
        // into a commit message became part of its subject, unsaid): it opens the line with it, not run —
        // Enter runs it, Esc drops it. (Text that starts with `:` goes in from insert mode, as typed.)
        if self.mode == Mode::Normal
            && let Some(cmd) = text.strip_suffix('\n').unwrap_or(&text).strip_prefix(':')
            && !cmd.contains('\n')
        {
            let cmd = cmd.to_string();
            self.open_prompt(PromptKind::Command, &cmd);
            return self
                .set_status("a pasted : line — in the command line, not run: enter runs it, esc close");
        }
        // A file under review (a turn's, a branch's) took the keys when the turn ended — a paste in normal mode
        // there was meant for the chat (#31: it landed in the file as an unsaved edit). It goes to the chat.
        // (In insert mode it's typed there on purpose.)
        // (the git tab's inline base isn't a review's: a file it shows isn't under review)
        let reviewing =
            self.session.code_review.is_some() && self.doc().own_base() && self.mode != Mode::Insert;
        if reviewing && let Some(c) = self.session.chat.as_mut() {
            let text: String = text.chars().filter(|&c| c == '\n' || !c.is_control()).collect();
            c.input.insert_str(&text);
            self.session.at = At::Chat;
            return self.set_status("pasted into the chat — the file is under review (i pastes there)");
        }
        // Normal or select mode: held, not put in (#124 — it went into the file, said only by a toast; a paste
        // there is mostly meant elsewhere). In the default register, so `p` puts it after the selection and
        // `P` before, as one edit; insert mode types it where you type.
        if self.mode != Mode::Insert {
            let lines = text.trim_end_matches('\n').lines().count().max(1);
            let what = if lines == 1 { "a line".to_string() } else { format!("{lines} lines") };
            self.write_register('"', vec![text]);
            self.mode = Mode::Normal;
            return self.set_status(format!(
                "pasted {what} — held, not in the file: p puts it after the selection, P before (i to type it in)"
            ));
        }
        if self.doc().loading {
            return self.set_warning("still loading — paste once it's read");
        }
        // A file with CRLF lines gets CRLF lines
        let crlf = self.doc().text.lines().next().is_some_and(|l| {
            let n = l.len_bytes();
            n >= 2 && l.byte(n - 2) == b'\r' && l.byte(n - 1) == b'\n'
        });
        let text = if crlf { text.replace('\n', "\r\n") } else { text };
        // A list or signature card for the text before it would be stale (accepting would replace the paste)
        self.completion = None;
        self.signature = None;
        // (insert mode: typed where you type)
        self.with_group(|cx| crate::commands::paste_text(cx, text));
    }

    pub fn handle_mouse(&mut self, m: crate::event::Mouse) {
        use crate::event::MouseKind as K;
        // (a press anywhere starts afresh: only a ctrl+press in the code arms its release)
        if m.kind == K::Down {
            self.mouse_goto = false;
        }
        // Input for follow = what can move the code's cursor: a press or a drag (anywhere — a chat link, a
        // side panel row, a pane), and the wheel over the code (below). A release, or the wheel over a
        // panel, moves nothing — counted, a reload in the same moment read as you taking over.
        if matches!(m.kind, K::Down | K::Drag) {
            self.input_came();
        }
        let (x, y) = (m.x as usize, m.y as usize);
        // Labels up for pointing: a click picks the element under it, or stops pointing; nothing else moves
        if self.pointing.is_some() {
            if m.kind == K::Down {
                crate::uistate::point::click(self, x, y);
            }
            return;
        }
        let s = self.screen.clone();
        self.status = None;
        // The prompt's card: what's under it isn't clicked through it
        if let Some((px, py, pw, ph)) = s.prompt_card.get()
            && (px..px + pw).contains(&x)
            && (py..py + ph).contains(&y)
        {
            return;
        }
        // A card: a press on one that waits focuses it (P3 §3.8 — its answers answer it from then on, a press on
        // one as it came up never one), a stacked one brought to the front with the keys; focused, its answers
        let button = s.offer_buttons.iter().find(|b| b.0 == y && (b.1..b.2).contains(&x)).map(|b| b.3);
        let on_card = s
            .offer_card
            .is_some_and(|(cx, cy, cw, ch)| (cx..cx + cw).contains(&x) && (cy..cy + ch).contains(&y));
        // The notification card: a press on it focuses it, focused one on a message takes it in hand (`toast.rs`); a
        // press anywhere else lets a focused one go
        let on_toast = s
            .toast_card
            .get()
            .is_some_and(|(tx, ty, tw, th)| (tx..tx + tw).contains(&x) && (ty..ty + th).contains(&y));
        if m.kind == K::Down && on_toast && !on_card && button.is_none() && self.picker.is_none() {
            // (a prompt open has the keys: the press is its — the code's under it isn't clicked either — said, never a
            // focus the prompt takes back the same frame, `toast_sync`)
            if self.prompt.is_some() {
                return self
                    .set_status("the prompt has the keys — esc close, then a press takes the message");
            }
            return self.toast_press(y);
        }
        if m.kind == K::Down && self.toast_focus.is_some() {
            self.toast_let_go();
        }
        if m.kind == K::Down && (button.is_some() || on_card) {
            let has = self.card_has_keys();
            match button {
                Some(d) if d.is_ascii_digit() => self.offer_peek(d.to_digit(10).unwrap_or(0) as usize),
                Some(k) if has => {
                    let weighty = self.offers.first().is_some_and(|o| o.weighty_answer(k));
                    match k {
                        _ if weighty && !self.offer_seen_whole() => self.offer_see_all_first(),
                        'y' => self.offer_accept(),
                        'v' => self.offer_view(),
                        'r' => self.offer_one_by_one(),
                        'a' => self.offer_for_chat(),
                        's' if self.offers.first().is_some_and(crate::offer::Offer::for_session) => {
                            self.offer_for_session()
                        }
                        'A' => self.offer_always(),
                        'D' => self.offer_remove(true),
                        'R' => self.offer_remove(false),
                        'c' => self.offer_commit_first(),
                        'p' => self.offer_preflight_first(),
                        'X' => self.offer_abort(),
                        'n' => self.offer_dismiss(),
                        'N' => self.offer_never(),
                        _ => {}
                    }
                }
                _ if !has && self.offers.first().is_some_and(|o| !o.installing()) => {
                    self.card_focus_front();
                    self.set_status("the card has the keys — its answers answer it, esc gives them back");
                }
                _ => {}
            }
            return;
        }
        // A border being dragged (the chat's edge, the side panel's, the bottom panel's top row): it follows the
        // mouse; let go, the size is kept
        if let Some(b) = self.dragging {
            match m.kind {
                K::Drag => self.border_drag(b, x, y),
                K::Up => {
                    self.dragging = None;
                    self.border_done(b);
                }
                _ => self.dragging = None,
            }
            return;
        }
        // A floating frame held (P3 step 7): its top line moves it, its edges size it; let go, kept
        if let Some((kind, grip)) = self.float_dragging {
            match m.kind {
                K::Drag => self.float_drag(kind, grip, x, y),
                K::Up => {
                    self.float_dragging = None;
                    self.float_drag_done(kind, grip);
                }
                _ => self.float_dragging = None,
            }
            return;
        }
        // (what's under a floating pane isn't pressed through it: the topmost there takes it)
        let floated = self.float_at(x, y).map(|f| f.0).filter(|_| self.picker.is_none());
        if m.kind == K::Down
            && floated.is_some()
            && let Some((kind, grip)) = self.float_grip(x, y)
        {
            self.float_dragging = Some((kind, grip));
            self.float_focus(kind);
            return;
        }
        // A floating list (the session list, `:messages`) — or the session list docked: a press on a row selects it
        // (again: opens it), the wheel moves through it
        let docked_list = crate::floatlist::KINDS.into_iter().find(|&k| {
            self.dock_rect(k)
                .is_some_and(|(_, r)| (r.x..r.x + r.w).contains(&x) && (r.y..r.y + r.h).contains(&y))
        });
        let list = floated.filter(|&k| crate::floatlist::is_list(k));
        if let Some(kind) = list.or(docked_list.filter(|_| floated.is_none() && self.picker.is_none())) {
            match m.kind {
                K::Down => match s.list(kind) {
                    Some((y0, rows, top)) if (y0..y0 + rows).contains(&y) => {
                        self.float_list_click(kind, top + y - y0)
                    }
                    _ => self.float_focus(kind),
                },
                K::ScrollDown | K::ScrollUp => crate::floatlist::wheel(self, kind, m.kind == K::ScrollUp),
                _ => {}
            }
            return;
        }
        if m.kind == K::Down
            && self.picker.is_none()
            && floated.is_none()
            && let Some(b) = self.border_at(x, y)
        {
            self.dragging = Some(b);
            return;
        }
        // The sessions' tabs, dropdown, chip
        if m.kind == K::Down
            && self.picker.is_none()
            && floated.is_none()
            && let Some(&(.., spot)) =
                s.session_spots.borrow().iter().find(|l| l.0 == y && (l.1..l.2).contains(&x))
        {
            return self.session_spot(spot);
        }
        // The bottom slot's tabs: that panel in front
        // (the last drawn first: a floating frame's names over what's under it)
        if m.kind == K::Down
            && self.picker.is_none()
            && let Some(&(.., slot)) =
                s.slot_tabs.borrow().iter().rev().find(|t| t.0 == y && (t.1..t.2).contains(&x))
        {
            return self.slot_show(slot);
        }
        // A group member's name in the slot's line: the keys to it (stacked, it comes into sight)
        if m.kind == K::Down
            && self.picker.is_none()
            && let Some(&(.., id)) =
                s.member_names.borrow().iter().rev().find(|t| t.0 == y && (t.1..t.2).contains(&x))
        {
            return self.focus_pane(id);
        }
        if let Some(p) = self.picker.as_mut() {
            // (a click acts on this query's rows, not the last one's)
            if m.kind == K::Down {
                p.filter_now();
            }
            let over_preview = s.picker_preview_x.is_some_and(|px| x >= px);
            match m.kind {
                K::ScrollUp if over_preview => p.preview_scroll = p.preview_scroll.saturating_sub(3),
                K::ScrollDown if over_preview => p.preview_scroll += 3,
                K::ScrollUp => p.move_by(-3),
                K::ScrollDown => p.move_by(3),
                // A click selects the row (the preview follows); the same row again soon after — a double
                // click — opens it, as Enter does (N3). A folder (nothing to preview) folds or unfolds at once.
                K::Down => {
                    if let Some((top, rows)) = s.picker_list
                        && (top..top + rows).contains(&y)
                        && let Some(row) = p.row_at(y - top)
                        && p.select_row(row)
                    {
                        let (id, folder) =
                            (p.id, p.current().is_some_and(|i| matches!(i.action, Action::Dir(_))));
                        let again = self
                            .picker_click
                            .is_some_and(|(i, r, at)| (i, r) == (id, row) && at.elapsed() < DOUBLE_CLICK);
                        self.picker_click = (!again).then(|| (id, row, std::time::Instant::now()));
                        if folder || again {
                            self.handle_key(Key::plain(Code::Enter));
                        }
                    }
                }
                _ => {}
            }
            return;
        }
        // Wheel over the floating docs (hover, `space g p`) scrolls them
        if let (Some(lines), Some((px, py, pw, ph))) = (self.popup.as_ref(), s.popup.get())
            && (px..px + pw).contains(&x)
            && (py..py + ph).contains(&y)
        {
            match m.kind {
                K::ScrollDown => return self.popup_scroll = (self.popup_scroll + 3).min(lines.len()),
                K::ScrollUp => return self.popup_scroll = self.popup_scroll.saturating_sub(3),
                _ => {}
            }
        }
        // The review's file rail: a click opens that file — on its mark, marks it viewed too (#67) — a click
        // on `↑/↓ N more` or the wheel scrolls it (`CodeReview::fit` clamps; #73: the rail is its columns, left
        // of where the code and the slot under it start — not "left of the first pane")
        let on_rail = self.review_rail_up() && x < s.slot_x && !s.review_rail.borrow().is_empty();
        let rail_keys = self.session.is_at(At::Rail);
        if on_rail && let Some(r) = self.session.code_review.as_mut() {
            let by = match m.kind {
                K::ScrollDown => 3,
                K::ScrollUp => -3,
                K::Down => s.review_rail_more.borrow().iter().find(|r| r.0 == y).map_or(0, |r| r.1),
                _ => 0,
            };
            // (with the keys, the file in hand follows into sight — as `C-e`/`C-y`; without them it's the code's
            // file and the wheel only looks)
            if by != 0 {
                return if rail_keys { r.scroll_keeping(by) } else { r.scroll_by(by) };
            }
        }
        if m.kind == K::Down && on_rail {
            let mark = s
                .review_rail_marks
                .borrow()
                .iter()
                .find(|r| r.0 == y && (r.2..r.2 + 2).contains(&x))
                .map(|r| r.1);
            if let Some(i) = mark {
                return crate::codereview::mark_clicked(self, i);
            }
            // (a click shows it in the preview slot; a double-click keeps it, as `enter`)
            if let Some(&(_, i)) = s.review_rail.borrow().iter().find(|r| r.0 == y) {
                // (the buffer that show opened — none for a binary row: never whatever was in front)
                let shown = crate::codereview::show(self, i, false);
                if self.clicked_again("rail", i)
                    && let Some(id) = shown
                {
                    self.preview_keep(id);
                }
                return;
            }
        }
        // A group docked beside the code (P3 step 6) or floating (step 7): what's under the pointer in its rect is its
        // panel's
        let docked = floated.or_else(|| {
            (s.docked.borrow().iter())
                .find(|d| (d.1.x..d.1.x + d.1.w).contains(&x) && (d.1.y..d.1.y + d.1.h).contains(&y))
                .map(|d| d.0)
        });
        // The bottom slot's columns: under the code, between the side panel (or a group docked left) and the chat
        // (or one docked right) — or a docked group's
        let in_slot = docked.is_some() || (x >= s.slot_x && x < s.slot_end());
        // (the run panel there: the docked one's, else the slot's)
        let slot = match docked {
            Some(k) => k.slot(),
            None => self.slot_shown(),
        };
        // The terminal: the wheel scrolls back, a click gives it the keys (a click anywhere else takes them)
        let in_terminal =
            s.terminal_rows.is_some_and(|(y0, y1)| (y0..=y1).contains(&y)) && in_slot && docked.is_none();
        // A press anywhere ends a selection whose release was lost (a pane switch mid-drag)
        let owned = std::mem::replace(&mut self.mouse_in_terminal, false) && m.kind != K::Down;
        if m.kind == K::Down
            && let Some(t) = self.session.terminal.as_mut()
        {
            t.selecting = false;
            if !in_terminal {
                t.selection = None;
                if self.session.is_at(At::Program) {
                    self.session.at = At::Code;
                }
            }
        }
        // A press there, the wheel there, or the drag and release of a press there (clamped to its edge — a
        // program that has the mouse always gets its release)
        let wheel = matches!(m.kind, K::ScrollUp | K::ScrollDown);
        if owned || (in_terminal && (m.kind == K::Down || wheel)) {
            // Its screen starts under the header (`chrome::SLOT_HEAD`), the padding in from the slot's edge
            let (head, x0) = (crate::chrome::SLOT_HEAD, s.slot_x + crate::chrome::PAD);
            let cell = s.terminal_rows.and_then(|(y0, y1)| {
                let inside = in_terminal && y >= y0 + head && x >= x0;
                (inside || owned).then(|| {
                    ((y.clamp(y0 + head, y1.max(y0 + head)) - y0 - head) as u16, x.saturating_sub(x0) as u16)
                })
            });
            self.mouse_in_terminal = (owned && m.kind == K::Drag) || (m.kind == K::Down && cell.is_some());
            self.terminal_mouse(m.kind, cell, m.alt);
            return;
        }
        // Another press's drag over it isn't the terminal's (nor moves the code's cursor); its release ends it
        if in_terminal {
            if m.kind == K::Up {
                self.mouse_anchor = None;
            }
            return;
        }
        // `:procs`' panel: the same — its detail beside the table (or stacked in its place) a run panel's document
        if let Some(a) = s.procs_panel.get()
            && (a.top..=a.bottom).contains(&y)
            && in_slot
            && slot == Some(crate::sizes::Slot::Procs)
        {
            let detail = s
                .run_rows
                .of(crate::sizes::Slot::Procs)
                .filter(|r| (r.1..r.1 + r.3).contains(&y) && x + 1 >= r.2)
                .filter(|_| !(a.x..a.x + a.w).contains(&x));
            let slot = crate::sizes::Slot::Procs;
            match m.kind {
                K::Down if let Some(r) = detail => {
                    crate::rundoc::click(self, slot, y - r.1, x.saturating_sub(r.2))
                }
                K::ScrollDown | K::ScrollUp if detail.is_some() => {
                    crate::rundoc::wheel(self, slot, m.kind == K::ScrollUp)
                }
                K::Down => {
                    let row = (a.first..a.first + a.shown).contains(&y) && (a.x..a.x + a.w).contains(&x);
                    self.procs_click(row.then(|| a.scroll + y - a.first))
                }
                // (the wheel scrolls it as `C-e`/`C-y` do, three rows: the selection stays unless it would leave
                // the rows in sight, then the nearest — `s` and `r` act on it, so it's never out of sight)
                K::ScrollDown | K::ScrollUp => {
                    let by = if m.kind == K::ScrollDown { 3 } else { -3 };
                    crate::grouplist::wheel(self, crate::pane::GroupKind::Procs, by)
                }
                _ => {}
            }
            return;
        }
        // A plugin's panel: a click takes the keys there (on a row, selects it); the wheel moves through it
        if let Some((y0, y1, first, scroll)) = s.plugin_panel.get()
            && (y0..=y1).contains(&y)
            && in_slot
            && self.plugin_panel_in_sight()
        {
            match m.kind {
                // (the rows: not the header, not the status line under them)
                K::Down => self.plugin_panel_click((first..y1).contains(&y).then(|| scroll + y - first)),
                K::ScrollDown => self.plugin_panel_wheel(3),
                K::ScrollUp => self.plugin_panel_wheel(-3),
                _ => {}
            }
            return;
        }
        // The test panel: the wheel scrolls its detail (the failure's message, or the output) — its document's rows
        // while it has the keys; a press gives it the keys — on its detail, the cursor there
        // (the row of the output under the pointer, and its column — as the panel drew them, `Screen.run_rows`)
        let run_row = |slot: crate::sizes::Slot| {
            s.run_rows
                .of(slot)
                .filter(|r| (r.1..r.1 + r.3).contains(&y) && x >= r.2)
                .map(|r| (y - r.1, x - r.2))
        };
        if let Some((y0, y1, _)) = s.test_panel.get()
            && slot == Some(crate::sizes::Slot::Test)
            && (y0..=y1).contains(&y)
            && in_slot
        {
            let at = run_row(crate::sizes::Slot::Test);
            match m.kind {
                K::ScrollDown | K::ScrollUp => {
                    crate::rundoc::wheel(self, crate::sizes::Slot::Test, m.kind == K::ScrollUp)
                }
                K::Down => match at {
                    Some((r, c)) => crate::rundoc::click(self, crate::sizes::Slot::Test, r, c),
                    None if let Some(i) = self.list_row(crate::pane::GroupKind::Tests, x, y) => {
                        crate::grouplist::click(self, crate::pane::GroupKind::Tests, i)
                    }
                    None => self.panel_take_keys(crate::sizes::Slot::Test),
                },
                _ => {}
            }
            return;
        }
        // The task panel: the same, over its output
        if let Some((y0, y1, _)) = s.task_panel.get()
            && slot == Some(crate::sizes::Slot::Task)
            && (y0..=y1).contains(&y)
            && in_slot
        {
            // (a preflight's step chip: its output from its heading — #72)
            let chip =
                s.task_steps.borrow().iter().find(|c| c.0 == y && (c.1..c.2).contains(&x)).map(|c| c.3);
            let step = chip.and_then(|k| Some(self.session.preflight.as_ref()?.steps.get(k)?.name.clone()));
            let at = run_row(crate::sizes::Slot::Task);
            // (a row of its list — its places, a preflight's steps — laid out beside the output)
            let listed = self
                .kind_of(crate::sizes::Slot::Task)
                .and_then(|kind| Some((kind, self.list_row(kind, x, y)?)));
            match m.kind {
                K::Down if let Some((kind, i)) = listed => crate::grouplist::click(self, kind, i),
                K::ScrollDown | K::ScrollUp => {
                    crate::rundoc::wheel(self, crate::sizes::Slot::Task, m.kind == K::ScrollUp)
                }
                // (its output from that step's heading, at the top)
                K::Down if let Some(name) = step.filter(|_| self.session.task_run.is_some()) => {
                    crate::rundoc::step(self, &name)
                }
                K::Down => match at {
                    Some((r, c)) => crate::rundoc::click(self, crate::sizes::Slot::Task, r, c),
                    None => self.panel_take_keys(crate::sizes::Slot::Task),
                },
                _ => {}
            }
            return;
        }
        // (the rest of a floating frame: the keys to it, nothing under it pressed)
        if let Some(kind) = floated {
            if m.kind == K::Down {
                self.float_focus(kind);
            }
            return;
        }
        // The side panel: a click opens that row (a folder folds or unfolds), the wheel scrolls it
        if self.session.sidebar.is_some()
            && !self.review_rail_up()
            && x < s.panes.iter().map(|p| p.1.x).min().unwrap_or(0)
        {
            let row = s.sidebar.borrow().iter().find(|l| l.0 == y).map(|l| l.1);
            let rows = s.sidebar.borrow().len();
            let side_keys = self.session.is_at(At::Side);
            let sb = self.session.sidebar.as_mut().unwrap();
            // (with the keys, as `C-e`/`C-y`: the selection, what they act on, follows into sight; without them it
            // marks where the code is and comes back there by itself — the wheel only looks)
            match m.kind {
                // (the git tab's file row: a click selects it and shows the file in the preview slot, the tab taking
                // the keys — as moving onto it; a double-click opens it, as `enter`)
                K::Down
                    if let Some(r) = row
                        && let Some(rel) = crate::sidebar::git_file_at(sb, r) =>
                {
                    if self.clicked_again("git", r) {
                        self.sidebar_activate(r);
                    } else {
                        if let Some(sb) = self.session.sidebar.as_mut() {
                            sb.selected = r;
                        }
                        self.session.leave(At::Chat);
                        self.session.at = At::Side;
                        self.git_show_file(&rel.0, rel.1);
                    }
                }
                K::Down => {
                    if let Some(r) = row {
                        self.sidebar_activate(r);
                    }
                }
                K::ScrollDown | K::ScrollUp if side_keys => {
                    sb.scroll_keeping(if m.kind == K::ScrollDown { 3 } else { -3 }, rows)
                }
                K::ScrollDown => sb.scroll += 3,
                K::ScrollUp => sb.scroll = sb.scroll.saturating_sub(3),
                _ => {}
            }
            return;
        }
        // The minimized chat: a trail row jumps there, anywhere else brings the panel back
        if let (Some((mx, my, mw, mh)), Some(c)) = (s.chat_mini.get(), self.session.chat.as_mut())
            && (mx..mx + mw).contains(&x)
            && (my..my + mh).contains(&y)
        {
            if m.kind == K::Down {
                let links = s.chat_links.borrow();
                match links.iter().find(|l| l.0 == y && (l.1..l.2).contains(&x)) {
                    Some((.., link)) => crate::chat::jump(self, link.clone()),
                    None => {
                        c.unfolded = true;
                        self.session.at = At::Chat;
                    }
                }
            }
            return;
        }
        if let (Some(cx), Some(_)) = (s.chat_x, self.session.chat.as_ref())
            && x >= cx
        {
            match m.kind {
                K::Down => {
                    self.session.at = At::Chat;
                    // Where a command ran, at its row's end: why, said (cardburden 4b)
                    let ran =
                        s.chat_ran.borrow().iter().find(|l| l.0 == y && (l.1..l.2).contains(&x)).cloned();
                    if let Some((.., call)) = ran {
                        return crate::chat::said_ran(self, &call);
                    }
                    // An element's id in an answer: into sight, the keys where they are
                    let ids = s.chat_ids.borrow();
                    if let Some((.., id)) = ids.iter().find(|l| l.0 == y && (l.1..l.2).contains(&x)) {
                        return self.ui_jump(id);
                    }
                    let links = s.chat_links.borrow();
                    if let Some((.., link)) = links.iter().find(|l| l.0 == y && (l.1..l.2).contains(&x)) {
                        // A tool call's row: its replay stop (an edit's changes shown), else the place
                        let call = s.chat_calls.borrow().iter().find(|c| c.0 == y).map(|c| c.1.clone());
                        if !call.is_some_and(|call| crate::follow::replay_call(self, &call)) {
                            crate::chat::jump(self, link.clone());
                        }
                        return;
                    }
                    // The conversation's text: its view mode, the cursor on the character pressed (as the code's)
                    let text_x = cx + crate::chrome::GAP + crate::chrome::PAD;
                    if let Some(row) = s
                        .chat_view
                        .get()
                        .and_then(|(y0, _, rows, first)| (y >= y0 && y < y0 + rows).then(|| first + (y - y0)))
                    {
                        crate::chatdoc::click(self, row, x.saturating_sub(text_x));
                    }
                }
                // The wheel over the conversation: its view mode at that point (tmux's copy mode by the wheel —
                // §3.8), the cursor on the row under the pointer
                K::ScrollUp | K::ScrollDown => {
                    let line = s.chat_view.get().and_then(|(y0, _, rows, first)| {
                        (y >= y0 && y < y0 + rows).then(|| first + (y - y0))
                    });
                    return crate::chatdoc::wheel(self, line, m.kind == K::ScrollUp);
                }
                _ => {}
            }
            return;
        }
        // The wheel over a change in review scrolls it (#61: a long insertion ran off the screen); a click on a
        // change takes it in hand (the view stays: it's in sight; one answered already, to change the answer, as
        // `tab` can't) — the review has no cursor, a click elsewhere moves nothing
        if let Some(r) = self.session.review.as_mut().filter(|r| r.ready)
            && x < s.chat_x.unwrap_or(usize::MAX)
        {
            match m.kind {
                K::ScrollDown => (r.scroll, r.follow) = (r.scroll + 3, false),
                K::ScrollUp => (r.scroll, r.follow) = (r.scroll.saturating_sub(3), false),
                K::Down => {
                    let hunk = s.review_rows.borrow().iter().find(|c| c.0 == y).map(|c| c.1);
                    if let Some(i) = hunk.filter(|&i| r.hunks.get(i).is_some_and(|h| !h.conflict)) {
                        (r.current, r.follow) = (i, false);
                    }
                }
                _ => {}
            }
        }
        if self.session.review.is_some() || self.welcome() {
            return;
        }
        // Which pane (with splits, focus moves to the pane clicked/scrolled). With several panes the top
        // row is the title row.
        let Some((pid, rect, gutter, lines)) = s
            .panes
            .iter()
            .find(|(_, r, ..)| (r.x..r.x + r.w).contains(&x) && (r.y..r.y + r.h).contains(&y))
            .cloned()
        else {
            return;
        };
        if matches!(m.kind, K::ScrollUp | K::ScrollDown) {
            self.input_came();
        }
        if matches!(m.kind, K::Down | K::ScrollUp | K::ScrollDown) {
            self.focus_view(pid);
        }
        // Scrolling the code yourself takes over from follow mode — the view back at your cursor first, the wheel
        // acting from there (clicks take over too, via the cursor)
        if matches!(m.kind, K::ScrollUp | K::ScrollDown) {
            crate::follow::your_view(self);
            if let Some(c) = self.session.chat.as_mut().filter(|c| c.follow.active() && c.busy()) {
                c.follow.paused = true;
            }
        }
        if pid != self.session.focus {
            return;
        }
        let title = usize::from(self.session.views.len() > 1);
        if y < rect.y + title {
            return;
        }
        let row = y - rect.y - title;
        self.session.leave(At::Chat);
        self.plugin_panel_unfocus();
        self.procs_unfocus();
        self.session.leave(At::Slot);
        // (a group's list, in the slot or docked: the code's too)
        if let At::List(id) = self.session.docked() {
            self.session.leave(At::List(id));
        }
        self.completion = None;
        self.popup = None;
        // (a woven view's conflict side by side: the column clicked is the part `enter` goes to, the cursor on its
        // line there — the row's own place in the conflict as drawn, the rows the view starts past counted too)
        let woven = |r: &crate::codeview::Row| match *r {
            crate::codeview::Row::Other { at, kind: crate::codeview::OtherKind::Woven(w), .. } => match w {
                crate::codeview::WovenRow::Head { part: None, .. } => Some((at, 0)),
                crate::codeview::WovenRow::Columns { index, .. } => Some((at, index + 1)),
                _ => None,
            },
            _ => None,
        };
        let picked = match (m.kind, self.view.get(row).and_then(woven)) {
            (K::Down, Some((at, into))) if !m.alt => {
                crate::weave::pick(self, at, into, x.saturating_sub(rect.x + gutter))
            }
            _ => None,
        };
        let tab = self.config.tab_width;
        let doc = self.doc();
        let last = mv::last_line(&doc.text);
        match m.kind {
            K::ScrollUp | K::ScrollDown => {
                let head = doc.selection().primary().cursor(&doc.text);
                let col = mv::visual_col(&doc.text, head, tab);
                let line = mv::line_of(&doc.text, head);
                let (line, top) = if m.kind == K::ScrollDown {
                    ((line + 3).min(last), (doc.top + 3).min(last))
                } else {
                    (line.saturating_sub(3), doc.top.saturating_sub(3))
                };
                let pos = mv::pos_at_col(&doc.text, line, col, tab);
                let doc = self.doc_mut();
                doc.set_top(top);
                doc.set_selection(Selection::point(pos));
            }
            K::Down | K::Drag => {
                let (line, c0, x0, end) =
                    lines.get(row).copied().unwrap_or((doc.top + row, doc.left, 0, usize::MAX));
                let line = line.min(last);
                // A wrapped row: cells after its indent, never past its end (that's the next row's)
                let col =
                    (c0 + x.saturating_sub(rect.x + gutter).saturating_sub(x0)).min(end.saturating_sub(1));
                let pos = match picked {
                    Some(l) => doc.text.line_to_byte(l),
                    None => mv::pos_at_col(&doc.text, line, col, tab),
                };
                // A plain press on a page's link that `gd` follows says so once (ctrl+click doesn't arrive from every
                // terminal: nothing tells the two apart then) — never a `mailto:`'s, whose `gd` says it goes nowhere
                let link = crate::web::page::of(doc).is_some_and(|p| {
                    p.facts.link_at(pos).is_some_and(|l| crate::web::facts::followed(&l.url))
                });
                let text = doc.text.clone();
                let anchor = self.mouse_anchor;
                // (a ctrl+press dragged off its character is a selection, not a `gd`)
                if m.kind == K::Drag && anchor.is_some_and(|a| a != pos) {
                    self.mouse_goto = false;
                }
                let doc = self.doc_mut();
                let mut new_anchor = anchor;
                match (m.kind, anchor) {
                    (K::Drag, Some(a)) => {
                        // Dragging forward includes the last char (selection = half-open byte range)
                        let r = if pos >= a {
                            Range::new(a, crate::graphemes::next_boundary(&text, pos))
                        } else {
                            Range::new(crate::graphemes::next_boundary(&text, a), pos)
                        };
                        doc.set_selection(Selection::new(vec![r], 0));
                    }
                    // A drag whose press wasn't in the text (the terminal's, a panel's) isn't the code's
                    (K::Drag, None) => {}
                    (K::Down, _) if m.alt => {
                        let mut ranges = doc.selection().ranges().to_vec();
                        ranges.push(Range::point(pos));
                        let n = ranges.len() - 1;
                        doc.set_selection(Selection::new(ranges, n));
                        new_anchor = Some(pos);
                    }
                    _ => {
                        doc.set_selection(Selection::point(pos));
                        new_anchor = Some(pos);
                    }
                }
                self.mouse_anchor = new_anchor;
                // ctrl+click goes there, inside tarae (as VS Code, JetBrains, Zed): a page's link followed, a name to
                // its definition, an address written in code opened as a page — the cursor put there first, so `C-o`
                // comes back to it; run at the release, so a ctrl+drag selects and ctrl+alt adds a cursor as without
                // ctrl
                if m.kind == K::Down && m.ctrl && !m.alt && picked.is_none() {
                    self.mouse_goto = true;
                    self.session.link_click_said = true;
                }
                if m.kind == K::Down && link && !m.alt && !self.session.link_click_said {
                    self.session.link_click_said = true;
                    self.set_status("gd follows a link · ctrl+click too, where the terminal sends it");
                }
            }
            K::Up => {
                self.mouse_anchor = None;
                // (an http(s) address written there — a comment's, a string's — opens as a page, `gf`'s act: inside
                // tarae, the system browser is `:web-open`'s; on a page a followed link there comes first — its words
                // may be an address of their own, `[https://a.example](https://b.example/x)`)
                if std::mem::take(&mut self.mouse_goto) {
                    let doc = self.doc();
                    let at = doc.selection().primary().cursor(&doc.text);
                    let followed = crate::web::facts::followed;
                    let link = crate::web::page::of(doc)
                        .is_some_and(|p| p.facts.link_at(at).is_some_and(|l| followed(&l.url)));
                    let url = !link && crate::gotofile::around(&doc.text, at).is_some_and(|u| followed(&u));
                    self.run_command_by_name(if url { "goto_file" } else { "goto_definition" });
                }
            }
        }
    }

    /// When to show the start screen: a single empty unnamed buffer and nothing typed — never a page or a server's
    /// document still empty while it's read (its line says so).
    pub fn welcome(&self) -> bool {
        self.docs.len() == 1
            && self.docs[0].path.is_none()
            && self.docs[0].virtual_uri.is_none()
            && self.docs[0].text.len_bytes() == 0
            && !self.docs[0].is_modified()
            && self.mode == Mode::Normal
    }

    /// While waiting on something (claude answer), redraw again after a `TICK` — wave animation, elapsed
    /// time. A toast needs frames only while it fades: between its fade-in and fade-out one wake-up is
    /// scheduled for when the fade-out starts. Nothing to wait for, no tick (an idle editor uses 0 CPU).
    /// When the notification card next needs a frame: a `TICK` while it fades in or out, else when its
    /// fade-out starts — None without one. One waiting for a card (`Toast::waiting`) needs none.
    pub(crate) fn toast_wake(&self) -> Option<std::time::Duration> {
        // (focused, it's held: no time runs — let go, one frame as its fade ends)
        if self.toast_focus.is_some() {
            return None;
        }
        if let Some(at) = self.toast_letgo.filter(|_| !self.toasts.is_empty()) {
            let left = crate::toast::LETGO.saturating_sub(crate::clock::since(at));
            return Some(left + std::time::Duration::from_millis(2));
        }
        let running = || self.toasts.iter().filter(|t| !t.waiting());
        let oldest = running().map(Toast::age).max()?;
        let left = running().map(|t| t.life.saturating_sub(t.age())).max();
        // (nothing moving on a clock: one frame as its time is up, none to fade)
        if !self.moving() {
            return Some(left.unwrap_or_default() + std::time::Duration::from_millis(2));
        }
        let steady = left.unwrap_or_default().saturating_sub(TOAST_FADE);
        let fading_in = oldest < TOAST_FADE_IN;
        Some(if fading_in || steady.is_zero() { TICK } else { steady })
    }

    /// When a frame is next wanted with nothing else happening: the waiting animation's next step while
    /// something is waited on, else a toast's (`toast_wake`) — None: no frame. Not the animation while the
    /// terminal says it's unfocused (`unfocused`): nobody sees it, and focus coming back draws it again.
    pub(crate) fn tick_wanted(&self) -> Option<std::time::Duration> {
        // (a task's or tests' wave and wait counter are in the bottom slot — its panel and tabs — only: no
        // frames for them while the last frame didn't draw it; one that does asks again, `frame_drawn`)
        let slot_drawn = self.screen.bottom_h > 0 || self.screen.width.is_none();
        let run =
            self.session.test_run.as_ref().is_some_and(|r| r.state == crate::testing::RunState::Running)
                || self.session.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running);
        let waiting = self.session.review.as_ref().is_some_and(|r| !r.ready)
            || self.session.chat.as_ref().is_some_and(|c| c.busy())
            || self.dap.as_ref().is_some_and(|d| d.state == crate::dap::State::Building)
            || (run && slot_drawn)
            || crate::web::page::reading_in_sight(self)
            || self.offer_busy();
        // (the wave held still where nothing moves on a clock: a frame a second, for the time waited — its timers
        // say whole seconds there, `term::running`)
        let next = if waiting && !self.unfocused {
            let elapsed = anim_clock().elapsed();
            Some(match self.moving() {
                true => step_in(elapsed),
                false => {
                    std::time::Duration::from_nanos(1_000_000_000 - u64::from(elapsed.subsec_nanos()))
                        + std::time::Duration::from_millis(2)
                }
            })
        } else {
            self.toast_wake()
        };
        // The reviewer reading, its time on the review rail drawn last frame: a frame as each tenth turns under a
        // minute, each second after — as `term::running` writes it; where nothing moves on a clock it writes whole
        // seconds, a frame each (none while it waits in the line: no time runs)
        let reviewer = self
            .session
            .reviewer
            .as_ref()
            .filter(|rv| {
                rv.reading() && !crate::reviewer::waits(self, &self.session) && self.screen.rail_w > 0
            })
            // (its time is drawn on the rail only — the status line's segment has none: an idle tarae draws nothing)
            .filter(|_| self.session.code_review.is_some())
            .map(|rv| {
                let read = crate::clock::since(rv.started);
                let step: u32 =
                    if read.as_secs() < 60 && self.moving() { 100_000_000 } else { 1_000_000_000 };
                std::time::Duration::from_nanos(u64::from(step - read.subsec_nanos() % step))
                    + std::time::Duration::from_millis(2)
            })
            .filter(|_| !self.unfocused);
        let next = [next, reviewer].into_iter().flatten().min();
        // An element flashing (a link followed): one frame when its flash is over, to take it away
        let flash = self.ui_flash.as_ref().and_then(|(_, at)| {
            let left = crate::uistate::claude::FLASH.checked_sub(at.elapsed())?;
            Some(left + std::time::Duration::from_millis(2))
        });
        // A run of Claude's going silent, in any session: one frame then — the inbox counts it (#157); a shuttle's
        // step lit on its chip: one frame when it goes faint (`shuttle::LIT`)
        // The terminal's view at a new panel size: one frame once it has held (`view::terminal_resized`)
        let settle = crate::view::terminal_settling(self).then_some(crate::view::SETTLE);
        // A session's name tag on its clock: as its stay ends, then while it fades (`presence::tag_wake`)
        let tag = crate::presence::tag_wake(self);
        // Claude's writing shown at its pace: a frame each `REVEAL_EVERY` while there's text left to show
        let reveal = crate::writing::reveal_wake(self).filter(|_| !self.unfocused);
        // A motion on its own clock (`motion.rs`): a frame each step while it dries, one as it goes
        let motion = self.motion_wake();
        [next, flash, self.silent_wake(), crate::shuttle::lit_wake(self), settle, tag, reveal, motion]
            .into_iter()
            .flatten()
            .min()
    }

    /// Whether the main loop draws after a batch of events (`unchanged`: every one of them said it changed
    /// nothing on screen) — and even then when something came that no frame has shown yet (#200: a wrong
    /// "nothing changed" held the screen for 30 minutes — no card settled, no toast aged, `tarae state` timed
    /// out). Owed is a bug somewhere: the log says what was. Once a frame is drawn nothing is owed, shown or
    /// not (a card under a picker), so an idle tarae stays idle.
    pub fn frame_due(&self, unchanged: bool) -> bool {
        if !unchanged {
            return true;
        }
        let Some(what) = self.frame_owed() else { return false };
        let line =
            format!("{what} came after the last frame, and every event since said nothing changed — drawn");
        crate::log::line("frames", "owed", &line);
        #[cfg(test)]
        FRAMES_OWED.with(|o| o.borrow_mut().push(line));
        true
    }

    /// What came that no frame has shown yet: input (a key, a click, a paste — whatever it changed, you look
    /// for it), a card, a toast (since the last frame), a UI-state ask whose input has come (answered after
    /// the next frame — `uistate::after_frame`).
    pub(crate) fn frame_owed(&self) -> Option<String> {
        if self.input_seq > self.frame_input {
            return Some("input (a key, click or paste)".into());
        }
        let after = |t: std::time::Instant| self.frame_at.is_none_or(|f| t > f);
        if let Some(o) = self.offers.iter().find(|o| after(o.since)) {
            return Some(format!("card \"{}\"", o.title));
        }
        if let Some(t) = self.toasts.iter().find(|t| after(t.at)) {
            return Some(format!("toast \"{}\"", t.text.chars().take(80).collect::<String>()));
        }
        let now = std::time::Instant::now();
        self.ui_asks
            .iter()
            .any(|a| self.input_seq >= a.after_input && a.until > now)
            .then(|| "a UI-state ask".to_string())
    }

    /// The job applied now names what it is (`DrawnFor::Named`: a tick, a watch round, a UI-state ask) — a frame
    /// it draws says that rather than where its result was sent from.
    pub(crate) fn drawn_for(&mut self, what: &'static str) {
        self.event_from = Some(DrawnFor::Named(what));
    }

    /// A frame went to the terminal (`term::event_loop`): its number and what drew it kept, the last
    /// `FRAMES_FOR_KEPT`; the reasons gathered for it go either way.
    pub(crate) fn frame_went(&mut self, written: bool) {
        let by = std::mem::take(&mut self.drawn_by);
        if !written {
            return;
        }
        if self.frames_for.len() == FRAMES_FOR_KEPT {
            self.frames_for.pop_front();
        }
        self.frames_for.push_back((self.frames_written, by));
    }

    pub(crate) fn schedule_tick(&mut self) {
        if self.motions_prune() {
            self.unchanged = false; // gone from the screen: drawn
        }
        if !self.toasts.is_empty() && !self.toasts.iter().any(Toast::alive) {
            self.toasts.clear();
            self.unchanged = false; // gone from the screen — whatever event this is, it's drawn
        }
        if self.toasts.is_empty() {
            (self.toast_letgo, self.toast_focus) = (None, None);
        }
        self.tick_ask();
    }

    /// After a frame: the frames what it drew wants — a slot it brought back into sight (`C-w z` back, a
    /// panel shown) with a run's wave in it was judged by the frame before (`tick_wanted`).
    pub fn frame_drawn(&mut self) {
        self.tick_ask();
    }

    /// A wake-up for the next frame `tick_wanted` asks for, unless one due by then is set already.
    fn tick_ask(&mut self) {
        use std::time::Duration;
        let Some(delay) = self.tick_wanted() else { return };
        let at = crate::clock::now() + delay;
        // One already due by then serves (a later one doesn't: a new toast must start fading in now)
        if self.ticking.is_some_and(|t| t <= at + Duration::from_millis(5)) || cfg!(test) {
            return;
        }
        self.ticking = Some(at);
        self.events.jobs().spawn(move || {
            crate::event::sleep(delay);
            move |ed: &mut Editor| ed.tick_landed(at)
        });
    }

    /// When the animation's next wake-up `tick_ask` armed lands (None: none armed) — the resting redraw waits for
    /// one due before long (`term::Resting`).
    pub(crate) fn tick_due(&self) -> Option<std::time::Instant> {
        self.ticking
    }

    /// The wake-up `tick_ask` set for `at`, come: the frame it was for — nothing to draw when an earlier one
    /// overtook it (a new toast, an error let go), or when it was asked while focused and lands after the
    /// terminal said it's unfocused with nothing else on a clock (only the resting animation's step: CI's macOS
    /// drew it after `CSI O`).
    pub(crate) fn tick_landed(&mut self, at: std::time::Instant) {
        self.drawn_for("a tick");
        if self.ticking != Some(at) {
            self.unchanged = true;
            return;
        }
        self.ticking = None;
        if self.unfocused && self.tick_wanted().is_none() {
            self.unchanged = true;
        }
    }

    // ── Pickers ──────────────────────────────────────────────────────────

    /// Opens a picker; if `fill` is given, fills the list on a worker thread.
    /// The prompt token of the picker open now, if it's a plugin's pick.
    fn plugin_pick_token(&self) -> Option<u64> {
        match self.picker.as_ref()?.items().first().map(|i| &i.action) {
            Some(Action::Plugin { token, .. }) => Some(*token),
            _ => None,
        }
    }

    pub fn open_picker(&mut self, mut picker: Picker, fill: Option<crate::picker::Fill>) {
        // A plugin's pick, replaced: it hears nothing was chosen
        if let Some(token) = self.plugin_pick_token() {
            self.picker = None;
            self.plugin_prompt_done(token, None);
        }
        self.picker_ids += 1;
        let id = self.picker_ids;
        picker.id = id;
        self.count(crate::features::Area::Picker, picker.kind);
        // (tests wait for particular events — no filtering ones)
        picker.asynchronous = !cfg!(test);
        if let Some(fill) = fill {
            picker.loading = true;
            let browse = picker.browse_root();
            self.events.jobs().spawn(move || {
                let result = fill().map(|items| {
                    let tree = browse.map(|root| crate::picker::browse_tree(&root, &items));
                    (items, tree)
                });
                move |ed: &mut Editor| {
                    let Some(p) = ed.picker.as_mut().filter(|p| p.id == id) else { return };
                    match result {
                        Ok((items, tree)) => p.set_items_with(items, tree),
                        Err(crate::picker::FillError::Nothing(e)) => {
                            ed.picker = None;
                            ed.set_status(e);
                        }
                        Err(crate::picker::FillError::Failed(e)) => {
                            ed.picker = None;
                            ed.set_error(e);
                        }
                    }
                }
            });
        }
        self.picker = Some(picker);
    }

    /// A closing picker is kept for `space '` — not code actions (their list goes stale with the text).
    pub(crate) fn keep_last_picker(&mut self) {
        // (a plugin's pick isn't kept: it's been answered)
        if self.plugin_pick_token().is_some() {
            self.picker = None;
            return;
        }
        if let Some(mut p) = self.picker.take()
            && !p.items().iter().any(|i| matches!(i.action, Action::Code(_)))
        {
            // (a worker's filtering would land with no picker to take it)
            p.filter_now();
            // (its previews are parsed files — up to `PREVIEW_CACHE` of them, read again when it reopens)
            p.previews.clear();
            self.last_picker = Some(p);
        }
    }

    /// `space '` — the last picker again, as it was left (query, selection).
    pub fn reopen_last_picker(&mut self) {
        match self.last_picker.take() {
            Some(p) => self.open_picker(p, None),
            None => self.set_status("no picker to reopen yet"),
        }
    }

    /// `space /` — the project searched as you type (N4): a regex with smart case, its results rows of code
    /// with the match standing out (`pattern`: what's typed already).
    pub(crate) fn global_search(&mut self, pattern: String) {
        let root = self.search_root();
        let mut picker = Picker::of("search", "search", Vec::new(), false).grouped(root);
        picker.live = true;
        picker.query = pattern;
        self.open_picker(picker, None);
        self.live_search();
    }

    /// Where `space /` searches: the project (tarae's folder; a test's own).
    fn search_root(&self) -> PathBuf {
        self.session.root.clone()
    }

    /// The live search's query changed (a key): the search under way stops, and after a pause in the typing
    /// (`LIVE_PAUSE`) the new one runs on a worker — its first results shown soon, the rest as they come;
    /// the last results stay until then. A pattern not finished yet (`(` open) keeps them, said quietly.
    pub(crate) fn live_search(&mut self) {
        use std::sync::atomic::Ordering;
        let Some(p) = self.picker.as_mut().filter(|p| p.live) else { return };
        let generation = self.live_search.fetch_add(1, Ordering::Relaxed) + 1;
        let pattern = p.query.clone();
        p.note = None;
        if pattern.is_empty() {
            p.set_items(Vec::new());
            (p.grep, p.emphasis, p.searching) = (None, None, false);
            return;
        }
        let re = match crate::picker::smart_regex(&pattern) {
            Ok(re) => re,
            Err(why) => {
                p.searching = false;
                // (the reason; term says the results are the last ones as the room allows)
                p.note = Some(why);
                return;
            }
        };
        p.searching = true;
        let id = p.id;
        let root = self.search_root();
        let now = self.live_search.clone();
        let tx = self.events.sender();
        self.events.jobs().spawn(move || {
            std::thread::sleep(LIVE_PAUSE);
            let stop = || now.load(Ordering::Relaxed) != generation;
            let found = if stop() {
                None
            } else {
                let mut partial = |items: Vec<crate::picker::Item>| {
                    let (re, pattern) = (re.clone(), pattern.clone());
                    let show = move |ed: &mut Editor| {
                        if ed.live_search.load(Ordering::Relaxed) == generation
                            && let Some(p) = ed.picker.as_mut().filter(|p| p.id == id)
                        {
                            p.set_found(items);
                            (p.grep, p.emphasis) = (Some(pattern), Some(re));
                        }
                    };
                    let _ = tx.send(crate::event::Event::Job(Box::new(show)));
                };
                Some(crate::picker::search(&root, &pattern, &stop, &mut partial))
            };
            move |ed: &mut Editor| {
                if ed.live_search.load(Ordering::Relaxed) != generation {
                    return;
                }
                let Some(p) = ed.picker.as_mut().filter(|p| p.id == id) else { return };
                p.searching = false;
                match found {
                    Some(Ok(items)) => {
                        p.set_found(items);
                        (p.grep, p.emphasis) = (Some(pattern), Some(re));
                    }
                    Some(Err(why)) => p.note = Some(why),
                    None => {}
                }
            }
        });
    }

    /// Every event: a filtering the picker asked for runs on a worker; its result lands if no later key
    /// asked for another.
    fn picker_filter(&mut self) {
        let Some(p) = self.picker.as_mut() else { return };
        let Some(job) = p.take_filter_job() else { return };
        let id = p.id;
        self.events.jobs().spawn(move || {
            let found = job.run();
            move |ed: &mut Editor| {
                if let Some(p) = ed.picker.as_mut().filter(|p| p.id == id) {
                    p.filtered(found);
                }
            }
        });
    }

    /// `C-x` in the buffer list: buffer `id` closes and leaves the list. One with unsaved changes is kept
    /// the first time, said so — `C-x` again on it closes it without saving.
    fn picker_close_buffer(&mut self, id: DocId) {
        let Some(i) = self.docs.iter().position(|d| d.id == id) else { return };
        // (under review: the card asks — the list makes way for it)
        if self.session.review.as_ref().is_some_and(|r| r.doc_id == id) {
            self.keep_last_picker();
            self.close_asks_review(id, false);
            return;
        }
        // (a `:w` a moment ago may still be being written — it counts)
        if self.docs[i].saving > 0 {
            self.flush_saves();
        }
        let d = &self.docs[i];
        let name = d.display_name(&self.session.root);
        let unsaved = d.is_modified() && !d.throwaway;
        let Some(p) = self.picker.as_mut() else { return };
        if unsaved && p.closing != Some(id) {
            p.closing = Some(id);
            return self
                .set_warning(format!("{name} has unsaved changes — C-x again closes it without saving"));
        }
        p.closing = None;
        let (at, rest): (usize, Vec<crate::picker::Item>) = (
            p.selected,
            p.items().iter().filter(|i| !matches!(i.action, Action::Buffer(b) if b == id)).cloned().collect(),
        );
        p.set_items(rest);
        p.selected = at.min(p.len().saturating_sub(1));
        self.close_doc(id);
        self.note(format!("closed {name}"));
    }

    /// `C-x` in the notes list: the note on `item`'s row closes · `all` (`A-x`): every note its author began.
    /// The list follows, the row in hand where it was — or goes (kept for `space '`) when a card asks first
    /// (your unsent drafts among them): the card takes the keys.
    fn picker_close_notes(&mut self, item: &crate::picker::Item, all: bool) {
        let Some(id) = crate::notes::of_row(self, item) else { return };
        match crate::notes::began(self, id).filter(|_| all) {
            Some(by) if crate::notes::close_by(self, Some(by)) => return self.keep_last_picker(),
            Some(_) => {}
            None => crate::notes::close(self, id),
        }
        let title = self.picker.as_ref().map(|p| p.title.clone()).unwrap_or_default();
        // (the stale card's list: its card follows first — `notes::stale_sync` — then its rows)
        crate::notes::stale_sync(self);
        let rest = crate::notes::list_items(self, &title);
        if let Some(p) = self.picker.as_mut() {
            let at = p.selected;
            p.set_items(rest);
            p.selected = at.min(p.len().saturating_sub(1));
        }
    }

    fn picker_key(&mut self, key: Key) {
        let Some(p) = self.picker.as_mut() else { return };
        // Anything but typing acts on the list — never on the last query's rows
        if !matches!((key.code, key.ctrl, key.alt), (Code::Char(_), false, false) | (Code::Backspace, ..)) {
            p.filter_now();
        }
        // (the list's rows in sight, last frame)
        let rows = match p.view.len() {
            0 => crate::key::UNDRAWN_ROWS,
            n => n,
        };
        // (`q` closes a list whose letters are keys — the session list, `:granted` — as every list panel's does;
        // elsewhere it's typed)
        let q_closes = key.code == Code::Char('q') && !key.alt && p.keys;
        match (key.code, key.ctrl) {
            (Code::Esc, _) | (Code::Char('c'), true) | (Code::Char('q'), false)
                if key.code != Code::Char('q') || q_closes =>
            {
                // A plugin's pick: it hears nothing was chosen
                if let Some(token) = self.plugin_pick_token() {
                    self.picker = None;
                    return self.plugin_prompt_done(token, None);
                }
                self.keep_last_picker();
                if let Some(t) = self.theme_before.take() {
                    self.theme = t;
                }
                return;
            }
            // File tree: Enter on a folder opens/folds it, → opens, ← folds or goes up
            (Code::Enter, _) if p.current().is_some_and(|i| matches!(i.action, Action::Dir(_))) => {
                return p.tree_toggle();
            }
            (Code::Right, _) if p.in_tree() => return p.tree_right(),
            (Code::Left, _) if p.in_tree() => return p.tree_left(),
            // Helix's picker keys (uxaudit H1): a place opened in a split — `C-s` under, `C-v` beside — or with
            // the list kept up (`A-ret`); `C-t` the preview away and back; Home/End the first row and the last
            (Code::Char('s'), true) if !key.alt => {
                return self.picker_open_place(key, Some(crate::split::Dir::Horizontal));
            }
            (Code::Char('v'), true) if !key.alt => {
                return self.picker_open_place(key, Some(crate::split::Dir::Vertical));
            }
            (Code::Enter, false) if key.alt => return self.picker_open_place(key, None),
            // (where none is drawn either way, said: a list with no preview, a screen too narrow for one)
            (Code::Char('t'), true) => {
                let room = self.screen.picker_preview_x.is_some() || p.preview_hidden;
                return match (p.preview, room) {
                    (false, _) => self.key_unused(key, "this list", "it has no preview"),
                    (true, false) => self.set_status("no room for the preview — the screen is too narrow"),
                    (true, true) => p.preview_hidden = !p.preview_hidden,
                };
            }
            (Code::Home | Code::End, _) => {
                p.preview_scroll = 0;
                let row = if key.code == Code::Home { 0 } else { p.len().saturating_sub(1) };
                p.select_near(row);
            }
            // Places: keep the listed ones as the list (`]q`)
            (Code::Char('q'), true) => {
                self.qlist_from_picker();
                return;
            }
            // `:granted`: its letters act on the row, a digit picks one (nothing is typed)
            (Code::Char(c), false) if p.keys && !key.alt => return self.granted_key(c),
            (Code::Backspace, _) if p.keys => return,
            // Global search results: replace what's listed
            (Code::Char('r'), true) if p.grep.is_some() => return self.replace_ask(),
            // A branch: rename it (the name to change, in a `:` line) · delete it
            (Code::Char('r' | 'x'), true)
                if matches!(p.current().map(|i| &i.action), Some(Action::Branch { .. })) =>
            {
                let Some(Action::Branch { name, .. }) = p.current().map(|i| i.action.clone()) else { return };
                self.keep_last_picker();
                return match key.code {
                    Code::Char('r') => {
                        self.open_prompt(PromptKind::Command, &format!("git-branch-rename {name} {name}"))
                    }
                    _ => self.git_branch_delete(&name, true),
                };
            }
            // A note: `C-x` closes it, `A-x` every note its author began (#197) — in the stale card's list
            // (`notes::STALE`) `C-x` only: the author's others aren't on it
            (Code::Char('x'), ctrl)
                if (ctrl && crate::notes::is_list(&p.title))
                    || (key.alt && p.title == crate::notes::NOTES) =>
            {
                let Some(item) = p.current().cloned() else { return };
                return self.picker_close_notes(&item, !ctrl);
            }
            // A buffer: close it (#47)
            (Code::Char('x'), true) if matches!(p.current().map(|i| &i.action), Some(Action::Buffer(_))) => {
                let Some(Action::Buffer(id)) = p.current().map(|i| i.action.clone()) else { return };
                return self.picker_close_buffer(id);
            }
            // Replace preview: Enter applies to every file still listed
            (Code::Enter, _) if p.current().is_some_and(|i| matches!(i.action, Action::ReplaceFile(_))) => {
                let files: Vec<usize> = p
                    .shown()
                    .filter_map(|i| match i.action {
                        Action::ReplaceFile(n) => Some(n),
                        _ => None,
                    })
                    .collect();
                self.picker = None;
                return self.replace_apply(&files);
            }
            (Code::Enter, _) => {
                let action = p.current().map(|i| i.action.clone());
                // (a command chosen in the palette: counted as the palette's — a `:` one as typed — and first
                // in it next time)
                if p.kind == "commands" {
                    match &action {
                        Some(Action::Command(name)) => self.count(crate::features::Area::Palette, name),
                        Some(Action::Typed(line)) => self.count_typed(line),
                        _ => {}
                    }
                    if let Some(k) = action.as_ref().and_then(crate::palette::key) {
                        self.palette_ran = crate::palette::bump(&self.palette_ran, &k);
                        if !cfg!(test) {
                            let ran = self.palette_ran.clone();
                            self.events.jobs().background(move || crate::palette::save(&ran));
                        }
                    }
                }
                self.keep_last_picker();
                self.theme_before = None;
                if let Some(a) = action {
                    self.run_picker_action(a);
                }
                return;
            }
            (Code::Down | Code::Tab, _) | (Code::Char('n'), true) => p.move_by(1),
            (Code::Up | Code::BackTab, _) | (Code::Char('p'), true) => p.move_by(-1),
            // (half the rows in sight, as everywhere — the page keys all of them)
            (Code::Char('d'), true) => p.move_by(crate::key::half(rows)),
            (Code::Char('u'), true) => p.move_by(-crate::key::half(rows)),
            (Code::PageDown, _) => p.move_by(rows as isize),
            (Code::PageUp, _) => p.move_by(-(rows as isize)),
            // The preview pane
            (Code::Char('f'), true) => p.preview_scroll += 10,
            (Code::Char('b'), true) => p.preview_scroll = p.preview_scroll.saturating_sub(10),
            (Code::Backspace, _) => p.pop(),
            (Code::Char(c), false) if !key.alt => p.push(c),
            _ => {}
        }
        if matches!(key.code, Code::Backspace | Code::Char(_)) && !key.ctrl {
            self.workspace_symbols_requery();
            self.live_search();
        }
        self.preview_theme();
        self.code_action_preview();
    }

    /// The picker's row opened in a split (`C-s` under, `C-v` beside — the list closes) or with the list kept up
    /// (`A-ret`: None), as Helix's pickers do (uxaudit H1) — a place only (`Action::is_place`); another row says
    /// so.
    fn picker_open_place(&mut self, key: Key, split: Option<crate::split::Dir>) {
        let Some(action) = self.picker.as_ref().and_then(|p| p.current()).map(|i| i.action.clone()) else {
            return;
        };
        if !action.is_place() {
            return self.key_unused(key, "this list", "its rows aren't places to open; enter picks one");
        }
        let made = split.map(|dir| {
            self.keep_last_picker();
            self.theme_before = None;
            self.split_view(dir);
        });
        if let Err(e) = self.picker_action(action) {
            // (the place didn't open: the pane made for it goes — no split nobody asked for)
            if made.is_some() {
                self.close_view();
            }
            self.set_error(format!("{e:#}"));
        }
    }

    /// Runs one command by name (`:theme` → theme picker, etc.).
    pub fn run_command_by_name(&mut self, name: &str) {
        if let Some(c) = commands::find(name) {
            self.run(&[MappableCommand::Static(c)], None);
        }
    }

    /// Theme picker: applies the highlighted theme right away (remembering the original theme).
    pub fn preview_theme(&mut self) {
        let Some(name) = self.picker.as_ref().and_then(|p| p.current()).and_then(|i| match &i.action {
            Action::Theme(n) => Some(n.clone()),
            _ => None,
        }) else {
            return;
        };
        if self.theme.name == name {
            return;
        }
        // User themes are files — read on a worker thread; apply only if that row is still highlighted
        self.events.jobs().spawn(move || {
            let result = crate::theme::load(&name);
            move |ed: &mut Editor| {
                let row = ed.picker.as_ref().and_then(|p| p.current()).map(|i| &i.action);
                if let (Some(Action::Theme(n)), Ok(t)) = (row, result)
                    && *n == name
                {
                    let before = std::mem::replace(&mut ed.theme, t);
                    ed.theme_before.get_or_insert(before);
                }
            }
        });
    }

    pub(crate) fn run_picker_action(&mut self, action: Action) {
        if let Err(e) = self.picker_action(action) {
            self.set_error(format!("{e:#}"));
        }
    }

    /// A picker's row acted on — what went wrong, for the caller to say (`run_picker_action`) or undo first
    /// (`picker_open_place`: the split made for it).
    fn picker_action(&mut self, action: Action) -> anyhow::Result<()> {
        match action {
            Action::Code(i) => {
                self.run_code_action(i);
                Ok(())
            }
            Action::ReplaceFile(n) => {
                self.replace_apply(&[n]);
                Ok(())
            }
            // (folders open in the picker: picker_key; `:granted`'s rows act by their keys)
            Action::Dir(_) | Action::Heading | Action::Granted(_) => Ok(()),
            #[cfg(test)]
            Action::Message => Ok(()),
            Action::Plugin { token, id } => {
                self.plugin_prompt_done(token, Some(id));
                Ok(())
            }
            Action::Conversation { id, .. } => {
                crate::chat::resume(self, &id);
                Ok(())
            }
            Action::Commit { hash, root } => {
                self.git_show(root, &hash);
                Ok(())
            }
            // (one checked out in another worktree: refused there, naming it)
            Action::Branch { name, .. } => {
                self.git_switch(&name);
                Ok(())
            }
            Action::Inbox(id) => {
                self.inbox_go(&id);
                Ok(())
            }
            // Open it at its first change
            Action::ChangedFile(n) => match self.session.changed_files.get(n).cloned() {
                Some(c) => self.picker_action(Action::Goto { path: c.path, line: c.line, col: 0 }),
                None => Err(anyhow::anyhow!("that file isn't among the changed ones any more")),
            },
            Action::Jump { index, .. } | Action::JumpParked { index, .. } => {
                self.jump_to_entry(index);
                Ok(())
            }
            Action::Buffer(id) => match self.docs.iter().position(|d| d.id == id) {
                Some(i) => {
                    self.current = i;
                    Ok(())
                }
                None => Err(anyhow::anyhow!("that buffer is closed")),
            },
            Action::Open(path) => self.open(&path),
            Action::Command(name) => {
                self.run_command_by_name(name);
                Ok(())
            }
            Action::Typed(line) => typed::execute(self, &line).map_err(|e| anyhow::anyhow!(e)),
            Action::Grant(row) => self.sandbox_grant(&row).map_err(|e| anyhow::anyhow!(e)),
            Action::Theme(name) => typed::execute(self, &format!("set! theme {name}"))
                .map_err(|e| anyhow::anyhow!(e))
                .map(|_| {
                    self.set_success(format!("Theme {name} · saved to your config"));
                }),
            Action::Goto { path, line, col } => self.open_at(&path, line, col),
            Action::Note { path, line, .. } => self.open_at(&path, line, 0),
            Action::Line { doc, line, col } => {
                let Some(i) = self.docs.iter().position(|d| d.id == doc) else {
                    return Err(anyhow::anyhow!("that buffer is closed"));
                };
                self.push_jump();
                self.current = i;
                let d = self.doc_mut();
                let start =
                    crate::movement::line_start(&d.text, line.min(crate::movement::last_line(&d.text)));
                let at = crate::graphemes::prev_boundary(&d.text, (start + col + 1).min(d.text.len_bytes()));
                d.set_selection(crate::selection::Selection::point(at.max(start)));
                Ok(())
            }
        }
    }

    // ── Syntax, themes (slow work all on worker threads) ──────────────────

    /// Picks a language by path, loads its grammar in the background and attaches it.
    pub fn attach_syntax(&mut self, id: DocId) {
        let Some(doc) = self.docs.iter().find(|d| d.id == id) else { return };
        let Some(spec) = doc.path.as_deref().and_then(syntax::detect) else { return };
        self.load_language(id, spec);
    }

    pub fn load_language(&mut self, id: DocId, spec: &'static syntax::LangSpec) {
        self.events.jobs().spawn(move || {
            let result = syntax::Loader::global().load(spec);
            move |ed: &mut Editor| {
                let Some(doc) = ed.docs.iter_mut().find(|d| d.id == id) else { return };
                match result {
                    Ok(lang) => doc.syntax = Some(syntax::Syntax::new(lang)),
                    // (only Claude's turn or a review opened it: offered once the user takes it up — #208)
                    Err(e) if e.starts_with("no grammar") && (doc.claudes_only || doc.review_only) => {
                        doc.grammar_held = Some(spec)
                    }
                    // Grammar not downloaded yet — offer to download it (once per language until quit)
                    Err(e) if e.starts_with("no grammar") => ed.offer_grammar(spec),
                    Err(e) => ed.set_status(format!("no highlighting: {e}")),
                }
            }
        });
    }

    /// `:grammar-install [lang|all]` — downloads and builds grammars (worker thread; when done, open
    /// documents get colored).
    /// Without a language, the current file's language.
    pub fn grammar_install(&mut self, arg: &str) -> Result<(), String> {
        let names: Vec<String> = match arg.trim() {
            "all" => crate::grammar::sources()
                .into_iter()
                .filter(|s| !crate::grammar::bundled(s))
                .map(|s| s.name)
                .collect(),
            "" => {
                let lang = self.doc().path.as_deref().and_then(syntax::detect).map(|s| s.name.clone());
                lang.map(|l| crate::grammar::for_language(&l)).unwrap_or_default()
            }
            some => some.split_whitespace().flat_map(crate::grammar::for_language).collect(),
        };
        if names.is_empty() {
            return Err("which language? :grammar-install rust (or all)".into());
        }
        let n = names.len();
        self.set_status(if n == 1 {
            format!("Fetching and building the {} grammar…", names[0])
        } else {
            format!("Fetching and building {n} grammars…")
        });
        self.install_grammars(names, None);
        Ok(())
    }

    /// Downloads and builds grammars (worker thread). `offer` = the card number if started from an offer
    /// card (results go to that card).
    pub fn install_grammars(&mut self, names: Vec<String>, offer: Option<u64>) {
        self.events.jobs().spawn(move || {
            let results = std::sync::Mutex::new(Vec::new());
            let r = crate::grammar::install(&names, false, |p| results.lock().unwrap().push(p));
            let results = results.into_inner().unwrap();
            move |ed: &mut Editor| {
                use crate::grammar::Progress;
                let mut resume = None;
                let mut failed: Vec<String> = results
                    .iter()
                    .filter_map(|p| match p {
                        Progress::Failed { name, why } => Some(format!("{name}: {why}")),
                        _ => None,
                    })
                    .collect();
                let whole_ok = r.is_ok();
                if let Err(e) = r {
                    failed.push(e);
                }
                let built = results.iter().filter(|p| matches!(p, Progress::Done { .. })).count();
                if offer.is_some() {
                    // If every reason is the same (no network, etc.), report it once
                    let whys: Vec<&str> = results
                        .iter()
                        .filter_map(|p| match p {
                            Progress::Failed { why, .. } => Some(why.as_str()),
                            _ => None,
                        })
                        .collect();
                    let reason = match whys.first() {
                        Some(w) if whole_ok && whys.iter().all(|x| x == w) => w.to_string(),
                        _ => failed.join("; "),
                    };
                    let what = offer.and_then(|id| ed.offer_by_id(id)).map(|o| o.what.clone());
                    if let (Some(crate::offer::What::Grammars { lang, then, .. }), Some(id)) = (what, offer) {
                        let ok = failed.is_empty();
                        ed.offer_finished(id, if ok { Ok(()) } else { Err(reason) });
                        if ok {
                            ed.set_success(format!("{lang} colors are on"));
                            resume = then;
                        }
                    }
                } else if failed.is_empty() {
                    ed.set_success(match built {
                        0 => "Grammars already up to date".to_string(),
                        1 => "Grammar installed — colors are on".to_string(),
                        n => format!("{n} grammars installed — colors are on"),
                    });
                } else {
                    ed.set_error(format!(
                        "grammar install failed — {} (needs git and a C compiler)",
                        failed.join("; ")
                    ));
                }
                syntax::Loader::global().forget_missing();
                let ids: Vec<DocId> = ed.docs.iter().filter(|d| d.syntax.is_none()).map(|d| d.id).collect();
                for id in ids {
                    ed.attach_syntax(id);
                }
                // Work that stopped for lack of a grammar (tests, debugging) — continue it now
                if let Some(r) = resume {
                    ed.resume(r);
                }
            }
        });
    }

    /// Which documents keep their parse trees (#36 — a big file's are ~15 MB, and a day's work keeps a dozen
    /// open): those on screen now and the `TREES_RECENT` seen most recently; any other seen less than
    /// `trees_unseen()` ago. The rest let theirs go (`Syntax::sleep`) and are parsed again when shown — a
    /// frame uncolored. One never shown yet (reopened at startup) isn't parsed until it is. Every event,
    /// at `now`; an idle tarae isn't woken for it — the trees go at the next event.
    pub fn trees_follow_sight(&mut self, now: std::time::Instant) {
        let current = self.doc().id;
        let shown = |id: DocId, views: &[View]| id == current || views.iter().any(|v| v.doc == id);
        // (a review's base parse goes and comes back with the trees: shown without it, it's asked again)
        let mut bases = Vec::new();
        for d in &mut self.docs {
            if shown(d.id, &self.session.views) {
                d.seen = Some(now);
                if let Some(s) = d.syntax.as_mut() {
                    s.dormant = false;
                }
                if d.review_base.is_some() && d.base_syntax.is_none() && !d.base_parsing {
                    bases.push(d.id);
                }
            }
        }
        // (the most recent few keep theirs however long ago — a handful of files you go between)
        let mut recent: Vec<(std::time::Instant, DocId)> =
            self.docs.iter().filter_map(|d| Some((d.seen?, d.id))).collect();
        crate::sorting::by_key(&mut recent, |r| std::cmp::Reverse(r.0));
        recent.truncate(TREES_RECENT);
        let unseen = trees_unseen();
        for d in &mut self.docs {
            let keep = match d.seen {
                None => false,
                Some(t) => recent.iter().any(|r| r.1 == d.id) || now.saturating_duration_since(t) < unseen,
            };
            if let Some(s) = d.syntax.as_mut().filter(|s| !keep && !s.dormant) {
                s.sleep();
                if let Some(base) = d.base_syntax.take() {
                    crate::event::drop_later(base);
                }
            }
        }
        for id in bases {
            self.base_parse(id);
        }
    }

    /// Starts a background parse for each edited document not already parsing (at the end of every event) —
    /// not those whose trees went (`trees_follow_sight`), until they're shown.
    pub fn schedule_parses(&mut self) {
        let now = std::time::Instant::now();
        self.trees_follow_sight(now);
        self.servers_follow_sight(now);
        for doc in &mut self.docs {
            let Some(syn) = doc.syntax.as_mut() else { continue };
            if doc.loading || !syn.dirty || syn.in_flight || syn.dormant {
                continue;
            }
            let job = syn.start_parse(&doc.text);
            let id = doc.id;
            self.events.jobs().spawn(move || {
                let generation = job.generation;
                let tree = job.run();
                move |ed: &mut Editor| {
                    if let Some(s) = ed.docs.iter_mut().find(|d| d.id == id).and_then(|d| d.syntax.as_mut()) {
                        s.finish_parse(generation, tree);
                    }
                }
            });
        }
    }

    /// If the configured theme name differs from the current theme, loads and swaps it in the background.
    pub fn ensure_theme(&mut self) {
        let name = self.config.theme.clone();
        if self.theme.name == name {
            return;
        }
        self.events.jobs().spawn(move || {
            let result = crate::theme::load(&name);
            move |ed: &mut Editor| match result {
                Ok(t) if ed.config.theme == t.name => {
                    if let Some(w) = t.warnings.first() {
                        let more = t.warnings.len() - 1;
                        ed.set_warning(if more > 0 { format!("{w} (+{more} more)") } else { w.clone() });
                    }
                    ed.theme = t;
                }
                Ok(_) => {}
                Err(e) => ed.set_error(e),
            }
        });
    }

    /// Replaces the config (already read and parsed by the watcher thread — no disk wait on the main thread).
    pub fn reload_config(&mut self, mut config: Config, warnings: Vec<String>) {
        // (read before a trust answer at its place given since, or while its write waits, about the same version:
        // that answer holds — N13; before a take-back there, any version: not trusted — F11)
        if let (Some(new), Some(t)) = (config.project.as_mut(), self.trust_path.as_deref()) {
            let (answered, taken) = crate::storewrite::since(t, &crate::trust::key(&new.place), new.read_at);
            let now = self.project_trust.as_ref();
            let same =
                now.filter(|now| new.file == now.file && new.hash == now.hash && new.state != now.state);
            let state = match same {
                Some(now) if answered => Some(now.state),
                _ if taken && new.state == crate::trust::State::Trusted => Some(crate::trust::State::Ask),
                _ => None,
            };
            if let Some(state) = state {
                new.state = state;
                let (editor, keymaps) =
                    if new.state == crate::trust::State::Trusted { &*new.with } else { &*new.without };
                (config.editor, config.keymaps) = (editor.clone(), keymaps.clone());
            }
        }
        self.keymaps = config.keymaps;
        self.config = config.editor;
        self.theme = config.theme;
        for (path, v) in &self.overrides {
            let _ = crate::settings::apply(&mut self.config, path, v);
        }
        self.project_trust_set(config.project);
        // (review at the end a chat had from what's no longer set goes with it)
        crate::edits::at_end_follow_config(self);
        self.ensure_theme();
        self.pending.clear();
        self.count = None;
        if warnings.is_empty() {
            self.set_status("config reloaded");
        } else {
            self.set_error(format!("config: {}", warnings.join("; ")));
        }
    }

    /// Whether the front card has the keys: focused (`card_focus` — P3 §3.8) as it says now, asking. One that came
    /// on its own waits in its faint frame until `C-q`, a click or `C-w w` focuses it, so no key meant for anything
    /// else is ever its answer — nothing guesses at what a key meant (the settle and the typing quiet went with it).
    pub fn card_has_keys(&self) -> bool {
        self.card_focus.is_some()
            && self.offers.first().filter(|o| !o.installing()).map(|o| (o.id, o.rev)) == self.card_focus
    }

    /// The front card waits to be focused: up, asking, without the keys — the status line's `▲1 waiting · C-q`, the
    /// state's `keys.waiting`.
    pub fn card_waits(&self) -> bool {
        !self.card_has_keys() && self.offers.first().is_some_and(|o| !o.installing())
    }

    /// `C-q` while a card is up — this session's first, then the one that has waited longest (`inbox.rs`) —
    /// focuses it from wherever the keys are (#24); while it has them, its answers answer it and Esc — or
    /// `C-q` again — gives them back unanswered (#25, #94), the card staying up, and the window prefix (`C-w`) goes
    /// on from where they were — other keys are said on it, what it asks. True if the key was the card's.
    fn card_key(&mut self, key: Key) -> bool {
        // (a sequence's first key: the floating pane a window prefix left is forgotten — `C-w w` reaches it again
        // after)
        if self.pending.is_empty() {
            self.float_left = None;
        }
        let up = self.offers.first().filter(|o| !o.installing()).map(|o| (o.id, o.rev));
        // (handed on to a card no longer in front as it was: nothing to hand)
        if self.card_next.is_some() && self.card_next != up {
            self.card_next = None;
        }
        let Some(id) = up.map(|u| u.0).filter(|_| self.picker.is_none() && self.prompt.is_none()) else {
            self.card_focus = None;
            return false;
        };
        let has = self.card_has_keys();
        // Handed the keys by an answer but not drawn yet: an answer of its typed on is refused, said on it (a `y y`
        // never approves a card never in sight); the rest go where the keys are
        if !has
            && self.card_next.is_some()
            && let Some(c) = key.plain_char()
            && self.offers.first().is_some_and(|o| o.acts_on(c))
        {
            self.offer_refused(format!("{key} came before this card was in sight — not taken"));
            return true;
        }
        if key.ctrl && !key.alt && key.code == Code::Char('q') {
            // Its text being read in its buffer: `C-q` gives the card the keys, never takes them (frictions15:
            // a toggle, it let go of a card that held them unseen — `j`/`k` read through it — and the `y`
            // after went to the code); Esc gives them back
            if self.offer_reading() {
                self.card_focus_front();
                self.pending.clear();
                self.set_status(match has {
                    true => "the card has the keys — esc gives them back",
                    false => "the card has the keys — its answers answer it, esc gives them back",
                });
                return true;
            }
            if has {
                self.card_focus = None;
                self.set_status(format!("keys back — the card waits, {} answers it", self.key_said("C-q")));
                return true;
            }
            // (another goes first — stacked under it here and older, or, nothing asking here, one in a
            // session behind: `C-q` goes there)
            if self.next_up() != crate::inbox::Next::Card(id) {
                return false;
            }
            self.card_focus_front();
            return true;
        }
        // (Esc never gives a card the keys, only back: handing them over too, Esc after Esc took turns — the
        // toggle `C-q` lost, on the key pressed out of habit)
        if !has {
            return false;
        }
        if key.code == Code::Esc {
            self.card_focus = None;
            self.pending.clear();
            return true;
        }
        // The window prefix: the keys back where they were, the prefix going on from there — a hop out is one key,
        // as from every pane (the card stays, waiting)
        if self.pending.is_empty() && self.keymaps.window_prefix() == Some(key) {
            self.card_focus = None;
            self.float_left = Some(crate::pane::Floating::Card(id));
            return false;
        }
        // Its whole text open in its buffer, the one in sight (its bar): the keys that read it move it — `ge`
        // to its end, as the bar asks (they were swallowed, and `y` waited for a read nobody could do)
        if self.offer_reading() && card_reads(key, &self.pending) {
            self.pending.push(key);
            self.dispatch_pending();
            return true;
        }
        self.pending.clear();
        let front = self.offers.first().map(|o| (o.id, o.yours));
        if !self.offer_key(key) {
            // (its answers are beside what it says — on the card, or its bar)
            // (short: a bar has room for little beside its title and answers; esc is on the card and
            // the status line says CARD)
            self.offer_refused(format!("{key} isn't an answer here"));
        }
        // Answered (or gone): the keys go to the next card up here when you were going through them (`C-q`'s round —
        // #101: card 2 of 3 waited for another `C-q`), or when it's yours too; one of yours answered, Claude's next
        // waits unfocused, the keys back where they were (P3 §3.8). Handed on once a frame has drawn it
        // (`card_next`): until then it waits as any card that came on its own
        if self.offers.first().map(|o| o.id) != front.map(|f| f.0) {
            let round = front.is_some_and(|f| !f.1);
            self.card_focus = None;
            self.card_next =
                self.offers.first().filter(|o| !o.installing() && (round || o.yours)).map(|o| (o.id, o.rev));
        }
        true
    }
}

/// Whether `key` (after `pending`) reads a card's text in its buffer: a line or a page up or down, its top
/// or its end (`g g`, `g e`, `G`).
fn card_reads(key: Key, pending: &[Key]) -> bool {
    let plain = !key.ctrl && !key.alt;
    match (pending, key.code) {
        ([], Code::Char('j' | 'k' | 'g' | 'G')) => plain,
        ([], Code::Down | Code::Up | Code::PageDown | Code::PageUp) => true,
        ([], Code::Char('d' | 'u' | 'f' | 'b')) => key.ctrl && !key.alt,
        ([g], Code::Char('e' | 'g')) => plain && g.plain_char() == Some('g'),
        _ => false,
    }
}

impl Editor {
    pub fn handle_key(&mut self, key: Key) {
        let begun = crate::document::input_begin();
        // (what it asks for — now, or through jobs it starts — was asked by this key, the count `input_came` is about
        // to take: a card it asks for is yours, focused as it comes — #205, P3 §3.8; a shuttle's isn't the person's)
        let asked = self.shuttles.stepping.is_none().then(|| self.events.set_asked(Some(self.input_seq + 1)));
        self.handle_key_now(key);
        if let Some(was) = asked {
            self.events.set_asked(was);
        }
        self.typed(begun);
    }

    /// A key or paste (`begun` by this call) is handled: what it edited is yours, but what said it wasn't
    /// (`Own::not_yours`) — in any document (a replace across files too).
    fn typed(&mut self, begun: bool) {
        if !begun {
            return;
        }
        for id in crate::document::input_end() {
            if let Some(d) = self.docs.iter_mut().find(|d| d.id == id) {
                d.own.input_end();
            }
        }
    }

    fn handle_key_now(&mut self, key: Key) {
        use crate::uistate::{Handler, took};
        self.input_came();
        // (each handler below that takes the key says so — the routing test holds `keys_to` to it)
        took(Handler::Code);
        self.session_store.keyed = true;
        // A key of the person's after an error has been read (`ERROR_MIN`) lets it go — it fades out (a shuttle's
        // key isn't someone reading it)
        // (a focused card's are held: its own keys let it go)
        let person = self.shuttles.stepping.is_none() && self.toast_focus.is_none();
        for t in self.toasts.iter_mut().filter(|t| person && t.kind == ToastKind::Error) {
            let age = t.age();
            if age >= ERROR_MIN && t.life > age + TOAST_FADE {
                t.life = age + TOAST_FADE;
            }
        }
        // (whoever took the keys since — a click, a job — has them)
        self.focus_sync();
        // Labels up for pointing at an element (`uistate/point.rs`): every key is theirs
        if crate::uistate::point::key(self, key) {
            return took(Handler::Point);
        }
        // A card is answered from anywhere (#24): `C-q` gives it the keys — what you were typing stays
        if self.card_key(key) {
            return took(Handler::Card);
        }
        // The notification card, focused (`toast.rs`): every key its but the window prefix
        if crate::toast::key(self, key) {
            took(Handler::Toast);
            return self.focus_sync();
        }
        // …else to the next up — nothing here, the one that has waited longest, wherever it is (#7, #90, #94,
        // #198) — the front's own edit review
        // takes its `C-q` itself
        if key.ctrl
            && !key.alt
            && key.code == Code::Char('q')
            && self.picker.is_none()
            && self.prompt.is_none()
            && matches!(
                self.next_up(),
                crate::inbox::Next::Go(_) | crate::inbox::Next::Done(_) | crate::inbox::Next::None
            )
        {
            took(Handler::NextCard);
            if !self.inbox_next() {
                // (one about to come — its call held back until the run before it ends: said, #5)
                match self.cards_coming().as_slice() {
                    [] => self.set_status(crate::inbox::NOTHING),
                    [one] => self.set_status(format!("no card yet — {one}'s comes once its run ends")),
                    many => self.set_status(format!("no card yet — {}'s come as runs end", many.join(", "))),
                }
            }
            return;
        }
        // The terminal has the keys: all of them go to its program (`C-\` gives them back)
        if self.picker.is_none() && self.prompt.is_none() && self.terminal_key(key) {
            took(Handler::Program);
            return self.focus_sync();
        }
        // If the completion list is up, intercept selection keys first (as in Helix: Tab/C-n down,
        // S-Tab/C-p up; Enter inserts only when something is selected — otherwise a plain newline).
        if self.mode == Mode::Insert
            && let Some(c) = self.completion.as_mut()
        {
            match (key.code, key.ctrl) {
                (Code::Tab | Code::Down, false) | (Code::Char('n'), true) => {
                    c.move_by(1);
                    return self.completion_docs();
                }
                (Code::BackTab | Code::Up, false) | (Code::Char('p'), true) => {
                    c.move_by(-1);
                    return self.completion_docs();
                }
                (Code::Enter, false) if c.selected.is_some() => return self.completion_accept(),
                (Code::Esc, _) => self.completion = None,
                _ => {}
            }
        }
        let was_insert = self.mode == Mode::Insert;
        self.insert_finished(0); // left insert mode some other way (mouse …)
        self.insert_key(key);
        self.handle_key_inner(key);
        self.insert_finished(self.last_trigger_len);
        self.focus_sync();
        self.track_doc_switch();
        if self.diag_card_hidden.is_some_and(|h| h != (self.doc().id, self.cursor_line())) {
            self.diag_card_hidden = None;
        }
        let typed = key.plain_char();
        // Replaying `.` — no completion or signature requests for the replayed keys
        if self.repeating_insert {
            return;
        }
        if was_insert || self.completion.is_some() {
            match key.code {
                Code::Char(_) if typed.is_some() => self.completion_after_edit(typed),
                Code::Backspace => self.completion_after_edit(None),
                _ => self.completion = None,
            }
        }
        if self.mode != Mode::Insert {
            self.signature = None;
        } else if was_insert {
            self.signature_after_key(typed);
        }
    }

    /// A key, click, wheel or paste is being handled: counted (`input_seq` — what drivers wait on) — and, unless a
    /// shuttle's step sent it, as the person's (`person_seq`): their reviews count the time before it, and a
    /// shuttle typing pauses (`shuttle::person_typed`).
    fn input_came(&mut self) {
        self.input_seq += 1;
        if self.shuttles.stepping.is_none() {
            self.person_seq += 1;
            self.review_input();
            crate::shuttle::person_typed(self);
        }
    }

    fn handle_key_inner(&mut self, key: Key) {
        use crate::uistate::{Handler, took};
        self.status = None;
        // Esc in normal mode = clear search highlight (and still does its other work) — the pane's own: a view
        // mode's esc clears its matches, the code's only the code's esc (not the chat's, a panel's)
        if key.code == Code::Esc && self.prompt.is_none() && crate::chatdoc::has_keys(self) {
            crate::chatdoc::search_unshown(self);
        } else if key.code == Code::Esc && self.prompt.is_none() && crate::view::has_keys(self).is_some() {
            crate::view::search_unshown(self);
        } else if key.code == Code::Esc && self.prompt.is_none() && crate::rundoc::has_keys(self).is_some() {
            crate::rundoc::search_unshown(self);
        } else if key.code == Code::Esc
            && self.mode == Mode::Normal
            && self.prompt.is_none()
            && self.session.is_at(At::Code)
        {
            self.search_hl = false;
            if self.popup.is_none() {
                self.diag_card_hidden = Some((self.doc().id, self.cursor_line()));
            }
        }
        // Floating text: C-d/C-u scroll it half its rows, C-e/C-y a row (as the code's), other keys close it (esc
        // only closes).
        if let Some(lines) = &self.popup
            && key.ctrl
            && !key.alt
        {
            let rows = self.screen.popup.get().map_or(16, |(_, _, _, h)| h);
            let by = match key.code {
                Code::Char('d') => Some(crate::key::half(rows)),
                Code::Char('u') => Some(-crate::key::half(rows)),
                Code::Char('e') => Some(1),
                Code::Char('y') => Some(-1),
                _ => None,
            };
            if let Some(by) = by {
                took(Handler::Popup);
                return self.popup_scroll = self.popup_scroll.saturating_add_signed(by).min(lines.len());
            }
        }
        self.popup_scroll = 0;
        if self.popup.take().is_some() && key.code == Code::Esc {
            return took(Handler::Popup);
        }
        // Keys a replayed macro feeds in aren't recorded — the `q` that replays them already was
        if self.replaying.is_empty()
            && !self.repeating_insert
            && let Some((_, keys)) = &mut self.recording
        {
            keys.push(key);
        }
        if llm::review_key(self, key) {
            return took(Handler::EditReview);
        }
        if self.picker.is_some() {
            took(Handler::Picker);
            self.picker_key(key);
            return;
        }
        // `C-w m` moving a floating pane: its keys (the window prefix goes on — `C-w m` again ends it)
        if self.pending.is_empty() && self.prompt.is_none() && self.float_move_key(key) {
            return took(Handler::Move);
        }
        // The chat's view mode: normal mode on its transcript, the chat's table over it
        if let Some(h) = crate::chatdoc::key(self, key) {
            return took(h);
        }
        // The terminal's view mode: its moves and selections on its rows
        if let Some(h) = crate::view::key(self, key) {
            return took(h);
        }
        // A run panel's output: normal mode on its document, the panel's keys over it
        if let Some(h) = crate::rundoc::key(self, key) {
            return took(h);
        }
        if crate::chat::unfold_key(self, key) {
            return took(Handler::Unfold);
        }
        if crate::chat::key(self, key) {
            return took(Handler::Chat);
        }
        if crate::codereview::rail_key(self, key) {
            return took(Handler::Rail);
        }
        if crate::sidebar::key(self, key) {
            return took(Handler::Side);
        }
        if crate::plugin::plugin_panel_key(self, key) {
            return took(Handler::PluginPanel);
        }
        if crate::floatlist::key(self, key) || crate::grouplist::key(self, key) {
            return took(Handler::List);
        }
        // Start screen: `r` = the files last open here
        if self.welcome()
            && self.pending.is_empty()
            && key.plain_char() == Some('r')
            && self.session.restorable.is_some()
        {
            self.restore_files();
            return;
        }
        // Start screen: digits = open a recent file
        if self.welcome()
            && self.pending.is_empty()
            && let Some(d) = key.plain_char().and_then(|c| c.to_digit(10)).filter(|d| *d >= 1)
            && let Some(path) = self.recent.get(d as usize - 1).cloned()
        {
            if let Err(e) = self.open(&path) {
                self.set_error(format!("{e:#}"));
            }
            return;
        }
        if self.prompt.is_some() {
            took(Handler::Prompt);
            // (the session list's own `/`, else the code's)
            if !crate::floatlist::prompt_key(self, key) {
                self.handle_prompt_key(key);
            }
            return;
        }
        // A loading buffer ignores edit keys (`:` commands and buffer moves are allowed — :q·:bn while
        // reading).
        if self.doc().loading && key.plain_char() != Some(':') && self.pending.is_empty() {
            let name = self.doc().display_name(&self.session.root);
            self.set_status(format!("{name} is still loading…"));
            return;
        }
        // `f x`·`r x`·`"a` — the awaited character. A non-character key (esc, etc.) cancels.
        if let Some((f, count)) = self.on_next_char.take() {
            let ch = match (key.code, key.ctrl || key.alt) {
                (Code::Char(c), false) => Some(c),
                (Code::Enter, _) => Some('\n'),
                (Code::Tab, _) => Some('\t'),
                _ => None,
            };
            match ch {
                Some(ch) => self.with_group_count(count, |cx| f(cx, ch)),
                None => self.jump_labels = None, // a `gw` label cancelled
            }
            return;
        }
        // Count prefix (normal/select, no pending key). A leading 0 isn't a count.
        if self.mode != Mode::Insert
            && self.pending.is_empty()
            && let Some(c @ '0'..='9') = key.plain_char()
            && (c != '0' || self.count.is_some())
        {
            let digit = c.to_digit(10).unwrap() as usize;
            self.count = Some(self.count.unwrap_or(0).saturating_mul(10).saturating_add(digit));
            return;
        }
        // A woven view's `enter`: to that change's workspace (`weave.rs`)
        if crate::weave::enter(self, key) {
            return took(Handler::Weave);
        }
        // A web page's `C-c` while it's read: stops the read (`web/page.rs`)
        if crate::web::page::key(self, key) {
            return took(Handler::Page);
        }
        self.pending.push(key);
        self.dispatch_pending();
    }

    /// Looks up the pending keys: run the command, wait for more, or (insert mode) type them.
    fn dispatch_pending(&mut self) {
        let cmds = match self.keymaps.lookup(self.mode, &self.pending) {
            Lookup::Pending => return,
            Lookup::Matched(cmds) => Some(cmds.to_vec()),
            Lookup::NotFound => None,
        };
        let pending = std::mem::take(&mut self.pending);
        let count = self.count.take();
        self.last_trigger_len = pending.len();
        // Sticky view mode: `Z` stays open for the next key (Esc, or any key it doesn't have, leaves)
        let sticky = cmds.is_some() && pending.len() > 1 && pending[0].plain_char() == Some('Z');
        match (cmds, pending.split_last()) {
            (Some(cmds), _) => {
                // (the command a key ran — never the key: `features.rs`)
                if self.counting() {
                    for c in &cmds {
                        match c {
                            MappableCommand::Static(s) => self.features.key(s),
                            MappableCommand::Typed(line) => self.count_typed(line),
                        }
                    }
                }
                self.run(&cmds, count);
                if self.on_next_char.is_some() {
                    self.char_for = pending.iter().map(Key::to_string).collect();
                }
                if sticky && self.mode == Mode::Normal {
                    self.pending = vec![pending[0]];
                }
            }
            // Unmapped keys in insert mode = input. A sequence that broke off (`jx` under `j k`): the keys
            // before are text, the last goes through the keymap again (`j<esc>` = "j", then leave insert).
            (None, Some((&last, before))) if self.mode == Mode::Insert => {
                let chars: Vec<char> = before.iter().filter_map(Key::plain_char).collect();
                let last_char = last.plain_char().filter(|_| before.is_empty());
                if !chars.is_empty() || last_char.is_some() {
                    self.with_group(|cx| {
                        for c in chars.into_iter().chain(last_char) {
                            commands::insert_char(cx, c);
                        }
                    });
                }
                if !before.is_empty() {
                    self.pending.push(last);
                    self.dispatch_pending();
                }
            }
            (None, _) => {}
        }
    }

    pub(crate) fn run(&mut self, cmds: &[MappableCommand], count: Option<usize>) {
        let was_insert = self.mode == Mode::Insert;
        self.with_group(|cx| {
            for cmd in cmds {
                match cmd {
                    MappableCommand::Static(c) => {
                        (c.fun)(&mut Context { editor: cx.editor, count });
                        if c.motion {
                            let fun = c.fun;
                            cx.editor.last_motion = Some(Rc::new(move |cx: &mut Context| {
                                fun(&mut Context { editor: cx.editor, count })
                            }));
                        }
                    }
                    MappableCommand::Typed(line) => {
                        if let Err(e) = typed::execute(cx.editor, line) {
                            cx.editor.set_error(e);
                        }
                    }
                }
                if cx.editor.should_quit {
                    break;
                }
            }
        });
        if !was_insert && self.mode == Mode::Insert {
            self.insert_started(cmds, count);
        }
        // `"a` applies only to the very next command.
        self.selected_register = None;
    }

    // ── Registers ────────────────────────────────────────────────────────

    pub fn register_name(&self) -> char {
        self.selected_register.unwrap_or('"')
    }

    pub fn write_register(&mut self, name: char, values: Vec<String>) {
        match name {
            '_' => {}
            '+' => {
                let what = crate::clipboard::selections(values.len());
                crate::clipboard::copy(&self.events.jobs(), values.join("\n"), what)
            }
            _ => {
                self.registers.insert(name, values);
            }
        }
    }

    /// Runs inside an edit group. Closes the group if it ends outside insert mode.
    pub(crate) fn with_group(&mut self, f: impl FnOnce(&mut Context)) {
        self.with_group_count(None, f)
    }

    pub(crate) fn with_group_count(&mut self, count: Option<usize>, f: impl FnOnce(&mut Context)) {
        self.open_undo_group();
        self.sticky_used = false;
        f(&mut Context { editor: self, count });
        if !self.sticky_used {
            self.sticky = None;
        }
        if self.mode != Mode::Insert {
            self.close_undo_group();
            self.clamp_for_normal();
        }
    }

    /// In normal/select mode the block cursor can't sit past the end of the document (EOF).
    fn clamp_for_normal(&mut self) {
        let doc = self.doc_mut();
        let len = doc.text.len_bytes();
        if len == 0 {
            return;
        }
        let sel = doc.selection().transform(|r| {
            if r.is_empty() && r.head >= len {
                Range::point(crate::graphemes::prev_boundary(&doc.text, len))
            } else {
                r
            }
        });
        doc.set_selection(sel);
    }

    /// The primary cursor's line.
    pub fn cursor_line(&self) -> usize {
        let doc = self.doc();
        doc.text.byte_to_line(doc.selection().primary().cursor(&doc.text).min(doc.text.len_bytes()))
    }

    /// The focused pane's doc-comment block holding `line` when it shows folded (`doc_block_folded`) — `j`/`k`
    /// step over it as one line.
    pub fn folded_doc_block_at(&self, line: usize) -> Option<(usize, usize)> {
        let doc = self.doc();
        // (a woven view's conflict side by side, at the width it was last drawn — `weave.rs`)
        if let Some(m) = crate::weave::of(doc)
            && let Some(c) = m.merge.conflict_at(line).map(|i| &m.merge.conflicts[i])
        {
            let r = c.lines();
            return crate::weave::columns(self, doc, c, m.cols.get(), self.editing_in(true))
                .then(|| (r.start, r.end - 1));
        }
        if !self.doc_comments_render(doc.lang_name()) {
            return None;
        }
        crate::doccomment::block_at(&doc.text, line)
            .filter(|&block| self.doc_block_folded(doc, self.editing_in(true), block))
    }

    /// Whether doc comments render in this language at all: the setting on, and the language has them.
    pub fn doc_comments_render(&self, lang: Option<&str>) -> bool {
        self.config.render_doc_comments && crate::doccomment::applies(lang)
    }

    /// Whether a pane is where its document is being edited: the focused one, in INSERT or SELECT (the mode is
    /// the editor's, but only the focused pane's selection is the one it moves).
    pub fn editing_in(&self, focused: bool) -> bool {
        focused && matches!(self.mode, Mode::Insert | Mode::Select)
    }

    /// **The one rule** for whether doc-comment block `a..=b` of `doc` shows rendered (folded) — every place a
    /// file's code is drawn and every move over it (`j`/`k`, the mouse, labels) asks it: doc comments render
    /// here (`doc_comments_render`); it isn't being edited or selected — where it's being edited (`editing`)
    /// any selection on it opens it, elsewhere one wider than a character; and no change shown on screen is in
    /// or beside it (`doccomment::touches_change`).
    pub fn doc_block_folded(
        &self,
        doc: &crate::document::Document,
        editing: bool,
        block: (usize, usize),
    ) -> bool {
        self.doc_comments_render(doc.lang_name()) && self.doc_block_closed(doc, editing, block)
    }

    /// `doc_block_folded` where doc comments are known to render in `doc` — for a pass over many blocks (the
    /// language looked up once, not per block: by path, without a grammar, that's a glob match each time).
    /// Reading something new here, `closed_key` says it too (a cache of what this decided holds by it).
    pub fn doc_block_closed(
        &self,
        doc: &crate::document::Document,
        editing: bool,
        (a, b): (usize, usize),
    ) -> bool {
        if crate::doccomment::touches_change(doc, a, b) {
            return false;
        }
        // (an edit being written over it or right beside it shows it raw, as the review will — `writing.rs`)
        if crate::writing::shown(doc)
            .is_some_and(|w| w.blocks.iter().any(|x| x.first <= b + 1 && x.last + 1 >= a))
        {
            return false;
        }
        let text = &doc.text;
        let (start, end) = (mv::line_start(text, a), mv::line_full_end(text, b));
        // (sorted: from the first that ends at its start on, until one starts past its end)
        let all = doc.selection().ranges();
        let lo = all.partition_point(|r| r.to() < start);
        !all[lo..].iter().take_while(|r| r.from() <= end).any(|r| {
            let wide = r.to() > crate::graphemes::next_boundary(text, r.from());
            // (its lines are those of its first and last characters: a line selected above isn't this one)
            let last =
                if r.to() > r.from() { crate::graphemes::prev_boundary(text, r.to()) } else { r.from() };
            (editing || wide) && text.byte_to_line(r.from()) <= b && text.byte_to_line(last) >= a
        })
    }

    /// Everything `doc_block_closed` reads but the block, for rows folded by it to be kept while it holds (the
    /// edit review's, `term::review_rows`) — what it reads, this says.
    pub fn closed_key(&self, doc: &crate::document::Document, editing: bool) -> crate::codeview::ClosedKey {
        crate::codeview::ClosedKey {
            doc: (doc.id, doc.version()),
            editing,
            selection: doc.selection().ranges().iter().map(|r| (r.from(), r.to())).collect(),
            changes: crate::codeview::Changes::of(doc).near_key(doc),
            conflicts: doc.conflicts.iter().map(|c| (c.start, c.end)).collect(),
        }
    }

    pub fn open_prompt(&mut self, kind: PromptKind, text: &str) {
        // A plugin's line, replaced: it's told nothing was typed
        if let Some(PromptKind::Plugin(token)) = self.prompt.as_ref().map(|p| p.kind) {
            self.prompt = None;
            self.plugin_prompt_done(token, None);
        }
        self.prompt = Some(Prompt { kind, label: None, text: text.to_string(), cycle: None });
        if kind == PromptKind::Command {
            self.cmdline_prefetch();
        }
    }

    /// Completion candidates for the `:` input (relative to the input when cycling began, if Tab-cycling).
    pub fn cmdline_completion(&self) -> Option<crate::cmdline::Completion> {
        let p = self.prompt.as_ref().filter(|p| p.kind == PromptKind::Command)?;
        let base = p.cycle.as_ref().map_or(p.text.as_str(), |(b, _)| b.as_str());
        Some(crate::cmdline::complete(base, &self.session.root, &self.cmd_lists))
    }

    /// If the lists a candidate needs (folders, themes) aren't read yet, read them on a worker thread.
    fn cmdline_prefetch(&mut self) {
        let Some(p) = self.prompt.as_ref().filter(|p| p.kind == PromptKind::Command) else { return };
        let text = p.text.clone();
        let Some((name, _)) = text.split_once(char::is_whitespace) else { return };
        match crate::cmdline::find(name).map(|c| c.arg) {
            Some(crate::cmdline::Arg::File) => {
                let token = text.rsplit(char::is_whitespace).next().unwrap_or_default();
                let (dir, _) = crate::cmdline::file_query(token, &self.session.root);
                if self.cmd_lists.dirs.contains_key(&dir) || !self.cmd_loading.insert(dir.clone()) {
                    return;
                }
                self.events.jobs().spawn(move || {
                    let mut entries: Vec<(String, bool)> = std::fs::read_dir(&dir)
                        .map(|rd| {
                            rd.flatten()
                                .take(5000)
                                .map(|e| {
                                    let is_dir = e.path().is_dir(); // folder symlinks count as folders
                                    (e.file_name().to_string_lossy().into_owned(), is_dir)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    crate::sorting::sort(&mut entries);
                    move |ed: &mut Editor| {
                        ed.cmd_loading.remove(&dir);
                        ed.cmd_lists.dirs.insert(dir, entries);
                    }
                });
            }
            // Branches for `:review` — local ones first, then remote (git on a worker thread)
            Some(crate::cmdline::Arg::Branch) if self.cmd_lists.branches.is_none() => {
                let key = std::path::PathBuf::from("\0branches");
                if !self.cmd_loading.insert(key.clone()) {
                    return;
                }
                let root = self.session.root.clone();
                self.events.jobs().spawn(move || {
                    let out =
                        crate::gitrepo::git(&root, &["branch", "-a", "--format=%(refname:short)"], None)
                            .unwrap_or_default();
                    let mut names: Vec<String> = out
                        .lines()
                        .map(str::trim)
                        .filter(|n| !n.is_empty() && !n.ends_with("/HEAD"))
                        .map(String::from)
                        .collect();
                    crate::sorting::by_key(&mut names, |n| (n.contains('/'), n.clone()));
                    move |ed: &mut Editor| {
                        ed.cmd_loading.remove(&key);
                        ed.cmd_lists.branches = Some(names);
                    }
                });
            }
            Some(crate::cmdline::Arg::Theme) if self.cmd_lists.themes.is_none() => {
                let key = std::path::PathBuf::from("\0themes");
                if !self.cmd_loading.insert(key.clone()) {
                    return;
                }
                self.events.jobs().spawn(move || {
                    let names = crate::theme::available();
                    move |ed: &mut Editor| {
                        ed.cmd_loading.remove(&key);
                        ed.cmd_lists.themes = Some(names);
                    }
                });
            }
            _ => {}
        }
    }

    /// Tab (forward)·Shift-Tab (backward): cycle through candidates, filling each in. With only one, fill
    /// and stop — a command gets a trailing space, a folder can be completed further inside.
    fn cmdline_tab(&mut self, forward: bool) {
        let Some(comp) = self.cmdline_completion() else { return };
        let n = comp.cands.len();
        let Some(p) = self.prompt.as_mut() else { return };
        if n == 0 {
            return;
        }
        let base = p.cycle.as_ref().map_or_else(|| p.text.clone(), |(b, _)| b.clone());
        if p.cycle.is_none() && n == 1 {
            let mut text = crate::cmdline::apply(&base, &comp, 0);
            // Trailing space = the next Tab shows what comes next (argument, value). Folders continue inside;
            // commands without arguments stay as is
            let more = if comp.command {
                crate::cmdline::find(&comp.cands[0].insert)
                    .is_some_and(|c| c.arg != crate::cmdline::Arg::None)
            } else {
                !comp.cands[0].dir
            };
            if more {
                text.push(' ');
            }
            p.text = text;
            p.cycle = None;
            return self.cmdline_prefetch();
        }
        let i = match (p.cycle.as_ref(), forward) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some((_, i)), true) => (i + 1) % n,
            (Some((_, i)), false) => (i + n - 1) % n,
        };
        p.text = crate::cmdline::apply(&base, &comp, i);
        p.cycle = Some((base, i));
    }

    pub(crate) fn handle_prompt_key(&mut self, key: Key) {
        let Some(prompt) = self.prompt.as_mut() else { return };
        match (key.code, key.ctrl || key.alt) {
            (Code::Esc, _) => self.cancel_prompt(),
            _ if prompt_submits(key) => {
                let Some(Prompt { kind, text, .. }) = self.prompt.take() else { return };
                self.search_origin = None;
                self.submit_prompt(kind, text);
            }
            // `:` completion — Tab·Shift-Tab = cycle and fill, → = accept the dim suggestion
            (Code::Tab | Code::BackTab, false) if prompt.kind == PromptKind::Command => {
                self.cmdline_tab(key.code == Code::Tab);
            }
            (Code::Right, false) if prompt.kind == PromptKind::Command => {
                let comp = self.cmdline_completion();
                if let (Some(p), Some(c)) = (self.prompt.as_mut(), comp)
                    && let Some(g) = crate::cmdline::ghost(&p.text, &c)
                {
                    p.text.push_str(g);
                    p.cycle = None;
                }
            }
            (Code::Backspace, _) => {
                prompt.cycle = None;
                if prompt.text.pop().is_none() {
                    self.cancel_prompt();
                }
            }
            (Code::Char('c'), true) if key.ctrl => self.cancel_prompt(),
            // A new session's name: another base to make it from
            (Code::Char('b'), true) if key.ctrl && prompt.kind == PromptKind::Session => {
                let name = std::mem::take(&mut prompt.text);
                self.prompt = None;
                self.session_base_picker(name);
            }
            (Code::Char(c), false) => {
                prompt.cycle = None;
                prompt.text.push(c);
            }
            // (said, not dropped: a `C-q` here waited for a card the prompt keeps from it)
            (Code::Char(_), true) => {
                let out = match self.offers.is_empty() {
                    true => "enter runs the line, esc close".to_string(),
                    // (the way back is this line's own `esc`: what reaches the card is said from the code)
                    false => format!("esc close — then {} reaches the card", self.c_q_from_code()),
                };
                self.key_unused(key, "the command line", &out);
            }
            _ => {}
        }
        if self.prompt.as_ref().is_some_and(|p| p.kind == PromptKind::Command) {
            self.cmdline_prefetch();
        }
        // Once ":ask …" starts being typed, pre-spawn claude — typing time hides the cold start.
        if self.prompt.as_ref().is_some_and(|p| p.kind == PromptKind::Command && p.text.starts_with("ask ")) {
            llm::prewarm(self);
        }
    }

    /// Closes the prompt without running it (Esc·C-c·Backspace on empty) — reverts the view moved by the
    /// search preview, so the screen follows the cursor again.
    fn cancel_prompt(&mut self) {
        if let Some(PromptKind::Plugin(token)) = self.prompt.take().map(|p| p.kind) {
            self.plugin_prompt_done(token, None);
        }
        if let Some(top) = self.search_origin.take() {
            self.doc_mut().set_top(top);
        }
    }

    fn submit_prompt(&mut self, kind: PromptKind, text: String) {
        match kind {
            PromptKind::Command => {
                self.count_typed(&text);
                self.run(&[MappableCommand::Typed(text)], None)
            }
            PromptKind::Search { reverse } => {
                // Empty search = repeat the previous search term
                let pat = if text.is_empty() { self.search.clone().unwrap_or_default() } else { text };
                self.with_group(|cx| search::search(cx, &pat, reverse));
            }
            PromptKind::SelectRegex => self.with_group(|cx| search::select_regex(cx, &text)),
            PromptKind::Split => self.with_group(|cx| search::split_selection(cx, &text)),
            PromptKind::Keep { remove } => self.with_group(|cx| search::keep_selections(cx, &text, remove)),
            PromptKind::BreakCondition => self.set_breakpoint_field(false, &text),
            PromptKind::LogMessage => self.set_breakpoint_field(true, &text),
            PromptKind::Plugin(token) => self.plugin_prompt_done(token, Some(text)),
            PromptKind::Watch => self.add_watch(&text),
            PromptKind::Shell(p) => self.shell_pipe(p, &text),
            PromptKind::Replace => self.replace_plan_start(text),
            PromptKind::Note(target) => crate::notes::submit(self, target, &text),
            PromptKind::Session => self.session_new(&text, None),
            PromptKind::Rename => {
                if !text.is_empty() {
                    let extra = serde_json::json!({ "newName": text });
                    self.lsp_request(crate::lsp_editor::Kind::Rename, "textDocument/rename", extra);
                }
            }
        }
    }
}

/// Cursor to (line, byte column) — clamped to the text, snapped to a character start (the file may have changed).
fn place_cursor(doc: &mut Document, line: usize, col: usize) {
    let line = line.min(mv::last_line(&doc.text));
    let (ls, le) = (mv::line_start(&doc.text, line), mv::line_end(&doc.text, line));
    let pos = doc.text.char_to_byte(doc.text.byte_to_char((ls + col).min(le)));
    doc.set_selection(Selection::point(pos));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feeds a string like "xd" as keys. Angle brackets are named keys, like `<esc>`, `<A-x>`, `<ret>`.
    fn feed(ed: &mut Editor, keys: &str) {
        let mut chars = keys.chars();
        while let Some(c) = chars.next() {
            let key = if c == '<' {
                let name: String = chars.by_ref().take_while(|&c| c != '>').collect();
                name.parse().unwrap()
            } else {
                Key::plain(Code::Char(c))
            };
            ed.handle_key(key);
        }
    }

    /// `feed`, each key an event: what runs after every event (the syncs) runs after each.
    fn feed_events(ed: &mut Editor, keys: &str) {
        let mut chars = keys.chars();
        while let Some(c) = chars.next() {
            let key = if c == '<' {
                let name: String = chars.by_ref().take_while(|&c| c != '>').collect();
                name.parse().unwrap()
            } else {
                Key::plain(Code::Char(c))
            };
            ed.handle_event(Event::Key(key));
        }
    }

    /// `keys` answering the front card from wherever the keys are — `C-q` first when it hasn't them (the
    /// chat keeps them as a card comes — frictions26).
    fn answer(ed: &mut Editor, keys: &str) {
        if !ed.card_has_keys() {
            feed(ed, "<C-q>");
        }
        feed(ed, keys);
    }

    fn editor(text: &str) -> Editor {
        let mut ed = Editor::new(Config::default());
        ed.docs[0].text = ropey::Rope::from_str(text);
        // (tests make their projects in the temp folder — follow goes only into the project)
        ed.session.root = test_root();
        // (a chat opened starts Claude: a stand-in that reads and says nothing, never the real one —
        // `llm::spawn` refuses it in tests)
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        ed
    }

    /// The root `editor()` gives: the temp folder, real path (not the folder the tests run in).
    fn test_root() -> PathBuf {
        std::fs::canonicalize(std::env::temp_dir()).unwrap()
    }

    /// This repository, for tests whose project it is (reads of its own files — harmless).
    fn this_repo() -> PathBuf {
        std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap()
    }

    fn run(text: &str, keys: &str) -> Editor {
        let mut ed = editor(text);
        feed(&mut ed, keys);
        ed
    }

    fn text(ed: &Editor) -> String {
        ed.doc().text.to_string()
    }

    #[test]
    fn delete_line() {
        assert_eq!(text(&run("a\nb\n", "xd")), "b\n");
        assert_eq!(text(&run("a\nb\nc\n", "2xd")), "c\n");
        assert_eq!(text(&run("a\nb\nc\n", "xxd")), "c\n");
    }

    #[test]
    fn word_delete_and_change() {
        assert_eq!(text(&run("foo bar", "wd")), "bar");
        assert_eq!(text(&run("foo bar", "ecqux<esc>")), "qux bar");
    }

    #[test]
    fn insert_and_append() {
        assert_eq!(text(&run("", "ihello<esc>")), "hello");
        assert_eq!(text(&run("ab", "aX<esc>")), "aXb");
        assert_eq!(text(&run("ab", "AX<esc>")), "abX");
        assert_eq!(text(&run("  ab", "lllIX<esc>")), "  Xab");
        assert_eq!(text(&run("ab", "i<backspace>x<del><esc>")), "xb");
    }

    #[test]
    fn open_lines_keep_indent() {
        assert_eq!(text(&run("  a\n", "oX<esc>")), "  a\n  X\n");
        assert_eq!(text(&run("  a\n", "OX<esc>")), "  X\n  a\n");
        assert_eq!(text(&run("  a", "A<ret>b<esc>")), "  a\n  b");
    }

    /// #176: in a commit message (`indent-heuristic = "simple"` — Helix's fallback, git-commit having no
    /// indent query) a new line takes the indentation of the line right before it: after a blank line none,
    /// so `O` on a heading below an indented bullet's paragraph starts at the margin; right under the bullet
    /// it goes on with it, as in Helix. Elsewhere the nearest non-blank line's, as before.
    #[test]
    fn a_commit_message_carries_no_indent_past_a_blank_line() {
        let msg = "Subject\n\n- a bullet\n  wrapped on\n\nLinux checks\n";
        let commit = |keys: &str| {
            let mut ed = editor(msg);
            ed.docs[0].path = Some(test_root().join(".git/COMMIT_EDITMSG"));
            feed(&mut ed, keys);
            text(&ed)
        };
        let after = |head: &str| format!("Subject\n\n- a bullet\n  wrapped on\n\n{head}Linux checks\n");
        assert_eq!(commit("5jOHeading<esc>"), after("Heading\n"));
        assert_eq!(commit("4joX<esc>"), after("X\n"));
        assert_eq!(commit("5jI<ret><esc>"), after("\n"));
        assert_eq!(commit("3joY<esc>"), "Subject\n\n- a bullet\n  wrapped on\n  Y\n\nLinux checks\n");
        assert_eq!(text(&run(msg, "5jOH<esc>")), after("  H\n"), "no language: the nearest non-blank's");
    }

    #[test]
    fn multi_cursor_insert() {
        assert_eq!(text(&run("a\nb\nc\n", "Ci-<esc>")), "-a\n-b\nc\n");
        assert_eq!(text(&run("a\nb\nc\n", "2Ci-<esc>")), "-a\n-b\n-c\n");
        assert_eq!(text(&run("a\nb\nc\n", "Ci-<esc>,i+<esc>")), "-a\n-+b\nc\n");
    }

    #[test]
    fn undo_redo_groups() {
        let mut ed = run("a\nb\n", "xd");
        feed(&mut ed, "u");
        assert_eq!(text(&ed), "a\nb\n");
        assert!(!ed.doc().is_modified());
        feed(&mut ed, "U");
        assert_eq!(text(&ed), "b\n");
        // One insert = one undo
        let mut ed = run("", "ihello world<esc>");
        feed(&mut ed, "u");
        assert_eq!(text(&ed), "");
    }

    #[test]
    fn yank_paste() {
        assert_eq!(text(&run("ab", "yp")), "aab");
        assert_eq!(text(&run("ab", "yP")), "aab");
        assert_eq!(text(&run("a\nb\n", "xyp")), "a\na\nb\n");
        assert_eq!(text(&run("a\nb\n", "jxyP")), "a\nb\nb\n");
        // Line-wise paste after a final line with no newline
        assert_eq!(text(&run("a\nb", "xyjp")), "a\nb\na");
        // The pasted text is selected
        let ed = run("ab", "yp");
        assert_eq!(ed.doc().selection().primary(), Range::new(1, 2));
    }

    #[test]
    fn select_mode_extends() {
        assert_eq!(text(&run("abcd", "vlld")), "d");
        assert_eq!(text(&run("foo bar baz", "vwwd")), "baz");
        assert_eq!(run("abcd", "vl<esc>").mode, Mode::Normal);
    }

    #[test]
    fn goto_and_counts() {
        let ed = run("a\nb\nc\n", "ge");
        assert_eq!(ed.doc().selection().primary(), Range::point(4));
        let ed = run("a\nb\nc\n", "2gg");
        assert_eq!(ed.doc().selection().primary(), Range::point(2));
        let ed = run("abcdef", "3l");
        assert_eq!(ed.doc().selection().primary(), Range::point(3));
        let ed = run("abc  \n", "gl");
        assert_eq!(ed.doc().selection().primary(), Range::point(4));
    }

    #[test]
    fn repeat_last_motion() {
        let ed = run("a b c d", "w<A-.><A-.>");
        assert_eq!(ed.doc().selection().primary(), Range::new(4, 6));
    }

    /// `:` completion: Tab cycles and fills candidates; a single one fills up to a trailing space;
    /// → accepts the dim suggestion.
    #[test]
    fn command_line_completion_keys() {
        let mut ed = Editor::new(Config::default());
        // (`:git-switch `'s branch names are read by git in the root: one that isn't tarae's own repository)
        ed.session.root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
        let text = |ed: &Editor| ed.prompt.as_ref().map(|p| p.text.clone()).unwrap_or_default();
        feed(&mut ed, ":wr<tab>");
        assert_eq!(text(&ed), "write", "several → the first");
        feed(&mut ed, "<tab>");
        assert_ne!(text(&ed), "write", "pressing again → next candidate");
        feed(&mut ed, "<backtab>");
        assert_eq!(text(&ed), "write", "Shift-Tab goes backward");
        feed(&mut ed, "<esc>:tut<tab>");
        assert_eq!(text(&ed), "tutor", "only one — argless command, no trailing space");
        feed(&mut ed, "<esc>:git-sw<tab>");
        assert_eq!(text(&ed), "git-switch ", "command with an argument gets a trailing space");
        feed(&mut ed, "<esc>:set editor.auto-sa<tab><tab>");
        assert_eq!(text(&ed), "set editor.auto-save \"off\"", "unique path: trailing space, Tab = value");
        feed(&mut ed, "<esc>:toggle editor.inlay<right>");
        assert_eq!(text(&ed), "toggle editor.inlay-hints", "→ = accept the dim suggestion");
        feed(&mut ed, "<ret>");
        assert!(!ed.config.inlay_hints, "the completed command runs as is");
    }

    /// In normal mode j/k treat a folded doc-comment block as one line (stepping over it without unfolding).
    #[test]
    fn j_and_k_step_over_folded_doc_comments() {
        let mut ed = Editor::new(Config::default());
        ed.docs[0].text = ropey::Rope::from_str("fn a() {}\n/// one\n/// two\n/// three\nfn b() {}\n");
        ed.docs[0].path = Some("/tmp/x.rs".into());
        feed(&mut ed, "j");
        assert_eq!(ed.cursor_line(), 1, "lands on the block (first line)");
        feed(&mut ed, "j");
        assert_eq!(ed.cursor_line(), 4, "past the block");
        feed(&mut ed, "k");
        assert_eq!(ed.cursor_line(), 1, "coming up lands on the block's first line");
        feed(&mut ed, "k");
        assert_eq!(ed.cursor_line(), 0);
        // With rendering off, line by line
        ed.config.render_doc_comments = false;
        feed(&mut ed, "jj");
        assert_eq!(ed.cursor_line(), 2);
    }

    /// Default `X` = line-select upward one line at a time (mirror of `x`) — one more line per press.
    #[test]
    fn default_shift_x_selects_lines_upward() {
        let mut ed = Editor::new(Config::default());
        ed.docs[0].text = ropey::Rope::from_str("a\nb\nc\nd\n");
        feed(&mut ed, "jjj");
        feed(&mut ed, "X");
        let sel = |ed: &Editor| {
            let r = ed.doc().selection().primary();
            ed.doc().text.byte_slice(r.from()..r.to()).to_string()
        };
        assert_eq!(sel(&ed), "c\nd\n");
        feed(&mut ed, "X");
        assert_eq!(sel(&ed), "b\nc\nd\n");
        feed(&mut ed, "d");
        assert_eq!(text(&ed), "a\n");
    }

    #[test]
    fn x_binding_selects_lines_upward() {
        // X = ["extend_line_up", "extend_to_line_bounds"] — line-select upward one line at a time
        let (config, w) =
            crate::config::parse("[keys.normal]\nX = [\"extend_line_up\", \"extend_to_line_bounds\"]\n");
        assert!(w.is_empty());
        let mut ed = Editor::new(config);
        ed.docs[0].text = ropey::Rope::from_str("a\nb\nc\n");
        feed(&mut ed, "jjXd");
        assert_eq!(text(&ed), "a\n");
    }

    #[test]
    fn select_all_and_collapse() {
        assert_eq!(text(&run("abc\ndef", "%d")), "");
        let ed = run("abc", "%;");
        assert_eq!(ed.doc().selection().primary(), Range::point(2));
    }

    #[test]
    fn typed_goto_and_quit_guard() {
        let mut ed = run("a\nb\nc\n", ":2<ret>");
        assert_eq!(ed.doc().selection().primary(), Range::point(2));
        feed(&mut ed, "d:q<ret>");
        assert!(!ed.should_quit);
        assert_eq!(ed.status.as_ref().map(|s| s.1), Some(Severity::Error));
        feed(&mut ed, ":q!<ret>");
        assert!(ed.should_quit);
    }

    /// Waits until one background job finishes and is applied.
    fn settle(ed: &mut Editor) {
        let ev = ed.events.recv_timeout(std::time::Duration::from_secs(5)).expect("job result");
        ed.handle_event(ev);
    }

    #[test]
    fn nothing_to_pick_is_news_a_failure_is_an_error() {
        use crate::picker::{FillError, Picker};
        let mut ed = editor("");
        ed.open_picker(
            Picker::new("tasks", Vec::new(), false),
            Some(Box::new(|| Err(FillError::Nothing("no tasks here".into())))),
        );
        settle(&mut ed);
        assert!(ed.picker.is_none(), "an empty list doesn't stay open");
        assert_eq!(
            ed.toasts.last().map(|t| (t.text.as_str(), t.kind)),
            Some(("no tasks here", ToastKind::Info))
        );
        ed.open_picker(
            Picker::new("grep", Vec::new(), false),
            Some(Box::new(|| Err("rg: bad regex".to_string().into()))),
        );
        settle(&mut ed);
        assert_eq!(ed.toasts.last().map(|t| t.kind), Some(ToastKind::Error));
    }

    #[test]
    fn a_toast_is_redrawn_only_while_it_fades() {
        use std::time::{Duration, Instant};
        let mut ed = editor("");
        assert_eq!(ed.toast_wake(), None, "no toast, no frames");
        ed.set_status("saved");
        assert_eq!(ed.toast_wake(), Some(TICK), "fading in");
        let life = ed.toasts[0].life;
        ed.toasts[0].at = Instant::now() - Duration::from_secs(1);
        let wake = ed.toast_wake().unwrap();
        let steady = life - Duration::from_secs(1) - TOAST_FADE;
        assert!(
            wake <= steady && wake + Duration::from_millis(50) > steady,
            "one wake-up when the fade-out starts: {wake:?}"
        );
        ed.toasts[0].at = Instant::now() - (life - TOAST_FADE / 2);
        assert_eq!(ed.toast_wake(), Some(TICK), "fading out");
    }

    /// An editor whose chat is waiting on Claude (a process that never answers).
    fn waiting_chat(text: &str) -> Editor {
        let mut ed = editor(text);
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        crate::chat::open(&mut ed);
        ed.session.chat.as_mut().unwrap().state = crate::chat::State::Thinking;
        ed
    }

    /// N6: the waiting animation wants its next step while Claude works — in a terminal that never says
    /// where its focus is, too. Focus out (CSI ?1004's `CSI O`) rests it, focus back or any input wakes it;
    /// a toast still gets its fade meanwhile.
    #[test]
    fn the_waiting_animation_rests_while_the_terminal_is_unfocused() {
        let mut ed = waiting_chat("x\n");
        let next = ed.tick_wanted().expect("animating — no focus event yet");
        assert!(next <= TICK + std::time::Duration::from_millis(2), "{next:?}");
        ed.handle_event(Event::Focus(false));
        assert_eq!(ed.tick_wanted(), None, "unfocused: resting");
        ed.handle_event(Event::Focus(true));
        assert!(ed.tick_wanted().is_some(), "focus back");
        ed.handle_event(Event::Focus(false));
        ed.handle_event(Event::Key("left".parse().unwrap()));
        assert!(ed.tick_wanted().is_some(), "input means the terminal has focus");
        ed.handle_event(Event::Focus(false));
        ed.set_status("saved");
        assert_eq!(ed.tick_wanted(), Some(TICK), "a toast's fade-in still draws");
    }

    /// A wake-up asked while focused that lands after focus went (`tick_landed`): the resting animation's step
    /// draws nothing; a toast still on its clock is drawn; one overtaken draws nothing whatever the focus.
    #[test]
    fn a_tick_asked_while_focused_draws_nothing_once_unfocused() {
        use std::time::Instant;
        let mut ed = waiting_chat("x\n");
        let landed = |ed: &mut Editor, focused: bool| {
            let at = Instant::now();
            (ed.ticking, ed.unfocused, ed.unchanged) = (Some(at), !focused, false);
            ed.tick_landed(at);
            assert_eq!(ed.ticking, None);
            std::mem::take(&mut ed.unchanged)
        };
        assert!(!landed(&mut ed, true), "focused: the animation's step");
        assert!(landed(&mut ed, false), "unfocused: nothing on a clock");
        ed.set_status("saved");
        assert!(!landed(&mut ed, false), "unfocused, a toast fading in: drawn");
        (ed.ticking, ed.unchanged) = (Some(Instant::now()), false);
        ed.tick_landed(Instant::now() - std::time::Duration::from_millis(5));
        assert!(ed.unchanged, "overtaken");
    }

    /// `:preflight` runs every step one after another in the task panel — the ones after a failure too —
    /// each with its row (how it ended, its time), a failure's places in `]q`, a pass with a note, and a
    /// one-line summary; the markers between steps aren't shown, each step's output is under its heading.
    #[test]
    fn preflight_runs_every_step_and_sums_up() {
        use crate::preflight::{Set, State, Step};
        let dir = std::env::temp_dir().join(format!("tarae-preflight-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        let step = |name: &str, cmd: &str| Step {
            name: name.into(),
            cmd: cmd.into(),
            note: None,
            from: ".tarae.toml".into(),
            full: false,
        };
        let mut ed = editor("");
        ed.session.root = dir.clone();
        // No steps, and no kind of project a pack knows (looked for on a worker)
        ed.preflight_start(Set::Quick).unwrap();
        settle_for(&mut ed, |ed| ed.status.is_some());
        let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert!(
            said.starts_with("no preflight steps — no language pack knows this kind of project"),
            "{said}"
        );
        // A Maven project with its own wrapper: no defaults — the wrapper is the repository's script, run from
        // the project's own [preflight] once trusted — and it says so, how to list it
        for f in ["pom.xml", "mvnw"] {
            std::fs::write(dir.join(f), "").unwrap();
        }
        ed.status = None;
        ed.preflight_start(Set::Quick).unwrap();
        settle_for(&mut ed, |ed| ed.status.is_some());
        let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert!(said.starts_with("no preflight steps — this project builds with its own ./mvnw"), "{said}");
        assert!(ed.offers.is_empty() && ed.session.task_run.is_none(), "nothing asked, nothing ran");
        for f in ["pom.xml", "mvnw"] {
            std::fs::remove_file(dir.join(f)).unwrap();
        }
        ed.config.preflight = vec![
            step("fmt", "echo formatted"),
            step("clippy", "echo 'a.rs:2:1: error: not like that' >&2; exit 3"),
            Step { note: Some("not measured".into()), ..step("perf", "echo 'not measured (machine busy)'") },
            step("test", "cd / && echo 'all fine'; exit 0"),
        ];
        // (none marked full: one set, every step in it, and nothing says "quick")
        ed.preflight_start(Set::Quick).unwrap();
        assert!(
            ed.preflight_start(Set::Full).unwrap_err().contains("preflight is running"),
            "not twice at once"
        );
        settle_for(&mut ed, |ed| {
            ed.session.task_run.as_ref().is_some_and(|r| r.state != crate::task::State::Running)
        });
        let p = ed.session.preflight.as_ref().unwrap();
        let states: Vec<&State> = p.steps.iter().map(|s| &s.state).collect();
        assert_eq!(
            states,
            [&State::Passed, &State::Failed(3), &State::Passed, &State::Passed],
            "every step ran"
        );
        assert!(p.steps.iter().all(|s| s.took.is_some()));
        assert_eq!(p.steps[2].note.as_deref(), Some("not measured (machine busy)"));
        let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert!(
            said.starts_with(
                "preflight: 3 of 4 passed — clippy failed · perf: not measured (machine busy) · "
            ),
            "{said}"
        );
        assert!(said.ends_with(" — ]q goes to the first"), "{said}");
        let listed: Vec<(PathBuf, usize)> =
            ed.session.qlist.as_ref().unwrap().items.iter().map(|i| (i.path.clone(), i.line)).collect();
        assert_eq!(listed, [(dir.join("a.rs"), 1)], "the failure's place");
        let out: Vec<String> =
            ed.session.task_run.as_ref().unwrap().output.iter().map(|(_, l)| l.into_owned()).collect();
        assert_eq!(out[..3], ["── fmt ──", "formatted", "── clippy ──"], "{out:?}");
        assert!(!out.iter().any(|l| l.contains("tarae-preflight")), "{out:?}");
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 120, 30).unwrap();
        let mut parser = vt100::Parser::new(30, 120, 0);
        parser.process(&bytes);
        let s = parser.screen().contents();
        assert!(
            s.contains("● fmt") && s.contains("▲ clippy") && s.contains("● perf") && s.contains("● test"),
            "{s}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// With no steps of the project's own, the packs' defaults: the first time, on a card that names them and
    /// says plainly they run the project's code, yours after them; `n` runs nothing, `y` runs them all, and
    /// the next `:preflight` there doesn't ask again. The project's own steps replace the packs' — yours
    /// still go after, and no card asks.
    #[test]
    fn preflight_uses_the_packs_defaults_after_saying_so() {
        use crate::preflight::{Set, State, Step};
        let dir = std::env::temp_dir().join(format!("tarae-preflight-packs-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        let step = |name: &str, cmd: &str, from: &str| Step {
            name: name.into(),
            cmd: cmd.into(),
            note: None,
            from: from.into(),
            full: false,
        };
        let mut ed = editor("");
        ed.session.root = dir.clone();
        ed.config.preflight_local = vec![step("mine", "echo mine", ".tarae.local.toml")];
        // A Cargo project: the rust pack's steps, on a card
        ed.preflight_start(Set::Quick).unwrap();
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        let q = ed.offers[0].question.clone();
        assert!(q.starts_with("The rust pack's defaults:"), "{q}");
        // (what's run is the project's code — said plainly, not "no trust needed")
        let lines: Vec<&str> = q.split('\n').collect();
        let say =
            lines.iter().position(|l| *l == crate::preflight::DEFAULTS_SAY).expect("the caution, a line");
        assert!(lines[say].contains("they build and run this project's code (its tests and build scripts)"));
        assert!(!q.contains("no trust"), "{q}");
        assert_eq!(ed.offers[0].warning, Some(say..say + 1), "the caution alone in the warning color");
        assert!(q.contains("  clippy  cargo clippy --workspace --all-targets -- -D warnings"), "{q}");
        assert!(q.contains("Yours, from .tarae.local.toml:\n  mine    echo mine"), "{q}");
        assert!(ed.offers[0].weighty(), "it runs commands: y waits for it settled in sight");
        ed.offer_dismiss();
        assert!(ed.session.task_run.is_none(), "n: nothing ran");
        // `y`: they run, yours after (stand-in pack steps — a real one would run cargo here)
        let pack = vec![step("fmt", "echo formatted", "rust pack")];
        ed.preflight_found(dir.clone(), pack.clone(), ed.config.preflight_local.clone(), Set::Quick);
        assert_eq!(ed.offers.len(), 1);
        ed.offer_accept();
        let done = |ed: &Editor| {
            ed.session.task_run.as_ref().is_some_and(|r| r.state != crate::task::State::Running)
        };
        settle_for(&mut ed, done);
        let ran = |ed: &Editor| -> Vec<(String, State)> {
            ed.session
                .preflight
                .as_ref()
                .unwrap()
                .steps
                .iter()
                .map(|s| (s.name.clone(), s.state.clone()))
                .collect()
        };
        assert_eq!(ran(&ed), [("fmt".to_string(), State::Passed), ("mine".to_string(), State::Passed)]);
        // Told once: not asked again in this project
        ed.preflight_found(dir.clone(), pack, ed.config.preflight_local.clone(), Set::Quick);
        assert!(ed.offers.is_empty(), "no card the second time");
        settle_for(&mut ed, done);
        // The project's own replace the packs'; yours still after
        ed.config.preflight = vec![step("own", "echo own", ".tarae.toml")];
        ed.preflight_start(Set::Quick).unwrap();
        assert!(ed.offers.is_empty());
        settle_for(&mut ed, done);
        assert_eq!(ran(&ed).iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), ["own", "mine"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Two sets: `:preflight` runs the quick one — every step but the ones marked `full` — named so in its
    /// tab and its end, which points at the full one; `:preflight-full` runs them all. A step of yours can
    /// be a full one too (the Linux run), and every step marked full leaves the quick set nothing to run.
    #[test]
    fn preflight_runs_the_quick_set_and_the_full_one_on_its_own() {
        use crate::preflight::Step;
        let dir = std::env::temp_dir().join(format!("tarae-preflight-sets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let step = |name: &str, full: bool| Step {
            name: name.into(),
            cmd: format!("echo ran {name}"),
            note: None,
            from: ".tarae.toml".into(),
            full,
        };
        let mut ed = editor("");
        ed.session.root = dir.clone();
        ed.config.preflight = vec![step("fmt", false), step("perf", true), step("test", false)];
        ed.config.preflight_local = vec![Step { from: ".tarae.local.toml".into(), ..step("linux", true) }];
        assert_eq!(ed.preflight_quick(), Some(vec!["fmt".to_string(), "test".to_string()]));
        let done = |ed: &Editor| {
            ed.session.task_run.as_ref().is_some_and(|r| r.state != crate::task::State::Running)
        };
        let ran = |ed: &Editor| -> Vec<String> {
            ed.session.preflight.as_ref().unwrap().steps.iter().map(|s| s.name.clone()).collect()
        };
        let said = |ed: &Editor| ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        crate::typed::execute(&mut ed, "preflight").unwrap();
        let tab = ed.slot_tabs().into_iter().find(|t| t.slot == crate::sizes::Slot::Task).unwrap();
        assert_eq!(tab.label, "quick preflight");
        settle_for(&mut ed, done);
        assert_eq!(ran(&ed), ["fmt", "test"]);
        let out: Vec<String> =
            ed.session.task_run.as_ref().unwrap().output.iter().map(|(_, l)| l.into_owned()).collect();
        assert!(!out.iter().any(|l| l.contains("ran perf") || l.contains("ran linux")), "{out:?}");
        assert!(said(&ed).starts_with("quick preflight: all 2 passed · "), "{}", said(&ed));
        assert!(
            said(&ed).ends_with(" — the full set (2 more steps) runs before publish: :preflight-full"),
            "{}",
            said(&ed)
        );
        crate::typed::execute(&mut ed, "preflight-full").unwrap();
        settle_for(&mut ed, done);
        assert_eq!(ran(&ed), ["fmt", "perf", "test", "linux"]);
        assert_eq!(ed.session.task_run.as_ref().unwrap().task.name, "full preflight");
        assert!(said(&ed).starts_with("full preflight: all 4 passed · "), "{}", said(&ed));
        assert!(!said(&ed).contains("before publish"), "{}", said(&ed));
        // Every step full: nothing quick to run — said, not an empty pass
        ed.config.preflight = vec![step("perf", true)];
        ed.config.preflight_local.clear();
        let e = crate::typed::execute(&mut ed, "preflight").unwrap_err();
        assert!(e.contains(":preflight-full runs them"), "{e}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A waiting tick lands just past the animation's next step — not a `TICK` after whenever the last one
    /// was handled (that drifted, and the wave now and then skipped a step).
    #[test]
    fn a_waiting_tick_lands_on_the_next_step() {
        let ms = std::time::Duration::from_millis;
        assert_eq!(step_in(ms(250)), ms(52));
        assert_eq!(step_in(ms(1_099)), ms(3));
        assert_eq!(step_in(ms(300)), ms(102), "on a step: the next one");
    }

    /// N6: a frame that only moves the waiting animation writes only its cells — the wave's and the seconds
    /// beside it — not the screen (`screen.rs`'s cell diff; a frame it couldn't read would go out whole).
    #[test]
    fn a_waiting_frame_writes_only_the_animation() {
        let mut ed = waiting_chat("fn main() {\n    println!(\"hi\");\n}\n");
        let mut screen = crate::screen::Screen::new();
        let frame = |ed: &mut Editor| {
            let mut b = Vec::new();
            crate::term::render(ed, &mut b, 160, 50).unwrap();
            b
        };
        let first = frame(&mut ed);
        let whole = screen.present(&first, 160, 50).len();
        std::thread::sleep(ed.tick_wanted().unwrap());
        let next = frame(&mut ed);
        let out = String::from_utf8(screen.present(&next, 160, 50).to_vec()).unwrap();
        let escapes = regex::Regex::new(r"\x1b\[[0-9;:?]*[ -/]*[@-~]").unwrap();
        let text = escapes.replace_all(&out, "");
        assert!(text.chars().any(|c| "▂▃▄▅▆▇".contains(c)), "the wave moved: {text:?}");
        assert!(text.chars().all(|c| "▂▃▄▅▆▇0123456789.s ".contains(c)), "only the animation: {text:?}");
        assert!(out.len() < 300, "{} bytes (the whole screen: {whole})", out.len());
    }

    /// An sh standing in for `claude` that reads one stdin line and emits a fixed result event.
    fn fake_claude(ed: &mut Editor, reply: &str, is_error: bool) {
        let json = serde_json::json!({"type": "result", "is_error": is_error, "result": reply}).to_string();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), format!("read line; printf '%s\\n' '{json}'")];
    }

    /// Processes events until the condition holds (a streamed answer is several events).
    fn settle_until(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done(ed) {
            let left = end.saturating_duration_since(std::time::Instant::now());
            let ev = ed.events.recv_timeout(left).expect("event before timeout");
            ed.handle_event(ev);
        }
    }

    /// `|` replaces each selection with the command's output (one undo step), `!`/`A-!` insert output
    /// before/after, `$` keeps the selections the command accepts; a failing command changes nothing.
    #[test]
    fn shell_pipes_on_selections() {
        let done = |ed: &Editor| ed.status.as_ref().is_none_or(|(m, _)| !m.starts_with("running"));
        let mut ed = run("abc def\n", "%s\\w+<ret>|tr a-z A-Z<ret>");
        settle_until(&mut ed, done);
        assert_eq!(text(&ed), "ABC DEF\n");
        assert_eq!(ed.doc().selection().len(), 2, "each selection piped on its own");
        feed(&mut ed, "u");
        assert_eq!(text(&ed), "abc def\n", "one undo step");
        feed(&mut ed, "%s\\w+<ret>$grep b<ret>");
        settle_until(&mut ed, done);
        assert_eq!(ed.doc().selection().len(), 1);
        assert_eq!(selected(&ed), "abc");
        feed(&mut ed, "!printf x<ret>");
        settle_until(&mut ed, done);
        assert_eq!(text(&ed), "xabc def\n");
        feed(&mut ed, ";<A-!>printf y<ret>");
        settle_until(&mut ed, done);
        assert_eq!(text(&ed), "xyabc def\n");
        feed(&mut ed, "%|exit 1<ret>");
        settle_until(&mut ed, done);
        assert_eq!(text(&ed), "xyabc def\n", "failure: nothing changes");
        assert!(ed.status.as_ref().is_some_and(|(m, _)| m.contains("exit 1")), "{:?}", ed.status);
    }

    /// Global search results → `C-r` → replacement → per-file preview → Enter replaces in the files still
    /// listed (one undo step each, open buffers included) → `:wa` saves them.
    #[test]
    fn replace_across_files() {
        let dir = std::env::temp_dir().join(format!("tarae-repl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap(); // buffers hold real paths (macOS /private/var)
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "foo one\nbar\nfoo two\n").unwrap();
        std::fs::write(&b, "foo three\n").unwrap();
        let mut ed = editor("");
        let hit = |p: &std::path::Path, line: usize| crate::picker::Item {
            label: format!("{}:{}", p.display(), line + 1),
            action: Action::Goto { path: p.to_path_buf(), line, col: 0 },
            hint: String::new(),
            glyph: None,
        };
        let mut picker = Picker::new("grep /foo/", vec![hit(&a, 0), hit(&a, 2), hit(&b, 0)], false);
        picker.grep = Some("foo".into());
        ed.open_picker(picker, None);
        feed(&mut ed, "<C-r>baz<ret>");
        settle_until(&mut ed, |ed| ed.picker.is_some());
        let p = ed.picker.as_ref().unwrap();
        assert_eq!(p.items().len(), 2, "one row per file");
        assert!(p.title.starts_with("replace 3"), "{}", p.title);
        feed(&mut ed, "<ret>");
        let text_of = |ed: &Editor, p: &std::path::Path| {
            ed.docs.iter().find(|d| d.path.as_deref() == Some(p)).map(|d| d.text.to_string())
        };
        assert_eq!(text_of(&ed, &a).as_deref(), Some("baz one\nbar\nbaz two\n"));
        assert_eq!(text_of(&ed, &b).as_deref(), Some("baz three\n"));
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "foo one\nbar\nfoo two\n", "not saved yet");
        feed(&mut ed, ":wa<ret>");
        ed.flush_saves();
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "baz three\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `space g`: with the index as the gutter's base, `r` resets a hunk, `s` stages it (it leaves the
    /// gutter and shows in `git diff --cached`), `b` blames the cursor line.
    #[test]
    fn git_menu_reset_stage_blame() {
        let dir = std::env::temp_dir().join(format!("tarae-gitmenu-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(&dir).args(args).output().expect("git").stdout
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Kim"]);
        git(&["config", "user.email", "kim@example.com"]);
        let file = dir.join("a.txt");
        std::fs::write(&file, "one\ntwo\nthree\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-qm", "first"]);
        let mut ed = editor("");
        ed.session.root = dir.clone(); // (a file's git is the session's repository's — `gitrepo`)
        ed.git_auto = true;
        // (the git tab `space g` opens reads its status in the panel's folder — not this repository)
        let sb = crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, dir.clone());
        ed.session.sidebar = Some(sb);
        ed.open(&file).unwrap();
        settle_until(&mut ed, |ed| ed.doc().git_base.is_some());
        feed(&mut ed, "jmiwcTWO<esc>");
        settle_until(&mut ed, |ed| !ed.doc().git_hunks.is_empty());
        // (the tab keeps the keys after its quick key — esc gives them back to the code: #50)
        feed(&mut ed, " gd");
        assert_eq!(text(&ed), "one\ntwo\nthree\n", "reset (the hunk's index lines back)");
        feed(&mut ed, "<esc>u");
        settle_until(&mut ed, |ed| ed.doc().git_hunks.len() == 1);
        feed(&mut ed, " gs");
        settle_until(&mut ed, |ed| ed.doc().git_hunks.is_empty());
        let staged = String::from_utf8(git(&["diff", "--cached"])).unwrap();
        assert!(staged.contains("-two") && staged.contains("+TWO"), "{staged}");
        feed(&mut ed, "<esc>ggOnew<esc>"); // blame is on by default
        ed.blame_schedule(); // the event loop does this after every event
        settle_until(&mut ed, |ed| ed.blame.of.is_some_and(|(_, v)| v == ed.doc().version()));
        assert_eq!(ed.blame_here(0).as_deref(), Some("not committed yet"));
        assert!(
            ed.blame_here(1).is_some_and(|b| b.starts_with("Kim, ") && b.ends_with("· first")),
            "{:?}",
            ed.blame_here(1)
        );
        // Changed files: each file's diff against HEAD is the preview, +N −M on the right; Enter opens it
        // at its first change
        feed(&mut ed, ":w<ret>");
        ed.flush_saves(); // (saving is in the background: on disk before git reads it)
        ed.git_changed_files_in(dir.clone());
        settle_until(&mut ed, |ed| ed.picker.is_some());
        let p = ed.picker.as_ref().unwrap();
        assert_eq!((p.items()[0].label.as_str(), p.items()[0].hint.as_str()), ("a.txt", "+2 −1"));
        assert_eq!(ed.session.changed_files[0].line, 0, "the first change");
        assert!(!ed.session.changed_files[0].diff[0].lines.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Blame waits for the text to stand still: a change during the wait puts it off, so the first blame
    /// is of the last version — not one per pause-less stretch of typing.
    #[test]
    fn blame_waits_for_typing_to_stop() {
        let dir = std::env::temp_dir().join(format!("tarae-blame-wait-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(&dir).args(args).output().expect("git").stdout
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Kim"]);
        git(&["config", "user.email", "kim@example.com"]);
        let file = dir.join("a.txt");
        std::fs::write(&file, "one\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-qm", "first"]);
        let mut ed = editor("");
        ed.session.root = dir.clone(); // (a file's git is the session's repository's — `gitrepo`)
        ed.git_auto = true;
        ed.open(&file).unwrap();
        settle_until(&mut ed, |ed| ed.doc().git_base.is_some());
        feed(&mut ed, "ix");
        ed.blame_schedule(); // (the event loop does this after every event)
        std::thread::sleep(std::time::Duration::from_millis(100));
        feed(&mut ed, "y<esc>"); // typed during the wait
        let last = ed.doc().version();
        settle_until(&mut ed, |ed| ed.blame.of.is_some());
        assert_eq!(ed.blame.of.map(|(_, v)| v), Some(last), "blamed a version typed over");
        assert_eq!(ed.blame_here(0).as_deref(), Some("not committed yet"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file in no session's repository, where no sandboxed run can write (here under this crate's `target/`:
    /// not the session's checkout, not a temp folder, no grant): its own repository, found from its folder —
    /// its gutter as anywhere else.
    #[test]
    fn a_file_outside_every_session_and_write_place_gets_its_gutter() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join(format!("target/tarae-outside-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let file = dir.join("sub/a.txt");
        std::fs::write(&file, "one\n").unwrap();
        for args in [&["init", "-q"][..], &["add", "sub/a.txt"]] {
            let ok = std::process::Command::new("git").args(args).current_dir(&dir).output().unwrap();
            assert!(ok.status.success(), "git {args:?}");
        }
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let mut ed = editor("");
        ed.sandbox = crate::sandbox::Availability::Ready;
        ed.git_auto = true;
        assert!(!file.starts_with(&ed.session.root));
        ed.open(&file).unwrap();
        settle_until(&mut ed, |ed| ed.doc().git_base.is_some());
        assert_eq!(ed.doc().git_base.as_deref().map(String::as_str), Some("one\n"));
        settle_until(&mut ed, |ed| !ed.doc().git_hunks.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A file in no session's repository where a sandboxed run can write (the temp folder), a git directory's
    /// layout planted around it: no git read there — and the status line says why, once.
    #[test]
    fn a_file_where_commands_write_gets_no_git_and_is_told_once() {
        let scratch = crate::scratch::Scratch::new("planted-outside");
        let dir = scratch.to_path_buf();
        let sub = dir.join("sub");
        std::fs::create_dir_all(sub.join("objects")).unwrap();
        std::fs::create_dir_all(sub.join("refs")).unwrap();
        std::fs::write(sub.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        std::fs::write(sub.join("config"), "[core]\n\tbare = true\n").unwrap();
        let file = sub.join("a.txt");
        std::fs::write(&file, "one\n").unwrap();
        let root = std::env::temp_dir().join(format!("tarae-planted-root-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut ed = editor("");
        ed.session.root = std::fs::canonicalize(&root).unwrap();
        ed.sandbox = crate::sandbox::Availability::Ready;
        ed.git_auto = true;
        ed.open(&file).unwrap();
        let why = "not in this session's repository — tarae doesn't read a git folder from where commands can write";
        let told = |ed: &Editor| ed.messages.iter().filter(|m| m.2.contains(why)).count();
        settle_until(&mut ed, |ed| told(ed) > 0);
        assert!(ed.doc().git_base.is_none());
        // Asked again (focus regained): not said again
        let read = ed.git_bases_read;
        ed.git_reload_bases();
        ed.git_reload_bases();
        settle_until(&mut ed, |ed| ed.git_bases_read >= read + 2);
        assert_eq!(told(&ed), 1, "once");
        assert!(ed.doc().git_base.is_none());
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&root).ok();
    }

    /// Runs not sandboxed (`llm.sandbox` off): a run can write anywhere and run git itself, so holding git
    /// back protects nothing — a file in no session's repository, in the temp folder here, gets its own,
    /// found from its folder, and nothing is said.
    #[test]
    fn unsandboxed_runs_leave_files_outside_the_sessions_their_git() {
        let scratch = crate::scratch::Scratch::new("unsandboxed");
        let dir = scratch.to_path_buf();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let file = dir.join("sub/a.txt");
        std::fs::write(&file, "one\n").unwrap();
        for args in [&["init", "-q"][..], &["add", "sub/a.txt"]] {
            let ok = std::process::Command::new("git").args(args).current_dir(&dir).output().unwrap();
            assert!(ok.status.success(), "git {args:?}");
        }
        std::fs::write(&file, "one\ntwo\n").unwrap();
        let root = std::env::temp_dir().join(format!("tarae-unsandboxed-root-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut ed = editor("");
        ed.session.root = std::fs::canonicalize(&root).unwrap();
        // (a sandbox here, the setting off — not the tests' Bypass, which is unsandboxed already)
        ed.sandbox = crate::sandbox::Availability::Ready;
        ed.config.llm.sandbox = false;
        ed.git_auto = true;
        ed.open(&file).unwrap();
        settle_until(&mut ed, |ed| ed.git_bases_read > 0);
        assert_eq!(ed.doc().git_base.as_deref().map(String::as_str), Some("one\n"));
        assert!(!ed.messages.iter().any(|m| m.2.contains(crate::gitrepo::NOT_READ)), "nothing said");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn chat_streams_keeps_one_process_and_applies_code() {
        let dir = std::env::temp_dir().join(format!("tarae-chat-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("stdin.log");
        let delta = |t: &str| {
            serde_json::json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": t}}}).to_string()
        };
        let result = serde_json::json!({"type": "result", "result": "done"}).to_string();
        // Fake claude: logs received lines; per line two chunks + a result — one process takes several turns
        let script = format!(
            "while read -r line; do case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac; printf '%s\\n' \"$line\" >> '{}'; printf '%s\\n' '{}' '{}' '{}'; done",
            log.display(),
            delta("Here:\n```\nBAR"),
            delta("\n```\n"),
            result
        );
        let mut ed = editor("foo bar");
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), script];
        feed(&mut ed, "ww<space>l");
        assert!(ed.session.chat_has_keys(), "space l = open and focus");
        feed(&mut ed, "explain<ret>");
        let c = ed.session.chat.as_ref().unwrap();
        assert_eq!(c.msgs[0].text, "explain");
        assert!(c.busy() && c.input.is_empty());
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let c = ed.session.chat.as_ref().unwrap();
        assert_eq!(c.msgs[1].text, "Here:\n```\nBAR\n```\n", "chunks are concatenated");
        assert_eq!(c.last_code().as_deref(), Some("BAR\n"));
        // Second turn: same process; the same file version isn't resent in full
        feed(&mut ed, "again<ret>");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let sent = std::fs::read_to_string(&log).unwrap();
        let turns: Vec<String> = sent
            .lines()
            .map(|l| {
                serde_json::from_str::<serde_json::Value>(l).unwrap()["message"]["content"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(turns.len(), 2, "one process received both turns");
        assert!(turns[0].contains("<editor-context>") && turns[0].contains("<file-content>"));
        assert!(
            turns[0].contains("<selection lines=\"1-1\">\nbar\n</selection>"),
            "selection as context: {}",
            turns[0]
        );
        assert!(!turns[1].contains("<file-content>"), "same version only once");
        // C-r: review replacing the selection with the last code block → y
        feed(&mut ed, "<C-r>");
        assert!(ed.session.review.as_ref().is_some_and(|r| r.ready));
        feed(&mut ed, "y");
        assert_eq!(text(&ed), "foo BAR");
        // esc = its view mode, esc again = back to the code (pane stays), space L = close
        feed(&mut ed, "<space>l<esc><esc>");
        assert!(ed.session.chat.is_some() && !ed.session.is_at(At::Chat));
        feed(&mut ed, "<space>L");
        assert!(ed.session.chat.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Claude reads other files itself: its tool calls become rows, open files (and unsaved text) go in the
    /// context, and `C-g` walks the places it pointed at — newest first — while the chat keeps focus.
    #[test]
    fn chat_explores_the_project() {
        let dir = std::env::temp_dir().join(format!("tarae-chat-explore-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let (main, lib) = (dir.join("src/main.rs"), dir.join("src/lib.rs"));
        std::fs::write(&main, "fn main() {}\n").unwrap();
        std::fs::write(&lib, "pub fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n").unwrap();
        let lib = std::fs::canonicalize(&lib).unwrap();
        let log = dir.join("stdin.log");
        let tool = serde_json::json!({"type": "assistant", "message": {"content": [
            {"type": "tool_use", "name": "Read", "input": {"file_path": lib, "offset": 2, "limit": 2}}]}});
        let delta = serde_json::json!({"type": "stream_event", "event": {"type": "content_block_delta",
            "delta": {"type": "text_delta", "text": format!("Defined at {}:1.", lib.display())}}});
        let result = serde_json::json!({"type": "result", "result": "done"});
        let script = format!(
            "while read -r line; do case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac; printf '%s\\n' \"$line\" >> '{}'; printf '%s\\n' '{tool}' '{delta}' '{result}'; done",
            log.display()
        );
        let mut ed = editor("");
        ed.open(&lib).unwrap();
        feed(&mut ed, "iX<esc>");
        ed.open(&main).unwrap();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), script];
        feed(&mut ed, "<space>lwhere is add?<ret>");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let c = ed.session.chat.as_ref().unwrap();
        let roles: Vec<_> = c.msgs.iter().map(|m| m.role).collect();
        use crate::chat::Role;
        assert_eq!(roles, [Role::User, Role::Tool, Role::Assistant, Role::Note]);
        assert_eq!(c.msgs[1].look.as_ref().unwrap().range, "L2–3");
        let sent = std::fs::read_to_string(&log).unwrap();
        assert!(sent.contains("<open-files>") && sent.contains("lib.rs (unsaved)"), "{sent}");
        assert!(sent.contains("<unsaved-file"), "unsaved text rides along: {sent}");
        // C-g: the answer's reference, then the read before it — the chat keeps the keys
        feed(&mut ed, "<C-g>");
        assert_eq!(ed.doc().path.as_deref(), Some(lib.as_path()));
        assert_eq!(ed.doc().text.byte_to_line(ed.doc().selection().primary().cursor(&ed.doc().text)), 0);
        feed(&mut ed, "<C-g>");
        assert_eq!(ed.doc().text.byte_to_line(ed.doc().selection().primary().cursor(&ed.doc().text)), 1);
        assert!(ed.session.is_at(crate::session::At::Chat));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Counting which features get used keeps nothing of what they were used on: a search, a `:` line's words, a
    /// file opened, the paths Claude read, a tool name it made up — none of it is in the counts, the file or the
    /// report (`features.rs`). The marker stands in for any of it.
    #[test]
    fn feature_counts_hold_no_content() {
        use serde_json::json;
        let scratch = crate::scratch::Scratch::new("features-content");
        let dir = scratch.to_path_buf();
        let marker = "zqxmarker";
        let path = dir.join(crate::features::FILE);
        let mut ed = editor("one zqxmarker two\n");
        ed.session.root = dir.clone();
        ed.config.features_count = true;
        ed.features.path = Some(path.clone());
        let tool = |id: &str, name: &str| {
            let input = json!({ "file_path": dir.join(format!("{marker}.txt")) });
            json!({"type": "assistant", "message": {"content": [
                {"type": "tool_use", "id": id, "name": name, "input": input}]}})
        };
        fake_claude_stream(
            &mut ed,
            &[
                tool("t1", "Read"),
                tool("t2", &format!("mcp__tarae__{marker}")),
                json!({"type": "result", "result": "done"}),
            ],
        );
        let file = dir.join(format!("{marker}.txt"));
        feed(&mut ed, &format!("/{marker}<ret>:{marker}-cmd {marker}<ret>:vs {}<ret>", file.display()));
        feed(&mut ed, &format!("<space>l{marker}<ret>"));
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let pending = ed.features.pending();
        let n = |k: &str| pending.iter().find(|(p, _)| p == k).map_or(0, |(_, c)| c.n);
        assert_eq!((n("claude:Read"), n("claude:other"), n("typed:vsplit")), (1, 1, 1), "{pending:?}");
        assert!(pending.iter().all(|(k, _)| !k.contains(marker)), "{pending:?}");
        let universe = crate::features::Universe::of(&ed);
        let r = crate::features::Report::new(true, &universe, &Default::default(), &pending, ed.features.day);
        let (text, json) = (r.markdown(), r.json().to_string());
        ed.features_flush();
        let saved = std::fs::read_to_string(&path).expect("written at the end");
        for (what, said) in [("the file", &saved), ("the report", &text), ("the JSON", &json)] {
            assert!(!said.contains(marker), "{what} holds the marker:\n{said}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Fake claude that answers every line with `events` (JSON lines; `sleep N` entries pause).
    fn fake_claude_stream(ed: &mut Editor, events: &[serde_json::Value]) {
        let body: Vec<String> = events
            .iter()
            .map(|e| match e.as_str() {
                Some(cmd) => cmd.to_string(),
                None => format!("printf '%s\\n' '{e}'"),
            })
            .collect();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac; {}; done",
                body.join("; ")
            ),
        ];
    }

    /// A turn of the recorded stream (`usage_stream.jsonl`, its first) answered by a fake claude: the line
    /// under the chat says the cache warm for the hour its 1h writes got, the context against the window
    /// its result named, the turn's cost and its counts; `:usage` lists the turn, the total, what each
    /// request starts with and today's total; the state carries the same. The cost is said once in each
    /// place to be the API price by Claude Code's own table (#161).
    #[test]
    fn chat_usage_shows_under_the_chat_and_in_its_view() {
        let conv = "cda055cc-67c1-4da7-9120-68435b027d4f";
        let mut events = vec![serde_json::json!({"type": "system", "subtype": "init", "session_id": conv,
            "model": "m", "claude_code_version": "2.1.288"})];
        for line in include_str!("usage_stream.jsonl").lines() {
            events.push(serde_json::from_str(line).unwrap());
            if line.contains("total_cost_usd") {
                break;
            }
        }
        let mut ed = editor("");
        fake_claude_stream(&mut ed, &events);
        feed(&mut ed, "<space>lhi<ret>");
        settle_until(&mut ed, |ed| {
            !ed.session.chat.as_ref().unwrap().busy() && ed.session.usage.turns.len() == 1
        });
        let rows = |ed: &mut Editor, w: u16| -> Vec<String> {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, w, 40).unwrap();
            let mut vt = vt100::Parser::new(40, w, 0);
            vt.process(&bytes);
            vt.screen().rows(0, w).collect()
        };
        // (the last request: 8 in + 7,924 read + 201 written, of haiku's 200k; warm for the hour its 1h writes
        // got, said by the clock)
        let at = crate::usage::until(ed.session.usage.cold_at().unwrap());
        let wide = rows(&mut ed, 240);
        let footer = wide.iter().find(|r| r.contains("● warm")).unwrap_or_else(|| panic!("{wide:#?}"));
        let long = format!(
            "● warm until {at}  ctx 4% of 200k  turn ≈ API $0.018  read 7.9k write 8.1k in 18 out 185"
        );
        assert!(footer.contains(&long), "{footer:?}");
        assert!(
            wide.iter().position(|r| r.contains("● warm")) > wide.iter().position(|r| r.contains("enter"))
        );
        // Narrower, the words fold first
        let narrow = rows(&mut ed, 160);
        let footer = narrow.iter().find(|r| r.contains("● warm")).unwrap_or_else(|| panic!("{narrow:#?}"));
        assert!(footer.contains(&format!("● warm until {at}  ctx 4%  ≈$0.018")), "{footer:?}");
        assert!(!footer.contains("of 200k") && !footer.contains("turn "), "{footer:?}");
        // (from the chat's input: `esc` its view mode, `esc` the code)
        feed(&mut ed, "<esc><esc>:usage<ret>");
        assert_eq!(ed.doc().title.as_deref(), Some("usage"));
        let text = ed.doc().text.to_string();
        for part in [
            "\n\nCosts are the API price, per Claude Code 2.1.288's own table — on a subscription, not what you \
             pay.\n\n## Plan limits",
            "\n## Turns\n",
            "| time  | model     | input | cache write | cache read | output | hit | API price |",
            "| haiku 4.5 |    18 |        8.1k |       7.9k |    185 | 49% |    $0.018 |",
            "| total | 1 turn    |    18 |        8.1k |       7.9k |    185 | 49% |    $0.018 |",
            "## Every request starts with\n\n7.9k tokens (cache write 7.9k · cache read 0) — what this",
            "Every session: $0.018 in 1 turn",
        ] {
            assert!(text.contains(part), "{part:?} in\n{text}");
        }
        let cache = format!("cache warm until {at} (1h writes) · context 8.1k of 200k (4%)");
        assert!(text.contains(&cache), "{cache:?} in\n{text}");
        let s = ed.ui_state();
        let u = s.sessions[0].usage.as_ref().expect("the session's usage");
        assert_eq!((u.cache, u.ttl, u.context, u.window), ("warm", Some("1h"), Some(8133), Some(200_000)));
        assert!(u.seconds_left.is_some_and(|s| s > 3590), "{u:?}");
        assert!(s.text().contains("usage s0: cache warm 60m (1h) · context 4% of 200k"), "{}", s.text());
        assert!(s.text().contains("total $0.018 in 1 turn · ≈ API price"), "{}", s.text());
        // The session list: its total, said as what it is
        let shown =
            crate::worktree::Shown::claude_of(&ed.session.usage, &Default::default(), crate::usage::now_ms());
        assert_eq!(shown.map(|s| s.1).as_deref(), Some("≈ API $0.018"));
    }

    fn follow_project(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tarae-follow-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let lines = |n: usize| (1..=n).map(|i| format!("line {i}\n")).collect::<String>();
        for (f, n) in [("main.rs", 3), ("util.rs", 5), ("lib.rs", 10)] {
            std::fs::write(dir.join(f), lines(n)).unwrap();
        }
        (dir.clone(), dir.join("main.rs"), dir.join("util.rs"), dir.join("lib.rs"))
    }

    fn cursor_line(ed: &Editor) -> usize {
        ed.doc().text.byte_to_line(ed.doc().selection().primary().cursor(&ed.doc().text))
    }

    /// The line follow keeps in sight (collab C1: follow moves the view, never your cursor) — `usize::MAX` none.
    fn led_line(ed: &Editor) -> usize {
        crate::follow::view_line(ed).unwrap_or(usize::MAX)
    }

    /// Follow stays in the project: a file Claude reads outside it (its memory, a temp file) isn't opened —
    /// the editor stays where it was, the card still says what it reads — and a place outside named in the
    /// answer doesn't go into the list. Inside, one pane is reused: the panes stay as they were (U5 said
    /// "splits pile up" — measured: one pane stays one, two stay two). Harmless: a marker file in the
    /// test's own temp folder.
    #[test]
    fn chat_follow_never_opens_files_outside_the_project() {
        use serde_json::json;
        let (dir, main, util, lib) = follow_project("outside");
        let out = std::env::temp_dir().join(format!("tarae-not-the-project-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        let memo = std::fs::canonicalize(&out).unwrap().join("memo.md");
        std::fs::write(&memo, "# marker\n").unwrap();
        let tool = |id: &str, name: &str, input: serde_json::Value| json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}});
        let text = |t: &str| json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": t}}});
        let mut ed = editor("");
        ed.session.root = dir.clone();
        ed.open(&main).unwrap();
        ed.split_view(crate::split::Dir::Vertical);
        fake_claude_stream(
            &mut ed,
            &[
                tool("r1", "Read", json!({"file_path": util})),
                json!("sleep 0.2"),
                tool("r2", "Read", json!({"file_path": memo})),
                json!("sleep 0.2"),
                text(&format!("See {}:1 and {}:2 and {}:3.", memo.display(), lib.display(), util.display())),
                json!({"type": "result", "result": "done"}),
            ],
        );
        feed(&mut ed, "<space>lwhere?<ret>");
        // At the outside read: the card says it, the editor stays on util.rs
        settle_until(&mut ed, |ed| {
            ed.session
                .chat
                .as_ref()
                .unwrap()
                .follow
                .gaze
                .as_ref()
                .is_some_and(|g| g.path.as_deref() == Some(&*memo))
        });
        assert_eq!(ed.doc().path.as_deref(), Some(util.as_path()), "not opened: stays where it was");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(!ed.docs.iter().any(|d| d.path.as_deref() == Some(memo.as_path())), "never opened");
        assert_eq!(ed.session.views.len(), 2, "the panes as they were");
        let listed: Vec<PathBuf> = ed
            .session
            .qlist
            .as_ref()
            .map(|l| l.items.iter().map(|i| i.path.clone()).collect())
            .unwrap_or_default();
        assert!(!listed.contains(&memo), "not in the list: {listed:?}");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&out).ok();
    }

    /// Follow mode: the editor goes to each file Claude reads, to a search's first hit, and at the end to
    /// the answer's first reference; files it opened on the way close; C-o returns to where you were.
    #[test]
    fn chat_follow_goes_where_claude_reads() {
        use serde_json::json;
        let (dir, main, util, lib) = follow_project("go");
        let delta = |kind: &str, key: &str, t: &str| {
            let mut d = json!({"type": kind});
            d[key] = json!(t);
            json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": d}})
        };
        let tool = |id: &str, name: &str, input: serde_json::Value| json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}});
        let result = |id: &str, text: String| json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": text}]}});
        let mut ed = editor("");
        ed.open(&main).unwrap();
        fake_claude_stream(
            &mut ed,
            &[
                delta("thinking_delta", "thinking", "Checking the helpers first."),
                tool("r1", "Read", json!({"file_path": util})),
                tool("g1", "Grep", json!({"pattern": "line 3", "output_mode": "content"})),
                result("g1", format!("{}:3:line 3", lib.display())),
                delta("text_delta", "text", &format!("Defined at {}:5.", lib.display())),
                json!({"type": "result", "result": "done"}),
            ],
        );
        feed(&mut ed, "<space>lwhere?<ret>");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let c = ed.session.chat.as_ref().unwrap();
        assert!(c.msgs.iter().any(|m| m.role == crate::chat::Role::Thought && m.text.contains("helpers")));
        assert!(c.follow.gaze.is_none(), "the gaze ends with the turn");
        assert_eq!(ed.doc().path.as_deref(), Some(lib.as_path()), "at the answer's reference");
        assert_eq!(led_line(&ed), 4, "the view held there");
        assert_eq!(cursor_line(&ed), 0, "your cursor never led");
        assert!(!ed.docs.iter().any(|d| d.path.as_deref() == Some(util.as_path())), "the preview closed");
        assert!(ed.session.is_at(crate::session::At::Chat), "the chat keeps the keys");
        feed(&mut ed, "<esc><esc><C-o>");
        assert_eq!(ed.doc().path.as_deref(), Some(main.as_path()), "one jump back = before the turn");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Follow shows the code Claude talks about: a `path:start-end` as it's written (washed); an answer's
    /// words move the editor at most every 1.5 s (a `name` right after a place is only recorded — found at
    /// its definition in a file open here, its outline worked out on a worker). The turn ends on the answer's
    /// first reference, the places in the list.
    #[test]
    fn chat_follow_shows_what_claude_talks_about() {
        use serde_json::json;
        let (dir, _, _, lib) = follow_project("flow");
        let wind = dir.join("wind.rs");
        std::fs::write(
            &wind,
            "struct Skein;\n\nimpl Skein {\n    fn wind(&self) {\n        let _ = 1;\n    }\n}\n",
        )
        .unwrap();
        let text = |t: &str| json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": t}}});
        let mut ed = editor("");
        ed.open(&wind).unwrap();
        ed.handle_event(crate::event::Event::Resize); // (every event schedules parses)
        let rust = crate::syntax::Loader::global().load(crate::syntax::spec("rust").unwrap()).is_ok();
        settle_until(&mut ed, |ed| !rust || ed.doc().syntax.as_ref().is_some_and(|s| s.tree.is_some()));
        fake_claude_stream(
            &mut ed,
            &[
                text(&format!("It starts at {}:2-4, ", lib.display())),
                json!("sleep 0.3"),
                text("then `Skein::wind` winds it. "),
                json!("sleep 0.3"),
                json!({"type": "result", "result": "done"}),
            ],
        );
        feed(&mut ed, "<space>lhow?<ret>");
        // The place, washed, while it writes
        settle_until(&mut ed, |ed| {
            ed.session.chat.as_ref().unwrap().follow.gaze.as_ref().is_some_and(|g| g.lines == Some(1..4))
        });
        assert_eq!((ed.doc().path.as_deref(), led_line(&ed)), (Some(lib.as_path()), 1));
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert_eq!((ed.doc().path.as_deref(), led_line(&ed)), (Some(lib.as_path()), 1), "the answer's first");
        if rust {
            settle_until(&mut ed, |ed| ed.session.qlist.is_some());
            let list = ed.session.qlist.as_ref().unwrap();
            assert_eq!((list.items[1].path.clone(), list.items[1].line), (wind.clone(), 3), "`Skein::wind`");
            assert!(list.items[0].message.starts_with("It starts at"), "what it said there");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A walk-through written into the answer (`<show at="…">why</show>`) plays stop by stop — each with its
    /// why and "k/n" — also after the answer is done; what it merely mentions doesn't pull the editor away;
    /// the stops go into the list, marked where the tour is; scrolling the chat (reading) stops it.
    #[test]
    fn chat_follow_plays_a_walk_through() {
        use serde_json::json;
        let (dir, main, util, lib) = follow_project("tour");
        let text = |t: &str| json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": t}}});
        let tag =
            |p: &PathBuf, at: &str, why: &str| format!("<show at=\"{}:{at}\">{why}</show>\n", p.display());
        let answer = format!(
            "The flow.\n{}It begins here (see also {}:1).\n{}Then the helpers.\n{}And the end.\n",
            tag(&main, "1-2", "where it begins"),
            lib.display(),
            tag(&util, "2-4", "the helpers"),
            tag(&lib, "8-9", "where it ends")
        );
        let mut ed = editor("");
        ed.open(&main).unwrap();
        fake_claude_stream(&mut ed, &[text(&answer), json!({"type": "result", "result": "done"})]);
        feed(&mut ed, "<space>lhow?<ret>");
        let showing = |ed: &Editor| {
            ed.session
                .chat
                .as_ref()
                .unwrap()
                .follow
                .gaze
                .as_ref()
                .and_then(|g| Some((g.why.clone()?, g.step?)))
        };
        settle_until(&mut ed, |ed| showing(ed).is_some_and(|s| s.1.0 == 1));
        assert_eq!(showing(&ed), Some(("where it begins".into(), (1, 3))));
        // The list it leaves says itself only once the tour is over (its card has the screen till then)
        let said = |ed: &Editor| ed.status.as_ref().is_some_and(|(m, _)| m.contains("in the list"));
        assert!(!said(&ed), "{:?}", ed.status);
        assert_eq!((ed.doc().path.as_deref(), led_line(&ed)), (Some(main.as_path()), 0));
        settle_until(&mut ed, |ed| showing(ed).is_some_and(|s| s.1.0 == 3));
        assert_eq!(
            (ed.doc().path.as_deref(), led_line(&ed)),
            (Some(lib.as_path()), 7),
            "never lib:1 (a mention)"
        );
        settle_until(&mut ed, |ed| {
            !ed.session.chat.as_ref().unwrap().busy() && !ed.session.chat.as_ref().unwrap().follow.touring()
        });
        let list = ed.session.qlist.as_ref().expect("the stops in the list");
        assert_eq!(
            list.items.iter().map(|i| i.message.as_str()).collect::<Vec<_>>(),
            ["where it begins", "the helpers", "where it ends"]
        );
        // The chat shows each tag as a link line
        let shown = crate::chat::displayed(
            &ed.session
                .chat
                .as_ref()
                .unwrap()
                .msgs
                .iter()
                .rev()
                .find(|m| m.role == crate::chat::Role::Assistant)
                .unwrap()
                .text,
        )
        .into_owned();
        assert!(shown.contains("▸ ") && !shown.contains("<show"), "{shown}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Every message says what the user is looking at: the lines on screen (as the last frame drew them), and
    /// another file in a pane beside it.
    #[test]
    fn the_chat_knows_what_is_on_screen() {
        let (dir, _, util, lib) = follow_project("screen");
        let log = dir.join("stdin.log");
        let mut ed = editor("");
        ed.open(&lib).unwrap();
        feed(&mut ed, "<C-w>v");
        ed.open(&util).unwrap();
        assert_eq!(ed.session.views.len(), 2);
        let rect = crate::split::Rect { x: 0, y: 0, w: 40, h: 10 };
        let rows = |lines: std::ops::Range<usize>| lines.map(|l| (l, 0, 0, usize::MAX)).collect::<Vec<_>>();
        let other = ed.session.views.iter().find(|v| v.id != ed.session.focus).unwrap().id;
        ed.screen.panes = vec![(other, rect, 3, rows(4..9)), (ed.session.focus, rect, 3, rows(1..4))];
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), format!("cat > '{}'", log.display())];
        feed(&mut ed, "<space>lwhat is this?<ret>");
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let sent = loop {
            let sent = std::fs::read_to_string(&log).unwrap_or_default();
            if sent.contains("what is this?") || std::time::Instant::now() > end {
                break sent;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        assert!(sent.contains("on screen: lines 2-4\\n"), "{sent}");
        assert!(sent.contains("also on screen: ") && sent.contains("lib.rs lines 5-9\\n"), "{sent}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// At a walk-through's stop, `space n` asks about the stop: the note takes its lines, and Claude hears
    /// what it said they do. The card says so.
    #[test]
    fn asking_at_a_walk_through_stop() {
        use serde_json::json;
        let (dir, main, _, lib) = follow_project("stop-ask");
        let log = dir.join("stdin.log");
        let text = |t: &str| json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": t}}});
        let answer = format!(
            "<show at=\"{}:1-2\">where it begins</show>\nFirst.\n<show at=\"{}:8-9\">where it ends</show>\nLast.\n",
            main.display(),
            lib.display()
        );
        let events = [text(&answer), json!({"type": "result", "result": "done"})];
        let body: Vec<String> = events.iter().map(|e| format!("printf '%s\\n' '{e}'")).collect();
        let script = format!(
            "while read -r line; do printf '%s\\n' \"$line\" >> '{}'; case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac; {}; done",
            log.display(),
            body.join("; ")
        );
        let mut ed = editor("");
        ed.open(&main).unwrap();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), script];
        feed(&mut ed, "<space>lhow?<ret>");
        settle_until(&mut ed, |ed| {
            let c = ed.session.chat.as_ref().unwrap();
            !c.busy() && !c.follow.touring() && c.follow.gaze.as_ref().is_some_and(|g| g.step == Some((2, 2)))
        });
        assert_eq!((ed.doc().path.as_deref(), led_line(&ed)), (Some(lib.as_path()), 7));
        assert!(crate::follow::stop_here(&ed).is_some(), "the stop is here");
        let said =
            ed.status.as_ref().is_some_and(|(m, _)| m.contains("Claude's walk-through: 2 in the list"));
        assert!(said, "the tour over, the list says itself: {:?}", ed.status);
        // The stop's card stays while the view is on it (the tour is over: follow keeps it in sight, your cursor
        // elsewhere) or your cursor is in it — and goes when your cursor leaves
        let card = |ed: &mut Editor| {
            crate::term::render(ed, &mut Vec::new(), 100, 30).unwrap();
            ed.screen.thought_card.get().is_some()
        };
        assert!(card(&mut ed), "the stop's card stays");
        feed(&mut ed, "<esc><esc>8G");
        // (as the loop sees each key: follow's look after it — `follow::watch`)
        ed.handle_event(crate::event::Event::Resize);
        assert!(card(&mut ed), "your cursor at the stop");
        assert!(!ed.session.chat.as_ref().unwrap().follow.active(), "your move took over");
        feed(&mut ed, "gg");
        assert!(!card(&mut ed), "gone once the cursor leaves");
        feed(&mut ed, "8G");
        assert!(card(&mut ed), "back at the stop");
        feed(&mut ed, "<esc><space>nwhy here?<ret>");
        let n = ed.session.notes.list.last().expect("a note");
        assert_eq!((n.path.as_path(), n.lines.clone()), (lib.as_path(), 7..9), "the stop's lines");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let sent = std::fs::read_to_string(&log).unwrap();
        assert!(sent.contains("why here?") && sent.contains("you said: where it ends"), "{sent}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Moving in the editor while Claude explores pauses following — it stays where the user is.
    #[test]
    fn chat_follow_pauses_when_the_user_moves() {
        use serde_json::json;
        let (dir, main, util, lib) = follow_project("pause");
        let read = |id: &str, p: &PathBuf| json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": "Read", "input": {"file_path": p}}]}});
        let mut ed = editor("");
        // (only C-f follows again here — the quiet while has its own test)
        ed.config.llm.follow_resume = 0;
        ed.open(&main).unwrap();
        fake_claude_stream(
            &mut ed,
            &[
                read("r1", &util),
                json!("sleep 0.5"),
                read("r2", &lib),
                json!({"type": "result", "result": "done"}),
            ],
        );
        feed(&mut ed, "<space>lwhere?<ret>");
        assert!(ed.session.chat.as_ref().unwrap().follow.active(), "on by default");
        settle_until(&mut ed, |ed| ed.doc().path.as_deref() == Some(util.as_path()));
        feed(&mut ed, "<esc>j");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(ed.session.chat.as_ref().unwrap().follow.paused);
        assert_eq!(ed.doc().path.as_deref(), Some(util.as_path()), "stayed with the user");
        assert_eq!(cursor_line(&ed), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Paused by a move, follow takes over again on its own a quiet while after the last key
    /// (`llm.follow-resume`) while Claude still works — back where Claude is, then on to what comes next.
    /// A key before then starts the wait over. (Claude's next read waits for the test's word, not a sleep: a
    /// slow runner could take the quiet while past a timed one, and the resume would follow that read.)
    #[test]
    fn chat_follow_resumes_after_a_quiet_while() {
        use serde_json::json;
        let (dir, main, util, lib) = follow_project("resume");
        let read = |id: &str, p: &PathBuf| json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": "Read", "input": {"file_path": p}}]}});
        let mut ed = editor("");
        // (5 "seconds" = 100 ms in tests)
        ed.config.llm.follow_resume = 5;
        ed.open(&main).unwrap();
        let go = dir.join("next-read");
        std::fs::remove_file(&go).ok();
        fake_claude_stream(
            &mut ed,
            &[
                read("r1", &util),
                // (given up 10 s on by the clock — a test that fails before its word leaves no loop behind;
                // a count of short sleeps ran past that, each `sleep` a process of its own)
                json!(format!(
                    "end=$(($(date +%s) + 10)); \
                     while [ ! -e '{}' ] && [ \"$(date +%s)\" -lt \"$end\" ]; do sleep 0.02; done",
                    go.display()
                )),
                read("r2", &lib),
                json!({"type": "result", "result": "done"}),
            ],
        );
        feed(&mut ed, "<space>lwhere?<ret>");
        settle_until(&mut ed, |ed| ed.doc().path.as_deref() == Some(util.as_path()));
        feed(&mut ed, "<esc>j");
        settle_until(&mut ed, |ed| ed.session.chat.as_ref().unwrap().follow.paused);
        // Another key 60 ms on: the wait starts over from it
        std::thread::sleep(std::time::Duration::from_millis(60));
        feed(&mut ed, "j");
        let last_key = std::time::Instant::now();
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().follow.paused);
        assert!(
            last_key.elapsed() >= std::time::Duration::from_millis(95),
            "a quiet while after the last key"
        );
        assert_eq!(
            (ed.doc().path.as_deref(), led_line(&ed)),
            (Some(util.as_path()), 0),
            "back where Claude is — its next read is still held"
        );
        assert!(ed.status.as_ref().is_some_and(|(m, _)| m.contains("following Claude again")));
        std::fs::write(&go, "").unwrap();
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert_eq!(ed.doc().path.as_deref(), Some(lib.as_path()), "and on with it");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A cursor moved with no key, click or paste since — Claude's edit reloading the open file shifts it —
    /// isn't the user: follow goes on (it used to pause there for the rest of the turn).
    #[test]
    fn a_reload_moving_the_cursor_doesnt_pause_follow() {
        use serde_json::json;
        let (dir, main, util, lib) = follow_project("reload");
        let read = |id: &str, p: &PathBuf, offset: u64| json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": "Read", "input": {"file_path": p, "offset": offset, "limit": 2}}]}});
        let mut ed = editor("");
        ed.config.llm.follow_resume = 0;
        ed.open(&main).unwrap();
        let grown = format!("new top\n{}", std::fs::read_to_string(&util).unwrap());
        fake_claude_stream(
            &mut ed,
            &[
                read("r1", &util, 4),
                json!("sleep 0.3"),
                read("r2", &lib, 1),
                json!({"type": "result", "result": "done"}),
            ],
        );
        feed(&mut ed, "<space>lwhere?<ret><esc>");
        settle_until(&mut ed, |ed| ed.doc().path.as_deref() == Some(util.as_path()));
        assert_eq!(
            (led_line(&ed), cursor_line(&ed)),
            (3, 0),
            "the view on the read, your cursor where it was"
        );
        // The file changes on disk (a line on top), and the cursor moves with no key of yours
        std::fs::write(&util, &grown).unwrap();
        ed.disk_take(&util, &ropey::Rope::from_str(&grown), crate::disk::stamp(&util));
        ed.doc_mut().set_selection(crate::selection::Selection::point(5));
        ed.handle_event(crate::event::Event::Resize);
        assert!(!ed.session.chat.as_ref().unwrap().follow.paused, "a cursor moved with no key isn't you");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(!ed.session.chat.as_ref().unwrap().follow.paused, "not the user");
        assert_eq!(ed.doc().path.as_deref(), Some(lib.as_path()), "followed on");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The line follow held in the last turn isn't the next question's: asked again, the view stays where it is (no
    /// scroll back to the last turn's line) until Claude reads somewhere.
    #[test]
    fn the_next_question_doesnt_take_the_view_back_to_the_last_turns_line() {
        use serde_json::json;
        let (dir, _, util, _) = follow_project("twice");
        let read = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "r1", "name": "Read", "input": {"file_path": util, "offset": 4, "limit": 2}}]}});
        let mut ed = editor("");
        ed.open(&util).unwrap();
        fake_claude_stream(
            &mut ed,
            &[json!("sleep 0.3"), read, json!("sleep 0.3"), json!({"type": "result", "result": "done"})],
        );
        feed(&mut ed, "<space>lwhere?<ret>");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert_eq!(
            (led_line(&ed), cursor_line(&ed)),
            (3, 0),
            "the view held on the read past the turn's end"
        );
        feed(&mut ed, "again?<ret>");
        assert!(ed.session.chat.as_ref().unwrap().busy());
        assert_eq!(led_line(&ed), usize::MAX, "the new question: no line held yet");
        settle_until(&mut ed, |ed| led_line(ed) == 3);
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The mouse is input only where it can move the code's cursor: the wheel over the chat and a release
    /// just before a reload moves the cursor don't pause follow; the wheel over the code does.
    #[test]
    fn only_the_mouse_that_moves_the_cursor_pauses_follow() {
        use crate::event::{Mouse, MouseKind as K};
        use serde_json::json;
        let (dir, main, util, lib) = follow_project("wheel");
        let read = |id: &str, p: &PathBuf, offset: u64| json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": "Read", "input": {"file_path": p, "offset": offset, "limit": 2}}]}});
        let mouse = |kind, x| Mouse { kind, x, y: 2, alt: false, ctrl: false };
        let mut ed = editor("");
        ed.config.llm.follow_resume = 0;
        ed.open(&main).unwrap();
        let grown = format!("new top\n{}", std::fs::read_to_string(&util).unwrap());
        fake_claude_stream(
            &mut ed,
            &[
                read("r1", &util, 4),
                json!("sleep 0.3"),
                read("r2", &lib, 1),
                json!("sleep 0.8"),
                json!({"type": "result", "result": "done"}),
            ],
        );
        feed(&mut ed, "<space>lwhere?<ret><esc>");
        settle_until(&mut ed, |ed| ed.doc().path.as_deref() == Some(util.as_path()));
        // The chat from column 40, the code left of it
        ed.screen.chat_x = Some(40);
        ed.screen.panes =
            vec![(ed.session.focus, crate::split::Rect { x: 0, y: 0, w: 40, h: 10 }, 3, vec![])];
        ed.handle_mouse(mouse(K::ScrollUp, 50));
        ed.handle_mouse(mouse(K::Up, 10));
        std::fs::write(&util, &grown).unwrap();
        ed.disk_take(&util, &ropey::Rope::from_str(&grown), crate::disk::stamp(&util));
        ed.handle_event(crate::event::Event::Resize);
        assert!(
            !ed.session.chat.as_ref().unwrap().follow.paused,
            "the chat's wheel and a release aren't you"
        );
        settle_until(&mut ed, |ed| ed.doc().path.as_deref() == Some(lib.as_path()));
        // The wheel over the code is you: paused, and (as after a key) following again a quiet while later
        ed.config.llm.follow_resume = 5;
        ed.screen.panes[0].0 = ed.session.focus;
        ed.handle_mouse(mouse(K::ScrollDown, 10));
        ed.handle_event(crate::event::Event::Resize);
        assert!(ed.session.chat.as_ref().unwrap().follow.paused, "the wheel over the code is you");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().follow.paused);
        assert!(ed.session.chat.as_ref().unwrap().busy(), "resumed while Claude still works");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Switching sessions isn't taking over: while a session is behind, Claude's edits reload its files
    /// (the docs are shared — a cursor shifts), and the click that brings it back is input; it used to stay
    /// paused after that until `C-f` (nine minutes, watched live). Back in front it follows on.
    #[test]
    fn switching_sessions_doesnt_pause_follow() {
        use serde_json::json;
        let (dir, main, util, lib) = follow_project("sessions");
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let read = |id: &str, p: &PathBuf| json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": "Read", "input": {"file_path": p, "offset": 4, "limit": 2}}]}});
        let mut ed = editor("");
        ed.config.llm.follow_resume = 0;
        ed.session.root = dir.clone();
        ed.open(&main).unwrap();
        fake_claude_stream(
            &mut ed,
            &[
                read("r1", &util),
                json!("sleep 0.8"),
                read("r2", &lib),
                json!({"type": "result", "result": "done"}),
            ],
        );
        feed(&mut ed, "<space>lwhere?<ret><esc>");
        settle_until(&mut ed, |ed| ed.doc().path.as_deref() == Some(util.as_path()));
        let (a, b) = (ed.session.id, ed.session_add(other.clone()));
        // A click on the other session's tab (the person's input), then back while the file changed under the first
        (ed.input_seq, ed.person_seq) = (ed.input_seq + 1, ed.person_seq + 1);
        assert!(ed.session_switch(b));
        ed.handle_event(crate::event::Event::Resize);
        let grown = format!("new top\n{}", std::fs::read_to_string(&util).unwrap());
        std::fs::write(&util, &grown).unwrap();
        ed.disk_take(&util, &ropey::Rope::from_str(&grown), crate::disk::stamp(&util));
        (ed.input_seq, ed.person_seq) = (ed.input_seq + 1, ed.person_seq + 1);
        assert!(ed.session_switch(a));
        ed.handle_event(crate::event::Event::Resize);
        assert!(
            !ed.session.chat.as_ref().unwrap().follow.paused,
            "a session switch isn't the user taking over"
        );
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert_eq!(ed.doc().path.as_deref(), Some(lib.as_path()), "followed on");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Paused, then the session goes behind before follow's wake-up comes: back in front and quiet, follow
    /// still takes over again on its own (the wake-up that landed behind once left it waiting for the rest
    /// of the chat).
    #[test]
    fn follow_resumes_after_its_session_was_behind() {
        use serde_json::json;
        let (dir, main, util, _) = follow_project("behind");
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let read = |id: &str, p: &PathBuf| json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": "Read", "input": {"file_path": p}}]}});
        let mut ed = editor("");
        // (5 "seconds" = 100 ms in tests)
        ed.config.llm.follow_resume = 5;
        ed.session.root = dir.clone();
        ed.open(&main).unwrap();
        fake_claude_stream(
            &mut ed,
            &[read("r1", &util), json!("sleep 2"), json!({"type": "result", "result": "done"})],
        );
        feed(&mut ed, "<space>lwhere?<ret>");
        settle_until(&mut ed, |ed| ed.doc().path.as_deref() == Some(util.as_path()));
        feed(&mut ed, "<esc>j");
        settle_until(&mut ed, |ed| ed.session.chat.as_ref().unwrap().follow.paused);
        let (a, b) = (ed.session.id, ed.session_add(other.clone()));
        assert!(ed.session_switch(b));
        // The wake-up lands while it's behind: follow doesn't take over in a session you aren't in
        let end = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while let Some(left) = end.checked_duration_since(std::time::Instant::now()) {
            if let Some(ev) = ed.events.recv_timeout(left) {
                ed.handle_event(ev);
            }
        }
        assert!(ed.session_by_id(a).unwrap().chat.as_ref().unwrap().follow.paused, "not while behind");
        assert!(!ed.messages.iter().any(|m| m.2.contains("following Claude again")), "nor said in front");
        // Back in front and quiet: a quiet while from coming back, then follow again
        assert!(ed.session_switch(a));
        let back = std::time::Instant::now();
        ed.handle_event(crate::event::Event::Resize);
        assert!(ed.session.chat.as_ref().unwrap().follow.paused, "still yours right after coming back");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().follow.paused);
        assert!(back.elapsed() >= std::time::Duration::from_millis(95), "a quiet while after coming back");
        assert!(ed.session.chat.as_ref().unwrap().busy(), "taken over while Claude still works");
        assert_eq!(
            (ed.doc().path.as_deref(), led_line(&ed)),
            (Some(util.as_path()), 0),
            "back where Claude is"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The way follow failed (watched live): review at the end on, Claude reads, edits a file four times,
    /// searches and reads again. Live, the editor goes to each edit once it's written — its changed lines
    /// on the review's wash, the replaced ones above them, the card naming it — and to the search's first
    /// hit with its count; nothing skipped. Then the turn replays stop by stop with `[r`, earlier edits
    /// shown where later ones moved them, and `]r` past the newest says so.
    #[test]
    fn chat_follow_shows_each_edit_and_replays_the_turn() {
        use serde_json::json;
        let (dir, main, _, lib) = follow_project("edit-replay");
        let file = dir.join("edit.rs");
        let base: String = (1..=30).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&file, &base).unwrap();
        // The four edits: (old, new) and the file after each
        let edits = [
            ("line 3\n", "three\n"),
            ("line 10\n", "line 10\nadded a\nadded b\n"),
            ("line 20\n", ""),
            ("line 1\n", "zero\nline 1\n"),
        ];
        let mut texts = Vec::new();
        let mut text = base.clone();
        for (old, new) in edits {
            text = text.replacen(old, new, 1);
            texts.push(text.clone());
        }
        let tool = |id: &str, name: &str, input: serde_json::Value| json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}});
        let result = |id: &str, text: &str| json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": text}]}});
        let mut steps: Vec<serde_json::Value> = vec![
            tool("r1", "Read", json!({"file_path": main, "offset": 1, "limit": 3})),
            result("r1", "1\tline 1"),
            json!("sleep 0.15"),
        ];
        for (k, (old, new)) in edits.iter().enumerate() {
            let id = format!("e{k}");
            let input = json!({"file_path": file, "old_string": old, "new_string": new});
            steps.push(tool(&id, "Edit", input.clone()));
            steps.push(json!({"type": "control_request", "request_id": format!("q{k}"),
                "request": {"subtype": "can_use_tool", "tool_name": "Edit", "input": input}}));
            // (claude waits for the answer, writes the file, then says it's done)
            steps.push(json!(format!(
                "while read -r l; do case \"$l\" in *'\"request_id\":\"q{k}\"'*) break;; esac; done; printf '%s' '{}' > '{}'",
                texts[k],
                file.display()
            )));
            steps.push(result(&id, "The file has been updated."));
            steps.push(json!("sleep 0.15"));
        }
        steps.push(tool("s1", "mcp__tarae__search", json!({"pattern": "line 5"})));
        steps.push(result("s1", "1 matching line in 1 file:\nlib.rs\n    5: line 5\n"));
        steps.push(json!("sleep 0.15"));
        steps.push(tool("r2", "Read", json!({"file_path": lib, "offset": 8, "limit": 2})));
        steps.push(result("r2", "8\tline 8"));
        steps.push(json!("sleep 0.15"));
        steps.push(json!({"type": "result", "result": "done"}));
        let mut ed = editor("");
        ed.session.root = dir.clone();
        ed.config.llm.review_at_end = true;
        ed.open(&main).unwrap();
        fake_claude_stream(&mut ed, &steps);
        feed(&mut ed, "<space>lchange it<ret><esc>");
        assert!(crate::edits::at_end(&ed), "review at the end is on");
        // Live: what the editor showed, event by event — (file, cursor line, the edit's changed lines, card)
        // (the changed lines as (start, end) — each hunk's)
        type Seen = (Option<PathBuf>, usize, Option<Vec<(usize, usize)>>, String);
        let snap = |ed: &Editor| -> Seen {
            let d = ed.doc();
            let shown = d.shown.as_ref().map(|h| h.iter().map(|h| (h.lines.start, h.lines.end)).collect());
            let card = ed
                .session
                .chat
                .as_ref()
                .and_then(|c| c.follow.gaze.as_ref())
                .map_or(String::new(), |g| format!("{} {} {}", g.look.verb, g.look.what, g.look.range));
            (d.path.clone(), led_line(ed), shown, card)
        };
        let mut seen: Vec<Seen> = Vec::new();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut started = false;
        while !started || ed.session.chat.as_ref().unwrap().busy() {
            started |= ed.session.chat.as_ref().unwrap().busy();
            let left = end.saturating_duration_since(std::time::Instant::now());
            let ev = ed.events.recv_timeout(left).expect("the turn goes on");
            ed.handle_event(ev);
            let s = snap(&ed);
            if seen.last() != Some(&s) {
                seen.push(s);
            }
        }
        let at = |path: &PathBuf, line: usize, lines: Option<Vec<(usize, usize)>>, card: &str| {
            seen.iter().any(|s| s.0.as_ref() == Some(path) && s.1 == line && s.2 == lines && s.3 == card)
        };
        assert!(at(&main, 0, None, "read main.rs L1–3"), "the read: {seen:#?}");
        assert!(at(&file, 2, Some(vec![(2, 3)]), "edit edit.rs L3"), "edit 1 with its line: {seen:#?}");
        assert!(at(&file, 10, Some(vec![(10, 12)]), "edit edit.rs L11–12"), "edit 2: {seen:#?}");
        assert!(at(&file, 21, Some(vec![(21, 21)]), "edit edit.rs L22"), "edit 3 (a line gone): {seen:#?}");
        assert!(at(&file, 0, Some(vec![(0, 1)]), "edit edit.rs L1"), "edit 4: {seen:#?}");
        assert!(at(&lib, 4, None, "search line 5 1 matching line in 1 file"), "the search's hit: {seen:#?}");
        assert!(at(&lib, 7, None, "read lib.rs L8–9"), "the last read: {seen:#?}");
        // Each edit's replaced lines are what the review shows above them
        let c = ed.session.chat.as_ref().unwrap();
        assert!(!c.follow.paused, "never paused — no key was pressed");
        let stops: Vec<String> = c
            .follow
            .replay
            .turns
            .back()
            .unwrap()
            .stops
            .iter()
            .map(|s| format!("{} {}", s.look.verb, s.look.range))
            .collect();
        assert_eq!(
            stops,
            [
                "read L1–3",
                "edit L4",
                "edit L12–13",
                "edit L23",
                "edit L1",
                "search 1 matching line in 1 file",
                "read L8–9"
            ],
            "every stop, in order — earlier edits where the later ones moved them"
        );
        // Replay, stop by stop: back from the newest
        let card = |ed: &mut Editor| {
            let mut out = Vec::new();
            crate::term::render(ed, &mut out, 120, 40).unwrap();
            let mut vt = vt100::Parser::new(40, 120, 0);
            vt.process(&out);
            vt.screen().contents()
        };
        feed(&mut ed, "[r");
        assert_eq!((ed.doc().path.as_deref(), cursor_line(&ed)), (Some(lib.as_path()), 7));
        feed(&mut ed, "[r");
        assert_eq!(
            (ed.doc().path.as_deref(), cursor_line(&ed)),
            (Some(lib.as_path()), 4),
            "the search's hit"
        );
        assert!(card(&mut ed).contains("search (project)"), "named as the tool");
        feed(&mut ed, "[r");
        assert_eq!((ed.doc().path.as_deref(), cursor_line(&ed)), (Some(file.as_path()), 0));
        assert_eq!(ed.doc().shown.as_ref().map(|h| h[0].lines.clone()), Some(0..1), "edit 4's line");
        feed(&mut ed, "[r[r[r");
        assert_eq!(cursor_line(&ed), 3, "edit 1, a line down since edit 4");
        assert_eq!(
            ed.doc().shown.as_ref().map(|h| (h[0].lines.clone(), h[0].removed.clone())),
            Some((3..4, vec!["line 3".to_string()]))
        );
        let screen = card(&mut ed);
        assert!(
            screen.contains("edit") && screen.contains("L4") && screen.contains("2/7"),
            "the card on it:\n{screen}"
        );
        assert!(screen.contains("−") && screen.contains("line 3"), "the replaced line above it:\n{screen}");
        feed(&mut ed, "[r");
        assert_eq!(ed.doc().path.as_deref(), Some(main.as_path()));
        assert!(ed.doc().shown.is_none() && ed.docs.iter().all(|d| d.shown.is_none()), "no edit at a read");
        feed(&mut ed, "[r");
        assert!(ed.status.as_ref().is_some_and(|(m, _)| m.contains("first stop")));
        // Moving away ends its showing; forward to the newest and past it
        feed(&mut ed, "]r");
        feed(&mut ed, "j");
        ed.handle_event(crate::event::Event::Resize);
        assert!(ed.session.chat.as_ref().unwrap().follow.replaying.is_none(), "you moved: its card goes");
        feed(&mut ed, "]r]r]r]r]r]r");
        assert!(ed.status.as_ref().is_some_and(|(m, _)| m.contains("newest stop")), "{:?}", ed.status);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Notes end to end over the control protocol: Claude pins one with the `note` tool (tarae answers the
    /// `mcp_message`), you reply in place with `space n`, Claude answers on the thread with `reply_to`.
    #[test]
    fn notes_pinned_by_claude_and_answered_in_place() {
        use serde_json::json;
        let (dir, main, _, lib) = follow_project("notes");
        let log = dir.join("stdin.log");
        let call = |id: u64, args: serde_json::Value| {
            json!({"type": "control_request", "request_id": format!("c{id}"), "request": {"subtype": "mcp_message",
                "server_name": "tarae", "message": {"jsonrpc": "2.0", "id": id, "method": "tools/call",
                "params": {"name": "note", "arguments": args}}}})
        };
        let tool = |args: serde_json::Value| json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t", "name": "mcp__tarae__note", "input": args}]}});
        let pin = json!({"path": lib, "line": 3, "text": "Line 3 repeats line 2."});
        let answer = json!({"reply_to": 1, "text": "Because the fixture counts lines."});
        let result = json!({"type": "result", "result": "done"});
        let turn = |events: &[serde_json::Value]| {
            events.iter().map(|e| format!("printf '%s\\n' '{e}'")).collect::<Vec<_>>().join("; ")
        };
        let script = format!(
            "n=0; while read -r line; do printf '%s\\n' \"$line\" >> '{}'; case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac; \
             n=$((n+1)); if [ $n = 1 ]; then {}; else {}; fi; done",
            log.display(),
            turn(&[tool(pin.clone()), call(7, pin), result.clone()]),
            turn(&[tool(answer.clone()), call(8, answer), result]),
        );
        let mut ed = editor("");
        ed.open(&main).unwrap();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), script];
        feed(&mut ed, "<space>lany notes?<ret>");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert_eq!(ed.session.notes.list.len(), 1);
        let n = &ed.session.notes.list[0];
        assert_eq!(
            (n.path.as_path(), n.lines.clone(), n.gist()),
            (lib.as_path(), 2..3, "Line 3 repeats line 2.")
        );
        // Follow went to the note: its card is under the cursor
        assert_eq!(ed.doc().path.as_deref(), Some(lib.as_path()));
        assert_eq!(crate::notes::here(&ed).map(|n| n.id), Some(1));
        // The fake logs our control_response when it reads it — wait for that
        let logged = |needle: &str| {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let sent = std::fs::read_to_string(&log).unwrap_or_default();
                if sent.contains(needle) || std::time::Instant::now() > end {
                    return sent;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        let sent = logged("note #1 pinned at");
        assert!(
            sent.lines().next().unwrap().contains("\"sdkMcpServers\":[\"tarae\"]"),
            "registered first: {sent}"
        );
        assert!(sent.contains("\"request_id\":\"c7\"") && sent.contains("note #1 pinned at"), "{sent}");
        // Reply in place — the keys stay in the editor
        feed(&mut ed, "<esc><esc><space>nwhy?<ret>");
        assert!(ed.session.notes.list[0].waiting);
        assert!(!ed.session.is_at(crate::session::At::Chat), "replying doesn't move the keys to the chat");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let n = &ed.session.notes.list[0];
        let thread: Vec<_> = n.thread.iter().map(|e| (e.by, e.text.as_str())).collect();
        use crate::notes::By;
        assert_eq!(
            thread,
            [
                (By::Author, "Line 3 repeats line 2."),
                (By::You, "why?"),
                (By::Author, "Because the fixture counts lines.")
            ]
        );
        assert!(!n.waiting);
        let sent = logged("replied on note #1");
        assert!(
            sent.contains("<note id=\\\"1\\\"") && sent.contains("claude: Line 3 repeats line 2."),
            "{sent}"
        );
        assert!(sent.contains("replied on note #1"), "{sent}");
        let c = ed.session.chat.as_ref().unwrap();
        assert!(c.msgs.iter().any(|m| m.role == crate::chat::Role::User && m.chip.starts_with("¶ #1")));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A running task's wave and wait counter ask for frames only while the bottom slot is drawn (its panel or
    /// its tab) — zoomed elsewhere, a run waiting minutes on cargo's lock wakes nothing; a frame that draws
    /// the slot again judges it by its own layout (`frame_drawn`).
    #[test]
    fn a_runs_counter_asks_for_frames_only_while_its_slot_is_drawn() {
        let mut ed = editor("");
        let task = crate::task::Task { name: "check".into(), cmd: "cargo check".into(), cwd: "/".into() };
        let mut r = crate::task::Run::for_test(task);
        r.state = crate::task::State::Running;
        ed.session.task_run = Some(r);
        ed.screen.width = Some(120);
        ed.screen.bottom_h = 12;
        assert!(ed.tick_wanted().is_some(), "in sight: its wave moves");
        ed.screen.bottom_h = 0;
        assert_eq!(ed.tick_wanted(), None, "not drawn: no frames for it");
        ed.session.task_run.as_mut().unwrap().state = crate::task::State::Passed;
        ed.screen.bottom_h = 12;
        assert_eq!(ed.tick_wanted(), None, "ended: none either");
    }

    /// Claude checks its work: `run_tests` runs the test at a line in the test pane (a real `cargo test`),
    /// the call waits for the run, the results go back (verdict, the failure with where and why), and the
    /// editor goes to the failure. The chat's row ends with the verdict.
    #[test]
    fn claude_runs_the_tests_and_reads_the_failure() {
        use serde_json::json;
        if crate::syntax::Loader::global().load(crate::syntax::spec("rust").unwrap()).is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("tarae-run-tests-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"calc\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .unwrap();
        crate::testing::cargo_builds_inside(&dir);
        let lib = dir.join("src/lib.rs");
        std::fs::write(
            &lib,
            "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    \
             #[test]\n    fn adds() {\n        assert_eq!(add(2, 2), 4);\n    }\n\n    #[test]\n    fn zero() {\n        \
             assert_eq!(add(0, 0), 0);\n    }\n}\n",
        )
        .unwrap();
        let log = dir.join("stdin.log");
        let args = json!({"path": lib, "line": 11});
        let tool = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t1", "name": "mcp__tarae__run_tests", "input": args}]}});
        let call = json!({"type": "control_request", "request_id": "c7", "request": {"subtype": "mcp_message",
            "server_name": "tarae", "message": {"jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": "run_tests", "arguments": args}}}});
        let result = json!({"type": "result", "result": "done"});
        // Like claude, the fake waits for the tool's answer before it ends the turn
        let script = format!(
            "while read -r line; do printf '%s\\n' \"$line\" >> '{log}'; case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac; \
             printf '%s\\n' '{tool}'; printf '%s\\n' '{call}'; \
             while read -r l; do printf '%s\\n' \"$l\" >> '{log}'; case \"$l\" in *'\"c7\"'*) break;; esac; done; \
             printf '%s\\n' '{result}'; done",
            log = log.display()
        );
        let mut ed = editor("");
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), script];
        feed(&mut ed, "<space>lcheck the adder<ret>");
        let end = std::time::Instant::now() + std::time::Duration::from_secs(180);
        while ed.session.chat.as_ref().unwrap().busy() {
            let left = end.saturating_duration_since(std::time::Instant::now());
            let ev = ed.events.recv_timeout(left).expect("the run ends before the timeout");
            ed.handle_event(ev);
        }
        let r = ed.session.test_run.as_ref().expect("the run shows in the test pane");
        assert_eq!(r.target.label, "tests::adds");
        assert_eq!(r.state, crate::testing::RunState::Failed(Some(101)));
        let sent = std::fs::read_to_string(&log).unwrap();
        let answer = sent.lines().find(|l| l.contains("\"c7\"")).expect("answered");
        assert!(answer.contains("failed (exit 101)") && answer.contains("tests::adds"), "{answer}");
        assert!(answer.contains("FAILED tests::adds at ") && answer.contains("src/lib.rs:11"), "{answer}");
        assert!(answer.contains("left: 0") && answer.contains("right: 4"), "the why: {answer}");
        // The view went to the failure, the card saying why
        assert_eq!((ed.doc().path.as_deref(), led_line(&ed)), (Some(lib.as_path()), 10));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `run_tests` doesn't cut a run short — the user's tests keep going and Claude's call waits its turn (not
    /// refused: `queued.rs`); a file with no runner is a tool error Claude can read.
    #[test]
    fn run_tests_waits_its_turn() {
        use serde_json::json;
        let mut ed = editor("");
        let call = |ed: &mut Editor, args: serde_json::Value| {
            let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "run_tests", "arguments": args}});
            crate::tools::mcp(ed, 0, "c1", &msg)
        };
        let text = |r: Option<serde_json::Value>| {
            let r = r.expect("answered now");
            (
                r["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string(),
                r["result"]["isError"] == true,
            )
        };
        let (t, err) = text(call(&mut ed, json!({})));
        assert!(err && t.contains("path is required"), "{t}");
        let target = crate::testing::Target {
            label: "slow".into(),
            cwd: std::env::temp_dir(),
            how: crate::testing::How::Pack {
                lang: "python".into(),
                argv: vec!["sleep".into(), "5".into()],
                debug: None,
                lines: crate::test_results::Rules::builtin("python"),
                build: None,
                debug_run: None,
                junit: None,
            },
        };
        ed.test_start(target);
        assert!(call(&mut ed, json!({"path": "src/lib.rs"})).is_none(), "not refused: it waits");
        let running = ed.session.test_run.as_ref().map(|r| (r.target.label.as_str(), r.state.clone()));
        assert_eq!(running, Some(("slow", crate::testing::RunState::Running)), "not cut short");
        ed.test_close();
        // Not a test runner's language: found on the worker, answered from there (no chat here → dropped)
        assert!(call(&mut ed, json!({"path": "README.md"})).is_none());
    }

    /// A fake `claude` that answers the first message by calling tarae's `name` tool with `args`, waits for
    /// tarae's answer (request `c7`), then ends the turn — what it was sent goes to the returned log.
    fn claude_calls(
        ed: &mut Editor,
        dir: &std::path::Path,
        name: &str,
        args: &serde_json::Value,
    ) -> std::path::PathBuf {
        use serde_json::json;
        let log = dir.join("stdin.log");
        let tool = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t1",
            "name": format!("mcp__tarae__{name}"), "input": args}]}});
        let call = json!({"type": "control_request", "request_id": "c7", "request": {"subtype": "mcp_message",
            "server_name": "tarae", "message": {"jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": name, "arguments": args}}}});
        let result = json!({"type": "result", "result": "done"});
        let script = format!(
            "while read -r line; do printf '%s\\n' \"$line\" >> '{log}'; case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac; \
             printf '%s\\n' '{tool}'; printf '%s\\n' '{call}'; \
             while read -r l; do printf '%s\\n' \"$l\" >> '{log}'; case \"$l\" in *'\"c7\"'*) break;; esac; done; \
             printf '%s\\n' '{result}'; done",
            log = log.display()
        );
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), script];
        log
    }

    /// Handles events until `done` (or panics after a minute).
    fn settle_for(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !done(ed) {
            let left = end.saturating_duration_since(std::time::Instant::now());
            let ev = ed.events.recv_timeout(left).expect("done before the timeout");
            ed.handle_event(ev);
        }
    }

    /// tarae's answer to the tool call (`c7`), as sent to claude.
    fn tool_answer(log: &std::path::Path) -> String {
        let sent = std::fs::read_to_string(log).unwrap();
        sent.lines().find(|l| l.contains("\"c7\"")).expect("answered").to_string()
    }

    /// One chat turn in which a fake claude calls tarae's `name` tool with `args` — in a fresh chat (review
    /// at the end if `at_end`) — and tarae's answer to it, once the turn is over.
    fn tool_turn(ed: &mut Editor, dir: &Path, name: &str, args: serde_json::Value, at_end: bool) -> String {
        let _ = std::fs::remove_file(dir.join("stdin.log"));
        let log = claude_calls(ed, dir, name, &args);
        crate::chat::close(ed);
        feed(ed, "<space>l");
        if at_end {
            ed.session.chat.as_mut().unwrap().edits.at_end = Some(dir.to_path_buf());
        }
        feed(ed, "do it<ret>");
        let done = |ed: &Editor| {
            ed.session.chat.as_ref().is_some_and(|c| !c.busy())
                && !ed.session.checkpoints.gathering
                && std::fs::read_to_string(dir.join("stdin.log")).is_ok_and(|s| s.contains("\"c7\""))
        };
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !done(ed) && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(50)) {
                ed.handle_event(ev);
            }
        }
        assert!(
            done(ed),
            "{:?}\n{:?}",
            std::fs::read_to_string(dir.join("stdin.log")),
            ed.session.chat.as_ref().map(|c| c.msgs.iter().map(|m| m.text.clone()).collect::<Vec<_>>())
        );
        let line: serde_json::Value = serde_json::from_str(&tool_answer(&log)).unwrap();
        let r = &line["response"]["response"]["mcp_response"]["result"];
        let text = r["content"][0]["text"].as_str().unwrap_or_default().to_string();
        if r["isError"] == true { format!("error: {text}") } else { text }
    }

    /// Claude's `replace`, through the chat: a preview lists what would change and changes nothing; past
    /// `max_files` it's refused; applied under review at the end it's written in one go — never a gitignored
    /// file, `.git`, or a case it wasn't asked for — the turn opens as a review of exactly those files, and
    /// `space u` takes it all back.
    #[test]
    fn claude_replaces_across_a_tree_in_one_review() {
        use serde_json::json;
        let scratch = crate::scratch::Scratch::new("replace-tool");
        let dir = scratch.to_path_buf();
        for d in ["src/deep", "target", ".git"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let dir = std::fs::canonicalize(&dir).unwrap();
        let files = [
            ("src/a.rs", "fn old_name() {}\nfn x() { old_name() }\n"),
            ("src/b.rs", "use crate::old_name;\n"),
            ("src/deep/c.rs", "// old_value here\n"),
            ("target/gen.rs", "old_name\n"),
            (".gitignore", "target/\n"),
            ("README.md", "OLD_NAME stays\n"),
            (".git/config", "old_name\n"),
        ];
        for (f, text) in files {
            std::fs::write(dir.join(f), text).unwrap();
        }
        let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();
        let mut ed = editor("");
        ed.session.root = dir.clone();
        let args = |extra: serde_json::Value| {
            let mut a = json!({"pattern": r"old_(\w+)", "replacement": "new_$1"});
            a.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
            a
        };
        // A preview: what it would change, nothing changed
        let t = tool_turn(&mut ed, &dir, "replace", args(json!({"preview": true})), false);
        assert!(
            t.starts_with("4 matches in 3 files — preview, nothing changed: `old_(\\w+)` → `new_$1`\nsrc/a.rs (2)\n  1: fn old_name() {}\n   → fn new_name() {}\n"),
            "{t}"
        );
        assert!(t.contains("\nsrc/deep/c.rs (1)\n  1: // old_value here\n   → // new_value here"), "{t}");
        assert_eq!(read("src/a.rs"), files[0].1);
        // Past max_files: refused, said how to mean it
        let t = tool_turn(&mut ed, &dir, "replace", args(json!({"max_files": 2})), true);
        assert!(t.starts_with("error: this replace would change 3 files — more than max_files (2)"), "{t}");
        assert!(t.contains("give max_files: 3"), "{t}");
        assert!(ed.session.code_review.is_none());
        // A path outside the project: refused
        let t = tool_turn(&mut ed, &dir, "replace", args(json!({"path": ".."})), true);
        assert!(t.starts_with("error: .. is outside the project"), "{t}");
        // Applied, review at the end: written in one go, the turn reviewed when it ends
        let t = tool_turn(&mut ed, &dir, "replace", args(json!({})), true);
        assert_eq!(
            t,
            "4 matches in 3 files — replace `old_(\\w+)` → `new_$1`\nwritten: src/a.rs (2), src/b.rs (1), \
             src/deep/c.rs (1) — the user reviews your turn when it ends (space u takes it back)"
        );
        assert_eq!(read("src/a.rs"), "fn new_name() {}\nfn x() { new_name() }\n");
        assert_eq!(read("src/b.rs"), "use crate::new_name;\n");
        assert_eq!(read("src/deep/c.rs"), "// new_value here\n");
        assert_eq!(read("target/gen.rs"), "old_name\n", "gitignored");
        assert_eq!(read(".git/config"), "old_name\n", "never .git");
        assert_eq!(read("README.md"), "OLD_NAME stays\n", "case as written");
        settle_for(&mut ed, reviewing);
        let r = ed.session.code_review.as_ref().unwrap();
        let rels: Vec<&str> = r.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, ["src/a.rs", "src/b.rs", "src/deep/c.rs"]);
        // `space u`: the whole turn back
        crate::codereview::close(&mut ed);
        crate::checkpoint::step(&mut ed, true).unwrap();
        for (f, text) in &files[..3] {
            assert_eq!(read(f), *text, "{f}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #74: when HEAD moves under an open buffer — a commit, a rebase's end, made outside (a session's
    /// terminal) — its git base is the index as it is now, not the one it had (+62 −62 over a clean tree).
    #[test]
    fn git_bases_follow_head_when_it_moves() {
        let dir = git_repo("head-moves", &[("a.txt", "one\n")]);
        std::fs::write(dir.join("a.txt"), "one\ntwo\n").unwrap();
        let mut ed = editor("");
        ed.git_auto = true;
        ed.session.root = dir.clone();
        ed.open(&dir.join("a.txt")).unwrap();
        let base = |ed: &Editor| ed.doc().git_base.as_deref().map(|b| b.to_string());
        settle_until(&mut ed, |ed| base(ed).as_deref() == Some("one\n"));
        crate::git::watch(ed.events.sender(), dir.clone());
        settle_until(&mut ed, |ed| ed.session.git_branch.is_some());
        // Committed outside: the branch's ref moves (HEAD's own file doesn't)
        git_in(&dir, &["commit", "-q", "-am", "two"]);
        settle_until(&mut ed, |ed| base(ed).as_deref() == Some("one\ntwo\n"));
        // A reset back, as a rebase's end rewrites HEAD: back again
        git_in(&dir, &["reset", "-q", "--mixed", "HEAD~1"]);
        settle_until(&mut ed, |ed| base(ed).as_deref() == Some("one\n"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #72: a preflight's output scrolls back — a click on a step's chip goes to its output (and gives the
    /// panel the keys), the wheel and C-u/C-d move from there.
    #[test]
    fn a_preflights_step_chips_scroll_its_output() {
        let mut ed = editor("");
        let task = crate::task::Task { name: "preflight".into(), cmd: String::new(), cwd: test_root() };
        let mut r = crate::task::Run::for_test(task);
        for (name, n) in [("fmt", 3), ("test", 120), ("clippy", 60)] {
            r.push_line(false, format!("── {name} ──"), true);
            for i in 0..n {
                r.push_line(false, format!("{name} line {i}"), false);
            }
        }
        ed.session.task_run = Some(r);
        let steps: Vec<crate::preflight::Step> = ["fmt", "test", "clippy"]
            .iter()
            .map(|n| crate::preflight::Step {
                name: n.to_string(),
                cmd: String::new(),
                note: None,
                from: String::new(),
                full: false,
            })
            .collect();
        ed.session.preflight =
            Some(crate::preflight::Preflight::new(0, &steps, crate::preflight::Set::Full, "k".into()));
        let draw = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 120, 40).unwrap();
            let mut parser = vt100::Parser::new(40, 120, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        let s = draw(&mut ed);
        assert!(s.contains("clippy line 59") && !s.contains("test line 0\n"), "its end: {s}");
        let click = |ed: &mut Editor, kind, x: usize, y: usize| {
            ed.handle_mouse(crate::event::Mouse { kind, x: x as u16, y: y as u16, alt: false, ctrl: false });
        };
        let (y, x, _, _) =
            *ed.screen.task_steps.borrow().iter().find(|c| c.3 == 1).expect("the test step's chip");
        click(&mut ed, crate::event::MouseKind::Down, x, y);
        let heading = ed.session.task_run.as_ref().unwrap().output.iter().position(|l| l.1 == "── test ──");
        assert_eq!(ed.session.task_run.as_ref().unwrap().scroll, heading, "at the test step's heading");
        assert!(ed.session.is_at(crate::session::At::Slot), "the panel has the keys");
        let s = draw(&mut ed);
        assert!(s.contains("test line 0"), "{s}");
        // The wheel and C-u: further back
        click(&mut ed, crate::event::MouseKind::ScrollUp, x, y + 3);
        assert_eq!(ed.session.task_run.as_ref().unwrap().scroll, heading.map(|h| h - 3));
        feed(&mut ed, "<C-u>");
        let s = draw(&mut ed);
        assert!(s.contains("fmt line 0"), "{s}");
    }

    /// #75: a replace across several files, not under review at the end, is one card — not a card per
    /// change: `v` shows every change in a buffer, `y` writes them all (kept for `space u`), `n` leaves them
    /// all, and the card's buffer goes with it.
    #[test]
    fn claudes_change_across_files_is_one_card() {
        use serde_json::json;
        let scratch = crate::scratch::Scratch::new("change-set");
        let dir = scratch.to_path_buf();
        std::fs::create_dir_all(dir.join("src/deep")).unwrap();
        let files = [
            ("src/a.rs", "fn old_name() {}\nfn x() { old_name() }\n"),
            ("src/b.rs", "use crate::old_name;\n"),
            ("src/deep/c.rs", "// old_value here\n"),
        ];
        let put = || {
            for (f, text) in files {
                std::fs::write(dir.join(f), text).unwrap();
            }
        };
        put();
        let read = |f: &str| std::fs::read_to_string(dir.join(f)).unwrap();
        let mut ed = editor("");
        ed.session.root = dir.clone();
        let log = claude_calls(
            &mut ed,
            &dir,
            "replace",
            &json!({"pattern": r"old_(\w+)", "replacement": "new_$1"}),
        );
        let screen = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 30).unwrap();
            let mut parser = vt100::Parser::new(30, 140, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        let settled = |ed: &mut Editor| {
            let o = ed.offers.first().map(|o| (o.id, o.rev)).unwrap();
            ed.card_focus = Some((o.0, o.1));
        };
        let answered = |ed: &Editor| {
            std::fs::read_to_string(dir.join("stdin.log")).is_ok_and(|s| s.contains("\"c7\""))
                && ed.session.chat.as_ref().is_some_and(|c| !c.busy())
        };
        let card_buffers = |ed: &Editor| {
            ed.docs.iter().filter(|d| d.title.as_deref().is_some_and(|t| t.starts_with("card ·"))).count()
        };
        feed(&mut ed, "<space>ldo it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        assert!(ed.session.is_at(crate::session::At::Chat), "the chat keeps the keys (frictions26)");
        assert!(ed.session.review.is_none(), "no change of it in review yet");
        let s = screen(&mut ed);
        assert!(s.contains("Claude changes 3 files") && s.contains("replace `old_(\\w+)` → `new_$1`"), "{s}");
        assert!(s.contains("4 matches in 3 files:") && s.contains("src/deep/c.rs  1"), "{s}");
        assert!(s.contains("y Accept all") && s.contains("r One by one") && s.contains("v See them"), "{s}");
        settled(&mut ed);
        // `v`: every change, file by file
        answer(&mut ed, "v");
        settle_for(&mut ed, |ed| card_buffers(ed) == 1);
        let all = text(&ed);
        assert!(all.contains("── src/a.rs (2)") && all.contains("  - fn old_name() {}"), "{all}");
        assert!(all.contains("  + fn new_name() {}") && all.contains("── src/deep/c.rs (1)"), "{all}");
        assert_eq!(read("src/a.rs"), files[0].1, "nothing written yet");
        // `y`: all of them written at once
        screen(&mut ed);
        settled(&mut ed);
        answer(&mut ed, "y");
        settle_for(&mut ed, answered);
        assert_eq!(read("src/a.rs"), "fn new_name() {}\nfn x() { new_name() }\n");
        assert_eq!(read("src/deep/c.rs"), "// new_value here\n");
        let t = tool_answer(&log);
        assert!(t.contains("accepted by the user: src/a.rs (2), src/b.rs (1), src/deep/c.rs (1)"), "{t}");
        assert_eq!(card_buffers(&ed), 0, "its buffer went with the answer");
        assert!(ed.offers.is_empty() && ed.session.review.is_none());
        // `space u` takes them back
        crate::checkpoint::step(&mut ed, true).unwrap();
        assert_eq!(read("src/a.rs"), files[0].1);
        // Again, `n`: every file left as it was
        put();
        std::fs::remove_file(&log).unwrap();
        ed.session.at = crate::session::At::Chat;
        feed(&mut ed, "again<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        screen(&mut ed);
        settled(&mut ed);
        answer(&mut ed, "n");
        settle_for(&mut ed, answered);
        let t = tool_answer(&log);
        assert!(t.contains("nothing changed") && t.contains("declined by the user"), "{t}");
        assert_eq!(read("src/b.rs"), files[1].1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Claude builds: a check runs at once in the task panel, the call waits for its end, and Claude reads
    /// how it ended, the places it pointed at and its output; the editor goes to the first error, and the
    /// It isn't the task `space m m` runs again.
    #[test]
    fn claude_runs_a_check_and_reads_how_it_ended() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-cmd-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let lib = dir.join("src/lib.rs");
        std::fs::write(&lib, "pub fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n").unwrap();
        let makefile = dir.join("Makefile");
        std::fs::write(
            &makefile,
            format!(
                "all:\n\t@echo compiling calc\n\t@echo '{}:2:5: error: mismatched types' >&2\n\t@exit 3\n",
                lib.display()
            ),
        )
        .unwrap();
        let mut ed = editor("");
        // (`make` is a check — its recipe a stand-in that prints an error; allowed by the setting)
        ed.config.llm.run_allow = vec!["make".into()];
        let command = format!("make -s -f {}", makefile.display());
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
        feed(&mut ed, "<space>lbuild it<ret>");
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(ed.offers.is_empty(), "a check doesn't ask");
        let r = ed.session.task_run.as_ref().expect("it ran in the task panel");
        // (make exits 2 when a recipe fails)
        assert_eq!(
            (r.task.name.as_str(), &r.state),
            (command.as_str(), &crate::task::State::Failed(Some(2)))
        );
        assert!(ed.session.task_last.is_none(), "not the user's task to run again");
        let answer = tool_answer(&log);
        assert!(answer.contains("exit 2 in ") && answer.contains(&format!("$ {command}")), "{answer}");
        // (a check's places are in the user's list — and the report says so)
        assert!(
            answer.contains("1 errors, 0 warnings in the user's list")
                && answer.contains("lib.rs:2:5 error: mismatched types"),
            "{answer}"
        );
        assert!(ed.session.qlist.as_ref().is_some_and(|l| l.items.len() == 1), "and they are");
        assert!(answer.contains("Output:") && answer.contains("compiling calc"), "{answer}");
        assert!(!answer.contains("isError"), "{answer}");
        // The view went to the error (the row's ending is `chat.rs`'s test: this claude sends no result)
        assert_eq!((ed.doc().path.as_deref(), led_line(&ed)), (Some(lib.as_path()), 1));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #27: a line of reads runs without a card — `cd`, pipes, `sh -c '…'`, a `|` inside quotes — and in the
    /// `cwd` given; a cwd outside the project, or not there, is refused. Harmless: reads of this repo's files.
    #[test]
    fn lines_of_reads_and_a_cwd_run_without_a_card() {
        use serde_json::json;
        let mut ed = editor("");
        ed.session.root = this_repo();
        let call = |ed: &mut Editor, args: serde_json::Value| {
            let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "run", "arguments": args}});
            crate::tools::mcp(ed, 0, "c1", &msg)
        };
        let ran = |ed: &mut Editor, cmd: &str| {
            settle_for(ed, |ed| {
                !ed.offers.is_empty()
                    || ed
                        .session
                        .task_run
                        .as_ref()
                        .is_some_and(|r| r.task.cmd == cmd && r.state != crate::task::State::Running)
            });
            assert!(ed.offers.is_empty(), "{cmd}: no card");
            let r = ed.session.task_run.as_ref().unwrap();
            assert_eq!(r.state, crate::task::State::Passed, "{cmd}: {:?}", r.output);
            r.task.cwd.clone()
        };
        for cmd in [
            "cd src && grep -c fn readonly.rs | head -1",
            "sh -c 'wc -l src/readonly.rs | sort -n'",
            "grep -cE 'fn (parse|parts)' src/readonly.rs",
        ] {
            assert!(call(&mut ed, json!({ "command": cmd })).is_none(), "answered when it ends");
            ran(&mut ed, cmd);
        }
        assert!(call(&mut ed, json!({ "command": "ls readonly.rs", "cwd": "src" })).is_none());
        assert!(ran(&mut ed, "ls readonly.rs").ends_with("src"), "in the cwd given");
        for (cwd, why) in [("/", "outside the project"), ("no-such-folder", "cwd no-such-folder")] {
            let r = call(&mut ed, json!({ "command": "ls", "cwd": cwd })).expect("refused now");
            assert!(r["result"]["isError"] == true && r.to_string().contains(why), "{r}");
        }
        // A cd into nothing: the shell would stay put and read `../../x` from the root — with no sandbox, the
        // card, not a run (in the sandbox it runs: what it reads is the sandbox's to hold — P3)
        ed.config.llm.sandbox = false;
        let cmd = "cd no-such-folder; cat ../../tarae-not-there";
        assert!(call(&mut ed, json!({ "command": cmd })).is_none());
        settle_for(&mut ed, |ed| {
            !ed.offers.is_empty() || ed.session.task_run.as_ref().is_some_and(|r| r.task.cmd == cmd)
        });
        assert!(ed.session.task_run.as_ref().is_none_or(|r| r.task.cmd != cmd), "it didn't run");
        assert!(
            matches!(&ed.offers[0].what, crate::offer::What::Run(r) if r.cmd == cmd && r.again.is_none())
        );
    }

    /// A command of Claude's runs without `CDPATH` — a `cd` lands where the read-only check resolved it, not
    /// in a folder of that name on `CDPATH`; the user's own task keeps their environment. (The test's own
    /// `CDPATH` is an empty folder of its own: any other test's `cd` finds nothing there, as without it.)
    #[test]
    fn claudes_commands_run_without_cdpath() {
        let dir = std::env::temp_dir().join(format!("tarae-cdpath-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: an empty folder of this test's own — a cd elsewhere falls back as if it weren't set
        unsafe { std::env::set_var("CDPATH", &dir) };
        let mut ed = editor("");
        let task = |name: &str| crate::task::Task {
            name: name.into(),
            cmd: "echo \"cdpath=${CDPATH-none}\"".into(),
            cwd: std::env::temp_dir(),
        };
        let printed = |ed: &mut Editor, name: &str| {
            settle_for(ed, |ed| {
                ed.session
                    .task_run
                    .as_ref()
                    .is_some_and(|r| r.task.name == name && r.state != crate::task::State::Running)
            });
            ed.session
                .task_run
                .as_ref()
                .unwrap()
                .output
                .iter()
                .map(|(_, l)| l.clone())
                .collect::<Vec<_>>()
                .join("\n")
        };
        ed.task_start(task("mine"));
        assert_eq!(printed(&mut ed, "mine"), format!("cdpath={}", dir.display()), "the user's: as it is");
        assert!(
            ed.task_start_for(task("claudes"), crate::tools::For::User("t".into()), None, Default::default())
                .is_ok()
        );
        assert_eq!(printed(&mut ed, "claudes"), "cdpath=none");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #27: a card's `a` lets its command (`tools::again` — this exact one, #193) run without asking for the
    /// rest of the chat: the same command the next turn has no card. The card says so.
    #[test]
    fn a_cards_this_chat_allows_its_pattern() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-pattern-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let mut ed = editor("");
        // (today's gate, for its card: in the sandbox it runs — P3)
        ed.config.llm.sandbox = false;
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": "basename one two" }));
        feed(&mut ed, "<space>lsay it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        let q = ed.offers[0].question.clone();
        assert!(q.contains("a — this exact command again unasked, this chat"), "{q}");
        assert!(ed.offers[0].lasts() && !ed.offers[0].keeps(), "a — this chat; never for good");
        // (`a`, as its key does once the card has been in sight)
        ed.offer_for_chat();
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(tool_answer(&log).contains("exit 0"));
        let again = &ed.session.chat.as_ref().unwrap().run_again;
        assert_eq!(
            again.iter().map(|r| r.again.parts.clone()).collect::<Vec<_>>(),
            [[["basename", "one", "two"]]]
        );
        ed.session.at = crate::session::At::Chat;
        let answers = |log: &Path| std::fs::read_to_string(log).unwrap_or_default().matches("\"c7\"").count();
        feed(&mut ed, "again<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty() || answers(&log) == 2);
        assert!(ed.offers.is_empty(), "this chat: no card");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #32: a check piped into filters is the check — unasked where the check alone is (`basename one` stands in
    /// for it here, as an allowed pattern), and on a card its pattern is offered; a pipe into what isn't a
    /// filter (`tee`) still asks.
    #[test]
    fn a_check_piped_into_filters_runs_as_the_check() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-piped-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        // (no `sh -c '…'` here: the fake claude's JSON goes in single quotes — tools.rs tests that form)
        let piped = "basename one two 2>&1 | grep -E \"one|x\" | head -n 1";
        // Allowed: no card, it ran
        let mut ed = editor("");
        ed.config.llm.sandbox = false;
        ed.config.llm.run_allow = vec!["basename one".into()];
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": piped }));
        feed(&mut ed, "<space>lsay it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty() || !ed.session.chat.as_ref().unwrap().busy());
        assert!(ed.offers.is_empty(), "a check through filters runs as the check would");
        let answer = tool_answer(&log);
        assert!(answer.contains("exit 0") && answer.contains("one two"), "{answer}");
        // Not allowed yet: the card offers the command again, its filters aside (#193)
        std::fs::remove_file(&log).ok();
        let mut ed = editor("");
        // (today's gate: in the sandbox any command runs — P3)
        ed.config.llm.sandbox = false;
        claude_calls(&mut ed, &dir, "run", &json!({ "command": piped }));
        feed(&mut ed, "<space>lsay it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        let q = ed.offers[0].question.clone();
        assert!(q.contains("a — this exact command again unasked, this chat"), "{q}");
        // A pipe into a writer isn't a filter
        std::fs::remove_file(&log).ok();
        let mut ed = editor("");
        ed.config.llm.sandbox = false;
        ed.config.llm.run_allow = vec!["basename one".into()];
        let tee = format!("basename one two | tee {}", dir.join("out").display());
        claude_calls(&mut ed, &dir, "run", &json!({ "command": tee }));
        feed(&mut ed, "<space>lsay it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        assert!(!dir.join("out").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Claude's command runs what's on disk: the user's unsaved file is saved first, and the run starts once
    /// it's written (a slow disk's stand-in here) — the saver's event, not a wait on the main loop.
    #[test]
    fn claudes_run_reads_the_users_file_as_saved() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-saved-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one\n").unwrap();
        let mut ed = editor("");
        ed.open(&file).unwrap();
        feed(&mut ed, "cTWO<esc>");
        ed.saver.slow = std::time::Duration::from_millis(500);
        let log =
            claude_calls(&mut ed, &dir, "run", &json!({ "command": format!("cat {}", file.display()) }));
        feed(&mut ed, "<space>lsay it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty() || !ed.session.chat.as_ref().unwrap().busy());
        assert!(ed.offers.is_empty(), "a read inside the project runs unasked");
        let answer = tool_answer(&log);
        assert!(answer.contains("exit 0") && answer.contains("TWOne"), "{answer}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #28: a run of Claude's that isn't a check or a build (a grep, a script) leaves the user's list as it
    /// was — its places still reach Claude's report; a check (`make`, its recipe a stand-in that prints an
    /// error line) takes the list over, as the user's own build would.
    #[test]
    fn only_claudes_checks_feed_the_users_list() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-list-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let lib = dir.join("lib.rs");
        std::fs::write(&lib, "fn a() {}\nfn b() {}\n").unwrap();
        let error = format!("{}:2:1: error: mismatched types", lib.display());
        std::fs::write(dir.join("x.sh"), format!("echo '{error}'\n")).unwrap();
        std::fs::write(dir.join("Makefile"), format!("all:\n\t@echo '{error}'\n")).unwrap();
        let mut ed = editor("");
        ed.config.llm.run_allow = vec!["sh".into(), "make".into()];
        let mine = crate::quickfix::Item {
            path: lib.clone(),
            line: 0,
            col: 0,
            message: "mine".into(),
            severity: 2,
            mark: None,
        };
        ed.qlist_set("my build", vec![mine]);
        let run = |ed: &mut Editor, cmd: &str| {
            let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "run", "arguments": {"command": cmd}}});
            assert!(crate::tools::mcp(ed, 0, "c1", &msg).is_none());
            settle_for(ed, |ed| {
                ed.session
                    .task_run
                    .as_ref()
                    .is_some_and(|r| r.task.cmd == cmd && r.state != crate::task::State::Running)
            });
        };
        let script = format!("sh {}", dir.join("x.sh").display());
        run(&mut ed, &script);
        let list = ed.session.qlist.as_ref().unwrap();
        assert_eq!((list.title.as_str(), list.items.len()), ("my build", 1), "not Claude's to replace");
        let r = ed.session.task_run.as_ref().unwrap();
        assert_eq!((r.places.len(), r.found), (1, (1, 0)), "its own places, for its report");
        let make = format!("make -s -f {}", dir.join("Makefile").display());
        run(&mut ed, &make);
        let list = ed.session.qlist.as_ref().unwrap();
        assert_eq!(list.title, make, "a check takes it over");
        assert!(list.items.iter().any(|i| i.message.contains("mismatched types")), "{:?}", list.items);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A command that only reads (`ls`, `cat`, `wc`, …) runs without a card when what it names stays in
    /// the project; one naming a path outside gets the run card, which says what it reads outside — `a`
    /// runs it and lets that place be read for the rest of the conversation, so the same command in the
    /// next turn runs unasked. Harmless: `wc` of this repo's Cargo.toml, `cat` of a marker in a temp dir.
    #[test]
    fn a_read_only_command_asks_only_for_what_is_outside() {
        use serde_json::json;
        let scratch = crate::scratch::Scratch::new("run-reads");
        let dir = scratch.to_path_buf();
        let marker = dir.join("marker.txt");
        std::fs::write(&marker, "tarae-marker-content\n").unwrap();
        let card_or_done = |ed: &Editor| !ed.offers.is_empty() || !ed.session.chat.as_ref().unwrap().busy();
        // Inside the project (this repo): it just runs
        let mut ed = editor("");
        ed.session.root = this_repo();
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": "wc -l Cargo.toml" }));
        feed(&mut ed, "<space>lcount<ret>");
        settle_for(&mut ed, card_or_done);
        assert!(ed.offers.is_empty(), "inside: no card");
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(tool_answer(&log).contains("Cargo.toml"), "{}", tool_answer(&log));
        // Outside: the card says what it reads
        std::fs::remove_file(&log).ok();
        let command = format!("cat {}", marker.display());
        let mut ed = editor("");
        // (today's gate: in the sandbox any command runs — P3)
        ed.config.llm.sandbox = false;
        ed.session.root = this_repo();
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
        feed(&mut ed, "<space>lread it<ret>");
        settle_for(&mut ed, card_or_done);
        let q = ed.offers.first().map(|o| o.question.clone()).expect("a card for a read outside");
        assert!(q.contains(&format!("It reads outside the project:\n  {}", marker.display())), "{q}");
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 140, 40).unwrap();
        let mut parser = vt100::Parser::new(40, 140, 0);
        parser.process(&bytes);
        assert!(
            parser
                .screen()
                .contents()
                .contains("y Once · a This chat · s This session · A Always here · n No")
        );
        let o = ed.offers.first().map(|o| (o.id, o.rev)).unwrap();
        ed.card_focus = Some((o.0, o.1));
        answer(&mut ed, "a");
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(tool_answer(&log).contains("tarae-marker-content"), "{}", tool_answer(&log));
        assert!(ed.session.chat.as_ref().unwrap().allowed.reads(&marker));
        // The next turn, the same command: allowed now, no card
        ed.session.at = crate::session::At::Chat;
        feed(&mut ed, "again<ret>");
        let answers = |log: &Path| std::fs::read_to_string(log).unwrap().matches("\"c7\"").count();
        settle_for(&mut ed, |ed| !ed.offers.is_empty() || answers(&log) == 2);
        assert!(ed.offers.is_empty(), "allowed this chat: no card");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// f22 review: shellcheck of a script in the project runs unasked — but where what it may read beyond its line
    /// (`Editor.shellcheck`: the process's home and environment outside tests) has an rc file turning external
    /// sources on, it gets the run card, saying why. (The card is answered `n`: nothing runs.)
    #[test]
    fn a_shellcheck_whose_home_rc_follows_sources_asks() {
        use serde_json::json;
        let scratch = crate::scratch::Scratch::new("run-shellcheck");
        let dir = scratch.to_path_buf();
        let (p, home) = (dir.join("p"), dir.join("home"));
        std::fs::create_dir_all(&p).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(p.join("x.sh"), "echo x\n").unwrap();
        std::fs::write(home.join(".shellcheckrc"), "external-sources=true\n").unwrap();
        let card_or_done = |ed: &Editor| !ed.offers.is_empty() || !ed.session.chat.as_ref().unwrap().busy();
        let cmd = json!({ "command": "shellcheck --norc x.sh" });
        // (the parser's gate, which only the no-sandbox path keeps until piece 5 — in the sandbox any command
        // runs, what it reads the sandbox's: P3)
        // (no rc file read: it reads only what it's named — no card, whatever it then says)
        let mut ed = editor("");
        ed.config.llm.sandbox = false;
        ed.session.root = p.clone();
        ed.shellcheck =
            crate::readonly::Shellcheck { homes: vec![home.join(".shellcheckrc")], opts: Vec::new() };
        claude_calls(&mut ed, &dir, "run", &cmd);
        feed(&mut ed, "<space>lcheck<ret>");
        settle_for(&mut ed, card_or_done);
        assert!(ed.offers.is_empty(), "--norc: no card");
        // (home's rc turns external sources on: the card says so)
        let mut ed = editor("");
        ed.config.llm.sandbox = false;
        ed.session.root = p.clone();
        ed.shellcheck =
            crate::readonly::Shellcheck { homes: vec![home.join(".shellcheckrc")], opts: Vec::new() };
        claude_calls(&mut ed, &dir, "run", &json!({ "command": "shellcheck x.sh" }));
        feed(&mut ed, "<space>lcheck<ret>");
        settle_for(&mut ed, card_or_done);
        let q = ed.offers.first().map(|o| o.question.clone()).expect("a card");
        assert!(q.contains(".shellcheckrc turns external sources on"), "{q}");
        ed.offer_dismiss();
    }

    /// #164: every run card says in one line why it asks — a line the shell reader doesn't read says what
    /// in it; a `cd` whose folder can't be told says which; a program that isn't a check says its name.
    #[test]
    fn a_run_card_says_why_it_asks() {
        use serde_json::json;
        let scratch = crate::scratch::Scratch::new("run-why");
        let dir = scratch.to_path_buf();
        // (no `'` in them: the fake claude's script holds them in single quotes)
        for (command, why) in [
            (
                "sh -c \"d=$(ls -d src/* | head -1); grep -rn fn $d\"",
                "What it does can't be told from its words: `$(…)` makes its words as it runs.",
            ),
            ("cd nope; cat x", "Where it reads can't be told — cd nope: no such folder."),
            ("make -s", "It runs make — not a check, a read, or a command allowed in this chat."),
        ] {
            let mut ed = editor("");
            // (today's gate: in the sandbox any command runs — P3)
            ed.config.llm.sandbox = false;
            ed.session.root = this_repo();
            let _log = claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
            feed(&mut ed, "<space>lgo<ret>");
            settle_for(&mut ed, |ed| !ed.offers.is_empty() || !ed.session.chat.as_ref().unwrap().busy());
            let q = ed.offers.first().map(|o| o.question.clone()).expect("a card");
            // (after the command and where it runs, before anything else)
            let after = q.strip_prefix(command).and_then(|r| r.split_once("\n\n")).map(|(_, r)| r);
            assert!(after.is_some_and(|r| r.starts_with(why)), "{q}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Claude asks to run a command outside the sandbox: never without a reason (told at once, no card);
    /// with one, always a card — even for a command that would run unasked (`pwd`) — its reason on it, `y`
    /// or `n` only (`a` isn't an answer: never kept); `y` runs it outside, once, and the result says so.
    #[test]
    fn claude_asks_to_run_outside_the_sandbox_with_a_reason() {
        use serde_json::json;
        let scratch = crate::scratch::Scratch::new("run-outside");
        let dir = scratch.to_path_buf();
        // No reason: told so, nothing asked
        let mut ed = editor("");
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "run", "arguments": {"command": "pwd", "outside_sandbox": true}}});
        let r = crate::tools::mcp(&mut ed, 0, "c1", &msg).expect("answered now");
        assert!(r["result"]["isError"] == true && r.to_string().contains("needs a reason"), "{r}");
        assert!(ed.offers.is_empty());
        // With one: a card, even for a command that runs unasked
        let reason = "it needs the network to fetch the crates";
        let mut ed = editor("");
        let args = json!({ "command": "pwd", "outside_sandbox": true, "reason": reason });
        let log = claude_calls(&mut ed, &dir, "run", &args);
        feed(&mut ed, "<space>lwhere am I<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty() || !ed.session.chat.as_ref().unwrap().busy());
        let o = ed.offers.first().expect("asked, not run");
        assert_eq!(o.title, crate::tools::OUTSIDE_RUN_TITLE);
        assert!(o.question.contains(&format!("Its reason: {reason}")), "{}", o.question);
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 140, 40).unwrap();
        let mut parser = vt100::Parser::new(40, 140, 0);
        parser.process(&bytes);
        let s = parser.screen().contents();
        assert!(s.contains("y Run") && !s.contains("This chat") && !s.contains("Always"), "{s}");
        let o = ed.offers.first().map(|o| (o.id, o.rev)).unwrap();
        ed.card_focus = Some((o.0, o.1));
        answer(&mut ed, "a");
        assert_eq!(ed.offers.len(), 1, "never kept: `a` isn't an answer");
        answer(&mut ed, "y");
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        assert!(answer.contains("Ran outside the sandbox"), "{answer}");
        assert!(matches!(
            ed.session.task_run.as_ref().and_then(|r| r.guard.clone()),
            Some(crate::sandbox::Guard::Outside)
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Claude asks to see the screen: a card first (`C-q` from the chat answers it — frictions26), nothing
    /// goes before `y`, and what goes is the frame drawn after it — the code and the terminal panel's
    /// output, not the card. The card says the terminal panel is in it; with `llm.screen-terminal` off its
    /// rows are blank and the card says that instead. `a` lets the next call go unasked; `n` tells Claude no.
    #[test]
    fn claude_sees_the_screen_once_the_user_says_yes() {
        use serde_json::json;
        if crate::sandbox::no_pty_here("editor::tests::claude_sees_the_screen_once_the_user_says_yes") {
            return;
        }
        let scratch = crate::scratch::Scratch::new("screen-tool");
        let dir = scratch.to_path_buf();
        std::fs::write(dir.join("a.txt"), "screen-marker on this line\n").unwrap();
        let draw = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 40).unwrap();
            let mut parser = vt100::Parser::new(40, 140, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        // Events handled with a frame drawn after each — the screen goes with a frame
        let settle_drawing = |ed: &mut Editor, done: &dyn Fn(&Editor) -> bool| {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while !done(ed) {
                assert!(std::time::Instant::now() < end, "done before the timeout");
                if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(50)) {
                    ed.handle_event(ev);
                }
                draw(ed);
            }
        };
        let with_terminal = |screen_terminal: bool| {
            let mut ed = editor("");
            ed.config.llm.screen_terminal = screen_terminal;
            ed.open(&dir.join("a.txt")).unwrap();
            let argv = ["/bin/sh".to_string(), "-c".into(), "printf term-marker; sleep 5".into()];
            ed.terminal_run(argv.to_vec(), Vec::new());
            let shows = |ed: &Editor| {
                ed.session
                    .terminal
                    .as_ref()
                    .is_some_and(|t| t.parser.screen().contents().contains("term-marker"))
            };
            settle_for(&mut ed, shows);
            ed.session.leave(crate::session::At::Program);
            draw(&mut ed);
            ed
        };
        let answers = |log: &Path| std::fs::read_to_string(log).unwrap_or_default().matches("\"c7\"").count();
        // y: the card first, then the screen as drawn after the yes
        let mut ed = with_terminal(true);
        let log = claude_calls(&mut ed, &dir, "screen", &json!({}));
        feed(&mut ed, "<space>lwhat do you see<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        let o = ed.offers.first().unwrap();
        assert_eq!(o.title, crate::seeing::TITLE);
        assert!(o.question.contains("Includes the terminal panel ("), "{}", o.question);
        assert!(ed.session.is_at(crate::session::At::Chat), "the chat keeps the keys");
        let s = draw(&mut ed);
        assert!(s.contains("y Once · a This chat · n No"), "{s}");
        assert_eq!(answers(&log), 0, "nothing goes before the answer");
        feed(&mut ed, "<C-q>y");
        assert!(ed.offers.is_empty());
        settle_drawing(&mut ed, &|ed| !ed.session.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        assert!(answer.contains("screen-marker on this line") && answer.contains("term-marker"), "{answer}");
        assert!(answer.contains("the terminal panel included"), "{answer}");
        assert!(
            !answer.contains(crate::seeing::TITLE) && !answer.contains("isError"),
            "not the card: {answer}"
        );
        assert!(!ed.session.chat.as_ref().unwrap().sees, "only once");
        // The terminal panel left out: said on the card, blank in what goes; `a` — the next call is unasked
        let mut ed = with_terminal(false);
        std::fs::remove_file(&log).ok();
        let log = claude_calls(&mut ed, &dir, "screen", &json!({}));
        feed(&mut ed, "<space>lwhat do you see<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        let q = ed.offers[0].question.clone();
        assert!(q.contains("Not the terminal panel (llm.screen-terminal is off)"), "{q}");
        feed(&mut ed, "<C-q>a");
        settle_drawing(&mut ed, &|ed| !ed.session.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        assert!(answer.contains("screen-marker") && !answer.contains("term-marker"), "{answer}");
        assert!(answer.contains(crate::seeing::LEFT_OUT), "{answer}");
        ed.session.at = crate::session::At::Chat;
        feed(&mut ed, "again<ret>");
        settle_drawing(&mut ed, &|ed| !ed.offers.is_empty() || answers(&log) == 2);
        assert!(ed.offers.is_empty(), "this chat: no card");
        // n: Claude is told
        let mut ed = editor("");
        std::fs::remove_file(&log).ok();
        let log = claude_calls(&mut ed, &dir, "screen", &json!({}));
        draw(&mut ed);
        feed(&mut ed, "<space>lwhat do you see<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        feed(&mut ed, "<C-q>n");
        settle_drawing(&mut ed, &|ed| !ed.session.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        assert!(answer.contains("said no") && answer.contains("isError"), "{answer}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// cardauto P3: in the sandbox, a command that isn't a check runs at once — no card — and Claude hears how it
    /// ended. (The run: `touch` in the test's own temp folder.)
    #[test]
    fn a_command_in_the_sandbox_runs_without_a_card() {
        use serde_json::json;
        let scratch = crate::scratch::Scratch::new("run-p3");
        let dir = std::fs::canonicalize(&scratch).unwrap();
        let made = dir.join("made");
        let mut ed = editor("");
        let command = format!("touch {} && echo made", made.display());
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
        feed(&mut ed, "<space>ltouch it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty() || !ed.session.chat.as_ref().unwrap().busy());
        assert!(ed.offers.is_empty(), "no card: {:?}", ed.offers.first().map(|o| &o.question));
        let answer = tool_answer(&log);
        assert!(made.exists() && answer.contains("exit 0") && !answer.contains("isError"), "{answer}");
    }

    /// Past the checks, a command waits for the user: nothing runs until `y`; `n` tells Claude no.
    #[test]
    fn a_command_past_the_checks_asks_first() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-ask-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let made = dir.join("made");
        let command = format!("touch {}", made.display());
        // No: Claude is told, nothing ran
        let mut ed = editor("");
        // (today's gate, for its card: in the sandbox it runs — P3)
        ed.config.llm.sandbox = false;
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
        feed(&mut ed, "<space>ltouch it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        let o = &ed.offers[0];
        assert!(matches!(&o.what, crate::offer::What::Run(r) if r.cmd == command));
        // (why it asks, a line under it — #164)
        let why = "It runs touch — not a check, a read, or a command allowed in this chat.";
        // (#193: `a` is this exact command — one word and a path makes one)
        let a = "a — this exact command again unasked, this chat";
        let off = "The sandbox is off (llm.sandbox) — it runs with all your permissions.";
        assert_eq!(
            (o.title.as_str(), o.question.as_str()),
            ("Claude wants to run", format!("{command}\n\n{why}\n\n{a}\n\n{off}").as_str())
        );
        assert!(o.weighty(), "y must have been in sight a moment to count");
        // Another command while that one is asked about waits on a card of its own, under it (#5: refused,
        // Claude sent it again and again)
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "run", "arguments": {"command": "rm -rf x"}}});
        assert!(crate::tools::mcp(&mut ed, 0, "c1", &msg).is_none(), "answered from its card");
        assert_eq!(ed.offers.len(), 2);
        assert!(matches!(&ed.offers[1].what, crate::offer::What::Run(r) if r.cmd == "rm -rf x"));
        ed.offer_dismiss();
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        assert!(answer.contains("the user chose not to run") && answer.contains("isError"), "{answer}");
        assert!(ed.session.task_run.is_none() && !made.exists());
        // Yes: it runs, and Claude hears how it ended
        std::fs::remove_file(&log).ok();
        let mut ed = editor("");
        // (today's gate, for its card: in the sandbox it runs — P3)
        ed.config.llm.sandbox = false;
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
        feed(&mut ed, "<space>ltouch it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        assert!(!made.exists(), "nothing runs before the answer");
        ed.offer_accept();
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(made.exists() && ed.offers.is_empty());
        let answer = tool_answer(&log);
        assert!(answer.contains("exit 0") && answer.contains("(no output)"), "{answer}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The follow card says Claude waits on the user while its command waits on its card — not "running";
    /// once allowed, it runs.
    #[test]
    fn a_command_waiting_on_its_card_is_waiting_for_you() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-waits-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let draw = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 40).unwrap();
            let mut parser = vt100::Parser::new(40, 140, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        let mut ed = editor("fn main() {}\n");
        // (today's gate, for its card: in the sandbox it runs — P3)
        ed.config.llm.sandbox = false;
        let command = format!("sleep 1; touch {}", dir.join("made").display());
        claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
        feed(&mut ed, "<space>lmake it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        let screen = draw(&mut ed);
        // (the card's title says it waits; its run is the knot in hand)
        assert!(screen.contains(" waiting ") && !screen.contains("exploring"), "{screen}");
        ed.offer_accept();
        settle_for(&mut ed, |ed| ed.session.task_run.is_some());
        let screen = draw(&mut ed);
        // (the card names the tool, as its chat row does)
        assert!(screen.contains("● run    sleep 1") && !screen.contains("waiting"), "{screen}");
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #24/#25: a card is answered from anywhere — with a draft in the chat's input its `y` is the draft's,
    /// and the card says `C-q` brings it the keys; there Esc goes back unanswered, keys it doesn't ask for
    /// are swallowed, and `y` answers it — the draft as it was. Esc never says no, in the input or out.
    #[test]
    fn a_card_is_answered_from_a_draft_with_c_q() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-card-cq-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let made = dir.join("made");
        let command = format!("touch {}", made.display());
        let draw = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 40).unwrap();
            let mut parser = vt100::Parser::new(40, 140, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        let mut ed = editor("");
        // (today's gate, for its card: in the sandbox it runs — P3)
        ed.config.llm.sandbox = false;
        ed.offer_popups = true;
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
        feed(&mut ed, "<space>ltouch it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        // A draft in the chat's input: what's typed is the draft's, the card says how to reach it
        ed.session.at = crate::session::At::Chat;
        // (minimized while Claude works: its first letter brings it back, typed — #151 — then the rest)
        assert!(ed.session.chat.as_ref().unwrap().compact());
        feed(&mut ed, "m");
        feed(&mut ed, "y draft y");
        let input = |ed: &Editor| ed.session.chat.as_ref().unwrap().input.text();
        assert_eq!(input(&ed), "my draft y");
        assert!(!made.exists() && ed.offers.len() == 1);
        assert!(draw(&mut ed).contains("C-q to answer it"));
        // Esc leaves the input; another Esc (the card has the keys now) isn't a no
        feed(&mut ed, "<esc><esc>");
        assert_eq!(ed.offers.len(), 1, "Esc never answers a card");
        // Back in the input: C-q — the card has the keys, the draft stays
        ed.session.at = crate::session::At::Chat;
        // (the status line's label names where the keys go — #78: it said CHAT while the card had them)
        // (the status line: the screen's last row)
        let label = |s: &str| s.lines().nth(39).unwrap_or_default().to_string();
        assert!(label(&draw(&mut ed)).contains("● chat "), "{}", label(&draw(&mut ed)));
        feed(&mut ed, "<C-q>");
        assert!(ed.card_has_keys());
        let s = draw(&mut ed);
        assert!(s.contains("esc goes back, unanswered"));
        assert!(label(&s).contains("● card ") && !label(&s).contains("chat"), "{}", label(&s));
        feed(&mut ed, "x");
        // (said on the card, where the eyes are — it was a flash on the status line)
        assert!(draw(&mut ed).contains("x isn't an answer here"), "{:?}", ed.offer_said_now());
        feed(&mut ed, "<esc>");
        assert!(!ed.card_has_keys() && ed.offers.len() == 1 && ed.session.is_at(crate::session::At::Chat));
        assert_eq!(input(&ed), "my draft y", "back where it was");
        assert!(label(&draw(&mut ed)).contains("● chat "), "the chat's again");
        // C-q, y: answered, the keys back in the draft
        feed(&mut ed, "<C-q>");
        draw(&mut ed);
        feed(&mut ed, "y");
        assert!(ed.offers.is_empty() && !ed.card_has_keys());
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(made.exists() && tool_answer(&log).contains("exit 0"));
        assert_eq!(input(&ed), "my draft y");
        assert!(ed.session.is_at(crate::session::At::Chat));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #101: `C-q` gave card 1 of 2 the keys; once it's answered they stay with card 2 (it waited for another
    /// `C-q`), and once that's answered they're back where they were (insert mode, its text as typed).
    #[test]
    fn the_keys_go_on_to_the_next_card_after_an_answer() {
        let mut ed = editor("");
        for t in ["Rust syntax colors", "Go syntax colors"] {
            let what = crate::offer::What::Grammars { lang: t.into(), names: Vec::new(), then: None };
            ed.push_offer(crate::offer::Offer::new(t.into(), "Download it?", "", String::new(), what));
        }
        feed(&mut ed, "iab");
        assert!(!ed.card_has_keys(), "insert mode keeps its keys");
        feed(&mut ed, "<C-q>n");
        assert_eq!(ed.offers.len(), 1, "the first answered");
        // (handed on once it's drawn: the frame the user sees it in)
        assert!(!ed.card_has_keys(), "not before it's in sight");
        crate::term::render(&mut ed, &mut Vec::new(), 100, 20).unwrap();
        assert!(ed.card_has_keys(), "the keys went on to the second");
        feed(&mut ed, "n");
        assert!(ed.offers.is_empty() && !ed.card_has_keys());
        feed(&mut ed, "c");
        assert_eq!((ed.mode, text(&ed).as_str()), (Mode::Insert, "abc"), "back where they were");
    }

    /// #23: Enter while Claude works queues the message — said over the input, nothing sent yet; ↑ takes it
    /// back to edit (out of the queue), Enter queues it again; it goes as the next turn when this one ends.
    #[test]
    fn a_message_sent_while_claude_works_waits_its_turn() {
        let dir = std::env::temp_dir().join(format!("tarae-chat-queue-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("sent.log");
        std::fs::remove_file(&log).ok();
        let draw = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 40).unwrap();
            let mut parser = vt100::Parser::new(40, 140, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        let mut ed = editor("fn main() {}\n");
        let logged = format!("printf '%s\\n' \"$line\" >> '{}'", log.display());
        fake_claude_stream(
            &mut ed,
            &[json_str(&logged), json_str("sleep 1"), serde_json::json!({"type": "result", "result": "ok"})],
        );
        let sent = |log: &Path| std::fs::read_to_string(log).unwrap_or_default().lines().count();
        feed(&mut ed, "<space>lfirst<ret>");
        // (its first turn takes a second: no events handled meanwhile, so it's still in flight)
        while sent(&log) < 1 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(ed.session.chat.as_ref().unwrap().busy());
        // (the panel in full while Claude works, not folded to the following card)
        ed.session.chat.as_mut().unwrap().unfolded = true;
        feed(&mut ed, "second<ret>");
        let c = ed.session.chat.as_ref().unwrap();
        assert_eq!((c.queued.as_deref(), c.input.text().as_str()), (Some("second"), ""));
        let s = draw(&mut ed);
        assert!(s.contains("queued second") && s.contains("↑ edit queued"), "{s}");
        // ↑: back to edit — out of the queue; Enter queues it again
        feed(&mut ed, "<up>");
        let c = ed.session.chat.as_ref().unwrap();
        assert_eq!((c.queued.as_deref(), c.input.text().as_str()), (None, "second"));
        feed(&mut ed, " too<ret>");
        assert_eq!(ed.session.chat.as_ref().unwrap().queued.as_deref(), Some("second too"));
        assert_eq!(sent(&log), 1, "nothing goes while Claude works");
        // The turn ends: it goes
        settle_for(&mut ed, |_| sent(&log) == 2);
        let last = std::fs::read_to_string(&log).unwrap();
        assert!(last.lines().nth(1).unwrap().contains("second too"), "{last}");
        assert!(ed.session.chat.as_ref().unwrap().queued.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// frictions29 18: while Claude works, the review's notes (`space v s`) and a reply on a note wait for the
    /// turn's end as a message does — not refused, drafts until they go — and the queued row says what waits: how
    /// many messages (a second joins the first) and the last one's start, the review's notes, the replies. At the
    /// turn's end they go first, one a turn, the messages after.
    #[test]
    fn review_notes_and_replies_wait_for_the_turns_end_and_the_row_counts_them() {
        let s = crate::scratch::Scratch::new("chat-sends-queued");
        let log = s.join("sent.log");
        let draw = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 40).unwrap();
            let mut parser = vt100::Parser::new(40, 140, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        let mut ed = editor("fn main() {}\n");
        let logged = format!("printf '%s\\n' \"$line\" >> '{}'", log.display());
        fake_claude_stream(
            &mut ed,
            &[json_str(&logged), json_str("sleep 1"), serde_json::json!({"type": "result", "result": "ok"})],
        );
        let sent = |log: &Path| std::fs::read_to_string(log).unwrap_or_default();
        feed(&mut ed, "<space>lfirst<ret>");
        while sent(&log).lines().count() < 1 {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(ed.session.chat.as_ref().unwrap().busy());
        ed.session.chat.as_mut().unwrap().unfolded = true;
        // Two messages: one queue, the row counts them and shows the last
        feed(&mut ed, "second<ret>third<ret>");
        let c = ed.session.chat.as_ref().unwrap();
        assert_eq!(c.queued.as_deref(), Some("second\n\nthird"));
        assert!(draw(&mut ed).contains("2 messages queued · third"), "{}", draw(&mut ed));
        // A reply on a note while Claude works: in its thread now, sent at the turn's end (review 5)
        let path = s.join("a.rs");
        let other = crate::notes::add(&mut ed, &path, 2..3, crate::notes::By::You, "and here?");
        crate::notes::submit(&mut ed, Some(other), "this one too");
        let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert_eq!(said, "reply queued — it goes when Claude's turn ends");
        let said = ed.session.chat.as_ref().unwrap().queued_said().map(|q| q.0);
        assert_eq!(said.as_deref(), Some("a reply and 2 messages queued"));
        // A draft note sent as a review while Claude works: queued, still a draft
        let id = crate::notes::add(&mut ed, &path, 0..1, crate::notes::By::You, "why?");
        let n = ed.session.notes.list.iter_mut().find(|n| n.id == id).unwrap();
        (n.thread[0].draft, n.waiting) = (true, false);
        crate::codereview::send_as(&mut ed, None);
        let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert_eq!(said, "review queued — its 1 comment goes when Claude's turn ends");
        assert!(
            ed.session.notes.list.iter().find(|n| n.id == id).unwrap().thread[0].draft,
            "a draft until sent"
        );
        let c = ed.session.chat.as_ref().unwrap();
        assert!(matches!(c.sends[..], [crate::chat::Later::Reply { .. }, crate::chat::Later::Review(None)]));
        let said = ed.session.chat.as_ref().unwrap().queued_said().map(|q| q.0);
        assert_eq!(said.as_deref(), Some("review notes, a reply and 2 messages queued"));
        let s = draw(&mut ed);
        assert!(s.contains("queued · third"), "{s}");
        // (one that sends nothing by its turn — its drafts gone with the first: the next goes on — review 1)
        ed.session.chat.as_mut().unwrap().sends.push(crate::chat::Later::Review(Some(true)));
        // (narrow: how many, and still the last one's start)
        let narrow = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 80, 24).unwrap();
            let mut parser = vt100::Parser::new(24, 80, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        assert!(narrow(&mut ed).contains("5 queued · third"), "{}", narrow(&mut ed));
        assert!(
            ed.ui_state().text().contains("review notes, a reply and 2 messages queued"),
            "{}",
            ed.ui_state().text()
        );
        assert_eq!(sent(&log).lines().count(), 1, "nothing goes while Claude works");
        // The turn ends: the reply goes first; at the next end the review, its draft sent; at the one after, the
        // second review has nothing to send and the messages go
        settle_for(&mut ed, |_| sent(&log).lines().count() == 2);
        assert!(sent(&log).lines().nth(1).unwrap().contains("this one too"), "{}", sent(&log));
        settle_for(&mut ed, |_| sent(&log).lines().count() == 3);
        assert!(sent(&log).lines().nth(2).unwrap().contains("<review of="), "{}", sent(&log));
        assert!(!ed.session.notes.list.iter().find(|n| n.id == id).unwrap().thread[0].draft);
        settle_for(&mut ed, |_| sent(&log).lines().count() == 4);
        let last = sent(&log).lines().nth(3).unwrap().to_string();
        assert!(last.contains("second") && last.contains("third"), "{last}");
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let c = ed.session.chat.as_ref().unwrap();
        assert!(c.queued.is_none() && c.sends.is_empty() && c.queued_said().is_none());
        // Idle, nothing waiting: a failed run of the last turn goes on by itself — not while something waits, which
        // is the conversation expected back too (review 1)
        let key = crate::blocked::RunKey { tests: false, generation: 7 };
        assert_eq!(crate::keepwarm::back(&ed.session), None);
        let c = ed.session.chat.as_mut().unwrap();
        c.failed = (c.conversation.clone(), vec![key]);
        assert!(crate::tools::goes_on(&ed, key));
        ed.session.chat.as_mut().unwrap().sends.push(crate::chat::Later::Review(None));
        assert!(!crate::tools::goes_on(&ed, key), "review notes wait to go first");
        assert_eq!(crate::keepwarm::back(&ed.session), Some(crate::keepwarm::Back::Queued));
    }

    /// The edit review's keys say what each takes (it read "y accept · a all" — one `y` left the rest
    /// waiting, easy to miss): with several left `y accept this one` and `a accept all N`; with one, no `a`.
    #[test]
    fn an_edit_reviews_keys_say_this_one_or_all() {
        let mut ed = editor("one\ntwo\nthree\n");
        let id = ed.doc().id;
        let changes = vec![
            (0, 3, "one".to_string(), "ONE".to_string()),
            (8, 13, "three".to_string(), "THREE".to_string()),
        ];
        crate::llm::review_with(&mut ed, id, "upper", changes, None);
        let draw = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 40).unwrap();
            let mut parser = vt100::Parser::new(40, 140, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        let s = draw(&mut ed);
        assert!(s.contains("y accept this one   a accept all 2   n reject"), "{s}");
        feed(&mut ed, "y");
        let s = draw(&mut ed);
        assert!(s.contains("y accept   n reject") && !s.contains("accept all"), "{s}");
    }

    /// #216: one `y` answers the one change there is to answer. A change that changes nothing comes decided
    /// already (left as it is) — the review starts past it: a `y` on the no-op left "1 undecided" and asked for a
    /// second press. On a change decided already (gone back to it), `y` answers that change again — a change
    /// of mind — never another one not looked at.
    #[test]
    fn one_y_answers_the_one_open_change() {
        let mut ed = editor("one\ntwo\n");
        let id = ed.doc().id;
        // (the first change changes nothing — decided at once)
        let changes =
            vec![(0, 4, "one\n".to_string(), "one\n".to_string()), (4, 8, "two\n".into(), "TWO\n".into())];
        crate::llm::review_with(&mut ed, id, "upper", changes, None);
        assert_eq!(ed.session.review.as_ref().unwrap().current, 1, "on the open change");
        feed(&mut ed, "y");
        assert!(ed.session.review.is_none(), "one y settles it");
        assert_eq!(ed.doc().text.to_string(), "one\nTWO\n");

        // Gone back to a change decided already: `y` changes that answer, the open one still waits
        let changes =
            vec![(0, 4, "one\n".to_string(), "ONE\n".to_string()), (4, 8, "TWO\n".into(), "two\n".into())];
        crate::llm::review_with(&mut ed, id, "swap", changes, None);
        let rev = ed.session.review.as_mut().unwrap();
        rev.hunks[0].decision = crate::llm::Decision::Reject;
        rev.current = 0;
        feed(&mut ed, "y");
        let rev = ed.session.review.as_ref().expect("the other change is still open");
        assert_eq!(rev.hunks[0].decision, crate::llm::Decision::Accept, "the change of mind");
        assert_eq!(
            (rev.current, rev.hunks[1].decision),
            (1, crate::llm::Decision::Undecided),
            "not answered"
        );
        feed(&mut ed, "n");
        assert_eq!(ed.doc().text.to_string(), "ONE\nTWO\n");
    }

    /// An edit review in front is said once on the status line — its mode, `● change` (`review` is the code
    /// review's rail, uxaudit D5); the right-hand `claude ● change` only while it waits behind the code's keys.
    #[test]
    fn the_review_in_front_is_said_once_on_the_status_line() {
        let mut ed = editor("one\ntwo\n");
        let id = ed.doc().id;
        crate::llm::review_with(&mut ed, id, "grow", vec![(0, 4, "one\n".into(), "uno\n".into())], None);
        let status = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 30).unwrap();
            let mut parser = vt100::Parser::new(30, 140, 0);
            parser.process(&bytes);
            parser.screen().rows(0, 140).nth(29).unwrap_or_default()
        };
        // In front, with the keys: the mode says it — no `claude ● change` beside
        let s = status(&mut ed);
        assert!(s.contains("● change ") && !s.contains("claude ● change"), "{s}");
        // Left unanswered: the code's mode, and the chip says Claude still waits on it
        feed(&mut ed, "<esc>");
        assert!(ed.session.review.is_some());
        let s = status(&mut ed);
        assert!(s.contains("● normal ") && s.contains("claude ● change"), "{s}");
        // Back to it: said once again
        feed(&mut ed, "<C-q>");
        let s = status(&mut ed);
        assert!(s.contains("● change ") && !s.contains("claude ● change"), "{s}");
    }

    /// #61, #70: while an edit review waits, the editor's keys still work over it — C-d/C-u and the wheel
    /// scroll it (a long insertion was cut off), `space` opens what it opens, the picker has the keys — and
    /// Esc goes back to the chat with the change unanswered (it declined it): typed keys are the chat's
    /// until C-q gives them back to the review. With no chat, Esc leaves it waiting.
    #[test]
    fn an_edit_review_waits_while_you_look_around() {
        let mut ed = editor("one\ntwo\n");
        let id = ed.doc().id;
        let long: String = (1..=80).map(|i| format!("new line {i}\n")).collect();
        crate::llm::review_with(&mut ed, id, "grow", vec![(0, 4, "one\n".into(), long.clone())], None);
        let draw = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 30).unwrap();
            let mut parser = vt100::Parser::new(30, 140, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        let s = draw(&mut ed);
        assert!(!s.contains("new line 60"), "below the screen");
        // Scrolling: keys and the wheel
        feed(&mut ed, "<C-d><C-d><C-d><C-d>");
        let s = draw(&mut ed);
        assert!(s.contains("new line 60"), "scrolled to it: {s}");
        let at = ed.session.review.as_ref().unwrap().scroll;
        feed(&mut ed, "<C-u>");
        assert!(ed.session.review.as_ref().unwrap().scroll < at);
        let wheel = |ed: &mut Editor, kind| {
            ed.handle_mouse(crate::event::Mouse { kind, x: 20, y: 10, alt: false, ctrl: false });
        };
        let at = ed.session.review.as_ref().unwrap().scroll;
        wheel(&mut ed, crate::event::MouseKind::ScrollDown);
        assert_eq!(ed.session.review.as_ref().unwrap().scroll, at + 3, "the wheel scrolls it");
        // `space`: its keys open what they open; the picker has the keys, the review waits
        feed(&mut ed, "<space>b");
        assert!(ed.picker.is_some(), "space b opened over the review");
        feed(&mut ed, "<esc>");
        assert!(ed.picker.is_none() && ed.session.review.is_some(), "the picker went, the review waits");
        // #118: it took the keys from the code — Esc gives them back to the code, unanswered: NORMAL, its
        // keys the editor's (a `j` moves the cursor, a `y` answers nothing) until C-q
        feed(&mut ed, "<esc>");
        assert!(ed.session.review.is_some(), "esc didn't answer it");
        assert_eq!(ed.mode, Mode::Normal);
        // (the change's row, scrolled past above)
        ed.session.review.as_mut().unwrap().follow = true;
        let s = draw(&mut ed);
        assert!(s.contains("C-q answer it — the code has the keys"), "{s}");
        assert_eq!(ed.keys_at().label(), None, "the status line says the code's mode, not REVIEW");
        let line = ed.cursor_line();
        feed(&mut ed, "jy");
        assert_eq!(ed.cursor_line(), line + 1, "the code's j");
        let undecided = |ed: &Editor| {
            ed.session.review.as_ref().is_some_and(|r| r.hunks[0].decision == crate::llm::Decision::Undecided)
        };
        assert!(undecided(&ed), "y was the code's");
        feed(&mut ed, "<C-q>");
        assert!(!ed.session.review.as_ref().unwrap().away, "C-q: the review has the keys again");
        // A chat that had the keys when the review took them: Esc goes back to it, unanswered
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        crate::chat::open(&mut ed);
        ed.session.leave(crate::session::At::Chat);
        ed.session.review.as_mut().unwrap().from_chat = true;
        feed(&mut ed, "<tab>");
        let s = draw(&mut ed);
        assert!(s.contains("esc back, unanswered"), "{s}");
        // Narrower (the chat beside it): the short form, esc's still there
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 100, 30).unwrap();
        let mut parser = vt100::Parser::new(30, 100, 0);
        parser.process(&bytes);
        let s = parser.screen().contents();
        assert!(s.contains("y yes  n no  esc back"), "{s}");
        feed(&mut ed, "<esc>");
        assert!(ed.session.is_at(crate::session::At::Chat), "back in the chat");
        let s = draw(&mut ed);
        assert!(s.contains("C-q answer it — the chat has the keys"), "{s}");
        feed(&mut ed, "yn");
        assert_eq!(ed.session.chat.as_ref().unwrap().input, "yn", "typed into the chat, not an answer");
        assert!(
            ed.session
                .review
                .as_ref()
                .is_some_and(|r| r.hunks[0].decision == crate::llm::Decision::Undecided)
        );
        // C-q: the review has the keys again
        feed(&mut ed, "<C-q>y");
        assert!(ed.session.review.is_none(), "answered");
        assert!(text(&ed).starts_with("new line 1\n") && text(&ed).ends_with("new line 80\ntwo\n"));
    }

    /// #29: the git tab reads the status again when Claude edits, writes or runs something — not only when
    /// it's reopened ("nothing to commit — the tree is clean" over files Claude had changed). Mid-turn: the
    /// fake claude changes the file (standing in for its Edit), then sends the edit's result, then waits.
    #[test]
    fn the_git_tab_follows_claudes_edits() {
        let repo = git_repo("git-follow", &[("a.txt", "one\n")]);
        let file = repo.join("a.txt");
        let mut ed = editor("");
        ed.session.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone()));
        ed.session.at = crate::session::At::Side;
        ed.git_refresh();
        settle_until(&mut ed, |ed| ed.session.git_status.as_ref().is_some_and(|s| s.unstaged.is_empty()));
        let edit = serde_json::json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "e1",
            "name": "Edit", "input": {"file_path": file, "old_string": "one", "new_string": "two"}}]}});
        let result = serde_json::json!({"type": "user", "message": {"content": [{"type": "tool_result",
            "tool_use_id": "e1", "content": "edited"}]}});
        let write = format!("printf 'two\\n' > '{}'", file.display());
        fake_claude_stream(
            &mut ed,
            &[
                edit,
                json_str(&write),
                result,
                json_str("sleep 2"),
                serde_json::json!({"type": "result", "result": "ok"}),
            ],
        );
        feed(&mut ed, "<space>lchange it<ret>");
        let dirty = |ed: &Editor| ed.session.git_status.as_ref().is_some_and(|s| !s.unstaged.is_empty());
        settle_for(&mut ed, |ed| dirty(ed) || !ed.session.chat.as_ref().unwrap().busy());
        assert!(dirty(&ed) && ed.session.chat.as_ref().unwrap().busy(), "the tab caught up mid-turn");
        std::fs::remove_dir_all(&repo).ok();
    }

    /// #31: a paste in normal mode into a file under review (the review took the keys at the turn's end)
    /// goes to the chat, saying so — it doesn't land in the file as an unsaved edit; in insert mode it's
    /// typed there on purpose.
    #[test]
    fn a_paste_into_a_file_under_review_goes_to_the_chat() {
        let dir = git_repo("paste-review", &[("a.rs", "fn a() {}\n")]);
        std::fs::write(dir.join("a.rs"), "fn a() { 1 }\n").unwrap();
        let mut ed = editor("");
        crate::chat::open(&mut ed);
        ed.session.leave(crate::session::At::Chat);
        crate::codereview::start_in(&mut ed, dir.clone(), None);
        settle_until(&mut ed, |ed| ed.session.code_review.is_some() && ed.doc().review_base.is_some());
        let before = ed.doc().text.to_string();
        ed.handle_paste("meant for the chat".into());
        assert_eq!(ed.doc().text.to_string(), before, "the file under review is as it was");
        let c = ed.session.chat.as_ref().unwrap();
        assert!(ed.session.is_at(At::Chat) && c.input == "meant for the chat", "{:?}", c.input);
        assert!(ed.status.as_ref().is_some_and(|s| s.0.contains("pasted into the chat")), "{:?}", ed.status);
        // Insert mode: typed there on purpose
        ed.session.leave(crate::session::At::Chat);
        feed(&mut ed, "i");
        ed.handle_paste("X".into());
        assert_ne!(ed.doc().text.to_string(), before);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #83: `space v z` reads the review wide — the rail and the chat give the code their columns, the
    /// header says so; going to the chat brings it while it has the keys; again, both are back. Without a
    /// review it says why.
    #[test]
    fn space_v_z_reads_the_review_wide() {
        let dir = git_repo("broad", &[("a.rs", "fn a() {}\n")]);
        std::fs::write(dir.join("a.rs"), format!("fn a() {{ {} }}\n", "x".repeat(150))).unwrap();
        let mut ed = editor("");
        feed(&mut ed, "<space>vz");
        assert!(ed.toasts.iter().any(|t| t.text.contains("no review open")), "{:?}", ed.toasts);
        crate::chat::open(&mut ed);
        ed.session.leave(crate::session::At::Chat);
        crate::codereview::start_in(&mut ed, dir.clone(), None);
        settle_until(&mut ed, |ed| ed.session.code_review.is_some() && ed.doc().review_base.is_some());
        // (the code's columns: its pane's width · the header's row)
        let frame = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 200, 40).unwrap();
            let mut parser = vt100::Parser::new(40, 200, 0);
            parser.process(&bytes);
            let head = parser.screen().contents().lines().next().unwrap_or_default().to_string();
            (ed.screen.panes[0].1.x, ed.screen.panes[0].1.w, ed.screen.chat_w, head)
        };
        let (x, w, chat, head) = frame(&mut ed);
        assert!(x > 0 && chat > 0 && x + w + chat == 200, "rail {x}, code {w}, chat {chat}");
        assert!(!head.contains("· wide"), "{head}");
        feed(&mut ed, "<space>vz");
        let (x, w, chat, head) = frame(&mut ed);
        assert_eq!((x, w, chat), (0, 200, 0), "the code has the screen");
        assert!(head.contains("review 1/1 · wide"), "{head}");
        // The chat with the keys: back while it has them
        ed.session.at = crate::session::At::Chat;
        assert!(frame(&mut ed).2 > 0, "the chat has the keys: it shows");
        ed.session.leave(crate::session::At::Chat);
        assert_eq!(frame(&mut ed).2, 0, "and goes again");
        feed(&mut ed, "<space>vz");
        let (x, _, chat, _) = frame(&mut ed);
        assert!(x > 0 && chat > 0, "both back");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn json_str(s: &str) -> serde_json::Value {
        serde_json::Value::String(s.to_string())
    }

    /// Stopping Claude's command (`space m c`) tells Claude so — it doesn't wait for an end that won't come.
    #[test]
    fn stopping_claudes_command_tells_it() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-stop-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let mut ed = editor("");
        ed.config.llm.run_allow = vec!["sleep".into()];
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": "sleep 30" }));
        feed(&mut ed, "<space>lwait<ret>");
        settle_for(&mut ed, |ed| ed.session.task_run.is_some());
        ed.task_close();
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        assert!(answer.contains("the command was stopped — the user stopped it"), "{answer}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Claude's run gets the prefix its containers are named from (`TARAE_CONTAINER_PREFIX` — no command line
    /// of Claude's is rewritten: a script names them from it), this tarae's, this session's, this run's; the
    /// user's own task doesn't, and a value from tarae's own environment never reaches either.
    #[test]
    fn claudes_run_is_told_the_prefix_its_containers_are_named_from() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-prefix-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let mut ed = editor("");
        ed.config.llm.run_allow = vec!["printenv".into()];
        let log =
            claude_calls(&mut ed, &dir, "run", &json!({ "command": "printenv TARAE_CONTAINER_PREFIX" }));
        feed(&mut ed, "<space>lname it<ret>");
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        let r = ed.session.task_run.as_ref().unwrap();
        let prefix = crate::containers::prefix(ed.session.id, r.trace.id());
        assert!(answer.contains("exit 0") && answer.contains(&prefix), "{prefix}: {answer}");
        // The user's own task: none
        let task = crate::task::Task {
            name: "env".into(),
            cmd: "printenv TARAE_CONTAINER_PREFIX || echo none".into(),
            cwd: dir.clone(),
        };
        ed.task_start(task);
        settle_for(&mut ed, |ed| {
            ed.session.task_run.as_ref().is_some_and(|r| r.state != crate::task::State::Running)
        });
        let out: Vec<String> =
            ed.session.task_run.as_ref().unwrap().output.iter().map(|l| l.1.into_owned()).collect();
        assert_eq!(out, ["none"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Stopping Claude's command (`space m c`) stops what it started in a container engine — its containers,
    /// by name (a stand-in `docker` here writes down what it's asked, and with which `DOCKER_HOST`), on the
    /// engine its line named — and asks about one made by hand meanwhile; a run that ended by itself leaves its
    /// containers be.
    #[test]
    fn stopping_claudes_command_stops_its_containers() {
        use serde_json::json;
        let scratch = crate::scratch::Scratch::new("run-containers");
        let dir = scratch.to_path_buf();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let asked = dir.join("asked");
        let mut ed = editor("");
        // (today's gate, for its card: in the sandbox it runs — P3)
        ed.config.llm.sandbox = false;
        let line = "DOCKER_HOST=tcp://tarae-test.invalid:2375 sleep 30";
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": line }));
        feed(&mut ed, "<space>lwait<ret>");
        // (its card — it sets a name in front — answered y)
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        ed.offer_accept();
        settle_for(&mut ed, |ed| ed.session.task_run.is_some());
        let prefix =
            ed.session.task_run.as_ref().unwrap().containers.as_ref().expect("Claude's").prefix.clone();
        let script = format!(
            "#!/bin/sh\necho \"$* on ${{DOCKER_HOST-the default}}\" >> '{}'\n[ \"$1\" = ps ] && printf \
             '%s\\t%s\\n' '{prefix}-checks' '2026-10-08 21:17:03 +0900 KST'\nexit 0\n",
            asked.display()
        );
        crate::scratch::write_script(&dir.join("bin/docker"), &script);
        // (the run as tarae started it — the engine its line named — its programs found on the stand-in's PATH)
        let r = ed.session.task_run.as_mut().unwrap();
        r.containers.as_mut().unwrap().find_on(dir.join("bin").as_os_str());
        ed.task_close();
        let stopped = format!("stop {prefix}-checks on tcp://tarae-test.invalid:2375");
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !std::fs::read_to_string(&asked).unwrap_or_default().contains(&stopped) {
            let asked = std::fs::read_to_string(&asked).unwrap_or_default();
            assert!(std::time::Instant::now() < end, "stopped by name, on its engine: {asked}");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(tool_answer(&log).contains("the command was stopped"));
        // Ended by itself (passed): its panel closed, nothing asked of the engine
        std::fs::remove_file(&asked).ok();
        let task = crate::task::Task { name: "done".into(), cmd: "true".into(), cwd: dir.clone() };
        let mut done = crate::task::Run::for_test(task);
        done.by_claude = true;
        done.containers = Some(crate::containers::Run::on(
            &prefix,
            "Claude's command",
            ed.events.sender(),
            &dir.join("bin"),
        ));
        ed.session.task_run = Some(done);
        ed.task_close();
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!asked.exists(), "a run that ended leaves its containers be");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A command already running isn't cut short — Claude's waits its turn (not refused: `queued.rs`) — a turn
    /// that ends takes its card and what waited away, and one too long to show whole isn't put to the user.
    #[test]
    fn run_waits_its_turn() {
        use serde_json::json;
        let mut ed = editor("");
        // (today's gate, for `git push`'s card: in the sandbox it runs — P3)
        ed.config.llm.sandbox = false;
        let call = |ed: &mut Editor, command: &str| {
            let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "run", "arguments": {"command": command}}});
            crate::tools::mcp(ed, 0, "c1", &msg)
        };
        let text = |r: Option<serde_json::Value>| {
            let r = r.expect("answered now");
            (
                r["result"]["content"][0]["text"].as_str().unwrap_or_default().to_string(),
                r["result"]["isError"] == true,
            )
        };
        let (t, err) = text(call(&mut ed, " "));
        assert!(err && t.contains("command is required"), "{t}");
        let (t, err) = text(call(&mut ed, &"echo x ".repeat(100)));
        // (one line — trimmed — only its length is the reason)
        assert!(err && t.contains("it's 699 characters — a card shows a command of at most 600"), "{t}");
        assert!(!t.contains("lines"), "{t}");
        let (t, err) = text(call(&mut ed, &"echo x\n".repeat(13)));
        assert!(err && t.contains("it's 13 lines — a card shows a command of at most 12"), "{t}");
        assert!(!t.contains("characters"), "{t}");
        ed.task_start(crate::task::Task {
            name: "slow".into(),
            cmd: "sleep 5".into(),
            cwd: std::env::temp_dir(),
        });
        assert!(call(&mut ed, "git status").is_none(), "not refused: it waits");
        assert_eq!(ed.session.runs_next.len(), 1);
        let running = ed.session.task_run.as_ref().map(|r| (r.task.name.as_str(), r.state.clone()));
        assert_eq!(running, Some(("slow", crate::task::State::Running)), "not cut short");
        // Asked about (a card holds no slot), then the turn ends: the card goes, and what waited
        assert!(call(&mut ed, "git push").is_none());
        assert_eq!(ed.offers.len(), 1);
        crate::tools::turn_over(&mut ed);
        assert!(ed.offers.is_empty() && ed.session.runs_next.is_empty());
        ed.task_close();
    }

    /// Review at the end: a command that runs a file Claude wrote unseen this turn shows that file whole on
    /// its card — what runs is in sight before `y`. A `y` after the file changed again doesn't run it, and
    /// one too long for a card isn't put to the user.
    #[test]
    fn running_a_file_written_unseen_shows_it_first() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-unseen-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let (script, made) = (dir.join("x.sh"), dir.join("made"));
        let body = format!("echo making\ntouch {}\n", made.display());
        // (no quotes: the fake claude's script is single-quoted — `named` is tested with them in tools.rs)
        let command = format!("sh {}", script.display());
        let start = |body: &str| {
            std::fs::write(&script, body).unwrap();
            std::fs::remove_file(dir.join("stdin.log")).ok();
            let mut ed = editor("");
            // (today's gate, for its card: in the sandbox it runs — P3; past it the card's the same —
            // `tools::the_frictions_commands_run_unasked_in_the_sandbox`)
            ed.config.llm.sandbox = false;
            ed.cards_dir = Some(dir.join("cards"));
            ed.session.checkpoints.turn.push(crate::checkpoint::Touched {
                path: script.clone(),
                before: None,
                after: ropey::Rope::new(),
                unseen: true,
                ..Default::default()
            });
            let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
            feed(&mut ed, "<space>lmake it<ret>");
            (ed, log)
        };
        // Shown on the card, drawn whole; `y` runs it
        let (mut ed, log) = start(&body);
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        let q = ed.offers[0].question.clone();
        assert!(q.starts_with(&command) && q.contains("x.sh") && q.contains(body.trim_end()), "{q}");
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 150, 40).unwrap();
        let mut parser = vt100::Parser::new(40, 150, 0);
        parser.process(&bytes);
        let screen = parser.screen().contents();
        // (words, not a phrase: a long temp path — a sandboxed run's own `TMPDIR` — wraps it)
        assert!(
            screen.contains("echo making") && screen.contains("written") && screen.contains("unseen:"),
            "{screen}"
        );
        assert!(ed.screen.offer_buttons.iter().any(|b| b.3 == 'y'), "y can answer it");
        ed.offer_accept();
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(made.exists(), "{}", tool_answer(&log));
        std::fs::remove_file(&made).ok();
        // (the record has its card, with the command and its run — cardauto 1)
        ed.cards_flush();
        let kept = crate::cards::read_record(&dir.join("cards"));
        let last = kept.last().expect("its card kept");
        assert_eq!(
            (&last["kind"], &last["answer"], &last["what"]),
            (&json!("run"), &json!("y"), &json!(command))
        );
        assert!(last["run"].is_u64(), "{last}");
        // Changed after it was shown: not run, Claude hears why
        let (mut ed, log) = start(&body);
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        let sneaked = dir.join("sneaked");
        std::fs::write(&script, format!("{body}touch {}\n", sneaked.display())).unwrap();
        ed.offer_accept();
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        assert!(answer.contains("changed after the user saw it") && answer.contains("isError"), "{answer}");
        assert!(!made.exists() && !sneaked.exists() && ed.session.task_run.is_none());
        // Too long to show: not asked about
        let (mut ed, log) = start(&"echo x\n".repeat(200));
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        assert!(
            answer.contains("it has 200 lines — a card shows at most 16") && answer.contains("isError"),
            "{answer}"
        );
        // (refused with no card: the record has it all the same — cardauto §8.3)
        ed.cards_flush();
        let kept = crate::cards::read_record(&dir.join("cards"));
        let last = kept.last().expect("the refusal kept");
        assert_eq!((&last["answer"], &last["what"]), (&json!("refused"), &json!(command)), "{last}");
        assert!(last["said"].as_str().is_some_and(|s| s.contains("200 lines")) && last.get("card").is_none());
        assert!(ed.offers.is_empty() && ed.session.task_run.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A turn in a fresh chat of `ed`'s session in which Claude runs `command` (`claude_calls`) — handled until
    /// a card is up or the turn is over; the log of what claude was sent.
    fn run_turn(ed: &mut Editor, dir: &Path, command: &str) -> std::path::PathBuf {
        std::fs::remove_file(dir.join("stdin.log")).ok();
        let log = claude_calls(ed, dir, "run", &serde_json::json!({ "command": command }));
        crate::chat::close(ed);
        feed(ed, "<space>ltouch it<ret>");
        let answered = |ed: &Editor| {
            ed.session.chat.as_ref().is_some_and(|c| !c.busy())
                && std::fs::read_to_string(&log).is_ok_and(|s| s.contains("\"c7\""))
        };
        settle_for(ed, |ed| !ed.offers.is_empty() || answered(ed));
        log
    }

    /// No sandbox on this system: a command of Claude's — a check too — doesn't run quietly without one. The
    /// session's first run asks how its runs go until it ends, in every chat (Q6): `n` none (Claude told why
    /// once, then a line each), `y` this one and a card before each next (where `s` turns it), `s` checks and
    /// reads unasked (the chat's title says so) — a command that asks anyway still has its own card. A restart
    /// asks again.
    #[test]
    fn without_a_sandbox_the_session_asks_once() {
        use crate::offer::What;
        use crate::sandbox::Availability;
        let scratch = crate::scratch::Scratch::new("run-bare");
        let dir = std::fs::canonicalize(&scratch).unwrap();
        let made = dir.join("made");
        let command = format!("touch {}", made.display());
        let start = || {
            let mut ed = editor("");
            ed.sandbox = Availability::Missing("not on this system".into());
            ed.config.llm.run_allow = vec!["touch".into()];
            ed
        };
        let first = |ed: &Editor| {
            matches!(ed.offers.first().map(|o| &o.what), Some(What::NoSandbox { first: true, .. }))
        };
        // n: nothing runs this session, Claude hears why once — the next chat's run, a line, no card
        let mut ed = start();
        let log = run_turn(&mut ed, &dir, &command);
        assert!(first(&ed) && ed.offers[0].weighty());
        let o = &ed.offers[0];
        assert!(o.question.contains(&command) && o.question.contains("not on this system"), "{}", o.question);
        assert_eq!(
            o.answers(false),
            vec![('y', "Ask each time"), ('s', "This session"), ('n', "Don't run them")]
        );
        ed.offer_dismiss();
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        assert!(answer.contains("run none of your commands") && answer.contains("isError"), "{answer}");
        let log = run_turn(&mut ed, &dir, &command);
        assert!(ed.offers.is_empty(), "one card a session");
        assert!(tool_answer(&log).contains("Not run: there's no sandbox here"), "{}", tool_answer(&log));
        assert!(!made.exists() && ed.session.task_run.is_none());
        // y: this one runs, and says how — the next chat's asks for its run (`n` there is that run's), and `s`
        // on such a card holds for the rest
        let mut ed = start();
        let log = run_turn(&mut ed, &dir, &command);
        ed.offer_accept();
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(made.exists() && tool_answer(&log).contains("Ran without a sandbox (not on this system)"));
        assert!(!crate::tools::unsandboxed(&ed), "asked each time");
        std::fs::remove_file(&made).ok();
        let log = run_turn(&mut ed, &dir, &command);
        assert!(matches!(ed.offers[0].what, What::NoSandbox { first: false, .. }), "its run's card");
        assert_eq!(ed.offers[0].answers(false), vec![('y', "Once"), ('s', "This session"), ('n', "No")]);
        ed.offer_dismiss();
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(tool_answer(&log).contains("chose not to run `touch") && !made.exists());
        run_turn(&mut ed, &dir, &command);
        assert!(matches!(ed.offers[0].what, What::NoSandbox { first: false, .. }), "still asked");
        ed.offer_for_session();
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(made.exists() && crate::tools::unsandboxed(&ed));
        std::fs::remove_file(&made).ok();
        // s: unasked in the next chat too — and the title says so
        let mut ed = start();
        run_turn(&mut ed, &dir, &command);
        assert!(first(&ed));
        ed.offer_for_session();
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(made.exists() && crate::tools::unsandboxed(&ed));
        std::fs::remove_file(&made).ok();
        let log = run_turn(&mut ed, &dir, &command);
        assert!(ed.offers.is_empty() && made.exists(), "a new chat, no card: {}", tool_answer(&log));
        std::fs::remove_file(&made).ok();
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 150, 40).unwrap();
        let mut parser = vt100::Parser::new(40, 150, 0);
        parser.process(&bytes);
        assert!(parser.screen().contents().contains("· no sandbox"), "{}", parser.screen().contents());
        // A command that asks anyway: its own card says there's no sandbox, and `y` there is enough
        // (`;` and what isn't a check make it more than a check)
        run_turn(&mut ed, &dir, &format!("{command}; basename x"));
        let o = &ed.offers[0];
        assert!(matches!(&o.what, What::Run(r) if r.unprotected));
        assert!(o.question.contains("No sandbox here (not on this system)"), "{}", o.question);
        ed.offer_accept();
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(ed.offers.is_empty() && made.exists(), "one card, not two");
        std::fs::remove_file(&made).ok();
        // Not past a restart: the tarae it restarts into asks again
        let state = crate::restart::snapshot(&mut ed);
        let mut back = start();
        crate::restart::restore(&mut back, &state);
        assert_eq!(back.session.no_sandbox, None, "never kept");
        run_turn(&mut back, &dir, &command);
        assert!(first(&back) && !made.exists(), "asked again");
    }

    /// A partial sandbox (Linux's Landlock ABI 2 — a stand-in here, any system) asks as none does, once a
    /// session, in its own words: `n` none runs in it (Claude told once), `s` the rest unasked — still in the
    /// sandbox as far as it goes, so the title doesn't say "no sandbox".
    #[test]
    fn a_partial_sandbox_asks_once_a_session() {
        use crate::offer::What;
        let scratch = crate::scratch::Scratch::new("run-partial-once");
        let dir = std::fs::canonicalize(&scratch).unwrap();
        let made = dir.join("made");
        let command = format!("touch {}", made.display());
        let why = "stand-in: this kernel's Landlock (ABI 2) can't control truncating files";
        let start = || {
            let mut ed = editor("");
            ed.sandbox = crate::sandbox::Availability::Partial(why.into());
            ed.config.llm.run_allow = vec!["touch".into()];
            ed
        };
        let mut ed = start();
        let log = run_turn(&mut ed, &dir, &command);
        let o = &ed.offers[0];
        assert!(matches!(o.what, What::NoSandbox { first: true, .. }), "{}", o.title);
        assert_eq!(o.title, crate::tools::PARTIAL_SANDBOX_TITLE);
        assert!(o.question.contains(why) && o.question.contains("goes only so far"), "{}", o.question);
        ed.offer_dismiss();
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(tool_answer(&log).contains("goes only so far") && tool_answer(&log).contains("run none"));
        let log = run_turn(&mut ed, &dir, &command);
        assert!(
            ed.offers.is_empty() && tool_answer(&log).contains("Not run: the sandbox here goes only so far")
        );
        assert!(!made.exists());
        // s: the next chat's run isn't asked (where it runs — the sandbox in a sandbox — isn't this test's)
        let mut ed = start();
        run_turn(&mut ed, &dir, &command);
        ed.offer_for_session();
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert_eq!(ed.session.no_sandbox, Some(crate::tools::NoSandbox::Unasked));
        run_turn(&mut ed, &dir, &command);
        assert!(ed.offers.is_empty(), "one card a session");
        assert!(!crate::tools::unsandboxed(&ed), "agreed, and still sandboxed");
    }

    /// `llm.sandbox = false`: Claude's commands run without one, unasked — and the title and Claude's report
    /// say so.
    #[test]
    fn the_sandbox_turned_off_is_said() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-off-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let mut ed = editor("");
        ed.sandbox = crate::sandbox::Availability::Missing("not on this system".into());
        ed.config.llm.sandbox = false;
        ed.config.llm.run_allow = vec!["touch".into()];
        let made = dir.join("made");
        let log =
            claude_calls(&mut ed, &dir, "run", &json!({ "command": format!("touch {}", made.display()) }));
        feed(&mut ed, "<space>ltouch it<ret>");
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(ed.offers.is_empty() && made.exists());
        assert!(tool_answer(&log).contains("the user turned it off (llm.sandbox)"));
        assert!(crate::tools::unsandboxed(&ed));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// For real, on macOS: Claude's check runs inside the sandbox — it writes in temp, not in a path the
    /// project keeps (`llm.sandbox-protect`, a harmless marker under `target/`), and Claude reads that it
    /// ran sandboxed. Inside a sandbox already (Claude's own `cargo test`) one can't start: skipped, said.
    #[cfg(target_os = "macos")]
    #[test]
    fn claudes_check_runs_in_the_sandbox() {
        use serde_json::json;
        if crate::sandbox::skipped_inside("editor::tests::claudes_check_runs_in_the_sandbox") {
            return;
        }
        let dir = std::env::temp_dir().join(format!("tarae-run-sb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let kept = std::env::current_dir().unwrap().join(format!("target/sb-kept-{}", std::process::id()));
        std::fs::create_dir_all(&kept).unwrap();
        let (made, marker) = (dir.join("made"), kept.join("marker"));
        let mut ed = editor("");
        ed.sandbox = crate::sandbox::Availability::Ready;
        ed.config.llm.run_allow = vec!["touch".into()];
        ed.config.llm.sandbox_protect = vec![format!("target/sb-kept-{}", std::process::id())];
        let command = format!("touch {} {}", made.display(), marker.display());
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
        feed(&mut ed, "<space>ltouch them<ret>");
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(made.exists(), "temp is writable");
        assert!(!marker.exists(), "a kept path isn't");
        let answer = tool_answer(&log);
        assert!(
            answer.contains("Operation not permitted") && answer.contains("In tarae's sandbox"),
            "{answer}"
        );
        // What was blocked, watched — the marker by name (JSON-escaped in the log), not a guess
        assert!(answer.contains("The sandbox blocked one thing"), "{answer}");
        assert!(
            answer.contains("  writing ") && answer.contains("marker (file-write-create, touch)"),
            "{answer}"
        );
        assert!(!crate::tools::unsandboxed(&ed));
        // (9d: the session's sandbox is its run's, in the state)
        assert_eq!(ed.ui_state().json()["sessions"][0]["sandbox"]["by"], "seatbelt");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&kept).ok();
    }

    /// Linux tests that need this kernel's Landlock at `abi` or more: None after saying the test skips itself.
    #[cfg(target_os = "linux")]
    fn landlock_at_least(abi: u32, test: &str) -> Option<u32> {
        match crate::landlock::abi() {
            Ok(n) if n >= abi => Some(n),
            got => {
                let line = format!(
                    "{}editor::tests::{test} — needs Landlock ABI {abi}, this kernel: {got:?}\n",
                    crate::test_results::SELF_SKIP
                );
                let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
                None
            }
        }
    }

    /// For real, on Linux: Claude's check runs confined by Landlock — it writes in its project, not in a
    /// folder outside it (a marker under the build's `target/`) — and Claude reads that it ran sandboxed,
    /// what was refused and where that was read from, and how Linux's sandbox differs.
    #[cfg(target_os = "linux")]
    #[test]
    fn claudes_check_runs_in_the_linux_sandbox() {
        use serde_json::json;
        let Some(abi) =
            landlock_at_least(crate::sandbox::linux::FULL, "claudes_check_runs_in_the_linux_sandbox")
        else {
            return;
        };
        let dir = std::env::temp_dir().join(format!("tarae-run-ll-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let kept = std::env::current_dir().unwrap().join(format!("target/ll-kept-{}", std::process::id()));
        std::fs::create_dir_all(&kept).unwrap();
        let (made, marker) = (dir.join("made"), kept.join("marker"));
        let mut ed = editor("");
        ed.sandbox = crate::sandbox::Availability::Ready;
        ed.config.llm.run_allow = vec!["touch".into()];
        let command = format!("touch {} {}", made.display(), marker.display());
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
        feed(&mut ed, "<space>ltouch them<ret>");
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(made.exists(), "the project is writable");
        assert!(!marker.exists(), "outside it isn't");
        let answer = tool_answer(&log).replace("\\n", " ");
        let answer = answer.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(answer.contains("Permission denied") && answer.contains("In tarae's sandbox"), "{answer}");
        // (read from the output — below: the output's words, not a record of tarae's)
        assert!(answer.contains("Its output says one thing was refused"), "{answer}");
        // (a checkout in home, as on the user's VM: reads are kept there too, so either is refused)
        let in_home = std::env::var_os("HOME").is_some_and(|h| !h.is_empty() && marker.starts_with(h));
        let op = if in_home { crate::sandbox::EITHER } else { "file-write*" };
        assert!(answer.contains(&format!("marker ({op}, said in the output)")), "{answer}");
        // (piece C: the network closed goes beyond through tarae's proxy, each host on the user's card)
        let net = match crate::sandbox::supervisor::unavailable() {
            None => "each host only once the user said yes to it on its card (llm.sandbox-network is off)",
            Some(_) => "make no socket",
        };
        assert!(answer.contains(net), "{answer}");
        if abi < 7 {
            assert!(
                answer.contains(&format!(
                    "(Read from its own error lines — this kernel's Landlock (ABI {abi})"
                ))
            );
        }
        assert!(!crate::tools::unsandboxed(&ed));
        // (9d: the session's sandbox is its run's keeper's, in the state)
        let state = ed.ui_state().json()["sessions"][0]["sandbox"].clone();
        assert_eq!(
            (&state["by"], &state["abi"]),
            (&serde_json::json!("landlock"), &serde_json::json!(abi)),
            "{state}"
        );
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&kept).ok();
    }

    /// For real, on Linux: a command of Claude's that removes the empty folders inside `.git` (its refs packed
    /// — `REMOVE_DIR` reaches there) leaves a repository git still finds: they're made again before its end
    /// is told, and Claude reads that they were. (The test's own repository in a temp folder; git's search
    /// upward stops there.)
    #[cfg(target_os = "linux")]
    #[test]
    fn a_claude_run_cant_leave_git_without_its_refs() {
        use serde_json::json;
        if landlock_at_least(crate::sandbox::linux::FULL, "a_claude_run_cant_leave_git_without_its_refs")
            .is_none()
        {
            return;
        }
        // (under the build's `target/`, not in `/tmp`, where a run may write anything — the project's)
        let dir =
            std::env::current_dir().unwrap().join(format!("target/tarae-run-refs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\n").unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false"])
                .args(["-c", "user.email=t@example.invalid", "-c", "user.name=t"])
                .args(args)
                .env("GIT_CEILING_DIRECTORIES", dir.parent().unwrap())
                .output()
                .unwrap()
        };
        for args in
            [&["init", "-q"][..], &["add", "-A"], &["commit", "-q", "-m", "base"], &["pack-refs", "--all"]]
        {
            assert!(git(args).status.success(), "git {args:?}");
        }
        let mut ed = editor("");
        ed.session.root = dir.clone();
        ed.sandbox = crate::sandbox::Availability::Ready;
        ed.config.llm.run_allow = vec!["rmdir".into()];
        let refs = dir.join(".git/refs");
        let command = format!("rmdir {0}/heads {0}/tags {0}", refs.display());
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
        feed(&mut ed, "<space>lempty them<ret>");
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log).replace("\\n", " ");
        let answer = answer.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(answer.contains("exit 0"), "it ran: {answer}");
        assert!(dir.join(".git/refs/heads").is_dir() && dir.join(".git/refs/tags").is_dir(), "{answer}");
        assert!(git(&["rev-parse", "--git-dir"]).status.success(), "git finds it");
        assert!(answer.contains("It removed empty folders inside protected ones — .git/refs"), "{answer}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Landlock ABI 2 (a stand-in: this kernel's rules, the availability ABI 2 has): the session's first run
    /// of Claude's asks first, saying why — `n` tells Claude, `y` runs it in the sandbox, `s` the rest of the
    /// session's too (still sandboxed: the title doesn't say "no sandbox").
    #[cfg(target_os = "linux")]
    #[test]
    fn a_partial_sandbox_asks_each_run() {
        use serde_json::json;
        if landlock_at_least(2, "a_partial_sandbox_asks_each_run").is_none() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("tarae-run-partial-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let made = dir.join("made");
        let why = "this kernel's Landlock (ABI 2) can't control truncating files";
        let start = || {
            std::fs::remove_file(&made).ok();
            std::fs::remove_file(dir.join("stdin.log")).ok();
            let mut ed = editor("");
            ed.sandbox = crate::sandbox::linux::availability(Ok(2), true);
            ed.config.llm.run_allow = vec!["touch".into()];
            let command = format!("touch {}", made.display());
            let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
            feed(&mut ed, "<space>ltouch it<ret>");
            settle_for(&mut ed, |ed| !ed.offers.is_empty());
            (ed, log)
        };
        let (mut ed, log) = start();
        let o = &ed.offers[0];
        assert_eq!(o.title, crate::tools::PARTIAL_SANDBOX_TITLE);
        assert!(o.question.contains(why) && o.weighty(), "{}", o.question);
        ed.offer_dismiss();
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(tool_answer(&log).contains("goes only so far") && !made.exists());
        let (mut ed, log) = start();
        ed.offer_accept();
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(made.exists());
        assert!(tool_answer(&log).contains("In tarae's sandbox"), "{}", tool_answer(&log));
        let (mut ed, _) = start();
        ed.offer_for_session();
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(made.exists() && !crate::tools::unsandboxed(&ed), "agreed, and still sandboxed");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Landlock ABI 1 is no sandbox (the user's decision, 2026-10-05): the no-sandbox card, saying why, and
    /// `y` runs the command without one, as on a kernel without Landlock.
    #[cfg(target_os = "linux")]
    #[test]
    fn landlock_abi_1_is_no_sandbox() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-abi1-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let made = dir.join("made");
        let mut ed = editor("");
        ed.sandbox = crate::sandbox::linux::availability(Ok(1), true);
        ed.config.llm.run_allow = vec!["touch".into()];
        let log =
            claude_calls(&mut ed, &dir, "run", &json!({ "command": format!("touch {}", made.display()) }));
        feed(&mut ed, "<space>ltouch it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        let o = &ed.offers[0];
        let why = "this kernel's Landlock (ABI 1) refuses every move across folders, so tarae doesn't use it";
        assert!(matches!(o.what, crate::offer::What::NoSandbox { .. }), "{}", o.title);
        assert!(o.question.contains(why), "{}", o.question);
        ed.offer_accept();
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(made.exists());
        let answer = tool_answer(&log);
        assert!(answer.contains(&format!("Ran without a sandbox ({why})")), "{answer}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Linux, tarae serving this machine's ports: a Gradle run of Claude's goes in the sandbox like any other
    /// — no card first — and is kept to it (a stand-in `./gradlew` writes in the project and a name the
    /// sandbox keeps).
    #[cfg(target_os = "linux")]
    #[test]
    fn gradle_runs_in_the_sandbox_on_linux() {
        use serde_json::json;
        use std::os::unix::fs::PermissionsExt;
        if landlock_at_least(crate::sandbox::linux::FULL, "gradle_runs_in_the_sandbox_on_linux").is_none() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("tarae-run-gradle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let gradlew = dir.join("gradlew");
        // (in a folder of its own: one holding a protected name — `gradlew` — takes no new entry on Linux)
        std::fs::create_dir_all(dir.join("build")).unwrap();
        let script = "#!/bin/sh\ncd \"$(dirname \"$0\")\"; echo in > build/built.txt; echo out > build.rs\n";
        std::fs::write(&gradlew, script).unwrap();
        std::fs::set_permissions(&gradlew, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut ed = editor("");
        ed.sandbox = crate::sandbox::Availability::Ready;
        ed.config.llm.run_allow = vec![gradlew.display().to_string()];
        let command = format!("{} test", gradlew.display());
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
        feed(&mut ed, "<space>ltest it<ret>");
        settle_for(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy() || !ed.offers.is_empty());
        assert!(ed.offers.is_empty(), "no card: {:?}", ed.offers.first().map(|o| &o.title));
        assert!(dir.join("build/built.txt").exists(), "it ran: {}", tool_answer(&log));
        assert!(!dir.join("build.rs").exists(), "in the sandbox");
        assert!(tool_answer(&log).contains("In tarae's sandbox"), "{}", tool_answer(&log));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A sandboxed run that was denied something: its panel says what, on its bottom row — and once it has
    /// printed nothing for a while, that it may be waiting on it. The same in the test pane.
    #[test]
    fn the_panel_says_what_the_sandbox_blocked() {
        let screen = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 30).unwrap();
            let mut parser = vt100::Parser::new(30, 140, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        let watch = crate::sandbox::Watch::quiet("t.");
        let policy = crate::sandbox::Policy {
            watch: Some(watch.clone()),
            ..crate::sandbox::Policy::for_project(&crate::sandbox::Env::default())
        };
        let mut ed = editor("");
        let mut run = crate::task::Run::for_test(crate::task::Task {
            name: "cargo test".into(),
            cmd: "cargo test".into(),
            cwd: std::env::temp_dir(),
        });
        run.state = crate::task::State::Running;
        run.guard = Some(crate::sandbox::Guard::In(Box::new(policy.clone())));
        ed.session.task_run = Some(run);
        assert!(!screen(&mut ed).contains("sandbox blocked"), "nothing blocked: no line");
        let d = |op: &str, target: &str| crate::sandbox::Denial {
            process: "cargo".into(),
            op: op.into(),
            target: target.into(),
        };
        // (a write outside: its words are short — a terminal's are long and cut, `denials_in_plain_words`)
        watch.record(d("file-write-data", "/opt/tarae-test/x"));
        watch.record(d("network-outbound", "192.0.2.1:9"));
        let s = screen(&mut ed);
        assert!(s.contains("▲ sandbox blocked writing /opt/tarae-test/x  +1"), "{s}");
        assert!(!s.contains("may be waiting"), "it printed just now");
        ed.session.task_run.as_mut().unwrap().last_output -= crate::sandbox::QUIET;
        assert!(screen(&mut ed).contains("— it may be waiting on what was blocked"));
        // The test pane, the same
        ed.session.task_run = None;
        let mut t = crate::testing::TestRun::finished("tests", Vec::new(), vec!["running 1 test".into()]);
        t.guard = Some(crate::sandbox::Guard::In(Box::new(policy)));
        ed.session.test_run = Some(t);
        let s = screen(&mut ed);
        assert!(
            s.contains("▲ sandbox blocked writing /opt/tarae-test/x  +1") && !s.contains("may be waiting"),
            "{s}"
        );
    }

    /// What the sandbox blocked in Claude's command is the user's to open (`space m s`): a list of the
    /// folders the denied write could open and running it outside once; a folder's card `a` lets Claude's
    /// commands write there this conversation and runs the command again now; `A` keeps one for the
    /// project; outside runs it once with no sandbox. Each run again is the user's: Claude hears how it ended
    /// with the next message. Harmless: the command is an `echo`, the folders this test's temp ones.
    /// A step that's one job (`Editor::step`): said while its work runs, and over before what came of it is
    /// said — it never lingers under its own result.
    #[test]
    fn a_step_is_said_until_its_result_lands() {
        let mut ed = editor("");
        let (go, wait) = std::sync::mpsc::channel::<()>();
        let work = move || {
            let _ = wait.recv();
            7
        };
        ed.step(ed.events.jobs(), "t", "working…", work, |ed, n| {
            assert!(!ed.toasts.iter().any(|t| t.step == Some("t")), "over before its result");
            ed.set_success(format!("got {n}"));
        });
        assert!(
            ed.toasts.iter().any(|t| t.step == Some("t") && t.text.contains("working…")),
            "said meanwhile"
        );
        go.send(()).unwrap();
        settle_for(&mut ed, |ed| ed.toasts.iter().any(|t| t.text.contains("got 7")));
        assert!(!ed.toasts.iter().any(|t| t.step.is_some()));
    }

    #[test]
    fn the_user_opens_what_the_sandbox_blocked() {
        let scratch = crate::scratch::Scratch::new("blocked-ks");
        let dir = scratch.to_path_buf();
        std::fs::create_dir_all(dir.join("cache/deep")).unwrap();
        let (deep, cache) = (dir.join("cache/deep"), dir.join("cache"));
        let store = dir.join("state/sandbox.json");
        let mut ed = editor("");
        // (the project is this repo: the temp folder the writes were denied in is outside it)
        ed.session.root = this_repo();
        ed.allow_path = Some(store.clone());
        // A chat to hear it (a claude that reads and says nothing)
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        crate::chat::open(&mut ed);
        // Claude's command, and what the sandbox blocked in it
        let watch = crate::sandbox::Watch::quiet("t.");
        watch.record(crate::sandbox::Denial {
            process: "echo".into(),
            op: "file-write-create".into(),
            target: deep.join("new/x.bin").display().to_string(),
        });
        let policy = crate::sandbox::Policy {
            watch: Some(watch),
            ..crate::sandbox::Policy::for_project(&crate::sandbox::Env::default())
        };
        let cmd = "echo tarae-rerun-marker";
        let mut run = crate::task::Run::for_test(crate::task::Task {
            name: cmd.into(),
            cmd: cmd.into(),
            cwd: dir.clone(),
        });
        run.guard = Some(crate::sandbox::Guard::In(Box::new(policy)));
        run.by_claude = true;
        ed.session.task_run = Some(run);
        let screen = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 160, 40).unwrap();
            let mut parser = vt100::Parser::new(40, 160, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        assert!(screen(&mut ed).contains("space m s open it"), "the panel says the key");
        // The list: the folders, widest last, then running it outside
        ed.session.leave(crate::session::At::Chat);
        feed(&mut ed, " ms");
        settle_for(&mut ed, |ed| ed.picker.is_some());
        let rows: Vec<String> = ed.picker.as_ref().unwrap().items().iter().map(|i| i.label.clone()).collect();
        assert_eq!(rows[0], format!("allow writing in {}", crate::allow::tilde(&deep)), "{rows:?}");
        assert_eq!(rows[1], format!("allow writing in {}", crate::allow::tilde(&cache)), "{rows:?}");
        assert_eq!(rows.last().unwrap(), "run it outside the sandbox, once");
        // A card in sight long enough for an answer
        let settled = |ed: &mut Editor, title: &str| {
            assert_eq!(ed.offers.first().map(|o| o.title.clone()).as_deref(), Some(title));
            let s = screen(ed);
            let o = ed.offers.first().map(|o| (o.id, o.rev)).unwrap();
            ed.card_focus = Some((o.0, o.1));
            s
        };
        let told = |ed: &Editor, what: &str| ed.session.checkpoints.tell.iter().any(|t| t.contains(what));
        let ran_again = |ed: &Editor| {
            ed.session
                .task_run
                .as_ref()
                .is_some_and(|r| r.state != crate::task::State::Running && r.tell.is_none())
                && ed.session.checkpoints.tell.iter().any(|t| t.contains("tarae-rerun-marker"))
        };
        // `a`: this conversation — and it runs again now
        feed(&mut ed, "<ret>");
        let s = settled(&mut ed, crate::blocked::GRANT_TITLE);
        assert!(s.contains("y Once") && s.contains("a This chat") && s.contains("A Always"), "{s}");
        feed(&mut ed, "a");
        assert!(ed.session.chat.as_ref().unwrap().allowed.write.contains(&deep));
        assert!(told(&ed, "The user let your commands write in"));
        settle_until(&mut ed, ran_again);
        assert!(told(&ed, &format!("The user ran `{cmd}` again, letting it write in")));
        // The rows are the list's own (`Action::Grant`), not a `:` command to complete or type
        assert!(crate::cmdline::find("sandbox-grant").is_none());
        assert!(crate::typed::execute(&mut ed, "sandbox-grant 1").is_err());
        // `A` on the wider one: kept for the project
        ed.session.checkpoints.tell.clear();
        ed.sandbox_grant("1").unwrap();
        settled(&mut ed, crate::blocked::GRANT_TITLE);
        feed(&mut ed, "A");
        let project = ed.session.root.clone();
        let kept =
            |p: &Path| crate::allow::Store::read(Some(&store)).get(&project).write.contains(&p.to_path_buf());
        settle_until(&mut ed, |ed| kept(&cache) && ran_again(ed));
        // Outside, once
        ed.session.checkpoints.tell.clear();
        ed.sandbox_grant("outside").unwrap();
        let s = settled(&mut ed, crate::blocked::OUTSIDE_TITLE);
        assert!(s.contains("y Run") && !s.contains("A Always"), "{s}");
        feed(&mut ed, "y");
        settle_until(&mut ed, ran_again);
        assert!(told(&ed, "again outside the sandbox, this once"));
        assert!(matches!(
            ed.session.task_run.as_ref().and_then(|r| r.guard.clone()),
            Some(crate::sandbox::Guard::Outside)
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `A` on what the sandbox blocked, with no chat open and a take-back of it whose write failed: the run again
    /// has the answer just given — not only once it's written.
    #[test]
    fn the_run_again_after_an_always_has_the_answer() {
        let scratch = crate::scratch::Scratch::new("blocked-always");
        let dir = scratch.to_path_buf();
        std::fs::create_dir_all(dir.join("cache")).unwrap();
        std::fs::write(dir.join("file"), "").unwrap();
        let (cache, store) = (dir.join("cache"), dir.join("sandbox.json"));
        let mut ed = editor("");
        ed.session.root = this_repo();
        ed.allow_path = Some(store.clone());
        // Kept for the project, then taken back with the write failing: until you quit
        let project = crate::sandbox::profile_path(&ed.session.root);
        crate::allow::keep(&store, &project, |a| a.add_write(cache.clone())).unwrap();
        ed.allow_path = Some(dir.join("file/sandbox.json"));
        ed.sandbox_disallow(&cache.display().to_string()).unwrap();
        settle_until(&mut ed, |ed| ed.messages.iter().any(|m| m.2.contains("taken back until you quit")));
        ed.allow_path = Some(store.clone());
        // A run of Claude's the sandbox refused a write in it
        let watch = crate::sandbox::Watch::quiet("t.");
        watch.record(crate::sandbox::Denial {
            process: "echo".into(),
            op: "file-write-create".into(),
            target: cache.join("x.bin").display().to_string(),
        });
        let policy = crate::sandbox::Policy {
            watch: Some(watch),
            ..crate::sandbox::Policy::for_project(&crate::sandbox::Env::default())
        };
        let cmd = "echo tarae-always-marker";
        let mut run = crate::task::Run::for_test(crate::task::Task {
            name: cmd.into(),
            cmd: cmd.into(),
            cwd: dir.clone(),
        });
        run.guard = Some(crate::sandbox::Guard::In(Box::new(policy)));
        run.by_claude = true;
        ed.session.task_run = Some(run);
        feed(&mut ed, " ms");
        settle_for(&mut ed, |ed| ed.picker.is_some());
        feed(&mut ed, "<ret>");
        assert_eq!(ed.offers.first().map(|o| o.title.as_str()), Some(crate::blocked::GRANT_TITLE));
        // (drawn, and in sight long enough for an answer)
        crate::term::render(&mut ed, &mut Vec::new(), 160, 40).unwrap();
        let o = ed.offers.first().map(|o| (o.id, o.rev)).unwrap();
        ed.card_focus = Some((o.0, o.1));
        let before = crate::tools::USER_ADMITTED.with(|a| a.borrow().len());
        feed(&mut ed, "A");
        let ride = crate::tools::USER_ADMITTED.with(|a| a.borrow().get(before).cloned());
        let (ran, extra) = ride.expect("run again");
        assert_eq!(ran, cmd);
        // (the run's policy as `tools::guard_then_go` makes it, the store's write not landed)
        let w = crate::tools::with_allowed(&ed.config.llm, &ed.caps_basis().granted(), extra.grant.as_ref());
        assert!(
            w.llm.sandbox_write.iter().any(|p| Path::new(p).ends_with("cache")),
            "{:?}",
            w.llm.sandbox_write
        );
    }

    /// Asking about a line starts a note with you; Claude's plain answer (no tool) lands in the thread.
    #[test]
    fn a_question_on_a_line_becomes_a_note() {
        use serde_json::json;
        let (dir, main, ..) = follow_project("ask");
        let mut ed = editor("");
        ed.open(&main).unwrap();
        fake_claude_stream(
            &mut ed,
            &[
                json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "It prints line 2."}}}),
                json!({"type": "result", "result": "done"}),
            ],
        );
        feed(&mut ed, "j<space>nwhat is this?<ret>");
        let n = &ed.session.notes.list[0];
        assert_eq!((n.lines.clone(), n.waiting), (1..2, true));
        assert!(
            ed.session.chat.is_some() && !ed.session.is_at(At::Chat),
            "the chat opens without taking the keys"
        );
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let n = &ed.session.notes.list[0];
        assert_eq!(n.thread.len(), 2);
        assert_eq!(n.thread[1].text, "It prints line 2.");
        assert!(!n.waiting);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An older claude that rejects `--thinking-display` is respawned without it — the chat still works.
    #[test]
    fn chat_survives_a_claude_without_thinking_display() {
        let mut ed = editor("fn main() {}\n");
        let result = serde_json::json!({"type": "result", "result": "fine"});
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "case \"$*\" in *thinking-display*) echo \"error: unknown option '--thinking-display'\" >&2; exit 1;; esac; \
                 while read -r line; do case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac; printf '%s\\n' '{result}'; done"
            ),
            "sh".into(),
        ];
        feed(&mut ed, "<space>l");
        // (respawned: a process after the first — generations are unique across chats)
        let first = ed.session.chat.as_ref().unwrap().generation();
        settle_until(&mut ed, |ed| {
            ed.session.chat.as_ref().unwrap().running()
                && ed.session.chat.as_ref().unwrap().generation() > first
        });
        feed(&mut ed, "hi<ret>");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let c = ed.session.chat.as_ref().unwrap();
        assert!(
            c.msgs.iter().any(|m| m.role == crate::chat::Role::Assistant && m.text == "fine"),
            "{:?}",
            c.msgs.iter().map(|m| &m.text).collect::<Vec<_>>()
        );
    }

    /// Several notes on one line: the card shows the first, `]n` walks them one by one, then moves on;
    /// a note just pinned is the one shown.
    #[test]
    fn a_crowded_line_is_read_in_order() {
        let (dir, _, _, lib) = follow_project("crowded");
        let mut ed = editor("");
        ed.open(&lib).unwrap();
        for (line, text) in [(3, "one"), (3, "two"), (3, "three"), (6, "later")] {
            crate::notes::tool_call(&mut ed, &serde_json::json!({"path": lib, "line": line, "text": text}))
                .unwrap();
        }
        let shown = |ed: &Editor| crate::notes::here(ed).map(|n| n.gist().to_string());
        ed.goto_line(2);
        assert_eq!(shown(&ed).as_deref(), Some("one"), "no note stepped to here → the first");
        ed.session.notes.focus = Some(3);
        assert_eq!(shown(&ed).as_deref(), Some("three"), "just pinned → that one");
        ed.session.notes.focus = None;
        feed(&mut ed, "]n");
        assert_eq!((ed.cursor_line(), shown(&ed).as_deref()), (2, Some("two")));
        feed(&mut ed, "]n");
        assert_eq!(shown(&ed).as_deref(), Some("three"));
        feed(&mut ed, "]n");
        assert_eq!((ed.cursor_line(), shown(&ed).as_deref()), (5, Some("later")));
        feed(&mut ed, "[n[n");
        assert_eq!(shown(&ed).as_deref(), Some("two"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Big files load in the background: a jump (picker, `C-g`, follow) into one lands once the text is there,
    /// instead of on the empty placeholder — and follow doesn't mistake that for the user moving.
    #[test]
    fn jumps_into_a_loading_file_land_when_it_loads() {
        let dir = std::env::temp_dir().join(format!("tarae-bigjump-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let big = |name: &str| {
            let p = dir.join(name);
            std::fs::write(
                &p,
                (0..60_000).map(|i| format!("line {i} with some padding\n")).collect::<String>(),
            )
            .unwrap();
            std::fs::canonicalize(p).unwrap()
        };
        let (a, b) = (big("a.txt"), big("b.txt"));
        let mut ed = editor("");
        ed.open_at(&a, 30_000, 5).unwrap();
        assert!(ed.doc().loading);
        settle_until(&mut ed, |ed| !ed.doc().loading);
        assert_eq!(ed.cursor_line(), 30_000);
        // Follow reading the other big file
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        crate::chat::open(&mut ed);
        let look = crate::chat::Look {
            verb: "read",
            what: "b.txt".into(),
            pattern: false,
            range: String::new(),
            link: Some(crate::chat::Link { path: b.clone(), line: 39_999 }),
        };
        let input = serde_json::json!({"file_path": b, "offset": 40_000, "limit": 20});
        crate::follow::on_tool(&mut ed, "r1", "Read", &input, &look);
        settle_until(&mut ed, |ed| ed.doc().path.as_deref() == Some(b.as_path()) && !ed.doc().loading);
        assert_eq!(crate::follow::view_line(&ed), Some(39_999), "the view lands once it's loaded");
        assert_eq!(ed.cursor_line(), 0, "your cursor never led");
        assert!(ed.session.chat.as_ref().unwrap().follow.active(), "the load isn't the user taking over");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A fake claude that asks to use `tool` with `input` on each question, then ends the turn once tarae
    /// answers the request (or, with `early`, right away). Its stdin goes to the returned log.
    fn fake_edit(ed: &mut Editor, dir: &Path, tool: &str, input: serde_json::Value, early: bool) -> PathBuf {
        let log = dir.join("stdin.log");
        let ask = serde_json::json!({"type": "control_request", "request_id": "p1",
            "request": {"subtype": "can_use_tool", "tool_name": tool, "input": input}});
        let result = serde_json::json!({"type": "result", "result": ""});
        let on_question = if early {
            format!("printf '%s\\n' '{ask}' '{result}'")
        } else {
            format!("printf '%s\\n' '{ask}'")
        };
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do printf '%s\\n' \"$line\" >> '{}'; case \"$line\" in \
                 '{{\"message\"'*) {on_question};; '{{\"response\"'*) printf '%s\\n' '{result}';; esac; done",
                log.display()
            ),
        ];
        log
    }

    /// The answer tarae gave claude's permission request (from the fake's log).
    fn permission_answer(log: &Path) -> serde_json::Value {
        permission_answer_to(log, "p1")
    }

    /// The answer tarae gave claude's permission request `id`.
    fn permission_answer_to(log: &Path, id: &str) -> serde_json::Value {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let sent = std::fs::read_to_string(log).unwrap_or_default();
            if let Some(l) = sent.lines().find(|l| l.contains(&format!("\"request_id\":\"{id}\""))) {
                return serde_json::from_str::<serde_json::Value>(l).unwrap()["response"]["response"].clone();
            }
            assert!(std::time::Instant::now() < end, "no answer: {sent}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// A fake claude that asks these tool permissions one after another (each once the one before is
    /// answered — `p1`, `p2`, …), then ends the turn; every line it's sent goes to the returned log.
    fn fake_tool_asks(ed: &mut Editor, dir: &Path, asks: &[(&str, serde_json::Value)]) -> PathBuf {
        let log = dir.join("stdin.log");
        let ask = |i: usize| {
            let (tool, input) = &asks[i];
            serde_json::json!({"type": "control_request", "request_id": format!("p{}", i + 1),
                "request": {"subtype": "can_use_tool", "tool_name": tool, "input": input}})
        };
        let result = serde_json::json!({"type": "result", "result": "done"});
        let mut cases = format!("'{{\"message\"'*) printf '%s\\n' '{}';; ", ask(0));
        for i in 1..=asks.len() {
            let next = if i < asks.len() { ask(i) } else { result.clone() };
            cases.push_str(&format!("*'\"p{i}\"'*) printf '%s\\n' '{next}';; "));
        }
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do printf '%s\\n' \"$line\" >> '{}'; case \"$line\" in {cases}esac; done",
                log.display()
            ),
        ];
        log
    }

    /// frictions29 34: `n` on a read's card holds for that place until the turn ends — Claude's Read of a file in the
    /// folder it said no to is told so at once, in those words, with no card; the turn's end lets it ask again.
    #[test]
    fn a_reads_no_holds_until_the_turn_ends() {
        use crate::reads::TITLE;
        use serde_json::json;
        let scratch = crate::scratch::Scratch::new("reads-no");
        let dir = std::fs::canonicalize(scratch.path()).unwrap();
        let out = dir.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let marker = out.join("marker.txt");
        std::fs::write(&marker, "tarae-marker\n").unwrap();
        let mut ed = editor("");
        ed.session.root = this_repo();
        let asks = [
            ("Grep", json!({"pattern": "tarae-marker", "path": out})),
            ("Read", json!({"file_path": marker})),
        ];
        let log = fake_tool_asks(&mut ed, &dir, &asks);
        let answered = |ed: &mut Editor, id: &str| {
            let quoted = format!("\"request_id\":\"{id}\"");
            settle_until(ed, |_| std::fs::read_to_string(&log).unwrap_or_default().contains(&quoted));
            permission_answer_to(&log, id)
        };
        feed(&mut ed, "<space>lread them<ret>");
        settle_until(&mut ed, |ed| ed.offers.iter().any(|o| o.title == TITLE));
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 140, 40).unwrap();
        let o = ed.offers.first().map(|o| (o.id, o.rev)).unwrap();
        ed.card_focus = Some((o.0, o.1));
        answer(&mut ed, "n");
        let a = answered(&mut ed, "p1");
        assert_eq!(a["behavior"], "deny");
        assert!(a["message"].as_str().unwrap().ends_with("(this no holds for the rest of the turn)"), "{a}");
        // Asked again for a file in it: no at once, said why, no card
        let a = answered(&mut ed, "p2");
        assert_eq!(a["behavior"], "deny");
        let said = format!("the user said no to {} this turn", out.display());
        assert!(a["message"].as_str().unwrap().starts_with(&said), "{a}");
        assert!(!ed.offers.iter().any(|o| o.title == TITLE), "no card");
        // The turn's end: the no is gone with it
        settle_until(&mut ed, |ed| ed.session.chat.as_ref().is_some_and(|c| c.read_no.is_empty()));
    }

    /// Claude's Read, Grep and Glob (measured: allowed by name they read anywhere unasked — so they aren't):
    /// inside the project they're allowed at once; outside, a card says exactly what's read — `n` no, `a`
    /// this chat (what it covers then goes unasked), `A` always (kept for the project, `:granted`
    /// lists it, `:sandbox-disallow` takes it back), and a place that's everything (`/`) only once. Nothing
    /// is read here: the fake claude only asks, and its paths are this test's own temp folder.
    #[test]
    fn claudes_reads_outside_the_project_ask_first() {
        use crate::reads::TITLE;
        use serde_json::json;
        let scratch = crate::scratch::Scratch::new("reads-ask");
        let dir = scratch.to_path_buf();
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::create_dir_all(dir.join("other")).unwrap();
        std::fs::create_dir_all(dir.join("sess")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let (out, other) = (dir.join("out"), dir.join("other"));
        let marker = out.join("marker.txt");
        std::fs::write(&marker, "tarae-marker\n").unwrap();
        let sess = dir.join("sess/notes.txt");
        std::fs::write(&sess, "tarae-marker\n").unwrap();
        let store = dir.join("state/sandbox.json");
        let project = this_repo();
        let mut ed = editor("");
        ed.session.root = project.clone();
        ed.allow_path = Some(store.clone());
        let asks = [
            ("Read", json!({"file_path": "Cargo.toml"})),
            ("Read", json!({"file_path": marker})),
            ("Grep", json!({"pattern": "tarae-marker", "path": out})),
            ("Read", json!({"file_path": marker})),
            ("Glob", json!({"pattern": "*.txt", "path": other})),
            ("Glob", json!({"pattern": "*", "path": "/"})),
            ("Read", json!({"file_path": sess})),
        ];
        let log = fake_tool_asks(&mut ed, &dir, &asks);
        // (answers decided on a worker: the editor's events go on while it waits)
        let answered = |ed: &mut Editor, id: &str| {
            let quoted = format!("\"request_id\":\"{id}\"");
            settle_until(ed, |_| std::fs::read_to_string(&log).unwrap_or_default().contains(&quoted));
            permission_answer_to(&log, id)
        };
        let screen = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 40).unwrap();
            let mut parser = vt100::Parser::new(40, 140, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        // The card up, drawn, and in sight long enough for an answer — what it says (a long path wraps on
        // screen) and the screen
        let card = |ed: &mut Editor| {
            settle_until(ed, |ed| ed.offers.iter().any(|o| o.title == TITLE));
            let s = screen(ed);
            let o = ed.offers.first().map(|o| (o.id, o.rev)).unwrap();
            ed.card_focus = Some((o.0, o.1));
            (ed.offers[0].question.clone(), s)
        };
        feed(&mut ed, "<space>lread them<ret>");
        assert_eq!(answered(&mut ed, "p1")["behavior"], "allow", "inside: at once");
        let (q, s) = card(&mut ed);
        assert!(
            q.starts_with(&marker.display().to_string()) && q.contains("What it reads goes to Claude"),
            "{q}"
        );
        assert!(s.contains("marker.txt"), "{s}");
        assert!(s.contains("y Once") && s.contains("a This chat") && s.contains("A Always"), "{s}");
        assert!(s.contains("s This session"), "{s}");
        answer(&mut ed, "n");
        let a = answered(&mut ed, "p2");
        assert_eq!(a["behavior"], "deny");
        assert!(a["message"].as_str().unwrap().contains(&marker.display().to_string()), "{a}");
        let (q, _) = card(&mut ed);
        assert!(
            q.contains(&format!("searches {} and everything in it\nfor tarae-marker", out.display())),
            "{q}"
        );
        answer(&mut ed, "a");
        assert_eq!(answered(&mut ed, "p3")["behavior"], "allow");
        assert_eq!(answered(&mut ed, "p4")["behavior"], "allow", "in what `a` allowed: unasked");
        let (q, _) = card(&mut ed);
        assert!(q.contains(&format!("lists *.txt in {}", other.display())), "{q}");
        answer(&mut ed, "A");
        assert_eq!(answered(&mut ed, "p5")["behavior"], "allow");
        let (q, s) = card(&mut ed);
        assert!(q.contains("Only once") && s.contains("y Once") && !s.contains("A Always"), "not kept: {s}");
        answer(&mut ed, "A");
        assert!(ed.offers.iter().any(|o| o.title == TITLE), "A isn't an answer here");
        answer(&mut ed, "y");
        assert_eq!(answered(&mut ed, "p6")["behavior"], "allow");
        // `s`: this session's — not the conversation's, not the project's
        card(&mut ed);
        answer(&mut ed, "s");
        assert_eq!(answered(&mut ed, "p7")["behavior"], "allow");
        assert!(ed.session.allowed.reads(&sess), "{:?}", ed.session.allowed);
        assert!(!ed.session.chat.as_ref().unwrap().allowed.reads(&sess));
        // Kept for this project; this chat's only in the chat
        let kept = |p: &Path| crate::allow::Store::read(Some(&store)).get(&project).reads(p);
        settle_until(&mut ed, |_| kept(&other));
        assert!(!kept(&out) && !kept(Path::new("/")) && !kept(&sess));
        assert!(ed.session.chat.as_ref().unwrap().allowed.reads(&marker));
        // Taken back
        crate::typed::execute(&mut ed, &format!("sandbox-disallow {}", other.display())).unwrap();
        settle_until(&mut ed, |_| !kept(&other));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 7c: Claude's Read of a file in the project named like a key (`runtime/secret-names.txt`) asks on the
    /// read card, as a run can't read it either (7a/7b); an exception (`!.env.example`) is read unasked. A key is
    /// read once at a time (cardauto 6, `Caps::storable`): the card offers `y` and `n` only, `a` and `A` are
    /// refused on it, and the run's panel opens one for its run only — the same file asks again. A Grep over a
    /// folder asks once, naming the keys it would search (not one below tools' output) and what else there looks
    /// like a token; a glob that leaves them out, and a Glob (names only), don't ask. A key an earlier build kept
    /// for the project (`A`) still opens it, in a later conversation too. Nothing is read here: the fake claude
    /// only asks, and its paths are this test's own temp folder.
    #[test]
    fn reading_a_key_named_file_in_the_project_asks() {
        use crate::reads::KEYS_TITLE;
        use serde_json::json;
        let scratch = crate::scratch::Scratch::new("key-reads");
        let dir = scratch.to_path_buf();
        for d in ["src/tls", "deploy", "node_modules/pkg"] {
            std::fs::create_dir_all(dir.join("project").join(d)).unwrap();
        }
        let dir = std::fs::canonicalize(&dir).unwrap();
        let project = dir.join("project");
        let (env, tls, pem) =
            (project.join(".env"), project.join("src/tls/server.key"), project.join("deploy/prod.pem"));
        for f in [".env.example", "src/main.rs", "node_modules/pkg/test.key"].map(|f| project.join(f)) {
            std::fs::write(f, "x\n").unwrap();
        }
        for f in [&env, &tls, &pem] {
            std::fs::write(f, "x\n").unwrap();
        }
        std::fs::write(project.join("deploy/hosts.yml"), "oauth_token: tarae-fake-token\n").unwrap();
        let store = dir.join("state/sandbox.json");
        let mut ed = editor("");
        ed.session.root = project.clone();
        ed.allow_path = Some(store.clone());
        let asks = [
            ("Read", json!({"file_path": ".env.example"})),
            ("Read", json!({"file_path": tls})),
            ("Read", json!({"file_path": ".env"})),
            ("Read", json!({"file_path": env})),
            ("Grep", json!({"pattern": "TOKEN"})),
            ("Grep", json!({"pattern": "TOKEN", "glob": "*.rs"})),
            ("Glob", json!({"pattern": "**/*.pem"})),
            ("Read", json!({"file_path": "deploy/prod.pem"})),
            ("Grep", json!({"pattern": "TOKEN"})),
        ];
        let log = fake_tool_asks(&mut ed, &dir, &asks);
        let answered = |ed: &mut Editor, id: &str| {
            let quoted = format!("\"request_id\":\"{id}\"");
            settle_until(ed, |_| std::fs::read_to_string(&log).unwrap_or_default().contains(&quoted));
            permission_answer_to(&log, id)
        };
        // The card up, drawn, and in sight long enough for an answer — what it says and the screen
        let card = |ed: &mut Editor| {
            settle_until(ed, |ed| ed.offers.iter().any(|o| o.title == KEYS_TITLE));
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 40).unwrap();
            let mut parser = vt100::Parser::new(40, 140, 0);
            parser.process(&bytes);
            let o = ed.offers.first().map(|o| (o.id, o.rev)).unwrap();
            ed.card_focus = Some((o.0, o.1));
            (ed.offers[0].question.clone(), parser.screen().contents())
        };
        feed(&mut ed, "<space>l");
        // (the run's panel opens one for its run only — `blocked::grant_answer`'s `a` is never on its card)
        crate::allow::grant(&mut ed, &crate::allow::Grant::Key(tls.clone()), crate::allow::Scope::ThisChat);
        assert!(ed.session.chat.as_ref().unwrap().allowed.keys.is_empty());
        feed(&mut ed, "read them<ret>");
        assert_eq!(answered(&mut ed, "p1")["behavior"], "allow", "an exception: unasked");
        let (q, s) = card(&mut ed);
        assert!(q.starts_with(&tls.display().to_string()) && q.contains("It's named like a key."), "{q}");
        assert!(!q.contains("a and A"), "{q}");
        assert!(s.contains("y Once") && !s.contains("a This chat") && !s.contains("A Always"), "{s}");
        answer(&mut ed, "a");
        answer(&mut ed, "A");
        assert!(ed.offers.iter().any(|o| o.title == KEYS_TITLE), "a and A aren't answers here");
        answer(&mut ed, "y");
        assert_eq!(answered(&mut ed, "p2")["behavior"], "allow");
        card(&mut ed);
        answer(&mut ed, "y");
        assert_eq!(answered(&mut ed, "p3")["behavior"], "allow");
        assert!(ed.session.chat.as_ref().unwrap().allowed.keys.is_empty(), "once: nothing opened");
        let (q, _) = card(&mut ed);
        assert!(q.starts_with(&env.display().to_string()), "the same file asks again: {q}");
        answer(&mut ed, "n");
        assert_eq!(answered(&mut ed, "p4")["behavior"], "deny");
        let (q, _) = card(&mut ed);
        let named = format!("searches {} and everything in it\nfor TOKEN", project.display());
        assert!(q.starts_with(&named) && q.contains("It opens files named like keys:\n"), "{q}");
        assert!(q.contains("    deploy/prod.pem") && !q.contains("test.key"), "{q}");
        assert!(q.contains("deploy/hosts.yml holds a token-like line"), "its yes reads more: {q}");
        answer(&mut ed, "n");
        let a = answered(&mut ed, "p5");
        assert_eq!(a["behavior"], "deny");
        assert!(a["message"].as_str().unwrap().contains(&pem.display().to_string()), "{a}");
        assert_eq!(answered(&mut ed, "p6")["behavior"], "allow", "a glob that leaves the keys out");
        assert_eq!(answered(&mut ed, "p7")["behavior"], "allow", "Glob lists names only");
        let (q, _) = card(&mut ed);
        assert!(q.starts_with(&pem.display().to_string()), "{q}");
        answer(&mut ed, "y");
        assert_eq!(answered(&mut ed, "p8")["behavior"], "allow");
        // (the search said no to this turn: told so at once, no card — frictions29 34)
        let a = answered(&mut ed, "p9");
        assert_eq!(a["behavior"], "deny");
        let said = format!("the user said no to {} this turn", project.display());
        assert!(a["message"].as_str().unwrap().starts_with(&said), "{a}");
        assert!(!ed.offers.iter().any(|o| o.title == KEYS_TITLE));
        // Kept for the project by an earlier build's `A`: a later conversation reads it unasked
        crate::allow::keep(&store, &project, |a| a.add(&crate::allow::Grant::Key(pem.clone()))).unwrap();
        let basis =
            crate::caps::Basis { root: project.clone(), store: Some(store.clone()), ..Default::default() };
        let later = |f: &Path| {
            let input = json!({"file_path": f});
            crate::reads::decide("Read", &input, &basis.build(), &Default::default()).1
        };
        assert!(later(&pem) && !later(&env));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #81: what one of Claude's runs made this turn (a shot under the temp folder) is read unasked; what
    /// was there before it still asks; the turn over, its runs count no more. Nothing is read here: the fake
    /// claude only asks.
    #[test]
    fn what_claudes_run_made_this_turn_reads_unasked() {
        use serde_json::json;
        let scratch = crate::scratch::Scratch::new("made-ask");
        let dir = scratch.to_path_buf();
        let (old, shot) = (dir.join("old.png"), dir.join("shot.png"));
        std::fs::write(&old, "x").unwrap();
        let mut ed = editor("");
        ed.session.root = this_repo();
        let log = fake_tool_asks(
            &mut ed,
            &dir,
            &[("Read", json!({"file_path": shot})), ("Read", json!({"file_path": old}))],
        );
        feed(&mut ed, "<space>l");
        // (apart in time: birth times are what's compared)
        std::thread::sleep(std::time::Duration::from_millis(20));
        crate::reads::run_started(&mut ed, None);
        std::fs::write(&shot, "x").unwrap();
        crate::reads::run_ended(&mut ed);
        assert_eq!(ed.session.chat.as_ref().map(|c| c.ran.len()), Some(1));
        feed(&mut ed, "look at the shot<ret>");
        let answered = |ed: &mut Editor, id: &str| {
            let quoted = format!("\"request_id\":\"{id}\"");
            settle_until(ed, |_| std::fs::read_to_string(&log).unwrap_or_default().contains(&quoted));
            permission_answer_to(&log, id)
        };
        assert_eq!(answered(&mut ed, "p1")["behavior"], "allow", "made by its run: unasked");
        settle_until(&mut ed, |ed| ed.offers.iter().any(|o| o.title == crate::reads::TITLE));
        assert!(ed.offers[0].question.starts_with(&old.display().to_string()), "there before: asks");
        // (drawn, and in sight long enough for an answer)
        crate::term::render(&mut ed, &mut Vec::new(), 140, 40).unwrap();
        let o = (ed.offers[0].id, ed.offers[0].rev);
        ed.card_focus = Some((o.0, o.1));
        answer(&mut ed, "n");
        assert_eq!(answered(&mut ed, "p2")["behavior"], "deny");
        settle_until(&mut ed, |ed| ed.session.chat.as_ref().unwrap().state == crate::chat::State::Idle);
        assert!(ed.session.chat.as_ref().unwrap().ran.is_empty(), "the turn's runs are over with it");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Claude's Edit from the chat is a review first; accepting lets claude write it (its own input).
    #[test]
    fn chat_edits_are_reviewed_then_allowed() {
        let (dir, main, ..) = follow_project("edit-accept");
        let mut ed = editor("");
        ed.open(&main).unwrap();
        let input = serde_json::json!({"file_path": main, "old_string": "line 2", "new_string": "LINE 2"});
        let log = fake_edit(&mut ed, &dir, "Edit", input, false);
        feed(&mut ed, "<space>lshout line 2<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        assert!(!ed.session.is_at(crate::session::At::Chat), "the keys go to the review");
        feed(&mut ed, "y");
        let a = permission_answer(&log);
        assert_eq!(a["behavior"], "allow", "{a}");
        assert_eq!(
            (a["updatedInput"]["old_string"].as_str(), a["updatedInput"]["new_string"].as_str()),
            (Some("line 2"), Some("LINE 2"))
        );
        assert_eq!(text(&ed), "line 1\nLINE 2\nline 3\n", "the buffer has it already");
        // Off to another pane at once: auto-save leaves the file to claude (saving first fails its Edit —
        // measured live, tests/live.rs)
        ed.on_focus(false);
        assert!(std::fs::read_to_string(&main).unwrap().contains("line 2"), "claude writes it, not us");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(ed.session.chat.as_ref().unwrap().follow.active(), "a review isn't the user taking over");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A stand-in claude whose turn is two Edits of `lib`, each a tool call (`e1`, `e2`) and its permission asked
    /// (`p1`, `p2`) — the second once the first is answered; the first changes two lines apart (two changes in
    /// its review), the second one.
    fn two_edit_calls(ed: &mut Editor, dir: &Path, lib: &Path) {
        edit_calls(ed, dir, lib, false)
    }

    /// `two_edit_calls`, or (`batch`) both calls in one message, their results only once both are answered — as
    /// claude sends parallel calls.
    fn edit_calls(ed: &mut Editor, dir: &Path, lib: &Path, batch: bool) {
        use serde_json::json;
        let first = json!({"file_path": lib, "old_string": "line 2\nline 3\nline 4", "new_string": "LINE 2\nline 3\nLINE 4"});
        let second = json!({"file_path": lib, "old_string": "line 8", "new_string": "LINE 8"});
        let call = |id: &str, input: &serde_json::Value| json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": "Edit", "input": input}]}});
        let ask = |id: &str, input: &serde_json::Value| {
            json!({"type": "control_request", "request_id": id,
            "request": {"subtype": "can_use_tool", "tool_name": "Edit", "input": input}})
        };
        let done = |id: &str| json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": "The file has been updated."}]}});
        let result = json!({"type": "result", "result": "done"});
        let (c1, a1, c2, a2) =
            (call("e1", &first), ask("p1", &first), call("e2", &second), ask("p2", &second));
        let (d1, d2) = (done("e1"), done("e2"));
        let steps = match batch {
            false => format!(
                "'{{\"message\"'*) printf '%s\\n' '{c1}' '{a1}';; *'\"p1\"'*) printf '%s\\n' '{d1}' '{c2}' '{a2}';; \
                 *'\"p2\"'*) printf '%s\\n' '{d2}' '{result}';;"
            ),
            true => format!(
                "'{{\"message\"'*) printf '%s\\n' '{c1}' '{c2}' '{a1}';; *'\"p1\"'*) printf '%s\\n' '{a2}';; \
                 *'\"p2\"'*) printf '%s\\n' '{d1}' '{d2}' '{result}';;"
            ),
        };
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do printf '%s\\n' \"$line\" >> '{}'; case \"$line\" in {steps} esac; done",
                dir.join("stdin.log").display()
            ),
        ];
    }

    /// Two Edits of one file in one message, both answered before either's result comes: each row takes its own
    /// review's keys — the second's never joins the first's.
    #[test]
    fn two_edits_of_one_file_in_one_message_are_stamped_each_its_own() {
        let scratch = crate::scratch::Scratch::new("knot-batch");
        let dir = std::fs::canonicalize(&scratch).unwrap();
        let lib = dir.join("lib.rs");
        std::fs::write(&lib, (1..=10).map(|i| format!("line {i}\n")).collect::<String>()).unwrap();
        let mut ed = editor("");
        ed.session.root = dir.clone();
        ed.open(&lib).unwrap();
        edit_calls(&mut ed, &dir, &lib, true);
        feed(&mut ed, "<space>ltwo edits please<ret>");
        settle_until(&mut ed, |ed| ed.session.review.as_ref().is_some_and(|r| r.ready));
        feed(&mut ed, "yy");
        settle_until(&mut ed, |ed| ed.session.review.as_ref().is_some_and(|r| r.ready));
        assert_eq!(ed.session.chat.as_ref().unwrap().calls.len(), 2, "both still waiting for their results");
        feed(&mut ed, "n");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let c = ed.session.chat.as_ref().unwrap();
        let stamps = |id: &str| -> String {
            c.msgs.iter().find(|m| m.call == id).unwrap().stamps.iter().map(|s| s.0).collect()
        };
        assert_eq!((stamps("e1"), stamps("e2")), ("y".into(), "n".into()));
    }

    /// 글타래 P6: a change's knot carries the keys that decided it — `y` and `n` on the edit whose two changes
    /// were answered so, `n` on the one refused — and replay's `[r` moves the knot in hand along the thread, the
    /// newest stop first. Unasked (review at the end), or a review closed unanswered: no key, no stamp.
    #[test]
    fn a_changes_knot_is_stamped_with_the_keys_that_decided_it() {
        let scratch = crate::scratch::Scratch::new("knot-stamps");
        let dir = std::fs::canonicalize(&scratch).unwrap();
        let lib = dir.join("lib.rs");
        std::fs::write(&lib, (1..=10).map(|i| format!("line {i}\n")).collect::<String>()).unwrap();
        let mut ed = editor("");
        ed.session.root = dir.clone();
        ed.open(&lib).unwrap();
        two_edit_calls(&mut ed, &dir, &lib);
        feed(&mut ed, "<space>ltwo edits please<ret>");
        settle_until(&mut ed, |ed| ed.session.review.as_ref().is_some_and(|r| r.ready));
        assert_eq!(ed.session.review.as_ref().unwrap().hunks.len(), 2, "two changes in its review");
        // (answered `n`, then `y`: stamped `y` first, whichever came first)
        feed(&mut ed, "ny");
        settle_until(&mut ed, |ed| ed.session.review.as_ref().is_some_and(|r| r.ready));
        feed(&mut ed, "n");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let stamps = |ed: &Editor, id: &str| -> String {
            let c = ed.session.chat.as_ref().unwrap();
            c.msgs.iter().find(|m| m.call == id).unwrap().stamps.iter().map(|s| s.0).collect()
        };
        assert_eq!((stamps(&ed, "e1"), stamps(&ed, "e2")), ("yn".into(), "n".into()));
        let mut out = Vec::new();
        crate::term::render(&mut ed, &mut out, 120, 40).unwrap();
        let mut vt = vt100::Parser::new(40, 120, 0);
        vt.process(&out);
        let screen = vt.screen().contents();
        assert!(screen.contains("▐y▌ ▐n▌"), "both keys on the first edit's knot: {screen}");
        // Replay: the newest stop in hand, then the one before
        let hand = |ed: &Editor| ed.session.chat.as_ref().unwrap().in_hand().map(str::to_string);
        assert_eq!(hand(&ed), None, "at rest, none");
        feed(&mut ed, "<esc><esc>[r");
        assert_eq!(hand(&ed).as_deref(), Some("e2"));
        feed(&mut ed, "[r");
        assert_eq!(hand(&ed).as_deref(), Some("e1"));
        // Review at the end: written unasked — nobody's key
        let scratch = crate::scratch::Scratch::new("knot-unasked");
        let dir = std::fs::canonicalize(&scratch).unwrap();
        let lib = dir.join("lib.rs");
        std::fs::write(&lib, (1..=10).map(|i| format!("line {i}\n")).collect::<String>()).unwrap();
        let mut ed = editor("");
        ed.session.root = dir.clone();
        ed.config.llm.review_at_end = true;
        ed.open(&lib).unwrap();
        two_edit_calls(&mut ed, &dir, &lib);
        feed(&mut ed, "<space>ltwo edits please<ret>");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert_eq!((stamps(&ed, "e1"), stamps(&ed, "e2")), (String::new(), String::new()), "unasked");
        // A review closed unanswered (another in its place, the turn stopped): none
        let scratch = crate::scratch::Scratch::new("knot-closed");
        let dir = std::fs::canonicalize(&scratch).unwrap();
        let lib = dir.join("lib.rs");
        std::fs::write(&lib, (1..=10).map(|i| format!("line {i}\n")).collect::<String>()).unwrap();
        let mut ed = editor("");
        ed.session.root = dir.clone();
        ed.open(&lib).unwrap();
        two_edit_calls(&mut ed, &dir, &lib);
        feed(&mut ed, "<space>ltwo edits please<ret>");
        settle_until(&mut ed, |ed| ed.session.review.as_ref().is_some_and(|r| r.ready));
        feed(&mut ed, "y");
        crate::llm::close_review(&mut ed);
        assert_eq!(stamps(&ed, "e1"), "", "answered in part, then closed: no key gave it");
    }

    /// A turn's edits across files are one step back: `:undo-turn` takes Claude's changes out (the user's own
    /// edits since stay), removes the file it created, and `:redo-turn` brings it all back. Claude hears of it.
    #[test]
    fn a_turn_of_claudes_edits_undoes_as_one() {
        let (dir, _, _, lib) = follow_project("turn-undo");
        let new = dir.join("new.rs");
        let log = dir.join("stdin.log");
        let ask = |id: &str, tool: &str, input: serde_json::Value| {
            serde_json::json!({"type": "control_request", "request_id": id,
                "request": {"subtype": "can_use_tool", "tool_name": tool, "input": input}})
        };
        let edit = ask(
            "p1",
            "Edit",
            serde_json::json!({"file_path": lib, "old_string": "line 2", "new_string": "LINE 2"}),
        );
        let write = ask("p2", "Write", serde_json::json!({"file_path": new, "content": "fn new() {}\n"}));
        let result = serde_json::json!({"type": "result", "result": "done"});
        let mut ed = editor("");
        ed.open(&lib).unwrap();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do printf '%s\\n' \"$line\" >> '{}'; case \"$line\" in \
                 '{{\"message\"'*) printf '%s\\n' '{edit}';; *'\"p1\"'*) printf '%s\\n' '{write}';; \
                 *'\"p2\"'*) printf '%s\\n' '{result}';; esac; done",
                log.display()
            ),
        ];
        feed(&mut ed, "<space>ltwo files please<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        feed(&mut ed, "y");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        feed(&mut ed, "y");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let c = ed.session.chat.as_ref().unwrap();
        assert!(c.msgs.iter().any(|m| m.text == "changed lib.rs, new.rs · space u undo"));
        assert_eq!(ed.session.checkpoints.list.len(), 1);
        // Claude writes what was accepted (here: saved as it would be)
        for d in ed.docs.iter_mut().filter(|d| d.path.is_some()) {
            d.save().unwrap();
        }
        assert!(new.exists());
        // The user edits away from Claude's change
        ed.open(&lib).unwrap();
        feed(&mut ed, "<esc><esc>8GA!<esc>");
        let lib_text = |ed: &Editor| {
            ed.docs.iter().find(|d| d.path.as_deref() == Some(lib.as_path())).unwrap().text.to_string()
        };
        let lines = |edits: &[(usize, &str)]| {
            let mut v: Vec<String> = (1..=10).map(|i| format!("line {i}")).collect();
            for (i, t) in edits {
                v[*i] = t.to_string();
            }
            v.join("\n") + "\n"
        };
        assert_eq!(lib_text(&ed), lines(&[(1, "LINE 2"), (7, "line 8!")]));
        feed(&mut ed, "<space>u");
        assert_eq!(lib_text(&ed), lines(&[(7, "line 8!")]), "Claude's change out, the user's stays");
        assert!(!new.exists(), "the file it made is gone");
        assert!(!ed.docs.iter().any(|d| d.path.as_deref() == Some(new.as_path())));
        feed(&mut ed, ":undo-turn<ret>");
        assert!(ed.status.as_ref().is_some_and(|(m, _)| m.contains("nothing to undo")), "{:?}", ed.status);
        feed(&mut ed, "<space>U");
        assert_eq!(lib_text(&ed), lines(&[(1, "LINE 2"), (7, "line 8!")]));
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "fn new() {}\n", "written again");
        assert_eq!(ed.session.checkpoints.tell.len(), 2, "Claude hears of both with the next message");
        assert!(
            ed.session.checkpoints.tell[0].contains("undid your changes from the turn \"two files please\"")
        );
        feed(&mut ed, "<space>lwhat now?<ret>");
        // (the fake asks the same again: its edit no longer applies, its write changes nothing)
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let sent = std::fs::read_to_string(&log).unwrap();
        let asked = sent.lines().rfind(|l| l.contains("what now?")).unwrap();
        assert!(asked.contains("<editor-note>") && asked.contains("brought back your changes"), "{asked}");
        assert!(ed.session.checkpoints.tell.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A review of Claude's edit says why: its words just before it asked, under the title.
    #[test]
    fn a_chat_edit_review_says_why() {
        let (dir, main, ..) = follow_project("edit-why");
        let mut ed = editor("");
        ed.open(&main).unwrap();
        let log = dir.join("stdin.log");
        let said = serde_json::json!({"type": "stream_event", "event": {"type": "content_block_delta",
            "delta": {"type": "text_delta", "text": "Line 2 is lowercase while the rest shout, so I will make it match."}}});
        let ask = serde_json::json!({"type": "control_request", "request_id": "p1", "request": {"subtype": "can_use_tool",
            "tool_name": "Edit", "input": {"file_path": main, "old_string": "line 2", "new_string": "LINE 2"}}});
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do printf '%s\\n' \"$line\" >> '{}'; case \"$line\" in \
                 '{{\"message\"'*) printf '%s\\n' '{said}' '{ask}';; esac; done",
                log.display()
            ),
        ];
        feed(&mut ed, "<space>lfix it<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        let r = ed.session.review.as_ref().unwrap();
        assert_eq!(r.reason, "Line 2 is lowercase while the rest shout, so I will make it match.");
        // Under the title
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 100, 24).unwrap();
        let mut parser = vt100::Parser::new(24, 100, 0);
        parser.process(&bytes);
        let rows: Vec<String> = parser.screen().rows(0, 100).collect();
        let title = rows.iter().position(|r| r.contains("“fix it”")).expect("the title");
        assert!(rows[title + 1].contains("so I will make it match."), "{rows:#?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The follow card keeps off an edit review's changes: with one near the top and one near the bottom it
    /// goes into the gap between — the bottom right covered the second change's new line.
    #[test]
    fn the_follow_card_keeps_off_the_changes_under_review() {
        let (dir, ..) = follow_project("edit-gap");
        let file = dir.join("long.txt");
        let lines: Vec<String> =
            (1..=30).map(|i| format!("line {i} of the long file under review")).collect();
        std::fs::write(&file, lines.join("\n") + "\n").unwrap();
        let mut ed = editor("");
        ed.open(&file).unwrap();
        let mut after = lines.clone();
        after[1] = "LINE 2 of the long file under review, now shouting".into();
        after[27] = "LINE 28 of the long file under review, now shouting".into();
        let ask = serde_json::json!({"type": "control_request", "request_id": "p1", "request": {"subtype": "can_use_tool",
            "tool_name": "Write", "input": {"file_path": file, "content": after.join("\n") + "\n"}}});
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do case \"$line\" in '{{\"message\"'*) printf '%s\\n' '{ask}';; esac; done"
            ),
        ];
        feed(&mut ed, "<space>lshout<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        assert_eq!(ed.session.review.as_ref().unwrap().hunks.len(), 2);
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 120, 40).unwrap();
        let (_, y, _, h) = ed.screen.thought_card.get().expect("the follow card");
        let rows: Vec<usize> = ed.screen.review_rows.borrow().iter().map(|r| r.0).collect();
        assert!(rows.iter().any(|&r| r > 30), "the second change is near the bottom: {rows:?}");
        assert!(!rows.iter().any(|r| (y..y + h).contains(r)), "card {y}..{} over {rows:?}", y + h);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A conversation outlives tarae: kept after each turn, and the chat opened after a restart shows it and
    /// resumes it in claude (`--resume`); one claude no longer has starts fresh, the rows still shown.
    #[test]
    fn a_conversation_continues_after_a_restart() {
        let dir = std::env::temp_dir().join(format!("tarae-chat-resume-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let argv = dir.join("argv.log");
        let init = serde_json::json!({"type": "system", "subtype": "init", "session_id": "s-1"});
        let text = serde_json::json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "Hello back."}}});
        let result = serde_json::json!({"type": "result", "result": "done"});
        // The fake says why it can't resume when asked to (as claude does), and logs how it was started
        let fake = |ed: &mut Editor, gone: bool| {
            ed.chats_dir = Some(dir.join("chats"));
            ed.config.llm.command = "sh".into();
            let refuse = if gone {
                "case \"$*\" in *--resume*) echo 'No conversation found with session ID: s-1' >&2; exit 1;; esac;"
            } else {
                ""
            };
            ed.config.llm.args = vec![
                "-c".into(),
                format!(
                    "printf '%s\\n' \"$*\" >> '{}'; {refuse} while read -r line; do case \"$line\" in '{{\"message\"'*) \
                     printf '%s\\n' '{init}' '{text}' '{result}';; esac; done",
                    argv.display()
                ),
            ];
        };
        let mut ed = editor("");
        fake(&mut ed, false);
        feed(&mut ed, "<space>lhello<ret>");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert_eq!(ed.session.chat.as_ref().unwrap().conversation.as_deref(), Some("s-1"));
        let kept = |dir: &Path| {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let all = crate::chats::load(&dir.join("chats"), &test_root());
                // (kept twice — the question when claude names the conversation, then the answer: wait for the answer)
                if all.first().is_some_and(|c| c.rows.len() >= 3) || std::time::Instant::now() > end {
                    return all;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        let all = kept(&dir);
        assert_eq!((all[0].id.as_str(), all[0].title.as_str()), ("s-1", "hello"));
        drop(ed);
        // "Restart": a new editor, the chat opened again
        let mut ed = editor("");
        fake(&mut ed, false);
        feed(&mut ed, "<space>l");
        settle_until(&mut ed, |ed| {
            ed.session.chat.as_ref().unwrap().msgs.iter().any(|m| m.text == "Hello back.")
        });
        let c = ed.session.chat.as_ref().unwrap();
        assert!(
            c.msgs.iter().any(|m| m.role == crate::chat::Role::User && m.text == "hello"),
            "the rows are back"
        );
        // (kept and reopened in whole seconds, `chats::now`: a second's turn between them says `1s ago`)
        let note = &c.msgs.last().unwrap().text;
        assert!(
            note.strip_prefix("continued from ").is_some_and(|w| w == "just now" || w.ends_with("s ago")),
            "{note}"
        );
        assert_eq!(c.msgs.iter().filter(|m| m.text.starts_with("continued from")).count(), 1);
        let started = || std::fs::read_to_string(&argv).unwrap_or_default();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !started().lines().last().unwrap_or_default().contains("--resume s-1") {
            assert!(std::time::Instant::now() < end, "resumed: {}", started());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!started().contains("--no-session-persistence"), "claude keeps the chat's conversations");
        drop(ed);
        // claude no longer has it (Linux's first start with tarae's own claude folder): a new conversation, the rows
        // still there, said once and not as an error — and a message sent to the process that couldn't resume, before
        // it said so, answered in the new one, not lost with it
        let mut ed = editor("");
        fake(&mut ed, true);
        feed(&mut ed, "<space>l");
        settle_until(&mut ed, |ed| {
            ed.session.chat.as_ref().unwrap().msgs.iter().any(|m| m.text == "Hello back.")
        });
        feed(&mut ed, "again<ret>");
        let answered = |ed: &Editor| {
            let c = ed.session.chat.as_ref().unwrap();
            c.msgs.iter().filter(|m| m.text == "Hello back.").count() == 2 && !c.busy()
        };
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !answered(&ed) {
            let c = ed.session.chat.as_ref().unwrap();
            let rows: Vec<_> = c.msgs.iter().map(|m| &m.text).collect();
            assert!(std::time::Instant::now() < end, "{rows:?} queued {:?}", c.queued);
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        let c = ed.session.chat.as_ref().unwrap();
        let said = |t: &str| c.msgs.iter().filter(|m| m.text.contains(t)).count();
        assert_eq!(said("no longer has this conversation"), 1);
        assert_eq!((said("claude exited"), said("No conversation found")), (0, 0), "not an error");
        assert_eq!(
            c.msgs.iter().filter(|m| m.role == crate::chat::Role::User && m.text == "again").count(),
            1
        );
        assert!(!ed.status.as_ref().is_some_and(|s| s.1 == Severity::Error), "{:?}", ed.status);
        assert!(!started().lines().last().unwrap_or_default().contains("--resume"), "fresh: {}", started());
        drop(ed);
        // #51: `:chat-new` at a fresh start — a new conversation, said, not the recent one continued once
        // the project's conversations are read
        let mut ed = editor("");
        fake(&mut ed, false);
        std::fs::remove_file(&argv).ok();
        feed(&mut ed, ":chat-new<ret>");
        assert!(ed.session.chat_has_keys(), "the chat, open, with the keys");
        let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert!(said.contains("New conversation"), "{said}");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().past.is_empty());
        settle_until(&mut ed, |ed| ed.session.chat.as_ref().unwrap().running());
        let c = ed.session.chat.as_ref().unwrap();
        assert!(c.msgs.is_empty() && c.conversation.is_none(), "nothing continued: {:?}", c.msgs.len());
        assert!(!started().contains("--resume"), "claude started fresh: {}", started());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Claude's plan (the `plan` tool) is pinned under the chat's title — the step in hand in front — and is
    /// answered at once.
    #[test]
    fn claudes_plan_is_pinned_in_the_chat() {
        use serde_json::json;
        let (dir, main, ..) = follow_project("plan");
        let log = dir.join("stdin.log");
        let args = json!({"title": "Add a flag", "steps": [
            {"text": "Parse the flag", "status": "done"},
            {"text": "Wire it into the config", "status": "doing"},
            {"text": "Write tests", "status": "todo"}]});
        let tool = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t1", "name": "mcp__tarae__plan", "input": args}]}});
        let call = json!({"type": "control_request", "request_id": "c7", "request": {"subtype": "mcp_message",
            "server_name": "tarae", "message": {"jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": "plan", "arguments": args}}}});
        // Then an edit with no words before it: its review says the plan's step in hand
        let edit_row = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t2", "name": "Edit", "input": {"file_path": main, "old_string": "line 2", "new_string": "LINE 2"}}]}});
        let edit = json!({"type": "control_request", "request_id": "p1", "request": {"subtype": "can_use_tool",
            "tool_name": "Edit", "input": {"file_path": main, "old_string": "line 2", "new_string": "LINE 2"}}});
        let result = json!({"type": "result", "result": "done"});
        // (its turn's end a moment after its edit: claude itself waits for the edit's answer — the review comes
        // once where the edit leads is worked out, `edits::judge_later`)
        let mut body: Vec<String> =
            [tool, call, edit_row, edit].iter().map(|e| format!("printf '%s\\n' '{e}'")).collect();
        body.push(format!("sleep 0.5; printf '%s\\n' '{result}'"));
        let mut ed = editor("");
        ed.open(&main).unwrap();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do printf '%s\\n' \"$line\" >> '{}'; case \"$line\" in '{{\"message\"'*) {};; esac; done",
                log.display(),
                body.join("; ")
            ),
        ];
        feed(&mut ed, "<space>ladd a flag<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        assert_eq!(ed.session.review.as_ref().unwrap().reason, "step 2/3: Wire it into the config");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        let p = ed.session.chat.as_ref().unwrap().plan.clone().expect("the plan");
        assert_eq!((p.title.as_str(), p.done(), p.current()), ("Add a flag", 1, Some(1)));
        let c = ed.session.chat.as_ref().unwrap();
        let row = c.msgs.iter().find_map(|m| m.look.as_ref().filter(|l| l.verb == "plan")).expect("a row");
        assert_eq!((row.what.as_str(), row.range.as_str()), ("Add a flag", "1/3"));
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 110, 30).unwrap();
        let mut parser = vt100::Parser::new(30, 110, 0);
        parser.process(&bytes);
        let rows: Vec<String> = parser.screen().rows(0, 110).collect();
        let at = |needle: &str| {
            rows.iter().position(|r| r.contains(needle)).unwrap_or_else(|| panic!("{needle}: {rows:#?}"))
        };
        assert!(at("▎ Add a flag") < at("● Parse the flag"), "{rows:#?}");
        assert!(
            at("● Parse the flag") < at("› Wire it into the config") && at("› Wire it") < at("◦ Write tests")
        );
        assert!(rows[at("▎ Add a flag")].contains("1/3"));
        // (the fake logs our answer when it reads it)
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !std::fs::read_to_string(&log).unwrap_or_default().contains("plan shown (1/3 done)") {
            assert!(std::time::Instant::now() < end, "answered");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The model and effort are chosen from what claude offers (its `initialize` answer), saved, applied to
    /// the conversation at once (`set_model` · `apply_flag_settings`), and named in the chat's title.
    #[test]
    fn claudes_model_is_chosen_live() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-model-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("stdin.log");
        let models = json!([
            {"value": "default", "displayName": "Default (recommended)", "resolvedModel": "claude-opus-5-5",
             "description": "Opus 5.5 · Best for everyday", "supportedEffortLevels": ["low", "high", "max"]},
            {"value": "opus", "displayName": "Opus 5.5", "resolvedModel": "claude-opus-5-5",
             "description": "For complex work", "supportedEffortLevels": ["low", "high", "max"]},
            {"value": "haiku", "displayName": "Haiku 4.5", "resolvedModel": "claude-haiku-4-5",
             "description": "Fastest for quick answers"}]);
        let init = json!({"type": "control_response", "response": {"subtype": "success", "request_id": "tarae-init",
            "response": {"models": models}}});
        let mut ed = editor("");
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do printf '%s\\n' \"$line\" >> '{}'; case \"$line\" in *tarae-init*) \
                 printf '%s\\n' '{init}';; esac; done",
                log.display()
            ),
        ];
        feed(&mut ed, "<space>l");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().models.is_empty());
        let title = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 110, 24).unwrap();
            let mut parser = vt100::Parser::new(24, 110, 0);
            parser.process(&bytes);
            parser.screen().rows(0, 110).find(|r| r.contains("claude")).unwrap_or_default()
        };
        assert!(title(&mut ed).contains("Opus 5.5"), "default names the model it stands for");
        let logged = |needle: &str| {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !std::fs::read_to_string(&log).unwrap_or_default().contains(needle) {
                assert!(std::time::Instant::now() < end, "sent {needle}");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        };
        feed(&mut ed, "<esc><esc>:model haiku<ret>");
        assert_eq!(ed.config.llm.model.as_deref(), Some("haiku"));
        logged(r#""model":"haiku","subtype":"set_model""#);
        assert!(title(&mut ed).contains("Haiku 4.5"));
        feed(&mut ed, ":effort<ret>");
        assert!(
            ed.status.as_ref().is_some_and(|(m, _)| m == "Haiku 4.5 has no effort levels"),
            "{:?}",
            ed.status
        );
        feed(&mut ed, ":model opus<ret>:effort max<ret>");
        logged(r#""settings":{"effortLevel":"max"},"subtype":"apply_flag_settings""#);
        assert_eq!(ed.config.llm.effort, "max");
        assert!(title(&mut ed).contains("Opus 5.5 · max"));
        // Back to the model's own — no restart: a null effort
        feed(&mut ed, ":effort default<ret>");
        assert_eq!(ed.config.llm.effort, "");
        logged(r#""settings":{"effortLevel":null},"subtype":"apply_flag_settings""#);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The title says the model that answered, not the one configured: a chat whose process started on
    /// sonnet (its `init`) while `llm.model` now says opus names both; once an answer comes from opus
    /// (`message.model`), just its name. (The claude is a stand-in.)
    #[test]
    fn the_chat_title_names_the_model_that_answered() {
        use serde_json::json;
        let models = json!([
            {"value": "opus", "displayName": "Opus 5.5", "resolvedModel": "claude-opus-5-5",
             "description": "For complex work"},
            {"value": "sonnet", "displayName": "Sonnet 5.5", "resolvedModel": "claude-sonnet-5-5",
             "description": "Efficient for routine tasks"}]);
        let offer = json!({"type": "control_response", "response": {"subtype": "success", "request_id": "tarae-init",
            "response": {"models": models}}});
        let init =
            json!({"type": "system", "subtype": "init", "session_id": "s1", "model": "claude-sonnet-5-5"});
        let answer = json!({"type": "assistant", "message": {"model": "claude-opus-5-5", "role": "assistant",
            "content": [{"type": "text", "text": "hi"}]}});
        let result = json!({"type": "result", "result": "hi"});
        let mut ed = editor("");
        ed.config.llm.model = Some("opus".into());
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do case \"$line\" in *tarae-init*) printf '%s\\n' '{offer}' '{init}';; \
                 '{{\"message\"'*) printf '%s\\n' '{answer}' '{result}';; esac; done"
            ),
        ];
        // (wide: the chat's title has room for both)
        let title = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 200, 24).unwrap();
            let mut parser = vt100::Parser::new(24, 200, 0);
            parser.process(&bytes);
            parser.screen().rows(0, 200).find(|r| r.contains("claude")).unwrap_or_default()
        };
        feed(&mut ed, "<space>l");
        settle_until(&mut ed, |ed| ed.session.chat.as_ref().is_some_and(|c| !c.model.is_empty()));
        assert!(title(&mut ed).contains("claude-sonnet-5-5 · asked opus"), "{}", title(&mut ed));
        feed(&mut ed, "hello<ret>");
        settle_until(&mut ed, |ed| ed.session.chat.as_ref().is_some_and(|c| c.model == "claude-opus-5-5"));
        settle_until(&mut ed, |ed| ed.session.chat.as_ref().is_some_and(|c| !c.busy()));
        let t = title(&mut ed);
        assert!(t.contains("Opus 5.5") && !t.contains("asked"), "{t}");
    }

    /// Declined → claude is told so and nothing changes.
    #[test]
    fn chat_edits_can_be_declined() {
        let (dir, main, ..) = follow_project("edit-deny");
        let mut ed = editor("");
        ed.open(&main).unwrap();
        let input = serde_json::json!({"file_path": main, "old_string": "line 2", "new_string": "LINE 2"});
        let log = fake_edit(&mut ed, &dir, "Edit", input, false);
        feed(&mut ed, "<space>lshout<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        feed(&mut ed, "n");
        let a = permission_answer(&log);
        assert_eq!(a["behavior"], "deny");
        assert!(a["message"].as_str().unwrap().contains("declined"));
        assert_eq!(text(&ed), "line 1\nline 2\nline 3\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #54: closing the buffer a change of Claude's is under review in asks first — `n` keeps both, `y`
    /// declines it (Claude hears so) and closes it. Not a silent "declined".
    #[test]
    fn closing_a_buffer_under_review_asks() {
        let (dir, main, ..) = follow_project("edit-close");
        let mut ed = editor("");
        ed.open(&main).unwrap();
        let input = serde_json::json!({"file_path": main, "old_string": "line 2", "new_string": "LINE 2"});
        let log = fake_edit(&mut ed, &dir, "Edit", input, false);
        feed(&mut ed, "<space>lshout<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        let open = |ed: &Editor| ed.docs.iter().any(|d| d.path.as_deref() == Some(main.as_path()));
        crate::typed::execute(&mut ed, "buffer-close!").unwrap();
        assert!(open(&ed) && ed.session.review.is_some(), "asked, not closed");
        let q = ed.offers.first().map(|o| o.question.clone()).expect("a card");
        assert!(q.contains("y declines") && q.contains("n keeps"), "{q}");
        feed(&mut ed, "n");
        assert!(open(&ed) && ed.session.review.is_some() && ed.offers.is_empty(), "n: both kept");
        crate::typed::execute(&mut ed, "buffer-close!").unwrap();
        feed(&mut ed, "y");
        assert!(!open(&ed) && ed.session.review.is_none(), "y: declined and closed");
        let a = permission_answer(&log);
        assert_eq!(a["behavior"], "deny");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Plain `:bc` on a buffer under review with unsaved edits of yours: `y` declines Claude's change, and
    /// your edits keep the buffer open, as `:bc` keeps it — they're not dropped; `:bc!` then closes it.
    #[test]
    fn closing_under_review_keeps_your_unsaved_edits() {
        let (dir, main, ..) = follow_project("edit-close-unsaved");
        let mut ed = editor("");
        ed.open(&main).unwrap();
        feed(&mut ed, "imine <esc>");
        assert!(ed.doc().is_modified());
        let input = serde_json::json!({"file_path": main, "old_string": "line 2", "new_string": "LINE 2"});
        let log = fake_edit(&mut ed, &dir, "Edit", input, false);
        feed(&mut ed, "<space>lshout<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        let open = |ed: &Editor| ed.docs.iter().any(|d| d.path.as_deref() == Some(main.as_path()));
        crate::typed::execute(&mut ed, "buffer-close").unwrap();
        let q = ed.offers.first().map(|o| o.question.clone()).expect("a card");
        assert!(q.contains("unsaved edits keep the buffer open"), "said on the card: {q}");
        feed(&mut ed, "y");
        assert!(ed.session.review.is_none(), "y: declined");
        assert_eq!(permission_answer(&log)["behavior"], "deny");
        assert!(open(&ed), "your unsaved edits keep it open");
        assert_eq!(text(&ed), "mine line 1\nline 2\nline 3\n");
        assert!(
            ed.status.as_ref().is_some_and(|(m, _)| m.contains("unsaved changes") && m.contains(":bc!")),
            "{:?}",
            ed.status
        );
        crate::typed::execute(&mut ed, "buffer-close!").unwrap();
        assert!(!open(&ed), ":bc! discards them");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #54: an edit of Claude's arriving while you type in a buffer waits — said once — and opens when you
    /// stop (it took the screen, and the next keys answered it).
    #[test]
    fn an_edit_waits_while_you_type() {
        let (dir, main, util, _) = follow_project("edit-typing");
        let mut ed = editor("");
        ed.open(&main).unwrap();
        let input = serde_json::json!({"file_path": main, "old_string": "line 2", "new_string": "LINE 2"});
        let log = fake_edit(&mut ed, &dir, "Edit", input, false);
        feed(&mut ed, "<space>lshout<ret>");
        // Typing in another file before Claude's ask comes in
        ed.session.leave(crate::session::At::Chat);
        ed.open(&util).unwrap();
        feed(&mut ed, "ityping");
        settle_until(&mut ed, |ed| ed.session.chat.as_ref().unwrap().edits.queue.len() == 1);
        assert!(ed.session.review.is_none(), "it waits");
        assert_eq!(ed.mode, Mode::Insert);
        let waits = |ed: &Editor| ed.messages.iter().filter(|m| m.2.contains("waits — it opens")).count();
        assert_eq!(waits(&ed), 1);
        feed(&mut ed, " more");
        assert_eq!(waits(&ed), 1, "said once");
        assert!(ed.doc().text.to_string().starts_with("typing more"), "the keys stayed yours");
        ed.handle_event(Event::Key("esc".parse().unwrap()));
        // (as soon as where it leads is worked out — `edits::judge_later`)
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        assert_eq!(ed.doc().path.as_deref(), Some(main.as_path()));
        feed(&mut ed, "y");
        assert_eq!(permission_answer(&log)["behavior"], "allow");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Accepting part of a Write: claude writes exactly what was accepted.
    #[test]
    fn a_partly_accepted_write_writes_the_accepted_part() {
        let (dir, _, util, _) = follow_project("edit-partial");
        let mut ed = editor("");
        ed.open(&util).unwrap();
        let content = "LINE 1\nline 2\nline 3\nline 4\nLINE 5\n";
        let input = serde_json::json!({"file_path": util, "content": content});
        let log = fake_edit(&mut ed, &dir, "Write", input, false);
        feed(&mut ed, "<space>lshout<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        feed(&mut ed, "yn");
        let a = permission_answer(&log);
        assert_eq!(a["behavior"], "allow", "{a}");
        let accepted = "LINE 1\nline 2\nline 3\nline 4\nline 5\n";
        assert_eq!(a["updatedInput"]["content"], accepted);
        assert_eq!(text(&ed), accepted);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An edit's review scrolls sideways where lines don't wrap: `l` brings a long line's end into sight, `h`
    /// and Home take it back — they did nothing ("h does nothing in the review of this change"), and a line
    /// past the edge could be read only by closing the chat panel. Wrapping, they're unused, said so.
    #[test]
    fn an_edit_review_scrolls_sideways_where_lines_dont_wrap() {
        let mut ed = editor("short\n");
        ed.config.soft_wrap = "never".into();
        let id = ed.doc().id;
        let long = format!("let x = {}; // THE END\n", "1 + ".repeat(30));
        crate::llm::review_with(&mut ed, id, "x", vec![(0, 6, "short\n".into(), long)], None);
        let shown = |ed: &mut Editor| {
            let mut out = Vec::new();
            crate::term::render(ed, &mut out, 80, 20).unwrap();
            let mut vt = vt100::Parser::new(20, 80, 0);
            vt.process(&out);
            vt.screen().contents()
        };
        assert!(!shown(&mut ed).contains("THE END"), "past the edge");
        for _ in 0..10 {
            feed(&mut ed, "l");
        }
        let after = shown(&mut ed);
        assert!(after.contains("THE END"), "its end in sight:\n{after}");
        assert!(!after.contains("let x"), "its start scrolled off:\n{after}");
        feed(&mut ed, "h");
        assert_eq!(ed.session.review.as_ref().unwrap().left, 9 * 8);
        feed(&mut ed, "<home>");
        assert!(shown(&mut ed).contains("let x"), "back at its start");
        assert!(ed.session.review.is_some(), "still under review: nothing answered");
        // Wrapping: the line's end is on screen already, h/l are unused and said so
        ed.config.soft_wrap = "always".into();
        feed(&mut ed, "l");
        assert_eq!(ed.session.review.as_ref().unwrap().left, 0);
        let said = ed.messages.last().map(|m| m.2.clone()).unwrap_or_default();
        assert!(said.contains("does nothing") || said.contains("y takes it"), "{said}");
    }

    /// A new file of Claude's in a folder that isn't there yet, accepted, and Claude never writes it (its turn
    /// stopped — the fake here writes nothing): tarae's own save makes the folders, inside the project. Every
    /// save of such a file failed "No such file or directory" while the card had said accepted.
    #[test]
    fn an_accepted_new_file_in_a_new_folder_is_saved_inside_the_project() {
        let (dir, ..) = follow_project("edit-new-folder");
        let new = dir.join("src/fresh/deeper/new.rs");
        let mut ed = editor("");
        ed.session.root = dir.clone();
        let input = serde_json::json!({"file_path": new, "content": "pub fn new() {}\n"});
        let log = fake_edit(&mut ed, &dir, "Write", input, false);
        feed(&mut ed, "<space>lmake it<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        assert!(
            !ed.session.review.as_ref().unwrap().reason.contains("isn't there"),
            "inside: nothing to say"
        );
        feed(&mut ed, "y");
        assert_eq!(permission_answer(&log)["behavior"], "allow");
        crate::typed::execute(&mut ed, "write").unwrap();
        assert!(ed.flush_saves());
        assert_eq!(std::fs::read_to_string(&new).unwrap(), "pub fn new() {}\n", "its folders were made");
        assert!(!ed.doc().is_modified());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An accepted edit whose write from Claude never comes (its turn cut short; the fake writes nothing):
    /// when the wait for it is over tarae saves the buffer itself — with auto-save at "focus", the default,
    /// nothing else did, and the file stayed unsaved (•1) while the card had said accepted. Not once you've
    /// typed in it since: then it's your unsaved text, auto-save's to judge.
    #[test]
    fn an_accepted_edit_claude_never_writes_is_saved_by_tarae() {
        let (dir, main, ..) = follow_project("edit-never-written");
        let mut ed = editor("");
        ed.session.root = dir.clone();
        assert_eq!(ed.config.auto_save, "focus");
        ed.open(&main).unwrap();
        let input = serde_json::json!({"file_path": main, "old_string": "line 2", "new_string": "LINE 2"});
        let log = fake_edit(&mut ed, &dir, "Edit", input, false);
        feed(&mut ed, "<space>lshout<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        feed(&mut ed, "y");
        assert_eq!(permission_answer(&log)["behavior"], "allow");
        let id = ed.doc().id;
        let (_, version) = ed.doc().agent_writes.expect("waiting for Claude's write");
        assert!(ed.doc().is_modified());
        // (the wait's end, as its job brings it)
        ed.agent_write_missed(id, version);
        assert!(ed.flush_saves());
        assert_eq!(std::fs::read_to_string(&main).unwrap(), "line 1\nLINE 2\nline 3\n");
        assert!(!ed.doc().is_modified());
        // Typed since: left to auto-save
        let input = serde_json::json!({"file_path": main, "old_string": "line 3", "new_string": "LINE 3"});
        crate::chat::close(&mut ed);
        let log = fake_edit(&mut ed, &dir, "Edit", input, false);
        feed(&mut ed, "<space>lagain<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        feed(&mut ed, "y");
        assert_eq!(permission_answer(&log)["behavior"], "allow");
        let (_, version) = ed.doc().agent_writes.expect("waiting for Claude's write");
        ed.session.leave(crate::session::At::Chat);
        feed(&mut ed, "ggiX<esc>");
        ed.agent_write_missed(id, version);
        assert!(ed.flush_saves());
        assert_eq!(std::fs::read_to_string(&main).unwrap(), "line 1\nLINE 2\nline 3\n", "not saved");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The same outside the project: tarae doesn't make folders there — the review says so before you answer,
    /// and a save of tarae's says why it can't write (only Claude's own write makes that folder).
    #[test]
    fn an_accepted_new_file_outside_the_project_says_its_folder_isnt_made() {
        let (dir, ..) = follow_project("edit-new-folder-out");
        let project = dir.join("project");
        std::fs::create_dir_all(&project).unwrap();
        let new = dir.join("elsewhere/new.rs");
        let mut ed = editor("");
        ed.session.root = project.clone();
        let input = serde_json::json!({"file_path": new, "content": "pub fn new() {}\n"});
        let log = fake_edit(&mut ed, &dir, "Write", input, false);
        feed(&mut ed, "<space>lmake it<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        let reason = ed.session.review.as_ref().unwrap().reason.clone();
        assert!(
            reason.contains("elsewhere/ isn't there yet") && reason.contains("won't make it"),
            "{reason}"
        );
        feed(&mut ed, "y");
        assert_eq!(permission_answer(&log)["behavior"], "allow");
        crate::typed::execute(&mut ed, "write").unwrap();
        assert!(ed.flush_saves());
        assert!(!dir.join("elsewhere").exists(), "no folder made outside the project");
        let said = ed.messages.iter().any(|m| m.2.contains("tarae makes folders only inside the project"));
        assert!(said, "{:?}", ed.messages.iter().map(|m| &m.2).collect::<Vec<_>>());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Unsaved edits in the buffer: disk isn't what the user sees, so the change stays in the buffer and
    /// claude is told it's done there. And an edit whose turn ended (stopped) closes its review.
    #[test]
    fn edits_on_unsaved_buffers_and_ended_turns() {
        let (dir, main, ..) = follow_project("edit-dirty");
        let mut ed = editor("");
        ed.open(&main).unwrap();
        feed(&mut ed, "ggiX<esc>");
        let input = serde_json::json!({"file_path": main, "old_string": "line 2", "new_string": "LINE 2"});
        let log = fake_edit(&mut ed, &dir, "Edit", input.clone(), false);
        feed(&mut ed, "<space>lshout<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        feed(&mut ed, "y");
        let a = permission_answer(&log);
        assert_eq!(a["behavior"], "deny");
        assert!(a["message"].as_str().unwrap().contains("unsaved"), "{a}");
        assert_eq!(text(&ed), "Xline 1\nLINE 2\nline 3\n");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        // The turn ends while the review is up → it closes (claude isn't waiting any more)
        let dir2 = dir.join("second");
        std::fs::create_dir_all(&dir2).unwrap();
        crate::chat::close(&mut ed);
        fake_edit(
            &mut ed,
            &dir2,
            "Edit",
            serde_json::json!({"file_path": main, "old_string": "line 3", "new_string": "x"}),
            true,
        );
        feed(&mut ed, "<space>lagain<ret>");
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(ed.session.review.is_none(), "no orphaned review");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A fake claude that asks for two Edits to `file` back to back: p2 right after tarae answers p1 —
    /// before the watch has seen claude write p1 (no watch runs in tests). Its stdin goes to the log.
    fn fake_two_edits(ed: &mut Editor, dir: &Path, file: &Path) -> PathBuf {
        let log = dir.join("stdin.log");
        let ask = |id: &str, old: &str, new: &str| {
            serde_json::json!({"type": "control_request", "request_id": id,
                "request": {"subtype": "can_use_tool", "tool_name": "Edit",
                    "input": {"file_path": file, "old_string": old, "new_string": new}}})
        };
        // (lengths change, so the file's fingerprint surely does)
        let (p1, p2) = (ask("p1", "line 2", "LINE TWO"), ask("p2", "line 3", "LINE THREE"));
        let result = serde_json::json!({"type": "result", "result": "done"});
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do printf '%s\\n' \"$line\" >> '{}'; case \"$line\" in \
                 '{{\"message\"'*) printf '%s\\n' '{p1}';; *'\"p1\"'*) printf '%s\\n' '{p2}';; \
                 *'\"p2\"'*) printf '%s\\n' '{result}';; esac; done",
                log.display()
            ),
        ];
        log
    }

    /// Claude's second Edit to a file comes while its write of the first is still on the way: the buffer
    /// differs from disk only by that write, so the second is Claude's to write too — not left unsaved in
    /// the buffer (seen twice making :restart — the tab kept its dot until a test run saved it).
    #[test]
    fn back_to_back_chat_edits_to_one_file_are_both_written() {
        let (dir, main, ..) = follow_project("edit-twice");
        let mut ed = editor("");
        ed.open(&main).unwrap();
        let log = fake_two_edits(&mut ed, &dir, &main);
        feed(&mut ed, "<space>ltwo edits<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        feed(&mut ed, "y");
        assert_eq!(permission_answer_to(&log, "p1")["behavior"], "allow");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        feed(&mut ed, "y");
        let a = permission_answer_to(&log, "p2");
        assert_eq!(a["behavior"], "allow", "{a}");
        assert_eq!(
            (a["updatedInput"]["old_string"].as_str(), a["updatedInput"]["new_string"].as_str()),
            (Some("line 3"), Some("LINE THREE"))
        );
        // Claude writes it; the watch takes that as the save
        let done = "line 1\nLINE TWO\nLINE THREE\n";
        assert_eq!(text(&ed), done);
        std::fs::write(&main, done).unwrap();
        ed.on_focus(true);
        settle_until(&mut ed, |ed| !ed.doc().is_modified());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// …but if the user typed in the buffer after accepting the first, disk isn't what they see: the second
    /// stays in the buffer, as with any unsaved edits.
    #[test]
    fn back_to_back_chat_edits_after_typing_stay_in_the_buffer() {
        let (dir, main, ..) = follow_project("edit-twice-typed");
        let mut ed = editor("");
        ed.open(&main).unwrap();
        let log = fake_two_edits(&mut ed, &dir, &main);
        feed(&mut ed, "<space>ltwo edits<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        feed(&mut ed, "y");
        // (the answer gave the keys back to the chat: Esc to the code, then typed — before p2 is read, events
        // wait for settle_until)
        assert!(ed.session.is_at(crate::session::At::Chat), "back in the chat");
        feed(&mut ed, "<esc>ggiX<esc>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        feed(&mut ed, "y");
        let a = permission_answer_to(&log, "p2");
        assert_eq!(a["behavior"], "deny", "{a}");
        assert!(a["message"].as_str().unwrap().contains("unsaved"), "{a}");
        assert_eq!(text(&ed), "Xline 1\nLINE TWO\nLINE THREE\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The chat's title row, drawn.
    fn chat_title(ed: &mut Editor) -> String {
        let mut bytes = Vec::new();
        crate::term::render(ed, &mut bytes, 110, 24).unwrap();
        let mut parser = vt100::Parser::new(24, 110, 0);
        parser.process(&bytes);
        parser.screen().rows(0, 110).find(|r| r.contains("claude")).unwrap_or_default()
    }

    /// `space v a` asks first: a card that says edits go into the files before they're seen and that the
    /// checks run that code. `n` leaves it off; `y` turns it on for this chat — the title says so and Claude
    /// hears of it with the next message — and `space v a` again turns it off without asking.
    #[test]
    fn review_at_the_end_asks_first() {
        let scratch = crate::scratch::Scratch::new("at-end");
        let dir = scratch.to_path_buf();
        let config = dir.join("config.toml");
        std::fs::write(&config, "# mine\n[llm]\nfollow = false\n").unwrap();
        // Kept at once, in the order you toggle (a worker each could land them the other way round, and the
        // config watch would take that back) — the file's own lines with it
        let kept = |_: &mut Editor, want: &str| {
            let now = std::fs::read_to_string(&config).unwrap();
            assert!(now.contains(want), "{now}");
            assert!(now.contains("# mine") && now.contains("follow = false"), "{now}");
        };
        let mut ed = editor("");
        ed.config_file = Some(config.clone());
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        feed(&mut ed, "<space>va");
        let o = ed.offers.first().expect("a card");
        assert!(matches!(o.what, crate::offer::What::ReviewAtEnd));
        assert!(o.weighty(), "y must have been in sight a moment to count");
        assert!(
            o.question.contains("before you see them") && o.question.contains("cargo test"),
            "{}",
            o.question
        );
        ed.offer_dismiss();
        assert!(!crate::edits::at_end(&ed));
        assert!(ed.session.chat.is_none());
        feed(&mut ed, "<space>va");
        ed.offer_accept();
        assert!(crate::edits::at_end(&ed));
        let c = ed.session.chat.as_ref().expect("the chat it's for");
        assert!(!ed.session.is_at(At::Chat), "the keys stay in the editor");
        assert!(c.msgs.iter().any(|m| m.text.contains("space v a turns it off")));
        assert!(chat_title(&mut ed).contains("review at end"), "{}", chat_title(&mut ed));
        assert_eq!(ed.session.checkpoints.tell.len(), 1);
        assert!(
            ed.session.checkpoints.tell[0].contains("when your turn ends"),
            "{:?}",
            ed.session.checkpoints.tell
        );
        kept(&mut ed, "review-at-end = true");
        feed(&mut ed, "<space>va");
        assert!(ed.offers.is_empty(), "off without asking");
        assert!(!crate::edits::at_end(&ed));
        assert!(!chat_title(&mut ed).contains("review at end"));
        assert!(ed.session.checkpoints.tell.is_empty(), "on then off before a message: nothing to tell");
        kept(&mut ed, "review-at-end = false");
        // Typed: on asks, off doesn't; a new chat keeps it (#51: it reset per chat), and Claude hears it
        feed(&mut ed, ":review-at-end on<ret>");
        assert!(matches!(ed.offers.first().map(|o| &o.what), Some(crate::offer::What::ReviewAtEnd)));
        ed.offer_accept();
        assert!(crate::edits::at_end(&ed));
        feed(&mut ed, ":review-at-end on<ret>");
        assert!(ed.offers.is_empty(), "already on");
        ed.session.checkpoints.tell.clear(); // (heard with a message)
        crate::chat::reset(&mut ed);
        assert!(crate::edits::at_end(&ed), "a new chat keeps it");
        assert_eq!(ed.session.checkpoints.tell.len(), 1, "the new conversation hears it");
        // Closed and opened again (as at the next start: the setting): still on
        crate::chat::close(&mut ed);
        crate::chat::open(&mut ed);
        assert!(crate::edits::at_end(&ed), "a chat opened later starts so");
        ed.session.leave(crate::session::At::Chat);
        feed(&mut ed, ":review-at-end on<ret>:review-at-end off<ret>");
        assert!(ed.offers.is_empty(), "off takes the card away");
        crate::chat::reset(&mut ed);
        assert!(!crate::edits::at_end(&ed), "off: new chats come one edit at a time");
        kept(&mut ed, "review-at-end = false");
        // On and straight off again: off is what's kept
        crate::edits::at_end_set(&mut ed, true);
        crate::edits::at_end_set(&mut ed, false);
        kept(&mut ed, "review-at-end = false");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A fake claude that asks to use each of `asks` (tool, input) in turn — p1 on the question, the next once
    /// tarae answers the one before — and ends the turn after the last answer. With `write`, each answer is
    /// taken as an allow and the file written as claude would (an Edit on the file as it is by then). Its stdin
    /// goes to the log.
    fn fake_asks(ed: &mut Editor, dir: &Path, asks: &[(&str, serde_json::Value)], write: bool) -> PathBuf {
        let log = dir.join("stdin.log");
        let ask = |i: usize| {
            let (tool, input) = &asks[i];
            serde_json::json!({"type": "control_request", "request_id": format!("p{}", i + 1),
                "request": {"subtype": "can_use_tool", "tool_name": tool, "input": input}})
        };
        // What each one writes, worked out now and put beside the log: `cp` once it's answered
        let mut files: HashMap<PathBuf, String> = HashMap::new();
        let mut writes = Vec::new();
        for (i, (tool, input)) in asks.iter().enumerate() {
            let path = PathBuf::from(input["file_path"].as_str().unwrap());
            let now = files
                .entry(path.clone())
                .or_insert_with(|| std::fs::read_to_string(&path).unwrap_or_default());
            *now = match *tool {
                "Write" => input["content"].as_str().unwrap().to_string(),
                _ => now.replacen(
                    input["old_string"].as_str().unwrap(),
                    input["new_string"].as_str().unwrap(),
                    1,
                ),
            };
            let prepared = dir.join(format!(".claude-writes-{i}"));
            std::fs::write(&prepared, &*now).unwrap();
            let folder = path.parent().unwrap().display().to_string();
            writes.push(format!("mkdir -p '{folder}'; cp '{}' '{}'; ", prepared.display(), path.display()));
        }
        let result = serde_json::json!({"type": "result", "result": "done"});
        let mut cases = format!("'{{\"message\"'*) printf '%s\\n' '{}';; ", ask(0));
        for i in 1..=asks.len() {
            let next = if i < asks.len() { ask(i).to_string() } else { result.to_string() };
            let then = if write { writes[i - 1].as_str() } else { "" };
            cases.push_str(&format!("*'\"p{i}\"'*) {then}printf '%s\\n' '{next}';; "));
        }
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do printf '%s\\n' \"$line\" >> '{}'; case \"$line\" in {cases}esac; done",
                log.display()
            ),
        ];
        log
    }

    /// Review at the end: an edit inside the project goes in unasked — claude is allowed its own input and
    /// writes it; tarae keeps what the file was. One outside the project, to a listed file (a build
    /// script), or to a file with unsaved edits is still a review first.
    #[test]
    fn review_at_the_end_lets_edits_in_unasked() {
        use serde_json::json;
        let (dir, main, util, _) = follow_project("at-end");
        let (build, new) = (dir.join("build.rs"), dir.join("src/new.rs"));
        std::fs::write(&build, "fn main() {}\n").unwrap();
        let away = dir.with_file_name(format!("tarae-at-end-away-{}", std::process::id()));
        std::fs::create_dir_all(&away).unwrap();
        let outside = away.join("notes.txt");
        std::fs::write(&outside, "note\n").unwrap();
        let edit =
            |f: &Path, old: &str, new: &str| json!({"file_path": f, "old_string": old, "new_string": new});
        let mut ed = editor("");
        ed.open(&util).unwrap();
        feed(&mut ed, "ggiX<esc>"); // unsaved edits in util.rs
        let log = fake_asks(
            &mut ed,
            &dir,
            &[
                ("Edit", edit(&main, "line 2", "LINE 2")),
                ("Write", json!({"file_path": new, "content": "pub fn new() {}\n"})),
                ("Edit", edit(&build, "fn main() {}", "fn main() { run() }")),
                ("Edit", edit(&outside, "note", "NOTE")),
                ("Edit", edit(&util, "line 3", "LINE 3")),
            ],
            false,
        );
        feed(&mut ed, "<space>l");
        ed.session.chat.as_mut().unwrap().edits.at_end = Some(dir.clone());
        feed(&mut ed, "write them<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        // p1, p2: allowed as asked, nothing shown, nothing written by tarae
        for (id, input) in [
            ("p1", edit(&main, "line 2", "LINE 2")),
            ("p2", json!({"file_path": new, "content": "pub fn new() {}\n"})),
        ] {
            let a = permission_answer_to(&log, id);
            assert_eq!((&a["behavior"], &a["updatedInput"]), (&json!("allow"), &input), "{id}: {a}");
        }
        assert_eq!(std::fs::read_to_string(&main).unwrap(), "line 1\nline 2\nline 3\n", "claude writes it");
        assert!(!new.exists());
        let turn = &ed.session.checkpoints.turn;
        let kept = |p: &Path| {
            turn.iter().find(|t| t.path == p).map(|t| (t.before.as_ref().map(|r| r.to_string()), t.unseen))
        };
        assert_eq!(kept(&main), Some((Some("line 1\nline 2\nline 3\n".into()), true)), "{turn:?}");
        assert_eq!(kept(&new), Some((None, true)), "a file it makes: none before");
        // p3: a build script is a review
        assert_eq!(ed.doc().path.as_deref(), Some(build.as_path()));
        feed(&mut ed, "y");
        assert_eq!(permission_answer_to(&log, "p3")["behavior"], "allow");
        // p4: outside the project too
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        assert_eq!(ed.doc().path.as_deref(), Some(outside.as_path()));
        feed(&mut ed, "y");
        // p5: unsaved edits in util.rs — a review, and it stays in the buffer
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        assert_eq!(ed.doc().path.as_deref(), Some(util.as_path()));
        let kept = |p: &Path| ed.session.checkpoints.turn.iter().find(|t| t.path == p).map(|t| t.unseen);
        assert_eq!(kept(&build), Some(false), "seen in its review");
        feed(&mut ed, "y");
        let a = permission_answer_to(&log, "p5");
        assert!(a["behavior"] == "deny" && a["message"].as_str().unwrap().contains("unsaved"), "{a}");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&away).ok();
    }

    /// #208, the grammar half: a file only Claude's turn opened, its language's grammar not here — no offer
    /// (`grammar_hinted` records one first, whether offers are on or not, as `lsp.missing` does for servers).
    /// Moved in, it's the user's: the grammar is looked for again and offered.
    #[test]
    fn a_missing_grammar_is_offered_only_once_you_take_the_file_up() {
        let dir = std::env::temp_dir().join(format!("tarae-grammar-claudes-only-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "x = 1\n").unwrap();
        let mut ed = Editor::new(Config::default());
        ed.open_for_claude(&file).unwrap();
        // A language whose grammar is never here (a real one may be downloaded on this machine), loaded as
        // `attach_syntax` loads a file's
        let mut lang = syntax::spec("lua").expect("lua's pack").clone();
        (lang.name, lang.grammar) = ("tarae-test-lang".into(), "tarae-test-no-such-grammar".into());
        let spec: &'static syntax::LangSpec = Box::leak(Box::new(lang));
        ed.load_language(ed.doc().id, spec);
        // (until its load is back: the grammar missing, its offer held)
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while ed.doc().grammar_held.is_none() {
            let ev = ed.events.recv_timeout(end.saturating_duration_since(std::time::Instant::now()));
            ed.handle_event(ev.expect("its load back, the offer held"));
        }
        assert!(!ed.grammar_hinted.contains(&spec.grammar), "only Claude's turn opened it: nothing offered");
        ed.handle_event(Event::Key("l".parse().unwrap()));
        assert!(!ed.doc().claudes_only, "moved in it: yours");
        assert!(ed.grammar_hinted.contains(&spec.grammar), "and its grammar offered");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #37: a file outside the project that a chat edit's review opened closes when the turn ends — its root
    /// keeps no language server — and a `:restart` meanwhile wouldn't bring it back. One the user pressed
    /// keys in stays theirs; keys typed in the chat's input don't take it up.
    #[test]
    fn files_a_turn_opened_outside_the_project_close_with_it() {
        use serde_json::json;
        let (dir, main, ..) = follow_project("aside");
        let away = dir.with_file_name(format!("tarae-aside-away-{}", std::process::id()));
        std::fs::create_dir_all(&away).unwrap();
        let outside = std::fs::canonicalize(&away).unwrap().join("config.toml");
        std::fs::write(&outside, "a = 1\n").unwrap();
        // #46: and one in the project's build output (a test's fixture)
        std::fs::create_dir_all(dir.join("target/sb")).unwrap();
        let fixture = dir.join("target/sb/fixture.toml");
        std::fs::write(&fixture, "b = 1\n").unwrap();
        let open = |ed: &Editor, p: &Path| ed.docs.iter().any(|d| d.path.as_deref() == Some(p));
        let mut ed = editor("");
        ed.session.root = dir.clone();
        ed.open(&main).unwrap();
        let edit = json!({"file_path": outside, "old_string": "a = 1", "new_string": "a = 2"});
        let in_target = json!({"file_path": fixture, "old_string": "b = 1", "new_string": "b = 2"});
        fake_asks(&mut ed, &dir, &[("Edit", edit), ("Edit", in_target)], true);
        feed(&mut ed, "<space>l");
        ed.handle_event(Event::Key("c".parse().unwrap())); // (typed in the chat's input)
        feed(&mut ed, "hange it<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        assert_eq!(ed.doc().path.as_deref(), Some(outside.as_path()), "its review");
        let restarted = crate::restart::snapshot(&mut ed);
        let saved: Vec<&str> =
            restarted["docs"].as_array().unwrap().iter().filter_map(|d| d["path"].as_str()).collect();
        assert_eq!(saved, [main.to_str().unwrap()], "a restart doesn't bring it");
        ed.handle_event(Event::Key("y".parse().unwrap())); // (the review's answer isn't taking it up)
        // The fixture's review: between the two, keys that go elsewhere (`:` and esc) don't take up the
        // file in front (#46: any key did)
        settle_until(&mut ed, |ed| ed.session.review.is_some() && ed.doc().path.as_deref() == Some(&fixture));
        for k in ["y", ":", "esc"] {
            ed.handle_event(Event::Key(k.parse().unwrap()));
        }
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "a = 2\n");
        assert_eq!(std::fs::read_to_string(&fixture).unwrap(), "b = 2\n");
        assert!(!open(&ed, &outside), "closed with the turn");
        assert!(!open(&ed, &fixture), "in target/: closed with the turn too");
        assert!(open(&ed, &main));
        // Taken up: a move in the code pane while it's in front — it stays when the turn ends
        ed.open(&outside).unwrap();
        ed.doc_mut().aside = true;
        ed.session.leave(crate::session::At::Chat);
        for k in [":", "esc"] {
            ed.handle_event(Event::Key(k.parse().unwrap()));
        }
        assert!(ed.doc().aside, "nothing moved: not yet");
        ed.handle_event(Event::Key("l".parse().unwrap()));
        crate::edits::turn_over(&mut ed);
        assert!(open(&ed, &outside), "yours now");
        // (as typed: each key an event)
        let keys = |ed: &mut Editor, ks: &[&str]| {
            for k in ks {
                ed.handle_event(Event::Key(k.parse().unwrap()));
            }
        };
        // Switched to by you (`ga`, back from your file): taken up
        ed.doc_mut().aside = true;
        ed.open(&main).unwrap();
        keys(&mut ed, &["g", "a"]);
        assert_eq!(ed.doc().path.as_deref(), Some(outside.as_path()));
        assert!(!ed.doc().aside, "you went there");
        // Brought to the front only by `:bc` on your own file: not taken up — it goes with the turn
        ed.doc_mut().aside = true;
        ed.open(&main).unwrap();
        keys(&mut ed, &[":", "b", "c", "ret"]);
        assert!(!open(&ed, &main));
        assert_eq!(ed.doc().path.as_deref(), Some(outside.as_path()), "in front now");
        assert!(ed.doc().aside, "closing yours isn't going there");
        crate::edits::turn_over(&mut ed);
        assert!(!open(&ed, &outside), "closed with the turn");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&away).ok();
    }

    /// #149: buffers are shared between sessions — another session's turn ending closes its own aside files,
    /// never the new file this session's Claude is showing in its review (it did: the review stayed, on a
    /// closed buffer, drawing nothing and keeping the keys for 80 minutes). A review whose buffer goes anyway
    /// ends with it and Claude is told why; one found without its buffer doesn't keep the keys.
    #[test]
    fn a_review_s_buffer_outlives_another_session_s_turn() {
        use serde_json::json;
        let (dir, main, ..) = follow_project("aside-149");
        let away = dir.with_file_name(format!("tarae-aside-149-away-{}", std::process::id()));
        std::fs::create_dir_all(&away).unwrap();
        let away = std::fs::canonicalize(&away).unwrap();
        let (setup, theirs) = (away.join("setup.sh"), away.join("theirs.toml"));
        std::fs::write(&theirs, "c = 1\n").unwrap();
        let open = |ed: &Editor, p: &Path| ed.docs.iter().any(|d| d.path.as_deref() == Some(p));
        let mut ed = editor("");
        ed.session.root = dir.clone();
        ed.open(&main).unwrap();
        let log = fake_asks(
            &mut ed,
            &dir,
            &[("Write", json!({"file_path": setup, "content": "echo hi\n"}))],
            false,
        );
        feed(&mut ed, "<space>l");
        feed(&mut ed, "make it<ret>");
        settle_until(&mut ed, |ed| ed.session.review.as_ref().is_some_and(|r| r.ready));
        assert_eq!(ed.doc().path.as_deref(), Some(setup.as_path()), "its review, a new file");
        // Another session: a file of its own opened aside, then its Claude's turn ends
        let other = ed.session_add(away.clone());
        ed.with_session(other, |ed| {
            ed.open(&theirs).unwrap();
            ed.doc_mut().aside = true;
            ed.session.chat = Some(crate::chat::Chat::new());
            ed.session.at = crate::session::At::Chat;
            crate::edits::turn_over(ed);
        });
        assert!(!open(&ed, &theirs), "its own aside file goes with its turn");
        assert!(open(&ed, &setup), "the one under review here stays");
        // #150: Claude waits on that review — its session says so, in front or behind, and the inbox has it
        let front = ed.session.id;
        assert_eq!(ed.doing(&ed.session), crate::session::Doing::Waiting);
        ed.session_switch(other);
        let behind = ed.sessions.iter().find(|l| l.id == front).unwrap();
        assert_eq!(ed.doing(behind), crate::session::Doing::Waiting, "behind too");
        assert_eq!(ed.cards_waiting(), (0, 1), "{:?}", ed.inbox());
        ed.session_switch(front);
        ed.handle_event(Event::Key("y".parse().unwrap()));
        assert_eq!(permission_answer_to(&log, "p1")["behavior"], "allow", "answered");
        // Closed under a review all the same: the review ends with it, Claude told why
        let reviewed = |ed: &mut Editor, id: &str, doc: &Path| {
            ed.session.chat.as_mut().unwrap().edits.queue.push_back(crate::edits::Pending {
                request_id: id.into(),
                tool: "Write".into(),
                input: json!({"file_path": doc, "content": "echo bye\n"}),
                reason: String::new(),
                since: std::time::Instant::now(),
                row: None,
                protected: Some(false),
            });
            ed.handle_event(Event::Job(Box::new(|_| {})));
            ed.session.review.as_ref().filter(|r| r.ready).map(|r| r.doc_id).expect("its review")
        };
        let doc = reviewed(&mut ed, "p9", &setup);
        ed.close_doc(doc);
        assert!(ed.session.review.is_none(), "the review went with its buffer");
        let a = permission_answer_to(&log, "p9");
        assert!(a["behavior"] == "deny" && a["message"].as_str().unwrap().contains("was closed"), "{a}");
        // Found without its buffer (gone some other way): it ends and says so — the keys stay the editor's
        let doc = reviewed(&mut ed, "p10", &setup);
        ed.session.review.as_mut().unwrap().doc_id = doc + 1000;
        ed.handle_event(Event::Job(Box::new(|_| {})));
        assert!(ed.session.review.is_none(), "not kept with nothing to draw");
        assert!(ed.messages.last().unwrap().2.contains("can't be shown"), "{:?}", ed.messages.last());
        assert_eq!(permission_answer_to(&log, "p10")["behavior"], "deny");
        assert!(!matches!(ed.keys_to().region, crate::uistate::Region::EditReview));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&away).ok();
    }

    /// Review at the end, on a fake claude that edits main.rs and lib.rs and makes src/new.rs (turns kept in
    /// `store`, if given).
    fn turn_of_three(name: &str, store: Option<&Path>) -> (Editor, PathBuf, [PathBuf; 3]) {
        use serde_json::json;
        let (dir, main, _, lib) = follow_project(name);
        let new = dir.join("src/new.rs");
        let edit =
            |f: &Path, old: &str, new: &str| json!({"file_path": f, "old_string": old, "new_string": new});
        let mut ed = editor("");
        ed.session.checkpoints.dir = store.map(Path::to_path_buf);
        ed.viewport = (30, 100);
        ed.open(&lib).unwrap();
        fake_asks(
            &mut ed,
            &dir,
            &[
                ("Edit", edit(&main, "line 2", "LINE 2")),
                ("Write", json!({"file_path": new, "content": "pub fn new() {}\n"})),
                ("Edit", edit(&lib, "line 9", "LINE 9")),
            ],
            true,
        );
        feed(&mut ed, "<space>l");
        ed.session.chat.as_mut().unwrap().edits.at_end = Some(dir.clone());
        feed(&mut ed, "three files<ret>");
        (ed, dir, [main, lib, new])
    }

    /// Its review is up and the file in front has its diff.
    fn reviewing(ed: &Editor) -> bool {
        ed.session.code_review.is_some()
            && ed.doc().review_base.is_some()
            && ed.doc().git_version == ed.doc().version()
    }

    /// Review at the end: when the turn ends, what Claude wrote is read from disk and the turn opens as a
    /// review — its files in the rail, each against what it was before the turn, `]g` on into the next file.
    /// `space g d` takes one change back, `space v q` keeps the rest, `space v t` opens the turn again, and
    /// `space u` takes it all back.
    #[test]
    fn the_turn_is_reviewed_when_it_ends() {
        let (mut ed, dir, [main, lib, new]) = turn_of_three("turn-review", None);
        settle_until(&mut ed, reviewing);
        let c = ed.session.chat.as_ref().unwrap();
        assert!(!c.busy() && !ed.session.is_at(At::Chat), "the keys went to the review");
        assert!(c.msgs.iter().any(|m| m.text == "changed 3 files · reviewing them now"));
        // The chat beside it leaves no room for the rail: the header says which file of the review this is
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 110, 30).unwrap();
        let mut parser = vt100::Parser::new(30, 110, 0);
        parser.process(&bytes);
        let header = parser.screen().rows(0, 110).next().unwrap();
        assert!(header.contains("lib.rs  review 1/3"), "{header}");
        let r = ed.session.code_review.as_ref().unwrap();
        assert!(
            matches!(&r.base, crate::codereview::Base::Turn { question, .. } if question == "three files"),
            "{:?}",
            r.base
        );
        let files: Vec<(&str, usize, usize, bool)> =
            r.files.iter().map(|f| (f.rel.as_str(), f.added, f.removed, f.new)).collect();
        assert_eq!(files, [("lib.rs", 1, 1, false), ("main.rs", 1, 1, false), ("src/new.rs", 1, 0, true)]);
        // On the file in front (lib.rs), its change selected; its base is what it was before the turn
        assert_eq!(ed.doc().path.as_deref(), Some(lib.as_path()));
        assert_eq!(text(&ed), std::fs::read_to_string(&lib).unwrap(), "the buffer has Claude's write");
        assert_eq!(ed.doc().git_hunks.len(), 1);
        assert_eq!(ed.cursor_line(), 8);
        assert!(ed.doc().review_base.as_ref().unwrap().to_string().contains("line 9\n"));
        // ]F: the next file (main.rs), [F back; ]g past lib.rs's one change: main.rs
        feed(&mut ed, "]F");
        settle_until(&mut ed, |ed| reviewing(ed) && ed.doc().path.as_deref() == Some(main.as_path()));
        feed(&mut ed, "[F");
        settle_until(&mut ed, |ed| reviewing(ed) && ed.doc().path.as_deref() == Some(lib.as_path()));
        feed(&mut ed, "]g");
        settle_until(&mut ed, |ed| reviewing(ed) && ed.doc().path.as_deref() == Some(main.as_path()));
        assert_eq!(ed.cursor_line(), 1);
        // One change back (`space v u`, as the rail says): that's the user's edit now (the buffer, saved by
        // auto-save as usual)
        feed(&mut ed, "<space>vu");
        assert_eq!(text(&ed), "line 1\nline 2\nline 3\n");
        // (files not viewed: asked first — #10)
        feed(&mut ed, "<space>vqy");
        assert!(ed.session.code_review.is_none());
        feed(&mut ed, "<space>vt");
        settle_until(&mut ed, reviewing);
        assert_eq!(ed.session.code_review.as_ref().unwrap().files.len(), 3, "the turn's files again");
        // All of it back: lib.rs as before, new.rs gone, main.rs as the user left it
        feed(&mut ed, "<space>u");
        let lib_now =
            ed.docs.iter().find(|d| d.path.as_deref() == Some(lib.as_path())).unwrap().text.to_string();
        assert!(lib_now.contains("line 9\n") && !lib_now.contains("LINE 9"), "{lib_now}");
        assert!(!new.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// While the turn's files are read (a worker), `space u` waits for them (a restart too — restart.rs). A
    /// review already open isn't replaced by the turn's.
    #[test]
    fn the_turn_review_waits_its_turn() {
        let (mut ed, dir, _) = turn_of_three("turn-review-wait", None);
        settle_until(&mut ed, |ed| ed.session.checkpoints.gathering);
        let err = crate::checkpoint::step(&mut ed, true).unwrap_err();
        assert!(err.contains("a moment"), "{err}");
        settle_until(&mut ed, reviewing);
        assert!(crate::checkpoint::can_undo(&ed));
        std::fs::remove_dir_all(&dir).ok();
        // A review open already: it stays, the row says how to get to the turn's
        let (mut ed, dir, _) = turn_of_three("turn-review-busy", None);
        let other =
            crate::codereview::CodeReview::for_test(crate::codereview::Base::Head, dir.clone(), Vec::new());
        ed.session.code_review = Some(other);
        settle_until(&mut ed, |ed| {
            !ed.session.chat.as_ref().unwrap().busy() && !ed.session.checkpoints.gathering
        });
        assert_eq!(ed.session.code_review.as_ref().unwrap().base, crate::codereview::Base::Head);
        let c = ed.session.chat.as_ref().unwrap();
        assert!(c.msgs.iter().any(|m| m.text == "changed 3 files · space v t reviews them"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// What's kept of the turns in `store` for this folder, once a worker's save has `ok` (5 s at most).
    fn turns_kept(
        store: &Path,
        ok: impl Fn(&crate::checkpoint::Stored) -> bool,
    ) -> crate::checkpoint::Stored {
        let root = test_root();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let kept = crate::checkpoint::load_whole(store, &root);
            if ok(&kept) {
                return kept;
            }
            assert!(std::time::Instant::now() < end, "never kept: {kept:?}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// Turns are kept on disk: a new tarae in the same folder (`:restart`, or after a crash) still has the
    /// latest turn — `space v t` reviews it, `space u` takes it back (and that's kept too).
    #[test]
    fn turns_outlive_tarae() {
        let store = std::env::temp_dir().join(format!("tarae-turns-kept-{}", std::process::id()));
        let (mut ed, dir, [_, lib, new]) = turn_of_three("turns-kept", Some(&store));
        settle_until(&mut ed, reviewing);
        let kept = turns_kept(&store, |k| k.list.len() == 1);
        assert_eq!(kept.list[0].files.len(), 3);
        drop(ed);
        let mut ed = editor("");
        ed.session.checkpoints.dir = Some(store.clone());
        crate::checkpoint::restore(&mut ed);
        settle_until(&mut ed, |ed| !ed.session.checkpoints.list.is_empty());
        assert_eq!(ed.session.checkpoints.list[0].question, "three files");
        feed(&mut ed, "<space>vt");
        settle_until(&mut ed, reviewing);
        assert_eq!(ed.session.code_review.as_ref().unwrap().files.len(), 3);
        feed(&mut ed, "<space>vqy<space>u");
        assert!(!std::fs::read_to_string(&lib).unwrap().contains("LINE 9"));
        assert!(!new.exists());
        turns_kept(&store, |k| k.list.first().is_some_and(|c| c.undone));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&store).ok();
    }

    /// #49: `space v t` reviews the latest turn — the turns before it that went in unseen (their review
    /// replaced by a later one's, never ended) are said. A turn whose review you ended is seen.
    #[test]
    fn review_turn_says_earlier_turns_unseen() {
        use crate::checkpoint::{Checkpoint, Touched};
        let (dir, main, ..) = follow_project("turn-unseen");
        let mut ed = editor("");
        ed.session.root = dir.clone();
        let now = std::fs::read_to_string(&main).unwrap();
        let turn = |q: &str| {
            let t = Touched {
                path: main.clone(),
                before: Some(ropey::Rope::from_str("x\n")),
                after: ropey::Rope::from_str(&now),
                unseen: true,
                ..Default::default()
            };
            Checkpoint::new(q.into(), vec![t])
        };
        for q in ["one", "two", "three"] {
            ed.session.checkpoints.list.push(turn(q));
        }
        let unseen = |ed: &Editor| -> Vec<String> {
            ed.messages.iter().filter(|m| m.2.contains("earlier turn")).map(|m| m.2.clone()).collect()
        };
        feed(&mut ed, "<space>vt");
        settle_until(&mut ed, reviewing);
        assert_eq!(unseen(&ed), ["2 earlier turns unseen — space v v for all"]);
        // Ended by you (its file not viewed: `y` on the card): seen — the next turn's review says nothing of it
        feed(&mut ed, "<space>vqy");
        ed.session.checkpoints.list.push(turn("four"));
        ed.messages.clear();
        feed(&mut ed, "<space>vt");
        settle_until(&mut ed, reviewing);
        assert!(unseen(&ed).is_empty(), "{:?}", unseen(&ed));
        // `space v v` — the one the warning points to — ended: the turns whose files it showed are seen. One
        // that didn't show them (another file only) leaves them as they were.
        let git_review = |ed: &mut Editor, path: &Path| {
            let rel = path.file_name().unwrap().to_string_lossy().into_owned();
            let f = crate::codereview::File {
                rel,
                path: path.to_path_buf(),
                added: 1,
                removed: 1,
                new: false,
                gone: false,
                binary: false,
            };
            let r =
                crate::codereview::CodeReview::for_test(crate::codereview::Base::Head, dir.clone(), vec![f]);
            ed.session.code_review = Some(r);
            feed(ed, "<space>vqy");
        };
        feed(&mut ed, "<space>vqy");
        for q in ["five", "six"] {
            ed.session.checkpoints.list.push(turn(q));
        }
        git_review(&mut ed, &dir.join("util.rs"));
        ed.session.checkpoints.list.push(turn("seven"));
        ed.messages.clear();
        feed(&mut ed, "<space>vt");
        settle_until(&mut ed, reviewing);
        assert_eq!(unseen(&ed), ["2 earlier turns unseen — space v v for all"], "util.rs isn't theirs");
        feed(&mut ed, "<space>vqy");
        ed.session.checkpoints.list.push(turn("eight"));
        git_review(&mut ed, &main);
        ed.session.checkpoints.list.push(turn("nine"));
        ed.messages.clear();
        feed(&mut ed, "<space>vt");
        settle_until(&mut ed, reviewing);
        assert!(unseen(&ed).is_empty(), "seen in space v v: {:?}", unseen(&ed));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #49: `]g` goes hunk by hunk — changes a few lines apart are one, as `git diff` shows them — `]G` change
    /// by change, `[g` back (not onto the hunk it's on); `]F` the review's next file.
    #[test]
    fn bracket_g_walks_hunks_and_capital_g_each_change() {
        let base: String = (0..30).map(|i| format!("line {i}\n")).collect();
        let now = base
            .replace("line 2\n", "LINE 2\n")
            .replace("line 4\n", "LINE 4\n")
            .replace("line 6\n", "LINE 6\n");
        let now = now.replace("line 14\n", "LINE 14\n");
        let now = now.replace("line 22\n", "LINE 22\n").replace("line 24\n", "LINE 24\n");
        let mut ed = editor(&now);
        ed.doc_mut().git_hunks = crate::git::hunks(&base, &now);
        assert_eq!(ed.doc().git_hunks.len(), 6);
        let sel = |ed: &Editor| {
            let r = ed.doc().selection().primary();
            let t = &ed.doc().text;
            (t.byte_to_line(r.from()), t.byte_to_line(r.to().saturating_sub(1)))
        };
        feed(&mut ed, "]g");
        assert_eq!(sel(&ed), (2, 6), "the rewritten lines as one");
        feed(&mut ed, "]g]g");
        assert_eq!(sel(&ed), (22, 24));
        feed(&mut ed, "[g");
        assert_eq!(sel(&ed), (14, 14), "back: the one before, not the one it's on");
        feed(&mut ed, "gg]G");
        assert_eq!(sel(&ed), (2, 2), "]G: each change");
        feed(&mut ed, "]G");
        assert_eq!(sel(&ed), (4, 4));
        // No review: ]F says so
        feed(&mut ed, "]F");
        let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert!(said.contains("no review"), "{said}");
    }

    /// A turn cut short (tarae ended mid-turn — a crash) comes back as a turn of its own: what it wrote
    /// unseen against what was there before, so it can still be reviewed and taken back.
    #[test]
    fn a_turn_cut_short_comes_back() {
        let store = std::env::temp_dir().join(format!("tarae-turns-cut-{}", std::process::id()));
        let (dir, main, ..) = follow_project("turn-cut");
        let mut ed = editor("");
        ed.session.checkpoints.dir = Some(store.clone());
        crate::checkpoint::record_unseen(
            &mut ed,
            main.clone(),
            Some(ropey::Rope::from_str("line 1\nline 2\nline 3\n")),
            |_| {},
        );
        std::fs::write(&main, "line 1\nLINE 2\nline 3\n").unwrap(); // claude's write
        turns_kept(&store, |k| k.turn.len() == 1);
        drop(ed); // (no turn end)
        let mut ed = editor("");
        ed.session.checkpoints.dir = Some(store.clone());
        crate::checkpoint::restore(&mut ed);
        settle_until(&mut ed, |ed| !ed.session.checkpoints.list.is_empty());
        let cp = &ed.session.checkpoints.list[0];
        assert_eq!(cp.question, "a turn cut short");
        let f = &cp.files.here().expect("cut short this run: its texts held")[0];
        assert_eq!(
            (f.path.as_path(), f.before.as_ref().map(|b| b.to_string())),
            (main.as_path(), Some("line 1\nline 2\nline 3\n".into()))
        );
        assert_eq!(f.after.to_string(), "line 1\nLINE 2\nline 3\n", "as it is now");
        // Kept as a turn now, not cut short any more
        turns_kept(&store, |k| k.turn.is_empty() && k.list.len() == 1);
        feed(&mut ed, "<space>u");
        assert_eq!(std::fs::read_to_string(&main).unwrap(), "line 1\nline 2\nline 3\n");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&store).ok();
    }

    /// Review at the end: what a file was is on disk before claude is allowed to write it — a crash right
    /// after still has it. (Saves are held here: the answer waits for them.)
    #[test]
    fn what_a_file_was_is_kept_before_claude_writes_it() {
        use serde_json::json;
        let store = std::env::temp_dir().join(format!("tarae-turns-first-{}", std::process::id()));
        let (dir, main, ..) = follow_project("kept-first");
        let mut ed = editor("");
        ed.session.checkpoints.dir = Some(store.clone());
        let edit = json!({"file_path": main, "old_string": "line 2", "new_string": "LINE 2"});
        let log = fake_asks(&mut ed, &dir, &[("Edit", edit)], true);
        feed(&mut ed, "<space>l");
        ed.session.chat.as_mut().unwrap().edits.at_end = Some(dir.clone());
        let held = crate::checkpoint::hold_saves();
        feed(&mut ed, "edit it<ret>");
        settle_until(&mut ed, |ed| !ed.session.checkpoints.turn.is_empty());
        // (a moment for an answer to reach claude)
        let end = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while std::time::Instant::now() < end {
            if let Ok(Some(ev)) = ed.events.recv_within(std::time::Duration::from_millis(10)) {
                ed.handle_event(ev);
            }
        }
        let sent = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(!sent.contains("\"request_id\":\"p1\""), "answered before what it was is kept: {sent}");
        drop(held);
        settle_until(&mut ed, |_| std::fs::read_to_string(&log).unwrap_or_default().contains("\"p1\""));
        assert_eq!(permission_answer_to(&log, "p1")["behavior"], "allow");
        let was = |t: &crate::checkpoint::Touched| {
            t.path == main && t.before.as_ref().is_some_and(|b| *b == "line 1\nline 2\nline 3\n")
        };
        turns_kept(&store, |k| {
            k.turn.iter().any(was) || k.list.iter().any(|c| c.files.here().is_some_and(|f| f.iter().any(was)))
        });
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&store).ok();
    }

    /// #154: `space o` in a review — the outline goes in front of the rail, as the git tab does (it had the
    /// keys, the status line said OUTLINE, and the rail stayed drawn in its place); again: the rail back.
    #[test]
    fn space_o_in_a_review_shows_the_outline() {
        let (mut ed, dir, _) = turn_of_three("turn-review-space-o", None);
        settle_until(&mut ed, reviewing);
        let screen = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 220, 30).unwrap();
            let mut parser = vt100::Parser::new(30, 220, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        assert!(screen(&mut ed).contains("undo the whole turn"), "the rail");
        feed(&mut ed, "<space>o");
        let s = screen(&mut ed);
        assert!(
            ed.session.sidebar.as_ref().is_some_and(|s| s.mode == crate::sidebar::Mode::Outline)
                && ed.session.is_at(At::Side)
        );
        assert!(!ed.review_rail_up() && ed.screen.side_w > 0, "the outline is drawn: {s}");
        assert!(s.contains("review behind") && !s.contains("undo the whole turn"), "{s}");
        feed(&mut ed, "<space>o");
        assert!(ed.review_rail_up() && ed.session.sidebar.is_none(), "closed: the rail back");
        assert!(screen(&mut ed).contains("undo the whole turn"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #146: on a screen where the git tab fits beside the code but the review's rail doesn't (the rail wants
    /// the code 90 wide), "review behind · space g back to it" was a promise `space g` didn't keep — the keys
    /// went to a rail nobody could see. Back with the keys, it's drawn in the tab's room; once it lets the
    /// keys go (a file opened), it gives the code its columns again.
    #[test]
    fn space_g_back_to_the_review_shows_the_rail_on_a_narrow_screen() {
        let (mut ed, dir, _) = turn_of_three("turn-review-narrow", None);
        settle_until(&mut ed, reviewing);
        let sb = crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, dir.clone());
        ed.session.sidebar = Some(sb);
        let screen = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            // (the chat open: the code gets 84 — the tab fits beside it, the rail not)
            crate::term::render(ed, &mut bytes, 120, 30).unwrap();
            let mut parser = vt100::Parser::new(30, 120, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        screen(&mut ed);
        assert_eq!(ed.screen.rail_w, 0, "unfocused, the rail gives way at this width");
        feed(&mut ed, "<space>g");
        let s = screen(&mut ed);
        assert!(s.contains("review behind") && s.contains("space g back"), "{s}");
        feed(&mut ed, "<space>g");
        let s = screen(&mut ed);
        assert!(ed.review_rail_up() && ed.session.is_at(crate::session::At::Rail), "the rail has the keys");
        assert!(ed.screen.rail_w > 0 && !ed.screen.review_rail.borrow().is_empty(), "and it's drawn: {s}");
        assert!(!s.contains("review behind"), "{s}");
        // A file opened from it: the keys go to the code, and so do the columns
        feed(&mut ed, "<ret>");
        screen(&mut ed);
        assert_eq!(ed.screen.rail_w, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// While a review is open, its rail and the git tab take turns in the left column (#19 — a status line
    /// saying the tab was behind went unseen): `space g` puts the tab in front with the keys, the review
    /// still open (a row at its top says so, the status line which file); `space g` or Esc there gives the
    /// column back to the rail, with the keys — no key lands in the code (`d` would delete the change).
    #[test]
    fn space_g_in_a_review_takes_turns_with_the_rail() {
        let (mut ed, dir, _) = turn_of_three("turn-review-space-g", None);
        settle_until(&mut ed, reviewing);
        // (the tab rooted in the test's folder, not this repository)
        let sb = crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, dir.clone());
        ed.session.sidebar = Some(sb);
        let screen = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 220, 30).unwrap();
            let mut parser = vt100::Parser::new(30, 220, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        let before = text(&ed);
        let s = screen(&mut ed);
        assert!(s.contains("undo the whole turn") && !s.contains("review behind"), "the rail: {s}");
        feed(&mut ed, "<space>g");
        let s = screen(&mut ed);
        assert!(ed.session.code_review.is_some() && !ed.review_rail_up(), "the review open, behind the tab");
        assert!(
            ed.session.sidebar.as_ref().is_some_and(|s| s.mode == crate::sidebar::Mode::Git)
                && ed.session.is_at(At::Side)
        );
        assert!(s.contains("review behind") && s.contains("space g back"), "{s}");
        assert!(!s.contains("undo the whole turn"), "the rail gave the column: {s}");
        assert!(s.contains("review 1/3"), "the status line says which file: {s}");
        feed(&mut ed, "jkd");
        assert_eq!(text(&ed), before, "the keys are the tab's");
        // `space g` again: the rail, with the keys
        feed(&mut ed, "<space>g");
        assert!(ed.review_rail_up() && ed.session.is_at(crate::session::At::Rail), "the rail has the keys");
        assert!(!ed.session.is_at(crate::session::At::Side));
        let s = screen(&mut ed);
        assert!(s.contains("m viewed, next  q end") && !s.contains("review behind"), "{s}");
        feed(&mut ed, "d");
        assert_eq!(text(&ed), before, "the change is still there");
        // Esc in the tab: the same
        feed(&mut ed, "<esc><space>g");
        assert!(!ed.review_rail_up());
        feed(&mut ed, "<esc>");
        assert!(ed.review_rail_up() && ed.session.is_at(crate::session::At::Rail), "Esc: the rail again");
        // The tab closed (`q`): the rail is back by itself
        feed(&mut ed, "<esc><space>gq");
        assert!(ed.session.sidebar.is_none() && ed.review_rail_up());
        assert!(screen(&mut ed).contains("undo the whole turn"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A throwaway git repository: `files` committed on `main`, then `run` on top.
    fn git_repo(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = crate::scratch::Scratch::new(&format!("review-{name}")).into_path();
        for (f, text) in files {
            std::fs::write(dir.join(f), text).unwrap();
        }
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
            &["config", "commit.gpgsign", "false"],
            &["add", "-A"],
            &["commit", "-q", "-m", "base"],
        ] {
            git_in(&dir, args);
        }
        std::fs::canonicalize(dir).unwrap()
    }

    /// A worktree of `repo` beside it, on a new branch `name` — another session's checkout.
    fn worktree(repo: &Path, name: &str) -> PathBuf {
        let wt = repo.with_file_name(format!("{}-{name}", repo.file_name().unwrap().to_string_lossy()));
        std::fs::remove_dir_all(&wt).ok();
        git_in(repo, &["worktree", "add", "-q", "-b", name, wt.to_str().unwrap()]);
        std::fs::canonicalize(wt).unwrap()
    }

    /// Session `id`, wherever it is (in front or behind).
    fn session_by(ed: &Editor, id: crate::event::SessionId) -> &crate::session::Session {
        std::iter::once(&ed.session).chain(&ed.sessions).find(|l| l.id == id).expect("the session")
    }

    /// A session behind goes on: its Claude (started in its checkout) streams into its own chat, not the front
    /// one's — and the front draws nothing for it but what it says; what it reads doesn't move the editor.
    #[test]
    fn a_background_sessions_chat_streams_into_its_session() {
        use serde_json::json;
        let repo = git_repo("session-chat", &[("a.txt", "one\n")]);
        let wt = worktree(&repo, "fix");
        let ran_in = wt.join("claude-ran-here");
        let text = |t: &str| json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": t}}});
        let read = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "r1", "name": "Read", "input": {"file_path": wt.join("a.txt")}}]}});
        let mut ed = editor("");
        ed.session.root = repo.clone();
        feed(&mut ed, ":o a.txt<ret>");
        let front_doc = ed.doc().id;
        fake_claude_stream(
            &mut ed,
            &[
                json!(format!("pwd > '{}'", ran_in.display())),
                json!("sleep 0.3"),
                read,
                text("Hello "),
                json!("sleep 0.2"),
                text("from the worktree"),
                json!({"type": "result", "result": "done"}),
            ],
        );
        feed(&mut ed, &format!(":session-open {}<ret>", wt.display()));
        let behind = ed.session.id;
        assert_eq!((behind, ed.session.root.clone()), (1, wt.clone()));
        feed(&mut ed, "<space>lhi<ret><esc>:session-switch 0<ret>");
        assert_eq!(ed.session.root, repo, "session 0 in front");
        assert!(session_by(&ed, behind).chat.as_ref().unwrap().busy(), "switched while Claude works");
        // (what's said from here on — `:session-open`'s own toast names it too)
        let before = ed.messages.len();
        let (mut events, mut drawn) = (0, 0);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while session_by(&ed, behind).chat.as_ref().unwrap().busy() {
            let left = end.saturating_duration_since(std::time::Instant::now());
            let ev = ed.events.recv_timeout(left).expect("the session behind's answer");
            // (the front's own results — `:session-open`'s path completion — draw as ever)
            let from_behind = matches!(ev, Event::For(id, _) if id == behind);
            ed.unchanged = false;
            ed.handle_event(ev);
            if from_behind {
                events += 1;
                drawn += usize::from(!std::mem::take(&mut ed.unchanged));
            }
        }
        assert!(events >= 3, "its answer came as {events} events");
        let c = session_by(&ed, behind).chat.as_ref().unwrap();
        let said: Vec<&str> = c.msgs.iter().map(|m| m.text.as_str()).collect();
        assert!(said.iter().any(|m| m.contains("Hello from the worktree")), "{said:?}");
        assert!(ed.session.chat.is_none(), "nothing in the front session's chat");
        assert_eq!(ed.doc().id, front_doc, "its read didn't move the editor in front");
        let opened = |p: &Path| ed.docs.iter().any(|d| d.path.as_deref() == Some(p));
        assert!(!opened(&wt.join("a.txt")), "nor opened its file behind");
        assert_eq!(std::fs::read_to_string(&ran_in).unwrap().trim(), wt.to_str().unwrap());
        let said = ed
            .messages
            .iter()
            .skip(before)
            .filter(|m| m.2.starts_with(&format!("{} · ", session_by(&ed, behind).name)));
        assert_eq!(drawn, said.count(), "{events} events behind drew {drawn} frames in front");
        feed(&mut ed, ":session-switch 1<ret>");
        assert!(ed.session.chat.as_ref().unwrap().msgs.iter().any(|m| m.text.contains("from the worktree")));
        git_in(&repo, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
        std::fs::remove_dir_all(&repo).ok();
    }

    /// A job that does `what` — then says, wrongly, that it changed nothing on screen; whether the main loop
    /// draws after it (`frame_due`, as `term::event_loop` asks) and what the log said.
    fn wrongly_unchanged(
        ed: &mut Editor,
        what: impl FnOnce(&mut Editor) + Send + 'static,
    ) -> (bool, Vec<String>) {
        FRAMES_OWED.with(|o| o.borrow_mut().clear());
        ed.unchanged = false;
        ed.handle_event(Event::Job(Box::new(move |ed: &mut Editor| {
            what(ed);
            ed.unchanged = true;
        })));
        let unchanged = std::mem::take(&mut ed.unchanged);
        assert!(unchanged, "the event said nothing changed");
        (ed.frame_due(unchanged), FRAMES_OWED.with(|o| o.borrow().clone()))
    }

    /// ci round 4 (CI's macOS: a frame in the idle check's window, its cause guessed): a frame says what drew it
    /// (`frames_for`, the state's) — a job's result by the line that sent or spawned it, one that names itself
    /// by its name; an event that changed nothing isn't among them, and a frame that wrote nothing isn't kept.
    #[test]
    fn a_frame_says_what_drew_it() {
        let mut ed = editor("one\n");
        let tx = ed.events.sender();
        let sent = line!() + 1;
        tx.send(Event::Job(Box::new(|_: &mut Editor| {}))).unwrap();
        tx.send(Event::Job(Box::new(|ed: &mut Editor| ed.unchanged = true))).unwrap();
        tx.send(Event::Job(Box::new(|ed: &mut Editor| ed.drawn_for("a named job")))).unwrap();
        let spawned = line!() + 1;
        ed.events.jobs().spawn(|| |_: &mut Editor| {});
        let mut unchanged = true;
        for _ in 0..4 {
            let ev = ed.events.recv_timeout(std::time::Duration::from_secs(10)).expect("each job's result");
            unchanged &= crate::term::handle_noting(&mut ed, ev);
        }
        assert!(crate::term::batch_done(&mut ed, unchanged));
        ed.frames_written += 1;
        ed.frame_went(true);
        let said: Vec<String> =
            ed.frames_for.iter().flat_map(|(_, by)| by.iter().map(|b| b.to_string())).collect();
        let here = |line: u32| format!("job src/editor.rs:{line}");
        assert_eq!(said, [here(sent), "a named job".into(), here(spawned)]);
        assert_eq!(ed.frames_for.back().map(|f| f.0), Some(1));
        // (a frame that wrote nothing: its reasons go, nothing kept)
        ed.drawn_by.push(DrawnFor::Named("the resting redraw"));
        ed.frame_went(false);
        assert!(ed.drawn_by.is_empty() && ed.frames_for.len() == 1);
    }

    /// A frame drawn though every event of its batch said nothing changed says what was owed (`batch_done`'s
    /// `owed: …`): a card that came, a toast — the reason #200's guard drew it, never an empty list.
    #[test]
    fn a_frame_drawn_for_what_was_owed_says_so() {
        let mut ed = editor("one\n");
        crate::term::render(&mut ed, &mut Vec::new(), 100, 30).unwrap();
        let drawn = |ed: &mut Editor, what: Box<dyn FnOnce(&mut Editor) + Send>| {
            let job = Event::Job(Box::new(move |ed: &mut Editor| {
                what(ed);
                ed.unchanged = true;
            }));
            let unchanged = crate::term::handle_noting(ed, job);
            assert!(unchanged && crate::term::batch_done(ed, unchanged), "owed: drawn");
            ed.frames_written += 1;
            ed.frame_went(true);
            crate::term::render(ed, &mut Vec::new(), 100, 30).unwrap();
            ed.frames_for.back().map(|(_, by)| by.iter().map(ToString::to_string).collect::<Vec<_>>())
        };
        let card = drawn(
            &mut ed,
            Box::new(|ed| {
                let what = crate::offer::What::ReviewAtEnd;
                ed.push_offer(crate::offer::Offer::new("Owed card".into(), "q", "w", String::new(), what));
            }),
        );
        assert_eq!(card, Some(vec!["owed: card \"Owed card\"".to_string()]));
        let toast = drawn(&mut ed, Box::new(|ed| ed.set_status("an owed toast")));
        assert_eq!(toast, Some(vec!["owed: toast \"an owed toast\"".to_string()]));
    }

    /// #200's guard: an event that wrongly says it changed nothing still gets its frame when something came
    /// that no frame has shown — a card, a toast, a UI-state ask whose input has come — and the log says what
    /// was owed. Once drawn, nothing more is owed.
    #[test]
    fn a_wrong_nothing_changed_still_draws_what_came() {
        let mut ed = editor("one\n");
        crate::term::render(&mut ed, &mut Vec::new(), 100, 30).unwrap();
        let (due, said) = wrongly_unchanged(&mut ed, |_| {});
        assert!(!due && said.is_empty(), "nothing came: no frame — {said:?}");
        // A card
        let (due, said) = wrongly_unchanged(&mut ed, |ed| {
            let what = crate::offer::What::ReviewAtEnd;
            ed.push_offer(crate::offer::Offer::new("Owed card".into(), "q", "w", String::new(), what));
        });
        assert!(due, "the card is drawn");
        assert_eq!(said.len(), 1, "{said:?}");
        assert!(said[0].contains("card \"Owed card\""), "{said:?}");
        crate::term::render(&mut ed, &mut Vec::new(), 100, 30).unwrap();
        assert!(!wrongly_unchanged(&mut ed, |_| {}).0, "drawn: nothing owed");
        // A toast
        let (due, said) = wrongly_unchanged(&mut ed, |ed| ed.set_status("an owed toast"));
        assert!(due && said.len() == 1 && said[0].contains("toast \"an owed toast\""), "{said:?}");
        crate::term::render(&mut ed, &mut Vec::new(), 100, 30).unwrap();
        // A UI-state ask, its input come — answered by the frame that's drawn for it
        let (tx, rx) = std::sync::mpsc::channel();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let ask = crate::uistate::Ask {
            after_input: ed.input_seq,
            json: true,
            reply: tx,
            until,
            what: crate::uistate::Asked::State,
        };
        let (due, said) = wrongly_unchanged(&mut ed, move |ed| ed.ui_asks.push(ask));
        assert!(due && said.len() == 1 && said[0].contains("a UI-state ask"), "{said:?}");
        crate::term::render(&mut ed, &mut Vec::new(), 100, 30).unwrap();
        assert!(rx.try_recv().is_ok(), "answered by that frame");
        assert!(!wrongly_unchanged(&mut ed, |_| {}).0);
        // A key (#200 as met: every key came back "unchanged") — whatever it changed, you look for it
        let (due, said) = wrongly_unchanged(&mut ed, |ed| ed.handle_key(Key::plain(Code::Char(':'))));
        assert!(due && said.len() == 1 && said[0].contains("input (a key, click or paste)"), "{said:?}");
        assert!(ed.prompt.is_some(), "the command line it opened is what that frame shows");
        crate::term::render(&mut ed, &mut Vec::new(), 100, 30).unwrap();
        assert!(!wrongly_unchanged(&mut ed, |_| {}).0, "drawn: nothing owed");
    }

    /// The guard costs an idle tarae nothing: a card under a picker (never drawn), an error waiting for its
    /// card under it, an ask waiting for input still to come, a key in the picker — after one frame none of
    /// it is owed again, and twenty events saying nothing changed draw no frame and log nothing.
    #[test]
    fn what_stays_undrawn_is_owed_one_frame_only() {
        let mut ed = editor("one\n");
        let what = crate::offer::What::ReviewAtEnd;
        ed.push_offer(crate::offer::Offer::new("Hidden".into(), "q", "w", String::new(), what));
        ed.picker = Some(Picker::new("files", Vec::new(), true));
        ed.set_error("an error under the picker");
        let (tx, _rx) = std::sync::mpsc::channel();
        let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let ask = crate::uistate::Ask {
            after_input: ed.input_seq + 1,
            json: true,
            reply: tx,
            until,
            what: crate::uistate::Asked::State,
        };
        ed.ui_asks.push(ask);
        ed.handle_key(Key::plain(Code::Down));
        assert!(ed.picker.is_some(), "the key went to the picker");
        assert!(ed.frame_due(true), "never drawn: owed");
        crate::term::render(&mut ed, &mut Vec::new(), 100, 30).unwrap();
        assert!(
            ed.screen.toast_card.get().is_none() && ed.toasts.iter().any(Toast::waiting),
            "still undrawn"
        );
        for i in 0..20 {
            let (due, said) = wrongly_unchanged(&mut ed, |_| {});
            assert!(!due && said.is_empty(), "event {i}: {said:?}");
        }
    }

    /// #200's guard in `handle_event`: what runs after every event can make it drawn, never not — a step there
    /// that says "nothing changed" leaves a key, a resize and a job drawn; a job that said so itself stays so.
    #[test]
    fn what_runs_after_an_event_never_says_it_changed_nothing() {
        let mut ed = editor("one\n");
        AFTER_EVENT.set(Some(|ed: &mut Editor| ed.unchanged = true));
        let mut drawn = |ev: Event| {
            ed.unchanged = false;
            ed.handle_event(ev);
            !std::mem::take(&mut ed.unchanged)
        };
        let key = drawn(Event::Key(Key::plain(Code::Char('l'))));
        let resize = drawn(Event::Resize);
        let job = drawn(Event::Job(Box::new(|_: &mut Editor| {})));
        let quiet = drawn(Event::Job(Box::new(|ed: &mut Editor| ed.unchanged = true)));
        AFTER_EVENT.set(None);
        assert!(key && resize && job, "drawn: key {key}, resize {resize}, job {job}");
        assert!(!quiet, "an event that said it changed nothing stays so");
    }

    /// A session behind's terminal goes on and its output is read into it — a terminal started in front
    /// meanwhile doesn't make its output "an older run's".
    #[test]
    fn a_background_sessions_terminal_keeps_its_output() {
        if crate::sandbox::no_pty_here("editor::tests::a_background_sessions_terminal_keeps_its_output") {
            return;
        }
        let repo = git_repo("session-term", &[("a.txt", "one\n")]);
        let wt = worktree(&repo, "term");
        let mut ed = editor("");
        ed.session.root = repo.clone();
        let behind = ed.session_open(&wt).unwrap();
        let sh = |s: &str| vec!["/bin/sh".to_string(), "-c".into(), s.into()];
        assert!(ed.terminal_run(sh("sleep 0.3; echo from-behind; sleep 5"), Vec::new()));
        assert!(ed.session_switch(0));
        assert!(ed.terminal_run(sh("echo in-front; sleep 5"), Vec::new()));
        let shows = |t: Option<&crate::terminal::Terminal>, s: &str| {
            t.is_some_and(|t| t.parser.screen().contents().contains(s))
        };
        settle_until(&mut ed, |ed| {
            shows(session_by(ed, behind).terminal.as_ref(), "from-behind")
                && shows(ed.session.terminal.as_ref(), "in-front")
        });
        assert!(
            !shows(ed.session.terminal.as_ref(), "from-behind"),
            "each session's output in its own terminal"
        );
        assert!(ed.session_switch(behind));
        assert!(shows(ed.session.terminal.as_ref(), "from-behind"));
        ed.session.terminal = None;
        ed.sessions.clear();
        git_in(&repo, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
        std::fs::remove_dir_all(&repo).ok();
    }

    /// A session's panes, its document and the cursor in it come back when it does; `:session-switch` alone
    /// lists the sessions; a relative path opens in the session's own checkout; `:session-open` on a checkout
    /// that has a session brings that one.
    #[test]
    fn a_sessions_panes_come_back_when_it_does() {
        let repo = git_repo("session-views", &[("a.txt", "one\ntwo\nthree\n")]);
        let wt = worktree(&repo, "views");
        let mut ed = editor("");
        (ed.session.root, ed.session.name) = (repo.clone(), "tarae-review-session-views".into());
        feed(&mut ed, ":o a.txt<ret>jj<C-w>v");
        assert_eq!(ed.session.views.len(), 2);
        feed(&mut ed, &format!(":session-open {}<ret>", wt.display()));
        assert_eq!(ed.session.views.len(), 1, "a new session starts with one pane");
        // (said as a session — the one word for it)
        let (msg, _) = ed.status.clone().unwrap();
        let name = wt.file_name().unwrap().to_string_lossy();
        assert_eq!(msg, format!("{name} · session 2 of 2 — space W lists them"));
        feed(&mut ed, ":o a.txt<ret>j");
        assert_eq!(ed.doc().path.as_deref(), Some(wt.join("a.txt").as_path()));
        assert_eq!(ed.docs.len(), 2, "its first file took its blank scratch's place, as at startup");
        feed(&mut ed, ":session-switch 0<ret>");
        assert_eq!(ed.session.views.len(), 2);
        assert_eq!(ed.doc().path.as_deref(), Some(repo.join("a.txt").as_path()));
        assert_eq!(cursor_line(&ed), 2);
        feed(&mut ed, ":session-switch 1<ret>");
        assert_eq!(ed.doc().path.as_deref(), Some(wt.join("a.txt").as_path()));
        assert_eq!(cursor_line(&ed), 1);
        feed(&mut ed, ":session-switch<ret>");
        let (msg, _) = ed.status.clone().unwrap();
        assert!(msg.starts_with("usage: :session-switch <n> — "), "{msg}");
        assert!(msg.contains("0 tarae-review-session-views") && msg.contains("-views ●"), "{msg}");
        feed(&mut ed, &format!(":session-open {}<ret>", repo.display()));
        assert_eq!((ed.session.id, ed.sessions.len()), (0, 1), "a checkout with a session gets that session");
        git_in(&repo, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Claude's cards in a session behind wait for it: nothing comes up over the front session — no card, no
    /// review, the cursor stays — and it's said once, named; in front they're up; leaving again, they go
    /// with it. Harmless: the read it asks for is never answered (a file in the test's own temp folder).
    #[test]
    fn a_background_claudes_cards_wait_for_its_session() {
        use serde_json::json;
        let repo = git_repo("session-card", &[("a.txt", "one\n")]);
        let wt = worktree(&repo, "card");
        let file = wt.join("a.txt");
        let out = repo.with_file_name(format!("tarae-session-outside-{}", std::process::id()));
        std::fs::create_dir_all(&out).unwrap();
        let outside = std::fs::canonicalize(&out).unwrap().join("notes.md");
        std::fs::write(&outside, "# marker\n").unwrap();
        let ask = |id: &str, tool: &str, input: serde_json::Value| {
            json!({"type": "control_request", "request_id": id,
            "request": {"subtype": "can_use_tool", "tool_name": tool, "input": input}})
        };
        let mut ed = editor("");
        ed.session.root = repo.clone();
        feed(&mut ed, ":o a.txt<ret>");
        let front_doc = ed.doc().id;
        fake_claude_stream(
            &mut ed,
            &[
                json!("sleep 0.3"),
                ask("r1", "Read", json!({"file_path": outside})),
                ask("e1", "Edit", json!({"file_path": file, "old_string": "one", "new_string": "ONE"})),
            ],
        );
        let behind = ed.session_open(&wt).unwrap();
        feed(&mut ed, "<space>lfix it<ret><esc>:session-switch 0<ret>");
        // (the edit judged too — where it leads, on a worker: `edits::next` shows none before; Linux's workers
        // came after the queue, and the review wasn't up yet at the switch)
        let waiting = |ed: &Editor| {
            let l = session_by(ed, behind);
            l.parked.iter().any(crate::offer::Offer::claudes)
                && l.chat.as_ref().is_some_and(|c| c.edits.queue.iter().any(|p| p.protected.is_some()))
        };
        settle_until(&mut ed, waiting);
        assert!(ed.offers.is_empty(), "no card over the front session");
        assert!(ed.session.review.is_none(), "no review over it");
        assert_eq!(ed.doc().id, front_doc, "the front stays where it was");
        let told: Vec<&String> = ed
            .messages
            .iter()
            .map(|m| &m.2)
            .filter(|m| m.contains(" · waits for you — C-q goes there"))
            .collect();
        assert_eq!(told.len(), 1, "said once: {told:?}");
        assert!(told[0].starts_with(&format!("{} · ", session_by(&ed, behind).name)), "named: {}", told[0]);
        // In front: its card and its review are up, on its own file
        feed(&mut ed, ":session-switch 1<ret>");
        assert!(ed.offers.iter().any(crate::offer::Offer::claudes), "the card");
        assert!(ed.session.review.is_some(), "the review");
        assert_eq!(ed.doc().path.as_deref(), Some(file.as_path()));
        // Away again: Claude's card goes with its session
        ed.session_switch(0);
        assert!(ed.offers.is_empty());
        assert!(session_by(&ed, behind).parked.iter().any(crate::offer::Offer::claudes));
        ed.sessions.clear();
        git_in(&repo, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&out).ok();
    }

    /// #189: Claude's edit waiting in a session behind is news, as a card there is: a toast naming the
    /// session (and how `C-q` gets there), its row marked, `C-q` reaching it — also when an earlier wait there
    /// ended while it stayed behind (it was said once and never again), and when you leave a session with its
    /// review up (Claude waits on it, so nothing else would come to say so). Harmless: a file in the test's
    /// own temp checkout.
    #[test]
    fn an_edit_waiting_behind_is_news() {
        use serde_json::json;
        let repo = git_repo("session-edit-news", &[("a.txt", "one\n")]);
        let wt = worktree(&repo, "edit-news");
        let file = wt.join("a.txt");
        let edit = json!({"type": "control_request", "request_id": "e1", "request": {"subtype": "can_use_tool",
            "tool_name": "Edit", "input": {"file_path": file, "old_string": "one", "new_string": "ONE"}}});
        let mut ed = editor("");
        ed.session.root = repo.clone();
        feed(&mut ed, ":o a.txt<ret>");
        fake_claude_stream(&mut ed, &[json!("sleep 0.3"), edit]);
        let behind = ed.session_open(&wt).unwrap();
        feed(&mut ed, "<space>lfix it<ret><esc>:session-switch 0<ret>");
        let told =
            |ed: &Editor| ed.messages.iter().filter(|m| m.2.contains(" · waits for you — C-q")).count();
        // A wait there that ended while it stayed behind: said once
        waits_for_you(&mut ed, behind);
        assert_eq!(told(&ed), 1);
        ed.with_session(behind, |ed| ed.drop_cards(|w| matches!(w, crate::offer::What::Screen { .. })));
        assert_ne!(ed.doing(session_by(&ed, behind)), crate::session::Doing::Waiting);
        // Then its edit: said again, its row marked
        // (in line, where it leads worked out — `edits::judge_later`)
        settle_until(&mut ed, |ed| {
            session_by(ed, behind)
                .chat
                .as_ref()
                .is_some_and(|c| c.edits.queue.front().is_some_and(|p| p.protected.is_some()))
        });
        assert_eq!(told(&ed), 2, "said again: {:?}", ed.messages.iter().map(|m| &m.2).collect::<Vec<_>>());
        assert_eq!(ed.doing(session_by(&ed, behind)), crate::session::Doing::Waiting);
        // C-q: there, its review up
        feed(&mut ed, "<C-q>");
        assert_eq!(ed.session.id, behind);
        assert!(ed.session.review.as_ref().is_some_and(|r| r.ready), "its review up");
        // Left with its review up: said as it goes behind
        feed(&mut ed, ":session-switch 0<ret>");
        assert_eq!(told(&ed), 3, "said on leaving");
        assert_eq!(ed.doing(session_by(&ed, behind)), crate::session::Doing::Waiting);
        ed.sessions.clear();
        git_in(&repo, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Switching sessions with 20 files open in each — a number for `cargo perf` (it's two swaps and a few
    /// lookups, whatever is open). At opt-level 0 it skips itself, saying so past libtest's capture.
    #[test]
    fn perf_session_switch_with_twenty_files_each() {
        if crate::test_results::unoptimized("editor::tests::perf_session_switch_with_twenty_files_each") {
            return;
        }
        let files: Vec<(String, String)> = (0..20)
            .map(|i| (format!("f{i}.rs"), (0..2_000).map(|l| format!("fn f{l}() {{ {i} }}\n")).collect()))
            .collect();
        let refs: Vec<(&str, &str)> = files.iter().map(|(n, t)| (n.as_str(), t.as_str())).collect();
        let repo = git_repo("session-perf", &refs);
        let wt = worktree(&repo, "perf");
        let mut ed = editor("");
        ed.session.root = repo.clone();
        let open_all = |ed: &mut Editor| {
            for (name, _) in &files {
                ed.open(Path::new(name)).unwrap();
            }
        };
        open_all(&mut ed);
        let behind = ed.session_open(&wt).unwrap();
        open_all(&mut ed);
        assert_eq!(ed.docs.len(), 40, "each session's first file took its scratch's place");
        let mut runs: Vec<f64> = (0..200)
            .map(|i| {
                let t0 = std::time::Instant::now();
                ed.session_switch(if i % 2 == 0 { 0 } else { behind });
                t0.elapsed().as_secs_f64() * 1e6
            })
            .collect();
        crate::sorting::by(&mut runs, f64::total_cmp);
        let us = runs[runs.len() / 2];
        let line = format!("PERF session_switch_us {us:.3} budget=1000 noise=2\n");
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        assert!(us < 1000.0, "{us:.1} µs (budget 1 ms)");
        git_in(&repo, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Each session has its own buffers: `:bn`, `space b` and the header's tabs go through the front session's
    /// documents only; closing a session's last one leaves a blank one there, not another session's file.
    #[test]
    fn each_session_has_its_own_buffers() {
        let repo = git_repo("session-bufs", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        let wt = worktree(&repo, "bufs");
        std::fs::write(wt.join("c.txt"), "c\n").unwrap();
        let mut ed = editor("");
        ed.session.root = repo.clone();
        feed(&mut ed, ":o a.txt<ret>:o b.txt<ret>");
        let behind = ed.session_open(&wt).unwrap();
        feed(&mut ed, ":o a.txt<ret>:o c.txt<ret>");
        let shown = |ed: &Editor| -> Vec<PathBuf> {
            ed.buffers().iter().filter_map(|&i| ed.docs[i].path.clone()).collect()
        };
        let header = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 120, 20).unwrap();
            let mut parser = vt100::Parser::new(20, 120, 0);
            parser.process(&bytes);
            parser.screen().rows(0, 120).next().unwrap()
        };
        assert_eq!(shown(&ed), [wt.join("a.txt"), wt.join("c.txt")]);
        let h = header(&mut ed);
        assert!(h.contains("a.txt  c.txt") && !h.contains("b.txt"), "{h}");
        feed(&mut ed, ":bn<ret>");
        assert_eq!(
            ed.doc().path.as_deref(),
            Some(wt.join("a.txt").as_path()),
            ":bn goes round in its session"
        );
        feed(&mut ed, ":bn<ret>");
        assert_eq!(ed.doc().path.as_deref(), Some(wt.join("c.txt").as_path()));
        feed(&mut ed, "<space>b");
        let listed: Vec<String> =
            ed.picker.as_ref().unwrap().items().iter().map(|i| i.label.clone()).collect();
        assert_eq!(listed, ["a.txt", "c.txt"]);
        // The checkout tarae started in: its two
        feed(&mut ed, "<esc>:session-switch 0<ret>");
        assert_eq!(shown(&ed), [repo.join("a.txt"), repo.join("b.txt")]);
        assert!(header(&mut ed).contains("a.txt  b.txt"));
        feed(&mut ed, ":bp<ret>");
        assert_eq!(ed.doc().path.as_deref(), Some(repo.join("a.txt").as_path()));
        // Its last ones closed: a blank buffer there, the other session's files stay out of it
        ed.session_switch(behind);
        feed(&mut ed, ":bc<ret>:bc<ret>");
        assert_eq!((ed.doc().path.as_deref(), ed.buffers().len()), (None, 1), "{:?}", shown(&ed));
        assert_eq!(ed.docs.iter().filter(|d| d.path.is_some()).count(), 2, "session 0's files still open");
        ed.sessions.clear();
        git_in(&repo, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
        std::fs::remove_dir_all(&repo).ok();
    }

    /// A Claude behind asking to see the screen — its card parks in its session: that session waits for you.
    fn waits_for_you(ed: &mut Editor, id: crate::event::SessionId) {
        let msg = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "screen", "arguments": {}}});
        ed.with_session(id, |ed| {
            crate::tools::mcp(ed, 0, "c1", &msg);
        });
        assert!(session_by(ed, id).parked.iter().any(crate::offer::Offer::claudes), "parked");
    }

    /// `space W` (P3 step 7): the session list floats in a named frame with the keys, the one in front selected — what
    /// git says of each against its base, what its Claude does (`▲ waits`, what waits counted); letters are keys, not
    /// a query: `j` and Enter or a digit switch — the list staying, the keys to the code —, `esc` gives the keys back
    /// (it stays), `q` closes it, `n` asks a new one's name, `d` on the checkout tarae started in is refused.
    #[test]
    fn the_session_list_shows_and_switches() {
        let repo = git_repo("session-list", &[("a.txt", "a\n")]);
        let wt = worktree(&repo, "feat");
        std::fs::write(wt.join("a.txt"), "feat\n").unwrap();
        git_in(&wt, &["commit", "-qam", "feat"]);
        std::fs::write(wt.join("new.txt"), "new\n").unwrap();
        let other = worktree(&repo, "other");
        let mut ed = editor("");
        (ed.session.root, ed.session.name) = (repo.clone(), "proj".into());
        ed.session.git_branch = Some("main".into());
        let feat = ed.session_open(&wt).unwrap();
        ed.session.name = "feat".into();
        let two = ed.session_open(&other).unwrap();
        ed.session.name = "other".into();
        waits_for_you(&mut ed, feat);
        ed.session_switch(0);
        feed(&mut ed, "<space>W");
        let rows = |ed: &Editor| -> Vec<(String, String)> {
            ed.session_items().iter().map(|i| (i.label.clone(), i.hint.trim().to_string())).collect()
        };
        assert_eq!(ed.keys_to().region, crate::uistate::Region::List(crate::pane::GroupKind::Sessions));
        assert_eq!(ed.sessions_here(), Some(0), "the one in front in hand");
        // Drawn: a floating frame named in its top line, its keys there; no cursor (nothing to type into)
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 120, 20).unwrap();
        let mut parser = vt100::Parser::new(20, 120, 0);
        parser.process(&bytes);
        let s = parser.screen().contents();
        assert!(s.contains("╭─ sessions ── 3 sessions"), "{s}");
        assert!(s.contains("enter switch"), "{s}");
        assert!(parser.screen().hide_cursor(), "no cursor");
        // A letter that isn't one of its keys is said, the list kept
        feed(&mut ed, "t");
        let said = ed.messages.last().map(|m| m.2.clone()).unwrap_or_default();
        assert!(said.starts_with("t does nothing in the session list"), "{said}");
        assert!(ed.sessions_list_keys());
        settle_until(&mut ed, |ed| rows(ed)[1].1.contains("changed"));
        let r = rows(&ed);
        // (the checkout tarae started in by its branch, as the sessions are)
        assert_eq!(r.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), ["main", "feat", "other"]);
        assert!(r[1].1.starts_with("▲ waits  1 card"), "{r:?}");
        assert!(r[1].1.contains("↑1  1 changed"), "{r:?}");
        // (each row's name in its session's colour, what needs you in warning)
        let row = &ed.sessions_rows()[1];
        let colour = ed.sessions_in_order()[1].colour;
        assert_eq!(row.colour, colour);
        use crate::sessionpane::{Col, Ink};
        assert_eq!(row.cell(Col::State), [("▲ waits".to_string(), Ink::Warn)]);
        // `esc` gives the keys back, the list stays; `space W` gives them to it again
        feed(&mut ed, "<esc>");
        assert!(ed.sessions_pane.is_some() && ed.session.is_at(At::Code));
        feed(&mut ed, "<space>W");
        assert!(ed.sessions_list_keys());
        // `q` closes it, as every list's (the tutor teaches it)
        feed(&mut ed, "q");
        assert!(ed.sessions_pane.is_none(), "q closed it");
        feed(&mut ed, "<space>W");
        // Letters don't type: `j` moves, Enter switches — the list done once used, it closes (user, 2026-10-23), the
        // keys to the code there
        feed(&mut ed, "j");
        assert_eq!(ed.sessions_here(), Some(feat));
        feed(&mut ed, "j<ret>");
        assert_eq!((ed.session.id, ed.sessions_pane.is_some()), (two, false));
        assert!(ed.session.is_at(At::Code), "the keys to its code");
        // A digit: that row's — and the list closes with it
        feed(&mut ed, "<space>W1");
        assert_eq!((ed.session.id, ed.sessions_pane.is_some()), (0, false));
        // On the row of the session in front, Enter is the list used too
        feed(&mut ed, "<space>W<ret>");
        assert_eq!((ed.session.id, ed.sessions_pane.is_some()), (0, false));
        assert!(ed.offers.is_empty(), "feat's card waits in feat");
        // n: the new one's name
        feed(&mut ed, "<space>Wo");
        assert!(matches!(ed.prompt.as_ref().map(|p| &p.kind), Some(PromptKind::Session)));
        assert!(ed.sessions_pane.is_none(), "o: the list used");
        feed(&mut ed, "<esc><space>Wd");
        assert!(ed.messages.iter().any(|m| m.2.contains("is the checkout tarae started in — it stays")));
        ed.sessions.clear();
        for w in [&wt, &other] {
            git_in(&repo, &["worktree", "remove", "--force", w.to_str().unwrap()]);
        }
        std::fs::remove_dir_all(&repo).ok();
    }

    /// The chat shows the sessions: a tab row under its header (`●` working, `▲ waits` for you, `+` new);
    /// where it doesn't fit, one dropdown with how many others wait. A tab click switches the whole session;
    /// with no chat, the status line's chip names the session and the count, and a click on it opens the
    /// list.
    #[test]
    fn the_chat_shows_the_sessions_and_a_click_switches() {
        use crate::event::{Mouse, MouseKind};
        let repo = git_repo("session-tabs", &[("a.txt", "a\n")]);
        let (wt, other) = (worktree(&repo, "tabs-one"), worktree(&repo, "tabs-two"));
        let mut ed = editor("");
        (ed.session.root, ed.session.name) = (repo.clone(), "proj".into());
        ed.session.git_branch = Some("main".into());
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        let one = ed.session_open(&wt).unwrap();
        ed.session.name = "fix-login-everywhere".into();
        let two = ed.session_open(&other).unwrap();
        ed.session.name = "parser".into();
        ed.session_switch(0);
        waits_for_you(&mut ed, two);
        crate::chat::open(&mut ed);
        ed.session.leave(crate::session::At::Chat);
        let rows = |ed: &mut Editor, w: u16| -> Vec<String> {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, w, 24).unwrap();
            let mut parser = vt100::Parser::new(24, w, 0);
            parser.process(&bytes);
            parser.screen().rows(0, w).collect()
        };
        let r = rows(&mut ed, 200);
        // (the checkout tarae started in by its branch) — the chat's top line names them, where `claude` stood
        let tabs = r.iter().position(|r| r.contains("─ main · fix-login-everywhere · parser ▲ waits · + ─"));
        let tabs = tabs.unwrap_or_else(|| panic!("the tabs: {r:#?}"));
        assert!(tabs == 0 && !r[0].contains("claude"), "the chat's top line: {r:#?}");
        // Narrow: the states fold to what needs you, then the sessions into `+N` — names whole, the front and the one
        // waiting always in, the order kept; `+` gone while `+N` shows. How many wait is the status line's
        let r = rows(&mut ed, 100);
        assert!(r[0].contains("─ main · parser ▲ · +1 ─"), "{}", r[0]);
        assert!(r[23].contains("● normal   ▲1 waiting"), "{}", r[23]);
        // A click on a tab: that session, whole
        let r = rows(&mut ed, 200);
        let x = r[tabs].find("fix-login").map(|b| r[tabs][..b].chars().count()).unwrap();
        ed.handle_mouse(Mouse {
            kind: MouseKind::Down,
            x: x as u16 + 2,
            y: tabs as u16,
            alt: false,
            ctrl: false,
        });
        assert_eq!(ed.session.id, one);
        // No chat there: the chip, then the list from it
        let r = rows(&mut ed, 120);
        let status = r.iter().position(|r| r.contains("● normal")).expect("the status line");
        assert!(r[status].contains("● normal   fix-login-everywhere ▲1 waiting"), "{}", r[status]);
        let x = r[status].find("fix-login").map(|b| r[status][..b].chars().count()).unwrap();
        ed.handle_mouse(Mouse {
            kind: MouseKind::Down,
            x: x as u16,
            y: status as u16,
            alt: false,
            ctrl: false,
        });
        assert!(ed.sessions_list_keys(), "the list");
        ed.float_list_close(crate::pane::GroupKind::Sessions);
        // `+1`: the list, where the one folded is
        ed.session_switch(0);
        let r = rows(&mut ed, 100);
        let x = r[0].find("+1").map(|b| r[0][..b].chars().count()).unwrap();
        ed.handle_mouse(Mouse { kind: MouseKind::Down, x: x as u16, y: 0, alt: false, ctrl: false });
        assert!(ed.sessions_list_keys(), "+1: the list");
        ed.float_list_close(crate::pane::GroupKind::Sessions);
        ed.sessions.clear();
        for w in [&wt, &other] {
            git_in(&repo, &["worktree", "remove", "--force", w.to_str().unwrap()]);
        }
        std::fs::remove_dir_all(&repo).ok();
    }

    /// The git tab: changes and staged files as sections; `s` stages the file under the selection, `c` opens
    /// the message buffer, `:w` commits it (and closes it), `d` twice deletes an untracked file. The user's
    /// own hook runs as git runs it from a terminal: no `GIT_DIR`/`GIT_WORK_TREE` handed to it (`gitrepo`'s
    /// hooked form) — a stand-in that only writes what it was given into the test's own folder.
    #[test]
    fn git_tab_stage_commit_discard() {
        let repo = git_repo("gittab", &[("a.txt", "one\n")]);
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(repo.join("b.txt"), "new\n").unwrap();
        let seen = repo.with_file_name(format!("tarae-gittab-hook-{}.seen", std::process::id()));
        let hook = repo.join(".git/hooks/pre-commit");
        let script =
            format!("#!/bin/sh\necho \"${{GIT_DIR-none}} ${{GIT_WORK_TREE-none}}\" > '{}'\n", seen.display());
        std::fs::write(&hook, script).unwrap();
        let mut perms = std::fs::metadata(&hook).unwrap().permissions();
        std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
        std::fs::set_permissions(&hook, perms).unwrap();
        // (a hook in the git folder is held until the repository is trusted — `gittrust.rs`: trusted here, as
        // the card's `y` does)
        let r = crate::gitrepo::repo(&repo).unwrap();
        crate::gitrepo::answered(&r.config_file(), r.local.digest(), true);
        let mut ed = editor("");
        ed.session.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone()));
        ed.session.at = crate::session::At::Side;
        ed.git_refresh();
        settle_until(&mut ed, |ed| ed.session.git_status.is_some());
        let rows = |ed: &mut Editor| -> Vec<String> {
            ed.sidebar_sync();
            let s = ed.session.sidebar.as_ref().unwrap();
            s.lines().iter().map(|l| format!("{}{} {}", "  ".repeat(l.depth), l.label, l.hint)).collect()
        };
        assert_eq!(rows(&mut ed), ["Changes 2", "  a.txt +1", "  b.txt +1"]);
        ed.session.sidebar.as_mut().unwrap().selected = 1;
        feed(&mut ed, "s");
        settle_until(&mut ed, |ed| ed.session.git_status.as_ref().is_some_and(|s| !s.staged.is_empty()));
        assert_eq!(rows(&mut ed), ["Changes 1", "  b.txt +1", "Staged 1", "  a.txt +1"]);
        feed(&mut ed, "c");
        settle_until(&mut ed, |ed| ed.session.committing.is_some());
        assert_eq!(ed.doc().display_name_short(), "COMMIT_EDITMSG");
        feed(&mut ed, "add two<esc>:w<ret>");
        // (a status read meanwhile can show it committed before the commit's own result closes the buffer)
        settle_until(&mut ed, |ed| {
            ed.session.committing.is_none()
                && ed.session.git_status.as_ref().is_some_and(|s| s.staged.is_empty())
                && ed.docs.iter().all(|d| d.display_name_short() != "COMMIT_EDITMSG")
        });
        let log = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["log", "-1", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "add two");
        assert!(ed.docs.iter().all(|d| d.display_name_short() != "COMMIT_EDITMSG"), "its buffer closed");
        assert_eq!(
            std::fs::read_to_string(&seen).unwrap(),
            "none none\n",
            "the hook ran, given no repository"
        );
        std::fs::remove_file(&seen).ok();
        // b.txt: untracked — the first d asks, the second deletes
        ed.session.at = crate::session::At::Side;
        ed.session.sidebar.as_mut().unwrap().selected = 1;
        feed(&mut ed, "d");
        assert!(repo.join("b.txt").exists());
        feed(&mut ed, "d");
        settle_until(&mut ed, |_| !repo.join("b.txt").exists());
        std::fs::remove_dir_all(&repo).ok();
    }

    /// #187: a section's header row is reachable — `k` from the rows under it and `gg` both land on
    /// "Changes", drawn as the panel draws it — and `s` there stages the whole section in one go (one `s` per
    /// folder before). The keys win over `space g`'s aim at the code's row: until that file's hunks settled,
    /// the aim put the selection back every event, so `k` never got past the file's folder.
    #[test]
    fn git_tab_section_header_is_reachable_and_stages_the_section() {
        let repo = git_repo("gittab-section", &[("README", "r\n")]);
        // (more rows than the panel holds: it scrolls, and pins the rows over the top)
        let files: Vec<String> =
            (0..12).flat_map(|d| [format!("d{d:02}/a.rs"), format!("d{d:02}/b.rs")]).collect();
        for f in &files {
            std::fs::create_dir_all(repo.join(f).parent().unwrap()).unwrap();
            std::fs::write(repo.join(f), "one\n").unwrap();
        }
        git_in(&repo, &["add", "-A"]);
        git_in(&repo, &["commit", "-qm", "folders"]);
        // (each file's change below the line the cursor is on)
        for f in &files {
            std::fs::write(repo.join(f), "one\ntwo\n").unwrap();
        }
        let mut ed = editor("");
        ed.session.root = repo.clone();
        // (the gutter's hunks on, as outside tests — the tab opens a file's hunks from them)
        ed.git_auto = true;
        // Keys as the main loop takes them — each with its event's work after it (`sidebar_sync` and the rest)
        let press = |ed: &mut Editor, c: char| {
            ed.handle_event(crate::event::Event::Key(Key::plain(crate::key::Code::Char(c))));
        };
        let space_g = |ed: &mut Editor| {
            press(ed, ' ');
            press(ed, 'g');
        };
        let settled = |ed: &Editor| {
            let d = ed.doc();
            ed.session.git_status.is_some() && d.git_base.is_some() && d.git_version == d.version()
        };
        // (opened as it's used: `space g` from a changed file in the code — its hunks read)
        feed(&mut ed, ":o d05/a.rs<ret>");
        space_g(&mut ed);
        settle_until(&mut ed, settled);
        let draw = |ed: &mut Editor| {
            let mut out = Vec::new();
            crate::term::render(ed, &mut out, 120, 30).unwrap();
        };
        let on = |ed: &mut Editor| -> String {
            let s = ed.session.sidebar.as_ref().unwrap();
            s.lines()[s.selected].label.clone()
        };
        // Back in the code, an edit — its hunks not read again yet — and `space g` once more: `k` moves off the
        // file's row and stays off it (no event between gives the hunks time to settle)
        ed.handle_event(crate::event::Event::Key(Key::plain(crate::key::Code::Esc)));
        feed(&mut ed, "ix<esc>");
        assert!(!settled(&ed), "its hunks behind the edit");
        space_g(&mut ed);
        let file = ed.session.sidebar.as_ref().unwrap().selected;
        assert_eq!(on(&mut ed), "a.rs", "on the code's file");
        press(&mut ed, 'k');
        draw(&mut ed);
        press(&mut ed, 'k');
        draw(&mut ed);
        assert_eq!(ed.session.sidebar.as_ref().unwrap().selected, file - 2, "two rows up, not put back");
        settle_until(&mut ed, settled);
        let labels: Vec<String> =
            ed.session.sidebar.as_ref().unwrap().lines().iter().map(|l| l.label.clone()).collect();
        assert_eq!(labels[0], "Changes", "{labels:?}");
        // k all the way up reaches the header
        let last = labels.len() - 1;
        for _ in 0..last {
            press(&mut ed, 'k');
            draw(&mut ed);
        }
        assert_eq!(on(&mut ed), "Changes", "k reaches the header");
        // gg from below: the header too
        ed.session.sidebar.as_mut().unwrap().selected = last;
        draw(&mut ed);
        press(&mut ed, 'g');
        press(&mut ed, 'g');
        draw(&mut ed);
        assert_eq!(on(&mut ed), "Changes", "gg reaches the header");
        // s on it: the whole section staged at once
        press(&mut ed, 's');
        settle_until(&mut ed, |ed| ed.session.git_status.as_ref().is_some_and(|s| s.unstaged.is_empty()));
        assert_eq!(ed.session.git_status.as_ref().unwrap().staged.len(), files.len());
        std::fs::remove_dir_all(&repo).ok();
    }

    /// git's output in `dir` (trimmed) — the test fails if git does.
    fn git_out(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Whether git succeeds in `dir`.
    fn git_ok(dir: &Path, args: &[&str]) -> bool {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        out.status.success()
    }

    /// `file` written and committed in `dir` as `msg`.
    fn commit_in(dir: &Path, file: &str, text: &str, msg: &str) {
        std::fs::write(dir.join(file), text).unwrap();
        git_in(dir, &["add", file]);
        git_in(
            dir,
            &[
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                msg,
            ],
        );
    }

    /// A remote of the test's own — never the network: a bare repository in its temp folder, made
    /// `work`'s `origin` (nothing pushed yet). (the bare repository, the folder holding it)
    fn bare_remote(name: &str, work: &Path) -> (PathBuf, PathBuf) {
        let base = crate::scratch::Scratch::new(&format!("remote-{name}")).into_path();
        git_in(&base, &["init", "-q", "--bare", "-b", "main", "origin.git"]);
        let bare = base.join("origin.git");
        git_in(work, &["remote", "add", "origin", bare.to_str().unwrap()]);
        (bare, base)
    }

    /// Push, pull and fetch in the git tab as they are, against a remote of the test's own: the first `P`
    /// sets the upstream, the next pushes what's ahead; `p` takes in what someone else pushed (fast-forward)
    /// and says when there's nothing; `f` fetches and prunes. Over diverged history a push is refused — the
    /// force card asks, nothing forced unasked (`amend_then_push_over_the_remote_with_a_lease`) — and a pull
    /// fails as git does, said — nothing merged. A remote that isn't there fails at once, said.
    #[test]
    fn git_tab_pushes_pulls_and_fetches_with_a_remote_of_its_own() {
        let work = git_repo("remote", &[("a.txt", "one\n")]);
        let (bare, base) = bare_remote("remote", &work);
        let said = |ed: &Editor, kind: ToastKind, what: &str| {
            ed.messages.iter().filter(|m| m.1 == kind && m.2.contains(what)).count()
        };
        let status = |ed: &Editor| ed.session.git_status.clone().unwrap_or_default();
        let mut ed = editor("");
        ed.session.root = work.clone();
        feed(&mut ed, "<space>g");
        settle_until(&mut ed, |ed| ed.session.git_status.is_some());
        assert_eq!(status(&ed).upstream, None, "nothing pushed yet");
        // The first push sets the upstream
        feed(&mut ed, "P");
        settle_until(&mut ed, |ed| {
            said(ed, ToastKind::Success, "Pushed main") == 1 && status(ed).upstream.is_some()
        });
        assert_eq!(git_out(&bare, &["rev-parse", "main"]), git_out(&work, &["rev-parse", "HEAD"]));
        assert_eq!(status(&ed).upstream.as_deref(), Some("origin/main"));
        // One of ours ahead: pushed
        commit_in(&work, "a.txt", "one\ntwo\n", "two");
        ed.git_refresh();
        settle_until(&mut ed, |ed| status(ed).ahead == 1);
        feed(&mut ed, "P");
        settle_until(&mut ed, |ed| said(ed, ToastKind::Success, "Pushed main") == 2 && status(ed).ahead == 0);
        assert_eq!(git_out(&bare, &["rev-parse", "main"]), git_out(&work, &["rev-parse", "HEAD"]));
        // Someone else pushes: `p` takes it in; again, nothing to take
        git_in(&base, &["clone", "-q", bare.to_str().unwrap(), "other"]);
        let other = base.join("other");
        commit_in(&other, "b.txt", "theirs\n", "theirs");
        git_in(&other, &["push", "-q"]);
        feed(&mut ed, "p");
        settle_until(&mut ed, |ed| said(ed, ToastKind::Success, "Pulled into main") == 1);
        assert_eq!(git_out(&work, &["rev-parse", "HEAD"]), git_out(&other, &["rev-parse", "HEAD"]));
        feed(&mut ed, "p");
        settle_until(&mut ed, |ed| said(ed, ToastKind::Info, "Already up to date") == 1);
        // `f`: a branch pushed elsewhere arrives; deleted there, it goes (pruned)
        git_in(&other, &["push", "-q", "origin", "HEAD:feature"]);
        feed(&mut ed, "f");
        settle_until(&mut ed, |ed| said(ed, ToastKind::Success, "Fetched") == 1);
        assert!(git_ok(&work, &["rev-parse", "--verify", "-q", "refs/remotes/origin/feature"]), "fetched");
        git_in(&other, &["push", "-q", "origin", "--delete", "feature"]);
        feed(&mut ed, "f");
        settle_until(&mut ed, |ed| said(ed, ToastKind::Success, "Fetched") == 2);
        assert!(!git_ok(&work, &["rev-parse", "--verify", "-q", "refs/remotes/origin/feature"]), "pruned");
        // Diverged — theirs pushed, ours not: the push is refused, nothing forced; the pull won't merge
        commit_in(&other, "b.txt", "theirs\nagain\n", "theirs again");
        git_in(&other, &["push", "-q"]);
        commit_in(&work, "a.txt", "one\ntwo\nours\n", "ours");
        let (theirs, ours) =
            (git_out(&other, &["rev-parse", "HEAD"]), git_out(&work, &["rev-parse", "HEAD"]));
        feed(&mut ed, "P");
        settle_until(&mut ed, |ed| {
            ed.offers.iter().any(|o| matches!(o.what, crate::offer::What::ForcePush(_)))
        });
        assert_eq!(git_out(&bare, &["rev-parse", "main"]), theirs, "the remote kept theirs");
        ed.offer_dismiss();
        feed(&mut ed, "p");
        settle_until(&mut ed, |ed| said(ed, ToastKind::Error, "pull: ") == 1);
        assert_eq!(git_out(&work, &["rev-parse", "HEAD"]), ours, "ours as it was — no merge");
        assert_eq!(said(&ed, ToastKind::Error, "pull: hint:"), 0, "why, not git's hint: {:?}", ed.messages);
        // A remote that isn't there: said at once
        git_in(&work, &["remote", "set-url", "origin", base.join("missing.git").to_str().unwrap()]);
        feed(&mut ed, "f");
        settle_until(&mut ed, |ed| said(ed, ToastKind::Error, "fetch: ") == 1);
        drop(ed);
        std::fs::remove_dir_all(&work).ok();
        std::fs::remove_dir_all(&base).ok();
    }

    /// Messages of `kind` containing `what`, since the editor started.
    fn said(ed: &Editor, kind: ToastKind, what: &str) -> usize {
        ed.messages.iter().filter(|m| m.1 == kind && m.2.contains(what)).count()
    }

    /// Branches in the picker (`:git-switch`, `B` in the git tab): a name no branch has is a `+` row that
    /// makes it and switches there; `C-r` renames it (a `:` line holding the name to change); `C-x` deletes
    /// one whose commits are kept elsewhere at once, and one with commits only on it after a card naming
    /// them — `n` keeps it, `y` deletes it, unless it moved since the card. A branch checked out in another
    /// worktree can't be switched to, renamed or deleted (said, naming where); nor can the one you're on be
    /// deleted, nor a bad name be made.
    #[test]
    fn branches_are_made_renamed_and_deleted_in_the_picker() {
        use crate::offer::What;
        let repo = git_repo("branch-picker", &[("a.txt", "one\n")]);
        let wt = repo.with_file_name(format!("tarae-wt-branches-{}", std::process::id()));
        std::fs::remove_dir_all(&wt).ok();
        let head = || git_out(&repo, &["symbolic-ref", "--short", "HEAD"]);
        let exists = |b: &str| git_ok(&repo, &["rev-parse", "--verify", "-q", &format!("refs/heads/{b}")]);
        let deleting =
            |ed: &Editor| ed.offers.iter().filter(|o| matches!(o.what, What::BranchDelete { .. })).count();
        let open = |ed: &mut Editor| {
            feed(ed, ":git-switch<ret>");
            settle_until(ed, |ed| ed.picker.as_ref().is_some_and(|p| !p.loading));
        };
        let current = |ed: &Editor| {
            let i = ed.picker.as_ref().and_then(|p| p.current()).unwrap();
            (i.label.clone(), i.hint.clone())
        };
        let mut ed = editor("");
        ed.session.root = repo.clone();
        // A name no branch has: the `+` row — Enter makes it and switches there
        open(&mut ed);
        feed(&mut ed, "fix-login");
        assert_eq!(current(&ed), ("fix-login".into(), "new branch".into()));
        feed(&mut ed, "<ret>");
        settle_until(&mut ed, |ed| said(ed, ToastKind::Success, "On fix-login") == 1);
        assert_eq!(head(), "fix-login");
        commit_in(&repo, "b.txt", "b\n", "only on fix");
        // An existing name has no `+` row
        open(&mut ed);
        feed(&mut ed, "main");
        assert_eq!(current(&ed).0, "main");
        assert_eq!(ed.picker.as_ref().unwrap().len(), 1, "no + row for a name that's there");
        feed(&mut ed, "<ret>");
        settle_until(&mut ed, |ed| said(ed, ToastKind::Success, "On main") == 1);
        // C-r: the `:` line with the name to change
        open(&mut ed);
        feed(&mut ed, "fix-lo");
        assert_eq!(current(&ed).0, "fix-login");
        feed(&mut ed, "<C-r>");
        assert!(ed.picker.is_none());
        assert_eq!(
            ed.prompt.as_ref().map(|p| p.text.as_str()),
            Some("git-branch-rename fix-login fix-login")
        );
        feed(&mut ed, "<backspace><backspace><backspace><backspace><backspace>auth<ret>");
        settle_until(&mut ed, |ed| said(ed, ToastKind::Success, "Renamed fix-login → fix-auth") == 1);
        assert!(exists("fix-auth") && !exists("fix-login"));
        // C-x on one with a commit only on it: a card naming it; n keeps it
        open(&mut ed);
        feed(&mut ed, "fix-auth<C-x>");
        settle_until(&mut ed, |ed| deleting(ed) == 1);
        let q = ed.offers[0].question.clone();
        assert!(q.starts_with("This commit is on no other branch") && q.contains("only on fix\n"), "{q}");
        assert!(q.contains("Gone with it — git's reflog keeps it a while"), "{q}");
        ed.offer_dismiss();
        assert!(exists("fix-auth"), "n keeps it");
        // y deletes it
        feed(&mut ed, ":git-branch-delete fix-auth<ret>");
        settle_until(&mut ed, |ed| deleting(ed) == 1);
        ed.offer_accept();
        settle_until(&mut ed, |ed| said(ed, ToastKind::Success, "Deleted fix-auth") == 1);
        assert!(!exists("fix-auth"));
        // Moved since its card: not deleted
        git_in(&repo, &["branch", "moving"]);
        commit_in(&repo, "c.txt", "c\n", "on main");
        git_in(&repo, &["switch", "-q", "moving"]);
        commit_in(&repo, "d.txt", "d\n", "only on moving");
        git_in(&repo, &["switch", "-q", "main"]);
        feed(&mut ed, ":git-branch-delete moving<ret>");
        settle_until(&mut ed, |ed| deleting(ed) == 1);
        git_in(&repo, &["branch", "-f", "moving", "main"]);
        ed.offer_accept();
        settle_until(&mut ed, |ed| said(ed, ToastKind::Error, "moving moved since the card") == 1);
        assert!(exists("moving"));
        // Its commits elsewhere (now at main): deleted at once, no card
        feed(&mut ed, ":git-branch-delete moving<ret>");
        settle_until(&mut ed, |ed| said(ed, ToastKind::Success, "Deleted moving (was") == 1);
        assert!(!exists("moving") && deleting(&ed) == 0);
        // Checked out in another worktree: listed with where, and refused — naming it
        git_in(&repo, &["worktree", "add", "-q", "-b", "held", wt.to_str().unwrap()]);
        open(&mut ed);
        feed(&mut ed, "held");
        let (label, hint) = current(&ed);
        let wt_name = wt.file_name().unwrap().to_str().unwrap();
        // (named from beside the repository: the worktree is its sibling)
        assert!(label == "held" && hint.starts_with(&format!("in {wt_name} · ")), "{hint}");
        // Enter's hint there says where it's checked out, not "switch"
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 160, 30).unwrap();
        let mut parser = vt100::Parser::new(30, 160, 0);
        parser.process(&bytes);
        let s = parser.screen().contents();
        assert!(s.contains(&format!("enter checked out in {wt_name}")) && !s.contains("enter switch"), "{s}");
        feed(&mut ed, "<backspace><backspace><backspace><backspace>main");
        assert_eq!(current(&ed).0, "main");
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 160, 30).unwrap();
        let mut parser = vt100::Parser::new(30, 160, 0);
        parser.process(&bytes);
        assert!(parser.screen().contents().contains("enter switch"), "a branch of this worktree: switch");
        feed(&mut ed, "<backspace><backspace><backspace><backspace>held");
        assert_eq!(current(&ed).0, "held");
        feed(&mut ed, "<ret>");
        settle_until(&mut ed, |ed| {
            said(ed, ToastKind::Error, "can't switch to held — it's checked out in") == 1
        });
        assert_eq!(head(), "main");
        assert!(ed.messages.iter().any(|m| m.2.contains(wt_name)), "names where");
        feed(&mut ed, ":git-branch-delete held<ret>");
        settle_until(&mut ed, |ed| said(ed, ToastKind::Error, "can't delete held") == 1);
        feed(&mut ed, ":git-branch-rename held other<ret>");
        settle_until(&mut ed, |ed| said(ed, ToastKind::Error, "can't rename held") == 1);
        assert!(exists("held") && !exists("other"));
        // The one you're on; a bad name
        feed(&mut ed, ":git-branch-delete main<ret>");
        settle_until(&mut ed, |ed| said(ed, ToastKind::Error, "you're on main") == 1);
        feed(&mut ed, ":git-branch-new bad..name<ret>");
        settle_until(&mut ed, |ed| said(ed, ToastKind::Error, "isn't a branch name") == 1);
        assert!(exists("main") && !exists("bad..name") && head() == "main");
        drop(ed);
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&wt).ok();
    }

    /// `:git-amend` opens the last commit's message as `A` does — warned when that commit was pushed — and
    /// `:w` puts what's staged into it, its message kept. The branch has then gone a different way from its upstream: `P` shows a card
    /// listing the remote's commit a push would replace. Someone pushing between the card and `y` makes the
    /// lease refuse even after a fetch brought theirs in — theirs kept, said; asked again, `y` replaces what
    /// that card listed. A push refused because the remote moved on unseen brings the card too; one the
    /// remote's hook refuses says why — not git's last line.
    #[test]
    fn amend_then_push_over_the_remote_with_a_lease() {
        use crate::offer::What;
        let work = git_repo("amend-push", &[("a.txt", "one\n")]);
        let (bare, base) = bare_remote("amend-push", &work);
        git_in(&work, &["push", "-q", "-u", "origin", "main"]);
        let first = git_out(&work, &["rev-parse", "HEAD"]);
        let status = |ed: &Editor| ed.session.git_status.clone().unwrap_or_default();
        let forcing = |ed: &Editor| ed.offers.iter().filter(|o| matches!(o.what, What::ForcePush(_))).count();
        let mut ed = editor("");
        ed.session.root = work.clone();
        feed(&mut ed, "<space>g");
        settle_until(&mut ed, |ed| ed.session.git_status.is_some());
        // `:git-amend` is `A` from anywhere — the git tab's status not read yet too: the last message in a
        // buffer, saying what `:w` does, nothing amended before it — and that commit was pushed: warned
        std::fs::write(work.join("a.txt"), "one\ntwo\n").unwrap();
        git_in(&work, &["add", "a.txt"]);
        ed.session.git_status = None;
        crate::typed::execute(&mut ed, "git-amend").unwrap();
        settle_until(&mut ed, |ed| {
            ed.session.committing.as_ref().is_some_and(|c| c.amend)
                && said(ed, ToastKind::Warning, crate::gitpanel::PUSHED_WARNING) == 1
        });
        assert_eq!(ed.doc().display_name_short(), "COMMIT_EDITMSG");
        assert!(text(&ed).starts_with("base\n") && text(&ed).contains("# :w amends the last commit"));
        assert_eq!(git_out(&work, &["rev-parse", "HEAD"]), first, "nothing amended before :w");
        // `:w`: what's staged into the last commit, its message kept
        feed(&mut ed, ":w<ret>");
        settle_until(&mut ed, |ed| said(ed, ToastKind::Success, "Amended") == 1);
        assert_ne!(git_out(&work, &["rev-parse", "HEAD"]), first);
        assert_eq!(git_out(&work, &["log", "-1", "--format=%s"]), "base");
        assert_eq!(git_out(&work, &["show", "HEAD:a.txt"]), "one\ntwo");
        assert_eq!(git_out(&work, &["rev-list", "--count", "HEAD"]), "1", "amended, not added");
        ed.git_refresh();
        settle_until(&mut ed, |ed| status(ed).ahead == 1 && status(ed).behind == 1);
        // P: the card, listing the remote's commit that would go
        ed.session.at = crate::session::At::Side;
        feed(&mut ed, "P");
        settle_until(&mut ed, |ed| forcing(ed) == 1);
        let q = ed.offers[0].question.clone();
        assert_eq!(ed.offers[0].title, "Push over origin/main?", "nothing of anyone else's goes");
        assert!(
            q.starts_with(&format!(
                "Pushing main replaces 1 commit on origin/main.\nAn earlier version of yours:\n  {} base\n",
                &first[..7]
            )),
            "the amended one: {q}"
        );
        assert!(ed.offers[0].warning.is_none(), "nothing of anyone else's goes");
        assert_eq!(git_out(&bare, &["rev-parse", "main"]), first, "nothing pushed before y");
        // A colleague pushes two meanwhile: y's lease refuses, theirs kept
        git_in(&base, &["clone", "-q", bare.to_str().unwrap(), "other"]);
        let other = base.join("other");
        let colleague = |file: &str, text: &str, msg: &str| {
            std::fs::write(other.join(file), text).unwrap();
            git_in(&other, &["add", file]);
            let who = ["-c", "user.email=kim@t", "-c", "user.name=Kim", "-c", "commit.gpgsign=false"];
            git_in(&other, &[&who[..], &["commit", "-qm", msg]].concat());
        };
        colleague("CHANGELOG.md", "0.4.1\n", "Bump the version to 0.4.1");
        let bump = git_out(&other, &["rev-parse", "HEAD"]);
        colleague("CHANGELOG.md", "0.4.1\nfix\n", "Fix the crash on an empty config");
        git_in(&other, &["push", "-q"]);
        let theirs = git_out(&other, &["rev-parse", "HEAD"]);
        // …and `f` fetches it before the y: a bare `--force-with-lease` would take theirs as the lease now
        ed.git_fetch();
        settle_until(&mut ed, |ed| said(ed, ToastKind::Success, "Fetched") == 1);
        ed.offer_accept();
        settle_until(&mut ed, |ed| {
            said(ed, ToastKind::Error, "origin/main moved since the card was shown") == 1
        });
        assert_eq!(git_out(&bare, &["rev-parse", "main"]), theirs, "the lease kept theirs");
        // Asked again: the colleague's two are named as not yours — first, warned, counted in the title — then
        // ours as it was before the amend; y replaces exactly what it listed
        ed.session.at = crate::session::At::Side;
        feed(&mut ed, "P");
        settle_until(&mut ed, |ed| forcing(ed) == 1);
        let q = ed.offers[0].question.clone();
        assert_eq!(ed.offers[0].title, "Push over 2 commits that aren't yours?");
        let expect = format!(
            "Pushing main replaces 3 commits on origin/main.\nNot in yours — lost if you push:\n  {} Fix the crash \
             on an empty config\n  {} Bump the version to 0.4.1\nAn earlier version of yours:\n  {} base\n",
            &theirs[..7],
            &bump[..7],
            &first[..7]
        );
        assert!(q.starts_with(&expect), "{q}");
        assert_eq!(
            ed.offers[0].warning,
            Some(1..4),
            "the heading and the colleague's two, in the warning color"
        );
        assert_eq!(ed.offers[0].decided_lines(), 1..6, "both groups are what's decided");
        ed.offer_accept();
        settle_until(&mut ed, |ed| said(ed, ToastKind::Success, "Pushed main over origin/main") == 1);
        assert_eq!(git_out(&bare, &["rev-parse", "main"]), git_out(&work, &["rev-parse", "HEAD"]));
        // `A` on a pushed commit warns too (`:q` cancels)
        ed.git_commit_start(true);
        settle_until(&mut ed, |ed| {
            ed.session.committing.is_some()
                && said(ed, ToastKind::Warning, crate::gitpanel::PUSHED_WARNING) == 2
        });
        feed(&mut ed, ":q<ret>");
        assert!(ed.session.committing.is_none());
        // The remote moves on unseen and ours goes ahead: the push is refused — said why, the card comes. One
        // of the remote's two we picked across and reworded: its change is ours (git cherry), its subject not
        git_in(&other, &["fetch", "-q"]);
        git_in(&other, &["reset", "-q", "--hard", "origin/main"]);
        colleague("c.txt", "c\n", "Shared fix");
        let shared = git_out(&other, &["rev-parse", "HEAD"]);
        colleague("d.txt", "d\n", "theirs later");
        git_in(&other, &["push", "-q"]);
        let later = git_out(&other, &["rev-parse", "HEAD"]);
        // (fetched by url: the remote-tracking branch stays as it was — the push is what finds out)
        git_in(&work, &["fetch", "-q", bare.to_str().unwrap(), "main"]);
        git_in(&work, &["cherry-pick", &shared]);
        git_in(&work, &["commit", "-q", "--amend", "-m", "Pick up the shared fix"]);
        commit_in(&work, "a.txt", "one\ntwo\nthree\n", "ours later");
        ed.git_refresh();
        settle_until(&mut ed, |ed| status(ed).ahead == 2 && status(ed).behind == 0);
        ed.session.at = crate::session::At::Side;
        feed(&mut ed, "P");
        settle_until(&mut ed, |ed| forcing(ed) == 1);
        let q = ed.offers[0].question.clone();
        assert_eq!(ed.offers[0].title, "Push over a commit that isn't yours?");
        let expect = format!(
            "Not in yours — lost if you push:\n  {} theirs later\nAn earlier version of yours:\n  {} Shared fix\n\n",
            &later[..7],
            &shared[..7]
        );
        assert!(q.contains(&expect), "{q}");
        assert!(ed.messages.iter().all(|m| !m.2.contains("hint:")), "{:?}", ed.messages);
        ed.offer_dismiss();
        assert_eq!(git_out(&bare, &["rev-parse", "main"]), later, "n: theirs as it was");
        // A push the remote refuses (its hook): said why — git's own last line is `error: failed to push`
        let hook = bare.join("hooks/pre-receive");
        std::fs::write(&hook, "#!/bin/sh\necho 'main is protected' >&2\nexit 1\n").unwrap();
        std::fs::set_permissions(&hook, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        git_in(&work, &["reset", "-q", "--hard", "origin/main"]);
        commit_in(&work, "a.txt", "again\n", "ours again");
        ed.git_refresh();
        settle_until(&mut ed, |ed| status(ed).ahead == 1 && status(ed).behind == 0);
        let pushes = said(&ed, ToastKind::Error, "push: ");
        feed(&mut ed, "P");
        settle_until(&mut ed, |ed| said(ed, ToastKind::Error, "push: ") == pushes + 1);
        assert_eq!(
            said(&ed, ToastKind::Error, "push: refused by the remote — main is protected"),
            1,
            "{:?}",
            ed.messages
        );
        assert_eq!(forcing(&ed), 0, "not a card: forcing wouldn't get past a hook");
        drop(ed);
        std::fs::remove_dir_all(&work).ok();
        std::fs::remove_dir_all(&base).ok();
    }

    /// The root is taken as its real path: a link to the project gives the project's own folder (the sandbox
    /// and `run_cwd` compare real paths — a worktree reached through a link would be refused as outside).
    #[test]
    fn the_root_is_its_real_path() {
        let scratch = crate::scratch::Scratch::new("root-real");
        let dir = scratch.to_path_buf();
        std::fs::create_dir_all(dir.join("project")).unwrap();
        std::os::unix::fs::symlink(dir.join("project"), dir.join("link")).unwrap();
        let ed = Editor::with_root(Config::default(), dir.join("link"));
        assert_eq!(ed.root(), std::fs::canonicalize(dir.join("project")).unwrap());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Sizes you set (U1): `C-w >`/`<` on the chat — from the chat's empty input too (with text there, `C-w`
    /// deletes a word) — and on the side panel when it has the keys; `C-w +`/`-` on the bottom slot, which
    /// then wins over its panel's own height; kept in a file (global), shown clamped on a smaller screen
    /// without changing what's kept; `C-w =` makes all automatic again.
    #[test]
    fn sizes_you_set_are_kept_and_clamped() {
        let scratch = crate::scratch::Scratch::new("sizes-keys");
        let dir = scratch.to_path_buf();
        let mut ed = editor("fn main() {}\n");
        ed.sizes_path = Some(dir.join("sizes.json"));
        // (the chat's process: a stand-in that reads and says nothing)
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        let draw = |ed: &mut Editor, w: u16| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, w, 40).unwrap();
        };
        draw(&mut ed, 140);
        // The chat: automatic 56 at 140; C-w > from its empty input widens it
        crate::chat::open(&mut ed);
        draw(&mut ed, 140);
        assert_eq!(ed.screen.chat_w, 56);
        assert!(ed.session.is_at(crate::session::At::Chat));
        feed(&mut ed, "<C-w>>");
        draw(&mut ed, 140);
        assert_eq!((ed.sizes.chat, ed.screen.chat_w), (Some(60), 60));
        assert!(ed.session.chat.as_ref().unwrap().input.is_empty(), "nothing typed into the chat");
        // With text in the input, C-w deletes a word as before
        feed(&mut ed, "two words<C-w>");
        assert_eq!(ed.session.chat.as_ref().unwrap().input, "two ");
        feed(&mut ed, "<C-u><esc>");
        // From the code: the chat
        feed(&mut ed, "<C-w><lt><C-w><lt>");
        draw(&mut ed, 140);
        assert_eq!(ed.screen.chat_w, 52);
        // Said in one toast that changes, not a stack of them
        let said: Vec<&str> =
            ed.toasts.iter().filter(|t| t.text.starts_with("chat ")).map(|t| t.text.as_str()).collect();
        assert_eq!(said, ["chat 52 columns · C-w = automatic"]);
        // Kept in the file
        let kept = || crate::sizes::Sizes::read(Some(&dir.join("sizes.json")));
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while kept().chat != Some(52) && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        assert_eq!(kept().chat, Some(52), "written");
        // A smaller screen shows less of it; the kept size stays, a bigger screen has it again
        ed.sizes.chat = Some(90);
        draw(&mut ed, 120);
        assert_eq!((ed.screen.chat_w, ed.sizes.chat), (80, Some(90)), "clamped, not overwritten");
        draw(&mut ed, 200);
        assert_eq!(ed.screen.chat_w, 90);
        // The side panel, with the keys: its own width
        crate::chat::close(&mut ed);
        ed.session.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Files, dir.clone()));
        ed.session.at = crate::session::At::Side;
        ed.session.at = crate::session::At::Side;
        draw(&mut ed, 140);
        let side = ed.screen.side_w;
        feed(&mut ed, "<C-w>>");
        draw(&mut ed, 140);
        assert_eq!(ed.screen.side_w, side + crate::sizes::STEP_COLS);
        // The bottom slot: yours wins over the task panel's own height
        let mut run = crate::task::Run::for_test(crate::task::Task {
            name: "t".into(),
            cmd: "true".into(),
            cwd: dir.clone(),
        });
        run.output = [(false, "one")].into_iter().collect();
        ed.session.task_run = Some(run);
        ed.session.leave(crate::session::At::Side);
        draw(&mut ed, 140);
        assert_eq!(ed.screen.bottom_h, 3, "its own: a small one (its header in the line over it, two rows)");
        feed(&mut ed, "<C-w>+<C-w>+");
        draw(&mut ed, 140);
        assert_eq!((ed.sizes.bottom, ed.screen.bottom_h), (Some(7), 7));
        // C-w =: all automatic again
        feed(&mut ed, "<C-w>=");
        draw(&mut ed, 140);
        assert_eq!(ed.sizes, crate::sizes::Sizes::default());
        assert_eq!((ed.screen.bottom_h, ed.screen.side_w), (3, side));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The borders drag (U1): the chat's left edge, the side panel's edge, the bottom panel's top row follow
    /// the mouse — within what leaves the code its room — and what's set when it's let go is kept; a press
    /// elsewhere isn't a drag.
    #[test]
    fn borders_drag_and_the_size_is_kept() {
        use crate::event::{Mouse, MouseKind as K};
        let scratch = crate::scratch::Scratch::new("sizes-drag");
        let dir = scratch.to_path_buf();
        let mut ed = editor("fn main() {}\n");
        ed.sizes_path = Some(dir.join("sizes.json"));
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        let draw = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 40).unwrap();
        };
        let mouse = |ed: &mut Editor, kind, x: u16, y: u16| {
            ed.handle_mouse(Mouse { kind, x, y, alt: false, ctrl: false })
        };
        crate::chat::open(&mut ed);
        ed.session.leave(crate::session::At::Chat);
        draw(&mut ed);
        // The chat's edge: at column 140 - 56; dragged 10 to the left, 10 wider
        let edge = (140 - ed.screen.chat_w) as u16;
        assert_eq!(ed.border_at(edge as usize, 10), Some(crate::sizes::Border::Chat));
        assert_eq!(ed.border_at(edge as usize - 1, 10), None, "beside it: not the border");
        mouse(&mut ed, K::Down, edge, 10);
        mouse(&mut ed, K::Drag, edge - 10, 12);
        draw(&mut ed);
        assert_eq!(ed.screen.chat_w, 66, "follows the mouse");
        // Past what leaves the code its room: held there
        mouse(&mut ed, K::Drag, 2, 12);
        draw(&mut ed);
        assert_eq!(ed.screen.chat_w, 140 - crate::sizes::CHAT_FULL_CODE);
        mouse(&mut ed, K::Drag, edge - 10, 12);
        assert!(ed.toasts.is_empty(), "nothing said while it moves");
        mouse(&mut ed, K::Up, edge - 10, 12);
        assert!(ed.dragging.is_none());
        assert!(ed.toasts.iter().any(|t| t.text == "chat 66 columns · C-w = automatic"), "said when let go");
        let kept = || crate::sizes::Sizes::read(Some(&dir.join("sizes.json")));
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while kept().chat != Some(66) && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        assert_eq!(kept().chat, Some(66), "kept when let go");
        // The side panel's edge (its last column)
        crate::chat::close(&mut ed);
        ed.session.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Files, dir.clone()));
        ed.session.at = crate::session::At::Side;
        draw(&mut ed);
        let side = ed.screen.side_w as u16;
        mouse(&mut ed, K::Down, side - 1, 5);
        mouse(&mut ed, K::Drag, side + 7, 5);
        mouse(&mut ed, K::Up, side + 7, 5);
        draw(&mut ed);
        assert_eq!(ed.screen.side_w, side as usize + 8);
        // The bottom panel's top row, dragged up 5 rows: 5 taller
        let mut run = crate::task::Run::for_test(crate::task::Task {
            name: "t".into(),
            cmd: "true".into(),
            cwd: dir.clone(),
        });
        run.output = [(false, "one")].into_iter().collect();
        ed.session.task_run = Some(run);
        draw(&mut ed);
        let (h, top) = (ed.screen.bottom_h, ed.screen.text_top + ed.screen.body_h - ed.screen.bottom_h);
        mouse(&mut ed, K::Down, 60, top as u16);
        mouse(&mut ed, K::Drag, 60, top as u16 - 5);
        mouse(&mut ed, K::Up, 60, top as u16 - 5);
        draw(&mut ed);
        assert_eq!(ed.screen.bottom_h, h + 5);
        // A press in the code isn't a drag
        mouse(&mut ed, K::Down, 60, 3);
        assert!(ed.dragging.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A finished task of 40 lines, its panel at the bottom.
    fn task_of_40_lines(ed: &mut Editor) {
        let mut run = crate::task::Run::for_test(crate::task::Task {
            name: "t".into(),
            cmd: "true".into(),
            cwd: std::env::temp_dir(),
        });
        run.output = (0..40).map(|i| (false, format!("line {i}"))).collect();
        ed.session.task_run = Some(run);
    }

    /// The bottom slot takes the keys — `C-w j` below the code, or a click on it: they scroll its output
    /// (back to following its end at `G`) and never reach the code; Esc or `C-w k` give them back, and so
    /// does a click in the code. Nothing there: it says so.
    #[test]
    fn the_bottom_slot_takes_the_keys() {
        use crate::event::{Mouse, MouseKind as K};
        let mut ed = editor("a\nb\nc\n");
        let draw = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 40).unwrap();
        };
        feed(&mut ed, "<C-w>j");
        assert!(!ed.session.is_at(crate::session::At::Slot));
        assert!(ed.toasts.iter().any(|t| t.text.starts_with("nothing at the bottom")));
        task_of_40_lines(&mut ed);
        draw(&mut ed);
        let rows = ed.screen.task_panel.get().unwrap().2;
        let end = 40 - rows;
        let top = |ed: &Editor| ed.session.task_run.as_ref().unwrap().top(rows);
        assert_eq!(top(&ed), end, "its end in sight");
        feed(&mut ed, "<C-w>j");
        assert!(ed.session.is_at(crate::session::At::Slot));
        assert_eq!(
            ed.keys_to().region,
            crate::uistate::Region::Output(crate::rundoc::Run::Task),
            "its output, a document"
        );
        feed(&mut ed, "<C-y><C-y><C-y>");
        assert_eq!(top(&ed), end - 3, "scrolled back");
        feed(&mut ed, "xdi");
        assert_eq!(text(&ed), "a\nb\nc\n", "the code never saw them");
        let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert_eq!(said, "the task's output is read-only — esc code");
        assert_eq!(ed.mode, Mode::Normal);
        assert_eq!(ed.cursor_line(), 0);
        feed(&mut ed, "gg");
        assert_eq!(top(&ed), 0);
        feed(&mut ed, "G");
        assert_eq!(ed.session.task_run.as_ref().unwrap().scroll, None, "at the end: follows it again");
        // Its header says what the keys do there, and the status line that they're its view mode's
        draw(&mut ed);
        let (frame, w, h) = ed.last_frame.clone().unwrap();
        let screen = crate::screen::text(&frame, w, h).unwrap();
        let head = screen.lines().find(|l| l.contains("y copy")).unwrap();
        assert!(head.contains(" ok ") && head.contains("/ search"), "{head}");
        assert!(screen.lines().any(|l| l.starts_with(" ● normal · task ")), "{screen}");
        feed(&mut ed, "<esc>j");
        assert!(!ed.session.is_at(crate::session::At::Slot));
        assert_eq!(ed.cursor_line(), 1, "the code's again");
        feed(&mut ed, "<C-w>j<C-w>k");
        assert!(!ed.session.is_at(crate::session::At::Slot), "C-w k: back up");
        // A click on the panel, then one in the code
        let y = ed.screen.task_panel.get().unwrap().0 as u16 + 2;
        ed.handle_mouse(Mouse { kind: K::Down, x: 20, y, alt: false, ctrl: false });
        assert!(ed.session.is_at(crate::session::At::Slot));
        let line_1 = ed.screen.text_top as u16 + 1;
        ed.handle_mouse(Mouse { kind: K::Down, x: 20, y: line_1, alt: false, ctrl: false });
        assert!(!ed.session.is_at(crate::session::At::Slot));
        assert_eq!(ed.cursor_line(), 1);
        // The panel gone: so are its keys
        feed(&mut ed, "<C-w>j");
        ed.session.task_run = None;
        feed(&mut ed, "j");
        assert!(!ed.session.is_at(crate::session::At::Slot));
        assert_eq!(ed.cursor_line(), 2);
    }

    /// `C-w z` fills the screen with what has the keys — the code (its pane alone), the bottom slot, the
    /// chat, the side panel — and again gives the rest back; the keys going elsewhere end it too. The
    /// sizes you set stay as they were.
    #[test]
    fn c_w_z_fills_the_screen_with_what_has_the_keys() {
        let mut ed = editor("fn main() {}\n");
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        let draw = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 40).unwrap();
        };
        let shown =
            |ed: &Editor| (ed.screen.chat_w, ed.screen.side_w, ed.screen.bottom_h, ed.screen.panes.len());
        // Alone: nothing to hide
        draw(&mut ed);
        feed(&mut ed, "<C-w>z");
        assert_eq!(ed.session.zoom, None);
        assert!(ed.toasts.iter().any(|t| t.text == "the code has the whole screen already"));
        // The chat, the side panel, a task, two panes
        crate::chat::open(&mut ed);
        ed.session.sidebar =
            Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Files, std::env::temp_dir()));
        ed.session.at = At::Code;
        task_of_40_lines(&mut ed);
        feed(&mut ed, "<C-w>v");
        draw(&mut ed);
        let all = shown(&ed);
        assert!(all.0 > 0 && all.1 > 0 && all.2 > 0 && all.3 == 2, "{all:?}");
        let sizes = ed.sizes;
        // The code: its pane alone, the whole width
        feed(&mut ed, "<C-w>z");
        draw(&mut ed);
        assert_eq!(shown(&ed), (0, 0, 0, 1));
        assert_eq!(ed.screen.panes[0].1.w, 140);
        let (frame, w, h) = ed.last_frame.clone().unwrap();
        let screen = crate::screen::text(&frame, w, h).unwrap();
        assert!(screen.lines().last().unwrap().contains("full screen  C-w z back"), "{screen}");
        feed(&mut ed, "<C-w>z");
        draw(&mut ed);
        assert_eq!(shown(&ed), all, "back as it was");
        // The bottom slot: all the rows; its keys still scroll it; Esc back to the code ends it
        feed(&mut ed, "<C-w>j<C-w>z");
        draw(&mut ed);
        assert_eq!((ed.screen.bottom_h, ed.screen.chat_w, ed.screen.side_w), (ed.screen.body_h, 0, 0));
        feed(&mut ed, "<C-y>");
        assert_eq!(
            ed.session.task_run.as_ref().unwrap().scroll,
            Some(40 - ed.screen.task_panel.get().unwrap().2 - 1)
        );
        feed(&mut ed, "<esc>");
        draw(&mut ed);
        assert_eq!((ed.session.zoom, shown(&ed)), (None, all), "the keys went to the code: it ended");
        // The chat (its empty input lets C-w through)
        ed.session.at = crate::session::At::Chat;
        feed(&mut ed, "<C-w>z");
        draw(&mut ed);
        assert_eq!((ed.screen.chat_w, ed.screen.bottom_h), (140, 0));
        // (`esc` its view mode, still the chat's — `esc` again the code's)
        feed(&mut ed, "<esc><esc>");
        draw(&mut ed);
        assert_eq!(shown(&ed), all);
        // The side panel
        ed.session.at = crate::session::At::Side;
        feed(&mut ed, "<C-w>z");
        draw(&mut ed);
        assert_eq!((ed.screen.side_w, ed.screen.chat_w, ed.screen.bottom_h), (140, 0, 0));
        feed(&mut ed, "<C-w>z");
        draw(&mut ed);
        assert_eq!(shown(&ed), all);
        assert!(ed.session.is_at(crate::session::At::Side), "back, the keys where they were");
        assert_eq!(ed.sizes, sizes, "nothing kept changed");
    }

    /// `A` in the git tab opens the last message to amend in normal mode (the first keys move, they don't
    /// land on top of the message); `c` still opens a new one ready to type.
    #[test]
    fn amending_opens_the_message_in_normal_mode() {
        let repo = git_repo("amend", &[("a.txt", "one\n")]);
        let mut ed = editor("");
        ed.session.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone()));
        ed.session.at = crate::session::At::Side;
        ed.git_refresh();
        settle_until(&mut ed, |ed| ed.session.git_status.is_some());
        ed.session.at = crate::session::At::Side;
        feed(&mut ed, "A");
        settle_until(&mut ed, |ed| ed.session.committing.is_some());
        assert_eq!(ed.doc().display_name_short(), "COMMIT_EDITMSG");
        assert_eq!(ed.mode, Mode::Normal);
        assert!(text(&ed).starts_with("base\n"), "{}", text(&ed));
        feed(&mut ed, "A, amended<esc>:w<ret>");
        settle_until(&mut ed, |ed| ed.docs.iter().all(|d| d.display_name_short() != "COMMIT_EDITMSG"));
        let log = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["log", "--format=%s"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "base, amended", "one commit, amended");
        // A new message: insert mode, as before
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        git_in(&repo, &["add", "a.txt"]);
        ed.git_refresh();
        settle_until(&mut ed, |ed| ed.session.git_status.as_ref().is_some_and(|s| !s.staged.is_empty()));
        ed.session.at = crate::session::At::Side;
        feed(&mut ed, "c");
        settle_until(&mut ed, |ed| ed.session.committing.is_some());
        assert_eq!(ed.mode, Mode::Insert);
        std::fs::remove_dir_all(&repo).ok();
    }

    /// `:git-amend` with nothing staged is `A`'s reword: the last message opens, `:w` changes only the message
    /// (the tree as it was). Not in a repository, the status can't be read: said, no buffer.
    #[test]
    fn git_amend_with_nothing_staged_rewords_and_outside_a_repository_says_why() {
        let repo = git_repo("amend-reword", &[("a.txt", "one\n")]);
        let tree = git_out(&repo, &["rev-parse", "HEAD^{tree}"]);
        let mut ed = editor("");
        ed.session.root = repo.clone();
        crate::typed::execute(&mut ed, "git-amend").unwrap();
        settle_until(&mut ed, |ed| ed.session.committing.as_ref().is_some_and(|c| c.amend));
        assert!(text(&ed).starts_with("base\n") && text(&ed).contains("# :w amends the last commit"));
        feed(&mut ed, "A, reworded<esc>:w<ret>");
        settle_until(&mut ed, |ed| said(ed, ToastKind::Success, "Amended") == 1);
        assert_eq!(
            git_out(&repo, &["log", "--format=%s"]),
            "base, reworded",
            "one commit, its message changed"
        );
        assert_eq!(git_out(&repo, &["rev-parse", "HEAD^{tree}"]), tree, "nothing else");
        std::fs::remove_dir_all(&repo).ok();
        // Not a repository
        let plain = std::env::temp_dir().join(format!("tarae-amend-plain-{}", std::process::id()));
        std::fs::create_dir_all(&plain).unwrap();
        let mut ed = editor("");
        ed.session.root = std::fs::canonicalize(&plain).unwrap();
        crate::typed::execute(&mut ed, "git-amend").unwrap();
        settle_until(&mut ed, |ed| ed.messages.iter().any(|m| m.2.starts_with("amend: ")));
        assert!(ed.session.committing.is_none(), "no buffer");
        std::fs::remove_dir_all(&plain).ok();
    }

    /// frictions14: after `:restart` another session can be in front — a checkout whose `.tarae.toml` is
    /// another version (one without `[preflight]`: the bring card's `p` ran the packs' defaults). The config
    /// is still the checkout tarae started in's: what's watched (`watches_start`) and what `:config-reload`
    /// reads.
    #[test]
    fn the_config_is_the_started_checkouts_whichever_session_is_in_front() {
        let scratch = crate::scratch::Scratch::new("config-reload-root");
        let dir = scratch.to_path_buf();
        for (sub, width) in [("started", 7), ("other", 3)] {
            std::fs::create_dir_all(dir.join(sub)).unwrap();
            std::fs::write(dir.join(sub).join(".tarae.toml"), format!("[editor]\ntab-width = {width}\n"))
                .unwrap();
        }
        let dir = std::fs::canonicalize(&dir).unwrap();
        let mut ed = editor("");
        ed.session.root = dir.join("started");
        let other = ed.session_open(&dir.join("other")).unwrap();
        assert!(ed.session_switch(other));
        assert_eq!(ed.root(), dir.join("other"), "the other session in front");
        assert_eq!(ed.started_root(), dir.join("started"));
        ed.config.tab_width = 4;
        crate::typed::execute(&mut ed, "config-reload").unwrap();
        settle_until(&mut ed, |ed| ed.config.tab_width != 4);
        assert_eq!(ed.config.tab_width, 7, "the started checkout's .tarae.toml");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `C` (and `c`) in the git tab with nothing staged: no buffer, and said why and what to press — `S`
    /// stages them all.
    #[test]
    fn committing_with_nothing_staged_says_what_to_press() {
        let repo = git_repo("commit-unstaged", &[("a.txt", "one\n")]);
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        let mut ed = editor("");
        ed.session.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone()));
        ed.session.at = crate::session::At::Side;
        ed.git_refresh();
        settle_until(&mut ed, |ed| ed.session.git_status.is_some());
        for key in ["C", "c"] {
            ed.status = None;
            ed.session.at = crate::session::At::Side;
            feed(&mut ed, key);
            settle_until(&mut ed, |ed| ed.status.is_some());
            let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
            assert!(
                said.contains("nothing staged to commit")
                    && said.contains(&format!("S stages them all, then {key}")),
                "{key}: {said}"
            );
            assert!(ed.session.committing.is_none(), "{key}: no message buffer");
        }
        std::fs::remove_dir_all(&repo).ok();
    }

    /// #217: `S` stages every change and `U` unstages everything, from whatever row is selected — a file deep
    /// in the list, not the Changes heading; with nothing to take, each says so.
    #[test]
    fn s_and_u_in_capitals_take_everything_from_any_row() {
        let repo = git_repo("stage-all", &[("a.txt", "one\n")]);
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        for f in ["b.txt", "c/d.txt", "c/e.txt"] {
            std::fs::create_dir_all(repo.join(f).parent().unwrap()).unwrap();
            std::fs::write(repo.join(f), "new\n").unwrap();
        }
        let mut ed = editor("");
        ed.session.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone()));
        ed.session.at = crate::session::At::Side;
        ed.git_refresh();
        settle_until(&mut ed, |ed| ed.session.git_status.as_ref().is_some_and(|s| s.unstaged.len() == 4));
        ed.session.at = At::Side;
        let sb = ed.session.sidebar.as_mut().unwrap();
        // (the last row: a file, not the heading)
        sb.selected = sb.lines().len() - 1;
        assert!(!sb.lines()[sb.selected].label.contains("Changes"));
        let counts = |ed: &Editor| ed.session.git_status.as_ref().map(|s| (s.staged.len(), s.unstaged.len()));
        feed(&mut ed, "S");
        settle_until(&mut ed, |ed| counts(ed) == Some((4, 0)));
        ed.session.at = crate::session::At::Side;
        feed(&mut ed, "S");
        assert_eq!(ed.status.as_ref().map(|s| s.0.as_str()), Some("nothing to stage"));
        ed.session.at = crate::session::At::Side;
        feed(&mut ed, "U");
        settle_until(&mut ed, |ed| counts(ed) == Some((0, 4)));
        std::fs::remove_dir_all(&repo).ok();
    }

    /// `C` in the git tab: the commit message is drafted by Claude from what's staged and the repository's
    /// recent messages — an in-buffer review over the message part; accepted and saved, it's the commit's.
    #[test]
    fn claude_drafts_the_commit_message() {
        let repo = git_repo("commitdraft", &[("a.txt", "one\n")]);
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        let log = repo.join("..").join(format!("commitdraft-{}.log", std::process::id()));
        let mut ed = editor("");
        // (a body line past 72 columns — the draft comes wrapped: #21)
        let body = "The second line was missing, so the file ended a line early and the count it gave back \
                    was one short.";
        let reply = serde_json::json!({"type": "result", "result": format!("Add two\n\n{body}")});
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!("read -r line; printf '%s\\n' \"$line\" > '{}'; printf '%s\\n' '{reply}'", log.display()),
        ];
        ed.session.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone()));
        ed.session.at = crate::session::At::Side;
        ed.git_refresh();
        settle_until(&mut ed, |ed| ed.session.git_status.is_some());
        ed.sidebar_sync();
        ed.session.sidebar.as_mut().unwrap().selected = 1;
        feed(&mut ed, "s");
        settle_until(&mut ed, |ed| ed.session.git_status.as_ref().is_some_and(|s| !s.staged.is_empty()));
        feed(&mut ed, "C");
        settle_until(&mut ed, |ed| ed.session.review.as_ref().is_some_and(|r| r.ready));
        let sent = std::fs::read_to_string(&log).unwrap();
        assert!(sent.contains("<recent-commits>") && sent.contains("base"), "its style: {sent}");
        assert!(sent.contains("+two"), "the staged diff: {sent}");
        assert!(sent.contains("Wrap the body at 72 columns"), "asked for: {sent}");
        feed(&mut ed, "a");
        let wrapped = "The second line was missing, so the file ended a line early and the\ncount it gave \
                       back was one short.";
        assert!(
            ed.doc().text.to_string().starts_with(&format!("Add two\n\n{wrapped}\n")),
            "wrapped at 72: {}",
            ed.doc().text
        );
        feed(&mut ed, ":w<ret>");
        settle_until(&mut ed, |ed| {
            ed.session.committing.is_none()
                && ed.docs.iter().all(|d| d.display_name_short() != "COMMIT_EDITMSG")
        });
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["log", "-1", "--format=%B"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), format!("Add two\n\n{wrapped}"));
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_file(&log).ok();
    }

    /// A draft that runs on can be stopped, and stops by itself: the waiting card says `esc stop` and Esc does
    /// (in normal mode, the code's keys), its process killed and nothing changed; one still writing at its
    /// time limit (`llm::DRAFT_LIMIT`) is stopped the same way, said. One ran 6 minutes with nothing to stop it.
    /// And with a code review open, `C` says to end it first (the message couldn't be typed there, the draft
    /// landed as an edit waiting in it).
    #[test]
    fn a_commit_draft_that_runs_on_is_stopped() {
        let repo = git_repo("commitdraft-stop", &[("a.txt", "one\n")]);
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        let mut ed = editor("");
        // (a claude that never answers)
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "read -r line; sleep 30".into()];
        ed.session.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone()));
        ed.session.at = crate::session::At::Side;
        ed.git_refresh();
        settle_until(&mut ed, |ed| ed.session.git_status.is_some());
        ed.sidebar_sync();
        ed.session.sidebar.as_mut().unwrap().selected = 1;
        feed(&mut ed, "s");
        settle_until(&mut ed, |ed| ed.session.git_status.as_ref().is_some_and(|s| !s.staged.is_empty()));
        // A code review open: said, no message opened
        ed.session.code_review = Some(crate::codereview::CodeReview::for_test(
            crate::codereview::Base::Head,
            repo.clone(),
            Vec::new(),
        ));
        feed(&mut ed, "C");
        let said = ed.messages.last().map(|m| m.2.clone()).unwrap_or_default();
        assert!(said.contains("a code review is open — end it first (space v q)"), "{said}");
        ed.session.code_review = None;
        // Esc stops it
        feed(&mut ed, "C");
        settle_until(&mut ed, |ed| ed.session.review.as_ref().is_some_and(|r| !r.ready));
        ed.session.leave(crate::session::At::Side);
        assert!(crate::llm::esc_stops(&ed), "Esc stops it, and the card says so");
        let mut screen = Vec::new();
        crate::term::render(&mut ed, &mut screen, 120, 40).unwrap();
        let screen = String::from_utf8_lossy(&screen).into_owned();
        assert!(screen.contains("esc") && screen.contains(" stop"), "the card's hint");
        assert_eq!(ed.keys_to().route(Key::plain(crate::key::Code::Esc)), crate::uistate::Region::EditReview);
        assert!(ed.llm_pid.is_some(), "its process");
        feed(&mut ed, "<esc>");
        assert!(ed.session.review.is_none() && ed.llm_pid.is_none(), "stopped, its process killed");
        let said = ed.messages.last().map(|m| m.2.clone()).unwrap_or_default();
        assert!(said.contains("Claude's a commit message: stopped — nothing changed"), "{said}");
        assert!(!ed.doc().text.to_string().starts_with("Add"), "nothing changed");
        // Its time limit: the same, said why
        feed(&mut ed, ":commit-draft<ret>");
        settle_until(&mut ed, |ed| ed.session.review.as_ref().is_some_and(|r| !r.ready));
        let generation = ed.session.review.as_ref().unwrap().generation;
        // (the limit's job, as it lands)
        crate::llm::stop_waiting(
            &mut ed,
            Some(generation),
            "still writing after 2 minutes — stopped, nothing changed",
        );
        assert!(ed.session.review.is_none() && ed.llm_pid.is_none());
        let said = ed.messages.last().map(|m| m.2.clone()).unwrap_or_default();
        assert!(said.contains("after 2 minutes — stopped"), "{said}");
        std::fs::remove_dir_all(&repo).ok();
    }

    /// `space e` during a review gives the rail the keys: `j`/`k` over its files (in its order), Enter opens
    /// one, `m` marks it viewed and goes on to the next, Esc back to the code.
    #[test]
    fn the_review_rail_takes_the_keys() {
        use crate::codereview::{Base, CodeReview, File};
        let dir = std::env::temp_dir().join(format!("tarae-rail-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let file = |rel: &str| {
            std::fs::write(dir.join(rel), "x\n").unwrap();
            File {
                rel: rel.into(),
                path: dir.join(rel),
                added: 1,
                removed: 0,
                new: true,
                gone: false,
                binary: false,
            }
        };
        let files = vec![file("src/b.rs"), file("a.rs"), file("src/c.rs")];
        let mut ed = editor("");
        ed.open(&dir.join("src/b.rs")).unwrap();
        ed.session.code_review = Some(CodeReview::for_test(Base::Head, dir.clone(), files));
        feed(&mut ed, " e");
        let r = ed.session.code_review.as_ref().unwrap();
        assert!(ed.session.is_at(At::Rail) && r.selected == 0, "on the file being read");
        // The rail's order: src/ (b, c) then a.rs
        feed(&mut ed, "j");
        assert_eq!(ed.session.code_review.as_ref().unwrap().selected, 2, "src/c.rs");
        feed(&mut ed, "<ret>");
        assert_eq!(ed.doc().path.as_deref(), Some(dir.join("src/c.rs").as_path()));
        assert!(!ed.session.is_at(crate::session::At::Rail), "the code has the keys");
        feed(&mut ed, " ek");
        assert_eq!(ed.session.code_review.as_ref().unwrap().selected, 0);
        feed(&mut ed, "m");
        let r = ed.session.code_review.as_ref().unwrap();
        assert!(r.viewed(&ed, &r.files[0]), "b.rs viewed (its base on the way)");
        settle_until(&mut ed, |ed| ed.doc().review_base.is_some());
        let r = ed.session.code_review.as_ref().unwrap();
        assert!(r.viewed(&ed, &r.files[0]), "b.rs viewed");
        assert_eq!(r.selected, 2, "on to the next");
        feed(&mut ed, "<esc>");
        assert!(!ed.session.is_at(crate::session::At::Rail));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The rail's `q` ends the review, as `space v q` does — files not viewed: its card asks first (uxaudit D2:
    /// it only gave the keys back, where every other list panel's `q` closes it). `C-w` goes through the rail to
    /// the window keys (D8: it said "does nothing in the review's file list"), and `C-w q` there ends it too (D3).
    #[test]
    fn the_rails_q_ends_the_review_and_c_w_goes_through_it() {
        use crate::codereview::{Base, CodeReview, File};
        let dir = crate::scratch::Scratch::new("rail-q");
        let files = ["a.rs", "b.rs"]
            .map(|rel| {
                std::fs::write(dir.join(rel), "x\n").unwrap();
                File {
                    rel: rel.into(),
                    path: dir.join(rel),
                    added: 1,
                    removed: 0,
                    new: true,
                    gone: false,
                    binary: false,
                }
            })
            .to_vec();
        let mut ed = editor("");
        ed.open(&dir.join("a.rs")).unwrap();
        ed.session.code_review = Some(CodeReview::for_test(Base::Head, dir.to_path_buf(), files));
        let asked =
            |ed: &Editor| ed.offers.iter().any(|o| matches!(o.what, crate::offer::What::ReviewEnd { .. }));
        feed(&mut ed, " eq");
        assert!(asked(&ed), "two files not viewed: the end asks");
        assert!(ed.session.code_review.is_some(), "not ended unasked");
        ed.offers.clear();
        ed.session.at = crate::session::At::Rail;
        feed(&mut ed, "<C-w>");
        assert!(!ed.pending.is_empty(), "C-w waits for its window key");
        assert!(ed.status.as_ref().is_none_or(|s| !s.0.contains("does nothing")), "{:?}", ed.status);
        feed(&mut ed, "q");
        assert!(asked(&ed), "C-w q from the rail: its end, asked");
        // Every file viewed: `q` ends it at once, nothing to ask
        ed.offers.clear();
        ed.session.at = crate::session::At::Rail;
        ed.session.code_review.as_mut().unwrap().selected = 0;
        let viewed = |ed: &Editor, i: usize| {
            let r = ed.session.code_review.as_ref().unwrap();
            r.viewed(ed, &r.files[i])
        };
        feed(&mut ed, "m");
        settle_until(&mut ed, |ed| viewed(ed, 0));
        ed.session.at = crate::session::At::Rail;
        feed(&mut ed, "m");
        settle_until(&mut ed, |ed| viewed(ed, 1));
        ed.session.at = crate::session::At::Rail;
        feed(&mut ed, "q");
        assert!(ed.session.code_review.is_none(), "all viewed: ended");
        assert!(!asked(&ed), "nothing asked");
    }

    /// `q` closes the bottom panel with tarae's keys (uxaudit D2) — a task's once its run has ended: one under
    /// way runs on, said (`space m c` stops it), and its header offers `q` only once it can. `C-w q` closes what
    /// has the keys — this panel, the side panel — and, from the code, a pane as ever (D3).
    #[test]
    fn q_closes_the_panel_that_has_the_keys() {
        let mut ed = editor("a\n");
        let head = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 40).unwrap();
            let (frame, w, h) = ed.last_frame.clone().unwrap();
            let screen = crate::screen::text(&frame, w, h).unwrap();
            screen.lines().find(|l| l.contains("y copy")).unwrap_or_default().to_string()
        };
        task_of_40_lines(&mut ed);
        ed.session.task_run.as_mut().unwrap().state = crate::task::State::Running;
        head(&mut ed);
        feed(&mut ed, "<C-w>j");
        assert!(ed.session.is_at(crate::session::At::Slot));
        let h = head(&mut ed);
        assert!(h.contains("esc code") && !h.contains("q close"), "{h}");
        feed(&mut ed, "q");
        assert!(ed.session.task_run.is_some(), "a run under way isn't stopped by q");
        assert!(ed.status.as_ref().is_some_and(|s| s.0.contains("space m c stops")), "{:?}", ed.status);
        ed.session.task_run.as_mut().unwrap().state = crate::task::State::Passed;
        let h = head(&mut ed);
        assert!(h.contains("q close"), "{h}");
        feed(&mut ed, "q");
        assert!(ed.session.task_run.is_none(), "ended: q closes it");
        assert!(!ed.session.is_at(crate::session::At::Slot));
        // `C-w q` in it: the same
        task_of_40_lines(&mut ed);
        head(&mut ed);
        feed(&mut ed, "<C-w>j<C-w>q");
        assert!(ed.session.task_run.is_none(), "C-w q closed the panel");
        // The side panel
        feed(&mut ed, " e");
        assert!(ed.session.is_at(crate::session::At::Side));
        feed(&mut ed, "<C-w>q");
        assert!(ed.session.sidebar.is_none(), "C-w q closed the side panel");
        // The code: its pane, as ever — the last one said, kept
        feed(&mut ed, "<C-w>q");
        let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert!(said.starts_with("the code keeps one pane"), "{said}");
        assert_eq!(ed.session.views.len(), 1);
        // A rebase stopped at its conflicts isn't running, and `space m c` isn't its way out: said so, kept
        task_of_40_lines(&mut ed);
        ed.session.task_run.as_mut().unwrap().state = crate::task::State::Stopped("2 conflicts".into());
        head(&mut ed);
        feed(&mut ed, "<C-w>jq");
        assert!(ed.session.task_run.is_some(), "the rebase's panel stays");
        let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert!(said.contains("waits at 2 conflicts") && said.contains(":rebase-abort"), "{said}");
        assert!(!said.contains("space m c"), "{said}");
    }

    /// The test panel's `q` as the task panel's: a run under way is left running (`space x c` stops it), an ended
    /// one closes — one that ran no test too.
    #[test]
    fn q_on_the_test_panel_closes_it_once_its_run_has_ended() {
        use crate::testing::{RunState, TestRun};
        let mut ed = editor("a\n");
        let draw = |ed: &mut Editor| crate::term::render(ed, &mut Vec::new(), 140, 40).unwrap();
        let mut run = TestRun::finished("t", Vec::new(), vec!["running 1 test".into()]);
        run.state = RunState::Running;
        ed.session.test_run = Some(run);
        draw(&mut ed);
        feed(&mut ed, "<C-w>j");
        assert!(ed.session.is_at(crate::session::At::Slot));
        feed(&mut ed, "q");
        assert!(ed.session.test_run.is_some(), "tests under way aren't stopped by q");
        assert!(ed.status.as_ref().is_some_and(|s| s.0.contains("space x c stops")), "{:?}", ed.status);
        ed.session.test_run.as_mut().unwrap().state = RunState::NoTests;
        feed(&mut ed, "q");
        assert!(ed.session.test_run.is_none(), "ended (no test ran): q closes it");
        assert!(!ed.session.is_at(crate::session::At::Slot));
    }

    /// `C-w q` from `:procs` closes it, as its `q` does; from the chat it closes nothing and says `space L` does
    /// (closing the chat ends its conversation's process — two keys mustn't cut a turn off).
    #[test]
    fn c_w_q_closes_procs_and_leaves_the_chat_saying_how() {
        let mut ed = editor("a\n");
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        ed.procs_toggle();
        assert!(ed.procs_focused());
        feed(&mut ed, "<C-w>q");
        assert!(ed.procs.panel.is_none(), "C-w q closed :procs");
        feed(&mut ed, " l");
        assert!(ed.session.chat_has_keys());
        feed(&mut ed, "<C-w>q");
        assert!(ed.session.chat.is_some(), "the chat stays");
        assert!(
            ed.status.as_ref().is_some_and(|s| s.0.contains("space L closes the chat")),
            "{:?}",
            ed.status
        );
    }

    /// `space g` is the git panel, lazygit beside the code: from the code it opens on the hunk under the
    /// cursor (its file opened to its hunks) and the next key acts there (`s` stages that hunk alone), the
    /// tab keeping the keys. In the panel, moving onto a hunk shows it in the editor and `d` puts the
    /// index's lines back; `z` / `Z` stash and take it back; `?` lists the keys.
    /// `space g` from a changed line: the tab keeps the keys for moving about (`k k k` walks its rows, none
    /// lands in the code — #18: after one, the rest went into the code), a quick key on the change under the
    /// cursor (`s`) too (#50: the `j`/`k`/`s` after it went into the code).
    #[test]
    fn space_g_on_a_change_keeps_the_keys_for_moving() {
        let repo = git_repo("gitmoves", &[("a.txt", "one\ntwo\nthree\nfour\nfive\n"), ("b.txt", "b\n")]);
        std::fs::write(repo.join("b.txt"), "b\nmore\n").unwrap();
        let file = repo.join("a.txt");
        let mut ed = editor("");
        ed.session.root = repo.clone(); // (a file's git is the session's repository's — `gitrepo`)
        ed.git_auto = true;
        ed.open(&file).unwrap();
        settle_until(&mut ed, |ed| ed.doc().git_base.is_some());
        feed(&mut ed, "4ggmiwcFOUR<esc>:w<ret>");
        ed.flush_saves();
        settle_until(&mut ed, |ed| ed.doc().git_hunks.len() == 1);
        let sb = crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone());
        ed.session.sidebar = Some(sb);
        feed(&mut ed, " g");
        settle_until(&mut ed, |ed| {
            ed.session.git_status.is_some() && ed.session.sidebar.as_ref().unwrap().aim.is_none()
        });
        let text_before = text(&ed);
        let at = |ed: &Editor| ed.session.sidebar.as_ref().unwrap().selected;
        let start = at(&ed);
        feed(&mut ed, "k");
        assert!(ed.session.is_at(crate::session::At::Side), "the tab keeps the keys after one k");
        feed(&mut ed, "kk");
        assert!(ed.session.is_at(crate::session::At::Side), "and after three");
        assert!(at(&ed) < start, "k walked up the tab's rows: {} → {}", start, at(&ed));
        assert_eq!(text(&ed), text_before, "no key landed in the code");
        feed(&mut ed, "jjj");
        assert!(ed.session.is_at(crate::session::At::Side));
        // A tab key that isn't one of the quick ones (refresh) leaves it a tab — the keys stay
        feed(&mut ed, "<esc>");
        feed(&mut ed, " g");
        settle_until(&mut ed, |ed| ed.session.sidebar.as_ref().unwrap().aim.is_none());
        feed(&mut ed, "r");
        assert!(ed.session.is_at(crate::session::At::Side), "r refreshed it and the tab still has the keys");
        feed(&mut ed, "k");
        assert!(ed.session.is_at(crate::session::At::Side));
        assert_eq!(text(&ed), text_before);
        // Pressed before git has answered (the tab still empty): the same
        feed(&mut ed, "<esc>");
        ed.session.git_status = None;
        feed(&mut ed, " gk");
        assert!(ed.session.is_at(crate::session::At::Side), "a k before the rows are in");
        settle_until(&mut ed, |ed| {
            ed.session.git_status.is_some() && ed.session.sidebar.as_ref().unwrap().aim.is_none()
        });
        feed(&mut ed, "kk");
        assert!(ed.session.is_at(crate::session::At::Side), "and the ones after");
        assert_eq!(text(&ed), text_before, "no key landed in the code");
        // #50: on the change itself, a file opened from the editor just before — `s` stages it there, and
        // the `j`/`k` after it are still the tab's
        feed(&mut ed, "<esc>");
        ed.open(&repo.join("b.txt")).unwrap();
        feed(&mut ed, "2gg");
        settle_until(&mut ed, |ed| ed.doc().git_hunks.len() == 1);
        feed(&mut ed, " g");
        settle_until(&mut ed, |ed| ed.session.sidebar.as_ref().unwrap().aim.is_none());
        assert!(ed.git_code_here().0, "on the change");
        let b_before = text(&ed);
        feed(&mut ed, "s");
        settle_until(&mut ed, |ed| ed.doc().git_hunks.is_empty());
        assert!(ed.session.is_at(crate::session::At::Side), "the tab keeps the keys after s");
        // (`o` would open a line in the code)
        feed(&mut ed, "jjko");
        assert!(ed.session.is_at(crate::session::At::Side), "the tab has the keys");
        assert_eq!(ed.mode, Mode::Normal);
        let b = ed.docs.iter().find(|d| d.path.as_ref().is_some_and(|p| p.ends_with("b.txt"))).unwrap();
        assert_eq!(b.text.to_string(), b_before, "none landed in the code");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn git_panel_hunks_like_lazygit() {
        let repo = git_repo("hub", &[("a.txt", "one\ntwo\nthree\nfour\nfive\n")]);
        let file = repo.join("a.txt");
        let mut ed = editor("");
        ed.session.root = repo.clone(); // (a file's git is the session's repository's — `gitrepo`)
        ed.git_auto = true;
        ed.open(&file).unwrap();
        settle_until(&mut ed, |ed| ed.doc().git_base.is_some());
        feed(&mut ed, "ggmiwcONE<esc>4gg");
        feed(&mut ed, "miwcFOUR<esc>:w<ret>");
        ed.flush_saves(); // (on disk before git's status reads it)
        settle_until(&mut ed, |ed| ed.doc().git_hunks.len() == 2);
        let sb = crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone());
        ed.session.sidebar = Some(sb);
        // From the code, the cursor in the second hunk
        feed(&mut ed, " g");
        let selected = |ed: &mut Editor| {
            ed.sidebar_sync();
            let s = ed.session.sidebar.as_ref().unwrap();
            s.lines().get(s.selected).map(|l| l.target.clone())
        };
        settle_until(&mut ed, |ed| {
            ed.session.git_status.is_some() && ed.session.sidebar.as_ref().unwrap().aim.is_none()
        });
        assert!(
            matches!(selected(&mut ed), Some(crate::sidebar::Target::Hunk { index: 1, .. })),
            "on the hunk under the cursor: {:?}",
            selected(&mut ed)
        );
        feed(&mut ed, "s");
        settle_until(&mut ed, |ed| ed.doc().git_hunks.len() == 1);
        let staged = std::process::Command::new("git")
            .arg("-C")
            .arg(&repo)
            .args(["diff", "--cached"])
            .output()
            .unwrap();
        let staged = String::from_utf8_lossy(&staged.stdout);
        assert!(staged.contains("+FOUR") && !staged.contains("+ONE"), "that hunk alone: {staged}");
        assert!(ed.session.is_at(crate::session::At::Side), "the tab keeps the keys (#50)");
        // In the panel: onto the other hunk (the editor goes there), `d` puts it back
        feed(&mut ed, "<esc> g");
        settle_until(&mut ed, |ed| ed.session.sidebar.as_ref().unwrap().aim.is_none());
        let rows: Vec<crate::sidebar::Target> =
            ed.session.sidebar.as_ref().unwrap().lines().iter().map(|l| l.target.clone()).collect();
        let hunk =
            rows.iter().position(|t| matches!(t, crate::sidebar::Target::Hunk { .. })).expect("a hunk row");
        ed.session.sidebar.as_mut().unwrap().from_code = false;
        ed.session.sidebar.as_mut().unwrap().selected = hunk.saturating_sub(1);
        feed(&mut ed, "j");
        assert_eq!(ed.cursor_line(), 0, "the editor on it");
        feed(&mut ed, "d");
        assert_eq!(ed.doc().text.line(0).to_string(), "one\n", "its index lines back");
        // Stash and back
        feed(&mut ed, ":w<ret>");
        ed.flush_saves(); // (on disk before git stashes it)
        ed.session.at = crate::session::At::Side;
        feed(&mut ed, "z");
        settle_until(&mut ed, |_| {
            std::fs::read_to_string(&file).unwrap_or_default() == "one\ntwo\nthree\nfour\nfive\n"
        });
        feed(&mut ed, "Z");
        settle_until(&mut ed, |_| std::fs::read_to_string(&file).unwrap_or_default().contains("FOUR"));
        feed(&mut ed, "?");
        assert!(ed.popup.is_some(), "every key, as a card");
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Panes P3 step 5: the git tab's selection drives the code — `j` onto a changed file shows it at its first
    /// change, its removed lines rows of their own (against the index) while the tab has the keys; onto another,
    /// that one's, the first's gone; `esc code`: the rows gone, the gutter the index's again, its changes kept.
    #[test]
    fn the_git_tabs_file_shows_its_changes_inline_while_the_tab_has_the_keys() {
        use crate::codeview::Changes;
        let repo = git_repo("inline", &[("a.txt", "one\ntwo\nthree\n"), ("b.txt", "x\ny\nz\nw\n")]);
        std::fs::write(repo.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::write(repo.join("b.txt"), "x\ny\nz\nW\n").unwrap();
        let mut ed = editor("");
        ed.session.root = repo.clone();
        ed.git_auto = true;
        feed(&mut ed, " g");
        settle_until(&mut ed, |ed| ed.session.git_status.is_some());
        let selected = |ed: &Editor| {
            let s = ed.session.sidebar.as_ref().unwrap();
            s.lines().get(s.selected).map(|l| l.target.clone())
        };
        let file = |rel: &str| crate::sidebar::Target::Change {
            rel: rel.into(),
            staged: false,
            untracked: false,
            file: true,
        };
        assert_eq!(selected(&ed), Some(file("a.txt")), "on the first file");
        let shown = |ed: &Editor, name: &str| {
            ed.doc().path.as_deref() == Some(repo.join(name).as_path())
                && ed.doc().inline.is_some()
                && ed.doc().git_version == ed.doc().version()
        };
        // `j`: the next file in the code, its change selected, what it removed a row of its own above it
        feed_events(&mut ed, "j");
        assert_eq!(selected(&ed), Some(file("b.txt")));
        settle_until(&mut ed, |ed| shown(ed, "b.txt") && ed.cursor_line() == 3);
        assert!(ed.session.is_at(crate::session::At::Side), "the tab keeps the keys");
        let d = ed.doc();
        assert_eq!(Changes::of(d), Changes::Base, "drawn as `:diff` draws it");
        assert_eq!(Changes::Base.removed(d, 0, 3), vec![(3, "w".to_string(), Some(3))]);
        assert!(d.review_base.as_ref().is_some_and(|b| *b == "x\ny\nz\nw\n"), "the index's text");
        let b = d.id;
        let screen = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 160, 24).unwrap();
            let mut parser = vt100::Parser::new(24, 160, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        let s = screen(&mut ed);
        assert!(s.contains("b.txt  against index"), "the path bar says what it's against: {s}");
        let rows: Vec<&str> =
            s.lines().filter(|l| l.contains("│")).map(|l| l.rsplit('│').next().unwrap()).collect();
        let at = rows
            .iter()
            .position(|l| l.trim_end().ends_with("−w"))
            .unwrap_or_else(|| panic!("the removed line, a row: {s}"));
        assert!(rows[at + 1].trim_end().ends_with("4▎W"), "above the line that replaced it: {s}");
        // `l`: its changes listed under it — against the index, not said to be a review's
        feed_events(&mut ed, "l");
        settle_until(&mut ed, |ed| {
            let s = ed.session.sidebar.as_ref().unwrap();
            s.lines().iter().any(|l| matches!(l.target, crate::sidebar::Target::Hunk { .. }))
        });
        feed_events(&mut ed, "h");
        // `k`: the first one, the second's rows gone
        feed_events(&mut ed, "k");
        settle_until(&mut ed, |ed| shown(ed, "a.txt") && ed.cursor_line() == 1);
        // (b.txt kept by `l` — opened to its hunks, as `enter`: its own buffer, not the preview a.txt replaces)
        let d = ed.docs.iter().find(|d| d.id == b).unwrap();
        assert!(d.inline.is_none() && d.review_base.is_none() && !d.preview, "only the file in the code");
        assert_eq!(Changes::of(d), Changes::Index);
        assert!(ed.doc().preview, "a.txt the preview");
        // `esc code`: the rows gone, the gutter back to the index, its change still there
        feed_events(&mut ed, "<esc>");
        assert!(ed.session.is_at(crate::session::At::Code));
        let d = ed.doc();
        assert!(d.inline.is_none() && d.review_base.is_none(), "gone with the keys");
        assert_eq!(Changes::of(d), Changes::Index);
        assert_eq!(d.git_hunks.len(), 1, "the index's change in the gutter");
        let s = screen(&mut ed);
        assert!(!s.contains("against index") && s.contains("a.txt"), "{s}");
        // Back in the tab: inline again, at once (the file in the code is among its changes)
        feed_events(&mut ed, " g");
        assert!(ed.doc().inline.is_some(), "the tab has the keys again");
        // `q`: the tab closed, the code its own
        feed_events(&mut ed, "q");
        assert!(ed.session.sidebar.is_none() && ed.doc().inline.is_none());
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Panes P3 step 5: a buffer under `:diff` keeps what you asked for — the git tab's selection on it shows it
    /// against that, and leaving the tab ends nothing of it; a staged hunk makes the inline base the new index.
    #[test]
    fn the_git_tab_leaves_a_diffs_base_and_follows_the_index() {
        let repo = git_repo("inline-diff", &[("a.txt", "one\ntwo\nthree\n"), ("b.txt", "x\ny\nz\n")]);
        std::fs::write(repo.join("a.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::write(repo.join("b.txt"), "X\ny\nZ\n").unwrap();
        let mut ed = editor("");
        ed.session.root = repo.clone();
        ed.git_auto = true;
        ed.open(&repo.join("a.txt")).unwrap();
        settle_until(&mut ed, |ed| ed.doc().git_base.is_some());
        feed(&mut ed, ":diff HEAD<ret>");
        settle_until(&mut ed, |ed| ed.doc().diff_of.is_some());
        let a = ed.doc().id;
        feed(&mut ed, " g");
        settle_until(&mut ed, |ed| {
            ed.session.git_status.is_some() && ed.session.sidebar.as_ref().unwrap().aim.is_none()
        });
        let d = ed.doc();
        assert!(
            d.id == a && d.inline.is_none() && d.diff_of.as_deref() == Some("HEAD"),
            "`:diff`'s, untouched"
        );
        // `j` onto b.txt and `<esc>`: a.txt's diff still there
        feed_events(&mut ed, "j");
        settle_until(&mut ed, |ed| ed.doc().inline.is_some() && ed.doc().git_version == ed.doc().version());
        assert_eq!(ed.doc().git_hunks.len(), 2);
        // Its first change staged (the tab's `s` on a hunk's row): the index read again, the inline base follows
        let before = ed.doc().inline.clone();
        assert_eq!(ed.cursor_line(), 0, "landed on its first change");
        ed.git_stage_hunk();
        settle_until(&mut ed, |ed| {
            let d = ed.doc();
            d.git_hunks.len() == 1 && d.git_version == d.version() && d.inline.is_some() && d.inline != before
        });
        assert!(ed.doc().review_base.as_ref().is_some_and(|b| *b == "X\ny\nz\n"), "the new index");
        // `:diff` there from the tab: what you asked for takes the base over, and the tab leaves it
        feed_events(&mut ed, ":diff HEAD<ret>");
        settle_until(&mut ed, |ed| ed.doc().diff_of.is_some());
        assert!(ed.doc().inline.is_none() && ed.doc().own_base(), "`:diff`'s now");
        let b = ed.doc().id;
        feed_events(&mut ed, "<esc>");
        let d = ed.docs.iter().find(|d| d.id == b).unwrap();
        assert!(d.review_base.as_ref().is_some_and(|b| *b == "x\ny\nz\n"), "HEAD's, kept");
        let d = ed.docs.iter().find(|d| d.id == a).unwrap();
        assert!(
            d.review_base.is_some() && d.diff_of.as_deref() == Some("HEAD"),
            "leaving the tab ends none of it"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Panes P3 step 5: a review started while the git tab shows a file inline takes the base over — HEAD's, not
    /// the index's the tab drew (its first file is the one in the code: it must load its base, not land at once).
    #[test]
    fn a_review_takes_the_base_over_from_the_git_tab() {
        let dir = git_repo("inline-review", &[("a.txt", "one\ntwo\n")]);
        std::fs::write(dir.join("a.txt"), "one\nTWO\n").unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(&dir).args(args).output().unwrap()
        };
        git(&["add", "a.txt"]);
        std::fs::write(dir.join("a.txt"), "ONE\nTWO\n").unwrap();
        let mut ed = editor("");
        ed.session.root = dir.clone();
        ed.git_auto = true;
        ed.open(&dir.join("a.txt")).unwrap();
        settle_until(&mut ed, |ed| ed.doc().git_base.is_some());
        feed_events(&mut ed, " g");
        settle_until(&mut ed, |ed| ed.doc().inline.is_some());
        assert!(ed.doc().review_base.as_ref().is_some_and(|b| *b == "one\nTWO\n"), "the index's");
        crate::codereview::start_in(&mut ed, dir.clone(), None);
        settle_until(&mut ed, |ed| ed.session.code_review.is_some() && ed.doc().own_base());
        assert!(ed.doc().review_base.as_ref().is_some_and(|b| *b == "one\ntwo\n"), "HEAD's, the review's");
        assert!(ed.doc().inline.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Panes P3 step 5: the tab's other file rows show what they can — a deleted file nothing (no buffer, no error),
    /// an untracked one plain with nothing to land on, a staged row its file without moving the cursor to the
    /// changes the index doesn't have yet (those are the Changes row's).
    #[test]
    fn the_git_tabs_deleted_untracked_and_staged_rows_show_what_they_can() {
        let repo = git_repo("inline-rows", &[("a.txt", "one\ntwo\nthree\n"), ("d.txt", "gone\n")]);
        let git = |args: &[&str]| {
            std::process::Command::new("git").arg("-C").arg(&repo).args(args).output().unwrap()
        };
        std::fs::write(repo.join("a.txt"), "ONE\ntwo\nthree\n").unwrap();
        git(&["add", "a.txt"]);
        std::fs::write(repo.join("a.txt"), "ONE\ntwo\nTHREE\n").unwrap();
        std::fs::remove_file(repo.join("d.txt")).unwrap();
        std::fs::write(repo.join("n.txt"), "new\n").unwrap();
        let mut ed = editor("");
        ed.session.root = repo.clone();
        ed.git_auto = true;
        feed_events(&mut ed, " g");
        settle_until(&mut ed, |ed| ed.session.git_status.is_some());
        // (onto the row for `rel` in its section, by `j` from the row above it)
        let onto = |ed: &mut Editor, rel: &str, staged: bool| {
            let s = ed.session.sidebar.as_mut().unwrap();
            let at = s.lines().iter().position(|l| {
                matches!(&l.target, crate::sidebar::Target::Change { rel: r, staged: st, file: true, .. }
                    if r == rel && *st == staged)
            });
            s.selected = at.expect(rel) - 1;
            feed_events(ed, "j");
        };
        let (docs, first) = (ed.docs.len(), ed.doc().id);
        onto(&mut ed, "d.txt", false);
        assert!(ed.docs.len() == docs && ed.doc().id == first, "a deleted file: nothing opened");
        assert!(!ed.toasts.iter().any(|t| t.kind == ToastKind::Error), "nor an error");
        onto(&mut ed, "n.txt", false);
        assert_eq!(ed.doc().path.as_deref(), Some(repo.join("n.txt").as_path()), "an untracked one, shown");
        let s = ed.session.sidebar.as_ref().unwrap();
        assert!(s.land.is_none() && ed.doc().inline.is_none(), "nothing to land on, nothing inline");
        onto(&mut ed, "a.txt", true);
        assert_eq!(ed.doc().path.as_deref(), Some(repo.join("a.txt").as_path()));
        settle_until(&mut ed, |ed| ed.doc().git_base.is_some() && ed.doc().git_version == ed.doc().version());
        assert_eq!(ed.doc().git_hunks.len(), 1, "its unstaged change");
        assert_eq!(ed.cursor_line(), 0, "the staged row doesn't land on it");
        std::fs::remove_dir_all(&repo).ok();
    }

    /// The reviewer (step 5): in a code review with the git tab in front, a paste while the tab shows a file that
    /// isn't the review's inline stays out of the chat — that file isn't under review.
    #[test]
    fn a_paste_over_the_git_tabs_inline_file_in_a_review_stays_out_of_the_chat() {
        let dir = git_repo("paste-inline", &[("a.txt", "one\n"), ("b.txt", "x\n")]);
        std::fs::write(dir.join("a.txt"), "ONE\n").unwrap();
        let mut ed = editor("");
        ed.session.root = dir.clone();
        ed.git_auto = true;
        crate::chat::open(&mut ed);
        ed.session.leave(At::Chat);
        crate::codereview::start_in(&mut ed, dir.clone(), None);
        settle_until(&mut ed, |ed| ed.session.code_review.is_some() && ed.doc().own_base());
        // (b.txt changed after the review took its files: not the review's)
        std::fs::write(dir.join("b.txt"), "X\n").unwrap();
        ed.open(&dir.join("b.txt")).unwrap();
        settle_until(&mut ed, |ed| ed.doc().git_base.is_some());
        feed_events(&mut ed, " g");
        settle_until(&mut ed, |ed| ed.doc().inline.is_some());
        ed.handle_paste("for the file".into());
        let c = ed.session.chat.as_ref().unwrap();
        assert!(c.input.is_empty() && !ed.session.is_at(At::Chat), "{:?}", c.input);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The reviewer (step 5): `r` in the git tab says it reads until the status lands — one job, one step.
    #[test]
    fn r_in_the_git_tab_says_it_reads_until_the_status_lands() {
        let repo = git_repo("refresh", &[("a.txt", "one\n")]);
        let mut ed = editor("");
        ed.session.root = repo.clone();
        feed_events(&mut ed, " g");
        settle_until(&mut ed, |ed| ed.session.git_status.is_some());
        let said = |ed: &Editor| ed.toasts.iter().any(|t| t.text.contains("reading git's status"));
        let before = ed.session.git_status.as_ref().unwrap().generation;
        feed_events(&mut ed, "r");
        assert!(said(&ed), "said while it reads");
        settle_until(&mut ed, |ed| ed.session.git_status.as_ref().is_some_and(|s| s.generation > before));
        assert!(!said(&ed), "gone once it landed");
        std::fs::remove_dir_all(&repo).ok();
    }

    /// The reviewer (step 5): where a view's marks come from — Claude's edit shown over the git tab's inline diff, a
    /// review's or `:diff`'s base over both.
    #[test]
    fn a_shown_edit_ranks_over_the_git_tabs_inline_diff_not_over_a_review() {
        use crate::codeview::Changes;
        let mut ed = editor("a\nb\n");
        let d = ed.doc_mut();
        let index = std::sync::Arc::new("a\nB\n".to_string());
        d.git_base = Some(index.clone());
        d.set_inline_base(Some(index));
        assert_eq!(Changes::of(d), Changes::Base, "the tab's, alone");
        let h = crate::replay::Hunk { lines: 1..2, removed: vec!["B".into()], added: vec!["b".into()] };
        d.shown = Some(vec![h]);
        assert_eq!(Changes::of(d), Changes::Shown, "Claude's edit over the tab's diff");
        d.set_review_base(Some(ropey::Rope::from_str("x\n")));
        assert_eq!(Changes::of(d), Changes::Base, "a review's or `:diff`'s over it");
    }

    /// A frame at 160×24 as the terminal shows it — its cells' attributes too (an italic name).
    fn frame_cells(ed: &mut Editor) -> vt100::Parser {
        let mut bytes = Vec::new();
        crate::term::render(ed, &mut bytes, 160, 24).unwrap();
        let mut parser = vt100::Parser::new(24, 160, 0);
        parser.process(&bytes);
        parser
    }

    /// Whether `name`'s first letter on row `y` of the frame is drawn in italic (None: not on that row).
    fn italic_on(p: &vt100::Parser, y: u16, name: &str, nth: usize) -> Option<bool> {
        let row = p.screen().rows(0, 160).nth(y as usize)?;
        let at = row.match_indices(name).nth(nth)?.0;
        let x = row[..at].chars().count() as u16;
        Some(p.screen().cell(y, x)?.italic())
    }

    /// The user's decision (2026-10-22): the files the git tab passes over go in one preview slot, as VS Code's —
    /// passing three leaves one preview and no other buffer, its name in italic; `esc code` into it and a move keep
    /// it a preview; an edit makes it a buffer of its own, and so does `enter` on its row (upright in the tabs).
    #[test]
    fn files_passed_over_in_the_git_tab_share_one_preview() {
        let repo = git_repo("preview", &[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")]);
        for f in ["a", "b", "c"] {
            std::fs::write(repo.join(format!("{f}.txt")), format!("{}\n", f.to_uppercase())).unwrap();
        }
        let mut ed = editor("");
        ed.session.root = repo.clone();
        ed.git_auto = true;
        feed_events(&mut ed, " g");
        settle_until(&mut ed, |ed| ed.session.git_status.is_some());
        let files = |ed: &Editor| -> Vec<(String, bool)> {
            let name =
                |d: &crate::document::Document| d.path.as_ref()?.file_name()?.to_str().map(str::to_string);
            ed.buffers().into_iter().filter_map(|i| Some((name(&ed.docs[i])?, ed.docs[i].preview))).collect()
        };
        // b, c, b, a: one preview, the last
        feed_events(&mut ed, "jjkk");
        assert_eq!(files(&ed), [("a.txt".to_string(), true)], "one preview, no other buffer");
        let p = frame_cells(&mut ed);
        assert_eq!(italic_on(&p, 0, "a.txt", 0), Some(true), "its name in italic in the path bar");
        // `esc code` into it, a move: still a preview
        feed_events(&mut ed, "<esc>w");
        assert!(ed.doc().preview, "looking at it keeps it a preview");
        // An edit: a buffer of its own
        feed_events(&mut ed, "ix<esc>");
        assert!(!ed.doc().preview, "edited: its own");
        // Back in the tab, onto b: a preview beside it, a.txt kept and upright, b in italic in the tabs
        feed_events(&mut ed, " gj");
        assert_eq!(files(&ed), [("a.txt".to_string(), false), ("b.txt".to_string(), true)]);
        let p = frame_cells(&mut ed);
        assert_eq!(italic_on(&p, 0, "a.txt", 0), Some(false), "the kept one upright in the tabs");
        assert_eq!(italic_on(&p, 0, "b.txt", 1), Some(true), "the preview in italic in the tabs");
        // `enter` on its row: its own
        feed_events(&mut ed, "<ret>");
        assert!(!ed.doc().preview && ed.doc().path.as_deref() == Some(repo.join("b.txt").as_path()));
        let p = frame_cells(&mut ed);
        assert_eq!(italic_on(&p, 0, "b.txt", 0), Some(false), "upright once it's yours");
        // (b's hunk row, then c: the third a preview, the two kept stay)
        feed_events(&mut ed, "jj");
        let names: Vec<(String, bool)> = files(&ed);
        assert_eq!(names.iter().filter(|f| f.1).count(), 1, "{names:?}");
        assert!(names.contains(&("c.txt".to_string(), true)) && names.len() == 3, "{names:?}");
        // `space b`: the preview last, said so
        feed_events(&mut ed, " b");
        let p = ed.picker.as_ref().expect("space b");
        let last = p.items().last().unwrap();
        assert!(last.label.ends_with("c.txt") && last.hint == "preview", "{:?}", last.label);
        std::fs::remove_dir_all(&repo).ok();
    }

    /// …and with the mouse: a click on a file's row shows it as the preview, the tab taking the keys; the same row
    /// again soon after — a double-click — makes it a buffer of its own.
    #[test]
    fn a_click_previews_a_git_tab_file_and_a_double_click_keeps_it() {
        use crate::event::{Mouse, MouseKind};
        let repo = git_repo("preview-click", &[("a.txt", "a\n"), ("b.txt", "b\n")]);
        std::fs::write(repo.join("b.txt"), "B\n").unwrap();
        let mut ed = editor("");
        ed.session.root = repo.clone();
        ed.git_auto = true;
        feed_events(&mut ed, " g<esc>");
        settle_until(&mut ed, |ed| ed.session.git_status.is_some());
        let p = frame_cells(&mut ed);
        let y = p.screen().rows(0, 160).position(|r| r.contains("b.txt")).expect("its row") as u16;
        let click = |ed: &mut Editor| {
            ed.handle_event(Event::Mouse(Mouse { kind: MouseKind::Down, x: 6, y, alt: false, ctrl: false }))
        };
        click(&mut ed);
        assert!(
            ed.doc().preview && ed.session.is_at(At::Side),
            "shown as the preview, the tab with the keys"
        );
        assert_eq!(ed.doc().path.as_deref(), Some(repo.join("b.txt").as_path()));
        click(&mut ed);
        assert!(!ed.doc().preview, "a double-click: its own");
        std::fs::remove_dir_all(&repo).ok();
    }

    /// The review rail the same: the files the review shows (`m`: viewed, the next) share one preview; `enter` on a
    /// row keeps that file; a double-click on a row too.
    #[test]
    fn files_the_review_shows_share_one_preview_until_one_is_kept() {
        use crate::event::{Mouse, MouseKind};
        let dir = git_repo("preview-rail", &[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")]);
        for f in ["a", "b", "c"] {
            std::fs::write(dir.join(format!("{f}.txt")), format!("{}\n", f.to_uppercase())).unwrap();
        }
        let mut ed = editor("");
        ed.session.root = dir.clone();
        ed.git_auto = true;
        crate::codereview::start_in(&mut ed, dir.clone(), None);
        settle_until(&mut ed, |ed| ed.session.code_review.is_some() && ed.doc().own_base());
        ed.session.at = At::Rail;
        let shown = |ed: &Editor| {
            ed.buffers()
                .into_iter()
                .filter(|&i| ed.docs[i].path.is_some())
                .map(|i| ed.docs[i].preview)
                .collect::<Vec<_>>()
        };
        // (each file's diff there before its `m`: a mark made before it waits on that buffer, which is kept then)
        for _ in 0..3 {
            settle_until(&mut ed, |ed| ed.doc().own_base() && ed.doc().git_version == ed.doc().version());
            feed_events(&mut ed, "m");
        }
        assert_eq!(shown(&ed), [true], "three files shown, one buffer: the preview");
        feed_events(&mut ed, "<ret>");
        assert_eq!(shown(&ed), [false], "enter: its own");
        // A click on another row: the preview beside it; the same row again: kept
        ed.session.at = At::Rail;
        let p = frame_cells(&mut ed);
        let kept = ed.doc().path.clone().unwrap();
        let other = ["a.txt", "b.txt", "c.txt"].into_iter().find(|f| dir.join(f) != kept).unwrap();
        let y = p.screen().rows(0, 160).position(|r| r.contains(other)).expect("its row") as u16;
        let click = |ed: &mut Editor| {
            ed.handle_event(Event::Mouse(Mouse { kind: MouseKind::Down, x: 6, y, alt: false, ctrl: false }))
        };
        click(&mut ed);
        assert_eq!(shown(&ed), [false, true], "a click: the preview");
        click(&mut ed);
        assert_eq!(shown(&ed), [false, false], "a double-click: kept");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The user (2026-10-22): the rail's `j`/`k` preview, as the git tab's moves — moving onto a file shows it, the
    /// rail keeping the keys (a binary row's buffer too). The reviewer: `enter` on a binary row keeps nothing — its
    /// buffer comes from a worker, the review's own — never the preview that was in front meanwhile.
    #[test]
    fn the_rails_moves_preview_and_enter_on_a_binary_row_keeps_nothing_else() {
        let dir = git_repo("preview-rail-jk", &[("a.txt", "a\n"), ("b.bin", "\0a"), ("c.txt", "c\n")]);
        std::fs::write(dir.join("a.txt"), "A\n").unwrap();
        std::fs::write(dir.join("b.bin"), "\0b").unwrap();
        std::fs::write(dir.join("c.txt"), "C\n").unwrap();
        let mut ed = editor("");
        ed.session.root = dir.clone();
        ed.git_auto = true;
        crate::codereview::start_in(&mut ed, dir.clone(), None);
        settle_until(&mut ed, |ed| ed.session.code_review.is_some() && ed.doc().own_base());
        assert_eq!(ed.doc().path.as_deref(), Some(dir.join("a.txt").as_path()), "on its first file");
        crate::codereview::rail_focus(&mut ed);
        assert!(ed.session.is_at(At::Rail));
        let binary = |ed: &Editor| ed.doc().title.as_deref() == Some("b.bin · binary");
        let on = |ed: &Editor, f: &str| ed.doc().path.as_deref() == Some(dir.join(f).as_path());
        // `j` onto the binary row: its buffer in front once read, the rail keeping the keys
        feed_events(&mut ed, "j");
        settle_until(&mut ed, binary);
        assert!(ed.session.is_at(At::Rail), "the rail keeps the keys");
        // `j` onto c.txt: the preview, a.txt's place taken
        feed_events(&mut ed, "j");
        assert!(on(&ed, "c.txt") && ed.doc().preview && ed.session.is_at(At::Rail));
        assert!(
            !ed.docs.iter().any(|d| d.path.as_deref() == Some(dir.join("a.txt").as_path())),
            "one preview"
        );
        // `k` back onto the binary row, `enter` before its buffer comes: c.txt, in front meanwhile, stays a preview
        feed_events(&mut ed, "k<ret>");
        assert!(on(&ed, "c.txt") && ed.doc().preview, "not kept: the binary row's enter keeps nothing");
        settle_until(&mut ed, binary);
        let c = ed.docs.iter().find(|d| d.path.as_deref() == Some(dir.join("c.txt").as_path())).unwrap();
        assert!(c.preview, "still the preview once the binary's buffer came");
        // A double-click on the binary row keeps nothing either: c.txt again in front, then the row clicked twice
        crate::codereview::rail_focus(&mut ed);
        feed_events(&mut ed, "j");
        assert!(on(&ed, "c.txt") && ed.doc().preview);
        // (its row where the rail drew it — the buffer tabs name `b.bin · binary` too)
        frame_cells(&mut ed);
        let r = ed.session.code_review.as_ref().unwrap();
        let b = r.files.iter().position(|f| f.rel == "b.bin").unwrap();
        let y = ed.screen.review_rail.borrow().iter().find(|r| r.1 == b).expect("its row").0 as u16;
        for _ in 0..2 {
            use crate::event::{Mouse, MouseKind};
            ed.handle_event(Event::Mouse(Mouse { kind: MouseKind::Down, x: 6, y, alt: false, ctrl: false }));
        }
        assert!(ed.toasts.iter().any(|t| t.step == Some("review-binary")), "the click reached the row: read");
        assert!(on(&ed, "c.txt") && ed.doc().preview, "a double-click on the binary row keeps nothing");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The user (2026-10-22): `space j` lists a replaced preview's jumps too, by its path, as `C-o` reaches them —
    /// picking one opens it again as the preview.
    #[test]
    fn the_jump_picker_lists_a_replaced_previews_jumps_and_opens_one_as_the_preview() {
        let repo = git_repo("preview-jump-picker", &[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")]);
        for f in ["a", "b", "c"] {
            std::fs::write(repo.join(format!("{f}.txt")), format!("{}\n", f.to_uppercase())).unwrap();
        }
        let mut ed = editor("");
        ed.session.root = repo.clone();
        ed.git_auto = true;
        feed_events(&mut ed, " g");
        settle_until(&mut ed, |ed| ed.session.git_status.is_some());
        // b, then c: b's buffer closed, its jump parked
        feed_events(&mut ed, "jj<esc>");
        assert!(!ed.docs.iter().any(|d| d.path.as_deref() == Some(repo.join("b.txt").as_path())));
        feed_events(&mut ed, " j");
        let p = ed.picker.as_ref().expect("space j");
        let b = p.items().iter().find(|i| i.label.starts_with("b.txt:")).expect("b.txt listed");
        assert!(matches!(b.action, crate::picker::Action::JumpParked { .. }), "by its path: {:?}", b.label);
        assert_eq!(b.hint, "B", "its line's text as it was parked");
        feed_events(&mut ed, "b.txt<ret>");
        assert!(ed.picker.is_none());
        assert_eq!(ed.doc().path.as_deref(), Some(repo.join("b.txt").as_path()), "picked: b.txt again");
        assert!(ed.doc().preview, "as the preview");
        std::fs::remove_dir_all(&repo).ok();
    }

    /// The reviewer (step 5): a replaced preview's spots are its session's — a jump in another session (its prune)
    /// leaves them, and the marks under the first session's jumps into open files too; back there `C-o` walks them.
    /// A `C-o` that can't go says so.
    #[test]
    fn a_sessions_parked_jumps_outlive_another_sessions_jumps() {
        let repo = git_repo("preview-sessions", &[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")]);
        for f in ["a", "b", "c"] {
            std::fs::write(repo.join(format!("{f}.txt")), format!("{}\n", f.to_uppercase())).unwrap();
        }
        let other = crate::scratch::Scratch::new("preview-sessions-b");
        let other = std::fs::canonicalize(&other).unwrap();
        std::fs::write(other.join("x.txt"), "x\ny\n").unwrap();
        let mut ed = editor("");
        ed.session.root = repo.clone();
        ed.git_auto = true;
        ed.open(&repo.join("a.txt")).unwrap();
        let on = |ed: &Editor, f: &str| ed.doc().path.as_deref() == Some(repo.join(f).as_path());
        // In the first session: from a.txt, the tab passes b and c — b replaced, parked
        feed_events(&mut ed, " g");
        settle_until(&mut ed, |ed| ed.session.git_status.is_some());
        feed_events(&mut ed, "jj<esc>");
        assert!(on(&ed, "c.txt") && ed.session.jumps_parked.len() == 1);
        let first = ed.session.id;
        // Another session: a file, a jump there (its list pruned)
        let id = ed.session_add(other.clone());
        assert!(ed.session_switch(id));
        ed.open(&other.join("x.txt")).unwrap();
        feed_events(&mut ed, "jgg");
        ed.push_jump();
        // Back: `C-o` to b, as the preview, then to a.txt where it was
        assert!(ed.session_switch(first));
        feed_events(&mut ed, "<C-o>");
        assert!(on(&ed, "b.txt") && ed.doc().preview, "b.txt again: its parked spot kept");
        feed_events(&mut ed, "<C-o>");
        assert!(on(&ed, "a.txt"), "a.txt: its mark kept through the other session's jumps");
        // A jump whose file is gone with nothing kept of it: said, not silent
        feed_events(&mut ed, " gjj<esc>");
        ed.session.jumps_parked.clear();
        ed.status = None;
        feed_events(&mut ed, "<C-o>");
        let said = ed.toasts.iter().any(|t| t.text.contains("was closed"));
        assert!(said, "{:?}", ed.toasts.iter().map(|t| &t.text).collect::<Vec<_>>());
        std::fs::remove_dir_all(&repo).ok();
    }

    /// The reviewer (step 5): a preview replaced keeps its place in the jump lists — `C-o` walks back through the files
    /// the git tab showed, each opened again as the preview, `C-i` forward again; one another pane shows isn't
    /// closed when the next comes — it's that pane's, a buffer of its own, said.
    #[test]
    fn a_replaced_preview_keeps_its_jumps_and_one_another_pane_shows_stays() {
        let repo = git_repo("preview-jumps", &[("a.txt", "a\n"), ("b.txt", "b\n"), ("c.txt", "c\n")]);
        for f in ["a", "b", "c"] {
            std::fs::write(repo.join(format!("{f}.txt")), format!("{}\n", f.to_uppercase())).unwrap();
        }
        let mut ed = editor("");
        ed.session.root = repo.clone();
        ed.git_auto = true;
        feed_events(&mut ed, " g");
        settle_until(&mut ed, |ed| ed.session.git_status.is_some());
        let on = |ed: &Editor, f: &str| ed.doc().path.as_deref() == Some(repo.join(f).as_path());
        let open = |ed: &Editor| ed.docs.iter().filter(|d| d.path.is_some()).count();
        // b, then c: b's buffer closed, its place in the jump list kept
        feed_events(&mut ed, "jj<esc>");
        assert!(on(&ed, "c.txt") && ed.doc().preview && open(&ed) == 1);
        feed_events(&mut ed, "<C-o>");
        assert!(on(&ed, "b.txt") && ed.doc().preview, "C-o: b.txt again, as the preview");
        assert_eq!(open(&ed), 1, "c.txt's place taken in turn");
        ed.jump_forward(1);
        assert!(on(&ed, "c.txt") && ed.doc().preview, "C-i: c.txt again");
        // Another pane shows c.txt: the tab moving on leaves it open, its own, said
        feed_events(&mut ed, "<C-w>v");
        feed_events(&mut ed, " gk");
        assert!(on(&ed, "b.txt") && ed.doc().preview);
        let c = ed.docs.iter().find(|d| d.path.as_deref() == Some(repo.join("c.txt").as_path()));
        assert!(c.is_some_and(|d| !d.preview), "c.txt stays, a buffer of its own");
        let c = c.unwrap().id;
        assert!(ed.session.views.iter().any(|v| v.doc == c && v.id != ed.session.focus), "the other pane's");
        let said = ed.toasts.iter().any(|t| t.text.contains("c.txt stays open") && t.text.contains("pane#"));
        assert!(said, "{:?}", ed.toasts.iter().map(|t| &t.text).collect::<Vec<_>>());
        std::fs::remove_dir_all(&repo).ok();
    }

    /// A conflicted file in the git panel lists its conflicts; `t` on one keeps theirs.
    #[test]
    fn git_panel_settles_a_conflict() {
        let repo = git_repo("hubconflict", &[("m.txt", "a\n")]);
        std::fs::write(repo.join("m.txt"), "a\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> b\nz\n").unwrap();
        let mut ed = editor("");
        ed.open(&repo.join("m.txt")).unwrap();
        ed.handle_event(crate::event::Event::Resize);
        let sb = crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone());
        ed.session.sidebar = Some(sb);
        feed(&mut ed, "gg g");
        settle_until(&mut ed, |ed| {
            ed.session.git_status.is_some() && ed.session.sidebar.as_ref().unwrap().aim.is_none()
        });
        ed.session.sidebar.as_mut().unwrap().from_code = false;
        ed.sidebar_sync();
        let rows: Vec<crate::sidebar::Target> =
            ed.session.sidebar.as_ref().unwrap().lines().iter().map(|l| l.target.clone()).collect();
        let at = rows
            .iter()
            .position(|t| matches!(t, crate::sidebar::Target::Conflict { .. }))
            .expect("its conflict");
        ed.session.sidebar.as_mut().unwrap().selected = at;
        feed(&mut ed, "t");
        assert_eq!(ed.doc().text.to_string(), "a\ntheirs\nz\n");
        std::fs::remove_dir_all(&repo).ok();
    }

    /// `space g` from a file with nothing changed under the cursor: there's no hunk for the next key to act
    /// on, so the tab has the keys — `s` stages the row it's on.
    #[test]
    fn space_g_with_nothing_here_gives_the_tab_the_keys() {
        let repo = git_repo("gitnothinghere", &[("a.txt", "one\n"), ("b.txt", "two\n")]);
        std::fs::write(repo.join("b.txt"), "two\nmore\n").unwrap();
        let mut ed = editor("");
        ed.session.root = repo.clone(); // (a file's git is the session's repository's — `gitrepo`)
        ed.git_auto = true;
        ed.open(&repo.join("a.txt")).unwrap();
        settle_until(&mut ed, |ed| ed.doc().git_base.is_some());
        let sb = crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone());
        ed.session.sidebar = Some(sb);
        feed(&mut ed, "<space>g");
        settle_until(&mut ed, |ed| ed.session.git_status.as_ref().is_some_and(|s| !s.unstaged.is_empty()));
        feed(&mut ed, "s");
        settle_until(&mut ed, |ed| ed.session.git_status.as_ref().is_some_and(|s| !s.staged.is_empty()));
        assert_eq!(ed.session.git_status.as_ref().unwrap().staged[0].rel, "b.txt");
        assert!(ed.session.is_at(crate::session::At::Side), "the tab keeps the keys");
        std::fs::remove_dir_all(&repo).ok();
    }

    /// From the review: `d d` on the Changes heading when every change is untracked deletes those files —
    /// one by one, and the folders they leave empty — never the repository.
    #[test]
    fn git_tab_discarding_new_files_never_takes_the_repository() {
        let repo = git_repo("gitdiscard", &[("a.txt", "one\n")]);
        std::fs::create_dir_all(repo.join("new/deep")).unwrap();
        std::fs::write(repo.join("new/deep/x.txt"), "x\n").unwrap();
        std::fs::write(repo.join("b.txt"), "b\n").unwrap();
        let mut ed = editor("");
        ed.session.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone()));
        ed.session.at = crate::session::At::Side;
        ed.git_refresh();
        settle_until(&mut ed, |ed| ed.session.git_status.is_some());
        ed.sidebar_sync();
        ed.session.sidebar.as_mut().unwrap().selected = 0; // the heading
        feed(&mut ed, "dd");
        settle_until(&mut ed, |_| !repo.join("b.txt").exists());
        settle_until(&mut ed, |_| !repo.join("new").exists());
        assert!(repo.join(".git").is_dir() && repo.join("a.txt").is_file(), "the repository stays");
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Merge conflicts: `]m` goes to one, `space g t` keeps theirs (one undo step), `space g a` both.
    #[test]
    fn conflicts_keep_a_side() {
        const SRC: &str =
            "a\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> b\nz\n<<<<<<< HEAD\n1\n=======\n2\n>>>>>>> b\n";
        // (`space g` opens the git tab too, which reads the status where it's rooted — not this repository)
        let repo = git_repo("conflicts", &[("a.txt", "a\n")]);
        let in_repo = |text: &str| {
            let mut ed = editor(text);
            let sb = crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone());
            ed.session.sidebar = Some(sb);
            ed
        };
        let mut ed = in_repo(SRC);
        feed(&mut ed, "]m");
        assert_eq!(ed.cursor_line(), 1);
        feed(&mut ed, " gt");
        assert_eq!(text(&ed), "a\ntheirs\nz\n<<<<<<< HEAD\n1\n=======\n2\n>>>>>>> b\n");
        assert_eq!(ed.doc().conflicts.len(), 1);
        // (the tab keeps the keys after its quick key — esc back to the code: #50)
        feed(&mut ed, "<esc>u");
        assert_eq!(text(&ed), SRC, "one undo step");
        feed(&mut ed, "]m]m");
        assert_eq!(ed.cursor_line(), 1, "wraps around");
        feed(&mut ed, "[m ga");
        assert!(text(&ed).ends_with("z\n1\n2\n"), "{}", text(&ed));
        // Below the last conflict: nothing out of sight above is taken
        let mut ed = in_repo("<<<<<<< a\nx\n=======\ny\n>>>>>>> b\nz\n");
        feed(&mut ed, "G gt");
        assert!(text(&ed).starts_with("<<<<<<<"), "{}", text(&ed));
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Conflicts are found in any file — past 1 MB too (the first look at a file is always made).
    #[test]
    fn conflicts_in_a_big_file() {
        let big = "x\n".repeat(600_000) + "<<<<<<< a\n1\n=======\n2\n>>>>>>> b\n";
        let mut ed = editor(&big);
        ed.conflicts_sync();
        assert_eq!(ed.doc().conflicts[0].start, 600_000);
    }

    /// `:diff` shows the buffer against HEAD the way a review does (the base's removed lines, `]g`),
    /// `:diff-off` goes back.
    #[test]
    fn diff_against_head() {
        let repo = git_repo("diffview", &[("a.txt", "one\ntwo\nthree\n")]);
        std::fs::write(repo.join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();
        let mut ed = editor("");
        ed.session.root = repo.clone(); // (a file's git is the session's repository's — `gitrepo`)
        ed.open(&repo.join("a.txt")).unwrap();
        feed(&mut ed, ":diff<ret>");
        settle_until(&mut ed, |ed| ed.doc().review_base.is_some());
        assert_eq!(ed.doc().diff_of.as_deref(), Some("HEAD"));
        assert_eq!(ed.doc().git_hunks.len(), 2, "changed TWO, added four");
        assert_eq!(ed.cursor_line(), 1, "on the first change");
        feed(&mut ed, ":diff-off<ret>");
        assert!(ed.doc().review_base.is_none() && ed.doc().diff_of.is_none());
        std::fs::remove_dir_all(&repo).ok();
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git").arg("-C").arg(dir).args(args).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    }

    /// Events until `done`, or 5 s — without failing (the caller says what didn't come).
    fn wait_for(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done(ed) && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
    }

    /// The editor's root — here a repository of the test's own, not the folder the tests run in (later a
    /// session's worktree) — is where everything runs and what paths are from: the terminal, a task (its
    /// folder from the root), Claude, `:sh`, a pipe, the git tab, the branch watch, `:open` and `:diff` of
    /// relative paths, the file picker, the search. Every miss is listed (one run shows them all). The
    /// stand-ins write `pwd` into a folder of the test's own, by absolute path.
    #[test]
    fn a_root_of_its_own_is_where_everything_runs() {
        let root = git_repo("own-root", &[("a.txt", "tarae-root-marker\n"), ("b.txt", "old\n")]);
        git_in(&root, &["switch", "-q", "-c", "tarae-own-root"]);
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let scratch = crate::scratch::Scratch::new("own-root-out");
        let out = scratch.to_path_buf();
        let here = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        assert!(!here.starts_with(&root) && !root.starts_with(&here), "the tests run elsewhere");
        let pwd = |name: &str| format!("pwd -P > '{}'", out.join(name).display());
        let mut ed = editor("");
        ed.session.root = root.clone();
        let mut wrong: Vec<String> = Vec::new();
        let mut ran_in = |ed: &mut Editor, what: &str, want: &Path| {
            let file = out.join(what);
            wait_for(ed, |_| std::fs::read_to_string(&file).is_ok_and(|s| s.ends_with('\n')));
            let got = std::fs::read_to_string(&file).map(|s| s.trim().to_string()).ok();
            if got.as_deref() != want.to_str() {
                wrong.push(format!("{what} ran in {got:?}"));
            }
        };
        // What runs (the terminal panel's part skipped inside macOS's sandbox — the rest still checked)
        if !crate::sandbox::no_pty_here("editor::tests::a_root_of_its_own_is_where_everything_runs") {
            ed.terminal_run(vec!["/bin/sh".into(), "-c".into(), pwd("terminal")], Vec::new());
            ran_in(&mut ed, "terminal", &root);
        }
        ed.task_start(crate::task::Task { name: "where".into(), cmd: pwd("task"), cwd: "sub".into() });
        ran_in(&mut ed, "task", &root.join("sub"));
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), format!("{}; cat > /dev/null", pwd("claude"))];
        crate::chat::open(&mut ed);
        ran_in(&mut ed, "claude", &root);
        ed.session.leave(crate::session::At::Chat);
        crate::typed::execute(&mut ed, &format!("sh {}", pwd("sh"))).unwrap();
        ran_in(&mut ed, "sh", &root);
        ed.shell_pipe(crate::shell::Pipe::To, &pwd("pipe"));
        ran_in(&mut ed, "pipe", &root);
        // Git: the tab's repository and the branch watched
        feed(&mut ed, "<space>g");
        wait_for(&mut ed, |ed| ed.session.git_status.is_some());
        let top = ed.session.git_status.as_ref().map(|s| s.top.clone());
        if top.as_deref() != Some(root.as_path()) {
            wrong.push(format!("the git tab read {top:?}"));
        }
        feed(&mut ed, "<esc>");
        ed.session.sidebar = None;
        crate::git::watch(ed.events.sender(), ed.session.root.clone());
        wait_for(&mut ed, |ed| ed.session.git_branch.is_some());
        if ed.session.git_branch.as_deref() != Some("tarae-own-root") {
            wrong.push(format!("the branch watched is {:?}", ed.session.git_branch));
        }
        // Relative paths are from the root
        let a = root.join("a.txt");
        match crate::typed::execute(&mut ed, "open a.txt") {
            Ok(()) if ed.doc().path.as_deref() == Some(a.as_path()) => {}
            r => wrong.push(format!(":open a.txt: {r:?}, {:?}", ed.doc().path)),
        }
        if ed.doc().path.as_deref() == Some(a.as_path()) {
            crate::typed::execute(&mut ed, "diff b.txt").unwrap();
            wait_for(&mut ed, |ed| ed.doc().diff_of.is_some() || ed.status.is_some());
            if ed.doc().diff_of.as_deref() != Some("b.txt") {
                wrong.push(format!(":diff b.txt: {:?} {:?}", ed.doc().diff_of, ed.status));
            }
            crate::typed::execute(&mut ed, "diff-off").ok();
        }
        // The file picker and the search look in the root
        feed(&mut ed, "<space>f");
        wait_for(&mut ed, |ed| ed.picker.as_ref().is_some_and(|p| !p.loading));
        let listed = ed.picker.as_ref().is_some_and(|p| {
            p.items().iter().any(|i| matches!(&i.action, crate::picker::Action::Open(p) if *p == a))
        });
        if !listed {
            wrong.push("the file picker doesn't list a.txt".into());
        }
        feed(&mut ed, "<esc><space>/tarae-root-marker");
        wait_for(&mut ed, |ed| ed.picker.as_ref().is_some_and(|p| !p.searching && p.current().is_some()));
        let hit = ed.picker.as_ref().and_then(|p| p.current()).map(|i| i.action.clone());
        if !matches!(&hit, Some(crate::picker::Action::Goto { path, .. }) if *path == a) {
            wrong.push(format!("the search found {hit:?}"));
        }
        feed(&mut ed, "<esc>");
        assert!(wrong.is_empty(), "not in the root:\n{}", wrong.join("\n"));
        drop(ed);
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&out).ok();
    }

    /// The whole review: changes against HEAD file by file (a changed file, a deleted one, a new one), `]g`
    /// running on across files and back, a draft comment, viewed, sent to Claude with its diff, ended.
    #[test]
    fn review_walks_the_changes_and_sends_the_comments() {
        let dir = git_repo(
            "walk",
            &[
                ("a.rs", "fn a() {}\nlet x = 1;\nlet y = 2;\nlet z = 3;\nfn end() {}\n"),
                ("b.rs", "old\nfile\n"),
            ],
        );
        std::fs::write(dir.join("a.rs"), "fn a() {}\nlet x = 10;\nlet y = 2;\nfn end() {}\n").unwrap();
        std::fs::remove_file(dir.join("b.rs")).unwrap();
        std::fs::write(dir.join("c.rs"), "new\n").unwrap();
        let mut ed = editor("");
        let ready =
            |ed: &Editor| ed.doc().git_version == ed.doc().version() && ed.doc().review_base.is_some();
        crate::codereview::start_in(&mut ed, dir.clone(), None);
        let said = |ed: &Editor| ed.toasts.iter().map(|t| t.text.clone()).collect::<Vec<_>>();
        assert_eq!(said(&ed), ["review: gathering changes…"], "a step under way, said");
        settle_until(&mut ed, |ed| ed.session.code_review.is_some() && ready(ed));
        // The step over: its toast goes — what came of it shows on its own (U9)
        assert_eq!(said(&ed), ["review: 3 files changed — ]g walks the changes"]);
        let r = ed.session.code_review.as_ref().unwrap();
        let files: Vec<_> =
            r.files.iter().map(|f| (f.rel.as_str(), f.added, f.removed, f.new, f.gone)).collect();
        assert_eq!(
            files,
            [("a.rs", 1, 2, false, false), ("b.rs", 0, 2, false, true), ("c.rs", 1, 0, true, false)]
        );
        assert_eq!(ed.doc().path.as_deref(), Some(dir.join("a.rs").as_path()));
        assert_eq!(ed.cursor_line(), 1, "on the first change");
        // ]G: the deletion (a line from the change, one hunk to `]g`), then ]g on into the deleted file, the
        // new one, and [g back
        feed(&mut ed, "]G");
        assert_eq!(ed.cursor_line(), 3, "where `let z` was");
        feed(&mut ed, "]g");
        settle_until(&mut ed, ready);
        assert_eq!(ed.doc().title.as_deref(), Some("b.rs (deleted)"));
        assert_eq!(ed.doc().review_base.as_ref().map(|b| b.to_string()).as_deref(), Some("old\nfile\n"));
        feed(&mut ed, "]g");
        settle_until(&mut ed, |ed| ready(ed) && ed.doc().path.is_some());
        assert_eq!(ed.doc().path.as_deref(), Some(dir.join("c.rs").as_path()));
        feed(&mut ed, "[g");
        settle_until(&mut ed, |ed| ready(ed) && ed.doc().path.is_none());
        // Back on a.rs: a draft comment (nothing sent), viewed
        crate::codereview::show(&mut ed, 0, false);
        settle_until(&mut ed, |ed| ready(ed) && ed.doc().path.is_some());
        feed(&mut ed, "<space>nwhy 10?<ret>");
        assert!(ed.session.chat.is_none(), "a draft doesn't go anywhere yet");
        let n = &ed.session.notes.list[0];
        assert!(n.thread[0].draft && !n.waiting);
        assert_eq!(n.lines, 1..2);
        feed(&mut ed, "<space>vm");
        let r = ed.session.code_review.as_ref().unwrap();
        assert!(r.viewed(&ed, &r.files[0]));
        // Send: one message with the comment and its diff
        let log = dir.join("stdin.log");
        let result = serde_json::json!({"type": "result", "result": "addressed"});
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do printf '%s\\n' \"$line\" >> '{}'; case \"$line\" in '{{\"message\"'*) printf '%s\\n' '{result}';; esac; done",
                log.display()
            ),
        ];
        feed(&mut ed, "<space>vs");
        assert!(!ed.session.notes.list[0].thread[0].draft && ed.session.notes.list[0].waiting);
        settle_until(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().busy());
        assert!(!ed.session.notes.list[0].waiting);
        let sent = std::fs::read_to_string(&log).unwrap();
        for want in [
            "<review of=\\\"uncommitted\\\" comments=\\\"1\\\">",
            "-let x = 1;",
            "+let x = 10;",
            "you: why 10?",
        ] {
            assert!(sent.contains(want), "{want} in {sent}");
        }
        // Ending it with files not viewed: asked first, on a card naming them (#10) — n keeps reviewing
        ed.session.leave(crate::session::At::Chat);
        feed(&mut ed, "<space>vq");
        assert!(ed.session.code_review.is_some(), "not ended without a word");
        let card = ed.offers.first().map(|o| (o.title.clone(), o.question.clone())).unwrap();
        let name = ed.session.label().to_string();
        assert_eq!(card.0, format!("End {name}'s review?"));
        let head = format!(
            "{name}'s review · uncommitted · 1 of 3 viewed\n\n2 files not viewed — end it anyway?\n\n  b.rs\n  c.rs"
        );
        assert!(card.1.starts_with(&head), "{}", card.1);
        feed(&mut ed, "n");
        assert!(ed.session.code_review.is_some() && ed.offers.is_empty());
        // y: ended — back to the index as the diff base
        feed(&mut ed, "<space>vqy");
        assert!(ed.session.code_review.is_none());
        assert!(ed.docs.iter().all(|d| d.review_base.is_none()));
        // Opened again: a.rs is still viewed — its diff is what it was (#9)
        crate::codereview::start_in(&mut ed, dir.clone(), None);
        settle_until(&mut ed, |ed| ed.session.code_review.is_some() && ready(ed));
        let viewed = |ed: &Editor| {
            let r = ed.session.code_review.as_ref().unwrap();
            r.files.iter().filter(|f| r.viewed(ed, f)).map(|f| f.rel.clone()).collect::<Vec<_>>()
        };
        assert_eq!(viewed(&ed), ["a.rs"], "kept past the review");
        // Its diff changes: the mark goes; back as it was, it's there again
        feed(&mut ed, "ggix<esc>");
        assert!(viewed(&ed).is_empty(), "a changed diff isn't viewed");
        feed(&mut ed, "u");
        assert_eq!(viewed(&ed), ["a.rs"], "the same diff again");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Against a branch: everything since this branch left `main` — its commits and what's uncommitted.
    #[test]
    fn review_against_a_branch() {
        let dir = git_repo("branch", &[("a.rs", "one\n"), ("b.rs", "two\n")]);
        git_in(&dir, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(dir.join("a.rs"), "one\nmore\n").unwrap();
        git_in(&dir, &["commit", "-q", "-am", "work"]);
        std::fs::write(dir.join("b.rs"), "TWO\n").unwrap();
        let mut ed = editor("");
        // Started from b.rs: the review opens there, not on the first file
        ed.open(&dir.join("b.rs")).unwrap();
        crate::codereview::start_in(&mut ed, dir.clone(), Some("main".into()));
        settle_until(&mut ed, |ed| ed.session.code_review.is_some());
        let r = ed.session.code_review.as_ref().unwrap();
        assert_eq!(r.base.label(), "since main");
        let files: Vec<_> = r.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(files, ["a.rs", "b.rs"], "the committed change and the uncommitted one");
        assert_eq!(ed.doc().path.as_deref(), Some(dir.join("b.rs").as_path()));
        // Nothing to review against HEAD's own branch point → no review
        let mut ed = editor("");
        git_in(&dir, &["checkout", "-q", "b.rs"]);
        crate::codereview::start_in(&mut ed, dir.clone(), Some("feature".into()));
        settle_until(&mut ed, |ed| {
            ed.status.is_some() && ed.status.as_ref().unwrap().0.contains("no changes")
        });
        assert!(ed.session.code_review.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Frictions 13, #1: a turn's review is the last turn's files only — it says so, and how many other files
    /// differ from HEAD (changed before the turn, untracked too), with the key that reviews them all. A turn
    /// that changed every one of them leaves nothing to say.
    #[test]
    fn a_turns_review_says_what_else_differs_from_head() {
        use crate::checkpoint::{Checkpoint, Touched};
        let dir = git_repo("beyond", &[("a.rs", "one\n"), ("b.rs", "two\n"), ("c.rs", "three\n")]);
        for f in ["a.rs", "b.rs", "c.rs", "d.rs"] {
            std::fs::write(dir.join(f), "changed\n").unwrap();
        }
        let touched = |f: &str, before: &str| Touched {
            path: dir.join(f),
            before: Some(ropey::Rope::from_str(before)),
            after: ropey::Rope::from_str("changed\n"),
            ..Default::default()
        };
        let mut ed = editor("");
        ed.session.root = dir.clone();
        ed.session.checkpoints.list.push(Checkpoint::new("fix a".into(), vec![touched("a.rs", "one\n")]));
        crate::codereview::start_turn(&mut ed, 0);
        settle_until(&mut ed, |ed| ed.session.code_review.is_some());
        let r = ed.session.code_review.as_ref().unwrap();
        assert_eq!(r.base.label(), "last turn");
        assert_eq!(r.files.len(), 1);
        assert_eq!(r.beyond, Some((3, "space v v".to_string())), "b.rs, c.rs and the untracked d.rs");
        // A turn over all four: nothing beyond it
        let all = vec![
            touched("a.rs", "one\n"),
            touched("b.rs", "two\n"),
            touched("c.rs", "three\n"),
            Touched { before: None, ..touched("d.rs", "") },
        ];
        let mut ed = editor("");
        ed.session.root = dir.clone();
        ed.session.checkpoints.list.push(Checkpoint::new("fix all".into(), all));
        crate::codereview::start_turn(&mut ed, 0);
        settle_until(&mut ed, |ed| ed.session.code_review.is_some());
        let r = ed.session.code_review.as_ref().unwrap();
        assert_eq!((r.files.len(), &r.beyond), (4, &None));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Frictions 13, #2: code moved from a file to a new one — the review's counts are git's (every line), and
    /// it knows how many of them moved: the 30 lines in both files, each move once in all.
    #[test]
    fn a_review_counts_the_lines_that_moved() {
        let body: String = (1..=30).map(|i| format!("let value_{i} = compute({i});\n")).collect();
        let dir = git_repo("moved", &[("a.rs", &format!("fn a() {{}}\n{body}"))]);
        std::fs::write(dir.join("a.rs"), "fn a() {}\nuse crate::b::*;\nb::all();\n").unwrap();
        std::fs::write(dir.join("b.rs"), &body).unwrap();
        let mut ed = editor("");
        crate::codereview::start_in(&mut ed, dir.clone(), None);
        settle_until(&mut ed, |ed| ed.session.code_review.is_some());
        let r = ed.session.code_review.as_ref().unwrap();
        let counts: Vec<_> =
            r.files.iter().map(|f| (f.rel.as_str(), f.added, f.removed, r.moved_in(f))).collect();
        assert_eq!(counts, [("a.rs", 2, 30, 30), ("b.rs", 30, 0, 30)]);
        assert_eq!(r.moved_total(), 30);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A diff against the index still under way when the review's base lands mustn't replace the review's
    /// diff (a staged file: index = worktree → no hunks → the file showed as plain code).
    #[test]
    fn review_diff_survives_a_stale_index_diff() {
        let dir = git_repo("stale", &[("a.txt", "a\nb\nc\n")]);
        std::fs::write(dir.join("a.txt"), "a\nB\nc\n").unwrap();
        git_in(&dir, &["add", "a.txt"]);
        let mut ed = editor("");
        ed.open(&dir.join("a.txt")).unwrap();
        // What `git_load_base` leaves (git_auto is off in tests): the index as the base, a diff due
        ed.doc_mut().git_base = Some(std::sync::Arc::new("a\nB\nc\n".into()));
        ed.doc_mut().git_version = u64::MAX;
        ed.git_schedule();
        let stale = ed.events.recv_timeout(std::time::Duration::from_secs(5)).expect("the index diff");
        crate::codereview::start_in(&mut ed, dir.clone(), None);
        settle_until(&mut ed, |ed| {
            ed.doc().review_base.is_some() && ed.doc().git_version == ed.doc().version()
        });
        assert_eq!(ed.doc().git_hunks.len(), 1, "the review's diff");
        ed.handle_event(stale);
        assert_eq!(ed.doc().git_hunks.len(), 1, "the review's diff stays");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The index base asked for when a review's file opened, landing before the review's own base, stays
    /// out of it.
    #[test]
    fn review_files_take_no_index_base() {
        let dir = git_repo("nobase", &[("a.txt", "a\n")]);
        std::fs::write(dir.join("a.txt"), "A\n").unwrap();
        let mut ed = editor("");
        ed.git_auto = true;
        ed.open(&dir.join("a.txt")).unwrap(); // asks for the index base
        let id = ed.doc().id;
        let mut r =
            crate::codereview::CodeReview::for_test(crate::codereview::Base::Head, dir.clone(), Vec::new());
        r.docs.push(id);
        ed.session.code_review = Some(r);
        let ev = ed.events.recv_timeout(std::time::Duration::from_secs(5)).expect("the index base");
        ed.handle_event(ev);
        assert!(ed.doc().git_base.is_none(), "the review's base is on its way");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #5: a file's viewed mark comes through `:restart` — still checked against its diff: the same diff
    /// reviewed again is viewed; once the file changes, it isn't.
    #[test]
    fn viewed_marks_come_through_a_restart() {
        let dir = git_repo("viewed-restart", &[("a.txt", "a\nb\nc\n"), ("b.txt", "x\n")]);
        std::fs::write(dir.join("a.txt"), "a\nB\nc\n").unwrap();
        std::fs::write(dir.join("b.txt"), "x\ny\n").unwrap();
        let review = |ed: &mut Editor| {
            crate::codereview::start_in(ed, dir.clone(), None);
            settle_until(ed, |ed| {
                ed.session.code_review.is_some() && ed.doc().review_base.is_some() && !ed.doc().loading
            });
        };
        let viewed = |ed: &Editor| -> Vec<String> {
            let r = ed.session.code_review.as_ref().unwrap();
            r.files.iter().filter(|f| r.viewed(ed, f)).map(|f| f.rel.clone()).collect()
        };
        let mut ed = editor("");
        review(&mut ed);
        let first = ed.session.code_review.as_ref().unwrap().files[0].rel.clone();
        feed(&mut ed, "<space>vm");
        assert_eq!(viewed(&ed), std::slice::from_ref(&first));
        let state = crate::restart::snapshot(&mut ed);
        // The next tarae
        let mut ed = editor("");
        crate::restart::restore(&mut ed, &state);
        review(&mut ed);
        assert_eq!(viewed(&ed), std::slice::from_ref(&first), "the same diff: still viewed");
        // Changed since: the mark goes
        let file = dir.join(&first);
        std::fs::write(&file, format!("{}more\n", std::fs::read_to_string(&file).unwrap())).unwrap();
        let mut ed = editor("");
        crate::restart::restore(&mut ed, &state);
        review(&mut ed);
        assert!(viewed(&ed).is_empty(), "{:?}", viewed(&ed));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A big file's review base arriving after its text: its diff is worked out on that text, not on the
    /// empty placeholder the base was asked for over.
    #[test]
    fn review_of_a_file_still_loading() {
        let dir = git_repo("loading", &[("a.txt", "a\nb\nc\n")]);
        std::fs::write(dir.join("a.txt"), "a\nB\nc\n").unwrap();
        let path = dir.join("a.txt");
        let mut ed = editor("");
        let id = ed.alloc_id();
        ed.add_doc(Document::placeholder(id, &path)); // what `open` makes of a file over 1 MB
        let file = crate::codereview::File {
            rel: "a.txt".into(),
            path: path.clone(),
            added: 1,
            removed: 1,
            new: false,
            gone: false,
            binary: false,
        };
        ed.session.code_review = Some(crate::codereview::CodeReview::for_test(
            crate::codereview::Base::Head,
            dir.clone(),
            vec![file],
        ));
        crate::codereview::show(&mut ed, 0, false); // the base is asked for over the placeholder
        ed.finish_loading(id, Ok(ropey::Rope::from_str("a\nB\nc\n")), std::time::Duration::ZERO);
        settle_until(&mut ed, |ed| {
            ed.doc().review_base.is_some() && ed.doc().git_version == ed.doc().version()
        });
        let h = &ed.doc().git_hunks;
        assert_eq!(h.len(), 1, "{h:?}");
        assert_eq!((h[0].kind, h[0].lines.clone()), (crate::git::HunkKind::Modified, 1..2));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A new file lands on its top — its whole text is one change, and the cursor at its end showed the
    /// last line.
    #[test]
    fn review_lands_on_the_top_of_a_new_file() {
        let dir = git_repo("newtop", &[("a.txt", "a\n")]);
        let body: String = (0..200).map(|i| format!("line {i}\n")).collect();
        std::fs::write(dir.join("n.txt"), &body).unwrap();
        let mut ed = editor("");
        ed.viewport = (20, 80);
        crate::codereview::start_in(&mut ed, dir.clone(), None);
        settle_until(&mut ed, |ed| {
            ed.doc().review_base.is_some() && ed.doc().git_version == ed.doc().version()
        });
        assert_eq!(ed.doc().path.as_deref(), Some(dir.join("n.txt").as_path()));
        assert_eq!(ed.cursor_line(), 0, "on the change's first line");
        assert_eq!(ed.doc().top, 0);
        let sel = ed.doc().selection().primary();
        assert_eq!((sel.from(), sel.to()), (0, body.len()), "the whole change selected");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// If claude can't be reached, a reply or a new note is taken back instead of waiting forever.
    #[test]
    fn a_note_claude_never_saw_is_taken_back() {
        let (dir, main, ..) = follow_project("unreachable");
        let mut ed = editor("");
        ed.open(&main).unwrap();
        ed.config.llm.command = "/nonexistent/no-llm".into();
        feed(&mut ed, "<space>nhello<ret>");
        assert!(ed.session.notes.list.is_empty(), "the new note went away");
        crate::notes::add(&mut ed, &main, 0..1, crate::notes::By::Author, "hi");
        feed(&mut ed, "<space>nreply<ret>");
        let n = &ed.session.notes.list[0];
        assert_eq!((n.thread.len(), n.waiting), (1, false), "the reply was taken back");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A note rides its mark: lines added above push it down, and the jump list's pruning keeps it.
    #[test]
    fn notes_follow_edits() {
        let (dir, _, _, lib) = follow_project("edits");
        let mut ed = editor("");
        ed.open(&lib).unwrap();
        let args = serde_json::json!({"path": lib, "line": 5, "end_line": 6, "text": "here"});
        crate::notes::tool_call(&mut ed, &args).unwrap();
        feed(&mut ed, "ggOa<ret>b<esc>");
        ed.push_jump();
        crate::notes::attach(&mut ed);
        assert_eq!(ed.session.notes.list[0].lines, 6..8);
        feed(&mut ed, "]n");
        assert_eq!(ed.cursor_line(), 6);
        // Across files, in (file, line) order, wrapping
        let util = dir.join("util.rs");
        crate::notes::tool_call(&mut ed, &serde_json::json!({"path": util, "line": 2, "text": "there"}))
            .unwrap();
        feed(&mut ed, "]n");
        assert_eq!(ed.doc().path.as_deref(), Some(util.as_path()), "the next note is in another file");
        feed(&mut ed, "]n");
        assert_eq!((ed.doc().path.as_deref(), ed.cursor_line()), (Some(lib.as_path()), 6), "wraps around");
        feed(&mut ed, "[n");
        assert_eq!(ed.doc().path.as_deref(), Some(util.as_path()));
        ed.open(&lib).unwrap();
        ed.goto_line(6);
        assert!(crate::notes::tool_call(&mut ed, &serde_json::json!({"path": lib, "text": "x"})).is_err());
        assert!(crate::notes::tool_call(&mut ed, &serde_json::json!({"reply_to": 9, "text": "x"})).is_err());
        feed(&mut ed, ":note-close<ret>");
        assert_eq!(ed.session.notes.list.len(), 1, "only the one here");
        feed(&mut ed, ":notes-clear<ret>");
        assert!(ed.session.notes.list.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn g_goes_to_the_changes_of_the_edit_shown() {
        let mut ed = editor("one\ntwo\nthree\nfour\nfive\n");
        let d = ed.doc_mut();
        // (the index says line 3 is new; the edit follow shows changed line 4)
        let kind = crate::git::HunkKind::Added;
        d.git_hunks = vec![crate::git::Hunk { kind, lines: 2..3, removed: 0, base: 2..2 }];
        let edit =
            crate::replay::Hunk { lines: 3..4, removed: vec!["FOUR".into()], added: vec!["four".into()] };
        d.shown = Some(vec![edit]);
        feed(&mut ed, "]g");
        let line =
            |ed: &Editor| mv::line_of(&ed.doc().text, ed.doc().selection().primary().cursor(&ed.doc().text));
        assert_eq!(line(&ed), 3, "the shown edit's change, not the index's");
        feed(&mut ed, "[g");
        assert_eq!(line(&ed), 3, "no change of the edit before it");
    }

    #[test]
    fn splits_focus_scroll_and_close() {
        let mut ed = editor(&"x\n".repeat(100));
        ed.handle_event(crate::event::Event::Resize); // first sync
        let first = ed.session.focus;
        feed(&mut ed, "<C-w>v");
        assert_eq!(ed.session.views.len(), 2);
        assert_ne!(ed.session.focus, first, "focus on the new pane");
        // Scroll down in the new pane → the old pane stays put
        ed.doc_mut().top = 50;
        ed.handle_event(crate::event::Event::Resize);
        feed(&mut ed, "<C-w>w");
        assert_eq!(ed.session.focus, first);
        assert_eq!(ed.doc().top, 0, "scroll is per pane");
        feed(&mut ed, "<C-w>w");
        assert_eq!(ed.doc().top, 50);
        // :q closes only the pane (quits only on the last pane)
        feed(&mut ed, ":q<ret>");
        assert_eq!(ed.session.views.len(), 1);
        assert!(!ed.should_quit);
        feed(&mut ed, "<C-w>s<C-w>o");
        assert_eq!(ed.session.views.len(), 1, "only this pane");
    }

    /// `focus_side_panel` (the palette's) takes the keys to the side panel and back from wherever they are; a closed
    /// panel opens at the tab it last showed. `C-w h/j/k/l` are the code's windows' — in the panel they leave it be.
    /// `C-w e` isn't the side panel's (user, 2026-10-23: it floats and docks, one meaning).
    #[test]
    fn focus_side_panel_to_the_side_panel_and_back() {
        use crate::sidebar::Mode;
        let mut ed = editor("x\n");
        feed(&mut ed, "<C-w>v");
        let came = ed.session.focus;
        let focus = |ed: &Editor| ed.session.sidebar.as_ref().map(|s| (s.mode, ed.session.is_at(At::Side)));
        feed(&mut ed, "<C-w>e");
        assert_eq!(focus(&ed), None, "C-w e: not the side panel's");
        ed.side_toggle_focus();
        assert_eq!(focus(&ed), Some((Mode::Files, true)), "closed: opens, at files the first time");
        ed.side_toggle_focus();
        assert_eq!(focus(&ed), Some((Mode::Files, false)), "back to the code");
        assert_eq!(ed.session.focus, came, "the window it came from");
        feed(&mut ed, " o");
        feed(&mut ed, "<C-w>h");
        assert_eq!(focus(&ed), Some((Mode::Outline, true)), "in the panel: a window move doesn't leave it");
        feed(&mut ed, "q");
        assert!(ed.session.sidebar.is_none());
        ed.side_toggle_focus();
        assert_eq!(focus(&ed), Some((Mode::Outline, true)), "reopens at the tab it last showed");
    }

    #[test]
    fn tutor_is_a_throwaway_buffer() {
        let mut ed = editor("");
        feed(&mut ed, ":tutor<ret>");
        assert_eq!(ed.doc().display_name(&ed.session.root), "tutor");
        assert!(ed.doc().text.to_string().starts_with("tarae tutorial"));
        feed(&mut ed, "ihello<esc>:q<ret>");
        assert!(ed.should_quit, "an edited tutorial doesn't block quitting");
    }

    /// The tutor is text (uxaudit 5) — its colours are spans kept on the buffer, and they follow what you do
    /// in it: typed before them they move along, undone they come back, deleted whole they go.
    #[test]
    fn the_tutors_colours_follow_its_edits() {
        let mut ed = editor("");
        feed(&mut ed, ":tutor<ret>");
        assert!(ed.doc().syntax.is_none() && !ed.doc().text.to_string().contains('`'));
        let painted = |ed: &Editor| -> Vec<String> {
            let (d, t) = (ed.doc(), ed.doc().text.to_string());
            d.paint.iter().map(|s| t[s.0..s.1].to_string()).collect()
        };
        let before = painted(&ed);
        assert_eq!(before[..2], ["tarae tutorial", "space"]);
        feed(&mut ed, "ggiHello <esc>");
        assert_eq!(painted(&ed), before, "typed before them, they move along");
        feed(&mut ed, "u");
        assert_eq!(painted(&ed), before, "undone, they're where they were");
        feed(&mut ed, "gg/space<ret>d");
        let mut want = before.clone();
        want.remove(1);
        assert_eq!(painted(&ed), want, "a key deleted whole takes its colour along");
        feed(&mut ed, "u");
        assert_eq!(painted(&ed), before, "undone, the key is back with its colour");
        feed(&mut ed, "U");
        assert_eq!(painted(&ed), want, "redone, gone again");
        // Typed right after a key still painted (`space ?`): the key keeps its span, what's typed stays plain
        feed(&mut ed, "gg/space \\?<ret>aX<esc>");
        assert!(ed.doc().text.to_string().contains("space ?X"), "appended after it");
        assert_eq!(painted(&ed), want, "typed against its end, it's plain");
    }

    /// `q` closes `:granted` too — the other list whose letters are keys (uxaudit 5, as the session list's); a
    /// letter it has no use for is still said there.
    #[test]
    fn q_closes_the_list_of_grants_too() {
        let mut ed = editor("");
        ed.granted_open(Vec::new(), false);
        assert_eq!(ed.picker.as_ref().map(|p| (p.kind, p.keys)), Some((crate::granted::KIND, true)));
        feed(&mut ed, "x");
        let said = ed.messages.last().map(|m| m.2.clone()).unwrap_or_default();
        assert!(ed.picker.is_some() && said.contains("does nothing in the list of grants"), "{said}");
        feed(&mut ed, "q");
        assert!(ed.picker.is_none(), "q closed it");
    }

    /// The palette for a first hour (uxaudit 5): with nothing typed, what you ran from it comes first, then the
    /// commands a first hour asks for; typed, a word's start ranks above letters inside a word, and those above
    /// letters here and there.
    #[test]
    fn the_palette_lists_what_you_ran_then_the_common_ones_and_ranks_word_starts_first() {
        let mut ed = editor("");
        let rows = |ed: &Editor, n: usize| -> Vec<String> {
            let p = ed.picker.as_ref().unwrap();
            (0..n).filter_map(|r| p.item_at(r)).map(|i| i.label.clone()).collect()
        };
        feed(&mut ed, "<space>?");
        assert_eq!(rows(&ed, 3), ["Learn the keys (tutorial)", "Open a file", "Search in project"]);
        // Every one of a first hour's is in the palette, in its order — one renamed or misspelt would fall to the rest
        let common = crate::palette::COMMON;
        let p = ed.picker.as_ref().unwrap();
        let keys: Vec<String> = (0..common.len())
            .filter_map(|r| p.item_at(r))
            .filter_map(|i| crate::palette::key(&i.action))
            .collect();
        assert_eq!(keys, common);
        // Run one: the next palette starts with it
        feed(&mut ed, "line highlight<ret>");
        assert!(!ed.config.cursorline, "ran");
        feed(&mut ed, "<space>?");
        assert_eq!(rows(&ed, 2), ["Toggle current line highlight", "Learn the keys (tutorial)"]);
        // `side`: at a word's start first, inside a word next, scattered letters last
        feed(&mut ed, "<esc><space>?side");
        let p = ed.picker.as_ref().unwrap();
        assert!(
            p.words,
            "ranked by where the words sit (picker.rs's test: where nucleo alone ranks otherwise)"
        );
        let all: Vec<String> =
            (0..p.len()).filter_map(|r| p.item_at(r)).map(|i| i.label.to_lowercase()).collect();
        let first = |f: &dyn Fn(&str) -> bool| all.iter().position(|l| f(l)).expect("listed");
        let start = first(&|l| l.starts_with("side") || l.contains(" side"));
        let inside = first(&|l| l.contains("inside"));
        let scattered = first(&|l| !l.contains("side"));
        assert!(start == 0 && start < inside && inside < scattered, "{all:#?}");
    }

    #[test]
    fn remembers_file_positions() {
        let dir = std::env::temp_dir().join(format!("tarae-pos-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let mut ed = editor("");
        ed.open(&path).unwrap();
        feed(&mut ed, "jjl");
        // Instead of saving and reopening: record the position in the restore state; a new editor reads it
        let head = ed.doc().selection().primary().head;
        let key = std::fs::canonicalize(&path).unwrap();
        ed.restore = crate::restore::State::from_json(
            serde_json::json!({ "positions": { key.to_str().unwrap(): [head, 0] } }),
        );
        let restore = std::mem::take(&mut ed.restore);
        let mut ed2 = editor("");
        ed2.restore = restore;
        ed2.open(&path).unwrap();
        assert_eq!(ed2.doc().selection().primary().head, head, "back to the last position");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn search_highlights_count_preview_and_esc() {
        let mut ed = editor(&(0..40).map(|i| format!("line {i} foo\n")).collect::<String>());
        // Preview while typing: if the first match after the cursor is off screen, only the view moves
        // (Esc goes back)
        feed(&mut ed, "/line 30");
        ed.refresh_search(10);
        assert_eq!(ed.search_hits.as_ref().map(|h| h.matches.len()), Some(1));
        assert!(ed.doc().top > 20, "view moved to the match");
        feed(&mut ed, "<esc>");
        assert_eq!(ed.doc().top, 0, "Esc = original position");
        // Confirm → highlight all + which match this is
        feed(&mut ed, "/foo<ret>n");
        ed.refresh_search(10);
        let h = ed.search_hits.as_ref().unwrap();
        assert_eq!(h.matches.len(), 40);
        assert_eq!(h.current(ed.doc().selection().primary()), Some(2));
        // Esc in normal mode = highlight off
        feed(&mut ed, "<esc>");
        ed.refresh_search(10);
        assert!(ed.search_hits.is_none());
    }

    #[test]
    fn git_hunks_and_change_navigation() {
        let mut ed = editor("a\nB\nc\nd\nnew\n");
        ed.docs[0].git_base = Some(std::sync::Arc::new("a\nb\nc\nd\n".to_string()));
        ed.git_schedule();
        let ev = ed.events.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
        ed.handle_event(ev);
        let kinds: Vec<_> = ed.doc().git_hunks.iter().map(|h| (h.kind, h.lines.clone())).collect();
        assert_eq!(kinds, [(crate::git::HunkKind::Modified, 1..2), (crate::git::HunkKind::Added, 4..5)]);
        // Each change (`]G`); two lines apart they're one hunk to `]g`, as `git diff` shows them
        feed(&mut ed, "]G");
        assert_eq!(ed.doc().selection().primary(), Range::new(2, 4), "edited line selected");
        feed(&mut ed, "]G");
        assert_eq!(ed.doc().selection().primary(), Range::new(8, 12), "added line");
        feed(&mut ed, "[G");
        assert_eq!(ed.doc().selection().primary(), Range::new(2, 4));
        feed(&mut ed, "gg]g");
        assert_eq!(ed.doc().selection().primary(), Range::new(2, 12), "]g: the hunk");
    }

    #[test]
    fn grammar_offer_asks_once_and_answers_only_once_focused() {
        use crate::offer::{OfferState, What};
        let mut ed = editor("fn main() {}\n");
        ed.offer_popups = true;
        let rust = syntax::spec("rust").unwrap();
        ed.offer_grammar(rust);
        let o = ed.offers.first().expect("offer");
        assert_eq!(o.title, "Rust syntax colors");
        let What::Grammars { names, .. } = &o.what else { panic!("grammar offer") };
        assert!(names.contains(&"rust".to_string()));
        assert!(matches!(o.state, OfferState::Asking));
        // In insert mode y·n are characters
        feed(&mut ed, "i");
        feed(&mut ed, "n");
        assert_eq!(ed.offers.len(), 1);
        assert_eq!(ed.doc().text.to_string(), "nfn main() {}\n");
        feed(&mut ed, "<esc>");
        // In normal mode too, until it's focused (P3 §3.8: it came on its own) — `n` the code's
        feed(&mut ed, "n");
        assert_eq!(ed.offers.len(), 1);
        // Focused, n = not now; the same language isn't asked again
        feed(&mut ed, "<C-q>n");
        assert!(ed.offers.is_empty());
        ed.offer_grammar(rust);
        assert!(ed.offers.is_empty(), "once until quit");
        // A different language is asked — "Not now" via the mouse: a press focuses it, the next answers
        ed.offer_grammar(syntax::spec("python").unwrap());
        assert_eq!(ed.offers.first().map(|o| o.title.as_str()), Some("Python syntax colors"));
        ed.screen.offer_buttons = vec![(20, 50, 59, 'y'), (20, 62, 71, 'n')];
        let press = crate::event::Mouse {
            kind: crate::event::MouseKind::Down,
            x: 65,
            y: 20,
            alt: false,
            ctrl: false,
        };
        ed.handle_mouse(press);
        assert!(ed.offers.len() == 1 && ed.card_has_keys(), "focused, not answered");
        ed.handle_mouse(press);
        assert!(ed.offers.is_empty());
    }

    /// A command of several lines running with nothing printed yet: the task panel shows its first line and
    /// `…` — printed whole, its newlines went down the rows as a staircase (seen in the #84 screenshot).
    #[test]
    fn a_command_of_several_lines_waits_on_its_first() {
        let mut ed = editor("text\n");
        // (today's gate, for its card: in the sandbox it runs — P3)
        ed.config.llm.sandbox = false;
        ed.offer_popups = true;
        crate::chat::open(&mut ed);
        let generation = ed.session.chat.as_ref().unwrap().generation();
        let msg = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "run", "arguments": {"command": "sleep 3\necho second line"}}});
        assert!(crate::tools::mcp(&mut ed, generation, "c1", &msg).is_none());
        settle_until(&mut ed, |ed| ed.offers.iter().any(|o| matches!(o.what, crate::offer::What::Run(_))));
        let screen = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 30).unwrap();
            let mut parser = vt100::Parser::new(30, 140, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        screen(&mut ed);
        let o = ed.offers.first().map(|o| (o.id, o.rev)).unwrap();
        ed.card_focus = Some((o.0, o.1));
        answer(&mut ed, "y");
        settle_until(&mut ed, |ed| ed.session.task_run.is_some());
        assert!(ed.session.task_run.as_ref().unwrap().output.is_empty(), "nothing printed yet");
        let s = screen(&mut ed);
        assert!(s.contains("sleep 3 …") && !s.contains("echo second line"), "{s}");
        ed.task_close();
    }

    /// #84: Claude's run card cut to fit (the chat beside it, Claude asking mid-turn) says that `y` waits
    /// for all of it to be seen, and a `y` before that says why on screen — it was taken in silence.
    #[test]
    fn a_cut_run_card_says_y_waits_for_all_of_it() {
        // (a screen too short for the card's 12 lines: it's cut)
        let frame = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 18).unwrap();
            let mut parser = vt100::Parser::new(18, 140, 0);
            parser.process(&bytes);
            parser
        };
        let screen = |ed: &mut Editor| frame(ed).screen().contents();
        // The color of the card's cut row (its `↓`) — colors drawn though tarae's test runner sets NO_COLOR
        crossterm::style::force_color_output(true);
        let cut_color = |ed: &mut Editor| {
            let p = frame(ed);
            let s = p.screen().contents();
            let (row, line) = s.lines().enumerate().find(|(_, l)| l.contains('↓')).expect("the cut row");
            let col = line.chars().position(|c| c == '↓').unwrap();
            p.screen().cell(row as u16, col as u16).unwrap().fgcolor()
        };
        let mut ed = editor("text\n");
        // (today's gate, for its card: in the sandbox it runs — P3)
        ed.config.llm.sandbox = false;
        ed.offer_popups = true;
        crate::chat::open(&mut ed);
        let generation = ed.session.chat.as_ref().unwrap().generation();
        let command: String = (1..=12).map(|i| format!("echo step {i}")).collect::<Vec<_>>().join("\n");
        let msg = serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "run", "arguments": {"command": command}}});
        let r = crate::tools::mcp(&mut ed, generation, "c1", &msg);
        assert!(r.is_none(), "it waits for the user: {r:?}");
        settle_until(&mut ed, |ed| ed.offers.iter().any(|o| matches!(o.what, crate::offer::What::Run(_))));
        let s = screen(&mut ed);
        assert!(s.contains("more lines · y once you've seen it all — v"), "{s}");
        // (the hint row too, where `y`'s meaning was — frictions29 13: the row under the cut one said `y Once`)
        let hint_color = |ed: &mut Editor| {
            let p = frame(ed);
            let s = p.screen().contents();
            let (row, line) =
                s.lines().enumerate().find(|(_, l)| l.contains("y read it first")).expect("{s}");
            let col = line[..line.find("read it first").unwrap()].chars().count();
            p.screen().cell(row as u16, col as u16).unwrap().fgcolor()
        };
        let hint_plain = hint_color(&mut ed);
        // (narrow, a short form where the long one has no room — it took a row of its own at 80 columns, and the
        // row said `y Once`)
        let narrow = |ed: &mut Editor, w: u16| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, w, 18).unwrap();
            let mut parser = vt100::Parser::new(18, w, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        let s = narrow(&mut ed, 80);
        let said = s.contains("y read it first — v") || s.contains("y after v");
        assert!(said && !s.contains("y Once"), "80 columns:\n{s}");
        let plain = cut_color(&mut ed);
        let y_at = |ed: &Editor| ed.screen.offer_buttons.iter().find(|b| b.3 == 'y').map(|b| (b.0, b.1));
        let before = y_at(&ed);
        let o = ed.offers.first().map(|o| (o.id, o.rev)).unwrap();
        ed.card_focus = Some((o.0, o.1));
        answer(&mut ed, "y");
        assert_eq!(ed.offers.len(), 1, "y waits");
        let s = screen(&mut ed);
        assert!(ed.messages.last().is_some_and(|m| m.2.contains("v shows all of it")), "kept in :messages");
        // …and said on the card, where the eyes are: its cut row, in the warning color
        assert!(s.contains("more lines · y waits — v shows all of it"), "{s}");
        let warning = ed.theme.try_get("warning").and_then(|s| s.fg);
        assert_ne!(cut_color(&mut ed), plain, "the warning color, not the faint row's ({warning:?})");
        assert_ne!(hint_color(&mut ed), hint_plain, "its hint row in the warning color too");
        assert_eq!(y_at(&ed), before, "the card stays where it was (no wider)\n{}", screen(&mut ed));
        // Read whole: `y` has its meaning back, and the card keeps its width — its `y` where it was (review 7)
        screen(&mut ed);
        let waiting_at = y_at(&ed);
        ed.offer_whole.set(ed.offers.first().map(|o| (o.id, o.rev)));
        let s = screen(&mut ed);
        assert!(s.contains("y Run") && !s.contains("read it first"), "{s}");
        assert_eq!(y_at(&ed), waiting_at, "no narrower once read:\n{s}");
    }

    /// A card taller than the screen is still drawn — cut to fit, its buttons in sight, a row saying how much
    /// is left — and answerable: `y` waits until all of it has been seen (`v` opens it whole in a buffer,
    /// and it counts once that buffer's last line has been on screen); new text on the card has to be seen
    /// again. A card that fits takes `y` as before. (#14: an 88-line
    /// card wasn't drawn at all, so it couldn't be answered.)
    #[test]
    fn a_card_taller_than_the_screen_is_cut_and_answerable() {
        use crate::offer::{Offer, What};
        let screen = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 120, 30).unwrap();
            let mut parser = vt100::Parser::new(30, 120, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        let settled = |ed: &mut Editor| {
            let o = ed.offers.first().map(|o| (o.id, o.rev)).unwrap();
            ed.card_focus = Some((o.0, o.1));
        };
        let question: String =
            (1..=80).map(|i| format!("line {i} of what it runs")).collect::<Vec<_>>().join("\n");
        let mut ed = editor("text\n");
        let card = |q: &str| {
            Offer::new(
                "Trust this project's settings?".into(),
                q,
                "",
                String::new(),
                What::ProjectTrust { hash: [0; 32] },
            )
        };
        ed.push_offer(card(&question));
        let s = screen(&mut ed);
        assert!(s.contains("Trust this project's settings?") && s.contains("line 1 of what it runs"), "{s}");
        assert!(!s.contains("line 80 of what it runs"), "cut to fit");
        // (its hint row says what `y` waits for, in place of `y Trust` — frictions29 13)
        assert!(
            s.contains("more lines · y once you've seen it all — v") && s.contains("y read it first — v"),
            "{s}"
        );
        assert!(!s.contains("y Trust"), "{s}");
        assert!(ed.screen.offer_buttons.iter().any(|b| b.3 == 'y'), "y in sight");
        settled(&mut ed);
        // `y` before all of it was seen: not an answer
        feed(&mut ed, "y");
        assert_eq!(ed.offers.len(), 1, "y waits");
        assert!(screen(&mut ed).contains("more lines · y waits — v shows all of it"), "said on the card");
        // `v`: all of it in a buffer — opened isn't read: `y` still waits until its end has been on screen.
        // The keys go to it (#55: the side panel or the review's rail kept them, and keys did nothing)
        let side = crate::sidebar::Sidebar::new(crate::sidebar::Mode::Files, test_root());
        ed.session.sidebar = Some(side);
        ed.session.at = crate::session::At::Side;
        feed(&mut ed, "v");
        assert!(!ed.session.is_at(crate::session::At::Side), "the code has the keys");
        ed.session.sidebar = None;
        assert!(text(&ed).contains("line 80 of what it runs") && text(&ed).starts_with("Trust this project"));
        assert_eq!(ed.mode, Mode::Normal, "v didn't start a selection");
        let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert!(said.contains("ge to its end"), "the keymap's key (#55: it said G): {said}");
        let s = screen(&mut ed);
        assert!(!s.contains("line 80 of what it runs"), "its end is below the screen");
        // While its text is read, the card is a bar over the status line (U8: the screen's last row) — nothing
        // of it over the text. It keeps the keys meanwhile (the reading keys reach the buffer through it), and
        // says so
        let rows: Vec<&str> = s.lines().collect();
        let bar = rows[30 - 2];
        assert!(
            bar.contains("Trust this project's settings?")
                && bar.contains("ge to its end · esc goes back")
                && bar.contains("y Trust"),
            "the bar: {bar:?}\n{s}"
        );
        assert!(!s.contains("more lines"), "the card stepped aside: {s}");
        assert!(!ed.offer_seen_whole(), "its bar in sight isn't the card read");
        assert!(ed.screen.offer_buttons.iter().any(|b| b.3 == 'y' && b.0 == 28), "its y still clicks");
        // Another buffer: the card whole again; back: the bar
        let viewed = ed.doc().id;
        ed.new_scratch();
        let s = screen(&mut ed);
        assert!(s.contains("more lines · y once read to the end — ge there"), "whole again: {s}");
        ed.current = ed.docs.iter().position(|d| d.id == viewed).unwrap();
        assert!(!screen(&mut ed).contains("more lines"), "the bar again");
        feed(&mut ed, "y");
        assert_eq!(ed.offers.len(), 1, "y waits while the end is unseen");
        assert!(screen(&mut ed).contains("ge to its end first — y waits"), "said on the card's bar");
        // Scrolled to the end: seen — the card says so in that same frame, and `y` answers
        feed(&mut ed, "ge");
        let s = screen(&mut ed);
        assert!(s.contains("line 80 of what it runs"), "{s}");
        assert!(s.contains("y answers · esc goes back"), "the bar says y counts now: {s}");
        ed.handle_event(Event::Key("y".parse().unwrap()));
        assert!(ed.offers.is_empty(), "answered once seen whole");
        let card_buffers = |ed: &Editor| {
            ed.docs.iter().filter(|d| d.title.as_deref().is_some_and(|t| t.starts_with("card ·"))).count()
        };
        assert_eq!(card_buffers(&ed), 0, "its buffer went with the answer (#55)");
        // The same card again (a script run again, unchanged): read already (#55)
        ed.push_offer(card(&question));
        assert!(ed.offer_seen_whole(), "the same text, read to its end before");
        // New text on the card: seen again first (a click on y waits as well)
        screen(&mut ed);
        settled(&mut ed);
        ed.push_offer(card(&format!("{question}\nline 81, new")));
        screen(&mut ed);
        settled(&mut ed);
        let click = |ed: &mut Editor, k: char| {
            let (row, x0, _, _) = *ed.screen.offer_buttons.iter().find(|b| b.3 == k).unwrap();
            let (x, y) = (x0 as u16, row as u16);
            ed.handle_mouse(crate::event::Mouse {
                kind: crate::event::MouseKind::Down,
                x,
                y,
                alt: false,
                ctrl: false,
            });
        };
        click(&mut ed, 'y');
        assert_eq!(ed.offers.len(), 1, "a click on y waits too");
        click(&mut ed, 'v');
        assert!(text(&ed).contains("line 81, new"), "a click on its last row views it");
        screen(&mut ed);
        click(&mut ed, 'y');
        assert_eq!(ed.offers.len(), 1, "opened by a click isn't read either");
        feed(&mut ed, "n");
        assert!(ed.offers.is_empty(), "n answers it unseen");
        // One that fits: y at once, and v is the code's (select mode)
        ed.push_offer(card("a = \"b\""));
        screen(&mut ed);
        settled(&mut ed);
        feed(&mut ed, "y");
        assert!(ed.offers.is_empty());
    }

    /// #65: a card's `v` buffer goes when the card is answered — also one viewed before another card's `v`
    /// (only the last one viewed was closed: the first stayed in the tab row); a card waiting in a session
    /// behind keeps its own.
    #[test]
    fn every_cards_buffer_goes_with_its_answer() {
        use crate::offer::{Offer, What};
        let mut ed = editor("text\n");
        // (a file's buffer: the blank startup one would be handed over to the first view)
        ed.docs[0].path = Some(test_root().join("x.rs"));
        let long: String = (1..=80).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let card = |title: &str| {
            Offer::new(title.into(), &long, "", String::new(), What::ProjectTrust { hash: [0; 32] })
        };
        let card_buffers = |ed: &Editor| -> Vec<String> {
            let mut t: Vec<String> = ed
                .docs
                .iter()
                .filter_map(|d| d.title.clone().filter(|t| t.starts_with(crate::offer::CARD_VIEW)))
                .collect();
            t.sort();
            t
        };
        ed.push_offer(card("First card"));
        ed.push_offer(card("Second card"));
        ed.offer_view_all();
        ed.offer_to_front(1);
        ed.offer_view_all();
        ed.handle_event(Event::Resize);
        assert_eq!(card_buffers(&ed), ["card · First card", "card · Second card"]);
        // The second answered: its buffer goes; then the first's, viewed before it, too
        ed.offer_dismiss();
        ed.handle_event(Event::Resize);
        assert_eq!(card_buffers(&ed), ["card · First card"]);
        ed.offer_dismiss();
        ed.handle_event(Event::Resize);
        assert!(card_buffers(&ed).is_empty(), "{:?}", card_buffers(&ed));
        // A card parked in a session behind keeps its buffer
        ed.push_offer(card("Third card"));
        ed.offer_view_all();
        let o = ed.offers.remove(0);
        ed.session.parked.push(o);
        ed.handle_event(Event::Resize);
        assert_eq!(card_buffers(&ed), ["card · Third card"]);
        ed.session.parked.clear();
        ed.handle_event(Event::Resize);
        // E17: a card of the same title up elsewhere — another session's "Claude wants to run" — doesn't keep
        // the answered one's buffer
        ed.push_offer(card("Claude wants to run"));
        ed.offer_view_all();
        ed.session.parked.push(card("Claude wants to run"));
        ed.handle_event(Event::Resize);
        assert_eq!(card_buffers(&ed), ["card · Claude wants to run"]);
        ed.offer_dismiss();
        ed.handle_event(Event::Resize);
        assert!(card_buffers(&ed).is_empty(), "went with its card: {:?}", card_buffers(&ed));
    }

    /// What a weighty card asks you to decide is its most visible text: its first paragraph (a command, a
    /// path) or the lines it names (a trust card's settings) at full strength, what explains it faint. A
    /// light card stays as it was.
    #[test]
    fn what_a_card_decides_is_drawn_at_full_strength() {
        use crate::offer::{Offer, What};
        // (crossterm drops every color under NO_COLOR, which tarae's own runs set)
        crossterm::style::force_color_output(true);
        let mut ed = editor("text\n");
        let fg = |ed: &mut Editor, needle: &str| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 120, 30).unwrap();
            let mut parser = vt100::Parser::new(30, 120, 0);
            parser.process(&bytes);
            let screen = parser.screen();
            let (y, row) = screen.rows(0, 120).enumerate().find(|(_, r)| r.contains(needle)).expect(needle);
            let x = row[..row.find(needle).unwrap()].chars().count();
            screen.cell(y as u16, x as u16).unwrap().fgcolor()
        };
        // A run card: the command, then why
        ed.push_offer(Offer::new(
            "Claude wants to run".into(),
            "make deploy-all\n\nNo sandbox here — it runs with all your permissions.",
            "",
            String::new(),
            What::ProjectTrust { hash: [0; 32] },
        ));
        let (cmd, why) = (fg(&mut ed, "make deploy-all"), fg(&mut ed, "No sandbox here"));
        assert_ne!(cmd, why, "the command stands out");
        // Lines it names: the settings under the line naming the file
        ed.offers.clear();
        ed.push_offer(
            Offer::new(
                "Trust this project's settings?".into(),
                ".tarae.toml sets what runs commands as you:\n\n  task.x = rm-it\n\nIgnored, the rest applies.",
                "",
                String::new(),
                What::ProjectTrust { hash: [1; 32] },
            )
            .deciding(2..3),
        );
        let (head, item, foot) = (
            fg(&mut ed, ".tarae.toml sets"),
            fg(&mut ed, "task.x = rm-it"),
            fg(&mut ed, "Ignored, the rest"),
        );
        assert_eq!(head, foot, "both faint");
        assert_eq!(item, cmd, "the setting as strong as a command");
        assert_ne!(item, head);
        // A light card: its question faint, as before
        ed.offers.clear();
        ed.offer_popups = true;
        ed.push_offer(Offer::new(
            "Rust syntax colors".into(),
            "Download and build the grammar?",
            "",
            String::new(),
            What::Grammars { lang: "Rust".into(), names: vec![], then: None },
        ));
        assert_eq!(fg(&mut ed, "Download and build"), head);
    }

    /// On the card for running outside the sandbox, what yes means — "with all your permissions" — is in the
    /// warning color (that of the panel's `▲ sandbox blocked` row); Claude's reason stays faint (its own
    /// words); the command is at full strength.
    #[test]
    fn running_outside_the_sandbox_warns_in_its_color() {
        use serde_json::json;
        // (crossterm drops every color under NO_COLOR, which tarae's own runs set)
        crossterm::style::force_color_output(true);
        let mut ed = editor("text\n");
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "run",
            "arguments": {"command": "cargo fetch", "outside_sandbox": true, "reason": "the network is closed"}}});
        assert!(crate::tools::mcp(&mut ed, 0, "c1", &msg).is_none(), "it waits for the user");
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 120, 30).unwrap();
        let mut parser = vt100::Parser::new(30, 120, 0);
        parser.process(&bytes);
        let screen = parser.screen();
        let fg = |needle: &str| {
            let (y, row) = screen.rows(0, 120).enumerate().find(|(_, r)| r.contains(needle)).expect(needle);
            let x = row[..row.find(needle).unwrap()].chars().count();
            screen.cell(y as u16, x as u16).unwrap().fgcolor()
        };
        let theme = |key: &str| match ed.theme.try_get(key).and_then(|s| s.fg) {
            Some(crossterm::style::Color::Rgb { r, g, b }) => vt100::Color::Rgb(r, g, b),
            c => panic!("{key}: {c:?}"),
        };
        assert_eq!(fg("Outside the sandbox it runs"), theme("warning"), "what yes means: the warning color");
        assert_eq!(fg("Its reason: the network"), theme("ui.virtual"), "Claude's words: faint");
        let cmd = fg("cargo fetch");
        assert!(cmd != theme("warning") && cmd != theme("ui.virtual"), "the command: full strength");
    }

    /// The screen as text reads a real frame the way a terminal shows it (checked against a terminal emulator:
    /// the same characters, rows trimmed alike).
    #[test]
    fn the_screen_as_text_is_what_a_terminal_shows() {
        let mut ed = editor("fn main() {\n    println!(\"타래 — 🎉 wide\");\n}\n");
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 100, 30).unwrap();
        let mut parser = vt100::Parser::new(30, 100, 0);
        parser.process(&bytes);
        let text = crate::screen::text(&bytes, 100, 30).expect("read");
        // (vt100 keeps the blanks a background was painted on; the text has none at a row's end)
        let shown: Vec<String> =
            parser.screen().contents().lines().map(|l| l.trim_end().to_string()).collect();
        assert_eq!(text, shown.join("\n").trim_end());
        assert!(text.contains("println!(\"타래 — 🎉 wide\");"), "{text}");
        assert_eq!(ed.last_frame.as_ref().map(|f| (f.1, f.2)), Some((100, 30)), "the frame is kept");
    }

    /// `C-s` in the chat input: the screen as the user saw it (the last frame, as text) goes with the next
    /// message — the line over the input says so, Backspace in the empty input takes it off; what's sent
    /// carries it, and the message's chip row opens a copy of it. `:chat-screen` opens the chat for it.
    #[test]
    fn the_screen_goes_with_the_next_message() {
        let scratch = crate::scratch::Scratch::new("chat-screen");
        let dir = scratch.to_path_buf();
        let log = dir.join("stdin.log");
        let mut ed = editor("fn main() {}\n");
        // (the copy of what went: in this test's folder)
        ed.sent_dir = Some(dir.join("sent"));
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!("while read -r line; do printf '%s\\n' \"$line\" >> '{}'; done", log.display()),
        ];
        let screen = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 100, 30).unwrap();
            let mut parser = vt100::Parser::new(30, 100, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        feed(&mut ed, "<space>l");
        screen(&mut ed);
        feed(&mut ed, "<C-s>");
        let shot = ed.session.chat.as_ref().unwrap().screen.clone().expect("attached");
        assert_eq!((shot.cols, shot.rows), (100, 30));
        assert!(shot.text.contains("fn main() {}") && shot.text.contains("claude"), "{}", shot.text);
        let s = screen(&mut ed);
        assert!(s.contains("screen 100×30 goes with it") && s.contains("backspace no screen"), "{s}");
        // Backspace in the empty input takes it off; C-s again puts it back
        feed(&mut ed, "<backspace>");
        assert!(ed.session.chat.as_ref().unwrap().screen.is_none());
        assert!(screen(&mut ed).contains("C-s screen"));
        feed(&mut ed, "<C-s>what is wrong here<ret>");
        let c = ed.session.chat.as_ref().unwrap();
        assert!(c.screen.is_none(), "it went");
        let m = c.msgs.iter().rev().find(|m| m.role == crate::chat::Role::User).unwrap();
        assert!(m.chip.ends_with(" · screen 100×30"), "{}", m.chip);
        let copy = m.attached.clone().expect("a copy").path;
        assert!(c.links(&ed.session.root).iter().any(|l| l.path == copy), "C-] reaches it");
        // On its chip row it looks like the link it is: `screen 100×30` underlined, the rest of the chip not
        // (the panel back while Claude works, as `space l` brings it)
        ed.session.chat.as_mut().unwrap().unfolded = true;
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 100, 30).unwrap();
        let mut parser = vt100::Parser::new(30, 100, 0);
        parser.process(&bytes);
        let vt = parser.screen();
        let rows: Vec<String> = vt.rows(0, 100).collect();
        let r = rows.iter().position(|l| l.contains(" · screen 100×30")).expect("the chip row");
        let col = |needle: &str| rows[r].find(needle).map(|b| rows[r][..b].chars().count() as u16).unwrap();
        let underlined = |c: u16| vt.cell(r as u16, c).is_some_and(|cell| cell.underline());
        assert!(underlined(col("screen")) && underlined(col("100×30")), "the screen is a link: {}", rows[r]);
        assert!(!underlined(col(" · screen") + 1), "the rest of the chip isn't: {}", rows[r]);
        // (the fake claude's log isn't an editor event: poll, handling events as they come)
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !(std::fs::read_to_string(&log).unwrap_or_default().contains("what is wrong here")
            && std::fs::read_to_string(&copy).is_ok_and(|t| t.contains("fn main() {}")))
        {
            assert!(std::time::Instant::now() < end, "sent, and its copy written");
            if let Ok(Some(ev)) = ed.events.recv_within(std::time::Duration::from_millis(10)) {
                ed.handle_event(ev);
            }
        }
        let sent = std::fs::read_to_string(&log).unwrap();
        assert!(
            sent.contains(r#"<screen cols=\"100\" rows=\"30\""#) && sent.contains("fn main() {}"),
            "{sent}"
        );
        // As seen when `C-s` was pressed: its hint still offered it (not the line that came after)
        assert!(sent.contains("C-s screen") && !sent.contains("goes with it"), "as seen then: {sent}");
        std::fs::remove_file(&copy).ok();
        // `:chat-screen` with the chat closed: it opens, with the screen waiting
        let mut ed = editor("fn other() {}\n");
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        screen(&mut ed);
        crate::typed::execute(&mut ed, "chat-screen").unwrap();
        let c = ed.session.chat.as_ref().expect("opened");
        assert!(
            ed.session.is_at(At::Chat) && c.screen.as_ref().is_some_and(|s| s.text.contains("fn other() {}"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A card never takes what's typed in the chat (B1: the project-trust card took every `n` of "guard_and_go"
    /// and was dismissed): while the input has the keys they're the message's — a card that comes then waits.
    /// One that comes while nothing is being typed (Claude asking mid-turn) leaves them there too — the next
    /// message's first letter was an answer (frictions26) — and `C-q` hands them over.
    #[test]
    fn a_card_doesnt_take_what_is_typed_in_the_chat() {
        use crate::offer::{Offer, What};
        let mut ed = editor("text\n");
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        let drawn = |ed: &mut Editor| crate::term::render(ed, &mut Vec::new(), 120, 30).unwrap();
        let card = |title: &str| {
            Offer::new(title.into(), "x = 1", "", String::new(), What::ProjectTrust { hash: [0; 32] })
        };
        // A card up, in sight — then the chat, and a message with `n`s in it
        ed.push_offer(card("Trust this project's settings?"));
        drawn(&mut ed);
        feed(&mut ed, "<space>l");
        assert!(ed.session.is_at(crate::session::At::Chat));
        feed(&mut ed, "guard_and_go no");
        assert_eq!(
            ed.session.chat.as_ref().unwrap().input,
            "guard_and_go no",
            "every key went to the message"
        );
        assert_eq!(ed.offers.len(), 1, "the card is still there, unanswered");
        // A card that comes mid-message waits; the keys stay the message's
        ed.push_offer(card("Second"));
        assert!(ed.session.is_at(crate::session::At::Chat));
        feed(&mut ed, "n");
        assert_eq!(ed.session.chat.as_ref().unwrap().input, "guard_and_go non");
        // Nothing being typed: a card that comes leaves the chat the keys — `C-q` hands them over
        ed.session.chat.as_mut().unwrap().input.clear();
        ed.offers.clear();
        ed.push_offer(card("Claude wants to run"));
        assert!(ed.session.is_at(crate::session::At::Chat), "the chat keeps the keys");
        drawn(&mut ed);
        feed(&mut ed, "a");
        assert_eq!(
            (ed.offers.len(), ed.session.chat.as_ref().unwrap().input.text().as_str()),
            (1, "a"),
            "typed"
        );
        ed.session.chat.as_mut().unwrap().input.clear();
        feed(&mut ed, "<C-q>y");
        assert!(ed.offers.is_empty(), "y answered it");
        // (in the code from here)
        ed.session.leave(crate::session::At::Chat);
        // Focused, an answer letter the card doesn't take is said on it (`a` would start inserting in the code)
        ed.push_offer(card("Only y or n"));
        drawn(&mut ed);
        feed(&mut ed, "<C-q>aA");
        assert_eq!((ed.mode, ed.offers.len()), (Mode::Normal, 1), "a/A went nowhere");
        feed(&mut ed, "y");
        assert!(ed.offers.is_empty());
        assert_eq!(text(&ed), "text\n");
    }

    /// A card that came on its own waits unfocused (P3 §3.8): every key is where the keys are — `y` the code's
    /// yank, never its yes — the status line says `▲1 waiting · C-q` and the state names it; `C-q` focuses it,
    /// `esc` gives the keys back with it still up. What your own key asked for is focused as it comes; answered, the
    /// card that came on its own behind it waits unfocused — while one focused by `C-q` hands the keys on to the next
    /// (#101: going through them).
    #[test]
    fn a_card_that_came_on_its_own_waits_to_be_focused() {
        use crate::offer::{Offer, What};
        use crate::uistate::Region;
        let mut ed = editor("one two\n");
        let status = |ed: &mut Editor| {
            crate::term::render(ed, &mut Vec::new(), 100, 20).unwrap();
            let text = crate::screen::text(ed.last_frame.as_ref().unwrap().0.as_slice(), 100, 20).unwrap();
            text.lines().last().unwrap_or_default().to_string()
        };
        let card = |title: &str| {
            Offer::new(title.into(), "x = 1", "", String::new(), What::ProjectTrust { hash: [0; 32] })
        };
        let yours = |ed: &mut Editor| {
            let was = ed.events.set_asked(Some(ed.input_seq));
            let end = What::ReviewEnd { session: 0, started: std::time::Instant::now() };
            ed.push_offer(Offer::new(
                "End the review?".into(),
                "2 files not viewed.",
                "",
                String::new(),
                end,
            ));
            ed.events.set_asked(was);
        };
        // It comes as you type in the code: drawn, waiting — the next keys are the code's, `y` a yank
        feed(&mut ed, "w");
        ed.push_offer(card("Trust this project's settings?"));
        assert!(!ed.card_has_keys(), "it came on its own: it waits");
        assert!(status(&mut ed).contains("▲1 waiting · C-q"), "said: {}", status(&mut ed));
        let to = ed.keys_to();
        assert_eq!((to.route(Key::plain(Code::Char('y'))), to.waiting.is_some()), (Region::Code, true));
        feed(&mut ed, "bey");
        assert_eq!(ed.offers.len(), 1, "y was the code's: the card is still up");
        assert_eq!(ed.doc().selection().primary().head, 3, "b and e moved: on the end of `one`");
        assert_eq!(ed.registers.get(&'"').map(|r| r[0].as_str()), Some("one"), "yanked");
        // `C-q` focuses it; Esc gives the keys back, the card up and waiting again
        feed(&mut ed, "<C-q>");
        assert!(ed.card_has_keys() && !status(&mut ed).contains("waiting"), "focused, it says so itself");
        feed(&mut ed, "<esc>");
        assert!(!ed.card_has_keys() && ed.offers.len() == 1, "keys back, unanswered");
        // Your own card comes in front, focused at once; answered, Claude's behind it waits
        yours(&mut ed);
        assert!(ed.offers[0].yours && ed.card_has_keys(), "the user's own: its keys at once");
        feed(&mut ed, "n");
        assert_eq!(ed.offers.len(), 1);
        assert!(!ed.card_has_keys(), "Claude's next isn't handed the keys");
        // Focused by `C-q`, answered: the keys go on to the next (going through them)
        ed.push_offer(card("Another"));
        feed(&mut ed, "<C-q>n");
        assert_eq!(ed.offers.len(), 1, "the first answered");
        status(&mut ed);
        assert!(ed.card_has_keys(), "the keys went on to the second, drawn");
        feed(&mut ed, "n");
        assert!(ed.offers.is_empty() && !ed.card_has_keys());
        assert_eq!(text(&ed), "one two\n");
    }

    /// A focused card whose text changes (`Offer.rev` — asked again, joined, its group grown) lets go of the keys: a
    /// key meant for what it said is never an answer to what it says now. Asked again by your own key, yours again.
    #[test]
    fn a_focused_card_that_changes_lets_go_of_the_keys() {
        use crate::offer::{Offer, What};
        let mut ed = editor("one\n");
        let card =
            |q: &str| Offer::new("Trust?".into(), q, "", String::new(), What::ProjectTrust { hash: [0; 32] });
        ed.push_offer(card("x = 1"));
        feed(&mut ed, "<C-q>");
        assert!(ed.card_has_keys());
        ed.push_offer(card("x = 2"));
        assert_eq!((ed.offers.len(), ed.offers[0].question.as_str()), (1, "x = 2"), "replaced in place");
        assert!(!ed.card_has_keys(), "changed: unfocused");
        feed(&mut ed, "y");
        assert_eq!(ed.offers.len(), 1, "y was the code's");
        // (a weighty card answers only drawn — its answers in sight)
        crate::term::render(&mut ed, &mut Vec::new(), 100, 20).unwrap();
        feed(&mut ed, "<C-q>y");
        assert!(ed.offers.is_empty(), "focused again: y answered it");
    }

    /// The stray-key guard after an answer (the review of 7b): `y y` in one batch — a repeat, a yank typed on — answers
    /// the first card and never the weighty one behind it before a frame has drawn it, its text new or the same as the
    /// one answered (Claude's same command asked again): the second `y` is refused, said on the card; drawn, the keys
    /// are its and `y` answers it. And a weighty card focused before any frame drew it takes no answer from the buttons
    /// the last one left.
    #[test]
    fn keys_typed_on_after_an_answer_never_answer_a_card_not_yet_drawn() {
        use crate::offer::{Offer, What};
        let draw = |ed: &mut Editor| {
            crate::term::render(ed, &mut Vec::new(), 100, 24).unwrap();
            crate::screen::text(ed.last_frame.as_ref().unwrap().0.as_slice(), 100, 24).unwrap()
        };
        let card =
            |t: &str| Offer::new(t.into(), "x = 1", "", String::new(), What::ProjectTrust { hash: [0; 32] });
        for same in [false, true] {
            let mut ed = editor("one\n");
            ed.push_offer(card("Trust this project?"));
            // (the second as Claude's same ask comes: stacked behind it, not merged into it)
            let mut second = card(if same { "Trust this project?" } else { "Trust it again?" });
            ed.offer_seq += 1;
            second.id = ed.offer_seq;
            ed.offers.push(second);
            let id = ed.offer_seq;
            draw(&mut ed);
            feed(&mut ed, "<C-q>yy");
            assert_eq!(
                ed.offers.iter().map(|o| o.id).collect::<Vec<_>>(),
                [id],
                "{same}: the second up, unanswered"
            );
            assert!(!ed.card_has_keys(), "{same}: not before it's in sight");
            let s = draw(&mut ed);
            assert!(s.contains("came before this card was in sight"), "{same}: said on it:\n{s}");
            assert!(ed.card_has_keys(), "{same}: drawn: the keys handed on");
            feed(&mut ed, "y");
            assert!(ed.offers.is_empty(), "{same}: seen, its y answers it");
        }
        // Focused before any frame drew it (the one in front gone unanswered, its text the same): the last frame's
        // buttons aren't its, nor is the text seen whole before
        let mut ed = editor("one\n");
        ed.push_offer(card("Trust this project?"));
        let mut second = card("Trust this project?");
        ed.offer_seq += 1;
        second.id = ed.offer_seq;
        ed.offers.push(second);
        draw(&mut ed);
        ed.offers.remove(0);
        feed(&mut ed, "<C-q>y");
        assert_eq!(ed.offers.len(), 1, "never drawn: its y isn't taken");
        draw(&mut ed);
        feed(&mut ed, "y");
        assert!(ed.offers.is_empty(), "drawn: answered");
    }

    /// A switch of session lets go of a focused card: a card that stays up across it (a project's trust doesn't park
    /// with its session) waits in the other one, and again once back — a key there is never its answer.
    #[test]
    fn a_session_switch_lets_go_of_a_focused_card() {
        use crate::offer::{Offer, What};
        let dir = crate::scratch::Scratch::new("cardfocus-switch");
        let mut ed = editor("one\n");
        let main = ed.session.id;
        let other = ed.session_open(dir.path()).unwrap();
        assert!(ed.session_switch(main));
        let trust = What::ProjectTrust { hash: [0; 32] };
        ed.push_offer(Offer::new("Trust this project?".into(), "x = 1", "", String::new(), trust));
        feed(&mut ed, "<C-q>");
        assert!(ed.card_has_keys());
        assert!(ed.session_switch(other));
        assert!(ed.offers.len() == 1 && !ed.card_has_keys(), "up in the other session, waiting");
        assert!(ed.session_switch(main));
        assert!(!ed.card_has_keys(), "back: still waiting");
    }

    /// A press on a card that waits focuses it — on one of its answers too, which isn't taken (a click as it came
    /// up is no answer); focused, a press on an answer answers. `C-w` from it gives the keys back, the window prefix
    /// going on; `C-w w` goes round to it after the panes.
    #[test]
    fn a_click_or_c_w_w_focuses_a_card_and_c_w_leaves_it() {
        use crate::event::{Mouse, MouseKind};
        use crate::offer::{Offer, What};
        let mut ed = editor("one\n");
        let trust = || What::ProjectTrust { hash: [0; 32] };
        ed.push_offer(Offer::new("Trust?".into(), "x = 1", "", String::new(), trust()));
        crate::term::render(&mut ed, &mut Vec::new(), 100, 20).unwrap();
        // (its `y` as drawn now — where a press lands on it)
        let y_at = |ed: &Editor| {
            let b = ed.screen.offer_buttons.iter().find(|b| b.3 == 'y').expect("its y drawn");
            Mouse { kind: MouseKind::Down, x: b.1 as u16, y: b.0 as u16, alt: false, ctrl: false }
        };
        ed.handle_mouse(y_at(&ed));
        assert!(ed.card_has_keys() && ed.offers.len() == 1, "focused, not answered");
        // `C-w`: the keys back, the prefix under way from where they were
        feed(&mut ed, "<C-w>");
        assert!(!ed.card_has_keys() && ed.offers.len() == 1 && !ed.pending.is_empty(), "C-w left it");
        feed(&mut ed, "<esc>");
        // `C-w w` from the code: round to the card
        crate::term::render(&mut ed, &mut Vec::new(), 100, 20).unwrap();
        feed(&mut ed, "<C-w>w");
        assert!(ed.card_has_keys(), "C-w w went round to it");
        // (from it, `C-w w` goes on round the panes — never back to it: the review of 7b)
        crate::term::render(&mut ed, &mut Vec::new(), 100, 20).unwrap();
        feed(&mut ed, "<C-w>w");
        assert!(!ed.card_has_keys() && ed.session.is_at(crate::session::At::Code), "C-w w from it left it");
        feed(&mut ed, "<C-w>w");
        assert!(ed.card_has_keys(), "and the round reaches it again");
        crate::term::render(&mut ed, &mut Vec::new(), 100, 20).unwrap();
        ed.handle_mouse(y_at(&ed));
        assert!(ed.offers.is_empty(), "focused: its y pressed answered it");
    }

    /// Hints where there were none (uxaudit D6): a prompt says what enter does with its line (the command line's
    /// names complete with tab), a picker what enter does and that typing narrows it, both how they close; a
    /// bottom panel without the keys says how they come, before its commands.
    #[test]
    fn prompts_pickers_and_a_panel_say_their_keys() {
        let mut ed = editor("one two\n");
        let shown = |ed: &mut Editor| {
            crate::term::render(ed, &mut Vec::new(), 120, 24).unwrap();
            crate::screen::text(ed.last_frame.as_ref().unwrap().0.as_slice(), 120, 24).unwrap()
        };
        let status = |ed: &mut Editor| shown(ed).lines().last().unwrap_or_default().to_string();
        feed(&mut ed, ":");
        assert!(status(&mut ed).contains("enter run  tab complete  esc close"), "{}", status(&mut ed));
        feed(&mut ed, "<esc>/");
        assert!(status(&mut ed).contains("enter search  esc close"), "{}", status(&mut ed));
        feed(&mut ed, "<esc><space>?");
        assert!(status(&mut ed).contains("enter run  type narrow  esc close"), "{}", status(&mut ed));
        feed(&mut ed, "<esc>");
        assert!(!status(&mut ed).contains("esc close"), "closed: no hints");
        let task =
            crate::task::Task { name: "build".into(), cmd: "true".into(), cwd: ed.session.root.clone() };
        ed.session.task_run = Some(crate::task::Run::for_test(task));
        let header = |ed: &mut Editor| shown(ed).lines().find(|l| l.contains("space m")).map(str::to_string);
        let h = header(&mut ed).expect("the task panel's header");
        assert!(h.contains("C-w j scroll  space m"), "the way in, first: {h}");
        // Split, the top pane in hand: `C-w j` goes to the pane below — not said; from the lower one, said
        feed(&mut ed, "<C-w>s");
        shown(&mut ed);
        feed(&mut ed, "<C-w>k");
        shown(&mut ed);
        assert!(ed.pane_toward(0, 1).is_some(), "a pane below the one in hand");
        let h = header(&mut ed).expect("the task panel's header");
        assert!(!h.contains("C-w j"), "a pane below: {h}");
        feed(&mut ed, "<C-w>j");
        shown(&mut ed);
        let h = header(&mut ed).expect("the task panel's header");
        assert!(h.contains("C-w j scroll"), "the lowest pane: {h}");
        // In insert mode `C-w` deletes a word: not said there
        feed(&mut ed, "i");
        let h = header(&mut ed).expect("the task panel's header");
        assert!(!h.contains("C-w j"), "insert mode: {h}");
        feed(&mut ed, "<esc>");
        ed.bottom_take_keys();
        let h = shown(&mut ed).lines().find(|l| l.contains("esc code")).map(str::to_string);
        assert!(h.is_some_and(|h| !h.contains("C-w j")), "with the keys: its own");
    }

    /// Multiple offers stack in arrival order and only the front one gets keys — clicking a back card
    /// brings it forward; a card that is downloading doesn't take keys.
    #[test]
    fn offers_stack_in_arrival_order() {
        use crate::offer::OfferState;
        let mut ed = editor("x\n");
        ed.offer_popups = true;
        for l in ["rust", "python", "go"] {
            ed.offer_grammar(syntax::spec(l).unwrap());
        }
        let titles = |ed: &Editor| {
            ed.offers.iter().map(|o| o.title.split(' ').next().unwrap().to_string()).collect::<Vec<_>>()
        };
        assert_eq!(titles(&ed), ["Rust", "Python", "Go"], "new ones go last — front card unchanged");
        // The same offer isn't stacked twice
        let again = ed.offers[1].clone();
        ed.push_offer(again);
        assert_eq!(ed.offers.len(), 3);
        // Clicking a back card (depth 2) brings it forward
        ed.screen.offer_buttons = vec![(10, 40, 70, '2')];
        ed.handle_mouse(crate::event::Mouse {
            kind: crate::event::MouseKind::Down,
            x: 50,
            y: 10,
            alt: false,
            ctrl: false,
        });
        assert_eq!(titles(&ed), ["Go", "Rust", "Python"]);
        feed(&mut ed, "n");
        assert_eq!(titles(&ed), ["Rust", "Python"]);
        // A downloading card in front doesn't take keys · failures go to that card
        let id = ed.offers[0].id;
        ed.offers[0].state = OfferState::Installing(std::time::Instant::now());
        assert!(ed.offer_busy());
        feed(&mut ed, "n");
        assert_eq!(ed.offers.len(), 2, "n doesn't close the card while it's downloading");
        ed.offer_finished(id, Err("no network".into()));
        assert!(matches!(&ed.offers[0].state, OfferState::Failed(w) if w == "no network"));
        ed.offer_finished(id, Ok(()));
        assert_eq!(titles(&ed), ["Python"]);
    }

    #[test]
    fn mouse_click_drag_alt_and_wheel() {
        use crate::event::{Mouse, MouseKind as K};
        let mut ed = editor(&(0..30).map(|i| format!("line {i}\n")).collect::<String>());
        let rect = crate::split::Rect { x: 0, y: 1, w: 80, h: 10 };
        ed.screen = Screen {
            panes: vec![(1, rect, 5, (0..10).map(|l| (l, 0, 0, usize::MAX)).collect())],
            ..Screen::default()
        };
        let at = |kind, x, y| Mouse { kind, x, y, alt: false, ctrl: false };
        // Screen (5+2, 3) = line 2, column 2
        ed.handle_mouse(at(K::Down, 7, 3));
        assert_eq!(ed.doc().selection().primary(), Range::point(14 + 2));
        // Drag = from the pressed spot through the last char
        ed.handle_mouse(at(K::Drag, 9, 3));
        assert_eq!(ed.doc().selection().primary(), Range::new(16, 19));
        ed.handle_mouse(at(K::Up, 9, 3));
        // alt+press = add a cursor
        ed.handle_mouse(Mouse { alt: true, ..at(K::Down, 5, 1) });
        assert_eq!(ed.doc().selection().len(), 2);
        // Wheel = 3 lines (the cursor moves too)
        ed.handle_mouse(at(K::ScrollDown, 7, 3));
        assert_eq!(ed.doc().top, 3);
        assert_eq!(mv::line_of(&ed.doc().text, ed.doc().selection().primary().head), 3);
    }

    /// An accepted `space i` answer is a step of its own for `space u` / `space U`.
    #[test]
    fn an_ask_is_a_step_space_u_takes_back() {
        let (dir, main, ..) = follow_project("ask-turn");
        let mut ed = editor("");
        ed.open(&main).unwrap();
        fake_claude(&mut ed, "<r i=\"1\">LINE</r>", false);
        feed(&mut ed, "e iupper<ret>");
        // (a file's own jobs — its parse — may come first)
        settle_until(&mut ed, |ed| ed.session.review.as_ref().is_some_and(|r| r.ready));
        feed(&mut ed, "y");
        assert_eq!(text(&ed), "LINE 1\nline 2\nline 3\n");
        assert_eq!(ed.session.checkpoints.list.last().map(|c| c.question.as_str()), Some("upper"));
        feed(&mut ed, "<space>u");
        assert_eq!(text(&ed), "line 1\nline 2\nline 3\n");
        feed(&mut ed, "<space>U");
        assert_eq!(text(&ed), "LINE 1\nline 2\nline 3\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `space i`'s answer and a commit draft are each a turn of their own in the call-order record, named by
    /// what asked (`ask` · `draft` — one `start` serves both), their requests not counted (`null`, not 0).
    #[test]
    fn an_ask_and_a_commit_draft_are_recorded_by_what_asked() {
        let dir = crate::scratch::Scratch::new("calls-asides");
        let mut ed = editor("foo bar");
        ed.calls_dir = Some(dir.path().to_path_buf());
        let reply = serde_json::json!({ "type": "result", "result": "<r i=\"1\">FOO</r>", "total_cost_usd": 0.01,
                                         "usage": { "input_tokens": 5, "output_tokens": 3 } });
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), format!("read line; printf '%s\\n' '{reply}'")];
        feed(&mut ed, "e iupper<ret>");
        settle_until(&mut ed, |ed| ed.session.review.as_ref().is_some_and(|r| r.ready));
        feed(&mut ed, "y");
        assert!(ed.session.review.is_none());
        assert_eq!(crate::calls::recorded(dir.path(), 1)[0]["what"], "ask");
        let doc = ed.doc().id;
        crate::llm::draft(&mut ed, doc, "a commit message", "draft it", 0, 0).unwrap();
        settle_until(&mut ed, |ed| ed.session.review.as_ref().is_some_and(|r| r.ready));
        let lines = crate::calls::recorded(dir.path(), 2);
        let mut what: Vec<&str> = lines.iter().map(|l| l["what"].as_str().unwrap()).collect();
        what.sort_unstable();
        assert_eq!(what, ["ask", "draft"], "{lines:?}");
        assert!(lines.iter().all(|l| l["requests"].is_null() && l["steps"] == serde_json::json!([])));
    }

    #[test]
    fn ask_review_accept_then_undo() {
        let mut ed = editor("foo bar");
        fake_claude(&mut ed, "<r i=\"1\">FOO</r>", false);
        feed(&mut ed, "e i");
        assert_eq!(ed.prompt.as_ref().map(|p| p.text.as_str()), Some("ask "));
        feed(&mut ed, "upper<ret>");
        assert!(ed.session.review.as_ref().is_some_and(|r| !r.ready), "editing isn't blocked while waiting");
        settle(&mut ed);
        assert!(ed.session.review.as_ref().is_some_and(|r| r.ready));
        feed(&mut ed, "y");
        assert_eq!(text(&ed), "FOO bar");
        assert!(ed.session.review.is_none());
        feed(&mut ed, "u");
        assert_eq!(text(&ed), "foo bar");
    }

    #[test]
    fn ask_multi_selection_per_change_decisions() {
        let mut ed = editor("foo\nbar\n");
        fake_claude(&mut ed, "<r i=\"1\">X</r>\n<r i=\"2\">Y</r>", false);
        feed(&mut ed, "eC:ask shout<ret>");
        settle(&mut ed);
        feed(&mut ed, "yn");
        assert_eq!(text(&ed), "X\nbar\n");
    }

    #[test]
    fn ask_survives_edits_while_waiting() {
        let mut ed = editor("foo bar");
        fake_claude(&mut ed, "BAR", false);
        feed(&mut ed, "ww:ask upper<ret>");
        feed(&mut ed, "ghizz<esc>");
        settle(&mut ed);
        feed(&mut ed, "a");
        assert_eq!(text(&ed), "zzfoo BAR");
    }

    /// A big ask (a 200 KB selection) to a claude still starting — not reading yet — goes out off the main
    /// loop, and the answer still comes.
    #[test]
    fn a_big_ask_to_a_claude_still_starting_doesnt_hold_the_main_loop() {
        let mut ed = editor(&"word ".repeat(40 * 1024));
        let json = serde_json::json!({"type": "result", "is_error": false, "result": "<r i=\"1\">X</r>"});
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), format!("sleep 1; read -r line; printf '%s\\n' '{json}'")];
        feed(&mut ed, "%");
        let t = std::time::Instant::now();
        feed(&mut ed, ":ask x<ret>");
        let took = t.elapsed();
        assert!(took < std::time::Duration::from_millis(500), "the main loop waited {took:?} on the pipe");
        settle_until(&mut ed, |ed| ed.session.review.as_ref().is_some_and(|r| r.ready));
        feed(&mut ed, "y");
        assert_eq!(text(&ed), "X");
    }

    #[test]
    fn ask_error_is_reported() {
        let mut ed = editor("foo");
        fake_claude(&mut ed, "Not logged in", true);
        feed(&mut ed, ":ask x<ret>");
        settle(&mut ed);
        assert!(ed.session.review.is_none());
        assert_eq!(ed.status, Some(("claude: Not logged in".into(), Severity::Error)));
    }

    #[test]
    fn large_file_loads_in_background_and_cannot_be_saved_meanwhile() {
        let dir = std::env::temp_dir().join(format!("tarae-load-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.txt");
        let body = "0123456789 abcdefghij\n".repeat(60_000); // ≈ 1.3 MB > threshold
        std::fs::write(&path, &body).unwrap();
        let mut ed = editor("");
        ed.open(&path).unwrap();
        assert!(ed.doc().loading, "big files load in the background");
        feed(&mut ed, "d");
        assert!(ed.status.as_ref().is_some_and(|s| s.0.contains("still loading")));
        feed(&mut ed, ":w<ret>");
        assert_eq!(ed.status.as_ref().map(|s| s.1), Some(Severity::Error), "no saving while loading");
        assert_eq!(std::fs::read_to_string(&path).unwrap().len(), body.len(), "the file wasn't emptied");
        settle(&mut ed);
        assert!(!ed.doc().loading);
        assert_eq!(ed.doc().text.len_bytes(), body.len());
        feed(&mut ed, "d");
        assert_eq!(ed.doc().text.len_bytes(), body.len() - 1, "editable once fully read");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn shell_runs_in_background() {
        let mut ed = run("", ":sh echo hi<ret>");
        assert!(ed.status.is_none(), "the result comes from a worker thread");
        settle(&mut ed);
        assert_eq!(ed.status, Some(("hi".to_string(), Severity::Info)));
        let mut ed = run("", ":sh false<ret>");
        settle(&mut ed);
        assert_eq!(ed.status.as_ref().map(|s| s.1), Some(Severity::Error));
    }

    #[test]
    fn config_reload_swaps_keymap() {
        let mut ed = editor("ab");
        let (config, w) = crate::config::parse(
            "[keys.normal]
q = \"delete_selection\"\n",
        );
        ed.reload_config(config, w);
        assert_eq!(ed.status.as_ref().map(|s| s.0.as_str()), Some("config reloaded"));
        feed(&mut ed, "q");
        assert_eq!(text(&ed), "b");
    }

    #[test]
    fn search_and_repeat() {
        let ed = run("foo bar foo baz", "/foo<ret>");
        assert_eq!(ed.doc().selection().primary(), Range::new(8, 11));
        let ed = run("foo bar foo baz", "/foo<ret>n");
        assert_eq!(ed.doc().selection().primary(), Range::new(0, 3));
        assert_eq!(ed.status.as_ref().map(|s| s.0.as_str()), Some("search wrapped around"));
        let ed = run("foo bar foo baz", "/ba.<ret>nN");
        assert_eq!(ed.doc().selection().primary(), Range::new(4, 7));
        // *: selection as search term → n
        let ed = run("foo bar foo baz", "e*n");
        assert_eq!(ed.doc().selection().primary(), Range::new(8, 11));
        // n in select mode adds
        let ed = run("foo bar foo baz", "e*vn");
        assert_eq!(ed.doc().selection().len(), 2);
    }

    #[test]
    fn select_regex_then_edit_all() {
        // Core helix flow: select all → s selects every match → c changes them all at once
        assert_eq!(
            text(&run("let a = 1;\nlet b = 2;\n", "%slet<ret>cconst<esc>")),
            "const a = 1;\nconst b = 2;\n"
        );
        assert_eq!(text(&run("a, b,c", "%S,\\s*<ret>i-<esc>")), "-a, -b,-c");
        assert_eq!(text(&run("x1\ny2\nx3\n", "%<A-s>Kx<ret>d")), "\ny2\n\n");
        assert_eq!(text(&run("x1\ny2\nx3\n", "%<A-s><A-K>x<ret>d")), "x1\n\nx3\n");
        // No match → selection kept + error
        let ed = run("abc", "%szzz<ret>");
        assert_eq!(ed.doc().selection().primary(), Range::new(0, 3));
        assert_eq!(ed.status.as_ref().map(|s| s.1), Some(Severity::Error));
    }

    #[test]
    fn find_char_family() {
        assert_eq!(run("a,b,c,d", "f,").doc().selection().primary(), Range::new(0, 2));
        assert_eq!(run("a,b,c,d", "2f,").doc().selection().primary(), Range::new(0, 4));
        assert_eq!(run("a,b,c,d", "t,").doc().selection().primary(), Range::new(0, 3));
        assert_eq!(run("a,b,c", "glF,").doc().selection().primary(), Range::new(5, 3));
        assert_eq!(text(&run("a,b,c,d", "fcd")), ",d");
        // A-. remembers the target char too
        assert_eq!(run("a,b,c,d", "f,<A-.>").doc().selection().primary(), Range::new(1, 4));
        // esc cancels the awaited char
        assert_eq!(text(&run("abc", "f<esc>d")), "bc");
    }

    #[test]
    fn replace_case_indent_join() {
        assert_eq!(text(&run("abc", "lrx")), "axc");
        assert_eq!(text(&run("ab\ncd", "%rx")), "xx\nxx");
        assert_eq!(text(&run("aBc", "%~")), "AbC");
        assert_eq!(text(&run("aBc", "%`")), "abc");
        assert_eq!(text(&run("aBc", "%<A-`>")), "ABC");
        // `>` indents by the file's own unit — 2 here, 4 there
        assert_eq!(text(&run("a\n  b\n\nc\n", "%>")), "  a\n    b\n\n  c\n");
        assert_eq!(text(&run("a\n    b\n", "%>")), "    a\n        b\n");
        assert_eq!(text(&run("    a\n  b\n\tc\n", "%<lt>")), "a\nb\nc\n");
        assert_eq!(text(&run("a\n   b\nc\n", "J")), "a b\nc\n");
        assert_eq!(text(&run("a\nb\nc\n", "%J")), "a b c\n");
    }

    #[test]
    fn named_and_blackhole_registers() {
        assert_eq!(text(&run("ab", "\"ayl\"ap")), "aba");
        let ed = run("ab", "\"ayp");
        assert_eq!(ed.status.as_ref().map(|s| s.1), Some(Severity::Error), "the default register is empty");
        // Deleting with _ keeps the default register (the previous y) alive
        assert_eq!(text(&run("ab", "yl\"_dP")), "aa");
    }

    #[test]
    fn clipboard_roundtrip() {
        let mut ed = run("ab", " y");
        settle(&mut ed); // write job done
        // Said once the copy is done (not before: it might fail) — kept in `:messages`, a toast only once a frame
        // didn't draw its flash (`motion::words_drawn`)
        let said: Vec<_> = ed.messages.iter().map(|m| m.2.as_str()).collect();
        assert_eq!((said, ed.toasts.len()), (vec!["copied 1 selection"], 0));
        feed(&mut ed, "l p");
        settle(&mut ed); // read → paste
        assert_eq!(text(&ed), "aba");
    }

    #[test]
    fn macros_record_and_replay() {
        assert_eq!(text(&run("a\nb\nc\n", "QA;<esc>jQqq")), "a;\nb;\nc;\n");
        assert_eq!(text(&run("a\nb\nc\n", "QA;<esc>jQ2q")), "a;\nb;\nc;\n");
        assert_eq!(text(&run("a\nb\n", "\"xQA!<esc>jQ\"xq")), "a!\nb!\n");
    }

    #[test]
    fn graphemes_move_and_delete_as_one() {
        assert_eq!(text(&run("e\u{301}x", "d")), "x");
        assert_eq!(run("e\u{301}x", "l").doc().selection().primary(), Range::point(3)); // é = 3 B
        assert_eq!(text(&run("x👍🏽", "A<backspace><esc>")), "x");
        assert_eq!(text(&run("👍🏽x", "i<del><esc>")), "x");
        assert_eq!(text(&run("👍🏽x", "rz")), "zx", "r treats one cluster as one char");
    }

    #[test]
    fn sticky_column() {
        assert_eq!(run("abcdef\nab\nabcdef", "5ljj").doc().selection().primary(), Range::point(15));
        // Wide chars: below '글' (display column 2) is 'c' (bytes: 한글자 9 + newline 1 + ab 2)
        assert_eq!(run("한글자\nabcdef", "lj").doc().selection().primary(), Range::point(12));
        // A non-vertical command in between resets the column
        assert_eq!(run("abcdef\nab\nabcdef", "5ljhj").doc().selection().primary(), Range::point(11));
    }

    /// Byte-position safety net: 4×1000 pseudo-random keys over text mixing Hangul, emoji, combining
    /// marks and CRLF — no panics, and every selection endpoint always stays on a char boundary.
    /// Notifications: what comes together is one card (newest last); a long message stays longer than a short
    /// one; the same message again restarts its time where it is; once all have had their time the next
    /// starts a new card. `:messages` keeps them all, a log's order: oldest first, opened at the newest.
    #[test]
    fn notifications_are_one_card_and_kept() {
        let mut ed = editor("");
        ed.set_status("one");
        ed.set_success("two");
        ed.set_status("one");
        assert_eq!(ed.toasts.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(), ["one", "two"], "one card");
        let long = "x".repeat(120);
        ed.set_status(long.clone());
        let life = |ed: &Editor, text: &str| ed.toasts.iter().find(|t| t.text == text).unwrap().life;
        assert!(life(&ed, &long) > life(&ed, "one"), "reading time");
        // All had their time → a new card
        let past = std::time::Instant::now() - std::time::Duration::from_secs(60);
        ed.toasts.iter_mut().for_each(|t| t.at = past);
        ed.set_status("three");
        assert_eq!(ed.toasts.len(), 1);
        feed(&mut ed, ":messages<ret>");
        assert!(ed.float_list_keys(crate::pane::GroupKind::Messages), "the messages list, with the keys");
        let rows = crate::messages::rows(&ed);
        let texts: Vec<&str> = rows.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(texts.first(), Some(&"one"), "oldest first");
        assert_eq!(texts.last(), Some(&"three"), "newest last");
        assert_eq!(texts.len(), 5);
        assert_eq!(
            ed.float_list_here(crate::pane::GroupKind::Messages),
            rows.last().map(|r| r.id),
            "at the newest"
        );
    }

    /// An error stays until a key is pressed — not one that's already on its way (the first seconds).
    #[test]
    fn an_error_waits_for_a_key() {
        let mut ed = editor("");
        ed.set_error("broke");
        feed(&mut ed, "l");
        assert!(ed.toasts[0].life > std::time::Duration::from_secs(30), "too soon to take it");
        // Five seconds on the card
        ed.toasts_drawn(true);
        ed.toasts[0].at = std::time::Instant::now() - std::time::Duration::from_secs(5);
        assert!(ed.toasts[0].alive(), "still up without a key");
        feed(&mut ed, "l");
        assert!(ed.toasts[0].life < std::time::Duration::from_secs(6), "a key lets it go (fading out)");
    }

    /// An error no card could show yet (a picker over it, U7) waits with its time stopped — and asks for no
    /// frames while it waits (restarting its time every frame drew 14 a second, an idle tarae at 4–9% CPU).
    /// Shown, its time starts and its fade-in gets frames; a key can't take it before it was read.
    #[test]
    fn an_error_waiting_for_its_card_asks_for_no_frames() {
        use std::time::{Duration, Instant};
        let mut ed = editor("");
        ed.set_error("broke");
        ed.toasts[0].at = Instant::now() - Duration::from_secs(120);
        ed.toasts_drawn(false);
        assert!(ed.toasts[0].alive(), "waits, its time stopped");
        assert_eq!(ed.toast_wake(), None, "no frames while it waits");
        feed(&mut ed, "l");
        assert!(ed.toasts[0].life > Duration::from_secs(30), "a key doesn't take what wasn't read");
        // On the card: its time starts now, fading in
        ed.toasts_drawn(true);
        assert!(ed.toasts[0].shown && ed.toasts[0].age() < Duration::from_secs(1));
        assert_eq!(ed.toast_wake(), Some(TICK), "its fade-in");
        // A routine message beside a waiting error runs its own course
        let mut ed = editor("");
        ed.set_warning("changed on disk");
        ed.set_status("saved");
        assert!(ed.toast_wake().is_some_and(|d| d <= TICK), "the routine one fades in");
    }

    #[test]
    fn stress_positions_stay_on_char_boundaries() {
        const KEYS: &[&str] = &[
            "h",
            "j",
            "k",
            "l",
            "w",
            "b",
            "e",
            "x",
            "X",
            "d",
            "u",
            "U",
            "y",
            "p",
            "P",
            "C",
            ",",
            ";",
            "%",
            "J",
            ">",
            "<lt>",
            "~",
            "v",
            "<esc>",
            "gl",
            "gh",
            "ge",
            "gg",
            "<C-o>",
            "<tab>",
            "<C-s>",
            ".",
            "<C-c>",
            "i(\"'<esc>",
            "A<C-w><esc>",
            "i<A-d><esc>",
            "i<C-u><esc>",
            "a<C-k><esc>",
            "i(<backspace><esc>",
            "]f",
            "[f",
            "]c",
            "<A-o>",
            "<A-i>",
            "<A-n>",
            "<A-p>",
            "]p",
            "[p",
            "] ",
            "[ ",
            "fa",
            "t한",
            "F,",
            "r*",
            "i한<esc>",
            "a👍🏽<esc>",
            "o<esc>",
            "<A-;>",
            "<A-C>",
            "A<backspace><esc>",
            "i<del><esc>",
            "2l",
            "3w",
        ];
        let base = "한글 abc 👨\u{200d}👩\u{200d}👧 e\u{301}t,\r\n\t타래 fn(x) 👍🏽\n마지막\n";
        for mut seed in [0x7a2a_e5ee_d000_0001u64, 1, 42, 0xdead_beef] {
            let mut ed = editor(&base.repeat(3));
            for step in 0..1000 {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let k = KEYS[(seed >> 33) as usize % KEYS.len()];
                feed(&mut ed, k);
                let t = &ed.doc().text;
                for r in ed.doc().selection().ranges() {
                    for b in [r.anchor, r.head] {
                        assert!(
                            b <= t.len_bytes() && t.char_to_byte(t.byte_to_char(b)) == b,
                            "step {step} key {k:?}: {b} is not a char boundary"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn match_mode() {
        assert_eq!(text(&run("f(a, (b), c)", "7lmi(d")), "f(a, (), c)");
        assert_eq!(text(&run("f(a, (b), c)", "7lma(d")), "f(a, , c)");
        assert_eq!(text(&run("say \"타래\" ok", "5lmi\"cX<esc>")), "say \"X\" ok");
        assert_eq!(text(&run("foo bar baz", "4lmawd")), "foo baz");
        assert_eq!(run("f(a(b))", "lmm").doc().selection().primary(), Range::point(6));
        // Surround: add · replace · delete
        assert_eq!(text(&run("word", "miwms(")), "(word)");
        assert_eq!(text(&run("(word)", "2lmr([")), "[word]");
        assert_eq!(text(&run("'word'", "2lmd'")), "word");
        // In a file without a grammar, `mif` says so
        let ed = run("fn x() {}", "mif");
        assert!(ed.status.as_ref().is_some_and(|s| s.0.contains("needs syntax")));
    }

    #[test]
    fn buffer_picker_switches() {
        let mut ed = editor("first");
        ed.new_scratch();
        ed.doc_mut().text = ropey::Rope::from_str("second");
        assert_eq!(text(&ed), "second");
        // From the second buffer, the list's first row = the first document → select it to switch
        feed(&mut ed, " b");
        assert!(ed.picker.is_some());
        feed(&mut ed, "<ret>");
        assert!(ed.picker.is_none());
        assert_eq!(text(&ed), "first");
        feed(&mut ed, " b<esc>");
        assert!(ed.picker.is_none(), "closes with esc");
    }

    /// #47: `C-x` in the buffer list closes the buffer on its row — one with unsaved changes only when asked
    /// again — and the list says its keys.
    #[test]
    fn the_buffer_list_closes_buffers() {
        let scratch = crate::scratch::Scratch::new("buflist");
        let dir = scratch.to_path_buf();
        for f in ["a.txt", "b.txt"] {
            std::fs::write(dir.join(f), "text\n").unwrap();
        }
        let mut ed = editor("");
        ed.session.root = dir.clone();
        feed(&mut ed, ":o a.txt<ret>:o b.txt<ret>ihey<esc>");
        let names = |ed: &Editor| -> Vec<String> {
            ed.picker.as_ref().unwrap().items().iter().map(|i| i.label.clone()).collect()
        };
        let open = |ed: &Editor, f: &str| ed.docs.iter().any(|d| d.path.as_deref() == Some(&dir.join(f)));
        let screen = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 30).unwrap();
            let mut parser = vt100::Parser::new(30, 140, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        feed(&mut ed, " b");
        assert!(screen(&mut ed).contains("C-x close the buffer"), "{}", screen(&mut ed));
        // a.txt, saved: gone at once, the list still up
        feed(&mut ed, "a.txt<C-x>");
        assert!(!open(&ed, "a.txt") && ed.picker.is_some());
        assert!(!names(&ed).iter().any(|n| n.contains("a.txt")), "{:?}", names(&ed));
        // b.txt, unsaved: kept and said, then closed on the second
        feed(&mut ed, "<backspace><backspace><backspace><backspace><backspace>b.txt<C-x>");
        assert!(open(&ed, "b.txt"), "unsaved: kept");
        let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert!(said.contains("unsaved") && said.contains("C-x again"), "{said}");
        assert!(screen(&mut ed).contains("C-x close without saving"));
        feed(&mut ed, "<C-x>");
        assert!(!open(&ed, "b.txt"), "asked twice: closed");
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "text\n", "not saved");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_picker_and_global_search_open_files() {
        // In a repository of its own (git lists its files — never in tarae's own): the file list fills, and a
        // search result jumps to its line.
        let scratch = crate::scratch::Scratch::new("picker-files");
        let dir = scratch.to_path_buf();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        for i in 0..12 {
            std::fs::write(dir.join(format!("src/m{i}.rs")), format!("fn m{i}() {{}}\n")).unwrap();
        }
        std::fs::write(dir.join("src/selection.rs"), "fn other() {}\n\nfn put_cursor() {}\n").unwrap();
        for args in [&["init", "-q"][..], &["add", "-A"]] {
            let ok = std::process::Command::new("git").args(args).current_dir(&dir).output().unwrap();
            assert!(ok.status.success(), "git {args:?}");
        }
        let mut ed = editor("");
        ed.session.root = dir.clone();
        feed(&mut ed, " f");
        settle(&mut ed);
        let p = ed.picker.as_ref().unwrap();
        assert!(!p.loading && p.counts().1 > 10, "file list {:?}", p.counts());
        feed(&mut ed, "selection.rs<ret>");
        assert!(
            ed.doc().path.as_ref().is_some_and(|p| p.ends_with("src/selection.rs")),
            "{:?}",
            ed.doc().path
        );
        // (the live search looks in the project too)
        feed(&mut ed, " /fn put_cursor");
        settle_for(&mut ed, |ed| ed.picker.as_ref().is_some_and(|p| !p.searching && p.current().is_some()));
        feed(&mut ed, "<ret>");
        let doc = ed.doc();
        let pos = doc.selection().primary().head;
        let line = doc.text.line(mv::line_of(&doc.text, pos)).to_string();
        assert!(line.contains("fn put_cursor"), "{line:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #36: editing a big Rust file keeps memory bounded — undo keeps no parse trees (every step kept its
    /// own: injected layers share nothing between versions, and a reparse rebuilds the lists an edit sits in,
    /// a third of this file's nodes), so the document holds one version's trees however long it's edited.
    /// An undo or redo edits the current trees instead — colored at once, and after edits, undos and redos
    /// landing while a parse runs (forced, not timed), the tree is exactly what a fresh parse gives.
    #[test]
    fn long_editing_keeps_its_trees_bounded() {
        let dir = std::env::temp_dir().join(format!("tarae-tree-memory-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.rs");
        let text: String = (0..400)
            // (a comment with a tag keeps its layer — one with nothing to colour keeps none)
            .map(|i| format!("/// Item {i} — does `thing_{i}` for [`Other`].\n// NOTE: {i}\nfn f{i}(x: u32) -> u32 {{ x + {i} }}\n"))
            .collect();
        std::fs::write(&file, &text).unwrap();
        let mut ed = editor("");
        ed.open(&file).unwrap();
        let parsed = |ed: &Editor| {
            ed.doc().syntax.as_ref().is_some_and(|s| !s.dirty && !s.in_flight && s.tree.is_some())
        };
        settle_for(&mut ed, parsed);
        let Some(syn) = ed.doc().syntax.as_ref() else { return }; // (no rust grammar in this build)
        assert!(!syn.layers.is_empty(), "tagged comments are injected layers");
        let one = ed.doc().tree_nodes();
        // The tree as a fresh parse of the text would have it — what an incremental one must come to
        let fresh = |ed: &Editor| {
            let doc = ed.doc();
            let lang = doc.syntax.as_ref().unwrap().lang.clone();
            crate::syntax::Syntax::parsed(lang, &doc.text).tree.unwrap().root_node().to_sexp()
        };
        let now =
            |ed: &Editor| ed.doc().syntax.as_ref().unwrap().tree.as_ref().unwrap().root_node().to_sexp();
        let in_flight = |ed: &Editor| ed.doc().syntax.as_ref().is_some_and(|s| s.in_flight);
        for k in 0..40 {
            // An edit here and there, each its own undo step…
            feed(&mut ed, &format!("{}gg", 1 + (k * 17) % 1200));
            feed(&mut ed, "Ax<esc>");
            ed.schedule_parses();
            // …and every other one, another edit — or an undo, or a redo — landing while that parse runs (the
            // parse's result gets them reapplied: forced here, not left to timing)
            assert!(in_flight(&ed));
            match k % 4 {
                1 => feed(&mut ed, &format!("{}ggAy<esc>", 1 + (k * 31 + 5) % 1200)),
                2 => feed(&mut ed, "u"),
                3 => feed(&mut ed, "uU"),
                _ => {}
            }
            settle_for(&mut ed, |ed| !in_flight(ed));
            ed.schedule_parses();
            settle_for(&mut ed, parsed);
            assert_eq!(now(&ed), fresh(&ed), "edit {k}: the tree is the text's");
        }
        // Undo keeps no trees: what the document holds is one version's, after all that
        let all = ed.doc().tree_nodes();
        assert!(all.0 < one.0 + 500 && all.1 < one.1 + 100, "one version {one:?}, after 40 edits {all:?}");
        // An undo (and a redo) edits the current trees into the restored text — colored at once where nothing
        // changed, right after the parse where something did
        for keys in ["uuu", "UU"] {
            feed(&mut ed, keys);
            let syn = ed.doc().syntax.as_ref().unwrap();
            assert!(syn.tree.is_some() && !syn.layers.is_empty(), "{keys}: the trees stay, edited");
            let first =
                crate::syntax::highlights(syn, &ed.doc().text, 0..60, &mut tree_sitter::QueryCursor::new());
            assert!(!first.is_empty(), "{keys}: the top of the file still colored before any reparse");
            ed.schedule_parses();
            settle_for(&mut ed, parsed);
            assert_eq!(now(&ed), fresh(&ed), "{keys}: the tree is the text's");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #36: only the files looked at lately keep their parse trees — the `TREES_RECENT` seen most recently,
    /// and any seen within `trees_unseen()`. The others' go and they aren't parsed in the background; one
    /// shown again is parsed again. One opened but never shown (reopened at startup) never gets any.
    #[test]
    fn trees_go_from_files_not_looked_at_for_a_while() {
        let scratch = crate::scratch::Scratch::new("trees-sight");
        let dir = scratch.to_path_buf();
        // (documents keep real paths — macOS: /var/folders = /private/var/folders)
        let dir = std::fs::canonicalize(&dir).unwrap();
        let names = ["a", "b", "c", "d", "e", "f", "g", "h", "never"];
        for n in names {
            std::fs::write(dir.join(format!("{n}.rs")), format!("// {n}\nfn {n}() -> u32 {{ 1 }}\n"))
                .unwrap();
        }
        let mut ed = editor("");
        let has_tree = |ed: &Editor, n: &str| {
            let path = dir.join(format!("{n}.rs"));
            let doc = ed.docs.iter().find(|d| d.path.as_deref() == Some(path.as_path())).unwrap();
            doc.syntax.as_ref().is_some_and(|s| s.tree.is_some())
        };
        // Each looked at in turn (its language loads on a worker, then its parse)
        let mut grammar = true;
        for n in &names[..8] {
            ed.open(&dir.join(format!("{n}.rs"))).unwrap();
            let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !has_tree(&ed, n) && std::time::Instant::now() < end {
                if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(50)) {
                    ed.handle_event(ev);
                }
                ed.schedule_parses();
            }
            grammar &= has_tree(&ed, n);
        }
        if !grammar {
            return; // (no rust grammar in this build)
        }
        // One opened and left before it was ever shown
        ed.open(&dir.join("never.rs")).unwrap();
        ed.open(&dir.join("h.rs")).unwrap();
        // Ten minutes on: the five seen last keep theirs, the others' go
        ed.trees_follow_sight(std::time::Instant::now() + std::time::Duration::from_secs(600));
        let kept: Vec<&str> = names.iter().copied().filter(|n| has_tree(&ed, n)).collect();
        assert_eq!(kept, ["d", "e", "f", "g", "h"]);
        ed.schedule_parses();
        settle_for(&mut ed, |ed| ed.docs.iter().all(|d| d.syntax.as_ref().is_none_or(|s| !s.in_flight)));
        let kept: Vec<&str> = names.iter().copied().filter(|n| has_tree(&ed, n)).collect();
        assert_eq!(kept, ["d", "e", "f", "g", "h"], "not parsed again in the background");
        // Shown again: parsed again
        ed.open(&dir.join("a.rs")).unwrap();
        ed.schedule_parses();
        settle_for(&mut ed, |ed| has_tree(ed, "a"));
        assert!(has_tree(&ed, "a"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// N4: `space /` searches the project as you type — a regex, smart case (lowercase = any case), rows of
    /// code with the match standing out. Keys typed before a search lands leave only the last one's results;
    /// an unfinished pattern keeps the last results and says so quietly (no error); Enter goes there.
    #[test]
    fn space_slash_searches_as_you_type() {
        let scratch = crate::scratch::Scratch::new("live-search");
        let dir = scratch.to_path_buf();
        std::fs::write(dir.join("a.rs"), "fn alpha() {}\nlet Beta = alpha();\n").unwrap();
        std::fs::write(dir.join("b.txt"), "ALPHA beta\n").unwrap();
        let mut ed = editor("");
        ed.session.root = dir.clone();
        feed(&mut ed, "<space>/");
        let p = ed.picker.as_ref().expect("the search");
        assert!(p.live && p.title == "search" && p.current().is_none());
        let landed = |ed: &Editor| ed.picker.as_ref().is_some_and(|p| !p.searching);
        let found = |ed: &mut Editor| -> Vec<(String, Vec<(u32, u32)>)> {
            let p = ed.picker.as_mut().unwrap();
            p.visible(20).into_iter().filter(|r| r.label.contains(": ")).map(|r| (r.label, r.spots)).collect()
        };
        // Lowercase: any case
        feed(&mut ed, "alp");
        settle_for(&mut ed, landed);
        let rows = found(&mut ed);
        let labels: Vec<&str> = rows.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(labels, ["1: fn alpha() {}", "2: let Beta = alpha();", "1: ALPHA beta"]);
        assert_eq!(rows[1].1, [(14, 17)], "the match stands out");
        // A capital: as written — typed all at once, only the last pattern's results land
        feed(&mut ed, "<backspace><backspace><backspace>Beta");
        settle_for(&mut ed, landed);
        let rows = found(&mut ed);
        assert_eq!(rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), ["2: let Beta = alpha();"]);
        // Unfinished: the last results stay, said in the title's place — no error
        feed(&mut ed, "(");
        assert_eq!(ed.picker.as_ref().unwrap().note.as_deref(), Some("unclosed group"));
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 160, 40).unwrap();
        let mut vt = vt100::Parser::new(40, 160, 0);
        vt.process(&bytes);
        let screen = vt.screen().contents();
        assert!(screen.contains("unclosed group · last results"), "{screen}");
        assert_eq!(found(&mut ed).len(), 1);
        assert!(ed.status.is_none(), "{:?}", ed.status);
        // Finished: searched again, the note gone
        feed(&mut ed, ")");
        settle_for(&mut ed, landed);
        assert!(ed.picker.as_ref().unwrap().note.is_none());
        assert_eq!(found(&mut ed).len(), 1);
        // A regex is searched, not fuzzy-matched against the rows (no `[` or `\` in them)
        feed(&mut ed, "<backspace><backspace><backspace><backspace><backspace><backspace>alph[a]\\(\\)");
        settle_for(&mut ed, landed);
        let rows = found(&mut ed);
        assert_eq!(
            rows.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["1: fn alpha() {}", "2: let Beta = alpha();"]
        );
        feed(&mut ed, "<down><ret>");
        assert_eq!(ed.doc().path.as_deref(), Some(dir.join("a.rs").as_path()));
        assert_eq!(mv::line_of(&ed.doc().text, ed.doc().selection().primary().head), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A click on a picker's row selects it — the picker stays, its preview follows; the same row clicked
    /// again soon after (a double click) opens it, as Enter does (N3). A slow second click, or a click on
    /// another row, only selects.
    #[test]
    fn a_click_selects_a_picker_row_and_a_double_click_opens_it() {
        use crate::event::{Mouse, MouseKind as K};
        let scratch = crate::scratch::Scratch::new("picker-click");
        let dir = scratch.to_path_buf();
        // Opened documents keep real paths (macOS: /var/folders = /private/var/folders)
        let dir = std::fs::canonicalize(&dir).unwrap();
        for f in ["a.txt", "b.txt"] {
            std::fs::write(dir.join(f), format!("{f}\n")).unwrap();
        }
        let mut ed = editor("");
        let open = |ed: &mut Editor| {
            let items = ["a.txt", "b.txt"]
                .iter()
                .map(|f| crate::picker::Item {
                    label: format!("{f}:1"),
                    action: Action::Goto { path: dir.join(f), line: 0, col: 0 },
                    hint: String::new(),
                    glyph: None,
                })
                .collect();
            ed.open_picker(Picker::new("places", items, true), None);
            crate::term::render(ed, &mut Vec::new(), 100, 30).unwrap();
        };
        let click = |ed: &mut Editor, row: usize| {
            let (top, _) = ed.screen.picker_list.expect("the list on screen");
            ed.handle_mouse(Mouse { kind: K::Down, x: 10, y: (top + row) as u16, alt: false, ctrl: false });
        };
        let selected = |ed: &Editor| ed.picker.as_ref().and_then(|p| p.current()).map(|i| i.label.clone());
        open(&mut ed);
        click(&mut ed, 1);
        assert_eq!(selected(&ed).as_deref(), Some("b.txt:1"), "selected, the picker still there");
        assert!(ed.doc().path.is_none(), "nothing opened");
        click(&mut ed, 1);
        assert!(ed.picker.is_none(), "the double click opened it");
        assert_eq!(ed.doc().path.as_deref(), Some(dir.join("b.txt").as_path()));
        // A slow second click selects again; another row in between breaks the pair
        open(&mut ed);
        click(&mut ed, 0);
        if let Some((.., at)) = ed.picker_click.as_mut() {
            *at -= std::time::Duration::from_secs(1);
        }
        click(&mut ed, 0);
        assert_eq!(selected(&ed).as_deref(), Some("a.txt:1"), "too slow to be a double click");
        click(&mut ed, 1);
        click(&mut ed, 0);
        assert!(ed.picker.is_some(), "two rows aren't a double click");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prompt_backspace_on_empty_closes() {
        let ed = run("", ":<backspace>");
        assert!(ed.prompt.is_none());
    }

    /// NBSP (Option+Space on macOS)·U+3000 are multi-byte whitespace — completion must not slice mid-char.
    #[test]
    fn command_line_takes_multibyte_whitespace() {
        let mut ed = editor("");
        feed(&mut ed, ":o a\u{a0}b");
        assert!(ed.cmdline_completion().is_some());
        feed(&mut ed, "<esc>:theme\u{3000}m");
        assert!(ed.cmdline_completion().is_some());
    }

    /// Esc·C-c·Backspace on empty all close the `/` prompt the same way — the view returns and follows
    /// the cursor again. A `:` prompt doesn't preview the last search.
    #[test]
    fn every_way_out_of_search_restores_the_view() {
        let body: String = (0..40).map(|i| format!("line {i} foo\n")).collect();
        for close in ["<esc>", "<C-c>", &"<backspace>".repeat(8)] {
            let mut ed = editor(&body);
            feed(&mut ed, "/line 30");
            ed.refresh_search(10);
            assert!(ed.doc().top > 20);
            feed(&mut ed, close);
            assert!(ed.prompt.is_none(), "{close}");
            assert!(ed.search_origin.is_none(), "{close}: view follows the cursor again");
            assert_eq!(ed.doc().top, 0, "{close}");
        }
        let mut ed = editor(&body);
        feed(&mut ed, "/line 30<ret>gg:");
        ed.refresh_search(10);
        assert!(ed.search_origin.is_none(), "`:` isn't a search preview");
    }

    /// Moving focus to another document's pane in insert mode — each document's edits undo on their own.
    #[test]
    fn insert_edits_stay_with_their_document_across_panes() {
        let mut ed = editor("aaa");
        ed.handle_event(crate::event::Event::Resize);
        let first = ed.session.focus;
        feed(&mut ed, ":vs<ret>:n<ret>");
        let second = ed.session.focus;
        ed.handle_event(crate::event::Event::Resize); // focused pane shows the new scratch
        feed(&mut ed, "iB");
        ed.focus_view(first);
        feed(&mut ed, "x<esc>u");
        assert_eq!(text(&ed), "aaa", "x undone in its own document");
        ed.focus_view(second);
        assert_eq!(text(&ed), "B");
        feed(&mut ed, "u");
        assert_eq!(text(&ed), "");
    }

    /// Insert-mode sequence that breaks off (`j k` bound, `jx` typed) = the keys are typed, the last one
    /// goes through the keymap again.
    #[test]
    fn broken_insert_sequence_types_its_keys() {
        let run_jk = |keys: &str| {
            let (config, w) = crate::config::parse("[keys.insert]\nj = { k = \"normal_mode\" }\n");
            assert!(w.is_empty(), "{w:?}");
            let mut ed = Editor::new(config);
            feed(&mut ed, keys);
            ed
        };
        let ed = run_jk("ijx");
        assert_eq!((text(&ed), ed.mode), ("jx".into(), Mode::Insert));
        let ed = run_jk("ijk");
        assert_eq!((text(&ed), ed.mode), ("".into(), Mode::Normal));
        let ed = run_jk("ij<esc>");
        assert_eq!((text(&ed), ed.mode), ("j".into(), Mode::Normal), "Esc still leaves");
        let ed = run_jk("ijjk");
        assert_eq!((text(&ed), ed.mode), ("j".into(), Mode::Normal), "second j starts the sequence anew");
        assert_eq!(text(&run_jk("ijx<esc>u")), "", "one undo unit");
    }

    /// A file opened over the startup scratch shows in every pane that showed the scratch; a failed
    /// background load closes its buffer like `:bc` (panes move off it).
    #[test]
    fn opening_over_the_scratch_and_failed_loads_keep_panes_valid() {
        let dir = std::env::temp_dir().join(format!("tarae-panes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.txt");
        std::fs::write(&path, "hello\n").unwrap();
        let mut ed = editor("");
        ed.handle_event(crate::event::Event::Resize);
        feed(&mut ed, &format!(":vs {}<ret>", path.display()));
        ed.handle_event(crate::event::Event::Resize);
        let id = ed.doc().id;
        assert!(ed.session.views.iter().all(|v| v.doc == id), "both panes show a.txt");
        // A big file that fails to read
        let gone = ed.alloc_id();
        ed.docs.push(Document::placeholder(gone, &dir.join("big.txt")));
        ed.current = 1;
        ed.handle_event(crate::event::Event::Resize);
        ed.finish_loading(gone, Err(anyhow::anyhow!("boom")), std::time::Duration::ZERO);
        assert!(ed.docs.iter().all(|d| d.id != gone));
        assert!(ed.session.views.iter().all(|v| v.doc == id), "panes moved back to a.txt");
        assert_eq!(ed.doc().id, id);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `.` repeats the last insert: the command that entered insert mode, then what was typed — at the
    /// current selections, with a count.
    #[test]
    fn dot_repeats_the_last_insert() {
        assert_eq!(text(&run("a\nb\n", "Ahi<esc>j.")), "ahi\nbhi\n");
        assert_eq!(
            text(&run("one\ntwo\n", "ecX<esc>jghe.")),
            "X\nX\n",
            "change replays on the new selection"
        );
        assert_eq!(text(&run("a\n", "Ax<esc>2.")), "axxx\n");
        assert_eq!(text(&run("a\n", "Ax<esc>.u")), "ax\n", "one undo step per repeat");
        assert_eq!(text(&run("a\n", "A x<C-w>y<esc>.")), "a y y\n", "editing keys replay too");
        assert_eq!(text(&run("a\n", ".")), "a\n", "nothing yet");
    }

    /// The list: `C-q` keeps a picker's places; `]q` goes through them (wrapping, `[q` back) and they follow
    /// edits — a line added above the second one doesn't put `]q` on the wrong line; `space q` shows it in
    /// the side panel under its files, where Enter goes to an item too.
    #[test]
    fn the_list_keeps_places_and_follows_edits() {
        let dir = std::env::temp_dir().join(format!("tarae-qf-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "one\ntwo\nthree\n").unwrap();
        std::fs::write(&b, "four\n").unwrap();
        let mut ed = editor("");
        ed.open(&a).unwrap();
        let hit = |p: &std::path::Path, line: usize| crate::picker::Item {
            label: format!("{}:{}: x", p.display(), line + 1),
            action: Action::Goto { path: p.to_path_buf(), line, col: 0 },
            hint: String::new(),
            glyph: None,
        };
        let picker = Picker::new("grep /x/", vec![hit(&a, 0), hit(&a, 2), hit(&b, 0)], false);
        ed.open_picker(picker, None);
        feed(&mut ed, "<C-q>");
        assert!(ed.picker.is_none());
        settle_until(&mut ed, |ed| ed.session.qlist.is_some()); // real paths on a worker
        assert_eq!(ed.session.qlist.as_ref().map(|l| l.items.len()), Some(3));
        let at = |ed: &Editor| (ed.doc().display_name_short(), ed.cursor_line());
        feed(&mut ed, "]q");
        assert_eq!(at(&ed), ("a.txt".into(), 0));
        feed(&mut ed, "Onew<esc>");
        feed(&mut ed, "]q");
        assert_eq!(at(&ed), ("a.txt".into(), 3), "`three` moved down a line — so did its place");
        feed(&mut ed, "]q");
        assert_eq!(at(&ed), ("b.txt".into(), 0));
        feed(&mut ed, "]q");
        assert_eq!(at(&ed), ("a.txt".into(), 1), "wraps around to the first (now on line 1)");
        feed(&mut ed, "[q");
        assert_eq!(at(&ed), ("b.txt".into(), 0));
        feed(&mut ed, " q");
        let s = ed.session.sidebar.as_ref().expect("the list in the side panel");
        let rows: Vec<String> =
            s.lines().iter().map(|l| format!("{}{}", "  ".repeat(l.depth), l.label)).collect();
        assert_eq!(rows.len(), 6, "{rows:?}"); // folder · a.txt · 2 hits · b.txt · 1 hit
        assert_eq!(s.selected, 5, "on the current item");
        feed(&mut ed, "k<ret>");
        assert_eq!(at(&ed), ("a.txt".into(), 3), "k steps over b.txt's heading onto a.txt's second item");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// From the review: an item in a file opened after the list was made follows edits made there, and
    /// still does after the file is closed and opened again; a task printing the same place twice counts it
    /// once.
    #[test]
    fn the_list_follows_files_opened_later() {
        let dir = std::env::temp_dir().join(format!("tarae-qf-later-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let a = dir.join("a.txt");
        std::fs::write(&a, "one\ntwo\nthree\n").unwrap();
        let mut ed = editor("");
        let item = crate::quickfix::Item {
            path: a.clone(),
            line: 2,
            col: 0,
            message: "x".into(),
            severity: 1,
            mark: None,
        };
        ed.qlist_set("t", vec![item]);
        ed.open(&a).unwrap();
        feed(&mut ed, "ggOnew<esc>:w<ret>");
        feed(&mut ed, "]q");
        assert_eq!(ed.cursor_line(), 3, "`three` moved down — so did the place");
        feed(&mut ed, ":bc<ret>");
        ed.open(&a).unwrap();
        feed(&mut ed, "ggOnewer<esc>");
        feed(&mut ed, "]q");
        assert_eq!(ed.cursor_line(), 4, "after closing and opening again too");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A paste (bracketed) goes in as it is — no auto-indent, no auto-pairs — as one undo step: at the
    /// cursor while inserting; into a picker's query on one line. In normal and select mode it's held, not
    /// put in (#124): `p` puts it after the selection, `P` before, one undo step each.
    #[test]
    fn a_paste_goes_in_as_it_is() {
        let mut ed = editor("fn main() {\n}\n");
        feed(&mut ed, "A");
        ed.handle_event(crate::event::Event::Paste("\r\n    let v = (1,\r\n2);".into()));
        assert_eq!(
            ed.doc().text.to_string(),
            "fn main() {\n    let v = (1,\n2);\n}\n",
            "no pairs, no indent"
        );
        feed(&mut ed, "<esc>u");
        assert_eq!(ed.doc().text.to_string(), "fn main() {\n}\n", "one undo step with the insert");
        feed(&mut ed, "gg");
        ed.handle_event(crate::event::Event::Paste("x".into()));
        assert_eq!(ed.doc().text.to_string(), "fn main() {\n}\n", "normal mode: held, not put in");
        let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert!(said.starts_with("pasted a line — held, not in the file: p puts it after"), "{said}");
        feed(&mut ed, "P");
        assert_eq!(ed.doc().text.to_string(), "xfn main() {\n}\n", "P: before the selection");
        feed(&mut ed, "u");
        assert_eq!(ed.doc().text.to_string(), "fn main() {\n}\n");
        feed(&mut ed, " b");
        ed.handle_event(crate::event::Event::Paste("two\nwords".into()));
        assert_eq!(ed.picker.as_ref().map(|p| p.query.as_str()), Some("two words"));
        assert_eq!(ed.doc().text.to_string(), "fn main() {\n}\n", "the buffer untouched");
        // Select mode: held too, and back to normal mode; `p` puts it after the selection
        feed(&mut ed, "<esc>ggvl");
        ed.handle_event(crate::event::Event::Paste("_".into()));
        assert_eq!(ed.doc().text.to_string(), "fn main() {\n}\n");
        assert_eq!(ed.mode, Mode::Normal);
        feed(&mut ed, "p");
        assert_eq!(ed.doc().text.to_string(), "fn_ main() {\n}\n");
    }

    /// The editor runs a big picker's filtering on a worker and takes the result; Enter right after typing
    /// opens this query's best match, not the last one's.
    #[test]
    fn a_big_pickers_filtering_runs_on_a_worker() {
        let mut ed = editor("");
        let items: Vec<crate::picker::Item> = (0..60_000)
            .map(|i| crate::picker::Item {
                label: format!("d{}/f{i}.rs", i % 50),
                action: crate::picker::Action::Open(format!("/nowhere/d{}/f{i}.rs", i % 50).into()),
                hint: String::new(),
                glyph: None,
            })
            .collect();
        let mut p = crate::picker::Picker::new("f", items, true).grouped("/nowhere".into());
        p.asynchronous = true;
        ed.picker = Some(p);
        ed.handle_event(crate::event::Event::Key("f".parse().unwrap()));
        assert!(ed.picker.as_ref().unwrap().filtering);
        settle_until(&mut ed, |ed| !ed.picker.as_ref().unwrap().filtering);
        // Typed, then at once a move: this query's rows
        for k in ["5", "9", "9", "9", "down"] {
            ed.handle_event(crate::event::Event::Key(k.parse().unwrap()));
        }
        let p = ed.picker.as_ref().unwrap();
        assert!(!p.filtering, "settled by the move");
        assert_eq!(p.query, "f5999");
        assert_eq!(p.shown().next().map(|i| i.label.as_str()), Some("d49/f5999.rs"), "the best match first");
        // Closed while a worker filters (a query that takes all of them again): kept for `space '` with this
        // query's rows, nothing pending
        for k in ["backspace", "backspace", "backspace", "backspace", "backspace", "d"] {
            ed.handle_event(crate::event::Event::Key(k.parse().unwrap()));
        }
        assert!(ed.picker.as_ref().unwrap().filtering);
        ed.handle_event(crate::event::Event::Key("esc".parse().unwrap()));
        let kept = ed.last_picker.as_ref().unwrap();
        assert!(!kept.filtering && kept.query == "d");
        assert_eq!(kept.counts().0, 60_000, "every one has a d");
    }

    /// A build task runs in the background; the places it prints fill the list as they come, `]q` goes to
    /// the first.
    #[test]
    fn a_task_fills_the_list() {
        let dir = std::env::temp_dir().join(format!("tarae-task-run-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::fs::write(dir.join("m.rs"), "fn main() {\n    x\n}\n").unwrap();
        let mut ed = editor("");
        ed.task_start(crate::task::Task {
            name: "check".into(),
            cmd: "echo compiling; echo 'm.rs:2:5: error[E0425]: cannot find value `x`' >&2; echo 'm.rs:2:5: error[E0425]: cannot find value `x`'; exit 1".into(),
            cwd: dir.clone(),
        });
        settle_until(&mut ed, |ed| ed.session.task_run.as_ref().is_some_and(|r| r.took.is_some()));
        let r = ed.session.task_run.as_ref().unwrap();
        assert_eq!((r.state.clone(), r.found), (crate::task::State::Failed(Some(1)), (1, 0)));
        assert_eq!(r.output.len(), 3, "the same place twice (both streams) — counted once");
        feed(&mut ed, "]q");
        assert_eq!((ed.doc().display_name_short(), ed.cursor_line()), ("m.rs".to_string(), 1));
        assert_eq!(ed.doc().selection().primary().head, 16, "line 2, column 5");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Enter, `o`, `O` indent from the syntax tree (`indents.scm`, the tree reparsed on the spot since the
    /// keys came faster than the background parse): between brackets the closer gets its own line, `o` in
    /// a block stays in it, `O` over a closer goes in, `o` after a block comes back out; a whitespace-only
    /// line keeps none of it. Only with the Rust grammar.
    #[test]
    fn new_lines_indent_from_the_tree() {
        let Some(mut ed) = parsed_rust("fn a() {}\n") else { return };
        feed(&mut ed, "f{a<ret>x<esc>");
        assert_eq!(text(&ed), "fn a() {\n    x\n}\n", "between brackets");
        feed(&mut ed, "oy<esc>");
        assert_eq!(text(&ed), "fn a() {\n    x\n    y\n}\n", "o stays in the block");
        feed(&mut ed, "jOw<esc>");
        assert_eq!(text(&ed), "fn a() {\n    x\n    y\n    w\n}\n", "O above the closer goes in");
        feed(&mut ed, "joq<esc>");
        assert_eq!(text(&ed), "fn a() {\n    x\n    y\n    w\n}\nq\n", "o after the block comes out");
        let Some(mut ed) = parsed_rust("fn a() {\n    \n}\n") else { return };
        feed(&mut ed, "jA<ret>x<esc>");
        assert_eq!(text(&ed), "fn a() {\n\n    x\n}\n", "the blank line's whitespace goes");
    }

    /// Without a tree: one level more after an opening bracket; the unit is the file's own (tabs here).
    #[test]
    fn new_lines_indent_without_a_tree() {
        assert_eq!(text(&run("x = [\n", "A<ret>1<esc>")), "x = [\n    1\n");
        assert_eq!(text(&run("a {\n\tb\n}\n", "jA<ret>c<esc>")), "a {\n\tb\n\tc\n}\n");
        assert_eq!(text(&run("a {\n\tb\n}\n", "ji<tab><esc>")), "a {\n\t\tb\n}\n", "Tab in a tab file");
        assert_eq!(text(&run("\n", "ix = [<ret>1<esc>")), "x = [\n    1\n]\n", "between brackets, no tree");
        assert_eq!(
            text(&run("a {\n", "A<ret><ret>b<esc>")),
            "a {\n\n    b\n",
            "a blank line under an opener"
        );
        assert_eq!(text(&run("foo(a, b)\n", "f,a<ret><esc>")), "foo(a,\nb)\n", "the spaces at the break go");
        assert_eq!(
            text(&run("    foo\n", "lli<ret><esc>")),
            "\n    foo\n",
            "from the indentation: nothing left above"
        );
    }

    fn rust_editor(text: &str) -> Editor {
        let mut ed = editor(text);
        ed.docs[0].path = Some("/tmp/tarae-test.rs".into());
        ed
    }

    #[test]
    fn auto_pairs_in_insert_mode() {
        assert_eq!(text(&run("\n", "i(x)<esc>")), "(x)\n", "closer added, then stepped over");
        assert_eq!(text(&run("\n", "i(<backspace><esc>")), "\n", "backspace removes both");
        assert_eq!(text(&run("\n", "idon't<esc>")), "don't\n");
        assert_eq!(text(&run("\n", "i\"a\"<esc>")), "\"a\"\n");
        assert_eq!(text(&run("x\n", "i(<esc>")), "(x\n", "before a word char: no closer");
        let mut ed = rust_editor("\n");
        feed(&mut ed, "i&'a<esc>");
        assert_eq!(text(&ed), "&'a\n", "Rust lifetimes don't pair");
        let mut ed = editor("\n");
        ed.config.auto_pairs = false;
        feed(&mut ed, "i(<esc>");
        assert_eq!(text(&ed), "(\n");
    }

    #[test]
    fn insert_mode_editing_keys() {
        assert_eq!(text(&run("foo bar\n", "A<C-w><esc>")), "foo \n");
        assert_eq!(text(&run("foo bar\n", "A<C-w><C-w><esc>")), "\n");
        assert_eq!(text(&run("a\nb\n", "jI<C-w><esc>")), "ab\n", "at a line start: the line break");
        assert_eq!(text(&run("foo.bar\n", "A<A-backspace><esc>")), "foo.\n", "punctuation is its own run");
        assert_eq!(text(&run("foo bar\n", "i<A-d><esc>")), " bar\n");
        assert_eq!(text(&run("    foo bar\n", "A<C-u><esc>")), "    \n", "to the indentation first");
        assert_eq!(text(&run("    foo\n", "A<C-u><C-u><esc>")), "\n");
        assert_eq!(text(&run("a\nb\n", "jI<C-u><esc>")), "ab\n");
        assert_eq!(text(&run("foo bar\n", "i<C-k><esc>")), "\n");
        assert_eq!(text(&run("a\nb\n", "i<C-k><C-k><esc>")), "b\n", "at the line end: joins");
        assert_eq!(text(&run("abc\n", "yA<C-r>\"<esc>")), "abca\n");
        assert_eq!(text(&run("\n", "iab<C-s>cd<esc>u")), "ab\n", "C-s = undo step");
        assert_eq!(text(&run("abc\n", "A<home>X<esc>")), "Xabc\n");
        assert_eq!(text(&run("abc\n", "i<end>X<esc>")), "abcX\n");
        assert_eq!(text(&run("ab\n", "A<C-h><C-j>x<esc>")), "a\nx\n");
    }

    #[test]
    fn c_c_toggles_line_comments() {
        let mut ed = rust_editor("fn a() {\n    x();\n}\n");
        feed(&mut ed, "jgs<C-c>");
        assert_eq!(text(&ed), "fn a() {\n    // x();\n}\n");
        assert_eq!(
            ed.doc().text.byte_slice(ed.doc().selection().primary().from()..).chars().next(),
            Some('x'),
            "the cursor stays on its char"
        );
        feed(&mut ed, "<C-c>");
        assert_eq!(text(&ed), "fn a() {\n    x();\n}\n");
        feed(&mut ed, "%<C-c>");
        assert_eq!(text(&ed), "// fn a() {\n//     x();\n// }\n");
        feed(&mut ed, "u");
        assert_eq!(text(&ed), "fn a() {\n    x();\n}\n", "one undo step");
        let ed = run("x\n", "<C-c>");
        assert_eq!(text(&ed), "x\n", "unknown language: nothing");
    }

    /// `space c` toggles line comments, as `C-c` does (Helix's keys): on, and back off.
    #[test]
    fn space_c_toggles_line_comments() {
        let mut ed = rust_editor("fn a() {\n    x();\n}\n");
        feed(&mut ed, "j<space>c");
        assert_eq!(text(&ed), "fn a() {\n    // x();\n}\n");
        feed(&mut ed, "<space>c");
        assert_eq!(text(&ed), "fn a() {\n    x();\n}\n");
    }

    /// `space C` puts a block comment around a selection — its text, not its lines — and takes it off again;
    /// a cursor's line is wrapped whole.
    #[test]
    fn space_capital_c_toggles_a_block_comment_around_the_selection() {
        let mut ed = rust_editor("call(alpha, beta);\n");
        feed(&mut ed, "w<space>C");
        assert_eq!(text(&ed), "/* call */(alpha, beta);\n");
        feed(&mut ed, "<space>C");
        assert_eq!(text(&ed), "call(alpha, beta);\n", "the selection still covers it: off again");
        let mut ed = rust_editor("    x();\n");
        feed(&mut ed, "<space>C");
        assert_eq!(text(&ed), "    /* x(); */\n", "a cursor: its line, inside its indentation");
        feed(&mut ed, "<space>C");
        assert_eq!(text(&ed), "    x();\n");
    }

    /// A language with no block comment (Python): `space C` gives line comments, as Helix does.
    #[test]
    fn space_capital_c_falls_back_to_line_comments() {
        let mut ed = editor("x = 1\n");
        ed.docs[0].path = Some("/tmp/tarae-test.py".into());
        feed(&mut ed, "<space>C");
        assert_eq!(text(&ed), "# x = 1\n");
        feed(&mut ed, "<space>C");
        assert_eq!(text(&ed), "x = 1\n");
    }

    /// Several cursors: `space c` comments each one's line, `space C` wraps each — two on one line wrap it
    /// once — and each comes off again.
    #[test]
    fn comment_toggles_take_every_cursor() {
        let mut ed = rust_editor("a();\nb();\n");
        feed(&mut ed, "C<space>c");
        assert_eq!(ed.doc().selection().ranges().len(), 2);
        assert_eq!(text(&ed), "// a();\n// b();\n");
        feed(&mut ed, "<space>c");
        assert_eq!(text(&ed), "a();\nb();\n");
        feed(&mut ed, "<space>C");
        assert_eq!(text(&ed), "/* a(); */\n/* b(); */\n");
        feed(&mut ed, "<space>C");
        assert_eq!(text(&ed), "a();\nb();\n");
        let mut ed = rust_editor("one two\n");
        let sel = Selection::new(vec![Range::point(0), Range::point(4)], 0);
        ed.doc_mut().set_selection(sel);
        feed(&mut ed, "<space>C");
        assert_eq!(text(&ed), "/* one two */\n", "two cursors on one line: one comment");
    }

    /// A Rust document with its tree parsed now — None when the grammar isn't built in.
    fn parsed_rust(text: &str) -> Option<Editor> {
        let lang = syntax::Loader::global().load(syntax::spec("rust")?).ok()?;
        let mut ed = rust_editor(text);
        let mut syn = syntax::Syntax::new(lang);
        let job = syn.start_parse(&ed.docs[0].text);
        let generation = job.generation;
        let tree = job.run();
        syn.finish_parse(generation, tree);
        ed.docs[0].syntax = Some(syn);
        Some(ed)
    }

    fn selected(ed: &Editor) -> String {
        let (doc, r) = (ed.doc(), ed.doc().selection().primary());
        doc.text.byte_slice(r.from()..r.to()).to_string()
    }

    const STRUCT_SRC: &str = "fn a(x: u8, y: u8) {\n    call(x);\n}\n\n// note\nfn b() {}\n";

    /// `]f` `[f` `]c` select the next/previous textobject; `C-o` comes back.
    #[test]
    fn bracket_keys_select_textobjects() {
        let Some(mut ed) = parsed_rust(STRUCT_SRC) else { return };
        feed(&mut ed, "]f");
        assert_eq!(selected(&ed), "fn b() {}");
        feed(&mut ed, "[f");
        assert_eq!(selected(&ed), "fn a(x: u8, y: u8) {\n    call(x);\n}");
        feed(&mut ed, "]c");
        assert_eq!(selected(&ed), "// note");
        feed(&mut ed, "<C-o>");
        assert_eq!(selected(&ed), "fn a(x: u8, y: u8) {\n    call(x);\n}", "a jump");
    }

    /// `A-o` grows by syntax node, `A-i` retraces it; `A-n`/`A-p` step through siblings.
    #[test]
    fn alt_o_grows_and_alt_i_retraces() {
        let Some(mut ed) = parsed_rust(STRUCT_SRC) else { return };
        let x = STRUCT_SRC.find("x)").unwrap();
        ed.docs[0].set_selection(Selection::point(x));
        feed(&mut ed, "<A-o><A-o>");
        assert_eq!(selected(&ed), "call(x)");
        feed(&mut ed, "<A-i><A-i>");
        assert_eq!(ed.doc().selection().primary(), Range::point(x), "back where it started");
        ed.docs[0].set_selection(Selection::point(STRUCT_SRC.find("x:").unwrap()));
        feed(&mut ed, "<A-o><A-n>");
        assert_eq!(selected(&ed), "y: u8");
        feed(&mut ed, "<A-p>");
        assert_eq!(selected(&ed), "x: u8");
        let mut plain = editor("x\n");
        feed(&mut plain, "<A-o>");
        assert!(plain.status.as_ref().is_some_and(|(m, _)| m.contains("tree-sitter")));
    }

    #[test]
    fn paragraphs_and_blank_lines() {
        let line = |ed: &Editor| ed.doc().text.byte_to_line(ed.doc().selection().primary().head);
        assert_eq!(line(&run("a\nb\n\nc\n", "]p")), 3);
        assert_eq!(line(&run("a\nb\n\nc\n", "]p[p")), 0);
        let ed = run("a\nb\n", "] ");
        assert_eq!((text(&ed), line(&ed)), ("a\n\nb\n".into(), 0), "cursor stays");
        let ed = run("a\nb\n", "j2[ ");
        assert_eq!((text(&ed), line(&ed)), ("a\n\n\nb\n".into(), 3), "moves down with its line");
    }

    /// `C-o` returns from big moves (`ge`, `gg`, search) and `C-i`/Tab goes forward again; a spot
    /// follows edits made after leaving it; `C-s` saves a spot by hand.
    #[test]
    fn jumplist_back_and_forth() {
        let head = |ed: &Editor| ed.doc().selection().primary().cursor(&ed.doc().text);
        let line = |ed: &Editor| ed.doc().text.byte_to_line(head(ed));
        let ed = run("one\ntwo\nthree\nfour\n", "jge<C-o>");
        assert_eq!(line(&ed), 1, "back where ge started");
        let mut ed = run("one\ntwo\nthree\nfour\n", "jge<C-o><tab>");
        assert_eq!(line(&ed), 3, "Tab = forward again");
        feed(&mut ed, "<C-i>");
        assert_eq!(line(&ed), 3, "nothing further ahead");
        let ed = run("one\ntwo\nthree\nfour\n", "/fo<ret>/tw<ret><C-o><C-o>");
        assert_eq!(line(&ed), 0, "two searches back");
        // Lines added above the saved spot: it moves with its text
        let ed = run("one\ntwo\nthree\n", "jjggOx<ret>y<esc><C-o>");
        assert_eq!(ed.doc().text.line(line(&ed)).to_string(), "three\n");
        let ed = run("one\ntwo\nthree\n", "j<C-s>jj<C-o>");
        assert_eq!(line(&ed), 1, "C-s spot");
        // A new jump after going back drops the forward branch
        let ed = run("a\nb\nc\nd\n", "jgej<C-o>gg<tab>");
        assert_eq!(line(&ed), 0, "nothing ahead of gg any more");
    }

    /// `space j` lists the pane's jumps; picking one goes there (and `Tab` comes back). `space '` reopens
    /// the last picker as it was left.
    #[test]
    fn jumplist_picker_and_reopening_the_last_picker() {
        let line = |ed: &Editor| ed.doc().text.byte_to_line(ed.doc().selection().primary().head);
        let mut ed = run("a\nb\nc\nd\n", "jge");
        feed(&mut ed, " j");
        let p = ed.picker.as_ref().expect("jump list open");
        assert_eq!((p.title.as_str(), p.items().len()), ("jumps", 1));
        assert!(p.items()[0].label.ends_with(":2"), "{}", p.items()[0].label);
        feed(&mut ed, "<ret>");
        assert_eq!(line(&ed), 1);
        feed(&mut ed, "<tab>");
        assert_eq!(line(&ed), 3, "the spot left is kept");
        feed(&mut ed, " jd<esc> '");
        let p = ed.picker.as_ref().expect("reopened");
        assert_eq!((p.title.as_str(), p.query.as_str()), ("jumps", "d"));
        feed(&mut ed, "<esc>");
        let mut fresh = editor("x\n");
        feed(&mut fresh, " '");
        assert!(fresh.picker.is_none());
    }

    /// The file picker's tree: it starts on the file being edited (its folders open); ← goes to the folder,
    /// Enter folds it, → opens it again; typing keeps the tree — the matches under their folders, which are
    /// headings the selection steps over — and emptying the query brings the folds back.
    #[test]
    fn file_picker_tree() {
        let root = PathBuf::from("/r");
        let item = |r: &str| crate::picker::Item {
            label: r.into(),
            action: Action::Open(root.join(r)),
            hint: String::new(),
            glyph: None,
        };
        let mut ed = editor("");
        let picker =
            Picker::new("files", Vec::new(), true).with_tree(root.clone(), Some("src/main.rs".into()));
        ed.open_picker(picker, None);
        ed.picker.as_mut().unwrap().set_items(vec![
            item("README.md"),
            item("src/main.rs"),
            item("src/lib.rs"),
            item("docs/lib.md"),
        ]);
        let cur = |ed: &Editor| ed.picker.as_ref().and_then(|p| p.current()).map(|i| i.label.clone());
        let rows = |ed: &mut Editor| -> Vec<String> {
            let p = ed.picker.as_mut().unwrap();
            p.visible(20).iter().map(|r| format!("{}{}", "  ".repeat(r.tree.unwrap().0), r.label)).collect()
        };
        assert_eq!(cur(&ed).as_deref(), Some("src/main.rs"), "on the file being edited");
        assert_eq!(rows(&mut ed), ["docs", "src", "  lib.rs", "  main.rs", "README.md"]);
        feed(&mut ed, "<left>");
        assert_eq!(cur(&ed).as_deref(), Some("src"));
        feed(&mut ed, "<ret>");
        assert_eq!(cur(&ed).as_deref(), Some("src"), "folded, still on it");
        assert_eq!(rows(&mut ed).len(), 3);
        feed(&mut ed, "<right>");
        assert_eq!(rows(&mut ed).len(), 5);
        feed(&mut ed, "lib");
        assert_eq!(rows(&mut ed), ["src", "  lib.rs", "docs", "  lib.md"], "typing: matches under folders");
        assert_eq!(cur(&ed).as_deref(), Some("src/lib.rs"), "the best one, past its heading");
        feed(&mut ed, "<down>");
        assert_eq!(cur(&ed).as_deref(), Some("docs/lib.md"), "headings are stepped over");
        feed(&mut ed, "<down>");
        assert_eq!(cur(&ed).as_deref(), Some("src/lib.rs"), "wrapping around");
        feed(&mut ed, "<backspace><backspace><backspace>");
        assert_eq!(rows(&mut ed).len(), 5, "the tree again");
    }

    /// The side panel's files: the current file's folders open and it's selected; j/Enter opens another
    /// (the code gets the keys back, the panel follows it); h/l fold and unfold; Esc leaves it showing;
    /// `space e` on a focused panel closes it.
    #[test]
    fn side_panel_files() {
        let dir = std::env::temp_dir().join(format!("tarae-sidebar-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("src/ui")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        for f in ["src/ui/card.rs", "src/a.rs", "b.md"] {
            std::fs::write(dir.join(f), "x\n").unwrap();
        }
        let mut ed = editor("");
        ed.open(&dir.join("src/ui/card.rs")).unwrap();
        ed.session.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Files, dir.clone()));
        ed.session.at = crate::session::At::Side;
        ed.sidebar_list_files();
        settle_until(&mut ed, |ed| ed.session.sidebar.as_ref().is_some_and(|s| !s.loading));
        let rows = |ed: &Editor| -> Vec<String> {
            let s = ed.session.sidebar.as_ref().unwrap();
            s.lines().iter().map(|l| format!("{}{}", "  ".repeat(l.depth), l.label)).collect()
        };
        let sel = |ed: &Editor| ed.session.sidebar.as_ref().map(|s| s.selected);
        assert_eq!(rows(&ed), ["src", "  ui", "    card.rs", "  a.rs", "b.md"]);
        assert_eq!(sel(&ed), Some(2), "on the file being edited");
        feed(&mut ed, "j<ret>");
        assert_eq!(ed.doc().display_name_short(), "a.rs");
        assert!(!ed.session.is_at(crate::session::At::Side), "the code has the keys again");
        feed(&mut ed, " e");
        assert!(ed.session.is_at(crate::session::At::Side));
        feed(&mut ed, "kkh");
        assert_eq!(rows(&ed), ["src", "  ui", "  a.rs", "b.md"], "ui folded");
        feed(&mut ed, "l");
        assert_eq!(rows(&ed).len(), 5);
        feed(&mut ed, "<esc>");
        assert!(ed.session.sidebar.is_some() && !ed.session.is_at(At::Side), "still showing");
        feed(&mut ed, " e e");
        assert!(ed.session.sidebar.is_none(), "space e on the focused panel closes it");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The outline (tree-sitter, worked out on a worker): unfocused it follows the cursor; Enter on a
    /// definition jumps there. Only with the Rust grammar.
    #[test]
    fn side_panel_outline() {
        let Some(mut ed) = parsed_rust("fn a() {}\nstruct S;\nimpl S {\n    fn b() {}\n}\n") else { return };
        feed(&mut ed, " o");
        settle_until(&mut ed, |ed| ed.session.sidebar.as_ref().is_some_and(|s| !s.defs.is_empty()));
        let s = ed.session.sidebar.as_ref().unwrap();
        let rows: Vec<String> =
            s.lines().iter().map(|l| format!("{}{}", "  ".repeat(l.depth), l.label)).collect();
        assert_eq!(rows, ["a", "S", "impl S", "  b"]);
        feed(&mut ed, "<esc>3gg");
        ed.sidebar_sync(); // what every event ends with
        assert_eq!(ed.session.sidebar.as_ref().map(|s| s.selected), Some(2), "follows the cursor: impl S");
        feed(&mut ed, " ojj<ret>");
        let head = ed.doc().selection().primary().head;
        assert_eq!(ed.doc().text.byte_slice(head..head + 1).to_string(), "b", "on b's name");
    }

    /// Switching files is a jump too (`C-o` returns to the previous file at its spot); `ga` toggles
    /// between the last two files; closing a file drops its jumps.
    #[test]
    fn jumplist_across_files_and_last_accessed() {
        let dir = std::env::temp_dir().join(format!("tarae-jumps-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "a1\na2\na3\n").unwrap();
        std::fs::write(&b, "b1\nb2\n").unwrap();
        let mut ed = editor("");
        let name = |ed: &Editor| ed.doc().display_name_short();
        feed(&mut ed, &format!(":o {}<ret>jj:o {}<ret>j", a.display(), b.display()));
        assert_eq!(name(&ed), "b.txt");
        feed(&mut ed, "<C-o>");
        assert_eq!(name(&ed), "a.txt");
        assert_eq!(ed.doc().text.byte_to_line(ed.doc().selection().primary().head), 2, "at its spot");
        feed(&mut ed, "<tab>");
        assert_eq!(name(&ed), "b.txt");
        assert_eq!(ed.doc().text.byte_to_line(ed.doc().selection().primary().head), 1);
        feed(&mut ed, "ga");
        assert_eq!(name(&ed), "a.txt");
        feed(&mut ed, "ga");
        assert_eq!(name(&ed), "b.txt");
        feed(&mut ed, ":bc<ret>");
        assert_eq!(name(&ed), "a.txt");
        assert!(ed.session.views[0].jumps.iter().all(|j| j.doc == ed.doc().id), "b's jumps went with it");
        feed(&mut ed, "ga<C-o>");
        assert_eq!(name(&ed), "a.txt");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The start screen offers the files last open in this folder (the ones still there) as its first row;
    /// `r` opens them again. Without any, `r` on the start screen does nothing.
    #[test]
    fn the_start_screen_reopens_the_last_files() {
        let (dir, main, util, _) = follow_project("last-files");
        let gone = dir.join("gone.rs");
        let cwd = test_root();
        let mut ed = editor("");
        feed(&mut ed, "r");
        assert!(ed.welcome(), "nothing to continue: r does nothing");
        ed.restore = crate::restore::State::from_json(serde_json::json!({ "folders": {
            cwd.to_str().unwrap(): { "files": [main, gone, util], "current": 2 } } }));
        ed.find_restorable();
        assert_eq!(
            ed.session.restorable.as_deref(),
            Some(&[main.clone(), util.clone()][..]),
            "only files still there"
        );
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 100, 30).unwrap();
        let mut parser = vt100::Parser::new(30, 100, 0);
        parser.process(&bytes);
        let rows: Vec<String> = parser.screen().rows(0, 100).collect();
        let row = rows.iter().find(|r| r.contains("Continue where you left off")).expect("offered");
        assert!(row.trim_start().starts_with('r') && row.contains("main.rs +1"), "{row}");
        let at = |t: &str| rows.iter().position(|r| r.contains(t)).unwrap();
        assert!(at("Continue where you left off") < at("Open a file"), "first");
        feed(&mut ed, "r");
        let open: Vec<_> = ed.docs.iter().filter_map(|d| d.path.clone()).collect();
        assert_eq!(open, [main, util.clone()]);
        assert_eq!(ed.doc().path.as_deref(), Some(util.as_path()), "the one you were in");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Big files (loaded in the background) also return to the last position.
    #[test]
    fn background_load_restores_position() {
        let mut ed = editor("");
        let path = PathBuf::from("/nonexistent/tarae-big.txt");
        ed.restore = crate::restore::State::from_json(
            serde_json::json!({ "positions": { path.to_str().unwrap(): [4, 1] } }),
        );
        let id = ed.alloc_id();
        ed.docs.push(Document::placeholder(id, &path));
        ed.current = 1;
        ed.finish_loading(id, Ok(ropey::Rope::from_str("one\ntwo\nthree\n")), std::time::Duration::ZERO);
        assert_eq!(ed.doc().selection().primary().head, 4);
        assert_eq!(ed.doc().top, 1);
    }

    /// Recording while replaying another macro keeps only the replaying keys (not their expansion too),
    /// and that macro replays the other one again. A macro never replays itself.
    #[test]
    fn recording_skips_keys_a_replay_feeds_in() {
        let ed = run("a\nb\nc\n", "\"bQA!<esc>Qj\"aQ\"bqQj\"aq");
        assert_eq!(text(&ed), "a!\nb!\nc!\n");
        assert_eq!(ed.macros[&'a'].len(), 3, "\"bq only");
        let ed = run("a\nb\n", "QA!<esc>qQq");
        assert_eq!(text(&ed), "a!!\nb\n");
        assert_eq!(ed.status.as_ref().map(|s| s.1), Some(Severity::Error));
    }

    /// Theme list and preview are read on a worker thread; a preview that lands after its row was left is
    /// dropped; Esc brings the original back.
    #[test]
    fn theme_picker_lists_and_previews_off_the_main_thread() {
        let mut ed = editor("");
        let original = ed.theme.name.clone();
        feed(&mut ed, ":theme<ret>");
        assert!(ed.picker.is_none(), "list comes from a worker thread");
        settle(&mut ed);
        let current = |ed: &Editor| ed.picker.as_ref().and_then(|p| p.current()).map(|i| i.label.clone());
        assert_eq!(current(&ed), Some(original.clone()), "starts on the current theme");
        feed(&mut ed, "<down>");
        let row = current(&ed).unwrap();
        settle(&mut ed);
        assert_eq!(ed.theme.name, row, "previewed");
        feed(&mut ed, "<down><up><up>");
        while let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(300)) {
            ed.handle_event(ev);
        }
        assert_eq!(
            Some(ed.theme.name.clone()),
            current(&ed),
            "late previews of rows already left are dropped"
        );
        feed(&mut ed, "<esc>");
        assert_eq!(ed.theme.name, original);
    }

    /// `space G w` with a one-character cursor takes the word — even when that character is multi-byte.
    #[test]
    fn watch_takes_the_word_under_a_wide_cursor() {
        let prompt = |ed: &Editor| ed.prompt.as_ref().map(|p| p.text.clone());
        assert_eq!(prompt(&run("값x + 1", " Gw")), Some("값x".into()));
        assert_eq!(prompt(&run("a.b", "% Gw")), Some("a.b".into()), "a wider selection is taken as is");
    }

    /// `:w other` — a failed write keeps the old name; writing over an existing file isn't a conflict
    /// judged by the old file's stamp; the buffer takes the new name.
    #[test]
    fn write_as_new_path() {
        let dir = std::env::temp_dir().join(format!("tarae-write-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b) = (dir.join("a.txt"), dir.join("b.txt"));
        std::fs::write(&a, "a\n").unwrap();
        std::fs::write(&b, "old b\n").unwrap();
        let mut ed = editor("");
        ed.open(&a).unwrap();
        let a = ed.doc().path.clone().unwrap();
        feed(&mut ed, &format!("ix<esc>:w {}/nowhere/c.txt<ret>", dir.display()));
        assert_eq!(ed.status.as_ref().map(|s| s.1), Some(Severity::Error));
        assert_eq!(ed.doc().path.as_ref(), Some(&a), "failed write keeps the name");
        feed(&mut ed, &format!(":w {}<ret>", b.display()));
        assert_ne!(ed.status.as_ref().map(|s| s.1), Some(Severity::Error), "{:?}", ed.status);
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "xa\n");
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "a\n");
        assert!(ed.doc().path.as_ref().is_some_and(|p| p.ends_with("b.txt")));
        assert!(!ed.doc().is_modified());
        std::fs::remove_dir_all(&dir).ok();
    }
}
