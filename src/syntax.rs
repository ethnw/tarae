//! Syntax trees·highlighting — tree-sitter.
//!
//! - Grammars: built into the binary (`build.rs` — the default build has the 4 `core` ones, debug builds add
//!   the `dev` one, `bundled-grammars` the `bundle` ones), the rest are downloaded shared libraries dlopen'd
//!   **on demand**.
//! - Queries in Helix format as is (`; inherits: a,b` supported). When several patterns match the same
//!   node **the later pattern wins** (a premise of Helix 25.07 queries — the generic
//!   `(identifier) @variable` is at the top of the file).
//! - Parsing runs in the background (principle 4). Edits are applied to the existing tree at once via
//!   `edit`, so highlights follow the text even while parsing. Edits arriving mid-parse are collected
//!   and reapplied to the new tree.
//! - Highlighting runs queries only over the visible line range.
//! - `locals.scm` (Helix format): a name used on screen that refers to a local definition — found through
//!   the enclosing scopes, read from the widest enclosing node within 64 KB above — takes that definition's
//!   highlight (`@local.definition.variable.parameter` → a parameter's uses look like it). Patterns marked
//!   `(#is-not? local)` skip such names.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use ropey::Rope;
use tree_sitter::{InputEdit, Language, Node, Parser, Point, Query, QueryCursor, StreamingIterator, Tree};

use crate::runtime;

// ── Language table ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct LangSpec {
    pub name: String,
    pub grammar: String,
    extensions: Vec<String>,
    filenames: Vec<String>,
    path_suffixes: Vec<String>,
    /// Path-suffix patterns — up to one `*` per segment (`templates/*.yaml` = Helm chart templates).
    globs: Vec<String>,
    /// Line comment token (`//`) · block comment (`/*`, `*/`) — `C-c` toggles them.
    pub comment_token: Option<String>,
    pub block_comment: Option<(String, String)>,
    /// Insert-mode pairs (open, close) — None = the defaults.
    pub auto_pairs: Option<Vec<(char, char)>>,
}

/// Insert-mode pairs when the language doesn't say.
pub const DEFAULT_PAIRS: &[(char, char)] =
    &[('(', ')'), ('[', ']'), ('{', '}'), ('"', '"'), ('\'', '\''), ('`', '`')];

fn specs() -> &'static [LangSpec] {
    static SPECS: OnceLock<Vec<LangSpec>> = OnceLock::new();
    SPECS.get_or_init(|| {
        let table: toml::Table =
            toml::from_str(include_str!("languages.toml")).expect("languages.toml parses");
        let strings = |v: Option<&toml::Value>| -> Vec<String> {
            v.and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect())
                .unwrap_or_default()
        };
        table["language"]
            .as_array()
            .expect("[[language]]")
            .iter()
            .filter_map(|l| {
                let name = l.get("name")?.as_str()?.to_string();
                Some(LangSpec {
                    grammar: l.get("grammar").and_then(|g| g.as_str()).unwrap_or(&name).to_string(),
                    extensions: strings(l.get("extensions")),
                    filenames: strings(l.get("filenames")),
                    path_suffixes: strings(l.get("path-suffixes")),
                    globs: strings(l.get("globs")),
                    comment_token: l.get("comment-token").and_then(|t| t.as_str()).map(str::to_string),
                    block_comment: l.get("block-comment-tokens").and_then(|b| {
                        Some((b.get("start")?.as_str()?.to_string(), b.get("end")?.as_str()?.to_string()))
                    }),
                    auto_pairs: l.get("auto-pairs").map(|_| {
                        strings(l.get("auto-pairs"))
                            .iter()
                            .filter_map(|p| {
                                let mut c = p.chars();
                                Some((c.next()?, c.next()?))
                            })
                            .collect()
                    }),
                    name,
                })
            })
            .collect()
    })
}

/// Up to one `*` per segment (`_*.tpl`, `*.yaml`).
fn wildcard(pat: &str, s: &str) -> bool {
    match pat.split_once('*') {
        Some((a, b)) => s.len() >= a.len() + b.len() && s.starts_with(a) && s.ends_with(b),
        None => pat == s,
    }
}

/// Whether the trailing segments of a path match the pattern (`templates/*.yaml`).
fn glob_match(pat: &str, path: &Path) -> bool {
    let parts: Vec<String> =
        path.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    let pats: Vec<&str> = pat.split('/').collect();
    parts.len() >= pats.len() && pats.iter().rev().zip(parts.iter().rev()).all(|(p, s)| wildcard(p, s))
}

/// Picks a language by path: path suffix → path pattern → exact file name → extension.
pub fn detect(path: &Path) -> Option<&'static LangSpec> {
    let full = path.to_string_lossy();
    let file = path.file_name()?.to_string_lossy();
    let ext = path.extension().map(|e| e.to_string_lossy());
    specs()
        .iter()
        .find(|s| s.path_suffixes.iter().any(|p| full.ends_with(p.as_str())))
        .or_else(|| specs().iter().find(|s| s.globs.iter().any(|g| glob_match(g, path))))
        .or_else(|| specs().iter().find(|s| s.filenames.iter().any(|f| *f == file)))
        .or_else(|| {
            let ext = ext?;
            specs().iter().find(|s| s.extensions.iter().any(|e| *e == ext))
        })
}

/// Known language names (`:lang` completion).
pub fn names() -> impl Iterator<Item = &'static str> {
    specs().iter().map(|s| s.name.as_str())
}

pub fn spec(name: &str) -> Option<&'static LangSpec> {
    specs().iter().find(|s| s.name == name)
}

/// Picks a language by a human-written name (code block header `rust`·`rs`·`sh`·`py` …, shebang `python3`).
pub fn resolve(name: &str) -> Option<&'static LangSpec> {
    let n = name.trim().trim_start_matches('{').trim_start_matches('.').to_ascii_lowercase();
    let n = match n.as_str() {
        "sh" | "shell" | "zsh" | "console" | "shellsession" => "bash",
        "golang" => "go",
        "c++" | "cxx" => "cpp",
        "py" | "python3" | "py3" => "python",
        "node" | "js" | "mjs" | "cjs" => "javascript",
        "yml" => "yaml",
        "jsonc" | "json5" => "json",
        "cs" | "csharp" => "c-sharp",
        "kt" => "kotlin",
        "hs" => "haskell",
        "rb" => "ruby",
        "text" | "txt" | "plain" | "plaintext" | "" => return None,
        x => x,
    };
    spec(n).or_else(|| detect(Path::new(&format!("x.{n}"))))
}

// ── Global capture names ────────────────────────────────────────────────

#[derive(Default)]
struct Captures {
    names: Vec<String>,
    ids: HashMap<String, u32>,
}

fn captures() -> &'static Mutex<Captures> {
    static C: OnceLock<Mutex<Captures>> = OnceLock::new();
    C.get_or_init(Mutex::default)
}

fn capture_id(name: &str) -> u32 {
    let mut c = captures().lock().unwrap();
    if let Some(&id) = c.ids.get(name) {
        return id;
    }
    let id = c.names.len() as u32;
    c.names.push(name.to_string());
    c.ids.insert(name.to_string(), id);
    id
}

/// Table of global capture number → something (usually a theme style). Index with `.get()` using the numbers
/// from `highlights` (another thread may load a new language after the table is built, growing the numbers).
pub fn capture_styles<T>(mut f: impl FnMut(&str) -> T) -> Vec<T> {
    captures().lock().unwrap().names.iter().map(|n| f(n)).collect()
}

/// How many capture names there are so far (they're only ever added).
pub fn capture_count() -> usize {
    captures().lock().unwrap().names.len()
}

#[cfg(test)]
pub fn capture_name(id: u32) -> String {
    captures().lock().unwrap().names[id as usize].clone()
}

// ── Grammar·query loading ───────────────────────────────────────────────

pub struct LangData {
    pub name: String,
    pub language: Language,
    pub highlights: Query,
    /// For `mi`/`ma` syntax objects (None if absent or failed to compile — text-based objects still work).
    pub textobjects: Option<Query>,
    /// Where other languages get injected (markdown code blocks·TODO in comments·macros …) — None if absent.
    pub injections: Option<Query>,
    /// Whether `injections` combines pieces from all over (`combines`) — worked out once, when first asked.
    combines: OnceLock<bool>,
    /// Scopes·definitions·references (`locals.scm`) — None if absent.
    locals: Option<Locals>,
    /// How new lines indent (`indents.scm`) — None if absent (a heuristic then).
    pub indents: Option<crate::indent::Indents>,
    /// Highlight patterns marked `(#is-not? local)` — not for names that resolve to a local.
    not_local: Vec<bool>,
    /// This language's capture number → global capture number (same name = same number across languages —
    /// painted with one style table).
    global: Vec<u32>,
}

/// Cache of loaded languages + open shared libraries (Language points into them, so they're kept alive).
#[derive(Default)]
pub struct Loader {
    langs: Mutex<HashMap<String, Arc<LangData>>>,
    libs: Mutex<Vec<libloading::Library>>,
    /// Languages that failed to load (language → error) — no grammar, ABI mismatch, broken query — so
    /// often-looked-up ones like `comment` (injected into every comment) aren't retried (disk·dlopen) on every
    /// parse. Cleared when a grammar is downloaded (`forget_missing`).
    failed: Mutex<HashMap<String, String>>,
}

impl Loader {
    pub fn global() -> &'static Loader {
        static LOADER: OnceLock<Loader> = OnceLock::new();
        LOADER.get_or_init(Loader::default)
    }

    /// After downloading a new grammar — clears the memory of failed loads.
    pub fn forget_missing(&self) {
        self.failed.lock().unwrap().clear();
    }

    /// Already-loaded language only (never touches disk) — safe on the main thread.
    pub fn loaded(&self, name: &str) -> Option<Arc<LangData>> {
        self.langs.lock().unwrap().get(name).cloned()
    }

    /// Slow (dlopen + query compilation, a few to tens of ms) — call from a worker thread.
    pub fn load(&self, spec: &LangSpec) -> Result<Arc<LangData>, String> {
        if let Some(l) = self.langs.lock().unwrap().get(&spec.name) {
            return Ok(l.clone());
        }
        if let Some(e) = self.failed.lock().unwrap().get(&spec.name) {
            return Err(e.clone());
        }
        let data = self.load_uncached(spec).inspect_err(|e| {
            self.failed.lock().unwrap().insert(spec.name.clone(), e.clone());
        })?;
        self.langs.lock().unwrap().insert(spec.name.clone(), data.clone());
        Ok(data)
    }

    fn load_uncached(&self, spec: &LangSpec) -> Result<Arc<LangData>, String> {
        let language = self.load_grammar(&spec.grammar)?;
        let source = read_query(&spec.name, "highlights.scm", 0)
            .ok_or_else(|| format!("{}: no highlights.scm in runtime", spec.name))?;
        let highlights = Query::new(&language, &source).map_err(|e| format!("{}: query: {e}", spec.name))?;
        let textobjects =
            read_query(&spec.name, "textobjects.scm", 0).and_then(|src| Query::new(&language, &src).ok());
        let injections =
            read_query(&spec.name, "injections.scm", 0).and_then(|src| Query::new(&language, &src).ok());
        let locals = read_query(&spec.name, "locals.scm", 0)
            .and_then(|src| Query::new(&language, &src).ok())
            .map(Locals::new);
        let indents = read_query(&spec.name, "indents.scm", 0)
            .and_then(|src| Query::new(&language, &src).ok())
            .map(crate::indent::Indents::new);
        let global = highlights.capture_names().iter().map(|n| capture_id(n)).collect();
        let not_local = (0..highlights.pattern_count())
            .map(|p| {
                highlights
                    .property_predicates(p)
                    .iter()
                    .any(|(prop, positive)| !positive && &*prop.key == "local")
            })
            .collect();
        Ok(Arc::new(LangData {
            name: spec.name.clone(),
            language,
            highlights,
            textobjects,
            injections,
            combines: OnceLock::new(),
            locals,
            indents,
            not_local,
            global,
        }))
    }

    fn load_grammar(&self, grammar: &str) -> Result<Language, String> {
        // Grammars in the binary first, otherwise a downloaded .so
        if let Some(func) = runtime::builtin_grammar(grammar) {
            // SAFETY: language function of a tree-sitter grammar compiled in by build.rs — takes no args,
            // returns a static language pointer.
            return Ok(Language::new(unsafe { tree_sitter_language::LanguageFn::from_raw(func) }));
        }
        let file = format!("grammars/{grammar}.so");
        let path =
            runtime::find(&file).ok_or_else(|| format!("no grammar '{grammar}' — :grammar-install"))?;
        let symbol = format!("tree_sitter_{}", grammar.replace('-', "_"));
        // SAFETY: language function of a grammar library built by the tree-sitter CLI — takes no args,
        // returns a static language pointer.
        let (lib, language) = unsafe {
            let lib = libloading::Library::new(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let func: libloading::Symbol<unsafe extern "C" fn() -> *const ()> =
                lib.get(symbol.as_bytes()).map_err(|e| format!("{grammar}: {e}"))?;
            let language = Language::new(tree_sitter_language::LanguageFn::from_raw(*func));
            (lib, language)
        };
        let v = language.abi_version();
        if !(tree_sitter::MIN_COMPATIBLE_LANGUAGE_VERSION..=tree_sitter::LANGUAGE_VERSION).contains(&v) {
            return Err(format!("grammar '{grammar}': ABI {v} not supported"));
        }
        // Kept open only once accepted (`language` points into it)
        self.libs.lock().unwrap().push(lib);
        Ok(language)
    }
}

