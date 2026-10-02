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

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::editor::Editor;
use crate::test_results::Status;
use crate::testing::{RunState, Scope, TestRun};

/// A tool call answered later: the chat process it came from and the ids to answer with.
#[derive(Clone, Debug)]
pub struct Call {
    generation: u64,
    request_id: String,
    id: Value,
}

/// An MCP message from claude → the `mcp_response` (None = answered later with `answer`).
pub fn mcp(ed: &mut Editor, generation: u64, request_id: &str, msg: &Value) -> Option<Value> {
    // Notifications get the empty answer the SDK gives
    let Some(id) = msg.get("id").cloned() else {
        return Some(json!({ "jsonrpc": "2.0", "result": {}, "id": 0 }));
    };
    let result = match msg["method"].as_str().unwrap_or_default() {
        "initialize" => Ok(json!({
            "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "tarae", "version": env!("CARGO_PKG_VERSION") },
        })),
        "tools/list" => Ok(
            json!({ "tools": [crate::notes::tool_schema(), run_tests_schema(), run_schema(), crate::plan::tool_schema()] }),
        ),
        "tools/call" => {
            let params = &msg["params"];
            let args = &params["arguments"];
            let r = match params["name"].as_str() {
                Some("note") => crate::notes::tool_call(ed, args),
                Some("plan") => crate::plan::tool_call(ed, args),
                Some("run_tests") => {
                    let call = Call { generation, request_id: request_id.to_string(), id: id.clone() };
                    match run_tests(ed, call, args) {
                        Ok(()) => return None,
                        Err(e) => Err(e),
                    }
                }
                Some("run") => {
                    let call = Call { generation, request_id: request_id.to_string(), id: id.clone() };
                    match run(ed, call, args) {
                        Ok(()) => return None,
                        Err(e) => Err(e),
                    }
                }
                other => Err(format!("no tool {}", other.unwrap_or_default())),
            };
            Ok(content(r))
        }
        "ping" => Ok(json!({})),
        m => Err(json!({ "code": -32601, "message": format!("no method {m}") })),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err(e) => json!({ "jsonrpc": "2.0", "id": id, "error": e }),
    })
}

/// A tool's text as MCP content (Err = a tool error Claude can correct).
fn content(r: Result<String, String>) -> Value {
    match r {
        Ok(t) => json!({ "content": [{ "type": "text", "text": t }] }),
        Err(e) => json!({ "content": [{ "type": "text", "text": e }], "isError": true }),
    }
}

/// Answers a call that waited — only to the process that made it.
pub fn answer(ed: &mut Editor, call: Call, r: Result<String, String>) {
    if ed.chat.as_ref().is_none_or(|c| c.generation() != call.generation) {
        return;
    }
    let response = json!({ "jsonrpc": "2.0", "id": call.id, "result": content(r) });
    crate::chat::respond(ed, &call.request_id, json!({ "mcp_response": response }));
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
    let path = crate::chat::resolve(path);
    let line = args["line"].as_u64().map(|l| l.max(1) as usize - 1);
    busy(ed)?;
    // The buffer's text when it's open (unsaved edits are what gets saved and run)
    let text = ed.docs.iter().find(|d| d.path.as_deref() == Some(path.as_path())).map(|d| d.text.clone());
    ed.events.jobs().spawn(move || {
        let found = find(&path, text, line);
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

/// The test to run in `path` (at `line`, else the whole file) — on a worker.
fn find(
    path: &std::path::Path,
    text: Option<ropey::Rope>,
    line: Option<usize>,
) -> Result<crate::testing::Target, Found> {
    let shown = crate::chat::shown(path);
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
    let before = ed.test_generation;
    let program = target.argv().first().cloned().unwrap_or_default();
    ed.test_start(target);
    match ed.test_run.as_mut().filter(|_| ed.test_generation != before) {
        Some(r) => r.chat = Some(call),
        None => answer(ed, call, Err(format!("couldn't start {program}"))),
    }
}

/// The run Claude asked for ended — its results go back, and the editor shows the first failure.
pub fn tests_done(ed: &mut Editor) {
    let Some(r) = ed.test_run.as_mut() else { return };
    let Some(call) = r.chat.take() else { return };
    let text = report(r);
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
    ed.offers.retain(|o| !matches!(o.what, crate::offer::What::Run { .. }));
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
        "description": "Run a shell command in the project root, in the user's editor, and get how it ended \
    and its output — build, lint, format, test suites, git. The user watches it in their task panel. Both \
    output streams come back whole (long output: its start and end), so don't redirect, pipe to head/tail or \
    chain commands: one command per call. Checks (cargo build/check/test/clippy/fmt, go build/test/vet, git \
    status/diff/log/show) start at once; any other command waits for the user to allow it — if they say no, \
    don't retry it, ask. No stdin and no terminal: nothing interactive. For one test file or test, \
    run_tests is better (it reads each failure).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "command": { "type": "string", "description": "The command line (run by sh -c in the project root)" }
            },
            "required": ["command"]
        }
    })
}

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

