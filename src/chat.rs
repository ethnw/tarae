//! Chat panel — talk to Claude in the right-hand panel (M4).
//!
//! - One process lives for the whole conversation (stream-json input takes many turns — remembers context).
//!   Spawned the moment the panel opens (prewarm), so even the first question skips the cold start.
//! - Each send attaches the current file·cursor·selection·diagnostics as `<editor-context>`. The full file
//!   only the first time that version is sent (not resent every time), and only around the cursor if large.
//! - Answers are drawn as they stream; stop (`C-c`) is an interrupt control request — process, memory stay.
//! - A code block in the answer: `C-r` applies it to the selection (via the review diff), `C-y` copies it.
//! - Claude explores the project itself with read-only tools (Read·Grep·Glob — it can't write). Each call
//!   is one dim row (`◦ read  src/lib.rs  L10–49`); it and every `path:line` in an answer is a link —
//!   click it, or `C-]` (`C-g`) walks them newest first. Open files are listed in the context; unsaved ones carry
//!   their text (disk is stale for them).
//! - It reads your CLAUDE.md files and Claude Code settings like `claude` (`llm.chat-settings`) — with hooks off
//!   and an `ask` rule so edits always come here first (measured: a project `allow` or hooks would otherwise apply).
//! - Its thinking comes as summaries (`--thinking-display summarized`) — a quiet italic block here, and
//!   beside the code in follow mode (`C-f`, `follow.rs`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::process::ChildStdin;
use std::time::Instant;

use crate::document::DocId;
use crate::editor::Editor;
use crate::key::{Code, Key};
use crate::llm::{self, StreamEv};
use crate::markdown::Span;

const SYSTEM: &str = "You are Claude, built into tarae — a terminal code editor, working in the user's project \
(your current directory). Explore it with Read, Glob and tarae's own code tools — look at the code you need \
across files instead of guessing. The language server's answers beat any text search and reading around — \
reach for them first: \"where is X defined\" is definition by name (symbol X, no file or line needed) — never a \
grep for `fn X`, `struct X` or `class X`; for a trait's implementations, definition with kind implementation; \
for a value's type or a function's signature and docs, hover; for where something is used or what a change \
would touch, references. For finding code or text, search (the editor's project search); for a file's outline \
or which function a line is in, symbols — Grep is for what those can't do. You can change files with Edit and Write when the user asks for changes: each one is shown to them as \
a diff they accept or reject (maybe in part), so keep edits focused, and if one is declined, ask before trying \
another way. After you change code, check your work: diagnostics with wait says in seconds whether it \
compiles as the language servers see it (before any build), then the run_tests tool runs the tests that cover \
the change (a test file, and a line for a single test) — when they fail, read why and fix it; the user \
watches the run in their editor. To build, lint, format, run a whole test suite or use git, use the run \
tool: one command per call, its output comes back; checks start at once, other commands wait for the user's \
yes. Both run in a sandbox: they can write only in the project (not .git or .tarae.toml), temp folders and the \
toolchain's own, and connect to nothing over the network, this machine included, but the ports the user \
opened (each result says which) — when it blocks something a command needs, tell the user (the settings \
llm.sandbox-ports, llm.sandbox-localhost and llm.sandbox-network open more); never work around it. Tests of dangerous behavior (deleting, overwriting, killing \
processes, the network) use a harmless stand-in — a marker file made in the test's own temp dir, then checked — \
never the real command or a real path outside that dir, not even in code meant never to run. When the work takes several steps, lay them out first with the plan tool \
and keep it current as you go (the step you're on doing, finished ones done). Each user message starts with <editor-context>: the file the user is looking at, \
cursor line, the lines on screen (and any other file shown beside it), selection, diagnostics and the other \
open files — \"this\" and \"here\" usually mean the selection, the cursor or what's on screen. When what they \
mean is on their screen but not in the context (a panel, a card, a message, the terminal panel's output), the \
screen tool shows you their screen as text — they're asked first, and it only looks. For the processes the \
editor runs (language servers, your chat, commands, tests, the terminal — pid, parent, age, CPU, memory, \
command line, folder), use the procs tool, not ps, pgrep or lsof through run. Files marked unsaved differ from disk — trust the \
text given in the context over what Read returns. When you point at code, cite it as path:line (relative to the \
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
search's), one call and one review for the user, instead of many Edits; replace with preview first to see what \
it would touch.";

