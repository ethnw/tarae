//! The plan's limits as the chat's claude says them, simulated (frictions29 #81): a running claude (a stand-in
//! whose lines are kept — `Editor.claude_lines`), and at random a `rate_limit_event` with no figure (the one a
//! claude sends as it starts), one with a figure, `get_usage`'s answer to the question in flight, the 5-hour
//! window's next period, the user taking their time (minutes). After every step: never a second question while one
//! is in flight; a figure said for a period is never taken away by an event of that period with none; and an event
//! with no figure, nothing in flight and no question young, asks one.

use std::cell::RefCell;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::Editor;
use crate::limits::{Answer, FRESH_MS, Reading, Report, Source, Status};

const SEEDS: &[u64] = &[0, 1, 2];

const INPUTS: usize = 40;

const LIMIT: Duration = crate::test_results::PATIENCE;

const WINDOW: &str = "five_hour";

#[test]
fn the_quota_keeps_its_figure() {
    super::seeds("the_quota_keeps_its_figure", SEEDS, run);
}

/// What the checks keep between steps.
#[derive(Default)]
struct Seen {
    /// Questions asked so far (the stand-in's `get_usage` lines).
    asked: usize,
    /// The period (its reset) a figure was said for.
    figured: Option<u64>,
    /// The step just taken was an event with no figure, with nothing in flight and no question young then.
    must_ask: bool,
    /// Its period's reset now, in unix ms.
    resets: u64,
}

fn run(choices: Choices) -> Vec<String> {
    let mut sim = Sim::new("the_quota_keeps_its_figure", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    let (tx, rx) = std::sync::mpsc::channel();
    ed.claude_lines = Some(tx);
    crate::chat::open(&mut ed);
    ed.limits.ask = true;
    let seen = RefCell::new(Seen { resets: crate::usage::now_ms() + 5 * 3_600_000, ..Seen::default() });
    sim.walk(
        &mut ed,
        INPUTS,
        |sim, ed| input(sim, ed, &seen),
        |ed| held(ed, &rx, &mut seen.borrow_mut()),
        print,
    );
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

fn reading(percent: Option<u32>, resets: u64, from: Source) -> Reading {
    let status = (from == Source::Event).then_some(Status::Allowed);
    let seen_at_ms = crate::usage::now_ms();
    Reading { window: WINDOW.into(), percent, resets_at_ms: Some(resets), status, seen_at_ms, from }
}

fn input(sim: &mut Sim, ed: &mut Editor, seen: &RefCell<Seen>) -> String {
    let c = &mut sim.choices;
    let resets = seen.borrow().resets;
    // (by arm: an event with no figure 4, one with a figure 2, the answer 3, the next period 1, minutes passing 3)
    match c.weighted(&[4, 2, 3, 1, 3]) {
        0 => {
            let now = crate::usage::now_ms();
            let young = ed.limits.asked_ms.is_some_and(|at| now.saturating_sub(at) < FRESH_MS);
            seen.borrow_mut().must_ask = ed.limits.pending.is_none() && !young;
            let r = Report { readings: vec![reading(None, resets, Source::Event)], ..Report::default() };
            crate::limits::seen(ed, r);
            "an event with no figure".into()
        }
        1 => {
            let p = *c.pick(&[3, 40, 85]);
            let r = Report { readings: vec![reading(Some(p), resets, Source::Event)], ..Report::default() };
            crate::limits::seen(ed, r);
            format!("an event at {p} %")
        }
        2 => match ed.limits.pending {
            None => "(no question in flight)".into(),
            Some(_) => {
                let p = *c.pick(&[4, 50]);
                let readings = vec![reading(Some(p), resets, Source::Usage)];
                crate::limits::seen(ed, Report { readings, answer: Some(Answer::Read), ..Report::default() });
                format!("get_usage answers {p} %")
            }
        },
        3 => {
            seen.borrow_mut().resets = resets + 5 * 3_600_000;
            "the next period's events from here".into()
        }
        4 => {
            let m = *c.pick(&[1, 6, 11]);
            sim.wait(Duration::from_secs(60 * m));
            format!("{m} min pass")
        }
        _ => unreachable!("one arm a weight"),
    }
}

fn held(ed: &Editor, rx: &Receiver<String>, seen: &mut Seen) -> Result<(), String> {
    let new = rx.try_iter().filter(|l| l.contains("get_usage")).count();
    if new > 1 {
        return Err(format!("{new} questions at once"));
    }
    if std::mem::take(&mut seen.must_ask) && new == 0 {
        return Err("an event with no figure, nothing in flight, nothing young: not asked".into());
    }
    seen.asked += new;
    let w = ed.limits.windows.iter().find(|w| w.window == WINDOW);
    match (w, seen.figured) {
        (Some(w), Some(p)) if w.resets_at_ms == Some(p) && w.percent.is_none() => {
            return Err("the figure said for this period is gone".into());
        }
        _ => {}
    }
    if let Some(w) = w.filter(|w| w.percent.is_some()) {
        seen.figured = w.resets_at_ms;
    }
    Ok(())
}

fn print(ed: &Editor) -> String {
    let w = ed.limits.windows.iter().find(|w| w.window == WINDOW);
    let said = w.map_or("none".into(), crate::limits::used);
    format!("5-hour {said}{}", if ed.limits.pending.is_some() { " · asked" } else { "" })
}
