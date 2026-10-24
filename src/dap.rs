//! Debugger (Debug Adapter Protocol). Which debugger a language has, how it's found and started, how a
//! file is launched (built first?) and a program attached to: its pack's `[debug]` and
//! `[debugger.<name>]` (`debugpack.rs`); Java's way through jdtls is `java.rs`.
//!
//! Flow: (build first, if the pack says so) → launch the adapter → initialize → launch → on the
//! `initialized` event, all breakpoints + configurationDone → on stop (`stopped`), call stack → top frame's
//! variables → the editor jumps to that line.
//! Reading is on a thread (same `Content-Length` framing as LSP); writes are small messages, sent from main.
//! Screen (breakpoints ●·stopped line ▶·variable values at line end·debug pane below) is in term.rs.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::editor::Editor;
use crate::event::Event;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Building,
    Starting,
    Running,
    Stopped { reason: String },
    Exited(Option<i64>),
}

#[derive(Clone, Debug)]
pub struct Frame {
    pub id: i64,
    pub name: String,
    pub path: Option<PathBuf>,
    /// 0-based.
    pub line: usize,
}

#[derive(Clone, Debug)]
pub struct Var {
    pub name: String,
    pub value: String,
    pub ty: String,
    /// Expandable value if nonzero (struct·list).
    pub reference: i64,
}

pub struct Dap {
    writer: Box<dyn Write + Send>,
    child: Option<Child>,
    /// Helper launched before attaching (`kubectl port-forward` etc. — attach.rs) — stopped with the debug run.
    pub(crate) helper: Option<Child>,
    /// Request sent after initialize — "launch" (start it) · "attach" (attach to a running one).
    pub(crate) request: &'static str,
    seq: i64,
    /// Request seq → command name.
    pending: HashMap<i64, String>,
    pub state: State,
    pub thread: Option<i64>,
    pub frames: Vec<Frame>,
    pub vars: Vec<Var>,
    /// The frame being viewed (watches are evaluated in it).
    pub frame: Option<i64>,
    /// Watch expression → value.
    pub watches: HashMap<String, Watch>,
    /// Request seq → what that request covered (setBreakpoints = file, evaluate = watch).
    bp_requests: HashMap<i64, PathBuf>,
    watch_requests: HashMap<i64, String>,
    /// (category stdout·stderr·console, line).
    pub output: Vec<(String, String)>,
    /// What's running (for display).
    pub program: String,
    launch: Value,
    /// The adapter's last stderr lines — why it died if it exits before starting.
    adapter_err: Arc<Mutex<Vec<String>>>,
    /// Its own output lines, as its pack says (dropped, dimmed).
    pub(crate) quiet: crate::debugpack::Quiet,
}

/// One breakpoint — condition·log message are optional.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Bp {
    /// Stops only when this expression is true (`space G C-c`).
    pub condition: Option<String>,
    /// Prints this text to output instead of stopping — a logpoint (`space G C-l`, `{expr}` = its value).
    pub log: Option<String>,
    /// Why the adapter rejected it (bad condition etc.) — shown at the end of that line.
    pub rejected: Option<String>,
}

/// Breakpoints: file → line (0-based) → breakpoint. Kept even without a debug run.
pub type Breakpoints = BTreeMap<PathBuf, BTreeMap<usize, Bp>>;

/// A watch's value — re-evaluated on every stop.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Watch {
    /// Running, or still evaluating.
    Pending,
    Value {
        value: String,
        ty: String,
    },
    Error(String),
}

impl Dap {
    fn send(&mut self, command: &str, arguments: Value) -> i64 {
        self.seq += 1;
        self.write(json!({ "seq": self.seq, "type": "request", "command": command, "arguments": arguments }));
        self.pending.insert(self.seq, command.to_string());
        self.seq
    }

    /// One framed message (same `Content-Length` framing as LSP).
    fn write(&mut self, msg: Value) {
        let body = msg.to_string();
        let _ = write!(self.writer, "Content-Length: {}\r\n\r\n{body}", body.len())
            .and_then(|_| self.writer.flush());
    }

    pub fn stopped(&self) -> bool {
        matches!(self.state, State::Stopped { .. })
    }

    /// Where it's stopped now (top frame).
    pub fn stop_at(&self) -> Option<(&Path, usize)> {
        if !self.stopped() {
            return None;
        }
        let f = self.frames.first()?;
        Some((f.path.as_deref()?, f.line))
    }
}

/// A writer whose messages go out on a thread of its own (like a language server's) — an adapter that
/// stops reading, its pipe or socket full, holds that thread, never the editor. What's written collects
/// until `flush`, which hands it over as one message.
struct Queued {
    buf: Vec<u8>,
    tx: std::sync::mpsc::Sender<Vec<u8>>,
}

impl Write for Queued {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let msg = std::mem::take(&mut self.buf);
        self.tx.send(msg).map_err(|_| std::io::ErrorKind::BrokenPipe.into())
    }
}

fn queued(mut out: Box<dyn Write + Send>) -> Box<dyn Write + Send> {
    let (tx, rx) = std::sync::mpsc::channel::<Vec<u8>>();
    crate::event::thread("tarae-dap-write", move || {
        for msg in rx {
            if out.write_all(&msg).and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    });
    Box::new(Queued { buf: Vec::new(), tx })
}

impl Drop for Dap {
    fn drop(&mut self) {
        if let Some(c) = self.child.take() {
            reap(c);
        }
        if let Some(h) = &mut self.helper {
            kill_group(h);
        }
    }
}

impl Dap {
    /// (the adapter's process, the helper's) — `:procs`.
    pub fn pids(&self) -> (Option<u32>, Option<u32>) {
        (self.child.as_ref().map(Child::id), self.helper.as_ref().map(Child::id))
    }
}

// ── Process groups ───────────────────────────────────────────────────────

/// Launch in a process group of its own, with no controlling terminal (`setsid(2)`) — the process and whatever it starts (debuggee·test binaries) can't
/// touch our controlling terminal and steal key input (measured: a program launched by debugpy grabbed
/// it and later keys stopped arriving), and `term_group` stops them all together.
pub(crate) fn setsid(c: &mut Command) -> &mut Command {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid and `restore`'s setrlimit are safe to call between fork and exec.
        unsafe {
            c.pre_exec(|| {
                libc::setsid();
                // (the descriptor limit tarae started with — `nofile`)
                crate::nofile::restore();
                Ok(())
            });
        }
    }
    c
}