/// The system prompt: `SYSTEM`, the refactoring tools, and where tarae's own log is (#45).
fn system() -> String {
    let mut s = format!("{SYSTEM}{SYSTEM_REFACTOR}");
    if let Some(log) = crate::log::path() {
        s.push_str(&format!(
            " tarae's own log, {}, says what the language servers are doing (starting, indexing, ready, \
             exited, what they printed) and every message the editor showed the user — the place to look when \
             a server seems slow or something failed; Read or grep it without asking.",
            log.display()
        ));
    }
    s
}

/// The tools the chat process gets. Reading and tarae's own (`note`, `run_tests`, `run` — `tools.rs`) are
/// allowed up front: the tests are the project's own, run the way the user runs them, in view, and any code
/// Claude wrote for them was reviewed; `run` asks the user itself for anything but the checks. Edit and Write
/// come to tarae as permission requests and show as a review first (`edits.rs`).
const TOOLS: &str = "Read,Grep,Glob,Edit,Write";
/// Not Read, Grep or Glob: allowed by name they read anywhere unasked (measured — `reads.rs` answers them).
/// (`screen` asks the user itself — `seeing.rs`)
/// (`references`, `search`, `symbols`, `definition`, `hover` and `diagnostics` read inside the project
/// only — `nav.rs`; `rename` and `replace` land as Claude's edits do, in the review — `refactor.rs`; `procs`
/// reads what `:procs` shows — `procs.rs`)
const ALLOWED: &str = "mcp__tarae__note,mcp__tarae__run_tests,mcp__tarae__run,mcp__tarae__plan,\
mcp__tarae__screen,mcp__tarae__references,mcp__tarae__search,mcp__tarae__symbols,mcp__tarae__definition,\
mcp__tarae__hover,mcp__tarae__diagnostics,mcp__tarae__rename,mcp__tarae__replace,mcp__tarae__procs";

/// Settings laid over the user's and the project's when they're loaded (`llm.chat-settings`).
const CHAT_SETTINGS: &str = r#"{"permissions":{"ask":["Edit","Write"]},"disableAllHooks":true}"#;

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
    /// Wrapped lines (text length, width, lines) — only changed messages are rewrapped, even while streaming.
    pub cache: RefCell<Rendered>,
}

/// The screen as the user saw it, waiting to go with their next message (`C-s` · `:chat-screen`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shot {
    pub text: String,
    pub cols: u16,
    pub rows: u16,
}

/// (text length, width, wrapped lines).
pub type Rendered = Option<(usize, usize, Vec<Vec<Span>>)>;

impl Msg {
    pub(crate) fn new(role: Role, text: impl Into<String>, chip: impl Into<String>) -> Self {
        Msg {
            role,
            text: text.into(),
            chip: chip.into(),
            look: None,
            call: String::new(),
            attached: None,
            cache: RefCell::new(None),
        }
    }

    fn tool(look: Look, call: &str) -> Self {
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
    stdin: ChildStdin,
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
    /// No sandbox on this system, and the user agreed (`a` on its card) to run Claude's commands without
    /// one for this conversation (`sandbox.rs`).
    pub unprotected: bool,
    /// What the user allowed for this conversation (`a` on a card — `allow.rs`) · Claude's reads waiting
    /// for their card (`reads.rs`).
    pub allowed: crate::allow::Allowed,
    pub reads: std::collections::VecDeque<crate::reads::Pending>,
    /// Claude may look at the screen without asking for the rest of this conversation (`a` on its card —
    /// `seeing.rs`).
    pub sees: bool,
    /// Commands starting so run without asking for the rest of this conversation (`a` on a run card —
    /// `tools::pattern`).
    pub run_allow: Vec<String>,
    /// Scripts run by their path that run without asking for the rest of this conversation while they're
    /// what they were (real path, hash of what's in it — `a` on a run card, `readonly::script_pins`, #93).
    pub run_scripts: Vec<(PathBuf, u64)>,
    /// A message sent while Claude works (#23) — it goes when the turn ends; `↑` on an empty input takes it
    /// back to edit (or drop).
    pub queued: Option<String>,
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
    /// Its tool calls this turn whose result hasn't come yet, in the order claude made them (#87: claude
    /// makes them one after another, so what comes after a run card waits on it unseen).
    pub calls: Vec<Called>,
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
        let extra: Vec<String> = allow.iter().chain(&self.run_allow).cloned().collect();
        self.calls.iter().skip(1).any(|k| {
            k.name == "mcp__tarae__run"
                && k.input["command"]
                    .as_str()
                    .is_some_and(|cmd| !crate::tools::runs_unasked(cmd.trim(), &extra))
        })
    }
}

