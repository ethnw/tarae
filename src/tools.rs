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
//! - `rename` and `replace` are the editor's own refactoring — one call, one change set that lands as
//!   Claude's edits do, in the review (`refactor.rs`).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::editor::Editor;
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
}

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
    let call = || Call { generation, request_id: request_id.to_string(), id: id.clone(), rev };
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
            ];
            Ok(json!({ "tools": tools.map(annotated) }))
        }
        "tools/call" => {
            let params = &msg["params"];
            let args = &params["arguments"];
            let r = match params["name"].as_str() {
                Some("note") => crate::notes::tool_call(ed, args),
                Some("plan") => crate::plan::tool_call(ed, args),
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
                Some(
                    name @ ("references" | "search" | "symbols" | "definition" | "hover" | "diagnostics"
                    | "rename" | "replace"),
                ) => {
                    let call = call();
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
                        Err(e) => Err(e),
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
fn annotated(mut schema: Value) -> Value {
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
fn content(r: Result<String, String>) -> Value {
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
        Call { generation: u64::MAX, request_id: String::new(), id: json!(id), rev: Default::default() }
    }
}

/// Answers a call that waited — only to the process that made it.
pub fn answer(ed: &mut Editor, call: Call, r: Result<String, String>) {
    #[cfg(test)]
    ANSWERED.with(|a| a.borrow_mut().push((call.id.clone(), r.clone())));
    if ed.chat.as_ref().is_none_or(|c| c.generation() != call.generation) {
        return;
    }
    let response = response(&call, r);
    crate::chat::respond(ed, &call.request_id, json!({ "mcp_response": response }));
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
/// the run's end (`tests_done`). Err = answered now.
fn run_tests(ed: &mut Editor, call: Call, args: &Value) -> Result<(), String> {
    let path = args["path"].as_str().filter(|p| !p.is_empty()).ok_or("path is required")?;
    let path = crate::chat::resolve(&ed.root, path);
    let line = args["line"].as_u64().map(|l| l.max(1) as usize - 1);
    busy(ed)?;
    // The buffer's text when it's open (unsaved edits are what gets saved and run)
    let text = ed.docs.iter().find(|d| d.path.as_deref() == Some(path.as_path())).map(|d| d.text.clone());
    let root = ed.root.clone();
    ed.events.jobs().spawn(move || {
        let found = find(&root, &path, text, line);
        move |ed: &mut Editor| match found {
            Ok(target) => start(ed, call, target),
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

/// Tests already running (the user's or an earlier call) aren't cut short.
fn busy(ed: &Editor) -> Result<(), String> {
    match &ed.test_run {
        Some(r) if r.state == RunState::Running => Err(format!(
            "tests are already running in the editor ({}) — wait for them to finish, then run again",
            r.target.label
        )),
        _ => Ok(()),
    }
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
    if let Err(e) = busy(ed) {
        return answer(ed, call, Err(e));
    }
    guarded(ed, call, Job::Tests(Box::new(target)), false);
}

fn start_tests(ed: &mut Editor, who: For, target: crate::testing::Target, guard: Option<Guard>) {
    if let Err(e) = busy(ed) {
        return fail(ed, who, e);
    }
    let before = ed.test_generation;
    let program = target.argv().first().cloned().unwrap_or_default();
    ed.test_start_guarded(target, guard);
    match ed.test_run.as_mut().filter(|_| ed.test_generation != before) {
        Some(r) => match who {
            For::Claude(call) => r.chat = Some(call),
            For::User(head) => r.tell = Some(head),
        },
        None => fail(ed, who, format!("couldn't start {program}")),
    }
}

/// The run Claude asked for ended — its results go back, and the editor shows the first failure. One the
/// user ran again (`blocked.rs`): Claude hears how it ended with the next message.
pub fn tests_done(ed: &mut Editor) {
    let Some(r) = ed.test_run.as_mut() else { return };
    if let Some(head) = r.tell.take() {
        let text = report(&ed.root, r);
        return told(ed, head, text);
    }
    let Some(call) = r.chat.take() else { return };
    let text = report(&ed.root, r);
    let failed = r.failures().first().map(|&i| r.cases[i].clone());
    answer(ed, call, Ok(text));
    if let Some(case) = failed
        && let Some((path, line)) = case.at.clone()
    {
        let why = case.message.iter().map(|m| m.trim()).filter(|m| !m.is_empty()).take(3).collect::<Vec<_>>();
        let why = why.join(" · ");
        crate::follow::point(ed, "failed", case.name, &path, line..line + 1, Some(why));
    }
}

/// The chat turn ended (stopped) with a run still going: it goes on for the user, unreported — and a
/// command still waiting for the user's yes is no longer asked about.
pub fn turn_over(ed: &mut Editor) {
    if let Some(r) = ed.test_run.as_mut() {
        r.chat = None;
    }
    if let Some(r) = ed.task_run.as_mut() {
        r.chat = None;
    }
    ed.offers
        .retain(|o| !matches!(o.what, crate::offer::What::Run(_) | crate::offer::What::NoSandbox { .. }));
    crate::seeing::turn_over(ed);
    crate::refactor::turn_over(ed);
}

/// A run Claude waited on is gone before it ended (closed, or another run took its place).
pub fn tests_dropped(ed: &mut Editor, why: &str) {
    if let Some(call) = ed.test_run.as_mut().and_then(|r| r.chat.take()) {
        answer(ed, call, Err(format!("the test run was stopped — {why}")));
    }
}

// ── run ─────────────────────────────────────────────────────────────────────

fn run_schema() -> Value {
    json!({
        "name": "run",
        "description": "Run a shell command in the project, in the user's editor, and get how it ended and \
    its output — build, lint, format, test suites, git. The user watches it in their task panel. Both output \
    streams come back whole (long output: its start and end), so don't redirect or pipe to head/tail for \
    that. Checks (cargo build/check/test/clippy/fmt, go build/test/vet, git status/diff/log/show) start at \
    once — one command, words only (quotes are fine), alone or piped into grep head tail wc sort cut uniq. \
    So do read-only ones (ls cat head tail wc grep rg find tree stat file du diff sort uniq cut pwd, git \
    blame/rev-parse/ls-files/branch) whose paths stay in the \
    project — and lines of them joined by ; && || |, with cd, or the same in sh -c '…' (no $, backticks or \
    redirects to files). To run somewhere other than the project root, give cwd rather than a cd. Any other \
    command waits for the user to allow it — if they say no, don't retry it, ask. No stdin and no terminal: \
    nothing interactive. For one test file or test, run_tests is better (it reads each failure). Commands \
    run in a sandbox; when one can't work in it (it needs the network, or to write where the sandbox \
    doesn't let it), you may ask to run it outside: outside_sandbox with your reason — the user decides on a \
    card, this once.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command line (run by sh -c)" },
                "cwd": { "type": "string", "description": "The folder it runs in, inside the project (relative to its root); the root if left out" },
                "outside_sandbox": { "type": "boolean", "description": "Ask the user to run it outside the sandbox, this once (with reason)" },
                "reason": { "type": "string", "description": "Why it can't run in the sandbox — required with outside_sandbox; the user reads it" }
            },
            "required": ["command"]
        }
    })
}

/// A reason to run outside the sandbox, at most (the card shows it whole).
const REASON_MAX: usize = 300;

/// Commands that run without asking: the checks, and git's read-only ones — by whole words, and only
/// when the line has nothing the shell would read as more than words (`allowed`).
pub const RUN_ALLOW: [&str; 13] = [
    "cargo build",
    "cargo check",
    "cargo test",
    "cargo clippy",
    "cargo fmt",
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

/// Whether `cmd` runs without asking: one command and nothing else (`readonly::one_command` — quoted words
/// are fine; expansion, redirection, chaining, globs or an assignment and the words don't say what runs),
/// starting with one of the `RUN_ALLOW` or `extra` prefixes word for word (`extra`: `llm.run-allow`, this
/// chat's patterns), and none of the options that make it write or run something.
pub fn allowed(cmd: &str, extra: &[String]) -> bool {
    if let Some(words) = crate::readonly::one_command(cmd) {
        return allowed_words(&words, extra);
    }
    // `NAME=value` words in front (#44): only a rule naming each of them lets it through (`A= cargo x`)
    let Some((names, words)) = crate::readonly::one_command_env(cmd) else { return false };
    if names.is_empty() || !names.iter().all(|n| crate::readonly::env_allowed(n)) {
        return false;
    }
    // (only the rules that name them — not the built-in checks: `RUSTC_WRAPPER=x cargo build` runs x)
    !words.iter().any(|w| WRITES.iter().any(|o| w.starts_with(o)))
        && extra.iter().any(|rule| {
            let (set, rest) = rule_env(rule);
            names.iter().all(|n| set.contains(&n.as_str())) && rule_matches(&words, &rest)
        })
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

/// Programs a check's output can go through and still count as the check (#32): they only read it.
const FILTERS: [&str; 7] = ["grep", "head", "tail", "wc", "sort", "cut", "uniq"];

/// The first command's words when `cmd` is one command piped into filters and nothing else (`cargo test
/// 2>&1 | grep -E "a|b" | head`): each filter one of `FILTERS` with no option that writes, runs or never
/// ends (`readonly::only_reads`). What the filters read besides their input is `readonly::outside`'s.
pub fn filtered(cmd: &str) -> Option<Vec<String>> {
    let mut parts = crate::readonly::piped(cmd)?.into_iter();
    let head = parts.next()?;
    let mut any = false;
    for f in parts {
        if !FILTERS.contains(&f[0].text.as_str()) || !crate::readonly::only_reads(&f) {
            return None;
        }
        any = true;
    }
    (any && !head.iter().any(|w| w.glob)).then(|| head.into_iter().map(|w| w.text).collect())
}

/// `allowed` for one command's words.
fn allowed_words(words: &[String], extra: &[String]) -> bool {
    if words.iter().any(|w| WRITES.iter().any(|o| w.starts_with(o))) {
        return false;
    }
    RUN_ALLOW
        .iter()
        .copied()
        .chain(extra.iter().map(String::as_str))
        .any(|rule| rule_matches(words, &rule_env(rule).1))
}

/// Commands whose places are worth the user's list (`]q`): builds, tests, linters — by whole words.
const CHECKS: [&str; 22] = [
    "cargo build",
    "cargo check",
    "cargo test",
    "cargo clippy",
    "cargo nextest",
    "cargo fmt",
    "cargo perf",
    "go build",
    "go test",
    "go vet",
    "gofmt -l",
    "make",
    "npm test",
    "npm run",
    "pnpm test",
    "pnpm run",
    "yarn test",
    "tsc",
    "eslint",
    "ruff check",
    "mypy",
    "pytest",
];

/// Whether a command Claude runs is a check or a build — every command in its line one of `CHECKS` (a `cd`
/// aside). Only those feed the user's list; a grep of Claude's looking around doesn't replace it (#28).
pub fn is_check(cmd: &str) -> bool {
    // (a check's output through filters is the check's: its places are what's left — #32)
    if let Some(head) = filtered(cmd) {
        return check_words(&head);
    }
    let Some(parts) = crate::readonly::parse(cmd) else { return false };
    let mut any = false;
    for p in &parts {
        let crate::readonly::Part::Cmd(w) = p else { continue };
        if !check_words(&w.iter().map(|w| w.text.clone()).collect::<Vec<_>>()) {
            return false;
        }
        any = true;
    }
    any
}

/// Whether one command's words start with one of `CHECKS`.
fn check_words(w: &[String]) -> bool {
    CHECKS.iter().any(|c| {
        let c: Vec<&str> = c.split(' ').collect();
        w.len() >= c.len() && w.iter().zip(&c).all(|(w, c)| w == c)
    })
}

/// Programs that run whatever follows them — a pattern ending in one would allow anything.
const RUNNERS: [&str; 22] = [
    "sh", "bash", "zsh", "dash", "fish", "env", "sudo", "doas", "xargs", "nohup", "time", "nice", "exec",
    "eval", "command", "builtin", "source", "python", "python3", "node", "ruby", "perl",
];

/// The pattern a run card can allow for the rest of the chat (`a`): the command's first plain words — no
/// option, path, value or glob — two or three of them (`cargo xtask shot`); a command piped into filters
/// gives its own (`filtered`, #32). None for a line of other commands, one word, or a runner (`sh x.sh`,
/// `env …`: anything could follow). `NAME=value` words in front go in by name (`A= B= cargo xtask shot`,
/// #44) — none for a name no pattern may set (`readonly::env_allowed`: PATH, LD_*, DYLD_* …).
pub fn pattern(cmd: &str) -> Option<String> {
    let (names, words) =
        crate::readonly::one_command_env(cmd).or_else(|| filtered(cmd).map(|w| (Vec::new(), w)))?;
    if !names.iter().all(|n| crate::readonly::env_allowed(n)) {
        return None;
    }
    let lead: Vec<&str> = words
        .iter()
        .map(String::as_str)
        .take_while(|w| {
            !w.is_empty()
                && !w.starts_with(['-', '.', '@', '+'])
                && !w.contains(['/', '=', ':', '@', '"', '\''])
        })
        .take(3)
        .collect();
    let runner = |w: &&str| RUNNERS.contains(w) || w.starts_with("python");
    // (git: what reads runs already; what writes goes to `.git`, which the sandbox denies)
    let git = lead.first() == Some(&"git");
    let names = names.iter().map(|n| format!("{n}="));
    (lead.len() >= 2 && !lead.iter().any(runner) && !git)
        .then(|| names.chain(lead.iter().map(|w| w.to_string())).collect::<Vec<_>>().join(" "))
}

/// A pattern as the card says it: each name's value left open (`A=… cargo xtask shot`).
pub fn pattern_shown(p: &str) -> String {
    let (set, rest) = rule_env(p);
    set.iter()
        .map(|n| format!("{n}=…"))
        .chain(rest.iter().map(|w| w.to_string()))
        .collect::<Vec<_>>()
        .join(" ")
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
    // Outside the sandbox: always asked, with Claude's reason on the card — a check or a read-only command too
    let outside = match args["outside_sandbox"].as_bool() == Some(true) {
        true => {
            let reason = args["reason"].as_str().map(str::trim).filter(|r| !r.is_empty()).ok_or(
                "outside_sandbox needs a reason: why it can't run in the sandbox — the user reads it on the card",
            )?;
            if reason.chars().count() > REASON_MAX {
                return Err(format!("keep the reason to {REASON_MAX} characters — the card shows it whole"));
            }
            Some(reason.to_string())
        }
        false => None,
    };
    run_busy(ed)?;
    let cwd = run_cwd(&ed.root, args["cwd"].as_str())?;
    let mut extra = ed.config.llm.run_allow.clone();
    extra.extend(ed.chat.as_ref().map(|c| c.run_allow.clone()).unwrap_or_default());
    if outside.is_none() && allowed(cmd, &extra) {
        guarded(ed, call, Job::Command(cmd.to_string(), cwd), false);
        return Ok(());
    }
    // (a check piped into filters: the check's rule, and what the filters name is read as any read's — #32)
    let checked = filtered(cmd).is_some_and(|head| allowed_words(&head, &extra));
    if outside.is_none() && (checked || crate::readonly::read_only(cmd)) {
        return read_only(ed, call, cmd.to_string(), cwd);
    }
    asked(ed)?;
    // Review at the end: a file it names that Claude wrote unseen this turn goes on the card with it —
    // which ones, and their text, found on a worker
    let unseen: Vec<PathBuf> =
        ed.checkpoints.turn.iter().filter(|t| t.unseen).map(|t| t.path.clone()).collect();
    let cmd = cmd.to_string();
    if unseen.is_empty() {
        return ask(ed, cmd, cwd, call, Vec::new(), outside);
    }
    let root = ed.root.clone();
    ed.events.jobs().spawn(move || {
        let shown = named(&root, &cmd, &cwd, &unseen);
        move |ed: &mut Editor| {
            // (the turn ended meanwhile: nobody waits for a card)
            if ed.chat.as_ref().is_some_and(|c| c.generation() != call.generation || !c.busy()) {
                return;
            }
            let c = call.clone();
            if let Err(e) = shown.and_then(|shown| ask(ed, cmd, cwd, call, shown, outside)) {
                answer(ed, c, Err(e));
            }
        }
    });
    Ok(())
}

/// Where a command runs: the project root (`root`, a real path), or `cwd` in it (relative to the root, or
/// absolute) — a folder that's there, inside the project (real path), so a `cd` isn't needed (#27).
fn run_cwd(root: &Path, cwd: Option<&str>) -> Result<PathBuf, String> {
    let Some(cwd) = cwd.map(str::trim).filter(|c| !c.is_empty() && *c != ".") else {
        return Ok(root.to_path_buf());
    };
    let dir = std::fs::canonicalize(root.join(cwd)).map_err(|e| format!("cwd {cwd}: {e}"))?;
    if !dir.is_dir() {
        return Err(format!("cwd {cwd} isn't a folder"));
    }
    if !dir.starts_with(root) {
        return Err(format!("cwd {cwd} is outside the project — commands run in it"));
    }
    Ok(dir)
}

/// A command that only reads (`readonly.rs`): where it reads is found on a worker — all of it in the
/// project, or in places allowed, and it runs now; anything else is on its card.
fn read_only(ed: &mut Editor, call: Call, cmd: String, cwd: PathBuf) -> Result<(), String> {
    let chat = ed.chat.as_ref().map(|c| c.allowed.clone()).unwrap_or_default();
    let (store, root) = (ed.allow_path.clone(), ed.root.clone());
    ed.events.jobs().spawn(move || {
        let kept = crate::allow::Store::read(store.as_deref()).get(&root);
        let reads = crate::readonly::outside(&cmd, &cwd, &root, &chat, &kept);
        move |ed: &mut Editor| {
            // (the turn ended meanwhile: nobody waits for it)
            if ed.chat.as_ref().is_some_and(|c| c.generation() != call.generation || !c.busy()) {
                return;
            }
            // (a `cd` into what isn't a folder: where the rest reads can't be told — the plain card)
            let reads = match reads {
                Ok(r) if r.is_empty() => return guarded(ed, call, Job::Command(cmd, cwd), false),
                Ok(r) => r,
                Err(_) => Vec::new(),
            };
            let c = call.clone();
            if let Err(e) = ask_with(ed, cmd, cwd, call, Vec::new(), reads, None) {
                answer(ed, c, Err(e));
            }
        }
    });
    Ok(())
}

/// One command asked about at a time.
fn asked(ed: &Editor) -> Result<(), String> {
    match ed.offers.iter().any(|o| matches!(o.what, crate::offer::What::Run(_))) {
        true => Err("the user hasn't answered your last command yet — wait for it, then run this".into()),
        false => Ok(()),
    }
}

/// The card for `cmd` — with each file in `shown` (path, text) whole under it; `outside`: Claude asks to run
/// it outside the sandbox, for this reason.
fn ask(
    ed: &mut Editor,
    cmd: String,
    cwd: PathBuf,
    call: Call,
    shown: Vec<(PathBuf, String)>,
    outside: Option<String>,
) -> Result<(), String> {
    ask_with(ed, cmd, cwd, call, shown, Vec::new(), outside)
}

pub const OUTSIDE_RUN_TITLE: &str = "Claude wants to run — outside the sandbox";

/// The card for `cmd`, saying too what it reads outside the project (`reads` — a read-only command's):
/// those can be allowed for longer, as on a read's card, unless one of them never can. Outside the sandbox
/// (`outside` = Claude's reason): its own title, the reason, what that means — once, never kept. Its
/// pattern (`pattern`) can be allowed for the rest of the chat with `a` — said on the card.
fn ask_with(
    ed: &mut Editor,
    cmd: String,
    cwd: PathBuf,
    call: Call,
    shown: Vec<(PathBuf, String)>,
    reads: Vec<PathBuf>,
    outside: Option<String>,
) -> Result<(), String> {
    asked(ed)?;
    let mut question = cmd.clone();
    // What's decided: the command (and where it runs, when not at the root), what it reads outside (one
    // place a line)
    let mut decided = 0..cmd.split('\n').count();
    if let Ok(rel) = cwd.strip_prefix(&ed.root).map(Path::to_path_buf)
        && !rel.as_os_str().is_empty()
    {
        question.push_str(&format!("\n  in {}", rel.display()));
        decided.end += 1;
    }
    // (outside: the reason faint — Claude's own words — and what yes means in the warning color)
    let mut warning = None;
    if let Some(reason) = &outside {
        question.push_str(&format!("\n\nIts reason: {}\n\n", reason.replace('\n', " ")));
        let at = question.split('\n').count() - 1;
        question.push_str(
            "Outside the sandbox it runs with all your permissions — your files, your network — this once.",
        );
        warning = Some(at..at + 1);
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
    // (a command that only reads is asked for where it reads, not what it is)
    let pattern = pattern(&cmd).filter(|_| outside.is_none() && reads.is_empty());
    if let Some(p) = &pattern {
        question.push_str(&format!("\n\na — also `{} …` unasked, this chat", pattern_shown(p)));
    }
    for (path, text) in &shown {
        let text = text.replace("\r\n", "\n").replace('\t', "    ");
        question.push_str(&format!(
            "\n\n{} — written this turn, unseen:\n{}",
            crate::chat::shown(&ed.root, path),
            text.trim_end_matches('\n')
        ));
    }
    // Where there's no sandbox, this card is where the user hears it (outside one, it said so already)
    let unprotected = match (&ed.sandbox, ed.config.llm.sandbox) {
        _ if outside.is_some() => None,
        (Availability::Missing(why), true) => Some(format!("No sandbox here ({why})")),
        (_, false) => Some("The sandbox is off (llm.sandbox)".to_string()),
        _ => None,
    };
    if let Some(u) = &unprotected {
        question.push_str(&format!("\n\n{u} — it runs with all your permissions."));
    }
    let title = match outside {
        Some(_) => OUTSIDE_RUN_TITLE.to_string(),
        None => "Claude wants to run".to_string(),
    };
    let card = RunCard {
        cmd,
        cwd,
        call,
        shown,
        unprotected: unprotected.is_some(),
        reads,
        keep,
        outside: outside.is_some(),
        pattern,
    };
    let what = crate::offer::What::Run(Box::new(card));
    let o = crate::offer::Offer::new(title, &question, "Running…", "Claude's command".into(), what);
    let o = o.deciding(decided);
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
    let hidden = |c: char| {
        (c.is_control() && c != '\n' && c != '\t')
            || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
    };
    if text.replace("\r\n", "\n").chars().any(hidden) {
        return Err(format!("{unseen} it has characters a card can't show as they are"));
    }
    Ok(text)
}

/// A task running in the panel isn't cut short (the user's, or an install).
fn run_busy(ed: &Editor) -> Result<(), String> {
    match &ed.task_run {
        Some(r) if r.state == crate::task::State::Running => Err(format!(
            "a command is already running in the editor ({}) — wait for it to finish, then run again",
            r.task.name
        )),
        _ => Ok(()),
    }
}

fn run_start(
    ed: &mut Editor,
    who: For,
    cmd: String,
    cwd: PathBuf,
    guard: Option<Guard>,
) -> Result<(), String> {
    let task = crate::task::Task { name: cmd.clone(), cmd, cwd };
    if !ed.task_start_for(task, who, guard) {
        return Err(run_busy(ed).err().unwrap_or_else(|| "couldn't start sh".into()));
    }
    Ok(())
}

/// Who waits for a run's end: Claude's call — or no one: the user ran it again from what the sandbox
/// blocked (`blocked.rs`), and Claude hears how it ended with the next message (`head` says what they did).
#[derive(Clone, Debug)]
pub enum For {
    Claude(Call),
    User(String),
}

/// What couldn't start, said to whoever waits for it.
fn fail(ed: &mut Editor, who: For, e: String) {
    match who {
        For::Claude(call) => answer(ed, call, Err(e)),
        For::User(_) => ed.set_error(e),
    }
}

/// A run the user started again ended: Claude reads how with the next message — the panel shows it now.
fn told(ed: &mut Editor, head: String, report: String) {
    ed.checkpoints.tell.push(format!("{head}\n{report}"));
    ed.note("Claude hears how it ended with your next message");
}

/// What a run may do besides what's allowed (`allow.rs`): a grant just this once, or no sandbox at all.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Extra {
    pub grant: Option<crate::allow::Grant>,
    pub outside: bool,
}

/// The settings a run's policy is made from with what's allowed added — kept for the project, for this
/// conversation, and `once` — and how its result says it.
pub fn with_allowed(
    llm: &crate::llm::LlmConfig,
    scopes: &[&crate::allow::Allowed],
    once: Option<&crate::allow::Grant>,
) -> (crate::llm::LlmConfig, Vec<String>) {
    use crate::allow::Grant;
    let mut llm = llm.clone();
    let mut granted: Vec<String> = Vec::new();
    let grants = scopes
        .iter()
        .flat_map(|a| {
            a.write.iter().map(|p| Grant::Write(p.clone())).chain(a.ports.iter().map(|n| Grant::Port(*n)))
        })
        .chain(once.cloned());
    for g in grants {
        match &g {
            Grant::Write(p) => llm.sandbox_write.push(p.display().to_string()),
            Grant::Port(n) => llm.sandbox_ports.push(*n),
        }
        let said = g.shown();
        if !granted.contains(&said) {
            granted.push(said);
        }
    }
    (llm, granted)
}

/// The user's answer on the card: `y` runs it — the files it showed read again first (on a worker), and not
/// run if one changed since — `a`/`A` too, and what it reads outside stays allowed (this chat · always);
/// None tells Claude no. `unprotected`: the card said there's no sandbox.
pub fn run_answer(ed: &mut Editor, id: u64, what: RunCard, scope: Option<crate::allow::Scope>) {
    let RunCard { cmd, cwd, call, shown, unprotected, reads, keep, outside, pattern } = what;
    ed.offer_finished(id, Ok(()));
    let Some(scope) = scope else {
        return answer(ed, call, Err(format!("the user chose not to run `{cmd}`")));
    };
    if keep {
        crate::allow::grant_reads(ed, &reads, scope);
    }
    // `a` (or `A` where reads are kept too): its pattern for the rest of this chat — never for good
    if let (Some(p), crate::allow::Scope::ThisChat | crate::allow::Scope::Always) = (&pattern, scope)
        && let Some(chat) = ed.chat.as_mut()
        && !chat.run_allow.contains(p)
    {
        chat.run_allow.push(p.clone());
        ed.note(format!("`{}` runs without asking for the rest of this chat", pattern_shown(p)));
    }
    if shown.is_empty() {
        return run_now(ed, cmd, cwd, call, unprotected, outside);
    }
    let root = ed.root.clone();
    ed.events.jobs().spawn(move || {
        let changed =
            shown.iter().find(|(p, text)| std::fs::read(p).ok().as_deref() != Some(text.as_bytes()));
        let changed = changed.map(|(p, _)| crate::chat::shown(&root, p));
        move |ed: &mut Editor| match changed {
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
    });
}

fn run_now(ed: &mut Editor, cmd: String, cwd: PathBuf, call: Call, unprotected: bool, outside: bool) {
    if let Err(e) = run_busy(ed) {
        return answer(ed, call, Err(e));
    }
    let extra = Extra { grant: None, outside };
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
    pub keep: bool,
    /// Claude asked to run it outside the sandbox (`y` runs it so, once).
    pub outside: bool,
    /// What `a` lets run without asking for the rest of the chat (`pattern`).
    pub pattern: Option<String>,
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
pub fn guard_and_go(ed: &mut Editor, who: For, job: Job, asked: bool, extra: Extra) {
    if extra.outside {
        return go(ed, who, job, Some(Guard::Outside));
    }
    if !ed.config.llm.sandbox {
        return go(ed, who, job, Some(Guard::Off));
    }
    let found = ed.sandbox.clone();
    if found == Availability::Bypass {
        return go(ed, who, job, None);
    }
    let llm = ed.config.llm.clone();
    let chat = ed.chat.as_ref().map(|c| c.allowed.clone()).unwrap_or_default();
    let (store, root) = (ed.allow_path.clone(), ed.root.clone());
    ed.events.jobs().spawn(move || {
        let found = match found {
            Availability::Unknown => crate::sandbox::probe(),
            f => f,
        };
        let policy = (found == Availability::Ready).then(|| {
            let kept = crate::allow::Store::read(store.as_deref()).get(&crate::sandbox::real(&root));
            let (llm, granted) = with_allowed(&llm, &[&kept, &chat], extra.grant.as_ref());
            let mut p = crate::sandbox::policy(&root, &llm);
            p.granted = granted;
            // Its denials watched from before it starts (the stream up first — measured)
            if let Some(tag) = &p.tag {
                p.watch = Some(crate::sandbox::Watch::start(tag));
                std::thread::sleep(crate::sandbox::WATCH_READY);
            }
            p
        });
        move |ed: &mut Editor| {
            ed.sandbox = found.clone();
            // (Claude's turn ended meanwhile: nothing waits for it)
            if let For::Claude(call) = &who
                && ed.chat.as_ref().is_some_and(|c| c.generation() != call.generation || !c.busy())
            {
                return;
            }
            match (policy, found, who) {
                (Some(p), _, who) => go(ed, who, job, Some(Guard::In(p))),
                (None, Availability::Missing(why), For::Claude(call))
                    if !asked && !ed.chat.as_ref().is_some_and(|c| c.unprotected) =>
                {
                    ask_unprotected(ed, call, job, why)
                }
                (None, Availability::Missing(why), who) => go(ed, who, job, Some(Guard::Without(why))),
                (None, _, who) => go(ed, who, job, None),
            }
        }
    });
}

fn go(ed: &mut Editor, who: For, job: Job, guard: Option<Guard>) {
    match job {
        Job::Command(cmd, cwd) => {
            if let Err(e) = run_busy(ed) {
                return fail(ed, who, e);
            }
            if let Err(e) = run_start(ed, who.clone(), cmd, cwd, guard) {
                fail(ed, who, e);
            }
        }
        Job::Tests(target) => start_tests(ed, who, *target, guard),
    }
}

pub const NO_SANDBOX_TITLE: &str = "Claude wants to run — without a sandbox";

/// No sandbox on this system: Claude's command waits for the user — `y` this once, `a` for this
/// conversation, `n` no.
fn ask_unprotected(ed: &mut Editor, call: Call, job: Job, why: String) {
    if ed.offers.iter().any(|o| matches!(o.what, crate::offer::What::NoSandbox { .. })) {
        let e = "the user hasn't answered about your last command yet — wait for it, then run this";
        return answer(ed, call, Err(e.into()));
    }
    let question = format!(
        "{}\n\nThere's none here ({why}) — it would run with all your permissions: your files, your network.",
        job.label()
    );
    let what = crate::offer::What::NoSandbox { job, call };
    ed.push_offer(crate::offer::Offer::new(NO_SANDBOX_TITLE.into(), &question, "", String::new(), what));
}

/// The answer on that card: `y` runs it unprotected, `a` too — and the rest of this conversation's — `n`
/// tells Claude no.
pub fn unprotected_answer(ed: &mut Editor, id: u64, job: Job, call: Call, answer_is: Unprotected) {
    ed.offer_finished(id, Ok(()));
    let why = match &ed.sandbox {
        Availability::Missing(why) => why.clone(),
        _ => "none here".into(),
    };
    match answer_is {
        Unprotected::No => {
            let e = format!(
                "There's no sandbox on this system ({why}), and the user chose not to run `{}` without one — \
                 ask them how to go on",
                job.label()
            );
            answer(ed, call, Err(e))
        }
        Unprotected::Once => go(ed, For::Claude(call), job, Some(Guard::Without(why))),
        Unprotected::ThisChat => {
            if let Some(c) = ed.chat.as_mut() {
                c.unprotected = true;
            }
            ed.set_warning("Claude's commands run without a sandbox in this conversation");
            go(ed, For::Claude(call), job, Some(Guard::Without(why)))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unprotected {
    Once,
    ThisChat,
    No,
}

/// Whether Claude's commands run without a sandbox now — the chat's title says so.
pub fn unsandboxed(ed: &Editor) -> bool {
    !ed.config.llm.sandbox || ed.chat.as_ref().is_some_and(|c| c.unprotected)
}

/// The command Claude ran ended — how, and its output, go back; the editor shows the first error. One the
/// user ran again (`blocked.rs`): Claude hears how it ended with the next message.
pub fn run_done(ed: &mut Editor) {
    let Some(r) = ed.task_run.as_mut() else { return };
    // (its own places — the user's list has them only when it was a check, #28)
    if let Some(head) = r.tell.take() {
        let r = &*r;
        let text = run_report(&ed.root, r, &r.places);
        return told(ed, head, text);
    }
    let Some(call) = r.chat.take() else { return };
    let items = &r.places;
    let text = run_report(&ed.root, r, items);
    let first =
        items.iter().find(|i| i.severity == 1).filter(|_| r.state != crate::task::State::Passed).cloned();
    let label = r.task.name.clone();
    answer(ed, call, Ok(text));
    if let Some(it) = first {
        crate::follow::point(ed, "failed", label, &it.path, it.line..it.line + 1, Some(it.message));
    }
}

/// A run Claude waited on is gone before it ended (stopped, or another task took its place).
pub fn run_dropped(ed: &mut Editor, why: &str) {
    if let Some(call) =
        ed.task_run.as_mut().filter(|r| r.state == crate::task::State::Running).and_then(|r| r.chat.take())
    {
        answer(ed, call, Err(format!("the command was stopped — {why}")));
    }
}

/// Places reported at most; output lines sent whole up to `OUTPUT_WHOLE`, else its first `OUTPUT_HEAD` and
/// last `OUTPUT_TAIL` lines (the errors come first, the summary last).
const PLACES_MAX: usize = 30;
const OUTPUT_WHOLE: usize = 300;
const OUTPUT_HEAD: usize = 100;
const OUTPUT_TAIL: usize = 150;
/// Characters per output line, at most (a minified bundle on one line).
const LINE_MAX: usize = 500;

/// How the command ended as Claude reads it: the verdict, the places it pointed at (from `root`, the
/// project's), its output.
fn run_report(root: &Path, r: &crate::task::Run, places: &[crate::quickfix::Item]) -> String {
    use crate::task::State;
    let took = r.took.map(|d| format!(" in {:.1}s", d.as_secs_f32())).unwrap_or_default();
    let verdict = match r.state {
        State::Passed => "exit 0".to_string(),
        State::Failed(Some(code)) => format!("exit {code}"),
        State::Failed(None) => "killed".to_string(),
        State::Running => "still running".to_string(),
    };
    let mut out = format!("{verdict}{took}\n$ {}\n", r.task.cmd);
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
/// output's tail when the run failed with no failed test to show for it (a build error). Paths from `root`.
fn report(root: &Path, r: &TestRun) -> String {
    let took = r.took.map(|d| format!(" in {:.1}s", d.as_secs_f32())).unwrap_or_default();
    let verdict = match r.state {
        RunState::Passed => "passed".to_string(),
        RunState::Failed(Some(code)) => format!("failed (exit {code})"),
        RunState::Failed(None) => "failed (killed)".to_string(),
        RunState::Running => "still running".to_string(),
        RunState::NoTests => "no tests ran".to_string(),
    };
    let mut out = format!(
        "{verdict}{took} — {}\n$ {}  (in {})\n",
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
        for name in ["references", "search", "symbols", "definition", "hover", "diagnostics", "screen"] {
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
    #[test]
    #[ignore = "runs the installed claude (no model call) — cargo test -- --ignored installed_claude"]
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
        // (not the running Claude Code session's variables — a nested claude would take them as its own)
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
        let text = super::report(elsewhere(), &r);
        assert!(text.starts_with("no tests ran — term_snapshots::shows\n"), "{text}");
        assert!(text.contains("running 0 tests\n"), "the output says why: {text}");
        assert_eq!(super::verdict(&text), "no tests ran");
    }

    /// A run's policy has what the user allowed in it — kept, this chat's, and a grant for this run alone —
    /// each once, and its result says them.
    #[test]
    fn what_the_user_allowed_goes_into_the_policy() {
        use crate::allow::{Allowed, Grant};
        let kept = Allowed { write: vec!["/w/kept".into()], ports: vec![5432], ..Default::default() };
        let chat = Allowed { write: vec!["/w/chat".into()], ports: vec![5432], ..Default::default() };
        let base = crate::llm::LlmConfig::default();
        let (llm, granted) = super::with_allowed(&base, &[&kept, &chat], Some(&Grant::Port(3000)));
        assert_eq!(llm.sandbox_write, ["/w/kept", "/w/chat"]);
        assert_eq!(llm.sandbox_ports, [5432, 5432, 3000], "(the policy keeps each port once)");
        assert_eq!(
            granted,
            [
                "write in /w/kept and everything in it",
                "connect to port 5432 on this machine",
                "write in /w/chat and everything in it",
                "connect to port 3000 on this machine"
            ]
        );
        let (none, said) = super::with_allowed(&base, &[], None);
        assert!(none.sandbox_write.is_empty() && none.sandbox_ports.is_empty() && said.is_empty());
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
        ] {
            assert!(ok(c), "{c}");
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
            "cargo test ~/x",
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

    /// The pattern a run card's `a` allows for the rest of the chat: a command's first plain words, two or
    /// three — none for a runner (anything could follow it), one word, or a line of several.
    #[test]
    fn a_cards_pattern_for_the_chat() {
        let p = |c: &str| super::pattern(c);
        assert_eq!(p("cargo xtask shot /tmp/x.png '@?ready'").as_deref(), Some("cargo xtask shot"));
        assert_eq!(p("git config --get user.name"), None, "git: reads run already, writes go to .git");
        assert_eq!(p("npm run build:prod").as_deref(), Some("npm run"), "a value isn't part of it");
        assert_eq!(p("echo one two three four").as_deref(), Some("echo one two"), "three at most");
        assert_eq!(p("sh -c 'cargo bench'").as_deref(), Some("cargo bench"), "the body's, sh -c taken off");
        for c in [
            "sh target/sb/x.sh",
            "bash x",
            "env FOO=1 cargo test",
            "python3 -m http.server",
            "xargs rm",
            "sudo make",
            "time cargo build",
            "make",
            "rm -rf target",
            "touch /tmp/made",
            "ls | wc -l",
            "cargo build; rm x",
        ] {
            assert_eq!(p(c), None, "{c}");
        }
        // Allowed for the chat: the pattern, word for word
        let chat = vec!["cargo xtask shot".to_string()];
        assert!(super::allowed("cargo xtask shot /tmp/a.png '@?files in'", &chat));
        assert!(!super::allowed("cargo xtask perf", &chat));
        assert!(!super::allowed("cargo xtask shot x; rm -rf y", &chat));
    }

    /// #44: assignments in front of the command go into the pattern by name, values left open — never a
    /// name that finds programs or libraries, or one the sandbox sets; and a command setting a name its
    /// rule doesn't name still asks.
    #[test]
    fn assignments_in_front_go_into_the_pattern_by_name() {
        let p = |c: &str| super::pattern(c);
        let shot =
            "sh -c 'TARAE_SHOT_XDG=/tmp/x TARAE_BACKGROUND=dark cargo xtask shot /tmp/a.png \"@?ready\"'";
        assert_eq!(p(shot).as_deref(), Some("TARAE_SHOT_XDG= TARAE_BACKGROUND= cargo xtask shot"));
        assert_eq!(
            super::pattern_shown(&p(shot).unwrap()),
            "TARAE_SHOT_XDG=… TARAE_BACKGROUND=… cargo xtask shot"
        );
        assert_eq!(p("FOO=1 cargo test").as_deref(), Some("FOO= cargo test"), "not only in sh -c");
        for c in [
            "PATH=/tmp/bin cargo xtask shot",
            "LD_PRELOAD=/tmp/x.so cargo xtask shot",
            "DYLD_INSERT_LIBRARIES=/tmp/x cargo xtask shot",
            "sh -c 'A=1 HOME=/tmp cargo xtask shot'",
            "TMPDIR=/x cargo xtask shot",
            "TARAE_SANDBOXED= cargo xtask shot",
            "A=1 env cargo test",
            "A=1 sh x.sh",
            "'A=1' cargo test",
            "A=$(id) cargo test",
            "A=1 B=2",
            "A=1 cd src",
        ] {
            assert_eq!(p(c), None, "{c}");
        }
        let chat = vec![p(shot).unwrap()];
        assert!(super::allowed(shot, &chat));
        assert!(super::allowed("TARAE_BACKGROUND=light cargo xtask shot /tmp/b.png", &chat), "fewer names");
        assert!(super::allowed("cargo xtask shot /tmp/b.png", &chat), "none");
        assert!(!super::allowed("RUSTC_WRAPPER=/tmp/w cargo xtask shot x", &chat), "a name it doesn't name");
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
        let shot = "sh -c 'cargo xtask shot /tmp/x.png 2>&1 | grep -E \"a|b\" | tail -3'";
        assert_eq!(super::pattern(shot).as_deref(), Some("cargo xtask shot"));
        assert_eq!(super::pattern("ls | wc -l"), None, "one word isn't a pattern");
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
        let text = super::run_report(elsewhere(), &r, &[]);
        assert!(
            text.contains("1 test skipped itself — libtest says ok, but nothing was checked:\n  sandbox::tests::real — Seatbelt"),
            "{text}"
        );
        r.output.retain(|(err, _)| !err);
        assert!(!super::run_report(elsewhere(), &r, &[]).contains("skipped"));
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
        let text = super::run_report(elsewhere(), &r, &places);
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
        let text = super::run_report(elsewhere(), &r, &[place]);
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
        let text = super::run_report(elsewhere(), &r, &[]);
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
