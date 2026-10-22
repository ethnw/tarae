//! Live edits, simulated (글타래 P7, `writing.rs`): a turn's edit streamed in pieces of any size, its place looked for
//! on a worker (the result landing among the steps), the call made once its input is whole and its review opening
//! from another job; keys between — moves, typing in the buffer, `y`/`n` on the review — frames at two widths, the
//! turn's end, the next turn's edit over another line, time passing. After every step: no document's text changed
//! but by a key (nothing applied, nothing decided); a stream piece never moved the keys nor your cursor (collab C1:
//! follow moves the view, never your cursor), and moved the view's line only while follow led — onto the line a drawn
//! block rewrites (`Block::place`; its caret, drawn, may move with every frame); a key of yours that moved the cursor
//! paused follow; a buffer with nothing unsaved and no save under
//! way is its file on disk (a review's `y` saves through the saver's job, the simulation's own); rows are drawn only in a
//! buffer that holds what they replace, never under a review, none once the turn is over; and a review that opens
//! on rows drawn whole shows the same rows — the same lines taken out, the same lines written.

use std::cell::RefCell;
use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::Editor;
use crate::key::{Code, Key};
use crate::llm::StreamEv;
use crate::writing::{Delta, Place, Scan, Tool};

// (4: the line rewritten moves as a kept line is written · 66: a review answered with something applied saves —
// the saver's job, never a thread of its own · 150: a review left open while follow is paused wakes nothing)
const SEEDS: &[u64] = &[0, 1, 2, 3, 4, 66, 150];

const INPUTS: usize = 80;

const LIMIT: Duration = Duration::from_secs(60);

const TEXT: &str =
    "fn main() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    println!(\"{a} {b} {c}\");\n}\n";

#[test]
fn an_edit_is_drawn_as_written_and_reviewed_as_drawn() {
    super::seeds("an_edit_is_drawn_as_written_and_reviewed_as_drawn", SEEDS, run);
}

/// The scenario's own account: the turn's edit (its input, how much of it went, its reader), the texts the keys
/// may have changed, the last rows drawn whole.
#[derive(Default)]
struct World {
    turn: usize,
    input: String,
    sent: usize,
    scan: Option<Scan>,
    made: bool,
    /// Every document's text as the last step left it, and whether that step was a key.
    texts: Vec<(crate::document::DocId, String)>,
    keyed: bool,
    /// A stream piece moved where the keys go.
    moved_keys: Option<String>,
    /// The cursor moved as follow mustn't lead it (by a piece), or a key's move left follow leading.
    led_wrong: Option<String>,
    /// The rows last drawn with their text whole: their document, its version then, each row's text.
    whole: Option<(crate::document::DocId, u64, Vec<String>)>,
    /// The review generation already compared.
    compared: Option<u64>,
    /// Inputs taken.
    inputs: usize,
}

