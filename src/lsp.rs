//! LSP client — **fully asynchronous** (principle 4: input and rendering never block on LSP).
//!
//! Three threads per server: writer (channel → stdin), reader (stdout → main-loop events), stderr drain.
//! The main thread only pushes to channels — editing never stalls even if a busy server stops reading stdin.
//! Big bodies (didOpen, full sync) are serialized on the writer thread too (we pass an O(1) rope clone).
//! Asks for UTF-8 positions first — editor positions are bytes, no conversion. UTF-16-only servers also work.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::mpsc::{self, Sender};

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

/// What a pack says of a server beyond how it starts (`[server.<name>]`, read once with the language table): the
/// extensions of the protocol it speaks, and what tarae does with them. A server no pack knows has none.
#[derive(Debug, Default)]
pub struct ServerTraits {
    /// `share`: one of it serves every checkout of a repository, unless `editor.lsp-share` says otherwise.
    pub share: Option<Share>,
    /// `quiescent`: it says when it has nothing left to load (a swap switches then — `share.rs`).
    pub quiescent: Option<Quiescent>,
    /// `symbol-params`: added to `workspace/symbol`'s params (an object; Null: none).
    pub symbol_params: Value,
    /// `hide-lines`: lines of a diagnostic's message that say nothing about what's wrong — left out of it.
    pub hide_lines: Vec<regex::Regex>,
    /// `args-add`: arguments added at its start (`start_spec`), unless its arguments hold `args-unless`.
    pub args_add: Vec<String>,
    pub args_unless: Option<String>,
    /// `init`: its `initializationOptions` (Null: none) — `{installed:<tool>}` filled where it starts
    /// (`installed_filled`).
    pub init: Value,
    /// `progress`: a notification of its own saying what it's doing while it starts.
    pub progress: Option<Status>,
    /// `virtual`: documents it serves at URIs of a scheme of its own (a library's class).
    pub virtual_docs: Option<Virtual>,
    /// `client-commands`: commands it hands back for the client to run, by what tarae does for each.
    pub client_commands: Vec<(String, ClientAction)>,
}

/// A pack's `progress = { … }`: a server's own notification of what it's doing, shown as progress.
#[derive(Debug)]
pub struct Status {
    pub method: String,
    /// Where in its params: what kind of report it is · its words.
    pub kind: String,
    pub text: String,
    /// Kinds that mean it's done (the progress goes) · words that mean so, whatever the kind · kinds that
    /// are errors.
    pub done: Vec<String>,
    pub done_text: Option<regex::Regex>,
    pub error: Vec<String>,
}

impl Status {
    /// What `params` says it's doing — None when it's done.
    pub fn text(&self, params: &Value) -> Option<String> {
        let words = params[&self.text].as_str().unwrap_or_default();
        let kind = params[&self.kind].as_str().unwrap_or_default();
        if self.done.iter().any(|d| d == kind) || self.done_text.as_ref().is_some_and(|r| r.is_match(words)) {
            return None;
        }
        Some(match self.error.iter().any(|e| e == kind) {
            true => format!("error · {words}"),
            false if words.is_empty() => "starting".into(),
            false => words.to_string(),
        })
    }
}

/// A pack's `virtual = { … }`: where a server's own documents are, and how they're fetched and shown.
#[derive(Debug)]
pub struct Virtual {
    /// The start of their URIs (`jdt://`).
    pub scheme: String,
    /// The request that returns one's text, given `{ "uri": … }`.
    pub fetch: String,
    /// The language they're in.
    pub language: String,
    /// Their title: the last part of the path without its extension, after the part before it and this
    /// (`.`: `java.util.ArrayList`) — none: the last part alone.
    pub title_join: Option<String>,
}

impl Virtual {
    /// The first place of a location answer, when it's one of these: (its URI, its start).
    pub fn location(&self, result: &Value) -> Option<(String, Value)> {
        let (uri, pos) = location_uris(result).into_iter().next()?;
        uri.starts_with(&self.scheme).then(|| (uri.to_string(), pos))
    }

    /// `jdt://contents/java.base/java.util/ArrayList.class?=…` → `java.util.ArrayList` (with `title-join`
    /// "."): the path decoded, without its query; the part right after the scheme is where, not what.
    pub fn title(&self, uri: &str) -> String {
        let rest = uri.strip_prefix(&self.scheme).unwrap_or(uri);
        let path = rest.split('?').next().unwrap_or(rest);
        let decoded = String::from_utf8_lossy(&percent_decode(path)).into_owned();
        let parts: Vec<&str> = decoded.split('/').filter(|p| !p.is_empty()).collect();
        let last = parts.last().copied().unwrap_or_default();
        let Some(join) = &self.title_join else { return last.to_string() };
        let stem = last.rsplit_once('.').map_or(last, |(s, _)| s);
        match parts.len() {
            n if n >= 3 => format!("{}{join}{stem}", parts[n - 2]),
            _ => stem.to_string(),
        }
    }
}

