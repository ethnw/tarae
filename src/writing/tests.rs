//! Live edits (`writing.rs`): what Claude writes drawn as it's written, the review opening on the same rows.

use serde_json::{Value, json};

use std::time::{Duration, Instant};

use super::*;
use crate::chat::State;
use crate::config::Config;
use crate::key::{Code, Key};
use crate::llm::StreamEv;

const TEXT: &str = "fn main() {\n    let a = 1;\n    let b = 2;\n    println!(\"{a} {b}\");\n}\n";

/// A project with `a.rs` open, a chat at work (a stand-in claude, follow on) — its generation.
fn scene(name: &str) -> (Editor, crate::scratch::Scratch, u64) {
    let dir = crate::scratch::Scratch::new(name);
    std::fs::write(dir.join("a.rs"), TEXT).unwrap();
    let mut ed = Editor::new(Config::default());
    ed.session.root = dir.to_path_buf();
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    ed.open(&dir.join("a.rs")).unwrap();
    let mut c = crate::chat::Chat::new();
    let _ = crate::chat::stand_in(&mut c);
    c.state = State::Thinking;
    c.follow.on = true;
    // (you in the code while Claude works: the keys stay `At::Code`)
    let g = c.generation();
    ed.session.chat = Some(c);
    (ed, dir, g)
}

fn keys(ed: &mut Editor, keys: &str) {
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let k: Key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        };
        ed.handle_key(k);
    }
}

/// The line follow keeps in sight (collab C1: follow moves the view, never your cursor).
fn led(ed: &Editor) -> Option<usize> {
    crate::follow::view_line(ed)
}

/// Events until `done` holds (a place is looked for on a worker).
fn settle(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !done(ed) {
        let left = end.saturating_duration_since(std::time::Instant::now());
        let ev = ed.events.recv_timeout(left).expect("an event before the time's up");
        ed.handle_event(ev);
    }
}

/// A call's input as the stream brings it, its reader kept between pieces as the reader thread keeps it.
struct Writer {
    call: String,
    tool: Tool,
    scan: Scan,
}

impl Writer {
    fn new(call: &str, tool: Tool) -> Writer {
        Writer { call: call.into(), tool, scan: Scan::new(tool) }
    }

    /// `json` cut into pieces at `cuts` (character counts), each sent as it would come — `done`: the last with its
    /// block's end.
    fn send(&mut self, ed: &mut Editor, g: u64, json: &str, cuts: &[usize], done: bool) {
        let chars: Vec<char> = json.chars().collect();
        let mut at = 0;
        for &cut in cuts.iter().chain([&chars.len()]) {
            let piece: String = chars[at..cut.min(chars.len())].iter().collect();
            at = cut.min(chars.len());
            self.scan.feed(&piece);
            let done = done && at == chars.len();
            let d = Delta { call: self.call.clone(), tool: self.tool, pieces: self.scan.take(), done };
            crate::chat::stream_event(ed, g, StreamEv::Writing(d));
        }
    }
}

/// A whole call's input, sent in pieces at `cuts`.
fn write(ed: &mut Editor, g: u64, call: &str, tool: Tool, json: &str, cuts: &[usize]) {
    Writer::new(call, tool).send(ed, g, json, cuts, true);
}

/// The code area's rows of a frame `w` wide (the chat's columns left out).
fn frame(ed: &mut Editor, w: u16, h: u16) -> Vec<String> {
    let mut buf = Vec::new();
    crate::term::render(ed, &mut buf, w, h).unwrap();
    let code = ed.screen.chat_x.unwrap_or(w as usize);
    let text = crate::screen::text(&buf, w, h).unwrap();
    // (Claude's caret right after a row's text is left out — the rows read as their text; a scrollbar's `▎`
    // after spaces stays)
    let uncaret = |r: String| match r.strip_suffix('▎') {
        Some(t) if !t.is_empty() && !t.ends_with(' ') => t.to_string(),
        _ => r,
    };
    text.lines().map(|l| uncaret(l.chars().take(code).collect::<String>().trim_end().to_string())).collect()
}

/// tarae's edit replacing line 3 with `new`, its keys in the order the model writes them (serde's own `to_string`
/// sorts them: `new_string` would come first).
fn edit_input(new: &str) -> String {
    let s = |t: &str| serde_json::to_string(t).unwrap();
    format!(
        r#"{{"edits":[{{"file_path":"a.rs","old_string":{},"new_string":{}}}]}}"#,
        s("    let b = 2;\n"),
        s(new)
    )
}

