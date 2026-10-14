//! What's on screen, as data (`docs/design/ui-state.md`, phase 1): where the keys go, the mode, the prompt,
//! the sessions, the cards (up, stacked, parked), the toasts — read from the same functions that route keys
//! and draw, never redone. JSON for drivers and scripts (`uisocket.rs`, `tarae state --json`), a short text
//! form for people and Claude, the structs themselves in tests. Built only when asked: nobody asking, nothing
//! runs.
//!
//! **Where the keys go** is `Editor::keys_to`: `handle_key`'s chain as data — one `Layer` per handler, in
//! its order, each with the keys it takes. The routing test holds the two together (every handler records
//! that it took a key under `cfg(test)`, and that must be the layer `KeysTo::route` names), and the status
//! line reads it (`Editor::keys_at`).

use serde_json::{Value, json};

use crate::editor::{Editor, Mode};
use crate::key::{Code, Key};
use crate::sizes::Slot;

/// The schema's version: a field removed, renamed or meaning something else bumps it; one added doesn't.
/// 2: `panels.edit_review.by` went (an edit's review is always Claude's); since added: `sessions[].sandbox`, what
/// its runs' sandbox keeps apart and shares (keeper 9d: `Guarantees::json`; null before its first sandboxed run).
pub const V: u64 = 2;

/// A card's `asks` in the one-line forms (the text form, a watch's lines): cut here — the JSON has it whole.
const ASKS_MAX: usize = 300;

// ── Where the keys go ────────────────────────────────────────────────────

/// A place that takes keys — one handler of `handle_key`'s chain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    Code,
    Prompt,
    Picker,
    Chat,
    /// The chat minimized while Claude works: a key that would type there brings it back instead
    /// (`chat::unfold_key`).
    Unfold,
    Side(crate::sidebar::Mode),
    /// The code review's file rail.
    Rail,
    /// An edit's review in the buffer (`llm::review_key`).
    EditReview,
    /// The bottom slot with tarae's keys: its panel's own (the terminal's: `i` types in it).
    Slot(Slot),
    /// The terminal's program: every key goes to it.
    Program,
    /// Card `id`: its answers (`offer_key`), or every key once `C-q` gave it them (`card_key`).
    Card(u64),
    /// `C-q` to the one that has waited longest, elsewhere (`inbox.rs`).
    NextCard,
    /// Floating text (hover): Esc closes it, `C-d`/`C-u` scroll it.
    Popup,
    /// Labels up for pointing at an element (`point.rs`): letters pick one, Esc stops.
    Point,
}

impl Region {
    /// Its name in the state (`region`).
    pub fn name(self) -> String {
        match self {
            Region::Code => "code".into(),
            Region::Prompt => "prompt".into(),
            Region::Picker => "picker".into(),
            Region::Chat => "chat".into(),
            Region::Unfold => "chat-minimized".into(),
            Region::Side(m) => format!("side:{}", side_tab(m)),
            Region::Rail => "review".into(),
            Region::EditReview => "edit-review".into(),
            Region::Slot(s) => format!("slot:{}", slot_name(s)),
            Region::Program => "terminal-program".into(),
            Region::Card(_) => "card".into(),
            Region::NextCard => "next-card".into(),
            Region::Popup => "popup".into(),
            Region::Point => "point".into(),
        }
    }

    /// The handler of `handle_key`'s chain that takes its keys (the routing test compares these).
    #[cfg(test)]
    pub fn handler(self) -> Handler {
        match self {
            Region::Code => Handler::Code,
            Region::Prompt => Handler::Prompt,
            Region::Picker => Handler::Picker,
            Region::Chat => Handler::Chat,
            Region::Unfold => Handler::Unfold,
            Region::Side(_) => Handler::Side,
            Region::Rail => Handler::Rail,
            Region::EditReview => Handler::EditReview,
            Region::Slot(Slot::Procs) => Handler::Procs,
            Region::Slot(Slot::Plugin) => Handler::PluginPanel,
            Region::Slot(_) => Handler::Bottom,
            Region::Program => Handler::Program,
            Region::Card(_) => Handler::Card,
            Region::NextCard => Handler::NextCard,
            Region::Popup => Handler::Popup,
            Region::Point => Handler::Point,
        }
    }
}

/// The side panel's tab as the state names it.
pub(crate) fn side_tab(m: crate::sidebar::Mode) -> &'static str {
    use crate::sidebar::Mode as S;
    match m {
        S::Files => "files",
        S::Outline => "outline",
        S::List => "list",
        S::Git => "git",
        S::Plugin => "plugin",
    }
}

fn slot_name(s: Slot) -> &'static str {
    match s {
        Slot::Terminal => "terminal",
        Slot::Test => "tests",
        Slot::Task => "task",
        Slot::Procs => "procs",
        Slot::Debug => "debug",
        Slot::Plugin => "plugin",
    }
}

/// Which handler of `handle_key`'s chain took a key — recorded under `cfg(test)` (`took`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handler {
    Point,
    Card,
    NextCard,
    Program,
    Popup,
    EditReview,
    Picker,
    Unfold,
    Chat,
    Rail,
    Side,
    Procs,
    PluginPanel,
    Bottom,
    Prompt,
    Code,
}

#[cfg(test)]
thread_local!(pub static TOOK: std::cell::Cell<Option<Handler>> = const { std::cell::Cell::new(None) });

/// `handle_key`'s handler `h` took the key (recorded in tests only — the routing test reads it).
#[inline]
pub fn took(h: Handler) {
    #[cfg(test)]
    TOOK.set(Some(h));
    #[cfg(not(test))]
    let _ = h;
}

/// What a region does with a letter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Takes {
    /// Commands: normal mode, the panels.
    Commands,
    /// Letters become text: insert mode, the chat's input, a prompt, a picker's query.
    Text,
    /// Everything goes to the terminal's program.
    Program,
    /// A card's keys (or an edit review's `y` `n` `a`).
    Answers,
    /// The next key that would type brings the minimized chat back — it isn't typed.
    Unfold,
    /// Letters pick a label (pointing at an element).
    Label,
}

impl Takes {
    pub fn name(self) -> &'static str {
        match self {
            Takes::Commands => "commands",
            Takes::Text => "text",
            Takes::Program => "program",
            Takes::Answers => "answers",
            Takes::Unfold => "unfold",
            Takes::Label => "label",
        }
    }
}

/// Which keys a layer takes. Keys are compared whole (code, ctrl, alt): the routing test probes plain and
/// `C-` keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Filter {
    All,
    /// All but these — they go on down the chain (`space`, `:`, `C-w` stay the editor's).
    Except(Vec<Key>),
    Only(Vec<Key>),
}

impl Filter {
    #[cfg(test)]
    fn takes(&self, key: Key) -> bool {
        match self {
            Filter::All => true,
            Filter::Except(v) => !v.contains(&key),
            Filter::Only(v) => v.contains(&key),
        }
    }
}

/// One handler of the chain: where its keys go, which it takes, and whether it's only a shortcut (`C-q`,
/// a popup's Esc) — not what has the keys.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Layer {
    pub region: Region,
    pub filter: Filter,
    pub shortcut: bool,
}

/// Where the keys go — finer than `chrome::Keys`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeysTo {
    /// `handle_key`'s chain as it stands: the first layer that takes a key has it.
    pub chain: Vec<Layer>,
    /// What has the keys: the first layer that's no shortcut.
    pub region: Region,
    pub takes: Takes,
    /// A card's (or an edit review's) answers, as its hint row has them.
    pub answers: Vec<char>,
    /// `C-q` gave the card the keys: every key is its (Esc gives them back).
    pub held: bool,
    /// A key sequence under way, as which-key shows it (a count first).
    pub pending: String,
    /// The next key is taken as a character (`f`, `r`, `"`, a `gw` label).
    pub char_next: bool,
    /// The region is on screen this frame.
    pub drawn: bool,
    /// What has them once the card lets go — and, not held, what its other keys go to.
    pub under: Option<Region>,
    /// The element id that takes the next key.
    pub to: String,
}

impl KeysTo {
    /// Where `key` goes now (the routing test's model — `handle_key` takes the same way).
    #[cfg(test)]
    pub fn route(&self, key: Key) -> Region {
        self.chain.iter().find(|l| l.filter.takes(key)).map_or(Region::Code, |l| l.region)
    }
}

/// Where the keys go, as the text forms name it: a pane with what it is — `pane#29, the code (commands)`;
/// bare, `pane#29 (commands)` was read as a pane called "commands" that nothing on screen showed.
pub fn keys_place(to: &str) -> String {
    match to.starts_with("pane#") {
        true => format!("{to}, the code"),
        false => to.to_string(),
    }
}

fn ch(c: char) -> Key {
    Key::plain(Code::Char(c))
}

fn ctrl(c: char) -> Key {
    Key { code: Code::Char(c), ctrl: true, alt: false }
}

