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
use crate::llm::{self, Review, Warm};
use crate::lsp_editor::LspState;
use crate::movement as mv;
use crate::picker::{Action, Picker};
use crate::search;
use crate::selection::{Range, Selection};
use crate::syntax;
use crate::theme::Theme;
use crate::typed;

/// Files larger than this are read in the background. Small files take only a few ms synchronously,
/// so async would only add flicker.
const ASYNC_LOAD_BYTES: u64 = 1 << 20;

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
}

/// Two clicks on the same picker row this close together are a double click (it opens the row).
const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(400);

/// The longest a step's toast stays up without its end being said.
const STEP_MAX: std::time::Duration = std::time::Duration::from_secs(120);

/// Messages `:messages` keeps (and the most a card holds).
const MESSAGES_KEPT: usize = 300;

/// An error stays up until a key is pressed — but at least this long, so a key already on its way doesn't
/// take it before it's read.
const ERROR_MIN: std::time::Duration = std::time::Duration::from_secs(4);
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
        Toast { text, kind, at: std::time::Instant::now(), life, step: None, shown: false }
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
        if self.waiting() { std::time::Duration::ZERO } else { self.at.elapsed() }
    }
}

/// The working directory. Only `main` sets it, before the editor starts, so it's read once: `getcwd` goes
/// to the file system, and drawing asks for it every frame (the header, paths shown relative to it).
pub fn cwd() -> &'static std::path::Path {
    static CWD: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
    CWD.get_or_init(|| std::env::current_dir().unwrap_or_default())
}

/// Screen layout of the last frame (term.rs fills it on every draw).
#[derive(Clone, Debug, Default)]
pub struct Screen {
    /// Per pane: (view, pane rect, line-number gutter width, per screen row `ViewRow::origin`).
    #[allow(clippy::type_complexity)]
    pub panes: Vec<(crate::split::ViewId, crate::split::Rect, usize, Vec<(usize, usize, usize, usize)>)>,
    /// Column where the chat pane starts.
    pub chat_x: Option<usize>,
    /// First row and row count of the picker list.
    pub picker_list: Option<(usize, usize)>,
    /// Grammar download offer buttons: (row, start col, end col, key to feed when clicked).
    pub offer_buttons: Vec<(usize, usize, usize, char)>,
    /// Column where the picker's preview card starts (the wheel scrolls it there).
    pub picker_preview_x: Option<usize>,
    /// The floating docs card this frame (x, y, width, height) — drawing fills it in; the wheel scrolls it.
    pub popup: std::cell::Cell<Option<(usize, usize, usize, usize)>>,
    /// Clickable spots in the chat pane: (row, start col, end col, where it points) — drawing fills them in.
    pub chat_links: std::cell::RefCell<Vec<(usize, usize, usize, crate::chat::Link)>>,
    /// The minimized chat card (x, y, width, height) — a click on it brings the panel back.
    pub chat_mini: std::cell::Cell<Option<(usize, usize, usize, usize)>>,
    /// The code review's file rail: (row, file index) — a click opens that file.
    pub review_rail: std::cell::RefCell<Vec<(usize, usize)>>,
    /// The side panel's rows: (screen row, list row) — a click opens it.
    pub sidebar: std::cell::RefCell<Vec<(usize, usize)>>,
    /// The whole screen's width (None before the first frame — tests) · the chat's, the side panel's (0: not shown — folded away if it's open) and
    /// the bottom slot's (rows) as drawn last frame (`sizes.rs` starts a change from these).
    pub width: Option<usize>,
    pub chat_w: usize,
    pub side_w: usize,
    pub bottom_h: usize,
    /// The rows of code and bottom slot together, from row `text_top`.
    pub body_h: usize,
    pub text_top: usize,
    /// The terminal panel: its rows on screen (first, last) when it shows · the size a terminal gets.
    pub terminal_rows: Option<(usize, usize)>,
    pub terminal_size: std::cell::Cell<Option<(u16, u16)>>,
    /// The test panel: its rows (first, last) and how many of them the detail has — the wheel scrolls it.
    pub test_panel: std::cell::Cell<Option<(usize, usize, usize)>>,
    /// The in-buffer review's change in hand: its first and last screen rows.
    pub review_change: std::cell::Cell<Option<(usize, usize)>>,
    /// The in-buffer review's changes: every screen row of one (its header, its lines) — cards keep off them.
    pub review_rows: std::cell::RefCell<Vec<usize>>,
    /// The task panel's: (first, last, rows of output).
    pub task_panel: std::cell::Cell<Option<(usize, usize, usize)>>,
    /// A plugin's panel: (its first screen line, its last, its first row's line, the first row shown).
    pub plugin_panel: std::cell::Cell<Option<(usize, usize, usize, usize)>>,
    /// The note card and the thought card this frame (x, y, width, height).
    pub note_card: std::cell::Cell<Option<(usize, usize, usize, usize)>>,
    pub thought_card: std::cell::Cell<Option<(usize, usize, usize, usize)>>,
    /// The notification card (x, y, width, height) — None when there's none, or it's folded into the status
    /// line.
    pub toast_card: std::cell::Cell<Option<(usize, usize, usize, usize)>>,
}

/// One pane of a split: what it shows (a document) + its scroll. Cursor and selection are the document's.
#[derive(Clone, Debug)]
pub struct View {
    pub id: crate::split::ViewId,
    pub doc: DocId,
    pub top: usize,
    pub left: usize,
    /// Spots to come back to (`C-o`/`C-i`) · the document shown before this one (`ga`).
    pub jumps: crate::jumplist::JumpList,
    pub last_doc: Option<DocId>,
}

impl View {
    pub fn new(id: crate::split::ViewId, doc: DocId) -> Self {
        Self { id, doc, top: 0, left: 0, jumps: Default::default(), last_doc: None }
    }
}

pub struct Editor {
    pub docs: Vec<Document>,
    pub current: usize,
    /// Panes, the split tree and the focused pane. The focused pane's scroll is its document's top/left
    /// (synced every event).
    pub views: Vec<View>,
    pub split: crate::split::Split,
    pub focus: crate::split::ViewId,
    pub(crate) next_view: crate::split::ViewId,
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
    /// `.`: the insert session being recorded · the last finished one · replaying it now (repeat.rs).
    pub(crate) insert_rec: Option<crate::repeat::LastInsert>,
    pub(crate) last_insert: Option<crate::repeat::LastInsert>,
    pub(crate) repeating_insert: bool,
    /// `A-o` steps (doc, version, selection before, after) — `A-i` walks back while nothing else changed.
    pub(crate) expand_history: Vec<(DocId, u64, Selection, Selection)>,
    /// Replace across files: (pattern, listed matches per file) waiting for the replacement · the plan
    /// shown in the preview picker.
    pub(crate) replace_targets: Option<(String, crate::replace::Targets)>,
    pub(crate) replace_plan: Vec<crate::replace::FilePlan>,
    /// `space g f`: per file, its path, first changed line and diff against HEAD.
    pub(crate) changed_files: Vec<crate::gitmenu::Changed>,
    /// Blame (gitmenu.rs — `b` in the git tab).
    pub blame: crate::gitmenu::BlameState,
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
    /// Current git branch (filled by the watcher thread — for the status line).
    pub git_branch: Option<String>,
    /// Open picker (files, buffers, search results).
    pub picker: Option<Picker>,
    /// The last picker closed (`space '`).
    last_picker: Option<Picker>,
    picker_ids: u64,
    /// The live search's latest key (N4) — a search under way for an earlier one stops (its rg killed).
    live_search: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// In-flight LLM request/review (one at a time).
    pub review: Option<Review>,
    pub llm_warm: Warm,
    pub llm_generation: u64,
    /// The ask process awaiting an answer (killed on cancel).
    pub llm_pid: Option<u32>,
    /// Chat pane on the right (if open).
    pub chat: Option<crate::chat::Chat>,
    /// Claude's notes pinned to code (`notes.rs`).
    pub notes: crate::notes::Notes,
    /// What Claude changed per chat turn — undone as one (`checkpoint.rs`).
    pub checkpoints: crate::checkpoint::Checkpoints,
    /// (document, line, byte column) to put the cursor on once that document finishes loading.
    pub goto_after_load: Option<(DocId, usize, usize)>,
    /// The code review in progress (`codereview.rs`).
    pub code_review: Option<crate::codereview::CodeReview>,
    /// The review being opened is for Claude to review (`space v c`) — asked once it's open.
    pub review_ask: bool,
    /// The side panel — files or outline (`sidebar.rs`).
    pub sidebar: Option<crate::sidebar::Sidebar>,
    /// The tab it last showed (`C-w e` opens it again).
    pub side_last: crate::sidebar::Mode,
    /// Debug session · breakpoints (kept without a session) · session generation (drops late messages).
    pub dap: Option<crate::dap::Dap>,
    pub breakpoints: crate::dap::Breakpoints,
    /// Claude Code IDE server (if running).
    pub agent: Option<crate::agent::Agent>,
    /// WebAssembly plugins: their thread and the commands they registered (`None` until there are some).
    pub plugins: Option<crate::plugin::Plugins>,
    /// Lists used by `:` completion (folders, themes — filled by a worker thread) · folders being read.
    pub cmd_lists: crate::cmdline::Lists,
    /// Grammars already offered for download (this session).
    pub grammar_hinted: std::collections::HashSet<String>,
    /// Offers asking to download (arrival order — the front one gets keys, offer.rs).
    pub offers: Vec<crate::offer::Offer>,
    pub offer_seq: u64,
    /// The front card as drawn: (its id, its rev, since when) — `y` waits for it to settle.
    pub offer_seen: std::cell::Cell<Option<(u64, u64, std::time::Instant)>>,
    /// The card (id, rev) last in sight whole — drawn uncut, or read to its end in its `v` buffer (a weighty
    /// `y` waits for it).
    pub offer_whole: std::cell::Cell<Option<(u64, u64)>>,
    /// The card (id, rev) whose whole text `v` opened, in this buffer — seen whole once its last line has
    /// been on screen (opening it isn't reading it: a line 50 would go unseen).
    pub offer_view: std::cell::Cell<Option<(u64, u64, DocId)>>,
    /// Whether to show offers (off in tests — so they don't swallow y/n keys).
    pub offer_popups: bool,
    /// Diagnostic card closed with Esc on this line (doc, line) — back once the cursor leaves the line.
    pub diag_card_hidden: Option<(DocId, usize)>,
    cmd_loading: std::collections::HashSet<std::path::PathBuf>,
    /// Debugger watch expressions (evaluated at every stop — survive session changes).
    pub watches: Vec<String>,
    pub dap_generation: u64,
    /// Test pane (running or last result) · what ran last (was it debug) · generation — testing.rs.
    pub test_run: Option<crate::testing::TestRun>,
    /// The build task running or last run (task.rs) · the last one started (`space m m`) · run generation.
    pub task_run: Option<crate::task::Run>,
    pub task_last: Option<crate::task::Task>,
    pub task_generation: u64,
    /// The built-in terminal (terminal.rs) · its run generation (a replaced one's late output is dropped).
    pub terminal: Option<crate::terminal::Terminal>,
    pub terminal_generation: u64,
    /// A terminal is being started (on a worker) · what it printed and its exit code, if they came before
    /// it was in place.
    pub terminal_starting: bool,
    pub terminal_early: (Vec<u8>, Option<i32>),
    /// The list `]q` goes through (quickfix.rs).
    pub qlist: Option<crate::quickfix::List>,
    /// Git: the repository's status (the side panel's git tab) · a discard waiting for its second `d` · a
    /// commit message being written (gitpanel.rs).
    pub git_status: Option<crate::gitpanel::Status>,
    pub git_discard_asked: Option<(String, std::time::Instant)>,
    /// Git work waiting its turn (one at a time) · one running · reads of the status asked for (the latest
    /// lands).
    pub git_queue: std::collections::VecDeque<(crate::gitpanel::GitWork, crate::gitpanel::GitDone)>,
    pub git_busy: bool,
    pub git_reads: u64,
    pub committing: Option<crate::gitpanel::Committing>,
    pub test_last: Option<(crate::testing::Target, bool)>,
    pub test_generation: u64,
    /// If the current debug session is a test, that test (after it ends, F5 = rerun it).
    pub dap_test: Option<crate::testing::Target>,
    /// What the current debugging started as — to start it again once a missing debugger is installed.
    pub dap_resume: Option<crate::offer::Resume>,
    /// Preparing a Java debug session (asking jdtls for the main class and classpath) — java.rs.
    pub java_debug: Option<crate::java::DebugPrep>,
    /// Screen layout of the last frame — for mapping mouse coordinates to text positions.
    pub screen: Screen,
    /// Screen rows of the last frame (folded doc comments make screen row ≠ doc line — used by the mouse).
    pub view: Vec<crate::doccomment::ViewRow>,
    /// Where a drag started (selecting with the mouse).
    mouse_anchor: Option<usize>,
    /// The mouse was pressed on the terminal's screen — its drag and release go there, wherever they are.
    mouse_in_terminal: bool,
    /// Recently opened files (start screen — open with number keys).
    pub recent: Vec<PathBuf>,
    /// Session file (last position per file, open files per folder).
    pub session: crate::session::State,
    /// The files of this folder's last session that are still there — the start screen's `r`.
    pub last_session: Option<Vec<std::path::PathBuf>>,
    /// Theme before the theme preview (Esc restores it).
    theme_before: Option<crate::theme::Theme>,
    /// The notification card: messages that came while it was up (a burst is one card, newest on top) —
    /// it goes when the last of them has had its time.
    pub toasts: Vec<Toast>,
    /// Every message this session (`:messages`): when, kind, text.
    pub messages: Vec<(std::time::Instant, ToastKind, String)>,
    /// The disk watch round that's current — a newer one (an immediate check) takes over the watch.
    pub disk_poll_gen: u64,
    /// The disk round that waits for changes: what ends its wait (a newer round), and the files it watches.
    pub disk_round: Option<(crate::fswatch::Waker, Vec<(DocId, PathBuf)>)>,
    /// Set by a job result that changed nothing on screen (a disk round that found nothing) — when a batch
    /// of events is all such, the main loop doesn't draw it.
    pub unchanged: bool,
    /// `:redraw` — the next frame draws the whole screen, not only what changed (the main loop takes it).
    pub redraw: bool,
    /// A git diff is running on a worker thread.
    pub git_diffing: bool,
    /// Whether to read HEAD contents when opening a file (off in tests — keeps event order stable;
    /// turn on when needed).
    pub git_auto: bool,
    /// Where the chat's conversations are kept (`chats.rs`) — None in tests (they set a temp dir).
    pub chats_dir: Option<std::path::PathBuf>,
    /// The project file's held-back settings and the answer about them (`trust.rs`).
    pub project_trust: Option<crate::trust::Project>,
    /// Where answers about projects are kept — None in tests.
    pub trust_path: Option<std::path::PathBuf>,
    /// Where what's always allowed is kept (`allow.rs`) — None in tests (they set a temp file).
    pub allow_path: Option<std::path::PathBuf>,
    /// The last frame drawn and its size — what the user sees (`chat::attach_screen`).
    pub last_frame: Option<(Vec<u8>, u16, u16)>,
    /// Sizes you set — the chat's and side panel's widths, the bottom slot's height (`sizes.rs`), global and
    /// kept in `sizes_path` (None in tests: nothing read or written) · the latest write's number.
    pub sizes: crate::sizes::Sizes,
    pub sizes_path: Option<PathBuf>,
    pub sizes_seq: u64,
    /// The border the mouse is dragging (pressed on it, not let go yet).
    pub dragging: Option<crate::sizes::Border>,
    /// The last click on a picker's row (picker, row, when) — the same row again within `DOUBLE_CLICK` opens it.
    pub picker_click: Option<(u64, usize, std::time::Instant)>,
    /// The front card has the keys, given with `C-q` from wherever they were (#24) — the chat's draft, insert
    /// mode, a review stay as they were; an answer or Esc gives them back.
    pub card_keys: bool,
    /// `C-w z`: what fills the screen — drawn only while it still has the keys (`sizes::Editor::zoomed`).
    pub zoom: Option<crate::sizes::Zoom>,
    /// The bottom slot has tarae's keys (`C-w j`, a click): they scroll its output (`sizes::bottom_key`).
    pub bottom_focus: bool,
    /// The project — the folder tarae started in, links resolved (follow mode goes nowhere else). Tests with
    /// a project of their own in a temp folder set it.
    pub project_root: Option<PathBuf>,
    /// The terminal said its focus went elsewhere (CSI ?1004 — tmux passes it on only with `focus-events
    /// on`), and no input came since: the waiting animation rests (`tick_wanted`). A terminal that never
    /// says keeps it going.
    pub unfocused: bool,
    /// Claude's `screen` calls the user said yes to — answered with the next frame drawn (`seeing.rs`).
    pub screen_sends: Vec<crate::tools::Call>,
    /// The run whose denials `space m s` listed, and what could open (`blocked.rs`).
    pub blocked: Option<(crate::blocked::BlockedRun, Vec<crate::blocked::Choice>)>,
    /// Whether Claude's commands can be sandboxed here (`sandbox.rs`) — found with the first one.
    pub sandbox: crate::sandbox::Availability,
    /// `:restart` under way (`restart.rs`) — once `Ready`, main runs the new binary after the loop ends.
    pub restart: Option<crate::restart::Restart>,
    /// `:preflight` under way, or the last one while its run is in the task panel (`preflight.rs`).
    pub preflight: Option<crate::preflight::Preflight>,
    /// Undo history files being written (`undofile.rs`) — the process ending waits for them.
    pub undo_writes: crate::undofile::Writes,
    pub idle_save_gen: u64,
    /// When the scheduled animation tick lands (None: none scheduled).
    ticking: Option<std::time::Instant>,
    /// Values `:set` at runtime — they win even when the config file is reloaded (the top layer).
    pub overrides: Vec<(String, toml::Value)>,
    pub pending: Vec<Key>,
    count: Option<usize>,
    /// Starting snapshot of the open edit group. Stays open throughout insert mode.
    undo_group: Option<(DocId, Snapshot)>,
    next_id: DocId,
}

