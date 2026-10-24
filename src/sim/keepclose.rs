//! Workspaces kept close, simulated (collab C3, `keepclose.rs`): a repository on `main` and two workspaces on it —
//! `fix`, whose Claude (a stand-in) takes messages and has runs, and `up` — while `main` moves again and again, the
//! card is answered `y` or `n` or left, fix's turns start and end and its runs come and go, time passes and frames are
//! drawn: the overlap rounds, looks and moves on their workers landing among the steps.
//!
//! After every step: at most one card a base, on a tip read; while fix moves its Claude isn't at work and nothing runs
//! there (a message or a run asked meanwhile waits); a message waiting goes once nothing holds it. Once all is still,
//! a `y` given on the newest tip has both workspaces on it.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::Editor;
use crate::event::SessionId;
use crate::key::{Code, Key};
use crate::offer::What;

/// Two seeds, ~7 s each at opt-level 0 (2026-10-23, macOS): seed 7 asks a run in fix while it moves (of seeds 0–7 the
/// one that does — a move is in flight one job's time).
const SEEDS: &[u64] = &[0, 7];

const INPUTS: usize = 40;

const LIMIT: Duration = crate::test_results::PATIENCE;

#[test]
fn workspaces_follow_their_base_only_when_idle() {
    super::seeds("workspaces_follow_their_base_only_when_idle", SEEDS, run);
}

struct World {
    main: PathBuf,
    fix: SessionId,
    up: SessionId,
    base: crate::keepclose::Base,
    /// main's tips, oldest first — the last the newest.
    tips: Vec<String>,
    /// The tip a `y` was given on (cleared when main moves past it).
    yes: Option<String>,
    moves: usize,
    /// The stand-in claude's lines (kept: a dropped receiver fails its writes).
    _rx: std::sync::mpsc::Receiver<String>,
}

fn run(choices: Choices) -> Vec<String> {
    let mut sim = Sim::new("workspaces_follow_their_base_only_when_idle", choices, LIMIT);
    let world = super::sessions::World::new();
    let main = world.repo.clone();
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = std::fs::canonicalize(&main).unwrap();
    ed.session.name = "main".into();
    ed.session.git_branch = Some("main".into());
    ed.overlap.dir = Some(world.dir.join("overlap"));
    ed.overlap.quiet = Duration::ZERO;
    let mut ids = Vec::new();
    let mut rx = None;
    for (name, root) in &world.sessions {
        let id = ed.session_add(std::fs::canonicalize(root).unwrap());
        let l = ed.session_by_id(id).unwrap();
        l.base = Some("main".into());
        l.git_branch = Some(name.clone());
        l.name = if ids.is_empty() { "fix" } else { "up" }.into();
        if ids.is_empty() {
            let mut c = crate::chat::Chat::new();
            rx = Some(crate::chat::stand_in(&mut c));
            l.chat = Some(c);
        }
        ids.push(id);
    }
    let common = crate::gitrepo::repo(&main).unwrap().common_dir.clone();
    let base = (common, "main".to_string());
    let t = tip_of(&main);
    crate::keepclose::tip(&mut ed, base.clone(), t.clone());
    let w = RefCell::new(World {
        main,
        fix: ids[0],
        up: ids[1],
        base,
        tips: vec![t],
        yes: None,
        moves: 0,
        _rx: rx.unwrap(),
    });
    let check = |ed: &Editor| held(ed, &w.borrow());
    sim.walk(&mut ed, INPUTS, |sim, ed| step(sim, ed, &mut w.borrow_mut()), check, print);
    let trace = sim.trace().to_vec();
    if let Err(e) = still(&ed, &w.borrow()) {
        panic!("once all was still: {e}\n{}", trace.join("\n"));
    }
    drop(sim);
    drop(ed);
    drop(world);
    trace
}

