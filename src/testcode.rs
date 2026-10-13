//! What's test code (N5b) — `references` and `search` list it apart (`nav.rs`). Any language: test folders
//! and test files by their usual names (`tests/`, `__tests__/`, `foo_test.go`, `test_foo.py`, `a.test.ts`,
//! `FooTest.java` …). Rust: what `#[cfg(test)]` or `#[test]` stands over — a module, a function — and a whole
//! file taken in by a `#[cfg(test)] mod …;` (`term.rs` takes in `term_snapshots.rs` so). Reads and parses
//! files: on workers.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use tree_sitter::Node;

/// How much of a file is test code.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Part {
    Whole,
    /// These lines (from 0).
    Lines(Vec<Range<usize>>),
}

impl Part {
    fn has(&self, line: usize) -> bool {
        match self {
            Part::Whole => true,
            Part::Lines(r) => r.iter().any(|r| r.contains(&line)),
        }
    }
}

/// Files looked at once each, in one answer's making.
pub struct Tests {
    root: PathBuf,
    files: HashMap<PathBuf, Part>,
    /// What each Rust file's parse found · folders' `.rs` files — each once an answer.
    found: HashMap<PathBuf, Found>,
    dirs: HashMap<PathBuf, Vec<PathBuf>>,
    parser: Option<tree_sitter::Parser>,
    /// Rust's, for parsers of other threads (`prefetch`).
    language: Option<tree_sitter::Language>,
    /// Files parsed for it, and their bytes (a search's `timing` line says them).
    pub parsed: usize,
    pub parsed_bytes: usize,
}

/// What one parse of a Rust file finds: the lines test attributes stand over, and the modules it declares for
/// tests only (`#[cfg(test)] mod name;`).
#[derive(Clone, Debug, Default, PartialEq)]
struct Found {
    lines: Vec<Range<usize>>,
    decls: Vec<Decl>,
}

/// A file as it was when parsed: its modified time and length.
pub(crate) type Stamp = (SystemTime, u64);

/// What parsing a file found, kept across answers while the file is unchanged — no trees. Claude's searches
/// hit the same files again and again, and parsing them was all of a search's time: a search hitting most of
/// this repository took 1.7 s, 24 ms of it the walk, the rest 233 files parsed for their test lines (10 MB)
/// and most of them again for their test-only modules (measured 2026-10-13).
static PARSED: Mutex<Option<HashMap<PathBuf, (Stamp, Found)>>> = Mutex::new(None);

/// Files kept at most — past it the table starts over.
const PARSED_MAX: usize = 8192;

/// A file changed this recently isn't kept: another change within its modified time's tick would keep its
/// stamp (git's racy rule; `gitrepo/held.rs` waits 1 s too).
const RACY: Duration = Duration::from_secs(2);

/// Threads parsing at most (`grep::THREADS`'s reason: more contend opening files).
const THREADS: usize = 4;

/// A file's stamp — None: it can't be told, or it changed too recently to keep what it holds.
pub(crate) fn stamp(path: &Path) -> Option<Stamp> {
    stamp_of(&std::fs::metadata(path).ok()?)
}

/// The stamp of what `m` describes (an opened file's own — `paths::open_read`).
pub(crate) fn stamp_of(m: &std::fs::Metadata) -> Option<Stamp> {
    let at = m.modified().ok()?;
    let settled = SystemTime::now().duration_since(at).is_ok_and(|age| age >= RACY);
    settled.then_some((at, m.len()))
}

/// `path`'s text (a real path — one judged or walked) as opened (`paths::open_read`), when what was opened is
/// that very file — one swapped for a link to elsewhere since the path was found isn't read.
pub(crate) fn read_opened(path: &Path) -> Option<String> {
    use std::io::Read;
    let mut o = crate::paths::open_read(path).ok()?;
    if o.real != path || !o.meta.is_file() {
        return None;
    }
    let mut text = String::new();
    o.file.read_to_string(&mut text).ok()?;
    Some(text)
}

fn kept(path: &Path, stamp: Option<Stamp>) -> Option<Found> {
    let parsed = PARSED.lock().unwrap_or_else(|p| p.into_inner());
    parsed.as_ref()?.get(path).filter(|(s, _)| Some(*s) == stamp).map(|(_, f)| f.clone())
}

fn keep(path: &Path, stamp: Option<Stamp>, found: &Found) {
    let Some(stamp) = stamp else { return };
    let mut parsed = PARSED.lock().unwrap_or_else(|p| p.into_inner());
    let table = parsed.get_or_insert_with(HashMap::new);
    if table.len() >= PARSED_MAX {
        table.clear();
    }
    table.insert(path.to_path_buf(), (stamp, found.clone()));
}

