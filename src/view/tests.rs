//! View mode on the chat: keys in sequence, each followed by a frame (the conversation's document is what the
//! last frame laid out — as on screen).

use super::*;
use crate::chat::{Chat, Msg, Role};
use crate::config::Config;

const W: u16 = 140;
const H: u16 = 34;

/// A conversation: a question, an answer of a few paragraphs (one long enough to wrap), a read, a last word.
fn convo() -> Vec<Msg> {
    vec![
        Msg::new(Role::User, "why does insert clone the name?", ""),
        Msg::new(
            Role::Assistant,
            "First answer, its opening line.\n\nA second paragraph with the word needle in it.",
            "",
        ),
        Msg::new(Role::Note, "3.1s", ""),
        Msg::new(Role::User, "and the second?", ""),
        Msg::new(Role::Assistant, "The latest answer starts here.\n\nIts end, with another needle.", ""),
        Msg::new(Role::Note, "2.0s", ""),
    ]
}

/// An editor with the chat up and focused on `msgs`, a draft typed, a frame drawn.
fn chat_with(config: Config, msgs: Vec<Msg>) -> Editor {
    let mut ed = Editor::new(config);
    ed.docs[0].text = ropey::Rope::from_str("fn main() {}\nlet code = 1;\n");
    let mut c = Chat::new();
    c.msgs = msgs;
    c.focused = true;
    c.input = "a draft".into();
    c.cursor = c.input.len();
    ed.session.chat = Some(c);
    frame(&mut ed);
    ed
}

/// Forty questions and answers — taller than the panel.
fn long() -> Vec<Msg> {
    (0..40)
        .map(|i| match i % 2 {
            0 => Msg::new(Role::User, format!("question number {i}"), ""),
            _ => Msg::new(Role::Assistant, format!("answer number {i}"), ""),
        })
        .collect()
}

fn editor() -> Editor {
    chat_with(Config::default(), convo())
}

/// A frame, as the screen shows it.
fn frame(ed: &mut Editor) -> String {
    let mut bytes = Vec::new();
    crate::term::render(ed, &mut bytes, W, H).unwrap();
    let mut p = vt100::Parser::new(H, W, 0);
    p.process(&bytes);
    p.screen().contents()
}

/// Keys as `run` takes them (`<esc>`, `<C-w>`), a frame after each; what the code's document is stays put.
fn feed(ed: &mut Editor, keys: &str) {
    let code = (ed.doc().id, ed.docs.len());
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let key: Key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        };
        ed.handle_key(key);
        // (the swap: between keys the front document is the pane's, never the view's — a frame, a job and
        // `context()` see only that)
        assert_eq!((ed.doc().id, ed.docs.len()), code, "after {key}: the pane's document is current");
        frame(ed);
    }
}

fn view(ed: &Editor) -> &View {
    chat_view_of(ed).expect("in the chat's view mode")
}

/// The view's cursor line's text, and what's selected.
fn at(ed: &Editor) -> (String, String) {
    let d = view(ed).doc();
    let r = d.selection().primary();
    let line = mv::line_of(&d.text, r.cursor(&d.text));
    let l = d.text.line(line).to_string();
    (l.trim_end().to_string(), d.text.byte_slice(r.from()..r.to()).to_string())
}

fn region(ed: &Editor) -> String {
    ed.keys_to().region.name()
}

/// `esc` from the chat's input: its view mode, the draft kept, the keys the view's (`view:chat`, the status
/// line's `● view · chat`); `i`, `a`, `enter` each back to the input with the draft as it was.
#[test]
fn esc_from_the_input_views_the_conversation_and_i_types_again() {
    for back in ["i", "a", "<ret>"] {
        let mut ed = editor();
        feed(&mut ed, "<esc>");
        assert_eq!(region(&ed), "view:chat");
        let shown = frame(&mut ed);
        let status = shown.lines().last().unwrap_or_default().to_string();
        assert!(status.contains("view · chat"), "the status line: {status}");
        assert!(shown.contains("● view"), "the chat's title says it:\n{shown}");
        feed(&mut ed, back);
        assert_eq!(region(&ed), "chat", "{back}: the input again");
        assert!(ed.session.view.is_none(), "{back}: the view is gone");
        let c = ed.session.chat.as_ref().unwrap();
        assert_eq!((c.input.as_str(), c.cursor), ("a draft", 7), "{back}: the draft as it was");
    }
}