impl Editor {
    /// `handle_key`'s chain as data, in its order (`editor.rs` — `card_key`, `C-q`, `terminal_key`,
    /// `offer_key`, then `handle_key_inner`'s). Each condition is the handler's own; the routing test holds
    /// them together.
    fn keys_chain(&self) -> Vec<Layer> {
        let layer = |region, filter| Layer { region, filter, shortcut: false };
        let shortcut = |region, filter| Layer { region, filter, shortcut: true };
        let mut v = Vec::new();
        // `point::key`: labels up, every key is theirs
        if self.pointing.is_some() {
            v.push(layer(Region::Point, Filter::All));
        }
        let modal = self.picker.is_some() || self.prompt.is_some();
        let next = self.next_up();
        // `card_key`: held, every key is the front card's (`C-q` lets go); not held, `C-q` gives it them when
        // it's the next up (`inbox::next_up`: this session's own first, then the oldest)
        let up = self.offers.first().filter(|o| !o.installing() && !modal);
        let held = up.is_some() && self.card_keys;
        if let Some(o) = up {
            if self.card_keys {
                v.push(layer(Region::Card(o.id), Filter::All));
            } else if self.offer_reading() {
                // (its text the buffer in sight: `C-q` gives it the keys whatever is next up)
                v.push(shortcut(Region::Card(o.id), Filter::Only(vec![ctrl('q')])));
            } else if next == crate::inbox::Next::Card(o.id) {
                v.push(shortcut(Region::Card(o.id), Filter::Only(vec![ctrl('q')])));
            }
        }
        // `C-q` to the one that has waited longest, elsewhere (the front's edit review takes its own) — nothing
        // waiting, to a session whose turn ended
        let elsewhere = matches!(
            next,
            crate::inbox::Next::Go(_) | crate::inbox::Next::Done(_) | crate::inbox::Next::None
        );
        if !modal && !held && elsewhere {
            v.push(shortcut(Region::NextCard, Filter::Only(vec![ctrl('q')])));
        }
        // `terminal_key`: the program has them all
        let program =
            self.session.terminal.as_ref().is_some_and(|t| t.focus && t.visible && t.exited.is_none());
        if !modal && program {
            v.push(layer(Region::Program, Filter::All));
        }
        // `offer_key`: the front card's answers, in normal mode with nothing else holding the keys
        if let Some(o) = self.offers.first().filter(|_| self.card_has_keys()) {
            let takes = self.card_takes(o).into_iter().map(ch).collect();
            v.push(layer(Region::Card(o.id), Filter::Only(takes)));
        }
        // (`handle_key_inner`:) floating text — Esc closes it, `C-d`/`C-u` scroll it
        if self.popup.is_some() {
            v.push(shortcut(Region::Popup, Filter::Only(vec![Key::plain(Code::Esc), ctrl('d'), ctrl('u')])));
        }
        let pending = !self.pending.is_empty();
        // `llm::review_key`: back in the chat or the code, only `C-q` returns to it
        if let Some(rev) = self.session.review.as_ref().filter(|r| r.ready && !modal && !pending) {
            let chat = self.session.chat.as_ref().is_some_and(|c| c.focused);
            v.push(match chat || rev.away {
                true => shortcut(Region::EditReview, Filter::Only(vec![ctrl('q')])),
                false => layer(Region::EditReview, Filter::Except(vec![ch(' '), ch(':'), ctrl('w')])),
            });
        }
        // …still waiting for its answer: Esc stops it
        if crate::llm::esc_stops(self) {
            v.push(shortcut(Region::EditReview, Filter::Only(vec![Key::plain(Code::Esc)])));
        }
        if self.picker.is_some() {
            v.push(layer(Region::Picker, Filter::All));
        }
        // `chat::unfold_key`: minimized, what would type brings it back — its own keys (and `C-w` from an
        // empty input) go on
        if let Some(c) = self.session.chat.as_ref().filter(|c| c.focused && c.compact() && !pending) {
            let mut passes: Vec<Key> = [
                Key::plain(Code::Esc),
                ctrl('c'),
                ctrl('f'),
                ctrl('g'),
                ctrl(']'),
                ctrl('z'),
                ctrl('t'),
                ctrl('p'),
                ctrl('l'),
                ctrl('y'),
                ctrl('r'),
            ]
            .into();
            if c.input.is_empty() {
                passes.extend([ctrl('w'), Key { alt: true, ..ctrl('w') }]);
            }
            v.push(layer(Region::Unfold, Filter::Except(passes)));
        }
        // `chat::key` — before the prompt: it doesn't ask whether one is open
        if let Some(c) = self.session.chat.as_ref().filter(|c| c.focused && !pending) {
            v.push(match c.input.is_empty() {
                true => layer(Region::Chat, Filter::Except(vec![ctrl('w'), Key { alt: true, ..ctrl('w') }])),
                false => layer(Region::Chat, Filter::All),
            });
        }
        // The panels: normal mode, no prefix, no prompt
        if self.mode == Mode::Normal && !pending && self.prompt.is_none() {
            let editors = |more: &[Key]| {
                let mut v = vec![ch(' '), ch(':')];
                v.extend_from_slice(more);
                Filter::Except(v)
            };
            if self.review_rail_up() && self.session.code_review.as_ref().is_some_and(|r| r.focus) {
                v.push(layer(Region::Rail, editors(&[])));
            }
            if let Some(s) = self.session.sidebar.as_ref().filter(|s| s.focus && !self.side_folded()) {
                v.push(layer(Region::Side(s.mode), editors(&[ctrl('w')])));
            }
            if self.procs_focused() {
                v.push(layer(Region::Slot(Slot::Procs), editors(&[ctrl('w')])));
            }
            if !self.session.sidebar.as_ref().is_some_and(|s| s.focus) && self.plugin_panel_focused() {
                v.push(layer(Region::Slot(Slot::Plugin), editors(&[])));
            }
            if let Some(s) = self
                .slot()
                .filter(|s| self.bottom_focus && matches!(s, Slot::Terminal | Slot::Test | Slot::Task))
            {
                v.push(layer(Region::Slot(s), editors(&[ch('['), ch(']'), ctrl('w')])));
            }
        }
        if self.prompt.is_some() {
            v.push(layer(Region::Prompt, Filter::All));
        }
        v.push(layer(Region::Code, Filter::All));
        v
    }

    /// The letters the front card takes now — `offer_key`'s cases: its answers, what a weighty card swallows
    /// while it settles, `v` while it's cut, `a`/`A` it refuses aloud.
    fn card_takes(&self, o: &crate::offer::Offer) -> Vec<char> {
        if o.installing() || self.plugin_panel_focused() {
            return Vec::new();
        }
        let mut t: Vec<char> = Vec::new();
        let answers = |o: &crate::offer::Offer| {
            let mut v = vec!['y', 'a', 'A', 'D', 'R', 'c'];
            v.extend(o.preflights().then_some('p'));
            v.extend(o.aborts().then_some('X'));
            v.extend(o.restart_other().then_some('f'));
            v
        };
        if o.weighty() {
            // (not drawn: its answers aren't on screen — nothing of it is taken)
            if !self.screen.offer_buttons.iter().any(|b| !b.3.is_ascii_digit()) {
                return Vec::new();
            }
            if !self.offer_settled() {
                t.extend(['n', 'N']);
                t.extend(answers(o));
            }
            if o.change_set().is_some() {
                t.extend(['r', 'v']);
            }
            if !self.offer_seen_whole() {
                t.push('v');
                t.extend(answers(o));
            }
        }
        t.extend(['y', 'n']);
        t.extend(o.end_dirty().then_some('R'));
        t.extend(o.commits_first().then_some('c'));
        t.extend(
            matches!(o.what, crate::offer::What::Install { .. } | crate::offer::What::Fetch { .. })
                .then_some('N'),
        );
        t.extend(o.lasts().then_some('a'));
        t.extend(o.keeps().then_some('A'));
        t.extend(o.both().then_some('D'));
        t.extend(o.preflights().then_some('p'));
        t.extend(o.aborts().then_some('X'));
        t.extend(o.base_steps().then_some('b'));
        t.extend(o.restart_other().then_some('f'));
        if o.lasts() || o.weighty() {
            t.extend(['a', 'A']);
        }
        crate::sorting::sort(&mut t);
        t.dedup();
        t
    }

