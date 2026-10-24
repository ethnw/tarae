//! Brought one after another, simulated (frictions29 24): the world of `sim/sessions.rs` — two sessions with
//! commits of their own — and at random a bring asked (`:session-bring`, its card graded on a worker), one brought
//! as its card's `y` does (`session_bring_now`, fast-forward only), a new commit in a session, a frame, the user
//! taking their time. A landing grades the other sessions' waiting bring cards again (`bring_regrade`) — its jobs
//! among the rest, a bring asked meanwhile, another landing. And the bring queue (24 a, `bringqueue.rs`): both
//! marked, `B`, its card's `y` — the rebase and the quick preflight it starts as tasks stood in for by the
//! scenario (no task is modelled): it rebases the checkout itself (`git rebase main`, aborted at a conflict —
//! the two sessions' `a.rs` clash) and says a preflight passed or failed. After every step: one bring card a
//! session, and the queue holds each session once, one under way at most.

use std::time::Duration;

use super::sessions::{World, git, one_bring_card, print};
use super::{Choices, Sim};
use crate::bringqueue::Stage;
use crate::editor::Editor;
use crate::key::{Code, Key};

/// The seeds every `cargo test` runs, each twice — git's processes are most of a step (~40 steps a second).
const SEEDS: &[u64] = &[0];

const INPUTS: usize = 30;

const LIMIT: Duration = crate::test_results::PATIENCE;

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
    ed.bring_queue.stand_in = true;
    let dir = world.dir.display().to_string();
    let check = |ed: &Editor| one_bring_card(ed).and_then(|()| queue_held(ed)).and_then(|()| failed_said(ed));
    let said = |ed: &Editor| format!("{} · {}", print(ed, &dir), queue_said(ed));
    sim.walk(&mut ed, INPUTS, |sim, ed| input(sim, ed, &names), check, said);
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

