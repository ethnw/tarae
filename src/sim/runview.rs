//! The run panels' view mode, simulated (view P3): a task printing at random, now and then replaced by another run
//! (a new generation), the keys of its view mode (`C-w j`, `k`, `gg`, `G`, `x`, `y` — its copy a job — `esc`, `q`,
//! `tab`, `enter`, a search), the wheel over its rows, frames at two sizes, time passing. After every step: a view
//! is a run panel's only while that panel has tarae's keys and shows the run it was made of; held still — the same
//! view's document never changes, whatever the run printed; its cursor in its rows, the run's scroll its top; the
//! panel's keys never without their view mode; and after a frame, the rows drawn are the document's.

use std::cell::Cell;
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
fn the_run_panels_view_mode() {
    super::seeds("the_run_panels_view_mode", SEEDS, run);
}

/// What the scenario keeps between steps: lines printed, runs made, the view's document as last checked (its id —
/// another when it's made again — and its text's hash), and the frame's height when the last step drew one.
#[derive(Default)]
struct World {
    printed: Cell<usize>,
    runs: Cell<u64>,
    seen: Cell<Option<(crate::document::DocId, u64)>>,
    framed: Cell<Option<u16>>,
}

fn task(n: u64) -> crate::task::Run {
    let t = crate::task::Task { name: "check".into(), cmd: "cargo check".into(), cwd: std::env::temp_dir() };
    crate::task::Run::for_test(t).of_generation(n)
}

fn run(choices: Choices) -> Vec<String> {
    let mut sim = Sim::new("the_run_panels_view_mode", choices, LIMIT);
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

fn input(sim: &mut Sim, ed: &mut Editor, world: &World) -> String {
    world.framed.set(None);
    let c = &mut sim.choices;
    // (by arm: the run prints 4, a key 6, the wheel 2, a frame 2, another run 1, time passing 1)
    match c.weighted(&[4, 6, 2, 2, 1, 1]) {
        0 => {
            let Some(r) = ed.session.task_run.as_mut() else { return "(no run)".into() };
            let lines = *c.pick(&[1, 3, 12]);
            for _ in 0..lines {
                world.printed.set(world.printed.get() + 1);
                r.push_line(
                    world.printed.get().is_multiple_of(5),
                    format!("out {}", world.printed.get()),
                    false,
                );
            }
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
                "<C-e>",
                "<up>",
                "i",
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
        _ => {
            let ms = *c.pick(&[100, 2000]);
            sim.wait(Duration::from_millis(ms));
            "time passes".into()
        }
    }
}

fn held(ed: &Editor, world: &World) -> Result<(), String> {
    let front = ed.session.is_at(crate::session::At::Slot) && ed.slot() == Some(Slot::Task);
    let Some(v) = crate::view::run_view_of(ed, Slot::Task) else {
        // (tarae's keys on the panel are its view mode — never without it, once the keys were looked at)
        if front && ed.session.task_run.is_some() && ed.prompt.is_none() && ed.picker.is_none() {
            return Err("the task panel has the keys, no view mode".into());
        }
        return Ok(());
    };
    if !front {
        return Err("a run panel's view without its panel's keys".into());
    }
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
    let last = crate::movement::last_line(&d.text);
    let line = crate::movement::line_of(&d.text, d.selection().primary().cursor(&d.text));
    let rows = crate::view::run_rows(ed, v.surface);
    if d.top > last || line < d.top || line >= d.top + rows {
        return Err(format!("the cursor's line {line} out of its rows from {} ({rows})", d.top));
    }
    // (the run's scroll is the view's top — what `o` reads as the step in view, where it's left)
    let r = ed.session.task_run.as_ref().ok_or("a view with no run")?;
    if r.top(rows) != d.top.min(r.output.len().saturating_sub(rows)) {
        return Err(format!("the run's top {}, the view's {}", r.top(rows), d.top));
    }
    // (after a frame: the rows drawn are the document's, from its top)
    if let (Some(h), Some((frame, w, fh))) = (world.framed.get(), ed.last_frame.as_ref())
        && *fh == h
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
    let view = crate::view::run_view_of(ed, Slot::Task).map(|v| {
        let d = v.doc();
        let line = crate::movement::line_of(&d.text, d.selection().primary().cursor(&d.text));
        format!("view at {}/{} from {}", line + 1, v.rows(), d.top)
    });
    let r = ed.session.task_run.as_ref();
    format!(
        "keys {keys} · {} · run {} of {} lines",
        view.unwrap_or_else(|| "no view".into()),
        r.map_or(0, |r| r.generation()),
        r.map_or(0, |r| r.output.len())
    )
}
