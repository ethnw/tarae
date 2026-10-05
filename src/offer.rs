//! Offers to download something (like IntelliJ's "install plugin" notice) — a card at the bottom right of
//! the editing area, `y` download · `n` not now (normal mode, mouse too — insert mode is not intercepted).
//! While downloading it shows a wave; on failure, the reason + `y` retry · `n` close.
//! What gets downloaded is `What` — grammars (grammar.rs) · the java-debug plugin (java.rs) · a tool a
//! language needs, installed the way its pack says (install.rs; `N` never offers that one again).
//! Drawn by `term::OfferCard`.
//!
//! **Multiple offers stack** (`Editor::offers`, arrival order): only the front one takes keys; the ones
//! behind overlap one line each above the front card, showing only their title — so none is missed.
//! New offers go to the back (so a `y` never lands on the wrong card if the front one changes
//! mid-answer). A card whose download started with `y` moves to the back and shows the wave while
//! downloading; the next one comes forward.
//! On failure it stays in place and asks again when its turn comes. Clicking a back card brings it forward.

use std::path::PathBuf;
use std::time::Instant;

#[derive(Clone, Debug)]
pub struct Offer {
    /// Id used to find this card when the download finishes.
    pub id: u64,
    /// Bold first line ("Rust syntax colors").
    pub title: String,
    /// Prompt text ("Download and build the tree-sitter grammar?").
    pub question: String,
    /// Text while downloading ("Fetching and building…").
    pub working: String,
    /// Failure title ("Couldn't build the Rust grammar").
    pub failed: String,
    pub what: What,
    pub state: OfferState,
    /// Bumped whenever what it says (and does) changes: a `y` answers the card as it's been in sight.
    pub rev: u64,
    /// The question's lines (split at `\n`) that say what's being decided — drawn at full strength, the rest
    /// faint. None: a weighty card's first paragraph (a light one's: none).
    pub decided: Option<std::ops::Range<usize>>,
    /// The question's lines that warn what saying yes means — drawn in the warning color (the run-outside
    /// card's "with all your permissions").
    pub warning: Option<std::ops::Range<usize>>,
    /// The question's lines that are a diff (`sinceyes.rs`): `+` and `-` lines in the diff's colours, `@@`
    /// faint.
    pub diff: Option<std::ops::Range<usize>>,
    /// When it came (`push_offer`) — `C-q` goes to the session whose card has waited longest (#90).
    pub since: Instant,
    /// When it was first drawn in front (`term::render_frame`) — its `card` line says how long it was up
    /// before the answer (`cards.rs`).
    pub drawn: std::cell::Cell<Option<Instant>>,
    /// What it asks, for its `card` line (`cards.rs`) — Claude's cards and the sandbox's.
    pub asked: Option<crate::cards::Asked>,
}

/// A card's text in a buffer of its own (`v`) is titled this, then the card's title.
pub const CARD_VIEW: &str = "card · ";

/// How long a card that runs something must have been in sight, as it is, before `y` answers it — a key
/// pressed as it appears, or as its text changes, isn't an answer.
pub const SETTLE: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Clone, Debug)]
pub enum What {
    /// Grammars (the language + ones used with it). `lang` = human-readable language name ·
    /// `then` = work to resume once downloaded.
    Grammars { lang: String, names: Vec<String>, then: Option<Resume> },
    /// java-debug plugin — once downloaded, resumes debugging this file
    /// (`attach` = the address, if it was attaching to a running JVM).
    JavaDebug { file: PathBuf, root: PathBuf, attach: Option<(String, u16)> },
    /// A language server or debugger a pack has tarae download (`fetch.rs`): `y` fetches `way` into `dir`, then
    /// `then`.
    Fetch { tool: String, way: Box<crate::fetch::Download>, dir: PathBuf, then: crate::install::Then },
    /// A plugin asks for permissions (when it loads): `y` approves the list, kept.
    PluginPermissions { plugin: String, permissions: Vec<crate::plugin::PluginPermission> },
    /// A plugin asks to connect somewhere new, or read a file outside its patterns: `y` allows it, kept — and
    /// every request waiting on it (all asking the same) goes on.
    PluginAsk { plugin: String, asks: Vec<crate::plugin::PluginAsk> },
    /// A language server or debugger — `run` in the task panel (in `cwd`), then `then`.
    Install { tool: String, run: Vec<String>, cwd: PathBuf, then: crate::install::Then },
    /// A command Claude wants to run (`tools.rs`): `y` runs it in the task panel, `n` tells Claude no.
    /// `shown` = files it names that Claude wrote unseen (review at the end), on the card as they were ·
    /// `unprotected` = the card said there's no sandbox (`sandbox.rs`).
    /// `reads` = places outside the project a read-only command reads — `a`/`A` allow them for longer too.
    Run(Box<crate::tools::RunCard>),
    /// No sandbox on this system (`tools.rs`): `y` runs Claude's command anyway, `a` does for the rest of
    /// the conversation, `n` tells Claude no. `ride`: what goes along to the run (`queued.rs`).
    NoSandbox {
        job: crate::tools::Job,
        call: crate::tools::Call,
        ride: Box<crate::queued::Ride>,
        kind: crate::tools::Unsandboxed,
    },
    /// Review at the end (`edits.rs`): `y` lets Claude's edits go in as it makes them.
    ReviewAtEnd,
    /// Claude's Read, Grep or Glob outside the project (`reads.rs`): `y` once, `a` this chat, `A` always
    /// (those two only when `keep`), `n` no.
    /// `folder`: what `a`/`A` allow instead of the file — its folder, beside other reads (#102).
    Read { pending: Box<crate::reads::Pending>, keep: bool, folder: Option<PathBuf> },
    /// The user opens the sandbox for what it blocked in `run` (`blocked.rs`): `y` once (it runs again with
    /// it), `a` this chat, `A` always — and runs again — `n` no.
    Grant { grant: crate::allow::Grant, run: Box<crate::blocked::BlockedRun> },
    /// The user runs `run` again outside the sandbox, once (`blocked.rs`): `y` · `n`.
    Outside { run: Box<crate::blocked::BlockedRun> },
    /// The project's `.tarae.toml` sets what runs commands (`trust.rs`): `y` trusts it, `n` ignores that
    /// part. `hash` = what the card showed.
    ProjectTrust { hash: u64 },
    /// Claude asks to see the screen (`seeing.rs`): `y` once, `a` this chat, `n` no.
    Screen { call: crate::tools::Call },
    /// `:preflight`'s first run in a project with the packs' default steps (`preflight.rs`): `y` runs those
    /// of `set`.
    Preflight { root: PathBuf, steps: Vec<crate::preflight::Step>, set: crate::preflight::Set },
    /// A branch with commits on no other (`gitpanel.rs`): `y` deletes it, if still at `tip`.
    BranchDelete { top: PathBuf, branch: String, tip: String },
    /// A push over the remote's commits (`gitpanel.rs`): `y` pushes with a lease on what the card listed.
    ForcePush(Box<crate::gitpanel::ForcePush>),
    /// A new session (`worktree.rs`): `y` makes its branch and checkout as the card says.
    SessionNew(Box<crate::worktree::Making>),
    /// Ending a session that has something to lose (`worktree.rs`): `y` removes its checkout and keeps its
    /// branch, `D` deletes the branch too, `n` keeps it all.
    SessionEnd(Box<crate::worktree::Ending>),
    /// Closing a buffer whose change is under review (#54): `y` declines the change and closes it, `n` keeps
    /// both. Not `force`d, unsaved edits of yours keep it open still.
    CloseReviewed { doc: crate::document::DocId, force: bool },
    /// Claude's rename or replace across several files (`refactor.rs`, #75): `y` accepts them all, `r`
    /// reviews them one by one, `v` shows every change, `n` declines them all. `generation` = its chat's.
    ChangeSet { generation: u64 },
    /// Ending a code review with files not viewed (`codereview.rs`, #10): `y` ends it, `n` keeps reviewing.
    /// Its session's, and that review's (`started`) only — #159.
    ReviewEnd { session: crate::event::SessionId, started: Instant },
    /// Bringing a session whose base moved since it left it (`rebase.rs`): `y` rebases it onto its base
    /// first, `n` leaves it.
    Rebase(Box<crate::rebase::Moved>),
    /// A session rebased cleanly (`rebase.rs`): `y` brings it into its base, `p` runs the preflight first and
    /// brings it once that passes, `n` not now.
    Rebased(Box<crate::rebase::Moved>),
    /// A rebase stopped at a conflict its Claude resolved (`rebase.rs`): `y` stages the files and goes on,
    /// `X` aborts the rebase, `n` leaves it stopped.
    RebaseResolved { root: PathBuf },
    /// A git-tab action would run hooks from inside the checkout that its base doesn't have as they are
    /// (`hooks.rs`): `y` runs it as usual, `n` runs it with no hooks this time.
    Hooks(Box<crate::hooks::Ask>),
    /// `:restart` (`restart.rs`, #108): which checkout it builds in and runs, at what commit — `y` the one
    /// tarae started in, `f` the session in front's (when another), `n` none.
    Restart(Box<crate::restart::Choice>),
    /// The binary a restart would run is the one it was, older than its checkout's HEAD: `y` restarts onto
    /// it anyway, `n` doesn't.
    RestartSame { id: u64 },
    /// A buffer's unsaved edits a gone tarae kept (`recovery.rs`): `y` restores them into the buffer, `c`
    /// compares them with the file, `D` discards them, `n` leaves them for the next start.
    Recover(Box<crate::recovery::Found>),
}

