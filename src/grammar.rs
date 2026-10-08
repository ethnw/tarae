//! Grammar install — `tarae grammar install [name…]` / in the editor `:grammar-install [language|all]`.
//! Grammars built into the binary (default = `core = true`; debug builds also `dev = true`; with
//! `--features bundled-grammars`, also `bundle = true`) are not downloaded.
//!
//! Source = a pack's `[grammar.<name>]` (`languages.rs`: git URL · commit · subfolder — pinned to a commit
//! that matches the queries). Fetch = `git` (one commit, shallow), build = the system C compiler (`$CC`,
//! default `cc` · `$CXX`/`c++` for C++ scanners) into `~/.local/share/tarae/grammars/<name>.so`.
//! The same repo+commit is fetched only once (markdown·markdown_inline etc.), and several are built
//! in parallel. Skipped if already built at that commit (`<name>.rev`).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    pub name: String,
    /// Is it in the `--features bundled-grammars` list (whether it's actually built in: `bundled`).
    pub bundle: bool,
    pub git: String,
    pub rev: String,
    pub subpath: Option<String>,
}

/// Every grammar source the packs have (`[grammar.<name>]`).
pub fn sources() -> Vec<Source> {
    crate::languages::get().grammars.clone()
}

pub fn grammars_dir() -> Option<PathBuf> {
    crate::runtime::data_dir().map(|d| d.join("grammars"))
}

/// Is it built into this binary (`core`, `dev` in a debug build, or `bundle` with `--features bundled-grammars`).
pub fn bundled(s: &Source) -> bool {
    crate::runtime::builtin_grammar(&s.name).is_some()
}

/// Usable — built into the binary, or built and installed at that commit.
pub fn installed(s: &Source) -> bool {
    bundled(s) || grammars_dir().is_some_and(|dir| installed_in(&dir, s))
}

/// Built and installed in `dir` at that commit.
fn installed_in(dir: &Path, s: &Source) -> bool {
    dir.join(format!("{}.so", s.name)).is_file()
        && std::fs::read_to_string(dir.join(format!("{}.rev", s.name))).is_ok_and(|r| r.trim() == s.rev)
}

#[derive(Clone, Debug)]
pub enum Progress {
    Done { name: String, took: Duration },
    Skipped { name: String },
    Failed { name: String, why: String },
}

/// Runs one command — on failure, the last lines of stderr as the reason.
fn run(cmd: &mut Command) -> Result<(), String> {
    let out = cmd.output().map_err(|e| format!("{}: {e}", cmd.get_program().to_string_lossy()))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    let tail: Vec<&str> = err.lines().filter(|l| !l.trim().is_empty()).rev().take(3).collect();
    Err(tail.into_iter().rev().collect::<Vec<_>>().join(" / "))
}

/// (Not through `gitrepo`, tarae's one way to run git on a project: this is a repository tarae makes itself in
/// its own data folder, from a grammar's URL and commit in the packs — no project's files, so nothing there
/// to pass for a repository; `git init` makes it, the folder is all it works in.)
fn git(dir: &Path) -> Command {
    let mut c = Command::new("git");
    c.arg("-C").arg(dir).env("GIT_TERMINAL_PROMPT", "0");
    c
}

/// Puts the repo at that commit (as-is if already there). Fetches just that commit shallowly,
/// or everything if the server doesn't allow it.
fn fetch(git_url: &str, rev: &str, dir: &Path) -> Result<(), String> {
    let stamp = dir.join(".tarae-rev");
    if std::fs::read_to_string(&stamp).is_ok_and(|r| r.trim() == rev) {
        return Ok(());
    }
    let _ = std::fs::remove_dir_all(dir);
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    run(git(dir).args(["init", "-q"]))?;
    let shallow = run(git(dir).args(["fetch", "-q", "--depth", "1", git_url, rev]))
        .and_then(|_| run(git(dir).args(["checkout", "-q", "FETCH_HEAD"])));
    if shallow.is_err() {
        run(git(dir).args(["fetch", "-q", git_url]))?;
        run(git(dir).args(["checkout", "-q", rev]))?;
    }
    std::fs::write(&stamp, rev).map_err(|e| e.to_string())
}

