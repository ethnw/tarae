//! Tests — run the test at the cursor · this file's tests (test pane below), debug them (debug pane),
//! rerun the last one. A language's tests are found and run the way its pack says (`packs.rs`,
//! `runtime/packs/`: Go, Rust, Python, Java). Finding goes **through the tree-sitter tree** — if the grammar
//! is missing, show a download offer and resume the intended work once downloaded (`offer::Resume`). Here:
//! the helpers packs use (the Cargo target, the virtualenv), running, and debugging.
//!
//! Runs on three worker threads (read stdout·stderr, wait) — editing isn't blocked. Launched in a new session
//! (setsid) so closing kills the whole process group (including the test binaries cargo spawned).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;
use tree_sitter::Tree;

use crate::editor::Editor;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    /// The test at the cursor (else the enclosing module, else the file).
    Nearest,
    File,
}

/// One test (or group) to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    /// Display name (`editor::tests::undo` · `TestParse` · `tests/test_x.py::TestA::test_b`).
    pub label: String,
    pub cwd: PathBuf,
    pub how: How,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum How {
    /// From a language pack: the command, the debugger's launch configuration, the output parser.
    /// And what to build before debugging; or the command to debug with instead, and how to attach
    /// (`java`); and for `junit`, where the reports land and the source roots.
    Pack {
        lang: String,
        argv: Vec<String>,
        debug: Option<Value>,
        parse: String,
        build: Option<crate::packs::Build>,
        debug_run: Option<(Vec<String>, String)>,
        junit: Option<(Vec<PathBuf>, Vec<PathBuf>)>,
    },
}

impl Target {
    /// Command to run (argv).
    pub fn argv(&self) -> Vec<String> {
        let How::Pack { argv, .. } = &self.how;
        argv.clone()
    }
}

fn lang_of(how: &How) -> &str {
    let How::Pack { lang, .. } = how;
    lang
}

/// Which output parser reads a run.
fn parser_of(how: &How) -> &str {
    let How::Pack { parse, .. } = how;
    parse
}

/// Can this language's tests be run (before finding — when asking whether to download the grammar).
pub fn supported(lang: &str) -> Result<(), String> {
    match lang {
        "" => Err("not a source file".into()),
        l => match crate::packs::has_tests(l) {
            Ok(true) => Ok(()),
            Ok(false) => Err(format!("no test runner for {l} yet")),
            Err(e) => Err(e),
        },
    }
}

/// The test to run for this file·cursor (byte) — `tree` is `src` parsed in that language.
pub fn find(
    lang: &str,
    path: &Path,
    src: &str,
    tree: &Tree,
    cursor: usize,
    scope: Scope,
) -> Result<Target, String> {
    supported(lang)?;
    let pack =
        crate::packs::tests(lang, &tree.language()).ok_or(format!("no test runner for {lang} yet"))??;
    let f = pack.find(path, src, tree, cursor.min(src.len()), scope == Scope::File)?;
    let how = How::Pack {
        lang: lang.to_string(),
        argv: f.argv,
        debug: f.debug,
        parse: f.parse,
        build: f.build,
        debug_run: f.debug_run,
        junit: f.junit,
    };
    Ok(Target { label: f.label, cwd: f.cwd, how })
}

