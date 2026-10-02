//! `:preflight` — the project's checks before a push, as the project lists them (`.tarae.toml`):
//!
//! ```toml
//! [preflight]
//! steps = [
//!   { name = "fmt", cmd = "cargo fmt --check" },
//!   { name = "perf", cmd = "cargo perf", note = "not measured|SLOWER" },
//! ]
//! ```
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

/// One step as the project lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    pub name: String,
    pub cmd: String,
    /// A line of its output matching this makes a pass one with a note.
    pub note: Option<String>,
}

/// `[preflight] steps = [{ name, cmd, note? }, …]` → the steps (what's wrong said in `warnings`).
pub fn parse_config(v: &toml::Value, file: &str, warnings: &mut Vec<String>) -> Vec<Step> {
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
        if out.iter().any(|s: &Step| s.name == name) {
            warnings.push(format!("{file}: preflight.steps[{i}]: a second step named \"{name}\""));
            continue;
        }
        out.push(Step { name, cmd, note });
    }
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
}

impl Preflight {
    pub fn new(generation: u64, steps: &[Step]) -> Preflight {
        let steps = steps
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
        Preflight { generation, steps, started: Instant::now(), took: None }
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
}

/// `412ms` · `8.3s` · `4m 12s` — a run of minutes reads in minutes.
pub fn long_duration(d: Duration) -> String {
    let s = d.as_secs_f32();
    if s < 60.0 { crate::term::duration(s) } else { format!("{}m {:02}s", s as u64 / 60, s as u64 % 60) }
}

impl Editor {
    /// `:preflight` — the project's steps, run in the task panel. Err: why not now.
    pub fn preflight_start(&mut self) -> Result<(), String> {
        let steps = self.config.preflight.clone();
        if steps.is_empty() {
            let held = self.config_project_held("preflight.");
            return Err(if held {
                "the project's preflight steps wait for it to be trusted — :project-trust".into()
            } else {
                "no preflight steps — add [preflight] steps = [{ name = \"…\", cmd = \"…\" }, …] to .tarae.toml"
                    .into()
            });
        }
        if self.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running) {
            let name = self.task_run.as_ref().map(|r| r.task.name.clone()).unwrap_or_default();
            return Err(format!("{name} is running — let it finish, or stop it (space m c)"));
        }
        self.auto_save(" · before the preflight");
        self.flush_saves();
        let root = self.project_root.clone().unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let task = crate::task::Task { name: "preflight".into(), cmd: script(&steps), cwd: root };
        let generation = self.task_start_preflight(task).ok_or("the preflight didn't start")?;
        self.preflight = Some(Preflight::new(generation, &steps));
        Ok(())
    }

    /// The task run `generation` ended — true if it was the preflight's (then its summary is said).
    pub fn preflight_done(&mut self, generation: u64) -> bool {
        let Some(p) = self.preflight.as_mut().filter(|p| p.generation == generation) else { return false };
        p.finish();
        let (summary, passed) = (p.summary(), p.passed());
        let places = self.task_run.as_ref().map_or(0, |r| r.found.0 + r.found.1);
        let tail = if places > 0 { " — ]q goes to the first" } else { "" };
        if passed {
            self.set_success(format!("preflight: {summary}"));
        } else {
            self.set_error(format!("preflight: {summary}{tail}"));
        }
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
        Step { name: name.into(), cmd: cmd.into(), note: None }
    }

    #[test]
    fn steps_from_the_config() {
        let v: toml::Value = toml::from_str(
            "steps = [{ name = \"fmt\", cmd = \"cargo fmt --check\" }, { name = \"perf\", cmd = \"cargo perf\", \
             note = \"not measured\" }, { name = \"x\" }, { name = \"bad\", cmd = \"y\", note = \"(\" }]",
        )
        .unwrap();
        let mut w = Vec::new();
        let steps = parse_config(&v, "f", &mut w);
        assert_eq!(
            steps,
            [
                step("fmt", "cargo fmt --check"),
                Step { note: Some("not measured".into()), ..step("perf", "cargo perf") }
            ]
        );
        assert_eq!(w.len(), 2, "{w:?}");
        assert!(w[0].contains("steps[2]: needs name") && w[1].contains("not a regex"), "{w:?}");
    }

    #[test]
    fn the_summary_says_what_failed_what_has_notes_and_how_long() {
        let steps =
            [step("fmt", ""), step("clippy", ""), Step { note: Some("busy".into()), ..step("perf", "") }];
        let mut p = Preflight::new(1, &steps);
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
        let mut p = Preflight::new(1, &steps);
        p.line(&format!("{MARK} start 0"));
        p.finish();
        p.took = Some(Duration::from_millis(1500));
        assert_eq!(p.summary(), "0 of 3 passed — fmt failed — stopped, 2 not run · 1.5s");
    }
}
