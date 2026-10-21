//! The terminal's view mode, simulated (view P1b): a program printing at random (now and then asking what the
//! terminal is), the keys of the round (`C-\`, `i`, `esc`, `q`, `space T`) and of view mode (`k`, `gg`, `G`, `x`,
//! `y` — its copy a job), the wheel up and down over its rows, frames, the user taking their time. After every
//! step: the rows on the panel are the view's document's (held still: nothing printed moves them), the output is
//! held exactly while it's viewed — no byte past one batch taken in, the program's next write waiting on the
//! go-ahead it's kept (a real terminal's flow control; the scenario's program is the reader, a batch only once the
//! last one's go-ahead came) — never waiting when not viewed, and every question the program asked answered.

use std::cell::{Cell, RefCell};
use std::sync::mpsc::Receiver;
use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::Editor;
use crate::event::MouseKind;
use crate::key::Key;

const SEEDS: &[u64] = &[0, 1, 2];

const INPUTS: usize = 60;

const LIMIT: Duration = Duration::from_secs(60);

/// What the terminal answers its program's `CSI c`.
const DA1: &[u8] = b"\x1b[?62;22c";

#[test]
fn the_terminals_view_mode() {
    super::seeds("the_terminals_view_mode", SEEDS, run);
}

/// What the scenario keeps between steps: the questions the program asked, the answers sent, lines printed.
#[derive(Default)]
struct World {
    asked: Cell<usize>,
    answered: Cell<usize>,
    printed: Cell<usize>,
    /// The view's document as last checked: its id (another when it's made again) and its text's hash.
    seen: Cell<Option<(crate::document::DocId, u64)>>,
    /// The last batch's go-ahead (`terminal::output_acked`), whether it came, and the batch's length.
    go: RefCell<Option<Receiver<()>>>,
    gone_ahead: Cell<bool>,
    last: Cell<usize>,
    /// A batch the reader read while it waits for that go-ahead: its questions answered, not handed on yet.
    in_reader: RefCell<Option<String>>,
}

/// The reader hands a batch on (`terminal::output_acked`), a new go-ahead to wait for.
fn hand(ed: &mut Editor, world: &World, generation: u64, text: &str) {
    let (done, go) = std::sync::mpsc::sync_channel(1);
    *world.go.borrow_mut() = Some(go);
    world.last.set(text.len());
    world.gone_ahead.set(false);
    crate::terminal::output_acked(ed, generation, text.as_bytes(), done);
    ed.focus_sync();
}

/// Whether the reader may send its next batch: the last one's go-ahead came (kept once it does).
fn went_ahead(world: &World) -> bool {
    if !world.gone_ahead.get() && world.go.borrow().as_ref().is_none_or(|go| go.try_recv().is_ok()) {
        world.gone_ahead.set(true);
    }
    world.gone_ahead.get()
}