    /// Where the keys go now — what drivers check before sending a key, and what the status line names.
    pub fn keys_to(&self) -> KeysTo {
        let chain = self.keys_chain();
        let main = chain.iter().position(|l| !l.shortcut).unwrap_or(chain.len() - 1);
        let region = chain[main].region;
        let held = matches!(region, Region::Card(_)) && chain[main].filter == Filter::All;
        let under = match region {
            Region::Card(_) => {
                chain[main + 1..].iter().find(|l| !l.shortcut && !matches!(l.region, Region::Card(_)))
            }
            _ => None,
        }
        .map(|l| l.region);
        let takes = match region {
            Region::Card(_) | Region::EditReview => Takes::Answers,
            Region::Program => Takes::Program,
            Region::Unfold => Takes::Unfold,
            Region::Point => Takes::Label,
            Region::Picker | Region::Prompt | Region::Chat => Takes::Text,
            Region::Code if self.mode == Mode::Insert => Takes::Text,
            _ => Takes::Commands,
        };
        let answers = match region {
            Region::Card(id) => self.offers.iter().find(|o| o.id == id).map_or_else(Vec::new, |o| {
                let mut v: Vec<char> =
                    o.answers(self.preflight_quick().is_some()).into_iter().map(|a| a.0).collect();
                // (a weighty one cut to fit says `v` too)
                if o.weighty() && !self.offer_seen_whole() && !v.contains(&'v') {
                    v.push('v');
                }
                v
            }),
            Region::EditReview => vec!['y', 'n', 'a', 'q'],
            _ => Vec::new(),
        };
        let mut pending: Vec<String> = self.count.iter().map(|n| n.to_string()).collect();
        pending.extend(self.pending.iter().map(Key::to_string));
        KeysTo {
            region,
            takes,
            answers,
            held,
            pending: pending.join(" "),
            char_next: self.on_next_char.is_some(),
            drawn: self.drawn(region),
            under,
            to: self.element_id(region),
            chain,
        }
    }

    /// Whether `r` was on screen in the last frame (`editor.screen`, filled by `term.rs`).
    fn drawn(&self, r: Region) -> bool {
        let s = &self.screen;
        match r {
            // (its focused pane — the one the keys go to — with room: a chat filling a narrow screen leaves
            // the code's panes laid out zero wide)
            Region::Code => s.panes.iter().any(|p| p.0 == self.session.focus && p.1.w > 0) || self.welcome(),
            Region::Prompt => s.width.is_some(),
            Region::Picker => s.picker_list.is_some(),
            Region::Chat => s.chat_w > 0,
            // (its input isn't on screen — the minimized card is, and the first key brings the panel back)
            Region::Unfold => s.chat_mini.get().is_some() || s.chat_w > 0,
            Region::Side(_) => s.side_w > 0,
            Region::Rail => s.rail_w > 0,
            Region::EditReview => s.review_change.get().is_some(),
            Region::Slot(Slot::Terminal) | Region::Program => s.terminal_rows.is_some(),
            Region::Slot(Slot::Test) => s.test_panel.get().is_some(),
            Region::Slot(Slot::Task) => s.task_panel.get().is_some(),
            Region::Slot(Slot::Procs) => s.procs_panel.get().is_some(),
            Region::Slot(Slot::Plugin) => s.plugin_panel.get().is_some(),
            Region::Slot(Slot::Debug) => s.bottom_h > 0,
            Region::Card(id) => s.offer_card.is_some() && self.offers.first().is_some_and(|o| o.id == id),
            Region::NextCard => true,
            Region::Popup => s.popup.get().is_some(),
            Region::Point => self.pointing.is_some(),
        }
    }

    /// The element id a region's keys go to: `card#12`, `pane#3`, `chat`, `git` …
    fn element_id(&self, r: Region) -> String {
        match r {
            Region::Card(id) => format!("card#{id}"),
            Region::Code => format!("pane#{}", self.session.focus),
            Region::Side(crate::sidebar::Mode::Git) => "git".into(),
            Region::Side(_) => "side".into(),
            Region::Slot(Slot::Terminal) | Region::Program => "terminal".into(),
            Region::Slot(s) => slot_name(s).into(),
            Region::Rail => "review".into(),
            Region::Unfold => "chat".into(),
            other => other.name(),
        }
    }

    /// The pids of Claude's processes and runs, every session's — a socket peer under one is refused
    /// (`uisocket.rs`).
    pub fn claude_pids(&self) -> Vec<u32> {
        let mut v = Vec::new();
        for l in std::iter::once(&self.session).chain(&self.sessions) {
            v.extend(l.chat.as_ref().and_then(|c| c.pid()));
            v.extend(l.task_run.as_ref().filter(|r| r.by_claude).and_then(|r| r.pid()));
            v.extend(
                l.test_run.as_ref().filter(|r| r.guard.is_some() || r.chat.is_some()).and_then(|r| r.pid()),
            );
        }
        v.extend(self.llm_pid);
        v.extend(self.llm_warm.pid());
        v.retain(|&p| p > 0);
        v
    }
}

// ── The state ────────────────────────────────────────────────────────────

/// The state at its full level — drivers' and tests' (Claude's strict subset is this filtered:
/// `ui_state_claude` — no unsent text, other sessions as counts).
#[derive(Clone, Debug, PartialEq)]
pub struct UiState {
    pub pid: u32,
    pub boot: String,
    pub version: &'static str,
    pub frame: u64,
    pub input: u64,
    pub size: Option<(usize, usize)>,
    pub mode: &'static str,
    pub keys: KeysTo,
    /// The prompt's label, and its text.
    pub prompt: Option<(String, String)>,
    pub front: String,
    pub sessions: Vec<SessionState>,
    pub cards: Vec<CardState>,
    /// What waits for the user's answer, every session's, oldest first (`inbox.rs`) — the inbox's order.
    pub inbox: Vec<crate::inbox::Entry>,
    pub toasts: Vec<ToastState>,
    /// The front session's panels — their rows on screen only (`panels.rs`).
    pub panels: panels::Panels,
    /// Every session's runs: running, ended, waiting in line.
    pub runs: Vec<panels::Run>,
    /// The cards that went last, and how (`watch.rs`' ring — newest first, `GONE_SHOWN` of them).
    pub gone: Vec<watch::Gone>,
    /// What `:procs` lists, as of its last look (#143 — every `procs::QUIET_EVERY`, each second while open).
    pub procs: ProcsState,
    /// The plan's limits as claude said them, each window known now (`limits.rs`) — the account's.
    pub limits: Vec<crate::limits::Reading>,
}

/// `:procs`' rows and the checks holding now, as of its last look.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProcsState {
    /// How long ago the look was (None: none yet).
    pub looked_ms: Option<u64>,
    pub rows: Vec<ProcRow>,
    /// Tarae's own checks that hold now (`Procs.checks` not ended): what each says.
    pub checks: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProcRow {
    /// 0: no process (a session's heading, a held server, a run waiting in line).
    pub pid: u32,
    pub parent: u32,
    pub depth: usize,
    /// `tarae` `server` `held` `debugger` `helper` `task` `tests` `terminal` `chat` `ask` `spare` `plugin`
    /// `other` `session` `queued`.
    pub kind: &'static str,
    pub title: String,
    pub what: String,
    pub session: Option<String>,
    pub up_s: Option<u64>,
    pub cpu: Option<f32>,
    pub memory: u64,
    pub resident: u64,
    /// It ended and nobody reaped it.
    pub zombie: bool,
    /// A check holds about it now.
    pub flagged: bool,
}

impl ProcsState {
    pub fn of(p: &crate::procs::Procs) -> ProcsState {
        use crate::procs::Kind;
        let rows = p
            .rows
            .iter()
            .map(|r| ProcRow {
                pid: r.pid,
                parent: r.ppid,
                depth: r.depth,
                kind: match r.kind {
                    Kind::Tarae => "tarae",
                    Kind::Lsp(_) => "server",
                    Kind::Held(..) => "held",
                    Kind::Debug => "debugger",
                    Kind::Helper => "helper",
                    Kind::Task => "task",
                    Kind::Test => "tests",
                    Kind::Terminal => "terminal",
                    Kind::Chat => "chat",
                    Kind::Ask => "ask",
                    Kind::Reviewer => "reviewer",
                    Kind::Spare => "spare",
                    Kind::Plugin(..) => "plugin",
                    Kind::Other => "other",
                    Kind::Session(_) => "session",
                    Kind::Queued(_) => "queued",
                },
                title: r.title.clone(),
                what: r.what.clone(),
                session: r.session.map(session_id),
                up_s: r.up.map(|d| d.as_secs()),
                cpu: r.cpu.map(|c| (c * 10.0).round() / 10.0),
                memory: r.memory(),
                resident: r.rss,
                zombie: r.zombie,
                flagged: r.flagged,
            })
            .collect();
        ProcsState { looked_ms: p.looked.map(|t| ms(t.elapsed())), rows, checks: p.holding() }
    }

    pub fn json(&self) -> Value {
        let rows: Vec<Value> = self
            .rows
            .iter()
            .map(|r| {
                json!({ "pid": r.pid, "parent": r.parent, "depth": r.depth, "kind": r.kind, "title": r.title,
                        "what": r.what, "session": r.session, "up_s": r.up_s, "cpu": r.cpu,
                        "memory": r.memory, "resident": r.resident, "zombie": r.zombie, "flagged": r.flagged })
            })
            .collect();
        json!({ "looked_ms": self.looked_ms, "rows": rows, "checks": self.checks })
    }