/// A compiled `locals.scm` and what its captures mean.
struct Locals {
    query: Query,
    scope: Option<u32>,
    reference: Option<u32>,
    /// `@local.definition.X` capture → global capture number of highlight `X` (plain `@local.definition` → variable).
    defs: HashMap<u32, u32>,
    /// Per pattern: a scope that doesn't see its parents' definitions (`(#set! local.scope-inherits false)`).
    closed: Vec<bool>,
}

impl Locals {
    fn new(query: Query) -> Locals {
        let names = query.capture_names();
        let find = |n: &str| names.iter().position(|c| *c == n).map(|i| i as u32);
        let defs = names
            .iter()
            .enumerate()
            .filter_map(|(i, n)| {
                let hl = n.strip_prefix("local.definition")?;
                Some((
                    i as u32,
                    capture_id(hl.strip_prefix('.').filter(|h| !h.is_empty()).unwrap_or("variable")),
                ))
            })
            .collect();
        let closed = (0..query.pattern_count())
            .map(|p| {
                query
                    .property_settings(p)
                    .iter()
                    .any(|s| &*s.key == "local.scope-inherits" && s.value.as_deref() == Some("false"))
            })
            .collect();
        Locals { scope: find("local.scope"), reference: find("local.reference"), defs, closed, query }
    }
}

/// Nodes by (start, end) byte.
type NodeSet = HashSet<(usize, usize)>;

/// Local names within `range`: (start, end, global capture) for definitions and the references that resolve
/// to one, plus those nodes' (start, end) — for `#is-not? local`.
fn resolve_locals(
    l: &Locals,
    tree: &Tree,
    text: &Rope,
    range: &std::ops::Range<usize>,
    cursor: &mut QueryCursor,
) -> (Vec<(usize, usize, u32)>, NodeSet) {
    // Definitions above the screen count too: from the widest node around its first byte that starts within
    // 64 KB — the enclosing function even when it's long, not a whole huge `impl` of siblings (measured: 15 ms
    // a frame for 256 KB of methods above, debug build)
    let root = tree.root_node();
    let mut from = range.start;
    let mut node = root.descendant_for_byte_range(range.start, range.start);
    while let Some(n) = node.filter(|n| n.start_byte() + 64 * 1024 >= range.start) {
        from = from.min(n.start_byte());
        node = n.parent();
    }
    cursor.set_byte_range(from..range.end);
    let provider = |node: Node| node_slice(text, node).chunks().map(str::as_bytes);
    // Scopes: (end byte, parent, sees its parents' definitions); 0 = the whole file
    let mut scopes: Vec<(usize, usize, bool)> = vec![(usize::MAX, 0, true)];
    let mut open = vec![0];
    let mut defs: HashMap<(usize, String), Vec<(usize, u32)>> = HashMap::new();
    let mut out = Vec::new();
    let mut marked = HashSet::new();
    let mut refs: Vec<(usize, usize, usize, String)> = Vec::new();
    let mut caps = cursor.captures(&l.query, root, provider);
    while let Some((m, i)) = caps.next() {
        let c = m.captures()[*i];
        let (s, e) = (c.node.start_byte(), c.node.end_byte());
        while open.len() > 1 && scopes[*open.last().unwrap()].0 <= s {
            open.pop();
        }
        let here = *open.last().unwrap();
        let on_screen = s < range.end && e > range.start;
        if Some(c.index) == l.scope {
            scopes.push((e, here, !l.closed[m.pattern_index]));
            open.push(scopes.len() - 1);
        } else if let Some(&hl) = l.defs.get(&c.index) {
            defs.entry((here, node_slice(text, c.node).to_string())).or_default().push((s, hl));
            if on_screen && marked.insert((s, e)) {
                out.push((s, e, hl));
            }
        } else if Some(c.index) == l.reference && on_screen {
            refs.push((s, e, here, node_slice(text, c.node).to_string()));
        }
    }
    // A use takes the nearest enclosing scope's latest definition of that name before it
    for (s, e, scope, name) in refs {
        if marked.contains(&(s, e)) {
            continue;
        }
        let mut at = scope;
        loop {
            if let Some(&(_, hl)) =
                defs.get(&(at, name.clone())).and_then(|v| v.iter().rev().find(|d| d.0 <= s))
            {
                marked.insert((s, e));
                out.push((s, e, hl));
                break;
            }
            let (_, parent, sees) = scopes[at];
            if at == 0 || !sees {
                break;
            }
            at = parent;
        }
    }
    (out, marked)
}

/// `queries/<lang>/<file>` — replaces `; inherits: a,b` lines with the parent queries' contents.
fn read_query(lang: &str, file: &str, depth: usize) -> Option<String> {
    if depth > 8 {
        return None;
    }
    let src = runtime::query(&format!("{lang}/{file}"))?;
    let mut out = String::with_capacity(src.len());
    for line in src.lines() {
        match line.trim().strip_prefix("; inherits:") {
            Some(parents) => {
                for p in parents.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    out.push_str(&read_query(p, file, depth + 1).unwrap_or_default());
                    out.push('\n');
                }
            }
            None => {
                out.push_str(line);
                out.push('\n');
            }
        }
    }
    Some(out)
}

// ── Per-document syntax state ───────────────────────────────────────────

/// One injected-language layer — a tree parsing only part of the text (`set_included_ranges`) as that
/// language.
#[derive(Clone)]
pub struct Layer {
    pub lang: Arc<LangData>,
    pub tree: Tree,
    /// An edit since its parse may have changed what parsing it gives — else the next parse reuses it where
    /// the injection query asks for the same ranges.
    touched: bool,
    /// The injection query's matches in it, as that parse found them.
    found: Arc<Vec<Match>>,
    /// How many layers it's in (1: the document's) — the same pieces can be injected at two depths, and
    /// how deep decides whether its own are made.
    depth: usize,
}

impl Layer {
    /// Moves the tree along with an edit, and tells whether it could now parse differently: the edit
    /// reaches its text (touching counts — a piece may grow), or changes the gap between two pieces of a
    /// tree holding errors (how errors are recovered counts how far apart tokens are). Moving it all
    /// doesn't count (`parse_layer` parses it as if at the start), nor does moving a piece along its line:
    /// the lexer's column counts only the included text before it on the line.
    fn edit(&mut self, e: &InputEdit) {
        if !self.touched {
            let ranges = self.tree.included_ranges();
            let reaches = ranges.iter().any(|r| e.start_byte <= r.end_byte && r.start_byte <= e.old_end_byte);
            let (first, last) = (ranges.first(), ranges.last());
            let in_gap = first
                .zip(last)
                .is_some_and(|(f, l)| f.start_byte < e.start_byte && e.old_end_byte < l.end_byte);
            self.touched = reaches || in_gap && self.tree.root_node().has_error();
        }
        self.tree.edit(e);
    }
}

/// Highlight spans: (start, end, global capture number) — see `highlights`.
pub type Spans = Vec<(usize, usize, u32)>;

/// Tree + injected layers (undo snapshots carry the whole thing — cloning only bumps refcounts).
pub type Trees = (Tree, Arc<Vec<Layer>>);

pub struct Syntax {
    pub lang: Arc<LangData>,
    pub tree: Option<Tree>,
    /// Injected languages — outer layers first, inner later (later ones paint on top). Shared with undo
    /// snapshots (every command takes one, and a file can have thousands of layers) — copied on an edit.
    pub layers: Arc<Vec<Layer>>,
    /// Whether a background parse is running.
    pub in_flight: bool,
    /// Whether there were edits since the last parse started (need to reparse).
    pub dirty: bool,
    /// Edits that arrived mid-parse — reapplied when the new tree arrives.
    pending_edits: Vec<InputEdit>,
    /// Bumped when the text is replaced wholesale (like undo), to drop parse results run on the old text.
    pub generation: u64,
    /// A `fresh_tree` reparse ran out of time — not tried again until the next background tree lands.
    slow: std::cell::Cell<bool>,
    /// The last few `highlights` answers (range → spans) — a frame asks the same as the last one unless the
    /// text scrolled or changed, and the queries are most of a frame. Cleared whenever a tree changes.
    highlighted: std::cell::RefCell<Vec<(std::ops::Range<usize>, Spans)>>,
    /// Where `layers` has the injected languages: only around what was shown (`None`: everywhere) — the
    /// injection query and the layers' parsing cost what's on screen, not the file's size.
    covered: Option<Vec<std::ops::Range<usize>>>,
    /// What `highlights` was asked for since the last parse started.
    asked: std::cell::RefCell<Vec<std::ops::Range<usize>>>,
    /// The last few places shown (`RECENT`, latest last) — a parse injects around all of them, so going
    /// back and forth between two keeps both colored.
    recent: Vec<std::ops::Range<usize>>,
    /// Something was asked for outside `covered`: parse again (for the injections), though nothing changed.
    wants: std::cell::Cell<bool>,
    /// Time a parse may spend on injected layers (`INJECT_BUDGET`).
    inject_budget: std::time::Duration,
    /// Lines injected above and below what's shown (`NEAR_LINES`).
    near_lines: usize,
    /// The injection query's matches in the tree, as the last parse found them, and the edits since — so
    /// the next asks it only where something changed (`None`: not known, ask everywhere).
    found: Option<Arc<Vec<Match>>>,
    since: Option<Vec<InputEdit>>,
}

impl Syntax {
    pub fn new(lang: Arc<LangData>) -> Self {
        Self {
            lang,
            tree: None,
            layers: Arc::default(),
            in_flight: false,
            dirty: true,
            pending_edits: Vec::new(),
            generation: 0,
            slow: std::cell::Cell::new(false),
            highlighted: Default::default(),
            covered: Some(Vec::new()),
            asked: Default::default(),
            recent: Vec::new(),
            wants: std::cell::Cell::new(false),
            inject_budget: INJECT_BUDGET,
            near_lines: NEAR_LINES,
            found: None,
            since: None,
        }
    }

    /// A syntax for `text`, parsed whole right here — for a worker, or a piece small enough (a preview, a
    /// code block).
    pub fn parsed(lang: Arc<LangData>, text: &Rope) -> Self {
        let mut syn = Syntax::new(lang);
        syn.covered = None;
        let job = syn.start_parse(text);
        let generation = job.generation;
        syn.finish_parse(generation, job.run());
        syn
    }

    pub fn edit(&mut self, edits: &[InputEdit]) {
        if let Some(tree) = &mut self.tree {
            for e in edits {
                tree.edit(e);
            }
        }
        if !self.layers.is_empty() {
            for l in Arc::make_mut(&mut self.layers) {
                for e in edits {
                    l.edit(e);
                }
            }
        }
        if self.in_flight {
            self.pending_edits.extend_from_slice(edits);
        }
        if let Some(since) = &mut self.since {
            since.extend_from_slice(edits);
        }
        // Places shown move along with the text
        for e in edits {
            for r in
                self.recent.iter_mut().chain(self.asked.get_mut()).chain(self.covered.iter_mut().flatten())
            {
                *r = moved(r.clone(), e);
            }
        }
        self.dirty = true;
        self.highlighted.get_mut().clear();
    }

    pub fn trees(&self) -> Option<Trees> {
        Some((self.tree.clone()?, self.layers.clone()))
    }

    /// Text replaced wholesale (undo/redo) — use the tree matching that text if there is one.
    pub fn reset(&mut self, trees: Option<Trees>) {
        (self.tree, self.layers) = match trees {
            Some((t, l)) => (Some(t), l),
            None => (None, Arc::default()),
        };
        self.pending_edits.clear();
        self.generation += 1;
        self.dirty = true;
        self.highlighted.get_mut().clear();
        // Which edits lead from the snapshot's matches to its text isn't known
        (self.found, self.since) = (None, None);
        // The layers given are the snapshot's — where it had them isn't known: what's shown asks again
        if self.covered.is_some() {
            self.covered = Some(Vec::new());
        }
    }

    /// Where the buffer is likely shown, before a frame says (it's opened, or edited out of sight): the next
    /// parse injects there too, so a buffer's first colors come with its first tree.
    pub fn shown_near(&self, range: std::ops::Range<usize>) {
        if self.covered.is_some() {
            self.asked.borrow_mut().push(range);
        }
    }

    /// Whether a parse should run though no edit came: something shown needs injections not made yet.
    pub fn wants_injections(&self) -> bool {
        self.wants.get()
    }

    /// Starts a parse — returns the input to send to the worker thread.
    pub fn start_parse(&mut self, text: &Rope) -> ParseJob {
        self.in_flight = true;
        self.dirty = false;
        self.pending_edits.clear();
        self.wants.set(false);
        // Where to inject: around the last few places shown, what was shown since the last parse latest
        for r in std::mem::take(self.asked.get_mut()) {
            self.recent.retain(|x| x.end < r.start || r.end < x.start);
            self.recent.push(r);
        }
        let over = self.recent.len().saturating_sub(RECENT);
        self.recent.drain(..over);
        let near = self.covered.as_ref().map(|_| self.recent.clone());
        let since = self.since.replace(Vec::new());
        let before = self.found.clone().zip(since).map(|(found, since)| Before {
            found,
            since,
            near: self.covered.clone(),
        });
        ParseJob {
            text: text.clone(),
            old: self.tree.clone(),
            old_layers: self.layers.clone(),
            lang: self.lang.clone(),
            generation: self.generation,
            near,
            near_lines: self.near_lines,
            budget: self.inject_budget,
            before,
        }
    }

