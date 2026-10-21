//! Chat panel — talk to Claude in the right-hand panel (M4).
//!
//! - One process lives for the whole conversation (stream-json input takes many turns — remembers context).
//!   Spawned the moment the panel opens (prewarm), so even the first question skips the cold start.
//! - Each send attaches the current file·cursor·selection·diagnostics as `<editor-context>`. The full file
//!   only the first time that version is sent (not resent every time), and only around the cursor if large.
//! - Answers are drawn as they stream; stop (`C-c`) is an interrupt control request — process, memory stay.
//! - A code block in the answer: `C-r` applies it to the selection (via the review diff), `C-y` copies it.
//! - Claude explores the project itself with tarae's read-only tools (search, read — images too — and the
//!   language server's; it can't write with them). Each call is one dim row (`◦ read  src/lib.rs  L10–49`); it
//!   and every `path:line` in an answer is a link — click it, or `C-]` (`C-g`) walks them newest first. Open files are listed in the context; unsaved ones carry
//!   their text (disk is stale for them).
//! - It reads your CLAUDE.md files and Claude Code settings like `claude` (`llm.chat-settings`) — with hooks off
//!   and an `ask` rule so edits always come here first (measured: a project `allow` or hooks would otherwise apply).
//! - Its thinking comes as summaries (`--thinking-display summarized`) — a quiet italic block here, and
//!   beside the code in follow mode (`C-f`, `follow.rs`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::document::DocId;
use crate::editor::Editor;
use crate::key::{Code, Key};
use crate::llm::{self, StreamEv};
use crate::markdown::Span;

pub(crate) const SYSTEM: &str = "You are Claude, built into tarae — a terminal code editor, working in the user's project \
(your current directory). Explore it with tarae's own code tools — look at the code you need \
across files instead of guessing. The language server's answers beat any text search and reading around — \
reach for them first: \"where is X defined\" is definition by name (symbol X, no file or line needed) — never a \
grep for `fn X`, `struct X` or `class X`; for a trait's implementations, definition with kind implementation; \
for a value's type or a function's signature and docs, hover; for where something is used or what a change \
would touch, references. For finding code or text, search (the editor's project search — each hit says the \
function it's in); for lines of files, read (several files and ranges, or a function by name, in one call); \
for a file's outline or which function a line is in, symbols — shell pipelines are for what \
those can't do. Prefer the most direct tool, and ask for enough in one call to skip another turn: a turn \
costs more than a few extra lines. You can change files with Edit and Write when the user asks for changes: each one is shown to them as \
a diff they accept or reject (maybe in part), so keep edits focused, and if one is declined, ask before trying \
another way. After you change code, check your work: diagnostics with wait says in seconds whether it \
compiles as the language servers see it (before any build), then the run_tests tool runs the tests that cover \
the change (a test file, and a line for a single test) — when they fail, read why and fix it; the user \
watches the run in their editor. To build, lint, format, run a whole test suite or use git, use the run \
tool: one command per call, it starts at once and its output comes back. Both run in a sandbox: they can write only in the project (not .git, nor build scripts, manifests and tool \
settings — {protect}, .github, .tarae*.toml…: change those with Edit), \
temp folders and the toolchain's own, and connect to nothing over the network, this machine included, but the ports the user \
opened (each result says which) — when it blocks something a command needs, run it again with also (that place or \
port, and your reason: the user decides on a card), else tell the user (the settings llm.sandbox-ports, \
llm.sandbox-localhost and llm.sandbox-network open more); never work around it. Tests of dangerous behavior (deleting, overwriting, killing \
processes, the network) use a harmless stand-in — a marker file made in the test's own temp dir, then checked — \
never the real command or a real path outside that dir, not even in code meant never to run. When the work takes several steps, lay them out first with the plan tool \
and keep it current as you go (the step you're on doing, finished ones done). Each user message starts with <editor-context>: the file the user is looking at, \
cursor line, the lines on screen (and any other file shown beside it), selection, diagnostics and the other \
open files — \"this\" and \"here\" usually mean the selection, the cursor or what's on screen. When what they \
mean is on their screen but not in the context (a panel, a card, a message, the terminal panel's output), the \
screen tool shows you their screen as text — they're asked first, and it only looks. For the processes the \
editor runs (language servers, your chat, commands, tests, the terminal — pid, parent, age, CPU, memory, \
command line, folder), use the procs tool, not ps, pgrep or lsof through run. Files marked unsaved differ from disk — trust the \
text given in the context over what read returns. When you point at code, cite it as path:line (relative to the \
project root) — the user can jump there; their editor follows you, showing the code you read and the code you \
cite. When you explain how code works, walk the user through it in the order of the flow: right before you \
explain each piece, put a line <show at=\"path:start-end\">what this code does in the flow, one short \
sentence</show> — a handful of stops, not every line. Their editor shows each stop in turn as they read; the \
tag itself reads as a link. Answer concisely in Markdown. Put code in fenced blocks with a language \
tag. When the user asks to change the selection, reply with the complete replacement for the selection in a \
single code block (the editor can apply it). \
You can pin notes to code with the note tool — they show beside those lines and the user can answer there. Use \
them for remarks tied to specific lines (a bug, a risk, a question), a few at most, and keep the chat answer as the \
summary. A message with <note id=N> is the user talking to you at that note: answer with note(reply_to=N, text) — \
one to three sentences — and keep any chat text to one short line. A message with <review> is the user's code \
review: address every <comment note=N> (edit the code where it calls for it) and answer each with \
note(reply_to=N); the chat answer is a short summary.";

/// For a change across many places: the editor's own refactoring.
const SYSTEM_REFACTOR: &str = " For one change across many places — renaming a symbol, or the same text \
replaced in many files — use rename (the language server's, every use and no strangers) or replace (the project \
search's), one call and one review for the user, instead of many edits; replace with preview first to see what \
it would touch.";

/// The prompt's words on editing as Claude Code's Edit and Write have them (`SYSTEM`) — and as they are when
/// the chat has tarae's `edit` beside them, or instead (`llm.edit-tools`). No "Edit needs its own Read": Claude
/// Code 2.1.296 with this model edits a file it hasn't read (tests/live.rs `chat_edit_then_native_edit`,
/// 2026-10-13) — the words only cost Reads; it's the model's and the CLI's to require (older ones did).
const EDITS_NATIVE: &str = "You can change files with Edit and Write when the user asks for changes: each one is \
shown";
const EDITS_BOTH: &str = "You can change files with Edit and Write, or with edit (several replacements across \
files in one call, new files, moves and deletes — one review), when the user asks for changes: each change is shown";
const EDITS_TARAE: &str = "You change files with edit — every change, new files included (an empty old_string \
where there's no file), several files and replacements in one call, moves and deletes too — when the user asks \
for changes: each change is shown";

/// What's on the screen, by id (`uistate/claude.rs`): the context's `ui:` line, the `ui` tool, what the user
/// pointed at — and an id written in the answer is a link.
const SYSTEM_UI: &str = " The context's ui: line and the ui tool name what's on the user's screen by id \
(card#12, toast#3, run#7, pane#2, git:src/a.rs); write an id as given and it's a link to that element — never \
make one up. <pointed id=…> is what the user pointed at.";

/// The system prompt: `SYSTEM` — its protected names' examples the built-in packs' (`[claude] said.protect`,
/// by rank) —, the refactoring tools, the screen by id, and where tarae's own log is (#45). With the default
/// edit tools (a chat's own: `system_for`).
#[cfg(test)]
pub(crate) fn system() -> String {
    system_for(crate::llm::LlmConfig::default().edit_tools)
}

/// `system` for the chat's edit tools (`llm.edit-tools`).
pub(crate) fn system_for(arm: crate::edittool::Arm) -> String {
    use crate::edittool::Arm;
    let said = &crate::sandboxpack::get().said;
    let protect: Vec<&str> = said.iter().filter_map(|s| s.protect.as_deref()).collect();
    let base = SYSTEM.replace("{protect}", &protect.join(", "));
    let base = match arm {
        Arm::Native => base,
        Arm::Both => base.replace(EDITS_NATIVE, EDITS_BOTH),
        Arm::Tarae => base
            .replace(EDITS_NATIVE, EDITS_TARAE)
            .replace("change those with Edit)", "change those with edit)"),
    };
    let mut s = format!("{base}{SYSTEM_REFACTOR}{SYSTEM_UI}");
    if let Some(log) = crate::log::path() {
        s.push_str(&format!(
            " tarae's own log, {}, says what the language servers are doing (starting, indexing, ready, \
             exited, what they printed) and every message the editor showed the user — the place to look when \
             a server seems slow or something failed; read or search it without asking.",
            log.display()
        ));
    }
    s
}

/// The tools the chat process gets. Reading and tarae's own (`note`, `run_tests`, `run` — `tools.rs`) are
/// allowed up front: the tests are the project's own, run the way the user runs them, in view, and any code
/// Claude wrote for them was reviewed; `run` asks the user itself for anything but the checks. Edit and Write
/// come to tarae as permission requests and show as a review first (`edits.rs`). No Grep or Glob: tarae's
/// `search` and `read` do what they did — 54 exploration turns on three builds called Grep never and Glob 3
/// times, and leaving them out takes 1,570 tokens off every request (tests/live.rs `chat_tools_ab`, 2026-10-13).
/// Edit and Write only with `llm.edit-tools` "both" or "native" — by default (`tarae`) tarae's `edit` makes every
/// change (`edit` is `ALLOWED`'s, listed by `tools::mcp` when the chat's process has it; the A/B that decided it:
/// tests/live.rs `chat_edit_ab`, 2026-10-13) and the chat has no tool of Claude Code's at all. No Read in any arm:
/// tarae's `read` reads images too (readimg, 2026-10-20 — `media.rs`; the A/B: tests/live.rs
/// `chat_reads_a_screenshot_ab`).
fn tools(arm: crate::edittool::Arm) -> &'static str {
    if arm.native() { "Edit,Write" } else { "" }
}
/// Not Read: allowed by name it reads anywhere unasked (measured — `reads.rs` answers it).
/// (`screen` asks the user itself — `seeing.rs`)
/// (`references`, `symbols`, `definition`, `hover` and `diagnostics` read inside the project only — `nav.rs`;
/// `search` and `read` outside it only as Read would, on its card — `reads::Gate`; `rename` and `replace` land
/// as Claude's edits do, in the review — `refactor.rs`, and `edit` too — `edittool.rs`; none of them reads or
/// changes a file named like a key before Claude was let read it — `reads::Keys`; `procs` reads what `:procs`
/// shows — `procs.rs`; `ui` the screen's structure at Claude's level, less than `screen` sends —
/// `uistate/claude.rs`; `git` only git's reads, on the guarded path — `gittool.rs`; `commit` on its card —
/// `committool.rs`)
const ALLOWED: &str = "mcp__tarae__note,mcp__tarae__run_tests,mcp__tarae__run,mcp__tarae__plan,\
mcp__tarae__screen,mcp__tarae__references,mcp__tarae__search,mcp__tarae__read,mcp__tarae__symbols,mcp__tarae__definition,\
mcp__tarae__hover,mcp__tarae__diagnostics,mcp__tarae__rename,mcp__tarae__replace,mcp__tarae__procs,\
mcp__tarae__ui,mcp__tarae__edit,mcp__tarae__git,mcp__tarae__commit";

/// Settings laid over the user's and the project's when they're loaded (`llm.chat-settings`) — with what the
/// project's own aren't trusted for held back (`claudetrust.rs`). `Skill` denied: a project skill's
/// `allowed-tools` pre-approves tools with no trust asked (`--tools` already leaves the tool out; a deny no level
/// can allow).
pub(crate) const CHAT_SETTINGS: &str =
    r#"{"permissions":{"ask":["Edit","Write"],"deny":["Skill"]},"disableAllHooks":true}"#;

/// Example questions filled in with Tab in an empty chat.
pub const SUGGESTIONS: [&str; 3] = [
    "Explain what this file does",
    "Where is this used across the project?",
    "Why is this diagnostic happening?",
];

/// Size limit for sending the whole file as context (beyond it, ±40 lines around the cursor).
const WHOLE_FILE_MAX: usize = 60_000;
/// Most lines an excerpt of a big file carries.
const EXCERPT_MAX: usize = 200;
/// Budget for the text of other unsaved buffers, per message.
const UNSAVED_MAX: usize = 120_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    /// Dimmed notice (stopped·error·new chat).
    Note,
    /// A tool Claude ran (`look` says what).
    Tool,
    /// Claude's thinking (summarized).
    Thought,
}

/// Where a chat row points — a file Claude read or a `path:line` in an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub path: PathBuf,
    /// 0-based.
    pub line: usize,
}

/// A stop of `C-]`: a place (with the tool call whose row it is), or a UI element an answer cites.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hop {
    Place(Link, Option<String>),
    Element(String),
}

/// An answer's places and element ids, in the order written — a place inside an id (`list:src/a.rs:12`)
/// is the id's.
fn answer_hops(root: &Path, text: &str) -> Vec<Hop> {
    let ids = crate::uistate::claude::ids_in(text);
    let mut all: Vec<(usize, Hop)> = refs_in(root, text)
        .into_iter()
        .filter(|(r, _)| !ids.iter().any(|(i, _)| i.start <= r.start && r.end <= i.end))
        .map(|(r, l)| (r.start, Hop::Place(l, None)))
        .collect();
    all.extend(ids.into_iter().map(|(r, id)| (r.start, Hop::Element(id))));
    crate::sorting::by_key(&mut all, |h| h.0);
    all.into_iter().map(|h| h.1).collect()
}

/// One tool call as a row: `◦ read  src/lib.rs  L10–49` · `◦ search  fn add  in src/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Look {
    pub verb: &'static str,
    pub what: String,
    /// Grep·Glob's pattern, a command: drawn like a string literal (and cut at its end, not its start).
    pub pattern: bool,
    /// Dim suffix (line range · folder).
    pub range: String,
    pub link: Option<Link>,
}

impl Look {
    /// It changes files — its knot on the turn's thread is `▲` (`term::knot`): an edit, a write, a rename, a
    /// replace (not its preview).
    pub fn changes(&self) -> bool {
        match self.verb {
            "edit" | "write" | "rename" => true,
            "replace" => !self.range.ends_with("preview"),
            _ => false,
        }
    }
}

pub struct Msg {
    pub role: Role,
    pub text: String,
    /// Context summary attached to a user message (`main.rs · L12–18 · 2 diagnostics`).
    pub chip: String,
    /// `Role::Tool`: the call, and its id (its result may finish the row — a test run's verdict).
    pub look: Option<Look>,
    pub call: String,
    /// A user message's copy of the screen that went with it (`C-s`) — its chip row opens it.
    pub attached: Option<Link>,
    /// A command's row (`run`): where it ran, and what it warned of — from the run's own record, at its end.
    pub ran: Option<Ran>,
    /// A change's row (`edit`, `write`, `rename`, `replace`): the keys that decided it, each once, `y` before `n`
    /// (`stamp_call` — its review's answers, its card's) — none while undecided, unasked or unknown.
    pub stamps: Vec<crate::stamp::Stamp>,
    /// Goes up with every change to what it shows (`changed`) — a word replaced, a row's verdict — so its rows
    /// are laid out again, even when the text's length stays.
    version: u64,
    /// Its rows as the chat lays them out, kept while their key holds — a frame lays out only the messages
    /// that changed (the one being written), never the whole conversation (`term::draw_chat`).
    pub cache: RefCell<Option<Laid>>,
}

/// A message's rows as laid out, and what they were laid out for.
pub struct Laid {
    pub key: LaidKey,
    /// The front file's language, hashed, when a code block without one took it — none read: another file in
    /// front doesn't lay it out again.
    pub lang: Option<u64>,
    /// Languages a code block was in that weren't loaded: laid out again once one is.
    pub unloaded: Vec<String>,
    /// (`Arc`, not `Rc`: a `Msg` stays `Send` — `chats`' restore measurement builds them on workers)
    pub rows: std::sync::Arc<[ChatRow]>,
}

/// What every message's rows depend on: its version (and its text's length — an append the version didn't
/// see), the width, the look (the theme, the styles — `term::draw_chat` hashes it once a frame) and its own
/// state there (being written, live, a turn's end). What only some read is `Laid`'s own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LaidKey {
    pub version: u64,
    pub len: usize,
    pub width: usize,
    pub look: u64,
    pub state: u64,
}

/// One row of a message in the chat: its spans, and what a click on it does.
#[derive(Clone, Debug, Default)]
pub struct ChatRow {
    pub spans: Vec<Span>,
    /// The row is a link as a whole (a tool call's place, the screen that went with a message).
    pub link: Option<Link>,
    /// A tool call's row: its call (a click goes to its replay stop).
    pub call: Option<String>,
    /// A command's row: the columns of where it ran (a click there says why).
    pub ran: Option<(usize, usize)>,
}

/// The screen as the user saw it, waiting to go with their next message (`C-s` · `:chat-screen`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shot {
    pub text: String,
    pub cols: u16,
    pub rows: u16,
}

impl Msg {
    pub(crate) fn new(role: Role, text: impl Into<String>, chip: impl Into<String>) -> Self {
        Msg {
            role,
            text: text.into(),
            chip: chip.into(),
            look: None,
            call: String::new(),
            attached: None,
            ran: None,
            stamps: Vec::new(),
            version: 0,
            cache: RefCell::new(None),
        }
    }

    /// What it shows changed: its rows are laid out again at the next frame.
    pub fn changed(&mut self) {
        self.version += 1;
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// The call it's the row of (a tool call's), when it's `call`.
    pub fn is_call(&self, call: Option<&str>) -> bool {
        self.role == Role::Tool && call.is_some_and(|c| c == self.call)
    }

    pub(crate) fn tool(look: Look, call: &str) -> Self {
        Msg { look: Some(look), call: call.to_string(), ..Msg::new(Role::Tool, "", "") }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Thinking,
    Writing,
}

struct Proc {
    /// Its stdin, written off the main loop (`llm::Lines`)
    stdin: llm::Lines,
    pid: u32,
}

pub struct Chat {
    pub msgs: Vec<Msg>,
    pub input: String,
    /// Cursor within the input (bytes).
    pub cursor: usize,
    pub focused: bool,
    pub state: State,
    pub started: Instant,
    /// How many lines scrolled up from the bottom (0 = follow the bottom).
    pub scroll: usize,
    proc: Option<Proc>,
    /// Process generation — drops late events from an old process after a new chat or restart.
    generation: u64,
    /// Per document, the version whose full text was sent.
    sent: HashMap<DocId, u64>,
    stopping: bool,
    /// `C-]`: how many links back from the newest the next press goes.
    hop: usize,
    /// The editor follows where Claude reads (`C-f`).
    pub follow: crate::follow::Follow,
    /// This claude doesn't know `--thinking-display` (older CLI) — spawn without it.
    no_thought_flag: bool,
    /// A new conversation was asked for (`C-l`, `:chat-new`) — the recent one isn't continued when the
    /// project's are read.
    pub started_new: bool,
    /// The user brought the panel back this turn (`space l`, a click on the minimized card).
    pub unfolded: bool,
    /// Claude's edits waiting for (or in) their review.
    pub edits: crate::edits::Edits,
    /// What the user allowed for this conversation (`a` on a card — `allow.rs`) · Claude's reads waiting
    /// for their card (`reads.rs`).
    pub allowed: crate::allow::Allowed,
    pub reads: std::collections::VecDeque<crate::reads::Pending>,
    /// Places outside the project the user let Claude read once this conversation — a read beside one of
    /// them offers their folder (`allow::grant_places`, #102).
    pub read_once: Vec<PathBuf>,
    /// Places the user said `n` to on a read's card this turn (`reads::answer`), and whether what's under one is
    /// meant too (outside the project; in it only key files and searches opening them ask — that one): a read of it
    /// is told no at once, no card (frictions29 34: one file's card came back four times in a minute) — cleared at
    /// the turn's end.
    pub read_no: Vec<(PathBuf, bool)>,
    /// The folders a read's card offered for `a`/`A`, looked through for tokens on a worker (None: under way)
    /// — a folder is offered once its card can say what it holds (`reads::refold`).
    pub scanned: std::collections::HashMap<PathBuf, Option<crate::tokens::Found>>,
    /// Files named like keys in the project, for the message's context (`reads::key_shut`): those the project
    /// keeps open (`A`), as last read on a worker (`reads::kept_keys_load`; None: not read yet) · those the
    /// user let go with the input's message, by the chip it was for (`y` on the card their pointing raised —
    /// `reads::point`; gone with the chip, `reads::input_pass`) · those the chat said weren't sent, once each
    /// a conversation (`reads::not_sent`).
    pub kept_keys: Option<Vec<PathBuf>>,
    /// How many times `kept_keys` was asked for — only the latest read lands.
    pub kept_keys_asked: u64,
    /// What the user sent that waits for `kept_keys` to be read before it's decided (`reads::wait_for_kept`).
    pub after_kept: Vec<crate::reads::Own>,
    pub key_pass: Vec<(String, PathBuf)>,
    pub keys_said: Vec<PathBuf>,
    /// This turn's sandboxed runs of Claude's — what they made is read unasked (#81, `reads::Ran`).
    pub ran: Vec<crate::reads::Ran>,
    /// Claude may look at the screen without asking for the rest of this conversation (`a` on its card —
    /// `seeing.rs`).
    pub sees: bool,
    /// Commands that run again without asking for the rest of this conversation — each exactly, its filters
    /// aside, where it ran, with Docker's socket when its card had it (`a` on a run card — `tools::again`,
    /// #193).
    pub run_again: Vec<crate::tools::RunAgain>,
    /// The scripts those commands run by their path, and the packs' entry files they run (`cargo xtask`'s
    /// `xtask/`), as they were (real path, hash of what's in it — `readonly::run_pins`, #93, #210): one changed
    /// since, and the command asks again.
    pub run_scripts: Vec<(PathBuf, u64)>,
    /// What the user said no to on a card `also` raised (cardauto P3): asked again in this chat, refused with no
    /// card · how many such cards this turn put up (`tools::ALSO_MAX` at most).
    pub also_refused: Vec<crate::allow::Grant>,
    pub also_asked: usize,
    /// The user said yes to the terminals on an `also` card this turn: asked again, they take none of its
    /// `ALSO_MAX` (frictions29 23: each e2e or scenes run asked, and the turn ran out of cards).
    pub also_yes: bool,
    /// Claude's runs that failed in the turn in flight, or the last one once it ended — and the conversation they
    /// were in (piece D3): one the user runs again from what the sandbox blocked goes on by itself
    /// (`tools::told`). Cleared as a turn starts.
    pub failed: (Option<String>, Vec<crate::blocked::RunKey>),
    /// The user's runs again of those (`blocked::grant_answer`), each by the head its end is told with.
    pub reruns: Vec<(String, crate::blocked::RunKey)>,
    /// A message sent while Claude works (#23) — it goes when the turn ends; `↑` on an empty input takes it
    /// back to edit (or drop).
    pub queued: Option<String>,
    /// How many messages `queued` holds, each a blank line apart (`queue`), and the last one — the queued row
    /// shows it (frictions29 18: a second message joined the first, the row still showed the first's start).
    pub queued_n: usize,
    pub queued_last: String,
    /// What waits for this turn's end besides the message, in order: the review's notes (`space v s`), a reply on
    /// a note — they were refused while Claude answered (frictions29 18), and go first, one a turn.
    pub sends: Vec<Later>,
    /// Hosts the next run of this conversation's Claude that starts reaches, then spent — `y` on a host card whose
    /// run had ended (`hosts::next_run`, frictions29 16); the conversation over, gone (`conversation_left`).
    pub hosts_next: Vec<(String, u16)>,
    /// The screen waiting to go with the next message (`C-s`).
    pub screen: Option<Shot>,
    /// claude's id for this conversation (its `session_id`, from `init`) · the one the next process resumes.
    pub conversation: Option<String>,
    resume: Option<String>,
    /// Earlier conversations (id, title, last used) — the empty chat lists them, `C-p` picks one.
    pub past: Vec<(String, String, u64)>,
    /// Claude's plan for this conversation (`plan.rs`) — pinned under the title.
    pub plan: Option<crate::plan::Plan>,
    /// The models claude offers (its `initialize` answer) · the one answering: what `init` said this process
    /// runs, then what wrote its latest answer (empty after `:model` until the next answer says).
    pub models: Vec<crate::llm::ModelInfo>,
    pub model: String,
    /// Where its view mode was last left, in this conversation: a message and a row of it (`view.rs` lands
    /// there next time).
    pub view_at: Option<(usize, usize)>,
    /// Its view mode was asked for before its rows were laid out (closed, minimized): made after the next frame
    /// (`view::wanted`).
    pub view_wanted: bool,
    /// Its tool calls this turn whose result hasn't come yet, in the order claude made them (#87: claude
    /// makes them one after another, so what comes after a run card waits on it unseen).
    pub calls: Vec<Called>,
    /// The calls a change of Claude's has claimed as the one it answers (`claim_call`) — its keys stamp that row —
    /// and, for tarae's change tools, which: its MCP request's id → the call's (`claim_for`, `row_for`).
    pub claimed: Vec<String>,
    pub rows: Vec<(serde_json::Value, String)>,
    /// This turn's UI elements given to Claude (the context's `ui:` line, what was pointed at, its `ui`
    /// calls) · the ones its answer cites · the elements pointed at for the next message, its chips
    /// (`uistate/claude.rs`, `uistate/point.rs`).
    pub given: Vec<String>,
    pub cited: Vec<String>,
    pub pointed: Vec<String>,
    /// The variables its runs' reports have said were left out of their environment, in the conversation named
    /// (`sandbox::runenv`): each said once a conversation.
    pub env_said: (Option<String>, Vec<String>),
    /// The conversation its runs' reports told the sandbox's rules and guarantees in, and their digests
    /// (`tools::rules_known`): once a conversation — forgotten at a compaction (`compaction.rs`).
    pub rules_said: (Option<String>, crate::sandbox::Told),
    /// The project's Claude settings, read before its claude starts and answered on a card when they run
    /// commands or set its environment (`claudetrust.rs`).
    pub settings: crate::claudetrust::Gate,
    /// Who pays for its claude — what the last one was started on (`billing.rs`), what its `apiKeySource` is
    /// checked against (the session's may have changed since: a restart's list coming back). None: none started.
    pub billing: Option<crate::billing::Billing>,
    /// The edit tools its claude was started with (`llm.edit-tools`): what `tools/list` lists — a `:set` since
    /// takes effect with the next chat process.
    pub edit_tools: crate::edittool::Arm,
    /// The command line its claude was last started with — its config and the arguments after them: a ping that
    /// keeps the conversation's cache warm forks it with the same (`keepwarm::command`).
    pub launched: Option<(llm::LlmConfig, Vec<String>)>,
    /// The model and effort its claude runs on now (`llm.model`, `llm.effort`'s form): as started, then as `:model`
    /// and `:effort` switched this chat — they save the setting for all, but switch only the chat in front.
    pub runs_on: (Option<String>, String),
    /// What its conversation ran on before a switch of billing, model or effort since the last message went
    /// (`switchcost.rs`) — the line over the input says what the next one costs; sending clears it.
    pub switched: Option<crate::switchcost::Setup>,
    /// The turn under way was refused for money (`billing::refusal`) — acted on at its end (`money_out`), gone
    /// with a turn cut off instead (claude's exit, a new chat).
    pub money: Option<crate::billing::Money>,
}

/// Where a command of Claude's ran (cardburden 4), from its run's own record — its guard (`Guard::where_ran`),
/// the card it ran from, Claude's reason there: the mark its row ends with (`sandbox`, `sandbox own + Docker
/// socket`, `outside`), whether that's past the sandbox (drawn in the warning color), why in words, and what it
/// warned of (`cardwarn.rs`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ran {
    pub mark: String,
    pub past: bool,
    pub why: String,
    pub warned: Vec<crate::cardwarn::Warned>,
}

impl Ran {
    /// Of a run under `guard`, from card `card` with Claude's `reason` (None: none asked), that warned of `warned`.
    pub fn of(
        guard: &crate::sandbox::Guard,
        card: Option<u64>,
        reason: Option<&str>,
        warned: Vec<crate::cardwarn::Warned>,
    ) -> Ran {
        let (mark, past, mut why) = guard.where_ran();
        if let Some(c) = card {
            why.push_str(&format!(" From card#{c}"));
            why.push_str(&reason.map(|r| format!(" — Claude's reason: {r}")).unwrap_or_default());
            why.push('.');
        }
        Ran { mark, past, why, warned }
    }