impl Chat {
    fn new() -> Self {
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
            unprotected: false,
            allowed: Default::default(),
            reads: Default::default(),
            sees: false,
            run_allow: Vec::new(),
            run_scripts: Vec::new(),
            queued: None,
            screen: None,
            conversation: None,
            resume: None,
            past: Vec::new(),
            plan: None,
            models: Vec::new(),
            model: String::new(),
            calls: Vec::new(),
        }
    }

    #[cfg(test)]
    pub fn proc_alive(&self) -> bool {
        self.proc.is_some()
    }

    /// The process of now — a later answer goes only to the one that asked.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn busy(&self) -> bool {
        self.state != State::Idle
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
    pub fn links(&self, root: &Path) -> Vec<Link> {
        self.targets(root).into_iter().map(|t| t.0).collect()
    }

    /// `links`, each with the tool call whose row it is (its replay stop).
    pub fn targets(&self, root: &Path) -> Vec<(Link, Option<String>)> {
        let mut out: Vec<(Link, Option<String>)> = Vec::new();
        for m in &self.msgs {
            let found: Vec<(Link, Option<String>)> = match m.role {
                Role::Tool => {
                    m.look.iter().filter_map(|l| Some((l.link.clone()?, Some(m.call.clone())))).collect()
                }
                Role::Assistant => refs_in(root, &m.text).into_iter().map(|(_, l)| (l, None)).collect(),
                Role::User => m.attached.iter().map(|l| (l.clone(), None)).collect(),
                _ => Vec::new(),
            };
            for t in found {
                // (two edits of one file are two stops — repeats collapse only for plain places)
                if t.1.is_some() || out.last().is_none_or(|l| l.0 != t.0) {
                    out.push(t);
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
    c.kill();
    ensure_proc(ed);
    ed.set_success("claude restarted · the conversation goes on");
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
            crate::edits::at_end_new_chat(ed);
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
    let Some(c) = &mut ed.session.chat else { return };
    if c.busy() {
        return ed.set_status("Claude is still answering — stop it (C-c) first");
    }
    c.kill();
    c.msgs = crate::chats::msgs_of(&saved.rows);
    // (an earlier "continued" row says nothing now — one, at the end, says it)
    c.msgs.retain(|m| !(m.role == Role::Note && m.text.starts_with("continued from ")));
    let when = crate::chats::ago(saved.updated, crate::chats::now());
    c.msgs.push(Msg::new(Role::Note, format!("continued from {when}"), ""));
    c.plan = saved.plan.as_ref().and_then(crate::plan::Plan::from_json);
    c.conversation = Some(saved.id.clone());
    c.resume = Some(saved.id);
    c.scroll = 0;
    c.follow.new_turn();
    ensure_proc(ed);
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
    ed.open_picker(crate::picker::Picker::new("conversations", Vec::new(), false), Some(fill));
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
    ed.session.chat = Some(c);
    let (Some(id), Some(dir)) = (r.conversation, ed.chats_dir.clone()) else { return ensure_proc(ed) };
    let root = ed.session.root.clone();
    ed.events.jobs().spawn(move || {
        let saved = crate::chats::load(&dir, &root);
        move |ed: &mut Editor| {
            let Some(c) = &mut ed.session.chat else { return };
            c.past = saved.iter().map(|s| (s.id.clone(), s.title.clone(), s.updated)).collect();
            let Some(s) = saved.into_iter().find(|s| s.id == id) else { return ensure_proc(ed) };
            continue_with(ed, s);
            let Some(c) = &mut ed.session.chat else { return };
            c.scroll = r.scroll;
            c.msgs.retain(|m| !(m.role == Role::Note && m.text == RESTARTED));
            if let Some(m) = c.msgs.last_mut().filter(|m| m.role == Role::Note) {
                m.text = RESTARTED.into();
            }
        }
    });
}

/// The conversation as it's kept: (store, project, it) — None before claude gave it an id.
fn kept(ed: &Editor) -> Option<(std::path::PathBuf, std::path::PathBuf, crate::chats::Saved)> {
    let (dir, c) = (ed.chats_dir.clone()?, ed.session.chat.as_ref()?);
    let id = c.conversation.clone()?;
    let title = c.msgs.iter().find(|m| m.role == Role::User).map(|m| m.text.clone()).unwrap_or_default();
    let plan = c.plan.as_ref().map(crate::plan::Plan::to_json);
    let saved = crate::chats::Saved {
        id,
        title,
        updated: crate::chats::now(),
        rows: crate::chats::rows_of(&c.msgs),
        plan,
        seq: crate::chats::seq(),
    };
    Some((dir, ed.session.root.clone(), saved))
}

/// The turn ended: the conversation is kept (on a worker) — its rows and claude's id for it.
fn keep(ed: &Editor) {
    let Some((dir, root, saved)) = kept(ed) else { return };
    ed.events.jobs().spawn(move || {
        crate::chats::save(&dir, &root, saved);
        |_: &mut Editor| {}
    });
}

/// Kept now, on this thread — the last thing before `:restart` replaces this process (a worker's save
/// would go with it).
pub fn keep_now(ed: &Editor) {
    if let Some((dir, root, saved)) = kept(ed) {
        crate::chats::save(&dir, &root, saved);
    }
}

pub fn close(ed: &mut Editor) {
    ed.session.chat = None;
    crate::restart::turn_over(ed);
}

/// New chat — fresh process (drops memory of the previous conversation): `C-l` in the chat, `:chat-new`
/// (the chat opened first if it's closed — and the recent conversation not continued when it's read).
pub fn reset(ed: &mut Editor) {
    let said = "New conversation with Claude";
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
    c.unprotected = false;
    c.allowed = Default::default();
    c.state = State::Idle;
    c.scroll = 0;
    c.follow.new_turn();
    c.conversation = None;
    c.resume = None;
    // (review at the end stays as it's set — `llm.review-at-end`)
    crate::edits::at_end_new_chat(ed);
    ensure_proc(ed);
    ed.set_status(said);
}

fn ensure_proc(ed: &mut Editor) {
    let cfg = ed.config.llm.clone();
    let tx = ed.events.sender();
    let Some(c) = &mut ed.session.chat else { return };
    if c.proc.is_some() {
        return;
    }
    let mut args = llm::with_tools(&cfg.args, TOOLS, ALLOWED);
    // The conversation is kept by claude too, so it can be continued (`chats.rs`)
    args.retain(|a| a != "--no-session-persistence");
    if let Some(id) = &c.resume {
        args.extend(["--resume".to_string(), id.clone()]);
    }
    if cfg.chat_settings {
        // CLAUDE.md files and settings as `claude` loads them — but hooks don't run, and edits always come
        // here as a review (an `ask` rule and the default mode outrank any allow rule or accept-edits mode)
        args = llm::with_flag(&args, "--setting-sources", "user,project,local");
        args = llm::with_flag(&args, "--settings", CHAT_SETTINGS);
        args = llm::with_flag(&args, "--permission-mode", "default");
    }
    let cfg = llm::LlmConfig { args, ..cfg };
    let system = system();
    let mut extra = vec!["--append-system-prompt", system.as_str(), "--permission-prompt-tool", "stdio"];
    if !c.no_thought_flag {
        extra.extend(["--thinking-display", "summarized"]);
    }
    let mut child = match llm::spawn(&cfg, &extra, &ed.session.root) {
        Ok(child) => child,
        Err(e) => {
            c.msgs.push(Msg::new(Role::Note, format!("{}: {e}", cfg.command), ""));
            return;
        }
    };
    let Some(mut stdin) = child.stdin.take() else { return };
    // tarae's MCP server (the note tool) — registered before anything else is said
    let _ = writeln!(stdin, "{}", llm::initialize_message(&["tarae"])).and_then(|_| stdin.flush());
    // A fresh process has seen no file yet
    c.sent.clear();
    // (unique across chats too: a closed chat's process ending after a new chat's started isn't the new one's)
    static GENERATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    c.generation = GENERATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let generation = c.generation;
    c.proc = Some(Proc { stdin, pid: child.id() });
    llm::pump(child, false, tx, move |ev| {
        Some(Box::new(move |ed: &mut Editor| on_event(ed, generation, ev)))
    });
}

fn on_event(ed: &mut Editor, generation: u64, ev: StreamEv) {
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
                Some(m) if m.role == Role::Thought => m.text.push_str(&t),
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
                    m.cache.replace(None);
                }
                _ => c.msgs.push(Msg::tool(look.clone(), &id)),
            }
            crate::follow::on_tool(ed, &id, &name, &input, &look);
        }
        // Kept from the first question on — a tarae closed mid-turn still has it
        StreamEv::Conversation { id, model } => {
            let new = c.conversation.as_deref() != Some(id.as_str());
            c.conversation = Some(id);
            c.model = model;
            if new {
                keep(ed);
            }
        }
        StreamEv::Models(list) => c.models = list,
        StreamEv::Answered(model) => c.model = model,
        StreamEv::ToolResult { id, text, error } => {
            c.calls.retain(|k| k.id != id);
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
                m.cache.replace(None);
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
                m.cache.replace(None);
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
            match c.msgs.last_mut() {
                Some(m) if m.role == Role::Assistant => m.text.push_str(&t),
                _ => c.msgs.push(Msg::new(Role::Assistant, t, "")),
            }
            crate::follow::on_words(ed, false);
        }
        StreamEv::Done(r) => {
            // (no turn in flight: claude ending before one — a conversation it can't resume)
            if c.state == State::Idle {
                return;
            }
            let secs = c.started.elapsed().as_secs_f32();
            c.state = State::Idle;
            c.calls.clear();
            let ok = r.is_ok();
            // (a session behind: its tab says `done` until it's looked at — #86)
            if ed.background() {
                ed.session.turn_done = true;
            }
            let Some(c) = ed.session.chat.as_mut() else { return };
            match r {
                // Answers with no streamed pieces (short answers, CLIs without partials) get the final text
                Ok(text) => {
                    if !text.is_empty() && !c.msgs.last().is_some_and(|m| m.role == Role::Assistant) {
                        c.msgs.push(Msg::new(Role::Assistant, text, ""));
                    }
                    c.msgs.push(Msg::new(Role::Note, format!("{secs:.1}s"), ""));
                }
                Err(e) => {
                    let e = if std::mem::take(&mut c.stopping) { "stopped".to_string() } else { e };
                    c.msgs.push(Msg::new(Role::Note, e, ""));
                }
            }
            crate::follow::on_done(ed);
            crate::edits::turn_over(ed);
            crate::tools::turn_over(ed);
            crate::reads::turn_over(ed);
            checkpoint_turn(ed);
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
            send_queued(ed);
        }
        StreamEv::Exit(err) => {
            c.proc = None;
            c.follow.gaze = None;
            c.calls.clear();
            // The conversation is gone from claude: a fresh one, the rows still shown
            if err.contains("No conversation found") && c.resume.is_some() {
                c.resume = None;
                c.conversation = None;
                c.state = State::Idle;
                c.msgs.push(Msg::new(
                    Role::Note,
                    "Claude no longer has this conversation — a new one starts",
                    "",
                ));
                return ensure_proc(ed);
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
                c.msgs.push(Msg::new(Role::Note, e, ""));
            }
            // A review (or a command's card) claude was waiting on has no one to answer now
            crate::edits::turn_over(ed);
            crate::tools::turn_over(ed);
            crate::reads::turn_over(ed);
            crate::restart::turn_over(ed);
        }
    }
}