/// (tests: as after a start — nothing kept)
#[cfg(test)]
pub fn forget_parsed() {
    *PARSED.lock().unwrap_or_else(|p| p.into_inner()) = None;
}

/// A Rust file parsed with `parser` — what it found, and the file's length (0: not parsed: no `test` in it).
fn parse(parser: &mut tree_sitter::Parser, path: &Path) -> (Found, usize) {
    let Some(src) = read_opened(path) else { return (Found::default(), 0) };
    if !src.contains("test") {
        return (Found::default(), 0);
    }
    let Some(tree) = parser.parse(&src, None) else { return (Found::default(), 0) };
    let mut found = Found::default();
    test_items(tree.root_node(), &src, &mut found.lines);
    if src.contains("mod ") {
        found.decls = decls(tree.root_node(), &src);
    }
    (found, src.len())
}

impl Tests {
    /// For files under `root` (the project: path names are read from there down).
    pub fn new(root: &Path) -> Tests {
        let rust = crate::syntax::spec("rust").and_then(|s| crate::syntax::Loader::global().load(s).ok());
        let language = rust.map(|l| l.language.clone());
        let parser = language.as_ref().and_then(|l| {
            let mut p = tree_sitter::Parser::new();
            p.set_language(l).ok().map(|_| p)
        });
        Tests {
            root: root.to_path_buf(),
            files: HashMap::new(),
            found: HashMap::new(),
            dirs: HashMap::new(),
            parser,
            language,
            parsed: 0,
            parsed_bytes: 0,
        }
    }

    /// Makes ready what `at` will ask of `paths`: the Rust files it parses — each of them, and the files that
    /// could take one in for tests — that aren't kept are parsed on up to `THREADS` threads, each its own
    /// parser.
    pub fn prefetch(&mut self, paths: &[PathBuf]) {
        if self.parser.is_none() {
            return;
        }
        let mut needed: Vec<PathBuf> = Vec::new();
        for path in paths {
            let rel = path.strip_prefix(&self.root).unwrap_or(path);
            if path.extension().is_none_or(|x| x != "rs") || by_name(rel) {
                continue;
            }
            needed.push(path.clone());
            if let Some((_, above)) = self.above(path) {
                needed.extend(above);
            }
        }
        crate::sorting::sort(&mut needed);
        needed.dedup();
        let mut todo: Vec<(PathBuf, Option<Stamp>)> = Vec::new();
        for path in needed {
            if self.found.contains_key(&path) {
                continue;
            }
            let s = stamp(&path);
            match kept(&path, s) {
                Some(found) => {
                    self.found.insert(path, found);
                }
                None => todo.push((path, s)),
            }
        }
        let Some(language) = self.language.clone() else { return };
        let threads = std::thread::available_parallelism().map_or(THREADS, |n| n.get()).min(THREADS);
        let threads = threads.min(todo.len().div_ceil(8));
        if threads < 2 {
            return; // (a few: parsed one by one as they're asked)
        }
        let next = AtomicUsize::new(0);
        let done: Vec<(usize, Found, usize)> = std::thread::scope(|s| {
            let work = || {
                let mut parser = tree_sitter::Parser::new();
                let mut out = Vec::new();
                if parser.set_language(&language).is_err() {
                    return out;
                }
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some((path, _)) = todo.get(i) else { break };
                    let (found, bytes) = parse(&mut parser, path);
                    out.push((i, found, bytes));
                }
                out
            };
            let handles: Vec<_> =
                (0..threads).map(|_| crate::event::scoped(s, "tarae-testcode", work)).collect();
            handles.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        });
        for (i, found, bytes) in done {
            let (path, s) = &todo[i];
            keep(path, *s, &found);
            self.counted(bytes);
            self.found.insert(path.clone(), found);
        }
    }

    fn counted(&mut self, bytes: usize) {
        if bytes > 0 {
            self.parsed += 1;
            self.parsed_bytes += bytes;
        }
    }

    /// What `path`'s parse found — kept, or parsed now.
    fn found(&mut self, path: &Path) -> &Found {
        if !self.found.contains_key(path) {
            let s = stamp(path);
            let found = match (kept(path, s), self.parser.as_mut()) {
                (Some(found), _) => found,
                (None, Some(parser)) => {
                    let (found, bytes) = parse(parser, path);
                    keep(path, s, &found);
                    self.counted(bytes);
                    found
                }
                (None, None) => Found::default(),
            };
            self.found.insert(path.to_path_buf(), found);
        }
        &self.found[path]
    }

    /// Whether line `line` (from 0) of `path` is test code.
    pub fn at(&mut self, path: &Path, line: usize) -> bool {
        self.of(path).has(line)
    }

    /// Whether all of `path` is — told without parsing it (what a parse finds is never the whole file).
    pub fn whole(&mut self, path: &Path) -> bool {
        match self.files.get(path) {
            Some(part) => *part == Part::Whole,
            None => self.without_parsing(path) == Some(Part::Whole),
        }
    }

    fn of(&mut self, path: &Path) -> &Part {
        if !self.files.contains_key(path) {
            let part = self.read(path);
            self.files.insert(path.to_path_buf(), part);
        }
        &self.files[path]
    }

    fn read(&mut self, path: &Path) -> Part {
        match self.without_parsing(path) {
            Some(part) => part,
            None => Part::Lines(self.found(path).lines.clone()),
        }
    }

    /// What's told of a file without parsing it: by its name, not Rust, no grammar, taken in for tests —
    /// None: it's parsed.
    fn without_parsing(&mut self, path: &Path) -> Option<Part> {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        if by_name(rel) {
            return Some(Part::Whole);
        }
        if path.extension().is_none_or(|x| x != "rs") || self.parser.is_none() {
            return Some(Part::Lines(Vec::new()));
        }
        if self.taken_in_for_tests(path) {
            return Some(Part::Whole);
        }
        None
    }
}

