//! tarae's own tools for the chat — an MCP server that lives in the editor and speaks over the chat's
//! stream-json (`control_request mcp_message` → `control_response {mcp_response}`, `chat.rs`), so no server
//! or port is involved.
//!
//! - `note` pins a remark to lines (`notes.rs`) — answered at once.
//! - `run_tests` runs the tests of a file, or the one at a line, the way `space x x` does (`testing.rs`):
//!   found through the tree on a worker, run in the test pane the user sees, answered when the run ends
//!   (the call waits — a `Call` rides on the run). Claude checks its own work with it; the editor shows the
//!   test while it runs and the first failure when it's done (`follow.rs`).

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
            json!({ "tools": [crate::notes::tool_schema(), run_tests_schema(), crate::plan::tool_schema()] }),
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

/// The chat turn ended (stopped) with a run still going: it goes on for the user, unreported.
pub fn turn_over(ed: &mut Editor) {
    if let Some(r) = ed.test_run.as_mut() {
        r.chat = None;
    }
}

/// A run Claude waited on is gone before it ended (closed, or another run took its place).
pub fn tests_dropped(ed: &mut Editor, why: &str) {
    if let Some(call) = ed.test_run.as_mut().and_then(|r| r.chat.take()) {
        answer(ed, call, Err(format!("the test run was stopped — {why}")));
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