// ── Send ───────────────────────────────────────────────────────────────────

/// `C-s` in the chat · `:chat-screen` — the screen as the user sees it now (the last frame, as text) goes
/// with their next message; the chat opens if it isn't, and the line over the input says so.
pub fn attach_screen(ed: &mut Editor) -> Result<(), String> {
    let (frame, cols, rows) = ed.last_frame.as_ref().ok_or("nothing is on screen yet")?;
    let (cols, rows) = (*cols, *rows);
    let text = crate::screen::text(frame, cols, rows).ok_or("this screen can't be read as text")?;
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
fn send_queued(ed: &mut Editor) {
    if ed.restart.is_some() {
        return;
    }
    let Some(q) = ed.session.chat.as_mut().and_then(|c| c.queued.take()) else { return };
    if !submit(ed, &q, None, None)
        && let Some(c) = ed.session.chat.as_mut()
    {
        c.queued = Some(q);
    }
}

pub fn send(ed: &mut Editor) {
    let Some(c) = &mut ed.session.chat else { return };
    let question = c.input.trim().to_string();
    // Claude is working: it waits its turn (#23) — said over the input, sent when this turn ends; a second
    // one joins it
    if c.busy() && !question.is_empty() {
        c.queued = Some(match c.queued.take() {
            Some(q) => format!("{q}\n\n{question}"),
            None => question,
        });
        c.input.clear();
        c.cursor = 0;
        return;
    }
    if !question.is_empty() && submit(ed, &question, None, None) {
        let c = ed.session.chat.as_mut().expect("just sent");
        c.input.clear();
        c.cursor = 0;
    }
}

/// Sends `question` as the next turn — with the editor context, plus `about` (a note's lines and thread;
/// `chip` then replaces the context summary). False if it couldn't go (busy, no process).
pub fn submit(ed: &mut Editor, question: &str, about: Option<String>, chip: Option<String>) -> bool {
    if ed.session.chat.as_ref().is_none_or(Chat::busy) {
        return false;
    }
    // Process first — a newly spawned one clears `sent`, so the context carries the whole file again
    ensure_proc(ed);
    let (context, ctx_chip, sent) = context(ed);
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
    let ok = writeln!(p.stdin, "{}", llm::user_message(&content)).and_then(|_| p.stdin.flush());
    if let Err(e) = ok {
        // (said again with the next message that goes)
        ed.session.checkpoints.tell = told;
        c.screen = shot;
        c.kill();
        c.msgs.push(Msg::new(Role::Note, format!("claude: {e} — press enter to retry"), ""));
        return false;
    }
    c.sent.extend(sent);
    c.hop = 0;
    c.follow.new_turn();
    c.unfolded = false;
    let mut chip = chip.unwrap_or(ctx_chip);
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
        let _ = writeln!(p.stdin, "{line}").and_then(|_| p.stdin.flush());
    }
}