/// Whether `cmd` runs without asking: plain words (no quoting, expansion, redirection, chaining or globs —
/// any of those and the words don't say what runs), starting with one of the `RUN_ALLOW` or `extra`
/// prefixes word for word, and none of the options that make a read-only command write or run something.
pub fn allowed(cmd: &str, extra: &[String]) -> bool {
    const SHELL: &[char] = &[
        ';', '&', '|', '`', '$', '(', ')', '<', '>', '\\', '\'', '"', '*', '?', '[', ']', '{', '}', '#', '!',
        '\n', '\r',
    ];
    const WRITES: [&str; 6] = ["--output", "--ext-diff", "--textconv", "--config", "--exec", "--upload-pack"];
    if cmd.contains(SHELL) {
        return false;
    }
    let words: Vec<&str> = cmd.split_whitespace().collect();
    // (`~` only expands at a word's start: `HEAD~1` is a plain word)
    let unsafe_word = |w: &&str| {
        w.starts_with('~')
            || w.contains('=') && !w.starts_with('-')
            || WRITES.iter().any(|o| w.starts_with(o))
    };
    if words.iter().any(unsafe_word) {
        return false;
    }
    RUN_ALLOW.iter().copied().chain(extra.iter().map(String::as_str)).any(|rule| {
        let rule: Vec<&str> = rule.split_whitespace().collect();
        !rule.is_empty() && words.starts_with(&rule)
    })
}

/// Runs `command` now (allowed), or once the user says yes on its card — answered when it ends.
fn run(ed: &mut Editor, call: Call, args: &Value) -> Result<(), String> {
    let cmd =
        args["command"].as_str().map(str::trim).filter(|c| !c.is_empty()).ok_or("command is required")?;
    if cmd.len() > COMMAND_MAX || cmd.lines().count() > COMMAND_LINES_MAX {
        return Err(format!(
            "too long to show the user ({} characters, {} lines; at most {COMMAND_MAX} and \
             {COMMAND_LINES_MAX}) — put it in a script with Write, then run that",
            cmd.len(),
            cmd.lines().count()
        ));
    }
    run_busy(ed)?;
    if allowed(cmd, &ed.config.llm.run_allow) {
        return run_start(ed, call, cmd.to_string());
    }
    let title = "Claude wants to run".to_string();
    if ed.offers.iter().any(|o| matches!(o.what, crate::offer::What::Run { .. })) {
        return Err("the user hasn't answered your last command yet — wait for it, then run this".into());
    }
    let what = crate::offer::What::Run { cmd: cmd.to_string(), call };
    ed.push_offer(crate::offer::Offer::new(title, cmd, "Running…", "Claude's command".into(), what));
    Ok(())
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

fn run_start(ed: &mut Editor, call: Call, cmd: String) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let task = crate::task::Task { name: cmd.clone(), cmd, cwd };
    if !ed.task_start_for(task, call) {
        return Err(run_busy(ed).err().unwrap_or_else(|| "couldn't start sh".into()));
    }
    Ok(())
}

/// The user's answer on the card: `y` runs it, `n` tells Claude no.
pub fn run_answer(ed: &mut Editor, id: u64, cmd: String, call: Call, yes: bool) {
    ed.offer_finished(id, Ok(()));
    if !yes {
        return answer(ed, call, Err(format!("the user chose not to run `{cmd}`")));
    }
    if let Err(e) = run_busy(ed) {
        return answer(ed, call, Err(e));
    }
    let c = call.clone();
    if let Err(e) = run_start(ed, call, cmd) {
        answer(ed, c, Err(e));
    }
}