/// Builds `src/`'s parser.c (+ scanner.c|cc) into a shared library.
fn build(src: &Path, out: &Path) -> Result<(), String> {
    let parser = src.join("parser.c");
    if !parser.is_file() {
        return Err(format!("no parser.c in {}", src.display()));
    }
    let cc = std::env::var("CC").unwrap_or_else(|_| "cc".into());
    let cxx = std::env::var("CXX").unwrap_or_else(|_| "c++".into());
    let tmp = out.with_extension("so.tmp");
    let scanner_c = src.join("scanner.c");
    let scanner_cc = ["scanner.cc", "scanner.cpp"].iter().map(|f| src.join(f)).find(|p| p.is_file());
    let common = ["-shared", "-fPIC", "-O2", "-w"];
    match scanner_cc {
        // C++ scanner: C is compiled to an object file with the C compiler, linking is done with C++
        Some(scc) => {
            let obj = out.with_extension("parser.o");
            run(Command::new(&cc)
                .args(["-c", "-fPIC", "-O2", "-w", "-I"])
                .arg(src)
                .arg(&parser)
                .arg("-o")
                .arg(&obj))?;
            let r = run(Command::new(&cxx)
                .args(common)
                .arg("-I")
                .arg(src)
                .arg(&obj)
                .arg(&scc)
                .arg("-o")
                .arg(&tmp));
            let _ = std::fs::remove_file(&obj);
            r?;
        }
        None => {
            let mut c = Command::new(&cc);
            c.args(common).arg("-I").arg(src).arg(&parser);
            if scanner_c.is_file() {
                c.arg(&scanner_c);
            }
            run(c.arg("-o").arg(&tmp))?;
        }
    }
    std::fs::rename(&tmp, out).map_err(|e| e.to_string())
}

/// Source folders being fetched·built right now — concurrent installs (two offer cards; typescript·tsx
/// share one repo) take turns per folder instead of racing on remove_dir_all·git init.
static BUSY: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());
static FREED: Condvar = Condvar::new();

/// This install's turn at a source folder (until dropped).
struct DirTurn(PathBuf);

impl DirTurn {
    fn take(dir: &Path) -> DirTurn {
        let mut busy = BUSY.lock().unwrap_or_else(|e| e.into_inner());
        while busy.contains(dir) {
            busy = FREED.wait(busy).unwrap_or_else(|e| e.into_inner());
        }
        busy.insert(dir.to_path_buf());
        DirTurn(dir.to_path_buf())
    }

    /// The turn, unless another install holds it.
    fn try_take(dir: &Path) -> Option<DirTurn> {
        let mut busy = BUSY.lock().unwrap_or_else(|e| e.into_inner());
        busy.insert(dir.to_path_buf()).then(|| DirTurn(dir.to_path_buf()))
    }
}

impl Drop for DirTurn {
    fn drop(&mut self) {
        BUSY.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
        FREED.notify_all();
    }
}

