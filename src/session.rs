//! Sessions: one checkout's work each — its panes, chat, terminal, tests, tasks, git and review. The editor
//! holds the one in front as `Editor.session` and the others in `Editor.sessions`; a switch is one swap, so no
//! field is forgotten. What every session shares (documents, servers, config, toasts, the generation
//! counters) stays on `Editor`. Making, ending and keeping them: `worktree.rs`.
//!
//! - A session behind goes on working: what it started (its Claude, terminal, tests, tasks, git) sends its
//!   results tagged with it (`event::Tx`), and `with_session` swaps it in to apply them, then back.
//! - What would take the screen waits in the session: a card (`push_offer` parks it), an edit review (its
//!   queue isn't taken up — `edits::next`), follow mode's moves (its trail is kept). Claude waiting on one
//!   is said once, named. A tag for a session that's gone runs in front — the generation checks drop it.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::document::{DocId, Document};
use crate::editor::{Editor, Mode, View};
use crate::event::SessionId;
use crate::key::{Code, Key};

pub struct Session {
    pub id: SessionId,
    /// The checkout, links resolved: where git, the terminal, tasks, Claude, pickers and searches run, what
    /// paths are shown relative to, what a relative path means (follow mode goes nowhere else). Read once,
    /// in `Editor::with_root`; nothing reads the process's folder after that. Tests with a project of their
    /// own in a temp folder set it.
    pub root: PathBuf,
    /// What it's called (toasts from it start with it): a session's name, else its checkout's folder name.
    pub name: String,
    /// A session's branch, as it was made (`worktree.rs`) · what it was made from — a branch, or a commit
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
    /// Its buffers: the documents opened or shown in it (`:bn`/`:bp`, the tab row, `space b` go through
    /// these — `Editor::buffers`). A document can be another session's too.
    pub docs: Vec<DocId>,
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
    /// The built-in terminal (terminal.rs) · the run that's this session's latest (a replaced one's late
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
    /// The bottom slot's panel put in front (a tab, `C-w t`, one just opened) · what was there last event,
    /// so a new one is told from one already seen (`chrome::slot_sync`).
    pub slot_front: Option<crate::sizes::Slot>,
    pub slot_seen: Vec<(crate::sizes::Slot, Option<std::time::Instant>)>,
    /// The list `]q` goes through (quickfix.rs).
    pub qlist: Option<crate::quickfix::List>,
    /// Git: the repository's status (the side panel's git tab) · a discard waiting for its second `d` · a
    /// commit message being written (gitpanel.rs).
    pub git_status: Option<crate::gitpanel::Status>,
    pub git_discard_asked: Option<(String, std::time::Instant)>,
    pub committing: Option<crate::gitpanel::Committing>,
    /// The commit message being opened is for ending the session after it (`worktree.rs`).
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
    /// Its checkout's rebase onto its base, under way (`rebase.rs`) · its bring waits for the preflight to
    /// pass (the bring card's `p`).
    pub rebase: Option<crate::rebase::Rebase>,
    pub bring_after_preflight: bool,
    /// The files last open in this folder that are still there — the start screen's `r` (`restore.rs`).
    pub restorable: Option<Vec<PathBuf>>,
    /// Cards that came while it was behind, and Claude's that were up when it left — up again when it's in
    /// front · Claude's waiting on one has been said (once while it's behind).
    pub parked: Vec<crate::offer::Offer>,
    pub waiting_told: bool,
    /// Its documents whose servers stopped while it rested behind (`lsp_rest`) — attached again when it's in
    /// front, or its Claude starts a turn.
    pub asleep: Vec<DocId>,
    /// Its Claude's tool calls waiting for the editor's answer (`tools::WATCHED`) — one silent past its time
    /// marks the session possibly stuck (`Doing::Stuck`).
    pub tool_waits: Vec<ToolWait>,
    /// The code review's viewed marks, by file — kept past the review (#9: reopened, they were gone) while the
    /// file's diff is what it was when marked (`codereview::Viewed`).
    pub viewed: std::collections::HashMap<PathBuf, crate::codereview::Viewed>,
}

/// How long a tool call may wait before its session is marked possibly stuck (a `diagnostics` call's own
/// wait comes on top).
pub const STUCK_AFTER: Duration = Duration::from_secs(30);

/// A tool call waiting for its answer: the chat process and the call's id, which tool, since when, and how
/// long it may take.
pub struct ToolWait {
    generation: u64,
    id: serde_json::Value,
    tool: &'static str,
    since: Instant,
    after: Duration,
}

impl ToolWait {
    /// Whether it's call `id` of chat process `generation`.
    pub fn is(&self, generation: u64, id: &serde_json::Value) -> bool {
        self.generation == generation && self.id == *id
    }

    /// After how long what its mark says changes next (it comes up · its seconds · its minutes).
    fn next_change(&self, now: Instant) -> Duration {
        let waited = now.saturating_duration_since(self.since);
        let step = if waited < Duration::from_secs(60) { 1 } else { 60 };
        let next = Duration::from_secs(waited.as_secs() / step * step + step);
        match waited < self.after {
            true => self.after - waited,
            false => next - waited,
        }
    }
}