    /// Its lines in the text form: a total, each process a step in under what started it (a session's
    /// heading as `── name`), then the checks holding.
    fn text(&self, out: &mut Vec<String>) {
        let live: Vec<&ProcRow> = self.rows.iter().filter(|r| r.pid != 0).collect();
        if live.is_empty() {
            return;
        }
        let memory = crate::procs::mem(live.iter().map(|r| r.memory).sum());
        let ago = self.looked_ms.map_or(String::new(), |m| format!(" · looked {}s ago", m / 1000));
        out.push(format!("procs: {} processes · {memory}{ago}", live.len()));
        for r in &self.rows {
            let pad = "  ".repeat(r.depth + 1);
            let what = if r.what.is_empty() { String::new() } else { format!(" — {}", r.what) };
            let line = match (r.kind, r.pid) {
                ("session", _) => format!("  ── {}{what}", r.title),
                ("queued", _) => format!("{pad}{}{what} · queued", r.title),
                (_, 0) => format!("{pad}{}{what} · held", r.title),
                _ => {
                    let cpu = r.cpu.map_or("—".into(), |c| format!("{c:.1}%"));
                    let mut s = format!(
                        "{pad}{} {}{what} · cpu {cpu} · {}",
                        r.pid,
                        r.title,
                        crate::procs::mem(r.memory)
                    );
                    if r.zombie {
                        s += " · ended, not reaped";
                    }
                    if r.flagged {
                        s += " · ▲";
                    }
                    s
                }
            };
            out.push(line);
        }
        out.extend(self.checks.iter().map(|c| format!("  ▲ {c}")));
    }
}

/// Cards the state lists of the gone ring (it keeps `watch::GONE_KEPT`).
const GONE_SHOWN: usize = 10;

#[derive(Clone, Debug, PartialEq)]
pub struct SessionState {
    pub id: String,
    pub name: String,
    pub branch: Option<String>,
    /// Its checkout — `tarae state` finds the tarae whose session holds the folder it's run in.
    pub root: String,
    /// What its chat's Claude does.
    pub doing: String,
    /// Its reviewer (`reviewer.rs`): `reviewing` while it reads — None else. A field of its own beside
    /// `doing`: the two are at work apart, often at once, and a driver waiting for `doing` to be `idle` waits
    /// for the chat's turn, not a review it didn't ask for (#195).
    pub reviewer: Option<&'static str>,
    /// Its reviewer's last run, how it went (`ReviewerState`) — while it reads and after, whether or not its
    /// review is open (cardburden 7: a driver read the findings off the rail).
    pub reviewer_run: Option<ReviewerState>,
    pub cards: usize,
    pub rebasing: Option<String>,
    pub reloaded: usize,
    pub front: bool,
    /// Its Claude's usage (`usage.rs`) — None: nothing reported yet.
    pub usage: Option<UsageState>,
    /// Its Claude's conversation compacted since tarae started (`compaction.rs`) — None: never.
    pub compacted: Option<crate::compaction::Compacted>,
    /// The credential its chat's claude last reported (`apiKeySource`: `none`, `apiKeyHelper` … — `limits.rs`)
    /// — None: none reported yet.
    pub key_source: Option<String>,
    /// Who pays for its Claude: `subscription` · `api` (`billing.rs`).
    pub billing: &'static str,
    /// What its last sandboxed run's sandbox keeps apart and shares (`Session.sandbox`) — None: none yet.
    pub sandbox: Option<crate::sandbox::guarantees::Guarantees>,
}

/// A session's reviewer (`reviewer.rs`) as the state says it: its last run — reading, done or stopped — and
/// what it found so far.
#[derive(Clone, Debug, PartialEq)]
pub struct ReviewerState {
    /// `reading` · `done` · `stopped`.
    pub state: &'static str,
    /// How long it read — so far, while it reads.
    pub ms: u64,
    pub reads: usize,
    /// Its findings still pinned, and those answered (by you or the author).
    pub findings: usize,
    pub answered: usize,
    /// Those on a high-risk change — None when its review isn't the one open (the grades are the open
    /// review's).
    pub high: Option<usize>,
    /// Its verdict's first line (empty while it reads, or none came).
    pub verdict: String,
    /// Why it stopped (`stopped` only, not by you).
    pub why: Option<String>,
    /// You stopped it (`:reviewer-stop`, `:procs`).
    pub by_you: bool,
    /// What its review was of (`Base::label`: `uncommitted`, `since main` …).
    pub of: String,
    /// It ended while its session was behind, and that session hasn't been in front since (its inbox item).
    pub news: bool,
}

impl ReviewerState {
    /// Session `l`'s reviewer — None: it never had one since tarae started.
    pub fn of(ed: &Editor, l: &crate::session::Session) -> Option<ReviewerState> {
        use crate::reviewer::State;
        let r = l.reviewer.as_ref()?;
        let t = crate::reviewer::review_tally_in(ed, l).unwrap_or_default();
        let open = l.code_review.as_ref().is_some_and(|c| c.started == r.of);
        let (state, why) = match &r.state {
            State::Reading => ("reading", None),
            State::Done => ("done", None),
            State::Failed(why) => ("stopped", Some(why.clone())),
            State::Stopped => ("stopped", None),
        };
        Some(ReviewerState {
            state,
            ms: ms(r.took.unwrap_or_else(|| r.started.elapsed())),
            reads: r.reads,
            findings: t.findings,
            answered: t.answered,
            high: open.then_some(t.high),
            verdict: r.verdict.lines().find(|l| !l.trim().is_empty()).unwrap_or_default().trim().to_string(),
            why,
            by_you: r.state == State::Stopped,
            of: r.label.clone(),
            news: r.unseen,
        })
    }

    pub fn json(&self) -> Value {
        json!({
            "state": self.state, "ms": self.ms, "reads": self.reads, "findings": self.findings,
            "answered": self.answered, "high": self.high, "verdict": self.verdict, "why": self.why,
            "by_you": self.by_you, "of": self.of, "news": self.news,
        })
    }

    /// One line: `done in 1m 20s · 14 reads · 2 findings ▲1 · 1 answered — <verdict>`.
    pub fn text(&self) -> String {
        let took = crate::term::duration(self.ms as f32 / 1000.0);
        let how = match (self.state, &self.why) {
            ("reading", _) => format!("reading {took}"),
            ("stopped", _) if self.by_you => format!("stopped by you after {took}"),
            ("stopped", Some(why)) => format!("stopped after {took}: {why}"),
            ("stopped", None) => format!("stopped after {took}"),
            _ => format!("done in {took}"),
        };
        let high = self.high.filter(|&h| h > 0).map(|h| format!(" ▲{h}")).unwrap_or_default();
        let answered = match self.answered {
            0 => String::new(),
            n => format!(" · {n} answered"),
        };
        let verdict = match self.verdict.is_empty() {
            true => String::new(),
            false => format!(" — {}", self.verdict),
        };
        format!(
            "{how} · {} · {}{high}{answered}{verdict}",
            crate::nav::count(self.reads, "read"),
            crate::nav::count(self.findings, "finding")
        )
    }
}

/// A session's usage as the state says it — None in a field: not reported.
#[derive(Clone, Debug, PartialEq)]
pub struct UsageState {
    /// `warm` · `cold` · `not reported`.
    pub cache: &'static str,
    pub seconds_left: Option<u64>,
    pub ttl: Option<&'static str>,
    /// The last request's input + cache read + cache write · its model's window (from a result).
    pub context: Option<u64>,
    pub window: Option<u64>,
    pub last_turn: Option<crate::usage::Turn>,
    pub totals: crate::usage::Totals,
}

impl UsageState {
    pub fn of(u: &crate::usage::Usage, now_ms: u64) -> Option<UsageState> {
        use crate::usage::Cache;
        if !u.reported() {
            return None;
        }
        let cache = u.cache(now_ms);
        let (seconds_left, ttl) = match cache {
            Cache::Warm { left_ms, ttl } => (Some(left_ms / 1000), Some(ttl.name())),
            _ => (None, u.conv.as_ref().and_then(|c| c.ttl).map(crate::usage::Ttl::name)),
        };
        let (context, window) = u.context().map_or((None, None), |(n, w)| (Some(n), w));
        Some(UsageState {
            cache: cache.word(),
            seconds_left,
            ttl,
            context,
            window,
            last_turn: u.last_turn().cloned(),
            totals: u.totals(),
        })
    }

    /// The context's share of the window, in percent.
    pub fn percent(&self) -> Option<f64> {
        Some(self.context? as f64 * 100.0 / self.window.filter(|&w| w > 0)? as f64)
    }

    pub fn json(&self) -> Value {
        let c = |n: &crate::usage::Counts| json!({ "input": n.input, "cache_write": n.write, "cache_read": n.read, "output": n.output });
        let last = self.last_turn.as_ref().map(|t| {
            let mut v = c(&t.counts);
            v["at_ms"] = json!(t.at_ms);
            v["model"] = json!(t.model);
            v["cost"] = json!(t.cost);
            v["requests"] = json!(t.requests);
            if let Some(k) = &t.compacted {
                v["compacted"] = json!(k);
            }
            // (its tool calls — `calls.rs`; none for a one-turn process's)
            if let Some(calls) = &t.calls {
                v["calls"] = calls.json();
            }
            v
        });
        let mut totals = c(&self.totals.counts);
        totals["turns"] = json!(self.totals.turns);
        if self.totals.counted > 0 {
            totals["calls"] = self.totals.calls.json();
            totals["calls"]["turns"] = json!(self.totals.counted);
        }
        // (the cost of the turns that reported one — none did: null; `unpriced` counts the others)
        let priced = self.totals.turns > self.totals.unpriced;
        totals["cost"] = if priced { json!(self.totals.cost) } else { Value::Null };
        totals["unpriced"] = json!(self.totals.unpriced);
        json!({
            "cache": self.cache, "seconds_left": self.seconds_left, "ttl": self.ttl,
            "context": { "tokens": self.context, "window": self.window,
                         "percent": self.percent().map(|p| (p * 10.0).round() / 10.0) },
            "last_turn": last, "totals": totals,
        })
    }

