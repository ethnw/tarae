//! The time logic decides by — a card settling, a toast's life, how long a run waited — read in one place, so a
//! simulated run (`sim.rs`, test builds only) can hold it still and move it by seed. Logging and measurements
//! read the real clock as they always did; what reads it here is what behaves differently as time passes.
//! Outside a simulation these are `Instant::now()` and `SystemTime::now()`.

use std::time::{Duration, Instant, SystemTime};

/// Now, as logic reads it.
#[inline]
pub fn now() -> Instant {
    #[cfg(test)]
    if let Some(t) = crate::sim::now() {
        return t;
    }
    #[cfg(test)]
    let now = HELD.get().unwrap_or_else(Instant::now) + AHEAD.get();
    #[cfg(not(test))]
    let now = Instant::now();
    now
}

#[cfg(test)]
thread_local! {
    /// How far `advance` moved this thread's clock on.
    static AHEAD: std::cell::Cell<Duration> = const { std::cell::Cell::new(Duration::ZERO) };
    /// Where `hold` stopped this thread's clock.
    static HELD: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// Tests outside a simulation: this thread's clock held still from now on (`advance` still moves it) — a frame with a
/// live time in it (a run's time so far) draws the same at every read, however long the drawing takes. Its now.
#[cfg(test)]
pub fn hold() -> Instant {
    let t = now();
    HELD.set(Some(t - AHEAD.get()));
    t
}

/// Tests outside a simulation: this thread's clock going again after a `hold` — a frame read at a pinned time in a test
/// whose runs go on after it.
#[cfg(test)]
pub fn release() {
    HELD.set(None);
}

/// Tests outside a simulation: this thread's clock moved on by `d` — what `now` and `since` read here from now on (a
/// key-sequence test's wait past follow's PACE, say, without sleeping and without reaching into the state).
#[cfg(test)]
pub fn advance(d: Duration) {
    AHEAD.set(AHEAD.get() + d);
}

/// The wall clock, as logic reads it.
#[inline]
pub fn system() -> SystemTime {
    #[cfg(test)]
    if let Some(t) = crate::sim::system() {
        return t;
    }
    SystemTime::now()
}

/// How long since `t` (one of `now`'s) — `t.elapsed()` on this clock; none for a `t` still to come.
#[inline]
pub fn since(t: Instant) -> Duration {
    now().saturating_duration_since(t)
}

#[cfg(test)]
mod tests {
    /// Every clock read in tarae's code that isn't on this list goes through `clock` — and the list only
    /// shrinks: a file's count is what's left of its raw reads (`CLOCK_READS`) in what a non-test build
    /// compiles, logging and measurements among them. A read added fails here; one converted fails too until
    /// its count goes down (the message prints the table as it is now).
    #[test]
    fn every_clock_read_in_logic_goes_through_clock() {
        let now = crate::sim::lint::counts(CLOCK_READS, &["clock.rs"]);
        crate::sim::lint::only_shrinks("clock reads", &now, RAW_CLOCK_READS);
    }

    /// What a raw clock read looks like — called, or passed as a value (`.map(|_| Instant::now)`,
    /// `.map(Instant::elapsed)`).
    const CLOCK_READS: &[&str] = &["Instant::now", "SystemTime::now", ".elapsed()", "::elapsed"];

    /// Second review 3: a clock read passed as a function value is one too.
    #[test]
    fn a_clock_read_passed_as_a_value_counts() {
        let f = "fn f(v: Option<u8>) {\n    let a = v.map(|_| Instant::now);\n    let b = w.map(Instant::elapsed);\n    let c = SystemTime::now();\n}\n";
        let files = [("main.rs".to_string(), "mod a;\n".to_string()), ("a.rs".to_string(), f.to_string())];
        let n = crate::sim::lint::count_tree(&files, CLOCK_READS, &[]);
        assert_eq!(n.get("a.rs"), Some(&3), "{n:?}");
    }

    /// Raw clock reads left, per file (paths under `src/`).
    const RAW_CLOCK_READS: &[(&str, usize)] = &[
        ("allow.rs", 1),
        ("attach.rs", 4),
        ("builds.rs", 3),
        ("calls.rs", 1),
        ("cards.rs", 3),
        ("chat.rs", 1),
        ("chats.rs", 2),
        ("claudeupdate.rs", 1),
        ("codereview.rs", 1),
        ("containers.rs", 1),
        ("dap.rs", 4),
        ("disk.rs", 3),
        ("dsyms.rs", 1),
        ("editor.rs", 10),
        ("edits.rs", 1),
        ("features.rs", 3),
        ("fswatch.rs", 8),
        ("gitmenu.rs", 1),
        ("gitpanel.rs", 2),
        ("gitrepo.rs", 1),
        ("grammar.rs", 4),
        ("inbox.rs", 4),
        ("llm.rs", 3),
        ("log.rs", 1),
        ("lsp.rs", 3),
        ("lsp_editor.rs", 15),
        ("main.rs", 2),
        ("nav.rs", 6),
        ("picker.rs", 3),
        ("plugin/abi.rs", 1),
        ("plugin/cli.rs", 2),
        ("plugin/io.rs", 7),
        ("plugin/perm.rs", 1),
        ("preflight.rs", 5),
        ("procs.rs", 13),
        ("queued.rs", 5),
        ("reads.rs", 7),
        ("reaper.rs", 3),
        ("rebase.rs", 3),
        ("recovery.rs", 6),
        ("refactor.rs", 2),
        ("restart.rs", 6),
        ("runhomes.rs", 1),
        ("sandbox.rs", 8),
        ("sandbox/keeper.rs", 13),
        ("sandbox/linux.rs", 3),
        ("sandbox/proxy.rs", 3),
        ("sandbox/proxy/egress.rs", 4),
        ("sandbox/proxy/relay.rs", 2),
        ("sandbox/supervisor.rs", 2),
        ("share.rs", 8),
        ("snapshot.rs", 1),
        ("spawn.rs", 2),
        ("stuck.rs", 1),
        ("syntax.rs", 4),
        ("task.rs", 6),
        ("term.rs", 10),
        ("testcode.rs", 1),
        ("testing.rs", 7),
        ("tools.rs", 5),
        ("trace.rs", 3),
        ("uisocket.rs", 1),
        ("uistate.rs", 5),
        ("uistate/claude.rs", 3),
        ("uistate/panels.rs", 1),
        ("uistate/watch.rs", 1),
        ("worktree.rs", 10),
    ];
}
