//! tarae's own tools for the chat — an MCP server that lives in the editor and speaks over the chat's
//! stream-json (`control_request mcp_message` → `control_response {mcp_response}`, `chat.rs`), so no server
//! or port is involved.
//!
//! - `note` pins a remark to lines (`notes.rs`) — answered at once.
//! - `run_tests` runs the tests of a file, or the one at a line, the way `space x x` does (`testing.rs`):
//!   found through the tree on a worker, run in the test pane the user sees, answered when the run ends
//!   (the call waits — a `Call` rides on the run). Claude checks its own work with it; the editor shows the
//!   test while it runs and the first failure when it's done (`follow.rs`).
//! - `run` runs a command in the task panel, the way `space m m` runs a task (`task.rs`) — answered when it
//!   ends, with how it ended, the places it pointed at and its output. Checks (build, test, lint, format,
//!   git's read-only commands) start at once; anything else asks the user first on a card (`y` run · `n` no).
//! - `screen` shows Claude the user's screen as text, once they say yes (`seeing.rs`).
//! - `references`, `search`, `symbols`, `definition`, `hover` and `diagnostics` find code the way the user
//!   does — the language server's answers, the project search, a file's outline — read-only, no card
//!   (`nav.rs`).
//! - `procs` says what `:procs` shows: the session's processes (or every session's) — pid, parent, age, CPU,
//!   footprint, command line, folder — read on a worker, no card (`procs.rs`).
//! - `rename` and `replace` are the editor's own refactoring — one call, one change set that lands as
//!   Claude's edits do, in the review (`refactor.rs`).
//! - `ui` says what's on the screen by id — the cards and what they ask, toasts, panes, panels, runs — at
//!   Claude's level, answered at once, no card (`uistate/claude.rs`).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::editor::Editor;
use crate::queued::{Queued, Ride};
use crate::sandbox::{Availability, Guard};
use crate::test_results::Status;
use crate::testing::{RunState, Scope, TestRun};

/// A tool call answered later: the chat process it came from, the ids to answer with, and the MCP revision
/// the answer is in.
#[derive(Clone, Debug)]
pub struct Call {
    generation: u64,
    request_id: String,
    id: Value,
    rev: crate::mcp::Rev,
    /// A `run`'s or `run_tests`' way from the ask to its end (`trace.rs`) — its card, the sandbox, the run
    /// carry it on; logged at the end, or where it was dropped.
    pub(crate) trace: Option<crate::trace::RunTrace>,
}

impl Call {
    /// A step on its run's way (`trace.rs`), if it's a run's.
    pub(crate) fn step(&self, what: &str) {
        if let Some(t) = &self.trace {
            t.step(what);
        }
    }

    /// The chat process it came from.
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }
}

/// The tools whose calls wait on the editor's own work (servers, searches, a change set) — a run, tests and
/// the screen wait on what may take long or on the user, and aren't watched.
const WATCHED: [&str; 8] =
    ["references", "search", "symbols", "definition", "hover", "diagnostics", "rename", "replace"];

/// An MCP message from claude → the `mcp_response` (None = answered later with `answer`). Either revision
/// (`mcp.rs`): after `initialize`, or each request naming its own.
pub fn mcp(ed: &mut Editor, generation: u64, request_id: &str, msg: &Value) -> Option<Value> {
    // Notifications get the empty answer the SDK gives
    let Some(id) = msg.get("id").cloned() else {
        return Some(json!({ "jsonrpc": "2.0", "result": {}, "id": 0 }));
    };
    let rev = match crate::mcp::rev(msg) {
        Ok(rev) => rev,
        Err(e) => return Some(json!({ "jsonrpc": "2.0", "id": id, "error": e })),
    };
    let call = || Call { generation, request_id: request_id.to_string(), id: id.clone(), rev, trace: None };
    let method = msg["method"].as_str().unwrap_or_default();
    let result = match method {
        "tools/list" => {
            sandbox_look(ed);
            let tools = [
                crate::notes::tool_schema(),
                run_tests_schema(),
                run_schema(),
                crate::plan::tool_schema(),
                crate::seeing::tool_schema(),
                crate::nav::references_schema(),
                crate::nav::search_schema(),
                crate::nav::symbols_schema(),
                crate::nav::definition_schema(),
                crate::nav::hover_schema(),
                crate::nav::diagnostics_schema(),
                crate::refactor::rename_schema(),
                crate::refactor::replace_schema(),
                crate::procs::tool_schema(),
                crate::uistate::claude::tool_schema(),
            ];
            Ok(json!({ "tools": tools.map(annotated) }))
        }
        "tools/call" => {
            let params = &msg["params"];
            let args = &params["arguments"];
            let r = match params["name"].as_str() {
                Some("note") => crate::notes::tool_call(ed, args),
                Some("plan") => crate::plan::tool_call(ed, args),
                Some("ui") => crate::uistate::claude::tool_call(ed),
                Some("run_tests") => match run_tests(ed, call(), args) {
                    Ok(()) => return None,
                    Err(e) => Err(e),
                },
                Some("run") => match run(ed, call(), args) {
                    Ok(()) => return None,
                    Err(e) => Err(e),
                },
                Some("screen") => {
                    crate::seeing::call(ed, call());
                    return None;
                }
                Some("procs") => {
                    crate::procs::tool_call(ed, call(), args);
                    return None;
                }
                Some(
                    name @ ("references" | "search" | "symbols" | "definition" | "hover" | "diagnostics"
                    | "rename" | "replace"),
                ) => {
                    let call = call();
                    // (watched while it waits — one silent too long marks its session: `session::Doing::Stuck`)
                    let tool = WATCHED.into_iter().find(|t| *t == name).unwrap_or("tool");
                    let wait = match name {
                        "diagnostics" => args["wait"].as_u64().unwrap_or(0).min(crate::nav::WAIT_MAX),
                        _ => 0,
                    };
                    let after = crate::session::STUCK_AFTER + std::time::Duration::from_secs(wait);
                    ed.tool_waits(generation, id.clone(), tool, after);
                    let asked = match name {
                        "rename" => crate::refactor::rename(ed, call, args),
                        "replace" => crate::refactor::replace(ed, call, args),
                        "references" => crate::nav::references(ed, call, args),
                        "symbols" => crate::nav::symbols(ed, call, args),
                        "definition" => crate::nav::definition(ed, call, args),
                        "hover" => crate::nav::hover(ed, call, args),
                        "diagnostics" => crate::nav::diagnostics(ed, call, args),
                        _ => crate::nav::search(ed, call, args),
                    };
                    match asked {
                        Ok(()) => return None,
                        Err(e) => {
                            ed.session.tool_waits.retain(|w| !w.is(generation, &id));
                            Err(e)
                        }
                    }
                }
                other => Err(format!("no tool {}", other.unwrap_or_default())),
            };
            Ok(content(r))
        }
        m => crate::mcp::common(m, msg, rev, json!({ "tools": {} })).ok_or_else(|| crate::mcp::not_found(m)),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": crate::mcp::finish(rev, method, r) }),
        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
    })
}

/// A tool's schema with its annotations: a title, and whether it only reads or may change things the
/// user has (hints for the client — tarae's own cards and reviews still decide).
pub(crate) fn annotated(mut schema: Value) -> Value {
    let (title, hint) = match schema["name"].as_str().unwrap_or_default() {
        "note" => ("Pin a note", None),
        "run_tests" => ("Run tests", None),
        "run" => ("Run a command", Some("destructiveHint")),
        "plan" => ("Show the plan", None),
        "screen" => ("See the screen", Some("readOnlyHint")),
        "references" => ("Find references", Some("readOnlyHint")),
        "search" => ("Search the project", Some("readOnlyHint")),
        "symbols" => ("File outline", Some("readOnlyHint")),
        "definition" => ("Go to definition", Some("readOnlyHint")),
        "hover" => ("Type and docs", Some("readOnlyHint")),
        "diagnostics" => ("Diagnostics", Some("readOnlyHint")),
        "rename" => ("Rename a symbol", Some("destructiveHint")),
        "replace" => ("Replace in files", Some("destructiveHint")),
        "procs" => ("The editor's processes", Some("readOnlyHint")),
        "ui" => ("What's on screen, by id", Some("readOnlyHint")),
        _ => return schema,
    };
    let mut a = json!({ "title": title });
    if let Some(hint) = hint {
        a[hint] = json!(true);
    }
    schema["annotations"] = a;
    schema
}

/// A tool's text as MCP content (Err = a tool error Claude can correct).
pub(crate) fn content(r: Result<String, String>) -> Value {
    match r {
        Ok(t) => json!({ "content": [{ "type": "text", "text": t }] }),
        Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
    }
}

#[cfg(test)]
thread_local! {
    /// (tests: what calls that waited were answered, by their id)
    pub static ANSWERED: std::cell::RefCell<Vec<(Value, Result<String, String>)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
impl Call {
    /// A call with this id from no chat process (its answer only goes to `ANSWERED`).
    pub fn test(id: u64) -> Call {
        Call {
            generation: u64::MAX,
            request_id: String::new(),
            id: json!(id),
            rev: Default::default(),
            trace: None,
        }
    }

    /// Call `id` (`request_id` its control request's) of chat process `generation`.
    pub fn of(generation: u64, request_id: &str, id: Value) -> Call {
        Call { generation, request_id: request_id.into(), id, rev: Default::default(), trace: None }
    }
}

/// Answers a call that waited — only to the process that made it, in whichever session it is. A run's
/// trace ends here unless its run's end said how (not run: why).
pub fn answer(ed: &mut Editor, call: Call, r: Result<String, String>) {
    #[cfg(test)]
    ANSWERED.with(|a| a.borrow_mut().push((call.id.clone(), r.clone())));
    if let Some(t) = &call.trace {
        match &r {
            Ok(_) => t.end("answered"),
            Err(e) => t.end(format!("not run: {}", e.chars().take(80).collect::<String>())),
        }
    }
    deliver(ed, call, r);
}

/// `answer`'s way to its chat: the session in `ed.session` if it's that chat's, else that session swapped in — or,
/// while another session behind is swapped in, from the main loop next (never a swap inside a swap). That
/// next turn is the owner's own event (`Event::For`, swapped in for it, so delivered at once), so it's one
/// hop, never a loop.
fn deliver(ed: &mut Editor, call: Call, r: Result<String, String>) {
    let owner = std::iter::once(&ed.session)
        .chain(&ed.sessions)
        .find(|l| l.chat.as_ref().is_some_and(|c| c.generation() == call.generation))
        .map(|l| l.id);
    match owner {
        // (its chat is gone, or another process took its place)
        None => {}
        Some(session) if session == ed.session.id => {
            ed.session.tool_waits.retain(|w| !w.is(call.generation, &call.id));
            let response = response(&call, r);
            crate::chat::respond(ed, &call.request_id, json!({ "mcp_response": response }));
        }
        Some(session) if !ed.background() => {
            ed.with_session(session, move |ed| deliver(ed, call, r));
        }
        // (its own event, the answer its whole work — `for_session`: nothing of it on screen, no frame)
        Some(session) => {
            let later = move |ed: &mut Editor| deliver(ed, call, r);
            let _ = ed.events.shared_sender().send(crate::event::Event::For(session, Box::new(later)));
        }
    }
}

/// A waited call's answer, in its request's revision.
fn response(call: &Call, r: Result<String, String>) -> Value {
    let result = crate::mcp::finish(call.rev, "tools/call", content(r));
    json!({ "jsonrpc": "2.0", "id": call.id, "result": result })
}

// ── run_tests ───────────────────────────────────────────────────────────────

fn run_tests_schema() -> Value {
    json!({
        "name": "run_tests",
        "description": "Run tests in the user's editor and get the results — after you change code, run the \
    tests that cover it to check your work. Give a test file, and a line to run just the test (or test \
    module/class) around it; without a line, the file's tests. The user watches the run in their editor. \
    Call it once the edits it should check are made, not alongside them. Unsaved files are saved first. Rust (cargo test), Go, Python (pytest), Java (Gradle/Maven).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Test file, absolute or relative to the project root" },
                "line": { "type": "integer", "description": "A line inside the test to run, 1-based (optional)" }
            },
            "required": ["path"]
        }
    })
}

/// Finds the target on a worker (the file read and parsed there), then runs it — the answer comes with
/// the run's end (`tests_done`); tests running already, it waits for them (`queued.rs`). Err = answered now.
fn run_tests(ed: &mut Editor, call: Call, args: &Value) -> Result<(), String> {
    let path = args["path"].as_str().filter(|p| !p.is_empty()).ok_or("path is required")?;
    let path = crate::chat::resolve(&ed.session.root, path);
    let line = args["line"].as_u64().map(|l| l.max(1) as usize - 1);
    let what = crate::chat::shown(&ed.session.root, &path);
    let what = line.map_or(what.clone(), |l| format!("{what}:{}", l + 1));
    let mut call = call;
    call.trace = Some(crate::trace::RunTrace::new(format!("claude's tests: {what}")));
    // The buffer's text when it's open (unsaved edits are what gets saved and run)
    let text = ed.docs.iter().find(|d| d.path.as_deref() == Some(path.as_path())).map(|d| d.text.clone());
    let root = ed.session.root.clone();
    ed.events.jobs().spawn(move || {
        let found = find(&root, &path, text, line);
        move |ed: &mut Editor| match found {
            Ok(target) => {
                call.step("found");
                start(ed, call, target)
            }
            Err(Found::NoGrammar(spec)) => {
                ed.offer_grammar(spec);
                let e = format!(
                    "tarae needs the {} grammar to find tests — it's offering the download to the user; ask \
                     them to accept it (y), then run again",
                    spec.name
                );
                answer(ed, call, Err(e));
            }
            Err(Found::Not(e)) => answer(ed, call, Err(e)),
        }
    });
    Ok(())
}

enum Found {
    NoGrammar(&'static crate::syntax::LangSpec),
    Not(String),
}

/// The test to run in `path` (at `line`, else the whole file) — on a worker; `root` the project's.
fn find(
    root: &Path,
    path: &std::path::Path,
    text: Option<ropey::Rope>,
    line: Option<usize>,
) -> Result<crate::testing::Target, Found> {
    let shown = crate::chat::shown(root, path);
    let path: PathBuf = std::fs::canonicalize(path).map_err(|e| Found::Not(format!("{shown}: {e}")))?;
    let spec = crate::syntax::detect(&path);
    crate::testing::supported(spec.map_or("", |s| s.name.as_str())).map_err(Found::Not)?;
    let spec = spec.expect("supported");
    let lang = crate::syntax::Loader::global().load(spec).map_err(|_| Found::NoGrammar(spec))?;
    let src = match text {
        Some(t) => t.to_string(),
        None => std::fs::read_to_string(&path).map_err(|e| Found::Not(format!("{shown}: {e}")))?,
    };
    let rope = ropey::Rope::from_str(&src);
    let (cursor, scope) = match line {
        Some(l) if l < rope.len_lines() => (rope.line_to_byte(l), Scope::Nearest),
        Some(l) => {
            return Err(Found::Not(format!("{shown} has {} lines — no line {}", rope.len_lines(), l + 1)));
        }
        None => (0, Scope::File),
    };
    // Past the leading whitespace, so the node found is the line's own
    let cursor = cursor + src[cursor..].len() - src[cursor..].trim_start_matches([' ', '\t']).len();
    let mut parser = tree_sitter::Parser::new();
    let tree = parser
        .set_language(&lang.language)
        .ok()
        .and_then(|_| parser.parse(&src, None))
        .ok_or_else(|| Found::Not(format!("couldn't parse {shown}")))?;
    crate::testing::find(&spec.name, &path, &src, &tree, cursor, scope).map_err(Found::Not)
}

fn start(ed: &mut Editor, call: Call, target: crate::testing::Target) {
    guarded(ed, call, Job::Tests(Box::new(target)), false);
}

fn start_tests(ed: &mut Editor, who: For, target: crate::testing::Target, guard: Option<Guard>, ride: Ride) {
    let before = ed.test_generation;
    let program = target.argv().first().cloned().unwrap_or_default();
    let trace = match &who {
        For::Claude(call) => call.trace.clone(),
        For::User(_) => ride.trace.clone(),
    };
    ed.test_start_guarded(target, guard, trace);
    match ed.session.test_run.as_mut().filter(|_| ed.test_generation != before) {
        Some(r) => {
            match who {
                For::Claude(call) => r.chat = Some(call),
                For::User(head) => r.tell = Some(head),
            }
            r.by_claude = true;
            (r.key, r.joined, r.queued, r.snap) = (ride.key, ride.joined, ride.queued, ride.snap);
        }
        None => fail(ed, who, ride.joined, format!("couldn't start {program}")),
    }
}

/// How a caller's wait before its run is said in its report (`waits::Waiting::said`).
#[derive(Clone, Copy, Debug)]
pub enum Waited<'a> {
    /// Not in line: it started when asked.
    No,
    /// So long in line, behind these.
    Queued(Duration, &'a [String]),
    /// It joined the same run under way, this far in.
    Joined(Duration),
}

impl Waited<'_> {
    /// The wait of a caller of the run that `started` then after `queued` in line: the one that asked it
    /// (`joined` None), or one that joined — asked when, and how far in the run was if it was under way.
    fn of<'a>(
        started: Instant,
        queued: Option<&'a Queued>,
        joined: Option<(Instant, Option<Duration>)>,
    ) -> Waited<'a> {
        let behind = queued.map_or(&[][..], |q| &q.behind[..]);
        match (joined, queued) {
            (Some((_, Some(d))), _) => Waited::Joined(d),
            (Some((asked, None)), _) => Waited::Queued(started.saturating_duration_since(asked), behind),
            (None, Some(q)) => Waited::Queued(started.saturating_duration_since(q.since), behind),
            (None, None) => Waited::No,
        }
    }

    /// The report's line on it, with the run's own waits (`waits.rs`) once it took `took`.
    fn line(self, wait: &crate::waits::Waiting, took: Option<Duration>) -> String {
        let joined = match self {
            Waited::Joined(d) => format!(
                "Joined a run of the same command already under way ({:.1}s in) — this is its result.\n",
                d.as_secs_f32()
            ),
            _ => String::new(),
        };
        let queued = match self {
            Waited::Queued(d, behind) => Some((d, behind)),
            _ => None,
        };
        let waited = took.and_then(|t| wait.said(t, queued)).map(|w| format!("{w}\n")).unwrap_or_default();
        format!("{joined}{waited}")
    }
}

/// The run Claude asked for ended — its results go back (to every call that joined it too), and the editor
/// shows the first failure. One the user ran again (`blocked.rs`): Claude hears how it ended with the next
/// message.
pub fn tests_done(ed: &mut Editor) {
    let Some(r) = ed.session.test_run.as_mut() else { return };
    if let Some(head) = r.tell.take() {
        let text = report(&ed.session.root, r, Waited::No);
        return told(ed, head, text);
    }
    let joined = std::mem::take(&mut r.joined);
    let Some(call) = r.chat.take() else { return };
    let snap = r.snap.take();
    let root = ed.session.root.clone();
    let text = report(&root, r, Waited::of(r.started, r.queued.as_ref(), None));
    let others: Vec<(Call, String)> = joined
        .into_iter()
        .map(|j| {
            let how = Waited::of(r.started, r.queued.as_ref(), Some((j.asked, j.running)));
            (j.call, report(&root, r, how))
        })
        .collect();
    let failed = r.failures().first().map(|&i| r.cases[i].clone());
    let label = format!("tests: {}", r.target.label);
    crate::reads::run_ended(ed);
    let generation = call.generation;
    let told = move |ed: &mut Editor, changed: String| {
        answer(ed, call, Ok(format!("{text}{changed}")));
        for (c, text) in others {
            answer(ed, c, Ok(format!("{text}{changed}")));
        }
        if let Some(case) = failed
            && let Some((path, line)) = case.at.clone()
        {
            let why =
                case.message.iter().map(|m| m.trim()).filter(|m| !m.is_empty()).take(3).collect::<Vec<_>>();
            let why = why.join(" · ");
            crate::follow::point(ed, "failed", case.name, &path, line..line + 1, Some(why));
        }
    };
    match snap {
        Some(snap) => run_changes(ed, snap, generation, label, told),
        None => told(ed, String::new()),
    }
}

/// The chat turn ended (stopped) with a run still going: it goes on for the user, unreported — and a
/// command still waiting for the user's yes is no longer asked about.
pub fn turn_over(ed: &mut Editor) {
    ed.session.tool_waits.clear();
    crate::queued::turn_over(ed);
    // (nothing waits on a card any more — its cards go below)
    ed.session.asking.clear();
    if let Some(r) = ed.session.test_run.as_mut() {
        r.chat = None;
        r.joined.clear();
    }
    if let Some(r) = ed.session.task_run.as_mut() {
        r.chat = None;
        r.joined.clear();
    }
    ed.drop_cards(|w| {
        use crate::offer::What;
        matches!(w, What::Run(_) | What::NoSandbox { .. } | What::ProtectedMade { .. })
    });
    crate::seeing::turn_over(ed);
    crate::refactor::turn_over(ed);
}

/// A run Claude waited on is gone before it ended (closed, or another run took its place).
pub fn tests_dropped(ed: &mut Editor, why: &str) {
    let Some(r) = ed.session.test_run.as_mut() else { return };
    let joined = std::mem::take(&mut r.joined);
    let silent = (r.state == RunState::Running)
        .then(|| crate::waits::silent_line(r.last_output, Instant::now()))
        .flatten()
        .map(|s| format!(". {s}"))
        .unwrap_or_default();
    let (snap, label) = (r.snap.take(), format!("tests: {}", r.target.label));
    if let Some(call) = r.chat.take() {
        crate::reads::run_ended(ed);
        let generation = call.generation;
        for c in std::iter::once(call).chain(joined.into_iter().map(|j| j.call)) {
            answer(ed, c, Err(format!("the test run was stopped — {why}{silent}")));
        }
        // (what it changed before it was stopped is the turn's all the same)
        if let Some(snap) = snap {
            run_changes(ed, snap, generation, label, |_: &mut Editor, _| {});
        }
    }
}

// ── run ─────────────────────────────────────────────────────────────────────

fn run_schema() -> Value {
    json!({
        "name": "run",
        "description": "Run a shell command in the project, in the user's editor, and get how it ended and \
    its output — build, lint, format, test suites, git. The user watches it in their task panel. Both output \
    streams come back whole (long output: its start and end), so don't redirect or pipe to head/tail for \
    that. Checks (cargo build/check/test/clippy/fmt/tree/metadata, go build/test/vet, git status/diff/log/show) \
    start at once — words only (quotes are fine), alone or piped into grep head tail wc sort cut uniq, sed -n \
    'a,bp'. So do read-only ones (ls cat head tail wc grep rg find tree stat file du diff sort uniq cut pwd \
    echo nm otool strings size, sed -n 'a,bp', git blame/rev-parse/ls-files/branch/show, git diff --no-index; \
    git -C <dir> too) whose paths stay in the project, the repository's other checkouts, the temp folders, \
    the Rust toolchain's sources ($(rustc --print sysroot)/lib/rustlib/src) or cargo's downloaded crates — \
    and lines of checks and reads joined by ; && || |, \
    with cd, for/while-read loops of reads, or the same in sh -c '…' (no other $, no backticks or redirects \
    to files). To run somewhere other than the project root, give cwd rather than a cd. Any other command \
    waits for the user to allow it — if they say no, don't retry it, ask. No stdin and no terminal: nothing \
    interactive. For one test file or test, run_tests is better (it reads each failure). Commands run in a \
    sandbox; when one needs one unix socket outside it (a server's — colima's docker.sock), ask for that: \
    unix_socket with your reason; when it can't work in it otherwise (the network, writing where the sandbox \
    doesn't let it), you may ask to run it outside: outside_sandbox with your reason — the user decides on a \
    card, this once.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command line (run by sh -c)" },
                "cwd": { "type": "string", "description": "The folder it runs in, inside the project (relative to its root); the root if left out" },
                "outside_sandbox": { "type": "boolean", "description": "Ask the user to run it outside the sandbox, this once (with reason)" },
                "unix_socket": { "type": "string", "description": "Ask the user to let it connect to this one unix socket (its path; with reason) — the rest stays sandboxed" },
                "reason": { "type": "string", "description": "Why it can't run in the sandbox as it is — required with outside_sandbox or unix_socket; the user reads it" }
            },
            "required": ["command"]
        }
    })
}

/// A reason to run outside the sandbox, at most (the card shows it whole).
const REASON_MAX: usize = 300;

/// Commands that run without asking: the checks, and git's read-only ones — by whole words, and only
/// when the line has nothing the shell would read as more than words (`allowed`). `cargo tree` and `cargo
/// metadata` read the dependency graph (#116) — cargo as the checks run it, in the sandbox.
pub const RUN_ALLOW: [&str; 15] = [
    "cargo build",
    "cargo check",
    "cargo test",
    "cargo clippy",
    "cargo fmt",
    "cargo tree",
    "cargo metadata",
    "go build",
    "go test",
    "go vet",
    "gofmt -l",
    "git status",
    "git diff",
    "git log",
    "git show",
];

/// A command longer than this isn't put to the user (a card can't show it whole).
const COMMAND_MAX: usize = 600;
const COMMAND_LINES_MAX: usize = 12;

/// Options that make a command that otherwise reads write a file or run a program.
pub const WRITES: [&str; 6] = ["--output", "--ext-diff", "--textconv", "--config", "--exec", "--upload-pack"];

/// The option of `WRITES` one of `words` passes, if any — no rule lets such a command through, so no card
/// offers one for it (#169).
fn writes_option(words: &[String]) -> Option<&'static str> {
    WRITES.iter().copied().find(|o| words.iter().any(|w| w.starts_with(o)))
}

/// Whether `cmd` runs without asking: one command and nothing else (`readonly::one_command` — quoted words
/// are fine; expansion, redirection, chaining, globs or an assignment and the words don't say what runs),
/// starting with one of the `RUN_ALLOW` or `extra` prefixes word for word (`extra`: `llm.run-allow`, this
/// chat's patterns), and none of the options that make it write or run something.
pub fn allowed(cmd: &str, extra: &[String]) -> bool {
    if let Some(words) = crate::readonly::one_command(cmd) {
        return allowed_words(&words, extra);
    }
    // `NAME=value` words in front (#44), or set through `env` (#93): only a rule naming each of them lets it
    // through (`A= cargo x`)
    let Some((names, words)) = crate::readonly::one_command_env(cmd) else { return false };
    env_allowed_words(&names, &words, extra)
}

