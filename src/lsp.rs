//! LSP client — **fully asynchronous** (principle 4: input and rendering never block on LSP).
//!
//! Three threads per server: writer (channel → stdin), reader (stdout → main-loop events), stderr drain.
//! The main thread only pushes to channels — editing never stalls even if a busy server stops reading stdin.
//! Big bodies (didOpen, full sync) are serialized on the writer thread too (we pass an O(1) rope clone).
//! Asks for UTF-8 positions first — editor positions are bytes, no conversion. UTF-16-only servers also work.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::thread;

use ropey::Rope;
use serde_json::{Value, json};

use crate::editor::Editor;
use crate::event::Event;
use crate::movement as mv;

pub type ClientId = usize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServerSpec {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// Features it isn't asked for (`[lsp.<name>] except`, names in `FEATURES`) — so, say, only one of two
    /// servers formats.
    pub except: Vec<String>,
}

/// What `[lsp.<name>] except` can turn off.
pub const FEATURES: &[&str] = &[
    "diagnostics",
    "format",
    "completion",
    "hover",
    "code-action",
    "rename",
    "goto",
    "signature-help",
    "inlay-hints",
    "symbols",
    "highlights",
];

/// (name, command, args)
type Candidate = (&'static str, &'static str, &'static [&'static str]);

/// Servers that run beside a language's main one (each if on PATH) — linters and formatters with their own
/// server. Overridden by `[lang.<lang>] also`.
const DEFAULT_ALSO: &[(&str, &[Candidate])] = &[("python", &[("ruff", "ruff", &["server"])])];

/// Default servers per language — the first on PATH wins. Overridden by `[lsp.<name>]`·`[lang.<lang>] lsp`.
const DEFAULTS: &[(&str, &[Candidate])] = &[
    ("rust", &[("rust-analyzer", "rust-analyzer", &[])]),
    ("go", &[("gopls", "gopls", &[])]),
    (
        "python",
        &[
            ("basedpyright", "basedpyright-langserver", &["--stdio"]),
            ("pyright", "pyright-langserver", &["--stdio"]),
            ("pylsp", "pylsp", &[]),
        ],
    ),
    ("typescript", &[("typescript-language-server", "typescript-language-server", &["--stdio"])]),
    ("tsx", &[("typescript-language-server", "typescript-language-server", &["--stdio"])]),
    ("javascript", &[("typescript-language-server", "typescript-language-server", &["--stdio"])]),
    ("jsx", &[("typescript-language-server", "typescript-language-server", &["--stdio"])]),
    ("c", &[("clangd", "clangd", &[])]),
    ("cpp", &[("clangd", "clangd", &[])]),
    ("lua", &[("lua-language-server", "lua-language-server", &[])]),
    ("zig", &[("zls", "zls", &[])]),
    ("bash", &[("bash-language-server", "bash-language-server", &["start"])]),
    ("toml", &[("taplo", "taplo", &["lsp", "stdio"])]),
    ("yaml", &[("yaml-language-server", "yaml-language-server", &["--stdio"])]),
    ("helm", &[("helm_ls", "helm_ls", &["serve"])]),
    ("markdown", &[("marksman", "marksman", &["server"])]),
    ("nix", &[("nil", "nil", &[])]),
    // jdtls workspace dir and debug plugin are added by java.rs
    ("java", &[("jdtls", "jdtls", &[])]),
];

/// Language id (LSP languageId) — mostly the language name as-is.
pub fn language_id(lang: &str) -> &str {
    match lang {
        "tsx" => "typescriptreact",
        "jsx" => "javascriptreact",
        "bash" => "shellscript",
        other => other,
    }
}

/// Servers for this language: the configured list if any, else the first default candidate on PATH — or,
/// failing that, one installed where its pack says it lands off `PATH` (`go install` → `~/go/bin`), by its
/// full path.
pub fn find_server(lang: &str, config: &LspConfig) -> Option<ServerSpec> {
    let all = candidates(lang, config);
    if let Some(s) = all.iter().find(|s| on_path(&s.command)) {
        return Some(s.clone());
    }
    all.into_iter().filter(|s| !s.command.contains('/')).find_map(|mut s| {
        s.command = crate::packs::installed_at(&s.command)?.display().to_string();
        Some(s)
    })
}

/// The servers configured for `lang` (or the defaults), installed or not.
pub fn candidates(lang: &str, config: &LspConfig) -> Vec<ServerSpec> {
    let names: Vec<&str> = match config.languages.get(lang) {
        Some(names) => names.iter().map(String::as_str).collect(),
        None => DEFAULTS
            .iter()
            .find(|(l, _)| *l == lang)
            .map(|(_, list)| list.iter().map(|(name, ..)| *name).collect())
            .unwrap_or_default(),
    };
    names.into_iter().filter_map(|n| config.servers.get(n).cloned().or_else(|| default_spec(n))).collect()
}

/// The servers that start beside the main one for `lang` (`[lang.<lang>] also`, else the defaults): those on
/// PATH, and the names of those that aren't (for the log).
pub fn also_servers(lang: &str, config: &LspConfig) -> (Vec<ServerSpec>, Vec<String>) {
    let names: Vec<&str> = match config.also.get(lang) {
        Some(names) => names.iter().map(String::as_str).collect(),
        None => DEFAULT_ALSO
            .iter()
            .find(|(l, _)| *l == lang)
            .map(|(_, list)| list.iter().map(|(name, ..)| *name).collect())
            .unwrap_or_default(),
    };
    let (on, off): (Vec<ServerSpec>, Vec<ServerSpec>) = names
        .into_iter()
        .filter_map(|n| config.servers.get(n).cloned().or_else(|| default_spec(n)))
        .partition(|s| on_path(&s.command));
    (on, off.into_iter().map(|s| s.name).collect())
}

