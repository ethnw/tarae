//! What a run waits on that isn't its own work — another cargo holding the build directory — read from its
//! output by the language packs' `[[waits]]` (`packs::waits`): a line one of them matches is the run's
//! state, not output, until the next line that isn't one. The task and test panels say `waiting for cargo's
//! lock · 0:37`, Claude's report how much of the run it was, the run's trace where it began and ended. The
//! core knows no tool's lines: a language whose pack declares none has none (#10: a check waited 37 of its
//! 48 s on another session's build, and looked stuck).

use std::time::{Duration, Instant};

use crate::trace::RunTrace;

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

    /// Claude's report's line, once the run ended (`took` all told): `Waited 16.0s for cargo's build
    /// directory lock. Ran 11.0s.` — None when it waited on nothing.
    pub fn said(&self, took: Duration) -> Option<String> {
        if self.waited.is_empty() {
            return None;
        }
        let total: Duration = self.waited.iter().map(|w| w.1).sum();
        let each: Vec<String> =
            self.waited.iter().map(|(r, d)| format!("{:.1}s for {r}", d.as_secs_f32())).collect();
        Some(format!("Waited {}. Ran {:.1}s.", each.join(" and "), took.saturating_sub(total).as_secs_f32()))
    }
}

/// `0:37` · `12:05` — a wait's time, minutes and seconds (the seconds always two digits, so it doesn't
/// slide as it counts).
fn clock(d: Duration) -> String {
    let s = d.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules() -> Rules {
        let re = regex::Regex::new(r"^\s*Blocking waiting for file lock on (?<what>.+)$").unwrap();
        Rules(vec![Rule::new(re, "cargo's lock".into(), Some("cargo's {what} lock".into()))])
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
            w.said(Duration::from_secs(48)).as_deref(),
            Some("Waited 37.0s for cargo's build directory lock. Ran 11.0s.")
        );
        trace.end("exit 0");
        let line =
            crate::trace::LOGGED.with(|l| l.borrow().iter().find(|l| l.starts_with("a-wait-test")).cloned());
        let line = line.expect("logged");
        assert!(
            line.contains("first output 1.0s · ran 1.0s · waited for cargo's lock 37.0s · exit 0"),
            "{line}"
        );
        // Nothing waited on: nothing said
        assert_eq!(Waiting::default().said(Duration::from_secs(3)), None);
    }
}
