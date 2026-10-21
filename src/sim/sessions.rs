//! Sessions and their bring cards, simulated (detsim P1): two sessions with commits of their own, and at random
//! the card a rebase leaves pushed (as `rebase_ended` does), `:session-bring`, a frame drawn, `n` or `y` on the
//! card in front, a switch of sessions, the user taking their time, a new commit in a session — each step's jobs
//! and their results interleaved by seed. After every step: one bring card a session (frictions28 S1,
//! `b56cdf41` — the card a rebase left and a later bring's stood side by side). A landing that grades the others
//! again is `sim/brings.rs`'s.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::Editor;
use crate::key::{Code, Key};
use crate::offer::{Offer, What};

/// The seeds every `cargo test` runs, each twice — as many as fit the scenario's share of the suite's time at
/// opt-level 0: 36 steps a second there (2026-10-18, macOS, `TARAE_SIM_SEEDS=4`: 832 steps in 23.1 s — git's
/// processes most of it), ~208 steps a run, so a seed is ~12 s; two keep it under the 30 s the plan gives the
/// simulations. Seed 0 finds two bring cards in 33 steps with `b56cdf41`'s fix taken out.
const SEEDS: &[u64] = &[0, 1];

/// The scenario's own steps a run (the jobs' and results' come between them).
const INPUTS: usize = 40;

/// A run's real time at most.
const LIMIT: Duration = Duration::from_secs(60);

/// Commits made by the scenario carry this date, so two runs of a seed make the same commits.
const DATE: &str = "2026-10-18T00:00:00Z";

#[test]
fn sessions_and_bring_cards() {
    super::seeds("sessions_and_bring_cards", SEEDS, run);
}

/// A repository on `main` and two sessions' checkouts beside it: `risky` (a commit a person must look at:
/// its bring card is held) and `plain`.
pub(super) struct World {
    pub(super) dir: crate::scratch::Scratch,
    pub(super) repo: PathBuf,
    pub(super) sessions: Vec<(String, PathBuf)>,
}

impl World {
    pub(super) fn new() -> World {
        let dir = crate::scratch::Scratch::new("sim-sessions");
        let repo = dir.join("proj");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("a.txt"), "one\n").unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
            &["config", "commit.gpgsign", "false"],
            &["add", "-A"],
            &["commit", "-q", "-m", "base"],
        ] {
            git(&repo, args);
        }
        let mut sessions = Vec::new();
        for (name, body) in [("risky", "fn a() {\n    unsafe { b() }\n}\n"), ("plain", "fn a() {}\n")] {
            let wt = dir.join("wt").join(name);
            git(&repo, &["worktree", "add", "-q", "-b", name, &wt.display().to_string(), "main"]);
            std::fs::write(wt.join("a.rs"), body).unwrap();
            git(&wt, &["add", "a.rs"]);
            git(&wt, &["commit", "-q", "-m", name]);
            sessions.push((name.to_string(), wt));
        }
        World { dir, repo, sessions }
    }
}

/// The scenario's own git (the world's changes, not tarae's): the user's global and system settings left out, and
/// their global ignore and attributes in the default place too (`$XDG_CONFIG_HOME/git/`, read with no config
/// at all) — two runs of a seed see the same git, wherever they run — and no hook, the repository's or anyone's.
/// The board's and the review record's scenarios' too (`sim/board.rs`, `sim/reviews.rs`).
pub(super) fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args(["-c", "core.hooksPath=/dev/null", "-C"])
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("XDG_CONFIG_HOME", "/dev/null")
        .env("GIT_AUTHOR_DATE", DATE)
        .env("GIT_COMMITTER_DATE", DATE)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// Review note 7: the scenario's commits run no hook — here the repository's own `core.hooksPath`, whose
/// pre-commit is a stand-in that leaves a marker in the test's own folder (the global and system settings are
/// left out the same way, by `git`'s environment).
#[test]
fn the_scenarios_git_runs_no_hook() {
    let dir = crate::scratch::Scratch::new("sim-git-hooks");
    let (repo, marker) = (dir.join("r"), dir.join("hook-ran"));
    std::fs::create_dir_all(repo.join("hooks")).unwrap();
    crate::scratch::write_script(
        &repo.join("hooks/pre-commit"),
        &format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
    );
    for args in [
        &["init", "-q", "-b", "main"][..],
        &["config", "user.email", "t@t"],
        &["config", "user.name", "t"],
        &["config", "commit.gpgsign", "false"],
        &["config", "core.hooksPath", "hooks"],
        &["commit", "-q", "--allow-empty", "-m", "one"],
    ] {
        git(&repo, args);
    }
    assert!(!marker.exists(), "the pre-commit hook ran");
}

