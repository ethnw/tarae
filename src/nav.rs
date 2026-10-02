//! Code navigation for the chat (N5) — tarae's `references` and `search` tools, so Claude finds its way
//! through the code the way the user does, through the editor:
//!
//! - `references` asks the file's language server for a name's references (`g r`'s question): each place
//!   with its line of code, by file then line, the declaration (its definition, asked beside them) first
//!   (N2). While the server starts or loads, or when it says nothing within `REFS_WAIT`, the name's
//!   whole-word text matches instead — said so (N3).
//! - `search` runs the project search `space /` runs (N4 — ripgrep, a regex, smart case): by file then
//!   line, each hit its line of code (and context lines when asked), capped and said so; or the files, or
//!   counts.
//!
//! Both only read, inside the project (`Editor::project_root`) — no card. Disk and searching go to workers.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{Value, json};

use crate::editor::Editor;
use crate::lsp::ClientId;
use crate::lsp_editor::Place;
use crate::tools::{Call, answer};

/// How long a server has to answer before the text matches are the answer.
const REFS_WAIT: std::time::Duration =
    std::time::Duration::from_millis(if cfg!(test) { 300 } else { 20_000 });

/// Rows of places a `references` answer lists at most (the rest summed up per file).
const REFS_SHOWN: usize = 200;

/// `search`: hits listed at most, rows with their context at most, files at most (files/count), and where
/// rg is stopped (the count then says `10000+`).
const HITS_SHOWN: usize = 200;
const ROWS_SHOWN: usize = 400;
const FILES_SHOWN: usize = 300;
const HITS_MAX: usize = 10_000;
const CONTEXT_MAX: u64 = 10;

/// A line's text in an answer, at most (chars).
const LINE_MAX: usize = 240;

/// Files named in the "more not shown" line, at most.
const MORE_NAMED: usize = 12;

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
                "symbol": { "type": "string", "description": "The name as written in the code (`goto_ask`; `Editor::goto_ask` or `x.goto_ask` mean the same)" }
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
    let root = project_root(ed);
    let path = root.join(path);
    ed.events.jobs().spawn(move || {
        let file = file_in(&root, &path);
        move |ed: &mut Editor| match file {
            Ok(path) => ask(ed, call, path, line, word),
            Err(e) => answer(ed, call, Err(e)),
        }
    });
    Ok(())
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
fn ask(ed: &mut Editor, call: Call, path: PathBuf, line: Option<usize>, word: String) {
    let shown = rel(&project_root(ed), &path);
    let id = match ed.docs.iter().find(|d| d.path.as_deref() == Some(path.as_path())) {
        Some(d) => d.id,
        None => {
            let before = ed.current;
            if let Err(e) = ed.open(&path) {
                return answer(ed, call, Err(format!("{shown}: {e:#}")));
            }
            let id = ed.doc().id;
            ed.current = before.min(ed.docs.len() - 1);
            id
        }
    };
    let Some(doc) = ed.docs.iter().find(|d| d.id == id) else { return };
    if doc.loading {
        return text_matches(ed, call, word, format!("{shown} is still loading"), true);
    }
    let (line, col) = match locate(&doc.text, &word, line, &shown) {
        Ok(at) => at,
        Err(e) => return answer(ed, call, Err(e)),
    };
    let byte = doc.text.line_to_byte(line) + col;
    let n = ed.lsp.tool_ask_next;
    ed.lsp.tool_ask_next += 1;
    match ed.ask_references(id, byte, n) {
        Err((why, later)) => text_matches(ed, call, word, why, later),
        Ok((cid, server, declares)) => {
            let declared = (!declares).then(Vec::new);
            let ask = Ask { call, word, server, cid, refs: None, declared };
            ed.lsp.tool_asks.insert(n, ask);
            // A server that says nothing in time: the text matches are the answer
            ed.events.jobs().spawn(move || {
                std::thread::sleep(REFS_WAIT);
                move |ed: &mut Editor| {
                    if let Some(a) = ed.lsp.tool_asks.remove(&n) {
                        let why = format!("{} didn't answer within {}", a.server, secs(REFS_WAIT));
                        text_matches(ed, a.call, a.word, why, true);
                    }
                }
            });
        }
    }
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
    let found = match refs {
        Err(why) => return text_matches(ed, a.call, a.word, why, true),
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
            return text_matches(ed, a.call, a.word, why, later);
        }
        Ok(found) => found,
    };
    let refs = {
        let mut at: Vec<_> = found.iter().map(|f| (&f.0, f.1, f.2)).collect();
        at.sort();
        at.dedup();
        at.len()
    };
    let files = found.iter().map(|f| &f.0).collect::<std::collections::HashSet<_>>().len();
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
    let first = if declared.is_empty() { "" } else { ", the declaration first" };
    let head = format!("{} in {} ({}){first}:", count(refs, "reference"), count(files, "file"), a.server);
    let call = a.call;
    ed.events.jobs().spawn(move || {
        let places = crate::lsp_editor::read_places(all, &open);
        let text = format!("{head}\n{}", refs_list(&root, places, &declared));
        move |ed: &mut Editor| answer(ed, call, Ok(text))
    });
}

