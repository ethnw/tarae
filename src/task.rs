//! Build tasks — `cargo check`, `go vet ./...`, `npm run lint`, `make`, or your own (`[task.<name>]`) — run
//! in the background with their output in the bottom panel, and every `file:line:col: message` they print
//! goes into the list (quickfix.rs) as it streams, for `]q`.
//!
//! Tasks are found from the project's files, next to the ones in config. A run is a new session (stopping
//! it stops everything it started); its lines come to the main loop as events, one generation per run so a
//! stopped run's late lines are dropped.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use crate::editor::Editor;
use crate::quickfix::Item;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub name: String,
    /// Run by `sh -c`.
    pub cmd: String,
    pub cwd: PathBuf,
}

/// Tasks the project's files suggest (in `root`), the likeliest first — from the language packs'
/// `[tasks]` (`packs.rs`).
pub fn detect(root: &Path) -> Vec<Task> {
    crate::packs::tasks(root)
        .into_iter()
        .map(|(name, cmd)| Task { name, cmd, cwd: root.to_path_buf() })
        .collect()
}

/// Every task: the configured ones (`[task.<name>]`), then what the project suggests (it reads the
/// project's files — on a worker).
fn all_tasks(configured: Vec<Task>) -> Vec<Task> {
    let root = std::env::current_dir().unwrap_or_default();
    let mut out = configured;
    for t in detect(&root) {
        if !out.iter().any(|x| x.name == t.name) {
            out.push(t);
        }
    }
    out
}

/// Reads places out of a task's output, a line at a time: `path:line[:col]: [error|warning] message` (rustc
/// `--message-format=short`, gcc/clang, go, eslint, ruff, mypy), `path(line,col): error …` (tsc), rustc's
/// `error…: message` + ` --> path:line:col`, and Python's `File "path", line N`. Only paths that exist
/// count — a URL or a timestamp isn't a place.
#[derive(Default)]
pub struct Matcher {
    /// rustc's long format: the message line waiting for its ` --> ` line.
    pending: Option<(u8, String)>,
}

impl Matcher {
    pub fn line(&mut self, cwd: &Path, line: &str) -> Option<Item> {
        let t = line.trim_end();
        let trimmed = t.trim_start();
        // rustc long format
        if let Some(rest) = trimmed.strip_prefix("--> ") {
            let (sev, msg) = self.pending.take()?;
            let (path, l, c, _) = split_place(rest)?;
            return item(cwd, path, l, c, sev, msg);
        }
        for (word, sev) in [("error", 1), ("warning", 2)] {
            if let Some(rest) = trimmed.strip_prefix(word)
                && (rest.starts_with(':') || rest.starts_with('['))
            {
                let msg = rest.split_once(": ").map_or("", |(_, m)| m);
                self.pending = Some((sev, msg.to_string()));
                return None;
            }
        }
        // Python traceback
        if let Some(rest) = trimmed.strip_prefix("File \"")
            && let Some((path, rest)) = rest.split_once("\", line ")
        {
            let n: usize = rest.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok()?;
            return item(cwd, path, n, 1, 1, "traceback".into());
        }
        // tsc: path(line,col): error TS1234: message — the last `(` before `):` (a path may hold parentheses:
        // Next.js's `app/(auth)/page.tsx`); if that isn't a place, the other forms get their turn
        if let Some(close) = trimmed.find("):")
            && let Some(open) = trimmed[..close].rfind('(')
            && let Some((l, c)) = trimmed[open + 1..close].split_once(',')
            && let (Ok(l), Ok(c)) = (l.parse(), c.parse())
        {
            let (sev, msg) = severity(trimmed[close + 2..].trim());
            if let Some(found) = item(cwd, &trimmed[..open], l, c, sev, msg) {
                return Some(found);
            }
        }
        let (path, l, c, rest) = split_place(trimmed)?;
        let (sev, msg) = severity(rest.trim());
        item(cwd, path, l, c, sev, msg)
    }
}

