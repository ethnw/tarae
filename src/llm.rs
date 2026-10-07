//! LLM — runs `claude -p` (or any CLI with the same stream-json protocol) as a subprocess.
//! No HTTP·SDK·API key inside the editor — uses the user's Claude Code login as is.
//!
//! Cold start is slow (measured 2026-09-27, Claude Code 2.1.280, a single "ok"):
//! default 8–10 s → minimal options (`--tools "" --strict-mcp-config --setting-sources ""`) 4.3 s
//! → **a process spawned early and waiting on stdin (stream-json): 2.5 s** (haiku 1.9 s).
//! So the spare process is spawned the moment the ask prompt opens (overlapping typing); a request only
//! writes the message. One process per request — so contexts don't mix.
//!
//! Flow: selections → one request (answer per selection in `<r i="N">` tags) → review (diff, y/n per change)
//! → applied as one transaction. The answer is received as it streams (`--include-partial-messages`) and
//! shown in a preview card. Cancel = kill the process.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use ropey::Rope;

use crate::document::DocId;
use crate::editor::Editor;
use crate::event::{Apply, Event};
use crate::key::{Code, Key};
use crate::selection::{Range, Selection};
use crate::transaction::{Assoc, Change, Transaction};

/// Speed options: no tools (pure transform), skip MCP·user settings, no conversation kept by claude (keeps /resume clean).
const DEFAULT_ARGS: &[&str] = &[
    "-p",
    "--input-format",
    "stream-json",
    "--output-format",
    "stream-json",
    "--verbose",
    "--include-partial-messages",
    "--tools",
    "",
    "--no-session-persistence",
    "--strict-mcp-config",
    "--setting-sources",
    "",
];

/// A model claude offers (`initialize` → `models`, measured): its alias or id, what it's called, a line
/// about it, the model it stands for now, and the effort levels it takes (none: it has no such knob).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelInfo {
    pub value: String,
    pub name: String,
    pub about: String,
    pub resolved: String,
    pub efforts: Vec<String>,
}

impl ModelInfo {
    pub fn list(v: &serde_json::Value) -> Vec<ModelInfo> {
        let s = |m: &serde_json::Value, k: &str| m[k].as_str().unwrap_or_default().to_string();
        v.as_array()
            .into_iter()
            .flatten()
            .filter(|m| m["value"].is_string())
            .map(|m| ModelInfo {
                value: s(m, "value"),
                name: s(m, "displayName"),
                about: s(m, "description"),
                resolved: s(m, "resolvedModel"),
                efforts: m["supportedEffortLevels"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.as_str().map(str::to_string))
                    .collect(),
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct LlmConfig {
    pub command: String,
    pub args: Vec<String>,
    pub model: Option<String>,
    /// `--effort` ("" = the model's default).
    pub effort: String,
    /// Lines of context sent before and after the selection.
    pub context_lines: usize,
    /// Chat: the editor follows where Claude is reading (`C-f` in the chat toggles it for that chat).
    pub follow: bool,
    /// Chat: seconds after your last key that follow takes over again while Claude works (0 = only `C-f`).
    pub follow_resume: u64,
    /// Chat: Claude's edits go in as it makes them, the turn reviewed when it ends — every new chat starts
    /// so (`space v a` turns it on and off, and keeps it here).
    pub review_at_end: bool,
    /// Chat: load CLAUDE.md files and Claude Code settings like `claude` does (hooks off, edits always ask).
    pub chat_settings: bool,
    /// Chat: commands Claude's `run` starts without asking, past the built-in checks (`tools::run_allow`) —
    /// each a prefix of whole words (`"npm test"`).
    pub run_allow: Vec<String>,
    /// Chat, review at the end (`edits.rs`): more files whose edits still ask first, past the protected ones
    /// (`sandbox::protected_names`, which always ask) — a name matches anywhere, a path (with `/`) from the
    /// project's root.
    pub review_ask: Vec<String>,
    /// Chat: Claude's commands run in a sandbox (`sandbox.rs`) — writes in the project, temp and the
    /// toolchain's folders; no network but this machine.
    pub sandbox: bool,
    pub sandbox_network: bool,
    /// With the network on, the hosts reached without asking (`llm.sandbox-hosts`) — None: any host (`"any"`);
    /// another is held and asked on a card (`hosts.rs`).
    pub sandbox_hosts: Option<Vec<crate::sandbox::proxy::egress::Pattern>>,
    /// With the network on, whether tarae's proxy goes on through the proxy tarae's own environment names
    /// (`HTTPS_PROXY`…, `NO_PROXY` — `proxy::upstream`), or straight out.
    pub sandbox_upstream: crate::sandbox::proxy::upstream::Setting,
    /// More folders the sandbox may write to (`~/` for home).
    pub sandbox_write: Vec<String>,
    /// Paths in the project the sandbox keeps as they are, besides the names it always keeps
    /// (`sandbox::protected_names`).
    pub sandbox_protect: Vec<String>,
    /// Ports on this machine the sandbox lets Claude's commands connect to (none else, the network open or not).
    pub sandbox_ports: Vec<u16>,
    /// Every port on this machine open to them — the risky ones too (Seatbelt can't leave some out), and on
    /// macOS other runs' ports to tarae's proxy.
    pub sandbox_localhost: bool,
    /// More of tarae's environment their commands get (`sandbox::runenv`): names, or `PREFIX*` families — a
    /// token-like one only by its whole name.
    pub sandbox_env: Vec<String>,
    /// Days a kept run home (a toolchain's home of the runs', kept across them) stays with no run using it before
    /// tarae's start removes it (`runhomes.rs`).
    pub sandbox_kept_days: u64,
    /// Chat: Claude's `screen` tool shows the terminal panel's contents too (`seeing.rs`) — off, its rows go
    /// blank.
    pub screen_terminal: bool,
    /// Chat: minutes a run of Claude's prints nothing at next to no CPU before a card asks whether it's
    /// stuck (`stuck.rs`; 0 = never).
    pub stuck_after: u64,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            command: "claude".into(),
            args: DEFAULT_ARGS.iter().map(|s| s.to_string()).collect(),
            model: None,
            effort: String::new(),
            context_lines: 20,
            follow: true,
            follow_resume: 20,
            review_at_end: false,
            chat_settings: true,
            run_allow: Vec::new(),
            review_ask: Vec::new(),
            sandbox: true,
            sandbox_network: false,
            sandbox_hosts: None,
            sandbox_upstream: Default::default(),
            sandbox_write: Vec::new(),
            sandbox_protect: Vec::new(),
            sandbox_ports: Vec::new(),
            sandbox_localhost: false,
            sandbox_env: Vec::new(),
            sandbox_kept_days: 14,
            screen_terminal: true,
            stuck_after: 10,
        }
    }
}

// ── Spare process ───────────────────────────────────────────────────────

/// A pre-spawned process for the next request (at most one), and the folder it runs in.
#[derive(Default)]
pub struct Warm {
    child: Option<(Child, PathBuf)>,
}

impl Warm {
    /// Spawns one in `dir` if there's no live spare there (one elsewhere goes). spawn itself returns at once
    /// (slow init happens in the child).
    pub fn ensure(&mut self, cfg: &LlmConfig, dir: &Path) -> Result<(), String> {
        if let Some((c, at)) = &mut self.child
            && matches!(c.try_wait(), Ok(None))
        {
            if at == dir {
                return Ok(());
            }
            // Spared for another folder: it goes (reaped off the main thread)
            let (mut old, _) = self.child.take().expect("just seen");
            crate::event::thread("tarae-reap", move || {
                let _ = old.kill();
                let _ = old.wait();
            });
        }
        let child = spawn(cfg, &[], dir).map_err(|e| spawn_failed(&cfg.command, &e, false))?;
        self.child = Some((child, dir.to_path_buf()));
        Ok(())
    }

    /// The spare's process (`:procs`).
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(|(c, _)| c.id())
    }

    fn take(&mut self, cfg: &LlmConfig, dir: &Path) -> Result<Child, String> {
        self.ensure(cfg, dir)?;
        Ok(self.child.take().expect("ensured").0)
    }
}

impl Drop for Warm {
    fn drop(&mut self) {
        // (reaped off the main thread — else a zombie until tarae exits)
        if let Some((mut c, _)) = self.child.take() {
            crate::event::thread("tarae-reap", move || {
                let _ = c.kill();
                let _ = c.wait();
            });
        }
    }
}

/// What a failed start of `command` (`llm.command`) says. No `claude` at all: that Claude Code isn't installed,
/// and how to get it and log in — in its own docs' words (code.claude.com/docs/en/setup); `whole`: every line
/// (the chat), else one (a toast).
pub fn spawn_failed(command: &str, e: &std::io::Error, whole: bool) -> String {
    if e.kind() != std::io::ErrorKind::NotFound {
        return format!("{command}: {e}");
    }
    if Path::new(command).file_name().is_none_or(|n| n != "claude") {
        return match command.contains('/') {
            true => format!("{command} isn't there (llm.command)"),
            false => format!("{command} isn't on PATH (llm.command)"),
        };
    }
    if !whole {
        return "Claude Code isn't installed, or `claude` isn't on your PATH — install it with \
                `curl -fsSL https://claude.ai/install.sh | bash`, then log in by running `claude`"
            .into();
    }
    "Claude Code isn't installed, or `claude` isn't on your PATH. Install it (macOS, Linux, WSL):\n\
     curl -fsSL https://claude.ai/install.sh | bash\n\
     After installing, log in by running `claude` and following the browser prompts.\n\
     If `claude` still isn't found, ~/.local/bin isn't on your PATH yet — add it, and start tarae from a \
     new terminal.\n\
     https://code.claude.com/docs/en/setup"
        .into()
}

/// Claude, started in `dir` (the project root — its relative paths, its tools and its kept conversations are there).
pub fn spawn(cfg: &LlmConfig, extra: &[&str], dir: &Path) -> std::io::Result<Child> {
    // (tests never start the real claude — a stand-in in `sh` answers for it; card model P2 found a test
    // whose claude read ~/.claude.json and the keychain)
    #[cfg(test)]
    assert!(
        Path::new(&cfg.command).file_name().is_none_or(|n| n != "claude"),
        "a test started the real claude — give llm.command a stand-in (`sh -c 'cat > /dev/null'`)"
    );
    let mut cmd = Command::new(&cfg.command);
    cmd.current_dir(dir).args(&cfg.args).args(extra);
    if let Some(m) = &cfg.model {
        cmd.args(["--model", m]);
    }
    if !cfg.effort.is_empty() {
        cmd.args(["--effort", &cfg.effort]);
    }
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()
}

/// One user message line (stream-json input).
pub fn user_message(content: &str) -> String {
    serde_json::json!({"type": "user", "message": {"role": "user", "content": content}}).to_string()
}

/// The first line to a chat process: registers tarae's own MCP server (`sdkMcpServers` — its messages come
/// back as `mcp_message` control requests).
pub fn initialize_message(servers: &[&str]) -> String {
    serde_json::json!({"type": "control_request", "request_id": "tarae-init",
        "request": {"subtype": "initialize", "sdkMcpServers": servers}})
    .to_string()
}

/// Our answer to a control request from claude.
pub fn control_response(request_id: &str, response: serde_json::Value) -> String {
    serde_json::json!({"type": "control_response",
        "response": {"subtype": "success", "request_id": request_id, "response": response}})
    .to_string()
}

/// Control request that stops the running turn — the process and conversation memory remain.
pub fn interrupt_message() -> String {
    serde_json::json!({"type": "control_request", "request_id": "tarae-stop", "request": {"subtype": "interrupt"}})
        .to_string()
}

/// Force-kills the process (cancel) — the reader thread ends on EOF.
pub fn kill(pid: u32) {
    // SAFETY: kill only sends a signal to the pid (merely fails if the pid already exited).
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGKILL);
    }
}