/// A command setting `names` (`NAME=value` in front, or through `env`) allowed: by a rule naming each of
/// them — the card said `A=… B=… cargo xtask shot`, so those, or fewer, with any values; never a name it
/// didn't show (#132, user: an allowance must never widen what runs unasked past what they meant — #114 had
/// let any name a pattern may set through, and no list of names that run something is whole: `CC=x cargo
/// build` runs x from a build script). One more name asks again, its card's `a` naming them all. Each a
/// pattern may set (`readonly::env_allowed`) — not by the built-in checks nor a rule setting none:
/// `RUSTC_WRAPPER=x cargo build` runs x.
fn env_allowed_words(names: &[String], words: &[String], extra: &[String]) -> bool {
    if names.is_empty() {
        return allowed_words(words, extra);
    }
    if !names.iter().all(|n| crate::readonly::env_allowed(n)) {
        return false;
    }
    writes_option(words).is_none()
        && extra.iter().any(|rule| {
            let (set, rest) = rule_env(rule);
            !set.is_empty() && names.iter().all(|n| set.contains(&n.as_str())) && rule_matches(words, &rest)
        })
}

/// One command of a line allowed on its own: a check, a rule's words — `env NAME=value …` in front read as
/// the names it sets (#93).
fn part_allowed(words: &[String], extra: &[String]) -> bool {
    crate::readonly::env_words(words).is_some_and(|(names, rest)| env_allowed_words(&names, &rest, extra))
}

/// Whether `words` start with `rule`'s, word for word (an empty rule matches nothing).
fn rule_matches(words: &[String], rule: &[&str]) -> bool {
    !rule.is_empty() && words.len() >= rule.len() && words.iter().zip(rule).all(|(w, r)| w == r)
}

/// A rule's leading `NAME=` words (the names a command may set — `A=…` reads the same) and the rest.
fn rule_env(rule: &str) -> (Vec<&str>, Vec<&str>) {
    let words: Vec<&str> = rule.split_whitespace().collect();
    fn name(w: &str) -> Option<&str> {
        w.strip_suffix("=…").or_else(|| w.strip_suffix('='))
    }
    let lead = words.iter().take_while(|w| name(w).is_some_and(crate::readonly::env_allowed)).count();
    (words[..lead].iter().filter_map(|w| name(w)).collect(), words[lead..].to_vec())
}

/// Programs a check's output can go through and still count as the check (#32): they only read it
/// (`sed` only as `sed -n '3,40p'` — `readonly::only_reads`; the rest with the options `filters.rs` lists).
const FILTERS: [&str; 8] = ["grep", "head", "tail", "wc", "sort", "cut", "uniq", "sed"];

/// The first command's words when `cmd` is one command piped into filters and nothing else (`cargo test
/// 2>&1 | grep -E "a|b" | head`): each filter one of `FILTERS` with only options its list allows
/// (`filters::reads`). What the filters read besides their input is `readonly::outside`'s.
pub fn filtered(cmd: &str) -> Option<Vec<String>> {
    let mut parts = crate::readonly::piped(cmd)?.into_iter();
    let head = parts.next()?;
    let mut any = false;
    for f in parts {
        let words: Vec<&str> = f.iter().map(|w| w.text.as_str()).collect();
        let reads = match words[0] {
            "sed" => crate::readonly::only_reads(&f),
            _ => crate::filters::reads(&words),
        };
        if !FILTERS.contains(&words[0]) || !reads {
            return None;
        }
        any = true;
    }
    (any && !head.iter().any(|w| w.glob)).then(|| head.into_iter().map(|w| w.text).collect())
}

/// Whether `cmd` is a line of parts each allowed on its own (#57, #66): `cargo fmt && cargo clippy`,
/// `cargo test; cargo clippy`, `cd sub && git -C x log | sort` — every command one that runs unasked
/// (`allowed_words`, no glob; git's after `-C <dir>` as without it) or only reads, joined by `;` `&&` `||`
/// `|`, a `cd` aside. Where it reads (a `cd`'s or `-C`'s folder, the files named) is then judged as any
/// read's (`read_only`). (A script run by its path is never one: what `a` allows of it is its line exactly —
/// `Again`, #193.)
fn chain_allowed(cmd: &str, extra: &[String]) -> bool {
    let Some(parts) = crate::readonly::parse(cmd) else { return false };
    let mut any = false;
    for p in &parts {
        let crate::readonly::Part::Cmd(w) = p else { continue };
        let mut words: Vec<String> = w.iter().map(|w| w.text.clone()).collect();
        if words.first().is_some_and(|w| w == "git") {
            let rest = crate::readonly::git_after_c(&words[1..]).to_vec();
            words.truncate(1);
            words.extend(rest);
        }
        let plain = !w.iter().any(|w| w.glob) && part_allowed(&words, extra);
        if !plain && !crate::readonly::only_reads(w) {
            return false;
        }
        any = true;
    }
    any
}

/// `allowed` for one command's words — git's harmless options before its subcommand read as if not there
/// (`git --no-pager log`, #88).
fn allowed_words(words: &[String], extra: &[String]) -> bool {
    if writes_option(words).is_some() {
        return false;
    }
    let words = &crate::readonly::git_plain(words);
    // (`git diff --no-index a b` reads the files it names: a read, judged where it reads — #110)
    if let [git, args @ ..] = &words[..]
        && git == "git"
        && let [sub, rest @ ..] = crate::readonly::git_after_c(args)
        && sub == "diff"
        && rest.iter().any(|w| w == "--no-index")
    {
        return false;
    }
    RUN_ALLOW
        .iter()
        .copied()
        .chain(extra.iter().map(String::as_str))
        .any(|rule| rule_matches(words, &rule_env(rule).1))
}

/// Whether a command Claude runs is a check or a build — every command in its line one of the packs'
/// `checks` (a `cd` aside). Only those feed the user's list; a grep of Claude's looking around doesn't replace
/// it (#28).
pub fn is_check(cmd: &str) -> bool {
    // (a check's output through filters is the check's: its places are what's left — #32)
    if let Some(head) = filtered(cmd) {
        return check_words(&head);
    }
    let Some(parts) = crate::readonly::parse(cmd) else { return false };
    let mut any = false;
    for p in &parts {
        let crate::readonly::Part::Cmd(w) = p else { continue };
        // (`&& echo clean` says a word — A9)
        if crate::readonly::says(w) {
            continue;
        }
        if !check_words(&w.iter().map(|w| w.text.clone()).collect::<Vec<_>>()) {
            return false;
        }
        any = true;
    }
    any
}

/// Whether one command's words start with one of the packs' `checks` (builds, tests, linters — by whole
/// words: `cargo test`, `tsc`).
fn check_words(w: &[String]) -> bool {
    crate::languages::get().checks.iter().any(|c| {
        let c: Vec<&str> = c.split(' ').collect();
        w.len() >= c.len() && w.iter().zip(&c).all(|(w, c)| w == c)
    })
}

/// Programs that run whatever follows them — a pattern ending in one would allow anything. Interpreters
/// among them run a script file (`INTERPRETERS` — its card shows it).
const RUNNERS: [&str; 34] = [
    "sh",
    "bash",
    "zsh",
    "dash",
    "fish",
    "env",
    "sudo",
    "doas",
    "xargs",
    "nohup",
    "time",
    "nice",
    "exec",
    "eval",
    "command",
    "builtin",
    "source",
    "python",
    "python3",
    "node",
    "ruby",
    "perl",
    "awk",
    "gawk",
    "mawk",
    "nawk",
    "lua",
    "php",
    "osascript",
    "deno",
    "bun",
    "tsx",
    "rustup",
    "npx",
];

/// Whether a program's name is a runner's (`RUNNERS`, any `python…`) — what follows it, it runs.
pub(crate) fn runner(name: &str) -> bool {
    RUNNERS.contains(&name) || name.starts_with("python")
}

/// What a run card's `a` lets run again unasked for the rest of the chat (#193, user decision 2026-10-06):
/// this exact command — every word of the line's commands before its filters, the `NAME=value` words in
/// front of them (the same names, the same values) and its `cd`s — with any filters after it that only read
/// (`pipe_filter`): `cargo xtask trace test | grep A`, then `… | grep B`. Scripts it runs by their path are
/// allowed as they are now (`readonly::script_pins`). It replaced a 2–3-word prefix (`cargo xtask shot`, any
/// arguments, any values): never another command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Again {
    /// The line's parts before its filters, word for word (a `cd` as `cd <dir>`).
    pub parts: Vec<Vec<String>>,
    pub scripts: Vec<String>,
}

impl Again {
    /// Whether `cmd` is this command, its filters aside.
    pub fn covers(&self, cmd: &str) -> bool {
        head_of(cmd).is_some_and(|h| h == self.parts)
    }

    /// What it allows besides the command, as the card says it: ` and `x.sh` as it is now`.
    pub fn scripts_said(&self) -> String {
        self.scripts.iter().map(|s| format!(" and `{s}` as it is now")).collect()
    }
}

/// An `Again` the user allowed, and where: the folder it runs in, and Docker's socket when it was its card's
/// (that socket rides with this command only — `allow::Grant::as_outside`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunAgain {
    pub cwd: PathBuf,
    pub socket: Option<PathBuf>,
    pub again: Again,
}

impl RunAgain {
    pub fn covers(&self, cwd: &Path, socket: Option<&Path>, cmd: &str) -> bool {
        self.cwd == cwd && self.socket.as_deref() == socket && self.again.covers(cmd)
    }
}

/// Filters a command's output can go through and still be that command, for `a` (#193 — the user's list;
/// no `sed`). Each only reads what's piped in — `pipe_filter` says how.
const PIPE_FILTERS: [&str; 10] = ["grep", "head", "tail", "cut", "wc", "sort", "uniq", "tr", "awk", "jq"];

/// Whether `w` is a filter that only reads: one of `PIPE_FILTERS`, no glob; grep … tr with only the options
/// `filters.rs` lists for each (`sort -o`, `sort --out=f`, `tail -f`, `uniq - out` aren't); `awk` and `jq`
/// as `awk_reads` and `jq_reads` say. A file a filter names is read as any read is (where it reads is
/// judged — `judged`).
fn pipe_filter(w: &[crate::readonly::Word]) -> bool {
    let words: Vec<&str> = w.iter().map(|w| w.text.as_str()).collect();
    let Some((&name, args)) = words.split_first() else { return false };
    if w.iter().any(|w| w.glob) || !PIPE_FILTERS.contains(&name) {
        return false;
    }
    match name {
        "awk" => awk_reads(args),
        "jq" => jq_reads(args),
        _ => crate::filters::reads(&words),
    }
}

/// `awk` that only prints: options `-F` and `-v` only (no `-f` file of code, `-i`/`-l` loading more), one
/// program and no file — and nothing in the program that runs a command, writes, or reads past its input:
/// `system`, `getline`, `ENVIRON`, `close`, a `|` or `>` (a pipe or a file — comparisons with `>` and `||`
/// ask too), `@` (`@load`, `@include`).
fn awk_reads(args: &[&str]) -> bool {
    let mut program = None;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "-F" | "-v" if i + 1 < args.len() => i += 1,
            a if (a.starts_with("-F") || a.starts_with("-v")) && a.len() > 2 => {}
            a if a.starts_with('-') => return false,
            a if program.is_none() => program = Some(a),
            _ => return false,
        }
        i += 1;
    }
    let Some(p) = program else { return false };
    !p.contains(['|', '>', '@']) && !["system", "getline", "ENVIRON", "close"].iter().any(|w| p.contains(w))
}

/// `jq` that only reads what's piped in: its output options and `--arg`/`--argjson`, one filter and no file
/// (no `-f`, `--rawfile`, `--slurpfile`, `-L`), and a filter that neither loads code (`include`, `import`)
/// nor reads the environment (`env`, `$ENV`).
fn jq_reads(args: &[&str]) -> bool {
    const LONG: [&str; 13] = [
        "--raw-output",
        "--compact-output",
        "--slurp",
        "--null-input",
        "--join-output",
        "--ascii-output",
        "--sort-keys",
        "--exit-status",
        "--tab",
        "--raw-input",
        "--color-output",
        "--monochrome-output",
        "--raw-output0",
    ];
    let mut program = None;
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--arg" | "--argjson" if i + 2 < args.len() => i += 2,
            a if LONG.contains(&a) => {}
            a if a.len() > 1 && a.starts_with('-') && !a.starts_with("--") => {
                if !a[1..].chars().all(|c| "rcesnjaSCMR".contains(c)) {
                    return false;
                }
            }
            a if a.starts_with('-') => return false,
            a if program.is_none() => program = Some(a),
            _ => return false,
        }
        i += 1;
    }
    let Some(p) = program else { return false };
    // (`env` as a word of the filter, not a field — `.env` reads the input)
    let word = |w: &str| {
        p.match_indices(w).any(|(at, _)| {
            let before = p[..at].chars().next_back();
            let after = p[at + w.len()..].chars().next();
            let id = |c: char| c.is_ascii_alphanumeric() || c == '_';
            !before.is_some_and(|c| id(c) || c == '.' || c == '$') && !after.is_some_and(id)
        })
    };
    !p.contains("$ENV") && !["env", "include", "import"].iter().any(|w| word(w))
}

/// The parts of `cmd` before the filters at its end (`pipe_filter`, each after a `|`), word for word — a
/// command's `NAME=value` words in front kept, a `cd` as `cd <dir>`. None for a line `readonly::parse_env`
/// doesn't read, or one with a glob before its filters (the shell fills it in as it runs).
fn head_of(cmd: &str) -> Option<Vec<Vec<String>>> {
    use crate::readonly::Part;
    let (mut parts, piped) = crate::readonly::parse_env(cmd)?;
    while parts.len() > 1
        && piped.get(parts.len() - 2) == Some(&true)
        && matches!(parts.last(), Some(Part::Cmd(w)) if pipe_filter(w))
    {
        parts.pop();
    }
    parts
        .iter()
        .map(|p| match p {
            Part::Cd(d) => (!d.glob).then(|| vec!["cd".to_string(), d.text.clone()]),
            Part::Cmd(w) => (!w.iter().any(|w| w.glob)).then(|| w.iter().map(|w| w.text.clone()).collect()),
        })
        .collect()
}

/// What `a` on `cmd`'s card would allow (`Again`) — None when it can't be offered: a part that runs whatever
/// follows it (`RUNNERS` — `sh -c`, `env -i`, `xargs`, `python3 -c`; a script by its path is fine, pinned),
/// git (what reads runs already; what writes goes to `.git`, which the sandbox denies), a name set in front
/// that finds programs or loads code (`readonly::env_allowed`), an option no rule lets through
/// (`writes_option`, #169), a glob.
pub fn again(cmd: &str) -> Option<Again> {
    use crate::readonly::Part;
    let parts = head_of(cmd)?;
    let (parsed, _) = crate::readonly::parse_env(cmd)?;
    let mut scripts = Vec::new();
    for p in parsed.iter().take(parts.len()) {
        let Part::Cmd(w) = p else { continue };
        let words: Vec<String> = w.iter().map(|w| w.text.clone()).collect();
        let (names, rest) = crate::readonly::env_words(&words)?;
        if !names.iter().all(|n| crate::readonly::env_allowed(n)) || writes_option(&rest).is_some() {
            return None;
        }
        let first = rest.first()?;
        if let Some(s) = crate::readonly::script_part(&w[words.len() - rest.len()..]) {
            scripts.push(s.to_string());
            continue;
        }
        // (a program by its path that isn't a script pinned as it is — `../elsewhere/x.sh`, `/bin/sh` —
        // could be anything next time)
        let name = Path::new(first).file_name().and_then(|n| n.to_str()).unwrap_or(first);
        if runner(name) || name == "git" || first.contains('/') {
            return None;
        }
    }
    Some(Again { parts, scripts })
}

/// `cmd` as its reads are judged when it runs again by an `Again` (`read_only`): its words, the names set in
/// front left out (they change what runs, not where it reads), quoted for `readonly::parse`. None when a word
/// can't be put back so.
fn judged(cmd: &str) -> Option<String> {
    use crate::readonly::Part;
    let quote = |w: &str| -> Option<String> {
        let plain = |c: char| c.is_ascii_alphanumeric() || "_./:=@%+,-".contains(c);
        if !w.is_empty() && w.chars().all(plain) {
            Some(w.to_string())
        } else if !w.contains('\'') {
            Some(format!("'{w}'"))
        } else if !w.contains(['"', '$', '`', '\\', '!']) {
            Some(format!("\"{w}\""))
        } else {
            None
        }
    };
    let (parts, piped) = crate::readonly::parse_env(cmd)?;
    let mut out = String::new();
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            out.push_str(if piped[i - 1] { " | " } else { " ; " });
        }
        let words: Vec<String> = match p {
            Part::Cd(d) => vec!["cd".into(), d.text.clone()],
            Part::Cmd(w) => {
                let words: Vec<String> = w.iter().map(|w| w.text.clone()).collect();
                crate::readonly::env_words(&words).map_or(words, |(_, rest)| rest)
            }
        };
        let quoted: Option<Vec<String>> = words.iter().map(|w| quote(w)).collect();
        out.push_str(&quoted?.join(" "));
    }
    Some(out)
}

/// Whether socket `s` is one `allow.rs` calls as much as running outside the sandbox (Docker's): never
/// allowed for a chat — its card's `a` is the command's (#193).
fn outside_socket(s: &Path) -> bool {
    crate::allow::Grant::Socket(s.to_path_buf()).as_outside()
}

// Each Claude run admitted, with what rode along — what a test checks a run was given.
#[cfg(test)]
thread_local! {
    pub static ADMITTED: std::cell::RefCell<Vec<(String, Extra)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Runs `command` now (allowed), or once the user says yes on its card — answered when it ends.
fn run(ed: &mut Editor, call: Call, args: &Value) -> Result<(), String> {
    let cmd =
        args["command"].as_str().map(str::trim).filter(|c| !c.is_empty()).ok_or("command is required")?;
    // Said as what was crossed, not every limit at once ("3 lines; at most 12" read as the reason)
    let (chars, lines) = (cmd.chars().count(), cmd.lines().count());
    let mut crossed = Vec::new();
    if chars > COMMAND_MAX {
        crossed.push(format!("it's {chars} characters — a card shows a command of at most {COMMAND_MAX}"));
    }
    if lines > COMMAND_LINES_MAX {
        crossed.push(format!("it's {lines} lines — a card shows a command of at most {COMMAND_LINES_MAX}"));
    }
    if !crossed.is_empty() {
        return Err(format!(
            "too long to put to the user: {} — put it in a script with Write, then run that",
            crossed.join(", and ")
        ));
    }
    // Outside the sandbox, or with one socket past it: always asked, with Claude's reason on the card — a
    // check or a read-only command too
    let socket = args["unix_socket"].as_str().map(str::trim).filter(|s| !s.is_empty());
    let outside = match (args["outside_sandbox"].as_bool() == Some(true), socket) {
        (false, None) => None,
        (whole, socket) => {
            let asked = if socket.is_some() && !whole { "unix_socket" } else { "outside_sandbox" };
            let reason = args["reason"].as_str().map(str::trim).filter(|r| !r.is_empty()).ok_or(format!(
                "{asked} needs a reason: why it can't run in the sandbox as it is — the user reads it on the card"
            ))?;
            if reason.chars().count() > REASON_MAX {
                return Err(format!("keep the reason to {REASON_MAX} characters — the card shows it whole"));
            }
            let socket = match socket.filter(|_| !whole) {
                Some(s) => Some(socket_at(s)?),
                None => None,
            };
            Some(Beyond { reason: reason.to_string(), socket, since: None })
        }
    };
    // (one socket this chat allows already — `a` on its card: the sandbox has it for every run of the chat,
    // so it isn't asked again; the command goes as any does — #182: the very same call came back to its card)
    // (Docker's never is: it's as much as the sandbox off — #193)
    let allowed_socket = |b: &Beyond| {
        b.socket.as_ref().is_some_and(|s| {
            !outside_socket(s) && ed.session.chat.as_ref().is_some_and(|c| c.allowed.sockets.contains(s))
        })
    };
    let outside = outside.filter(|b| !allowed_socket(b));
    let cwd = run_cwd(&ed.session.root, args["cwd"].as_str())?;
    let mut call = call;
    call.trace = Some(crate::trace::RunTrace::new(format!("claude's run: {}", crate::task::traced(cmd))));
    // The same command sent again, while the first waits or runs what it would: one run, both answered (§3.4
    // of docs/design/async.md) — it was allowed or said yes to already, so no card
    let call = match outside {
        Some(_) => call,
        None => {
            let key = crate::queued::key_of(
                &ed.session.root,
                &Job::Command(cmd.to_string(), cwd.clone()),
                &Extra::default(),
            );
            match crate::queued::join(ed, &key, call) {
                Ok(()) => return Ok(()),
                Err(call) => call,
            }
        }
    };
    // This exact command, allowed for the chat on its card (`a` — #193): its filters may differ, Docker's
    // socket rides with it when that was its card's. Where it reads, and whether a script it runs is as it
    // was allowed, are looked at as for any read — anything off and it's the card.
    let socket = outside.as_ref().and_then(|b| b.socket.clone());
    let whole = outside.as_ref().is_some_and(|b| b.socket.is_none());
    let chat = ed.session.chat.as_ref();
    let allowed_again =
        !whole && chat.is_some_and(|c| c.run_again.iter().any(|r| r.covers(&cwd, socket.as_deref(), cmd)));
    if let Some(judge) = judged(cmd).filter(|_| allowed_again) {
        let pins = chat.map(|c| c.run_scripts.clone()).unwrap_or_default();
        return read_only(ed, call, cmd.to_string(), cwd, Some(pins), (judge, outside));
    }
    let extra = ed.config.llm.run_allow.clone();
    if outside.is_none() && allowed(cmd, &extra) {
        guarded(ed, call, Job::Command(cmd.to_string(), cwd), false);
        return Ok(());
    }
    // (a check piped into filters: the check's rule, and what the filters name is read as any read's — #32)
    // (a chain of allowed parts likewise — #57, #66)
    let checked =
        filtered(cmd).is_some_and(|head| allowed_words(&head, &extra)) || chain_allowed(cmd, &extra);
    if outside.is_none() && (checked || crate::readonly::read_only(cmd)) {
        return read_only(ed, call, cmd.to_string(), cwd, None, (cmd.to_string(), None));
    }
    let why = match &outside {
        Some(Beyond { socket: Some(_), .. }) => crate::cards::Why::Socket,
        Some(_) => crate::cards::Why::Outside,
        None => why_asked(cmd, &extra),
    };
    card(ed, call, cmd.to_string(), cwd, outside, why)
}

/// Why `cmd` comes to its card when nothing past the sandbox was asked, as far as its words tell — the
/// rule of `run`'s it fell through (its `card` line, `cards.rs`).
fn why_asked(cmd: &str, extra: &[String]) -> crate::cards::Why {
    use crate::cards::Why;
    if !scripts_run(cmd).is_empty() {
        return Why::Script;
    }
    if crate::readonly::one_command_env(cmd).is_some_and(|(names, _)| !names.is_empty()) {
        return Why::EnvName;
    }
    let parts = match crate::readonly::parse(cmd) {
        Some(parts) => parts,
        None => return Why::Unreadable(crate::readonly::unreadable(cmd).unwrap_or_default()),
    };
    for p in &parts {
        let crate::readonly::Part::Cmd(w) = p else { continue };
        if w.iter().any(|w| w.glob) {
            return Why::Glob;
        }
        let words: Vec<String> = w.iter().map(|w| w.text.clone()).collect();
        if !part_allowed(&words, extra) && !crate::readonly::only_reads(w) {
            // (`cargo test --config …`: the check's words, but no rule takes that option — #169)
            if let Some(o) = writes_option(&words) {
                return Why::Writes(o);
            }
            // (with its subcommand, when it has one: `cargo xtask`, `git commit` — `cargo test` is a check)
            let sub =
                |s: &&String| s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
            let sub = words.get(1).filter(|s| !s.starts_with('-')).filter(sub);
            let named = words.first().into_iter().chain(sub).cloned().collect::<Vec<_>>().join(" ");
            return Why::NotACheck(named);
        }
    }
    Why::Unreadable("no rule reads the line whole")
}

/// What Claude asked to run past the sandbox, and why: outside it whole, or with one unix socket (#119) —
/// and, the user having said yes to it before in this chat, what changed since (`sinceyes.rs`).
#[derive(Clone, Debug)]
pub struct Beyond {
    pub reason: String,
    pub socket: Option<PathBuf>,
    pub since: Option<crate::sinceyes::Since>,
}

/// The socket `unix_socket` names — `~/` from home — by its real path: a unix socket that's there. Err says
/// why not (said to Claude). Never a folder or a pattern: one file.
fn socket_at(s: &str) -> Result<PathBuf, String> {
    use std::os::unix::fs::FileTypeExt as _;
    let path = match (s.strip_prefix("~/"), crate::allow::home()) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => PathBuf::from(s),
    };
    if !path.is_absolute() || s.contains(['*', '?', '[']) {
        return Err(format!("unix_socket {s}: give the socket's full path — one file"));
    }
    let real = std::fs::canonicalize(&path).map_err(|e| format!("unix_socket {s}: {e}"))?;
    match std::fs::metadata(&real).is_ok_and(|m| m.file_type().is_socket()) {
        true => Ok(real),
        false => Err(format!("unix_socket {s} isn't a unix socket")),
    }
}