/// A call of Claude's waiting on a card for the user's answer (`Session.asking`): the card, how the call is
/// answered, and what the card asks (its title). Taken off when the user answers the card or the turn
/// ends; a card gone otherwise was lost, and the call is told so (`Editor::asking_sync`, #94) — never a
/// call left waiting on a card nobody can see.
#[derive(Clone, Debug)]
pub struct Asking {
    pub card: u64,
    pub asker: Asker,
    pub what: String,
}

#[derive(Clone, Debug)]
pub enum Asker {
    /// One of tarae's tools (`run`, `screen`) — answered as a tool's result.
    Tool(crate::tools::Call),
    /// A permission ask (`can_use_tool` — a Read outside the project) of chat process `generation`.
    Permission { request_id: String, generation: u64 },
}

/// What Claude hears for a call whose card was lost.
pub const LOST: &str = "the card asking the user about this was lost before they could answer (an editor fault) \
— nothing ran; send it again";

impl Offer {
    /// The call waiting on it, if it's Claude's and asks for one call's answer.
    fn asker(&self) -> Option<Asker> {
        match &self.what {
            What::Run(r) => Some(Asker::Tool(r.call.clone())),
            What::NoSandbox { call, .. } | What::Screen { call } => Some(Asker::Tool(call.clone())),
            What::Read { pending, .. } => Some(Asker::Permission {
                request_id: pending.request_id.clone(),
                generation: pending.generation,
            }),
            _ => None,
        }
    }
}

/// A card's text, as `offer_read` knows it: its title and question.
fn text_hash(o: &Offer) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    (&o.title, &o.question).hash(&mut h);
    h.finish()
}

/// Work that stopped for lack of a grammar — resumed once downloaded
/// (language features only once the grammar is in).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resume {
    Test(crate::testing::Scope),
    TestDebug,
    /// Debugging this test (the one it was — not the one at the cursor now).
    TestTarget(Box<crate::testing::Target>),
    Debug,
    Attach(crate::attach::AttachTarget),
}

#[derive(Clone, Debug)]
pub enum OfferState {
    Asking,
    Installing(Instant),
    Failed(String),
}

impl Offer {
    /// A new offer (asking) — `Editor::push_offer` assigns the id.
    pub fn new(title: String, question: &str, working: &str, failed: String, what: What) -> Offer {
        Offer {
            id: 0,
            title,
            question: question.into(),
            working: working.into(),
            failed,
            what,
            state: OfferState::Asking,
            rev: 0,
            decided: None,
            warning: None,
            diff: None,
            since: Instant::now(),
            drawn: Default::default(),
            asked: None,
        }
    }

    /// What it asks, for its `card` line.
    pub fn asked(mut self, a: crate::cards::Asked) -> Offer {
        self.asked = Some(a);
        self
    }

    /// Its `card` line: answered (`y` `a` `A` `n`) or dropped — in `session`.
    fn said(&self, session: &str, answer: &str) {
        crate::cards::said(session, self, answer);
    }

    /// What's being decided is these lines of the question, not its first paragraph.
    pub fn deciding(mut self, lines: std::ops::Range<usize>) -> Offer {
        self.decided = Some(lines);
        self
    }

    /// These lines of the question warn what yes means: the warning color.
    pub fn warning(mut self, lines: std::ops::Range<usize>) -> Offer {
        self.warning = Some(lines);
        self
    }

    /// The question's lines drawn at full strength.
    pub fn decided_lines(&self) -> std::ops::Range<usize> {
        self.decided.clone().unwrap_or_else(|| match self.weighty() {
            true => 0..self.question.split('\n').position(|l| l.trim().is_empty()).unwrap_or(usize::MAX),
            false => 0..0,
        })
    }

