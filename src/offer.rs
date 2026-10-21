//! Offers to download something (like IntelliJ's "install plugin" notice) — a card at the bottom right of
//! the editing area, `y` download · `n` not now (normal mode, mouse too — insert mode is not intercepted).
//! While downloading it shows a wave; on failure, the reason + `y` retry · `n` close.
//! What gets downloaded is `What` — grammars (grammar.rs) · a tool a language needs, installed the way its
//! pack says (install.rs; `N` never offers that one again).
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
    /// More of its lines in the warning color: the warnings its data says (`cardwarn.rs` — a run card's, under
    /// what it asks, apart from `warning`).
    pub warnings: Vec<std::ops::Range<usize>>,
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
    /// Its yes is held, and why (a review's end or a bring while high-risk changes wait for a person's
    /// look — `codereview.rs`): `y` and `p` aren't among its answers, and pressed are refused on it.
    pub held: Option<String>,
    /// The changed lines' keys a held yes waits on (`risk::keys`): once the session has looked at every one,
    /// the hold goes (`codereview::held_sync`) — no asking again. Empty: held until answered otherwise.
    pub gate: Vec<crate::risk::Key>,
    /// The high-risk changes a bring's card names, in its order (`codereview::Held`): `h` goes to the first
    /// not looked at — in the review, or shown as the bring takes it when the review can't show it.
    pub held_changes: Vec<crate::codereview::Held>,
    /// It stepped aside for what its `h` went to (`codereview::card_highs`): up, but its letters are the code's
    /// again — `C-q` answers it; back when every change it held for has had a look.
    pub aside: bool,
    /// The front run card's group (cardburden 6): the cards stacked behind it asking for the very same run
    /// (`same_run`), by id — its last paragraph lists them, and `Y` answers them with it (`offer_group_sync`).
    pub like: Vec<u64>,
    /// The user's input count (`Editor.input_seq`) when it came (`push_offer`).
    pub came: u64,
    /// The user's own (`Offer::rank`, #205): a kind a command of theirs asks for, made by the input the user
    /// gave last (`Queue.asked` — the key, or a job it started; nothing typed since).
    pub yours: bool,
}

/// Where a card stands among the ones up (`Editor::offer_place`): the lower first, arrival order within.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Rank {
    /// Asked for by the user's own command — what they're waiting on.
    Yours,
    /// Claude's turn waits on it.
    Claudes,
    /// The rest: a grammar or a server to install, a repository to trust, … — never in front of a turn's.
    Other,
}

/// How the front run card's last paragraph starts when it lists the cards like it (`offer_group_sync`).
const LIKE_SAID: &str = "\n\n+";

/// Whether two run cards ask for the very same run (cardburden 6): the same command and folder exactly — the
/// card says "same command" (the review of 6: `queued`'s key collapses spaces, inside quotes too, and
/// `touch 'a  b'` isn't `touch 'a b'`) — the same run as `queued` keys it besides, and the same shown: what
/// each says it reads outside the project, and the files and scripts on it, text and all (nothing changed
/// between them).
pub fn same_run(root: &std::path::Path, a: &crate::tools::RunCard, b: &crate::tools::RunCard) -> bool {
    let key = |r: &crate::tools::RunCard| {
        let job = crate::tools::Job::Command(r.cmd.clone(), r.cwd.clone());
        crate::queued::key_of(root, &job, &crate::tools::Extra { grant: None, outside: false, base: None })
    };
    a.cmd == b.cmd && a.cwd == b.cwd && key(a) == key(b) && a.reads == b.reads && a.shown == b.shown
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
    /// No sandbox on this system, or a partial one (`tools.rs`): `y` runs Claude's command anyway, `s` its
    /// checks and reads unasked until the session ends, `n` tells Claude no. `first`: the session's card —
    /// its answer holds for the session's runs (`y` ask each time, `n` none — `Session.no_sandbox`).
    /// `ride`: what goes along to the run (`queued.rs`).
    NoSandbox {
        job: crate::tools::Job,
        call: crate::tools::Call,
        ride: Box<crate::queued::Ride>,
        kind: crate::tools::Unsandboxed,
        first: bool,
    },
    /// Claude's last run made protected names where none was (`made` — what Landlock can't refuse,
    /// `tools::ask_made`), before what runs next there (`then`: Claude's run, your task or preflight): `y`
    /// keeps them and runs it, `r` removes them first, `v` shows them, `n` doesn't run it.
    ProtectedMade { then: Box<crate::tools::MadeThen>, made: Vec<crate::snapshot::Made> },
    /// Review at the end (`edits.rs`): `y` lets Claude's edits go in as it makes them.
    ReviewAtEnd,
    /// Claude's Read, Grep or Glob outside the project (`reads.rs`): `y` once, `a` this chat, `s` this session,
    /// `A` always (those three only when `keep`), `n` no.
    /// `folder`: what `a`/`s`/`A` allow instead of the file — its folder, beside other reads (#102).
    Read { pending: Box<crate::reads::Pending>, keep: bool, folder: Option<PathBuf> },
    /// The user opens the sandbox for what it blocked in `run` (`blocked.rs`): `y` once (it runs again with
    /// it), `a` this chat, `s` this session, `A` always — as far as `Caps::storable` lets it last — and runs
    /// again — `n` no.
    Grant { grant: crate::allow::Grant, run: Box<crate::blocked::BlockedRun> },
    /// The user runs `run` again outside the sandbox, once (`blocked.rs`): `y` · `n`.
    Outside { run: Box<crate::blocked::BlockedRun> },
    /// The project's `.tarae.toml` sets what runs commands (`trust.rs`): `y` trusts it, `n` ignores that
    /// part. `hash` = what the card showed.
    ProjectTrust { hash: crate::trust::Digest },
    /// The project's Claude Code settings run commands or set Claude's environment (`claudetrust.rs`): `y`
    /// trusts them, `n` holds those keys back in the chat. `hash` = what the card showed.
    ClaudeSettings { hash: crate::trust::Digest },
    /// A repository's own git settings and hooks run commands (`gittrust.rs`): `y` lets them run, `n` keeps
    /// them off (held — `gitrepo`).
    GitTrust(Box<crate::gittrust::Ask>),
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
    /// Claude's rename, replace or edit across several files, or with a move or delete (`refactor.rs`, #75):
    /// `y` accepts them all, `r` reviews them one by one (not with `ops` — a move or delete among them, which
    /// no buffer shows), `v` shows every change, `n` declines them all. `generation` = its chat's.
    ChangeSet { generation: u64, ops: bool },
    /// Claude's `commit` (`committool.rs`): `y` commits exactly the tree the card shows on the HEAD it names, no
    /// hooks, `v` shows the whole diff, `n` tells Claude no.
    Commit(Box<crate::committool::Commit>),
    /// Ending a code review with files not viewed (`codereview.rs`, #10): `y` ends it, `n` keeps reviewing.
    /// Its session's, and that review's (`started`) only — #159.
    ReviewEnd { session: crate::event::SessionId, started: Instant },
    /// Bringing a session whose base moved since it left it (`rebase.rs`): `y` rebases it onto its base
    /// first, `n` leaves it.
    Rebase(Box<crate::rebase::Moved>),
    /// That rebase held back by its checkout — changes in no commit, buffers unsaved (`rebase.rs`, #188):
    /// `s` stashes the changes for it and puts them back after (nothing unsaved), `g` shows them in the git
    /// tab, `n` leaves it. No `y`: what it would do is the letter's.
    RebaseHeld(Box<crate::rebase::Held>),
    /// A session rebased cleanly (`rebase.rs`): `y` brings it into its base, `p` runs the preflight first and
    /// brings it once that passes, `n` not now.
    Rebased(Box<crate::rebase::Moved>),
    /// A rebase stopped at a conflict its Claude resolved (`rebase.rs`): `y` stages the files and goes on,
    /// `X` aborts the rebase, `n` leaves it stopped.
    RebaseResolved { root: PathBuf },
    /// A session with nothing to bring, behind its base (`rebase.rs`, #186): `y` moves it up to its base's tip
    /// (a fast-forward in its checkout), `n` leaves it.
    UpTo(Box<crate::rebase::Moved>),
    /// A git-tab action would run hooks from inside the checkout that its base doesn't have as they are
    /// (`hooks.rs`): `y` runs it as usual, `n` runs it with no hooks this time.
    Hooks(Box<crate::hooks::Ask>),
    /// `:restart` (`restart.rs`, #108): which checkout it builds in and runs, at what commit — `y` the one
    /// tarae started in, `f` the session in front's (when another), `n` none.
    Restart(Box<crate::restart::Choice>),
    /// The binary a restart would run is the one it was, older than its checkout's HEAD: `y` restarts onto
    /// it anyway, `n` doesn't.
    RestartSame { id: u64 },
    /// `:restart` again while restart `id` is under way (#194): `y` calls it off, `n` keeps it going.
    RestartCancel { id: u64 },
    /// Drafts (`notes.rs`) whose lines changed since they were pinned, at a turn's end (#197): `y` closes
    /// notes `ids`, `n` keeps them. Its session's.
    NotesStale { ids: Vec<u64> },
    /// Closing every note an author began (`:notes-clear <who>`, the notes list's `A-x`) — or every note, `by`
    /// None — when some hold your unsent drafts: `y` closes notes `ids` (the ones asked about), `n` keeps
    /// them. Its session's.
    NotesClose { by: Option<crate::notes::By>, ids: Vec<u64> },
    /// A buffer's unsaved edits a gone tarae kept (`recovery.rs`): `y` restores them into the buffer, `c`
    /// compares them with the file, `D` discards them, `n` leaves them for the next start.
    Recover(Box<crate::recovery::Found>),
    /// A run of Claude's printing nothing at next to no CPU for minutes (`stuck.rs`, #181): `y` stops it, `w`
    /// shows what its threads wait in (its output), `n` keeps waiting. Gone once it prints or ends.
    Stuck(Box<crate::stuck::Stuck>),
    /// A host a run's rules don't name, its connection held by tarae's proxy (`hosts.rs`): `y` once (this run),
    /// `s` this session, `A` always here (this project), `n` refused for this session. Gone with its run.
    Host(Box<crate::hosts::Ask>),
    /// Containers made on an engine while a stopped run of Claude's went, under names not its own
    /// (`containers.rs`): `y` stops them, by name · `n` leaves them running.
    Containers(Box<crate::containers::Ask>),
    /// `$` in the session list on a session with a conversation going (`billing.rs`): `y` switches its billing to
    /// `to`, the card having said what the next message costs · `n` keeps it. `money`: the key was refused for
    /// money (`Editor::money_out`) — the plan offered back, `y` this session · `n` stay.
    BillingSwitch { session: crate::event::SessionId, to: crate::billing::Billing, money: bool },
    /// The plan's weekly window refused a session's Claude (`billing.rs`): `y` that session on your key · `a` new
    /// sessions and the reviewer too, until the reset (when it's known) · `n` waits. Its session's.
    Limit(Box<crate::billing::LimitAsk>),
    /// `:features-reset` (`features.rs`): `y` empties the feature counts · `n` keeps them.
    FeaturesReset,
    /// The chat's claude has no login and tarae held a key of its environment back (`envkey.rs`): `y` Claude
    /// takes it until session `session` ends — `message`, the one that failed, goes again · `A` always (your
    /// config) · `n` not asked again this session.
    EnvKey { session: crate::event::SessionId, message: Option<String> },
    /// The user stopped a turn and the runs its calls waited on go on (`tools::left_running`): `y` stops them ·
    /// `n` lets them run. Light; it goes by itself once they've ended (`tools::left_sync`). Its session's.
    LeftRunning { runs: Vec<crate::tools::Left> },
}