fn up_to(start: &Path, found: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    let mut dir = start.parent()?.to_path_buf();
    loop {
        if found(&dir) {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

// ── Rust ─────────────────────────────────────────────────────────────────

/// The Cargo target a file belongs to (the `cargo` helper of the Rust pack).
pub(crate) struct Crate {
    /// The target's root file (for picking the test binary to debug: the artifact's `target.src_path`).
    pub crate_root: PathBuf,
    /// `--lib`, `--bin x`, `--test x` (or nothing: the binary of src/main.rs).
    pub flag: Vec<String>,
    /// The file's module path within it.
    pub mods: Vec<String>,
}

pub(crate) fn rust_crate(path: &Path) -> Result<Crate, String> {
    let c = |crate_root: PathBuf, flag: Vec<String>, mods: Vec<String>| Crate { crate_root, flag, mods };
    let root = up_to(path, |d| d.join("Cargo.toml").is_file()).ok_or("not in a Cargo project")?;
    let rel = path.strip_prefix(&root).map_err(|_| "file outside its package")?;
    let parts: Vec<String> = rel.iter().map(|c| c.to_string_lossy().into_owned()).collect();
    let stem = |s: &str| s.trim_end_matches(".rs").to_string();
    match parts.iter().map(String::as_str).collect::<Vec<_>>().as_slice() {
        // tests/foo.rs · tests/foo/main.rs = integration test target foo
        ["tests", f] => Ok(c(path.to_path_buf(), vec!["--test".into(), stem(f)], Vec::new())),
        ["tests", dir, rest @ ..] => {
            let crate_root = root.join("tests").join(dir).join("main.rs");
            let mods = module_path(rest);
            Ok(c(crate_root, vec!["--test".into(), dir.to_string()], mods))
        }
        ["src", "bin", f] => Ok(c(path.to_path_buf(), vec!["--bin".into(), stem(f)], Vec::new())),
        // src/bin/tool/main.rs (+ its modules) = binary target tool
        ["src", "bin", dir, rest @ ..] => {
            let crate_root = root.join("src/bin").join(dir).join("main.rs");
            Ok(c(crate_root, vec!["--bin".into(), dir.to_string()], module_path(rest)))
        }
        ["src", rest @ ..] => {
            let lib = root.join("src/lib.rs");
            let crate_root =
                if lib.is_file() && rest != ["main.rs"] { lib } else { root.join("src/main.rs") };
            let flag = if crate_root.ends_with("lib.rs") { vec!["--lib".to_string()] } else { Vec::new() };
            Ok(c(crate_root, flag, module_path(rest)))
        }
        _ => Err("tests live under src/ or tests/".into()),
    }
}

/// `a/b.rs` → [a, b] · `a/mod.rs` → [a] · `lib.rs`·`main.rs` → [].
fn module_path(rest: &[&str]) -> Vec<String> {
    let mut v: Vec<String> = rest.iter().map(|s| s.trim_end_matches(".rs").to_string()).collect();
    if v.len() == 1 && (v[0] == "lib" || v[0] == "main") {
        v.clear();
    }
    if v.last().is_some_and(|l| l == "mod") {
        v.pop();
    }
    v
}

/// Builds the test binary with `cargo test --no-run` and returns its path (on a worker thread).
pub fn cargo_test_binary(cwd: &Path, flag: &[String], crate_root: &Path) -> Result<PathBuf, String> {
    let root = std::fs::canonicalize(crate_root).unwrap_or_else(|_| crate_root.to_path_buf());
    let mut args = vec!["test", "--no-run"];
    args.extend(flag.iter().map(String::as_str));
    let pick = |v: &Value| {
        let src = v["target"]["src_path"].as_str().map(PathBuf::from);
        v["profile"]["test"] == true && src.is_some_and(|s| std::fs::canonicalize(&s).unwrap_or(s) == root)
    };
    crate::dap::cargo_artifact(&args, cwd, pick)?.ok_or_else(|| "no test binary for this file".to_string())
}

// ── Python ───────────────────────────────────────────────────────────────

/// The venv's python if any (`.venv`·`venv`·`$VIRTUAL_ENV`), else python3.
pub fn py_interpreter(root: &Path) -> String {
    let venv = std::env::var_os("VIRTUAL_ENV").map(PathBuf::from);
    [root.join(".venv"), root.join("venv")]
        .into_iter()
        .chain(venv)
        .map(|v| v.join("bin/python"))
        .find(|p| p.is_file())
        .map_or_else(|| "python3".to_string(), |p| p.display().to_string())
}

// ── Running ──────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RunState {
    Running,
    Passed,
    Failed(Option<i32>),
}

pub struct TestRun {
    pub target: Target,
    pub state: RunState,
    /// (is stderr, line).
    pub output: Vec<(bool, String)>,
    pub started: Instant,
    pub took: Option<Duration>,
    pid: Option<u32>,
    generation: u64,
    /// Java test debug: attach once the test JVM prints
    /// "Listening for transport dt_socket at address: N" (file, root).
    java_attach: Option<(PathBuf, PathBuf)>,
    /// Per-test results (parsed per line — Java: JUnit XML after finishing) · the one picked in the
    /// results pane.
    pub cases: Vec<crate::test_results::Case>,
    parser: crate::test_results::Parser,
    pub selected: usize,
    /// The wheel's first line of the pane's detail (`detail_top`); None = where it starts on its own.
    pub scroll: Option<usize>,
    /// Claude's `run_tests` waiting for this run's results (`tools.rs`).
    pub(crate) chat: Option<crate::tools::Call>,
}

impl TestRun {
    /// Indices of the failed ones.
    pub fn failures(&self) -> Vec<usize> {
        use crate::test_results::Status;
        self.cases.iter().enumerate().filter(|(_, c)| c.status == Status::Failed).map(|(i, _)| i).collect()
    }

    /// The failure the pane shows beside the list — None: it shows the output.
    pub fn picked(&self) -> Option<&crate::test_results::Case> {
        self.cases.get(self.selected).filter(|c| c.status == crate::test_results::Status::Failed)
    }

    /// Lines of the pane's detail: the picked failure's message (its placeholder when empty), else the output.
    pub fn detail_len(&self) -> usize {
        self.picked().map_or(self.output.len(), |c| c.message.len().max(1))
    }

    /// The detail's first line in `rows` rows: the wheel's, else a message's top / the output's end (so a
    /// run's newest lines stay in sight).
    pub fn detail_top(&self, rows: usize) -> usize {
        let max = self.detail_len().saturating_sub(rows);
        match self.scroll {
            Some(top) => top.min(max),
            None if self.picked().is_some() => 0,
            None => max,
        }
    }

    /// A finished run with these results — no process behind it (tests, snapshots).
    #[cfg(test)]
    pub(crate) fn finished(
        label: &str,
        cases: Vec<crate::test_results::Case>,
        output: Vec<String>,
    ) -> TestRun {
        let how = How::Pack {
            lang: "python".into(),
            argv: vec!["python".into()],
            debug: None,
            parse: "python".into(),
            build: None,
            debug_run: None,
            junit: None,
        };
        TestRun {
            parser: crate::test_results::Parser::for_lang(parser_of(&how)),
            target: Target { label: label.into(), cwd: std::env::temp_dir(), how },
            state: RunState::Failed(Some(1)),
            output: output.into_iter().map(|l| (false, l)).collect(),
            started: Instant::now(),
            took: None,
            pid: None,
            generation: 0,
            java_attach: None,
            cases,
            selected: 0,
            scroll: None,
            chat: None,
        }
    }

    /// The wheel over the pane: `by` lines down (up when negative). Back at the output's end, it follows
    /// the output again.
    pub fn scroll_by(&mut self, by: isize, rows: usize) {
        let max = self.detail_len().saturating_sub(rows);
        let top = self.detail_top(rows).saturating_add_signed(by).min(max);
        self.scroll = (self.picked().is_some() || top < max).then_some(top);
    }
}

impl Drop for TestRun {
    fn drop(&mut self) {
        self.kill();
    }
}

impl TestRun {
    fn kill(&mut self) {
        if let Some(pid) = self.pid.take()
            && self.state == RunState::Running
        {
            crate::dap::term_group(pid);
        }
    }
}

/// Strips color codes (`ESC [ … letter`).
pub(crate) fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c == '\x1b' && it.peek() == Some(&'[') {
            it.next();
            for d in it.by_ref() {
                if d.is_ascii_alphabetic() {
                    break;
                }
            }
        } else if c != '\r' {
            out.push(c);
        }
    }
    out
}