    /// One line: `cache warm 54m (1h) · context 4% of 200k · last turn $0.0011 … · total $0.020 in 3 turns ·
    /// ≈ API price`.
    pub fn text(&self) -> String {
        use crate::usage::{count, dollars, tokens};
        let cache = match (self.seconds_left, self.ttl) {
            (Some(s), Some(t)) => format!("cache warm {} ({t})", crate::usage::left(s * 1000)),
            _ => format!("cache {}", self.cache),
        };
        let context = match (self.context, self.percent(), self.window) {
            (Some(_), Some(p), Some(w)) => format!("context {p:.0}% of {}", tokens(w)),
            (Some(n), ..) => format!("context {} (window not reported)", tokens(n)),
            _ => "context not reported".into(),
        };
        let last = self.last_turn.as_ref().map_or_else(String::new, |t| {
            let n = &t.counts;
            format!(
                " · last turn {} (read {} · write {} · in {} · out {})",
                t.cost.map_or_else(|| "cost not reported".into(), dollars),
                count(n.read),
                count(n.write),
                count(n.input),
                count(n.output)
            )
        });
        let turns = crate::usage::turns(self.totals.turns);
        // (#161: claude's own table's price, not what a subscription pays)
        format!("{cache} · {context}{last} · total {} in {turns} · ≈ API price", self.totals.cost_said())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CardState {
    pub id: String,
    pub session: String,
    pub whose: &'static str,
    pub kind: &'static str,
    pub title: String,
    pub asks: String,
    pub answers: Vec<(char, &'static str)>,
    pub state: &'static str,
    pub weighty: bool,
    pub settled: bool,
    pub rev: u64,
    /// `front` (up, taking its answers) · `stacked` (up behind it) · `parked` (in a session behind).
    pub up: &'static str,
    pub waited_ms: u64,
    /// When it came — the inbox's order (`waited_ms` is cut to milliseconds, each read at its own moment:
    /// two cards come within one could swap between two reads).
    pub since: std::time::Instant,
    pub in_sight_ms: Option<u64>,
    /// Every line of it has been in sight — drawn whole, or read to its end in its `v` buffer (a weighty
    /// card's `y` waits for it).
    pub seen_whole: bool,
    /// The front card's lines its drawing left out (`↓ 2 more lines`, 0: drawn whole) — None for a card not
    /// drawn, or one stepped aside to a bar while its text is read (`viewing`).
    pub cut: Option<usize>,
    /// The front card's whole text is open in its buffer (`v`).
    pub viewing: bool,
    pub at: Option<(usize, usize, usize, usize)>,
    /// A run card the same as one answered before: that card, its answer and when, and whether anything changed
    /// since (`sinceyes`, cardburden 3).
    pub same_as: Option<crate::sinceyes::SameAs>,
    /// Its text whole, a line at a time with what each is (`text`, cardburden 5) — empty at Claude's level.
    pub text: Vec<(String, &'static str)>,
    /// The warnings it says (`cardwarn.rs`), by id.
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ToastState {
    pub id: String,
    pub kind: &'static str,
    pub text: String,
    /// `card` · `status` (folded into the status line) · `waiting` (an error or warning no card has shown
    /// yet — U7).
    pub place: &'static str,
    pub shown: bool,
}

/// This process's start, random enough to tell one start from the next (`:restart` keeps the pid, and the
/// ids start over).
pub fn boot() -> String {
    static BOOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BOOT.get_or_init(|| {
        use std::hash::{BuildHasher, Hasher};
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos(),
        );
        h.write_u32(std::process::id());
        format!("{:08x}", h.finish() as u32)
    })
    .clone()
}

pub(crate) fn session_id(id: crate::event::SessionId) -> String {
    format!("s{id}")
}

fn ms(d: std::time::Duration) -> u64 {
    d.as_millis().min(u64::MAX as u128) as u64
}

impl Editor {
    /// What a session's Claude is doing, in the state's words.
    fn doing_said(&self, l: &crate::session::Session) -> String {
        match self.doing(l) {
            crate::session::Doing::Idle => "idle".into(),
            crate::session::Doing::Working => "working".into(),
            crate::session::Doing::Waiting => "waiting".into(),
            crate::session::Doing::Done => "done".into(),
            d @ crate::session::Doing::Stuck { .. } => format!("stuck {}", d.said().trim()),
            // (at work, held back in line behind another run: `queued 2nd`)
            d @ crate::session::Doing::Queued { .. } => d.said(),
        }
    }

    /// The state as of the last frame drawn.
    pub fn ui_state(&self) -> UiState {
        let front = &self.session;
        let now_ms = crate::usage::now_ms();
        let session = |l: &crate::session::Session, is_front: bool| SessionState {
            id: session_id(l.id),
            name: l.label().to_string(),
            branch: l.branch.clone().or_else(|| l.git_branch.clone()),
            root: l.root.display().to_string(),
            doing: self.doing_said(l),
            reviewer: l.reviewing().then_some("reviewing"),
            reviewer_run: ReviewerState::of(self, l),
            cards: if is_front { self.offers.len() + l.parked.len() } else { l.parked.len() },
            rebasing: l.rebase.as_ref().map(|r| r.said()),
            reloaded: l.reloaded,
            front: is_front,
            usage: UsageState::of(&l.usage, now_ms),
            compacted: Some(l.compacted.clone()).filter(|c| c.count > 0),
            key_source: l.usage.key_source.clone(),
            billing: l.billing.name(),
            sandbox: l.sandbox.clone(),
        };
        let mut sessions = vec![session(front, true)];
        sessions.extend(self.sessions.iter().map(|l| session(l, false)));
        crate::sorting::by_key(&mut sessions, |s| s.id[1..].parse::<u64>().unwrap_or(0));
        let cards = self.ui_cards();
        let inbox = self.inbox_from(&cards);
        let on_card = self.screen.toast_card.get().is_some();
        let toasts = self
            .toasts
            .iter()
            .filter(|t| t.alive())
            .map(|t| ToastState {
                id: format!("toast#{}", t.id),
                kind: match t.kind {
                    crate::editor::ToastKind::Info => "info",
                    crate::editor::ToastKind::Success => "success",
                    crate::editor::ToastKind::Warning => "warning",
                    crate::editor::ToastKind::Error => "error",
                },
                text: t.text.clone(),
                place: match (on_card, t.waiting()) {
                    (true, _) => "card",
                    (false, true) => "waiting",
                    (false, false) => "status",
                },
                shown: t.shown,
            })
            .collect();
        UiState {
            pid: std::process::id(),
            boot: boot(),
            version: env!("CARGO_PKG_VERSION"),
            frame: self.frames,
            input: self.input_seq,
            size: self.last_frame.as_ref().map(|f| (f.1 as usize, f.2 as usize)),
            mode: match self.mode {
                Mode::Normal => "normal",
                Mode::Insert => "insert",
                Mode::Select => "select",
            },
            keys: self.keys_to(),
            prompt: self
                .prompt
                .as_ref()
                .map(|p| (p.label.clone().unwrap_or_else(|| p.kind.label().to_string()), p.text.clone())),
            front: session_id(front.id),
            sessions,
            cards,
            inbox,
            toasts,
            panels: self.ui_panels(),
            runs: self.ui_runs(),
            gone: watch::recent(&self.ui_gone, GONE_SHOWN),
            procs: ProcsState::of(&self.procs),
            limits: self.limits.known(now_ms).into_iter().cloned().collect(),
        }
    }

    /// Every card of every session — up in front (the front one, the ones stacked behind it), parked in the
    /// sessions behind — as the state lists them. The inbox is built on these (`inbox.rs`), so the two never
    /// disagree.
    pub fn ui_cards(&self) -> Vec<CardState> {
        let front = &self.session;
        let quick = self.preflight_quick().is_some();
        let card = |o: &crate::offer::Offer, sid: crate::event::SessionId, up: &'static str| {
            let is_front = up == "front";
            CardState {
                id: format!("card#{}", o.id),
                session: session_id(sid),
                whose: if o.claudes() { "claude" } else { "tarae" },
                kind: o.kind(),
                title: o.title.clone(),
                asks: asks(o),
                answers: o.answers(quick),
                state: match o.state {
                    crate::offer::OfferState::Asking => "asking",
                    crate::offer::OfferState::Installing(_) => "installing",
                    crate::offer::OfferState::Failed(_) => "failed",
                },
                weighty: o.weighty(),
                settled: is_front && self.offer_settled(),
                rev: o.rev,
                up,
                waited_ms: ms(o.since.elapsed()),
                since: o.since,
                in_sight_ms: self
                    .offer_seen
                    .get()
                    .filter(|&(id, rev, _)| (id, rev) == (o.id, o.rev))
                    .map(|(.., at)| ms(at.elapsed())),
                seen_whole: is_front && self.offer_seen_whole(),
                cut: self.screen.offer_cut.filter(|_| is_front),
                viewing: is_front && self.offer_viewing(),
                at: self.screen.offer_card.filter(|_| is_front),
                same_as: match &o.what {
                    crate::offer::What::Run(r) => r.same.clone(),
                    _ => None,
                },
                text: text(o),
                warnings: o.asked.as_ref().map(|a| a.warn.clone()).unwrap_or_default(),
            }
        };
        let mut cards: Vec<CardState> = self
            .offers
            .iter()
            .enumerate()
            .map(|(i, o)| card(o, front.id, if i == 0 { "front" } else { "stacked" }))
            .collect();
        cards.extend(front.parked.iter().map(|o| card(o, front.id, "parked")));
        for l in &self.sessions {
            cards.extend(l.parked.iter().map(|o| card(o, l.id, "parked")));
        }
        cards
    }
}

/// A card's `asks`: its title and the lines drawn at full strength (what's decided), whole — scripts and the
/// inbox read it all (frictions 13), the text line and the watch's `card-up` too (frictions15); Claude's level
/// and the inbox picker's row cut it (`asks_line`).
fn asks(o: &crate::offer::Offer) -> String {
    let decided = o.decided_lines();
    let mut s = o.title.clone();
    for (i, line) in o.question.split('\n').enumerate() {
        if decided.contains(&i) && !line.trim().is_empty() {
            s.push_str(" · ");
            s.push_str(line.trim());
        }
    }
    s
}

/// A card's text whole, a line at a time with what it is, as the card draws it (cardburden 5 — a driver told
/// the card's weight without reading the frame): `warning` (what yes risks, in the warning colour — `warning`,
/// the data's `warnings`) · `diff` (what changed since, `sinceyes`) · `decided` (at full strength: what's being
/// decided) · `said` (the rest, faint).
fn text(o: &crate::offer::Offer) -> Vec<(String, &'static str)> {
    let decided = o.decided_lines();
    let warns = |i: usize| {
        o.warning.as_ref().is_some_and(|r| r.contains(&i)) || o.warnings.iter().any(|r| r.contains(&i))
    };
    o.question
        .split('\n')
        .enumerate()
        .map(|(i, line)| {
            let role = match () {
                _ if warns(i) => "warning",
                _ if o.diff.as_ref().is_some_and(|r| r.contains(&i)) => "diff",
                _ if decided.contains(&i) => "decided",
                _ => "said",
            };
            (line.to_string(), role)
        })
        .collect()
}

/// A card's `asks` where room or tokens count (Claude's level, the inbox picker's row): cut at `ASKS_MAX`.
pub(crate) fn asks_line(asks: &str) -> String {
    match asks.chars().count() > ASKS_MAX {
        true => asks.chars().take(ASKS_MAX - 1).collect::<String>() + "…",
        false => asks.to_string(),
    }
}

impl UiState {
    /// The JSON drivers read (`v` is the schema's version).
    pub fn json(&self) -> Value {
        let k = &self.keys;
        let keys = json!({
            "to": k.to,
            "region": k.region.name(),
            "takes": k.takes.name(),
            "answers": k.answers.iter().map(char::to_string).collect::<Vec<_>>(),
            "held": k.held,
            "pending": k.pending,
            "char_next": k.char_next,
            "drawn": k.drawn,
            "under": k.under.map(Region::name),
        });
        let sessions: Vec<Value> = self
            .sessions
            .iter()
            .map(|s| {
                let mut v = json!({ "id": s.id, "name": s.name, "doing": s.doing, "cards": s.cards, "root": s.root,
                    "billing": s.billing });
                if let Some(b) = &s.branch {
                    v["branch"] = json!(b);
                }
                if let Some(r) = s.reviewer {
                    v["reviewer"] = json!(r);
                }
                if let Some(r) = &s.reviewer_run {
                    v["reviewer_run"] = r.json();
                }
                if let Some(r) = &s.rebasing {
                    v["rebasing"] = json!(r);
                }
                if s.reloaded > 0 {
                    v["reloaded"] = json!(s.reloaded);
                }
                if let Some(u) = &s.usage {
                    v["usage"] = u.json();
                }
                if let Some(c) = &s.compacted {
                    v["compacted"] = c.json();
                }
                if let Some(k) = &s.key_source {
                    v["key_source_seen"] = json!(k);
                }
                v["sandbox"] = s.sandbox.as_ref().map_or(Value::Null, |g| g.json());
                v
            })
            .collect();
        let cards: Vec<Value> = self
            .cards
            .iter()
            .map(|c| {
                let mut v = json!({
                    "id": c.id, "session": c.session, "whose": c.whose, "kind": c.kind, "title": c.title,
                    "asks": c.asks,
                    "answers": c.answers.iter().map(|(k, d)| json!({ "key": k.to_string(), "does": d }))
                        .collect::<Vec<_>>(),
                    "state": c.state, "weighty": c.weighty, "settled": c.settled, "rev": c.rev, "up": c.up,
                    "waited_ms": c.waited_ms, "in_sight_ms": c.in_sight_ms, "seen_whole": c.seen_whole,
                    "cut": c.cut, "viewing": c.viewing,
                    "at": c.at.map(|(x, y, w, h)| [x, y, w, h]),
                    "same_as": c.same_as.as_ref().map(|s| json!({
                        "card": format!("card#{}", s.card), "answer": s.answer.to_string(), "at": s.at,
                        "unchanged": s.unchanged,
                    })),
                });
                // (none at Claude's level: its `asks` is cut, and the text would be it whole)
                if !c.text.is_empty() {
                    v["text"] =
                        c.text.iter().map(|(line, role)| json!({ "line": line, "role": role })).collect();
                }
                if !c.warnings.is_empty() {
                    v["warnings"] = json!(c.warnings);
                }
                v
            })
            .collect();
        let toasts: Vec<Value> = self
            .toasts
            .iter()
            .map(
                |t| json!({ "id": t.id, "kind": t.kind, "text": t.text, "where": t.place, "shown": t.shown }),
            )
            .collect();
        json!({
            "v": V,
            "tarae": { "pid": self.pid, "boot": self.boot, "version": self.version },
            "frame": self.frame,
            "input": self.input,
            "size": self.size.map(|(w, h)| [w, h]),
            "mode": self.mode,
            "keys": keys,
            "prompt": self.prompt.as_ref().map(|(label, text)| json!({ "kind": label, "text": text })),
            "front": self.front,
            "sessions": sessions,
            "cards": cards,
            "inbox": self.inbox.iter().map(crate::inbox::Entry::json).collect::<Vec<_>>(),
            "toasts": toasts,
            "panels": self.panels.json(),
            "runs": self.runs.iter().map(panels::Run::json).collect::<Vec<_>>(),
            "gone": self.gone.iter().map(|g| json!({
                "id": g.id, "title": g.title, "how": g.how, "ago_ms": g.at.elapsed().as_millis() as u64,
            })).collect::<Vec<_>>(),
            "procs": self.procs.json(),
            "limits": crate::limits::json(&self.limits.iter().collect::<Vec<_>>()),
        })
    }

    /// The short text form, for people (not a contract — the JSON is; Claude's is `text_claude`).
    pub fn text(&self) -> String {
        let mut out = Vec::new();
        let size = self.size.map(|(w, h)| format!(" · {w}×{h}")).unwrap_or_default();
        out.push(format!("tarae {} · frame {} · input {}{size}", self.pid, self.frame, self.input));
        out.extend(self.keys_line());
        out.push(self.mode_line());
        if self.sessions.len() > 1 {
            let s: Vec<String> = self
                .sessions
                .iter()
                .map(|s| {
                    let front = if s.front { " (front)" } else { "" };
                    let cards = match s.cards {
                        0 => String::new(),
                        1 => " · 1 card".into(),
                        n => format!(" · {n} cards"),
                    };
                    let reviewer = s.reviewer.map(|r| format!(" · {r}")).unwrap_or_default();
                    // (the plan is the calm default; a key session says so)
                    let api = if s.billing == "api" { " · api" } else { "" };
                    format!("{} {}{front}{api} · {}{reviewer}{cards}", s.id, s.name, s.doing)
                })
                .collect();
            out.push(format!("sessions: {}", s.join("   ")));
        }
        if !self.limits.is_empty() {
            let said: Vec<String> = self.limits.iter().map(crate::limits::said).collect();
            out.push(format!("limits: {}", said.join("   ")));
        }
        for s in &self.sessions {
            if let Some(u) = &s.usage {
                out.push(format!("usage {}: {}", s.id, u.text()));
            }
            if let Some(c) = &s.compacted {
                out.push(format!("compacted {}: {}", s.id, c.text()));
            }
            if let Some(r) = &s.reviewer_run {
                out.push(format!("reviewer {}: {}", s.id, r.text()));
            }
            if let Some(g) = &s.sandbox {
                out.push(format!("sandbox {}: {}", s.id, g.line_after_label()));
            }
        }
        out.extend(self.card_lines());
        self.panels.text(&mut out);
        out.extend(self.runs.iter().map(panels::Run::text));
        if let Some(g) = self.gone.first() {
            out.push(format!("last gone: {} {:?} — {}", g.id, g.title, g.how));
        }
        self.procs.text(&mut out);
        out.join("\n") + "\n"
    }

    /// Where the keys go, as the text forms say it.
    fn keys_line(&self) -> Option<String> {
        let k = &self.keys;
        let mut keys = format!("keys → {} ({}", keys_place(&k.to), k.takes.name());
        if !k.answers.is_empty() {
            let a: Vec<String> = k.answers.iter().map(char::to_string).collect();
            keys += &format!(" {}", a.join(" "));
        }
        keys.push(')');
        if k.held {
            keys += " · held: every key is its";
        }
        if !k.drawn {
            keys += " · NOT ON SCREEN";
        }
        if !k.pending.is_empty() {
            keys += &format!(" · pending {}", k.pending);
        }
        if k.char_next {
            keys += " · the next key is a character";
        }
        if let Some(u) = k.under {
            keys += &format!(" · under it: {}", u.name());
        }
        Some(keys)
    }

    fn mode_line(&self) -> String {
        let mut mode = format!("mode {}", self.mode);
        if let Some((label, text)) = &self.prompt {
            mode += &match text.is_empty() {
                true => format!(" · prompt {label}"),
                false => format!(" · prompt {label} {text:?}"),
            };
        }
        mode
    }

    /// A line a card, then one a toast.
    fn card_lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for c in &self.cards {
            let answers: Vec<String> =
                c.answers.iter().map(|(k, d)| format!("{k} {}", d.to_lowercase())).collect();
            // (whole: a driver asked to approve a command reads all of it — frictions15)
            let mut line = format!("{} {} · {} · {:?}", c.id, c.whose, c.kind, c.asks);
            if c.session != self.front {
                line += &format!(" · in {}", c.session);
            }
            line += &format!(" · {}", c.up);
            if !answers.is_empty() {
                line += &format!(" · {}", answers.join(" · "));
            }
            if c.state != "asking" {
                line += &format!(" · {}", c.state);
            } else if c.up == "front" && c.weighty {
                line += if c.settled { " · settled" } else { " · settling" };
            }
            // (what's been read of it: `y` on a weighty one waits for all of it)
            if c.up == "front" && (c.weighty || c.viewing || c.cut.is_some_and(|k| k > 0)) {
                line += &match (c.seen_whole, c.cut, c.viewing) {
                    (true, ..) => " · seen whole".to_string(),
                    (false, _, true) => " · open in its buffer, not read to its end".to_string(),
                    (false, Some(k), false) if k > 0 => format!(" · {k} more lines, not seen whole"),
                    _ => String::new(),
                };
            }
            out.push(line);
        }
        if !self.inbox.is_empty() {
            let ids: Vec<String> = self.inbox.iter().map(|e| format!("{} ({})", e.id, e.session)).collect();
            out.push(format!("inbox, oldest first: {}", ids.join(" · ")));
        }
        for t in &self.toasts {
            let text = t.text.lines().next().unwrap_or_default();
            out.push(format!("{} {} {text:?} ({})", t.id, t.kind, t.place));
        }
        out
    }
}

// ── Asked over the socket ────────────────────────────────────────────────

/// A snapshot asked for (`uisocket.rs`): answered after the frame drawn once `input_seq` reached
/// `after_input` — so it never describes a state the user hasn't seen.
pub struct Ask {
    pub after_input: u64,
    pub json: bool,
    pub reply: std::sync::mpsc::Sender<String>,
    /// Past this the asker has given up: dropped unanswered.
    pub until: std::time::Instant,
    pub what: Asked,
}

/// What a snapshot is of. Chat, review and frame are the user's level only: none of them is part of `UiState`,
/// which Claude's level is built from, and only the socket asks for them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Asked {
    State,
    /// A session's last turn (`tarae state --chat`): its id or name, empty for the one in front
    /// (`Editor::chat_turn`).
    Chat(String),
    /// A session's reviewer, whole (`tarae state --review`, `Editor::review_report`).
    Review(String),
    /// The frame drawn, as the terminal got it (`tarae state --frame`, `cargo xtask shot --live`) — always JSON.
    Frame,
    /// Which features get used (`tarae state --features`, `features.rs`) — the file read on a worker, answered
    /// from there.
    Features,
}

/// A session's last turn as its chat shows it (#204: a driver read the conversation's own files to know
/// what Claude answered) — from the last message the user sent on: tool rows as the chat draws them, then
/// the answer. The user's level only: it isn't part of `UiState`, which Claude's level is built from
/// (`claude.rs` — another session is counts there), and only the socket asks for it.
pub struct ChatTurn {
    pub session: String,
    pub name: String,
    pub doing: String,
    /// (role, text): `user` · `assistant` · `tool` · `thought` · `note`.
    pub rows: Vec<(&'static str, String)>,
}

impl ChatTurn {
    pub fn json(&self) -> Value {
        json!({
            "v": V,
            "chat": {
                "session": self.session, "name": self.name, "doing": self.doing,
                "turn": self.rows.iter().map(|(role, text)| json!({ "role": role, "text": text })).collect::<Vec<_>>(),
            },
        })
    }