fn input(sim: &mut Sim, ed: &mut Editor, names: &[(String, std::path::PathBuf)]) -> String {
    let c = &mut sim.choices;
    let (name, root) = c.pick(names).clone();
    // (by arm: a bring asked 4, one brought 2, a commit 2, a frame 1, time passing 1, the queue asked 1, the
    // queue's task stood in for 3, a bring card's failed preflight 1)
    match c.weighted(&[4, 2, 2, 1, 1, 1, 3, 1]) {
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
        4 => {
            let ms = *c.pick(&[50, 600]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
        5 => queue_asked(sim, ed, names),
        6 => {
            let passes = *c.pick(&[true, false]);
            stand_in(ed, names, passes)
        }
        _ => preflight_failed(ed, &name, &root),
    }
}

/// A session's bring card answered `p` (the card gone, as its `p` takes it), the preflight it ran stood in for and
/// failing (no task is modelled): the card is back saying so (frictions34 42) — one bring card a session still,
/// whatever was asked meanwhile.
fn preflight_failed(ed: &mut Editor, name: &str, root: &std::path::Path) -> String {
    let Ok((id, _)) = ed.session_named(name) else { return format!("({name}: no session)") };
    if ed.bring_queue_at(id).is_some() {
        return format!("({name} is the queue's)");
    }
    let m = crate::rebase::Moved {
        root: root.to_path_buf(),
        name: name.into(),
        branch: name.into(),
        base: "main".into(),
        moved: 0,
        ahead: 1,
    };
    ed.with_session(id, |ed| {
        (ed.session.bring_after_preflight, ed.session.bring_card_after) = (true, Some(Box::new(m)));
        ed.rebase_preflight_done(false, "quick preflight");
    });
    format!("{name}'s bring card: p, its preflight failed")
}

/// `space W`'s `m` on each session, `B`, and the card's `y` once it's drawn and focused.
fn queue_asked(_sim: &mut Sim, ed: &mut Editor, names: &[(String, std::path::PathBuf)]) -> String {
    if ed.bring_queue.run.is_some() {
        return "(a queue is under way)".into();
    }
    ed.bring_queue.marks.clear();
    for (name, _) in names {
        if let Ok((id, _)) = ed.session_named(name) {
            ed.bring_mark(id);
        }
    }
    if let Err(e) = ed.bring_queue_ask() {
        return format!("B: {e}");
    }
    let front = |ed: &Editor| ed.offers.first().is_some_and(|o| o.kind() == "bring-queue");
    if !front(ed) {
        return "B: its card behind another".into();
    }
    crate::term::render(ed, &mut Vec::new(), 112, 30).unwrap();
    // (focused — a press on it — then its `y`)
    ed.card_focus_front();
    ed.handle_key(Key::plain(Code::Char('y')));
    "B, and y on its card".into()
}

/// What the queue's one under way waits on, ended by the scenario: its rebase — run here, aborted at a conflict —
/// or its preflight (`passes`).
fn stand_in(ed: &mut Editor, names: &[(String, std::path::PathBuf)], passes: bool) -> String {
    let Some((id, stage)) = ed.bring_queue.run.as_ref().and_then(|r| r.now) else {
        return "(nothing of the queue's under way)".into();
    };
    let Some((name, root)) = names.iter().find(|(n, _)| ed.session_named(n).is_ok_and(|(i, _)| i == id))
    else {
        return "(the queue's one isn't the scenario's)".into();
    };
    match stage {
        Stage::Rebasing => {
            let rebased = rebase(root);
            let done = if rebased { Ok(()) } else { Err("its rebase stopped at a conflict".into()) };
            ed.with_session(id, |ed| {
                ed.bring_queue_rebased(done);
            });
            format!("{name}'s rebase {}", if rebased { "done" } else { "stopped: aborted" })
        }
        Stage::Preflight => {
            ed.with_session(id, |ed| ed.rebase_preflight_done(passes, "quick preflight"));
            format!("{name}'s preflight {}", if passes { "passed" } else { "failed" })
        }
        s => format!("({name}: {s:?}, nothing to stand in for)"),
    }
}

/// `git rebase main` in `root` as the scenario's own git runs — aborted at a conflict: whether it went through.
fn rebase(root: &std::path::Path) -> bool {
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null", "-C"])
            .arg(root)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("XDG_CONFIG_HOME", "/dev/null")
            .env("GIT_AUTHOR_DATE", "2026-10-18T00:00:00Z")
            .env("GIT_COMMITTER_DATE", "2026-10-18T00:00:00Z")
            .output()
            .unwrap()
            .status
            .success()
    };
    run(&["rebase", "-q", "main"]) || !run(&["rebase", "--abort"])
}

/// A bring card back from a failed preflight reads as one wherever it is — its risk warned, its `y` a bring anyway —
/// and none says the plain card's words with the failed one's output behind `v` (frictions29 91: a bring asked
/// again put them over it).
fn failed_said(ed: &Editor) -> Result<(), String> {
    let parked = std::iter::once(&ed.session).chain(&ed.sessions).flat_map(|l| &l.parked);
    for o in ed.offers.iter().chain(parked) {
        if !matches!(o.what, crate::offer::What::Rebased(_)) {
            continue;
        }
        let failed = o.question.contains("preflight failed");
        // (a held one has no `y` — `codereview::push_bring_card`)
        let y = o.answers(true).first().filter(|a| a.0 == 'y').map(|a| a.1);
        let anyway = o.held.is_some() || y == Some("Bring anyway");
        if failed != !o.warnings.is_empty() || (failed && !anyway) || (o.output.is_some() && !failed) {
            return Err(format!("card#{} {:?} reads as {y:?}: {:?}", o.id, o.title, o.question));
        }
    }
    Ok(())
}

/// The queue holds each session once, and one under way at most.
fn queue_held(ed: &Editor) -> Result<(), String> {
    let Some(run) = ed.bring_queue.run.as_ref() else { return Ok(()) };
    let mut all: Vec<u64> = run.left.iter().copied().chain(run.now.map(|(id, _)| id)).collect();
    let n = all.len();
    all.sort_unstable();
    all.dedup();
    match all.len() == n {
        true => Ok(()),
        false => Err(format!("a session twice in the queue: {:?} · {:?}", run.left, run.now)),
    }
}

/// The queue as it is: the one under way and its stage, those left, brought and out.
fn queue_said(ed: &Editor) -> String {
    let Some(run) = ed.bring_queue.run.as_ref() else { return "no queue".into() };
    let brought: Vec<&str> = run.brought.iter().map(|(n, _)| n.as_str()).collect();
    let out: Vec<String> = run.out.iter().map(|(n, why)| format!("{n}: {why}")).collect();
    format!("queue now {:?} left {:?} brought {brought:?} out {out:?}", run.now, run.left)
}
