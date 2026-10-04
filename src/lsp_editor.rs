//! Editor ↔ LSP — attaching servers, sending changes (at the end of each event batch), handling server
//! messages, requests (`gd` `gr` hover), diagnostic jumps. Late results (cursor moved/edited) are dropped.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::completion::{self, Completion};
use crate::document::DocId;
use crate::editor::Editor;
use crate::lsp::{self, Client, ClientId, Diagnostic, Encoding};
use crate::movement as mv;
use crate::picker::{Action, Item, Picker};
use crate::selection::{Range, Selection};

#[derive(Default)]
pub struct LspState {
    pub clients: Vec<Client>,
    next_id: ClientId,
    pub(crate) pending: HashMap<(ClientId, u64), Pending>,
    /// Last code action list, each with the server it came from (picker picks are taken from here) · new
    /// id per list · servers yet to answer for it · per-item previews.
    actions: Vec<(ClientId, Value)>,
    actions_gen: u64,
    actions_waiting: usize,
    /// Where the code actions were asked for (doc, version, cursor) — the list only shows there.
    actions_at: Option<(DocId, u64, usize)>,
    action_previews: HashMap<usize, ActionPreview>,
    /// The completion requests of the latest round still unanswered (one to each server that completes) —
    /// a new round waits for these, unless a trigger character asks anew.
    completion_waiting: Vec<(ClientId, u64)>,
    /// Where that round asked (doc, word start) — a new word asks again even if a server is still slow ·
    /// the round whose list showed (a later server's answer doesn't bring back a list the user closed).
    completion_at: Option<(DocId, usize)>,
    completion_shown: u64,
    /// Signature request in flight / more edits since (ask once more when the response arrives).
    signature_inflight: bool,
    signature_again: bool,
    /// Inlay hints: in-flight request (doc, version, line range) / waiting briefly (ask once input pauses).
    inlay_inflight: Option<(DocId, u64, (usize, usize))>,
    inlay_waiting: bool,
    /// The latest round of completion requests — its answers merge into one list (a late resolve for an
    /// older list is dropped).
    completion_gen: u64,
    /// Go-to target in a file still loading in the background — applied once it has loaded.
    jump_after_load: Option<(DocId, Value, Encoding)>,
    /// A `g d`/hover/`space h` a busy server (still indexing) had no answer for — asked again when its work
    /// ends, if the cursor is still where it was: (server, what, doc, version, cursor) · how many times it's
    /// been put off at that place (a server that never settles gets a plain "try again").
    ask_when_idle: Option<(ClientId, Kind, DocId, u64, usize)>,
    ask_tries: (Option<(DocId, u64, usize)>, u8),
    /// Servers that failed to start or exited, by name → why (the status line's red dot) · which server a
    /// language uses · languages whose servers aren't installed (faint dot; logged once).
    pub(crate) failed: HashMap<String, String>,
    server_for: HashMap<String, String>,
    missing: std::collections::HashSet<String>,
    /// Diagnostics for files that aren't open (`space D`): path → (server, severity, line, column, message).
    pub(crate) elsewhere: HashMap<std::path::PathBuf, Vec<Elsewhere>>,
    /// The go-to in hand, timed from its key to the frame that shows its answer (N1 — `tarae.log`).
    pub trace: Option<Trace>,
    /// When the message being handled was read off the server's output (its reader thread) — the time
    /// it then waited for the main loop is ours, not the server's.
    pub(crate) arrived: Option<std::time::Instant>,
    /// Servers no open file uses, since when (`lsp_unused` — they stop after `IDLE_STOP`).
    unused: HashMap<ClientId, std::time::Instant>,
    /// The `g r` in hand (N2): its list is laid out on a worker — the code of each place read there.
    refs: Refs,
    /// Claude's `references` calls waiting for their server (N5, `nav.rs`), by number · the next number.
    pub(crate) tool_asks: HashMap<u64, crate::nav::Ask>,
    pub(crate) tool_ask_next: u64,
    /// …and its `symbols` calls (N5b), by the same numbers.
    pub(crate) tool_symbols: HashMap<u64, crate::nav::SymAsk>,
    /// (tests: the trace lines written — the log writes nothing in tests)
    #[cfg(test)]
    pub traced: Vec<String>,
}

/// One `g r`: the name asked about · where it's declared (the server's definition, asked beside it — None
/// until it answers) · the list once shown (picker id and its places, to put the declaration first if
/// that answer comes later).
/// · while the server was loading, the text matches shown meanwhile (N3 — that picker's id).
#[derive(Default)]
struct Refs {
    generation: u64,
    name: String,
    declared: Option<Vec<(std::path::PathBuf, usize)>>,
    shown: Option<(u64, Vec<Place>)>,
    texts: Option<u64>,
}

/// A place of a list with the code there: path, line and byte column (from 0), the line's text.
#[derive(Clone, Debug)]
pub struct Place {
    pub path: std::path::PathBuf,
    pub line: usize,
    pub col: usize,
    pub text: String,
}

/// A line's text as a list shows it: trimmed, at most this many chars.
const PLACE_TEXT_MAX: usize = 240;

/// The places' lines — open files from their buffers (`open`, what's on screen even unsaved), the rest
/// read from disk (on a worker), each file once.
pub fn read_places(
    found: Vec<(std::path::PathBuf, usize, usize)>,
    open: &HashMap<std::path::PathBuf, ropey::Rope>,
) -> Vec<Place> {
    let mut files: HashMap<std::path::PathBuf, Option<Vec<String>>> = HashMap::new();
    found
        .into_iter()
        .map(|(path, line, col)| {
            let text = match open.get(&path) {
                Some(rope) => (line < rope.len_lines()).then(|| rope.line(line).to_string()),
                None => files
                    .entry(path.clone())
                    .or_insert_with(|| {
                        std::fs::read(&path)
                            .ok()
                            .map(|b| String::from_utf8_lossy(&b).lines().map(str::to_string).collect())
                    })
                    .as_ref()
                    .and_then(|lines| lines.get(line).cloned()),
            };
            let text: String = text.unwrap_or_default().trim().chars().take(PLACE_TEXT_MAX).collect();
            Place { path, line, col, text }
        })
        .collect()
}

/// The list's items: the declaration first (marked), the rest by file then line — each row its code.
fn place_items(
    root: &std::path::Path,
    mut places: Vec<Place>,
    declared: &[(std::path::PathBuf, usize)],
) -> Vec<Item> {
    let is_decl = |p: &Place| declared.iter().any(|(path, line)| *path == p.path && *line == p.line);
    places.sort_by(|a, b| (!is_decl(a), &a.path, a.line, a.col).cmp(&(!is_decl(b), &b.path, b.line, b.col)));
    places.dedup_by(|a, b| a.path == b.path && a.line == b.line && a.col == b.col);
    places
        .into_iter()
        .map(|p| {
            let rel = p.path.strip_prefix(root).unwrap_or(&p.path);
            Item {
                label: format!("{}:{}: {}", rel.display(), p.line + 1, p.text),
                // (short — the code keeps the room)
                hint: if is_decl(&p) { "decl".into() } else { String::new() },
                // Approximate column (UTF-16 may drift after non-ASCII; line is exact)
                action: Action::Goto { path: p.path, line: p.line, col: p.col },
                glyph: None,
            }
        })
        .collect()
}

/// One `g d`·`g r`… from its key to the first frame that shows the answer (N1): each step and what it
/// took, written to `tarae.log` as one line once that frame is drawn (`Editor::trace_drawn`) —
/// `references 1.84s · sent 3ms · waited for "cargo check" 1.20s · asked again 1ms · answered 600ms (412
/// places) · main loop 2ms · list 4ms · drawn 6ms`.
pub struct Trace {
    what: &'static str,
    start: std::time::Instant,
    last: std::time::Instant,
    steps: Vec<String>,
    /// The answer has been dealt with (listed, jumped to, nothing found): the next frame ends it.
    answered: bool,
}

impl Trace {
    fn new(what: &'static str) -> Trace {
        let now = std::time::Instant::now();
        Trace { what, start: now, last: now, steps: Vec::new(), answered: false }
    }

    /// A step done now — what it took is the time since the step before.
    fn step(&mut self, what: impl Into<String>) {
        self.step_at(what, std::time::Instant::now());
    }

    fn step_at(&mut self, what: impl Into<String>, at: std::time::Instant) {
        let took = at.saturating_duration_since(self.last);
        self.steps.push(format!("{} {}", what.into(), crate::term::duration(took.as_secs_f32())));
        self.last = self.last.max(at);
    }

    /// The log line.
    pub fn line(&self) -> String {
        let total = self.last.saturating_duration_since(self.start).as_secs_f32();
        format!("{} {} · {}", self.what, crate::term::duration(total), self.steps.join(" · "))
    }
}

/// A diagnostic in a file that isn't open: (server, severity, line, column, message).
pub(crate) type Elsewhere = (ClientId, u8, usize, usize, String);

/// What applying one code action would change (picker's right pane).
#[derive(Clone)]
pub enum ActionPreview {
    /// Asking the server for the edit (codeAction/resolve) / building the diff.
    Working,
    Ready(std::sync::Arc<Vec<crate::editdiff::FileDiff>>),
    /// Server command only, no edit — can't preview.
    CommandOnly,
    Failed(String),
}

/// How long a server stays up once no open file uses it while another file of its project is still open —
/// a file reopened soon after needn't wait for it to load again (rust-analyzer took minutes on this
/// repository — N1); after that it stops (it ran on for hours).
pub(crate) const IDLE_STOP: std::time::Duration =
    std::time::Duration::from_millis(if cfg!(test) { 60_000 } else { 10 * 60 * 1000 });

/// …and once no file under its root is open at all (#34 — taplo and bash-language-server for closed temp
/// folders stayed the full ten minutes): a grace for a quick reopen, then it stops.
const ROOT_GONE: std::time::Duration =
    std::time::Duration::from_millis(if cfg!(test) { 30 } else { 30 * 1000 });

/// How long `g r`'s text matches wait for a busy server's own answer before they show (N3) — one that
/// answers by then (warm rust-analyzer: 35–68 ms) isn't preceded by a flash of text matches.
const TEXT_GRACE: std::time::Duration = std::time::Duration::from_millis(150);

/// How often one question (same place, same text) is put off for a busy server before it's given up —
/// asked again at each piece of work that ends (rust-analyzer reports a dozen while loading a project).
const MAX_ASKS: u8 = 20;

/// Where `g d`·`g D`·`g y`·`g i`·`g r` go — one request shape, one reply handler.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Goto {
    Definition,
    Declaration,
    TypeDefinition,
    Implementation,
    References,
}

impl Goto {
    fn method(self) -> &'static str {
        match self {
            Goto::Definition => "textDocument/definition",
            Goto::Declaration => "textDocument/declaration",
            Goto::TypeDefinition => "textDocument/typeDefinition",
            Goto::Implementation => "textDocument/implementation",
            Goto::References => "textDocument/references",
        }
    }

    /// Server capability to check first — only the ones servers often lack (every server does
    /// definitions and references; some leave the capability out anyway).
    fn provider(self) -> Option<&'static str> {
        match self {
            Goto::Declaration => Some("declarationProvider"),
            Goto::TypeDefinition => Some("typeDefinitionProvider"),
            Goto::Implementation => Some("implementationProvider"),
            Goto::Definition | Goto::References => None,
        }
    }

    /// Picker title / what "nothing found" names.
    fn noun(self) -> &'static str {
        match self {
            Goto::Definition => "definitions",
            Goto::Declaration => "declarations",
            Goto::TypeDefinition => "type definitions",
            Goto::Implementation => "implementations",
            Goto::References => "references",
        }
    }
}

/// Language server state for the status line's dot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LspDot {
    Starting,
    Ready,
    Failed,
    /// Its language has servers, none installed.
    Missing,
}

/// Inlay hints: ask once input has paused this long.
const INLAY_DEBOUNCE_MS: u64 = 120;

/// What a request was for — plus what's needed to use its reply.
#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Goto(Goto),
    /// The definition of what `g r` round `generation` lists — its declaration goes first (N2).
    Declared {
        generation: u64,
    },
    /// Claude's `references` question `ask` (N5, `nav.rs`) — its references and its definition. Not tied to
    /// the cursor: answered wherever the user is by then.
    ToolRefs {
        ask: u64,
    },
    ToolDeclared {
        ask: u64,
    },
    /// Claude's `symbols` question `ask` (N5b) — a file's document symbols, wherever the user is by then.
    ToolSymbols {
        ask: u64,
    },
    /// `space h` — this symbol's uses in the file become the selections.
    Highlights,
    Hover,
    /// Code actions for list `list` (one request per server; the list shows when all have answered).
    CodeAction {
        list: u64,
    },
    ResolveAction,
    /// resolve for preview of action `index` in code action list `list` — fetch the edit only, don't execute.
    ResolvePreview {
        list: u64,
        index: usize,
    },
    ExecuteCommand,
    Rename,
    Format,
    /// `:w` with format-on-save: format, then save (and quit for `:wq`).
    FormatSave {
        quit: bool,
    },
    Completion,
    /// Docs/extra edits for item `index` of completion list `list`.
    ResolveCompletion {
        list: u64,
        index: usize,
    },
    SignatureHelp,
    /// Hints for this line range.
    InlayHint {
        range: (usize, usize),
    },
    DocumentSymbols,
    WorkspaceSymbols,
    /// Java debug prep (jdtls `vscode.java.*` commands) — java.rs.
    JavaDebug,
    /// Library/JDK class source (`java/classFileContents`, jdt:// definitions) — java.rs.
    ClassFile {
        uri: String,
        pos: Value,
    },
}

impl Kind {
    /// The `[lsp] except` feature and the server capability a request needs — None: it goes to a known
    /// server (resolves, commands).
    fn route(&self) -> Option<(&'static str, &'static str)> {
        Some(match self {
            Kind::Goto(Goto::References) => ("goto", "referencesProvider"),
            Kind::Goto(g) => ("goto", g.provider().unwrap_or("definitionProvider")),
            Kind::Declared { .. } | Kind::ToolDeclared { .. } => ("goto", "definitionProvider"),
            Kind::ToolRefs { .. } => ("goto", "referencesProvider"),
            Kind::Highlights => ("highlights", "documentHighlightProvider"),
            Kind::Hover => ("hover", "hoverProvider"),
            Kind::CodeAction { .. } => ("code-action", "codeActionProvider"),
            Kind::Rename => ("rename", "renameProvider"),
            Kind::Format | Kind::FormatSave { .. } => ("format", "documentFormattingProvider"),
            Kind::Completion => ("completion", "completionProvider"),
            Kind::SignatureHelp => ("signature-help", "signatureHelpProvider"),
            Kind::InlayHint { .. } => ("inlay-hints", "inlayHintProvider"),
            Kind::DocumentSymbols | Kind::ToolSymbols { .. } => ("symbols", "documentSymbolProvider"),
            Kind::WorkspaceSymbols => ("symbols", "workspaceSymbolProvider"),
            _ => return None,
        })
    }
}

/// A sent request — with what's needed to judge whether it's still valid when the response arrives.
#[derive(Clone, Debug)]
pub struct Pending {
    pub kind: Kind,
    doc: DocId,
    version: u64,
    head: usize,
}

impl Editor {
    pub(crate) fn client_mut(&mut self, cid: ClientId) -> Option<&mut Client> {
        self.lsp.clients.iter_mut().find(|c| c.id == cid)
    }

    pub(crate) fn client(&self, cid: ClientId) -> Option<&Client> {
        self.lsp.clients.iter().find(|c| c.id == cid)
    }

    /// Position encoding of server `cid` (UTF-16 until it says otherwise).
    pub(crate) fn encoding_of(&self, cid: ClientId) -> Encoding {
        self.client(cid).map_or(Encoding::Utf16, |c| c.encoding)
    }

    /// Attach the doc's language servers — its main one and those beside it (`[lang] also`), spawning them
    /// if needed; ones already attached stay. Startup/init are all async — no waiting. Also called once a
    /// background-loaded doc has its text (`finish_loading`), and to put back a restarted server.
    pub fn attach_lsp(&mut self, id: DocId) {
        self.jump_if_loaded(id);
        if !self.config.lsp.enabled {
            return;
        }
        let Some(doc) = self.docs.iter().find(|d| d.id == id) else { return };
        if doc.loading {
            return;
        }
        let Some(path) = doc.path.clone() else { return };
        let Some(spec) = crate::syntax::detect(&path) else { return };
        // No project for a server that needs one: none started (said once per folder, in the log — not a
        // message on screen at every open)
        if let Some(markers) = lsp::project_markers(&spec.name)
            && lsp::root_by_markers(&path, markers).is_none()
        {
            let dir = path.parent().unwrap_or(&path).display().to_string();
            if self.lsp.missing.insert(format!("{}:no project:{dir}", spec.name)) {
                let msg = format!(
                    "no {} above {dir} — no language server for {} files there (it needs a project)",
                    markers.join(" or "),
                    spec.name
                );
                crate::log::line("tarae", "info", &msg);
            }
            return;
        }
        let attached: Vec<String> =
            doc.lsp.clients().filter_map(|c| self.client(c).map(|c| c.name.clone())).collect();
        let has_main = doc.lsp.client.is_some();
        if !has_main {
            match lsp::find_server(&spec.name, &self.config.lsp) {
                Some(server) => {
                    self.lsp.server_for.insert(spec.name.clone(), server.name.clone());
                    if let Some(cid) = self.lsp_spawn(&server, &spec.name, &path) {
                        let u16 = self.client(cid).is_some_and(|c| c.ready && c.encoding == Encoding::Utf16);
                        let doc = self.docs.iter_mut().find(|d| d.id == id).expect("checked above");
                        doc.lsp.attach(cid, u16, true);
                        self.lsp_did_open(cid, id, &spec.name);
                    }
                }
                None => {
                    let tried: Vec<String> = lsp::candidates(&spec.name, &self.config.lsp)
                        .into_iter()
                        .map(|s| s.command)
                        .collect();
                    if !tried.is_empty() && self.lsp.missing.insert(spec.name.clone()) {
                        let msg = format!(
                            "no language server for {} — tried {} (not on PATH)",
                            spec.name,
                            tried.join(", ")
                        );
                        crate::log::line("tarae", "warning", &msg);
                        self.offer_install_server(&tried);
                    }
                }
            }
        }
        // The main one counts as attached too (it may be in `also` as well — never twice)
        let mut attached = attached;
        if let Some(c) =
            self.docs.iter().find(|d| d.id == id).and_then(|d| d.lsp.client).and_then(|c| self.client(c))
        {
            attached.push(c.name.clone());
        }
        let (also, absent) = lsp::also_servers(&spec.name, &self.config.lsp);
        for name in absent {
            if self.lsp.missing.insert(format!("{}+{name}", spec.name)) {
                crate::log::line(
                    "tarae",
                    "warning",
                    &format!("{name} (beside {}'s server) isn't on PATH", spec.name),
                );
            }
        }
        for server in also {
            if attached.contains(&server.name) {
                continue;
            }
            let Some(cid) = self.lsp_spawn(&server, &spec.name, &path) else { continue };
            let doc = self.docs.iter().find(|d| d.id == id).expect("checked above");
            if doc.lsp.clients().any(|c| c == cid) {
                continue;
            }
            let u16 = self.client(cid).is_some_and(|c| c.ready && c.encoding == Encoding::Utf16);
            let doc = self.docs.iter_mut().find(|d| d.id == id).expect("checked above");
            doc.lsp.attach(cid, u16, false);
            self.lsp_did_open(cid, id, &spec.name);
        }
    }

    /// The running server `server` for this file's project, or a new one — None if it couldn't start.
    fn lsp_spawn(
        &mut self,
        server: &lsp::ServerSpec,
        lang: &str,
        path: &std::path::Path,
    ) -> Option<ClientId> {
        let root = lsp::find_root_for(lang, path);
        if let Some(c) = self.lsp.clients.iter().find(|c| c.name == server.name && c.root == root) {
            return Some(c.id);
        }
        // It kept exiting: held until `r` in `:procs` (its red dot stays)
        if self.procs_held(&server.name, &root) {
            return None;
        }
        self.lsp.next_id += 1;
        let cid = self.lsp.next_id;
        let (server, options) = crate::java::prepare(server, &root);
        let options = lsp::options_for(&server, &root, options);
        match Client::start(cid, &server, &root, options, self.events.sender()) {
            Ok(c) => {
                // (started again after it exited: no longer the status line's red dot)
                self.lsp.failed.remove(&server.name);
                self.lsp.clients.push(c);
                Some(cid)
            }
            Err(e) => {
                self.lsp.failed.insert(server.name.clone(), e.clone());
                crate::log::line(&server.name, "error", &format!("couldn't start: {e}"));
                self.set_error(format!("{} couldn't start: {e} · :log-open", server.name));
                None
            }
        }
    }

    fn lsp_did_open(&mut self, cid: ClientId, id: DocId, lang: &str) {
        let Some(doc) = self.docs.iter().find(|d| d.id == id) else { return };
        let Some(path) = doc.path.as_deref() else { return };
        let (text, uri, lang) = (doc.text.clone(), lsp::uri(path), lsp::language_id(lang).to_string());
        let Some(client) = self.client_mut(cid) else { return };
        client.notify_lazy(move || {
            let params = json!({ "textDocument": { "uri": uri, "languageId": lang, "version": 0, "text": text.to_string() } });
            json!({ "jsonrpc": "2.0", "method": "textDocument/didOpen", "params": params }).to_string()
        });
    }

    /// Whether server `cid` may be asked for `feature` (`[lsp.<name>] except`).
    pub(crate) fn allows(&self, cid: ClientId, feature: &str) -> bool {
        let Some(c) = self.client(cid) else { return false };
        !self.config.lsp.servers.get(&c.name).is_some_and(|s| s.except.iter().any(|f| f == feature))
    }

    /// The current doc's server for a request of `kind`: the first attached one (main first) that's ready,
    /// may be asked, and says it can — else the main one, if it may be asked (some can more than they say).
    pub(crate) fn server_for(&self, kind: &Kind) -> Option<ClientId> {
        self.server_for_doc(self.doc().id, kind)
    }

    /// `server_for` for any open document.
    fn server_for_doc(&self, id: DocId, kind: &Kind) -> Option<ClientId> {
        let doc = self.docs.iter().find(|d| d.id == id)?;
        let Some((feature, cap)) = kind.route() else { return doc.lsp.client };
        let can = |cid: ClientId| {
            self.client(cid).is_some_and(|c| c.ready && !c.caps[cap].is_null() && c.caps[cap] != json!(false))
        };
        doc.lsp
            .clients()
            .find(|&c| self.allows(c, feature) && can(c))
            .or_else(|| doc.lsp.client.filter(|&c| self.allows(c, feature)))
    }

    /// Flush queued changes as didChange (at the end of each event batch — key bursts become one message),
    /// to every server a doc has, each from its own queue once it's ready.
    pub fn lsp_flush(&mut self) {
        let clients = &mut self.lsp.clients;
        for doc in &mut self.docs {
            let uri = || lsp::uri(doc.path.as_deref().unwrap_or(std::path::Path::new("")));
            for l in doc.lsp.links.iter_mut().filter(|l| l.behind()) {
                let Some(client) = clients.iter_mut().find(|c| c.id == l.client && c.ready) else { continue };
                l.u16 = client.encoding == Encoding::Utf16;
                l.version += 1;
                let changes = std::mem::take(&mut l.changes);
                let full = std::mem::take(&mut l.full_sync);
                send_changes(client, uri(), &doc.text, l.version, changes, full);
            }
        }
    }

    pub fn lsp_did_save(&mut self, id: DocId) {
        self.lsp_flush();
        let Some(doc) = self.docs.iter().find(|d| d.id == id) else { return };
        let Some(path) = doc.path.clone() else { return };
        let clients: Vec<ClientId> = doc.lsp.clients().collect();
        for cid in clients {
            if let Some(c) = self.client_mut(cid) {
                c.notify("textDocument/didSave", json!({ "textDocument": { "uri": lsp::uri(&path) } }));
            }
        }
    }

