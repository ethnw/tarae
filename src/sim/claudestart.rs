//! A send while the chat's claude starts, simulated (frictions29 #67): a chat whose claude waits for the project's
//! Claude settings to be read (`claudetrust` — the read a job), and at random the chat opened, a reply written on
//! a line (`notes::submit`), a frame, the user taking their time — the read's job among them; its claude a stand-in
//! whose lines are kept (`Editor.claude_lines`). After every step: no reply refused ("couldn't reach claude"), each
//! reply written either in the claude's lines or waiting its turn (`Chat.sends`) — never lost — and a chat waiting
//! for its claude says so in its header (`starting`).

use std::cell::RefCell;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::Editor;

const SEEDS: &[u64] = &[0, 1];

const INPUTS: usize = 24;

const LIMIT: Duration = Duration::from_secs(30);

#[test]
fn a_send_while_claude_starts() {
    super::seeds("a_send_while_claude_starts", SEEDS, run);
}

/// What the checks keep: the replies written, and the lines the stand-in got.
#[derive(Default)]
struct Seen {
    replies: Vec<String>,
    lines: Vec<String>,
    /// A frame of a chat waiting for its claude that didn't say so.
    unsaid: Option<String>,
}

fn run(choices: Choices) -> Vec<String> {
    let dir = crate::scratch::Scratch::new("sim-claude-start");
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(root.join("a.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    let mut sim = Sim::new("a_send_while_claude_starts", choices, LIMIT);
    let mut ed = Editor::with_root(crate::config::Config::default(), root.clone());
    ed.claude_settings_gate = true;
    let (tx, rx) = std::sync::mpsc::channel();
    ed.claude_lines = Some(tx);
    ed.open(&root.join("a.rs")).unwrap();
    let seen = RefCell::new(Seen::default());
    sim.walk(
        &mut ed,
        INPUTS,
        |sim, ed| input(sim, ed, &root, &seen),
        |ed| held(ed, &rx, &mut seen.borrow_mut()),
        print,
    );
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

fn input(sim: &mut Sim, ed: &mut Editor, _root: &Path, seen: &RefCell<Seen>) -> String {
    let c = &mut sim.choices;
    // (by arm: the chat opened 2, a reply 4, a frame 1, time passing 1)
    match c.weighted(&[2, 4, 1, 1]) {
        0 => match ed.session.chat.is_some() {
            true => "(the chat is open)".into(),
            false => {
                crate::chat::open(ed);
                ed.session.leave(crate::session::At::Chat);
                "the chat opened".into()
            }
        },
        1 => {
            let n = seen.borrow().replies.len() + 1;
            let text = format!("why line {n}?");
            let line = *c.pick(&[0, 1]);
            ed.goto_line(line);
            crate::notes::submit(ed, None, &text);
            seen.borrow_mut().replies.push(text.clone());
            format!("a reply: {text}")
        }
        2 => {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 112, 30).unwrap();
            let mut vt = vt100::Parser::new(30, 112, 0);
            vt.process(&bytes);
            let starting = ed.session.chat.as_ref().is_some_and(|c| c.settings.waiting());
            if starting && !vt.screen().contents().contains(" starting ") {
                seen.borrow_mut().unsaid = Some(vt.screen().contents());
            }
            "a frame".into()
        }
        3 => {
            let ms = *c.pick(&[50, 300, 2000]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
        _ => unreachable!("one arm a weight"),
    }
}

/// No reply refused; each one the stand-in's or waiting its turn; a chat waiting for its claude says `starting`.
fn held(ed: &Editor, rx: &Receiver<String>, seen: &mut Seen) -> Result<(), String> {
    seen.lines.extend(rx.try_iter());
    if let Some(frame) = &seen.unsaid {
        return Err(format!("waiting for its claude, the chat didn't say so:\n{frame}"));
    }
    if let Some(t) = ed.toasts.iter().find(|t| t.text.contains("couldn't reach claude")) {
        return Err(format!("refused: {}", t.text));
    }
    let waits = |text: &str| {
        ed.session.chat.as_ref().is_some_and(|c| {
            c.sends.iter().any(|l| matches!(l, crate::chat::Later::Reply { text: t, .. } if t == text))
        })
    };
    for r in &seen.replies {
        if !seen.lines.iter().any(|l| l.contains(r.as_str())) && !waits(r) {
            return Err(format!("\"{r}\" neither sent nor waiting"));
        }
    }
    Ok(())
}

fn print(ed: &Editor) -> String {
    match ed.session.chat.as_ref() {
        None => "no chat".into(),
        Some(c) => format!(
            "chat: {}{}, {} waiting",
            if c.settings.waiting() { "starting" } else { "up" },
            if c.busy() { ", busy" } else { "" },
            c.sends.len()
        ),
    }
}