/// A built-in server by name (a main one or one that runs beside).
pub fn default_spec(name: &str) -> Option<ServerSpec> {
    DEFAULTS.iter().chain(DEFAULT_ALSO).flat_map(|(_, l)| l.iter()).find(|(n, _, _)| *n == name).map(
        |(n, c, a)| ServerSpec {
            name: n.to_string(),
            command: c.to_string(),
            args: a.iter().map(|s| s.to_string()).collect(),
            except: Vec::new(),
        },
    )
}

fn on_path(cmd: &str) -> bool {
    if cmd.contains('/') {
        return Path::new(cmd).is_file();
    }
    std::env::var_os("PATH").is_some_and(|p| std::env::split_paths(&p).any(|d| d.join(cmd).is_file()))
}

/// Config `[lsp.<name>] command/args` + `[lang.<lang>] lsp = ["name", …]`.
/// One inlay hint — inserted as dim text before the character at `pos` (text includes padding).
#[derive(Clone, Debug, PartialEq)]
pub struct InlayHint {
    pub pos: usize,
    pub text: String,
}

/// `InlayHint[]` response → hints in position order. label is a string or parts (`{value}`).
pub fn inlay_hints(text: &Rope, v: &Value, enc: Encoding) -> Vec<InlayHint> {
    let mut out: Vec<InlayHint> = v
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|h| {
            let pos = from_position(text, &h["position"], enc)?;
            let label = match &h["label"] {
                Value::String(s) => s.clone(),
                Value::Array(parts) => parts.iter().filter_map(|p| p["value"].as_str()).collect(),
                _ => return None,
            };
            let label = label.replace(['\n', '\r'], " ");
            let pad = |k: &str| if h[k].as_bool() == Some(true) { " " } else { "" };
            Some(InlayHint { pos, text: format!("{}{label}{}", pad("paddingLeft"), pad("paddingRight")) })
        })
        .collect();
    out.sort_by_key(|h| h.pos);
    out
}

#[derive(Clone, Debug, Default)]
pub struct LspConfig {
    pub enabled: bool,
    pub servers: HashMap<String, ServerSpec>,
    /// `[lang.<lang>] lsp` — the main server's candidates, the first on PATH starts.
    pub languages: HashMap<String, Vec<String>>,
    /// `[lang.<lang>] also` — servers that start beside it.
    pub also: HashMap<String, Vec<String>>,
}

/// Project markers of any language (`.git` is handled by `root_by_markers`).
const MARKERS: &[&str] = &[
    "Cargo.toml",
    "go.mod",
    "package.json",
    "pyproject.toml",
    "compile_commands.json",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
];

/// Markers that make a project root for this language — other languages' don't count (a Python file under
/// a repo-root package.json). Unknown languages take any marker.
fn markers_for(lang: &str) -> &'static [&'static str] {
    match lang {
        "rust" => &["Cargo.toml"],
        "go" => &["go.work", "go.mod"],
        "python" => &["pyproject.toml", "setup.py", "setup.cfg"],
        "typescript" | "tsx" | "javascript" | "jsx" => &["package.json", "tsconfig.json", "jsconfig.json"],
        "c" | "cpp" => &["compile_commands.json", "CMakeLists.txt"],
        "java" => crate::java::BUILD_FILES,
        _ => MARKERS,
    }
}

/// Languages whose server can't work without a project, and the markers that make one: rust-analyzer in a
/// lone `.rs` finds no workspace ("Failed to discover workspace" on the status line at every open) — so a
/// file with none of these above it gets no server.
pub fn project_markers(lang: &str) -> Option<&'static [&'static str]> {
    match lang {
        "rust" => Some(&["Cargo.toml", "rust-project.json"]),
        _ => None,
    }
}

/// The **topmost** dir with a marker, walking up but never past the repo root (`.git`) or into `$HOME` —
/// one server per workspace, not one per member crate/module (each is hundreds of MB). None if no marker.
pub fn root_by_markers(file: &Path, markers: &[&str]) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dir = file.parent()?.to_path_buf();
    let mut top = None;
    loop {
        if home.as_deref() == Some(dir.as_path()) {
            return top;
        }
        if markers.iter().any(|m| dir.join(m).is_file()) {
            top = Some(dir.clone());
        }
        if dir.join(".git").exists() || !dir.pop() {
            return top;
        }
    }
}

/// Nearest repo root (`.git`), else the file's dir.
fn repo_or_dir(file: &Path) -> PathBuf {
    let start = file.parent().unwrap_or(file);
    start.ancestors().find(|d| d.join(".git").exists()).unwrap_or(start).to_path_buf()
}

/// Project root by any language's markers (`root_by_markers`), else the repo root, else the file's dir.
pub fn find_root(file: &Path) -> PathBuf {
    root_by_markers(file, MARKERS).unwrap_or_else(|| repo_or_dir(file))
}

/// Per-language project root — the topmost of this language's markers below the repo root.
pub fn find_root_for(lang: &str, file: &Path) -> PathBuf {
    root_by_markers(file, markers_for(lang)).unwrap_or_else(|| repo_or_dir(file))
}

