//! `:preflight` — the project's checks before a push, as the project lists them (`.tarae.toml`):
//!
//! ```toml
//! [preflight]
//! steps = [
//!   { name = "fmt", cmd = "cargo fmt --check" },
//!   { name = "perf", cmd = "cargo perf", note = "not measured|SLOWER", full = true },
//! ]
//! ```
//!
//! Two sets (`Set`): `:preflight` — and the bring card's `p`, before a session goes into its base — runs the
//! quick one, every step but those marked `full = true`; `:preflight-full`, before publishing, runs them all.
//! A project that marks none has one set: both run every step, and nothing says "quick".
//!
//! Where the steps come from: the project's `[preflight]` (`.tarae.toml`, or your config for every project),
//! else the language packs' defaults for the kind of project it is (`packs::preflight` — Cargo.toml, go.mod,
//! a Gradle or Maven build without its own wrapper …; the commands are tarae's, but they build and run the
//! project's code, so the first run in a project shows them on a card that says so — `DEFAULTS_SAY`; a
//! project with a wrapper is told to list it, since the wrapper is the repository's own script); then
//! `.tarae.local.toml`'s steps, yours, added after either (`config.rs` — read only while git ignores it).
//!
//! One task run (`task.rs`, the task panel): an `sh` script runs the steps one after another — every one,
//! whatever the ones before did — each in a subshell with its output and errors as one stream, between
//! marker lines (`MARK`) that tarae reads off as they come: the step's row (name, time, how it ended) in the
//! panel, its output under a `── name ──` heading. Places any step's output points at go to the list
//! (`]q`) — all of them, the run feeds it from its start. A line matching a step's `note` (a regex) makes
//! its pass one with a note (a perf run on a busy machine). At the end a one-line summary: `5 of 6 passed —
//! clippy failed · 4m 12s`. Commands from a project file need the project trusted (`config.rs`, as
//! `restart-build` does).

use std::time::{Duration, Instant};

use crate::editor::Editor;

/// What a step's start and end lines begin with (the script prints them; they're read off, not shown).
const MARK: &str = "::tarae-preflight::";

/// A step's note in the summary, at most (chars).
const NOTE_SHOWN: usize = 40;

/// What the card before pack defaults run says of them: the commands are tarae's, but what they run is the
/// project's — its tests and build scripts — so running them is running its code.
pub const DEFAULTS_SAY: &str = "These are tarae's defaults, but they build and run this project's code (its \
tests and build scripts) — run them only for code you trust.";

/// …and under it, plainly: how a project has its own.
const DEFAULTS_REPLACED: &str = "A [preflight] of the project's own (.tarae.toml) replaces them.";

/// One step as the project lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub name: String,
    pub cmd: String,
    /// A line of its output matching this makes a pass one with a note.
    pub note: Option<String>,
    /// Where it came from: `.tarae.toml`, `.tarae.local.toml`, `config.toml`, `rust pack`.
    pub from: String,
    /// Only in the full set (`full = true`): left out of the quick one.
    pub full: bool,
}

/// Which steps a run takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Set {
    /// `:preflight`, and before a session is brought: every step but the `full` ones.
    Quick,
    /// `:preflight-full`, before publishing: every step.
    Full,
}

impl Set {
    /// The steps of `all` in this set.
    pub fn of(self, all: &[Step]) -> Vec<Step> {
        all.iter().filter(|s| self == Set::Full || !s.full).cloned().collect()
    }

    /// What a run of it over `all` is called — plain `preflight` when no step is marked `full` (both sets are
    /// every step, nothing to tell apart).
    pub fn title(self, all: &[Step]) -> &'static str {
        match (all.iter().any(|s| s.full), self) {
            (false, _) => "preflight",
            (true, Set::Quick) => "quick preflight",
            (true, Set::Full) => "full preflight",
        }
    }
}

/// `[preflight] steps = [{ name, cmd, note?, full? }, …]` from `file` (a path, or a pack's name) → the steps
/// (what's wrong said in `warnings`).
pub fn parse_config(v: &toml::Value, file: &str, warnings: &mut Vec<String>) -> Vec<Step> {
    let from = match file.contains('/') {
        true => {
            std::path::Path::new(file).file_name().map_or(file.to_string(), |n| n.to_string_lossy().into())
        }
        false => file.to_string(),
    };
    let Some(steps) = v.get("steps").and_then(toml::Value::as_array) else {
        warnings.push(format!("{file}: preflight: needs steps = [{{ name = \"…\", cmd = \"…\" }}, …]"));
        return Vec::new();
    };
    let mut out = Vec::new();
    for (i, s) in steps.iter().enumerate() {
        let get = |k: &str| s.get(k).and_then(toml::Value::as_str).map(str::to_string);
        let (Some(name), Some(cmd)) = (get("name"), get("cmd")) else {
            warnings.push(format!("{file}: preflight.steps[{i}]: needs name = \"…\" and cmd = \"…\""));
            continue;
        };
        let note = get("note");
        if let Some(n) = &note
            && regex::Regex::new(n).is_err()
        {
            warnings.push(format!("{file}: preflight.steps[{i}].note: not a regex: {n}"));
            continue;
        }
        let full = match s.get("full") {
            None => false,
            Some(toml::Value::Boolean(b)) => *b,
            Some(_) => {
                warnings.push(format!("{file}: preflight.steps[{i}].full: true or false"));
                continue;
            }
        };
        if out.iter().any(|s: &Step| s.name == name) {
            warnings.push(format!("{file}: preflight.steps[{i}]: a second step named \"{name}\""));
            continue;
        }
        out.push(Step { name, cmd, note, from: from.clone(), full });
    }
    out
}