    /// Whether answering it runs something (an install, a plugin's permissions).
    pub fn weighty(&self) -> bool {
        matches!(
            self.what,
            What::Install { .. }
                | What::Fetch { .. }
                | What::PluginPermissions { .. }
                | What::PluginAsk { .. }
                | What::Run(_)
                | What::NoSandbox { .. }
                | What::ReviewAtEnd
                | What::ProjectTrust { .. }
                | What::Read { .. }
                | What::Grant { .. }
                | What::Outside { .. }
                | What::Preflight { .. }
                | What::BranchDelete { .. }
                | What::ForcePush(_)
                | What::SessionNew(_)
                | What::SessionEnd(_)
                | What::ChangeSet { .. }
                | What::Rebase(_)
                | What::Rebased(_)
                | What::RebaseResolved { .. }
                | What::Hooks(_)
                | What::Restart(_)
                | What::RestartSame { .. }
                | What::Recover(_)
        )
    }

    /// Whether `c` (compare) and `D` (discard) answer it: unsaved edits a gone tarae kept.
    pub fn recovers(&self) -> bool {
        matches!(self.what, What::Recover(_))
    }

    /// Whether `f` answers it: a restart's card with the session in front's checkout to build instead.
    pub fn restart_other(&self) -> bool {
        matches!(&self.what, What::Restart(c) if c.targets.len() > 1)
    }

    /// Whether `p` answers it: the bring card after a rebase (the preflight, then the bring).
    pub fn preflights(&self) -> bool {
        matches!(self.what, What::Rebased(_))
    }

    /// Whether `X` answers it: a resolved rebase's card (aborts the rebase).
    pub fn aborts(&self) -> bool {
        matches!(self.what, What::RebaseResolved { .. })
    }

    /// Whether `key` is one of its answers that only a settled card in sight takes (`y`, and the letters
    /// that do something too).
    pub fn weighty_answer(&self, c: char) -> bool {
        matches!(c, 'y' | 'a' | 'A' | 'D' | 'R' | 'c')
            || (c == 'p' && self.preflights())
            || (c == 'X' && self.aborts())
            || (c == 'f' && self.restart_other())
    }

    /// The `(generation)` of a change set's card (`r` one by one, `v` every change).
    pub fn change_set(&self) -> Option<u64> {
        match self.what {
            What::ChangeSet { generation } => Some(generation),
            _ => None,
        }
    }

    /// Whether `b` answers it: a new session's card with another base to make it from (#85).
    pub fn base_steps(&self) -> bool {
        matches!(&self.what, What::SessionNew(m) if !m.others.is_empty())
    }

    /// Whether `D` answers it: the end card of a session on a branch (removes both).
    pub fn both(&self) -> bool {
        matches!(&self.what, What::SessionEnd(e) if e.branch.is_some())
    }

    /// The end card of a session with work in no commit: no `y` (a reflex would throw it away) — `R`
    /// removes the checkout, `c` commits first (`commits_first`).
    pub fn end_dirty(&self) -> bool {
        matches!(&self.what, What::SessionEnd(e) if e.dirty())
    }

    /// Whether `c` answers it: that card, on a branch to commit on.
    pub fn commits_first(&self) -> bool {
        matches!(&self.what, What::SessionEnd(e) if e.dirty() && e.branch.is_some())
    }

    /// Whether its answer can last longer than once (`a` this chat · `A` always — `A` only for reads).
    pub fn lasts(&self) -> bool {
        match &self.what {
            What::NoSandbox { .. }
            | What::Read { keep: true, .. }
            | What::Grant { .. }
            | What::Screen { .. } => true,
            // (what it reads outside, or its pattern for the rest of the chat)
            What::Run(r) => r.keep || r.pattern.is_some() || r.socket.is_some(),
            _ => false,
        }
    }

    /// Whether `A` (always) answers it: what it allows is kept for the project — a run card's reads, never
    /// its pattern.
    pub fn keeps(&self) -> bool {
        match &self.what {
            What::Run(r) => r.keep,
            // (a socket is this conversation's at most — #119)
            What::Grant { grant, .. } => grant.keepable(),
            _ => self.lasts() && matches!(self.what, What::Read { .. }),
        }
    }

    /// Whether Claude's call waits on it — it goes with its session when another comes to the front.
    pub fn claudes(&self) -> bool {
        matches!(
            self.what,
            What::Run(_)
                | What::NoSandbox { .. }
                | What::Read { .. }
                | What::Screen { .. }
                | What::ChangeSet { .. }
        )
    }

    /// Whether it parks with its session when another comes to the front: Claude's, and a review's end
    /// (#159: it stayed up in the other session, and `y` there ended that one's review).
    pub fn goes_with_session(&self) -> bool {
        self.claudes() || matches!(self.what, What::ReviewEnd { .. })
    }

    pub fn installing(&self) -> bool {
        matches!(self.state, OfferState::Installing(_))
    }

