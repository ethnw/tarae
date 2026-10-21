//! Live edits (`writing.rs`): what Claude writes drawn as it's written, the review opening on the same rows.

use serde_json::{Value, json};

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
    // (you in the code while Claude works)
    c.focused = false;
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
    text.lines().map(|l| l.chars().take(code).collect::<String>().trim_end().to_string()).collect()
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
    // (its place in sight: nothing moves — the cursor where you left it, the keys in the chat or not)
    ed.session.chat.as_mut().unwrap().focused = true;
    frame(&mut ed, 100, 30);
    let mut w = Writer::new("t1", Tool::Edit);
    w.send(&mut ed, g, &input[..mid], &[20, 60], false);
    settle(&mut ed, |ed| shown(ed.doc()).is_some());
    frame(&mut ed, 100, 30);
    w.send(&mut ed, g, "", &[], false);
    assert_eq!(ed.doc().selection().primary().head, 0, "the cursor where it was");
    ed.session.chat.as_mut().unwrap().focused = false;
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
/// down with it — and the cursor where follow put it, on the first line it replaces.
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
    assert_eq!(crate::movement::line_of(&ed.doc().text, ed.doc().selection().primary().head), 9);
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
