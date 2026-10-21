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
    /// Its checkout, known to the log while it lives: a line about a path in it names it (`log::Session::of`)
    /// by its label — told again when that changes (`git::watch`: the branch known, or switched).
    pub in_log: crate::log::Checkout,
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
    /// The in-buffer review (`llm.rs`) — `space i`'s, a chat edit's.
    pub review: Option<crate::llm::Review>,
    /// The code review in progress (`codereview.rs`).
    pub code_review: Option<crate::codereview::CodeReview>,
    /// The review being opened is for the reviewer (`space v c`) — asked once it's open.
    pub review_ask: bool,
    /// The review being opened is for the high-risk changes a bring's card names (its `h`, #196): those not
    /// looked at, and the card's id — it opens on the first (`codereview::held_go`).
    pub review_high: Option<(u64, Vec<crate::codereview::Held>)>,
    /// The bring's changes the review couldn't show as they are now, in a buffer of their own as the commits
    /// have them (`codereview::HeldView`) — `space v h` there looks at one.
    pub held_view: Option<crate::codereview::HeldView>,
    /// The reviewer — a second Claude reviewing the code review's changes, apart from the chat's
    /// conversation (`reviewer.rs`): reading, or what it found last.
    pub reviewer: Option<crate::reviewer::Reviewer>,
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
    /// Where the packs' default preflights shown on their card and run are kept, by clone and steps listed
    /// (`$state/preflight-told`, `preflight::told_key`) — None in tests: since tarae started only.
    pub preflight_kept: Option<PathBuf>,
    /// Its checkout's rebase onto its base, under way (`rebase.rs`) · its bring waits for the preflight to
    /// pass (the bring card's `p`).
    pub rebase: Option<crate::rebase::Rebase>,
    pub bring_after_preflight: bool,
    /// A turn's review `:restart` carried, waiting for the session's kept turns to load to open again
    /// (`codereview::reopen`; frictions15 — it held the restart).
    pub turn_review_back: Option<serde_json::Value>,
    /// The files last open in this folder that are still there — the start screen's `r` (`restore.rs`).
    pub restorable: Option<Vec<PathBuf>>,
    /// Cards that came while it was behind, and Claude's that were up when it left — up again when it's in
    /// front · Claude's waiting on one has been said (once while it's behind).
    pub parked: Vec<crate::offer::Offer>,
    /// The card that took the keys from the chat as it came (`push_offer`): once it's gone and no other is up,
    /// they go back to the chat (`Editor::chat_lent_sync`).
    pub chat_lent: Option<u64>,
    pub waiting_told: bool,
    /// Its Claude ended a turn while it was behind, and it hasn't been in front since: when, and how (stopped
    /// or failed: its words) — its tab says `done` (#86: not the `▲` of a card waiting), a toast says it once,
    /// and `C-q` goes there once nothing waits (the one that ended first, first).
    pub turn_end: Option<(Instant, Result<(), String>)>,
    /// Its documents whose servers stopped while it rested behind (`lsp_rest`) — attached again when it's in
    /// front, or its Claude starts a turn.
    pub asleep: Vec<DocId>,
    /// Its Claude's tool calls waiting for the editor's answer (`tools::WATCHED`) — one silent past its time
    /// marks the session possibly stuck (`Doing::Stuck`).
    pub tool_waits: Vec<ToolWait>,
    /// Runs waiting for their slot (the task panel, the test pane) — Claude's and, first in line, the user's
    /// own; each starts when its slot frees (`queued.rs`).
    pub runs_next: std::collections::VecDeque<crate::queued::Next>,
    /// When a file in its checkout was last saved or written (Claude's edit, a change set) — a run under way
    /// since then is no longer the one the same command would run (`queued::join`).
    pub wrote: Option<Instant>,
    /// Protected names Claude's runs made where none was (`snapshot::Found::protected`, from its root) — its
    /// next run waits on a card about them (`tools::ask_made`); a `y` there clears it.
    pub made_protected: Vec<crate::snapshot::Made>,
    /// Those kept on that card (`y`), as they were then — one a later run makes again holding the same isn't
    /// asked about; holding something else, it is.
    pub made_kept: Vec<crate::snapshot::Made>,
    /// A commit's tree being written for a run of its Claude's (`basetree.rs`) — another such run waits for
    /// that one's answer: they'd write the same tree.
    pub base_writing: bool,
    /// The run cards of its Claude's answered until you quit — each the last for its run in its folder
    /// (`sinceyes::Answered`): the same run asked again says `Same as card#41 — y at 18:02` and what changed
    /// since in what it runs (cardburden 3).
    pub answered: Vec<crate::sinceyes::Answered>,
    /// Its Claude's calls waiting on a card for the user's answer (`offer::Asking`) — each card must be up
    /// (in front) or parked (behind) until it's answered; one gone is told (`asking_sync`).
    pub asking: Vec<crate::offer::Asking>,
    /// The processes it started (runs, tasks, tests) while the OS still shows them — `:procs` and Claude's
    /// `procs` keep them under it after their run is closed or replaced (#100).
    pub spawned: Vec<crate::procs::Spawned>,
    /// The code review's viewed marks, by file — kept past the review (#9: reopened, they were gone) while the
    /// file's diff is what it was when marked (`codereview::Viewed`).
    pub viewed: std::collections::HashMap<PathBuf, crate::codereview::Viewed>,
    /// The changed lines a person looked at in a review (`space v h` on a high-risk change — `risk::keys`):
    /// what holds the review's end and the bring until they're all here (`risk.rs`).
    pub looked: std::collections::HashSet<crate::risk::Key>,
    /// The added lines a person looked at (`space v h`), by file (from the checkout's root): where each was
    /// then and its text — one the file no longer has is a change to look at again (`risk::taken_out`). Kept
    /// in the restore file with `looked`.
    pub looked_lines: std::collections::HashMap<String, Vec<crate::risk::LookedLine>>,
    /// Of each high-risk change looked at, by file: its added lines' keys in the order they were — one out of
    /// that order since (moved within the change) is a change to look at again (`risk::moved`). The latest
    /// look at a change replaces its order.
    pub looked_orders: std::collections::HashMap<String, Vec<crate::risk::LookedOrder>>,
    /// Its files that came again from disk (or changed there under an edit) while it was behind — said only
    /// in `:messages` (`disk_said`), so its tab, the chip and its row in the list carry a `•` (#6) until it
    /// comes to the front.
    pub reloaded: usize,
    /// What its Claude's turns cost and how warm its cache is (`usage.rs`).
    pub usage: crate::usage::Usage,
    /// Who pays for its Claude — the chat's, the ask's, the reviewer's unless `llm.reviewer-billing` says
    /// (`billing.rs`): `llm.billing` when it's made, kept with it (`worktree::Kept`), `:session-billing` switches it.
    pub billing: crate::billing::Billing,
    /// The billing its chat goes on once Claude's turn is over (the limit card's `y` mid-turn — `billing.rs`).
    pub billing_next: Option<crate::billing::Billing>,
    /// The plan's weekly window its limit card was up for (its reset, unix ms): not asked again for it.
    pub limit_asked: Option<u64>,
    /// Its Claude's conversation compacted since tarae started — how often, the last one, and whether one
    /// came behind and it hasn't been in front since (`compaction.rs`).
    pub compacted: crate::compaction::Compacted,
    /// The hosts its commands reach through tarae's proxy by the user's `s` (this session, until you quit) — over
    /// TLS or HTTP, raw, or a name's records looked up (6e+; a lookup's port 0) — what `:sandbox-allowed` lists and
    /// takes back; the proxy holds the answers itself (`hosts.rs`).
    pub hosts: Vec<(crate::sandbox::proxy::Kind, String, u16)>,
    /// What its Claude may do past the defaults by the user's `s` on a card (this session — every grant but a
    /// host's, `hosts`), kept with it in `$state/sessions` until it ends or `:sandbox-disallow` takes it back.
    pub allowed: crate::allow::Allowed,
    /// What its Claude may do, as last made (`Editor::caps_sync_of`) with the generation it was asked as — for
    /// what reads it on the main thread (the card line's shadow); None until the first lands. The gates make
    /// theirs on their workers, from what's answered at that moment.
    pub caps: Option<(u64, std::sync::Arc<crate::caps::Caps>)>,
    /// The last generation of its `caps` asked for: one landing older than what's there is dropped.
    pub caps_asked: u64,
    /// What its last sandboxed run's sandbox keeps apart and shares (9d) — its keeper's, or its Seatbelt policy's —
    /// as the run's policy has it (`Guard::guarantees`: made on the worker that made the policy, `linux::confine`),
    /// kept on the main loop once the run started (`tools::go_now` → `watch::sandbox`; one that didn't spawn
    /// leaves it as it was): the state reads it with no lock or probe. Shown only, never a run's input. None: no
    /// sandboxed run started yet — a keeper is born with the first.
    pub sandbox: Option<crate::sandbox::guarantees::Guarantees>,
    /// Its id for tarae's proxy, with the root it was worked out for (`proxy_id`).
    proxy_id: std::cell::RefCell<(PathBuf, u64)>,
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
        // (worked out once, here, as the root is made real here: the main loop names it with no look at the disk)
        let proxy_id = std::cell::RefCell::new((root.clone(), crate::sandbox::proxy::session_of(&root)));
        Self {
            id,
            in_log: crate::log::Checkout::new(&root, &name),
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
            checkpoints: Default::default(),
            review: None,
            code_review: None,
            review_ask: false,
            review_high: None,
            held_view: None,
            reviewer: None,
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
            preflight_kept: if cfg!(test) { None } else { crate::preflight::kept_path() },
            rebase: None,
            bring_after_preflight: false,
            turn_review_back: None,
            restorable: None,
            parked: Vec::new(),
            chat_lent: None,
            waiting_told: false,
            turn_end: None,
            asleep: Vec::new(),
            tool_waits: Vec::new(),
            runs_next: Default::default(),
            wrote: None,
            made_protected: Vec::new(),
            made_kept: Vec::new(),
            base_writing: false,
            answered: Vec::new(),
            asking: Vec::new(),
            spawned: Vec::new(),
            viewed: Default::default(),
            looked: Default::default(),
            looked_lines: Default::default(),
            looked_orders: Default::default(),
            reloaded: 0,
            usage: Default::default(),
            billing: Default::default(),
            billing_next: None,
            limit_asked: None,
            compacted: Default::default(),
            hosts: Vec::new(),
            allowed: Default::default(),
            caps: None,
            caps_asked: 0,
            sandbox: None,
            proxy_id,
        }
    }

    /// The session as tarae's proxy names it — `proxy::session_of` its root, resolved as a run's policy resolves
    /// it: the answers the main loop sends (`hosts.rs`) reach the session its runs' connections are of, whatever
    /// way the root was given. Worked out when the session is made, and again only if its root is changed since.
    pub fn proxy_id(&self) -> u64 {
        let mut c = self.proxy_id.borrow_mut();
        if c.0 != self.root {
            *c = (self.root.clone(), crate::sandbox::proxy::session_of(&self.root));
        }
        c.1
    }

    /// Whose its log lines are: what the list calls it (`label`) in the log's session column
    /// (`log::Session::line`).
    pub fn log(&self) -> crate::log::Session {
        crate::log::Session::named(self.label())
    }

    /// What the tabs, the chip and the list call it: a session by its name (its branch), the checkout tarae
    /// started in by its branch (`main │ fix-login ● │ …`) — its folder's name until the branch is known.
    pub fn label(&self) -> &str {
        match (self.id, &self.git_branch) {
            (0, Some(b)) => b,
            _ => &self.name,
        }
    }

    /// Whether it waits for the user: a card of its (Claude's, or tarae's come while it was behind — a
    /// rebase's, a trust card: #94, one counted nowhere was never found), or an edit (a rename's files) to
    /// review — the one up in its review too (#150: Claude waited on it while its session said `working`).
    pub fn waits(&self) -> bool {
        self.parked.iter().any(|o| !o.installing())
            || self.review.as_ref().is_some_and(|r| r.ready)
            || self.chat.as_ref().is_some_and(|c| {
                !c.edits.queue.is_empty() || c.edits.sets.iter().any(|l| !l.review.is_empty())
            })
    }

    /// Its reviewer (`reviewer.rs`) reads — said beside what its chat's Claude does (both can be at work).
    pub fn reviewing(&self) -> bool {
        self.reviewer.as_ref().is_some_and(crate::reviewer::Reviewer::reading)
    }

    /// Something came while it was behind that it hasn't been in front for since: files again from disk
    /// (#6), its reviewer's end (#195) — its tab's `•`.
    pub fn news(&self) -> bool {
        self.reloaded > 0 || self.reviewer.as_ref().is_some_and(|r| r.unseen)
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

/// How a turn ended, as a toast says it: `turn ended` · `turn stopped` · `turn ended: <its error's first
/// line>` (a warning).
pub fn turn_end_words(how: &Result<(), String>) -> (crate::editor::ToastKind, String) {
    use crate::editor::ToastKind;
    match how {
        Ok(()) => (ToastKind::Info, "turn ended".into()),
        Err(e) if e == "stopped" => (ToastKind::Info, "turn stopped".into()),
        Err(e) => {
            (ToastKind::Warning, format!("turn ended: {}", e.lines().next().unwrap_or_default().trim()))
        }
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
    /// The status line's `▲3 waiting`: the inbox.
    Inbox,
}

/// What a session's Claude is doing, as its tab and its row in the session list say it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Doing {
    Idle,
    /// Answering — `●`.
    Working,
    /// Waiting for your answer: a card of its, an edit to review — `▲ waits` (before working: it's both).
    Waiting,
    /// Its turn ended while it was behind, not looked at since — `done` (#86).
    Done,
    /// Possibly stuck: a call to one of tarae's tools has had no answer for `secs` — `◦ symbols 42s`
    /// (`said`). Before working: the work may be going nowhere.
    Stuck {
        tool: &'static str,
        secs: u64,
    },
    /// Its run waits in line for its slot, `pos`th (`queued.rs`) — `● queued 2nd`, dim: at work, held back,
    /// not stuck.
    Queued {
        pos: usize,
    },
}

impl Doing {
    /// A stuck call as its mark says it: the tool and how long, the time as wide as it stays (`symbols 42s`,
    /// `symbols  3m`); a run in line, where (`queued 2nd`) — empty for the rest.
    pub fn said(&self) -> String {
        match *self {
            Doing::Stuck { tool, secs } if secs < 60 => format!("{tool} {secs:>2}s"),
            Doing::Stuck { tool, secs } => format!("{tool} {:>2}m", secs / 60),
            Doing::Queued { pos } => format!("queued {}", crate::queued::ordinal(pos)),
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
        } else if let Some(pos) = crate::queued::position(l) {
            Doing::Queued { pos }
        } else if l.chat.as_ref().is_some_and(crate::chat::Chat::busy) {
            Doing::Working
        } else if l.turn_end.is_some() && !front {
            Doing::Done
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
    #[cfg(test)]
    pub fn sessions_waiting(&self) -> usize {
        self.sessions.iter().filter(|l| self.doing(l) == Doing::Waiting).count()
    }

    /// How many things wait for you (the inbox, `inbox.rs`) — here: the cards up (the front one too), an edit
    /// in its review, its chat's reads and edits queued (parallel calls queued silently), a change set's
    /// files — and in sessions behind (#7; #91: one `1/N` mixed the two, the card's `1/2 here · 1 elsewhere`
    /// keeps them apart). Only what asks for an answer (`Entry::asks_answer`): a quiet run, a reviewer's end
    /// is news, not something waiting.
    pub fn cards_waiting(&self) -> (usize, usize) {
        let front = crate::uistate::session_id(self.session.id);
        let inbox: Vec<_> = self.inbox().into_iter().filter(crate::inbox::Entry::asks_answer).collect();
        let here = inbox.iter().filter(|e| e.session == front).count();
        (here, inbox.len() - here)
    }

    /// What waits behind the card in front, as the card says it (frictions15 #5: `C-q` takes the oldest here
    /// first, so the others are a queue nobody named): `2 more wait — 1 here, 1 in feat · space I lists them`
    /// — each other session by its name, the key the keymap's. None: nothing else waits.
    pub fn cards_behind_said(&self) -> Option<String> {
        let front = crate::uistate::session_id(self.session.id);
        let up = self.offers.first().map(|o| format!("card#{}", o.id));
        let behind: Vec<_> =
            self.inbox().into_iter().filter(|e| e.asks_answer() && Some(&e.id) != up.as_ref()).collect();
        if behind.is_empty() {
            return None;
        }
        let mut wheres: Vec<(String, usize)> = Vec::new();
        for e in &behind {
            let at = match e.session == front {
                true => "here".to_string(),
                false => std::iter::once(&self.session)
                    .chain(&self.sessions)
                    .find(|l| crate::uistate::session_id(l.id) == e.session)
                    .map_or_else(|| format!("in {}", e.session), |l| format!("in {}", l.label())),
            };
            match wheres.iter_mut().find(|w| w.0 == at) {
                Some(w) => w.1 += 1,
                None => wheres.push((at, 1)),
            }
        }
        // (here first, then the sessions in the order their oldest came)
        crate::sorting::by_key(&mut wheres, |w| w.0 != "here");
        let wheres: Vec<String> = wheres.iter().map(|(at, n)| format!("{n} {at}")).collect();
        let lists = match self.keymaps.bindings().remove("inbox") {
            Some(key) => format!("{key} lists them"),
            None => ":inbox lists them".into(),
        };
        let n = behind.len();
        let wait = if n == 1 { "waits" } else { "wait" };
        Some(format!("{n} more {wait} — {} · {lists}", wheres.join(", ")))
    }

    /// Sessions whose Claude is about to ask (its next call needs a card, held back by claude until the one
    /// running ends — #5): their names, for `C-q` finding no card yet.
    pub(crate) fn cards_coming(&self) -> Vec<String> {
        std::iter::once(&self.session)
            .chain(&self.sessions)
            .filter(|l| l.chat.as_ref().is_some_and(|c| c.busy() && c.asks_next(&self.config.llm.run_allow)))
            .map(|l| l.label().to_string())
            .collect()
    }

    /// How many sessions behind ended a turn you haven't looked at (`Doing::Done`).
    pub fn sessions_done(&self) -> usize {
        self.sessions.iter().filter(|l| self.doing(l) == Doing::Done).count()
    }

    /// The sessions behind whose turn ended unseen (`Doing::Done`), the first to end first — where `C-q`
    /// goes once nothing waits (`inbox::Next::Done`).
    pub(crate) fn turns_ended(&self) -> Vec<SessionId> {
        let mut ended: Vec<(Instant, SessionId)> = self
            .sessions
            .iter()
            .filter(|l| self.doing(l) == Doing::Done)
            .filter_map(|l| Some((l.turn_end.as_ref()?.0, l.id)))
            .collect();
        crate::sorting::sort(&mut ended);
        ended.into_iter().map(|(_, id)| id).collect()
    }

    /// How `C-q` gets to session `id`'s news — a card or an edit of its waiting (`waits`), else its turn's
    /// end — as its toast says it: `C-q goes there`, or after how many came first (`C-q` takes the oldest).
    fn c_q_says(&self, id: SessionId, waits: bool) -> String {
        let inbox = self.inbox();
        let before = match waits {
            true => inbox.iter().position(|e| e.session_id() == id).unwrap_or(inbox.len()),
            false => inbox.len() + self.turns_ended().iter().position(|&l| l == id).unwrap_or(0),
        };
        match before {
            0 => "C-q goes there".into(),
            n => format!("C-q goes there after {n} older"),
        }
    }

    /// How many sessions behind are possibly stuck (`Doing::Stuck`) — `◦n` where their tabs aren't drawn.
    pub fn sessions_stuck(&self) -> usize {
        self.sessions.iter().filter(|l| matches!(self.doing(l), Doing::Stuck { .. })).count()
    }

    /// How many sessions behind had news since they were in front (`Session::news`: files again from disk,
    /// #6; a reviewer's end, #195) — `•n` where their tabs aren't drawn.
    pub fn sessions_with_news(&self) -> usize {
        self.sessions.iter().filter(|l| l.news()).count()
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
        // (a shared server is no one session's)
        let servers = self
            .lsp
            .clients
            .iter()
            .filter(|c| c.roots.len() == 1 && self.session_of(&c.root) == Some(l.id))
            .map(|c| c.pid());
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
        self.inbox_sync();
        self.asking_sync();
        self.waiting_sync();
        // (a shared server's folders follow the checkouts — `share.rs`)
        self.share_sync();
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
                // (a shared server: every checkout it serves behind and idle; never mid-swap)
                let idle = c.roots.iter().all(|r| {
                    let behind = self.sessions.iter().find(|l| Some(l.id) == self.session_of(r));
                    behind.is_some_and(|l| !l.chat.as_ref().is_some_and(crate::chat::Chat::busy))
                });
                (c.id, idle && !in_front(self, c.id) && !self.lsp.share.swapping(c.id))
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

    /// Each session's turn under way counts the time its Claude waits for the user — a card of its, an edit
    /// to review (`calls.rs`). After every event: waiting starts and ends only with one.
    fn waiting_sync(&mut self) {
        let now = crate::usage::now_ms();
        let waits: Vec<(SessionId, bool)> = std::iter::once(&self.session)
            .chain(&self.sessions)
            .map(|l| {
                let busy = l.chat.as_ref().is_some_and(crate::chat::Chat::busy);
                (l.id, busy && self.doing(l) == Doing::Waiting)
            })
            .collect();
        for l in std::iter::once(&mut self.session).chain(&mut self.sessions) {
            if let Some(&(_, on)) = waits.iter().find(|(id, _)| *id == l.id) {
                l.usage.tally.waiting(on, now);
            }
        }
    }

    /// Every event (#94): a call of Claude's waiting on a card whose card is neither up (in front) nor parked
    /// (behind) — lost, not answered — is told so at once (`offer::LOST`) and stops waiting, and the user
    /// hears which. Nothing waits on a card nobody can see: a phantom held every run of a session until its
    /// turn ended.
    pub(crate) fn asking_sync(&mut self) {
        let mut lost: Vec<(SessionId, crate::offer::Asking)> = Vec::new();
        let front = self.session.id;
        let take = |l: &mut Session, up: &[u64], lost: &mut Vec<(SessionId, crate::offer::Asking)>| {
            let here: Vec<u64> = l.parked.iter().map(|o| o.id).chain(up.iter().copied()).collect();
            let (keep, gone): (Vec<_>, Vec<_>) = l.asking.drain(..).partition(|a| here.contains(&a.card));
            l.asking = keep;
            lost.extend(gone.into_iter().map(|a| (l.id, a)));
        };
        let up: Vec<u64> = self.offers.iter().map(|o| o.id).collect();
        take(&mut self.session, &up, &mut lost);
        for l in &mut self.sessions {
            take(l, &[], &mut lost);
        }
        for (id, a) in lost {
            self.card_how(a.card, "lost — its call was told nothing ran");
            let tell = move |ed: &mut Editor| {
                ed.session.log().line(
                    "cards",
                    "lost",
                    &format!("a card was lost with its call waiting: {}", a.what),
                );
                match a.asker {
                    crate::offer::Asker::Tool(call) => {
                        crate::tools::answer(ed, call, Err(crate::offer::LOST.into()))
                    }
                    crate::offer::Asker::Permission { request_id, generation } => {
                        if ed.session.chat.as_ref().is_some_and(|c| c.generation() == generation) {
                            let deny =
                                serde_json::json!({ "behavior": "deny", "message": crate::offer::LOST });
                            crate::chat::respond(ed, &request_id, deny);
                        }
                    }
                }
                // (`set_warning` names a session behind itself)
                ed.set_warning(format!("a card went missing ({}) — Claude was told nothing ran", a.what));
            };
            match id == front {
                true => tell(self),
                false => {
                    self.with_session(id, tell);
                }
            }
        }
    }

    /// `msg` as a toast says it: from a session behind, its name first (as its tab says it).
    pub(crate) fn said_by_session(&self, msg: String) -> String {
        match self.behind {
            true => format!("{} · {msg}", self.session.label()),
            false => msg,
        }
    }

    /// An event's whole work, for session `id` (`Event::For`): `with_session`, and the event draws nothing when
    /// nothing of it is on screen — an idle front draws nothing for a busy session behind.
    pub fn for_session(&mut self, id: SessionId, f: impl FnOnce(&mut Editor)) {
        if self.with_session(id, f) {
            self.unchanged = true;
        }
    }

    /// Applies `f` in session `id` — swapped in for it if it's behind, as it is if it's in front or gone.
    /// Whether nothing of it is on screen (behind, nothing said, no turn ended); `unchanged` isn't touched — it's
    /// the event's, and `f` is often only part of it (#200: a look at a card behind after every event made
    /// every key, resize and job draw nothing for 30 minutes). `for_session` is the form for a whole event.
    pub fn with_session(&mut self, id: SessionId, f: impl FnOnce(&mut Editor)) -> bool {
        let Some(i) = self.sessions.iter().position(|l| l.id == id) else {
            f(self);
            return false;
        };
        let front = self.session.id;
        self.session.current_doc = self.doc().id;
        std::mem::swap(&mut self.session, &mut self.sessions[i]);
        self.current = self.doc_index(self.session.current_doc);
        self.events.set_session(id);
        // (behind as it was after — a swap inside a swap leaves the outer one's session behind still)
        let was = std::mem::replace(&mut self.behind, true);
        let said = self.messages.last().map(|m| m.0);
        let ended_before = self.session.turn_end.as_ref().map(|t| t.0);
        f(self);
        self.behind = was;
        // (a wait that ended while it stayed behind: the next one is news again — #189, it was said once and
        // never again)
        let waits = self.session.waits();
        let tell = waits && !std::mem::replace(&mut self.session.waiting_told, true);
        if !waits {
            self.session.waiting_told = false;
        }
        let ended = self.session.turn_end.as_ref().filter(|t| Some(t.0) != ended_before).map(|t| t.1.clone());
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
        // Its turn ended · it waits for you: said once each, named — and how `C-q` gets there (the waiting
        // one's toast says it when both are so: `C-q` takes a card first)
        if let Some(how) = &ended {
            let (kind, words) = turn_end_words(how);
            let msg = match tell {
                true => format!("{name} · {words}"),
                false => format!("{name} · {words} — {}", self.c_q_says(id, false)),
            };
            self.said_for(id, kind, msg);
        }
        if tell {
            let msg = format!("{name} · waits for you — {}", self.c_q_says(id, true));
            self.said_for(id, crate::editor::ToastKind::Info, msg);
        }
        !tell && ended.is_none() && self.messages.last().map(|m| m.0) == said
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
        session.billing = self.new_session_billing();
        session.views.push(View::new(view, doc));
        (session.current_doc, session.scratch, session.docs) = (doc, Some(doc), vec![doc]);
        self.sessions.push(session);
        self.session_kept_back(id);
        // (its checkout joins its repository's shared language server now — `share.rs`)
        self.share_session(&root);
        if self.git_auto {
            // (its results are its session's — tagged so while the watch is made)
            self.events.set_session(id);
            crate::git::watch(self.events.sender(), root);
            self.events.set_session(self.session.id);
        }
        id
    }

    /// What session `id`'s checkout kept from last time, back in it: Claude's notes, and its turns (read on a
    /// worker — nothing of them is written until they're back, or its first turn would write over them).
    /// Each session's once: session 0's at start (`main.rs` — in front or, after a restart, maybe behind),
    /// the others' as they're made.
    pub fn session_kept_back(&mut self, id: SessionId) {
        crate::notes::restore_in(self, id);
        // (what its Claude may do: built as it's made — a chat restored after `:restart` finds it there)
        self.caps_sync_of(id);
        let root = std::iter::once(&self.session).chain(&self.sessions).find(|l| l.id == id).map(|l| &l.root);
        if let (Some(root), Some(dir)) = (root.cloned(), self.turns_dir.clone()) {
            if let Some(l) = std::iter::once(&mut self.session).chain(&mut self.sessions).find(|l| l.id == id)
            {
                l.checkpoints.restoring = true;
            }
            self.events.set_session(id);
            crate::checkpoint::restore_from(self, dir, root);
            self.events.set_session(self.session.id);
        }
    }

    /// Some session's kept turns are still being read back (`checkpoint::restore_from`): tarae's own memory
    /// isn't judged meanwhile (`Procs::restoring`).
    pub fn restoring(&self) -> bool {
        std::iter::once(&self.session).chain(&self.sessions).any(|l| l.checkpoints.restoring)
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
        // Out: where it was, and its own cards — Claude's, its review's end (they wait for it)
        self.session.current_doc = self.doc().id;
        let (mut mine, rest): (Vec<_>, Vec<_>) =
            std::mem::take(&mut self.offers).into_iter().partition(crate::offer::Offer::goes_with_session);
        self.offers = rest;
        mine.append(&mut self.session.parked);
        self.session.parked = mine;
        // Left with something waiting (its review up, a card of Claude's): said as it goes — Claude waits on
        // it, so nothing else would come to say so (#189)
        let (out, name, waits) = (self.session.id, self.session.label().to_string(), self.session.waits());
        self.session.waiting_told = waits;
        // (the texts of its turns read back go again: their files have them — #218)
        crate::checkpoint::shed(&mut self.session);
        std::mem::swap(&mut self.session, &mut self.sessions[i]);
        if waits {
            let msg = format!("{name} · waits for you — {}", self.c_q_says(out, true));
            self.said_for(out, crate::editor::ToastKind::Info, msg);
        }
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
            v.show(shown);
        }
        self.events.set_session(id);
        // (servers that stopped while it rested: back for its files)
        for d in std::mem::take(&mut self.session.asleep) {
            self.attach_lsp(d);
        }
        self.offers.append(&mut self.session.parked);
        self.session.waiting_told = false;
        self.session.turn_end = None;
        // (what came from disk meanwhile is in sight now — its `•` goes; so does a compaction's mark: its note
        // is in the chat)
        self.session.reloaded = 0;
        self.session.compacted.unseen = false;
        // (its reviewer ended while it was behind: said now — #195)
        crate::reviewer::session_in(self);
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
            Spot::Inbox => self.inbox_open(),
        }
    }

    /// The sessions in the order they were opened: (id, name, in front).
    pub fn sessions_listed(&self) -> Vec<(SessionId, String, bool)> {
        let mut v: Vec<_> = std::iter::once((self.session.id, self.session.name.clone(), true))
            .chain(self.sessions.iter().map(|l| (l.id, l.name.clone(), false)))
            .collect();
        crate::sorting::by_key(&mut v, |l| l.0);
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

    /// #205: a card the user's own input just asked for — through the work it started, as `space W` `n`'s step
    /// brings its card back — comes in front of Claude's card up, which stays, stacked and unanswered; it takes
    /// the keys and is the inbox's first while it's in front. One nobody asked for just now (a grammar for a
    /// file shown, #208) never goes in front of a turn's. And a card of the user's kind that comes late doesn't
    /// take the front from a card that may be read or answered: from no input's work (a task that ended), after
    /// more typing, behind a card that came while its work ran, or from a card being answered.
    #[test]
    fn a_card_your_command_asked_for_comes_in_front_of_claudes() {
        use crate::offer::{Offer, What};
        let (mut ed, front, _) = two_projects("yours-in-front");
        let claudes = || {
            let what = What::Screen { call: crate::tools::Call::test(1) };
            Offer::new("Claude asks".into(), "to see the screen", "", String::new(), what)
        };
        let grammar = || {
            let what = What::Grammars { lang: "Lua".into(), names: Vec::new(), then: None };
            Offer::new("Lua syntax colors".into(), "Download it?", "", String::new(), what)
        };
        let yours = || {
            let what = What::BranchDelete { top: front.clone(), branch: "x".into(), tip: "0".repeat(40) };
            Offer::new("Delete branch x".into(), "It's on no other branch.", "", String::new(), what)
        };
        let titles = |ed: &Editor| ed.offers.iter().map(|o| o.title.clone()).collect::<Vec<_>>();
        // An input, as `handle_event` has one (counted; what it starts asked by it), whose work brings the card
        // back on a worker — `asked`: it lands later, `land`: until it's up. (Stood in for here, to time the
        // late cases; the real wiring — keys through `handle_event`, the jobs and the saver's wait they start —
        // is `restart::tests::restarts_card_typed_while_claudes_is_up_comes_in_front_of_it`.)
        let asked = |ed: &mut Editor, o: Offer| {
            ed.input_seq += 1;
            let was = ed.events.set_asked(Some(ed.input_seq));
            ed.events.jobs().spawn(move || move |ed: &mut Editor| ed.push_offer(o));
            ed.events.set_asked(was);
        };
        let land = |ed: &mut Editor| {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !ed.offers.iter().any(|o| o.title == "Delete branch x") {
                let ev = ed.events.recv_timeout(end.saturating_duration_since(std::time::Instant::now()));
                ed.handle_event(ev.expect("its card before timeout"));
            }
        };
        ed.push_offer(claudes());
        ed.push_offer(grammar());
        asked(&mut ed, yours());
        land(&mut ed);
        assert_eq!(titles(&ed), ["Delete branch x", "Claude asks", "Lua syntax colors"]);
        // The inbox too: yours first while it's in front (`C-q`, `enter` go to it), though Claude's came first
        let first = |ed: &Editor| ed.inbox().first().and_then(|e| e.card());
        let id_of = |ed: &Editor, t: &str| ed.offers.iter().find(|o| o.title == t).map(|o| o.id);
        assert_eq!(first(&ed), id_of(&ed, "Delete branch x"));
        let claudes_id = id_of(&ed, "Claude asks");
        // (drawn and settled — a weighty card's key as it appears isn't an answer)
        crate::term::render(&mut ed, &mut Vec::new(), 120, 30).unwrap();
        let o = &ed.offers[0];
        ed.offer_seen.set(Some((o.id, o.rev, std::time::Instant::now() - crate::offer::SETTLE * 2)));
        feed(&mut ed, "n");
        assert_eq!(titles(&ed), ["Claude asks", "Lua syntax colors"], "yours answered, Claude's still up");
        assert_eq!(first(&ed), claudes_id, "answered: the oldest first again");
        // Came first, but nobody asked for it just now: behind the turn's card
        ed.offers.clear();
        ed.push_offer(grammar());
        ed.push_offer(claudes());
        assert_eq!(titles(&ed), ["Claude asks", "Lua syntax colors"]);
        let late = [
            // From work no input started — a rebase's task that ended, a preflight's run
            ("no input's work", &(|ed: &mut Editor| ed.push_offer(yours())) as &dyn Fn(&mut Editor)),
            // Asked, but more was typed before it came — maybe toward the card up
            ("typed since", &|ed: &mut Editor| {
                asked(ed, yours());
                ed.input_seq += 1;
                land(ed);
            }),
            // Asked, and Claude's card came while its work ran — maybe read now
            ("came meanwhile", &|ed: &mut Editor| {
                ed.offers.clear();
                asked(ed, yours());
                ed.push_offer(claudes());
                land(ed);
            }),
            // Asked, but the card up is being answered — its keys brought to it
            ("being answered", &|ed: &mut Editor| {
                ed.card_keys = true;
                asked(ed, yours());
                land(ed);
            }),
        ];
        for (why, comes) in late {
            ed.offers.retain(|o| o.title == "Claude asks");
            comes(&mut ed);
            assert_eq!(
                titles(&ed),
                ["Claude asks", "Delete branch x"],
                "{why}: Claude's card keeps the front"
            );
            assert_eq!(first(&ed), id_of(&ed, "Claude asks"), "{why}: and the inbox's first");
            ed.card_keys = false;
        }
    }

    /// #7: the card up says how many wait behind it — stacked, queued in its chat, in sessions behind — and
    /// `C-q` gives the keys to the one up here (#198: this session's own first, though the one behind came
    /// first), then goes to the session behind's, its card up there; once it's the card up, the keys.
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
        // In front: one card up, one under it — and the one behind, counted apart (#91)
        ed.push_offer(screen_card(2));
        ed.push_offer(screen_card(3));
        assert_eq!(ed.cards_waiting(), (2, 1));
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 120, 30).unwrap();
        let mut parser = vt100::Parser::new(30, 120, 0);
        parser.process(&bytes);
        let s = parser.screen().contents();
        assert!(s.contains("Claude asks 2") && s.contains("1/2 here · 1 elsewhere"), "{s}");
        // C-q with cards up here — back's came first, but this session's own go first (#198): the keys
        feed(&mut ed, "<C-q>");
        assert!(ed.card_keys && ed.session.id == 0, "this session's own first");
        ed.offers.clear();
        ed.card_keys = false;
        // None up here: C-q goes to back's, its card up there
        feed(&mut ed, "<C-q>");
        assert_eq!(ed.session.id, back_id, "then the one that waited longest");
        assert_eq!(ed.offers.first().map(|o| o.title.as_str()), Some("Claude asks 1"));
        // C-q again: the keys
        feed(&mut ed, "<C-q>");
        assert!(ed.card_keys && ed.session.id == back_id);
        // Nothing waits anywhere: said
        ed.offers.clear();
        ed.card_keys = false;
        feed(&mut ed, "<C-q>");
        assert_eq!(ed.session.id, back_id);
        assert!(ed.status.as_ref().is_some_and(|s| s.0.contains("no card waits")), "{:?}", ed.status);
    }

    /// #90: `C-q` goes to the session whose card has waited longest — not the first in the list — and the
    /// card up there names its session; #91: its count keeps here and elsewhere apart.
    #[test]
    fn c_q_goes_to_the_card_waiting_longest_and_names_its_session() {
        use crate::offer::{Offer, What};
        let (mut ed, front, back) = two_projects("longest");
        let third = front.with_file_name("third");
        std::fs::create_dir_all(&third).unwrap();
        let card = |n: u64, ago: u64| {
            let what = What::Screen { call: crate::tools::Call::test(n) };
            let mut o = Offer::new(format!("Claude asks {n}"), "to see the screen", "", String::new(), what);
            o.since = Instant::now() - Duration::from_secs(ago);
            o
        };
        let back_id = ed.session_open(&back).unwrap();
        let third_id = ed.session_open(&third).unwrap();
        assert!(ed.session_switch(0));
        // `back` is first in the list, but `third`'s card came earlier
        session_by_mut(&mut ed, back_id).parked.push(card(1, 5));
        session_by_mut(&mut ed, third_id).parked.push(card(2, 60));
        session_by_mut(&mut ed, third_id).parked.push(card(3, 1));
        assert_eq!(ed.cards_waiting(), (0, 3));
        feed(&mut ed, "<C-q>");
        assert_eq!(ed.session.id, third_id, "the one that waited longest");
        assert_eq!(ed.cards_waiting(), (2, 1), "its two here, back's elsewhere");
        let rows = screen(&mut ed, 140, 30);
        let head = rows.iter().find(|r| r.contains("Claude asks 2")).expect("its card");
        assert!(head.contains("third · Claude asks 2"), "named: {head}");
        assert!(head.contains("1/2 here · 1 elsewhere"), "{head}");
        // Answered there: the next C-q goes to back
        ed.offers.clear();
        feed(&mut ed, "<C-q>");
        assert_eq!(ed.session.id, back_id);
        let rows = screen(&mut ed, 140, 30);
        assert!(
            rows.iter().any(|r| r.contains("back · Claude asks 1") && !r.contains("elsewhere")),
            "{rows:#?}"
        );
        std::fs::remove_dir_all(front.parent().unwrap()).ok();
    }

    /// A message typed in a chat whose input isn't on screen is marked: a session behind's on its tab, the
    /// front's on the status line while its chat is minimized (Claude at work, followed) — and not once the
    /// chat is up, or the input empty.
    #[test]
    fn a_draft_out_of_sight_is_marked_unsent() {
        let (mut ed, _front, back) = two_projects("unsent");
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        let id = ed.session_open(&back).unwrap();
        crate::chat::open(&mut ed);
        ed.session.chat.as_mut().unwrap().input = "and also the tests".into();
        assert!(ed.session_switch(0));
        crate::chat::open(&mut ed);
        let rows = screen(&mut ed, 160, 24);
        let tabs = rows.iter().find(|r| r.contains(" │ +")).cloned().unwrap_or_else(|| panic!("{rows:#?}"));
        assert!(tabs.contains("back unsent │ +") && !tabs.contains("front unsent"), "{tabs}");
        session_by_mut(&mut ed, id).chat.as_mut().unwrap().input.clear();
        let rows = screen(&mut ed, 160, 24);
        assert!(!rows.iter().any(|r| r.contains("unsent")), "{rows:#?}");
        // The front's chat minimized while Claude works: its draft on the status line
        let c = ed.session.chat.as_mut().unwrap();
        (c.input, c.focused, c.follow.on, c.state) =
            ("half a thought\nmore".into(), false, true, crate::chat::State::Thinking);
        let rows = screen(&mut ed, 160, 24);
        let status = rows.iter().find(|r| r.contains("NORMAL")).unwrap_or_else(|| panic!("{rows:#?}"));
        assert!(status.contains("unsent half a thought"), "{status}");
        ed.session.chat.as_mut().unwrap().unfolded = true;
        let rows = screen(&mut ed, 160, 24);
        assert!(!rows.iter().any(|r| r.contains("unsent")), "the chat up: its input says it");
        ed.session.chat.as_mut().unwrap().state = crate::chat::State::Idle;
        std::fs::remove_dir_all(back.parent().unwrap()).ok();
    }

    /// #86: a session behind whose turn ends — its card that waited goes with the turn (the front's stays),
    /// and its tab says `done`, not the `▲ waits` of a card; looked at, the mark goes.
    #[test]
    fn a_turn_ended_behind_says_done_not_waits() {
        use crate::offer::{Offer, What};
        let (mut ed, front, back) = two_projects("done");
        let log = back.join("claude-heard.log");
        claude_calls(&mut ed, &log, &[]);
        let screen_card = |n: u64| {
            let what = What::Screen { call: crate::tools::Call::test(n) };
            Offer::new(format!("Claude asks {n}"), "to see the screen", "", String::new(), what)
        };
        let (id, _) = back_at_work(&mut ed, &back);
        // A card of back's waits behind; one of front's is up
        session_by_mut(&mut ed, id).parked.push(screen_card(1));
        ed.push_offer(screen_card(2));
        assert_eq!(ed.doing(session_by(&ed, id)), Doing::Waiting);
        feed(&mut ed, "<space>l");
        let end = Instant::now() + Duration::from_secs(10);
        while session_by(&ed, id).chat.as_ref().unwrap().busy() {
            assert!(Instant::now() < end, "back's turn ends");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        assert!(session_by(&ed, id).parked.is_empty(), "its card went with its turn");
        assert_eq!(ed.offers.len(), 1, "the front's card stays");
        assert_eq!(ed.doing(session_by(&ed, id)), Doing::Done);
        assert_eq!((ed.sessions_waiting(), ed.sessions_done()), (0, 1));
        // (front's own card up: front waits; back is done)
        let tabs = |ed: &mut Editor| {
            let rows = screen(ed, 160, 24);
            rows.iter().find(|r| r.contains(" │ +")).cloned().unwrap_or_else(|| panic!("{rows:#?}"))
        };
        let row = tabs(&mut ed);
        assert!(row.contains("front ▲ waits │ back done │ +"), "{row}");
        ed.offers.clear();
        let row = tabs(&mut ed);
        assert!(row.contains("back done") && !row.contains('▲'), "{row}");
        // A card waiting says so apart
        session_by_mut(&mut ed, id).parked.push(screen_card(3));
        let row = tabs(&mut ed);
        assert!(row.contains("back ▲ waits"), "{row}");
        session_by_mut(&mut ed, id).parked.clear();
        // The status line's chip with no chat up counts them apart
        ed.session.chat = None;
        let rows = screen(&mut ed, 160, 24);
        let status = rows.iter().find(|r| r.contains("NORMAL")).unwrap_or_else(|| panic!("{rows:#?}"));
        assert!(status.contains("front 1 done") && !status.contains('▲'), "{status}");
        // Looked at: no longer done
        assert!(ed.session_switch(id));
        assert!(ed.session_switch(0));
        assert_eq!(ed.doing(session_by(&ed, id)), Doing::Idle);
        std::fs::remove_dir_all(front.parent().unwrap()).ok();
    }

    /// #11 (frictions3): a run card up in front, its call pending — `C-q`, a `y` too soon (refused), Esc —
    /// and meanwhile another session's turn ends behind. The card stays up, `C-q` finds it, and the call is
    /// still the card's to answer (it was gone from the screen, the call pending for minutes, `C-q` saying
    /// no card waits: the session behind's turn end had swept the front's run cards). #94: a Read outside
    /// the project asked while the run card is up waits under it — and is the card up once the run's is
    /// answered (it was swept too: its call pending, no card, `C-q` saying none waits).
    #[test]
    fn a_pending_calls_card_stays_when_another_session_ends_its_turn() {
        let (mut ed, front, back) = two_projects("pending-card");
        // (today's gate, for the run's card: in the sandbox it runs — P3)
        ed.config.llm.sandbox = false;
        let made = front.join("made");
        let log = front.join("claude-heard.log");
        // Front's Claude: a run that asks, and with it a Read outside the project (back's file)
        let run = json!({"type": "control_request", "request_id": "q-run", "request": {
            "subtype": "mcp_message", "server_name": "tarae", "message": {"jsonrpc": "2.0", "id": 1,
            "method": "tools/call", "params": {"name": "run",
            "arguments": {"command": format!("touch {}", made.display())}}}}});
        let read = json!({"type": "control_request", "request_id": "q-Read", "request": {
            "subtype": "can_use_tool", "tool_name": "Read",
            "input": {"file_path": back.join("main.rs").display().to_string()}}});
        let result = json!({"type": "result", "result": "done"});
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac; \
                 printf '%s\\n' '{run}'; printf '%s\\n' '{read}'; n=0; while read -r l; do \
                 printf '%s\\n' \"$l\" >> '{log}'; case \"$l\" in *'\"q-run\"'*|*'\"q-Read\"'*) n=$((n+1));; esac; \
                 [ $n -ge 2 ] && break; done; printf '%s\\n' '{result}'; done",
                log = log.display()
            ),
        ];
        feed(&mut ed, "<space>lmake it<ret>");
        let is = |ed: &Editor, i: usize, read: bool| {
            ed.offers.get(i).is_some_and(|o| match read {
                true => matches!(o.what, crate::offer::What::Read { .. }),
                false => matches!(o.what, crate::offer::What::Run(_)),
            })
        };
        let end = Instant::now() + Duration::from_secs(10);
        while !(is(&ed, 0, false) && is(&ed, 1, true)) {
            assert!(Instant::now() < end, "front's run card, and the Read's under it");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        let is_run = |ed: &Editor| is(ed, 0, false);
        // C-q, a `y` before it settled (swallowed), Esc: the keys back, the card still up
        feed(&mut ed, "<C-q>y<esc>");
        assert!(is_run(&ed) && !ed.card_keys, "Esc gives the keys back, never drops the card");
        // Another session's Claude ends a turn behind
        let back_log = back.join("claude-heard.log");
        claude_calls(&mut ed, &back_log, &[]);
        let (id, _) = back_at_work(&mut ed, &back);
        let end = Instant::now() + Duration::from_secs(10);
        while session_by(&ed, id).chat.as_ref().unwrap().busy() {
            assert!(Instant::now() < end, "back's turn ends");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        assert!(is_run(&ed), "front's card is still up");
        assert!(is(&ed, 1, true), "and the Read's under it");
        assert!(answered(&log, "run").is_none(), "its call still waits for the card");
        feed(&mut ed, "<C-q>");
        assert!(ed.card_keys, "C-q finds it");
        // …and it's answered from there
        std::thread::sleep(crate::offer::SETTLE + Duration::from_millis(50));
        ed.offer_accept();
        let end = Instant::now() + Duration::from_secs(10);
        while answered(&log, "run").is_none() {
            assert!(Instant::now() < end, "Claude hears how it ran");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        assert!(made.exists());
        // The run's card answered: the Read's is the one up, `C-q` finds it, and `y` lets it read
        assert!(
            is(&ed, 0, true),
            "the Read's card up: {:?}",
            ed.offers.iter().map(|o| &o.title).collect::<Vec<_>>()
        );
        assert!(answered(&log, "Read").is_none());
        ed.card_keys = false;
        feed(&mut ed, "<C-q>");
        assert!(ed.card_keys, "C-q finds it");
        ed.offer_accept();
        let end = Instant::now() + Duration::from_secs(10);
        while answered(&log, "Read").is_none() {
            assert!(Instant::now() < end, "Claude hears the Read's answer");
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(answered(&log, "Read").is_some_and(|a| a.contains("allow")), "{:?}", answered(&log, "Read"));
        std::fs::remove_dir_all(front.parent().unwrap()).ok();
    }

    /// #94 (ra-swap, as met): a run card waits in a session behind; `C-q` goes there, its card up; `C-q`
    /// gives it the keys (CARD); `C-q` again gives them back — the card stays up — and `C-q` brings them to
    /// it again. Meanwhile the other session's turn ends behind: the card stays up with the keys (it had
    /// vanished for good, `C-q` saying no card waits, the run pending until its turn was stopped), and `y`
    /// answers it.
    #[test]
    fn c_q_on_a_card_with_the_keys_never_drops_it() {
        let (mut ed, front, back) = two_projects("c-q-twice");
        // (today's gate, for the runs' cards: in the sandbox they run — P3)
        ed.config.llm.sandbox = false;
        let (front_log, back_log) = (front.join("claude-heard.log"), back.join("claude-heard.log"));
        let pump = |ed: &mut Editor, done: &dyn Fn(&Editor) -> bool, what: &str| {
            let end = Instant::now() + Duration::from_secs(10);
            while !done(ed) {
                assert!(Instant::now() < end, "{what}");
                if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                    ed.handle_event(ev);
                }
            }
        };
        let runs = |cards: &[crate::offer::Offer]| {
            cards.iter().any(|o| matches!(o.what, crate::offer::What::Run(_)))
        };
        // Back's Claude asks to run, behind
        let made = back.join("made");
        claude_calls(
            &mut ed,
            &back_log,
            &[("run", json!({ "command": format!("touch {}", made.display()) }))],
        );
        let (id, _) = back_at_work(&mut ed, &back);
        pump(&mut ed, &|ed| runs(&session_by(ed, id).parked), "back's card, parked");
        // Front's Claude at work (waiting on its server), to end its turn later
        claude_calls(&mut ed, &front_log, &[("symbols", json!({ "path": "main.rs" }))]);
        feed(&mut ed, "<space>lgo<ret><esc>");
        pump(&mut ed, &|ed| !ed.session.tool_waits.is_empty(), "front's Claude asked its server");
        // C-q: to back, its card up · C-q: the keys · C-q: back · C-q: the keys again
        feed(&mut ed, "<C-q>");
        assert!(ed.session.id == id && runs(&ed.offers), "C-q goes to the card");
        feed(&mut ed, "<C-q>");
        assert!(ed.card_keys, "the card has the keys");
        assert_eq!(ed.keys_at().label(), Some("CARD"));
        feed(&mut ed, "<C-q>");
        assert!(!ed.card_keys && runs(&ed.offers), "keys back, the card still up");
        feed(&mut ed, "<C-q>");
        assert!(ed.card_keys && runs(&ed.offers), "C-q brings them to it again");
        // Front's turn ends behind: the card stays, with the keys
        let end = Instant::now() + Duration::from_secs(10);
        while session_by(&ed, 0).chat.as_ref().unwrap().busy() {
            assert!(Instant::now() < end, "front's turn ends");
            serve(&mut ed, &front.join("main.rs"));
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        assert!(runs(&ed.offers) && ed.card_keys, "still up, still with the keys");
        // …and it's answered
        let mut out = Vec::new();
        crate::term::render(&mut ed, &mut out, 140, 30).unwrap();
        std::thread::sleep(crate::offer::SETTLE + Duration::from_millis(50));
        crate::term::render(&mut ed, &mut out, 140, 30).unwrap();
        feed(&mut ed, "y");
        pump(&mut ed, &|_| answered(&back_log, "run").is_some(), "back's Claude hears how it ran");
        assert!(made.exists());
        std::fs::remove_dir_all(front.parent().unwrap()).ok();
    }

    /// #94 (ra-swap): a session behind asking to run while the front has a run card up isn't refused ("the
    /// user hasn't answered your last command" — the front's card, which its own screen never showed): it
    /// gets a card of its own, parked, that `C-q` reaches. And a card lost by some fault never leaves its
    /// call waiting unseen: the call is told nothing ran, and the user hears which card it was.
    #[test]
    fn no_call_waits_on_a_card_nobody_can_see() {
        let (mut ed, front, back) = two_projects("phantom");
        // (today's gate, for the run's card: in the sandbox it runs — P3)
        ed.config.llm.sandbox = false;
        let (front_log, back_log) = (front.join("claude-heard.log"), back.join("claude-heard.log"));
        let run = |dir: &Path| json!({ "command": format!("touch {}", dir.join("made").display()) });
        claude_calls(&mut ed, &front_log, &[("run", run(&front))]);
        feed(&mut ed, "<space>lfront<ret><esc>");
        let pump = |ed: &mut Editor, done: &dyn Fn(&Editor) -> bool, what: &str| {
            let end = Instant::now() + Duration::from_secs(10);
            while !done(ed) {
                assert!(Instant::now() < end, "{what}");
                if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                    ed.handle_event(ev);
                }
            }
        };
        let runs = |cards: &[crate::offer::Offer]| {
            cards.iter().any(|o| matches!(o.what, crate::offer::What::Run(_)))
        };
        pump(&mut ed, &|ed| runs(&ed.offers), "front's run card");
        // Back's Claude asks to run too, behind, while front's card is up: its own card, parked
        claude_calls(&mut ed, &back_log, &[("run", run(&back))]);
        let (id, _) = back_at_work(&mut ed, &back);
        pump(&mut ed, &|ed| runs(&session_by(ed, id).parked), "back's own card, parked");
        assert!(answered(&back_log, "run").is_none(), "not refused: {:?}", answered(&back_log, "run"));
        // Front's answered (no): C-q goes to back's, up there
        std::thread::sleep(crate::offer::SETTLE);
        ed.offer_dismiss();
        assert!(!runs(&ed.offers));
        feed(&mut ed, "<C-q>");
        assert_eq!(ed.session.id, id, "C-q reaches it");
        assert!(runs(&ed.offers), "its card up");
        // Lost by a fault (gone without an answer): the call is told, the user hears it
        ed.offers.clear();
        ed.asking_sync();
        pump(&mut ed, &|_| answered(&back_log, "run").is_some(), "back's call told");
        let told = answered(&back_log, "run").unwrap();
        assert!(told.contains("was lost before they could answer") && told.contains("isError"), "{told}");
        assert!(ed.session.asking.is_empty(), "nothing waits any more");
        let said = ed.messages.iter().map(|m| m.2.clone()).collect::<Vec<_>>().join(" | ");
        assert!(said.contains("a card went missing (Claude wants to run)"), "{said}");
        assert!(!back.join("made").exists(), "nothing ran");
        // …the same behind: its parked card lost, told in its session, named
        let front_log = front.join("claude-heard-again.log");
        claude_calls(&mut ed, &front_log, &[("run", run(&front))]);
        assert!(ed.session_switch(0));
        ed.session.chat = None;
        feed(&mut ed, "<space>lagain<ret><esc>");
        pump(&mut ed, &|ed| runs(&ed.offers), "front's card again");
        let front_id = ed.session.id;
        assert!(ed.session_switch(id), "front goes behind, its card parked");
        session_by_mut(&mut ed, front_id).parked.clear();
        ed.asking_sync();
        pump(
            &mut ed,
            &|_| answered(&front_log, "run").is_some_and(|a| a.contains("was lost")),
            "front's call told",
        );
        let said = ed.messages.iter().map(|m| m.2.clone()).collect::<Vec<_>>().join(" | ");
        assert!(said.contains("front · a card went missing"), "named: {said}");
        std::fs::remove_dir_all(front.parent().unwrap()).ok();
    }

    /// #159: a review's end card is its session's — it names the session and the review, goes behind with
    /// its session, and `y` ends only that review: not another session's after a trip through the inbox, not
    /// a review begun since in its place.
    #[test]
    fn a_review_end_card_ends_only_its_own_review() {
        use crate::codereview::{Base, CodeReview, File};
        let (mut ed, front, back) = two_projects("review-end");
        let review = |root: &Path| {
            let f = |rel: &str| File {
                rel: rel.into(),
                path: root.join(rel),
                added: 1,
                removed: 0,
                new: false,
                gone: false,
                binary: false,
            };
            CodeReview::for_test(Base::Head, root.to_path_buf(), vec![f("a.rs"), f("b.rs"), f("c.rs")])
        };
        ed.session.code_review = Some(review(&front));
        let back_id = ed.session_open(&back).unwrap();
        ed.session.code_review = Some(review(&back));
        assert!(ed.session_switch(0));
        let name = ed.session.label().to_string();
        feed(&mut ed, "<space>vq");
        let o = ed.offers.first().expect("the end card");
        let card = format!("card#{}", o.id);
        assert_eq!(o.title, format!("End {name}'s review?"));
        let head =
            format!("{name}'s review · uncommitted · 0 of 3 viewed\n\n3 files not viewed — end it anyway?");
        assert!(o.question.starts_with(&head), "{}", o.question);
        // To back: front's card goes behind with front; back's own end card is its own, not merged into it
        assert!(ed.session_switch(back_id));
        assert!(ed.offers.is_empty(), "front's card isn't up in back: {:?}", ed.offers);
        feed(&mut ed, "<space>vq");
        assert_eq!(ed.offers.len(), 1);
        assert_eq!(ed.offers[0].title, format!("End {}'s review?", ed.session.label()));
        // Back to front's card through the inbox, `y`: front's review ends, back's stays
        ed.inbox_go(&card);
        assert_eq!(ed.session.id, 0);
        feed(&mut ed, "y");
        assert!(ed.session.code_review.is_none(), "front's ended");
        assert!(session_by(&ed, back_id).code_review.is_some(), "back's untouched");
        // Back's card, its review ended and another begun meanwhile: `y` ends nothing
        assert!(ed.session_switch(back_id));
        let r = ed.session.code_review.as_mut().unwrap();
        r.started += Duration::from_millis(1);
        feed(&mut ed, "<C-q>y");
        assert!(ed.offers.is_empty(), "answered");
        assert!(ed.session.code_review.is_some(), "the review begun since stays");
        std::fs::remove_dir_all(front.parent().unwrap()).ok();
    }

    /// #200: `with_session` says whether nothing of it is on screen and leaves the event's `unchanged` alone —
    /// only `for_session` (an event's whole work, `Event::For`) sets it from that. Saying something behind is
    /// on screen; the front or a gone session is applied as is, never "nothing".
    #[test]
    fn with_session_says_what_it_showed_and_leaves_the_event_alone() {
        let mut ed = Editor::new(crate::config::Config::default());
        let back = ed.session_add(std::env::temp_dir());
        ed.unchanged = false;
        assert!(ed.with_session(back, |_| {}), "nothing of it on screen");
        assert!(!ed.unchanged, "the event's own flag untouched");
        assert!(!ed.with_session(back, |ed| ed.set_status("from behind")), "said: on screen");
        assert!(!ed.with_session(ed.session.id, |_| {}), "the front's");
        assert!(!ed.with_session(99, |_| {}), "a gone one: applied in front");
        assert!(!ed.unchanged);
        ed.for_session(back, |_| {});
        assert!(ed.unchanged, "an event that only did that draws nothing");
    }

    /// An idle front draws nothing for a busy session behind: a tool call of its Claude's answered in an event
    /// of its own — a job's result or a server's reply (`Event::For`), or the hop an answer made inside another
    /// session's swap takes (`tools::deliver`) — goes to its chat behind, and that event, through every step
    /// after it, changed nothing: no frame.
    #[test]
    fn a_tool_answer_behind_draws_no_frame_in_front() {
        use crate::event::Event;
        let mut ed = Editor::new(crate::config::Config::default());
        (ed.config.llm.command, ed.config.llm.args) =
            ("sh".into(), vec!["-c".into(), "cat > /dev/null".into()]);
        let back = ed.session_add(std::env::temp_dir());
        let other = ed.session_add(std::env::temp_dir());
        let mut generation = 0;
        ed.with_session(back, |ed| {
            crate::chat::open(ed);
            let c = ed.session.chat.as_mut().unwrap();
            (c.state, c.focused) = (crate::chat::State::Thinking, false);
            generation = c.generation();
            for id in [1, 2] {
                let (id, tool, since, after) = (json!(id), "symbols", Instant::now(), STUCK_AFTER);
                ed.session.tool_waits.push(ToolWait { generation, id, tool, since, after });
            }
        });
        crate::term::render(&mut ed, &mut Vec::new(), 100, 30).unwrap();
        let waits = |ed: &Editor| session_by(ed, back).tool_waits.len();
        let quiet = |ed: &mut Editor, ev: Event| {
            ed.unchanged = false;
            ed.handle_event(ev);
            let unchanged = std::mem::take(&mut ed.unchanged);
            unchanged && !ed.frame_due(unchanged)
        };
        // Its own event
        let call = crate::tools::Call::of(generation, "r1", json!(1));
        let answer = move |ed: &mut Editor| crate::tools::answer(ed, call, Ok("one".into()));
        assert!(quiet(&mut ed, Event::For(back, Box::new(answer))), "its own event: no frame");
        assert_eq!(waits(&ed), 1, "answered behind");
        // Made inside another session's swap: delivered by the hop after it
        let call = crate::tools::Call::of(generation, "r2", json!(2));
        ed.with_session(other, |ed| crate::tools::answer(ed, call, Ok("two".into())));
        assert_eq!(waits(&ed), 1, "not inside that swap");
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let ev = ed.events.recv_timeout(left).expect("the hop");
            let quiet = quiet(&mut ed, ev);
            if waits(&ed) == 0 {
                assert!(quiet, "the hop: no frame");
                break;
            }
        }
    }

    /// A pane of a session behind, its document closed from another meanwhile, shows the next one from its
    /// first row: the closed one's scroll isn't the next one's (kept, it was drawn from line 3's sixth row). An
    /// unfocused pane: the focused one takes the document's scroll.
    #[test]
    fn a_pane_whose_document_closed_behind_shows_the_next_from_its_top() {
        let scratch = crate::scratch::Scratch::new("sessions-closed-behind");
        let dir = scratch.to_path_buf();
        for p in ["front", "back"] {
            std::fs::create_dir_all(dir.join(p)).unwrap();
        }
        let dir = std::fs::canonicalize(dir).unwrap();
        let mut ed = Editor::new(crate::config::Config::default());
        (ed.session.root, ed.session.name) = (dir.join("front"), "front".into());
        let back = ed.session_open(&dir.join("back")).unwrap();
        assert_eq!(ed.session.id, back);
        // (in `back`: two panes, the unfocused one on a document of its own scrolled into its line 3)
        let doc = crate::document::Document::from_str(ed.alloc_id(), &"word ".repeat(400).repeat(5));
        let gone = doc.id;
        ed.docs.push(doc);
        ed.session.docs.push(gone);
        let left = ed.session.focus;
        ed.split_view(crate::split::Dir::Vertical);
        let v = ed.session.views.iter_mut().find(|v| v.id == left).unwrap();
        (v.doc, v.top, v.left, v.top_rows) = (gone, 3, 2, Some((3, 5)));
        assert!(ed.session_switch(0));
        ed.close_doc(gone);
        assert!(ed.session_switch(back));
        let v = ed.session.views.iter().find(|v| v.id == left).unwrap();
        assert_ne!(v.doc, gone);
        assert_eq!((v.top, v.left, v.top_rows), (0, 0, None));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Two checkouts, `front` and `back`, each a cargo project with a `main.rs` — and an editor whose rust
    /// server is a stand-in (`cat`: the test answers for it, as its reader thread would, in whichever session
    /// is in front).
    fn two_projects(name: &str) -> (Editor, PathBuf, PathBuf) {
        let dir = crate::scratch::Scratch::new(&format!("sessions-{name}")).into_path();
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
            reloaded: 0,
            compacted: false,
            reviewer: String::new(),
            claude: None,
            billing: Default::default(),
            build: None,
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
        // (its turn ended behind: done, not looked at yet — #86)
        assert_eq!((ed.doing(session_by(&ed, id)), ed.sessions_stuck()), (Doing::Done, 0));
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

    /// A fake claude that ends its `n`th turn with `results[n]` (the last one from then on).
    fn claude_ends(ed: &mut Editor, results: &[Value]) {
        let mut cases = String::new();
        for (i, r) in results.iter().enumerate() {
            let n = if i + 1 == results.len() { "*".to_string() } else { (i + 1).to_string() };
            cases.push_str(&format!("{n}) printf '%s\\n' '{r}';; "));
        }
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "n=0; while read -r line; do case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac; \
                 n=$((n+1)); case $n in {cases}esac; done"
            ),
        ];
    }

    /// Events handled until session `id`'s Claude is idle again.
    fn turn_ends(ed: &mut Editor, id: SessionId) {
        let end = Instant::now() + Duration::from_secs(10);
        while session_by(ed, id).chat.as_ref().unwrap().busy() {
            assert!(Instant::now() < end, "its turn ends");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
    }

    /// A session behind ends its turn: a toast says so once, named, with how `C-q` gets there — and `C-q`
    /// goes there. A failed turn says its error's first line, a warning; a stopped one says so, and with a card
    /// that came first (the front's), `C-q` goes there after it. The toast is that session's news: the
    /// front's Claude doesn't see it (other sessions are counts at its level).
    #[test]
    fn a_turn_ended_behind_is_said_once_with_its_name() {
        use crate::editor::ToastKind;
        use crate::offer::{Offer, What};
        let (mut ed, front, back) = two_projects("ended");
        let ok = json!({"type": "result", "result": "done"});
        let failed = json!({"type": "result", "is_error": true, "result": "API Error: overloaded\nlater"});
        let stopped = json!({"type": "result", "subtype": "error_during_execution", "result": ""});
        claude_ends(&mut ed, &[ok, failed, stopped]);
        let (id, _) = back_at_work(&mut ed, &back);
        turn_ends(&mut ed, id);
        let said = |ed: &Editor, text: &str| ed.toasts.iter().filter(|t| t.text == text).count();
        let msg = "back · turn ended — C-q goes there";
        assert_eq!(said(&ed, msg), 1, "{:?}", ed.toasts);
        assert_eq!(ed.toasts.iter().find(|t| t.text == msg).map(|t| t.of), Some(Some(id)));
        // Once: more of it applied behind says nothing again
        ed.with_session(id, |_| {});
        assert_eq!(said(&ed, msg), 1);
        // The user's state has it; the front's Claude's doesn't
        assert!(ed.ui_state().toasts.iter().any(|t| t.text == msg));
        assert!(ed.ui_state_claude().toasts.iter().all(|t| !t.text.contains("back ·")));
        // C-q: there
        feed(&mut ed, "<C-q>");
        assert_eq!(ed.session.id, id);
        // A failed turn: its error's first line, a warning
        feed(&mut ed, "<space>lagain<ret><esc>");
        assert!(ed.session_switch(0));
        turn_ends(&mut ed, id);
        let warn = ed.toasts.iter().find(|t| t.text.starts_with("back · turn ended: ")).expect("said");
        assert_eq!(
            (warn.text.as_str(), warn.kind),
            ("back · turn ended: API Error: overloaded — C-q goes there", ToastKind::Warning)
        );
        // A stopped one, a card of the front's up first: `C-q` goes there after it
        assert!(ed.session_switch(id));
        feed(&mut ed, "<space>lonce more<ret><esc>");
        assert!(ed.session_switch(0));
        let what = What::Screen { call: crate::tools::Call::test(1) };
        ed.push_offer(Offer::new("Claude asks".into(), "to see the screen", "", String::new(), what));
        turn_ends(&mut ed, id);
        assert_eq!(said(&ed, "back · turn stopped — C-q goes there after 1 older"), 1, "{:?}", ed.toasts);
        std::fs::remove_dir_all(front.parent().unwrap()).ok();
    }

    /// The front session's turn ends with its chat out of sight past the turn — the code zoomed (`C-w z`):
    /// said, with where it's seen. In sight, or minimized only by follow mode while Claude worked (it's back
    /// with the answer): nothing.
    #[test]
    fn the_fronts_turn_ended_out_of_sight_is_said() {
        let (mut ed, front, _) = two_projects("front-ended");
        claude_ends(&mut ed, &[json!({"type": "result", "result": "done"})]);
        let said = |ed: &Editor| ed.toasts.iter().filter(|t| t.text.starts_with("Claude's turn")).count();
        feed(&mut ed, "<space>l<esc>");
        ed.session.chat.as_mut().unwrap().follow.on = false;
        // In sight
        feed(&mut ed, "<space>lone<ret><esc>");
        screen(&mut ed, 160, 30);
        assert!(ed.screen.chat_x.is_some());
        turn_ends(&mut ed, 0);
        assert_eq!(said(&ed), 0, "{:?}", ed.toasts);
        // Minimized by follow mode while Claude works: nothing
        ed.session.chat.as_mut().unwrap().follow.on = true;
        feed(&mut ed, "<space>ltwo<ret><esc>");
        screen(&mut ed, 160, 30);
        assert!(ed.screen.chat_x.is_none(), "minimized while it works");
        turn_ends(&mut ed, 0);
        assert_eq!(said(&ed), 0, "{:?}", ed.toasts);
        // The code zoomed: said
        ed.session.chat.as_mut().unwrap().follow.on = false;
        feed(&mut ed, "<space>lthree<ret><esc>");
        screen(&mut ed, 160, 30);
        feed(&mut ed, "<C-w>z");
        screen(&mut ed, 160, 30);
        assert!(ed.zoomed().is_some() && ed.screen.chat_x.is_none());
        turn_ends(&mut ed, 0);
        let t = ed.toasts.iter().find(|t| t.text.starts_with("Claude's turn")).map(|t| t.text.as_str());
        assert_eq!(t, Some("Claude's turn ended — space l shows it"), "{:?}", ed.toasts);
        std::fs::remove_dir_all(front.parent().unwrap()).ok();
    }
}