// ── Writing to it ───────────────────────────────────────────────────────

/// A claude's stdin, written on a thread of its own, lines in the order sent. A claude still starting (a
/// cold start is 8–10 s) reads nothing yet, and a pipe takes 64 KB before a write waits — one message can
/// carry the file, other unsaved buffers and a screen: measured, a 200 KB message held the main loop as long
/// as the reader didn't read (2.0 s for a stand-in sleeping 2 s). Dropped: what's queued still goes, then
/// stdin closes.
pub struct Lines(std::sync::mpsc::Sender<String>);

impl Lines {
    pub fn new(mut stdin: std::process::ChildStdin) -> Lines {
        let (tx, rx) = channel::<String>();
        crate::event::thread("tarae-llm-write", move || {
            while let Ok(line) = rx.recv() {
                // (a failed write: the process is gone — `pump` tells its end)
                if writeln!(stdin, "{line}").and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        Lines(tx)
    }

    /// Err: an earlier write failed — the process doesn't read anymore.
    pub fn send(&self, line: String) -> std::io::Result<()> {
        self.0.send(line).map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))
    }
}

// ── Reading the streamed answer ─────────────────────────────────────────

/// What one stdout line means.
pub enum StreamEv {
    /// Thinking (thinking fragments·token estimate).
    Thinking,
    /// Summarized thinking text (chat with `--thinking-display summarized` — batched like `Text`).
    Thought(String),
    /// Answer text fragment (batched every 25 ms).
    Text(String),
    /// A tool call (chat panel — Read·Grep·Glob with its full input).
    Tool { id: String, name: String, input: serde_json::Value },
    /// What a tool returned (its first 4 KB — enough to see where a search landed).
    ToolResult { id: String, text: String, error: bool },
    /// claude asks the host something (`mcp_message` for tarae's own tools) — needs a `control_response`.
    Control { request_id: String, request: serde_json::Value },
    /// The process's conversation (claude's `session_id`), the model it runs and Claude Code's version
    /// (`system init`, None: not sent) — the chat resumes it later (`--resume`).
    Conversation { id: String, model: String, version: Option<String> },
    /// The model that wrote an answer (an assistant message's `message.model`, or a stream's
    /// `message_start`) — sent when it changes.
    Answered(String),
    /// The models this claude offers (its answer to our `initialize`) — the chat's model picker.
    Models(Vec<ModelInfo>),
    /// A request goes out (`system` `status: "requesting"`), when it was read (unix ms) — `usage.rs`.
    Requesting(u64),
    /// A request's usage, as a line reported it (`message_start`, `message_delta`, an assistant message).
    Usage(crate::usage::Seen),
    /// The turn's result as far as usage goes — sent before its `Done`.
    Spent(crate::usage::Spent),
    /// claude compacting the conversation: begun, over, the compaction itself (`compaction.rs`).
    Compaction(crate::compaction::Ev),
    /// End of a turn — final text or error (not logged in·stopped etc.).
    Done(Result<String, String>),
    /// The process exited (for the chat panel — last stderr line).
    Exit(String),
}

/// Reader thread body: parses stdout line by line and sends closures built by `route` to the main loop.
/// With `once`, reaps the process after the first turn (ask). Otherwise runs until the process exits (chat).
/// stderr is drained separately (so a full pipe doesn't stall the child) — its last line is the error text.
pub fn pump(
    mut child: Child,
    once: bool,
    tx: crate::event::Tx,
    route: impl Fn(StreamEv) -> Option<Apply> + Send + 'static,
) {
    crate::event::thread("tarae-llm-read", move || {
        // stderr's last line arrives when the pipe closes
        let (err_tx, err_rx) = channel::<String>();
        if let Some(err) = child.stderr.take() {
            crate::event::thread("tarae-llm-err", move || {
                let mut last = String::new();
                for line in BufReader::new(err).lines().map_while(Result::ok) {
                    if !line.trim().is_empty() {
                        last = line;
                    }
                }
                let _ = err_tx.send(last);
            });
        }
        let send = |ev: StreamEv| route(ev).is_none_or(|apply| tx.send(Event::Job(apply)).is_ok());
        // Streamed text not yet sent — answer text or (`pending_thought`) thinking summary
        let mut pending = String::new();
        let mut pending_thought = false;
        let flush = |pending: &mut String, thought: bool| {
            let t = std::mem::take(pending);
            t.is_empty() || send(if thought { StreamEv::Thought(t) } else { StreamEv::Text(t) })
        };
        let mut last_flush = Instant::now();
        let mut thinking_sent = false;
        let mut done = false;
        let mut model = String::new();
        if let Some(out) = child.stdout.take() {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                let Ok(ev) = serde_json::from_str::<serde_json::Value>(&line) else { continue };
                // (what it costs — the same line may carry a tool call or the turn's end too)
                if let Some(u) = usage_line(&ev, crate::usage::now_ms())
                    && !send(u)
                {
                    break;
                }
                if let Some(m) = answered_by(&ev).filter(|m| *m != model) {
                    model = m.to_string();
                    if !send(StreamEv::Answered(model.clone())) {
                        break;
                    }
                }
                // Tool calls arrive whole in the assistant message — text streamed before them goes first
                let tools = tool_events(&ev);
                if !tools.is_empty() {
                    if !flush(&mut pending, pending_thought) {
                        break;
                    }
                    thinking_sent = false;
                    if !tools.into_iter().all(&send) {
                        break;
                    }
                    continue;
                }
                match parse_line(&ev) {
                    Some(ev @ (StreamEv::Text(_) | StreamEv::Thought(_))) => {
                        let (t, thought) = match ev {
                            StreamEv::Thought(t) => (t, true),
                            StreamEv::Text(t) => (t, false),
                            _ => unreachable!(),
                        };
                        if thought != pending_thought {
                            if !flush(&mut pending, pending_thought) {
                                break;
                            }
                            pending_thought = thought;
                        }
                        pending.push_str(&t);
                        thinking_sent = thought;
                        if last_flush.elapsed() >= Duration::from_millis(25) {
                            last_flush = Instant::now();
                            if !flush(&mut pending, pending_thought) {
                                break;
                            }
                        }
                    }
                    // Answered on the main loop · said where it happened (text streamed before it goes first)
                    Some(c @ (StreamEv::Control { .. } | StreamEv::Compaction(_))) => {
                        if !flush(&mut pending, pending_thought) || !send(c) {
                            break;
                        }
                    }
                    Some(e @ (StreamEv::Conversation { .. } | StreamEv::Models(_))) => {
                        if !send(e) {
                            break;
                        }
                    }
                    Some(StreamEv::Thinking) if !thinking_sent => {
                        thinking_sent = true;
                        send(StreamEv::Thinking);
                    }
                    Some(d @ StreamEv::Done(_)) => {
                        flush(&mut pending, pending_thought);
                        thinking_sent = false;
                        let ok = send(d);
                        if once || !ok {
                            done = true;
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
        if once {
            let _ = child.kill();
        }
        let _ = child.wait();
        // Let stderr drain after exit (else the real error is lost) — bounded, in case a grandchild holds it
        let err = err_rx.recv_timeout(Duration::from_millis(500)).unwrap_or_default();
        if once && !done {
            let msg = if err.is_empty() { "exited without a result".to_string() } else { err };
            send(StreamEv::Done(Err(msg)));
        } else if !once {
            send(StreamEv::Exit(err));
        }
    });
}

/// `tool_use` blocks of a complete assistant message · `tool_result` blocks of the user message that
/// answers them.
fn tool_events(ev: &serde_json::Value) -> Vec<StreamEv> {
    let Some(content) = ev["message"]["content"].as_array() else { return Vec::new() };
    let id = |b: &serde_json::Value, k: &str| b[k].as_str().unwrap_or_default().to_string();
    match ev["type"].as_str() {
        Some("assistant") => content
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .map(|b| StreamEv::Tool { id: id(b, "id"), name: id(b, "name"), input: b["input"].clone() })
            .collect(),
        Some("user") => content
            .iter()
            .filter(|b| b["type"] == "tool_result")
            .map(|b| {
                // A string, or text blocks
                let mut text = match &b["content"] {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Array(parts) => {
                        parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join("\n")
                    }
                    _ => String::new(),
                };
                if text.len() > 4096 {
                    let cut = (0..=4096).rev().find(|&i| text.is_char_boundary(i)).unwrap_or(0);
                    text.truncate(cut);
                }
                let error = b["is_error"].as_bool() == Some(true);
                StreamEv::ToolResult { id: id(b, "tool_use_id"), text, error }
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The model that wrote this answer, if the line says — not claude's own stand-ins (`<synthetic>`, for an
/// answer it made up after an error).
fn answered_by(ev: &serde_json::Value) -> Option<&str> {
    let m = match ev["type"].as_str()? {
        "assistant" => &ev["message"]["model"],
        "stream_event" if ev["event"]["type"] == "message_start" => &ev["event"]["message"]["model"],
        _ => return None,
    };
    m.as_str().filter(|m| !m.is_empty() && !m.starts_with('<'))
}

/// What a line says about usage (`usage.rs` has what's where), read at `at_ms` — None: nothing.
pub fn usage_line(ev: &serde_json::Value, at_ms: u64) -> Option<StreamEv> {
    use crate::usage::{Counts, Seen, Spent, Ttl};
    let seen = |id: Option<&serde_json::Value>, model: &serde_json::Value, u: &serde_json::Value| {
        Some(StreamEv::Usage(Seen {
            id: match id {
                Some(id) => Some(id.as_str()?.to_string()),
                None => None,
            },
            model: model.as_str().unwrap_or_default().to_string(),
            counts: Some(Counts::of(u)).filter(Counts::any)?,
            ttl: Ttl::of(u),
            at_ms,
        }))
    };
    match ev["type"].as_str()? {
        "system" if ev["subtype"] == "status" && ev["status"] == "requesting" => {
            Some(StreamEv::Requesting(at_ms))
        }
        "stream_event" => {
            let e = &ev["event"];
            match e["type"].as_str()? {
                "message_start" => {
                    seen(Some(&e["message"]["id"]), &e["message"]["model"], &e["message"]["usage"])
                }
                "message_delta" => seen(None, &serde_json::Value::Null, &e["usage"]),
                _ => None,
            }
        }
        // (claude's own stand-in after an error, `<synthetic>`, cost nothing)
        "assistant" if !ev["message"]["model"].as_str().unwrap_or_default().starts_with('<') => {
            let m = &ev["message"];
            seen(Some(&m["id"]), &m["model"], &m["usage"])
        }
        "result" => {
            let models = ev["modelUsage"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(name, m)| (name.clone(), m["contextWindow"].as_u64()))
                .collect();
            Some(StreamEv::Spent(Spent {
                conversation: ev["session_id"].as_str().unwrap_or_default().to_string(),
                counts: Counts::of(&ev["usage"]),
                total_cost: ev["total_cost_usd"].as_f64(),
                models,
                ms: ev["duration_ms"].as_u64(),
                at_ms,
            }))
        }
        _ => None,
    }
}

/// `args` with `flag`'s value replaced (the flag appended if it wasn't there).
pub fn with_flag(args: &[String], flag: &str, value: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len() + 2);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            it.next();
        } else {
            out.push(a.clone());
        }
    }
    out.extend([flag.to_string(), value.to_string()]);
    out
}

/// `args` with its `--tools <list>` swapped for `tools` (read-only ones for the chat panel), and `allowed`
/// (those + tarae's own MCP tools) allowed up front, so `-p` never stops to ask.
pub fn with_tools(args: &[String], tools: &str, allowed: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(args.len() + 4);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--tools" {
            it.next();
        } else {
            out.push(a.clone());
        }
    }
    out.extend(["--tools", tools, "--allowedTools", allowed].map(String::from));
    out
}

fn parse_line(ev: &serde_json::Value) -> Option<StreamEv> {
    match ev["type"].as_str()? {
        "control_request" => Some(StreamEv::Control {
            request_id: ev["request_id"].as_str()?.to_string(),
            request: ev["request"].clone(),
        }),
        "stream_event" => {
            let delta = &ev["event"]["delta"];
            match delta["type"].as_str()? {
                "text_delta" => Some(StreamEv::Text(delta["text"].as_str()?.to_string())),
                "thinking_delta" => Some(match delta["thinking"].as_str().filter(|t| !t.is_empty()) {
                    Some(t) => StreamEv::Thought(t.to_string()),
                    None => StreamEv::Thinking,
                }),
                _ => None,
            }
        }
        "system" if ev["subtype"] == "thinking_tokens" => Some(StreamEv::Thinking),
        // (`status: "requesting"` is usage's — `usage_line`)
        "system" if ev["subtype"] == "compact_boundary" || ev["subtype"] == "status" => {
            crate::compaction::Ev::of(ev, crate::usage::now_ms()).map(StreamEv::Compaction)
        }
        "system" if ev["subtype"] == "init" => Some(StreamEv::Conversation {
            id: ev["session_id"].as_str()?.to_string(),
            model: ev["model"].as_str().unwrap_or_default().to_string(),
            version: ev["claude_code_version"].as_str().filter(|v| !v.is_empty()).map(String::from),
        }),
        "control_response" if ev["response"]["request_id"] == "tarae-init" => {
            Some(StreamEv::Models(ModelInfo::list(&ev["response"]["response"]["models"])))
        }
        "result" => {
            let text = ev["result"].as_str().unwrap_or_default().to_string();
            let failed = ev["is_error"].as_bool() == Some(true)
                || ev["subtype"].as_str().is_some_and(|s| s.starts_with("error"));
            Some(StreamEv::Done(if failed {
                Err(if text.is_empty() { "stopped".into() } else { text })
            } else {
                Ok(text)
            }))
        }
        _ => None,
    }
}

// ── Prompt·response ─────────────────────────────────────────────────────

pub fn build_prompt(
    instruction: &str,
    file: &str,
    text: &Rope,
    ranges: &[(usize, usize)],
    ctx: usize,
) -> String {
    let mut p = String::from(
        "You are the editing engine inside a text editor. Rewrite each <selection> according to the \
         instruction.\nReply with ONLY the replacement text of every selection, each wrapped in \
         <r i=\"N\">...</r> in the same order — no explanations, no markdown fences, nothing else. \
         Keep the surrounding code style and indentation. If a selection needs no change, return it \
         unchanged.\n\n",
    );
    p.push_str(&format!("Instruction: {instruction}\nFile: {file}\n"));
    let last_line = text.len_lines().saturating_sub(1);
    for (i, &(from, to)) in ranges.iter().enumerate() {
        // Last line = that of the last selected byte (a line-wise selection ends at the next line's start)
        let (l0, l1) = (text.byte_to_line(from), text.byte_to_line(to.saturating_sub(1).max(from)));
        let c0 = text.line_to_byte(l0.saturating_sub(ctx));
        let c1 = text.line_to_byte((l1 + ctx + 1).min(last_line + 1));
        p.push_str(&format!(
            "\n<selection i=\"{}\" lines=\"{}-{}\">\n<before>{}</before>\n<text>{}</text>\n<after>{}</after>\n</selection>\n",
            i + 1,
            l0 + 1,
            l1 + 1,
            text.byte_slice(c0..from),
            text.byte_slice(from..to),
            text.byte_slice(to..c1.max(to)),
        ));
    }
    p
}

/// Extracts N `<r i="N">…</r>`. With a single selection, an untagged answer is accepted too.
pub fn parse_replies(out: &str, n: usize) -> Result<Vec<String>, String> {
    let mut replies = Vec::with_capacity(n);
    for i in 1..=n {
        let open = format!("<r i=\"{i}\">");
        let found = out.find(&open).and_then(|s| {
            let body = &out[s + open.len()..];
            body.find("</r>").map(|e| &body[..e])
        });
        match found {
            Some(body) => {
                let body = body.strip_prefix('\n').unwrap_or(body);
                replies.push(body.strip_suffix('\n').unwrap_or(body).to_string());
            }
            None if n == 1 => replies.push(strip_fences(out.trim_matches('\n')).to_string()),
            None => return Err(format!("reply {i}/{n} missing")),
        }
    }
    Ok(replies)
}

fn strip_fences(s: &str) -> &str {
    let Some(rest) = s.strip_prefix("```") else { return s };
    let rest = rest.split_once('\n').map_or("", |(_, r)| r);
    rest.strip_suffix("```").map(|r| r.strip_suffix('\n').unwrap_or(r)).unwrap_or(rest)
}

/// If the original ended with a newline (line-wise selection), end the answer with one; otherwise strip it.
pub fn match_trailing_newline(old: &str, new: String) -> String {
    match (old.ends_with('\n'), new.ends_with('\n')) {
        (true, false) => new + "\n",
        (false, true) => new.trim_end_matches('\n').to_string(),
        _ => new,
    }
}

/// Line-level diff → (' ' | '-' | '+', line).
pub fn line_diff<'a>(old: &'a str, new: &'a str) -> Vec<(char, &'a str)> {
    use imara_diff::intern::InternedInput;
    use imara_diff::{Algorithm, diff};
    // imara's `&str` tokens = `str::lines` (terminator excluded) — indexes line up
    let (a, b): (Vec<&str>, Vec<&str>) = (old.lines().collect(), new.lines().collect());
    let input = InternedInput::new(old, new);
    let mut out = Vec::with_capacity(a.len().max(b.len()));
    let mut next = 0; // first old line not yet emitted
    diff(Algorithm::Histogram, &input, |before: std::ops::Range<u32>, after: std::ops::Range<u32>| {
        let (bs, be) = (before.start as usize, before.end as usize);
        out.extend(a[next..bs].iter().map(|l| (' ', *l)));
        out.extend(a[bs..be].iter().map(|l| ('-', *l)));
        out.extend(b[after.start as usize..after.end as usize].iter().map(|l| ('+', *l)));
        next = be;
    });
    out.extend(a[next..].iter().map(|l| (' ', *l)));
    out
}

// ── Review state ────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Undecided,
    Accept,
    Reject,
}

pub struct Hunk {
    pub from: usize,
    pub to: usize,
    pub old: String,
    pub new: String,
    pub decision: Decision,
    /// The original changed while waiting and can't be located — cannot apply.
    pub conflict: bool,
}

/// One row of the in-buffer review view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RRow {
    /// This document line as is.
    Doc(usize),
    /// Change header (change number).
    Header(usize),
    /// Line inside a change: ' ' kept · '-' removed · '+' added, old line number (none for added lines).
    Diff { kind: char, text: String, old_no: Option<usize>, hunk: usize },
}

pub struct Review {
    pub generation: u64,
    pub doc_id: DocId,
    pub instruction: String,
    pub hunks: Vec<Hunk>,
    pub ready: bool,
    pub current: usize,
    pub scroll: usize,
    /// Whether the view follows the current change (off when scrolling directly with j/k).
    pub follow: bool,
    pub started: Instant,
    /// When what it reviews was asked for (a chat edit waits in its queue first) — its place in the inbox.
    pub asked: Instant,
    /// Streamed answer (preview before it's done) · whether thinking.
    pub streamed: String,
    pub thinking: bool,
    /// What to call when done (applied?) — a chat edit or a change set waiting for its answer.
    pub on_done: Option<OnDone>,
    /// Why, in Claude's words: what it thought or said just before it asked for this edit (the chat's — a
    /// row or two under the title, the rest is in the chat).
    pub reason: String,
    /// Accepted, it's a step of its own for `space u` (`space i`'s) — a chat edit is part of its turn instead
    /// (`checkpoint.rs`).
    pub own_turn: bool,
    /// The answer's lines wrapped at this width when it lands (a commit message's body at 72 — `wrap_message`).
    pub wrap: Option<usize>,
    /// It took the keys from the chat (a chat edit asked while you were in it): Esc gives them back there —
    /// else to the code (#118).
    pub from_chat: bool,
    /// Esc gave the keys back to the code, the change unanswered: they're the editor's until `C-q`.
    pub away: bool,
}

/// Called when the review ends (applied?).
pub type OnDone = Box<dyn FnOnce(&mut Editor, bool)>;

/// Closes the open review as rejected (before another review takes its place — so the waiter gets an answer).
/// An ask still waiting for its answer is killed (its result would no longer find its review).
pub fn close_review(ed: &mut Editor) {
    let Some(mut rev) = ed.session.review.take() else { return };
    if !rev.ready
        && let Some(pid) = ed.llm_pid.take()
    {
        kill(pid);
    }
    if let Some(cb) = rev.on_done.take() {
        cb(ed, false);
    }
}

impl Review {
    /// Waiting for an answer (`ready: false`), proposed by Claude, nobody to call when done.
    fn new(generation: u64, doc_id: DocId, instruction: &str, hunks: Vec<Hunk>) -> Self {
        Review {
            generation,
            doc_id,
            instruction: instruction.to_string(),
            hunks,
            ready: false,
            current: 0,
            scroll: 0,
            follow: true,
            started: Instant::now(),
            asked: Instant::now(),
            streamed: String::new(),
            thinking: false,
            on_done: None,
            reason: String::new(),
            own_turn: true,
            wrap: None,
            from_chat: false,
            away: false,
        }
    }