    /// Its answers as its hint row draws them, key and what it does — the card and the UI state read this
    /// one list (`uistate.rs`). None while it's downloading. `quick`: the project has a quick preflight set
    /// (the bring card's `p` runs that one).
    pub fn answers(&self, quick: bool) -> Vec<(char, &'static str)> {
        use What as W;
        match self.state {
            OfferState::Installing(_) => return Vec::new(),
            OfferState::Failed(_) => return vec![('y', "Retry"), ('n', "Dismiss")],
            OfferState::Asking => {}
        }
        match &self.what {
            W::Grammars { .. } | W::JavaDebug { .. } => vec![('y', "Install"), ('n', "Not now")],
            W::Install { .. } | W::Fetch { .. } => vec![('y', "Install"), ('n', "Not now"), ('N', "Never")],
            W::PluginPermissions { .. } => vec![('y', "Allow"), ('n', "Not now")],
            W::PluginAsk { .. } => vec![('y', "Allow"), ('n', "No")],
            // (one socket past the sandbox — #119)
            W::Run(r) if r.socket.is_some() => vec![('y', "This run"), ('a', "This chat"), ('n', "No")],
            W::Run(r) if r.keep => vec![('y', "Run"), ('a', "This chat"), ('A', "Always"), ('n', "No")],
            // (its pattern for the rest of the chat — the card says which)
            W::Run(r) if r.pattern.is_some() => vec![('y', "Run"), ('a', "This chat"), ('n', "No")],
            W::Run(_) => vec![('y', "Run"), ('n', "No")],
            W::ReviewAtEnd => vec![('y', "Turn on"), ('n', "No")],
            W::ProjectTrust { .. } => vec![('y', "Trust"), ('n', "Ignore")],
            W::NoSandbox { .. } => vec![('y', "Run"), ('a', "This chat"), ('n', "No")],
            W::Read { keep: true, .. } => {
                vec![('y', "Once"), ('a', "This chat"), ('A', "Always"), ('n', "No")]
            }
            W::Read { keep: false, .. } => vec![('y', "Once"), ('n', "No")],
            W::Grant { grant, .. } if !grant.keepable() => {
                vec![('y', "This run"), ('a', "This chat"), ('n', "No")]
            }
            W::Grant { .. } => vec![('y', "Once"), ('a', "This chat"), ('A', "Always"), ('n', "No")],
            W::Outside { .. } => vec![('y', "Run"), ('n', "No")],
            W::Preflight { .. } => vec![('y', "Run them"), ('n', "Not now")],
            W::BranchDelete { .. } => vec![('y', "Delete"), ('n', "Keep it")],
            W::ForcePush(_) => vec![('y', "Push"), ('n', "No")],
            W::SessionNew(_) if self.base_steps() => {
                vec![('y', "Make it"), ('b', "Other base"), ('n', "Not now")]
            }
            W::SessionNew(_) => vec![('y', "Make it"), ('n', "Not now")],
            // (work in no commit: no `y` — committing leads, removing takes a capital)
            W::SessionEnd(_) if self.commits_first() => {
                vec![('c', "Commit, then end"), ('R', "Remove"), ('D', "Remove both"), ('n', "Cancel")]
            }
            W::SessionEnd(_) if self.end_dirty() => vec![('R', "Remove"), ('n', "Cancel")],
            W::SessionEnd(_) if self.both() => {
                vec![('y', "Remove checkout"), ('D', "Remove both"), ('n', "Cancel")]
            }
            W::SessionEnd(_) => vec![('y', "Remove it"), ('n', "Cancel")],
            // (it fell to the download's row once: `y Install` on a card whose `y` declines Claude's change)
            W::CloseReviewed { .. } => vec![('y', "Decline and close"), ('n', "Keep both")],
            W::Screen { .. } => vec![('y', "Once"), ('a', "This chat"), ('n', "No")],
            W::ReviewEnd { .. } => vec![('y', "End it"), ('n', "Keep reviewing")],
            W::Rebase(_) => vec![('y', "Rebase first"), ('n', "Leave it")],
            // (`p` runs the quick set when the project has two — the card names its steps)
            W::Rebased(_) => {
                let p = if quick { "Quick preflight, then bring" } else { "Preflight, then bring" };
                vec![('y', "Bring"), ('p', p), ('n', "Not now")]
            }
            // (abort throws the resolution away: a capital, as removing does)
            W::RebaseResolved { .. } => vec![('y', "Continue"), ('X', "Abort"), ('n', "Not now")],
            W::Hooks(a) if a.runnable() => {
                vec![('y', if a.hooks.len() == 1 { "Run it" } else { "Run them" }), ('n', "Skip hooks")]
            }
            W::Hooks(_) => vec![('n', "Skip hooks")],
            W::Restart(_) if self.restart_other() => {
                vec![('y', "Restart"), ('f', "The front's instead"), ('n', "Not now")]
            }
            W::Restart(_) => vec![('y', "Restart"), ('n', "Not now")],
            W::RestartSame { .. } => vec![('y', "Restart anyway"), ('n', "Don't")],
            // (throwing them away takes a capital, as removing does)
            W::Recover(_) => vec![('y', "Restore"), ('c', "Compare"), ('D', "Discard"), ('n', "Not now")],
            W::ChangeSet { .. } => {
                vec![('y', "Accept all"), ('r', "One by one"), ('v', "See them"), ('n', "Decline all")]
            }
        }
    }

    /// Its kind, as the UI state names it (`uistate.rs`): the `What` in kebab case.
    pub fn kind(&self) -> &'static str {
        use What as W;
        match self.what {
            W::Grammars { .. } => "grammars",
            W::JavaDebug { .. } => "java-debug",
            W::Fetch { .. } => "fetch",
            W::PluginPermissions { .. } => "plugin-permissions",
            W::PluginAsk { .. } => "plugin-ask",
            W::Install { .. } => "install",
            W::Run(_) => "run",
            W::NoSandbox { .. } => "no-sandbox",
            W::ReviewAtEnd => "review-at-end",
            W::Read { .. } => "read",
            W::Grant { .. } => "grant",
            W::Outside { .. } => "outside",
            W::ProjectTrust { .. } => "project-trust",
            W::Screen { .. } => "screen",
            W::Preflight { .. } => "preflight",
            W::BranchDelete { .. } => "branch-delete",
            W::ForcePush(_) => "force-push",
            W::SessionNew(_) => "session-new",
            W::SessionEnd(_) => "session-end",
            W::CloseReviewed { .. } => "close-reviewed",
            W::ChangeSet { .. } => "change-set",
            W::ReviewEnd { .. } => "review-end",
            W::Rebase(_) => "rebase",
            W::Rebased(_) => "rebased",
            W::RebaseResolved { .. } => "rebase-resolved",
            W::Hooks(_) => "hooks",
            W::Restart(_) => "restart",
            W::RestartSame { .. } => "restart-same",
            W::Recover(_) => "recover",
        }
    }
}

impl crate::editor::Editor {
    /// Resumes the stopped work (after the grammar is downloaded).
    pub fn resume(&mut self, r: Resume) {
        match r {
            Resume::Test(scope) => self.test_run(scope),
            Resume::TestDebug => self.test_debug(),
            Resume::TestTarget(t) => self.test_debug_target(*t),
            Resume::Debug => self.dap_launch(),
            Resume::Attach(t) => self.attach(t),
        }
    }

    /// Card `id` was answered by the user: the call it held is the answer's now, no longer waiting on it.
    fn card_answered(&mut self, id: u64) {
        self.session.asking.retain(|a| a.card != id);
    }

    /// The cards of the session in `ed.session`: up in front, parked behind.
    pub fn session_cards(&self) -> impl Iterator<Item = &Offer> {
        let front = !self.background();
        self.session.parked.iter().chain(self.offers.iter().filter(move |_| front))
    }

    /// The cards of the session in `ed.session` that `gone` picks go: up in front, parked behind (#86: a
    /// session behind ended its turn swapped in, so its cards were parked — they stayed, its tab said `▲`
    /// after its turn was over, and the front's cards went instead).
    pub fn drop_cards(&mut self, gone: impl Fn(&What) -> bool) {
        let front = !self.background();
        let label = self.session.label();
        let mut dropped = Vec::new();
        for o in self.session.parked.iter().chain(self.offers.iter().filter(|_| front)) {
            if gone(&o.what) {
                o.said(label, "dropped");
                dropped.push(o.id);
            }
        }
        for id in dropped {
            self.card_how(id, "dropped — its turn ended");
        }
        self.session.parked.retain(|o| !gone(&o.what));
        if front {
            self.offers.retain(|o| !gone(&o.what));
        }
    }