/// `path:line[:col]` and what follows it (after `: ` or `:`).
fn split_place(s: &str) -> Option<(&str, usize, usize, &str)> {
    // The path may hold colons (Windows drives aside, rare) — take the first `:<digits>`
    let mut from = 0;
    while let Some(i) = s[from..].find(':').map(|i| i + from) {
        let rest = &s[i + 1..];
        let digits = rest.chars().take_while(char::is_ascii_digit).count();
        if digits > 0 && i > 0 {
            let line: usize = rest[..digits].parse().ok()?;
            let after = &rest[digits..];
            let (col, after) = match after.strip_prefix(':') {
                Some(a) => {
                    let d = a.chars().take_while(char::is_ascii_digit).count();
                    match a[..d].parse() {
                        Ok(c) => (c, a[d..].strip_prefix(':').unwrap_or(&a[d..])),
                        Err(_) => (1, a),
                    }
                }
                None => (1, after),
            };
            return Some((&s[..i], line, col, after));
        }
        from = i + 1;
    }
    None
}

/// `error[E0425]: message` / `warning: message` / `error TS2304: message` / plain message (an error).
fn severity(rest: &str) -> (u8, String) {
    for (word, sev) in [("error", 1u8), ("warning", 2), ("note", 3), ("info", 3), ("help", 3)] {
        if let Some(after) = rest.strip_prefix(word) {
            let msg = after.split_once(':').map_or(after, |(_, m)| m);
            return (sev, msg.trim().to_string());
        }
    }
    (1, rest.to_string())
}

fn item(cwd: &Path, path: &str, line: usize, col: usize, severity: u8, message: String) -> Option<Item> {
    let path = path.trim().trim_start_matches("./");
    if path.is_empty() || path.contains("://") || line == 0 {
        return None;
    }
    // Relative to the task's folder — or a folder above it (cargo prints paths from the workspace's root)
    let full = cwd.ancestors().take(6).map(|d| d.join(path)).find(|p| p.is_file())?;
    let path = std::fs::canonicalize(&full).unwrap_or(full);
    Some(Item { path, line: line - 1, col: col.saturating_sub(1), message, severity, mark: None })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Running,
    Passed,
    Failed(Option<i32>),
}

pub struct Run {
    pub task: Task,
    pub state: State,
    /// (is stderr, line).
    pub output: Vec<(bool, String)>,
    pub started: Instant,
    pub took: Option<Duration>,
    pid: Option<u32>,
    generation: u64,
    /// (errors, warnings) put in the list.
    pub found: (usize, usize),
    /// An install's (`install.rs`): what goes on once it ends.
    after: Option<After>,
    /// An install run (not one `space m m` runs again).
    pub install: bool,
    /// Claude's `run` waiting for this run's end (`tools.rs`).
    pub chat: Option<crate::tools::Call>,
    /// Claude's command (not one `space m m` runs again).
    pub by_claude: bool,
    /// How Claude's command ran — in the sandbox or not (`sandbox.rs`); None for the user's own.
    pub guard: Option<crate::sandbox::Guard>,
    /// When it last printed (a sandboxed run quiet after a denial may be waiting on it — the panel says so).
    pub last_output: Instant,
}

/// An install run's card, tool, and what goes on after it.
#[derive(Clone, Debug)]
pub struct After {
    pub offer: u64,
    pub tool: String,
    pub then: crate::install::Then,
}

#[cfg(test)]
impl Run {
    /// A finished run with no process — for tests of what's made of one.
    pub fn for_test(task: Task) -> Run {
        Run {
            task,
            state: State::Passed,
            output: Vec::new(),
            started: Instant::now(),
            took: None,
            pid: None,
            generation: 0,
            found: (0, 0),
            after: None,
            install: false,
            chat: None,
            by_claude: false,
            guard: None,
            last_output: Instant::now(),
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if let Some(pid) = self.pid.take()
            && self.state == State::Running
        {
            crate::dap::term_group(pid);
        }
    }
}

impl Editor {
    /// `space m m` — run the last task again (or pick one, the first time).
    pub fn task_again(&mut self) {
        match self.task_last.clone() {
            Some(t) => self.task_start(t),
            None => self.task_picker(),
        }
    }

