//! Code navigation for the chat (N5) — tarae's `references` and `search` tools, so Claude finds its way
//! through the code the way the user does, through the editor:
//!
//! - `references` asks the file's language server for a name's references (`g r`'s question): each place
//!   with its line of code, by file then line, the declaration (its definition, asked beside them) first
//!   (N2). While the server starts or loads, or when it says nothing within `REFS_WAIT`, the name's
//!   whole-word text matches instead — said so (N3).
//! - `search` runs the project search `space /` runs (`grep.rs` — ripgrep's crates, a regex, smart case):
//!   by file then line, each hit its line of code (and context lines when asked), capped and said so; or
//!   the files, or counts.
//! - `symbols` lists a file's outline (the server's document symbols, else tarae's from the tree).
//! - `definition` (N6) asks a name's definition, implementations or type definition at a place (`g d`,
//!   `g i`, `g y`), or finds it by name (`workspace/symbol`) — each place with its signature. While the
//!   server can't say, the syntax tree's definitions among the name's text matches, said so.
//! - `hover` gives the server's hover text at a place (type, signature, docs), trimmed.
//! - `diagnostics` says what the servers have published for some files now — and whether they've checked
//!   them yet or are still at it — optionally after waiting (bounded) for them to settle.
//!
//! All only read, inside the project (`Editor::root`) — no card. Disk and searching go to workers.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::editor::Editor;
use crate::grep::Case;
use crate::lsp::ClientId;
use crate::lsp_editor::{Goto, Kind, Place};
use crate::tools::{Call, answer};

/// How long a server has to answer before the text matches are the answer — tests too: one that answers
/// mustn't race it (300 ms lost to a slow runner's stalls); one about silence sets a short one
/// (`LspState.refs_wait`).
const REFS_WAIT: std::time::Duration = std::time::Duration::from_secs(20);

/// `REFS_WAIT`, or the one a test set.
fn refs_wait(ed: &Editor) -> std::time::Duration {
    ed.lsp.refs_wait.unwrap_or(REFS_WAIT)
}

/// Rows of places a `references` answer lists at most (the rest summed up per file).
const REFS_SHOWN: usize = 200;

/// `search`: hits listed at most, rows with their context at most, files at most (files/count), and where
/// the search is stopped (the count then says `10000+`).
const HITS_SHOWN: usize = 200;
const ROWS_SHOWN: usize = 400;
const FILES_SHOWN: usize = 300;
const HITS_MAX: usize = 10_000;
const CONTEXT_MAX: u64 = 10;

/// A line's text in an answer, at most (chars).
const LINE_MAX: usize = 240;

/// Files named in the "more not shown" line, at most.
const MORE_NAMED: usize = 12;

/// Over the hits in test code (`testcode.rs`), listed after the rest.
const TESTS_HEADING: &str = "in test code:\n";

/// The `tests` option both lookups take.
fn tests_schema() -> Value {
    json!({ "type": "boolean", "description": "Include hits in test code (default true) — they're listed apart, \
        after the rest. false leaves them out (the count of them is still said)" })
}

/// What's said of the test hits after a count: listed last, or left out.
fn tests_said(n: usize, included: bool) -> String {
    match (n, included) {
        (0, _) => String::new(),
        (n, true) => format!("; {n} in test code, listed last"),
        (n, false) => format!("; {n} more in test code, left out"),
    }
}

// ── references ──────────────────────────────────────────────────────────────

pub fn references_schema() -> Value {
    json!({
        "name": "references",
        "description": "Find where a function, type, method, field or variable is used — the language \
    server's references, each with its line of code, by file, the declaration first. Semantic, not text: \
    no same-named strangers, no misses through imports or aliases. Reach for it over Grep and search whenever \
    the question is who calls or uses something, or what a change to it would touch. Give the file and line \
    where the name is written (its declaration or any use) and the name. While the language server is still \
    loading, the answer is the name's whole-word text matches instead, and says so — ask again later for the \
    server's.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "A file where the name is written, relative to the project root" },
                "line": { "type": "integer", "description": "The line it's on, 1-based — left out: its first appearance in the file" },
                "symbol": { "type": "string", "description": "The name as written in the code (`goto_ask`; `Editor::goto_ask` or `x.goto_ask` mean the same)" },
                "tests": tests_schema()
            },
            "required": ["path", "symbol"]
        }
    })
}

/// Places a server named: path, line, column (from 0; the column in its encoding).
type Found = Vec<(PathBuf, usize, usize)>;

/// A `references` question asked of a server: what's come back so far.
pub struct Ask {
    call: Call,
    word: String,
    /// Test code included (listed last) or left out.
    tests: bool,
    server: String,
    cid: ClientId,
    /// The server's references (Err: why it gave none) · where it's declared (none if it can't say) —
    /// None until each answers.
    refs: Option<Result<Found, String>>,
    declared: Option<Vec<(PathBuf, usize)>>,
}

/// The tool: the file checked on a worker (in the project, there), then asked about (`ask`). Err =
/// answered now.
pub fn references(ed: &mut Editor, call: Call, args: &Value) -> Result<(), String> {
    let path = args["path"].as_str().map(str::trim).filter(|p| !p.is_empty()).ok_or("path is required")?;
    let word = name_of(args["symbol"].as_str().unwrap_or_default())?;
    let line = args["line"].as_u64().map(|l| l.max(1) as usize - 1);
    let tests = args["tests"].as_bool().unwrap_or(true);
    let root = project_root(ed);
    let path = root.join(path);
    ed.events.jobs().spawn(move || {
        let file = file_in(&root, &path);
        move |ed: &mut Editor| match file {
            Ok(path) => ask(ed, call, path, line, Name { word, tests }),
            Err(e) => answer(ed, call, Err(e)),
        }
    });
    Ok(())
}

/// What a `references` call asks about: the name, and whether test code is wanted.
struct Name {
    word: String,
    tests: bool,
}

/// The name in `symbol`: its last part (`Editor::goto_ask`, `x.goto_ask()` → `goto_ask`), a word.
fn name_of(symbol: &str) -> Result<String, String> {
    let s = symbol.trim().trim_end_matches("()");
    let last = s.rsplit([':', '.']).find(|p| !p.is_empty()).unwrap_or_default();
    let word = |c: char| c.is_alphanumeric() || c == '_';
    if !last.is_empty() && last.chars().all(word) && !last.starts_with(|c: char| c.is_ascii_digit()) {
        Ok(last.to_string())
    } else if symbol.trim().is_empty() {
        Err("symbol is required".into())
    } else {
        Err(format!("symbol is a name as written in the code (`goto_ask`) — not `{symbol}`"))
    }
}

/// `path`, real, if it's a file in the project — on a worker.
pub(crate) fn file_in(root: &Path, path: &Path) -> Result<PathBuf, String> {
    let shown = rel(root, path);
    let real = std::fs::canonicalize(path).map_err(|e| format!("{shown}: {e}"))?;
    let root = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
    if !real.starts_with(&root) {
        return Err(format!("{shown} is outside the project — these tools look inside it only"));
    }
    if !real.is_file() {
        return Err(format!("{shown} isn't a file"));
    }
    Ok(real)
}

/// Where `word` is in `text` — on `line` if given, else its first line with it: (line, byte column), the
/// first whole-word match.
fn locate(
    text: &ropey::Rope,
    word: &str,
    line: Option<usize>,
    shown: &str,
) -> Result<(usize, usize), String> {
    let re = regex::Regex::new(&format!(r"\b{}\b", regex::escape(word))).map_err(|e| e.to_string())?;
    let find = |l: usize| re.find(&text.line(l).to_string()).map(|m| (l, m.start()));
    let lines = crate::movement::last_line(text) + 1;
    match line {
        Some(l) if l >= lines => Err(format!("{shown} has {lines} lines — no line {}", l + 1)),
        Some(l) => find(l).ok_or_else(|| {
            let reads = text.line(l).to_string();
            format!(
                "`{word}` isn't on line {} of {shown} — it reads `{}`. Give the line where the name is written, \
                 or leave line out for its first appearance in the file",
                l + 1,
                clip(reads.trim(), 120)
            )
        }),
        // (its first line in code: a module's doc comment naming it came first, and hover found nothing
        // there — a comment's only when that's all there is)
        None => {
            let mut first = None;
            for l in 0..lines {
                if let Some(at) = find(l) {
                    if !commented(&text.line(l).to_string()) {
                        return Ok(at);
                    }
                    first.get_or_insert(at);
                }
            }
            first.ok_or_else(|| format!("`{word}` isn't in {shown}"))
        }
    }
}

/// Whether a line is a comment's, by how it starts (`//`, `/*`, `*`, `--`, `# ` — not `#[`).
fn commented(line: &str) -> bool {
    let t = line.trim_start();
    ["//", "/*", "*", "--"].iter().any(|m| t.starts_with(m))
        || t.strip_prefix('#').is_some_and(|r| r.is_empty() || r.starts_with(char::is_whitespace))
}

/// On the main loop: the file's buffer (opened aside when it isn't — its server starts with it; the
/// file in view stays), the name found in it, the question sent — or the text matches when the server
/// can't be asked now.
fn ask(ed: &mut Editor, call: Call, path: PathBuf, line: Option<usize>, name: Name) {
    let shown = rel(&project_root(ed), &path);
    let id = match buffer_for(ed, &path) {
        Ok(id) => id,
        Err(e) => return answer(ed, call, Err(format!("{shown}: {e}"))),
    };
    let Some(doc) = ed.docs.iter().find(|d| d.id == id) else { return };
    if doc.loading {
        return text_matches(ed, call, name, format!("{shown} is still loading"), true);
    }
    let (line, col) = match locate(&doc.text, &name.word, line, &shown) {
        Ok(at) => at,
        Err(e) => return answer(ed, call, Err(e)),
    };
    let byte = doc.text.line_to_byte(line) + col;
    let n = ed.lsp.tool_ask_next;
    ed.lsp.tool_ask_next += 1;
    match ed.ask_references(id, byte, n) {
        Err((why, later)) => text_matches(ed, call, name, why, later),
        Ok((cid, server, declares)) => {
            let declared = (!declares).then(Vec::new);
            let Name { word, tests } = name;
            let ask = Ask { call, word, tests, server, cid, refs: None, declared };
            ed.lsp.tool_asks.insert(n, ask);
            // A server that says nothing in time: the text matches are the answer
            let wait = refs_wait(ed);
            ed.events.jobs().spawn(move || {
                std::thread::sleep(wait);
                move |ed: &mut Editor| {
                    if let Some(a) = ed.lsp.tool_asks.remove(&n) {
                        let why = format!("{} didn't answer within {}", a.server, secs(wait));
                        text_matches(ed, a.call, Name { word: a.word, tests: a.tests }, why, true);
                    }
                }
            });
        }
    }
}

/// `path`'s buffer — opened aside when it isn't (its language server starts with it; the file in view
/// stays in view).
pub(crate) fn buffer_for(ed: &mut Editor, path: &Path) -> Result<crate::document::DocId, String> {
    if let Some(d) = ed.docs.iter().find(|d| d.path.as_deref() == Some(path)) {
        return Ok(d.id);
    }
    let before = ed.current;
    ed.open(path).map_err(|e| format!("{e:#}"))?;
    let id = ed.doc().id;
    ed.current = before.min(ed.docs.len() - 1);
    Ok(id)
}

/// The server's answer to question `n`'s references (Err: its error).
pub fn refs_answered(ed: &mut Editor, n: u64, result: Result<&Value, &Value>) {
    let Some(a) = ed.lsp.tool_asks.get_mut(&n) else { return };
    a.refs = Some(match result {
        Ok(v) => Ok(crate::lsp::locations(v)
            .into_iter()
            .map(|(path, pos)| {
                let at = |k: &str| pos[k].as_u64().unwrap_or(0) as usize;
                (path, at("line"), at("character"))
            })
            .collect()),
        // (content modified / server cancelled: the project changed under the question — it's loading)
        Err(e) if matches!(e["code"].as_i64(), Some(-32801 | -32802)) => {
            Err(format!("{} is still loading the project", a.server))
        }
        Err(e) => Err(format!("{}: {}", a.server, e["message"].as_str().unwrap_or("the request failed"))),
    });
    finish(ed, n);
}

/// …and to its definition (None: an error — no declaration to put first).
pub fn declared_answered(ed: &mut Editor, n: u64, result: Option<&Value>) {
    let Some(a) = ed.lsp.tool_asks.get_mut(&n) else { return };
    let found = result.map(crate::lsp::locations).unwrap_or_default();
    a.declared =
        Some(found.into_iter().map(|(p, pos)| (p, pos["line"].as_u64().unwrap_or(0) as usize)).collect());
    finish(ed, n);
}

/// Both answers in: the places listed with their code (read on a worker) — none from a server still at
/// work, or an error, and it's the text matches.
fn finish(ed: &mut Editor, n: u64) {
    if !ed.lsp.tool_asks.get(&n).is_some_and(|a| a.refs.is_some() && a.declared.is_some()) {
        return;
    }
    let Some(a) = ed.lsp.tool_asks.remove(&n) else { return };
    let (refs, declared) = (a.refs.expect("in"), a.declared.unwrap_or_default());
    let name = Name { word: a.word, tests: a.tests };
    let found = match refs {
        Err(why) => return text_matches(ed, a.call, name, why, true),
        Ok(found) if found.is_empty() => {
            let busy: Vec<String> = ed
                .client(a.cid)
                .map(|c| c.progress.values().map(|p| p.title.clone()).collect())
                .unwrap_or_default();
            let (why, later) = match busy.is_empty() {
                true => (format!("{} found no references there", a.server), false),
                false => {
                    (format!("{} is still loading ({}) and found none yet", a.server, busy.join(", ")), true)
                }
            };
            return text_matches(ed, a.call, name, why, later);
        }
        Ok(mut found) => {
            found.sort();
            found.dedup();
            found
        }
    };
    // The declaration's own line, should the server have left it out of the references
    let mut all = found;
    for (path, line) in &declared {
        if !all.iter().any(|f| f.0 == *path && f.1 == *line) {
            all.push((path.clone(), *line, 0));
        }
    }
    let open: HashMap<PathBuf, ropey::Rope> = ed
        .docs
        .iter()
        .filter_map(|d| Some((d.path.clone()?, d.text.clone())))
        .filter(|(p, _)| all.iter().any(|f| f.0 == *p))
        .collect();
    let root = project_root(ed);
    let (call, server, tests) = (a.call, a.server, name.tests);
    ed.events.jobs().spawn(move || {
        let places = crate::lsp_editor::read_places(all, &open);
        let mut t = crate::testcode::Tests::new(&root);
        let places: Vec<(Place, bool)> = places
            .into_iter()
            .map(|p| {
                let test = t.at(&p.path, p.line);
                (p, test)
            })
            .collect();
        let text = refs_text(&root, places, &declared, &server, tests);
        move |ed: &mut Editor| answer(ed, call, Ok(text))
    });
}