impl Session {
    /// A session for the checkout in `root` (a real path) — nothing open in it yet, its first pane `view`.
    pub fn new(id: SessionId, root: PathBuf, view: crate::split::ViewId) -> Self {
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
            docs: Vec::new(),
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
            slot_front: None,
            slot_seen: Vec::new(),
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
            rebase: None,
            bring_after_preflight: false,
            restorable: None,
            parked: Vec::new(),
            waiting_told: false,
            asleep: Vec::new(),
            tool_waits: Vec::new(),
            viewed: Default::default(),
        }
    }

    /// What the tabs, the chip and the list call it: a session by its name (its branch), the checkout tarae
    /// started in by its branch (`main │ fix-login ● │ …`) — its folder's name until the branch is known.
    pub fn label(&self) -> &str {
        match (self.id, &self.git_branch) {
            (0, Some(b)) => b,
            _ => &self.name,
        }
    }

    /// Whether its Claude waits for the user: a card of its, or an edit (a rename's files) to review.
    pub fn waits(&self) -> bool {
        self.parked.iter().any(crate::offer::Offer::claudes)
            || self.chat.as_ref().is_some_and(|c| {
                !c.edits.queue.is_empty() || c.edits.sets.iter().any(|l| !l.review.is_empty())
            })
    }

    /// Its Claude's oldest tool call still unanswered past its time (its chat at work, the call this
    /// process's).
    fn stuck(&self, now: Instant) -> Option<&ToolWait> {
        let chat = self.chat.as_ref().filter(|c| c.busy())?;
        self.tool_waits
            .iter()
            .filter(|w| {
                w.generation == chat.generation() && now.saturating_duration_since(w.since) >= w.after
            })
            .min_by_key(|w| w.since)
    }
}

/// What a click on the sessions' tabs or chip does (`Screen.session_spots`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spot {
    /// That session to the front.
    Session(SessionId),
    /// `+`: a new session.
    New,
    /// The dropdown's name, the status line's chip: the list (`space W`).
    List,
    /// The dropdown's `◂` · `▸`: the session before · after the one in front.
    Prev,
    Next,
}

/// What a session's Claude is doing, as its tab and its row in the session list say it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Doing {
    Idle,
    /// Answering — `●`.
    Working,
    /// Waiting for your answer: a card of its, an edit to review — `▲` (before working: it's both).
    Waiting,
    /// Possibly stuck: a call to one of tarae's tools has had no answer for `secs` — `◦ symbols 42s`
    /// (`said`). Before working: the work may be going nowhere.
    Stuck {
        tool: &'static str,
        secs: u64,
    },
}

impl Doing {
    /// A stuck call as its mark says it: the tool and how long, the time as wide as it stays (`symbols 42s`,
    /// `symbols  3m`) — empty for the rest.
    pub fn said(&self) -> String {
        match *self {
            Doing::Stuck { tool, secs } if secs < 60 => format!("{tool} {secs:>2}s"),
            Doing::Stuck { tool, secs } => format!("{tool} {:>2}m", secs / 60),
            _ => String::new(),
        }
    }
}

impl Editor {
    /// Whether a result of a session behind is being applied (`with_session`): what would take the screen
    /// waits.
    pub fn background(&self) -> bool {
        self.behind
    }

    /// What session `l`'s Claude is doing — in front, its cards are up in `offers`.
    pub fn doing(&self, l: &Session) -> Doing {
        let front = l.id == self.session.id && !self.behind;
        if l.waits() || (front && self.offers.iter().any(crate::offer::Offer::claudes)) {
            Doing::Waiting
        } else if let Some(w) = l.stuck(Instant::now()) {
            Doing::Stuck { tool: w.tool, secs: w.since.elapsed().as_secs() }
        } else if l.chat.as_ref().is_some_and(crate::chat::Chat::busy) {
            Doing::Working
        } else {
            Doing::Idle
        }
    }

    /// Claude's call `id` (of chat process `generation`, in the session in `ed.session`) to `tool` waits for
    /// its answer from now on, `after` its time — watched: woken when its mark would come up, then as its
    /// time moves on, until it's answered (`tools::answer` takes it off; a turn's end, all of them).
    pub(crate) fn tool_waits(
        &mut self,
        generation: u64,
        id: serde_json::Value,
        tool: &'static str,
        after: Duration,
    ) {
        self.session.tool_waits.push(ToolWait {
            generation,
            id: id.clone(),
            tool,
            since: Instant::now(),
            after,
        });
        self.stuck_watch(generation, id, after);
    }