/// It lands on the last answer's first row; left and come back to, where you were.
#[test]
fn it_lands_on_the_last_answer_then_where_you_were() {
    let mut ed = editor();
    feed(&mut ed, "<esc>");
    assert_eq!(at(&ed).0, "The latest answer starts here.");
    feed(&mut ed, "gg");
    let d = view(&ed).doc();
    assert_eq!(mv::line_of(&d.text, d.selection().primary().head), 0, "gg: the first row");
    assert!(at(&ed).0.ends_with("why does insert clone the name?"), "the question's row: {:?}", at(&ed));
    // (the first needle after it: the first answer's)
    feed(&mut ed, "/needle<ret>");
    let there_now = at(&ed).0;
    assert_eq!(there_now, "A second paragraph with the word needle in it.");
    // (to the input and back: where it was)
    feed(&mut ed, "i<esc>");
    assert_eq!(at(&ed).0, there_now);
    // (to the code and back again, by the input)
    feed(&mut ed, "<esc>");
    assert_eq!(region(&ed), "code");
    ed.session.chat.as_mut().unwrap().focused = true;
    feed(&mut ed, "<esc>");
    assert_eq!(at(&ed).0, there_now);
}

/// Helix's moves and selections run on the transcript: `x` a row, `w` a word, `/` a search, `v` extends, a
/// count — the code's text, selection and mode untouched.
#[test]
fn moves_and_selections_run_on_the_transcript_never_the_code() {
    let mut ed = editor();
    let code_sel = ed.doc().selection().clone();
    feed(&mut ed, "<esc>ww");
    assert_eq!(at(&ed).1, "latest ");
    feed(&mut ed, "x");
    assert_eq!(at(&ed).1, "The latest answer starts here.\n");
    feed(&mut ed, "ggvj");
    assert_eq!(view(&ed).mode(), Mode::Select);
    assert!(frame(&mut ed).lines().last().unwrap_or_default().contains("view · chat · select"));
    feed(&mut ed, "<esc>");
    assert_eq!(view(&ed).mode(), Mode::Normal, "esc ends `v`, the view stays");
    assert!(chat_view_of(&ed).is_some());
    feed(&mut ed, "3j");
    assert_eq!(mv::line_of(&view(&ed).doc().text, view(&ed).doc().selection().primary().head), 4);
    feed(&mut ed, "%sneedle<ret>");
    assert_eq!(view(&ed).doc().selection().ranges().len(), 2, "two needles, two selections");
    assert_eq!(ed.mode, Mode::Normal, "the code's mode is its own");
    assert_eq!(ed.doc().selection(), &code_sel, "the code's selection untouched");
    assert_eq!(ed.doc().text.to_string(), "fn main() {}\nlet code = 1;\n");
}

/// What view mode has no use for — an edit, the space menu, `:` — is said, the transcript as it was.
#[test]
fn an_edit_or_a_menu_is_said_not_done() {
    let mut ed = editor();
    feed(&mut ed, "<esc>");
    let text = view(&ed).doc().text.to_string();
    for k in ["d", " ", ":", "p", "u", "o"] {
        ed.status = None;
        feed(&mut ed, k);
        let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert!(
            said.contains("isn't in view mode")
                && said.contains("i types")
                && said.contains("esc to the code"),
            "{k:?}: {said}"
        );
        assert_eq!(view(&ed).doc().text.to_string(), text, "{k:?}");
        assert!(ed.picker.is_none() && ed.prompt.is_none(), "{k:?} opened nothing");
    }
}

/// `esc` collapses a selection first, the view kept; with none, the keys go to the code.
#[test]
fn esc_collapses_then_leaves() {
    let mut ed = editor();
    feed(&mut ed, "<esc>xx");
    assert!(view(&ed).selects());
    feed(&mut ed, "<esc>");
    assert!(!view(&ed).selects(), "collapsed");
    assert_eq!(region(&ed), "view:chat");
    feed(&mut ed, "<esc>");
    assert_eq!(region(&ed), "code");
    assert!(ed.session.view.is_none());
    assert_eq!(ed.session.chat.as_ref().unwrap().input, "a draft");
}

