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
//! project's code, so the first run in a repository shows them on a card that says so — `DEFAULTS_SAY`, once
//! per clone for the steps it listed, kept across starts (`told_key`); a
//! project with a wrapper is told to list it, since the wrapper is the repository's own script); then
//! `.tarae.local.toml`'s steps, yours, added after either (`config.rs` — read only while git ignores it).
//!
//! One task run (`task.rs`, the task panel): an `sh` script runs the steps one after another — every one,
//! whatever the ones before did — each in a subshell with its output and errors as one stream, between
//! marker lines (`MARK` and the run's key — a step's own output can print `MARK`, never the key: V2-1) that tarae
//! reads off as they come: the step's row (name, time, how it ended) in the
//! panel, its output under a `── name ──` heading. Places any step's output points at go to the list
//! (`]q`) — all of them, the run feeds it from its start. A line matching a step's `note` (a regex) makes
//! its pass one with a note (a perf run on a busy machine). At the end a one-line summary: `5 of 6 passed —
//! clippy failed · 4m 12s`. Commands from a project file need the project trusted (`config.rs`, as
//! `restart-build` does).

use std::time::{Duration, Instant};

use crate::editor::Editor;

/// What a step's start and end lines begin with, then the run's key (the script prints them; they're read off, not
/// shown).
const MARK: &str = "::tarae-preflight::";

