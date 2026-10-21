//! A stuck run stopped from its card, simulated (frictions29 35): a run of Claude's going (the scenario's own
//! `sleep`, in a session of its own), at random its stuck card asked, a frame, the card's `y` once it's settled, the
//! run printing (its card goes), the user taking their time — the stop's job (its tree read, TERM, the wait, KILL,
//! the wait, what outlived it) among the rest; one stop a run, later cards kept waiting (`n`). After every step: one
//! stuck card at most, and the stop said once for its `y`.

use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::Editor;
use crate::key::{Code, Key};
use crate::sizes::Slot;

const SEEDS: &[u64] = &[0, 1];

const INPUTS: usize = 30;

const LIMIT: Duration = Duration::from_secs(60);

/// The line a stop puts in the run's output first.
const STOPPED: &str = "── stopped from tarae's card";

#[test]
fn a_stuck_run_stopped_from_its_card() {
    super::seeds("a_stuck_run_stopped_from_its_card", SEEDS, run);
}

fn run(choices: Choices) -> Vec<String> {
    let dir = crate::scratch::Scratch::new("sim-stuckstop");
    let root = std::fs::canonicalize(dir.path()).unwrap();
    // (the run's process: the scenario's own, in a session of its own as a run's is)
    let mut c = std::process::Command::new("sleep");
    c.arg("60").current_dir(&root);
    let pid = crate::spawn::setsid(&c).unwrap().pid;
    let mut sim = Sim::new("a_stuck_run_stopped_from_its_card", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    (ed.session.root, ed.session.name) = (root.clone(), "proj".into());
    let task = crate::task::Task { name: "hangs".into(), cmd: "sleep 60".into(), cwd: root };
    let mut r = crate::task::Run::for_test(task).running_as(pid);
    r.by_claude = true;
    ed.session.task_run = Some(r);
    let ys = std::cell::Cell::new(0);
    let check = |ed: &Editor| held(ed, ys.get());
    sim.walk(&mut ed, INPUTS, |sim, ed| input(sim, ed, pid, &ys), check, print);
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    // (ended whatever the steps did; reaped)
    // SAFETY: a KILL to the scenario's own process, never reaped before this: still its own.
    unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    let _ = crate::spawn::wait(pid);
    trace
}

fn stuck_cards(ed: &Editor) -> usize {
    ed.offers.iter().chain(&ed.session.parked).filter(|o| o.stuck().is_some()).count()
}

fn stops_said(ed: &Editor) -> usize {
    let out = ed.session.task_run.as_ref().map(|r| r.output.as_slice()).unwrap_or_default();
    out.iter().filter(|(_, l)| l.starts_with(STOPPED)).count()
}

fn input(sim: &mut Sim, ed: &mut Editor, pid: u32, ys: &std::cell::Cell<usize>) -> String {
    let c = &mut sim.choices;
    // (by arm: the card asked 3, a frame 2, `y` 3, the run prints 1, time passing 1)
    match c.weighted(&[3, 2, 3, 1, 1]) {
        0 if stuck_cards(ed) == 0 => {
            let Some(r) = ed.session.task_run.as_ref() else { return "(no run)".into() };
            let s = crate::stuck::Stuck {
                slot: Slot::Task,
                run: r.trace.id(),
                pid,
                asked: crate::clock::now(),
                quiet: Duration::from_secs(11 * 60),
                cpu: 0.0,
                at: 0,
                reach: None,
            };
            if let Some(r) = ed.session.task_run.as_mut()
                && let Some(t) = crate::clock::now().checked_sub(Duration::from_secs(11 * 60))
            {
                r.last_output = t;
            }
            ed.push_offer(crate::stuck::card(s, "Claude's command sleep 60", &[], &[]));
            "the stuck card asks".into()
        }
        0 => "(a stuck card is up)".into(),
        1 => {
            crate::term::render(ed, &mut Vec::new(), 112, 30).unwrap();
            "a frame".into()
        }
        2 if ed.offers.first().is_some_and(|o| o.stuck().is_some()) => {
            // (drawn, then settled: a weighty card's keys)
            crate::term::render(ed, &mut Vec::new(), 112, 30).unwrap();
            sim.wait(crate::offer::SETTLE + Duration::from_millis(50));
            // (one stop a run of the scenario: a second one's read of the tree would race the kernel ending what
            // the first signalled — the later cards are kept waiting, `n`)
            if ys.get() > 0 {
                ed.handle_key(Key::plain(Code::Char('n')));
                return "n on the stuck card".into();
            }
            ed.handle_key(Key::plain(Code::Char('y')));
            ys.set(1);
            "y on the stuck card".into()
        }
        2 => "(no stuck card in front)".into(),
        3 => {
            if let Some(r) = ed.session.task_run.as_mut() {
                r.last_output = crate::clock::now();
            }
            crate::stuck::sync(ed);
            "the run prints".into()
        }
        _ => {
            let ms = *c.pick(&[100, 4000]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
    }
}

/// One stuck card at most; the stop said in the run's output no more than once per `y`.
fn held(ed: &Editor, ys: usize) -> Result<(), String> {
    if stuck_cards(ed) > 1 {
        return Err(format!("{} stuck cards up", stuck_cards(ed)));
    }
    if stops_said(ed) > ys {
        return Err(format!("the stop said {} times for {ys} y", stops_said(ed)));
    }
    Ok(())
}

/// What the trace says after a step (the processes' own end isn't in it: the kernel's timing).
fn print(ed: &Editor) -> String {
    format!("stuck cards {} · stops said {}", stuck_cards(ed), stops_said(ed))
}