/// A test folder or file by its name (`rel` from the project root): in `tests`/`test`, or in a folder or by a
/// name the packs' languages keep tests in (`test-dirs`, `test-files`).
pub fn by_name(rel: &Path) -> bool {
    let table = crate::languages::get();
    let dirs: Vec<&str> =
        rel.parent().into_iter().flat_map(|p| p.iter()).filter_map(|c| c.to_str()).collect();
    if dirs.iter().any(|d| matches!(*d, "tests" | "test") || table.test_dirs.iter().any(|t| t == d)) {
        return true;
    }
    let Some(name) = rel.file_name().and_then(|n| n.to_str()) else { return false };
    table.test_files.iter().any(|p| crate::syntax::wildcard(p, name))
}

/// Whether an attribute says what follows is test code: `#[cfg(test)]` (and `cfg(all(test, …))`, not
/// `not(test)`), `#[test]`, `#[tokio::test]` and the like.
fn test_attribute(text: &str) -> bool {
    let t: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    let Some(inner) = t.strip_prefix("#[").and_then(|t| t.strip_suffix(']')) else { return false };
    if let Some(cfg) = inner.strip_prefix("cfg(") {
        let words: Vec<&str> = cfg.split(|c: char| !c.is_alphanumeric() && c != '_').collect();
        return words.contains(&"test") && !cfg.contains("not(test");
    }
    inner == "test" || inner.ends_with("::test")
}

/// The lines of the items under `node` that a test attribute stands over (the attributes included).
fn test_items(node: Node, src: &str, out: &mut Vec<Range<usize>>) {
    let mut cursor = node.walk();
    let kids: Vec<Node> = node.named_children(&mut cursor).collect();
    let mut i = 0;
    while i < kids.len() {
        let n = kids[i];
        if n.kind() == "attribute_item" && test_attribute(n.utf8_text(src.as_bytes()).unwrap_or_default()) {
            // The item it's on: past its other attributes and comments
            let item = kids[i + 1..]
                .iter()
                .find(|k| !matches!(k.kind(), "attribute_item" | "line_comment" | "block_comment"));
            if let Some(item) = item {
                out.push(n.start_position().row..item.end_position().row + 1);
                i = kids.iter().position(|k| k.id() == item.id()).unwrap_or(i) + 1;
                continue;
            }
        }
        if n.named_child_count() > 0 {
            test_items(n, src, out);
        }
        i += 1;
    }
}

/// A module a file declares under a test attribute without a body (`#[cfg(test)] mod name;`): its name,
/// and the `#[path]` it gives, if any.
#[derive(Clone, Debug, PartialEq)]
struct Decl {
    name: String,
    at: Option<String>,
}

impl Tests {
    /// Whether a Rust file is a module declared under a test attribute — `#[cfg(test)] mod name;` in the
    /// file that takes it in (`lib.rs`/`main.rs`/`mod.rs` beside it, or the folder's own `name.rs` above),
    /// or `#[cfg(test)] #[path = "file.rs"] mod x;` in a file beside it. Each file that could say so is
    /// read and parsed once an answer (`found`), and kept while unchanged — a search hitting most of this
    /// repository's files took 11 s when every hit read its folder anew (measured).
    fn taken_in_for_tests(&mut self, path: &Path) -> bool {
        let Some((module, above)) = self.above(path) else { return false };
        for file in above {
            let parent = file.parent().map(Path::to_path_buf).unwrap_or_default();
            let names_it = |d: &Decl| match &d.at {
                Some(p) => parent.join(p) == path,
                None => d.name == module && parent == dir_of_module(path),
            };
            if self.found(&file).decls.iter().any(names_it) {
                return true;
            }
        }
        false
    }

