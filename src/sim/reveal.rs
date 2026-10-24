//! Claude's writing shown at the stream's pace, simulated (`writing::Pace`, `writing::reveal`): an edit's text streamed
//! in pieces of any size at any gap (the measured stream's lumps among them), frames ticking between, its input whole,
//! the edit landing at any point of the reveal, the turn's end, time passing. After every step: what's shown of an
//! item never goes back nor past what came, never cuts a character; the rows drawn are that much (the caret's row ends
//! with the shown text's last line); once whole, all of it is shown within `WHOLE` of a frame; a frame is asked
//! exactly while an item drawn has text left to show — none once it landed or the turn ended (an idle tarae draws
//! nothing).

use std::cell::RefCell;
use std::time::{Duration, Instant};

use super::{Choices, Sim};
use crate::codeview::Row;
use crate::editor::Editor;
use crate::llm::StreamEv;
use crate::writing::{Delta, Place, REVEAL_EVERY, Scan, Tool};

const SEEDS: &[u64] = &[0, 1, 2, 3, 4];

const INPUTS: usize = 70;

const LIMIT: Duration = crate::test_results::PATIENCE;

const TEXT: &str =
    "fn main() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    println!(\"{a} {b} {c}\");\n}\n";

const OLD: &str = "    let b = 2;\n";

const NEW: &str = "    let b = 2;\n    // — 타래 e\u{301}\n    let d = b * 40_000;\n    let e = d + 1;\n";

#[test]
fn claudes_writing_is_shown_at_the_streams_pace() {
    super::seeds("claudes_writing_is_shown_at_the_streams_pace", SEEDS, run);
}

#[derive(Default)]
struct World {
    input: String,
    sent: usize,
    scan: Option<Scan>,
    /// When the input came whole, and whether a frame ticked `WHOLE` after it.
    whole: Option<Instant>,
    landed: bool,
    /// What was shown at the last check.
    shown: usize,
    /// The step just taken was a frame's tick.
    ticked: bool,
    inputs: usize,
}

