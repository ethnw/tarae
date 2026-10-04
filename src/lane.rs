//! Lanes: one checkout's work each — its panes, chat, terminal, tests, tasks, git and review. The editor
//! holds the one in front as `Editor.lane` and the others in `Editor.lanes`; a switch is one swap, so no
//! field is forgotten. What every lane shares (documents, servers, config, toasts, the generation
//! counters) stays on `Editor`.
//!
//! - A lane behind goes on working: what it started (its Claude, terminal, tests, tasks, git) sends its
//!   results tagged with it (`event::Tx`), and `with_lane` swaps it in to apply them, then back.
//! - What would take the screen waits in the lane: a card (`push_offer` parks it), an edit review (its
//!   queue isn't taken up — `edits::next`), follow mode's moves (its trail is kept). Claude waiting on one
//!   is said once, named. A tag for a lane that's gone runs in front — the generation checks drop it.

use std::path::{Path, PathBuf};

use crate::document::{DocId, Document};
use crate::editor::{Editor, Mode, View};
use crate::event::LaneId;
use crate::key::{Code, Key};

pub struct Lane {
    pub id: LaneId,
    /// The checkout, links resolved: where git, the terminal, tasks, Claude, pickers and searches run, what
    /// paths are shown relative to, what a relative path means (follow mode goes nowhere else). Read once,
    /// in `Editor::with_root`; nothing reads the process's folder after that. Tests with a project of their
    /// own in a temp folder set it.
    pub root: PathBuf,
    /// What it's called (toasts from it start with it): a session's name, else its checkout's folder name.
    pub name: String,
    /// A session's branch, as it was made (`sessions.rs`) · what it was made from — a branch, or a commit
    /// when that checkout was detached; what `:session-end` and `:session-bring` compare with (None:
    /// the branch of the checkout tarae started in).
    pub branch: Option<String>,
    pub base: Option<String>,
    /// Panes, the split tree and the focused pane. The focused pane's scroll is its document's top/left
    /// (synced every event).
    pub views: Vec<View>,
    pub split: crate::split::Split,
    pub focus: crate::split::ViewId,
    /// While it's behind: the document its focused pane shows (in front, that's `Editor.current`).
    pub current_doc: DocId,
    /// The blank scratch it opened with, until a document is added in it — the first file opened takes
    /// its place, as at startup.
    pub scratch: Option<DocId>,
    /// Chat pane on the right (if open).
    pub chat: Option<crate::chat::Chat>,
    /// Claude's notes pinned to code (`notes.rs`).
    pub notes: crate::notes::Notes,
    /// What Claude changed per chat turn — undone as one (`checkpoint.rs`).
    pub checkpoints: crate::checkpoint::Checkpoints,
    /// The in-buffer review (`llm.rs`) — `space i`'s, a chat edit's, Claude Code's.
    pub review: Option<crate::llm::Review>,
    /// The code review in progress (`codereview.rs`).
    pub code_review: Option<crate::codereview::CodeReview>,
    /// The review being opened is for Claude to review (`space v c`) — asked once it's open.
    pub review_ask: bool,
    /// The side panel — files or outline (`sidebar.rs`).
    pub sidebar: Option<crate::sidebar::Sidebar>,
    /// The tab it last showed (`C-w e` opens it again).
    pub side_last: crate::sidebar::Mode,
    /// The built-in terminal (terminal.rs) · the run that's this lane's latest (a replaced one's late
    /// output is dropped).
    pub terminal: Option<crate::terminal::Terminal>,
    pub terminal_run: u64,
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
    /// The commit message being opened is for ending the session after it (`sessions.rs`).
    pub commit_then_end: bool,
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
    /// Cards that came while it was behind, and Claude's that were up when it left — up again when it's in
    /// front · Claude's waiting on one has been said (once while it's behind).
    pub parked: Vec<crate::offer::Offer>,
    pub waiting_told: bool,
}

impl Lane {
    /// A lane for the checkout in `root` (a real path) — nothing open in it yet, its first pane `view`.
    pub fn new(id: LaneId, root: PathBuf, view: crate::split::ViewId) -> Self {
        let name =
            root.file_name().map_or_else(|| root.display().to_string(), |n| n.to_string_lossy().into());
        Self {
            id,
            root,
            name,
            branch: None,
            base: None,
            views: Vec::new(),
            split: crate::split::Split::Leaf(view),
            focus: view,
            current_doc: 0,
            scratch: None,
            chat: None,
            notes: Default::default(),
            checkpoints: crate::checkpoint::Checkpoints {
                dir: if cfg!(test) { None } else { crate::checkpoint::default_dir() },
                ..Default::default()
            },
            review: None,
            code_review: None,
            review_ask: false,
            sidebar: None,
            side_last: crate::sidebar::Mode::Files,
            terminal: None,
            terminal_run: 0,
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
            commit_then_end: false,
            git_queue: Default::default(),
            git_busy: false,
            git_reads: 0,
            git_branch: None,
            changed_files: Vec::new(),
            blocked: None,
            preflight: None,
            last_session: None,
            parked: Vec::new(),
            waiting_told: false,
        }
    }

    /// Whether its Claude waits for the user: a card of its, or an edit (a rename's files) to review.
    pub fn waits(&self) -> bool {
        self.parked.iter().any(crate::offer::Offer::claudes)
            || self.chat.as_ref().is_some_and(|c| {
                !c.edits.queue.is_empty() || c.edits.sets.iter().any(|l| !l.review.is_empty())
            })
    }
}

impl Editor {
    /// Whether a result of a lane behind is being applied (`with_lane`): what would take the screen waits.
    pub fn background(&self) -> bool {
        self.behind
    }

