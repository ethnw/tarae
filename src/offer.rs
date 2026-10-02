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
    Run { cmd: String, call: crate::tools::Call },
    /// Review at the end (`edits.rs`): `y` lets Claude's edits go in as it makes them.
    ReviewAtEnd,
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
        }
    }

    /// Whether answering it runs something (an install, a plugin's permissions).
    pub fn weighty(&self) -> bool {
        matches!(
            self.what,
            What::Install { .. }
                | What::PluginPermissions { .. }
                | What::PluginAsk { .. }
                | What::Run { .. }
                | What::ReviewAtEnd
        )
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
        if let Some(old) = self.offers.iter_mut().find(|x| x.title == o.title) {
            // Replaced whole — what it says with what it does — and it settles again before `y` counts
            if !old.installing() {
                (old.question, old.working, old.failed) = (o.question, o.working, o.failed);
                (old.what, old.state) = (o.what, OfferState::Asking);
                old.rev += 1;
            }
            return;
        }
        // A plugin asking something: its panel gives the keys back, so they reach the card
        if matches!(o.what, What::PluginPermissions { .. } | What::PluginAsk { .. }) {
            self.plugin_panel_unfocus();
        }
        self.offer_seq += 1;
        o.id = self.offer_seq;
        self.offers.push(o);
    }

    /// Normal-mode keys while an offer is shown — true if handled
    /// (`y` download · `n`/Esc dismiss · `N` never, for an install, on the front card).
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
            if !self.offer_settled() && matches!(key.code, Code::Char('y' | 'n' | 'N') | Code::Esc) {
                return true;
            }
        }
        match key.code {
            Code::Char('y') => self.offer_accept(),
            Code::Char('n') | Code::Esc => self.offer_dismiss(),
            Code::Char('N') if matches!(o.what, What::Install { .. }) => self.offer_never(),
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
            What::Run { cmd, call } => crate::tools::run_answer(self, id, cmd, call, true),
            What::ReviewAtEnd => {
                self.offer_finished(id, Ok(()));
                crate::edits::at_end_set(self, true);
            }
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
            What::Run { cmd, call } => crate::tools::run_answer(self, o.id, cmd, call, false),
            What::ReviewAtEnd => self.set_status("Claude's edits come one at a time"),
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
