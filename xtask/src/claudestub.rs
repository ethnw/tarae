//! `cargo xtask claude-stub serve | drive | summary` — what the `claude` program touches on Linux, measured with
//! no real credentials (validation 1, its Linux half; `target/sb/v1b-linux.sh` runs the three in a container):
//!
//! - `serve --project DIR [--port N] [--log FILE]` — a stand-in for Anthropic's Messages API on 127.0.0.1
//!   (`ANTHROPIC_BASE_URL`), streaming or not. The chat's requests (those carrying claude's `Read` tool) get a
//!   scripted conversation, one step per assistant message so far — Read a file in DIR, Edit it, Write a new one,
//!   Read one outside DIR (a permission prompt), then the turn's end; claude's side requests get a short text.
//!   Every request is a line in the log: what was asked, the last tool result, what it got. The port it bound is
//!   printed first (port 0: any).
//!   `--script enforce --home H --absent TEXT --present TEXT` (run C: claude under its Landlock rules) adds a Read
//!   of a fake `H/.ssh/id_ed25519`, a Read of `H/.bashrc` and a Write over it, and judges each step's result —
//!   done, refused with `EACCES`, or failed — and every chat request: `--absent` (a marker in claude's folder's
//!   `CLAUDE.md`) not in it, `--present` (one in the project's) in it. Each judgement a `check … PASS|FAIL` line.
//! - `drive --transcript FILE --prompt TEXT [--cwd DIR] [--timeout S] -- COMMAND…` — tarae's side of the chat's
//!   pipes, as `chat::ensure_proc` speaks it: `initialize` registering tarae's MCP server, then the prompt; every
//!   `can_use_tool` allowed with its input (a `y` on the review), the MCP messages answered with no tools, claude's
//!   stdin closed at its `result`. COMMAND (claude, or strace around it) runs in a process group of its own,
//!   killed whole at the timeout. Says what it was asked.
//! - `summary --label L --strace FILE --project DIR --home DIR` — an `strace -f -yy` log as Markdown: what was
//!   written, opened and looked at outside DIR by folder, claude's own state files, renames with their targets,
//!   links, processes started, sockets.
//!
//! An xtask rather than an `#[ignore]`d test: it runs in the container beside claude — a server, and a driver of
//! claude's pipes — and xtask builds there in a minute where tarae's test binary would take many. Its tests drive
//! each part with a fake on the other side, no Docker.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::Result;

const USAGE: &str = "usage: cargo xtask claude-stub serve --project DIR [--port N] [--log FILE] [--script enforce --home DIR --absent TEXT --present TEXT] | drive --transcript FILE --prompt TEXT [--cwd DIR] [--timeout S] -- COMMAND… | summary --label L --strace FILE --project DIR --home DIR";

pub fn run(args: &[String]) -> Result<()> {
    match args.first().map(String::as_str) {
        Some("serve") => serve(&args[1..]),
        Some("drive") => drive(&args[1..]),
        Some("summary") => summary_cmd(&args[1..]),
        _ => Err(USAGE.into()),
    }
}

/// `--name value` among the options (those before a `--`).
fn opt<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let end = args.iter().position(|a| a == "--").unwrap_or(args.len());
    args[..end].windows(2).find(|w| w[0] == name).map(|w| w[1].as_str())
}

fn need<'a>(args: &'a [String], name: &str) -> Result<&'a str> {
    opt(args, name).ok_or_else(|| format!("{name} missing — {USAGE}"))
}

// ---- serve: the fake API ----

/// The file the script's last Read asks for: outside the project (claude asks before reading it), there on
/// Debian, and no key.
const OUTSIDE: &str = "/etc/os-release";

/// One step of the chat's conversation: the tool claude is told to call, with what, what its result must be (judged
/// only in the `enforce` script), and why that.
struct Step {
    name: &'static str,
    input: Value,
    expect: Expect,
    why: &'static str,
}

/// The chat's conversation: claude's request after `n` assistant messages gets step `n`, then the turn's end —
/// with `enforce`, its steps after the rest, and the project's as its rules have them (N1).
fn steps(project: &str, enforce: Option<&Enforce>) -> Vec<Step> {
    let p = |f: &str| format!("{}/{f}", project.trim_end_matches('/'));
    let step = |name, input, expect, why| Step { name, input, expect, why };
    let confined = enforce.is_some();
    let mut s = vec![
        step("Read", json!({ "file_path": p("hello.txt") }), Expect::Done, "the project is read"),
        step(
            "Edit",
            json!({ "file_path": p("hello.txt"), "old_string": "hello", "new_string": "hello, edited" }),
            Expect::Done,
            match confined {
                // (measured, run C: its temp file beside it refused, then written in place)
                true => "a file there, in the project's top: claude 2.1.296 falls back to writing in place",
                false => "the project is written",
            },
        ),
        step(
            "Write",
            json!({ "file_path": p("written.txt"), "content": "written by claude\n" }),
            if confined { Expect::Fails } else { Expect::Done },
            match confined {
                true => {
                    "a new file in the project's top: N1, a folder holding a protected name (.git) takes no new entry"
                }
                false => "the project is written",
            },
        ),
        step("Read", json!({ "file_path": OUTSIDE }), Expect::Done, "outside home is read"),
    ];
    if let Some(e) = enforce {
        let h = |f: &str| format!("{}/{f}", e.home.trim_end_matches('/'));
        s.extend([
            step(
                "Read",
                json!({ "file_path": h(".ssh/id_ed25519") }),
                Expect::Eacces,
                "a key folder is kept",
            ),
            step(
                "Read",
                json!({ "file_path": h(".bashrc") }),
                Expect::Done,
                "home is read around what's kept",
            ),
            step(
                "Write",
                json!({ "file_path": h(".bashrc"), "content": "echo written by claude\n" }),
                Expect::Fails,
                "home isn't written",
            ),
        ]);
    }
    s
}

/// The `enforce` script's places and markers (`serve --script enforce`).
struct Enforce {
    /// claude's home: the fake key and `.bashrc` are there.
    home: String,
    /// Never in a chat request: the marker in claude's folder's `CLAUDE.md` (user sources off).
    absent: String,
    /// Always in a chat request: the marker in the project's `CLAUDE.md` — the check above can fail.
    present: String,
}

/// What a step's tool result must be (judged only in the `enforce` script).
#[derive(Clone, Copy, Debug, PartialEq)]
enum Expect {
    /// Done, as in runs A and B: no error.
    Done,
    /// Refused by the kernel: Node's `EACCES` in it (Landlock's word).
    Eacces,
    /// Not done: an error, whatever its words.
    Fails,
}

impl Expect {
    /// Whether a result (is it an error, its text) is what this step wanted.
    fn met(self, result: Option<&(bool, String)>) -> bool {
        match (self, result) {
            (_, None) => false,
            (Expect::Done, Some((error, _))) => !error,
            (Expect::Eacces, Some((_, text))) => text.contains("EACCES"),
            (Expect::Fails, Some((error, text))) => {
                *error || text.contains("EACCES") || text.contains("EPERM")
            }
        }
    }
}

const DONE: &str = "Done: read, edited, wrote, and read outside the project.";

struct Stub {
    project: String,
    log: Mutex<Box<dyn Write + Send>>,
    seq: AtomicU64,
    enforce: Option<Enforce>,
}

struct Request {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(h, _)| h.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

struct Reply {
    status: u16,
    content_type: &'static str,
    body: String,
}

impl Reply {
    fn ok(content_type: &'static str, body: String) -> Reply {
        Reply { status: 200, content_type, body }
    }
}

fn serve(args: &[String]) -> Result<()> {
    let project = need(args, "--project")?.to_string();
    let port: u16 = opt(args, "--port").unwrap_or("0").parse().map_err(|e| format!("--port: {e}"))?;
    let log: Box<dyn Write + Send> = match opt(args, "--log") {
        Some(p) => Box::new(std::fs::File::create(p).map_err(|e| format!("{p}: {e}"))?),
        None => Box::new(std::io::stderr()),
    };
    let enforce = match opt(args, "--script") {
        None => None,
        Some("enforce") => Some(Enforce {
            home: need(args, "--home")?.to_string(),
            absent: need(args, "--absent")?.to_string(),
            present: need(args, "--present")?.to_string(),
        }),
        Some(s) => return Err(format!("--script {s}: only `enforce` — {USAGE}")),
    };
    let listener = TcpListener::bind(("127.0.0.1", port)).map_err(|e| format!("bind: {e}"))?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    println!("{port}");
    std::io::stdout().flush().map_err(|e| e.to_string())?;
    Arc::new(Stub { enforce, ..Stub::new(project, log) }).serve_on(listener);
    Ok(())
}

impl Stub {
    fn new(project: String, log: Box<dyn Write + Send>) -> Stub {
        Stub { project, log: Mutex::new(log), seq: AtomicU64::new(1), enforce: None }
    }

    fn say(&self, line: &str) {
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(log, "{line}");
        let _ = log.flush();
    }