/// The answer: how many, in how many files, and the list — by file then line, the declaration's file
/// first and its row first in it (marked), test code last (or left out, `tests` false).
fn refs_text(
    root: &Path,
    mut places: Vec<(Place, bool)>,
    declared: &[(PathBuf, usize)],
    server: &str,
    tests: bool,
) -> String {
    let (refs, in_tests) = (places.len(), places.iter().filter(|p| p.1).count());
    if !tests {
        places.retain(|p| !p.1);
    }
    let files = places.iter().map(|p| &p.0.path).collect::<std::collections::HashSet<_>>().len();
    let is_decl = |p: &Place| declared.iter().any(|(path, line)| *path == p.path && *line == p.line);
    let decl_file = places.iter().find(|p| is_decl(&p.0)).map(|p| p.0.path.clone());
    places.sort_by(|(a, at), (b, bt)| {
        let key = |p: &Place, test: bool| {
            (test, Some(&p.path) != decl_file.as_ref(), p.path.clone(), !is_decl(p), p.line, p.col)
        };
        key(a, *at).cmp(&key(b, *bt))
    });
    let shown = if tests { refs } else { refs - in_tests };
    let first = if decl_file.is_some() { ", the declaration first" } else { "" };
    let head = format!(
        "{} in {} ({server}){first}{}:",
        count(shown, "reference"),
        count(files, "file"),
        tests_said(in_tests, tests)
    );
    if places.is_empty() {
        return head.trim_end_matches(':').to_string();
    }
    let mark = |p: &Place| is_decl(p).then(|| "declaration".to_string());
    format!("{head}\n{}", grouped(root, places, &mark, REFS_SHOWN))
}

/// The name's whole-word text matches in the project (case as written), as the answer — `why` the
/// server's references aren't (`later`: they will be, ask again).
fn text_matches(ed: &mut Editor, call: Call, name: Name, why: String, later: bool) {
    let root = project_root(ed);
    let Name { word, tests } = name;
    ed.events.jobs().spawn(move || {
        let r = crate::picker::word_matches(&root, &word).map(|items| {
            let mut t = crate::testcode::Tests::new(&root);
            let mut places: Vec<(Place, bool)> = items
                .into_iter()
                .filter_map(|it| match it.action {
                    crate::picker::Action::Goto { path, line, col } => {
                        let text = it.label.split_once(&format!(":{}: ", line + 1)).map(|(_, t)| t);
                        let test = t.at(&path, line);
                        Some((
                            Place { text: clip(text.unwrap_or_default(), LINE_MAX), path, line, col },
                            test,
                        ))
                    }
                    _ => None,
                })
                .collect();
            let n = places.len();
            let in_tests = places.iter().filter(|p| p.1).count();
            if !tests {
                places.retain(|p| !p.1);
            }
            // (by file then line, test code last)
            places.sort_by(|(a, at), (b, bt)| (at, &a.path, a.line).cmp(&(bt, &b.path, b.line)));
            let files = places.iter().map(|p| &p.0.path).collect::<std::collections::HashSet<_>>().len();
            let then = if later { " — ask again in a moment for those" } else { "" };
            if n == 0 {
                return format!("no text matches of `{word}` in the project either — {why}");
            }
            format!(
                "{} in {} — {why}, so these are the whole-word text matches of `{word}` (case as written), \
                 not its references{then}{}:\n{}",
                count(if tests { n } else { n - in_tests }, "text match"),
                count(files, "file"),
                tests_said(in_tests, tests),
                grouped(&root, places, &|_| None, REFS_SHOWN)
            )
        });
        move |ed: &mut Editor| answer(ed, call, r)
    });
}

/// Places in order — each with whether it's test code, those last — a file's name then its lines
/// (`  12: code`, numbers right-aligned in the file), test code under its own heading; one row a line; past
/// `max` rows the rest summed up per file.
fn grouped(
    root: &Path,
    places: Vec<(Place, bool)>,
    mark: &dyn Fn(&Place) -> Option<String>,
    max: usize,
) -> String {
    let mut files: Vec<(PathBuf, bool, Vec<Place>)> = Vec::new();
    for (p, test) in places {
        match files.last_mut() {
            Some((f, t, rows)) if *f == p.path && *t == test => {
                if rows.last().is_none_or(|r| r.line != p.line) {
                    rows.push(p);
                }
            }
            _ => files.push((p.path.clone(), test, vec![p])),
        }
    }
    let mut out = String::new();
    let mut shown = 0;
    let mut rest = Vec::new();
    let mut in_tests = false;
    for (path, test, rows) in files {
        let name = rel(root, &path);
        let take = rows.len().min(max - shown);
        if take > 0 {
            if test && !in_tests {
                in_tests = true;
                out.push_str(TESTS_HEADING);
            }
            let w = rows[..take].iter().map(|r| r.line + 1).max().unwrap_or(1).to_string().len();
            out.push_str(&name);
            out.push('\n');
            for r in &rows[..take] {
                let m = mark(r).map(|m| format!("  ({m})")).unwrap_or_default();
                out.push_str(&format!("  {:>w$}: {}{m}\n", r.line + 1, r.text));
            }
            shown += take;
        }
        if take < rows.len() {
            rest.push((name, rows.len() - take));
        }
    }
    if !rest.is_empty() {
        out.push_str(&more(&rest, "line", ""));
    }
    out.truncate(out.trim_end().len());
    out
}

// ── symbols ─────────────────────────────────────────────────────────────────

pub fn symbols_schema() -> Value {
    json!({
        "name": "symbols",
        "description": "A file's outline — its modules, types, impls, functions, methods, fields … nested, \
    each with the lines it spans — from the language server (tarae's own outline from the syntax tree when \
    there's none yet). With line: only the symbols that hold that line, outermost first — which function, \
    impl or test module a line is in. Use it to find your way in a file before reading it, and to place a \
    line a search or references answer gave.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The file, relative to the project root" },
                "line": { "type": "integer", "description": "A line, 1-based — the symbols holding it; left out: the whole outline" }
            },
            "required": ["path"]
        }
    })
}

/// Symbols an answer lists at most (the whole outline — ask with a line for the part you need).
const SYMS_SHOWN: usize = 300;

/// One symbol: how deep it sits, what kind it is (`fn`, `struct` — empty when its name says it: `impl X`),
/// its name, its first and last line (from 0).
#[derive(Clone, Debug, PartialEq)]
struct Sym {
    depth: usize,
    kind: &'static str,
    name: String,
    lines: (usize, usize),
}

/// A `symbols` question asked of a server.
pub struct SymAsk {
    call: Call,
    path: PathBuf,
    line: Option<usize>,
    server: String,
    cid: ClientId,
}

/// The tool: the file checked on a worker (in the project, there), then its server asked (`ask_symbols`) —
/// or the outline. Err = answered now.
pub fn symbols(ed: &mut Editor, call: Call, args: &Value) -> Result<(), String> {
    let path = args["path"].as_str().map(str::trim).filter(|p| !p.is_empty()).ok_or("path is required")?;
    let line = args["line"].as_u64().map(|l| l.max(1) as usize - 1);
    let root = project_root(ed);
    let path = root.join(path);
    ed.events.jobs().spawn(move || {
        let file = file_in(&root, &path);
        move |ed: &mut Editor| match file {
            Ok(path) => ask_symbols(ed, call, path, line),
            Err(e) => answer(ed, call, Err(e)),
        }
    });
    Ok(())
}

fn ask_symbols(ed: &mut Editor, call: Call, path: PathBuf, line: Option<usize>) {
    let shown = rel(&project_root(ed), &path);
    let id = match buffer_for(ed, &path) {
        Ok(id) => id,
        Err(e) => return answer(ed, call, Err(format!("{shown}: {e}"))),
    };
    let Some(doc) = ed.docs.iter().find(|d| d.id == id) else { return };
    if doc.loading {
        return outline(ed, call, path, line, format!("{shown} is still loading"));
    }
    let lines = crate::movement::last_line(&doc.text) + 1;
    if let Some(l) = line.filter(|&l| l >= lines) {
        return answer(ed, call, Err(format!("{shown} has {lines} lines — no line {}", l + 1)));
    }
    let n = ed.lsp.tool_ask_next;
    ed.lsp.tool_ask_next += 1;
    match ed.ask_symbols(id, n) {
        Err(why) => outline(ed, call, path, line, why),
        Ok((cid, server)) => {
            ed.lsp.tool_symbols.insert(n, SymAsk { call, path, line, server, cid });
            let wait = refs_wait(ed);
            ed.events.jobs().spawn(move || {
                std::thread::sleep(wait);
                move |ed: &mut Editor| {
                    if let Some(a) = ed.lsp.tool_symbols.remove(&n) {
                        let why = format!("{} didn't answer within {}", a.server, secs(wait));
                        outline(ed, a.call, a.path, a.line, why);
                    }
                }
            });
        }
    }
}

/// The server's document symbols for question `n` (Err: its error) — none from one still at work, or an
/// error, and it's tarae's outline.
pub fn symbols_answered(ed: &mut Editor, n: u64, result: Result<&Value, &Value>) {
    let Some(a) = ed.lsp.tool_symbols.remove(&n) else { return };
    let syms = match result {
        Ok(v) => lsp_symbols(v),
        Err(e) => {
            let why = format!("{}: {}", a.server, e["message"].as_str().unwrap_or("the request failed"));
            return outline(ed, a.call, a.path, a.line, why);
        }
    };
    if syms.is_empty() {
        let busy = ed.client(a.cid).is_some_and(|c| !c.progress.is_empty());
        let why = if busy { "is still loading" } else { "found none" };
        return outline(ed, a.call, a.path, a.line, format!("{} {why}", a.server));
    }
    let shown = rel(&project_root(ed), &a.path);
    answer(ed, a.call, Ok(symbols_text(&shown, &syms, a.line, &a.server)));
}