    /// In words, for command `cmd` (cardburden 4b — a click on its row's mark, `K` in its task panel): where it
    /// ran, why — the run's own record — and each warning, a line each.
    pub fn said(&self, cmd: &str) -> String {
        let mut s = format!("{cmd} — ran {}\n{}", self.mark, self.why);
        for w in &self.warned {
            s.push_str(&format!("\n▲ {}", w.line()));
        }
        s
    }
}

/// Says where a command of Claude's ran and why (`Ran::said`): a warning when that was past the sandbox, else
/// a note that goes by.
pub fn say_ran(ed: &mut Editor, cmd: &str, ran: &Ran) {
    match ran.past {
        true => ed.set_warning(ran.said(cmd)),
        false => ed.set_status(ran.said(cmd)),
    }
}

/// A click on the mark of the chat's row of call `call`: where that command ran, and why.
pub fn said_ran(ed: &mut Editor, call: &str) {
    let Some(c) = &ed.session.chat else { return };
    let Some((what, ran)) = c
        .msgs
        .iter()
        .find(|m| m.call == call)
        .and_then(|m| Some((m.look.as_ref()?.what.clone(), m.ran.clone()?)))
    else {
        return;
    };
    say_ran(ed, &what, &ran);
}

/// The call a change of Claude's answers, found once as its request comes (the native Edit's permission request,
/// tarae's tool call — neither carries the call's id, both its input): the call of `tool` still waiting with that
/// very `input` that no change has claimed yet (`Chat.claimed`) — two Edits of one file, two renames side by side,
/// each its own, whatever order they land in (a rename on its server's answer). Equal or nothing: a request
/// not its call's input byte for byte (a native Edit's with a default filled in, an input a hook reworked — the
/// chat runs with hooks off, the native Edit only under `llm.edit-tools` both/native) claims none, so a mismatch
/// leaves no stamp, never a wrong one ("the same file's" would pick between two Edits of it by order).
pub fn claim_call(ed: &mut Editor, tool: &str, input: &serde_json::Value) -> Option<String> {
    let c = ed.session.chat.as_mut()?;
    let id = c
        .calls
        .iter()
        .find(|k| k.name == tool && k.input == *input && !c.claimed.contains(&k.id))?
        .id
        .clone();
    c.claimed.push(id.clone());
    Some(id)
}

/// `claim_call` for tarae's change tool's request `mcp_id` (as it comes — its change lands later, maybe after one
/// asked after it): kept by that id for its landing (`row_for`).
pub fn claim_for(ed: &mut Editor, tool: &str, input: &serde_json::Value, mcp_id: &serde_json::Value) {
    if let Some(row) = claim_call(ed, tool, input)
        && let Some(c) = ed.session.chat.as_mut()
    {
        c.rows.push((mcp_id.clone(), row));
    }
}

/// The row request `mcp_id` claimed (`claim_for`).
pub fn row_for(ed: &Editor, mcp_id: &serde_json::Value) -> Option<String> {
    let c = ed.session.chat.as_ref()?;
    c.rows.iter().find(|(m, _)| m == mcp_id).map(|(_, row)| row.clone())
}

/// A change of Claude's was answered by `keys` (its review's, its card's): the row of call `id` — the one it
/// claimed (`claim_call`) — is stamped with them.
pub fn stamp_call(ed: &mut Editor, id: &str, keys: &[crate::stamp::Stamp]) {
    let Some(c) = ed.session.chat.as_mut() else { return };
    let Some(m) = c.msgs.iter_mut().rev().find(|m| m.call == id) else { return };
    for k in keys {
        if !m.stamps.contains(k) {
            m.stamps.push(*k);
        }
    }
    // (`y` first, whichever came first)
    crate::sorting::by_key(&mut m.stamps, |s| s.0 != 'y');
    m.changed();
}

/// The command `cmd` Claude asked to run ended: its row (the newest `run` row of that command whose call is
/// still waiting, not marked yet) says where it ran (`ran`).
pub fn mark_ran(ed: &mut Editor, cmd: &str, ran: Ran) {
    let Some(c) = ed.session.chat.as_mut() else { return };
    let calls: Vec<String> = c.calls.iter().filter(|k| k.runs(cmd.trim())).map(|k| k.id.clone()).collect();
    if let Some(m) = c.msgs.iter_mut().rev().find(|m| m.ran.is_none() && calls.contains(&m.call)) {
        m.ran = Some(ran);
        m.changed();
    }
}

/// A tool call claude made (a `tool_use` block) — its id, tool and input.
#[derive(Clone, Debug)]
pub struct Called {
    pub id: String,
    pub name: String,
    pub input: serde_json::Value,
}

impl Called {
    /// Whether it's tarae's `run` of `cmd`.
    fn runs(&self, cmd: &str) -> bool {
        self.name == "mcp__tarae__run" && self.input["command"].as_str().map(str::trim) == Some(cmd)
    }
}

/// Claude's calls this turn held back behind its `run` of `cmd` (the card up for it) — what each does, as
/// its row says it (`read src/a.rs`); not a later run with a card of its own (it counts as a card).
pub fn behind_run(ed: &Editor, cmd: &str) -> Vec<String> {
    let Some(c) = ed.session.chat.as_ref().filter(|c| c.busy()) else { return Vec::new() };
    let carded: Vec<&str> = ed
        .session_cards()
        .filter_map(|o| match &o.what {
            crate::offer::What::Run(r) => Some(r.cmd.as_str()),
            _ => None,
        })
        .collect();
    let Some(at) = c.calls.iter().position(|k| k.runs(cmd)) else { return Vec::new() };
    c.calls[at + 1..]
        .iter()
        .filter(|k| !carded.iter().any(|cmd| k.runs(cmd)))
        .map(|k| {
            let l = look(&ed.session.root, &k.name, &k.input);
            format!("{} {}", label(l.verb), l.what).trim().to_string()
        })
        .collect()
}

impl Chat {
    /// Whether a call it made is about to ask the user: one waiting behind the call in hand (claude holds
    /// it back until that one is answered) is a command that won't run unasked (#5: `C-q` found no card
    /// while one was about to come). `allow`: `llm.run-allow`.
    pub fn asks_next(&self, allow: &[String]) -> bool {
        // (one this chat allowed again: by its words — where it runs is the run's to tell)
        let again = |cmd: &str| self.run_again.iter().any(|r| r.again.covers(cmd));
        self.calls.iter().skip(1).any(|k| {
            k.name == "mcp__tarae__run"
                && k.input["command"]
                    .as_str()
                    .is_some_and(|cmd| !crate::tools::runs_unasked(cmd.trim(), allow) && !again(cmd.trim()))
        })
    }
}

impl Chat {
    /// `q` waits for this turn's end with what's queued — after it, or (`first`: one that failed, sent again)
    /// before it.
    pub fn queue(&mut self, q: String, first: bool) {
        let n = self.queued.as_ref().map_or(0, |_| self.queued_n.max(1));
        if !first || n == 0 {
            self.queued_last = q.clone();
        }
        self.queued = Some(match (self.queued.take(), first) {
            (Some(was), true) => format!("{q}\n\n{was}"),
            (Some(was), false) => format!("{was}\n\n{q}"),
            (None, _) => q,
        });
        self.queued_n = n + 1;
    }

    /// What was queued went (sent, or taken back to edit).
    pub fn queued_gone(&mut self) {
        (self.queued_n, self.queued_last) = (0, String::new());
    }

    /// What waits for this turn's end, as its row and the state say it — (`2 messages queued` · `review notes
    /// queued`, the last message's text when there's one): frictions29 18. None: nothing waits.
    pub fn queued_said(&self) -> Option<(String, String)> {
        let n = self.queued.as_ref().map_or(0, |_| self.queued_n.max(1));
        let reviews = self.sends.iter().filter(|l| matches!(l, Later::Review(_))).count();
        let replies = self.sends.len() - reviews;
        let mut parts = Vec::new();
        if reviews > 0 {
            parts.push("review notes".to_string());
        }
        if replies > 0 {
            parts.push(if replies == 1 { "a reply".into() } else { format!("{replies} replies") });
        }
        if n > 0 {
            parts.push(if n == 1 { "a message".into() } else { format!("{n} messages") });
        }
        let said = match parts.as_slice() {
            [] => return None,
            [one] => format!("{one} queued"),
            [rest @ .., last] => format!("{} and {last} queued", rest.join(", ")),
        };
        let last = match (&self.queued, self.queued_last.is_empty()) {
            (Some(q), true) => q.clone(),
            (Some(_), false) => self.queued_last.clone(),
            (None, _) => String::new(),
        };
        Some((said, last))
    }

    /// Its conversation ends (a new one, another one continued): what its answers let through goes with it —
    /// `a` on a card (`allowed`), on a run's card (`run_again`, a socket's too, and the scripts they run), on the
    /// screen card (`sees`). The caller tells the proxy (`hosts::chat_ended`, first) and makes the `Caps` again.
    pub(crate) fn conversation_left(&mut self) {
        self.allowed = Default::default();
        self.run_again.clear();
        self.run_scripts.clear();
        self.sees = false;
        self.hosts_next.clear();
    }

    pub(crate) fn new() -> Self {
        Chat {
            msgs: Vec::new(),
            input: String::new(),
            cursor: 0,
            focused: true,
            state: State::Idle,
            started: Instant::now(),
            scroll: 0,
            proc: None,
            generation: 0,
            sent: HashMap::new(),
            stopping: false,
            hop: 0,
            follow: Default::default(),
            no_thought_flag: false,
            started_new: false,
            unfolded: false,
            edits: Default::default(),
            allowed: Default::default(),
            reads: Default::default(),
            read_once: Vec::new(),
            read_no: Vec::new(),
            scanned: Default::default(),
            kept_keys: None,
            kept_keys_asked: 0,
            after_kept: Vec::new(),
            key_pass: Vec::new(),
            keys_said: Vec::new(),
            ran: Vec::new(),
            sees: false,
            run_again: Vec::new(),
            also_refused: Vec::new(),
            also_asked: 0,
            also_yes: false,
            run_scripts: Vec::new(),
            queued: None,
            queued_n: 0,
            queued_last: String::new(),
            view_at: None,
            view_wanted: false,
            sends: Vec::new(),
            hosts_next: Vec::new(),
            screen: None,
            conversation: None,
            resume: None,
            past: Vec::new(),
            plan: None,
            models: Vec::new(),
            model: String::new(),
            calls: Vec::new(),
            claimed: Vec::new(),
            rows: Vec::new(),
            given: Vec::new(),
            cited: Vec::new(),
            pointed: Vec::new(),
            env_said: (None, Vec::new()),
            failed: (None, Vec::new()),
            reruns: Vec::new(),
            rules_said: (None, Default::default()),
            settings: Default::default(),
            billing: None,
            edit_tools: crate::edittool::Arm::Tarae,
            launched: None,
            runs_on: (None, String::new()),
            switched: None,
            money: None,
        }
    }

    /// Its claude is running.
    pub(crate) fn running(&self) -> bool {
        self.proc.is_some()
    }

    /// A line to its claude — whether it went (not running: no).
    pub(crate) fn to_claude(&self, line: String) -> bool {
        self.proc.as_ref().is_some_and(|p| p.stdin.send(line).is_ok())
    }

    /// A line of tarae's in the conversation — once, not again right under itself.
    pub(crate) fn note(&mut self, text: &str) {
        if self.msgs.last().is_none_or(|m| m.role != Role::Note || m.text != text) {
            self.msgs.push(Msg::new(Role::Note, text, ""));
        }
    }

    /// The process of now — a later answer goes only to the one that asked.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn busy(&self) -> bool {
        self.state != State::Idle
    }

    /// The call whose stop is in hand — its knot `●`, in the chat and the follow card alike: the
    /// stop replay shows, else while Claude works where it is (follow's, else its newest call); at rest none.
    pub fn in_hand(&self) -> Option<&str> {
        let live = || {
            let gaze = self.follow.gaze.as_ref().map(|g| g.call.as_str()).filter(|c| !c.is_empty());
            gaze.or_else(|| self.msgs.last().filter(|m| m.role == Role::Tool).map(|m| m.call.as_str()))
        };
        let call = match &self.follow.replaying {
            Some(g) => Some(g.call.as_str()),
            None if self.busy() => live(),
            None => None,
        };
        call.filter(|c| !c.is_empty())
    }

    /// The last turn's answer: its words since the last message sent, joined.
    pub fn last_answer(&self) -> String {
        let from = self.msgs.iter().rposition(|m| m.role == Role::User).map_or(0, |i| i + 1);
        let words: Vec<&str> =
            self.msgs[from..].iter().filter(|m| m.role == Role::Assistant).map(|m| m.text.as_str()).collect();
        words.join("\n\n")
    }

    /// Following while Claude works: the thought card beside the code carries its words, so the panel
    /// is minimized to a floating card (back in full with the answer, or on `space l`).
    pub fn compact(&self) -> bool {
        self.follow.on && self.busy() && !self.unfolded
    }

    /// Last code block of the last answer.
    pub fn last_code(&self) -> Option<String> {
        let msg = self.msgs.iter().rev().find(|m| m.role == Role::Assistant)?;
        let mut blocks = Vec::new();
        let mut cur: Option<String> = None;
        for line in msg.text.lines() {
            match (&mut cur, line.trim_start().starts_with("```")) {
                (None, true) => cur = Some(String::new()),
                (Some(_), true) => blocks.push(cur.take().unwrap_or_default()),
                (Some(b), false) => {
                    b.push_str(line);
                    b.push('\n');
                }
                (None, false) => {}
            }
        }
        blocks.pop()
    }

    /// Every place the conversation points at, oldest first (repeats in a row collapsed); relative ones
    /// from `root`.
    #[cfg(test)]
    pub fn links(&self, root: &Path) -> Vec<Link> {
        self.hops(root)
            .into_iter()
            .filter_map(|h| match h {
                Hop::Place(l, _) => Some(l),
                Hop::Element(_) => None,
            })
            .collect()
    }

    /// Everything `C-]` goes to, oldest first: the places (`targets`) and, in Claude's answers, the UI
    /// elements it cites by id (`card#12`, `git:src/a.rs`) in the order written.
    pub fn hops(&self, root: &Path) -> Vec<Hop> {
        let mut out: Vec<Hop> = Vec::new();
        for m in &self.msgs {
            let found: Vec<Hop> = match m.role {
                Role::Tool => m
                    .look
                    .iter()
                    .filter_map(|l| Some(Hop::Place(l.link.clone()?, Some(m.call.clone()))))
                    .collect(),
                Role::Assistant => answer_hops(root, &m.text),
                Role::User => m.attached.iter().map(|l| Hop::Place(l.clone(), None)).collect(),
                _ => Vec::new(),
            };
            for h in found {
                // (two edits of one file are two stops — repeats collapse only for plain places)
                let again = match (&h, out.last()) {
                    (Hop::Place(l, None), Some(Hop::Place(k, _))) => l == k,
                    (Hop::Element(a), Some(Hop::Element(b))) => a == b,
                    _ => false,
                };
                if !again {
                    out.push(h);
                }
            }
        }
        out
    }

    fn kill(&mut self) {
        if let Some(p) = self.proc.take() {
            llm::kill(p.pid);
        }
    }

    /// Its claude's process (`:procs`).
    pub fn pid(&self) -> Option<u32> {
        self.proc.as_ref().map(|p| p.pid)
    }

    /// `s` in `:procs`: its claude stops — as if it had exited (said in the chat, a turn under way ends);
    /// the next message starts one again.
    pub fn stop_process(&mut self) {
        if let Some(p) = &self.proc {
            llm::kill(p.pid);
        }
    }
}

/// `r` in `:procs`: the chat's claude started again, the conversation kept (`--resume`) — between turns
/// only (mid-turn, `s` stops it).
pub fn restart_process(ed: &mut Editor) {
    let Some(c) = ed.session.chat.as_mut() else { return };
    if c.state != State::Idle {
        return ed.set_status("claude is answering — s stops it, or wait for the end of its turn");
    }
    // (the same conversation, as said — a claude started without `--resume` would begin another)
    if let Some(id) = c.conversation.clone() {
        c.resume = Some(id);
    }
    c.kill();
    ensure_proc(ed);
    ed.set_success("claude restarted · the conversation goes on");
}

/// The session's billing changed (`:session-billing`, between turns): the next claude continues the same
/// conversation (`--resume`) on it — one running stops and starts again at once. A message held after a
/// refusal for money (`hold_queued`) goes now.
pub(crate) fn billing_changed(ed: &mut Editor) {
    let billing = ed.session.billing;
    let Some(c) = ed.session.chat.as_mut() else { return };
    if let Some(id) = c.conversation.clone() {
        c.resume = Some(id);
    }
    c.note(&format!("Billing: {} from here — the conversation goes on", billing.name()));
    if c.running() {
        c.kill();
        ensure_proc(ed);
    }
    send_queued(ed);
}

impl Drop for Chat {
    fn drop(&mut self) {
        self.kill();
    }
}

// ── Open·close ──────────────────────────────────────────────────────────────

/// `space l`: opens if absent (prewarming the process), moves focus to the input if already open.
pub fn open(ed: &mut Editor) {
    match &mut ed.session.chat {
        Some(c) => {
            c.focused = true;
            c.unfolded = true;
        }
        None => {
            let mut c = Chat::new();
            c.follow.on = ed.config.llm.follow;
            // The project's conversations, read on a worker — a recent one continues (the process spawned
            // meanwhile gives way to one resuming it, unless something was said already)
            if let Some(dir) = ed.chats_dir.clone() {
                let root = ed.session.root.clone();
                ed.events.jobs().spawn(move || {
                    let saved = crate::chats::load(&dir, &root);
                    move |ed: &mut Editor| loaded(ed, saved)
                });
            }
            ed.session.chat = Some(c);
            ed.caps_sync();
            crate::edits::at_end_new_chat(ed);
            crate::reads::kept_keys_load(ed);
        }
    }
    ensure_proc(ed);
}

/// The project's conversations are read: the latest continues if it's recent and nothing was said yet.
fn loaded(ed: &mut Editor, saved: Vec<crate::chats::Saved>) {
    let Some(c) = &mut ed.session.chat else { return };
    c.past = saved.iter().map(|s| (s.id.clone(), s.title.clone(), s.updated)).collect();
    let fresh = saved.first().filter(|s| crate::chats::now().saturating_sub(s.updated) < crate::chats::FRESH);
    match fresh {
        Some(s) if c.msgs.is_empty() && !c.busy() && !c.started_new => continue_with(ed, s.clone()),
        _ => {}
    }
}

/// Shows `saved` in the panel and resumes its conversation in claude (the process of now, if any, goes).
pub fn continue_with(ed: &mut Editor, saved: crate::chats::Saved) {
    continue_from(ed, saved, true);
}

/// `continue_with` — the process only when `spawn` (else the first message starts it, resuming).
fn continue_from(ed: &mut Editor, saved: crate::chats::Saved, spawn: bool) {
    let Some(c) = &ed.session.chat else { return };
    if c.busy() {
        return ed.set_status("Claude is still answering — stop it (C-c) first");
    }
    // (another conversation: the last one's answers go with it, as a new one's do — `reset`; a chat with none yet
    // — `:restart`'s, one just opened — leaves only answers it holds)
    let other = match c.conversation.as_deref() {
        Some(id) => id != saved.id,
        None => !c.allowed.is_empty() || !c.run_again.is_empty() || !c.run_scripts.is_empty() || c.sees,
    };
    if other {
        crate::hosts::chat_ended(ed);
    }
    // (a view mode — `C-p` picks from it — was of the conversation left: it goes)
    crate::view::forget(ed);
    let Some(c) = &mut ed.session.chat else { return };
    if other {
        c.conversation_left();
    }
    c.kill();
    c.msgs = crate::chats::msgs_of(&saved.rows);
    // (an earlier "continued" row says nothing now — one, at the end, says it)
    c.msgs.retain(|m| !(m.role == Role::Note && m.text.starts_with("continued from ")));
    let when = crate::chats::ago(saved.updated, crate::chats::now());
    c.msgs.push(Msg::new(Role::Note, format!("continued from {when}"), ""));
    c.plan = saved.plan.as_ref().and_then(crate::plan::Plan::from_json);
    c.conversation = Some(saved.id.clone());
    c.resume = Some(saved.id.clone());
    c.scroll = 0;
    c.view_at = None;
    c.follow.new_turn();
    // (its turns, cost so far and cache, as far as they were kept)
    match &saved.usage {
        Some(u) => ed.session.usage.restore(&saved.id, u),
        None => ed.session.usage.resumed(&saved.id),
    }
    crate::usage::watch(ed);
    if other {
        crate::reads::conversation_new(ed);
        ed.caps_sync();
    }
    if spawn {
        ensure_proc(ed);
    }
}

/// `C-p` · `:chat-resume`: the project's earlier conversations, newest first — Enter continues one.
pub fn past_picker(ed: &mut Editor) {
    use crate::picker::{Action, Item};
    let Some(dir) = ed.chats_dir.clone() else { return ed.set_status("conversations aren't kept here") };
    let root = ed.session.root.clone();
    let now_id = ed.session.chat.as_ref().and_then(|c| c.conversation.clone());
    let fill: crate::picker::Fill = Box::new(move || {
        let now = crate::chats::now();
        let store = crate::chats::file(&dir, &root);
        let items: Vec<Item> = crate::chats::load(&dir, &root)
            .into_iter()
            .filter(|s| Some(&s.id) != now_id.as_ref())
            .map(|s| Item {
                label: s.title.lines().next().unwrap_or_default().to_string(),
                action: Action::Conversation { id: s.id, store: store.clone() },
                hint: crate::chats::ago(s.updated, now),
                glyph: Some(("◦", "ui.virtual")),
            })
            .collect();
        if items.is_empty() {
            Err(crate::picker::FillError::Nothing("no earlier conversations in this project".into()))
        } else {
            Ok(items)
        }
    });
    ed.open_picker(
        crate::picker::Picker::of("conversations", "conversations", Vec::new(), false),
        Some(fill),
    );
}