    /// Stacks an offer (at the back). If one with the same title exists, keeps it (untouched while
    /// downloading; only its content is refreshed while asking) — not a command of Claude's: each waits for
    /// its own answer (#5: the second was refused, and Claude sent it again and again).
    pub fn push_offer(&mut self, mut o: Offer) {
        self.offer_seq += 1;
        o.id = self.offer_seq;
        o.since = Instant::now();
        // (the call it holds waits on it — watched until it's answered: `asking_sync`)
        if let Some(asker) = o.asker() {
            self.session.asking.push(Asking { card: o.id, asker, what: o.title.clone() });
        }
        // From a session behind: it waits in that session, and comes up when the session is in front (`session_in`)
        if self.background() {
            return self.session.parked.push(o);
        }
        // (one holding a call isn't merged into another by its title — the other's call would be lost; nor is a
        // plugin's ask — the other's request would be: one asking the same joins its card, `plugin_ask`)
        let own = o.asker().is_some() || matches!(o.what, What::PluginAsk { .. });
        if let Some(old) = self.offers.iter_mut().find(|x| x.title == o.title).filter(|_| !own) {
            // Replaced whole — what it says with what it does — and it settles again before `y` counts
            if !old.installing() {
                (old.question, old.working, old.failed) = (o.question, o.working, o.failed);
                (old.decided, old.warning, old.asked, old.diff) = (o.decided, o.warning, o.asked, o.diff);
                (old.what, old.state) = (o.what, OfferState::Asking);
                old.rev += 1;
            }
            return;
        }
        // A plugin asking something: its panel gives the keys back, so they reach the card
        if matches!(o.what, What::PluginPermissions { .. } | What::PluginAsk { .. }) {
            self.plugin_panel_unfocus();
        }
        // A card that runs something (or shows Claude the screen), while the chat has the keys and nothing is
        // being typed there (Claude asking mid-turn): the chat gives them to the card — as a review takes them. Mid-message, the
        // card waits (cards don't take keys from the chat's input — a `n` typed there was a "no")
        if (o.weighty() || matches!(o.what, What::Screen { .. }))
            && let Some(c) = self.session.chat.as_mut().filter(|c| c.focused && c.input.is_empty())
        {
            c.focused = false;
        }
        self.offers.push(o);
    }

    /// A card's keys while it has them (`Editor::card_has_keys`) — true if handled (`y` yes · `n` no · `a`/`A`
    /// for longer · `N` never, for an install, on the front card · `v` all of a cut one).
    pub fn offer_key(&mut self, key: crate::key::Key) -> bool {
        use crate::key::Code;
        let Some(o) = self.offers.first() else { return false };
        if o.installing() || key.ctrl || key.alt || self.plugin_panel_focused() {
            return false;
        }
        // One that runs something: only from a card in sight (one too big for the screen isn't drawn) — and
        // settled: a key pressed as it appeared or changed is swallowed, not an answer
        if o.weighty() {
            // (drawn: its answers are on screen — a digit is a card stacked behind)
            if !self.screen.offer_buttons.iter().any(|b| !b.3.is_ascii_digit()) {
                return false;
            }
            if !self.offer_settled()
                && matches!(key.code, Code::Char(c) if matches!(c, 'n' | 'N') || o.weighty_answer(c))
            {
                // (a key pressed as it came up isn't an answer — said, not swallowed in silence)
                self.offer_refused(format!("{key} came as the card did — not taken; press it again"));
                return true;
            }
            // A change set's card: `r` one by one, `v` every change in a buffer (not a cut card's text)
            if let Some(generation) = o.change_set() {
                match key.code {
                    Code::Char('r') => {
                        let id = o.id;
                        crate::refactor::one_by_one(self, id, generation);
                        return true;
                    }
                    Code::Char('v') => {
                        crate::refactor::view_all(self, generation);
                        return true;
                    }
                    _ => {}
                }
            }
            // Cut to fit the screen: `v` shows it all, and `y` waits for that (not taken otherwise — it's
            // select mode in the code)
            if !self.offer_seen_whole() {
                match key.code {
                    Code::Char('v') => {
                        self.offer_view_all();
                        return true;
                    }
                    Code::Char(c) if o.weighty_answer(c) => {
                        self.offer_see_all_first();
                        return true;
                    }
                    _ => {}
                }
            }
        }
        // (Esc isn't an answer — #25: it's how one leaves an input or a mode, and a "no" by accident told Claude
        // no; `n` says no)
        match key.code {
            Code::Char('R') if o.end_dirty() => self.offer_remove(false),
            Code::Char('c') if o.commits_first() => self.offer_commit_first(),
            Code::Char('c') if o.recovers() => crate::recovery::compare(self),
            Code::Char('D') if o.recovers() => self.offer_discard(),
            Code::Char('y') => self.offer_accept(),
            Code::Char('n') => self.offer_dismiss(),
            Code::Char('N') if matches!(o.what, What::Install { .. } | What::Fetch { .. }) => {
                self.offer_never()
            }
            Code::Char('a') if o.lasts() => self.offer_for_chat(),
            Code::Char('A') if o.keeps() => self.offer_always(),
            Code::Char('D') if o.both() => self.offer_remove(true),
            Code::Char('p') if o.preflights() => self.offer_preflight_first(),
            Code::Char('X') if o.aborts() => self.offer_abort(),
            Code::Char('b') if o.base_steps() => self.offer_session_base(),
            Code::Char('f') if o.restart_other() => self.offer_restart_other(),
            // An answer letter this card doesn't take isn't the code's either (`a`/`A` there start inserting —
            // and the `y` after it was typed)
            Code::Char('a' | 'A') if o.lasts() => self.set_status("this card asks y, a or n"),
            Code::Char('a' | 'A') if o.weighty() => self.set_status("this card asks only y or n"),
            _ => return false,
        }
        true
    }