    /// Wakes the main loop after `wake` — a frame for a stuck call's mark (in front: whichever session's it
    /// is) — and again while it waits; nothing once it's answered.
    fn stuck_watch(&self, generation: u64, id: serde_json::Value, wake: Duration) {
        self.events.shared_jobs().spawn(move || {
            std::thread::sleep(wake);
            move |ed: &mut Editor| {
                let now = Instant::now();
                let next = std::iter::once(&ed.session)
                    .chain(&ed.sessions)
                    .flat_map(|l| &l.tool_waits)
                    .find(|w| w.is(generation, &id))
                    .map(|w| w.next_change(now));
                match next {
                    Some(next) => ed.stuck_watch(generation, id, next),
                    None => ed.unchanged = true,
                }
            }
        });
    }

    /// How many sessions behind wait for your answer.
    pub fn sessions_waiting(&self) -> usize {
        self.sessions.iter().filter(|l| self.doing(l) == Doing::Waiting).count()
    }

    /// How many of Claude's questions wait behind the card up — cards stacked under it, its chat's reads and
    /// edits queued (parallel calls queued silently), a change set's files, and what waits in sessions
    /// behind (#7: the card's `1/N`).
    pub fn cards_behind(&self) -> usize {
        let queued = |l: &Session| {
            l.chat.as_ref().map_or(0, |c| {
                c.reads.len() + c.edits.queue.len() + c.edits.sets.iter().map(|s| s.waiting()).sum::<usize>()
            })
        };
        let behind: usize =
            self.sessions.iter().map(|l| l.parked.iter().filter(|o| o.claudes()).count() + queued(l)).sum();
        self.offers.len().saturating_sub(1) + queued(&self.session) + behind
    }

    /// `C-q` with no card up in front (#7): to the first session behind whose Claude waits for you — its card
    /// comes up there. False: none waits.
    pub(crate) fn go_to_waiting_card(&mut self) -> bool {
        let Some(id) = self.sessions.iter().find(|l| self.doing(l) == Doing::Waiting).map(|l| l.id) else {
            return false;
        };
        self.session_switch(id)
    }

    /// How many sessions behind are possibly stuck (`Doing::Stuck`) — `◦n` where their tabs aren't drawn.
    pub fn sessions_stuck(&self) -> usize {
        self.sessions.iter().filter(|l| matches!(self.doing(l), Doing::Stuck { .. })).count()
    }

    /// The front session's buffers as indexes into `docs`, in their order there (the one shown always among
    /// them) — every document while there's one session.
    pub fn buffers(&self) -> Vec<usize> {
        let mine = |i: usize, d: &Document| {
            self.sessions.is_empty() || i == self.current || self.session.docs.contains(&d.id)
        };
        self.docs.iter().enumerate().filter(|&(i, d)| mine(i, d)).map(|(i, _)| i).collect()
    }

    /// Document `id` is one of the front session's buffers from now on.
    pub(crate) fn session_keeps(&mut self, id: DocId) {
        if !self.session.docs.contains(&id) {
            self.session.docs.push(id);
        }
    }

    /// The session whose checkout `path` is in (the innermost, when one is inside another).
    pub fn session_of(&self, path: &Path) -> Option<SessionId> {
        std::iter::once(&self.session)
            .chain(&self.sessions)
            .filter(|l| path.starts_with(&l.root))
            .max_by_key(|l| l.root.as_os_str().len())
            .map(|l| l.id)
    }

    /// Session `l`'s processes: its chat, terminal, task, tests, and its checkout's language servers.
    pub fn session_pids(&self, l: &Session) -> Vec<u32> {
        let servers =
            self.lsp.clients.iter().filter(|c| self.session_of(&c.root) == Some(l.id)).map(|c| c.pid());
        let mine = [
            l.chat.as_ref().and_then(|c| c.pid()),
            l.terminal.as_ref().and_then(|t| t.pid()),
            l.task_run.as_ref().and_then(|r| r.pid()),
            l.test_run.as_ref().and_then(|r| r.pid()),
        ];
        mine.into_iter().chain(servers).flatten().collect()
    }

    /// Every event: the session list says what's so now; with sessions behind, their servers rest while
    /// their Claude is idle (and stop after a while — `lsp_rest`), and wake when it starts a turn. A server
    /// one of the front session's files uses never rests.
    pub(crate) fn sessions_sync(&mut self) {
        self.session_list_sync();
        if self.sessions.is_empty() {
            return;
        }
        let in_front = |ed: &Editor, cid| {
            ed.docs.iter().any(|d| ed.session.docs.contains(&d.id) && d.lsp.clients().any(|c| c == cid))
        };
        let resting: Vec<(crate::lsp::ClientId, bool)> = self
            .lsp
            .clients
            .iter()
            .map(|c| {
                let behind = self.sessions.iter().find(|l| Some(l.id) == self.session_of(&c.root));
                let idle = behind.is_some_and(|l| !l.chat.as_ref().is_some_and(crate::chat::Chat::busy));
                (c.id, idle && !in_front(self, c.id))
            })
            .collect();
        self.lsp_rest(&resting);
        // A session behind whose Claude started a turn: its servers back (its tools ask them)
        let woken: Vec<DocId> = self
            .sessions
            .iter_mut()
            .filter(|l| l.chat.as_ref().is_some_and(crate::chat::Chat::busy))
            .flat_map(|l| std::mem::take(&mut l.asleep))
            .collect();
        for id in woken {
            self.attach_lsp(id);
        }
    }