    /// Parse result arrived.
    pub fn finish_parse(&mut self, generation: u64, parsed: Parsed) {
        self.in_flight = false;
        self.slow.set(false);
        if generation != self.generation {
            self.dirty = true;
            return;
        }
        if let Some(mut tree) = parsed.tree {
            let mut layers = parsed.layers;
            if self.covered.is_some() {
                self.covered = parsed.covered;
                // What was shown while it ran may be covered now
                let covered = self.covered.as_deref().unwrap_or_default();
                let outside = self.asked.get_mut().iter().any(|r| !within(covered, r));
                self.wants.set(outside);
            }
            for e in self.pending_edits.drain(..) {
                tree.edit(&e);
                for l in &mut layers {
                    l.edit(&e);
                }
                for r in self.covered.iter_mut().flatten() {
                    *r = moved(r.clone(), &e);
                }
            }
            self.tree = Some(tree);
            self.layers = Arc::new(layers);
            self.found = Some(Arc::new(parsed.found));
            self.highlighted.get_mut().clear();
        } else {
            self.found = None;
        }
    }
}

pub struct ParseJob {
    text: Rope,
    old: Option<Tree>,
    /// The layers so far, edited along — the untouched ones are reused.
    old_layers: Arc<Vec<Layer>>,
    lang: Arc<LangData>,
    pub generation: u64,
    /// Inject only around these (`None`: everywhere).
    near: Option<Vec<std::ops::Range<usize>>>,
    near_lines: usize,
    budget: std::time::Duration,
    /// What the last parse found (`None`: not known).
    before: Option<Before>,
}

/// Parse result: tree + injected layers (+ where they are made).
pub struct Parsed {
    pub tree: Option<Tree>,
    pub layers: Vec<Layer>,
    covered: Option<Vec<std::ops::Range<usize>>>,
    /// How many layers, and matches of the injection query, were taken from the last parse, and how many
    /// layers were parsed from theirs.
    #[cfg_attr(not(test), allow(dead_code))] // for the tests
    reused: (usize, usize, usize),
    /// The injection query's matches in the tree.
    found: Vec<Match>,
}

impl Syntax {
    /// The tree for the text as it is now: the current one if no edit came since its parse started, else a
    /// reparse from it (incremental — edits already applied) right here, given up after `budget` (None).
    /// For answers that can't wait for the background parse — a new line's indentation. Real files take
    /// well under a millisecond (a 7k-line Rust file: 0.4 ms); one that ran out of time (a flat list of
    /// 20k items) isn't tried again until the background parse lands.
    pub fn fresh_tree(&self, text: &Rope, budget: std::time::Duration) -> Option<Tree> {
        if !self.dirty && !self.in_flight {
            return self.tree.clone();
        }
        if self.slow.get() {
            return None;
        }
        let mut parser = Parser::new();
        parser.set_language(&self.lang.language).ok()?;
        let deadline = std::time::Instant::now() + budget;
        let mut check = |_: &tree_sitter::ParseState| {
            if std::time::Instant::now() > deadline {
                std::ops::ControlFlow::Break(())
            } else {
                std::ops::ControlFlow::Continue(())
            }
        };
        let options = tree_sitter::ParseOptions::new().progress_callback(&mut check);
        let tree = parse_rope(&mut parser, text, self.tree.as_ref(), Some(options));
        self.slow.set(tree.is_none());
        tree
    }
}

/// Feeds rope chunks directly (no full copy).
fn parse_rope(
    parser: &mut Parser,
    text: &Rope,
    old: Option<&Tree>,
    options: Option<tree_sitter::ParseOptions>,
) -> Option<Tree> {
    let len = text.len_bytes();
    let mut read = |byte: usize, _: Point| -> &[u8] {
        if byte >= len {
            return &[];
        }
        let (chunk, start, _, _) = text.chunk_at_byte(byte);
        &chunk.as_bytes()[byte - start..]
    };
    parser.parse_with_options(&mut read, old, options)
}

/// A layer's parse over `ranges` (sorted, not empty), as if its text began the document, then moved to where
/// it is. How a parser recovers from an error counts how far a skipped token is from the one before — for
/// the first one, from the document's start — so else typing far above could change a layer's errors and
/// their colors; this way a layer parses the same wherever it is, and one only moved can be kept. From
/// `old` (a layer edited along) if given: what it has that the edits didn't reach is taken over.
fn parse_layer(
    parser: &mut Parser,
    text: &Rope,
    ranges: &[tree_sitter::Range],
    old: Option<&Tree>,
) -> Option<Tree> {
    let (base, at) = (ranges[0].start_byte, ranges[0].start_point);
    let back = |p: Point| match p.row == at.row {
        true => Point::new(0, p.column - at.column),
        false => Point::new(p.row - at.row, p.column),
    };
    let shifted: Vec<tree_sitter::Range> = ranges
        .iter()
        .map(|r| tree_sitter::Range {
            start_byte: r.start_byte - base,
            end_byte: r.end_byte - base,
            start_point: back(r.start_point),
            end_point: back(r.end_point),
        })
        .collect();
    parser.set_included_ranges(&shifted).ok()?;
    let len = text.len_bytes();
    let mut read = |byte: usize, _: Point| -> &[u8] {
        let byte = byte + base;
        if byte >= len {
            return &[];
        }
        let (chunk, start, _, _) = text.chunk_at_byte(byte);
        &chunk.as_bytes()[byte - start..]
    };
    let zero = Point::new(0, 0);
    // The old one as if it began there too: the text before deleted
    let old = old.map(|t| {
        let mut t = t.clone();
        t.edit(&InputEdit {
            start_byte: 0,
            old_end_byte: base,
            new_end_byte: 0,
            start_position: zero,
            old_end_position: at,
            new_end_position: zero,
        });
        t
    });
    let mut tree = parser.parse_with_options(&mut read, old.as_ref(), None)?;
    // Text inserted before it all: everything moves over by `base`, the first row's columns by `at`'s
    tree.edit(&InputEdit {
        start_byte: 0,
        old_end_byte: 0,
        new_end_byte: base,
        start_position: zero,
        old_end_position: zero,
        new_end_position: at,
    });
    Some(tree)
}

impl ParseJob {
    /// On a worker thread — incremental parse after edits. Injected languages only around what's shown
    /// (`near`, a few screens' worth up and down): a layer whose text no edit reached is kept, and the
    /// injection query asked only where the text or the tree changed and what's newly near.
    pub fn run(self) -> Parsed {
        let mut parser = Parser::new();
        let tree = parser
            .set_language(&self.lang.language)
            .ok()
            .and_then(|_| parse_rope(&mut parser, &self.text, self.old.as_ref(), None));
        let covered = self.near.map(|near| around(&self.text, near, self.near_lines));
        let (mut layers, mut reused, mut found) = (Vec::new(), (0, 0, 0), Vec::new());
        if let Some(tree) = &tree {
            let mut inj = Injector::new(&self.text, covered.as_deref(), self.budget);
            for l in self.old_layers.iter() {
                if l.touched {
                    if FROM_LAST.contains(&l.lang.name.as_str()) {
                        inj.redo.entry((l.lang.name.clone(), l.depth)).or_default().push(l.clone());
                    }
                } else {
                    let at = l.tree.included_ranges().first().map_or(0, |r| r.start_byte);
                    inj.reuse.entry((l.lang.name.clone(), l.depth, at)).or_default().push(l.clone());
                }
            }
            let old = self.old.as_ref().zip(self.before.as_ref());
            inj.before = old.map(|(_, before)| {
                // Each edit's new text, moved along with the edits after it
                let mut edits: Vec<std::ops::Range<usize>> = Vec::with_capacity(before.since.len());
                for e in &before.since {
                    for r in &mut edits {
                        *r = moved(r.clone(), e);
                    }
                    edits.push(e.start_byte..e.new_end_byte);
                }
                (before, edits)
            });
            let old = old.map(|(old, before)| {
                let mut kept = before.found.to_vec();
                for m in &mut kept {
                    for e in &before.since {
                        m.edit(e);
                    }
                }
                let edits = inj.before.as_ref().map_or(&[][..], |(_, e)| &e[..]);
                (old, kept, changed(old, tree, edits))
            });
            found = inj.found(&self.lang, tree, old);
            inj.run(&found, 0, &mut layers);
            reused = (inj.reused_count, inj.kept_count, inj.redone_count);
        }
        Parsed { tree, layers, covered, reused, found }
    }
}

// ── Injections ──────────────────────────────────────────────────────────

/// Caps on layer count·depth·time per document — beyond them the rest keeps the outer colors (not wrong).
const MAX_LAYERS: usize = 2000;
/// Lines injected above and below what's shown — scrolling a few screens finds them made.
const NEAR_LINES: usize = 150;

/// Byte ranges widened to whole lines, `lines` more each way, sorted and merged.
fn around(text: &Rope, ranges: Vec<std::ops::Range<usize>>, lines: usize) -> Vec<std::ops::Range<usize>> {
    let len = text.len_bytes();
    let mut out: Vec<std::ops::Range<usize>> = ranges
        .into_iter()
        .map(|r| {
            let first = text.byte_to_line(r.start.min(len)).saturating_sub(lines);
            let last = (text.byte_to_line(r.end.min(len)) + lines + 1).min(text.len_lines());
            text.line_to_byte(first)..text.line_to_byte(last)
        })
        .collect();
    out.sort_by_key(|r| r.start);
    let mut merged: Vec<std::ops::Range<usize>> = Vec::with_capacity(out.len());
    for r in out {
        match merged.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => merged.push(r),
        }
    }
    merged
}

/// Whether an injection query combines pieces from all over into a layer — then it's asked over the whole
/// tree even around what's shown. A combined pattern into a language this machine doesn't have makes no
/// layer (Rust's doc comments into `markdown-rustdoc`), so it doesn't count.
fn combines(q: &Query) -> bool {
    (0..q.pattern_count()).any(|i| {
        let props = q.property_settings(i);
        let prop = |k: &str| props.iter().find(|p| &*p.key == k);
        prop("injection.combined").is_some()
            && prop("injection.language")
                .and_then(|p| p.value.as_deref())
                .is_none_or(|lang| resolve(lang).is_some_and(|s| Loader::global().load(s).is_ok()))
    })
}

/// Languages whose layers an edit reached are parsed from the last tree, not afresh — where the random-edit
/// test shows that gives what a fresh parse does: HTML, which PHP, Markdown's HTML blocks and Go templates
/// make one big layer of. Not others, small layers mostly: what tree-sitter takes over from the last tree
/// can be parsed another way afresh — Markdown's inline grammar matches emphasis across a paragraph, a
/// span taken over misses a delimiter now matching; YAML's indentation after a Helm action — and in HTML
/// one holding an error is parsed afresh again (how errors are recovered from differs).
const FROM_LAST: &[&str] = &["html"];

/// Places shown kept for injecting around (`Syntax::recent`) — a window or two, and a place gone back to.
const RECENT: usize = 4;

/// Whether `r` lies within one of `ranges`.
fn within(ranges: &[std::ops::Range<usize>], r: &std::ops::Range<usize>) -> bool {
    ranges.iter().any(|c| c.start <= r.start && r.end <= c.end)
}

/// A range moved along with an edit: shifted after it; a bound inside what it replaced goes to the
/// replacement's start (the range's start) or end (its end).
fn moved(r: std::ops::Range<usize>, e: &InputEdit) -> std::ops::Range<usize> {
    let at = |b: usize, inside: usize| {
        if b >= e.old_end_byte {
            b - e.old_end_byte + e.new_end_byte
        } else if b > e.start_byte {
            inside
        } else {
            b
        }
    };
    at(r.start, e.start_byte)..at(r.end, e.new_end_byte)
}

/// Whether byte range `r` meets one of `near` (sorted, merged) — touching counts.
fn meets(near: &[std::ops::Range<usize>], start: usize, end: usize) -> bool {
    let from = near.partition_point(|c| c.end < start);
    near.get(from).is_some_and(|c| c.start <= end)
}
const MAX_DEPTH: usize = 3;
const INJECT_BUDGET: std::time::Duration = std::time::Duration::from_millis(80);

/// A match of an injection query, kept from one parse to the next.
#[derive(Clone)]
struct Match {
    pattern: usize,
    lang: String,
    combined: bool,
    /// Each content node with its injected pieces.
    nodes: Vec<(std::ops::Range<usize>, Vec<tree_sitter::Range>)>,
    /// Every capture: (index, start, end) — what tells two matches apart, and which an edit reached.
    captures: Vec<(u32, usize, usize)>,
}

impl Match {
    fn start(&self) -> usize {
        self.nodes.first().map_or(0, |(n, _)| n.start)
    }

    /// Moved along with an edit (one it reached is asked again anyway, so where it lands doesn't matter).
    fn edit(&mut self, e: &InputEdit) {
        for (node, pieces) in &mut self.nodes {
            *node = moved(node.clone(), e);
            for r in pieces {
                *r = edited(*r, e);
            }
        }
        for c in &mut self.captures {
            let r = moved(c.1..c.2, e);
            (c.1, c.2) = (r.start, r.end);
        }
    }
}

/// A tree's last parse for `Injector::found`: its tree as edited since, its matches moved along, and where
/// the text or the tree changed.
type Was<'t> = (&'t Tree, Vec<Match>, Vec<std::ops::Range<usize>>);

/// What the last parse found, so this one asks the query again only where something may have changed.
struct Before {
    /// The document's matches, in the text that parse ran on (layers keep theirs).
    found: Arc<Vec<Match>>,
    /// The edits since, in order.
    since: Vec<InputEdit>,
    /// Where it injected around (`None`: everywhere), as the text is now.
    near: Option<Vec<std::ops::Range<usize>>>,
}