/// Folders where agents keep their worktrees — left out whole, worktrees made later included.
const WORKTREE_HOMES: &[&str] = &[".claude/worktrees"];
/// How deep `nested_checkouts` looks, and how many folders at most (a huge tree mustn't hold up the start).
const NESTED_DEPTH: usize = 4;
const NESTED_BUDGET: usize = 5000;

/// Checkouts inside `root` that aren't part of its project — a folder with its own `.git` (worktrees,
/// clones; not submodules, `.gitmodules` — those may be workspace members), and `WORKTREE_HOMES` — relative
/// to `root`. Measured on this repo: rust-analyzer scanned 40 GB of `.claude/worktrees` twice before it
/// could answer `gr` (222 s cold). Disk work: the server's writer thread.
pub fn nested_checkouts(root: &Path) -> Vec<String> {
    let submodules: Vec<String> = std::fs::read_to_string(root.join(".gitmodules"))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.trim().strip_prefix("path")?.trim_start().strip_prefix('='))
        .map(|p| p.trim().trim_end_matches('/').to_string())
        .collect();
    let mut found: Vec<String> =
        WORKTREE_HOMES.iter().filter(|h| root.join(h).is_dir()).map(|h| h.to_string()).collect();
    let mut queue = std::collections::VecDeque::from([(root.to_path_buf(), 0)]);
    let mut seen = 0;
    while let Some((dir, depth)) = queue.pop_front() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            // (not through links — a link out of the project isn't the project's to scan)
            if !e.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let path = e.path();
            let Some(rel) = path.strip_prefix(root).ok().map(|r| r.to_string_lossy().into_owned()) else {
                continue;
            };
            let name = e.file_name();
            if [".git", "target", "node_modules"].iter().any(|n| name == *n)
                || WORKTREE_HOMES.contains(&rel.as_str())
            {
                continue;
            }
            seen += 1;
            if seen > NESTED_BUDGET {
                return found;
            }
            if path.join(".git").exists() && !submodules.contains(&rel) {
                found.push(rel);
            } else if depth + 1 < NESTED_DEPTH {
                queue.push_back((path, depth + 1));
            }
        }
    }
    found
}

/// What a server is told at its start: `initializationOptions` (`given` — `java::prepare`'s — and more), and
/// the settings `workspace/configuration` is answered from (`section`).
pub struct Options {
    pub init: Value,
    pub config: Value,
}

/// Made on the server's writer thread: its pack is read and the project walked (disk work).
pub type MakeOptions = Box<dyn FnOnce() -> Options + Send>;

/// `given`, and the server's pack's `leave-out` (`packs::LeaveOut`) filled with `nested_checkouts` — so a
/// server that scans the workspace leaves worktrees and checkouts inside it alone, through its own setting.
/// The server is looked for by its name, else by the built-in one its command is (`[lsp.py] command =
/// "pyright-langserver"` is pyright).
pub fn options_for(spec: &ServerSpec, root: &Path, given: Value) -> MakeOptions {
    let mut names = vec![spec.name.clone()];
    let program = Path::new(&spec.command).file_name().map(|n| n.to_string_lossy().into_owned());
    let builtin = DEFAULTS.iter().chain(DEFAULT_ALSO).flat_map(|(_, l)| l.iter());
    names.extend(builtin.filter(|(_, c, _)| program.as_deref() == Some(*c)).map(|(n, ..)| n.to_string()));
    let (root, name) = (root.to_path_buf(), spec.name.clone());
    Box::new(move || {
        let mut o = Options { init: given, config: Value::Null };
        let Some(leave) = names.iter().find_map(|n| crate::packs::leave_out(n)) else { return o };
        let found = nested_checkouts(&root);
        if found.is_empty() {
            return o;
        }
        crate::log::line(&name, "info", &format!("left out: {}", found.join(", ")));
        let each = found.iter().flat_map(|rel| leave.each.iter().map(move |e| e.replace("{rel}", rel)));
        let list = Value::Array(leave.keep.iter().cloned().chain(each).map(Value::String).collect());
        match &leave.place {
            crate::packs::Place::Init(path) => put(&mut o.init, path, list),
            crate::packs::Place::Config(path) => put(&mut o.config, path, list),
        }
        o
    })
}

/// `x` at the dotted `path` in `v`, the objects on the way made (what's beside them kept).
fn put(v: &mut Value, path: &str, x: Value) {
    let mut at = v;
    for key in path.split('.') {
        if !at.is_object() {
            *at = json!({});
        }
        at = at.as_object_mut().expect("made an object above").entry(key).or_insert(Value::Null);
    }
    *at = x;
}

/// The settings under a `workspace/configuration` item's `section` (a dotted path; none = all of them).
pub fn section(config: &Value, section: Option<&str>) -> Value {
    let mut at = config;
    for key in section.into_iter().flat_map(|s| s.split('.')).filter(|k| !k.is_empty()) {
        at = &at[key];
    }
    at.clone()
}

// ── URI ──────────────────────────────────────────────────────────────────

pub fn uri(path: &Path) -> String {
    let mut s = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => s.push(b as char),
            _ => s.push_str(&format!("%{b:02X}")),
        }
    }
    s
}

pub fn path_from_uri(uri: &str) -> Option<PathBuf> {
    let rest = uri.strip_prefix("file://")?;
    Some(PathBuf::from(String::from_utf8(percent_decode(rest)).ok()?))
}