impl Editor {
    pub fn new(config: Config) -> Self {
        let events = Queue::default();
        let saver = crate::saver::Saver::new(events.sender());
        let mut ed = Self {
            docs: Vec::new(),
            current: 0,
            views: Vec::new(),
            split: crate::split::Split::Leaf(1),
            focus: 1,
            next_view: 2,
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
            review: None,
            picker: None,
            last_picker: None,
            picker_ids: 0,
            live_search: Default::default(),
            git_branch: None,
            lsp: LspState::default(),
            popup: None,
            popup_scroll: 0,
            completion: None,
            signature: None,
            llm_warm: Warm::default(),
            llm_generation: 0,
            llm_pid: None,
            chat: None,
            notes: Default::default(),
            checkpoints: crate::checkpoint::Checkpoints {
                dir: if cfg!(test) { None } else { crate::checkpoint::default_dir() },
                ..Default::default()
            },
            goto_after_load: None,
            code_review: None,
            review_ask: false,
            sidebar: None,
            side_last: crate::sidebar::Mode::Files,
            dap: None,
            breakpoints: Default::default(),
            agent: None,
            plugins: None,
            cmd_lists: Default::default(),
            grammar_hinted: Default::default(),
            offers: Vec::new(),
            offer_seen: Default::default(),
            offer_whole: Default::default(),
            offer_view: Default::default(),
            offer_seq: 0,
            offer_popups: !cfg!(test),
            diag_card_hidden: None,
            cmd_loading: Default::default(),
            watches: Vec::new(),
            dap_generation: 0,
            java_debug: None,
            test_run: None,
            task_run: None,
            task_last: None,
            task_generation: 0,
            qlist: None,
            terminal: None,
            terminal_generation: 0,
            terminal_starting: false,
            terminal_early: Default::default(),
            git_status: None,
            git_discard_asked: None,
            git_queue: Default::default(),
            git_busy: false,
            git_reads: 0,
            committing: None,
            test_last: None,
            test_generation: 0,
            dap_test: None,
            dap_resume: None,
            ticking: None,
            toasts: Vec::new(),
            messages: Vec::new(),
            disk_poll_gen: 0,
            disk_round: None,
            unchanged: false,
            redraw: false,
            git_diffing: false,
            git_auto: !cfg!(test),
            chats_dir: if cfg!(test) { None } else { crate::chats::default_dir() },
            project_trust: None,
            trust_path: if cfg!(test) { None } else { crate::trust::Store::path() },
            allow_path: if cfg!(test) { None } else { crate::allow::Store::path() },
            blocked: None,
            last_frame: None,
            project_root: std::fs::canonicalize(cwd()).ok(),
            unfocused: false,
            // (a few bytes, read before the first frame — it's drawn at the sizes you left)
            sizes: if cfg!(test) {
                Default::default()
            } else {
                crate::sizes::Sizes::read(crate::sizes::Sizes::path().as_deref())
            },
            sizes_path: if cfg!(test) { None } else { crate::sizes::Sizes::path() },
            sizes_seq: 0,
            dragging: None,
            picker_click: None,
            card_keys: false,
            zoom: None,
            bottom_focus: false,
            screen_sends: Vec::new(),
            sandbox: if cfg!(test) { crate::sandbox::Availability::Bypass } else { Default::default() },
            restart: None,
            preflight: None,
            undo_writes: Default::default(),
            idle_save_gen: 0,
            recent: Vec::new(),
            session: crate::session::State::default(),
            last_session: None,
            theme_before: None,
            screen: Screen::default(),
            mouse_anchor: None,
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
            replace_targets: None,
            replace_plan: Vec::new(),
            changed_files: Vec::new(),
            next_id: 0,
        };
        ed.new_scratch();
        ed.views.push(View::new(1, ed.docs[0].id));
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

    /// If the only buffer is the empty scratch created at startup, hand over its slot.
    fn replaceable_scratch(&self) -> bool {
        self.docs.len() == 1 && self.docs[0].path.is_none() && !self.docs[0].is_modified()
    }

    /// Adds a document and makes it current — taking over the startup scratch's slot if that's all there
    /// is (panes showing the scratch then show the new document).
    fn add_doc(&mut self, doc: Document) {
        if self.replaceable_scratch() {
            let (old, new) = (self.docs[0].id, doc.id);
            for v in self.views.iter_mut().filter(|v| v.doc == old) {
                (v.doc, v.top, v.left) = (new, 0, 0);
            }
            self.docs[0] = doc;
            self.current = 0;
        } else {
            self.docs.push(doc);
            self.current = self.docs.len() - 1;
        }
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

    /// `:tutor` — hands-on tutorial (a disposable practice buffer, markdown colors).
    pub fn open_tutor(&mut self) {
        let id = self.alloc_id();
        let mut doc = Document::from_str(id, include_str!("tutor.md"));
        doc.title = Some("tutor".into());
        doc.throwaway = true;
        self.add_doc(doc);
        if let Some(spec) = crate::syntax::spec("markdown") {
            self.load_language(id, spec);
        }
    }

    pub fn new_scratch(&mut self) {
        let id = self.alloc_id();
        self.docs.push(Document::from_str(id, ""));
        self.current = self.docs.len() - 1;
    }

    /// Opens a file. If already open, switches to that buffer.
    pub fn open(&mut self, path: &Path) -> Result<()> {
        let path = std::fs::canonicalize(path).or_else(|_| std::path::absolute(path))?;
        if let Some(i) = self.docs.iter().position(|d| d.path.as_deref() == Some(path.as_path())) {
            self.current = i;
            return Ok(());
        }
        let id = self.alloc_id();
        let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let doc = if size < ASYNC_LOAD_BYTES {
            Document::open(id, &path)?
        } else {
            // Big files load in the background — the first frame is instant regardless of file size.
            let p = path.clone();
            self.events.jobs().spawn(move || {
                let started = std::time::Instant::now();
                // Stamp before reading — a change during the read is caught by the next watch round
                let disk = crate::disk::stamp(&p);
                let result = crate::document::read_file(&p);
                let ms = started.elapsed().as_millis();
                move |ed: &mut Editor| {
                    if let Some(d) = ed.docs.iter_mut().find(|d| d.id == id) {
                        d.disk = disk;
                    }
                    ed.finish_loading(id, result, ms)
                }
            });
            Document::placeholder(id, &path)
        };
        self.add_doc(doc);
        self.session.opened = true;
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
            self.events.jobs().spawn(move || {
                crate::recent::save(&list);
                |_: &mut Editor| {}
            });
        }
        Ok(())
    }

    fn finish_loading(&mut self, id: DocId, result: Result<ropey::Rope>, ms: u128) {
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
                self.set_status(format!("{} loaded — {mb:.1} MB in {ms} ms", self.docs[i].display_name()));
                self.plugin_event(crate::plugin::PluginEvent::Opened, id);
            }
            Err(e) => {
                self.close_doc(id);
                self.set_error(format!("{e:#}"));
            }
        }
    }

    pub fn close_current(&mut self, force: bool) -> Result<(), String> {
        // (a `:w` a moment ago may still be being written — it counts)
        if self.doc().saving > 0 {
            self.flush_saves();
        }
        if !force && self.doc().is_modified() && !self.doc().throwaway {
            return Err(format!("unsaved changes in {} (use :bc! to discard)", self.doc().display_name()));
        }
        self.close_doc(self.docs[self.current].id);
        Ok(())
    }

    /// Closes a document (without asking) — notifies servers; panes showing it switch to another document.
    pub fn close_doc(&mut self, gone: DocId) {
        let Some(i) = self.docs.iter().position(|d| d.id == gone) else { return };
        self.plugin_closing(gone);
        self.lsp_did_close(gone);
        self.qlist_doc_closing(gone);
        self.docs.remove(i);
        if self.docs.is_empty() {
            self.new_scratch();
        }
        if self.current > i || self.current >= self.docs.len() {
            self.current = self.current.saturating_sub(1).min(self.docs.len() - 1);
        }
        let now = self.docs[self.current].id;
        for v in self.views.iter_mut() {
            if v.doc == gone {
                (v.doc, v.top, v.left) = (now, 0, 0);
            }
            v.jumps.remove_doc(gone);
            v.last_doc = v.last_doc.filter(|&d| d != gone);
        }
    }

    // ── Splits ────────────────────────────────────────────────────────────

    fn view_mut(&mut self, id: crate::split::ViewId) -> Option<&mut View> {
        self.views.iter_mut().find(|v| v.id == id)
    }

    /// Every event: focused pane = the current document and its scroll (even if the document changed by
    /// another path — opening a file, switching buffers).
    pub fn sync_focus_view(&mut self) {
        self.track_doc_switch();
        let (doc, top, left) = {
            let d = self.doc();
            (d.id, d.top, d.left)
        };
        let focus = self.focus;
        if let Some(v) = self.view_mut(focus) {
            (v.doc, v.top, v.left) = (doc, top, left);
        }
    }

    /// Moves focus to another pane: saves the current pane's scroll, then restores the new pane's
    /// document and scroll.
    pub fn focus_view(&mut self, id: crate::split::ViewId) {
        if id == self.focus {
            return;
        }
        self.sync_focus_view();
        let Some(v) = self.views.iter().find(|v| v.id == id).cloned() else { return };
        self.focus = id;
        if let Some(i) = self.docs.iter().position(|d| d.id == v.doc) {
            self.current = i;
            let d = &mut self.docs[i];
            (d.top, d.left) = (v.top, v.left);
        }
        self.completion = None;
        self.popup = None;
    }

    /// Splits the current pane — the new pane shows the same document at the same spot; focus moves to it.
    pub fn split_view(&mut self, dir: crate::split::Dir) {
        self.sync_focus_view();
        let id = self.next_view;
        self.next_view += 1;
        let Some(cur) = self.views.iter().find(|v| v.id == self.focus).cloned() else { return };
        self.views.push(View { id, ..cur });
        self.split.split(self.focus, id, dir);
        self.focus = id;
    }

    /// Closes the current pane — false if it's the last one (then `:q` quits the editor).
    pub fn close_view(&mut self) -> bool {
        if self.views.len() <= 1 {
            return false;
        }
        let gone = self.focus;
        let order = self.split.leaves();
        let i = order.iter().position(|v| *v == gone).unwrap_or(0);
        self.split.remove(gone);
        self.views.retain(|v| v.id != gone);
        let next = self.split.leaves()[i.saturating_sub(1).min(self.views.len() - 1)];
        self.focus_view(next); // syncing the gone pane finds nothing
        true
    }

    /// Keep only this pane.
    pub fn only_view(&mut self) {
        self.sync_focus_view();
        self.views.retain(|v| v.id == self.focus);
        self.split = crate::split::Split::Leaf(self.focus);
    }

    /// Next pane (`C-w w`) / by direction (`C-w h j k l` — using the last frame's pane layout).
    pub fn cycle_view(&mut self, delta: isize) {
        let order = self.split.leaves();
        let i = order.iter().position(|v| *v == self.focus).unwrap_or(0) as isize;
        let next = order[(i + delta).rem_euclid(order.len() as isize) as usize];
        self.focus_view(next);
    }

    /// Between the code's windows only (`C-w e` is the side panel's way in and out).
    /// `C-w h/j/k/l`: to the pane that way — below the lowest one, the bottom slot (`C-w k` from it, back to
    /// the code).
    pub fn focus_dir(&mut self, dx: i32, dy: i32) {
        if self.bottom_has_keys() {
            if dy < 0 {
                self.bottom_focus = false;
                self.plugin_panel_unfocus();
            }
            return;
        }
        let panes: Vec<_> = self.screen.panes.iter().map(|(v, r, _, _)| (*v, *r)).collect();
        if let Some(v) = crate::split::neighbor(&panes, self.focus, dx, dy) {
            self.focus_view(v);
        } else if dy > 0 {
            self.bottom_take_keys();
        }
    }

    /// `C-w e`: to the side panel from wherever the keys are — or back to the code from it. A closed panel
    /// opens at the tab it last showed.
    pub fn side_toggle_focus(&mut self) {
        match self.sidebar.as_mut() {
            Some(s) if s.focus => s.focus = false,
            Some(_) => self.side_focus(),
            None => self.sidebar_show(self.side_last),
        }
    }

    /// The keys to the open side panel, its row the one for where the code is.
    fn side_focus(&mut self) {
        if let Some(c) = self.chat.as_mut() {
            c.focused = false;
        }
        let Some(s) = self.sidebar.as_mut() else { return };
        s.focus = true;
        self.sidebar_sync();
        let here = self.sidebar.as_ref().and_then(|s| s.here(self));
        if let (Some(s), Some(h)) = (self.sidebar.as_mut(), here) {
            s.selected = h;
        }
    }

    pub fn cycle_buffer(&mut self, delta: isize) {
        let n = self.docs.len() as isize;
        self.current = (self.current as isize + delta).rem_euclid(n) as usize;
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
        let level = match kind {
            ToastKind::Error => "error",
            ToastKind::Warning => "warning",
            _ => "info",
        };
        crate::log::line("tarae", level, &msg);
        self.messages.push((std::time::Instant::now(), kind, msg.clone()));
        if self.messages.len() > MESSAGES_KEPT {
            self.messages.remove(0);
        }
        self.toast(kind, msg.clone());
        let severity = if kind == ToastKind::Error { Severity::Error } else { Severity::Info };
        self.status = Some((msg, severity));
    }

    /// A step under way (`fetching…`): its toast stays up until `step_done(key)` says it's over (at most
    /// `STEP_MAX`) — then it goes, and what came of it shows on its own. The same key again replaces it.
    pub fn set_step(&mut self, key: &'static str, msg: impl Into<String>) {
        let msg = msg.into();
        self.step_done(key);
        self.report(ToastKind::Info, msg.clone());
        if let Some(t) = self.toasts.iter_mut().rev().find(|t| t.text == msg && t.kind == ToastKind::Info) {
            t.step = Some(key);
            t.life = STEP_MAX;
        }
    }

    /// Step `key` is over (done or failed): its toast goes.
    pub fn step_done(&mut self, key: &'static str) {
        self.toasts.retain(|t| t.step != Some(key));
    }

    /// Record only, no toast — `status` is never drawn. Only for what the screen shows already (the
    /// diagnostic at the cursor, the selections just made); anything else the user has to see is a toast.
    pub fn note(&mut self, msg: impl Into<String>) {
        self.status = Some((msg.into(), Severity::Info));
    }

    fn toast(&mut self, kind: ToastKind, text: String) {
        // The card is gone (all had their time) → this one starts a new card
        if !self.toasts.iter().any(Toast::alive) {
            self.toasts.clear();
        }
        // The same message again: its time starts over, where it is (moving it would make the card jump)
        if let Some(t) = self.toasts.iter_mut().find(|t| t.text == text && t.kind == kind) {
            *t = Toast::new(kind, text);
            return;
        }
        self.toasts.push(Toast::new(kind, text));
        // (the card shows the newest few and counts the rest — all of them are in `:messages`)
        if self.toasts.len() > MESSAGES_KEPT {
            self.toasts.remove(0);
        }
    }

    /// `:messages` — this session's notifications, newest first: kind bar, text, how long ago.
    pub fn messages_picker(&mut self) {
        if self.messages.is_empty() {
            return self.set_status("no messages yet");
        }
        let ago = |at: std::time::Instant| match at.elapsed().as_secs() {
            s if s < 10 => "just now".to_string(),
            s if s < 60 => format!("{s} s ago"),
            s if s < 3600 => format!("{} min ago", s / 60),
            s => format!("{} h ago", s / 3600),
        };
        let items = self
            .messages
            .iter()
            .rev()
            .map(|(at, kind, text)| crate::picker::Item {
                label: text.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join(" · "),
                action: Action::Message,
                hint: ago(*at),
                glyph: Some((
                    "▎",
                    match kind {
                        ToastKind::Info => "ui.accent",
                        ToastKind::Success => "diff.plus",
                        ToastKind::Warning => "warning",
                        ToastKind::Error => "error",
                    },
                )),
            })
            .collect();
        self.open_picker(crate::picker::Picker::new("messages", items, false).without_preview(), None);
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
        let now = std::time::Instant::now();
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

    /// The card's opacity 0..1: fades in with its first message (150 ms), out with the last one's time (400 ms).
    pub fn toast_alpha(&self) -> f32 {
        let Some(oldest) = self.toasts.iter().map(Toast::age).max() else { return 0.0 };
        let left = self.toasts.iter().map(|t| t.life.saturating_sub(t.age())).max().unwrap_or_default();
        (oldest.as_secs_f32() / TOAST_FADE_IN.as_secs_f32())
            .min(left.as_secs_f32() / TOAST_FADE.as_secs_f32())
            .clamp(0.0, 1.0)
    }

    /// For the status line — pending count and keys.
    pub fn pending_display(&self) -> String {
        let mut s = self.count.map(|c| c.to_string()).unwrap_or_default();
        for k in &self.pending {
            s.push_str(&k.to_string());
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
    pub fn wraps(&self, doc: &Document) -> bool {
        match self.config.soft_wrap.as_str() {
            "always" => true,
            "never" => false,
            _ => {
                matches!(doc.lang_name(), None | Some("markdown" | "git-commit"))
            }
        }
    }

    /// Insert-mode auto-pairs for this document — empty when turned off.
    pub fn pairs(&self) -> &'static [(char, char)] {
        if !self.config.auto_pairs {
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
        // Input means the terminal has focus, whatever it said last (a focus-in lost on the way)
        if matches!(ev, Event::Key(_) | Event::Mouse(_) | Event::Paste(_)) {
            self.unfocused = false;
            crate::edits::taken_up(self);
        }
        match ev {
            Event::Key(k) => {
                self.handle_key(k);
                self.auto_save_idle();
                self.plugin_input_went_in();
            }
            Event::Mouse(m) => self.handle_mouse(m),
            Event::Focus(gained) => {
                self.unfocused = !gained;
                self.on_focus(gained)
            }
            Event::Resize => {}
            Event::Paste(text) => {
                self.handle_paste(text);
                self.plugin_input_went_in();
            }
            Event::Job(apply) => apply(self),
        }
        self.sync_focus_view();
        self.focus_sync();
        self.schedule_parses();
        self.lsp_flush();
        self.inlay_schedule();
        self.git_schedule();
        self.blame_schedule();
        self.sidebar_sync();
        self.picker_filter();
        self.conflicts_sync();
        self.disk_sync();
        self.agent_next_diff();
        crate::follow::watch(self);
        crate::notes::attach(self);
        crate::edits::next(self);
        crate::codereview::land(self);
        self.agent_selection_tick(false);
        self.plugin_front();
        self.schedule_tick();
    }

    // ── Mouse ────────────────────────────────────────────────────────────

    /// Press = place cursor (alt adds one) · drag = select · wheel = 3 lines. Clicking the chat pane
    /// focuses it; in a picker, wheel = move selection, click = open.
    /// Pasted text (the outer terminal's bracketed paste): to the built-in terminal when it has the keys; typed
    /// into a picker, prompt, or the chat (one line — newlines as spaces; the chat keeps them); else into
    /// the buffer as one edit.
    pub fn handle_paste(&mut self, text: String) {
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
        if let Some(c) = self.chat.as_mut().filter(|c| c.focused) {
            let text: String = text.chars().filter(|&c| c == '\n' || !c.is_control()).collect();
            c.input.insert_str(c.cursor, &text);
            c.cursor += text.len();
            return;
        }
        if self.review.is_some() {
            return self.set_status("finish reviewing the answer first");
        }
        // A file under review (a turn's, a branch's) took the keys when the turn ended — a paste in normal mode
        // there was meant for the chat (#31: it landed in the file as an unsaved edit). It goes to the chat;
        // without one it goes in, saying where. (In insert mode it's typed there on purpose.)
        let reviewing =
            self.code_review.is_some() && self.doc().review_base.is_some() && self.mode != Mode::Insert;
        if reviewing && let Some(c) = self.chat.as_mut() {
            let text: String = text.chars().filter(|&c| c == '\n' || !c.is_control()).collect();
            c.input.insert_str(c.cursor, &text);
            c.cursor += text.len();
            c.focused = true;
            return self.set_status("pasted into the chat — the file is under review (i pastes there)");
        }
        if reviewing {
            let name = self.doc().display_name();
            self.set_warning(format!("pasted into {name}, which is under review — u takes it back"));
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
        self.with_group(|cx| crate::commands::paste_text(cx, text));
    }

    pub fn handle_mouse(&mut self, m: crate::event::Mouse) {
        use crate::event::MouseKind as K;
        let s = self.screen.clone();
        let (x, y) = (m.x as usize, m.y as usize);
        self.status = None;
        if m.kind == K::Down
            && let Some(&(_, _, _, k)) = s.offer_buttons.iter().find(|b| b.0 == y && (b.1..b.2).contains(&x))
        {
            match k {
                'y' | 'a' | 'A' if !self.offer_seen_whole() => self.offer_see_all_first(),
                'y' if self.offer_settled() => self.offer_accept(),
                'v' => self.offer_view_all(),
                'a' if self.offer_settled() => self.offer_for_chat(),
                'A' if self.offer_settled() => self.offer_always(),
                'y' | 'a' | 'A' => {}
                'n' => self.offer_dismiss(),
                'N' => self.offer_never(),
                d => self.offer_to_front(d.to_digit(10).unwrap_or(0) as usize),
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
        if m.kind == K::Down
            && self.picker.is_none()
            && let Some(b) = self.border_at(x, y)
        {
            self.dragging = Some(b);
            return;
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
        // The review's file rail: a click opens that file, the wheel scrolls it (`CodeReview::fit` clamps)
        let on_rail =
            x < s.panes.iter().map(|p| p.1.x).min().unwrap_or(0) && !s.review_rail.borrow().is_empty();
        if on_rail && let Some(r) = self.code_review.as_mut() {
            match m.kind {
                K::ScrollDown => return r.scroll += 3,
                K::ScrollUp => return r.scroll = r.scroll.saturating_sub(3),
                _ => {}
            }
        }
        if m.kind == K::Down
            && let Some(&(_, i)) = s.review_rail.borrow().iter().find(|r| r.0 == y)
            && on_rail
        {
            crate::codereview::show(self, i, false);
            return;
        }
        // The terminal: the wheel scrolls back, a click gives it the keys (a click anywhere else takes them)
        let in_terminal = s.terminal_rows.is_some_and(|(y0, y1)| (y0..=y1).contains(&y))
            && s.chat_x.is_none_or(|cx| x < cx);
        // A press anywhere ends a selection whose release was lost (a pane switch mid-drag)
        let owned = std::mem::replace(&mut self.mouse_in_terminal, false) && m.kind != K::Down;
        if m.kind == K::Down
            && let Some(t) = self.terminal.as_mut()
        {
            t.selecting = false;
            if !in_terminal {
                t.focus = false;
                t.selection = None;
            }
        }
        // A press there, the wheel there, or the drag and release of a press there (clamped to its edge — a
        // program that has the mouse always gets its release)
        let wheel = matches!(m.kind, K::ScrollUp | K::ScrollDown);
        if owned || (in_terminal && (m.kind == K::Down || wheel)) {
            // Its screen starts a row below the title, a column in from the edge
            let cell = s.terminal_rows.and_then(|(y0, y1)| {
                let inside = in_terminal && y > y0 && x >= 1;
                (inside || owned).then(|| ((y.clamp(y0 + 1, y1) - y0 - 1) as u16, x.saturating_sub(1) as u16))
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
        // A plugin's panel: a click takes the keys there (on a row, selects it); the wheel moves through it
        if let Some((y0, y1, first, scroll)) = s.plugin_panel.get()
            && (y0..=y1).contains(&y)
            && s.chat_x.is_none_or(|cx| x < cx)
            && self.plugins.as_ref().is_some_and(|p| p.panel.is_some())
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
        // The test panel: the wheel scrolls its detail (the failure's message, or the output); a click gives it
        // the keys (`sizes::bottom_key`)
        let slot = self.slot();
        if let Some((y0, y1, rows)) = s.test_panel.get()
            && slot == Some(crate::sizes::Slot::Test)
            && (y0..=y1).contains(&y)
            && s.chat_x.is_none_or(|cx| x < cx)
        {
            match (m.kind, self.test_run.as_mut()) {
                (K::ScrollDown, Some(r)) => r.scroll_by(3, rows),
                (K::ScrollUp, Some(r)) => r.scroll_by(-3, rows),
                (K::Down, _) => self.bottom_take_keys(),
                _ => {}
            }
            return;
        }
        // The task panel: the same, over its output
        if let Some((y0, y1, rows)) = s.task_panel.get()
            && slot == Some(crate::sizes::Slot::Task)
            && (y0..=y1).contains(&y)
            && s.chat_x.is_none_or(|cx| x < cx)
        {
            match (m.kind, self.task_run.as_mut()) {
                (K::ScrollDown, Some(r)) => r.scroll_by(3, rows),
                (K::ScrollUp, Some(r)) => r.scroll_by(-3, rows),
                (K::Down, _) => self.bottom_take_keys(),
                _ => {}
            }
            return;
        }
        // The side panel: a click opens that row (a folder folds or unfolds), the wheel scrolls it
        if self.sidebar.is_some()
            && !self.review_rail_up()
            && x < s.panes.iter().map(|p| p.1.x).min().unwrap_or(0)
        {
            let row = s.sidebar.borrow().iter().find(|l| l.0 == y).map(|l| l.1);
            let sb = self.sidebar.as_mut().unwrap();
            match m.kind {
                K::Down => {
                    if let Some(r) = row {
                        self.sidebar_activate(r);
                    }
                }
                K::ScrollDown => sb.scroll += 3,
                K::ScrollUp => sb.scroll = sb.scroll.saturating_sub(3),
                _ => {}
            }
            return;
        }
        // The minimized chat: a trail row jumps there, anywhere else brings the panel back
        if let (Some((mx, my, mw, mh)), Some(c)) = (s.chat_mini.get(), self.chat.as_mut())
            && (mx..mx + mw).contains(&x)
            && (my..my + mh).contains(&y)
        {
            if m.kind == K::Down {
                let links = s.chat_links.borrow();
                match links.iter().find(|l| l.0 == y && (l.1..l.2).contains(&x)) {
                    Some((.., link)) => crate::chat::jump(self, link.clone()),
                    None => {
                        c.unfolded = true;
                        c.focused = true;
                    }
                }
            }
            return;
        }
        if let (Some(cx), Some(c)) = (s.chat_x, self.chat.as_mut())
            && x >= cx
        {
            match m.kind {
                K::Down => {
                    c.focused = true;
                    let links = s.chat_links.borrow();
                    if let Some((.., link)) = links.iter().find(|l| l.0 == y && (l.1..l.2).contains(&x)) {
                        crate::chat::jump(self, link.clone());
                    }
                }
                K::ScrollUp => c.scroll += 3,
                K::ScrollDown => c.scroll = c.scroll.saturating_sub(3),
                _ => {}
            }
            return;
        }
        if self.review.is_some() || self.welcome() {
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
        if matches!(m.kind, K::Down | K::ScrollUp | K::ScrollDown) {
            self.focus_view(pid);
        }
        // Scrolling the code yourself takes over from follow mode (clicks do too, via the cursor)
        if matches!(m.kind, K::ScrollUp | K::ScrollDown)
            && let Some(c) = self.chat.as_mut().filter(|c| c.follow.active() && c.busy())
        {
            c.follow.paused = true;
        }
        if pid != self.focus {
            return;
        }
        let title = usize::from(self.views.len() > 1);
        if y < rect.y + title {
            return;
        }
        let row = y - rect.y - title;
        if let Some(c) = self.chat.as_mut() {
            c.focused = false;
        }
        self.plugin_panel_unfocus();
        self.bottom_focus = false;
        self.completion = None;
        self.popup = None;
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
                doc.top = top;
                doc.set_selection(Selection::point(pos));
            }
            K::Down | K::Drag => {
                let (line, c0, x0, end) =
                    lines.get(row).copied().unwrap_or((doc.top + row, doc.left, 0, usize::MAX));
                let line = line.min(last);
                // A wrapped row: cells after its indent, never past its end (that's the next row's)
                let col =
                    (c0 + x.saturating_sub(rect.x + gutter).saturating_sub(x0)).min(end.saturating_sub(1));
                let pos = mv::pos_at_col(&doc.text, line, col, tab);
                let text = doc.text.clone();
                let anchor = self.mouse_anchor;
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
            }
            K::Up => self.mouse_anchor = None,
        }
    }

    /// When to show the start screen: a single empty unnamed buffer and nothing typed.
    pub fn welcome(&self) -> bool {
        self.docs.len() == 1
            && self.docs[0].path.is_none()
            && self.docs[0].text.len_bytes() == 0
            && !self.docs[0].is_modified()
            && self.mode == Mode::Normal
    }

    /// While waiting on something (claude answer), redraw again after a `TICK` — wave animation, elapsed
    /// time. A toast needs frames only while it fades: between its fade-in and fade-out one wake-up is
    /// scheduled for when the fade-out starts. Nothing to wait for, no tick (an idle editor uses 0 CPU).
    /// When the notification card next needs a frame: a `TICK` while it fades in or out, else when its
    /// fade-out starts — None without one. One waiting for a card (`Toast::waiting`) needs none.
    fn toast_wake(&self) -> Option<std::time::Duration> {
        let running = || self.toasts.iter().filter(|t| !t.waiting());
        let oldest = running().map(Toast::age).max()?;
        let left = running().map(|t| t.life.saturating_sub(t.age())).max();
        let steady = left.unwrap_or_default().saturating_sub(TOAST_FADE);
        let fading_in = oldest < TOAST_FADE_IN;
        Some(if fading_in || steady.is_zero() { TICK } else { steady })
    }

    /// When a frame is next wanted with nothing else happening: the waiting animation's next step while
    /// something is waited on, else a toast's (`toast_wake`) — None: no frame. Not the animation while the
    /// terminal says it's unfocused (`unfocused`): nobody sees it, and focus coming back draws it again.
    fn tick_wanted(&self) -> Option<std::time::Duration> {
        let waiting = self.review.as_ref().is_some_and(|r| !r.ready)
            || self.chat.as_ref().is_some_and(|c| c.busy())
            || self.dap.as_ref().is_some_and(|d| d.state == crate::dap::State::Building)
            || self.test_run.as_ref().is_some_and(|r| r.state == crate::testing::RunState::Running)
            || self.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running)
            || self.offer_busy();
        if waiting && !self.unfocused { Some(step_in(anim_clock().elapsed())) } else { self.toast_wake() }
    }

    fn schedule_tick(&mut self) {
        use std::time::{Duration, Instant};
        if !self.toasts.is_empty() && !self.toasts.iter().any(Toast::alive) {
            self.toasts.clear();
            self.unchanged = false; // gone from the screen — whatever event this is, it's drawn
        }
        let Some(delay) = self.tick_wanted() else { return };
        let at = Instant::now() + delay;
        // One already due by then serves (a later one doesn't: a new toast must start fading in now)
        if self.ticking.is_some_and(|t| t <= at + Duration::from_millis(5)) || cfg!(test) {
            return;
        }
        self.ticking = Some(at);
        self.events.jobs().spawn(move || {
            std::thread::sleep(delay);
            move |ed: &mut Editor| {
                if ed.ticking == Some(at) {
                    ed.ticking = None;
                } else {
                    // Overtaken by an earlier one (a new toast, an error let go): nothing to draw
                    ed.unchanged = true;
                }
            }
        });
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
        let mut picker = Picker::new("search", Vec::new(), false).grouped(root);
        picker.live = true;
        picker.query = pattern;
        self.open_picker(picker, None);
        self.live_search();
    }

    /// Where `space /` searches: the project (tarae's folder; a test's own).
    fn search_root(&self) -> PathBuf {
        self.project_root.clone().unwrap_or_else(|| cwd().to_path_buf())
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

    fn picker_key(&mut self, key: Key) {
        let Some(p) = self.picker.as_mut() else { return };
        // Anything but typing acts on the list — never on the last query's rows
        if !matches!((key.code, key.ctrl, key.alt), (Code::Char(_), false, false) | (Code::Backspace, ..)) {
            p.filter_now();
        }
        match (key.code, key.ctrl) {
            (Code::Esc, _) | (Code::Char('c'), true) => {
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
            // Places: keep the listed ones as the list (`]q`)
            (Code::Char('q'), true) => {
                self.qlist_from_picker();
                return;
            }
            // Global search results: replace what's listed
            (Code::Char('r'), true) if p.grep.is_some() => return self.replace_ask(),
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
                self.keep_last_picker();
                self.theme_before = None;
                if let Some(a) = action {
                    self.run_picker_action(a);
                }
                return;
            }
            (Code::Down | Code::Tab, _) | (Code::Char('n'), true) => p.move_by(1),
            (Code::Up | Code::BackTab, _) | (Code::Char('p'), true) => p.move_by(-1),
            (Code::PageDown, _) | (Code::Char('d'), true) => p.move_by(10),
            (Code::PageUp, _) | (Code::Char('u'), true) => p.move_by(-10),
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
        let result = match action {
            Action::Code(i) => {
                self.run_code_action(i);
                Ok(())
            }
            Action::ReplaceFile(n) => {
                self.replace_apply(&[n]);
                Ok(())
            }
            Action::Dir(_) | Action::Message => Ok(()), // folders open in the picker (picker_key)
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
            // Open it at its first change
            Action::ChangedFile(n) => match self.changed_files.get(n).cloned() {
                Some(c) => {
                    return self.run_picker_action(Action::Goto { path: c.path, line: c.line, col: 0 });
                }
                None => Ok(()),
            },
            Action::Jump { index, .. } => {
                self.jump_to_entry(index);
                Ok(())
            }
            Action::Buffer(id) => {
                if let Some(i) = self.docs.iter().position(|d| d.id == id) {
                    self.current = i;
                }
                Ok(())
            }
            Action::Open(path) => self.open(&path),
            Action::Command(name) => {
                self.run_command_by_name(name);
                Ok(())
            }
            Action::Typed(line) => typed::execute(self, &line).map_err(|e| anyhow::anyhow!(e)),
            Action::Theme(name) => typed::execute(self, &format!("set! theme {name}"))
                .map_err(|e| anyhow::anyhow!(e))
                .map(|_| {
                    self.set_success(format!("Theme {name} · saved to your config"));
                }),
            Action::Goto { path, line, col } => self.open_at(&path, line, col),
        };
        if let Err(e) = result {
            self.set_error(format!("{e:#}"));
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
                    // Grammar not downloaded yet — offer to download it (once per language per session)
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

    /// Which documents keep their parse trees (#36 — a big file's are ~15 MB, and a session keeps a dozen
    /// open): those on screen now and the `TREES_RECENT` seen most recently; any other seen less than
    /// `trees_unseen()` ago. The rest let theirs go (`Syntax::sleep`) and are parsed again when shown — a
    /// frame uncolored. One never shown yet (restored by the session) isn't parsed until it is. Every event,
    /// at `now`; an idle tarae isn't woken for it — the trees go at the next event.
    pub fn trees_follow_sight(&mut self, now: std::time::Instant) {
        let current = self.doc().id;
        let shown = |id: DocId, views: &[View]| id == current || views.iter().any(|v| v.doc == id);
        for d in &mut self.docs {
            if shown(d.id, &self.views) {
                d.seen = Some(now);
                if let Some(s) = d.syntax.as_mut() {
                    s.dormant = false;
                }
            }
        }
        // (the most recent few keep theirs however long ago — a handful of files you go between)
        let mut recent: Vec<(std::time::Instant, DocId)> =
            self.docs.iter().filter_map(|d| Some((d.seen?, d.id))).collect();
        recent.sort_unstable_by_key(|r| std::cmp::Reverse(r.0));
        recent.truncate(TREES_RECENT);
        let unseen = trees_unseen();
        for d in &mut self.docs {
            let keep = match d.seen {
                None => false,
                Some(t) => recent.iter().any(|r| r.1 == d.id) || now.saturating_duration_since(t) < unseen,
            };
            if let Some(s) = d.syntax.as_mut().filter(|s| !keep && !s.dormant) {
                s.sleep();
            }
        }
    }

    /// Starts a background parse for each edited document not already parsing (at the end of every event) —
    /// not those whose trees went (`trees_follow_sight`), until they're shown.
    pub fn schedule_parses(&mut self) {
        self.trees_follow_sight(std::time::Instant::now());
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
    pub fn reload_config(&mut self, config: Config, warnings: Vec<String>) {
        self.keymaps = config.keymaps;
        self.config = config.editor;
        self.theme = config.theme;
        for (path, v) in &self.overrides {
            let _ = crate::settings::apply(&mut self.config, path, v);
        }
        self.project_trust_set(config.project);
        self.ensure_theme();
        self.pending.clear();
        self.count = None;
        if warnings.is_empty() {
            self.set_status("config reloaded");
        } else {
            self.set_error(format!("config: {}", warnings.join("; ")));
        }
    }

    /// Whether the front card takes the keys: given it with `C-q`, or in normal mode with nothing else
    /// holding them (no prefix key, prompt, picker, review, the chat's input or the terminal).
    pub fn card_has_keys(&self) -> bool {
        self.card_keys
            || (self.mode == Mode::Normal
                && self.pending.is_empty()
                && self.prompt.is_none()
                && self.picker.is_none()
                && self.review.is_none()
                && !self.chat.as_ref().is_some_and(|c| c.focused)
                && !self.terminal.as_ref().is_some_and(|t| t.focus && t.visible && t.exited.is_none()))
    }

    /// `C-q` while a card is up gives it the keys from wherever they are (#24); while it has them, its
    /// answers answer it and Esc gives them back unanswered (#25) — other keys are swallowed, saying what it
    /// asks. True if the key was the card's.
    fn card_key(&mut self, key: Key) -> bool {
        let up = self.offers.first().is_some_and(|o| !o.installing());
        if !up || self.picker.is_some() || self.prompt.is_some() {
            self.card_keys = false;
            return false;
        }
        if key.ctrl && !key.alt && key.code == Code::Char('q') {
            self.card_keys = true;
            return true;
        }
        if !self.card_keys {
            return false;
        }
        if key.code == Code::Esc {
            self.card_keys = false;
            return true;
        }
        let front = self.offers.first().map(|o| o.id);
        if !self.offer_key(key) {
            let o = &self.offers[0];
            let asks = match (o.keeps(), o.lasts()) {
                (true, _) => "y, a, A or n",
                (false, true) => "y, a or n",
                _ => "y or n",
            };
            self.set_status(format!("the card asks {asks} — esc goes back"));
        }
        // Answered (or gone): the keys go back where they were
        if self.offers.first().map(|o| o.id) != front {
            self.card_keys = false;
        }
        true
    }

    pub fn handle_key(&mut self, key: Key) {
        // A key after an error has been read (`ERROR_MIN`) lets it go — it fades out
        for t in self.toasts.iter_mut().filter(|t| t.kind == ToastKind::Error) {
            let age = t.age();
            if age >= ERROR_MIN && t.life > age + TOAST_FADE {
                t.life = age + TOAST_FADE;
            }
        }
        // (whoever took the keys since — a click, a job — has them)
        self.focus_sync();
        // A card is answered from anywhere (#24): `C-q` gives it the keys — what you were typing stays
        if self.card_key(key) {
            return;
        }
        // The terminal has the keys: all of them go to its program (`C-\` gives them back)
        if self.picker.is_none() && self.prompt.is_none() && self.terminal_key(key) {
            return self.focus_sync();
        }
        // Cards: y·a·n in normal mode (something else open or a pending prefix key goes first) — not while
        // the chat's input has the keys: a card took the `n`s of a message being typed (`C-q` reaches it)
        if !self.offers.is_empty() && self.card_has_keys() && self.offer_key(key) {
            return;
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

    fn handle_key_inner(&mut self, key: Key) {
        self.status = None;
        // Esc in normal mode = clear search highlight (and still does its other work)
        if key.code == Code::Esc && self.mode == Mode::Normal && self.prompt.is_none() {
            self.search_hl = false;
            if self.popup.is_none() {
                self.diag_card_hidden = Some((self.doc().id, self.cursor_line()));
            }
        }
        // Floating text: C-d/C-u scroll it, other keys close it (esc only closes).
        if let Some(lines) = &self.popup
            && key.ctrl
        {
            match key.code {
                Code::Char('d') => return self.popup_scroll = (self.popup_scroll + 8).min(lines.len()),
                Code::Char('u') => return self.popup_scroll = self.popup_scroll.saturating_sub(8),
                _ => {}
            }
        }
        self.popup_scroll = 0;
        if self.popup.take().is_some() && key.code == Code::Esc {
            return;
        }
        // Keys a replayed macro feeds in aren't recorded — the `q` that replays them already was
        if self.replaying.is_empty()
            && !self.repeating_insert
            && let Some((_, keys)) = &mut self.recording
        {
            keys.push(key);
        }
        if llm::review_key(self, key) {
            return;
        }
        if self.picker.is_some() {
            self.picker_key(key);
            return;
        }
        if crate::chat::key(self, key) {
            return;
        }
        if crate::codereview::rail_key(self, key) {
            return;
        }
        if crate::sidebar::key(self, key) {
            return;
        }
        if crate::plugin::plugin_panel_key(self, key) {
            return;
        }
        if crate::sizes::bottom_key(self, key) {
            return;
        }
        // Start screen: `r` = the last session here
        if self.welcome()
            && self.pending.is_empty()
            && key.plain_char() == Some('r')
            && self.last_session.is_some()
        {
            self.restore_session();
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
            self.handle_prompt_key(key);
            return;
        }
        // A loading buffer ignores edit keys (`:` commands and buffer moves are allowed — :q·:bn while
        // reading).
        if self.doc().loading && key.plain_char() != Some(':') && self.pending.is_empty() {
            let name = self.doc().display_name();
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
                self.run(&cmds, count);
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
            '+' => crate::clipboard::copy(&self.events.jobs(), values.join("\n")),
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

    /// Doc-comment block shown folded on screen (normal mode, rendering on, language uses that marker,
    /// no horizontal scroll) — `j`/`k` step over it as one line.
    pub fn folded_doc_block_at(&self, line: usize) -> Option<(usize, usize)> {
        let doc = self.doc();
        let lang = doc.lang_name();
        if !self.config.render_doc_comments
            || self.mode != Mode::Normal
            || self.review.is_some()
            || doc.left > 0
            || !crate::doccomment::applies(lang)
        {
            return None;
        }
        crate::doccomment::block_at(&doc.text, line)
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
        let cwd = std::env::current_dir().unwrap_or_default();
        Some(crate::cmdline::complete(base, &cwd, &self.cmd_lists))
    }

    /// If the lists a candidate needs (folders, themes) aren't read yet, read them on a worker thread.
    fn cmdline_prefetch(&mut self) {
        let Some(p) = self.prompt.as_ref().filter(|p| p.kind == PromptKind::Command) else { return };
        let text = p.text.clone();
        let Some((name, _)) = text.split_once(char::is_whitespace) else { return };
        match crate::cmdline::find(name).map(|c| c.arg) {
            Some(crate::cmdline::Arg::File) => {
                let token = text.rsplit(char::is_whitespace).next().unwrap_or_default();
                let cwd = std::env::current_dir().unwrap_or_default();
                let (dir, _) = crate::cmdline::file_query(token, &cwd);
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
                    entries.sort();
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
                self.events.jobs().spawn(move || {
                    let out = std::process::Command::new("git")
                        .args(["branch", "-a", "--format=%(refname:short)"])
                        .stderr(std::process::Stdio::null())
                        .output()
                        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                        .unwrap_or_default();
                    let mut names: Vec<String> = out
                        .lines()
                        .map(str::trim)
                        .filter(|n| !n.is_empty() && !n.ends_with("/HEAD"))
                        .map(String::from)
                        .collect();
                    names.sort_by_key(|n| (n.contains('/'), n.clone()));
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

    fn handle_prompt_key(&mut self, key: Key) {
        let Some(prompt) = self.prompt.as_mut() else { return };
        match (key.code, key.ctrl || key.alt) {
            (Code::Esc, _) => self.cancel_prompt(),
            (Code::Enter, _) => {
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
            (Code::Char(c), false) => {
                prompt.cycle = None;
                prompt.text.push(c);
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
            self.doc_mut().top = top;
        }
    }

    fn submit_prompt(&mut self, kind: PromptKind, text: String) {
        match kind {
            PromptKind::Command => self.run(&[MappableCommand::Typed(text)], None),
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

    fn editor(text: &str) -> Editor {
        let mut ed = Editor::new(Config::default());
        ed.docs[0].text = ropey::Rope::from_str(text);
        // (tests make their projects in the temp folder — follow goes only into the project)
        ed.project_root = std::fs::canonicalize(std::env::temp_dir()).ok();
        ed
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
        let text = |ed: &Editor| ed.prompt.as_ref().map(|p| p.text.clone()).unwrap_or_default();
        feed(&mut ed, ":wr<tab>");
        assert_eq!(text(&ed), "write", "several → the first");
        feed(&mut ed, "<tab>");
        assert_ne!(text(&ed), "write", "pressing again → next candidate");
        feed(&mut ed, "<backtab>");
        assert_eq!(text(&ed), "write", "Shift-Tab goes backward");
        feed(&mut ed, "<esc>:tut<tab>");
        assert_eq!(text(&ed), "tutor", "only one — argless command, no trailing space");
        feed(&mut ed, "<esc>:them<tab>");
        assert_eq!(text(&ed), "theme ", "command with an argument gets a trailing space");
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
        ed.chat.as_mut().unwrap().state = crate::chat::State::Thinking;
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

    /// `:preflight` runs every step one after another in the task panel — the ones after a failure too —
    /// each with its row (how it ended, its time), a failure's places in `]q`, a pass with a note, and a
    /// one-line summary; the markers between steps aren't shown, each step's output is under its heading.
    #[test]
    fn preflight_runs_every_step_and_sums_up() {
        use crate::preflight::{State, Step};
        let dir = std::env::temp_dir().join(format!("tarae-preflight-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
        let step = |name: &str, cmd: &str| Step { name: name.into(), cmd: cmd.into(), note: None };
        let mut ed = editor("");
        ed.project_root = Some(dir.clone());
        assert!(ed.preflight_start().unwrap_err().contains("no preflight steps"));
        ed.config.preflight = vec![
            step("fmt", "echo formatted"),
            step("clippy", "echo 'a.rs:2:1: error: not like that' >&2; exit 3"),
            Step { note: Some("not measured".into()), ..step("perf", "echo 'not measured (machine busy)'") },
            step("test", "cd / && echo 'all fine'; exit 0"),
        ];
        ed.preflight_start().unwrap();
        assert!(ed.preflight_start().unwrap_err().contains("preflight is running"), "not twice at once");
        settle_for(&mut ed, |ed| {
            ed.task_run.as_ref().is_some_and(|r| r.state != crate::task::State::Running)
        });
        let p = ed.preflight.as_ref().unwrap();
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
            ed.qlist.as_ref().unwrap().items.iter().map(|i| (i.path.clone(), i.line)).collect();
        assert_eq!(listed, [(dir.join("a.rs"), 1)], "the failure's place");
        let out: Vec<&str> = ed.task_run.as_ref().unwrap().output.iter().map(|(_, l)| l.as_str()).collect();
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
        ed.git_auto = true;
        // (the git tab `space g` opens reads its status in the panel's folder — not this repository)
        let mut sb = crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, dir.clone());
        sb.focus = false;
        ed.sidebar = Some(sb);
        ed.open(&file).unwrap();
        settle_until(&mut ed, |ed| ed.doc().git_base.is_some());
        feed(&mut ed, "jmiwcTWO<esc>");
        settle_until(&mut ed, |ed| !ed.doc().git_hunks.is_empty());
        feed(&mut ed, " gd");
        assert_eq!(text(&ed), "one\ntwo\nthree\n", "reset (the hunk's index lines back)");
        feed(&mut ed, "u");
        settle_until(&mut ed, |ed| ed.doc().git_hunks.len() == 1);
        feed(&mut ed, " gs");
        settle_until(&mut ed, |ed| ed.doc().git_hunks.is_empty());
        let staged = String::from_utf8(git(&["diff", "--cached"])).unwrap();
        assert!(staged.contains("-two") && staged.contains("+TWO"), "{staged}");
        feed(&mut ed, "ggOnew<esc>"); // blame is on by default
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
        assert_eq!(ed.changed_files[0].line, 0, "the first change");
        assert!(!ed.changed_files[0].diff[0].lines.is_empty());
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
        assert!(ed.chat.as_ref().is_some_and(|c| c.focused), "space l = open and focus");
        feed(&mut ed, "explain<ret>");
        let c = ed.chat.as_ref().unwrap();
        assert_eq!(c.msgs[0].text, "explain");
        assert!(c.busy() && c.input.is_empty());
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        let c = ed.chat.as_ref().unwrap();
        assert_eq!(c.msgs[1].text, "Here:\n```\nBAR\n```\n", "chunks are concatenated");
        assert_eq!(c.last_code().as_deref(), Some("BAR\n"));
        // Second turn: same process; the same file version isn't resent in full
        feed(&mut ed, "again<ret>");
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
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
        assert!(ed.review.as_ref().is_some_and(|r| r.ready));
        feed(&mut ed, "y");
        assert_eq!(text(&ed), "foo BAR");
        // esc = back to the editor (pane stays), space L = close
        feed(&mut ed, "<space>l<esc>");
        assert!(ed.chat.as_ref().is_some_and(|c| !c.focused));
        feed(&mut ed, "<space>L");
        assert!(ed.chat.is_none());
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
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        let c = ed.chat.as_ref().unwrap();
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
        assert!(ed.chat.as_ref().unwrap().focused);
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
        ed.project_root = Some(dir.clone());
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
            ed.chat.as_ref().unwrap().follow.gaze.as_ref().is_some_and(|g| g.path.as_deref() == Some(&*memo))
        });
        assert_eq!(ed.doc().path.as_deref(), Some(util.as_path()), "not opened: stays where it was");
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        assert!(!ed.docs.iter().any(|d| d.path.as_deref() == Some(memo.as_path())), "never opened");
        assert_eq!(ed.views.len(), 2, "the panes as they were");
        let listed: Vec<PathBuf> =
            ed.qlist.as_ref().map(|l| l.items.iter().map(|i| i.path.clone()).collect()).unwrap_or_default();
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
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        let c = ed.chat.as_ref().unwrap();
        assert!(c.msgs.iter().any(|m| m.role == crate::chat::Role::Thought && m.text.contains("helpers")));
        assert!(c.follow.gaze.is_none(), "the gaze ends with the turn");
        assert_eq!(ed.doc().path.as_deref(), Some(lib.as_path()), "at the answer's reference");
        assert_eq!(cursor_line(&ed), 4);
        assert!(!ed.docs.iter().any(|d| d.path.as_deref() == Some(util.as_path())), "the preview closed");
        assert!(ed.chat.as_ref().unwrap().focused, "the chat keeps the keys");
        feed(&mut ed, "<esc><C-o>");
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
            ed.chat.as_ref().unwrap().follow.gaze.as_ref().is_some_and(|g| g.lines == Some(1..4))
        });
        assert_eq!((ed.doc().path.as_deref(), cursor_line(&ed)), (Some(lib.as_path()), 1));
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        assert_eq!(
            (ed.doc().path.as_deref(), cursor_line(&ed)),
            (Some(lib.as_path()), 1),
            "the answer's first"
        );
        if rust {
            settle_until(&mut ed, |ed| ed.qlist.is_some());
            let list = ed.qlist.as_ref().unwrap();
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
            ed.chat.as_ref().unwrap().follow.gaze.as_ref().and_then(|g| Some((g.why.clone()?, g.step?)))
        };
        settle_until(&mut ed, |ed| showing(ed).is_some_and(|s| s.1.0 == 1));
        assert_eq!(showing(&ed), Some(("where it begins".into(), (1, 3))));
        // The list it leaves says itself only once the tour is over (its card has the screen till then)
        let said = |ed: &Editor| ed.status.as_ref().is_some_and(|(m, _)| m.contains("in the list"));
        assert!(!said(&ed), "{:?}", ed.status);
        assert_eq!((ed.doc().path.as_deref(), cursor_line(&ed)), (Some(main.as_path()), 0));
        settle_until(&mut ed, |ed| showing(ed).is_some_and(|s| s.1.0 == 3));
        assert_eq!(
            (ed.doc().path.as_deref(), cursor_line(&ed)),
            (Some(lib.as_path()), 7),
            "never lib:1 (a mention)"
        );
        settle_until(&mut ed, |ed| {
            !ed.chat.as_ref().unwrap().busy() && !ed.chat.as_ref().unwrap().follow.touring()
        });
        let list = ed.qlist.as_ref().expect("the stops in the list");
        assert_eq!(
            list.items.iter().map(|i| i.message.as_str()).collect::<Vec<_>>(),
            ["where it begins", "the helpers", "where it ends"]
        );
        // The chat shows each tag as a link line
        let shown = crate::chat::displayed(
            &ed.chat
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
        assert_eq!(ed.views.len(), 2);
        let rect = crate::split::Rect { x: 0, y: 0, w: 40, h: 10 };
        let rows = |lines: std::ops::Range<usize>| lines.map(|l| (l, 0, 0, usize::MAX)).collect::<Vec<_>>();
        let other = ed.views.iter().find(|v| v.id != ed.focus).unwrap().id;
        ed.screen.panes = vec![(other, rect, 3, rows(4..9)), (ed.focus, rect, 3, rows(1..4))];
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
            let c = ed.chat.as_ref().unwrap();
            !c.busy() && !c.follow.touring() && c.follow.gaze.as_ref().is_some_and(|g| g.step == Some((2, 2)))
        });
        assert_eq!((ed.doc().path.as_deref(), cursor_line(&ed)), (Some(lib.as_path()), 7));
        assert!(crate::follow::stop_here(&ed).is_some(), "the stop is here");
        let said =
            ed.status.as_ref().is_some_and(|(m, _)| m.contains("Claude's walk-through: 2 in the list"));
        assert!(said, "the tour over, the list says itself: {:?}", ed.status);
        // The stop's card stays while the cursor is in it (the tour is over) — and goes when it leaves
        let card = |ed: &mut Editor| {
            crate::term::render(ed, &mut Vec::new(), 100, 30).unwrap();
            ed.screen.thought_card.get().is_some()
        };
        assert!(card(&mut ed), "the stop's card stays");
        feed(&mut ed, "<esc>gg");
        assert!(!card(&mut ed), "gone once the cursor leaves");
        feed(&mut ed, "8G");
        assert!(card(&mut ed), "back at the stop");
        feed(&mut ed, "<esc><space>nwhy here?<ret>");
        let n = ed.notes.list.last().expect("a note");
        assert_eq!((n.path.as_path(), n.lines.clone()), (lib.as_path(), 7..9), "the stop's lines");
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
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
        assert!(ed.chat.as_ref().unwrap().follow.active(), "on by default");
        settle_until(&mut ed, |ed| ed.doc().path.as_deref() == Some(util.as_path()));
        feed(&mut ed, "<esc>j");
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        assert!(ed.chat.as_ref().unwrap().follow.paused);
        assert_eq!(ed.doc().path.as_deref(), Some(util.as_path()), "stayed with the user");
        assert_eq!(cursor_line(&ed), 1);
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
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        assert_eq!(ed.notes.list.len(), 1);
        let n = &ed.notes.list[0];
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
        feed(&mut ed, "<esc><space>nwhy?<ret>");
        assert!(ed.notes.list[0].waiting);
        assert!(!ed.chat.as_ref().unwrap().focused, "replying doesn't move the keys to the chat");
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        let n = &ed.notes.list[0];
        let thread: Vec<_> = n.thread.iter().map(|e| (e.by, e.text.as_str())).collect();
        use crate::notes::By;
        assert_eq!(
            thread,
            [
                (By::Claude, "Line 3 repeats line 2."),
                (By::You, "why?"),
                (By::Claude, "Because the fixture counts lines.")
            ]
        );
        assert!(!n.waiting);
        let sent = logged("replied on note #1");
        assert!(
            sent.contains("<note id=\\\"1\\\"") && sent.contains("claude: Line 3 repeats line 2."),
            "{sent}"
        );
        assert!(sent.contains("replied on note #1"), "{sent}");
        let c = ed.chat.as_ref().unwrap();
        assert!(c.msgs.iter().any(|m| m.role == crate::chat::Role::User && m.chip.starts_with("¶ #1")));
        std::fs::remove_dir_all(&dir).ok();
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
        while ed.chat.as_ref().unwrap().busy() {
            let left = end.saturating_duration_since(std::time::Instant::now());
            let ev = ed.events.recv_timeout(left).expect("the run ends before the timeout");
            ed.handle_event(ev);
        }
        let r = ed.test_run.as_ref().expect("the run shows in the test pane");
        assert_eq!(r.target.label, "tests::adds");
        assert_eq!(r.state, crate::testing::RunState::Failed(Some(101)));
        let sent = std::fs::read_to_string(&log).unwrap();
        let answer = sent.lines().find(|l| l.contains("\"c7\"")).expect("answered");
        assert!(answer.contains("failed (exit 101)") && answer.contains("tests::adds"), "{answer}");
        assert!(answer.contains("FAILED tests::adds at ") && answer.contains("src/lib.rs:11"), "{answer}");
        assert!(answer.contains("left: 0") && answer.contains("right: 4"), "the why: {answer}");
        // The editor went to the failure, the card saying why
        assert_eq!((ed.doc().path.as_deref(), cursor_line(&ed)), (Some(lib.as_path()), 10));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `run_tests` doesn't cut a run short — the user's tests keep going and Claude is told to wait; a file with
    /// no runner is a tool error Claude can read.
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
                parse: "python".into(),
                build: None,
                debug_run: None,
                junit: None,
            },
        };
        ed.test_start(target);
        let (t, err) = text(call(&mut ed, json!({"path": "src/lib.rs"})));
        assert!(err && t.contains("already running in the editor (slow)"), "{t}");
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
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        assert!(ed.offers.is_empty(), "a check doesn't ask");
        let r = ed.task_run.as_ref().expect("it ran in the task panel");
        // (make exits 2 when a recipe fails)
        assert_eq!(
            (r.task.name.as_str(), &r.state),
            (command.as_str(), &crate::task::State::Failed(Some(2)))
        );
        assert!(ed.task_last.is_none(), "not the user's task to run again");
        let answer = tool_answer(&log);
        assert!(answer.contains("exit 2 in ") && answer.contains(&format!("$ {command}")), "{answer}");
        // (a check's places are in the user's list — and the report says so)
        assert!(
            answer.contains("1 errors, 0 warnings in the user's list")
                && answer.contains("lib.rs:2:5 error: mismatched types"),
            "{answer}"
        );
        assert!(ed.qlist.as_ref().is_some_and(|l| l.items.len() == 1), "and they are");
        assert!(answer.contains("Output:") && answer.contains("compiling calc"), "{answer}");
        assert!(!answer.contains("isError"), "{answer}");
        // The editor went to the error (the row's ending is `chat.rs`'s test: this claude sends no result)
        assert_eq!((ed.doc().path.as_deref(), cursor_line(&ed)), (Some(lib.as_path()), 1));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #27: a line of reads runs without a card — `cd`, pipes, `sh -c '…'`, a `|` inside quotes — and in the
    /// `cwd` given; a cwd outside the project, or not there, is refused. Harmless: reads of this repo's files.
    #[test]
    fn lines_of_reads_and_a_cwd_run_without_a_card() {
        use serde_json::json;
        let mut ed = editor("");
        let call = |ed: &mut Editor, args: serde_json::Value| {
            let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": "run", "arguments": args}});
            crate::tools::mcp(ed, 0, "c1", &msg)
        };
        let ran = |ed: &mut Editor, cmd: &str| {
            settle_for(ed, |ed| {
                !ed.offers.is_empty()
                    || ed
                        .task_run
                        .as_ref()
                        .is_some_and(|r| r.task.cmd == cmd && r.state != crate::task::State::Running)
            });
            assert!(ed.offers.is_empty(), "{cmd}: no card");
            let r = ed.task_run.as_ref().unwrap();
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
        // A cd into nothing: the shell would stay put and read `../../x` from the root — the card, not a run
        let cmd = "cd no-such-folder; cat ../../tarae-not-there";
        assert!(call(&mut ed, json!({ "command": cmd })).is_none());
        settle_for(&mut ed, |ed| {
            !ed.offers.is_empty() || ed.task_run.as_ref().is_some_and(|r| r.task.cmd == cmd)
        });
        assert!(ed.task_run.as_ref().is_none_or(|r| r.task.cmd != cmd), "it didn't run");
        assert!(
            matches!(&ed.offers[0].what, crate::offer::What::Run(r) if r.cmd == cmd && r.pattern.is_none())
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
                ed.task_run
                    .as_ref()
                    .is_some_and(|r| r.task.name == name && r.state != crate::task::State::Running)
            });
            ed.task_run.as_ref().unwrap().output.iter().map(|(_, l)| l.clone()).collect::<Vec<_>>().join("\n")
        };
        ed.task_start(task("mine"));
        assert_eq!(printed(&mut ed, "mine"), format!("cdpath={}", dir.display()), "the user's: as it is");
        assert!(ed.task_start_for(task("claudes"), crate::tools::For::User("t".into()), None));
        assert_eq!(printed(&mut ed, "claudes"), "cdpath=none");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #27: a card's `a` lets its pattern (`tools::pattern` — here the whole `echo one two`) run without
    /// asking for the rest of the chat: the same command the next turn has no card. The card says so.
    #[test]
    fn a_cards_this_chat_allows_its_pattern() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-pattern-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let mut ed = editor("");
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": "echo one two" }));
        feed(&mut ed, "<space>lsay it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        let q = ed.offers[0].question.clone();
        assert!(q.contains("a — also `echo one two …` unasked, this chat"), "{q}");
        assert!(ed.offers[0].lasts() && !ed.offers[0].keeps(), "a — this chat; never for good");
        // (`a`, as its key does once the card has been in sight)
        ed.offer_for_chat();
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        assert!(tool_answer(&log).contains("exit 0"));
        assert_eq!(ed.chat.as_ref().unwrap().run_allow, ["echo one two"]);
        ed.chat.as_mut().unwrap().focused = true;
        let answers = |log: &Path| std::fs::read_to_string(log).unwrap_or_default().matches("\"c7\"").count();
        feed(&mut ed, "again<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty() || answers(&log) == 2);
        assert!(ed.offers.is_empty(), "this chat: no card");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #32: a check piped into filters is the check — unasked where the check alone is (`echo one` stands in
    /// for it here, as an allowed pattern), and on a card its pattern is offered; a pipe into what isn't a
    /// filter (`tee`) still asks.
    #[test]
    fn a_check_piped_into_filters_runs_as_the_check() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-piped-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        // (no `sh -c '…'` here: the fake claude's JSON goes in single quotes — tools.rs tests that form)
        let piped = "echo one two 2>&1 | grep -E \"one|x\" | head -n 1";
        // Allowed: no card, it ran
        let mut ed = editor("");
        ed.config.llm.run_allow = vec!["echo one".into()];
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": piped }));
        feed(&mut ed, "<space>lsay it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty() || !ed.chat.as_ref().unwrap().busy());
        assert!(ed.offers.is_empty(), "a check through filters runs as the check would");
        let answer = tool_answer(&log);
        assert!(answer.contains("exit 0") && answer.contains("one two"), "{answer}");
        // Not allowed yet: the card offers the check's own pattern
        std::fs::remove_file(&log).ok();
        let mut ed = editor("");
        claude_calls(&mut ed, &dir, "run", &json!({ "command": piped }));
        feed(&mut ed, "<space>lsay it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        let q = ed.offers[0].question.clone();
        assert!(q.contains("a — also `echo one two …` unasked, this chat"), "{q}");
        // A pipe into a writer isn't a filter
        std::fs::remove_file(&log).ok();
        let mut ed = editor("");
        ed.config.llm.run_allow = vec!["echo one".into()];
        let tee = format!("echo one two | tee {}", dir.join("out").display());
        claude_calls(&mut ed, &dir, "run", &json!({ "command": tee }));
        feed(&mut ed, "<space>lsay it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        assert!(!dir.join("out").exists());
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
                ed.task_run
                    .as_ref()
                    .is_some_and(|r| r.task.cmd == cmd && r.state != crate::task::State::Running)
            });
        };
        let script = format!("sh {}", dir.join("x.sh").display());
        run(&mut ed, &script);
        let list = ed.qlist.as_ref().unwrap();
        assert_eq!((list.title.as_str(), list.items.len()), ("my build", 1), "not Claude's to replace");
        let r = ed.task_run.as_ref().unwrap();
        assert_eq!((r.places.len(), r.found), (1, (1, 0)), "its own places, for its report");
        let make = format!("make -s -f {}", dir.join("Makefile").display());
        run(&mut ed, &make);
        let list = ed.qlist.as_ref().unwrap();
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
        let dir = std::env::temp_dir().join(format!("tarae-run-reads-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let marker = dir.join("marker.txt");
        std::fs::write(&marker, "tarae-marker-content\n").unwrap();
        let card_or_done = |ed: &Editor| !ed.offers.is_empty() || !ed.chat.as_ref().unwrap().busy();
        // Inside the project: it just runs
        let mut ed = editor("");
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": "wc -l Cargo.toml" }));
        feed(&mut ed, "<space>lcount<ret>");
        settle_for(&mut ed, card_or_done);
        assert!(ed.offers.is_empty(), "inside: no card");
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        assert!(tool_answer(&log).contains("Cargo.toml"), "{}", tool_answer(&log));
        // Outside: the card says what it reads
        std::fs::remove_file(&log).ok();
        let command = format!("cat {}", marker.display());
        let mut ed = editor("");
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
        feed(&mut ed, "<space>lread it<ret>");
        settle_for(&mut ed, card_or_done);
        let q = ed.offers.first().map(|o| o.question.clone()).expect("a card for a read outside");
        assert!(q.contains(&format!("It reads outside the project:\n  {}", marker.display())), "{q}");
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 140, 40).unwrap();
        let mut parser = vt100::Parser::new(40, 140, 0);
        parser.process(&bytes);
        assert!(parser.screen().contents().contains("y Run   a This chat   A Always   n No"));
        let o = ed.offers.first().map(|o| (o.id, o.rev)).unwrap();
        ed.offer_seen.set(Some((o.0, o.1, std::time::Instant::now() - crate::offer::SETTLE)));
        feed(&mut ed, "a");
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        assert!(tool_answer(&log).contains("tarae-marker-content"), "{}", tool_answer(&log));
        assert!(ed.chat.as_ref().unwrap().allowed.reads(&marker));
        // The next turn, the same command: allowed now, no card
        ed.chat.as_mut().unwrap().focused = true;
        feed(&mut ed, "again<ret>");
        let answers = |log: &Path| std::fs::read_to_string(log).unwrap().matches("\"c7\"").count();
        settle_for(&mut ed, |ed| !ed.offers.is_empty() || answers(&log) == 2);
        assert!(ed.offers.is_empty(), "allowed this chat: no card");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Claude asks to run a command outside the sandbox: never without a reason (told at once, no card);
    /// with one, always a card — even for a command that would run unasked (`pwd`) — its reason on it, `y`
    /// or `n` only (`a` isn't an answer: never kept); `y` runs it outside, once, and the result says so.
    #[test]
    fn claude_asks_to_run_outside_the_sandbox_with_a_reason() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-outside-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
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
        settle_for(&mut ed, |ed| !ed.offers.is_empty() || !ed.chat.as_ref().unwrap().busy());
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
        ed.offer_seen.set(Some((o.0, o.1, std::time::Instant::now() - crate::offer::SETTLE)));
        feed(&mut ed, "a");
        assert_eq!(ed.offers.len(), 1, "never kept: `a` isn't an answer");
        feed(&mut ed, "y");
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        assert!(answer.contains("Ran outside the sandbox"), "{answer}");
        assert!(matches!(
            ed.task_run.as_ref().and_then(|r| r.guard.clone()),
            Some(crate::sandbox::Guard::Outside)
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Claude asks to see the screen: a card first (it takes the keys from an empty chat input), nothing
    /// goes before `y`, and what goes is the frame drawn after it — the code and the terminal panel's
    /// output, not the card. The card says the terminal panel is in it; with `llm.screen-terminal` off its
    /// rows are blank and the card says that instead. `a` lets the next call go unasked; `n` tells Claude no.
    #[test]
    fn claude_sees_the_screen_once_the_user_says_yes() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-screen-tool-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
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
                ed.terminal.as_ref().is_some_and(|t| t.parser.screen().contents().contains("term-marker"))
            };
            settle_for(&mut ed, shows);
            ed.terminal.as_mut().unwrap().focus = false;
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
        assert!(!ed.chat.as_ref().unwrap().focused, "the card has the keys");
        let s = draw(&mut ed);
        assert!(s.contains("y Once   a This chat   n No"), "{s}");
        assert_eq!(answers(&log), 0, "nothing goes before the answer");
        feed(&mut ed, "y");
        assert!(ed.offers.is_empty());
        settle_drawing(&mut ed, &|ed| !ed.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        assert!(answer.contains("screen-marker on this line") && answer.contains("term-marker"), "{answer}");
        assert!(answer.contains("the terminal panel included"), "{answer}");
        assert!(
            !answer.contains(crate::seeing::TITLE) && !answer.contains("isError"),
            "not the card: {answer}"
        );
        assert!(!ed.chat.as_ref().unwrap().sees, "only once");
        // The terminal panel left out: said on the card, blank in what goes; `a` — the next call is unasked
        let mut ed = with_terminal(false);
        std::fs::remove_file(&log).ok();
        let log = claude_calls(&mut ed, &dir, "screen", &json!({}));
        feed(&mut ed, "<space>lwhat do you see<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        let q = ed.offers[0].question.clone();
        assert!(q.contains("Not the terminal panel (llm.screen-terminal is off)"), "{q}");
        feed(&mut ed, "a");
        settle_drawing(&mut ed, &|ed| !ed.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        assert!(answer.contains("screen-marker") && !answer.contains("term-marker"), "{answer}");
        assert!(answer.contains(crate::seeing::LEFT_OUT), "{answer}");
        ed.chat.as_mut().unwrap().focused = true;
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
        feed(&mut ed, "n");
        settle_drawing(&mut ed, &|ed| !ed.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        assert!(answer.contains("said no") && answer.contains("isError"), "{answer}");
        std::fs::remove_dir_all(&dir).ok();
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
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
        feed(&mut ed, "<space>ltouch it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        let o = &ed.offers[0];
        assert!(matches!(&o.what, crate::offer::What::Run(r) if r.cmd == command));
        assert_eq!((o.title.as_str(), o.question.as_str()), ("Claude wants to run", command.as_str()));
        assert!(o.weighty(), "y must have been in sight a moment to count");
        // Another command while that one is asked about isn't stacked
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "run", "arguments": {"command": "rm -rf x"}}});
        let r = crate::tools::mcp(&mut ed, 0, "c1", &msg).expect("answered now");
        assert!(r["result"]["isError"] == true, "{r}");
        assert_eq!(ed.offers.len(), 1);
        ed.offer_dismiss();
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        assert!(answer.contains("the user chose not to run") && answer.contains("isError"), "{answer}");
        assert!(ed.task_run.is_none() && !made.exists());
        // Yes: it runs, and Claude hears how it ended
        std::fs::remove_file(&log).ok();
        let mut ed = editor("");
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
        feed(&mut ed, "<space>ltouch it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        assert!(!made.exists(), "nothing runs before the answer");
        ed.offer_accept();
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
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
        let command = format!("sleep 1; touch {}", dir.join("made").display());
        claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
        feed(&mut ed, "<space>lmake it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        let screen = draw(&mut ed);
        assert!(screen.contains("● waiting for you") && !screen.contains("● running"), "{screen}");
        ed.offer_accept();
        settle_for(&mut ed, |ed| ed.task_run.is_some());
        let screen = draw(&mut ed);
        assert!(screen.contains("● running") && !screen.contains("waiting for you"), "{screen}");
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
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
        ed.offer_popups = true;
        let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
        feed(&mut ed, "<space>ltouch it<ret>");
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        // A draft in the chat's input: what's typed is the draft's, the card says how to reach it
        ed.chat.as_mut().unwrap().focused = true;
        feed(&mut ed, "my draft y");
        let input = |ed: &Editor| ed.chat.as_ref().unwrap().input.clone();
        assert_eq!(input(&ed), "my draft y");
        assert!(!made.exists() && ed.offers.len() == 1);
        assert!(draw(&mut ed).contains("C-q to answer it"));
        // Esc leaves the input; another Esc (the card has the keys now) isn't a no
        feed(&mut ed, "<esc><esc>");
        assert_eq!(ed.offers.len(), 1, "Esc never answers a card");
        // Back in the input: C-q — the card has the keys, the draft stays
        ed.chat.as_mut().unwrap().focused = true;
        feed(&mut ed, "<C-q>");
        assert!(ed.card_keys);
        assert!(draw(&mut ed).contains("esc goes back, unanswered"));
        feed(&mut ed, "x");
        assert!(ed.status.as_ref().is_some_and(|s| s.0.contains("the card asks y or n")), "{:?}", ed.status);
        feed(&mut ed, "<esc>");
        assert!(!ed.card_keys && ed.offers.len() == 1 && ed.chat.as_ref().unwrap().focused);
        assert_eq!(input(&ed), "my draft y", "back where it was");
        // C-q, y (once the card has been in sight a moment): answered, the keys back in the draft
        feed(&mut ed, "<C-q>");
        draw(&mut ed);
        std::thread::sleep(crate::offer::SETTLE);
        draw(&mut ed);
        feed(&mut ed, "y");
        assert!(ed.offers.is_empty() && !ed.card_keys);
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        assert!(made.exists() && tool_answer(&log).contains("exit 0"));
        assert_eq!(input(&ed), "my draft y");
        assert!(ed.chat.as_ref().unwrap().focused);
        std::fs::remove_dir_all(&dir).ok();
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
        assert!(ed.chat.as_ref().unwrap().busy());
        // (the panel in full while Claude works, not folded to the following card)
        ed.chat.as_mut().unwrap().unfolded = true;
        feed(&mut ed, "second<ret>");
        let c = ed.chat.as_ref().unwrap();
        assert_eq!((c.queued.as_deref(), c.input.as_str()), (Some("second"), ""));
        let s = draw(&mut ed);
        assert!(s.contains("queued second") && s.contains("↑ edit queued"), "{s}");
        // ↑: back to edit — out of the queue; Enter queues it again
        feed(&mut ed, "<up>");
        let c = ed.chat.as_ref().unwrap();
        assert_eq!((c.queued.as_deref(), c.input.as_str()), (None, "second"));
        feed(&mut ed, " too<ret>");
        assert_eq!(ed.chat.as_ref().unwrap().queued.as_deref(), Some("second too"));
        assert_eq!(sent(&log), 1, "nothing goes while Claude works");
        // The turn ends: it goes
        settle_for(&mut ed, |_| sent(&log) == 2);
        let last = std::fs::read_to_string(&log).unwrap();
        assert!(last.lines().nth(1).unwrap().contains("second too"), "{last}");
        assert!(ed.chat.as_ref().unwrap().queued.is_none());
        std::fs::remove_dir_all(&dir).ok();
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

    /// #29: the git tab reads the status again when Claude edits, writes or runs something — not only when
    /// it's reopened ("nothing to commit — the tree is clean" over files Claude had changed). Mid-turn: the
    /// fake claude changes the file (standing in for its Edit), then sends the edit's result, then waits.
    #[test]
    fn the_git_tab_follows_claudes_edits() {
        let repo = git_repo("git-follow", &[("a.txt", "one\n")]);
        let file = repo.join("a.txt");
        let mut ed = editor("");
        ed.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone()));
        ed.git_refresh();
        settle_until(&mut ed, |ed| ed.git_status.as_ref().is_some_and(|s| s.unstaged.is_empty()));
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
        let dirty = |ed: &Editor| ed.git_status.as_ref().is_some_and(|s| !s.unstaged.is_empty());
        settle_for(&mut ed, |ed| dirty(ed) || !ed.chat.as_ref().unwrap().busy());
        assert!(dirty(&ed) && ed.chat.as_ref().unwrap().busy(), "the tab caught up mid-turn");
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
        ed.chat.as_mut().unwrap().focused = false;
        crate::codereview::start_in(&mut ed, dir.clone(), None);
        settle_until(&mut ed, |ed| ed.code_review.is_some() && ed.doc().review_base.is_some());
        let before = ed.doc().text.to_string();
        ed.handle_paste("meant for the chat".into());
        assert_eq!(ed.doc().text.to_string(), before, "the file under review is as it was");
        let c = ed.chat.as_ref().unwrap();
        assert!(c.focused && c.input == "meant for the chat", "{:?}", c.input);
        assert!(ed.status.as_ref().is_some_and(|s| s.0.contains("pasted into the chat")), "{:?}", ed.status);
        // Insert mode: typed there on purpose
        ed.chat.as_mut().unwrap().focused = false;
        feed(&mut ed, "i");
        ed.handle_paste("X".into());
        assert_ne!(ed.doc().text.to_string(), before);
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
        settle_for(&mut ed, |ed| ed.task_run.is_some());
        ed.task_close();
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        assert!(answer.contains("the command was stopped — the user stopped it"), "{answer}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A command already running isn't cut short, a turn that ends takes its card away, and one too long to
    /// show whole isn't put to the user.
    #[test]
    fn run_waits_its_turn() {
        use serde_json::json;
        let mut ed = editor("");
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
        let (t, err) = text(call(&mut ed, "git status"));
        assert!(err && t.contains("already running in the editor (slow)"), "{t}");
        let (t, err) = text(call(&mut ed, "git push"));
        assert!(err && t.contains("already running"), "{t}");
        assert!(ed.offers.is_empty());
        ed.task_close();
        // Asked about, then the turn ends: the card goes
        assert!(call(&mut ed, "git push").is_none());
        assert_eq!(ed.offers.len(), 1);
        crate::tools::turn_over(&mut ed);
        assert!(ed.offers.is_empty());
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
            ed.checkpoints.turn.push(crate::checkpoint::Touched {
                path: script.clone(),
                before: None,
                after: ropey::Rope::new(),
                unseen: true,
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
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        assert!(made.exists(), "{}", tool_answer(&log));
        std::fs::remove_file(&made).ok();
        // Changed after it was shown: not run, Claude hears why
        let (mut ed, log) = start(&body);
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        let sneaked = dir.join("sneaked");
        std::fs::write(&script, format!("{body}touch {}\n", sneaked.display())).unwrap();
        ed.offer_accept();
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        assert!(answer.contains("changed after the user saw it") && answer.contains("isError"), "{answer}");
        assert!(!made.exists() && !sneaked.exists() && ed.task_run.is_none());
        // Too long to show: not asked about
        let (mut ed, log) = start(&"echo x\n".repeat(200));
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        assert!(
            answer.contains("it has 200 lines — a card shows at most 16") && answer.contains("isError"),
            "{answer}"
        );
        assert!(ed.offers.is_empty() && ed.task_run.is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// No sandbox on this system: a command of Claude's — a check too — doesn't run quietly without one. Its
    /// card says so: `n` tells Claude, `y` runs it this once, `a` for the rest of the conversation (the
    /// chat's title says so meanwhile); a command that asks anyway says it on its own card.
    #[test]
    fn without_a_sandbox_claudes_commands_ask() {
        use crate::sandbox::Availability;
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-bare-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let made = dir.join("made");
        let command = format!("touch {}", made.display());
        let start = |command: &str| {
            std::fs::remove_file(dir.join("stdin.log")).ok();
            let mut ed = editor("");
            ed.sandbox = Availability::Missing("not on this system".into());
            ed.config.llm.run_allow = vec!["touch".into()];
            let log = claude_calls(&mut ed, &dir, "run", &json!({ "command": command }));
            feed(&mut ed, "<space>ltouch it<ret>");
            (ed, log)
        };
        // n: nothing ran, Claude hears why
        let (mut ed, log) = start(&command);
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        let o = &ed.offers[0];
        assert!(matches!(o.what, crate::offer::What::NoSandbox { .. }) && o.weighty());
        assert!(o.question.contains(&command) && o.question.contains("not on this system"), "{}", o.question);
        ed.offer_dismiss();
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        let answer = tool_answer(&log);
        assert!(answer.contains("chose not to run") && answer.contains("isError"), "{answer}");
        assert!(!made.exists() && ed.task_run.is_none());
        // y: this once — and it says how it ran
        let (mut ed, log) = start(&command);
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        ed.offer_accept();
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        assert!(made.exists());
        let answer = tool_answer(&log);
        assert!(answer.contains("Ran without a sandbox (not on this system)"), "{answer}");
        assert!(!crate::tools::unsandboxed(&ed), "only this once");
        std::fs::remove_file(&made).ok();
        // a: for this conversation — the next one doesn't ask, and the title says so
        let (mut ed, _) = start(&command);
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        ed.offer_for_chat();
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        assert!(made.exists() && crate::tools::unsandboxed(&ed));
        std::fs::remove_file(&made).ok();
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 150, 40).unwrap();
        let mut parser = vt100::Parser::new(40, 150, 0);
        parser.process(&bytes);
        assert!(parser.screen().contents().contains("› no sandbox"), "{}", parser.screen().contents());
        let msg = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call",
            "params": {"name": "run", "arguments": {"command": command}}});
        ed.chat.as_mut().unwrap().state = crate::chat::State::Thinking;
        ed.task_run = None;
        let generation = ed.chat.as_ref().unwrap().generation();
        assert!(crate::tools::mcp(&mut ed, generation, "c1", &msg).is_none());
        settle_for(&mut ed, |ed| {
            ed.task_run.as_ref().is_some_and(|r| r.state != crate::task::State::Running)
        });
        assert!(ed.offers.is_empty() && made.exists(), "agreed for the conversation: no card");
        std::fs::remove_file(&made).ok();
        // A command that asks anyway: its own card says there's no sandbox, and `y` there is enough
        // (`;` makes it more than a check)
        let (mut ed, _) = start(&format!("{command}; true"));
        settle_for(&mut ed, |ed| !ed.offers.is_empty());
        let o = &ed.offers[0];
        assert!(matches!(&o.what, crate::offer::What::Run(r) if r.unprotected));
        assert!(o.question.contains("No sandbox here (not on this system)"), "{}", o.question);
        ed.offer_accept();
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        assert!(ed.offers.is_empty() && made.exists(), "one card, not two");
        std::fs::remove_dir_all(&dir).ok();
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
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
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
        settle_for(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
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
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&kept).ok();
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
        run.guard = Some(crate::sandbox::Guard::In(policy.clone()));
        ed.task_run = Some(run);
        assert!(!screen(&mut ed).contains("sandbox blocked"), "nothing blocked: no line");
        let d = |op: &str, target: &str| crate::sandbox::Denial {
            process: "script".into(),
            op: op.into(),
            target: target.into(),
        };
        watch.record(d("file-write-data", "/dev/ttys004"));
        watch.record(d("network-outbound", "192.0.2.1:9"));
        let s = screen(&mut ed);
        assert!(s.contains("▲ sandbox blocked writing /dev/ttys004  +1"), "{s}");
        assert!(!s.contains("may be waiting"), "it printed just now");
        ed.task_run.as_mut().unwrap().last_output -= crate::sandbox::QUIET;
        assert!(screen(&mut ed).contains("— it may be waiting on what was blocked"));
        // The test pane, the same
        ed.task_run = None;
        let mut t = crate::testing::TestRun::finished("tests", Vec::new(), vec!["running 1 test".into()]);
        t.guard = Some(crate::sandbox::Guard::In(policy));
        ed.test_run = Some(t);
        let s = screen(&mut ed);
        assert!(
            s.contains("▲ sandbox blocked writing /dev/ttys004  +1") && !s.contains("may be waiting"),
            "{s}"
        );
    }

    /// What the sandbox blocked in Claude's command is the user's to open (`space m s`): a list of the
    /// folders the denied write could open and running it outside once; a folder's card `a` lets Claude's
    /// commands write there this conversation and runs the command again now; `A` keeps one for the
    /// project; outside runs it once with no sandbox. Each run again is the user's: Claude hears how it ended
    /// with the next message. Harmless: the command is an `echo`, the folders this test's temp ones.
    #[test]
    fn the_user_opens_what_the_sandbox_blocked() {
        let dir = std::env::temp_dir().join(format!("tarae-blocked-ks-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("cache/deep")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let (deep, cache) = (dir.join("cache/deep"), dir.join("cache"));
        let store = dir.join("state/sandbox.json");
        let mut ed = editor("");
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
        run.guard = Some(crate::sandbox::Guard::In(policy));
        run.by_claude = true;
        ed.task_run = Some(run);
        let screen = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 160, 40).unwrap();
            let mut parser = vt100::Parser::new(40, 160, 0);
            parser.process(&bytes);
            parser.screen().contents()
        };
        assert!(screen(&mut ed).contains("space m s open it"), "the panel says the key");
        // The list: the folders, widest last, then running it outside
        ed.chat.as_mut().unwrap().focused = false;
        feed(&mut ed, " ms");
        let rows: Vec<String> = ed.picker.as_ref().unwrap().items().iter().map(|i| i.label.clone()).collect();
        assert_eq!(rows[0], format!("allow writing in {}", crate::allow::tilde(&deep)), "{rows:?}");
        assert_eq!(rows[1], format!("allow writing in {}", crate::allow::tilde(&cache)), "{rows:?}");
        assert_eq!(rows.last().unwrap(), "run it outside the sandbox, once");
        // A card in sight long enough for an answer
        let settled = |ed: &mut Editor, title: &str| {
            assert_eq!(ed.offers.first().map(|o| o.title.clone()).as_deref(), Some(title));
            let s = screen(ed);
            let o = ed.offers.first().map(|o| (o.id, o.rev)).unwrap();
            ed.offer_seen.set(Some((o.0, o.1, std::time::Instant::now() - crate::offer::SETTLE)));
            s
        };
        let told = |ed: &Editor, what: &str| ed.checkpoints.tell.iter().any(|t| t.contains(what));
        let ran_again = |ed: &Editor| {
            ed.task_run.as_ref().is_some_and(|r| r.state != crate::task::State::Running && r.tell.is_none())
                && ed.checkpoints.tell.iter().any(|t| t.contains("tarae-rerun-marker"))
        };
        // `a`: this conversation — and it runs again now
        feed(&mut ed, "<ret>");
        let s = settled(&mut ed, crate::blocked::GRANT_TITLE);
        assert!(s.contains("y Once") && s.contains("a This chat") && s.contains("A Always"), "{s}");
        feed(&mut ed, "a");
        assert!(ed.chat.as_ref().unwrap().allowed.write.contains(&deep));
        assert!(told(&ed, "The user let your commands write in"));
        settle_until(&mut ed, ran_again);
        assert!(told(&ed, &format!("The user ran `{cmd}` again, letting it write in")));
        // `A` on the wider one: kept for the project
        ed.checkpoints.tell.clear();
        crate::typed::execute(&mut ed, "sandbox-grant 1").unwrap();
        settled(&mut ed, crate::blocked::GRANT_TITLE);
        feed(&mut ed, "A");
        let kept = |p: &Path| {
            crate::allow::Store::read(Some(&store))
                .get(&crate::allow::project())
                .write
                .contains(&p.to_path_buf())
        };
        settle_until(&mut ed, |ed| kept(&cache) && ran_again(ed));
        // Outside, once
        ed.checkpoints.tell.clear();
        crate::typed::execute(&mut ed, "sandbox-grant outside").unwrap();
        let s = settled(&mut ed, crate::blocked::OUTSIDE_TITLE);
        assert!(s.contains("y Run") && !s.contains("A Always"), "{s}");
        feed(&mut ed, "y");
        settle_until(&mut ed, ran_again);
        assert!(told(&ed, "again outside the sandbox, this once"));
        assert!(matches!(
            ed.task_run.as_ref().and_then(|r| r.guard.clone()),
            Some(crate::sandbox::Guard::Outside)
        ));
        std::fs::remove_dir_all(&dir).ok();
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
        let n = &ed.notes.list[0];
        assert_eq!((n.lines.clone(), n.waiting), (1..2, true));
        assert!(ed.chat.as_ref().is_some_and(|c| !c.focused), "the chat opens without taking the keys");
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        let n = &ed.notes.list[0];
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
        settle_until(&mut ed, |ed| {
            ed.chat.as_ref().unwrap().proc_alive() && ed.chat.as_ref().unwrap().generation() == 2
        });
        feed(&mut ed, "hi<ret>");
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        let c = ed.chat.as_ref().unwrap();
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
        ed.notes.focus = Some(3);
        assert_eq!(shown(&ed).as_deref(), Some("three"), "just pinned → that one");
        ed.notes.focus = None;
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
        assert_eq!(ed.cursor_line(), 39_999);
        assert!(ed.chat.as_ref().unwrap().follow.active(), "the load isn't the user taking over");
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

    /// Claude's Read, Grep and Glob (measured: allowed by name they read anywhere unasked — so they aren't):
    /// inside the project they're allowed at once; outside, a card says exactly what's read — `n` no, `a`
    /// this chat (what it covers then goes unasked), `A` always (kept for the project, `:sandbox-allowed`
    /// lists it, `:sandbox-disallow` takes it back), and a place that's everything (`/`) only once. Nothing
    /// is read here: the fake claude only asks, and its paths are this test's own temp folder.
    #[test]
    fn claudes_reads_outside_the_project_ask_first() {
        use crate::reads::TITLE;
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-reads-ask-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("out")).unwrap();
        std::fs::create_dir_all(dir.join("other")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let (out, other) = (dir.join("out"), dir.join("other"));
        let marker = out.join("marker.txt");
        std::fs::write(&marker, "tarae-marker\n").unwrap();
        let store = dir.join("state/sandbox.json");
        let project = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let mut ed = editor("");
        ed.allow_path = Some(store.clone());
        let asks = [
            ("Read", json!({"file_path": "Cargo.toml"})),
            ("Read", json!({"file_path": marker})),
            ("Grep", json!({"pattern": "tarae-marker", "path": out})),
            ("Read", json!({"file_path": marker})),
            ("Glob", json!({"pattern": "*.txt", "path": other})),
            ("Glob", json!({"pattern": "*", "path": "/"})),
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
            ed.offer_seen.set(Some((o.0, o.1, std::time::Instant::now() - crate::offer::SETTLE)));
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
        feed(&mut ed, "n");
        let a = answered(&mut ed, "p2");
        assert_eq!(a["behavior"], "deny");
        assert!(a["message"].as_str().unwrap().contains(&marker.display().to_string()), "{a}");
        let (q, _) = card(&mut ed);
        assert!(
            q.contains(&format!("searches {} and everything in it\nfor tarae-marker", out.display())),
            "{q}"
        );
        feed(&mut ed, "a");
        assert_eq!(answered(&mut ed, "p3")["behavior"], "allow");
        assert_eq!(answered(&mut ed, "p4")["behavior"], "allow", "in what `a` allowed: unasked");
        let (q, _) = card(&mut ed);
        assert!(q.contains(&format!("lists *.txt in {}", other.display())), "{q}");
        feed(&mut ed, "A");
        assert_eq!(answered(&mut ed, "p5")["behavior"], "allow");
        let (q, s) = card(&mut ed);
        assert!(q.contains("Only once") && s.contains("y Once") && !s.contains("A Always"), "not kept: {s}");
        feed(&mut ed, "A");
        assert!(ed.offers.iter().any(|o| o.title == TITLE), "A isn't an answer here");
        feed(&mut ed, "y");
        assert_eq!(answered(&mut ed, "p6")["behavior"], "allow");
        // Kept for this project; this chat's only in the chat
        let kept = |p: &Path| crate::allow::Store::read(Some(&store)).get(&project).reads(p);
        settle_until(&mut ed, |_| kept(&other));
        assert!(!kept(&out) && !kept(Path::new("/")));
        assert!(ed.chat.as_ref().unwrap().allowed.reads(&marker));
        // Taken back
        crate::typed::execute(&mut ed, &format!("sandbox-disallow {}", other.display())).unwrap();
        settle_until(&mut ed, |_| !kept(&other));
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
        settle_until(&mut ed, |ed| ed.review.is_some());
        assert!(!ed.chat.as_ref().unwrap().focused, "the keys go to the review");
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
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        assert!(ed.chat.as_ref().unwrap().follow.active(), "a review isn't the user taking over");
        std::fs::remove_dir_all(&dir).ok();
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
        settle_until(&mut ed, |ed| ed.review.is_some());
        feed(&mut ed, "y");
        settle_until(&mut ed, |ed| ed.review.is_some());
        feed(&mut ed, "y");
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        let c = ed.chat.as_ref().unwrap();
        assert!(c.msgs.iter().any(|m| m.text == "changed lib.rs, new.rs · space u undo"));
        assert_eq!(ed.checkpoints.list.len(), 1);
        // Claude writes what was accepted (here: saved as it would be)
        for d in ed.docs.iter_mut().filter(|d| d.path.is_some()) {
            d.save().unwrap();
        }
        assert!(new.exists());
        // The user edits away from Claude's change
        ed.open(&lib).unwrap();
        feed(&mut ed, "<esc>8GA!<esc>");
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
        assert_eq!(ed.checkpoints.tell.len(), 2, "Claude hears of both with the next message");
        assert!(ed.checkpoints.tell[0].contains("undid your changes from the turn \"two files please\""));
        feed(&mut ed, "<space>lwhat now?<ret>");
        // (the fake asks the same again: its edit no longer applies, its write changes nothing)
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        let sent = std::fs::read_to_string(&log).unwrap();
        let asked = sent.lines().rfind(|l| l.contains("what now?")).unwrap();
        assert!(asked.contains("<editor-note>") && asked.contains("brought back your changes"), "{asked}");
        assert!(ed.checkpoints.tell.is_empty());
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
        settle_until(&mut ed, |ed| ed.review.is_some());
        let r = ed.review.as_ref().unwrap();
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
        settle_until(&mut ed, |ed| ed.review.is_some());
        assert_eq!(ed.review.as_ref().unwrap().hunks.len(), 2);
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 120, 40).unwrap();
        let (_, y, _, h) = ed.screen.thought_card.get().expect("the follow card");
        let rows = ed.screen.review_rows.borrow().clone();
        assert!(rows.iter().any(|&r| r > 30), "the second change is near the bottom: {rows:?}");
        assert!(!rows.iter().any(|r| (y..y + h).contains(r)), "card {y}..{} over {rows:?}", y + h);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A conversation outlives tarae: kept after each turn, and the chat opened after a restart shows it and
    /// resumes claude's session (`--resume`); one claude no longer has starts fresh, the rows still shown.
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
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        assert_eq!(ed.chat.as_ref().unwrap().session.as_deref(), Some("s-1"));
        let kept = |dir: &Path| {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let all = crate::chats::load(&dir.join("chats"), &std::env::current_dir().unwrap());
                // (kept twice — the question when the session starts, then the answer: wait for the answer)
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
        settle_until(&mut ed, |ed| ed.chat.as_ref().unwrap().msgs.iter().any(|m| m.text == "Hello back."));
        let c = ed.chat.as_ref().unwrap();
        assert!(
            c.msgs.iter().any(|m| m.role == crate::chat::Role::User && m.text == "hello"),
            "the rows are back"
        );
        assert!(c.msgs.last().unwrap().text.starts_with("continued from just now"));
        assert_eq!(c.msgs.iter().filter(|m| m.text.starts_with("continued from")).count(), 1);
        let started = || std::fs::read_to_string(&argv).unwrap_or_default();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !started().lines().last().unwrap_or_default().contains("--resume s-1") {
            assert!(std::time::Instant::now() < end, "resumed: {}", started());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(!started().contains("--no-session-persistence"), "the chat's sessions are kept");
        drop(ed);
        // claude no longer has it: a new conversation, the rows still there
        let mut ed = editor("");
        fake(&mut ed, true);
        feed(&mut ed, "<space>l");
        settle_until(&mut ed, |ed| {
            ed.chat.as_ref().unwrap().msgs.iter().any(|m| m.text.contains("no longer has this conversation"))
        });
        let c = ed.chat.as_ref().unwrap();
        assert!(c.msgs.iter().any(|m| m.text == "Hello back."));
        assert_eq!(c.session, None);
        settle_until(&mut ed, |ed| ed.chat.as_ref().unwrap().proc_alive());
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
        let body: Vec<String> =
            [tool, call, edit_row, edit, result].iter().map(|e| format!("printf '%s\\n' '{e}'")).collect();
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
        settle_until(&mut ed, |ed| ed.review.is_some());
        assert_eq!(ed.review.as_ref().unwrap().reason, "step 2/3: Wire it into the config");
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        let p = ed.chat.as_ref().unwrap().plan.clone().expect("the plan");
        assert_eq!((p.title.as_str(), p.done(), p.current()), ("Add a flag", 1, Some(1)));
        let c = ed.chat.as_ref().unwrap();
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
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().models.is_empty());
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
        feed(&mut ed, "<esc>:model haiku<ret>");
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

    /// Declined → claude is told so and nothing changes.
    #[test]
    fn chat_edits_can_be_declined() {
        let (dir, main, ..) = follow_project("edit-deny");
        let mut ed = editor("");
        ed.open(&main).unwrap();
        let input = serde_json::json!({"file_path": main, "old_string": "line 2", "new_string": "LINE 2"});
        let log = fake_edit(&mut ed, &dir, "Edit", input, false);
        feed(&mut ed, "<space>lshout<ret>");
        settle_until(&mut ed, |ed| ed.review.is_some());
        feed(&mut ed, "n");
        let a = permission_answer(&log);
        assert_eq!(a["behavior"], "deny");
        assert!(a["message"].as_str().unwrap().contains("declined"));
        assert_eq!(text(&ed), "line 1\nline 2\nline 3\n");
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
        settle_until(&mut ed, |ed| ed.review.is_some());
        feed(&mut ed, "yn");
        let a = permission_answer(&log);
        assert_eq!(a["behavior"], "allow", "{a}");
        let accepted = "LINE 1\nline 2\nline 3\nline 4\nline 5\n";
        assert_eq!(a["updatedInput"]["content"], accepted);
        assert_eq!(text(&ed), accepted);
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
        settle_until(&mut ed, |ed| ed.review.is_some());
        feed(&mut ed, "y");
        let a = permission_answer(&log);
        assert_eq!(a["behavior"], "deny");
        assert!(a["message"].as_str().unwrap().contains("unsaved"), "{a}");
        assert_eq!(text(&ed), "Xline 1\nLINE 2\nline 3\n");
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
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
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        assert!(ed.review.is_none(), "no orphaned review");
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
        settle_until(&mut ed, |ed| ed.review.is_some());
        feed(&mut ed, "y");
        assert_eq!(permission_answer_to(&log, "p1")["behavior"], "allow");
        settle_until(&mut ed, |ed| ed.review.is_some());
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
        settle_until(&mut ed, |ed| ed.review.is_some());
        feed(&mut ed, "y");
        feed(&mut ed, "ggiX<esc>"); // (before p2 is read — events wait for settle_until)
        settle_until(&mut ed, |ed| ed.review.is_some());
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
        let mut ed = editor("");
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
        assert!(ed.chat.is_none());
        feed(&mut ed, "<space>va");
        ed.offer_accept();
        assert!(crate::edits::at_end(&ed));
        let c = ed.chat.as_ref().expect("the chat it's for");
        assert!(!c.focused, "the keys stay in the editor");
        assert!(c.msgs.iter().any(|m| m.text.contains("space v a turns it off")));
        assert!(chat_title(&mut ed).contains("review at end"), "{}", chat_title(&mut ed));
        assert_eq!(ed.checkpoints.tell.len(), 1);
        assert!(ed.checkpoints.tell[0].contains("when your turn ends"), "{:?}", ed.checkpoints.tell);
        feed(&mut ed, "<space>va");
        assert!(ed.offers.is_empty(), "off without asking");
        assert!(!crate::edits::at_end(&ed));
        assert!(!chat_title(&mut ed).contains("review at end"));
        assert!(ed.checkpoints.tell.is_empty(), "on then off before a message: nothing to tell");
        // Typed: on asks, off doesn't; a new chat starts with it off
        feed(&mut ed, ":review-at-end on<ret>");
        assert!(matches!(ed.offers.first().map(|o| &o.what), Some(crate::offer::What::ReviewAtEnd)));
        ed.offer_accept();
        assert!(crate::edits::at_end(&ed));
        feed(&mut ed, ":review-at-end on<ret>");
        assert!(ed.offers.is_empty(), "already on");
        crate::chat::reset(&mut ed);
        assert!(!crate::edits::at_end(&ed), "a new chat asks again");
        feed(&mut ed, ":review-at-end on<ret>:review-at-end off<ret>");
        assert!(ed.offers.is_empty(), "off takes the card away");
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
        ed.chat.as_mut().unwrap().edits.at_end = Some(dir.clone());
        feed(&mut ed, "write them<ret>");
        settle_until(&mut ed, |ed| ed.review.is_some());
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
        let turn = &ed.checkpoints.turn;
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
        settle_until(&mut ed, |ed| ed.review.is_some());
        assert_eq!(ed.doc().path.as_deref(), Some(outside.as_path()));
        feed(&mut ed, "y");
        // p5: unsaved edits in util.rs — a review, and it stays in the buffer
        settle_until(&mut ed, |ed| ed.review.is_some());
        assert_eq!(ed.doc().path.as_deref(), Some(util.as_path()));
        let kept = |p: &Path| ed.checkpoints.turn.iter().find(|t| t.path == p).map(|t| t.unseen);
        assert_eq!(kept(&build), Some(false), "seen in its review");
        feed(&mut ed, "y");
        let a = permission_answer_to(&log, "p5");
        assert!(a["behavior"] == "deny" && a["message"].as_str().unwrap().contains("unsaved"), "{a}");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&away).ok();
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
        let open = |ed: &Editor, p: &Path| ed.docs.iter().any(|d| d.path.as_deref() == Some(p));
        let mut ed = editor("");
        ed.project_root = Some(dir.clone());
        ed.open(&main).unwrap();
        let edit = json!({"file_path": outside, "old_string": "a = 1", "new_string": "a = 2"});
        fake_asks(&mut ed, &dir, &[("Edit", edit)], true);
        feed(&mut ed, "<space>l");
        ed.handle_event(Event::Key("c".parse().unwrap())); // (typed in the chat's input)
        feed(&mut ed, "hange it<ret>");
        settle_until(&mut ed, |ed| ed.review.is_some());
        assert_eq!(ed.doc().path.as_deref(), Some(outside.as_path()), "its review");
        let restarted = crate::restart::snapshot(&mut ed);
        let saved: Vec<&str> =
            restarted["docs"].as_array().unwrap().iter().filter_map(|d| d["path"].as_str()).collect();
        assert_eq!(saved, [main.to_str().unwrap()], "a restart doesn't bring it");
        ed.handle_event(Event::Key("y".parse().unwrap())); // (the review's answer isn't taking it up)
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "a = 2\n");
        assert!(!open(&ed, &outside), "closed with the turn");
        assert!(open(&ed, &main));
        // Taken up: a key in the code pane while it's in front — it stays when the turn ends
        ed.open(&outside).unwrap();
        ed.doc_mut().aside = true;
        ed.chat.as_mut().unwrap().focused = false;
        ed.handle_event(Event::Key("j".parse().unwrap()));
        crate::edits::turn_over(&mut ed);
        assert!(open(&ed, &outside), "yours now");
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
        ed.checkpoints.dir = store.map(Path::to_path_buf);
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
        ed.chat.as_mut().unwrap().edits.at_end = Some(dir.clone());
        feed(&mut ed, "three files<ret>");
        (ed, dir, [main, lib, new])
    }

    /// Its review is up and the file in front has its diff.
    fn reviewing(ed: &Editor) -> bool {
        ed.code_review.is_some()
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
        let c = ed.chat.as_ref().unwrap();
        assert!(!c.busy() && !c.focused, "the keys went to the review");
        assert!(c.msgs.iter().any(|m| m.text == "changed 3 files · reviewing them now"));
        // The chat beside it leaves no room for the rail: the header says which file of the review this is
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 110, 30).unwrap();
        let mut parser = vt100::Parser::new(30, 110, 0);
        parser.process(&bytes);
        let header = parser.screen().rows(0, 110).next().unwrap();
        assert!(header.contains("lib.rs  review 1/3"), "{header}");
        let r = ed.code_review.as_ref().unwrap();
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
        // ]g past lib.rs's one change: main.rs
        feed(&mut ed, "]g");
        settle_until(&mut ed, |ed| reviewing(ed) && ed.doc().path.as_deref() == Some(main.as_path()));
        assert_eq!(ed.cursor_line(), 1);
        // One change back (`space v u`, as the rail says): that's the user's edit now (the buffer, saved by
        // auto-save as usual)
        feed(&mut ed, "<space>vu");
        assert_eq!(text(&ed), "line 1\nline 2\nline 3\n");
        feed(&mut ed, "<space>vq");
        assert!(ed.code_review.is_none());
        feed(&mut ed, "<space>vt");
        settle_until(&mut ed, reviewing);
        assert_eq!(ed.code_review.as_ref().unwrap().files.len(), 3, "the turn's files again");
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
        settle_until(&mut ed, |ed| ed.checkpoints.gathering);
        let err = crate::checkpoint::step(&mut ed, true).unwrap_err();
        assert!(err.contains("a moment"), "{err}");
        settle_until(&mut ed, reviewing);
        assert!(crate::checkpoint::can_undo(&ed));
        std::fs::remove_dir_all(&dir).ok();
        // A review open already: it stays, the row says how to get to the turn's
        let (mut ed, dir, _) = turn_of_three("turn-review-busy", None);
        let other =
            crate::codereview::CodeReview::for_test(crate::codereview::Base::Head, dir.clone(), Vec::new());
        ed.code_review = Some(other);
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy() && !ed.checkpoints.gathering);
        assert_eq!(ed.code_review.as_ref().unwrap().base, crate::codereview::Base::Head);
        let c = ed.chat.as_ref().unwrap();
        assert!(c.msgs.iter().any(|m| m.text == "changed 3 files · space v t reviews them"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// What's kept of the turns in `store` for this folder, once a worker's save has `ok` (5 s at most).
    fn turns_kept(
        store: &Path,
        ok: impl Fn(&crate::checkpoint::Stored) -> bool,
    ) -> crate::checkpoint::Stored {
        let root = std::env::current_dir().unwrap();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let kept = crate::checkpoint::load(store, &root);
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
        ed.checkpoints.dir = Some(store.clone());
        crate::checkpoint::restore(&mut ed);
        settle_until(&mut ed, |ed| !ed.checkpoints.list.is_empty());
        assert_eq!(ed.checkpoints.list[0].question, "three files");
        feed(&mut ed, "<space>vt");
        settle_until(&mut ed, reviewing);
        assert_eq!(ed.code_review.as_ref().unwrap().files.len(), 3);
        feed(&mut ed, "<space>vq<space>u");
        assert!(!std::fs::read_to_string(&lib).unwrap().contains("LINE 9"));
        assert!(!new.exists());
        turns_kept(&store, |k| k.list.first().is_some_and(|c| c.undone));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&store).ok();
    }

    /// A turn cut short (tarae ended mid-turn — a crash) comes back as a turn of its own: what it wrote
    /// unseen against what was there before, so it can still be reviewed and taken back.
    #[test]
    fn a_turn_cut_short_comes_back() {
        let store = std::env::temp_dir().join(format!("tarae-turns-cut-{}", std::process::id()));
        let (dir, main, ..) = follow_project("turn-cut");
        let mut ed = editor("");
        ed.checkpoints.dir = Some(store.clone());
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
        ed.checkpoints.dir = Some(store.clone());
        crate::checkpoint::restore(&mut ed);
        settle_until(&mut ed, |ed| !ed.checkpoints.list.is_empty());
        let cp = &ed.checkpoints.list[0];
        assert_eq!(cp.question, "a turn cut short");
        let f = &cp.files[0];
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
        ed.checkpoints.dir = Some(store.clone());
        let edit = json!({"file_path": main, "old_string": "line 2", "new_string": "LINE 2"});
        let log = fake_asks(&mut ed, &dir, &[("Edit", edit)], true);
        feed(&mut ed, "<space>l");
        ed.chat.as_mut().unwrap().edits.at_end = Some(dir.clone());
        let held = crate::checkpoint::hold_saves();
        feed(&mut ed, "edit it<ret>");
        settle_until(&mut ed, |ed| !ed.checkpoints.turn.is_empty());
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
        turns_kept(&store, |k| k.turn.iter().any(was) || k.list.iter().any(|c| c.files.iter().any(was)));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&store).ok();
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
        let mut sb = crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, dir.clone());
        sb.focus = false;
        ed.sidebar = Some(sb);
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
        assert!(ed.code_review.is_some() && !ed.review_rail_up(), "the review open, behind the tab");
        assert!(ed.sidebar.as_ref().is_some_and(|s| s.focus && s.mode == crate::sidebar::Mode::Git));
        assert!(s.contains("review behind") && s.contains("space g back"), "{s}");
        assert!(!s.contains("undo the whole turn"), "the rail gave the column: {s}");
        assert!(s.contains("review 1/3"), "the status line says which file: {s}");
        feed(&mut ed, "jkd");
        assert_eq!(text(&ed), before, "the keys are the tab's");
        // `space g` again: the rail, with the keys
        feed(&mut ed, "<space>g");
        assert!(ed.review_rail_up() && ed.code_review.as_ref().unwrap().focus, "the rail has the keys");
        assert!(!ed.sidebar.as_ref().unwrap().focus);
        let s = screen(&mut ed);
        assert!(s.contains("m viewed, on to the next") && !s.contains("review behind"), "{s}");
        feed(&mut ed, "d");
        assert_eq!(text(&ed), before, "the change is still there");
        // Esc in the tab: the same
        feed(&mut ed, "<esc><space>g");
        assert!(!ed.review_rail_up());
        feed(&mut ed, "<esc>");
        assert!(ed.review_rail_up() && ed.code_review.as_ref().unwrap().focus, "Esc: the rail again");
        // The tab closed (`q`): the rail is back by itself
        feed(&mut ed, "<esc><space>gq");
        assert!(ed.sidebar.is_none() && ed.review_rail_up());
        assert!(screen(&mut ed).contains("undo the whole turn"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A throwaway git repository: `files` committed on `main`, then `run` on top.
    fn git_repo(name: &str, files: &[(&str, &str)]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tarae-review-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
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

    /// The git tab: changes and staged files as sections; `s` stages the file under the selection, `c` opens
    /// the message buffer, `:w` commits it (and closes it), `d` twice deletes an untracked file.
    #[test]
    fn git_tab_stage_commit_discard() {
        let repo = git_repo("gittab", &[("a.txt", "one\n")]);
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(repo.join("b.txt"), "new\n").unwrap();
        let mut ed = editor("");
        ed.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone()));
        ed.git_refresh();
        settle_until(&mut ed, |ed| ed.git_status.is_some());
        let rows = |ed: &mut Editor| -> Vec<String> {
            ed.sidebar_sync();
            let s = ed.sidebar.as_ref().unwrap();
            s.lines().iter().map(|l| format!("{}{} {}", "  ".repeat(l.depth), l.label, l.hint)).collect()
        };
        assert_eq!(rows(&mut ed), ["Changes 2", "  a.txt +1", "  b.txt +1"]);
        ed.sidebar.as_mut().unwrap().selected = 1;
        feed(&mut ed, "s");
        settle_until(&mut ed, |ed| ed.git_status.as_ref().is_some_and(|s| !s.staged.is_empty()));
        assert_eq!(rows(&mut ed), ["Changes 1", "  b.txt +1", "Staged 1", "  a.txt +1"]);
        feed(&mut ed, "c");
        settle_until(&mut ed, |ed| ed.committing.is_some());
        assert_eq!(ed.doc().display_name_short(), "COMMIT_EDITMSG");
        feed(&mut ed, "add two<esc>:w<ret>");
        // (a status read meanwhile can show it committed before the commit's own result closes the buffer)
        settle_until(&mut ed, |ed| {
            ed.committing.is_none()
                && ed.git_status.as_ref().is_some_and(|s| s.staged.is_empty())
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
        // b.txt: untracked — the first d asks, the second deletes
        ed.sidebar.as_mut().unwrap().focus = true;
        ed.sidebar.as_mut().unwrap().selected = 1;
        feed(&mut ed, "d");
        assert!(repo.join("b.txt").exists());
        feed(&mut ed, "d");
        settle_until(&mut ed, |_| !repo.join("b.txt").exists());
        std::fs::remove_dir_all(&repo).ok();
    }

    /// Sizes you set (U1): `C-w >`/`<` on the chat — from the chat's empty input too (with text there, `C-w`
    /// deletes a word) — and on the side panel when it has the keys; `C-w +`/`-` on the bottom slot, which
    /// then wins over its panel's own height; kept in a file (global), shown clamped on a smaller screen
    /// without changing what's kept; `C-w =` makes all automatic again.
    #[test]
    fn sizes_you_set_are_kept_and_clamped() {
        let dir = std::env::temp_dir().join(format!("tarae-sizes-keys-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
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
        assert!(ed.chat.as_ref().unwrap().focused);
        feed(&mut ed, "<C-w>>");
        draw(&mut ed, 140);
        assert_eq!((ed.sizes.chat, ed.screen.chat_w), (Some(60), 60));
        assert!(ed.chat.as_ref().unwrap().input.is_empty(), "nothing typed into the chat");
        // With text in the input, C-w deletes a word as before
        feed(&mut ed, "two words<C-w>");
        assert_eq!(ed.chat.as_ref().unwrap().input, "two ");
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
        ed.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Files, dir.clone()));
        ed.sidebar.as_mut().unwrap().focus = true;
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
        run.output = vec![(false, "one".into())];
        ed.task_run = Some(run);
        ed.sidebar.as_mut().unwrap().focus = false;
        draw(&mut ed, 140);
        assert_eq!(ed.screen.bottom_h, 3, "its own: a small one");
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
        let dir = std::env::temp_dir().join(format!("tarae-sizes-drag-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let mut ed = editor("fn main() {}\n");
        ed.sizes_path = Some(dir.join("sizes.json"));
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        let draw = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 140, 40).unwrap();
        };
        let mouse = |ed: &mut Editor, kind, x: u16, y: u16| ed.handle_mouse(Mouse { kind, x, y, alt: false });
        crate::chat::open(&mut ed);
        ed.chat.as_mut().unwrap().focused = false;
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
        ed.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Files, dir.clone()));
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
        run.output = vec![(false, "one".into())];
        ed.task_run = Some(run);
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
        ed.task_run = Some(run);
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
        assert!(!ed.bottom_focus);
        assert!(ed.toasts.iter().any(|t| t.text.starts_with("nothing at the bottom")));
        task_of_40_lines(&mut ed);
        draw(&mut ed);
        let rows = ed.screen.task_panel.get().unwrap().2;
        let end = 40 - rows;
        let top = |ed: &Editor| ed.task_run.as_ref().unwrap().top(rows);
        assert_eq!(top(&ed), end, "its end in sight");
        feed(&mut ed, "<C-w>j");
        assert!(ed.bottom_focus);
        feed(&mut ed, "kkk");
        assert_eq!(top(&ed), end - 3, "scrolled back");
        feed(&mut ed, "xdi");
        assert_eq!(text(&ed), "a\nb\nc\n", "the code never saw them");
        assert_eq!(ed.mode, Mode::Normal);
        assert_eq!(ed.cursor_line(), 0);
        feed(&mut ed, "g");
        assert_eq!(top(&ed), 0);
        feed(&mut ed, "G");
        assert_eq!(ed.task_run.as_ref().unwrap().scroll, None, "at the end: follows it again");
        // Its header says it has the keys, and what they do
        draw(&mut ed);
        let (frame, w, h) = ed.last_frame.clone().unwrap();
        let screen = crate::screen::text(&frame, w, h).unwrap();
        let head = screen.lines().find(|l| l.contains(" OK ")).unwrap();
        assert!(head.starts_with('▎') && head.contains("j k scroll"), "{head}");
        feed(&mut ed, "<esc>j");
        assert!(!ed.bottom_focus);
        assert_eq!(ed.cursor_line(), 1, "the code's again");
        feed(&mut ed, "<C-w>j<C-w>k");
        assert!(!ed.bottom_focus, "C-w k: back up");
        // A click on the panel, then one in the code
        let y = ed.screen.task_panel.get().unwrap().0 as u16 + 2;
        ed.handle_mouse(Mouse { kind: K::Down, x: 20, y, alt: false });
        assert!(ed.bottom_focus);
        let line_1 = ed.screen.text_top as u16 + 1;
        ed.handle_mouse(Mouse { kind: K::Down, x: 20, y: line_1, alt: false });
        assert!(!ed.bottom_focus);
        assert_eq!(ed.cursor_line(), 1);
        // The panel gone: so are its keys
        feed(&mut ed, "<C-w>j");
        ed.task_run = None;
        feed(&mut ed, "j");
        assert!(!ed.bottom_focus);
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
        assert_eq!(ed.zoom, None);
        assert!(ed.toasts.iter().any(|t| t.text == "the code has the whole screen already"));
        // The chat, the side panel, a task, two panes
        crate::chat::open(&mut ed);
        ed.chat.as_mut().unwrap().focused = false;
        ed.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Files, std::env::temp_dir()));
        ed.sidebar.as_mut().unwrap().focus = false;
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
        feed(&mut ed, "k");
        assert_eq!(
            ed.task_run.as_ref().unwrap().scroll,
            Some(40 - ed.screen.task_panel.get().unwrap().2 - 1)
        );
        feed(&mut ed, "<esc>");
        draw(&mut ed);
        assert_eq!((ed.zoom, shown(&ed)), (None, all), "the keys went to the code: it ended");
        // The chat (its empty input lets C-w through)
        ed.chat.as_mut().unwrap().focused = true;
        feed(&mut ed, "<C-w>z");
        draw(&mut ed);
        assert_eq!((ed.screen.chat_w, ed.screen.bottom_h), (140, 0));
        feed(&mut ed, "<esc>");
        draw(&mut ed);
        assert_eq!(shown(&ed), all);
        // The side panel
        ed.sidebar.as_mut().unwrap().focus = true;
        feed(&mut ed, "<C-w>z");
        draw(&mut ed);
        assert_eq!((ed.screen.side_w, ed.screen.chat_w, ed.screen.bottom_h), (140, 0, 0));
        feed(&mut ed, "<C-w>z");
        draw(&mut ed);
        assert_eq!(shown(&ed), all);
        assert!(ed.sidebar.as_ref().unwrap().focus, "back, the keys where they were");
        assert_eq!(ed.sizes, sizes, "nothing kept changed");
    }

    /// `A` in the git tab opens the last message to amend in normal mode (the first keys move, they don't
    /// land on top of the message); `c` still opens a new one ready to type.
    #[test]
    fn amending_opens_the_message_in_normal_mode() {
        let repo = git_repo("amend", &[("a.txt", "one\n")]);
        let mut ed = editor("");
        ed.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone()));
        ed.git_refresh();
        settle_until(&mut ed, |ed| ed.git_status.is_some());
        ed.sidebar.as_mut().unwrap().focus = true;
        feed(&mut ed, "A");
        settle_until(&mut ed, |ed| ed.committing.is_some());
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
        settle_until(&mut ed, |ed| ed.git_status.as_ref().is_some_and(|s| !s.staged.is_empty()));
        ed.sidebar.as_mut().unwrap().focus = true;
        feed(&mut ed, "c");
        settle_until(&mut ed, |ed| ed.committing.is_some());
        assert_eq!(ed.mode, Mode::Insert);
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
        ed.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone()));
        ed.git_refresh();
        settle_until(&mut ed, |ed| ed.git_status.is_some());
        ed.sidebar_sync();
        ed.sidebar.as_mut().unwrap().selected = 1;
        feed(&mut ed, "s");
        settle_until(&mut ed, |ed| ed.git_status.as_ref().is_some_and(|s| !s.staged.is_empty()));
        feed(&mut ed, "C");
        settle_until(&mut ed, |ed| ed.review.as_ref().is_some_and(|r| r.ready));
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
            ed.committing.is_none() && ed.docs.iter().all(|d| d.display_name_short() != "COMMIT_EDITMSG")
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

    /// `space v c`: the review opens and Claude reviews it — the diff goes to the chat, its findings come back
    /// as notes on the lines.
    #[test]
    fn claude_reviews_the_changes() {
        use serde_json::json;
        let repo = git_repo("claudereview", &[("a.txt", "one\n")]);
        std::fs::write(repo.join("a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(repo.join("b.txt"), "brand new\n").unwrap();
        let path = std::fs::canonicalize(repo.join("a.txt")).unwrap();
        let log = repo.join("..").join(format!("claudereview-{}.log", std::process::id()));
        let args = json!({"path": path, "line": 2, "text": "This line is new — is it tested?"});
        let tool = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": "t1", "name": "mcp__tarae__note", "input": args}]}});
        let call = json!({"type": "control_request", "request_id": "c7", "request": {"subtype": "mcp_message",
            "server_name": "tarae", "message": {"jsonrpc": "2.0", "id": 7, "method": "tools/call",
            "params": {"name": "note", "arguments": args}}}});
        let result = json!({"type": "result", "result": "One question."});
        let body: Vec<String> =
            [tool, call, result].iter().map(|e| format!("printf '%s\\n' '{e}'")).collect();
        let mut ed = editor("");
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do printf '%s\\n' \"$line\" >> '{}'; case \"$line\" in '{{\"message\"'*) {};; esac; done",
                log.display(),
                body.join("; ")
            ),
        ];
        // (`ask_claude` in the repository of the test: open it there, then the ask follows)
        ed.review_ask = true;
        crate::codereview::start_in(&mut ed, repo.clone(), None);
        settle_until(&mut ed, |ed| ed.code_review.is_some() && ed.chat.as_ref().is_some_and(|c| c.busy()));
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        let sent = std::fs::read_to_string(&log).unwrap();
        let asked = sent.lines().find(|l| l.contains("Review my changes")).expect("asked");
        assert!(asked.contains(r#"<changes of=\"uncommitted\">"#) && asked.contains("+two"), "{asked}");
        assert!(
            asked.contains("+++ b/b.txt") && asked.contains("+brand new"),
            "untracked files too: {asked}"
        );
        let n = ed.notes.list.last().expect("its finding");
        assert_eq!(
            (n.path.as_path(), n.lines.clone(), n.gist()),
            (path.as_path(), 1..2, "This line is new — is it tested?")
        );
        assert_eq!(n.thread[0].by, crate::notes::By::Claude);
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_file(&log).ok();
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
        ed.code_review = Some(CodeReview::for_test(Base::Head, dir.clone(), files));
        feed(&mut ed, " e");
        let r = ed.code_review.as_ref().unwrap();
        assert!(r.focus && r.selected == 0, "on the file being read");
        // The rail's order: src/ (b, c) then a.rs
        feed(&mut ed, "j");
        assert_eq!(ed.code_review.as_ref().unwrap().selected, 2, "src/c.rs");
        feed(&mut ed, "<ret>");
        assert_eq!(ed.doc().path.as_deref(), Some(dir.join("src/c.rs").as_path()));
        assert!(!ed.code_review.as_ref().unwrap().focus, "the code has the keys");
        feed(&mut ed, " ek");
        assert_eq!(ed.code_review.as_ref().unwrap().selected, 0);
        feed(&mut ed, "m");
        let r = ed.code_review.as_ref().unwrap();
        assert!(r.viewed(&ed, &r.files[0]), "b.rs viewed");
        assert_eq!(r.selected, 2, "on to the next");
        feed(&mut ed, "<esc>");
        assert!(!ed.code_review.as_ref().unwrap().focus);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `space g` is the git panel, lazygit beside the code: from the code it opens on the hunk under the
    /// cursor (its file opened to its hunks) and the next key acts there (`s` stages that hunk alone), then
    /// the code has the keys again. In the panel, moving onto a hunk shows it in the editor and `d` puts
    /// the index's lines back; `z` / `Z` stash and take it back; `?` lists the keys.
    /// `space g` from a changed line: the tab keeps the keys for moving about (`k k k` walks its rows, none
    /// lands in the code — #18: after one, the rest went into the code); only a quick key on the change
    /// under the cursor (`s`) gives them back.
    #[test]
    fn space_g_on_a_change_keeps_the_keys_for_moving() {
        let repo = git_repo("gitmoves", &[("a.txt", "one\ntwo\nthree\nfour\nfive\n"), ("b.txt", "b\n")]);
        std::fs::write(repo.join("b.txt"), "b\nmore\n").unwrap();
        let file = repo.join("a.txt");
        let mut ed = editor("");
        ed.git_auto = true;
        ed.open(&file).unwrap();
        settle_until(&mut ed, |ed| ed.doc().git_base.is_some());
        feed(&mut ed, "4ggmiwcFOUR<esc>:w<ret>");
        ed.flush_saves();
        settle_until(&mut ed, |ed| ed.doc().git_hunks.len() == 1);
        let mut sb = crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone());
        sb.focus = false;
        ed.sidebar = Some(sb);
        feed(&mut ed, " g");
        settle_until(&mut ed, |ed| ed.git_status.is_some() && ed.sidebar.as_ref().unwrap().aim.is_none());
        let text_before = text(&ed);
        let at = |ed: &Editor| ed.sidebar.as_ref().unwrap().selected;
        let start = at(&ed);
        feed(&mut ed, "k");
        assert!(ed.sidebar.as_ref().unwrap().focus, "the tab keeps the keys after one k");
        feed(&mut ed, "kk");
        assert!(ed.sidebar.as_ref().unwrap().focus, "and after three");
        assert!(at(&ed) < start, "k walked up the tab's rows: {} → {}", start, at(&ed));
        assert_eq!(text(&ed), text_before, "no key landed in the code");
        feed(&mut ed, "jjj");
        assert!(ed.sidebar.as_ref().unwrap().focus);
        // A tab key that isn't one of the quick ones (refresh) leaves it a tab — the keys stay
        feed(&mut ed, "<esc>");
        feed(&mut ed, " g");
        settle_until(&mut ed, |ed| ed.sidebar.as_ref().unwrap().aim.is_none());
        feed(&mut ed, "r");
        assert!(ed.sidebar.as_ref().unwrap().focus, "r refreshed it and the tab still has the keys");
        feed(&mut ed, "k");
        assert!(ed.sidebar.as_ref().unwrap().focus);
        assert_eq!(text(&ed), text_before);
        // Pressed before git has answered (the tab still empty): the same
        feed(&mut ed, "<esc>");
        ed.git_status = None;
        feed(&mut ed, " gk");
        assert!(ed.sidebar.as_ref().unwrap().focus, "a k before the rows are in");
        settle_until(&mut ed, |ed| ed.git_status.is_some() && ed.sidebar.as_ref().unwrap().aim.is_none());
        feed(&mut ed, "kk");
        assert!(ed.sidebar.as_ref().unwrap().focus, "and the ones after");
        assert_eq!(text(&ed), text_before, "no key landed in the code");
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn git_panel_hunks_like_lazygit() {
        let repo = git_repo("hub", &[("a.txt", "one\ntwo\nthree\nfour\nfive\n")]);
        let file = repo.join("a.txt");
        let mut ed = editor("");
        ed.git_auto = true;
        ed.open(&file).unwrap();
        settle_until(&mut ed, |ed| ed.doc().git_base.is_some());
        feed(&mut ed, "ggmiwcONE<esc>4gg");
        feed(&mut ed, "miwcFOUR<esc>:w<ret>");
        ed.flush_saves(); // (on disk before git's status reads it)
        settle_until(&mut ed, |ed| ed.doc().git_hunks.len() == 2);
        let mut sb = crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone());
        sb.focus = false;
        ed.sidebar = Some(sb);
        // From the code, the cursor in the second hunk
        feed(&mut ed, " g");
        let selected = |ed: &mut Editor| {
            ed.sidebar_sync();
            let s = ed.sidebar.as_ref().unwrap();
            s.lines().get(s.selected).map(|l| l.target.clone())
        };
        settle_until(&mut ed, |ed| ed.git_status.is_some() && ed.sidebar.as_ref().unwrap().aim.is_none());
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
        assert!(!ed.sidebar.as_ref().unwrap().focus, "the code has the keys again");
        // In the panel: onto the other hunk (the editor goes there), `d` puts it back
        feed(&mut ed, " g");
        settle_until(&mut ed, |ed| ed.sidebar.as_ref().unwrap().aim.is_none());
        let rows: Vec<crate::sidebar::Target> =
            ed.sidebar.as_ref().unwrap().lines().iter().map(|l| l.target.clone()).collect();
        let hunk =
            rows.iter().position(|t| matches!(t, crate::sidebar::Target::Hunk { .. })).expect("a hunk row");
        ed.sidebar.as_mut().unwrap().from_code = false;
        ed.sidebar.as_mut().unwrap().selected = hunk.saturating_sub(1);
        feed(&mut ed, "j");
        assert_eq!(ed.cursor_line(), 0, "the editor on it");
        feed(&mut ed, "d");
        assert_eq!(ed.doc().text.line(0).to_string(), "one\n", "its index lines back");
        // Stash and back
        feed(&mut ed, ":w<ret>");
        ed.flush_saves(); // (on disk before git stashes it)
        ed.sidebar.as_mut().unwrap().focus = true;
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

    /// A conflicted file in the git panel lists its conflicts; `t` on one keeps theirs.
    #[test]
    fn git_panel_settles_a_conflict() {
        let repo = git_repo("hubconflict", &[("m.txt", "a\n")]);
        std::fs::write(repo.join("m.txt"), "a\n<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> b\nz\n").unwrap();
        let mut ed = editor("");
        ed.open(&repo.join("m.txt")).unwrap();
        ed.handle_event(crate::event::Event::Resize);
        let mut sb = crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone());
        sb.focus = false;
        ed.sidebar = Some(sb);
        feed(&mut ed, "gg g");
        settle_until(&mut ed, |ed| ed.git_status.is_some() && ed.sidebar.as_ref().unwrap().aim.is_none());
        ed.sidebar.as_mut().unwrap().from_code = false;
        ed.sidebar_sync();
        let rows: Vec<crate::sidebar::Target> =
            ed.sidebar.as_ref().unwrap().lines().iter().map(|l| l.target.clone()).collect();
        let at = rows
            .iter()
            .position(|t| matches!(t, crate::sidebar::Target::Conflict { .. }))
            .expect("its conflict");
        ed.sidebar.as_mut().unwrap().selected = at;
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
        ed.git_auto = true;
        ed.open(&repo.join("a.txt")).unwrap();
        settle_until(&mut ed, |ed| ed.doc().git_base.is_some());
        let mut sb = crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone());
        sb.focus = false;
        ed.sidebar = Some(sb);
        feed(&mut ed, "<space>g");
        settle_until(&mut ed, |ed| ed.git_status.as_ref().is_some_and(|s| !s.unstaged.is_empty()));
        feed(&mut ed, "s");
        settle_until(&mut ed, |ed| ed.git_status.as_ref().is_some_and(|s| !s.staged.is_empty()));
        assert_eq!(ed.git_status.as_ref().unwrap().staged[0].rel, "b.txt");
        assert!(ed.sidebar.as_ref().unwrap().focus, "the tab keeps the keys");
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
        ed.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone()));
        ed.git_refresh();
        settle_until(&mut ed, |ed| ed.git_status.is_some());
        ed.sidebar_sync();
        ed.sidebar.as_mut().unwrap().selected = 0; // the heading
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
            let mut sb = crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, repo.clone());
            sb.focus = false;
            ed.sidebar = Some(sb);
            ed
        };
        let mut ed = in_repo(SRC);
        feed(&mut ed, "]m");
        assert_eq!(ed.cursor_line(), 1);
        feed(&mut ed, " gt");
        assert_eq!(text(&ed), "a\ntheirs\nz\n<<<<<<< HEAD\n1\n=======\n2\n>>>>>>> b\n");
        assert_eq!(ed.doc().conflicts.len(), 1);
        feed(&mut ed, "u");
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
        settle_until(&mut ed, |ed| ed.code_review.is_some() && ready(ed));
        // The step over: its toast goes — what came of it shows on its own (U9)
        assert_eq!(said(&ed), ["review: 3 files changed — ]g walks the changes"]);
        let r = ed.code_review.as_ref().unwrap();
        let files: Vec<_> =
            r.files.iter().map(|f| (f.rel.as_str(), f.added, f.removed, f.new, f.gone)).collect();
        assert_eq!(
            files,
            [("a.rs", 1, 2, false, false), ("b.rs", 0, 2, false, true), ("c.rs", 1, 0, true, false)]
        );
        assert_eq!(ed.doc().path.as_deref(), Some(dir.join("a.rs").as_path()));
        assert_eq!(ed.cursor_line(), 1, "on the first change");
        // ]g: the deletion, then on into the deleted file, the new one, and [g back
        feed(&mut ed, "]g");
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
        assert!(ed.chat.is_none(), "a draft doesn't go anywhere yet");
        let n = &ed.notes.list[0];
        assert!(n.thread[0].draft && !n.waiting);
        assert_eq!(n.lines, 1..2);
        feed(&mut ed, "<space>vm");
        let r = ed.code_review.as_ref().unwrap();
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
        assert!(!ed.notes.list[0].thread[0].draft && ed.notes.list[0].waiting);
        settle_until(&mut ed, |ed| !ed.chat.as_ref().unwrap().busy());
        assert!(!ed.notes.list[0].waiting);
        let sent = std::fs::read_to_string(&log).unwrap();
        for want in [
            "<review of=\\\"uncommitted\\\" comments=\\\"1\\\">",
            "-let x = 1;",
            "+let x = 10;",
            "you: why 10?",
        ] {
            assert!(sent.contains(want), "{want} in {sent}");
        }
        // Ended: back to the index as the diff base
        feed(&mut ed, "<space>vq");
        assert!(ed.code_review.is_none());
        assert!(ed.docs.iter().all(|d| d.review_base.is_none()));
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
        settle_until(&mut ed, |ed| ed.code_review.is_some());
        let r = ed.code_review.as_ref().unwrap();
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
        assert!(ed.code_review.is_none());
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
        ed.code_review = Some(r);
        let ev = ed.events.recv_timeout(std::time::Duration::from_secs(5)).expect("the index base");
        ed.handle_event(ev);
        assert!(ed.doc().git_base.is_none(), "the review's base is on its way");
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
        ed.code_review = Some(crate::codereview::CodeReview::for_test(
            crate::codereview::Base::Head,
            dir.clone(),
            vec![file],
        ));
        crate::codereview::show(&mut ed, 0, false); // the base is asked for over the placeholder
        ed.finish_loading(id, Ok(ropey::Rope::from_str("a\nB\nc\n")), 0);
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
        ed.config.llm.command = "/nonexistent/claude".into();
        feed(&mut ed, "<space>nhello<ret>");
        assert!(ed.notes.list.is_empty(), "the new note went away");
        crate::notes::add(&mut ed, &main, 0..1, crate::notes::By::Claude, "hi");
        feed(&mut ed, "<space>nreply<ret>");
        let n = &ed.notes.list[0];
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
        assert_eq!(ed.notes.list[0].lines, 6..8);
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
        assert_eq!(ed.notes.list.len(), 1, "only the one here");
        feed(&mut ed, ":notes-clear<ret>");
        assert!(ed.notes.list.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn splits_focus_scroll_and_close() {
        let mut ed = editor(&"x\n".repeat(100));
        ed.handle_event(crate::event::Event::Resize); // first sync
        feed(&mut ed, "<C-w>v");
        assert_eq!(ed.views.len(), 2);
        assert_eq!(ed.focus, 2, "focus on the new pane");
        // Scroll down in the new pane → the old pane stays put
        ed.doc_mut().top = 50;
        ed.handle_event(crate::event::Event::Resize);
        feed(&mut ed, "<C-w>w");
        assert_eq!(ed.focus, 1);
        assert_eq!(ed.doc().top, 0, "scroll is per pane");
        feed(&mut ed, "<C-w>w");
        assert_eq!(ed.doc().top, 50);
        // :q closes only the pane (quits only on the last pane)
        feed(&mut ed, ":q<ret>");
        assert_eq!(ed.views.len(), 1);
        assert!(!ed.should_quit);
        feed(&mut ed, "<C-w>s<C-w>o");
        assert_eq!(ed.views.len(), 1, "only this pane");
    }

    /// `C-w e` takes the keys to the side panel and back from wherever they are; a closed panel opens at the
    /// tab it last showed. `C-w h/j/k/l` are the code's windows' — in the panel they leave it be.
    #[test]
    fn c_w_e_to_the_side_panel_and_back() {
        use crate::sidebar::Mode;
        let mut ed = editor("x\n");
        feed(&mut ed, "<C-w>v");
        let focus = |ed: &Editor| ed.sidebar.as_ref().map(|s| (s.mode, s.focus));
        feed(&mut ed, "<C-w>e");
        assert_eq!(focus(&ed), Some((Mode::Files, true)), "closed: opens, at files the first time");
        feed(&mut ed, "<C-w>e");
        assert_eq!(focus(&ed), Some((Mode::Files, false)), "back to the code");
        assert_eq!(ed.focus, 2, "the window it came from");
        feed(&mut ed, " o");
        feed(&mut ed, "<C-w>h");
        assert_eq!(focus(&ed), Some((Mode::Outline, true)), "in the panel: a window move doesn't leave it");
        feed(&mut ed, "q");
        assert!(ed.sidebar.is_none());
        feed(&mut ed, "<C-w>e");
        assert_eq!(focus(&ed), Some((Mode::Outline, true)), "reopens at the tab it last showed");
    }

    #[test]
    fn tutor_is_a_throwaway_buffer() {
        let mut ed = editor("");
        feed(&mut ed, ":tutor<ret>");
        assert_eq!(ed.doc().display_name(), "tutor");
        assert!(ed.doc().text.to_string().starts_with("# tarae tutorial"));
        feed(&mut ed, "ihello<esc>:q<ret>");
        assert!(ed.should_quit, "an edited tutorial doesn't block quitting");
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
        // Instead of saving and reopening: record the position in session state; a new editor reads it
        let head = ed.doc().selection().primary().head;
        let key = std::fs::canonicalize(&path).unwrap();
        ed.session = crate::session::State::from_json(
            serde_json::json!({ "positions": { key.to_str().unwrap(): [head, 0] } }),
        );
        let session = std::mem::take(&mut ed.session);
        let mut ed2 = editor("");
        ed2.session = session;
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
        feed(&mut ed, "]g");
        assert_eq!(ed.doc().selection().primary(), Range::new(2, 4), "edited line selected");
        feed(&mut ed, "]g");
        assert_eq!(ed.doc().selection().primary(), Range::new(8, 12), "added line");
        feed(&mut ed, "[g");
        assert_eq!(ed.doc().selection().primary(), Range::new(2, 4));
    }

    #[test]
    fn grammar_offer_asks_once_and_answers_only_in_normal_mode() {
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
        // n in normal mode = not now; the same language isn't asked again
        feed(&mut ed, "n");
        assert!(ed.offers.is_empty());
        ed.offer_grammar(rust);
        assert!(ed.offers.is_empty(), "once per session");
        // A different language is asked — "Not now" via the mouse
        ed.offer_grammar(syntax::spec("python").unwrap());
        assert_eq!(ed.offers.first().map(|o| o.title.as_str()), Some("Python syntax colors"));
        ed.screen.offer_buttons = vec![(20, 50, 59, 'y'), (20, 62, 71, 'n')];
        ed.handle_mouse(crate::event::Mouse {
            kind: crate::event::MouseKind::Down,
            x: 65,
            y: 20,
            alt: false,
        });
        assert!(ed.offers.is_empty());
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
            ed.offer_seen.set(Some((o.0, o.1, std::time::Instant::now() - crate::offer::SETTLE)));
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
                What::ProjectTrust { hash: 0 },
            )
        };
        ed.push_offer(card(&question));
        let s = screen(&mut ed);
        assert!(s.contains("Trust this project's settings?") && s.contains("line 1 of what it runs"), "{s}");
        assert!(!s.contains("line 80 of what it runs"), "cut to fit");
        assert!(s.contains("more lines · v view all") && s.contains("y Trust"), "{s}");
        assert!(ed.screen.offer_buttons.iter().any(|b| b.3 == 'y'), "y in sight");
        settled(&mut ed);
        // `y` before all of it was seen: not an answer
        feed(&mut ed, "y");
        assert_eq!(ed.offers.len(), 1, "y waits");
        assert!(ed.toasts.last().is_some_and(|t| t.text.contains("see all of it first")));
        // `v`: all of it in a buffer — opened isn't read: `y` still waits until its end has been on screen
        feed(&mut ed, "v");
        assert!(text(&ed).contains("line 80 of what it runs") && text(&ed).starts_with("Trust this project"));
        assert_eq!(ed.mode, Mode::Normal, "v didn't start a selection");
        let s = screen(&mut ed);
        assert!(!s.contains("line 80 of what it runs"), "its end is below the screen");
        // While its text is read, the card is a bar over the status line (U8) — nothing of it over the text
        let rows: Vec<&str> = s.lines().collect();
        let bar = rows[30 - 3];
        assert!(
            bar.contains("Trust this project's settings?")
                && bar.contains("G to its end, then y answers")
                && bar.contains("y Trust"),
            "the bar: {bar:?}\n{s}"
        );
        assert!(!s.contains("more lines"), "the card stepped aside: {s}");
        assert!(!ed.offer_seen_whole(), "its bar in sight isn't the card read");
        assert!(ed.screen.offer_buttons.iter().any(|b| b.3 == 'y' && b.0 == 27), "its y still clicks");
        // Another buffer: the card whole again; back: the bar
        let viewed = ed.doc().id;
        ed.new_scratch();
        let s = screen(&mut ed);
        assert!(s.contains("more lines · v view all · G to the end"), "whole again: {s}");
        ed.current = ed.docs.iter().position(|d| d.id == viewed).unwrap();
        assert!(!screen(&mut ed).contains("more lines"), "the bar again");
        feed(&mut ed, "y");
        assert_eq!(ed.offers.len(), 1, "y waits while the end is unseen");
        assert!(ed.toasts.last().is_some_and(|t| t.text.contains("read it to its end")));
        // Scrolled to the end: seen — the card says so in that same frame, and `y` answers
        feed(&mut ed, "G");
        let s = screen(&mut ed);
        assert!(s.contains("line 80 of what it runs"), "{s}");
        assert!(s.contains("read to its end — y answers"), "the bar says y counts now: {s}");
        feed(&mut ed, "y");
        assert!(ed.offers.is_empty(), "answered once seen whole");
        // New text on the card: seen again first (a click on y waits as well)
        ed.push_offer(card(&question));
        screen(&mut ed);
        settled(&mut ed);
        ed.push_offer(card(&format!("{question}\nline 81, new")));
        screen(&mut ed);
        settled(&mut ed);
        let click = |ed: &mut Editor, k: char| {
            let (row, x0, _, _) = *ed.screen.offer_buttons.iter().find(|b| b.3 == k).unwrap();
            let (x, y) = (x0 as u16, row as u16);
            ed.handle_mouse(crate::event::Mouse { kind: crate::event::MouseKind::Down, x, y, alt: false });
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
            What::ProjectTrust { hash: 0 },
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
                What::ProjectTrust { hash: 1 },
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
        let dir = std::env::temp_dir().join(format!("tarae-chat-screen-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("stdin.log");
        let mut ed = editor("fn main() {}\n");
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
        let shot = ed.chat.as_ref().unwrap().screen.clone().expect("attached");
        assert_eq!((shot.cols, shot.rows), (100, 30));
        assert!(shot.text.contains("fn main() {}") && shot.text.contains("claude"), "{}", shot.text);
        let s = screen(&mut ed);
        assert!(s.contains("screen 100×30 goes with it") && s.contains("backspace no screen"), "{s}");
        // Backspace in the empty input takes it off; C-s again puts it back
        feed(&mut ed, "<backspace>");
        assert!(ed.chat.as_ref().unwrap().screen.is_none());
        assert!(screen(&mut ed).contains("C-s screen"));
        feed(&mut ed, "<C-s>what is wrong here<ret>");
        let c = ed.chat.as_ref().unwrap();
        assert!(c.screen.is_none(), "it went");
        let m = c.msgs.iter().rev().find(|m| m.role == crate::chat::Role::User).unwrap();
        assert!(m.chip.ends_with(" · screen 100×30"), "{}", m.chip);
        let copy = m.attached.clone().expect("a copy").path;
        assert!(c.links().iter().any(|l| l.path == copy), "C-] reaches it");
        // On its chip row it looks like the link it is: `screen 100×30` underlined, the rest of the chip not
        // (the panel back while Claude works, as `space l` brings it)
        ed.chat.as_mut().unwrap().unfolded = true;
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
        let c = ed.chat.as_ref().expect("opened");
        assert!(c.focused && c.screen.as_ref().is_some_and(|s| s.text.contains("fn other() {}")));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A card never takes what's typed in the chat (B1: the project-trust card took every `n` of "guard_and_go"
    /// and was dismissed): while the input has the keys they're the message's — a card that comes then waits.
    /// One that comes while nothing is being typed (Claude asking mid-turn) takes the keys, as a review does.
    #[test]
    fn a_card_doesnt_take_what_is_typed_in_the_chat() {
        use crate::offer::{Offer, What};
        let mut ed = editor("text\n");
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        let settle = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 120, 30).unwrap();
            let o = ed.offers.first().map(|o| (o.id, o.rev)).unwrap();
            ed.offer_seen.set(Some((o.0, o.1, std::time::Instant::now() - crate::offer::SETTLE)));
        };
        let card = |title: &str| {
            Offer::new(title.into(), "x = 1", "", String::new(), What::ProjectTrust { hash: 0 })
        };
        // A card up, in sight and settled — then the chat, and a message with `n`s in it
        ed.push_offer(card("Trust this project's settings?"));
        settle(&mut ed);
        feed(&mut ed, "<space>l");
        assert!(ed.chat.as_ref().unwrap().focused);
        feed(&mut ed, "guard_and_go no");
        assert_eq!(ed.chat.as_ref().unwrap().input, "guard_and_go no", "every key went to the message");
        assert_eq!(ed.offers.len(), 1, "the card is still there, unanswered");
        // A card that comes mid-message waits; the keys stay the message's
        ed.push_offer(card("Second"));
        assert!(ed.chat.as_ref().unwrap().focused);
        feed(&mut ed, "n");
        assert_eq!(ed.chat.as_ref().unwrap().input, "guard_and_go non");
        // Nothing being typed: a card that comes takes the keys
        ed.chat.as_mut().unwrap().input.clear();
        ed.offers.clear();
        ed.push_offer(card("Claude wants to run"));
        assert!(!ed.chat.as_ref().unwrap().focused, "the card has the keys");
        settle(&mut ed);
        feed(&mut ed, "y");
        assert!(ed.offers.is_empty(), "y answered it");
        // An answer letter the card doesn't take isn't the code's (`a` would start inserting, and the `y` after
        // it would be typed)
        ed.push_offer(card("Only y or n"));
        settle(&mut ed);
        feed(&mut ed, "aA");
        assert_eq!((ed.mode, ed.offers.len()), (Mode::Normal, 1), "a/A went nowhere");
        feed(&mut ed, "y");
        assert!(ed.offers.is_empty());
        assert_eq!(text(&ed), "text\n");
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
        let at = |kind, x, y| Mouse { kind, x, y, alt: false };
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

    /// An accepted `space i` answer (like Claude Code's diffs) is a step of its own for `space u` / `space U`.
    #[test]
    fn an_ask_is_a_step_space_u_takes_back() {
        let (dir, main, ..) = follow_project("ask-turn");
        let mut ed = editor("");
        ed.open(&main).unwrap();
        fake_claude(&mut ed, "<r i=\"1\">LINE</r>", false);
        feed(&mut ed, "e iupper<ret>");
        // (a file's own jobs — its parse — may come first)
        settle_until(&mut ed, |ed| ed.review.as_ref().is_some_and(|r| r.ready));
        feed(&mut ed, "y");
        assert_eq!(text(&ed), "LINE 1\nline 2\nline 3\n");
        assert_eq!(ed.checkpoints.list.last().map(|c| c.question.as_str()), Some("upper"));
        feed(&mut ed, "<space>u");
        assert_eq!(text(&ed), "line 1\nline 2\nline 3\n");
        feed(&mut ed, "<space>U");
        assert_eq!(text(&ed), "LINE 1\nline 2\nline 3\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ask_review_accept_then_undo() {
        let mut ed = editor("foo bar");
        fake_claude(&mut ed, "<r i=\"1\">FOO</r>", false);
        feed(&mut ed, "e i");
        assert_eq!(ed.prompt.as_ref().map(|p| p.text.as_str()), Some("ask "));
        feed(&mut ed, "upper<ret>");
        assert!(ed.review.as_ref().is_some_and(|r| !r.ready), "editing isn't blocked while waiting");
        settle(&mut ed);
        assert!(ed.review.as_ref().is_some_and(|r| r.ready));
        feed(&mut ed, "y");
        assert_eq!(text(&ed), "FOO bar");
        assert!(ed.review.is_none());
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

    #[test]
    fn ask_error_is_reported() {
        let mut ed = editor("foo");
        fake_claude(&mut ed, "Not logged in", true);
        feed(&mut ed, ":ask x<ret>");
        settle(&mut ed);
        assert!(ed.review.is_none());
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
    /// starts a new card. `:messages` keeps them all, newest first.
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
        let p = ed.picker.as_ref().expect("the messages list");
        let labels: Vec<&str> = p.items().iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels[0], "three", "newest first");
        assert_eq!(labels.len(), 5);
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

    #[test]
    fn file_picker_and_global_search_open_files() {
        // On this repo itself: the file list fills, and a search result jumps to its line.
        let mut ed = editor("");
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
        // (the live search looks in the project — this repo here)
        ed.project_root = std::fs::canonicalize(cwd()).ok();
        feed(&mut ed, " /fn put_cursor");
        settle_for(&mut ed, |ed| ed.picker.as_ref().is_some_and(|p| !p.searching && p.current().is_some()));
        feed(&mut ed, "<ret>");
        let doc = ed.doc();
        let pos = doc.selection().primary().head;
        let line = doc.text.line(mv::line_of(&doc.text, pos)).to_string();
        assert!(line.contains("fn put_cursor"), "{line:?}");
    }

    /// #36: editing a big Rust file keeps memory bounded — undo keeps no parse trees (every step kept its
    /// own: injected layers share nothing between versions, and a reparse rebuilds the lists an edit sits in,
    /// a third of this file's nodes), so the document holds one version's trees however long it's edited.
    /// An undo or redo edits the current trees instead — colored at once, and after edits, undos and redos
    /// landing while a parse runs (forced, not timed), the tree is exactly what a fresh parse gives.
    #[test]
    fn a_long_edit_session_keeps_its_trees_bounded() {
        let dir = std::env::temp_dir().join(format!("tarae-tree-memory-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.rs");
        let text: String = (0..400)
            .map(|i| format!("/// Item {i} — does `thing_{i}` for [`Other`].\n// note {i}\nfn f{i}(x: u32) -> u32 {{ x + {i} }}\n"))
            .collect();
        std::fs::write(&file, &text).unwrap();
        let mut ed = editor("");
        ed.open(&file).unwrap();
        let parsed = |ed: &Editor| {
            ed.doc().syntax.as_ref().is_some_and(|s| !s.dirty && !s.in_flight && s.tree.is_some())
        };
        settle_for(&mut ed, parsed);
        let Some(syn) = ed.doc().syntax.as_ref() else { return }; // (no rust grammar in this build)
        assert!(!syn.layers.is_empty(), "doc comments are injected layers");
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
    /// shown again is parsed again. One opened but never shown (a restored session) never gets any.
    #[test]
    fn trees_go_from_files_not_looked_at_for_a_while() {
        let dir = std::env::temp_dir().join(format!("tarae-trees-sight-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
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
        let dir = std::env::temp_dir().join(format!("tarae-live-search-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn alpha() {}\nlet Beta = alpha();\n").unwrap();
        std::fs::write(dir.join("b.txt"), "ALPHA beta\n").unwrap();
        let mut ed = editor("");
        ed.project_root = Some(dir.clone());
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
        let dir = std::env::temp_dir().join(format!("tarae-picker-click-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
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
            ed.handle_mouse(Mouse { kind: K::Down, x: 10, y: (top + row) as u16, alt: false });
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
        feed(&mut ed, ":vs<ret>:n<ret>");
        ed.handle_event(crate::event::Event::Resize); // focused pane shows the new scratch
        feed(&mut ed, "iB");
        ed.focus_view(1);
        feed(&mut ed, "x<esc>u");
        assert_eq!(text(&ed), "aaa", "x undone in its own document");
        ed.focus_view(2);
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
        assert!(ed.views.iter().all(|v| v.doc == id), "both panes show a.txt");
        // A big file that fails to read
        let gone = ed.alloc_id();
        ed.docs.push(Document::placeholder(gone, &dir.join("big.txt")));
        ed.current = 1;
        ed.handle_event(crate::event::Event::Resize);
        ed.finish_loading(gone, Err(anyhow::anyhow!("boom")), 0);
        assert!(ed.docs.iter().all(|d| d.id != gone));
        assert!(ed.views.iter().all(|v| v.doc == id), "panes moved back to a.txt");
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
        settle_until(&mut ed, |ed| ed.qlist.is_some()); // real paths on a worker
        assert_eq!(ed.qlist.as_ref().map(|l| l.items.len()), Some(3));
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
        let s = ed.sidebar.as_ref().expect("the list in the side panel");
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
    /// cursor while inserting, before the selection in normal mode; into a picker's query on one line.
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
        assert_eq!(ed.doc().text.to_string(), "xfn main() {\n}\n", "normal mode: before the selection");
        feed(&mut ed, "u");
        assert_eq!(ed.doc().text.to_string(), "fn main() {\n}\n");
        feed(&mut ed, " b");
        ed.handle_event(crate::event::Event::Paste("two\nwords".into()));
        assert_eq!(ed.picker.as_ref().map(|p| p.query.as_str()), Some("two words"));
        assert_eq!(ed.doc().text.to_string(), "fn main() {\n}\n", "the buffer untouched");
        // Select mode: at the selection's cursor (its last character, not past it), then normal mode
        feed(&mut ed, "<esc>ggvl");
        ed.handle_event(crate::event::Event::Paste("_".into()));
        assert_eq!(ed.doc().text.to_string(), "f_n main() {\n}\n");
        assert_eq!(ed.mode, Mode::Normal);
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
        settle_until(&mut ed, |ed| ed.task_run.as_ref().is_some_and(|r| r.took.is_some()));
        let r = ed.task_run.as_ref().unwrap();
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
        ed.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Files, dir.clone()));
        ed.sidebar_list_files();
        settle_until(&mut ed, |ed| ed.sidebar.as_ref().is_some_and(|s| !s.loading));
        let rows = |ed: &Editor| -> Vec<String> {
            let s = ed.sidebar.as_ref().unwrap();
            s.lines().iter().map(|l| format!("{}{}", "  ".repeat(l.depth), l.label)).collect()
        };
        let sel = |ed: &Editor| ed.sidebar.as_ref().map(|s| s.selected);
        assert_eq!(rows(&ed), ["src", "  ui", "    card.rs", "  a.rs", "b.md"]);
        assert_eq!(sel(&ed), Some(2), "on the file being edited");
        feed(&mut ed, "j<ret>");
        assert_eq!(ed.doc().display_name_short(), "a.rs");
        assert!(!ed.sidebar.as_ref().unwrap().focus, "the code has the keys again");
        feed(&mut ed, " e");
        assert!(ed.sidebar.as_ref().unwrap().focus);
        feed(&mut ed, "kkh");
        assert_eq!(rows(&ed), ["src", "  ui", "  a.rs", "b.md"], "ui folded");
        feed(&mut ed, "l");
        assert_eq!(rows(&ed).len(), 5);
        feed(&mut ed, "<esc>");
        assert!(ed.sidebar.as_ref().is_some_and(|s| !s.focus), "still showing");
        feed(&mut ed, " e e");
        assert!(ed.sidebar.is_none(), "space e on the focused panel closes it");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The outline (tree-sitter, worked out on a worker): unfocused it follows the cursor; Enter on a
    /// definition jumps there. Only with the Rust grammar.
    #[test]
    fn side_panel_outline() {
        let Some(mut ed) = parsed_rust("fn a() {}\nstruct S;\nimpl S {\n    fn b() {}\n}\n") else { return };
        feed(&mut ed, " o");
        settle_until(&mut ed, |ed| ed.sidebar.as_ref().is_some_and(|s| !s.defs.is_empty()));
        let s = ed.sidebar.as_ref().unwrap();
        let rows: Vec<String> =
            s.lines().iter().map(|l| format!("{}{}", "  ".repeat(l.depth), l.label)).collect();
        assert_eq!(rows, ["a", "S", "impl S", "  b"]);
        feed(&mut ed, "<esc>3gg");
        ed.sidebar_sync(); // what every event ends with
        assert_eq!(ed.sidebar.as_ref().map(|s| s.selected), Some(2), "follows the cursor: impl S");
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
        assert!(ed.views[0].jumps.iter().all(|j| j.doc == ed.doc().id), "b's jumps went with it");
        feed(&mut ed, "ga<C-o>");
        assert_eq!(name(&ed), "a.txt");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The start screen offers this folder's last session (the files still there) as its first row; `r`
    /// brings it back. Without one, `r` on the start screen does nothing.
    #[test]
    fn the_start_screen_continues_the_last_session() {
        let (dir, main, util, _) = follow_project("last-session");
        let gone = dir.join("gone.rs");
        let cwd = std::env::current_dir().unwrap();
        let mut ed = editor("");
        feed(&mut ed, "r");
        assert!(ed.welcome(), "nothing to continue: r does nothing");
        ed.session = crate::session::State::from_json(serde_json::json!({ "sessions": {
            cwd.to_str().unwrap(): { "files": [main, gone, util], "current": 2 } } }));
        ed.find_last_session();
        assert_eq!(
            ed.last_session.as_deref(),
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
        ed.session = crate::session::State::from_json(
            serde_json::json!({ "positions": { path.to_str().unwrap(): [4, 1] } }),
        );
        let id = ed.alloc_id();
        ed.docs.push(Document::placeholder(id, &path));
        ed.current = 1;
        ed.finish_loading(id, Ok(ropey::Rope::from_str("one\ntwo\nthree\n")), 0);
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