    fn next_undecided(&self) -> Option<usize> {
        let n = self.hunks.len();
        (1..=n)
            .map(|d| (self.current + d) % n)
            .find(|&i| self.hunks[i].decision == Decision::Undecided && !self.hunks[i].conflict)
    }

    /// Rows of the in-buffer review (like Cursor): shows the whole document, expanding only the changes.
    /// Undecided = both removed and added lines, accepted = new text only, rejected = original only.
    pub fn rows(&self, text: &Rope) -> Vec<RRow> {
        let last = text.len_lines().saturating_sub(1);
        let line_of = |b: usize| text.byte_to_line(b.min(text.len_bytes()));
        let mut order: Vec<usize> = (0..self.hunks.len()).collect();
        crate::sorting::by_key(&mut order, |&i| self.hunks[i].from);
        let mut out = Vec::new();
        let mut next = 0; // first document line not yet emitted
        for i in order {
            let h = &self.hunks[i];
            let lf = line_of(h.from);
            let lt = if h.to > h.from { line_of(h.to - 1) } else { lf };
            out.extend((next..lf.max(next)).map(RRow::Doc));
            out.push(RRow::Header(i));
            next = next.max(lt + 1);
            if h.conflict {
                out.extend((lf..=lt).map(RRow::Doc));
                continue;
            }
            // Changes line-wise: attach same-line pieces around the selection so whole lines are compared
            let ls = text.line_to_byte(lf);
            let le = text.line_to_byte((lt + 1).min(last + 1)).max(h.to);
            let (pre, post) =
                (text.byte_slice(ls..h.from.max(ls)).to_string(), text.byte_slice(h.to..le).to_string());
            let old = format!("{pre}{}{post}", h.old);
            let new = format!("{pre}{}{post}", h.new);
            let mut old_no = lf + 1;
            for (kind, line) in line_diff(&old, &new) {
                let kind = match (h.decision, kind) {
                    (Decision::Accept, '-') | (Decision::Reject, '+') => {
                        if kind == '-' {
                            old_no += 1;
                        }
                        continue;
                    }
                    (Decision::Reject, '-') => ' ',
                    _ => kind,
                };
                let no = (kind != '+').then_some(old_no);
                if kind != '+' {
                    old_no += 1;
                }
                out.push(RRow::Diff { kind, text: line.to_string(), old_no: no, hunk: i });
            }
        }
        out.extend((next..=last).map(RRow::Doc));
        out
    }
}

// ── Commands ────────────────────────────────────────────────────────────

/// When the ask prompt opens or "ask" starts being typed — spawns the spare process (overlapping typing).
pub fn prewarm(ed: &mut Editor) {
    let cfg = ed.config.llm.clone();
    if let Err(e) = ed.llm_warm.ensure(&cfg, &ed.session.root) {
        ed.set_error(e);
    }
}

pub fn ask(ed: &mut Editor, instruction: &str) -> Result<(), String> {
    let instruction = instruction.trim();
    if instruction.is_empty() {
        return Err("usage: :ask <instruction>".into());
    }
    if ed.session.review.is_some() {
        return Err("an ask is already in progress (:ask-cancel)".into());
    }
    let cfg = ed.config.llm.clone();
    let doc = ed.doc();
    let ranges: Vec<(usize, usize)> = doc
        .selection()
        .ranges()
        .iter()
        .map(|r| {
            let r = r.min_width_1(&doc.text);
            (r.from(), r.to())
        })
        .collect();
    let prompt =
        build_prompt(instruction, &doc.display_name(&ed.session.root), &doc.text, &ranges, cfg.context_lines);
    let hunks = ranges
        .iter()
        .map(|&(from, to)| Hunk {
            from,
            to,
            old: doc.text.byte_slice(from..to).to_string(),
            new: String::new(),
            decision: Decision::Undecided,
            conflict: false,
        })
        .collect::<Vec<_>>();
    let doc_id = doc.id;
    start(ed, doc_id, instruction, &prompt, hunks)
}

/// Claude writes what `from..to` of document `doc_id` should be, from `prompt` alone (one answer, no tools) —
/// streamed, then the in-buffer review like `space i`'s (a commit message's draft).
pub fn draft(
    ed: &mut Editor,
    doc_id: DocId,
    label: &str,
    prompt: &str,
    from: usize,
    to: usize,
) -> Result<(), String> {
    if ed.session.review.is_some() {
        return Err("Claude is already answering (:ask-cancel)".into());
    }
    let doc = ed.docs.iter().find(|d| d.id == doc_id).ok_or("the buffer is gone")?;
    let old = doc.text.byte_slice(from..to).to_string();
    let hunk = Hunk { from, to, old, new: String::new(), decision: Decision::Undecided, conflict: false };
    start(ed, doc_id, label, prompt, vec![hunk])
}

/// One answer for `hunks` of `doc_id` (the prompt asks for a `<r i="N">` each): the process, the review
/// waiting for it, the stream into it.
fn start(
    ed: &mut Editor,
    doc_id: DocId,
    instruction: &str,
    prompt: &str,
    hunks: Vec<Hunk>,
) -> Result<(), String> {
    let cfg = ed.config.llm.clone();
    let mut child = ed.llm_warm.take(&cfg, &ed.session.root)?;
    // Write the message and close stdin — the process exits on its own after this one turn (written off the
    // main loop: a commit draft's prompt can be 80 KB, and a spare still starting reads nothing yet).
    // Before anything is installed: a process without stdin leaves no review waiting forever.
    let sent =
        child.stdin.take().ok_or_else(|| "claude: no stdin".to_string()).and_then(|stdin| {
            Lines::new(stdin).send(user_message(prompt)).map_err(|e| format!("claude: {e}"))
        });
    if let Err(e) = sent {
        // Reaped off the main thread
        crate::event::thread("tarae-reap", move || {
            let _ = child.kill();
            let _ = child.wait();
        });
        return Err(e);
    }
    ed.llm_generation += 1;
    let generation = ed.llm_generation;
    let n = hunks.len();
    ed.session.review = Some(Review::new(generation, doc_id, instruction, hunks));
    ed.llm_pid = Some(child.id());
    pump(child, true, ed.events.sender(), move |ev| {
        Some(match ev {
            StreamEv::Thinking | StreamEv::Thought(_) => Box::new(move |ed: &mut Editor| {
                if let Some(r) = ed.session.review.as_mut().filter(|r| r.generation == generation) {
                    r.thinking = true;
                }
            }),
            StreamEv::Text(t) => Box::new(move |ed: &mut Editor| {
                if let Some(r) = ed.session.review.as_mut().filter(|r| r.generation == generation) {
                    r.thinking = false;
                    r.streamed.push_str(&t);
                }
            }),
            StreamEv::Done(r) => {
                let result = r.and_then(|out| parse_replies(&out, n));
                Box::new(move |ed: &mut Editor| on_result(ed, generation, result))
            }
            // (what it cost is the session's too — a turn of its own, beside the chat's conversation; a
            // result that reports nothing has nothing to add)
            StreamEv::Spent(s) if !s.counts.any() && s.total_cost.is_none() => return None,
            StreamEv::Spent(s) => Box::new(move |ed: &mut Editor| {
                ed.session.usage.aside(s, "ask");
                crate::usage::view_sync(ed);
            }),
            // ask runs without tools, and its conversation isn't kept
            StreamEv::Requesting(_)
            | StreamEv::Usage(_)
            | StreamEv::Conversation { .. }
            | StreamEv::Answered(_)
            | StreamEv::Models(_)
            | StreamEv::Tool { .. }
            | StreamEv::ToolResult { .. }
            | StreamEv::Control { .. }
            | StreamEv::Compaction(_)
            | StreamEv::Exit(_) => {
                return None;
            }
        })
    });
    Ok(())
}

/// Cancel: if a request is pending, kill the process (stops tokens too); if reviewing, reject all.
pub fn cancel(ed: &mut Editor) {
    if let Some(pid) = ed.llm_pid.take() {
        kill(pid);
    }
    if ed.session.review.is_some() {
        close_review(ed);
        ed.llm_generation += 1;
        ed.set_status("claude: cancelled");
    }
}

/// Opens a review directly with changes in hand (chat code apply, chat edits, change sets — same y/n flow·one undo as
/// ask) + what to call when done (applied?).
pub fn review_with(
    ed: &mut Editor,
    doc_id: DocId,
    instruction: &str,
    changes: Vec<(usize, usize, String, String)>,
    on_done: Option<OnDone>,
) {
    close_review(ed);
    ed.llm_generation += 1;
    let hunks: Vec<Hunk> = changes
        .into_iter()
        .map(|(from, to, old, new)| Hunk {
            from,
            to,
            decision: if old == new { Decision::Reject } else { Decision::Undecided },
            old,
            new,
            conflict: false,
        })
        .collect();
    if hunks.iter().all(|h| h.decision == Decision::Reject) {
        if let Some(cb) = on_done {
            cb(ed, false);
        }
        return ed.set_status("claude: no changes");
    }
    ed.session.review =
        Some(Review { ready: true, on_done, ..Review::new(ed.llm_generation, doc_id, instruction, hunks) });
}

/// A commit message with its body wrapped at `width` (git's convention, 72): the subject line stays as it
/// is; a longer body line breaks between words, a list item's continuation under its text; indented lines
/// (code), trailers (`Co-Authored-By: …`) and words longer than the width stay whole.
pub fn wrap_message(text: &str, width: usize) -> String {
    let mut out = String::new();
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        let trailer = line.split_once(": ").is_some_and(|(k, _)| !k.is_empty() && !k.contains(' '));
        if i == 0 || line.chars().count() <= width || line.starts_with([' ', '\t']) || trailer {
            out.push_str(line);
            continue;
        }
        // A list item's lines go on under its text
        let hang = ["- ", "* "].iter().find(|b| line.starts_with(**b)).map_or(0, |b| b.len());
        let mut row = String::new();
        for word in line.split(' ').filter(|w| !w.is_empty()) {
            if !row.is_empty() && row.chars().count() + 1 + word.chars().count() > width {
                out.push_str(&row);
                out.push('\n');
                row = " ".repeat(hang);
            } else if !row.is_empty() && !row.ends_with(' ') {
                row.push(' ');
            }
            row.push_str(word);
        }
        out.push_str(&row);
    }
    out
}