fn run(choices: Choices) -> Vec<String> {
    let mut sim = Sim::new("an_edit_is_drawn_as_written_and_reviewed_as_drawn", choices, LIMIT);
    let dir = crate::scratch::Scratch::new("sim-writing");
    std::fs::write(dir.join("a.rs"), TEXT).unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = dir.to_path_buf();
    ed.open(&dir.join("a.rs")).unwrap();
    let mut c = crate::chat::Chat::new();
    let _claude = crate::chat::stand_in(&mut c);
    c.follow.on = true;
    ed.session.chat = Some(c);
    crate::term::render(&mut ed, &mut Vec::new(), 100, 32).unwrap();
    let world = RefCell::new(World::default());
    begin(&mut ed, &mut world.borrow_mut(), 2);
    world.borrow_mut().texts = texts(&ed);
    let check = |ed: &Editor| held(ed, &mut world.borrow_mut());
    sim.walk(&mut ed, INPUTS, |sim, ed| step(sim, ed, &mut world.borrow_mut()), check, print);
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

fn texts(ed: &Editor) -> Vec<(crate::document::DocId, String)> {
    ed.docs.iter().map(|d| (d.id, d.text.to_string())).collect()
}

fn generation(ed: &Editor) -> u64 {
    ed.session.chat.as_ref().map_or(0, crate::chat::Chat::generation)
}

/// A turn begins (a message sent, as the stand-in has it): its edit replaces lines `line` and the next of the text as
/// it is now, keeping the first (its head moves from line to line as it's written) — the keys in the order the
/// model writes them.
fn begin(ed: &mut Editor, w: &mut World, line: usize) {
    let Some(c) = ed.session.chat.as_mut() else { return };
    c.state = crate::chat::State::Thinking;
    let doc = ed.doc();
    let last = crate::movement::last_line(&doc.text);
    let (a, b) = (doc.text.line(line.min(last)).to_string(), doc.text.line((line + 1).min(last)).to_string());
    let old = format!("{a}{b}");
    let new = format!("    // turn {}\n{a}{}    let z = {};\n", w.turn, b.replace("= ", "= 1 + "), w.turn);
    let s = |t: &str| serde_json::to_string(t).unwrap();
    w.input =
        format!(r#"{{"edits":[{{"file_path":"a.rs","old_string":{},"new_string":{}}}]}}"#, s(&old), s(&new));
    (w.sent, w.made, w.scan) = (0, false, Some(Scan::new(Tool::Edit)));
    // (the rows an earlier turn drew are its own: this turn's review is compared with this turn's)
    w.whole = None;
    w.turn += 1;
}

fn call_id(w: &World) -> String {
    format!("t{}", w.turn)
}

fn keys(ed: &mut Editor, keys: &str) {
    let mut chars = keys.chars();
    while let Some(ch) = chars.next() {
        let k: Key = match ch {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            ch => Key::plain(Code::Char(ch)),
        };
        // (through the loop, as a person's key comes: follow sees it after it — `follow::watch`)
        ed.handle_event(crate::event::Event::Key(k));
    }
}

/// Where the cursor is: its document and line.
fn cursor(ed: &Editor) -> (crate::document::DocId, usize) {
    (ed.doc().id, ed.cursor_line())
}

/// A piece moved the view's line (collab C1: follow moves the view, never your cursor): only while follow could lead
/// (on, not paused, nothing of yours under way), and onto the line a drawn block in the document in front rewrites
/// (`Block::place`) — never a line between.
fn led_right(ed: &Editor, could: bool) -> Result<(), String> {
    if !could {
        return Err("a piece moved the view while follow couldn't lead".into());
    }
    let doc = ed.doc();
    let Some(line) = crate::follow::view_line(ed) else { return Ok(()) };
    let last = crate::movement::last_line(&doc.text);
    let blocks = crate::writing::shown(doc).map(|s| s.blocks.clone()).unwrap_or_default();
    if !blocks.iter().any(|b| b.place().min(last) == line) {
        return Err(format!("a piece led the view to line {line}, no line rewritten there"));
    }
    Ok(())
}

fn step(sim: &mut Sim, ed: &mut Editor, w: &mut World) -> String {
    let busy = ed.session.chat.as_ref().is_some_and(|c| c.state != crate::chat::State::Idle);
    let g = generation(ed);
    w.keyed = false;
    w.inputs += 1;
    let c = &mut sim.choices;
    // (by arm: a piece of the input 6, the call made 2, a key 4, a frame 2, the turn's end 1, a new turn 1, time 1;
    // the last input ends a turn under way — a turn left open keeps follow's and the session's timers going. The
    // chat closed mid-turn is `writing::tests::a_closed_chat_takes_its_rows_with_it`'s: its saver's thread isn't
    // the simulation's)
    let arm = if w.inputs == INPUTS && busy { 4 } else { c.weighted(&[6, 2, 4, 2, 1, 1, 1]) };
    match arm {
        0 => {
            let chars: Vec<char> = w.input.chars().collect();
            if !busy || w.sent >= chars.len() {
                return "(no input to stream)".into();
            }
            let n = *c.pick(&[1, 7, 30, 200]);
            let end = (w.sent + n).min(chars.len());
            let piece: String = chars[w.sent..end].iter().collect();
            w.sent = end;
            let call = call_id(w);
            let Some(scan) = w.scan.as_mut() else { return "(no reader)".into() };
            scan.feed(&piece);
            let d = Delta { call, tool: Tool::Edit, pieces: scan.take(), done: end == chars.len() };
            let region = ed.keys_to().region.name().to_string();
            let could =
                ed.session.chat.as_ref().is_some_and(|c| c.follow.active()) && crate::follow::free(ed);
            let before = (cursor(ed), could, crate::follow::view_line(ed), ed.doc().id);
            crate::chat::stream_event(ed, g, StreamEv::Writing(d));
            if ed.keys_to().region.name() != region {
                w.moved_keys = Some(format!("{region} → {}", ed.keys_to().region.name()));
            }
            // (your cursor: never a piece's — in the document it was in)
            let still = ed.doc().id != before.3 || cursor(ed) == before.0;
            if !still {
                w.led_wrong = Some(format!("a piece moved your cursor: {:?} → {:?}", before.0, cursor(ed)));
            } else if crate::follow::view_line(ed) != before.2
                && let Err(e) = led_right(ed, before.1)
            {
                w.led_wrong = Some(e);
            }
            format!("{n} characters of the input ({end} of {})", chars.len())
        }
        1 => {
            if !busy || w.made || w.sent < w.input.chars().count() {
                return "(no whole input to call with)".into();
            }
            w.made = true;
            let input: serde_json::Value = serde_json::from_str(&w.input).unwrap();
            let id = call_id(w);
            let tool = StreamEv::Tool { id, name: "mcp__tarae__edit".into(), input: input.clone() };
            crate::chat::stream_event(ed, g, tool);
            let request = serde_json::json!({"subtype": "mcp_message", "server_name": "tarae", "message": {
                "jsonrpc": "2.0", "id": w.turn, "method": "tools/call",
                "params": {"name": "edit", "arguments": input}}});
            crate::chat::stream_event(
                ed,
                g,
                StreamEv::Control { request_id: format!("r{}", w.turn), request },
            );
            "the call made".into()
        }
        2 => {
            let k = *c.pick(&["j", "k", "gg", "ix<esc>", "u", "y", "n", "<esc>"]);
            w.keyed = true;
            let leading = ed.keys_to().region.name() == "code" && crate::follow::leads(ed);
            let at = cursor(ed);
            keys(ed, k);
            // (`y`/`n` answer a review, which may lead the cursor to what it took: follow's, not yours)
            let paused = ed.session.chat.as_ref().is_none_or(|c| !c.follow.active());
            if leading && cursor(ed) != at && !paused {
                w.led_wrong = Some(format!("keys {k} moved the cursor and follow still leads"));
            }
            format!("keys {k}")
        }
        3 => {
            let width = *c.pick(&[100, 160]);
            crate::term::render(ed, &mut Vec::new(), width, 32).unwrap();
            format!("a frame, {width} columns")
        }
        4 => {
            if !busy {
                return "(no turn to end)".into();
            }
            crate::chat::stream_event(ed, g, StreamEv::Done(Ok(String::new())));
            "the turn ends".into()
        }
        5 => {
            if busy {
                return "(a turn under way)".into();
            }
            let line = *c.pick(&[1, 2, 3]);
            begin(ed, w, line);
            format!("a turn begins, its edit over line {}", line + 1)
        }
        _ => {
            let ms = *c.pick(&[30, 2000]);
            sim.wait(Duration::from_millis(ms));
            "time passes".into()
        }
    }
}

/// A row of a block or a review as text: a document line's (one it keeps or takes out) or its own.
fn row_text(doc: &crate::document::Document, r: &crate::codeview::Row) -> String {
    match r {
        crate::codeview::Row::Text { line, .. } => {
            doc.text.line(*line).to_string().trim_end_matches('\n').to_string()
        }
        crate::codeview::Row::Other { text, .. } => format!("+{text}"),
        _ => String::new(),
    }
}

fn held(ed: &Editor, w: &mut World) -> Result<(), String> {
    // Nothing applied: a document's text changes only by a key
    let now = texts(ed);
    if !w.keyed && now != w.texts {
        return Err("a document's text changed with no key".into());
    }
    w.texts = now;
    if let Some(m) = w.moved_keys.take() {
        return Err(format!("a stream piece moved the keys: {m}"));
    }
    if let Some(m) = w.led_wrong.take() {
        return Err(m);
    }
    // A review answered with something applied saves it (`refactor::reviewed` → `save_beneath`, the saver's job):
    // a buffer with nothing unsaved and no save under way is its file on disk
    for d in ed.docs.iter().filter(|d| !d.is_modified() && d.saving == 0) {
        let Some(path) = d.path.as_ref() else { continue };
        let disk = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        if d.text != disk.as_str() {
            return Err(format!("{} unsaved on disk, its buffer saying it's saved", path.display()));
        }
    }
    let review = ed.session.review.as_ref();
    let c = ed.session.chat.as_ref().ok_or("the chat went")?;
    for d in &ed.docs {
        let Some(shown) = crate::writing::shown(d) else { continue };
        if review.is_some_and(|r| r.doc_id == d.id) {
            return Err("rows drawn under a review".into());
        }
        if c.state == crate::chat::State::Idle {
            return Err("rows drawn with the turn over".into());
        }
        // Only where the buffer holds what they replace
        for it in c.writing.calls.iter().flat_map(|c| &c.items) {
            if let Place::Found(f) = &it.place
                && f.bound.is_some_and(|b| b.0 == d.id)
                && d.text.get_byte_slice(f.from..f.to).is_none_or(|s| s != it.old.s.as_str())
            {
                return Err(format!(
                    "rows over {}..{}, which no longer holds what they replace",
                    f.from, f.to
                ));
            }
        }
        for b in &shown.blocks {
            if b.last > crate::movement::last_line(&d.text) {
                return Err(format!("a block past the text's end: {}..={}", b.first, b.last));
            }
            if b.head.is_none() {
                let rows = b.rows.iter().filter(|r| changed(r)).map(|r| row_text(d, r)).collect();
                w.whole = Some((d.id, d.version(), rows));
            }
        }
    }
    // A review opening on rows drawn whole shows those rows
    if let Some(r) = review.filter(|r| w.compared != Some(r.generation)) {
        w.compared = Some(r.generation);
        if let Some((id, version, rows)) = w.whole.take()
            && r.doc_id == id
            && let Some(d) = ed.docs.iter().find(|d| d.id == id && d.version() == version)
        {
            let its: Vec<String> = r
                .rows(&d.text)
                .iter()
                .filter(|x| x.hunk().is_some() && changed(x))
                .map(|x| row_text(d, x))
                .collect();
            if its != rows {
                return Err(format!("the review shows {its:?} where {rows:?} were drawn"));
            }
        }
    }
    Ok(())
}

/// A row of a change: a line taken out or one written (not a line it keeps, nor a change's head).
fn changed(r: &crate::codeview::Row) -> bool {
    use crate::codeview::{Mark, OtherKind, Row};
    match r {
        Row::Text { edit: Some(e), .. } => e.mark == Mark::Taken,
        Row::Other { kind: OtherKind::Writing { .. } | OtherKind::Edit(_), .. } => true,
        _ => false,
    }
}

fn print(ed: &Editor) -> String {
    let rows: usize =
        ed.docs.iter().filter_map(crate::writing::shown).flat_map(|s| &s.blocks).map(|b| b.rows.len()).sum();
    let c = ed.session.chat.as_ref();
    format!(
        "{} · {:?} · {rows} rows drawn · review {}",
        ed.keys_to().region.name(),
        c.map(|c| c.state),
        if ed.session.review.is_some() { "open" } else { "none" }
    )
}