/// Downloads and builds grammars (even if present, with `force`). Calls `report` as each one finishes.
pub fn install(names: &[String], force: bool, report: impl Fn(Progress) + Sync) -> Result<(), String> {
    let data = crate::runtime::data_dir().ok_or("no home directory")?;
    let out_dir = data.join("grammars");
    let src_dir = data.join("sources");
    std::fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let all = sources();
    let mut want: Vec<Source> = Vec::new();
    for n in names {
        match all.iter().find(|s| s.name == *n) {
            Some(s) if bundled(s) => report(Progress::Skipped { name: s.name.clone() }),
            Some(s) if force || !installed_in(&out_dir, s) => want.push(s.clone()),
            Some(s) => report(Progress::Skipped { name: s.name.clone() }),
            None => report(Progress::Failed { name: n.clone(), why: "unknown grammar".into() }),
        }
    }
    // Group by repo+commit — fetch once, build several
    let mut groups: Vec<(String, String, Vec<Source>)> = Vec::new();
    for s in want {
        match groups.iter_mut().find(|g| g.0 == s.git && g.1 == s.rev) {
            Some(g) => g.2.push(s),
            None => groups.push((s.git.clone(), s.rev.clone(), vec![s])),
        }
    }
    sweep(&src_dir, &out_dir, &all);
    let queue = Mutex::new(groups);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
    std::thread::scope(|s| {
        for _ in 0..workers {
            crate::event::scoped(s, "tarae-grammar", || {
                loop {
                    let Some((git_url, rev, grammars)) = queue.lock().unwrap().pop() else { return };
                    let dir = source_dir(&src_dir, &git_url, &rev);
                    install_group(&dir, &out_dir, &git_url, &rev, grammars, force, &report);
                }
            });
        }
    });
    Ok(())
}

/// Where one repo+commit is checked out.
fn source_dir(src_dir: &Path, git_url: &str, rev: &str) -> PathBuf {
    let repo = git_url.trim_end_matches('/').rsplit('/').next().unwrap_or("repo");
    src_dir.join(format!("{repo}-{}", &rev[..rev.len().min(12)]))
}

