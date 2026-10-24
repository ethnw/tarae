//! Presence, simulated (collab C1, `presence.rs`): a session behind at work in its own checkout — its Claude's gaze
//! moving, an edit of its being written there, its turn ending and the next beginning, its session closing and
//! coming back — while you type in the same file in another checkout of the repository, another file comes to the
//! front and back, and frames are drawn at two widths; the line maps worked out on workers land among the steps. After every step: a mark is drawn only within
//! the buffer's lines, never empty; nothing is drawn of a session gone or between turns, nor with one session open
//! alone (what was gathered lags until the next event: what's drawn doesn't); and every line map kept is for a buffer
//! in sight.

use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::Editor;
use crate::key::{Code, Key};

const SEEDS: &[u64] = &[0, 1, 2];

const INPUTS: usize = 60;

const LIMIT: Duration = crate::test_results::PATIENCE;

const TEXT: &str = "fn main() {\n    let a = 1;\n    let b = 2;\n    let c = 3;\n    let d = 4;\n}\n";

#[test]
fn presence_follows_sessions_across_checkouts() {
    super::seeds("presence_follows_sessions_across_checkouts", SEEDS, run);
}

struct World {
    /// The second checkout (the session behind's) and its session — None while it's closed.
    fix: PathBuf,
    session: Option<crate::event::SessionId>,
    turns: usize,
}

fn run(choices: Choices) -> Vec<String> {
    let mut sim = Sim::new("presence_follows_sessions_across_checkouts", choices, LIMIT);
    let dir = crate::scratch::Scratch::new("sim-presence");
    let main = dir.join("main");
    std::fs::create_dir_all(&main).unwrap();
    let main = std::fs::canonicalize(main).unwrap();
    std::fs::write(main.join("a.rs"), TEXT).unwrap();
    std::fs::write(main.join("b.rs"), "// b\n").unwrap();
    for args in [&["init", "-q", "-b", "main"][..], &["add", "-A"], &["commit", "-q", "-m", "base"]] {
        super::sessions::git(&main, &[&["-c", "user.name=t", "-c", "user.email=t@t"][..], args].concat());
    }
    super::sessions::git(&main, &["worktree", "add", "-q", "../fix"]);
    let fix = std::fs::canonicalize(dir.join("fix")).unwrap();
    std::fs::write(fix.join("a.rs"), format!("// fix\n{TEXT}")).unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = main.clone();
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    ed.open(&main.join("a.rs")).unwrap();
    let world = RefCell::new(World { fix, session: None, turns: 0 });
    open(&mut ed, &mut world.borrow_mut());
    crate::term::render(&mut ed, &mut Vec::new(), 100, 30).unwrap();
    let check = |ed: &Editor| held(ed, &world.borrow());
    sim.walk(&mut ed, INPUTS, |sim, ed| step(sim, ed, &mut world.borrow_mut()), check, print);
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

/// The second checkout's session, behind, its Claude in a turn (as the stand-in has it).
fn open(ed: &mut Editor, w: &mut World) {
    let id = ed.session_add(w.fix.clone());
    if let Some(l) = ed.session_by_id(id) {
        l.name = "fix".into();
        let mut c = crate::chat::Chat::new();
        let _ = crate::chat::stand_in(&mut c);
        c.state = crate::chat::State::Thinking;
        c.follow.on = true;
        l.chat = Some(c);
    }
    w.session = Some(id);
}

fn chat<'a>(ed: &'a mut Editor, w: &World) -> Option<&'a mut crate::chat::Chat> {
    let id = w.session?;
    ed.sessions.iter_mut().find(|l| l.id == id)?.chat.as_mut()
}

