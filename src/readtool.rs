//! The `read` tool (toolcalls, 2026-10-13): Claude reads files in the editor — several files and line ranges
//! in one call, a symbol by name from its syntax tree (`outlines.rs`), a file's end — with no process and,
//! inside the project, no card. It runs in tarae, outside every sandbox, so it keeps Read's rules itself
//! (`reads::Gate`): what Read would ask about (outside the project and its allowed places, a file named like a
//! key) it asks on the same card, and runs again once let through. A big file asked whole comes as its outline
//! with line numbers (Zed's rule), saying how to ask for the text — not a page cut short. What it read is
//! what Claude saw of those files (`Edits.seen` — an `edit` of one changed since says so).

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::editor::Editor;
use crate::tools::{Call, answer};

/// A file asked whole bigger than this (bytes) comes as its outline.
const WHOLE_MAX: usize = 16 * 1024;

/// Lines in one answer at most, and a line's characters.
const LINES_MAX: usize = 1500;
const LINE_MAX: usize = 500;

/// Files and ranges one call asks for at most · symbols one name reads at most · outline rows shown.
const ITEMS_MAX: usize = 30;
const NAMED_MAX: usize = 5;
const OUTLINE_MAX: usize = 300;

/// A file whole asked of one without an outline: its first lines.
const HEAD_LINES: usize = 100;

/// The tool's schema. (It no longer sends Claude to Read before an Edit: Claude Code 2.1.296 with this model
/// edits a file it hasn't read — tests/live.rs `chat_edit_then_native_edit`; older models needed the Read.)
pub fn schema() -> Value {
    json!({
        "name": "read",
        "description": "Read files in the editor — no process, and no card inside the project. Several files \
    and ranges in one call: `src/a.rs:10-40,90-120`, `src/a.rs:-30` (its last 30 lines), `src/a.rs:run_search` \
    or `src/a.rs:Editor::open` (a function, type or module by name, from its syntax tree), or a path alone (the \
    whole file — over 16 KB it comes as its outline with line numbers; then ask for ranges or names). Lines \
    come numbered (`12: …`). Ask for enough in one call to skip another turn. Outside the project it asks the \
    user, as Read does.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "files": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "`path`, `path:a-b,c-d`, `path:-N` or `path:name` — from the project's root, or absolute"
                }
            },
            "required": ["files"]
        }
    })
}

/// What one item asks of its file.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Want {
    Whole,
    /// Line ranges, from 1, both ends in.
    Lines(Vec<(usize, usize)>),
    /// The last N lines.
    Tail(usize),
    Symbol(String),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Item {
    pub(crate) given: String,
    pub(crate) path: String,
    pub(crate) want: Want,
}

/// `path`, `path:10-40,90`, `path:-30`, `path:name` — the `:` after the path's last `/`; what follows that
/// isn't a range, an end or a name leaves the whole as the path. (The chat's row reads it so too.)
pub(crate) fn item(spec: &str) -> Item {
    let spec = spec.trim();
    let whole = || Item { given: spec.into(), path: spec.into(), want: Want::Whole };
    let from = spec.rfind('/').map_or(0, |i| i + 1);
    let Some(at) = spec[from..].find(':').map(|i| from + i) else { return whole() };
    let (path, what) = (&spec[..at], &spec[at + 1..]);
    if path.is_empty() || what.is_empty() {
        return whole();
    }
    let num = |s: &str| s.trim().parse::<usize>().ok().filter(|&n| n > 0);
    let want = if let Some(n) = what.strip_prefix('-').and_then(num) {
        Want::Tail(n)
    } else if let Some(ranges) = what
        .split(',')
        .map(|r| match r.split_once('-') {
            Some((a, b)) => Some((num(a)?, num(b)?)),
            None => num(r).map(|a| (a, a)),
        })
        .collect::<Option<Vec<_>>>()
    {
        Want::Lines(ranges.into_iter().map(|(a, b)| (a.min(b), a.max(b))).collect())
    } else if what.starts_with(|c: char| c.is_alphabetic() || c == '_')
        && what.chars().all(|c| c.is_alphanumeric() || c == '_' || c == ':')
    {
        Want::Symbol(what.into())
    } else {
        return whole();
    };
    Item { given: spec.into(), path: path.into(), want }
}