/// What tarae does for a server's client command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientAction {
    /// `apply-edit`: each of its arguments is a WorkspaceEdit — applied here, the server not asked.
    ApplyEdit,
}

/// `spec` as it starts in `root`: with its pack's `args-add` — `{cache}` (tarae's cache folder), `{root}`,
/// `{root-name}` (its folder's name, `root` for `/`), `{root-hash}` (FNV-1a of its whole path, stable across
/// runs and Rust versions — jdtls's `-data` folder must stay where it is) — unless its arguments already hold
/// `args-unless`, or a variable can't be filled (no home folder).
pub fn start_spec(spec: &ServerSpec, root: &Path) -> ServerSpec {
    start_spec_with(spec, traits(spec), root, crate::runtime::cache_dir())
}

fn start_spec_with(spec: &ServerSpec, t: &ServerTraits, root: &Path, cache: Option<PathBuf>) -> ServerSpec {
    let mut spec = spec.clone();
    if t.args_add.is_empty() || t.args_unless.as_ref().is_some_and(|u| spec.args.contains(u)) {
        return spec;
    }
    let name = root.file_name().map_or_else(|| "root".into(), |n| n.to_string_lossy().into_owned());
    let hash = root
        .to_string_lossy()
        .bytes()
        .fold(0xcbf29ce484222325u64, |h, b| (h ^ u64::from(b)).wrapping_mul(0x100000001b3));
    let fill = |a: &String| -> Option<String> {
        let mut a = a.replace("{root-name}", &name).replace("{root-hash}", &format!("{hash:016x}"));
        a = a.replace("{root}", &root.to_string_lossy());
        if a.contains("{cache}") {
            a = a.replace("{cache}", &cache.as_ref()?.to_string_lossy());
        }
        Some(a)
    };
    match t.args_add.iter().map(fill).collect::<Option<Vec<String>>>() {
        Some(more) => spec.args.extend(more),
        None => crate::log::line(&spec.name, "warning", "no home folder: its pack's `args-add` left out"),
    }
    spec
}

/// `init` with each `{installed:<tool>}` string the path where that tool is (`packs::installed_at`) — an item
/// of a list left out, any other place null, when it isn't there. Disk: the server's writer thread.
pub fn installed_filled(v: Value) -> Value {
    fn tool(s: &str) -> Option<&str> {
        s.strip_prefix("{installed:")?.strip_suffix('}')
    }
    let found = |t: &str| crate::packs::installed_at(t).map(|p| Value::String(p.display().to_string()));
    match v {
        Value::String(s) => match tool(&s) {
            Some(t) => found(t).unwrap_or(Value::Null),
            None => Value::String(s),
        },
        Value::Array(a) => Value::Array(
            a.into_iter()
                .filter_map(|x| match x.as_str().and_then(tool) {
                    Some(t) => found(t),
                    None => Some(installed_filled(x)),
                })
                .collect(),
        ),
        Value::Object(m) => Value::Object(m.into_iter().map(|(k, x)| (k, installed_filled(x))).collect()),
        v => v,
    }
}

/// A pack's `share = { … }`: what sharing one server between checkouts needs to know of it.
#[derive(Debug, Default)]
pub struct Share {
    /// Files whose words pick its toolchain, the nearest up: checkouts on different ones get a server each.
    pub toolchain: Vec<String>,
    /// The setting (a dotted path) that turns its checks off — held off while a swap's new server loads.
    pub checks: Option<String>,
    /// The `$/progress` title that means it's loading its workspaces again (a toolchain may have changed).
    pub reload_progress: Option<String>,
}

/// A pack's `quiescent = { … }`: the notification that says it's done loading.
#[derive(Debug)]
pub struct Quiescent {
    pub method: String,
    /// The client capability (a dotted path, set to true) that asks for it.
    pub capability: Option<String>,
    /// Where in its params: whether it's quiescent · its health (`ok` unless said) · what's wrong.
    pub field: String,
    pub health: Option<String>,
    pub message: Option<String>,
}

/// The traits of a server no pack knows.
pub static NO_TRAITS: ServerTraits = ServerTraits {
    share: None,
    quiescent: None,
    symbol_params: Value::Null,
    hide_lines: Vec::new(),
    args_add: Vec::new(),
    args_unless: None,
    init: Value::Null,
    progress: None,
    virtual_docs: None,
    client_commands: Vec::new(),
};

/// The names `spec` goes by in the packs: its own, then the built-in servers whose command it runs (`[lsp.py]
/// command = "pyright-langserver"` is pyright).
fn pack_names(spec: &ServerSpec) -> Vec<String> {
    let mut names = vec![spec.name.clone()];
    let program = Path::new(&spec.command).file_name().map(|n| n.to_string_lossy().into_owned());
    let builtin = crate::languages::get().servers.iter();
    names.extend(builtin.filter(|s| program.as_deref() == Some(s.command.as_str())).map(|s| s.name.clone()));
    names
}

