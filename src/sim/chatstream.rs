//! The chat's transcript as a document, simulated (panes P2c step 2): an answer being written word by word — now and
//! then a paragraph's end, a long run that wraps, a question and a new answer, the turn's end — normal mode's keys on
//! the transcript and the input's around them (`esc` in and out, moves, `x`, `v`, a search, an edit's keys, `enter`'s
//! quote, `i`, `C-w` between the code and the chat), the wheel over its rows, frames at three sizes, time passing.
//! After every step: the document is what the last frame laid out, line for line — nothing a key typed or deleted
//! stays; never left swapped in; while its view has the keys its cursor is in its rows, and text arriving moves
//! nothing in sight (the rows above the line being written, the cursor, the top), said `new text after`; after a
//! frame, the rows drawn are the document's.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use super::{Choices, Sim};
use crate::chat::{Msg, Role, State};
use crate::chatdoc::{self, Of};
use crate::editor::Editor;
use crate::event::{Mouse, MouseKind};
use crate::key::{Code, Key};

const SEEDS: &[u64] = &[0, 1, 2];

const INPUTS: usize = 70;

const LIMIT: Duration = Duration::from_secs(60);

const SIZES: &[(u16, u16)] = &[(120, 30), (160, 36), (100, 24)];

#[test]
fn the_chats_transcript_follows_an_answer_being_written() {
    super::seeds("the_chats_transcript_follows_an_answer_being_written", SEEDS, run);
}

/// What's in sight of the transcript while its view has the keys: its top row, its selection (what arrives at it goes
/// under and after it: on the line being written too, it keeps its bytes), and the text of the rows in sight above
/// its last line (the line an answer grows at).
type Sight = (usize, crate::selection::Selection, Vec<String>);

/// What the scenario keeps between steps: what was in sight before text arrived while the view had the keys, and
/// the frame's size (the last one drawn, and whether this step drew it).
#[derive(Default)]
struct World {
    before: RefCell<Option<Sight>>,
    size: Cell<(u16, u16)>,
    framed: Cell<bool>,
}