fn items_of(args: &Value) -> Result<Vec<Item>, String> {
    let files =
        args["files"].as_array().ok_or("files is required: a list of `path`, `path:a-b`, `path:name`")?;
    let specs: Vec<&str> = files.iter().filter_map(Value::as_str).filter(|s| !s.trim().is_empty()).collect();
    if specs.is_empty() {
        return Err("files is empty — give `path`, `path:a-b`, `path:-N` or `path:name`".into());
    }
    if specs.len() > ITEMS_MAX {
        return Err(format!("{} items — {ITEMS_MAX} at most in one call", specs.len()));
    }
    Ok(specs.into_iter().map(item).collect())
}

/// The tool: the items checked now, read on a worker.
pub fn call(ed: &mut Editor, call: Call, args: &Value) -> Result<(), String> {
    let items = items_of(args)?;
    go(ed, call, items, args.clone(), Vec::new());
    Ok(())
}

/// A read that waited on the read card, run again with what it let through (`reads::Then`).
pub fn again(ed: &mut Editor, t: crate::reads::Then) {
    match items_of(&t.args) {
        Ok(items) => go(ed, t.call, items, t.args, t.granted),
        Err(e) => answer(ed, t.call, Err(e)),
    }
}

fn go(ed: &mut Editor, call: Call, items: Vec<Item>, args: Value, granted: Vec<PathBuf>) {
    let root = crate::nav::project_root(ed);
    let unsaved: Vec<PathBuf> =
        ed.docs.iter().filter(|d| d.is_modified()).filter_map(|d| d.path.clone()).collect();
    let gate = crate::reads::Gate::of(ed, granted.clone());
    ed.events.jobs().spawn(move || {
        // (taken before it's read: a change while it reads counts as one since)
        let stamps: Vec<(PathBuf, Option<crate::disk::Stamp>)> = items
            .iter()
            .filter_map(|it| std::fs::canonicalize(crate::chat::resolve(&root, &it.path)).ok())
            .map(|p| {
                let s = crate::disk::stamp(&p);
                (p, s)
            })
            .collect();
        let r = read(&root, &items, &gate, &unsaved);
        move |ed: &mut Editor| match r {
            Ok(Ok(text)) => {
                for (p, s) in stamps {
                    crate::edittool::saw(ed, &p, s);
                }
                answer(ed, call, Ok(text))
            }
            Ok(Err(ask)) => {
                let then = crate::reads::Then { call, tool: "read", args, granted };
                crate::reads::tool_ask(ed, then, *ask)
            }
            Err(e) => answer(ed, call, Err(e)),
        }
    });
}

/// The answer: each item's lines under its heading (`== path:a-b (of N lines)`), or what's said instead —
/// Ok(Err): a file Read would ask about, the card's question (the whole call waits on it). A worker's.
fn read(
    root: &Path,
    items: &[Item],
    gate: &crate::reads::Gate,
    unsaved: &[PathBuf],
) -> Result<Result<String, Box<crate::reads::Ask>>, String> {
    read_between(root, items, gate, unsaved, &|| {})
}

/// What an item found: a file opened (read from that descriptor — never by its path again), a folder,
/// nothing there.
enum Found {
    File(crate::paths::Opened),
    Folder,
    Missing,
}