impl Editor {
    /// Test for the current file·cursor — if the grammar is missing, shows a download offer and returns None
    /// (`resume` continues once downloaded).
    fn test_target(&mut self, scope: Scope, resume: crate::offer::Resume) -> Option<Target> {
        let doc = self.doc();
        let Some(path) = doc.path.clone() else {
            self.set_status("save the file first");
            return None;
        };
        let spec = crate::syntax::detect(&path);
        if let Err(e) = supported(spec.map_or("", |s| s.name.as_str())) {
            self.set_status(e);
            return None;
        }
        let spec = spec.expect("supported");
        let lang = match crate::syntax::Loader::global().load(spec) {
            Ok(l) => l,
            Err(_) => {
                self.offer_grammar_then(spec, resume);
                return None;
            }
        };
        // The tree of the text as it is now: the doc's own, brought up to date from the edits since its last
        // parse (well under a ms) — a whole parse only without one (tens of ms on a big file)
        let src = doc.text.to_string();
        let cursor = doc.selection().primary().cursor(&doc.text);
        let current = doc
            .syntax
            .as_ref()
            .filter(|s| s.lang.name == lang.name)
            .and_then(|s| s.fresh_tree(&doc.text, Duration::from_millis(20)));
        let tree = current.or_else(|| {
            let mut parser = tree_sitter::Parser::new();
            parser.set_language(&lang.language).ok().and_then(|_| parser.parse(&src, None))
        });
        let Some(tree) = tree else {
            self.set_error(format!("test: couldn't parse {}", spec.name));
            return None;
        };
        match find(&spec.name, &path, &src, &tree, cursor, scope) {
            Ok(t) => Some(t),
            Err(e) => {
                self.set_status(e);
                None
            }
        }
    }

    /// `space x x` · `space x f`.
    pub fn test_run(&mut self, scope: Scope) {
        if let Some(t) = self.test_target(scope, crate::offer::Resume::Test(scope)) {
            self.test_start(t);
        }
    }

    /// `space x l` — rerun the last one (as debug if it was debug).
    pub fn test_rerun(&mut self) {
        match self.test_last.clone() {
            Some((t, false)) => self.test_start(t),
            Some((t, true)) => self.test_debug_target(t),
            None => self.set_status("no test run yet — space x x runs the test at the cursor"),
        }
    }

    /// `]x` · `[x` — go to a failed test's location (first the picked one; if already there, next·prev).
    /// The results pane picks it too.
    pub fn test_failure_step(&mut self, forward: bool) {
        let here = self.doc().path.clone().map(|p| {
            let d = self.doc();
            (p, d.text.byte_to_line(d.selection().primary().cursor(&d.text)))
        });
        let Some(r) = self.test_run.as_mut() else {
            return self.set_status("no test results — space x x runs a test");
        };
        let fails = r.failures();
        if fails.is_empty() {
            return self.set_status("no failed tests");
        }
        let at_selected = r.cases.get(r.selected).and_then(|c| c.at.clone()).is_some_and(|a| Some(a) == here);
        let pos = fails.iter().position(|&i| i == r.selected);
        let next = match (pos, at_selected) {
            (Some(p), true) => fails[(p + if forward { 1 } else { fails.len() - 1 }) % fails.len()],
            (Some(p), false) => fails[p],
            (None, _) => fails[0],
        };
        if r.selected != next {
            r.scroll = None;
        }
        r.selected = next;
        let case = r.cases[next].clone();
        let n = fails.iter().position(|&i| i == next).unwrap_or(0) + 1;
        if let Some((path, line)) = case.at {
            if let Err(e) = self.open(&path) {
                return self.set_error(format!("{e:#}"));
            }
            let doc = self.doc_mut();
            let line = line.min(crate::movement::last_line(&doc.text));
            let at = crate::movement::line_start(&doc.text, line);
            doc.set_selection(crate::selection::Selection::point(at));
        }
        let msg = case.message.first().map(|m| format!(" — {}", m.trim())).unwrap_or_default();
        self.note(format!("failure {n}/{} · {}{msg}", fails.len(), case.name));
    }

    /// `space x o` — the run's whole output in a buffer (search it, copy from it; the pane shows a window).
    pub fn test_output(&mut self) {
        let Some(r) = &self.test_run else {
            return self.set_status("no test run yet — space x x runs the test at the cursor");
        };
        let mut text: String = r.output.iter().map(|(_, l)| format!("{l}\n")).collect();
        if text.is_empty() {
            text = format!("(no output yet)\n$ {}\n", r.target.argv().join(" "));
        }
        // Opened at the picked failure's message, where the pane was looking
        let gist = r.picked().and_then(|c| c.message.first()).map(|m| m.trim()).filter(|m| !m.is_empty());
        let line = gist.and_then(|g| r.output.iter().position(|(_, l)| l.contains(g)));
        let title = format!("test output · {}", r.target.label);
        // One buffer per run's label: an earlier one gives way to this output
        let old: Vec<_> = self
            .docs
            .iter()
            .filter(|d| d.throwaway && d.title.as_deref() == Some(title.as_str()))
            .map(|d| d.id)
            .collect();
        for id in old {
            self.close_doc(id);
        }
        self.open_virtual(&title, &text);
        if let Some(line) = line {
            let doc = self.doc_mut();
            let at = crate::movement::line_start(&doc.text, line);
            doc.set_selection(crate::selection::Selection::point(at));
        }
    }

    /// `space x c` — closes the test pane (stops the run if it's going).
    pub fn test_close(&mut self) {
        crate::tools::tests_dropped(self, "the user closed it");
        self.test_run = None;
    }

