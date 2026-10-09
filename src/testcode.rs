//! What's test code (N5b) — `references` and `search` list it apart (`nav.rs`). Any language: test folders
//! and test files by their usual names (`tests/`, `__tests__/`, `foo_test.go`, `test_foo.py`, `a.test.ts`,
//! `FooTest.java` …). Rust: what `#[cfg(test)]` or `#[test]` stands over — a module, a function — and a whole
//! file taken in by a `#[cfg(test)] mod …;` (`term.rs` takes in `term_snapshots.rs` so). Reads and parses
//! files: on workers.

use std::collections::HashMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

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
    /// Files' test-only module declarations · folders' `.rs` files — each read once.
    decls: HashMap<PathBuf, Vec<Decl>>,
    dirs: HashMap<PathBuf, Vec<PathBuf>>,
    parser: Option<tree_sitter::Parser>,
}

impl Tests {
    /// For files under `root` (the project: path names are read from there down).
    pub fn new(root: &Path) -> Tests {
        let rust = crate::syntax::spec("rust").and_then(|s| crate::syntax::Loader::global().load(s).ok());
        let parser = rust.and_then(|l| {
            let mut p = tree_sitter::Parser::new();
            p.set_language(&l.language).ok().map(|_| p)
        });
        Tests {
            root: root.to_path_buf(),
            files: HashMap::new(),
            decls: HashMap::new(),
            dirs: HashMap::new(),
            parser,
        }
    }

    /// Whether line `line` (from 0) of `path` is test code.
    pub fn at(&mut self, path: &Path, line: usize) -> bool {
        self.of(path).has(line)
    }

    /// Whether all of `path` is.
    pub fn whole(&mut self, path: &Path) -> bool {
        *self.of(path) == Part::Whole
    }

    fn of(&mut self, path: &Path) -> &Part {
        if !self.files.contains_key(path) {
            let part = self.read(path);
            self.files.insert(path.to_path_buf(), part);
        }
        &self.files[path]
    }

    fn read(&mut self, path: &Path) -> Part {
        let rel = path.strip_prefix(&self.root).unwrap_or(path);
        if by_name(rel) {
            return Part::Whole;
        }
        if path.extension().is_none_or(|x| x != "rs") {
            return Part::Lines(Vec::new());
        }
        if self.parser.is_none() {
            return Part::Lines(Vec::new());
        }
        if self.taken_in_for_tests(path) {
            return Part::Whole;
        }
        let Ok(src) = std::fs::read_to_string(path) else { return Part::Lines(Vec::new()) };
        if !src.contains("test") {
            return Part::Lines(Vec::new());
        }
        let Some(tree) = self.parser.as_mut().and_then(|p| p.parse(&src, None)) else {
            return Part::Lines(Vec::new());
        };
        let mut lines = Vec::new();
        test_items(tree.root_node(), &src, &mut lines);
        Part::Lines(lines)
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
struct Decl {
    name: String,
    at: Option<String>,
}

impl Tests {
    /// Whether a Rust file is a module declared under a test attribute — `#[cfg(test)] mod name;` in the
    /// file that takes it in (`lib.rs`/`main.rs`/`mod.rs` beside it, or the folder's own `name.rs` above),
    /// or `#[cfg(test)] #[path = "file.rs"] mod x;` in a file beside it. Each file that could say so is
    /// read and parsed once an answer (`decls`) — a search hitting most of this repository's files took
    /// 11 s when every hit read its folder anew (measured).
    fn taken_in_for_tests(&mut self, path: &Path) -> bool {
        let (Some(dir), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str())) else {
            return false;
        };
        let stem = name.trim_end_matches(".rs");
        // (mod.rs is its folder's module — taken in from above, by the folder's name)
        let (module, mut above): (String, Vec<PathBuf>) = if stem == "mod" {
            let Some(folder) = dir.file_name().and_then(|n| n.to_str()) else { return false };
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
        for file in above {
            if file == path {
                continue;
            }
            let parent = file.parent().map(Path::to_path_buf).unwrap_or_default();
            let names_it = |d: &Decl| match &d.at {
                Some(p) => parent.join(p) == path,
                None => d.name == module && parent == dir_of_module(path),
            };
            if self.decls_in(&file).iter().any(names_it) {
                return true;
            }
        }
        false
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

    /// `file`'s top-level test-only module declarations (read and parsed once an answer).
    fn decls_in(&mut self, file: &Path) -> &[Decl] {
        if !self.decls.contains_key(file) {
            let found = self.read_decls(file);
            self.decls.insert(file.to_path_buf(), found);
        }
        &self.decls[file]
    }

    fn read_decls(&mut self, file: &Path) -> Vec<Decl> {
        let Ok(src) = std::fs::read_to_string(file) else { return Vec::new() };
        let Some(parser) = self.parser.as_mut().filter(|_| src.contains("mod ") && src.contains("test"))
        else {
            return Vec::new();
        };
        let Some(tree) = parser.parse(&src, None) else { return Vec::new() };
        let text = |n: Node| n.utf8_text(src.as_bytes()).unwrap_or_default().to_string();
        let root = tree.root_node();
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