fn tip_of(dir: &std::path::Path) -> String {
    let out =
        std::process::Command::new("git").args(["rev-parse", "main"]).current_dir(dir).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

fn session(ed: &Editor, id: SessionId) -> &crate::session::Session {
    std::iter::once(&ed.session).chain(&ed.sessions).find(|l| l.id == id).unwrap()
}

fn card(ed: &Editor) -> Option<&crate::offer::Offer> {
    ed.offers.iter().find(|o| matches!(o.what, What::BaseMoved(_)))
}

fn step(sim: &mut Sim, ed: &mut Editor, w: &mut World) -> String {
    let c = &mut sim.choices;
    // (by arm: main moves 3, a message to fix 3, fix's turn ends 2, a run in fix 3, y 4, n 1, time 1, a frame 1 — `y`
    // and fix's asks weighed up: a move is in flight one job's time, and they're what must meet it)
    match c.weighted(&[3, 3, 2, 3, 4, 1, 1, 1]) {
        0 => {
            w.moves += 1;
            let file = w.main.join("main.txt");
            std::fs::write(&file, format!("move {}\n", w.moves)).unwrap();
            super::sessions::git(&w.main, &["add", "main.txt"]);
            super::sessions::git(&w.main, &["commit", "-q", "-m", &format!("move {}", w.moves)]);
            let t = tip_of(&w.main);
            w.tips.push(t.clone());
            w.yes = None;
            crate::keepclose::tip(ed, w.base.clone(), t);
            format!("main moves ({})", w.moves)
        }
        1 => {
            let moving = session(ed, w.fix).moving.is_some();
            ed.with_session(w.fix, |ed| {
                if let Some(c) = ed.session.chat.as_mut() {
                    c.input.set("go on");
                }
                crate::chat::send(ed);
            });
            format!("a message to fix{}", if moving { " (it moves)" } else { "" })
        }
        2 => {
            if !session(ed, w.fix).chat.as_ref().is_some_and(crate::chat::Chat::busy) {
                return "(fix's Claude is idle)".into();
            }
            ed.with_session(w.fix, crate::chat::stand_in_done);
            "fix's turn ends".into()
        }
        3 => {
            let l = session(ed, w.fix);
            let running = l.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running);
            let moving = l.moving.is_some();
            let task = crate::task::Task { name: "build".into(), cmd: "true".into(), cwd: l.root.clone() };
            ed.with_session(w.fix, |ed| match (running, moving) {
                (true, _) => {
                    if let Some(r) = ed.session.task_run.as_mut() {
                        r.state = crate::task::State::Passed;
                    }
                    crate::queued::run_next(ed);
                }
                // (asked while it moves: it must wait — then taken out of the line, as `:procs`' `s` does: this
                // scenario runs no process)
                (false, true) => {
                    ed.task_start(task);
                    let ran =
                        ed.session.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running);
                    assert!(!ran, "a run started while fix moved");
                    let id = ed.session.runs_next.front().map(|n| n.id).expect("the run waits in line");
                    crate::queued::leave(ed, id, "the scenario's");
                }
                (false, false) => {
                    let mut r = crate::task::Run::for_test(task);
                    r.state = crate::task::State::Running;
                    ed.session.task_run = Some(r);
                }
            });
            match (running, moving) {
                (true, _) => "fix's run ends".into(),
                (false, true) => "a run asked in fix while it moves".into(),
                (false, false) => "a run starts in fix".into(),
            }
        }
        k @ (4 | 5) => {
            let Some(to) = card(ed).map(|o| match &o.what {
                What::BaseMoved(m) => m.to.clone(),
                _ => unreachable!(),
            }) else {
                return "(no card)".into();
            };
            let key = if k == 4 { 'y' } else { 'n' };
            crate::term::render(ed, &mut Vec::new(), 120, 40).unwrap();
            let o = card(ed).map(|o| (o.id, o.rev)).unwrap();
            ed.card_focus = Some((o.0, o.1));
            ed.handle_key(Key::plain(Code::Char(key)));
            if key == 'y' && w.tips.last() == Some(&to) {
                w.yes = Some(to);
            }
            format!("{key} on the card")
        }
        6 => {
            let ms = *c.pick(&[100, 1500]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
        _ => {
            crate::term::render(ed, &mut Vec::new(), 100, 30).unwrap();
            "a frame".into()
        }
    }
}

fn held(ed: &Editor, w: &World) -> Result<(), String> {
    let cards: Vec<&crate::offer::Offer> =
        ed.offers.iter().filter(|o| matches!(o.what, What::BaseMoved(_))).collect();
    if cards.len() > 1 {
        return Err(format!("{} base-moved cards", cards.len()));
    }
    if let Some(What::BaseMoved(m)) = cards.first().map(|o| &o.what)
        && !w.tips.contains(&m.to)
    {
        return Err(format!("the card's tip {} was never read", m.to));
    }
    let fix = session(ed, w.fix);
    if fix.moving.is_some() {
        if fix.chat.as_ref().is_some_and(crate::chat::Chat::busy) {
            return Err("fix moves while its Claude is at work".into());
        }
        if fix.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running) {
            return Err("fix moves while a run is under way there".into());
        }
    }
    if fix.chat.as_ref().is_some_and(|c| c.queued.is_some() && !c.busy()) && fix.moving.is_none() {
        return Err("a message waits with nothing holding it".into());
    }
    if session(ed, w.up).moving.is_some() && fix.moving.is_some() {
        return Err("two moves at once".into());
    }
    Ok(())
}

/// Once still: a `y` on the newest tip, nothing busy in fix — both on it.
fn still(ed: &Editor, w: &World) -> Result<(), String> {
    let Some(yes) = &w.yes else { return Ok(()) };
    let busy = crate::keepclose::busy(ed, session(ed, w.fix));
    if ed.keep_close.moving.is_some() || busy.is_some() {
        return Ok(());
    }
    for id in [w.fix, w.up] {
        let l = session(ed, id);
        let on = std::process::Command::new("git")
            .args(["merge-base", "--is-ancestor", yes, "HEAD"])
            .current_dir(&l.root)
            .status()
            .unwrap()
            .success();
        if !on {
            return Err(format!("{} isn't on the tip its y was given on", l.name));
        }
    }
    Ok(())
}

fn print(ed: &Editor) -> String {
    let moving = ed.keep_close.moving.map(|id| format!(" · moving {id}")).unwrap_or_default();
    format!(
        "card {} · waits {} · moved {}{moving} · pending {} · started {}",
        card(ed).map_or("none".into(), |o| format!("#{}", o.id)),
        ed.keep_close.waits.len(),
        ed.keep_close.moved,
        ed.keep_close.said(),
        ed.overlap.started,
    )
}