/// `cmd`'s card. Review at the end: a file it names that Claude wrote unseen this turn goes on the card
/// with it — which ones, and their text, found on a worker. So does each script it runs (#117: `awk -f x`,
/// `python x.py` — what runs is in the file, not the line). Outside the sandbox, said yes to before in this
/// chat: what changed since that yes, in what it runs (`sinceyes.rs`).
fn card(
    ed: &mut Editor,
    call: Call,
    cmd: String,
    cwd: PathBuf,
    mut outside: Option<Beyond>,
    why: crate::cards::Why,
) -> Result<(), String> {
    let checkpoints = &ed.session.checkpoints;
    let unseen: Vec<PathBuf> = checkpoints
        .turn
        .iter()
        .filter(|t| t.unseen)
        .map(|t| t.path.clone())
        .chain(checkpoints.carried.iter().cloned())
        .collect();
    let scripts = scripts_run(&cmd);
    // (the same run: what runs, not the filters after it — `sinceyes::key`)
    let yes = outside.as_ref().filter(|b| b.socket.is_none()).and_then(|_| {
        let (c, key) = (ed.session.chat.as_ref()?, crate::sinceyes::key(&cmd));
        c.outside_yes.iter().rev().find(|y| y.key == key && y.cwd == cwd).cloned()
    });
    if unseen.is_empty() && scripts.is_empty() && yes.is_none() {
        return ask_with(ed, cmd, cwd, call, Default::default(), (Vec::new(), why), outside);
    }
    let root = ed.session.root.clone();
    ed.events.jobs().spawn(move || {
        let since = yes.map(|y| y.since(&root, &cwd, &cmd));
        if let Some(b) = outside.as_mut() {
            b.since = since;
        }
        let shown = match unseen.is_empty() {
            true => Ok(Vec::new()),
            false => named(&root, &cmd, &cwd, &unseen),
        };
        let scripts = read_scripts(&root, &cwd, &scripts, shown.as_deref().unwrap_or_default());
        // A move or copy of an unseen file: where it goes is unseen too, from now on — before it runs, so no
        // later call reads it as a file the user saw (what's there now kept, when it's text or nothing)
        let carried: Vec<(PathBuf, Result<Option<ropey::Rope>, ()>)> = match crate::readonly::only_moves(&cmd)
        {
            true => moved(&cmd, &cwd, &unseen)
                .unwrap_or_default()
                .into_iter()
                .map(|p| {
                    let before = match std::fs::read(&p) {
                        Err(_) => Ok(None),
                        Ok(b) => {
                            String::from_utf8(b).map(|s| Some(ropey::Rope::from_str(&s))).map_err(|_| ())
                        }
                    };
                    (p, before)
                })
                .collect(),
            false => Vec::new(),
        };
        move |ed: &mut Editor| {
            call.step("files it names read");
            // (the turn ended meanwhile: nobody waits for a card)
            if ed.session.chat.as_ref().is_some_and(|c| c.generation() != call.generation || !c.busy()) {
                return;
            }
            for (path, before) in carried {
                match before {
                    Ok(before) => crate::checkpoint::record_unseen(ed, path, before, |_: &mut Editor| {}),
                    Err(()) => ed.session.checkpoints.carried.push(path),
                }
            }
            let c = call.clone();
            let asked = |shown| ask_with(ed, cmd, cwd, call, (shown, scripts), (Vec::new(), why), outside);
            if let Err(e) = shown.and_then(asked) {
                answer(ed, c, Err(e));
            }
        }
    });
    Ok(())
}

/// Programs that run a file of code named in their words, and how to find it: `sh x.sh`, `python3 -u
/// x.py`, `perl x.pl` — the first word not an option; `awk -f x.awk`, `sed -f x.sed` — the value of `-f`;
/// `deno run x.ts`, `bun run x.ts` — after the subcommand. An option that takes the code inline (`-c`,
/// `-e`, `-m` a module) names no file.
const INTERPRETERS: [&str; 22] = [
    "sh",
    "bash",
    "zsh",
    "dash",
    "fish",
    "python",
    "python3",
    "perl",
    "ruby",
    "node",
    "deno",
    "bun",
    "tsx",
    "lua",
    "php",
    "osascript",
    "Rscript",
    "awk",
    "gawk",
    "mawk",
    "nawk",
    "sed",
];

/// The script files `cmd` runs (`INTERPRETERS`, or one run by its path — `./x`), as written from where it
/// runs (after a `cd`: from there). A line the shell reader can't read (a redirect, `$`) is split at its
/// operators instead: this only says more.
pub(crate) fn scripts_run(cmd: &str) -> Vec<PathBuf> {
    let mut here = PathBuf::new();
    let commands: Vec<(PathBuf, Vec<String>)> = match crate::readonly::parse(cmd) {
        Some(parts) => parts
            .into_iter()
            .filter_map(|p| match p {
                crate::readonly::Part::Cmd(w) => {
                    Some((here.clone(), w.into_iter().map(|w| w.text).collect()))
                }
                crate::readonly::Part::Cd(d) => {
                    here.push(d.text);
                    None
                }
            })
            .collect(),
        None => cmd
            .split(['\n', ';', '|', '&', '(', ')', '`'])
            .map(|c| {
                (
                    here.clone(),
                    c.split_whitespace().map(|w| w.trim_matches(['\'', '"']).to_string()).collect(),
                )
            })
            .collect(),
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for (at, words) in commands {
        let words: Vec<String> = match crate::readonly::env_words(&words) {
            Some((_, rest)) => rest,
            None => words,
        };
        let Some((first, args)) = words.split_first() else { continue };
        let name = Path::new(first).file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let name = if name.starts_with("python") { "python" } else { name };
        let file = if !INTERPRETERS.contains(&name) {
            // (a program by its path that isn't one of them — the file itself, if it's text: `read_scripts`)
            first.contains('/').then(|| first.clone())
        } else if matches!(name, "awk" | "gawk" | "mawk" | "nawk" | "sed") {
            args.iter().enumerate().find_map(|(i, a)| match a.as_str() {
                "-f" | "--file" => args.get(i + 1).cloned(),
                a => a
                    .strip_prefix("--file=")
                    .or_else(|| a.strip_prefix("-f"))
                    .filter(|f| !f.is_empty())
                    .map(String::from),
            })
        } else {
            let args = match (name, args.first().map(String::as_str)) {
                ("deno" | "bun", Some("run")) => &args[1..],
                _ => args,
            };
            let inline =
                |a: &String| matches!(a.as_str(), "-c" | "-e" | "-E" | "-m" | "-p" | "--eval" | "--print");
            match args.iter().position(|a| !a.starts_with('-') || inline(a)) {
                Some(i) if !inline(&args[i]) => Some(args[i].clone()),
                _ => None,
            }
        };
        if let Some(f) = file.filter(|f| !f.is_empty()).map(|f| at.join(f)).filter(|f| !out.contains(f)) {
            out.push(f);
        }
    }
    out
}

/// A script a run card shows: where it is, and its text — or why it isn't shown.
#[derive(Clone, Debug)]
pub struct Script {
    pub path: PathBuf,
    pub text: Result<String, String>,
}

/// A script longer than this isn't put on the card (its size and path are).
const SCRIPT_LINES_MAX: usize = 400;

/// The scripts a card shows (`scripts_run`, from `cwd`), each that's there and not already on it as written
/// unseen (`shown`) — on a worker. One that isn't text, has characters a card can't show as they are, or is
/// longer than `SCRIPT_LINES_MAX` is named, and why it isn't shown. A program (not text) isn't a script.
fn read_scripts(root: &Path, cwd: &Path, scripts: &[PathBuf], shown: &[(PathBuf, String)]) -> Vec<Script> {
    let mut out: Vec<Script> = Vec::new();
    for s in scripts {
        let Ok(path) = std::fs::canonicalize(cwd.join(s)) else { continue };
        if !path.is_file() || shown.iter().any(|(p, _)| *p == path) || out.iter().any(|x| x.path == path) {
            continue;
        }
        let Ok(bytes) = std::fs::read(&path) else { continue };
        let name = crate::chat::shown(root, &path);
        let text = match String::from_utf8(bytes) {
            // (a binary run by its path is a program, not a script)
            Err(_) => continue,
            Ok(t) if t.contains('\0') => continue,
            Ok(t) if !draws_as_itself(&t) => {
                Err(format!("{name} has characters a card can't show as they are"))
            }
            Ok(t) if t.lines().count() > SCRIPT_LINES_MAX => Err(format!(
                "{name} has {} lines — too long to show here; read it before you say yes",
                t.lines().count()
            )),
            Ok(t) => Ok(t),
        };
        out.push(Script { path, text });
    }
    out
}

/// The folders `cmd`'s commands run in that aren't the checkout's root, as its run card says them (#152: a
/// `cd` in the line took it elsewhere, unsaid): from `cwd`, after each `cd` — relative to `root`, or the
/// whole path (`~` for home) outside it. Worked out from the words alone (`..` taken lexically, no disk);
/// a line the shell reader can't read says only `cwd`.
pub(crate) fn runs_in(root: &Path, cwd: &Path, cmd: &str) -> Vec<String> {
    let lexical = |p: PathBuf| {
        let mut out = PathBuf::new();
        for c in p.components() {
            match c {
                std::path::Component::ParentDir => {
                    out.pop();
                }
                std::path::Component::CurDir => {}
                c => out.push(c),
            }
        }
        out
    };
    let mut here = cwd.to_path_buf();
    let mut at: Vec<PathBuf> = Vec::new();
    match crate::readonly::parse(cmd) {
        Some(parts) => {
            for p in parts {
                match p {
                    crate::readonly::Part::Cd(d) => here = lexical(here.join(d.text)),
                    crate::readonly::Part::Cmd(_) if !at.contains(&here) => at.push(here.clone()),
                    crate::readonly::Part::Cmd(_) => {}
                }
            }
        }
        None => at.push(here),
    }
    at.into_iter()
        .filter(|f| f != root)
        .map(|f| match f.strip_prefix(root) {
            Ok(rel) => rel.display().to_string(),
            Err(_) => crate::allow::tilde(&f),
        })
        .collect()
}

/// Where a command runs: the project root (`root`, a real path), or `cwd` in it (relative to the root, or
/// absolute) — a folder that's there, inside the project (real path), so a `cd` isn't needed (#27).
fn run_cwd(root: &Path, cwd: Option<&str>) -> Result<PathBuf, String> {
    let Some(cwd) = cwd.map(str::trim).filter(|c| !c.is_empty() && *c != ".") else {
        return Ok(root.to_path_buf());
    };
    let dir =
        crate::paths::existing_inside(root, &root.join(cwd), &format!("cwd {cwd}"), "commands run in it")?;
    if !dir.is_dir() {
        return Err(format!("cwd {cwd} isn't a folder"));
    }
    Ok(dir)
}

/// A command that only reads (`readonly.rs`) — or a line of checks and reads (`chain_allowed`, `filtered`):
/// where it reads is found on a worker — all of it in the project, or in places allowed, and it runs now;
/// anything else is on its card. A part that runs something (a check, git) outside the project is the plain
/// card, reads allowed there or not (`readonly::runs_outside`). `pins` (a command `a` allowed — #193): every
/// script it runs must be the chat's as it was allowed — one changed since, or another, and it's the plain
/// card. `judge`: the line as where it reads is judged (`judged` — the names set in front left out);
/// `beyond`: Docker's socket riding with it (its card's, `a`).
fn read_only(
    ed: &mut Editor,
    call: Call,
    cmd: String,
    cwd: PathBuf,
    pins: Option<Vec<(PathBuf, u64)>>,
    (judge, beyond): (String, Option<Beyond>),
) -> Result<(), String> {
    let chat = ed.session.chat.as_ref().map(|c| c.allowed.clone()).unwrap_or_default();
    let (store, root, temp) = (ed.allow_path.clone(), ed.session.root.clone(), ed.read_temp);
    ed.events.jobs().spawn(move || {
        let mut kept = crate::allow::Store::read(store.as_deref()).get(&root);
        let defaults = crate::allow::defaults(&root, temp);
        kept.read.extend(defaults.all());
        let sysroot = judge.contains(crate::readonly::SYSROOT).then(|| crate::allow::sysroot(&cwd)).flatten();
        let reads = crate::readonly::outside(&judge, &cwd, &root, &chat, &kept, sysroot.as_deref());
        // (where they are, for the card's line)
        let classes = reads.as_ref().map(|r| crate::cards::classes(r, &defaults)).unwrap_or_default();
        // (a check or git that would run outside the project: the plain card, whatever reads are allowed)
        let runs_outside = crate::readonly::runs_outside(&judge, &cwd, &root, sysroot.as_deref());
        let pinned = pins.is_none_or(|pins| {
            crate::readonly::script_pins(&judge, &cwd, &root)
                .is_some_and(|s| s.iter().all(|p| pins.contains(p)))
        });
        move |ed: &mut Editor| {
            use crate::cards::Why;
            call.step("where it reads looked at");
            // (the turn ended meanwhile: nobody waits for it)
            if ed.session.chat.as_ref().is_some_and(|c| c.generation() != call.generation || !c.busy()) {
                return;
            }
            // (a check or git running outside; a script not as it was allowed: its card, as any command's —
            // written unseen, shown on it)
            if runs_outside || !pinned {
                let c = call.clone();
                // (a `cd` that can't be followed counts as outside: said as what it is)
                let why = match &reads {
                    Err(e) if runs_outside => Why::CdUnknown(e.clone()),
                    _ if runs_outside => Why::RunsOutside,
                    _ => Why::ScriptChanged,
                };
                if let Err(e) = card(ed, call, cmd, cwd, beyond, why) {
                    answer(ed, c, Err(e));
                }
                return;
            }
            // (a `cd` into what isn't a folder: where the rest reads can't be told — the plain card)
            let (reads, why) = match reads {
                Ok(r) if r.is_empty() => {
                    // (Docker's socket for this run alone — its card's `a` was this command's)
                    let grant = beyond.and_then(|b| b.socket).map(crate::allow::Grant::Socket);
                    let extra = Extra { grant, outside: false };
                    return guard_and_go(ed, For::Claude(call), Job::Command(cmd, cwd), false, extra);
                }
                Ok(r) => (r, Why::ReadsOutside(classes)),
                Err(e) => (Vec::new(), Why::CdUnknown(e)),
            };
            let c = call.clone();
            if let Err(e) = ask_with(ed, cmd, cwd, call, Default::default(), (reads, why), beyond) {
                answer(ed, c, Err(e));
            }
        }
    });
    Ok(())
}

/// Whether `cmd` runs without a card as far as its words tell (`run`'s order: a check or an allowed
/// pattern, a check through filters, a chain of allowed parts, a read) — where a read reads isn't looked at
/// (that's a worker's): a read outside the project may still ask.
pub fn runs_unasked(cmd: &str, extra: &[String]) -> bool {
    allowed(cmd, extra)
        || filtered(cmd).is_some_and(|head| allowed_words(&head, extra))
        || chain_allowed(cmd, extra)
        || crate::readonly::read_only(cmd)
}

pub const OUTSIDE_RUN_TITLE: &str = "Claude wants to run — outside the sandbox";
pub const SOCKET_RUN_TITLE: &str = "Claude wants to run — with one socket";

/// The card for `cmd`, saying too what it reads outside the project (`reads` — a read-only command's):
/// those can be allowed for longer, as on a read's card, unless one of them never can. Outside the sandbox
/// (`outside` = Claude's reason): its own title, the reason, what that means — once, never kept. Its
/// pattern (`pattern`) can be allowed for the rest of the chat with `a` — said on the card. With each file
/// in `shown` (path, text) whole under it — written unseen —, and each script it runs (`scripts`). `why`:
/// the rule that put it to the user (its `card` line).
fn ask_with(
    ed: &mut Editor,
    cmd: String,
    cwd: PathBuf,
    call: Call,
    (shown, scripts): (Vec<(PathBuf, String)>, Vec<Script>),
    (reads, why): (Vec<PathBuf>, crate::cards::Why),
    outside: Option<Beyond>,
) -> Result<(), String> {
    // (a card up for another command already: this one waits under it, counted — #5: refused, Claude sent
    // the same command again and again)
    let mut question = cmd.clone();
    // What's decided: the command (and where it runs, when not at the root), what it reads outside (one
    // place a line)
    let mut decided = 0..cmd.split('\n').count();
    let folders = runs_in(&ed.session.root, &cwd, &cmd);
    if !folders.is_empty() {
        question.push_str(&format!("\n  in {}", folders.join(", ")));
        decided.end += 1;
    }
    // Why it asks, where nothing below says it (#164: `cd ~/…/*/x && grep …` asked in silence)
    if let Some(said) = why.said() {
        question.push_str(&format!("\n\n{said}"));
    }
    // Said yes to before in this chat: when, and what changed since in what it runs — decided, the diff in
    // its colours (`sinceyes.rs`)
    let mut diff = None;
    let since = outside.as_ref().and_then(|b| b.since.as_ref());
    if let Some(s) = since {
        let (lines, d) = s.card_lines();
        let at = question.split('\n').count() + 1;
        question.push_str("\n\n");
        question.push_str(&lines.join("\n"));
        decided = 0..at + lines.len();
        diff = Some(at + d.start..at + d.end);
    }
    // (outside: the reason faint — Claude's own words — and what yes means in the warning color; one
    // socket: which, said as decided, and the rest staying sandboxed — Docker's in the warning color)
    let mut warning = None;
    if let Some(b) = &outside {
        question.push_str(&format!("\n\nIts reason: {}\n\n", b.reason.replace('\n', " ")));
        let at = question.split('\n').count() - 1;
        match &b.socket {
            None => {
                question.push_str(
                    "Outside the sandbox it runs with all your permissions — your files, your network — this \
                     once.",
                );
                warning = Some(at..at + 1);
            }
            Some(s) => {
                let g = crate::allow::Grant::Socket(s.clone());
                // (what `a` allows, said exactly — #182: read as this command's, it was the socket's; #193:
                // Docker's is this command's only)
                let a = match (g.as_outside(), again(&cmd).filter(|_| reads.is_empty())) {
                    (true, Some(_)) => {
                        "a lets this command connect again for the rest of this chat — with any filters after \
                         it that only read — never another command."
                    }
                    (true, None) => "Nothing longer than this run can be allowed for it.",
                    (false, _) => {
                        "a for the rest of this chat, whatever command asks — each command is still asked as \
                         usual."
                    }
                };
                question.push_str(&format!(
                    "It may connect to {} — that one file; everything else stays in the sandbox. y lets it \
                     connect for this run only; {a}",
                    crate::allow::tilde(s)
                ));
                if let Some(w) = g.warning() {
                    question.push_str(&format!("\n\n{w}"));
                    let at = question.split('\n').count() - 1;
                    warning = Some(at..at + 1);
                }
            }
        }
    }
    if !reads.is_empty() {
        question.push_str("\n\nIt reads outside the project:");
        for p in &reads {
            question.push_str(&format!("\n  {}", p.display()));
        }
        question.push_str("\n\nWhat it reads goes to Claude.");
        decided.end += 2 + reads.len();
    }
    let home = crate::allow::home();
    let keep = outside.is_none()
        && !reads.is_empty()
        && reads.iter().all(|p| crate::allow::keepable(p, home.as_deref()));
    // What `a`/`A` allow: files beside each other (or beside one read once) as their folder (#102)
    let once = ed.session.chat.as_ref().map(|c| c.read_once.clone()).unwrap_or_default();
    let grants = match keep {
        true => crate::allow::grant_places(&reads, &once, home.as_deref()),
        false => Vec::new(),
    };
    let folders: Vec<&PathBuf> = grants.iter().filter(|g| !reads.contains(g)).collect();
    if !folders.is_empty() {
        let shown: Vec<String> = folders.iter().map(|f| f.display().to_string()).collect();
        question.push_str(&format!(
            "\n\na and A allow everything in {} — not only these files.",
            shown.join(", ")
        ));
    }
    // (a command that only reads is asked for where it reads, not what it is; past the sandbox only
    // Docker's socket, as this command's — #193)
    // (nor where it reads can't be told — a `cd` into no folder: it would ask again anyway)
    let docker = outside.as_ref().is_some_and(|b| b.socket.as_deref().is_some_and(outside_socket));
    let judged_reads = reads.is_empty() && !matches!(why, crate::cards::Why::CdUnknown(_));
    let offered = again(&cmd).filter(|_| (outside.is_none() || docker) && judged_reads);
    if let Some(a) = offered.as_ref().filter(|_| outside.is_none()) {
        question
            .push_str(&format!("\n\na — this exact command{} again unasked, this chat", a.scripts_said()));
    }
    for (path, text) in &shown {
        let text = text.replace("\r\n", "\n").replace('\t', "    ");
        question.push_str(&format!(
            "\n\n{} — written this turn, unseen:\n{}",
            crate::chat::shown(&ed.session.root, path),
            text.trim_end_matches('\n')
        ));
    }
    // What a script it runs says — what runs is in it (#117); one that can't be shown, why. (Said yes to
    // before: what changed in it since is on the card already — not all of it again.)
    let told = since.is_some_and(|s| !matches!(s.changed, crate::sinceyes::Changed::Unknown(_)));
    for s in scripts.iter().filter(|_| !told) {
        match &s.text {
            Ok(text) => question.push_str(&format!(
                "\n\n{} — what it runs:\n{}",
                crate::chat::shown(&ed.session.root, &s.path),
                text.replace("\r\n", "\n").replace('\t', "    ").trim_end_matches('\n')
            )),
            Err(why) => question.push_str(&format!("\n\n{why}")),
        }
    }
    // (`y` reads them again, as the unseen files: not run if one changed since the card)
    let shown: Vec<(PathBuf, String)> = shown
        .into_iter()
        .chain(scripts.into_iter().filter_map(|s| s.text.ok().map(|t| (s.path, t))))
        .collect();
    // Where there's no sandbox, this card is where the user hears it (outside one, it said so already)
    let socket = outside.as_ref().and_then(|b| b.socket.clone());
    let unprotected = match (&ed.sandbox, ed.config.llm.sandbox) {
        _ if outside.is_some() && socket.is_none() => None,
        (Availability::Missing(why), true) => {
            Some(format!("No sandbox here ({why}) — it runs with all your permissions."))
        }
        (Availability::Partial(why), true) => {
            Some(format!("The sandbox here goes only so far ({why}) — it runs in it as far as it goes."))
        }
        (_, false) => {
            Some("The sandbox is off (llm.sandbox) — it runs with all your permissions.".to_string())
        }
        _ => None,
    };
    if let Some(u) = &unprotected {
        question.push_str(&format!("\n\n{u}"));
    }
    let title = match (&outside, &socket) {
        (_, Some(_)) => SOCKET_RUN_TITLE.to_string(),
        (Some(_), None) => OUTSIDE_RUN_TITLE.to_string(),
        (None, None) => "Claude wants to run".to_string(),
    };
    let past = match (&outside, &socket) {
        (_, Some(_)) => crate::cards::Past::Socket,
        (Some(_), None) => crate::cards::Past::Outside,
        (None, None) => crate::cards::Past::No,
    };
    let at = match &why {
        crate::cards::Why::ReadsOutside(c) => c.clone(),
        _ => Vec::new(),
    };
    let sandboxed =
        ed.config.llm.sandbox && !matches!(ed.sandbox, Availability::Missing(_) | Availability::Partial(_));
    let asked = crate::cards::Asked::new(why, crate::cards::Request::Run { past, sandboxed, reads: at });
    call.step("card up");
    let card = RunCard {
        cmd,
        cwd,
        call,
        shown,
        unprotected: unprotected.is_some(),
        reads,
        grants,
        keep,
        outside: outside.is_some() && socket.is_none(),
        socket,
        again: offered,
    };
    let what = crate::offer::What::Run(Box::new(card));
    let o = crate::offer::Offer::new(title, &question, "Running…", "Claude's command".into(), what);
    let mut o = o.deciding(decided).asked(asked);
    o.diff = diff;
    ed.push_offer(match warning {
        Some(lines) => o.warning(lines),
        None => o,
    });
    Ok(())
}

/// A file Claude wrote unseen is shown on the card whole — at most this many lines, each at most this many
/// characters — or the command isn't put to the user.
const SHOWN_LINES_MAX: usize = 16;
const SHOWN_LINE_MAX: usize = 160;

/// The files among `unseen` (real paths) that `cmd` names, with their text — on a worker. The words are
/// read the way the shell would join them (quotes and backslashes gone) and split at its operators; a
/// name made up while it runs (a variable, a glob) isn't seen. Err: one can't be shown whole (said from
/// `root`, the project's).
fn named(
    root: &Path,
    cmd: &str,
    cwd: &std::path::Path,
    unseen: &[PathBuf],
) -> Result<Vec<(PathBuf, String)>, String> {
    // Moving, copying, removing or reading a file runs none of it (#76: `mv src/a.rs src/b.rs` was refused)
    // — where it goes is marked unseen in its stead (`moved`); a line that runs anything else may run what
    // it moved, so it names them all, and so does a move whose targets can't be told
    if crate::readonly::only_moves(cmd) && moved(cmd, cwd, unseen).is_some() {
        return Ok(Vec::new());
    }
    let joined: String = cmd.chars().filter(|c| !matches!(c, '\'' | '"' | '\\')).collect();
    let mut out: Vec<(PathBuf, String)> = Vec::new();
    for word in joined.split(|c: char| c.is_whitespace() || ";&|`$()<>={}".contains(c)) {
        if word.is_empty() {
            continue;
        }
        let Ok(real) = std::fs::canonicalize(cwd.join(word)) else { continue };
        if unseen.contains(&real) && !out.iter().any(|(p, _)| *p == real) {
            let text = showable(root, &real)?;
            out.push((real, text));
        }
    }
    Ok(out)
}

/// Where a line that only moves files (`readonly::only_moves`) puts what was written unseen (`unseen`, real
/// paths): for each `mv`, `cp`, `ln` and `git mv`, its target for every unseen file among its sources or
/// under a folder it moves — real paths, made up from the real folder for a target not there yet. The
/// source of a `mv` is gone after it, so its mark goes nowhere else. On a worker. None: a part whose targets
/// can't be told (a glob, an option that picks the target — `-t` —, a long option, `git -C`) — then every
/// unseen file the line names counts as named, as for any command.
fn moved(cmd: &str, cwd: &Path, unseen: &[PathBuf]) -> Option<Vec<PathBuf>> {
    use crate::readonly::Part;
    let real = |p: &Path| -> Option<PathBuf> {
        std::fs::canonicalize(p)
            .ok()
            .or_else(|| Some(std::fs::canonicalize(p.parent()?).ok()?.join(p.file_name()?)))
    };
    let mut here = cwd.to_path_buf();
    let mut out = Vec::new();
    for part in crate::readonly::parse(cmd)? {
        let w = match part {
            Part::Cd(d) => {
                here = crate::readonly::cd_into(&here, &d, None).ok()?;
                continue;
            }
            Part::Cmd(w) => w,
        };
        let words: Vec<&str> = w.iter().map(|w| w.text.as_str()).collect();
        let args = match words.as_slice() {
            ["mv" | "cp" | "ln", rest @ ..] => rest,
            ["git", ..] if words.contains(&"-C") => return None,
            // (its harmless options passed over: `git --no-pager mv` moves as `git mv` does)
            ["git", rest @ ..] => match crate::readonly::git_after_c(rest) {
                ["mv", rest @ ..] => rest,
                _ => continue,
            },
            _ => continue,
        };
        let picks = |a: &&str| a.starts_with("--") || (a.starts_with('-') && a[1..].contains(['t', 'T']));
        if w.iter().any(|w| w.glob) || args.iter().any(picks) {
            return None;
        }
        let paths: Vec<&str> = args.iter().filter(|a| !a.starts_with('-')).copied().collect();
        let Some((dest, sources)) = paths.split_last().filter(|(_, s)| !s.is_empty()) else { continue };
        let dest = here.join(dest);
        let into = sources.len() > 1 || dest.is_dir();
        for s in sources {
            let Ok(s) = std::fs::canonicalize(here.join(s)) else { continue };
            let target = match into {
                true => real(&dest)?.join(s.file_name()?),
                false => real(&dest)?,
            };
            for u in unseen {
                if let Ok(rest) = u.strip_prefix(&s) {
                    let t = if rest.as_os_str().is_empty() { target.clone() } else { target.join(rest) };
                    if !out.contains(&t) {
                        out.push(t);
                    }
                }
            }
        }
    }
    Some(out)
}

/// The file's text if a card can show it as it is: text, short, and nothing that draws as something else
/// (control characters, bidi and zero-width marks).
fn showable(root: &Path, path: &std::path::Path) -> Result<String, String> {
    let shown = crate::chat::shown(root, path);
    let unseen = format!("{shown} was written this turn without the user seeing it, and");
    let bytes = std::fs::read(path).map_err(|e| format!("{shown}: {e}"))?;
    let text = String::from_utf8(bytes).map_err(|_| format!("{unseen} it isn't text the card can show"))?;
    // Said as what was crossed: too many lines, or one line too long (which one) — not both limits at once
    let lines = text.lines().count();
    let mut crossed = Vec::new();
    if lines > SHOWN_LINES_MAX {
        crossed.push(format!("it has {lines} lines — a card shows at most {SHOWN_LINES_MAX}"));
    }
    if let Some((i, l)) = text.lines().enumerate().find(|(_, l)| l.chars().count() > SHOWN_LINE_MAX) {
        crossed.push(format!(
            "its line {} is {} characters — a card shows lines of at most {SHOWN_LINE_MAX}",
            i + 1,
            l.chars().count()
        ));
    }
    if !crossed.is_empty() {
        return Err(format!(
            "{unseen} it's too long to show them on the card: {} — they review it when your turn ends: run it \
             after that, or keep the script within what a card shows",
            crossed.join(", and ")
        ));
    }
    if !draws_as_itself(&text) {
        return Err(format!("{unseen} it has characters a card can't show as they are"));
    }
    Ok(text)
}

/// Whether a card shows `text` as it is: nothing that draws as something else (control characters, bidi and
/// zero-width marks) — what a script's card asks before showing it (`showable`, `hooks.rs`).
pub(crate) fn draws_as_itself(text: &str) -> bool {
    let hidden = |c: char| {
        (c.is_control() && c != '\n' && c != '\t')
            || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
    };
    !text.replace("\r\n", "\n").chars().any(hidden)
}

/// Claude's command into the task panel (`go_now` saw it free) — one that couldn't spawn is told to its
/// callers.
fn run_start(ed: &mut Editor, who: For, cmd: String, cwd: PathBuf, guard: Option<Guard>, ride: Ride) {
    let task = crate::task::Task { name: cmd.clone(), cmd, cwd };
    if let Err(back) = ed.task_start_for(task, who, guard, ride) {
        let (who, ride) = *back;
        fail(ed, who, ride.joined, "couldn't start sh".into());
    }
}

/// Who waits for a run's end: Claude's call — or no one: the user ran it again from what the sandbox
/// blocked (`blocked.rs`), and Claude hears how it ended with the next message (`head` says what they did).
#[derive(Clone, Debug)]
pub enum For {
    Claude(Call),
    User(String),
}

/// What couldn't start, said to whoever waits for it (the calls that `joined` it too) — and its slot, still
/// free, goes to the next in line.
fn fail(ed: &mut Editor, who: For, joined: Vec<crate::queued::Joined>, e: String) {
    match who {
        For::Claude(call) => {
            crate::reads::run_ended(ed);
            for c in std::iter::once(call).chain(joined.into_iter().map(|j| j.call)) {
                answer(ed, c, Err(e.clone()));
            }
        }
        For::User(_) => ed.set_error(e),
    }
    crate::queued::run_next(ed);
}

/// A run the user started again ended: Claude reads how with the next message — the panel shows it now.
fn told(ed: &mut Editor, head: String, report: String) {
    ed.session.checkpoints.tell.push(format!("{head}\n{report}"));
    ed.note("Claude hears how it ended with your next message");
}

/// What a run may do besides what's allowed (`allow.rs`): a grant just this once, or no sandbox at all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extra {
    pub grant: Option<crate::allow::Grant>,
    pub outside: bool,
}