/// The command Claude ran ended — how, and its output, go back; the editor shows the first error.
pub fn run_done(ed: &mut Editor) {
    let Some(r) = ed.task_run.as_mut() else { return };
    let Some(call) = r.chat.take() else { return };
    let items = ed.qlist.as_ref().filter(|l| l.title == r.task.name).map_or(&[][..], |l| &l.items[..]);
    let text = run_report(r, items);
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

/// How the command ended as Claude reads it: the verdict, the places it pointed at, its output.
fn run_report(r: &crate::task::Run, places: &[crate::quickfix::Item]) -> String {
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
    if errors + warnings > 0 && !places.is_empty() {
        out.push_str(&format!("\n{errors} errors, {warnings} warnings in the user's list (]q):\n"));
        for it in places.iter().take(PLACES_MAX) {
            let kind = match it.severity {
                1 => "error",
                2 => "warning",
                _ => "note",
            };
            out.push_str(&format!(
                "  {}:{}:{} {kind}: {}\n",
                crate::chat::shown(&it.path),
                it.line + 1,
                it.col + 1,
                it.message
            ));
        }
    }
    let lines = &r.output;
    if lines.is_empty() {
        out.push_str("\n(no output)\n");
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
/// output's tail when the run failed with no failed test to show for it (a build error).
fn report(r: &TestRun) -> String {
    let took = r.took.map(|d| format!(" in {:.1}s", d.as_secs_f32())).unwrap_or_default();
    let verdict = match r.state {
        RunState::Passed => "passed".to_string(),
        RunState::Failed(Some(code)) => format!("failed (exit {code})"),
        RunState::Failed(None) => "failed (killed)".to_string(),
        RunState::Running => "still running".to_string(),
    };
    let mut out = format!(
        "{verdict}{took} — {}\n$ {}  (in {})\n",
        r.target.label,
        r.target.argv().join(" "),
        crate::chat::shown(&r.target.cwd)
    );
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
    out.push_str(&failures(&r.cases, &fails));
    if r.state != RunState::Passed && fails.is_empty() {
        let from = r.output.len().saturating_sub(TAIL);
        out.push_str(&format!("\nOutput (last {} lines):\n", r.output.len() - from));
        for (_, line) in &r.output[from..] {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// The failed cases (indices into `cases`): the first `FAILURES_MAX` with where and why, the rest by name
/// and place — so every failure is known, but the long messages stay few.
fn failures(cases: &[crate::test_results::Case], fails: &[usize]) -> String {
    let at = |c: &crate::test_results::Case| {
        c.at.as_ref().map(|(p, l)| format!(" at {}:{}", crate::chat::shown(p), l + 1)).unwrap_or_default()
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
    } else if head.starts_with("failed") {
        count("failed").unwrap_or_else(|| "failed".into())
    } else {
        "not run".into()
    }
}

#[cfg(test)]
mod tests {
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
        for c in
            ["cargo fmt", "cargo fmt --check", "git status", "git diff HEAD~1 -- src/tools.rs", "git log -5"]
        {
            assert!(ok(c), "{c}");
        }
        for c in [
            "cargo test; rm -rf ~",
            "cargo test && git push",
            "cargo test | tail",
            "cargo build 2>&1",
            "cargo test > out.txt",
            "cargo test $(rm x)",
            "cargo test `rm x`",
            "git log --format='%h'",
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
        assert!(!super::allowed("npm test FOO=1", &extra), "an assignment isn't");
        assert!(!super::allowed("npm install", &extra), "an empty rule allows nothing");
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
        let text = super::run_report(&r, &[place]);
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
        let text = super::run_report(&r, &[]);
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
        let few = super::failures(&cases, &(0..12).collect::<Vec<_>>());
        assert!(few.contains("FAILED t9\n  why 9\n"), "{few}");
        assert!(few.contains("\n2 more failed (names only):\n  t10\n  t11\n"), "{few}");
        assert!(!few.contains("why 10"), "{few}");
        let many = super::failures(&cases, &(0..70).collect::<Vec<_>>());
        assert!(many.contains("60 more failed") && many.contains("  t59\n"), "{many}");
        assert!(!many.contains("  t60\n") && many.ends_with("  … and 10 more\n"), "{many}");
        assert!(super::failures(&cases, &[]).is_empty());
    }
}
