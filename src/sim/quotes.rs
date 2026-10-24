//! Quoting, simulated (view P2): the keys that make quotes and take them off — the code's selection and `space >`,
//! the chat's view and `enter`, Backspace — a question typed and sent to a stand-in claude (no process), its turn
//! ending, frames, the user taking their time; the copy of what went with a message written by a job, its result
//! picked by the stream among the steps. After every step: the chips over the input are what goes, each once, within
//! the limits; a message sent carries each quote that waited — its block opened and closed once by its own id, never
//! an id twice; a copy, once written, holds the blocks its message carried; Claude's level never lists them.

use std::cell::RefCell;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::Editor;
use crate::key::{Code, Key};

/// The seeds every `cargo test` runs, each twice (no process, no git: a step is quick).
const SEEDS: &[u64] = &[0, 1, 2];

const INPUTS: usize = 70;

const LIMIT: Duration = crate::test_results::PATIENCE;

#[test]
fn quotes_go_with_the_message_once() {
    super::seeds("quotes_go_with_the_message_once", SEEDS, run);
}

/// What the scenario keeps between steps: the quotes a message would carry as the last input began (Claude idle: those
/// over the input; at work: those queued with a message, for its turn's end), the messages sent with their blocks,
/// every id seen.
#[derive(Default)]
struct World {
    waiting: RefCell<Vec<String>>,
    /// The scenario's own account of what the queued message carries: the quotes over the input at each Enter that
    /// queued (Claude at work) — never read from the chat's own.
    queued: RefCell<Vec<String>>,
    /// Claude was at work as the last input began: a message sent in it is the queued one.
    busy: std::cell::Cell<bool>,
    sent: RefCell<Vec<String>>,
    ids: RefCell<Vec<String>>,
}