    fn serve_on(self: Arc<Self>, listener: TcpListener) {
        for conn in listener.incoming().flatten() {
            let stub = self.clone();
            std::thread::spawn(move || {
                if let Err(e) = stub.handle(conn) {
                    stub.say(&format!("connection: {e}"));
                }
            });
        }
    }

    /// One request per connection: the reply says `Connection: close` and carries its length.
    fn handle(&self, conn: TcpStream) -> std::io::Result<()> {
        conn.set_read_timeout(Some(Duration::from_secs(60)))?;
        let mut write = conn.try_clone()?;
        let req = read_request(&mut BufReader::new(conn), &mut write)?;
        let reply = self.answer(&req);
        let reason = if reply.status == 200 { "OK" } else { "Not Found" };
        write!(
            write,
            "HTTP/1.1 {} {reason}\r\nContent-Type: {}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\n\
             request-id: req_stub\r\nConnection: close\r\n\r\n",
            reply.status,
            reply.content_type,
            reply.body.len()
        )?;
        write.write_all(reply.body.as_bytes())?;
        write.flush()
    }

    fn answer(&self, req: &Request) -> Reply {
        let n = self.seq.fetch_add(1, Ordering::Relaxed);
        let path = req.path.split('?').next().unwrap_or_default();
        let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
        let mut said = format!(
            "#{n} {} {} user-agent={} anthropic-beta={}",
            req.method,
            req.path,
            req.header("user-agent").unwrap_or("-"),
            req.header("anthropic-beta").unwrap_or("-")
        );
        let json = "application/json";
        let reply = match (req.method.as_str(), path) {
            ("POST", "/v1/messages") => {
                let stream = body["stream"] == true;
                let model = body["model"].as_str().unwrap_or("claude-stub");
                let (blocks, stop, what) = self.turn(&body);
                said.push_str(&format!(" model={model} stream={stream} {what}"));
                match stream {
                    true => Reply::ok("text/event-stream", sse(n, model, &blocks, stop)),
                    false => Reply::ok(json, message(n, model, blocks, Value::from(stop)).to_string()),
                }
            }
            ("POST", "/v1/messages/count_tokens") => Reply::ok(json, json!({ "input_tokens": 1 }).to_string()),
            ("GET", "/v1/models") => Reply::ok(
                json,
                json!({ "data": [], "has_more": false, "first_id": null, "last_id": null }).to_string(),
            ),
            _ => Reply {
                status: 404,
                content_type: json,
                body: json!({ "type": "error", "error": { "type": "not_found_error", "message": "not in the stub" } })
                    .to_string(),
            },
        };
        said.push_str(&format!(" → {}", reply.status));
        self.say(&said);
        reply
    }

    /// What a Messages request gets: the script's next step when it's the chat's conversation (its tools hold
    /// `Read`), else a short text. Also what the log says of it.
    fn turn(&self, body: &Value) -> (Vec<Value>, &'static str, String) {
        let tools: Vec<&str> =
            body["tools"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str()).collect();
        let messages = body["messages"].as_array().map(Vec::as_slice).unwrap_or_default();
        if !tools.contains(&"Read") {
            return (vec![text("ok")], "end_turn", format!("side request ({} tools)", tools.len()));
        }
        let turn = messages.iter().filter(|m| m["role"] == "assistant").count();
        let what = format!(
            "chat, turn {turn}, tools {} · ends {}{}",
            tools.join(","),
            shape(messages),
            last_result(messages, turn)
        );
        let steps = steps(&self.project, self.enforce.as_ref());
        if let Some(e) = &self.enforce {
            self.judge(e, turn, &steps, body, messages);
        }
        match steps.into_iter().nth(turn) {
            Some(Step { name, input, .. }) => {
                let said = format!("{what} → {name} {input}");
                let block = json!({ "type": "tool_use", "id": tool_id(turn), "name": name, "input": input });
                (vec![block], "tool_use", said)
            }
            None => (vec![text(DONE)], "end_turn", format!("{what} → the turn's end")),
        }
    }