    /// `msg` as a toast says it: from a lane behind, its name first.
    pub(crate) fn said_by_lane(&self, msg: String) -> String {
        match self.behind {
            true => format!("{} · {msg}", self.lane.name.clone()),
            false => msg,
        }
    }

    /// Applies `f` in lane `id` — swapped in for it if it's behind, as it is if it's in front or gone.
    pub fn with_lane(&mut self, id: LaneId, f: impl FnOnce(&mut Editor)) {
        let Some(i) = self.lanes.iter().position(|l| l.id == id) else { return f(self) };
        let front = self.lane.id;
        self.lane.current_doc = self.doc().id;
        std::mem::swap(&mut self.lane, &mut self.lanes[i]);
        self.current = self.doc_index(self.lane.current_doc);
        self.events.set_lane(id);
        self.behind = true;
        let said = self.messages.last().map(|m| m.0);
        f(self);
        self.behind = false;
        let tell = self.lane.waits() && !std::mem::replace(&mut self.lane.waiting_told, true);
        let name = self.lane.name.clone();
        if let Some(d) = self.docs.get(self.current) {
            self.lane.current_doc = d.id;
        }
        let i =
            self.lanes.iter().position(|l| l.id == front).expect("the front lane is behind while swapped");
        std::mem::swap(&mut self.lane, &mut self.lanes[i]);
        self.current = self.doc_index(self.lane.current_doc);
        self.events.set_lane(front);
        if tell {
            self.set_status(format!("{name} · Claude waits for your answer"));
        } else if self.messages.last().map(|m| m.0) == said {
            // (nothing of it is on screen — an idle front draws nothing for a busy lane behind)
            self.unchanged = true;
        }
    }

    /// The index of document `id` — the first one's if it's gone (closed from another lane).
    fn doc_index(&self, id: DocId) -> usize {
        self.docs.iter().position(|d| d.id == id).unwrap_or(0)
    }

    /// `:lane-open <folder>` — a lane for that checkout (a worktree, a clone), in front; the one there
    /// already if it has a lane.
    pub fn lane_open(&mut self, folder: &Path) -> Result<LaneId, String> {
        let folder = self.path_of(folder);
        let root = std::fs::canonicalize(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
        if !root.is_dir() {
            return Err(format!("{} isn't a folder", root.display()));
        }
        if let Some(id) =
            std::iter::once(&self.lane).chain(&self.lanes).find(|l| l.root == root).map(|l| l.id)
        {
            self.lane_switch(id);
            return Ok(id);
        }
        let id = self.lane_add(root);
        self.lane_switch(id);
        Ok(id)
    }

    /// A lane for the checkout in `root` (a real path), behind — its first pane on a blank scratch, its
    /// branch watched.
    pub(crate) fn lane_add(&mut self, root: PathBuf) -> LaneId {
        self.next_lane += 1;
        let id = self.next_lane;
        let view = self.next_view;
        self.next_view += 1;
        let doc = self.alloc_id();
        self.docs.push(Document::from_str(doc, ""));
        let mut lane = Lane::new(id, root.clone(), view);
        lane.views.push(View::new(view, doc));
        (lane.current_doc, lane.scratch) = (doc, Some(doc));
        self.lanes.push(lane);
        if self.git_auto {
            // (its results are its lane's — tagged so while the watch is made)
            self.events.set_lane(id);
            crate::git::watch(self.events.sender(), root);
            self.events.set_lane(self.lane.id);
        }
        id
    }

    /// `:lane-switch <n>` — lane `id` to the front. What was up over the one in front closes (a picker, a
    /// prompt, insert mode); Claude's cards there go with it. False: no such lane.
    pub fn lane_switch(&mut self, id: LaneId) -> bool {
        if id == self.lane.id {
            return true;
        }
        let Some(i) = self.lanes.iter().position(|l| l.id == id) else { return false };
        let esc = Key { code: Code::Esc, ctrl: false, alt: false };
        if self.picker.is_some() {
            self.handle_key(esc);
        }
        if self.prompt.is_some() {
            self.handle_key(esc);
        }
        if self.mode != Mode::Normal {
            self.handle_key(esc);
            self.mode = Mode::Normal;
        }
        // Out: where it was, and Claude's cards (they wait for it)
        self.lane.current_doc = self.doc().id;
        let (mut mine, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.offers).into_iter().partition(crate::offer::Offer::claudes);
        self.offers = rest;
        mine.append(&mut self.lane.parked);
        self.lane.parked = mine;
        self.lane.waiting_told = false;
        std::mem::swap(&mut self.lane, &mut self.lanes[i]);
        // In: its panes on documents that are still there, its cards up, its tour going on
        self.current = self.doc_index(self.lane.current_doc);
        let shown = self.docs[self.current].id;
        let docs: std::collections::HashSet<DocId> = self.docs.iter().map(|d| d.id).collect();
        for v in self.lane.views.iter_mut().filter(|v| !docs.contains(&v.doc)) {
            v.doc = shown;
        }
        self.events.set_lane(id);
        self.offers.append(&mut self.lane.parked);
        self.lane.waiting_told = false;
        crate::edits::next(self);
        crate::follow::lane_in(self);
        // (which one is in front is kept too — `:restart` and the next start come back to it)
        self.lanes_keep();
        true
    }

    /// The lanes in the order they were opened: (id, name, in front).
    pub fn lanes_listed(&self) -> Vec<(LaneId, String, bool)> {
        let mut v: Vec<_> = std::iter::once((self.lane.id, self.lane.name.clone(), true))
            .chain(self.lanes.iter().map(|l| (l.id, l.name.clone(), false)))
            .collect();
        v.sort_by_key(|l| l.0);
        v
    }
}