fn run(choices: Choices) -> Vec<String> {
    let mut sim = Sim::new("the_terminals_view_mode", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    ed.docs[0].text = ropey::Rope::from_str("fn main() {}\n");
    let sent = crate::terminal::stand_in(&mut ed, 8, 60, 200);
    crate::term::render(&mut ed, &mut Vec::new(), 100, 30).unwrap();
    let world = World::default();
    let check = |ed: &Editor| {
        // (what the program got: its answers counted)
        while let Ok(b) = sent.try_recv() {
            world.answered.set(world.answered.get() + b.windows(DA1.len()).filter(|w| *w == DA1).count());
        }
        held(ed, &world)
    };
    sim.walk(&mut ed, INPUTS, |sim, ed| input(sim, ed, &world), check, print);
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    drain(&sent);
    trace
}

fn drain(sent: &Receiver<Vec<u8>>) {
    while sent.try_recv().is_ok() {}
}

fn key(s: &str) -> Key {
    s.parse().unwrap()
}

fn input(sim: &mut Sim, ed: &mut Editor, world: &World) -> String {
    let c = &mut sim.choices;
    // (by arm: the program prints 4, a key 6, the wheel 2, a frame 2, time passing 1)
    match c.weighted(&[4, 6, 2, 2, 1]) {
        0 => {
            let Some(generation) = ed.session.terminal.as_ref().map(|t| t.generation()) else {
                return "(no terminal)".into();
            };
            // (the reader, as `spawn`'s: a batch is answered as it's read, then handed on once the last one's
            // go-ahead came — while it waits, it holds that one batch and the program's next write waits)
            if went_ahead(world) {
                let held = world.in_reader.borrow_mut().take();
                if let Some(text) = held {
                    hand(ed, world, generation, &text);
                    return "the reader hands on the batch it read".into();
                }
            } else if world.in_reader.borrow().is_some() {
                return "(the program's write waits: the reader holds a batch, its go-ahead kept)".into();
            }
            let lines = *c.pick(&[1, 3, 12]);
            let ask = c.below(4) == 0;
            let mut text = String::new();
            for _ in 0..lines {
                world.printed.set(world.printed.get() + 1);
                text.push_str(&format!("out {}\r\n", world.printed.get()));
            }
            if ask {
                text.push_str("\x1b[c");
                world.asked.set(world.asked.get() + 1);
            }
            text.push_str("$ ");
            crate::terminal::reader_reads(ed, text.as_bytes());
            let asks = if ask { ", asks what the terminal is" } else { "" };
            if went_ahead(world) {
                hand(ed, world, generation, &text);
                return format!("the program prints {lines} lines{asks}");
            }
            *world.in_reader.borrow_mut() = Some(text);
            format!(
                "the program prints {lines} lines{asks} — the reader holds them, waiting for the go-ahead"
            )
        }
        1 => {
            let keys = *c.pick(&[
                "<C-\\>", "<C-\\>", "k", "k", "gg", "G", "x", "y", "i", "<esc>", "q", "<space>T", "<C-w>j",
                "j", "a",
            ]);
            let mut chars = keys.chars();
            while let Some(ch) = chars.next() {
                let k = match ch {
                    '<' => key(&chars.by_ref().take_while(|&c| c != '>').collect::<String>()),
                    ch => Key::plain(crate::key::Code::Char(ch)),
                };
                ed.handle_key(k);
            }
            format!("keys {keys}")
        }
        2 => {
            let up = c.below(2) == 0;
            let row = c.below(8) as u16;
            let kind = if up { MouseKind::ScrollUp } else { MouseKind::ScrollDown };
            if !ed.session.terminal.as_ref().is_some_and(|t| t.visible) {
                return "(the terminal's hidden: no wheel over it)".into();
            }
            ed.terminal_mouse(kind, Some((row, 2)), false);
            ed.focus_sync();
            format!("the wheel {} over row {row}", if up { "up" } else { "down" })
        }
        3 => {
            let h = *c.pick(&[30, 36]);
            crate::term::render(ed, &mut Vec::new(), 100, h).unwrap();
            format!("a frame, {h} rows")
        }
        _ => {
            let ms = *c.pick(&[100, 2000]);
            sim.wait(Duration::from_millis(ms));
            "time passes".into()
        }
    }
}

/// The program's row as the document has it (`view::cell_len`'s count: control characters a space, blanks at
/// its end off).
fn row_text(row: &str) -> String {
    let s: String = row.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    s.trim_end().to_string()
}

fn held(ed: &Editor, world: &World) -> Result<(), String> {
    let Some(t) = ed.session.terminal.as_ref() else { return Ok(()) };
    let viewed = crate::view::terminal_viewed(ed);
    if t.holding() != viewed {
        return Err(format!("output held {} while viewed {viewed}", t.holding()));
    }
    // (viewed: no byte past one batch taken in, and the program's write waiting while something's held; not
    // viewed: it never waits)
    if viewed && t.held_len() > world.last.get() {
        return Err(format!("{} bytes held, past one batch ({})", t.held_len(), world.last.get()));
    }
    if viewed && t.came() && went_ahead(world) {
        return Err("output held, and the program let go on".into());
    }
    if !viewed && !went_ahead(world) {
        return Err("not viewed, and the program's write still waits".into());
    }
    if world.answered.get() != world.asked.get() {
        return Err(format!("{} questions asked, {} answered", world.asked.get(), world.answered.get()));
    }
    if let Some(v) = crate::view::terminal_view_of(ed) {
        // (held still: the same view's document never changes — only a new one, made by catching up)
        let d = v.doc();
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            d.text.to_string().hash(&mut h);
            h.finish()
        };
        if let Some((id, was)) = world.seen.get()
            && id == d.id
            && was != hash
        {
            return Err("the view's document changed under it".into());
        }
        world.seen.set(Some((d.id, hash)));
        // (the rows on the panel are the document's — what its cursor and selections are painted on; a new panel
        // size not settled yet is read at its second frame, `view::terminal_resized`)
        let screen = t.parser.screen();
        if crate::view::terminal_settling(ed) {
            return Ok(());
        }
        let (_, cols) = screen.size();
        for (r, row) in screen.rows(0, cols).enumerate() {
            let line = d.top + r;
            let doc = d.text.get_line(line).map(|l| l.to_string().trim_end_matches('\n').to_string());
            if doc.as_deref() != Some(row_text(&row).as_str()) {
                return Err(format!("row {r}: on the panel {row:?}, the document's line {line} {doc:?}"));
            }
        }
        if !(ed.session.is_at(crate::session::At::Slot) && ed.slot() == Some(crate::sizes::Slot::Terminal)) {
            return Err("viewed, its panel without tarae's keys".into());
        }
    }
    Ok(())
}

fn print(ed: &Editor) -> String {
    let t = ed.session.terminal.as_ref();
    let keys = ed.keys_to().region.name();
    let view = crate::view::terminal_view_of(ed).map(|v| {
        let d = v.doc();
        let line = crate::movement::line_of(&d.text, d.selection().primary().cursor(&d.text));
        format!("view at {}/{}", line + 1, v.rows())
    });
    format!(
        "keys {keys} · {} · held {} · shown {}",
        view.unwrap_or_else(|| "no view".into()),
        t.map_or(0, |t| t.held_len()),
        t.is_some_and(|t| t.visible)
    )
}
