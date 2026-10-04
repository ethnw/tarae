//! Traces of work from its ask to its end, a step at a time, as one `timing` line in `tarae.log` — where
//! the time went. Each step says what it took since the step before: a go-to (N1) `references 1.84s · sent
//! 3ms · waited for "cargo check" 1.20s · answered 600ms (412 places) · main loop 2ms · list 4ms · drawn
//! 6ms`; a run `claude's run: cargo test 48.2s · card up 2ms · answered y 3.1s · sandbox ready 210ms ·
//! saved 0ms · spawned 3ms · first output 120ms · waited for cargo's lock 37.0s · exit 0 9.0s`.

use std::sync::{Arc, Mutex};
use std::time::Instant;

pub struct Trace {
    what: String,
    start: Instant,
    last: Instant,
    steps: Vec<String>,
    /// A go-to's answer has been dealt with (listed, jumped to, nothing found): the next frame ends it.
    pub answered: bool,
}

impl Trace {
    pub fn new(what: impl Into<String>) -> Trace {
        let now = Instant::now();
        Trace { what: what.into(), start: now, last: now, steps: Vec::new(), answered: false }
    }

    /// A step done now — what it took is the time since the step before.
    pub fn step(&mut self, what: impl Into<String>) {
        self.step_at(what, Instant::now());
    }

    pub fn step_at(&mut self, what: impl Into<String>, at: Instant) {
        let took = at.saturating_duration_since(self.last);
        self.steps.push(format!("{} {}", what.into(), crate::term::duration(took.as_secs_f32())));
        self.last = self.last.max(at);
    }

    /// The log line.
    pub fn line(&self) -> String {
        let total = self.last.saturating_duration_since(self.start).as_secs_f32();
        format!("{} {} · {}", self.what, crate::term::duration(total), self.steps.join(" · "))
    }
}

/// A run's trace — a task's, tests', a preflight's, Claude's command from its call on: shared by whatever
/// carries the run along (Claude's call, its card, the run), and logged once: at its end (`end`), else when
/// the last of them lets it go — a call dropped on the way (a card gone, a turn that ended) says where it
/// stood, so a run that never started still shows where its time went.
#[derive(Clone)]
pub struct RunTrace(Arc<Mutex<Shared>>);

struct Shared {
    trace: Trace,
    ended: bool,
}

impl RunTrace {
    pub fn new(what: impl Into<String>) -> RunTrace {
        RunTrace(Arc::new(Mutex::new(Shared { trace: Trace::new(what), ended: false })))
    }

    fn shared(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn step(&self, what: impl Into<String>) {
        self.step_at(what, Instant::now());
    }

    pub fn step_at(&self, what: impl Into<String>, at: Instant) {
        let mut s = self.shared();
        if !s.ended {
            s.trace.step_at(what, at);
        }
    }

    /// It ended — `how` (`exit 0`, `declined`): its line goes to the log, once.
    pub fn end(&self, how: impl Into<String>) {
        let mut s = self.shared();
        if s.ended {
            return;
        }
        s.trace.step(how);
        s.ended = true;
        logged(&s.trace.line());
    }
}

impl Drop for Shared {
    fn drop(&mut self) {
        if !self.ended {
            self.trace.step("dropped before its end");
            logged(&self.trace.line());
        }
    }
}

impl std::fmt::Debug for RunTrace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RunTrace({})", self.shared().trace.what)
    }
}

fn logged(line: &str) {
    crate::log::line("tarae", "timing", line);
    #[cfg(test)]
    LOGGED.with(|l| l.borrow_mut().push(line.to_string()));
}

#[cfg(test)]
thread_local! {
    /// (tests: the run traces logged on this thread)
    pub static LOGGED: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run's trace is logged once — at its end, or, never ended, when the last holder lets it go.
    #[test]
    fn a_run_trace_is_logged_once_at_its_end_or_when_dropped() {
        let ours = |w: &str| {
            LOGGED.with(|l| l.borrow().iter().filter(|x| x.starts_with(w)).cloned().collect::<Vec<_>>())
        };
        let t = RunTrace::new("ended-run");
        let held = t.clone();
        t.step("spawned");
        held.end("exit 0");
        t.end("again");
        drop((t, held));
        let lines = ours("ended-run");
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(lines[0].contains(" · spawned ") && lines[0].contains(" · exit 0 "), "{}", lines[0]);
        let t = RunTrace::new("lost-run");
        t.step("card up");
        let held = t.clone();
        drop(t);
        assert!(ours("lost-run").is_empty(), "still held");
        drop(held);
        let lines = ours("lost-run");
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].contains(" · card up ") && lines[0].contains(" · dropped before its end "),
            "{}",
            lines[0]
        );
    }
}
