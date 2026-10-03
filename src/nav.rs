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
//!
//! Both only read, inside the project (`Editor::project_root`) — no card. Disk and searching go to workers.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::editor::Editor;
use crate::grep::Case;
use crate::lsp::ClientId;
use crate::lsp_editor::Place;
use crate::tools::{Call, answer};

/// How long a server has to answer before the text matches are the answer.
const REFS_WAIT: std::time::Duration =
    std::time::Duration::from_millis(if cfg!(test) { 300 } else { 20_000 });

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
fn file_in(root: &Path, path: &Path) -> Result<PathBuf, String> {
    let shown = rel(root, path);
    let real = std::fs::canonicalize(path).map_err(|e| format!("{shown}: {e}"))?;
    let root = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
    if !real.starts_with(&root) {
        return Err(format!("{shown} is outside the project — references looks inside it only"));
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
        None => (0..lines).find_map(find).ok_or_else(|| format!("`{word}` isn't in {shown}")),
    }
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
            ed.events.jobs().spawn(move || {
                std::thread::sleep(REFS_WAIT);
                move |ed: &mut Editor| {
                    if let Some(a) = ed.lsp.tool_asks.remove(&n) {
                        let why = format!("{} didn't answer within {}", a.server, secs(REFS_WAIT));
                        text_matches(ed, a.call, Name { word: a.word, tests: a.tests }, why, true);
                    }
                }
            });
        }
    }
}

/// `path`'s buffer — opened aside when it isn't (its language server starts with it; the file in view
/// stays in view).
fn buffer_for(ed: &mut Editor, path: &Path) -> Result<crate::document::DocId, String> {
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
    format!("{head}\n{}", grouped(root, places, &|p| is_decl(p).then_some("declaration"), REFS_SHOWN))
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
    mark: &dyn Fn(&Place) -> Option<&'static str>,
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
            ed.events.jobs().spawn(move || {
                std::thread::sleep(REFS_WAIT);
                move |ed: &mut Editor| {
                    if let Some(a) = ed.lsp.tool_symbols.remove(&n) {
                        let why = format!("{} didn't answer within {}", a.server, secs(REFS_WAIT));
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
fn project_root(ed: &Editor) -> PathBuf {
    ed.project_root.clone().unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

/// `path` relative to `root` when inside it.
fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

/// `n` and the noun, plural when it isn't one (`1 file`, `3 files`, `2 text matches`).
fn count(n: usize, noun: &str) -> String {
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
fn clip(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn secs(d: std::time::Duration) -> String {
    crate::term::duration(d.as_secs_f32())
}

/// A row's verdict once its answer is in (`12 references in 3 files`, `no matches for …`) — None for what
/// isn't one of these tools' answers.
pub fn verdict(text: &str) -> Option<String> {
    let first = text.lines().next()?;
    let re = regex::Regex::new(
        r"^(\d+\+? (?:references?|text match(?:es)?|matching lines?) in \d+ files?|\d+ files? with matches|\d+ symbols?|no (?:text )?matches)",
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