/// The reader thread's part: a tool call's input read from its block's lines — each piece once, a block of another
/// tool or text left alone, the block's end said.
#[test]
fn the_stream_s_edit_input_is_read_as_it_comes() {
    let line = |e: Value| json!({"type": "stream_event", "event": e});
    let mut r = Reading::default();
    let start = |i: u64, name: &str| {
        line(json!({"type": "content_block_start", "index": i,
            "content_block": {"type": "tool_use", "id": format!("t{i}"), "name": name, "input": {}}}))
    };
    let piece = |i: u64, p: &str| {
        line(json!({"type": "content_block_delta", "index": i,
            "delta": {"type": "input_json_delta", "partial_json": p}}))
    };
    let stop = |i: u64| line(json!({"type": "content_block_stop", "index": i}));
    assert!(!r.line(&start(0, "Read")), "not an edit");
    assert!(!r.line(&piece(0, "{\"file_path\"")));
    assert!(r.line(&start(1, "mcp__tarae__edit")));
    assert!(r.line(&piece(1, r#"{"edits":[{"file_path":"a.rs","new_string":"fn "#)));
    assert!(r.line(&piece(1, r#"ma"#)));
    let text = json!({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0,
        "delta": {"type": "text_delta", "text": "hi"}}});
    assert!(!r.line(&text), "text is the pump's own");
    let sent = r.take();
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].call, "t1");
    assert!(!sent[0].done);
    assert_eq!(
        sent[0].pieces,
        [
            Piece::Text { item: 0, field: Field::File, add: "a.rs".into(), closed: true },
            Piece::Text { item: 0, field: Field::New, add: "fn ma".into(), closed: false },
        ],
        "one piece a field: the batch's pieces carried on"
    );
    assert!(r.line(&piece(1, "in\"}]}")));
    assert!(r.line(&stop(1)));
    let sent = r.take();
    assert!(sent[0].done, "its block ended");
    assert_eq!(sent[0].pieces, [Piece::Text { item: 0, field: Field::New, add: "in".into(), closed: true }]);
    assert!(r.take().is_empty(), "each piece once");
}

/// What Claude writes is drawn where it goes as it comes — the line it replaces washed, the lines written so far on
/// the proposed wash, the head wet — and nothing else: the buffer as it was, unsaved nothing, the keys the code's.
#[test]
fn an_edit_is_drawn_as_it_is_written() {
    let (mut ed, _dir, g) = scene("writing-drawn");
    ed.wet_ink = true;
    let v0 = ed.doc().version();
    let input = edit_input("    let b = 20;\n    let c = a + b;\n");
    // (to the middle of the second line it writes)
    let mid = input.find("let c").unwrap() + "let c = a".len();
    // (its place in sight: the screen holds still — the view held on the line under the row it writes, your cursor
    // where it was, the keys in the chat or not)
    ed.session.at = crate::session::At::Chat;
    frame(&mut ed, 100, 30);
    let mut w = Writer::new("t1", Tool::Edit);
    w.send(&mut ed, g, &input[..mid], &[20, 60], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    frame(&mut ed, 100, 30);
    w.send(&mut ed, g, "", &[], false);
    assert_eq!(
        (led(&ed), ed.cursor_line(), ed.doc().top),
        (Some(2), 0, 0),
        "the view on the line it rewrites, your cursor where it was, the screen still"
    );
    ed.session.leave(crate::session::At::Chat);
    crate::term::WET_SEEN.with_borrow_mut(Vec::clear);
    let rows = frame(&mut ed, 100, 30);
    let at =
        |t: &str| rows.iter().position(|r| r.ends_with(t)).unwrap_or_else(|| panic!("{t:?} in {rows:#?}"));
    let (gone, b, c) = (at("let b = 2;"), at("let b = 20;"), at("let c = a"));
    assert_eq!((b, c), (gone + 1, gone + 2), "under the line it replaces: {rows:#?}");
    assert!(rows[c + 1].ends_with(r#"println!("{a} {b}");"#), "then the file as it is: {rows:#?}");
    let wet = crate::term::WET_SEEN
        .with_borrow(|w| w.iter().filter(|w| w.0 == "code").map(|w| w.1.clone()).collect::<Vec<_>>());
    assert_eq!(wet, ["= a"], "its head's last words wet");
    let doc = ed.doc();
    assert_eq!((doc.text.to_string().as_str(), doc.version()), (TEXT, v0), "nothing applied");
    assert!(!doc.is_modified());
    // The keys are the code's: `j` moves the cursor
    let before = ed.doc().selection().primary().head;
    assert_eq!(ed.keys_to().region.name(), "code");
    keys(&mut ed, "j");
    assert_ne!(ed.doc().selection().primary().head, before, "j moved");
    assert!(ed.session.review.is_none());
    // The state says it, and Claude's level
    let s = ed.ui_state();
    assert_eq!(s.sessions[0].writing[0].lines, Some((3, 3)));
    assert!(!s.sessions[0].writing[0].whole);
    assert!(ed.ui_state_claude().text_claude(false).contains("writing (drawn as it comes"));
    // Its text whole: dry
    w.send(&mut ed, g, &input[mid..], &[], true);
    crate::term::WET_SEEN.with_borrow_mut(Vec::clear);
    frame(&mut ed, 100, 30);
    assert!(crate::term::WET_SEEN.with_borrow(|w| w.iter().all(|w| w.0 != "code")), "nothing wet once whole");
    // The turn's end takes it away
    crate::chat::stream_event(&mut ed, g, StreamEv::Done(Ok(String::new())));
    assert!(ed.doc().writing.is_none());
    assert!(!frame(&mut ed, 100, 30).iter().any(|r| r.ends_with("let b = 20;")));
}

/// Once the call is made the review opens on the rows that were drawn — each at the screen row it was on: follow
/// took the editor to a place out of sight and put its rows where the review shows its change. (Near a file's
/// top there's no room above for the review's title and the change's head: it opens by its own rule, as always,
/// the code a few rows lower.)
#[test]
fn the_review_opens_on_the_same_rows() {
    let (mut ed, dir, g) = scene("writing-review");
    let mut text: String = (1..=60).map(|i| format!("    let x{i} = {i};\n")).collect();
    text.insert_str(text.find("    let x40").unwrap(), TEXT);
    std::fs::write(dir.join("a.rs"), &text).unwrap();
    crate::typed::execute(&mut ed, "reload").unwrap();
    frame(&mut ed, 100, 30);
    let input = edit_input("    let b = 20;\n    let c = a + b;\n");
    write(&mut ed, g, "t1", Tool::Edit, &input, &[30, 70]);
    let input: Value = serde_json::from_str(&input).unwrap();
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    let live = frame(&mut ed, 100, 30);
    crate::chat::stream_event(
        &mut ed,
        g,
        StreamEv::Tool { id: "t1".into(), name: "mcp__tarae__edit".into(), input: input.clone() },
    );
    let request = json!({"subtype": "mcp_message", "server_name": "tarae", "message": {"jsonrpc": "2.0",
        "id": 2, "method": "tools/call", "params": {"name": "edit", "arguments": input}}});
    crate::chat::stream_event(&mut ed, g, StreamEv::Control { request_id: "c1".into(), request });
    settle(&mut ed, |ed| ed.session.review.is_some());
    let review = frame(&mut ed, 100, 30);
    let rows = |f: &[String], t: &str| f.iter().position(|r| r.ends_with(t));
    for t in ["let b = 2;", "let b = 20;", "let c = a + b;", r#"println!("{a} {b}");"#] {
        let (a, b) = (rows(&live, t), rows(&review, t));
        assert!(a.is_some() && a == b, "{t:?}: {a:?} live, {b:?} in the review\n{live:#?}\n{review:#?}");
        assert_eq!(live[a.unwrap()], review[a.unwrap()], "the same row");
    }
    assert!(ed.doc().writing.is_none(), "the review's now");
}

/// Not drawn where it can't be told where it goes: `old_string` in two places, a file outside the project. An edit
/// of yours elsewhere in the file keeps it, moved with what it replaces; one in what it replaces ends it for good.
#[test]
fn an_edit_with_no_one_place_is_not_drawn() {
    let (mut ed, dir, g) = scene("writing-nowhere");
    let twice = json!({"edits": [{"file_path": "a.rs", "old_string": "let", "new_string": "const"}]});
    write(&mut ed, g, "t1", Tool::Edit, &twice.to_string(), &[]);
    let outside = json!({"file_path": "/etc/hosts", "old_string": "localhost", "new_string": "x"});
    write(&mut ed, g, "t2", Tool::NativeEdit, &outside.to_string(), &[]);
    let looked = |ed: &Editor| {
        let c = ed.session.chat.as_ref().unwrap();
        c.writing.calls.iter().flat_map(|c| &c.items).all(|it| matches!(it.place, Place::Nowhere))
    };
    settle(&mut ed, looked);
    assert!(ed.doc().writing.is_none());
    assert_eq!(ed.ui_state().sessions[0].writing[0].lines, None);
    // One place; your line above it: still drawn, a line lower
    let once = edit_input("    let b = 3;\n");
    let mut w = Writer::new("t3", Tool::Edit);
    w.send(&mut ed, g, &once[..once.len() - 8], &[], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    ed.doc_mut().set_selection(crate::selection::Selection::point(0));
    keys(&mut ed, "O// yours<esc>");
    w.send(&mut ed, g, &once[once.len() - 8..once.len() - 4], &[], false);
    let b = shown(ed.doc()).map(|s| (s.blocks[0].first, s.blocks[0].last));
    assert_eq!(b, Some((3, 3)), "moved with the line it replaces");
    assert!(frame(&mut ed, 100, 30).iter().any(|r| r.contains("▎    let b = 3")));
    // Your key in the line it replaces: gone for good
    let inside = ed.doc().text.to_string().find("let b = 2").unwrap() + 4;
    ed.doc_mut().set_selection(crate::selection::Selection::point(inside));
    keys(&mut ed, "ix<esc>");
    w.send(&mut ed, g, &once[once.len() - 4..], &[], true);
    assert!(shown(ed.doc()).is_none(), "what it replaces changed under it");
    assert!(!frame(&mut ed, 100, 30).iter().any(|r| r.ends_with("let b = 3;")));
    drop(dir);
}

/// The change landing unasked (Claude's write, its file reloaded) over rows drawn whole: they go — also when
/// what it writes holds what it replaced (an edit that adds lines), never its lines drawn twice.
#[test]
fn a_change_that_lands_takes_its_rows() {
    let (mut ed, dir, g) = scene("writing-landed");
    let input = edit_input("    let b = 2;\n    let c = 3;\n");
    write(&mut ed, g, "t1", Tool::Edit, &input, &[]);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    std::fs::write(dir.join("a.rs"), TEXT.replace("    let b = 2;\n", "    let b = 2;\n    let c = 3;\n"))
        .unwrap();
    crate::typed::execute(&mut ed, "reload").unwrap();
    // (another call's piece comes before this one's result: the rows are laid out again)
    Writer::new("t2", Tool::Edit).send(&mut ed, g, r#"{"edits":[{"file_path":"a.rs""#, &[], false);
    assert!(shown(ed.doc()).is_none(), "the landed change's rows");
    let rows = frame(&mut ed, 100, 30);
    assert_eq!(rows.iter().filter(|r| r.ends_with("let c = 3;")).count(), 1, "once: {rows:#?}");
}

/// A write longer than the screen: follow keeps the line being written in sight as it comes, the screen moving
/// down with it — and the cursor where follow put it, on the line it rewrites.
#[test]
fn the_head_stays_in_sight_as_it_is_written() {
    let (mut ed, dir, g) = scene("writing-head");
    let long: String = (1..=60).map(|i| format!("line {i}\n")).collect();
    std::fs::write(dir.join("a.rs"), &long).unwrap();
    ed.open(&dir.join("a.rs")).unwrap();
    crate::typed::execute(&mut ed, "reload").unwrap();
    frame(&mut ed, 100, 30);
    let s = |t: &str| serde_json::to_string(t).unwrap();
    let new: String = (1..=40).map(|i| format!("written {i}\n")).collect();
    let input = format!(
        r#"{{"edits":[{{"file_path":"a.rs","old_string":{},"new_string":{}}}]}}"#,
        s("line 10\n"),
        s(&new)
    );
    let mut w = Writer::new("t1", Tool::Edit);
    let upto = |n: usize| input.find(&format!("written {n}\\n")).unwrap() + format!("written {n}").len();
    w.send(&mut ed, g, &input[..upto(1)], &[], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    let mut at = upto(1);
    for n in [5, 15, 25, 35] {
        w.send(&mut ed, g, &input[at..upto(n)], &[], false);
        at = upto(n);
        let rows = frame(&mut ed, 100, 30);
        // (the follow card may cover a row's end)
        assert!(rows.iter().any(|r| r.contains(&format!("▎written {n}"))), "written {n} in sight: {rows:#?}");
    }
    assert_eq!(
        (led(&ed), ed.cursor_line()),
        (Some(9), 0),
        "the view kept it in sight, your cursor never led"
    );
}

/// A key as the person presses it: through the event loop, so follow sees it (`follow::watch`).
fn press(ed: &mut Editor, keys: &str) {
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let k: Key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        };
        ed.handle_event(crate::event::Event::Key(k));
    }
}

/// The status line's left words (`● normal`, `● follow`) — the frame's last row.
fn status(rows: &[String]) -> String {
    rows.last().cloned().unwrap_or_default()
}

/// Lines 1–3 rewritten, the first two kept: written a line at a time, each piece ending a line — the head on the
/// line it keeps.
fn kept_lines_input() -> String {
    let s = |t: &str| serde_json::to_string(t).unwrap();
    let old = "    let a = 1;\n    let b = 2;\n    println!(\"{a} {b}\");\n";
    let new = "    let a = 1;\n    let b = 2;\n    println!(\"{a}\");\n";
    format!(r#"{{"edits":[{{"file_path":"a.rs","old_string":{},"new_string":{}}}]}}"#, s(old), s(new))
}

/// The cell right after a text on the frame, and where the terminal's own cursor is.
struct After {
    fg: vt100::Color,
    bg: vt100::Color,
    text: String,
    /// (row, column); None: hidden
    cursor: Option<(u16, u16)>,
    at: (u16, u16),
}

impl After {
    /// Claude's caret: an insert caret's bar in the accent, on the row's own ground (not a block of colour).
    fn is_caret(&self, accent: vt100::Color) -> bool {
        self.text == "▎" && self.fg == accent && self.bg != accent
    }
}

/// The frame as a terminal shows it: the cell right after `needle` on the row it ends (that row ending with it, or
/// with it and Claude's caret).
fn after(ed: &mut Editor, needle: &str) -> After {
    crossterm::style::force_color_output(true);
    let mut bytes = Vec::new();
    crate::term::render(ed, &mut bytes, 100, 30).unwrap();
    let mut parser = vt100::Parser::new(30, 100, 0);
    parser.process(&bytes);
    let screen = parser.screen();
    let with_caret = format!("{needle}▎");
    let ends = |r: &str| r.trim_end().ends_with(needle) || r.trim_end().ends_with(&with_caret);
    let (y, row) = screen.rows(0, 100).enumerate().find(|(_, r)| ends(r)).expect(needle);
    let x = (row[..row.rfind(needle).unwrap()].chars().count() + needle.chars().count()) as u16;
    let cell = screen.cell(y as u16, x).unwrap();
    let cursor = (!screen.hide_cursor()).then(|| screen.cursor_position());
    After {
        fg: cell.fgcolor(),
        bg: cell.bgcolor(),
        text: cell.contents().to_string(),
        cursor,
        at: (y as u16, x),
    }
}

/// (a) An edit written live with the keys in the code: follow keeps the line it rewrites in sight — the first it takes
/// out — at once, never paced, moving with it when a line it writes turns out kept; your cursor stays where it was
/// (collab C1: follow moves the view, never your cursor). Claude's caret, a cell in the accent, rides the end of the
/// row being written with every piece (not the terminal's cursor: that's where the keys are); the status line says
/// `● follow`. Your key pauses it, said, acts at your cursor, and the view comes back to it.
#[test]
fn follow_keeps_the_line_being_rewritten_in_sight() {
    let (mut ed, _dir, g) = scene("writing-lead");
    ed.config.llm.follow_resume = 0;
    frame(&mut ed, 100, 30);
    assert!(status(&frame(&mut ed, 100, 30)).contains("● normal"), "not led yet");
    let input = kept_lines_input();
    let upto = |t: &str| input.rfind(t).unwrap() + t.len();
    let mut w = Writer::new("t1", Tool::Edit);
    w.send(&mut ed, g, &input[..upto(r"let a = 1;\n")], &[], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    w.send(&mut ed, g, "", &[], false);
    assert_eq!(
        (led(&ed), ed.cursor_line(), ed.doc().top),
        (Some(2), 0, 0),
        "at once, on the first line it takes out; your cursor where it was; the screen still"
    );
    assert!(status(&frame(&mut ed, 100, 30)).contains("● follow"));
    // The next line it writes is one it keeps: the line it rewrites is the one after — the view there at once
    w.send(&mut ed, g, &input[upto(r"let a = 1;\n")..upto(r"let b = 2;\n")], &[], false);
    assert_eq!(led(&ed), Some(3), "not paced");
    // The caret at the end of what's written, piece by piece — the terminal's cursor elsewhere
    let accent = claude_colour(&ed);
    let mut at = upto(r"let b = 2;\n");
    for (json, t) in [("    print", "    print"), (r#"    println!(\"{a"#, r#"    println!("{a"#)] {
        w.send(&mut ed, g, &input[at..upto(json)], &[], false);
        at = upto(json);
        let a = after(&mut ed, t);
        assert!(a.is_caret(accent), "{t:?}: the caret right after it ({:?} {:?})", a.text, a.fg);
        assert_ne!(a.cursor, Some(a.at), "{t:?}: a mark, not the terminal's cursor");
        assert_eq!((led(&ed), ed.cursor_line()), (Some(3), 0), "{t:?}: the view on the line rewritten");
    }
    // Your key: follow pauses, says so, leads no more; it acted at your cursor, and the view is yours again
    press(&mut ed, "j");
    assert_eq!((ed.cursor_line(), led(&ed)), (1, None));
    let f = &ed.session.chat.as_ref().unwrap().follow;
    assert!(f.paused, "your key paused it");
    assert!(ed.toasts.iter().any(|t| t.text.contains("you took over")), "said");
    assert!(status(&frame(&mut ed, 100, 30)).contains("● normal"));
}

/// A piece that ends right at the end of a line the edit keeps: the row being written is that line, and the caret
/// sits after its last character — on every frame while the writing goes on, never a frame without it; it leaves the
/// line before as it goes on.
#[test]
fn the_caret_never_misses_a_frame_on_a_line_kept() {
    let (mut ed, _dir, g) = scene("writing-caret-kept");
    frame(&mut ed, 100, 30);
    let accent = claude_colour(&ed);
    let input = kept_lines_input();
    let upto = |t: &str| input.rfind(t).unwrap() + t.len();
    let mut w = Writer::new("t1", Tool::Edit);
    w.send(&mut ed, g, &input[..upto(r"let a = 1;\n")], &[], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    w.send(&mut ed, g, "", &[], false);
    assert_eq!(crate::writing::caret_line(ed.doc()), Some(1), "the row being written is a line it keeps");
    let a = after(&mut ed, "    let a = 1;");
    assert!(a.is_caret(accent), "the caret after the kept line's last character");
    assert_ne!(a.cursor, Some(a.at), "a mark, not the terminal's cursor");
    w.send(&mut ed, g, &input[upto(r"let a = 1;\n")..upto(r"let b = 2;\n")], &[], false);
    let b = after(&mut ed, "    let b = 2;");
    assert!(b.is_caret(accent), "on the next kept line");
    assert!(!after(&mut ed, "    let a = 1;").is_caret(accent), "gone from the line before");
    // Into a line it writes: the caret on that row's end again
    w.send(&mut ed, g, &input[upto(r"let b = 2;\n")..upto("    print")], &[], false);
    assert!(after(&mut ed, "    print").is_caret(accent));
    assert!(!after(&mut ed, "    let b = 2;").is_caret(accent));
}

/// Claude's caret is a bar and the person's cursor a block: they never look alike. In both built-in themes, the keys
/// in the code or in the chat (the pane faded), the bar reads at a glance on the writing row's wash — WCAG 2's floor
/// for a mark that isn't text, 3:1.
#[test]
fn the_caret_is_a_bar_that_reads_on_the_writing_rows_wash() {
    let rgb = |c: vt100::Color| match c {
        vt100::Color::Rgb(r, g, b) => [r, g, b],
        c => panic!("a true colour: {c:?}"),
    };
    for theme in ["meok", "hanji"] {
        for at in [crate::session::At::Code, crate::session::At::Chat] {
            let (mut ed, _dir, g) = scene("writing-caret-reads");
            ed.theme = crate::theme::load(theme).unwrap();
            ed.session.at = at;
            frame(&mut ed, 100, 30);
            let input = edit_input("    let b = 20;\n    let c = a");
            let mut w = Writer::new("t1", Tool::Edit);
            w.send(&mut ed, g, &input[..input.len() - 4], &[], false);
            settle(&mut ed, |ed| shown(ed.doc()).is_some());
            w.send(&mut ed, g, "", &[], false);
            let accent = claude_colour(&ed);
            let a = after(&mut ed, "    let c = a");
            assert!(a.is_caret(accent), "{theme} {at:?}: a bar in the accent ({:?})", a.text);
            let r = crate::chrome::contrast(rgb(a.fg), rgb(a.bg));
            assert!(r >= 3.0, "{theme} {at:?}: the caret on the wash {r:.2} (floor 3)");
            // The person's cursor, where it was (follow never moves it): a block of the cursor's colour, never a bar
            if at == crate::session::At::Code {
                let mut bytes = Vec::new();
                crate::term::render(&mut ed, &mut bytes, 100, 30).unwrap();
                let mut parser = vt100::Parser::new(30, 100, 0);
                parser.process(&bytes);
                let screen = parser.screen();
                let (y, row) = screen
                    .rows(0, 100)
                    .enumerate()
                    .find(|(_, r)| r.contains("fn main() {"))
                    .expect("its row");
                let x = row[..row.find("fn main() {").unwrap()].chars().count();
                let cell = screen.cell(y as u16, x as u16).unwrap();
                let block = ed.theme.try_get("ui.cursor.primary").and_then(|s| s.bg).map(|c| match c {
                    crossterm::style::Color::Rgb { r, g, b } => vt100::Color::Rgb(r, g, b),
                    c => panic!("{c:?}"),
                });
                assert_eq!(Some(cell.bgcolor()), block, "{theme}: the person's cursor a block");
                assert_ne!(cell.contents(), "▎", "{theme}: not a bar");
            }
        }
    }
}

/// A key sequence under way — `g`…, a count, `space` (which-key), `C-w` (the window prefix, the way to the chat) —
/// isn't taking over: follow holds the cursor still meanwhile and stays on, and the sequence's end decides: `esc`
/// (or a hop elsewhere) leaves it leading, a move (`2j`) pauses it.
#[test]
fn a_key_sequence_under_way_holds_the_cursor_and_its_end_decides() {
    for prefix in ["g", "2", "<space>", "<C-w>"] {
        let (mut ed, _dir, g) = scene("writing-sequence");
        frame(&mut ed, 100, 30);
        press(&mut ed, prefix);
        let input = kept_lines_input();
        let upto = |t: &str| input.rfind(t).unwrap() + t.len();
        let mut w = Writer::new("t1", Tool::Edit);
        w.send(&mut ed, g, &input[..upto(r"let a = 1;\n")], &[], false);
        settle(&mut ed, |ed| shown(ed.doc()).is_some());
        w.send(&mut ed, g, "", &[], false);
        assert!(!ed.session.chat.as_ref().unwrap().follow.paused, "{prefix}: not paused");
        assert!(!ed.toasts.iter().any(|t| t.text.contains("you took over")), "{prefix}: nothing said");
        assert_eq!((led(&ed), ed.cursor_line()), (None, 0), "{prefix}: the view held while it's under way");
        // Its end without a move: follow leads on, the next piece takes the view to the line rewritten
        press(&mut ed, "<esc>");
        w.send(&mut ed, g, &input[upto(r"let a = 1;\n")..upto(r"let b = 2;\n")], &[], false);
        assert!(!ed.session.chat.as_ref().unwrap().follow.paused, "{prefix}: still on");
        assert_eq!(
            (led(&ed), ed.cursor_line()),
            (Some(3), 0),
            "{prefix}: led again, your cursor where it was"
        );
    }
    // A sequence that ends in a move: you took over
    let (mut ed, _dir, _g) = scene("writing-sequence-move");
    frame(&mut ed, 100, 30);
    let read = json!({"file_path": _dir.join("a.rs"), "offset": 1, "limit": 1});
    crate::chat::stream_event(
        &mut ed,
        _g,
        StreamEv::Tool { id: "r1".into(), name: "Read".into(), input: read },
    );
    press(&mut ed, "2");
    assert!(!ed.session.chat.as_ref().unwrap().follow.paused, "a count alone: not yet");
    press(&mut ed, "j");
    assert_eq!(ed.cursor_line(), 2);
    assert!(ed.session.chat.as_ref().unwrap().follow.paused, "2j: paused");
}

/// A motion that leaves a selection, as Helix's do (`w`, a search `/let` and its `enter`), pauses follow like any
/// move — and follow comes back after it as after any key: on its own once you've been quiet `llm.follow-resume`,
/// or with `C-f`; leading again, it takes the view where Claude is, your selection as you left it.
#[test]
fn follow_comes_back_after_a_word_motion_or_a_search() {
    for keys in ["w", "/let<ret>", "/let<ret>n"] {
        for by in ["quiet", "C-f"] {
            let (mut ed, dir, g) = scene("writing-resume");
            // (tests: a second of `llm.follow-resume` is 20 ms)
            ed.config.llm.follow_resume = 1;
            frame(&mut ed, 100, 30);
            let read = json!({"file_path": dir.join("a.rs"), "offset": 3, "limit": 1});
            crate::chat::stream_event(
                &mut ed,
                g,
                StreamEv::Tool { id: "r1".into(), name: "Read".into(), input: read },
            );
            assert_eq!((led(&ed), ed.cursor_line()), (Some(2), 0));
            press(&mut ed, keys);
            let r = ed.doc().selection().primary();
            assert!(r.to() - r.from() > 1, "{keys}: a selection wider than a character");
            let mine = (r.from(), r.to());
            assert!(ed.session.chat.as_ref().unwrap().follow.paused, "{keys}: you took over");
            match by {
                "quiet" => settle(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().follow.paused),
                _ => crate::follow::toggle(&mut ed),
            }
            assert!(crate::follow::leads(&ed), "{keys}, {by}: leading again");
            let r = ed.doc().selection().primary();
            assert_eq!(
                (led(&ed), (r.from(), r.to())),
                (Some(2), mine),
                "{keys}, {by}: the view back on Claude's line, your selection yours"
            );
        }
    }
}

/// SELECT holds follow: entered with `v` it pauses, and follow waits — no piece moves the cursor, and the quiet while
/// passes without taking over (the selection being extended is yours); `esc` back to normal mode and the quiet while
/// after it, follow leads again, the view on Claude's line, your cursor where you left it.
#[test]
fn select_holds_follow_until_you_leave_it() {
    let (mut ed, dir, g) = scene("writing-select");
    // (tests: a second of `llm.follow-resume` is 20 ms)
    ed.config.llm.follow_resume = 1;
    frame(&mut ed, 100, 30);
    let read = json!({"file_path": dir.join("a.rs"), "offset": 3, "limit": 1});
    crate::chat::stream_event(
        &mut ed,
        g,
        StreamEv::Tool { id: "r1".into(), name: "Read".into(), input: read },
    );
    press(&mut ed, "vl");
    assert!(ed.session.chat.as_ref().unwrap().follow.paused, "SELECT: you took over");
    // The quiet while's wake comes and goes (every event until none for a while): follow still waits
    while let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(300)) {
        ed.handle_event(ev);
    }
    assert!(ed.session.chat.as_ref().unwrap().follow.paused, "SELECT holds it past the quiet while");
    let read = json!({"file_path": dir.join("a.rs"), "offset": 5, "limit": 1});
    crate::chat::stream_event(
        &mut ed,
        g,
        StreamEv::Tool { id: "r2".into(), name: "Read".into(), input: read },
    );
    assert_eq!(ed.cursor_line(), 0, "no stop moves it");
    let r = ed.doc().selection().primary();
    assert!(r.to() - r.from() > 1, "your selection stays");
    // Out of SELECT: the quiet while after it, follow leads again
    press(&mut ed, "<esc>");
    settle(&mut ed, |ed| !ed.session.chat.as_ref().unwrap().follow.paused);
    assert!(crate::follow::leads(&ed));
    assert_eq!((led(&ed), ed.cursor_line()), (Some(4), 0), "the view on Claude's line, its last stop");
}

/// INSERT or SELECT entered with a key of yours is you taking over, though the cursor stays: follow pauses, and a
/// piece then doesn't move the cursor.
#[test]
fn a_key_left_under_way_pauses_follow() {
    for keys in ["i", "v"] {
        let (mut ed, _dir, g) = scene("writing-midway");
        frame(&mut ed, 100, 30);
        press(&mut ed, keys);
        assert!(ed.session.chat.as_ref().unwrap().follow.paused, "{keys}: paused");
        let input = kept_lines_input();
        Writer::new("t1", Tool::Edit).send(&mut ed, g, &input[..input.len() - 20], &[], false);
        settle(&mut ed, |ed| shown(ed.doc()).is_some());
        assert_eq!(ed.cursor_line(), 0, "{keys}: the cursor where you left it");
    }
}

/// A key of yours that edits where the cursor is — `d` at a point, `r`, `~`, `p` — is you acting in the code:
/// follow pauses as for a move, said the same way, and leads no more.
#[test]
fn an_edit_of_yours_where_the_cursor_is_pauses_follow() {
    for keys in ["d", "rx", "~", "yp"] {
        let (mut ed, dir, g) = scene("writing-edited");
        frame(&mut ed, 100, 30);
        // Follow puts the cursor on line 1's `f` (a read there)
        let read = json!({"file_path": dir.join("a.rs"), "offset": 1, "limit": 1});
        crate::chat::stream_event(
            &mut ed,
            g,
            StreamEv::Tool { id: "r1".into(), name: "Read".into(), input: read },
        );
        assert_eq!(ed.cursor_line(), 0);
        assert!(crate::follow::leads(&ed), "{keys}: led");
        let before = ed.doc().text.to_string();
        press(&mut ed, keys);
        assert_ne!(ed.doc().text.to_string(), before, "{keys}: an edit");
        let f = &ed.session.chat.as_ref().unwrap().follow;
        assert!(f.paused, "{keys}: paused");
        assert!(ed.toasts.iter().any(|t| t.text.contains("you took over")), "{keys}: said");
        assert!(status(&frame(&mut ed, 100, 30)).contains("● normal"), "{keys}");
        // (Claude reads elsewhere: the editor stays with you)
        let read = json!({"file_path": dir.join("a.rs"), "offset": 4, "limit": 1});
        crate::chat::stream_event(
            &mut ed,
            g,
            StreamEv::Tool { id: "r2".into(), name: "Read".into(), input: read },
        );
        assert_eq!(ed.cursor_line(), 0, "{keys}: stays with you");
    }
}

/// The cell where `needle` starts on a frame drawn by a terminal — and its foreground.
fn fg_before(ed: &mut Editor, needle: &str) -> (vt100::Color, String) {
    crossterm::style::force_color_output(true);
    let mut bytes = Vec::new();
    crate::term::render(ed, &mut bytes, 100, 30).unwrap();
    let mut parser = vt100::Parser::new(30, 100, 0);
    parser.process(&bytes);
    let screen = parser.screen();
    let (y, row) = screen.rows(0, 100).enumerate().find(|(_, r)| r.contains(needle)).expect(needle);
    let x = row[..row.find(needle).unwrap()].chars().count();
    let cell = screen.cell(y as u16, x.saturating_sub(1) as u16).unwrap();
    (cell.fgcolor(), cell.contents().to_string())
}

/// The front session's Claude's colour: its participant colour (`ui.participant.1` for the session tarae started in
/// — collab C1), at full strength while you follow it.
fn claude_colour(ed: &Editor) -> vt100::Color {
    let k = crate::session::PARTICIPANTS[ed.session.colour as usize];
    match ed.theme.try_get(k).and_then(|s| s.fg) {
        Some(crossterm::style::Color::Rgb { r, g, b }) => vt100::Color::Rgb(r, g, b),
        c => panic!("{k}: {c:?}"),
    }
}

/// (c) The keys in the chat: the code has no cursor, so Claude's place is a mark — its caret at the end of the row
/// being written (that row's bar the written rows' own); a stop with nothing written
/// (a read) has the accent bar on the line follow keeps in sight; the status line says `chat · follow`. The keys back
/// in the code: your cursor where it was, the mark still Claude's place.
#[test]
fn with_the_keys_in_the_chat_claudes_place_is_a_mark() {
    let (mut ed, dir, g) = scene("writing-mark");
    ed.session.at = crate::session::At::Chat;
    frame(&mut ed, 100, 30);
    let input = edit_input("    let b = 20;\n    let c = a");
    let mut w = Writer::new("t1", Tool::Edit);
    w.send(&mut ed, g, &input[..input.len() - 4], &[], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    w.send(&mut ed, g, "", &[], false);
    // The row being written: its bar as the other rows written have it (the caret marks Claude's place)
    let (head, bar) = fg_before(&mut ed, "    let c = a");
    let (written, _) = fg_before(&mut ed, "    let b = 20;");
    assert_eq!((head, bar.as_str()), (written, "▎"), "the head's bar: the written rows' own");
    // Claude's caret right after what's written — the code's only cursor-like thing; the terminal's is the chat's
    let a = after(&mut ed, "    let c = a");
    assert!(a.is_caret(claude_colour(&ed)), "the caret, unfaded in the pane without the keys");
    assert_ne!(a.cursor, Some(a.at), "not the terminal's cursor");
    assert_eq!(
        (led(&ed), ed.cursor_line()),
        (Some(2), 0),
        "the view on the line it rewrites, your cursor never led"
    );
    assert_ne!(written, claude_colour(&ed), "the rows written: their own bar");
    assert!(status(&frame(&mut ed, 100, 30)).contains("● chat · follow"));
    // Nothing written any more, a read: the accent bar on its line
    crate::chat::stream_event(&mut ed, g, StreamEv::Done(Ok(String::new())));
    ed.session.chat.as_mut().unwrap().state = State::Thinking;
    let read = json!({"file_path": dir.join("a.rs"), "offset": 4, "limit": 1});
    crate::chat::stream_event(
        &mut ed,
        g,
        StreamEv::Tool { id: "r1".into(), name: "Read".into(), input: read },
    );
    assert_eq!((led(&ed), ed.cursor_line()), (Some(3), 0));
    let (mark, bar) = fg_before(&mut ed, r#"    println!("{a} {b}");"#);
    assert_eq!((mark, bar.as_str()), (claude_colour(&ed), "▎"), "Claude's place marked");
    let (other, bar) = fg_before(&mut ed, "    let a = 1;");
    assert!(other != claude_colour(&ed) || bar != "▎", "one line marked");
    // The keys back in the code: your cursor where it was, Claude's place still marked (your cursor isn't on it)
    ed.session.leave(crate::session::At::Chat);
    assert_eq!((led(&ed), ed.cursor_line()), (Some(3), 0));
    let (mark, bar) = fg_before(&mut ed, r#"    println!("{a} {b}");"#);
    assert_eq!((mark, bar.as_str()), (claude_colour(&ed), "▎"), "still Claude's place");
    assert!(status(&frame(&mut ed, 100, 30)).contains("● follow"));
}

/// One edit of two changes far apart in an 80-line file, written until the second is drawn while its jump waits: the
/// first jumped to, the second out of sight within PACE — the view held there, your cursor never moved. The editor,
/// its folder, the generation, the call's reader and the view's top then.
fn a_jump_waiting(name: &str) -> (Editor, crate::scratch::Scratch, u64, Writer, usize) {
    let (mut ed, dir, g) = scene(name);
    let text: String = (1..=80).map(|i| format!("    let x{i} = {i};\n")).collect();
    std::fs::write(dir.join("a.rs"), &text).unwrap();
    crate::typed::execute(&mut ed, "reload").unwrap();
    frame(&mut ed, 100, 30);
    let s = |t: &str| serde_json::to_string(t).unwrap();
    let input = format!(
        r#"{{"edits":[{{"file_path":"a.rs","old_string":{},"new_string":{}}},{{"file_path":"a.rs","old_string":{},"new_string":{}}}]}}"#,
        s("    let x60 = 60;\n"),
        s("    let x60 = 600;\n"),
        s("    let x5 = 5;\n"),
        s("    let x5 = 50;\n")
    );
    let mut w = Writer::new("t1", Tool::Edit);
    let first = input.find("600").unwrap();
    w.send(&mut ed, g, &input[..first], &[], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    w.send(&mut ed, g, "", &[], false);
    assert_eq!((led(&ed), ed.cursor_line()), (Some(59), 0), "the first change: the view jumped there");
    frame(&mut ed, 100, 30);
    let top = ed.doc().top;
    // The second, far above, within PACE: the view holds
    let second = input.find("50;").unwrap();
    w.send(&mut ed, g, &input[first..second], &[], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some_and(|s| s.blocks.len() == 2));
    w.send(&mut ed, g, "", &[], false);
    frame(&mut ed, 100, 30);
    assert_eq!((led(&ed), ed.doc().top, ed.cursor_line()), (Some(59), top, 0), "within PACE: held");
    (ed, dir, g, w, top)
}

/// A second change elsewhere out of sight within PACE of the last jump: the view holds (it moves at most that often)
/// — then the next piece once PACE has gone by takes it there; your cursor never moves.
#[test]
fn a_jump_to_another_change_waits_for_pace() {
    let (mut ed, _dir, g, mut w, _) = a_jump_waiting("writing-jumps");
    // Still within PACE: the next piece holds too
    w.send(&mut ed, g, "", &[], false);
    assert_eq!(led(&ed), Some(59));
    // PACE gone by: the next piece jumps
    crate::clock::advance(crate::follow::PACE);
    w.send(&mut ed, g, "", &[], false);
    assert_eq!((led(&ed), ed.cursor_line()), (Some(4), 0), "there now, on the line it rewrites");
}

/// Every key of yours that only moves the view, and the wheel, while follow holds it far from your cursor (Claude on
/// line 60, you on line 1): follow pauses, said, and the key acts from your cursor — the view back there first, never
/// from Claude's place. Each with how far from the top it may leave the view and your cursor (a page at most).
#[test]
fn a_view_key_of_yours_takes_the_view_back_to_your_cursor() {
    let keys = [
        ("zz", 2),
        ("zt", 2),
        ("zb", 2),
        // (the code's line scrolls — `C-e`/`C-y` are the run panels' and the chat's, not the code's)
        // (a row down: the cursor kept `scrolloff` below the view's top)
        ("zj", 6),
        ("zk", 2),
        ("<C-d>", 20),
        ("<C-u>", 2),
        ("<C-f>", 30),
        ("<C-b>", 2),
        ("<pagedown>", 30),
        ("<pageup>", 2),
        ("wheel", 8),
    ];
    for (k, near) in keys {
        let (mut ed, _dir, _g, _w, top) = a_jump_waiting("writing-view-key");
        assert!(top > 20, "follow holds the view far from your cursor: top {top}");
        match k {
            // (over the code, as the frame laid it out)
            "wheel" => {
                let m = crate::event::Mouse {
                    kind: crate::event::MouseKind::ScrollDown,
                    x: 10,
                    y: 5,
                    alt: false,
                };
                ed.handle_event(crate::event::Event::Mouse(m));
            }
            k => press(&mut ed, k),
        }
        frame(&mut ed, 100, 30);
        let f = &ed.session.chat.as_ref().unwrap().follow;
        assert!(f.paused, "{k}: you took over");
        assert!(ed.toasts.iter().any(|t| t.text.contains("you took over")), "{k}: said");
        assert_eq!(led(&ed), None, "{k}: the view yours");
        assert!(ed.doc().top <= near, "{k}: from your cursor, not from Claude's place: top {}", ed.doc().top);
        assert!(ed.cursor_line() <= near, "{k}: from your cursor: {}", ed.cursor_line());
    }
}

/// The stream ends with a jump still waiting (its call answered inside PACE): it's never taken — not late, once PACE
/// has gone by, to a change that's gone — and it doesn't hold the view back: a later edit in sight gets it at once.
#[test]
fn a_jump_left_waiting_is_never_taken_late() {
    let (mut ed, _dir, g, _w, top) = a_jump_waiting("writing-jump-left");
    crate::writing::answered(&mut ed, "t1");
    crate::clock::advance(crate::follow::PACE);
    let s = |t: &str| serde_json::to_string(t).unwrap();
    let near = format!(
        r#"{{"edits":[{{"file_path":"a.rs","old_string":{},"new_string":{}}}]}}"#,
        s("    let x62 = 62;\n"),
        s("    let x62 = 620;\n")
    );
    let mut w = Writer::new("t2", Tool::Edit);
    w.send(&mut ed, g, &near[..near.find("620").unwrap()], &[], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some_and(|s| s.blocks.iter().any(|b| b.first == 61)));
    w.send(&mut ed, g, "", &[], false);
    frame(&mut ed, 100, 30);
    assert_eq!(
        (led(&ed), ed.doc().top, ed.cursor_line()),
        (Some(61), top, 0),
        "the later edit in sight: the view there, no late jump"
    );
}

/// A landed edit of Claude's leaves your selection as it is — follow moves the view, never your cursor (here a file
/// outside the project: not even the view).
#[test]
fn a_landed_edit_follow_wont_follow_leaves_your_selection() {
    let (mut ed, _dir, _g) = scene("writing-landed-outside");
    let elsewhere = crate::scratch::Scratch::new("writing-landed-elsewhere");
    let path = elsewhere.join("b.rs");
    std::fs::write(&path, TEXT).unwrap();
    ed.open(&path).unwrap();
    let wide = crate::selection::Range { anchor: 0, head: 8 };
    ed.doc_mut().set_selection(crate::selection::Selection::single(wide));
    let before = ropey::Rope::from_str(TEXT);
    let after = ropey::Rope::from_str(&TEXT.replace("let b = 2", "let b = 3"));
    crate::follow::wrote(&mut ed, None, &path, &before, &after);
    let r = ed.doc().selection().primary();
    assert_eq!((r.from(), r.to()), (0, 8), "your selection stays");
}

/// An edit that only adds lines: the view on the line its new rows stand on — where its first line is once it
/// lands, so landing doesn't move it; your cursor never led.
#[test]
fn an_added_lines_cursor_stays_put_when_it_lands() {
    let (mut ed, dir, g) = scene("writing-adds");
    ed.session.chat.as_mut().unwrap().edits.at_end = Some(dir.to_path_buf());
    frame(&mut ed, 100, 30);
    let s = |t: &str| serde_json::to_string(t).unwrap();
    let (old, new) = ("    let b = 2;\n", "    let b = 2;\n    let c = 3;\n");
    let input =
        format!(r#"{{"edits":[{{"file_path":"a.rs","old_string":{},"new_string":{}}}]}}"#, s(old), s(new));
    let mut w = Writer::new("t1", Tool::Edit);
    w.send(&mut ed, g, &input[..input.find("= 3").unwrap()], &[], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    w.send(&mut ed, g, "", &[], false);
    assert_eq!((led(&ed), ed.cursor_line()), (Some(3), 0), "on the line its new row stands on");
    w.send(&mut ed, g, &input[input.find("= 3").unwrap()..], &[], true);
    let input: Value = serde_json::from_str(&input).unwrap();
    crate::chat::stream_event(
        &mut ed,
        g,
        StreamEv::Tool { id: "t1".into(), name: "mcp__tarae__edit".into(), input: input.clone() },
    );
    let request = json!({"subtype": "mcp_message", "server_name": "tarae", "message": {"jsonrpc": "2.0",
        "id": 2, "method": "tools/call", "params": {"name": "edit", "arguments": input}}});
    crate::chat::stream_event(&mut ed, g, StreamEv::Control { request_id: "c1".into(), request });
    settle(&mut ed, |ed| ed.doc().text.to_string().contains("let c = 3;"));
    assert_eq!(ed.doc().text.line(3).to_string(), "    let c = 3;\n");
    assert_eq!((led(&ed), ed.cursor_line()), (Some(3), 0), "landed: on its first line, where it was");
}

/// An edit landing unseen (review at the end): the view on its first changed line — your cursor where it was.
#[test]
fn an_edit_landing_unseen_leads_the_view_there() {
    let (mut ed, dir, g) = scene("writing-unseen");
    let mut text: String = (1..=60).map(|i| format!("    let x{i} = {i};\n")).collect();
    text.insert_str(text.find("    let x40").unwrap(), TEXT);
    std::fs::write(dir.join("a.rs"), &text).unwrap();
    crate::typed::execute(&mut ed, "reload").unwrap();
    ed.session.chat.as_mut().unwrap().edits.at_end = Some(dir.to_path_buf());
    frame(&mut ed, 100, 30);
    let input: Value = serde_json::from_str(&edit_input("    let b = 20;\n")).unwrap();
    ed.doc_mut().set_selection(crate::selection::Selection::point(0));
    crate::chat::stream_event(
        &mut ed,
        g,
        StreamEv::Tool { id: "t1".into(), name: "mcp__tarae__edit".into(), input: input.clone() },
    );
    let request = json!({"subtype": "mcp_message", "server_name": "tarae", "message": {"jsonrpc": "2.0",
        "id": 2, "method": "tools/call", "params": {"name": "edit", "arguments": input}}});
    crate::chat::stream_event(&mut ed, g, StreamEv::Control { request_id: "c1".into(), request });
    settle(&mut ed, |ed| ed.doc().text.to_string().contains("let b = 20;"));
    assert!(ed.session.review.is_none(), "unseen");
    let changed = ed.doc().text.to_string().lines().position(|l| l == "    let b = 20;").unwrap();
    assert_eq!((led(&ed), ed.cursor_line()), (Some(changed), 0), "the view on its first changed line");
    assert!(status(&frame(&mut ed, 100, 30)).contains("● follow"));
}

/// (b) An edit landing through its review and `y`: the view on its first changed line, still led.
#[test]
fn an_edit_accepted_in_its_review_leads_the_cursor_there() {
    let (mut ed, dir, g) = scene("writing-accepted");
    let mut text: String = (1..=60).map(|i| format!("    let x{i} = {i};\n")).collect();
    text.insert_str(text.find("    let x40").unwrap(), TEXT);
    std::fs::write(dir.join("a.rs"), &text).unwrap();
    crate::typed::execute(&mut ed, "reload").unwrap();
    frame(&mut ed, 100, 30);
    let input = edit_input("    let b = 20;\n");
    write(&mut ed, g, "t1", Tool::Edit, &input, &[30, 70]);
    let input: Value = serde_json::from_str(&input).unwrap();
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    crate::chat::stream_event(
        &mut ed,
        g,
        StreamEv::Tool { id: "t1".into(), name: "mcp__tarae__edit".into(), input: input.clone() },
    );
    let request = json!({"subtype": "mcp_message", "server_name": "tarae", "message": {"jsonrpc": "2.0",
        "id": 2, "method": "tools/call", "params": {"name": "edit", "arguments": input}}});
    crate::chat::stream_event(&mut ed, g, StreamEv::Control { request_id: "c1".into(), request });
    settle(&mut ed, |ed| ed.session.review.is_some());
    frame(&mut ed, 100, 30);
    // (the cursor somewhere else meanwhile, as a reload or your own look may leave it)
    ed.doc_mut().set_selection(crate::selection::Selection::point(0));
    crate::follow::settle(&mut ed);
    press(&mut ed, "y");
    settle(&mut ed, |ed| ed.session.review.is_none());
    let changed = ed.doc().text.to_string().lines().position(|l| l == "    let b = 20;").unwrap();
    assert_eq!(led(&ed), Some(changed), "the view on its first changed line");
    assert!(!ed.session.chat.as_ref().unwrap().follow.paused, "your `y` answered the review: still led");
    assert!(status(&frame(&mut ed, 100, 30)).contains("● follow"));
}

/// tarae's edit made as claude makes it, its input whole: the call, then tarae's tool call (`rpc` its id).
fn call_edit(ed: &mut Editor, g: u64, id: &str, rpc: u64, input: &str) {
    let input: Value = serde_json::from_str(input).unwrap();
    let tool = StreamEv::Tool { id: id.into(), name: "mcp__tarae__edit".into(), input: input.clone() };
    crate::chat::stream_event(ed, g, tool);
    let request = json!({"subtype": "mcp_message", "server_name": "tarae", "message": {"jsonrpc": "2.0",
        "id": rpc, "method": "tools/call", "params": {"name": "edit", "arguments": input}}});
    crate::chat::stream_event(ed, g, StreamEv::Control { request_id: format!("c{rpc}"), request });
}

/// tarae's edit of `file` replacing `old` with `new`.
fn edit_of(file: &str, old: &str, new: &str) -> String {
    let s = |t: &str| serde_json::to_string(t).unwrap();
    format!(r#"{{"edits":[{{"file_path":{},"old_string":{},"new_string":{}}}]}}"#, s(file), s(old), s(new))
}

/// An 80-line `a.rs` (`x1`…`x80`) whose edits land unseen (review at the end).
fn tall_scene(name: &str) -> (Editor, crate::scratch::Scratch, u64) {
    let (mut ed, dir, g) = scene(name);
    let text: String = (1..=80).map(|i| format!("    let x{i} = {i};\n")).collect();
    std::fs::write(dir.join("a.rs"), &text).unwrap();
    crate::typed::execute(&mut ed, "reload").unwrap();
    ed.session.chat.as_mut().unwrap().edits.at_end = Some(dir.to_path_buf());
    frame(&mut ed, 100, 30);
    (ed, dir, g)
}

/// tarae's edit `t1` writing 40 lines (`y1`…`y40`) over line 10 of `tall_scene`'s file — `more` items after it —
/// streamed a frame each ten lines to its end, `keys` pressed before its last ten, and whole. Its input.
fn write_tall(ed: &mut Editor, g: u64, keys: &str, more: &str) -> String {
    let new: String = (1..=40).map(|i| format!("    let y{i} = {i};\n")).collect();
    let input = edit_of("a.rs", "    let x10 = 10;\n", &new).replace("}]}", &format!("}}{more}]}}"));
    let mut w = Writer::new("t1", Tool::Edit);
    let upto = |n: usize| input.find(&format!("y{n} = ")).unwrap();
    w.send(ed, g, &input[..upto(1)], &[], false);
    settle(ed, |ed| shown(ed.doc()).is_some());
    let mut at = upto(1);
    for n in [10, 20, 30, 40] {
        if n == 40 {
            press(ed, keys);
        }
        w.send(ed, g, &input[at..upto(n)], &[], false);
        at = upto(n);
        frame(ed, 100, 30);
    }
    w.send(ed, g, &input[at..], &[], true);
    input
}

/// `tall_scene` with `write_tall`'s edit streamed in sight to its end — whole, not made yet: the view went down with
/// its writing and stays there. The editor, its folder, the generation, the edit's input and the screen row its last
/// line is on.
fn written_down(name: &str) -> (Editor, crate::scratch::Scratch, u64, String, usize) {
    let (mut ed, dir, g) = tall_scene(name);
    let input = write_tall(&mut ed, g, "", "");
    // (the follow card may cover a row's end)
    let rows = frame(&mut ed, 100, 30);
    let row = rows.iter().position(|r| r.contains("let y40 ")).expect("its last line in sight");
    assert!(
        !rows.iter().any(|r| r.contains("let x9 ")),
        "whole: the view where the writing ended, not back up to its first line: {rows:#?}"
    );
    (ed, dir, g, input, row)
}

/// A streamed edit taller than the screen lands (unseen, review at the end): the view stays where its writing ended —
/// on the last line it wrote, the screen as it was — never back up to its first changed line; your cursor never led.
#[test]
fn a_streamed_edit_landing_leaves_the_view_where_its_writing_ended() {
    let (mut ed, _dir, g, input, row) = written_down("writing-stays");
    call_edit(&mut ed, g, "t1", 2, &input);
    settle(&mut ed, |ed| ed.doc().text.to_string().contains("let y40 = 40;"));
    assert!(ed.session.review.is_none(), "unseen");
    let last = ed.doc().text.to_string().lines().position(|l| l == "    let y40 = 40;").unwrap();
    assert_eq!((led(&ed), ed.cursor_line()), (Some(last), 0), "on the last line it wrote");
    let rows = frame(&mut ed, 100, 30);
    let now = rows.iter().position(|r| r.contains("let y40 ")).expect("still in sight");
    assert!(now.abs_diff(row) <= 1, "landed: the screen as it was (row {row} → {now}): {rows:#?}");
    assert!(!rows.iter().any(|r| r.contains("let x9 ")), "never back up to its first line: {rows:#?}");
    assert!(status(&rows).contains("● follow"));
}

/// Drawn in the buffer in front but its end written while you had the view (a key of yours paused follow), then
/// follow on again (`C-f`) and a piece of another call before it lands: nobody watched it end, so landing takes the
/// view to its first changed line.
#[test]
fn an_edit_whose_end_you_did_not_watch_goes_to_its_first_line() {
    let (mut ed, _dir, g) = tall_scene("writing-stays-unled");
    let input = write_tall(&mut ed, g, "j", "");
    assert!(ed.session.chat.as_ref().unwrap().follow.paused, "j: you took over");
    crate::follow::toggle(&mut ed);
    assert!(!ed.session.chat.as_ref().unwrap().follow.paused, "C-f: following again");
    // (a call made beside it streams on: what's drawn is laid out again, follow leading now)
    Writer::new("t2", Tool::Edit).send(&mut ed, g, r#"{"edits":[{"file_path":"a.rs""#, &[], false);
    call_edit(&mut ed, g, "t1", 2, &input);
    settle(&mut ed, |ed| ed.doc().text.to_string().contains("let y40 = 40;"));
    assert_eq!(led(&ed), Some(9), "the view on its first changed line");
}

/// `write_tall`'s edit with a delete beside it (`c.rs`), review at the end off: it waits on its change-set card, then
/// `y` — the editor, the generation, whether you moved (`G`) while the card waited.
fn on_its_card(name: &str, keys: &str) -> (Editor, crate::scratch::Scratch, u64) {
    let (mut ed, dir, g) = tall_scene(name);
    std::fs::write(dir.join("c.rs"), "gone soon\n").unwrap();
    ed.session.chat.as_mut().unwrap().edits.at_end = None;
    let input = write_tall(&mut ed, g, "", r#",{"file_path":"c.rs","delete":true}"#);
    call_edit(&mut ed, g, "t1", 2, &input);
    let card = |ed: &Editor| {
        ed.offers.iter().find_map(|o| match o.what {
            crate::offer::What::ChangeSet { whose, .. } => Some((o.id, whose)),
            _ => None,
        })
    };
    settle(&mut ed, |ed| card(ed).is_some());
    frame(&mut ed, 100, 30);
    press(&mut ed, keys);
    let (id, whose) = card(&ed).unwrap();
    crate::refactor::accept_all(&mut ed, id, whose);
    settle(&mut ed, |ed| !dir.join("c.rs").exists() && ed.doc().text.to_string().contains("let y40 = 40;"));
    (ed, dir, g)
}

/// A streamed edit that waited on its card, `y` given, follow leading there still: the view stays where its writing
/// ended.
#[test]
fn an_edit_from_its_card_stays_where_its_writing_ended() {
    let (mut ed, _dir, _g) = on_its_card("writing-stays-card", "");
    let last = ed.doc().text.to_string().lines().position(|l| l == "    let y40 = 40;").unwrap();
    assert_eq!((led(&ed), ed.cursor_line()), (Some(last), 0), "on the last line it wrote");
    let rows = frame(&mut ed, 100, 30);
    assert!(!rows.iter().any(|r| r.contains("let x9 ")), "not back up to its first line: {rows:#?}");
}

/// … you moved while its card waited (`G`): follow paused, your view stays yours — the landing takes it nowhere.
#[test]
fn an_edit_from_its_card_leaves_your_view_yours_once_you_moved() {
    let (mut ed, _dir, _g) = on_its_card("writing-stays-card-moved", "G");
    assert!(ed.session.chat.as_ref().unwrap().follow.paused, "G: you took over");
    let end = crate::movement::last_line(&ed.doc().text);
    assert_eq!((led(&ed), ed.cursor_line()), (None, end), "your cursor, your view");
    let rows = frame(&mut ed, 100, 30);
    assert!(rows.iter().any(|r| r.contains("let x80 ")), "where you went: {rows:#?}");
}

/// A key sequence of yours under way (`g`) as the edit lands: follow holds the view, and it's where the writing
/// ended — not taken back up to the change's first line; your cursor where it was, follow still on.
#[test]
fn a_key_sequence_under_way_holds_the_view_where_the_writing_ended() {
    let (mut ed, _dir, g, input, _) = written_down("writing-stays-pending");
    press(&mut ed, "g");
    call_edit(&mut ed, g, "t1", 2, &input);
    settle(&mut ed, |ed| ed.doc().text.to_string().contains("let y40 = 40;"));
    assert!(!ed.session.chat.as_ref().unwrap().follow.paused, "g: not paused");
    let last = ed.doc().text.to_string().lines().position(|l| l == "    let y40 = 40;").unwrap();
    assert_eq!((led(&ed), ed.cursor_line()), (Some(last), 0), "held where it was");
    let rows = frame(&mut ed, 100, 30);
    assert!(!rows.iter().any(|r| r.contains("let x9 ")), "not back up to its first line: {rows:#?}");
}

/// A whole block taller than the screen, follow off (your cursor's view is yours): your cursor on the line it rewrites
/// keeps the block's first rows in sight — never pinned to its last row as follow's view is.
#[test]
fn a_whole_block_follow_does_not_lead_leaves_your_cursor_in_sight() {
    let (mut ed, _dir, g) = tall_scene("writing-tall-unled");
    ed.session.chat.as_mut().unwrap().follow.on = false;
    let at = ed.doc().text.line_to_byte(9);
    ed.doc_mut().set_selection(crate::selection::Selection::point(at));
    frame(&mut ed, 100, 30);
    let new: String = (1..=40).map(|i| format!("    let y{i} = {i};\n")).collect();
    write(&mut ed, g, "t1", Tool::Edit, &edit_of("a.rs", "    let x10 = 10;\n", &new), &[]);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    let rows = frame(&mut ed, 100, 30);
    assert!(rows.iter().any(|r| r.contains("let x10 ")), "your cursor's line in sight: {rows:#?}");
    assert!(rows.iter().any(|r| r.contains("let y1 ")), "the block's first rows: {rows:#?}");
    assert_eq!(ed.cursor_line(), 9);
}

/// One edit of three items, the last one written above the one before it (and below a first that adds lines): the
/// view stays where its writing ended — the last line the last item wrote, moved by the lines the first added — not at
/// the edit's lowest change.
#[test]
fn a_many_item_edit_stays_at_its_last_written_items_end() {
    let (mut ed, _dir, g) = tall_scene("writing-stays-items");
    let s = |t: &str| serde_json::to_string(t).unwrap();
    let item = |old: &str, new: &str| {
        format!(r#"{{"file_path":"a.rs","old_string":{},"new_string":{}}}"#, s(old), s(new))
    };
    let input = format!(
        r#"{{"edits":[{},{},{}]}}"#,
        item("    let x5 = 5;\n", "    let x5 = 50;\n    let x5b = 51;\n    let x5c = 52;\n"),
        item("    let x70 = 70;\n", "    let x70 = 700;\n"),
        item("    let x40 = 40;\n", "    let z1 = 1;\n    let z2 = 2;\n    let z3 = 3;\n")
    );
    let cuts: Vec<usize> = input.match_indices(r#"{"file_path""#).map(|(i, _)| i).collect();
    let mut w = Writer::new("t1", Tool::Edit);
    let mut at = 0;
    for (n, &cut) in cuts[1..].iter().chain([&input.len()]).enumerate() {
        w.send(&mut ed, g, &input[at..cut], &[], cut == input.len());
        at = cut;
        settle(&mut ed, |ed| shown(ed.doc()).is_some_and(|s| s.blocks.len() == n + 1));
        frame(&mut ed, 100, 30);
        // (each jump to the next item's place taken at once)
        crate::clock::advance(crate::follow::PACE);
    }
    call_edit(&mut ed, g, "t1", 2, &input);
    settle(&mut ed, |ed| ed.doc().text.to_string().contains("let z3 = 3;"));
    let lines: Vec<String> = ed.doc().text.to_string().lines().map(String::from).collect();
    let z3 = lines.iter().position(|l| l == "    let z3 = 3;").unwrap();
    assert_eq!(z3, 43, "x40's line (39) moved by the two x5 added, its own last");
    assert_eq!(led(&ed), Some(z3), "the last line the last item wrote");
}

/// Claude's next edit after it, elsewhere in the file (three lines, not streamed: nothing showed it) — the one before
/// still listed, its result not come yet: the view goes there, on its first changed line.
#[test]
fn the_next_edit_elsewhere_takes_the_view_there() {
    let (mut ed, _dir, g, input, _) = written_down("writing-stays-next");
    call_edit(&mut ed, g, "t1", 2, &input);
    settle(&mut ed, |ed| ed.doc().text.to_string().contains("let y40 = 40;"));
    let three = "    let x70 = 700;\n    let x71 = 710;\n    let x72 = 720;\n";
    call_edit(&mut ed, g, "t2", 3, &edit_of("a.rs", "    let x70 = 70;\n", three));
    settle(&mut ed, |ed| ed.doc().text.to_string().contains("let x72 = 720;"));
    let changed = ed.doc().text.to_string().lines().position(|l| l == "    let x70 = 700;").unwrap();
    assert_eq!((led(&ed), ed.cursor_line()), (Some(changed), 0), "the view on its first changed line");
    let rows = frame(&mut ed, 100, 30);
    assert!(rows.iter().any(|r| r.contains("let x70 ")), "in sight: {rows:#?}");
}

/// An edit landing in a file not in front (nothing showed it as it was written): the view there, on its first
/// changed line, so it's seen at all.
#[test]
fn an_edit_landing_in_another_file_takes_the_view_to_its_first_line() {
    let (mut ed, dir, g) = scene("writing-elsewhere");
    let text: String = (1..=80).map(|i| format!("    let x{i} = {i};\n")).collect();
    std::fs::write(dir.join("b.rs"), &text).unwrap();
    ed.session.chat.as_mut().unwrap().edits.at_end = Some(dir.to_path_buf());
    frame(&mut ed, 100, 30);
    call_edit(&mut ed, g, "t1", 2, &edit_of("b.rs", "    let x50 = 50;\n", "    let x50 = 500;\n"));
    settle(&mut ed, |ed| ed.doc().text.to_string().contains("let x50 = 500;"));
    assert_eq!(
        (led(&ed), ed.cursor_line()),
        (Some(49), 0),
        "b.rs in front, the view on its first changed line"
    );
    let rows = frame(&mut ed, 100, 30);
    assert!(rows.iter().any(|r| r.contains("let x50 ")), "in sight: {rows:#?}");
}

/// Performance budget: a stream's batch while an edit is written into a 200k-line file — its pieces read, its rows
/// laid out again, the frame drawn — within one frame (16 ms, 60 Hz; `TARAE_PERF_SLACK` loosens it as the other
/// budgets'); the worst batch, each its best of 3 rounds.
#[test]
fn perf_budget_stream_frame_with_live_rows() {
    if crate::test_results::unoptimized("writing::tests::perf_budget_stream_frame_with_live_rows") {
        return;
    }
    let slack = std::env::var("TARAE_PERF_SLACK").ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
    let budget = std::time::Duration::from_millis(16) * slack.max(1);
    let text: String =
        (0..200_000).map(|i| format!("line {i} 타래 fn main() {{ let x = {i}; }}\n")).collect();
    let old: String =
        (100_000..100_005).map(|i| format!("line {i} 타래 fn main() {{ let x = {i}; }}\n")).collect();
    let new: String = (0..40).map(|i| format!("    let written_{i} = compute({i}, \"타래\");\n")).collect();
    let s = |t: &str| serde_json::to_string(t).unwrap();
    let input =
        format!(r#"{{"edits":[{{"file_path":"a.rs","old_string":{},"new_string":{}}}]}}"#, s(&old), s(&new));
    // (a 25 ms batch at ~150 tokens a second: about 16 characters)
    let chars: Vec<char> = input.chars().collect();
    let head = input.find("new_string").unwrap() + 14;
    let cuts: Vec<usize> = (head..chars.len()).step_by(16).collect();
    let mut best = vec![std::time::Duration::MAX; cuts.len()];
    for _ in 0..3 {
        let (mut ed, dir, g) = scene("writing-perf");
        std::fs::write(dir.join("a.rs"), &text).unwrap();
        crate::typed::execute(&mut ed, "reload").unwrap();
        let mut buf = Vec::with_capacity(1 << 16);
        crate::term::render(&mut ed, &mut buf, 120, 40).unwrap();
        let mut w = Writer::new("t1", Tool::Edit);
        let first: String = chars[..head].iter().collect();
        w.send(&mut ed, g, &first, &[], false);
        settle(&mut ed, |ed| shown(ed.doc()).is_some());
        for (k, pair) in cuts.windows(2).enumerate() {
            let piece: String = chars[pair[0]..pair[1]].iter().collect();
            let t = std::time::Instant::now();
            w.send(&mut ed, g, &piece, &[], false);
            buf.clear();
            crate::term::render(&mut ed, &mut buf, 120, 40).unwrap();
            best[k] = best[k].min(t.elapsed());
        }
        assert!(shown(ed.doc()).is_some_and(|s| s.blocks[0].rows.len() > 40), "its rows drawn");
    }
    let worst = best.iter().take(cuts.len() - 1).max().copied().unwrap_or_default();
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    let line = format!("PERF stream_frame_with_live_rows {:.3} budget={:.0}\n", ms(worst), ms(budget));
    let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    assert!(worst < budget, "a batch took {worst:?} (budget {budget:?})");
}

/// Performance budget: a frame while an edit's text is shown at its pace (`reveal`) in a 200k-line file — the reveal
/// moved, the rows laid out again, the frame drawn — within one frame (16 ms; `TARAE_PERF_SLACK` loosens it); the
/// measured stream: a lump of ~190 characters every 640 ms, a frame every `REVEAL_EVERY`; the worst frame, each its
/// best of 3 rounds.
#[test]
fn perf_budget_reveal_frame_with_live_rows() {
    if crate::test_results::unoptimized("writing::tests::perf_budget_reveal_frame_with_live_rows") {
        return;
    }
    let slack = std::env::var("TARAE_PERF_SLACK").ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
    let budget = Duration::from_millis(16) * slack.max(1);
    let text: String =
        (0..200_000).map(|i| format!("line {i} 타래 fn main() {{ let x = {i}; }}\n")).collect();
    let old: String =
        (100_000..100_005).map(|i| format!("line {i} 타래 fn main() {{ let x = {i}; }}\n")).collect();
    let new: String = (0..40).map(|i| format!("    let written_{i} = compute({i}, \"타래\");\n")).collect();
    let s = |t: &str| serde_json::to_string(t).unwrap();
    let input =
        format!(r#"{{"edits":[{{"file_path":"a.rs","old_string":{},"new_string":{}}}]}}"#, s(&old), s(&new));
    let chars: Vec<char> = input.chars().collect();
    let head = input.find("new_string").unwrap() + 14;
    // (four lumps of 190 characters, 640 ms apart: 25 frames each)
    let lumps: Vec<usize> = (0..5).map(|k| (head + k * 190).min(chars.len())).collect();
    let frames = 4 * 25;
    let mut best = vec![Duration::MAX; frames];
    for _ in 0..3 {
        let (mut ed, dir, g) = scene("writing-perf-reveal");
        ed.session.chat.as_mut().unwrap().writing.paced = true;
        std::fs::write(dir.join("a.rs"), &text).unwrap();
        crate::typed::execute(&mut ed, "reload").unwrap();
        let mut buf = Vec::with_capacity(1 << 16);
        crate::term::render(&mut ed, &mut buf, 120, 40).unwrap();
        let mut w = Writer::new("t1", Tool::Edit);
        let first: String = chars[..head].iter().collect();
        w.send(&mut ed, g, &first, &[], false);
        settle(&mut ed, |ed| shown(ed.doc()).is_some());
        let mut sim = crate::sim::Sim::new(
            "writing_perf_reveal",
            crate::sim::Choices::seeded(1),
            Duration::from_secs(120),
        );
        for (k, pair) in lumps.windows(2).enumerate() {
            let piece: String = chars[pair[0]..pair[1]].iter().collect();
            w.send(&mut ed, g, &piece, &[], false);
            for f in 0..25 {
                sim.wait(REVEAL_EVERY + Duration::from_micros(600));
                let t = Instant::now();
                tick(&mut ed);
                buf.clear();
                crate::term::render(&mut ed, &mut buf, 120, 40).unwrap();
                best[k * 25 + f] = best[k * 25 + f].min(t.elapsed());
            }
        }
        assert!(shown(ed.doc()).is_some_and(|s| s.blocks[0].rows.len() > 10), "its rows drawn");
        drop(sim);
    }
    let worst = best.iter().max().copied().unwrap_or_default();
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let line = format!("PERF reveal_frame_with_live_rows {:.3} budget={:.0}\n", ms(worst), ms(budget));
    let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    assert!(worst < budget, "a frame took {worst:?} (budget {budget:?})");
}

/// Claude Code's own Edit, from the stream's lines to the rows: its block read by the reader (`Reading`), its
/// pieces drawn where they go. A message's start begins its blocks anew: a piece for a block of the message before
/// is no one's.
#[test]
fn the_native_edit_streams_from_its_lines_to_its_rows() {
    let (mut ed, _dir, g) = scene("writing-native");
    let line = |e: Value| json!({"type": "stream_event", "event": e});
    let piece = |p: &str| {
        line(json!({"type": "content_block_delta", "index": 1,
            "delta": {"type": "input_json_delta", "partial_json": p}}))
    };
    let mut r = Reading::default();
    let start = json!({"type": "content_block_start", "index": 1,
        "content_block": {"type": "tool_use", "id": "e1", "name": "Edit", "input": {}}});
    assert!(r.line(&line(start)));
    let input =
        r#"{"file_path":"a.rs","old_string":"    let b = 2;\n","new_string":"    let b = 22;\n    let d = 4"#;
    for p in input.as_bytes().chunks(9) {
        assert!(r.line(&piece(std::str::from_utf8(p).unwrap())));
    }
    for d in r.take() {
        assert_eq!((d.call.as_str(), d.tool), ("e1", Tool::NativeEdit));
        crate::chat::stream_event(&mut ed, g, StreamEv::Writing(d));
    }
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    let rows = frame(&mut ed, 100, 30);
    assert!(rows.iter().any(|r| r.ends_with("let b = 22;")) && rows.iter().any(|r| r.ends_with("let d = 4")));
    // A new message: the block it numbered 1 is gone — its piece no one's
    assert!(!r.line(&line(json!({"type": "message_start", "message": {}}))));
    assert!(!r.line(&piece(";\n\"}")), "a piece of the message before");
    assert!(r.take().is_empty());
}

/// The chat closed mid-turn: its process goes and its end is never heard — the rows go with it.
#[test]
fn a_closed_chat_takes_its_rows_with_it() {
    let (mut ed, _dir, g) = scene("writing-close");
    let input = edit_input("    let b = 20;\n");
    Writer::new("t1", Tool::Edit).send(&mut ed, g, &input[..input.len() - 8], &[], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    crate::chat::close(&mut ed);
    assert!(ed.doc().writing.is_none());
    assert!(!frame(&mut ed, 100, 30).iter().any(|r| r.contains("▎    let b = 20")));
}

/// A review that opened on the file once the call was whole — and was answered `n` — before the place was found:
/// the place, found after, is the review's; nothing drawn again.
#[test]
fn a_place_found_after_its_review_is_not_drawn() {
    let (mut ed, _dir, g) = scene("writing-late");
    let input = edit_input("    let b = 20;\n");
    // (whole: the place is being looked for, its result not landed yet)
    write(&mut ed, g, "t1", Tool::Edit, &input, &[]);
    let id = ed.doc().id;
    let from = TEXT.find("    let b").unwrap();
    let change = (from, from + 15, "    let b = 2;\n".to_string(), "    let b = 20;\n".to_string());
    crate::llm::review_with(&mut ed, id, "an edit", vec![change], None);
    crate::llm::close_review(&mut ed);
    let looked = |ed: &Editor| {
        let c = ed.session.chat.as_ref().unwrap();
        c.writing.calls.iter().flat_map(|c| &c.items).all(|it| !matches!(it.place, Place::Looking))
    };
    settle(&mut ed, looked);
    assert!(shown(ed.doc()).is_none(), "the rejected change drawn again");
}

/// macOS's Reduce Motion on the frame: `full` drawn as `calm` — the head still wet, the wait's frames a second
/// apart — and a toast's frames when nothing moves on a clock: none to fade it, one as its time is up.
#[test]
fn reduce_motion_and_calm_on_the_frame() {
    let (mut ed, _dir, g) = scene("writing-reduce");
    ed.wet_ink = true;
    ed.reduce_motion = true;
    let input = edit_input("    let b = 20;\n");
    Writer::new("t1", Tool::Edit).send(&mut ed, g, &input[..input.find("20").unwrap() + 2], &[], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    crate::term::WET_SEEN.with_borrow_mut(Vec::clear);
    frame(&mut ed, 100, 30);
    assert!(
        crate::term::WET_SEEN.with_borrow(|w| w.iter().any(|w| w.0 == "code")),
        "wet under Reduce Motion"
    );
    assert!(ed.tick_wanted().unwrap() > crate::editor::TICK * 2, "the wait's frames a second apart");
    // A toast with nothing waited on: its one frame as its time is up — at `full`, frames while it fades in
    crate::chat::stream_event(&mut ed, g, StreamEv::Done(Ok(String::new())));
    ed.toasts.clear();
    ed.set_status("saved");
    let calm = ed.tick_wanted().unwrap();
    assert!(calm > std::time::Duration::from_secs(1), "no fade: {calm:?}");
    ed.reduce_motion = false;
    assert_eq!(ed.tick_wanted(), Some(crate::editor::TICK), "full: it fades in");
}

/// Performance budget: a batch of a Write over a 5,000-line file (200 KB — laid out plain while it's written, the
/// diff waiting for the whole text), the rows laid out again and the frame drawn, near its end: within one frame
/// (16 ms; `TARAE_PERF_SLACK` loosens it); the worst batch, each its best of 3 rounds.
#[test]
fn perf_budget_stream_frame_of_a_big_write() {
    if crate::test_results::unoptimized("writing::tests::perf_budget_stream_frame_of_a_big_write") {
        return;
    }
    let slack = std::env::var("TARAE_PERF_SLACK").ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1);
    let budget = std::time::Duration::from_millis(16) * slack.max(1);
    let text: String = (0..5_000).map(|i| format!("    let value_{i} = compute({i}, \"타래\");\n")).collect();
    let new = text.replace("compute(", "measure(");
    let input = format!(r#"{{"file_path":"a.rs","content":{}}}"#, serde_json::to_string(&new).unwrap());
    let chars: Vec<char> = input.chars().collect();
    let near_end = chars.len() - 40 * 16 - 10;
    let cuts: Vec<usize> = (near_end..chars.len() - 10).step_by(16).collect();
    let mut best = vec![std::time::Duration::MAX; cuts.len() - 1];
    for _ in 0..3 {
        let (mut ed, dir, g) = scene("writing-perf-write");
        std::fs::write(dir.join("a.rs"), &text).unwrap();
        crate::typed::execute(&mut ed, "reload").unwrap();
        let mut buf = Vec::with_capacity(1 << 16);
        crate::term::render(&mut ed, &mut buf, 120, 40).unwrap();
        let mut w = Writer::new("w1", Tool::Write);
        let first: String = chars[..near_end].iter().collect();
        w.send(&mut ed, g, &first, &[], false);
        settle(&mut ed, |ed| shown(ed.doc()).is_some());
        crate::term::render(&mut ed, &mut buf, 120, 40).unwrap();
        for (k, pair) in cuts.windows(2).enumerate() {
            let piece: String = chars[pair[0]..pair[1]].iter().collect();
            let t = std::time::Instant::now();
            w.send(&mut ed, g, &piece, &[], false);
            buf.clear();
            crate::term::render(&mut ed, &mut buf, 120, 40).unwrap();
            best[k] = best[k].min(t.elapsed());
        }
        assert!(shown(ed.doc()).is_some_and(|s| s.blocks[0].rows.len() > 5_000), "its rows drawn");
    }
    let worst = best.iter().max().copied().unwrap_or_default();
    let ms = |d: std::time::Duration| d.as_secs_f64() * 1000.0;
    let line = format!("PERF stream_frame_of_a_big_write {:.3} budget={:.0}\n", ms(worst), ms(budget));
    let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    assert!(worst < budget, "a batch took {worst:?} (budget {budget:?})");
}

/// The native Write: the whole file its place, the text written so far over it.
#[test]
fn a_write_is_drawn_over_the_whole_file() {
    let (mut ed, _dir, g) = scene("writing-write");
    let input = json!({"file_path": "a.rs", "content": "fn main() {\n    let a = 1;\n    todo!()"});
    write(&mut ed, g, "w1", Tool::Write, &input.to_string(), &[]);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    let b = &shown(ed.doc()).unwrap().blocks[0];
    assert_eq!((b.first, b.last), (0, 4), "every line of it");
    let rows = frame(&mut ed, 100, 30);
    assert!(rows.iter().any(|r| r.ends_with("todo!()")), "{rows:#?}");
}

/// `editor.motion`: `none` draws nothing wet (the rows stay), `calm` keeps wet ink but moves nothing on a clock —
/// the wave still, a frame a second while waiting, a toast whole at once; macOS's Reduce Motion makes `full` calm,
/// never `none` less.
#[test]
fn the_motion_setting_holds_still_what_moves() {
    let (mut ed, _dir, g) = scene("writing-motion");
    ed.wet_ink = true;
    let input = edit_input("    let b = 20;\n");
    let cut = input.find("20").unwrap() + 2;
    Writer::new("t1", Tool::Edit).send(&mut ed, g, &input[..cut], &[], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    let wet = |ed: &mut Editor| {
        crate::term::WET_SEEN.with_borrow_mut(Vec::clear);
        let rows = frame(ed, 100, 30);
        assert!(rows.iter().any(|r| r.ends_with("let b = 20")), "the rows stay: {rows:#?}");
        crate::term::WET_SEEN.with_borrow(|w| w.iter().any(|w| w.0 == "code"))
    };
    assert!(wet(&mut ed), "full: wet");
    crate::typed::execute(&mut ed, "set editor.motion none").unwrap();
    assert!(!wet(&mut ed), "none: nothing wet");
    crate::typed::execute(&mut ed, "set editor.motion calm").unwrap();
    assert!(wet(&mut ed), "calm: wet ink stays");
    assert!(!ed.moving());
    let tick = ed.tick_wanted().unwrap();
    assert!(tick > crate::editor::TICK * 2, "waiting, a frame a second: {tick:?}");
    ed.set_status("saved");
    assert_eq!(ed.toast_alpha(), 1.0, "whole at once");
    crate::typed::execute(&mut ed, "set editor.motion full").unwrap();
    assert!(ed.moving());
    assert!(ed.toast_alpha() < 1.0, "fading in");
    ed.reduce_motion = true;
    assert_eq!(ed.motion(), crate::motion::Motion::Calm, "Reduce Motion: full as calm");
    crate::typed::execute(&mut ed, "set editor.motion none").unwrap();
    assert_eq!(ed.motion(), crate::motion::Motion::None, "an explicit none stays");
}

/// A region's lines are counted on bytes: its last byte (`to - 1`) sits inside the character a region ends with
/// when that one is wider than a byte (`—` is three) — slicing the text there panicked the worker, and the release
/// build's abort took tarae down with it (2026-10-22).
#[test]
fn a_regions_lines_count_bytes_never_slice_inside_a_character() {
    let text = "fn main() {\n    let a = 1; // one —\n}\n";
    let end = text.find('—').unwrap() + '—'.len_utf8();
    assert_eq!(lines_of(text, text.find("let").unwrap(), end), (1, 1), "a region ending in `—`");
    assert_eq!(lines_of(text, 0, text.len()), (0, 2), "the whole text, its last byte the last newline");
    assert_eq!(lines_of(text, 5, 5), (0, 0), "an empty region: its start");
}

/// The same through the stream: an edit whose `old_string` ends in `—` is placed (its worker answers), not lost.
#[test]
fn an_edit_ending_in_a_wide_character_is_placed() {
    let dir = crate::scratch::Scratch::new("writing-wide-end");
    let text = "fn main() {\n    let a = 1; // one —\n}\n";
    std::fs::write(dir.join("a.rs"), text).unwrap();
    let mut ed = Editor::new(Config::default());
    ed.session.root = dir.to_path_buf();
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    ed.open(&dir.join("a.rs")).unwrap();
    let mut c = crate::chat::Chat::new();
    let _ = crate::chat::stand_in(&mut c);
    c.state = State::Thinking;
    c.follow.on = true;
    let g = c.generation();
    ed.session.chat = Some(c);
    ed.wet_ink = true;
    let s = |t: &str| serde_json::to_string(t).unwrap();
    let input = format!(
        r#"{{"edits":[{{"file_path":"a.rs","old_string":{},"new_string":{}}}]}}"#,
        s("let a = 1; // one —"),
        s("let a = 1; // one — two")
    );
    let mut w = Writer::new("t1", Tool::Edit);
    w.send(&mut ed, g, &input, &[30], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    assert_eq!(
        shown(ed.doc()).map(|s| s.blocks.iter().map(|b| (b.first, b.last)).collect::<Vec<_>>()),
        Some(vec![(1, 1)])
    );
}

/// The pace (`Pace`), on the measured stream (`cargo xtask stream --edit`: lumps of ~190 characters every ~640 ms,
/// each lump's tail `llm::FLUSH` after its head): a lump is shown evenly until the next is due — never all at once as
/// it comes, all of it by then — and once the text is whole, what's left within `WHOLE`.
#[test]
fn what_came_is_shown_evenly_by_when_the_next_piece_is_due() {
    let t0 = Instant::now();
    let mut p = Pace::default();
    let mut text = String::new();
    // (time in 5 ms steps; a frame every 25 ms: (at, shown, came by then))
    let mut frames: Vec<(u64, usize, usize)> = Vec::new();
    for t in (0..3200u64).step_by(5) {
        let now = t0 + Duration::from_millis(t);
        let lump = t / 640;
        if lump < 4 && t % 640 == 0 {
            p.came(now, text.len());
            text.push_str(&"a".repeat(20));
        }
        if lump < 4 && t % 640 == 25 {
            p.came(now, text.len());
            text.push_str(&"b".repeat(170));
        }
        if t == 2600 {
            p.closed(now);
        }
        if t % 25 == 0 {
            p.step(&text, now);
            frames.push((t, p.shown, text.len()));
        }
    }
    for w in frames.windows(2).filter(|w| w[0].0 >= 640) {
        let ((_, a, _), (t, b, _)) = (w[0], w[1]);
        assert!(b >= a && b - a <= 12, "{} bytes in one frame at {t} ms: {frames:?}", b - a);
    }
    // (all of a lump shown by the next one's time: the frame before it within a frame's worth)
    for next in [1280, 1920] {
        let &(t, shown, came) = frames.iter().rfind(|f| f.0 < next).unwrap();
        assert!(came - shown <= 10, "{} left at {t} ms: {frames:?}", came - shown);
    }
    let whole = frames.iter().find(|f| f.1 == text.len()).map(|f| f.0);
    assert!(whole.is_some_and(|t| t <= 2600 + 150), "all of it within WHOLE once whole: {whole:?}");
}

/// A piece after a pause (all shown, then quiet for 3 s) is shown from when it came — not all at once on the next
/// frame; nor does the pause slow the pace past `GAP_MAX`.
#[test]
fn a_piece_after_a_pause_is_shown_from_when_it_came() {
    let t0 = Instant::now();
    let at = |ms: u64| t0 + Duration::from_millis(ms);
    let mut p = Pace::default();
    let mut text = String::new();
    for k in 0..3u64 {
        p.came(at(k * 640), text.len());
        text.push_str(&"a".repeat(190));
    }
    p.step(&text, at(2000));
    assert_eq!(p.shown, text.len(), "caught up");
    p.came(at(5000), text.len());
    text.push_str(&"b".repeat(190));
    p.step(&text, at(5025));
    let first = p.shown - 570;
    assert!(first > 0 && first <= 12, "a frame's worth, not all: {first}");
    p.step(&text, at(5000) + GAP_MAX);
    assert_eq!(p.shown, text.len(), "all of it by GAP_MAX at the most");
}

/// The reveal never shows half a character: a step ending inside a cluster goes on to its end.
#[test]
fn the_reveal_never_cuts_a_character() {
    let text = "ab e\u{301}\u{301}—한";
    for at in 0..=text.len() {
        let end = cluster_end(text, at);
        assert!(text.is_char_boundary(end) && end >= at, "{at} → {end}");
        assert!(![4, 5, 6, 7].contains(&end), "inside é́: {at} → {end}");
    }
}

/// A frame's tick, as the loop brings one: an event with nothing in it (the reveal moves after every event).
fn tick(ed: &mut Editor) {
    ed.handle_event(crate::event::Event::Job(Box::new(|_: &mut Editor| {})));
}

/// The row a line of the code area shows with `text` in it.
fn row_with(ed: &mut Editor, text: &str) -> Option<String> {
    frame(ed, 100, 30).into_iter().find(|r| r.contains(text))
}

/// Paced (`Live.paced`, on a simulation's clock): a piece isn't drawn as it comes — the frames after it show it a
/// little at a time, Claude's caret at the shown head; once whole, the rest within `WHOLE`; a frame is asked only
/// while there's text left to show.
#[test]
fn a_piece_is_drawn_as_typed_at_the_streams_pace() {
    let (mut ed, _dir, g) = scene("writing-paced");
    ed.session.chat.as_mut().unwrap().writing.paced = true;
    frame(&mut ed, 100, 30);
    let input = edit_input("    let b = 2000000000;\n");
    let head = input.find("2000").unwrap();
    let tail = head + input[head..].find(';').unwrap();
    let mut w = Writer::new("t1", Tool::Edit);
    w.send(&mut ed, g, &input[..head], &[], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    let mut sim =
        crate::sim::Sim::new("writing_paced", crate::sim::Choices::seeded(1), Duration::from_secs(30));
    sim.wait(Duration::from_secs(2));
    tick(&mut ed);
    assert!(row_with(&mut ed, "let b = ").is_some(), "what came before, shown by now");
    assert_eq!(reveal_wake(&ed), None, "nothing left to show: no frame");
    w.send(&mut ed, g, &input[head..tail], &[], false);
    tick(&mut ed);
    // (`let b = 2;` is the line it takes out)
    assert!(row_with(&mut ed, "let b = 20").is_none(), "not drawn as it came");
    assert!(reveal_wake(&ed).is_some_and(|d| d <= REVEAL_EVERY), "a frame asked");
    assert!(ed.tick_wanted().is_some_and(|d| d <= REVEAL_EVERY), "the loop's next frame that soon");
    sim.wait(Duration::from_millis(250));
    tick(&mut ed);
    let row = row_with(&mut ed, "let b = 20").expect("some of it, a frame later");
    assert!(!row.contains("2000000000"), "not all of it: {row}");
    // (Claude's caret right after what's shown)
    let b = shown(ed.doc()).and_then(|s| s.blocks.iter().find(|b| b.head.is_some()).cloned()).unwrap();
    assert!(matches!(&b.rows[b.head.unwrap()], Row::Other { text, .. } if row.ends_with(text.as_str())));
    w.send(&mut ed, g, &input[tail..], &[], true);
    tick(&mut ed);
    sim.wait(WHOLE + REVEAL_EVERY);
    tick(&mut ed);
    assert!(row_with(&mut ed, "let b = 2000000000;").is_some(), "whole within WHOLE");
    assert_eq!(reveal_wake(&ed), None, "all shown: no more frames");
    drop(sim);
}

/// The terminal unfocused (nobody sees it): no reveal frames asked — and the next event, focus back among them,
/// catches the reveal up.
#[test]
fn an_unfocused_tarae_asks_no_reveal_frames_and_catches_up() {
    let (mut ed, _dir, g) = scene("writing-paced-unfocused");
    ed.session.chat.as_mut().unwrap().writing.paced = true;
    frame(&mut ed, 100, 30);
    let input = edit_input("    let b = 2000000000;\n");
    let head = input.find("2000").unwrap();
    let tail = head + input[head..].find(';').unwrap();
    let mut w = Writer::new("t1", Tool::Edit);
    w.send(&mut ed, g, &input[..head], &[], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    let mut sim = crate::sim::Sim::new(
        "writing_paced_unfocused",
        crate::sim::Choices::seeded(1),
        Duration::from_secs(30),
    );
    sim.wait(Duration::from_secs(2));
    tick(&mut ed);
    ed.handle_event(crate::event::Event::Focus(false));
    w.send(&mut ed, g, &input[head..tail], &[], false);
    tick(&mut ed);
    assert!(reveal_wake(&ed).is_some(), "text left to show");
    assert!(ed.tick_wanted().is_none_or(|d| d > REVEAL_EVERY), "but no frame asked for it");
    sim.wait(Duration::from_secs(2));
    ed.handle_event(crate::event::Event::Focus(true));
    assert!(row_with(&mut ed, "let b = 2000000000").is_some(), "caught up as focus came back");
    assert_eq!(reveal_wake(&ed), None);
    drop(sim);
}

/// `editor.motion = "none"`: what's written is shown as it comes, no frames of its own.
#[test]
fn without_motion_the_writing_is_shown_as_it_comes() {
    let (mut ed, _dir, g) = scene("writing-paced-still");
    ed.session.chat.as_mut().unwrap().writing.paced = true;
    ed.config.motion = "none".into();
    frame(&mut ed, 100, 30);
    let input = edit_input("    let b = 3000;\n");
    let head = input.find("3000").unwrap();
    let mut w = Writer::new("t1", Tool::Edit);
    w.send(&mut ed, g, &input[..head], &[], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    let sim =
        crate::sim::Sim::new("writing_paced_still", crate::sim::Choices::seeded(1), Duration::from_secs(30));
    // (up to its closing `\n"}]}`; the event it came in, no time passing)
    w.send(&mut ed, g, &input[head..input.len() - 6], &[], false);
    tick(&mut ed);
    assert!(row_with(&mut ed, "let b = 3000;").is_some(), "all that came, at once");
    assert_eq!(reveal_wake(&ed), None, "no frames of its own");
    drop(sim);
}

/// The edit lands while its text is still being shown: the landed text is the buffer's at once, the rows go with the
/// next frame, and no frame is asked after.
#[test]
fn an_edit_landing_mid_reveal_shows_what_landed() {
    let (mut ed, dir, g) = scene("writing-paced-lands");
    ed.session.chat.as_mut().unwrap().writing.paced = true;
    frame(&mut ed, 100, 30);
    let input = edit_input("    let b = 2000000000;\n");
    let head = input.find("2000").unwrap();
    let mut w = Writer::new("t1", Tool::Edit);
    w.send(&mut ed, g, &input[..head], &[], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    let mut sim =
        crate::sim::Sim::new("writing_paced_lands", crate::sim::Choices::seeded(1), Duration::from_secs(30));
    sim.wait(Duration::from_secs(2));
    tick(&mut ed);
    w.send(&mut ed, g, &input[head..], &[], true);
    sim.wait(REVEAL_EVERY);
    tick(&mut ed);
    assert!(reveal_wake(&ed).is_some(), "mid-reveal");
    std::fs::write(dir.join("a.rs"), TEXT.replace("let b = 2;", "let b = 2000000000;")).unwrap();
    crate::typed::execute(&mut ed, "reload").unwrap();
    assert!(shown(ed.doc()).is_none(), "the rows of another version aren't drawn");
    assert!(row_with(&mut ed, "let b = 2000000000;").is_some(), "what landed");
    tick(&mut ed);
    assert_eq!(reveal_wake(&ed), None, "landed: no more frames");
    drop(sim);
}