    /// Downloads the front card — it moves to the back while downloading (the next offer comes forward).
    pub fn offer_accept(&mut self) {
        if self.offers.first().is_none_or(Offer::installing) {
            return;
        }
        // (no yes that throws away work in no commit)
        if self.offers.first().is_some_and(Offer::end_dirty) {
            return self.set_status(match self.offers[0].commits_first() {
                true => "its changes are in no commit — c commits them first, R removes them anyway",
                false => "its changes are in no commit — R removes them anyway, n keeps them",
            });
        }
        // (a hook the card couldn't show isn't run on a yes)
        if matches!(&self.offers[0].what, What::Hooks(a) if !a.runnable()) {
            return self.set_status("a hook here can't be shown — n goes on without hooks");
        }
        // (no restore over edits of yours, nor into a file still loading — said on the card, which stays)
        if let What::Recover(f) = &self.offers[0].what
            && let Some(why) = crate::recovery::refuses(self, f)
        {
            return self.offer_refused(why);
        }
        let mut o = self.offers.remove(0);
        o.said(self.session.label(), "y");
        self.card_how(o.id, "answered y");
        o.state = OfferState::Installing(Instant::now());
        let (id, what) = (o.id, o.what.clone());
        self.offers.push(o);
        self.card_answered(id);
        match what {
            What::Grammars { names, .. } => {
                self.install_grammars(names, Some(id));
            }
            What::JavaDebug { .. } => self.java_debug_download(id),
            What::Fetch { tool, way, dir, then } => self.fetch_start(id, tool, *way, dir, then),
            What::Install { tool, run, cwd, then } => self.install_start(id, tool, run, cwd, then),
            What::PluginPermissions { plugin, permissions } => {
                self.offer_finished(id, Ok(()));
                self.plugin_permissions_answer(plugin, permissions, true);
            }
            What::PluginAsk { plugin, asks } => {
                self.offer_finished(id, Ok(()));
                self.plugin_ask_answer(plugin, asks, true);
            }
            What::Run(card) => crate::tools::run_answer(self, id, *card, Some(crate::allow::Scope::Once)),
            What::NoSandbox { job, call, ride, kind } => crate::tools::unprotected_answer(
                self,
                id,
                (job, call, *ride, kind),
                crate::tools::Unprotected::Once,
            ),
            What::ReviewAtEnd => {
                self.offer_finished(id, Ok(()));
                crate::edits::at_end_set(self, true);
            }
            What::ProjectTrust { hash } => self.project_trust_answer(id, hash, true),
            What::Read { pending, keep, folder } => {
                crate::reads::answer(self, id, *pending, Some(crate::allow::Scope::Once), keep, folder)
            }
            What::Grant { grant, run } => {
                crate::blocked::grant_answer(self, id, grant, *run, Some(crate::allow::Scope::Once))
            }
            What::Outside { run } => crate::blocked::outside_answer(self, id, *run, true),
            What::Screen { call } => crate::seeing::answer(self, id, call, Some(crate::allow::Scope::Once)),
            What::Preflight { root, steps, set } => self.preflight_answer(id, root, (steps, set), true),
            What::BranchDelete { top, branch, tip } => self.git_branch_delete_answer(id, top, branch, tip),
            What::ForcePush(f) => self.git_force_push(id, *f),
            What::SessionNew(m) => self.session_make(id, *m),
            What::SessionEnd(e) => self.session_remove(*e, false, Some(id)),
            What::CloseReviewed { doc, force } => {
                self.offer_finished(id, Ok(()));
                self.close_reviewed(doc, force);
            }
            What::ChangeSet { generation } => crate::refactor::accept_all(self, id, generation),
            What::ReviewEnd { session, started } => {
                self.offer_finished(id, Ok(()));
                crate::codereview::close_answered(self, session, started);
            }
            What::Rebase(m) => self.rebase_start(id, *m),
            What::Rebased(m) => {
                self.offer_finished(id, Ok(()));
                self.rebase_bring(*m);
            }
            What::RebaseResolved { root } => {
                self.offer_finished(id, Ok(()));
                self.rebase_answer(&root, true);
            }
            What::Hooks(ask) => self.hooks_answer(id, *ask, true),
            What::Restart(c) => crate::restart::chosen(self, id, *c, Some(0)),
            What::RestartSame { id: r } => crate::restart::same_answer(self, id, r, true),
            What::Recover(f) => crate::recovery::restore(self, id, *f),
        }
    }

    /// `D` on a card of unsaved edits a gone tarae kept: they're thrown away.
    pub fn offer_discard(&mut self) {
        if !self.offers.first().is_some_and(Offer::recovers) {
            return;
        }
        let o = self.offers.remove(0);
        o.said(self.session.label(), "D");
        self.card_how(o.id, "answered D");
        self.card_answered(o.id);
        if let What::Recover(f) = o.what {
            crate::recovery::discard(self, *f);
        }
    }

    /// `f` on a restart's card: the session in front's checkout instead of the one tarae started in.
    pub fn offer_restart_other(&mut self) {
        if !self.offers.first().is_some_and(Offer::restart_other) {
            return;
        }
        let o = self.offers.remove(0);
        self.card_answered(o.id);
        if let What::Restart(c) = o.what {
            crate::restart::chosen(self, o.id, *c, Some(1));
        }
    }

    /// `p` on the bring card after a rebase: the preflight first, the bring once it passes.
    pub fn offer_preflight_first(&mut self) {
        if !self.offers.first().is_some_and(Offer::preflights) {
            return;
        }
        if let What::Rebased(m) = self.offers.remove(0).what {
            self.rebase_preflight_then_bring(*m);
        }
    }

    /// `X` on a resolved rebase's card: the rebase aborted.
    pub fn offer_abort(&mut self) {
        if !self.offers.first().is_some_and(Offer::aborts) {
            return;
        }
        if let What::RebaseResolved { root } = self.offers.remove(0).what {
            self.rebase_answer(&root, false);
        }
    }

    /// `D` on a session's end card: its checkout and its branch go · `R` (`both` false, work in no commit
    /// there): its checkout goes anyway, the branch stays.
    pub fn offer_remove(&mut self, both: bool) {
        let fits = |o: &Offer| if both { o.both() } else { o.end_dirty() };
        if !self.offers.first().is_some_and(fits) {
            return;
        }
        let mut o = self.offers.remove(0);
        o.state = OfferState::Installing(Instant::now());
        let (id, what) = (o.id, o.what.clone());
        self.offers.push(o);
        if let What::SessionEnd(e) = what {
            self.session_remove(*e, both, Some(id));
        }
    }

    /// `b` on a new session's card: made from the next base it offers — the card says so, and settles again
    /// before `y` (#85).
    pub fn offer_session_base(&mut self) {
        let Some(What::SessionNew(m)) = self.offers.first().map(|o| o.what.clone()) else { return };
        let mut m = *m;
        if m.others.is_empty() {
            return;
        }
        let (b, s) = m.others.remove(0);
        m.others.push((std::mem::replace(&mut m.base, b), std::mem::replace(&mut m.base_sha, s)));
        self.push_offer(m.card());
    }

    /// `c` on a session's end card with work in no commit: committed on its branch first, then asked again.
    pub fn offer_commit_first(&mut self) {
        if !self.offers.first().is_some_and(Offer::commits_first) {
            return;
        }
        if let What::SessionEnd(e) = self.offers.remove(0).what {
            self.session_commit_first(*e);
        }
    }

    /// Whether the front card has been in sight whole — drawn uncut, or opened with `v` — as it is now; or a
    /// card with the very same text was, since tarae started (#55: the same script run again was to be read again).
    pub fn offer_seen_whole(&self) -> bool {
        self.offers.first().is_some_and(|o| {
            self.offer_whole.get() == Some((o.id, o.rev)) || self.offer_read.borrow().contains(&text_hash(o))
        })
    }

    /// Card `(id, rev)` has been in sight whole — and so has its text, should a card bring it again.
    pub fn offer_read_whole(&self, card: (u64, u64)) {
        self.offer_whole.set(Some(card));
        if let Some(o) = self.offers.iter().find(|o| (o.id, o.rev) == card) {
            self.offer_read.borrow_mut().insert(text_hash(o));
        }
    }

    /// The key that goes to a buffer's end, as the keymap has it (`ge`) — #55: the card said `G`.
    pub(crate) fn end_key(&self) -> String {
        let key = self.keymaps.bindings().remove("goto_last_line").unwrap_or_else(|| "g e".into());
        // (`g e` reads as typed: `ge` — a key with a name keeps its space: `space e`)
        match key.split(' ').all(|k| k.chars().count() == 1) {
            true => key.replace(' ', ""),
            false => key,
        }
    }