/// `:chat-resume <id>` — continue that conversation (read on a worker).
pub fn resume(ed: &mut Editor, id: &str) {
    let Some(dir) = ed.chats_dir.clone() else { return };
    let root = ed.session.root.clone();
    let id = id.to_string();
    if ed.session.chat.is_none() {
        open(ed);
    }
    ed.events.jobs().spawn(move || {
        let saved = crate::chats::load(&dir, &root).into_iter().find(|s| s.id == id);
        move |ed: &mut Editor| match saved {
            Some(s) => {
                open(ed);
                continue_with(ed, s);
            }
            None => ed.set_error("that conversation isn't kept any more"),
        }
    });
}

/// What the chat panel was, for the tarae `:restart` runs (`restart.rs`).
pub struct Restored {
    pub conversation: Option<String>,
    pub input: String,
    pub cursor: usize,
    pub focused: bool,
    pub scroll: usize,
    pub follow: bool,
    /// Review at the end, on for this folder (`edits.rs`).
    pub at_end: Option<PathBuf>,
    /// … by your `y` (`Edits.at_end_yours`).
    pub at_end_yours: bool,
    /// A session behind's: no claude until its first message (which resumes the conversation).
    pub lazy: bool,
}

/// The note a conversation carried over a restart ends with (one — earlier ones go).
pub const RESTARTED: &str = "tarae restarted — same conversation";

/// After `:restart`: the panel as it was — its conversation (resumed in claude, not the latest one
/// guessed), the words being typed, where it was scrolled.
pub fn restore(ed: &mut Editor, r: Restored) {
    let mut c = Chat::new();
    c.follow.on = r.follow;
    c.cursor = if r.input.is_char_boundary(r.cursor) { r.cursor } else { r.input.len() };
    c.input = r.input;
    c.focused = r.focused;
    c.edits.at_end = r.at_end;
    c.edits.at_end_yours = r.at_end_yours;
    ed.session.chat = Some(c);
    let spawn = !r.lazy;
    let (Some(id), Some(dir)) = (r.conversation, ed.chats_dir.clone()) else {
        if spawn {
            ensure_proc(ed);
        }
        return;
    };
    let root = ed.session.root.clone();
    ed.events.jobs().spawn(move || {
        let saved = crate::chats::load(&dir, &root);
        move |ed: &mut Editor| {
            let Some(c) = &mut ed.session.chat else { return };
            c.past = saved.iter().map(|s| (s.id.clone(), s.title.clone(), s.updated)).collect();
            let Some(s) = saved.into_iter().find(|s| s.id == id) else {
                if spawn {
                    ensure_proc(ed);
                }
                return;
            };
            continue_from(ed, s, spawn);
            let Some(c) = &mut ed.session.chat else { return };
            c.scroll = r.scroll;
            c.msgs.retain(|m| !(m.role == Role::Note && m.text == RESTARTED));
            if let Some(m) = c.msgs.last_mut().filter(|m| m.role == Role::Note) {
                m.text = RESTARTED.into();
                m.changed();
            }
        }
    });
}

/// The conversation as it's kept: (store, project, it) — None before claude gave it an id.
fn kept(ed: &Editor) -> Option<(std::path::PathBuf, std::path::PathBuf, crate::chats::Saved)> {
    kept_of(ed.chats_dir.clone()?, &ed.session)
}

/// `kept` for session `l`'s chat, in front or behind.
fn kept_of(
    dir: std::path::PathBuf,
    l: &crate::session::Session,
) -> Option<(std::path::PathBuf, std::path::PathBuf, crate::chats::Saved)> {
    let c = l.chat.as_ref()?;
    let id = c.conversation.clone()?;
    let title = c.msgs.iter().find(|m| m.role == Role::User).map(|m| m.text.clone()).unwrap_or_default();
    let plan = c.plan.as_ref().map(crate::plan::Plan::to_json);
    let saved = crate::chats::Saved {
        usage: l.usage.conv_json(&id),
        id,
        title,
        updated: crate::chats::now(),
        rows: crate::chats::rows_of(&c.msgs),
        plan,
        seq: crate::chats::seq(),
    };
    Some((dir, l.root.clone(), saved))
}

/// The turn ended: the conversation is kept (on a worker) — its rows and claude's id for it.
fn keep(ed: &Editor) {
    let Some((dir, root, saved)) = kept(ed) else { return };
    ed.events.jobs().background(move || crate::chats::save(&dir, &root, saved));
}

/// Every session's kept now, on this thread — the last thing before `:restart` replaces this process (a
/// worker's save would go with it).
pub fn keep_now(ed: &Editor) {
    let Some(dir) = ed.chats_dir.clone() else { return };
    for l in ed.sessions_in_order() {
        if let Some((dir, root, saved)) = kept_of(dir.clone(), l) {
            crate::chats::save(&dir, &root, saved);
        }
    }
}

pub fn close(ed: &mut Editor) {
    // (the hosts its conversation's `a` let through go with it — one conversation a session at a time)
    crate::hosts::chat_ended(ed);
    // (a turn under way ends here, cut off: its process goes with the chat, and its exit is never heard)
    ed.session.usage.cut_off(crate::usage::now_ms());
    ed.calls_record();
    ed.session.chat = None;
    // (what its answers let through goes with it at once — `:granted` and the chat's count too)
    ed.caps_sync();
    crate::restart::turn_over(ed);
}

/// New chat — fresh process (drops memory of the previous conversation): `C-l` in the chat, `:chat-new`
/// (the chat opened first if it's closed — and the recent conversation not continued when it's read).
pub fn reset(ed: &mut Editor) {
    let said = "New conversation with Claude";
    // (the hosts this conversation's `a` let through go with it — the proxy keeps them as the session's, right
    // while a session has one conversation at a time)
    crate::hosts::chat_ended(ed);
    // (its view mode — `C-l` is one of its chords — was the conversation that goes: it goes with it)
    crate::view::forget(ed);
    let Some(c) = &mut ed.session.chat else {
        open(ed);
        if let Some(c) = &mut ed.session.chat {
            c.started_new = true;
        }
        return ed.set_status(said);
    };
    c.started_new = true;
    (c.focused, c.unfolded) = (true, true);
    c.kill();
    // The one just left is an earlier one now (the empty chat lists it)
    if let Some(id) = c.conversation.take() {
        let title = c.msgs.iter().find(|m| m.role == Role::User).map(|m| m.text.clone()).unwrap_or_default();
        c.past.retain(|p| p.0 != id);
        c.past.insert(0, (id, title, crate::chats::now()));
    }
    c.msgs.clear();
    c.plan = None;
    c.conversation_left();
    c.state = State::Idle;
    c.scroll = 0;
    c.follow.new_turn();
    c.view_at = None;
    c.conversation = None;
    c.resume = None;
    c.switched = None;
    // (a refusal for money read in a turn the new chat cut off is that turn's — `money_out` never sees it)
    c.money = None;
    // (a billing waiting for the last conversation's turn to end goes with it)
    ed.session.billing_next = None;
    // (a turn under way ends here, cut off: the killed process's exit comes to a generation no longer heard)
    ed.session.usage.cut_off(crate::usage::now_ms());
    ed.calls_record();
    ed.session.usage.new_conversation();
    // (what the last one's answers let through goes with it at once — `:granted` and the chat's count too)
    ed.caps_sync();
    // (review at the end stays as it's set — `llm.review-at-end`)
    crate::edits::at_end_new_chat(ed);
    // (a `y` for the last one's message, and a card pointing raised, go with it)
    crate::reads::conversation_new(ed);
    ensure_proc(ed);
    ed.set_status(said);
}

/// The project's Claude settings are read (`claudetrust.rs`): its claude starts — held back while their card
/// waits — and a message that waited for the read goes, and a stopped rebase's ask (`rebase::chat_ready`).
pub(crate) fn gate_opened(ed: &mut Editor) {
    ensure_proc(ed);
    send_queued(ed);
    crate::rebase::chat_ready(ed);
}

/// The project's Claude settings as answered now need another claude than the one running (`claudetrust.rs`):
/// said (`why`), and started again on the same conversation (`--resume`) — between turns at once. Mid-turn,
/// `stop`: it runs with more than they allow now, so it's stopped at once and starts again as its exit ends the
/// turn; else (`y`: it runs with less) it finishes the turn first. Either way `Gate.again` (`settings_again`).
pub(crate) fn settings_restart(ed: &mut Editor, why: &str, stop: bool) {
    let Some(c) = ed.session.chat.as_mut() else { return };
    if !c.busy() {
        c.note(why);
        c.settings.again = true;
        settings_again(ed);
        return;
    }
    c.settings.again = true;
    match stop {
        // (the process stays this chat's until its exit, so nothing starts another before the turn has ended)
        true => {
            c.note(why);
            c.stop_process();
        }
        false => c.note(&format!("{why} — once this turn is over")),
    }
}

/// `:claude-restart` — every session's chat claude started again on its conversation (`--resume`): an idle one
/// now, a busy one once its turn ends (`settings_restart`'s way) — so a new login reaches every chat without
/// `:restart`, which ends the terminal and what runs there too (frictions29 #7).
pub fn restart_all(ed: &mut Editor) {
    let running: Vec<crate::event::SessionId> = std::iter::once(&ed.session)
        .chain(&ed.sessions)
        .filter(|l| l.chat.as_ref().is_some_and(|c| c.proc.is_some()))
        .map(|l| l.id)
        .collect();
    if running.is_empty() {
        return ed.set_status("no chat's claude is running — each starts on the current login");
    }
    let mut busy = 0;
    for &id in &running {
        ed.with_session(id, |ed| {
            busy += usize::from(ed.session.chat.as_ref().is_some_and(Chat::busy));
            settings_restart(ed, "started again on its conversation (:claude-restart)", false);
        });
    }
    // (the idle ones started again now, the busy ones only at their turn's end — counted apart)
    let idle = running.len() - busy;
    let once = |b: usize| if b == 1 { "once its turn ends" } else { "once their turns end" };
    ed.set_status(match (idle, busy) {
        (n, 0) => format!("{} started again on the current login", crate::nav::count(n, "chat")),
        (0, 1) => format!("1 chat starts again on the current login {}", once(1)),
        (0, b) => format!("{b} chats start again on the current login {}", once(b)),
        (n, b) => {
            format!("{} started again on the current login · {b} {}", crate::nav::count(n, "chat"), once(b))
        }
    });
}

/// Its claude started again as things hold now (the key card's `y` — `envkey.rs`), said in the chat — the
/// conversation resumed when there is one — and `message`, the one that failed, sent again (its row goes: it's
/// sent anew, before a message queued since).
pub(crate) fn start_again(ed: &mut Editor, message: Option<String>, said: &str) {
    let Some(c) = ed.session.chat.as_mut() else { return };
    c.kill();
    if let Some(id) = c.conversation.clone() {
        c.resume = Some(id);
    }
    let failed = message.filter(|_| !c.busy()).and_then(|q| {
        let at = c.msgs.iter().rposition(|m| m.role == Role::User && m.text == q)?;
        Some(c.msgs.remove(at).text)
    });
    if let Some(q) = failed {
        c.queue(q, true);
    }
    c.note(said);
    ensure_proc(ed);
    send_queued(ed);
}

/// The turn is over — its end, or the exit of a claude stopped mid-turn: the restart that waited for it
/// (`Gate.again`), the conversation resumed. Whether one started.
fn settings_again(ed: &mut Editor) -> bool {
    let Some(c) = ed.session.chat.as_mut().filter(|c| c.settings.again) else { return false };
    c.settings.again = false;
    if let Some(id) = c.conversation.clone() {
        c.resume = Some(id);
    }
    c.kill();
    ensure_proc(ed);
    true
}

fn ensure_proc(ed: &mut Editor) {
    // (a key passed on until the session ends — the key card's `y`, `envkey.rs`)
    let cfg = llm::LlmConfig { keep_env_key: llm::keeps_key(ed), ..ed.config.llm.clone() };
    let tx = ed.events.sender();
    let Some(c) = &mut ed.session.chat else { return };
    if c.proc.is_some() {
        return;
    }
    // The project's Claude settings run commands as you under `-p` (no trust dialog there): read first, and
    // held back unless trusted — claude starts once they're answered (`claudetrust.rs`)
    let settings = match cfg.chat_settings {
        true => match crate::claudetrust::ready(ed, CHAT_SETTINGS) {
            Some(s) => Some(s),
            None => return,
        },
        false => None,
    };
    let Some(c) = &mut ed.session.chat else { return };
    // (the edit tools are this process's for its life: `tools/list` reads them here — `Chat.edit_tools`)
    let arm = cfg.edit_tools;
    c.edit_tools = arm;
    let mut args = llm::with_tools(&cfg.args, tools(arm), ALLOWED);
    // The conversation is kept by claude too, so it can be continued (`chats.rs`)
    args.retain(|a| a != "--no-session-persistence");
    // Only tarae's own MCP server, whatever `llm.args` says: under `-p` a project's `.mcp.json` servers connect
    // unasked
    if !args.iter().any(|a| a == "--strict-mcp-config") {
        args.push("--strict-mcp-config".into());
    }
    if let Some(id) = &c.resume {
        args.extend(["--resume".to_string(), id.clone()]);
    }
    if let Some((settings, sources)) = settings {
        // CLAUDE.md files and settings as `claude` loads them — but hooks don't run, and edits always come
        // here as a review (an `ask` rule and the default mode outrank any allow rule or accept-edits mode)
        args = llm::with_flag(&args, "--setting-sources", &sources);
        args = llm::with_flag(&args, "--settings", &settings);
        args = llm::with_flag(&args, "--permission-mode", "default");
    }
    let cfg = llm::LlmConfig { args, ..cfg };
    let system = system_for(arm);
    let mut extra = vec!["--append-system-prompt", system.as_str(), "--permission-prompt-tool", "stdio"];
    if !c.no_thought_flag {
        extra.extend(["--thinking-display", "summarized"]);
    }
    // Who pays (`billing.rs`): an API session's key helper goes into the `--settings` above, merged
    let started = ed.credential(ed.session.billing, crate::billing::Role::Chat).and_then(|cred| {
        let child = llm::spawn(&cfg, &extra, &ed.session.root, ed.claude_env.as_ref(), &cred);
        child.map(|child| (child, cred.billing())).map_err(|e| e.said(&cfg.command, true))
    });
    let Some(c) = &mut ed.session.chat else { return };
    let (mut child, billing) = match started {
        Ok(started) => started,
        Err(said) => {
            // (once: opening the chat tries, and so does sending)
            c.note(&said);
            return;
        }
    };
    c.billing = Some(billing);
    c.runs_on = (cfg.model.clone(), cfg.effort.clone());
    c.launched = Some((cfg, extra.iter().map(|s| s.to_string()).collect()));
    let Some(stdin) = child.stdin.take().map(llm::Lines::new) else { return };
    // tarae's MCP server (the note tool) — registered before anything else is said
    let _ = stdin.send(llm::initialize_message(&["tarae"]));
    // A fresh process has seen no file yet — and said no credential yet: the last one's may not be its
    c.sent.clear();
    (ed.session.usage.key_source, ed.session.usage.plan_event) = (None, false);
    // (nor its account: on a key or a cloud provider it says none, and the last one's would stand — #3)
    ed.session.usage.account = None;
    // (unique across chats too: a closed chat's process ending after a new chat's started isn't the new one's)
    static GENERATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    c.generation = GENERATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let generation = c.generation;
    c.proc = Some(Proc { stdin, pid: child.id() });
    llm::pump(child, false, tx, move |ev| {
        Some(Box::new(move |ed: &mut Editor| on_event(ed, generation, ev)))
    });
    // What claude's environment leaves out, once since tarae started (names only)
    if let Some(said) = llm::env_told(ed)
        && let Some(c) = &mut ed.session.chat
    {
        c.note(&said);
    }
    // Where the plan's limits stand, unless a claude said it lately (`limits.rs`)
    crate::limits::ask_chat(ed);
    // (a session's instruction files: the main checkout's changes since its copy — #27)
    crate::instructions::follow(ed);
}

/// `on_event` for other modules' tests: an event of chat process `generation`'s stream, as `llm::pump` sends it.
#[cfg(test)]
pub(crate) fn stream_event(ed: &mut Editor, generation: u64, ev: StreamEv) {
    on_event(ed, generation, ev)
}

fn on_event(ed: &mut Editor, generation: u64, ev: StreamEv) {
    // (a failure for want of a login, with a key tarae left out: said with it, and what passes it)
    let login_hint = match &ev {
        StreamEv::Done(Err(e)) | StreamEv::Exit(e) => llm::login_hint(ed, e),
        _ => None,
    };
    // (the credential claude says it runs on, against who pays for it — `billing.rs`)
    let keep_key = llm::keeps_key(ed);
    let wrong = |c: &Chat, said: Option<&str>| {
        c.billing.and_then(|b| crate::billing::checked(b, keep_key, crate::billing::Role::Chat, said))
    };
    let log = ed.session.log();
    let Some(c) = ed.session.chat.as_mut().filter(|c| c.generation == generation) else { return };
    match ev {
        StreamEv::Thinking => {
            if c.state == State::Idle {
                return;
            }
            c.state = State::Thinking;
        }
        StreamEv::Thought(t) => {
            if c.state == State::Idle {
                return;
            }
            c.state = State::Thinking;
            match c.msgs.last_mut() {
                Some(m) if m.role == Role::Thought => {
                    m.text.push_str(&t);
                    m.changed();
                }
                _ => c.msgs.push(Msg::new(Role::Thought, t, "")),
            }
            crate::follow::on_words(ed, false);
        }
        StreamEv::Tool { id, name, input } => {
            if c.state == State::Idle {
                return;
            }
            c.state = State::Thinking;
            c.calls.push(Called { id: id.clone(), name: name.clone(), input: input.clone() });
            if ed.config.features_count {
                ed.features.bump(crate::features::Area::Claude, crate::features::claude_tool(&name));
            }
            let now = crate::usage::now_ms();
            ed.session.usage.tally.called(&id, &name, &input, &ed.session.root, ed.session.wrote, now);
            let mut look = look(&ed.session.root, &name, &input);
            // A reply points at its note
            if let Some(n) =
                input["reply_to"].as_u64().and_then(|id| ed.session.notes.list.iter().find(|n| n.id == id))
            {
                look.link = Some(Link { path: n.path.clone(), line: n.lines.start });
            }
            // One plan row a turn — it shows the latest (the plan itself is pinned above)
            let turn = c.msgs.iter().rposition(|m| m.role == Role::User).map_or(0, |i| i + 1);
            let planned =
                c.msgs[turn..].iter_mut().find(|m| m.look.as_ref().is_some_and(|l| l.verb == "plan"));
            match planned {
                Some(m) if look.verb == "plan" => {
                    m.look = Some(look.clone());
                    m.changed();
                }
                _ => c.msgs.push(Msg::tool(look.clone(), &id)),
            }
            crate::follow::on_tool(ed, &id, &name, &input, &look);
        }
        // Kept from the first question on — a tarae closed mid-turn still has it
        StreamEv::Conversation { id, model, version, key_source } => {
            let new = c.conversation.as_deref() != Some(id.as_str());
            // (a process that didn't resume one began it: it has cost nothing before)
            match c.resume.is_none() {
                true => ed.session.usage.fresh(&id),
                false => ed.session.usage.resumed(&id),
            }
            if let Some(v) = &version {
                crate::claudeupdate::seen(ed, v);
            }
            let Some(c) = ed.session.chat.as_mut().filter(|c| c.generation == generation) else { return };
            ed.session.usage.cli = version;
            // Not the credential its billing gives: this turn stops, and the claude at its end — said
            if let Some(said) = wrong(c, key_source.as_deref()) {
                log.line("claude", "warn", &said);
                c.note(&said);
                c.stopping = true;
                c.to_claude(llm::interrupt_message());
            }
            // (the credential it reported, each turn — `limits.rs`)
            if key_source.is_some() {
                ed.session.usage.key_source = key_source;
            }
            c.conversation = Some(id);
            c.model = model;
            if new {
                keep(ed);
            }
        }
        StreamEv::Initialized { models, key_source, account } => {
            c.models = models;
            // Not the credential its billing gives, as it starts: it stops — said
            if let Some(said) = wrong(c, key_source.as_deref()) {
                log.line("claude", "warn", &said);
                c.note(&said);
                c.kill();
            }
            // (the credential it runs on, as it starts — the first turn's `init` says it again)
            if key_source.is_some() {
                ed.session.usage.key_source = key_source;
            }
            // (which account: its organization and plan — #3)
            if let Some(a) = account {
                crate::usage::account_seen(ed, a);
            }
        }
        // What it costs (`usage.rs`) — a request going out changes nothing on screen yet
        StreamEv::Requesting(at) => {
            ed.session.usage.requesting(at);
            ed.unchanged = true;
        }
        StreamEv::Usage(seen) => ed.session.usage.seen(seen),
        StreamEv::Spent(spent) => {
            ed.session.usage.spent(spent);
            ed.calls_record();
            crate::uistate::watch::usage(ed, "turn");
            crate::usage::view_sync(ed);
        }
        StreamEv::Answered(model) => c.model = model,
        StreamEv::Compaction(e) => crate::compaction::on_event(ed, e),
        StreamEv::Limits(report) => {
            // (its own claude's `rate_limit_event`: sent to plan users only — `limits::on_plan`)
            if report.readings.iter().any(|r| r.from == crate::limits::Source::Event) {
                ed.session.usage.plan_event = true;
            }
            // (refused: wait, or the limit card — `billing.rs`)
            let refused: Vec<(String, Option<u64>)> = report
                .readings
                .iter()
                .filter(|r| r.status == Some(crate::limits::Status::Rejected))
                .map(|r| (r.window.clone(), r.resets_at_ms))
                .collect();
            // (a claude still on another login than the last one seen says that account's limits, not these —
            // #3: they'd fill the new login's again; its own refusal below is still its chat's)
            let other_login =
                matches!((&ed.session.usage.account, ed.limits.login), (Some(a), Some(l)) if a.login != l);
            match other_login {
                true => log.line("limits", "info", "a reading from a claude on another login — not kept"),
                false => crate::limits::seen(ed, report),
            }
            for (window, resets_at_ms) in refused {
                ed.limit_hit(&window, resets_at_ms);
            }
        }
        // (refused for money: said, and the plan offered, at the turn's end — `billing.rs`)
        StreamEv::Money(m) => {
            if c.state != State::Idle {
                c.money = Some(m);
            }
            ed.unchanged = true;
        }
        // (a retry said in place of the one before it — the turn's, not a row each)
        StreamEv::Retry(r) => {
            let said = r.said();
            let retry = |m: &&mut Msg| m.role == Role::Note && m.text.starts_with(crate::limits::RETRY_SAID);
            match c.msgs.last_mut().filter(retry) {
                Some(m) => {
                    m.text = said;
                    m.changed();
                }
                None => c.msgs.push(Msg::new(Role::Note, said, "")),
            }
        }
        StreamEv::ToolResult { id, text, error } => {
            c.calls.retain(|k| k.id != id);
            ed.session.usage.tally.answered(&id, error, crate::usage::now_ms());
            // What it did on disk (an edit, a write, a command): the git tab reads the status again (#29 — it
            // said "the tree is clean" over files Claude had changed, until reopened)
            let wrote = c
                .msgs
                .iter()
                .rev()
                .find(|m| m.call == id)
                .and_then(|m| m.look.as_ref())
                .is_some_and(|l| matches!(l.verb, "edit" | "write" | "run" | "test"));
            // A test run's row ends with its verdict, a command's with how it ended
            if let Some(m) = c.msgs.iter_mut().rev().find(|m| m.call == id)
                && let Some(l) = m.look.as_mut().filter(|l| matches!(l.verb, "test" | "run"))
            {
                l.range = if l.verb == "test" {
                    crate::tools::verdict(&text)
                } else {
                    crate::tools::run_verdict(&text)
                };
                m.changed();
            }
            // A lookup's row ends with what it found (`12 references in 3 files`) — Grep's count is follow's
            // (`follow::found`, from its hits)
            if let Some(m) = c.msgs.iter_mut().rev().find(|m| m.call == id)
                && let Some(l) = m.look.as_mut().filter(|l| {
                    matches!(
                        l.verb,
                        "references"
                            | "search"
                            | "symbols"
                            | "definition"
                            | "diagnostics"
                            | "rename"
                            | "replace"
                    )
                })
                && let Some(v) = crate::nav::verdict(&text)
            {
                l.range = v;
                m.changed();
            }
            if c.state != State::Idle {
                crate::follow::on_result(ed, &id, &text, error);
            }
            if wrote {
                ed.git_tab_follow();
            }
        }
        StreamEv::Control { request_id, request } => {
            // Answered now, or (an edit) once its review ends
            if let Some(response) = control(ed, generation, &request_id, &request)
                && ed.session.chat.as_ref().is_some_and(|c| c.generation == generation)
            {
                respond(ed, &request_id, response);
            }
        }
        StreamEv::Text(t) => {
            if c.state == State::Idle {
                return;
            }
            c.state = State::Writing;
            // (an id cited marks its element as it's written — the whole answer is read again at its end)
            let ids = t.contains(['#', ':']);
            match c.msgs.last_mut() {
                Some(m) if m.role == Role::Assistant => {
                    m.text.push_str(&t);
                    m.changed();
                }
                _ => c.msgs.push(Msg::new(Role::Assistant, t, "")),
            }
            if ids {
                crate::uistate::claude::cite(c);
            }
            crate::follow::on_words(ed, false);
        }
        StreamEv::Done(r) => {
            // (no turn in flight: claude ending before one — a conversation it can't resume)
            if c.state == State::Idle {
                return;
            }
            let secs = c.started.elapsed().as_secs_f32();
            let (calls, took) = (c.calls.len(), c.started.elapsed());
            // (the user stopped it: what its runs left going is offered to stop — `tools::left_running`)
            let stopped = c.stopping;
            c.state = State::Idle;
            c.calls.clear();
            c.claimed.clear();
            c.rows.clear();
            // (refused for money on the key, the turn ending on its error — not stopped, not answered after
            // all: said in tarae's words instead — `money_out`)
            let money = c
                .money
                .take()
                .filter(|_| ed.session.billing == crate::billing::Billing::Api && r.is_err() && !c.stopping);
            let how = match &r {
                Ok(_) => Ok(()),
                Err(_) if c.stopping => Err("stopped".to_string()),
                Err(e) => Err(e.clone()),
            };
            crate::uistate::watch::turn(ed, "ended", calls, Some(took));
            let ok = r.is_ok();
            // (refused by the plan, in Claude Code's own words — `limits::of_words`)
            let refused = r.as_ref().err().and_then(|e| crate::limits::of_words(e));
            // A session behind: its tab says `done` until it's looked at (#86), and a toast says it once
            // (`with_session`). In front with the chat out of sight past the turn: said here
            if ed.background() {
                ed.session.turn_end = Some((Instant::now(), how));
            } else if out_of_sight(ed) {
                turn_end_said(ed, how);
            }
            let Some(c) = ed.session.chat.as_mut() else { return };
            match r {
                // Answers with no streamed pieces (short answers, CLIs without partials) get the final text
                Ok(text) => {
                    if !text.is_empty() && !c.msgs.last().is_some_and(|m| m.role == Role::Assistant) {
                        c.msgs.push(Msg::new(Role::Assistant, text, ""));
                    }
                    crate::uistate::claude::cite(c);
                    c.msgs.push(Msg::new(Role::Note, format!("{secs:.1}s"), ""));
                }
                Err(_) if money.is_some() => {}
                Err(e) => {
                    let e = if std::mem::take(&mut c.stopping) { "stopped".to_string() } else { e };
                    c.msgs.push(Msg::new(Role::Note, e, ""));
                    if let Some(hint) = &login_hint {
                        c.note(hint);
                    }
                }
            }
            // (a key held back: the card asks whether Claude takes it)
            if login_hint.is_some() {
                crate::envkey::ask(ed);
            }
            crate::follow::on_done(ed);
            crate::edits::turn_over(ed);
            let left = crate::tools::turn_over(ed);
            if stopped {
                crate::tools::left_running(ed, left);
            }
            crate::reads::turn_over(ed);
            checkpoint_turn(ed);
            // (its result came just before — `Spent`: the cache's TTL runs from its last request)
            crate::usage::watch(ed);
            keep(ed);
            let answer = ok.then(|| {
                ed.session.chat.as_ref().and_then(|c| {
                    let turn = c.msgs.iter().rposition(|m| m.role == Role::User).map_or(0, |i| i + 1);
                    c.msgs[turn..].iter().rev().find(|m| m.role == Role::Assistant).map(|m| m.text.clone())
                })
            });
            crate::notes::turn_done(ed, answer.flatten());
            crate::restart::turn_over(ed);
            crate::rebase::turn_over(ed);
            // (whatever the turn changed on disk, the git tab shows it — #29)
            ed.git_tab_follow();
            // A claude that said another credential than its billing's goes with its turn: the next message
            // starts one that's checked again
            if let Some(c) = ed.session.chat.as_mut()
                && wrong(c, ed.session.usage.key_source.as_deref()).is_some()
            {
                c.kill();
                // (its interrupt may have come after the turn's end: the next turn isn't "stopped")
                c.stopping = false;
            }
            // The plan refused it: wait, or the limit card (`billing.rs`) — its reset as the limits know it
            if let Some(window) = refused {
                let resets = ed.limits.get(window, crate::usage::now_ms()).and_then(|r| r.resets_at_ms);
                ed.limit_hit(window, resets);
            }
            let broke = money.is_some();
            if let Some(m) = money {
                ed.money_out(m);
            }
            // (the project's Claude settings trusted mid-turn: claude takes them now — `settings_restart`)
            settings_again(ed);
            // (a billing that waited for the turn's end — the limit card's `y` mid-turn)
            crate::billing::turn_over(ed);
            // (the key can't pay: a message queued behind the refused turn would only be refused too — #211)
            match broke {
                true => hold_queued(ed),
                false => send_queued(ed),
            }
        }
        StreamEv::Exit(err) => {
            c.proc = None;
            c.follow.gaze = None;
            c.calls.clear();
            c.claimed.clear();
            c.rows.clear();
            // (a refusal for money read in the turn this exit cut off is that turn's alone)
            c.money = None;
            // The conversation is gone from claude (its own cleanup took it, or you deleted it): a fresh one, the
            // rows still shown, said once — and a message sent to the process that couldn't resume goes again, to
            // the new one
            if err.contains("No conversation found") && c.resume.is_some() {
                c.resume = None;
                c.conversation = None;
                let unanswered = match std::mem::replace(&mut c.state, State::Idle) {
                    State::Idle => None,
                    _ => c.msgs.iter().rposition(|m| m.role == Role::User),
                };
                if let Some(at) = unanswered {
                    let q = c.msgs.remove(at).text;
                    c.queue(q, true);
                }
                c.msgs.push(Msg::new(
                    Role::Note,
                    "Claude no longer has this conversation — a new one starts",
                    "",
                ));
                ensure_proc(ed);
                return send_queued(ed);
            }
            // An older claude without the flag: quietly again without it
            if err.contains("thinking-display") && !c.no_thought_flag {
                c.no_thought_flag = true;
                if c.state != State::Idle {
                    c.state = State::Idle;
                    c.msgs.push(Msg::new(Role::Note, "restarted claude — send again", ""));
                }
                return ensure_proc(ed);
            }
            if c.state != State::Idle {
                c.state = State::Idle;
                for id in std::mem::take(&mut ed.session.notes.turn) {
                    if let Some(n) = ed.session.notes.list.iter_mut().find(|n| n.id == id) {
                        n.waiting = false;
                    }
                }
                let e = if err.is_empty() {
                    "claude exited".to_string()
                } else {
                    format!("claude exited: {err}")
                };
                // (stopped for the project's settings: said already, and it starts again below)
                if !c.settings.again {
                    c.msgs.push(Msg::new(Role::Note, e, ""));
                }
            }
            if let Some(hint) = &login_hint {
                c.note(hint);
            }
            // (a turn cut off: what its requests reported, no cost)
            ed.session.usage.cut_off(crate::usage::now_ms());
            ed.calls_record();
            crate::usage::watch(ed);
            if login_hint.is_some() {
                crate::envkey::ask(ed);
            }
            // A review (or a command's card) claude was waiting on has no one to answer now
            crate::edits::turn_over(ed);
            crate::tools::turn_over(ed);
            crate::reads::turn_over(ed);
            crate::restart::turn_over(ed);
            // (a billing that waited for the turn's end — the turn ended here)
            crate::billing::turn_over(ed);
            // (stopped mid-turn for the project's settings: started again as they hold now — a message typed
            // meanwhile goes to it)
            if settings_again(ed) {
                send_queued(ed);
            }
        }
    }
}