    pub fn text(&self) -> String {
        let mut out = vec![format!("chat of {} {} — {}", self.session, self.name, self.doing)];
        if self.rows.is_empty() {
            out.push("no turn yet".into());
        }
        for (role, text) in &self.rows {
            match *role {
                "tool" | "note" => out.push(text.clone()),
                _ => out.push(format!("{role}:\n{text}")),
            }
        }
        out.join("\n") + "\n"
    }
}

impl Editor {
    /// `which` session's last turn (`ChatTurn`): by id (`s1`) or name, empty for the one in front. Err names
    /// the sessions there are.
    pub fn chat_turn(&self, which: &str) -> Result<ChatTurn, String> {
        use crate::chat::Role;
        let l = self.asked_session(which)?;
        let msgs = l.chat.as_ref().map_or(&[][..], |c| &c.msgs[..]);
        let from = msgs.iter().rposition(|m| m.role == Role::User).unwrap_or(msgs.len());
        let rows = msgs[from..]
            .iter()
            .map(|m| match m.role {
                Role::User => ("user", m.text.clone()),
                Role::Assistant => ("assistant", m.text.clone()),
                Role::Thought => ("thought", m.text.clone()),
                Role::Note => ("note", m.text.clone()),
                Role::Tool => {
                    let row = m.look.as_ref().map(|k| format!("◦ {}  {}  {}", k.verb, k.what, k.range));
                    let row = [row.unwrap_or_default().trim_end(), m.text.trim()].join(" ");
                    ("tool", row.trim().to_string())
                }
            })
            .collect();
        Ok(ChatTurn {
            session: session_id(l.id),
            name: l.label().to_string(),
            doing: self.doing_said(l),
            rows,
        })
    }