    /// `y` on a card cut to fit: not an answer yet — said on the card (and its cut row too).
    pub fn offer_see_all_first(&mut self) {
        self.offer_early.set(self.offers.first().map(|o| (o.id, o.rev)));
        // (what to do first — a bar cuts the end of what it says)
        match self.offer_viewing() {
            true => {
                self.offer_refused(format!("{} to its end first — y waits until it's read", self.end_key()))
            }
            // (the card's cut row says it, in the warning colour — `offer_early`)
            false => self.recorded(
                crate::editor::ToastKind::Info,
                "v shows all of it — y waits until it's been seen".into(),
            ),
        }
    }

    /// A key the front card didn't take as an answer, and why — said on the card itself (a row in the warning
    /// colour over its answers, or its bar's), where the one who pressed it is looking; kept in `:messages`.
    /// No silent refusal: the status line flashing it was missed (2026-10-04).
    pub fn offer_refused(&mut self, msg: impl Into<String>) {
        let Some((id, rev)) = self.offers.first().map(|o| (o.id, o.rev)) else { return };
        let msg = msg.into();
        self.recorded(crate::editor::ToastKind::Info, msg.clone());
        self.offer_said = Some((id, rev, msg));
    }

    /// What the front card says about the last key it refused (None: none, or it was about another card or
    /// what it said before it changed).
    pub fn offer_said_now(&self) -> Option<&str> {
        let o = self.offers.first()?;
        // (a `y` that waited for it to be read: past once it has been — the card says `y` answers now)
        let stale = |s: &str| s.contains("y waits until") && self.offer_seen_whole();
        self.offer_said.as_ref().filter(|s| (s.0, s.1) == (o.id, o.rev) && !stale(&s.2)).map(|s| s.2.as_str())
    }

    /// Every event: the whole text of a card that's been answered (or has gone) closes with it (#55) — any
    /// `card · <title>` buffer whose card is gone, not only the last one viewed (#65: one viewed before
    /// another card's `v` stayed in the tab row after its answer). A card waiting in a session behind keeps
    /// its own. By the card's id (E17): a card of the same title up elsewhere — another session's "Claude
    /// wants to run" — doesn't keep it.
    pub(crate) fn offer_view_sync(&mut self) {
        if let Some((id, _, _)) = self.offer_view.get()
            && !self.offers.iter().any(|o| o.id == id)
        {
            self.offer_view.set(None);
        }
        let card = |d: &crate::document::Document| {
            d.throwaway && d.path.is_none() && d.title.as_deref().is_some_and(|t| t.starts_with(CARD_VIEW))
        };
        if !self.docs.iter().any(card) {
            self.offer_views.clear();
            return;
        }
        let cards: Vec<(u64, &str)> = std::iter::once(&self.session)
            .chain(&self.sessions)
            .flat_map(|s| &s.parked)
            .chain(&self.offers)
            .map(|o| (o.id, o.title.as_str()))
            .collect();
        let up = |d: &crate::document::Document| match self.offer_views.iter().find(|(doc, _)| *doc == d.id) {
            Some((_, id)) => cards.iter().any(|(c, _)| c == id),
            // (one of an earlier kind, its card not noted: by title, as before)
            None => {
                let t = d.title.as_deref().unwrap_or_default();
                cards.iter().any(|(_, title)| t.strip_prefix(CARD_VIEW) == Some(title))
            }
        };
        let gone: Vec<_> = self.docs.iter().filter(|d| card(d) && !up(d)).map(|d| d.id).collect();
        for doc in gone {
            self.close_doc(doc);
        }
        let docs: Vec<_> = self.docs.iter().map(|d| d.id).collect();
        self.offer_views.retain(|(doc, _)| docs.contains(doc));
    }

    /// `v` on the front card: a change set's every change, else its whole text.
    pub fn offer_view(&mut self) {
        match self.offers.first().and_then(Offer::change_set) {
            Some(generation) => crate::refactor::view_all(self, generation),
            None => self.offer_view_all(),
        }
    }

    /// `r` on a change set's card: one by one.
    pub fn offer_one_by_one(&mut self) {
        if let Some((id, generation)) = self.offers.first().and_then(|o| Some((o.id, o.change_set()?))) {
            crate::refactor::one_by_one(self, id, generation);
        }
    }

    /// Whether the front card's whole text is open (`v`), not yet read to its end.
    pub fn offer_viewing(&self) -> bool {
        self.offers
            .first()
            .is_some_and(|o| self.offer_view.get().is_some_and(|v| (v.0, v.1) == (o.id, o.rev)))
    }

    /// The front card's whole text (`v`) is the buffer in sight: the card steps aside to a one-row bar under
    /// it (U8 — whole it covered the right half of the very text it's about); back whole once you leave it.
    pub fn offer_bar(&self) -> bool {
        self.offer_viewing() && self.offer_view.get().is_some_and(|v| v.2 == self.doc().id)
    }

    /// `v` on a card cut to fit — its whole text in a throwaway buffer (one per card title: an earlier one
    /// gives way), and the card stays to answer.
    pub fn offer_view_all(&mut self) {
        let Some(o) = self.offers.first() else { return };
        let (id, rev, title) = (o.id, o.rev, o.title.clone());
        let text = format!("{title}\n\n{}\n", o.question);
        let name = format!("{CARD_VIEW}{title}");
        let old: Vec<_> = self
            .docs
            .iter()
            .filter(|d| d.throwaway && d.title.as_deref() == Some(&name))
            .map(|d| d.id)
            .collect();
        for d in old {
            self.close_doc(d);
        }
        let doc = self.open_virtual(&name, &text);
        // (wrapped as the card is, whatever editor.soft-wrap says: a script's long line read before its run
        // never runs off the edge — #156; `z w` unwraps it)
        if let Some(d) = self.docs.iter_mut().find(|d| d.id == doc) {
            d.wrap = Some(true);
        }
        // (seen whole once its last line has been on screen — `term::render_frame` looks)
        self.offer_view.set(Some((id, rev, doc)));
        self.offer_views.retain(|(d, _)| *d != doc);
        self.offer_views.push((doc, id));
        // The code has the keys, to read it (#55: the review's rail kept them, and keys did nothing)
        if let Some(c) = self.session.chat.as_mut() {
            c.focused = false;
        }
        if let Some(s) = self.session.sidebar.as_mut() {
            s.focus = false;
        }
        if let Some(r) = self.session.code_review.as_mut() {
            r.focus = false;
        }
        if let Some(t) = self.session.terminal.as_mut() {
            t.focus = false;
        }
        self.bottom_focus = false;
        // (the card's bar under it says so — U8)
        self.note(format!("the card's whole text — {} to its end, then y answers", self.end_key()));
    }