/// A range moved along with an edit as tree-sitter moves a tree's (`ts_range_edit`): points too.
fn edited(mut r: tree_sitter::Range, e: &InputEdit) -> tree_sitter::Range {
    let at = |b: usize, p: Point| {
        if b >= e.old_end_byte {
            let p = match p.row > e.old_end_position.row {
                true => Point::new(p.row - e.old_end_position.row, p.column),
                false => Point::new(0, p.column.saturating_sub(e.old_end_position.column)),
            };
            let p = match p.row > 0 {
                true => Point::new(e.new_end_position.row + p.row, p.column),
                false => Point::new(e.new_end_position.row, e.new_end_position.column + p.column),
            };
            (b - e.old_end_byte + e.new_end_byte, p)
        } else if b > e.start_byte {
            (e.new_end_byte, e.new_end_position)
        } else {
            (b, p)
        }
    };
    (r.start_byte, r.start_point) = at(r.start_byte, r.start_point);
    (r.end_byte, r.end_point) = at(r.end_byte, r.end_point);
    r
}

/// Where a tree may differ from the last one (edited along — for a kept layer, the same): where the text changed
/// (`edits`) and the tree's changed ranges — a byte wider each way (touching counts), then widened to the
/// tokens, and the errors, of either tree its ends are in, until none reaches out. The changed ranges say
/// where a byte's ancestors changed, which misses some: two tokens of a kind merged into one (every byte
/// still in a comment) or one split — tree-sitter steps over two same-kind tokens without looking whether
/// their ends agree — and an empty node moved or made where an error was recovered from (Svelte's `{}`).
fn changed(old: &Tree, tree: &Tree, edits: &[std::ops::Range<usize>]) -> Vec<std::ops::Range<usize>> {
    let changes = old.changed_ranges(tree).map(|r| r.start_byte..r.end_byte);
    let wider = |r: std::ops::Range<usize>| r.start.saturating_sub(1)..r.end + 1;
    let mut out = merged(edits.iter().cloned().chain(changes).map(wider).collect());
    loop {
        let mut grown = false;
        for r in &mut out {
            for t in [old, tree] {
                // What its ends are in
                for at in [r.start, r.end - 1] {
                    let mut n = t.root_node().descendant_for_byte_range(at, at + 1);
                    let mut token = true;
                    while let Some(node) = n {
                        let reaches_out = node.start_byte() < r.start || r.end < node.end_byte();
                        if (token && node.child_count() == 0 || node.is_error()) && reaches_out {
                            *r = r.start.min(node.start_byte())..r.end.max(node.end_byte());
                            grown = true;
                        }
                        (n, token) = (node.parent(), false);
                    }
                }
            }
        }
        out = merged(out);
        if !grown {
            return out;
        }
    }
}

/// Ranges sorted and merged — touching ones too.
fn merged(mut ranges: Vec<std::ops::Range<usize>>) -> Vec<std::ops::Range<usize>> {
    ranges.sort_by_key(|r| r.start);
    let mut out: Vec<std::ops::Range<usize>> = Vec::with_capacity(ranges.len());
    for r in ranges {
        match out.last_mut() {
            Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
            _ => out.push(r),
        }
    }
    out
}

/// The parts of `a` outside `b` (both sorted, merged) — an empty one in `a` (a window at the end) too,
/// unless `b` meets it.
fn outside(a: &[std::ops::Range<usize>], b: &[std::ops::Range<usize>]) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    for r in a {
        if r.is_empty() {
            if !meets(b, r.start, r.end) {
                out.push(r.clone());
            }
            continue;
        }
        let mut from = r.start;
        for c in b.iter().filter(|c| c.start < r.end && r.start < c.end) {
            if c.start > from {
                out.push(from..c.start);
            }
            from = from.max(c.end);
        }
        if from < r.end {
            out.push(from..r.end);
        }
    }
    out
}

struct Injector<'a> {
    text: &'a Rope,
    parsers: HashMap<String, Parser>,
    /// Layers parsed before and untouched since, by (language, depth, where the first piece starts): one
    /// asked for with the same pieces again is taken from here, not parsed.
    reuse: HashMap<(String, usize, usize), Vec<Layer>>,
    /// Layers an edit reached, by (language, depth): one asked for where one of these was is parsed from it.
    redo: HashMap<(String, usize), Vec<Layer>>,
    /// How many were kept, parsed from the last, and how many matches were kept.
    reused_count: usize,
    redone_count: usize,
    kept_count: usize,
    /// Time spent parsing injected languages (grammar loading·query compilation happen once, so not counted).
    spent: std::time::Duration,
    budget: std::time::Duration,
    /// Only the layers meeting these (sorted, merged; `None`: all).
    near: Option<&'a [std::ops::Range<usize>]>,
    /// Ask the query over the whole tree even so (what the tests compare with).
    whole_query: bool,
    /// The last parse's, and each edit since as the text is now: matches are kept, and the query asked
    /// only where edits were and what's newly near (`None`: everywhere).
    before: Option<(&'a Before, Vec<std::ops::Range<usize>>)>,
}

/// Node range — children excluded (default), only unnamed children included (`include-unnamed-children`),
/// or whole (`include-children`).
fn node_ranges(node: Node, children: bool, unnamed: bool) -> Vec<tree_sitter::Range> {
    if children {
        return vec![node.range()];
    }
    let mut out = Vec::new();
    let (mut sb, mut sp) = (node.start_byte(), node.start_position());
    let mut cur = node.walk();
    for child in node.children(&mut cur) {
        if unnamed && !child.is_named() {
            continue;
        }
        if child.start_byte() > sb {
            out.push(tree_sitter::Range {
                start_byte: sb,
                end_byte: child.start_byte(),
                start_point: sp,
                end_point: child.start_position(),
            });
        }
        (sb, sp) = (child.end_byte().max(sb), child.end_position());
    }
    if node.end_byte() > sb {
        out.push(tree_sitter::Range {
            start_byte: sb,
            end_byte: node.end_byte(),
            start_point: sp,
            end_point: node.end_position(),
        });
    }
    out
}

/// A node's text — ends snapped to char boundaries, so a briefly stale tree (edits not yet reparsed) stays
/// safe to slice.
pub(crate) fn node_slice<'a>(text: &'a Rope, node: Node) -> ropey::RopeSlice<'a> {
    let len = text.len_bytes();
    // Already on a boundary almost always — told by the chunk, one lookup instead of a round trip via chars
    let snap = |b: usize| {
        let b = b.min(len);
        let (chunk, at, _, _) = text.chunk_at_byte(b);
        if chunk.is_char_boundary(b - at) { b } else { text.char_to_byte(text.byte_to_char(b)) }
    };
    let (s, e) = (snap(node.start_byte()), snap(node.end_byte()));
    text.byte_slice(s..e.max(s))
}

/// `#!/usr/bin/env python3` → `python3`.
fn shebang(first_line: &str) -> Option<String> {
    let rest = first_line.strip_prefix("#!")?;
    let mut words = rest.split_whitespace();
    let prog = words.next()?.rsplit('/').next()?;
    let prog = if prog == "env" { words.find(|w| !w.starts_with('-'))? } else { prog };
    Some(prog.to_string())
}

impl<'a> Injector<'a> {
    fn new(text: &'a Rope, near: Option<&'a [std::ops::Range<usize>]>, budget: std::time::Duration) -> Self {
        Injector {
            text,
            parsers: HashMap::new(),
            reuse: HashMap::new(),
            redo: HashMap::new(),
            reused_count: 0,
            redone_count: 0,
            kept_count: 0,
            spent: std::time::Duration::ZERO,
            budget,
            near,
            whole_query: false,
            before: None,
        }
    }

    fn node_text(&self, node: Node) -> String {
        node_slice(self.text, node).chars().take(200).collect()
    }

    /// A layer parsed before over exactly these pieces, untouched since.
    fn reused(&mut self, lang: &str, depth: usize, ranges: &[tree_sitter::Range]) -> Option<Layer> {
        let at = ranges.first()?.start_byte;
        let same = self.reuse.get_mut(&(lang.to_string(), depth, at))?;
        let i = same.iter().position(|l| l.tree.included_ranges() == ranges)?;
        self.reused_count += 1;
        Some(same.swap_remove(i))
    }

    /// The layer an edit reached that was where these pieces are (spans overlapping most), if any.
    fn redone(&mut self, lang: &str, depth: usize, ranges: &[tree_sitter::Range]) -> Option<Layer> {
        let span = |r: &[tree_sitter::Range]| {
            r.first().map_or(0, |r| r.start_byte)..r.last().map_or(0, |r| r.end_byte)
        };
        let new = span(ranges);
        let same = self.redo.get_mut(&(lang.to_string(), depth))?;
        let overlap = |l: &Layer| {
            let old = span(&l.tree.included_ranges());
            old.end.min(new.end) as isize - old.start.max(new.start) as isize
        };
        let (i, _) = same
            .iter()
            .enumerate()
            .map(|(i, l)| (i, overlap(l)))
            .filter(|&(_, o)| o > 0)
            .max_by_key(|&(_, o)| o)?;
        self.redone_count += 1;
        Some(same.swap_remove(i))
    }

    /// The query's matches in `tree` over `passes` (sorted, merged) — those far from what's shown left out,
    /// but where the language `combines` its pieces from all over.
    fn ask(
        &self,
        q: &Query,
        combines: bool,
        tree: &Tree,
        passes: &[std::ops::Range<usize>],
        out: &mut Vec<Match>,
    ) {
        let idx = |name: &str| q.capture_names().iter().position(|n| *n == name).map(|i| i as u32);
        let (content_i, lang_i, shebang_i, file_i) = (
            idx("injection.content"),
            idx("injection.language"),
            idx("injection.shebang"),
            idx("injection.filename"),
        );
        let text = self.text;
        let provider = |node: Node| node_slice(text, node).chunks().map(str::as_bytes);
        let mut cursor = QueryCursor::new();
        for pass in passes {
            // A byte wider: the cursor leaves out a node only touching the range (`meets` counts it), and
            // an empty one at its end
            cursor.set_byte_range(pass.start.saturating_sub(1)..pass.end.saturating_add(1));
            let mut matches = cursor.matches(q, tree.root_node(), provider);
            while let Some(m) = matches.next() {
                let props = q.property_settings(m.pattern_index);
                let prop = |k: &str| props.iter().find(|p| &*p.key == k);
                // Far from what's shown: not worth the work (by the whole node — two patterns on one node
                // take its pieces differently, and which wins the spot mustn't change)
                if let Some(near) = self.near
                    && !(combines && prop("injection.combined").is_some())
                    && !m.captures().iter().any(|c| {
                        Some(c.index) == content_i && meets(near, c.node.start_byte(), c.node.end_byte())
                    })
                {
                    continue;
                }
                let mut lang =
                    prop("injection.language").and_then(|p| p.value.as_deref().map(str::to_string));
                let mut content = Vec::new();
                for c in m.captures() {
                    let i = Some(c.index);
                    if i == content_i {
                        content.push(c.node);
                    }
                    if i == lang_i {
                        lang = Some(self.node_text(c.node));
                    } else if i == shebang_i && lang.is_none() {
                        let first = self.node_text(c.node);
                        lang = shebang(first.lines().next().unwrap_or_default());
                    } else if i == file_i && lang.is_none() {
                        let f = self.node_text(c.node);
                        lang = detect(Path::new(f.trim())).map(|s| s.name.clone());
                    }
                }
                let Some(lang) = lang else { continue };
                let (children, unnamed) = (
                    prop("injection.include-children").is_some(),
                    prop("injection.include-unnamed-children").is_some(),
                );
                out.push(Match {
                    pattern: m.pattern_index,
                    lang,
                    combined: prop("injection.combined").is_some(),
                    nodes: content
                        .iter()
                        .map(|&n| (n.byte_range(), node_ranges(n, children, unnamed)))
                        .collect(),
                    captures: m
                        .captures()
                        .iter()
                        .map(|c| (c.index, c.node.start_byte(), c.node.end_byte()))
                        .collect(),
                });
            }
        }
    }

    /// The query's matches in `tree` (the document's or a layer's). With what the last parse found there —
    /// `old`: its tree as edited since, its matches moved along, and where the text or the tree changed
    /// (sorted, merged) — those are kept but where it changed, and the query is asked only there and
    /// where's newly near.
    ///
    /// The cursor starts a match only at a node meeting the range it's asked over (or whose parent does,
    /// for a pattern of siblings), and one starting at a node clear of every change is the same in both
    /// trees. So the matches to drop are the old tree's there (asked over the same ranges — their captured
    /// text is unchanged, so they still match — plus any capturing what changed), and the ones to add the
    /// new tree's.
    fn found(&mut self, host: &LangData, tree: &Tree, old: Option<Was>) -> Vec<Match> {
        let Some(q) = &host.injections else { return Vec::new() };
        // Around what's shown, the query asks only there (a pass per range) — unless the language combines
        // what it injects: those pieces come from all over
        let combines = *host.combines.get_or_init(|| combines(q));
        let all = 0..usize::MAX;
        let everywhere = std::slice::from_ref(&all);
        let near = match self.near {
            Some(near) if !combines && !self.whole_query => near,
            _ => everywhere,
        };
        let mut out = Vec::new();
        let key = |m: &Match| (m.pattern, m.captures.clone());
        match (old, &self.before) {
            (Some((old_tree, mut kept, changed)), Some((before, _))) => {
                // Newly near: where far matches were left out before and aren't now (in a language that
                // combines, the query was asked everywhere, but its far matches left out all the same)
                let newly = match &before.near {
                    None => Vec::new(),
                    Some(was) => outside(self.near.unwrap_or(everywhere), was),
                };
                let mut gone = Vec::new();
                self.ask(q, combines, old_tree, &changed, &mut gone);
                let gone: std::collections::HashSet<_> = gone.iter().map(key).collect();
                kept.retain(|m| {
                    let reached = m.captures.iter().any(|&(_, s, e)| meets(&changed, s, e));
                    let far = !(combines && m.combined)
                        && self.near.is_some_and(|n| !m.nodes.iter().any(|(r, _)| meets(n, r.start, r.end)));
                    !reached && !far && !gone.contains(&key(m))
                });
                self.kept_count += kept.len();
                out = kept;
                self.ask(q, combines, tree, &merged([changed, newly].concat()), &mut out);
            }
            _ => self.ask(q, combines, tree, near, &mut out),
        }
        // A match reaching into two passes comes twice; a kept one may come again
        let mut seen = std::collections::HashSet::new();
        out.retain(|m| seen.insert(key(m)));
        out
    }