    /// Session `which`: by id (`s1`) or name, empty for the one in front. Err names the sessions there are.
    fn asked_session(&self, which: &str) -> Result<&crate::session::Session, String> {
        let all = || std::iter::once(&self.session).chain(&self.sessions);
        all()
            .find(|l| match which {
                "" => l.id == self.session.id,
                w => session_id(l.id) == w || l.label() == w,
            })
            .ok_or_else(|| {
                let there: Vec<String> =
                    all().map(|l| format!("{} {}", session_id(l.id), l.label())).collect();
                format!("no session {which} — there are: {}", there.join(", "))
            })
    }

    /// `which` session's reviewer, whole (`ReviewReport`): as `chat_turn` names sessions.
    pub fn review_report(&self, which: &str) -> Result<ReviewReport, String> {
        let l = self.asked_session(which)?;
        let findings = crate::reviewer::findings_in(l).unwrap_or_default();
        let root = l.reviewer.as_ref().map_or(&l.root, |r| &r.root);
        Ok(ReviewReport {
            session: session_id(l.id),
            name: l.label().to_string(),
            reviewer: ReviewerState::of(self, l),
            verdict: l.reviewer.as_ref().map(|r| r.verdict.clone()).unwrap_or_default(),
            findings: findings
                .into_iter()
                .map(|n| Finding {
                    path: crate::chat::shown(root, &n.path).to_string(),
                    lines: n.lines.start + 1..n.lines.end.max(n.lines.start + 1) + 1,
                    high: crate::reviewer::on_high_in(self, l, &n.path, &n.lines),
                    answered: crate::reviewer::answered(n),
                    thread: n.thread.iter().map(|e| (e.by.label(), e.text.clone())).collect(),
                })
                .collect(),
            unseen: l.code_review.as_ref().map(|r| self.high_unseen_of(l, r)),
        })
    }