/// `%XX` escapes → bytes (a malformed escape stays as-is).
pub fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && let Some(v) = s.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(h, 16).ok())
        {
            out.push(v);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

// ── Position conversion ──────────────────────────────────────────────────

pub fn to_position(text: &Rope, byte: usize, enc: Encoding) -> Value {
    let byte = byte.min(text.len_bytes());
    let line = text.byte_to_line(byte);
    let ls = text.line_to_byte(line);
    let character = match enc {
        Encoding::Utf8 => byte - ls,
        Encoding::Utf16 => utf16_between(text, ls, byte),
    };
    json!({ "line": line, "character": character })
}

/// UTF-16 code units in `from..to` (char boundaries) — ropey counts them from its node totals and SIMD over
/// chunks, where a char-by-char sum read the line from its start for every position (a minified file's line).
pub fn utf16_between(text: &Rope, from: usize, to: usize) -> usize {
    text.char_to_utf16_cu(text.byte_to_char(to)) - text.char_to_utf16_cu(text.byte_to_char(from))
}

/// Server position → byte (column overflow → line end; line past the end → document end; mid-character →
/// the character's start in UTF-8, its end in UTF-16 — the unit count reached or passed).
pub fn from_position(text: &Rope, pos: &Value, enc: Encoding) -> Option<usize> {
    let line = pos["line"].as_u64()? as usize;
    let character = pos["character"].as_u64()? as usize;
    // `{line: lineCount, character: 0}` = end of document (whole-document edits end there)
    if line >= text.len_lines() {
        return Some(text.len_bytes());
    }
    let (ls, le) = (mv::line_start(text, line), mv::line_end(text, line));
    Some(match enc {
        Encoding::Utf8 => text.char_to_byte(text.byte_to_char((ls + character).min(le))),
        Encoding::Utf16 => {
            let (lc, ec) = (text.byte_to_char(ls), text.byte_to_char(le));
            let (l16, e16) = (text.char_to_utf16_cu(lc), text.char_to_utf16_cu(ec));
            let want = (l16 + character).min(e16);
            let mut c = text.utf16_cu_to_char(want);
            // (inside a surrogate pair: past that char, as counting up to `character` did)
            if text.char_to_utf16_cu(c) < want {
                c += 1;
            }
            text.char_to_byte(c.min(ec))
        }
    })
}

// ── Client ───────────────────────────────────────────────────────────────

/// What goes to the writer thread — large bodies are serialized there.
pub enum Out {
    Body(String),
    Lazy(Box<dyn FnOnce() -> String + Send>),
}

/// One `$/progress` token — reports update message/percentage, the title stays from `begin`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    pub title: String,
    pub message: Option<String>,
    pub percentage: Option<u64>,
    /// When it began — its end goes to the log with how long it ran (what a go-to waited for — N1).
    pub began: Option<std::time::Instant>,
}

impl Progress {
    /// `begin`/`report` value — fields a report leaves out keep their previous value (spec).
    pub fn update(&mut self, v: &Value) {
        if let Some(t) = v["title"].as_str() {
            self.title = t.to_string();
        }
        if let Some(m) = v["message"].as_str() {
            self.message = Some(m.to_string());
        }
        if let Some(p) = v["percentage"].as_u64() {
            self.percentage = Some(p);
        }
    }

    /// `Indexing 7% · core` — the telling part first, so when the status line cuts it (~40 cells) it takes
    /// the detail, not the percentage. A message that already counts (`3/9 crates`) says what the
    /// percentage would.
    pub fn text(&self) -> String {
        let mut s = self.title.clone();
        let msg = self.message.as_deref().map(str::trim).unwrap_or_default();
        // `3/9` counts; `src/a.rs` doesn't
        let counts = msg.split(|c: char| !c.is_ascii_digit() && c != '/').any(|w| {
            w.split_once('/').is_some_and(|(a, b)| !a.is_empty() && !b.is_empty() && !b.contains('/'))
        });
        if let Some(p) = self.percentage.filter(|_| !counts) {
            s.push_str(&format!(" {p}%"));
        }
        if !msg.is_empty() {
            s.push_str(" · ");
            s.push_str(msg);
        }
        s.trim_start_matches([' ', '·']).to_string()
    }
}

pub struct Client {
    pub id: ClientId,
    pub name: String,
    pub root: PathBuf,
    tx: Sender<Out>,
    next_req: u64,
    /// Whether the initialize response has arrived — until then, outgoing messages are queued.
    pub ready: bool,
    queue: Vec<Out>,
    pub encoding: Encoding,
    /// Work in progress (token → what it's doing).
    pub progress: HashMap<String, Progress>,
    /// Server capabilities (initialize response) — completion trigger characters etc.
    pub caps: Value,
    /// (taken by `Drop` — killed and reaped on a thread of its own)
    child: Option<Child>,
    /// For the log: how long it took to be ready.
    pub started: std::time::Instant,
    /// When the initialize response came (a server that's only just up hasn't begun loading yet).
    pub ready_at: Option<std::time::Instant>,
    /// The settings `workspace/configuration` is answered from (`Options.config`, set on the writer thread
    /// before `initialize` is written).
    pub config: std::sync::Arc<std::sync::OnceLock<Value>>,
}