/// The front session's chat stays out of sight once its turn is over: the last frame drew none, and not only
/// because follow mode minimized it while Claude worked (that comes back with the answer) — something else
/// fills the screen (`C-w z`), or a review is read wide (`space v z`).
pub(crate) fn out_of_sight(ed: &Editor) -> bool {
    let zoomed = ed.zoomed().is_some_and(|z| z != crate::sizes::Zoom::Chat);
    ed.last_frame.is_some() && ed.screen.chat_x.is_none() && (zoomed || ed.review_wide())
}

/// The front session's turn ended with its chat out of sight: said, and where it's seen.
fn turn_end_said(ed: &mut Editor, how: Result<(), String>) {
    let (kind, words) = crate::session::turn_end_words(&how);
    let msg = format!("Claude's {words} — space l shows it");
    match kind {
        crate::editor::ToastKind::Warning => ed.set_warning(msg),
        _ => ed.set_status(msg),
    }
}

// ── Send ───────────────────────────────────────────────────────────────────

/// `C-s` in the chat · `:chat-screen` — the screen as the user sees it now (the last frame, as text) goes
/// with their next message, a pane showing a file named like a key Claude may not read blanked
/// (`reads::key_panes`); the chat opens if it isn't, and the line over the input says so.
pub fn attach_screen(ed: &mut Editor) -> Result<(), String> {
    let (frame, cols, rows) = ed.last_frame.as_ref().ok_or("nothing is on screen yet")?;
    let (cols, rows) = (*cols, *rows);
    let hidden = crate::reads::key_panes(ed);
    let text =
        crate::screen::text_hiding(frame, cols, rows, &hidden).ok_or("this screen can't be read as text")?;
    if ed.session.chat.is_none() {
        open(ed);
    }
    let c = ed.session.chat.as_mut().ok_or("no chat")?;
    c.focused = true;
    c.screen = Some(Shot { text, cols, rows });
    Ok(())
}

/// Where a sent screen's copy goes (the temp folder — one per message).
fn screen_copy_path() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("tarae-screen-{}-{n}.txt", std::process::id()))
}

/// The turn ended: the message queued during it goes now (#23) — unless a restart waits for the turn (it
/// stays queued, over the input, for after).
pub(crate) fn send_queued(ed: &mut Editor) {
    if ed.restart.is_some() {
        return;
    }
    // (it waits for the user's answer on a card their pointing raised — `reads::pointed_answer` sends it)
    if crate::reads::pointing_asks(ed) {
        return;
    }
    // (what waits besides a message goes first, one a turn — frictions29 18; one that sends nothing now, its
    // drafts gone meanwhile, lets the next go)
    while let Some(later) =
        ed.session.chat.as_mut().filter(|c| !c.sends.is_empty()).map(|c| c.sends.remove(0))
    {
        match later {
            Later::Review(lines) => crate::codereview::send_as(ed, lines),
            Later::Reply { id, text, fresh, why } => crate::notes::send(ed, id, &text, fresh, why, None),
        }
        if ed.session.chat.as_ref().is_some_and(Chat::busy) {
            return;
        }
    }
    let Some(q) = ed.session.chat.as_mut().and_then(|c| c.queued.take()) else { return };
    match submit_input(ed, &q) {
        true => {
            if let Some(c) = ed.session.chat.as_mut() {
                c.queued_gone();
            }
        }
        false => {
            if let Some(c) = ed.session.chat.as_mut() {
                c.queued = Some(q);
            }
        }
    }
}

/// What waits for a turn's end besides a message (`Chat.sends`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Later {
    /// The review's notes (`codereview::send_as`'s `lines`).
    Review(Option<bool>),
    /// A reply on note `id` (`notes::send`'s).
    Reply { id: u64, text: String, fresh: bool, why: Option<String> },
}

/// The turn ended refused for money: the message queued during it stays queued, said — it goes once the
/// session's billing changes (`billing_changed`); `↑` takes it back.
fn hold_queued(ed: &mut Editor) {
    let Some(c) = ed.session.chat.as_mut().filter(|c| c.queued.is_some()) else { return };
    c.note("Your queued message waits — it goes once this session's billing changes (↑ takes it back)");
}

/// Enter in the chat: the input goes as the next turn — or waits its turn while Claude works, or for the
/// user's answer on a card their pointing raised (`reads::point`), said, the input kept.
pub fn send(ed: &mut Editor) {
    let Some(c) = &mut ed.session.chat else { return };
    let question = c.input.trim().to_string();
    // Claude is working: it waits its turn (#23) — said over the input, sent when this turn ends; a second
    // one joins it. So it does while the project's settings are read (`claudetrust.rs` — their card doesn't
    // hold it: claude runs held back meanwhile): sent once it starts
    if (c.busy() || c.settings.waiting()) && !question.is_empty() {
        c.queue(question, false);
        c.input.clear();
        c.cursor = 0;
        return;
    }
    if !question.is_empty() && crate::reads::pointing_asks(ed) {
        return ed.set_status("not sent yet — answer the card about what you pointed at first (C-q)");
    }
    if !question.is_empty() && submit_input(ed, &question) {
        let c = ed.session.chat.as_mut().expect("just sent");
        c.input.clear();
        c.cursor = 0;
    }
}

/// Sends `question` as the next turn — with the editor context, plus `about` (a note's lines and thread;
/// `chip` then replaces the context summary). False if it couldn't go (busy, no process). Not the chat
/// input's: what was pointed at stays for the input's own message (`submit_input`), and a `y` given for that
/// one isn't this one's.
pub fn submit(ed: &mut Editor, question: &str, about: Option<String>, chip: Option<String>) -> bool {
    submit_as(ed, question, about, chip, false)
}

/// The chat input's own message: with what the user pointed at (its chips), and the files named like keys a
/// `y` let go with it (`reads::input_pass`).
fn submit_input(ed: &mut Editor, question: &str) -> bool {
    submit_as(ed, question, None, None, true)
}

fn submit_as(
    ed: &mut Editor,
    question: &str,
    about: Option<String>,
    chip: Option<String>,
    input: bool,
) -> bool {
    if ed.session.chat.as_ref().is_none_or(Chat::busy) {
        return false;
    }
    // Process first — a newly spawned one clears `sent`, so the context carries the whole file again
    ensure_proc(ed);
    let pass = if input { crate::reads::input_pass(ed) } else { Vec::new() };
    // What's up on screen by id (the `ui:` line, when there's something), and what the user pointed at
    let ui = ed.ui_state_claude().context_line();
    let (context, ctx_chip, sent, withheld) = context(ed, ui.as_ref().map(|u| u.0.as_str()), &pass);
    let pointed: Vec<String> =
        ed.session.chat.as_ref().filter(|_| input).map(|c| c.pointed.clone()).unwrap_or_default();
    let context = match pointed.is_empty() {
        true => context,
        false => {
            // (one carrying a file named like a key the user said no to: its name only)
            let block = |id: &String| match crate::reads::pointed_key(ed, id, &pass) {
                Some(k) => {
                    let name = k.strip_prefix(&ed.session.root).unwrap_or(&k).display().to_string();
                    format!("<pointed id=\"{id}\">{id} — {name}, {}</pointed>", crate::reads::NOT_SENT)
                }
                None => crate::uistate::claude::pointed(ed, id),
            };
            let blocks: Vec<String> = pointed.iter().map(block).collect();
            format!("{context}\n{}", blocks.join("\n"))
        }
    };
    let session = ed.session.log();
    let Some(c) = &mut ed.session.chat else { return false };
    let Some(p) = &mut c.proc else { return false };
    // What happened to its edits since (a turn undone): Claude's memory still has them
    let told = std::mem::take(&mut ed.session.checkpoints.tell);
    let context = match told.is_empty() {
        true => context,
        false => format!("{context}\n<editor-note>\n{}\n</editor-note>", told.join("\n")),
    };
    // The screen the user attached (`C-s`): as they saw it, characters only
    let shot = c.screen.take();
    let context = match &shot {
        Some(s) => format!(
            "{context}\n<screen cols=\"{}\" rows=\"{}\" note=\"the user's editor as they saw it, characters only \
             (no colors)\">\n{}\n</screen>",
            s.cols, s.rows, s.text
        ),
        None => context,
    };
    let content = match about {
        Some(a) => format!("{context}\n{a}\n\n{question}"),
        None => format!("{context}\n\n{question}"),
    };
    // A write that fails: the process stopped reading — it ended, and its exit, on the way (`llm::pump`), says
    // why. Until then the message is the turn's, as one written a moment before that exit is: for a conversation
    // claude no longer has it goes again to the new one; any other end is said once, as an exit mid-turn is
    if p.stdin.send(llm::user_message(&content)).is_err() {
        // (said with the next message that goes; ended, if it only closed its stdin — so its exit comes)
        ed.session.checkpoints.tell = told;
        llm::kill(p.pid);
    } else {
        crate::usage::message_sent(&session, content.len(), context.len());
    }
    c.sent.extend(sent);
    c.hop = 0;
    // (a new turn: the last one's failed runs are no longer what Claude ended on)
    c.failed.1.clear();
    c.follow.new_turn();
    c.unfolded = false;
    // (a switch's cost is this message's now — the line over the input goes)
    c.switched = None;
    // A new turn: what went to Claude is what this message carries (marks of the last one go) — the input's
    // chips, and a `y` for them, once its own message went
    if input {
        c.pointed.clear();
        c.key_pass.clear();
    }
    c.cited.clear();
    c.given = ui.map(|u| u.1).unwrap_or_default();
    for id in &pointed {
        if !c.given.contains(id) {
            c.given.push(id.clone());
        }
    }
    let mut chip = chip.unwrap_or(ctx_chip);
    for id in &pointed {
        chip.push_str(&format!(" · {id}"));
    }
    let mut msg = Msg::new(Role::User, question, String::new());
    if let Some(s) = shot {
        // A copy of what went, to open from its row (written on a worker)
        chip.push_str(&format!(" · screen {}×{}", s.cols, s.rows));
        let path = screen_copy_path();
        msg.attached = Some(Link { path: path.clone(), line: 0 });
        ed.events.jobs().spawn(move || {
            let r = std::fs::write(&path, format!("{}\n", s.text));
            move |ed: &mut Editor| {
                if let Err(e) = r {
                    ed.set_error(format!("couldn't keep a copy of the screen: {e}"));
                }
            }
        });
    }
    msg.chip = chip;
    let c = ed.session.chat.as_mut().expect("sent above");
    c.msgs.push(msg);
    c.scroll = 0;
    c.state = State::Thinking;
    c.started = Instant::now();
    crate::reads::not_sent(ed, &withheld);
    crate::uistate::watch::turn(ed, "started", 0, None);
    true
}

/// The turn ended: what Claude changed in it becomes one step `C-z` takes back (a row says so).
fn checkpoint_turn(ed: &mut Editor) {
    let question = ed
        .session
        .chat
        .as_ref()
        .and_then(|c| c.msgs.iter().rev().find(|m| m.role == Role::User))
        .map(|m| m.text.clone())
        .unwrap_or_default();
    // Files written unseen (review at the end): read first — `turn_gathered` goes on from there
    if crate::checkpoint::gather(ed, &question) {
        return;
    }
    if let Some(n) = crate::checkpoint::turn_over(ed, &question)
        && let Some(c) = &mut ed.session.chat
    {
        c.msgs.push(Msg::new(Role::Note, format!("changed {n} · space u undo"), ""));
    }
}

/// A turn that wrote files unseen is a checkpoint now (`checkpoint::gather`; `n` = its files, None: nothing
/// changed in the end): its review opens — unless another review is up, or a restart waits for the turn
/// (then the row says how to get to it).
pub fn turn_gathered(ed: &mut Editor, n: Option<String>) {
    if let Some(n) = n {
        let open = ed.session.code_review.is_none() && ed.session.review.is_none() && ed.restart.is_none();
        // (short: a row this narrow chat cuts at its end)
        let row = match open {
            true => format!("changed {n} · reviewing them now"),
            false => format!("changed {n} · space v t reviews them"),
        };
        if let Some(c) = &mut ed.session.chat {
            c.msgs.push(Msg::new(Role::Note, row, ""));
            keep(ed);
        }
        if open {
            // (the keys go to the review, as with an edit's)
            if let Some(c) = &mut ed.session.chat {
                c.focused = false;
            }
            crate::codereview::start_turn(ed, ed.session.checkpoints.list.len() - 1);
        }
    }
    crate::restart::turn_over(ed);
}

/// A control request of ours to the chat's process (a model or effort chosen) — its answer isn't needed.
pub fn request(ed: &mut Editor, request: serde_json::Value) {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    if let Some(p) = ed.session.chat.as_mut().and_then(|c| c.proc.as_mut()) {
        let id = format!("tarae-{}", N.fetch_add(1, std::sync::atomic::Ordering::Relaxed));
        let line = serde_json::json!({ "type": "control_request", "request_id": id, "request": request });
        let _ = p.stdin.send(line.to_string());
    }
}

/// Our answer to one of claude's control requests (the chat process of now).
pub fn respond(ed: &mut Editor, request_id: &str, response: serde_json::Value) {
    // (a permission's answer: a no makes the call's failed result a declined one — `calls.rs`)
    ed.session.usage.tally.answer(request_id, response["behavior"] == "deny");
    if let Some(p) = ed.session.chat.as_mut().and_then(|c| c.proc.as_mut()) {
        let _ = p.stdin.send(llm::control_response(request_id, response));
    }
}

/// A control request from claude → our answer (None = later). `mcp_message` = tarae's MCP server (its
/// tools, `tools.rs`); `can_use_tool` = an edit to review (`edits.rs`).
fn control(
    ed: &mut Editor,
    generation: u64,
    request_id: &str,
    req: &serde_json::Value,
) -> Option<serde_json::Value> {
    use serde_json::json;
    if req["subtype"] == "can_use_tool" {
        ed.session.usage.tally.asked(request_id, req["tool_name"].as_str().unwrap_or_default());
        return crate::edits::request(ed, request_id, req);
    }
    if req["subtype"] != "mcp_message" || req["server_name"] != "tarae" {
        return Some(json!({}));
    }
    let response = crate::tools::mcp(ed, generation, request_id, &req["message"])?;
    Some(json!({ "mcp_response": response }))
}

/// Tests: the stream says the turn ended — `stopped` by the user (`C-c`), or answered.
#[cfg(test)]
pub(crate) fn turn_ended(ed: &mut Editor, generation: u64, stopped: bool) {
    if let Some(c) = ed.session.chat.as_mut() {
        c.stopping = stopped;
    }
    let r = if stopped { Err("stopped".to_string()) } else { Ok(String::new()) };
    on_event(ed, generation, StreamEv::Done(r));
}

pub fn stop(ed: &mut Editor) {
    let Some(c) = &mut ed.session.chat else { return };
    if !c.busy() {
        return;
    }
    if let Some(p) = &mut c.proc {
        c.stopping = true;
        let _ = p.stdin.send(llm::interrupt_message());
    }
}