    /// `msg` as a toast says it: from a session behind, its name first (as its tab says it).
    pub(crate) fn said_by_session(&self, msg: String) -> String {
        match self.behind {
            true => format!("{} · {msg}", self.session.label()),
            false => msg,
        }
    }

    /// Applies `f` in session `id` — swapped in for it if it's behind, as it is if it's in front or gone.
    pub fn with_session(&mut self, id: SessionId, f: impl FnOnce(&mut Editor)) {
        let Some(i) = self.sessions.iter().position(|l| l.id == id) else { return f(self) };
        let front = self.session.id;
        self.session.current_doc = self.doc().id;
        std::mem::swap(&mut self.session, &mut self.sessions[i]);
        self.current = self.doc_index(self.session.current_doc);
        self.events.set_session(id);
        // (behind as it was after — a swap inside a swap leaves the outer one's session behind still)
        let was = std::mem::replace(&mut self.behind, true);
        let said = self.messages.last().map(|m| m.0);
        f(self);
        self.behind = was;
        let tell = self.session.waits() && !std::mem::replace(&mut self.session.waiting_told, true);
        let name = self.session.label().to_string();
        if let Some(id) = self.docs.get(self.current).map(|d| d.id) {
            self.session.current_doc = id;
            self.session_keeps(id);
        }
        let i = self
            .sessions
            .iter()
            .position(|l| l.id == front)
            .expect("the front session is behind while swapped");
        std::mem::swap(&mut self.session, &mut self.sessions[i]);
        self.current = self.doc_index(self.session.current_doc);
        self.events.set_session(front);
        if tell {
            self.set_status(format!("{name} · Claude waits for your answer — C-q goes there"));
        } else if self.messages.last().map(|m| m.0) == said {
            // (nothing of it is on screen — an idle front draws nothing for a busy session behind)
            self.unchanged = true;
        }
    }

    /// The index of document `id` — the first one's if it's gone (closed from another session).
    fn doc_index(&self, id: DocId) -> usize {
        self.docs.iter().position(|d| d.id == id).unwrap_or(0)
    }

    /// `:session-open <folder>` — a session for that checkout (a worktree, a clone), in front; the one there
    /// already if it has a session.
    pub fn session_open(&mut self, folder: &Path) -> Result<SessionId, String> {
        let folder = self.path_of(folder);
        let root = std::fs::canonicalize(&folder).map_err(|e| format!("{}: {e}", folder.display()))?;
        if !root.is_dir() {
            return Err(format!("{} isn't a folder", root.display()));
        }
        if let Some(id) =
            std::iter::once(&self.session).chain(&self.sessions).find(|l| l.root == root).map(|l| l.id)
        {
            self.session_switch(id);
            return Ok(id);
        }
        let id = self.session_add(root);
        self.session_switch(id);
        Ok(id)
    }

    /// A session for the checkout in `root` (a real path), behind — its first pane on a blank scratch, its
    /// branch watched.
    pub(crate) fn session_add(&mut self, root: PathBuf) -> SessionId {
        self.next_session += 1;
        let id = self.next_session;
        let view = self.next_view;
        self.next_view += 1;
        let doc = self.alloc_id();
        self.docs.push(Document::from_str(doc, ""));
        let mut session = Session::new(id, root.clone(), view);
        session.views.push(View::new(view, doc));
        (session.current_doc, session.scratch, session.docs) = (doc, Some(doc), vec![doc]);
        self.sessions.push(session);
        if self.git_auto {
            // (its results are its session's — tagged so while the watch is made)
            self.events.set_session(id);
            crate::git::watch(self.events.sender(), root);
            self.events.set_session(self.session.id);
        }
        id
    }

    /// `:session-switch <n>` — session `id` to the front. What was up over the one in front closes (a
    /// picker, a prompt, insert mode); Claude's cards there go with it. False: no such session.
    pub fn session_switch(&mut self, id: SessionId) -> bool {
        if id == self.session.id {
            return true;
        }
        let Some(i) = self.sessions.iter().position(|l| l.id == id) else { return false };
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
        self.session.current_doc = self.doc().id;
        let (mut mine, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.offers).into_iter().partition(crate::offer::Offer::claudes);
        self.offers = rest;
        mine.append(&mut self.session.parked);
        self.session.parked = mine;
        self.session.waiting_told = false;
        std::mem::swap(&mut self.session, &mut self.sessions[i]);
        // In: its panes on documents that are still there (its own, else a blank one), its cards up, its tour
        // going on
        let here = |id: DocId| self.docs.iter().position(|d| d.id == id);
        match here(self.session.current_doc).or_else(|| self.session.docs.iter().find_map(|&d| here(d))) {
            Some(i) => self.current = i,
            None => self.new_scratch(),
        }
        let shown = self.docs[self.current].id;
        let docs: std::collections::HashSet<DocId> = self.docs.iter().map(|d| d.id).collect();
        for v in self.session.views.iter_mut().filter(|v| !docs.contains(&v.doc)) {
            v.doc = shown;
        }
        self.events.set_session(id);
        // (servers that stopped while it rested: back for its files)
        for d in std::mem::take(&mut self.session.asleep) {
            self.attach_lsp(d);
        }
        self.offers.append(&mut self.session.parked);
        self.session.waiting_told = false;
        crate::edits::next(self);
        crate::follow::session_in(self);
        // (which one is in front is kept too — `:restart` and the next start come back to it)
        self.sessions_keep();
        true
    }