    /// One message from a server (reader thread → event → here, on the main loop).
    pub fn on_lsp_message(&mut self, cid: ClientId, mut msg: Value) {
        let method = msg["method"].as_str().map(str::to_string);
        match (method, msg.get("id")) {
            // Response to our request
            (None, Some(id)) => {
                let id = id.as_u64().unwrap_or(u64::MAX);
                if id == 0 {
                    let Some(c) = self.client_mut(cid) else { return };
                    c.on_initialized(&msg["result"]);
                    let (name, ms) = (c.name.clone(), c.started.elapsed().as_millis());
                    // Now the encoding is known — edits from here on carry UTF-16 columns if it needs them
                    let u16 = c.encoding == Encoding::Utf16;
                    for l in self.docs.iter_mut().flat_map(|d| &mut d.lsp.links).filter(|l| l.client == cid) {
                        l.u16 = u16;
                    }
                    crate::log::line(&name, "ready", &format!("after {ms} ms"));
                    self.lsp.failed.remove(&name);
                    self.set_success(format!("{name} ready"));
                    self.lsp_flush();
                    // A question put off while it started (N3): asked once it's up — by the end of the work
                    // it begins now (indexing), or after a moment if it begins none
                    if self.lsp.ask_when_idle.as_ref().is_some_and(|w| w.0 == cid) {
                        self.events.jobs().spawn(move || {
                            std::thread::sleep(std::time::Duration::from_millis(if cfg!(test) {
                                5
                            } else {
                                500
                            }));
                            move |ed: &mut Editor| {
                                if ed.client(cid).is_some_and(|c| c.progress.is_empty()) {
                                    ed.ask_again(cid);
                                }
                            }
                        });
                    }
                    return;
                }
                let Some(p) = self.lsp.pending.remove(&(cid, id)) else { return };
                let (error, result) = (msg.get("error"), &msg["result"]);
                match &p.kind {
                    Kind::JavaDebug => self.java_debug_reply(cid, id, error, result),
                    Kind::ClassFile { uri, pos } => {
                        self.class_file_reply(cid, uri.clone(), pos, error, result)
                    }
                    Kind::InlayHint { range } => self.on_inlay(cid, &p, *range, result),
                    &Kind::ToolRefs { ask } => {
                        crate::nav::refs_answered(self, ask, error.map_or(Ok(result), Err))
                    }
                    &Kind::ToolDeclared { ask } => {
                        crate::nav::declared_answered(self, ask, error.is_none().then_some(result))
                    }
                    &Kind::ToolSymbols { ask } => {
                        crate::nav::symbols_answered(self, ask, error.map_or(Ok(result), Err))
                    }
                    // One server failing leaves the others' actions (it's logged)
                    &Kind::CodeAction { list } if error.is_some() => {
                        let name = self.client(cid).map(|c| c.name.clone()).unwrap_or_default();
                        let why = error.and_then(|e| e["message"].as_str()).unwrap_or("failed");
                        crate::log::line(&name, "error", &format!("code actions: {why}"));
                        self.on_code_actions(cid, list, &Value::Null);
                    }
                    // The server couldn't format — save as is
                    &Kind::FormatSave { quit } if error.is_some() => {
                        self.format_save_done(p.doc, quit, Some("the server couldn't format — saved as is"))
                    }
                    Kind::SignatureHelp => self.on_signature(&p, result, error.is_some()),
                    Kind::Completion => {
                        // Only the latest round's answers (an older one's word is gone)
                        let n = self.lsp.completion_waiting.len();
                        self.lsp.completion_waiting.retain(|w| *w != (cid, id));
                        if error.is_none() && self.lsp.completion_waiting.len() < n {
                            // (moved, not copied — thousands of items, each with its edits and data)
                            self.on_completion(cid, &p, msg["result"].take());
                        }
                    }
                    // Preview failure stays in that pane (picking it asks again on execution)
                    &Kind::ResolvePreview { list, index } if error.is_some() => {
                        if list == self.lsp.actions_gen {
                            let m = error.and_then(|e| e["message"].as_str()).unwrap_or("request failed");
                            self.lsp.action_previews.insert(index, ActionPreview::Failed(m.to_string()));
                        }
                    }
                    // "Content modified" / "server cancelled": the project changed under the question (the
                    // server loading it) — not an error to show (the spec says so); asked again
                    Kind::Goto(_) | Kind::Hover | Kind::Highlights
                        if error.is_some_and(|e| matches!(e["code"].as_i64(), Some(-32801 | -32802))) =>
                    {
                        self.ask_again_later(cid, &p);
                    }
                    _ => match error {
                        Some(e) => self
                            .set_error(format!("lsp: {}", e["message"].as_str().unwrap_or("request failed"))),
                        None => self.on_response(cid, p, result),
                    },
                }
            }
            // A request from the server to us — some servers stall if unanswered.
            (Some(m), Some(id)) => {
                let result = match m.as_str() {
                    // Server says "hints changed" — drop all and ask again
                    "workspace/inlayHint/refresh" => {
                        for d in &mut self.docs {
                            d.lsp.inlay_have = None;
                        }
                        Value::Null
                    }
                    // What its pack set (`lsp::options_for` — pyright's `exclude`), null for the rest
                    "workspace/configuration" => {
                        let config =
                            self.client(cid).and_then(|c| c.config.get().cloned()).unwrap_or_default();
                        let items = msg["params"]["items"].as_array().cloned().unwrap_or_default();
                        Value::Array(
                            items.iter().map(|i| lsp::section(&config, i["section"].as_str())).collect(),
                        )
                    }
                    "workspace/workspaceFolders" => {
                        let root = self.lsp.clients.iter().find(|c| c.id == cid).map(|c| c.root.clone());
                        json!([{ "uri": root.as_deref().map(lsp::uri), "name": "root" }])
                    }
                    // Many code action commands send their edits this way.
                    "workspace/applyEdit" => match self.apply_workspace_edit(cid, &msg["params"]["edit"]) {
                        Ok(_) => json!({ "applied": true }),
                        Err(e) => {
                            self.set_error(format!("lsp edit: {e}"));
                            json!({ "applied": false, "failureReason": e })
                        }
                    },
                    _ => Value::Null,
                };
                let id = id.clone();
                if let Some(c) = self.client_mut(cid) {
                    c.reply(&id, result);
                }
            }
            (Some(m), None) => self.on_notification(cid, &m, &msg["params"]),
            (None, None) => {}
        }
    }

    fn on_notification(&mut self, cid: ClientId, method: &str, params: &Value) {
        match method {
            "textDocument/publishDiagnostics" => {
                if !self.allows(cid, "diagnostics") {
                    return;
                }
                let Some(path) = params["uri"].as_str().and_then(lsp::path_from_uri) else { return };
                let enc = self.encoding_of(cid);
                // Doc paths are canonical — only a URI matching no open doc as-is is resolved on disk
                let open = |p: &std::path::Path| self.docs.iter().position(|d| d.path.as_deref() == Some(p));
                let mut canonical = None; // (asked of the disk once — a flycheck sends a burst of these)
                let found =
                    open(&path).or_else(|| open(canonical.insert(std::fs::canonicalize(&path).ok()?)));
                let Some(i) = found else {
                    // Not open — kept for `space D`
                    let list: Vec<Elsewhere> = params["diagnostics"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .map(|d| {
                            let at = &d["range"]["start"];
                            let n = |k: &str| at[k].as_u64().unwrap_or(0) as usize;
                            let sev = d["severity"].as_u64().unwrap_or(1).clamp(1, 4) as u8;
                            (
                                cid,
                                sev,
                                n("line"),
                                n("character"),
                                d["message"].as_str().unwrap_or_default().trim().to_string(),
                            )
                        })
                        .collect();
                    // Each server's list replaces only its own
                    let path = canonical.unwrap_or(path);
                    let all = self.lsp.elsewhere.entry(path.clone()).or_default();
                    all.retain(|d| d.0 != cid);
                    all.extend(list);
                    if all.is_empty() {
                        self.lsp.elsewhere.remove(&path);
                    }
                    return;
                };
                let doc = &mut self.docs[i];
                let mut list: Vec<Diagnostic> = params["diagnostics"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|d| {
                        let from = lsp::from_position(&doc.text, &d["range"]["start"], enc)?;
                        let to = lsp::from_position(&doc.text, &d["range"]["end"], enc)?;
                        Some(Diagnostic {
                            from,
                            to: to.max(from),
                            severity: d["severity"].as_u64().unwrap_or(1).clamp(1, 4) as u8,
                            message: d["message"].as_str().unwrap_or_default().trim().to_string(),
                            raw: d.clone(),
                            source: cid,
                        })
                    })
                    .collect();
                // Each server's list replaces only its own
                list.extend(doc.lsp.diagnostics.drain(..).filter(|d| d.source != cid));
                list.sort_by_key(|d| (d.from, d.severity));
                doc.lsp.diagnostics = list;
            }
            "$/progress" => {
                let token = params["token"].to_string();
                let v = &params["value"];
                let Some(c) = self.client_mut(cid) else { return };
                match v["kind"].as_str() {
                    Some("end") => {
                        // (what it was busy with, and how long — a go-to may have waited on it)
                        if let Some(p) = c.progress.remove(&token)
                            && let Some(began) = p.began
                        {
                            let took = crate::term::duration(began.elapsed().as_secs_f32());
                            crate::log::line(&c.name, "progress", &format!("{} took {took}", p.title));
                        }
                        self.ask_again(cid);
                    }
                    Some("begin") => {
                        let mut p =
                            lsp::Progress { began: Some(std::time::Instant::now()), ..Default::default() };
                        p.update(v);
                        c.progress.insert(token, p);
                    }
                    Some(_) => c.progress.entry(token).or_default().update(v),
                    None => {}
                }
            }
            // jdtls startup progress (project import, indexing)
            "language/status" => {
                let Some(c) = self.client_mut(cid) else { return };
                let key = "language/status".to_string();
                // (jdtls says what it's at with each change — the log keeps them in order, timed)
                if let Some(text) = crate::java::status_text(params) {
                    crate::log::line(&c.name, "status", &text);
                }
                match crate::java::status_text(params) {
                    Some(title) => c.progress.insert(key, lsp::Progress { title, ..Default::default() }),
                    None => c.progress.remove(&key),
                };
                self.ask_again(cid);
            }
            "window/logMessage" => {
                let level = ["", "error", "warning", "info", "log"]
                    [params["type"].as_u64().unwrap_or(4).clamp(1, 4) as usize];
                let name = self.client(cid).map(|c| c.name.clone()).unwrap_or_default();
                crate::log::line(&name, level, params["message"].as_str().unwrap_or_default());
            }
            "window/showMessage" => {
                let m = params["message"].as_str().unwrap_or_default().to_string();
                if params["type"].as_u64() == Some(1) {
                    self.set_error(m);
                } else {
                    self.set_status(m);
                }
            }
            _ => {}
        }
    }

    /// Send a request at the cursor. If there's no server or it's still starting, say so and return false.
    pub fn lsp_request(&mut self, kind: Kind, method: &str, extra: Value) -> bool {
        let Some(cid) = self.ready_client(&kind) else { return false };
        let doc = self.doc();
        let Some(path) = &doc.path else { return false };
        let head = doc.selection().primary().cursor(&doc.text);
        let position = lsp::to_position(&doc.text, head, self.encoding_of(cid));
        let mut params = json!({ "textDocument": { "uri": lsp::uri(path) }, "position": position });
        if let (Value::Object(p), Value::Object(e)) = (&mut params, extra) {
            p.extend(e);
        }
        self.lsp_send(cid, kind, method, params).is_some()
    }

    /// The current doc's server for `kind` (`server_for`), if it's ready — else says why. Flushes pending
    /// edits first (else the server answers from the old doc — completion asks on new chars).
    fn ready_client(&mut self, kind: &Kind) -> Option<ClientId> {
        self.lsp_flush();
        if self.doc().lsp.clients().next().is_none() {
            self.set_status("no language server for this file");
            return None;
        }
        let Some(cid) = self.server_for(kind) else {
            self.set_status("no language server here does that");
            return None;
        };
        let c = self.client(cid)?;
        if !c.ready {
            let name = c.name.clone();
            self.set_status(format!("{name} is starting…"));
            return None;
        }
        Some(cid)
    }

    fn on_response(&mut self, cid: ClientId, p: Pending, result: &Value) {
        // Late response: drop if the doc changed (edit kinds) or the cursor moved (position kinds).
        let Some(doc) = self.docs.iter().find(|d| d.id == p.doc) else { return };
        let cursor_bound = matches!(
            p.kind,
            Kind::Goto(_) | Kind::Declared { .. } | Kind::Highlights | Kind::Hover | Kind::CodeAction { .. }
        );
        let version_bound = !matches!(
            p.kind,
            Kind::ResolvePreview { .. }
                | Kind::ExecuteCommand
                | Kind::ResolveCompletion { .. }
                | Kind::WorkspaceSymbols
        );
        let moved = self.doc().id != p.doc || doc.selection().primary().cursor(&doc.text) != p.head;
        if (version_bound && doc.version() != p.version) || (cursor_bound && moved) {
            // Moved on: the list being gathered isn't wanted any more
            if matches!(p.kind, Kind::CodeAction { .. }) {
                self.lsp.actions_waiting = 0;
            }
            // Typed on after :w — save what's there now, unformatted (a save is never lost)
            if let Kind::FormatSave { quit } = p.kind {
                return self.format_save_done(p.doc, quit, Some("changed while formatting — saved as is"));
            }
            if p.kind == Kind::ResolveAction {
                self.set_status("code action dropped — the file changed meanwhile");
            }
            return;
        }
        let enc = self.encoding_of(cid);
        match p.kind {
            Kind::CodeAction { list } => self.on_code_actions(cid, list, result),
            Kind::ResolveAction => self.run_code_action_value(cid, result.clone(), false),
            Kind::ResolvePreview { list, index } => {
                // Keep the received edit in the list — picking it applies directly without asking again
                if list == self.lsp.actions_gen
                    && let Some((_, slot)) = self.lsp.actions.get_mut(index)
                {
                    if result.is_object() {
                        *slot = result.clone();
                    }
                    self.lsp.action_previews.remove(&index);
                    self.code_action_preview_of(index, false);
                }
            }
            Kind::ResolveCompletion { list, index } => {
                if let Some(c) = self.completion.as_mut().filter(|c| c.list == list) {
                    c.merge_resolved(index, result);
                    self.completion_docs();
                }
            }
            // Nothing to do with the reply / handled in on_lsp_message
            Kind::ExecuteCommand
            | Kind::Completion
            | Kind::SignatureHelp
            | Kind::InlayHint { .. }
            | Kind::JavaDebug
            | Kind::ClassFile { .. }
            | Kind::ToolRefs { .. }
            | Kind::ToolDeclared { .. }
            | Kind::ToolSymbols { .. } => {}
            Kind::Rename => match self.apply_workspace_edit(cid, result) {
                Ok(n) => self.set_status(format!("renamed in {n} file(s)")),
                Err(e) => self.set_error(format!("rename: {e}")),
            },
            Kind::FormatSave { quit } => {
                let edits = result.as_array().cloned().unwrap_or_default();
                let why =
                    self.apply_text_edits(p.doc, enc, &edits).err().map(|e| format!("not formatted: {e}"));
                self.format_save_done(p.doc, quit, why.as_deref());
            }
            Kind::Format => {
                let edits = result.as_array().cloned().unwrap_or_default();
                if edits.is_empty() {
                    return self.set_status("already formatted");
                }
                match self.apply_text_edits(p.doc, enc, &edits) {
                    Ok(()) => self.set_status("formatted"),
                    Err(e) => self.set_error(format!("format: {e}")),
                }
            }
            Kind::Hover => {
                let text = lsp::hover_text(result);
                if text.is_empty() {
                    if !self.wait_for_idle(cid, &p) {
                        self.set_status("no hover information");
                    }
                } else {
                    let lang = self.doc().syntax.as_ref().map(|s| s.lang.name.clone());
                    self.popup = Some(crate::markdown::render(&text, &self.theme, lang.as_deref()));
                }
            }
            Kind::DocumentSymbols => self.on_document_symbols(p.doc, result),
            Kind::WorkspaceSymbols => self.on_workspace_symbols(result),
            Kind::Highlights => {
                let doc = self.doc();
                let ranges: Vec<Range> = result
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|h| {
                        let from = lsp::from_position(&doc.text, &h["range"]["start"], enc)?;
                        let to = lsp::from_position(&doc.text, &h["range"]["end"], enc)?;
                        (to > from).then(|| Range::new(from, to))
                    })
                    .collect();
                if ranges.is_empty() {
                    if self.wait_for_idle(cid, &p) {
                        return;
                    }
                    return self.set_status("no other uses of this symbol here");
                }
                let n = ranges.len();
                let primary = ranges.iter().position(|r| r.from() <= p.head && p.head < r.to()).unwrap_or(0);
                self.doc_mut().set_selection(Selection::new(ranges, primary));
                self.note(format!("{n} uses selected"));
            }
            Kind::Goto(goto) => {
                // (the server's time ends where its reader read the answer; the rest is ours)
                let arrived = self.lsp.arrived.take();
                if let Some(t) = self.lsp.trace.as_mut() {
                    let n = result.as_array().map_or(usize::from(!result.is_null()), Vec::len);
                    t.step_at(format!("answered ({n})"), arrived.unwrap_or_else(std::time::Instant::now));
                    t.step("main loop");
                }
                // jdtls: library/JDK classes come as jdt:// URIs — source into a read-only buffer (java.rs)
                if goto != Goto::References
                    && let Some((uri, pos)) = crate::java::jdt_location(result)
                {
                    self.push_jump();
                    self.trace_ends("class file asked");
                    return self.open_class_file(cid, uri, pos);
                }
                let locs = lsp::locations(result);
                match locs.as_slice() {
                    [] if self.wait_for_idle(cid, &p) => {}
                    [] => {
                        self.trace_ends("none");
                        self.set_status(format!("no {} found", goto.noun()))
                    }
                    // One place: go straight there (references always list — even one is worth seeing)
                    [(path, pos)] if goto != Goto::References => {
                        self.lsp_jump(path, pos, enc);
                        self.trace_ends("jumped");
                    }
                    _ => {
                        let found = locs
                            .iter()
                            .map(|(path, pos)| {
                                let line = pos["line"].as_u64().unwrap_or(0) as usize;
                                (path.clone(), line, pos["character"].as_u64().unwrap_or(0) as usize)
                            })
                            .collect();
                        self.list_places(goto, found);
                    }
                }
            }
            Kind::Declared { generation } => {
                if generation != self.lsp.refs.generation {
                    return;
                }
                let declared: Vec<(std::path::PathBuf, usize)> = lsp::locations(result)
                    .into_iter()
                    .map(|(path, pos)| (path, pos["line"].as_u64().unwrap_or(0) as usize))
                    .collect();
                // The list already on screen: its declaration goes first now
                if let Some((id, places)) = self.lsp.refs.shown.clone()
                    && let Some(p) = self.picker.as_mut().filter(|p| p.id == id)
                {
                    let root = std::env::current_dir().unwrap_or_default();
                    p.set_items(place_items(&root, places, &declared));
                }
                // (none while the server is busy: asked again with the references)
                self.lsp.refs.declared = (!declared.is_empty()).then_some(declared);
            }
        }
    }

    /// Any request to this server, remembered against the current doc (version, cursor) — returns the
    /// request id. None if the server is gone or not ready.
    pub(crate) fn lsp_send(&mut self, cid: ClientId, kind: Kind, method: &str, params: Value) -> Option<u64> {
        self.lsp_send_for(self.doc().id, cid, kind, method, params)
    }

    /// A request about document `id` (not necessarily the current one — `:wa` formats every file).
    fn lsp_send_for(
        &mut self,
        id: DocId,
        cid: ClientId,
        kind: Kind,
        method: &str,
        params: Value,
    ) -> Option<u64> {
        let doc = self.docs.iter().find(|d| d.id == id)?;
        let head = doc.selection().primary().cursor(&doc.text);
        let p = Pending { kind, doc: doc.id, version: doc.version(), head };
        let id = self.client_mut(cid).filter(|c| c.ready)?.request(method, params);
        self.lsp.pending.insert((cid, id), p);
        Some(id)
    }

    // ── Symbol search ─────────────────────────────────────────────────────

    /// Request to the current doc's server without a position (symbols etc.).
    fn lsp_request_raw(&mut self, kind: Kind, method: &str, params: Value) -> bool {
        let Some(cid) = self.ready_client(&kind) else { return false };
        self.lsp_send(cid, kind, method, params).is_some()
    }

    /// `space s` — symbols in this file (functions·types·fields …).
    pub fn document_symbols(&mut self) {
        let Some(path) = self.doc().path.clone() else { return self.set_status("no file") };
        self.lsp_request_raw(
            Kind::DocumentSymbols,
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": lsp::uri(&path) } }),
        );
    }