/// `textDocument/documentSymbol`'s answer: DocumentSymbols (nested, `range` the whole symbol) or
/// SymbolInformation (flat — `location.range`, the container in the name).
fn lsp_symbols(v: &Value) -> Vec<Sym> {
    fn walk(v: &Value, depth: usize, out: &mut Vec<Sym>) {
        for s in v.as_array().into_iter().flatten() {
            let nested = s["range"].is_object();
            let range = if nested { &s["range"] } else { &s["location"]["range"] };
            let at = |k: &str| range[k]["line"].as_u64().unwrap_or(0) as usize;
            let mut name = s["name"].as_str().unwrap_or_default().to_string();
            if let Some(c) = s["containerName"].as_str().filter(|c| !c.is_empty() && !nested) {
                name = format!("{c}::{name}");
            }
            let kind = lsp_kind(s["kind"].as_u64().unwrap_or(0), &name);
            out.push(Sym { depth, kind, name, lines: (at("start"), at("end").max(at("start"))) });
            if s["children"].is_array() {
                walk(&s["children"], depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    walk(v, 0, &mut out);
    out
}

/// LSP's `SymbolKind` as a word (none for an `impl X`, a name that says what it is).
fn lsp_kind(kind: u64, name: &str) -> &'static str {
    if name.starts_with("impl ") {
        return "";
    }
    match kind {
        1 => "file",
        2 => "mod",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "fn",
        13 => "variable",
        14 => "const",
        22 => "variant",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type",
        _ => "",
    }
}

/// tarae's outline (`syntax::outline` — its grammar, on a worker) as the answer — `why` the server's
/// symbols aren't.
fn outline(ed: &mut Editor, call: Call, path: PathBuf, line: Option<usize>, why: String) {
    let text = ed.docs.iter().find(|d| d.path.as_ref() == Some(&path) && !d.loading).map(|d| d.text.clone());
    let shown = rel(&project_root(ed), &path);
    ed.events.jobs().spawn(move || {
        let r = outline_syms(&path, text)
            .map(|syms| symbols_text(&shown, &syms, line, &format!("tarae's outline — {why}")))
            .map_err(|e| format!("{why}, and {e}"));
        move |ed: &mut Editor| answer(ed, call, r)
    });
}

fn outline_syms(path: &Path, text: Option<ropey::Rope>) -> Result<Vec<Sym>, String> {
    let spec = crate::syntax::detect(path).ok_or("tarae doesn't know this file's language")?;
    let lang = crate::syntax::Loader::global()
        .load(spec)
        .map_err(|_| format!("tarae has no {} grammar installed for its own outline", spec.name))?;
    let rope = match text {
        Some(t) => t,
        None => ropey::Rope::from_str(&std::fs::read_to_string(path).map_err(|e| e.to_string())?),
    };
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(&lang.language).map_err(|e| e.to_string())?;
    let tree = parser.parse(rope.to_string(), None).ok_or("its parse failed")?;
    let line = |b: usize| rope.byte_to_line(b.min(rope.len_bytes()));
    Ok(crate::syntax::outline(&tree, &rope)
        .into_iter()
        .map(|d| {
            let kind = match d.kind {
                _ if d.name.starts_with("impl ") => "",
                'f' => "fn",
                't' => "type",
                'm' => "mod",
                _ => "",
            };
            let lines = (line(d.range.start), line(d.range.end.saturating_sub(1).max(d.range.start)));
            Sym { depth: d.depth, kind, name: d.name, lines }
        })
        .collect())
}

/// The answer: the outline, nested — or, with `line`, the symbols holding it, outermost first (by what
/// spans it, so a flat list nests too). `from`: the server, or why it's tarae's outline.
fn symbols_text(shown: &str, syms: &[Sym], line: Option<usize>, from: &str) -> String {
    let row = |s: &Sym, indent: usize| {
        let (a, b) = (s.lines.0 + 1, s.lines.1 + 1);
        let lines = if a == b { a.to_string() } else { format!("{a}–{b}") };
        let kind = if s.kind.is_empty() { String::new() } else { format!("{} ", s.kind) };
        format!("{}{kind}{}  {lines}\n", "  ".repeat(indent), s.name)
    };
    let text = match line {
        Some(l) => {
            let mut holding: Vec<&Sym> = syms.iter().filter(|s| s.lines.0 <= l && l <= s.lines.1).collect();
            holding.sort_by_key(|s| (s.lines.0, std::cmp::Reverse(s.lines.1)));
            if holding.is_empty() {
                return format!("line {} of {shown} is in no symbol — at the top level ({from})", l + 1);
            }
            let rows: String = holding.iter().enumerate().map(|(k, s)| row(s, k)).collect();
            format!("line {} of {shown} is in ({from}):\n{rows}", l + 1)
        }
        None => {
            if syms.is_empty() {
                return format!("no symbols in {shown} ({from})");
            }
            let rows: String = syms.iter().take(SYMS_SHOWN).map(|s| row(s, s.depth)).collect();
            let more = match syms.len().saturating_sub(SYMS_SHOWN) {
                0 => String::new(),
                n => format!("… {n} more not shown — ask with a line for the part you need\n"),
            };
            format!("{} in {shown} ({from}):\n{rows}{more}", count(syms.len(), "symbol"))
        }
    };
    text.trim_end().to_string()
}

// ── definition ──────────────────────────────────────────────────────────────

pub fn definition_schema() -> Value {
    json!({
        "name": "definition",
        "description": "Where something is defined — the language server's go-to-definition, each place \
    with its signature (the definition's first lines, joined). Give the file and line where the name is \
    written (any use of it) and the name, or a column on that line instead of the name; kind implementation \
    lists a trait's or type's implementations, kind type where the value's type is defined. Or give only \
    the symbol (a type, function, const …; `Editor::root` puts the ones in Editor first) to find it by \
    name across the project — fields aren't found by name, ask at a use. Reach for it instead of searching for `fn name` and reading files. While the \
    server is still loading, the answer is the syntax tree's definitions among the name's text matches, \
    and says so.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "symbol": { "type": "string", "description": "The name as written in the code (`goto_ask`; `Editor::goto_ask`)" },
                "path": { "type": "string", "description": "A file where the name is written, relative to the project root — left out: found by name" },
                "line": { "type": "integer", "description": "The line it's on, 1-based — left out: its first appearance in the file" },
                "column": { "type": "integer", "description": "Its column on that line, 1-based (characters) — instead of the name" },
                "kind": { "type": "string", "enum": ["definition", "implementation", "type"], "description": "definition (default) · implementation: a trait's or type's implementations · type: where the value's type is defined" }
            }
        }
    })
}

/// Lines of a definition joined into its signature at most (while brackets stay open — a parameter list
/// over lines).
const SIG_LINES: usize = 12;

/// The syntax tree's stand-in: files parsed at most, and how big one may be.
const TREE_FILES: usize = 400;
const TREE_FILE_MAX: usize = 4 << 20;

/// Kinds of nodes that define what their `name` names, by their ends (`function_item`, `class_definition`,
/// `method_declaration`, `type_spec`, `variable_declarator` …).
const DEF_ENDS: [&str; 7] =
    ["_item", "_definition", "_declaration", "_specifier", "_spec", "_declarator", "_signature"];

/// Where a `definition` or `hover` call points: the file, the line, and the name on it — or a column
/// (from 0, in characters).
pub(crate) struct Spot {
    pub path: PathBuf,
    pub line: Option<usize>,
    pub column: Option<usize>,
    pub word: Option<String>,
}

/// The call's place: `path` (from `root`), `line`, and the name in `symbol` or a `column`.
pub(crate) fn spot_of(root: &Path, args: &Value) -> Result<Spot, String> {
    let path = args["path"].as_str().map(str::trim).filter(|p| !p.is_empty()).ok_or("path is required")?;
    let one = |k: &str| args[k].as_u64().map(|l| l.max(1) as usize - 1);
    let (line, column) = (one("line"), one("column"));
    let word = match args["symbol"].as_str().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(name_of(s)?),
        None => None,
    };
    if word.is_none() && (line.is_none() || column.is_none()) {
        return Err("give the symbol (the name as written), or a line and a column".into());
    }
    Ok(Spot { path: root.join(path), line, column, word })
}

/// The spot in `text`: (line, byte column, the name there — the one given, else the word at the column).
pub(crate) fn place_in(
    text: &ropey::Rope,
    spot: &Spot,
    shown: &str,
) -> Result<(usize, usize, String), String> {
    let (Some(l), Some(c)) = (spot.line, spot.column) else {
        let word = spot.word.clone().unwrap_or_default();
        let (l, col) = locate(text, &word, spot.line, shown)?;
        return Ok((l, col, word));
    };
    let lines = crate::movement::last_line(text) + 1;
    if l >= lines {
        return Err(format!("{shown} has {lines} lines — no line {}", l + 1));
    }
    let s = text.line(l).to_string();
    let s = s.trim_end_matches(['\n', '\r']);
    let Some((byte, _)) = s.char_indices().nth(c) else {
        let n = s.chars().count();
        return Err(format!("line {} of {shown} has {n} characters — no column {}", l + 1, c + 1));
    };
    let word = |ch: char| ch.is_alphanumeric() || ch == '_';
    let start =
        s[..byte].char_indices().rev().find(|&(_, ch)| !word(ch)).map_or(0, |(i, ch)| i + ch.len_utf8());
    let end = s[byte..].find(|ch: char| !word(ch)).map_or(s.len(), |i| byte + i);
    Ok((l, byte, spot.word.clone().unwrap_or_else(|| s[start..end].to_string())))
}

/// The question's next number (shared by the tools' questions).
pub(crate) fn next_ask(ed: &mut Editor) -> u64 {
    let n = ed.lsp.tool_ask_next;
    ed.lsp.tool_ask_next += 1;
    n
}

/// What `definition` lists, one of them.
fn what_of(goto: Goto) -> &'static str {
    match goto {
        Goto::Implementation => "implementation",
        Goto::TypeDefinition => "type definition",
        _ => "definition",
    }
}

fn goto_of(args: &Value) -> Result<Goto, String> {
    match args["kind"].as_str().map(str::trim).unwrap_or_default() {
        "" | "definition" => Ok(Goto::Definition),
        "implementation" | "implementations" => Ok(Goto::Implementation),
        "type" | "type_definition" | "typeDefinition" => Ok(Goto::TypeDefinition),
        k => Err(format!("kind is definition, implementation or type — not `{k}`")),
    }
}

/// The container a qualified name gives (`Editor` of `Editor::root`, `a::Editor::root`).
fn container_of(symbol: &str) -> Option<String> {
    let s = symbol.trim().trim_end_matches("()");
    let (before, _) = s.rsplit_once("::").or_else(|| s.rsplit_once('.'))?;
    before.rsplit([':', '.']).find(|p| !p.is_empty()).map(str::to_string)
}

/// A `definition` question asked of its servers: what's come back so far.
pub struct DefAsk {
    call: Call,
    word: String,
    goto: Goto,
    /// Asked by name (`workspace/symbol`), and the container named (`Editor` of `Editor::root`).
    by_name: bool,
    container: Option<String>,
    /// Servers yet to answer · the names of those asked.
    waiting: Vec<(ClientId, String)>,
    servers: Vec<String>,
    /// Places found: path, line, what it is (by name: its kind and container) · by name, those whose name
    /// isn't exactly it.
    found: Vec<(PathBuf, usize, String)>,
    near: Vec<(PathBuf, usize, String)>,
    /// Why a server gave nothing (an error, still loading — `later`: it will, ask again).
    why: Vec<String>,
    later: bool,
}

/// The tool: at a place — the file checked on a worker, then asked about (`def_at`); or by name. Err =
/// answered now.
pub fn definition(ed: &mut Editor, call: Call, args: &Value) -> Result<(), String> {
    let goto = goto_of(args)?;
    let root = project_root(ed);
    if args["path"].as_str().is_none_or(|p| p.trim().is_empty()) {
        let symbol = args["symbol"].as_str().unwrap_or_default();
        let word = name_of(symbol)?;
        if goto != Goto::Definition {
            return Err(
                "kind implementation and type need the place: the path (and line) where the name is \
                        written"
                    .into(),
            );
        }
        by_name(ed, call, word, container_of(symbol));
        return Ok(());
    }
    let spot = spot_of(&root, args)?;
    ed.events.jobs().spawn(move || {
        let file = file_in(&root, &spot.path);
        move |ed: &mut Editor| match file {
            Ok(path) => def_at(ed, call, Spot { path, ..spot }, goto),
            Err(e) => answer(ed, call, Err(e)),
        }
    });
    Ok(())
}

/// On the main loop: the file's buffer (opened aside), the name found in it, the go-to sent — or the
/// syntax tree's stand-in when the server can't be asked now.
fn def_at(ed: &mut Editor, call: Call, spot: Spot, goto: Goto) {
    let shown = rel(&project_root(ed), &spot.path);
    let id = match buffer_for(ed, &spot.path) {
        Ok(id) => id,
        Err(e) => return answer(ed, call, Err(format!("{shown}: {e}"))),
    };
    let Some(doc) = ed.docs.iter().find(|d| d.id == id) else { return };
    if doc.loading {
        let word = spot.word.unwrap_or_default();
        return tree_defs(ed, call, word, goto, format!("{shown} is still loading"), true);
    }
    let (line, col, word) = match place_in(&doc.text, &spot, &shown) {
        Ok(at) => at,
        Err(e) => return answer(ed, call, Err(e)),
    };
    let byte = doc.text.line_to_byte(line) + col;
    let n = next_ask(ed);
    let cannot = format!("no server here finds {}s", what_of(goto));
    match ed.ask_at(id, byte, Kind::ToolGoto { ask: n, goto }, &cannot) {
        Err((why, later)) => tree_defs(ed, call, word, goto, why, later),
        Ok((cid, server)) => {
            let waiting = vec![(cid, server.clone())];
            wait_defs(ed, n, DefAsk::new(call, word, goto, None, waiting, vec![server]));
        }
    }
}

/// By name: the project's servers asked (`workspace/symbol`) — or the syntax tree's stand-in.
fn by_name(ed: &mut Editor, call: Call, word: String, container: Option<String>) {
    let n = next_ask(ed);
    let root = project_root(ed);
    match ed.ask_named(&word, n, &root) {
        Err((why, later)) => tree_defs(ed, call, word, Goto::Definition, why, later),
        Ok(asked) => {
            let servers = asked.iter().map(|a| a.1.clone()).collect();
            wait_defs(ed, n, DefAsk::new(call, word, Goto::Definition, Some(container), asked, servers));
        }
    }
}

impl DefAsk {
    fn new(
        call: Call,
        word: String,
        goto: Goto,
        by_name: Option<Option<String>>,
        waiting: Vec<(ClientId, String)>,
        servers: Vec<String>,
    ) -> DefAsk {
        DefAsk {
            call,
            word,
            goto,
            by_name: by_name.is_some(),
            container: by_name.flatten(),
            waiting,
            servers,
            found: Vec::new(),
            near: Vec::new(),
            why: Vec::new(),
            later: false,
        }
    }
}

/// Question `n` kept until its servers answer — the ones silent for `REFS_WAIT` are given up on.
fn wait_defs(ed: &mut Editor, n: u64, a: DefAsk) {
    ed.lsp.tool_defs.insert(n, a);
    let wait = refs_wait(ed);
    ed.events.jobs().spawn(move || {
        std::thread::sleep(wait);
        move |ed: &mut Editor| {
            let Some(a) = ed.lsp.tool_defs.get_mut(&n) else { return };
            for (_, server) in a.waiting.drain(..) {
                a.why.push(format!("{server} didn't answer within {}", secs(wait)));
                a.later = true;
            }
            finish_defs(ed, n);
        }
    });
}

/// A server's answer to question `n` (Err: its error) — places (`textDocument/definition` and kin), or
/// symbols by name (`workspace/symbol`: the name exactly, else close ones).
pub fn def_answered(ed: &mut Editor, n: u64, cid: ClientId, result: Result<&Value, &Value>) {
    let busy = ed.client(cid).filter(|c| !c.progress.is_empty()).map(|c| {
        let titles: Vec<String> = c.progress.values().map(|p| p.title.clone()).collect();
        format!("{} is still loading ({})", c.name, titles.join(", "))
    });
    let Some(a) = ed.lsp.tool_defs.get_mut(&n) else { return };
    let Some(i) = a.waiting.iter().position(|w| w.0 == cid) else { return };
    let (_, server) = a.waiting.remove(i);
    let before = a.found.len() + a.near.len();
    match result {
        Ok(v) if a.by_name => {
            for s in v.as_array().into_iter().flatten() {
                let Some(path) = s["location"]["uri"].as_str().and_then(crate::lsp::path_from_uri) else {
                    continue;
                };
                let line = s["location"]["range"]["start"]["line"].as_u64().unwrap_or(0) as usize;
                let name = s["name"].as_str().unwrap_or_default();
                let kind = lsp_kind(s["kind"].as_u64().unwrap_or(0), name);
                let what = match s["containerName"].as_str().filter(|c| !c.is_empty()) {
                    Some(c) => format!("{kind} in {c}").trim_start().to_string(),
                    None => kind.to_string(),
                };
                match name == a.word {
                    true => a.found.push((path, line, what)),
                    false => {
                        a.near.push((path, line, format!("{name}: {what}").trim_end_matches(": ").into()))
                    }
                }
            }
        }
        Ok(v) => a.found.extend(
            crate::lsp::locations(v)
                .into_iter()
                .map(|(path, pos)| (path, pos["line"].as_u64().unwrap_or(0) as usize, String::new())),
        ),
        // (content modified / server cancelled: the project changed under the question — it's loading)
        Err(e) if matches!(e["code"].as_i64(), Some(-32801 | -32802)) => {
            a.why.push(format!("{server} is still loading the project"));
            a.later = true;
        }
        Err(e) => a.why.push(format!("{server}: {}", e["message"].as_str().unwrap_or("the request failed"))),
    }
    if a.found.len() + a.near.len() == before
        && let Some(busy) = busy
    {
        a.why.push(format!("{busy} and found none yet"));
        a.later = true;
    }
    finish_defs(ed, n);
}