/// Every card's kind (`Offer::kind`) — what `:features` lists, so a card never asked shows as 0
/// (`every_card_kind_is_listed` reads them from `kind`).
pub const KINDS: &[&str] = &[
    "grammars",
    "fetch",
    "plugin-permissions",
    "plugin-ask",
    "install",
    "run",
    "no-sandbox",
    "protected-made",
    "review-at-end",
    "read",
    "grant",
    "outside",
    "project-trust",
    "claude-settings",
    "git-trust",
    "screen",
    "preflight",
    "branch-delete",
    "force-push",
    "session-new",
    "session-end",
    "close-reviewed",
    "change-set",
    "commit",
    "review-end",
    "rebase",
    "rebase-held",
    "rebased",
    "rebase-resolved",
    "up-to",
    "hooks",
    "restart",
    "restart-same",
    "restart-cancel",
    "notes-stale",
    "notes-close",
    "recover",
    "stuck",
    "host",
    "containers",
    "billing-switch",
    "environment-key",
    "limit",
    "features-reset",
    "left-running",
];

/// What becomes of a card waiting at `:restart` (`restart.rs`). Claude's can't be there — the restart waits
/// for every turn, and their calls end with it.
#[derive(Debug, PartialEq, Eq)]
pub enum OverRestart {
    /// Asked again after it by the command that raised it (`:` line, run in its session): a fresh card, with
    /// fresh checks.
    Again(String),
    /// It comes back by itself — what raises it runs again in the new tarae (a grammar shown, a plugin
    /// loaded, the project's settings read, a copy found) — or it's the restart's own.
    ByItself,
    /// A moment's decision about something live: gone — named on the restart's card, to answer first.
    Lost,
}

impl What {
    pub fn over_restart(&self) -> OverRestart {
        use OverRestart::*;
        match self {
            What::Preflight { set: crate::preflight::Set::Quick, .. } => Again("preflight".into()),
            What::Preflight { set: crate::preflight::Set::Full, .. } => Again("preflight-full".into()),
            What::SessionEnd(e) => Again(format!("session-end {}", e.name)),
            What::Rebase(m) | What::Rebased(m) | What::UpTo(m) => Again(format!("session-bring {}", m.name)),
            What::BranchDelete { branch, .. } => Again(format!("git-branch-delete {branch}")),
            What::FeaturesReset => Again("features-reset".into()),
            What::Grammars { .. }
            | What::Fetch { .. }
            | What::Install { .. }
            | What::PluginPermissions { .. }
            | What::PluginAsk { .. }
            | What::ProjectTrust { .. }
            // (the chat reads the settings again when it opens)
            | What::ClaudeSettings { .. }
            | What::GitTrust(_)
            | What::Recover(_)
            | What::Restart(_)
            | What::RestartSame { .. }
            | What::RestartCancel { .. } => ByItself,
            What::ForcePush(_)
            | What::Hooks(_)
            | What::Grant { .. }
            | What::Outside { .. }
            | What::CloseReviewed { .. }
            | What::ReviewEnd { .. }
            | What::SessionNew(_)
            | What::RebaseResolved { .. }
            // (about the checkout as it was then — `:session-bring` looks at it afresh and raises it again)
            | What::RebaseHeld(_)
            // (Claude's: a run's job can't be run again blind — its call ends with the turn)
            | What::Run(_)
            | What::NoSandbox { .. }
            | What::ProtectedMade { .. }
            | What::ReviewAtEnd
            | What::Read { .. }
            | What::Screen { .. }
            | What::ChangeSet { .. }
            | What::Commit(_)
            | What::NotesStale { .. }
            | What::NotesClose { .. }
            | What::Stuck(_)
            // (its runs end with the restart)
            | What::LeftRunning { .. }
            // (about containers as they were then: they run on, the restart's tarae doesn't know them)
            | What::Containers(_)
            // (its run ends with the restart)
            | What::Host(_)
            // (about a conversation's cache as it was — `$` asks again, a refusal comes again)
            | What::BillingSwitch { .. }
            // (the chat after a restart fails again, and asks again: the answer isn't kept)
            | What::EnvKey { .. }
            | What::Limit(_) => Lost,
        }
    }
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
            // (one `also` raised: Claude's call waits on it — `tools::also_ask`)
            What::Grant { run, .. } if run.claude.is_some() => run.claude.clone().map(Asker::Tool),
            What::ProtectedMade { then, .. } => then.call().map(|c| Asker::Tool(c.clone())),
            // (one the user's pointing raised holds no call — `reads::point`; one of tarae's tools holds its call)
            What::Read { pending, .. } if pending.then.is_some() => {
                pending.then.as_ref().map(|t| Asker::Tool(t.call.clone()))
            }
            What::Read { pending, .. } if pending.own.is_none() => Some(Asker::Permission {
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
            warnings: Vec::new(),
            diff: None,
            since: Instant::now(),
            drawn: Default::default(),
            asked: None,
            held: None,
            gate: Vec::new(),
            held_changes: Vec::new(),
            aside: false,
            like: Vec::new(),
            came: 0,
            yours: false,
        }
    }

    /// Where it stands among the cards up (`Editor::offer_place`): the user's own (`yours` — #205: `space W`
    /// `n`'s card went behind Claude's), Claude's turn's, or the rest.
    pub fn rank(&self) -> Rank {
        match () {
            _ if self.yours => Rank::Yours,
            _ if self.claudes() => Rank::Claudes,
            _ => Rank::Other,
        }
    }

