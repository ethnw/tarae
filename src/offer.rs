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
}

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
    /// A plugin asks for permissions (when it loads): `y` approves the list, kept.
    PluginPermissions { plugin: String, permissions: Vec<crate::plugin::PluginPermission> },
    /// A plugin asks to connect somewhere new, or read a file outside its patterns: `y` allows it, kept.
    PluginAsk { plugin: String, ask: Box<crate::plugin::PluginAsk> },
    /// A language server or debugger — `run` in the task panel (in `cwd`), then `then`.
    Install { tool: String, run: Vec<String>, cwd: PathBuf, then: crate::install::Then },
    /// A command Claude wants to run (`tools.rs`): `y` runs it in the task panel, `n` tells Claude no.
    /// `shown` = files it names that Claude wrote unseen (review at the end), on the card as they were ·
    /// `unprotected` = the card said there's no sandbox (`sandbox.rs`).
    /// `reads` = places outside the project a read-only command reads — `a`/`A` allow them for longer too.
    Run(Box<crate::tools::RunCard>),
    /// No sandbox on this system (`tools.rs`): `y` runs Claude's command anyway, `a` does for the rest of
    /// the conversation, `n` tells Claude no.
    NoSandbox { job: crate::tools::Job, call: crate::tools::Call },
    /// Review at the end (`edits.rs`): `y` lets Claude's edits go in as it makes them.
    ReviewAtEnd,
    /// Claude's Read, Grep or Glob outside the project (`reads.rs`): `y` once, `a` this chat, `A` always
    /// (those two only when `keep`), `n` no.
    Read { pending: Box<crate::reads::Pending>, keep: bool },
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
    /// `:preflight`'s first run in a project with the packs' default steps (`preflight.rs`): `y` runs them.
    Preflight { root: PathBuf, steps: Vec<crate::preflight::Step> },
    /// A branch with commits on no other (`gitpanel.rs`): `y` deletes it, if still at `tip`.
    BranchDelete { top: PathBuf, branch: String, tip: String },
    /// A push over the remote's commits (`gitpanel.rs`): `y` pushes with a lease on what the card listed.
    ForcePush(Box<crate::gitpanel::ForcePush>),
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
        }
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
        )
    }

    /// Whether its answer can last longer than once (`a` this chat · `A` always — `A` only for reads).
    pub fn lasts(&self) -> bool {
        match &self.what {
            What::NoSandbox { .. }
            | What::Read { keep: true, .. }
            | What::Grant { .. }
            | What::Screen { .. } => true,
            // (what it reads outside, or its pattern for the rest of the chat)
            What::Run(r) => r.keep || r.pattern.is_some(),
            _ => false,
        }
    }

    /// Whether `A` (always) answers it: what it allows is kept for the project — a run card's reads, never
    /// its pattern.
    pub fn keeps(&self) -> bool {
        match &self.what {
            What::Run(r) => r.keep,
            _ => self.lasts() && matches!(self.what, What::Read { .. } | What::Grant { .. }),
        }
    }

    /// Whether Claude's call waits on it — it goes with its lane when another comes to the front.
    pub fn claudes(&self) -> bool {
        matches!(self.what, What::Run(_) | What::NoSandbox { .. } | What::Read { .. } | What::Screen { .. })
    }

    pub fn installing(&self) -> bool {
        matches!(self.state, OfferState::Installing(_))
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

    /// Stacks an offer (at the back). If one with the same title exists, keeps it (untouched while
    /// downloading; only its content is refreshed while asking).
    pub fn push_offer(&mut self, mut o: Offer) {
        // From a lane behind: it waits in that lane, and comes up when the lane is in front (`lane_in`)
        if self.background() {
            self.offer_seq += 1;
            o.id = self.offer_seq;
            return self.lane.parked.push(o);
        }
        if let Some(old) = self.offers.iter_mut().find(|x| x.title == o.title) {
            // Replaced whole — what it says with what it does — and it settles again before `y` counts
            if !old.installing() {
                (old.question, old.working, old.failed) = (o.question, o.working, o.failed);
                (old.decided, old.warning) = (o.decided, o.warning);
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
            && let Some(c) = self.lane.chat.as_mut().filter(|c| c.focused && c.input.is_empty())
        {
            c.focused = false;
        }
        self.offer_seq += 1;
        o.id = self.offer_seq;
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
            if !self.screen.offer_buttons.iter().any(|b| b.3 == 'y') {
                return false;
            }
            if !self.offer_settled() && matches!(key.code, Code::Char('y' | 'n' | 'N' | 'a' | 'A')) {
                return true;
            }
            // Cut to fit the screen: `v` shows it all, and `y` waits for that (not taken otherwise — it's
            // select mode in the code)
            if !self.offer_seen_whole() {
                match key.code {
                    Code::Char('v') => {
                        self.offer_view_all();
                        return true;
                    }
                    Code::Char('y' | 'a' | 'A') => {
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
            Code::Char('y') => self.offer_accept(),
            Code::Char('n') => self.offer_dismiss(),
            Code::Char('N') if matches!(o.what, What::Install { .. }) => self.offer_never(),
            Code::Char('a') if o.lasts() => self.offer_for_chat(),
            Code::Char('A') if o.keeps() => self.offer_always(),
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
        let mut o = self.offers.remove(0);
        o.state = OfferState::Installing(Instant::now());
        let (id, what) = (o.id, o.what.clone());
        self.offers.push(o);
        match what {
            What::Grammars { names, .. } => {
                self.install_grammars(names, Some(id));
            }
            What::JavaDebug { .. } => self.java_debug_download(id),
            What::Install { tool, run, cwd, then } => self.install_start(id, tool, run, cwd, then),
            What::PluginPermissions { plugin, permissions } => {
                self.offer_finished(id, Ok(()));
                self.plugin_permissions_answer(plugin, permissions, true);
            }
            What::PluginAsk { plugin, ask } => {
                self.offer_finished(id, Ok(()));
                self.plugin_ask_answer(plugin, *ask, true);
            }
            What::Run(card) => crate::tools::run_answer(self, id, *card, Some(crate::allow::Scope::Once)),
            What::NoSandbox { job, call } => {
                crate::tools::unprotected_answer(self, id, job, call, crate::tools::Unprotected::Once)
            }
            What::ReviewAtEnd => {
                self.offer_finished(id, Ok(()));
                crate::edits::at_end_set(self, true);
            }
            What::ProjectTrust { hash } => self.project_trust_answer(id, hash, true),
            What::Read { pending, keep } => {
                crate::reads::answer(self, id, *pending, Some(crate::allow::Scope::Once), keep)
            }
            What::Grant { grant, run } => {
                crate::blocked::grant_answer(self, id, grant, *run, Some(crate::allow::Scope::Once))
            }
            What::Outside { run } => crate::blocked::outside_answer(self, id, *run, true),
            What::Screen { call } => crate::seeing::answer(self, id, call, Some(crate::allow::Scope::Once)),
            What::Preflight { root, steps } => self.preflight_answer(id, root, steps, true),
            What::BranchDelete { top, branch, tip } => self.git_branch_delete_answer(id, top, branch, tip),
            What::ForcePush(f) => self.git_force_push(id, *f),
        }
    }

    /// Whether the front card has been in sight whole — drawn uncut, or opened with `v` — as it is now.
    pub fn offer_seen_whole(&self) -> bool {
        self.offers.first().is_some_and(|o| self.offer_whole.get() == Some((o.id, o.rev)))
    }

    /// `y` on a card cut to fit: not an answer yet.
    pub fn offer_see_all_first(&mut self) {
        self.set_status(match self.offer_viewing() {
            true => "read it to its end first — G in its buffer",
            false => "see all of it first — v shows the whole card",
        });
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
        let name = format!("card · {title}");
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
        // (seen whole once its last line has been on screen — `term::render_frame` looks)
        self.offer_view.set(Some((id, rev, doc)));
        // (the card's bar under it says so — U8)
        self.note("the card's whole text — G to its end, then y answers");
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
        match o.what {
            What::NoSandbox { job, call } => {
                crate::tools::unprotected_answer(self, o.id, job, call, crate::tools::Unprotected::ThisChat)
            }
            What::Read { pending, keep } => crate::reads::answer(self, o.id, *pending, Some(scope), keep),
            What::Run(card) => crate::tools::run_answer(self, o.id, *card, Some(scope)),
            What::Grant { grant, run } => crate::blocked::grant_answer(self, o.id, grant, *run, Some(scope)),
            What::Screen { call } => crate::seeing::answer(self, o.id, call, Some(scope)),
            _ => {}
        }
    }

    /// `N` on an install: closes it, and it isn't offered again.
    pub fn offer_never(&mut self) {
        let Some(o) = self.offers.first().filter(|o| !o.installing()) else { return };
        let What::Install { tool, .. } = &o.what else { return };
        let tool = tool.clone();
        self.offers.remove(0);
        self.install_never(&tool);
    }

    pub fn offer_dismiss(&mut self) {
        if self.offers.first().is_none_or(Offer::installing) {
            return;
        }
        let o = self.offers.remove(0);
        match o.what {
            What::Grammars { lang, names, .. } => {
                let first = names.first().cloned().unwrap_or_default();
                self.set_status(format!(
                    "No colors for {lang} this session — :grammar-install {first} anytime"
                ));
            }
            What::JavaDebug { .. } => self.set_status("Java debugging needs java-debug — F5 asks again"),
            What::PluginPermissions { plugin, permissions } => {
                self.plugin_permissions_answer(plugin, permissions, false)
            }
            What::PluginAsk { plugin, ask } => self.plugin_ask_answer(plugin, *ask, false),
            What::Install { tool, .. } => {
                self.set_status(format!("Not installing {tool} for now (N on its card: never ask)"))
            }
            What::Run(card) => crate::tools::run_answer(self, o.id, *card, None),
            What::NoSandbox { job, call } => {
                crate::tools::unprotected_answer(self, o.id, job, call, crate::tools::Unprotected::No)
            }
            What::ReviewAtEnd => self.set_status("Claude's edits come one at a time"),
            What::ProjectTrust { hash } => self.project_trust_answer(o.id, hash, false),
            What::Read { pending, keep } => crate::reads::answer(self, o.id, *pending, None, keep),
            What::Grant { grant, run } => crate::blocked::grant_answer(self, o.id, grant, *run, None),
            What::Outside { run } => crate::blocked::outside_answer(self, o.id, *run, false),
            What::Screen { call } => crate::seeing::answer(self, o.id, call, None),
            What::Preflight { root, steps } => self.preflight_answer(o.id, root, steps, false),
            What::BranchDelete { branch, .. } => self.set_status(format!("Kept {branch}")),
            What::ForcePush(f) => self.set_status(format!("Not pushed — {} as it was", f.upstream())),
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