    /// Symbols of doc `doc_id` — dropped if another buffer is showing by now.
    fn on_document_symbols(&mut self, doc_id: DocId, result: &Value) {
        if self.doc().id != doc_id {
            return;
        }
        let Some(path) = self.doc().path.clone() else { return };
        let mut items = Vec::new();
        let mut lines = Vec::new();
        fn walk(
            v: &Value,
            parents: &[String],
            path: &std::path::Path,
            items: &mut Vec<Item>,
            lines: &mut Vec<usize>,
        ) {
            for s in v.as_array().into_iter().flatten() {
                let name = s["name"].as_str().unwrap_or_default().to_string();
                // DocumentSymbol (hierarchical) → selectionRange, SymbolInformation (flat) → location.range
                let start = if s["selectionRange"].is_object() {
                    &s["selectionRange"]["start"]
                } else {
                    &s["location"]["range"]["start"]
                };
                let line = start["line"].as_u64().unwrap_or(0) as usize;
                let col = start["character"].as_u64().unwrap_or(0) as usize;
                let container = s["containerName"].as_str().filter(|c| !c.is_empty()).map(str::to_string);
                let hint = container.unwrap_or_else(|| parents.join(" › "));
                items.push(Item {
                    label: name.clone(),
                    action: Action::Goto { path: path.to_path_buf(), line, col },
                    hint,
                    glyph: Some(symbol_glyph(s["kind"].as_u64().unwrap_or(0))),
                });
                lines.push(line);
                if s["children"].is_array() {
                    let mut p = parents.to_vec();
                    p.push(name);
                    walk(&s["children"], &p, path, items, lines);
                }
            }
        }
        walk(result, &[], &path, &mut items, &mut lines);
        if items.is_empty() {
            return self.set_status("no symbols in this file");
        }
        // Start at the symbol enclosing (or just before) the cursor
        let doc = self.doc();
        let cur = doc.text.byte_to_line(doc.selection().primary().head.min(doc.text.len_bytes()));
        let start = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| **l <= cur)
            .max_by_key(|(_, l)| **l)
            .map_or(0, |(i, _)| i);
        let mut p = Picker::new(format!("symbols in {}", doc.display_name_short()), items, false);
        p.selected = start;
        self.open_picker(p, None);
    }

    /// `space S` — workspace-wide symbols. Re-asks the server as you type (after a short pause).
    pub fn workspace_symbols(&mut self) {
        if self.doc().lsp.clients().next().is_none() {
            return self.set_status("no language server for this file");
        }
        let mut p = Picker::new("workspace symbols", Vec::new(), false);
        p.requery = true;
        p.loading = true;
        self.open_picker(p, None);
        self.lsp_request_raw(Kind::WorkspaceSymbols, "workspace/symbol", json!({ "query": "" }));
    }

    /// The workspace symbol picker's query changed — re-ask after a 120 ms pause.
    pub fn workspace_symbols_requery(&mut self) {
        let Some(query) = self.picker.as_ref().filter(|p| p.requery).map(|p| p.query.clone()) else { return };
        self.events.jobs().spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(120));
            move |ed: &mut Editor| {
                if ed.picker.as_ref().is_some_and(|p| p.requery && p.query == query) {
                    ed.lsp_request_raw(Kind::WorkspaceSymbols, "workspace/symbol", json!({ "query": query }));
                }
            }
        });
    }

    fn on_workspace_symbols(&mut self, result: &Value) {
        let Some(p) = self.picker.as_mut().filter(|p| p.requery) else { return };
        let root = std::env::current_dir().unwrap_or_default();
        let items: Vec<Item> = result
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| {
                let loc = &s["location"];
                let path = lsp::path_from_uri(loc["uri"].as_str()?)?;
                let start = &loc["range"]["start"];
                let line = start["line"].as_u64().unwrap_or(0) as usize;
                let col = start["character"].as_u64().unwrap_or(0) as usize;
                let rel = path.strip_prefix(&root).unwrap_or(&path).display().to_string();
                let container = s["containerName"].as_str().filter(|c| !c.is_empty());
                let hint = match container {
                    Some(c) => format!("{c} · {rel}:{}", line + 1),
                    None => format!("{rel}:{}", line + 1),
                };
                Some(Item {
                    label: s["name"].as_str()?.to_string(),
                    action: Action::Goto { path, line, col },
                    hint,
                    glyph: Some(symbol_glyph(s["kind"].as_u64().unwrap_or(0))),
                })
            })
            .collect();
        p.loading = false;
        p.set_items(items);
    }

    // ── Autocompletion ───────────────────────────────────────────────────

    /// A char was typed (`Some`) or deleted (`None`) in insert mode: filter the open list, re-ask, or close.
    pub fn completion_after_edit(&mut self, typed: Option<char>) {
        if self.mode != crate::editor::Mode::Insert {
            self.completion = None;
            return;
        }
        let doc = self.doc();
        let head = doc.selection().primary().head;
        let start = completion::word_start(&doc.text, head);
        let prefix = doc.text.byte_slice(start..head).to_string();
        let doc_id = doc.id;
        // Open list: filter while in the same word, close when leaving it
        if let Some(c) = &mut self.completion {
            let same_word = c.doc == doc_id && c.start == start && head >= c.start;
            if same_word && typed.is_none_or(completion::is_word) {
                c.filter(&prefix);
                let incomplete = c.incomplete;
                if self.completion.as_ref().is_some_and(|c| c.shown.is_empty()) && !incomplete {
                    self.completion = None;
                    // (filtered away, not closed — another server's answer is still wanted)
                    self.lsp.completion_shown = 0;
                }
                if !incomplete {
                    return;
                }
            } else {
                self.completion = None;
            }
        }
        let Some(t) = typed else { return };
        let servers = self.completion_servers();
        let triggers = |cid: ClientId| {
            self.client(cid)
                .is_some_and(|c| lists_char(&c.caps["completionProvider"]["triggerCharacters"], t))
        };
        let triggered: Vec<ClientId> = servers.iter().copied().filter(|&c| triggers(c)).collect();
        // A trigger char asks anew — the servers it triggers (an answer for the word before is dropped); a
        // word char asks every server once this word's round isn't in flight (a slow server's answer for an
        // earlier word doesn't hold a new one back) — the rest too when it triggered some
        let new_word = self.lsp.completion_at != Some((doc_id, start));
        let word = completion::is_word(t)
            && !prefix.is_empty()
            && (self.lsp.completion_waiting.is_empty() || new_word);
        if triggered.is_empty() && !word {
            return;
        }
        let trigger = json!({ "triggerKind": 2, "triggerCharacter": t.to_string() });
        let ask: Vec<(ClientId, Value)> = servers
            .into_iter()
            .filter_map(|c| match triggered.contains(&c) {
                true => Some((c, trigger.clone())),
                false => word.then(|| (c, json!({ "triggerKind": 1 }))),
            })
            .collect();
        self.completion_ask(ask);
    }

    /// The current doc's servers that complete (ready, not told `except = ["completion"]`), main first.
    fn completion_servers(&self) -> Vec<ClientId> {
        self.doc()
            .lsp
            .clients()
            .filter(|&cid| {
                self.allows(cid, "completion")
                    && self.client(cid).is_some_and(|c| {
                        let p = &c.caps["completionProvider"];
                        c.ready && !p.is_null() && *p != json!(false)
                    })
            })
            .collect()
    }

    /// A new round: ask each server (with its context) for completions at the cursor.
    fn completion_ask(&mut self, ask: Vec<(ClientId, Value)>) -> bool {
        self.lsp_flush();
        let doc = self.doc();
        let Some(path) = doc.path.clone() else { return false };
        let head = doc.selection().primary().cursor(&doc.text);
        let text = doc.text.clone();
        self.lsp.completion_at =
            Some((doc.id, completion::word_start(&text, doc.selection().primary().head)));
        self.lsp.completion_gen += 1;
        self.lsp.completion_waiting.clear();
        for (cid, context) in ask {
            let position = lsp::to_position(&text, head, self.encoding_of(cid));
            let params = json!({ "textDocument": { "uri": lsp::uri(&path) }, "position": position, "context": context });
            if let Some(id) = self.lsp_send(cid, Kind::Completion, "textDocument/completion", params) {
                self.lsp.completion_waiting.push((cid, id));
            }
        }
        !self.lsp.completion_waiting.is_empty()
    }

    /// Fill the selected item's docs pane — if it has none, ask the server to resolve once.
    pub fn completion_docs(&mut self) {
        let lang = self.doc().syntax.as_ref().map_or_else(String::new, |s| s.lang.name.clone());
        let Some(c) = &mut self.completion else { return };
        let Some(index) = c.current_index() else {
            c.docs = None;
            return;
        };
        if c.docs.as_ref().is_some_and(|(i, _)| *i == index) {
            return;
        }
        let list = c.list;
        let Some(item) = c.item_mut(index) else { return };
        let cid = item.client;
        let can_resolve = self
            .lsp
            .clients
            .iter()
            .find(|x| x.id == cid)
            .is_some_and(|x| x.caps["completionProvider"]["resolveProvider"].as_bool() == Some(true));
        if item.raw["documentation"].is_null() && !item.resolving && can_resolve {
            item.resolving = true;
            let raw = item.raw.clone();
            self.lsp_send(cid, Kind::ResolveCompletion { list, index }, "completionItem/resolve", raw);
        }
        let Some(c) = &mut self.completion else { return };
        let md = c.item_mut(index).map(|it| completion::doc_markdown(&it.raw, &lang)).unwrap_or_default();
        let lines = crate::markdown::render(&md, &self.theme, Some(&lang));
        c.docs = Some((index, lines));
    }

    // ── Inlay hints ───────────────────────────────────────────────────────

    /// Line range to request for the current screen (visible area + one screen above and below).
    fn inlay_range(&self) -> (usize, usize) {
        let doc = self.doc();
        let rows = self.viewport.0.max(1);
        (doc.top.saturating_sub(rows), doc.top + 2 * rows)
    }

    /// Every event: if our hints are stale (version differs or don't cover the visible range), ask shortly —
    /// while typing, wait and ask once it pauses (don't hammer the server on every key).
    pub fn inlay_schedule(&mut self) {
        if !self.config.inlay_hints {
            if !self.doc().lsp.inlay.is_empty() {
                self.doc_mut().lsp.inlay.clear();
                self.doc_mut().lsp.inlay_have = None;
            }
            return;
        }
        if self.lsp.inlay_waiting || !self.inlay_stale() {
            return;
        }
        self.lsp.inlay_waiting = true;
        let version = self.doc().version();
        self.events.jobs().spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(INLAY_DEBOUNCE_MS));
            move |ed: &mut Editor| {
                ed.lsp.inlay_waiting = false;
                // Typed again meanwhile — wait once more
                if ed.doc().version() != version {
                    return ed.inlay_schedule();
                }
                ed.inlay_request();
            }
        });
    }

    pub(crate) fn inlay_stale(&self) -> bool {
        let Some(cid) = self.server_for(&Kind::InlayHint { range: (0, 0) }) else { return false };
        let doc = self.doc();
        let ready = self.lsp.clients.iter().find(|c| c.id == cid).is_some_and(|c| {
            c.ready && !c.caps["inlayHintProvider"].is_null() && c.caps["inlayHintProvider"] != json!(false)
        });
        if !ready || doc.loading {
            return false;
        }
        let (a, b) = self.inlay_range();
        let covers = |(v, (x, y)): (u64, (usize, usize))| {
            v == doc.version() && x <= a.max(doc.top) && y >= b.min(doc.top + self.viewport.0)
        };
        let inflight = self.lsp.inlay_inflight.is_some_and(|(d, v, r)| d == doc.id && covers((v, r)));
        !inflight && !doc.lsp.inlay_have.is_some_and(covers)
    }

    pub fn inlay_request(&mut self) {
        if !self.inlay_stale() {
            return;
        }
        self.lsp_flush();
        let (a, b) = self.inlay_range();
        let cid = self.server_for(&Kind::InlayHint { range: (a, b) });
        let doc = self.doc();
        let (Some(cid), Some(path), id, version) = (cid, doc.path.clone(), doc.id, doc.version()) else {
            return;
        };
        let last = mv::last_line(&doc.text);
        let range = json!({ "start": { "line": a.min(last), "character": 0 }, "end": { "line": (b + 1).min(last + 1), "character": 0 } });
        let params = json!({ "textDocument": { "uri": lsp::uri(&path) }, "range": range });
        if self.lsp_send(cid, Kind::InlayHint { range: (a, b) }, "textDocument/inlayHint", params).is_some() {
            self.lsp.inlay_inflight = Some((id, version, (a, b)));
        }
    }

    /// Hint response: drop if the doc changed since (shifted old hints hold their place until new ones come).
    fn on_inlay(&mut self, cid: ClientId, p: &Pending, range: (usize, usize), result: &Value) {
        self.lsp.inlay_inflight = None;
        let enc = self.encoding_of(cid);
        let Some(doc) = self.docs.iter_mut().find(|d| d.id == p.doc) else { return };
        if doc.version() != p.version {
            return;
        }
        doc.lsp.inlay = lsp::inlay_hints(&doc.text, result, enc);
        doc.lsp.inlay_have = Some((p.version, range));
    }

    // ── Signature help ────────────────────────────────────────────────────

    /// After a key in insert mode: ask on trigger chars (`(` `,` …); while shown, re-ask on every edit/move
    /// (the server reports when the current argument changes or the parens are left — empty reply = close).
    pub fn signature_after_key(&mut self, typed: Option<char>) {
        let Some(cid) = self.server_for(&Kind::SignatureHelp) else { return };
        let provider = self.client(cid).map(|c| c.caps["signatureHelpProvider"].clone());
        let Some(provider) = provider.filter(|p| !p.is_null()) else { return };
        let listed = |key: &str, t: char| lists_char(&provider[key], t);
        let trigger = typed.filter(|&t| listed("triggerCharacters", t));
        let open = self.signature.is_some();
        if trigger.is_none() && !open {
            return;
        }
        if self.lsp.signature_inflight {
            self.lsp.signature_again = true;
            return;
        }
        let context = match (trigger, typed) {
            (Some(t), _) => {
                json!({ "triggerKind": 2, "triggerCharacter": t.to_string(), "isRetrigger": open })
            }
            (None, Some(t)) if listed("retriggerCharacters", t) => {
                json!({ "triggerKind": 2, "triggerCharacter": t.to_string(), "isRetrigger": true })
            }
            _ => json!({ "triggerKind": 3, "isRetrigger": true }),
        };
        if self.lsp_request(Kind::SignatureHelp, "textDocument/signatureHelp", json!({ "context": context }))
        {
            self.lsp.signature_inflight = true;
        }
    }

    /// Signature response: if the doc changed since the request, drop it (edits happened) and ask once more.
    fn on_signature(&mut self, p: &Pending, result: &Value, failed: bool) {
        self.lsp.signature_inflight = false;
        let again = std::mem::take(&mut self.lsp.signature_again);
        if self.mode != crate::editor::Mode::Insert || self.doc().id != p.doc {
            return;
        }
        if self.doc().version() != p.version || again {
            if self.signature.is_some() || !failed {
                let context = json!({ "triggerKind": 3, "isRetrigger": self.signature.is_some() });
                let params = json!({ "context": context });
                if self.lsp_request(Kind::SignatureHelp, "textDocument/signatureHelp", params) {
                    self.lsp.signature_inflight = true;
                }
            }
            return;
        }
        self.signature = if failed { None } else { crate::signature::Signature::from_lsp(p.doc, result) };
    }

    /// Invoke explicitly (insert-mode `C-x`) — asks even with an empty word; says why when no server can.
    pub fn completion_request(&mut self) {
        self.completion = None;
        let Some(main) = self.ready_client(&Kind::Completion) else { return };
        // (none says it completes: the main one's asked anyway — some can more than they say)
        let mut servers = self.completion_servers();
        if servers.is_empty() {
            servers.push(main);
        }
        let ask = servers.into_iter().map(|c| (c, json!({ "triggerKind": 1 }))).collect();
        self.completion_ask(ask);
    }

    /// Completion response — accepted and filtered even if more was typed since, if still in the same word
    /// (a plain "drop if the cursor moved" would mean the list never shows during fast typing).
    fn on_completion(&mut self, cid: ClientId, p: &Pending, result: Value) {
        if self.mode != crate::editor::Mode::Insert || self.doc().id != p.doc {
            return;
        }
        let doc = self.doc();
        let head = doc.selection().primary().head;
        let start = completion::word_start(&doc.text, p.head);
        if head < start || completion::word_start(&doc.text, head) != start {
            return;
        }
        let prefix = doc.text.byte_slice(start..head).to_string();
        let round = self.lsp.completion_gen;
        // Another server's answer in this round joins the list showing — or stays away, if it was closed
        if let Some(c) =
            self.completion.as_mut().filter(|c| c.list == round && c.doc == p.doc && c.start == start)
        {
            c.merge(cid, result, &prefix);
            return;
        }
        if self.lsp.completion_shown == round {
            return;
        }
        let mut c = Completion::new(cid, p.doc, start, result);
        c.list = round;
        c.filter(&prefix);
        if !c.shown.is_empty() {
            self.lsp.completion_shown = round;
            self.completion = Some(c);
        }
    }

    /// Insert the selected item (snippet expanded, cursor at first placeholder, extra edits like imports).
    pub fn completion_accept(&mut self) {
        let Some(c) = self.completion.take() else { return };
        let Some(item) = c.current() else { return };
        let raw = item.raw.clone();
        let enc = self.encoding_of(item.client);
        self.with_group(|cx| {
            let doc = cx.editor.doc_mut();
            let head = doc.selection().primary().head;
            let edit = &raw["textEdit"];
            let text = edit["newText"]
                .as_str()
                .or_else(|| raw["insertText"].as_str())
                .or_else(|| raw["label"].as_str())
                .unwrap_or_default();
            let (plain, cursor) = if raw["insertTextFormat"].as_u64() == Some(2) {
                completion::strip_snippet(text)
            } else {
                (text.to_string(), text.len())
            };
            // Start of the server-given range (else word start) .. current cursor
            let range_start = edit["range"]["start"]
                .is_object()
                .then(|| &edit["range"]["start"])
                .or_else(|| edit["insert"]["start"].is_object().then(|| &edit["insert"]["start"]));
            let from = range_start
                .and_then(|s| lsp::from_position(&doc.text, s, enc))
                .map_or(c.start, |f| f.min(c.start));
            let replaced = (head.saturating_sub(from), plain.clone(), cursor);
            let mut changes = vec![crate::transaction::Change { from, to: head, insert: plain }];
            for e in raw["additionalTextEdits"].as_array().into_iter().flatten() {
                if let (Some(a), Some(b), Some(t)) = (
                    lsp::from_position(&doc.text, &e["range"]["start"], enc),
                    lsp::from_position(&doc.text, &e["range"]["end"], enc),
                    e["newText"].as_str(),
                ) {
                    changes.push(crate::transaction::Change { from: a, to: b.max(a), insert: t.to_string() });
                }
            }
            let tx = crate::transaction::Transaction::new(changes);
            let pos = tx.map_pos(from, crate::transaction::Assoc::Before) + cursor;
            doc.apply_with(&tx, Selection::point(pos));
            // `.` replays this as the text it put in (repeat.rs)
            let (replace, text, cursor) = replaced;
            cx.editor.insert_completion(replace, text, cursor);
        });
    }

    // ── Applying edits (shared by code actions, rename, format) ─────────────

    /// TextEdit[] → one transaction (one undo per doc). Ranges use this server's position encoding.
    pub(crate) fn apply_text_edits(
        &mut self,
        doc_id: DocId,
        enc: Encoding,
        edits: &[Value],
    ) -> Result<(), String> {
        let doc = self.docs.iter_mut().find(|d| d.id == doc_id).ok_or("buffer closed")?;
        if doc.loading {
            return Err("still loading".into());
        }
        let changes = edits
            .iter()
            .filter_map(|e| {
                let from = lsp::from_position(&doc.text, &e["range"]["start"], enc)?;
                let to = lsp::from_position(&doc.text, &e["range"]["end"], enc)?;
                Some(crate::transaction::Change {
                    from,
                    to: to.max(from),
                    insert: e["newText"].as_str()?.to_string(),
                })
            })
            .collect();
        let before = doc.snapshot();
        doc.apply(&crate::transaction::Transaction::new(changes));
        doc.commit_undo(before);
        Ok(())
    }

    /// WorkspaceEdit (`changes` or `documentChanges`) — multiple files, in order. Unopened files are opened
    /// and edited (current buffer stays). Also file create/move/delete (`resourceOperations`) — not undoable.
    /// Returns the number of files changed (including created·renamed·deleted).
    pub fn apply_workspace_edit(&mut self, cid: ClientId, edit: &Value) -> Result<usize, String> {
        let enc = self.encoding_of(cid);
        let current = self.doc().id;
        let mut n = 0;
        let path_of = |u: &Value| -> Result<std::path::PathBuf, String> {
            let u = u.as_str().ok_or("missing uri")?;
            lsp::path_from_uri(u).ok_or_else(|| format!("bad uri {u}"))
        };
        // Both present → documentChanges only (spec)
        let doc_changes = edit["documentChanges"].as_array();
        if let Some(map) = edit["changes"].as_object().filter(|_| doc_changes.is_none()) {
            for (u, edits) in map {
                let path = path_of(&Value::String(u.clone()))?;
                self.edit_file(&path, enc, edits.as_array().map(Vec::as_slice).unwrap_or_default())?;
                n += 1;
            }
        }
        // Edits for a versioned doc the user has changed since → the offsets are stale; apply nothing
        for dc in doc_changes.into_iter().flatten() {
            if let (Some(v), Ok(path)) =
                (dc["textDocument"]["version"].as_i64(), path_of(&dc["textDocument"]["uri"]))
                && let Some(doc) = self.open_doc_at(&path)
                // (against that server's own version and queue)
                && let Some(l) = doc.lsp.link(cid)
                && (i64::from(l.version) != v || l.behind())
            {
                return Err(format!("{} changed since the server computed this edit", doc.display_name()));
            }
        }
        for dc in doc_changes.into_iter().flatten() {
            let opts = &dc["options"];
            let flag = |k: &str| opts[k].as_bool().unwrap_or(false);
            match dc["kind"].as_str() {
                Some("create") => {
                    let path = path_of(&dc["uri"])?;
                    if path.exists() && !flag("overwrite") {
                        if flag("ignoreIfExists") {
                            continue;
                        }
                        return Err(format!("{} already exists", path.display()));
                    }
                    if let Some(dir) = path.parent() {
                        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
                    }
                    std::fs::write(&path, "").map_err(|e| format!("create {}: {e}", path.display()))?;
                }
                Some("rename") => {
                    let (from, to) = (path_of(&dc["oldUri"])?, path_of(&dc["newUri"])?);
                    if to.exists() && !flag("overwrite") {
                        if flag("ignoreIfExists") {
                            continue;
                        }
                        return Err(format!("{} already exists", to.display()));
                    }
                    self.rename_file(&from, &to)?;
                }
                Some("delete") => {
                    let path = path_of(&dc["uri"])?;
                    if !path.exists() {
                        if flag("ignoreIfNotExists") {
                            continue;
                        }
                        return Err(format!("{} does not exist", path.display()));
                    }
                    let r = if path.is_dir() {
                        if flag("recursive") {
                            std::fs::remove_dir_all(&path)
                        } else {
                            std::fs::remove_dir(&path)
                        }
                    } else {
                        std::fs::remove_file(&path)
                    };
                    r.map_err(|e| format!("delete {}: {e}", path.display()))?;
                    // Close buffers that were open (including files under it)
                    let gone: Vec<DocId> = self
                        .docs
                        .iter()
                        .filter(|d| d.path.as_deref().is_some_and(|p| p.starts_with(&path)))
                        .map(|d| d.id)
                        .collect();
                    for id in gone {
                        self.close_doc(id);
                    }
                }
                _ => {
                    let path = path_of(&dc["textDocument"]["uri"])?;
                    self.edit_file(
                        &path,
                        enc,
                        dc["edits"].as_array().map(Vec::as_slice).unwrap_or_default(),
                    )?;
                }
            }
            n += 1;
        }
        if let Some(i) = self.docs.iter().position(|d| d.id == current) {
            self.current = i;
        }
        self.lsp_flush();
        Ok(n)
    }

    /// Open doc for this path (paths of docs are canonical).
    pub(crate) fn open_doc_at(&self, path: &std::path::Path) -> Option<&crate::document::Document> {
        let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        self.docs.iter().find(|d| d.path.as_deref() == Some(path.as_path()))
    }

    /// TextEdit[] for one file — opened if not already open.
    fn edit_file(&mut self, path: &std::path::Path, enc: Encoding, edits: &[Value]) -> Result<(), String> {
        let id = match self.open_doc_at(path) {
            Some(d) => d.id,
            None => {
                self.open(path).map_err(|e| format!("{e:#}"))?;
                self.doc().id
            }
        };
        self.apply_text_edits(id, enc, edits)
    }

    /// Move a file — an open buffer takes the new name too (content intact); the server gets close + reopen.
    fn rename_file(&mut self, from: &std::path::Path, to: &std::path::Path) -> Result<(), String> {
        if let Some(dir) = to.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::rename(from, to).map_err(|e| format!("rename {}: {e}", from.display()))?;
        let from = std::fs::canonicalize(from.parent().unwrap_or(from))
            .map(|d| d.join(from.file_name().unwrap_or_default()))
            .unwrap_or_else(|_| from.to_path_buf());
        let to = std::fs::canonicalize(to).unwrap_or_else(|_| to.to_path_buf());
        let moved: Vec<DocId> = self
            .docs
            .iter()
            .filter(|d| d.path.as_deref().is_some_and(|p| p.starts_with(&from)))
            .map(|d| d.id)
            .collect();
        for id in moved {
            self.lsp_did_close(id);
            let doc = self.docs.iter_mut().find(|d| d.id == id).expect("listed above");
            let old = doc.path.take().unwrap_or_default();
            let rest = old.strip_prefix(&from).map(|r| r.to_path_buf()).unwrap_or_default();
            let new = if rest.as_os_str().is_empty() { to.clone() } else { to.join(rest) };
            doc.disk = crate::disk::stamp(&new);
            doc.path = Some(new);
            self.attach_lsp(id);
        }
        Ok(())
    }

    /// Tell the server "this document was closed" (buffer closed or renamed).
    pub fn lsp_did_close(&mut self, id: DocId) {
        let Some(doc) = self.docs.iter_mut().find(|d| d.id == id) else { return };
        let Some(path) = doc.path.clone() else { return };
        let clients: Vec<ClientId> = doc.lsp.clients().collect();
        doc.lsp.client = None;
        doc.lsp.links.clear();
        doc.lsp.diagnostics.clear();
        doc.lsp.inlay.clear();
        for &cid in &clients {
            if let Some(c) = self.client_mut(cid) {
                c.notify("textDocument/didClose", json!({ "textDocument": { "uri": lsp::uri(&path) } }));
            }
        }
        // The servers this file's closing leaves with none to serve — its own, and others of its root (one
        // already let go waits less once its project has no file open): they stop once none has come back
        // for a while — a short while when no file under their root is open (`ROOT_GONE`)
        let serves = |cid: ClientId| self.docs.iter().any(|d| d.lsp.clients().any(|c| c == cid));
        let root_open = |root: &std::path::Path| {
            self.docs.iter().any(|d| d.id != id && d.path.as_deref().is_some_and(|p| p.starts_with(root)))
        };
        let waits: Vec<(ClientId, bool)> = self
            .lsp
            .clients
            .iter()
            .filter(|c| clients.contains(&c.id) || path.starts_with(&c.root))
            .filter(|c| !serves(c.id))
            .map(|c| (c.id, root_open(&c.root)))
            .filter(|&(cid, open)| clients.contains(&cid) || !open)
            .collect();
        for (cid, open) in waits {
            self.lsp_unused(cid, if open { IDLE_STOP } else { ROOT_GONE });
        }
    }

    /// Server `cid` serves no open file now: after `wait` it stops, unless one has come back by then (or it
    /// was let go again since — the latest wait counts). The next file of its project starts it again.
    fn lsp_unused(&mut self, cid: ClientId, wait: std::time::Duration) {
        let since = std::time::Instant::now();
        self.lsp.unused.insert(cid, since);
        self.events.jobs().spawn(move || {
            std::thread::sleep(wait);
            move |ed: &mut Editor| ed.lsp_stop_if_unused(cid, since, wait)
        });
    }

    fn lsp_stop_if_unused(&mut self, cid: ClientId, since: std::time::Instant, wait: std::time::Duration) {
        if self.lsp.unused.get(&cid) != Some(&since) {
            return;
        }
        self.lsp.unused.remove(&cid);
        if self.docs.iter().any(|d| d.lsp.clients().any(|c| c == cid)) {
            return;
        }
        let Some(root) = self.client(cid).map(|c| c.root.display().to_string()) else { return };
        if let Some(name) = self.lsp_stopped(cid) {
            let idle = crate::term::duration(wait.as_secs_f32());
            crate::log::line(
                &name,
                "stop",
                &format!("{name} for {root} stopped — no file of it open for {idle}"),
            );
        }
    }

    /// Code action preview state (for drawing).
    pub fn action_preview(&self, i: usize) -> Option<&ActionPreview> {
        self.lsp.action_previews.get(&i)
    }

    /// Selected row in the code action picker — starts building its preview if needed (called on row moves).
    pub fn code_action_preview(&mut self) {
        let Some(Action::Code(i)) =
            self.picker.as_ref().and_then(|p| p.current()).map(|it| it.action.clone())
        else {
            return;
        };
        self.code_action_preview_of(i, true);
    }

    fn code_action_preview_of(&mut self, i: usize, may_resolve: bool) {
        if self.lsp.action_previews.contains_key(&i) {
            return;
        }
        let gen_ = self.lsp.actions_gen;
        let Some((cid, a)) = self.lsp.actions.get(i).cloned() else { return };
        let edit = &a["edit"];
        // Command-only actions have nothing to preview. Empty edits (lazy action) → ask for the edit first
        if a["command"].is_string() || (edit.is_null() && !may_resolve) {
            self.lsp.action_previews.insert(i, ActionPreview::CommandOnly);
            return;
        }
        if edit.is_null() {
            // The reply is dropped if the list changed by then
            let kind = Kind::ResolvePreview { list: gen_, index: i };
            if self.lsp_send(cid, kind, "codeAction/resolve", a).is_some() {
                self.lsp.action_previews.insert(i, ActionPreview::Working);
            }
            return;
        }
        // Open buffers use their current content; the rest is read from disk on a worker thread
        let enc = self.encoding_of(cid);
        let open: HashMap<std::path::PathBuf, ropey::Rope> =
            self.docs.iter().filter_map(|d| Some((d.path.clone()?, d.text.clone()))).collect();
        let edit = edit.clone();
        self.lsp.action_previews.insert(i, ActionPreview::Working);
        self.events.jobs().spawn(move || {
            let files = crate::editdiff::build(&edit, enc, |p| {
                let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
                open.get(&p).cloned().or_else(|| crate::document::read_file(&p).ok().filter(|_| p.exists()))
            });
            move |ed: &mut Editor| {
                if ed.lsp.actions_gen == gen_ {
                    ed.lsp.action_previews.insert(i, ActionPreview::Ready(std::sync::Arc::new(files)));
                }
            }
        });
    }

    /// Code action picked in the picker.
    pub fn run_code_action(&mut self, i: usize) {
        if let Some((cid, a)) = self.lsp.actions.get(i).cloned() {
            self.run_code_action_value(cid, a, true);
        }
    }

    /// Run a Command | CodeAction: apply edits if any, ask the server to execute the command if any.
    /// If neither edit nor command (lazily computed), codeAction/resolve first.
    fn run_code_action_value(&mut self, cid: ClientId, a: Value, may_resolve: bool) {
        if a["command"].is_string() {
            return self.execute_command(cid, &a);
        }
        let (edit, cmd) = (&a["edit"], &a["command"]);
        if edit.is_null() && cmd.is_null() && may_resolve {
            self.lsp_send(cid, Kind::ResolveAction, "codeAction/resolve", a);
            return;
        }
        if !edit.is_null()
            && let Err(e) = self.apply_workspace_edit(cid, edit)
        {
            return self.set_error(format!("code action: {e}"));
        }
        if cmd.is_object() {
            self.execute_command(cid, cmd);
        }
    }

    fn execute_command(&mut self, cid: ClientId, cmd: &Value) {
        // jdtls: wraps the edit in a client command — apply without asking the server
        if cmd["command"] == "java.apply.workspaceEdit" {
            for edit in cmd["arguments"].as_array().into_iter().flatten() {
                if let Err(e) = self.apply_workspace_edit(cid, edit) {
                    return self.set_error(format!("code action: {e}"));
                }
            }
            return;
        }
        let params = json!({ "command": cmd["command"], "arguments": cmd["arguments"] });
        self.lsp_send(cid, Kind::ExecuteCommand, "workspace/executeCommand", params);
    }

    /// `space a` — code actions for the selection (cursor), from every server of the file that has them
    /// (each gets the diagnostics it reported there as context). The list shows once all have answered —
    /// or after a second, with what came.
    pub fn code_action(&mut self) {
        self.lsp_flush();
        let kind = Kind::CodeAction { list: 0 };
        let doc = self.doc();
        let mut servers: Vec<ClientId> = doc
            .lsp
            .clients()
            .filter(|&c| {
                self.allows(c, "code-action")
                    && self.client(c).is_some_and(|x| {
                        let cap = &x.caps["codeActionProvider"];
                        x.ready && !cap.is_null() && *cap != json!(false)
                    })
            })
            .collect();
        if servers.is_empty() {
            // None says so: the main one (it may still answer), or why not
            servers.extend(self.ready_client(&kind));
            if servers.is_empty() {
                return;
            }
        }
        self.lsp.actions_gen += 1;
        let list = self.lsp.actions_gen;
        let doc = self.doc();
        self.lsp.actions_at = Some((doc.id, doc.version(), doc.selection().primary().cursor(&doc.text)));
        self.lsp.actions.clear();
        self.lsp.action_previews.clear();
        self.lsp.actions_waiting = 0;
        for cid in servers {
            let doc = self.doc();
            let Some(path) = doc.path.clone() else { return };
            let enc = self.encoding_of(cid);
            let r = doc.selection().primary().min_width_1(&doc.text);
            let diags: Vec<Value> = doc
                .lsp
                .diagnostics
                .iter()
                .filter(|d| d.source == cid && d.from <= r.to() && d.to >= r.from())
                .map(|d| d.raw.clone())
                .collect();
            let range = json!({ "start": lsp::to_position(&doc.text, r.from(), enc), "end": lsp::to_position(&doc.text, r.to(), enc) });
            let params = json!({
                "textDocument": { "uri": lsp::uri(&path) },
                "range": range,
                "context": { "diagnostics": diags, "triggerKind": 1 },
            });
            if self.lsp_send(cid, Kind::CodeAction { list }, "textDocument/codeAction", params).is_some() {
                self.lsp.actions_waiting += 1;
            }
        }
        // Several servers: a slow one doesn't hold the others' list past a second (one alone is waited for)
        if self.lsp.actions_waiting > 1 {
            self.events.jobs().spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(1));
                move |ed: &mut Editor| {
                    if ed.lsp.actions_gen == list && ed.lsp.actions_waiting > 0 {
                        ed.lsp.actions_waiting = 0;
                        ed.show_code_actions();
                    }
                }
            });
        }
    }

    /// One server's code actions for list `list` (None: it failed) — the list shows once all have answered.
    fn on_code_actions(&mut self, cid: ClientId, list: u64, result: &Value) {
        if list != self.lsp.actions_gen || self.lsp.actions_waiting == 0 {
            return;
        }
        self.lsp.actions.extend(result.as_array().into_iter().flatten().map(|a| (cid, a.clone())));
        self.lsp.actions_waiting -= 1;
        if self.lsp.actions_waiting == 0 {
            self.show_code_actions();
        }
    }

    /// The gathered code actions as a picker: server-preferred → quick fixes → the rest (each server's order
    /// within a tier); with more than one server, each row says whose it is.
    fn show_code_actions(&mut self) {
        // Only where they were asked for (the cursor may have moved by the time a timer shows them)
        let doc = self.doc();
        let here = (doc.id, doc.version(), doc.selection().primary().cursor(&doc.text));
        if self.lsp.actions_at != Some(here) {
            return;
        }
        if self.lsp.actions.is_empty() {
            return self.set_status("no code actions");
        }
        self.lsp.actions.sort_by_key(|(_, a)| {
            let quickfix = a["kind"].as_str().is_some_and(|k| k.starts_with("quickfix"));
            (!a["isPreferred"].as_bool().unwrap_or(false), !quickfix)
        });
        let several = self.lsp.actions.iter().any(|(c, _)| *c != self.lsp.actions[0].0);
        let items = self
            .lsp
            .actions
            .iter()
            .enumerate()
            .map(|(i, (cid, a))| {
                // Kind as a short dim note on the right (`refactor.rewrite` → `rewrite`)
                let kind = a["kind"].as_str().unwrap_or_default();
                let mut hint = kind.rsplit('.').next().unwrap_or_default().to_string();
                if a["isPreferred"].as_bool().unwrap_or(false) {
                    hint = format!("● {hint}");
                }
                if several && let Some(c) = self.client(*cid) {
                    hint = format!("{hint} · {}", c.name);
                }
                Item {
                    label: a["title"].as_str().unwrap_or("?").to_string(),
                    action: Action::Code(i),
                    hint,
                    glyph: None,
                }
            })
            .collect();
        self.open_picker(Picker::new("code actions", items, false), None);
        self.code_action_preview();
    }

    /// `:w` / `:wq` with format-on-save: ask the server to format and save when it answers — true if
    /// asked (the caller then doesn't save). A slow server gets 2 s, then the file is saved as is.
    pub fn format_then_save(&mut self, quit: bool) -> bool {
        self.format_then_save_doc(self.doc().id, quit)
    }

    /// `format_then_save` for any open document (`:wa` goes through every modified one). With `quit`, tarae
    /// quits once the last of these saves is done. Plugins that want `before-save` come first.
    pub fn format_then_save_doc(&mut self, doc_id: DocId, quit: bool) -> bool {
        self.plugins_before_save(doc_id, quit) || self.lsp_format_then_save_doc(doc_id, quit)
    }

    fn lsp_format_then_save_doc(&mut self, doc_id: DocId, quit: bool) -> bool {
        let cid = self.server_for_doc(doc_id, &Kind::FormatSave { quit });
        let Some(doc) = self.docs.iter().find(|d| d.id == doc_id) else { return false };
        let (Some(path), Some(cid)) = (doc.path.clone(), cid) else { return false };
        if !self.config.format_on_save || doc.loading || doc.virtual_uri.is_some() {
            return false;
        }
        let can = self.client(cid).is_some_and(|c| {
            let cap = &c.caps["documentFormattingProvider"];
            c.ready && !cap.is_null() && *cap != json!(false)
        });
        if !can {
            return false;
        }
        self.lsp_flush();
        let tab = self.config.tab_width;
        let params = json!({
            "textDocument": { "uri": lsp::uri(&path) },
            "options": { "tabSize": tab, "insertSpaces": true },
        });
        let Some(id) =
            self.lsp_send_for(doc_id, cid, Kind::FormatSave { quit }, "textDocument/formatting", params)
        else {
            return false;
        };
        self.note("formatting…");
        self.events.jobs().spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(2));
            move |ed: &mut Editor| {
                if ed.lsp.pending.remove(&(cid, id)).is_some() {
                    ed.format_save_done(doc_id, quit, Some("the formatter took too long — saved as is"));
                }
            }
        });
        true
    }

    /// Formatting settled (or gave up with `why`): save the document, then quit for `:wq`.
    pub(crate) fn format_save_done(&mut self, id: DocId, quit: bool, why: Option<&str>) {
        if let Err(e) = crate::typed::save_doc(self, id, false) {
            return self.set_error(e);
        }
        if let Some(why) = why {
            self.set_warning(why.to_string());
        }
        // (`:wqa`: quit after the last file's format, and the last plugins' `before-save`)
        let formatting = self.lsp.pending.values().any(|p| matches!(p.kind, Kind::FormatSave { .. }))
            || self.plugins_saving();
        if quit
            && !formatting
            && let Err(e) = crate::typed::quit(self, false)
        {
            self.set_error(e);
        }
    }

    /// `space h` — every use of the symbol under the cursor in this file, as selections (documentHighlight).
    pub fn select_symbol_uses(&mut self) {
        let Some(cid) = self.ready_client(&Kind::Highlights) else { return };
        let Some(c) = self.client(cid) else { return };
        let cap = &c.caps["documentHighlightProvider"];
        if cap.is_null() || *cap == json!(false) {
            let name = c.name.clone();
            return self.set_status(format!("{name} can't find a symbol's uses"));
        }
        self.lsp_request(Kind::Highlights, "textDocument/documentHighlight", json!({}));
    }

    /// `:lsp-stop` — this file's language servers stop (files using them lose diagnostics until they're
    /// back). Their names.
    pub fn lsp_stop(&mut self) -> Result<String, String> {
        let cids: Vec<ClientId> = self.doc().lsp.clients().collect();
        if cids.is_empty() {
            return Err("no language server for this file".into());
        }
        let names: Vec<String> = cids.into_iter().filter_map(|c| self.lsp_stopped(c)).collect();
        Ok(names.join(" + "))
    }

    /// `:lsp-restart` — stop this file's servers and start them again for every file that used them.
    pub fn lsp_restart(&mut self) -> Result<(), String> {
        let cids: Vec<ClientId> = self.doc().lsp.clients().collect();
        let docs: Vec<DocId> =
            self.docs.iter().filter(|d| d.lsp.clients().any(|c| cids.contains(&c))).map(|d| d.id).collect();
        let name = self.lsp_stop()?;
        for id in docs {
            self.attach_lsp(id);
        }
        self.set_success(format!("{name} restarted"));
        Ok(())
    }

    /// An empty answer from a server still at work (rust-analyzer indexing, jdtls importing) isn't "nothing
    /// there" — say what it's busy with and ask again once it's done (`ask_again`). true if so.
    fn wait_for_idle(&mut self, cid: ClientId, p: &Pending) -> bool {
        let Some(c) = self.client(cid) else { return false };
        if c.progress.is_empty() {
            return false;
        }
        // (what it's doing is on the status line — "Roots Scanned" and the like mean little here)
        let then = match p.kind {
            Kind::Goto(Goto::References) => "listing them",
            Kind::Goto(_) => "going there",
            Kind::Hover => "showing it",
            _ => "selecting them",
        };
        let msg = format!("{} is loading — {then} when ready", c.name);
        let doing: Vec<String> = c.progress.values().map(|p| p.title.clone()).collect();
        if !self.put_off(cid, p) {
            return false;
        }
        if let Some(t) = self.lsp.trace.as_mut() {
            t.step(format!("answered empty while busy ({})", doing.join(", ")));
        }
        self.set_status(msg);
        true
    }

    /// The question at `p` waits for server `cid` — false once it has waited too often at that place.
    fn put_off(&mut self, cid: ClientId, p: &Pending) -> bool {
        let at = Some((p.doc, p.version, p.head));
        let tries = if self.lsp.ask_tries.0 == at { self.lsp.ask_tries.1 + 1 } else { 1 };
        self.lsp.ask_tries = (at, tries);
        if tries > MAX_ASKS {
            return false;
        }
        self.lsp.ask_when_idle = Some((cid, p.kind.clone(), p.doc, p.version, p.head));
        true
    }

    /// The server dropped the question because the project changed meanwhile: ask again when it's idle — at
    /// once if it already is (after a moment, so a burst of changes can settle).
    fn ask_again_later(&mut self, cid: ClientId, p: &Pending) {
        if let Some(t) = self.lsp.trace.as_mut() {
            t.step("content modified");
        }
        if self.wait_for_idle(cid, p) {
            return;
        }
        if !self.put_off(cid, p) {
            let name = self.client(cid).map(|c| c.name.clone()).unwrap_or_default();
            return self.set_status(format!("{name} is still settling the project — try again in a moment"));
        }
        self.events.jobs().spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(if cfg!(test) { 5 } else { 300 }));
            move |ed: &mut Editor| ed.ask_again(cid)
        });
    }

    /// Server `cid` finished a piece of work: if something waits on it, ask again — as long as the cursor is
    /// still where the question was asked. Not only once it's all done: rust-analyzer's `cargo check` can
    /// run long after indexing, and the answer is usually there once indexing ends.
    fn ask_again(&mut self, cid: ClientId) {
        let Some((_, kind, doc, version, head)) = self.lsp.ask_when_idle.take_if(|w| w.0 == cid) else {
            return;
        };
        let d = self.doc();
        if d.id != doc || d.version() != version || d.selection().primary().cursor(&d.text) != head {
            return; // moved on
        }
        match kind {
            Kind::Goto(goto) => self.goto_ask(goto, true),
            Kind::Hover => {
                self.lsp_request(Kind::Hover, "textDocument/hover", json!({}));
            }
            Kind::Highlights => self.select_symbol_uses(),
            _ => {}
        }
    }

    /// `g d`·`g D`·`g y`·`g i`·`g r` — asks only a server that says it can answer.
    pub fn goto_location(&mut self, goto: Goto) {
        // One still waiting (never answered, or the user gave up): logged as it stood
        if let Some(mut old) = self.lsp.trace.replace(Trace::new(goto.noun())) {
            old.step("asked anew, unanswered");
            crate::log::line("tarae", "timing", &old.line());
            #[cfg(test)]
            self.lsp.traced.push(old.line());
        }
        // A new round: the name under the cursor is what its list's rows show
        let doc = self.doc();
        let head = doc.selection().primary().cursor(&doc.text);
        let from = crate::completion::word_start(&doc.text, head);
        let name: String =
            doc.text.byte_slice(from..).chars().take_while(|&c| crate::completion::is_word(c)).collect();
        self.lsp.refs = Refs { generation: self.lsp.refs.generation + 1, name, ..Refs::default() };
        self.goto_ask(goto, false);
    }

    /// The question — from its key, or `again` once the busy server it waited for has done a piece of work.
    fn goto_ask(&mut self, goto: Goto, again: bool) {
        if again && let Some(t) = self.lsp.trace.as_mut() {
            t.step("waited for the server");
        }
        // A server still starting can't be asked yet (N3): the text matches meanwhile, the question once
        // it's up
        if goto == Goto::References
            && !again
            && let Some(cid) = self.doc().lsp.client
            && let Some(name) = self.client(cid).filter(|c| !c.ready).map(|c| c.name.clone())
        {
            let d = self.doc();
            let head = d.selection().primary().cursor(&d.text);
            let p = Pending { kind: Kind::Goto(goto), doc: d.id, version: d.version(), head };
            self.put_off(cid, &p);
            self.text_matches(cid);
            if let Some(t) = self.lsp.trace.as_mut() {
                t.step("server starting");
            }
            return self.set_status(format!("{name} is starting — {} when it's ready", goto.noun()));
        }
        let Some(cid) = self.ready_client(&Kind::Goto(goto)) else { return self.trace_ends("not asked") };
        let Some(c) = self.client(cid) else { return };
        if let Some(cap) = goto.provider().map(|p| &c.caps[p])
            && (cap.is_null() || *cap == json!(false))
        {
            let name = c.name.clone();
            self.trace_ends("can't");
            return self.set_status(format!("{name} can't find {}", goto.noun()));
        }
        let extra = match goto {
            Goto::References => json!({ "context": { "includeDeclaration": true } }),
            _ => json!({}),
        };
        if !self.lsp_request(Kind::Goto(goto), goto.method(), extra) {
            return;
        }
        if let Some(t) = self.lsp.trace.as_mut() {
            t.step(if again { "asked again" } else { "sent" });
        }
        // A server still loading answers late, or empty: the name's text matches meanwhile (N3)
        if goto == Goto::References && !again && self.client(cid).is_some_and(|c| !c.progress.is_empty()) {
            self.text_matches(cid);
        }
        // References: where it's declared, asked beside them — that row goes first, marked
        let defines = self.client(cid).is_some_and(|c| {
            let cap = &c.caps["definitionProvider"];
            !cap.is_null() && *cap != json!(false)
        });
        if goto == Goto::References && defines && self.lsp.refs.declared.is_none() {
            let generation = self.lsp.refs.generation;
            self.lsp_request(Kind::Declared { generation }, "textDocument/definition", json!({}));
        }
    }

    /// `g r` with server `cid` still loading (N3): the name's text matches in its project (`grep.rs` — whole word,
    /// case as written) listed at once, titled as text matches; the server's references take their place
    /// when they come (`list_places`). Not if the server answered first, or another list is up.
    fn text_matches(&mut self, cid: ClientId) {
        let Some(c) = self.client(cid) else { return };
        let (root, server) = (c.root.clone(), c.name.clone());
        let name = self.lsp.refs.name.clone();
        if name.is_empty() {
            return;
        }
        let generation = self.lsp.refs.generation;
        let here = std::env::current_dir().unwrap_or_else(|_| root.clone());
        let asked = std::time::Instant::now();
        self.events.jobs().spawn(move || {
            // (paths as the server's list shows them — from where tarae runs, not the server's root)
            let found = crate::picker::word_matches(&root, &name).map(|items| {
                items
                    .into_iter()
                    .map(|mut it| {
                        if let Action::Goto { path, .. } = &it.action
                            && let Ok(rel) = path.strip_prefix(&root)
                        {
                            let rest = it.label[rel.to_string_lossy().len()..].to_string();
                            it.label =
                                format!("{}{rest}", path.strip_prefix(&here).unwrap_or(path).display());
                        }
                        it
                    })
                    .collect::<Vec<_>>()
            });
            // (a server that answers within the grace never shows text matches — no flash of a list)
            std::thread::sleep(TEXT_GRACE.saturating_sub(asked.elapsed()));
            move |ed: &mut Editor| {
                if ed.lsp.refs.generation != generation || ed.lsp.refs.shown.is_some() || ed.picker.is_some()
                {
                    return;
                }
                let items = match found {
                    Ok(items) if !items.is_empty() => items,
                    _ => return,
                };
                // (short — the title holds a few words; the server's state is said beside it)
                let mut picker = Picker::new("text matches", items, true).grouped(here);
                picker.emphasis = regex::Regex::new(&format!(r"\b{}\b", regex::escape(&name))).ok();
                ed.open_picker(picker, None);
                ed.lsp.refs.texts = ed.picker.as_ref().map(|p| p.id);
                ed.set_status(format!("text matches of `{name}` until {server} is ready"));
                if let Some(t) = ed.lsp.trace.as_mut() {
                    t.step("text matches shown");
                }
            }
        });
    }

    /// Claude's `references` question `ask` (N5): the references at `byte` of document `id` (its
    /// declaration included) and its definition, asked of its server — that server, its name, and whether
    /// the definition was asked too. Err: why it can't be asked now, and whether it can be soon (a server
    /// starting) — the answers go to `nav::refs_answered`/`declared_answered`.
    pub(crate) fn ask_references(
        &mut self,
        id: DocId,
        byte: usize,
        ask: u64,
    ) -> Result<(ClientId, String, bool), (String, bool)> {
        self.lsp_flush();
        let doc = self.docs.iter().find(|d| d.id == id).ok_or(("the file was closed".to_string(), false))?;
        let lang = doc.syntax.as_ref().map(|s| s.lang.name.clone());
        let lang = lang.or_else(|| crate::syntax::detect(doc.path.as_deref()?).map(|s| s.name.clone()));
        if doc.lsp.clients().next().is_none() {
            let lang = lang.map_or(String::new(), |l| format!("{l} "));
            return Err((format!("there's no language server for {lang}files here"), false));
        }
        let kind = Kind::ToolRefs { ask };
        let cid =
            self.server_for_doc(id, &kind).ok_or(("no server here finds references".to_string(), false))?;
        let c = self.client(cid).ok_or(("its language server stopped".to_string(), true))?;
        if !c.ready {
            return Err((format!("{} is starting", c.name), true));
        }
        let name = c.name.clone();
        let defines = !c.caps["definitionProvider"].is_null() && c.caps["definitionProvider"] != json!(false);
        let doc = self.docs.iter().find(|d| d.id == id).expect("found above");
        let Some(path) = doc.path.as_deref() else { return Err(("the file has no path".into(), false)) };
        let at = json!({ "textDocument": { "uri": lsp::uri(path) },
            "position": lsp::to_position(&doc.text, byte, self.encoding_of(cid)) });
        let mut refs = at.clone();
        refs["context"] = json!({ "includeDeclaration": true });
        if self.lsp_send_for(id, cid, kind, "textDocument/references", refs).is_none() {
            return Err((format!("{name} couldn't be asked"), true));
        }
        let defines = defines
            && self
                .lsp_send_for(id, cid, Kind::ToolDeclared { ask }, "textDocument/definition", at)
                .is_some();
        Ok((cid, name, defines))
    }

    /// Claude's `symbols` question `ask` (N5b): document `id`'s symbols, asked of its server — that server
    /// and its name. Err: why it can't be asked now (the answer is tarae's outline then); the answer goes
    /// to `nav::symbols_answered`.
    pub(crate) fn ask_symbols(&mut self, id: DocId, ask: u64) -> Result<(ClientId, String), String> {
        self.lsp_flush();
        let doc = self.docs.iter().find(|d| d.id == id).ok_or("the file was closed")?;
        if doc.lsp.clients().next().is_none() {
            return Err("there's no language server for it".into());
        }
        let kind = Kind::ToolSymbols { ask };
        let cid = self.server_for_doc(id, &kind).ok_or("its language server doesn't list symbols")?;
        let c = self.client(cid).ok_or("its language server stopped")?;
        if !c.ready {
            return Err(format!("{} is starting", c.name));
        }
        let name = c.name.clone();
        let Some(path) = self.docs.iter().find(|d| d.id == id).and_then(|d| d.path.clone()) else {
            return Err("the file has no path".into());
        };
        let params = json!({ "textDocument": { "uri": lsp::uri(&path) } });
        self.lsp_send_for(id, cid, kind, "textDocument/documentSymbol", params)
            .ok_or_else(|| format!("{name} couldn't be asked"))?;
        Ok((cid, name))
    }

    /// A go-to's places as a list (N2): each row the code there, the name it's about standing out, by file
    /// then line, the declaration first — the lines read on a worker (files not open are on disk).
    fn list_places(&mut self, goto: Goto, found: Vec<(std::path::PathBuf, usize, usize)>) {
        let open: HashMap<std::path::PathBuf, ropey::Rope> = self
            .docs
            .iter()
            .filter_map(|d| Some((d.path.clone()?, d.text.clone())))
            .filter(|(p, _)| found.iter().any(|f| f.0 == *p))
            .collect();
        let generation = self.lsp.refs.generation;
        self.events.jobs().spawn(move || {
            let places = read_places(found, &open);
            move |ed: &mut Editor| {
                // (another go-to since: this list is moot)
                if ed.lsp.refs.generation != generation {
                    return;
                }
                if let Some(t) = ed.lsp.trace.as_mut() {
                    t.step("lines read");
                }
                let root = std::env::current_dir().unwrap_or_default();
                let declared = ed.lsp.refs.declared.clone().unwrap_or_default();
                let items = place_items(&root, places.clone(), &declared);
                // The text matches shown while the server loaded (N3): its answer takes their place — said;
                // once they were left (one picked, or closed), the answer isn't pushed on you
                if let Some(id) = ed.lsp.refs.texts {
                    match ed.picker.as_mut().filter(|p| p.id == id) {
                        Some(p) => {
                            p.title = goto.noun().to_string();
                            p.selected = 0;
                            p.set_items(items);
                            ed.lsp.refs.shown = Some((id, places));
                            ed.set_status(format!("the server's {} replaced the text matches", goto.noun()));
                            ed.trace_ends("list (text matches replaced)");
                        }
                        None => ed.trace_ends("not listed (the text matches were left)"),
                    }
                    return;
                }
                let mut picker = Picker::new(goto.noun(), items, true).grouped(root);
                let name = &ed.lsp.refs.name;
                picker.emphasis = (!name.is_empty())
                    .then(|| regex::Regex::new(&format!(r"\b{}\b", regex::escape(name))).ok())
                    .flatten();
                ed.open_picker(picker, None);
                ed.lsp.refs.shown = ed.picker.as_ref().map(|p| (p.id, places));
                ed.trace_ends("list");
            }
        });
    }

    /// The go-to's answer is dealt with (`what` it came to): the next frame drawn ends its trace.
    fn trace_ends(&mut self, what: &str) {
        if let Some(t) = self.lsp.trace.as_mut() {
            t.step(what);
            t.answered = true;
        }
    }

    /// After a frame: a go-to whose answer was dealt with is on screen now — its way into the log (N1).
    pub fn trace_drawn(&mut self) {
        let Some(mut t) = self.lsp.trace.take_if(|t| t.answered) else { return };
        t.step("drawn");
        crate::log::line("tarae", "timing", &t.line());
        #[cfg(test)]
        self.lsp.traced.push(t.line());
    }

    fn lsp_jump(&mut self, path: &std::path::Path, pos: &Value, enc: Encoding) {
        self.push_jump();
        if let Err(e) = self.open(path) {
            return self.set_error(format!("{e:#}"));
        }
        self.lsp.jump_after_load = Some((self.doc().id, pos.clone(), enc));
        self.jump_if_loaded(self.doc().id);
    }

    /// A go-to target waiting for this doc — taken once its text is there (big files load in the background).
    fn jump_if_loaded(&mut self, id: DocId) {
        let Some(doc) = self.docs.iter_mut().find(|d| d.id == id && !d.loading) else { return };
        let Some((_, pos, enc)) = self.lsp.jump_after_load.take_if(|(d, ..)| *d == id) else { return };
        if let Some(b) = lsp::from_position(&doc.text, &pos, enc) {
            doc.set_selection(Selection::point(b));
        }
    }

    /// `]d` / `[d` — next/previous diagnostic (wraps around). Selects the diagnostic range.
    pub fn goto_diagnostic(&mut self, forward: bool) {
        let doc = self.doc_mut();
        let head = doc.selection().primary().cursor(&doc.text);
        let list = &doc.lsp.diagnostics;
        if list.is_empty() {
            return self.set_status("no diagnostics");
        }
        let d = if forward {
            list.iter().find(|d| d.from > head).or(list.first())
        } else {
            list.iter().rev().find(|d| d.from < head).or(list.last())
        };
        let d = d.expect("non-empty").clone();
        let end = d.to.max(crate::graphemes::next_boundary(&doc.text, d.from));
        doc.set_selection(Selection::single(crate::selection::Range::new(d.from, end)));
        self.note(d.message);
    }

    /// `space d` — diagnostics list for this document.
    pub fn diagnostics_picker(&mut self) {
        let doc = self.doc();
        let Some(path) = doc.path.clone() else { return };
        let items: Vec<Item> = doc
            .lsp
            .diagnostics
            .iter()
            .map(|d| {
                let line = mv::line_of(&doc.text, d.from);
                let col = d.from - mv::line_start(&doc.text, line);
                Item {
                    label: format!(
                        "{}:{}  {}",
                        line + 1,
                        col + 1,
                        d.message.lines().next().unwrap_or_default()
                    ),
                    action: Action::Goto { path: path.clone(), line, col },
                    hint: String::new(),
                    glyph: Some(severity_glyph(d.severity)),
                }
            })
            .collect();
        self.open_picker(Picker::new("diagnostics", items, false), None);
    }

    /// `space D` — diagnostics in every file the servers reported: open ones (live), then the rest; errors
    /// first.
    pub fn workspace_diagnostics_picker(&mut self) {
        let root = std::env::current_dir().unwrap_or_default();
        let rel = |p: &std::path::Path| p.strip_prefix(&root).unwrap_or(p).display().to_string();
        let mut rows: Vec<(u8, String, Item)> = Vec::new();
        let mut push = |sev: u8, path: &std::path::Path, line: usize, col: usize, msg: &str| {
            let label = format!("{}:{}  {}", rel(path), line + 1, msg.lines().next().unwrap_or_default());
            let action = Action::Goto { path: path.to_path_buf(), line, col };
            let item = Item { label, action, hint: String::new(), glyph: Some(severity_glyph(sev)) };
            rows.push((sev, rel(path), item));
        };
        for doc in &self.docs {
            let Some(path) = doc.path.as_deref() else { continue };
            for d in &doc.lsp.diagnostics {
                let line = mv::line_of(&doc.text, d.from);
                push(d.severity, path, line, d.from - mv::line_start(&doc.text, line), &d.message);
            }
        }
        for (path, list) in &self.lsp.elsewhere {
            if self.docs.iter().any(|d| d.path.as_deref() == Some(path.as_path())) {
                continue; // open — its live list is above
            }
            for (_, sev, line, col, msg) in list {
                push(*sev, path, *line, *col, msg);
            }
        }
        if rows.is_empty() {
            return self.set_status("no diagnostics anywhere");
        }
        rows.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
        let items = rows.into_iter().map(|r| r.2).collect();
        self.open_picker(Picker::new("workspace diagnostics", items, true).grouped(root), None);
    }

    /// For the status line — what the current document's server is doing.
    pub fn lsp_progress(&self) -> Option<String> {
        let c = self.client(self.doc().lsp.client?)?;
        if !c.ready {
            return Some(format!("{}: starting", c.name));
        }
        c.progress_text()
    }

    /// Server `cid` closed its stdout (exited or crashed) — forget it and everything waiting on it; its docs
    /// go without a server (reopening the file starts a new one).
    pub fn lsp_exited(&mut self, cid: ClientId) {
        let root = self.client(cid).map(|c| c.root.clone()).unwrap_or_default();
        if let Some(name) = self.lsp_stopped(cid) {
            // (reaped as it's dropped; the next file of its project starts it again — unless it keeps
            // exiting: then `:procs` holds it)
            crate::log::line(&name, "exit", &format!("{name} for {} exited", root.display()));
            self.lsp.failed.insert(name.clone(), "exited".into());
            self.set_error(format!("{name} exited · :log-open shows why"));
            self.procs_server_exited(&name, &root);
        }
    }

    /// The current file's language server as the status line shows it: (dot, server name) — None when its
    /// language has no server configured.
    pub fn lsp_state(&self) -> Option<(LspDot, String)> {
        let doc = self.doc();
        let dot = |c: &Client| (if c.ready { LspDot::Ready } else { LspDot::Starting }, c.name.clone());
        if let Some(c) = doc.lsp.client.and_then(|cid| self.client(cid)) {
            return Some(dot(c));
        }
        // No main server: why (failed, not installed) — it's what matters, even with one beside it
        let lang = &doc.path.as_deref().and_then(crate::syntax::detect)?.name;
        if let Some(server) = self.lsp.server_for.get(lang)
            && self.lsp.failed.contains_key(server)
        {
            return Some((LspDot::Failed, server.clone()));
        }
        if self.lsp.missing.contains(lang) {
            return Some((LspDot::Missing, lang.clone()));
        }
        // A language with no main server at all: the one beside stands in
        doc.lsp.beside().next().and_then(|c| self.client(c)).map(dot)
    }

    /// Drop client `cid` (the process is killed) and everything that hung on it — its name, if it was there.
    pub(crate) fn lsp_stopped(&mut self, cid: ClientId) -> Option<String> {
        let i = self.lsp.clients.iter().position(|c| c.id == cid)?;
        let name = self.lsp.clients.remove(i).name.clone();
        // Its diagnostics in closed files go; the other servers' stay
        for list in self.lsp.elsewhere.values_mut() {
            list.retain(|d| d.0 != cid);
        }
        self.lsp.elsewhere.retain(|_, l| !l.is_empty());
        self.lsp.pending.retain(|(c, _), _| *c != cid);
        self.lsp.completion_waiting.retain(|w| w.0 != cid);
        (self.lsp.signature_inflight, self.lsp.signature_again) = (false, false);
        self.lsp.inlay_inflight = None;
        for d in &mut self.docs {
            d.lsp.diagnostics.retain(|x| x.source != cid);
            if d.lsp.client == Some(cid) {
                d.lsp.inlay.clear();
                d.lsp.inlay_have = None;
            }
            d.lsp.detach(cid);
        }
        if self.completion.as_ref().is_some_and(|c| c.has_client(cid)) {
            self.completion = None;
        }
        self.signature = None;
        if self.lsp.actions.iter().any(|(c, _)| *c == cid) {
            self.lsp.actions.clear();
            self.lsp.action_previews.clear();
        }
        if self.java_debug.as_ref().is_some_and(|p| p.cid == cid) {
            self.java_debug_fail(format!("{name} exited"));
        }
        Some(name)
    }
}