/// SIGTERM to the process group of a process launched with `setsid`. A sandboxed run on Linux is signalled by
/// its session's keeper, its reaper — tarae's kill could reach a group whose pid was reused once it reaped it.
pub(crate) fn term_group(pid: u32) {
    #[cfg(target_os = "linux")]
    if crate::sandbox::keeper::signal(pid, libc::SIGTERM) {
        return;
    }
    #[cfg(unix)]
    // SAFETY: only sends a signal to the process group we launched with `setsid`.
    unsafe {
        libc::kill(-(pid as i32), libc::SIGTERM);
    }
}

/// Stops a `setsid` process with its whole group and reaps it.
pub(crate) fn kill_group(c: &mut Child) {
    term_group(c.id());
    let _ = c.kill();
    let _ = c.wait();
}

/// Gives the adapter up to 1 s to finish on its own (after disconnect), then stops its group
/// (the debuggee too) and reaps it — on a thread.
fn reap(mut c: Child) {
    crate::event::thread("tarae-reap", move || {
        let end = Instant::now() + Duration::from_secs(1);
        while Instant::now() < end && matches!(c.try_wait(), Ok(None)) {
            std::thread::sleep(Duration::from_millis(20));
        }
        kill_group(&mut c);
    });
}

// ── Adapters ─────────────────────────────────────────────────────────────

/// An adapter process with its transport ready (made on a worker thread), and its pack's say.
pub(crate) struct Started {
    reader: Box<dyn std::io::Read + Send>,
    writer: Box<dyn Write + Send>,
    child: Child,
    err: Arc<Mutex<Vec<String>>>,
    debugger: crate::debugpack::Debugger,
}

/// Launches the adapter (`cmd`, as `debugger`'s pack says) in a process group of its own (`setsid`) in `cwd`
/// and connects to it (worker thread — a TCP port takes a moment to come up).
fn spawn_adapter(
    (debugger, cmd): (crate::debugpack::Debugger, Vec<String>),
    cwd: &Path,
) -> Result<Started, String> {
    let mut c = Command::new(&cmd[0]);
    // The adapter runs in the project folder (the same real path as launch's cwd — if paths diverge,
    // like macOS /tmp ↔ /private/tmp, dlv's go build fails with "outside main module", measured)
    c.args(&cmd[1..]).current_dir(cwd);
    let tcp = debugger.tcp();
    let port = tcp
        .then(|| std::net::TcpListener::bind("127.0.0.1:0").ok()?.local_addr().ok().map(|a| a.port()))
        .flatten();
    if let Some(listen) = port.and_then(|p| debugger.listen(p)) {
        c.args(listen);
    }
    c.stdin(Stdio::piped()).stdout(if tcp { Stdio::null() } else { Stdio::piped() }).stderr(Stdio::piped());
    let mut child = setsid(&mut c).spawn().map_err(|e| format!("{}: {e}", cmd[0]))?;
    // Keep the last stderr lines (the reason if it dies — `No module named debugpy` etc.)
    let err = Arc::new(Mutex::new(Vec::new()));
    if let Some(e) = child.stderr.take() {
        let err = err.clone();
        crate::event::thread("tarae-dap-err", move || {
            for l in BufReader::new(e).lines().map_while(Result::ok).filter(|l| !l.trim().is_empty()) {
                let mut v = err.lock().unwrap();
                v.push(l);
                let n = v.len();
                if n > 5 {
                    v.drain(..n - 5);
                }
            }
        });
    }
    // Transport: pipes for stdio; for TCP, keep poking briefly until the adapter is up
    let (reader, writer): (Box<dyn std::io::Read + Send>, Box<dyn Write + Send>) = match port {
        Some(p) => {
            let started = Instant::now();
            let s = loop {
                if let Ok(s) = std::net::TcpStream::connect(("127.0.0.1", p)) {
                    break s;
                }
                if started.elapsed() > Duration::from_secs(2) || !matches!(child.try_wait(), Ok(None)) {
                    kill_group(&mut child);
                    return Err(with_reason("adapter did not start", &err.lock().unwrap()));
                }
                std::thread::sleep(Duration::from_millis(40));
            };
            let r = s.try_clone().map_err(|e| e.to_string())?;
            (Box::new(r), Box::new(s))
        }
        None => {
            (Box::new(child.stdout.take().expect("stdout")), Box::new(child.stdin.take().expect("stdin")))
        }
    };
    Ok(Started { reader, writer, child, err, debugger })
}

/// `what` + the reason lines below it (if any).
pub(crate) fn with_reason(what: &str, why: &[impl AsRef<str>]) -> String {
    let why: Vec<&str> = why.iter().map(AsRef::as_ref).collect();
    if why.is_empty() { what.to_string() } else { format!("{what}\n{}", why.join("\n")) }
}

/// Color scope chosen by the value's shape (string·number·boolean·other).
pub fn value_scope(v: &str) -> &'static str {
    let t = v.trim();
    if t.starts_with('"') || t.starts_with('\'') {
        "string"
    } else if t == "true" || t == "false" || t == "True" || t == "False" || t == "None" || t == "nil" {
        "constant.builtin"
    } else if t.parse::<f64>().is_ok() || t.starts_with("0x") {
        "constant.numeric"
    } else {
        "ui.text"
    }
}

impl Editor {
    // ── Breakpoints ──────────────────────────────────────────────────────

    /// `F9` · `space G b` — toggles a breakpoint on the current line (sent to the adapter right away
    /// during a debug run).
    pub fn toggle_breakpoint(&mut self) {
        let Some((path, line)) = self.cursor_file_line() else {
            return self.set_status("save the file first");
        };
        let set = self.breakpoints.entry(path.clone()).or_default();
        if set.remove(&line).is_none() {
            set.insert(line, Bp::default());
        }
        self.dap_send_breakpoints(&path);
    }

    fn cursor_file_line(&self) -> Option<(PathBuf, usize)> {
        let doc = self.doc();
        let path = doc.path.clone()?;
        Some((path, doc.text.byte_to_line(doc.selection().primary().head.min(doc.text.len_bytes()))))
    }

