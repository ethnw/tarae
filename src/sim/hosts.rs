//! A host card through its run's end, simulated (frictions29 16): a session's runs of Claude's start and end at
//! random, the proxy asks about a host for the run going (or for one over, through what it left), a frame is
//! drawn, the user takes their time, the card in front is answered `y` · `s` · `n` once it's settled — each step's
//! jobs and their results interleaved by seed. After every step: one card a host, a card for a run that ended
//! says so (or is a left process's), a card says its run ended only when that run isn't going, and only a `y` on
//! such a card puts a host on the next run's list.

use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::Editor;
use crate::key::{Code, Key};
use crate::offer::Offer;
use crate::sandbox::proxy::{FromProxy, session_of};

/// The seeds every `cargo test` runs, each twice (no git, no process: a run is quick).
const SEEDS: &[u64] = &[0, 1, 2, 3];

/// The scenario's own steps a run.
const INPUTS: usize = 60;

/// A run's real time at most.
const LIMIT: Duration = Duration::from_secs(60);

const HOSTS: [&str; 2] = ["registry.example.invalid", "docs.example.invalid"];

#[test]
fn a_host_card_through_its_runs_end() {
    super::seeds("a_host_card_through_its_runs_end", SEEDS, run);
}

/// One run of the scenario; its trace.
fn run(choices: Choices) -> Vec<String> {
    let dir = crate::scratch::Scratch::new("sim-hosts");
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let mut sim = Sim::new("a_host_card_through_its_runs_end", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    (ed.session.root, ed.session.name) = (root.clone(), "proj".into());
    ed.allow_path = Some(root.join("state/sandbox.json"));
    // (the conversation whose runs they are: an ended card's `y` is its next run's)
    ed.session.chat = Some(crate::chat::Chat::new());
    let granted = std::cell::RefCell::new(Vec::new());
    let check = |ed: &Editor| held(ed, &granted.borrow());
    let mut runs = Vec::new();
    sim.walk(&mut ed, INPUTS, |sim, ed| input(sim, ed, &root, &mut runs, &granted), check, print);
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

/// A run of Claude's going in the session (no process), sandboxed with the network closed: its policy's run id.
fn start(ed: &mut Editor, root: &std::path::Path) -> u64 {
    let env = crate::sandbox::Env { root: root.to_path_buf(), network: false, ..Default::default() };
    let p = crate::sandbox::Policy::for_project(&env);
    let id = p.run_id;
    let task = crate::task::Task { name: "npm install".into(), cmd: "npm install".into(), cwd: root.into() };
    let mut r = crate::task::Run::for_test(task);
    (r.by_claude, r.state) = (true, crate::task::State::Running);
    r.guard = Some(crate::sandbox::Guard::In(Box::new(p)));
    ed.session.task_run = Some(r);
    id
}

/// The run going now, if one is (its id).
fn going(ed: &Editor) -> Option<u64> {
    let r = ed.session.task_run.as_ref().filter(|r| r.state == crate::task::State::Running)?;
    match r.guard.as_ref() {
        Some(crate::sandbox::Guard::In(p)) => Some(p.run_id),
        _ => None,
    }
}

/// One of the scenario's steps, said.
fn input(
    sim: &mut Sim,
    ed: &mut Editor,
    root: &std::path::Path,
    runs: &mut Vec<u64>,
    granted: &std::cell::RefCell<Vec<(String, u16)>>,
) -> String {
    let c = &mut sim.choices;
    // (by arm: a run starts 2, the proxy asks 4, the run ends 2, a frame 2, time passing 1, an answer 3)
    match c.weighted(&[2, 4, 2, 2, 1, 3]) {
        0 if going(ed).is_none() => {
            runs.push(start(ed, root));
            format!("run {} starts", runs.len())
        }
        0 => "(a run is going)".into(),
        1 => {
            let Some(&run) = going(ed).as_ref().or(runs.last()) else { return "(no run yet)".into() };
            let host = *c.pick(&HOSTS);
            let ask = FromProxy::Ask {
                session: session_of(root),
                run,
                host: host.into(),
                port: 443,
                process: "npm".into(),
            };
            crate::hosts::asked(ed, ask);
            // (runs by their place in this run of the scenario: their ids come from a counter the seeds share)
            let n = runs.iter().position(|r| *r == run).map_or(0, |i| i + 1);
            format!("the proxy asks for {host} (run {n})")
        }
        2 => match going(ed) {
            Some(run) => {
                if let Some(r) = ed.session.task_run.as_mut() {
                    r.state = crate::task::State::Failed(Some(1));
                }
                crate::hosts::run_ended(ed, Some(run));
                format!("run {} ends", runs.len())
            }
            None => "(no run going)".into(),
        },
        3 => {
            crate::term::render(ed, &mut Vec::new(), 112, 30).unwrap();
            "a frame".into()
        }
        4 => {
            let ms = *c.pick(&[300, 2000]);
            sim.wait(Duration::from_millis(ms));
            "time passes".into()
        }
        _ => {
            crate::term::render(ed, &mut Vec::new(), 112, 30).unwrap();
            sim.wait(crate::offer::SETTLE + Duration::from_millis(100));
            crate::term::render(ed, &mut Vec::new(), 112, 30).unwrap();
            let key = *sim.choices.pick(&['y', 'y', 's', 'n']);
            // (a `y` on a card whose run ended: what it lets the next run reach)
            if let Some(a) = ed.offers.first().and_then(Offer::host).filter(|a| a.ended && key == 'y') {
                let mut g = granted.borrow_mut();
                g.extend(std::iter::once(&a.host).chain(&a.also).map(|h| (h.clone(), a.port)));
            }
            if !ed.offers.is_empty() && !ed.card_has_keys() {
                ed.handle_key("C-q".parse().unwrap());
            }
            ed.handle_key(Key::plain(Code::Char(key)));
            format!("{key} on the card")
        }
    }
}

/// The host cards, up and parked.
fn cards(ed: &Editor) -> impl Iterator<Item = &crate::hosts::Ask> {
    ed.offers.iter().chain(&ed.session.parked).filter_map(Offer::host)
}

fn held(ed: &Editor, granted: &[(String, u16)]) -> Result<(), String> {
    let mut seen = Vec::new();
    for a in cards(ed) {
        if seen.contains(&a.at()) {
            return Err(format!("two cards for {}", a.at()));
        }
        seen.push(a.at());
        let runs = going(ed) == Some(a.run);
        if a.ended && runs {
            return Err(format!("{}'s card says its run ended while run {} goes", a.at(), a.run));
        }
        if !a.ended && !a.left && !runs {
            return Err(format!("{}'s card for run {}, over, doesn't say so", a.at(), a.run));
        }
    }
    let next = ed.session.chat.as_ref().map_or(&[][..], |c| &c.hosts_next[..]);
    if let Some(h) = next.iter().find(|h| !granted.contains(h)) {
        return Err(format!("{}:{} on the next run's list with no `y` on an ended card", h.0, h.1));
    }
    Ok(())
}

/// What a trace line ends with: the run going, the host cards (`ended` marked), the next run's hosts.
fn print(ed: &Editor) -> String {
    let cards: Vec<String> =
        cards(ed).map(|a| format!("{}{}", a.host, if a.ended { " (ended)" } else { "" })).collect();
    let next: Vec<&str> = ed.session.chat.iter().flat_map(|c| &c.hosts_next).map(|h| h.0.as_str()).collect();
    let going = if going(ed).is_some() { "a run going" } else { "none going" };
    format!("{going} · cards [{}] · next [{}]", cards.join(", "), next.join(", "))
}