impl Drop for Client {
    /// The process goes — killed now (if it hasn't exited; tarae quitting can't leave it running), then waited
    /// for on a thread of its own, so it never stays behind as a zombie and never holds up the main loop.
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

impl Client {
    /// The server's process id (`:procs`; tests: is it gone, reaped?).
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    pub fn start(
        id: ClientId,
        spec: &ServerSpec,
        root: &Path,
        options: MakeOptions,
        events: Sender<Event>,
    ) -> Result<Client, String> {
        let mut child = Command::new(&spec.command)
            .args(&spec.args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("{}: {e}", spec.command))?;
        let mut stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let mut stderr = child.stderr.take().ok_or("no stderr")?;
        let (tx, rx) = mpsc::channel::<Out>();
        let events_err = events.clone();
        thread::spawn(move || {
            while let Ok(out) = rx.recv() {
                let body = match out {
                    Out::Body(s) => s,
                    Out::Lazy(f) => f(),
                };
                let frame = format!("Content-Length: {}\r\n\r\n{body}", body.len());
                if stdin.write_all(frame.as_bytes()).and_then(|_| stdin.flush()).is_err() {
                    break;
                }
            }
        });
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            while let Some(msg) = read_message(&mut reader) {
                // (when it was read — what it then waits for the main loop is ours, not the server's)
                let at = std::time::Instant::now();
                let apply = move |ed: &mut Editor| {
                    ed.lsp.arrived = Some(at);
                    ed.on_lsp_message(id, msg)
                };
                if events.send(Event::Job(Box::new(apply))).is_err() {
                    return;
                }
            }
            // stdout closed — the server exited (or crashed)
            let _ = events.send(Event::Job(Box::new(move |ed: &mut Editor| ed.lsp_exited(id))));
        });
        // stderr → the log (`:log-open`) — also keeps the pipe from filling up and stalling the server. A
        // rustup proxy whose component isn't installed says so here, and exits: offer it
        let name = spec.name.clone();
        let tool = Path::new(&spec.command).file_name().map(|n| n.to_string_lossy().into_owned());
        let events = events_err;
        thread::spawn(move || {
            let mut lines = BufReader::new(&mut stderr);
            let mut line = String::new();
            let mut said = false;
            loop {
                line.clear();
                match lines.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => crate::log::line(&name, "stderr", &line),
                }
                if !said
                    && (line.contains("is not installed for the toolchain")
                        || line.contains("Unknown binary"))
                    && let Some(tool) = tool.clone()
                {
                    said = true;
                    let _ = events
                        .send(Event::Job(Box::new(move |ed: &mut Editor| ed.offer_install_server(&[tool]))));
                }
            }
        });
        crate::log::line(
            &spec.name,
            "start",
            &format!("{} {} (in {})", spec.command, spec.args.join(" "), root.display()),
        );
        let client = Client {
            id,
            name: spec.name.clone(),
            root: root.to_path_buf(),
            tx,
            next_req: 1,
            ready: false,
            queue: Vec::new(),
            encoding: Encoding::Utf16,
            progress: HashMap::new(),
            caps: Value::Null,
            child: Some(child),
            started: std::time::Instant::now(),
            ready_at: None,
            config: Default::default(),
        };
        let root_uri = uri(root);
        let mut init = json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "initializationOptions": Value::Null,
                "clientInfo": { "name": "tarae", "version": env!("CARGO_PKG_VERSION") },
                "rootUri": root_uri,
                "rootPath": root.to_string_lossy(),
                "workspaceFolders": [{ "uri": root_uri, "name": root.file_name().map(|n| n.to_string_lossy()).unwrap_or_default() }],
                "capabilities": {
                    "general": { "positionEncodings": ["utf-8", "utf-16"] },
                    "window": { "workDoneProgress": true, "showMessage": {} },
                    "workspace": {
                        "inlayHint": { "refreshSupport": true },
                        "configuration": true,
                        "workspaceFolders": true,
                        "symbol": {},
                        "applyEdit": true,
                        "workspaceEdit": {
                            "documentChanges": true,
                            "resourceOperations": ["create", "rename", "delete"],
                            "failureHandling": "abort",
                        },
                        "executeCommand": {},
                    },
                    "textDocument": {
                        "synchronization": { "didSave": true, "dynamicRegistration": false },
                        "publishDiagnostics": { "relatedInformation": false },
                        "hover": { "contentFormat": ["plaintext", "markdown"] },
                        "definition": { "linkSupport": true },
                        "declaration": { "linkSupport": true },
                        "typeDefinition": { "linkSupport": true },
                        "implementation": { "linkSupport": true },
                        "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                        "references": {},
                        "documentHighlight": {},
                        // Without this the server may send only Commands — rust-analyzer sends none at all.
                        "codeAction": {
                            "codeActionLiteralSupport": { "codeActionKind": { "valueSet": [
                                "", "quickfix", "refactor", "refactor.extract", "refactor.inline",
                                "refactor.rewrite", "source", "source.organizeImports"
                            ] } },
                            "resolveSupport": { "properties": ["edit", "command"] },
                            "dataSupport": true,
                            "isPreferredSupport": true,
                        },
                        "rename": { "prepareSupport": false },
                        "inlayHint": { "dynamicRegistration": false },
                        "signatureHelp": {
                            "signatureInformation": {
                                "documentationFormat": ["markdown", "plaintext"],
                                "parameterInformation": { "labelOffsetSupport": true },
                                "activeParameterSupport": true,
                            },
                            "contextSupport": true,
                        },
                        "completion": {
                            "completionItem": {
                                "snippetSupport": true,
                                "labelDetailsSupport": true,
                                "insertReplaceSupport": false,
                                "documentationFormat": ["markdown", "plaintext"],
                                "resolveSupport": { "properties": ["documentation", "detail", "additionalTextEdits"] },
                            },
                            "contextSupport": true,
                        },
                        "formatting": {},
                    },
                },
            },
        });
        // The options are made on the writer thread: some need the disk (`nested_checkouts` walks the project).
        // Its settings are in place before `initialize` goes, so before the server can ask for them.
        let config = std::sync::Arc::clone(&client.config);
        let _ = client.tx.send(Out::Lazy(Box::new(move || {
            let o = options();
            let _ = config.set(o.config);
            init["params"]["initializationOptions"] = o.init;
            init.to_string()
        })));
        Ok(client)
    }

    fn send(&mut self, out: Out) {
        if self.ready {
            let _ = self.tx.send(out);
        } else {
            self.queue.push(out);
        }
    }

    pub fn request(&mut self, method: &str, params: Value) -> u64 {
        let id = self.next_req;
        self.next_req += 1;
        self.send(Out::Body(
            json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }).to_string(),
        ));
        id
    }

    pub fn notify(&mut self, method: &str, params: Value) {
        self.send(Out::Body(json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string()));
    }

    /// Notification serialized on the writer thread (didOpen, full sync — carries the whole document).
    pub fn notify_lazy(&mut self, f: impl FnOnce() -> String + Send + 'static) {
        self.send(Out::Lazy(Box::new(f)));
    }

    pub fn reply(&mut self, id: &Value, result: Value) {
        let _ = self.tx.send(Out::Body(json!({ "jsonrpc": "2.0", "id": id, "result": result }).to_string()));
    }

    pub fn on_initialized(&mut self, result: &Value) {
        self.caps = result["capabilities"].clone();
        self.encoding = match result["capabilities"]["positionEncoding"].as_str() {
            Some("utf-8") => Encoding::Utf8,
            _ => Encoding::Utf16,
        };
        self.ready = true;
        self.ready_at = Some(std::time::Instant::now());
        let _ = self
            .tx
            .send(Out::Body(json!({ "jsonrpc": "2.0", "method": "initialized", "params": {} }).to_string()));
        for out in std::mem::take(&mut self.queue) {
            let _ = self.tx.send(out);
        }
    }

    pub fn progress_text(&self) -> Option<String> {
        self.progress.values().next().map(|p| format!("{}: {}", self.name, p.text()))
    }
}