    /// Condition / log message of the current line's breakpoint (prefills the input with the current value).
    pub fn breakpoint_field(&self, log: bool) -> Option<String> {
        let (path, line) = self.cursor_file_line()?;
        let bp = self.breakpoints.get(&path)?.get(&line)?;
        if log { bp.log.clone() } else { bp.condition.clone() }
    }

    /// Changes the condition / log message of the current line's breakpoint (creates the breakpoint if
    /// missing; empty text = clear).
    pub fn set_breakpoint_field(&mut self, log: bool, text: &str) {
        let Some((path, line)) = self.cursor_file_line() else {
            return self.set_status("save the file first");
        };
        let bp = self.breakpoints.entry(path.clone()).or_default().entry(line).or_default();
        let v = (!text.trim().is_empty()).then(|| text.trim().to_string());
        if log {
            bp.log = v;
        } else {
            bp.condition = v;
        }
        bp.rejected = None;
        let what = match (log, text.trim().is_empty()) {
            (false, false) => format!("Breaks on line {} only when {}", line + 1, text.trim()),
            (false, true) => format!("Line {} breaks every time", line + 1),
            (true, false) => format!("Logs on line {} without stopping", line + 1),
            (true, true) => format!("Line {} stops again (no log message)", line + 1),
        };
        self.set_status(what);
        self.dap_send_breakpoints(&path);
    }

    fn dap_send_breakpoints(&mut self, path: &Path) {
        if let Some(d) = &mut self.dap
            && !matches!(d.state, State::Building | State::Exited(_))
        {
            let bps: Vec<Value> = self
                .breakpoints
                .get(path)
                .into_iter()
                .flatten()
                .map(|(l, b)| {
                    let mut v = json!({ "line": l + 1 });
                    if let Some(c) = &b.condition {
                        v["condition"] = json!(c);
                    }
                    if let Some(m) = &b.log {
                        v["logMessage"] = json!(m);
                    }
                    v
                })
                .collect();
            let seq = d.send("setBreakpoints", json!({ "source": { "path": path, "name": path.file_name().and_then(|n| n.to_str()) }, "breakpoints": bps }));
            d.bp_requests.insert(seq, path.to_path_buf());
        }
    }

    // ── Watches ──────────────────────────────────────────────────────────

    /// Adds a watch (unchanged if already there) — evaluated right away if stopped.
    pub fn add_watch(&mut self, expr: &str) {
        let expr = expr.trim().to_string();
        if expr.is_empty() {
            return;
        }
        if !self.watches.contains(&expr) {
            self.watches.push(expr.clone());
        }
        self.note(format!("Watching {expr}"));
        self.dap_evaluate_watches();
    }

    /// Removes a watch (all of them if empty).
    pub fn remove_watch(&mut self, expr: &str) {
        let expr = expr.trim();
        if expr.is_empty() {
            self.watches.clear();
        } else {
            self.watches.retain(|w| w != expr);
        }
        if let Some(d) = &mut self.dap {
            let keep: Vec<String> = self.watches.clone();
            d.watches.retain(|k, _| keep.contains(k));
        }
    }

    /// If stopped, evaluates the watches in the current frame.
    fn dap_evaluate_watches(&mut self) {
        let watches = self.watches.clone();
        let Some(d) = self.dap.as_mut().filter(|d| d.stopped()) else { return };
        let Some(frame) = d.frame else { return };
        for w in watches {
            let seq = d.send("evaluate", json!({ "expression": w, "frameId": frame, "context": "watch" }));
            d.watch_requests.insert(seq, w.clone());
            d.watches.entry(w).or_insert(Watch::Pending);
        }
    }

    // ── Start·control ────────────────────────────────────────────────────

    /// `F5` · `space G l` — continue if stopped; with no debug run, start for the current file's language.
    pub fn dap_launch(&mut self) {
        match self.dap.as_ref().map(|d| d.state.clone()) {
            Some(State::Stopped { .. }) => return self.dap_continue(),
            Some(State::Running | State::Starting | State::Building) => {
                return self.set_status("already debugging");
            }
            // Finished test debug → the same test again
            Some(State::Exited(_)) if self.dap_test.is_some() => {
                let t = self.dap_test.clone().expect("checked");
                return self.test_debug_target(t);
            }
            _ => {}
        }
        self.dap_test = None;
        self.dap_resume = Some(crate::offer::Resume::Debug);
        let doc = self.doc();
        let Some(path) = doc.path.clone() else { return self.set_error("debug: save the file first") };
        let lang = match (&doc.syntax, crate::syntax::detect(&path)) {
            (Some(s), _) => s.lang.name.clone(),
            // No grammar yet — download offer; debugging resumes once downloaded
            // (language features only once the grammar is in)
            (None, Some(spec)) if crate::syntax::Loader::global().load(spec).is_err() => {
                return self.offer_grammar_then(spec, crate::offer::Resume::Debug);
            }
            (None, Some(spec)) => spec.name.clone(),
            (None, None) => String::new(),
        };
        let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        self.dap_generation += 1;
        let generation = self.dap_generation;
        self.dap = Some(Dap::placeholder(State::Starting, name(&path)));
        // The pack read, the project's root found, the build's files looked for — on a worker
        self.events.jobs().spawn(move || {
            let debug = crate::debugpack::of(&lang);
            // A way in code finds its project as its language does (Java's: the topmost build file); a
            // debugger runs in the project's root by any language's markers
            let root = match debug.as_ref().map(|d| &d.with) {
                Ok(crate::debugpack::With::Way(_)) => crate::lsp::find_root_for(&lang, &path),
                _ => crate::lsp::find_root(&path),
            };
            let root = std::fs::canonicalize(&root).unwrap_or(root);
            let how = debug.and_then(|d| d.launch(&path, &root));
            move |ed: &mut Editor| ed.dap_launch_with(how, lang, path, root, generation)
        });
    }