    /// `path`'s module name, and the files that could take it in for tests (not itself).
    fn above(&mut self, path: &Path) -> Option<(String, Vec<PathBuf>)> {
        let (dir, name) = (path.parent()?, path.file_name()?.to_str()?);
        let stem = name.trim_end_matches(".rs");
        // (mod.rs is its folder's module — taken in from above, by the folder's name)
        let (module, mut above): (String, Vec<PathBuf>) = if stem == "mod" {
            let folder = dir.file_name()?.to_str()?;
            let up = dir.parent().map(Path::to_path_buf).unwrap_or_default();
            let c =
                ["lib.rs", "main.rs", "mod.rs"].iter().map(|f| up.join(f)).chain([dir.with_extension("rs")]);
            (folder.to_string(), c.collect())
        } else {
            let c =
                ["lib.rs", "main.rs", "mod.rs"].iter().map(|f| dir.join(f)).chain([dir.with_extension("rs")]);
            (stem.to_string(), c.collect())
        };
        // Beside it, any file with a `#[path]` naming it
        if stem != "mod" {
            above.extend(self.rust_files_in(dir));
        }
        above.dedup();
        above.retain(|f| f != path);
        Some((module, above))
    }

    /// The `.rs` files in `dir`, sorted (listed once an answer).
    fn rust_files_in(&mut self, dir: &Path) -> Vec<PathBuf> {
        self.dirs
            .entry(dir.to_path_buf())
            .or_insert_with(|| {
                let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| Some(e.ok()?.path()))
                    .filter(|p| p.extension().is_some_and(|x| x == "rs"))
                    .collect();
                crate::sorting::sort(&mut v);
                v
            })
            .clone()
    }
}

/// A file's top-level test-only module declarations, from its tree (`root`).
fn decls(root: Node, src: &str) -> Vec<Decl> {
    let text = |n: Node| n.utf8_text(src.as_bytes()).unwrap_or_default().to_string();
    let mut cursor = root.walk();
    let mut out = Vec::new();
    for m in root.named_children(&mut cursor) {
        if m.kind() != "mod_item" || m.child_by_field_name("body").is_some() {
            continue;
        }
        // Its attributes: the items right above it
        let (mut test, mut at) = (false, None);
        let mut prev = m.prev_named_sibling();
        while let Some(a) =
            prev.filter(|p| matches!(p.kind(), "attribute_item" | "line_comment" | "block_comment"))
        {
            let t = text(a);
            test |= test_attribute(&t);
            if let Some(p) = t.split_once("path").and_then(|(_, r)| r.split('"').nth(1)) {
                at = Some(p.to_string());
            }
            prev = a.prev_named_sibling();
        }
        if test && let Some(name) = m.child_by_field_name("name").map(text) {
            out.push(Decl { name, at });
        }
    }
    out
}