/// didChange with `changes` — the whole text if `full`, or if the server counts UTF-16 columns and some change
/// was queued before that was known (without them).
fn send_changes(
    client: &mut Client,
    uri: String,
    text: &ropey::Rope,
    version: i32,
    changes: Vec<crate::transaction::Edit>,
    full: bool,
) {
    let utf16 = client.encoding == Encoding::Utf16;
    if full || (utf16 && changes.iter().any(|c| c.u16.is_none())) {
        let text = text.clone();
        client.notify_lazy(move || {
            let params = json!({ "textDocument": { "uri": uri, "version": version }, "contentChanges": [{ "text": text.to_string() }] });
            json!({ "jsonrpc": "2.0", "method": "textDocument/didChange", "params": params }).to_string()
        });
        return;
    }
    let content: Vec<Value> = changes
        .iter()
        .map(|c| {
            let (sc, ec) = match c.u16 {
                Some((s, e)) if utf16 => (s, e),
                _ => (c.ts.start_position.column, c.ts.old_end_position.column),
            };
            json!({
                "range": {
                    "start": { "line": c.ts.start_position.row, "character": sc },
                    "end": { "line": c.ts.old_end_position.row, "character": ec },
                },
                "text": c.insert,
            })
        })
        .collect();
    client.notify(
        "textDocument/didChange",
        json!({ "textDocument": { "uri": uri, "version": version }, "contentChanges": content }),
    );
}

