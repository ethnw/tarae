//! A run's pty broker, simulated (terminals A): at random a run starts with its broker, asks for some ptys, and
//! ends — or one asks and then says nothing (the broker waits `ASK_WITHIN` at most for it) — or something asks
//! after the run is over, or time passes. Each run lives within its step (its broker's thread joined at its end).
//! After every step: no broker left over, and each run's end said as many ptys as were handed in. Inside a
//! sandbox that can't make one (no `/dev/ptmx`) every ask comes back with none — the count says 0 then too.

use std::cell::RefCell;
use std::path::Path;
use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::Editor;

const SEEDS: &[u64] = &[0, 1];

const INPUTS: usize = 20;

const LIMIT: Duration = crate::test_results::PATIENCE;

#[test]
fn a_runs_pty_broker() {
    super::seeds("a_runs_pty_broker", SEEDS, run);
}

fn run(choices: Choices) -> Vec<String> {
    let dir = crate::scratch::Scratch::new("sim-ptys");
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let mut sim = Sim::new("a_runs_pty_broker", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    // (each run's end: what its broker said it made, what its asks got)
    let ends: RefCell<Vec<(usize, usize)>> = RefCell::new(Vec::new());
    let mut n = 0u64;
    sim.walk(
        &mut ed,
        INPUTS,
        |sim, _| input(sim, &root, &mut n, &ends),
        |_| held(&ends.borrow()),
        |_| String::new(),
    );
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

fn input(sim: &mut Sim, root: &Path, n: &mut u64, ends: &RefCell<Vec<(usize, usize)>>) -> String {
    *n += 1;
    let id = 7_000_000 + *n;
    let run = root.join(n.to_string());
    std::fs::create_dir(&run).unwrap();
    let socket = crate::sandbox::ptybroker::socket(&run);
    let c = &mut sim.choices;
    // (by arm: a run that asks for some 4, one with an asker that says nothing 1, an ask after the end 1, time 1)
    match c.weighted(&[4, 1, 1, 1]) {
        0 => {
            let asks = *c.pick(&[0, 1, 2, 3]);
            crate::sandbox::ptybroker::start(id, &run);
            let got = (0..asks).filter(|_| crate::pty::asked(&socket, 24, 80).is_ok()).count();
            let (made, _) = crate::sandbox::ptybroker::end(id);
            ends.borrow_mut().push((made, got));
            format!("a run asked {asks}: {got} handed in, its end says {made}")
        }
        1 => {
            crate::sandbox::ptybroker::start(id, &run);
            let silent = std::os::unix::net::UnixStream::connect(&socket);
            let (made, _) = crate::sandbox::ptybroker::end(id);
            drop(silent);
            ends.borrow_mut().push((made, 0));
            format!("a run with a silent asker ended: its end says {made}")
        }
        2 => {
            crate::sandbox::ptybroker::start(id, &run);
            let (made, _) = crate::sandbox::ptybroker::end(id);
            let after = crate::pty::asked(&socket, 24, 80).is_ok();
            ends.borrow_mut().push((made, usize::from(after)));
            format!("an ask after the run's end: {}", if after { "answered" } else { "refused" })
        }
        3 => {
            let ms = *c.pick(&[50, 300]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
        _ => unreachable!("one arm a weight"),
    }
}

/// Each run's end said as many ptys as were handed in — none after it — and no broker is left going.
fn held(ends: &[(usize, usize)]) -> Result<(), String> {
    if let Some((made, got)) = ends.iter().find(|(made, got)| made != got) {
        return Err(format!("a run's end said {made} ptys, {got} were handed in"));
    }
    // (this scenario's runs only: other tests' brokers may be going beside it)
    let left =
        crate::sandbox::ptybroker::going().into_iter().filter(|id| (7_000_000..8_000_000).contains(id));
    match left.count() {
        0 => Ok(()),
        n => Err(format!("{n} broker(s) left going")),
    }
}