/// What a run may do past the settings (`with_allowed`).
#[derive(Clone, Debug, Default)]
pub struct Widened {
    /// The settings with the folders to write in and the ports added.
    pub llm: crate::llm::LlmConfig,
    /// How its result says what was allowed.
    pub granted: Vec<String>,
    /// The sockets it may connect to (no setting has those).
    pub sockets: Vec<PathBuf>,
    /// The places in home it may read (real paths — what Claude's Read was let read too).
    pub reads: Vec<PathBuf>,
}

/// The settings a run's policy is made from with what's allowed added — kept for the project, for this
/// conversation, and `once` — and what no setting has: the sockets, the places to read.
pub fn with_allowed(
    llm: &crate::llm::LlmConfig,
    scopes: &[&crate::allow::Allowed],
    once: Option<&crate::allow::Grant>,
) -> Widened {
    use crate::allow::Grant;
    let mut w = Widened { llm: llm.clone(), ..Widened::default() };
    let grants = scopes
        .iter()
        .flat_map(|a| {
            a.read
                .iter()
                .map(|p| Grant::Read(p.clone()))
                .chain(a.write.iter().map(|p| Grant::Write(p.clone())))
                .chain(a.ports.iter().map(|n| Grant::Port(*n)))
                .chain(a.sockets.iter().map(|s| Grant::Socket(s.clone())))
        })
        .chain(once.cloned());
    for g in grants {
        match &g {
            Grant::Read(p) if !w.reads.contains(p) => w.reads.push(p.clone()),
            Grant::Write(p) => w.llm.sandbox_write.push(p.display().to_string()),
            Grant::Port(n) => w.llm.sandbox_ports.push(*n),
            Grant::Socket(s) if !w.sockets.contains(s) => w.sockets.push(s.clone()),
            Grant::Read(_) | Grant::Socket(_) => {}
        }
        let said = g.shown();
        if !w.granted.contains(&said) {
            w.granted.push(said);
        }
    }
    w
}

/// The user's answer on the card: `y` runs it — the files it showed read again first (on a worker), and not
/// run if one changed since — `a`/`A` too, and what it reads outside stays allowed (this chat · always);
/// None tells Claude no. `unprotected`: the card said there's no sandbox.
pub fn run_answer(ed: &mut Editor, id: u64, what: RunCard, scope: Option<crate::allow::Scope>) {
    let RunCard { cmd, cwd, call, shown, unprotected, reads, grants, keep, outside, socket, again } = what;
    ed.offer_finished(id, Ok(()));
    let Some(scope) = scope else {
        return answer(ed, call, Err(format!("the user chose not to run `{cmd}`")));
    };
    call.step("answered yes");
    // Outside the sandbox: what it runs as it is at this yes, kept with the chat before it runs — its next
    // ask shows only what changed since (`sinceyes.rs`)
    let yes = outside.then(|| (ed.session.root.clone(), std::time::SystemTime::now()));
    // One socket past the sandbox: this run's (`y`), or this conversation's (`a` — #119) — Docker's never
    // the conversation's: its `a` is this command's (#193, below)
    let docker = socket.as_deref().is_some_and(outside_socket);
    let lasting = matches!(scope, crate::allow::Scope::ThisChat | crate::allow::Scope::Always);
    let grant = socket.clone().map(crate::allow::Grant::Socket);
    if let Some(g) = grant.as_ref().filter(|_| lasting && !docker) {
        crate::allow::grant(ed, g, crate::allow::Scope::ThisChat);
    }
    let outside = Extra { grant, outside };
    if keep {
        crate::allow::grant_reads(ed, &grants, scope);
    }
    // (read once: a read beside one later offers their folder)
    if let Some(c) = ed.session.chat.as_mut().filter(|_| scope == crate::allow::Scope::Once) {
        c.read_once.extend(reads.iter().filter(|r| !c.read_once.contains(r)).cloned().collect::<Vec<_>>());
    }
    // `a` (or `A` where reads are kept too): this exact command for the rest of this chat — never for good,
    // in this folder, with Docker's socket when it was its card's; its scripts as they are now (pinned on the
    // worker below)
    let pin = lasting && again.as_ref().is_some_and(|a| !a.scripts.is_empty());
    if let (Some(a), true) = (&again, lasting)
        && let Some(chat) = ed.session.chat.as_mut()
    {
        let r = RunAgain { cwd: cwd.clone(), socket: socket.clone().filter(|_| docker), again: a.clone() };
        if !chat.run_again.contains(&r) {
            chat.run_again.push(r);
        }
        if let Some(s) = socket.as_ref().filter(|_| docker) {
            ed.session.checkpoints.tell.push(format!(
                "The user let `{cmd}` connect to the socket {} again for the rest of this conversation — that \
                 command only, word for word (filters after it that only read may differ); any other command \
                 with it is asked.",
                crate::allow::tilde(s)
            ));
        }
        ed.note(format!(
            "this command{} runs again without asking for the rest of this chat",
            a.scripts_said()
        ));
    }
    if shown.is_empty() && !pin && yes.is_none() {
        return run_now(ed, cmd, cwd, call, unprotected, outside);
    }
    let root = ed.session.root.clone();
    ed.events.jobs().spawn(move || {
        let changed =
            shown.iter().find(|(p, text)| std::fs::read(p).ok().as_deref() != Some(text.as_bytes()));
        let changed = changed.map(|(p, _)| crate::chat::shown(&root, p));
        // (none: one isn't a text script in the checkout — `script_pins` — it runs this once only)
        let pins = pin.then(|| crate::readonly::script_pins(&cmd, &cwd, &root));
        let yes = yes.filter(|_| changed.is_none()).map(|(root, at)| crate::sinceyes::Yes {
            kept: crate::sinceyes::take(&root, &cwd, &cmd),
            key: crate::sinceyes::key(&cmd),
            cmd: cmd.clone(),
            cwd: cwd.clone(),
            at,
        });
        move |ed: &mut Editor| {
            // (kept for the chat that asked — the last yes to the same run in the same folder)
            if let Some(y) = yes
                && let Some(c) = ed.session.chat.as_mut().filter(|c| c.generation() == call.generation)
            {
                c.outside_yes.retain(|o| (&o.key, &o.cwd) != (&y.key, &y.cwd));
                c.outside_yes.push(y);
            }
            // (its scripts as they are now, for the chat that asked — one pinned before gives way)
            if let Some(Some(pins)) = &pins
                && let Some(c) = ed.session.chat.as_mut().filter(|c| c.generation() == call.generation)
            {
                c.run_scripts.retain(|(p, _)| !pins.iter().any(|(q, _)| q == p));
                c.run_scripts.extend(pins.iter().cloned());
            }
            if let Some(None) = pins {
                ed.set_warning(
                    "not allowed for the chat: what it runs by its path isn't a text script in this checkout — \
                     it runs this once",
                );
            }
            match changed {
                Some(name) => answer(
                    ed,
                    call,
                    Err(format!(
                        "{name} changed after the user saw it on the card — nothing ran; run it again to show \
                         them what's there now"
                    )),
                ),
                None => run_now(ed, cmd, cwd, call, unprotected, outside),
            }
        }
    });
}

/// A yes on its card: it runs — or, another command running (two cards answered one after the other), it
/// waits in line and runs next (#5, `queued.rs`).
fn run_now(ed: &mut Editor, cmd: String, cwd: PathBuf, call: Call, unprotected: bool, extra: Extra) {
    guard_and_go(ed, For::Claude(call), Job::Command(cmd, cwd), unprotected, extra);
}

/// A `run` card's command and where it runs, its call, the files it showed, whether it said there's no
/// sandbox, what it reads outside the project (a read-only command's), whether that can be allowed for
/// longer, and the pattern `a` allows for the rest of the chat.
#[derive(Clone, Debug)]
pub struct RunCard {
    pub cmd: String,
    pub cwd: PathBuf,
    pub call: Call,
    pub shown: Vec<(PathBuf, String)>,
    pub unprotected: bool,
    pub reads: Vec<PathBuf>,
    /// What `a`/`A` allow of `reads` — files beside each other as their folder (`allow::grant_places`).
    pub grants: Vec<PathBuf>,
    pub keep: bool,
    /// Claude asked to run it outside the sandbox (`y` runs it so, once).
    pub outside: bool,
    /// Claude asked to let it connect to this one unix socket (#119): `y` this run, `a` this chat.
    pub socket: Option<PathBuf>,
    /// What `a` lets run again without asking for the rest of the chat (`again` — this exact command).
    pub again: Option<Again>,
}

// ── The sandbox ─────────────────────────────────────────────────────────────

/// What Claude asked to run: a command and the folder it runs in, or tests.
#[derive(Clone, Debug)]
pub enum Job {
    Command(String, PathBuf),
    Tests(Box<crate::testing::Target>),
}

impl Job {
    fn label(&self) -> String {
        match self {
            Job::Command(c, _) => c.clone(),
            Job::Tests(t) => format!("tests: {}", t.label),
        }
    }
}

/// Starts looking whether there's a sandbox here, before Claude's first command (claude lists the tools
/// when it starts) — so a card for a command can say there's none.
fn sandbox_look(ed: &mut Editor) {
    if ed.sandbox != Availability::Unknown {
        return;
    }
    ed.events.jobs().spawn(|| {
        let found = crate::sandbox::probe();
        move |ed: &mut Editor| {
            if ed.sandbox == Availability::Unknown {
                ed.sandbox = found;
            }
        }
    });
}

/// Runs Claude's `job` the way `sandbox.rs` says: in the sandbox (the policy made on a worker — the
/// toolchain's folders are asked there), or, where there's none, once the user agrees: on its own card, for
/// this conversation (`Chat.unprotected`), or already (`asked`: the command's card said so).
fn guarded(ed: &mut Editor, call: Call, job: Job, asked: bool) {
    guard_and_go(ed, For::Claude(call), job, asked, Extra::default());
}

/// `guarded` for whoever waits (`For`), with `extra` — what the user allowed this run alone, or that it
/// runs outside the sandbox. The policy has what's allowed for the project and this conversation in it.
/// Its slot busy (another run, or Claude's before it in line): it waits its turn (`queued::admit`).
pub fn guard_and_go(ed: &mut Editor, who: For, job: Job, asked: bool, extra: Extra) {
    #[cfg(test)]
    if let (For::Claude(_), Job::Command(c, _)) = (&who, &job) {
        ADMITTED.with(|a| a.borrow_mut().push((c.clone(), extra.clone())));
    }
    crate::queued::admit(ed, who, job, Ride { unprotected: asked, extra, ..Default::default() });
}

/// `guard_and_go` once it's this run's turn — what rides along goes on to the run (`ride`).
pub(crate) fn guard_then_go(ed: &mut Editor, who: For, job: Job, ride: Ride) {
    // (a protected name a run of Claude's made: its next run waits for the user's look first)
    if let For::Claude(call) = &who
        && !ed.session.made_protected.is_empty()
    {
        return ask_made(ed, call.clone(), job, ride);
    }
    if ride.extra.outside {
        return go(ed, who, job, Some(Guard::Outside), ride);
    }
    if !ed.config.llm.sandbox {
        return go(ed, who, job, Some(Guard::Off), ride);
    }
    let found = ed.sandbox.clone();
    if found == Availability::Bypass {
        return go(ed, who, job, None, ride);
    }
    let llm = ed.config.llm.clone();
    let chat = ed.session.chat.as_ref().map(|c| c.allowed.clone()).unwrap_or_default();
    let (store, root) = (ed.allow_path.clone(), ed.session.root.clone());
    let grant = ride.extra.grant.clone();
    let said = ed.tokens_said.clone();
    ed.events.jobs().spawn(move || {
        let found = match found {
            Availability::Unknown => crate::sandbox::probe(),
            f => f,
        };
        let mut cargo = (Vec::new(), None);
        let policy = matches!(found, Availability::Ready | Availability::Partial(_)).then(|| {
            let kept = crate::allow::Store::read(store.as_deref()).get(&crate::sandbox::profile_path(&root));
            let w = with_allowed(&llm, &[&kept, &chat], grant.as_ref());
            let mut p = crate::sandbox::policy(&root, &w.llm, &w.reads);
            (p.granted, p.sockets) = (w.granted, w.sockets);
            // (cargo's configs holding a token: the user hears once — `tokens::tell_cargo`)
            cargo = crate::tokens::cargo_to_tell(&p.reads.copies.cargo, said.as_deref());
            // Its denials watched from before it starts (the stream up first — measured)
            p.watch = crate::sandbox::watch_for(&p);
            std::thread::sleep(crate::sandbox::watch_wait(&p));
            p
        });
        move |ed: &mut Editor| {
            ed.sandbox = found.clone();
            crate::tokens::tell_cargo(ed, &cargo.0, cargo.1);
            if let For::Claude(call) = &who {
                call.step("sandbox ready");
            }
            // (Claude's turn ended meanwhile: nothing waits for it — the slot goes to the next in line)
            if let For::Claude(call) = &who
                && ed.session.chat.as_ref().is_some_and(|c| c.generation() != call.generation || !c.busy())
            {
                return crate::queued::run_next(ed);
            }
            let agreed = ride.unprotected || ed.session.chat.as_ref().is_some_and(|c| c.unprotected);
            match (policy, found, who) {
                (Some(_), Availability::Partial(why), For::Claude(call)) if !agreed => {
                    ask_unprotected(ed, call, job, Unsandboxed::Partial(why), ride)
                }
                (Some(p), _, who) => go(ed, who, job, Some(Guard::In(Box::new(p))), ride),
                (None, Availability::Missing(why), For::Claude(call)) if !agreed => {
                    ask_unprotected(ed, call, job, Unsandboxed::Missing(why), ride)
                }
                (None, Availability::Missing(why), who) => go(ed, who, job, Some(Guard::Without(why)), ride),
                (None, _, who) => go(ed, who, job, None, ride),
            }
        }
    });
}

/// It runs what's on disk: the user's unsaved files are saved, and it starts once they're written
/// (`after_saves` — an event, not a wait).
fn go(ed: &mut Editor, who: For, job: Job, guard: Option<Guard>, ride: Ride) {
    ed.auto_save(match &job {
        Job::Command(..) => " · before the command",
        Job::Tests(_) => " · before test",
    });
    let turn = |ed: &Editor, call: &Call| {
        ed.session.chat.as_ref().is_some_and(|c| c.generation() == call.generation && c.busy())
    };
    let asked_in_turn = matches!(&who, For::Claude(call) if turn(ed, call));
    ed.after_saves(move |ed| {
        // (Claude's turn ended while the saves were written: nothing waits for it — nor holds the slot)
        if asked_in_turn
            && let For::Claude(call) = &who
            && !turn(ed, call)
        {
            return crate::queued::run_next(ed);
        }
        // A run of Claude's — a check or its tests too (a test that deletes files is what undo is for) — but
        // not a command that only reads (two snapshots cost ~110 ms in a small checkout, and those are most of
        // its runs): the checkout as it is now, first — what it changes joins the turn (`snapshot.rs`). After
        // the saves: what the user saved isn't the run's.
        let reads = matches!(&job, Job::Command(cmd, _) if crate::readonly::read_only(cmd));
        // (its slot taken meanwhile: back in line now, its snapshot when its turn comes)
        match &who {
            For::Claude(call) if !reads && !slot_taken(ed, &job) => {
                call.step("snapshot");
                let root = ed.session.root.clone();
                ed.events.jobs().spawn(move || {
                    let snap = crate::snapshot::Before::take(&root);
                    move |ed: &mut Editor| {
                        if let For::Claude(call) = &who {
                            call.step("snapshot taken");
                            if asked_in_turn && !turn(ed, call) {
                                return crate::queued::run_next(ed);
                            }
                        }
                        go_now(ed, who, job, guard, Ride { snap: Some(snap), ..ride })
                    }
                });
            }
            _ => go_now(ed, who, job, guard, ride),
        }
    });
}

/// It starts — unless its slot was taken while it got ready (the user's task, a race with another call of
/// Claude's): then it's back at the front of its line, its sandbox made again when its turn comes.
fn go_now(ed: &mut Editor, who: For, job: Job, guard: Option<Guard>, ride: Ride) {
    if slot_taken(ed, &job) {
        drop(guard);
        return crate::queued::again(ed, crate::queued::What::Guarded { who, job, ride }, None);
    }
    if matches!(who, For::Claude(_)) {
        let dir = match &guard {
            Some(Guard::In(p)) => p.run.clone(),
            _ => None,
        };
        crate::reads::run_started(ed, dir);
    }
    match job {
        Job::Command(cmd, cwd) => run_start(ed, who, cmd, cwd, guard, ride),
        Job::Tests(target) => start_tests(ed, who, *target, guard, ride),
    }
}

/// Whether `job`'s slot has a run under way (the task panel, the test pane).
fn slot_taken(ed: &Editor, job: &Job) -> bool {
    match job {
        Job::Command(..) => {
            ed.session.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running)
        }
        Job::Tests(_) => ed.session.test_run.as_ref().is_some_and(|r| r.state == RunState::Running),
    }
}

pub const NO_SANDBOX_TITLE: &str = "Claude wants to run — without a sandbox";
pub const PARTIAL_SANDBOX_TITLE: &str = "Claude wants to run — in a partial sandbox";

/// Why a run of Claude's asks before it runs (`What::NoSandbox`): no sandbox here (why), or one that goes
/// only so far (why — Linux's Landlock below ABI 3), the run going in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unsandboxed {
    Missing(String),
    Partial(String),
}

/// Claude's command waits for the user (`kind` says why) — `y` this once, `a` for this conversation, `n` no.
fn ask_unprotected(ed: &mut Editor, call: Call, job: Job, kind: Unsandboxed, ride: Ride) {
    use crate::cards::{Past, Request, Why};
    let label = job.label();
    let (title, question, why, request) = match &kind {
        Unsandboxed::Missing(why) => (
            NO_SANDBOX_TITLE,
            format!(
                "{label}\n\nThere's none here ({why}) — it would run with all your permissions: your files, your \
                 network."
            ),
            Why::NoSandbox,
            Request::Run { past: Past::No, sandboxed: false, reads: Vec::new() },
        ),
        Unsandboxed::Partial(why) => (
            PARTIAL_SANDBOX_TITLE,
            format!("{label}\n\nThe sandbox here goes only so far: {why}. It runs in it as far as it goes."),
            Why::NoSandbox,
            Request::Run { past: Past::No, sandboxed: false, reads: Vec::new() },
        ),
    };
    call.step("no-sandbox card up");
    let what = crate::offer::What::NoSandbox { job, call, ride: Box::new(ride), kind };
    let asked = crate::cards::Asked::new(why, request);
    let o = crate::offer::Offer::new(title.into(), &question, "", String::new(), what);
    ed.push_offer(o.asked(asked));
}

/// The answer on that card: `y` runs it — without a sandbox, or in the partial one — `a` too, and the rest
/// of this conversation's, `n` tells Claude no (and the calls that joined it).
pub fn unprotected_answer(
    ed: &mut Editor,
    id: u64,
    (job, call, ride, kind): (Job, Call, Ride, Unsandboxed),
    answer_is: Unprotected,
) {
    ed.offer_finished(id, Ok(()));
    if answer_is != Unprotected::No {
        call.step("answered yes");
    }
    if answer_is == Unprotected::No {
        let e = match &kind {
            Unsandboxed::Missing(why) => format!(
                "There's no sandbox on this system ({why}), and the user chose not to run `{}` without one — \
                 ask them how to go on",
                job.label()
            ),
            Unsandboxed::Partial(why) => format!(
                "The sandbox here goes only so far ({why}), and the user chose not to run `{}` in it — ask them \
                 how to go on",
                job.label()
            ),
        };
        for c in std::iter::once(call).chain(ride.joined.into_iter().map(|j| j.call)) {
            answer(ed, c, Err(e.clone()));
        }
        return crate::queued::run_next(ed);
    }
    let chat = answer_is == Unprotected::ThisChat;
    if chat && let Some(c) = ed.session.chat.as_mut() {
        c.unprotected = true;
    }
    match kind {
        Unsandboxed::Missing(why) => {
            if chat {
                ed.set_warning("Claude's commands run without a sandbox in this conversation");
            }
            go(ed, For::Claude(call), job, Some(Guard::Without(why)), ride)
        }
        // (in the sandbox: made again for it, now agreed)
        Unsandboxed::Partial(_) => {
            if chat {
                ed.set_warning("Claude's commands run in the partial sandbox unasked in this conversation");
            }
            guard_then_go(ed, For::Claude(call), job, Ride { unprotected: true, ..ride })
        }
    }
}

pub const MADE_TITLE: &str = "Claude wants to run — after it made a protected file";

/// The protected names a run made, named — `(a.toml, a name` · `a, b — names`, then `it` · `them`) — for
/// what follows: "… the sandbox keeps".
fn made_named(made: &[String]) -> (String, &'static str) {
    match made.len() {
        1 => (format!("{}, a name", made[0]), "it"),
        _ => (format!("{} — names", some_named(made)), "them"),
    }
}