/// `spec`'s traits, by the names it goes by in the packs.
pub fn traits(spec: &ServerSpec) -> &'static ServerTraits {
    let all = &crate::languages::get().traits;
    pack_names(spec).iter().find_map(|n| all.get(n)).unwrap_or(&NO_TRAITS)
}

impl ServerTraits {
    /// A diagnostic's `message` without the lines `hide-lines` leaves out — whole when every line would go.
    pub fn shown_message(&self, message: &str) -> String {
        let hidden = |l: &str| self.hide_lines.iter().any(|r| r.is_match(l.trim()));
        if self.hide_lines.is_empty() || !message.lines().any(hidden) {
            return message.to_string();
        }
        let kept: Vec<&str> = message.lines().filter(|l| !hidden(l)).collect();
        match kept.iter().all(|l| l.trim().is_empty()) {
            true => message.to_string(),
            false => kept.join("\n"),
        }
    }
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

/// Language id (LSP languageId) — its pack's `language-id`, else the language name as-is.
pub fn language_id(lang: &str) -> &str {
    match crate::syntax::spec(lang) {
        Some(s) => &s.language_id,
        None => lang,
    }
}

/// Servers for this language: the configured list if any, else the first default candidate on PATH — or,
/// failing that, one installed where its pack says it lands off `PATH` (`go install` → `~/go/bin`), by its
/// full path.
pub fn find_server(lang: &str, config: &LspConfig) -> Option<ServerSpec> {
    let all = candidates(lang, config);
    if let Some(s) = all.iter().find(|s| findable(s, config)) {
        return Some(s.clone());
    }
    all.into_iter().filter(|s| !s.command.contains('/') && test_named(s, config)).find_map(|mut s| {
        s.command = crate::packs::installed_at(&s.command)?.display().to_string();
        Some(s)
    })
}

/// The names of the servers configured for `lang` (`[lang.<lang>] lsp`, else its pack's `lsp`).
pub fn names(lang: &str, config: &LspConfig) -> Vec<String> {
    match config.languages.get(lang) {
        Some(names) => names.clone(),
        None => crate::syntax::spec(lang).map(|s| s.lsp.clone()).unwrap_or_default(),
    }
}

/// The servers configured for `lang` (or its pack's `lsp`), installed or not.
pub fn candidates(lang: &str, config: &LspConfig) -> Vec<ServerSpec> {
    names(lang, config)
        .iter()
        .filter_map(|n| config.servers.get(n).cloned().or_else(|| default_spec(n)))
        .collect()
}

/// The servers that start beside the main one for `lang` (`[lang.<lang>] also`, else the defaults): those on
/// PATH, and the names of those that aren't (for the log).
pub fn also_servers(lang: &str, config: &LspConfig) -> (Vec<ServerSpec>, Vec<String>) {
    let (on, off): (Vec<ServerSpec>, Vec<ServerSpec>) = also_names(lang, config)
        .into_iter()
        .filter_map(|n| config.servers.get(&n).cloned().or_else(|| default_spec(&n)))
        .partition(|s| findable(s, config));
    (on, off.into_iter().map(|s| s.name).collect())
}

/// A server that may start: on PATH — and, in tests, one the test's own config names (`test_named`).
fn findable(s: &ServerSpec, config: &LspConfig) -> bool {
    test_named(s, config) && on_path(&s.command)
}

/// Tests find only the servers their config names (a fake: `[lsp.<name>] command = "sh"`), never a pack's
/// installed here — taplo fetched its schema catalog from the network, rust-analyzer runs cargo — as CI, with
/// none installed, always found. (`Client::start` refuses a pack's program whatever names it.)
fn test_named(s: &ServerSpec, config: &LspConfig) -> bool {
    !cfg!(test) || config.servers.contains_key(&s.name)
}

/// Tests: a pack server's own program is never started, whatever config names it (`test_named` keeps them from
/// being found) — the error says what to configure instead.
#[cfg(test)]
fn refused_in_tests(spec: &ServerSpec) -> Option<String> {
    let program = |c: &str| Path::new(c).file_name().map(std::ffi::OsStr::to_os_string);
    let packs = &crate::languages::get().servers;
    packs.iter().any(|s| program(&s.command) == program(&spec.command)).then(|| {
        format!(
            "a unit test starts no real language server ({}: it may reach the network) — give the test a fake: \
             [lsp.{}] command = \"sh\", args = [\"-c\", \"cat > /dev/null\"]",
            spec.command, spec.name
        )
    })
}

/// The names of the servers that start beside the main one for `lang` (`[lang.<lang>] also`, else its
/// pack's `also`), installed or not.
pub fn also_names(lang: &str, config: &LspConfig) -> Vec<String> {
    match config.also.get(lang) {
        Some(names) => names.clone(),
        None => crate::syntax::spec(lang).map(|s| s.also.clone()).unwrap_or_default(),
    }
}

/// A server the packs know, by name (`[server.<name>]` — a main one or one that runs beside).
pub fn default_spec(name: &str) -> Option<ServerSpec> {
    crate::languages::get().servers.iter().find(|s| s.name == name).cloned()
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
    crate::sorting::by_key(&mut out, |h| h.pos);
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
    /// `editor.lsp-share` — servers one of which serves every checkout of a repository (`share.rs`). None: the
    /// ones whose pack says `share`.
    pub share: Option<Vec<String>>,
}

impl LspConfig {
    /// Whether one server `name` (with `traits`) serves every checkout of a repository.
    pub fn shares(&self, name: &str, traits: &ServerTraits) -> bool {
        match &self.share {
            Some(list) => list.iter().any(|s| s == name),
            None => traits.share.is_some(),
        }
    }
}

/// The servers whose pack says `share` — `editor.lsp-share` when it isn't set.
pub fn shared_by_packs() -> Vec<String> {
    let mut names: Vec<String> = crate::languages::get()
        .traits
        .iter()
        .filter(|(_, t)| t.share.is_some())
        .map(|(n, _)| n.clone())
        .collect();
    crate::sorting::sort(&mut names);
    names
}

/// Markers that make a project root for this language (its pack's `roots`) — other languages' don't count
/// (a Python file under a repo-root package.json). A language with none, or an unknown one, takes any
/// language's (`.git` is handled by `root_by_markers`).
pub(crate) fn markers_for(lang: &str) -> &'static [String] {
    match crate::syntax::spec(lang) {
        Some(s) if !s.roots.is_empty() => &s.roots,
        _ => &crate::languages::get().roots_any,
    }
}