/// Read one `Content-Length` frame.
pub fn read_message(r: &mut impl BufRead) -> Option<Value> {
    let mut len = None;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).ok()? == 0 {
            return None;
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length:") {
            len = v.trim().parse::<usize>().ok();
        }
    }
    let mut buf = vec![0; len?];
    r.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

/// One diagnostic (byte range).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub from: usize,
    pub to: usize,
    /// 1 error · 2 warning · 3 info · 4 hint
    pub severity: u8,
    pub message: String,
    /// Raw object from the server — must be sent back as-is in the code action request's context.
    pub raw: Value,
    /// The server that reported it (a document can have several).
    pub source: ClientId,
}

/// Location list (Location | Location[] | LocationLink[]) → (URI, start position object).
pub fn location_uris(v: &Value) -> Vec<(&str, Value)> {
    fn one(l: &Value) -> Option<(&str, Value)> {
        let uri = l["uri"].as_str().or_else(|| l["targetUri"].as_str())?;
        let range =
            if l["targetSelectionRange"].is_object() { &l["targetSelectionRange"] } else { &l["range"] };
        Some((uri, range["start"].clone()))
    }
    match v {
        Value::Array(a) => a.iter().filter_map(one).collect(),
        Value::Object(_) => one(v).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// Location list → (file path, start position object) — non-file URIs are skipped.
pub fn locations(v: &Value) -> Vec<(PathBuf, Value)> {
    location_uris(v).into_iter().filter_map(|(u, pos)| Some((path_from_uri(u)?, pos))).collect()
}

/// hover contents → text lines (for markdown, only code fences are stripped).
pub fn hover_text(v: &Value) -> String {
    // MarkedString {language, value} → code block, merged into one markdown (rendered by markdown.rs)
    fn part(v: &Value) -> String {
        match v {
            Value::String(s) => s.clone(),
            Value::Object(o) => {
                let value = o.get("value").and_then(Value::as_str).unwrap_or_default();
                match o.get("language").and_then(Value::as_str) {
                    Some(lang) => format!("```{lang}\n{value}\n```"),
                    None => value.to_string(),
                }
            }
            Value::Array(a) => a.iter().map(part).collect::<Vec<_>>().join("\n\n---\n\n"),
            _ => String::new(),
        }
    }
    part(&v["contents"]).trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// UTF-16 positions counted by ropey are the ones counting chars gave — both ways, at every char of random
    /// text, and for columns inside a surrogate pair or past the line's end.
    #[test]
    fn utf16_positions_match_counting_chars() {
        for seed in 1..6 {
            let t = Rope::from_str(&crate::graphemes::tests::random_text(seed, 1500, true));
            let mut byte = 0;
            loop {
                let ls = t.line_to_byte(t.byte_to_line(byte));
                let want: usize = t.byte_slice(ls..byte).chars().map(char::len_utf16).sum();
                assert_eq!(to_position(&t, byte, Encoding::Utf16)["character"], json!(want), "at {byte}");
                if byte == t.len_bytes() {
                    break;
                }
                byte = crate::graphemes::next_char(&t, byte);
            }
            for line in 0..t.len_lines() {
                let (ls, le) = (mv::line_start(&t, line), mv::line_end(&t, line));
                let units: usize = t.byte_slice(ls..le).chars().map(char::len_utf16).sum();
                for character in 0..units + 3 {
                    let (mut n, mut want) = (0, ls);
                    for c in t.byte_slice(ls..le).chars() {
                        if n >= character {
                            break;
                        }
                        n += c.len_utf16();
                        want += c.len_utf8();
                    }
                    let pos = json!({ "line": line, "character": character });
                    assert_eq!(
                        from_position(&t, &pos, Encoding::Utf16),
                        Some(want),
                        "line {line} char {character}"
                    );
                }
            }
        }
    }

    #[test]
    fn every_default_server_says_how_to_get_it() {
        let first = DEFAULTS.iter().map(|(_, c)| c[0].1);
        for tool in first.chain(["dlv", "lldb-dap"]) {
            let found = crate::packs::install(tool);
            assert!(found.is_some(), "no [install.{tool}] in the packs (or it doesn't read)");
            let found = found.unwrap();
            assert!(!found.what.is_empty());
        }
    }

    #[test]
    fn uri_roundtrip() {
        let p = Path::new("/tmp/타래 dir/a b.rs");
        let u = uri(p);
        assert!(u.starts_with("file:///tmp/%ED%83%80"));
        assert_eq!(path_from_uri(&u).as_deref(), Some(p));
    }

    #[test]
    fn positions_in_both_encodings() {
        let t = Rope::from_str("ab\n타래👍x\n");
        // 'x' = after 3 + 3 + 3 + 4 bytes / after 1 + 1 + 2 units in UTF-16
        let x = 3 + 3 + 3 + 4;
        assert_eq!(to_position(&t, x, Encoding::Utf8), json!({"line": 1, "character": 10}));
        assert_eq!(to_position(&t, x, Encoding::Utf16), json!({"line": 1, "character": 4}));
        assert_eq!(from_position(&t, &json!({"line": 1, "character": 10}), Encoding::Utf8), Some(x));
        assert_eq!(from_position(&t, &json!({"line": 1, "character": 4}), Encoding::Utf16), Some(x));
        // Overflowing column → line end
        assert_eq!(from_position(&t, &json!({"line": 0, "character": 99}), Encoding::Utf8), Some(2));
    }

    #[test]
    fn framing_and_locations() {
        let body = r#"{"jsonrpc":"2.0","id":1,"result":[{"targetUri":"file:///a.rs","targetSelectionRange":{"start":{"line":3,"character":4}}}]}"#;
        let raw = format!("Content-Length: {}\r\n\r\n{body}", body.len());
        let msg = read_message(&mut raw.as_bytes()).unwrap();
        let locs = locations(&msg["result"]);
        assert_eq!(locs, vec![(PathBuf::from("/a.rs"), json!({"line": 3, "character": 4}))]);
        let h = hover_text(&json!({"contents": {"kind": "markdown", "value": "```rust\nfn f()\n```\ndocs"}}));
        assert_eq!(h, "```rust\nfn f()\n```\ndocs", "markdown as-is (rendering parses it)");
        let old = hover_text(&json!({"contents": [{"language": "rust", "value": "fn f()"}, "docs"]}));
        assert_eq!(old, "```rust\nfn f()\n```\n\n---\n\ndocs", "old MarkedString becomes a code block");
    }

    #[test]
    fn document_end_position_is_the_end_of_the_text() {
        let t = Rope::from_str("a\nb\n");
        // Whole-document edit range ends at {line: lineCount, character: 0}
        assert_eq!(from_position(&t, &json!({"line": 2, "character": 0}), Encoding::Utf8), Some(4));
        assert_eq!(from_position(&t, &json!({"line": 9, "character": 0}), Encoding::Utf16), Some(4));
        assert_eq!(from_position(&t, &json!({"line": 1, "character": 5}), Encoding::Utf8), Some(3));
        let t = Rope::from_str("a\nb");
        assert_eq!(from_position(&t, &json!({"line": 2, "character": 0}), Encoding::Utf8), Some(3));
    }

    #[test]
    fn progress_reports_keep_the_title() {
        let mut p = Progress::default();
        p.update(&json!({"kind": "begin", "title": "Indexing", "percentage": 0}));
        for n in [5, 6, 7] {
            p.update(&json!({"kind": "report", "percentage": n}));
        }
        assert_eq!(p.text(), "Indexing 7%");
        p.update(&json!({"kind": "report", "message": "3/9 crates"}));
        assert_eq!(p.text(), "Indexing · 3/9 crates");
        p.update(&json!({"kind": "report", "message": "core"}));
        assert_eq!(p.text(), "Indexing 7% · core");
        p.update(&json!({"kind": "report", "message": "src/a/b.rs"}));
        assert_eq!(p.text(), "Indexing 7% · src/a/b.rs", "a path isn't a count");
        p.update(&json!({"kind": "report", "message": "101/101: core"}));
        assert_eq!(p.text(), "Indexing · 101/101: core");
    }

    #[test]
    fn finds_root_by_markers() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(find_root(&here.join("src/lsp.rs")), here);
    }

    #[test]
    fn root_is_the_topmost_marker_below_the_repo() {
        let t = std::env::temp_dir().join(format!("tarae-lsp-root-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let member = t.join("repo/crates/core/src");
        std::fs::create_dir_all(&member).unwrap();
        std::fs::create_dir_all(t.join("repo/.git")).unwrap();
        std::fs::write(t.join("Cargo.toml"), "").unwrap(); // outside the repo — not crossed
        std::fs::write(t.join("repo/Cargo.toml"), "[workspace]").unwrap();
        std::fs::write(t.join("repo/crates/core/Cargo.toml"), "").unwrap();
        std::fs::write(t.join("repo/crates/package.json"), "").unwrap();
        let file = member.join("lib.rs");
        assert_eq!(find_root_for("rust", &file), t.join("repo"), "one server per workspace, not per member");
        assert_eq!(find_root_for("python", &file), t.join("repo"), "no python marker → repo root");
        assert_eq!(find_root_for("typescript", &file), t.join("repo/crates"));
        std::fs::remove_file(t.join("repo/Cargo.toml")).unwrap();
        assert_eq!(find_root_for("rust", &file), t.join("repo/crates/core"));
        let _ = std::fs::remove_dir_all(&t);
    }

    /// rust-analyzer is told to leave alone what isn't the project: worktrees (whole `.claude/worktrees`,
    /// later ones too) and clones inside it — not submodules (may be members), not through links, not
    /// under `target`, not deeper than `NESTED_DEPTH`. Every server whose pack says how gets them through
    /// its own setting — at start (rust-analyzer, jdtls beside its own options, gopls) or when it asks for
    /// its section (pyright, found by its command under another name) — with the setting's defaults kept.
    /// Others get their options untouched, and so does every server of a project with nothing nested.
    #[test]
    fn servers_leave_nested_checkouts_alone() {
        let t = std::env::temp_dir().join(format!("tarae-lsp-nested-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let root = t.join("repo");
        for d in [
            ".git",
            ".claude/worktrees/a",
            "vendor/clone/.git",
            "sub/mod",
            "target/x/.git",
            "crates/core/src",
        ] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join(".claude/worktrees/a/.git"), "gitdir: ../../../.git/worktrees/a").unwrap();
        std::fs::write(root.join("sub/mod/.git"), "gitdir: ../../.git/modules/mod").unwrap();
        std::fs::write(root.join(".gitmodules"), "[submodule \"mod\"]\n\tpath = sub/mod\n").unwrap();
        std::fs::create_dir_all(root.join("src/a/b/c/d/.git")).unwrap(); // level 5
        std::fs::create_dir_all(root.join("src/a/b/c4/.git")).unwrap(); // level 4
        std::os::unix::fs::symlink(root.join("vendor/clone"), root.join("link")).unwrap();
        let mut found = nested_checkouts(&root);
        found.sort();
        assert_eq!(found, [".claude/worktrees", "src/a/b/c4", "vendor/clone"]);

        let spec = |name: &str, command: &str| ServerSpec {
            name: name.into(),
            command: command.into(),
            args: vec![],
            except: vec![],
        };
        let strs = |v: &Value| -> Vec<String> {
            v.as_array().into_iter().flatten().filter_map(|s| s.as_str().map(str::to_string)).collect()
        };
        let ra = options_for(&spec("rust-analyzer", "rust-analyzer"), &root, Value::Null)();
        let mut ex = strs(&ra.init["files"]["exclude"]);
        ex.sort();
        assert_eq!(ex, [".claude/worktrees", "src/a/b/c4", "vendor/clone"]);
        assert_eq!(ra.config, Value::Null);

        let given =
            json!({ "bundles": ["x.jar"], "settings": { "java": { "maven": { "downloadSources": true } } } });
        let jdtls = options_for(&spec("jdtls", "/opt/jdtls/bin/jdtls"), &root, given.clone())().init;
        assert_eq!(
            (&jdtls["bundles"], &jdtls["settings"]["java"]["maven"]),
            (&given["bundles"], &json!({ "downloadSources": true })),
            "its own kept"
        );
        let ex = strs(&jdtls["settings"]["java"]["import"]["exclusions"]);
        let defaults =
            ["**/node_modules/**", "**/.metadata/**", "**/archetype-resources/**", "**/META-INF/maven/**"];
        assert!(ex.starts_with(&defaults.map(String::from)), "defaults first: {ex:?}");
        // (the folder holding a build file, and below it — jdtls matches the folder)
        for e in ["**/.claude/worktrees", "**/.claude/worktrees/**", "**/vendor/clone", "**/vendor/clone/**"]
        {
            assert!(ex.contains(&e.to_string()), "{e}: {ex:?}");
        }
        assert_eq!(ex.len(), 4 + 3 * 2, "{ex:?}");

        let py = options_for(&spec("py", "/usr/local/bin/pyright-langserver"), &root, Value::Null)();
        assert_eq!(py.init, Value::Null, "pyright asks for it");
        let ex = strs(&section(&py.config, Some("python.analysis"))["exclude"]);
        assert!(ex.starts_with(&["**/node_modules", "**/__pycache__", "**/.*"].map(String::from)), "{ex:?}");
        assert!(ex.contains(&"vendor/clone".to_string()) && ex.len() == 7, "{ex:?}");
        assert_eq!(
            section(&py.config, Some("python"))["analysis"]["exclude"].as_array().map(Vec::len),
            Some(7)
        );
        assert_eq!(section(&py.config, Some("pyright")), Value::Null);

        let gopls = options_for(&spec("gopls", "gopls"), &root, Value::Null)().init;
        let f = strs(&gopls["directoryFilters"]);
        assert!(
            f.first().is_some_and(|d| d == "-**/node_modules") && f.contains(&"-vendor/clone".into()),
            "{f:?}"
        );

        let taplo = options_for(&spec("taplo", "taplo"), &root, Value::Null)();
        assert_eq!((taplo.init, taplo.config), (Value::Null, Value::Null), "no leave-out in its pack");
        let plain = t.join("plain");
        std::fs::create_dir_all(plain.join("src")).unwrap();
        let ra = options_for(&spec("rust-analyzer", "rust-analyzer"), &plain, Value::Null)();
        assert_eq!(ra.init, Value::Null, "nothing nested: nothing said");
        let _ = std::fs::remove_dir_all(&t);
    }
}