    /// A click on the sessions' tabs, dropdown or chip.
    pub fn session_spot(&mut self, s: Spot) {
        let order: Vec<SessionId> = self.sessions_in_order().iter().map(|l| l.id).collect();
        let at = order.iter().position(|&id| id == self.session.id).unwrap_or(0) as isize;
        let step = |by: isize| order[(at + by).rem_euclid(order.len() as isize) as usize];
        match s {
            Spot::Session(id) => {
                self.session_switch(id);
            }
            Spot::New => self.session_prompt(),
            Spot::List => self.session_list(),
            Spot::Prev => {
                self.session_switch(step(-1));
            }
            Spot::Next => {
                self.session_switch(step(1));
            }
        }
    }

    /// The sessions in the order they were opened: (id, name, in front).
    pub fn sessions_listed(&self) -> Vec<(SessionId, String, bool)> {
        let mut v: Vec<_> = std::iter::once((self.session.id, self.session.name.clone(), true))
            .chain(self.sessions.iter().map(|l| (l.id, l.name.clone(), false)))
            .collect();
        v.sort_by_key(|l| l.0);
        v
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use serde_json::{Value, json};

    use super::*;
    use crate::lsp_editor::Kind;

    fn feed(ed: &mut Editor, keys: &str) {
        let mut chars = keys.chars();
        while let Some(c) = chars.next() {
            let key = match c {
                '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
                c => Key::plain(Code::Char(c)),
            };
            ed.handle_key(key);
        }
        ed.lsp_flush();
    }

    /// #7: the card up says how many wait behind it — stacked, queued in its chat, in sessions behind — and
    /// `C-q` with no card up here goes to the session whose Claude waits, its card up there.
    #[test]
    fn the_card_counts_what_waits_and_c_q_goes_to_it() {
        use crate::offer::{Offer, What};
        let (mut ed, front, back) = two_projects("waiting-cards");
        let screen_card = |n: u64| {
            let what = What::Screen { call: crate::tools::Call::test(n) };
            Offer::new(format!("Claude asks {n}"), "to see the screen", "", String::new(), what)
        };
        let back_id = ed.session_open(&back).unwrap();
        ed.session.parked.push(screen_card(1));
        assert!(ed.session_switch(0) && ed.session.root == front);
        assert_eq!(ed.sessions_waiting(), 1);
        // In front: one card up, one under it — and the one behind
        ed.push_offer(screen_card(2));
        ed.push_offer(screen_card(3));
        assert_eq!(ed.cards_behind(), 2);
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 120, 30).unwrap();
        let mut parser = vt100::Parser::new(30, 120, 0);
        parser.process(&bytes);
        let s = parser.screen().contents();
        assert!(s.contains("Claude asks 2") && s.contains("1/3"), "{s}");
        // C-q with cards up here: the card's (it takes the keys)
        feed(&mut ed, "<C-q>");
        assert!(ed.card_keys && ed.session.id == 0);
        ed.offers.clear();
        ed.card_keys = false;
        // None up here: C-q goes to the session that waits, its card up
        feed(&mut ed, "<C-q>");
        assert_eq!(ed.session.id, back_id, "to the session whose Claude waits");
        assert_eq!(ed.offers.first().map(|o| o.title.as_str()), Some("Claude asks 1"));
        // Nothing waits anywhere: said
        ed.offers.clear();
        feed(&mut ed, "<C-q>");
        assert_eq!(ed.session.id, back_id);
        assert!(ed.status.as_ref().is_some_and(|s| s.0.contains("no card waits")), "{:?}", ed.status);
    }

    /// Two checkouts, `front` and `back`, each a cargo project with a `main.rs` — and an editor whose rust
    /// server is a stand-in (`cat`: the test answers for it, as its reader thread would, in whichever session
    /// is in front).
    fn two_projects(name: &str) -> (Editor, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tarae-sessions-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        for p in ["front", "back"] {
            std::fs::create_dir_all(dir.join(p)).unwrap();
            std::fs::write(dir.join(p).join("Cargo.toml"), "[package]\nname = \"t\"\n").unwrap();
            std::fs::write(dir.join(p).join("main.rs"), "fn main() {}\n").unwrap();
        }
        let dir = std::fs::canonicalize(dir).unwrap();
        let cfg = "[lsp.fake]\ncommand = \"sh\"\nargs = [\"-c\", \"cat > /dev/null\"]\n[lang.rust]\nlsp = [\"fake\"]\n";
        let (config, w) = crate::config::parse(cfg);
        assert!(w.is_empty(), "{w:?}");
        let mut ed = Editor::new(config);
        ed.session.root = dir.join("front");
        ed.session.name = "front".into();
        let front = dir.join("front/main.rs");
        ed.open(&front).unwrap();
        ready(&mut ed);
        (ed, dir.join("front"), dir.join("back"))
    }