/// Claude's next run waits for the user (`What::ProtectedMade`): its last run made protected names where none
/// was (`Session.made_protected`) — what Landlock couldn't refuse, and what runs outside the sandbox later.
fn ask_made(ed: &mut Editor, call: Call, job: Job, ride: Ride) {
    use crate::cards::{Past, Request, Why};
    let label = job.label();
    let made = ed.session.made_protected.clone();
    let (named, it) = made_named(&made);
    let among = if made.len() == 1 { "it's" } else { "they're" };
    let question = format!(
        "{label}\n\nIts last run made {named} the sandbox keeps from Claude's commands: what's in {it} runs \
         outside the sandbox, in your builds and your terminal. The sandbox couldn't refuse {it}: none was \
         there yet.\n\nLook at {it} before this runs — {among} among this turn's changes (space u takes the \
         turn back)."
    );
    let at = label.split('\n').count() + 1;
    call.step("protected-made card up");
    let what = crate::offer::What::ProtectedMade { job, call, ride: Box::new(ride), made };
    let asked = crate::cards::Asked::new(
        Why::ProtectedMade,
        Request::Run { past: Past::No, sandboxed: true, reads: Vec::new() },
    );
    let o = crate::offer::Offer::new(MADE_TITLE.into(), &question, "", String::new(), what);
    ed.push_offer(o.asked(asked).warning(at..at + 1));
}

/// The answer on that card: `y` runs it (the names seen — not asked about again), `n` tells Claude no (and
/// the calls that joined it) — asked again before its next run.
pub fn made_answer(
    ed: &mut Editor,
    id: u64,
    (job, call, ride, made): (Job, Call, Ride, Vec<String>),
    yes: bool,
) {
    ed.offer_finished(id, Ok(()));
    if yes {
        call.step("answered yes");
        ed.session.made_protected.retain(|p| !made.contains(p));
        return guard_then_go(ed, For::Claude(call), job, ride);
    }
    let (named, it) = made_named(&made);
    let e = format!(
        "Your last run made {named} the sandbox keeps (what's in {it} runs outside it), and the user chose not \
         to run `{}` after it — ask them how to go on",
        job.label()
    );
    for c in std::iter::once(call).chain(ride.joined.into_iter().map(|j| j.call)) {
        answer(ed, c, Err(e.clone()));
    }
    crate::queued::run_next(ed)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unprotected {
    Once,
    ThisChat,
    No,
}

/// Whether Claude's commands run without a sandbox now — the chat's title says so (a partial one agreed
/// for the conversation is still a sandbox).
pub fn unsandboxed(ed: &Editor) -> bool {
    let partial = matches!(ed.sandbox, Availability::Partial(_));
    !ed.config.llm.sandbox || ed.session.chat.as_ref().is_some_and(|c| c.unprotected && !partial)
}

/// The command Claude ran ended — how, and its output, go back; the editor shows the first error. One the
/// user ran again (`blocked.rs`): Claude hears how it ended with the next message.
pub fn run_done(ed: &mut Editor) {
    let Some(r) = ed.session.task_run.as_mut() else { return };
    // (its own places — the user's list has them only when it was a check, #28)
    if let Some(head) = r.tell.take() {
        let r = &*r;
        let text = run_report(&ed.session.root, r, &r.places, Waited::No);
        return told(ed, head, text);
    }
    let joined = std::mem::take(&mut r.joined);
    let Some(call) = r.chat.take() else { return };
    let snap = r.snap.take();
    let r = &*r;
    let items = &r.places;
    let text = run_report(&ed.session.root, r, items, Waited::of(r.started, r.queued.as_ref(), None));
    // (each call that joined it: the same run, its own wait)
    let others: Vec<(Call, String)> = joined
        .into_iter()
        .map(|j| {
            let how = Waited::of(r.started, r.queued.as_ref(), Some((j.asked, j.running)));
            (j.call, run_report(&ed.session.root, r, items, how))
        })
        .collect();
    let first =
        items.iter().find(|i| i.severity == 1).filter(|_| r.state != crate::task::State::Passed).cloned();
    let (label, cmd) = (r.task.name.clone(), r.task.cmd.clone());
    crate::reads::run_ended(ed);
    let generation = call.generation;
    let told = move |ed: &mut Editor, changed: String| {
        answer(ed, call, Ok(format!("{text}{changed}")));
        for (c, text) in others {
            answer(ed, c, Ok(format!("{text}{changed}")));
        }
        if let Some(it) = first {
            crate::follow::point(ed, "failed", label, &it.path, it.line..it.line + 1, Some(it.message));
        }
    };
    match snap {
        Some(snap) => run_changes(ed, snap, generation, cmd, told),
        None => told(ed, String::new()),
    }
}

/// A run Claude waited on is gone before it ended (stopped, or another task took its place).
pub fn run_dropped(ed: &mut Editor, why: &str) {
    let Some(r) = ed.session.task_run.as_mut().filter(|r| r.state == crate::task::State::Running) else {
        return;
    };
    let joined = std::mem::take(&mut r.joined);
    // (stopped after minutes of nothing: Claude hears that too — #157)
    let silent = crate::waits::silent_line(r.last_output, Instant::now())
        .map(|s| format!(". {s}"))
        .unwrap_or_default();
    let (snap, cmd) = (r.snap.take(), r.task.cmd.clone());
    if let Some(call) = r.chat.take() {
        crate::reads::run_ended(ed);
        let generation = call.generation;
        for c in std::iter::once(call).chain(joined.into_iter().map(|j| j.call)) {
            answer(ed, c, Err(format!("the command was stopped — {why}{silent}")));
        }
        // (what it changed before it was stopped is the turn's all the same)
        if let Some(snap) = snap {
            run_changes(ed, snap, generation, cmd, |_: &mut Editor, _| {});
        }
    }
}

/// Files a report names at most (the rest counted).
const CHANGED_NAMED: usize = 12;

/// `names`, at most `CHANGED_NAMED` of them, the rest counted (`a, b and 3 more`).
fn some_named(names: &[String]) -> String {
    let mut said = names.iter().take(CHANGED_NAMED).cloned().collect::<Vec<_>>().join(", ");
    if names.len() > CHANGED_NAMED {
        said.push_str(&format!(" and {} more", names.len() - CHANGED_NAMED));
    }
    said
}

/// What Claude's run `cmd` (a command, or `tests: …`) changed in the checkout since `snap` (`snapshot.rs`) —
/// taken on a worker, then recorded in the turn it ran in (chat process `generation`), or as a checkpoint of
/// its own when that turn is over. `told` gets what Claude reads of it (nothing when nothing changed) — and
/// the ignored files and folders it deleted, named, which nothing takes back; then the line's next run goes
/// (held for this — `task.rs`, `testing.rs`).
fn run_changes(
    ed: &mut Editor,
    snap: crate::snapshot::Before,
    generation: u64,
    cmd: String,
    told: impl FnOnce(&mut Editor, String) + Send + 'static,
) {
    ed.events.jobs().spawn(move || {
        let found = snap.since();
        move |ed: &mut Editor| {
            let said = match found {
                Ok(crate::snapshot::Found { files, skipped, left_out, ignored_gone, protected })
                    if !files.is_empty()
                        || !skipped.is_empty()
                        || left_out > 0
                        || !ignored_gone.is_empty()
                        || !protected.is_empty() =>
                {
                    let root = ed.session.root.clone();
                    let named: Vec<String> = files
                        .iter()
                        .map(|c| {
                            let name = crate::chat::shown(&root, &c.path);
                            match (&c.before, &c.after) {
                                (None, _) => format!("{name} (new)"),
                                (_, None) => format!("{name} (deleted)"),
                                _ => name,
                            }
                        })
                        .collect();
                    let said = some_named(&named);
                    let turn =
                        ed.session.chat.as_ref().is_some_and(|c| c.generation() == generation && c.busy());
                    if turn && !files.is_empty() {
                        crate::checkpoint::record_run(ed, files);
                    } else if !files.is_empty()
                        && let Some(n) = crate::checkpoint::record_run_alone(ed, &format!("ran `{cmd}`"), files)
                        && let Some(c) = &mut ed.session.chat
                    {
                        let row = format!("its run changed {n} after the turn · space v t · space u undo");
                        c.msgs.push(crate::chat::Msg::new(crate::chat::Role::Note, row, ""));
                    }
                    let mut out = String::new();
                    if !named.is_empty() {
                        out.push_str(&format!(
                            "\nIt changed in the checkout: {said} — part of this turn's changes, which the \
                             user reviews and can take back.\n"
                        ));
                    }
                    if !skipped.is_empty() {
                        out.push_str(&format!("Changed, and not taken back with the turn: {}.\n", skipped.join(", ")));
                    }
                    if left_out > 0 {
                        out.push_str(&format!(
                            "{left_out} new untracked files in the checkout (not ignored) weren't kept — too many at \
                             once: its changes to them can't be taken back.\n"
                        ));
                    }
                    if !ignored_gone.is_empty() {
                        out.push_str(&format!(
                            "It deleted ignored files, which aren't kept — the user can't take them back: {}.\n",
                            some_named(&ignored_gone)
                        ));
                    }
                    // (kept for the card before its next run — Landlock couldn't refuse them)
                    if !protected.is_empty() {
                        let (named, it) = made_named(&protected);
                        out.push_str(&format!(
                            "It made {named} the sandbox keeps from your commands: what's in {it} runs later \
                             outside it (the user's builds, their terminal). The sandbox couldn't refuse {it} \
                             (none was there yet); the user is asked about {it} before your next command or \
                             test runs. To change one, use Edit.\n"
                        ));
                        for p in protected {
                            if !ed.session.made_protected.contains(&p) {
                                ed.session.made_protected.push(p);
                            }
                        }
                    }
                    out
                }
                Ok(_) => String::new(),
                Err(e) => {
                    let line = format!("snapshot: what `{cmd}` changed can't be told — {e}");
                    crate::log::line("tarae", "info", &line);
                    String::new()
                }
            };
            told(ed, said);
            crate::queued::run_next(ed);
        }
    });
}

/// Places reported at most; output lines sent whole up to `OUTPUT_WHOLE`, else its first `OUTPUT_HEAD` and
/// last `OUTPUT_TAIL` lines (the errors come first, the summary last).
const PLACES_MAX: usize = 30;
const OUTPUT_WHOLE: usize = 300;
const OUTPUT_HEAD: usize = 100;
const OUTPUT_TAIL: usize = 150;
/// Characters per output line, at most (a minified bundle on one line).
const LINE_MAX: usize = 500;

/// How the command ended as Claude reads it: the verdict, what it waited on (`waited` — in line, then on
/// another's lock), the places it pointed at (from `root`, the project's), its output.
fn run_report(root: &Path, r: &crate::task::Run, places: &[crate::quickfix::Item], waited: Waited) -> String {
    use crate::task::State;
    let took = r.took.map(|d| format!(" in {:.1}s", d.as_secs_f32())).unwrap_or_default();
    let verdict = match r.state {
        State::Passed => "exit 0".to_string(),
        State::Failed(Some(code)) => format!("exit {code}"),
        State::Failed(None) => "killed".to_string(),
        State::Stopped(ref why) => format!("stopped: {why}"),
        State::Running => "still running".to_string(),
    };
    // (how long it waited in line, and how much of it on what isn't its own — another cargo's lock —
    // `waits.rs`)
    let waited = waited.line(&r.wait, r.took);
    // (silent at its end for minutes: said — #157)
    let silent = r
        .took
        .and_then(|t| crate::waits::silent_line(r.last_output, r.started + t))
        .map(|s| format!("{s}\n"))
        .unwrap_or_default();
    let mut out = format!("{verdict}{took}\n{waited}{silent}$ {}\n", r.task.cmd);
    let (errors, warnings) = r.found;
    // Where its places are said as they are (#38): a check's or a build's went to the user's list; any
    // other run's (a grep's `path:line:` hits) are only in its output — not the user's, not errors
    if errors + warnings > 0 && !places.is_empty() && r.feeds {
        out.push_str(&format!("\n{errors} errors, {warnings} warnings in the user's list (]q):\n"));
    } else if !places.is_empty() {
        let n = places.len();
        out.push_str(&format!(
            "\n{n} place{} in its output (not in the user's list — only checks and builds go there):\n",
            if n == 1 { "" } else { "s" }
        ));
    }
    if !places.is_empty() && (errors + warnings > 0 || !r.feeds) {
        for it in places.iter().take(PLACES_MAX) {
            let kind = match it.severity {
                _ if !r.feeds => "",
                1 => "error: ",
                2 => "warning: ",
                _ => "note: ",
            };
            out.push_str(&format!(
                "  {}:{}:{} {kind}{}\n",
                crate::chat::shown(root, &it.path),
                it.line + 1,
                it.col + 1,
                it.message
            ));
        }
    }
    // Tests that skipped themselves (`test_results::SELF_SKIP`) — libtest calls them ok
    let skipped: Vec<&str> =
        r.output.iter().filter_map(|(_, l)| l.strip_prefix(crate::test_results::SELF_SKIP)).collect();
    if !skipped.is_empty() {
        out.push_str(&format!(
            "\n{} test{} skipped {} — libtest says ok, but nothing was checked:\n",
            skipped.len(),
            if skipped.len() == 1 { "" } else { "s" },
            if skipped.len() == 1 { "itself" } else { "themselves" }
        ));
        for s in skipped {
            out.push_str(&format!("  {s}\n"));
        }
    }
    let lines = &r.output;
    let guard = r.guard.as_ref().map(|g| format!("\n{}\n", g.report(&r.output))).unwrap_or_default();
    if lines.is_empty() {
        out.push_str("\n(no output)\n");
        out.push_str(&guard);
        return out;
    }
    let cut = |l: &str| -> String {
        if l.len() <= LINE_MAX {
            return l.to_string();
        }
        let mut end = LINE_MAX;
        while !l.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &l[..end])
    };
    out.push_str("\nOutput:\n");
    if lines.len() <= OUTPUT_WHOLE {
        for (_, l) in lines {
            out.push_str(&cut(l));
            out.push('\n');
        }
    } else {
        for (_, l) in &lines[..OUTPUT_HEAD] {
            out.push_str(&cut(l));
            out.push('\n');
        }
        out.push_str(&format!("… {} lines …\n", lines.len() - OUTPUT_HEAD - OUTPUT_TAIL));
        for (_, l) in &lines[lines.len() - OUTPUT_TAIL..] {
            out.push_str(&cut(l));
            out.push('\n');
        }
    }
    out.push_str(&guard);
    out
}

/// A run's report in a word or two — the chat row's suffix (`ok` · `exit 101` · `not run`).
pub fn run_verdict(result: &str) -> String {
    match result.split([' ', '\n']).next().unwrap_or_default() {
        "exit" if result.starts_with("exit 0") => "ok".into(),
        "exit" => result.split_whitespace().take(2).collect::<Vec<_>>().join(" "),
        "killed" => "killed".into(),
        _ => "not run".into(),
    }
}

/// Failed cases reported in full, at most (the rest by name, up to `NAMES_MAX`); the rest of the output's
/// tail when nothing says why.
const FAILURES_MAX: usize = 10;
const MESSAGE_MAX: usize = 40;
const NAMES_MAX: usize = 50;
const TAIL: usize = 80;

/// The results as Claude reads them: the verdict, counts, each failure with where and why — and the
/// output's tail when the run failed with no failed test to show for it (a build error). Paths from `root`;
/// `waited`: its wait before it ran.
fn report(root: &Path, r: &TestRun, waited: Waited) -> String {
    let took = r.took.map(|d| format!(" in {:.1}s", d.as_secs_f32())).unwrap_or_default();
    let verdict = match r.state {
        RunState::Passed => "passed".to_string(),
        RunState::Failed(Some(code)) => format!("failed (exit {code})"),
        RunState::Failed(None) => "failed (killed)".to_string(),
        RunState::Running => "still running".to_string(),
        RunState::NoTests => "no tests ran".to_string(),
    };
    // (how long it waited in line, and how much of it on what isn't its own — another cargo's lock —
    // `waits.rs`)
    let waited = waited.line(&r.wait, r.took);
    let silent = r
        .took
        .and_then(|t| crate::waits::silent_line(r.last_output, r.started + t))
        .map(|s| format!("{s}\n"))
        .unwrap_or_default();
    let mut out = format!(
        "{verdict}{took} — {}\n{waited}{silent}$ {}  (in {})\n",
        r.target.label,
        r.target.argv().join(" "),
        crate::chat::shown(root, &r.target.cwd)
    );
    if r.state == RunState::NoTests {
        out.push_str("The run ended without running a test — nothing matched what it asked for (the output says what the runner saw).\n");
    }
    let count = |s: Status| r.cases.iter().filter(|c| c.status == s).count();
    let counts: Vec<String> =
        [(Status::Passed, "passed"), (Status::Failed, "failed"), (Status::Skipped, "skipped")]
            .into_iter()
            .map(|(s, name)| (count(s), name))
            .filter(|(n, _)| *n > 0)
            .map(|(n, name)| format!("{n} {name}"))
            .collect();
    if !counts.is_empty() {
        out.push_str(&counts.join(", "));
        out.push('\n');
    }
    let fails = r.failures();
    out.push_str(&failures(root, &r.cases, &fails));
    // Skipped with a why: they skipped themselves (`test_results::SELF_SKIP`) — nothing was checked
    let said: Vec<_> =
        r.cases.iter().filter(|c| c.status == Status::Skipped && !c.message.is_empty()).collect();
    if !said.is_empty() {
        out.push_str("\nSkipped — nothing was checked:\n");
        for c in said {
            out.push_str(&format!("  {} — {}\n", c.name, c.message.join(" ")));
        }
    }
    if r.state != RunState::Passed && fails.is_empty() {
        let from = r.output.len().saturating_sub(TAIL);
        out.push_str(&format!("\nOutput (last {} lines):\n", r.output.len() - from));
        for (_, line) in &r.output[from..] {
            out.push_str(line);
            out.push('\n');
        }
    }
    if let Some(g) = &r.guard {
        out.push_str(&format!("\n{}\n", g.report(&r.output)));
    }
    out
}

/// The failed cases (indices into `cases`): the first `FAILURES_MAX` with where and why, the rest by name
/// and place — so every failure is known, but the long messages stay few.
fn failures(root: &Path, cases: &[crate::test_results::Case], fails: &[usize]) -> String {
    let at = |c: &crate::test_results::Case| {
        c.at.as_ref()
            .map(|(p, l)| format!(" at {}:{}", crate::chat::shown(root, p), l + 1))
            .unwrap_or_default()
    };
    let mut out = String::new();
    for &i in fails.iter().take(FAILURES_MAX) {
        let c = &cases[i];
        out.push_str(&format!("\nFAILED {}{}\n", c.name, at(c)));
        for m in c.message.iter().take(MESSAGE_MAX) {
            out.push_str(&format!("  {m}\n"));
        }
    }
    let rest = fails.get(FAILURES_MAX..).unwrap_or_default();
    if !rest.is_empty() {
        out.push_str(&format!("\n{} more failed (names only):\n", rest.len()));
        for &i in rest.iter().take(NAMES_MAX) {
            out.push_str(&format!("  {}{}\n", cases[i].name, at(&cases[i])));
        }
        if rest.len() > NAMES_MAX {
            out.push_str(&format!("  … and {} more\n", rest.len() - NAMES_MAX));
        }
    }
    out
}

/// A report in a few words — the chat row's suffix (`2 passed` · `1 failed` · `failed` — a build error).
pub fn verdict(result: &str) -> String {
    let mut lines = result.lines();
    let head = lines.next().unwrap_or_default();
    // (the line after the command: the counts, when any test was seen)
    let counts = lines.nth(1).filter(|l| ["passed", "failed", "skipped"].iter().any(|w| l.contains(w)));
    let count = |what: &str| counts?.split(", ").find(|c| c.ends_with(what)).map(str::to_string);
    if head.starts_with("passed") {
        count("passed").unwrap_or_else(|| "passed".into())
    } else if head.starts_with("no tests ran") {
        "no tests ran".into()
    } else if head.starts_with("failed") {
        count("failed").unwrap_or_else(|| "failed".into())
    } else {
        "not run".into()
    }
}