/// All answers in: the places listed with their signatures (read on a worker) — none, and it's the syntax
/// tree's stand-in, saying why.
fn finish_defs(ed: &mut Editor, n: u64) {
    if ed.lsp.tool_defs.get(&n).is_none_or(|a| !a.waiting.is_empty()) {
        return;
    }
    let Some(a) = ed.lsp.tool_defs.remove(&n) else { return };
    let servers = a.servers.join(" and ");
    if a.found.is_empty() && (a.near.is_empty() || a.later) {
        let why = match a.why.is_empty() {
            false => a.why.join("; "),
            true if a.by_name => format!("{servers} found nothing named `{}`", a.word),
            true => format!("{servers} found no {}s there", what_of(a.goto)),
        };
        return tree_defs(ed, a.call, a.word, a.goto, why, a.later);
    }
    let all: Vec<&PathBuf> = a.found.iter().chain(&a.near).map(|f| &f.0).collect();
    let open: HashMap<PathBuf, ropey::Rope> = ed
        .docs
        .iter()
        .filter_map(|d| Some((d.path.clone()?, d.text.clone())))
        .filter(|(p, _)| all.contains(&p))
        .collect();
    let root = project_root(ed);
    ed.events.jobs().spawn(move || {
        let text = defs_text(&root, &a, &servers, &open);
        move |ed: &mut Editor| answer(ed, a.call, Ok(text))
    });
}

/// The answer: how many and from which server, then the places by file — each its signature (one outside
/// the project named, not read); by name, those in the container asked for first, close names after
/// when there's no exact one.
fn defs_text(root: &Path, a: &DefAsk, servers: &str, open: &HashMap<PathBuf, ropey::Rope>) -> String {
    let real = std::fs::canonicalize(root).ok();
    let inside = |p: &Path| p.starts_with(root) || real.as_ref().is_some_and(|r| p.starts_with(r));
    let read = |found: &[(PathBuf, usize, String)]| -> Vec<(Place, String)> {
        let mut files: HashMap<&Path, Vec<String>> = HashMap::new();
        let mut out: Vec<(Place, String)> = found
            .iter()
            .map(|(path, line, what)| {
                let text = if !inside(path) {
                    "(outside the project — not read)".to_string()
                } else if let Some(rope) = open.get(path) {
                    signature(&|k| (k < rope.len_lines()).then(|| rope.line(k).to_string()), *line)
                } else {
                    let lines = files.entry(path.as_path()).or_insert_with(|| {
                        std::fs::read(path)
                            .map(|b| String::from_utf8_lossy(&b).lines().map(str::to_string).collect())
                            .unwrap_or_default()
                    });
                    signature(&|k| lines.get(k).cloned(), *line)
                };
                (Place { path: path.clone(), line: *line, col: 0, text }, what.clone())
            })
            .collect();
        let first = |w: &str| a.container.as_ref().is_some_and(|c| w.ends_with(&format!(" in {c}")));
        out.sort_by(|x, y| (!first(&x.1), &x.0.path, x.0.line).cmp(&(!first(&y.1), &y.0.path, y.0.line)));
        out.dedup_by(|x, y| x.0.path == y.0.path && x.0.line == y.0.line);
        out
    };
    let list = |places: Vec<(Place, String)>| {
        let marks: HashMap<(PathBuf, usize), String> =
            places.iter().map(|(p, w)| ((p.path.clone(), p.line), w.clone())).collect();
        let mark = |p: &Place| marks.get(&(p.path.clone(), p.line)).filter(|w| !w.is_empty()).cloned();
        grouped(root, places.into_iter().map(|(p, _)| (p, false)).collect(), &mark, REFS_SHOWN)
    };
    let found = read(&a.found);
    if found.is_empty() {
        let near = read(&a.near);
        return format!(
            "nothing named exactly `{}` ({servers}, by name); close names:\n{}",
            a.word,
            list(near)
        );
    }
    // Places outside the project (the standard library, dependencies) are listed only when there are no
    // others — sorted in by path they took the cap from the project's own (`impl Drop`: 200 of 218 rows)
    let (mine, outside): (Vec<_>, Vec<_>) = found.into_iter().partition(|f| inside(&f.0.path));
    let (found, outside) = if mine.is_empty() { (outside, Vec::new()) } else { (mine, outside) };
    let by = if a.by_name { format!(" of `{}` by name", a.word) } else { String::new() };
    let first = match &a.container {
        Some(c) if found.iter().any(|f| f.1.ends_with(&format!(" in {c}"))) => {
            format!(", those in {c} first")
        }
        _ => String::new(),
    };
    let more = match outside.len() {
        0 => String::new(),
        n => format!("; {n} more outside the project ({}), not listed", owners(&outside)),
    };
    format!("{}{by} ({servers}){first}{more}:\n{}", count(found.len(), what_of(a.goto)), list(found))
}

/// Whose places outside the project are, in a few words — their crates, the most first (`std 80, hashbrown
/// 9, …`): a toolchain library's name, a registry folder's without its version.
fn owners(places: &[(Place, String)]) -> String {
    let crate_of = |p: &Path| -> String {
        let parts: Vec<String> =
            p.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        let library = parts.windows(2).find(|w| w[0] == "library").map(|w| w[1].clone());
        let versioned = || {
            parts.iter().find_map(|c| {
                let (name, version) = c.rsplit_once('-')?;
                let numbered =
                    version.split('.').count() >= 3 && version.starts_with(|d: char| d.is_ascii_digit());
                numbered.then(|| name.to_string())
            })
        };
        library.or_else(versioned).unwrap_or_else(|| "elsewhere".into())
    };
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for (p, _) in places {
        *counts.entry(crate_of(&p.path)).or_default() += 1;
    }
    let mut counts: Vec<(String, usize)> = counts.into_iter().collect();
    counts.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut said: Vec<String> = counts.iter().take(6).map(|(c, n)| format!("{c} {n}")).collect();
    if counts.len() > 6 {
        said.push("…".into());
    }
    said.join(", ")
}

/// A definition's signature from its first line: while brackets stay open, the next lines joined (at
/// most `SIG_LINES`), cut at `LINE_MAX`.
fn signature(line: &dyn Fn(usize) -> Option<String>, at: usize) -> String {
    let mut out = String::new();
    let mut depth = 0i32;
    for k in at..at + SIG_LINES {
        let Some(l) = line(k) else { break };
        let l = l.trim();
        if !out.is_empty() && !l.is_empty() {
            if l.starts_with([')', ']']) {
                out.truncate(out.trim_end_matches(',').len());
            } else if !out.ends_with(['(', '[']) {
                out.push(' ');
            }
        }
        out.push_str(l);
        for c in l.chars() {
            match c {
                '(' | '[' => depth += 1,
                ')' | ']' => depth -= 1,
                _ => {}
            }
        }
        if depth <= 0 {
            break;
        }
    }
    clip(&out, LINE_MAX)
}

/// The syntax tree's definitions of `word` (its implementations for `goto` implementation) among its
/// whole-word text matches in the project, as the answer — `why` the server's isn't (`later`: it will be,
/// ask again).
fn tree_defs(ed: &mut Editor, call: Call, word: String, goto: Goto, why: String, later: bool) {
    if word.is_empty() {
        return answer(ed, call, Ok(format!("{why} — and there's no name at that column to look for")));
    }
    let root = project_root(ed);
    ed.events.jobs().spawn(move || {
        let r = tree_found(&root, &word, goto).map(|(places, no_grammar)| {
            let then = if later { " — ask again in a moment for the server's" } else { "" };
            let what = if goto == Goto::Implementation { "implementation" } else { "definition" };
            let langs = match no_grammar.is_empty() {
                true => String::new(),
                false => format!(
                    " ({} files weren't looked at — no grammar installed)",
                    no_grammar.into_iter().collect::<Vec<_>>().join(", ")
                ),
            };
            if places.is_empty() {
                return format!(
                    "no {what}s of `{word}` found by the syntax tree either — {why}{then}{langs}"
                );
            }
            let caveat = match goto {
                Goto::TypeDefinition => " (where the name itself is defined — its type needs the server)",
                _ => "",
            };
            let n = places.len();
            let marks: HashMap<(PathBuf, usize), String> =
                places.iter().map(|(p, w)| ((p.path.clone(), p.line), w.clone())).collect();
            let mark = |p: &Place| marks.get(&(p.path.clone(), p.line)).cloned();
            let places = places.into_iter().map(|(p, _)| (p, false)).collect();
            format!(
                "{} of `{word}` by the syntax tree among its text matches — {why}, so not the server's \
                 answer{then}{caveat}{langs}:\n{}",
                count(n, what),
                grouped(&root, places, &mark, REFS_SHOWN)
            )
        });
        move |ed: &mut Editor| answer(ed, call, r)
    });
}

/// The places the tree says define `word` (or implement it), each with its signature and what defines it
/// (`function`, `struct`, `impl`) — and the languages of files with hits that have no grammar here. On a
/// worker: a search, then each file with hits parsed (at most `TREE_FILES`).
type TreeFound = (Vec<(Place, String)>, std::collections::BTreeSet<String>);
fn tree_found(root: &Path, word: &str, goto: Goto) -> Result<TreeFound, String> {
    let q = crate::grep::Query {
        pattern: word,
        literal: true,
        case: Case::Sensitive,
        word: true,
        globs: &[],
        context: 0,
        mode: crate::grep::Mode::Lines,
    };
    let mut files: Vec<(PathBuf, Vec<usize>)> = Vec::new();
    crate::grep::search(root, None, &q, &|| false, &mut |f| {
        files.push((root.join(&f.rel), f.rows.iter().filter(|r| r.hit).map(|r| r.line - 1).collect()));
        files.len() < TREE_FILES
    })?;
    files.sort();
    let re = regex::Regex::new(&format!(r"\b{}\b", regex::escape(word))).map_err(|e| e.to_string())?;
    let (mut out, mut no_grammar) = (Vec::new(), std::collections::BTreeSet::new());
    let mut parser = tree_sitter::Parser::new();
    for (path, hits) in files {
        let Some(spec) = crate::syntax::detect(&path) else { continue };
        let Ok(lang) = crate::syntax::Loader::global().load(spec) else {
            no_grammar.insert(spec.name.clone());
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        if text.len() > TREE_FILE_MAX || parser.set_language(&lang.language).is_err() {
            continue;
        }
        let Some(tree) = parser.parse(&text, None) else { continue };
        let starts: Vec<usize> =
            std::iter::once(0).chain(text.match_indices('\n').map(|(i, _)| i + 1)).collect();
        let lines: Vec<&str> = text.lines().collect();
        let line_of = |b: usize| starts.partition_point(|&s| s <= b) - 1;
        for l in hits {
            let (Some(&s), e) = (starts.get(l), starts.get(l + 1).map_or(text.len(), |e| e - 1)) else {
                continue;
            };
            for m in re.find_iter(&text[s..e.max(s)]) {
                let b = s + m.start();
                let Some(node) = tree.root_node().descendant_for_byte_range(b, b + m.len()) else { continue };
                let Some((what, at)) = defines(node, goto) else { continue };
                let line = line_of(at);
                let text = signature(&|k| lines.get(k).map(|l| l.to_string()), line);
                out.push((Place { path: path.clone(), line, col: 0, text }, what));
            }
        }
    }
    out.dedup_by(|a, b| a.0.path == b.0.path && a.0.line == b.0.line);
    Ok((out, no_grammar))
}

/// Whether `node` (a name) is what a definition defines — its node's `name` (`function_item` →
/// `function`) — or, for implementations, in what an `impl` (or a class's superclasses) is for: (what
/// it is, where it starts).
fn defines(node: tree_sitter::Node, goto: Goto) -> Option<(String, usize)> {
    if goto == Goto::Implementation {
        let mut up = node.parent();
        while let Some(x) = up {
            for field in ["trait", "type", "superclass", "interfaces", "superclasses"] {
                if field == "type" && x.kind() != "impl_item" {
                    continue;
                }
                if let Some(f) = x.child_by_field_name(field)
                    && f.start_byte() <= node.start_byte()
                    && node.end_byte() <= f.end_byte()
                {
                    let what = x.kind().split('_').next().unwrap_or_default().to_string();
                    return Some((what, x.start_byte()));
                }
            }
            up = x.parent();
        }
        return None;
    }
    let parent = node.parent()?;
    if parent.child_by_field_name("name").is_none_or(|n| n.id() != node.id()) {
        return None;
    }
    let k = parent.kind();
    let what =
        if k == "enum_variant" { "variant" } else { DEF_ENDS.iter().find_map(|e| k.strip_suffix(e))? };
    Some((what.replace('_', " "), node.start_byte()))
}

// ── hover ───────────────────────────────────────────────────────────────────

pub fn hover_schema() -> Value {
    json!({
        "name": "hover",
        "description": "What the language server says about a name at a place — its type, signature and \
    docs (an editor's hover). Give the file, the line and the name on it (or a column). Reach for it to \
    learn a variable's or an expression's type, what a function takes and returns, or which method a call \
    resolves to, instead of reading definitions. While the server is still loading, the syntax tree's \
    definitions of the name stand in, and it says so.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "The file, relative to the project root" },
                "line": { "type": "integer", "description": "The line, 1-based — left out: the name's first appearance in the file" },
                "symbol": { "type": "string", "description": "The name as written on that line" },
                "column": { "type": "integer", "description": "Its column, 1-based (characters) — instead of the name" }
            },
            "required": ["path"]
        }
    })
}

/// Hover text at most: lines, chars.
const HOVER_LINES: usize = 80;
const HOVER_MAX: usize = 4000;

/// A `hover` question asked of a server.
pub struct HoverAsk {
    call: Call,
    word: String,
    /// `path:line` as answered.
    at: String,
    server: String,
    cid: ClientId,
}

/// The tool: the file checked on a worker, then asked about (`hover_at`). Err = answered now.
pub fn hover(ed: &mut Editor, call: Call, args: &Value) -> Result<(), String> {
    let root = project_root(ed);
    let spot = spot_of(&root, args)?;
    ed.events.jobs().spawn(move || {
        let file = file_in(&root, &spot.path);
        move |ed: &mut Editor| match file {
            Ok(path) => hover_at(ed, call, Spot { path, ..spot }),
            Err(e) => answer(ed, call, Err(e)),
        }
    });
    Ok(())
}

