//! Brought one after another, simulated (frictions29 24): the world of `sim/sessions.rs` — two sessions with
//! commits of their own — and at random a bring asked (`:session-bring`, its card graded on a worker), one brought
//! as its card's `y` does (`session_bring_now`, fast-forward only), a new commit in a session, a frame, the user
//! taking their time. A landing grades the other sessions' waiting bring cards again (`bring_regrade`) — its jobs
//! among the rest, a bring asked meanwhile, another landing. After every step: one bring card a session.

use std::time::Duration;

use super::sessions::{World, git, one_bring_card, print};
use super::{Choices, Sim};
use crate::editor::Editor;

/// The seeds every `cargo test` runs, each twice — git's processes are most of a step (~40 steps a second).
const SEEDS: &[u64] = &[0];

const INPUTS: usize = 30;

const LIMIT: Duration = Duration::from_secs(60);

#[test]
fn brought_one_after_another() {
    super::seeds("brought_one_after_another", SEEDS, run);
}

fn run(choices: Choices) -> Vec<String> {
    let world = World::new();
    let mut sim = Sim::new("brought_one_after_another", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    (ed.session.root, ed.session.name) = (world.repo.clone(), "proj".into());
    ed.session.git_branch = Some("main".into());
    ed.config.session_worktrees = world.dir.join("wt").display().to_string();
    let mut names = Vec::new();
    for (name, wt) in &world.sessions {
        let id = ed.session_open(wt).unwrap();
        if let Some(l) = ed.session_by_id(id) {
            (l.name, l.branch, l.base) = (name.clone(), Some(name.clone()), Some("main".into()));
        }
        names.push((name.clone(), wt.clone()));
    }
    ed.session_switch(0);
    let dir = world.dir.display().to_string();
    sim.walk(&mut ed, INPUTS, |sim, ed| input(sim, ed, &names), one_bring_card, |ed| print(ed, &dir));
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

fn input(sim: &mut Sim, ed: &mut Editor, names: &[(String, std::path::PathBuf)]) -> String {
    let c = &mut sim.choices;
    let (name, root) = c.pick(names).clone();
    // (by arm: a bring asked 4, one brought 2, a commit 2, a frame 1, time passing 1)
    match c.weighted(&[4, 2, 2, 1, 1]) {
        0 => match ed.session_bring(&name) {
            Ok(()) => format!(":session-bring {name}"),
            Err(e) => format!(":session-bring {name}: {e}"),
        },
        1 => match ed.session_bring_now(&name) {
            Ok(()) => format!("{name} brought (its card's y)"),
            Err(e) => format!("{name} brought: {e}"),
        },
        2 => {
            let file = format!("c{}.txt", sim.steps);
            std::fs::write(root.join(&file), "x\n").unwrap();
            git(&root, &["add", &file]);
            git(&root, &["commit", "-q", "-m", &file]);
            format!("{name}: a commit")
        }
        3 => {
            crate::term::render(ed, &mut Vec::new(), 112, 30).unwrap();
            "a frame".into()
        }
        _ => {
            let ms = *c.pick(&[50, 600]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
    }
}