fn run(choices: Choices) -> Vec<String> {
    let mut sim = Sim::new("claudes_writing_is_shown_at_the_streams_pace", choices, LIMIT);
    let dir = crate::scratch::Scratch::new("sim-reveal");
    std::fs::write(dir.join("a.rs"), TEXT).unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = dir.to_path_buf();
    ed.open(&dir.join("a.rs")).unwrap();
    let mut c = crate::chat::Chat::new();
    let _claude = crate::chat::stand_in(&mut c);
    c.follow.on = true;
    c.state = crate::chat::State::Thinking;
    c.writing.paced = true;
    ed.session.chat = Some(c);
    crate::term::render(&mut ed, &mut Vec::new(), 100, 32).unwrap();
    let s = |t: &str| serde_json::to_string(t).unwrap();
    let input =
        format!(r#"{{"edits":[{{"file_path":"a.rs","old_string":{},"new_string":{}}}]}}"#, s(OLD), s(NEW));
    let world = RefCell::new(World { input, scan: Some(Scan::new(Tool::Edit)), ..World::default() });
    let root = dir.to_path_buf();
    let check = |ed: &Editor| held(ed, &mut world.borrow_mut());
    sim.walk(&mut ed, INPUTS, |sim, ed| step(sim, ed, &mut world.borrow_mut(), &root), check, print);
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

fn busy(ed: &Editor) -> bool {
    ed.session.chat.as_ref().is_some_and(|c| c.state != crate::chat::State::Idle)
}

fn step(sim: &mut Sim, ed: &mut Editor, w: &mut World, root: &std::path::Path) -> String {
    let g = ed.session.chat.as_ref().map_or(0, crate::chat::Chat::generation);
    w.inputs += 1;
    w.ticked = false;
    let c = &mut sim.choices;
    // (by arm: a piece 5, a frame's tick 4, time 3, a frame drawn 1, the edit landing 1, the turn's end 1 — the last
    // input ends a turn under way)
    let arm = if w.inputs == INPUTS && busy(ed) { 5 } else { c.weighted(&[5, 4, 3, 1, 1, 1]) };
    match arm {
        0 => {
            let chars: Vec<char> = w.input.chars().collect();
            if !busy(ed) || w.sent >= chars.len() {
                return "(no input to stream)".into();
            }
            let n = *c.pick(&[1, 7, 16, 30, 190]);
            let end = (w.sent + n).min(chars.len());
            let piece: String = chars[w.sent..end].iter().collect();
            w.sent = end;
            let Some(scan) = w.scan.as_mut() else { return "(no reader)".into() };
            scan.feed(&piece);
            let done = end == chars.len();
            let d = Delta { call: "t1".into(), tool: Tool::Edit, pieces: scan.take(), done };
            // (as the loop brings it: an event, the reveal moved after it)
            ed.handle_event(crate::event::Event::Job(Box::new(move |ed: &mut Editor| {
                crate::chat::stream_event(ed, g, StreamEv::Writing(d));
            })));
            if done {
                w.whole.get_or_insert(crate::clock::now());
            }
            format!("{n} characters of the input ({end} of {})", chars.len())
        }
        1 => {
            // (as the loop's: `REVEAL_EVERY` after the last)
            sim.wait(REVEAL_EVERY);
            ed.handle_event(crate::event::Event::Job(Box::new(|_: &mut Editor| {})));
            w.ticked = true;
            "a frame's tick".into()
        }
        2 => {
            let ms = *c.pick(&[5, 40, 160, 640, 1500]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
        3 => {
            let width = *c.pick(&[60, 100]);
            crate::term::render(ed, &mut Vec::new(), width, 32).unwrap();
            format!("a frame, {width} columns")
        }
        4 => {
            if w.landed || w.whole.is_none() || !busy(ed) {
                return "(nothing whole to land)".into();
            }
            w.landed = true;
            std::fs::write(root.join("a.rs"), TEXT.replace(OLD, NEW)).unwrap();
            let _ = crate::typed::execute(ed, "reload");
            "the edit lands".into()
        }
        _ => {
            // (a turn ends once its edit is whole — one cut short is `writing`'s scenario)
            if !busy(ed) || w.whole.is_none() {
                return "(no turn to end)".into();
            }
            crate::chat::stream_event(ed, g, StreamEv::Done(Ok(String::new())));
            "the turn ends".into()
        }
    }
}

fn held(ed: &Editor, w: &mut World) -> Result<(), String> {
    let Some(c) = ed.session.chat.as_ref() else { return Err("the chat went".into()) };
    let item = c.writing.calls.iter().flat_map(|c| &c.items).next();
    let Some(it) = item else {
        if crate::writing::reveal_wake(ed).is_some() {
            return Err("a frame asked with nothing being written".into());
        }
        w.shown = 0;
        return Ok(());
    };
    let (new, shown) = (it.new.s.as_str(), it.pace.shown);
    if shown > new.len() || !new.is_char_boundary(shown) {
        return Err(format!("{shown} shown of {} bytes, or inside a character", new.len()));
    }
    if shown < w.shown {
        return Err(format!("what's shown went back: {} → {shown}", w.shown));
    }
    // (never half a cluster: `é`)
    if new[..shown].ends_with('e') && new[shown..].starts_with('\u{301}') {
        return Err("a character shown in half".into());
    }
    w.shown = shown;
    // All of it within WHOLE once whole — by the first frame after that
    if let Some(whole) = w.whole
        && w.ticked
        && crate::clock::since(whole) >= crate::writing::WHOLE + REVEAL_EVERY
        && shown < new.len()
    {
        return Err(format!(
            "{} bytes still to show {:?} after it came whole",
            new.len() - shown,
            crate::clock::since(whole)
        ));
    }
    // A frame asked exactly while an item drawn has text left to show
    let drawn = matches!(&it.place, Place::Found(f) if f.bound.is_some());
    let wake = crate::writing::reveal_wake(ed);
    if wake.is_some() != (drawn && shown < new.len()) {
        return Err(format!("frame asked {wake:?} with {drawn} drawn, {shown} of {} shown", new.len()));
    }
    if w.landed && w.ticked && wake.is_some() {
        return Err("frames asked after it landed".into());
    }
    // The rows drawn are what's shown: the caret's row ends with its last line
    let doc = ed.doc();
    if let Some(s) = crate::writing::shown(doc) {
        if w.landed {
            return Err("rows drawn over what landed".into());
        }
        for b in &s.blocks {
            if let Some(Row::Other { text, .. }) = b.head.and_then(|h| b.rows.get(h)) {
                let last = new[..shown].rsplit('\n').next().unwrap_or_default();
                if !text.ends_with(last) {
                    return Err(format!("the caret's row {text:?} isn't what's shown ({last:?})"));
                }
            }
        }
    }
    Ok(())
}

fn print(ed: &Editor) -> String {
    let c = ed.session.chat.as_ref();
    let it = c.and_then(|c| c.writing.calls.iter().flat_map(|c| &c.items).next());
    format!(
        "{:?} · {} of {} shown · wake {:?}",
        c.map(|c| c.state),
        it.map_or(0, |it| it.pace.shown),
        it.map_or(0, |it| it.new.s.len()),
        crate::writing::reveal_wake(ed)
    )
}
