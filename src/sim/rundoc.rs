//! The run panels' output as documents, simulated (panes P2c): a task printing at random — now and then a long burst
//! its run trims (`Run::keep`), now and then replaced by another run (a new generation) — normal mode's keys on its
//! document and the panel's over them (`C-w j`, moves, `gg`, `G`, `x`, `v`, `y` — its copy a job — an edit's keys,
//! `u`, `esc`, `q`, `tab`, `enter`, a search), the wheel over its rows, frames at two sizes, time passing. After
//! every step: the document is the run's lines, line for line — nothing a key typed or deleted stays; never left
//! swapped in; while it has the keys its cursor is in its rows and the run's scroll is its top, and a print never
//! moves what's in sight; after a frame, the rows drawn are the document's.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::Editor;
use crate::event::{Mouse, MouseKind};
use crate::key::Key;
use crate::sizes::Slot;

const SEEDS: &[u64] = &[0, 1, 2];

const INPUTS: usize = 60;

const LIMIT: Duration = Duration::from_secs(60);

const W: u16 = 100;

#[test]
fn the_run_panels_output_documents() {
    super::seeds("the_run_panels_output_documents", SEEDS, run);
}

/// What's in sight of an output: its document, top, cursor, and the text of its lines in sight (up to a count of
/// lines — what a print adds below them, room left, comes into sight).
type Sight = (crate::document::DocId, usize, usize, String);

/// What the scenario keeps between steps: lines printed, runs made, what was in sight before a print while the
/// output had the keys (and its lines then), and the frame's height when the last step drew one.
#[derive(Default)]
struct World {
    printed: Cell<usize>,
    runs: Cell<u64>,
    before: RefCell<Option<(Sight, usize)>>,
    framed: Cell<Option<u16>>,
}

fn task(n: u64) -> crate::task::Run {
    let t = crate::task::Task { name: "check".into(), cmd: "cargo check".into(), cwd: std::env::temp_dir() };
    crate::task::Run::for_test(t).of_generation(n)
}