/// `read`, `between` run after every item was judged and before any is read (tests: what a run writing the
/// project could do in between).
fn read_between(
    root: &Path,
    items: &[Item],
    gate: &crate::reads::Gate,
    unsaved: &[PathBuf],
    between: &dyn Fn(),
) -> Result<Result<String, Box<crate::reads::Ask>>, String> {
    let root = std::fs::canonicalize(root).map_err(|e| e.to_string())?;
    // Every item opened and judged before any is read: one card, then the whole answer. What's judged is what
    // was opened — the descriptor's own path (`paths::open_read`), and it's read from that descriptor: a
    // folder on the path swapped for a link meanwhile can't send the read somewhere never judged. Outside the
    // project the place is judged before anything is said of it — whether it's there, a file or a folder, is
    // read too (Read asks first as well); inside, a file named like a key.
    let mut found: Vec<Found> = Vec::new();
    for it in items {
        let abs = root.join(&it.path);
        let (judged, f) = match crate::paths::open_read(&abs) {
            Ok(o) => {
                let inside = o.real.starts_with(&root);
                let judged = (!inside || o.meta.is_file()).then(|| (o.real.clone(), !o.meta.is_dir()));
                let f = match () {
                    _ if o.meta.is_file() => Found::File(o),
                    _ if o.meta.is_dir() => Found::Folder,
                    _ => Found::Missing,
                };
                (judged, f)
            }
            // (nothing opened: the place by its path — a `..` past what's there can't be told where it leads,
            // the card says the path as given)
            Err(_) => match crate::paths::real(&abs) {
                Some(r) if r.starts_with(&root) => (None, Found::Missing),
                Some(r) => (Some((r, true)), Found::Missing),
                None => (Some((abs, true)), Found::Missing),
            },
        };
        if let Some((at, file)) = judged
            && let Err(ask) = gate.judge(&at, file)
        {
            return Ok(Err(ask));
        }
        found.push(f);
    }
    between();
    let mut out = String::new();
    let mut left = LINES_MAX;
    let mut dirty = Vec::new();
    for (it, f) in items.iter().zip(found) {
        let o = match f {
            Found::File(o) => o,
            Found::Folder => {
                out.push_str(&format!("== {}: a folder — search with output files lists it\n", it.given));
                continue;
            }
            Found::Missing => {
                out.push_str(&format!("== {}: no such file\n", it.given));
                continue;
            }
        };
        let shown = crate::nav::rel(&root, &o.real);
        if unsaved.contains(&o.real) {
            dirty.push(shown.clone());
        }
        out.push_str(&section(o, &shown, it, &mut left));
    }
    if !dirty.is_empty() {
        out.push_str(&format!(
            "(unsaved edits in {} aren't here — what's here is from disk)\n",
            dirty.join(", ")
        ));
    }
    Ok(Ok(out.trim_end().to_string()))
}

/// One item's part of the answer, read from its opened file, at most `left` lines of text (taken from it).
fn section(mut o: crate::paths::Opened, shown: &str, it: &Item, left: &mut usize) -> String {
    use std::io::Read;
    let mut bytes = Vec::new();
    if let Err(e) = o.file.read_to_end(&mut bytes) {
        return format!("== {shown}: {e}\n");
    }
    let (real, stamp) = (o.real.as_path(), crate::testcode::stamp_of(&o.meta));
    if bytes.contains(&0) {
        return format!("== {shown}: binary — not read\n");
    }
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len();
    let of = format!("of {n} line{}", if n == 1 { "" } else { "s" });
    let ranges: Vec<(usize, usize, String)> = match &it.want {
        Want::Whole if bytes.len() > WHOLE_MAX => {
            return outline(crate::outlines::of_text(real, stamp, &text), shown, &lines, bytes.len(), left);
        }
        Want::Whole => vec![(1, n, format!("== {shown} ({of})"))],
        Want::Tail(k) => {
            let a = n.saturating_sub(*k) + 1;
            vec![(a, n, format!("== {shown}:{a}-{n} ({of})"))]
        }
        Want::Lines(rs) => {
            rs.iter().map(|&(a, b)| (a, b.min(n), format!("== {shown}:{a}-{} ({of})", b.min(n)))).collect()
        }
        Want::Symbol(name) => match crate::outlines::of_text(real, stamp, &text) {
            Err(e) => return format!("== {shown}:{name}: no symbols — {e}; ask for line ranges\n"),
            Ok(syms) => {
                let found = crate::outlines::named(&syms, name);
                if found.is_empty() {
                    let names: Vec<&str> =
                        syms.iter().filter(|s| s.depth == 0).map(|s| s.name.as_str()).take(30).collect();
                    return format!(
                        "== {shown}:{name}: no symbol named {name} — at its top level: {}; `{shown}` alone gives its outline\n",
                        names.join(", ")
                    );
                }
                found
                    .iter()
                    .take(NAMED_MAX)
                    .map(|s| {
                        (s.lines.0 + 1, s.lines.1 + 1, format!("== {shown}:{name} — {} ({of})", s.said()))
                    })
                    .collect()
            }
        },
    };
    let mut out = String::new();
    for (a, b, head) in ranges {
        out.push_str(&head);
        out.push('\n');
        if a > b || a > n {
            out.push_str(&format!("  (past its end — it has {n} lines)\n"));
            continue;
        }
        let take = (b - a + 1).min(*left);
        for (i, l) in lines[a - 1..a - 1 + take].iter().enumerate() {
            out.push_str(&format!("{}: {}\n", a + i, crate::nav::clip(l, LINE_MAX)));
        }
        *left -= take;
        if take < b - a + 1 {
            let from = a + take;
            out.push_str(&format!("… cut at {LINES_MAX} lines an answer — `{shown}:{from}-{b}` reads on\n"));
        }
    }
    out
}