/// What the card before a first run with pack defaults says: each source's steps under it, the commands in
/// a column.
pub fn shown(steps: &[Step]) -> String {
    let w = steps.iter().map(|s| s.name.chars().count()).max().unwrap_or(0);
    let mut out = String::new();
    let mut last = "";
    for s in steps {
        if s.from != last {
            last = &s.from;
            let whose = match s.from.ends_with(" pack") {
                true => format!("The {}'s defaults", s.from),
                false => format!("Yours, from {}", s.from),
            };
            out.push_str(&format!("{}{whose}:\n", if out.is_empty() { "" } else { "\n" }));
        }
        out.push_str(&format!("  {:<w$}  {}\n", s.name, s.cmd));
    }
    out.truncate(out.trim_end().len());
    out
}

/// The script that runs `steps`: each in a subshell (a `cd` or an `exit` in one stays in it), output and
/// errors as one stream (in order with the markers), all of them; it fails when one did.
pub fn script(steps: &[Step]) -> String {
    let mut s = String::from("fails=0\n");
    for (i, step) in steps.iter().enumerate() {
        s.push_str(&format!(
            "printf '%s\\n' '{MARK} start {i}'\n( {}\n) 2>&1\ncode=$?\nprintf '%s\\n' \"{MARK} end {i} $code\"\n\
             [ $code -eq 0 ] || fails=$((fails + 1))\n",
            step.cmd
        ));
    }
    s.push_str("[ $fails -eq 0 ]\n");
    s
}

/// Whether a line of the run's output is a step's start marker (its output begins after it).
pub fn starts(line: &str) -> bool {
    line.strip_prefix(MARK).is_some_and(|rest| rest.starts_with(" start "))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Waiting,
    Running,
    Passed,
    Failed(i32),
}

/// A step in a run: how it's going.
#[derive(Clone, Debug)]
pub struct StepRun {
    pub name: String,
    pub state: State,
    pub started: Option<Instant>,
    pub took: Option<Duration>,
    /// The first line of its output its `note` matched.
    pub note: Option<String>,
    note_re: Option<regex::Regex>,
}

/// A `:preflight` under way, or the last one (its rows stay while its run is in the panel).
pub struct Preflight {
    /// Its task run's generation (`task::Run::generation`).
    pub generation: u64,
    pub steps: Vec<StepRun>,
    pub started: Instant,
    /// When it ended.
    pub took: Option<Duration>,
    /// `quick preflight` · `full preflight` · `preflight` (`Set::title`).
    pub title: &'static str,
    /// The steps the quick set left out (the full set's own).
    pub left_out: usize,
}

impl Preflight {
    /// A run of `set` out of `all` the steps.
    pub fn new(generation: u64, all: &[Step], set: Set) -> Preflight {
        let ours = set.of(all);
        let steps = ours
            .iter()
            .map(|s| StepRun {
                name: s.name.clone(),
                state: State::Waiting,
                started: None,
                took: None,
                note: None,
                note_re: s.note.as_deref().and_then(|n| regex::Regex::new(n).ok()),
            })
            .collect();
        let (title, left_out) = (set.title(all), all.len() - ours.len());
        Preflight { generation, steps, started: Instant::now(), took: None, title, left_out }
    }

    /// A line of the run's output: a marker moves a step on (None: not shown — or the heading a step's
    /// output goes under); any other line is the running step's (its note, if it matches).
    pub fn line(&mut self, line: &str) -> Option<String> {
        let Some(rest) = line.strip_prefix(MARK) else {
            if let Some(s) = self.steps.iter_mut().find(|s| s.state == State::Running)
                && s.note.is_none()
                && s.note_re.as_ref().is_some_and(|re| re.is_match(line))
            {
                s.note = Some(line.trim().to_string());
            }
            return Some(line.to_string());
        };
        let mut w = rest.split_whitespace();
        let (what, i) = (w.next(), w.next().and_then(|i| i.parse::<usize>().ok()));
        let Some(s) = i.and_then(|i| self.steps.get_mut(i)) else { return Some(line.to_string()) };
        match what {
            Some("start") => {
                s.state = State::Running;
                s.started = Some(Instant::now());
                Some(format!("── {} ──", s.name))
            }
            Some("end") => {
                let code = w.next().and_then(|c| c.parse::<i32>().ok()).unwrap_or(-1);
                s.state = if code == 0 { State::Passed } else { State::Failed(code) };
                s.took = s.started.map(|t| t.elapsed());
                None
            }
            _ => Some(line.to_string()),
        }
    }

