//! Build tasks — `cargo check`, `go vet ./...`, `npm run lint`, `make`, or your own (`[task.<name>]`) — run
//! in the background with their output in the bottom panel, and every `file:line:col: message` they print
//! goes into the list (quickfix.rs) as it streams, for `]q`.
//!
//! Tasks are found from the project's files, next to the ones in config. A run is a process group of its own (`setsid` — stopping
//! it stops everything it started); its lines come to the main loop as events, one generation per run so a
//! stopped run's late lines are dropped.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::editor::Editor;
use crate::quickfix::Item;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub name: String,
    /// Run by `sh -c`.
    pub cmd: String,
    /// Where it runs: a relative folder is from the editor's root (empty = the root), resolved as it starts.
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
fn all_tasks(configured: Vec<Task>, root: &Path) -> Vec<Task> {
    let mut out = configured;
    for t in detect(root) {
        if !out.iter().any(|x| x.name == t.name) {
            out.push(t);
        }
    }
    out
}

/// A run whose output is read, not shown on a terminal (a task, tests): colours off — `NO_COLOR`, and what the
/// packs say turns a tool's off (`color-off`, `CARGO_TERM_COLOR=never`).
pub(crate) fn color_off(cmd: &mut std::process::Command) {
    cmd.env("NO_COLOR", "1");
    for (k, v) in &crate::languages::get().color_off {
        cmd.env(k, v);
    }
}

/// A pack's `[[places]]` rule: a form of place one tool's output has beyond the one every tool shares
/// (`path:line[:col]: [severity] message`, read by the core). Lines are matched trimmed.
///
/// - `match`: a regular expression; its groups `path`, `line`, `col` — or `place`, a `path:line[:col]` read
///   as the shared form is — and `severity`, `message`. With no `severity` group the message's first word
///   says it, as in the shared form (`error TS2304: …`); `message = "…"`: one of its own.
/// - `then`: the place comes on a later line, matching this (rustc's `error[E0308]: …` + ` --> src/a.rs:3:5`):
///   the line is a message waiting for it, the `then` line's groups its place.
///
/// A rule whose place isn't a file lets the next rule, then the shared form, try the line.
#[derive(Clone, Debug)]
pub struct PlaceRule {
    re: regex::Regex,
    then: Option<regex::Regex>,
    message: Option<String>,
}

impl PlaceRule {
    /// One `[[places]]` entry.
    pub(crate) fn read(t: &toml::Table) -> Result<PlaceRule, String> {
        crate::packs::known(t, &["match", "then", "message"], "[[places]]")?;
        let re = |k: &str| -> Result<Option<regex::Regex>, String> {
            let Some(p) = crate::packs::string(t, k)? else { return Ok(None) };
            let re = regex::Regex::new(&p).map_err(|e| format!("`{k}`: {e}"))?;
            let groups = ["path", "line", "col", "place", "severity", "message"];
            match re.capture_names().flatten().find(|g| !groups.contains(g)) {
                Some(g) => Err(format!("`{k}`: no group `{g}` (there are: {})", groups.join(", "))),
                None => Ok(Some(re)),
            }
        };
        let rule = PlaceRule {
            re: re("match")?.ok_or("an entry has no `match`")?,
            then: re("then")?,
            message: None,
        };
        let placed = |r: &regex::Regex| {
            let g: Vec<&str> = r.capture_names().flatten().collect();
            g.contains(&"place") || (g.contains(&"path") && g.contains(&"line"))
        };
        if !placed(rule.then.as_ref().unwrap_or(&rule.re)) {
            let k = if rule.then.is_some() { "then" } else { "match" };
            return Err(format!("`{k}` has no place (`place`, or `path` and `line`)"));
        }
        Ok(PlaceRule { message: crate::packs::string(t, "message")?, ..rule })
    }

    /// How the language table's snapshot says it.
    #[cfg(test)]
    pub(crate) fn summary(&self) -> String {
        let mut s = self.re.as_str().to_string();
        if let Some(t) = &self.then {
            s += &format!(" then {}", t.as_str());
        }
        if let Some(m) = &self.message {
            s += &format!(" says {m:?}");
        }
        s
    }

    /// What this rule makes of `line`: a place, a message waiting for its place, or nothing.
    fn read_line(&self, cwd: &Path, line: &str) -> Option<Read> {
        let caps = self.re.captures(line)?;
        let (sev, msg) = self.said(&caps);
        if self.then.is_some() {
            return Some(Read::Waits(sev, msg));
        }
        Some(Read::Place(placed(cwd, &caps, sev, msg)))
    }

    /// The severity and message a line says.
    fn said(&self, caps: &regex::Captures) -> (u8, String) {
        let msg = self.message.clone().or_else(|| caps.name("message").map(|m| m.as_str().to_string()));
        match (caps.name("severity"), msg) {
            (Some(s), m) => (severity(s.as_str()).0, m.unwrap_or_default()),
            (None, m) => severity(m.as_deref().unwrap_or_default()),
        }
    }
}

enum Read {
    Place(Option<Item>),
    Waits(u8, String),
}

/// The place `caps` names — `place`, else `path`, `line`, `col` — if it's a file.
fn placed(cwd: &Path, caps: &regex::Captures, sev: u8, msg: String) -> Option<Item> {
    if let Some(p) = caps.name("place") {
        let (path, l, c, _) = split_place(p.as_str())?;
        return item(cwd, path, l, c, sev, msg);
    }
    let n = |k: &str| caps.name(k).and_then(|m| m.as_str().parse::<usize>().ok());
    item(cwd, caps.name("path")?.as_str(), n("line")?, n("col").unwrap_or(1), sev, msg)
}

