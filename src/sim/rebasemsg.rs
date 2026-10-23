//! A resolved rebase's commit message, simulated (frictions29 #25): a session's rebase stopped at a conflict
//! its Claude resolved — and, by seed, proposed another message for — and at random its card put up (as
//! `:session-bring` does), `m` on the card in front (the message written to the git folder on a worker, then
//! opened), `:q` in that message, a frame, a switch of sessions, the user taking their time. After every step:
//! at most one resolved card a checkout, and a message being written is an open buffer. `:wq` runs
//! `git rebase --continue` as a task, which this scenario doesn't model (`worktree.rs`'s tests run it).

use std::path::PathBuf;
use std::time::Duration;

use super::sessions::{World, print};
use super::{Choices, Sim};
use crate::editor::Editor;
use crate::key::{Code, Key};
use crate::offer::What;

const SEEDS: &[u64] = &[0, 1];

const INPUTS: usize = 30;

const LIMIT: Duration = Duration::from_secs(30);

#[test]
fn a_resolved_rebases_message() {
    super::seeds("a_resolved_rebases_message", SEEDS, run);
}

fn run(choices: Choices) -> Vec<String> {
    let world = World::new();
    let mut sim = Sim::new("a_resolved_rebases_message", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    (ed.session.root, ed.session.name) = (world.repo.clone(), "proj".into());
    let mut ids = Vec::new();
    for (name, wt) in &world.sessions {
        let id = ed.session_open(wt).unwrap();
        ed.session_switch(id);
        ed.session.name = name.clone();
        let proposed = (name == "risky").then_some("risky, beside main\n\nKeeps both.");
        crate::rebase::resolved_for_test(&mut ed, name, proposed);
        ids.push((id, wt.clone()));
    }
    ed.session_switch(0);
    let dir = world.dir.display().to_string();
    sim.walk(&mut ed, INPUTS, |sim, ed| input(sim, ed, &ids), held, |ed| print(ed, &dir));
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

fn input(sim: &mut Sim, ed: &mut Editor, ids: &[(u64, PathBuf)]) -> String {
    let c = &mut sim.choices;
    let (sid, _) = c.pick(ids).clone();
    // (by arm: its card up 3, `m` on the card in front 3, `:q` 2, a frame 2, a switch 2, time passing 2, the
    // message's buffer closed another way 2)
    match c.weighted(&[3, 3, 2, 2, 2, 2, 2]) {
        0 => match ed.rebase_offer(sid) {
            Ok(()) => format!("session {sid}'s card up"),
            Err(e) => format!("session {sid}'s card: {e}"),
        },
        // (drawn and focused first — a press on it: a card that came on its own takes no key until then)
        1 => {
            crate::term::render(ed, &mut Vec::new(), 112, 30).unwrap();
            ed.card_focus_front();
            crate::term::render(ed, &mut Vec::new(), 112, 30).unwrap();
            ed.handle_key(Key::plain(Code::Char('m')));
            "m".into()
        }
        2 => match ed.in_rebase_message() {
            true => match ed.rebase_message_left() {
                Ok(()) => ":q in the message".into(),
                Err(e) => format!(":q in the message: {e}"),
            },
            false => "(no message in front)".into(),
        },
        3 => {
            crate::term::render(ed, &mut Vec::new(), 112, 30).unwrap();
            "a frame".into()
        }
        4 => {
            let to = *c.pick(&[0, ids[0].0, ids[1].0]);
            ed.session_switch(to);
            format!("session {to} in front")
        }
        5 => {
            let ms = *c.pick(&[50, 300, 2000]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
        // (as `:bd` or a session's end does — not `:w`/`:q`)
        6 => {
            let writing = std::iter::once(&ed.session)
                .chain(&ed.sessions)
                .find_map(|l| l.rebase.as_ref()?.messages.editing.as_ref().map(|e| (l.id, e.0)));
            match writing {
                Some((id, doc)) => {
                    ed.close_doc(doc);
                    format!("session {id}'s message buffer closed")
                }
                None => "(no message being written)".into(),
            }
        }
        _ => unreachable!("one arm a weight"),
    }
}

/// At most one resolved card a checkout, up or parked, in any session — and a message being written is a buffer
/// that's open.
fn held(ed: &Editor) -> Result<(), String> {
    let parked = std::iter::once(&ed.session).chain(&ed.sessions).flat_map(|l| &l.parked);
    let mut roots: Vec<&std::path::Path> = ed
        .offers
        .iter()
        .chain(parked)
        .filter_map(|o| match &o.what {
            What::RebaseResolved { root, .. } => Some(root.as_path()),
            _ => None,
        })
        .collect();
    roots.sort();
    if let Some(w) = roots.windows(2).find(|w| w[0] == w[1]) {
        return Err(format!("two resolved cards for {}", w[0].display()));
    }
    // (every session's: the documents are the editor's, whichever session holds them)
    for l in std::iter::once(&ed.session).chain(&ed.sessions) {
        let editing = l.rebase.as_ref().and_then(|r| r.messages.editing.as_ref());
        if let Some((doc, _)) = editing
            && !ed.docs.iter().any(|d| d.id == *doc)
        {
            return Err(format!("session {}'s message is written in buffer {doc:?}, not open", l.id));
        }
    }
    Ok(())
}