    /// The step a line of the run's output ends, as the run's trace says it (`clippy exit 1`) — None: not a
    /// step's end marker.
    pub fn ending(&self, line: &str) -> Option<String> {
        let mut w = line.strip_prefix(MARK)?.split_whitespace();
        if w.next() != Some("end") {
            return None;
        }
        let s = self.steps.get(w.next()?.parse::<usize>().ok()?)?;
        Some(format!("{} exit {}", s.name, w.next().unwrap_or("?")))
    }

    /// The run ended (`stopped`: before every step had its end — stopped, or the script died).
    pub fn finish(&mut self) {
        self.took = Some(self.started.elapsed());
        for s in &mut self.steps {
            if s.state == State::Running {
                s.state = State::Failed(-1);
                s.took = s.started.map(|t| t.elapsed());
            }
        }
    }

    /// The one line at the end: `5 of 6 passed — clippy failed · 4m 12s`, with the notes
    /// (`perf: not measured …`), or what was left unrun.
    pub fn summary(&self) -> String {
        let n = self.steps.len();
        let passed = self.steps.iter().filter(|s| s.state == State::Passed).count();
        let failed: Vec<&str> = self
            .steps
            .iter()
            .filter(|s| matches!(s.state, State::Failed(_)))
            .map(|s| s.name.as_str())
            .collect();
        let unrun = self.steps.iter().filter(|s| s.state == State::Waiting).count();
        let mut out = if passed == n { format!("all {n} passed") } else { format!("{passed} of {n} passed") };
        if !failed.is_empty() {
            out.push_str(&format!(" — {} failed", failed.join(", ")));
        }
        if unrun > 0 {
            out.push_str(&format!(" — stopped, {unrun} not run"));
        }
        // (a note cut short — its whole line is in the output)
        for s in self.steps.iter().filter(|s| s.state == State::Passed) {
            if let Some(note) = &s.note {
                let note = match note.char_indices().nth(NOTE_SHOWN) {
                    Some((i, _)) => format!("{}…", note[..i].trim_end()),
                    None => note.clone(),
                };
                out.push_str(&format!(" · {}: {note}", s.name));
            }
        }
        if let Some(t) = self.took {
            out.push_str(&format!(" · {}", long_duration(t)));
        }
        out
    }

    pub fn passed(&self) -> bool {
        self.steps.iter().all(|s| s.state == State::Passed)
    }

    /// Step `k`'s output in `output` (the run's, as kept): the lines under its heading, up to the next
    /// step's. None: it hasn't printed its heading (or it's gone from what's kept).
    pub fn output_of(&self, k: usize, output: &[(bool, String)]) -> Option<Vec<String>> {
        let heading = |s: &StepRun| format!("── {} ──", s.name);
        let head = heading(self.steps.get(k)?);
        let start = output.iter().position(|l| l.1 == head)? + 1;
        let others: Vec<String> =
            self.steps.iter().enumerate().filter(|(i, _)| *i != k).map(|(_, s)| heading(s)).collect();
        let end =
            output[start..].iter().position(|l| others.contains(&l.1)).map_or(output.len(), |e| start + e);
        Some(output[start..end].iter().map(|l| l.1.clone()).collect())
    }

    /// The step whose output a key asks for: one named (`which`), else the first that failed, else the one
    /// whose output is at line `top` of the panel (None: its end — the last that started).
    pub fn step_for(&self, which: &str, top: Option<usize>, output: &[(bool, String)]) -> Option<usize> {
        let which = which.trim();
        if !which.is_empty() {
            return self.steps.iter().position(|s| s.name == which);
        }
        if let Some(k) = self.steps.iter().position(|s| matches!(s.state, State::Failed(_))) {
            return Some(k);
        }
        let started = |k: &usize| self.steps[*k].state != State::Waiting;
        match top {
            Some(top) => (0..self.steps.len()).filter(started).rfind(|&k| {
                let head = format!("── {} ──", self.steps[k].name);
                output.iter().position(|l| l.1 == head).is_some_and(|at| at <= top)
            }),
            None => (0..self.steps.len()).rfind(started),
        }
    }

    /// What its end says, the set named: a quick one passed points at the full one; a failure, at `]q` when
    /// the run put `places` in the list.
    pub fn said(&self, places: usize) -> String {
        let mut out = format!("{}: {}", self.title, self.summary());
        if !self.passed() {
            if places > 0 {
                out.push_str(" — ]q goes to the first");
            }
        } else if self.left_out > 0 {
            let n = self.left_out;
            out.push_str(&format!(
                " — the full set ({n} more step{}) runs before publish: :preflight-full",
                if n == 1 { "" } else { "s" }
            ));
        }
        out
    }
}