/// Context to send + summary chip + the document versions whose full text went along + the files named like
/// keys whose text it left out (`reads::key_shut`: their names go, "not sent"). `ui`: the line naming what's
/// up on screen by id (`UiState::context_line`); `pass`: those this message may carry all the same (a `y` for
/// the chat input's message — `reads::input_pass`).
fn context(
    ed: &Editor,
    ui: Option<&str>,
    pass: &[PathBuf],
) -> (String, String, Vec<(DocId, u64)>, Vec<PathBuf>) {
    let doc = ed.doc();
    let text = &doc.text;
    let name = doc.display_name(&ed.session.root);
    let lang = doc.syntax.as_ref().map(|s| s.lang.name.clone()).unwrap_or_default();
    let sel = doc.selection().primary();
    let head = sel.cursor(text);
    let line = text.byte_to_line(head.min(text.len_bytes()));
    let shut = |d: &crate::document::Document| {
        d.path.as_deref().filter(|p| crate::reads::key_shut(ed, p, pass)).map(Path::to_path_buf)
    };
    let mut withheld: Vec<PathBuf> = shut(doc).into_iter().collect();
    let mut ctx = format!(
        "<editor-context>\nfile: {name}{}{}{}\ncursor: line {}\n",
        if lang.is_empty() { String::new() } else { format!(" ({lang})") },
        if doc.is_modified() && doc.path.is_some() { " (unsaved)" } else { "" },
        if withheld.is_empty() { String::new() } else { format!(" — {}", crate::reads::NOT_SENT) },
        line + 1
    );
    // What the user is looking at: this file's lines on screen, and any other pane beside it
    let screen = on_screen(ed);
    let here = screen.iter().find(|s| s.0 == doc.id).map(|s| s.1.clone());
    if let Some(r) = &here {
        ctx.push_str(&format!("on screen: lines {}-{}\n", r.start + 1, r.end));
    }
    for (id, r) in screen.iter().filter(|s| s.0 != doc.id) {
        if let Some(d) = ed.docs.iter().find(|d| d.id == *id) {
            ctx.push_str(&format!(
                "also on screen: {} lines {}-{}\n",
                d.display_name(&ed.session.root),
                r.start + 1,
                r.end
            ));
        }
    }
    if let Some(ui) = ui {
        ctx.push_str(ui);
        ctx.push('\n');
    }
    let mut chip = doc.display_name_short();
    let mut sent = Vec::new();
    let was_sent = |d: &crate::document::Document| {
        ed.session.chat.as_ref().and_then(|c| c.sent.get(&d.id)) == Some(&d.version())
    };
    // A file named like a key, not open to Claude: its name only — no selection, text or diagnostics (a
    // diagnostic's message can quote a line)
    let its_text = withheld.is_empty();
    if !its_text {
        chip.push_str(" · not sent");
    }
    // Selection (only when more than one character is selected)
    if its_text && sel.to() > crate::graphemes::next_boundary(text, sel.from()) {
        let (l0, l1) =
            (text.byte_to_line(sel.from()), text.byte_to_line(sel.to().saturating_sub(1).max(sel.from())));
        ctx.push_str(&format!(
            "<selection lines=\"{}-{}\">\n{}\n</selection>\n",
            l0 + 1,
            l1 + 1,
            text.byte_slice(sel.from()..sel.to())
        ));
        chip.push_str(&if l0 == l1 {
            format!(" · L{}", l0 + 1)
        } else {
            format!(" · L{}–{}", l0 + 1, l1 + 1)
        });
    }
    // File: if this version hasn't been sent yet, the full text (if small) or around the cursor
    if its_text && !was_sent(doc) {
        if text.len_bytes() <= WHOLE_FILE_MAX {
            ctx.push_str(&format!("<file-content>\n{text}</file-content>\n"));
            sent.push((doc.id, doc.version()));
        } else {
            // Around the cursor, and what's on screen with it (the user may be reading away from the cursor)
            let (mut a, mut b) = (line.saturating_sub(40), line + 40);
            if let Some(r) = here.filter(|r| r.start.abs_diff(line) < EXCERPT_MAX) {
                (a, b) = (a.min(r.start), b.max(r.end));
            }
            let b = b.min(a + EXCERPT_MAX).min(text.len_lines().saturating_sub(1));
            let (fa, fb) = (text.line_to_byte(a), text.line_to_byte(b + 1));
            ctx.push_str(&format!(
                "<excerpt lines=\"{}-{}\">\n{}</excerpt>\n",
                a + 1,
                b + 1,
                text.byte_slice(fa..fb)
            ));
        }
    }
    let diags = &doc.lsp.diagnostics;
    if its_text && !diags.is_empty() {
        ctx.push_str("<diagnostics>\n");
        for d in diags.iter().take(30) {
            let l = text.byte_to_line(d.from.min(text.len_bytes()));
            let kind = ["", "error", "warning", "info", "hint"][d.severity as usize % 5];
            ctx.push_str(&format!(
                "line {}: {kind}: {}\n",
                l + 1,
                d.message.lines().next().unwrap_or_default()
            ));
        }
        ctx.push_str("</diagnostics>\n");
        chip.push_str(&format!(" · {} diagnostic{}", diags.len(), if diags.len() == 1 { "" } else { "s" }));
    }
    // The other open files — Claude reads them itself, except unsaved text it can't see on disk
    let others: Vec<_> =
        ed.docs.iter().filter(|d| d.id != doc.id && d.path.is_some() && !d.loading).collect();
    if !others.is_empty() {
        // (an unsaved one named like a key, not open to Claude: named, its text not sent)
        let unsaved_key = |d: &crate::document::Document| d.is_modified().then(|| shut(d)).flatten();
        ctx.push_str("<open-files>\n");
        for d in others.iter().take(30) {
            ctx.push_str(&d.display_name(&ed.session.root));
            ctx.push_str(match (d.is_modified(), unsaved_key(d).is_some()) {
                (true, true) => " (unsaved — named like a key, not sent)\n",
                (true, false) => " (unsaved)\n",
                _ => "\n",
            });
        }
        ctx.push_str("</open-files>\n");
        let mut budget = UNSAVED_MAX;
        for d in others.iter().filter(|d| d.is_modified() && !was_sent(d)) {
            if let Some(k) = unsaved_key(d) {
                withheld.push(k);
                continue;
            }
            let n = d.text.len_bytes();
            if n > budget.min(WHOLE_FILE_MAX) {
                continue;
            }
            budget -= n;
            ctx.push_str(&format!(
                "<unsaved-file path=\"{}\">\n{}</unsaved-file>\n",
                d.display_name(&ed.session.root),
                d.text
            ));
            sent.push((d.id, d.version()));
        }
    }
    ctx.push_str("</editor-context>");
    (ctx, chip, sent, withheld)
}

/// Per pane on screen (last frame, the focused one first): its document and the lines it shows. A document
/// in two panes counts once, as its first.
fn on_screen(ed: &Editor) -> Vec<(DocId, Range<usize>)> {
    let mut panes: Vec<_> = ed.screen.panes.iter().collect();
    crate::sorting::by_key(&mut panes, |p| p.0 != ed.session.focus);
    let mut out: Vec<(DocId, Range<usize>)> = Vec::new();
    for (view, _, _, rows) in panes {
        let Some(doc) = ed.session.views.iter().find(|v| v.id == *view).map(|v| v.doc) else { continue };
        let (Some(a), Some(b)) = (rows.iter().map(|r| r.0).min(), rows.iter().map(|r| r.0).max()) else {
            continue;
        };
        if !out.iter().any(|o| o.0 == doc) {
            out.push((doc, a..b + 1));
        }
    }
    out
}

// ── Links ─────────────────────────────────────────────────────────────────

/// A tool call → its row (paths from `root`).
fn look(root: &Path, name: &str, input: &serde_json::Value) -> Look {
    let s = |k: &str| input[k].as_str().unwrap_or_default().to_string();
    let n = |k: &str| input[k].as_u64().map(|v| v as usize);
    let shown = |p: &Path| shown(root, p);
    let resolve = |p: &str| resolve(root, p);
    let within =
        |p: String| if p.is_empty() { String::new() } else { format!("in {}", shown(Path::new(&p))) };
    match name {
        "Read" => {
            let path = resolve(&s("file_path"));
            let range = match (n("offset"), n("limit")) {
                (Some(o), Some(l)) => format!("L{}–{}", o.max(1), o.max(1) + l.max(1) - 1),
                (Some(o), None) => format!("L{}–", o.max(1)),
                _ => String::new(),
            };
            let line = n("offset").unwrap_or(1).saturating_sub(1);
            Look { verb: "read", what: shown(&path), pattern: false, range, link: Some(Link { path, line }) }
        }
        // Named as the tool it is — Claude's own Grep and Glob, or tarae's search (`search (project)`)
        "Grep" => {
            let place = if input["glob"].is_string() { s("glob") } else { s("path") };
            Look { verb: "grep", what: s("pattern"), pattern: true, range: within(place), link: None }
        }
        "Glob" => {
            Look { verb: "glob", what: s("pattern"), pattern: true, range: within(s("path")), link: None }
        }
        "Edit" | "Write" => {
            let path = resolve(&s("file_path"));
            let verb = if name == "Edit" { "edit" } else { "write" };
            Look {
                verb,
                what: shown(&path),
                pattern: false,
                range: String::new(),
                link: Some(Link { path, line: 0 }),
            }
        }
        "mcp__tarae__note" => {
            let gist = s("text").lines().next().unwrap_or_default().to_string();
            let gist = if gist.chars().count() > 48 {
                format!("{}…", gist.chars().take(47).collect::<String>())
            } else {
                gist
            };
            match n("reply_to") {
                Some(id) => {
                    Look { verb: "reply", what: format!("#{id}"), pattern: false, range: gist, link: None }
                }
                None => {
                    let path = resolve(&s("path"));
                    let line = n("line").unwrap_or(1).max(1) - 1;
                    let what = format!("{}:{}", shown(&path), line + 1);
                    Look { verb: "note", what, pattern: false, range: gist, link: Some(Link { path, line }) }
                }
            }
        }
        "mcp__tarae__plan" => {
            let (n, done) = input["steps"].as_array().map_or((0, 0), |a| {
                (
                    a.len(),
                    a.iter().filter(|s| matches!(s["status"].as_str(), Some("done" | "completed"))).count(),
                )
            });
            let title = s("title");
            let what = if title.is_empty() { format!("{n} steps") } else { title };
            Look { verb: "plan", what, pattern: false, range: format!("{done}/{n}"), link: None }
        }
        "mcp__tarae__run_tests" => {
            let path = resolve(&s("path"));
            let (what, line) = match n("line") {
                Some(l) => (format!("{}:{}", shown(&path), l.max(1)), l.max(1) - 1),
                None => (shown(&path), 0),
            };
            Look { verb: "test", what, pattern: false, range: String::new(), link: Some(Link { path, line }) }
        }
        "mcp__tarae__run" => {
            let cmd = s("command").split_whitespace().collect::<Vec<_>>().join(" ");
            let what = if cmd.chars().count() > 48 {
                format!("{}…", cmd.chars().take(47).collect::<String>())
            } else {
                cmd
            };
            Look { verb: "run", what, pattern: true, range: String::new(), link: None }
        }
        "mcp__tarae__screen" => {
            Look { verb: "look", what: "your screen".into(), pattern: true, range: String::new(), link: None }
        }
        "mcp__tarae__ui" => Look {
            verb: "ui",
            what: "what's on screen".into(),
            pattern: true,
            range: String::new(),
            link: None,
        },
        "mcp__tarae__git" => {
            let words: Vec<&str> = input["args"]
                .as_array()
                .map(|a| a.iter().filter_map(serde_json::Value::as_str).collect())
                .unwrap_or_default();
            Look { verb: "git", what: words.join(" "), pattern: true, range: String::new(), link: None }
        }
        "mcp__tarae__commit" => {
            let subject = input["message"].as_str().and_then(|m| m.lines().next()).unwrap_or_default();
            Look { verb: "commit", what: subject.into(), pattern: true, range: String::new(), link: None }
        }
        "mcp__tarae__procs" => {
            let what =
                if input["all"].as_bool() == Some(true) { "every session's" } else { "this session's" };
            Look { verb: "procs", what: what.into(), pattern: true, range: String::new(), link: None }
        }
        "mcp__tarae__references" => {
            let path = resolve(&s("path"));
            let (place, line) = match n("line") {
                Some(l) => (format!("{}:{}", shown(&path), l.max(1)), l.max(1) - 1),
                None => (shown(&path), 0),
            };
            let what = s("symbol");
            Look {
                verb: "references",
                what,
                pattern: true,
                range: format!("in {place}"),
                link: Some(Link { path, line }),
            }
        }
        "mcp__tarae__search" => {
            let place = if input["glob"].is_string() { s("glob") } else { s("path") };
            Look { verb: "search", what: s("pattern"), pattern: true, range: within(place), link: None }
        }
        // (its first file, where its first range starts; the rest counted)
        "mcp__tarae__read" => {
            let files: Vec<&str> = input["files"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_str)
                .collect();
            // (read as the tool reads it — `readtool::item`)
            let it = crate::readtool::item(files.first().copied().unwrap_or_default());
            let path = resolve(&it.path);
            let line = match &it.want {
                crate::readtool::Want::Lines(r) => r.first().map_or(0, |r| r.0 - 1),
                _ => 0,
            };
            let more = match files.len() {
                0 | 1 => String::new(),
                n => format!("+{} more", n - 1),
            };
            let asked = it.given.strip_prefix(it.path.as_str()).unwrap_or_default();
            let what = format!("{}{asked}", shown(&path));
            Look { verb: "read", what, pattern: false, range: more, link: Some(Link { path, line }) }
        }
        "mcp__tarae__symbols" => {
            let path = resolve(&s("path"));
            let (what, line) = match n("line") {
                Some(l) => (format!("{}:{}", shown(&path), l.max(1)), l.max(1) - 1),
                None => (shown(&path), 0),
            };
            Look {
                verb: "symbols",
                what,
                pattern: false,
                range: String::new(),
                link: Some(Link { path, line }),
            }
        }
        // At a place: the editor goes there; by name (definition), or the files (diagnostics): nowhere
        "mcp__tarae__definition" | "mcp__tarae__hover" => {
            let verb = if name == "mcp__tarae__hover" { "hover" } else { "definition" };
            let what = match (s("symbol"), n("column")) {
                (w, _) if !w.is_empty() => w,
                (_, Some(c)) => format!("column {c}"),
                _ => String::new(),
            };
            if s("path").is_empty() {
                return Look { verb, what, pattern: true, range: "by name".into(), link: None };
            }
            let path = resolve(&s("path"));
            let (place, line) = match n("line") {
                Some(l) => (format!("{}:{}", shown(&path), l.max(1)), l.max(1) - 1),
                None => (shown(&path), 0),
            };
            let link = Some(Link { path, line });
            Look { verb, what, pattern: true, range: format!("in {place}"), link }
        }
        "mcp__tarae__diagnostics" => {
            let paths: Vec<String> = input["paths"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|p| p.as_str())
                .map(|p| shown(&resolve(p)))
                .collect();
            let what = match paths.len() {
                0 => "your edits".to_string(),
                1..=2 => paths.join(", "),
                n => format!("{}, … {} files", paths[0], n),
            };
            let range = n("wait").filter(|&w| w > 0).map_or(String::new(), |w| format!("waits up to {w}s"));
            Look { verb: "diagnostics", what, pattern: false, range, link: None }
        }
        // (the editor goes to the name renamed; a replace goes nowhere — its review shows the files)
        "mcp__tarae__rename" => {
            let path = resolve(&s("path"));
            let (place, line) = match n("line") {
                Some(l) => (format!("{}:{}", shown(&path), l.max(1)), l.max(1) - 1),
                None => (shown(&path), 0),
            };
            let from = if s("symbol").is_empty() { "…".to_string() } else { s("symbol") };
            let what = format!("{from} → {}", s("new_name"));
            Look {
                verb: "rename",
                what,
                pattern: true,
                range: format!("in {place}"),
                link: Some(Link { path, line }),
            }
        }
        "mcp__tarae__replace" => {
            let place = if input["glob"].is_string() { s("glob") } else { s("path") };
            let what = format!("{} → {}", s("pattern"), s("replacement"));
            let mut range = within(place);
            if input["preview"].as_bool() == Some(true) {
                range = format!("{range} preview").trim().to_string();
            }
            Look { verb: "replace", what, pattern: true, range, link: None }
        }
        // tarae's `edit`: its first file, and how many more
        "mcp__tarae__edit" => {
            let items = input["edits"].as_array().map(Vec::as_slice).unwrap_or_default();
            let mut files: Vec<&str> = items.iter().filter_map(|e| e["file_path"].as_str()).collect();
            files.dedup();
            let path = resolve(files.first().copied().unwrap_or_default());
            let range = match files.len() {
                0 | 1 => String::new(),
                n => format!("+{} more", n - 1),
            };
            Look {
                verb: "edit",
                what: shown(&path),
                pattern: false,
                range,
                link: Some(Link { path, line: 0 }),
            }
        }
        other => {
            Look { verb: "use", what: other.to_string(), pattern: false, range: String::new(), link: None }
        }
    }
}

/// What a row and the follow card call a call's tool — its own name, so Claude's Grep reads apart from
/// tarae's project search (the editor's `space /`).
pub fn label(verb: &str) -> &str {
    match verb {
        "search" => "search (project)",
        v => v,
    }
}

/// Relative to the project root (`Editor::root`) when inside it.
pub fn shown(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

/// A path Claude gave: a relative one is from the project root (where its process runs).
pub fn resolve(root: &Path, p: &str) -> PathBuf {
    let p = Path::new(p);
    if p.is_absolute() { p.to_path_buf() } else { root.join(p) }
}

/// A stop of a walk-through, as Claude writes it in its answer: `<show at="path:start-end">why</show>`
/// (byte range of the whole tag · where · why).
pub fn show_tags(text: &str) -> Vec<(Range<usize>, String, String)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(k) = text[from..].find("<show") {
        let start = from + k;
        let Some(close) = text[start..].find('>').map(|c| start + c) else { break };
        let Some(end) = text[close..].find("</show>").map(|e| close + e) else { break };
        let head = &text[start + 5..close];
        let at = head
            .split_once("at=")
            .map(|(_, v)| v.trim().trim_matches(|c| c == '"' || c == '\''))
            .unwrap_or_default();
        out.push((start..end + 7, at.to_string(), text[close + 1..end].trim().to_string()));
        from = end + 7;
    }
    out
}

/// Claude's words as they read: a stop's tag becomes a link line (`▸ path:start-end · why`), and one still
/// being written doesn't show yet.
pub fn displayed(text: &str) -> std::borrow::Cow<'_, str> {
    if !text.contains("<show") {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::new();
    let mut last = 0;
    for (r, at, why) in show_tags(text) {
        out.push_str(&text[last..r.start]);
        out.push_str(&format!("▸ {at} · {why}"));
        last = r.end;
    }
    let rest = &text[last..];
    out.push_str(rest.find("<show").map_or(rest, |k| &rest[..k]));
    std::borrow::Cow::Owned(out)
}

/// `path:line` references in text (byte range of `path:line` · target). Paths must look like files
/// (a `/` or an extension) — no disk access, a miss only shows when followed. Relative ones are from `root`.
pub fn refs_in(root: &Path, text: &str) -> Vec<(Range<usize>, Link)> {
    let is_path = |c: char| c.is_ascii_alphanumeric() || "_./-~+@".contains(c);
    let b = text.as_bytes();
    let mut out = Vec::new();
    for (i, _) in text.match_indices(':') {
        let digits = text[i + 1..].bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            continue;
        }
        let start = text[..i]
            .rfind(|c: char| !is_path(c))
            .map_or(0, |j| j + text[j..].chars().next().map_or(1, char::len_utf8));
        let path = text[start..i].trim_end_matches('.');
        let file_like = path.contains('/')
            || path
                .rsplit_once('.')
                .is_some_and(|(a, e)| !a.is_empty() && e.chars().any(|c| c.is_ascii_alphabetic()));
        // URLs (`http://x.com:80`) and times (`12:30`) aren't files
        if path.is_empty()
            || !file_like
            || (start > 0 && b[start - 1] == b':')
            || !path.chars().any(|c| c.is_ascii_alphabetic())
        {
            continue;
        }
        let Ok(line) = text[i + 1..i + 1 + digits].parse::<usize>() else { continue };
        out.push((start..i + 1 + digits, Link { path: resolve(root, path), line: line.saturating_sub(1) }));
    }
    out
}

/// Opens a link (keeps the chat's focus — the code shows beside the conversation).
pub fn jump(ed: &mut Editor, link: Link) {
    if !link.path.is_file() {
        return ed.set_status(format!("{} isn't a file here", shown(&ed.session.root, &link.path)));
    }
    match ed.open_at(&link.path, link.line, 0) {
        // Centered — the code around it is what the conversation is about
        Ok(()) => ed.align_view(crate::viewalign::Place::Center),
        Err(e) => ed.set_error(format!("{e:#}")),
    }
}

/// `C-]` (`C-g`): the newest link, then one further back each press (wraps) — a tool call's row goes to its
/// replay stop (an edit's changes shown).
pub fn follow(ed: &mut Editor) {
    let Some(c) = &mut ed.session.chat else { return };
    let hops = c.hops(&ed.session.root);
    if hops.is_empty() {
        return ed.set_status("nothing to go to yet");
    }
    let hop = hops[hops.len() - 1 - c.hop % hops.len()].clone();
    c.hop = (c.hop + 1) % hops.len();
    match hop {
        Hop::Place(link, call) => {
            if !call.is_some_and(|call| crate::follow::replay_call(ed, &call)) {
                jump(ed, link);
            }
        }
        Hop::Element(id) => ed.ui_jump(&id),
    }
}

// ── Keys ───────────────────────────────────────────────────────────────────

/// Keys while focus is in the chat panel. True if handled.
/// Keys the minimized chat (`Chat::compact`) still takes as they are: Esc, and the ones that act on the chat
/// rather than its input — stop, follow, go to, undo the turn, model, past, new, copy, apply. Any other would
/// type, edit or send what isn't on screen.
pub fn acts_unseen(key: Key) -> bool {
    key.code == Code::Esc
        || (key.ctrl
            && !key.alt
            && matches!(key.code, Code::Char('c' | 'f' | 'g' | ']' | 'z' | 't' | 'p' | 'l' | 'y' | 'r')))
}

/// The chat minimized while Claude works has the keys, its input not on screen (slip 3, 2026-10-04): a key
/// that would type, edit or send there brings the panel back first. A letter comes back typed — the input
/// is in sight with it, and nothing is sent (#151: the first letter of a follow-up was swallowed); space
/// (the leader: slip 3 was `space W` typed into it), Enter and editing keys only bring it back, said so.
/// True if it was that key.
pub fn unfold_key(ed: &mut Editor, key: Key) -> bool {
    if !ed.pending.is_empty() {
        return false;
    }
    let Some(c) = ed.session.chat.as_mut().filter(|c| c.focused && c.compact()) else { return false };
    // (as `key`: from an empty input `C-w` is the windows')
    if acts_unseen(key) || (key.ctrl && key.code == Code::Char('w') && c.input.is_empty()) {
        return false;
    }
    c.unfolded = true;
    match key.code {
        Code::Char(x) if !key.ctrl && !key.alt && !x.is_whitespace() => {
            c.input.insert(c.cursor, x);
            c.cursor += x.len_utf8();
        }
        _ => {
            ed.set_status(format!("{key} brought the chat back — it wasn't typed; its input is in sight now"))
        }
    }
    true
}

/// Lines a page key scrolls the conversation.
const PAGE: usize = 10;

pub fn key(ed: &mut Editor, key: Key) -> bool {
    // (the second key of a sequence — `C-w >` from an empty input — is the keymap's)
    if !ed.pending.is_empty() {
        return false;
    }
    // (its view mode has the keys — what reaches here is the chat's chords, and `C-w`: the windows')
    if crate::view::chat_view_of(ed).is_some() && !crate::view::chat_chord(ed, key) {
        return false;
    }
    let Some(c) = ed.session.chat.as_mut().filter(|c| c.focused) else { return false };
    // An empty input: `C-w` is the windows' (sizes — `C-w >` widens the chat), nothing to delete
    if key.ctrl && key.code == Code::Char('w') && c.input.is_empty() {
        return false;
    }
    // The input's rows of the keymap (`[chat]` — `esc` to its view mode) before what it types
    if let Some(crate::keymap::KeyTrie::Leaf(cmds)) = ed.keymaps.chat.search(&[key]) {
        let cmds = cmds.clone();
        ed.run(&cmds, None);
        return true;
    }
    let ch = |c: &Chat| c.input[..c.cursor].chars().next_back().map_or(0, char::len_utf8);
    match (key.code, key.ctrl, key.alt) {
        (Code::Esc, ..) => c.focused = false,
        (Code::Enter, false, true) | (Code::Char('j'), true, _) => {
            c.input.insert(c.cursor, '\n');
            c.cursor += 1;
        }
        (Code::Enter, ..) => send(ed),
        (Code::Char('c'), true, _) => {
            if c.busy() {
                stop(ed);
            } else {
                c.input.clear();
                c.cursor = 0;
            }
        }
        (Code::Char('l'), true, _) => reset(ed),
        (Code::Char('p'), true, _) => past_picker(ed),
        (Code::Char('t'), true, _) => crate::models::picker(ed),
        (Code::Char('z'), true, _) => {
            if let Err(e) = crate::checkpoint::step(ed, true) {
                ed.set_status(e);
            }
        }
        (Code::Char('r'), true, _) => apply_code(ed),
        (Code::Char('s'), true, _) => {
            if let Err(e) = attach_screen(ed) {
                ed.set_status(e);
            }
        }
        // Point at an element on screen: its id goes with the message (`uistate/point.rs`)
        (Code::Char('o'), true, _) => crate::uistate::point::start(ed),
        // An empty input: Backspace takes off what goes with it — the last element pointed at, the screen
        (Code::Backspace, ..) if c.input.is_empty() && !c.pointed.is_empty() => {
            c.pointed.pop();
            crate::reads::unpointed(ed);
        }
        (Code::Backspace, ..) if c.input.is_empty() && c.screen.is_some() => c.screen = None,
        // (C-g too — zellij takes it for its lock mode)
        (Code::Char(']') | Code::Char('g'), true, _) => follow(ed),
        (Code::Char('f'), true, _) => crate::follow::toggle(ed),
        (Code::Char('y'), true, _) => match c.last_code() {
            Some(code) => crate::clipboard::copy(&ed.events.jobs(), code, "the code block".into()),
            None => ed.set_status("no code block in the last answer"),
        },
        // The editor's page keys page the conversation (#153) — `C-u` from an empty input (with text it
        // clears up to the cursor, as readline's; `↑` scrolls only from an empty input too)
        (Code::Char('d'), true, _) => c.scroll = c.scroll.saturating_sub(PAGE),
        (Code::Char('u'), true, _) if c.input.is_empty() => c.scroll += PAGE,
        (Code::Char('u'), true, _) => {
            c.input.drain(..c.cursor);
            c.cursor = 0;
        }
        (Code::Char('w'), true, _) => {
            let before = &c.input[..c.cursor];
            let trimmed = before.trim_end();
            let start = trimmed.rfind(char::is_whitespace).map_or(0, |i| i + 1);
            c.input.drain(start..c.cursor);
            c.cursor = start;
        }
        (Code::Char('a'), true, _) | (Code::Home, ..) => c.cursor = 0,
        (Code::Char('e'), true, _) | (Code::End, ..) => c.cursor = c.input.len(),
        (Code::Left, ..) => c.cursor -= ch(c),
        (Code::Right, ..) => c.cursor += c.input[c.cursor..].chars().next().map_or(0, char::len_utf8),
        (Code::Backspace, ..) => {
            let n = ch(c);
            c.input.drain(c.cursor - n..c.cursor);
            c.cursor -= n;
        }
        (Code::Delete, ..) if c.cursor < c.input.len() => {
            let n = c.input[c.cursor..].chars().next().map_or(0, char::len_utf8);
            c.input.drain(c.cursor..c.cursor + n);
        }
        // The queued message back to edit — or drop (#23): it's out of the queue while it's in the input
        (Code::Up, ..) if c.input.is_empty() && c.queued.is_some() => {
            c.input = c.queued.take().unwrap_or_default();
            c.queued_gone();
            c.cursor = c.input.len();
        }
        (Code::PageUp, ..) | (Code::Up, ..) if c.input.is_empty() || key.code == Code::PageUp => {
            c.scroll += if key.code == Code::PageUp { PAGE } else { 1 };
        }
        (Code::PageDown, ..) | (Code::Down, ..) if c.input.is_empty() || key.code == Code::PageDown => {
            c.scroll = c.scroll.saturating_sub(if key.code == Code::PageDown { PAGE } else { 1 });
        }
        // Tab in an empty chat = cycles through example questions
        (Code::Tab, ..) if c.msgs.is_empty() => {
            let next =
                SUGGESTIONS.iter().position(|s| *s == c.input).map_or(0, |i| (i + 1) % SUGGESTIONS.len());
            c.input = SUGGESTIONS[next].to_string();
            c.cursor = c.input.len();
        }
        (Code::Char(x), false, false) => {
            c.input.insert(c.cursor, x);
            c.cursor += x.len_utf8();
        }
        // (a `C-`/`A-` key the chat has no use for: said, not dropped — the keys stay the chat's)
        (Code::Char(_), ..) => ed.key_unused(key, "the chat", "esc gives the keys to the code"),
        _ => {}
    }
    true
}