fn run(choices: Choices) -> Vec<String> {
    let mut sim = Sim::new("the_run_panels_output_documents", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    ed.docs[0].text = ropey::Rope::from_str("fn main() {}\n");
    ed.session.task_run = Some(task(0));
    // (the chat `enter` and a quote go to: a stand-in claude, no process)
    let mut c = crate::chat::Chat::new();
    let claude = crate::chat::stand_in(&mut c);
    ed.session.chat = Some(c);
    crate::term::render(&mut ed, &mut Vec::new(), W, 30).unwrap();
    let world = World::default();
    let check = |ed: &Editor| {
        while claude.try_recv().is_ok() {}
        held(ed, &world)
    };
    sim.walk(&mut ed, INPUTS, |sim, ed| input(sim, ed, &world), check, print);
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

/// What's in sight of the task panel's output while it has the keys, of its first `upto` lines — and its lines.
fn in_sight(ed: &Editor, upto: Option<usize>) -> Option<(Sight, usize)> {
    crate::rundoc::has_keys(ed).filter(|&s| s == Slot::Task)?;
    let d = ed.session.outputs.doc(Slot::Task)?;
    let lines = crate::movement::last_line(&d.text) + 1;
    let rows = crate::rundoc::rows(ed, Slot::Task);
    let end = (d.top + rows).min(upto.unwrap_or(lines));
    let text: String = (d.top..end).filter_map(|l| d.text.get_line(l)).map(|l| l.to_string()).collect();
    Some(((d.id, d.top, d.selection().primary().head, text), lines))
}

fn input(sim: &mut Sim, ed: &mut Editor, world: &World) -> String {
    world.framed.set(None);
    let c = &mut sim.choices;
    // (by arm: the run prints 4, a key 6, the wheel 2, a frame 2, another run 1, time passing 1, a burst 1)
    match c.weighted(&[4, 6, 2, 2, 1, 1, 1]) {
        0 => {
            let Some(r) = ed.session.task_run.as_mut() else { return "(no run)".into() };
            let lines = *c.pick(&[1, 3, 12]);
            let empty = r.output.is_empty();
            for _ in 0..lines {
                world.printed.set(world.printed.get() + 1);
                r.push_line(
                    world.printed.get().is_multiple_of(5),
                    format!("out {}", world.printed.get()),
                    false,
                );
            }
            // (what's in sight held still — but its first lines: the command it showed goes)
            *world.before.borrow_mut() = in_sight(ed, None).filter(|_| !empty);
            ed.focus_sync();
            format!("the run prints {lines} lines")
        }
        1 => {
            let keys = *c.pick(&[
                "<C-w>j",
                "<C-w>j",
                "k",
                "j",
                "gg",
                "G",
                "x",
                "y",
                "<esc>",
                "q",
                "<tab>",
                "<ret>",
                "/out 1<ret>",
                "n",
                "<C-e>",
                "<C-d>",
                "vjj",
                "xd",
                "u",
                "i",
                "ox",
                "<C-w>k",
            ]);
            let mut chars = keys.chars();
            while let Some(ch) = chars.next() {
                let k: Key = match ch {
                    '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
                    ch => Key::plain(crate::key::Code::Char(ch)),
                };
                ed.handle_key(k);
            }
            format!("keys {keys}")
        }
        2 => {
            let up = c.below(2) == 0;
            let Some((_, y, x, rows)) = ed.screen.run_rows.get().filter(|_| ed.slot() == Some(Slot::Task))
            else {
                return "(no run panel in front: no wheel over it)".into();
            };
            let row = c.below(rows.max(1));
            let kind = if up { MouseKind::ScrollUp } else { MouseKind::ScrollDown };
            ed.handle_mouse(Mouse { kind, x: (x + 1) as u16, y: (y + row) as u16, alt: false });
            ed.focus_sync();
            format!("the wheel {} over row {row}", if up { "up" } else { "down" })
        }
        3 => {
            let h = *c.pick(&[30, 36]);
            crate::term::render(ed, &mut Vec::new(), W, h).unwrap();
            world.framed.set(Some(h));
            format!("a frame, {h} rows")
        }
        4 => {
            world.runs.set(world.runs.get() + 1);
            ed.session.task_run = Some(task(world.runs.get()));
            ed.focus_sync();
            "another run takes the panel".into()
        }
        5 => {
            let ms = *c.pick(&[100, 2000]);
            sim.wait(Duration::from_millis(ms));
            "time passes".into()
        }
        _ => {
            // (past what a step keeps: the run trims its output, a marker line saying what went)
            let Some(r) = ed.session.task_run.as_mut() else { return "(no run)".into() };
            for _ in 0..1800 {
                world.printed.set(world.printed.get() + 1);
                r.push_line(false, format!("burst {}", world.printed.get()), false);
            }
            r.keep(0);
            ed.focus_sync();
            "the run prints 1800 lines, kept as it keeps them".into()
        }
    }
}

fn held(ed: &Editor, world: &World) -> Result<(), String> {
    if crate::rundoc::swapped(ed) {
        return Err("an output left swapped in".into());
    }
    let Some(r) = ed.session.task_run.as_ref() else {
        return match ed.session.outputs.doc(Slot::Task) {
            Some(_) => Err("an output with no run".into()),
            None => Ok(()),
        };
    };
    let d = ed.session.outputs.doc(Slot::Task).ok_or("a run with no output")?;
    // The run's lines, line for line — nothing a key typed or deleted stays (read-only)
    let want: String = match r.output.is_empty() {
        true => "cargo check\n".into(),
        false => r.output.iter().map(|(_, l)| format!("{l}\n")).collect(),
    };
    if d.text != want.as_str() {
        return Err(format!("the output's {} lines aren't the run's {}", d.text.len_lines(), r.output.len()));
    }
    if let Some((before, lines)) = world.before.borrow_mut().take()
        && let Some((now, _)) = in_sight(ed, Some(lines))
        && before != now
    {
        return Err(format!("a print moved what's in sight: top {} → {}", before.1, now.1));
    }
    if crate::rundoc::has_keys(ed) != Some(Slot::Task) {
        return Ok(());
    }
    let last = crate::movement::last_line(&d.text);
    let line = crate::movement::line_of(&d.text, d.selection().primary().cursor(&d.text));
    let rows = crate::rundoc::rows(ed, Slot::Task);
    if d.top > last || line < d.top || line >= d.top + rows {
        return Err(format!("the cursor's line {line} out of its rows from {} ({rows})", d.top));
    }
    // (the run's scroll is the document's top — what `o` reads as the step in view, where it's left)
    if r.top(rows) != d.top.min(r.output.len().saturating_sub(rows)) {
        return Err(format!("the run's top {}, the document's {}", r.top(rows), d.top));
    }
    // (after a frame: the rows drawn are the document's, from its top)
    if let (Some(h), Some((frame, w, fh))) = (world.framed.get(), ed.last_frame.as_ref())
        && *fh == h
        && d.left == 0
    {
        let screen = crate::screen::text(frame, *w, *fh).unwrap_or_default();
        let (_, y, x, rows) = ed.screen.run_rows.get().ok_or("drawn, its rows not recorded")?;
        for (i, shown) in screen.lines().skip(y).take(rows).enumerate() {
            let shown: String = shown.chars().skip(x).collect();
            let want =
                d.text.get_line(d.top + i).map(|l| l.to_string().trim_end().to_string()).unwrap_or_default();
            let n = want.chars().count().min(20);
            let want: String = want.chars().take(n).collect();
            if !shown.starts_with(&want) {
                return Err(format!("row {i}: drawn {shown:?}, the document's line {} {want:?}", d.top + i));
            }
        }
    }
    Ok(())
}

fn print(ed: &Editor) -> String {
    let keys = ed.keys_to().region.name();
    let doc = ed.session.outputs.doc(Slot::Task).map(|d| {
        let line = crate::movement::line_of(&d.text, d.selection().primary().cursor(&d.text));
        format!("output at {}/{} from {}", line + 1, crate::movement::last_line(&d.text) + 1, d.top)
    });
    let r = ed.session.task_run.as_ref();
    format!(
        "keys {keys} · {} · run {} of {} lines",
        doc.unwrap_or_else(|| "no output".into()),
        r.map_or(0, |r| r.generation()),
        r.map_or(0, |r| r.output.len())
    )
}
