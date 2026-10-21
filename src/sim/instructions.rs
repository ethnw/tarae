//! A session's instruction files following the main checkout's, simulated (frictions29 #27): the world of
//! `sim/sessions.rs`, `CLAUDE.md` copied into each session's checkout with its base kept, and at random a line
//! added to the main checkout's or to a session's, a look asked (as the session's claude starts), the card in
//! front answered `y` or `n`, a frame, a switch of sessions, the user taking their time — the looks' jobs among
//! them. After every step: no line a session wrote is gone from its file, and at most one card a file.

use std::path::PathBuf;
use std::time::Duration;

use super::sessions::{World, print};
use super::{Choices, Sim};
use crate::editor::Editor;
use crate::offer::What;

const SEEDS: &[u64] = &[0, 1];

const INPUTS: usize = 30;

const LIMIT: Duration = Duration::from_secs(30);

#[test]
fn instruction_files_follow() {
    super::seeds("instruction_files_follow", SEEDS, run);
}

fn run(choices: Choices) -> Vec<String> {
    let world = World::new();
    // (a run's own: a scratch folder's name comes again in the next run)
    MINE.with(|m| m.borrow_mut().clear());
    let mut sim = Sim::new("instruction_files_follow", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    (ed.session.root, ed.session.name) = (world.repo.clone(), "proj".into());
    std::fs::write(world.repo.join("CLAUDE.md"), "rule\n").unwrap();
    let mut ids = Vec::new();
    for (name, wt) in &world.sessions {
        std::fs::write(wt.join("CLAUDE.md"), "rule\n").unwrap();
        crate::instructions::keep_bases(&world.repo, wt, &["CLAUDE.md".into()]);
        let id = ed.session_open(wt).unwrap();
        if let Some(l) = ed.session_by_id(id) {
            l.name = name.clone();
        }
        ids.push((id, wt.clone()));
    }
    ed.session_switch(0);
    let dir = world.dir.display().to_string();
    let repo = world.repo.clone();
    let mut lines = 0;
    let roots: Vec<PathBuf> = ids.iter().map(|(_, wt)| wt.clone()).collect();
    sim.walk(
        &mut ed,
        INPUTS,
        |sim, ed| input(sim, ed, &ids, &repo, &mut lines),
        |ed| held(ed, &roots, &repo),
        |ed| print(ed, &dir),
    );
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

fn input(
    sim: &mut Sim,
    ed: &mut Editor,
    ids: &[(u64, PathBuf)],
    repo: &std::path::Path,
    n: &mut usize,
) -> String {
    let c = &mut sim.choices;
    let (sid, wt) = c.pick(ids).clone();
    // (by arm: the main checkout's line 2, a session's 2, a look 3, `y` 2, `n` 1, a frame 1, a switch 2, time 1,
    // its changes asked for the main checkout's — a bring, frictions29 #79 — 2)
    match c.weighted(&[2, 2, 3, 2, 1, 1, 2, 1, 2]) {
        0 => {
            *n += 1;
            let at_top = *c.pick(&[true, false]);
            add(&repo.join("CLAUDE.md"), &format!("main {n}"), at_top);
            format!("main: a line {}", if at_top { "on top" } else { "at the end" })
        }
        1 => {
            *n += 1;
            add(&wt.join("CLAUDE.md"), &format!("mine {n}"), false);
            format!("session {sid}: a line of its own")
        }
        2 => {
            ed.session_switch(sid);
            crate::instructions::follow(ed);
            format!("session {sid}: a look")
        }
        3 | 4 => {
            let key = if c.pick(&[0, 1, 2]) < &2 { 'y' } else { 'n' };
            match ed.offers.first().map(|o| matches!(o.what, What::Instructions(_))) {
                Some(true) if key == 'y' => ed.offer_accept(),
                Some(true) => ed.offer_dismiss(),
                _ => return format!("({key}: no card here)"),
            }
            key.to_string()
        }
        5 => {
            crate::term::render(ed, &mut Vec::new(), 112, 30).unwrap();
            "a frame".into()
        }
        6 => {
            let to = *c.pick(&[0, ids[0].0, ids[1].0]);
            ed.session_switch(to);
            format!("session {to} in front")
        }
        7 => {
            let ms = *c.pick(&[50, 300, 2000]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
        8 => {
            crate::instructions::carry_out(ed, wt);
            format!("session {sid}: its changes asked for the main checkout's")
        }
        _ => unreachable!("one arm a weight"),
    }
}

/// `line` added to the file at `path`, on top or at its end.
fn add(path: &std::path::Path, line: &str, at_top: bool) {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let text = if at_top { format!("{line}\n{text}") } else { format!("{text}{line}\n") };
    std::fs::write(path, text).unwrap();
}

/// No line a session wrote (`mine …`) is gone from its file, nor one the main checkout's had (`main …`) from
/// its — and at most one card a file and direction.
fn held(ed: &Editor, roots: &[PathBuf], repo: &std::path::Path) -> Result<(), String> {
    for root in roots {
        let text = std::fs::read_to_string(root.join("CLAUDE.md")).unwrap_or_default();
        let mine = text.lines().filter(|l| l.starts_with("mine ")).count();
        let written = MINE.with(|m| m.borrow().get(root).copied().unwrap_or(0));
        if mine < written {
            return Err(format!(
                "{}: {written} lines of its own written, {mine} left:\n{text}",
                root.display()
            ));
        }
        MINE.with(|m| m.borrow_mut().insert(root.clone(), mine));
    }
    let parked = std::iter::once(&ed.session).chain(&ed.sessions).flat_map(|l| &l.parked);
    // (and the main checkout's own lines — a session's carried in never takes one away: frictions29 #79)
    let text = std::fs::read_to_string(repo.join("CLAUDE.md")).unwrap_or_default();
    let mains = text.lines().filter(|l| l.starts_with("main ")).count();
    let written = MINE.with(|m| m.borrow().get(repo).copied().unwrap_or(0));
    if mains < written {
        return Err(format!("the main checkout: {written} lines of its own written, {mains} left:\n{text}"));
    }
    MINE.with(|m| m.borrow_mut().insert(repo.to_path_buf(), mains));
    let mut cards: Vec<(PathBuf, String, bool)> = ed
        .offers
        .iter()
        .chain(parked)
        .filter_map(|o| match &o.what {
            What::Instructions(b) => Some((b.root.clone(), b.rel.clone(), b.out.is_some())),
            _ => None,
        })
        .collect();
    cards.sort();
    match cards.windows(2).find(|w| w[0] == w[1]) {
        Some(w) => Err(format!("two cards for {}'s {}", w[0].0.display(), w[0].1)),
        None => Ok(()),
    }
}

thread_local! {
    /// The lines of its own each session's file had at the last check (a line is never taken away by the
    /// scenario itself: fewer is a loss).
    static MINE: std::cell::RefCell<std::collections::HashMap<PathBuf, usize>> = Default::default();
}