fn hover_at(ed: &mut Editor, call: Call, spot: Spot) {
    let shown = rel(&project_root(ed), &spot.path);
    let id = match buffer_for(ed, &spot.path) {
        Ok(id) => id,
        Err(e) => return answer(ed, call, Err(format!("{shown}: {e}"))),
    };
    let Some(doc) = ed.docs.iter().find(|d| d.id == id) else { return };
    if doc.loading {
        let word = spot.word.unwrap_or_default();
        return tree_defs(ed, call, word, Goto::Definition, format!("{shown} is still loading"), true);
    }
    let (line, col, word) = match place_in(&doc.text, &spot, &shown) {
        Ok(at) => at,
        Err(e) => return answer(ed, call, Err(e)),
    };
    let byte = doc.text.line_to_byte(line) + col;
    let n = next_ask(ed);
    match ed.ask_at(id, byte, Kind::ToolHover { ask: n }, "no server here gives hover information") {
        Err((why, later)) => tree_defs(ed, call, word, Goto::Definition, why, later),
        Ok((cid, server)) => {
            let at = format!("{shown}:{}", line + 1);
            ed.lsp.tool_hovers.insert(n, HoverAsk { call, word, at, server, cid });
            let wait = refs_wait(ed);
            ed.events.jobs().spawn(move || {
                std::thread::sleep(wait);
                move |ed: &mut Editor| {
                    if let Some(a) = ed.lsp.tool_hovers.remove(&n) {
                        let why = format!("{} didn't answer within {}", a.server, secs(wait));
                        tree_defs(ed, a.call, a.word, Goto::Definition, why, true);
                    }
                }
            });
        }
    }
}

/// The server's hover for question `n` (Err: its error): its text, trimmed — nothing from a server still at
/// work, and the syntax tree's definitions stand in.
pub fn hover_answered(ed: &mut Editor, n: u64, result: Result<&Value, &Value>) {
    let Some(a) = ed.lsp.tool_hovers.remove(&n) else { return };
    let busy = ed
        .client(a.cid)
        .filter(|c| !c.progress.is_empty())
        .map(|c| c.progress.values().map(|p| p.title.clone()).collect::<Vec<_>>().join(", "));
    match result {
        Ok(v) => match (hover_trim(&crate::lsp::hover_text(v)), busy) {
            (t, _) if !t.is_empty() => {
                answer(ed, a.call, Ok(format!("`{}` at {} ({}):\n{t}", a.word, a.at, a.server)))
            }
            (_, Some(busy)) => {
                let why = format!("{} is still loading ({busy}) and has nothing for it yet", a.server);
                tree_defs(ed, a.call, a.word, Goto::Definition, why, true)
            }
            (_, None) => {
                let t = format!("{} has no hover information for `{}` at {}", a.server, a.word, a.at);
                answer(ed, a.call, Ok(t))
            }
        },
        Err(e) if matches!(e["code"].as_i64(), Some(-32801 | -32802)) => {
            let why = format!("{} is still loading the project", a.server);
            tree_defs(ed, a.call, a.word, Goto::Definition, why, true)
        }
        Err(e) => {
            let why = e["message"].as_str().unwrap_or("the request failed");
            answer(ed, a.call, Err(format!("{}: {why}", a.server)))
        }
    }
}

/// Hover text as an answer: trailing spaces off, runs of blank lines one, at most `HOVER_LINES` lines and
/// `HOVER_MAX` chars — said when cut.
fn hover_trim(text: &str) -> String {
    let mut lines: Vec<&str> = Vec::new();
    for l in text.lines().map(str::trim_end) {
        if !(l.is_empty() && lines.last().is_none_or(|p| p.is_empty())) {
            lines.push(l);
        }
    }
    let n = lines.len();
    lines.truncate(HOVER_LINES);
    let mut out = lines.join("\n").trim_end().to_string();
    if out.chars().count() > HOVER_MAX || n > HOVER_LINES {
        out = clip(&out, HOVER_MAX);
        out.push_str(&format!("\n… cut ({n} lines) — the definition has the rest"));
    }
    out
}

// ── diagnostics ─────────────────────────────────────────────────────────────

pub fn diagnostics_schema() -> Value {
    json!({
        "name": "diagnostics",
        "description": "What the language servers report for files right now — errors, warnings and the rest, \
    each with its line, message and code — and whether they have checked those files yet or are still at \
    it (rust-analyzer's cargo check), so clean can be told from not checked yet. Without paths: the files \
    you edited this turn, else the files open in the editor. After an edit, give wait (seconds) to wait for \
    the servers to settle first — a quick way to learn whether a change compiles before a build; the \
    build and the tests stay the final word.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "paths": { "type": "array", "items": { "type": "string" }, "description": "Files, relative to the project root — left out: the files you edited this turn, else the open ones" },
                "wait": { "type": "integer", "description": "Seconds to wait at most for the servers to settle (their check done) before answering — after an edit; at most 120. Left out: what's there now" }
            }
        }
    })
}

/// The longest wait asked for, and what's waited at least / for quiet before a server counts as settled:
/// a check a save starts begins within `SETTLE`; one with nothing begun or published for `QUIET` has
/// nothing to say.
pub(crate) const WAIT_MAX: u64 = 120;
const SETTLE: std::time::Duration = std::time::Duration::from_millis(if cfg!(test) { 20 } else { 400 });
const QUIET: std::time::Duration = std::time::Duration::from_millis(if cfg!(test) { 100 } else { 1500 });

/// Rows of diagnostics an answer lists at most.
const DIAGS_SHOWN: usize = 150;

/// A `diagnostics` call waiting for its files' servers to settle.
pub struct DiagWait {
    call: Call,
    paths: Vec<PathBuf>,
    /// Paths given that aren't files in the project (why, each) · which files these are, when not given.
    unread: Vec<String>,
    which: &'static str,
    since: std::time::Instant,
    until: std::time::Instant,
    /// The session whose chat asked — answered there.
    session: crate::event::SessionId,
}

/// The tool: the files checked on a worker, then what's published for them — now, or once their servers
/// settle (`diagnostics_moved`). Err = answered now.
pub fn diagnostics(ed: &mut Editor, call: Call, args: &Value) -> Result<(), String> {
    let root = project_root(ed);
    // Writes whose save is held for quiet (`lsp_saved_outside`): their servers hear now — the check they
    // start is what a wait waits for
    ed.saves_due(true);
    let since = std::time::Instant::now();
    let wait = std::time::Duration::from_secs(args["wait"].as_u64().unwrap_or(0).min(WAIT_MAX));
    let given: Vec<PathBuf> = match &args["paths"] {
        Value::Array(a) => a.iter().filter_map(Value::as_str).map(|p| root.join(p.trim())).collect(),
        Value::String(p) if !p.trim().is_empty() => vec![root.join(p.trim())],
        _ => Vec::new(),
    };
    let (paths, which) = match given.is_empty() {
        false => (given, ""),
        true if !ed.session.checkpoints.turn.is_empty() => (
            ed.session.checkpoints.turn.iter().map(|t| t.path.clone()).collect(),
            " (the files you edited this turn)",
        ),
        true => {
            let open = ed.docs.iter().filter(|d| !crate::log::is_log(d.path.as_deref()));
            (open.filter_map(|d| d.path.clone()).collect::<Vec<_>>(), " (the files open in the editor)")
        }
    };
    if paths.is_empty() {
        return Err("no files to look at — you edited none this turn and none is open; give paths".into());
    }
    ed.events.jobs().spawn(move || {
        let (mut ok, mut unread) = (Vec::new(), Vec::new());
        for p in &paths {
            match file_in(&root, p) {
                Ok(p) if !ok.contains(&p) => ok.push(p),
                Ok(_) => {}
                Err(e) => unread.push(e),
            }
        }
        move |ed: &mut Editor| {
            if ok.is_empty() {
                return answer(ed, call, Err(unread.join("\n")));
            }
            let w = DiagWait {
                call,
                paths: ok,
                unread,
                which,
                since,
                until: since + wait,
                session: ed.session.id,
            };
            if wait.is_zero() {
                let text = diags_text(ed, &w, None);
                return answer(ed, w.call, Ok(text));
            }
            ed.lsp.tool_diags.push(w);
            // (looked at again when it may have settled with nothing to wake it, and at the end)
            for after in [SETTLE, QUIET, wait] {
                ed.events.jobs().spawn(move || {
                    std::thread::sleep(after.saturating_sub(since.elapsed()));
                    crate::nav::diagnostics_moved
                });
            }
        }
    });
    Ok(())
}

/// A server published, ended a piece of work or went: the `diagnostics` calls waiting whose servers have
/// all settled (or whose wait is up) are answered — each in its session (a server's news lands in the one in
/// front). While a session behind is swapped in, only its own: the others' are looked at by their own timers
/// or the next news.
pub fn diagnostics_moved(ed: &mut Editor) {
    if ed.lsp.tool_diags.is_empty() {
        return;
    }
    // (a session that's gone has no chat to answer)
    let sessions: Vec<crate::event::SessionId> =
        std::iter::once(&ed.session).chain(&ed.sessions).map(|l| l.id).collect();
    ed.lsp.tool_diags.retain(|w| sessions.contains(&w.session));
    let mut asking: Vec<crate::event::SessionId> = ed.lsp.tool_diags.iter().map(|w| w.session).collect();
    asking.sort_unstable();
    asking.dedup();
    for session in asking {
        if session == ed.session.id {
            diagnostics_moved_here(ed);
        } else if !ed.background() {
            ed.with_session(session, diagnostics_moved_here);
        }
    }
}

/// `diagnostics_moved` for the calls of the session in `ed.session`.
fn diagnostics_moved_here(ed: &mut Editor) {
    let (now, here) = (std::time::Instant::now(), ed.session.id);
    let (mine, others): (Vec<DiagWait>, Vec<DiagWait>) =
        std::mem::take(&mut ed.lsp.tool_diags).into_iter().partition(|w| w.session == here);
    ed.lsp.tool_diags = others;
    for w in mine {
        let servers: Vec<ClientId> = w.paths.iter().flat_map(|p| ed.servers_of_path(p)).collect();
        let settled = servers.iter().all(|&c| settled(ed, c, w.since));
        if settled || now >= w.until {
            let text = diags_text(ed, &w, Some(settled));
            answer(ed, w.call, Ok(text));
        } else {
            ed.lsp.tool_diags.push(w);
        }
    }
}

/// Whether server `cid` has settled since `since`: up, at no work, at least `SETTLE` on — and it has
/// published or ended work since, or been quiet for `QUIET`. One gone has nothing more to say.
fn settled(ed: &Editor, cid: ClientId, since: std::time::Instant) -> bool {
    let Some(c) = ed.client(cid) else { return true };
    let after = |t: &std::time::Instant| *t > since;
    let moved = ed.lsp.work_ended.get(&cid).is_some_and(after)
        || ed.lsp.published.iter().any(|((s, _), t)| *s == cid && after(t));
    c.ready && c.progress.is_empty() && since.elapsed() >= SETTLE && (moved || since.elapsed() >= QUIET)
}