/// Preview of the streaming answer: the last few lines inside the `<r i="N">` being written.
pub fn preview(streamed: &str, rows: usize) -> Vec<&str> {
    let body = match streamed.rfind("<r i=\"") {
        Some(i) => {
            let rest = &streamed[i..];
            rest.find('>').map_or("", |g| &rest[g + 1..])
        }
        None => streamed,
    };
    let body = body.split("</r>").next().unwrap_or_default().trim_start_matches('\n');
    let lines: Vec<&str> = body.lines().collect();
    lines[lines.len().saturating_sub(rows)..].to_vec()
}

fn on_result(ed: &mut Editor, generation: u64, result: Result<Vec<String>, String>) {
    let Some(rev) = ed.session.review.as_mut().filter(|r| r.generation == generation) else { return };
    ed.llm_pid = None;
    let secs = rev.started.elapsed().as_secs_f32();
    let replies = match result {
        Ok(r) => r,
        Err(e) => {
            ed.session.review = None;
            ed.set_error(format!("claude: {e}"));
            return;
        }
    };
    let Some(doc_idx) = ed.docs.iter().position(|d| d.id == rev.doc_id) else {
        ed.session.review = None;
        ed.set_error("claude: buffer was closed");
        return;
    };
    let text = &ed.docs[doc_idx].text;
    let wrap = rev.wrap;
    for (h, new) in rev.hunks.iter_mut().zip(replies) {
        let new = match wrap {
            Some(w) => wrap_message(&new, w),
            None => new,
        };
        h.new = match_trailing_newline(&h.old, new);
        if h.new == h.old {
            h.decision = Decision::Reject;
        } else if !relocate(h, text) {
            h.conflict = true;
        }
    }
    if rev.hunks.iter().all(|h| h.decision == Decision::Reject || h.conflict) {
        let msg = if rev.hunks.iter().any(|h| h.conflict) {
            "claude: text changed while waiting — nothing applied"
        } else {
            "claude: no changes"
        };
        ed.session.review = None;
        ed.set_status(msg);
        return;
    }
    rev.ready = true;
    rev.current =
        rev.hunks.iter().position(|h| h.decision == Decision::Undecided && !h.conflict).unwrap_or(0);
    ed.current = doc_idx;
    ed.set_status(format!("claude answered in {secs:.1}s"));
}

