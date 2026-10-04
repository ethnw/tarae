//! Embeds three things into the binary at build time.
//! 1. Queries — `runtime/queries/<lang>/*.scm` (so colors work even without the files at run time).
//! 2. Language packs — `runtime/packs/<lang>/` (`pack.toml` and its queries).
//! 3. Grammars — unpacks `runtime/grammars.tar.gz` (C sources of `bundle = true` in languages.toml, same
//!    commits as the queries) and compiles them in parallel with the C compiler. The default build has only
//!    `core = true` (toml·markdown·comment), debug builds also `dev = true` (rust), `--features
//!    bundled-grammars` all of `bundle = true`. The rest are downloaded at run time.
//!
//! Nothing it prints or writes names the checkout. Every checkout's build of tarae shares one build
//! script run in the shared `build-dir` (`~/.cargo/build/shared` — the unit's hash is the same for the
//! same package): an absolute `rerun-if-changed` path didn't match the next checkout's, so every check after
//! another checkout's re-ran this and rebuilt tarae (≈ 10 s). The paths are relative (cargo reads them from
//! the package root, this script's working directory), and the generated code holds the files' text
//! rather than an `include_str!` of a path that would point into whichever checkout ran this last.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    let out = PathBuf::from(std::env::var("OUT_DIR").unwrap());
    embed_packed(Path::new("runtime/queries"), &["scm"], "QUERIES", &out);
    embed(Path::new("runtime/packs"), &["toml", "scm"], "PACKS", &out.join("packs.rs"));
    grammars(&out);
    // The perf budgets measure an optimized build (`cargo perf`); the test profile's opt-level 0 (a fast
    // edit-test loop) has them skip themselves
    println!("cargo::rustc-check-cfg=cfg(tarae_optimized)");
    if std::env::var("OPT_LEVEL").is_ok_and(|o| o != "0") {
        println!("cargo::rustc-cfg=tarae_optimized");
    }
}

/// Grammar names to embed (`core` from languages.toml `[[grammar]]`, `dev` in debug builds, plus `bundle` if
/// the feature is on).
fn wanted() -> Vec<String> {
    let path = "src/languages.toml";
    println!("cargo:rerun-if-changed={path}");
    let table: toml::Table = toml::from_str(&std::fs::read_to_string(path).unwrap()).expect("languages.toml");
    let all = cfg!(feature = "bundled-grammars");
    // `cargo run`/`cargo test` (the debug profile): the tests and screen snapshots are written against these,
    // and working on tarae itself wants them without a download. So do builds with debug info (`cargo
    // perf`'s profile: its syntax budgets). Release builds leave them out (size).
    let dev = std::env::var("PROFILE").is_ok_and(|p| p == "debug")
        || std::env::var("DEBUG").is_ok_and(|d| d == "true");
    println!("cargo::rustc-check-cfg=cfg(tarae_dev_grammars)");
    if dev {
        println!("cargo::rustc-cfg=tarae_dev_grammars");
    }
    let flag = |g: &toml::Value, key: &str| g.get(key).and_then(|v| v.as_bool()) == Some(true);
    table["grammar"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|g| flag(g, "core") || (dev && flag(g, "dev")) || (all && flag(g, "bundle")))
        .map(|g| g["name"].as_str().unwrap().to_string())
        .collect()
}

/// Every `<lang>/<file>` under `root` (relative) with one of `exts`, sorted.
fn files(root: &Path, exts: &[&str]) -> Vec<(String, PathBuf)> {
    // A directory is scanned recursively by cargo — covers every language folder
    println!("cargo:rerun-if-changed={}", root.display());
    let mut entries = Vec::new();
    for lang in std::fs::read_dir(root).unwrap().flatten() {
        if !lang.path().is_dir() {
            continue;
        }
        for f in std::fs::read_dir(lang.path()).unwrap().flatten() {
            let p = f.path();
            if p.extension().is_some_and(|e| exts.iter().any(|x| e == *x)) {
                let rel =
                    format!("{}/{}", lang.file_name().to_string_lossy(), f.file_name().to_string_lossy());
                entries.push((rel, p));
            }
        }
    }
    entries.sort();
    entries
}

/// `static NAME: &[(&str, &str)]` of every `<lang>/<file>` under `root` (relative) with one of `exts`, and
/// its text.
fn embed(root: &Path, exts: &[&str], name: &str, out: &Path) {
    let mut s = format!("pub static {name}: &[(&str, &str)] = &[\n");
    for (rel, p) in files(root, exts) {
        // The text itself (a Rust literal by `{:?}`), not `include_str!` of a path: see the top
        let text = std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
        writeln!(s, "    ({rel:?}, {text:?}),").unwrap();
    }
    s.push_str("];\n");
    std::fs::write(out, s).unwrap();
}