    /// `space m p` — pick a task (the last one first). The project is looked at on a worker.
    pub fn task_picker(&mut self) {
        let (configured, last) = (self.config.tasks.clone(), self.task_last.as_ref().map(|t| t.name.clone()));
        let fill = move || {
            let mut tasks = all_tasks(configured);
            if let Some(i) = last.and_then(|l| tasks.iter().position(|t| t.name == l)) {
                let t = tasks.remove(i);
                tasks.insert(0, t);
            }
            if tasks.is_empty() {
                return Err(crate::picker::FillError::Nothing(
                    "no tasks here — add one: [task.<name>] cmd = \"…\" in the config".to_string(),
                ));
            }
            Ok(tasks
                .iter()
                .map(|t| crate::picker::Item {
                    label: t.name.clone(),
                    action: crate::picker::Action::Typed(format!("task {}", t.name)),
                    hint: t.cmd.clone(),
                    glyph: None,
                })
                .collect())
        };
        let picker = crate::picker::Picker::new("tasks", Vec::new(), false).without_preview();
        self.open_picker(picker, Some(Box::new(fill)));
    }

    /// `:task <name>` — found on a worker, then started.
    pub fn task_named(&mut self, name: &str) -> Result<(), String> {
        let (configured, name) = (self.config.tasks.clone(), name.to_string());
        self.events.jobs().spawn(move || {
            let found = all_tasks(configured).into_iter().find(|t| t.name == name);
            move |ed: &mut Editor| match found {
                Some(t) => ed.task_start(t),
                None => ed.set_error(format!("no task \"{name}\"")),
            }
        });
        Ok(())
    }

    /// `space m c` — stop the task and close its panel.
    pub fn task_close(&mut self) {
        // An install stopped half-way: its card says so (and `y` tries again)
        if let Some(a) =
            self.task_run.as_mut().filter(|r| r.state == State::Running).and_then(|r| r.after.take())
        {
            self.offer_finished(a.offer, Err("stopped before it finished".into()));
        }
        crate::tools::run_dropped(self, "the user stopped it");
        crate::restart::run_dropped(self);
        self.task_run = None;
    }

    pub fn task_start(&mut self, task: Task) {
        // Not over an install: stopping it half-way can leave a tool broken
        if self.task_run.as_ref().is_some_and(|r| r.install && r.state == State::Running) {
            return self.set_status("an install is running — space m c stops it");
        }
        self.auto_save(" · before the task");
        self.flush_saves();
        self.task_last = Some(task.clone());
        self.task_spawn(task, None, None);
    }

    /// A command Claude runs (`tools.rs`): like a task, but not the one `space m m` runs again — the call
    /// waits for its end; in the sandbox, unless `guard` says otherwise. False: it didn't start (an install
    /// is running, or it couldn't spawn).
    pub(crate) fn task_start_for(
        &mut self,
        task: Task,
        call: crate::tools::Call,
        guard: Option<crate::sandbox::Guard>,
    ) -> bool {
        if self.task_run.as_ref().is_some_and(|r| r.state == State::Running) {
            return false;
        }
        self.auto_save(" · before the command");
        self.flush_saves();
        let before = self.task_generation;
        let policy = match &guard {
            Some(crate::sandbox::Guard::In(p)) => Some(p),
            _ => None,
        };
        self.task_spawn(task, None, policy);
        match self.task_run.as_mut().filter(|_| self.task_generation != before) {
            Some(r) => {
                r.chat = Some(call);
                r.by_claude = true;
                r.guard = guard;
                true
            }
            None => false,
        }
    }