/// Where a diagnostic is from and its code, as one label (`rustc E0308`, `clippy needless_return`).
pub(crate) fn code_of(d: &Value) -> String {
    let code = match &d["code"] {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    format!("{} {code}", d["source"].as_str().unwrap_or_default()).trim().to_string()
}

/// One diagnostic as listed: severity, line and column (from 1), message, code.
struct Diag {
    severity: u8,
    line: usize,
    col: usize,
    message: String,
    code: String,
}

/// The answer: counts, each file's diagnostics by line (or whether it's clean or not checked yet), then
/// each server's state — at work and since when, its last publish. `settled`: how a wait ended (None: none
/// asked).
fn diags_text(ed: &Editor, w: &DiagWait, settled: Option<bool>) -> String {
    let root = project_root(ed);
    let ago = |t: &std::time::Instant| secs(t.elapsed());
    let name = |c: &ClientId| ed.client(*c).map(|c| c.name.clone()).unwrap_or_default();
    let mut servers: Vec<ClientId> = Vec::new();
    let (mut listed, mut quiet) = (Vec::new(), Vec::new());
    let mut counts = [0usize; 5];
    for path in &w.paths {
        let mut rows: Vec<Diag> = match ed.docs.iter().find(|d| d.path.as_deref() == Some(path.as_path())) {
            Some(doc) => doc
                .lsp
                .diagnostics
                .iter()
                .map(|d| {
                    let line = crate::movement::line_of(&doc.text, d.from);
                    let start = crate::movement::line_start(&doc.text, line);
                    let col = doc.text.byte_slice(start..d.from.max(start)).len_chars() + 1;
                    let (message, code) = (d.message.clone(), code_of(&d.raw));
                    Diag { severity: d.severity, line: line + 1, col, message, code }
                })
                .collect(),
            None => ed
                .lsp
                .elsewhere
                .get(path)
                .into_iter()
                .flatten()
                .map(|(_, severity, line, col, message, code)| Diag {
                    severity: *severity,
                    line: line + 1,
                    col: col + 1,
                    message: message.clone(),
                    code: code.clone(),
                })
                .collect(),
        };
        rows.sort_by_key(|d| (d.line, d.col, d.severity));
        let theirs = ed.servers_of_path(path);
        for c in &theirs {
            if !servers.contains(c) {
                servers.push(*c);
            }
        }
        let shown = rel(&root, path);
        if !rows.is_empty() {
            for d in &rows {
                counts[d.severity.clamp(1, 4) as usize] += 1;
            }
            listed.push((shown, rows));
            continue;
        }
        let names = theirs.iter().map(name).collect::<Vec<_>>().join(" and ");
        let last = theirs.iter().filter_map(|c| ed.lsp.published.get(&(*c, path.clone()))).max();
        quiet.push(match (theirs.is_empty(), last) {
            (true, _) => format!("{shown} — no language server reports on it"),
            (false, Some(t)) => format!("{shown} — clean ({names} published for it {} ago)", ago(t)),
            (false, None) => format!("{shown} — not checked yet ({names} hasn't published for it)"),
        });
    }
    let n = w.paths.len();
    let said: Vec<String> = [(1, "error"), (2, "warning"), (3, "note"), (4, "hint")]
        .iter()
        .filter(|(s, _)| counts[*s] > 0)
        .map(|(s, what)| count(counts[*s], what))
        .collect();
    let unchecked = quiet.iter().filter(|q| q.contains("not checked yet")).count();
    let mut out = match said.is_empty() {
        true => {
            let not = if unchecked > 0 { format!(" — {unchecked} not checked yet") } else { String::new() };
            format!("no diagnostics in {}{}{not}", count(n, "file"), w.which)
        }
        false => format!("{} in {} of {}{}:", said.join(", "), listed.len(), count(n, "file"), w.which),
    };
    let mut rows_left = DIAGS_SHOWN;
    for (shown, rows) in &listed {
        out.push_str(&format!("\n{shown}"));
        for d in rows.iter().take(rows_left) {
            let sev = ["", "error", "warning", "note", "hint"][d.severity.clamp(1, 4) as usize];
            let msg: Vec<&str> = d.message.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
            let code = if d.code.is_empty() { String::new() } else { format!(" [{}]", d.code) };
            out.push_str(&format!(
                "\n  {}:{} {sev}: {}{code}",
                d.line,
                d.col,
                clip(&msg.join(" — "), 2 * LINE_MAX)
            ));
        }
        if rows.len() > rows_left {
            out.push_str(&format!("\n  … {} more not shown", rows.len() - rows_left));
        }
        rows_left = rows_left.saturating_sub(rows.len());
    }
    for q in &quiet {
        out.push_str(&format!("\n{q}"));
    }
    // Each server: at work (since when) or idle, and its last publish — whether what's above is the news
    for c in &servers {
        let Some(s) = ed.client(*c) else { continue };
        let state = match (s.ready, s.progress.is_empty()) {
            (false, _) => "starting".to_string(),
            (true, true) => "idle".to_string(),
            (true, false) => {
                let work: Vec<String> = s
                    .progress
                    .values()
                    .map(|p| match p.began {
                        Some(b) => format!("{} for {}", p.title, ago(&b)),
                        None => p.title.clone(),
                    })
                    .collect();
                format!("at work ({}) — what's above may still change", work.join(", "))
            }
        };
        let last = ed.lsp.published.iter().filter(|((x, _), _)| x == c).map(|(_, t)| t).max();
        let last = match last {
            Some(t) => format!("last published {} ago", ago(t)),
            None => "hasn't published anything yet".into(),
        };
        out.push_str(&format!("\n{}: {state}; {last}", s.name));
    }
    match settled {
        Some(true) => out.push_str(&format!("\n(waited {} for the servers to settle)", ago(&w.since))),
        Some(false) => out.push_str(&format!("\n(waited the full {} — not settled yet)", ago(&w.since))),
        None => {}
    }
    for u in &w.unread {
        out.push_str(&format!("\n{u}"));
    }
    out
}

// ── search ──────────────────────────────────────────────────────────────────

pub fn search_schema() -> Value {
    json!({
        "name": "search",
        "description": "Search the project's files for a regex — the editor's own project search \
    (ripgrep's engine). Use it over Grep for finding code and text in the project. Hits come by file then line, each \
    with its line of code (and context lines around it if you ask), capped and said so — narrow with path \
    or glob, or page with offset. Smart case: an all-lowercase pattern matches any case. Skips what \
    .gitignore skips; hidden files are searched. Inside the project only. For where a function, type or \
    variable is used, references is better (the language server's, not text).",
        "inputSchema": {
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "A regex (Rust/ripgrep syntax) — plain text with literal" },
                "path": { "type": "string", "description": "A folder or file in the project to search, relative to its root — the whole project if left out" },
                "glob": { "type": "string", "description": "Only files matching these globs, space-separated (`*.rs`, `src/**/*.{ts,tsx}`, `!*_test.go`)" },
                "context": { "type": "integer", "description": "Lines of context before and after each hit (up to 10)" },
                "output": { "type": "string", "enum": ["lines", "files", "count"], "description": "lines (default): each hit's line · files: the files with hits · count: hits per file" },
                "case": { "type": "string", "enum": ["smart", "sensitive", "insensitive"], "description": "smart (default): case-insensitive when the pattern is all lowercase" },
                "literal": { "type": "boolean", "description": "The pattern is plain text, not a regex" },
                "offset": { "type": "integer", "description": "Skip this many hits (lines) or files — the next page" },
                "tests": tests_schema()
            },
            "required": ["pattern"]
        }
    })
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Output {
    Lines,
    Files,
    Count,
}

#[derive(Clone, Debug)]
struct Query {
    pattern: String,
    literal: bool,
    case: Case,
    globs: Vec<String>,
    context: usize,
    output: Output,
    offset: usize,
    /// Hits in test code included (listed last) or left out.
    tests: bool,
}

/// The tool: the pattern checked now (a typo is answered at once), searched on a worker.
pub fn search(ed: &mut Editor, call: Call, args: &Value) -> Result<(), String> {
    let s = |k: &str| args[k].as_str().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
    let pattern = args["pattern"].as_str().filter(|p| !p.is_empty()).ok_or("pattern is required")?;
    let output = match s("output").as_deref() {
        None | Some("lines" | "content") => Output::Lines,
        Some("files" | "files_with_matches") => Output::Files,
        Some("count") => Output::Count,
        Some(o) => return Err(format!("output is lines, files or count — not {o}")),
    };
    let case = match s("case").as_deref() {
        None | Some("smart") => Case::Smart,
        Some("sensitive") => Case::Sensitive,
        Some("insensitive") => Case::Insensitive,
        Some(c) => return Err(format!("case is smart, sensitive or insensitive — not {c}")),
    };
    let q = Query {
        pattern: pattern.to_string(),
        literal: args["literal"].as_bool().unwrap_or(false),
        case,
        globs: s("glob").map(|g| g.split_whitespace().map(str::to_string).collect()).unwrap_or_default(),
        context: args["context"].as_u64().unwrap_or(0).min(CONTEXT_MAX) as usize,
        output,
        offset: args["offset"].as_u64().unwrap_or(0) as usize,
        tests: args["tests"].as_bool().unwrap_or(true),
    };
    if !q.literal {
        regex::RegexBuilder::new(&q.pattern).build().map_err(|e| {
            format!("not a regex: {}", e.to_string().lines().last().unwrap_or("invalid").trim())
        })?;
    }
    let root = project_root(ed);
    let path = s("path");
    // Unsaved buffers aren't what's on disk — said, when they're in what's searched
    let unsaved: Vec<PathBuf> =
        ed.docs.iter().filter(|d| d.is_modified()).filter_map(|d| d.path.clone()).collect();
    ed.events.jobs().spawn(move || {
        let r = run_search(&root, path.as_deref(), &q, &unsaved);
        move |ed: &mut Editor| answer(ed, call, r)
    });
    Ok(())
}

/// One line of a file in the answer: its number (from 1), whether it's a hit (else context), its text.
#[derive(Clone, Debug)]
struct Row {
    line: usize,
    hit: bool,
    text: String,
}

/// The search on a worker → the answer's text.
fn run_search(root: &Path, path: Option<&str>, q: &Query, unsaved: &[PathBuf]) -> Result<String, String> {
    let root = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
    let scope = match path {
        Some(p) => {
            let real = std::fs::canonicalize(root.join(p)).map_err(|e| format!("{p}: {e}"))?;
            if !real.starts_with(&root) {
                return Err(format!("{p} is outside the project — search looks inside it only"));
            }
            real
        }
        None => root.clone(),
    };
    let query = crate::grep::Query {
        pattern: &q.pattern,
        literal: q.literal,
        case: q.case,
        word: false,
        globs: &q.globs,
        context: q.context,
        mode: match q.output {
            Output::Lines => crate::grep::Mode::Lines,
            Output::Files => crate::grep::Mode::Files,
            Output::Count => crate::grep::Mode::Count,
        },
    };
    let search = |each: &mut dyn FnMut(crate::grep::File) -> bool| {
        crate::grep::search(&root, Some(&scope), &query, &|| false, each)
    };
    let unsaved: Vec<String> =
        unsaved.iter().filter(|p| p.starts_with(&scope)).map(|p| rel(&root, p)).collect();
    let note = match unsaved.is_empty() {
        true => String::new(),
        false => {
            format!("\n(unsaved edits in {} aren't searched — what's here is from disk)", unsaved.join(", "))
        }
    };
    let text = match q.output {
        Output::Lines => {
            let mut files: BTreeMap<String, Vec<Row>> = BTreeMap::new();
            let mut hits = 0;
            let warn = search(&mut |f| {
                hits += f.hits;
                let rows = f.rows.into_iter().map(|r| Row {
                    line: r.line,
                    hit: r.hit,
                    text: clip(&r.text, LINE_MAX),
                });
                files.entry(f.rel).or_default().extend(rows);
                hits < HITS_MAX
            })?;
            if files.is_empty() {
                return no_hits(warn, q);
            }
            let mut t = crate::testcode::Tests::new(&root);
            let entries = entries(files, &mut |path, line| t.at(&root.join(path), line - 1));
            lines_text(entries, q.tests, hits >= HITS_MAX, q.offset) + &warned(warn)
        }
        Output::Files | Output::Count => {
            let mut rows: Vec<String> = Vec::new();
            let counts = q.output == Output::Count;
            let warn = search(&mut |f| {
                rows.push(if counts { format!("{}:{}", f.rel, f.hits) } else { f.rel });
                true
            })?;
            if rows.is_empty() {
                return no_hits(warn, q);
            }
            // A file all test code goes last (one with some tests in it is counted with the rest)
            let mut t = crate::testcode::Tests::new(&root);
            let mut rows: Vec<(String, bool)> = rows
                .into_iter()
                .map(|r| {
                    let path = if counts { r.rsplit_once(':').map_or(r.as_str(), |(p, _)| p) } else { &r };
                    let test = t.whole(&root.join(path));
                    (r, test)
                })
                .collect();
            rows.sort_by(|a, b| (a.1, &a.0).cmp(&(b.1, &b.0)));
            files_text(rows, q.tests, counts, q.offset) + &warned(warn)
        }
    };
    Ok(text + &note)
}

fn no_hits(warn: Option<String>, q: &Query) -> Result<String, String> {
    match warn {
        Some(w) => Err(w),
        None if q.globs.is_empty() => Ok(format!("no matches for `{}`", q.pattern)),
        None => Ok(format!("no matches for `{}` in files matching {}", q.pattern, q.globs.join(" "))),
    }
}

/// What the search couldn't read, said after its hits.
fn warned(warn: Option<String>) -> String {
    warn.map(|w| format!("\n(not searched — {w})")).unwrap_or_default()
}

/// A file's runs of adjacent lines — in test code or not.
struct Entry {
    path: String,
    runs: Vec<Vec<Row>>,
    test: bool,
}

impl Entry {
    fn hits(&self) -> usize {
        self.runs.iter().flatten().filter(|r| r.hit).count()
    }
}

/// Each file's runs of adjacent lines (a line once), told apart by `is_test(path, line)` (rg's line, from
/// 1) at each run's first hit: the code by path, then the test code by path.
fn entries(files: BTreeMap<String, Vec<Row>>, is_test: &mut dyn FnMut(&str, usize) -> bool) -> Vec<Entry> {
    let (mut code, mut tests) = (Vec::new(), Vec::new());
    for (path, file) in files {
        let mut runs: Vec<Vec<Row>> = Vec::new();
        for r in file {
            match runs.last_mut() {
                Some(run) if run.last().is_some_and(|l| r.line <= l.line + 1) => {
                    if run.last().is_some_and(|l| l.line == r.line) {
                        continue;
                    }
                    run.push(r)
                }
                _ => runs.push(vec![r]),
            }
        }
        let (mut c, mut t) = (Vec::new(), Vec::new());
        for run in runs {
            let at = run.iter().find(|r| r.hit).map_or(run[0].line, |r| r.line);
            if is_test(&path, at) { t.push(run) } else { c.push(run) }
        }
        if !c.is_empty() {
            code.push(Entry { path: path.clone(), runs: c, test: false });
        }
        if !t.is_empty() {
            tests.push(Entry { path, runs: t, test: true });
        }
    }
    code.extend(tests);
    code
}

/// Hits by file, each run of lines together (`--` between runs), hit lines `12:`, context `13-` (as rg
/// prints them), test code last under its heading (or left out, `tests` false); from hit `offset` on, at
/// most `HITS_SHOWN` hits and `ROWS_SHOWN` rows.
fn lines_text(entries: Vec<Entry>, tests: bool, stopped: bool, offset: usize) -> String {
    let in_tests: usize = entries.iter().filter(|e| e.test).map(Entry::hits).sum();
    let entries: Vec<Entry> = entries.into_iter().filter(|e| tests || !e.test).collect();
    if entries.is_empty() {
        return format!("no matches outside test code{}", tests_said(in_tests, false));
    }
    let total: usize = entries.iter().map(Entry::hits).sum();
    let files_n = entries.iter().map(|e| &e.path).collect::<std::collections::HashSet<_>>().len();
    let plus = if stopped { "+" } else { "" };
    let mut out = String::new();
    let (mut seen, mut hits, mut rows) = (0, 0, 0);
    let mut rest: Vec<(String, usize)> = Vec::new();
    let mut last_shown = 0;
    let mut in_heading = false;
    for Entry { path, runs, test } in entries {
        let mut shown_here = Vec::new();
        let mut left = 0;
        for run in runs {
            let n = run.iter().filter(|r| r.hit).count();
            if seen + n <= offset {
                seen += n;
                continue;
            }
            if hits >= HITS_SHOWN || rows >= ROWS_SHOWN {
                left += n;
                continue;
            }
            let run: Vec<Row> = run.into_iter().take(ROWS_SHOWN - rows).collect();
            let (all, n) = (n, run.iter().filter(|r| r.hit).count());
            left += all - n;
            seen += n;
            hits += n;
            rows += run.len();
            last_shown = seen;
            shown_here.push(run);
        }
        if !shown_here.is_empty() {
            if test && !in_heading {
                in_heading = true;
                out.push_str(TESTS_HEADING);
            }
            let w = shown_here.iter().flatten().map(|r| r.line).max().unwrap_or(1).to_string().len();
            out.push_str(&path);
            out.push('\n');
            for (k, run) in shown_here.iter().enumerate() {
                if k > 0 {
                    out.push_str("  --\n");
                }
                for r in run {
                    let sep = if r.hit { ':' } else { '-' };
                    out.push_str(&format!("  {:>w$}{sep} {}\n", r.line, r.text));
                }
            }
        }
        if left > 0 {
            rest.push((path, left));
        }
    }
    // (`10000+ matching lines` — the `+` with the number)
    let lines = count(total, "matching line").replacen(' ', &format!("{plus} "), 1);
    let mut head = format!("{lines} in {}{}", count(files_n, "file"), tests_said(in_tests, tests));
    if hits == 0 {
        return format!("{head}; none past offset {offset}");
    }
    if offset > 0 || !rest.is_empty() {
        head.push_str(&format!(" — {}–{last_shown} below", offset + 1));
    }
    let mut text = format!("{head}:\n{out}");
    if !rest.is_empty() {
        text.push_str(&more(&rest, "matching line", &format!(" — offset: {last_shown} for the next ones")));
    }
    text.truncate(text.trim_end().len());
    text
}