    /// The server of the document in view says it's up, with what Claude's tools ask of it.
    fn ready(ed: &mut Editor) {
        let cid = ed.doc().lsp.client.expect("attached");
        let caps = json!({ "positionEncoding": "utf-8", "documentSymbolProvider": true,
            "referencesProvider": true, "definitionProvider": true, "hoverProvider": true });
        ed.on_lsp_message(cid, json!({ "id": 0, "result": { "capabilities": caps } }));
    }

    /// A fake claude that, given a message, makes each tool call in `calls` in turn — waiting, as claude
    /// does, for each one's answer (`"q-<tool>"`) before the next — then ends its turn. All it hears goes
    /// to `log`.
    fn claude_calls(ed: &mut Editor, log: &Path, calls: &[(&str, Value)]) {
        let mut body = String::new();
        for (i, (tool, args)) in calls.iter().enumerate() {
            let call = json!({"type": "control_request", "request_id": format!("q-{tool}"), "request": {
                "subtype": "mcp_message", "server_name": "tarae", "message": {"jsonrpc": "2.0", "id": i + 1,
                "method": "tools/call", "params": {"name": tool, "arguments": args}}}});
            body.push_str(&format!(
                "printf '%s\\n' '{call}'; while read -r l; do printf '%s\\n' \"$l\" >> '{log}'; \
                 case \"$l\" in *'\"q-{tool}\"'*) break;; esac; done; ",
                log = log.display()
            ));
        }
        let result = json!({"type": "result", "result": "done"});
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac; \
                 {body}printf '%s\\n' '{result}'; done"
            ),
        ];
    }

    fn session_by(ed: &Editor, id: SessionId) -> &Session {
        std::iter::once(&ed.session).chain(&ed.sessions).find(|l| l.id == id).expect("the session")
    }

    fn session_by_mut(ed: &mut Editor, id: SessionId) -> &mut Session {
        std::iter::once(&mut ed.session).chain(&mut ed.sessions).find(|l| l.id == id).expect("the session")
    }

    /// What claude was answered for call `q-<tool>` (None: not yet).
    fn answered(log: &Path, tool: &str) -> Option<String> {
        let heard = std::fs::read_to_string(log).unwrap_or_default();
        heard.lines().find(|l| l.contains(&format!("\"q-{tool}\""))).map(str::to_string)
    }

    /// The stand-in server's answers to Claude's tools' questions sent so far — given as its reader thread
    /// gives them: on the main loop, whichever session is in front.
    fn serve(ed: &mut Editor, file: &Path) {
        let range = json!({ "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 7 } });
        let at = json!({ "uri": crate::lsp::uri(file), "range": range });
        let asked: Vec<_> = ed.lsp.pending.iter().map(|(k, p)| (*k, p.kind.clone())).collect();
        for ((cid, id), kind) in asked {
            let result = match kind {
                Kind::ToolSymbols { .. } => json!([{ "name": "main", "kind": 12,
                    "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 12 } },
                    "selectionRange": range }]),
                Kind::ToolRefs { .. } | Kind::ToolGoto { .. } => json!([at]),
                Kind::ToolDeclared { .. } => at.clone(),
                Kind::ToolHover { .. } => json!({ "contents": { "kind": "markdown", "value": "fn main()" } }),
                _ => continue,
            };
            ed.on_lsp_message(cid, json!({ "jsonrpc": "2.0", "id": id, "result": result }));
        }
    }

    /// Session `back` opened with its file (its own server, up), its Claude asked something — then `front`
    /// in front again, `back`'s Claude at work behind it. Returns `back`'s id and file.
    fn back_at_work(ed: &mut Editor, back: &Path) -> (SessionId, PathBuf) {
        let id = ed.session_open(back).unwrap();
        let file = back.join("main.rs");
        ed.open(&file).unwrap();
        ready(ed);
        feed(ed, "<space>lgo<ret><esc>");
        assert!(ed.session_switch(0) && ed.session.id == 0, "front in front");
        assert!(session_by(ed, id).chat.as_ref().unwrap().busy(), "back's Claude at work behind it");
        (id, file)
    }

    /// The bug, as it was met: a session behind called `symbols` and never heard back — its server
    /// answered on the main loop with the front session in front, so the answer went to the front's chat
    /// (none, or another process) and was dropped; the call's fallback had nothing left to answer. Here
    /// the session behind calls each of the tools that ask a server, in turn, and each is answered by the
    /// server's own answer.
    #[test]
    fn a_session_behind_hears_back_from_every_tool() {
        let (mut ed, front, back) = two_projects("tools");
        let log = back.join("claude-heard.log");
        let calls = [
            ("symbols", json!({ "path": "main.rs" })),
            ("references", json!({ "path": "main.rs", "symbol": "main" })),
            ("definition", json!({ "path": "main.rs", "symbol": "main" })),
            ("hover", json!({ "path": "main.rs", "symbol": "main" })),
            ("diagnostics", json!({ "paths": ["main.rs"], "wait": 5 })),
        ];
        claude_calls(&mut ed, &log, &calls);
        let (id, file) = back_at_work(&mut ed, &back);
        let back_server =
            ed.docs.iter().find(|d| d.path.as_deref() == Some(file.as_path())).unwrap().lsp.client;
        let end = Instant::now() + Duration::from_secs(20);
        let mut published = false;
        while session_by(&ed, id).chat.as_ref().unwrap().busy() {
            assert!(
                Instant::now() < end,
                "the session behind hangs — answered: {:?}",
                calls.iter().filter(|c| answered(&log, c.0).is_some()).map(|c| c.0).collect::<Vec<_>>()
            );
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
            assert_eq!(ed.session.id, 0, "front stays in front");
            serve(&mut ed, &file);
            // `diagnostics` waits for its server to settle: it publishes (past `SETTLE`) — news that lands
            // in front too
            if !published && !ed.lsp.tool_diags.is_empty() {
                std::thread::sleep(Duration::from_millis(30));
                let diag = json!({ "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0,
                    "character": 7 } }, "severity": 2, "message": "an unused stand-in", "source": "fake" });
                let publish = json!({ "jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
                    "params": { "uri": crate::lsp::uri(&file), "diagnostics": [diag] } });
                ed.on_lsp_message(back_server.unwrap(), publish);
                published = true;
            }
        }
        let said = |tool: &str| answered(&log, tool).unwrap_or_else(|| panic!("{tool} answered"));
        let symbols = said("symbols");
        assert!(symbols.contains("fn main") && symbols.contains("(fake)"), "the server's outline: {symbols}");
        assert!(said("references").contains("1 reference in 1 file (fake)"), "{}", said("references"));
        assert!(said("definition").contains("1 definition (fake)"), "{}", said("definition"));
        assert!(said("hover").contains("fn main()"), "{}", said("hover"));
        let diags = said("diagnostics");
        assert!(diags.contains("an unused stand-in"), "the published one: {diags}");
        // Its places named from its own checkout (answered in its session, not just delivered there)
        assert!(said("references").contains(r"first:\nmain.rs\n"), "{}", said("references"));
        assert!(ed.session.chat.is_none(), "nothing went to the front");
        assert!(session_by(&ed, id).tool_waits.is_empty(), "no call left waiting");
        std::fs::remove_dir_all(front.parent().unwrap()).ok();
    }

    /// A tool call unanswered past `STUCK_AFTER` marks its session possibly stuck — `◦`, the tool and how
    /// long, in its tab and its row in the list, `◦1` in the dropdown — not `●`; a call within its time
    /// doesn't, nor once it's answered.
    #[test]
    fn a_call_silent_too_long_marks_its_session() {
        let (mut ed, front, back) = two_projects("stuck");
        let log = back.join("claude-heard.log");
        claude_calls(&mut ed, &log, &[("symbols", json!({ "path": "main.rs" }))]);
        let (id, file) = back_at_work(&mut ed, &back);
        // (front's chat open: the tabs over it)
        feed(&mut ed, "<space>l");
        let asked = |ed: &Editor| ed.lsp.pending.values().any(|p| matches!(p.kind, Kind::ToolSymbols { .. }));
        let end = Instant::now() + Duration::from_secs(10);
        while !asked(&ed) {
            assert!(Instant::now() < end, "back's Claude called symbols");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        let w = &session_by(&ed, id).tool_waits;
        assert_eq!((w.len(), w[0].tool, w[0].after), (1, "symbols", STUCK_AFTER));
        let since = |ed: &mut Editor, ago: u64| {
            session_by_mut(ed, id).tool_waits[0].since = Instant::now() - Duration::from_secs(ago);
        };
        // Within its time: working
        since(&mut ed, 29);
        assert_eq!(ed.doing(session_by(&ed, id)), Doing::Working);
        // Past it: possibly stuck, said
        since(&mut ed, 42);
        let doing = ed.doing(session_by(&ed, id));
        assert_eq!(doing, Doing::Stuck { tool: "symbols", secs: 42 });
        assert_eq!(ed.sessions_stuck(), 1);
        let tabs = screen(&mut ed, 160, 24).into_iter().find(|r| r.contains("front │")).expect("the tabs");
        assert!(tabs.contains("back ◦ symbols 42s"), "{tabs}");
        assert!(!tabs.contains('●'), "{tabs}");
        let shown = |doing| crate::worktree::Shown {
            id,
            name: "back".into(),
            branch: None,
            facts: None,
            doing,
            front: false,
            usage: None,
            rebasing: None,
        };
        let row = &crate::worktree::items(&[shown(doing)])[0];
        assert!(row.hint.contains("stuck? symbols 42s"), "{}", row.hint);
        assert_eq!(row.glyph.map(|g| g.0), Some("◦"));
        // In minutes past one; no room for the tabs: the dropdown counts it
        since(&mut ed, 200);
        assert_eq!(ed.doing(session_by(&ed, id)).said(), "symbols  3m");
        session_by_mut(&mut ed, id).name = "back-and-forth-and-back-again".into();
        let narrow =
            screen(&mut ed, 100, 24).into_iter().find(|r| r.contains("◂ front")).expect("the dropdown");
        assert!(narrow.contains("◦1"), "{narrow}");
        // Answered: the mark goes
        serve(&mut ed, &file);
        let end = Instant::now() + Duration::from_secs(10);
        while session_by(&ed, id).chat.as_ref().unwrap().busy() {
            assert!(Instant::now() < end, "back's turn ends once answered");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        assert!(answered(&log, "symbols").is_some_and(|a| a.contains("(fake)")));
        assert!(session_by(&ed, id).tool_waits.is_empty());
        assert_eq!((ed.doing(session_by(&ed, id)), ed.sessions_stuck()), (Doing::Idle, 0));
        std::fs::remove_dir_all(front.parent().unwrap()).ok();
    }

    /// An answer for one session's Claude made while another session behind is swapped in isn't delivered
    /// inside that swap — and isn't dropped: it goes out from the main loop's next turn, in its own session.
    #[test]
    fn an_answer_made_behind_for_another_session_goes_out_next() {
        let (mut ed, front, back) = two_projects("deferred");
        let log = front.join("claude-heard.log");
        claude_calls(&mut ed, &log, &[("symbols", json!({ "path": "main.rs" }))]);
        let back_id = ed.session_open(&back).unwrap();
        assert!(ed.session_switch(0) && ed.session.id == 0, "front in front");
        feed(&mut ed, "<space>lgo<ret><esc>");
        let end = Instant::now() + Duration::from_secs(10);
        while ed.session.tool_waits.is_empty() {
            assert!(Instant::now() < end, "front's Claude called symbols");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        let generation = ed.session.chat.as_ref().unwrap().generation();
        let call = crate::tools::Call::of(generation, "q-symbols", json!(1));
        // Answered from inside `back`'s swap: front's call still waits as the swap ends
        ed.with_session(back_id, |ed| crate::tools::answer(ed, call, Ok("from behind".into())));
        assert_eq!(ed.session.tool_waits.len(), 1, "not delivered inside another session's swap");
        let end = Instant::now() + Duration::from_secs(10);
        while ed.session.chat.as_ref().unwrap().busy() {
            assert!(Instant::now() < end, "the answer went out — front's turn ends");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        let said = answered(&log, "symbols").expect("front's Claude heard back");
        assert!(said.contains("from behind"), "this answer, not the tool's own fallback: {said}");
        assert!(ed.session.tool_waits.is_empty() && ed.session.id == 0);
        assert!(session_by(&ed, back_id).chat.is_none(), "nothing went to back");
        std::fs::remove_dir_all(front.parent().unwrap()).ok();
    }

    /// The wake-ups for a waiting call's mark: when it comes up, then each second, then each minute.
    #[test]
    fn a_waiting_calls_mark_wakes_as_it_changes() {
        let t0 = Instant::now();
        let w = ToolWait { generation: 1, id: json!(1), tool: "hover", since: t0, after: STUCK_AFTER };
        let at = |s: f64| w.next_change(t0 + Duration::from_secs_f64(s));
        assert_eq!(at(0.0), STUCK_AFTER);
        assert_eq!(at(12.5), Duration::from_secs_f64(17.5));
        assert_eq!(at(30.0), Duration::from_secs(1));
        assert_eq!(at(42.25), Duration::from_millis(750));
        assert_eq!(at(61.0), Duration::from_secs(59));
        assert!(w.is(1, &json!(1)) && !w.is(2, &json!(1)) && !w.is(1, &json!(2)));
        let stuck = |secs| Doing::Stuck { tool: "hover", secs }.said();
        assert_eq!(
            (stuck(30), stuck(59), stuck(60), stuck(6000)),
            ("hover 30s".into(), "hover 59s".into(), "hover  1m".into(), "hover 100m".into())
        );
        assert_eq!(Doing::Working.said(), "");
    }

    fn screen(ed: &mut Editor, w: u16, h: u16) -> Vec<String> {
        let mut bytes = Vec::new();
        crate::term::render(ed, &mut bytes, w, h).unwrap();
        let mut vt = vt100::Parser::new(h, w, 0);
        vt.process(&bytes);
        vt.screen().rows(0, w).collect()
    }
}