fn step(sim: &mut Sim, ed: &mut Editor, w: &mut World) -> String {
    let c = &mut sim.choices;
    // (by arm: a gaze 3, an edit being written 2, a key 3, a frame 2, the turn's end 1, a new turn 1, the session
    // closed 1, back 1, another buffer in front 1, the mouse moving 1, time 1)
    match c.weighted(&[3, 2, 3, 2, 1, 1, 1, 1, 1, 1, 1]) {
        0 => {
            let line = *c.pick(&[0, 1, 2, 4, 6]);
            let path = w.fix.join("a.rs");
            let Some(ch) = chat(ed, w).filter(|c| c.busy()) else { return "(no turn to look in)".into() };
            let look = crate::chat::Look {
                verb: "read",
                what: String::new(),
                pattern: false,
                range: String::new(),
                link: None,
            };
            ch.follow.gaze = Some(crate::follow::Gaze::new(look, Some(path), Some(line..line + 2)));
            format!("its Claude reads lines {}–{}", line + 1, line + 2)
        }
        1 => {
            let line = *c.pick(&[1, 3, 5]);
            let path = w.fix.join("a.rs");
            let Some(ch) = chat(ed, w).filter(|c| c.busy()) else { return "(no turn to write in)".into() };
            let mut it = crate::writing::Item::default();
            it.place = crate::writing::Place::Found(crate::writing::Found {
                path,
                from: 0,
                to: 0,
                seen: None,
                bound: None,
                held: String::new(),
                lines: (line, line + 1),
                protected: false,
            });
            ch.writing.calls = vec![crate::writing::Call {
                id: format!("t{}", w.turns),
                tool: crate::writing::Tool::Edit,
                items: vec![it],
                done: false,
                reviewed: Vec::new(),
            }];
            format!("its Claude writes over lines {}–{}", line + 1, line + 2)
        }
        2 => {
            let k = *c.pick(&["ggO// x<esc>", "Gdd", "u", "j", "<esc>"]);
            keys(ed, k);
            format!("keys {k}")
        }
        3 => {
            let width = *c.pick(&[100, 160]);
            crate::term::render(ed, &mut Vec::new(), width, 30).unwrap();
            format!("a frame, {width} columns")
        }
        4 => {
            let Some(ch) = chat(ed, w).filter(|c| c.busy()) else { return "(no turn to end)".into() };
            ch.state = crate::chat::State::Idle;
            ch.writing.calls.clear();
            ch.follow.gaze = None;
            "its turn ends".into()
        }
        5 => {
            let Some(ch) = chat(ed, w).filter(|c| !c.busy()) else { return "(no idle chat to ask)".into() };
            ch.state = crate::chat::State::Thinking;
            w.turns += 1;
            "a new turn".into()
        }
        6 => {
            if w.session.take().is_none() {
                return "(no session to close)".into();
            }
            let _ = ed.session_close(&w.fix.clone());
            "its session closes".into()
        }
        7 => {
            if w.session.is_some() {
                return "(its session is open)".into();
            }
            open(ed, w);
            "its session again".into()
        }
        8 => {
            let other =
                if ed.doc().path.as_deref().is_some_and(|p| p.ends_with("a.rs")) { "b.rs" } else { "a.rs" };
            keys(ed, &format!(":e {other}<ret>"));
            format!("{other} in front")
        }
        // (over the sign column's rows — a session's bar there, or nothing — or away)
        9 => {
            let (x, y) = *c.pick(&[(0, 1), (0, 2), (0, 3), (0, 5), (40, 0)]);
            ed.handle_event(crate::event::Event::Hover(x, y));
            format!("the mouse at {x},{y}")
        }
        _ => {
            let ms = *c.pick(&[30, 2000, 4000]);
            sim.wait(Duration::from_millis(ms));
            "time passes".into()
        }
    }
}

fn keys(ed: &mut Editor, keys: &str) {
    let mut chars = keys.chars();
    while let Some(ch) = chars.next() {
        let k: Key = match ch {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            ch => Key::plain(Code::Char(ch)),
        };
        ed.handle_event(crate::event::Event::Key(k));
    }
}

fn held(ed: &Editor, w: &World) -> Result<(), String> {
    let doc = ed.doc();
    let last = crate::movement::last_line(&doc.text);
    let marks = crate::presence::marks(ed, doc);
    let cursor = doc.text.byte_to_line(doc.selection().primary().cursor(&doc.text).min(doc.text.len_bytes()));
    for m in &marks {
        if m.lines.is_empty() || m.lines.end > last + 1 {
            return Err(format!("a mark over lines {:?} of a buffer of {} lines", m.lines, last + 1));
        }
        // (its tag: shown with a reason — writing, the mouse over it, your cursor on it, a write within its stay and
        // fade — and always while it writes)
        let reason = m.act == crate::presence::Act::Writing
            || ed.presence.hover == Some(m.session)
            || m.lines.contains(&cursor)
            || ed
                .presence
                .wrote_since(m.session)
                .is_some_and(|e| e < crate::presence::TAG_STAY + crate::presence::TAG_FADE);
        if m.tag > 0 && !reason {
            return Err(format!("{}'s tag shown ({}%) with nothing new to say", m.name, m.tag));
        }
        if m.here && m.act == crate::presence::Act::Writing && m.tag != 100 {
            return Err(format!("{}'s tag not whole while it writes", m.name));
        }
        let busy = chat_of(ed, m.session).is_some_and(|c| c.busy());
        if !busy {
            return Err(format!("a mark of session {} with no turn under way", m.session));
        }
    }
    if w.session.is_none() && !marks.is_empty() {
        return Err("a mark of a session closed".into());
    }
    // (what was gathered lags a session closed until the next event: what's drawn and said doesn't)
    if ed.sessions.is_empty() && (!marks.is_empty() || crate::presence::at(ed, ed.session.id).is_some()) {
        return Err("a presence drawn with one session open".into());
    }
    // (the line maps kept: each for a buffer in sight)
    let sight: Vec<_> = ed.session.views.iter().map(|v| v.doc).chain([ed.doc().id]).collect();
    if let Some(d) = ed.presence.mapped().into_iter().find(|d| !sight.contains(d)) {
        return Err(format!("a line map kept for a buffer out of sight: {d:?}"));
    }
    Ok(())
}

fn chat_of(ed: &Editor, id: crate::event::SessionId) -> Option<&crate::chat::Chat> {
    std::iter::once(&ed.session).chain(&ed.sessions).find(|l| l.id == id)?.chat.as_ref()
}

fn print(ed: &Editor) -> String {
    let marks = crate::presence::marks(ed, ed.doc());
    let said: Vec<String> =
        marks.iter().map(|m| format!("{} {:?} {}", m.name, m.lines, m.act.word())).collect();
    format!("{} sessions · marks [{}]", ed.sessions.len() + 1, said.join(", "))
}