/// If edits happened while waiting, relocates the original (nearest match). False if not found.
fn relocate(h: &mut Hunk, text: &Rope) -> bool {
    // Edits while waiting may leave the old position mid-character — check boundaries before slicing.
    let boundary = |b: usize| b <= text.len_bytes() && text.char_to_byte(text.byte_to_char(b)) == b;
    if boundary(h.from) && boundary(h.to) && h.from <= h.to && text.byte_slice(h.from..h.to) == h.old.as_str()
    {
        return true;
    }
    if h.old.is_empty() {
        return false;
    }
    let s = text.to_string();
    let best = s.match_indices(&h.old).map(|(b, _)| b).min_by_key(|&b| b.abs_diff(h.from));
    match best {
        Some(from) => {
            h.to = from + h.old.len();
            h.from = from;
            true
        }
        None => false,
    }
}

/// Keys during review. True if handled. While it waits, the editor's own keys still work over it (#61): a
/// picker or a prompt open, a key sequence under way (`space W`, `space m c`), `space`, `:` and `C-w` that
/// start one — and `C-d` `C-u` `C-f` `C-b` `C-e` `C-y` (page keys too) scroll it. Esc gives the keys back
/// where they were before it took them, the change unanswered (#70, #118): to the chat if it had them,
/// else to the code — `C-q` brings them back to it.
pub fn review_key(ed: &mut Editor, key: Key) -> bool {
    if ed.picker.is_some() || ed.prompt.is_some() || !ed.pending.is_empty() {
        return false;
    }
    let page = ed.viewport.0.max(2) as isize;
    let chat = ed.session.chat.as_ref().map(|c| c.focused);
    let Some(rev) = ed.session.review.as_mut().filter(|r| r.ready) else { return false };
    let c_q = key.ctrl && !key.alt && key.code == Code::Char('q');
    // Back in the chat: its keys are the chat's until `C-q`
    if chat == Some(true) {
        if c_q {
            rev.away = false;
            if let Some(c) = ed.session.chat.as_mut() {
                c.focused = false;
            }
            return true;
        }
        return false;
    }
    // Back in the code: the editor's until `C-q`
    if rev.away {
        rev.away = !c_q;
        return c_q;
    }
    let scroll = |rev: &mut Review, by: isize| {
        rev.scroll = rev.scroll.saturating_add_signed(by);
        rev.follow = false;
        false
    };
    let done = match (key.code, key.ctrl, key.alt) {
        (Code::Char(' ' | ':'), false, false) | (Code::Char('w'), true, false) => return false,
        (Code::Char('d'), true, false) => scroll(rev, page / 2),
        (Code::Char('u'), true, false) => scroll(rev, -page / 2),
        (Code::Char('f'), true, false) | (Code::PageDown, false, false) => scroll(rev, page),
        (Code::Char('b'), true, false) | (Code::PageUp, false, false) => scroll(rev, -page),
        (Code::Char('e'), true, false) => scroll(rev, 1),
        (Code::Char('y'), true, false) => scroll(rev, -1),
        (Code::Esc, ..) => {
            match ed.session.chat.as_mut().filter(|_| rev.from_chat) {
                Some(c) => {
                    c.focused = true;
                    ed.set_status("back to the chat — the change waits unanswered, C-q answers it");
                }
                None => {
                    rev.away = true;
                    ed.mode = crate::editor::Mode::Normal;
                    ed.set_status("back to the code — the change waits unanswered, C-q answers it");
                }
            }
            return true;
        }
        _ => return review_answer(ed, key),
    };
    if done {
        finish(ed);
    }
    true
}