fn run(choices: Choices) -> Vec<String> {
    let mut sim = Sim::new("the_chats_transcript_follows_an_answer_being_written", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    ed.docs[0].text = ropey::Rope::from_str("fn main() {}\n");
    let mut c = crate::chat::Chat::new();
    c.msgs = vec![
        Msg::new(Role::User, "why does insert clone the name?", ""),
        Msg::new(Role::Assistant, "Because the map keeps its own.\n\nThe needle is the key's owner.", ""),
        Msg::new(Role::Note, "3.1s", ""),
        Msg::new(Role::User, "and the second?", ""),
    ];
    let claude = crate::chat::stand_in(&mut c);
    ed.session.chat = Some(c);
    ed.session.at = crate::session::At::Chat;
    let world = World::default();
    world.size.set(SIZES[0]);
    frame(&mut ed, &world);
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

fn frame(ed: &mut Editor, world: &World) {
    let (w, h) = world.size.get();
    crate::term::render(ed, &mut Vec::new(), w, h).unwrap();
    world.framed.set(true);
}

/// The text of the transcript's row `i` as drawn.
fn row_text(ed: &Editor, i: usize) -> Option<String> {
    let t = chatdoc::transcript(ed)?;
    let s = t.row(i)?;
    Some(t.pane.doc_in()?.text.byte_slice(s.start..s.start + s.len).to_string())
}

/// What's in sight while the view has the keys (`Sight`).
fn in_sight(ed: &Editor) -> Option<Sight> {
    if !chatdoc::has_keys(ed) {
        return None;
    }
    let t = chatdoc::transcript(ed)?;
    let d = t.pane.doc_in()?;
    let (_, _, area, _) = ed.screen.chat_view.get()?;
    let last = crate::movement::last_line(&d.text);
    let top = t.top_row();
    let rows = (top..(top + area).min(t.rows()))
        .filter(|&i| t.row(i).is_some_and(|s| s.line < last))
        .filter_map(|i| row_text(ed, i))
        .collect();
    Some((top, d.selection().clone(), rows))
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

fn input(sim: &mut Sim, ed: &mut Editor, world: &World) -> String {
    world.framed.set(false);
    let c = &mut sim.choices;
    // (by arm: text arrives 6, a key 6, a question 1, the turn's end 1, the wheel 2, a frame 2, time passing 1)
    match c.weighted(&[6, 6, 1, 1, 2, 2, 1]) {
        0 => {
            let words = *c.pick(&[
                "word ",
                "another needle ",
                "\n\n",
                "a much longer run of words that wraps across the chat's rows wherever it is narrow enough ",
                "한글 넓은 글자 ",
            ]);
            *world.before.borrow_mut() = in_sight(ed);
            let Some(c) = ed.session.chat.as_mut() else { return "(no chat)".into() };
            if c.msgs.last().is_none_or(|m| m.role != Role::Assistant) || c.state == State::Idle {
                c.msgs.push(Msg::new(Role::Assistant, "", ""));
            }
            c.state = State::Writing;
            let m = c.msgs.last_mut().expect("an answer, above");
            m.text.push_str(words);
            m.changed();
            frame(ed, world);
            format!("{words:?} arrives")
        }
        1 => {
            let k = *c.pick(&[
                "<esc>",
                "<esc>",
                "i",
                "j",
                "k",
                "gg",
                "G",
                "x",
                "w",
                "vjj",
                "/needle<ret>",
                "n",
                "<C-d>",
                "<C-u>",
                "d",
                "u",
                "<ret>",
                "<C-w>h",
                "<C-w>l",
            ]);
            keys(ed, k);
            ed.focus_sync();
            format!("keys {k}")
        }
        2 => {
            let Some(c) = ed.session.chat.as_mut() else { return "(no chat)".into() };
            c.msgs.push(Msg::new(Role::User, "and then?", ""));
            c.msgs.push(Msg::new(Role::Assistant, "", ""));
            c.state = State::Thinking;
            "a question, its answer begun".into()
        }
        3 => {
            crate::chat::stand_in_done(ed);
            "the turn ends".into()
        }
        4 => {
            let up = c.below(2) == 0;
            let (Some(x), Some((y, _, area, _))) = (ed.screen.chat_x, ed.screen.chat_view.get()) else {
                return "(no chat drawn: no wheel over it)".into();
            };
            let row = c.below(area.max(1));
            let kind = if up { MouseKind::ScrollUp } else { MouseKind::ScrollDown };
            ed.handle_mouse(Mouse { kind, x: (x + 4) as u16, y: (y + row) as u16, alt: false, ctrl: false });
            ed.focus_sync();
            format!("the wheel {} over row {row}", if up { "up" } else { "down" })
        }
        5 => {
            let size = *c.pick(SIZES);
            world.size.set(size);
            frame(ed, world);
            format!("a frame, {}×{}", size.0, size.1)
        }
        _ => {
            let ms = *c.pick(&[100, 3000]);
            sim.wait(Duration::from_millis(ms));
            "time passes".into()
        }
    }
}

fn held(ed: &Editor, world: &World) -> Result<(), String> {
    if crate::docpane::swapped(ed).is_some() {
        return Err("a document left swapped in".into());
    }
    let Some(t) = chatdoc::transcript(ed) else { return Ok(()) };
    let d = t.pane.doc_in().ok_or("the transcript's document is out")?;
    // What the last frame laid out, line for line — nothing a key typed or deleted stays (read-only)
    let laid = ed.screen.chat_laid.borrow();
    if !laid.is_empty() {
        let want: String =
            laid.iter().filter(|(of, _)| *of != Of::Own).map(|(_, rows)| chatdoc::lines_of(rows).0).collect();
        if d.text != want.as_str() {
            return Err(format!(
                "the transcript's {} lines aren't the {} laid out",
                d.text.len_lines(),
                want.lines().count()
            ));
        }
    }
    if let Some(before) = world.before.borrow_mut().take()
        && let Some(now) = in_sight(ed)
    {
        if (before.0, &before.1) != (now.0, &now.1) {
            return Err(format!(
                "text arriving moved the view: top {} → {}, {:?} → {:?}",
                before.0, now.0, before.1, now.1
            ));
        }
        if let Some(i) = (0..before.2.len().min(now.2.len())).find(|&i| before.2[i] != now.2[i]) {
            return Err(format!("text arriving changed a row in sight: {:?} → {:?}", before.2[i], now.2[i]));
        }
        if !chatdoc::came(ed) {
            return Err("text arrived and the view doesn't say so".into());
        }
    }
    if !chatdoc::has_keys(ed) {
        return Ok(());
    }
    let Some((y, _, area, from)) = ed.screen.chat_view.get() else { return Ok(()) };
    let g = d.rows.as_deref().ok_or("a transcript with no rows")?;
    let cursor = d.selection().primary().cursor(&d.text);
    let line = crate::movement::line_of(&d.text, cursor);
    let at = g.index(line, crate::wrap::row_of(g.of(line), cursor));
    let top = t.top_row();
    if at < top || at >= top + area {
        return Err(format!("the cursor's row {at} out of the rows from {top} ({area})"));
    }
    // (after a frame: the rows drawn are the document's, from its top)
    let (Some((frame, w, h)), Some(x)) = (ed.last_frame.as_ref(), ed.screen.chat_x) else { return Ok(()) };
    if !world.framed.get() || (*w, *h) != world.size.get() {
        return Ok(());
    }
    if from != top {
        return Err(format!("drawn from row {from}, the view's top is {top}"));
    }
    let screen = crate::screen::text(frame, *w, *h).unwrap_or_default();
    for (i, shown) in screen.lines().skip(y).take(area).enumerate() {
        let Some(want) = row_text(ed, top + i) else { break };
        let shown: String = shown.chars().skip(x).collect();
        let want: String = want.trim().chars().take(16).collect();
        if !shown.contains(&want) {
            return Err(format!("row {i}: drawn {shown:?}, the document's row {} {want:?}", top + i));
        }
    }
    Ok(())
}

fn print(ed: &Editor) -> String {
    let keys = ed.keys_to().region.name();
    let doc = chatdoc::transcript(ed).and_then(|t| {
        let d = t.pane.doc_in()?;
        let line = crate::movement::line_of(&d.text, d.selection().primary().cursor(&d.text));
        Some(format!(
            "transcript at {}/{} from row {}",
            line + 1,
            crate::movement::last_line(&d.text) + 1,
            t.top_row()
        ))
    });
    let c = ed.session.chat.as_ref();
    format!(
        "keys {keys} · {} · {} messages, {:?}",
        doc.unwrap_or_else(|| "no transcript".into()),
        c.map_or(0, |c| c.msgs.len()),
        c.map(|c| c.state)
    )
}