    /// `F5`'s next step, as `lang`'s pack says: Java's own way, the debugger started, or a build first.
    fn dap_launch_with(
        &mut self,
        how: Result<crate::debugpack::Launch, String>,
        lang: String,
        path: PathBuf,
        root: PathBuf,
        generation: u64,
    ) {
        use crate::debugpack::Launch;
        if self.dap_generation != generation {
            return;
        }
        let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        match how {
            Err(e) => {
                self.dap = None;
                self.set_error(format!("debug: {e}"));
            }
            // Java: the adapter is java-debug inside jdtls — nothing to launch, ask jdtls step by step (java.rs)
            // (it puts up its own placeholder once it begins)
            Ok(Launch::Way) => {
                self.dap = None;
                self.java_debug_start(path, root, generation);
            }
            Ok(Launch::Ready(launch)) => self.dap_start(&lang, root, launch, name(&path), generation),
            Ok(Launch::Build(build, pending)) => {
                let cwd = root.clone();
                self.dap_build_and_start(&lang, name(&root), root, generation, move || {
                    let built = crate::debugpack::run_build(&build, &cwd)?;
                    Ok((pending.with(&built), name(&built)))
                });
            }
        }
    }

    /// Launches `lang`'s adapter in `cwd` (resolve·spawn·connect on a worker thread) and starts a debug run
    /// with `launch`. Meanwhile "starting" in the debug pane — callers may set `request`·`helper` on that
    /// placeholder; the debug run takes them over.
    pub(crate) fn dap_start(
        &mut self,
        lang: &str,
        cwd: PathBuf,
        launch: Value,
        program: String,
        generation: u64,
    ) {
        self.dap = Some(Dap::placeholder(State::Starting, program.clone()));
        let lang = lang.to_string();
        self.events.jobs().spawn(move || {
            let started = crate::debugpack::resolve(&lang, &cwd)
                .and_then(|found| Ok(spawn_adapter(found, &cwd)?))
                .map(|s| (s, launch, program));
            move |ed: &mut Editor| ed.dap_started(started, generation)
        });
    }

    /// Built first, then the adapter launched on the result — all on a worker thread, "building" in the
    /// debug pane meanwhile. `build` returns (launch arguments, program name).
    pub(crate) fn dap_build_and_start(
        &mut self,
        lang: &str,
        label: String,
        cwd: PathBuf,
        generation: u64,
        build: impl FnOnce() -> Result<(Value, String), String> + Send + 'static,
    ) {
        self.dap = Some(Dap::placeholder(State::Building, label));
        self.set_step("debug-build", "Building for debug…");
        let lang = lang.to_string();
        self.events.jobs().spawn(move || {
            // Adapter first — no point building if there's nothing to debug with
            let started = crate::debugpack::resolve(&lang, &cwd).and_then(|found| {
                let (launch, program) = build()?;
                Ok((spawn_adapter(found, &cwd)?, launch, program))
            });
            move |ed: &mut Editor| ed.dap_started(started, generation)
        });
    }

    /// The adapter is up (or couldn't start) — begins the debug run if it's still the current one.
    fn dap_started(
        &mut self,
        started: Result<(Started, Value, String), crate::debugpack::Failed>,
        generation: u64,
    ) {
        // (built, or not: the debug panel says how it goes from here)
        self.step_done("debug-build");
        if self.dap_generation != generation {
            if let Ok((s, ..)) = started {
                reap(s.child);
            }
            return;
        }
        match started {
            Ok((s, launch, program)) => {
                let d = s.debugger;
                self.dap_connect(&d.id, s.reader, s.writer, Some(s.child), launch, program, generation);
                if let Some(run) = &mut self.dap {
                    run.adapter_err = s.err;
                    run.quiet = d.quiet;
                }
                if let Some(slow) = d.slow_start.filter(|s| s.here()) {
                    self.dap_slow_start(slow, generation);
                }
            }
            Err(e) => {
                self.dap = None;
                // No debugger: offer it if its pack says how (then debugging starts again)
                if !e.offer.is_some_and(|tool| self.offer_install_debugger(&tool)) {
                    self.set_error(format!("debug: {}", e.said));
                }
            }
        }
    }

    /// What the debugger's pack says when it hasn't started in a while (macOS's lldb waiting on a permission
    /// prompt that may not be in sight).
    fn dap_slow_start(&mut self, slow: crate::debugpack::SlowStart, generation: u64) {
        self.events.jobs().spawn(move || {
            std::thread::sleep(slow.after);
            move |ed: &mut Editor| {
                if ed.dap_generation == generation
                    && ed.dap.as_ref().is_some_and(|d| d.state == State::Starting)
                {
                    ed.set_warning(slow.say);
                }
            }
        });
    }