    pub fn test_start(&mut self, target: Target) {
        // Save modified files first — tests run the code on disk
        self.auto_save(" · before test");
        self.flush_saves();
        self.test_last = Some((target.clone(), false));
        let argv = target.argv();
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..])
            .current_dir(&target.cwd)
            .env("NO_COLOR", "1")
            .env("CARGO_TERM_COLOR", "never")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let wall = std::time::SystemTime::now();
        let mut child = match crate::dap::new_session(&mut cmd).spawn() {
            Ok(c) => c,
            // (the previous run, if any, stays current — its lines·end still land)
            Err(e) => return self.set_error(format!("test: {}: {e}", argv[0])),
        };
        crate::tools::tests_dropped(self, "another run took its place");
        self.test_generation += 1;
        let generation = self.test_generation;
        let pid = child.id();
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let tx = self.events.sender();
        let send_lines =
            move |tx: &std::sync::mpsc::Sender<crate::event::Event>, err: bool, lines: Vec<String>| {
                let apply = move |ed: &mut Editor| {
                    let Some(r) = ed.test_run.as_mut().filter(|r| r.generation == generation) else { return };
                    let mut attach = None;
                    for line in lines {
                        let line = strip_ansi(&line);
                        // Java test debug: the test JVM waits for a debugger — attach to that port
                        let listen = line
                            .split_once("Listening for transport dt_socket at address:")
                            .and_then(|(_, a)| a.trim().rsplit(':').next()?.parse::<u16>().ok());
                        if let Some(port) = listen
                            && let Some(a) = r.java_attach.take()
                        {
                            attach = Some((a, port, r.target.label.clone()));
                        }
                        r.parser.line(&r.target.cwd, &line, &mut r.cases);
                        r.output.push((err, line));
                    }
                    let n = r.output.len();
                    if n > 5000 {
                        r.output.drain(..n - 5000);
                    }
                    if let Some(((file, root), port, label)) = attach {
                        ed.dap_generation += 1;
                        let g = ed.dap_generation;
                        ed.java_attach(file, root, "127.0.0.1".into(), port, g);
                        // Debug pane header = the test name, not the address
                        if let Some(d) = ed.dap.as_mut() {
                            d.program = label;
                        }
                    }
                };
                tx.send(crate::event::Event::Job(Box::new(apply))).is_ok()
            };
        // `parse = "junit"`: results from the JUnit XML the build tool leaves (only files written since this
        // run) — read here on the waiter thread once it ends
        let How::Pack { junit, .. } = &target.how;
        let junit = junit.clone();
        std::thread::spawn(move || {
            let err_reader = stderr.map(|e| {
                let tx = tx.clone();
                std::thread::spawn(move || crate::task::read_batches(e, |lines| send_lines(&tx, true, lines)))
            });
            if let Some(o) = stdout {
                crate::task::read_batches(o, |lines| send_lines(&tx, false, lines));
            }
            if let Some(h) = err_reader {
                let _ = h.join();
            }
            let status = child.wait().ok();
            let cases = junit.map(|(reports, src)| crate::test_results::junit(&reports, wall, &src));
            let done = move |ed: &mut Editor| {
                let Some(r) = ed.test_run.as_mut().filter(|r| r.generation == generation) else { return };
                if r.state != RunState::Running {
                    return;
                }
                r.took = Some(r.started.elapsed());
                r.pid = None;
                if let Some(cases) = cases {
                    r.cases = cases;
                }
                // Pick the first failure (right side of the results pane · ]x)
                r.selected = r.failures().first().copied().unwrap_or(0);
                r.scroll = None;
                r.state = match status.and_then(|s| s.code()) {
                    Some(0) => RunState::Passed,
                    code => RunState::Failed(code),
                };
                crate::tools::tests_done(ed);
            };
            let _ = tx.send(crate::event::Event::Job(Box::new(done)));
        });
        let parser = crate::test_results::Parser::for_lang(parser_of(&target.how));
        self.test_run = Some(TestRun {
            target,
            state: RunState::Running,
            output: Vec::new(),
            started: Instant::now(),
            took: None,
            pid: Some(pid),
            generation,
            java_attach: None,
            cases: Vec::new(),
            parser,
            selected: 0,
            scroll: None,
            chat: None,
        });
    }

    /// Debugging by running `argv` (a pack's `debug-args`): Java — the build tool starts the test JVM waiting
    /// for a debugger (build output in the test pane), and once it prints that it's waiting, java-debug
    /// attaches (debug pane).
    fn test_debug_by_running(&mut self, target: Target, argv: Vec<String>) {
        let Some(file) = self.doc().path.clone() else { return };
        let mut t = target.clone();
        let How::Pack { argv: run, .. } = &mut t.how;
        *run = argv;
        let root = target.cwd.clone();
        let before = self.test_generation;
        self.test_start(t);
        if self.test_generation == before {
            return; // couldn't launch (reason shown)
        }
        self.test_last = Some((target.clone(), true));
        self.dap_test = Some(target);
        if let Some(r) = self.test_run.as_mut() {
            r.java_attach = Some((file, root));
        }
        self.set_status("Building and starting the test JVM — attaching when it's ready");
    }

    /// `space x d` — debugs the test at the cursor (stops at breakpoints).
    pub fn test_debug(&mut self) {
        if let Some(t) = self.test_target(Scope::Nearest, crate::offer::Resume::TestDebug) {
            self.test_debug_target(t);
        }
    }

    pub fn test_debug_target(&mut self, target: Target) {
        // Save modified files first — tests run the code on disk
        self.auto_save(" · before test");
        self.flush_saves();
        self.test_last = Some((target.clone(), true));
        self.dap_test = Some(target.clone());
        self.dap_resume = Some(crate::offer::Resume::TestTarget(Box::new(target.clone())));
        let lang = lang_of(&target.how).to_string();
        let How::Pack { debug_run, .. } = &target.how;
        if let Some((argv, _)) = debug_run.clone() {
            return self.test_debug_by_running(target, argv);
        }
        let lang = lang.as_str();
        self.dap_generation += 1;
        let generation = self.dap_generation;
        let cwd = target.cwd.clone();
        let label = target.label.clone();
        match target.how {
            // The test binary built first (on a worker), its path put into the launch configuration
            How::Pack {
                debug: Some(launch),
                build: Some(crate::packs::Build::CargoTestBinary { flag, crate_root }),
                ..
            } => {
                self.dap_build_and_start(lang, label.clone(), cwd.clone(), generation, move || {
                    let exe = cargo_test_binary(&cwd, &flag, &crate_root)?;
                    Ok((with_binary(launch, &exe.display().to_string()), label))
                });
            }
            How::Pack { debug: Some(launch), .. } => self.dap_start(lang, launch, label, generation),
            How::Pack { debug: None, .. } => self.set_error(format!("the {lang} pack has no debug for this")),
        }
    }
}