/// `--files-with-matches` / `--count` lines, each with whether its file is all test code — sorted, those
/// last under their heading (or left out, `tests` false): from `offset` on, at most `FILES_SHOWN`.
fn files_text(rows: Vec<(String, bool)>, tests: bool, counts: bool, offset: usize) -> String {
    let in_tests = rows.iter().filter(|r| r.1).count();
    let rows: Vec<(String, bool)> = rows.into_iter().filter(|r| tests || !r.1).collect();
    if rows.is_empty() {
        return format!("no matches outside test code{}", tests_said(in_tests, false));
    }
    let total = rows.len();
    let shown: Vec<&(String, bool)> = rows.iter().skip(offset).take(FILES_SHOWN).collect();
    let said = tests_said(in_tests, tests);
    let mut head = if counts {
        let lines: usize = rows.iter().filter_map(|r| r.0.rsplit_once(':')?.1.parse::<usize>().ok()).sum();
        format!("{} in {}{said}", count(lines, "matching line"), count(total, "file"))
    } else {
        format!("{} with matches{said}", count(total, "file"))
    };
    if shown.is_empty() {
        return format!("{head}; none past offset {offset}");
    }
    let end = offset + shown.len();
    if offset > 0 || end < total {
        head.push_str(&format!(" — {}–{end} below", offset + 1));
    }
    let mut text = format!("{head}:\n");
    let mut in_heading = false;
    for (r, test) in shown {
        if *test && !in_heading {
            in_heading = true;
            text.push_str(TESTS_HEADING);
        }
        text.push_str(r);
        text.push('\n');
    }
    if end < total {
        text.push_str(&format!("… {} more — offset: {end} for the next ones", total - end));
    }
    text.truncate(text.trim_end().len());
    text
}

// ── shared ──────────────────────────────────────────────────────────────────

/// The project — where both tools look.
pub(crate) fn project_root(ed: &Editor) -> PathBuf {
    ed.session.root.clone()
}

/// `path` relative to `root` when inside it.
pub(crate) fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

/// `n` and the noun, plural when it isn't one (`1 file`, `3 files`, `2 text matches`).
pub(crate) fn count(n: usize, noun: &str) -> String {
    match n {
        1 => format!("1 {noun}"),
        _ if noun.ends_with("ch") => format!("{n} {noun}es"),
        _ => format!("{n} {noun}s"),
    }
}

/// `… N more <what>s in M files not shown: a (3), b (12), …<tail>` — files with how many each.
fn more(rest: &[(String, usize)], what: &str, tail: &str) -> String {
    let n: usize = rest.iter().map(|r| r.1).sum();
    let mut named: Vec<String> = rest.iter().take(MORE_NAMED).map(|(f, k)| format!("{f} ({k})")).collect();
    if rest.len() > MORE_NAMED {
        named.push("…".into());
    }
    let what = count(n, what);
    let (n, what) = what.split_once(' ').unwrap_or_default();
    format!("… {n} more {what} in {} not shown: {}{tail}\n", count(rest.len(), "file"), named.join(", "))
}

/// At most `max` chars, `…` when cut.
pub(crate) fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

pub(crate) fn secs(d: std::time::Duration) -> String {
    crate::term::duration(d.as_secs_f32())
}