    /// The `enforce` script's checks on the chat's request at `turn`: the last step's result as it wanted, and the
    /// markers — one `check` line each.
    fn judge(&self, e: &Enforce, turn: usize, steps: &[Step], body: &Value, messages: &[Value]) {
        let verdict = |ok: bool| if ok { "PASS" } else { "FAIL" };
        if let Some(Step { name, input, expect, why }) = turn.checked_sub(1).and_then(|t| steps.get(t)) {
            let result = result_for(messages, &tool_id(turn - 1));
            let said = match &result {
                Some((error, text)) => {
                    let text: String = text.replace('\n', "⏎").chars().take(200).collect();
                    format!("{}{text}", if *error { "(error) " } else { "" })
                }
                None => "no tool result".into(),
            };
            let path = input["file_path"].as_str().unwrap_or_default();
            let ok = expect.met(result.as_ref());
            self.say(&format!(
                "check step {turn} {name} {path} ({why}): {expect:?} {} — {said}",
                verdict(ok)
            ));
        }
        let whole = body.to_string();
        let (absent, present) = (!whole.contains(&e.absent), whole.contains(&e.present));
        self.say(&format!(
            "check turn {turn} claude's folder's CLAUDE.md not in the request: {}",
            verdict(absent)
        ));
        self.say(&format!("check turn {turn} the project's CLAUDE.md in the request: {}", verdict(present)));
    }
}

fn text(s: &str) -> Value {
    json!({ "type": "text", "text": s })
}

/// The id of the tool use the stub answers at `turn` with.
fn tool_id(turn: usize) -> String {
    format!("toolu_stub_{turn}")
}

/// The result of tool use `id`, wherever in the conversation it is (the last one if twice): whether it's an error,
/// and its text. Not only in the last message — claude 2.1.296 puts more after the one that holds it (run C).
fn result_for(messages: &[Value], id: &str) -> Option<(bool, String)> {
    let blocks = messages.iter().rev().flat_map(|m| m["content"].as_array().into_iter().flatten());
    let r = blocks.filter(|b| b["type"] == "tool_result").find(|b| b["tool_use_id"] == id)?;
    let content = match &r["content"] {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts.iter().filter_map(|p| p["text"].as_str()).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    };
    Some((r["is_error"] == true, content))
}

/// The result of the tool use the stub answered at the turn before `turn`, cut short: whether claude's tool did
/// what the script asked.
fn last_result(messages: &[Value], turn: usize) -> String {
    let Some((error, content)) = turn.checked_sub(1).and_then(|t| result_for(messages, &tool_id(t))) else {
        return String::new();
    };
    let content: String = content.replace('\n', "⏎").chars().take(160).collect();
    let error = if error { " (error)" } else { "" };
    format!(" · last tool result{error}: {content}")
}

/// How the conversation ends: its last three messages, each as its role and its blocks' types
/// (`user[tool_result,text]`) — what claude sends after a result, seen in the log.
fn shape(messages: &[Value]) -> String {
    let one = |m: &Value| {
        let blocks: Vec<&str> = match &m["content"] {
            Value::String(_) => vec!["string"],
            c => c.as_array().into_iter().flatten().map(|b| b["type"].as_str().unwrap_or("?")).collect(),
        };
        format!("{}[{}]", m["role"].as_str().unwrap_or("?"), blocks.join(","))
    };
    let from = messages.len().saturating_sub(3);
    messages[from..].iter().map(one).collect::<Vec<_>>().join(" ")
}

fn usage() -> Value {
    json!({ "input_tokens": 10, "output_tokens": 5, "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0 })
}

fn message(n: u64, model: &str, content: Vec<Value>, stop: Value) -> Value {
    json!({ "id": format!("msg_stub_{n}"), "type": "message", "role": "assistant", "model": model,
        "content": content, "stop_reason": stop, "stop_sequence": null, "usage": usage() })
}

/// The Messages API's stream: `message_start`, each block's start · delta · stop, `message_delta`, `message_stop`.
fn sse(n: u64, model: &str, blocks: &[Value], stop: &str) -> String {
    let mut out = String::new();
    let mut event = |data: Value| {
        out.push_str(&format!("event: {}\ndata: {data}\n\n", data["type"].as_str().unwrap_or_default()));
    };
    event(json!({ "type": "message_start", "message": message(n, model, Vec::new(), Value::Null) }));
    for (i, b) in blocks.iter().enumerate() {
        let (empty, delta) = match b["type"].as_str() {
            Some("tool_use") => (
                json!({ "type": "tool_use", "id": b["id"], "name": b["name"], "input": {} }),
                json!({ "type": "input_json_delta", "partial_json": b["input"].to_string() }),
            ),
            _ => (json!({ "type": "text", "text": "" }), json!({ "type": "text_delta", "text": b["text"] })),
        };
        event(json!({ "type": "content_block_start", "index": i, "content_block": empty }));
        event(json!({ "type": "content_block_delta", "index": i, "delta": delta }));
        event(json!({ "type": "content_block_stop", "index": i }));
    }
    event(json!({ "type": "message_delta", "delta": { "stop_reason": stop, "stop_sequence": null },
        "usage": { "output_tokens": 5 } }));
    event(json!({ "type": "message_stop" }));
    out
}

fn invalid(e: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())
}

/// An HTTP/1.1 request: its length from `Content-Length` or chunks, gzip undone; `100 Continue` when asked.
fn read_request(conn: &mut impl BufRead, write: &mut impl Write) -> std::io::Result<Request> {
    let mut line = String::new();
    conn.read_line(&mut line)?;
    let mut first = line.split_whitespace();
    let (Some(method), Some(path)) = (first.next(), first.next()) else {
        return Err(invalid(format!("not a request line: {line:?}")));
    };
    let (method, path) = (method.to_string(), path.to_string());
    let mut headers = Vec::new();
    loop {
        let mut h = String::new();
        if conn.read_line(&mut h)? == 0 || h.trim_end().is_empty() {
            break;
        }
        if let Some((k, v)) = h.trim_end().split_once(':') {
            headers.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    let mut req = Request { method, path, headers, body: Vec::new() };
    if req.header("expect").is_some_and(|v| v.eq_ignore_ascii_case("100-continue")) {
        write.write_all(b"HTTP/1.1 100 Continue\r\n\r\n")?;
        write.flush()?;
    }
    if req.header("transfer-encoding").is_some_and(|v| v.eq_ignore_ascii_case("chunked")) {
        loop {
            let mut size = String::new();
            conn.read_line(&mut size)?;
            let size = size.trim().split(';').next().unwrap_or_default();
            let n = usize::from_str_radix(size, 16).map_err(invalid)?;
            let mut chunk = vec![0; n];
            conn.read_exact(&mut chunk)?;
            req.body.extend(chunk);
            let mut end = String::new();
            conn.read_line(&mut end)?;
            if n == 0 {
                break;
            }
        }
    } else if let Some(n) = req.header("content-length") {
        let n: usize = n.parse().map_err(invalid)?;
        req.body.resize(n, 0);
        conn.read_exact(&mut req.body)?;
    }
    if req.header("content-encoding").is_some_and(|v| v.eq_ignore_ascii_case("gzip")) {
        let mut plain = Vec::new();
        flate2::read::GzDecoder::new(req.body.as_slice()).read_to_end(&mut plain)?;
        req.body = plain;
    }
    Ok(req)
}

// ---- drive: tarae's side of claude's pipes ----

/// What a driven claude asked and how it ended.
#[derive(Debug, Default)]
struct Driven {
    /// The permission prompts (`can_use_tool`) answered with an allow: the tool and the path it named.
    asked: Vec<String>,
    /// The MCP methods answered for tarae's server.
    mcp: Vec<String>,
    /// Its `result` line: subtype, `is_error`, turns.
    result: Option<String>,
    exit: String,
    timed_out: bool,
    lines: usize,
}

impl std::fmt::Display for Driven {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let none = |v: &[String]| if v.is_empty() { "none".to_string() } else { v.join(" · ") };
        writeln!(f, "- exit: {}{}", self.exit, if self.timed_out { " (killed at the timeout)" } else { "" })?;
        writeln!(
            f,
            "- result: {}",
            self.result.as_deref().unwrap_or("none — claude ended before its result")
        )?;
        writeln!(f, "- permission prompts answered with an allow: {}", none(&self.asked))?;
        writeln!(f, "- MCP messages answered for tarae's server: {}", none(&self.mcp))?;
        writeln!(f, "- lines claude wrote: {}", self.lines)
    }
}

fn drive(args: &[String]) -> Result<()> {
    let transcript = Path::new(need(args, "--transcript")?);
    let prompt = need(args, "--prompt")?;
    let timeout: u64 =
        opt(args, "--timeout").unwrap_or("300").parse().map_err(|e| format!("--timeout: {e}"))?;
    let command = args.iter().position(|a| a == "--").map(|i| &args[i + 1..]).unwrap_or_default();
    if command.is_empty() {
        return Err(format!("no command after `--` — {USAGE}"));
    }
    let cwd = opt(args, "--cwd").map(Path::new);
    let driven = drive_command(command, cwd, prompt, transcript, Duration::from_secs(timeout))?;
    print!("{driven}");
    Ok(())
}

/// One line to claude, and into the transcript (`>` ours, `<` claude's).
fn send(stdin: &mut Option<ChildStdin>, transcript: &mut impl Write, line: String) {
    let _ = writeln!(transcript, "> {line}");
    if let Some(s) = stdin {
        let _ = writeln!(s, "{line}");
        let _ = s.flush();
    }
}

fn drive_command(
    command: &[String],
    cwd: Option<&Path>,
    prompt: &str,
    transcript: &Path,
    timeout: Duration,
) -> Result<Driven> {
    use std::os::unix::process::CommandExt as _;
    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .process_group(0);
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    // (the transcript before the command: an error after its start would leave it running, its pipes cut — the
    // watchdog kills only at the timeout)
    let mut out = std::fs::File::create(transcript).map_err(|e| format!("{}: {e}", transcript.display()))?;
    let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", command[0]))?;
    let group = child.id() as libc::pid_t;
    let (done, ended) = std::sync::mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        let late = matches!(ended.recv_timeout(timeout), Err(std::sync::mpsc::RecvTimeoutError::Timeout));
        if late {
            // SAFETY: a signal to the process group the driven command leads, made for it
            unsafe { libc::kill(-group, libc::SIGKILL) };
        }
        late
    });
    let mut stdin = child.stdin.take();
    // As `chat::ensure_proc`: tarae's MCP server registered first (`llm::initialize_message`), then the prompt
    let init = json!({ "type": "control_request", "request_id": "tarae-init",
        "request": { "subtype": "initialize", "sdkMcpServers": ["tarae"] } });
    send(&mut stdin, &mut out, init.to_string());
    let user = json!({ "type": "user", "message": { "role": "user", "content": prompt } });
    send(&mut stdin, &mut out, user.to_string());
    let mut driven = Driven::default();
    let stdout = child.stdout.take().ok_or("no stdout")?;
    for line in BufReader::new(stdout).lines() {
        let Ok(line) = line else { break };
        let _ = writeln!(out, "< {line}");
        driven.lines += 1;
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        match v["type"].as_str() {
            Some("control_request") => {
                let req = &v["request"];
                let response = match req["subtype"].as_str() {
                    Some("can_use_tool") => {
                        let tool = req["tool_name"].as_str().unwrap_or("?");
                        driven
                            .asked
                            .push(format!("{tool} {}", req["input"]["file_path"].as_str().unwrap_or("")));
                        json!({ "behavior": "allow", "updatedInput": req["input"] })
                    }
                    Some("mcp_message") => {
                        driven.mcp.push(req["message"]["method"].as_str().unwrap_or("?").to_string());
                        json!({ "mcp_response": mcp(&req["message"]) })
                    }
                    _ => json!({}),
                };
                // (`llm::control_response`)
                let id = v["request_id"].as_str().unwrap_or_default();
                let answer = json!({ "type": "control_response",
                    "response": { "subtype": "success", "request_id": id, "response": response } });
                send(&mut stdin, &mut out, answer.to_string());
            }
            Some("result") => {
                driven.result = Some(format!(
                    "{} · is_error {} · {} turns",
                    v["subtype"].as_str().unwrap_or("?"),
                    v["is_error"],
                    v["num_turns"]
                ));
                // (its stdin at its end: claude ends, as tarae's chat does when it stops one)
                stdin = None;
            }
            _ => {}
        }
    }
    drop(stdin);
    let status = child.wait().map_err(|e| e.to_string())?;
    let _ = done.send(());
    driven.timed_out = watchdog.join().unwrap_or(false);
    driven.exit = status.to_string();
    Ok(driven)
}

/// tarae's MCP server with no tools: its handshake answered, the list empty (`tools::mcp`, `mcp::common`).
fn mcp(msg: &Value) -> Value {
    let Some(id) = msg.get("id").cloned() else {
        return json!({ "jsonrpc": "2.0", "result": {}, "id": 0 });
    };
    let result = match msg["method"].as_str() {
        Some("initialize") => json!({
            "protocolVersion": msg["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "tarae", "version": "0" },
        }),
        Some("tools/list") => json!({ "tools": [] }),
        _ => json!({}),
    };
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

// ---- summary: the strace log, by folder ----

fn summary_cmd(args: &[String]) -> Result<()> {
    let label = need(args, "--label")?;
    let path = need(args, "--strace")?;
    let log = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
    let project = need(args, "--project")?;
    let home = need(args, "--home")?;
    print!("{}", summary(label, &String::from_utf8_lossy(&log), project, home));
    Ok(())
}

/// One system call from the log, its unfinished and resumed halves joined.
#[derive(Debug)]
struct Call {
    pid: u32,
    name: String,
    args: Vec<String>,
    ret: String,
}

impl Call {
    fn ok(&self) -> bool {
        !self.ret.starts_with('-') && !self.ret.starts_with('?')
    }

    /// `ok`, or the error's name (`ENOENT`).
    fn result(&self) -> String {
        match self.ok() {
            true => "ok".into(),
            false => self.ret.split_whitespace().nth(1).unwrap_or(&self.ret).to_string(),
        }
    }

    fn arg(&self, i: usize) -> &str {
        self.args.get(i).map_or("", String::as_str)
    }
}

/// `strace -f -tt` lines → calls (`PID TIME name(args) = ret`, `<unfinished ...>` / `<... name resumed>`).
fn calls(log: &str) -> Vec<Call> {
    let mut pending: HashMap<u32, String> = HashMap::new();
    let mut out = Vec::new();
    for line in log.lines() {
        let line = line.trim_start();
        let digits = line.find(|c: char| !c.is_ascii_digit()).unwrap_or(line.len());
        let Ok(pid) = line[..digits].parse::<u32>() else { continue };
        let mut rest = line[digits..].trim_start();
        // (`-tt`'s time)
        if rest
            .split(' ')
            .next()
            .is_some_and(|t| t.contains(':') && t.starts_with(|c: char| c.is_ascii_digit()))
        {
            rest = rest.split_once(' ').map_or("", |(_, r)| r).trim_start();
        }
        let full = if let Some(resumed) = rest.strip_prefix("<... ") {
            let Some((_, tail)) = resumed.split_once(" resumed>") else { continue };
            let Some(head) = pending.remove(&pid) else { continue };
            format!("{head}{tail}")
        } else if let Some(head) = rest.strip_suffix("<unfinished ...>") {
            pending.insert(pid, head.trim_end().to_string());
            continue;
        } else {
            rest.to_string()
        };
        let (Some(open), Some(close)) = (full.find('('), full.rfind(") = ")) else { continue };
        let name = &full[..open];
        if close < open || name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            continue;
        }
        let args = split_args(&full[open + 1..close]);
        out.push(Call { pid, name: name.to_string(), args, ret: full[close + 4..].trim().to_string() });
    }
    out
}

/// A call's arguments, split at its top-level commas (strings, brackets and `-yy`'s `<…>` kept whole).
fn split_args(s: &str) -> Vec<String> {
    let (mut out, mut cur) = (Vec::new(), String::new());
    let (mut depth, mut quoted, mut escaped) = (0i32, false, false);
    for c in s.chars() {
        if quoted {
            cur.push(c);
            match (escaped, c) {
                (true, _) => escaped = false,
                (false, '\\') => escaped = true,
                (false, '"') => quoted = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => quoted = true,
            '(' | '[' | '{' | '<' => depth += 1,
            ')' | ']' | '}' | '>' => depth -= 1,
            ',' if depth <= 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

/// A quoted argument's text (`"…"`, strace's escapes undone), or None when it isn't one (`NULL`, a number).
fn unquote(a: &str) -> Option<String> {
    let mut chars = a.strip_prefix('"')?.chars().peekable();
    let mut bytes = Vec::new();
    let push = |c: char, bytes: &mut Vec<u8>| bytes.extend(c.encode_utf8(&mut [0; 4]).as_bytes());
    while let Some(c) = chars.next() {
        match c {
            '"' => break,
            '\\' => match chars.next() {
                Some('n') => bytes.push(b'\n'),
                Some('t') => bytes.push(b'\t'),
                Some('x') => {
                    let hex: String = [chars.next(), chars.next()].into_iter().flatten().collect();
                    bytes.push(u8::from_str_radix(&hex, 16).unwrap_or(b'?'));
                }
                Some(d @ '0'..='7') => {
                    let mut v = d.to_digit(8).unwrap_or(0);
                    while let Some(d) = chars.peek().and_then(|c| c.to_digit(8)).filter(|_| v < 0o40) {
                        v = v * 8 + d;
                        chars.next();
                    }
                    bytes.push(v as u8);
                }
                Some(c) => push(c, &mut bytes),
                None => break,
            },
            c => push(c, &mut bytes),
        }
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// What a descriptor argument stands for under `-yy`: `AT_FDCWD`, `3</a/folder>` → its path, a socket or pipe.
enum Base {
    Cwd,
    Path(String),
    Other,
}

fn base(a: &str) -> Base {
    if a == "AT_FDCWD" {
        return Base::Cwd;
    }
    match (a.find('<'), a.rfind('>')) {
        (Some(l), Some(r)) if r > l && a[l + 1..].starts_with('/') => Base::Path(a[l + 1..r].to_string()),
        _ => Base::Other,
    }
}

/// `a/b/../c` → `a/c`, lexically.
fn normal(p: &Path) -> String {
    let mut out = PathBuf::from("/");
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(n) => out.push(n),
            _ => {}
        }
    }
    out.to_string_lossy().into_owned()
}

/// A path argument made absolute: against its descriptor's folder or the process's cwd.
fn resolve(base: &Base, cwd: &str, path: &str) -> Option<String> {
    let from = match base {
        _ if path.starts_with('/') => return Some(normal(Path::new(path))),
        Base::Cwd => cwd,
        Base::Path(p) => p,
        Base::Other => return None,
    };
    Some(normal(&Path::new(from).join(path)))
}

/// The path `-yy` prints after a returned descriptor (`3</real/path>`): where the open landed, links followed.
fn ret_path(ret: &str) -> Option<String> {
    let (l, r) = (ret.find("</")?, ret.rfind('>')?);
    (ret[..l].chars().all(|c| c.is_ascii_digit()) && r > l).then(|| ret[l + 1..r].to_string())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    /// Made, changed, moved or removed — opens for writing too.
    Write,
    /// Opened for reading (a file or a folder's list): what Landlock's read rights govern.
    Open,
    /// stat, access, readlink, `O_PATH`, chdir: what Landlock doesn't govern.
    Look,
    /// An inotify watch.
    Watch,
}

fn open_kind(flags: &str) -> Kind {
    let has = |f: &str| flags.split(['|', ' ', ',', '{', '}', '=']).any(|w| w == f);
    if ["O_WRONLY", "O_RDWR", "O_CREAT", "O_TRUNC", "O_APPEND"].iter().any(|f| has(f)) {
        Kind::Write
    } else if has("O_PATH") {
        Kind::Look
    } else {
        Kind::Open
    }
}

/// The paths a call names: (descriptor argument, path argument, what it does to it). Descriptor `None` = the
/// path alone (relative to the cwd).
fn named(c: &Call) -> Vec<(Option<usize>, usize, Kind)> {
    use Kind::*;
    match c.name.as_str() {
        "open" => vec![(None, 0, open_kind(c.arg(1)))],
        "openat" | "openat2" => vec![(Some(0), 1, open_kind(c.arg(2)))],
        "creat" | "mkdir" | "rmdir" | "unlink" | "truncate" | "chmod" | "chown" | "lchown" | "utime"
        | "utimes" | "mknod" | "setxattr" | "lsetxattr" | "removexattr" | "lremovexattr" => {
            vec![(None, 0, Write)]
        }
        "mkdirat" | "unlinkat" | "fchmodat" | "fchmodat2" | "fchownat" | "mknodat" | "utimensat"
        | "futimesat" => {
            vec![(Some(0), 1, Write)]
        }
        "rename" | "link" => vec![(None, 0, Write), (None, 1, Write)],
        "renameat" | "renameat2" | "linkat" => vec![(Some(0), 1, Write), (Some(2), 3, Write)],
        "symlink" => vec![(None, 1, Write)],
        "symlinkat" => vec![(Some(1), 2, Write)],
        "stat" | "lstat" | "stat64" | "lstat64" | "access" | "readlink" | "statfs" | "getxattr"
        | "lgetxattr" | "listxattr" | "llistxattr" | "chdir" => vec![(None, 0, Look)],
        "newfstatat" | "fstatat64" | "statx" | "faccessat" | "faccessat2" | "readlinkat" => {
            vec![(Some(0), 1, Look)]
        }
        "execve" => vec![(None, 0, Open)],
        "execveat" => vec![(Some(0), 1, Open)],
        "inotify_add_watch" => vec![(None, 1, Watch)],
        _ => Vec::new(),
    }
}

struct Access {
    kind: Kind,
    call: String,
    path: String,
    result: String,
    by: String,
}

/// What one folder had done in it.
#[derive(Default)]
struct Folder {
    ok: usize,
    failed: BTreeMap<String, usize>,
    calls: BTreeSet<String>,
    by: BTreeSet<String>,
    names: BTreeSet<String>,
}

fn inside(path: &str, dir: &str) -> bool {
    Path::new(path).starts_with(dir)
}

/// Where a path is grouped: its folder, home as `~`, at most three parts deep below `/` or `~`, `/proc`'s
/// numbers as `<n>`; and the name below that.
fn folder_of(path: &str, home: &str) -> (String, String) {
    let parent = Path::new(path).parent().unwrap_or(Path::new("/"));
    let (root, below) = match parent.strip_prefix(home) {
        Ok(below) => ("~", below),
        Err(_) => ("", parent.strip_prefix("/").unwrap_or(parent)),
    };
    let proc = parent.starts_with("/proc");
    let parts: Vec<String> = below
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .map(|c| if proc && c.chars().all(|d| d.is_ascii_digit()) { "<n>".into() } else { c })
        .collect();
    let shown = parts.iter().take(3).cloned().collect::<Vec<_>>().join("/");
    let folder = match (root, shown.is_empty()) {
        ("~", true) => "~".to_string(),
        ("~", false) => format!("~/{shown}"),
        _ => format!("/{shown}"),
    };
    let mut name: Vec<String> = parts.into_iter().skip(3).collect();
    name.push(Path::new(path).file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned()));
    let name = name.join("/");
    let name = if proc {
        name.split('/')
            .map(|c| if c.parse::<u32>().is_ok() { "<n>" } else { c })
            .collect::<Vec<_>>()
            .join("/")
    } else {
        name
    };
    (folder, name)
}

/// A socket address in short: `unix:/path`, `unix:@abstract`, `127.0.0.1:443`, `[::1]:443`, `netlink`.
fn sockaddr(s: &str) -> Option<String> {
    let at = s.find("sa_family=")?;
    let s = &s[at + "sa_family=".len()..];
    let family = &s[..s.find([',', '}']).unwrap_or(s.len())];
    let between = |from: &str, to: char| {
        s.find(from).map(|i| &s[i + from.len()..]).map(|r| &r[..r.find(to).unwrap_or(r.len())])
    };
    Some(match family {
        "AF_UNIX" => match s.find("sun_path=") {
            Some(i) => {
                let v = &s[i + "sun_path=".len()..];
                let (at, v) = v.strip_prefix('@').map_or(("", v), |v| ("@", v));
                format!("unix:{at}{}", unquote(v).unwrap_or_default())
            }
            None => "unix:(unnamed)".into(),
        },
        "AF_INET" => format!(
            "{}:{}",
            between("inet_addr(\"", '"').unwrap_or("?"),
            between("htons(", ')').unwrap_or("?")
        ),
        "AF_INET6" => {
            let addr = between("inet_pton(AF_INET6, \"", '"').unwrap_or("?");
            format!("[{addr}]:{}", between("htons(", ')').unwrap_or("?"))
        }
        "AF_NETLINK" => "netlink".into(),
        other => other.to_string(),
    })
}

fn cell(s: &str) -> String {
    s.replace('|', "\\|").replace('\n', " ")
}

fn cut(s: &str, n: usize) -> String {
    match s.chars().count() > n {
        true => format!("{}…", s.chars().take(n).collect::<String>()),
        false => s.to_string(),
    }
}

fn summary(label: &str, log: &str, project: &str, home: &str) -> String {
    let calls = calls(log);
    let mut cwd: HashMap<u32, String> = HashMap::new();
    let mut exe: HashMap<u32, String> = HashMap::new();
    let mut accesses = Vec::new();
    let (mut renames, mut links, mut procs) = (Vec::new(), Vec::new(), Vec::new());
    let mut sockets: BTreeMap<(String, String, String), (usize, BTreeSet<String>)> = BTreeMap::new();
    let mut families: BTreeMap<String, usize> = BTreeMap::new();
    let mut uring = 0;
    for c in &calls {
        let here = cwd.entry(c.pid).or_insert_with(|| project.to_string()).clone();
        let by = exe.entry(c.pid).or_insert_with(|| "claude".into()).clone();
        // (where a path lands: `-yy`'s path after a returned descriptor, else the argument resolved)
        let path_at = |fd: Option<usize>, p: usize| -> Option<String> {
            let fd = fd.map_or(Base::Cwd, |i| base(c.arg(i)));
            let path = unquote(c.arg(p))?;
            match (path.is_empty(), &fd) {
                (true, Base::Path(b)) => Some(b.clone()),
                _ => resolve(&fd, &here, &path),
            }
        };
        let mut paths = Vec::new();
        for (fd, p, kind) in named(c) {
            // (utimensat's NULL path: the descriptor itself)
            let path = match (c.arg(p), fd) {
                ("NULL", Some(i)) => match base(c.arg(i)) {
                    Base::Path(b) => Some(b),
                    _ => None,
                },
                _ => path_at(fd, p),
            };
            let landed = (c.name.starts_with("open") && c.ok()).then(|| ret_path(&c.ret)).flatten();
            if let Some(path) = landed.or(path) {
                paths.push(path.clone());
                accesses.push(Access {
                    kind,
                    call: c.name.clone(),
                    path,
                    result: c.result(),
                    by: by.clone(),
                });
            }
        }
        match c.name.as_str() {
            "clone" | "clone3" | "fork" | "vfork" => {
                if let Ok(child) = c.ret.split_whitespace().next().unwrap_or_default().parse::<u32>() {
                    cwd.insert(child, here.clone());
                    exe.insert(child, by.clone());
                }
            }
            "chdir" if c.ok() => {
                if let Some(p) = paths.first() {
                    cwd.insert(c.pid, p.clone());
                }
            }
            "fchdir" if c.ok() => {
                if let Base::Path(p) = base(c.arg(0)) {
                    cwd.insert(c.pid, p);
                }
            }
            "execve" | "execveat" => {
                let program = paths.first().cloned().unwrap_or_else(|| c.arg(0).to_string());
                let argv = c.arg(if c.name == "execve" { 1 } else { 2 });
                procs.push(format!(
                    "| {} | {} | `{}` | `{}` | {} |",
                    c.pid,
                    by,
                    cell(&program),
                    cell(&cut(argv, 200)),
                    c.result()
                ));
                if c.ok() {
                    let name = Path::new(&program)
                        .file_name()
                        .map_or(program.clone(), |n| n.to_string_lossy().into());
                    exe.insert(c.pid, name);
                }
            }
            "rename" | "renameat" | "renameat2" | "link" | "linkat" if paths.len() == 2 => {
                let line =
                    format!("- `{}` → `{}` — {} by {}: {}", paths[0], paths[1], c.name, by, c.result());
                if c.name.contains("rename") { renames.push(line) } else { links.push(line) }
            }
            "symlink" | "symlinkat" => {
                let to = paths.first().cloned().unwrap_or_default();
                let target = unquote(c.arg(0)).unwrap_or_default();
                links.push(format!(
                    "- `{to}` → (symlink to) `{target}` — {} by {by}: {}",
                    c.name,
                    c.result()
                ));
            }
            "socket" => {
                let kind = c.arg(1).split('|').next().unwrap_or_default();
                *families.entry(format!("{} {kind}", c.arg(0))).or_default() += 1;
            }
            "connect" | "bind" | "sendto" | "sendmsg" | "sendmmsg" | "listen" | "accept" | "accept4" => {
                let joined = c.args.join(", ");
                let addr = sockaddr(&joined).or_else(|| {
                    // (no address: the socket the descriptor is, as `-yy` names it)
                    let fd = c.arg(0);
                    fd.find('<').map(|i| fd[i + 1..].trim_end_matches('>').to_string())
                });
                let Some(addr) = addr else { continue };
                if c.name.starts_with("send") && !joined.contains("sa_family=") {
                    continue;
                }
                let e = sockets.entry((c.name.clone(), addr, c.result())).or_default();
                e.0 += 1;
                e.1.insert(by.clone());
            }
            "io_uring_setup" => uring += 1,
            _ => {}
        }
    }

    let mut s = format!("## Run {label}: what strace saw\n\n");
    let pids: BTreeSet<u32> = calls.iter().map(|c| c.pid).collect();
    s.push_str(&format!(
        "{} calls from {} processes and threads. Paths are where an open landed (`-yy`, links followed), else the \
         argument joined to its descriptor's folder or the cwd. \"by\" = the program the process was running.\n\n",
        calls.len(),
        pids.len()
    ));
    if uring > 0 {
        s.push_str(&format!(
            "**io_uring: set up {uring} times** — file work done through it doesn't show here.\n\n"
        ));
    }
    let tables = [
        (
            Kind::Write,
            "Written outside the project",
            "opened for writing, made, removed, moved (both ends), linked, chmod/chown, times",
        ),
        (
            Kind::Open,
            "Opened for reading outside the project",
            "files and folders' lists, programs run — what Landlock's read rights govern",
        ),
        (
            Kind::Look,
            "Only looked at outside the project",
            "stat, access, readlink, `O_PATH`, chdir — what Landlock doesn't govern",
        ),
        (Kind::Watch, "Watched (inotify) outside the project", ""),
    ];
    for (kind, title, what) in tables {
        let mut folders: BTreeMap<String, Folder> = BTreeMap::new();
        for a in accesses.iter().filter(|a| a.kind == kind && !inside(&a.path, project)) {
            let (folder, name) = folder_of(&a.path, home);
            let f = folders.entry(folder).or_default();
            match a.result.as_str() {
                "ok" => f.ok += 1,
                e => *f.failed.entry(e.to_string()).or_default() += 1,
            }
            f.calls.insert(a.call.clone());
            f.by.insert(a.by.clone());
            f.names.insert(name);
        }
        s.push_str(&format!("### {title}\n\n"));
        if !what.is_empty() {
            s.push_str(&format!("({what})\n\n"));
        }
        if folders.is_empty() {
            s.push_str("none\n\n");
            continue;
        }
        s.push_str("| folder | ok | failed | calls | by | names |\n|---|---|---|---|---|---|\n");
        for (folder, f) in &folders {
            let failed: Vec<String> = f.failed.iter().map(|(e, n)| format!("{n} {e}")).collect();
            let names: Vec<&str> = f.names.iter().map(String::as_str).take(8).collect();
            let more = if f.names.len() > 8 { format!(" … +{}", f.names.len() - 8) } else { String::new() };
            s.push_str(&format!(
                "| `{}` | {} | {} | {} | {} | {}{} |\n",
                cell(folder),
                f.ok,
                failed.join(", "),
                f.calls.iter().cloned().collect::<Vec<_>>().join(" "),
                f.by.iter().cloned().collect::<Vec<_>>().join(" "),
                cell(&names.join(", ")),
                more
            ));
        }
        s.push('\n');
    }

    // Claude's own state, path by path: where `.claude.json`, its credentials and its project state were sought
    // (its calls, their results, how many)
    type Seen = (BTreeSet<String>, BTreeSet<String>, usize);
    let mut state: BTreeMap<(String, Kind), Seen> = BTreeMap::new();
    for a in &accesses {
        let p = Path::new(&a.path);
        let named_claude = p.components().any(|c| c.as_os_str().to_string_lossy().contains("claude"));
        if !named_claude || a.path.contains("/versions/") || inside(&a.path, project) {
            continue;
        }
        let e = state.entry((a.path.clone(), a.kind)).or_default();
        e.0.insert(a.call.clone());
        e.1.insert(a.result.clone());
        e.2 += 1;
    }
    s.push_str("### Its own state (paths naming `claude`, the program's own files left out)\n\n");
    if state.is_empty() {
        s.push_str("none\n\n");
    } else {
        s.push_str("| path | what | calls | results | n |\n|---|---|---|---|---|\n");
        for ((path, kind), (calls, results, n)) in &state {
            let shown = path.strip_prefix(home).map_or(path.clone(), |p| format!("~{p}"));
            let join = |s: &BTreeSet<String>| s.iter().cloned().collect::<Vec<_>>().join(" ");
            s.push_str(&format!(
                "| `{}` | {kind:?} | {} | {} | {n} |\n",
                cell(&shown),
                join(calls),
                join(results)
            ));
        }
        s.push('\n');
    }

    for (title, lines) in [("Renames", &renames), ("Links", &links)] {
        s.push_str(&format!("### {title}\n\n"));
        if lines.is_empty() {
            s.push_str("none\n");
        }
        for l in lines {
            s.push_str(l);
            s.push('\n');
        }
        s.push('\n');
    }

    s.push_str("### Processes started\n\n");
    if procs.is_empty() {
        s.push_str("none\n\n");
    } else {
        s.push_str("| pid | by | program | argv | result |\n|---|---|---|---|---|\n");
        for p in &procs {
            s.push_str(p);
            s.push('\n');
        }
        s.push('\n');
    }

    s.push_str("### Sockets\n\n");
    let made: Vec<String> = families.iter().map(|(f, n)| format!("{f} ×{n}")).collect();
    s.push_str(&format!("socket(): {}\n\n", if made.is_empty() { "none".into() } else { made.join(" · ") }));
    if !sockets.is_empty() {
        s.push_str("| call | address | result | n | by |\n|---|---|---|---|---|\n");
        for ((call, addr, result), (n, by)) in &sockets {
            let by = by.iter().cloned().collect::<Vec<_>>().join(" ");
            s.push_str(&format!("| {call} | `{}` | {result} | {n} | {by} |\n", cell(addr)));
        }
        s.push('\n');
    }

    s.push_str("### In the project\n\n");
    let in_project: Vec<&Access> = accesses.iter().filter(|a| inside(&a.path, project)).collect();
    let mut written: BTreeMap<String, (BTreeSet<String>, BTreeSet<String>)> = BTreeMap::new();
    for a in in_project.iter().filter(|a| a.kind == Kind::Write) {
        let rel = a.path.strip_prefix(project).unwrap_or(&a.path).trim_start_matches('/');
        let e = written.entry(rel.to_string()).or_default();
        e.0.insert(format!("{} {}", a.call, a.result));
        e.1.insert(a.by.clone());
    }
    let count = |k: Kind, git: bool| {
        in_project.iter().filter(|a| a.kind == k && a.path.contains("/.git") == git).count()
    };
    s.push_str(&format!(
        "opened for reading: {} (in `.git`: {}) · only looked at: {} (in `.git`: {})\n\n",
        count(Kind::Open, false),
        count(Kind::Open, true),
        count(Kind::Look, false),
        count(Kind::Look, true)
    ));
    for (rel, (calls, by)) in &written {
        let calls = calls.iter().cloned().collect::<Vec<_>>().join(", ");
        let by = by.iter().cloned().collect::<Vec<_>>().join(" ");
        s.push_str(&format!("- written `{rel}` — {calls} — by {by}\n"));
    }
    s.push('\n');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stub(project: &str) -> (Stub, Arc<Mutex<Vec<u8>>>) {
        struct Shared(Arc<Mutex<Vec<u8>>>);
        impl Write for Shared {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().write(b)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let log = Arc::new(Mutex::new(Vec::new()));
        (Stub::new(project.into(), Box::new(Shared(log.clone()))), log)
    }

    /// A Messages request as claude makes one: its tools, and the conversation so far.
    fn request(tools: &[&str], messages: Value, stream: bool) -> Vec<u8> {
        let tools: Vec<Value> = tools.iter().map(|t| json!({ "name": t, "input_schema": {} })).collect();
        json!({ "model": "claude-x", "stream": stream, "tools": tools, "messages": messages, "max_tokens": 32000 })
            .to_string()
            .into_bytes()
    }

    /// The fake client: one HTTP request over a real socket, the reply's status and body.
    fn post(port: u16, path: &str, body: &[u8], chunked: bool) -> (u16, String) {
        let mut conn = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let mut head =
            format!("POST {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\n");
        if chunked {
            head.push_str("Transfer-Encoding: chunked\r\n\r\n");
            conn.write_all(head.as_bytes()).unwrap();
            for part in body.chunks(100) {
                conn.write_all(format!("{:x}\r\n", part.len()).as_bytes()).unwrap();
                conn.write_all(part).unwrap();
                conn.write_all(b"\r\n").unwrap();
            }
            conn.write_all(b"0\r\n\r\n").unwrap();
        } else {
            head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
            conn.write_all(head.as_bytes()).unwrap();
            conn.write_all(body).unwrap();
        }
        let mut reply = String::new();
        conn.read_to_string(&mut reply).unwrap();
        let (head, body) = reply.split_once("\r\n\r\n").unwrap();
        let status = head.split_whitespace().nth(1).unwrap().parse().unwrap();
        let length: usize = head
            .lines()
            .find_map(|l| l.strip_prefix("Content-Length: "))
            .and_then(|n| n.parse().ok())
            .unwrap();
        assert_eq!(length, body.len(), "the reply's length is its body's");
        (status, body.to_string())
    }

    /// A stream read as claude's client reads it: the events in order, and the blocks they build.
    fn read_stream(body: &str) -> (Vec<String>, Vec<Value>, String) {
        let (mut events, mut blocks, mut stop) = (Vec::new(), Vec::<Value>::new(), String::new());
        let mut partial: HashMap<usize, String> = HashMap::new();
        for event in body.split("\n\n").filter(|e| !e.is_empty()) {
            let name = event.lines().find_map(|l| l.strip_prefix("event: ")).unwrap();
            let data: Value =
                serde_json::from_str(event.lines().find_map(|l| l.strip_prefix("data: ")).unwrap()).unwrap();
            assert_eq!(data["type"], name, "an event's name is its data's type");
            events.push(name.to_string());
            let i = data["index"].as_u64().unwrap_or(0) as usize;
            match name {
                "message_start" => {
                    assert_eq!(data["message"]["role"], "assistant");
                    assert!(data["message"]["stop_reason"].is_null());
                }
                "content_block_start" => {
                    assert_eq!(i, blocks.len(), "blocks come in order");
                    blocks.push(data["content_block"].clone());
                }
                "content_block_delta" => match data["delta"]["type"].as_str().unwrap() {
                    "text_delta" => {
                        let t = blocks[i]["text"].as_str().unwrap().to_string();
                        blocks[i]["text"] = Value::from(t + data["delta"]["text"].as_str().unwrap());
                    }
                    "input_json_delta" => partial
                        .entry(i)
                        .or_default()
                        .push_str(data["delta"]["partial_json"].as_str().unwrap()),
                    other => panic!("delta {other}"),
                },
                "content_block_stop" => {
                    if let Some(json) = partial.remove(&i) {
                        blocks[i]["input"] = serde_json::from_str(&json).unwrap();
                    }
                }
                "message_delta" => stop = data["delta"]["stop_reason"].as_str().unwrap().to_string(),
                _ => {}
            }
        }
        (events, blocks, stop)
    }

    #[test]
    fn the_stub_streams_the_scripted_conversation_to_a_client_over_http() {
        let (stub, log) = stub("/work/proj");
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let stub = Arc::new(stub);
        std::thread::spawn(move || stub.serve_on(listener));

        // The conversation as claude grows it: each tool use, then its result
        let mut messages = vec![json!({ "role": "user", "content": "go" })];
        let mut seen = Vec::new();
        for turn in 0..5 {
            // (the second request's body comes in chunks: the stub reads both framings)
            let body = request(&["Read", "Edit", "Write"], Value::from(messages.clone()), true);
            let (status, reply) = post(port, "/v1/messages?beta=true", &body, turn == 1);
            assert_eq!(status, 200);
            let (events, blocks, stop) = read_stream(&reply);
            assert_eq!(events.first().map(String::as_str), Some("message_start"));
            assert_eq!(&events[events.len() - 2..], ["message_delta", "message_stop"]);
            assert_eq!(blocks.len(), 1);
            let block = &blocks[0];
            if turn == 4 {
                assert_eq!((stop.as_str(), block["text"].as_str()), ("end_turn", Some(DONE)));
                break;
            }
            assert_eq!(stop, "tool_use");
            assert_eq!(block["type"], "tool_use");
            seen.push((block["name"].as_str().unwrap().to_string(), block["input"]["file_path"].clone()));
            let id = block["id"].clone();
            messages.push(json!({ "role": "assistant", "content": [block] }));
            messages.push(json!({ "role": "user", "content": [
                { "type": "tool_result", "tool_use_id": id, "content": format!("result {turn}") }] }));
        }
        let seen: Vec<(&str, &str)> = seen.iter().map(|(n, p)| (n.as_str(), p.as_str().unwrap())).collect();
        assert_eq!(
            seen,
            [
                ("Read", "/work/proj/hello.txt"),
                ("Edit", "/work/proj/hello.txt"),
                ("Write", "/work/proj/written.txt"),
                ("Read", OUTSIDE)
            ]
        );

        // A side request (no Read among its tools) gets a short text, streamed or whole
        let (_, reply) = post(port, "/v1/messages", &request(&[], json!([]), true), false);
        assert_eq!(read_stream(&reply).1, [text("ok")]);
        let (_, whole) = post(port, "/v1/messages", &request(&[], json!([]), false), false);
        let whole: Value = serde_json::from_str(&whole).unwrap();
        assert_eq!(
            (whole["content"][0]["text"].as_str(), whole["stop_reason"].as_str()),
            (Some("ok"), Some("end_turn"))
        );
        // What isn't the Messages API: a 404 the log names
        assert_eq!(post(port, "/api/somewhere", b"{}", false).0, 404);

        let log = String::from_utf8(log.lock().unwrap().clone()).unwrap();
        assert!(
            log.contains(
                "turn 1, tools Read,Edit,Write · ends user[string] assistant[tool_use] user[tool_result] · last tool \
                 result: result 0 → Edit"
            ),
            "{log}"
        );
        assert!(log.contains("side request (0 tools)"), "{log}");
        assert!(log.contains("POST /api/somewhere") && log.contains("→ 404"), "{log}");
    }

    /// The `enforce` script: its steps after A's and B's, each result judged, and the markers in every chat request.
    #[test]
    fn the_enforce_script_judges_each_result_and_the_markers() {
        let (mut stub, log) = stub("/work/proj");
        let enforce =
            Enforce { home: "/home/m".into(), absent: "cfg-marker".into(), present: "proj-marker".into() };
        stub.enforce = Some(enforce);
        // (results as claude's tools would give them: the new file and the key refused, the Write over .bashrc
        // "ok" — the profile failing — each with something after it, as claude 2.1.296 sends: a later message, or a
        // text in the same one)
        let results = [
            ("ok", false),
            ("ok", false),
            ("EACCES: permission denied, open '/work/proj/written.txt.tmp.1'", true),
            ("ok", false),
            ("EACCES: permission denied, open '/home/m/.ssh/id_ed25519'", true),
            ("# rc", false),
            ("ok", false),
        ];
        let mut messages = vec![json!({ "role": "user", "content": "go proj-marker" })];
        let mut names = Vec::new();
        let reminder = json!({ "type": "text", "text": "<system-reminder>a note</system-reminder>" });
        for (turn, (said, error)) in results.iter().enumerate() {
            let body = json!({ "tools": [{ "name": "Read" }], "messages": messages.clone() });
            let (blocks, stop, _) = stub.turn(&body);
            assert_eq!(stop, "tool_use", "turn {turn}");
            names.push(format!(
                "{} {}",
                blocks[0]["name"].as_str().unwrap(),
                blocks[0]["input"]["file_path"]
            ));
            let id = blocks[0]["id"].clone();
            messages.push(json!({ "role": "assistant", "content": blocks }));
            let result =
                json!({ "type": "tool_result", "tool_use_id": id, "content": said, "is_error": error });
            match turn % 2 {
                0 => messages.extend([
                    json!({ "role": "user", "content": [result] }),
                    json!({ "role": "user", "content": [reminder.clone()] }),
                ]),
                _ => messages.push(json!({ "role": "user", "content": [result, reminder.clone()] })),
            }
        }
        // (the last turn: the marker of claude's folder shows up — judged a failure)
        let mut last = messages.clone();
        last[0] = json!({ "role": "user", "content": "go proj-marker cfg-marker" });
        let (_, stop, what) = stub.turn(&json!({ "tools": [{ "name": "Read" }], "messages": last }));
        assert_eq!((stop, what.ends_with("the turn's end")), ("end_turn", true), "{what}");
        assert!(
            what.contains("· ends assistant[tool_use] user[tool_result] user[text] · last tool result: ok"),
            "{what}"
        );
        // (a request at turn 3 whose step-3 result is gone: never judged by what else is there)
        let gone: Vec<Value> = messages[..7]
            .iter()
            .chain([&json!({ "role": "user", "content": [reminder] })])
            .cloned()
            .collect();
        stub.turn(&json!({ "tools": [{ "name": "Read" }], "messages": gone }));
        assert_eq!(
            &names[4..],
            ["Read \"/home/m/.ssh/id_ed25519\"", "Read \"/home/m/.bashrc\"", "Write \"/home/m/.bashrc\""]
        );
        let log = String::from_utf8(log.lock().unwrap().clone()).unwrap();
        let checks: Vec<&str> = log.lines().filter(|l| l.starts_with("check step")).collect();
        let verdicts: Vec<bool> = checks.iter().map(|l| l.contains(" PASS — ")).collect();
        assert_eq!(verdicts, [true, true, true, true, true, true, false, false], "{log}");
        assert!(checks[1].contains("Edit /work/proj/hello.txt (a file there, in the project's top: claude 2.1.296 falls back to writing in place): Done PASS"), "{log}");
        assert!(checks[2].contains("(a new file in the project's top: N1"), "{log}");
        assert!(checks[6].contains("Write /home/m/.bashrc (home isn't written): Fails FAIL — ok"), "{log}");
        assert!(
            checks[7].starts_with("check step 3 Write") && checks[7].ends_with("FAIL — no tool result"),
            "{log}"
        );
        assert_eq!(log.matches("the project's CLAUDE.md in the request: PASS").count(), 9, "{log}");
        assert_eq!(log.matches("claude's folder's CLAUDE.md not in the request: PASS").count(), 8, "{log}");
        assert!(log.contains("check turn 7 claude's folder's CLAUDE.md not in the request: FAIL"), "{log}");
    }

    #[test]
    fn a_gzipped_request_is_read() {
        let body = request(&["Read"], json!([{ "role": "user", "content": "go" }]), false);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(&body).unwrap();
        let gz = gz.finish().unwrap();
        let mut raw = format!(
            "POST /v1/messages HTTP/1.1\r\nContent-Encoding: gzip\r\nContent-Length: {}\r\n\r\n",
            gz.len()
        )
        .into_bytes();
        raw.extend(gz);
        let req = read_request(&mut raw.as_slice(), &mut std::io::sink()).unwrap();
        assert_eq!(req.body, body);
        let (stub, _) = stub("/p");
        let reply: Value = serde_json::from_str(&stub.answer(&req).body).unwrap();
        assert_eq!(reply["content"][0]["name"], "Read");
    }

    /// The driver against a fake claude in `sh` (`read -r`): what it sends, what it answers, when it closes.
    #[test]
    fn the_driver_answers_permission_prompts_and_mcp_and_ends_at_the_result() {
        let dir = std::env::temp_dir().join(format!("tarae-claudestub-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let heard = dir.join("heard");
        let script = r#"
read -r init; printf '%s\n' "$init" >> "$1"
read -r user; printf '%s\n' "$user" >> "$1"
echo '{"type":"system","subtype":"init"}'
echo '{"type":"control_request","request_id":"r1","request":{"subtype":"can_use_tool","tool_name":"Edit","input":{"file_path":"/p/a.txt","old_string":"a","new_string":"b"}}}'
read -r answer; printf '%s\n' "$answer" >> "$1"
echo '{"type":"control_request","request_id":"r2","request":{"subtype":"mcp_message","server_name":"tarae","message":{"jsonrpc":"2.0","id":7,"method":"tools/list"}}}'
read -r answer; printf '%s\n' "$answer" >> "$1"
echo '{"type":"result","subtype":"success","is_error":false,"num_turns":3}'
if read -r more; then echo "still open: $more" >> "$1"; else echo eof >> "$1"; fi
pwd >> "$1"
"#;
        let command: Vec<String> =
            ["sh", "-c", script, "sh", heard.to_str().unwrap()].iter().map(|s| s.to_string()).collect();
        let transcript = dir.join("transcript");
        let driven =
            drive_command(&command, Some(&dir), "hello", &transcript, Duration::from_secs(20)).unwrap();

        let heard = std::fs::read_to_string(&heard).unwrap();
        let lines: Vec<&str> = heard.lines().collect();
        let line = |i: usize| serde_json::from_str::<Value>(lines[i]).unwrap();
        assert_eq!(line(0)["request"]["sdkMcpServers"], json!(["tarae"]));
        assert_eq!(line(1)["message"]["content"], "hello");
        let allow = &line(2)["response"];
        assert_eq!((allow["request_id"].as_str(), allow["subtype"].as_str()), (Some("r1"), Some("success")));
        assert_eq!(allow["response"]["behavior"], "allow");
        assert_eq!(allow["response"]["updatedInput"]["new_string"], "b");
        let listed = &line(3)["response"]["response"]["mcp_response"];
        assert_eq!((listed["id"].as_i64(), &listed["result"]["tools"]), (Some(7), &json!([])));
        assert_eq!(lines[4], "eof", "claude's stdin closes at its result");
        let real = |p: &Path| p.canonicalize().unwrap();
        assert_eq!(real(Path::new(lines[5])), real(&dir), "it runs in --cwd");

        assert_eq!(driven.asked, ["Edit /p/a.txt"]);
        assert_eq!(driven.mcp, ["tools/list"]);
        assert_eq!(driven.result.as_deref(), Some("success · is_error false · 3 turns"));
        assert!(!driven.timed_out);
        let transcript = std::fs::read_to_string(&transcript).unwrap();
        assert!(
            transcript.contains("< {\"type\":\"result\"") && transcript.starts_with("> "),
            "{transcript}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_driven_command_that_hangs_is_killed_at_the_timeout() {
        let command: Vec<String> = ["sh", "-c", "sleep 30"].iter().map(|s| s.to_string()).collect();
        let transcript = std::env::temp_dir().join(format!("tarae-claudestub-hang-{}", std::process::id()));
        let driven = drive_command(&command, None, "x", &transcript, Duration::from_millis(300)).unwrap();
        assert!(driven.timed_out && driven.result.is_none(), "{driven}");
        let _ = std::fs::remove_file(transcript);
    }

    const LOG: &str = r#"100   10:00:00.000001 openat(AT_FDCWD, "/home/m/.claude.json", O_RDONLY|O_CLOEXEC) = 3</home/m/.claude.json>
100   10:00:00.000002 openat(AT_FDCWD, "/home/m/.claude.json.tmp.100.17", O_WRONLY|O_CREAT|O_TRUNC|O_CLOEXEC, 0600) = 4</home/m/.claude.json.tmp.100.17>
100   10:00:00.000003 rename("/home/m/.claude.json.tmp.100.17", "/home/m/.claude.json") = 0
100   10:00:00.000004 mkdirat(5</home/m/.claude>, "projects", 0755 <unfinished ...>
101   10:00:00.000005 statx(AT_FDCWD, "hello.txt", AT_STATX_SYNC_AS_STAT, STATX_ALL, {stx_mask=STATX_ALL}) = 0
100   10:00:00.000006 <... mkdirat resumed>) = -1 EEXIST (File exists)
100   10:00:00.000007 openat(AT_FDCWD, "/home/m/.claude/.credentials.json", O_RDONLY|O_CLOEXEC) = -1 ENOENT (No such file or directory)
100   10:00:00.000008 clone3({flags=CLONE_VM|CLONE_VFORK, exit_signal=SIGCHLD, stack=0x1, stack_size=0x2}, 88) = 102
102   10:00:00.000009 execve("/usr/bin/git", ["git", "status", "--porcelain"], 0x7ff /* 20 vars */) = 0
102   10:00:00.000010 openat(AT_FDCWD, "/home/m/.gitconfig", O_RDONLY) = -1 ENOENT (No such file or directory)
102   10:00:00.000011 chdir("sub") = 0
102   10:00:00.000012 openat(AT_FDCWD, "x", O_RDONLY) = -1 ENOENT (No such file or directory)
100   10:00:00.000013 socket(AF_UNIX, SOCK_STREAM|SOCK_CLOEXEC, 0) = 6<UNIX:[999]>
100   10:00:00.000014 bind(6<UNIX:[999]>, {sa_family=AF_UNIX, sun_path="/tmp/cc-socks/a.sock"}, 110) = 0
100   10:00:00.000015 connect(7<TCP:[1]>, {sa_family=AF_INET, sin_port=htons(443), sin_addr=inet_addr("160.79.104.10")}, 16) = -1 ENETUNREACH (Network is unreachable)
100   10:00:00.000016 openat(AT_FDCWD, "/proc/100/status", O_RDONLY) = 8</proc/100/status>
100   10:00:00.000017 openat(AT_FDCWD, "/work/p/written.txt", O_WRONLY|O_CREAT|O_TRUNC, 0644) = 9</work/p/written.txt>
100   10:00:00.000018 symlinkat("/x/target", 5</home/m/.claude>, "link") = 0
100   10:00:00.000019 +++ exited with 0 +++
"#;

    #[test]
    fn strace_lines_join_their_halves_and_split_their_arguments() {
        let calls = calls(LOG);
        let mkdir = calls.iter().find(|c| c.name == "mkdirat").unwrap();
        assert_eq!(
            (mkdir.pid, mkdir.args.as_slice(), mkdir.result().as_str()),
            (100, &["5</home/m/.claude>".to_string(), "\"projects\"".into(), "0755".into()][..], "EEXIST")
        );
        let clone = calls.iter().find(|c| c.name == "clone3").unwrap();
        assert_eq!((clone.args.len(), clone.ret.as_str()), (2, "102"));
        assert_eq!(unquote(r#""a\"b\x41\101\n""#).as_deref(), Some("a\"bAA\n"));
        assert_eq!(calls.len(), 17);
    }

    #[test]
    fn the_summary_sorts_what_was_touched_by_folder_and_program() {
        let s = summary("T", LOG, "/work/p", "/home/m");
        let section = |title: &str| {
            let at = s.find(&format!("### {title}")).unwrap_or_else(|| panic!("no {title} in {s}"));
            let rest = &s[at + 4..];
            rest[..rest.find("\n### ").unwrap_or(rest.len())].to_string()
        };
        let written = section("Written outside");
        // (the rename's two ends both count)
        assert!(
            written
                .contains("| `~` | 3 |  | openat rename | claude | .claude.json, .claude.json.tmp.100.17 |"),
            "{written}"
        );
        assert!(written.contains("`~/.claude` | 1 | 1 EEXIST | mkdirat symlinkat |"), "{written}");
        let opened = section("Opened for reading");
        // (git's own read, and its relative open after its chdir, by git; /proc's pid as <n>)
        assert!(
            opened.contains("| `~` | 1 | 1 ENOENT | openat | claude git | .claude.json, .gitconfig |"),
            "{opened}"
        );
        assert!(opened.contains("`/proc/<n>` | 1 |"), "{opened}");
        assert!(opened.contains("`/usr/bin` | 1 |  | execve | claude | git |"), "{opened}");
        assert!(!opened.contains("/work/p"), "{opened}");
        let state = section("Its own state");
        assert!(state.contains("| `~/.claude/.credentials.json` | Open | openat | ENOENT | 1 |"), "{state}");
        assert!(
            section("Renames").contains(
                "- `/home/m/.claude.json.tmp.100.17` → `/home/m/.claude.json` — rename by claude: ok"
            )
        );
        assert!(section("Links").contains("`/home/m/.claude/link` → (symlink to) `/x/target`"));
        assert!(
            section("Processes").contains(
                "| 102 | claude | `/usr/bin/git` | `[\"git\", \"status\", \"--porcelain\"]` | ok |"
            )
        );
        let sockets = section("Sockets");
        assert!(sockets.contains("AF_UNIX SOCK_STREAM ×1"), "{sockets}");
        assert!(sockets.contains("| bind | `unix:/tmp/cc-socks/a.sock` | ok | 1 | claude |"), "{sockets}");
        assert!(
            sockets.contains("| connect | `160.79.104.10:443` | ENETUNREACH | 1 | claude |"),
            "{sockets}"
        );
        let project = section("In the project");
        assert!(project.contains("- written `written.txt` — openat ok — by claude"), "{project}");
        // (statx and the chdir; git's `x` after `chdir sub` is in the project's `sub`, not elsewhere)
        assert!(project.contains("opened for reading: 1 (in `.git`: 0) · only looked at: 2"), "{project}");
    }
}
