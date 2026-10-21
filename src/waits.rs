//! What a run waits on that isn't its own work — another cargo holding the build directory — read from its
//! output by the language packs' `[[waits]]` (`packs::waits`): a line one of them matches is the run's
//! state, not output, until the next line that isn't one. The task and test panels say `waiting for cargo's
//! lock · 0:37`, Claude's report how much of the run it was, the run's trace where it began and ended. The
//! core knows no tool's lines: a language whose pack declares none has none (#10: a check waited 37 of its
//! 48 s on another session's build, and looked stuck).

use std::time::{Duration, Instant};

use crate::trace::RunTrace;

/// A running run that has printed nothing this long says so — the chat's row, `tarae state`'s runs, the
/// inbox for one of Claude's — and its report says it (#157: a server that never exited once its input
/// closed showed only `● run` for 27 minutes).
pub const SILENT: Duration = Duration::from_secs(180);

/// Under this share of a core, all its processes told, a run is doing nothing (`Waiting::looked`).
pub const IDLE_PCT: f32 = 1.0;

/// Claude's report's line on a run that ended (or was stopped) at `end` after printing nothing since `last`
/// for `SILENT` or more: `It printed nothing for its last 27:04.`
pub fn silent_line(last: Instant, end: Instant) -> Option<String> {
    let d = end.saturating_duration_since(last);
    (d >= SILENT).then(|| {
        format!(
            "It printed nothing for its last {} — a program that waits for input or never exits (a server \
             after its input closed) hangs until the user stops it.",
            clock(d)
        )
    })
}

/// A pack's `[[waits]]` entry: `line`, a regular expression a waiting line matches (its named groups go into
/// the texts as `{name}`); `for`, what it waits for as the panel says it (`cargo's lock`); `report`, as
/// Claude's report says it (`cargo's {what} lock`; `for` unless set).
pub struct Rule {
    line: regex::Regex,
    said: String,
    report: String,
}

impl Rule {
    pub fn new(line: regex::Regex, said: String, report: Option<String>) -> Rule {
        let report = report.unwrap_or_else(|| said.clone());
        Rule { line, said, report }
    }

    fn hit(&self, line: &str) -> Option<Hit> {
        let caps = self.line.captures(line)?;
        let fill = |t: &str| {
            let mut t = t.to_string();
            for name in self.line.capture_names().flatten() {
                let value = caps.name(name).map_or("", |m| m.as_str().trim());
                t = t.replace(&format!("{{{name}}}"), value);
            }
            t
        };
        Some(Hit { said: fill(&self.said), report: fill(&self.report) })
    }
}

/// Every pack's rules, read once per run on its reader thread (a pack of one's own is a file).
#[derive(Default)]
pub struct Rules(Vec<Rule>);

impl Rules {
    pub fn load() -> Rules {
        Rules(crate::packs::waits())
    }

    /// The wait `line` says, if one of the rules matches it.
    pub fn hit(&self, line: &str) -> Option<Hit> {
        self.0.iter().find_map(|r| r.hit(line))
    }
}

/// A line that says the run waits: on what, as the panel says it and as Claude's report does.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hit {
    pub said: String,
    pub report: String,
}

/// A run's waits, from its output's lines.
#[derive(Default)]
pub struct Waiting {
    /// What it waits on now, and since when.
    pub now: Option<(Hit, Instant)>,
    /// What it waited on, all told — (as the report says it, how long), in the order first waited on.
    pub waited: Vec<(String, Duration)>,
    /// A line has come (the trace's `first output` is said).
    output: bool,
    /// Since when its processes have used next to no CPU, as `:procs`' looks saw them (`stuck.rs`) — None:
    /// they're working, or haven't been looked at.
    pub idle: Option<Instant>,
    /// Its share of a core at the last look, all its processes told.
    pub cpu: Option<f32>,
    /// When the stuck card last asked about it — it asks again only after as long again.
    pub stuck_asked: Option<Instant>,
    /// The processes of its tree at work at the last look — (pid, start), `procs::at_work`.
    pub at_work: Vec<(u32, u64)>,
    /// When a look last found a process in its tree newer than any before — work that prints nothing here
    /// (frictions34 39: each scene's tarae under `| tail`) moves on as a line of output does.
    pub moved: Option<Instant>,
    /// When the time limit its own command set ends it (`timeout 2700 …` — `stuck::limit`): the stuck card
    /// doesn't ask before then (cardburden 5: the breaks run, its own 45 minutes, asked at 10).
    pub limit_at: Option<Instant>,
    /// Until when that limit holds the stuck card: it, at most `stuck::HOLD_MAX` from the start (`timeout 30d`
    /// hung is still asked about).
    pub hold_until: Option<Instant>,
    /// Its processes were found connected outside themselves (a container engine, another host —
    /// `stuck::connected_outside`): its work may go on there, and it's asked as one let reach a daemon is.
    pub connected: bool,
}