/// The arrows: `↑`/`↓` a line of the view (the cursor carried at the edge), `←`/`→` a page back and on.
#[test]
fn the_arrows_scroll_a_line_and_a_page() {
    let mut ed = chat_with(Config::default(), long());
    feed(&mut ed, "<esc>");
    let top = view(&ed).doc().top;
    feed(&mut ed, "<up>");
    assert_eq!(view(&ed).doc().top, top - 1, "a line up");
    feed(&mut ed, "<down>");
    assert_eq!(view(&ed).doc().top, top);
    let line = |ed: &Editor| mv::line_of(&view(ed).doc().text, view(ed).doc().selection().primary().head);
    let before = line(&ed);
    feed(&mut ed, "<left>");
    let rows = ed.screen.chat_view.get().unwrap().2;
    assert_eq!(before - line(&ed), rows, "a page back");
    feed(&mut ed, "<right>");
    assert_eq!(line(&ed), before, "a page on");
}

/// It holds still: Claude's text arriving doesn't move it, the title says something came; `G` catches up, on
/// the latest line.
#[test]
fn it_holds_still_until_g_catches_up() {
    let mut ed = editor();
    feed(&mut ed, "<esc>gg");
    let rows = view(&ed).rows();
    let shown = frame(&mut ed);
    assert!(!shown.contains("new text after"));
    let c = ed.session.chat.as_mut().unwrap();
    c.msgs.push(Msg::new(Role::User, "a third question", ""));
    c.msgs.push(Msg::new(Role::Assistant, "A brand new answer.", ""));
    let shown = frame(&mut ed);
    assert!(shown.contains("new text after"), "said:\n{shown}");
    assert!(!shown.contains("A brand new answer."), "held still:\n{shown}");
    assert_eq!(view(&ed).rows(), rows);
    feed(&mut ed, "G");
    assert!(view(&ed).rows() > rows, "made again from what came");
    let shown = frame(&mut ed);
    assert!(
        shown.contains("A brand new answer.") && !shown.contains("new text after"),
        "caught up:\n{shown}"
    );
    let d = view(&ed).doc();
    assert_eq!(
        mv::line_of(&d.text, d.selection().primary().head),
        mv::last_line(&d.text),
        "on the latest line"
    );
    // (a count: that line, as `goto_line`)
    feed(&mut ed, "2G");
    assert_eq!(mv::line_of(&view(&ed).doc().text, view(&ed).doc().selection().primary().head), 1);
}

/// `y` copies what's selected to the clipboard: the rows as drawn, a line each.
#[test]
fn y_copies_the_rows_as_drawn() {
    let mut ed = editor();
    feed(&mut ed, "<esc>xy");
    let ev = ed.events.recv_timeout(std::time::Duration::from_secs(5)).expect("the copy's result");
    ed.handle_event(ev);
    assert!(crate::clipboard::written("The latest answer starts here.\n"));
    // (and to `"`: `p` in the code pastes it too)
    assert_eq!(
        ed.registers.get(&'"').map(Vec::as_slice),
        Some(&["The latest answer starts here.\n".to_string()][..])
    );
    assert_eq!(region(&ed), "view:chat");
}

/// For `term.rs`'s painter test: an editor in the chat's view mode after `keys`.
pub(crate) fn viewing(keys: &str) -> Editor {
    let mut ed = editor();
    feed(&mut ed, keys);
    ed
}

/// The wheel over the conversation: its view mode at that point, the keys its; back down to the end, nothing
/// selected, it ends — the keys where they were.
#[test]
fn the_wheel_enters_view_mode_and_leaves_it_at_the_end() {
    let mut ed = chat_with(Config::default(), long());
    ed.session.chat.as_mut().unwrap().focused = false;
    frame(&mut ed);
    let (y0, _, rows, first) = ed.screen.chat_view.get().unwrap();
    let x = ed.screen.chat_x.unwrap() + 4;
    let wheel = |ed: &mut Editor, kind| {
        ed.handle_mouse(crate::event::Mouse { kind, x: x as u16, y: (y0 + 5) as u16, alt: false });
        frame(ed);
    };
    wheel(&mut ed, crate::event::MouseKind::ScrollUp);
    assert_eq!(region(&ed), "view:chat");
    assert_eq!(view(&ed).doc().top, first - 3, "scrolled as the wheel took it");
    assert!(rows > 5);
    wheel(&mut ed, crate::event::MouseKind::ScrollDown);
    assert!(ed.session.view.is_none(), "back at the end: done reading");
    assert_eq!(region(&ed), "code", "the keys where they were");
    // (entered by a key, it stays)
    ed.session.chat.as_mut().unwrap().focused = true;
    feed(&mut ed, "<esc>");
    wheel(&mut ed, crate::event::MouseKind::ScrollUp);
    wheel(&mut ed, crate::event::MouseKind::ScrollDown);
    assert_eq!(region(&ed), "view:chat");
}