/// A launch configuration with `{cargo-test-binary}` replaced by the binary built.
fn with_binary(v: Value, exe: &str) -> Value {
    match v {
        Value::String(s) => Value::String(s.replace("{cargo-test-binary}", exe)),
        Value::Array(a) => Value::Array(a.into_iter().map(|x| with_binary(x, exe)).collect()),
        Value::Object(o) => Value::Object(o.into_iter().map(|(k, x)| (k, with_binary(x, exe))).collect()),
        v => v,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("tarae-testing-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Parse with that language's grammar — None where the grammar isn't available (that test is skipped).
    fn parse(lang: &str, src: &str) -> Option<Tree> {
        let l = crate::syntax::Loader::global().load(crate::syntax::spec(lang)?).ok()?;
        let mut p = tree_sitter::Parser::new();
        p.set_language(&l.language).ok()?;
        p.parse(src, None)
    }

    /// Where `needle` first appears (cursor).
    fn at(src: &str, needle: &str) -> usize {
        src.find(needle).unwrap_or_else(|| panic!("{needle}"))
    }

    #[test]
    fn rust_finds_the_test_through_the_tree() {
        let d = tmp("rust");
        std::fs::write(d.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::create_dir_all(d.join("src/editor")).unwrap();
        let file = d.join("src/editor/keys.rs");
        let src = r####"
fn helper() { let s = "}"; let c = '}'; let r = r#"}"#; } // } 주석
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    // 표지와 함수 사이 주석
    fn undo_works() {
        assert!(true);
    }

    fn not_a_test() {
        let x = 1;
    }

    #[tokio::test]
    async fn async_one() {}
}
"####;
        let tree = parse("rust", src).expect("rust grammar is always built in");
        let find_at =
            |needle: &str, f: &Path| find("rust", f, src, &tree, at(src, needle), Scope::Nearest).unwrap();
        let t = find_at("assert!(true)", &file);
        assert_eq!(t.label, "editor::keys::tests::undo_works");
        assert_eq!(t.argv(), ["cargo", "test", "--", "editor::keys::tests::undo_works", "--exact"]);
        // On the attribute line too, and async tests too
        assert_eq!(find_at("#[tokio::test]", &file).label, "editor::keys::tests::async_one");
        // Inside a non-test function = the enclosing module
        let t = find_at("let x = 1", &file);
        assert_eq!(
            (t.label.as_str(), t.argv().last().unwrap().as_str()),
            ("editor::keys::tests::*", "editor::keys::tests::")
        );
        // Outside a module = the file
        assert_eq!(find_at("fn helper", &file).label, "editor::keys::*");
        // Integration tests use the --test target
        std::fs::create_dir_all(d.join("tests")).unwrap();
        let t = find_at("assert!", &d.join("tests/api.rs"));
        assert_eq!(t.argv(), ["cargo", "test", "--test", "api", "--", "tests::undo_works", "--exact"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The Rust pack's query: a test attribute makes the function right after it (other attributes and
    /// comments between) a test, and no other function; debugging builds the test binary first.
    #[test]
    fn rust_test_attributes_belong_to_their_function() {
        let d = tmp("rust-attrs");
        std::fs::write(d.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src/lib.rs"), "").unwrap();
        let file = d.join("src/lib.rs");
        let src = "#[test]\nfn a() {}\n\nfn b() { let in_b = 1; }\n\n#[test]\n#[should_panic]\nfn c() { let in_c = 1; }\n\n#[cfg(test)]\nfn d() { let in_d = 1; }\n\n#[cfg(test)]\nmod tests {\n    #[ignore]\n    #[test]\n    fn e() { let in_e = 1; }\n}\n";
        let tree = parse("rust", src).expect("rust grammar is always built in");
        let find_at =
            |needle: &str| find("rust", &file, src, &tree, at(src, needle), Scope::Nearest).unwrap();
        assert_eq!(find_at("in_b").label, "all tests", "b isn't a test: a's attribute is a's");
        assert_eq!(find_at("in_c").label, "c", "#[should_panic] between");
        assert_eq!(find_at("in_d").label, "all tests", "#[cfg(test)] isn't a test attribute");
        assert_eq!(find_at("#[cfg(test)]\nmod").label, "tests::*", "a module from its attribute");
        assert_eq!(find_at("#[ignore]").label, "tests::e", "an attribute above the test one");
        assert_eq!(find_at("in_e").label, "tests::e", "once, not tests::tests::e");
        let t = find_at("in_c");
        assert_eq!(t.argv(), ["cargo", "test", "--lib", "--", "c", "--exact"]);
        let How::Pack { debug: Some(launch), build: Some(build), .. } = t.how else {
            panic!("a build to debug")
        };
        assert_eq!(
            build,
            crate::packs::Build::CargoTestBinary {
                flag: vec!["--lib".into()],
                crate_root: d.join("src/lib.rs")
            }
        );
        assert_eq!(launch["program"], "{cargo-test-binary}", "filled in once built");
        assert_eq!(launch["args"], json!(["c", "--exact", "--nocapture", "--test-threads=1"]));
        let launch = with_binary(launch, "/t/x-123");
        assert_eq!(launch["program"], "/t/x-123");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn go_picks_the_enclosing_test_func() {
        let src = "package p\n\nfunc helper() {}\n\nfunc TestParse(t *testing.T) {\n\tx := 1\n}\n\nfunc TestLex(t *testing.T) {}\n\nfunc Testify() {}\n\nfunc BenchmarkParse(b *testing.B) {}\n";
        let Some(tree) = parse("go", src) else { return };
        let f = Path::new("/p/parse_test.go");
        let find_at = |needle: &str| find("go", f, src, &tree, at(src, needle), Scope::Nearest).unwrap();
        assert_eq!(find_at("x := 1").argv(), ["go", "test", "-v", "-run", "^TestParse$", "."]);
        assert_eq!(
            find_at("BenchmarkParse").argv(),
            ["go", "test", "-v", "-run", "^$", "-bench", "^BenchmarkParse$", "."]
        );
        assert_eq!(
            find_at("helper").argv()[4],
            "^(TestParse|TestLex)$",
            "outside a test = the file (Testify is not a test)"
        );
        assert!(find("go", Path::new("/p/parse.go"), src, &tree, 0, Scope::Nearest).is_err());
        assert_eq!(
            find_at("Testify").label,
            "parse_test.go",
            "Test + a lowercase letter isn't a test (go's rule)"
        );
        // Debugging: the pack's launch configuration, filled in
        let How::Pack { debug: Some(launch), parse, .. } = find_at("x := 1").how else {
            panic!("from the pack")
        };
        assert_eq!(parse, "go");
        assert_eq!(
            launch,
            json!({ "mode": "test", "program": "/p", "cwd": "/p", "args": ["-test.run", "^TestParse$", "-test.v"], "outputMode": "remote" })
        );
    }

    /// A pack of one's own (as it would be in `~/.local/share/tarae/packs/go/`): run from the module's root, with
    /// other flags — no code involved.
    #[test]
    fn a_pack_of_ones_own() {
        let src = "package p\n\nfunc TestA(t *testing.T) {\n\tx := 1\n}\n";
        let Some(tree) = parse("go", src) else { return };
        let d = tmp("ownpack");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("go.mod"), "module m\n").unwrap();
        let toml = r#"
            [test]
            query = "q.scm"
            cwd = { nearest = ["go.mod"] }
            parse = "go"
            run = ["go", "test", "-race", "-run", "^{name}$", "./..."]
            run-file = ["go", "test", "{file}"]
        "#;
        let query = "((function_declaration name: (identifier) @name) @test (#match? @name \"^Test\"))";
        let pack =
            crate::packs::Tests::parse(toml, &tree.language(), |f| (f == "q.scm").then(|| query.to_string()))
                .unwrap();
        let file = d.join("sub/a_test.go");
        let f = pack.find(&file, src, &tree, at(src, "x := 1"), false).unwrap();
        assert_eq!(f.cwd, d, "the folder with go.mod");
        assert_eq!(f.argv, ["go", "test", "-race", "-run", "^TestA$", "./..."]);
        assert_eq!(f.debug, None, "no `debug` in this pack");
        let whole = pack.find(&file, src, &tree, 0, true).unwrap();
        assert_eq!(whole.argv, ["go", "test", &file.display().to_string()]);
        // Mistakes in a pack are said, not crashed on
        let bad = |t: &str| {
            crate::packs::Tests::parse(t, &tree.language(), |_| Some(query.to_string())).err().unwrap()
        };
        assert!(bad("[test]\nrun = 1").contains("query"), "no query");
        assert!(bad("[test]\nquery = \"q\"\nrun = 1").contains("`run` isn't a list"));
        // …and so are the quiet ones: a typo, a parser tarae hasn't, debugging with no way to attach
        assert!(bad("[test]\nquery = \"q\"\nrunn = []").contains("no `runn`"));
        assert!(bad("[test]\nquery = \"q\"\nparse = \"Junit\"").contains("parse"));
        assert!(bad("[test]\nquery = \"q\"\ndebug-args = [\"-d\"]").contains("debug-attach"));
        assert!(bad("[test]\nquery = \"q\"\n[test.kind.x]\nfile = \"no\"").contains("true or false"));
        assert!(
            bad("[test]\nquery = \"q\"\nparse = \"junit\"\n[test.variant.a]\nreports = [\"r\"]")
                .contains("reports"),
            "variants with no variant-by"
        );
        let unfilled = r#"
            [test]
            query = "q.scm"
            cwd = "{java-root}"
            run = ["x"]
        "#;
        let pack =
            crate::packs::Tests::parse(unfilled, &tree.language(), |_| Some(query.to_string())).unwrap();
        assert!(pack.find(&file, src, &tree, 0, true).unwrap_err().contains("no helper"));
        assert!(
            crate::packs::Tests::parse("[test]\nquery = \"q\"", &tree.language(), |_| Some(
                "(nonsense".into()
            ))
            .is_err()
        );
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn the_vitest_pack_in_the_docs_works() {
        // The example as the docs have it
        let md = include_str!("../docs/testing-and-debugging.md");
        let block = |fence: &str, first: &str| {
            md.split(fence)
                .skip(1)
                .map(|b| b.split("```").next().unwrap())
                .find(|b| b.contains(first))
                .unwrap()
        };
        let toml = block("```toml\n", "typescript/pack.toml");
        let query = block("```scheme\n", "typescript/tests.scm").to_string();
        let src = "import { describe, it, expect } from \"vitest\";\n\ndescribe(\"math\", () => {\n  it(\"adds\", () => {\n    expect(1 + 1).toBe(2);\n  });\n  it(\"adds (1+1)?\", () => {});\n\n  describe(\"edge\", () => {\n    it(\"fails\", () => {\n      expect(1 + 2).toBe(4);\n    });\n  });\n});\n\ntest(\"top level\", () => {});\n";
        let Some(tree) = parse("typescript", src) else { return };
        let pack = crate::packs::Tests::parse(toml, &tree.language(), |_| Some(query.clone())).unwrap();
        let d = tmp("vitest");
        std::fs::write(d.join("package.json"), "{}").unwrap();
        std::fs::create_dir_all(d.join("src")).unwrap();
        let file = d.join("src/math.test.ts");
        let f = pack.find(&file, src, &tree, at(src, "1 + 2"), false).unwrap();
        assert_eq!((f.label.as_str(), f.cwd.as_path()), ("math > edge > fails", d.as_path()));
        assert_eq!(&f.argv[..6], ["npx", "vitest", "run", "src/math.test.ts", "-t", "^math > edge > fails$"]);
        assert_eq!(f.junit, Some((vec![d.join("node_modules/.cache/tarae")], vec![d.clone()])));
        let g = pack.find(&file, src, &tree, at(src, "describe(\"edge"), false).unwrap();
        assert_eq!((g.label.as_str(), g.argv[5].as_str()), ("math > edge", "^math > edge > "));
        assert_eq!(
            pack.find(&file, src, &tree, at(src, "test(\"top"), false).unwrap().argv[5],
            "^top level$"
        );
        let odd = pack.find(&file, src, &tree, at(src, "(1+1)"), false).unwrap();
        assert_eq!(odd.argv[5], r"^math > adds \(1\+1\)\?$", "escaped for -t's regular expression");
        assert_eq!(pack.find(&file, src, &tree, 0, true).unwrap().argv.len(), 7, "the file: no -t");
        assert!(pack.find(&d.join("src/math.ts"), src, &tree, 0, true).is_err(), "not a test file");
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn python_builds_pytest_node_ids() {
        let d = tmp("py");
        std::fs::write(d.join("pyproject.toml"), "").unwrap();
        std::fs::create_dir_all(d.join("tests")).unwrap();
        let file = d.join("tests/test_math.py");
        let src = "import x\n\ndef test_add():\n    def inner():\n        return 1\n    assert inner() == 1\n\nclass TestDiv:\n    def helper(self):\n        pass\n\n    @pytest.mark.slow\n    def test_zero(self):\n        # 주석\n        assert True\n";
        let Some(tree) = parse("python", src) else { return };
        // The node id is also the label
        let node =
            |needle: &str| find("python", &file, src, &tree, at(src, needle), Scope::Nearest).unwrap().label;
        assert_eq!(node("return 1"), "tests/test_math.py::test_add", "the outer test, not the inner helper");
        assert_eq!(node("assert True"), "tests/test_math.py::TestDiv::test_zero");
        assert_eq!(node("@pytest"), "tests/test_math.py::TestDiv::test_zero", "on the decorator too");
        assert_eq!(node("pass"), "tests/test_math.py::TestDiv", "non-test method = its class");
        assert_eq!(node("import x"), "tests/test_math.py");
        // Nested test classes are all in the node id
        let src =
            "class TestOuter:\n    class TestInner:\n        def test_deep(self):\n            assert 1\n";
        let Some(tree) = parse("python", src) else { return };
        let t = find("python", &file, src, &tree, at(src, "assert 1"), Scope::Nearest).unwrap();
        assert_eq!(t.label, "tests/test_math.py::TestOuter::TestInner::test_deep");
        assert_eq!(t.cwd, d, "the folder with pyproject.toml");
        assert_eq!(&t.argv()[1..4], ["-m", "pytest", "tests/test_math.py::TestOuter::TestInner::test_deep"]);
        // unittest classes of any name, and classes from their decorators, as pytest collects them
        let src = "class MathTests(unittest.TestCase):\n    def test_add(self):\n        assert 1\n\n@pytest.mark.usefixtures('x')\nclass TestDeco(TestCase):\n    def helper(self):\n        pass\n";
        let Some(tree) = parse("python", src) else { return };
        let node =
            |needle: &str| find("python", &file, src, &tree, at(src, needle), Scope::Nearest).unwrap().label;
        assert_eq!(node("assert 1"), "tests/test_math.py::MathTests::test_add");
        assert_eq!(node("@pytest"), "tests/test_math.py::TestDeco", "on its decorator");
        assert_eq!(node("pass"), "tests/test_math.py::TestDeco", "matched twice, named once");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn java_runs_through_gradle_or_maven_with_nested_classes() {
        let src = "package demo;\n\nimport org.junit.jupiter.api.*;\n\nclass AppTest {\n    @Test void greets() {\n        int x = 1;\n    }\n\n    void helper() {}\n\n    @Nested\n    class Edge {\n        @ParameterizedTest\n        void empty() {}\n    }\n}\n";
        let Some(tree) = parse("java", src) else { return };
        let d = tmp("java");
        std::fs::write(d.join("settings.gradle.kts"), "").unwrap();
        std::fs::write(d.join("gradlew"), "").unwrap();
        std::fs::create_dir_all(d.join("app/src/test/java/demo")).unwrap();
        std::fs::write(d.join("app/build.gradle.kts"), "").unwrap();
        let file = d.join("app/src/test/java/demo/AppTest.java");
        let find_at = |needle: &str, scope| find("java", &file, src, &tree, at(src, needle), scope).unwrap();
        let t = find_at("int x", Scope::Nearest);
        assert_eq!(t.label, "AppTest.greets");
        let gw = d.join("gradlew").display().to_string();
        assert_eq!(
            t.argv(),
            [gw.as_str(), ":app:test", "--rerun", "--console=plain", "--tests", "demo.AppTest.greets"]
        );
        assert_eq!(t.cwd, d, "runs from the multi-module top");
        assert_eq!(find_at("void empty", Scope::Nearest).argv()[5], "demo.AppTest$Edge.empty", "nested: $");
        assert_eq!(
            find_at("@ParameterizedTest", Scope::Nearest).label,
            "AppTest.Edge.empty",
            "on the annotation too"
        );
        assert_eq!(
            find_at("void helper", Scope::Nearest).argv()[5],
            "demo.AppTest",
            "non-test method = its class"
        );
        assert_eq!(find_at("int x", Scope::File).argv()[5], "demo.AppTest");
        assert_eq!(find_at("void helper", Scope::Nearest).label, "AppTest");
        // Debugging: the same run, waiting for a debugger; results from the module's JUnit XML
        let How::Pack { debug_run: Some((argv, attach)), junit: Some((reports, sources)), .. } =
            find_at("int x", Scope::Nearest).how
        else {
            panic!("no debug run or junit")
        };
        assert_eq!((argv.last().map(String::as_str), attach.as_str()), (Some("--debug-jvm"), "java"));
        assert_eq!(reports, [d.join("app/build/test-results/test")]);
        assert_eq!(sources, [d.join("app/src/test/java"), d.join("app/src/main/java")]);
        // Maven: -pl module -Dtest=Class#method
        std::fs::remove_file(d.join("settings.gradle.kts")).unwrap();
        std::fs::remove_file(d.join("app/build.gradle.kts")).unwrap();
        std::fs::write(d.join("pom.xml"), "").unwrap();
        std::fs::write(d.join("app/pom.xml"), "").unwrap();
        let t = find_at("int x", Scope::Nearest);
        assert_eq!(
            t.argv(),
            [
                "mvn",
                "-B",
                "test",
                "-pl",
                "app",
                "-Dtest=demo.AppTest#greets",
                "-Dsurefire.failIfNoSpecifiedTests=false"
            ]
        );
        assert_eq!(find_at("void empty", Scope::Nearest).argv()[5], "-Dtest=demo.AppTest$Edge#empty");
        let How::Pack { debug_run: Some((argv, _)), junit: Some((reports, _)), .. } = t.how else {
            panic!("no debug run or junit")
        };
        assert_eq!(argv.last().map(String::as_str), Some("-Dmaven.surefire.debug"));
        assert_eq!(reports, [d.join("app/target/surefire-reports")]);
        // The package from the tree: not one in a comment, not the rest of its line
        let src = "/* package old; */\npackage demo; import org.junit.jupiter.api.*; // x\nclass T { @Test void t() {} }\n";
        let Some(tree) = parse("java", src) else { return };
        let t = find("java", &file, src, &tree, at(src, "void t"), Scope::Nearest).unwrap();
        assert_eq!(t.argv()[5], "-Dtest=demo.T#t");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn rust_binaries_in_their_own_folder() {
        let d = tmp("rust-bin");
        std::fs::write(d.join("Cargo.toml"), "").unwrap();
        std::fs::create_dir_all(d.join("src/bin/tool")).unwrap();
        let c = rust_crate(&d.join("src/bin/tool/main.rs")).unwrap();
        assert_eq!(
            (c.crate_root, c.flag, c.mods),
            (d.join("src/bin/tool/main.rs"), vec!["--bin".into(), "tool".into()], vec![])
        );
        let c = rust_crate(&d.join("src/bin/tool/util.rs")).unwrap();
        assert_eq!((c.flag, c.mods), (vec!["--bin".to_string(), "tool".into()], vec!["util".to_string()]));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_run_that_cannot_launch_leaves_the_previous_one_current() {
        let d = tmp("spawn");
        let mut ed = Editor::new(crate::config::Config::default());
        let target = |python: &str| Target {
            label: "t".into(),
            cwd: d.clone(),
            how: How::Pack {
                lang: "python".into(),
                argv: vec![python.into(), "-m".into(), "pytest".into(), "x".into()],
                debug: None,
                parse: "python".into(),
                build: None,
                debug_run: None,
                junit: None,
            },
        };
        // `sh -m pytest …` — ends at once (no such script)
        ed.test_start(target("sh"));
        ed.test_start(target("/nonexistent/python"));
        assert!(ed.status.as_ref().is_some_and(|s| s.0.contains("/nonexistent/python")), "{:?}", ed.status);
        assert_eq!(ed.test_run.as_ref().map(|r| r.generation), Some(ed.test_generation));
        // …so its end still lands (not "running" forever)
        let end = Instant::now() + Duration::from_secs(10);
        while ed.test_run.as_ref().is_some_and(|r| r.state == RunState::Running) {
            let ev = ed.events.recv_timeout(end.saturating_duration_since(Instant::now())).expect("event");
            ed.handle_event(ev);
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn strips_color_codes() {
        assert_eq!(strip_ansi("\x1b[32mok\x1b[0m done\r"), "ok done");
    }

    use TestRun as R;
    fn finished(cases: Vec<crate::test_results::Case>, output: Vec<String>) -> TestRun {
        R::finished("t", cases, output)
    }

    fn failed(name: &str, lines: usize, at: Option<(PathBuf, usize)>) -> crate::test_results::Case {
        crate::test_results::Case {
            name: name.into(),
            status: crate::test_results::Status::Failed,
            secs: None,
            message: (0..lines).map(|i| format!("{name} line {i}")).collect(),
            at,
        }
    }

    /// The pane's detail: a message starts at its top and stops at its end; the output follows its end until
    /// the wheel moves it, and follows again once the wheel is back at the end.
    #[test]
    fn the_detail_scrolls_within_its_lines() {
        let mut r = finished(Vec::new(), (0..100).map(|i| format!("out {i}")).collect());
        assert_eq!(r.detail_top(10), 90);
        r.scroll_by(-3, 10);
        assert_eq!(r.detail_top(10), 87);
        r.output.push((false, "later".into()));
        assert_eq!(r.detail_top(10), 87, "held where the wheel left it");
        r.scroll_by(100, 10);
        assert_eq!((r.scroll, r.detail_top(10)), (None, 91), "back at the end: follows it");

        let mut r = finished(vec![failed("a", 30, None)], Vec::new());
        assert_eq!(r.detail_top(10), 0);
        r.scroll_by(5, 10);
        assert_eq!(r.detail_top(10), 5);
        r.scroll_by(100, 10);
        assert_eq!(r.detail_top(10), 20, "the message's last rows");
        r.scroll_by(-100, 10);
        assert_eq!(r.detail_top(10), 0);
    }

    /// The wheel over the test panel scrolls the failure's message (not elsewhere); `]x` to another failure
    /// starts it at its top; `space x o` opens the whole output at the picked failure.
    #[test]
    fn the_wheel_scrolls_the_panel_and_the_output_opens_in_a_buffer() {
        use crate::event::{Mouse, MouseKind as K};
        let d = tmp("panel");
        // (plain text — nothing to start a language server for)
        let file = d.join("t.txt");
        std::fs::write(&file, "test a\ntest b\n").unwrap();
        let file = std::fs::canonicalize(file).unwrap();
        let mut ed = Editor::new(crate::config::Config::default());
        let output = ["collected 2", "a line 0", "a line 1", "b line 0", "2 failed"];
        ed.test_run = Some(finished(
            vec![failed("a", 30, Some((file.clone(), 0))), failed("b", 3, Some((file.clone(), 1)))],
            output.iter().map(|l| l.to_string()).collect(),
        ));
        ed.screen.test_panel.set(Some((20, 30, 9)));
        let wheel = |ed: &mut Editor, y| ed.handle_mouse(Mouse { kind: K::ScrollDown, x: 5, y, alt: false });
        wheel(&mut ed, 25);
        assert_eq!(ed.test_run.as_ref().unwrap().detail_top(9), 3);
        wheel(&mut ed, 5);
        assert_eq!(
            ed.test_run.as_ref().unwrap().detail_top(9),
            3,
            "the wheel over the code isn't the panel's"
        );
        // First `]x` goes to the picked one (a), the next to b — whose message starts at its top
        ed.test_failure_step(true);
        assert_eq!(ed.test_run.as_ref().unwrap().detail_top(9), 3, "still a");
        ed.test_failure_step(true);
        let r = ed.test_run.as_ref().unwrap();
        assert_eq!((r.selected, r.scroll), (1, None));

        ed.test_output();
        ed.test_output();
        let outputs = ed.docs.iter().filter(|d| d.title.as_deref() == Some("test output · t")).count();
        assert_eq!(outputs, 1, "opened again, it's the same one");
        let doc = ed.doc();
        assert_eq!(doc.title.as_deref(), Some("test output · t"));
        assert_eq!(doc.text.to_string(), output.map(|l| format!("{l}\n")).concat());
        assert_eq!(doc.text.byte_to_line(doc.selection().primary().head), 3, "at b's message");
        let _ = std::fs::remove_dir_all(&d);
    }
}