    /// `a`: yes for the rest of this conversation — the no-sandbox card (Claude's commands run unprotected),
    /// a read outside the project or a command reading there (those places).
    pub fn offer_for_chat(&mut self) {
        if !self.offers.first().is_some_and(Offer::lasts) {
            return;
        }
        let o = self.offers.remove(0);
        self.offer_lasting(o, crate::allow::Scope::ThisChat);
    }

    /// `A` on a read outside the project, or a command reading there: those places, always, in this
    /// project (`allow.rs`).
    pub fn offer_always(&mut self) {
        if !self.offers.first().is_some_and(Offer::keeps) {
            return;
        }
        let o = self.offers.remove(0);
        self.offer_lasting(o, crate::allow::Scope::Always);
    }

    fn offer_lasting(&mut self, o: Offer, scope: crate::allow::Scope) {
        let key = if scope == crate::allow::Scope::Always { "A" } else { "a" };
        o.said(self.session.label(), key);
        self.card_how(o.id, format!("answered {key}"));
        self.card_answered(o.id);
        match o.what {
            What::NoSandbox { job, call, ride, kind } => crate::tools::unprotected_answer(
                self,
                o.id,
                (job, call, *ride, kind),
                crate::tools::Unprotected::ThisChat,
            ),
            What::Read { pending, keep, folder } => {
                crate::reads::answer(self, o.id, *pending, Some(scope), keep, folder)
            }
            What::Run(card) => crate::tools::run_answer(self, o.id, *card, Some(scope)),
            What::Grant { grant, run } => crate::blocked::grant_answer(self, o.id, grant, *run, Some(scope)),
            What::Screen { call } => crate::seeing::answer(self, o.id, call, Some(scope)),
            _ => {}
        }
    }

    /// `N` on an install: closes it, and it isn't offered again.
    pub fn offer_never(&mut self) {
        let Some(o) = self.offers.first().filter(|o| !o.installing()) else { return };
        let (What::Install { tool, .. } | What::Fetch { tool, .. }) = &o.what else { return };
        let tool = tool.clone();
        self.offers.remove(0);
        self.install_never(&tool);
    }

    pub fn offer_dismiss(&mut self) {
        if self.offers.first().is_none_or(Offer::installing) {
            return;
        }
        let o = self.offers.remove(0);
        o.said(self.session.label(), "n");
        self.card_how(o.id, "answered n");
        self.card_answered(o.id);
        match o.what {
            What::Grammars { lang, names, .. } => {
                let first = names.first().cloned().unwrap_or_default();
                self.set_status(format!(
                    "No colors for {lang} until you quit — :grammar-install {first} anytime"
                ));
            }
            What::JavaDebug { .. } => self.set_status("Java debugging needs java-debug — F5 asks again"),
            What::PluginPermissions { plugin, permissions } => {
                self.plugin_permissions_answer(plugin, permissions, false)
            }
            What::PluginAsk { plugin, asks } => self.plugin_ask_answer(plugin, asks, false),
            What::Install { tool, .. } | What::Fetch { tool, .. } => {
                self.set_status(format!("Not installing {tool} for now (N on its card: never ask)"))
            }
            What::Run(card) => crate::tools::run_answer(self, o.id, *card, None),
            What::NoSandbox { job, call, ride, kind } => crate::tools::unprotected_answer(
                self,
                o.id,
                (job, call, *ride, kind),
                crate::tools::Unprotected::No,
            ),
            What::ReviewAtEnd => self.set_status("Claude's edits come one at a time"),
            What::ProjectTrust { hash } => self.project_trust_answer(o.id, hash, false),
            What::Read { pending, keep, folder } => {
                crate::reads::answer(self, o.id, *pending, None, keep, folder)
            }
            What::Grant { grant, run } => crate::blocked::grant_answer(self, o.id, grant, *run, None),
            What::Outside { run } => crate::blocked::outside_answer(self, o.id, *run, false),
            What::Screen { call } => crate::seeing::answer(self, o.id, call, None),
            What::Preflight { root, steps, set } => self.preflight_answer(o.id, root, (steps, set), false),
            What::BranchDelete { branch, .. } => self.set_status(format!("Kept {branch}")),
            What::ForcePush(f) => self.set_status(format!("Not pushed — {} as it was", f.upstream())),
            What::SessionNew(m) => self.set_status(format!("No session {} — nothing was made", m.branch)),
            What::SessionEnd(e) => self.set_status(format!("Kept {} as it is", e.name)),
            What::CloseReviewed { .. } => {
                self.set_status("kept open — Claude's change still waits for y or n")
            }
            What::ChangeSet { generation } => crate::refactor::decline_all(self, generation),
            What::ReviewEnd { .. } => self.set_status("still reviewing — space v m marks a file viewed"),
            What::Rebase(m) => {
                self.set_status(format!("Not rebased — {} as it is, nothing brought", m.branch))
            }
            What::Rebased(m) => {
                self.set_status(format!("Not brought — :session-bring {} when you're ready", m.name))
            }
            What::RebaseResolved { .. } => {
                self.set_status("the rebase waits — :rebase-continue goes on, :rebase-abort ends it")
            }
            What::Hooks(ask) => self.hooks_answer(o.id, *ask, false),
            What::Restart(c) => crate::restart::chosen(self, o.id, *c, None),
            What::RestartSame { id } => crate::restart::same_answer(self, o.id, id, false),
            What::Recover(f) => crate::recovery::not_now(self, *f),
        }
    }

    /// Whether the front card has been in sight, as it is, long enough for `y` (always, for a light one).
    pub fn offer_settled(&self) -> bool {
        let Some(o) = self.offers.first() else { return false };
        !o.weighty()
            || self
                .offer_seen
                .get()
                .is_some_and(|(id, rev, at)| (id, rev) == (o.id, o.rev) && at.elapsed() >= SETTLE)
    }

    /// Brings the `i`-th stacked card to the front (when a back card is clicked).
    pub fn offer_to_front(&mut self, i: usize) {
        if i > 0 && i < self.offers.len() {
            let o = self.offers.remove(i);
            self.offers.insert(0, o);
        }
    }

    /// That offer (when its download finishes — None if it was closed meanwhile).
    pub fn offer_by_id(&self, id: u64) -> Option<&Offer> {
        self.offers.iter().find(|o| o.id == id)
    }

    /// Download finished — on success the card goes away, on failure it shows the reason
    /// (and asks again when its turn comes).
    pub fn offer_finished(&mut self, id: u64, result: Result<(), String>) {
        let Some(i) = self.offers.iter().position(|o| o.id == id) else { return };
        match result {
            Ok(()) => {
                self.offers.remove(i);
            }
            Err(why) => self.offers[i].state = OfferState::Failed(why),
        }
    }

    /// Is anything downloading (keeps the tick running — the wave).
    pub fn offer_busy(&self) -> bool {
        self.offers.iter().any(Offer::installing)
    }
}