/// One run of the scenario; its trace.
fn run(choices: Choices) -> Vec<String> {
    let world = World::new();
    let mut sim = Sim::new("sessions_and_bring_cards", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    (ed.session.root, ed.session.name) = (world.repo.clone(), "proj".into());
    ed.session.git_branch = Some("main".into());
    ed.config.session_worktrees = world.dir.join("wt").display().to_string();
    let mut ids = Vec::new();
    for (name, wt) in &world.sessions {
        let id = ed.session_open(wt).unwrap();
        if let Some(l) = ed.session_by_id(id) {
            (l.name, l.branch, l.base) = (name.clone(), Some(name.clone()), Some("main".into()));
        }
        ids.push((id, name.clone(), wt.clone()));
    }
    ed.session_switch(0);
    let dir = world.dir.display().to_string();
    sim.walk(&mut ed, INPUTS, |sim, ed| input(sim, ed, &ids), one_bring_card, |ed| print(ed, &dir));
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

/// One of the scenario's steps, said.
fn input(sim: &mut Sim, ed: &mut Editor, ids: &[(u64, String, PathBuf)]) -> String {
    let c = &mut sim.choices;
    let (sid, name, root) = c.pick(ids).clone();
    // (by arm: a rebase's card 3, a bring 3, a frame 3, `n` at once 1, `y` after a frame 1, a switch 2, time
    // passing 2, a commit 1, a card read and answered 3)
    match c.weighted(&[3, 3, 3, 1, 1, 2, 2, 1, 3]) {
        // The card a rebase leaves (`rebase_ended`): "Rebased … onto main", graded on a worker
        0 => {
            let m = crate::rebase::Moved {
                root: root.clone(),
                name: name.clone(),
                branch: name.clone(),
                base: "main".into(),
                moved: 0,
                ahead: 1,
            };
            crate::codereview::push_bring_card(ed, sid, m.bring_card(None, true), root, "main".into());
            format!("{name} rebased")
        }
        1 => match ed.session_bring(&name) {
            Ok(()) => format!(":session-bring {name}"),
            Err(e) => format!(":session-bring {name}: {e}"),
        },
        2 => {
            crate::term::render(ed, &mut Vec::new(), 112, 30).unwrap();
            "a frame".into()
        }
        // A key pressed at once (taken only by a card drawn and settled)
        3 => {
            ed.handle_key(Key::plain(Code::Char('n')));
            "n".into()
        }
        4 => answer(ed, 'y'),
        5 => {
            let to = *c.pick(&[0, ids[0].0, ids[1].0]);
            ed.session_switch(to);
            format!("session {to} in front")
        }
        6 => {
            let ms = *c.pick(&[50, 300, 600, 2000]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
        7 => {
            let file = format!("c{}.rs", sim.steps);
            let body = if name == "risky" { "fn c() {\n    unsafe { d() }\n}\n" } else { "fn c() {}\n" };
            std::fs::write(root.join(&file), body).unwrap();
            git(&root, &["add", &file]);
            git(&root, &["commit", "-q", "-m", &file]);
            format!("{name}: a commit")
        }
        // The card in front read, then answered: drawn, in sight past `SETTLE`, drawn again, its key
        8 => {
            crate::term::render(ed, &mut Vec::new(), 112, 30).unwrap();
            sim.wait(crate::offer::SETTLE + Duration::from_millis(100));
            let key = *sim.choices.pick(&['n', 'n', 'y']);
            format!("(read) {}", answer(ed, key))
        }
        _ => unreachable!("one arm a weight"),
    }
}

/// `key` on the card in front, after a frame — `y` only on what brings or moves up: a rebase card's `y` runs
/// `git rebase` as a task, which this scenario doesn't model.
fn answer(ed: &mut Editor, key: char) -> String {
    let brings = |o: &Offer| matches!(o.what, What::Rebased(_) | What::UpTo(_));
    if key == 'y' && !ed.offers.first().is_some_and(brings) {
        return "(no y here)".into();
    }
    crate::term::render(ed, &mut Vec::new(), 112, 30).unwrap();
    ed.handle_key(Key::plain(Code::Char(key)));
    key.to_string()
}

/// The cards up and parked, by every session.
fn cards(ed: &Editor) -> impl Iterator<Item = &Offer> {
    let parked = std::iter::once(&ed.session).chain(&ed.sessions).flat_map(|l| &l.parked);
    ed.offers.iter().chain(parked)
}

/// frictions28 S1: at most one bring card for a session's checkout, up or parked, in any session.
pub(super) fn one_bring_card(ed: &Editor) -> Result<(), String> {
    let mut by_root: BTreeMap<&Path, Vec<String>> = BTreeMap::new();
    for o in cards(ed) {
        if let Some(root) = o.bring_root() {
            by_root.entry(root).or_default().push(format!("card#{} {:?}", o.id, o.title));
        }
    }
    match by_root.into_iter().find(|(_, v)| v.len() > 1) {
        Some((root, v)) => Err(format!("{} bring cards for {}: {}", v.len(), root.display(), v.join(", "))),
        None => Ok(()),
    }
}

/// What a trace line ends with: the session in front, the cards up and parked, the last thing said.
pub(super) fn print(ed: &Editor, dir: &str) -> String {
    let said = |os: &[Offer]| {
        let held = |o: &Offer| if o.held.is_some() { " (held)" } else { "" };
        os.iter().map(|o| format!("#{} {}{}", o.id, o.title, held(o))).collect::<Vec<_>>().join(", ")
    };
    let parked: Vec<String> = std::iter::once(&ed.session)
        .chain(&ed.sessions)
        .filter(|l| !l.parked.is_empty())
        .map(|l| format!("{}: {}", l.id, said(&l.parked)))
        .collect();
    let last = ed.messages.last().map(|m| m.2.replace(dir, "<dir>")).unwrap_or_default();
    format!(
        "front {} · up [{}] · parked [{}] · said {last:?}",
        ed.session.id,
        said(&ed.offers),
        parked.join("; ")
    )
}