/// Your keys: `[keys.normal]` spills into view mode (kept to its commands), `[keys.view]` over it, `[keys.chat]`
/// moves the way in — the hints follow.
#[test]
fn your_keys_remap_view_mode_and_its_hints() {
    let (config, warnings) = crate::config::parse(
        "[keys.normal]\nw = \"move_next_word_end\"\n[keys.view]\nleft = \"page_down\"\ny = \"yank\"\n\"C-y\" = \"view_copy\"\n[keys.chat]\nesc = \"normal_mode\"\n\"C-v\" = \"chat_view\"\n",
    );
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut ed = chat_with(config, long());
    feed(&mut ed, "<C-v>");
    assert_eq!(region(&ed), "view:chat");
    assert!(frame(&mut ed).contains("C-y copy"), "the hint names the key as bound");
    feed(&mut ed, "w");
    assert_eq!(at(&ed).1, "answer", "normal's `w`, remapped, in view mode too");
    // (`left` remapped to `page_down`: from the first row, a page on — the default would stay at the top)
    feed(&mut ed, "gg<left>");
    let d = view(&ed).doc();
    let rows = ed.screen.chat_view.get().unwrap().2;
    assert_eq!(mv::line_of(&d.text, d.selection().primary().head), rows, "left pages on");
    assert!(d.top > 0, "the view went with it");
    feed(&mut ed, "i<C-v>i");
    assert_eq!(region(&ed), "chat");
}

/// The chat's chords stay its in view mode — `C-y` the last code block (none here: said) — and `C-w` is the
/// windows' (`C-w z` fills the screen with the chat, its view mode kept).
#[test]
fn the_chats_chords_work_in_view_mode() {
    let mut ed = editor();
    feed(&mut ed, "<esc><C-y>");
    assert_eq!(ed.status.as_ref().map(|s| s.0.as_str()), Some("no code block in the last answer"));
    assert_eq!(region(&ed), "view:chat");
    feed(&mut ed, "<C-w>z");
    assert_eq!(ed.zoomed(), Some(crate::sizes::Zoom::Chat));
    assert_eq!(region(&ed), "view:chat");
}

/// `space >` views the conversation from the code (which-key and the palette name it); the chat closed, it
/// opens — an empty conversation leaves its input the keys, said; one with something in it is viewed once the
/// frame has laid it out.
#[test]
fn space_gt_views_the_conversation_from_the_code() {
    let mut ed = editor();
    ed.session.chat.as_mut().unwrap().focused = false;
    frame(&mut ed);
    assert_eq!(region(&ed), "code");
    feed(&mut ed, " >");
    assert_eq!(region(&ed), "view:chat");
    assert_eq!(at(&ed).0, "The latest answer starts here.");
    // (the palette names its key)
    assert_eq!(ed.keymaps.bindings().get("chat_view").map(String::as_str), Some("space >"));
    // The chat closed: it opens, nothing in it to view — the input has the keys, said
    let mut ed = editor();
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    ed.session.chat = None;
    frame(&mut ed);
    feed(&mut ed, " >");
    assert_eq!(region(&ed), "chat");
    let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
    assert!(said.starts_with("nothing to view yet"), "{said}");
    // The chat open but not drawn (the code fills the screen): viewed with the next frame
    let mut ed = editor();
    ed.session.chat.as_mut().unwrap().focused = false;
    frame(&mut ed);
    ed.zoom = Some(crate::sizes::Zoom::Code(ed.session.focus));
    frame(&mut ed);
    assert!(ed.screen.chat_view.get().is_none(), "the chat isn't drawn");
    feed(&mut ed, " >");
    assert_eq!(region(&ed), "view:chat", "viewed once drawn");
}

/// An empty conversation has nothing to read: the input's `esc` goes to the code (its hint says `esc code`), the
/// wheel does nothing.
#[test]
fn an_empty_conversation_has_nothing_to_view() {
    let mut ed = chat_with(Config::default(), Vec::new());
    assert!(frame(&mut ed).contains("esc code"));
    feed(&mut ed, "<esc>");
    assert_eq!(region(&ed), "code");
    let (y0, ..) = ed.screen.chat_view.get().unwrap();
    let x = ed.screen.chat_x.unwrap() + 4;
    ed.handle_mouse(crate::event::Mouse {
        kind: crate::event::MouseKind::ScrollUp,
        x: x as u16,
        y: (y0 + 2) as u16,
        alt: false,
    });
    assert!(ed.session.view.is_none(), "the wheel: nothing to view");
}