/// Reads places out of a task's output, a line at a time: the forms the packs say (`[[places]]` — tsc's,
/// rustc's long one, Python's tracebacks), then the one every tool shares, `path:line[:col]: [error|warning]
/// message` (rustc `--message-format=short`, gcc/clang, go, eslint, ruff, mypy). Only paths that exist
/// count — a URL or a timestamp isn't a place.
pub struct Matcher {
    rules: std::sync::Arc<Vec<PlaceRule>>,
    /// A message waiting for its place: the rule's index, its severity and text.
    pending: Option<(usize, u8, String)>,
}

impl Matcher {
    pub fn new(rules: std::sync::Arc<Vec<PlaceRule>>) -> Matcher {
        Matcher { rules, pending: None }
    }

    /// The built-in packs' rules (tests).
    #[cfg(test)]
    pub(crate) fn builtin() -> Matcher {
        let mut rules = Vec::new();
        for (_, t) in crate::packs::builtin_manifests() {
            if let Some(v) = t.as_ref().unwrap().get("places") {
                rules.extend(crate::packs::places_of(v).unwrap());
            }
        }
        Matcher::new(std::sync::Arc::new(rules))
    }

    pub fn line(&mut self, cwd: &Path, line: &str) -> Option<Item> {
        let trimmed = line.trim();
        if let Some((i, sev, msg)) = self.pending.take() {
            let then = self.rules[i].then.as_ref().expect("a waiting rule has a `then`");
            match then.captures(trimmed) {
                Some(caps) => return placed(cwd, &caps, sev, msg),
                None => self.pending = Some((i, sev, msg)),
            }
        }
        for (i, r) in self.rules.iter().enumerate() {
            match r.read_line(cwd, trimmed) {
                Some(Read::Waits(sev, msg)) => {
                    self.pending = Some((i, sev, msg));
                    return None;
                }
                Some(Read::Place(Some(found))) => return Some(found),
                Some(Read::Place(None)) | None => {}
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
    /// Ended part-way on purpose, waiting for the user — a rebase at its conflicts (`rebase.rs`): what it
    /// waits on (`2 conflicts`).
    Stopped(String),
}

/// Places a run keeps, at most.
const PLACES_KEPT: usize = 5000;

/// The output a run keeps (#72: a long step pushed the steps before it out — a failed test's name with
/// them): each step's (a preflight's; a plain run is one step) first `STEP_HEAD` lines and its last, up to
/// `STEP_KEPT` in all, the lines between said on one line — trimmed a batch at a time, once it's
/// `TRIM_SLACK` over; and `OUTPUT_KEPT` lines in all, the oldest going first.
const STEP_HEAD: usize = 200;
const STEP_KEPT: usize = 3000;
const TRIM_SLACK: usize = 500;
const OUTPUT_KEPT: usize = 20_000;

pub struct Run {
    pub task: Task,
    pub state: State,
    /// (is stderr, line).
    pub output: Vec<(bool, String)>,
    /// Every line's bytes it printed, a newline each — what was cut from `output` too.
    pub output_bytes: usize,
    pub started: Instant,
    pub took: Option<Duration>,
    pid: Option<u32>,
    generation: u64,
    /// (errors, warnings) among its places.
    pub found: (usize, usize),
    /// The places its output pointed at (each once, at most `PLACES_KEPT`) — Claude's report and the jump
    /// to the first error read these; the user's list gets them only from a run that feeds it (#28).
    pub places: Vec<Item>,
    /// Its places went to the user's list too (a check or a build — #28); the report says where they are.
    pub feeds: bool,
    /// An install's (`install.rs`): what goes on once it ends.
    after: Option<After>,
    /// An install run (not one `space m m` runs again).
    pub install: bool,
    /// tarae's own git run (`rebase.rs`) — not one `space m m` runs again either.
    pub git: bool,
    /// Claude's `run` waiting for this run's end (`tools.rs`).
    pub chat: Option<crate::tools::Call>,
    /// The user ran Claude's command again (`blocked.rs`): how it ended goes to Claude with the next message,
    /// after this.
    pub tell: Option<String>,
    /// Claude's command (not one `space m m` runs again).
    pub by_claude: bool,
    /// How Claude's command ran — in the sandbox or not (`sandbox.rs`); None for the user's own.
    pub guard: Option<crate::sandbox::Guard>,
    /// Claude's command: where it ran and what it warned of, as its chat row and `tarae state` say it
    /// (`chat::Ran`, from `guard` and the card it ran from — cardburden 4).
    pub ran: Option<crate::chat::Ran>,
    /// When it last printed (a sandboxed run quiet after a denial may be waiting on it — the panel says so).
    pub last_output: Instant,
    /// The tallest its panel has been (`term::task_panel_h`) — it never shrinks while the run is up; a run of
    /// the same task again starts there.
    pub tall: std::cell::Cell<usize>,
    /// The output's first line shown, scrolled back (`sizes::bottom_key`, the wheel) — None: its end.
    pub scroll: Option<usize>,
    /// Where the step whose output comes now starts in `output` · its line saying what was cut (and how
    /// many), once it's been cut (`keep`).
    section: usize,
    cut: Option<(usize, usize)>,
    /// What it waits on that isn't its own work — another cargo's lock — from its output (`waits.rs`).
    pub wait: crate::waits::Waiting,
    /// Its way from the ask to its end, for the log (`trace.rs`).
    pub trace: crate::trace::RunTrace,
    /// Claude's: its dedupe key (`queued::key_of`), the calls that joined it, its time in line before it
    /// started (`queued.rs`).
    pub key: Option<String>,
    pub joined: Vec<crate::queued::Joined>,
    pub queued: Option<crate::queued::Queued>,
    /// Claude's command that isn't a check: the checkout as it found it — what it changed joins the turn at
    /// its end (`snapshot.rs`; its line's next run waits for that).
    pub snap: Option<crate::snapshot::Before>,
    /// Claude's: the containers it names from its prefix, stopped when it's stopped (`containers.rs`).
    pub containers: Option<crate::containers::Run>,
    /// Claude's run on a commit's tree (`basetree.rs`): said in its report, the tree removed at its end.
    pub base: Option<crate::basetree::Base>,
}

impl Run {
    /// Which run this is (each spawn a new one).
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Its process, while it runs (`:procs`).
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// The output's first line in `rows` rows: where it was scrolled to, else the end (the newest in sight).
    pub fn top(&self, rows: usize) -> usize {
        let max = self.output.len().saturating_sub(rows);
        self.scroll.map_or(max, |t| t.min(max))
    }

    /// `by` lines down (up when negative); back at the end, it follows the output again.
    pub fn scroll_by(&mut self, by: isize, rows: usize) {
        let max = self.output.len().saturating_sub(rows);
        let top = self.top(rows).saturating_add_signed(by).min(max);
        self.scroll = (top < max).then_some(top);
    }

    /// Scrolled to line `at` at the top (the end follows the output again).
    pub fn scroll_to(&mut self, at: usize, rows: usize) {
        let max = self.output.len().saturating_sub(rows);
        self.scroll = (at < max).then_some(at);
    }

    /// A line of output; `step`: it begins a step of its own (a preflight's heading) — the one before is
    /// trimmed first.
    pub fn push_line(&mut self, err: bool, line: String, step: bool) {
        if step {
            self.keep(0);
            (self.section, self.cut) = (self.output.len(), None);
        }
        self.output_bytes += line.len() + 1;
        self.output.push((err, line));
    }

    /// The output trimmed to what's kept (see `STEP_KEPT`) — once the step in hand is `slack` lines over.
    pub fn keep(&mut self, slack: usize) {
        let len = self.output.len() - self.section;
        if len > STEP_KEPT + slack {
            let excess = len - STEP_KEPT;
            let (marker, before) = match self.cut {
                Some(c) => c,
                None => {
                    let at = self.section + STEP_HEAD;
                    self.output.insert(at, (false, String::new()));
                    (at, 0)
                }
            };
            let from = marker + 1;
            self.output.drain(from..from + excess);
            self.cut = Some((marker, before + excess));
            self.output[marker].1 = format!("… {} lines not kept …", before + excess);
            self.scroll = self.scroll.map(|t| if t >= from + excess { t - excess } else { t.min(from) });
        }
        let n = self.output.len();
        if n > OUTPUT_KEPT {
            let k = n - OUTPUT_KEPT;
            self.output.drain(..k);
            self.section = self.section.saturating_sub(k);
            self.cut = self.cut.and_then(|(m, d)| (m >= k).then_some((m - k, d)));
            self.scroll = self.scroll.map(|t| t.saturating_sub(k));
        }
    }
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
            output_bytes: 0,
            started: Instant::now(),
            took: None,
            pid: None,
            generation: 0,
            found: (0, 0),
            places: Vec::new(),
            feeds: true,
            after: None,
            install: false,
            git: false,
            chat: None,
            tell: None,
            by_claude: false,
            guard: None,
            ran: None,
            last_output: Instant::now(),
            tall: Default::default(),
            scroll: None,
            section: 0,
            cut: None,
            wait: Default::default(),
            trace: crate::trace::RunTrace::new("test run"),
            key: None,
            joined: Vec::new(),
            queued: None,
            snap: None,
            containers: None,
            base: None,
        }
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if let Some(pid) = self.pid.take()
            && self.state == State::Running
        {
            crate::dap::term_group(pid);
            self.trace.end("stopped before its end");
            // (what it started in a container engine goes with it — `containers.rs`)
            if let Some(c) = self.containers.take() {
                c.stopped();
            }
        }
    }
}

/// A command as a trace's name says it: its first line.
pub(crate) fn traced(cmd: &str) -> String {
    let mut lines = cmd.trim().lines();
    let first = lines.next().unwrap_or_default();
    if lines.next().is_some() { format!("{first} …") } else { first.to_string() }
}

/// How a run ended, as its trace's last step says it.
fn ended(state: &State) -> String {
    match state {
        State::Passed => "exit 0".into(),
        State::Failed(Some(c)) => format!("exit {c}"),
        State::Failed(None) => "killed".into(),
        State::Stopped(why) => format!("stopped: {why}"),
        State::Running => "running".into(),
    }
}

impl Editor {
    /// `space m m` — run the last task again (or pick one, the first time).
    pub fn task_again(&mut self) {
        match self.session.task_last.clone() {
            Some(t) => self.task_start(t),
            None => self.task_picker(),
        }
    }