/// The review's own keys: `y` `n` `a` answer, `q` ends it, `j` `k` scroll, `tab` goes to the next change.
fn review_answer(ed: &mut Editor, key: Key) -> bool {
    let Some(rev) = ed.session.review.as_mut().filter(|r| r.ready) else { return false };
    let decide = |rev: &mut Review, d: Decision| {
        rev.hunks[rev.current].decision = d;
        rev.follow = true;
        match rev.next_undecided() {
            Some(i) => {
                rev.current = i;
                false
            }
            None => true,
        }
    };
    let done = match (key.code, key.ctrl || key.alt) {
        (Code::Char('y'), false) => decide(rev, Decision::Accept),
        (Code::Char('n'), false) => decide(rev, Decision::Reject),
        (Code::Char('a'), false) => {
            rev.hunks
                .iter_mut()
                .filter(|h| h.decision == Decision::Undecided)
                .for_each(|h| h.decision = Decision::Accept);
            true
        }
        (Code::Char('q'), false) => true,
        (Code::Char('j') | Code::Down, false) => {
            rev.scroll += 1;
            rev.follow = false;
            false
        }
        (Code::Char('k') | Code::Up, false) => {
            rev.scroll = rev.scroll.saturating_sub(1);
            rev.follow = false;
            false
        }
        (Code::Tab, _) => {
            rev.current = rev.next_undecided().unwrap_or(rev.current);
            rev.follow = true;
            false
        }
        _ => {
            let out = "y takes it, n leaves it, a takes all, q ends — esc goes back unanswered";
            ed.key_unused(key, "the review of this change", out);
            return true;
        }
    };
    if done {
        finish(ed);
    }
    true
}