/// A run's own key, 16 random bytes in hex: its markers carry it (V2-1). Err where the system gives no randomness.
pub fn key() -> Result<String, String> {
    let mut b = [0u8; 16];
    // SAFETY: a buffer of ours and its length (at most getentropy's 256).
    if unsafe { libc::getentropy(b.as_mut_ptr().cast(), b.len()) } != 0 {
        return Err(format!("no key for the preflight's markers: {}", std::io::Error::last_os_error()));
    }
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

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
/// errors as one stream (in order with the markers), all of them; it fails when one did. Its markers carry the
/// run's key, which it reads first from its stdin — never in its words or its environment, which a step can read
/// of the shell (`ps`, `/proc/<pid>/cmdline`, `environ`) — and keeps from the steps: their stdin is `/dev/null`,
/// their shell has no `k`, so a step's output can't say a step ended (V2-1). No key: exit 125 before any step.
pub fn script(steps: &[Step]) -> String {
    let mut s = String::from("IFS= read -r k || exit 125\nexec </dev/null\nfails=0\n");
    for (i, step) in steps.iter().enumerate() {
        s.push_str(&format!(
            "printf '%s\\n' \"{MARK} $k start {i}\"\n( unset k; {}\n) 2>&1\ncode=$?\n\
             printf '%s\\n' \"{MARK} $k end {i} $code\"\n[ $code -eq 0 ] || fails=$((fails + 1))\n",
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
    /// `quick preflight` · `full preflight` · `preflight` (`Set::title`).
    pub title: &'static str,
    /// The steps the quick set left out (the full set's own).
    pub left_out: usize,
    /// The run's key (`key`): a marker without it is a step's own output.
    key: String,
    /// Its run's output, kept once it failed — `:preflight-output` reads it after another run took the task
    /// panel (frictions34 42). (A clone of the run's: its text shared until the run's goes.)
    pub kept: crate::runout::Output,
    /// Its list (panes P3 §2.2, `GroupKind::Preflight`): the step selected, and the first row in sight (as last
    /// drawn).
    pub selected: usize,
    pub list_top: std::cell::Cell<usize>,
}

impl Preflight {
    /// A run of `set` out of `all` the steps, its markers carrying `key`.
    pub fn new(generation: u64, all: &[Step], set: Set, key: String) -> Preflight {
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
        Preflight {
            generation,
            steps,
            started: Instant::now(),
            took: None,
            title,
            left_out,
            key,
            kept: Default::default(),
            selected: 0,
            list_top: Default::default(),
        }
    }

    /// The step running now.
    pub fn running(&self) -> Option<usize> {
        self.steps.iter().position(|s| s.state == State::Running)
    }

    /// Step `k`'s heading in `output` — None: it hasn't printed one (or it's gone from what's kept).
    pub fn heading_at(&self, k: usize, output: &crate::runout::Output) -> Option<usize> {
        let head = format!("── {} ──", self.steps.get(k)?.name);
        output.position(|l| l == head)
    }

    /// The step whose output line `at` of `output` is: the last started whose heading is at or above it.
    pub fn step_at(&self, at: usize, output: &crate::runout::Output) -> Option<usize> {
        let started = |k: &usize| self.steps[*k].state != State::Waiting;
        (0..self.steps.len()).filter(started).rfind(|&k| self.heading_at(k, output).is_some_and(|h| h <= at))
    }

    /// What a marker line of this run's says after its key — None for any other line, a marker with another
    /// key (or none) too: a step's own output, shown as such.
    fn marked<'a>(&self, line: &'a str) -> Option<&'a str> {
        line.strip_prefix(MARK)?.strip_prefix(' ')?.strip_prefix(self.key.as_str())?.strip_prefix(' ')
    }

    /// Whether a line of the run's output is a step's start marker (its output begins after it).
    pub fn starts(&self, line: &str) -> bool {
        self.marked(line).is_some_and(|rest| rest.starts_with("start "))
    }

    /// A line of the run's output: a marker moves a step on (None: not shown — or the heading a step's
    /// output goes under); any other line is the running step's (its note, if it matches).
    pub fn line(&mut self, line: &str) -> Option<String> {
        let Some(rest) = self.marked(line) else {
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
        let mut w = self.marked(line)?.split_whitespace();
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
    pub fn output_of(&self, k: usize, output: &crate::runout::Output) -> Option<Vec<String>> {
        let heading = |s: &StepRun| format!("── {} ──", s.name);
        let head = heading(self.steps.get(k)?);
        let start = output.position(|l| l == head)? + 1;
        let others: Vec<String> =
            self.steps.iter().enumerate().filter(|(i, _)| *i != k).map(|(_, s)| heading(s)).collect();
        let lines = output.iter_from(start).map(|(_, l)| l).take_while(|l| !others.iter().any(|o| o == l));
        Some(lines.map(|l| l.into_owned()).collect())
    }

    /// The step whose output a key asks for: one named (`which`), else the first that failed, else the one
    /// whose output is at line `top` of the panel (None: its end — the last that started).
    pub fn step_for(&self, which: &str, top: Option<usize>, output: &crate::runout::Output) -> Option<usize> {
        let which = which.trim();
        if !which.is_empty() {
            return self.steps.iter().position(|s| s.name == which);
        }
        if let Some(k) = self.steps.iter().position(|s| matches!(s.state, State::Failed(_))) {
            return Some(k);
        }
        let started = |k: &usize| self.steps[*k].state != State::Waiting;
        match top {
            Some(top) => self.step_at(top, output),
            None => (0..self.steps.len()).rfind(started),
        }
    }

    /// The output `which` asks for (`step_for`; `top` the panel's line in view) out of `output`: its buffer's
    /// title (`quick preflight · test`) and lines.
    pub fn step_text(
        &self,
        which: &str,
        top: Option<usize>,
        output: &crate::runout::Output,
    ) -> Result<(String, Vec<String>), String> {
        let k = self.step_for(which, top, output).ok_or_else(|| match which.trim() {
            "" => "no step has printed anything yet".to_string(),
            w => format!(
                "no step {w} — {}",
                self.steps.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
            ),
        })?;
        let name = &self.steps[k].name;
        let lines = self.output_of(k, output).ok_or(format!("{name} hasn't printed anything yet"))?;
        Ok((format!("{} · {name}", self.title), lines))
    }

    /// What its end says, the set named: a quick one passed points at the full one; a failure, at `]q` when
    /// the run put `places` in the list, and the first test that failed (`failure`, in `output`).
    pub fn said(&self, places: usize, output: &crate::runout::Output) -> String {
        let mut out = format!("{}: {}", self.title, self.summary());
        if !self.passed() {
            if let Some(l) = self.step_text("", None, output).ok().map(|(_, l)| l) {
                // (every other test that failed, by name — frictions29 93: only the first was said, and a
                // known flake's name read as the only failure; the count before the first one's why — a toast is
                // cut at its third row)
                match (failure(&l), also_failed(&l).filter(|(n, _)| *n > 1)) {
                    (Some(f), Some((n, others))) => out.push_str(&format!(
                        " — {n} tests failed: {f} — also {}",
                        tests_named(&others, NAMES_ROOM)
                    )),
                    (Some(f), None) => out.push_str(&format!(" — {f}")),
                    (None, _) => {}
                }
            }
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

/// The first test that failed in a step's `lines`, and the first line of why (frictions34 42: 2745 passed, 1
/// failed, and its name was in no toast): `worktree::tests::x: panicked at src/worktree.rs:12:5: msg` — cargo
/// test's `test x ... FAILED` / `---- x stdout ----` and the panic under it (`panicked at …:` with its message
/// on the next line), Go's `--- FAIL: TestX` and its first `file_test.go:12: msg` line. None: no test named.
pub fn failure(lines: &[String]) -> Option<String> {
    let (test, why) = failed_test(lines)?;
    Some(match why {
        Some(w) => format!("{test}: {w}"),
        None => test,
    })
}

/// `failure` as its two parts: the test, and the first line of why when there is one (the bring card names them
/// on lines of their own — frictions29 91).
pub fn failed_test(lines: &[String]) -> Option<(String, Option<String>)> {
    let (test, block) = first_failed(lines)?;
    let rest = &block[1..];
    let why = rest.iter().position(|l| l.contains("panicked at ")).map(|p| {
        let l = rest[p].trim_end();
        let place = &l[l.find("panicked at ").unwrap_or(0)..];
        let message = rest.get(p + 1).map(|m| m.trim()).filter(|m| !m.is_empty());
        match (place.strip_suffix(':'), message) {
            // (`panicked at src/a.rs:3:5:`, the message under it)
            (Some(place), Some(m)) => format!("{place}: {m}"),
            (Some(place), None) => place.to_string(),
            (None, _) => place.to_string(),
        }
    });
    // (Go: the line under `--- FAIL` that says where, `    a_test.go:12: msg`)
    let why = why.or_else(|| {
        let next = rest.first()?.trim();
        next.split_once(':').is_some_and(|(file, _)| file.ends_with(".go")).then(|| next.to_string())
    });
    Some((test, why))
}

/// The test a line names as failed: cargo test's `test x ... FAILED` (with `block`, its `---- x stdout ----` too),
/// Go's `--- FAIL: TestX (0.00s)`.
fn named_failed(l: &str, block: bool) -> Option<String> {
    let l = l.trim();
    l.strip_prefix("test ")
        .and_then(|r| r.strip_suffix(" ... FAILED"))
        .or_else(|| {
            block.then(|| l.strip_prefix("---- ").and_then(|r| r.strip_suffix(" stdout ----"))).flatten()
        })
        .or_else(|| l.strip_prefix("--- FAIL: ").map(|r| r.split(' ').next().unwrap_or(r)))
        .map(str::to_string)
}

/// Every test that failed in `lines`, in the order they're named, each once (frictions29 93). A block's `---- x
/// stdout ----` only when no line said a test failed (`--show-output` prints the passed ones' blocks too). Only the
/// leaves: Go fails a parent whenever a subtest of it fails (`TestA` beside `TestA/sub`) — one failure, not two.
pub fn failed_tests(lines: &[String]) -> Vec<String> {
    let mut tests: Vec<String> = Vec::new();
    for t in lines.iter().filter_map(|l| named_failed(l, false)) {
        if !tests.contains(&t) {
            tests.push(t);
        }
    }
    if tests.is_empty() {
        tests.extend(first_failed(lines).map(|(t, _)| t));
    }
    let parent = |t: &String, all: &[String]| all.iter().any(|o| o.starts_with(&format!("{t}/")));
    let all = tests.clone();
    tests.retain(|t| !parent(t, &all));
    tests
}

/// How many tests failed in `lines` and the ones besides `failure`'s (named apart from it — never by place in
/// `failed_tests`, which reads the lines another way). None: no test named.
pub fn also_failed(lines: &[String]) -> Option<(usize, Vec<String>)> {
    let (first, _) = first_failed(lines)?;
    let others: Vec<String> = failed_tests(lines).into_iter().filter(|t| *t != first).collect();
    Some((others.len() + 1, others))
}

/// What a cut bring card's `v` lists before the first failure's block: every test that failed, a line each, at
/// most `FAILURE_SHOWN` of them, the rest `+N more` (its `y` waits until the view is read to its end). Empty when
/// one failed or none is named.
pub fn failed_listed(lines: &[String]) -> Vec<String> {
    let tests = failed_tests(lines);
    if tests.len() < 2 {
        return Vec::new();
    }
    let mut out: Vec<String> = tests.iter().take(FAILURE_SHOWN).cloned().collect();
    if tests.len() > FAILURE_SHOWN {
        out.push(format!("+{} more — v again shows the whole output", tests.len() - FAILURE_SHOWN));
    }
    out
}

/// About this many characters of test names on a toast's or a card's line (`tests_named`).
pub const NAMES_ROOM: usize = 100;

/// A test's name past this many characters loses its leading modules (`short_name`).
const NAME_SHOWN: usize = 48;

/// `t` as a list names it: past `NAME_SHOWN` characters its leading `mod::` parts go (`…::` says so) — the end
/// is what tells tests apart, and a name longer than a card's line breaks mid-word (frictions38's shots).
fn short_name(t: &str) -> String {
    let mut s = t;
    while s.chars().count() > NAME_SHOWN
        && let Some((_, rest)) = s.split_once("::")
    {
        s = rest;
    }
    match s.len() < t.len() {
        true => format!("…::{s}"),
        false => s.to_string(),
    }
}

/// `tests` by name (`short_name`), `, ` between, as many as fit in `room` characters (one at least), the rest
/// `+N more`.
pub fn tests_named(tests: &[String], room: usize) -> String {
    let mut out = String::new();
    let mut shown = 0;
    for t in tests.iter().map(|t| short_name(t)) {
        if shown > 0 && out.chars().count() + 2 + t.chars().count() > room {
            break;
        }
        if shown > 0 {
            out.push_str(", ");
        }
        out.push_str(&t);
        shown += 1;
    }
    if shown < tests.len() {
        out.push_str(&format!(" +{} more", tests.len() - shown));
    }
    out
}

/// The first test that failed in `lines` (`failure`) and its block: the line naming it (its own `---- x stdout
/// ----` when there is one — cargo lists the failures after every test's line), then its lines up to the next.
fn first_failed(lines: &[String]) -> Option<(String, &[String])> {
    let named = |l: &str| named_failed(l, true);
    let mut test = lines.iter().find_map(|l| named(l))?;
    // (Go: a parent's subtest that failed, its leaf — the parent's line says nothing of why)
    while let Some(sub) = lines.iter().filter_map(|l| named(l)).find(|n| n.starts_with(&format!("{test}/"))) {
        test = sub;
    }
    let block = format!("---- {test} stdout ----");
    let from = lines
        .iter()
        .position(|l| l.trim() == block)
        .or_else(|| lines.iter().position(|l| named(l).as_deref() == Some(test.as_str())))?;
    let rest = &lines[from + 1..];
    let end = rest.iter().position(|l| l.starts_with("---- ") || l.trim_start().starts_with("--- FAIL"));
    let end = from + 1 + end.unwrap_or(rest.len());
    Some((test, &lines[from..end]))
}

/// At most this many lines of the first failure's block on a cut card's view (`failure_block`).
pub const FAILURE_SHOWN: usize = 20;

/// What a cut bring card's `v` shows of a failed step's `lines` (the review of frictions34 42: its `y` waits until
/// that view is read to its end, and the whole of a `cargo test` is thousands of lines): the first failure's block,
/// its first `FAILURE_SHOWN` lines — none found, the output's last ones.
pub fn failure_block(lines: &[String]) -> Vec<String> {
    match first_failed(lines) {
        Some((_, block)) => block.iter().take(FAILURE_SHOWN).cloned().collect(),
        None => lines[lines.len().saturating_sub(FAILURE_SHOWN)..].to_vec(),
    }
}

/// `412ms` · `8.3s` · `4m 12s` — a run of minutes reads in minutes.
pub fn long_duration(d: Duration) -> String {
    let s = d.as_secs_f32();
    if s < 60.0 { crate::term::duration(s) } else { format!("{}m {:02}s", s as u64 / 60, s as u64 % 60) }
}

/// Where the packs' default preflights told on their card are kept (`$state/preflight-told`, a key a line —
/// `told_key`) so the card shows once until the steps change, across starts and checkouts (cardburden 5: it
/// asked again after every `:restart` and in every new session).
pub fn kept_path() -> Option<std::path::PathBuf> {
    crate::config::state_dir().map(|d| d.join("preflight-told"))
}

/// What a told preflight is kept by: its repository — git's common folder, one for all its checkouts (else the
/// project's real path) — and that clone of it (`identity`: the review — a repository removed and another
/// cloned at its path, a fork with the same root commit too, had the same key and ran with no card), and each
/// step the card listed, name and command — as one SHA-256 (`trust::hash`): no path or command in the file, and
/// a step changed or added is a card again. None where the clone can't be told apart: nothing is kept.
pub(crate) fn told_key(root: &std::path::Path, steps: &[Step]) -> Option<String> {
    let repo = crate::allow::common_dir(root)
        .or_else(|| std::fs::canonicalize(root).ok())
        .unwrap_or_else(|| root.to_path_buf());
    let items: Vec<(String, String)> =
        [("repository".to_string(), repo.display().to_string()), ("clone".to_string(), identity(&repo)?)]
            .into_iter()
            .chain(steps.iter().map(|s| (s.name.clone(), s.cmd.clone())))
            .collect();
    Some(crate::trust::hash(&items).iter().map(|b| format!("{b:02x}")).collect())
}

/// The folder `dir` as the file system tells this one from another made at its path: its device and inode,
/// and when it was made (`Metadata::created` — macOS's `st_birthtime`, Linux's statx `btime`). None where
/// its making time can't be read (a file system without it): then nothing is kept, a card each start.
fn identity(dir: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let m = std::fs::metadata(dir).ok()?;
    let born = m.created().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(format!("{}:{}:{}", m.dev(), m.ino(), born.as_nanos()))
}

/// Whether the card a `:preflight` of `set` over `steps` in `root` would show was answered `y` before — for
/// exactly the steps it lists (`set.of`: the review — a `y` on the quick card kept the `full = true` ones it
/// never showed). None: nothing to go by (nothing kept here, or no key — `told_key`). The worker's lookup, and
/// the tests' entry's.
fn told_before(
    kept: Option<&std::path::Path>,
    root: &std::path::Path,
    steps: &[Step],
    set: Set,
) -> Option<bool> {
    let kept = kept?;
    let key = told_key(root, &set.of(steps))?;
    Some(told_in(kept, &key))
}

/// The lines kept in `kept` for the packs' default preflight in `root` over `steps` (theirs and yours) — either
/// set's card's (`:granted`). A worker's.
pub(crate) fn told_kept(kept: &std::path::Path, root: &std::path::Path, steps: &[Step]) -> Vec<String> {
    let mut keys: Vec<String> = [Set::Quick, Set::Full]
        .iter()
        .filter_map(|s| told_key(root, &s.of(steps)))
        .filter(|k| told_in(kept, k))
        .collect();
    keys.dedup();
    keys
}

/// `keys` taken out of `kept` (`:granted`'s `d`, on a worker): the card shows again before the next run.
pub(crate) fn told_forget(kept: &std::path::Path, keys: &[String]) -> std::io::Result<()> {
    let Ok(text) = std::fs::read_to_string(kept) else { return Ok(()) };
    let left: String =
        text.lines().filter(|l| !keys.iter().any(|k| l.trim() == k)).map(|l| format!("{l}\n")).collect();
    crate::disk::write_atomic(kept, |w| std::io::Write::write_all(w, left.as_bytes()))
}

/// Whether `key` is kept in `path`.
fn told_in(path: &std::path::Path, key: &str) -> bool {
    std::fs::read_to_string(path).is_ok_and(|t| t.lines().any(|l| l.trim() == key))
}

/// Keeps `key` in `path` — appending, so two tareas answering at once both keep theirs, and a file that can't
/// be read isn't written over.
fn told_add(path: &std::path::Path, key: &str) -> std::io::Result<()> {
    use std::io::Write;
    if told_in(path, key) {
        return Ok(());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // (after a last line with no newline)
    let ends_open = std::fs::read(path).is_ok_and(|b| b.last().is_some_and(|&c| c != b'\n'));
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    writeln!(f, "{}{key}", if ends_open { "\n" } else { "" })
}

impl Editor {
    /// `o` in the task panel · `:preflight-output [step]` — a step's whole output (as kept) in a buffer of its
    /// own, to read and search: the step named, else the first that failed, else the one in view (#126: a
    /// failed step's output was hard to reach — the panel is a few rows, and the `▲ test` that was clicked
    /// was the test panel's tab).
    pub fn preflight_output(&mut self, which: &str) -> Result<(), String> {
        let p = self.session.preflight.as_ref();
        let run =
            self.session.task_run.as_ref().filter(|r| p.is_some_and(|p| p.generation == r.generation()));
        let (title, lines) = match (p, run) {
            (Some(p), Some(run)) => p.step_text(which, run.scroll, &run.output)?,
            // (frictions34 42: a failed one's, kept after another run took the panel)
            (Some(p), None) if !p.kept.is_empty() => p.step_text(which, None, &p.kept)?,
            _ if self.session.task_run.is_some() => {
                return Err("the task panel's run isn't a preflight".into());
            }
            _ => return Err("no preflight in the task panel — :preflight runs one".into()),
        };
        self.output_buffer(&title, &lines);
        Ok(())
    }

    /// `lines` in a buffer of its own titled `title` (an earlier one of that title gives way), at the failure
    /// when it says where — the last `error`/`FAILED`/`panicked` line.
    pub(crate) fn output_buffer(&mut self, title: &str, lines: &[String]) {
        let mut text = lines.join("\n");
        text.push('\n');
        // One buffer per step: an earlier one gives way to this output
        let old: Vec<_> = self
            .docs
            .iter()
            .filter(|d| d.throwaway && d.title.as_deref() == Some(title))
            .map(|d| d.id)
            .collect();
        for id in old {
            self.close_doc(id);
        }
        self.open_virtual(title, &text);
        self.session.leave(crate::session::At::Slot);
        let at = lines
            .iter()
            .rposition(|l| l.contains("FAILED") || l.contains("panicked") || l.starts_with("error"));
        if let Some(line) = at {
            let doc = self.doc_mut();
            let at = crate::movement::line_start(&doc.text, line);
            doc.set_selection(crate::selection::Selection::point(at));
            self.align_view(crate::viewalign::Place::Center);
        }
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
        self.preflight_start_with(set, always, crate::packs::preflight)
    }

    /// `preflight_start_asking`, the packs' defaults for the project looked for by `defaults` on the worker
    /// (`packs::preflight`; a test's stand-in steps — the rest of the path is the real one).
    pub(crate) fn preflight_start_with(
        &mut self,
        set: Set,
        always: bool,
        defaults: fn(&std::path::Path) -> crate::packs::Defaults,
    ) -> Result<(), String> {
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
        let kept = self.session.preflight_kept.clone();
        self.events.jobs().spawn(move || {
            let packs = defaults(&root);
            // (the card for these very steps answered before in this clone, at any start of tarae's)
            let all = [&packs.steps[..], &local[..]].concat();
            let told = told_before(kept.as_deref(), &root, &all, set);
            move |ed: &mut Editor| match packs.held.first() {
                // (a kind of project the packs know, but it runs its checks its own way: say how)
                Some(why) if packs.steps.is_empty() && local.is_empty() => {
                    ed.session.bring_after_preflight = false;
                    ed.set_error(format!("no preflight steps — {why}"));
                }
                _ => ed.preflight_found_asking(root, packs.steps, local, set, always, told),
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
    /// the first time in a project, a card says what will run, and that it runs the project's code. (What's
    /// kept is read here, by the worker's `told_before`.)
    #[cfg(test)]
    pub(crate) fn preflight_found(
        &mut self,
        root: std::path::PathBuf,
        packs: Vec<Step>,
        local: Vec<Step>,
        set: Set,
    ) {
        let all = [&packs[..], &local[..]].concat();
        let told = told_before(self.session.preflight_kept.as_deref(), &root, &all, set);
        self.preflight_found_asking(root, packs, local, set, false, told);
    }

    /// The packs' default steps for `root` came (none: not a kind of project they know), with yours after:
    /// the first time, a card says what will run, and that it runs the project's code — once per clone until
    /// the steps it lists change (`told`: so kept, read on the worker — None: nothing to go by, then once per
    /// project since tarae started, for a card that listed every step). `always`: on the card even when told
    /// already, or with no defaults among them.
    fn preflight_found_asking(
        &mut self,
        root: std::path::PathBuf,
        packs: Vec<Step>,
        local: Vec<Step>,
        set: Set,
        always: bool,
        told: Option<bool>,
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
        let told = told.unwrap_or_else(|| self.preflight_told.contains(&root));
        if always || (defaults && !told) {
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

    /// `y` on that card: run, and no card again for the steps it listed in this clone — kept on a worker
    /// (nothing kept here, or no key: for this project until quit, when it listed every step).
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
        let listed = set.of(&steps);
        if let Some(kept) = self.session.preflight_kept.clone() {
            let (root, listed) = (root.clone(), listed.clone());
            self.events.jobs().background(move || {
                let Some(key) = told_key(&root, &listed) else { return };
                if let Err(e) = told_add(&kept, &key) {
                    crate::log::line("preflight", "told not kept", &format!("{}: {e}", kept.display()));
                }
            });
        }
        // (since tarae started, what goes by where nothing is kept: only a card that listed every step — one
        // that left the `full = true` ones out says nothing of them)
        if listed.len() == steps.len() {
            self.preflight_told.insert(root);
        }
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
    pub(crate) fn preflight_run(&mut self, all: Vec<Step>, set: Set) -> Result<(), String> {
        self.preflight_free()?;
        let steps = set.of(&all);
        if steps.is_empty() {
            return Err("every preflight step is marked full = true — :preflight-full runs them".into());
        }
        // (it runs what's there outside the sandbox: what Claude's runs made that the sandbox keeps, looked at
        // first — G23)
        if !self.session.made_protected.is_empty() {
            crate::tools::ask_made(self, crate::tools::MadeThen::Preflight { all, set });
            return Ok(());
        }
        // (its trace named for the set, and the steps it runs)
        let names: Vec<&str> = steps.iter().map(|s| s.name.as_str()).collect();
        let title = set.title(&all);
        let trace = crate::trace::RunTrace::new(format!("{title}: {}", names.join(", ")));
        let key = key()?;
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
                    ed.task_start_preflight(task, trace, &key)
                        .ok_or_else(|| "the preflight didn't start".to_string())
                }
            };
            match started {
                Ok(generation) => ed.session.preflight = Some(Preflight::new(generation, &all, set, key)),
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
        let run = self.session.task_run.as_ref().filter(|r| r.generation() == generation);
        let places = run.map_or(0, |r| r.found.0 + r.found.1);
        let output = run.map(|r| r.output.clone()).unwrap_or_default();
        let (said, passed, title) = (p.said(places, &output), p.passed(), p.title);
        // (a failed one's output outlives the panel's next run — frictions34 42)
        if !passed {
            p.kept = output;
        }
        if passed {
            self.set_success(said);
        } else {
            // (what of tarae's environment its steps got that changes their tools: a test that fails on one
            // here passes in CI — frictions29 26)
            let names: Vec<String> = match &self.claude_env {
                Some(env) => env.keys().cloned().collect(),
                None => std::env::vars_os().filter_map(|(k, _)| k.into_string().ok()).collect(),
            };
            let set = tools_vars(names, &crate::sandboxpack::get().pass);
            self.set_error(match set.is_empty() {
                true => said,
                false => format!(
                    "{said} · your environment sets {} — a step reading one can fail here and pass in CI",
                    listed(&set)
                ),
            });
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

/// The names in `names` that the packs say their tools read (`[sandbox] pass`: `PYTHON*`, `RUST*`, `CARGO_*` …,
/// a `*` at the end matching any rest), sorted — the variables of tarae's environment a preflight's steps get
/// that change what the project's tools do (frictions29 26: `PYTHONDONTWRITEBYTECODE=1` since a reboot failed
/// a test, and the failure looked like the session's). Names only, never values.
fn tools_vars(names: impl IntoIterator<Item = String>, pass: &[String]) -> Vec<String> {
    let read = |n: &str| {
        pass.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => n.starts_with(prefix),
            None => n == p,
        })
    };
    let mut out: Vec<String> = names.into_iter().filter(|n| read(n)).collect();
    crate::sorting::sort(&mut out);
    out.dedup();
    out
}

/// At most this many names said — the rest counted.
const NAMES_SAID: usize = 4;

/// `a, b, c and 2 more`.
fn listed(names: &[String]) -> String {
    match names.len() > NAMES_SAID {
        true => format!("{} and {} more", names[..NAMES_SAID].join(", "), names.len() - NAMES_SAID),
        false => names.join(", "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(name: &str, cmd: &str) -> Step {
        Step { name: name.into(), cmd: cmd.into(), note: None, from: ".tarae.toml".into(), full: false }
    }

    /// The key these tests' runs carry, and what their markers start with (`super::MARK` and the key).
    const KEY: &str = "k3y";
    const MARK: &str = "::tarae-preflight:: k3y";

    /// V2-1: a step's output can print a marker — `MARK`, a key that isn't the run's, none — and it moves no
    /// step: one that failed stays failed after a later step says it ended well, and the line is shown as
    /// output.
    #[test]
    fn a_marker_a_step_prints_moves_no_step() {
        assert!(MARK.starts_with(super::MARK) && MARK.ends_with(KEY));
        let mut p = Preflight::new(1, &[step("clippy", ""), step("test", "")], Set::Full, KEY.into());
        for l in [
            format!("{MARK} start 0"),
            "error: x".into(),
            format!("{MARK} end 0 1"),
            format!("{MARK} start 1"),
        ] {
            p.line(&l);
        }
        for forged in [
            format!("{} start 0", super::MARK),
            format!("{} end 0 0", super::MARK),
            format!("{} 0000 end 0 0", super::MARK),
            format!("{}  end 0 0", super::MARK),
            format!("{MARK}x end 0 0"),
        ] {
            assert_eq!(p.line(&forged).as_deref(), Some(forged.as_str()), "shown as output");
            assert!(p.ending(&forged).is_none() && !p.starts(&forged), "{forged}");
        }
        p.line(&format!("{MARK} end 1 0"));
        p.finish();
        assert_eq!(p.steps[0].state, State::Failed(1), "clippy still failed");
        assert_eq!(p.steps[1].state, State::Passed);
    }

    /// The script as `sh` runs it, its key on its stdin: a step can't print the run's markers — its shell has no
    /// `k`, its stdin is `/dev/null`, so its forged end of a failed step moves nothing — and with no key the
    /// script runs no step at all.
    #[test]
    fn the_scripts_steps_never_have_its_key() {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let forge = "printf '%s\\n' \"::tarae-preflight:: $k end 0 0\" '::tarae-preflight:: end 0 0'; \
                     echo \"stdin: $(cat)\"";
        let steps = [step("fails", "false"), step("forges", forge)];
        let run = |input: &str| {
            let mut c = Command::new("sh")
                .args(["-c", &script(&steps)])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .spawn()
                .unwrap();
            c.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
            c.wait_with_output().unwrap()
        };
        let out = run(&format!("{KEY}\n"));
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        let mut p = Preflight::new(1, &steps, Set::Full, KEY.into());
        let shown: Vec<String> = text.lines().filter_map(|l| p.line(l)).collect();
        p.finish();
        assert_eq!((&p.steps[0].state, &p.steps[1].state), (&State::Failed(1), &State::Passed), "{text}");
        assert!(shown.contains(&"stdin: ".to_string()), "its stdin is /dev/null: {shown:?}");
        assert!(shown.iter().all(|l| !l.contains(KEY)), "no step had the key: {shown:?}");
        assert!(!out.status.success(), "a step failed");
        let none = run("");
        assert_eq!((none.status.code(), none.stdout.len()), (Some(125), 0), "no key, no step");
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
            let mut p = Preflight::new(1, all, set, KEY.into());
            for (i, c) in codes.iter().enumerate() {
                p.line(&format!("{MARK} start {i}"));
                p.line(&format!("{MARK} end {i} {c}"));
            }
            p.finish();
            p.took = Some(Duration::from_secs(62));
            p
        };
        assert_eq!(
            ran(Set::Quick, &all, &[0, 0]).said(0, &Default::default()),
            "quick preflight: all 2 passed · 1m 02s — the full set (2 more steps) runs before publish: \
             :preflight-full"
        );
        assert_eq!(
            ran(Set::Quick, &all, &[0, 1]).said(3, &Default::default()),
            "quick preflight: 1 of 2 passed — test failed · 1m 02s — ]q goes to the first"
        );
        assert_eq!(
            ran(Set::Full, &all, &[0, 0, 0, 0]).said(0, &Default::default()),
            "full preflight: all 4 passed · 1m 02s"
        );
        let one = [step("fmt", "")];
        assert_eq!(
            ran(Set::Quick, &one, &[0]).said(0, &Default::default()),
            "preflight: all 1 passed · 1m 02s"
        );
        let p = Preflight::new(1, &all[..3], Set::Quick, KEY.into());
        assert_eq!((p.steps.len(), p.left_out), (2, 1));
        // (frictions34 42: the first test that failed, from the failed step's output)
        let output: crate::runout::Output = [
            "── fmt ──",
            "── test ──",
            "test a ... FAILED",
            "---- a stdout ----",
            "thread 'a' panicked at x.rs:1:2:",
            "no",
        ]
        .map(|l| (false, l))
        .into_iter()
        .collect();
        assert_eq!(
            ran(Set::Quick, &all, &[0, 1]).said(0, &output),
            "quick preflight: 1 of 2 passed — test failed · 1m 02s — a: panicked at x.rs:1:2: no"
        );
        // (frictions29 93: every other test that failed, by name, the rest `+N more`)
        let many: crate::runout::Output =
            ["── fmt ──", "── test ──", "test a ... FAILED", "test b ... FAILED", "test c ... FAILED"]
                .map(|l| (false, l))
                .into_iter()
                .collect();
        assert_eq!(
            ran(Set::Quick, &all, &[0, 1]).said(0, &many),
            "quick preflight: 1 of 2 passed — test failed · 1m 02s — 3 tests failed: a — also b, c"
        );
    }

    /// frictions34 42: the first test that failed is named with the first line of why — cargo test's (its own
    /// block's panic, not the next test's; the message on the line under `panicked at …:`, or the old form on
    /// one line), Go's (`--- FAIL` and the place under it); a failure that names no test: None.
    #[test]
    fn the_first_failure_is_named_with_its_panic() {
        let lines = |s: &str| s.lines().map(String::from).collect::<Vec<_>>();
        let cargo = lines(
            "running 3 tests\ntest worktree::tests::b ... FAILED\ntest worktree::tests::a ... FAILED\n\
             test c ... ok\n\nfailures:\n\n---- worktree::tests::a stdout ----\n\
             thread 'worktree::tests::a' (1) panicked at src/worktree.rs:9:5:\nnot this one\n\n\
             ---- worktree::tests::b stdout ----\nsome output\n\
             thread 'worktree::tests::b' (2) panicked at src/worktree.rs:3920:9:\n\
             assertion failed: card(&ed).is_some()\nnote: run with `RUST_BACKTRACE=1`\n",
        );
        assert_eq!(
            failure(&cargo).as_deref(),
            Some(
                "worktree::tests::b: panicked at src/worktree.rs:3920:9: assertion failed: card(&ed).is_some()"
            )
        );
        let old =
            lines("test a ... FAILED\n---- a stdout ----\nthread 'a' panicked at 'boom', src/a.rs:3:5\n");
        assert_eq!(failure(&old).as_deref(), Some("a: panicked at 'boom', src/a.rs:3:5"));
        let timed_out = lines(
            "test a ... FAILED\n---- a stdout ----\n---- b stdout ----\nthread 'b' panicked at b.rs:1:1:\nx",
        );
        assert_eq!(failure(&timed_out).as_deref(), Some("a"), "not b's panic");
        let go = lines(
            "=== RUN   TestParse\n--- FAIL: TestParse (0.00s)\n    parse_test.go:12: got 3, want 4\nFAIL\n",
        );
        assert_eq!(failure(&go).as_deref(), Some("TestParse: parse_test.go:12: got 3, want 4"));
        assert_eq!(failure(&lines("error[E0308]: mismatched types\n --> src/a.rs:1:1")), None);
        // (a cut card's view: that failure's block, at most `FAILURE_SHOWN` lines; none found, the end)
        let block = failure_block(&cargo);
        assert_eq!(block[0], "---- worktree::tests::b stdout ----");
        assert!(
            block.iter().any(|l| l.contains("assertion failed"))
                && !block.iter().any(|l| l.contains("not this one"))
        );
        let long: Vec<String> = (0..100)
            .map(|i| format!("line {i}"))
            .chain(["---- t stdout ----".into()])
            .chain((0..50).map(|i| format!("t {i}")))
            .collect();
        assert_eq!(failure_block(&long).len(), FAILURE_SHOWN);
        let none = lines("a\nb\nc");
        assert_eq!(failure_block(&none), none);
    }

    /// frictions29 93: every test that failed, in the order named, once each (cargo's `test x ... FAILED` and its
    /// block, Go's `--- FAIL`); named as many as fit, the rest `+N more`.
    #[test]
    fn every_failing_test_is_listed_and_named_as_room_allows() {
        let lines = |s: &str| s.lines().map(String::from).collect::<Vec<_>>();
        let cargo = lines(
            "test b ... FAILED\ntest a ... FAILED\ntest c ... ok\nfailures:\n---- a stdout ----\n---- b stdout ----\n\
             successes:\n---- c stdout ----\n",
        );
        assert_eq!(failed_tests(&cargo), ["b", "a"]);
        assert_eq!(failed_tests(&lines("---- only stdout ----\nx")), ["only"]);
        // (Go: a subtest's failure fails its parent too — one failure, the leaf, named with its why)
        let go = lines(
            "--- FAIL: TestA (0.00s)\n    --- FAIL: TestA/sub (0.00s)\n        a_test.go:3: no\n\
             --- FAIL: TestB (0.01s)\n",
        );
        assert_eq!(failed_tests(&go), ["TestA/sub", "TestB"]);
        assert_eq!(failure(&go).as_deref(), Some("TestA/sub: a_test.go:3: no"));
        assert_eq!(also_failed(&go), Some((2, vec!["TestB".to_string()])));
        assert!(failed_tests(&lines("error: x")).is_empty());
        assert_eq!(also_failed(&lines("error: x")), None);
        // (the ones besides `failure`'s by name, wherever it was found: here a block ahead of the FAILED lines)
        let block_first = lines("---- b stdout ----\npanicked\ntest a ... FAILED\ntest b ... FAILED\n");
        assert_eq!(failure(&block_first).as_deref(), Some("b"));
        assert_eq!(also_failed(&block_first), Some((2, vec!["a".to_string()])));
        // (a cut card's `v`: a line each, `FAILURE_SHOWN` at most, the rest counted; one failure: none)
        let many: Vec<String> = (0..50).map(|i| format!("test t{i} ... FAILED")).collect();
        let listed = failed_listed(&many);
        assert_eq!((listed.len(), listed[0].as_str()), (FAILURE_SHOWN + 1, "t0"));
        assert_eq!(listed[FAILURE_SHOWN], "+30 more — v again shows the whole output");
        assert!(failed_listed(&many[..1]).is_empty());
        let names: Vec<String> = ["aaaa", "bbbb", "cccc", "dddd"].map(String::from).to_vec();
        assert_eq!(tests_named(&names, 100), "aaaa, bbbb, cccc, dddd");
        assert_eq!(tests_named(&names, 10), "aaaa, bbbb +2 more");
        assert_eq!(tests_named(&names, 1), "aaaa +3 more", "one at least");
        // (a long name loses its leading modules, never its end)
        let long = ["worktree::tests::a_new_sessions_bring_card_runs_the_projects_quick_set".to_string()];
        assert_eq!(tests_named(&long, 100), "…::a_new_sessions_bring_card_runs_the_projects_quick_set");
        let mid = ["crate::sim::quotes::quotes_go_with_the_message_once_x".to_string()];
        assert_eq!(tests_named(&mid, 100), "…::sim::quotes::quotes_go_with_the_message_once_x");
    }

    /// Whatever a step prints, its lines are read without a panic — a marker naming no step, one past the last,
    /// a code that's no number, a marker cut short — and one that moves no step is shown as any line. The
    /// script's markers and a step's words mutated, 20,000 lines through a run of three steps.
    #[test]
    fn a_steps_output_is_read_without_a_panic() {
        use crate::mutate::{self, Rng};
        let all = [step("fmt", ""), step("test", ""), step("clippy", "")];
        let mut p = Preflight::new(1, &all, Set::Full, KEY.into());
        let seeds = [
            format!("{MARK} start 0"),
            format!("{MARK} end 1 0"),
            format!("{MARK} end 2 101"),
            format!("{MARK} start 99"),
            format!("{MARK} end 0 x"),
            "error: could not compile `p` (lib) due to 1 previous error".into(),
        ];
        let words = [MARK, " start ", " end ", "18446744073709551616", "-1", " "];
        let mut rng = Rng(0x5eed_0add_7e55_0110);
        for _ in 0..20_000 {
            let mut l = rng.pick(&seeds).clone();
            mutate::text(&mut rng, &mut l, &words);
            let named = l.strip_prefix(MARK).and_then(|r| r.split_whitespace().nth(1)?.parse::<usize>().ok());
            let shown = p.line(&l);
            let _ = p.ending(&l);
            if named.is_none_or(|i| i >= all.len()) {
                assert_eq!(shown.as_deref(), Some(l.as_str()), "moves no step");
            }
        }
        p.finish();
        let _ = p.summary();
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

    /// CI's network job runs the tests that need the network by name, and those are the ones marked so
    /// (`#[ignore = "network: …"]`): `--ignored` alone also holds the children run in a process of their own,
    /// measurements and checks by hand on a VM, none of them CI's.
    #[test]
    fn cis_network_job_runs_every_network_test_and_only_those() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let ci = std::fs::read_to_string(root.join(".github/workflows/ci.yml")).unwrap();
        let line = ci.lines().find(|l| l.contains("-- --ignored")).expect("the network job");
        let named: std::collections::BTreeSet<String> = line
            .split(" --exact ")
            .nth(1)
            .expect("its tests named, `--exact`")
            .split_whitespace()
            .map(String::from)
            .collect();
        // (the marked ones: a test fn below the mark, by name)
        let mut marked = std::collections::BTreeSet::new();
        let mut dirs = vec![root.join("src")];
        while let Some(d) = dirs.pop() {
            for e in std::fs::read_dir(&d).unwrap() {
                let p = e.unwrap().path();
                if p.is_dir() {
                    dirs.push(p);
                    continue;
                }
                if p.extension().is_none_or(|x| x != "rs") {
                    continue;
                }
                let text = std::fs::read_to_string(&p).unwrap();
                let lines: Vec<&str> = text.lines().collect();
                for (i, l) in lines.iter().enumerate() {
                    if !l.trim().starts_with("#[ignore = \"network: ") {
                        continue;
                    }
                    let f = lines[i..].iter().find_map(|l| l.trim().strip_prefix("fn ")).expect("a fn below");
                    marked.insert(f.split('(').next().unwrap().to_string());
                }
            }
        }
        let src = root.join("src");
        for n in &named {
            assert!(crate::sandbox::golden::test_exists(&src, n), "CI names {n}: no such test");
        }
        let names: std::collections::BTreeSet<String> =
            named.iter().map(|n| n.rsplit("::").next().unwrap().to_string()).collect();
        assert_eq!(names, marked, "CI's network job and the tests marked `network:`");
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

    /// Stand-in pack defaults for `preflight_start_with`: one step that `echo`es.
    fn pack_a(_: &std::path::Path) -> crate::packs::Defaults {
        crate::packs::Defaults {
            steps: vec![Step { from: "rust pack".into(), ..step("fmt", "echo a") }],
            held: vec![],
        }
    }

    /// …and the same step changed.
    fn pack_b(_: &std::path::Path) -> crate::packs::Defaults {
        crate::packs::Defaults {
            steps: vec![Step { from: "rust pack".into(), ..step("fmt", "echo b") }],
            held: vec![],
        }
    }

    /// cardburden 5: the defaults' card asked again after every `:restart` and in every new session of the same
    /// repository. A `y` is kept by repository (git's common folder: a linked checkout is the same one), by that
    /// clone of it, and by the steps the card listed — the next tarae, in another checkout, runs them with no
    /// card. The review: a step changed, another repository, the repository removed and cloned again at its path
    /// (a fork's root commit is the same: the folder's device, inode and making time aren't), or the full set's
    /// steps the quick card never listed — each a card again. Driven through `:preflight`'s own start and
    /// worker, the packs' lookup the only stand-in. (Stand-in git folders, no git run; the steps `echo`.)
    #[test]
    fn the_defaults_card_shows_once_per_clone_for_the_steps_it_listed() {
        use crate::editor::Editor;
        let scratch = crate::scratch::Scratch::new("preflight-told");
        let dir = scratch.to_path_buf();
        // (a file system that can't tell when a folder was made keeps nothing: the card each start, as before)
        if identity(&dir).is_none() {
            return;
        }
        let (repo, linked, other) = (dir.join("repo"), dir.join("linked"), dir.join("other"));
        let meta = repo.join(".git/worktrees/linked");
        for d in [&meta, &linked, &other.join(".git")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(linked.join(".git"), format!("gitdir: {}\n", meta.display())).unwrap();
        std::fs::write(meta.join("gitdir"), format!("{}\n", linked.join(".git").display())).unwrap();
        std::fs::write(meta.join("commondir"), "../..\n").unwrap();
        let kept = dir.join("state/preflight-told");
        let soak = Step { from: ".tarae.local.toml".into(), full: true, ..step("soak", "echo soak") };
        let tarae = |root: &std::path::Path| {
            let mut ed = Editor::new(crate::config::Config::default());
            (ed.session.root, ed.session.preflight_kept) = (root.to_path_buf(), Some(kept.clone()));
            ed.config.preflight_local = vec![soak.clone()];
            ed
        };
        let carded =
            |ed: &Editor| ed.offers.iter().any(|o| matches!(o.what, crate::offer::What::Preflight { .. }));
        let pump = |ed: &mut Editor, what: &str, done: &dyn Fn(&Editor) -> bool| {
            let end = Instant::now() + Duration::from_secs(5);
            while !done(ed) {
                assert!(Instant::now() < end, "{what}");
                if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
                    ed.handle_event(ev);
                }
            }
        };
        // `:preflight` as typed: its card, or its run (stopped: the test's `echo`)
        let asks = |ed: &mut Editor, set: Set, packs: fn(&std::path::Path) -> crate::packs::Defaults| {
            ed.preflight_start_with(set, false, packs).unwrap();
            pump(ed, "a card or a run", &|ed| carded(ed) || ed.session.task_run.is_some());
            let card = carded(ed);
            if !card {
                ed.session.task_run = None;
            }
            card
        };
        let lines = || std::fs::read_to_string(&kept).map_or(0, |t| t.lines().count());
        let yes = |ed: &mut Editor, n: usize| {
            ed.offer_accept();
            pump(ed, "kept after y, and run", &|ed| lines() == n && ed.session.task_run.is_some());
            ed.session.task_run = None;
        };
        let mut ed = tarae(&repo);
        assert!(asks(&mut ed, Set::Quick, pack_a), "the first time: its card");
        let listed = ed.offers[0].question.clone();
        assert!(listed.contains("echo a") && !listed.contains("soak"), "the quick set: {listed}");
        yes(&mut ed, 1);
        let text = std::fs::read_to_string(&kept).unwrap();
        assert!(
            !text.contains("echo") && !text.contains(&*dir.to_string_lossy()),
            "no command or path: {text}"
        );
        // A tarae started again, in the linked checkout: no card
        let mut ed = tarae(&linked);
        assert!(!asks(&mut ed, Set::Quick, pack_a), "told in this repository");
        // `:preflight-full`: the step the quick card never listed — its own card, then told too
        assert!(asks(&mut ed, Set::Full, pack_a), "the full set's steps weren't on the quick card");
        yes(&mut ed, 2);
        assert!(!asks(&mut tarae(&repo), Set::Full, pack_a));
        // A step changed, or another repository: asked again
        assert!(asks(&mut tarae(&linked), Set::Quick, pack_b), "the steps changed");
        assert!(asks(&mut tarae(&other), Set::Quick, pack_a), "another repository");
        // Removed and cloned again at its path (its common folder made anew): asked again
        std::fs::remove_dir_all(&repo).unwrap();
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        assert!(asks(&mut tarae(&repo), Set::Quick, pack_a), "another clone at the same path");
        // A folder that isn't there can't be told apart: no key, nothing kept
        assert_eq!(told_key(&dir.join("gone"), &pack_a(&dir).steps), None);
        std::fs::remove_dir_all(&dir).ok();
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
        let mut p = Preflight::new(1, &steps, Set::Full, KEY.into());
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
        let mut p = Preflight::new(1, &steps, Set::Full, KEY.into());
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

    /// frictions29 26: a failed preflight names the variables of tarae's environment its steps got that change the
    /// project's tools (the packs' `pass`: `PYTHON*`, `RUST*` …) — names, never values; none set, or a preflight
    /// that passed: nothing said. The environment is a stand-in (`Editor.claude_env`), never this process's.
    #[test]
    fn a_failed_preflight_names_the_tools_variables_it_ran_with() {
        let s = crate::scratch::Scratch::new("preflight-env");
        let run = |env: &[(&str, &str)], cmd: &str| {
            let mut ed = Editor::new(crate::config::Config::default());
            ed.session.root = s.to_path_buf();
            ed.claude_env = Some(env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect());
            ed.preflight_run(vec![step("test", cmd)], Set::Quick).unwrap();
            let end = Instant::now() + Duration::from_secs(10);
            while ed.session.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running) {
                assert!(Instant::now() < end, "it ended");
                if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(10)) {
                    ed.handle_event(ev);
                }
            }
            ed.messages.last().map(|m| m.2.clone()).unwrap_or_default()
        };
        let env = [("PYTHONDONTWRITEBYTECODE", "1"), ("RUSTFLAGS", "secret-value-marker"), ("HOME", "/h")];
        let said = run(&env, "exit 1");
        assert!(said.contains("your environment sets PYTHONDONTWRITEBYTECODE, RUSTFLAGS — "), "{said}");
        assert!(
            !said.contains("HOME") && !said.contains("secret-value-marker"),
            "names of tools' only: {said}"
        );
        assert!(!run(&env, "true").contains("your environment"), "passed: nothing said");
        assert!(!run(&[("HOME", "/h")], "exit 1").contains("your environment"), "none set");
        let many: Vec<String> = (0..6).map(|i| format!("PYTHON{i}")).collect();
        assert_eq!(listed(&many), "PYTHON0, PYTHON1, PYTHON2, PYTHON3 and 2 more");
    }

    /// #126: a failed step's output one key away — `o` on the output selects the step in view in the steps' list
    /// beside it (panes P3 §2.2), `o` there opens the selected step's whole in a buffer (only its lines, on its
    /// failure), `:preflight-output <step>` any step's; a test step and a step named `test bundled` don't get mixed
    /// up.
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
        // The output has the keys (a click on it, `C-w j`), its cursor at its end: `o` — the step in view selected
        ed.session.at = crate::session::At::Slot;
        ed.focus_sync();
        let key = |ed: &mut Editor, c: char| ed.handle_key(crate::key::Key::plain(crate::key::Code::Char(c)));
        key(&mut ed, 'o');
        let listed = ed.list_focused().map(|g| g.kind);
        assert_eq!(listed, Some(crate::pane::GroupKind::Preflight), "the steps' list has the keys");
        assert_eq!(ed.session.preflight.as_ref().map(|p| p.selected), Some(2), "test bundled, in view");
        // …then the step above, and `o` there
        key(&mut ed, 'k');
        key(&mut ed, 'o');
        let title = ed.doc().title.clone();
        assert_eq!(title.as_deref(), Some("preflight · test"), "{:?}", ed.messages.last());
        let text = ed.doc().text.to_string();
        assert_eq!(text, "running 3 tests\ntest a ... ok\ntest b ... FAILED\nfailures:\n", "its lines alone");
        assert_eq!(ed.cursor_line(), 2, "on its failure");
        assert!(!ed.session.is_at(crate::session::At::Slot), "the keys in the buffer");
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