    /// `space m p` — pick a task (the last one first). The project is looked at on a worker.
    pub fn task_picker(&mut self) {
        let (configured, last) =
            (self.config.tasks.clone(), self.session.task_last.as_ref().map(|t| t.name.clone()));
        let root = self.session.root.clone();
        let fill = move || {
            let mut tasks = all_tasks(configured, &root);
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
        let picker = crate::picker::Picker::of("tasks", "tasks", Vec::new(), false).without_preview();
        self.open_picker(picker, Some(Box::new(fill)));
    }

    /// `:task <name>` — found on a worker, then started.
    pub fn task_named(&mut self, name: &str) -> Result<(), String> {
        let (configured, name, root) =
            (self.config.tasks.clone(), name.to_string(), self.session.root.clone());
        self.events.jobs().spawn(move || {
            let found = all_tasks(configured, &root).into_iter().find(|t| t.name == name);
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
            self.session.task_run.as_mut().filter(|r| r.state == State::Running).and_then(|r| r.after.take())
        {
            self.offer_finished(a.offer, Err("stopped before it finished".into()));
        }
        crate::tools::run_dropped(self, "the user stopped it");
        crate::restart::run_dropped(self);
        crate::rebase::run_dropped(self);
        self.session.task_run = None;
        // (the panel is free: what waits for it starts)
        crate::queued::run_next(self);
    }

    /// The user's task — Claude's command running in the panel isn't stopped: the task waits first in line
    /// (`queued.rs`; `enter` in the panel runs it now).
    pub fn task_start(&mut self, task: Task) {
        if self.install_running() {
            return;
        }
        self.session.task_last = Some(task.clone());
        if crate::queued::claudes_running(&self.session, crate::queued::Slot::Task) {
            return crate::queued::push(self, crate::queued::What::Task(task));
        }
        self.task_start_now(task, false, None);
    }

    /// The user's task starts — `force`: over Claude's command running (it stops); else one of Claude's that
    /// started while the saves were written keeps the panel, and the task waits for it. `trace`: its own from
    /// its place in line (the same run, the same id).
    pub(crate) fn task_start_now(&mut self, task: Task, force: bool, trace: Option<crate::trace::RunTrace>) {
        let trace = trace.unwrap_or_else(|| crate::trace::RunTrace::new(format!("task: {}", task.name)));
        // (it runs what's there outside the sandbox: what Claude's runs made that the sandbox keeps, looked at
        // first — G23)
        if !self.session.made_protected.is_empty() {
            trace.step("protected-made card up");
            return crate::tools::ask_made(self, crate::tools::MadeThen::Task { task, force, trace });
        }
        self.auto_save(" · before the task");
        // (it runs what's on disk: once the saves are written — `after_saves`)
        self.after_saves(move |ed| {
            trace.step("saved");
            if ed.install_running() {
                return trace.end("an install was running");
            }
            if !force && crate::queued::claudes_running(&ed.session, crate::queued::Slot::Task) {
                return crate::queued::again(ed, crate::queued::What::Task(task), Some(trace));
            }
            ed.task_spawn(task, None, None, true, false, &[], trace);
        });
    }

    /// Not over an install: stopping it half-way can leave a tool broken (said).
    fn install_running(&mut self) -> bool {
        let running = self.session.task_run.as_ref().is_some_and(|r| r.install && r.state == State::Running);
        if running {
            self.set_status("an install is running — space m c stops it");
        }
        running
    }

    /// A command Claude runs (`tools.rs`): like a task, but not the one `space m m` runs again — the call
    /// waits for its end (or, the user's run of it again, Claude hears it later); in the sandbox, unless
    /// `guard` says otherwise. Its places go to the list only when it's a check or a build
    /// (`tools::is_check`, #28) — a grep of Claude's looking around doesn't replace the user's list. `ride`:
    /// what goes along to the run (`queued.rs`). Err (who and what rode, back): it didn't start (a run is
    /// under way, or it couldn't spawn). Saves first: `tools::go` starts it `after_saves`.
    pub(crate) fn task_start_for(
        &mut self,
        task: Task,
        who: crate::tools::For,
        guard: Option<crate::sandbox::Guard>,
        ride: crate::queued::Ride,
    ) -> Result<(), Box<(crate::tools::For, crate::queued::Ride)>> {
        if self.session.task_run.as_ref().is_some_and(|r| r.state == State::Running) {
            return Err(Box::new((who, ride)));
        }
        // (Claude's call's trace goes on — from its ask; the user's run again of it starts here)
        let trace = match &who {
            crate::tools::For::Claude(call) => call.trace.clone(),
            crate::tools::For::User(_) => ride.trace.clone(),
        };
        let trace = trace.unwrap_or_else(|| {
            crate::trace::RunTrace::new(format!("claude's run again: {}", traced(&task.cmd)))
        });
        // (saved and written by now — `tools::go` started this `after_saves`)
        trace.step("saved");
        let before = self.task_generation;
        let policy = match &guard {
            Some(crate::sandbox::Guard::In(p)) => Some(&**p),
            _ => None,
        };
        // (one on a commit's tree: its places are in a copy that goes at its end — not for the list)
        let feeds = crate::tools::is_check(&task.cmd) && ride.extra.base.is_none();
        // Where it runs, and what it warns of: its card's warnings — none, judged on its line now (it ran
        // unasked: said only on its chat row and in the log — D2)
        let ran = guard.as_ref().map(|g| {
            let warned = ride.warned.clone().unwrap_or_else(|| {
                let cwd = self.session.root.join(&task.cwd);
                let outside = matches!(g, crate::sandbox::Guard::Outside);
                let on = crate::tools::WarnOn {
                    cmd: &task.cmd,
                    cwd: &cwd,
                    scripts: &[],
                    reads: &[],
                    script_changed: false,
                    outside,
                };
                let warned = crate::tools::warned(self, on);
                crate::cards::ran_warned(&self.session.log(), &task.cmd, &warned);
                warned
            });
            crate::chat::Ran::of(g, ride.card, ride.reason.as_deref(), warned)
        });
        self.task_spawn(task, None, policy, feeds, true, &[], trace);
        match self.session.task_run.as_mut().filter(|_| self.task_generation != before) {
            Some(r) => {
                match who {
                    crate::tools::For::Claude(call) => r.chat = Some(call),
                    crate::tools::For::User(head) => r.tell = Some(head),
                }
                r.by_claude = true;
                r.guard = guard;
                r.ran = ran;
                // (a time limit its own command sets: the stuck card waits for it — cardburden 5)
                r.wait.limited(r.started, crate::stuck::limit(&r.task.cmd));
                (r.key, r.joined, r.queued, r.snap) = (ride.key, ride.joined, ride.queued, ride.snap);
                r.base = ride.extra.base;
                Ok(())
            }
            None => Err(Box::new((who, ride))),
        }
    }

    /// `:restart`'s build (`restart.rs`): not the task `space m m` runs again — the restart waits for its
    /// end. Its generation; None if it didn't start (an install is running, or it couldn't spawn).
    pub(crate) fn task_start_restart(&mut self, task: Task) -> Option<u64> {
        if self.session.task_run.as_ref().is_some_and(|r| r.install && r.state == State::Running) {
            return None;
        }
        let before = self.task_generation;
        let trace = crate::trace::RunTrace::new(format!("restart's build: {}", traced(&task.cmd)));
        self.task_spawn(task, None, None, true, false, &[], trace);
        (self.task_generation != before).then_some(self.task_generation)
    }

    /// `:preflight`'s run (`preflight.rs`), `trace` from its ask: not the task `space m m` runs again; every
    /// step's places go to the list. Its generation; None if it didn't start.
    pub(crate) fn task_start_preflight(&mut self, task: Task, trace: crate::trace::RunTrace) -> Option<u64> {
        let before = self.task_generation;
        self.task_spawn(task, None, None, true, false, &[], trace);
        (self.task_generation != before).then_some(self.task_generation)
    }

    /// tarae's own git run (`rebase.rs`), with `env` (what its line names, kept out of the line): not the
    /// task `space m m` runs again, and its output's places stay out of the list. Its generation; None if it
    /// didn't start.
    pub(crate) fn task_start_git(&mut self, task: Task, env: &[(String, String)]) -> Option<u64> {
        let before = self.task_generation;
        let trace = crate::trace::RunTrace::new(format!("tarae's git: {}", traced(&task.cmd)));
        self.task_spawn(task, None, None, false, false, env, trace);
        let r = self.session.task_run.as_mut().filter(|_| self.task_generation != before)?;
        r.git = true;
        Some(self.task_generation)
    }

    /// An install (`install.rs`) in the task panel: not the task `space m m` runs again, and its output's
    /// places stay out of the list.
    pub(crate) fn task_start_install(&mut self, task: Task, after: After) {
        let trace = crate::trace::RunTrace::new(format!("install: {}", traced(&task.cmd)));
        self.task_spawn(task, Some(after), None, false, false, &[], trace);
    }

    /// `feeds`: its places make the list (`]q`) — the run takes it over from its start. `claudes`: a
    /// command of Claude's — without `CDPATH`, so a `cd` lands where `readonly::outside` resolved it (with
    /// it set, `cd src` may go to another `src` on it, and the line read there unasked). `env`: variables it
    /// runs with besides tarae's own. `trace`: its way so far, logged at its end.
    #[allow(clippy::too_many_arguments)]
    fn task_spawn(
        &mut self,
        task: Task,
        after: Option<After>,
        sandbox: Option<&crate::sandbox::Policy>,
        feeds: bool,
        claudes: bool,
        env: &[(String, String)],
        trace: crate::trace::RunTrace,
    ) {
        // (a configured task's folder is as written — from the root, the root itself when it says none)
        let task = Task { cwd: self.path_of(&task.cwd), ..task };
        let install = after.is_some();
        let watched = sandbox.is_some_and(|p| p.watch.is_some());
        let after_run = crate::sandbox::after_run(sandbox);
        let run_id = sandbox.map(|p| p.run_id);
        let mut cmd = crate::sandbox::command(sandbox, "sh", &["-c".into(), task.cmd.clone()]);
        // (the user's own find this tarae's UI state — `tarae state`; Claude's never get a socket's path, and
        // couldn't reach it from the sandbox anyway)
        match crate::uisocket::bound().filter(|_| !claudes) {
            Some(ui) => cmd.env("TARAE_UI", ui),
            None => cmd.env_remove("TARAE_UI"),
        };
        if claudes {
            cmd.env_remove("CDPATH");
        }
        // (Claude's: the prefix its containers are named from, stopped with it — `containers.rs`)
        let containers = claudes.then(|| crate::containers::prefix(self.session.id, trace.id()));
        match &containers {
            Some(p) => cmd.env(crate::containers::VAR, p),
            None => cmd.env_remove(crate::containers::VAR),
        };
        cmd.envs(env.iter().map(|(k, v)| (k, v)));
        cmd.current_dir(&task.cwd);
        color_off(&mut cmd);
        // (which engine its containers are on: its line's settings over its environment)
        let containers = containers.map(|p| (p, crate::containers::Reach::of_command(Some(&task.cmd), &cmd)));
        // (its own session and group, not a fork of the editor — `spawn.rs`; on Linux from a thread confined
        // to its sandbox)
        let acts = if claudes { crate::spawn::Acts::Claude } else { crate::spawn::Acts::User };
        let child = match crate::sandbox::spawn_as(sandbox, &cmd, acts) {
            Ok(c) => c,
            Err(e) => {
                trace.end(format!("didn't spawn: {e}"));
                if let Some(a) = after {
                    return self.offer_finished(a.offer, Err(format!("{}: {e}", task.name)));
                }
                self.set_error(format!("{}: {e}", task.name));
                // (the panel is still free: what waits for it — Claude's own failure says so itself, `tools::fail`)
                if !claudes {
                    crate::queued::run_next(self);
                }
                return;
            }
        };
        trace.step("spawned");
        // A finished test run gives the panel over
        if self.session.test_run.as_ref().is_some_and(|r| r.state != crate::testing::RunState::Running) {
            self.session.test_run = None;
        }
        crate::tools::run_dropped(self, "another task took its place");
        crate::restart::run_dropped(self);
        crate::rebase::run_dropped(self);
        self.task_generation += 1;
        let generation = self.task_generation;
        let pid = child.pid;
        // (its watch ends with this process, whatever becomes of the run here)
        if let Some(w) = sandbox.and_then(|p| p.watch.as_ref()) {
            w.run_started(pid);
        }
        let title = if claudes { "claude's command" } else { task.name.as_str() };
        self.procs_spawned(pid, title, &task.cmd);
        crate::queued::started(self, crate::queued::Slot::Task, &traced(&task.name));
        let (stdout, stderr) = (child.stdout, child.stderr);
        let tx = self.events.sender();
        // Each stream reads its places on its own thread (a stat per `path:line` — never the main loop's)
        let cwd = task.cwd.clone();
        // (a line a pack says is a wait — `waits.rs` — is told here, off the main loop, as places are)
        let send_lines = move |tx: &crate::event::Tx,
                               matcher: &mut Matcher,
                               waits: &crate::waits::Rules,
                               err: bool,
                               lines: Vec<String>| {
            let at = Instant::now();
            let lines: Vec<(String, Option<Item>, Option<crate::waits::Hit>)> = lines
                .iter()
                .map(|l| {
                    // (as shown first: `strip_ansi` drops the carriage returns)
                    let line = crate::testing::strip_ansi(as_shown(l));
                    let found = matcher.line(&cwd, &line);
                    let hit = waits.hit(&line);
                    (line, found, hit)
                })
                .collect();
            let apply = move |ed: &mut Editor| {
                let Some(r) = ed.session.task_run.as_mut().filter(|r| r.generation == generation) else {
                    return;
                };
                // (a preflight's markers move its steps on, and aren't shown — `preflight.rs`)
                let mut preflight = ed.session.preflight.as_mut().filter(|p| p.generation == generation);
                let mut places = Vec::new();
                r.last_output = Instant::now();
                for (line, found, hit) in lines {
                    // (a wait's line is the run's state — the panel says it — not output)
                    if r.wait.line(hit, at, &r.trace) {
                        continue;
                    }
                    // (a preflight step's end: the trace says how long it took)
                    if let Some(ended) = preflight.as_deref().and_then(|p| p.ending(&line)) {
                        r.trace.step_at(ended, at);
                    }
                    // (a step's start: its output kept as its own — `Run::keep`)
                    let step = preflight.is_some() && crate::preflight::starts(&line);
                    let shown = match preflight.as_deref_mut() {
                        Some(p) => p.line(&line),
                        None => Some(line),
                    };
                    if let Some(line) = shown {
                        r.push_line(err, line, step);
                    }
                    places.extend(found.filter(|_| !install));
                }
                r.keep(TRIM_SLACK);
                for it in places {
                    let Some(r) = ed.session.task_run.as_mut() else { break };
                    // Counted once (the same place twice isn't two) — the run's own, for its report; the
                    // user's list only from a run that feeds it (#28)
                    let seen = |p: &Item| p.path == it.path && p.line == it.line && p.message == it.message;
                    if r.places.iter().any(seen) || r.places.len() >= PLACES_KEPT {
                        continue;
                    }
                    match it.severity {
                        1 => r.found.0 += 1,
                        2 => r.found.1 += 1,
                        _ => {}
                    }
                    r.places.push(it.clone());
                    if feeds {
                        ed.qlist_push(it);
                    }
                }
            };
            tx.send(crate::event::Event::Job(Box::new(apply))).is_ok()
        };
        crate::event::thread("tarae-task", move || {
            // (the packs read here — one of one's own is a file)
            let waits = std::sync::Arc::new(crate::waits::Rules::load());
            let places = std::sync::Arc::new(crate::packs::places());
            let (eof, read_all) = std::sync::mpsc::channel();
            let mut readers = 0;
            for (stream, err) in [(stdout, false), (stderr, true)] {
                let Some(s) = stream else { continue };
                let (tx, send_lines, waits, eof) =
                    (tx.clone(), send_lines.clone(), waits.clone(), eof.clone());
                let places = places.clone();
                readers += 1;
                crate::event::thread("tarae-task-read", move || {
                    let mut matcher = Matcher::new(places);
                    read_batches(s, |lines| send_lines(&tx, &mut matcher, &waits, err, lines));
                    let _ = eof.send(());
                });
            }
            let status = crate::spawn::wait_and_drain(pid, &read_all, readers).ok();
            // What it removed inside protected folders is made again before its end is told (`sandbox::after_run`)
            after_run();
            // Its last denials reach the watch before its end is told (here, not on the main loop)
            if watched {
                std::thread::sleep(crate::sandbox::WATCH_LAST);
            }
            let done = move |ed: &mut Editor| {
                // (a host its proxy holds for it has no one left to ask for — its card goes)
                crate::hosts::run_ended(ed, run_id);
                let session = ed.session.log();
                let Some(r) = ed.session.task_run.as_mut().filter(|r| r.generation == generation) else {
                    return;
                };
                if let Some(w) = r.guard.as_ref().and_then(|g| g.watch()) {
                    w.finish();
                    crate::cards::denied(&session, &r.task.name, w);
                }
                r.took = Some(r.started.elapsed());
                r.pid = None;
                r.state = match status.and_then(|s| s.code()) {
                    Some(0) => State::Passed,
                    code => State::Failed(code),
                };
                // (a wait it ended in — killed while it waited — ends with it)
                r.wait.end(Instant::now(), &r.trace);
                r.trace.end(ended(&r.state));
                // (what waits for the panel goes next — once this one's end is told; once what it changed is
                // taken, for a run whose changes are — `tools::run_done`)
                if !ed.session.runs_next.is_empty() && !(r.snap.is_some() && r.chat.is_some()) {
                    let next = crate::event::Event::Job(Box::new(crate::queued::run_next));
                    let _ = ed.events.sender().send(next);
                }
                let Some(r) = ed.session.task_run.as_mut() else { return };
                let (passed, errors) = (r.state == State::Passed, r.found.0);
                if crate::restart::built(ed, generation, passed, errors)
                    || ed.preflight_done(generation)
                    || crate::rebase::ran(ed, generation)
                {
                    return;
                }
                let Some(r) = ed.session.task_run.as_mut() else { return };
                if let Some(after) = r.after.take() {
                    let passed = r.state == State::Passed;
                    return ed.install_done(after, passed);
                }
                if r.chat.is_some() || r.tell.is_some() {
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
        if feeds {
            self.qlist_set(task.name.clone(), Vec::new());
        }
        // The same task again: its panel starts as tall as it was (no shrink and grow back)
        let tall =
            self.session.task_run.as_ref().filter(|r| r.task.name == task.name).map_or(0, |r| r.tall.get());
        let label = format!("Claude's command `{}`", traced(&task.cmd));
        let containers =
            containers.map(|(p, reach)| crate::containers::Run::new(p, label, self.events.sender(), reach));
        self.session.task_run = Some(Run {
            task,
            state: State::Running,
            output: Vec::new(),
            output_bytes: 0,
            started: Instant::now(),
            took: None,
            pid: Some(pid),
            generation,
            found: (0, 0),
            places: Vec::new(),
            feeds,
            after,
            install,
            git: false,
            chat: None,
            tell: None,
            by_claude: false,
            guard: None,
            ran: None,
            last_output: Instant::now(),
            tall: std::cell::Cell::new(tall),
            scroll: None,
            section: 0,
            cut: None,
            wait: Default::default(),
            trace,
            key: None,
            joined: Vec::new(),
            queued: None,
            snap: None,
            containers,
            base: None,
        });
    }
}

/// A line as a terminal leaves it: what comes after its last carriage return — progress redrawn in place
/// read as one line glued to what followed (`Rebasing (2/2)Successfully rebased…` — a shot, 2026-10-04). A
/// CRLF line's own `\r` isn't one.
fn as_shown(line: &str) -> &str {
    let line = line.trim_end_matches('\r');
    line.rsplit('\r').next().unwrap_or(line)
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
    use std::process::{Command, Stdio};

    /// Progress redrawn in place shows as a terminal leaves it: the text after the last carriage return.
    #[test]
    fn a_line_redrawn_in_place_shows_its_last_state() {
        assert_eq!(as_shown("Rebasing (2/2)\rSuccessfully rebased"), "Successfully rebased");
        assert_eq!(as_shown(" 10%\r 50%\r100%"), "100%");
        assert_eq!(as_shown("plain\r"), "plain", "a CRLF line's own end");
        assert_eq!(as_shown("plain"), "plain");
        // Through a run, colours and all (`strip_ansi` drops carriage returns — it came first once)
        let mut ed = Editor::new(crate::config::Config::default());
        let cmd = r"printf 'Rebasing (2/2)\r\033[1mSuccessfully\033[0m rebased\n'".to_string();
        ed.task_start(Task { name: "redrawn".into(), cmd, cwd: std::env::temp_dir() });
        let end = Instant::now() + Duration::from_secs(10);
        while ed.session.task_run.as_ref().is_some_and(|r| r.state == State::Running) {
            assert!(Instant::now() < end, "the run ended");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                ed.handle_event(ev);
            }
        }
        let out: Vec<&str> =
            ed.session.task_run.as_ref().unwrap().output.iter().map(|l| l.1.as_str()).collect();
        assert_eq!(out, ["Successfully rebased"]);
    }

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

    /// #72: each step's output is kept on its own — its start and its end, the lines between said on one
    /// line — so a long step after it doesn't push a failed test's name out (5000 lines in all did); a
    /// line scrolled to stays in sight as lines go.
    #[test]
    fn each_steps_output_is_kept_its_start_and_its_end() {
        let task = Task { name: "preflight".into(), cmd: String::new(), cwd: PathBuf::from("/") };
        let mut r = Run::for_test(task);
        let batch = |r: &mut Run, lines: Vec<String>, step: bool| {
            for (k, l) in lines.into_iter().enumerate() {
                r.push_line(false, l, step && k == 0);
            }
            r.keep(TRIM_SLACK);
        };
        batch(&mut r, vec!["── test ──".into()], true);
        for chunk in 0..10 {
            batch(&mut r, (0..1000).map(|i| format!("test t{chunk}_{i} ... ok")).collect(), false);
        }
        batch(&mut r, vec!["test tricky ... FAILED".into(), "failures:".into(), "    tricky".into()], false);
        let lines = |r: &Run| r.output.iter().map(|l| l.1.clone()).collect::<Vec<_>>();
        let out = lines(&r);
        assert!(out.len() <= STEP_KEPT + TRIM_SLACK + 1, "{}", out.len());
        assert_eq!(out[1], "test t0_0 ... ok", "its start");
        assert!(
            out[STEP_HEAD].starts_with("… ") && out[STEP_HEAD].ends_with(" lines not kept …"),
            "{}",
            out[STEP_HEAD]
        );
        assert!(out.ends_with(&["test tricky ... FAILED".into(), "failures:".into(), "    tricky".into()]));
        // Scrolled to its failure, then a long step after it
        let at = r.output.iter().position(|l| l.1 == "test tricky ... FAILED").unwrap();
        r.scroll_to(at, 1);
        batch(&mut r, vec!["── clippy ──".into()], true);
        for chunk in 0..8 {
            batch(&mut r, (0..1000).map(|i| format!("warning: w{chunk}_{i}")).collect(), false);
        }
        let out = lines(&r);
        assert!(out.contains(&"    tricky".to_string()), "the failed test's name is kept");
        assert!(out.contains(&"── clippy ──".to_string()) && out.last().unwrap() == "warning: w7_999");
        let top = r.scroll.expect("still scrolled back");
        assert_eq!(r.output[top].1, "test tricky ... FAILED", "the line scrolled to stays in sight");
        // All of it is bounded
        for chunk in 0..40 {
            batch(&mut r, vec![format!("── s{chunk} ──")], true);
            batch(&mut r, (0..2000).map(|i| format!("s{chunk} {i}")).collect(), false);
        }
        assert!(r.output.len() <= OUTPUT_KEPT, "{}", r.output.len());
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
        let mut m = Matcher::builtin();
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

    /// A task's or a test run's output is read, not a terminal's: colours off — `NO_COLOR`, and the packs'
    /// `color-off` (cargo's `term.color = "always"` would colour it anyway).
    #[test]
    fn runs_read_plain_get_colours_off() {
        let mut cmd = std::process::Command::new("true");
        color_off(&mut cmd);
        let envs: Vec<(String, String)> = cmd
            .get_envs()
            .filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())))
            .collect();
        for want in [("NO_COLOR", "1"), ("CARGO_TERM_COLOR", "never")] {
            assert!(envs.iter().any(|(k, v)| (k.as_str(), v.as_str()) == want), "{want:?}: {envs:?}");
        }
    }

    /// Reading places out of a task's output — on its reader threads, a line at a time: 20k lines of cargo's
    /// long and short forms, tsc, a Python traceback and lines that are no place, the best of 5. A number for
    /// `cargo perf`.
    #[test]
    fn perf_task_places_lines() {
        if crate::test_results::unoptimized("task::tests::perf_task_places_lines") {
            return;
        }
        let d = dir();
        let mut out = Vec::new();
        for i in 0..1_000 {
            let n = i % 40 + 1;
            out.extend([
                format!("   Compiling crate{i} v0.1.{i}"),
                format!("warning: unused variable: `x{i}`"),
                format!("  --> src/main.rs:{n}:9"),
                "   |".to_string(),
                format!("{n} |     let x{i} = 1;"),
                "   |         ^^ help: if this is intentional, prefix it with an underscore".to_string(),
                "   |".to_string(),
                "   = note: `#[warn(unused_variables)]` on by default".to_string(),
                String::new(),
                format!("src/main.rs:{n}:5: error[E0425]: cannot find value `y{i}`"),
                format!("src/main.rs({n},2): error TS2304: Cannot find name 'y'."),
                "Traceback (most recent call last):".to_string(),
                format!("  File \"a.py\", line {n}, in <module>"),
                "    import missing".to_string(),
                "ModuleNotFoundError: No module named 'missing'".to_string(),
                format!("    Finished `dev` profile [unoptimized + debuginfo] target(s) in {i}.0s"),
                format!("see https://example.com:8080/x{i}"),
                format!("test m::t{i} ... ok"),
                format!("{i} | some text: with a colon"),
                format!("     Running unittests src/lib.rs (target/debug/deps/x-{i})"),
            ]);
        }
        let mut best = f64::MAX;
        for _ in 0..5 {
            let t0 = std::time::Instant::now();
            let mut m = Matcher::builtin();
            let found = out.iter().filter(|l| m.line(&d, l).is_some()).count();
            best = best.min(t0.elapsed().as_secs_f64() * 1000.0);
            assert_eq!(found, 4_000, "the long form, the short, tsc's and the traceback's");
        }
        let line = format!("PERF task_places_20k_lines_ms {best:.3} budget=150 noise=3\n");
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        assert!(best < 150.0, "{best:.1} ms for {} lines (budget 150)", out.len());
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