/// Our answer to one of claude's control requests (the chat process of now).
pub fn respond(ed: &mut Editor, request_id: &str, response: serde_json::Value) {
    if let Some(p) = ed.session.chat.as_mut().and_then(|c| c.proc.as_mut()) {
        let line = llm::control_response(request_id, response);
        let _ = writeln!(p.stdin, "{line}").and_then(|_| p.stdin.flush());
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
        return crate::edits::request(ed, request_id, req);
    }
    if req["subtype"] != "mcp_message" || req["server_name"] != "tarae" {
        return Some(json!({}));
    }
    let response = crate::tools::mcp(ed, generation, request_id, &req["message"])?;
    Some(json!({ "mcp_response": response }))
}

pub fn stop(ed: &mut Editor) {
    let Some(c) = &mut ed.session.chat else { return };
    if !c.busy() {
        return;
    }
    if let Some(p) = &mut c.proc {
        c.stopping = true;
        let _ = writeln!(p.stdin, "{}", llm::interrupt_message()).and_then(|_| p.stdin.flush());
    }
}

/// Context to send + summary chip + the document versions whose full text went along.
fn context(ed: &Editor) -> (String, String, Vec<(DocId, u64)>) {
    let doc = ed.doc();
    let text = &doc.text;
    let name = doc.display_name(&ed.session.root);
    let lang = doc.syntax.as_ref().map(|s| s.lang.name.clone()).unwrap_or_default();
    let sel = doc.selection().primary();
    let head = sel.cursor(text);
    let line = text.byte_to_line(head.min(text.len_bytes()));
    let mut ctx = format!(
        "<editor-context>\nfile: {name}{}{}\ncursor: line {}\n",
        if lang.is_empty() { String::new() } else { format!(" ({lang})") },
        if doc.is_modified() && doc.path.is_some() { " (unsaved)" } else { "" },
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
    let mut chip = doc.display_name_short();
    let mut sent = Vec::new();
    let was_sent = |d: &crate::document::Document| {
        ed.session.chat.as_ref().and_then(|c| c.sent.get(&d.id)) == Some(&d.version())
    };
    // Selection (only when more than one character is selected)
    if sel.to() > crate::graphemes::next_boundary(text, sel.from()) {
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
    if !was_sent(doc) {
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
    if !diags.is_empty() {
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
        ctx.push_str("<open-files>\n");
        for d in others.iter().take(30) {
            ctx.push_str(&d.display_name(&ed.session.root));
            ctx.push_str(if d.is_modified() { " (unsaved)\n" } else { "\n" });
        }
        ctx.push_str("</open-files>\n");
        let mut budget = UNSAVED_MAX;
        for d in others.iter().filter(|d| d.is_modified() && !was_sent(d)) {
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
    (ctx, chip, sent)
}

/// Per pane on screen (last frame, the focused one first): its document and the lines it shows. A document
/// in two panes counts once, as its first.
fn on_screen(ed: &Editor) -> Vec<(DocId, Range<usize>)> {
    let mut panes: Vec<_> = ed.screen.panes.iter().collect();
    panes.sort_by_key(|p| p.0 != ed.session.focus);
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
    let links = c.targets(&ed.session.root);
    if links.is_empty() {
        return ed.set_status("nothing to go to yet");
    }
    let (link, call) = links[links.len() - 1 - c.hop % links.len()].clone();
    c.hop = (c.hop + 1) % links.len();
    if !call.is_some_and(|call| crate::follow::replay_call(ed, &call)) {
        jump(ed, link);
    }
}

// ── Keys ───────────────────────────────────────────────────────────────────

/// Keys while focus is in the chat panel. True if handled.
pub fn key(ed: &mut Editor, key: Key) -> bool {
    // (the second key of a sequence — `C-w >` from an empty input — is the keymap's)
    if !ed.pending.is_empty() {
        return false;
    }
    let Some(c) = ed.session.chat.as_mut().filter(|c| c.focused) else { return false };
    // An empty input: `C-w` is the windows' (sizes — `C-w >` widens the chat), nothing to delete
    if key.ctrl && key.code == Code::Char('w') && c.input.is_empty() {
        return false;
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
        // An empty input: Backspace takes the screen off
        (Code::Backspace, ..) if c.input.is_empty() && c.screen.is_some() => c.screen = None,
        // (C-g too — zellij takes it for its lock mode)
        (Code::Char(']') | Code::Char('g'), true, _) => follow(ed),
        (Code::Char('f'), true, _) => crate::follow::toggle(ed),
        (Code::Char('y'), true, _) => match c.last_code() {
            Some(code) => {
                crate::clipboard::copy(&ed.events.jobs(), code);
                ed.set_status("copied the code block");
            }
            None => ed.set_status("no code block in the last answer"),
        },
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
            c.cursor = c.input.len();
        }
        (Code::PageUp, ..) | (Code::Up, ..) if c.input.is_empty() || key.code == Code::PageUp => {
            c.scroll += if key.code == Code::PageUp { 10 } else { 1 };
        }
        (Code::PageDown, ..) | (Code::Down, ..) if c.input.is_empty() || key.code == Code::PageDown => {
            c.scroll = c.scroll.saturating_sub(if key.code == Code::PageDown { 10 } else { 1 });
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

    fn editor(text: &str) -> Editor {
        let mut ed = Editor::new(crate::config::Config::default());
        ed.docs[0].text = ropey::Rope::from_str(text);
        ed
    }

    fn settle_until(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
        let end = Instant::now() + std::time::Duration::from_secs(5);
        while !done(ed) {
            let left = end.saturating_duration_since(Instant::now());
            let ev = ed.events.recv_timeout(left).expect("event before timeout");
            ed.handle_event(ev);
        }
    }

    /// One character (a multi-byte grapheme) under the cursor isn't a selection; two are.
    #[test]
    fn selection_context_counts_graphemes() {
        use crate::selection::{Range, Selection};
        let mut ed = editor("한글\n");
        ed.doc_mut().set_selection(Selection::single(Range::new(0, 3)));
        assert!(!context(&ed).0.contains("<selection"));
        ed.doc_mut().set_selection(Selection::single(Range::new(0, 6)));
        assert!(context(&ed).0.contains("<selection lines=\"1-1\">\n한글\n</selection>"));
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
    }

    /// The system prompt sends "where is X defined" to the language server by name (never a grep for `fn X`
    /// / `struct X`), a type to hover, uses to references — Grep for what those can't do.
    #[test]
    fn the_prompt_prefers_the_language_server() {
        let s = system();
        let at = |w: &str| s.find(w).unwrap_or_else(|| panic!("{w:?} in the prompt"));
        assert!(
            s.contains("\"where is X defined\" is definition by name (symbol X, no file or line needed)")
        );
        assert!(s.contains("never a grep for `fn X`, `struct X` or `class X`"));
        assert!(at("definition by name") < at(", hover;") && at(", hover;") < at("references."));
        assert!(at("references.") < at("Grep is for what those can't do"));
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
}