/// `412ms` · `8.3s` · `4m 12s` — a run of minutes reads in minutes.
pub fn long_duration(d: Duration) -> String {
    let s = d.as_secs_f32();
    if s < 60.0 { crate::term::duration(s) } else { format!("{}m {:02}s", s as u64 / 60, s as u64 % 60) }
}

impl Editor {
    /// `o` in the task panel · `:preflight-output [step]` — a step's whole output (as kept) in a buffer of its
    /// own, to read and search: the step named, else the first that failed, else the one in view (#126: a
    /// failed step's output was hard to reach — the panel is a few rows, and the `▲ test` that was clicked
    /// was the test panel's tab).
    pub fn preflight_output(&mut self, which: &str) -> Result<(), String> {
        let run =
            self.session.task_run.as_ref().ok_or("no preflight in the task panel — :preflight runs one")?;
        let p = self
            .session
            .preflight
            .as_ref()
            .filter(|p| p.generation == run.generation())
            .ok_or("the task panel's run isn't a preflight")?;
        let k = p.step_for(which, run.scroll, &run.output).ok_or_else(|| match which.trim() {
            "" => "no step has printed anything yet".to_string(),
            w => format!(
                "no step {w} — {}",
                p.steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
            ),
        })?;
        let name = p.steps[k].name.clone();
        let lines = p.output_of(k, &run.output).ok_or(format!("{name} hasn't printed anything yet"))?;
        let title = format!("{} · {name}", p.title);
        let mut text = lines.join("\n");
        text.push('\n');
        // One buffer per step: an earlier one gives way to this output
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
        self.bottom_focus = false;
        // (on the failure, when it says where: the last `error`/`FAILED`/`panicked` line)
        let at = lines
            .iter()
            .rposition(|l| l.contains("FAILED") || l.contains("panicked") || l.starts_with("error"));
        if let Some(line) = at {
            let doc = self.doc_mut();
            let at = crate::movement::line_start(&doc.text, line);
            doc.set_selection(crate::selection::Selection::point(at));
            self.align_view(crate::viewalign::Place::Center);
        }
        Ok(())
    }

    /// `:preflight` (`set` quick) / `:preflight-full` — the project's steps (else the packs' defaults,
    /// looked for on a worker), then `.tarae.local.toml`'s, those of `set` run in the task panel. Err: why
    /// not now.
    pub fn preflight_start(&mut self, set: Set) -> Result<(), String> {
        self.preflight_start_asking(set, false)
    }

    /// `preflight_start` — `always`: on its card whatever the steps are (its card asked again after
    /// `:restart` was never answered: asking again never runs anything).
    pub fn preflight_start_asking(&mut self, set: Set, always: bool) -> Result<(), String> {
        self.preflight_free()?;
        // The project's own, held back until it's trusted: not the packs' instead
        if self.config_project_held("preflight.") {
            return Err("the project's preflight steps wait for it to be trusted — :project-trust".into());
        }
        let (project, local) = (self.config.preflight.clone(), self.config.preflight_local.clone());
        if !project.is_empty() {
            let steps = [project, local].concat();
            if always {
                let root = self.preflight_root();
                self.preflight_ask(root, steps, set, false);
                return Ok(());
            }
            return self.preflight_run(steps, set);
        }
        let root = self.preflight_root();
        self.events.jobs().spawn(move || {
            let packs = crate::packs::preflight(&root);
            move |ed: &mut Editor| match packs.held.first() {
                // (a kind of project the packs know, but it runs its checks its own way: say how)
                Some(why) if packs.steps.is_empty() && local.is_empty() => {
                    ed.session.bring_after_preflight = false;
                    ed.set_error(format!("no preflight steps — {why}"));
                }
                _ => ed.preflight_found_asking(root, packs.steps, local, set, always),
            }
        });
        Ok(())
    }

    /// The quick set's step names, when the configured steps (the project's, yours) draw a line between the
    /// sets — what the bring card says its `p` runs. None: one set (or the packs', unknown until looked for).
    pub fn preflight_quick(&self) -> Option<Vec<String>> {
        if self.config.preflight.is_empty() || self.config_project_held("preflight.") {
            return None;
        }
        let all = [&self.config.preflight[..], &self.config.preflight_local[..]].concat();
        all.iter().any(|s| s.full).then(|| Set::Quick.of(&all).into_iter().map(|s| s.name).collect())
    }

    /// The packs' default steps for `root` came (none: not a kind of project they know), with yours after:
    /// the first time in a project, a card says what will run, and that it runs the project's code.
    #[cfg(test)]
    pub(crate) fn preflight_found(
        &mut self,
        root: std::path::PathBuf,
        packs: Vec<Step>,
        local: Vec<Step>,
        set: Set,
    ) {
        self.preflight_found_asking(root, packs, local, set, false);
    }

