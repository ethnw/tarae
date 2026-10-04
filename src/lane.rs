//! A lane: one checkout's work — its panes, chat, terminal, tests, tasks, git and review. The editor holds
//! the one in front as `Editor.lane`; what every lane shares (documents, servers, config, toasts, the
//! generation counters) stays on `Editor`.

use std::path::PathBuf;

use crate::editor::View;

pub struct Lane {
    /// The checkout, links resolved: where git, the terminal, tasks, Claude, pickers and searches run, what
    /// paths are shown relative to, what a relative path means (follow mode goes nowhere else). Read once,
    /// in `Editor::with_root`; nothing reads the process's folder after that. Tests with a project of their
    /// own in a temp folder set it.
    pub root: PathBuf,
    /// Panes, the split tree and the focused pane. The focused pane's scroll is its document's top/left
    /// (synced every event).
    pub views: Vec<View>,
    pub split: crate::split::Split,
    pub focus: crate::split::ViewId,
    /// Chat pane on the right (if open).
    pub chat: Option<crate::chat::Chat>,
    /// Claude's notes pinned to code (`notes.rs`).
    pub notes: crate::notes::Notes,
    /// What Claude changed per chat turn — undone as one (`checkpoint.rs`).
    pub checkpoints: crate::checkpoint::Checkpoints,
    /// The code review in progress (`codereview.rs`).
    pub code_review: Option<crate::codereview::CodeReview>,
    /// The review being opened is for Claude to review (`space v c`) — asked once it's open.
    pub review_ask: bool,
    /// The side panel — files or outline (`sidebar.rs`).
    pub sidebar: Option<crate::sidebar::Sidebar>,
    /// The tab it last showed (`C-w e` opens it again).
    pub side_last: crate::sidebar::Mode,
    /// The built-in terminal (terminal.rs).
    pub terminal: Option<crate::terminal::Terminal>,
    /// A terminal is being started (on a worker) · what it printed and its exit code, if they came before
    /// it was in place.
    pub terminal_starting: bool,
    pub terminal_early: (Vec<u8>, Option<i32>),
    /// Test pane (running or last result) · what ran last (was it debug) — testing.rs.
    pub test_run: Option<crate::testing::TestRun>,
    pub test_last: Option<(crate::testing::Target, bool)>,
    /// The build task running or last run (task.rs) · the last one started (`space m m`).
    pub task_run: Option<crate::task::Run>,
    pub task_last: Option<crate::task::Task>,
    /// The list `]q` goes through (quickfix.rs).
    pub qlist: Option<crate::quickfix::List>,
    /// Git: the repository's status (the side panel's git tab) · a discard waiting for its second `d` · a
    /// commit message being written (gitpanel.rs).
    pub git_status: Option<crate::gitpanel::Status>,
    pub git_discard_asked: Option<(String, std::time::Instant)>,
    pub committing: Option<crate::gitpanel::Committing>,
    /// Git work waiting its turn (one at a time) · one running · reads of the status asked for (the latest
    /// lands).
    pub git_queue: std::collections::VecDeque<crate::gitpanel::GitJob>,
    pub git_busy: bool,
    pub git_reads: u64,
    /// Current git branch (filled by the watcher thread — for the status line).
    pub git_branch: Option<String>,
    /// `space g f`: per file, its path, first changed line and diff against HEAD.
    pub(crate) changed_files: Vec<crate::gitmenu::Changed>,
    /// The run whose denials `space m s` listed, and what could open (`blocked.rs`).
    pub blocked: Option<(crate::blocked::BlockedRun, Vec<crate::blocked::Choice>)>,
    /// `:preflight` under way, or the last one while its run is in the task panel (`preflight.rs`).
    pub preflight: Option<crate::preflight::Preflight>,
    /// The files of this folder's last session that are still there — the start screen's `r`.
    pub last_session: Option<Vec<PathBuf>>,
}

impl Lane {
    /// A lane for the checkout in `root` (a real path) — nothing open in it yet, its first pane `view`.
    pub fn new(root: PathBuf, view: crate::split::ViewId) -> Self {
        Self {
            root,
            views: Vec::new(),
            split: crate::split::Split::Leaf(view),
            focus: view,
            chat: None,
            notes: Default::default(),
            checkpoints: crate::checkpoint::Checkpoints {
                dir: if cfg!(test) { None } else { crate::checkpoint::default_dir() },
                ..Default::default()
            },
            code_review: None,
            review_ask: false,
            sidebar: None,
            side_last: crate::sidebar::Mode::Files,
            terminal: None,
            terminal_starting: false,
            terminal_early: Default::default(),
            test_run: None,
            test_last: None,
            task_run: None,
            task_last: None,
            qlist: None,
            git_status: None,
            git_discard_asked: None,
            committing: None,
            git_queue: Default::default(),
            git_busy: false,
            git_reads: 0,
            git_branch: None,
            changed_files: Vec::new(),
            blocked: None,
            preflight: None,
            last_session: None,
        }
    }
}