impl Waiting {
    /// A line of output read at `at` (`hit`: a wait's line) — true: a wait's line, the run's state rather
    /// than output. The trace hears the first output, where a wait began (`ran`) and ended (`waited for …`).
    pub fn line(&mut self, hit: Option<Hit>, at: Instant, trace: &RunTrace) -> bool {
        let first = !std::mem::replace(&mut self.output, true);
        if first {
            trace.step_at("first output", at);
        }
        let Some(hit) = hit else {
            self.end(at, trace);
            return false;
        };
        if self.now.as_ref().is_some_and(|(h, _)| *h == hit) {
            return true;
        }
        // (the time up to it is said already when it's the first line, or another wait ended with it —
        // cargo's package cache, then its build directory)
        if !self.end(at, trace) && !first {
            trace.step_at("ran", at);
        }
        self.now = Some((hit, at));
        true
    }

    /// The wait under way ends at `at` (its next line, or the run's end) — false: none was.
    pub fn end(&mut self, at: Instant, trace: &RunTrace) -> bool {
        let Some((hit, since)) = self.now.take() else { return false };
        let took = at.saturating_duration_since(since);
        trace.step_at(format!("waited for {}", hit.said), at);
        match self.waited.iter_mut().find(|(r, _)| *r == hit.report) {
            Some(w) => w.1 += took,
            None => self.waited.push((hit.report, took)),
        }
        true
    }

    /// The panel's: `waiting for cargo's lock · 0:37` — while it waits.
    pub fn shown(&self, now: Instant) -> Option<String> {
        let (hit, since) = self.now.as_ref()?;
        Some(format!("waiting for {} · {}", hit.said, clock(now.saturating_duration_since(*since))))
    }

    /// How long a running run has printed nothing, since `last` — when that's `SILENT` or more and it waits
    /// on nothing it said (a wait says itself).
    pub fn silent_for(&self, last: Instant, now: Instant) -> Option<Duration> {
        let d = now.saturating_duration_since(last);
        (self.now.is_none() && d >= SILENT).then_some(d)
    }

    /// Whether a quiet run is for the user to look at (the inbox): its processes idle as well for `SILENT`, or
    /// not looked at yet; one still working — a build under `| tail -60`, its output held by the filter — only
    /// once half the time its own command gives it (`limited`) is gone (frictions29 9: three long Linux runs an
    /// hour landed there at 3 minutes).
    pub fn worth_a_look(&self, started: Instant, now: Instant) -> bool {
        let idle = match (self.cpu, self.idle) {
            (None, _) => true,
            (Some(_), Some(since)) => now.saturating_duration_since(since) >= SILENT,
            (Some(_), None) => false,
        };
        // (a process new in its tree lately: it's at work, printing or not)
        let idle = idle && !self.moved.is_some_and(|m| now.saturating_duration_since(m) < SILENT);
        let half = self
            .limit_at
            .and_then(|at| at.checked_duration_since(started))
            .and_then(|d| started.checked_add(d / 2))
            .is_some_and(|at| now >= at);
        idle || half
    }

    /// How long until a running run that printed last at `last` goes silent — None: it is, or it waits.
    pub fn silent_in(&self, last: Instant, now: Instant) -> Option<Duration> {
        let d = (last + SILENT).checked_duration_since(now)?;
        (self.now.is_none() && !d.is_zero()).then_some(d)
    }

    /// Its own command limits it to `limit` from `started` (`stuck::limit`): said until then, and the stuck
    /// card held until then — `stuck::HOLD_MAX` at most.
    pub fn limited(&mut self, started: Instant, limit: Option<Duration>) {
        self.limit_at = limit.and_then(|d| started.checked_add(d));
        self.hold_until = limit.and_then(|d| started.checked_add(d.min(crate::stuck::HOLD_MAX)));
    }

    /// A look at its processes at `now`: `pct` of a core, all told — under `IDLE_PCT`, idle from then on
    /// (unless it already was).
    pub fn looked(&mut self, pct: f32, now: Instant) {
        self.cpu = Some(pct);
        match pct < IDLE_PCT {
            true => _ = self.idle.get_or_insert(now),
            false => self.idle = None,
        }
    }