    /// Session `l`'s high-risk changes no one has looked at in its review `r`, in the rail's order — with their
    /// lines' text where the file is open (a file not open isn't read here: the main thread).
    fn high_unseen_of(&self, l: &crate::session::Session, r: &crate::codereview::CodeReview) -> Vec<Unseen> {
        let mut out = Vec::new();
        for i in r.rail_order() {
            let f = &r.files[i];
            let doc = self.docs.iter().find(|d| d.path.as_deref() == Some(f.path.as_path()));
            for g in r.graded(self, f).iter().filter(|g| g.grade == crate::risk::Grade::High) {
                if g.looked_at(&l.looked) {
                    continue;
                }
                let left = g.left(&l.looked);
                let since = (g.keys.iter().any(|k| l.looked.contains(k))).then_some(left.len());
                let lines = g.at + 1..g.at + g.lines.max(1) + 1;
                let text = doc.map(|d| {
                    (g.at..g.at + g.lines)
                        .filter(|&n| n < d.text.len_lines())
                        .map(|n| d.text.line(n).to_string().trim_end_matches(['\n', '\r']).to_string())
                        .collect()
                });
                out.push(Unseen { path: f.rel.clone(), lines, why: g.why.clone(), since, text });
            }
        }
        out
    }

    /// The frame drawn last, as the terminal got it (`Asked::Frame`) — None before the first.
    pub fn frame_json(&self) -> Option<Value> {
        let (frame, cols, rows) = self.last_frame.as_ref()?;
        Some(json!({
            "v": V,
            "frame": {
                "cols": cols, "rows": rows, "frames": self.frames, "input": self.input_seq,
                "bytes": String::from_utf8_lossy(frame),
            },
        }))
    }
}

/// A session's reviewer as `space v r` reads it, for a driver (#213: the verdict and findings were only on the
/// screen, under toasts) — the user's level only, as `ChatTurn` is.
pub struct ReviewReport {
    pub session: String,
    pub name: String,
    /// None: it never had a reviewer since tarae started.
    pub reviewer: Option<ReviewerState>,
    /// The verdict, whole.
    pub verdict: String,
    pub findings: Vec<Finding>,
    /// The high-risk changes of the session's open review no one has looked at (#214, the read half: a driver
    /// reads them; only a person's key in review mode marks one) — None: no review open.
    pub unseen: Option<Vec<Unseen>>,
}

/// A high-risk change not looked at yet.
pub struct Unseen {
    pub path: String,
    /// 1-based, its end exclusive.
    pub lines: std::ops::Range<usize>,
    /// The rule and whose: `unsafe (rust pack)`.
    pub why: String,
    /// Looked at before, but for this many lines since.
    pub since: Option<usize>,
    /// Its lines as they are, where the file is open.
    pub text: Option<Vec<String>>,
}

/// One of the reviewer's findings still pinned.
pub struct Finding {
    /// From the checkout it reviewed.
    pub path: String,
    /// 1-based, its end exclusive.
    pub lines: std::ops::Range<usize>,
    /// On a high-risk change — None when its review isn't the one open.
    pub high: Option<bool>,
    pub answered: bool,
    /// (who, text): `reviewer` · `you` · `author`.
    pub thread: Vec<(&'static str, String)>,
}

impl ReviewReport {
    pub fn json(&self) -> Value {
        let findings: Vec<Value> = self
            .findings
            .iter()
            .map(|f| {
                json!({
                    "path": f.path, "line": f.lines.start, "end": f.lines.end - 1, "high": f.high,
                    "answered": f.answered,
                    "thread": f.thread.iter().map(|(by, text)| json!({ "by": by, "text": text })).collect::<Vec<_>>(),
                })
            })
            .collect();
        let unseen = self.unseen.as_ref().map(|u| {
            u.iter()
                .map(|c| {
                    json!({
                        "path": c.path, "line": c.lines.start, "end": c.lines.end - 1, "why": c.why,
                        "since": c.since, "text": c.text,
                    })
                })
                .collect::<Vec<_>>()
        });
        json!({
            "v": V,
            "review": {
                "session": self.session, "name": self.name,
                "reviewer": self.reviewer.as_ref().map(ReviewerState::json),
                "verdict": self.verdict, "findings": findings, "high_unseen": unseen,
            },
        })
    }

    pub fn text(&self) -> String {
        let mut out = match &self.reviewer {
            Some(r) => vec![format!("reviewer of {} {} ({}) — {}", self.session, self.name, r.of, r.text())],
            None => {
                vec![format!("reviewer of {} {} — none yet (space v c asks one)", self.session, self.name)]
            }
        };
        self.reviewer_text(&mut out);
        // (#214: what's left for a person to look at — read here, marked only by their key)
        match self.unseen.as_deref() {
            None => {}
            Some([]) => out.push("\nno high-risk change left to look at".into()),
            Some(u) => {
                out.push(format!(
                    "\n{} not looked at (a person's space v h marks them):",
                    crate::nav::count(u.len(), "high-risk change")
                ));
                for c in u {
                    let lines = match c.lines.len() {
                        1 => format!("{}", c.lines.start),
                        _ => format!("{}–{}", c.lines.start, c.lines.end - 1),
                    };
                    let since = c
                        .since
                        .map(|n| format!(" · {} since a look", crate::nav::count(n, "line")))
                        .unwrap_or_default();
                    out.push(format!("- {}:{lines} {}{since}", c.path, c.why));
                    out.extend(c.text.iter().flatten().map(|l| format!("    {l}")));
                }
            }
        }
        out.join("\n") + "\n"
    }

    /// The reviewer's verdict and findings, as lines (none: nothing).
    fn reviewer_text(&self, out: &mut Vec<String>) {
        if self.reviewer.is_none() {
            return;
        }
        if !self.verdict.is_empty() {
            out.push(format!("\nverdict:\n{}", self.verdict.trim_end()));
        }
        out.push(match self.findings.is_empty() {
            true => "\nno findings".into(),
            false => "\nfindings:".into(),
        });
        for f in &self.findings {
            let lines = match f.lines.len() {
                1 => format!("{}", f.lines.start),
                _ => format!("{}–{}", f.lines.start, f.lines.end - 1),
            };
            let high = if f.high == Some(true) { " ▲ high" } else { "" };
            let answered = if f.answered { " · answered" } else { "" };
            out.push(format!("- {}:{lines}{high}{answered}", f.path));
            for (by, text) in &f.thread {
                let mut it = text.trim_end().lines();
                out.push(format!("  {by}: {}", it.next().unwrap_or_default()));
                out.extend(it.map(|l| format!("    {l}")));
            }
        }
    }
}

/// After each frame (`term::render`): the gone ring and the watchers (`watch.rs`), then the snapshots waiting
/// for it. Nobody asking — a branch each.
pub fn after_frame(ed: &mut Editor) {
    watch::after_frame(ed);
    // (a flash over: this frame drew it gone — nothing to look ids up for, no frame to ask for)
    if ed.ui_flash.as_ref().is_some_and(|f| f.1.elapsed() >= claude::FLASH) {
        ed.ui_flash = None;
    }
    if ed.ui_asks.is_empty() {
        return;
    }
    let now = std::time::Instant::now();
    ed.ui_asks.retain(|a| a.until > now);
    let (ready, waiting): (Vec<Ask>, Vec<Ask>) =
        std::mem::take(&mut ed.ui_asks).into_iter().partition(|a| ed.input_seq >= a.after_input);
    ed.ui_asks = waiting;
    if ready.is_empty() {
        return;
    }
    // (built only when one asks for it, not for a chat)
    let state = ready.iter().any(|a| a.what == Asked::State).then(|| ed.ui_state());
    for a in ready {
        let error = |e: String| match a.json {
            true => json!({ "error": e }).to_string(),
            false => format!("error: {e}\n"),
        };
        let said = match (&a.what, &state) {
            (Asked::State, Some(s)) if a.json => s.json().to_string(),
            (Asked::State, Some(s)) => s.text(),
            (Asked::State, None) => continue,
            (Asked::Chat(w), _) => match ed.chat_turn(w) {
                Ok(t) if a.json => t.json().to_string(),
                Ok(t) => t.text(),
                Err(e) => error(e),
            },
            (Asked::Review(w), _) => match ed.review_report(w) {
                Ok(r) if a.json => r.json().to_string(),
                Ok(r) => r.text(),
                Err(e) => error(e),
            },
            (Asked::Features, _) => {
                ed.features_answer(a.json, a.reply);
                continue;
            }
            // (after a frame: there is one)
            (Asked::Frame, _) => {
                ed.frame_json().map_or_else(|| error("nothing drawn yet".into()), |f| f.to_string())
            }
        };
        let _ = a.reply.send(said);
    }
}

pub mod claude;
pub mod panels;
pub mod point;
pub mod watch;

#[cfg(test)]
mod tests;