    /// Whether it's a kind only a command of the user's asks for (a key, a `:` line, a click — also through
    /// the work it starts) — not Claude's turn, nor nobody just now (a file shown, a plugin loaded, a turn's
    /// end, a task that ended). A grammar, a server or a debugger is only when it resumes what the user
    /// started (a launch, an attach). Its card is `yours` only when the input that asked is the last one.
    pub fn yours_kind(&self) -> bool {
        use crate::install::Then;
        match &self.what {
            What::Grammars { then, .. } => then.is_some(),
            What::Fetch { then, .. } | What::Install { then, .. } => matches!(then, Then::Resume(_)),
            // (one `also` raised is Claude's)
            What::Grant { run, .. } => run.claude.is_none(),
            w => matches!(
                w,
                What::Outside { .. }
                    | What::Preflight { .. }
                    | What::BranchDelete { .. }
                    | What::ForcePush(_)
                    | What::SessionNew(_)
                    | What::SessionEnd(_)
                    | What::CloseReviewed { .. }
                    | What::ReviewEnd { .. }
                    | What::Rebase(_)
                    | What::RebaseHeld(_)
                    | What::Rebased(_)
                    | What::UpTo(_)
                    | What::Hooks(_)
                    | What::Restart(_)
                    | What::RestartSame { .. }
                    | What::RestartCancel { .. }
                    | What::NotesClose { .. }
                    | What::BillingSwitch { .. }
                    | What::FeaturesReset
            ),
        }
    }

    /// What it asks, for its `card` line.
    pub fn asked(mut self, a: crate::cards::Asked) -> Offer {
        self.asked = Some(a);
        self
    }