    /// A look at `now` found `at_work` in its tree (`procs::at_work`): one the last look didn't is progress
    /// (`moved`) — started since, or at work only now, whenever it started; the first one too: a run's root (`sh`)
    /// never does `procs::WORKED`, so its first worker (`sleep 900; cargo test | tail`'s cargo) is its first sign
    /// of work.
    pub fn saw(&mut self, at_work: Vec<(u32, u64)>, now: Instant) {
        if at_work.iter().any(|p| !self.at_work.contains(p)) {
            self.moved = Some(now);
        }
        self.at_work = at_work;
    }

    /// Its last sign of work: the later of `last` (its last line) and a new process a look saw (`moved`).
    pub fn progress(&self, last: Instant) -> Instant {
        self.moved.map_or(last, |m| m.max(last))
    }

    /// How long a running run has printed nothing, when it has also used next to no CPU for `after` or
    /// more, and wasn't asked about within `after` — it looks stuck (#181: cargo hung under qemu, `no output
    /// for 68:11` at 0% CPU, and nothing asked). Never when it says what it waits on, nor `after` zero.
    pub fn stuck_for(&self, last: Instant, now: Instant, after: Duration) -> Option<Duration> {
        let quiet = now.saturating_duration_since(last);
        let idle = now.saturating_duration_since(self.idle?);
        let asked = self.stuck_asked.is_some_and(|a| now.saturating_duration_since(a) < after);
        // (its own time limit still to come: it ends by then, stuck or not — cardburden 5; two hours at most)
        let limited = self.hold_until.is_some_and(|at| at > now);
        let due = !after.is_zero() && self.now.is_none() && quiet >= after && idle >= after;
        (due && !asked && !limited).then_some(quiet)
    }

    /// The chat's row, `tarae state`'s runs, the inbox: `no output for 27:04` (`silent_for`) — and, under a time
    /// limit of its own still to come, `· its timeout ends it in 35:00` (`1h 05m`, `29d 21h` — `span`).
    pub fn silent(&self, last: Instant, now: Instant) -> Option<String> {
        let limit = self
            .limit_at
            .and_then(|at| at.checked_duration_since(now))
            .map(|d| format!(" · its timeout ends it in {}", span(d)))
            .unwrap_or_default();
        self.silent_for(last, now).map(|d| format!("no output for {}{limit}", clock(d)))
    }

    /// Claude's report's line, once the run ended (`took` all told — from its start): `Waited 16.0s for
    /// cargo's build directory lock. Ran 11.0s.` — and with its time in line before it started (`queued`: how
    /// long, behind what — `queued.rs`), `Waited 37.0s — 21.0s queued behind cargo test, 16.0s for cargo's
    /// build directory lock. Ran 11.0s.` None when it waited on nothing.
    pub fn said(&self, took: Duration, queued: Option<(Duration, &[String])>) -> Option<String> {
        let locks: Duration = self.waited.iter().map(|w| w.1).sum();
        let ran = took.saturating_sub(locks).as_secs_f32();
        let queued_said = queued.map(|(d, behind)| match behind {
            [] => format!("{:.1}s queued", d.as_secs_f32()),
            b => format!("{:.1}s queued behind {}", d.as_secs_f32(), listed(b)),
        });
        let each: Vec<String> = queued_said
            .into_iter()
            .chain(self.waited.iter().map(|(r, d)| format!("{:.1}s for {r}", d.as_secs_f32())))
            .collect();
        let total = locks + queued.map_or(Duration::ZERO, |q| q.0);
        match &each[..] {
            [] => None,
            [one] => Some(format!("Waited {one}. Ran {ran:.1}s.")),
            all => Some(format!("Waited {:.1}s — {}. Ran {ran:.1}s.", total.as_secs_f32(), all.join(", "))),
        }
    }
}