/// The places by file then line, the declaration's file first and its row first in it (marked).
fn refs_list(root: &Path, mut places: Vec<Place>, declared: &[(PathBuf, usize)]) -> String {
    let is_decl = |p: &Place| declared.iter().any(|(path, line)| *path == p.path && *line == p.line);
    let decl_file = places.iter().find(|p| is_decl(p)).map(|p| p.path.clone());
    places.sort_by(|a, b| {
        let key =
            |p: &Place| (Some(&p.path) != decl_file.as_ref(), p.path.clone(), !is_decl(p), p.line, p.col);
        key(a).cmp(&key(b))
    });
    grouped(root, places, &|p| is_decl(p).then_some("declaration"), REFS_SHOWN)
}

/// The name's whole-word text matches in the project (case as written), as the answer — `why` the
/// server's references aren't (`later`: they will be, ask again).
fn text_matches(ed: &mut Editor, call: Call, word: String, why: String, later: bool) {
    let root = project_root(ed);
    ed.events.jobs().spawn(move || {
        let r = crate::picker::word_matches(&root, &word).map(|items| {
            let places: Vec<Place> = items
                .into_iter()
                .filter_map(|it| match it.action {
                    crate::picker::Action::Goto { path, line, col } => {
                        let text = it.label.split_once(&format!(":{}: ", line + 1)).map(|(_, t)| t);
                        Some(Place { text: clip(text.unwrap_or_default(), LINE_MAX), path, line, col })
                    }
                    _ => None,
                })
                .collect();
            let files = places.iter().map(|p| &p.path).collect::<std::collections::HashSet<_>>().len();
            let then = if later { " — ask again in a moment for those" } else { "" };
            if places.is_empty() {
                return format!("no text matches of `{word}` in the project either — {why}");
            }
            format!(
                "{} in {} — {why}, so these are the whole-word text matches of `{word}` (case as written), \
                 not its references{then}:\n{}",
                count(places.len(), "text match"),
                count(files, "file"),
                grouped(&root, places, &|_| None, REFS_SHOWN)
            )
        });
        move |ed: &mut Editor| answer(ed, call, r)
    });
}