    /// `:restart`'s build (`restart.rs`): not the task `space m m` runs again — the restart waits for its
    /// end. Its generation; None if it didn't start (an install is running, or it couldn't spawn).
    pub(crate) fn task_start_restart(&mut self, task: Task) -> Option<u64> {
        if self.task_run.as_ref().is_some_and(|r| r.install && r.state == State::Running) {
            return None;
        }
        let before = self.task_generation;
        self.task_spawn(task, None, None);
        (self.task_generation != before).then_some(self.task_generation)
    }

    /// An install (`install.rs`) in the task panel: not the task `space m m` runs again, and its output's
    /// places stay out of the list.
    pub(crate) fn task_start_install(&mut self, task: Task, after: After) {
        self.task_spawn(task, Some(after), None);
    }

    fn task_spawn(&mut self, task: Task, after: Option<After>, sandbox: Option<&crate::sandbox::Policy>) {
        let install = after.is_some();
        let watched = sandbox.is_some_and(|p| p.watch.is_some());
        let mut cmd = crate::sandbox::command(sandbox, "sh", &["-c".into(), task.cmd.clone()]);
        cmd.current_dir(&task.cwd)
            .env("NO_COLOR", "1")
            .env("CARGO_TERM_COLOR", "never")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = match crate::dap::new_session(&mut cmd).spawn() {
            Ok(c) => c,
            Err(e) => {
                if let Some(a) = after {
                    return self.offer_finished(a.offer, Err(format!("{}: {e}", task.name)));
                }
                return self.set_error(format!("{}: {e}", task.name));
            }
        };
        // A finished test run gives the panel over
        if self.test_run.as_ref().is_some_and(|r| r.state != crate::testing::RunState::Running) {
            self.test_run = None;
        }
        crate::tools::run_dropped(self, "another task took its place");
        crate::restart::run_dropped(self);
        self.task_generation += 1;
        let generation = self.task_generation;
        let pid = child.id();
        let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
        let tx = self.events.sender();
        // Each stream reads its places on its own thread (a stat per `path:line` — never the main loop's)
        let cwd = task.cwd.clone();
        let send_lines = move |tx: &std::sync::mpsc::Sender<crate::event::Event>,
                               matcher: &mut Matcher,
                               err: bool,
                               lines: Vec<String>| {
            let lines: Vec<(String, Option<Item>)> = lines
                .iter()
                .map(|l| {
                    let line = crate::testing::strip_ansi(l);
                    let found = matcher.line(&cwd, &line);
                    (line, found)
                })
                .collect();
            let apply = move |ed: &mut Editor| {
                let Some(r) = ed.task_run.as_mut().filter(|r| r.generation == generation) else { return };
                let mut places = Vec::new();
                r.last_output = Instant::now();
                for (line, found) in lines {
                    r.output.push((err, line));
                    places.extend(found.filter(|_| !install));
                }
                let n = r.output.len();
                if n > 5000 {
                    r.output.drain(..n - 5000);
                }
                for it in places {
                    let severity = it.severity;
                    // Counted once it's in the list (the same place twice isn't two)
                    if ed.qlist_push(it)
                        && let Some(r) = ed.task_run.as_mut()
                    {
                        match severity {
                            1 => r.found.0 += 1,
                            2 => r.found.1 += 1,
                            _ => {}
                        }
                    }
                }
            };
            tx.send(crate::event::Event::Job(Box::new(apply))).is_ok()
        };
        std::thread::spawn(move || {
            let err_reader = stderr.map(|e| {
                let tx = tx.clone();
                let send_lines = send_lines.clone();
                std::thread::spawn(move || {
                    let mut matcher = Matcher::default();
                    read_batches(e, |lines| send_lines(&tx, &mut matcher, true, lines));
                })
            });
            if let Some(o) = stdout {
                let mut matcher = Matcher::default();
                read_batches(o, |lines| send_lines(&tx, &mut matcher, false, lines));
            }
            if let Some(h) = err_reader {
                let _ = h.join();
            }
            let status = child.wait().ok();
            // Its last denials reach the watch before its end is told (here, not on the main loop)
            if watched {
                std::thread::sleep(crate::sandbox::WATCH_LAST);
            }
            let done = move |ed: &mut Editor| {
                let Some(r) = ed.task_run.as_mut().filter(|r| r.generation == generation) else { return };
                if let Some(w) = r.guard.as_ref().and_then(|g| g.watch()) {
                    w.finish();
                }
                r.took = Some(r.started.elapsed());
                r.pid = None;
                r.state = match status.and_then(|s| s.code()) {
                    Some(0) => State::Passed,
                    code => State::Failed(code),
                };
                let (passed, errors) = (r.state == State::Passed, r.found.0);
                if crate::restart::built(ed, generation, passed, errors) {
                    return;
                }
                let Some(r) = ed.task_run.as_mut() else { return };
                if let Some(after) = r.after.take() {
                    let passed = r.state == State::Passed;
                    return ed.install_done(after, passed);
                }
                if r.chat.is_some() {
                    return crate::tools::run_done(ed);
                }
                let (name, state, (errors, warnings)) = (r.task.name.clone(), r.state.clone(), r.found);
                let found = errors + warnings;
                match (state, found) {
                    (State::Passed, 0) => ed.set_success(format!("{name} — all good")),
                    (_, 0) => ed.set_error(format!("{name} failed — nothing to point at; the panel has why")),
                    _ => {
                        let n = |k: usize, w: &str| if k == 0 { String::new() } else { format!("{k} {w} ") };
                        let what = format!("{}{}", n(errors, "errors"), n(warnings, "warnings"));
                        ed.set_warning(format!("{name}: {what}— ]q goes to the first"));
                    }
                }
            };
            let _ = tx.send(crate::event::Event::Job(Box::new(done)));
        });
        // The list is this run's from now on
        if !install {
            self.qlist_set(task.name.clone(), Vec::new());
        }
        self.task_run = Some(Run {
            task,
            state: State::Running,
            output: Vec::new(),
            started: Instant::now(),
            took: None,
            pid: Some(pid),
            generation,
            found: (0, 0),
            after,
            install,
            chat: None,
            by_claude: false,
            guard: None,
            last_output: Instant::now(),
        });
    }
}

/// The lines of a program's output, a batch at a time to `send` (false: stop): what has come by the time
/// the stream goes quiet, up to a thousand. A chatty program is then a few events a frame, not one a line —
/// each event runs the editor's hooks. Bytes that aren't UTF-8 read as `�` (stopping there would leave the
/// program blocked on a full pipe).
pub fn read_batches(
    stream: impl std::io::Read + std::os::fd::AsRawFd,
    mut send: impl FnMut(Vec<String>) -> bool,
) {
    let fd = stream.as_raw_fd();
    let mut reader = BufReader::new(stream);
    let (mut batch, mut raw) = (Vec::new(), Vec::new());
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        // Without its line end, as `lines()` gives them
        let end = raw.strip_suffix(b"\n").map_or(&raw[..], |l| l.strip_suffix(b"\r").unwrap_or(l));
        batch.push(String::from_utf8_lossy(end).into_owned());
        let quiet = reader.buffer().is_empty() && !crate::terminal::ready(fd);
        if (quiet || batch.len() >= 1000) && !send(std::mem::take(&mut batch)) {
            return;
        }
    }
    if !batch.is_empty() {
        send(batch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Every line arrives, in batches, without its line end — CR LF, bytes that aren't UTF-8, a last line
    /// without a newline — and a thousand lines are a handful of batches, not a thousand.
    #[test]
    fn output_comes_in_batches_of_whole_lines() {
        let mut child = Command::new("sh")
            .args(["-c", r"printf 'a\r\nb\n\377 c\n'; seq 1 3000; printf last"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut batches: Vec<Vec<String>> = Vec::new();
        read_batches(child.stdout.take().unwrap(), |b| {
            batches.push(b);
            true
        });
        child.wait().unwrap();
        let lines: Vec<String> = batches.concat();
        assert_eq!(lines[..3], ["a", "b", "\u{fffd} c"]);
        assert_eq!(lines.len(), 3 + 3000 + 1);
        assert_eq!(lines.last().unwrap(), "last");
        assert!(batches.len() < 100, "{} batches", batches.len());
    }

    fn dir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("tarae-task-{}", std::process::id()));
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(d.join("a.py"), "x\n").unwrap();
        std::fs::canonicalize(d).unwrap()
    }

    #[test]
    fn places_from_common_formats() {
        let d = dir();
        let mut m = Matcher::default();
        let mut one = |l: &str| {
            m.line(&d, l).map(|i| {
                (
                    i.path.file_name().unwrap().to_string_lossy().into_owned(),
                    i.line,
                    i.col,
                    i.severity,
                    i.message,
                )
            })
        };
        let main = |l, c, s, msg: &str| Some(("main.rs".to_string(), l, c, s, msg.to_string()));
        assert_eq!(
            one("src/main.rs:3:5: error[E0425]: cannot find value `x`"),
            main(2, 4, 1, "cannot find value `x`")
        );
        assert_eq!(one("src/main.rs:7:1: warning: unused import"), main(6, 0, 2, "unused import"));
        assert_eq!(
            one("./src/main.rs:2:9: undefined: x"),
            main(1, 8, 1, "undefined: x"),
            "go: no word — an error"
        );
        assert_eq!(
            one("src/main.rs(4,2): error TS2304: Cannot find name 'y'."),
            main(3, 1, 1, "Cannot find name 'y'.")
        );
        assert_eq!(
            one("a.py:1:1: F401 [*] `os` imported but unused").map(|x| (x.1, x.4)),
            Some((0, "F401 [*] `os` imported but unused".to_string()))
        );
        assert_eq!(
            one("a.py:1: error: Name \"x\" is not defined").map(|x| (x.1, x.2, x.3)),
            Some((0, 0, 1)),
            "mypy"
        );
        // rustc's long format: the message, then where
        assert_eq!(one("error[E0308]: mismatched types"), None);
        assert_eq!(one("  --> src/main.rs:5:10"), main(4, 9, 1, "mismatched types"));
        assert_eq!(one("  File \"a.py\", line 1, in <module>").map(|x| x.1), Some(0), "traceback");
        assert_eq!(one("see https://example.com:8080/x"), None, "not a place");
        assert_eq!(one("nothing/here.rs:3:1: error: gone"), None, "a file that doesn't exist");
        assert_eq!(one("   Compiling tarae v0.1.0"), None);
        // A path with parentheses (Next.js route groups), and one relative to a folder above (a workspace)
        std::fs::create_dir_all(d.join("app/(auth)")).unwrap();
        std::fs::write(d.join("app/(auth)/page.tsx"), "x\n").unwrap();
        let got = m.line(&d, "app/(auth)/page.tsx(4,2): error TS2304: Cannot find name 'y'.");
        assert_eq!(got.map(|i| (i.path.ends_with("app/(auth)/page.tsx"), i.line)), Some((true, 3)));
        std::fs::create_dir_all(d.join("member")).unwrap();
        let got = m.line(&d.join("member"), "src/main.rs:1:1: error: from the workspace root");
        assert_eq!(got.map(|i| i.path.ends_with("src/main.rs")), Some(true));
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn tasks_from_project_files() {
        let d = std::env::temp_dir().join(format!("tarae-task-detect-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(d.join("package.json"), r#"{"scripts":{"lint":"eslint .","build":"vite build"}}"#)
            .unwrap();
        let names: Vec<String> = detect(&d).into_iter().map(|t| t.name).collect();
        assert_eq!(names[0], "cargo check", "the likeliest first");
        assert!(names.contains(&"npm run lint".to_string()) && names.contains(&"npm run build".to_string()));
        std::fs::remove_dir_all(&d).ok();
    }
}