/// A row's verdict once its answer is in (`12 references in 3 files`, `no matches for …`) — None for what
/// isn't one of these tools' answers.
pub fn verdict(text: &str) -> Option<String> {
    let first = text.lines().next()?;
    let re = regex::Regex::new(
        r"^(\d+\+? (?:references?|text match(?:es)?|matching lines?|places?|match(?:es)?) in \d+ files?|\d+ files? with matches|\d+ symbols?|no (?:text )?matches|\d+ (?:type )?(?:definitions?|implementations?)|\d+ (?:errors?|warnings?|notes?|hints?)(?:, \d+ (?:errors?|warnings?|notes?|hints?))*|no diagnostics)",
    )
    .ok()?;
    re.find(first).map(|m| m.as_str().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(path: &str, line: usize, text: &str) -> Place {
        Place { path: PathBuf::from(path), line, col: 0, text: text.into() }
    }

    #[test]
    fn a_symbol_is_its_last_name() {
        assert_eq!(name_of("Editor::goto_ask").unwrap(), "goto_ask");
        assert_eq!(name_of(" x.len() ").unwrap(), "len");
        assert_eq!(name_of("한글_이름").unwrap(), "한글_이름");
        assert!(name_of("a b").unwrap_err().contains("a name as written"));
        assert!(name_of("1st").is_err() && name_of("").unwrap_err().contains("required"));
    }

    #[test]
    fn the_name_is_found_on_its_line_as_a_whole_word() {
        let text = ropey::Rope::from_str("let ab = 1;\nfn a() { ab + a }\n");
        assert_eq!(locate(&text, "a", None, "f").unwrap(), (1, 3), "not inside `ab`");
        assert_eq!(locate(&text, "ab", Some(1), "f").unwrap(), (1, 9));
        let e = locate(&text, "a", Some(0), "f").unwrap_err();
        assert!(e.contains("isn't on line 1 of f — it reads `let ab = 1;`"), "{e}");
        assert!(locate(&text, "a", Some(9), "f").unwrap_err().contains("f has 2 lines — no line 10"));
        assert_eq!(locate(&text, "zz", None, "f").unwrap_err(), "`zz` isn't in f");
        // Without a line: its first line in code — a comment's only when it's nowhere else
        let text = ropey::Rope::from_str("//! what ab is\n#[ab]\n# ab\nlet ab = 1;\n");
        assert_eq!(locate(&text, "ab", None, "f").unwrap(), (1, 2), "an attribute is code");
        let text = ropey::Rope::from_str("//! what ab is\n    // ab\n# ab\nlet ab = 1;\n");
        assert_eq!(locate(&text, "ab", None, "f").unwrap(), (3, 4));
        let text = ropey::Rope::from_str("/* ab */\n");
        assert_eq!(locate(&text, "ab", None, "f").unwrap(), (0, 3), "only in a comment");
    }

    /// The project's places are listed; those outside it (the standard library, dependencies) only counted
    /// by crate — unless they're all there is.
    #[test]
    fn places_outside_the_project_are_counted_not_listed() {
        let outside = [
            "/h/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/hashbrown-0.15.5/src/raw/mod.rs",
            "/h/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/hashbrown-0.15.5/src/scopeguard.rs",
            "/h/.rustup/toolchains/1.99.0-aarch64-apple-darwin/lib/rustlib/src/rust/library/std/src/path.rs",
        ];
        let mut a =
            DefAsk::new(Call::test(0), "Drop".into(), Goto::Implementation, None, vec![], vec!["ra".into()]);
        a.found = outside.iter().map(|p| (PathBuf::from(p), 9, String::new())).collect();
        let open = HashMap::new();
        let text = defs_text(Path::new("/p"), &a, "ra", &open);
        assert!(text.starts_with("3 implementations (ra):\n/h/.cargo/"), "all outside: listed — {text}");
        assert!(text.contains("  10: (outside the project — not read)"), "{text}");
        a.found.push((PathBuf::from("/p/a.rs"), 2, String::new()));
        assert_eq!(
            defs_text(Path::new("/p"), &a, "ra", &open),
            "1 implementation (ra); 3 more outside the project (hashbrown 2, std 1), not listed:\na.rs\n  3:"
        );
    }

    /// The declaration's file first and its row first in it, marked; the rest by file then line; one row a
    /// line; past the cap the rest summed up per file.
    #[test]
    fn references_list_by_file_the_declaration_first() {
        let root = Path::new("/p");
        let places = [
            place("/p/b.rs", 4, "b()"),
            place("/p/a.rs", 2, "a()"),
            place("/p/b.rs", 1, "fn b() {}"),
            place("/p/a.rs", 11, "a(); a()"),
        ];
        let code = |ps: Vec<Place>| ps.into_iter().map(|p| (p, false)).collect::<Vec<_>>();
        let decl = [(PathBuf::from("/p/b.rs"), 1)];
        let text = refs_text(root, code(places[..4].to_vec()), &decl, "s", true);
        assert_eq!(
            text,
            "4 references in 2 files (s), the declaration first:\n\
             b.rs\n  2: fn b() {}  (declaration)\n  5: b()\na.rs\n   3: a()\n  12: a(); a()"
        );
        let sorted = vec![places[1].clone(), places[3].clone(), places[2].clone(), places[0].clone()];
        let text = grouped(root, code(sorted.clone()), &|_| None, 1);
        assert_eq!(text, "a.rs\n  3: a()\n… 3 more lines in 2 files not shown: a.rs (1), b.rs (2)");
        let text = grouped(root, code(sorted), &|_| None, 3);
        assert!(text.ends_with("… 1 more line in 1 file not shown: b.rs (1)"), "{text}");
    }

    /// Test code goes last, under its heading — a file with both shows twice — and is counted; left out, it's
    /// still counted.
    #[test]
    fn references_in_test_code_come_last_or_are_left_out() {
        let root = Path::new("/p");
        let places = vec![
            (place("/p/a.rs", 2, "a()"), false),
            (place("/p/a.rs", 30, "a() // in mod tests"), true),
            (place("/p/tests/t.rs", 4, "a()"), true),
            (place("/p/b.rs", 1, "pub fn a() {}"), false),
        ];
        let decl = [(PathBuf::from("/p/b.rs"), 1)];
        let text = refs_text(root, places.clone(), &decl, "s", true);
        assert_eq!(
            text,
            "4 references in 3 files (s), the declaration first; 2 in test code, listed last:\n\
             b.rs\n  2: pub fn a() {}  (declaration)\na.rs\n  3: a()\nin test code:\na.rs\n  31: a() // in mod tests\n\
             tests/t.rs\n  5: a()"
        );
        let text = refs_text(root, places, &decl, "s", false);
        assert_eq!(
            text,
            "2 references in 2 files (s), the declaration first; 2 more in test code, left out:\n\
             b.rs\n  2: pub fn a() {}  (declaration)\na.rs\n  3: a()"
        );
    }

    /// Search hits: each run told apart by its first hit — a file's tests after all the code; files all test
    /// code last in files/count; left out, said how many.
    #[test]
    fn search_hits_in_test_code_come_last_or_are_left_out() {
        let mut files = BTreeMap::new();
        files.insert("a.rs".to_string(), rows(&[(3, true), (40, true), (41, false)]));
        files.insert("tests/t.rs".to_string(), rows(&[(1, true)]));
        let mut is_test = |p: &str, l: usize| p.starts_with("tests/") || l >= 40;
        let e = entries(files.clone(), &mut is_test);
        assert_eq!(
            lines_text(e, true, false, 0),
            "3 matching lines in 2 files; 2 in test code, listed last:\na.rs\n  3: t3\n\
             in test code:\na.rs\n  40: t40\n  41- t41\ntests/t.rs\n  1: t1"
        );
        let e = entries(files.clone(), &mut is_test);
        assert_eq!(
            lines_text(e, false, false, 0),
            "1 matching line in 1 file; 2 more in test code, left out:\na.rs\n  3: t3"
        );
        let mut only_tests = BTreeMap::new();
        only_tests.insert("tests/t.rs".to_string(), rows(&[(1, true)]));
        let e = entries(only_tests, &mut is_test);
        assert_eq!(
            lines_text(e, false, false, 0),
            "no matches outside test code; 1 more in test code, left out"
        );
        let rows =
            vec![("a.rs".to_string(), false), ("b.rs".to_string(), false), ("tests/t.rs".to_string(), true)];
        assert_eq!(
            files_text(rows.clone(), true, false, 0),
            "3 files with matches; 1 in test code, listed last:\na.rs\nb.rs\nin test code:\ntests/t.rs"
        );
        assert_eq!(
            files_text(rows, false, false, 0),
            "2 files with matches; 1 more in test code, left out:\na.rs\nb.rs"
        );
    }

    fn rows(lines: &[(usize, bool)]) -> Vec<Row> {
        lines.iter().map(|&(line, hit)| Row { line, hit, text: format!("t{line}") }).collect()
    }

    /// Runs of lines with `--` between, `:` hits and `-` context; an offset skips hits by runs; the cap
    /// says what's left and how to get it.
    #[test]
    fn search_lines_come_by_file_in_runs_and_say_what_is_left() {
        let mut files = BTreeMap::new();
        files.insert("a.rs".to_string(), rows(&[(9, false), (10, true), (11, false), (40, true)]));
        files.insert("b.rs".to_string(), rows(&[(2, true)]));
        let lines_text = |files: BTreeMap<String, Vec<Row>>, stopped, offset| {
            lines_text(entries(files, &mut |_, _| false), true, stopped, offset)
        };
        let text = lines_text(files.clone(), false, 0);
        assert_eq!(
            text,
            "3 matching lines in 2 files:\na.rs\n   9- t9\n  10: t10\n  11- t11\n  --\n  40: t40\nb.rs\n  2: t2"
        );
        let text = lines_text(files.clone(), false, 1);
        assert_eq!(text, "3 matching lines in 2 files — 2–3 below:\na.rs\n  40: t40\nb.rs\n  2: t2");
        assert_eq!(lines_text(files.clone(), true, 5), "3+ matching lines in 2 files; none past offset 5");
        let many: Vec<(usize, bool)> = (0..HITS_SHOWN + 5).map(|i| (i * 3, true)).collect();
        let mut files = BTreeMap::new();
        files.insert("a.rs".to_string(), rows(&many));
        let text = lines_text(files, false, 0);
        assert!(text.starts_with("205 matching lines in 1 file — 1–200 below:"), "{}", &text[..80]);
        assert!(
            text.ends_with(
                "… 5 more matching lines in 1 file not shown: a.rs (5) — offset: 200 for the next ones"
            ),
            "{}",
            &text[text.len() - 120..]
        );
    }

    #[test]
    fn files_and_counts_page_too() {
        let rows: Vec<(String, bool)> = (0..5).map(|i| (format!("f{i}:{}", i + 1), false)).collect();
        assert_eq!(
            files_text(rows.clone(), true, true, 0),
            "15 matching lines in 5 files:\nf0:1\nf1:2\nf2:3\nf3:4\nf4:5"
        );
        let names: Vec<(String, bool)> = (0..FILES_SHOWN + 2).map(|i| (format!("f{i:03}"), false)).collect();
        let text = files_text(names, true, false, 0);
        assert!(text.starts_with("302 files with matches — 1–300 below:\nf000\n"), "{}", &text[..60]);
        assert!(text.ends_with("… 2 more — offset: 300 for the next ones"));
    }

    #[test]
    fn verdicts_are_the_answers_first_words() {
        let v = |t: &str| verdict(t);
        assert_eq!(
            v("12 references in 3 files (rust-analyzer), the declaration first:\n…").unwrap(),
            "12 references in 3 files"
        );
        assert_eq!(v("4 text matches in 1 file — fake is starting, …").unwrap(), "4 text matches in 1 file");
        assert_eq!(
            v("10000+ matching lines in 40 files — 1–200 below:").unwrap(),
            "10000+ matching lines in 40 files"
        );
        assert_eq!(v("no matches for `x`").unwrap(), "no matches");
        assert_eq!(v("src/a.rs:12:fn main"), None, "Grep's answer isn't one");
        assert_eq!(v("2 definitions of `a` by name (ra):").unwrap(), "2 definitions");
        assert_eq!(v("1 type definition (ra):").unwrap(), "1 type definition");
        assert_eq!(v("3 implementations of `S` by the syntax tree …").unwrap(), "3 implementations");
        assert_eq!(v("1 error, 2 warnings in 1 of 3 files:").unwrap(), "1 error, 2 warnings");
        assert_eq!(v("no diagnostics in 2 files").unwrap(), "no diagnostics");
    }

    /// A definition's signature: its lines joined while a bracket is open, a trailing comma before the
    /// closer dropped; one line when it closes there; cut at `SIG_LINES`.
    #[test]
    fn a_signature_joins_the_lines_its_brackets_span() {
        let lines = |src: &'static str| move |k: usize| src.lines().nth(k).map(str::to_string);
        let src = "    pub(crate) fn ask_at(\n        &mut self,\n        id: DocId,\n    ) -> Result<(), String> {\n        x\n";
        assert_eq!(
            signature(&lines(src), 0),
            "pub(crate) fn ask_at(&mut self, id: DocId) -> Result<(), String> {"
        );
        assert_eq!(signature(&lines("struct S;\nfn x() {}\n"), 0), "struct S;");
        let open = format!("f(\n{}", "a,\n".repeat(20));
        let open = |k: usize| open.lines().nth(k).map(str::to_string);
        assert_eq!(signature(&open, 0).matches("a,").count(), SIG_LINES - 1);
    }

    /// A column (characters, not bytes) names the word there; past the line's end it's said.
    #[test]
    fn a_column_is_counted_in_characters() {
        let text = ropey::Rope::from_str("let 한글 = foo(바);\n");
        // (from 0 here — the call's 1-based column less one)
        let spot =
            |column: usize| Spot { path: PathBuf::new(), line: Some(0), column: Some(column), word: None };
        assert_eq!(place_in(&text, &spot(5), "f").unwrap(), (0, 7, "한글".to_string()), "inside the word");
        assert_eq!(place_in(&text, &spot(9), "f").unwrap(), (0, 13, "foo".to_string()));
        assert!(place_in(&text, &spot(40), "f").unwrap_err().contains("has 16 characters — no column 41"));
        assert_eq!(container_of("Editor::root").as_deref(), Some("Editor"));
        assert_eq!(container_of("crate::editor::Editor::root()").as_deref(), Some("Editor"));
        assert_eq!(container_of("root"), None);
    }

    /// The syntax tree's guess: a node's `name` in a defining node (any language's `_item`, `_definition`
    /// …), not a use; for implementations, an `impl` naming it.
    #[test]
    fn the_tree_tells_a_definition_from_a_use() {
        let Some(spec) = crate::syntax::spec("rust") else { return };
        let Ok(lang) = crate::syntax::Loader::global().load(spec) else { return }; // (no Rust grammar)
        let src = "const N: usize = 1;\nfn f() -> N { N }\nenum E { N }\nimpl Tr for N {}\n";
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(&lang.language).unwrap();
        let tree = parser.parse(src, None).unwrap();
        let at =
            |b: usize, goto| defines(tree.root_node().descendant_for_byte_range(b, b + 1).unwrap(), goto);
        let ns: Vec<usize> = src.match_indices('N').map(|(i, _)| i).collect();
        assert_eq!(at(ns[0], Goto::Definition), Some(("const".to_string(), ns[0])));
        assert_eq!(at(ns[1], Goto::Definition), None, "a use right under a definition (its return type)");
        assert_eq!(at(ns[2], Goto::Definition), None, "a use");
        assert_eq!(at(ns[3], Goto::Definition), Some(("variant".to_string(), ns[3])));
        assert_eq!(at(ns[4], Goto::Implementation), Some(("impl".to_string(), src.find("impl").unwrap())));
        assert_eq!(at(ns[1], Goto::Implementation), None);
    }

    /// The search itself (in-process — rg or not): smart case, a glob, a folder, context, hidden files in
    /// and `.gitignore`d out; outside the project refused; an unsaved buffer said.
    #[test]
    fn search_looks_inside_the_project() {
        let dir = std::env::temp_dir().join(format!("tarae-nav-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join(".github")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn Alpha() {}\nlet alpha = 1;\n// x\n").unwrap();
        std::fs::write(dir.join("notes.md"), "alpha here\n").unwrap();
        std::fs::write(dir.join(".github/ci.yml"), "alpha: ci\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir_all(dir.join("ignored")).unwrap();
        std::fs::write(dir.join("ignored/x.rs"), "alpha\n").unwrap();
        // (.gitignore is read only in a git repository, as rg does)
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let q = |pattern: &str| Query {
            pattern: pattern.into(),
            literal: false,
            case: Case::Smart,
            globs: Vec::new(),
            context: 0,
            output: Output::Lines,
            offset: 0,
            tests: true,
        };
        let text = run_search(&dir, None, &q("alpha"), &[]).unwrap();
        assert!(text.starts_with("4 matching lines in 3 files:"), "{text}");
        assert!(text.contains(".github/ci.yml\n  1: alpha: ci"), "hidden files are searched: {text}");
        assert!(!text.contains("ignored"), ".gitignore'd: {text}");
        assert!(text.contains("src/a.rs\n  1: fn Alpha() {}\n  2: let alpha = 1;"), "{text}");
        let text = run_search(&dir, None, &q("Alpha"), &[]).unwrap();
        assert!(
            text.starts_with("1 matching line in 1 file:"),
            "an uppercase letter: case as written — {text}"
        );
        let text = run_search(&dir, Some("src"), &q("alpha"), &[]).unwrap();
        assert!(text.starts_with("2 matching lines in 1 file:\nsrc/a.rs"), "only in the folder: {text}");
        let text = run_search(&dir, Some("src"), &Query { context: 1, ..q("let") }, &[]).unwrap();
        assert_eq!(
            text,
            "1 matching line in 1 file:\nsrc/a.rs\n  1- fn Alpha() {}\n  2: let alpha = 1;\n  3- // x"
        );
        let text = run_search(&dir, None, &Query { globs: vec!["*.md".into()], ..q("alpha") }, &[]).unwrap();
        assert!(text.starts_with("1 matching line in 1 file:\nnotes.md"), "{text}");
        let text = run_search(&dir, None, &Query { output: Output::Count, ..q("alpha") }, &[]).unwrap();
        assert!(
            text.starts_with("4 matching lines in 3 files:\n.github/ci.yml:1\nnotes.md:1\nsrc/a.rs:2"),
            "{text}"
        );
        let text = run_search(&dir, None, &Query { literal: true, ..q("a.rs(") }, &[]).unwrap();
        assert_eq!(text, "no matches for `a.rs(`");
        let e = run_search(&dir, Some(".."), &q("alpha"), &[]).unwrap_err();
        assert!(e.contains("outside the project"), "{e}");
        let unsaved = [std::fs::canonicalize(dir.join("src/a.rs")).unwrap()];
        let text = run_search(&dir, Some("src"), &q("alpha"), &unsaved).unwrap();
        assert!(
            text.ends_with("(unsaved edits in src/a.rs aren't searched — what's here is from disk)"),
            "{text}"
        );
        // Test code, for real: a tests/ file, and a #[cfg(test)] module's lines (with the Rust grammar)
        std::fs::create_dir_all(dir.join("tests")).unwrap();
        std::fs::write(dir.join("tests/t.rs"), "fn beta() {}\n").unwrap();
        std::fs::write(
            dir.join("src/b.rs"),
            "fn beta() {}\n#[cfg(test)]\nmod tests {\n    fn beta2() {}\n}\n",
        )
        .unwrap();
        let text = run_search(&dir, None, &q("beta"), &[]).unwrap();
        let rust =
            crate::syntax::spec("rust").is_some_and(|s| crate::syntax::Loader::global().load(s).is_ok());
        let want = if rust {
            "3 matching lines in 2 files; 2 in test code, listed last:\nsrc/b.rs\n  1: fn beta() {}\n\
             in test code:\nsrc/b.rs\n  4:     fn beta2() {}\ntests/t.rs\n  1: fn beta() {}"
        } else {
            "3 matching lines in 2 files; 1 in test code, listed last:\nsrc/b.rs\n  1: fn beta() {}\n  --\n  \
             4:     fn beta2() {}\nin test code:\ntests/t.rs\n  1: fn beta() {}"
        };
        assert_eq!(text, want);
        let text = run_search(&dir, None, &Query { tests: false, ..q("beta") }, &[]).unwrap();
        assert!(text.starts_with("1 matching line in 1 file; "), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    fn sym(depth: usize, kind: &'static str, name: &str, a: usize, b: usize) -> Sym {
        Sym { depth, kind, name: name.into(), lines: (a, b) }
    }

    /// The outline nested by depth, each with its lines; with a line, what holds it, outermost first —
    /// worked out from the lines, so a flat list (SymbolInformation) nests too.
    #[test]
    fn symbols_list_the_outline_and_what_holds_a_line() {
        let syms = vec![
            sym(0, "", "impl Editor", 9, 99),
            sym(1, "fn", "handle_event", 19, 29),
            sym(1, "fn", "new", 31, 31),
            sym(0, "mod", "tests", 109, 199),
        ];
        assert_eq!(
            symbols_text("e.rs", &syms, None, "s"),
            "4 symbols in e.rs (s):\nimpl Editor  10–100\n  fn handle_event  20–30\n  fn new  32\nmod tests  110–200"
        );
        assert_eq!(
            symbols_text("e.rs", &syms, Some(24), "s"),
            "line 25 of e.rs is in (s):\nimpl Editor  10–100\n  fn handle_event  20–30"
        );
        let flat: Vec<Sym> = syms.iter().map(|s| Sym { depth: 0, ..s.clone() }).rev().collect();
        assert_eq!(symbols_text("e.rs", &flat, Some(24), "s"), symbols_text("e.rs", &syms, Some(24), "s"));
        assert_eq!(
            symbols_text("e.rs", &syms, Some(104), "s"),
            "line 105 of e.rs is in no symbol — at the top level (s)"
        );
    }

    /// Both shapes of `documentSymbol`'s answer.
    #[test]
    fn server_symbols_nested_and_flat() {
        let range = |a: u64, b: u64| json!({ "start": { "line": a, "character": 0 }, "end": { "line": b, "character": 1 } });
        let nested = json!([{ "name": "impl Editor", "kind": 19, "range": range(9, 99), "selectionRange": range(9, 9),
            "children": [{ "name": "handle_event", "kind": 6, "range": range(19, 29), "selectionRange": range(19, 19) }] }]);
        assert_eq!(
            lsp_symbols(&nested),
            [sym(0, "", "impl Editor", 9, 99), sym(1, "method", "handle_event", 19, 29)]
        );
        let flat = json!([{ "name": "run", "kind": 12, "containerName": "app", "location": { "uri": "file:///a", "range": range(3, 8) } }]);
        assert_eq!(lsp_symbols(&flat), [sym(0, "fn", "app::run", 3, 8)]);
    }

    /// A search over this repository that hits most of its files, test code told apart (`testcode.rs`) — a
    /// number for `cargo perf` (11.3 s once, 0.72 s since each file's declarations are read once an answer;
    /// measured 2026-10-03). At opt-level 0 (the test profile) it skips itself, saying so past libtest's
    /// capture (`test_results::SELF_SKIP`) — `cargo perf` runs it.
    #[test]
    fn perf_search_wide_with_test_code() {
        if !cfg!(tarae_optimized) {
            let line = format!(
                "{}nav::tests::perf_search_wide_with_test_code — built at opt-level 0 (the test profile); \
                 `cargo perf` runs it\n",
                crate::test_results::SELF_SKIP
            );
            let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
            return;
        }
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let q = Query {
            pattern: "fn ".into(),
            literal: true,
            case: Case::Sensitive,
            globs: vec!["*.rs".into()],
            context: 0,
            output: Output::Lines,
            offset: 0,
            tests: true,
        };
        let t0 = std::time::Instant::now();
        let text = run_search(root, None, &q, &[]).unwrap();
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        assert!(text.contains("in test code, listed last"), "{}", text.lines().next().unwrap_or_default());
        let line = format!("PERF search_wide_with_test_code_ms {ms:.3} budget=3000 noise=100\n");
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        assert!(ms < 3000.0, "{ms:.0} ms (budget 3000)");
    }

    /// With no server: tarae's outline from the syntax tree (when the grammar is there).
    #[test]
    fn the_outline_stands_in_for_the_server() {
        let text = ropey::Rope::from_str("struct S;\nimpl S {\n    fn f(&self) {\n        1;\n    }\n}\n");
        let Ok(syms) = outline_syms(Path::new("a.rs"), Some(text)) else { return }; // (no Rust grammar)
        assert_eq!(syms, [sym(0, "type", "S", 0, 0), sym(0, "", "impl S", 1, 5), sym(1, "fn", "f", 2, 4)]);
    }
}