/// Places in order, a file's name then its lines (`  12: code`, numbers right-aligned in the file) — one
/// row a line; past `max` rows the rest summed up per file.
fn grouped(
    root: &Path,
    places: Vec<Place>,
    mark: &dyn Fn(&Place) -> Option<&'static str>,
    max: usize,
) -> String {
    let mut files: Vec<(PathBuf, Vec<Place>)> = Vec::new();
    for p in places {
        match files.last_mut() {
            Some((f, rows)) if *f == p.path => {
                if rows.last().is_none_or(|r| r.line != p.line) {
                    rows.push(p);
                }
            }
            _ => files.push((p.path.clone(), vec![p])),
        }
    }
    let mut out = String::new();
    let mut shown = 0;
    let mut rest = Vec::new();
    for (path, rows) in files {
        let name = rel(root, &path);
        let take = rows.len().min(max - shown);
        if take > 0 {
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

// ── search ──────────────────────────────────────────────────────────────────

pub fn search_schema() -> Value {
    json!({
        "name": "search",
        "description": "Search the project's files for a regex — the editor's own project search \
    (ripgrep). Use it over Grep for finding code and text in the project. Hits come by file then line, each \
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
                "offset": { "type": "integer", "description": "Skip this many hits (lines) or files — the next page" }
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

#[derive(Clone, Copy, Debug, PartialEq)]
enum Case {
    Smart,
    Sensitive,
    Insensitive,
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
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).min(4);
    let mut args: Vec<String> =
        ["--color=never", "--hidden", "--glob=!.git", "--threads", &threads.to_string()]
            .map(String::from)
            .into();
    args.push(
        match q.case {
            Case::Smart => "--smart-case",
            Case::Sensitive => "--case-sensitive",
            Case::Insensitive => "--ignore-case",
        }
        .into(),
    );
    if q.literal {
        args.push("--fixed-strings".into());
    }
    args.extend(q.globs.iter().map(|g| format!("--glob={g}")));
    match q.output {
        Output::Lines if q.context > 0 => args.extend(["--json".into(), format!("--context={}", q.context)]),
        Output::Lines => args.push("--json".into()),
        Output::Files => args.push("--files-with-matches".into()),
        // (the file's name even when it's the one file searched)
        Output::Count => args.extend(["--count".into(), "--with-filename".into()]),
    }
    args.extend(["-e".into(), q.pattern.clone(), "--".into()]);
    if let Ok(rel) = scope.strip_prefix(&root)
        && !rel.as_os_str().is_empty()
    {
        args.push(rel.to_string_lossy().into_owned());
    }
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
            let warn = rg(&root, &args, &mut |l| {
                let Ok(v) = serde_json::from_slice::<Value>(l) else { return true };
                let hit = match v["type"].as_str() {
                    Some("match") => true,
                    Some("context") => false,
                    _ => return true,
                };
                let d = &v["data"];
                let (Some(path), Some(line)) = (d["path"]["text"].as_str(), d["line_number"].as_u64()) else {
                    return true;
                };
                let text =
                    d["lines"]["text"].as_str().map_or("(not UTF-8)", |t| t.trim_end_matches(['\n', '\r']));
                let row = Row { line: line as usize, hit, text: clip(text, LINE_MAX) };
                files.entry(path.to_string()).or_default().push(row);
                hits += usize::from(hit);
                hits < HITS_MAX
            })?;
            if files.is_empty() {
                return no_hits(warn, q);
            }
            lines_text(files, hits >= HITS_MAX, q.offset) + &warned(warn)
        }
        Output::Files | Output::Count => {
            let mut rows: Vec<String> = Vec::new();
            let warn = rg(&root, &args, &mut |l| {
                rows.push(String::from_utf8_lossy(l).into_owned());
                true
            })?;
            if rows.is_empty() {
                return no_hits(warn, q);
            }
            rows.sort();
            files_text(rows, q.output == Output::Count, q.offset) + &warned(warn)
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

fn warned(warn: Option<String>) -> String {
    warn.map(|w| format!("\n(rg: {w})")).unwrap_or_default()
}

/// Hits by file, each run of lines together (`--` between runs), hit lines `12:`, context `13-` (as rg
/// prints them); from hit `offset` on, at most `HITS_SHOWN` hits and `ROWS_SHOWN` rows.
fn lines_text(files: BTreeMap<String, Vec<Row>>, stopped: bool, offset: usize) -> String {
    let total: usize = files.values().map(|r| r.iter().filter(|r| r.hit).count()).sum();
    let files_n = files.values().filter(|r| r.iter().any(|r| r.hit)).count();
    let plus = if stopped { "+" } else { "" };
    let mut out = String::new();
    let (mut seen, mut hits, mut rows) = (0, 0, 0);
    let mut rest: Vec<(String, usize)> = Vec::new();
    let mut last_shown = 0;
    for (path, file) in files {
        // Runs of adjacent lines
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
    let mut head = format!("{lines} in {}", count(files_n, "file"));
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

/// `--files-with-matches` / `--count` lines, sorted: from `offset` on, at most `FILES_SHOWN`.
fn files_text(rows: Vec<String>, counts: bool, offset: usize) -> String {
    let total = rows.len();
    let shown: Vec<&String> = rows.iter().skip(offset).take(FILES_SHOWN).collect();
    let mut head = if counts {
        let lines: usize = rows.iter().filter_map(|r| r.rsplit_once(':')?.1.parse::<usize>().ok()).sum();
        format!("{} in {}", count(lines, "matching line"), count(total, "file"))
    } else {
        format!("{} with matches", count(total, "file"))
    };
    if shown.is_empty() {
        return format!("{head}; none past offset {offset}");
    }
    let end = offset + shown.len();
    if offset > 0 || end < total {
        head.push_str(&format!(" — {}–{end} below", offset + 1));
    }
    let mut text = format!("{head}:\n");
    for r in shown {
        text.push_str(r);
        text.push('\n');
    }
    if end < total {
        text.push_str(&format!("… {} more — offset: {end} for the next ones", total - end));
    }
    text.truncate(text.trim_end().len());
    text
}

/// rg with `args` in `root`, each line of its output to `each` (false: enough — rg is stopped). Ok(Some):
/// what rg complained about while it still searched (a file it couldn't read, a bad glob).
fn rg(root: &Path, args: &[String], each: &mut dyn FnMut(&[u8]) -> bool) -> Result<Option<String>, String> {
    use std::io::{BufRead, Read};
    let mut child = Command::new("rg")
        .args(args)
        .current_dir(root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("couldn't run rg (ripgrep): {e} — Grep can search instead"))?;
    let err = child.stderr.take().map(|mut e| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = e.read_to_string(&mut s);
            s
        })
    });
    let mut stopped = false;
    if let Some(out) = child.stdout.take() {
        for line in std::io::BufReader::new(out).split(b'\n').map_while(Result::ok) {
            if !each(&line) {
                stopped = true;
                break;
            }
        }
    }
    if stopped {
        let _ = child.kill();
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let err = err.and_then(|h| h.join().ok()).unwrap_or_default();
    let first = err.lines().map(|l| l.trim_start_matches("rg: ")).find(|l| !l.trim().is_empty());
    Ok(match status.code() {
        _ if stopped => None,
        Some(0 | 1) => None,
        _ => Some(first.unwrap_or("rg failed").to_string()),
    })
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
        r"^(\d+\+? (?:references?|text match(?:es)?|matching lines?) in \d+ files?|\d+ files? with matches|no (?:text )?matches)",
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
        let places = vec![
            place("/p/b.rs", 4, "b()"),
            place("/p/a.rs", 2, "a()"),
            place("/p/b.rs", 1, "fn b() {}"),
            place("/p/a.rs", 11, "a(); a()"),
            place("/p/a.rs", 11, "a(); a()"),
        ];
        let text = refs_list(root, places.clone(), &[(PathBuf::from("/p/b.rs"), 1)]);
        assert_eq!(text, "b.rs\n  2: fn b() {}  (declaration)\n  5: b()\na.rs\n   3: a()\n  12: a(); a()");
        let sorted = vec![places[1].clone(), places[3].clone(), places[2].clone(), places[0].clone()];
        let text = grouped(root, sorted.clone(), &|_| None, 1);
        assert_eq!(text, "a.rs\n  3: a()\n… 3 more lines in 2 files not shown: a.rs (1), b.rs (2)");
        let text = grouped(root, sorted, &|_| None, 3);
        assert!(text.ends_with("… 1 more line in 1 file not shown: b.rs (1)"), "{text}");
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
        let rows: Vec<String> = (0..5).map(|i| format!("f{i}:{}", i + 1)).collect();
        assert_eq!(
            files_text(rows.clone(), true, 0),
            "15 matching lines in 5 files:\nf0:1\nf1:2\nf2:3\nf3:4\nf4:5"
        );
        let names: Vec<String> = (0..FILES_SHOWN + 2).map(|i| format!("f{i:03}")).collect();
        let text = files_text(names, false, 0);
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

    fn rg_here() -> bool {
        Command::new("rg").arg("--version").output().is_ok()
    }

    /// The search itself: smart case, a glob, a folder, context, hidden files in and `.gitignore`d out;
    /// outside the project refused; an unsaved buffer said.
    #[test]
    fn search_runs_rg_inside_the_project() {
        if !rg_here() {
            return;
        }
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
        // (rg reads .gitignore only in a git repository)
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        let q = |pattern: &str| Query {
            pattern: pattern.into(),
            literal: false,
            case: Case::Smart,
            globs: Vec::new(),
            context: 0,
            output: Output::Lines,
            offset: 0,
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
        std::fs::remove_dir_all(&dir).ok();
    }
}