/// Last answer's code block → review that replaces the current selection (y/n — same path as ask).
pub fn apply_code(ed: &mut Editor) {
    let Some(code) = ed.session.chat.as_ref().and_then(Chat::last_code) else {
        return ed.set_status("no code block in the last answer");
    };
    let doc = ed.doc();
    let r = doc.selection().primary().min_width_1(&doc.text);
    let old = doc.text.byte_slice(r.from()..r.to()).to_string();
    let new = llm::match_trailing_newline(&old, code);
    let doc_id = doc.id;
    if let Some(c) = &mut ed.session.chat {
        c.focused = false;
    }
    llm::review_with(ed, doc_id, "apply code from chat", vec![(r.from(), r.to(), old, new)], None);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// cardburden 4: where a command ran is its guard's — the run's own record, nothing guessed: the sandbox
    /// (Seatbelt; Linux's tier from its confinement), a socket granted (Docker's: past the sandbox, as outside),
    /// the network open; outside, no sandbox, the sandbox off — and the card it ran from with Claude's reason.
    #[test]
    fn where_a_command_ran_is_its_guards() {
        use crate::sandbox::{Env, Guard, Policy};
        let base = || Policy::for_project(&Env::default());
        let ran = |g: Guard| Ran::of(&g, None, None, Vec::new());
        let inside = ran(Guard::In(Box::new(base())));
        assert_eq!((inside.mark.as_str(), inside.past), ("sandbox", false));
        let how = if cfg!(target_os = "linux") { "Landlock" } else { "Seatbelt" };
        assert!(inside.why.starts_with(&format!("In tarae's sandbox ({how}")), "{}", inside.why);
        let docker =
            ran(Guard::In(Box::new(Policy { sockets: vec!["/x/.colima/docker.sock".into()], ..base() })));
        assert_eq!((docker.mark.as_str(), docker.past), ("sandbox + Docker socket", true));
        assert!(docker.why.contains("Docker's: as much as outside the sandbox"), "{}", docker.why);
        let other =
            ran(Guard::In(Box::new(Policy { sockets: vec!["/x/app.sock".into()], network: true, ..base() })));
        assert_eq!((other.mark.as_str(), other.past), ("sandbox + app.sock + network", false));
        for (g, mark) in [
            (Guard::Outside, "outside"),
            (Guard::Without("no Landlock".into()), "no sandbox"),
            (Guard::Off, "sandbox off"),
        ] {
            let r = ran(g);
            assert_eq!((r.mark.as_str(), r.past), (mark, true));
        }
        let carded = Ran::of(&Guard::Outside, Some(12), Some("it needs the network"), Vec::new());
        assert!(
            carded.why.ends_with(" From card#12 — Claude's reason: it needs the network."),
            "{}",
            carded.why
        );
    }

    /// cardburden 4b: where a command ran was only a mark at its row's end — its reason in `tarae state`
    /// alone. A click on its trail — `ok · outside ▲1`, the verdict too (the review of 4b: only the mark took it,
    /// the docs said the trail) — says it: the command, where, why from the run's record (the card it ran from,
    /// Claude's reason there), each warning — a warning when that was past the sandbox. A click on the command
    /// isn't one. (A stand-in row; nothing runs.)
    #[test]
    fn a_click_on_where_a_command_ran_says_why() {
        use crate::event::{Mouse, MouseKind};
        let mut ed = editor("");
        let mut c = Chat::new();
        let look = Look {
            verb: "run",
            what: "docker run alpine true".into(),
            pattern: true,
            range: "ok".into(),
            link: None,
        };
        let warned = crate::cardwarn::Warned {
            id: "docker-own-config".into(),
            said: "Docker: no DOCKER_CONFIG.".into(),
            whose: "tools pack".into(),
            asks: false,
            start: false,
        };
        let mut m = Msg::tool(look, "c1");
        m.ran = Some(Ran::of(
            &crate::sandbox::Guard::Outside,
            Some(12),
            Some("it needs the network"),
            vec![warned],
        ));
        c.msgs.push(m);
        ed.session.chat = Some(c);
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 120, 30).unwrap();
        let (y, x0, x1, call) = ed.screen.chat_ran.borrow().first().cloned().expect("the mark's spot");
        assert_eq!(call, "c1");
        let mut parser = vt100::Parser::new(30, 120, 0);
        parser.process(&bytes);
        let row: Vec<char> = parser.screen().rows(0, 120).nth(y).unwrap().chars().collect();
        let under: String = row[x0..x1].iter().collect();
        assert_eq!(under, "ok · outside ▲1", "the spot is the whole trail, from its verdict: {row:?}");
        let click = |ed: &mut Editor, x: usize| {
            let at = |kind| Mouse { kind, x: x as u16, y: y as u16, alt: false };
            ed.handle_mouse(at(MouseKind::Down));
            ed.handle_mouse(at(MouseKind::Up));
        };
        let said = |ed: &Editor| ed.messages.iter().any(|m| m.2.contains("ran outside"));
        click(&mut ed, x0 - 4);
        assert!(!said(&ed), "on the command, before its trail: not it");
        // (on its verdict, `ok` — the start of the trail)
        click(&mut ed, x0);
        let m = ed.messages.last().expect("said");
        assert_eq!(
            m.2,
            "docker run alpine true — ran outside\nOutside the sandbox, with all your permissions — said yes to. \
             From card#12 — Claude's reason: it needs the network.\n▲ Docker: no DOCKER_CONFIG. (tools pack)"
        );
        assert_eq!(m.1, crate::editor::ToastKind::Warning, "past the sandbox");
    }

    fn editor(text: &str) -> Editor {
        let mut ed = Editor::new(crate::config::Config::default());
        ed.docs[0].text = ropey::Rope::from_str(text);
        ed
    }

    fn settle_until(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
        let end = Instant::now() + std::time::Duration::from_secs(5);
        while !done(ed) {
            let left = end.saturating_duration_since(Instant::now());
            let Some(ev) = ed.events.recv_timeout(left) else {
                let rows: Vec<_> =
                    ed.session.chat.iter().flat_map(|c| c.msgs.iter().map(|m| &m.text)).collect();
                panic!("no event before the timeout — the chat: {rows:?}");
            };
            ed.handle_event(ev);
        }
    }

    /// A message whose write fails because its claude already ended — the writer met the ended process on an
    /// earlier line (its `initialize`) before tarae heard of the exit — is the turn's, as one written a moment
    /// before that exit: claude no longer having the conversation, it goes again to the new one and is answered
    /// there, no error said; a claude that just died says so once. (The fake refusing `--resume` exits before
    /// reading anything; the message is sent after the writer met that exit, before tarae handles its event.)
    #[test]
    fn a_message_written_after_claude_ended_waits_for_why_it_ended() {
        let init = serde_json::json!({"type": "system", "subtype": "init", "session_id": "s-2"});
        let text = serde_json::json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "Hello back."}}});
        let result = serde_json::json!({"type": "result", "result": "done"});
        // (`gone`: it no longer has the conversation; else it dies without a word, resumed or not)
        let started = |gone: bool| {
            let mut ed = editor("");
            ed.config.llm.command = "sh".into();
            let ends = match gone {
                true => {
                    "case \"$*\" in *--resume*) echo 'No conversation found with session ID: s-1' >&2; exit 1;; esac;"
                }
                false => "echo 'it fell over' >&2; exit 1;",
            };
            ed.config.llm.args = vec![
                "-c".into(),
                format!(
                    "{ends} while read -r line; do case \"$line\" in '{{\"message\"'*) \
                     printf '%s\\n' '{init}' '{text}' '{result}';; esac; done"
                ),
            ];
            let mut c = Chat::new();
            (c.resume, c.conversation) = (Some("s-1".into()), Some("s-1".into()));
            ed.session.chat = Some(c);
            ensure_proc(&mut ed);
            // (it starts once what it waits on is read — and nothing after: its exit stays unheard)
            settle_until(&mut ed, |ed| ed.session.chat.as_ref().unwrap().proc.is_some());
            // (the writer has met the ended process once a line can't be handed to it)
            let end = Instant::now() + std::time::Duration::from_secs(5);
            let p = ed.session.chat.as_ref().unwrap().proc.as_ref().unwrap();
            while p.stdin.send(String::new()).is_ok() {
                assert!(Instant::now() < end, "the writer still writes");
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let c = ed.session.chat.as_mut().unwrap();
            c.input = "again".into();
            send(&mut ed);
            ed
        };
        let said = |ed: &Editor, t: &str| {
            ed.session.chat.as_ref().unwrap().msgs.iter().filter(|m| m.text.contains(t)).count()
        };
        let asked = |ed: &Editor| {
            let c = ed.session.chat.as_ref().unwrap();
            c.msgs.iter().filter(|m| m.role == Role::User && m.text == "again").count()
        };
        let mut ed = started(true);
        settle_until(&mut ed, |ed| said(ed, "Hello back.") == 1 && !ed.session.chat.as_ref().unwrap().busy());
        assert_eq!(said(&ed, "no longer has this conversation"), 1);
        assert_eq!((said(&ed, "broken pipe"), said(&ed, "claude exited")), (0, 0), "not an error");
        assert_eq!(asked(&ed), 1, "asked once, answered in the new one");
        assert!(ed.session.chat.as_ref().unwrap().input.is_empty());
        // A claude that just died: said once, as an exit mid-turn is
        let mut ed = started(false);
        settle_until(&mut ed, |ed| ed.session.chat.as_ref().unwrap().proc.is_none());
        assert_eq!(said(&ed, "claude exited: it fell over"), 1);
        assert_eq!(said(&ed, "broken pipe"), 0);
        assert_eq!(asked(&ed), 1);
        assert!(!ed.session.chat.as_ref().unwrap().busy(), "the turn is over");
    }

    /// #153: the editor's page keys page the conversation — `C-u` up from an empty input, `C-d` down; with
    /// text in the input `C-u` still clears up to the cursor.
    #[test]
    fn page_keys_page_the_chat() {
        let mut ed = editor("");
        let mut c = Chat::new();
        c.focused = true;
        ed.session.chat = Some(c);
        let press = |ed: &mut Editor, k: &str| assert!(key(ed, k.parse().unwrap()), "{k} is the chat's");
        let scroll = |ed: &Editor| ed.session.chat.as_ref().unwrap().scroll;
        press(&mut ed, "C-u");
        press(&mut ed, "C-u");
        assert_eq!(scroll(&ed), 2 * PAGE);
        press(&mut ed, "C-d");
        assert_eq!(scroll(&ed), PAGE);
        let c = ed.session.chat.as_mut().unwrap();
        (c.input, c.cursor) = ("draft".into(), 5);
        press(&mut ed, "C-u");
        let c = ed.session.chat.as_ref().unwrap();
        assert_eq!((c.input.as_str(), c.scroll), ("", PAGE), "with text: cleared, not scrolled");
        press(&mut ed, "C-d");
        assert_eq!(scroll(&ed), 0);
    }

    /// One character (a multi-byte grapheme) under the cursor isn't a selection; two are.
    #[test]
    fn selection_context_counts_graphemes() {
        use crate::selection::{Range, Selection};
        let mut ed = editor("한글\n");
        ed.doc_mut().set_selection(Selection::single(Range::new(0, 3)));
        assert!(!context(&ed, None, &[]).0.contains("<selection"));
        ed.doc_mut().set_selection(Selection::single(Range::new(0, 6)));
        assert!(context(&ed, None, &[]).0.contains("<selection lines=\"1-1\">\n한글\n</selection>"));
    }

    /// A project of this test's own temp folder: `.env` holding a fake token, its exception `.env.example`,
    /// and a plain file (`.txt` — no language server starts) — real paths, as `Editor::open` keeps them.
    fn key_project(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
        let dir = crate::scratch::Scratch::new(&format!("chat-{name}")).into_path();
        std::fs::create_dir_all(dir.join("project")).unwrap();
        let project = std::fs::canonicalize(dir.join("project")).unwrap();
        let (env, example, plain) =
            (project.join(".env"), project.join(".env.example"), project.join("notes.txt"));
        std::fs::write(&env, "API_KEY=tarae-fake-token\n").unwrap();
        std::fs::write(&example, "API_KEY=example-only\n").unwrap();
        std::fs::write(&plain, "plain words\n").unwrap();
        (dir, env, example, plain)
    }

    /// 7c′: the message's context names a file in the project named like a key (`runtime/secret-names.txt`)
    /// but carries none of its text — not the file, its selection, its diagnostics (a message can quote a
    /// line), nor its unsaved buffer behind another file — as Claude's Read asks for it (7c) and a run can't
    /// read it (7a/7b). An exception (`!.env.example`) goes as any file does; so does one opened: for this
    /// conversation (`Grant::Key`), kept for the project (the chat's copy), or let go with the next message
    /// (`y` on the card pointing at it raised). The token is a fake.
    #[test]
    fn a_key_named_file_s_text_stays_out_of_the_context() {
        use crate::selection::{Range, Selection};
        let (dir, env, example, plain) = key_project("key-context");
        let mut ed = editor("");
        ed.session.root = env.parent().unwrap().to_path_buf();
        let mut c = Chat::new();
        c.kept_keys = Some(Vec::new());
        ed.session.chat = Some(c);
        ed.open(&env).unwrap();
        let n = ed.doc().text.len_bytes();
        ed.doc_mut().set_selection(Selection::single(Range::new(0, n)));
        ed.doc_mut().lsp.diagnostics = vec![crate::lsp::Diagnostic {
            from: 0,
            to: 7,
            severity: 1,
            message: "unexpected tarae-fake-token".into(),
            raw: serde_json::json!({}),
            source: 0,
        }];
        let (ctx, chip, sent, withheld) = context(&ed, None, &[]);
        assert!(!ctx.contains("tarae-fake-token"), "{ctx}");
        assert!(ctx.contains("file: .env — named like a key — not sent\n"), "{ctx}");
        assert!(!ctx.contains("<selection") && !ctx.contains("<diagnostics>"), "{ctx}");
        assert!(chip.ends_with(" · not sent"), "{chip}");
        assert!(sent.is_empty(), "no version of it counts as sent");
        assert_eq!(withheld, std::slice::from_ref(&env));
        // Unsaved, behind another file: named, its text not sent
        use crate::transaction::{Change, Transaction};
        ed.doc_mut().apply(&Transaction::new(vec![Change::insert(0, "OTHER=tarae-fake-token-2\n")]));
        ed.open(&plain).unwrap();
        let (ctx, _, _, withheld) = context(&ed, None, &[]);
        assert!(!ctx.contains("tarae-fake-token") && ctx.contains("plain words"), "{ctx}");
        assert!(ctx.contains(".env (unsaved — named like a key, not sent)\n"), "{ctx}");
        assert_eq!(withheld, std::slice::from_ref(&env));
        // An exception goes as any file
        ed.open(&example).unwrap();
        let (ctx, _, _, withheld) = context(&ed, None, &[]);
        assert!(ctx.contains("example-only") && withheld == [env.clone()], "{ctx}");
        // Opened — each way — its text goes, the unsaved buffer's too
        ed.open(&env).unwrap();
        let opened: [fn(&mut Chat, &Path); 3] = [
            |c, k| c.allowed.keys.push(k.to_path_buf()),
            |c, k| c.kept_keys = Some(vec![k.to_path_buf()]),
            |c, k| {
                c.pointed.push("pane#1".into());
                c.key_pass.push(("pane#1".into(), k.to_path_buf()));
            },
        ];
        for open in opened {
            let mut c = Chat::new();
            c.kept_keys = Some(Vec::new());
            open(&mut c, &env);
            ed.session.chat = Some(c);
            let pass = crate::reads::input_pass(&ed);
            let (ctx, chip, _, withheld) = context(&ed, None, &pass);
            assert!(
                ctx.contains("<file-content>\nOTHER=tarae-fake-token-2\nAPI_KEY=tarae-fake-token\n"),
                "{ctx}"
            );
            assert!(
                ctx.contains("<diagnostics>") && !chip.contains("not sent") && withheld.is_empty(),
                "{ctx}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 7c′ through the chat: with `.env` in front, what reaches claude names it only, and the chat says once
    /// a conversation that its text wasn't sent and how to send it. Pointing at its pane (`C-o`) is the
    /// user's own act: the read card asks, the message waits for the answer — `y` sends its text with that
    /// message only, `n` keeps it to its name; taking the chip off drops the card, or the `y` given for it;
    /// a new conversation drops the card; `A` isn't an answer (a key is opened once at a time), but one an
    /// earlier build kept for the project is open. The fake claude logs each message to this test's temp folder
    /// (the token is a fake).
    /// The chat over `.env` in front (`key_project`), its claude a fake that logs each message it's sent to
    /// the test's temp folder — one process a turn —, the project's kept keys read: (dir, .env, notes.txt,
    /// the log, the editor).
    fn key_chat(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf, Editor) {
        let (dir, env, plain, log, mut ed) = key_editor(name);
        open(&mut ed);
        settle_until(&mut ed, |ed| ed.session.chat.as_ref().is_some_and(|c| c.kept_keys.is_some()));
        (dir, env, plain, log, ed)
    }

    /// `key_chat` before the chat opens.
    fn key_editor(name: &str) -> (PathBuf, PathBuf, PathBuf, PathBuf, Editor) {
        let (dir, env, _, plain) = key_project(name);
        let log = dir.join("stdin.log");
        let result = serde_json::json!({"type": "result", "result": "ok"}).to_string();
        let script = format!(
            "read -r init; read -r line; printf '%s\\n' \"$line\" >> '{}'; printf '%s\\n' '{result}'",
            log.display()
        );
        let mut ed = editor("");
        ed.session.root = env.parent().unwrap().to_path_buf();
        ed.allow_path = Some(dir.join("state/sandbox.json"));
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), script];
        ed.open(&env).unwrap();
        (dir, env, plain, log, ed)
    }

    /// The messages the fake claude was sent, one a line.
    fn messages(log: &Path) -> Vec<String> {
        std::fs::read_to_string(log).unwrap_or_default().lines().map(str::to_string).collect()
    }

    /// Events until the turn under way is over, its process gone, and nothing waits to go.
    fn turn_done(ed: &mut Editor) {
        let done = |c: &Chat| !c.busy() && c.proc.is_none() && c.queued.is_none();
        settle_until(ed, |ed| ed.session.chat.as_ref().is_some_and(done));
    }

    /// `C-o` and the label of element `id` typed, on the frame drawn now — the read card asks.
    fn point_at(ed: &mut Editor, id: &str) {
        crate::term::render(ed, &mut Vec::<u8>::new(), 140, 40).unwrap();
        crate::uistate::point::start(ed);
        let i = ed.pointing.as_ref().unwrap().targets.iter().position(|t| t.id == id).expect("its label");
        let (a, b) = crate::labels::Labels::label(i);
        for c in [a, b] {
            crate::uistate::point::key(ed, c.to_string().parse().unwrap());
        }
        assert!(ed.offers.iter().any(|o| o.title == crate::reads::POINTED_TITLE), "the read card asks");
    }

    /// `k` on the front card, drawn and in sight long enough for an answer.
    fn answer_card(ed: &mut Editor, k: &str) {
        crate::term::render(ed, &mut Vec::<u8>::new(), 140, 40).unwrap();
        let o = ed.offers.first().map(|o| (o.id, o.rev)).unwrap();
        ed.offer_seen.set(Some((o.0, o.1, Instant::now() - crate::offer::SETTLE * 2)));
        assert!(ed.offer_key(k.parse().unwrap()), "{k} answers the card");
    }

    #[test]
    fn a_key_named_file_reaches_claude_only_when_the_user_lets_it() {
        let (dir, env, _, log, mut ed) = key_chat("key-messages");
        let messages = || messages(&log);
        let ask = |ed: &mut Editor, q: &str| {
            ed.session.chat.as_mut().unwrap().input = q.into();
            let n = messages().len();
            send(ed);
            turn_done(ed);
            assert_eq!(messages().len(), n + 1, "{q} went");
            messages().pop().unwrap()
        };
        let said = |ed: &Editor| {
            let msgs = &ed.session.chat.as_ref().unwrap().msgs;
            msgs.iter().filter(|m| m.role == Role::Note && m.text.contains("wasn't sent to Claude")).count()
        };
        for q in ["first", "second"] {
            let m = ask(&mut ed, q);
            assert!(
                !m.contains("tarae-fake-token") && m.contains(".env — named like a key — not sent"),
                "{m}"
            );
        }
        assert_eq!(said(&ed), 1, "said once a conversation");
        let note = ed.session.chat.as_ref().unwrap().msgs.iter().find(|m| m.role == Role::Note).unwrap();
        assert!(note.text.starts_with(".env is named like a key") && note.text.contains("point at it (C-o)"));
        // Pointing at its pane: the card asks; the message waits for the answer
        let pane = format!("pane#{}", ed.session.focus);
        let point = |ed: &mut Editor| point_at(ed, &pane);
        let answer = answer_card;
        point(&mut ed);
        ed.session.chat.as_mut().unwrap().input = "this one".into();
        send(&mut ed);
        assert_eq!(messages().len(), 2, "it waits for the answer");
        assert_eq!(ed.session.chat.as_ref().unwrap().input, "this one", "kept in the input");
        ed.session.chat.as_mut().unwrap().input.clear();
        answer(&mut ed, "y");
        let m = ask(&mut ed, "this one");
        assert!(m.contains("API_KEY=tarae-fake-token"), "y: this message carries it: {m}");
        let m = ask(&mut ed, "and after");
        assert!(!m.contains("tarae-fake-token"), "y was for that message only: {m}");
        point(&mut ed);
        answer(&mut ed, "n");
        let m = ask(&mut ed, "no");
        assert!(!m.contains("tarae-fake-token"), "n: its name only: {m}");
        assert!(
            m.contains(&format!("<pointed id=\\\"{pane}\\\">{pane} — .env, named like a key — not sent")),
            "{m}"
        );
        // The chip taken off: its card goes with it — and so does a `y` already given for it
        let chip_off = |ed: &mut Editor| {
            // (the card took the keys from the empty input — back in the chat)
            ed.session.chat.as_mut().unwrap().focused = true;
            assert!(key(ed, "backspace".parse().unwrap()));
        };
        point(&mut ed);
        chip_off(&mut ed);
        assert!(!ed.offers.iter().any(|o| o.title == crate::reads::POINTED_TITLE), "dropped with its chip");
        point(&mut ed);
        answer(&mut ed, "y");
        chip_off(&mut ed);
        let m = ask(&mut ed, "the chip went");
        assert!(!m.contains("tarae-fake-token"), "its y went with the chip: {m}");
        assert_eq!(said(&ed), 1);
        // A new conversation: a card pointing raised doesn't hold its first message
        point(&mut ed);
        reset(&mut ed);
        assert!(!ed.offers.iter().any(|o| o.title == crate::reads::POINTED_TITLE), "dropped with it");
        let m = ask(&mut ed, "a new conversation, first");
        assert!(!m.contains("tarae-fake-token") && m.contains("not sent"), "{m}");
        // `A` isn't an answer: a key is opened once at a time (`Caps::storable`)
        point(&mut ed);
        answer(&mut ed, "A");
        assert!(ed.offers.iter().any(|o| o.title == crate::reads::POINTED_TITLE), "A refused on the card");
        answer(&mut ed, "n");
        // Kept for the project by an earlier build's `A`: the chat's copy read again — a new conversation sends it
        let store = ed.allow_path.clone().expect("a stand-in sandbox.json");
        let project = crate::sandbox::profile_path(&ed.session.root);
        crate::allow::keep(&store, &project, |a| a.add(&crate::allow::Grant::Key(env.clone()))).unwrap();
        crate::reads::kept_keys_load(&mut ed);
        let kept = |ed: &Editor| {
            ed.session.chat.as_ref().unwrap().kept_keys.as_ref().is_some_and(|k| k.contains(&env))
        };
        settle_until(&mut ed, kept);
        reset(&mut ed);
        let m = ask(&mut ed, "a new conversation");
        assert!(m.contains("API_KEY=tarae-fake-token"), "kept open: {m}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 7c′: what else sends to Claude — a note, the code review, the rebase's ask — goes while a card the
    /// user's pointing raised waits (it holds only the chat input's own message), keeps its work, and never
    /// carries what a `y` let go with the input's message, nor the input's chips; that message still gets
    /// them after. The token is a fake, the claude a logging stand-in.
    #[test]
    fn other_senders_go_while_a_pointing_card_waits_and_never_use_its_yes() {
        let (dir, env, plain, log, mut ed) = key_chat("key-senders");
        let pane = format!("pane#{}", ed.session.focus);
        let pointing_up = |ed: &Editor| ed.offers.iter().any(|o| o.title == crate::reads::POINTED_TITLE);
        let chips = |ed: &Editor| ed.session.chat.as_ref().unwrap().pointed.clone();
        // Each sender goes, and carries neither the key's text nor the input's chip
        let went = |ed: &mut Editor, what: &str, send: &dyn Fn(&mut Editor)| {
            let n = messages(&log).len();
            send(ed);
            assert!(ed.session.chat.as_ref().unwrap().busy(), "{what} went to claude, not held");
            turn_done(ed);
            let all = messages(&log);
            assert_eq!(all.len(), n + 1, "{what} went");
            let m = all.last().unwrap();
            assert!(!m.contains("tarae-fake-token") && !m.contains(&pane), "{what}: {m}");
        };
        point_at(&mut ed, &pane);
        ed.open(&plain).unwrap();
        went(&mut ed, "a note", &|ed| crate::notes::submit(ed, None, "about this line"));
        assert_eq!(ed.session.notes.list.len(), 1, "the note is kept");
        for n in &mut ed.session.notes.list {
            n.thread.iter_mut().for_each(|e| e.draft = true);
        }
        went(&mut ed, "the code review", &crate::codereview::send);
        assert!(ed.session.notes.list[0].thread.iter().all(|e| !e.draft), "the review went");
        went(&mut ed, "the rebase's ask", &|ed| crate::rebase::ask_for_test(ed, "resolve them"));
        assert!(ed.session.rebase.as_ref().unwrap().asked, "asked, and Claude was reached");
        assert!(pointing_up(&ed) && chips(&ed) == [pane.clone()], "the pointing and its chip stay");
        // `y`: the input's message carries it — not one sent before it
        ed.open(&env).unwrap();
        answer_card(&mut ed, "y");
        went(&mut ed, "the review after a y", &|ed| {
            ed.session.notes.list[0].thread.iter_mut().for_each(|e| e.draft = true);
            crate::codereview::send(ed);
        });
        ed.session.chat.as_mut().unwrap().input = "now".into();
        send(&mut ed);
        turn_done(&mut ed);
        let m = messages(&log).pop().unwrap();
        assert!(m.contains("API_KEY=tarae-fake-token") && m.contains(&pane), "the input's own: {m}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 7c′: a note or the review that opens the chat itself decides once the project's kept keys are read (a
    /// worker's): a file the user opened for the project (`A`) isn't asked about again — no card, its lines go.
    /// The token is a fake, the claude a logging stand-in.
    #[test]
    fn a_note_or_review_opening_the_chat_waits_for_the_kept_keys() {
        for review in [false, true] {
            let (dir, env, _, log, mut ed) = key_editor(if review { "kept-review" } else { "kept-note" });
            let store = ed.allow_path.clone().unwrap();
            let opened = crate::allow::Grant::Key(env.clone());
            crate::allow::keep(&store, &ed.session.root, |a| a.add(&opened)).unwrap();
            assert!(ed.session.chat.is_none(), "the chat isn't open yet");
            match review {
                false => crate::notes::submit(&mut ed, None, "what is this"),
                true => {
                    let id = crate::notes::add(&mut ed, &env, 0..1, crate::notes::By::You, "this line");
                    let n = ed.session.notes.list.iter_mut().find(|n| n.id == id).unwrap();
                    n.thread.iter_mut().for_each(|e| e.draft = true);
                    crate::codereview::send(&mut ed);
                }
            }
            let asked = |ed: &Editor| ed.offers.iter().any(|o| o.title == crate::reads::POINTED_TITLE);
            assert!(!asked(&ed), "not asked before the kept keys are read");
            settle_until(&mut ed, |ed| asked(ed) || messages(&log).len() == 1);
            assert!(!asked(&ed), "kept open for the project: not asked again (review {review})");
            let m = messages(&log).pop().unwrap();
            assert!(m.contains("API_KEY=tarae-fake-token"), "its lines went (review {review}): {m}");
            turn_done(&mut ed);
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    /// 7c′: the screen as text — `C-s`'s and Claude's `screen` tool's — blanks a pane showing a file named like
    /// a key (its marker in its place) and keeps the rest; once opened (`Grant::Key`) it shows. The token is
    /// a fake.
    #[test]
    fn the_screen_sent_blanks_a_key_named_file_s_pane() {
        let (dir, env, _, plain) = key_project("key-screen");
        let mut ed = editor("");
        ed.session.root = env.parent().unwrap().to_path_buf();
        let mut c = Chat::new();
        c.kept_keys = Some(Vec::new());
        ed.session.chat = Some(c);
        ed.open(&env).unwrap();
        ed.split_view(crate::split::Dir::Horizontal);
        ed.open(&plain).unwrap();
        let shots = |ed: &mut Editor| {
            crate::term::render(ed, &mut Vec::<u8>::new(), 140, 40).unwrap();
            attach_screen(ed).unwrap();
            let attached = ed.session.chat.as_mut().unwrap().screen.take().unwrap().text;
            [attached, crate::seeing::text(ed).unwrap()]
        };
        for s in shots(&mut ed) {
            assert!(!s.contains("tarae-fake-token") && s.contains("plain words"), "{s}");
            assert!(s.contains("[.env — named like a key — not sent]"), "{s}");
        }
        // A picker's preview card over it, no pane showing it: blanked too
        assert!(ed.close_view());
        ed.open(&plain).unwrap();
        let id = ed.docs.iter().find(|d| d.path.as_deref() == Some(env.as_path())).unwrap().id;
        let item = crate::picker::Item {
            label: ".env".into(),
            action: crate::picker::Action::Buffer(id),
            hint: String::new(),
            glyph: None,
        };
        ed.open_picker(crate::picker::Picker::new("buffers", vec![item], false), None);
        crate::term::render(&mut ed, &mut Vec::<u8>::new(), 140, 40).unwrap();
        let (f, w, h) = ed.last_frame.clone().unwrap();
        let drawn = crate::screen::text(&f, w, h).unwrap();
        assert!(drawn.contains("API_KEY=tarae-fake-token"), "(the preview shows it) {drawn}");
        for s in shots(&mut ed) {
            assert!(!s.contains("tarae-fake-token"), "the preview: {s}");
            assert!(s.contains("[.env — named like a key — not sent]"), "{s}");
        }
        // A search's hit row in it (`path:12: text`): that row blanked, its file named; another file's hit
        // stays — the one in hand, so the preview isn't what hides it
        let hit = |path: &Path, text: &str| crate::picker::Item {
            label: format!("{}:1: {text}", path.file_name().unwrap().to_string_lossy()),
            action: crate::picker::Action::Goto { path: path.to_path_buf(), line: 0, col: 0 },
            hint: String::new(),
            glyph: None,
        };
        let hits = vec![hit(&env, "API_KEY=tarae-fake-token"), hit(&plain, "plain words")];
        let root = ed.session.root.clone();
        ed.open_picker(crate::picker::Picker::new("search", hits, true).grouped(root), None);
        let on = |ed: &Editor| ed.picker.as_ref().and_then(|p| p.current()).map(|i| i.label.clone());
        for _ in 0..4 {
            if on(&ed).is_some_and(|l| l.starts_with("notes.txt")) {
                break;
            }
            ed.handle_key("down".parse().unwrap());
        }
        assert!(on(&ed).is_some_and(|l| l.starts_with("notes.txt")), "{:?}", on(&ed));
        crate::term::render(&mut ed, &mut Vec::<u8>::new(), 140, 40).unwrap();
        let (f, w, h) = ed.last_frame.clone().unwrap();
        let drawn = crate::screen::text(&f, w, h).unwrap();
        assert!(drawn.contains("API_KEY=tarae-fake-token"), "(its row shows it) {drawn}");
        for s in shots(&mut ed) {
            assert!(!s.contains("tarae-fake-token"), "the hit row: {s}");
            assert!(s.contains("[.env — named like a key — not sent]") && s.contains("plain words"), "{s}");
        }
        // Claude's `ui` tool: that row's path and line, the marker for its text (its id too); the other hit
        // as it is — and the user's own state (`tarae state`) unchanged
        let rows = |s: &crate::uistate::UiState| {
            let p = s.panels.picker.as_ref().expect("the picker's rows");
            p.rows.iter().map(|r| format!("{} | {}", r.id, r.label)).collect::<Vec<_>>().join("\n")
        };
        let claudes = ed.ui_state_claude();
        let (listed, text) = (rows(&claudes), claudes.text_claude(false));
        assert!(
            !listed.contains("tarae-fake-token") && !text.contains("tarae-fake-token"),
            "{listed}\n{text}"
        );
        assert!(
            listed.contains("picker:.env:1: (named like a key — not sent) | .env:1: (named like a key"),
            "{listed}"
        );
        assert!(listed.contains("plain words"), "{listed}");
        assert!(rows(&ed.ui_state()).contains("API_KEY=tarae-fake-token"), "the user's own state");
        // The side panel's list (`space q` — a search's hits put there): the same rule, the same marker
        ed.handle_key("esc".parse().unwrap());
        assert!(ed.picker.is_none());
        let item = |path: &Path, message: &str| crate::quickfix::Item {
            path: path.to_path_buf(),
            line: 0,
            col: 0,
            message: message.into(),
            severity: 0,
            mark: None,
        };
        ed.qlist_put("search", vec![item(&env, "API_KEY=tarae-fake-token"), item(&plain, "plain words")]);
        ed.sidebar_show(crate::sidebar::Mode::List);
        crate::term::render(&mut ed, &mut Vec::<u8>::new(), 140, 40).unwrap();
        let (f, w, h) = ed.last_frame.clone().unwrap();
        let drawn = crate::screen::text(&f, w, h).unwrap();
        assert!(drawn.contains("API_KEY=tarae-fake-token"), "(the list shows it) {drawn}");
        for s in shots(&mut ed) {
            assert!(!s.contains("tarae-fake-token"), "the list row: {s}");
            // (the panel too narrow for the file's name: the marker alone, under the file's heading)
            assert!(s.contains("[named like a key — not sent]") && s.contains("1: plain words"), "{s}");
        }
        let side = |s: &crate::uistate::UiState| {
            let (_, _, rows, _) = s.panels.side.as_ref().expect("the side panel's rows");
            rows.iter().map(|r| format!("{} | {}", r.id, r.label)).collect::<Vec<_>>().join("\n")
        };
        let claudes = ed.ui_state_claude();
        let (listed, text) = (side(&claudes), claudes.text_claude(false));
        assert!(
            !listed.contains("tarae-fake-token") && !text.contains("tarae-fake-token"),
            "{listed}\n{text}"
        );
        assert!(listed.contains("| .env:1: (named like a key — not sent)"), "{listed}");
        assert!(listed.contains("plain words"), "{listed}");
        assert!(side(&ed.ui_state()).contains("API_KEY=tarae-fake-token"), "the user's own state");
        ed.session.chat.as_mut().unwrap().allowed.keys.push(env.clone());
        for s in shots(&mut ed) {
            assert!(s.contains("API_KEY=tarae-fake-token") && !s.contains("not sent]"), "opened: {s}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 7c′: a note or the code review's comments on a file named like a key ask on the same card before its
    /// lines go — the user's own act, never sent nor left out silently: `n` sends the note with the file's
    /// name and the marker in place of its lines, `y` with them (that note only — the chat's next message
    /// still names the file only), and the review's the same. The note is kept while the card waits. The token
    /// is a fake, the claude a logging stand-in.
    #[test]
    fn a_note_or_review_on_a_key_named_file_asks_before_its_lines_go() {
        let (dir, env, _, log, mut ed) = key_chat("key-notes");
        let up = |ed: &Editor| ed.offers.iter().any(|o| o.title == crate::reads::POINTED_TITLE);
        let last = || messages(&log).pop().unwrap_or_default();
        let answered = |ed: &mut Editor, k: &str| {
            let n = messages(&log).len();
            answer_card(ed, k);
            turn_done(ed);
            assert_eq!(messages(&log).len(), n + 1, "{k}: it went");
            last()
        };
        // A note on its first line: asked, nothing sent yet, the note kept
        crate::notes::submit(&mut ed, None, "what is this");
        assert!(up(&ed) && messages(&log).is_empty(), "asked first");
        assert_eq!(ed.session.notes.list.len(), 1, "the note waits with the card");
        let m = answered(&mut ed, "n");
        assert!(!m.contains("tarae-fake-token") && m.contains("what is this"), "{m}");
        assert!(m.contains("<code>\\n(named like a key — not sent)\\n</code>"), "n: its name only: {m}");
        // A reply: `y` sends its lines, that note only
        let id = ed.session.notes.list[0].id;
        crate::notes::submit(&mut ed, Some(id), "and this?");
        assert!(up(&ed));
        let m = answered(&mut ed, "y");
        assert!(m.contains("<code>\\nAPI_KEY=tarae-fake-token\\n</code>") && m.contains("and this?"), "{m}");
        ed.session.chat.as_mut().unwrap().input = "after".into();
        send(&mut ed);
        turn_done(&mut ed);
        assert!(!last().contains("tarae-fake-token"), "y was for that note only");
        // The review's comment on it: `n` names it, `y` sends its lines — a key is opened once at a time
        // (`Caps::storable`): nothing opened for the conversation
        let draft = |ed: &mut Editor| ed.session.notes.list[0].thread.iter_mut().for_each(|e| e.draft = true);
        draft(&mut ed);
        crate::codereview::send(&mut ed);
        assert!(up(&ed), "the review asks");
        let m = answered(&mut ed, "n");
        assert!(!m.contains("tarae-fake-token") && m.contains("(named like a key — not sent)"), "{m}");
        draft(&mut ed);
        crate::codereview::send(&mut ed);
        let m = answered(&mut ed, "y");
        assert!(m.contains("API_KEY=tarae-fake-token"), "y: its lines go: {m}");
        assert!(!ed.session.chat.as_ref().unwrap().allowed.keys.contains(&env), "nothing opened");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// claude exits after each turn: the next question goes to a new process, which gets the file again.
    #[test]
    fn new_process_gets_the_file_again() {
        let dir = std::env::temp_dir().join(format!("tarae-chat-respawn-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("stdin.log");
        let result = serde_json::json!({"type": "result", "result": "ok"}).to_string();
        let script = format!(
            "read -r init; read -r line; printf '%s\\n' \"$line\" >> '{}'; printf '%s\\n' '{result}'",
            log.display()
        );
        let mut ed = editor("fn main() {}\n");
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), script];
        open(&mut ed);
        for q in ["first", "second"] {
            ed.session.chat.as_mut().unwrap().input = q.into();
            send(&mut ed);
            settle_until(&mut ed, |ed| {
                ed.session.chat.as_ref().is_some_and(|c| !c.busy() && c.proc.is_none())
            });
        }
        let sent = std::fs::read_to_string(&log).unwrap();
        assert_eq!(sent.lines().count(), 2, "{sent}");
        assert!(sent.lines().all(|l| l.contains("<file-content>")), "{sent}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A claude still starting doesn't read its stdin yet (a cold start is 8–10 s), and a pipe takes 64 KB
    /// before a write waits: a big message (the file, other unsaved buffers, a screen) goes out on the
    /// writer thread, the main loop going on — and what's written keeps its order.
    #[test]
    fn a_big_message_to_a_claude_still_starting_doesnt_hold_the_main_loop() {
        let dir = std::env::temp_dir().join(format!("tarae-chat-cold-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("stdin.log");
        // (a stand-in: reads nothing for two seconds, then everything into the log)
        let script = format!("sleep 2; cat > '{}'", log.display());
        let mut ed = editor("fn main() {}\n");
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), script];
        open(&mut ed);
        let question = format!("{}\nend of the question", "x".repeat(200 * 1024));
        ed.session.chat.as_mut().unwrap().input = question;
        let t = Instant::now();
        send(&mut ed);
        let took = t.elapsed();
        stop(&mut ed);
        eprintln!("chat write: send took {:.2} ms with a 200 KB message", took.as_secs_f64() * 1000.0);
        assert!(ed.session.chat.as_ref().unwrap().busy(), "sent");
        assert!(took < std::time::Duration::from_millis(500), "the main loop waited {took:?} on the pipe");
        // Everything arrives, in the order it was written: initialize, the message, the interrupt
        let end = Instant::now() + std::time::Duration::from_secs(10);
        let sent = loop {
            let s = std::fs::read_to_string(&log).unwrap_or_default();
            if s.lines().count() >= 3 || Instant::now() > end {
                break s;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        let lines: Vec<&str> = sent.lines().collect();
        assert_eq!(lines.len(), 3, "{}", &sent[..sent.len().min(300)]);
        assert!(lines[0].contains("initialize"), "{}", &lines[0][..lines[0].len().min(200)]);
        assert!(lines[1].contains("end of the question"));
        assert!(lines[2].contains("interrupt"), "{}", lines[2]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #102: a read outside the project beside another one asking (or one let through once) offers its
    /// folder for `a`/`A`, said on the card — and `a` lets the other through; a lone one offers the file.
    #[test]
    fn reads_beside_each_other_offer_their_folder() {
        let shots = crate::sandbox::profile_path(&std::env::temp_dir())
            .join(format!("tarae-presplit-shot-{}", std::process::id()));
        std::fs::create_dir_all(&shots).unwrap();
        for f in ["a.png", "b.png", "c.png"] {
            std::fs::write(shots.join(f), "marker").unwrap();
        }
        let mut ed = editor("");
        let root = ed.session.root.clone();
        ed.session.chat = Some(Chat::new());
        let generation = ed.session.chat.as_ref().unwrap().generation;
        let pending = |f: &str| crate::reads::Pending {
            request_id: f.into(),
            generation,
            input: serde_json::json!({"file_path": shots.join(f)}),
            look: crate::reads::look("Read", &serde_json::json!({"file_path": shots.join(f)}), &root),
            class: crate::cards::Class::Elsewhere,
            since: std::time::Instant::now(),
            found: Default::default(),
            repository: None,
            not_whole: None,
            own: None,
            then: None,
        };
        let card = |ed: &Editor| match ed.offers.first().map(|o| (&o.what, o.question.clone(), o.id)) {
            Some((crate::offer::What::Read { pending, folder, .. }, q, id)) => {
                (pending.look.at.clone(), folder.clone(), q, id)
            }
            other => panic!("{other:?}"),
        };
        // One alone: the file
        ed.session.chat.as_mut().unwrap().reads.push_back(pending("a.png"));
        crate::reads::next(&mut ed);
        let (at, folder, q, id) = card(&ed);
        assert_eq!((at, folder), (Some(shots.join("a.png")), None));
        assert!(!q.contains("everything in"), "{q}");
        // `y` once — then one beside it offers the folder, said plainly
        let p = pending("a.png");
        crate::reads::answer(&mut ed, id, p, Some(crate::allow::Scope::Once), true, None);
        ed.session.chat.as_mut().unwrap().reads.extend([pending("b.png"), pending("c.png")]);
        crate::reads::next(&mut ed);
        // (the folder's offered once it's been looked through for tokens — a worker's)
        let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while card(&ed).1.is_none() {
            let left = end.saturating_duration_since(std::time::Instant::now());
            let ev = ed.events.recv_timeout(left).expect("the folder offered before the timeout");
            ed.handle_event(ev);
        }
        let (at, folder, q, id) = card(&ed);
        assert_eq!((at, folder.clone()), (Some(shots.join("b.png")), Some(shots.clone())));
        assert!(q.contains(&format!("a, s and A allow everything in {}", shots.display())), "{q}");
        // `a`: the folder for this chat, and the one waiting beside it goes through without a card
        crate::reads::answer(
            &mut ed,
            id,
            pending("b.png"),
            Some(crate::allow::Scope::ThisChat),
            true,
            folder,
        );
        let c = ed.session.chat.as_ref().unwrap();
        assert_eq!(c.allowed.read, std::slice::from_ref(&shots));
        assert!(c.reads.is_empty() && ed.offers.is_empty(), "{:?}", ed.offers.first().map(|o| &o.title));
        std::fs::remove_dir_all(&shots).ok();
    }

    /// Piece J: a read in a repository outside the project offers that repository for `a`/`A` once it's been looked
    /// through and nothing there looks like a token — said on the card, with what `A` keeps it for: every checkout
    /// of the project's repository. `A` keeps it once, and the project's other checkout reads by it. A repository
    /// holding a token-like file isn't offered: the file alone.
    #[test]
    fn a_read_in_a_repository_offers_the_repository() {
        let s = crate::scratch::Scratch::new("chat-read-repo");
        let (main, wt) = crate::allow::tests::repo_with_worktree(s.path());
        let base = std::fs::canonicalize(s.path()).unwrap();
        let repo = |name: &str, token: bool| {
            let r = base.join(name);
            std::fs::create_dir_all(r.join("docs")).unwrap();
            std::fs::write(r.join("docs/README"), "marker").unwrap();
            if token {
                std::fs::write(r.join("docs/app.conf"), "password = tarae-fake-token\n").unwrap();
            }
            let out =
                std::process::Command::new("git").args(["init", "-q"]).current_dir(&r).output().unwrap();
            assert!(out.status.success());
            r
        };
        let (clean, tokened) = (repo("ref", false), repo("tokened", true));
        let store = base.join("state/sandbox.json");
        let mut ed = editor("");
        (ed.session.root, ed.allow_path) = (main.clone(), Some(store.clone()));
        ed.session.chat = Some(Chat::new());
        let generation = ed.session.chat.as_ref().unwrap().generation;
        let until = |ed: &mut Editor, what: &str, done: &dyn Fn(&Editor) -> bool| {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
            while !done(ed) {
                let left = end.saturating_duration_since(std::time::Instant::now());
                let ev = ed.events.recv_timeout(left).unwrap_or_else(|| panic!("timed out: {what}"));
                ed.handle_event(ev);
            }
        };
        ed.caps_sync();
        until(&mut ed, "the session's caps", &|ed| {
            ed.session.caps().is_some_and(|c| c.here.repository.is_some())
        });
        let pending =
            |tool: &str, input: serde_json::Value, repository: Option<&Path>| crate::reads::Pending {
                request_id: "r".into(),
                generation,
                look: crate::reads::look(tool, &input, &main),
                input,
                class: crate::cards::Class::Elsewhere,
                since: std::time::Instant::now(),
                found: Default::default(),
                repository: repository.map(Path::to_path_buf),
                not_whole: None,
                own: None,
                then: None,
            };
        let read =
            |r: &Path| pending("Read", serde_json::json!({"file_path": r.join("docs/README")}), Some(r));
        let card = |ed: &Editor| match ed.offers.first().map(|o| (&o.what, o.question.clone(), o.id)) {
            Some((crate::offer::What::Read { folder, .. }, q, id)) => (folder.clone(), q, id),
            other => panic!("{other:?}"),
        };
        // Claude's Read, through the request's worker: the repository found there (review J 16)
        let input =
            serde_json::json!({"tool_name": "Read", "input": {"file_path": clean.join("docs/README")}});
        assert!(crate::reads::request(&mut ed, "r1", &input).is_none(), "it waits for the card");
        until(&mut ed, "the repository offered", &|ed| !ed.offers.is_empty() && card(ed).0.is_some());
        let asked = match &ed.offers[0].what {
            crate::offer::What::Read { pending, .. } => (**pending).clone(),
            other => panic!("{other:?}"),
        };
        assert_eq!(asked.repository.as_ref(), Some(&clean));
        let (folder, q, id) = card(&ed);
        assert_eq!(folder.as_ref(), Some(&clean));
        let whole = format!("a, s and A allow everything in {} — the repository it's in", clean.display());
        assert!(q.contains(&whole) && q.contains("A is for every checkout of main."), "{q}");
        crate::reads::answer(&mut ed, id, asked, Some(crate::allow::Scope::Always), true, folder);
        let toast = ed.toasts.last().map(|t| t.text.clone()).unwrap_or_default();
        assert!(toast.ends_with(" for every checkout of main — :granted lists it"), "{toast}");
        until(&mut ed, "kept", &|_| {
            crate::allow::Store::read(Some(&store)).get(&main).read == [clean.clone()]
        });
        assert_eq!(
            crate::allow::Store::read(Some(&store)).get(&wt).read,
            std::slice::from_ref(&clean),
            "one answer, both"
        );
        // A token-like file in it: the file alone, once the repository's been looked through
        ed.session.chat.as_mut().unwrap().reads.push_back(read(&tokened));
        crate::reads::next(&mut ed);
        let looked = |ed: &Editor| {
            ed.session.chat.as_ref().unwrap().scanned.get(&tokened).is_some_and(|f| f.is_some())
        };
        until(&mut ed, "the repository looked through", &looked);
        let (folder, q, id) = card(&ed);
        assert_eq!(folder, None, "{q}");
        assert!(!q.contains("everything in"), "{q}");
        // …and says why it isn't offered whole (frictions29 11: a reference repository came a card per path)
        let why = format!(
            "Not offered whole, the repository {}: a file in it looks like it holds a token.",
            crate::allow::tilde(&tokened)
        );
        assert!(q.contains(&why), "{q}");
        crate::reads::answer(&mut ed, id, read(&tokened), None, true, None);
        // Each of these offers the file alone, never the repository (review J 5, 15): one that holds a file named
        // like a key, a read of a file named like a key, a repository too big to look through all of it
        let keyed = repo("keyed", false);
        std::fs::create_dir_all(keyed.join("certs")).unwrap();
        std::fs::write(keyed.join("certs/server.key"), "-----BEGIN TARAE FAKE KEY-----\n").unwrap();
        let dotenv = repo("dotenv", false);
        std::fs::write(dotenv.join(".env"), "MODE=dev\n").unwrap();
        let big = repo("big", false);
        let cut = crate::tokens::Found { cut: true, ..Default::default() };
        let scanned = &mut ed.session.chat.as_mut().unwrap().scanned;
        scanned.insert(big.clone(), Some(cut));
        // (as if its walk had found nothing: the read itself named like a key is what keeps it the file's)
        scanned.insert(dotenv.clone(), Some(Default::default()));
        let env_read = pending("Read", serde_json::json!({"file_path": dotenv.join(".env")}), Some(&dotenv));
        for (p, r) in [(read(&keyed), &keyed), (env_read, &dotenv), (read(&big), &big)] {
            ed.session.chat.as_mut().unwrap().reads.push_back(p.clone());
            crate::reads::next(&mut ed);
            let looked = |ed: &Editor| {
                let c = ed.session.chat.as_ref().unwrap();
                c.scanned.get(r).is_some_and(|f| f.is_some())
            };
            until(&mut ed, "looked through", &looked);
            let (folder, q, id) = card(&ed);
            assert_eq!(folder, None, "{}: {q}", r.display());
            crate::reads::answer(&mut ed, id, p, None, true, None);
        }
        // A folder holding the checkouts: this checkout's own — said on the card and the toast (review J 13)
        let holding = pending("Grep", serde_json::json!({"pattern": "x", "path": base}), None);
        ed.session.chat.as_mut().unwrap().reads.push_back(holding.clone());
        crate::reads::next(&mut ed);
        let (_, q, id) = card(&ed);
        assert!(q.ends_with("A is for this checkout only."), "{q}");
        crate::reads::answer(&mut ed, id, holding, Some(crate::allow::Scope::Always), true, None);
        let toast = ed.toasts.last().map(|t| t.text.clone()).unwrap_or_default();
        assert!(toast.ends_with(" for this checkout — :granted lists it"), "{toast}");
        until(&mut ed, "kept", &|_| crate::allow::Store::read(Some(&store)).get(&main).reads(&base));
        let there = crate::allow::Store::read(Some(&store)).get(&wt);
        assert!(!there.reads(&base) && there.reads(&clean), "{there:?}");
    }

    /// #102: Claude's reads ask one after another — one beside the read on the card, come after the card went
    /// up, makes that card offer their folder (said on it; settled again before `a` counts).
    #[test]
    fn a_read_come_beside_the_card_widens_it() {
        let shots = crate::sandbox::profile_path(&std::env::temp_dir())
            .join(format!("tarae-late-sibling-{}", std::process::id()));
        std::fs::create_dir_all(&shots).unwrap();
        for f in ["x.png", "y.png"] {
            std::fs::write(shots.join(f), "marker").unwrap();
        }
        let mut ed = editor("");
        ed.session.chat = Some(Chat::new());
        let ask = |ed: &mut Editor, id: &str, f: &str| {
            let req = serde_json::json!({"tool_name": "Read", "input": {"file_path": shots.join(f)}});
            crate::reads::request(ed, id, &req);
        };
        let folder = |ed: &Editor| match ed.offers.first().map(|o| (&o.what, o.rev)) {
            Some((crate::offer::What::Read { folder, .. }, rev)) => Some((folder.clone(), rev)),
            _ => None,
        };
        let settle = |ed: &mut Editor, done: &dyn Fn(&Editor) -> bool| {
            let end = Instant::now() + std::time::Duration::from_secs(5);
            while !done(ed) {
                assert!(Instant::now() < end, "timed out");
                if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
                    ed.handle_event(ev);
                }
            }
        };
        ask(&mut ed, "r1", "x.png");
        settle(&mut ed, &|ed| folder(ed).is_some());
        let (f, rev) = folder(&ed).unwrap();
        assert_eq!(f, None, "alone: the file");
        ask(&mut ed, "r2", "y.png");
        settle(&mut ed, &|ed| folder(ed).is_some_and(|(f, _)| f.is_some()));
        let (f, rev2) = folder(&ed).unwrap();
        assert_eq!(f.as_deref(), Some(shots.as_path()));
        assert!(rev2 > rev, "settles again before `a` counts");
        let q = &ed.offers[0].question;
        assert!(q.contains(&format!("a, s and A allow everything in {}", shots.display())), "{q}");
        std::fs::remove_dir_all(&shots).ok();
    }

    #[test]
    fn last_code_block() {
        let mut c = Chat::new();
        c.msgs.push(Msg::new(Role::Assistant, "a\n```rust\nfn a() {}\n```\nand\n```\nb\n```\n", ""));
        assert_eq!(c.last_code().as_deref(), Some("b\n"));
        c.msgs.push(Msg::new(Role::User, "q", ""));
        assert_eq!(c.last_code().as_deref(), Some("b\n"), "skips user messages");
    }

    #[test]
    fn refs_in_answers() {
        let text = "See `src/lib.rs:12` and main.rs:3-5 — 파일 /abs/p.rs:7. Not 12:30 or http://x.com:80.";
        let root = Path::new("/proj");
        let got: Vec<(String, usize)> =
            refs_in(root, text).into_iter().map(|(r, l)| (text[r].to_string(), l.line)).collect();
        assert_eq!(got, [("src/lib.rs:12".into(), 11), ("main.rs:3".into(), 2), ("/abs/p.rs:7".into(), 6)]);
        assert_eq!(refs_in(root, "/abs/p.rs:7")[0].1.path, Path::new("/abs/p.rs"));
        assert_eq!(refs_in(root, "src/lib.rs:12")[0].1.path, root.join("src/lib.rs"), "from the root");
        assert!(refs_in(root, "no refs: 1, v1.2:3").is_empty());
    }

    /// A row's knot is `▲` for what changes files (`Look::changes`, `term::knot`): an Edit, a Write, tarae's
    /// edit, rename, replace — not a replace's preview, nor a read, a search or a run. Read off the rows the
    /// calls really make, so a preview worded otherwise can't pass for a change.
    #[test]
    fn the_rows_that_change_files_are_the_changes() {
        use serde_json::json;
        let root = Path::new("/proj");
        let changes = |name: &str, input: serde_json::Value| look(root, name, &input).changes();
        let replace = json!({"pattern": "a", "replacement": "b", "path": "src"});
        let preview = json!({"pattern": "a", "replacement": "b", "path": "src", "preview": true});
        assert!(changes("Edit", json!({"file_path": "a.rs", "old_string": "x", "new_string": "y"})));
        assert!(changes("Write", json!({"file_path": "a.rs", "content": "y"})));
        assert!(changes("mcp__tarae__edit", json!({"edits": [{"file_path": "a.rs"}]})));
        assert!(changes("mcp__tarae__rename", json!({"path": "a.rs", "symbol": "x", "new_name": "y"})));
        assert!(changes("mcp__tarae__replace", replace));
        assert!(!changes("mcp__tarae__replace", preview), "a preview changes nothing");
        assert!(!changes("mcp__tarae__read", json!({"files": ["a.rs"]})));
        assert!(!changes("mcp__tarae__search", json!({"pattern": "x"})));
        assert!(!changes("mcp__tarae__run", json!({"command": "cargo test"})));
    }

    /// A change claims the waiting call of its tool made with its very input (`claim_call`): each call once —
    /// two made alike go to two changes, a third finds none; another tool's or another input's, none — a native
    /// Edit's request with a default its call lacked too.
    #[test]
    fn a_change_claims_the_call_made_with_its_input_once() {
        use serde_json::json;
        let mut ed = editor("");
        let mut c = Chat::new();
        let edit = json!({"edits": [{"file_path": "a.rs", "old_string": "x", "new_string": "y"}]});
        for id in ["t1", "t2"] {
            c.calls.push(Called { id: id.into(), name: "mcp__tarae__edit".into(), input: edit.clone() });
        }
        c.calls.push(Called { id: "r1".into(), name: "mcp__tarae__rename".into(), input: edit.clone() });
        ed.session.chat = Some(c);
        let other = json!({"edits": []});
        assert_eq!(claim_call(&mut ed, "mcp__tarae__edit", &other), None, "another input");
        assert_eq!(claim_call(&mut ed, "mcp__tarae__edit", &edit).as_deref(), Some("t1"));
        assert_eq!(claim_call(&mut ed, "mcp__tarae__edit", &edit).as_deref(), Some("t2"), "t1 is taken");
        assert_eq!(claim_call(&mut ed, "mcp__tarae__edit", &edit), None, "both taken; not the rename's");
        // (a native Edit's request with a default its call didn't have: none — no stamp, never another's)
        let call = json!({"file_path": "a.rs", "old_string": "x", "new_string": "y"});
        let c = ed.session.chat.as_mut().unwrap();
        c.calls.push(Called { id: "e1".into(), name: "Edit".into(), input: call.clone() });
        let asked = json!({"file_path": "a.rs", "old_string": "x", "new_string": "y", "replace_all": false});
        assert_eq!(claim_call(&mut ed, "Edit", &asked), None, "not byte for byte: none");
        assert_eq!(claim_call(&mut ed, "Edit", &call).as_deref(), Some("e1"));
    }

    /// A command's row: the command in a line, ended by how it ended once its result comes.
    #[test]
    fn a_command_row_ends_with_how_it_ended() {
        let root = Path::new("/proj");
        let r = look(root, "mcp__tarae__run", &serde_json::json!({"command": "cargo  test\n  -p tarae"}));
        assert_eq!(
            (r.verb, r.what.as_str(), r.pattern, r.link.is_none()),
            ("run", "cargo test -p tarae", true, true)
        );
        let long = look(root, "mcp__tarae__run", &serde_json::json!({"command": "x".repeat(60)}));
        assert_eq!(long.what.chars().count(), 48);
        let mut ed = editor("");
        let mut c = Chat::new();
        c.msgs.push(Msg::tool(r, "t1"));
        let generation = c.generation;
        ed.session.chat = Some(c);
        let text = "exit 101 in 2.0s\n$ cargo test -p tarae\n".to_string();
        on_event(&mut ed, generation, StreamEv::ToolResult { id: "t1".into(), text, error: false });
        let c = ed.session.chat.as_ref().unwrap();
        assert_eq!(c.msgs[0].look.as_ref().unwrap().range, "exit 101");
    }

    /// A request's usage as the stream says it, for message `id`.
    fn seen(id: &str) -> StreamEv {
        let counts =
            crate::usage::Counts { input: Some(10), write: Some(500), read: Some(20_000), output: Some(90) };
        StreamEv::Usage(crate::usage::Seen {
            id: Some(id.into()),
            model: "claude-opus-5-5".into(),
            counts,
            ttl: None,
            at_ms: 0,
        })
    }

    /// A turn that ends while its session is behind still reaches the call-order record: its events applied
    /// in the session they're tagged with, the line written on a worker, nothing left waiting in any
    /// session's usage.
    #[test]
    fn a_turn_ending_in_a_session_behind_reaches_the_call_order_record() {
        let dir = crate::scratch::Scratch::new("calls-behind");
        let mut ed = editor("");
        ed.calls_dir = Some(dir.path().to_path_buf());
        let behind = ed.session_add(ed.session.root.clone());
        let mut c = Chat::new();
        c.state = State::Thinking;
        let generation = c.generation;
        ed.sessions.iter_mut().find(|l| l.id == behind).unwrap().chat = Some(c);
        // (as its pump sends them: tagged with its session, applied with it swapped in)
        let send = |ed: &mut Editor, ev: StreamEv| {
            ed.handle_event(crate::event::Event::For(
                behind,
                Box::new(move |ed| on_event(ed, generation, ev)),
            ))
        };
        send(&mut ed, seen("m1"));
        let input = serde_json::json!({ "pattern": "x" });
        send(&mut ed, StreamEv::Tool { id: "t1".into(), name: "mcp__tarae__search".into(), input });
        send(&mut ed, StreamEv::ToolResult { id: "t1".into(), text: String::new(), error: false });
        let spent = crate::usage::Spent { at_ms: crate::usage::now_ms(), ..Default::default() };
        send(&mut ed, StreamEv::Spent(spent));
        assert_ne!(ed.session.id, behind, "it stayed behind");
        assert!(
            ed.sessions.iter().all(|l| l.usage.to_record.is_empty()) && ed.session.usage.to_record.is_empty()
        );
        let lines = crate::calls::recorded(dir.path(), 1);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["steps"][0]["tools"], serde_json::json!(["search"]), "{lines:?}");
    }

    /// A turn the user leaves mid-way — a new chat (`C-l`, `:chat-new`) or the chat closed — is recorded as
    /// cut off, with the calls it made; what it left doesn't join the next turn's.
    #[test]
    fn a_turn_left_by_a_new_chat_or_a_close_is_recorded_cut_off() {
        let dir = crate::scratch::Scratch::new("calls-left");
        let mut ed = editor("");
        ed.calls_dir = Some(dir.path().to_path_buf());
        // (a stand-in for the new chat's claude, which reads and says nothing)
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        let thinking = |ed: &mut Editor| {
            let c = ed.session.chat.get_or_insert_with(Chat::new);
            c.state = State::Thinking;
            c.generation
        };
        let called = |ed: &mut Editor, generation: u64, id: &str, tool: &str| {
            on_event(ed, generation, seen(id));
            let input = serde_json::json!({ "file_path": "a.rs" });
            on_event(ed, generation, StreamEv::Tool { id: format!("{id}-t"), name: tool.into(), input });
        };
        let g = thinking(&mut ed);
        called(&mut ed, g, "m1", "mcp__tarae__search");
        reset(&mut ed);
        let g = thinking(&mut ed);
        called(&mut ed, g, "m2", "mcp__tarae__read");
        close(&mut ed);
        assert!(ed.session.chat.is_none());
        let g = thinking(&mut ed);
        called(&mut ed, g, "m3", "mcp__tarae__hover");
        on_event(&mut ed, g, StreamEv::Spent(crate::usage::Spent { at_ms: 1, ..Default::default() }));
        let lines = crate::calls::recorded(dir.path(), 3);
        let mut steps: Vec<String> = lines
            .iter()
            .map(|l| l["steps"].as_array().unwrap().iter().map(|s| s["tools"].to_string()).collect())
            .collect();
        steps.sort();
        assert_eq!(steps, [r#"["hover"]"#, r#"["read"]"#, r#"["search"]"#], "each turn its own: {lines:?}");
    }

    /// A turn's calls counted from the stream as the chat reads it: each `tool_use` and its result, a range
    /// read twice, a run that only reads, the time a card of Claude's waits for the user — kept with the
    /// turn when its result comes, and said in `:usage`.
    #[test]
    fn a_turns_tool_calls_are_counted_from_the_stream() {
        let mut ed = editor("");
        let mut c = Chat::new();
        c.state = State::Thinking;
        let generation = c.generation;
        ed.session.chat = Some(c);
        let root = ed.session.root.clone();
        let read = serde_json::json!({ "file_path": root.join("src/a.rs"), "offset": 1, "limit": 40 });
        let calls = [
            ("t1", "Read", read.clone()),
            ("t2", "Read", read),
            ("t3", "mcp__tarae__run", serde_json::json!({ "command": "grep -n x src/a.rs | head" })),
        ];
        for (id, name, input) in calls {
            on_event(&mut ed, generation, StreamEv::Tool { id: id.into(), name: name.into(), input });
            on_event(
                &mut ed,
                generation,
                StreamEv::ToolResult { id: id.into(), text: String::new(), error: false },
            );
        }
        // A card of Claude's up while the turn is under way: the user is waited for, until it goes
        ed.push_offer(crate::offer::Offer::new(
            "Claude changes 3 files".into(),
            "q",
            "",
            String::new(),
            crate::offer::What::ChangeSet { whose: crate::refactor::Whose::Claude(generation), ops: false },
        ));
        ed.sessions_sync();
        assert!(ed.session.usage.tally.is_waiting());
        ed.offers.clear();
        ed.sessions_sync();
        assert!(!ed.session.usage.tally.is_waiting());
        let spent =
            crate::usage::Spent { ms: Some(9_000), at_ms: crate::usage::now_ms(), ..Default::default() };
        on_event(&mut ed, generation, StreamEv::Spent(spent));
        let turn = ed.session.usage.last_turn().unwrap();
        let calls = turn.calls.as_ref().expect("counted");
        assert_eq!((calls.sums.calls, calls.sums.reread, calls.sums.read_only), (3, 1, 1));
        assert_eq!(calls.programs.get("grep"), Some(&1));
        let report = crate::usage::report("s", &ed.session.usage, &ed.limits, &[], crate::usage::now_ms());
        assert!(report.contains("## Tool calls"), "{report}");
        assert!(report.contains("By tool: Read 2 · run 1"), "{report}");
        assert!(
            report.contains("Read-only runs by program (each a process tarae's reads could spare): grep 1")
        );
    }

    #[test]
    fn tool_calls_become_rows() {
        let cwd = Path::new("/proj");
        let file = cwd.join("src/lib.rs");
        let r = look(cwd, "Read", &serde_json::json!({"file_path": file, "offset": 10, "limit": 40}));
        assert_eq!((r.verb, r.what.as_str(), r.range.as_str()), ("read", "src/lib.rs", "L10–49"));
        assert_eq!(r.link, Some(Link { path: file.clone(), line: 9 }));
        // (a relative path is from the root)
        let rel = look(cwd, "Read", &serde_json::json!({"file_path": "src/lib.rs"}));
        assert_eq!(rel.link, Some(Link { path: file, line: 0 }));
        let g = look(cwd, "Grep", &serde_json::json!({"pattern": "fn add", "path": cwd.join("src")}));
        assert_eq!(
            (g.verb, g.what.as_str(), g.range.as_str(), g.pattern),
            ("grep", "fn add", "in src", true)
        );
        let f = look(cwd, "Glob", &serde_json::json!({"pattern": "**/*.rs"}));
        assert_eq!((f.verb, f.range.as_str(), f.link), ("glob", "", None));
        // Each named as the tool it is: tarae's search over the project, a definition by name
        let s = look(cwd, "mcp__tarae__search", &serde_json::json!({"pattern": "fn add"}));
        assert_eq!((label(s.verb), label(g.verb)), ("search (project)", "grep"));
        let d = look(cwd, "mcp__tarae__definition", &serde_json::json!({"symbol": "Session"}));
        assert_eq!((d.verb, d.what.as_str()), ("definition", "Session"));
        let r =
            look(cwd, "mcp__tarae__references", &serde_json::json!({"path": "src/lib.rs", "symbol": "a"}));
        assert_eq!(r.verb, "references");
        // tarae's edit: its first file, and how many more
        let e = look(
            cwd,
            "mcp__tarae__edit",
            &serde_json::json!({"edits": [{"file_path": "src/a.rs"}, {"file_path": "src/a.rs"},
                {"file_path": "src/b.rs"}]}),
        );
        assert_eq!((e.verb, e.what.as_str(), e.range.as_str()), ("edit", "src/a.rs", "+1 more"));
    }

    /// Each `llm.edit-tools` has the prompt and tools it says: tarae's `edit` alone (the default — every change,
    /// new files included), both, or Claude Code's Edit and Write — the prompt never sends Claude to a tool it
    /// hasn't, and each one's words are really in (a `replace` that matched nothing would leave the native ones).
    #[test]
    fn each_edit_arm_says_its_own_tools() {
        use crate::edittool::Arm;
        let (native, both, tarae) = (system_for(Arm::Native), system_for(Arm::Both), system_for(Arm::Tarae));
        assert_eq!(tarae, system(), "the default: tarae's edit alone");
        assert!(native.contains(EDITS_NATIVE) && !native.contains("with edit"));
        assert!(both.contains(EDITS_BOTH) && both.contains("change those with Edit)"));
        assert!(tarae.contains(EDITS_TARAE), "{tarae}");
        assert!(tarae.contains("every change, new files included"), "{tarae}");
        assert!(!tarae.contains("Edit and Write") && !tarae.contains("with Edit"), "{tarae}");
        assert!(tarae.contains("change those with edit)"));
        // (no arm sends Claude to a Read before it edits: the CLI doesn't ask one of this model — it only cost Reads)
        for (arm, s) in [("native", &native), ("both", &both), ("tarae", &tarae)] {
            assert!(!s.contains("Edit needs") && !s.contains("Read first"), "{arm}: {s}");
        }
        let read = crate::readtool::schema()["description"].as_str().unwrap().to_string();
        assert!(!read.contains("Before an Edit") && !read.contains("own read"), "{read}");
        assert_eq!(
            (tools(Arm::Native), tools(Arm::Both), tools(Arm::Tarae)),
            ("Edit,Write", "Edit,Write", "")
        );
        assert!(ALLOWED.split(',').any(|t| t == "mcp__tarae__edit"));
    }

    /// The system prompt sends "where is X defined" to the language server by name (never a grep for `fn X`
    /// / `struct X`), a type to hover, uses to references — then tarae's search and read; shell pipelines for
    /// what those can't do. Grep and Glob aren't the chat's tools, and the prompt doesn't name them.
    #[test]
    fn the_prompt_prefers_the_language_server() {
        let s = system();
        let at = |w: &str| s.find(w).unwrap_or_else(|| panic!("{w:?} in the prompt"));
        assert!(
            s.contains("\"where is X defined\" is definition by name (symbol X, no file or line needed)")
        );
        assert!(s.contains("never a grep for `fn X`, `struct X` or `class X`"));
        assert!(at("definition by name") < at(", hover;") && at(", hover;") < at("references."));
        assert!(at("references.") < at("for lines of files, read"));
        assert!(at("for lines of files, read") < at("shell pipelines are for what those can't do"));
        assert!(!s.contains("Grep") && !s.contains("Glob"), "a tool the chat hasn't");
        assert!(!tools(crate::edittool::Arm::Native).split(',').any(|t| t == "Grep" || t == "Glob"));
    }

    /// A walk-through's stops as Claude writes them: parsed from its words, shown as link lines, a tag
    /// still being written hidden until it's whole.
    #[test]
    fn show_tags_in_the_answer() {
        let t = "First:\n<show at=\"src/filetree.rs:78-149\">builds the tree</show>\nThen <show at='src/a.rs:3'>x</show> and <show at=\"src/b";
        let tags = show_tags(t);
        assert_eq!(
            tags.iter().map(|(_, at, why)| (at.as_str(), why.as_str())).collect::<Vec<_>>(),
            [("src/filetree.rs:78-149", "builds the tree"), ("src/a.rs:3", "x")]
        );
        assert_eq!(
            displayed(t),
            "First:\n▸ src/filetree.rs:78-149 · builds the tree\nThen ▸ src/a.rs:3 · x and "
        );
        assert_eq!(displayed("no tags"), "no tags");
    }

    /// tarae's `read` as a row: its first item as the tool reads it (`readtool::item`) — the editor goes to its
    /// first range's start (a file's top for an end or a symbol) — and how many more it asks for.
    #[test]
    fn a_read_row_names_its_first_item() {
        let cwd = Path::new("/proj");
        let read =
            |files: serde_json::Value| look(cwd, "mcp__tarae__read", &serde_json::json!({ "files": files }));
        let r = read(serde_json::json!(["src/lib.rs:10-40,90", "b.rs"]));
        assert_eq!((r.verb, r.what.as_str(), r.range.as_str()), ("read", "src/lib.rs:10-40,90", "+1 more"));
        assert_eq!(r.link, Some(Link { path: cwd.join("src/lib.rs"), line: 9 }));
        for (spec, what) in
            [("src/lib.rs:-30", "src/lib.rs:-30"), ("src/lib.rs:Editor::open", "src/lib.rs:Editor::open")]
        {
            let r = read(serde_json::json!([spec]));
            assert_eq!((r.what.as_str(), r.range.as_str()), (what, ""));
            assert_eq!(r.link, Some(Link { path: cwd.join("src/lib.rs"), line: 0 }), "{spec}");
        }
        let r = read(serde_json::json!(["notes:2026/x.md"]));
        assert_eq!(
            r.link.map(|l| l.path),
            Some(cwd.join("notes:2026/x.md")),
            "a `:` in the path is the path's"
        );
    }
}