    /// `found` for a layer, from the layer it was (`was`: its tree, edited along, and matches): what changed
    /// is the edits within their spans — a node spanning a gap between pieces reads the text there — and
    /// where the trees differ (a kept layer's is the same).
    fn layer_found(
        &mut self,
        data: &LangData,
        tree: &Tree,
        was: Option<(&Tree, &[Match])>,
        depth: usize,
    ) -> Vec<Match> {
        if depth >= MAX_DEPTH {
            return Vec::new();
        }
        let old = was.zip(self.before.as_ref()).map(|((old_tree, was), (before, edits))| {
            let mut kept = was.to_vec();
            for m in &mut kept {
                for e in &before.since {
                    m.edit(e);
                }
            }
            let (a, b) = (tree.included_ranges(), old_tree.included_ranges());
            let start = a.iter().chain(&b).map(|r| r.start_byte).min().unwrap_or(0);
            let end = a.iter().chain(&b).map(|r| r.end_byte).max().unwrap_or(0);
            let within: Vec<_> = edits.iter().filter(|r| r.start <= end && start <= r.end).cloned().collect();
            // A kept one nothing changed in: no need to look (a file has hundreds of layers)
            let same = std::ptr::eq(old_tree, tree);
            let changed =
                if same && within.is_empty() { Vec::new() } else { changed(old_tree, tree, &within) };
            (old_tree, kept, changed)
        });
        self.found(data, tree, old)
    }

    /// The layers `found` makes (and theirs, nested).
    fn run(&mut self, found: &[Match], depth: usize, out: &mut Vec<Layer>) {
        // In document order — passes over ranges apart give them in another order than one pass over all,
        // and kept ones come first. Where two patterns catch the same spot (shebang·language name,
        // `lang="ts"` or not) the one listed first wins.
        let mut found: Vec<&Match> = found.iter().collect();
        found.sort_by(|a, b| (a.start(), a.pattern, &a.captures).cmp(&(b.start(), b.pattern, &b.captures)));
        // (language, ranges) — combined ones (`injection.combined`) become one per (pattern, language)
        let mut single: Vec<(String, Vec<tree_sitter::Range>)> = Vec::new();
        let mut combined: Vec<((usize, String), Vec<tree_sitter::Range>)> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for m in found {
            let mut ranges = Vec::new();
            for (node, pieces) in &m.nodes {
                if seen.insert((node.start, node.end)) {
                    ranges.extend_from_slice(pieces);
                }
            }
            if ranges.is_empty() {
                continue;
            }
            if m.combined {
                let key = (m.pattern, m.lang.clone());
                match combined.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, r)) => r.extend(ranges),
                    None => combined.push((key, ranges)),
                }
            } else {
                single.push((m.lang.clone(), ranges));
            }
        }
        let all = combined.into_iter().map(|((_, l), r)| (l, r)).chain(single);
        for (lang, mut ranges) in all {
            // Only layers meeting what's shown
            if let Some(near) = self.near
                && !ranges.iter().any(|r| meets(near, r.start_byte, r.end_byte))
            {
                continue;
            }
            if out.len() >= MAX_LAYERS || self.spent > self.budget {
                return;
            }
            let Some(data) = resolve(&lang).and_then(|s| Loader::global().load(s).ok()) else { continue };
            ranges.sort_by_key(|r| r.start_byte);
            ranges.dedup_by(|b, a| b.start_byte < a.end_byte);
            let (tree, was) = match self.reused(&data.name, depth + 1, &ranges) {
                Some(l) => (l.tree, Some((None, l.found))),
                None => {
                    let old = self.redone(&data.name, depth + 1, &ranges);
                    let parser = self.parsers.entry(data.name.clone()).or_insert_with(|| {
                        let mut p = Parser::new();
                        let _ = p.set_language(&data.language);
                        p
                    });
                    let t = std::time::Instant::now();
                    // One holding an error is always a fresh parse (`FROM_LAST`)
                    let from = old.as_ref().map(|l| &l.tree);
                    let mut tree = parse_layer(parser, self.text, &ranges, from);
                    if from.is_some() && tree.as_ref().is_some_and(|t| t.root_node().has_error()) {
                        tree = parse_layer(parser, self.text, &ranges, None);
                    }
                    self.spent += t.elapsed();
                    let Some(tree) = tree else { continue };
                    (tree, old.map(|l| (Some(l.tree), l.found)))
                }
            };
            // A kept layer's last tree is itself
            let was = was.as_ref().map(|(old, found)| (old.as_ref().unwrap_or(&tree), &found[..]));
            let found = Arc::new(self.layer_found(&data, &tree, was, depth + 1));
            out.push(Layer {
                lang: data.clone(),
                tree,
                touched: false,
                found: found.clone(),
                depth: depth + 1,
            });
            self.run(&found, depth + 1, out);
        }
    }
}

// ── Definition location (header path) ─────────────────────────────────

/// Definitions enclosing the cursor, outer → inner: (kind 'f' function · 't' type · 'm' module, name).
/// Guessed from node names, not per-grammar queries (`function_item`, `class_definition`, `impl_item` …).
pub fn scope_path(syn: &Syntax, text: &Rope, pos: usize) -> Vec<(char, String)> {
    let Some(tree) = &syn.tree else { return Vec::new() };
    let len = text.len_bytes();
    let mut node = tree.root_node().descendant_for_byte_range(pos.min(len), pos.min(len));
    let mut out = Vec::new();
    while let Some(n) = node {
        if let Some(c) = def_kind(n)
            && let Some(name) = n.child_by_field_name("name").or_else(|| n.child_by_field_name("type"))
        {
            let s: String = node_slice(text, name).chars().take(40).collect();
            if !s.is_empty() {
                out.push((c, s));
            }
        }
        node = n.parent();
    }
    out.reverse();
    out
}

/// What kind of definition a node is ('f' function · 't' type · 'm' module), if it is one.
fn def_kind(n: Node) -> Option<char> {
    let k = n.kind();
    if !["_item", "_definition", "_declaration", "_specifier"].iter().any(|s| k.ends_with(s)) {
        None
    } else if k.contains("function") || k.contains("method") {
        Some('f')
    } else if k == "impl_item"
        || ["struct", "enum", "trait", "class", "interface", "type_item", "union"]
            .iter()
            .any(|w| k.starts_with(w))
    {
        Some('t')
    } else if k.starts_with("mod") || k.starts_with("namespace") {
        Some('m')
    } else {
        None
    }
}

/// A definition in a file's outline.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Def {
    /// How many definitions enclose it.
    pub depth: usize,
    pub kind: char,
    pub name: String,
    /// The whole definition, and where its name is (bytes).
    pub range: std::ops::Range<usize>,
    pub at: usize,
}

/// Every definition in the file, in order (the same guess as `scope_path`). `impl` blocks read
/// `impl Trait for Type`. Walks the whole tree — run it on a worker thread.
pub fn outline(tree: &Tree, text: &Rope) -> Vec<Def> {
    let mut out = Vec::new();
    let mut cursor = tree.walk();
    let mut depth = 0; // definitions open on the way down
    let mut open: Vec<usize> = Vec::new(); // tree depth of each open definition
    let mut level = 0;
    loop {
        let n = cursor.node();
        if let Some(kind) = def_kind(n)
            && let Some(name) = def_name(n, text)
        {
            let at = n
                .child_by_field_name("name")
                .or_else(|| n.child_by_field_name("type"))
                .map_or(n.start_byte(), |x| x.start_byte());
            out.push(Def { depth, kind, name, range: n.start_byte()..n.end_byte(), at });
            depth += 1;
            open.push(level);
        }
        if cursor.goto_first_child() {
            level += 1;
            continue;
        }
        loop {
            // Leaving this node: close the definition it was
            if open.last() == Some(&level) {
                open.pop();
                depth -= 1;
            }
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return out;
            }
            level -= 1;
        }
    }
}

fn def_name(n: Node, text: &Rope) -> Option<String> {
    let slice = |x: Node| -> String {
        node_slice(text, x).to_string().split_whitespace().collect::<Vec<_>>().join(" ")
    };
    let s = if n.kind() == "impl_item" {
        let ty = slice(n.child_by_field_name("type")?);
        match n.child_by_field_name("trait") {
            Some(t) => format!("impl {} for {ty}", slice(t)),
            None => format!("impl {ty}"),
        }
    } else {
        slice(n.child_by_field_name("name").or_else(|| n.child_by_field_name("type"))?)
    };
    let s: String = s.chars().take(60).collect();
    (!s.is_empty()).then_some(s)
}

// ── Edit → InputEdit ────────────────────────────────────────────────────

fn point(text: &Rope, byte: usize) -> Point {
    let row = text.byte_to_line(byte);
    Point { row, column: byte - text.line_to_byte(row) }
}

/// Call **right before** applying one change to the rope (positions relative to the text at that moment).
pub fn input_edit(text: &Rope, from: usize, to: usize, insert: &str) -> InputEdit {
    let start = point(text, from);
    let new_end = match insert.rfind('\n') {
        Some(i) => Point { row: start.row + insert.matches('\n').count(), column: insert.len() - i - 1 },
        None => Point { row: start.row, column: start.column + insert.len() },
    };
    InputEdit {
        start_byte: from,
        old_end_byte: to,
        new_end_byte: from + insert.len(),
        start_position: start,
        old_end_position: point(text, to),
        new_end_position: new_end,
    }
}

// ── Highlighting ────────────────────────────────────────────────────────

/// Captures of one language·one tree (start, end, global capture number, pattern number).
fn query_spans(
    lang: &LangData,
    tree: &Tree,
    text: &Rope,
    range: &std::ops::Range<usize>,
    cursor: &mut QueryCursor,
) -> Vec<(usize, usize, u32, usize)> {
    let (locals, local_nodes) = match &lang.locals {
        Some(l) => resolve_locals(l, tree, text, range, cursor),
        None => Default::default(),
    };
    cursor.set_byte_range(range.clone());
    // For text predicates (#eq? #match? …)
    let provider = |node: Node| node_slice(text, node).chunks().map(str::as_bytes);
    let mut spans = Vec::new();
    let mut caps = cursor.captures(&lang.highlights, tree.root_node(), provider);
    while let Some((m, i)) = caps.next() {
        let c = m.captures()[*i];
        if lang.not_local[m.pattern_index] && local_nodes.contains(&(c.node.start_byte(), c.node.end_byte()))
        {
            continue;
        }
        let (s, e) = (c.node.start_byte().max(range.start), c.node.end_byte().min(range.end));
        if s < e {
            spans.push((s, e, lang.global[c.index as usize], m.pattern_index));
        }
    }
    // Local names paint over everything else on the same node (last among equals)
    for (s, e, hl) in locals {
        let (s, e) = (s.max(range.start), e.min(range.end));
        if s < e {
            spans.push((s, e, hl, usize::MAX));
        }
    }
    spans.sort_by(|a, b| (b.1 - b.0).cmp(&(a.1 - a.0)).then(a.3.cmp(&b.3)));
    spans
}

/// (start, end, global capture number — index into the `capture_styles` table) within `range`. In draw
/// order — outer first, same node earlier pattern first (so inner·later patterns paint on top and win).
/// Injected languages come after (painted on top).
pub fn highlights(
    syn: &Syntax,
    text: &Rope,
    range: std::ops::Range<usize>,
    cursor: &mut QueryCursor,
) -> Vec<(usize, usize, u32)> {
    const KEEP: usize = 4; // a few panes' worth
    // What's shown is where the next parse injects; outside what's injected now, one is wanted (if the
    // language injects anything)
    if let Some(covered) = &syn.covered
        && syn.lang.injections.is_some()
    {
        let mut asked = syn.asked.borrow_mut();
        if !asked.iter().any(|r| r.start <= range.start && range.end <= r.end) {
            asked.push(range.clone());
        }
        if !within(covered, &range) {
            syn.wants.set(true);
        }
    }
    let mut memo = syn.highlighted.borrow_mut();
    if let Some(k) = memo.iter().position(|(r, _)| *r == range) {
        let hit = memo.remove(k);
        let spans = hit.1.clone();
        memo.push(hit);
        return spans;
    }
    let spans = query_all(syn, text, &range, cursor);
    if memo.len() == KEEP {
        memo.remove(0);
    }
    memo.push((range, spans.clone()));
    spans
}