/// Whether `lang`'s server can't work without a project, and the markers that make one (its pack's
/// `needs-project`): rust-analyzer in a lone `.rs` finds no workspace ("Failed to discover workspace" on the
/// status line at every open) — so a file with none of these above it gets no server.
pub fn project_markers(lang: &str) -> Option<&'static [String]> {
    crate::syntax::spec(lang).map(|s| s.needs_project.as_slice()).filter(|m| !m.is_empty())
}

/// The **topmost** dir with a marker, walking up but never past the repo root (`.git`) or into `$HOME` —
/// one server per workspace, not one per member crate/module (each is hundreds of MB). None if no marker.
pub fn root_by_markers<S: AsRef<str>>(file: &Path, markers: &[S]) -> Option<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut dir = file.parent()?.to_path_buf();
    let mut top = None;
    loop {
        if home.as_deref() == Some(dir.as_path()) {
            return top;
        }
        if markers.iter().any(|m| dir.join(m.as_ref()).is_file()) {
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
    root_by_markers(file, &crate::languages::get().roots_any).unwrap_or_else(|| repo_or_dir(file))
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

/// Folders `nested_checkouts` doesn't go into: git's own, and tools' output (the packs' `outputs`).
pub(crate) fn nested_skips(name: &str) -> bool {
    name == ".git" || crate::languages::get().outputs.iter().any(|o| o == name)
}

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
            if nested_skips(&e.file_name().to_string_lossy()) || WORKTREE_HOMES.contains(&rel.as_str()) {
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

/// What a server is told at its start: `initializationOptions` (its pack's `init`, and more), and the settings
/// `workspace/configuration` is answered from (`section`).
pub struct Options {
    pub init: Value,
    pub config: Value,
}

/// Made on the server's writer thread: its pack is read and the project walked (disk work).
pub type MakeOptions = Box<dyn FnOnce() -> Options + Send>;

/// Its pack's `init` (the tools it names found — `installed_filled`), and the server's pack's `leave-out`
/// (`packs::LeaveOut`) filled with `nested_checkouts` — so a server that scans the workspace leaves worktrees
/// and checkouts inside it alone, through its own setting. The server is looked for by its name, else by the
/// built-in one its command is (`[lsp.py] command = "pyright-langserver"` is pyright). Several folders (a
/// shared server): the first one's are relative to it, as the server reads them, the others' whole paths.
pub fn options_for(spec: &ServerSpec, roots: &[PathBuf]) -> MakeOptions {
    let names = pack_names(spec);
    let init = traits(spec).init.clone();
    let (roots, name) = (roots.to_vec(), spec.name.clone());
    Box::new(move || {
        let mut o = Options { init: installed_filled(init), config: Value::Null };
        let Some(leave) = names.iter().find_map(|n| crate::packs::leave_out(n)) else { return o };
        let found: Vec<String> = roots
            .iter()
            .enumerate()
            .flat_map(|(i, r)| {
                nested_checkouts(r)
                    .into_iter()
                    .map(move |rel| if i == 0 { rel } else { r.join(rel).display().to_string() })
            })
            .collect();
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
pub(crate) fn put(v: &mut Value, path: &str, x: Value) {
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

impl Client {
    /// Whose its log lines are: the session whose checkout holds its folders — one that serves several
    /// sessions' checkouts is each of theirs, so tarae's own.
    pub fn log(&self) -> crate::log::Session {
        log_of(&self.roots)
    }
}

/// Whose a server's log lines are, serving `roots`: the one session whose checkout holds them all, else
/// tarae's own.
pub(crate) fn log_of(roots: &[PathBuf]) -> crate::log::Session {
    let Some(first) = roots.first() else { return crate::log::Session::none() };
    let s = crate::log::Session::of(first);
    match roots.iter().all(|r| crate::log::Session::of(r) == s) {
        true => s,
        false => crate::log::Session::none(),
    }
}

pub struct Client {
    pub id: ClientId,
    pub name: String,
    /// What it was started as (a swap starts its next one the same way — `share.rs`).
    pub spec: ServerSpec,
    /// What its pack says it speaks beyond the protocol.
    pub traits: &'static ServerTraits,
    /// Its first workspace folder (its working folder).
    pub root: PathBuf,
    /// Every workspace folder it serves, `root` first — several when checkouts of a repository share it.
    pub roots: Vec<PathBuf>,
    /// The repository and toolchain it's shared by (None: one folder's own server).
    pub share: Option<crate::share::Key>,
    /// Its checks (its pack's `share.checks`) held off — a swap's new server until it takes over, a restarted
    /// one until it's loaded (`share.rs`; measured: that skips a check of every folder at its start).
    pub checks_off: bool,
    /// What its pack's `quiescent` notification said: quiescent now (None until it says) · whether it ever was
    /// (a server still loading is restarted, not swapped) · its health.
    pub quiescent: Option<bool>,
    pub settled_once: bool,
    pub health: String,
    /// The `initializationOptions` it was started with (set on the writer thread) — a shared server's
    /// settings answer is these.
    pub init_sent: std::sync::Arc<std::sync::OnceLock<Value>>,
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
    /// tarae's own, in this process (`Client::own`): no program, no folder — a server's life (its process, its idle
    /// stop, its restart) isn't its.
    pub own: bool,
    /// tarae's own: what's sent to it, and what answers it (`own_batch`).
    own_io: Option<OwnIo>,
}

struct OwnIo {
    rx: mpsc::Receiver<Out>,
    serve: std::sync::Arc<std::sync::Mutex<Serve>>,
}

/// What tarae's own server answers (`Client::own`): a message from the client — a request's result (None: `null`),
/// a notification's ignored. Run on a worker, in the order the messages were sent.
pub type Serve = Box<dyn FnMut(&str, &Value) -> Option<Value> + Send>;

impl Drop for Client {
    /// The process goes — killed now (if it hasn't exited; tarae quitting can't leave it running), then waited
    /// for on a thread of its own, so it never stays behind as a zombie and never holds up the main loop.
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            crate::event::thread("tarae-reap", move || {
                let _ = child.wait();
            });
        }
    }
}

/// A workspace folder as LSP says it.
pub fn folder(r: &Path) -> Value {
    json!({ "uri": uri(r), "name": r.file_name().map(|n| n.to_string_lossy()).unwrap_or_default() })
}

impl Client {
    /// Whether it takes `workspace/didChangeWorkspaceFolders` (its `initialize` answer says so: `true`, or
    /// the id it registers them under).
    pub fn takes_folder_changes(&self) -> bool {
        let n = &self.caps["workspace"]["workspaceFolders"]["changeNotifications"];
        n.as_bool() == Some(true) || n.is_string()
    }

    /// The server's process id (`:procs`; tests: is it gone, reaped?).
    pub fn pid(&self) -> Option<u32> {
        self.child.as_ref().map(Child::id)
    }

    /// Whether it serves `path` (it's under one of its folders).
    pub fn serves(&self, path: &Path) -> bool {
        self.roots.iter().any(|r| path.starts_with(r))
    }

    /// Started for the workspace folders `roots` (the first is its working folder).
    pub fn start(
        id: ClientId,
        spec: &ServerSpec,
        roots: &[PathBuf],
        options: MakeOptions,
        events: crate::event::Tx,
    ) -> Result<Client, String> {
        let root = roots.first().ok_or("no folder")?.as_path();
        #[cfg(test)]
        if let Some(why) = refused_in_tests(spec) {
            return Err(why);
        }
        // (the descriptor limit tarae started with — what it builds and runs inherits it: `nofile`)
        let mut child = crate::nofile::command(&spec.command, Some(root))
            .args(&spec.args)
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
        crate::event::thread("tarae-lsp-write", move || {
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
        crate::event::thread("tarae-lsp-read", move || {
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
        // program that's there but not installed (a rustup proxy whose component isn't) says so here, as its
        // pack's `missing-says` has it, and exits: offer it
        let name = spec.name.clone();
        let tool = Path::new(&spec.command).file_name().map(|n| n.to_string_lossy().into_owned());
        let events = events_err;
        // (whose, by the folders it starts with: a checkout joining a shared one later isn't seen here)
        let log = log_of(roots);
        let said_start = log.clone();
        crate::event::thread("tarae-lsp-err", move || {
            let mut lines = BufReader::new(&mut stderr);
            let mut line = String::new();
            let mut said = false;
            let mut missing: Option<Vec<String>> = None;
            loop {
                line.clear();
                match lines.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => log.line(&name, "stderr", &line),
                }
                let Some(tool) = tool.clone() else { continue };
                // (read at the first line: most servers never print one)
                let missing = missing.get_or_insert_with(|| crate::packs::missing_says(&tool));
                if !said && missing.iter().any(|m| line.contains(m.as_str())) {
                    said = true;
                    let _ = events
                        .send(Event::Job(Box::new(move |ed: &mut Editor| ed.offer_install_server(&[tool]))));
                }
            }
        });
        said_start.line(
            &spec.name,
            "start",
            &format!("{} {} (in {})", spec.command, spec.args.join(" "), root.display()),
        );
        let client = Client {
            id,
            name: spec.name.clone(),
            spec: spec.clone(),
            traits: traits(spec),
            root: root.to_path_buf(),
            roots: roots.to_vec(),
            share: None,
            checks_off: false,
            quiescent: None,
            settled_once: false,
            health: String::new(),
            init_sent: Default::default(),
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
            own: false,
            own_io: None,
        };
        let root_uri = uri(root);
        let folders: Vec<Value> = roots.iter().map(|r| folder(r)).collect();
        let mut init = json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "initializationOptions": Value::Null,
                "clientInfo": { "name": "tarae", "version": env!("CARGO_PKG_VERSION") },
                "rootUri": root_uri,
                "rootPath": root.to_string_lossy(),
                "workspaceFolders": folders,
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
        // One that says when it's quiescent, asked to (a swap switches then — `share.rs`)
        if let Some(cap) = client.traits.quiescent.as_ref().and_then(|q| q.capability.as_deref()) {
            put(&mut init["params"]["capabilities"], cap, json!(true));
        }
        // The options are made on the writer thread: some need the disk (`nested_checkouts` walks the project).
        // Its settings are in place before `initialize` goes, so before the server can ask for them.
        let config = std::sync::Arc::clone(&client.config);
        let init_sent = std::sync::Arc::clone(&client.init_sent);
        let _ = client.tx.send(Out::Lazy(Box::new(move || {
            let o = options();
            let _ = config.set(o.config);
            let _ = init_sent.set(o.init.clone());
            init["params"]["initializationOptions"] = o.init;
            init.to_string()
        })));
        Ok(client)
    }

    /// A server of tarae's own, in this process (web-P3: a page's links and headings): what's sent to it waits until
    /// the editor hands it to `serve` on a worker (`own_batch` — a job, as the simulation knows them), and its answers
    /// come back as a server's reader thread brings them — events, to `on_lsp_message` — so every request takes the
    /// one path: `server_for`, `Pending`, a late answer dropped, the kind's handler. Initialized as a server is
    /// (`serve` answers `initialize` with its capabilities).
    pub fn own(id: ClientId, name: &str, traits: &'static ServerTraits, serve: Serve) -> Client {
        let (tx, rx) = mpsc::channel::<Out>();
        let client = Client {
            id,
            name: name.to_string(),
            spec: ServerSpec {
                name: name.to_string(),
                command: String::new(),
                args: Vec::new(),
                except: Vec::new(),
            },
            traits,
            root: PathBuf::new(),
            roots: Vec::new(),
            share: None,
            checks_off: false,
            quiescent: None,
            settled_once: false,
            health: String::new(),
            init_sent: Default::default(),
            tx,
            next_req: 1,
            ready: false,
            queue: Vec::new(),
            encoding: Encoding::Utf8,
            progress: HashMap::new(),
            caps: Value::Null,
            child: None,
            started: crate::clock::now(),
            ready_at: None,
            config: Default::default(),
            own: true,
            own_io: Some(OwnIo { rx, serve: std::sync::Arc::new(std::sync::Mutex::new(serve)) }),
        };
        let init = json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {} });
        let _ = client.tx.send(Out::Body(init.to_string()));
        client
    }

    /// tarae's own: what was sent to it since the last batch, as work for a worker — each request's reply, in the
    /// order sent (a notification heard, answered by nothing). None: nothing sent, or not tarae's own.
    pub fn own_batch(&mut self) -> Option<impl FnOnce() -> Vec<Value> + Send + 'static> {
        let io = self.own_io.as_ref()?;
        let sent: Vec<Out> = io.rx.try_iter().collect();
        if sent.is_empty() {
            return None;
        }
        let serve = io.serve.clone();
        Some(move || {
            let mut serve = serve.lock().unwrap_or_else(|e| e.into_inner());
            let mut replies = Vec::new();
            for out in sent {
                let body = match out {
                    Out::Body(s) => s,
                    Out::Lazy(f) => f(),
                };
                let Ok(msg) = serde_json::from_str::<Value>(&body) else { continue };
                let method = msg["method"].as_str().unwrap_or_default().to_string();
                let answer = serve(&method, &msg["params"]);
                // (a request has an id and a method; a response to one of its own it never sends)
                let Some(rid) = msg.get("id").filter(|_| !method.is_empty()).cloned() else { continue };
                replies.push(json!({ "jsonrpc": "2.0", "id": rid, "result": answer.unwrap_or(Value::Null) }));
            }
            replies
        })
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

    fn spec_of(name: &str, command: &str, args: &[&str]) -> ServerSpec {
        let args = args.iter().map(|a| a.to_string()).collect();
        ServerSpec { name: name.into(), command: command.into(), args, except: vec![] }
    }

    /// jdtls's `-data` folder for a project, as every user's jdtls has it now — a new name re-imports each
    /// project from scratch (minutes each). Pinned from what `java::workspace_dir` gave on 2026-10-05, before
    /// it went for the java pack's `args-add`.
    #[test]
    fn jdtls_data_folders_stay_where_they_are() {
        let jdtls = spec_of("jdtls", "jdtls", &[]);
        let cache = crate::runtime::cache_dir().expect("a home folder");
        for (root, name) in [
            ("/work/a/app", "app-964de123d19fd75b"),
            ("/Users/someone/Workspace/commons-lang", "commons-lang-c23c2a3c8437fd01"),
            ("/private/tmp/tarae-java-small", "tarae-java-small-4a468dd58d495f8a"),
            ("/", "root-af63a24c860189fe"),
        ] {
            let data = cache.join("jdtls").join(name).display().to_string();
            assert_eq!(start_spec(&jdtls, Path::new(root)).args, ["-data", data.as_str()], "{root}");
        }
        // Renamed in the config, found by its command; one given its own `-data` keeps it
        let renamed = spec_of("java", "/opt/jdtls/bin/jdtls", &["-Xmx2G"]);
        assert_eq!(start_spec(&renamed, Path::new("/work/a/app")).args.len(), 3);
        let own = spec_of("jdtls", "jdtls", &["-data", "/x"]);
        assert_eq!(start_spec(&own, Path::new("/p")).args, ["-data", "/x"]);
        // No home folder: nothing added (the launcher's own default)
        assert!(start_spec_with(&jdtls, traits(&jdtls), Path::new("/p"), None).args.is_empty());
        // Others untouched
        let ra = spec_of("rust-analyzer", "rust-analyzer", &[]);
        assert_eq!(start_spec(&ra, Path::new("/p")), ra);
    }

    /// jdtls's `language/status`, read by its pack's `progress`: what it's at until it's done — jdtls 1.61
    /// sends a last "100% Starting …" after "Started", and "ProjectStatus: OK" says nothing of progress.
    #[test]
    fn jdtls_status_is_progress_until_done() {
        let s = traits(&spec_of("jdtls", "jdtls", &[])).progress.as_ref().expect("the java pack says it");
        assert_eq!(s.method, "language/status");
        let text = |kind: &str, message: &str| s.text(&json!({ "type": kind, "message": message }));
        assert_eq!(text("Starting", "Init...").as_deref(), Some("Init..."));
        assert_eq!(
            text("Starting", "42% Importing Maven project(s)").as_deref(),
            Some("42% Importing Maven project(s)")
        );
        assert_eq!(text("Error", "no JDK").as_deref(), Some("error · no JDK"));
        for (kind, message) in [
            ("ServiceReady", "ServiceReady"),
            ("Started", "Ready"),
            ("Starting", "100% Starting Java Language Server"),
            ("ProjectStatus", "OK"),
        ] {
            assert_eq!(text(kind, message), None, "{kind} {message}");
        }
    }

    /// jdtls's own documents (`virtual`): a definition in a library's class is one; its title is the class.
    #[test]
    fn jdtls_classes_are_its_own_documents() {
        let v = traits(&spec_of("jdtls", "jdtls", &[])).virtual_docs.as_ref().expect("the java pack says it");
        assert_eq!((v.fetch.as_str(), v.language.as_str()), ("java/classFileContents", "java"));
        let uri = "jdt://contents/java.base/java.util/ArrayList.class?=jproj/%5C/usr/lib/jvm%3Cjava.util(ArrayList.class";
        let r = json!([{ "uri": uri, "range": { "start": { "line": 40, "character": 13 }, "end": {} } }]);
        let (u, pos) = v.location(&r).unwrap();
        assert_eq!((u.as_str(), pos["line"].as_u64()), (uri, Some(40)));
        assert!(v.location(&json!([{ "uri": "file:///a/B.java", "range": { "start": {} } }])).is_none());
        assert_eq!(v.title(uri), "java.util.ArrayList");
        assert_eq!(v.title("jdt://contents/rt.jar/java.util/Map%24Entry.class"), "java.util.Map$Entry");
        assert_eq!(v.title("jdt://contents/src.zip/java.util/List.java"), "java.util.List");
        assert_eq!(v.title("jdt://contents/Lone.class"), "Lone", "no package");
        let joined = Virtual {
            title_join: None,
            ..Virtual { scheme: "x:".into(), fetch: String::new(), language: String::new(), title_join: None }
        };
        assert_eq!(joined.title("x:/a/b/c.ts"), "c.ts", "no `title-join`: the last part as it is");
        let cmds = &traits(&spec_of("jdtls", "jdtls", &[])).client_commands;
        assert_eq!(cmds, &[("java.apply.workspaceEdit".to_string(), ClientAction::ApplyEdit)]);
    }

    /// `{installed:<tool>}` in a pack's `init`: where that tool is, or gone from its list (none here).
    #[test]
    fn installed_tools_fill_a_servers_options() {
        let init = json!({ "bundles": ["{installed:tarae-no-such-tool}", "kept.jar"], "one": "{installed:tarae-no-such-tool}", "n": 1 });
        assert_eq!(installed_filled(init), json!({ "bundles": ["kept.jar"], "one": null, "n": 1 }));
        let jdtls = &traits(&spec_of("jdtls", "jdtls", &[])).init;
        assert_eq!(jdtls["bundles"], json!(["{installed:java-debug}"]));
        assert_eq!(jdtls["extendedClientCapabilities"]["classFileContentsSupport"], json!(true));
    }

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
        let config = LspConfig::default();
        let first: Vec<String> = crate::syntax::names()
            .filter_map(|l| candidates(l, &config).into_iter().next())
            .map(|s| s.command)
            .collect();
        assert!(first.len() >= 15, "the packs' servers: {first:?}");
        for tool in first.iter().map(String::as_str).chain(["dlv", "lldb-dap"]) {
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
        let scratch = crate::scratch::Scratch::new("lsp-root");
        let t = scratch.to_path_buf();
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
        let scratch = crate::scratch::Scratch::new("lsp-nested");
        let t = scratch.to_path_buf();
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
        let ra = options_for(&spec("rust-analyzer", "rust-analyzer"), std::slice::from_ref(&root))();
        let mut ex = strs(&ra.init["files"]["exclude"]);
        ex.sort();
        assert_eq!(ex, [".claude/worktrees", "src/a/b/c4", "vendor/clone"]);
        assert_eq!(ra.config, Value::Null);

        // (found by its command: the java pack's jdtls)
        let jdtls = options_for(&spec("java", "/opt/jdtls/bin/jdtls"), std::slice::from_ref(&root))().init;
        assert!(jdtls["bundles"].is_array(), "{jdtls}");
        assert_eq!(
            jdtls["settings"]["java"]["maven"],
            json!({ "downloadSources": true }),
            "its pack's `init` kept beside what's left out"
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

        let py = options_for(&spec("py", "/usr/local/bin/pyright-langserver"), std::slice::from_ref(&root))();
        assert_eq!(py.init, Value::Null, "pyright asks for it");
        let ex = strs(&section(&py.config, Some("python.analysis"))["exclude"]);
        assert!(ex.starts_with(&["**/node_modules", "**/__pycache__", "**/.*"].map(String::from)), "{ex:?}");
        assert!(ex.contains(&"vendor/clone".to_string()) && ex.len() == 7, "{ex:?}");
        assert_eq!(
            section(&py.config, Some("python"))["analysis"]["exclude"].as_array().map(Vec::len),
            Some(7)
        );
        assert_eq!(section(&py.config, Some("pyright")), Value::Null);

        let gopls = options_for(&spec("gopls", "gopls"), std::slice::from_ref(&root))().init;
        let f = strs(&gopls["directoryFilters"]);
        assert!(
            f.first().is_some_and(|d| d == "-**/node_modules") && f.contains(&"-vendor/clone".into()),
            "{f:?}"
        );

        let taplo = options_for(&spec("taplo", "taplo"), std::slice::from_ref(&root))();
        assert_eq!((taplo.init, taplo.config), (Value::Null, Value::Null), "no leave-out in its pack");
        let plain = t.join("plain");
        std::fs::create_dir_all(plain.join("src")).unwrap();
        let ra = options_for(&spec("rust-analyzer", "rust-analyzer"), std::slice::from_ref(&plain))();
        assert_eq!(ra.init, Value::Null, "nothing nested: nothing said");
        // A shared server's second folder: its nested ones as whole paths (the server reads them from its first)
        let ra = options_for(&spec("rust-analyzer", "rust-analyzer"), &[plain.clone(), root.clone()])();
        let mut ex = strs(&ra.init["files"]["exclude"]);
        ex.sort();
        let whole: Vec<String> = [".claude/worktrees", "src/a/b/c4", "vendor/clone"]
            .map(|r| root.join(r).display().to_string())
            .into();
        assert_eq!(ex, whole);
        let _ = std::fs::remove_dir_all(&t);
    }
}