    /// Starts a debug run with an adapter whose transport is ready
    /// (reader thread → initialize → launch on reply).
    /// `child` = the adapter we launched (None when attaching to one already running, like Java).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn dap_connect(
        &mut self,
        id: &str,
        reader: Box<dyn std::io::Read + Send>,
        writer: Box<dyn Write + Send>,
        child: Option<Child>,
        launch: Value,
        program: String,
        generation: u64,
    ) {
        let tx = self.events.shared_sender();
        crate::event::thread("tarae-dap-read", move || {
            let mut r = BufReader::new(reader);
            while let Some(msg) = crate::lsp::read_message(&mut r) {
                let apply = move |ed: &mut Editor| ed.on_dap(generation, msg);
                if tx.send(Event::Job(Box::new(apply))).is_err() {
                    return;
                }
            }
            // A moment for the stderr reader to catch the adapter's last words (the reason it quit)
            std::thread::sleep(Duration::from_millis(50));
            let _ = tx.send(Event::Job(Box::new(move |ed: &mut Editor| ed.on_dap_closed(generation))));
        });
        let mut d = Dap::placeholder(State::Starting, program);
        // Take over what this debug run's placeholder holds (attach: `before` helper·request) — dropping it
        // would stop the helper (port-forward) right before attaching
        if let Some(old) = self.dap.as_mut() {
            d.helper = old.helper.take();
            d.request = old.request;
        }
        d.writer = queued(writer);
        d.child = child;
        d.launch = launch;
        d.send(
            "initialize",
            json!({ "clientID": "tarae", "clientName": "tarae", "adapterID": id, "linesStartAt1": true,
                    "columnsStartAt1": true, "pathFormat": "path", "supportsVariableType": true }),
        );
        self.dap = Some(d);
    }

    pub fn dap_continue(&mut self) {
        self.dap_step("continue");
    }

    /// continue · next · stepIn · stepOut.
    pub fn dap_step(&mut self, command: &str) {
        let Some(d) = self.dap.as_mut().filter(|d| d.stopped()) else { return self.set_status("not paused") };
        let thread = d.thread.unwrap_or(1);
        d.send(command, json!({ "threadId": thread }));
        d.state = State::Running;
        d.vars.clear();
        // The frame is gone once it runs — watches wait for the next stop's stack
        d.frames.clear();
        d.frame = None;
        for v in d.watches.values_mut() {
            *v = Watch::Pending;
        }
    }

    pub fn dap_pause(&mut self) {
        if let Some(d) = self.dap.as_mut().filter(|d| d.state == State::Running) {
            let thread = d.thread.unwrap_or(1);
            d.send("pause", json!({ "threadId": thread }));
        }
    }

    pub fn dap_terminate(&mut self) {
        if let Some(d) = &mut self.dap {
            // Attached programs are only detached — don't kill someone else's (remote) program
            let terminate = d.request == "launch";
            d.send("disconnect", json!({ "terminateDebuggee": terminate }));
        }
        self.dap = None;
        self.dap_generation += 1;
        self.set_status("Debug run ended");
    }

    // ── Incoming ─────────────────────────────────────────────────────────

    fn on_dap_closed(&mut self, generation: u64) {
        if self.dap_generation != generation {
            return;
        }
        let Some(d) = &mut self.dap else { return };
        match d.state {
            // Quit before the debug run began (`No module named debugpy` etc.) — say why
            State::Starting => {
                let why = d.adapter_err.lock().unwrap().clone();
                self.dap = None;
                self.set_error(with_reason("debug: the debugger quit before starting", &why));
            }
            State::Exited(_) => {}
            _ => d.state = State::Exited(None),
        }
    }

    fn on_dap(&mut self, generation: u64, msg: Value) {
        if self.dap_generation != generation {
            return;
        }
        let Some(d) = self.dap.as_mut() else { return };
        // Detailed reason is in body.error.format (many adapters, e.g. dlv, only summarize in message)
        let error = || msg["body"]["error"]["format"].as_str().or_else(|| msg["message"].as_str());
        match msg["type"].as_str() {
            Some("response") => {
                let request_seq = msg["request_seq"].as_i64().unwrap_or(-1);
                let command = d.pending.remove(&request_seq).unwrap_or_default();
                if msg["success"] == false && command == "evaluate" {
                    // Watch doesn't resolve in this frame (no such name etc.) — show the reason on its line
                    if let Some(w) = d.watch_requests.remove(&request_seq) {
                        let m = error().unwrap_or("not available").lines().next().unwrap_or_default();
                        d.watches.insert(w, Watch::Error(m.to_string()));
                    }
                    return;
                }
                if msg["success"] == false {
                    if command == "launch" || command == "attach" || command == "initialize" {
                        // The reason is usually in the preceding stderr output (build errors etc.) — add it
                        // to the toast
                        let why: Vec<&str> = d
                            .output
                            .iter()
                            .filter(|(c, _)| c == "stderr")
                            .map(|(_, l)| l.as_str())
                            .filter(|l| !l.trim().is_empty())
                            .take(3)
                            .collect();
                        let m = with_reason(&format!("debug: {}", error().unwrap_or("request failed")), &why);
                        self.dap = None;
                        return self.set_error(m);
                    }
                    return;
                }
                let body = &msg["body"];
                match command.as_str() {
                    "initialize" => {
                        let (request, args) = (d.request, d.launch.clone());
                        d.send(request, args);
                    }
                    "stackTrace" => {
                        d.frames = body["stackFrames"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|f| Frame {
                                id: f["id"].as_i64().unwrap_or(0),
                                name: f["name"].as_str().unwrap_or_default().to_string(),
                                path: f["source"]["path"].as_str().map(PathBuf::from),
                                line: (f["line"].as_u64().unwrap_or(1) as usize).saturating_sub(1),
                            })
                            .collect();
                        // Go to the first frame with source (my code) and ask for variables · watches in
                        // that frame too
                        if let Some(f) =
                            d.frames.iter().find(|f| f.path.as_ref().is_some_and(|p| p.is_file())).cloned()
                        {
                            d.frame = Some(f.id);
                            d.send("scopes", json!({ "frameId": f.id }));
                            self.dap_show_frame(&f);
                            self.dap_evaluate_watches();
                        }
                    }
                    "scopes" => {
                        let scope = body["scopes"]
                            .as_array()
                            .and_then(|s| {
                                s.iter()
                                    .find(|x| x["name"].as_str().is_some_and(|n| n.starts_with("Local")))
                                    .or(s.first())
                            })
                            .and_then(|s| s["variablesReference"].as_i64());
                        if let Some(r) = scope {
                            d.send("variables", json!({ "variablesReference": r }));
                        }
                    }
                    "variables" => {
                        d.vars = body["variables"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .map(|v| Var {
                                name: v["name"].as_str().unwrap_or_default().to_string(),
                                value: v["value"].as_str().unwrap_or_default().replace('\n', " "),
                                ty: v["type"].as_str().unwrap_or_default().to_string(),
                                reference: v["variablesReference"].as_i64().unwrap_or(0),
                            })
                            .filter(|v| !v.name.starts_with("special ") && !v.name.starts_with("function "))
                            .collect();
                    }
                    "evaluate" => {
                        if let Some(w) = d.watch_requests.remove(&request_seq) {
                            let value = body["result"].as_str().unwrap_or_default().replace('\n', " ");
                            let ty = body["type"].as_str().unwrap_or_default().to_string();
                            d.watches.insert(w, Watch::Value { value, ty });
                        }
                    }
                    "setBreakpoints" => {
                        // Rejected breakpoints (bad condition etc.) get the reason on their line
                        let Some(path) = d.bp_requests.remove(&request_seq) else { return };
                        let got: Vec<&Value> = body["breakpoints"].as_array().into_iter().flatten().collect();
                        if let Some(set) = self.breakpoints.get_mut(&path) {
                            for ((_, bp), b) in set.iter_mut().zip(got) {
                                let msg = b["message"].as_str().filter(|m| !m.is_empty());
                                bp.rejected = (b["verified"] == false
                                    && (bp.condition.is_some() || bp.log.is_some()))
                                .then(|| msg.unwrap_or("not accepted by the debugger").to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
            Some("event") => {
                let body = &msg["body"];
                match msg["event"].as_str().unwrap_or_default() {
                    "initialized" => {
                        d.state = State::Running;
                        let files: Vec<PathBuf> = self.breakpoints.keys().cloned().collect();
                        for f in files {
                            self.dap_send_breakpoints(&f);
                        }
                        if let Some(d) = self.dap.as_mut() {
                            d.send("configurationDone", json!({}));
                        }
                        let program = self.dap.as_ref().map(|d| d.program.clone()).unwrap_or_default();
                        self.set_status(format!("Debugging {program}"));
                    }
                    "stopped" => {
                        let reason = body["reason"].as_str().unwrap_or("paused").to_string();
                        d.thread = body["threadId"].as_i64().or(d.thread);
                        d.state = State::Stopped { reason };
                        let thread = d.thread.unwrap_or(1);
                        d.send("stackTrace", json!({ "threadId": thread, "startFrame": 0, "levels": 30 }));
                    }
                    "continued" => d.state = State::Running,
                    "output" => {
                        let cat = body["category"].as_str().unwrap_or("console").to_string();
                        if cat != "telemetry" {
                            for line in body["output"].as_str().unwrap_or_default().lines() {
                                let first = d.output.is_empty();
                                if let Some(c) = d.quiet.category(&cat, line, first) {
                                    d.output.push((c.to_string(), line.to_string()));
                                }
                            }
                            let n = d.output.len();
                            if n > 2000 {
                                d.output.drain(..n - 2000);
                            }
                        }
                    }
                    "exited" => {
                        let code = body["exitCode"].as_i64();
                        d.state = State::Exited(code);
                        self.set_status(format!(
                            "Program exited with code {}",
                            code.map_or("?".into(), |c| c.to_string())
                        ));
                    }
                    "terminated" if !matches!(d.state, State::Exited(_)) => {
                        d.state = State::Exited(None);
                    }
                    _ => {}
                }
            }
            // A request from the adapter (runInTerminal etc.) — answer that it's unsupported
            Some("request") => {
                let seq = msg["seq"].as_i64().unwrap_or(0);
                let command = msg["command"].as_str().unwrap_or_default().to_string();
                d.seq += 1;
                let reply = json!({ "seq": d.seq, "type": "response", "request_seq": seq, "command": command, "success": false });
                d.write(reply);
            }
            _ => {}
        }
    }

    /// To the stopped frame: opens that file and goes to that line (around mid-screen).
    fn dap_show_frame(&mut self, f: &Frame) {
        let Some(path) = f.path.clone() else { return };
        if self.open(&path).is_err() {
            return;
        }
        let doc = self.doc_mut();
        let line = f.line.min(crate::movement::last_line(&doc.text));
        let pos = crate::movement::line_start(&doc.text, line);
        doc.set_selection(crate::selection::Selection::point(pos));
        let rows = self.viewport.0.max(1);
        let doc = self.doc_mut();
        if line < doc.top || line >= doc.top + rows {
            doc.set_top(line.saturating_sub(rows / 3));
        }
    }
}

impl Dap {
    pub(crate) fn placeholder(state: State, program: String) -> Dap {
        Dap {
            writer: Box::new(std::io::sink()),
            child: None,
            helper: None,
            request: "launch",
            seq: 0,
            pending: HashMap::new(),
            state,
            thread: None,
            frames: Vec::new(),
            vars: Vec::new(),
            frame: None,
            watches: HashMap::new(),
            bp_requests: HashMap::new(),
            watch_requests: HashMap::new(),
            output: Vec::new(),
            program,
            launch: Value::Null,
            adapter_err: Arc::default(),
            quiet: Default::default(),
        }
    }
}

/// Variable values to append at line ends: walking up from the stopped line, on the first line where each
/// variable name appears (as a word). (line → [(name, value)]). Only up to 80 lines above the stopped line.
pub fn inline_values(
    text: &ropey::Rope,
    stop_line: usize,
    vars: &[Var],
) -> BTreeMap<usize, Vec<(String, String)>> {
    let mut out: BTreeMap<usize, Vec<(String, String)>> = BTreeMap::new();
    let from = stop_line.saturating_sub(80);
    let lines: Vec<String> = (from..=stop_line.min(text.len_lines().saturating_sub(1)))
        .map(|l| text.line(l).to_string())
        .collect();
    let is_word = |c: char| c.is_alphanumeric() || c == '_';
    for v in vars.iter().filter(|v| !v.name.is_empty() && v.name.chars().all(is_word)) {
        let found = lines.iter().enumerate().rev().find(|(_, l)| {
            l.match_indices(&v.name).any(|(i, _)| {
                let before = l[..i].chars().next_back().is_none_or(|c| !is_word(c));
                let after = l[i + v.name.len()..].chars().next().is_none_or(|c| !is_word(c));
                before && after
            })
        });
        if let Some((k, _)) = found {
            out.entry(from + k).or_default().push((v.name.clone(), v.value.clone()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn breakpoints_toggle_per_line() {
        let dir = std::env::temp_dir().join(format!("tarae-bp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.py");
        std::fs::write(&path, "a = 1\nb = 2\n").unwrap();
        let mut ed = Editor::new(crate::config::Config::default());
        ed.open(&path).unwrap();
        ed.handle_key("j".parse().unwrap());
        ed.handle_key("F9".parse().unwrap());
        let key = ed.doc().path.clone().unwrap();
        assert_eq!(ed.breakpoints[&key].keys().copied().collect::<Vec<_>>(), [1]);
        ed.handle_key("F9".parse().unwrap());
        assert!(ed.breakpoints[&key].is_empty(), "pressing again turns it off");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn inline_values_find_nearest_word_above() {
        let text = ropey::Rope::from_str("let count = 1;\nlet total = count + 2;\nprintln!(\"{total}\");\n");
        let v =
            |n: &str, val: &str| Var { name: n.into(), value: val.into(), ty: String::new(), reference: 0 };
        let m = inline_values(&text, 2, &[v("count", "1"), v("total", "3"), v("tot", "?")]);
        assert_eq!(m.get(&1).map(|x| x.len()), Some(1), "count on the nearest line above (1)");
        assert_eq!(m[&1][0], ("count".to_string(), "1".to_string()));
        assert_eq!(m[&2][0].0, "total");
        assert!(!m.values().flatten().any(|(n, _)| n == "tot"), "word boundary");
        assert_eq!(value_scope("\"hi\""), "string");
        assert_eq!(value_scope("42"), "constant.numeric");
    }

    /// dlv's and lldb-dap's packs: dlv's terminal hint and lldb's disassembly dropped, dlv's first `Building`
    /// line dimmed as its own — the program's output kept.
    #[test]
    fn adapter_chatter_is_dimmed_or_dropped() {
        let q = |name: &str| crate::debugpack::debugger(name).unwrap().quiet;
        let dlv = q("dlv");
        assert_eq!(dlv.category("stdout", "Building /tmp/app", true), Some("console"));
        assert_eq!(
            dlv.category("stdout", "Building /tmp/app", false),
            Some("stdout"),
            "printed by the program"
        );
        assert_eq!(dlv.category("console", "Type 'dlv help' for list of commands.", false), None);
        let lldb = q("lldb-dap");
        assert_eq!(lldb.category("stdout", "Building /tmp/app", true), Some("stdout"), "dlv's, not lldb's");
        // Drop the disassembly lldb prints on attach (program output stays)
        assert_eq!(lldb.category("console", "dyld`_dyld_start:", false), None);
        assert_eq!(lldb.category("console", "->  0x1044d0bf0 <+0>:  mov    x0, sp", false), None);
        assert_eq!(
            lldb.category("console", "    0x1044d0bf4 <+4>:  and    sp, x0, #0xfffffff0", false),
            None
        );
        assert_eq!(lldb.category("console", "Process 42 stopped", false), Some("console"));
        assert_eq!(lldb.category("stdout", "0x10 <+4>: mine", false), Some("stdout"));
        assert_eq!(lldb.category("stderr", "boom", false), Some("stderr"));
        let none = crate::debugpack::Quiet::default();
        assert_eq!(none.category("console", "dyld`_dyld_start:", false), Some("console"), "no pack, no rule");
    }

    /// A writer that collects what was sent to the adapter.
    #[derive(Clone, Default)]
    struct Sent(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for Sent {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl Sent {
        fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
        }
        fn last_seq(&self) -> i64 {
            let t = self.text();
            let i = t.rfind("\"seq\":").unwrap() + 6;
            t[i..].chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap()
        }
    }

    fn debug_run(src: &str) -> (Editor, Sent, PathBuf) {
        let dir = std::env::temp_dir().join(format!("tarae-dap-{}-{}", std::process::id(), src.len()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.py");
        std::fs::write(&path, src).unwrap();
        let mut ed = Editor::new(crate::config::Config::default());
        ed.open(&path).unwrap();
        let sent = Sent::default();
        let mut d = Dap::placeholder(State::Running, "a.py".into());
        d.writer = Box::new(sent.clone());
        ed.dap = Some(d);
        let path = ed.doc().path.clone().unwrap();
        (ed, sent, path)
    }

    #[test]
    fn conditional_and_log_breakpoints_are_sent_and_rejections_kept() {
        let (mut ed, sent, path) = debug_run("n = 1\nfor i in range(9):\n    n += i\n");
        ed.handle_key("j".parse().unwrap());
        ed.handle_key("j".parse().unwrap());
        // A condition creates the breakpoint even if there isn't one
        ed.set_breakpoint_field(false, "i > 3");
        let bp = &ed.breakpoints[&path][&2];
        assert_eq!(bp.condition.as_deref(), Some("i > 3"));
        assert!(sent.text().contains(r#""breakpoints":[{"condition":"i > 3","line":3}]"#), "{}", sent.text());
        assert_eq!(ed.breakpoint_field(false).as_deref(), Some("i > 3"), "prefilled with the current value");
        // Logpoint
        ed.set_breakpoint_field(true, "n is {n}");
        assert!(sent.text().contains(r#""logMessage":"n is {n}""#));
        // If the adapter rejects it, keep the reason
        let seq = sent.last_seq();
        ed.on_dap(
            ed.dap_generation,
            json!({ "type": "response", "request_seq": seq, "command": "setBreakpoints", "success": true,
            "body": { "breakpoints": [{ "verified": false, "message": "invalid syntax" }] } }),
        );
        assert_eq!(ed.breakpoints[&path][&2].rejected.as_deref(), Some("invalid syntax"));
        // Empty text = clear the condition (breakpoint stays)
        ed.set_breakpoint_field(false, "");
        ed.set_breakpoint_field(true, " ");
        assert_eq!(ed.breakpoints[&path][&2], Bp::default());
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn watches_are_evaluated_in_the_stopped_frame() {
        let (mut ed, sent, path) = debug_run("a = 1\nb = 2\n");
        ed.add_watch("a + b");
        ed.add_watch("missing");
        ed.add_watch("a + b");
        assert_eq!(ed.watches, ["a + b", "missing"], "same expression only once");
        assert!(!sent.text().contains("evaluate"), "not evaluated while running");
        // Stop → call stack → evaluate watches in that frame
        let g = ed.dap_generation;
        ed.on_dap(
            g,
            json!({ "type": "event", "event": "stopped", "body": { "reason": "breakpoint", "threadId": 1 } }),
        );
        let st = sent.last_seq();
        ed.on_dap(g, json!({ "type": "response", "request_seq": st, "command": "stackTrace", "success": true,
            "body": { "stackFrames": [{ "id": 7, "name": "<module>", "line": 2, "source": { "path": path } }] } }));
        let t = sent.text();
        assert!(t.contains(r#""context":"watch","expression":"a + b","frameId":7"#), "{t}");
        let seq = sent.last_seq(); // missing (the last one sent)
        ed.on_dap(
            g,
            json!({ "type": "response", "request_seq": seq - 1, "command": "evaluate", "success": true,
            "body": { "result": "3", "type": "int" } }),
        );
        ed.on_dap(g, json!({ "type": "response", "request_seq": seq, "command": "evaluate", "success": false,
            "message": "evaluate failed", "body": { "error": { "format": "name 'missing' is not defined" } } }));
        let w = &ed.dap.as_ref().unwrap().watches;
        assert_eq!(w["a + b"], Watch::Value { value: "3".into(), ty: "int".into() });
        assert_eq!(w["missing"], Watch::Error("name 'missing' is not defined".into()));
        // Continuing clears the values until the next stop
        ed.dap_continue();
        assert_eq!(ed.dap.as_ref().unwrap().watches["a + b"], Watch::Pending);
        ed.remove_watch("missing");
        assert_eq!(ed.watches, ["a + b"]);
        // …and the frame — a watch added between the next stop and its stack isn't evaluated in frame 7
        let d = ed.dap.as_ref().unwrap();
        assert!(d.frame.is_none() && d.frames.is_empty());
        ed.on_dap(
            g,
            json!({ "type": "event", "event": "stopped", "body": { "reason": "step", "threadId": 1 } }),
        );
        let evaluations = sent.text().matches("\"evaluate\"").count();
        ed.add_watch("b");
        assert_eq!(sent.text().matches("\"evaluate\"").count(), evaluations, "no stale frame");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    fn alive(pid: u32) -> bool {
        // SAFETY: signal 0 only checks that the process exists.
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    /// Processes events until the condition holds.
    fn settle_until(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
        let end = Instant::now() + crate::test_results::PATIENCE;
        while !done(ed) {
            let left = end.saturating_duration_since(Instant::now());
            let ev = ed.events.recv_timeout(left).expect("event before timeout");
            ed.handle_event(ev);
        }
    }

    #[test]
    fn the_debug_run_takes_over_the_attach_helper_and_request() {
        let mut ed = Editor::new(crate::config::Config::default());
        // attach.rs puts the `before` helper (port-forward) on the placeholder while java-debug prepares
        let mut p = Dap::placeholder(State::Starting, "api".into());
        p.helper = Some(setsid(Command::new("sleep").arg("30")).spawn().unwrap());
        p.request = "attach";
        ed.dap = Some(p);
        let g = ed.dap_generation;
        let sent = Sent::default();
        ed.dap_connect(
            "java",
            Box::new(std::io::empty()),
            Box::new(sent.clone()),
            None,
            json!({}),
            "api".into(),
            g,
        );
        let d = ed.dap.as_mut().unwrap();
        assert_eq!(d.request, "attach");
        let h = d.helper.as_mut().expect("the helper moved into the debug run");
        assert!(matches!(h.try_wait(), Ok(None)), "port-forward still running");
        let pid = h.id();
        ed.dap = None;
        assert!(!alive(pid), "stopped with the debug run");
    }

    #[test]
    fn ending_a_debug_run_stops_the_adapter_and_its_debuggee() {
        // Adapter that ignores disconnect and whose debuggee (same process group) keeps running
        let mut c = Command::new("sh");
        c.args(["-c", "sleep 30 & echo $!; wait"]).stdout(Stdio::piped());
        let mut child = setsid(&mut c).spawn().unwrap();
        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let debuggee: u32 = line.trim().parse().unwrap();
        let adapter = child.id();
        let mut d = Dap::placeholder(State::Running, "app".into());
        d.child = Some(child);
        drop(d);
        let end = Instant::now() + crate::test_results::PATIENCE;
        while (alive(adapter) || alive(debuggee)) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(!alive(debuggee), "debuggee stopped with the group");
        assert!(!alive(adapter), "adapter reaped (no zombie)");
    }

    #[test]
    fn adapter_starts_off_the_main_thread_with_the_venv_python_and_reports_why_it_died() {
        let root = std::env::temp_dir().join(format!("tarae-dap-venv-{}", std::process::id()));
        std::fs::create_dir_all(root.join(".venv/bin")).unwrap();
        let py = root.join(".venv/bin/python");
        std::fs::write(&py, "#!/bin/sh\necho \"$0 $*\" >&2\necho 'No module named debugpy' >&2\nexit 1\n")
            .unwrap();
        std::fs::set_permissions(&py, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let mut ed = Editor::new(crate::config::Config::default());
        ed.dap_generation += 1;
        let g = ed.dap_generation;
        ed.dap_start("python", root.clone(), json!({ "program": "a.py", "cwd": root }), "a.py".into(), g);
        assert_eq!(ed.dap.as_ref().map(|d| d.state.clone()), Some(State::Starting), "returns at once");
        settle_until(&mut ed, |ed| ed.dap.is_none());
        let msg = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert!(msg.contains("No module named debugpy"), "{msg}");
        assert!(msg.contains("-m debugpy.adapter"), "the venv's python runs the adapter: {msg}");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn build_errors_show_rustc_diagnostics() {
        if std::env::var_os("CARGO_TARGET_DIR").is_some() {
            return; // would share (and wait on) the outer build's lock
        }
        let root = std::env::temp_dir().join(format!("tarae-dap-cargo-{}", std::process::id()));
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"x\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {\n    let n: i32 = \"one\";\n}\n").unwrap();
        crate::testing::cargo_builds_inside(&root);
        let root = root.canonicalize().unwrap();
        // The Rust pack's F5: its build, then its launch with what was built
        let rust = crate::debugpack::of("rust").unwrap();
        let file = root.join("src/main.rs");
        let crate::debugpack::Launch::Build(build, pending) = rust.launch(&file, &root).unwrap() else {
            panic!("rust builds first")
        };
        let e = crate::debugpack::run_build(&build, &root).unwrap_err();
        assert!(e.starts_with("build failed\n") && e.contains("mismatched types"), "{e}");
        assert!(e.contains("src/main.rs:2"), "where, as rustc renders it: {e}");
        std::fs::write(&file, "fn main() {\n    let n: i32 = 1;\n    println!(\"{n}\");\n}\n").unwrap();
        let exe = crate::debugpack::run_build(&build, &root).unwrap();
        assert!(exe.is_file() && exe.file_name().unwrap() == "x", "{exe:?}");
        let launch = pending.with(&exe);
        assert_eq!(launch, json!({ "program": exe, "args": [], "cwd": root, "stopOnEntry": false }));
        std::fs::remove_dir_all(&root).ok();
    }
}