fn query_all(
    syn: &Syntax,
    text: &Rope,
    range: &std::ops::Range<usize>,
    cursor: &mut QueryCursor,
) -> Vec<(usize, usize, u32)> {
    let range = range.clone();
    let Some(tree) = &syn.tree else { return Vec::new() };
    let strip = |v: Vec<(usize, usize, u32, usize)>| v.into_iter().map(|(s, e, c, _)| (s, e, c));
    let mut out: Vec<(usize, usize, u32)> =
        strip(query_spans(&syn.lang, tree, text, &range, cursor)).collect();
    for l in syn.layers.iter() {
        let r = l.tree.root_node().byte_range();
        if r.end <= range.start || r.start >= range.end {
            continue;
        }
        let spans = query_spans(&l.lang, &l.tree, text, &range, cursor);
        // Layers stitched from several pieces (Helm's YAML etc.) have nodes spanning the gaps (template
        // spots) — paint only within the given ranges
        let pieces = l.tree.included_ranges();
        if pieces.len() <= 1 {
            out.extend(strip(spans));
            continue;
        }
        for (s, e, c, _) in spans {
            let from = pieces.partition_point(|p| p.end_byte <= s);
            for p in pieces[from..].iter().take_while(|p| p.start_byte < e) {
                let (a, b) = (s.max(p.start_byte), e.min(p.end_byte));
                if a < b {
                    out.push((a, b, c));
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_languages() {
        let name = |p: &str| detect(Path::new(p)).map(|s| s.name.as_str());
        assert_eq!(name("src/main.rs"), Some("rust"));
        assert_eq!(name("/x/Makefile"), Some("make"));
        assert_eq!(name("/home/u/.ssh/config"), Some("sshclientconfig"));
        assert_eq!(name("/proj/config"), None, "common names only by path");
        assert_eq!(name("a.tsx"), Some("tsx"));
        assert_eq!(spec("protobuf").map(|s| s.grammar.as_str()), Some("proto"));
        // Helm chart templates are helm, not YAML (Go template + text between injected as YAML)
        assert_eq!(name("/charts/app/templates/deployment.yaml"), Some("helm"));
        assert_eq!(name("/charts/app/templates/_helpers.tpl"), Some("helm"));
        assert_eq!(name("/charts/app/templates/NOTES.txt"), Some("helm"));
        assert_eq!(
            name("/charts/app/templates/tests/test-connection.yaml"),
            Some("helm"),
            "a folder under them"
        );
        assert_eq!(name("/charts/app/values.yaml"), Some("yaml"));
        assert_eq!(name("/x/tmpl.gotmpl"), Some("gotmpl"));
    }

    #[test]
    fn input_edit_points() {
        let t = Rope::from_str("ab\ncd");
        let e = input_edit(&t, 1, 4, "X\nYZ");
        assert_eq!(e.start_position, Point { row: 0, column: 1 });
        assert_eq!(e.old_end_position, Point { row: 1, column: 1 });
        assert_eq!(e.new_end_position, Point { row: 1, column: 2 });
        assert_eq!(e.new_end_byte, 5);
    }

    /// The outline: every definition in order, nested ones deeper, `impl` blocks named for what they're
    /// of. Only with the Rust grammar.
    #[test]
    fn outline_lists_definitions_nested() {
        let Ok(lang) = Loader::global().load(spec("rust").unwrap()) else { return };
        let src = "struct P;\nimpl Display for P {\n    fn fmt(&self) {}\n}\nmod m {\n    fn inner() { let x = 1; }\n}\nfn last() {}\n";
        let text = Rope::from_str(src);
        let mut syn = Syntax::new(lang);
        let job = syn.start_parse(&text);
        let generation = job.generation;
        syn.finish_parse(generation, job.run());
        let defs = outline(syn.tree.as_ref().unwrap(), &text);
        let got: Vec<(usize, char, &str)> = defs.iter().map(|d| (d.depth, d.kind, d.name.as_str())).collect();
        assert_eq!(
            got,
            [
                (0, 't', "P"),
                (0, 't', "impl Display for P"),
                (1, 'f', "fmt"),
                (0, 'm', "m"),
                (1, 'f', "inner"),
                (0, 'f', "last")
            ]
        );
        assert_eq!(&src[defs[2].at..defs[2].at + 3], "fmt", "where its name is");
    }

    /// Only when the real grammar is present (downloaded or built in) — skipped otherwise.
    #[test]
    fn highlights_rust_when_grammar_available() {
        let Ok(lang) = Loader::global().load(spec("rust").unwrap()) else { return };
        let text = Rope::from_str("fn main() { let x = \"hi\"; }\n");
        let mut syn = Syntax::new(lang.clone());
        let job = syn.start_parse(&text);
        let generation = job.generation;
        syn.finish_parse(generation, job.run());
        let spans = highlights(&syn, &text, 0..text.len_bytes(), &mut QueryCursor::new());
        let names: Vec<String> = spans.iter().map(|&(_, _, c)| capture_name(c)).collect();
        assert!(names.iter().any(|n| n.starts_with("keyword")), "{names:?}");
        assert!(names.iter().any(|n| n == "string"), "{names:?}");
        // The last capture painted on "fn" (0..2) is keyword-ish
        let fn_kw = spans.iter().rev().find(|&&(s, e, _)| s == 0 && e == 2).unwrap();
        assert!(capture_name(fn_kw.2).starts_with("keyword"));
    }

    /// A parameter's uses look like the parameter (`locals.scm`) — a `let` doesn't, nor does the same name in
    /// another function. Only when the Rust grammar is there.
    #[test]
    fn parameters_are_colored_where_they_are_used() {
        let Ok(lang) = Loader::global().load(spec("rust").unwrap()) else { return };
        let src = "fn f(count: u32) -> u32 {\n    let other = 1;\n    if true { count + other } else { count }\n}\nfn g() { count }\n";
        let text = Rope::from_str(src);
        let mut syn = Syntax::new(lang);
        let job = syn.start_parse(&text);
        let g = job.generation;
        syn.finish_parse(g, job.run());
        // Just the last lines on screen — the definition is above
        let from = src.find("    if").unwrap();
        let spans = highlights(&syn, &text, from..text.len_bytes(), &mut QueryCursor::new());
        let wins = |at: usize, len: usize| {
            spans.iter().rev().find(|&&(s, e, _)| s == at && e == at + len).map(|&(_, _, c)| capture_name(c))
        };
        let uses: Vec<usize> = src.match_indices("count").map(|(i, _)| i).collect();
        assert_eq!(wins(uses[1], 5).as_deref(), Some("variable.parameter"), "in a nested block");
        assert_eq!(wins(uses[2], 5).as_deref(), Some("variable.parameter"));
        assert_ne!(
            wins(uses[3], 5).as_deref(),
            Some("variable.parameter"),
            "g's own name, not f's parameter"
        );
        let other = src.find("other }").unwrap();
        assert_ne!(wins(other, 5).as_deref(), Some("variable.parameter"));
    }

    #[test]
    fn resolves_fence_names_and_shebangs() {
        let name = |n: &str| resolve(n).map(|s| s.name.as_str());
        assert_eq!(name("rust"), Some("rust"));
        assert_eq!(name("rs"), Some("rust"));
        assert_eq!(name("sh"), Some("bash"));
        assert_eq!(name("Python3"), Some("python"));
        assert_eq!(name("text"), None);
        assert_eq!(shebang("#!/usr/bin/env python3").as_deref(), Some("python3"));
        assert_eq!(shebang("#!/bin/bash -e").as_deref(), Some("bash"));
        assert_eq!(shebang("print(1)"), None);
    }

    /// The inside of a markdown code block is colored as that language (only on machines with the grammar).
    #[test]
    fn injects_fenced_code_when_grammars_available() {
        let (Ok(md), Ok(_)) =
            (Loader::global().load(spec("markdown").unwrap()), Loader::global().load(spec("rust").unwrap()))
        else {
            return;
        };
        let src = "# Title\n\nSome **bold** text.\n\n```rust\nfn main() {}\n```\n";
        let text = Rope::from_str(src);
        let mut syn = Syntax::new(md);
        // Everywhere, not just around what was shown (nothing is here)
        syn.covered = None;
        let job = syn.start_parse(&text);
        let generation = job.generation;
        syn.finish_parse(generation, job.run());
        assert!(syn.layers.iter().any(|l| l.lang.name == "rust"), "code block layer");
        let fn_at = src.find("fn main").unwrap();
        let spans = highlights(&syn, &text, 0..text.len_bytes(), &mut QueryCursor::new());
        let on_fn = spans.iter().rev().find(|&&(s, e, _)| s <= fn_at && fn_at < e).unwrap();
        assert!(capture_name(on_fn.2).starts_with("keyword"), "{}", capture_name(on_fn.2));
        // Edits shift the layer: inserting a line at the very top makes fn paint that much later too
        let tx = crate::transaction::Transaction::new(vec![crate::transaction::Change::insert(0, "x\n")]);
        let mut t2 = text.clone();
        let edits = tx.apply(&mut t2, false);
        syn.edit(&edits.iter().map(|e| e.ts).collect::<Vec<_>>());
        let spans = highlights(&syn, &t2, 0..t2.len_bytes(), &mut QueryCursor::new());
        let on_fn = spans.iter().rev().find(|&&(s, e, _)| s <= fn_at + 2 && fn_at + 2 < e).unwrap();
        assert!(capture_name(on_fn.2).starts_with("keyword"));
        if Loader::global().load(spec("markdown.inline").unwrap()).is_ok() {
            let b = src.find("bold").unwrap();
            let on_b = spans.iter().rev().find(|&&(s, e, _)| s <= b + 2 && b + 2 < e).unwrap();
            assert!(capture_name(on_b.2).contains("bold"), "{}", capture_name(on_b.2));
        }
    }

    /// Edits at random, and what's shown at random (one window or two, a few lines): the layers made around
    /// it — the injection query asked only there — are the ones the query asked over the whole tree gives
    /// there, in every language with injections whose grammar is here (nested, combined, template strings).
    /// `SEED=n` runs that one instead.
    #[test]
    fn injected_layers_around_what_is_shown_match_the_whole_querys() {
        match std::env::var("SEED") {
            Ok(s) => injected_layers_match(&[s.parse().unwrap()]),
            Err(_) => injected_layers_match(&[0x2545_f491_4f6c_dd1d]),
        }
    }

    /// Runs where something was found — beside the others, so they don't take longer.
    fn seeds<const N: usize>(n: [u64; N]) -> [u64; N] {
        n.map(|n| n * 104_729 + 7)
    }

    /// Passes over ranges apart give the matches in another order than one pass (svelte, vue).
    #[test]
    fn injected_layers_match_where_passes_come_in_another_order() {
        injected_layers_match(&seeds([23, 36]));
    }

    /// Where errors are recovered from: an edit between two pieces of PHP's HTML layer changes how; PHP's
    /// HTML layer parsed from the last holds one; an empty node moves in one (svelte) — and a macro's
    /// tokens are injected at two depths (rust).
    #[test]
    fn injected_layers_match_where_errors_are() {
        injected_layers_match(&seeds([11, 2, 116]));
    }

    /// A window shown at the end, on the empty last line; an edit coming while a parse runs, before what
    /// it injected around.
    #[test]
    fn injected_layers_match_at_the_end_and_mid_parse() {
        injected_layers_match(&seeds([14, 232]));
    }

    fn injected_layers_match(seeds: &[u64]) {
        let rust = "/// Doc with `code`.\nfn main() {\n    // a comment\n    let v = vec![1, 2, 3];\n    \
                    println!(\"{} {}\", v[0], v[1]); /* block */\n    assert_eq!(v.len(), 3);\n}\n\n\
                    // tail\nfn other() { let s = format!(\"x\"); }\n";
        let md = "# Title\n\nSome *text* and `code`.\n\n```rust\nfn f() {} // c\nlet x = vec![1];\n```\n\n\
                  More **bold** text.\n\n```toml\na = 1\n```\n";
        let html = "<!doctype html>\n<html>\n<head>\n<style>\nbody { color: red; } /* c */\n</style>\n<script>\n\
                    // note\nconst x = `<b>${1}</b>`;\nfunction f() { return /a+b/.test(\"x\"); }\n</script>\n</head>\n\
                    <body style=\"margin: 0\" onclick=\"f()\">\n<!-- comment -->\n<p>Hi <b>there</b></p>\n\
                    <script type=\"module\">import a from \"./a.js\"; a();</script>\n</body>\n</html>\n";
        let vue = "<template>\n  <div :class=\"cls\" @click=\"go(1)\">{{ msg + 1 }}</div>\n</template>\n\
                   <script setup>\n// comment\nconst msg = ref(`hi ${name}`)\n</script>\n\
                   <style scoped>\n.a { color: red }\n</style>\n";
        let svelte = "<script lang=\"ts\">\n  let count: number = 0;\n  // c\n</script>\n\
                      <button on:click={() => count++}>{count} {#if count > 1}many{/if}</button>\n\
                      <style>\n  button { color: red; }\n</style>\n";
        let php = "<html><body>\n<?php\n// note\n$r = preg_match('/a+/', $s);\n$db->query(\"SELECT * FROM t\");\n\
                   echo \"<b>\" . $x . \"</b>\";\n?>\n<script>var a = 1;</script>\n<p><?= $name ?></p>\n</body></html>\n";
        let js = "// note\n/** @param {string} a */\nfunction f(a) {\n  const q = gql`query { a }`;\n\
                  const h = html`<b>${a}</b>`;\n  return /x+/g.test(a) ? `t ${a}` : a;\n}\n";
        let dockerfile = "FROM alpine\n# comment\nRUN apk add curl && \\\n    echo \"hi\" > /x\n\
                          RUN [\"echo\", \"x\"]\nCMD echo $HOME\n";
        let nix = "{ pkgs }:\n{\n  # comment\n  script = ''\n    echo hi\n  '';\n  buildPhase = ''\n    make\n  '';\n\
                   x = pkgs.writeShellScript \"a\" ''\n    ls\n  '';\n}\n";
        let helm = "apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: {{ .Release.Name }}\n  # comment\ndata:\n\
                    {{- range $k, $v := .Values.data }}\n  {{ $k }}: {{ $v | quote }}\n{{- end }}\n  plain: value\n";
        let markup: &[&str] = &[
            "<script>x()</script>",
            "<style>a{b:c}</style>",
            "</script>",
            "<script>",
            "</style>",
            "<!-- c -->",
            "<",
            ">",
            "=\"y\" ",
        ];
        let cases: [(&str, &str, &[&str]); 10] = [
            ("rust", rust, &[]),
            ("markdown", md, &[]),
            ("html", html, markup),
            ("vue", vue, &[markup, &["{{ a }}", ":x=\"y\" ", "@click=\"f()\" "]].concat()),
            ("svelte", svelte, &[markup, &["{x}", "{#if a}", "{/if}", "lang=\"ts\" "]].concat()),
            ("php", php, &[markup, &["<?php ", "?>", "<?= $a ?>", "'", "preg_match('/b/', $x)"]].concat()),
            ("javascript", js, &["html`<i>x</i>`", "${", "/re/", "/** @a */", "gql`{ b }`"]),
            ("dockerfile", dockerfile, &["RUN echo a\n", "\\\n", "&& ls", "$X"]),
            ("nix", nix, &["''", "''\n echo x\n''", "${a}", "writeShellScript \"b\" ''x''"]),
            (
                "helm",
                helm,
                &["{{ .Values.x }}", "{{- if .a }}", "{{- end }}", "{{/* c */}}", ": ", "\n  ", "- item\n"],
            ),
        ];
        let common = [
            "x",
            " ",
            "\n",
            "// note\n",
            "/// doc `a`\n",
            "/* b */",
            "println!(\"{}\", a);\n",
            "vec![1, 2]",
            "\"",
            "{",
            "}",
            "(",
            ")",
            "`",
            "```rust\nfn g() {}\n```\n",
            "*",
            "# H\n",
            "é",
            "TODO: ",
            "https://x.y ",
        ];
        let describe = |layers: &[Layer]| -> Vec<String> {
            layers
                .iter()
                .map(|l| {
                    let r: Vec<_> = l
                        .tree
                        .included_ranges()
                        .iter()
                        .map(|r| (r.start_byte, r.end_byte, r.start_point, r.end_point))
                        .collect();
                    format!("{} {r:?} {}", l.lang.name, l.tree.root_node().to_sexp())
                })
                .collect()
        };
        let mut tested = Vec::new();
        let mut cut = 0;
        let mut redone_in: HashMap<&str, usize> = HashMap::new();
        for &(mut seed) in seeds {
            let mut rnd = |n: usize| {
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                (seed % n as u64) as usize
            };
            for (lang, src, own) in cases {
                let Ok(data) = Loader::global().load(spec(lang).unwrap()) else { continue };
                let snippets: Vec<&str> = common.iter().chain(own.iter()).copied().collect();
                let mut text = Rope::from_str(src);
                let mut syn = Syntax::new(data);
                syn.inject_budget = std::time::Duration::MAX;
                let mut layered = false;
                let edit_once = |text: &mut Rope, syn: &mut Syntax, rnd: &mut dyn FnMut(usize) -> usize| {
                    let len = text.len_chars();
                    let at = text.char_to_byte(rnd(len + 1));
                    let change = if rnd(3) == 0 && at < text.len_bytes() {
                        let to = text.char_to_byte((text.byte_to_char(at) + 1 + rnd(6)).min(len));
                        crate::transaction::Change::delete(at, to)
                    } else {
                        crate::transaction::Change::insert(at, snippets[rnd(snippets.len())])
                    };
                    let edits = crate::transaction::Transaction::new(vec![change]).apply(text, false);
                    syn.edit(&edits.iter().map(|e| e.ts).collect::<Vec<_>>());
                };
                let mut snapshots: Vec<(Rope, Option<Trees>)> = Vec::new();
                let (mut reused, mut kept, mut redone) = (0, 0, 0);
                for step in 0..300 {
                    for _ in 0..rnd(3) {
                        if rnd(5) == 0 {
                            snapshots.push((text.clone(), syn.trees()));
                        }
                        edit_once(&mut text, &mut syn, &mut rnd);
                    }
                    if rnd(8) == 0 && !snapshots.is_empty() {
                        let (t, trees) = snapshots.swap_remove(rnd(snapshots.len()));
                        text = t;
                        syn.reset(trees);
                    }
                    // What's shown: a window or two of a few lines, a line or none around them
                    syn.near_lines = rnd(2);
                    for _ in 0..1 + rnd(2) {
                        let lines = text.len_lines();
                        let first = rnd(lines);
                        let last = (first + rnd(4)).min(lines);
                        let range = text.line_to_byte(first)..text.line_to_byte(last);
                        highlights(&syn, &text, range, &mut QueryCursor::new());
                    }
                    if !syn.dirty && !syn.wants_injections() {
                        continue;
                    }
                    let job = syn.start_parse(&text);
                    let generation = job.generation;
                    let parsed = job.run();
                    reused += parsed.reused.0;
                    kept += parsed.reused.1;
                    redone += parsed.reused.2;
                    if rnd(4) == 0 {
                        edit_once(&mut text, &mut syn, &mut rnd);
                    }
                    syn.finish_parse(generation, parsed);
                    if syn.dirty {
                        let job = syn.start_parse(&text);
                        let generation = job.generation;
                        let parsed = job.run();
                        reused += parsed.reused.0;
                        kept += parsed.reused.1;
                        redone += parsed.reused.2;
                        syn.finish_parse(generation, parsed);
                    }
                    let covered = syn.covered.clone().unwrap();
                    let made_fresh = |near: Option<&[std::ops::Range<usize>]>, whole_query: bool| {
                        let mut inj =
                            Injector { whole_query, ..Injector::new(&text, near, std::time::Duration::MAX) };
                        let found = inj.found(&syn.lang, syn.tree.as_ref().unwrap(), None);
                        let mut out = Vec::new();
                        inj.run(&found, 0, &mut out);
                        out
                    };
                    // The matches kept from parse to parse are the ones asking afresh gives — the document's
                    // and each layer's
                    let keys = |found: &[Match]| {
                        let mut k: Vec<_> = found.iter().map(|m| (m.pattern, m.captures.clone())).collect();
                        k.sort();
                        k
                    };
                    let mut inj = Injector::new(&text, Some(&covered), std::time::Duration::MAX);
                    let fresh = inj.found(&syn.lang, syn.tree.as_ref().unwrap(), None);
                    assert!(keys(syn.found.as_ref().unwrap()) == keys(&fresh), "{lang} step {step}: matches");
                    for l in syn.layers.iter() {
                        let fresh = inj.layer_found(&l.lang, &l.tree, None, l.depth);
                        assert!(
                            keys(&l.found) == keys(&fresh),
                            "{lang} step {step}: {}'s matches",
                            l.lang.name
                        );
                    }
                    let whole = made_fresh(Some(&covered), true);
                    assert_eq!(
                        describe(&syn.layers),
                        describe(&whole),
                        "{lang} step {step} around {covered:?}:\n{text}"
                    );
                    layered |= !whole.is_empty();
                    let all = made_fresh(None, false);
                    cut += all.len() - whole.len();
                }
                assert!(layered, "{lang}: never injected anything — the test text doesn't exercise it");
                // Else it would only check fresh layers
                assert!(reused > 10, "{lang}: only {reused} layers reused");
                assert!(kept > 0, "{lang}: no match kept");
                *redone_in.entry(lang).or_default() += redone;
                tested.push(lang);
            }
        }
        if !tested.is_empty() {
            assert!(cut > 100, "layers left out for being away from what's shown: {cut} ({tested:?})");
        }
        // Over all the runs: PHP's HTML layer (`FROM_LAST`) was parsed from the last
        if std::env::var("SEED").is_err() && tested.contains(&"php") {
            assert!(redone_in["php"] > 0, "no layer parsed from the last");
        }
        eprintln!("tested in {tested:?}");
    }

    /// A Rust text with a comment on each of `lines` lines.
    fn commented(lines: usize) -> Rope {
        Rope::from_str(&(0..lines).map(|i| format!("let x{i} = {i}; // c{i}\n")).collect::<String>())
    }

    fn parse_now(syn: &mut Syntax, text: &Rope) {
        let job = syn.start_parse(text);
        let generation = job.generation;
        syn.finish_parse(generation, job.run());
    }

    /// The lines with an injected comment layer.
    fn comment_lines(syn: &Syntax, text: &Rope) -> Vec<usize> {
        let comments = syn.layers.iter().filter(|l| l.lang.name == "comment");
        comments.map(|l| text.byte_to_line(l.tree.root_node().start_byte())).collect()
    }

    /// A language with nothing to inject doesn't parse again for being scrolled.
    #[test]
    fn a_language_without_injections_never_wants_them() {
        let Ok(comment) = Loader::global().load(spec("comment").unwrap()) else { return };
        assert!(comment.injections.is_none());
        let text = Rope::from_str(&"TODO: a note\n".repeat(1000));
        let mut syn = Syntax::new(comment);
        parse_now(&mut syn, &text);
        highlights(&syn, &text, text.line_to_byte(900)..text.line_to_byte(920), &mut QueryCursor::new());
        assert!(!syn.wants_injections());
    }

    /// Shown again while the parse for it ran: once it lands, nothing more is wanted.
    #[test]
    fn what_the_landing_parse_covers_isnt_wanted_again() {
        let Ok(rust) = Loader::global().load(spec("rust").unwrap()) else { return };
        let text = commented(1000);
        let mut syn = Syntax::new(rust);
        syn.near_lines = 0;
        let shown = text.line_to_byte(500)..text.line_to_byte(520);
        highlights(&syn, &text, shown.clone(), &mut QueryCursor::new());
        let job = syn.start_parse(&text);
        let generation = job.generation;
        let parsed = job.run();
        highlights(&syn, &text, shown.clone(), &mut QueryCursor::new());
        assert!(syn.wants_injections(), "not covered yet");
        syn.finish_parse(generation, parsed);
        assert!(!syn.wants_injections(), "it is now");
        let lines = comment_lines(&syn, &text);
        assert!((500..520).all(|l| lines.contains(&l)), "{lines:?}");
    }

    /// A syntax made whole (a preview) keeps no list of what it was asked for.
    #[test]
    fn a_whole_syntax_keeps_nothing_of_what_was_shown() {
        let Ok(rust) = Loader::global().load(spec("rust").unwrap()) else { return };
        let text = commented(200);
        let syn = Syntax::parsed(rust, &text);
        for i in 0..100 {
            highlights(&syn, &text, text.line_to_byte(i)..text.line_to_byte(i + 20), &mut QueryCursor::new());
        }
        assert!(syn.asked.borrow().is_empty());
        assert!(!syn.wants_injections());
    }

    /// Going back and forth between two places keeps both injected (no parse, no frame without their
    /// colors); parses with nothing new shown don't spread; places move along with edits above them.
    #[test]
    fn recent_places_stay_injected() {
        let Ok(rust) = Loader::global().load(spec("rust").unwrap()) else { return };
        let mut text = commented(2000);
        let mut syn = Syntax::new(rust);
        syn.near_lines = 0;
        let at = |text: &Rope, line: usize| text.line_to_byte(line)..text.line_to_byte(line + 10);
        for line in [100, 1500] {
            highlights(&syn, &text, at(&text, line), &mut QueryCursor::new());
            parse_now(&mut syn, &text);
        }
        highlights(&syn, &text, at(&text, 100), &mut QueryCursor::new());
        assert!(!syn.wants_injections(), "the first place is still injected");
        let lines = comment_lines(&syn, &text);
        assert!(lines.contains(&100) && lines.contains(&1500), "{lines:?}");
        let covered = syn.covered.clone();
        parse_now(&mut syn, &text);
        parse_now(&mut syn, &text);
        assert_eq!(syn.covered, covered, "no wider for parses with nothing new shown");
        // The same place shown again and again takes one of the places kept, not all
        for _ in 0..RECENT {
            highlights(&syn, &text, at(&text, 100), &mut QueryCursor::new());
            parse_now(&mut syn, &text);
        }
        highlights(&syn, &text, at(&text, 1500), &mut QueryCursor::new());
        assert!(!syn.wants_injections(), "the other place is still kept");
        // 300 lines pasted at the top: the places move down with their text
        let paste = crate::transaction::Change::insert(0, commented(300).to_string());
        let edits = crate::transaction::Transaction::new(vec![paste]).apply(&mut text, false);
        syn.edit(&edits.iter().map(|e| e.ts).collect::<Vec<_>>());
        parse_now(&mut syn, &text);
        highlights(&syn, &text, at(&text, 1800), &mut QueryCursor::new());
        assert!(!syn.wants_injections(), "the place moved along");
        assert!(comment_lines(&syn, &text).contains(&1800));
    }

    /// Around what's shown: whole lines, so many more each way, merged; and a layer touching it meets it.
    #[test]
    #[allow(clippy::single_range_in_vec_init)] // lists of ranges that happen to have one
    fn around_what_is_shown_is_whole_lines() {
        let text = Rope::from_str("a\nbb\nccc\ndddd\neeeee\n");
        // "bb" (2..4) and "dddd" (9..13), a line more each way: lines 0–4 and 2–5, one range
        assert_eq!(around(&text, vec![9..10, 2..3], 1), [0..20]);
        assert_eq!(around(&text, vec![2..3], 0), [2..5]);
        assert_eq!(around(&text, vec![2..3, 14..15], 0), [2..5, 14..20]);
        let near = [10..20];
        assert!(meets(&near, 5, 10) && meets(&near, 20, 25), "touching counts");
        assert!(!meets(&near, 5, 9) && !meets(&near, 21, 25));
    }

    /// A language whose combined injections can make a layer is asked everywhere; Rust's only one goes into
    /// `markdown-rustdoc`, which there's no grammar for, so Rust is asked around what's shown.
    #[test]
    fn only_injections_that_can_combine_are_asked_everywhere() {
        let Ok(rust) = Loader::global().load(spec("rust").unwrap()) else { return };
        assert!(resolve("markdown-rustdoc").is_none(), "if there's a grammar for it now, this changes");
        assert!(!combines(rust.injections.as_ref().unwrap()));
        let lang = rust.language.clone();
        let q = |src: &str| Query::new(&lang, src).unwrap();
        assert!(combines(&q(
            "((line_comment) @injection.content (#set! injection.language \"comment\") (#set! injection.combined))"
        )));
        assert!(
            combines(&q("((line_comment) @injection.content (#set! injection.combined))")),
            "a language from the text"
        );
        assert!(!combines(&q("((line_comment) @injection.content (#set! injection.language \"comment\"))")));
    }

    /// Where two patterns catch the same spot, the one listed first wins, however the query was asked —
    /// Svelte's `<script lang="ts">` is TypeScript (listed before plain JavaScript), also when only the
    /// script's inside is shown.
    #[test]
    fn the_pattern_listed_first_wins_a_spot() {
        let (Ok(svelte), Ok(_), Ok(_)) = (
            Loader::global().load(spec("svelte").unwrap()),
            Loader::global().load(spec("typescript").unwrap()),
            Loader::global().load(spec("javascript").unwrap()),
        ) else {
            return;
        };
        let text =
            Rope::from_str("<p>a</p>\n<script lang=\"ts\">\nlet a: number = 1;\n</script>\n<p>b</p>\n");
        for shown in [0..text.len_bytes(), 30..40] {
            let mut syn = Syntax::new(svelte.clone());
            syn.near_lines = 0;
            highlights(&syn, &text, shown.clone(), &mut QueryCursor::new());
            let job = syn.start_parse(&text);
            let generation = job.generation;
            syn.finish_parse(generation, job.run());
            let langs: Vec<_> = syn.layers.iter().map(|l| l.lang.name.as_str()).collect();
            assert_eq!(langs, ["typescript"], "shown {shown:?}");
        }
    }

    /// Built-in queries mesh with every downloaded grammar (catches queries broken by a grammar bump).
    /// Missing grammars are skipped.
    #[test]
    fn every_installed_grammar_loads_with_its_queries() {
        let mut checked = 0;
        for spec in specs() {
            let installed = crate::runtime::builtin_grammar(&spec.grammar).is_some()
                || crate::runtime::find(format!("grammars/{}.so", spec.grammar)).is_some();
            if !installed {
                continue;
            }
            match Loader::global().load(spec) {
                Err(e) => panic!("{}: {e}", spec.name),
                // A locals.scm or indents.scm that doesn't compile would silently do nothing
                Ok(l) => {
                    assert!(
                        l.locals.is_some() || read_query(&spec.name, "locals.scm", 0).is_none(),
                        "{}: locals.scm doesn't compile against the grammar",
                        spec.name
                    );
                    assert!(
                        l.indents.is_some() || read_query(&spec.name, "indents.scm", 0).is_none(),
                        "{}: indents.scm doesn't compile against the grammar",
                        spec.name
                    );
                }
            }
            checked += 1;
        }
        eprintln!("{checked} languages checked");
    }

    /// Helm templates: `{{ }}` in Go template colors, text between is a YAML layer (not HTML) — templates
    /// inside quotes aren't covered by the YAML string.
    #[test]
    fn helm_templates_color_yaml_between_actions() {
        // Skip if a build without bundled grammars hasn't downloaded it yet
        let (Ok(lang), Ok(_)) =
            (Loader::global().load(spec("helm").unwrap()), Loader::global().load(spec("yaml").unwrap()))
        else {
            return;
        };
        let src = "kind: Deployment\nimage: \"{{ .Values.tag }}\"\n";
        let text = Rope::from_str(src);
        let mut syn = Syntax::new(lang);
        syn.covered = None; // everywhere, not just around what was shown (nothing is here)
        let job = syn.start_parse(&text);
        let g = job.generation;
        syn.finish_parse(g, job.run());
        assert_eq!(syn.layers.iter().map(|l| l.lang.name.as_str()).collect::<Vec<_>>(), ["yaml"]);
        let spans = highlights(&syn, &text, 0..text.len_bytes(), &mut QueryCursor::new());
        let top = |at: usize| {
            spans.iter().rev().find(|&&(s, e, _)| s <= at && at < e).map(|&(_, _, c)| capture_name(c))
        };
        assert!(top(src.find("Deployment").unwrap()).is_some_and(|c| c.starts_with("string")), "YAML value");
        let values = src.find("Values").unwrap();
        assert!(!top(values).unwrap_or_default().starts_with("string"), "in quotes: template color");
    }

    /// Editing a template action moves the YAML after it along its line, and the YAML layer is kept: the
    /// lexer counts a column from the included text only, so `c` stays nested under `b` however long the
    /// action before `b` is.
    #[test]
    fn a_layer_moved_along_its_line_is_kept() {
        let (Ok(lang), Ok(_)) =
            (Loader::global().load(spec("helm").unwrap()), Loader::global().load(spec("yaml").unwrap()))
        else {
            return;
        };
        let yaml = |syn: &Syntax| -> Vec<String> {
            syn.layers
                .iter()
                .filter(|l| l.lang.name == "yaml")
                .map(|l| l.tree.root_node().to_sexp())
                .collect()
        };
        let mut text = Rope::from_str("{{ .a }}b:\n         c: 1\n");
        let mut syn = Syntax::new(lang.clone());
        syn.covered = None;
        parse_now(&mut syn, &text);
        // `b` from column 8 to 10 — past `c`'s 9, were columns counted in the whole line
        let tx = crate::transaction::Transaction::new(vec![crate::transaction::Change::insert(4, "bc")]);
        let edits = tx.apply(&mut text, false);
        syn.edit(&edits.iter().map(|e| e.ts).collect::<Vec<_>>());
        let job = syn.start_parse(&text);
        let generation = job.generation;
        let parsed = job.run();
        assert_eq!(parsed.reused.0, 1);
        syn.finish_parse(generation, parsed);
        let fresh = yaml(&Syntax::parsed(lang, &text));
        assert_eq!(yaml(&syn), fresh);
        assert_eq!(fresh[0].matches("(block_mapping ").count(), 2, "c nested under b: {fresh:?}");
    }

    /// Layers by language and pieces.
    type Described = Vec<(String, Vec<(usize, usize)>)>;

    /// `src` parsed, then `change` made and parsed again from that: its layers (language, pieces), a fresh
    /// parse's, and how many layers were parsed from the last — `None` if a grammar in `langs` isn't here.
    fn layers_after(
        langs: &[&str],
        src: &str,
        change: crate::transaction::Change,
    ) -> Option<(Described, Described, usize)> {
        for l in langs {
            Loader::global().load(spec(l)?).ok()?;
        }
        let lang = Loader::global().load(spec(langs[0])?).ok()?;
        let describe = |syn: &Syntax| {
            let ranges =
                |l: &Layer| l.tree.included_ranges().iter().map(|r| (r.start_byte, r.end_byte)).collect();
            syn.layers.iter().map(|l| (l.lang.name.clone(), ranges(l))).collect::<Vec<_>>()
        };
        let mut text = Rope::from_str(src);
        let mut syn = Syntax::new(lang.clone());
        syn.covered = None;
        parse_now(&mut syn, &text);
        let edits = crate::transaction::Transaction::new(vec![change]).apply(&mut text, false);
        syn.edit(&edits.iter().map(|e| e.ts).collect::<Vec<_>>());
        let job = syn.start_parse(&text);
        let generation = job.generation;
        let parsed = job.run();
        let redone = parsed.reused.2;
        syn.finish_parse(generation, parsed);
        Some((describe(&syn), describe(&Syntax::parsed(lang, &text)), redone))
    }

    /// An argument typed before the string in `$db->query(…)` — clear of all the match captures, the
    /// method's name and the string — and the SQL is gone: the query wants it as the first argument. Only
    /// asking the last tree where the edit was finds that match to drop.
    #[test]
    fn a_match_broken_between_what_it_captures_is_dropped() {
        let src = "<?php\n$db->query(  \"SELECT 1\");\n";
        let at = src.find('(').unwrap() + 2;
        let change = crate::transaction::Change::insert(at, "$x, ");
        let Some((kept, fresh, _)) = layers_after(&["php", "sql"], src, change) else { return };
        assert!(!fresh.iter().any(|(l, _)| l == "sql"), "{fresh:?}");
        assert_eq!(kept, fresh);
    }

    /// PHP's HTML layer starting after `<?php … ?>`, a word typed in its text: it's parsed from the last
    /// one, which first goes where the new one starts (both parsed as if at the document's start) — so
    /// what the edit didn't reach is taken over: the list's tokens after it are the last tree's own.
    #[test]
    fn a_layer_parsed_from_the_last_takes_over_what_the_edit_missed() {
        let (Ok(lang), Ok(_)) =
            (Loader::global().load(spec("php").unwrap()), Loader::global().load(spec("html").unwrap()))
        else {
            return;
        };
        let src = "<?php $a = 1; ?>\n<div>\n<p>one two</p>\n<ul><li>x</li></ul>\n</div>\n";
        let mut text = Rope::from_str(src);
        let mut syn = Syntax::new(lang.clone());
        syn.covered = None;
        parse_now(&mut syn, &text);
        let edits = crate::transaction::Transaction::new(vec![crate::transaction::Change::insert(
            src.find("two").unwrap(),
            "and ",
        )])
        .apply(&mut text, false);
        syn.edit(&edits.iter().map(|e| e.ts).collect::<Vec<_>>());
        let html = |syn: &Syntax| syn.layers.iter().find(|l| l.lang.name == "html").unwrap().tree.clone();
        // `ul` in `<ul>`
        let list = |tree: &Tree| {
            let at = text.to_string().find("<ul>").unwrap() + 1;
            let n = tree.root_node().descendant_for_byte_range(at, at + 2).unwrap();
            assert_eq!(n.kind(), "tag_name");
            n.id()
        };
        let before = html(&syn);
        assert!(before.included_ranges()[0].start_byte > 0);
        let job = syn.start_parse(&text);
        let generation = job.generation;
        let parsed = job.run();
        assert_eq!(parsed.reused.2, 1);
        syn.finish_parse(generation, parsed);
        assert_eq!(list(&html(&syn)), list(&before), "the list taken over");
        let fresh = Syntax::parsed(lang, &text);
        assert_eq!(html(&syn).root_node().to_sexp(), html(&fresh).root_node().to_sexp());
    }

    /// JavaScript's `// a`, `/* b */// c` on the next line: the line break and the slash deleted, it's one
    /// comment — of the same kind as all three, so the tree's changed ranges are empty, and the layers
    /// for the other two mustn't stay.
    #[test]
    fn comments_merged_into_one_leave_one_layer() {
        let src = "// a\n/* b */// c\nlet x = 1;\n";
        let change = crate::transaction::Change::delete(4, 6);
        let Some((kept, fresh, _)) = layers_after(&["javascript", "comment"], src, change) else { return };
        assert_eq!(fresh.iter().filter(|(l, _)| l == "comment").count(), 1, "{fresh:?}");
        assert_eq!(kept, fresh);
    }

    /// Where two patterns catch the same spot, the one listed first wins whatever order the matches come in
    /// — kept ones come before those asked anew, and passes over ranges apart in their own order.
    #[test]
    fn the_pattern_listed_first_wins_a_spot_in_any_order() {
        let (Ok(lang), Ok(_)) = (
            Loader::global().load(spec("svelte").unwrap()),
            Loader::global().load(spec("typescript").unwrap()),
        ) else {
            return;
        };
        let text = Rope::from_str("<script lang=\"ts\">\nlet a: number = 1;\n</script>\n");
        let syn = Syntax::parsed(lang.clone(), &text);
        let mut inj = Injector::new(&text, None, std::time::Duration::MAX);
        let mut found = inj.found(&lang, syn.tree.as_ref().unwrap(), None);
        let langs: Vec<&str> = found.iter().map(|m| m.lang.as_str()).collect();
        assert!(
            langs.contains(&"typescript") && langs.contains(&"javascript"),
            "both catch the script: {langs:?}"
        );
        for _ in 0..2 {
            let mut layers = Vec::new();
            inj.run(&found, 0, &mut layers);
            assert_eq!(layers[0].lang.name, "typescript");
            found.reverse();
        }
    }
}