/// `a` · `a and b` · `a, b and c`.
fn listed(names: &[String]) -> String {
    match names {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// `0:37` · `12:05` — a wait's time, minutes and seconds (the seconds always two digits, so it doesn't
/// slide as it counts).
pub(crate) fn clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

/// A time that may be long — when a run's own timeout ends it: within the hour as `clock` says it (`35:00`),
/// then `1h 05m`, a day or more `30d` · `29d 22h` (cardburden 4b: `timeout 30d` read `43079:00`).
pub(crate) fn span(d: Duration) -> String {
    let s = d.as_secs();
    match (s / 86_400, s % 86_400 / 3600) {
        (0, 0) => clock(d),
        (0, h) => format!("{h}h {:02}m", s % 3600 / 60),
        (days, 0) => format!("{days}d"),
        (days, h) => format!("{days}d {h:02}h"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Rules {
        let re = regex::Regex::new(r"^\s*Blocking waiting for file lock on (?<what>.+)$").unwrap();
        Rules(vec![Rule::new(re, "cargo's lock".into(), Some("cargo's {what} lock".into()))])
    }

    /// cardburden 4b: when a run's own timeout ends it, said past an hour in hours and past a day in days —
    /// `timeout 30d` read `43079:00`; within the hour as every wait's clock, `35:00`.
    #[test]
    fn a_timeouts_end_reads_in_hours_and_days() {
        let s = Duration::from_secs;
        for (secs, said) in [
            (2100, "35:00"),
            (3599, "59:59"),
            (3600, "1h 00m"),
            (3900, "1h 05m"),
            (86_399, "23h 59m"),
            (86_400, "1d"),
            (30 * 86_400, "30d"),
            (30 * 86_400 - 121 * 60, "29d 21h"),
        ] {
            assert_eq!(span(s(secs)), said, "{secs}");
        }
        let mut w = Waiting::default();
        let t0 = Instant::now();
        w.limited(t0, Some(s(30 * 86_400)));
        let said = w.silent(t0, t0 + s(181)).unwrap();
        assert_eq!(said, "no output for 3:01 · its timeout ends it in 29d 23h");
    }

    /// A wait's line is state, from it to the next line that isn't one; its time is told apart from the
    /// run's, and the trace says where it began and ended.
    #[test]
    fn a_wait_lasts_from_its_line_to_the_next() {
        let rules = rules();
        assert_eq!(rules.hit("   Compiling tarae v0.1.0"), None);
        let hit = rules.hit("    Blocking waiting for file lock on build directory").unwrap();
        assert_eq!(hit.said, "cargo's lock");
        assert_eq!(hit.report, "cargo's build directory lock");
        let trace = RunTrace::new("a-wait-test");
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut w = Waiting::default();
        assert!(!w.line(None, at(1), &trace), "output");
        assert!(w.line(Some(hit.clone()), at(2), &trace), "a wait's line isn't output");
        assert!(w.line(Some(hit.clone()), at(3), &trace), "the same again: still waiting");
        assert_eq!(w.shown(at(39)).as_deref(), Some("waiting for cargo's lock · 0:37"));
        assert_eq!(w.shown(at(2 + 605)).as_deref(), Some("waiting for cargo's lock · 10:05"));
        assert!(!w.line(None, at(39), &trace), "the next line ends it");
        assert_eq!(w.shown(at(40)), None);
        assert_eq!(
            w.said(Duration::from_secs(48), None).as_deref(),
            Some("Waited 37.0s for cargo's build directory lock. Ran 11.0s.")
        );
        // In line before it started too: both said, and their sum
        let behind = ["cargo test".to_string(), "cargo clippy".to_string()];
        assert_eq!(
            w.said(Duration::from_secs(48), Some((Duration::from_secs(21), &behind))).as_deref(),
            Some(
                "Waited 58.0s — 21.0s queued behind cargo test and cargo clippy, 37.0s for cargo's build \
                 directory lock. Ran 11.0s."
            )
        );
        trace.end("exit 0");
        let line =
            crate::trace::LOGGED.with(|l| l.borrow().iter().find(|l| l.starts_with("a-wait-test")).cloned());
        let line = line.expect("logged");
        assert!(
            line.contains("first output 1.0s · ran 1.0s · waited for cargo's lock 37.0s · exit 0"),
            "{line}"
        );
        // Silent: from `SILENT` on, said — unless it says what it waits on
        let mut w = Waiting::default();
        assert_eq!(w.silent(at(0), at(179)), None);
        assert_eq!(w.silent(at(0), at(1624)).as_deref(), Some("no output for 27:04"));
        w.line(Some(hit.clone()), at(5), &trace);
        assert_eq!(w.silent(at(5), at(1624)), None, "its wait says it");
        assert_eq!(silent_line(at(0), at(179)), None);
        assert!(silent_line(at(0), at(1624)).unwrap().starts_with("It printed nothing for its last 27:04"));
        // Nothing waited on: nothing said; only in line, that alone
        assert_eq!(Waiting::default().said(Duration::from_secs(3), None), None);
        assert_eq!(
            Waiting::default().said(Duration::from_secs(11), Some((Duration::from_secs(21), &behind[..1]))),
            Some("Waited 21.0s queued behind cargo test. Ran 11.0s.".into())
        );
    }
}