fn run(choices: Choices) -> Vec<String> {
    let mut sim = Sim::new("quotes_go_with_the_message_once", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    ed.docs[0].text = ropey::Rope::from_str("fn main() {\n    let a = 1;\n    let b = 2;\n}\n");
    let mut c = crate::chat::Chat::new();
    c.msgs = vec![
        crate::chat::Msg::new(crate::chat::Role::User, "what does main do?", ""),
        crate::chat::Msg::new(crate::chat::Role::Assistant, "It sets two names.\n\nNothing else.", ""),
    ];
    let claude = crate::chat::stand_in(&mut c);
    ed.session.chat = Some(c);
    ed.session.at = crate::session::At::Chat;
    // (the copies of what went: in the run's own folder)
    let dir = crate::scratch::Scratch::new("sim-quotes");
    ed.sent_dir = Some(dir.join("sent"));
    crate::term::render(&mut ed, &mut Vec::new(), 120, 32).unwrap();
    let world = World::default();
    let check = |ed: &Editor| held(ed, &world, &claude);
    sim.walk(&mut ed, INPUTS, |sim, ed| input(sim, ed, &world), check, print);
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

fn keys(ed: &mut Editor, keys: &str) {
    let mut chars = keys.chars();
    while let Some(ch) = chars.next() {
        let k: Key = match ch {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            ch => Key::plain(Code::Char(ch)),
        };
        ed.handle_key(k);
    }
}

/// A step, and what a message would carry: Claude idle, the quotes over the input; at work, the queued message's —
/// and an Enter that queued one adds those over the input then to it.
fn input(sim: &mut Sim, ed: &mut Editor, world: &World) -> String {
    let before = ed.session.chat.as_ref().map(|c| {
        (c.busy(), c.queued.is_some(), c.queued_n, c.quotes.iter().map(|q| q.id.clone()).collect::<Vec<_>>())
    });
    let Some((busy, was, n, over)) = before else { return step(sim, ed) };
    world.busy.set(busy);
    *world.waiting.borrow_mut() = if busy { world.queued.borrow().clone() } else { over.clone() };
    let said = step(sim, ed);
    if let Some(c) = ed.session.chat.as_ref()
        && busy
        && c.queued.is_some()
        && (!was || c.queued_n != n)
    {
        // (one message's limits: the newest past them don't wait — the scenario's quotes are small, the count bites)
        let mut queued = world.queued.borrow_mut();
        queued.extend(over);
        queued.truncate(crate::quote::QUOTES_MAX);
    }
    said
}

fn step(sim: &mut Sim, ed: &mut Editor) -> String {
    let c = &mut sim.choices;
    // (by arm: the code's lines quoted 3, the chat's rows quoted 3, a key 4, a question 2, the turn's end 1, a frame
    // 2, time passing 1)
    match c.weighted(&[3, 3, 4, 2, 1, 2, 1]) {
        0 => {
            // (out of the chat's input and its view first: `esc esc`)
            let sel = *c.pick(&["x", "jx", "xC", "%", "jw"]);
            keys(ed, &format!("<esc><esc>{sel}<space>>"));
            format!("the code's {sel} quoted")
        }
        1 => {
            if ed.keys_to().region.name() != "chat" {
                return "(the chat's input hasn't the keys)".into();
            }
            let sel = *c.pick(&["x", "kx", "%", "ggxx", ""]);
            keys(ed, &format!("<esc>{sel}<ret>"));
            format!("the chat's view: {sel} enter")
        }
        2 => {
            let k = *c.pick(&[
                "j",
                "k",
                "x",
                "C",
                "%",
                "<space>>",
                "<esc>",
                "<ret>",
                "<backspace>",
                "i",
                "w",
                "<C-c>",
            ]);
            keys(ed, k);
            format!("keys {k}")
        }
        3 => {
            if ed.keys_to().region.name() != "chat" {
                return "(the chat's input hasn't the keys)".into();
            }
            keys(ed, "why?<ret>");
            "a question, sent".into()
        }
        4 => {
            crate::chat::stand_in_done(ed);
            "Claude's turn ends".into()
        }
        5 => {
            let w = *c.pick(&[100, 120]);
            crate::term::render(ed, &mut Vec::new(), w, 32).unwrap();
            format!("a frame, {w} columns")
        }
        _ => {
            let ms = *c.pick(&[100, 3000]);
            sim.wait(Duration::from_millis(ms));
            "time passes".into()
        }
    }
}

/// The quotes' blocks in a message: their ids, each checked opened and closed once.
fn blocks(content: &str) -> Result<Vec<String>, String> {
    let mut ids = Vec::new();
    for part in content.split("<quoted id=\"").skip(1) {
        let id = part.get(..6).ok_or("a cut id")?.to_string();
        let close = format!("</quoted id=\"{id}\">");
        if content.matches(&close).count() != 1 {
            return Err(format!("{id} isn't closed once: {content}"));
        }
        ids.push(id);
    }
    Ok(ids)
}

fn held(ed: &Editor, world: &World, claude: &Receiver<String>) -> Result<(), String> {
    let Some(c) = ed.session.chat.as_ref() else { return Err("the chat went".into()) };
    // Over the input: each thing once, within the limits
    let items = c.pointed.len() + usize::from(c.screen.is_some()) + c.quotes.len();
    if c.with_chips().len() != items {
        return Err(format!("chips {:?} for {items} things", c.with_chips()));
    }
    // (and a queued message's: one message's limits)
    for (what, q) in [("over the input", &c.quotes), ("queued", &c.queued_quotes)] {
        let bytes: usize = q.iter().map(|q| q.text.len()).sum();
        if q.len() > crate::quote::QUOTES_MAX || bytes > crate::quote::QUOTES_BYTES {
            return Err(format!("{} quotes {what}, {bytes} bytes", q.len()));
        }
    }
    // Sent: what waited went, each block whole, no id twice
    while let Ok(line) = claude.try_recv() {
        let v: serde_json::Value = serde_json::from_str(&line).map_err(|e| format!("{e}: {line}"))?;
        // (a message — not a control request: `C-c`'s stop)
        if v["type"] != "user" {
            continue;
        }
        let content = v["message"]["content"].as_str().unwrap_or_default().to_string();
        let ids = blocks(&content)?;
        let (mut went, mut waited) = (ids.clone(), world.waiting.borrow().clone());
        went.sort();
        waited.sort();
        if went != waited {
            return Err(format!("{waited:?} waited, {went:?} went: {content}"));
        }
        let mut seen = world.ids.borrow_mut();
        if let Some(id) = ids.iter().find(|id| seen.contains(id)) {
            return Err(format!("{id} went twice"));
        }
        let still = c.quotes.iter().chain(&c.queued_quotes).find(|q| ids.contains(&q.id));
        if let Some(q) = still {
            return Err(format!("{} still waits after it went", q.id));
        }
        seen.extend(ids);
        world.sent.borrow_mut().push(content);
        if world.busy.get() {
            world.queued.borrow_mut().clear();
        }
    }
    // A copy, once written, holds its message's blocks
    let sent = world.sent.borrow();
    let with_copy = c.msgs.iter().filter(|m| m.role == crate::chat::Role::User && m.attached.is_some());
    for (m, content) in with_copy.zip(sent.iter().filter(|s| s.contains("<quoted id="))) {
        let path = &m.attached.as_ref().expect("filtered").path;
        let Ok(kept) = std::fs::read_to_string(path) else { continue };
        for id in blocks(content)? {
            if !kept.contains(&format!("<quoted id=\"{id}\""))
                || !kept.contains(&format!("</quoted id=\"{id}\">"))
            {
                return Err(format!("the copy {} lacks {id}", path.display()));
            }
        }
    }
    // Unsent: Claude's level never lists it
    if ed.ui_state_claude().sessions.iter().any(|s| !s.with.is_empty()) {
        return Err("Claude's level lists what goes with the next message".into());
    }
    Ok(())
}

fn print(ed: &Editor) -> String {
    let c = ed.session.chat.as_ref();
    let chips = c.map(|c| c.with_chips().join(", ")).unwrap_or_default();
    let queued = c.map_or(0, |c| c.queued_quotes.len());
    format!(
        "{} · [{chips}] · {queued} queued · {} sent",
        ed.keys_to().region.name(),
        c.map_or(0, |c| c.msgs.len())
    )
}