    /// The packs' default steps for `root` came (none: not a kind of project they know), with yours after:
    /// the first time in a project, a card says what will run, and that it runs the project's code —
    /// `always`: on the card even when told already, or with no defaults among them.
    fn preflight_found_asking(
        &mut self,
        root: std::path::PathBuf,
        packs: Vec<Step>,
        local: Vec<Step>,
        set: Set,
        always: bool,
    ) {
        if packs.is_empty() && local.is_empty() {
            self.session.bring_after_preflight = false;
            return self.set_error(
                "no preflight steps — no language pack knows this kind of project; add [preflight] steps = \
                 [{ name = \"…\", cmd = \"…\" }, …] to .tarae.toml",
            );
        }
        let defaults = !packs.is_empty();
        let steps = [packs, local].concat();
        if always || (defaults && !self.preflight_told.contains(&root)) {
            return self.preflight_ask(root, steps, set, defaults);
        }
        if let Err(e) = self.preflight_run(steps, set) {
            self.session.bring_after_preflight = false;
            self.set_error(e);
        }
    }

    /// The card before `steps` run (`y` runs them): the packs' defaults (`defaults`) say they run the
    /// project's code; others, asked again after `:restart`, say why they're asked.
    fn preflight_ask(&mut self, root: std::path::PathBuf, steps: Vec<Step>, set: Set, defaults: bool) {
        let what = crate::offer::What::Preflight { root, steps: steps.clone(), set };
        let shown = shown(&set.of(&steps));
        if !defaults {
            let question = format!("{shown}\n\nAsked again after :restart — it hadn't been answered.");
            let o = crate::offer::Offer::new(
                "Run these checks?".into(),
                &question,
                "running",
                "didn't start".into(),
                what,
            );
            return self.push_offer(o);
        }
        let question = format!("{shown}\n\n{DEFAULTS_SAY}\n{DEFAULTS_REPLACED}");
        // (what yes means — the project's code runs — in the warning color)
        let last = question.split('\n').count() - 2;
        let o = crate::offer::Offer::new(
            "Run these checks?".into(),
            &question,
            "running",
            "didn't start".into(),
            what,
        )
        .warning(last..last + 1);
        self.push_offer(o);
    }

    /// `y` on that card: run, and no card again for this project until quit.
    pub(crate) fn preflight_answer(
        &mut self,
        id: u64,
        root: std::path::PathBuf,
        (steps, set): (Vec<Step>, Set),
        yes: bool,
    ) {
        self.offer_finished(id, Ok(()));
        if !yes {
            self.session.bring_after_preflight = false;
            return self.set_status("preflight not run");
        }
        self.preflight_told.insert(root);
        if let Err(e) = self.preflight_run(steps, set) {
            self.session.bring_after_preflight = false;
            self.set_error(e);
        }
    }

    /// Where the preflight runs: the project.
    fn preflight_root(&self) -> std::path::PathBuf {
        self.session.root.clone()
    }

    /// Err while a task runs (the panel is one run's).
    fn preflight_free(&self) -> Result<(), String> {
        match self.session.task_run.as_ref().filter(|r| r.state == crate::task::State::Running) {
            Some(r) => Err(format!("{} is running — let it finish, or stop it (space m c)", r.task.name)),
            None => Ok(()),
        }
    }

    /// The steps of `set` out of `all`, one after another, in the task panel (its tab named for the set) —
    /// started once the saves before it are written (`after_saves`; what stops it then is said there).
    fn preflight_run(&mut self, all: Vec<Step>, set: Set) -> Result<(), String> {
        self.preflight_free()?;
        let steps = set.of(&all);
        if steps.is_empty() {
            return Err("every preflight step is marked full = true — :preflight-full runs them".into());
        }
        // (its trace named for the set, and the steps it runs)
        let names: Vec<&str> = steps.iter().map(|s| s.name.as_str()).collect();
        let title = set.title(&all);
        let trace = crate::trace::RunTrace::new(format!("{title}: {}", names.join(", ")));
        self.auto_save(" · before the preflight");
        self.after_saves(move |ed| {
            trace.step("saved");
            let started = match ed.preflight_free() {
                Err(e) => {
                    trace.end(format!("didn't start: {e}"));
                    Err(e)
                }
                Ok(()) => {
                    let task = crate::task::Task {
                        name: title.into(),
                        cmd: script(&steps),
                        cwd: ed.preflight_root(),
                    };
                    ed.task_start_preflight(task, trace)
                        .ok_or_else(|| "the preflight didn't start".to_string())
                }
            };
            match started {
                Ok(generation) => ed.session.preflight = Some(Preflight::new(generation, &all, set)),
                Err(e) => {
                    ed.session.bring_after_preflight = false;
                    ed.set_error(e);
                }
            }
        });
        Ok(())
    }