/// Fetches one repo+commit and builds its grammars. The checkout (mostly generated parser.c, tens of MB)
/// is deleted once they all built; after a failure it stays, so a retry (say, with a C++ compiler
/// installed) doesn't fetch again.
fn install_group(
    dir: &Path,
    out_dir: &Path,
    git_url: &str,
    rev: &str,
    grammars: Vec<Source>,
    force: bool,
    report: &impl Fn(Progress),
) {
    let _turn = DirTurn::take(dir);
    // Another install may have built them (and deleted the checkout) while this one waited its turn —
    // checked before fetching, so it isn't fetched again for nothing
    let (done, grammars): (Vec<Source>, Vec<Source>) =
        grammars.into_iter().partition(|s| !force && installed_in(out_dir, s));
    for s in done {
        report(Progress::Skipped { name: s.name });
    }
    if grammars.is_empty() {
        return;
    }
    let started = Instant::now();
    let fetched = fetch(git_url, rev, dir);
    let mut all_built = true;
    for s in grammars {
        let result = fetched.clone().and_then(|_| {
            let root = s.subpath.as_ref().map_or(dir.to_path_buf(), |p| dir.join(p));
            build(&root.join("src"), &out_dir.join(format!("{}.so", s.name)))?;
            std::fs::write(out_dir.join(format!("{}.rev", s.name)), &s.rev).map_err(|e| e.to_string())
        });
        all_built &= result.is_ok();
        report(match result {
            Ok(()) => Progress::Done { name: s.name.clone(), took: started.elapsed() },
            Err(why) => Progress::Failed { name: s.name.clone(), why },
        });
    }
    if all_built {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Deletes checkouts nothing needs: a commit no longer pinned, or one whose grammars are all installed
/// (left by versions that kept every checkout). Checkouts another install is using are left alone.
fn sweep(src_dir: &Path, out_dir: &Path, all: &[Source]) {
    let Ok(entries) = std::fs::read_dir(src_dir) else { return };
    for e in entries.flatten() {
        let dir = e.path();
        let mut group = all.iter().filter(|s| source_dir(src_dir, &s.git, &s.rev) == dir).peekable();
        let pending = group.peek().is_some() && group.any(|s| !bundled(s) && !installed_in(out_dir, s));
        if pending {
            continue;
        }
        if let Some(_turn) = DirTurn::try_take(&dir) {
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

/// Language name (or grammar name) → grammars to install: its own, the ones its pack installs along
/// (`grammars-with`: inline markdown in paragraphs), and those every grammar brings (`everywhere`: TODO in
/// comments).
pub fn for_language(lang: &str) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(spec) = crate::syntax::spec(lang) {
        v.push(spec.grammar.clone());
        v.extend(spec.grammars_with.iter().cloned());
    } else if sources().iter().any(|s| s.name == lang) {
        v.push(lang.to_string());
    }
    if !v.is_empty() {
        for g in &crate::languages::get().everywhere {
            if !v.contains(g) {
                v.push(g.clone());
            }
        }
    }
    v
}

// ── Editor: offer a download for a newly opened language (card in offer.rs) ───

/// Language name → human-readable name (its pack's `title`).
pub fn pretty(lang: &str) -> String {
    match crate::syntax::spec(lang) {
        Some(s) => s.title.clone(),
        None => {
            let mut c = lang.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        }
    }
}

impl crate::editor::Editor {
    /// A language feature (test·debug) was requested but the grammar is missing — ask again, at the front,
    /// even for a language already asked about.
    /// The card states the intended work ("…then run the test?") and resumes `then` once downloaded.
    pub fn offer_grammar_then(&mut self, spec: &crate::syntax::LangSpec, then: crate::offer::Resume) {
        use crate::offer::{Offer, Resume, What};
        let lang = pretty(&spec.name);
        self.grammar_hinted.insert(spec.grammar.clone());
        if !self.offer_popups {
            return self
                .set_status(format!("{lang} needs its grammar first — :grammar-install {}", spec.name));
        }
        let (ask, doing) = match &then {
            Resume::Test(crate::testing::Scope::Nearest) => ("run the test", "running the test"),
            Resume::Test(crate::testing::Scope::File) => ("run this file's tests", "running the tests"),
            Resume::TestDebug | Resume::TestTarget(_) => ("debug the test", "debugging the test"),
            Resume::Debug => ("start debugging", "starting the debugger"),
            Resume::Attach(_) => ("attach the debugger", "attaching"),
        };
        let card = Offer::new(
            format!("{lang} grammar needed"),
            &format!("Download it, then {ask}?"),
            &format!("Fetching and building… then {doing}"),
            format!("Couldn't build the {lang} grammar"),
            What::Grammars { lang: lang.clone(), names: for_language(&spec.name), then: Some(then.clone()) },
        );
        // If a card for the same language already exists (shown when the file was opened), turn it into
        // this job and move it to the front
        let same = |o: &Offer| matches!(&o.what, What::Grammars { lang: l, .. } if *l == lang);
        match self.offers.iter().position(same) {
            Some(i) if self.offers[i].installing() => {
                if let What::Grammars { then: t, .. } = &mut self.offers[i].what {
                    *t = Some(then);
                }
            }
            Some(i) => {
                let id = self.offers[i].id;
                self.offers[i] = Offer { id, ..card };
                self.offer_to_front(i);
            }
            None => {
                self.push_offer(card);
                let last = self.offers.len() - 1;
                self.offer_to_front(last);
            }
        }
    }

    /// When the file's language grammar is missing (called by `load_language`): offer a download —
    /// once per language until quit.
    pub fn offer_grammar(&mut self, spec: &crate::syntax::LangSpec) {
        if !self.grammar_hinted.insert(spec.grammar.clone()) {
            return;
        }
        if !self.offer_popups || !self.config.offer_grammars {
            return self.set_status(format!(
                "No syntax colors for {} yet — :grammar-install fetches and builds it",
                pretty(&spec.name)
            ));
        }
        let lang = pretty(&spec.name);
        self.push_offer(crate::offer::Offer::new(
            format!("{lang} syntax colors"),
            "Download and build the tree-sitter grammar?",
            "Fetching and building…",
            format!("Couldn't build the {lang} grammar"),
            crate::offer::What::Grammars { lang, names: for_language(&spec.name), then: None },
        ));
    }
}

// ── CLI: `tarae grammar install|list` ───────────────────────────────────

fn paint(code: &str, s: &str) -> String {
    format!("\x1b[{code}m{s}\x1b[0m")
}

pub fn cli(args: &[String]) -> anyhow::Result<()> {
    match args.first().map(String::as_str) {
        Some("install") | Some("update") => {
            let force = args.iter().any(|a| a == "--force");
            let named: Vec<String> = args[1..].iter().filter(|a| !a.starts_with("--")).cloned().collect();
            let names: Vec<String> = if named.is_empty() {
                sources().into_iter().filter(|s| !bundled(s)).map(|s| s.name).collect()
            } else {
                named
                    .iter()
                    .flat_map(|n| for_language(n))
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .collect()
            };
            let dir = grammars_dir().ok_or_else(|| anyhow::anyhow!("no home directory"))?;
            println!(
                "{} {} grammars → {}",
                paint("1;36", "tarae grammar"),
                names.len(),
                paint("2", &dir.display().to_string())
            );
            let failed = Mutex::new(Vec::new());
            let started = Instant::now();
            install(&names, force, |p| match p {
                Progress::Done { name, took } => {
                    println!(
                        "  {} {name:<18} {}",
                        paint("32", "●"),
                        paint("2", &format!("{:.1}s", took.as_secs_f32()))
                    )
                }
                Progress::Skipped { name } => {
                    println!("  {} {name:<18} {}", paint("2", "·"), paint("2", "up to date"))
                }
                Progress::Failed { name, why } => {
                    println!("  {} {name:<18} {}", paint("31", "▲"), why);
                    failed.lock().unwrap().push(name);
                }
            })
            .map_err(|e| anyhow::anyhow!(e))?;
            let failed = failed.into_inner().unwrap();
            if failed.is_empty() {
                println!("{} in {:.0}s", paint("1;32", "done"), started.elapsed().as_secs_f32());
            } else {
                println!("{} {} failed: {}", paint("1;33", "done"), failed.len(), failed.join(" "));
                println!(
                    "  {}",
                    paint("2", "needs git and a C compiler (cc; C++ grammars also c++) on PATH")
                );
            }
            Ok(())
        }
        Some("list") | None => {
            for s in sources() {
                let (mark, note) = match (bundled(&s), installed(&s)) {
                    (true, _) => (paint("32", "●"), paint("2", "built in")),
                    (false, true) => (paint("32", "●"), String::new()),
                    (false, false) => (paint("2", "·"), paint("2", "tarae grammar install")),
                };
                println!("{mark} {:<18} {note}", s.name);
            }
            println!("{}", paint("2", "tarae grammar install [name…]   (no name = all)"));
            Ok(())
        }
        Some(other) => anyhow::bail!("unknown grammar command '{other}' — install [name…] · list"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn pretty_language_names() {
        for (l, want) in [
            ("go", "Go"),
            ("c", "C"),
            ("bash", "Bash"),
            ("json", "JSON"),
            ("hcl", "Terraform"),
            ("typescript", "TypeScript"),
            ("cpp", "C++"),
            ("rust", "Rust"),
        ] {
            assert_eq!(super::pretty(l), want);
        }
    }

    use super::*;

    #[test]
    fn every_language_has_a_grammar_source() {
        let s = sources();
        assert!(s.len() >= 50);
        for l in &crate::languages::get().langs {
            let grammar = &l.grammar;
            assert!(s.iter().any(|x| x.name == *grammar), "{}: no [grammar.{grammar}]", l.name);
            for with in &l.grammars_with {
                assert!(s.iter().any(|x| x.name == *with), "{}: no [grammar.{with}]", l.name);
            }
        }
        assert!(s.iter().all(|x| x.rev.len() == 40 && x.git.starts_with("https://")));
    }

    #[test]
    fn language_brings_its_companions() {
        assert_eq!(for_language("rust"), ["rust", "comment"]);
        assert_eq!(for_language("markdown"), ["markdown", "markdown_inline", "comment"]);
        assert_eq!(for_language("protobuf"), ["proto", "comment"], "language name → grammar name");
        assert!(for_language("nope").is_empty());
    }

    #[test]
    fn embedded_grammars_follow_the_build() {
        for core in ["toml", "markdown", "markdown_inline", "comment"] {
            assert!(crate::runtime::builtin_grammar(core).is_some(), "{core}: core, in every build");
        }
        // build.rs keys `dev` off the debug profile (what turns debug assertions on) or `--features
        // dev-grammars` (`cargo perf`) — never debug info: `cargo test` has rust built in, `cargo test --release`
        // (like `cargo install`, line tables and all) doesn't
        let rust = crate::runtime::builtin_grammar("rust").is_some();
        assert_eq!(rust, cfg!(tarae_dev_grammars) || cfg!(feature = "bundled-grammars"));
        const { assert!(!cfg!(debug_assertions) || cfg!(tarae_dev_grammars), "every debug build has them") };
        const {
            assert!(
                cfg!(debug_assertions) || cfg!(tarae_dev_grammars) == cfg!(feature = "dev-grammars"),
                "outside the debug profile, only when asked"
            )
        };
    }

    fn source(name: &str, git: &str, rev: &str) -> Source {
        Source { name: name.into(), bundle: false, git: git.into(), rev: rev.into(), subpath: None }
    }

    #[test]
    fn sweep_keeps_only_checkouts_still_needed() {
        let tmp = std::env::temp_dir().join(format!("tarae-sweep-{}", std::process::id()));
        let (src, out) = (tmp.join("sources"), tmp.join("grammars"));
        let all = [
            source("done", "https://x/tree-sitter-done", &"a".repeat(40)),
            source("pending", "https://x/tree-sitter-pending", &"b".repeat(40)),
            // Two grammars from one checkout: kept while either is missing
            source("ts", "https://x/tree-sitter-typescript", &"c".repeat(40)),
            source("tsx", "https://x/tree-sitter-typescript", &"c".repeat(40)),
        ];
        let old = src.join("tree-sitter-done-dddddddddddd"); // a commit no longer pinned
        let busy = src.join("tree-sitter-gone-eeeeeeeeeeee");
        let dirs: Vec<PathBuf> = all.iter().map(|s| source_dir(&src, &s.git, &s.rev)).collect();
        for d in dirs.iter().chain([&old, &busy]) {
            std::fs::create_dir_all(d.join("src")).unwrap();
        }
        std::fs::create_dir_all(&out).unwrap();
        for s in &all[..3] {
            std::fs::write(out.join(format!("{}.so", s.name)), "").unwrap();
            std::fs::write(out.join(format!("{}.rev", s.name)), &s.rev).unwrap();
        }
        std::fs::write(out.join("pending.rev"), "an older commit").unwrap();
        let turn = DirTurn::take(&busy);
        sweep(&src, &out, &all);
        assert!(!dirs[0].exists(), "all its grammars installed");
        assert!(dirs[1].exists(), "built at another commit — a retry reuses it");
        assert!(dirs[2].exists(), "tsx still missing");
        assert!(!old.exists(), "unpinned commit");
        assert!(busy.exists(), "another install holds it");
        drop(turn);
        std::fs::remove_dir_all(tmp).ok();
    }

    /// A local repo with `src/parser.c` — anything `cc` compiles counts as a grammar here.
    fn repo_with_parser(root: &Path, parser_c: &str) -> String {
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/parser.c"), parser_c).unwrap();
        for args in [
            &["init", "-q"][..],
            &["add", "."],
            &["-c", "user.name=t", "-c", "user.email=t@t", "commit", "-qm", "g"],
        ] {
            assert!(git(root).args(args).status().unwrap().success(), "git {args:?}");
        }
        let out = git(root).args(["rev-parse", "HEAD"]).output().unwrap();
        String::from_utf8(out.stdout).unwrap().trim().to_string()
    }

    #[test]
    fn checkout_is_deleted_after_a_build_and_kept_after_a_failure() {
        let tmp = std::env::temp_dir().join(format!("tarae-group-{}", std::process::id()));
        let out = tmp.join("grammars");
        std::fs::create_dir_all(&out).unwrap();
        for (name, parser_c, builds) in [("good", "int tarae_good;", true), ("bad", "not C at all", false)] {
            let repo = tmp.join(format!("tree-sitter-{name}"));
            let rev = repo_with_parser(&repo, parser_c);
            let git_url = repo.to_string_lossy().to_string();
            let dir = source_dir(&tmp.join("sources"), &git_url, &rev);
            let failed = Mutex::new(false);
            let g = vec![source(name, &git_url, &rev)];
            install_group(&dir, &out, &git_url, &rev, g, true, &|p| {
                *failed.lock().unwrap() |= matches!(p, Progress::Failed { .. })
            });
            assert_eq!(!*failed.lock().unwrap(), builds, "{name}");
            assert_eq!(out.join(format!("{name}.so")).is_file(), builds, "{name}");
            assert_eq!(dir.exists(), !builds, "{name}: checkout");
        }
        std::fs::remove_dir_all(tmp).ok();
    }

    #[test]
    fn installed_grammars_are_not_fetched_again() {
        let tmp = std::env::temp_dir().join(format!("tarae-refetch-{}", std::process::id()));
        let out = tmp.join("grammars");
        std::fs::create_dir_all(&out).unwrap();
        let repo = tmp.join("tree-sitter-typescript");
        let rev = repo_with_parser(&repo, "int tarae_ts;");
        let git_url = repo.to_string_lossy().to_string();
        let dir = source_dir(&tmp.join("sources"), &git_url, &rev);
        let g = || vec![source("typescript", &git_url, &rev), source("tsx", &git_url, &rev)];
        let run = |force| {
            let seen = Mutex::new(Vec::new());
            install_group(&dir, &out, &git_url, &rev, g(), force, &|p| seen.lock().unwrap().push(p));
            seen.into_inner().unwrap()
        };
        assert!(run(false).iter().all(|p| matches!(p, Progress::Done { .. })));
        assert!(!dir.exists());
        // The other card's install, after its turn: both are built and the checkout is gone. With the repo
        // gone too, any fetch would fail — so only skipping without fetching passes.
        std::fs::remove_dir_all(&repo).unwrap();
        let second = run(false);
        assert!(second.iter().all(|p| matches!(p, Progress::Skipped { .. })), "{second:?}");
        assert_eq!(second.len(), 2);
        assert!(!dir.exists(), "nothing fetched");
        assert!(
            run(true).iter().all(|p| matches!(p, Progress::Failed { .. })),
            "force fetches — and fails here"
        );
        std::fs::remove_dir_all(tmp).ok();
    }

    /// Actually downloads and builds — on a machine with network·git·cc:
    /// `cargo test -- --ignored grammar_install_e2e`.
    #[test]
    #[ignore]
    fn grammar_install_e2e() {
        let dir = std::env::temp_dir().join(format!("tarae-grammar-{}", std::process::id()));
        // SAFETY: a variable only this test uses
        unsafe { std::env::set_var("XDG_DATA_HOME", &dir) };
        let done = Mutex::new(Vec::new());
        install(&["json".to_string()], false, |p| done.lock().unwrap().push(format!("{p:?}"))).unwrap();
        assert!(dir.join("tarae/grammars/json.so").is_file(), "{:?}", done.lock().unwrap());
        let left = std::fs::read_dir(dir.join("tarae/sources")).map_or(0, |d| d.count());
        assert_eq!(left, 0, "checkout deleted after the build");
        std::fs::remove_dir_all(dir).ok();
    }
}