/// Accepted changes as one transaction (= one undo).
fn finish(ed: &mut Editor) {
    let Some(mut rev) = ed.session.review.take() else { return };
    let on_done = rev.on_done.take();
    let before = ed.docs.iter().find(|d| d.id == rev.doc_id).map(|d| d.text.clone());
    // (the editor's keys work while it waits — #61: a change of the text since is found again, or skipped)
    if let Some(text) = &before {
        for h in rev.hunks.iter_mut().filter(|h| h.decision == Decision::Accept && !h.conflict) {
            h.conflict = !relocate(h, text);
        }
    }
    let applied = apply_accepted(ed, &rev);
    // A step `space u` takes back on its own
    if applied
        && rev.own_turn
        && let Some(before) = before
    {
        crate::checkpoint::single(ed, rev.doc_id, before, &rev.instruction);
    }
    if let Some(cb) = on_done {
        cb(ed, applied);
    }
}

fn apply_accepted(ed: &mut Editor, rev: &Review) -> bool {
    let accepted: Vec<&Hunk> =
        rev.hunks.iter().filter(|h| h.decision == Decision::Accept && !h.conflict).collect();
    if accepted.is_empty() {
        ed.set_status("claude: nothing applied");
        return false;
    }
    let Some(idx) = ed.docs.iter().position(|d| d.id == rev.doc_id) else { return false };
    ed.current = idx;
    let tx = Transaction::new(
        accepted.iter().map(|h| Change { from: h.from, to: h.to, insert: h.new.clone() }).collect(),
    );
    let ranges: Vec<Range> = accepted
        .iter()
        .map(|h| {
            let start = tx.map_pos(h.from, Assoc::Before);
            Range::new(start, start + h.new.len())
        })
        .collect();
    let n = ranges.len();
    ed.with_group(|cx| cx.editor.doc_mut().apply_with(&tx, Selection::new(ranges, 0)));
    ed.set_status(format!("claude: applied {n} change(s) — u to undo"));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests never start the real claude (card model P2 caught one reading ~/.claude.json and the keychain):
    /// the default command, by name or path, is refused before anything starts.
    #[test]
    #[should_panic(expected = "a test started the real claude")]
    fn a_test_never_starts_the_real_claude() {
        // (a path that isn't there: without the guard, nothing starts and nothing panics)
        let cfg = LlmConfig { command: "/nonexistent/claude".into(), ..LlmConfig::default() };
        let _ = spawn(&cfg, &[], &std::env::temp_dir());
    }

    /// No claude: Claude Code isn't installed — how to get it and log in, as its docs say (#163); any other
    /// failure as it is.
    #[test]
    fn a_missing_claude_says_how_to_get_it() {
        let gone = std::io::Error::from(std::io::ErrorKind::NotFound);
        for command in ["claude", "/opt/x/claude"] {
            let whole = spawn_failed(command, &gone, true);
            assert!(whole.starts_with("Claude Code isn't installed"), "{whole}");
            assert!(whole.contains("curl -fsSL https://claude.ai/install.sh | bash"), "{whole}");
            assert!(whole.contains("log in by running `claude`"), "{whole}");
            assert!(!whole.contains("os error"), "{whole}");
            let one = spawn_failed(command, &gone, false);
            assert!(!one.contains('\n') && one.contains("install.sh"), "a toast's one line: {one}");
        }
        assert_eq!(spawn_failed("my-agent", &gone, true), "my-agent isn't on PATH (llm.command)");
        assert_eq!(spawn_failed("/x/my-agent", &gone, true), "/x/my-agent isn't there (llm.command)");
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(spawn_failed("claude", &denied, true).starts_with("claude: "), "not a missing one");
    }

    /// A commit message's body wrapped at 72: the subject and short lines as they are, a long line between
    /// words, a list item's rest under its text; code, trailers and an over-long word whole.
    #[test]
    fn commit_messages_wrap_at_72() {
        let long = "word ".repeat(20).trim_end().to_string(); // 99 columns
        let msg = format!(
            "{long}\n\nshort line\n{long}\n- {long}\n    {long}\nCo-Authored-By: {long}\n{}",
            "x".repeat(80)
        );
        let out = wrap_message(&msg, 72);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], long, "the subject stays");
        assert_eq!(lines[2], "short line");
        let row = "word ".repeat(14).trim_end().to_string(); // 69 columns — one more word would be 74
        assert_eq!((lines[3], lines[4]), (row.as_str(), "word word word word word word"));
        assert_eq!(lines[5], format!("- {}", "word ".repeat(14).trim_end()), "a list item (71 columns)");
        assert!(lines[6].starts_with("  word"), "its rest under its text: {:?}", lines[6]);
        assert!(lines.contains(&format!("    {long}").as_str()), "code stays");
        assert!(lines.contains(&format!("Co-Authored-By: {long}").as_str()), "a trailer stays");
        assert_eq!(*lines.last().unwrap(), "x".repeat(80), "a word longer than the width stays whole");
        assert!(
            lines[2..]
                .iter()
                .all(|l| l.len() <= 72 || l.starts_with(' ') || !l.contains(' ') || l.contains(": "))
        );
    }

    #[test]
    fn stream_lines_and_preview() {
        let delta = serde_json::json!({"type": "stream_event", "event": {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "ab"}}});
        assert!(matches!(parse_line(&delta), Some(StreamEv::Text(t)) if t == "ab"));
        let think =
            serde_json::json!({"type": "system", "subtype": "thinking_tokens", "estimated_tokens": 5});
        assert!(matches!(parse_line(&think), Some(StreamEv::Thinking)));
        let stopped =
            serde_json::json!({"type": "result", "subtype": "error_during_execution", "result": ""});
        assert!(matches!(parse_line(&stopped), Some(StreamEv::Done(Err(e))) if e == "stopped"));
        // Preview: the last lines inside the tag being written
        assert_eq!(preview("<r i=\"1\">a</r>\n<r i=\"2\">\nx\ny\nz", 2), vec!["y", "z"]);
        assert_eq!(preview("plain\ntext", 5), vec!["plain", "text"]);
    }

    #[test]
    fn parses_tagged_replies() {
        let out = "<r i=\"1\">\nfoo\n</r>\n<r i=\"2\">bar</r>";
        assert_eq!(parse_replies(out, 2).unwrap(), vec!["foo", "bar"]);
        assert!(parse_replies("<r i=\"1\">x</r>", 2).is_err());
        // With one selection, also an answer without tags·fences
        assert_eq!(parse_replies("```rust\nlet x = 1;\n```", 1).unwrap(), vec!["let x = 1;"]);
    }

    #[test]
    fn review_rows_expand_changes_in_place() {
        let text = Rope::from_str("a\nb\nc\n");
        let hunk = |decision| Hunk {
            from: 2,
            to: 4,
            old: "b\n".into(),
            new: "B\nB2\n".into(),
            decision,
            conflict: false,
        };
        let rev = |h| Review::new(0, 0, "", vec![h]);
        let diff = |kind, t: &str, no| RRow::Diff { kind, text: t.into(), old_no: no, hunk: 0 };
        let rows = rev(hunk(Decision::Undecided)).rows(&text);
        assert_eq!(
            rows,
            [
                RRow::Doc(0),
                RRow::Header(0),
                diff('-', "b", Some(2)),
                diff('+', "B", None),
                diff('+', "B2", None),
                RRow::Doc(2),
                RRow::Doc(3)
            ]
        );
        // Accept = new text only, reject = original only
        let acc = rev(hunk(Decision::Accept)).rows(&text);
        assert!(
            acc.contains(&diff('+', "B", None))
                && !acc.iter().any(|r| matches!(r, RRow::Diff { kind: '-', .. }))
        );
        let rej = rev(hunk(Decision::Reject)).rows(&text);
        assert_eq!(rej[2], diff(' ', "b", Some(2)));
        assert_eq!(rej.len(), 5);
    }

    #[test]
    fn trailing_newline_follows_original() {
        assert_eq!(match_trailing_newline("a\n", "b".into()), "b\n");
        assert_eq!(match_trailing_newline("a", "b\n\n".into()), "b");
    }

    #[test]
    fn line_diff_marks_changes() {
        let d = line_diff("a\nb\nc", "a\nB\nc");
        assert_eq!(d, vec![(' ', "a"), ('-', "b"), ('+', "B"), (' ', "c")]);
    }

    #[test]
    fn prompt_carries_selection_and_context() {
        let text = Rope::from_str("one\ntwo\nthree\n");
        let p = build_prompt("upper", "x.txt", &text, &[(4, 7)], 1);
        assert!(p.contains("Instruction: upper"));
        assert!(p.contains("<text>two</text>"));
        assert!(p.contains("<before>one\n</before>"));
        assert!(p.contains("lines=\"2-2\""));
        // A line-wise selection (up to the next line's start) is still that one line
        let p = build_prompt("upper", "x.txt", &text, &[(4, 8)], 1);
        assert!(p.contains("lines=\"2-2\""), "{p}");
        assert!(p.contains("<after>three\n</after>"), "{p}");
    }

    #[test]
    fn tool_calls_come_from_assistant_messages() {
        let ev = serde_json::json!({"type": "assistant", "message": {"content": [
            {"type": "thinking", "thinking": ""},
            {"type": "tool_use", "name": "Read", "input": {"file_path": "/p/src/lib.rs", "offset": 10}},
            {"type": "tool_use", "name": "Grep", "input": {"pattern": "fn add"}},
        ]}});
        let got: Vec<(String, serde_json::Value)> = tool_events(&ev)
            .into_iter()
            .map(|e| match e {
                StreamEv::Tool { name, input, .. } => (name, input),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, "Read");
        assert_eq!(got[0].1["offset"], 10);
        assert_eq!(got[1].1["pattern"], "fn add");
        let text = serde_json::json!({"type": "assistant", "message": {"content": [{"type": "text", "text": "hi"}]}});
        assert!(tool_events(&text).is_empty());
        let result = serde_json::json!({"type": "user", "message": {"content": [
            {"type": "tool_result", "tool_use_id": "t1", "content": [{"type": "text", "text": "a.rs:3:fn a"}]}]}});
        assert!(matches!(&tool_events(&result)[..],
            [StreamEv::ToolResult { id, text, error: false }] if id == "t1" && text == "a.rs:3:fn a"));
        // Summarized thinking carries text; the omitted kind is just a signal
        let delta = |t: &str| {
            serde_json::json!({"type": "stream_event", "event": {"type": "content_block_delta",
            "delta": {"type": "thinking_delta", "thinking": t}}})
        };
        assert!(matches!(parse_line(&delta("hmm")), Some(StreamEv::Thought(t)) if t == "hmm"));
        assert!(matches!(parse_line(&delta("")), Some(StreamEv::Thinking)));
    }

    #[test]
    fn flags_are_replaced_not_repeated() {
        let args = with_flag(&LlmConfig::default().args, "--setting-sources", "user,project,local");
        let at = args.iter().position(|a| a == "--setting-sources").unwrap();
        assert_eq!(args[at + 1], "user,project,local");
        assert_eq!(args.iter().filter(|a| *a == "--setting-sources").count(), 1);
        assert_eq!(with_flag(&[], "--x", "1"), ["--x", "1"]);
    }

    #[test]
    fn chat_tools_replace_the_no_tools_default() {
        let args =
            with_tools(&LlmConfig::default().args, "Read,Grep,Glob", "Read,Grep,Glob,mcp__tarae__note");
        let at = |f: &str| args.iter().position(|a| a == f).unwrap();
        assert_eq!(args.iter().filter(|a| *a == "--tools").count(), 1);
        assert_eq!(args[at("--tools") + 1], "Read,Grep,Glob");
        assert_eq!(args[at("--allowedTools") + 1], "Read,Grep,Glob,mcp__tarae__note");
        assert!(args.windows(2).all(|w| w[0] != "--setting-sources" || w[1].is_empty()), "rest kept");
    }
}