/// `q` leaves as `esc` does (tmux's copy mode, less) — a selection collapsing first.
#[test]
fn q_leaves_view_mode_as_esc_does() {
    let mut ed = editor();
    feed(&mut ed, "<esc>xq");
    assert_eq!(region(&ed), "view:chat", "collapsed first");
    feed(&mut ed, "q");
    assert_eq!(region(&ed), "code");
}

/// Leaving after something came while it held: the rows you were reading stay in sight (the chat's scroll
/// counted from the live bottom, what came below them).
#[test]
fn leaving_keeps_the_rows_you_read_when_text_came_after() {
    let mut ed = chat_with(Config::default(), long());
    feed(&mut ed, "<esc><up><up><up>");
    let (top, held) = (view(&ed).doc().top, view(&ed).rows());
    assert!(top > 0);
    let c = ed.session.chat.as_mut().unwrap();
    for i in 0..6 {
        c.msgs.push(Msg::new(Role::Assistant, format!("a new answer {i}"), ""));
    }
    assert!(frame(&mut ed).contains("new text after"));
    feed(&mut ed, "<esc>");
    assert!(ed.session.view.is_none(), "left");
    let first = ed.screen.chat_view.get().unwrap().3;
    assert_eq!(first, top, "the row on top before is on top now");
    let total: usize = ed.screen.chat_laid.borrow().iter().map(|(_, r)| r.len()).sum();
    assert!(total > held, "the new rows are there, below");
}

/// A new conversation (`C-l`) or an earlier one (`C-p` → `chat::continue_with`) from view mode: the view of the
/// one left goes with it — nothing of its rows or its place is the next one's.
#[test]
fn another_conversation_ends_the_view_and_keeps_nothing_of_it() {
    // (a new or an earlier conversation starts a claude: the stand-in, never the real one)
    let editor = || {
        let mut ed = editor();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        ed
    };
    let mut ed = editor();
    feed(&mut ed, "<esc>gg<C-l>");
    assert!(ed.session.view.is_none(), "C-l: the view went");
    assert_eq!(ed.session.chat.as_ref().unwrap().view_at, None);
    assert_eq!(region(&ed), "chat");
    let mut ed = editor();
    feed(&mut ed, "<esc>gg");
    let saved = crate::chats::Saved {
        id: "earlier".into(),
        title: "an earlier one".into(),
        updated: 0,
        rows: Vec::new(),
        plan: None,
        usage: None,
        seq: 0,
    };
    crate::chat::continue_with(&mut ed, saved);
    frame(&mut ed);
    assert!(ed.session.view.is_none(), "C-p's pick: the view went");
    assert_eq!(ed.session.chat.as_ref().unwrap().view_at, None);
}

/// A session switch ends the view mode in front — its prompt (`/`) closed, nothing of it riding into the session
/// behind but its place, kept for next time.
#[test]
fn a_session_switch_ends_the_view_and_its_prompt() {
    let dir = crate::scratch::Scratch::new("view-switch");
    let mut ed = editor();
    let id = ed.session_open(dir.path()).unwrap();
    assert!(ed.session_switch(0));
    frame(&mut ed);
    feed(&mut ed, "<esc>gg/need");
    assert!(ed.prompt.is_some() && view(&ed).prompt, "its search prompt up");
    assert!(ed.session_switch(id));
    assert!(ed.prompt.is_none(), "its prompt closed");
    assert!(ed.session.view.is_none() && ed.sessions.iter().all(|s| s.view.is_none()), "no view anywhere");
    let behind = ed.sessions.iter().find(|s| s.id == 0).unwrap();
    assert!(behind.chat.as_ref().unwrap().view_at.is_some(), "its place kept");
}

/// The state lists the view's shape — never its text — and Claude's level none of it.
#[test]
fn the_state_says_the_views_shape_and_claude_never_sees_it() {
    let mut ed = editor();
    feed(&mut ed, "<esc>x");
    let s = ed.ui_state();
    let v = s.sessions[0].view.clone().expect("the front session's view");
    assert_eq!(v["surface"], "chat");
    assert_eq!(v["selections"], 1);
    assert_eq!(v["selects"], true);
    assert!(!v.to_string().contains("latest"), "no text: {v}");
    let claude = ed.ui_state_claude();
    assert!(claude.sessions.iter().all(|s| s.view.is_none()), "Claude's level holds it back");
}