    /// The task run `generation` ended — true if it was the preflight's (then its summary is said).
    pub fn preflight_done(&mut self, generation: u64) -> bool {
        let Some(p) = self.session.preflight.as_mut().filter(|p| p.generation == generation) else {
            return false;
        };
        p.finish();
        let places = self.session.task_run.as_ref().map_or(0, |r| r.found.0 + r.found.1);
        let (said, passed, title) = (p.said(places), p.passed(), p.title);
        if passed {
            self.set_success(said);
        } else {
            self.set_error(said);
        }
        self.rebase_preflight_done(passed, title);
        true
    }

    /// Whether the project file holds back something whose key starts with `prefix` (not trusted yet).
    fn config_project_held(&self, prefix: &str) -> bool {
        self.project_trust.as_ref().is_some_and(|p| {
            p.state != crate::trust::State::Trusted && p.items.iter().any(|(k, _)| k.starts_with(prefix))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(name: &str, cmd: &str) -> Step {
        Step { name: name.into(), cmd: cmd.into(), note: None, from: ".tarae.toml".into(), full: false }
    }

    #[test]
    fn steps_from_the_config() {
        let v: toml::Value = toml::from_str(
            "steps = [{ name = \"fmt\", cmd = \"cargo fmt --check\" }, { name = \"perf\", cmd = \"cargo perf\", \
             note = \"not measured\", full = true }, { name = \"x\" }, { name = \"bad\", cmd = \"y\", note = \
             \"(\" }, { name = \"odd\", cmd = \"z\", full = \"yes\" }]",
        )
        .unwrap();
        let mut w = Vec::new();
        let steps = parse_config(&v, "~/p/.tarae.toml", &mut w);
        assert_eq!(
            steps,
            [
                step("fmt", "cargo fmt --check"),
                Step { note: Some("not measured".into()), full: true, ..step("perf", "cargo perf") }
            ]
        );
        assert_eq!(w.len(), 3, "{w:?}");
        assert!(w[0].contains("steps[2]: needs name") && w[1].contains("not a regex"), "{w:?}");
        assert!(w[2].contains("steps[4].full: true or false"), "{w:?}");
        // A pack's steps say so
        assert_eq!(parse_config(&v, "rust pack", &mut Vec::new())[0].from, "rust pack");
    }

    /// The quick set leaves out the steps marked `full`, the full set is every step; a run says which — and
    /// with none marked, there's one set, every step in both, said plainly `preflight`.
    #[test]
    fn the_quick_set_leaves_out_the_full_steps_and_says_so() {
        let full = |name: &str| Step { full: true, ..step(name, "") };
        let all = [step("fmt", ""), full("clippy bundled"), step("test", ""), full("perf")];
        let names = |s: Vec<Step>| s.into_iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(names(Set::Quick.of(&all)), ["fmt", "test"]);
        assert_eq!(names(Set::Full.of(&all)), ["fmt", "clippy bundled", "test", "perf"]);
        assert_eq!((Set::Quick.title(&all), Set::Full.title(&all)), ("quick preflight", "full preflight"));
        // None marked: both run every step, and nothing says "quick"
        let one = [step("fmt", ""), step("test", "")];
        assert_eq!(names(Set::Quick.of(&one)), ["fmt", "test"]);
        assert_eq!((Set::Quick.title(&one), Set::Full.title(&one)), ("preflight", "preflight"));
        // Yours (`.tarae.local.toml`) can draw the line too — it's over every step
        let yours = [step("fmt", ""), Step { from: ".tarae.local.toml".into(), ..full("linux clippy") }];
        assert_eq!(Set::Quick.title(&yours), "quick preflight");
    }

    /// The end of a quick run that passed points at the full one; a full run's, a failed one's, and a run
    /// with one set don't.
    #[test]
    fn the_end_says_which_set_ran() {
        let full = |name: &str| Step { full: true, ..step(name, "") };
        let all = [step("fmt", ""), step("test", ""), full("clippy bundled"), full("perf")];
        let ran = |set: Set, all: &[Step], codes: &[i32]| {
            let mut p = Preflight::new(1, all, set);
            for (i, c) in codes.iter().enumerate() {
                p.line(&format!("{MARK} start {i}"));
                p.line(&format!("{MARK} end {i} {c}"));
            }
            p.finish();
            p.took = Some(Duration::from_secs(62));
            p
        };
        assert_eq!(
            ran(Set::Quick, &all, &[0, 0]).said(0),
            "quick preflight: all 2 passed · 1m 02s — the full set (2 more steps) runs before publish: \
             :preflight-full"
        );
        assert_eq!(
            ran(Set::Quick, &all, &[0, 1]).said(3),
            "quick preflight: 1 of 2 passed — test failed · 1m 02s — ]q goes to the first"
        );
        assert_eq!(ran(Set::Full, &all, &[0, 0, 0, 0]).said(0), "full preflight: all 4 passed · 1m 02s");
        let one = [step("fmt", "")];
        assert_eq!(ran(Set::Quick, &one, &[0]).said(0), "preflight: all 1 passed · 1m 02s");
        let p = Preflight::new(1, &all[..3], Set::Quick);
        assert_eq!((p.steps.len(), p.left_out), (2, 1));
    }

    /// The checks are one set in three places: `.tarae.toml`'s `[preflight]` (what `:preflight` runs here),
    /// CONTRIBUTING's list, and CI's jobs — CI read as it runs them (`--locked` aside, `RUSTFLAGS: -D
    /// warnings` making clippy's warnings errors, its feature matrix spelled out). Where CI differs on
    /// purpose: `cargo perf` (it runs only the budgets — noisy runners) and the network job (`--ignored`).
    #[test]
    fn the_checks_are_the_same_here_in_contributing_and_in_ci() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let read = |f: &str| std::fs::read_to_string(root.join(f)).unwrap();
        let set = |v: Vec<String>| v.into_iter().collect::<std::collections::BTreeSet<String>>();
        // Here
        let tarae: toml::Table = read(".tarae.toml").parse().unwrap();
        let ours = parse_config(&tarae["preflight"], ".tarae.toml", &mut Vec::new());
        let ours = set(ours.into_iter().map(|s| s.cmd).collect());
        // CONTRIBUTING: the block under its marker
        let contributing = read("CONTRIBUTING.md");
        let block =
            contributing.split("<!-- the checks:").nth(1).expect("the marker").split("```").nth(1).unwrap();
        let listed =
            set(block.lines().skip(1).map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect());
        assert_eq!(listed, ours, "CONTRIBUTING's checks and .tarae.toml's [preflight]");
        // CI
        let ci = read(".github/workflows/ci.yml");
        let warnings_are_errors = ci.lines().any(|l| l.trim() == "RUSTFLAGS: -D warnings");
        let features: Vec<String> = ci
            .lines()
            .filter_map(|l| l.trim().strip_prefix("features:"))
            .flat_map(|v| {
                v.trim().trim_matches(['[', ']']).split(',').map(|f| f.trim().trim_matches('"').to_string())
            })
            .collect();
        let mut ran = Vec::new();
        for cmd in
            ci.lines().filter_map(|l| l.trim().strip_prefix("- run: ")).filter(|c| c.starts_with("cargo "))
        {
            // (on purpose: perf as budgets only, and the network job)
            if cmd.contains("--profile perf") || cmd.contains("-- --ignored") {
                continue;
            }
            let cmd = cmd.replace(" --locked", "");
            let each: Vec<String> = match cmd.contains("\"${{ matrix.features }}\"") {
                true => features
                    .iter()
                    .map(|f| match f.is_empty() {
                        true => cmd.replace(" --features \"${{ matrix.features }}\"", ""),
                        false => cmd.replace("\"${{ matrix.features }}\"", f),
                    })
                    .collect(),
                false => vec![cmd],
            };
            for c in each {
                let errors = warnings_are_errors && c.starts_with("cargo clippy");
                ran.push(if errors { format!("{c} -- -D warnings") } else { c });
            }
        }
        let mut expected = ours.clone();
        expected.remove("cargo perf");
        assert_eq!(set(ran), expected, "CI's checks and .tarae.toml's [preflight] (perf aside)");
    }

    /// Clippy in the checks lints the whole workspace: without `--workspace`, cargo in a workspace with a root
    /// package lints only that package — tarae's `netlink/` and `xtask/` went unlinted by `:preflight` and CI
    /// alike. Here (`.tarae.toml`, which CONTRIBUTING and CI follow — the test above) and in the Rust pack's
    /// defaults, for any project shaped so.
    #[test]
    fn clippy_in_the_checks_lints_every_package_of_the_workspace() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let read = |f: &str| std::fs::read_to_string(root.join(f)).unwrap();
        let tarae: toml::Table = read(".tarae.toml").parse().unwrap();
        let ours = parse_config(&tarae["preflight"], ".tarae.toml", &mut Vec::new());
        let pack: toml::Table = read("runtime/packs/rust/pack.toml").parse().unwrap();
        let defaults = pack["preflight"].as_array().unwrap()[0].clone();
        let defaults = parse_config(&defaults, "rust pack", &mut Vec::new());
        let clippy: Vec<String> = ours
            .iter()
            .chain(&defaults)
            .map(|s| s.cmd.clone())
            .filter(|c| c.starts_with("cargo clippy"))
            .collect();
        assert!(clippy.len() >= 3, "{clippy:?}");
        for c in &clippy {
            assert!(c.split_whitespace().any(|w| w == "--workspace"), "{c}: lints only the root package");
        }
    }

    /// The card before pack defaults run: each source's steps under it, a pack's said to be tarae's own.
    #[test]
    fn the_card_says_what_runs_and_whose_it_is() {
        let pack = |name: &str, cmd: &str, from: &str| Step { from: from.into(), ..step(name, cmd) };
        let steps = [
            pack("fmt", "cargo fmt --all --check", "rust pack"),
            pack("go test", "go test ./...", "go pack"),
            pack("lint", "make lint", ".tarae.local.toml"),
        ];
        assert_eq!(
            shown(&steps),
            "The rust pack's defaults:\n  fmt      cargo fmt --all --check\n\n\
             The go pack's defaults:\n  go test  go test ./...\n\n\
             Yours, from .tarae.local.toml:\n  lint     make lint"
        );
    }

    #[test]
    fn the_summary_says_what_failed_what_has_notes_and_how_long() {
        let steps =
            [step("fmt", ""), step("clippy", ""), Step { note: Some("busy".into()), ..step("perf", "") }];
        let mut p = Preflight::new(1, &steps, Set::Full);
        for l in [
            format!("{MARK} start 0"),
            format!("{MARK} end 0 0"),
            format!("{MARK} start 1"),
            "error: x".into(),
            format!("{MARK} end 1 101"),
            format!("{MARK} start 2"),
            "not measured (machine busy)".into(),
            format!("{MARK} end 2 0"),
        ] {
            p.line(&l);
        }
        p.finish();
        p.took = Some(Duration::from_secs(252));
        assert_eq!(p.summary(), "2 of 3 passed — clippy failed · perf: not measured (machine busy) · 4m 12s");
        assert!(!p.passed());
        // Stopped half-way: the one under way failed, the rest not run
        let mut p = Preflight::new(1, &steps, Set::Full);
        p.line(&format!("{MARK} start 0"));
        p.finish();
        p.took = Some(Duration::from_millis(1500));
        assert_eq!(p.summary(), "0 of 3 passed — fmt failed — stopped, 2 not run · 1.5s");
    }

    /// A preflight's trace says each step's end and a step's wait on another cargo's lock (a stand-in `sh`
    /// printing cargo's line) — the wait isn't output, and the steps go on after it.
    #[test]
    fn a_preflights_trace_says_each_step_and_its_waits() {
        let dir = std::env::temp_dir().join(format!("tarae-preflight-trace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ed = Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        let waits =
            "printf '    Blocking waiting for file lock on build directory\\n' >&2; sleep 0.2; echo built";
        ed.preflight_run(vec![step("first", waits), step("second", "exit 3")], Set::Quick).unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        while ed.session.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running) {
            assert!(Instant::now() < end, "it ended");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(10)) {
                ed.handle_event(ev);
            }
        }
        let r = ed.session.task_run.as_ref().unwrap();
        assert!(!r.output.iter().any(|l| l.1.contains("Blocking")), "{:?}", r.output);
        assert!(r.output.iter().any(|l| l.1 == "built"));
        let line = crate::trace::LOGGED
            .with(|l| l.borrow().iter().find(|l| l.starts_with("preflight: first, second")).cloned());
        let line = line.expect("traced");
        let mut at = 0;
        for step in [
            "saved",
            "spawned",
            "first output",
            "ran",
            "waited for cargo's lock",
            "first exit 0",
            "second exit 3",
            "exit 1",
        ] {
            let found = line[at..].find(&format!(" · {step} ")).unwrap_or_else(|| panic!("{step}: {line}"));
            at += found + 1;
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #126: a failed step's output one key away — `o` in the task panel opens it whole in a buffer (only
    /// its lines, on its failure), `:preflight-output <step>` any step's; a test step and a step named
    /// `test bundled` don't get mixed up.
    #[test]
    fn a_failed_steps_output_is_one_key_away() {
        let dir = std::env::temp_dir().join(format!("tarae-preflight-output-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ed = Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        let failing = "echo 'running 3 tests'; echo 'test a ... ok'; echo 'test b ... FAILED'; echo 'failures:'; exit 101";
        let steps = vec![
            step("fmt", "echo formatted"),
            step("test", failing),
            step("test bundled", "echo bundled ok"),
        ];
        ed.preflight_run(steps, Set::Quick).unwrap();
        let end = Instant::now() + Duration::from_secs(10);
        while ed.session.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running) {
            assert!(Instant::now() < end, "it ended");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(10)) {
                ed.handle_event(ev);
            }
        }
        // The panel has the keys (a click on it, `C-w j`): `o`
        ed.bottom_focus = true;
        ed.handle_key(crate::key::Key::plain(crate::key::Code::Char('o')));
        let title = ed.doc().title.clone();
        assert_eq!(title.as_deref(), Some("preflight · test"), "{:?}", ed.messages.last());
        let text = ed.doc().text.to_string();
        assert_eq!(text, "running 3 tests\ntest a ... ok\ntest b ... FAILED\nfailures:\n", "its lines alone");
        assert_eq!(ed.cursor_line(), 2, "on its failure");
        assert!(!ed.bottom_focus, "the keys in the buffer");
        // Any step by name
        crate::typed::execute(&mut ed, "preflight-output test bundled").unwrap();
        assert_eq!(ed.doc().title.as_deref(), Some("preflight · test bundled"));
        assert_eq!(ed.doc().text.to_string(), "bundled ok\n");
        assert!(
            crate::typed::execute(&mut ed, "preflight-output lint").unwrap_err().starts_with("no step lint")
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