/// Picker glyph for a severity — the same marks as at the line end, in the theme's severity colors.
fn severity_glyph(sev: u8) -> (&'static str, &'static str) {
    match sev {
        1 => ("●", "error"),
        2 => ("▲", "warning"),
        3 => ("●", "info"),
        _ => ("·", "hint"),
    }
}

/// Whether a capability's character list (`triggerCharacters` …) has `t`.
fn lists_char(list: &Value, t: char) -> bool {
    let mut buf = [0; 4];
    let t = &*t.encode_utf8(&mut buf);
    list.as_array().is_some_and(|a| a.iter().any(|x| x.as_str() == Some(t)))
}

/// LSP SymbolKind → (glyph, theme scope) — same glyph rules as autocompletion.
pub fn symbol_glyph(kind: u64) -> (&'static str, &'static str) {
    match kind {
        6 | 9 | 12 => ("ƒ", "function"),
        5 | 10 | 11 | 19 | 23 | 26 => ("τ", "type"),
        1..=4 => ("§", "namespace"),
        7 | 8 | 20 => ("◦", "variable.other.member"),
        14 | 22 => ("π", "constant"),
        13 => ("ν", "variable"),
        _ => ("·", "ui.virtual"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use super::*;
    use crate::key::Key;
    use crate::selection::Range;

    fn feed(ed: &mut Editor, keys: &str) {
        let mut chars = keys.chars();
        while let Some(c) = chars.next() {
            let key: Key = if c == '<' {
                chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap()
            } else {
                c.to_string().parse().unwrap()
            };
            ed.handle_key(key);
        }
        ed.lsp_flush();
    }

    /// Fake server = `cat` writing everything it receives to a file. Tests feed responses via on_lsp_message.
    fn setup(name: &str, src: &str) -> (Editor, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tarae-lsp-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.rs");
        std::fs::write(&file, src).unwrap();
        // (a cargo project: rust's server isn't started for a lone .rs)
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"t\"\n").unwrap();
        let log = dir.join("sent.log");
        let cfg = format!(
            "[lsp.fake]\ncommand = \"sh\"\nargs = [\"-c\", \"cat > '{}'\"]\n[lang.rust]\nlsp = [\"fake\"]\n",
            log.display()
        );
        let (config, w) = crate::config::parse(&cfg);
        assert!(w.is_empty(), "{w:?}");
        let mut ed = Editor::new(config);
        ed.open(&file).unwrap();
        let file = std::fs::canonicalize(&file).unwrap();
        (ed, file, log)
    }

    fn sent(log: &PathBuf) -> String {
        std::thread::sleep(Duration::from_millis(150)); // writer thread
        std::fs::read_to_string(log).unwrap_or_default()
    }

    fn ready(ed: &mut Editor) -> ClientId {
        let cid = ed.doc().lsp.client.expect("attached");
        let init = json!({ "jsonrpc": "2.0", "id": 0, "result": { "capabilities": { "positionEncoding": "utf-8" } } });
        ed.on_lsp_message(cid, init);
        cid
    }

    /// Events until `done` (or `secs` pass) — the language server's own messages and our timers.
    fn settle_for(ed: &mut Editor, secs: u64, done: impl Fn(&Editor) -> bool) {
        let end = std::time::Instant::now() + Duration::from_secs(secs);
        while !done(ed) && std::time::Instant::now() < end {
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
    }

    /// Process `pid` is gone and reaped — not even a zombie is left (`kill(pid, 0)` finds a zombie).
    fn reaped(pid: u32) -> bool {
        // SAFETY: signal 0 only asks whether the process exists; nothing is sent.
        let r = unsafe { libc::kill(pid as i32, 0) };
        r != 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    /// A server no open file uses stops after `IDLE_STOP` — quietly (it's said in the log), its process
    /// reaped — unless a file of it comes back first; the next file of its project starts it again.
    #[test]
    fn an_unused_server_stops_unless_a_file_comes_back() {
        let (mut ed, file, _log) = setup("idle", "fn main() {}\n");
        let cid = ready(&mut ed);
        let pid = ed.client(cid).and_then(Client::pid).expect("its process");
        // Closed and opened again before the wait ends: it stays
        let id = ed.doc().id;
        ed.close_doc(id);
        assert!(ed.lsp.unused.contains_key(&cid), "unused, waiting");
        ed.open(&file).unwrap();
        assert_eq!(ed.doc().lsp.client, Some(cid), "the same server took the file back");
        settle_for(&mut ed, 1, |_| false);
        assert!(ed.client(cid).is_some(), "a file of it came back: it stays");
        // Closed for good: it stops, without a word on screen, and its process is gone — not a zombie
        let id = ed.doc().id;
        ed.close_doc(id);
        settle_for(&mut ed, 5, |ed| ed.client(cid).is_none());
        assert!(ed.client(cid).is_none(), "stopped");
        let said: Vec<&str> = ed.toasts.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(said, ["fake ready"], "nothing on screen about the stop (only its start was said)");
        assert!(!ed.lsp.failed.contains_key("fake"), "a stop isn't a failure");
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while !reaped(pid) && std::time::Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(reaped(pid), "reaped");
        // The next file of its project starts it again
        ed.open(&file).unwrap();
        assert!(ed.doc().lsp.client.is_some_and(|c| c != cid), "a new one");
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    /// A server that exits (crashed, killed) is reaped — no zombie left — and said so; the next file of its
    /// project starts it again. (The test kills its own stand-in server, a `cat`.)
    #[test]
    fn a_server_that_exits_is_reaped_and_started_again_when_needed() {
        let (mut ed, file, _log) = setup("exits", "fn main() {}\n");
        let cid = ready(&mut ed);
        let pid = ed.client(cid).and_then(Client::pid).expect("its process");
        // SAFETY: a signal to the stand-in server this test started.
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        settle_for(&mut ed, 5, |ed| ed.client(cid).is_none());
        assert!(ed.client(cid).is_none(), "noticed");
        assert!(ed.toasts.iter().any(|t| t.text.starts_with("fake exited")), "said");
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while !reaped(pid) && std::time::Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(reaped(pid), "reaped, not a zombie");
        let id = ed.doc().id;
        ed.close_doc(id);
        ed.open(&file).unwrap();
        assert!(ed.doc().lsp.client.is_some_and(|c| c != cid), "started again for the file that needs it");
        assert!(!ed.lsp.failed.contains_key("fake"), "running again: no red dot");
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    /// Every kind of server stops once no file of it is open (a TOML one here, beside rust's): while another
    /// file of its project is open it waits the long wait (`IDLE_STOP`), and once none under its root is,
    /// both go after the short one (#34); rust-analyzer's `initialize` tells it to leave the project's
    /// worktrees alone.
    #[test]
    fn every_server_stops_unused_and_rust_analyzer_skips_worktrees() {
        let dir = std::env::temp_dir().join(format!("tarae-lsp-kinds-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join(".claude/worktrees/w")).unwrap();
        std::fs::write(dir.join(".claude/worktrees/w/.git"), "gitdir: elsewhere").unwrap();
        std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"t\"\n").unwrap();
        let (rust_log, toml_log) = (dir.join("rust.log"), dir.join("toml.log"));
        let cfg = format!(
            "[lsp.rust-analyzer]\ncommand = \"sh\"\nargs = [\"-c\", \"cat > '{}'\"]\n\
             [lsp.tomlfake]\ncommand = \"sh\"\nargs = [\"-c\", \"cat > '{}'\"]\n\
             [lang.rust]\nlsp = [\"rust-analyzer\"]\n[lang.toml]\nlsp = [\"tomlfake\"]\n",
            rust_log.display(),
            toml_log.display()
        );
        let (config, w) = crate::config::parse(&cfg);
        assert!(w.is_empty(), "{w:?}");
        let mut ed = Editor::new(config);
        ed.open(&dir.join("main.rs")).unwrap();
        let rust = ed.doc().lsp.client.expect("rust's server");
        ed.open(&dir.join("Cargo.toml")).unwrap();
        let toml = ed.doc().lsp.client.expect("toml's server");
        assert_ne!(rust, toml);
        let out = sent(&rust_log);
        assert!(
            out.contains(r#""initializationOptions":{"files":{"exclude":[".claude/worktrees"]}}"#),
            "{out}"
        );
        assert!(sent(&toml_log).contains(r#""initializationOptions":null"#), "others get none");
        let pid = ed.client(toml).and_then(Client::pid).expect("its process");
        // Cargo.toml closed, main.rs still open in the same root: toml's server waits the long wait
        let id = ed.doc().id;
        ed.close_doc(id);
        assert!(ed.lsp.unused.contains_key(&toml), "unused, waiting");
        settle_for(&mut ed, 1, |_| false);
        assert!(ed.client(toml).is_some(), "its project still has a file open: it stays a while");
        assert!(ed.client(rust).is_some(), "rust's file is still open: its server stays");
        // main.rs closed too: nothing under the root — both stop soon
        let id = ed.doc().id;
        ed.close_doc(id);
        settle_for(&mut ed, 5, |ed| ed.client(toml).is_none() && ed.client(rust).is_none());
        assert!(ed.client(toml).is_none(), "toml's server stopped once its root had no file open");
        assert!(ed.client(rust).is_none(), "and rust's");
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while !reaped(pid) && std::time::Instant::now() < end {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(reaped(pid), "reaped");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A server that takes its settings by section (pyright) gets the project's left-out folders when it asks —
    /// the section it names, null for the rest — and its `initialize` carries none.
    #[test]
    fn a_server_asking_for_its_settings_gets_what_to_leave_out() {
        let dir = std::env::temp_dir().join(format!("tarae-lsp-config-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("vendor/clone/.git")).unwrap();
        std::fs::write(dir.join("a.py"), "x = 1\n").unwrap();
        let log = dir.join("sent.log");
        let cfg = format!(
            "[lsp.pyright]\ncommand = \"sh\"\nargs = [\"-c\", \"cat > '{}'\"]\n[lang.python]\nlsp = [\"pyright\"]\nalso = []\n",
            log.display()
        );
        let (config, w) = crate::config::parse(&cfg);
        assert!(w.is_empty(), "{w:?}");
        let mut ed = Editor::new(config);
        ed.open(&dir.join("a.py")).unwrap();
        let cid = ready(&mut ed);
        assert!(sent(&log).contains(r#""initializationOptions":null"#));
        let items = json!([{ "section": "python.analysis" }, { "section": "python.pythonPath" }, {}]);
        ed.on_lsp_message(cid, json!({ "jsonrpc": "2.0", "id": 7, "method": "workspace/configuration", "params": { "items": items } }));
        let out = sent(&log);
        let reply =
            out.lines().find(|l| l.contains(r#""id":7"#)).unwrap_or_else(|| panic!("no reply: {out}"));
        let reply: Value = serde_json::from_str(&reply[reply.find('{').unwrap()..]).unwrap();
        let ex = &reply["result"][0]["exclude"];
        assert_eq!(
            ex,
            &json!(["**/node_modules", "**/__pycache__", "**/.*", "**/__editable__.*", "vendor/clone"]),
            "{reply}"
        );
        assert_eq!(reply["result"][1], Value::Null, "nothing set there");
        assert_eq!(reply["result"][2]["python"]["analysis"]["exclude"], *ex, "no section: all of them");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lifecycle_and_incremental_sync() {
        let (mut ed, file, log) = setup("sync", "fn main() {\n    let 타래 = 1;\n}\n");
        assert!(!ed.lsp.clients[0].ready);
        assert!(!ed.lsp_request(Kind::Hover, "textDocument/hover", json!({})), "no requests before ready");
        // Edits before ready are queued
        feed(&mut ed, "jglix<esc>");
        assert!(!sent(&log).contains("didChange"));
        ready(&mut ed);
        assert_eq!(ed.lsp.clients[0].encoding, Encoding::Utf8);
        let out = sent(&log);
        assert!(out.contains("\"method\":\"initialize\""));
        assert!(out.contains("textDocument/didOpen") && out.contains(&lsp::uri(&file)));
        // `gl` = line-end char (';'), x before it: line 1, byte col = "    let 타래 = 1" = 4+4+6+1+1+1+1 = 18
        assert!(
            out.contains(
                r#""range":{"end":{"character":18,"line":1},"start":{"character":18,"line":1}},"text":"x""#
            ),
            "{out}"
        );
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    /// A server beside the main one (`[lang] also`): it gets the document and every change on its own queue,
    /// its diagnostics join the list and replace only its own, a feature the main one is told `except` goes
    /// to it, code actions come from both (the list waits for both), and stopping it leaves the main one.
    #[test]
    fn a_server_beside_the_main_one() {
        let dir = std::env::temp_dir().join(format!("tarae-lsp-also-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.rs");
        std::fs::write(&file, "fn main() {\n    let x = 1;\n}\n").unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"t\"\n").unwrap();
        let (main_log, side_log) = (dir.join("main.log"), dir.join("side.log"));
        let cfg = format!(
            "[lsp.fake]\ncommand = \"sh\"\nargs = [\"-c\", \"cat > '{}'\"]\nexcept = [\"format\"]\n\
             [lsp.side]\ncommand = \"sh\"\nargs = [\"-c\", \"cat > '{}'\"]\n\
             [lang.rust]\nlsp = [\"fake\"]\nalso = [\"side\"]\n",
            main_log.display(),
            side_log.display()
        );
        let (config, w) = crate::config::parse(&cfg);
        assert!(w.is_empty(), "{w:?}");
        let mut ed = Editor::new(config);
        ed.open(&file).unwrap();
        let file = std::fs::canonicalize(&file).unwrap();
        let main = ed.doc().lsp.client.expect("the main one");
        let side = ed.doc().lsp.beside().next().expect("the one beside");
        let init = |ed: &mut Editor, cid, caps: Value| {
            let mut caps = caps;
            caps["positionEncoding"] = json!("utf-8");
            ed.on_lsp_message(cid, json!({ "jsonrpc": "2.0", "id": 0, "result": { "capabilities": caps } }));
        };
        init(&mut ed, main, json!({ "codeActionProvider": true, "documentFormattingProvider": true }));
        init(&mut ed, side, json!({ "codeActionProvider": true, "documentFormattingProvider": true }));
        feed(&mut ed, "jglix<esc>");
        ed.lsp_flush();
        for log in [&main_log, &side_log] {
            let out = sent(log);
            assert!(out.contains("didOpen") && out.contains("didChange"), "{out}");
        }
        // Diagnostics from both; each server's new list replaces only its own
        let publish = |ed: &mut Editor, cid, msgs: &[&str]| {
            let list: Vec<Value> = msgs.iter().map(|m| json!({ "range": { "start": { "line": 1, "character": 8 }, "end": { "line": 1, "character": 9 } }, "severity": 2, "message": m })).collect();
            ed.on_lsp_message(cid, json!({ "method": "textDocument/publishDiagnostics", "params": { "uri": lsp::uri(&file), "diagnostics": list }}));
        };
        publish(&mut ed, main, &["unused"]);
        publish(&mut ed, side, &["F841 local variable assigned but never used"]);
        assert_eq!(ed.doc().lsp.diagnostics.len(), 2);
        publish(&mut ed, side, &[]);
        assert_eq!(
            ed.doc().lsp.diagnostics.iter().map(|d| d.message.as_str()).collect::<Vec<_>>(),
            ["unused"]
        );
        // Formatting: the main one is told not to — the one beside formats
        feed(&mut ed, ":format<ret>");
        assert!(ed.lsp.pending.iter().any(|((c, _), p)| *c == side && p.kind == Kind::Format));
        // Code actions from both, in one list once both have answered
        feed(&mut ed, " a");
        let asked: Vec<(ClientId, u64)> = ed
            .lsp
            .pending
            .iter()
            .filter(|(_, p)| matches!(p.kind, Kind::CodeAction { .. }))
            .map(|(k, _)| *k)
            .collect();
        assert_eq!(asked.len(), 2);
        for (cid, id) in asked {
            let title = if cid == main { "Rename to _x" } else { "Disable F841 for this line" };
            ed.on_lsp_message(cid, json!({ "id": id, "result": [{ "title": title, "kind": "quickfix" }] }));
        }
        let p = ed.picker.as_ref().expect("the list");
        assert_eq!(p.items().len(), 2);
        assert!(p.items().iter().any(|i| i.hint.ends_with("· side")), "says whose");
        ed.picker = None;
        // Stopping the one beside leaves the main one
        ed.lsp_stopped(side);
        assert!(ed.doc().lsp.beside().next().is_none() && ed.doc().lsp.client == Some(main));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Completion with two servers that complete: a word char asks both and their answers merge into one
    /// list as they come (a label both give shows once, the selection stays put); a trigger char asks only
    /// the servers it triggers; an item is resolved by the server that gave it; the one beside stopping
    /// closes a list holding its items.
    #[test]
    fn completion_from_two_servers() {
        let dir = std::env::temp_dir().join(format!("tarae-lsp-comp2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.rs");
        std::fs::write(&file, "fn main() {\n    \n}\n").unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"t\"\n").unwrap();
        let cat =
            |n: &str| format!("command = \"sh\"\nargs = [\"-c\", \"cat > '{}'\"]\n", dir.join(n).display());
        let cfg = format!(
            "[lsp.a]\n{}[lsp.b]\n{}[lang.rust]\nlsp = [\"a\"]\nalso = [\"b\"]\n",
            cat("a.log"),
            cat("b.log")
        );
        let (config, w) = crate::config::parse(&cfg);
        assert!(w.is_empty(), "{w:?}");
        let mut ed = Editor::new(config);
        ed.open(&file).unwrap();
        let a = ed.doc().lsp.client.unwrap();
        let b = ed.doc().lsp.beside().next().unwrap();
        let caps = [
            json!({ "positionEncoding": "utf-8", "completionProvider": { "triggerCharacters": ["."] } }),
            json!({ "positionEncoding": "utf-8", "completionProvider": { "resolveProvider": true } }),
        ];
        for (cid, caps) in [a, b].into_iter().zip(caps) {
            ed.on_lsp_message(cid, json!({ "jsonrpc": "2.0", "id": 0, "result": { "capabilities": caps } }));
        }
        let asked = |ed: &Editor| -> Vec<(ClientId, u64)> {
            let mut v: Vec<_> =
                ed.lsp.pending.iter().filter(|(_, p)| p.kind == Kind::Completion).map(|(k, _)| *k).collect();
            v.sort();
            v
        };
        let item = |l: &str| json!({ "label": l, "kind": 3 });
        feed(&mut ed, "jAp");
        let round = asked(&ed);
        assert_eq!(round.iter().map(|r| r.0).collect::<Vec<_>>(), [a, b], "a word char asks both");
        ed.on_lsp_message(a, json!({ "id": round[0].1, "result": [item("push"), item("pop")] }));
        let labels = |ed: &Editor| -> Vec<String> {
            let c = ed.completion.as_ref().unwrap();
            (0..c.shown.len()).map(|i| c.item(i).label.clone()).collect()
        };
        assert_eq!(labels(&ed).len(), 2, "a's answer shows at once");
        feed(&mut ed, "<C-n>");
        let chosen = ed.completion.as_ref().unwrap().current().unwrap().label.clone();
        ed.on_lsp_message(b, json!({ "id": round[1].1, "result": [item("pop"), item("print")] }));
        let mut got = labels(&ed);
        got.sort();
        assert_eq!(got, ["pop", "print", "push"], "merged, pop once");
        assert_eq!(ed.completion.as_ref().unwrap().current().unwrap().label, chosen, "the selection stays");
        // b's item is resolved by b
        while ed.completion.as_ref().unwrap().current().unwrap().label != "print" {
            feed(&mut ed, "<C-n>");
        }
        assert!(
            ed.lsp
                .pending
                .iter()
                .any(|((c, _), p)| *c == b && matches!(p.kind, Kind::ResolveCompletion { .. }))
        );
        // A trigger char: only a lists "."
        feed(&mut ed, "<esc>A.");
        let round2: Vec<ClientId> =
            asked(&ed).into_iter().filter(|r| !round.contains(r)).map(|r| r.0).collect();
        assert_eq!(round2, [a], "only the server the char triggers");
        // b is slow: a new word asks again without waiting for it; a list closed before b answers stays closed
        feed(&mut ed, "<esc>o");
        ed.lsp.completion_waiting.clear();
        feed(&mut ed, "x");
        let first: Vec<(ClientId, u64)> = asked(&ed);
        feed(&mut ed, " y");
        assert!(asked(&ed).len() > first.len(), "b's silence doesn't hold back the next word");
        let latest =
            |ed: &Editor, cid| asked(ed).into_iter().filter(|r| r.0 == cid).map(|r| r.1).max().unwrap();
        ed.on_lsp_message(a, json!({ "id": latest(&ed, a), "result": [item("yes")] }));
        assert!(ed.completion.is_some());
        feed(&mut ed, "<left>");
        assert!(ed.completion.is_none(), "closed");
        ed.on_lsp_message(b, json!({ "id": latest(&ed, b), "result": [item("yield")] }));
        assert!(ed.completion.is_none(), "b's answer doesn't bring it back");
        // b goes: a list holding its items closes
        ed.lsp.completion_waiting.clear();
        feed(&mut ed, "<C-x>");
        let round3: Vec<(ClientId, u64)> = asked(&ed).into_iter().filter(|r| r.0 == b).collect();
        let id = round3.iter().map(|r| r.1).max().unwrap();
        ed.on_lsp_message(b, json!({ "id": id, "result": [item("print")] }));
        assert!(ed.completion.is_some());
        ed.lsp_stopped(b);
        assert!(ed.completion.is_none(), "b's items can't be resolved or applied any more");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A main server that doesn't say it completes still answers `C-x` — and that list filters and closes as
    /// you type like any other.
    #[test]
    fn an_explicit_list_from_a_server_that_does_not_advertise_completion_filters() {
        let (mut ed, file, _log) = setup("comp-noad", "fn main() {\n    \n}\n");
        let cid = ready(&mut ed);
        feed(&mut ed, "jA<C-x>");
        let id = pending_of(&ed, Kind::Completion);
        ed.on_lsp_message(cid, json!({ "id": id, "result": [{ "label": "alpha" }, { "label": "beta" }] }));
        assert_eq!(ed.completion.as_ref().map(|c| c.shown.len()), Some(2));
        feed(&mut ed, "al");
        assert_eq!(ed.completion.as_ref().map(|c| c.shown.len()), Some(1), "filtered");
        feed(&mut ed, " ");
        assert!(ed.completion.is_none(), "closed on leaving the word");
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    /// From the review: a server that is both the main one and in `also` is attached once; code actions
    /// asked for and then left (the cursor moved) don't pop up later; one server's error leaves the other's.
    #[test]
    fn several_servers_edge_cases() {
        let dir = std::env::temp_dir().join(format!("tarae-lsp-edge-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.rs");
        std::fs::write(&file, "fn main() {\n    let x = 1;\n}\n").unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"t\"\n").unwrap();
        let cat =
            |n: &str| format!("command = \"sh\"\nargs = [\"-c\", \"cat > '{}'\"]\n", dir.join(n).display());
        let cfg = format!(
            "[lsp.a]\n{}[lsp.b]\n{}[lang.rust]\nlsp = [\"a\"]\nalso = [\"a\", \"b\"]\n",
            cat("a.log"),
            cat("b.log")
        );
        let (config, w) = crate::config::parse(&cfg);
        assert!(w.is_empty(), "{w:?}");
        let mut ed = Editor::new(config);
        ed.open(&file).unwrap();
        let a = ed.doc().lsp.client.unwrap();
        let also: Vec<ClientId> = ed.doc().lsp.beside().collect();
        assert_eq!(also.len(), 1, "a is the main one — only b beside it");
        assert_ne!(also[0], a);
        let b = also[0];
        for cid in [a, b] {
            let caps = json!({ "positionEncoding": "utf-8", "codeActionProvider": true });
            ed.on_lsp_message(cid, json!({ "jsonrpc": "2.0", "id": 0, "result": { "capabilities": caps } }));
        }
        let asked = |ed: &Editor| -> Vec<(ClientId, u64)> {
            ed.lsp
                .pending
                .iter()
                .filter(|(_, p)| matches!(p.kind, Kind::CodeAction { .. }))
                .map(|(k, _)| *k)
                .collect()
        };
        // Asked, then moved on: the answers don't open anything
        feed(&mut ed, " a");
        feed(&mut ed, "j");
        for (cid, id) in asked(&ed) {
            ed.on_lsp_message(cid, json!({ "id": id, "result": [{ "title": "fix" }] }));
        }
        assert!(ed.picker.is_none(), "the list for where the cursor was doesn't show");
        // One fails, the other's list shows
        feed(&mut ed, " a");
        for (cid, id) in asked(&ed) {
            let msg = if cid == a {
                json!({ "id": id, "error": { "code": -32603, "message": "boom" } })
            } else {
                json!({ "id": id, "result": [{ "title": "from b" }] })
            };
            ed.on_lsp_message(cid, msg);
        }
        let p = ed.picker.as_ref().expect("b's list");
        assert_eq!(p.items().iter().map(|i| i.label.as_str()).collect::<Vec<_>>(), ["from b"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn diagnostics_follow_edits_and_navigate() {
        let (mut ed, file, _log) = setup("diag", "fn main() {\n    let x = 1;\n}\n");
        let cid = ready(&mut ed);
        let diag = json!({ "method": "textDocument/publishDiagnostics", "params": { "uri": lsp::uri(&file), "diagnostics": [
            { "range": { "start": { "line": 1, "character": 8 }, "end": { "line": 1, "character": 9 } }, "severity": 2, "message": "unused variable: `x`" }
        ]}});
        ed.on_lsp_message(cid, diag);
        assert_eq!(ed.doc().diagnostic_counts(), [0, 1]);
        feed(&mut ed, "]d");
        assert_eq!(ed.doc().selection().primary(), Range::new(20, 21), "selects the diagnostic range");
        assert_eq!(ed.status.as_ref().map(|s| s.0.as_str()), Some("unused variable: `x`"));
        // Inserting a line above moves the diagnostic down too
        feed(&mut ed, "ggO// hi<esc>");
        assert_eq!(ed.doc().lsp.diagnostics[0].from, 26);
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    /// Latest request of this kind (payloads aren't compared).
    fn pending_of(ed: &Editor, kind: Kind) -> u64 {
        ed.lsp
            .pending
            .iter()
            .filter(|(_, p)| std::mem::discriminant(&p.kind) == std::mem::discriminant(&kind))
            .map(|((_, id), _)| *id)
            .max()
            .expect("request sent")
    }

    #[test]
    fn code_action_rename_format_and_server_edits() {
        let (mut ed, file, log) = setup("edit", "fn main() {\n    let x = 1;\n}\n");
        let cid = ready(&mut ed);
        let uri = lsp::uri(&file);
        let edit = |line: u64, from: u64, to: u64, text: &str| json!({ "range": { "start": { "line": line, "character": from }, "end": { "line": line, "character": to } }, "newText": text });
        // Code action: list → pick in picker → apply edit, one undo reverts it
        feed(&mut ed, "j8l<space>a");
        assert!(sent(&log).contains("textDocument/codeAction"));
        let id = pending_of(&ed, Kind::CodeAction { list: 0 });
        let actions = json!([
            { "title": "Rename to _x", "kind": "quickfix", "edit": { "changes": { uri.clone(): [edit(1, 8, 9, "_x")] } } },
            { "title": "Inline", "kind": "refactor.inline", "edit": { "changes": {} } }
        ]);
        ed.on_lsp_message(cid, json!({ "id": id, "result": actions }));
        assert_eq!(ed.picker.as_ref().map(|p| p.counts()), Some((2, 2)));
        feed(&mut ed, "<ret>");
        assert_eq!(ed.doc().text.to_string(), "fn main() {\n    let _x = 1;\n}\n");
        feed(&mut ed, "u");
        assert_eq!(ed.doc().text.to_string(), "fn main() {\n    let x = 1;\n}\n", "applying is one undo");
        // Rename: prompt (prefilled with the word under cursor) → request → edits
        feed(&mut ed, "<space>r");
        assert_eq!(ed.prompt.as_ref().map(|p| p.text.as_str()), Some("x"));
        feed(&mut ed, "<backspace>count<ret>");
        let id = pending_of(&ed, Kind::Rename);
        // Versioned edits for an older version of the doc (we've sent 2: the action, then the undo) → refused
        let stale = json!({ "documentChanges": [
            { "textDocument": { "uri": uri.clone(), "version": 1 }, "edits": [edit(1, 8, 9, "count")] }
        ]});
        assert!(ed.apply_workspace_edit(cid, &stale).is_err());
        assert_eq!(ed.doc().text.to_string(), "fn main() {\n    let x = 1;\n}\n", "nothing applied");
        // `changes` alongside `documentChanges` is ignored (spec)
        ed.on_lsp_message(
            cid,
            json!({ "id": id, "result": {
                "changes": { uri.clone(): [edit(0, 0, 2, "XX")] },
                "documentChanges": [
                    { "textDocument": { "uri": uri.clone(), "version": 2 }, "edits": [edit(1, 8, 9, "count")] }
                ]
            }}),
        );
        assert_eq!(ed.doc().text.to_string(), "fn main() {\n    let count = 1;\n}\n");
        // Format
        feed(&mut ed, ":fmt<ret>");
        let id = pending_of(&ed, Kind::Format);
        ed.on_lsp_message(cid, json!({ "id": id, "result": [edit(1, 0, 4, "  ")] }));
        assert!(ed.doc().text.to_string().contains("\n  let count = 1;"));
        // Server-initiated edit (workspace/applyEdit) → apply and reply applied: true
        ed.on_lsp_message(cid, json!({ "id": 77, "method": "workspace/applyEdit", "params": { "edit": { "changes": { uri: [edit(0, 3, 7, "start")] } } } }));
        assert!(ed.doc().text.to_string().starts_with("fn start()"));
        assert!(
            sent(&log).contains(r#""id":77,"jsonrpc":"2.0","result":{"applied":true}"#),
            "{}",
            sent(&log)
        );
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    /// Apply worker-thread results until the condition holds.
    fn settle_until(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
        let end = std::time::Instant::now() + Duration::from_secs(5);
        while !done(ed) {
            let left = end.saturating_duration_since(std::time::Instant::now());
            let ev = ed.events.recv_timeout(left).expect("event before timeout");
            ed.handle_event(ev);
        }
    }

    #[test]
    fn code_action_preview_resolves_lazily_and_shows_the_diff() {
        let (mut ed, file, log) = setup("preview", "fn main() {\n    let x = 1;\n}\n");
        let cid = ready(&mut ed);
        let uri = lsp::uri(&file);
        feed(&mut ed, "j8l<space>a");
        let id = pending_of(&ed, Kind::CodeAction { list: 0 });
        // The first is a lazy action (no edit + data) — once selected, only its edit is fetched via resolve
        let actions = json!([
            { "title": "Rename to _x", "kind": "quickfix", "data": { "n": 1 } },
            { "title": "Run tests", "command": "rust-analyzer.runSingle" }
        ]);
        ed.on_lsp_message(cid, json!({ "id": id, "result": actions }));
        assert!(matches!(ed.action_preview(0), Some(ActionPreview::Working)));
        assert!(sent(&log).contains("codeAction/resolve"));
        let rid = pending_of(&ed, Kind::ResolvePreview { list: 0, index: 0 });
        let resolved = json!({ "title": "Rename to _x", "edit": { "changes": { uri.clone(): [
            { "range": { "start": { "line": 1, "character": 8 }, "end": { "line": 1, "character": 9 } }, "newText": "_x" }
        ] } } });
        ed.on_lsp_message(cid, json!({ "id": rid, "result": resolved }));
        settle_until(&mut ed, |ed| matches!(ed.action_preview(0), Some(ActionPreview::Ready(_))));
        let Some(ActionPreview::Ready(files)) = ed.action_preview(0) else { unreachable!() };
        assert_eq!(crate::editdiff::summary(files), "1 file  +1 −1");
        assert_eq!(ed.doc().text.to_string(), "fn main() {\n    let x = 1;\n}\n", "preview doesn't modify");
        // The diff is drawn in the right pane (wide screen)
        let mut buf = Vec::new();
        crate::term::render(&mut ed, &mut buf, 140, 20).unwrap();
        let screen = String::from_utf8_lossy(&buf);
        assert!(
            screen.contains("main.rs  +1 −1") && screen.contains("− ") && screen.contains("+ "),
            "{screen}"
        );
        // Command-only actions have nothing to preview
        feed(&mut ed, "<down>");
        assert!(matches!(ed.action_preview(1), Some(ActionPreview::CommandOnly)));
        // Picking applies the stored edit directly (no re-ask)
        feed(&mut ed, "<up><ret>");
        assert_eq!(ed.doc().text.to_string(), "fn main() {\n    let _x = 1;\n}\n");
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn workspace_edit_creates_renames_and_deletes_files() {
        let (mut ed, file, _log) = setup("fileops", "mod util;\nfn main() {}\n");
        let cid = ready(&mut ed);
        let dir = file.parent().unwrap().to_path_buf();
        std::fs::write(dir.join("util.rs"), "pub fn f() {}\n").unwrap();
        std::fs::write(dir.join("junk.rs"), "x").unwrap();
        ed.open(&dir.join("util.rs")).unwrap();
        ed.open(&dir.join("junk.rs")).unwrap();
        ed.current = 0;
        let u = |n: &str| lsp::uri(&dir.join(n));
        let we = json!({ "documentChanges": [
            { "kind": "create", "uri": u("new/extra.rs") },
            { "textDocument": { "uri": u("new/extra.rs"), "version": null }, "edits": [
                { "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } }, "newText": "pub fn g() {}\n" }
            ] },
            { "kind": "rename", "oldUri": u("util.rs"), "newUri": u("helpers.rs") },
            { "kind": "delete", "uri": u("junk.rs") },
        ] });
        assert_eq!(ed.apply_workspace_edit(cid, &we), Ok(4));
        // Created file was opened and edited (saving is up to the user)
        let extra =
            ed.docs.iter().find(|d| d.path.as_deref().is_some_and(|p| p.ends_with("new/extra.rs"))).unwrap();
        assert_eq!(extra.text.to_string(), "pub fn g() {}\n");
        assert!(dir.join("new/extra.rs").exists());
        // Renamed file: on disk and the open buffer's name
        assert!(!dir.join("util.rs").exists() && dir.join("helpers.rs").exists());
        assert!(ed.docs.iter().any(|d| d.path.as_deref().is_some_and(|p| p.ends_with("helpers.rs"))));
        // Deleted file's buffer is closed too
        assert!(!dir.join("junk.rs").exists());
        assert!(!ed.docs.iter().any(|d| d.path.as_deref().is_some_and(|p| p.ends_with("junk.rs"))));
        assert_eq!(ed.doc().path.as_deref(), Some(file.as_path()), "the viewed buffer stays");
        // Creating an existing file is refused (without the overwrite option)
        let again = json!({ "documentChanges": [{ "kind": "create", "uri": u("helpers.rs") }] });
        assert!(ed.apply_workspace_edit(cid, &again).is_err());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn completion_request_filter_and_accept() {
        let (mut ed, file, log) = setup("comp", "fn main() {\n    let v = Vec::new();\n    \n}\n");
        let cid = ed.doc().lsp.client.unwrap();
        let caps =
            json!({ "positionEncoding": "utf-8", "completionProvider": { "triggerCharacters": ["."] } });
        ed.on_lsp_message(cid, json!({ "id": 0, "result": { "capabilities": caps } }));
        // Ask at '.' (trigger), and filter with chars typed before the response arrives
        feed(&mut ed, "jjAv.pu");
        assert!(sent(&log).contains(r#""triggerCharacter":".""#), "{}", sent(&log));
        let id = pending_of(&ed, Kind::Completion);
        let items = json!({ "isIncomplete": false, "items": [
            { "label": "push", "kind": 2, "insertTextFormat": 2, "detail": "fn(&mut self, value: T)", "textEdit": {
                "range": { "start": { "line": 2, "character": 6 }, "end": { "line": 2, "character": 8 } },
                "newText": "push(${1:value})$0" },
              "additionalTextEdits": [{ "range": { "start": { "line": 0, "character": 0 }, "end": { "line": 0, "character": 0 } }, "newText": "use x;\n" }] },
            { "label": "pop", "kind": 2 },
            { "label": "len", "kind": 2 },
        ]});
        ed.on_lsp_message(cid, json!({ "id": id, "result": items }));
        let c = ed.completion.as_ref().expect("list shows");
        assert_eq!(c.shown.len(), 1, "\"pu\" filters to push only");
        assert!(c.current().is_none());
        feed(&mut ed, "x");
        assert!(ed.completion.is_none(), "closes when nothing matches");
        feed(&mut ed, "<backspace>");
        assert!(ed.completion.is_none(), "a closed list doesn't reopen on delete");
        feed(&mut ed, "<C-x>");
        ed.on_lsp_message(cid, json!({ "id": pending_of(&ed, Kind::Completion), "result": items }));
        assert!(ed.completion.is_some(), "C-x asks again");
        // Pick with Tab, insert with Enter: snippet expanded + extra edits, cursor at the first placeholder
        feed(&mut ed, "<tab>");
        let docs = ed.completion.as_ref().and_then(|c| c.docs.as_ref()).expect("docs show when selected");
        assert!(!docs.1.is_empty());
        feed(&mut ed, "<ret>");
        assert_eq!(
            ed.doc().text.to_string(),
            "use x;\nfn main() {\n    let v = Vec::new();\n    v.push(value)\n}\n"
        );
        feed(&mut ed, "!<esc>");
        assert!(ed.doc().text.to_string().contains("v.push(!value)"));
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn late_resolve_for_an_older_completion_list_is_dropped() {
        let (mut ed, file, _log) = setup("resolve", "fn main() {\n    \n}\n");
        let cid = ed.doc().lsp.client.unwrap();
        let caps = json!({ "positionEncoding": "utf-8", "completionProvider": { "resolveProvider": true } });
        ed.on_lsp_message(cid, json!({ "id": 0, "result": { "capabilities": caps } }));
        let list = |labels: &[&str]| json!({ "items": labels.iter().map(|l| json!({ "label": l, "kind": 3 })).collect::<Vec<_>>() });
        feed(&mut ed, "jA<C-x>");
        ed.on_lsp_message(
            cid,
            json!({ "id": pending_of(&ed, Kind::Completion), "result": list(&["alpha"]) }),
        );
        feed(&mut ed, "<tab>");
        let resolve = pending_of(&ed, Kind::ResolveCompletion { list: 0, index: 0 });
        // A new list before the resolve reply — same index, different item
        feed(&mut ed, "<C-x>");
        ed.on_lsp_message(cid, json!({ "id": pending_of(&ed, Kind::Completion), "result": list(&["beta"]) }));
        let old = json!({ "label": "alpha", "documentation": "alpha docs" });
        ed.on_lsp_message(cid, json!({ "id": resolve, "result": old }));
        let c = ed.completion.as_ref().expect("new list shows");
        assert_eq!(c.item(0).label, "beta");
        assert!(c.item(0).raw["documentation"].is_null(), "alpha's docs didn't land on beta");
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn document_symbols_for_a_buffer_no_longer_shown_are_dropped() {
        let (mut ed, file, _log) = setup("symbols", "fn main() {}\n");
        let cid = ready(&mut ed);
        ed.document_symbols();
        let id = pending_of(&ed, Kind::DocumentSymbols);
        let other = file.with_file_name("other.txt");
        std::fs::write(&other, "text\n").unwrap();
        ed.open(&other).unwrap();
        let sym = json!([{ "name": "main", "kind": 12, "selectionRange": { "start": { "line": 0, "character": 3 } } }]);
        ed.on_lsp_message(cid, json!({ "id": id, "result": sym }));
        assert!(ed.picker.is_none(), "main.rs symbols don't open over other.txt");
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn resolved_code_action_is_dropped_if_the_file_changed() {
        let (mut ed, file, _log) = setup("stale-action", "fn main() {\n    let x = 1;\n}\n");
        let cid = ready(&mut ed);
        let uri = lsp::uri(&file);
        feed(&mut ed, "j8l<space>a");
        let lazy = json!([{ "title": "Rename to _x", "kind": "quickfix", "data": { "n": 1 } }]);
        ed.on_lsp_message(
            cid,
            json!({ "id": pending_of(&ed, Kind::CodeAction { list: 0 }), "result": lazy }),
        );
        // Preview's resolve fails, so picking asks again (ResolveAction)
        let rid = pending_of(&ed, Kind::ResolvePreview { list: 0, index: 0 });
        ed.on_lsp_message(cid, json!({ "id": rid, "error": { "message": "later" } }));
        feed(&mut ed, "<ret>");
        let id = pending_of(&ed, Kind::ResolveAction);
        feed(&mut ed, "ggOuse a;<esc>");
        let resolved = json!({ "title": "Rename to _x", "edit": { "changes": { uri: [
            { "range": { "start": { "line": 1, "character": 8 }, "end": { "line": 1, "character": 9 } }, "newText": "_x" }
        ] } } });
        ed.on_lsp_message(cid, json!({ "id": id, "result": resolved }));
        assert_eq!(
            ed.doc().text.to_string(),
            "use a;\nfn main() {\n    let x = 1;\n}\n",
            "not at stale offsets"
        );
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn utf16_server_gets_incremental_changes_from_the_first_edit() {
        let (mut ed, file, log) = setup("u16", "fn main() {}\n");
        let cid = ed.doc().lsp.client.unwrap();
        ed.on_lsp_message(cid, json!({ "id": 0, "result": { "capabilities": {} } }));
        assert_eq!(ed.lsp.clients[0].encoding, Encoding::Utf16);
        feed(&mut ed, "i타<esc>");
        let out = sent(&log);
        assert!(out.contains(r#""contentChanges":[{"range""#), "incremental, not full sync: {out}");
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn server_exit_clears_its_state() {
        let (mut ed, file, _log) = setup("exit", "fn main() {}\n");
        let cid = ready(&mut ed);
        feed(&mut ed, "i<C-x>");
        assert!(!ed.lsp.completion_waiting.is_empty() && !ed.lsp.pending.is_empty());
        ed.lsp_exited(cid);
        assert!(ed.lsp.completion_waiting.is_empty() && ed.lsp.pending.is_empty());
        assert!(ed.lsp.clients.is_empty() && ed.doc().lsp.client.is_none());
        assert!(ed.status.as_ref().is_some_and(|s| s.0.contains("fake exited")));
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn inlay_hints_request_map_and_draw() {
        let (mut ed, file, log) = setup("inlay", "fn main() {\n    let x = add(1, 2);\n}\n");
        let cid = ed.doc().lsp.client.unwrap();
        let caps = json!({ "positionEncoding": "utf-8", "inlayHintProvider": true });
        ed.on_lsp_message(cid, json!({ "id": 0, "result": { "capabilities": caps } }));
        ed.viewport = (10, 80);
        ed.inlay_request();
        assert!(sent(&log).contains("textDocument/inlayHint"));
        assert!(!ed.inlay_stale(), "no re-ask while in flight");
        let hints = json!([
            { "position": { "line": 1, "character": 9 }, "label": ": i32", "kind": 1 },
            { "position": { "line": 1, "character": 16 }, "label": [{ "value": "a" }, { "value": ":" }], "paddingRight": true },
        ]);
        ed.on_lsp_message(
            cid,
            json!({ "id": pending_of(&ed, Kind::InlayHint { range: (0, 0) }), "result": hints }),
        );
        let pos: Vec<usize> = ed.doc().lsp.inlay.iter().map(|h| h.pos).collect();
        assert_eq!(pos, [21, 28]);
        assert_eq!(ed.doc().lsp.inlay[1].text, "a: ");
        assert!(!ed.inlay_stale(), "same version and range → no re-ask");
        // Typing before it pushes it along, and the version changed, so it's time to re-ask
        feed(&mut ed, "jwiy<esc>");
        assert_eq!(ed.doc().lsp.inlay.iter().map(|h| h.pos).collect::<Vec<_>>(), [22, 29]);
        assert!(ed.inlay_stale());
        // On screen: `let yx: i32 = add(a: 1, 2);`
        let mut buf = Vec::new();
        crate::term::render(&mut ed, &mut buf, 80, 8).unwrap();
        let screen = String::from_utf8_lossy(&buf);
        assert!(screen.contains(": i32") && screen.contains("a: "), "{screen}");
        // Soft-wrapped, too wide for both: the one that doesn't fit whole stays out — never half a type
        ed.doc_mut().lsp.inlay[0].text = ": HashMap<String, Vec<u32>>".into();
        let frame = |ed: &mut Editor, w| {
            let mut buf = Vec::new();
            crate::term::render(ed, &mut buf, w, 8).unwrap();
            let mut p = vt100::Parser::new(8, w, 0);
            p.process(&buf);
            p.screen().contents()
        };
        let narrow = frame(&mut ed, 42);
        assert!(!narrow.contains(": Hash"), "{narrow}");
        assert!(narrow.contains("a: "), "the short one still fits: {narrow}");
        assert!(frame(&mut ed, 80).contains(": HashMap<String, Vec<u32>>"), "whole when it fits");
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn signature_help_follows_typing() {
        let (mut ed, file, log) = setup("sig", "fn main() {\n    \n}\n");
        let cid = ed.doc().lsp.client.unwrap();
        let caps = json!({ "positionEncoding": "utf-8", "signatureHelpProvider": { "triggerCharacters": ["(", ","] } });
        ed.on_lsp_message(cid, json!({ "id": 0, "result": { "capabilities": caps } }));
        let help = |active: u64| {
            json!({ "signatures": [{ "label": "fn add(a: i32, b: &str)",
            "parameters": [{ "label": [7, 13] }, { "label": [15, 22] }] }], "activeParameter": active })
        };
        feed(&mut ed, "jAadd");
        assert!(ed.lsp.pending.values().all(|p| p.kind != Kind::SignatureHelp), "no ask before a trigger");
        feed(&mut ed, "(");
        assert!(sent(&log).contains(r#""triggerCharacter":"(""#));
        ed.on_lsp_message(cid, json!({ "id": pending_of(&ed, Kind::SignatureHelp), "result": help(0) }));
        assert_eq!(ed.signature.as_ref().and_then(|s| s.active), Some((7, 13)));
        // While shown, re-ask on every edit. Typing again (",") while in flight → stale reply: drop, re-ask
        feed(&mut ed, "1");
        let stale = pending_of(&ed, Kind::SignatureHelp);
        feed(&mut ed, ",");
        ed.on_lsp_message(cid, json!({ "id": stale, "result": help(0) }));
        let fresh = pending_of(&ed, Kind::SignatureHelp);
        assert!(fresh > stale, "re-asked after the stale response");
        ed.on_lsp_message(cid, json!({ "id": fresh, "result": help(1) }));
        assert_eq!(ed.signature.as_ref().and_then(|s| s.active), Some((15, 22)), "next argument");
        // Leaving the parens → server replies empty → closes. Leaving insert mode closes too
        feed(&mut ed, "2)");
        for _ in 0..2 {
            // The reply to "2" is stale (")" was typed after) — the re-asked one is the real one
            ed.on_lsp_message(cid, json!({ "id": pending_of(&ed, Kind::SignatureHelp), "result": null }));
        }
        assert!(ed.signature.is_none());
        feed(&mut ed, "(");
        ed.on_lsp_message(cid, json!({ "id": pending_of(&ed, Kind::SignatureHelp), "result": help(0) }));
        assert!(ed.signature.is_some());
        feed(&mut ed, "<esc>");
        assert!(ed.signature.is_none());
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn goto_kinds_ask_their_method_and_check_the_server_can() {
        let (mut ed, file, log) = setup("gotos", "trait T {}\nstruct S;\nimpl T for S {}\n");
        let cid = ed.doc().lsp.client.unwrap();
        let caps = json!({ "positionEncoding": "utf-8", "implementationProvider": true, "typeDefinitionProvider": {} });
        ed.on_lsp_message(cid, json!({ "id": 0, "result": { "capabilities": caps } }));
        // gD: the server didn't say it does declarations — nothing sent, the toast says why
        feed(&mut ed, "6lgD");
        assert!(ed.lsp.pending.is_empty());
        assert!(ed.status.as_ref().is_some_and(|(m, _)| m.contains("declarations")), "{:?}", ed.status);
        // gi: several → picker titled after the kind
        feed(&mut ed, "gi");
        assert!(sent(&log).contains(r#""method":"textDocument/implementation""#));
        let at = |line: u64| json!({ "uri": lsp::uri(&file), "range": { "start": { "line": line, "character": 0 }, "end": { "line": line, "character": 1 } } });
        let id = pending_of(&ed, Kind::Goto(Goto::Implementation));
        ed.on_lsp_message(cid, json!({ "id": id, "result": [at(1), at(2)] }));
        settle_for(&mut ed, 5, |ed| ed.picker.is_some());
        assert_eq!(ed.picker.as_ref().map(|p| p.title.as_str()), Some("implementations"));
        feed(&mut ed, "<esc>");
        // gy: one → straight there, and C-o comes back
        feed(&mut ed, "gy");
        assert!(sent(&log).contains(r#""method":"textDocument/typeDefinition""#));
        let id = pending_of(&ed, Kind::Goto(Goto::TypeDefinition));
        ed.on_lsp_message(cid, json!({ "id": id, "result": at(1) }));
        assert_eq!(ed.doc().selection().primary(), Range::point(11));
        feed(&mut ed, "<C-o>");
        assert_eq!(ed.doc().selection().primary(), Range::point(6), "back on `T`");
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    /// N2: `g r` lists the code at each place — the line trimmed, the name standing out (whole word: `a`,
    /// not the `a` in `crate`) — open files from their buffer (unsaved text too), the rest from disk; by
    /// file then line, the declaration (the server's definition, asked beside them) first and marked, even
    /// when that answer comes after the list is up.
    #[test]
    fn references_list_the_code_with_the_declaration_first() {
        let (mut ed, file, log) = setup("refs-code", "fn b() { a() }\nfn c() { a(); a() }\n");
        let other = file.parent().unwrap().join("other.rs");
        std::fs::write(&other, "// other\nuse crate::a;\n\npub fn a() {}\n").unwrap();
        let cid = ed.doc().lsp.client.unwrap();
        let caps =
            json!({ "positionEncoding": "utf-8", "definitionProvider": true, "referencesProvider": true });
        ed.on_lsp_message(cid, json!({ "id": 0, "result": { "capabilities": caps } }));
        // Unsaved: the buffer's text is what the row shows
        feed(&mut ed, "A // edited<esc>");
        ed.doc_mut().set_selection(Selection::point(9));
        feed(&mut ed, "gr");
        assert_eq!(ed.lsp.refs.name, "a");
        let out = sent(&log);
        assert!(out.contains("textDocument/references") && out.contains("textDocument/definition"), "{out}");
        let at = |path: &PathBuf, line: u64, col: u64| {
            json!({ "uri": lsp::uri(path), "range": {
            "start": { "line": line, "character": col }, "end": { "line": line, "character": col + 1 } } })
        };
        // The references first, out of order — the declaration's answer after the list is up
        let id = pending_of(&ed, Kind::Goto(Goto::References));
        let refs =
            json!([at(&file, 1, 9), at(&other, 3, 7), at(&file, 0, 9), at(&other, 1, 11), at(&file, 1, 14)]);
        ed.on_lsp_message(cid, json!({ "id": id, "result": refs }));
        settle_for(&mut ed, 5, |ed| ed.picker.is_some());
        // (a row: its label, hint and what stands out in it)
        type Shown = (String, String, Vec<(u32, u32)>);
        let rows = |ed: &mut Editor| -> Vec<Shown> {
            let p = ed.picker.as_mut().expect("the list");
            p.visible(20).into_iter().map(|r| (r.label, r.hint, r.spots)).collect()
        };
        // (past the folder's heading — the temp folder is outside the project)
        let labels = |rows: &[Shown]| -> Vec<String> {
            rows.iter()
                .skip(1)
                .map(|r| if r.1.is_empty() { r.0.clone() } else { format!("{} [{}]", r.0, r.1) })
                .collect()
        };
        assert_eq!(
            labels(&rows(&mut ed)),
            [
                "main.rs [3]",
                "1: fn b() { a() } // edited",
                "2: fn c() { a(); a() }",
                "2: fn c() { a(); a() }",
                "other.rs [2]",
                "2: use crate::a;",
                "4: pub fn a() {}",
            ],
            "by file then line, each its code"
        );
        let id = pending_of(&ed, Kind::Declared { generation: 0 });
        ed.on_lsp_message(cid, json!({ "id": id, "result": at(&other, 3, 7) }));
        let shown = rows(&mut ed);
        assert_eq!(
            labels(&shown),
            [
                "other.rs [2]",
                "4: pub fn a() {} [decl]",
                "2: use crate::a;",
                "main.rs [3]",
                "1: fn b() { a() } // edited",
                "2: fn c() { a(); a() }",
                "2: fn c() { a(); a() }",
            ],
            "the declaration first"
        );
        // The name stands out, whole words only: `a` in `crate::a`, not the one in `crate`
        assert_eq!(shown[3].2, [(14, 15)], "{:?}", shown[3]);
        assert_eq!(shown[6].2, [(12, 13), (17, 18)], "{:?}", shown[6]);
        // The other way round: the declaration known before the references come — first from the start
        feed(&mut ed, "<esc>gr");
        let id = pending_of(&ed, Kind::Declared { generation: 0 });
        ed.on_lsp_message(cid, json!({ "id": id, "result": [at(&other, 3, 7)] }));
        let id = pending_of(&ed, Kind::Goto(Goto::References));
        ed.on_lsp_message(cid, json!({ "id": id, "result": refs }));
        settle_for(&mut ed, 5, |ed| ed.picker.is_some());
        assert_eq!(labels(&rows(&mut ed))[..2], ["other.rs [2]", "4: pub fn a() {} [decl]"]);
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    /// N3: `g r` while the server is loading lists the name's text matches at once (whole word, case as
    /// written — not `ab`, not `A`), titled as such; the server's answer takes their place, said so. Left
    /// (closed) before the answer came, the text matches aren't followed by a list popping up.
    #[test]
    fn references_while_loading_show_text_matches_until_the_answer() {
        let (mut ed, file, _log) = setup("refs-loading", "fn a() {}\nfn b() { a() }\n// ab, A\n");
        let dir = file.parent().unwrap().to_path_buf();
        std::fs::write(dir.join("other.rs"), "use a;\nlet ab = 1;\n").unwrap();
        let cid = ready(&mut ed);
        let progress = |kind: &str| {
            json!({ "method": "$/progress", "params": { "token": "rustAnalyzer/Indexing",
                "value": { "kind": kind, "title": "Indexing" } } })
        };
        ed.on_lsp_message(cid, progress("begin"));
        ed.doc_mut().set_selection(Selection::point(3));
        feed(&mut ed, "gr");
        settle_for(&mut ed, 5, |ed| ed.picker.is_some());
        let shown = |ed: &mut Editor| -> (String, Vec<String>) {
            let p = ed.picker.as_mut().expect("a list");
            let rows = p.visible(20).into_iter().filter(|r| r.label.contains(':')).map(|r| r.label).collect();
            (p.title.clone(), rows)
        };
        let (title, rows) = shown(&mut ed);
        assert_eq!(title, "text matches");
        assert!(
            ed.status.as_ref().is_some_and(|s| s.0 == "text matches of `a` until fake is ready"),
            "{:?}",
            ed.status
        );
        assert!(
            rows.contains(&"1: fn a() {}".to_string()) && rows.contains(&"2: fn b() { a() }".to_string()),
            "{rows:?}"
        );
        assert!(rows.contains(&"1: use a;".to_string()), "{rows:?}");
        // (the fake server's log in that folder has the file's text too — its line isn't what's checked)
        assert!(
            !rows.iter().any(|r| r.ends_with("// ab, A") || r.ends_with("let ab = 1;")),
            "whole words, case as written: {rows:?}"
        );
        // The server's answer takes their place
        let at = |line: u64, col: u64| {
            json!({ "uri": lsp::uri(&file), "range": {
            "start": { "line": line, "character": col }, "end": { "line": line, "character": col + 1 } } })
        };
        let id = pending_of(&ed, Kind::Goto(Goto::References));
        ed.on_lsp_message(cid, json!({ "id": id, "result": [at(0, 3), at(1, 9)] }));
        settle_for(&mut ed, 5, |ed| ed.picker.as_ref().is_some_and(|p| p.title == "references"));
        let (title, rows) = shown(&mut ed);
        assert_eq!(
            (title.as_str(), rows),
            ("references", vec!["1: fn a() {}".into(), "2: fn b() { a() }".into()])
        );
        assert!(
            ed.status.as_ref().is_some_and(|s| s.0.contains("replaced the text matches")),
            "{:?}",
            ed.status
        );
        // Closed before the answer came: no list pops up after
        feed(&mut ed, "<esc>gr");
        settle_for(&mut ed, 5, |ed| ed.picker.is_some());
        feed(&mut ed, "<esc>");
        let id = pending_of(&ed, Kind::Goto(Goto::References));
        ed.on_lsp_message(cid, json!({ "id": id, "result": [at(0, 3), at(1, 9)] }));
        settle_for(&mut ed, 1, |ed| ed.picker.is_some());
        assert!(ed.picker.is_none(), "the answer isn't pushed on you");
        // Not loading: no text matches — the server's list only
        ed.on_lsp_message(cid, progress("end"));
        feed(&mut ed, "gr");
        let id = pending_of(&ed, Kind::Goto(Goto::References));
        ed.on_lsp_message(cid, json!({ "id": id, "result": [at(0, 3), at(1, 9)] }));
        settle_for(&mut ed, 5, |ed| ed.picker.is_some());
        assert_eq!(ed.picker.as_ref().map(|p| p.title.as_str()), Some("references"));
        settle_for(&mut ed, 1, |ed| ed.picker.as_ref().is_some_and(|p| p.title != "references"));
        assert_eq!(ed.picker.as_ref().map(|p| p.title.as_str()), Some("references"), "and none come after");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// N3: a server still starting can't be asked — the text matches show, it's said, and the question goes
    /// once it's up; its answer takes their place. A busy server that answers within `TEXT_GRACE` never has
    /// text matches flash up before its list.
    #[test]
    fn references_while_the_server_starts_and_a_quick_answer() {
        let (mut ed, file, log) = setup("refs-starting", "fn a() {}\nfn b() { a() }\n");
        let cid = ed.doc().lsp.client.unwrap();
        ed.doc_mut().set_selection(Selection::point(3));
        feed(&mut ed, "gr");
        assert!(ed.status.as_ref().is_some_and(|s| s.0 == "fake is starting — references when it's ready"));
        settle_for(&mut ed, 5, |ed| ed.picker.is_some());
        assert_eq!(ed.picker.as_ref().map(|p| p.title.as_str()), Some("text matches"));
        assert!(!sent(&log).contains("textDocument/references"), "not asked before it's up");
        let init = json!({ "id": 0, "result": { "capabilities": { "positionEncoding": "utf-8" } } });
        ed.on_lsp_message(cid, init);
        settle_for(&mut ed, 5, |ed| ed.lsp.pending.values().any(|p| p.kind == Kind::Goto(Goto::References)));
        let at = |line: u64, col: u64| {
            json!({ "uri": lsp::uri(&file), "range": {
            "start": { "line": line, "character": col }, "end": { "line": line, "character": col + 1 } } })
        };
        let id = pending_of(&ed, Kind::Goto(Goto::References));
        ed.on_lsp_message(cid, json!({ "id": id, "result": [at(0, 3), at(1, 9)] }));
        settle_for(&mut ed, 5, |ed| ed.picker.as_ref().is_some_and(|p| p.title == "references"));
        assert_eq!(
            ed.picker.as_ref().map(|p| p.title.as_str()),
            Some("references"),
            "asked once up, replaced"
        );
        // Busy, but its answer comes within the grace: the list only — no text matches before or after
        feed(&mut ed, "<esc>");
        let progress = json!({ "method": "$/progress", "params": { "token": "t",
            "value": { "kind": "begin", "title": "cargo check" } } });
        ed.on_lsp_message(cid, progress);
        feed(&mut ed, "gr");
        // (the answer after the text search is done — a tiny folder — but within the grace)
        std::thread::sleep(Duration::from_millis(60));
        let id = pending_of(&ed, Kind::Goto(Goto::References));
        ed.on_lsp_message(cid, json!({ "id": id, "result": [at(0, 3), at(1, 9)] }));
        settle_for(&mut ed, 1, |ed| ed.picker.as_ref().is_some_and(|p| p.title != "references"));
        assert_eq!(ed.picker.as_ref().map(|p| p.title.as_str()), Some("references"));
        assert!(ed.lsp.refs.texts.is_none(), "no text matches came up");
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    /// Claude's `references` call `id`'s answer, once it's given (`tools::ANSWERED`).
    fn tool_answer(id: u64) -> Option<Result<String, String>> {
        crate::tools::ANSWERED
            .with(|a| a.borrow().iter().find(|(i, _)| *i == json!(id)).map(|(_, r)| r.clone()))
    }

    fn ask_tool(ed: &mut Editor, id: u64, args: Value) -> Result<String, String> {
        crate::nav::references(ed, crate::tools::Call::test(id), &args)?;
        settle_for(ed, 10, |_| tool_answer(id).is_some());
        tool_answer(id).expect("answered")
    }

    /// N5: Claude's `references` asks the file's server at the name on the line it gave (not the user's
    /// cursor, which may move meanwhile) and answers with the places' code, the declaration first — a file
    /// not open is opened aside, the user's file staying in view.
    #[test]
    fn the_references_tool_asks_the_server_at_the_name() {
        let (mut ed, file, log) = setup("tool-refs", "fn b() { a() }\nfn c() { a(); a() }\n");
        let dir = file.parent().unwrap().to_path_buf();
        let other = dir.join("other.rs");
        std::fs::write(&other, "// other\nuse crate::a;\n\npub fn a() {}\n").unwrap();
        let other = std::fs::canonicalize(&other).unwrap();
        ed.project_root = Some(dir.clone());
        let cid = ed.doc().lsp.client.unwrap();
        let caps =
            json!({ "positionEncoding": "utf-8", "definitionProvider": true, "referencesProvider": true });
        ed.on_lsp_message(cid, json!({ "id": 0, "result": { "capabilities": caps } }));
        crate::nav::references(
            &mut ed,
            crate::tools::Call::test(1),
            &json!({"path": "other.rs", "line": 4, "symbol": "crate::a"}),
        )
        .unwrap();
        settle_for(&mut ed, 5, |ed| ed.lsp.pending.values().any(|p| matches!(p.kind, Kind::ToolRefs { .. })));
        assert_eq!(ed.doc().path.as_deref(), Some(file.as_path()), "the user's file stays in view");
        assert!(ed.docs.iter().any(|d| d.path.as_deref() == Some(other.as_path())), "opened aside");
        let out = sent(&log);
        let asked = out.lines().find(|l| l.contains("textDocument/references")).expect("asked");
        assert!(
            asked.contains(r#""position":{"character":7,"line":3}"#) && asked.contains("other.rs"),
            "{asked}"
        );
        assert!(asked.contains(r#""includeDeclaration":true"#), "{asked}");
        // The user moves on meanwhile: still answered
        feed(&mut ed, "j");
        let at = |path: &PathBuf, line: u64, col: u64| {
            json!({ "uri": lsp::uri(path), "range": {
            "start": { "line": line, "character": col }, "end": { "line": line, "character": col + 1 } } })
        };
        let id = pending_of(&ed, Kind::ToolRefs { ask: 0 });
        let refs =
            json!([at(&file, 1, 9), at(&other, 3, 7), at(&file, 0, 9), at(&other, 1, 11), at(&file, 1, 14)]);
        ed.on_lsp_message(cid, json!({ "id": id, "result": refs }));
        assert!(tool_answer(1).is_none(), "waits for the declaration");
        let id = pending_of(&ed, Kind::ToolDeclared { ask: 0 });
        ed.on_lsp_message(cid, json!({ "id": id, "result": at(&other, 3, 7) }));
        settle_for(&mut ed, 5, |_| tool_answer(1).is_some());
        assert_eq!(
            tool_answer(1).unwrap().unwrap(),
            "5 references in 2 files (fake), the declaration first:\nother.rs\n  4: pub fn a() {}  (declaration)\n  \
             2: use crate::a;\nmain.rs\n  1: fn b() { a() }\n  2: fn c() { a(); a() }"
        );
        assert!(ed.lsp.tool_asks.is_empty());
        // Not on that line: said, with what the line reads
        let e = ask_tool(&mut ed, 2, json!({"path": "other.rs", "line": 1, "symbol": "a"})).unwrap_err();
        assert!(e.contains("isn't on line 1 of other.rs — it reads `// other`"), "{e}");
        // Outside the project: refused
        let e = ask_tool(&mut ed, 3, json!({"path": "/etc/hosts", "symbol": "localhost"})).unwrap_err();
        assert!(e.contains("outside the project"), "{e}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// N5: when the server can't give its references — starting, still loading with nothing yet, or silent
    /// for `REFS_WAIT` — the answer is the name's whole-word text matches, saying why and that they aren't
    /// its references.
    #[test]
    fn the_references_tool_gives_text_matches_while_the_server_cannot() {
        let (mut ed, file, _log) = setup("tool-refs-loading", "fn a() {}\nfn b() { a() }\n// ab, A\n");
        let dir = file.parent().unwrap().to_path_buf();
        ed.project_root = Some(dir.clone());
        let args = json!({"path": "main.rs", "symbol": "a"});
        let t = ask_tool(&mut ed, 1, args.clone()).unwrap();
        assert!(t.contains("— fake is starting, so these are the whole-word text matches of `a`"), "{t}");
        assert!(t.contains("not its references — ask again in a moment for those:"), "{t}");
        assert!(t.contains("main.rs\n  1: fn a() {}\n  2: fn b() { a() }"), "{t}");
        assert!(!t.contains("// ab, A"), "whole words, case as written: {t}");
        // Loading, and its answer empty
        let cid = ready(&mut ed);
        let progress = json!({ "method": "$/progress", "params": { "token": "i",
            "value": { "kind": "begin", "title": "Indexing" } } });
        ed.on_lsp_message(cid, progress);
        crate::nav::references(&mut ed, crate::tools::Call::test(2), &args).unwrap();
        settle_for(&mut ed, 5, |ed| ed.lsp.pending.values().any(|p| matches!(p.kind, Kind::ToolRefs { .. })));
        let id = pending_of(&ed, Kind::ToolRefs { ask: 0 });
        ed.on_lsp_message(cid, json!({ "id": id, "result": [] }));
        settle_for(&mut ed, 5, |_| tool_answer(2).is_some());
        let t = tool_answer(2).unwrap().unwrap();
        assert!(t.contains("fake is still loading (Indexing) and found none yet, so these are"), "{t}");
        // Silent: the text matches after the wait
        let t = ask_tool(&mut ed, 3, args).unwrap();
        assert!(t.contains("fake didn't answer within 300ms, so these are"), "{t}");
        assert!(ed.lsp.tool_asks.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// N5b: Claude's `symbols` — while the file's server is starting, tarae's own outline (said so); once
    /// it's up, the server's document symbols; with a line, what holds it.
    #[test]
    fn the_symbols_tool_asks_the_server_or_gives_the_outline() {
        let (mut ed, file, log) =
            setup("tool-syms", "struct S;\nimpl S {\n    fn f(&self) {\n        1;\n    }\n}\n");
        let dir = file.parent().unwrap().to_path_buf();
        ed.project_root = Some(dir.clone());
        let syms = |ed: &mut Editor, id: u64, args: Value| {
            crate::nav::symbols(ed, crate::tools::Call::test(id), &args).unwrap();
            settle_for(ed, 10, |_| tool_answer(id).is_some());
            tool_answer(id).expect("answered")
        };
        let t = syms(&mut ed, 1, json!({"path": "main.rs", "line": 4})).unwrap();
        let rust =
            crate::syntax::spec("rust").is_some_and(|s| crate::syntax::Loader::global().load(s).is_ok());
        if rust {
            assert_eq!(
                t,
                "line 4 of main.rs is in (tarae's outline — fake is starting):\nimpl S  2–6\n  fn f  3–5"
            );
        }
        let cid = ed.doc().lsp.client.unwrap();
        let caps = json!({ "positionEncoding": "utf-8", "documentSymbolProvider": true });
        ed.on_lsp_message(cid, json!({ "id": 0, "result": { "capabilities": caps } }));
        crate::nav::symbols(&mut ed, crate::tools::Call::test(2), &json!({"path": "main.rs"})).unwrap();
        settle_for(&mut ed, 5, |ed| {
            ed.lsp.pending.values().any(|p| matches!(p.kind, Kind::ToolSymbols { .. }))
        });
        assert!(sent(&log).contains("textDocument/documentSymbol"));
        let range = |a: u64, b: u64| json!({ "start": { "line": a, "character": 0 }, "end": { "line": b, "character": 1 } });
        let answer = json!([{ "name": "S", "kind": 23, "range": range(0, 0), "selectionRange": range(0, 0) },
            { "name": "impl S", "kind": 19, "range": range(1, 5), "selectionRange": range(1, 1),
              "children": [{ "name": "f", "kind": 6, "range": range(2, 4), "selectionRange": range(2, 2) }] }]);
        let id = pending_of(&ed, Kind::ToolSymbols { ask: 0 });
        ed.on_lsp_message(cid, json!({ "id": id, "result": answer }));
        settle_for(&mut ed, 5, |_| tool_answer(2).is_some());
        assert_eq!(
            tool_answer(2).unwrap().unwrap(),
            "3 symbols in main.rs (fake):\nstruct S  1\nimpl S  2–6\n  method f  3–5"
        );
        let e = syms(&mut ed, 3, json!({"path": "main.rs", "line": 99})).unwrap_err();
        assert!(e.contains("main.rs has 6 lines — no line 99"), "{e}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A go-to is timed from its key to the frame that shows its answer (N1): one line in the log once
    /// that frame is drawn, each step in order — what the server took, the wait for its work, the re-ask,
    /// our list and our frame.
    #[test]
    fn a_goto_is_timed_from_its_key_to_its_frame() {
        let (mut ed, file, _log) = setup("traced", "fn a() {}\nfn b() { a() }\n");
        let cid = ready(&mut ed);
        let progress = |kind: &str| {
            json!({ "method": "$/progress", "params": { "token": "rustAnalyzer/Indexing",
                "value": { "kind": kind, "title": "Indexing" } } })
        };
        ed.on_lsp_message(cid, progress("begin"));
        feed(&mut ed, "jwwgr");
        // (the server is busy: the text matches come first — N3)
        settle_for(&mut ed, 5, |ed| ed.picker.is_some());
        let id = pending_of(&ed, Kind::Goto(Goto::References));
        ed.on_lsp_message(cid, json!({ "id": id, "result": [] }));
        ed.on_lsp_message(cid, progress("end"));
        let id = pending_of(&ed, Kind::Goto(Goto::References));
        let at = |line: u64| json!({ "uri": lsp::uri(&file), "range": { "start": { "line": line, "character": 3 }, "end": { "line": line, "character": 4 } } });
        ed.on_lsp_message(cid, json!({ "id": id, "result": [at(0), at(1)] }));
        settle_for(&mut ed, 5, |ed| ed.picker.as_ref().is_some_and(|p| p.title == "references"));
        assert!(ed.picker.is_some(), "listed");
        assert!(ed.lsp.traced.is_empty(), "not before it's on screen");
        crate::term::render(&mut ed, &mut Vec::new(), 100, 30).unwrap();
        assert_eq!(ed.lsp.traced.len(), 1, "one line, once drawn");
        let line = &ed.lsp.traced[0];
        let steps: Vec<&str> =
            line.split(" · ").skip(1).map(|s| s.rsplit_once(' ').map_or(s, |(what, _took)| what)).collect();
        assert_eq!(
            steps,
            [
                "sent",
                "text matches shown",
                "answered (0)",
                "main loop",
                "answered empty while busy (Indexing)",
                "waited for the server",
                "asked again",
                "answered (2)",
                "main loop",
                "lines read",
                "list (text matches replaced)",
                "drawn"
            ],
            "{line}"
        );
        assert!(line.starts_with("references "), "{line}");
        // The next frame writes nothing more
        crate::term::render(&mut ed, &mut Vec::new(), 100, 30).unwrap();
        assert_eq!(ed.lsp.traced.len(), 1);
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    /// `g d` while the server is still indexing gets an empty answer: that's said as "loading", and the
    /// question goes again by itself when the work ends — unless the cursor has moved on.
    #[test]
    fn goto_while_indexing_asks_again_when_done() {
        let (mut ed, file, _log) = setup("indexing", "fn a() {}\nfn b() { a() }\n");
        let cid = ready(&mut ed);
        let progress = |kind: &str| {
            json!({ "method": "$/progress", "params": { "token": "rustAnalyzer/Indexing",
                "value": { "kind": kind, "title": "Indexing", "message": "4/21" } } })
        };
        ed.on_lsp_message(cid, progress("begin"));
        // (a long `cargo check` beside the indexing — the question needn't wait for it)
        let check = json!({ "method": "$/progress", "params": { "token": "rustAnalyzer/Flycheck",
            "value": { "kind": "begin", "title": "cargo check" } } });
        ed.on_lsp_message(cid, check);
        feed(&mut ed, "jwwgd");
        let asked =
            |ed: &Editor| ed.lsp.pending.values().filter(|p| p.kind == Kind::Goto(Goto::Definition)).count();
        let id = pending_of(&ed, Kind::Goto(Goto::Definition));
        ed.on_lsp_message(cid, json!({ "id": id, "result": [] }));
        let status = ed.status.as_ref().map(|(m, _)| m.clone()).unwrap_or_default();
        assert_eq!(status, "fake is loading — going there when ready");
        assert_eq!(asked(&ed), 0);
        // Done indexing (cargo check still running) → asked again, and the answer jumps
        ed.on_lsp_message(cid, progress("end"));
        assert_eq!(asked(&ed), 1, "asked again");
        let id = pending_of(&ed, Kind::Goto(Goto::Definition));
        let at = json!({ "uri": lsp::uri(&file), "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 4 } } });
        ed.on_lsp_message(cid, json!({ "id": id, "result": [at] }));
        assert_eq!(ed.doc().selection().primary(), Range::point(3), "on `a`'s definition");
        let check_end = json!({ "method": "$/progress", "params": { "token": "rustAnalyzer/Flycheck",
            "value": { "kind": "end" } } });
        ed.on_lsp_message(cid, check_end);
        // Moved on meanwhile → not asked again; an idle server's empty answer is just "nothing"
        feed(&mut ed, "jwwgd");
        ed.on_lsp_message(cid, progress("begin"));
        let id = pending_of(&ed, Kind::Goto(Goto::Definition));
        ed.on_lsp_message(cid, json!({ "id": id, "result": [] }));
        feed(&mut ed, "gg");
        ed.on_lsp_message(cid, progress("end"));
        assert_eq!(asked(&ed), 0, "the cursor moved — not asked again");
        feed(&mut ed, "jwwgd");
        let id = pending_of(&ed, Kind::Goto(Goto::Definition));
        ed.on_lsp_message(cid, json!({ "id": id, "result": [] }));
        assert!(ed.status.as_ref().is_some_and(|(m, _)| m == "no definitions found"), "{:?}", ed.status);
        // "Content modified" (the project changed under the question): no error toast — asked again shortly
        feed(&mut ed, "gd");
        let id = pending_of(&ed, Kind::Goto(Goto::Definition));
        let modified = json!({ "code": -32801, "message": "content modified" });
        ed.on_lsp_message(cid, json!({ "id": id, "error": modified }));
        assert!(!ed.toasts.iter().any(|t| t.text.contains("content modified")), "not shown");
        assert_eq!(asked(&ed), 0);
        let settle = |ed: &mut Editor| {
            let end = std::time::Instant::now() + Duration::from_secs(5);
            while asked(ed) == 0 && std::time::Instant::now() < end {
                if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(50)) {
                    ed.handle_event(ev);
                }
            }
        };
        settle(&mut ed);
        assert_eq!(asked(&ed), 1, "asked again");
        // …but not forever at one place
        for _ in 0..MAX_ASKS {
            let id = pending_of(&ed, Kind::Goto(Goto::Definition));
            ed.on_lsp_message(cid, json!({ "id": id, "error": modified }));
            if asked(&ed) == 0 && ed.status.as_ref().is_some_and(|(m, _)| m.contains("try again")) {
                break;
            }
            settle(&mut ed);
        }
        assert!(ed.status.as_ref().is_some_and(|(m, _)| m.contains("try again")), "{:?}", ed.status);
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    /// `:w` formats first, then saves; a server error or an edit made meanwhile still saves (as is); a
    /// server that can't format saves right away.
    #[test]
    fn format_on_save() {
        let (mut ed, file, log) = setup("fmtsave", "fn main() {\n    let x = 1;\n}\n");
        let cid = ed.doc().lsp.client.unwrap();
        let caps = json!({ "positionEncoding": "utf-8", "documentFormattingProvider": true });
        ed.on_lsp_message(cid, json!({ "id": 0, "result": { "capabilities": caps } }));
        // (saves are written in the background: what's on disk once they're done)
        let disk = |ed: &mut Editor| {
            ed.flush_saves();
            std::fs::read_to_string(&file).unwrap()
        };
        let two = json!([{ "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 4 } }, "newText": "  " }]);
        feed(&mut ed, "ggA//<esc>:w<ret>");
        assert!(sent(&log).contains(r#""method":"textDocument/formatting""#));
        assert_eq!(disk(&mut ed), "fn main() {\n    let x = 1;\n}\n", "not saved before the answer");
        ed.on_lsp_message(
            cid,
            json!({ "id": pending_of(&ed, Kind::FormatSave { quit: false }), "result": two }),
        );
        assert_eq!(disk(&mut ed), "fn main() {//\n  let x = 1;\n}\n", "formatted, then saved");
        assert!(!ed.doc().is_modified());
        // Server error → saved as is
        feed(&mut ed, "A!<esc>:w<ret>");
        let id = pending_of(&ed, Kind::FormatSave { quit: false });
        ed.on_lsp_message(cid, json!({ "id": id, "error": { "message": "no" } }));
        assert_eq!(disk(&mut ed), "fn main() {//!\n  let x = 1;\n}\n");
        // Typed on while formatting → the reply is stale, what's there is saved
        feed(&mut ed, "A?<esc>:w<ret>A#<esc>");
        let id = pending_of(&ed, Kind::FormatSave { quit: false });
        ed.on_lsp_message(cid, json!({ "id": id, "result": two }));
        assert_eq!(disk(&mut ed), "fn main() {//!?#\n  let x = 1;\n}\n");
        // No formatter → saved right away
        let caps = json!({ "positionEncoding": "utf-8" });
        ed.lsp.clients[0].caps = caps;
        feed(&mut ed, "A%<esc>:w<ret>");
        assert_eq!(disk(&mut ed), "fn main() {//!?#%\n  let x = 1;\n}\n");
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    /// `:wa` formats every modified file like `:w` — also one that isn't the current buffer — and each
    /// answer lands in its own file (measured live: `:wa` used to save unformatted).
    #[test]
    fn write_all_formats_every_file() {
        let (mut ed, file, _log) = setup("fmtall", "fn main() {\n    let x = 1;\n}\n");
        let other = file.with_file_name("other.rs");
        std::fs::write(&other, "fn o() {\n    let y = 2;\n}\n").unwrap();
        let cid = ed.doc().lsp.client.unwrap();
        let caps = json!({ "positionEncoding": "utf-8", "documentFormattingProvider": true });
        ed.on_lsp_message(cid, json!({ "id": 0, "result": { "capabilities": caps } }));
        feed(&mut ed, "ggA//<esc>");
        ed.open(&other).unwrap();
        feed(&mut ed, "ggA!!<esc>:wa<ret>");
        let asks: Vec<(u64, DocId)> = ed
            .lsp
            .pending
            .iter()
            .filter(|(_, p)| matches!(p.kind, Kind::FormatSave { .. }))
            .map(|((_, id), p)| (*id, p.doc))
            .collect();
        assert_eq!(asks.len(), 2, "both files asked");
        let indent = json!([{ "range": { "start": { "line": 1, "character": 0 }, "end": { "line": 1, "character": 4 } }, "newText": "  " }]);
        for (id, _) in &asks {
            ed.on_lsp_message(cid, json!({ "id": id, "result": indent }));
        }
        ed.flush_saves();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "fn main() {//\n  let x = 1;\n}\n");
        assert_eq!(std::fs::read_to_string(&other).unwrap(), "fn o() {!!\n  let y = 2;\n}\n");
        assert!(ed.docs.iter().all(|d| !d.is_modified()));
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    /// `space h` selects the symbol's uses (primary on the cursor's); `space D` lists diagnostics from open
    /// and unopened files, errors first; `:lsp-stop` detaches, `:lsp-restart` starts a new server.
    #[test]
    fn symbol_uses_workspace_diagnostics_and_restart() {
        let (mut ed, file, _log) = setup("extras", "fn f(x: u8) {\n    g(x);\n}\n");
        let cid = ed.doc().lsp.client.unwrap();
        let caps = json!({ "positionEncoding": "utf-8", "documentHighlightProvider": true });
        ed.on_lsp_message(cid, json!({ "id": 0, "result": { "capabilities": caps } }));
        let at = |l: u64, a: u64, b: u64| json!({ "range": { "start": { "line": l, "character": a }, "end": { "line": l, "character": b } } });
        let head = 14 + 6; // the second `x`
        ed.docs[0].set_selection(Selection::point(head));
        feed(&mut ed, " h");
        ed.on_lsp_message(
            cid,
            json!({ "id": pending_of(&ed, Kind::Highlights), "result": [at(0, 5, 6), at(1, 6, 7)] }),
        );
        let sel = ed.doc().selection();
        assert_eq!(sel.len(), 2);
        assert!(
            sel.primary().from() <= head && head < sel.primary().to(),
            "primary = the use under the cursor"
        );
        // Diagnostics: one in the open file, two in a file that isn't open
        let other = file.parent().unwrap().join("other.rs");
        let diags = |uri: String, list: Value| json!({ "method": "textDocument/publishDiagnostics", "params": { "uri": uri, "diagnostics": list } });
        ed.on_lsp_message(
            cid,
            diags(
                lsp::uri(&file),
                json!([{ "range": at(0, 0, 1)["range"], "severity": 2, "message": "warn here" }]),
            ),
        );
        ed.on_lsp_message(
            cid,
            diags(
                lsp::uri(&other),
                json!([
                    { "range": at(3, 0, 1)["range"], "severity": 1, "message": "broken" },
                    { "range": at(4, 0, 1)["range"], "severity": 4, "message": "hint" }
                ]),
            ),
        );
        feed(&mut ed, "<esc> D");
        let p = ed.picker.as_ref().expect("workspace diagnostics");
        let labels: Vec<&str> = p.items().iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels.len(), 3);
        assert!(labels[0].ends_with("other.rs:4  broken"), "errors first: {labels:?}");
        assert!(labels[1].ends_with("main.rs:1  warn here"), "{labels:?}");
        feed(&mut ed, "<esc>:lsp-stop<ret>");
        assert!(ed.doc().lsp.client.is_none() && ed.lsp.clients.is_empty());
        ed.attach_lsp(ed.doc().id);
        let first = ed.doc().lsp.client.unwrap();
        feed(&mut ed, ":lsp-restart<ret>");
        let again = ed.doc().lsp.client.expect("attached again");
        assert!(again != first && ed.lsp.clients.len() == 1, "a new server");
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    /// A `.rs` with no cargo project above it gets no server — rust-analyzer would only say "Failed to discover
    /// workspace" — and nothing on screen says so (the log does); one under a `Cargo.toml`, or a
    /// `rust-project.json`, gets its server.
    #[test]
    fn a_lone_rust_file_gets_no_server() {
        let dir = std::env::temp_dir().join(format!("tarae-lsp-lone-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        for d in ["lone", "cargo/src", "json"] {
            std::fs::create_dir_all(dir.join(d)).unwrap();
        }
        std::fs::write(dir.join("cargo/Cargo.toml"), "[package]\nname = \"t\"\n").unwrap();
        std::fs::write(dir.join("json/rust-project.json"), "{}\n").unwrap();
        let cfg = format!(
            "[lsp.fake]\ncommand = \"sh\"\nargs = [\"-c\", \"cat > '{}'\"]\n[lang.rust]\nlsp = [\"fake\"]\n",
            dir.join("sent.log").display()
        );
        let (config, w) = crate::config::parse(&cfg);
        assert!(w.is_empty(), "{w:?}");
        let mut ed = Editor::new(config);
        for f in ["lone/calc.rs", "cargo/src/main.rs", "json/lib.rs"] {
            std::fs::write(dir.join(f), "fn main() {}\n").unwrap();
        }
        ed.open(&dir.join("lone/calc.rs")).unwrap();
        assert!(ed.doc().lsp.client.is_none() && ed.lsp.clients.is_empty(), "no server for a lone file");
        assert!(ed.status.is_none(), "nothing on screen: {:?}", ed.status);
        ed.open(&dir.join("cargo/src/main.rs")).unwrap();
        assert!(ed.doc().lsp.client.is_some(), "a cargo project: its server");
        ed.open(&dir.join("json/lib.rs")).unwrap();
        assert!(ed.doc().lsp.client.is_some(), "rust-project.json: its server");
        assert_eq!(ed.lsp.clients.len(), 2, "one per project");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A language whose server isn't installed: a faint dot, no toast (the log says what was tried).
    #[test]
    fn server_not_installed() {
        let dir = std::env::temp_dir().join(format!("tarae-lsp-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"t\"\n").unwrap();
        let (config, w) = crate::config::parse(
            "[lsp.broken]\ncommand = \"tarae-no-such-server\"\n[lang.rust]\nlsp = [\"broken\"]\n",
        );
        assert!(w.is_empty(), "{w:?}");
        let mut ed = Editor::new(config);
        ed.open(&file).unwrap();
        assert_eq!(ed.lsp_state(), Some((LspDot::Missing, "rust".into())));
        assert!(ed.status.is_none(), "no toast for it");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The status line's dot: starting until initialize answers, ready after, failed once it exits.
    #[test]
    fn lsp_state_dot() {
        let (mut ed, file, _log) = setup("dot", "fn main() {}\n");
        assert_eq!(ed.lsp_state().map(|s| s.0), Some(LspDot::Starting));
        let cid = ready(&mut ed);
        assert_eq!(ed.lsp_state(), Some((LspDot::Ready, "fake".into())));
        ed.lsp_exited(cid);
        assert_eq!(ed.lsp_state().map(|s| s.0), Some(LspDot::Failed));
        assert!(ed.status.as_ref().is_some_and(|(m, _)| m.contains(":log-open")));
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }

    #[test]
    fn definition_hover_and_stale_responses() {
        let (mut ed, file, _log) = setup("def", "fn helper() {}\nfn main() { helper(); }\n");
        let cid = ready(&mut ed);
        let pending_id = |ed: &Editor| *ed.lsp.pending.keys().next().map(|(_, id)| id).unwrap();
        // gd → response → jump
        feed(&mut ed, "j3wgd");
        let id = pending_id(&ed);
        let loc = json!({ "uri": lsp::uri(&file), "range": { "start": { "line": 0, "character": 3 }, "end": { "line": 0, "character": 9 } } });
        ed.on_lsp_message(cid, json!({ "id": id, "result": loc }));
        assert_eq!(ed.doc().selection().primary(), Range::point(3));
        // hover → popup text, any key closes it
        feed(&mut ed, " k");
        let id = pending_id(&ed);
        let hover =
            json!({ "contents": { "kind": "markdown", "value": "```rust\nfn helper()\n```\nhelps" } });
        ed.on_lsp_message(cid, json!({ "id": id, "result": hover }));
        let shown = crate::markdown::wrap(ed.popup.as_deref().unwrap(), 40, Default::default());
        let plain: Vec<String> = shown.iter().map(|l| l.iter().map(|s| s.text.as_str()).collect()).collect();
        assert_eq!(plain, ["fn helper()", "", "helps"]);
        feed(&mut ed, "<C-d>");
        assert!(ed.popup.is_some() && ed.popup_scroll > 0, "C-d scrolls (doesn't close)");
        feed(&mut ed, "<esc>");
        assert!(ed.popup.is_none());
        assert_eq!(ed.popup_scroll, 0);
        // Late response: drop if the cursor moved after the request
        feed(&mut ed, " kl");
        let id = pending_id(&ed);
        ed.on_lsp_message(cid, json!({ "id": id, "result": hover }));
        assert!(ed.popup.is_none(), "hover arriving after the cursor moved is dropped");
        std::fs::remove_dir_all(file.parent().unwrap()).ok();
    }
}