#[cfg(test)]
mod tests {
    /// A project root none of the reports' paths are under (they're shown whole).
    fn elsewhere() -> &'static std::path::Path {
        std::path::Path::new("/elsewhere")
    }

    #[test]
    fn verdict_is_the_report_in_a_few_words() {
        let v = super::verdict;
        assert_eq!(
            v("failed (exit 101) in 1.2s — tests::adds\n$ cargo test\n1 passed, 2 failed\n"),
            "2 failed"
        );
        assert_eq!(v("passed in 0.4s — a.rs\n$ cargo test\n3 passed\n"), "3 passed");
        assert_eq!(v("failed (exit 101) in 3s — a.rs\n$ cargo test\n\nOutput (last 9 lines):\n"), "failed");
        assert_eq!(v("the test run was stopped — the user closed it"), "not run");
        assert_eq!(v("no tests ran in 0.3s — a::b\n$ cargo test -- a::b --exact\n"), "no tests ran");
    }

    /// The server through both revisions' sequences: the handshake (`initialize` →
    /// `notifications/initialized` → `tools/list` → a call, results plain) and the stateless one
    /// (`server/discover`, then requests naming their version in `_meta`, nothing before them; results
    /// with `resultType`, serverInfo, and the list's caching) — a version it doesn't speak is refused, and
    /// a waited call's late answer is in its request's revision too.
    #[test]
    fn the_tools_server_speaks_both_revisions() {
        use crate::mcp::{Rev, SUPPORTED, UNSUPPORTED_VERSION};
        use serde_json::{Value, json};
        const VERSION: &str = "io.modelcontextprotocol/protocolVersion";
        const INFO: &str = "io.modelcontextprotocol/serverInfo";
        let names = |r: &Value| -> Vec<String> {
            let tools = r["result"]["tools"].as_array().expect("tools");
            tools.iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
        };
        let annotations = |r: &Value, name: &str| -> Value {
            let tools = r["result"]["tools"].as_array().unwrap();
            tools.iter().find(|t| t["name"] == name).unwrap()["annotations"].clone()
        };

        // The handshake, as claude 2.1.283 speaks it
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        let mut send = |msg: Value| super::mcp(&mut ed, 0, "c1", &msg).expect("answered now");
        let init = send(json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": { "protocolVersion": "2025-11-25", "capabilities": {},
                        "clientInfo": { "name": "claude-code", "version": "2.1.283" } } }));
        assert_eq!(init["result"]["protocolVersion"], "2025-11-25", "{init}");
        assert_eq!(init["result"]["serverInfo"]["name"], "tarae");
        assert!(init["result"].get("resultType").is_none(), "plain: {init}");
        let older = send(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2025-03-26" } }));
        assert_eq!(older["result"]["protocolVersion"], "2025-03-26", "an older one we speak: {older}");
        let unknown = send(json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2024-10-07" } }));
        assert_eq!(unknown["result"]["protocolVersion"], "2025-11-25", "never an echo: {unknown}");
        send(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));
        let list = send(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }));
        let old_names = names(&list);
        assert_eq!(old_names.first().map(String::as_str), Some("note"), "{old_names:?}");
        assert!(list["result"].get("ttlMs").is_none() && list["result"].get("resultType").is_none());
        // Annotations: a title for each, read-only and destructive hints
        for t in list["result"]["tools"].as_array().unwrap() {
            assert!(t["annotations"]["title"].is_string(), "{t}");
        }
        for name in ["references", "search", "symbols", "definition", "hover", "diagnostics", "screen", "ui"]
        {
            assert_eq!(annotations(&list, name)["readOnlyHint"], true, "{name}");
        }
        for name in ["run", "rename", "replace"] {
            let a = annotations(&list, name);
            assert!(a["destructiveHint"] == true && a.get("readOnlyHint").is_none(), "{name}: {a}");
        }
        let call = send(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "nope", "arguments": {} } }));
        assert_eq!(call["result"]["isError"], true, "{call}");
        assert!(call["result"].get("resultType").is_none());
        let ping = send(json!({ "jsonrpc": "2.0", "id": 4, "method": "ping" }));
        assert_eq!(ping["result"], json!({}), "kept for the older revisions");

        // Stateless: a fresh server, no initialize — discover, then each request says its version
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        let mut send = |msg: Value| super::mcp(&mut ed, 0, "c1", &msg).expect("answered now");
        let meta = |v: &str| json!({ VERSION: v, "io.modelcontextprotocol/clientCapabilities": {} });
        let found = send(json!({ "jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {} }));
        assert_eq!(found["result"]["supportedVersions"], json!(SUPPORTED), "{found}");
        assert_eq!(found["result"]["capabilities"], json!({ "tools": {} }));
        assert_eq!(found["result"]["_meta"][INFO]["name"], "tarae");
        assert!(found["result"].get("serverInfo").is_none(), "only in _meta: {found}");
        assert_eq!(found["result"]["resultType"], "complete");
        assert_eq!(found["result"]["cacheScope"], "private", "cacheable, as the lists");
        assert!(found["result"]["ttlMs"].as_u64().is_some_and(|t| t > 0));
        let list = send(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list",
            "params": { "_meta": meta("2026-07-28") } }));
        assert_eq!(list["result"]["resultType"], "complete", "{list}");
        assert_eq!(list["result"]["_meta"][INFO]["name"], "tarae");
        assert_eq!(list["result"]["cacheScope"], "private");
        assert!(list["result"]["ttlMs"].as_u64().is_some_and(|t| t > 0));
        assert_eq!(names(&list), old_names, "the same tools, in the same order");
        let call = send(json!({ "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "nope", "arguments": {}, "_meta": meta("2026-07-28") } }));
        assert_eq!(call["result"]["resultType"], "complete", "{call}");
        assert_eq!(call["result"]["isError"], true);
        assert!(call["result"].get("ttlMs").is_none(), "only lists hold");
        let ping = send(json!({ "jsonrpc": "2.0", "id": 4, "method": "ping",
            "params": { "_meta": meta("2026-07-28") } }));
        assert_eq!(ping["error"]["code"], -32601, "gone in 2026-07-28: {ping}");
        let refused = send(json!({ "jsonrpc": "2.0", "id": 5, "method": "tools/list",
            "params": { "_meta": meta("2031-01-01") } }));
        assert_eq!(refused["error"]["code"], UNSUPPORTED_VERSION, "{refused}");
        assert_eq!(refused["error"]["data"]["requested"], "2031-01-01");
        assert!(refused.get("result").is_none());
        // An older version named in _meta: plain results
        let list = send(json!({ "jsonrpc": "2.0", "id": 6, "method": "tools/list",
            "params": { "_meta": meta("2025-06-18") } }));
        assert!(list["result"].get("resultType").is_none(), "{list}");

        // A waited call answers in its request's revision
        let mut late = super::Call::test(9);
        assert!(super::response(&late, Ok("done".into()))["result"].get("resultType").is_none());
        late.rev = Rev::Stateless;
        let r = super::response(&late, Ok("done".into()));
        assert_eq!(r["result"]["resultType"], "complete", "{r}");
        assert_eq!(r["result"]["content"][0]["text"], "done");
        assert_eq!(r["id"], 9);
    }

    /// Which MCP revision the installed claude speaks to tarae's tools server — with no user message, so no
    /// model call (it registers and lists the tools at startup). Fails when a CLI upgrade changes it, saying
    /// to what: then check `mcp.rs` (a revision we don't speak means no tools in the chat) and move `KNOWN`.
    /// A local check: it needs claude installed (and logged in), which CI's runners don't have — the network
    /// job skips it by name (`.github/workflows/ci.yml`).
    #[test]
    #[ignore = "needs claude installed; a local check, not run on CI — cargo test -- --ignored installed_claude"]
    fn installed_claude_speaks_a_revision_we_speak() {
        use serde_json::{Value, json};
        use std::io::{BufRead, Write};
        use std::process::{Command, Stdio};
        /// Measured 2026-10-03, claude 2.1.283.
        const KNOWN: &str = "2025-11-25";
        let dir = std::env::temp_dir().join(format!("tarae-mcp-probe-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let version = Command::new("claude")
            .arg("--version")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        let Ok(version) = version else { panic!("no claude on PATH") };
        let mut cmd = Command::new("claude");
        cmd.args(["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose"])
            .args(["--tools", "", "--no-session-persistence", "--strict-mcp-config", "--setting-sources", ""])
            .current_dir(&dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        // (not the running Claude Code's variables — a nested claude would take them as its own)
        for (k, _) in std::env::vars_os() {
            if k.to_string_lossy().starts_with("CLAUDE") {
                cmd.env_remove(k);
            }
        }
        let mut child = cmd.spawn().expect("claude starts");
        let mut stdin = child.stdin.take().unwrap();
        writeln!(stdin, "{}", crate::llm::initialize_message(&["tarae"])).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let out = std::io::BufReader::new(child.stdout.take().unwrap());
        std::thread::spawn(move || {
            for line in out.lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    return;
                }
            }
        });
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        // (method, the version it named — initialize's, or _meta's)
        let mut seen: Vec<(String, Option<String>)> = Vec::new();
        let mut listed = false;
        while !listed {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            let Ok(line) = rx.recv_timeout(left) else { break };
            let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
            let req = &v["request"];
            if v["type"] != "control_request" || req["subtype"] != "mcp_message" {
                continue;
            }
            let msg = &req["message"];
            let method = msg["method"].as_str().unwrap_or_default().to_string();
            let named = msg["params"]["protocolVersion"]
                .as_str()
                .or(msg["params"]["_meta"]["io.modelcontextprotocol/protocolVersion"].as_str());
            seen.push((method.clone(), named.map(str::to_string)));
            let request_id = v["request_id"].as_str().unwrap_or_default();
            let answer = super::mcp(&mut ed, 0, request_id, msg).expect("answered now");
            let line = crate::llm::control_response(request_id, json!({ "mcp_response": answer }));
            writeln!(stdin, "{line}").unwrap();
            listed = method == "tools/list" && answer["result"]["tools"].is_array();
        }
        let _ = child.kill();
        let _ = child.wait();
        std::fs::remove_dir_all(&dir).ok();
        eprintln!("MCP {version}: {seen:?}");
        assert!(listed, "{version} never listed tarae's tools: {seen:?}");
        let spoken = seen.iter().find_map(|(_, v)| v.clone()).unwrap_or_default();
        assert!(crate::mcp::SUPPORTED.contains(&spoken.as_str()), "{version} speaks {spoken}: {seen:?}");
        assert_eq!(spoken, KNOWN, "{version} moved from {KNOWN} to {spoken} — check mcp.rs, then KNOWN");
    }

    /// A run where no test ran is reported as that — not "passed" — with the output to see why.
    #[test]
    fn a_run_where_no_test_ran_is_not_passed() {
        let output = vec![
            "running 0 tests".to_string(),
            "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out".into(),
        ];
        let mut r = crate::testing::TestRun::finished("term_snapshots::shows", Vec::new(), output);
        r.state = crate::testing::RunState::NoTests;
        let text = super::report(elsewhere(), &r, super::Waited::No);
        assert!(text.starts_with("no tests ran — term_snapshots::shows\n"), "{text}");
        assert!(text.contains("running 0 tests\n"), "the output says why: {text}");
        assert_eq!(super::verdict(&text), "no tests ran");
    }

    /// A run's policy has what the user allowed in it — kept, this chat's, and a grant for this run alone —
    /// each once, and its result says them; a place Claude's Read was let read is read by its runs too (card
    /// model P2).
    #[test]
    fn what_the_user_allowed_goes_into_the_policy() {
        use crate::allow::{Allowed, Grant};
        let kept = Allowed {
            read: vec!["/r/kept".into()],
            write: vec!["/w/kept".into()],
            ports: vec![5432],
            ..Default::default()
        };
        let chat = Allowed { write: vec!["/w/chat".into()], ports: vec![5432], ..Default::default() };
        let base = crate::llm::LlmConfig::default();
        let w = super::with_allowed(&base, &[&kept, &chat], Some(&Grant::Port(3000)));
        assert_eq!(w.llm.sandbox_write, ["/w/kept", "/w/chat"]);
        assert_eq!(w.llm.sandbox_ports, [5432, 5432, 3000], "(the policy keeps each port once)");
        assert_eq!(
            w.granted,
            [
                "read /r/kept and everything in it",
                "write in /w/kept and everything in it",
                "connect to port 5432 on this machine",
                "write in /w/chat and everything in it",
                "connect to port 3000 on this machine"
            ]
        );
        assert!(w.sockets.is_empty());
        assert_eq!(w.reads, [std::path::PathBuf::from("/r/kept")]);
        let none = super::with_allowed(&base, &[], None);
        assert!(none.llm.sandbox_write.is_empty() && none.llm.sandbox_ports.is_empty());
        assert!(none.granted.is_empty() && none.sockets.is_empty() && none.reads.is_empty());
        // #119: a socket — this chat's, or this run's — as itself, nothing wider; a read for this run alone
        let chat =
            Allowed { sockets: vec!["/s/a.sock".into()], read: vec!["/r/x".into()], ..Default::default() };
        let w = super::with_allowed(&base, &[&chat], Some(&Grant::Socket("/s/b.sock".into())));
        assert_eq!(w.sockets, [std::path::PathBuf::from("/s/a.sock"), "/s/b.sock".into()]);
        assert!(w.llm.sandbox_write.is_empty() && w.llm.sandbox_ports.is_empty());
        assert_eq!(w.granted[2], "connect to the socket /s/b.sock — that one file");
        let once = super::with_allowed(&base, &[], Some(&Grant::Read("/r/once".into())));
        assert_eq!(once.reads, [std::path::PathBuf::from("/r/once")]);
    }

    /// Checks run at once; anything the words don't fully say — or a read-only command made to write —
    /// asks first.
    #[test]
    fn only_plain_checks_run_without_asking() {
        let ok = |c: &str| super::allowed(c, &[]);
        for c in
            ["cargo test", "cargo test -p tarae --lib tools::", "cargo clippy --all-targets -- -D warnings"]
        {
            assert!(ok(c), "{c}");
        }
        for c in [
            "cargo fmt",
            "cargo fmt --check",
            "git status",
            "git diff HEAD~1 -- src/tools.rs",
            "git log -5",
            // Quoted words are words; a redirect to nowhere writes nothing (#27)
            "git log --format='%h'",
            "cargo test -- 'a b'",
            "cargo build 2>&1",
            "sh -c 'cargo test'",
            // #88: git's options that change neither what runs nor where
            "git --no-optional-locks status",
            "git --no-pager log -5",
            "git -P diff HEAD",
            "git --no-optional-locks --no-pager diff --stat",
            "git --literal-pathspecs show HEAD -- src/a.rs",
        ] {
            assert!(ok(c), "{c}");
        }
        // …and the ones that do still ask
        for c in [
            "git --git-dir=/x status",
            "git --git-dir /x status",
            "git --work-tree=/x status",
            "git --exec-path=/x status",
            "git --namespace=x log",
            "git --config-env=core.pager=X log",
            "git --no-pager -c core.pager=x log",
            "git -p log",
            "git --paginate log",
            "git --no-pager push",
            "git --no-optional-locks commit -m x",
            // (where -C goes is `read_only`'s to judge — not a plain check)
            "git --no-pager -C .. status",
        ] {
            assert!(!ok(c), "{c}");
        }
        for c in [
            "cargo test; rm -rf ~",
            "cargo test && git push",
            "cargo test | tail",
            "cargo test > out.txt",
            "cargo test 2> err.txt",
            "cargo test $(rm x)",
            "cargo test `rm x`",
            "cargo test \"$HOME\"",
            "cargo test src/*",
            "git diff --output=src/main.rs",
            "git diff --ext-diff",
            "git log --exec=x",
            "cargo build --config build.rustc-wrapper=x",
            "RUSTC_WRAPPER=x cargo build",
            "cargo testx",
            "cargo",
            "cargo xtask dist",
            "git push",
            "git -c core.pager=x log",
            "git commit -m wip",
            "rm -rf target",
            "cargo test\nrm -rf x",
            // (`~/x` is home's, as any path — #164; someone else's can't be told)
            "cargo test ~root/x",
            "",
        ] {
            assert!(!ok(c), "{c}");
        }
        let extra = vec!["npm test".to_string(), " ".to_string()];
        assert!(super::allowed("npm test -- --watch=false", &extra), "an option's value is a plain word");
        assert!(super::allowed("npm test FOO=1", &extra), "an argument with = in it is a word");
        assert!(!super::allowed("FOO=1 npm test", &extra), "an assignment where the command goes isn't");
        assert!(!super::allowed("npm install", &extra), "an empty rule allows nothing");
        assert!(!super::allowed("npm testx", &extra), "word for word");
    }

    /// The card `run` puts up for `cmd` (through the worker it asks on, as Claude's call does) — its text.
    fn run_card(ed: &mut crate::editor::Editor, cmd: &str) -> String {
        ed.offers.clear();
        super::run(ed, super::Call::test(1), &serde_json::json!({ "command": cmd })).unwrap();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while ed.offers.is_empty() && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(50)) {
                ed.handle_event(ev);
            }
        }
        ed.offers.first().map(|o| o.question.clone()).unwrap_or_else(|| panic!("a card for {cmd}"))
    }

    /// P0 of docs/design/cards.md: each answer on a card of Claude's is one `card` line — the session, why
    /// today's rules asked, what the capability model would do instead (`cards::decide`), the answer, and
    /// the time since it was first drawn (or that it never was); one a turn took away is `dropped`.
    #[test]
    fn each_answer_on_a_card_is_one_line_in_the_log() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-card-lines-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        ed.session.name = "cardlines".into();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        crate::chat::open(&mut ed);
        let c = ed.session.chat.as_mut().unwrap();
        c.state = crate::chat::State::Thinking;
        c.focused = false;
        let generation = c.generation();
        let lines = || crate::cards::logged_lines("card cardlines · ");
        // (its card, through the worker it may ask on — the call the chat's, or the card is moot)
        let card = |ed: &mut crate::editor::Editor, n: u64, cmd: &str| {
            let call = super::Call::of(generation, &format!("r{n}"), json!(n));
            super::run(ed, call, &json!({ "command": cmd })).unwrap();
            let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while ed.offers.is_empty() {
                assert!(std::time::Instant::now() < end, "a card for {cmd}");
                if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
                    ed.handle_event(ev);
                }
            }
        };
        // A command none of today's rules lets through: drawn, then `n`
        card(&mut ed, 1, "python3 -c 'print(1)'");
        crate::term::render(&mut ed, &mut Vec::new(), 140, 36).unwrap();
        assert!(ed.offers[0].drawn.get().is_some(), "drawn in front");
        ed.offer_dismiss();
        let l = lines();
        assert_eq!(l.len(), 1, "{l:?}");
        assert!(
            l[0].starts_with("card cardlines · run · why=not-a-check:python3 · shadow=none · n · ")
                && !l[0].contains("not drawn")
                && l[0].contains(" · up "),
            "{}",
            l[0]
        );
        // Outside the sandbox: still a card under the model; never drawn
        let outside = json!({ "command": "ls", "outside_sandbox": true, "reason": "to see" });
        super::run(&mut ed, super::Call::of(generation, "r2", json!(2)), &outside).unwrap();
        ed.offer_dismiss();
        let l = lines();
        assert!(l[1].contains("· run · why=outside · shadow=outside · n · not drawn · up "), "{}", l[1]);
        // A turn that ends takes its card away: dropped
        card(&mut ed, 3, "awk -f x.awk");
        ed.drop_cards(|w| matches!(w, crate::offer::What::Run(_)));
        let l = lines();
        assert_eq!(l.len(), 3, "{l:?}");
        assert!(l[2].contains("· run · why=script · shadow=none · dropped · "), "{}", l[2]);
        // A light card that asks nothing of Claude's says nothing
        ed.push_offer(crate::offer::Offer::new(
            "x".into(),
            "q",
            "",
            String::new(),
            crate::offer::What::ReviewEnd { session: 0, started: std::time::Instant::now() },
        ));
        ed.offer_dismiss();
        assert_eq!(lines().len(), 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #87, #5: Claude's calls held back behind a run card are said on it; a second run while the card is up
    /// queues its own card (not refused — Claude sent the refused command again and again), and a yes while
    /// the first runs runs it next. With no card yet but one about to come, `C-q` says so.
    #[test]
    fn calls_behind_a_run_card_are_said_and_a_second_card_waits_its_turn() {
        use crate::chat::Called;
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-queue-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        crate::chat::open(&mut ed);
        let c = ed.session.chat.as_mut().unwrap();
        c.state = crate::chat::State::Thinking;
        c.focused = false;
        let generation = c.generation();
        let called = |id: &str, name: &str, input: serde_json::Value| Called {
            id: id.into(),
            name: name.into(),
            input,
        };
        let (one, two) = ("touch one.txt && echo one", "touch two.txt && echo two");
        c.calls = vec![
            called("t1", "mcp__tarae__run", json!({ "command": one })),
            called("t2", "Read", json!({ "file_path": "a.rs" })),
            called("t3", "Read", json!({ "file_path": "b.rs" })),
            called("t4", "mcp__tarae__run", json!({ "command": two })),
        ];
        let call = |n: u64| super::Call::of(generation, &format!("r{n}"), json!(n));
        super::run(&mut ed, call(1), &json!({ "command": one })).unwrap();
        // The second while the first card is up: a card of its own under it, nothing refused
        super::run(&mut ed, call(2), &json!({ "command": two })).unwrap();
        assert_eq!(ed.offers.len(), 2, "both cards");
        let refused = super::ANSWERED.with(|a| a.borrow().iter().any(|(id, _)| *id == json!(2)));
        assert!(!refused, "the second isn't refused");
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 140, 36).unwrap();
        let mut vt = vt100::Parser::new(36, 140, 0);
        vt.process(&bytes);
        let s = vt.screen().contents();
        // (the card wraps — the words read in a row once its rows are joined)
        let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(joined.contains("2 more calls wait on this: read a.rs · read b.rs"), "{s}");
        assert!(joined.contains("1/2"), "the second counted: {s}");
        // Yes to the first, then to the second while the first runs: it runs next (the first starts once the
        // checkout's snapshot is taken — `snapshot.rs`)
        ed.offer_accept();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ed.session.task_run.as_ref().is_some_and(|r| r.task.cmd == one) {
            assert!(std::time::Instant::now() < end, "the first runs");
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(5)) {
                ed.handle_event(ev);
            }
        }
        ed.offer_accept();
        assert_eq!(ed.session.runs_next.len(), 1, "the second waits for the first to end");
        while !dir.join("two.txt").exists() {
            assert!(std::time::Instant::now() < end, "the second ran after the first");
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        let said =
            |n: u64| super::ANSWERED.with(|a| a.borrow().iter().find(|(id, _)| *id == json!(n)).cloned());
        assert!(said(1).is_some_and(|(_, r)| r.is_ok_and(|t| t.starts_with("exit 0"))), "{:?}", said(1));
        // No card yet, one about to come (claude holds it until the call in hand is answered): said
        ed.offers.clear();
        let c = ed.session.chat.as_mut().unwrap();
        c.calls = vec![
            called("t5", "mcp__tarae__run", json!({ "command": "cargo test" })),
            called("t6", "mcp__tarae__run", json!({ "command": "git status" })),
        ];
        assert!(!c.asks_next(&[]), "a check comes next: it runs unasked");
        c.calls.push(called("t7", "mcp__tarae__run", json!({ "command": "touch x" })));
        assert!(c.asks_next(&[]));
        ed.session.name = "solo".into();
        ed.handle_key("C-q".parse().unwrap());
        let status = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert!(status.contains("no card yet — solo's comes once its run ends"), "{status}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Phase 0 of docs/design/async.md: Claude's run whose output says it waits on another cargo's lock — a
    /// stand-in `sh` printing cargo's line, then sleeping, nothing else — shows the wait as its state (the
    /// panel, not its output), Claude's answer tells the wait from the run, and the log gets the run's way
    /// from the ask, through its card, to its end.
    #[test]
    fn a_run_waiting_on_cargos_lock_is_said_and_traced() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-waits-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        crate::chat::open(&mut ed);
        let c = ed.session.chat.as_mut().unwrap();
        c.state = crate::chat::State::Thinking;
        c.focused = false;
        let call = super::Call::of(c.generation(), "r1", json!(41));
        let cmd = "printf '    Blocking waiting for file lock on build directory\\n' >&2; sleep 0.5; \
                   printf '   Compiling stand-in\\n' >&2; echo built";
        super::run(&mut ed, call, &json!({ "command": cmd })).unwrap();
        ed.offer_accept();
        let pump = |ed: &mut crate::editor::Editor, until: &dyn Fn(&crate::editor::Editor) -> bool| {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !until(ed) {
                assert!(std::time::Instant::now() < end, "timed out");
                if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(10)) {
                    ed.handle_event(ev);
                }
            }
        };
        // Waiting: the panel says so, in the wave's place; cargo's line isn't output
        pump(&mut ed, &|ed| ed.session.task_run.as_ref().is_some_and(|r| r.wait.now.is_some()));
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 140, 36).unwrap();
        let mut vt = vt100::Parser::new(36, 140, 0);
        vt.process(&bytes);
        let s = vt.screen().contents();
        assert!(s.contains("waiting for cargo's lock · 0:0"), "{s}");
        assert!(ed.session.task_run.as_ref().unwrap().output.is_empty(), "cargo's line isn't output");
        // Its end: the answer says how much was the wait
        let running = |ed: &crate::editor::Editor| {
            ed.session.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running)
        };
        // (answered once what it changed is taken — `snapshot.rs`)
        let said = || super::ANSWERED.with(|a| a.borrow().iter().find(|(id, _)| *id == json!(41)).cloned());
        pump(&mut ed, &|ed| !running(ed) && said().is_some());
        let r = ed.session.task_run.as_ref().unwrap();
        assert!(r.wait.now.is_none() && !r.output.iter().any(|l| l.1.contains("Blocking")));
        let said = said();
        let text = said.expect("answered").1.unwrap();
        let second = text.lines().nth(1).unwrap_or_default();
        let waited: f32 = second
            .strip_prefix("Waited ")
            .and_then(|w| w.split_once("s for cargo's build directory lock. Ran "))
            .and_then(|(w, _)| w.parse().ok())
            .unwrap_or_else(|| panic!("{text}"));
        assert!(text.starts_with("exit 0") && (0.4..3.0).contains(&waited), "{text}");
        let output = text.split("\nOutput:\n").nth(1).unwrap_or_default();
        assert!(output.contains("Compiling stand-in") && !output.contains("Blocking"), "{text}");
        // The log's line: each step on its way, in order
        let line = crate::trace::LOGGED.with(|l| l.borrow().iter().find(|l| l.contains("stand-in")).cloned());
        let line = line.expect("traced");
        let mut at = 0;
        for step in [
            "card up",
            "answered yes",
            "saved",
            "spawned",
            "first output",
            "waited for cargo's lock",
            "exit 0",
        ] {
            let found = line[at..].find(&format!(" · {step} ")).unwrap_or_else(|| panic!("{step}: {line}"));
            at += found + 1;
        }
        assert!(line.starts_with("claude's run: printf"), "{line}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Review note: a copy, move or link of a file written unseen this turn is unseen too — a later call that
    /// runs the copy still shows it on its card (`cp evil.sh ok.sh`, then `sh ok.sh`, had lost the mark).
    /// Into a folder too, over a file that wasn't text, and a move whose target can't be told names the
    /// unseen file on its own card instead.
    #[test]
    fn a_copy_of_an_unseen_file_is_unseen_too() {
        use crate::checkpoint::Touched;
        let dir = std::env::temp_dir().join(format!("tarae-carried-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::fs::write(dir.join("evil.sh"), "echo evil\n").unwrap();
        std::fs::write(dir.join("blob.bin"), [0xff, 0xfe, 0x00]).unwrap();
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        ed.session.checkpoints.turn.push(Touched {
            path: dir.join("evil.sh"),
            before: None,
            after: ropey::Rope::new(),
            unseen: true,
            ..Default::default()
        });
        let shown = |q: &str, f: &str| q.contains(&format!("{f} — written this turn, unseen:\necho evil"));
        // The copy: nothing on its own card (copying runs nothing) — then, run in a second call, flagged
        let q = run_card(&mut ed, "cp evil.sh ok.sh");
        assert!(!q.contains("written this turn"), "{q}");
        std::fs::copy(dir.join("evil.sh"), dir.join("ok.sh")).unwrap();
        let q = run_card(&mut ed, "sh ok.sh");
        assert!(shown(&q, "ok.sh"), "the copy still unseen: {q}");
        // Moved into a folder, then run from there
        run_card(&mut ed, "mv ok.sh bin");
        std::fs::rename(dir.join("ok.sh"), dir.join("bin/ok.sh")).unwrap();
        let q = run_card(&mut ed, "./bin/ok.sh");
        assert!(shown(&q, "bin/ok.sh"), "{q}");
        // Over a file that wasn't text (nothing kept of it for `space u` — still unseen for the guard)
        run_card(&mut ed, "cp evil.sh blob.bin");
        std::fs::copy(dir.join("evil.sh"), dir.join("blob.bin")).unwrap();
        assert!(shown(&run_card(&mut ed, "sh blob.bin"), "blob.bin"));
        // A target it can't tell: the unseen source is named on the card itself
        let q = run_card(&mut ed, "cp -t bin evil.sh");
        assert!(shown(&q, "evil.sh"), "{q}");
        // The copy is in the turn's files: its review shows it, `space u` takes it back
        assert!(ed.session.checkpoints.turn.iter().any(|t| t.unseen && t.path == dir.join("ok.sh")));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #119: Claude asks for one unix socket past the sandbox, not the sandbox off: the card names that file,
    /// its reason, that the rest stays sandboxed (Docker's said for what it is); `a` lets it for this chat.
    /// A path that isn't a socket, a pattern, or no reason is refused before any card.
    #[test]
    fn claude_asks_for_one_socket_not_the_sandbox_off() {
        use serde_json::json;
        let dir =
            crate::sandbox::profile_path(&std::env::temp_dir()).join(format!("q{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("c")).unwrap();
        let sock = dir.join("c/docker.sock");
        let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        std::fs::write(dir.join("plain"), "marker").unwrap();
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        crate::chat::open(&mut ed);
        let ask = |ed: &mut crate::editor::Editor, socket: &str, reason: &str| {
            ed.offers.clear();
            // (a stand-in for `cargo xtask linux test`: it runs, after `a`)
            let args = json!({ "command": "true", "unix_socket": socket, "reason": reason });
            super::run(ed, super::Call::test(1), &args)
        };
        ask(&mut ed, sock.to_str().unwrap(), "docker runs the Linux checks").unwrap();
        let o = ed.offers.first().expect("its card");
        assert_eq!(o.title, super::SOCKET_RUN_TITLE);
        let q = o.question.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(q.contains(&format!(
            "It may connect to {} — that one file; everything else stays",
            sock.display()
        )));
        assert!(
            q.contains("Its reason: docker runs the Linux checks") && q.contains("mounts any folder"),
            "{q}"
        );
        assert!(o.lasts() && !o.keeps(), "this chat at most");
        ed.offer_for_chat();
        // (Docker's: the command's for this chat, never the socket — #193)
        let c = ed.session.chat.as_ref().unwrap();
        assert!(c.allowed.sockets.is_empty(), "not the socket");
        assert_eq!(c.run_again.len(), 1, "the command, with it");
        assert!(
            ed.session.checkpoints.tell.iter().any(|t| t.contains("connect to the socket")),
            "Claude hears it"
        );
        for (socket, reason, why) in [
            (dir.join("plain").display().to_string(), "x", "isn't a unix socket"),
            (format!("{}/c/*.sock", dir.display()), "x", "one file"),
            ("docker.sock".to_string(), "x", "full path"),
            (sock.display().to_string(), "", "needs a reason"),
        ] {
            let e = ask(&mut ed, &socket, reason).unwrap_err();
            assert!(e.contains(why) && ed.offers.is_empty(), "{socket}: {e}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #193 (user decision 2026-10-06): on Docker's socket — what `allow.rs` calls as much as running
    /// outside the sandbox — `a` is this command with that socket, never the socket for every command. The
    /// same command again, its filters changed: no card, and it runs with the socket (this run's grant). Another
    /// command with the socket: the socket card again. The chat's sockets never hold it, whatever asks it in:
    /// the run card, the blocked-run card's grant, `allow::grant`.
    #[test]
    fn a_on_dockers_socket_is_this_command_never_the_socket() {
        use serde_json::json;
        let dir =
            crate::sandbox::profile_path(&std::env::temp_dir()).join(format!("qd{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("c")).unwrap();
        let sock = dir.join("c/docker.sock");
        let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        // (the stand-in runs unsandboxed here — `echo`, nothing else; what's checked is what it was given)
        ed.config.llm.sandbox = false;
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        crate::chat::open(&mut ed);
        let c = ed.session.chat.as_mut().unwrap();
        c.state = crate::chat::State::Thinking;
        c.focused = false;
        let generation = c.generation();
        let ask = |ed: &mut crate::editor::Editor, n: u64, command: &str| {
            let args =
                json!({ "command": command, "unix_socket": sock.to_str().unwrap(), "reason": "docker" });
            super::run(ed, super::Call::of(generation, &format!("r{n}"), json!(n)), &args)
        };
        let pump = |ed: &mut crate::editor::Editor, done: &dyn Fn(&crate::editor::Editor) -> bool| {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !done(ed) {
                assert!(std::time::Instant::now() < end, "waited too long");
                if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
                    ed.handle_event(ev);
                }
            }
        };
        let admitted =
            |cmd: &str| super::ADMITTED.with(|a| a.borrow().iter().rev().find(|x| x.0 == cmd).cloned());
        // (`basename` runs nothing else and isn't a read: without a rule, it asks)
        let first = "basename trace test | grep trace";
        ask(&mut ed, 1, first).unwrap();
        let o = ed.offers.first().expect("its card");
        assert_eq!(o.title, super::SOCKET_RUN_TITLE);
        assert_eq!(o.answers(false), [('y', "This run"), ('a', "This command, this chat"), ('n', "No")]);
        let q = o.question.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(q.contains("a lets this command connect again for the rest of this chat"), "{q}");
        assert!(q.contains("never another command"), "{q}");
        ed.offer_for_chat();
        assert!(ed.session.chat.as_ref().unwrap().allowed.sockets.is_empty(), "not the socket");
        pump(&mut ed, &|_| admitted(first).is_some());
        // The same command, another filter: no card, and the socket rides with it
        ed.offers.clear();
        let again = "basename trace test | tail -1";
        ask(&mut ed, 2, again).unwrap();
        pump(&mut ed, &|ed| admitted(again).is_some() || !ed.offers.is_empty());
        assert!(ed.offers.is_empty(), "asked again: {:?}", ed.offers.first().map(|o| &o.question));
        let grant = admitted(again).and_then(|a| a.1.grant);
        assert_eq!(grant, Some(crate::allow::Grant::Socket(sock.clone())), "this run's grant");
        // Another command with it: the socket card
        ask(&mut ed, 3, "basename other").unwrap();
        pump(&mut ed, &|ed| !ed.offers.is_empty() || admitted("basename other").is_some());
        assert_eq!(ed.offers.first().map(|o| o.title.as_str()), Some(super::SOCKET_RUN_TITLE));
        // The same command without the socket isn't it either: its own card
        ed.offers.clear();
        let bare = json!({ "command": first });
        super::run(&mut ed, super::Call::of(generation, "r4", json!(4)), &bare).unwrap();
        pump(&mut ed, &|ed| !ed.offers.is_empty() || admitted(first).is_some_and(|a| a.1.grant.is_none()));
        assert_eq!(ed.offers.first().map(|o| o.title.as_str()), Some("Claude wants to run"));
        // Never the chat's, whatever asks: the grant card (`blocked.rs`) offers no `a`, `allow::grant` keeps
        // none and says none
        let g = crate::allow::Grant::Socket(sock.clone());
        ed.toasts.clear();
        crate::allow::grant(&mut ed, &g, crate::allow::Scope::ThisChat);
        assert!(ed.session.chat.as_ref().unwrap().allowed.sockets.is_empty());
        assert!(ed.toasts.is_empty(), "nothing granted, nothing said");
        // (and were it in the chat's sockets somehow, a run asking for it still gets its card)
        ed.session.chat.as_mut().unwrap().allowed.sockets.push(sock.clone());
        ed.offers.clear();
        ask(&mut ed, 5, "basename five").unwrap();
        pump(&mut ed, &|ed| !ed.offers.is_empty() || admitted("basename five").is_some());
        assert_eq!(ed.offers.first().map(|o| o.title.as_str()), Some(super::SOCKET_RUN_TITLE));
        let mut a = crate::allow::Allowed::default();
        a.add(&g);
        assert!(a.sockets.is_empty(), "never added");
        let card = crate::offer::Offer::new(
            "x".into(),
            "",
            "",
            String::new(),
            crate::offer::What::Grant {
                grant: g,
                run: Box::new(crate::blocked::BlockedRun {
                    job: super::Job::Command(first.into(), dir.clone()),
                    denials: Vec::new(),
                    from: None,
                }),
            },
        );
        assert_eq!(card.answers(false), [('y', "This run"), ('n', "No")]);
        assert!(!card.lasts());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #182: `a` on a socket's card is that socket for the rest of the chat, for any command — and the card
    /// says so (a socket that isn't Docker's: Docker's is the command's — #193). Asked again for it (the very
    /// same call, right after): no socket card; the command goes as any command does — one that runs unasked
    /// runs, another gets its own card, without the socket in it.
    #[test]
    fn a_socket_allowed_for_the_chat_isnt_asked_again() {
        use serde_json::json;
        let dir =
            crate::sandbox::profile_path(&std::env::temp_dir()).join(format!("qa{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("c")).unwrap();
        let sock = dir.join("c/app.sock");
        let _l = std::os::unix::net::UnixListener::bind(&sock).unwrap();
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        crate::chat::open(&mut ed);
        let ask = |ed: &mut crate::editor::Editor, n: u64, command: &str| {
            let args =
                json!({ "command": command, "unix_socket": sock.to_str().unwrap(), "reason": "docker" });
            super::run(ed, super::Call::test(n), &args)
        };
        // (stand-ins: `true` reads nothing and runs unasked; `make demo` — not a check — asks)
        ask(&mut ed, 1, "true").unwrap();
        let o = ed.offers.first().expect("its card");
        assert_eq!(o.title, super::SOCKET_RUN_TITLE);
        assert_eq!(o.answers(false), [('y', "This run"), ('a', "This socket, this chat"), ('n', "No")]);
        let q = o.question.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            q.contains(
                "y lets it connect for this run only; a for the rest of this chat, whatever command asks — each \
                 command is still asked as usual."
            ),
            "{q}"
        );
        ed.offer_for_chat();
        // The very same call again: no card — it runs
        ed.offers.clear();
        ask(&mut ed, 2, "true").unwrap();
        assert!(ed.offers.is_empty(), "asked again: {:?}", ed.offers.first().map(|o| &o.question));
        // Another command with it: its own card, as any — the socket not asked again
        ask(&mut ed, 3, "make demo").unwrap();
        let o = ed.offers.first().expect("the command's card");
        assert_eq!(o.title, "Claude wants to run");
        assert!(!o.question.contains("It may connect"), "{}", o.question);
        ed.session.task_run = None;
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #117: a command that runs a file of code — `awk -f`, `sed -f`, `python x.py`, `perl x.pl`, `sh x.sh`,
    /// `./x` — puts that file's text on its card (written by whoever, not only unseen this turn), and `y`
    /// reads it again: changed since, nothing runs. A binary run by its path is a program, not a script; a
    /// script too long or with hidden characters is named, and why. (Fixtures: files with a marker only —
    /// nothing here runs them.)
    #[test]
    fn a_card_shows_the_script_a_command_runs() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-scripts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        for (f, text) in [
            ("x.awk", "# marker-awk\n{ print }\n"),
            ("x.sed", "# marker-sed\np\n"),
            ("tools/gen.py", "# marker-py\nprint(1)\n"),
            ("run.pl", "# marker-pl\n"),
            ("setup.sh", "# marker-sh\n"),
            ("tools/go", "#!/bin/sh\n# marker-go\n"),
            ("hidden.py", "# marker \u{202E}x\n"),
        ] {
            std::fs::write(dir.join(f), text).unwrap();
        }
        std::fs::write(dir.join("long.py"), "# marker\n".repeat(super::SCRIPT_LINES_MAX + 1)).unwrap();
        std::fs::write(dir.join("tool"), b"\xcf\xfa\xed\xfe\x00\x00binary").unwrap();
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        for (cmd, file, marker) in [
            ("awk -f x.awk data.txt", "x.awk", "marker-awk"),
            ("gawk --file=x.awk data.txt", "x.awk", "marker-awk"),
            ("sed -n -f x.sed data.txt", "x.sed", "marker-sed"),
            ("python3 -u tools/gen.py --fast", "tools/gen.py", "marker-py"),
            ("perl run.pl > out.txt", "run.pl", "marker-pl"),
            ("A=1 sh setup.sh", "setup.sh", "marker-sh"),
            ("cd tools && ./go", "tools/go", "marker-go"),
        ] {
            let q = run_card(&mut ed, cmd);
            assert!(q.contains(&format!("{file} — what it runs:\n")) && q.contains(marker), "{cmd}: {q}");
        }
        for (cmd, said) in [
            ("python long.py", "long.py has 401 lines — too long to show here"),
            ("python hidden.py", "hidden.py has characters a card can't show as they are"),
        ] {
            let q = run_card(&mut ed, cmd);
            assert!(q.contains(said) && !q.contains("what it runs"), "{cmd}: {q}");
        }
        for cmd in ["./tool --x", "python3 -c 'print(1)'", "awk '{print}' x.awk", "perl -e 1 run.pl"] {
            let q = run_card(&mut ed, cmd);
            assert!(!q.contains("what it runs"), "{cmd}: {q}");
        }
        // `y` reads it again: changed since the card, nothing runs
        run_card(&mut ed, "perl run.pl");
        std::fs::write(dir.join("run.pl"), "# changed\n").unwrap();
        ed.offer_accept();
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        // (every card here is call 1's: the ones cleared away were answered as lost)
        let changed = || {
            super::ANSWERED.with(|a| {
                a.borrow().iter().any(|(id, r)| {
                    *id == json!(1)
                        && r.as_ref().is_err_and(|e| e.contains("run.pl changed after the user saw it"))
                })
            })
        };
        while !changed() {
            assert!(std::time::Instant::now() < end, "answered: changed since");
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        assert!(ed.session.task_run.is_none(), "nothing ran");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Review note: a chain that `cd`s out of the project, or git with `-C` outside it, goes to a card through
    /// `run` — a build there runs that folder's build scripts, git its repository's config — even when
    /// reading that folder was allowed; inside, the same line runs at once.
    #[test]
    fn a_chain_that_runs_outside_the_project_asks() {
        let dir = std::env::temp_dir().join(format!("tarae-chain-out-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for d in ["proj/sub", "other"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        let dir = std::fs::canonicalize(&dir).unwrap();
        let (proj, other) = (dir.join("proj"), dir.join("other"));
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        ed.session.root = proj.clone();
        // A card, or the run started at once — what `run` decided
        let decided = |ed: &mut crate::editor::Editor, cmd: &str| -> Option<String> {
            ed.offers.clear();
            ed.session.task_run = None;
            super::run(ed, super::Call::test(1), &serde_json::json!({ "command": cmd })).unwrap();
            let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while ed.offers.is_empty() && ed.session.task_run.is_none() && std::time::Instant::now() < end {
                if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(50)) {
                    ed.handle_event(ev);
                }
            }
            assert!(!ed.offers.is_empty() || ed.session.task_run.is_some(), "{cmd}: neither");
            ed.offers.first().map(|o| o.question.clone())
        };
        let outside = [
            "cd ../other && cargo test",
            "cd /tmp && cargo build",
            &format!("git -C {} status", other.display()),
            "cd ../other; cargo fmt && cargo clippy",
            "git -C .. log | sort",
            "cd .. && git ls-files",
            // #88: a -C behind git's harmless options is judged all the same
            "git --no-optional-locks -C .. status",
            "git --no-pager -C .. log | sort",
        ];
        // (then reading all of it allowed for good, `A` on a read's card: still not a build or git run there)
        let store = dir.join("sandbox.json");
        crate::allow::keep(&store, &proj, |a| a.read.push(dir.clone())).unwrap();
        for allowed in [false, true] {
            ed.allow_path = allowed.then(|| store.clone());
            for cmd in outside {
                let q =
                    decided(&mut ed, cmd).unwrap_or_else(|| panic!("{cmd} ran (reads allowed: {allowed})"));
                assert!(q.starts_with(cmd), "the plain card, the command on it: {q}");
                assert!(ed.session.task_run.is_none(), "{cmd}: nothing ran");
            }
        }
        // #152: and where it runs — the folder after its `cd`s, decided, under the command
        let q = decided(&mut ed, "cd ../other && cargo test").unwrap();
        assert_eq!(q.lines().nth(1), Some(format!("  in {}", other.display()).as_str()), "{q}");
        assert_eq!(ed.offers[0].decided_lines(), 0..2, "said as decided");
        // Inside the project: at once
        assert_eq!(decided(&mut ed, "cd sub && git status"), None, "inside: runs");
        ed.session.task_run = None;
        assert_eq!(decided(&mut ed, "git --no-optional-locks -C sub status"), None, "#88 inside: runs");
        ed.session.task_run = None;
        // #92, word for word: a pipeline of reads over a file under the git-ignored `target/` — at once,
        // whether the file is there yet or not
        let line = r#"sh -c 'grep -v "^.*note" target/ra-out/grow/grow.events | sed -n "1,200p" | grep -E "begin|end|status|stopped"'"#;
        std::fs::write(proj.join(".gitignore"), "target/\n").unwrap();
        assert_eq!(decided(&mut ed, line), None, "not there yet: runs");
        ed.session.task_run = None;
        std::fs::create_dir_all(proj.join("target/ra-out/grow")).unwrap();
        std::fs::write(proj.join("target/ra-out/grow/grow.events"), "begin\nnote\nend\n").unwrap();
        assert_eq!(decided(&mut ed, line), None, "there: runs");
        ed.session.task_run = None;
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #152: where a run card says a command runs — every folder its commands run in that isn't the root,
    /// after its `cd`s, from the words alone; a line the reader can't read says its `cwd` only.
    #[test]
    fn a_run_card_says_the_folder_it_runs_in() {
        use std::path::Path;
        let root = Path::new("/r/proj");
        let at = |cwd: &str, cmd: &str| super::runs_in(root, Path::new(cwd), cmd);
        assert!(at("/r/proj", "cargo test").is_empty(), "the root: nothing to say");
        assert_eq!(at("/r/proj/crates/a", "cargo test"), ["crates/a"], "a cwd inside");
        assert_eq!(at("/r/proj", "cd sub && make"), ["sub"]);
        assert!(at("/r/proj/sub", "cd .. && make").is_empty(), "back at the root");
        assert_eq!(at("/r/proj", "cd ../other && cargo test"), ["/r/other"], "outside: the whole path");
        assert_eq!(at("/r/proj", "cd a && make; cd ../b && make && make check"), ["a", "b"], "each once");
        assert!(at("/r/proj", "cd /tmp/x").is_empty(), "a cd alone runs nothing there");
        assert_eq!(at("/r/proj/sub", "echo $HOME"), ["sub"], "unreadable: its cwd");
    }

    /// #57, #66: a line of parts each allowed on its own runs unasked (where it reads still judged) —
    /// checks chained, git's reads with `-C <dir>`, a check or a read piped into `sort` or `sed -n 'a,bp'`;
    /// one part that isn't, and the line asks.
    #[test]
    fn a_chain_of_allowed_parts_is_allowed() {
        let ok = |c: &str| super::chain_allowed(c, &[]);
        for c in [
            "cargo fmt && cargo clippy",
            "cargo test; cargo clippy",
            "cargo build || cargo check",
            "cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test",
            "git -C sub log -5",
            "git -C a -C b status",
            "git -C sub diff HEAD -- x | sort",
            "cargo test 2>&1 | sed -n '1,40p'",
            "git log --oneline | sort",
            "cd src && cargo check && ls",
            "sh -c 'cargo test; cargo clippy'",
            // #88
            "git --no-optional-locks -C sub diff",
            "git -C sub --no-pager log | sort",
            "git --no-pager blame src/a.rs | head",
        ] {
            assert!(ok(c), "{c}");
        }
        for c in [
            "cargo fmt && rm x",
            "cargo test; git push",
            "git -C sub push",
            "git -C sub -c core.pager=x log",
            "git -C sub diff --ext-diff",
            "git -C sub --no-pager -c core.pager=x log",
            "git --no-pager --git-dir=x -C sub log",
            "cargo test | sed 's/a/b/w out'",
            "cargo test | sed -n '1,4w out'",
            "cargo test | sh",
            "cargo test src/* && ls",
            "cargo test > out; cargo clippy",
            "cd src",
            "",
        ] {
            assert!(!ok(c), "{c}");
        }
        let rule = vec!["cargo xtask shot".to_string()];
        assert!(super::chain_allowed("cargo xtask shot a.png && cargo test", &rule), "llm.run-allow's own");
        assert!(super::filtered("cargo test | sed -n '1,40p'").is_some(), "sed -n is a filter");
        assert!(super::is_check("cargo fmt && cargo clippy"), "a chain of checks feeds the list");
        // A9: an `echo` in a line of checks says a word — the line still runs unasked, and is still checks
        let echo = "cargo clippy --all-targets -q && cargo fmt --check && echo clean";
        assert!(ok(echo) && super::runs_unasked(echo, &[]) && super::is_check(echo));
        assert!(!super::is_check("echo hi"), "nothing but words isn't a check");
        // #116: cargo's own reads of the graph
        for c in ["cargo tree -i libc -e normal", "cargo metadata --format-version 1 --no-deps"] {
            assert!(super::allowed(c, &[]), "{c}");
        }
        // #110: `git diff --no-index` reads what it names — judged as a read, never a check's unasked run
        assert!(!super::allowed("git diff --no-index --stat /tmp/a /tmp/b", &[]));
        assert!(super::runs_unasked("git diff --no-index --stat a b", &[]), "(where: the worker's)");
        assert!(super::runs_unasked("git show :2:src/a.rs | head -40", &[]));
        // #117: a reader into an interpreter is the interpreter's run — never unasked by the built-in rules
        for c in ["nm -n -C x | awk -f y.awk", "sh -c 'nm -n x | awk -f y.awk'", "cat a | sed -f s.sed"] {
            assert!(!super::runs_unasked(c, &[]), "{c}");
        }
    }

    /// What a run card's `a` can't be offered for (#193: it's the exact command, but these could run
    /// anything next time): a runner, `env` with an option or no command, a name that finds programs. And
    /// an `llm.run-allow` rule (the user's own, in settings) is still a prefix, word for word.
    #[test]
    fn a_cards_a_is_never_for_a_runner() {
        let p = |c: &str| super::again(c);
        assert_eq!(p("git config --get user.name"), None, "git: reads run already, writes go to .git");
        assert!(p("sh -c 'cargo bench'").is_some_and(|a| a.covers("cargo bench")), "the body's");
        for c in [
            "env -i cargo test",
            "env -S 'cargo test' x",
            "env FOO=1",
            "env",
            "env PATH=/x cargo xtask shot",
            "python3 -m http.server",
            "xargs rm",
            "sudo make",
            "time cargo build",
            "sh x.sh && python3 y.py",
            "../elsewhere/x.sh",
            "/bin/rm -rf x",
        ] {
            assert_eq!(p(c), None, "{c}");
        }
        let rule = vec!["cargo xtask shot".to_string()];
        assert!(super::allowed("cargo xtask shot /tmp/a.png '@?files in'", &rule));
        assert!(!super::allowed("cargo xtask perf", &rule));
        assert!(!super::allowed("cargo xtask shot x; rm -rf y", &rule));
    }

    /// #169: a card's `a` promised `cargo test …`, and the next `cargo test` with other arguments asked
    /// again — `--config` was what asked, and no allowance lifts it. What `a` offers, once taken, lets that
    /// command through again (#193: that one, exactly — its filters aside); a card it can't keep that for
    /// offers no `a` and says what asks.
    #[test]
    fn what_a_cards_a_offers_is_what_it_lets_through() {
        use crate::cards::Why;
        let theirs = [
            "cargo test --target x86_64-unknown-linux-gnu --config 'target.x86_64-unknown-linux-gnu.runner=\"x\"' \
             zz_drive -- --ignored --nocapture | grep -E 'ok|FAILED' | tail -20",
            "cargo test --target aarch64-unknown-linux-gnu --config 'build.rustflags=[]' zz_drive -- --ignored \
             --nocapture",
        ];
        for c in theirs {
            assert_eq!(super::again(c), None, "{c}");
            assert_eq!(super::why_asked(c, &[]), Why::Writes("--config"), "{c}");
            assert!(Why::Writes("--config").said().unwrap().contains("--config"));
        }
        // Each card's `a`, taken: the command itself again, another filter after it — not another of its
        // kind (#193: those were let through by a prefix)
        for (c, filtered, other) in [
            (
                "cargo xtask shot /tmp/x.png '@?ready'",
                "cargo xtask shot /tmp/x.png '@?ready' | tail",
                "cargo xtask shot /tmp/y.png",
            ),
            (
                "cargo xtask shot x.png 2>&1 | grep -E 'a|b' | tail -3",
                "cargo xtask shot x.png | wc -l",
                "cargo xtask shot z.png",
            ),
            (
                "TARAE_BACKGROUND=dark cargo xtask shot a.png",
                "TARAE_BACKGROUND=dark cargo xtask shot a.png | head",
                "TARAE_BACKGROUND=light cargo xtask shot a.png",
            ),
            ("npm run build:prod", "npm run build:prod | grep error", "npm run lint"),
            ("cargo bench --bench x | tail", "cargo bench --bench x", "cargo bench"),
        ] {
            let a = super::again(c).unwrap_or_else(|| panic!("{c}: no a"));
            assert!(a.covers(c) && a.covers(filtered) && !a.covers(other), "{c}: {a:?}");
        }
    }

    /// #193 (user decision 2026-10-06): a run card's `a` is this exact command — every word, the names and
    /// values set in front, its `cd`s — with any filters after it that only read: `cargo xtask trace test |
    /// grep A`, then `… | grep B`, `… | tail -5`, `… | jq -r .x` run unasked; one word more, less or
    /// different in the command, another value, another name: asked. A filter that writes, runs or reads a
    /// file of its own is no filter (awk's `system`, `|`, `>`, `getline`, `ENVIRON`, `@load`, `-f`; jq's
    /// files, `include`, `env`; `sort -o`, `tee`).
    #[test]
    fn a_run_cards_a_is_this_exact_command_with_any_reading_filters() {
        let again = |c: &str| super::again(c);
        let words = |c: &str| again(c).map(|a| a.parts);
        let w = |s: &[&str]| s.iter().map(|w| w.to_string()).collect::<Vec<_>>();
        let trace = "cargo xtask trace test | grep A";
        assert_eq!(words(trace), Some(vec![w(&["cargo", "xtask", "trace", "test"])]));
        // (a name set in front is part of the command, value and all — it offered no `a` before)
        let env = "TARAE_UI=x cargo xtask trace test 2>&1 | grep -E 'a|b' | head -20";
        assert_eq!(words(env), Some(vec![w(&["TARAE_UI=x", "cargo", "xtask", "trace", "test"])]));
        assert_eq!(
            words("cd sub && cargo xtask trace test | grep A"),
            Some(vec![w(&["cd", "sub"]), w(&["cargo", "xtask", "trace", "test"])])
        );
        let a = again(trace).unwrap();
        for same in [
            "cargo xtask trace test | grep B",
            "cargo xtask trace test",
            "cargo xtask trace test 2>&1 | tail -5",
            "cargo xtask trace test | grep -v x | sort | uniq -c | sort -rn | head -3 | wc -l",
            "cargo xtask trace test | cut -d: -f1 | tr a-z A-Z",
            "cargo xtask trace test | awk -F: -v n=2 '{ print $n }'",
            "cargo xtask trace test | awk '$3 == \"x\" { n++ } END { print n }'",
            "cargo xtask trace test | jq -r --arg k v '.[$k] | select(.a)'",
            "sh -c 'cargo xtask trace test | grep A'",
        ] {
            assert!(a.covers(same), "{same}");
        }
        for other in [
            "cargo xtask trace test2 | grep A",
            "cargo xtask trace test --x | grep A",
            "cargo xtask trace | grep A",
            "cargo xtask shot | grep A",
            "TARAE_UI=x cargo xtask trace test | grep A",
            "cargo xtask trace test; rm x",
            "cargo xtask trace test && cargo xtask trace test2",
            "cd sub && cargo xtask trace test | grep A",
            // filters that aren't: a program, a writer, a file of their own
            "cargo xtask trace test | sh",
            "cargo xtask trace test | tee out",
            "cargo xtask trace test | sort -o out",
            "cargo xtask trace test | sort --out=f",
            "cargo xtask trace test | sort --compress-prog=./x",
            "cargo xtask trace test | uniq - out",
            "cargo xtask trace test | tail -f",
            "cargo xtask trace test | tail --fol",
            "cargo xtask trace test | xargs rm",
            "cargo xtask trace test | sed -n 1p",
            "cargo xtask trace test | grep A *.rs",
            "cargo xtask trace test | awk '{ system(\"rm x\") }'",
            "cargo xtask trace test | awk '{ print > \"f\" }'",
            "cargo xtask trace test | awk '{ print | \"sh\" }'",
            "cargo xtask trace test | awk '{ \"date\" | getline d }'",
            "cargo xtask trace test | awk 'BEGIN { print ENVIRON[\"T\"] }'",
            "cargo xtask trace test | awk '@load \"x\"'",
            "cargo xtask trace test | awk -f x.awk",
            "cargo xtask trace test | awk '{ print }' other.txt",
            "cargo xtask trace test | awk -i x '{ print }'",
            "cargo xtask trace test | jq . other.json",
            "cargo xtask trace test | jq --rawfile x f .",
            "cargo xtask trace test | jq -f prog.jq",
            "cargo xtask trace test | jq 'env.T'",
            "cargo xtask trace test | jq '$ENV.T'",
            "cargo xtask trace test | jq 'include \"x\"; .'",
        ] {
            assert!(!a.covers(other), "{other}");
        }
        let e = again(env).unwrap();
        assert!(e.covers("TARAE_UI=x cargo xtask trace test | tail -1"));
        for other in [
            "TARAE_UI=y cargo xtask trace test | grep A",
            "cargo xtask trace test | grep A",
            "TARAE_UI=x CC=/tmp/evil cargo xtask trace test | grep A",
            "CC=/tmp/evil TARAE_UI=x cargo xtask trace test",
        ] {
            assert!(!e.covers(other), "{other}");
        }
        // No `a`: a program that runs what follows it, git (what reads runs already), a name that finds
        // programs, an option no rule lets through, a glob the shell fills in
        for c in [
            "python3 -c 'print(1)' | grep 1",
            "env -i cargo test",
            "xargs rm",
            "git commit -m x",
            "PATH=/tmp/bin cargo xtask shot",
            "LD_PRELOAD=/tmp/x.so cargo test",
            "cargo test --config 'build.rustflags=[]'",
            "cargo run -- *.rs",
        ] {
            assert_eq!(again(c), None, "{c}");
        }
        // A script run by its path: in the key, and allowed as it is now (`script_pins`)
        let s = again("sh target/x/setup.sh && cargo xtask shot a.png").unwrap();
        assert_eq!(s.scripts, ["target/x/setup.sh"]);
        assert!(!s.covers("sh target/x/setup.sh && cargo xtask shot b.png"), "another file: asked");
    }

    /// #93: a line of several commands gets a pattern for each part — a script run by its path as it is
    /// now, a pattern for the rest, nothing for a part that runs unasked — or none when a part can't.
    #[test]
    fn a_line_of_a_script_and_a_command_gets_a_pattern_for_each() {
        let p = |c: &str| super::again(c);
        let scripts = |c: &str| p(c).map(|a| a.scripts);
        let shot = "sh target/x/setup.sh && env TARAE_SHOT_XDG=/tmp/x TARAE_BACKGROUND=dark cargo xtask shot \
                    target/x/2.png '@?ready'";
        let a = p(shot).expect("an a");
        assert_eq!(a.scripts, ["target/x/setup.sh"]);
        assert_eq!(a.scripts_said(), " and `target/x/setup.sh` as it is now");
        assert!(a.covers(shot) && !a.covers(&shot.replace("2.png", "3.png")), "#193: this line, exactly");
        assert_eq!(scripts("./run.sh fast"), Some(vec!["./run.sh".to_string()]));
        assert_eq!(
            scripts("cd sub && bash gen.sh && cargo test && ls"),
            Some(vec!["gen.sh".to_string()]),
            "the check and the read are in its words"
        );
        // (#193: the same line exactly, `rm` and all, is what was said yes to)
        assert!(p("sh x.sh && rm -rf y").is_some());
        for c in [
            "sh x.sh && python3 y.py",
            "sh -c 'sh x.sh'",
            "sh -x x.sh",
            "sh x*.sh && cargo xtask shot",
            "sh x.sh && env -i cargo xtask shot",
            "sh x.sh && PATH=/x cargo xtask shot",
        ] {
            assert_eq!(p(c), None, "{c}");
        }
        // Review: a program by its path is no script — a runner's would let anything through it, and a
        // binary's hash never changes; nothing outside the checkout either
        for c in [
            "/bin/sh -c 'echo hi'",
            "/bin/sh x.sh",
            "/usr/bin/env FOO=1 cargo test",
            "/usr/bin/python3 tools/gen.py",
            "./node_modules/.bin/sh -c 'echo hi'",
            "bash /bin/sh",
            "sh /usr/bin/env",
            "/bin/rm -rf x",
            "/usr/bin/curl -o x y",
            "../elsewhere/x.sh",
            "sh ../x.sh",
        ] {
            // (#193: and no `a` at all — what's at that path could be anything next time)
            assert_eq!(p(c), None, "{c}");
        }
        // A rule of the user's (`llm.run-allow`) never lets a script through: what `a` allows of one is its
        // line exactly, the script as it was (`read_only`'s pins)
        let rule = vec!["TARAE_SHOT_XDG= TARAE_BACKGROUND= cargo xtask shot".to_string()];
        assert!(!super::chain_allowed(shot, &rule), "a script isn't a check");
    }

    /// #93: `a` on a scripted line's card allows its script as it is with the line — the same line runs again
    /// unasked; the script changed, and it asks again. #193 (user decision 2026-10-06): the line exactly —
    /// one argument changed asks (#93's prefix let `… target/x/2.png` through).
    #[test]
    fn a_scripted_line_allowed_for_the_chat_runs_again_unasked_while_its_script_is_the_same() {
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("tarae-run-script-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("target/x")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        std::fs::write(dir.join("target/x/setup.sh"), "touch set-up\n").unwrap();
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        crate::chat::open(&mut ed);
        let c = ed.session.chat.as_mut().unwrap();
        c.state = crate::chat::State::Thinking;
        c.focused = false;
        let generation = c.generation();
        let line =
            |n: u32| format!("sh target/x/setup.sh && env TARAE_BACKGROUND=dark echo shot target/x/{n}.png");
        let pump = |ed: &mut crate::editor::Editor, done: &dyn Fn(&crate::editor::Editor) -> bool| {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !done(ed) {
                assert!(std::time::Instant::now() < end, "waited too long");
                if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(20)) {
                    ed.handle_event(ev);
                }
            }
        };
        let ended = |ed: &crate::editor::Editor| {
            ed.session.task_run.as_ref().is_some_and(|r| r.state != crate::task::State::Running)
        };
        // The first time: a card, offering both
        super::run(&mut ed, super::Call::of(generation, "r1", json!(1)), &json!({ "command": line(1) }))
            .unwrap();
        pump(&mut ed, &|ed| !ed.offers.is_empty());
        let q = ed.offers[0].question.clone();
        assert!(
            q.contains(
                "a — this exact command and `target/x/setup.sh` as it is now again unasked, this chat"
            ),
            "{q}"
        );
        ed.offer_for_chat();
        pump(&mut ed, &ended);
        assert!(dir.join("set-up").exists(), "it ran");
        assert_eq!(ed.session.chat.as_ref().unwrap().run_scripts.len(), 1, "the script, as it was");
        // Again, the same line: unasked
        ed.session.task_run = None;
        super::run(&mut ed, super::Call::of(generation, "r2", json!(2)), &json!({ "command": line(1) }))
            .unwrap();
        pump(&mut ed, &|ed| ed.session.task_run.is_some() || !ed.offers.is_empty());
        assert!(ed.offers.is_empty(), "no card: {:?}", ed.offers.first().map(|o| &o.question));
        pump(&mut ed, &ended);
        // Another file: asked (#193)
        ed.session.task_run = None;
        super::run(&mut ed, super::Call::of(generation, "r5", json!(5)), &json!({ "command": line(2) }))
            .unwrap();
        pump(&mut ed, &|ed| ed.session.task_run.is_some() || !ed.offers.is_empty());
        assert!(ed.session.task_run.is_none() && !ed.offers.is_empty(), "another line: its card");
        ed.offer_dismiss();
        // The script changed: asked again
        std::fs::write(dir.join("target/x/setup.sh"), "touch set-up-again\n").unwrap();
        ed.session.task_run = None;
        super::run(&mut ed, super::Call::of(generation, "r3", json!(3)), &json!({ "command": line(1) }))
            .unwrap();
        pump(&mut ed, &|ed| ed.session.task_run.is_some() || !ed.offers.is_empty());
        assert!(ed.session.task_run.is_none() && !ed.offers.is_empty(), "a card for the changed script");
        assert!(!dir.join("set-up-again").exists());
        // Review: a shell pinned by its path (as the old rule let `a` do) lets nothing through it
        ed.offer_dismiss();
        let sh = std::fs::canonicalize("/bin/sh").unwrap();
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            std::fs::read(&sh).unwrap().hash(&mut h);
            h.finish()
        };
        ed.session.chat.as_mut().unwrap().run_scripts.push((sh, hash));
        let marker = dir.join("through-sh");
        let through = format!("/bin/sh -c 'touch {}'", marker.display());
        super::run(&mut ed, super::Call::of(generation, "r4", json!(4)), &json!({ "command": through }))
            .unwrap();
        pump(&mut ed, &|ed| ed.session.task_run.is_some() || !ed.offers.is_empty());
        assert!(ed.session.task_run.is_none() && !ed.offers.is_empty(), "a card, not a run");
        assert!(!marker.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #44: assignments in front of the command — in a run card's `a` (#193) with their values, exactly;
    /// never a name that finds programs or libraries, or one the sandbox sets. A rule of the user's
    /// (`llm.run-allow`, `NAME=` words) takes the names it lists, any values (#132: a name it doesn't list
    /// asks).
    #[test]
    fn assignments_in_front_go_into_the_pattern_by_name() {
        let p = |c: &str| super::again(c);
        let shot =
            "sh -c 'TARAE_SHOT_XDG=/tmp/x TARAE_BACKGROUND=dark cargo xtask shot /tmp/a.png \"@?ready\"'";
        let a = p(shot).expect("an a");
        assert_eq!(a.parts[0][..2], ["TARAE_SHOT_XDG=/tmp/x", "TARAE_BACKGROUND=dark"]);
        assert!(
            a.covers("TARAE_SHOT_XDG=/tmp/x TARAE_BACKGROUND=dark cargo xtask shot /tmp/a.png '@?ready'")
        );
        assert!(!a.covers(&shot.replace("dark", "light")), "another value");
        assert!(!a.covers("cargo xtask shot /tmp/a.png '@?ready'"), "no names");
        assert!(p("FOO=1 cargo test").is_some_and(|a| a.covers("FOO=1 cargo test")), "not only in sh -c");
        assert!(p("A=1 env cargo test").is_some_and(|a| !a.covers("A=1 cargo test")), "its words, `env` too");
        for c in [
            "PATH=/tmp/bin cargo xtask shot",
            "LD_PRELOAD=/tmp/x.so cargo xtask shot",
            "DYLD_INSERT_LIBRARIES=/tmp/x cargo xtask shot",
            "sh -c 'A=1 HOME=/tmp cargo xtask shot'",
            "TMPDIR=/x cargo xtask shot",
            "TARAE_SANDBOXED= cargo xtask shot",
            "A=1 env -i cargo test",
            "'A=1' cargo test",
            "A=$(id) cargo test",
            "A=1 B=2",
            "A=1 cd src",
        ] {
            assert_eq!(p(c), None, "{c}");
        }
        let chat = vec!["TARAE_SHOT_XDG= TARAE_BACKGROUND= cargo xtask shot".to_string()];
        assert!(super::allowed(shot, &chat));
        assert!(super::allowed("TARAE_BACKGROUND=light cargo xtask shot /tmp/b.png", &chat), "fewer names");
        assert!(super::allowed("cargo xtask shot /tmp/b.png", &chat), "none");
        assert!(!super::allowed("RUSTC_WRAPPER=/tmp/w cargo xtask shot x", &chat), "a never-name");
        // #132: a name the card didn't show asks again — `CC` runs a program from a build script, and no list
        // of such names is whole; that card's `a` names them all, and covers it then
        let more =
            "TARAE_SHOT_XDG=/tmp/x TARAE_BACKGROUND=dark TARAE_SHOT_COLS=120 cargo xtask shot /tmp/c.png";
        for c in [
            more,
            "env TARAE_SHOT_XDG=/tmp/x TARAE_BACKGROUND=dark TARAE_SHOT_COLS=120 cargo xtask shot /tmp/c.png",
            "sh -c 'RUST_LOG=debug cargo xtask shot /tmp/d.png'",
            "TARAE_BACKGROUND=dark CC=/tmp/evil cargo xtask shot /tmp/e.png",
        ] {
            assert!(!super::allowed(c, &chat) && !super::chain_allowed(c, &chat), "{c}");
        }
        let wider = vec!["TARAE_SHOT_XDG= TARAE_BACKGROUND= TARAE_SHOT_COLS= cargo xtask shot".to_string()];
        assert!(super::allowed(more, &wider) && super::allowed(shot, &wider), "all three, or fewer");
        assert!(
            super::chain_allowed("env TARAE_BACKGROUND=dark cargo xtask shot /tmp/f.png", &chat),
            "fewer names through env, in a line"
        );
        for c in [
            "TARAE_BACKGROUND=dark CARGO_ALIAS_XTASK=run cargo xtask shot",
            "TARAE_BACKGROUND=dark RUSTUP_TOOLCHAIN=./tc cargo xtask shot",
            "TARAE_BACKGROUND=dark CARGO_HOME=/tmp/h cargo xtask shot",
        ] {
            assert!(!super::allowed(c, &chat), "{c}");
        }
        // A rule that sets no names takes none
        assert!(!super::allowed("A=1 cargo xtask shot", &["cargo xtask shot".to_string()]));
        assert!(!super::allowed("TARAE_BACKGROUND=dark cargo xtask perf", &chat));
        assert!(!super::allowed("PATH=/tmp TARAE_BACKGROUND=dark cargo xtask shot", &chat));
        // The built-in checks don't take names: `RUSTC_WRAPPER` runs a program of its own
        assert!(!super::allowed("RUSTC_WRAPPER=/tmp/w cargo build", &[]));
        assert!(!super::allowed("A=1 cargo build", &["A= make".to_string()]));
        // A rule written by hand can't let a never-name in
        assert!(!super::allowed("PATH=/tmp cargo test", &["PATH= cargo test".to_string()]));
        // The read-only reading still refuses assignments
        assert_eq!(crate::readonly::parse("PAGER=sh git log"), None);
    }

    /// #32: a check piped only into filters that read is the check — its words, for the rules, the
    /// pattern and the list; anything else in the line, or a filter's option that writes, runs or never
    /// ends, and it isn't.
    #[test]
    fn a_check_piped_into_filters_is_the_check() {
        let f = |c: &str| super::filtered(c).map(|w| w.join(" "));
        let yours = "sh -c 'cargo test -p tarae --lib 2>&1 | grep -E \"FAILED|panicked\" | head -40'";
        assert_eq!(f(yours).as_deref(), Some("cargo test -p tarae --lib"));
        for c in [
            "cargo build 2>&1 | tail -n 20",
            "cargo test | sort | uniq -c | cut -c1-80",
            "go test ./... | grep -v '^ok'",
            "cargo clippy 2>&1 | wc -l",
        ] {
            assert!(f(c).is_some(), "{c}");
        }
        for c in [
            "cargo test",
            "cargo test | sh",
            "cargo test | tee out",
            "cargo test | xargs rm",
            "cargo test | tail -f",
            "cargo test | sort -o out",
            "cargo test | uniq a out",
            "cargo test | grep x > out",
            "cargo test; grep x",
            "cargo test && head x",
            "cargo test || head",
            "cd src | grep x",
            "cargo test src/* | head",
            "grep x | cargo test",
        ] {
            assert_eq!(f(c), None, "{c}");
        }
        assert!(super::is_check(yours), "it still feeds the list");
        assert!(!super::is_check("grep -rn x src | head"), "a grep through a filter isn't a check");
        assert!(!super::is_check("cargo xtask shot x.png | head"));
        // (a run card's `a`: the command before its filters — #193)
        let shot = "sh -c 'cargo xtask shot /tmp/x.png 2>&1 | grep -E \"a|b\" | tail -3'";
        assert!(super::again(shot).is_some_and(|a| a.covers("cargo xtask shot /tmp/x.png")));
    }

    /// Only checks and builds feed the user's list (#28): every command in the line one of them; Claude's
    /// looking around (a grep, a script) doesn't.
    #[test]
    fn what_feeds_the_users_list() {
        for c in [
            "cargo build",
            "cargo test --bin tarae -- lsp",
            "cargo clippy --all-targets",
            "cd sub && cargo check",
            "sh -c 'cargo build 2>&1'",
            "make -j4",
            "go vet ./...",
        ] {
            assert!(super::is_check(c), "{c}");
        }
        for c in [
            "grep -rn fn src",
            "rg -n x",
            "sh target/sb/x.sh",
            "cargo xtask shot x.png",
            "cargo build && grep -n x src/a.rs",
            "cargo testx",
            "cd src",
            "",
            "cargo build > out",
            // #38: the line whose matches were said to be in the user's list
            r#"sh -c 'grep -rn "pub fn proc_pid_rusage\|ri_phys_footprint" ~/.cargo/registry/src/*/libc-0.2.*/src/unix/bsd/apple/mod.rs | head -5'"#,
            "grep -rn x src | head -5",
            "sh -c 'grep -rn x src | head -5'",
        ] {
            assert!(!super::is_check(c), "{c}");
        }
    }

    /// The files a command names among those written unseen: read as the shell joins words (quotes and
    /// backslashes gone), split at its operators, relative to the folder it runs in; one that can't be
    /// shown as it is stops it.
    #[test]
    fn a_command_names_the_unseen_files_it_runs() {
        let dir = std::env::temp_dir().join(format!("tarae-named-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("tools")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let (x, g, seen) = (dir.join("x.sh"), dir.join("tools/gen.py"), dir.join("seen.sh"));
        std::fs::write(&x, "echo x\n").unwrap();
        std::fs::write(&g, "print(1)\n").unwrap();
        std::fs::write(&seen, "echo seen\n").unwrap();
        let unseen = vec![x.clone(), g.clone()];
        let names = |cmd: &str| -> Vec<std::path::PathBuf> {
            super::named(&dir, cmd, &dir, &unseen).unwrap().into_iter().map(|(p, _)| p).collect()
        };
        assert_eq!(names("sh x.sh"), vec![x.clone()]);
        assert_eq!(names("sh 'x'.sh"), vec![x.clone()], "quotes joined as the shell does");
        assert_eq!(names("sh x\\.sh && python3 \"tools/gen.py\""), [x.clone(), g.clone()]);
        assert_eq!(names("bash -c \"$(cat ./tools/../x.sh)\""), vec![x.clone()]);
        assert_eq!(names("F=x.sh; sh $F"), vec![x.clone()]);
        assert_eq!(names(&format!("sh {}", x.display())), vec![x.clone()], "a full path");
        assert!(names("sh seen.sh x.sh.bak").is_empty(), "only what was written unseen");
        assert_eq!(names("sh x.sh x.sh"), vec![x.clone()], "once each");
        // #76: moving, copying or removing runs nothing — unless the line runs something else too
        for c in ["mv x.sh y.sh", "git mv tools/gen.py tools/g.py", "rm x.sh", "cp x.sh x2.sh && ls"] {
            assert!(names(c).is_empty(), "{c}");
        }
        assert_eq!(names("cp x.sh y.sh && sh y.sh"), vec![x.clone()], "what it moved may run");
        assert_eq!(names("./x.sh"), vec![x.clone()]);
        assert_eq!(names("source x.sh"), vec![x.clone()]);
        std::fs::write(&x, "echo a\u{202E}b\n").unwrap();
        let e = super::named(&dir, "sh x.sh", &dir, &unseen).unwrap_err();
        assert!(e.contains("can't show"), "{e}");
        std::fs::write(&x, "echo x\r\n").unwrap();
        assert_eq!(names("sh x.sh"), vec![x.clone()], "CRLF is fine");
        // The limit it crossed, not every limit: too many lines · one line too long (#15: a 12-line script
        // with a long line was told "12 lines; at most 16")
        std::fs::write(&x, "a\n".repeat(super::SHOWN_LINES_MAX + 1)).unwrap();
        let e = super::named(&dir, "sh x.sh", &dir, &unseen).unwrap_err();
        assert!(
            e.contains("too long to show") && e.contains("it has 17 lines — a card shows at most 16"),
            "{e}"
        );
        assert!(!e.contains("characters"), "{e}");
        std::fs::write(&x, format!("echo a\n{}\necho b\n", "a".repeat(super::SHOWN_LINE_MAX + 1))).unwrap();
        let e = super::named(&dir, "sh x.sh", &dir, &unseen).unwrap_err();
        assert!(e.contains("its line 2 is 161 characters — a card shows lines of at most 160"), "{e}");
        assert!(!e.contains("lines —"), "3 lines isn't the reason: {e}");
        std::fs::write(&x, format!("{}{}\n", "a\n".repeat(20), "a".repeat(200))).unwrap();
        let e = super::named(&dir, "sh x.sh", &dir, &unseen).unwrap_err();
        assert!(e.contains("21 lines") && e.contains(", and its line 21 is 200 characters"), "both: {e}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn run_verdict_in_a_word_or_two() {
        let v = super::run_verdict;
        assert_eq!(v("exit 0 in 1.2s\n$ cargo build\n"), "ok");
        assert_eq!(v("exit 101 in 3.0s\n$ cargo test\n"), "exit 101");
        assert_eq!(v("killed in 3.0s\n$ cargo test\n"), "killed");
        assert_eq!(v("the user chose not to run `git push`"), "not run");
        assert_eq!(v("the command was stopped — the user stopped it"), "not run");
    }

    /// Long output comes as its start and its end, a line too long cut; the places come first.
    #[test]
    fn run_report_says_which_tests_skipped_themselves() {
        use crate::task::{Run, Task};
        let mut r = Run::for_test(Task { name: "t".into(), cmd: "cargo test".into(), cwd: "/p".into() });
        r.output = vec![
            (false, "test sandbox::tests::real ... ok".into()),
            (true, "tarae-test: skipped sandbox::tests::real — Seatbelt can't start inside a sandbox".into()),
            (false, "test result: ok. 1 passed".into()),
        ];
        let text = super::run_report(elsewhere(), &r, &[], super::Waited::No);
        assert!(
            text.contains("1 test skipped itself — libtest says ok, but nothing was checked:\n  sandbox::tests::real — Seatbelt"),
            "{text}"
        );
        r.output.retain(|(err, _)| !err);
        assert!(!super::run_report(elsewhere(), &r, &[], super::Waited::No).contains("skipped"));
    }

    /// #38: a run that isn't a check or a build (a grep) leaves the user's list alone (#28) — and its report
    /// says so: its `path:line:` hits are places in its output, not "errors in the user's list" (that claim
    /// misled both sides into thinking the list had been replaced).
    #[test]
    fn a_greps_report_says_its_places_are_not_in_the_users_list() {
        use crate::task::{Run, State, Task};
        let cmd = r#"sh -c 'grep -rn "pub fn proc_pid_rusage\|ri_phys_footprint" ~/.cargo/registry/src/*/libc-0.2.*/src/unix/bsd/apple/mod.rs | head -5'"#;
        let mut r = Run::for_test(Task { name: "grep".into(), cmd: cmd.into(), cwd: "/p".into() });
        r.feeds = super::is_check(cmd);
        r.state = State::Passed;
        r.found = (5, 0);
        let hit = |line: usize| crate::quickfix::Item {
            path: "/c/libc/src/unix/bsd/apple/mod.rs".into(),
            line,
            col: 0,
            message: "pub ri_phys_footprint: u64,".into(),
            severity: 1,
            mark: None,
        };
        let places: Vec<_> = [957, 971, 991, 1013, 1044].into_iter().map(hit).collect();
        let text = super::run_report(elsewhere(), &r, &places, super::Waited::No);
        assert!(!r.feeds, "a grep isn't a check");
        assert!(!text.contains("user's list (]q)") && !text.contains("error:"), "{text}");
        assert!(
            text.contains(
                "5 places in its output (not in the user's list — only checks and builds go there):\n  \
                 /c/libc/src/unix/bsd/apple/mod.rs:958:1 pub ri_phys_footprint: u64,\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn run_report_keeps_the_start_and_the_end() {
        use crate::task::{Run, State, Task};
        let mut r = Run::for_test(Task { name: "b".into(), cmd: "cargo build".into(), cwd: "/p".into() });
        r.state = State::Failed(Some(101));
        r.output = (0..1000).map(|i| (false, format!("line {i}"))).collect();
        r.output[0].1 = "x".repeat(2000);
        r.found = (1, 0);
        let place = crate::quickfix::Item {
            path: "/p/src/a.rs".into(),
            line: 4,
            col: 2,
            message: "mismatched types".into(),
            severity: 1,
            mark: None,
        };
        let text = super::run_report(elsewhere(), &r, &[place], super::Waited::No);
        assert!(text.starts_with("exit 101\n$ cargo build\n"), "{text}");
        assert!(
            text.contains(
                "1 errors, 0 warnings in the user's list (]q):\n  /p/src/a.rs:5:3 error: mismatched types\n"
            ),
            "{text}"
        );
        assert!(text.contains(&format!("{}…\n", "x".repeat(super::LINE_MAX))), "a long line is cut");
        assert!(
            text.contains("line 99\n… 750 lines …\nline 850\n") && text.ends_with("line 999\n"),
            "{text}"
        );
        assert!(!text.contains("line 100\n"));
        r.output.truncate(3);
        r.output[0].1 = "line 0".into();
        r.state = State::Passed;
        let text = super::run_report(elsewhere(), &r, &[], super::Waited::No);
        assert!(text.contains("Output:\n") && text.ends_with("line 2\n") && !text.contains("…"), "{text}");
    }

    /// Past the first ten failures only names come — but every one of them, up to fifty.
    #[test]
    fn failures_past_the_first_ten_come_by_name() {
        use crate::test_results::{Case, Status};
        let case = |i: usize| Case {
            name: format!("t{i}"),
            status: Status::Failed,
            secs: None,
            message: vec![format!("why {i}")],
            at: None,
        };
        let cases: Vec<Case> = (0..70).map(case).collect();
        let few = super::failures(elsewhere(), &cases, &(0..12).collect::<Vec<_>>());
        assert!(few.contains("FAILED t9\n  why 9\n"), "{few}");
        assert!(few.contains("\n2 more failed (names only):\n  t10\n  t11\n"), "{few}");
        assert!(!few.contains("why 10"), "{few}");
        let many = super::failures(elsewhere(), &cases, &(0..70).collect::<Vec<_>>());
        assert!(many.contains("60 more failed") && many.contains("  t59\n"), "{many}");
        assert!(!many.contains("  t60\n") && many.ends_with("  … and 10 more\n"), "{many}");
        assert!(super::failures(elsewhere(), &cases, &[]).is_empty());
    }
}