/// The folder whose files declare `path`'s module with a plain `mod name;` (beside it, or — for
/// `name/mod.rs` — the folder above).
fn dir_of_module(path: &Path) -> &Path {
    let dir = path.parent().unwrap_or(path);
    if path.file_name().is_some_and(|n| n == "mod.rs") { dir.parent().unwrap_or(dir) } else { dir }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_files_by_their_names() {
        for p in [
            "tests/e2e.rs",
            "src/test/java/a/FooTest.java",
            "pkg/a_test.go",
            "app/test_views.py",
            "web/a.test.ts",
            "x/__tests__/a.js",
            "spec/a_spec.rb",
        ] {
            assert!(by_name(Path::new(p)), "{p}");
        }
        for p in ["src/testing.rs", "src/tests.rs", "pkg/a.go", "contest/a.py", "latest/x.ts", "README.md"] {
            assert!(!by_name(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn test_attributes() {
        for a in ["#[cfg(test)]", "#[ cfg( test ) ]", "#[cfg(all(test, unix))]", "#[test]", "#[tokio::test]"]
        {
            assert!(test_attribute(a), "{a}");
        }
        for a in ["#[cfg(not(test))]", "#[cfg(feature = \"testing\")]", "#[derive(Debug)]", "#[cfg(unix)]"] {
            assert!(!test_attribute(a), "{a}");
        }
    }

    /// What a parse found is kept while the file is unchanged — the next answer parses nothing — and found
    /// anew once it changes; a file changed within `RACY` isn't kept (another change in its time's tick would
    /// look the same). Parsed on threads, the parts are those found one by one.
    #[test]
    fn what_a_parse_found_is_kept_while_the_file_is_unchanged() {
        if Tests::new(Path::new("/")).parser.is_none() {
            return; // (no Rust grammar in this build)
        }
        let dir = std::env::temp_dir().join(format!("tarae-testcode-kept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let hour_ago = SystemTime::now() - Duration::from_secs(3600);
        let write = |p: &Path, text: &str, at: Option<SystemTime>| {
            std::fs::write(p, text).unwrap();
            if let Some(at) = at {
                std::fs::File::options().write(true).open(p).unwrap().set_modified(at).unwrap();
            }
        };
        let paths: Vec<PathBuf> = (0..20).map(|i| dir.join(format!("f{i}.rs"))).collect();
        for (i, p) in paths.iter().enumerate() {
            let pad = "\n".repeat(i % 3);
            write(
                p,
                &format!(
                    "{pad}pub fn a() {{}}\n#[cfg(test)]\nmod tests {{\n    #[test]\n    fn t() {{}}\n}}\n"
                ),
                Some(hour_ago),
            );
        }
        let lines = |t: &mut Tests| -> Vec<bool> {
            paths.iter().flat_map(|p| (0..8).map(|l| t.at(p, l)).collect::<Vec<_>>()).collect()
        };
        forget_parsed();
        let mut one = Tests::new(&dir);
        let found = lines(&mut one);
        assert_eq!(one.parsed, 20, "one by one, nothing kept yet");
        assert!(found.iter().any(|&b| b) && found.iter().any(|&b| !b));
        let mut again = Tests::new(&dir);
        again.prefetch(&paths);
        assert_eq!(
            (again.parsed, lines(&mut again)),
            (0, found.clone()),
            "kept: the next answer parses nothing"
        );
        forget_parsed();
        let mut threads = Tests::new(&dir);
        threads.prefetch(&paths);
        assert_eq!((threads.parsed, lines(&mut threads)), (20, found), "on threads: the same parts");
        // Changed (another length): found anew
        write(&paths[0], "#[test]\nfn t() {}\n", Some(hour_ago));
        let mut changed = Tests::new(&dir);
        assert!(changed.at(&paths[0], 0));
        assert_eq!(changed.parsed, 1);
        // Changed just now: not kept — each answer parses it
        write(&paths[1], "#[test]\nfn t() {}\n", None);
        for _ in 0..2 {
            let mut racy = Tests::new(&dir);
            assert!(racy.at(&paths[1], 0));
            assert_eq!(racy.parsed, 1);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// In Rust: a `#[cfg(test)]` module's lines and a lone `#[test]` function's — and a whole file a
    /// `#[cfg(test)] #[path = …] mod …;` or a `#[cfg(test)] mod name;` takes in, not one a plain `mod` does.
    #[test]
    fn rust_test_code_by_its_attributes() {
        if Tests::new(Path::new("/")).parser.is_none() {
            return; // (no Rust grammar in this build)
        }
        let dir = std::env::temp_dir().join(format!("tarae-testcode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let lib = dir.join("src/lib.rs");
        std::fs::write(
            &lib,
            "mod util;\n#[cfg(test)]\nmod helpers;\n#[cfg(test)]\n#[path = \"snap_checks.rs\"]\nmod snaps;\n\
             pub fn a() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {}\n}\n#[test]\nfn lone() {}\n",
        )
        .unwrap();
        for f in ["util.rs", "helpers.rs", "snap_checks.rs"] {
            std::fs::write(dir.join("src").join(f), "pub fn x() {}\n").unwrap();
        }
        let mut t = Tests::new(&dir);
        let at = |t: &mut Tests, l: usize| t.at(&lib, l);
        assert!(!at(&mut t, 6), "pub fn a");
        assert!(at(&mut t, 8) && at(&mut t, 9) && at(&mut t, 12), "the tests module, its attribute too");
        assert!(at(&mut t, 13) && at(&mut t, 14), "a lone #[test]");
        assert!(!at(&mut t, 0), "mod util; is code");
        assert!(t.whole(&dir.join("src/helpers.rs")), "#[cfg(test)] mod helpers;");
        assert!(t.whole(&dir.join("src/snap_checks.rs")), "#[cfg(test)] #[path] mod snaps;");
        assert!(!t.whole(&dir.join("src/util.rs")), "a plain mod");
        std::fs::remove_dir_all(&dir).ok();
    }
}