/// A big file asked whole: its outline with line numbers and how to ask for the text — or, with no outline
/// (no grammar for it), its first lines and the same.
fn outline(
    syms: Result<std::sync::Arc<Vec<crate::nav::Sym>>, String>,
    shown: &str,
    lines: &[&str],
    bytes: usize,
    left: &mut usize,
) -> String {
    let n = lines.len();
    let ask = format!(
        "The text isn't here — ask for `{shown}:a-b` line ranges or `{shown}:name` symbols (several in one call)."
    );
    let kb = bytes.div_ceil(1024);
    match syms {
        Ok(syms) if !syms.is_empty() => {
            let mut out = format!("== {shown} — {n} lines, {kb} KB: its outline. {ask}\n");
            for s in syms.iter().take(OUTLINE_MAX) {
                let (a, b) = (s.lines.0 + 1, s.lines.1 + 1);
                let kind = if s.kind.is_empty() { String::new() } else { format!("{} ", s.kind) };
                out.push_str(&format!("{}{kind}{}  {a}–{b}\n", "  ".repeat(s.depth), s.name));
            }
            if syms.len() > OUTLINE_MAX {
                out.push_str(&format!(
                    "… {} more symbols — `{shown}:a-b` for a part\n",
                    syms.len() - OUTLINE_MAX
                ));
            }
            out
        }
        _ => {
            let take = HEAD_LINES.min(n).min(*left);
            *left -= take;
            let mut out = format!("== {shown}:1-{take} (of {n} lines, {kb} KB — no outline for it). {ask}\n");
            for (i, l) in lines[..take].iter().enumerate() {
                out.push_str(&format!("{}: {}\n", i + 1, crate::nav::clip(l, LINE_MAX)));
            }
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_an_item_asks() {
        let it = |s: &str| item(s).want;
        assert_eq!(it("src/a.rs"), Want::Whole);
        assert_eq!(it("src/a.rs:10-40,90"), Want::Lines(vec![(10, 40), (90, 90)]));
        assert_eq!(it("src/a.rs:40-10"), Want::Lines(vec![(10, 40)]), "either way round");
        assert_eq!(it("src/a.rs:-30"), Want::Tail(30));
        assert_eq!(it("src/a.rs:run_search"), Want::Symbol("run_search".into()));
        assert_eq!(item("src/a.rs:Editor::open").path, "src/a.rs");
        assert_eq!(it("src/a.rs:Editor::open"), Want::Symbol("Editor::open".into()));
        assert_eq!(it("notes:2026/x.md"), Want::Whole, "a `:` before the last `/` is the path's");
        assert_eq!(it("a.rs:1-x"), Want::Whole, "neither a range nor a name: the whole is the path");
    }

    /// A project, a place outside it, and a link in the project to that place — under the test's own temp
    /// folder. (`dir`, the project's real path, outside's.)
    fn places(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("tarae-readtool-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (dir, outside) = (base.join("p"), base.join("outside"));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(
            dir.join("src/a.rs"),
            "struct Thing;\n\nimpl Thing {\n    fn run(&self) {\n        go();\n    }\n}\n\nfn go() {}\n",
        )
        .unwrap();
        let big: String = (0..800).map(|i| format!("fn f{i}() {{\n    let x = {i};\n}}\n")).collect();
        std::fs::write(dir.join("src/big.rs"), big).unwrap();
        std::fs::write(dir.join(".env"), "API_KEY=tarae-fake-token\n").unwrap();
        std::fs::write(outside.join("o.txt"), "outside marker\n").unwrap();
        std::os::unix::fs::symlink(outside.join("o.txt"), dir.join("src/link.txt")).unwrap();
        let real = |p: &Path| crate::paths::real(p).unwrap();
        (real(&dir), real(&outside), base)
    }

    fn text(r: Result<Result<String, Box<crate::reads::Ask>>, String>) -> String {
        match r {
            Ok(Ok(t)) => t,
            Ok(Err(ask)) => panic!("asks the user about {}", ask.at_said()),
            Err(e) => panic!("{e}"),
        }
    }

    fn items(specs: &[&str]) -> Vec<Item> {
        specs.iter().map(|s| item(s)).collect()
    }

    /// Ranges, a file's end, a symbol by its type's name, several in one answer, numbered; a big file asked
    /// whole is its outline, saying how to ask for the text; what isn't a file is said in its place.
    #[test]
    fn reads_ranges_ends_symbols_and_outlines_big_files() {
        let (dir, _, base) = places("what");
        let gate = crate::reads::Gate::for_test(&dir);
        let t = text(read(&dir, &items(&["src/a.rs:2-3", "src/a.rs:-1", "src/nope.rs", "src"]), &gate, &[]));
        assert!(t.starts_with("== src/a.rs:2-3 (of 9 lines)\n2: \n3: impl Thing {\n"), "{t}");
        assert!(t.contains("== src/a.rs:9-9 (of 9 lines)\n9: fn go() {}\n"), "{t}");
        assert!(t.contains("== src/nope.rs: no such file\n"), "{t}");
        assert!(t.contains("== src: a folder"), "{t}");
        if crate::outlines::of(&dir.join("src/a.rs")).is_err() {
            return std::fs::remove_dir_all(&base).unwrap(); // (no Rust grammar in this build)
        }
        let t = text(read(&dir, &items(&["src/a.rs:Thing::run", "src/a.rs:nothing"]), &gate, &[]));
        assert!(
            t.starts_with("== src/a.rs:Thing::run — fn run (4–6) (of 9 lines)\n4:     fn run(&self) {\n"),
            "{t}"
        );
        assert!(t.contains("no symbol named nothing — at its top level: Thing, impl Thing, go"), "{t}");
        let t = text(read(&dir, &items(&["src/big.rs"]), &gate, &[]));
        assert!(t.starts_with("== src/big.rs — 2400 lines, "), "{t}");
        assert!(t.contains("its outline. The text isn't here — ask for `src/big.rs:a-b` line ranges or `src/big.rs:name` symbols"), "{t}");
        assert!(t.contains("\nfn f10  31–33\n"), "with its line numbers: {t}");
        assert!(!t.contains("let x"), "no text: {t}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Read's rules, kept by the tool itself (it runs outside every sandbox): a file outside the project, a
    /// file named like a key in it, and a link in it to a place outside each ask the user — the link as
    /// where it leads; what the card let through is read.
    #[test]
    fn what_read_would_ask_about_asks() {
        let (dir, outside, base) = places("rules");
        let gate = crate::reads::Gate::for_test(&dir);
        let asks = |specs: &[&str], gate: &crate::reads::Gate| match read(&dir, &items(specs), gate, &[]) {
            Ok(Err(ask)) => ask.at_said(),
            other => panic!("{:?}", other.map(|r| r.map_err(|a| a.at_said()))),
        };
        let o = outside.join("o.txt");
        assert_eq!(asks(&[o.to_str().unwrap()], &gate), o.display().to_string(), "outside");
        assert_eq!(asks(&["src/link.txt"], &gate), o.display().to_string(), "a link in it: where it leads");
        assert_eq!(asks(&[".env"], &gate), dir.join(".env").display().to_string(), "named like a key");
        // Whether a place outside is there, or a folder, is read too: asked before it's said
        let nope = outside.join("nope.txt");
        assert_eq!(asks(&[nope.to_str().unwrap()], &gate), nope.display().to_string(), "missing, outside");
        assert_eq!(
            asks(&[outside.to_str().unwrap()], &gate),
            outside.display().to_string(),
            "a folder outside"
        );
        let t = text(read(&dir, &items(&["src/nope.rs"]), &gate, &[]));
        assert_eq!(t, "== src/nope.rs: no such file", "missing in the project: said");
        assert_eq!(
            asks(&["src/a.rs:1-2", "src/link.txt"], &gate),
            o.display().to_string(),
            "one item asking: the whole call waits, nothing read before"
        );
        let let_through = crate::reads::Gate::for_test(&dir).granting(o.clone());
        let t = text(read(&dir, &items(&["src/link.txt"]), &let_through, &[]));
        assert!(t.ends_with("1: outside marker"), "{t}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// cardauto D3: outside home is read unasked, as runs read it — the system's files here — but not a
    /// mounted volume, nor a file named like a key there, nor anything with the switch off (tests' own:
    /// `Editor.read_outside`).
    #[test]
    fn outside_home_reads_unasked_but_volumes_and_keys() {
        let (dir, _, base) = places("outside-home");
        let gate = crate::reads::Gate::for_test(&dir).reading_outside();
        let hosts = std::fs::canonicalize("/etc/hosts").unwrap();
        let t = text(read(&dir, &items(&[hosts.to_str().unwrap()]), &gate, &[]));
        assert!(t.starts_with(&format!("== {}", hosts.display())), "read: {t}");
        let asks = |spec: &str, gate: &crate::reads::Gate| match read(&dir, &items(&[spec]), gate, &[]) {
            Ok(Err(ask)) => ask.at_said(),
            other => panic!("{spec}: {:?}", other.map(|r| r.map_err(|a| a.at_said()))),
        };
        // (named like a key: asked wherever it is — not there, so nothing's read either way)
        let key = hosts.with_file_name(".env");
        assert_eq!(asks(key.to_str().unwrap(), &gate), key.display().to_string());
        if let Some(v) = ["/Volumes", "/mnt", "/media"].iter().find(|v| Path::new(v).is_dir()) {
            assert_eq!(asks(v, &gate), *v, "a volume");
        }
        let off = crate::reads::Gate::for_test(&dir);
        assert_eq!(asks(hosts.to_str().unwrap(), &off), hosts.display().to_string(), "off: asks");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// What was judged is what's read: a folder on the path swapped for a link to a place outside after the
    /// judging (a run writing the project can) doesn't send the read there — it reads from the descriptor
    /// that was judged.
    #[test]
    fn a_folder_swapped_for_a_link_after_judging_isnt_followed() {
        let (dir, outside, base) = places("swap");
        std::fs::write(dir.join("src/s.txt"), "inside marker\n").unwrap();
        std::fs::create_dir_all(outside.join("src")).unwrap();
        std::fs::write(outside.join("src/s.txt"), "outside marker\n").unwrap();
        let gate = crate::reads::Gate::for_test(&dir);
        let swap = || {
            std::fs::rename(dir.join("src"), dir.join("src.old")).unwrap();
            std::os::unix::fs::symlink(outside.join("src"), dir.join("src")).unwrap();
        };
        let t = text(read_between(&dir, &items(&["src/s.txt"]), &gate, &[], &swap));
        assert!(t.ends_with("1: inside marker"), "{t}");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Calls waiting on the read card are always answered: `a` for the chat lets the one queued behind the
    /// card through as well (each run again, answered with its text); at the turn's end the one queued is
    /// answered at once and the one on the card when its card is found gone (`asking_sync`) — a call left
    /// waiting would mark its session stuck.
    #[test]
    fn calls_waiting_on_the_card_are_always_answered() {
        let (dir, outside, base) = places("queued");
        std::fs::write(outside.join("p.txt"), "second marker\n").unwrap();
        let elsewhere = base.join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        for f in ["q.txt", "r.txt"] {
            std::fs::write(elsewhere.join(f), "marker\n").unwrap();
        }
        let elsewhere = crate::paths::real(&elsewhere).unwrap();
        let mut ed = Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        ed.session.chat = Some(crate::chat::Chat::new());
        let generation = ed.session.chat.as_ref().unwrap().generation();
        let settle = |ed: &mut Editor, done: &dyn Fn(&Editor) -> bool| {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !done(ed) {
                let left = end.saturating_duration_since(std::time::Instant::now());
                let ev = ed.events.recv_timeout(left).expect("settled before the timeout");
                ed.handle_event(ev);
            }
        };
        let card = |ed: &Editor| {
            ed.offers.iter().find_map(|o| match &o.what {
                crate::offer::What::Read { pending, .. } => Some((o.id, (**pending).clone())),
                _ => None,
            })
        };
        let queued = |ed: &Editor| ed.session.chat.as_ref().map_or(0, |c| c.reads.len());
        let answered = |id: u64| {
            crate::tools::ANSWERED
                .with(|a| a.borrow().iter().find(|(k, _)| *k == json!(id)).map(|(_, r)| r.clone()))
        };
        let ask = |ed: &mut Editor, n: u64, f: &Path| {
            call(ed, Call::of(generation, "", json!(n)), &json!({ "files": [f.display().to_string()] }))
                .unwrap();
        };
        // `a`: the folder for this chat — the one queued behind goes through without a card
        ask(&mut ed, 911, &outside.join("o.txt"));
        ask(&mut ed, 912, &outside.join("p.txt"));
        // (the card offers their folder once it's been looked through for tokens — `reads::refold`; answered
        // by its own key, so the call is the answer's, not a card gone — `card_answered`)
        let folder = |ed: &Editor| {
            ed.offers.iter().any(|o| matches!(&o.what, crate::offer::What::Read { folder: Some(_), .. }))
        };
        settle(&mut ed, &|ed| folder(ed) && queued(ed) == 1);
        ed.offer_for_chat();
        settle(&mut ed, &|_| answered(911).is_some() && answered(912).is_some());
        assert!(answered(911).unwrap().unwrap().ends_with("1: outside marker"));
        assert!(answered(912).unwrap().unwrap().ends_with("1: second marker"));
        // The turn ends with one on the card and one behind it
        ask(&mut ed, 913, &elsewhere.join("q.txt"));
        ask(&mut ed, 914, &elsewhere.join("r.txt"));
        settle(&mut ed, &|ed| card(ed).is_some() && queued(ed) == 1);
        // (which one is on the card: the first judged — the two are judged on workers, either may be)
        let on_card = card(&ed).map(|(_, p)| p.input.to_string()).unwrap_or_default();
        let (up, behind) = if on_card.contains("q.txt") { (913, 914) } else { (914, 913) };
        crate::reads::turn_over(&mut ed);
        let ended = answered(behind).expect("the one queued: answered at once");
        assert_eq!(ended, Err("the turn ended before the user answered".to_string()));
        ed.sessions_sync();
        settle(&mut ed, &|_| answered(up).is_some());
        assert_eq!(answered(up).unwrap(), Err(crate::offer::LOST.to_string()), "the one on the card");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// The whole way: a read outside the project waits on Read's card holding its call; `y` runs it again
    /// and answers with the text, `n` answers no — the call answered either way.
    #[test]
    fn a_read_outside_waits_on_the_card_and_is_answered() {
        let (dir, outside, base) = places("card");
        let mut ed = Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        ed.session.chat = Some(crate::chat::Chat::new());
        let generation = ed.session.chat.as_ref().unwrap().generation();
        let o = outside.join("o.txt");
        let settle = |ed: &mut Editor, done: &dyn Fn(&Editor) -> bool| {
            let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while !done(ed) {
                let left = end.saturating_duration_since(std::time::Instant::now());
                let ev = ed.events.recv_timeout(left).expect("settled before the timeout");
                ed.handle_event(ev);
            }
        };
        let card = |ed: &Editor| {
            ed.offers.iter().find_map(|o| match &o.what {
                crate::offer::What::Read { pending, .. } => Some((o.id, (**pending).clone())),
                _ => None,
            })
        };
        let answered = |id: u64| {
            crate::tools::ANSWERED
                .with(|a| a.borrow().iter().find(|(k, _)| *k == json!(id)).map(|(_, r)| r.clone()))
        };
        for (n, yes) in [(901, true), (902, false)] {
            let args = json!({ "files": [o.display().to_string()] });
            call(&mut ed, Call::of(generation, "", json!(n)), &args).unwrap();
            settle(&mut ed, &|ed| card(ed).is_some());
            let (_, pending) = card(&ed).unwrap();
            assert!(pending.then.is_some(), "the card holds the call");
            // (by the card's own keys: the call is the answer's then, not a card gone — `card_answered`)
            if yes {
                ed.offer_accept()
            } else {
                ed.offer_dismiss()
            }
            settle(&mut ed, &|_| answered(n).is_some());
            match (yes, answered(n).unwrap()) {
                (true, Ok(t)) => assert!(t.ends_with("1: outside marker"), "{t}"),
                (false, Err(e)) => assert!(e.starts_with("the user chose not to let you read"), "{e}"),
                (_, r) => panic!("{r:?}"),
            }
        }
        std::fs::remove_dir_all(&base).unwrap();
    }
}