/// The same files as one deflated text (`<name>.z`, unpacked once at run time — `runtime::queries_text`)
/// and `static NAME: &[(&str, usize, usize)]`, each file's byte range in the unpacked text.
fn embed_packed(root: &Path, exts: &[&str], name: &str, out: &Path) {
    use std::io::Write as _;
    let mut text = String::new();
    let mut s = format!("pub static {name}: &[(&str, usize, usize)] = &[\n");
    for (rel, p) in files(root, exts) {
        let start = text.len();
        text.push_str(&std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display())));
        writeln!(s, "    ({rel:?}, {start}, {}),", text.len()).unwrap();
    }
    s.push_str("];\n");
    let z = out.join(format!("{}.z", name.to_lowercase()));
    let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::best());
    enc.write_all(text.as_bytes()).unwrap();
    std::fs::write(&z, enc.finish().unwrap()).unwrap();
    // A path in OUT_DIR — the shared build-dir's, written by this run — names no checkout (see the top)
    writeln!(s, "static {name}_Z: &[u8] = include_bytes!({:?});", z.display().to_string()).unwrap();
    std::fs::write(out.join(format!("{}.rs", name.to_lowercase())), s).unwrap();
}

fn grammars(out: &Path) {
    let archive = "runtime/grammars.tar.gz";
    println!("cargo:rerun-if-changed={archive}");
    let dir = out.join("grammars");
    let _ = std::fs::remove_dir_all(&dir);
    let file = std::fs::File::open(archive).expect("runtime/grammars.tar.gz (cargo xtask vendor-grammars)");
    tar::Archive::new(flate2::read::GzDecoder::new(file)).unpack(&dir).expect("unpack grammars");
    let mut names = wanted();
    for n in &names {
        assert!(dir.join(n).is_dir(), "{n}: not in runtime/grammars.tar.gz (cargo xtask vendor-grammars)");
    }
    names.sort();
    // Compile in parallel (several large parser.c files — slow if done in series)
    let queue = std::sync::Mutex::new(names.clone());
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                while let Some(name) = queue.lock().unwrap().pop() {
                    // Unpacked in repo layout — find the src/ holding parser.c (grammars with subfolders)
                    let src = find_src(&dir.join(&name)).expect("parser.c");
                    compile(&src, &name);
                }
            });
        }
    });
    // Name → language function table
    let mut s = String::from("unsafe extern \"C\" {\n");
    for n in &names {
        writeln!(s, "    fn tree_sitter_{}() -> *const ();", n.replace('-', "_")).unwrap();
    }
    s.push_str("}\n\n/// Grammars built into the binary: (grammar name, language function).\npub static BUILTIN_GRAMMARS: &[(&str, unsafe extern \"C\" fn() -> *const ())] = &[\n");
    for n in &names {
        writeln!(s, "    ({n:?}, tree_sitter_{}),", n.replace('-', "_")).unwrap();
    }
    s.push_str("];\n");
    std::fs::write(out.join("builtin_grammars.rs"), s).unwrap();
}

fn find_src(root: &Path) -> Option<PathBuf> {
    if root.join("parser.c").is_file() {
        return Some(root.to_path_buf());
    }
    std::fs::read_dir(root).ok()?.flatten().filter(|e| e.path().is_dir()).find_map(|e| find_src(&e.path()))
}

fn compile(src: &Path, name: &str) {
    // Grammars are optimized regardless of build profile (so parsing isn't slow even in debug builds)
    let mut c = cc::Build::new();
    c.include(src).file(src.join("parser.c")).opt_level(2).warnings(false).cargo_warnings(false);
    if src.join("scanner.c").is_file() {
        c.file(src.join("scanner.c"));
    }
    c.compile(&format!("tree-sitter-{name}"));
    let cc_scanner = ["scanner.cc", "scanner.cpp"].into_iter().map(|f| src.join(f)).find(|p| p.is_file());
    if let Some(scanner) = cc_scanner {
        cc::Build::new()
            .cpp(true)
            .include(src)
            .file(scanner)
            .opt_level(2)
            .warnings(false)
            .cargo_warnings(false)
            .compile(&format!("tree-sitter-{name}-scanner"));
    }
}