    /// Its `card` line: answered (`y` `a` `A` `n`) or dropped — in `session`; its entry for the record back.
    fn said(
        &self,
        session: &crate::log::Session,
        answer: &str,
        caps: Option<&crate::caps::Caps>,
    ) -> serde_json::Value {
        crate::cards::said(session, self, answer, caps)
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
                | What::ProtectedMade { .. }
                | What::ReviewAtEnd
                | What::ProjectTrust { .. }
                | What::ClaudeSettings { .. }
                | What::GitTrust(_)
                | What::Read { .. }
                | What::Grant { .. }
                | What::Outside { .. }
                | What::Preflight { .. }
                | What::BranchDelete { .. }
                | What::ForcePush(_)
                | What::SessionNew(_)
                | What::SessionEnd(_)
                | What::ChangeSet { .. }
                | What::Commit(_)
                | What::Rebase(_)
                | What::RebaseHeld(_)
                | What::Rebased(_)
                | What::RebaseResolved { .. }
                | What::UpTo(_)
                | What::Hooks(_)
                | What::Restart(_)
                | What::RestartSame { .. }
                | What::Recover(_)
                | What::Stuck(_)
                | What::Host(_)
                | What::Containers(_)
                // (one `y` closes every draft it lists — cardburden 6: answered once read, its list as it is)
                | What::NotesStale { .. }
                // (its yes spends your credits)
                | What::BillingSwitch { .. }
                | What::EnvKey { .. }
                | What::Limit(_)
        )
    }

    /// Whether `a` answers it: the limit card whose window's reset is known (new sessions too, until then).
    pub fn limit_all(&self) -> bool {
        matches!(&self.what, What::Limit(a) if a.resets_at_ms.is_some())
    }

    /// Whether `$` steps it: a new session's card asking who pays for its Claude (the plan's weekly window is
    /// near its limit — `worktree::Making::billing_ask`).
    pub fn bills(&self) -> bool {
        matches!(&self.what, What::SessionNew(m) if m.billing_ask.is_some())
    }

    /// The host it asks about, if it's that card — or a raw connection's, or a lookup's (6e+).
    pub fn host(&self) -> Option<&crate::hosts::Ask> {
        match &self.what {
            What::Host(a) => Some(a),
            _ => None,
        }
    }

    /// What its `v` shows after its text, if it's the closed network's host card (`hosts::details`).
    pub fn host_details(&self) -> Option<String> {
        match &self.what {
            What::Host(a) if !a.open => Some(crate::hosts::details(a)),
            _ => None,
        }
    }

    /// Whether `s` answers it — for this session: a host's card, or a card that grants what may be kept that
    /// long (`top`).
    pub fn for_session(&self) -> bool {
        self.host().is_some()
            || self.top() >= Some(crate::allow::Scope::Session)
            || matches!(self.what, What::NoSandbox { .. })
    }

    /// For a card that grants (what the sandbox blocked, a read outside the project, a run reading there): the
    /// longest its yes may last — `Caps::storable` of what it grants (plan §2.3). None for the rest.
    pub fn top(&self) -> Option<crate::allow::Scope> {
        use crate::allow::Scope;
        match &self.what {
            What::Grant { grant, .. } => Some(crate::caps::Caps::storable(grant)),
            What::Read { keep, .. } => Some(if *keep { Scope::Always } else { Scope::Once }),
            What::Run(r) if r.keep => Some(Scope::Always),
            _ => None,
        }
    }

    /// The stuck run it asks about, if it's that card (`w` shows what it waits in).
    pub fn stuck(&self) -> Option<&crate::stuck::Stuck> {
        match &self.what {
            What::Stuck(s) => Some(s),
            _ => None,
        }
    }

    /// Whether `c` (compare) and `D` (discard) answer it: unsaved edits a gone tarae kept.
    pub fn recovers(&self) -> bool {
        matches!(self.what, What::Recover(_))
    }

    /// Whether `f` answers it: a restart's card with the session in front's checkout to build instead.
    pub fn restart_other(&self) -> bool {
        matches!(&self.what, What::Restart(c) if c.targets.len() > 1)
    }

    /// Whether `w` answers it: a restart's card naming work under way (it waits for it to end).
    pub fn restart_waits(&self) -> bool {
        matches!(&self.what, What::Restart(c) if !c.under_way.is_empty())
    }

    /// Whether `p` answers it: the bring card after a rebase (the preflight, then the bring).
    pub fn preflights(&self) -> bool {
        matches!(self.what, What::Rebased(_))
    }

    /// Whether `X` answers it: a resolved rebase's card (aborts the rebase).
    pub fn aborts(&self) -> bool {
        matches!(self.what, What::RebaseResolved { .. })
    }

    /// The held rebase it asks about, if it's that card (`s` stashes, `g` shows the changes).
    pub fn held(&self) -> Option<&crate::rebase::Held> {
        match &self.what {
            What::RebaseHeld(h) => Some(h),
            _ => None,
        }
    }

    /// Whether `s` answers it: a held rebase's card with nothing unsaved (its changes stashed for it).
    pub fn stashes(&self) -> bool {
        self.held().is_some_and(crate::rebase::Held::stashes)
    }

    /// Whether `key` is one of its answers that only a settled card in sight takes (`y`, and the letters
    /// that do something too).
    pub fn weighty_answer(&self, c: char) -> bool {
        matches!(c, 'y' | 'a' | 'A' | 'D' | 'R' | 'c')
            || (c == 'Y' && !self.like.is_empty())
            || (c == 'r' && self.made())
            || (c == 's' && (self.stashes() || self.for_session()))
            || (c == 'p' && self.preflights())
            || (c == 'X' && self.aborts())
            || (c == 'f' && self.restart_other())
            || (c == 'w' && self.restart_waits())
    }

    /// Its run, when it can be one of a group (cardburden 6): a run card still asking, in the sandbox — not
    /// outside it, no socket granted (Docker's is as much as outside), not where there's none or it's off
    /// (`unprotected`: "it runs with all your permissions" — the review of 6) — and warning of nothing.
    pub fn groupable(&self) -> Option<&crate::tools::RunCard> {
        match &self.what {
            What::Run(r)
                if matches!(self.state, OfferState::Asking)
                    && !r.outside
                    && r.socket.is_none()
                    && !r.unprotected
                    && r.warned.is_empty() =>
            {
                Some(r)
            }
            _ => None,
        }
    }

    /// The `(generation)` of a change set's card (`r` one by one, `v` every change).
    pub fn change_set(&self) -> Option<u64> {
        match self.what {
            What::ChangeSet { generation, .. } => Some(generation),
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
        // (Docker's socket is never the chat's: a run card's `a` is then its command's, when it has one — #193)
        let as_outside = |s: &std::path::Path| crate::allow::Grant::Socket(s.to_path_buf()).as_outside();
        match &self.what {
            What::Grant { .. } => self.top() >= Some(crate::allow::Scope::ThisChat),
            // (piece C: a host's `a` is this conversation's — `hosts::chat_ended`)
            What::Host(_) => true,
            What::Read { keep: true, .. } | What::Screen { .. } => true,
            // (what it reads outside, this command again, or its socket for the rest of the chat)
            What::Run(r) => {
                r.keep || r.again.is_some() || r.socket.as_deref().is_some_and(|s| !as_outside(s))
            }
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
            // (piece C: only a host list's — the network closed, always is the setting: cards.md decision 4)
            What::Host(a) => a.open,
            What::EnvKey { .. } => true,
            _ => self.lasts() && matches!(self.what, What::Read { .. }),
        }
    }

    /// Whether Claude's call waits on it — it goes with its session when another comes to the front (cardlost:
    /// an `also` card stayed up there, and `asking_sync` told its call it was lost while its `y` ran it).
    pub fn claudes(&self) -> bool {
        self.asker().is_some()
            || matches!(
                self.what,
                What::Run(_)
                    | What::NoSandbox { .. }
                    | What::Read { .. }
                    | What::Screen { .. }
                    | What::ChangeSet { .. }
                    | What::Commit(_)
            )
            || matches!(&self.what, What::ProtectedMade { then, .. } if then.call().is_some())
    }

    /// Whether it's the protected-made card (`r` removes them, `v` shows them).
    pub fn made(&self) -> bool {
        matches!(self.what, What::ProtectedMade { .. })
    }

    /// Whether it parks with its session when another comes to the front: Claude's, and a review's end
    /// (#159: it stayed up in the other session, and `y` there ended that one's review).
    pub fn goes_with_session(&self) -> bool {
        self.claudes()
            || matches!(
                self.what,
                What::ReviewEnd { .. }
                    | What::Stuck(_)
                    | What::LeftRunning { .. }
                    | What::NotesStale { .. }
                    | What::NotesClose { .. }
                    // (its session's: a key of another session's never answers it)
                    | What::Host(_)
                    | What::Containers(_)
                    // (its session's chat waits on it — answered in another, it'd answer nothing)
                    | What::ClaudeSettings { .. }
                    // (its session's conversation was refused)
                    | What::Limit(_)
            )
    }

    pub fn installing(&self) -> bool {
        matches!(self.state, OfferState::Installing(_))
    }

    /// Its answers as its hint row draws them, key and what it does — the card and the UI state read this
    /// one list (`uistate.rs`). None while it's downloading. `quick`: the project has a quick preflight set
    /// (the bring card's `p` runs that one).
    pub fn answers(&self, quick: bool) -> Vec<(char, &'static str)> {
        let mut v = self.answers_all(quick);
        // (the cards like it, listed on it: `Y` runs each — cardburden 6)
        if !self.like.is_empty() && matches!(self.state, OfferState::Asking) {
            v.insert(1.min(v.len()), ('Y', "All listed"));
        }
        if self.held.is_some() && matches!(self.state, OfferState::Asking) {
            v.retain(|a| !matches!(a.0, 'y' | 'p'));
            if self.highs().is_some() {
                v.insert(0, ('h', "Go to them"));
            }
        }
        v
    }

    /// Held for high-risk changes (#196): `h` goes to them — `Some(None)` in the review in front (its end
    /// card), `Some(Some((checkout, base)))` in a review since the base (a bring's). The card stays.
    pub fn highs(&self) -> Option<Option<(PathBuf, String)>> {
        self.held.as_ref()?;
        match &self.what {
            What::ReviewEnd { .. } => Some(None),
            What::Rebased(m) => Some(Some((m.root.clone(), m.base.clone()))),
            _ => None,
        }
    }

    fn answers_all(&self, quick: bool) -> Vec<(char, &'static str)> {
        use What as W;
        match self.state {
            OfferState::Installing(_) => return Vec::new(),
            OfferState::Failed(_) => return vec![('y', "Retry"), ('n', "Dismiss")],
            OfferState::Asking => {}
        }
        match &self.what {
            W::Grammars { .. } => vec![('y', "Install"), ('n', "Not now")],
            W::Install { .. } | W::Fetch { .. } => vec![('y', "Install"), ('n', "Not now"), ('N', "Never")],
            W::PluginPermissions { .. } => vec![('y', "Allow"), ('n', "Not now")],
            W::PluginAsk { .. } => vec![('y', "Allow"), ('n', "No")],
            // (one socket past the sandbox — #119; `a` is the socket's, for any command — #182; Docker's,
            // this command's with it, never the socket's — #193)
            W::Run(r) if r.socket.is_some() && self.lasts() => match r.again.is_some() {
                true => vec![('y', "Once"), ('a', "This command, this chat"), ('n', "No")],
                false => vec![('y', "Once"), ('a', "This socket, this chat"), ('n', "No")],
            },
            W::Run(r) if r.socket.is_some() => vec![('y', "Once"), ('n', "No")],
            W::Run(r) if r.keep => {
                vec![
                    ('y', "Once"),
                    ('a', "This chat"),
                    ('s', "This session"),
                    ('A', "Always here"),
                    ('n', "No"),
                ]
            }
            // (this command again for the rest of the chat — the card says which)
            W::Run(r) if r.again.is_some() => vec![('y', "Once"), ('a', "This chat"), ('n', "No")],
            W::Run(_) => vec![('y', "Run"), ('n', "No")],
            W::ReviewAtEnd => vec![('y', "Turn on"), ('n', "No")],
            W::ProjectTrust { .. } => vec![('y', "Trust"), ('n', "Hold back")],
            W::ClaudeSettings { .. } => vec![('y', "Trust"), ('v', "Details"), ('n', "Hold back")],
            W::GitTrust(_) => vec![('y', "Trust"), ('n', "Hold back")],
            W::NoSandbox { first: true, .. } => {
                vec![('y', "Ask each time"), ('s', "This session"), ('n', "Don't run them")]
            }
            W::NoSandbox { .. } => vec![('y', "Once"), ('s', "This session"), ('n', "No")],
            W::ProtectedMade { .. } => {
                vec![('y', "Keep, run"), ('r', "Remove, run"), ('v', "See them"), ('n', "No")]
            }
            W::Read { keep: true, .. } => {
                vec![
                    ('y', "Once"),
                    ('a', "This chat"),
                    ('s', "This session"),
                    ('A', "Always here"),
                    ('n', "No"),
                ]
            }
            W::Read { keep: false, .. } => vec![('y', "Once"), ('n', "No")],
            // (as the host's own card, the network closed: never kept past a session)
            W::Grant { grant: crate::allow::Grant::Host(..), .. } => {
                vec![('y', "Once"), ('a', "This chat"), ('s', "This session"), ('n', "No")]
            }
            // (as far as it may be kept: Docker's socket and a key this run's only, another socket this chat's)
            W::Grant { .. } => match self.top() {
                Some(crate::allow::Scope::Always) => vec![
                    ('y', "Once"),
                    ('a', "This chat"),
                    ('s', "This session"),
                    ('A', "Always here"),
                    ('n', "No"),
                ],
                Some(crate::allow::Scope::ThisChat) => {
                    vec![('y', "Once"), ('a', "This chat"), ('n', "No")]
                }
                _ => vec![('y', "Once"), ('n', "No")],
            },
            W::Outside { .. } => vec![('y', "Run"), ('n', "No")],
            W::Preflight { .. } => vec![('y', "Run them"), ('n', "Not now")],
            W::BranchDelete { .. } => vec![('y', "Delete"), ('n', "Keep it")],
            W::ForcePush(_) => vec![('y', "Push"), ('n', "No")],
            W::SessionNew(m) => {
                let mut v = vec![('y', "Make it")];
                if self.bills() {
                    v.push((
                        '$',
                        if m.billing == crate::billing::Billing::Api { "Bill plan" } else { "Bill API" },
                    ));
                }
                if self.base_steps() {
                    v.push(('b', "Other base"));
                }
                v.push(('n', "Not now"));
                v
            }
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
            W::RebaseHeld(_) if self.stashes() => {
                vec![('s', "Stash, rebase, put back"), ('g', "Git tab"), ('n', "Leave it")]
            }
            W::RebaseHeld(_) => vec![('g', "Git tab"), ('n', "Leave it")],
            // (`p` runs the quick set when the project has two — the card names its steps)
            W::Rebased(_) => {
                let p = if quick { "Quick preflight, then bring" } else { "Preflight, then bring" };
                vec![('y', "Bring"), ('p', p), ('n', "Not now")]
            }
            // (abort throws the resolution away: a capital, as removing does)
            W::RebaseResolved { .. } => vec![('y', "Continue"), ('X', "Abort"), ('n', "Not now")],
            W::UpTo(_) => vec![('y', "Move it up"), ('n', "Leave it")],
            W::Hooks(a) if a.runnable() => {
                vec![('y', if a.hooks.len() == 1 { "Run it" } else { "Run them" }), ('n', "Skip hooks")]
            }
            W::Hooks(_) => vec![('n', "Skip hooks")],
            // (work under way: `y` restarts anyway, ending it — `w` waits for it)
            W::Restart(c) => {
                let busy = !c.under_way.is_empty();
                let mut v = vec![('y', if busy { "Restart anyway" } else { "Restart" })];
                if self.restart_other() {
                    v.push(('f', "The front's instead"));
                }
                if busy {
                    v.push(('w', "Wait for them"));
                }
                v.push(('n', "Not now"));
                v
            }
            W::RestartSame { .. } => vec![('y', "Restart anyway"), ('n', "Not now")],
            W::RestartCancel { .. } => vec![('y', "Call it off"), ('n', "Keep it")],
            // (`r`: the notes list of just these — cardburden 6)
            W::NotesStale { .. } => vec![('y', "Close them"), ('r', "One by one"), ('n', "Keep them")],
            W::NotesClose { .. } => vec![('y', "Close them"), ('n', "Keep them")],
            // (throwing them away takes a capital, as removing does)
            W::Recover(_) => vec![('y', "Restore"), ('c', "Compare"), ('D', "Discard"), ('n', "Not now")],
            W::ChangeSet { ops: true, .. } => {
                vec![('y', "Accept all"), ('v', "See them"), ('n', "Decline all")]
            }
            W::ChangeSet { .. } => {
                vec![('y', "Accept all"), ('r', "One by one"), ('v', "See them"), ('n', "Decline all")]
            }
            W::Commit(_) => vec![('y', "Commit"), ('v', "See the diff"), ('n', "Don't")],
            W::Stuck(_) => vec![('y', "Stop it"), ('w', "What it waits on"), ('n', "Keep waiting")],
            W::LeftRunning { runs } if runs.len() == 1 => vec![('y', "Stop it"), ('n', "Let it run")],
            W::LeftRunning { .. } => vec![('y', "Stop them"), ('n', "Let them run")],
            // (`A` only with the network open: cards.md decision 4 — piece C)
            W::Host(a) if a.open => vec![
                ('y', "Once"),
                ('a', "This chat"),
                ('s', "This session"),
                ('A', "Always here"),
                ('n', "No"),
            ],
            // (the network closed: the wait and how long a yes lasts are behind `v` — `hosts::details`)
            W::Host(_) => {
                vec![('y', "Once"), ('a', "This chat"), ('s', "This session"), ('v', "Details"), ('n', "No")]
            }
            W::Containers(_) => vec![('y', "Stop them"), ('n', "Leave them")],
            W::BillingSwitch { money: true, .. } => vec![('y', "This session"), ('n', "Stay")],
            W::BillingSwitch { .. } => vec![('y', "Switch"), ('n', "Keep it")],
            W::EnvKey { .. } => vec![('y', "This session"), ('A', "Always"), ('n', "No")],
            W::Limit(_) if self.limit_all() => {
                vec![('y', "This session"), ('a', "New sessions too"), ('n', "Wait")]
            }
            W::Limit(_) => vec![('y', "This session"), ('n', "Wait")],
            W::FeaturesReset => vec![('y', "Empty them"), ('n', "Keep them")],
        }
    }

    /// Its kind, as the UI state names it (`uistate.rs`): the `What` in kebab case.
    pub fn kind(&self) -> &'static str {
        use What as W;
        match self.what {
            W::Grammars { .. } => "grammars",
            W::Fetch { .. } => "fetch",
            W::PluginPermissions { .. } => "plugin-permissions",
            W::PluginAsk { .. } => "plugin-ask",
            W::Install { .. } => "install",
            W::Run(_) => "run",
            W::NoSandbox { .. } => "no-sandbox",
            W::ProtectedMade { .. } => "protected-made",
            W::ReviewAtEnd => "review-at-end",
            W::Read { .. } => "read",
            W::Grant { .. } => "grant",
            W::Outside { .. } => "outside",
            W::ProjectTrust { .. } => "project-trust",
            W::ClaudeSettings { .. } => "claude-settings",
            W::GitTrust(_) => "git-trust",
            W::Screen { .. } => "screen",
            W::Preflight { .. } => "preflight",
            W::BranchDelete { .. } => "branch-delete",
            W::ForcePush(_) => "force-push",
            W::SessionNew(_) => "session-new",
            W::SessionEnd(_) => "session-end",
            W::CloseReviewed { .. } => "close-reviewed",
            W::ChangeSet { .. } => "change-set",
            W::Commit(_) => "commit",
            W::ReviewEnd { .. } => "review-end",
            W::Rebase(_) => "rebase",
            W::RebaseHeld(_) => "rebase-held",
            W::Rebased(_) => "rebased",
            W::RebaseResolved { .. } => "rebase-resolved",
            W::UpTo(_) => "up-to",
            W::Hooks(_) => "hooks",
            W::Restart(_) => "restart",
            W::RestartSame { .. } => "restart-same",
            W::RestartCancel { .. } => "restart-cancel",
            W::NotesStale { .. } => "notes-stale",
            W::NotesClose { .. } => "notes-close",
            W::Recover(_) => "recover",
            W::Stuck(_) => "stuck",
            W::LeftRunning { .. } => "left-running",
            W::Host(_) => "host",
            W::Containers(_) => "containers",
            W::BillingSwitch { .. } => "billing-switch",
            W::EnvKey { .. } => "environment-key",
            W::Limit(_) => "limit",
            W::FeaturesReset => "features-reset",
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
        self.drop_cards_for(gone, "dropped — its turn ended");
    }

    /// `drop_cards`, the gone ring told `why`.
    pub fn drop_cards_for(&mut self, gone: impl Fn(&What) -> bool, why: &str) {
        let front = !self.background();
        let session = self.session.log();
        let (mut dropped, mut kept) = (Vec::new(), Vec::new());
        for o in self.session.parked.iter().chain(self.offers.iter().filter(|_| front)) {
            if gone(&o.what) {
                kept.push(o.said(&session, "dropped", self.session.caps()));
                dropped.push(o.id);
            }
        }
        self.cards_record(kept);
        for id in dropped {
            self.card_how(id, why);
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
        // (the user's own: the input that asked is the last one given — a card that comes after more typing,
        // or from work nobody's input started, waits its turn: #205)
        o.came = self.input_seq;
        o.yours = o.yours_kind() && self.events.asked() == Some(self.input_seq);
        // (the call it holds waits on it — watched until it's answered: `asking_sync`)
        if let Some(asker) = o.asker() {
            self.session.asking.push(Asking { card: o.id, asker, what: o.title.clone() });
        }
        // From a session behind: it waits in that session, and comes up when the session is in front (`session_in`)
        if self.background() {
            self.count(crate::features::Area::Card, o.kind());
            // (still waiting a while later: one warning names it — `card_nudge`)
            self.nudge_later(self.session.id, o.id);
            return self.session.parked.push(o);
        }
        // (one holding a call isn't merged into another by its title — the other's call would be lost; nor is a
        // plugin's ask — the other's request would be: one asking the same joins its card, `plugin_ask`; nor a
        // repository's commands — a submodule's card would take its parent's place: one each, `git_trust_check`)
        // (a host's card is joined by `hosts::asked` itself — by its host, never by its title)
        let own = o.asker().is_some()
            || o.host().is_some()
            || matches!(o.what, What::PluginAsk { .. } | What::GitTrust(_));
        if let Some(old) = self.offers.iter_mut().find(|x| x.title == o.title).filter(|_| !own) {
            // Replaced whole — what it says with what it does — and it settles again before `y` counts
            if !old.installing() {
                (old.question, old.working, old.failed) = (o.question, o.working, o.failed);
                (old.decided, old.warning, old.asked, old.diff) = (o.decided, o.warning, o.asked, o.diff);
                old.warnings = o.warnings;
                (old.held, old.gate, old.held_changes, old.aside) = (o.held, o.gate, o.held_changes, false);
                (old.what, old.state) = (o.what, OfferState::Asking);
                old.rev += 1;
            }
            return;
        }
        // A plugin asking something: its panel gives the keys back, so they reach the card
        if matches!(o.what, What::PluginPermissions { .. } | What::PluginAsk { .. }) {
            self.plugin_panel_unfocus();
        }
        // (the chat keeps its keys, its input empty or not: a card takes them there only by `C-q` — frictions26:
        // a card that took them from an empty input took the `a` of a message typed next as a grant)
        // (asked: counted once it's stacked — one asked again while it's up only refreshed the one there, above)
        self.count(crate::features::Area::Card, o.kind());
        let at = self.offer_place(&o);
        self.offers.insert(at, o);
    }

    /// Where a new card goes among those up: behind every one that ranks with it or before it
    /// (`Offer::rank` — arrival order within a rank), so one the user's input just asked for comes in front of
    /// Claude's (#205) and a grammar's or a server's never in front of a turn's (#208). Never in front of the
    /// one at the front while it's being answered — its keys brought to it (`C-q`), its text open in its
    /// buffer — nor, for one of the user's, of a front card that came after the input that asked (it came while
    /// the work ran: maybe being read now). One that comes to the front settles again before `y` counts
    /// (`offer_settled`: by id).
    fn offer_place(&self, o: &Offer) -> usize {
        let came_since = |f: &Offer| o.yours && f.came >= o.came;
        let held = self
            .offers
            .first()
            .is_some_and(|f| self.card_keys || self.offer_viewing() || self.offer_bar() || came_since(f));
        let rank = o.rank();
        let behind = self.offers.iter().enumerate().skip(usize::from(held)).find(|(_, x)| x.rank() > rank);
        behind.map_or(self.offers.len(), |(i, _)| i)
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
                    // (refused there when a move or delete is among them: decided on the card only)
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
                        self.offer_view();
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
            Code::Char('Y') if !o.like.is_empty() => self.offer_accept_like(),
            // (the card stays: the list of its drafts over it, closed one by one there)
            Code::Char('r') if matches!(o.what, What::NotesStale { .. }) => crate::notes::stale_list(self),
            // (the card stays while it's read)
            Code::Char('v') if o.made() => crate::tools::made_view(self),
            // (its detail, kept off the card — it stands beside the work while Claude runs held back)
            Code::Char('v') if matches!(o.what, What::ClaudeSettings { .. }) => self.offer_view(),
            Code::Char('v') if o.host_details().is_some() => self.offer_view(),
            Code::Char('r') if o.made() => self.offer_accept_by('r'),
            Code::Char('n') => self.offer_dismiss(),
            Code::Char('N') if matches!(o.what, What::Install { .. } | What::Fetch { .. }) => {
                self.offer_never()
            }
            Code::Char('a') if o.limit_all() => self.offer_limit_all(),
            Code::Char('$') if o.bills() => self.offer_session_billing(),
            Code::Char('a') if o.lasts() => self.offer_for_chat(),
            Code::Char('A') if o.keeps() => self.offer_always(),
            Code::Char('D') if o.both() => self.offer_remove(true),
            Code::Char('p') if o.preflights() => self.offer_preflight_first(),
            Code::Char(c @ ('s' | 'g')) if (c == 's' && o.stashes()) || (c == 'g' && o.held().is_some()) => {
                self.offer_held(c)
            }
            Code::Char('s') if o.for_session() => self.offer_for_session(),
            Code::Char('a' | 'A') if matches!(o.what, What::NoSandbox { .. }) => {
                self.set_status("this card asks y, s or n")
            }
            Code::Char('a') if matches!(o.what, What::EnvKey { .. }) => {
                self.set_status("this card asks y, A or n")
            }
            Code::Char('a') if o.for_session() => self.set_status("this card asks y, s, A or n"),
            Code::Char('X') if o.aborts() => self.offer_abort(),
            Code::Char('b') if o.base_steps() => self.offer_session_base(),
            Code::Char('f') if o.restart_other() => self.offer_restart_other(),
            Code::Char('w') if o.restart_waits() => self.offer_restart_wait(),
            // (the card stays: what it waits on is yours to look at, then it's answered)
            Code::Char('h') if o.highs().is_some() => crate::codereview::card_highs(self),
            // (the card stays: shown, it's still to answer)
            Code::Char('w') if o.stuck().is_some() => {
                let s = o.stuck().cloned().expect("just seen");
                crate::stuck::show(self, &s);
            }
            // An answer letter this card doesn't take isn't the code's either (`a`/`A` there start inserting —
            // and the `y` after it was typed)
            Code::Char('a' | 'A') if o.lasts() || o.limit_all() => {
                self.set_status("this card asks y, a or n")
            }
            Code::Char('a' | 'A') if o.weighty() => self.set_status("this card asks only y or n"),
            _ => return false,
        }
        true
    }

    /// Downloads the front card — it moves to the back while downloading (the next offer comes forward).
    pub fn offer_accept(&mut self) {
        self.offer_accept_by('y');
    }

    /// `Y` on a run card listing the cards like it (cardburden 6): `y` on it, then on each card it listed that
    /// still asks for the same run — the front read whole first (`offer_key`: a weighty answer). Each its own
    /// run, as its own `y` would be (the same command again joins the one under way — `queued::join`); each
    /// its own `card` line, `Y`.
    pub fn offer_accept_like(&mut self) {
        let Some(front) = self.offers.first() else { return };
        if !self.offer_seen_whole() {
            return self.offer_see_all_first();
        }
        let (members, run) = (front.like.clone(), front.groupable().cloned());
        let root = self.session.root.clone();
        self.offer_accept_by('Y');
        for id in members {
            let still = |o: &Offer| {
                o.id == id && o.groupable().zip(run.as_ref()).is_some_and(|(a, b)| same_run(&root, a, b))
            };
            let Some(i) = self.offers.iter().position(still) else { continue };
            let o = self.offers.remove(i);
            self.offers.insert(0, o);
            self.offer_accept_by('Y');
        }
    }

    /// `offer_accept`, answered by `key` (its `card` line and how it went say it).
    fn offer_accept_by(&mut self, key: char) {
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
        // (a held rebase: what it'd do is the letter's — stash, or see them)
        if let Some(h) = self.offers[0].held() {
            let ask =
                if h.stashes() { "s stashes them for the rebase, g shows them" } else { "g shows them" };
            return self.offer_refused(format!("this card has no y — {ask}, n leaves it"));
        }
        // (a hook the card couldn't show isn't run on a yes)
        if matches!(&self.offers[0].what, What::Hooks(a) if !a.runnable()) {
            return self.set_status("a hook here can't be shown — n goes on without hooks");
        }
        // (a value too long to show isn't trusted)
        if matches!(&self.offers[0].what, What::GitTrust(a) if !a.showable()) {
            return self.offer_refused("a value here is too long to show — only n holds them back");
        }
        // (no restore over edits of yours, nor into a file still loading — said on the card, which stays)
        if let What::Recover(f) = &self.offers[0].what
            && let Some(why) = crate::recovery::refuses(self, f)
        {
            return self.offer_refused(why);
        }
        // (a yes held — high-risk changes no one has looked at: said on the card, which stays)
        if let Some(why) = self.offers[0].held.clone() {
            return self.offer_refused(why);
        }
        let mut o = self.offers.remove(0);
        self.offer_said(&o, key);
        o.state = OfferState::Installing(Instant::now());
        let (id, what) = (o.id, o.what.clone());
        self.offers.push(o);
        self.card_answered(id);
        match what {
            What::Grammars { names, .. } => {
                self.install_grammars(names, Some(id));
            }
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
            What::NoSandbox { job, call, ride, kind, first } => crate::tools::unprotected_answer(
                self,
                id,
                (job, call, *ride, kind, first),
                crate::tools::Unprotected::Once,
            ),
            What::ProtectedMade { then, made } => {
                let how = if key == 'r' {
                    crate::tools::MadeAnswer::Remove
                } else {
                    crate::tools::MadeAnswer::Keep
                };
                crate::tools::made_answer(self, id, *then, made, how)
            }
            What::ReviewAtEnd => {
                self.offer_finished(id, Ok(()));
                crate::edits::at_end_set(self, true);
            }
            What::ProjectTrust { hash } => self.project_trust_answer(id, hash, true),
            What::ClaudeSettings { hash } => self.claude_settings_answer(id, hash, true),
            What::GitTrust(ask) => self.git_trust_answer(id, *ask, true),
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
            What::ChangeSet { generation, .. } => crate::refactor::accept_all(self, id, generation),
            What::Commit(c) => crate::committool::answer(self, id, *c, true),
            What::ReviewEnd { session, started } => {
                self.offer_finished(id, Ok(()));
                crate::codereview::close_answered(self, session, started);
            }
            What::Rebase(m) => self.rebase_start(id, *m),
            // (refused above)
            What::RebaseHeld(_) => self.offer_finished(id, Ok(())),
            What::Rebased(m) => {
                self.offer_finished(id, Ok(()));
                self.rebase_bring(*m);
            }
            What::RebaseResolved { root } => {
                self.offer_finished(id, Ok(()));
                self.rebase_answer(&root, true);
            }
            What::UpTo(m) => self.rebase_move_up(id, *m),
            What::Hooks(ask) => self.hooks_answer(id, *ask, true),
            What::Restart(c) => crate::restart::chosen(self, id, *c, Some(0)),
            What::RestartSame { id: r } => crate::restart::same_answer(self, id, r, true),
            What::RestartCancel { id: r } => crate::restart::cancel_answer(self, id, r, true),
            What::NotesStale { ids } => crate::notes::stale_answer(self, id, ids, true),
            What::NotesClose { by, ids } => crate::notes::close_by_answer(self, id, by, ids, true),
            What::Recover(f) => crate::recovery::restore(self, id, *f),
            What::Stuck(s) => {
                self.offer_finished(id, Ok(()));
                crate::stuck::stop(self, &s);
            }
            What::LeftRunning { runs } => {
                self.offer_finished(id, Ok(()));
                crate::tools::left_stop(self, &runs);
            }
            What::Host(a) => crate::hosts::answer(self, id, *a, crate::sandbox::proxy::Scope::Run),
            What::Containers(a) => crate::containers::answer(self, id, *a, true),
            What::BillingSwitch { session, to, .. } => self.switch_answer(id, session, to),
            What::EnvKey { session, message } => {
                crate::envkey::answer(self, id, (session, message), crate::envkey::Answer::Session)
            }
            What::Limit(a) => self.limit_answer(id, *a, false),
            What::FeaturesReset => self.features_reset_answer(id, true),
        }
    }

    /// `a` on the limit card: its session on your key, and new sessions and the reviewer too, until the reset.
    fn offer_limit_all(&mut self) {
        if !self.offers.first().is_some_and(Offer::limit_all) {
            return;
        }
        let mut o = self.offers.remove(0);
        self.offer_answered(&o, 'a');
        o.state = OfferState::Installing(Instant::now());
        let (id, what) = (o.id, o.what.clone());
        self.offers.push(o);
        if let What::Limit(a) = what {
            self.limit_answer(id, *a, true);
        }
    }

    /// `$` on a new session's card that asks who pays: the other way — the card says so, and settles again.
    pub fn offer_session_billing(&mut self) {
        let Some(What::SessionNew(m)) = self.offers.first().map(|o| o.what.clone()) else { return };
        let mut m = *m;
        m.billing = match m.billing {
            crate::billing::Billing::Subscription => crate::billing::Billing::Api,
            crate::billing::Billing::Api => crate::billing::Billing::Subscription,
        };
        let card = self.session_card(&m);
        self.push_offer(card);
    }

    /// `s`: yes for this session — a host for every run of it, or what a grant's, a read's or a run's card
    /// opens, kept with the session (`Session.allowed`).
    pub fn offer_for_session(&mut self) {
        if !self.offers.first().is_some_and(Offer::for_session) {
            return;
        }
        let o = self.offers.remove(0);
        self.offer_said(&o, 's');
        self.card_answered(o.id);
        let session = Some(crate::allow::Scope::Session);
        match o.what {
            What::Host(a) => crate::hosts::answer(self, o.id, *a, crate::sandbox::proxy::Scope::Session),
            What::Grant { grant, run } => crate::blocked::grant_answer(self, o.id, grant, *run, session),
            What::Read { pending, keep, folder } => {
                crate::reads::answer(self, o.id, *pending, session, keep, folder)
            }
            What::Run(card) => crate::tools::run_answer(self, o.id, *card, session),
            What::NoSandbox { job, call, ride, kind, first } => crate::tools::unprotected_answer(
                self,
                o.id,
                (job, call, *ride, kind, first),
                crate::tools::Unprotected::ThisSession,
            ),
            _ => {}
        }
    }

    /// `D` on a card of unsaved edits a gone tarae kept: they're thrown away.
    pub fn offer_discard(&mut self) {
        if !self.offers.first().is_some_and(Offer::recovers) {
            return;
        }
        let o = self.offers.remove(0);
        self.offer_said(&o, 'D');
        self.card_answered(o.id);
        if let What::Recover(f) = o.what {
            crate::recovery::discard(self, *f);
        }
    }

    /// `w` on a restart's card naming work under way: it waits for that to end, then asks again.
    pub fn offer_restart_wait(&mut self) {
        if !self.offers.first().is_some_and(Offer::restart_waits) {
            return;
        }
        let o = self.offers.remove(0);
        self.offer_answered(&o, 'w');
        if let What::Restart(c) = o.what {
            crate::restart::wait_answer(self, o.id, *c);
        }
    }

    /// `f` on a restart's card: the session in front's checkout instead of the one tarae started in.
    pub fn offer_restart_other(&mut self) {
        if !self.offers.first().is_some_and(Offer::restart_other) {
            return;
        }
        let o = self.offers.remove(0);
        self.offer_answered(&o, 'f');
        if let What::Restart(c) = o.what {
            crate::restart::chosen(self, o.id, *c, Some(1));
        }
    }

    /// `p` on the bring card after a rebase: the preflight first, the bring once it passes.
    pub fn offer_preflight_first(&mut self) {
        if !self.offers.first().is_some_and(Offer::preflights) {
            return;
        }
        if let Some(why) = self.offers[0].held.clone() {
            return self.offer_refused(why);
        }
        let o = self.offers.remove(0);
        self.offer_answered(&o, 'p');
        if let What::Rebased(m) = o.what {
            self.rebase_preflight_then_bring(*m);
        }
    }

    /// `s` · `g` on a held rebase's card: its changes stashed for the rebase · shown in the git tab.
    pub fn offer_held(&mut self, c: char) {
        if self.offers.first().is_none_or(|o| o.held().is_none()) {
            return;
        }
        let o = self.offers.remove(0);
        self.offer_said(&o, c);
        self.card_answered(o.id);
        if let What::RebaseHeld(h) = o.what {
            match c {
                's' => self.rebase_held_stash(*h),
                _ => self.rebase_held_show(*h),
            }
        }
    }

    /// `X` on a resolved rebase's card: the rebase aborted.
    pub fn offer_abort(&mut self) {
        if !self.offers.first().is_some_and(Offer::aborts) {
            return;
        }
        let o = self.offers.remove(0);
        self.offer_answered(&o, 'X');
        if let What::RebaseResolved { root } = o.what {
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
        self.offer_said(&o, if both { 'D' } else { 'R' });
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
        let card = self.session_card(&m);
        self.push_offer(card);
    }

    /// `c` on a session's end card with work in no commit: committed on its branch first, then asked again.
    pub fn offer_commit_first(&mut self) {
        if !self.offers.first().is_some_and(Offer::commits_first) {
            return;
        }
        let o = self.offers.remove(0);
        self.offer_answered(&o, 'c');
        if let What::SessionEnd(e) = o.what {
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

    /// Every event: the front run card lists the cards stacked behind it that ask for the very same run
    /// (`same_run` — groupable each: no warning, nothing past the sandbox), and `Y` answers them with it
    /// (cardburden 6: parallel calls of one command stacked a card each, every one read and answered). Its
    /// last paragraph says which — `+2 more like it: card#52, card#53 — same command, unchanged` — and a list
    /// that changed makes it a new card to read (`rev`): `Y` waits until it's been seen as it is. No other card
    /// keeps a list.
    pub(crate) fn offer_group_sync(&mut self) {
        let root = self.session.root.clone();
        let want: Vec<u64> = match self.offers.first().and_then(Offer::groupable) {
            Some(front) => self.offers[1..]
                .iter()
                .filter(|o| o.groupable().is_some_and(|r| same_run(&root, front, r)))
                .map(|o| o.id)
                .collect(),
            None => Vec::new(),
        };
        for (i, o) in self.offers.iter_mut().enumerate() {
            let like = if i == 0 { want.clone() } else { Vec::new() };
            if o.like == like {
                continue;
            }
            if let Some(at) = o.question.rfind(LIKE_SAID).filter(|_| !o.like.is_empty()) {
                o.question.truncate(at);
            }
            if !like.is_empty() {
                let ids: Vec<String> = like.iter().map(|id| format!("card#{id}")).collect();
                o.question.push_str(&format!(
                    "{LIKE_SAID}{} more like it: {} — same command, unchanged",
                    like.len(),
                    ids.join(", ")
                ));
            }
            o.like = like;
            o.rev += 1;
        }
    }

    /// `v` on the front card: a change set's every change, the names a protected-made card is about, the Claude
    /// settings card's detail, else its whole text.
    pub fn offer_view(&mut self) {
        if self.offers.first().is_some_and(Offer::made) && self.offer_seen_whole() {
            return crate::tools::made_view(self);
        }
        if let Some(&What::ClaudeSettings { hash }) = self.offers.first().map(|o| &o.what) {
            return self.claude_settings_view(hash);
        }
        if let Some(more) = self.offers.first().and_then(Offer::host_details) {
            return self.offer_view_with(more);
        }
        if let Some(What::Commit(c)) = self.offers.first().map(|o| &o.what) {
            let more = crate::committool::view_text(&c.ask);
            return self.offer_view_with(more);
        }
        match self.offers.first().and_then(Offer::change_set) {
            Some(generation) => crate::refactor::view_all(self, generation),
            None => self.offer_view_all(),
        }
    }

    /// `r` on a change set's card: one by one — on a protected-made card: remove them (settled, as `y`).
    pub fn offer_one_by_one(&mut self) {
        if self.offers.first().is_some_and(Offer::made) {
            if self.offer_settled() {
                self.offer_accept_by('r');
            }
            return;
        }
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

    /// The front card is a one-row bar over the status line: its whole text (`v`) is the buffer in sight
    /// (`offer_reading` — U8: whole it covered the right half of the very text it's about), or it stepped aside
    /// for what its `h` went to (`Offer.aside` — whole it covered the code to look at).
    pub fn offer_bar(&self) -> bool {
        self.offer_reading() || self.offers.first().is_some_and(|o| o.aside)
    }

    /// The front card's whole text (`v`) is the buffer in sight — back whole once you leave it.
    pub fn offer_reading(&self) -> bool {
        self.offer_viewing() && self.offer_view.get().is_some_and(|v| v.2 == self.doc().id)
    }

    /// `v` on a card cut to fit — its whole text in a throwaway buffer (one per card title: an earlier one
    /// gives way), and the card stays to answer.
    pub fn offer_view_all(&mut self) {
        self.offer_view_with(String::new());
    }

    /// `offer_view_all`, `more` after the card's text (what a protected-made card's names hold).
    pub fn offer_view_with(&mut self, more: String) {
        let Some(o) = self.offers.first() else { return };
        let (id, rev, title) = (o.id, o.rev, o.title.clone());
        let text = format!("{title}\n\n{}\n{more}", o.question);
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
        // The card keeps the keys while it's read: the keys that read reach the buffer through it (`card_reads`),
        // its answers answer it. In its buffer those are the card's only while it holds them — Esc gives them
        // back, and then `y` is a yank, not a yes; `C-q` gives them again (frictions15 review)
        self.card_keys = true;
        // (in the focused pane now, not at the next event: its keys go through the card, past the code's
        // path that would put it there — `track_doc_switch`)
        self.sync_focus_view();
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
        self.offer_said(&o, if scope == crate::allow::Scope::Always { 'A' } else { 'a' });
        self.card_answered(o.id);
        match o.what {
            What::Read { pending, keep, folder } => {
                crate::reads::answer(self, o.id, *pending, Some(scope), keep, folder)
            }
            What::Run(card) => crate::tools::run_answer(self, o.id, *card, Some(scope)),
            What::Grant { grant, run } => crate::blocked::grant_answer(self, o.id, grant, *run, Some(scope)),
            What::Screen { call } => crate::seeing::answer(self, o.id, call, Some(scope)),
            What::Host(a) if scope == crate::allow::Scope::Always => {
                crate::hosts::answer(self, o.id, *a, crate::sandbox::proxy::Scope::Project)
            }
            What::Host(a) => crate::hosts::answer(self, o.id, *a, crate::sandbox::proxy::Scope::Chat),
            What::EnvKey { session, message } if scope == crate::allow::Scope::Always => {
                crate::envkey::answer(self, o.id, (session, message), crate::envkey::Answer::Always)
            }
            _ => {}
        }
    }

    /// `N` on an install: closes it, and it isn't offered again.
    pub fn offer_never(&mut self) {
        let Some(o) = self.offers.first().filter(|o| !o.installing()) else { return };
        let (What::Install { tool, .. } | What::Fetch { tool, .. }) = &o.what else { return };
        let tool = tool.clone();
        let o = self.offers.remove(0);
        self.offer_answered(&o, 'N');
        self.install_never(&tool);
    }

    /// Card `o`, taken off by key `key` (one of its own letters, past `y` `n`): its `card` line, how it went
    /// for the gone ring, and its call told it's answered.
    fn offer_answered(&mut self, o: &Offer, key: char) {
        self.offer_said(o, key);
        self.card_answered(o.id);
    }

    /// Card `o` answered by key `key`: its `card` line, how it went for the gone ring, its answer counted
    /// (`features.rs`) — every answer goes through here.
    fn offer_said(&mut self, o: &Offer, key: char) {
        let entry = o.said(&self.session.log(), &key.to_string(), self.session.caps());
        self.cards_record(vec![entry]);
        self.card_how(o.id, format!("answered {key}"));
        if self.counting() {
            self.features.answer(o.kind(), key);
        }
    }

    pub fn offer_dismiss(&mut self) {
        if self.offers.first().is_none_or(Offer::installing) {
            return;
        }
        let o = self.offers.remove(0);
        self.offer_said(&o, 'n');
        self.card_answered(o.id);
        match o.what {
            What::Grammars { lang, names, .. } => {
                let first = names.first().cloned().unwrap_or_default();
                self.set_status(format!(
                    "No colors for {lang} until you quit — :grammar-install {first} anytime"
                ));
            }
            What::PluginPermissions { plugin, permissions } => {
                self.plugin_permissions_answer(plugin, permissions, false)
            }
            What::PluginAsk { plugin, asks } => self.plugin_ask_answer(plugin, asks, false),
            What::Install { tool, .. } | What::Fetch { tool, .. } => {
                self.set_status(format!("Not installing {tool} for now (N on its card: never ask)"))
            }
            What::Run(card) => crate::tools::run_answer(self, o.id, *card, None),
            What::NoSandbox { job, call, ride, kind, first } => crate::tools::unprotected_answer(
                self,
                o.id,
                (job, call, *ride, kind, first),
                crate::tools::Unprotected::No,
            ),
            What::ProtectedMade { then, made } => {
                crate::tools::made_answer(self, o.id, *then, made, crate::tools::MadeAnswer::No)
            }
            What::ReviewAtEnd => self.set_status("Claude's edits come one at a time"),
            What::ProjectTrust { hash } => self.project_trust_answer(o.id, hash, false),
            What::ClaudeSettings { hash } => self.claude_settings_answer(o.id, hash, false),
            What::GitTrust(ask) => self.git_trust_answer(o.id, *ask, false),
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
            What::ChangeSet { generation, .. } => crate::refactor::decline_all(self, generation),
            What::Commit(c) => crate::committool::answer(self, o.id, *c, false),
            What::ReviewEnd { .. } => self.set_status("still reviewing — space v m marks a file viewed"),
            What::Rebase(m) => {
                self.set_status(format!("Not rebased — {} as it is, nothing brought", m.branch))
            }
            What::RebaseHeld(h) => {
                self.set_status(format!("Not rebased — {} as it is, nothing brought", h.moved.branch))
            }
            What::Rebased(m) => {
                self.set_status(format!("Not brought — :session-bring {} when you're ready", m.name))
            }
            What::RebaseResolved { .. } => {
                self.set_status("the rebase waits — its card is in the inbox (space I), or :rebase-continue")
            }
            What::UpTo(m) => self.set_status(format!("Left {} where it is, behind {}", m.name, m.base)),
            What::Hooks(ask) => self.hooks_answer(o.id, *ask, false),
            What::Restart(c) => crate::restart::chosen(self, o.id, *c, None),
            What::RestartSame { id } => crate::restart::same_answer(self, o.id, id, false),
            What::RestartCancel { id } => crate::restart::cancel_answer(self, o.id, id, false),
            What::NotesStale { ids } => crate::notes::stale_answer(self, o.id, ids, false),
            What::NotesClose { by, ids } => crate::notes::close_by_answer(self, o.id, by, ids, false),
            What::Recover(f) => crate::recovery::not_now(self, *f),
            What::Stuck(s) => crate::stuck::keep_waiting(self, &s),
            What::LeftRunning { .. } => {
                self.set_status("left running — space m c stops the task, space x c the tests")
            }
            What::Host(a) => crate::hosts::answer(self, o.id, *a, crate::sandbox::proxy::Scope::Refused),
            What::Containers(a) => crate::containers::answer(self, o.id, *a, false),
            What::BillingSwitch { session, to, .. } => {
                let name = std::iter::once(&self.session)
                    .chain(&self.sessions)
                    .find(|l| l.id == session)
                    .map_or_else(String::new, |l| l.name.clone());
                let stays = match to {
                    crate::billing::Billing::Api => "on your plan",
                    crate::billing::Billing::Subscription => "on your API key",
                };
                self.set_status(format!("{name} stays {stays}"));
            }
            What::EnvKey { session, message } => {
                crate::envkey::answer(self, o.id, (session, message), crate::envkey::Answer::No)
            }
            What::Limit(a) => self.limit_wait(&a),
            What::FeaturesReset => self.features_reset_answer(o.id, false),
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
