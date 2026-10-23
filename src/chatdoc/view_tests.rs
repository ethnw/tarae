//! The chat's view mode — normal mode on its transcript: keys in sequence, each followed by a frame (the transcript
//! follows what the frame laid out — as on screen).

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
    c.input = "a draft".into();
    ed.session.chat = Some(c);
    ed.session.at = crate::session::At::Chat;
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

fn view(ed: &Editor) -> &Transcript {
    assert!(has_keys(ed), "in the chat's view mode");
    transcript(ed).expect("its transcript")
}

fn doc(ed: &Editor) -> &Document {
    view(ed).pane.doc()
}

/// The row of the transcript its cursor is on.
fn cursor_row(ed: &Editor) -> usize {
    let d = doc(ed);
    let g = d.rows.as_deref().unwrap();
    let c = d.selection().primary().cursor(&d.text);
    let line = mv::line_of(&d.text, c);
    g.index(line, crate::wrap::row_of(g.of(line), c))
}

/// The view's cursor line's text, and what's selected.
fn at(ed: &Editor) -> (String, String) {
    let d = doc(ed);
    let r = d.selection().primary();
    let line = mv::line_of(&d.text, r.cursor(&d.text));
    let l = d.text.line(line).to_string();
    (l.trim_end().to_string(), d.text.byte_slice(r.from()..r.to()).to_string())
}

fn region(ed: &Editor) -> String {
    ed.keys_to().region.name()
}

/// Each pane its own search (panes P2a, `target/sb/panes-plan.md` §2.3): the chat's view mode's `/` searches its
/// rows only — its matches there while typed and after, `n` moving there; the code's pattern, highlights and
/// selection as they were (they leaked: the view's `/` was the editor's register, its preview counted and
/// scrolled the code). `esc` clears the view's matches, not the code's. And the other way round: the code's
/// `n` repeats the code's search, a view's `n` never the code's.
#[test]
fn a_view_modes_search_is_its_own_and_the_codes_stays_the_codes() {
    let mut ed = editor();
    ed.docs[0].text = ropey::Rope::from_str("let code = 1;\nlet more = code;\n");
    ed.session.at = crate::session::At::Code;
    feed(&mut ed, "/code<ret>");
    let code_sel = ed.doc().selection().primary();
    let code_search = |ed: &Editor| {
        let hits = ed.search_hits.as_ref().map(|h| (h.pattern.clone(), h.matches.len()));
        (ed.search.clone(), ed.search_hl, hits)
    };
    let before = code_search(&ed);
    assert_eq!(before, (Some("code".into()), true, Some(("code".into(), 2))), "the code's search");
    ed.session.at = crate::session::At::Chat;
    feed(&mut ed, "<esc>");
    assert_eq!(region(&ed), "view:chat");
    // Typing: the view's rows previewed, the code's untouched
    feed(&mut ed, "/need");
    assert_eq!(view(&ed).pane.hits().map(|h| (h.pattern.as_str(), h.matches.len())), Some(("need", 2)));
    assert_eq!(code_search(&ed), before, "the code's search while the view's is typed");
    assert_eq!(ed.doc().selection().primary(), code_sel);
    feed(&mut ed, "le<ret>");
    assert_eq!(at(&ed).1, "needle", "the view's selection on its match");
    let first = doc(&ed).selection().primary();
    assert_eq!(view(&ed).pane.hits().map(|h| h.matches.len()), Some(2), "its matches shown there");
    assert_eq!(code_search(&ed), before, "the code's search after the view's");
    // `n`: the view's next match — the code's selection where it was
    feed(&mut ed, "n");
    assert_eq!(at(&ed).1, "needle");
    assert_ne!(doc(&ed).selection().primary(), first, "n moved in the view");
    assert_eq!((ed.doc().selection().primary(), code_search(&ed)), (code_sel, before.clone()));
    // `esc`: the selection collapses, the view's matches go — the code's stay
    feed(&mut ed, "<esc>");
    assert_eq!(region(&ed), "view:chat");
    assert!(view(&ed).pane.hits().is_none(), "the view's matches unshown");
    assert_eq!(code_search(&ed), before, "the code's highlights stay");
    // The other way round: back in the code (the view's `esc` with nothing selected), `n` is the code's search
    feed(&mut ed, "<esc>");
    assert_eq!(region(&ed), "code");
    feed(&mut ed, "n");
    let d = ed.doc();
    let r = d.selection().primary();
    assert_eq!(d.text.byte_slice(r.from()..r.to()).to_string(), "code", "the code's n: its own pattern");
    assert_ne!(r, code_sel, "n moved in the code");
    // …and back in the view, its `n` is its own search, kept with it — never the code's
    ed.session.at = crate::session::At::Chat;
    feed(&mut ed, "<esc>n");
    assert_eq!(at(&ed).1, "needle", "the view's own pattern");
    assert_eq!(code_search(&ed), (Some("code".into()), true, Some(("code".into(), 2))));
}

/// Friction 88 (the user walk of 2026-10-21): `C-w h` from the chat's input — the code is to its left — goes to the
/// code (it did nothing and said nothing); `C-w l` from the code's rightmost pane comes back. A hop with nothing that
/// way is said.
#[test]
fn c_w_h_from_the_chats_input_goes_to_the_code_on_its_left() {
    let mut ed = editor();
    ed.session.chat.as_mut().unwrap().input.clear();
    assert_eq!(region(&ed), "chat");
    feed(&mut ed, "<C-w>h");
    assert_eq!(region(&ed), "code", "the code, on the chat's left");
    feed(&mut ed, "<C-w>l");
    assert_eq!(region(&ed), "chat", "and back: the chat, right of the code");
    feed(&mut ed, "<C-w>l");
    assert_eq!(region(&ed), "chat");
    let said = ed.messages.last().map(|m| m.2.clone()).unwrap_or_default();
    assert!(said.contains("nothing to the right"), "{said}");
    // The side panel on the code's left: `C-w h` from the code reaches it, `C-w l` comes back
    let dir = crate::scratch::Scratch::new("c-w-side");
    ed.session.root = dir.path().to_path_buf();
    ed.sidebar_show(crate::sidebar::Mode::Files);
    feed(&mut ed, "<C-w>l");
    assert_eq!(region(&ed), "code");
    feed(&mut ed, "<C-w>h");
    assert!(region(&ed).starts_with("side"), "{}", region(&ed));
    feed(&mut ed, "<C-w>l");
    assert_eq!(region(&ed), "code");
    // A review's rail in the side panel's place: `C-w h` gives it the keys on the file in hand (as `space e`),
    // `C-w l` back
    // (three files, the code on the last of them in the rail's order; the rail's selection on the first)
    ed.session.sidebar = None;
    let files: Vec<crate::codereview::File> = ["a.rs", "b.rs", "c.rs"]
        .iter()
        .map(|rel| {
            let path = dir.path().join(rel);
            std::fs::write(&path, "fn f() {}\n").unwrap();
            crate::codereview::File {
                rel: rel.to_string(),
                path,
                added: 1,
                removed: 0,
                new: false,
                gone: false,
                binary: false,
            }
        })
        .collect();
    let r = crate::codereview::CodeReview::for_test(
        crate::codereview::Base::Head,
        dir.path().to_path_buf(),
        files,
    );
    assert_eq!(r.rail_order(), [0, 1, 2]);
    ed.session.code_review = Some(r);
    ed.open(&dir.path().join("c.rs")).unwrap();
    frame(&mut ed);
    assert!(ed.screen.rail_w > 0, "the rail drawn");
    assert_eq!(ed.session.code_review.as_ref().unwrap().selected, 0, "the rail's row: the first file");
    feed(&mut ed, "<C-w>h");
    assert_eq!(region(&ed), "review", "the rail (its region's name)");
    assert_eq!(ed.session.code_review.as_ref().unwrap().selected, 2, "on the file in hand: c.rs");
    feed(&mut ed, "<C-w>l");
    assert_eq!(region(&ed), "code");
    ed.session.code_review = None;
}

/// `esc` from the chat's input: its view mode, the draft kept, the keys the view's (`view:chat`, the status
/// line's `● normal · chat` — normal mode on its document, said as a run's output says it; the input's hint
/// `esc normal`); `i`, `a`, `enter` each back to the input with the draft as it was.
#[test]
fn esc_from_the_input_views_the_conversation_and_i_types_again() {
    for back in ["i", "a", "<ret>"] {
        let mut ed = editor();
        let shown = frame(&mut ed);
        assert!(shown.contains("esc normal"), "the input's hint names where esc goes:\n{shown}");
        feed(&mut ed, "<esc>");
        assert_eq!(region(&ed), "view:chat");
        let shown = frame(&mut ed);
        let status = shown.lines().last().unwrap_or_default().to_string();
        assert!(status.contains("● normal · chat "), "the status line: {status}");
        assert!(shown.contains("claude ── ● normal"), "the chat's title says it:\n{shown}");
        let title = shown.lines().find(|l| l.contains("claude ── ●")).unwrap_or_default();
        assert!(!(title.contains("view") || status.contains("view")), "no other word for it:\n{shown}");
        feed(&mut ed, back);
        assert_eq!(region(&ed), "chat", "{back}: the input again");
        assert!(!keyed(&ed), "{back}: the view is gone");
        let c = ed.session.chat.as_ref().unwrap();
        assert_eq!(
            (c.input.text().as_str(), c.input.cursor()),
            ("a draft", 7),
            "{back}: the draft as it was"
        );
    }
}

/// It lands on the last answer's first row; left and come back to, where you were.
#[test]
fn it_lands_on_the_last_answer_then_where_you_were() {
    let mut ed = editor();
    feed(&mut ed, "<esc>");
    assert_eq!(at(&ed).0, "The latest answer starts here.");
    feed(&mut ed, "gg");
    let d = doc(&ed);
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
    ed.session.at = crate::session::At::Chat;
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
    assert_eq!(view(&ed).pane.keys.mode, Mode::Select);
    let shown = frame(&mut ed);
    assert!(shown.lines().last().unwrap_or_default().contains("● select · chat "), "{shown}");
    assert!(shown.contains("claude ── ● select"), "the chat's title too:\n{shown}");
    feed(&mut ed, "<esc>");
    assert_eq!(view(&ed).pane.keys.mode, Mode::Normal, "esc ends `v`, the view stays");
    assert!(has_keys(&ed));
    feed(&mut ed, "3j");
    assert_eq!(mv::line_of(&doc(&ed).text, doc(&ed).selection().primary().head), 4);
    feed(&mut ed, "%sneedle<ret>");
    assert_eq!(doc(&ed).selection().ranges().len(), 2, "two needles, two selections");
    assert_eq!(ed.mode, Mode::Normal, "the code's mode is its own");
    assert_eq!(ed.doc().selection(), &code_sel, "the code's selection untouched");
    assert_eq!(ed.doc().text.to_string(), "fn main() {}\nlet code = 1;\n");
}

/// The transcript is read-only: an edit, a paste, typing — put back and said, with the ways on; the view stays.
/// `space` is the editor's menu, `:` the editor's command line (as from every pane).
#[test]
fn an_edit_is_put_back_and_said() {
    let mut ed = editor();
    feed(&mut ed, "<esc>");
    let text = doc(&ed).text.to_string();
    for k in ["d", "c", "o", "A"] {
        ed.status = None;
        feed(&mut ed, k);
        let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
        assert_eq!(said, "the conversation is read-only — i types, esc code", "{k:?}");
        assert_eq!(doc(&ed).text.to_string(), text, "{k:?}");
        assert_eq!((region(&ed), view(&ed).pane.keys.mode), ("view:chat".into(), Mode::Normal), "{k:?}");
    }
    feed(&mut ed, "<space>");
    assert!(view(&ed).pane.keys.pending.len() == 1, "space: a menu under way");
    feed(&mut ed, "<esc>:");
    assert!(ed.prompt.is_some() && !view(&ed).pane.prompt, "the editor's command line");
}

/// `esc` collapses a selection first, the view kept; with none, the keys go to the code.
#[test]
fn esc_collapses_then_leaves() {
    let mut ed = editor();
    feed(&mut ed, "<esc>xx");
    assert!(view(&ed).pane.selects());
    feed(&mut ed, "<esc>");
    assert!(!view(&ed).pane.selects(), "collapsed");
    assert_eq!(region(&ed), "view:chat");
    feed(&mut ed, "<esc>");
    assert_eq!(region(&ed), "code");
    assert!(!keyed(&ed));
    assert_eq!(ed.session.chat.as_ref().unwrap().input, "a draft");
}

/// The arrows: `↑`/`↓` a row of the view (the cursor carried at the edge), `←`/`→` a page back and on — by the
/// rows drawn, a paragraph's each its own.
#[test]
fn the_arrows_scroll_a_row_and_a_page() {
    let mut ed = chat_with(Config::default(), long());
    feed(&mut ed, "<esc>");
    let top = view(&ed).top_row();
    feed(&mut ed, "<up>");
    assert_eq!(view(&ed).top_row(), top - 1, "a row up");
    feed(&mut ed, "<down>");
    assert_eq!(view(&ed).top_row(), top);
    let before = cursor_row(&ed);
    feed(&mut ed, "<left>");
    let rows = ed.screen.chat_view.get().unwrap().2;
    assert_eq!(before - cursor_row(&ed), rows, "a page back");
    feed(&mut ed, "<right>");
    assert_eq!(cursor_row(&ed), before, "a page on");
}

/// It holds still: Claude's text arriving is appended below what's in sight — the rows in sight and the cursor
/// don't move — the title says something came; `G` catches up, on the latest line.
#[test]
fn it_holds_still_until_g_catches_up() {
    let mut ed = chat_with(Config::default(), long());
    feed(&mut ed, "<esc>gg");
    let (rows, at) = (view(&ed).rows(), doc(&ed).selection().clone());
    let shown = frame(&mut ed);
    assert!(!shown.contains("new text after"));
    let c = ed.session.chat.as_mut().unwrap();
    c.msgs.push(Msg::new(Role::User, "a third question", ""));
    c.msgs.push(Msg::new(Role::Assistant, "A brand new answer.", ""));
    let shown = frame(&mut ed);
    assert!(shown.contains("new text after"), "said:\n{shown}");
    assert!(!shown.contains("A brand new answer."), "held still:\n{shown}");
    assert_eq!((view(&ed).top_row(), doc(&ed).selection()), (0, &at), "nothing moved");
    assert!(view(&ed).rows() > rows, "what came is in it, below");
    feed(&mut ed, "G");
    let shown = frame(&mut ed);
    assert!(
        shown.contains("A brand new answer.") && !shown.contains("new text after"),
        "caught up:\n{shown}"
    );
    let d = doc(&ed);
    assert_eq!(
        mv::line_of(&d.text, d.selection().primary().head),
        mv::last_line(&d.text),
        "on the latest line"
    );
    // (a count: that line, as `goto_line`)
    feed(&mut ed, "2G");
    assert_eq!(mv::line_of(&doc(&ed).text, doc(&ed).selection().primary().head), 1);
}

/// What arrives never takes the selection along: `x` on the paragraph being written keeps those bytes while words come
/// at its end, and when a message comes after it; a cursor on its line break keeps its cell, the words going on under
/// and after it.
#[test]
fn what_arrives_never_takes_the_selection_along() {
    let mut msgs = convo();
    msgs.pop();
    let mut ed = chat_with(Config::default(), msgs);
    feed(&mut ed, "<esc>Gx");
    assert_eq!(at(&ed).1, "Its end, with another needle.\n");
    let arrive = |ed: &mut Editor, f: &dyn Fn(&mut Chat)| {
        let (sel, row) = (doc(ed).selection().clone(), cursor_row(ed));
        f(ed.session.chat.as_mut().unwrap());
        frame(ed);
        assert_eq!((doc(ed).selection(), cursor_row(ed)), (&sel, row), "nothing moved");
    };
    let words = |c: &mut Chat| {
        let m = c.msgs.last_mut().unwrap();
        m.text.push_str(" More words came.");
        m.changed();
    };
    arrive(&mut ed, &words);
    let line = "Its end, with another needle. More words came.";
    assert_eq!(at(&ed), (line.into(), "Its end, with another needle. ".into()));
    // (a cursor on the line break: what comes there goes under it)
    let under = |ed: &Editor| {
        let d = doc(ed);
        let c = d.selection().primary().cursor(&d.text);
        d.text.byte_slice(c..crate::graphemes::next_boundary(&d.text, c)).to_string()
    };
    feed(&mut ed, "x;");
    assert_eq!(under(&ed), "\n");
    arrive(&mut ed, &words);
    assert_eq!(under(&ed), " ", "the first word's space under it now");
    feed(&mut ed, "x");
    arrive(&mut ed, &|c| c.msgs.push(Msg::new(Role::User, "and then?", "")));
}

/// `y` copies what's selected as written: a paragraph the chat wrapped over several rows is one line — no
/// hanging indent, no row breaks — to the clipboard and to `"`.
#[test]
fn y_copies_the_text_as_written() {
    let para = "The name is cloned because insert takes the key by value, and first moves into the map in the \
                same call; keying by the name alone means a second strand with the same name replaces the first.";
    let msgs = vec![
        Msg::new(Role::User, "why does insert clone the name?", ""),
        Msg::new(Role::Assistant, para, ""),
    ];
    let mut ed = chat_with(Config::default(), msgs);
    feed(&mut ed, "<esc>");
    let d = doc(&ed);
    let line = mv::line_of(&d.text, d.selection().primary().cursor(&d.text));
    assert!(d.rows.as_deref().unwrap().of(line).len() > 2, "the paragraph drawn over several rows");
    feed(&mut ed, "xy");
    let ev = ed.events.recv_timeout(std::time::Duration::from_secs(5)).expect("the copy's result");
    ed.handle_event(ev);
    let want = format!("{para}\n");
    assert!(crate::clipboard::written(&want), "the paragraph, one line");
    // (and to `"`: `p` in the code pastes it too)
    assert_eq!(ed.registers.get(&'"').map(Vec::as_slice), Some(&[want][..]));
    assert_eq!(region(&ed), "view:chat");
}

/// `space y` in the view mode yanks to the clipboard as in the code — but the transcript draws no flash
/// (`term::chat_paint`), so its words are a toast after the frame, never only a note (`motion.rs`).
#[test]
fn a_yank_in_the_view_mode_is_said_by_a_toast() {
    let mut ed = editor();
    ed.motion_drawn = true;
    feed(&mut ed, "<esc>x y");
    let ev = ed.events.recv_timeout(std::time::Duration::from_secs(5)).expect("the copy's result");
    ed.handle_event(ev);
    frame(&mut ed);
    let toasts: Vec<&str> = ed.toasts.iter().map(|t| t.text.as_str()).collect();
    assert_eq!(toasts, ["copied 1 selection"], "not drawn: a toast");
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
    ed.session.leave(crate::session::At::Chat);
    frame(&mut ed);
    let (y0, _, rows, first) = ed.screen.chat_view.get().unwrap();
    let x = ed.screen.chat_x.unwrap() + 4;
    let wheel = |ed: &mut Editor, kind| {
        ed.handle_mouse(crate::event::Mouse { kind, x: x as u16, y: (y0 + 5) as u16, alt: false });
        frame(ed);
    };
    wheel(&mut ed, crate::event::MouseKind::ScrollUp);
    assert_eq!(region(&ed), "view:chat");
    assert_eq!(view(&ed).top_row(), first - 3, "scrolled as the wheel took it");
    assert!(rows > 5);
    wheel(&mut ed, crate::event::MouseKind::ScrollDown);
    assert!(!keyed(&ed), "back at the end: done reading");
    assert_eq!(region(&ed), "code", "the keys where they were");
    // (entered by a key, it stays)
    ed.session.at = crate::session::At::Chat;
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
    let d = doc(&ed);
    let rows = ed.screen.chat_view.get().unwrap().2;
    assert_eq!(mv::line_of(&d.text, d.selection().primary().head), rows, "left pages on");
    assert!(d.top > 0, "the view went with it");
    feed(&mut ed, "i<C-v>i");
    assert_eq!(region(&ed), "chat");
}

/// `C-w q` in the chat's view mode closes nothing — as from its input, it's said (`space L` closes the chat); the
/// code's panes stay.
#[test]
fn ctrl_w_q_in_the_chats_view_is_said_and_closes_no_pane() {
    let mut ed = editor();
    ed.session.leave(crate::session::At::Chat);
    feed(&mut ed, "<C-w>v");
    let panes = ed.session.views.len();
    assert!(panes >= 2, "two panes");
    ed.session.at = crate::session::At::Chat;
    feed(&mut ed, "<esc><C-w>q");
    assert_eq!(ed.session.views.len(), panes, "no pane closed");
    assert_eq!(
        ed.status.as_ref().map(|s| s.0.as_str()),
        Some("C-w q closes panels — space L closes the chat")
    );
    assert_eq!(region(&ed), "view:chat");
}

/// The chat's chords stay its in view mode — `C-y` the last code block (none here: said) — and `C-w` is the
/// windows' (`C-w z` fills the screen with the chat, its view mode kept).
#[test]
fn the_chats_chords_work_in_view_mode() {
    let mut ed = editor();
    feed(&mut ed, "<esc>");
    // (and the state says so: the chord's layer is the chat's — the routing matrix probes no chord)
    let chord: Key = "C-y".parse().unwrap();
    let said = ed.keys_to().route(chord).handler();
    crate::uistate::TOOK.set(None);
    ed.handle_key(chord);
    assert_eq!(
        (crate::uistate::TOOK.get(), said),
        (Some(crate::uistate::Handler::Chat), crate::uistate::Handler::Chat)
    );
    frame(&mut ed);
    assert_eq!(ed.status.as_ref().map(|s| s.0.as_str()), Some("no code block in the last answer"));
    assert_eq!(region(&ed), "view:chat");
    // (normal mode's `C-s` saves a selection: in the conversation it's the chat's — the screen attached)
    feed(&mut ed, "<C-s>");
    assert!(ed.session.chat.as_ref().unwrap().screen.is_some(), "the screen attached");
    assert_eq!(region(&ed), "view:chat");
    feed(&mut ed, "<C-w>z");
    assert_eq!(ed.zoomed_shows(), Some(crate::pane::Shows::Chat));
    assert_eq!(region(&ed), "view:chat");
}

/// `space >` views the conversation from the code (which-key and the palette name it); the chat closed, it
/// opens — an empty conversation leaves its input the keys, said; one with something in it is viewed once the
/// frame has laid it out.
#[test]
fn space_gt_views_the_conversation_from_the_code() {
    let mut ed = editor();
    ed.session.leave(crate::session::At::Chat);
    frame(&mut ed);
    assert_eq!(region(&ed), "code");
    feed(&mut ed, " >");
    assert_eq!(region(&ed), "view:chat");
    assert_eq!(at(&ed).0, "The latest answer starts here.");
    // (the palette names its key — `quote`'s: nothing selected in the code, it views the chat)
    assert_eq!(ed.keymaps.bindings().get("quote").map(String::as_str), Some("space >"));
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
    ed.session.leave(crate::session::At::Chat);
    frame(&mut ed);
    ed.session.zoom = Some(crate::sizes::Zoom(ed.session.focus));
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
    assert!(!keyed(&ed), "the wheel: nothing to view");
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
    let (top, held) = (view(&ed).top_row(), view(&ed).rows());
    assert!(top > 0);
    let c = ed.session.chat.as_mut().unwrap();
    for i in 0..6 {
        c.msgs.push(Msg::new(Role::Assistant, format!("a new answer {i}"), ""));
    }
    assert!(frame(&mut ed).contains("new text after"));
    feed(&mut ed, "<esc>");
    assert!(!keyed(&ed), "left");
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
    assert!(!keyed(&ed), "C-l: the view went");
    assert!(!transcript(&ed).unwrap().visited, "where it was is forgotten");
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
    assert!(!keyed(&ed), "C-p's pick: the view went");
    assert!(!transcript(&ed).unwrap().visited, "where it was is forgotten");
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
    assert!(ed.prompt.is_some() && view(&ed).pane.prompt, "its search prompt up");
    assert!(ed.session_switch(id));
    assert!(ed.prompt.is_none(), "its prompt closed");
    let behind = ed.sessions.iter().find(|s| s.id == 0).unwrap();
    let t = behind.chat.as_ref().unwrap().transcript.as_ref().unwrap();
    assert!(!t.keyed && !keyed(&ed), "no view anywhere");
    assert!(t.visited, "its place kept");
}

/// `x enter` on the conversation: a chip over the input — Claude's answer, its row — the input with the keys and its
/// draft as left, the view gone; the hint rows say `enter quote` while something is selected. Two cursors, two
/// chips. `enter` without a selection still types.
#[test]
fn enter_quotes_the_selection_and_the_input_takes_the_keys() {
    let mut ed = editor();
    feed(&mut ed, "<esc>x");
    let shown = frame(&mut ed);
    // (said while selected: on the status line and the chat's hint row)
    assert_eq!(shown.lines().filter(|l| l.contains("enter quote")).count(), 2, "{shown}");
    feed(&mut ed, "<ret>");
    assert_eq!(region(&ed), "chat");
    assert!(!keyed(&ed));
    let c = ed.session.chat.as_ref().unwrap();
    assert_eq!(c.input, "a draft");
    let q = &c.quotes[..];
    assert_eq!((q.len(), q[0].text.as_str()), (1, "The latest answer starts here."));
    assert_eq!(q[0].label, "Claude's answer 1 line");
    assert_eq!(q[0].attrs, [("from", "chat".to_string()), ("role", "assistant".to_string())]);
    assert!(frame(&mut ed).contains("Claude's answer 1 line goes with it"));
    // Plain cursors, nothing wider than a character: `enter` types, `enter quote` isn't said
    let mut ed = editor();
    feed(&mut ed, "<esc>C");
    assert!(
        view(&ed).pane.selects() && !frame(&mut ed).contains("enter quote"),
        "two cursors, nothing to quote"
    );
    feed(&mut ed, "<ret>");
    assert_eq!(region(&ed), "chat");
    assert!(ed.session.chat.as_ref().unwrap().quotes.is_empty());
    // Two selections: two quotes
    let mut ed = editor();
    feed(&mut ed, "<esc>%sneedle<ret><ret>");
    let c = ed.session.chat.as_ref().unwrap();
    let texts: Vec<&str> = c.quotes.iter().map(|q| q.text.as_str()).collect();
    assert_eq!(texts, ["needle", "needle"]);
    // The user's own words from their message, its bar left out
    let mut ed = editor();
    feed(&mut ed, "<esc>gg/insert<ret>x<ret>");
    let c = ed.session.chat.as_ref().unwrap();
    assert_eq!(c.quotes[0].text, "why does insert clone the name?");
    assert_eq!(c.quotes[0].label, "your message 1 line");
    // Two paragraphs of yours: the blank line between them a blank line, its bar left out too
    let msgs =
        vec![Msg::new(Role::User, "first part\n\nsecond part", ""), Msg::new(Role::Assistant, "ok", "")];
    let mut ed = chat_with(Config::default(), msgs);
    feed(&mut ed, "<esc>/first<ret>xxx<ret>");
    assert_eq!(ed.session.chat.as_ref().unwrap().quotes[0].text, "first part\n\nsecond part");
}

/// A paragraph the chat wrapped is quoted as the line it was; a code row cut with `…` as its whole line; the
/// code well's edge left out.
#[test]
fn a_quote_of_the_chat_is_the_text_not_its_rows() {
    let words = "every word of a paragraph long enough to wrap in the chat's narrow column twice over at least, \
                 and then some more of them so that whatever the chat's width it wraps more than once";
    let code = format!("let x = \"{}\";", "a".repeat(150));
    let answer = format!("{words}\n\n```\n{code}\n```");
    let mut ed = chat_with(
        Config::default(),
        vec![Msg::new(Role::User, "q", ""), Msg::new(Role::Assistant, answer, "")],
    );
    feed(&mut ed, "<esc>");
    // (one line of the transcript, wrapped into the chat's rows)
    assert_eq!(at(&ed).0, words, "on the answer's paragraph, one line");
    let d = doc(&ed);
    let line = mv::line_of(&d.text, d.selection().primary().head);
    assert!(d.rows.as_deref().unwrap().of(line).len() > 1, "wrapped on screen");
    feed(&mut ed, "mip<ret>");
    let c = ed.session.chat.as_ref().unwrap();
    assert_eq!(c.quotes[0].text, words, "rejoined");
    // The code row, cut on screen: whole
    let mut ed = chat_with(
        Config::default(),
        vec![Msg::new(Role::User, "q", ""), Msg::new(Role::Assistant, format!("```\n{code}\n```"), "")],
    );
    let shown = frame(&mut ed);
    assert!(!shown.contains(&code), "cut on screen");
    feed(&mut ed, "<esc>/let<ret>x<ret>");
    assert_eq!(ed.session.chat.as_ref().unwrap().quotes[0].text, code);
}

/// A paragraph is one line of the transcript however it wraps: `j`/`k` step by its drawn rows, `x` takes it whole,
/// a search finds across a wrap, and a resize moves no cursor — it stays on its character, kept in sight.
#[test]
fn a_paragraph_is_a_line_its_rows_are_drawn() {
    let words = "every word of a paragraph long enough to wrap in the chat's narrow column twice over at least, \
                 and then some more of them so that whatever the chat's width it wraps more than once";
    let msgs =
        vec![Msg::new(Role::User, "q", ""), Msg::new(Role::Assistant, format!("{words}\n\nAfter."), "")];
    let mut ed = chat_with(Config::default(), msgs);
    feed(&mut ed, "<esc>");
    assert_eq!(at(&ed).0, words);
    let (line, row) = (mv::line_of(&doc(&ed).text, doc(&ed).selection().primary().head), cursor_row(&ed));
    // (`j`: its next drawn row, the same line)
    feed(&mut ed, "j");
    assert_eq!(cursor_row(&ed), row + 1, "a drawn row down");
    assert_eq!(mv::line_of(&doc(&ed).text, doc(&ed).selection().primary().head), line, "the same paragraph");
    feed(&mut ed, "k");
    assert_eq!(cursor_row(&ed), row);
    // (`x`: the paragraph, all its rows)
    feed(&mut ed, "x");
    assert_eq!(at(&ed).1, format!("{words}\n"));
    // (a search across where it wraps: the paragraph's first word to its last)
    feed(&mut ed, "<esc>gg/every.*once<ret>");
    assert_eq!(at(&ed).1, words, "found across its rows");
    // A resize: the same character under the cursor, still in sight
    feed(&mut ed, "<esc>gg/whatever<ret>");
    let before = doc(&ed).selection().primary();
    let mut bytes = Vec::new();
    crate::term::render(&mut ed, &mut bytes, W - 40, H).unwrap();
    assert_eq!(doc(&ed).selection().primary(), before, "no cursor moved");
    // (in sight at once, no key pressed: `resized` after the frame)
    let area = ed.screen.chat_view.get().unwrap().2;
    let (top, at_row) = (view(&ed).top_row(), cursor_row(&ed));
    assert!((top..top + area).contains(&at_row), "in sight: row {at_row} of {top}..+{area}");
}

/// The chat drawn shorter while its view has the keys: the cursor kept in sight by that frame, no key pressed.
#[test]
fn a_shorter_chat_keeps_the_cursor_in_sight_at_once() {
    let mut ed = chat_with(Config::default(), long());
    feed(&mut ed, "<esc>G");
    let mut bytes = Vec::new();
    crate::term::render(&mut ed, &mut bytes, W, H - 14).unwrap();
    let area = ed.screen.chat_view.get().unwrap().2;
    let (top, at_row) = (view(&ed).top_row(), cursor_row(&ed));
    assert!((top..top + area).contains(&at_row), "in sight: row {at_row} of {top}..+{area}");
}

/// A press on the conversation's text places the cursor there, as in the code and a run's output: from the input,
/// its view mode on that character, the rows under the pointer where they were; in it, the cursor moved; past a
/// row's text, its line's end.
#[test]
fn a_click_on_the_conversation_puts_the_cursor_on_that_character() {
    let mut ed = editor();
    let screen = frame(&mut ed);
    let first = ed.screen.chat_view.get().unwrap().3;
    let chat_x = ed.screen.chat_x.unwrap();
    // (where a word is drawn in the chat's column)
    let cell = |screen: &str, word: &str| {
        screen.lines().enumerate().find_map(|(y, l)| {
            let l: String = l.chars().skip(chat_x).collect();
            let b = l.find(word)?;
            Some((chat_x + l[..b].chars().count(), y))
        })
    };
    let press = |ed: &mut Editor, (x, y): (usize, usize)| {
        let m =
            crate::event::Mouse { kind: crate::event::MouseKind::Down, x: x as u16, y: y as u16, alt: false };
        ed.handle_mouse(m);
        frame(ed)
    };
    let under = |ed: &Editor| {
        let d = doc(ed);
        let c = d.selection().primary().cursor(&d.text);
        d.text.byte_slice(c..).to_string()
    };
    let screen = press(&mut ed, cell(&screen, "needle in").unwrap());
    assert_eq!(region(&ed), "view:chat");
    assert!(under(&ed).starts_with("needle in it."), "on the word pressed: {:?}", under(&ed));
    assert_eq!(view(&ed).top_row(), first, "the rows under the pointer stay");
    assert!(!view(&ed).pane.selects(), "a cursor, nothing selected");
    // (in it: the cursor moves)
    let screen = press(&mut ed, cell(&screen, "latest").unwrap());
    assert!(under(&ed).starts_with("latest answer"), "{:?}", under(&ed));
    // (past a row's text: its line's end, the cell after it)
    let (x, y) = cell(&screen, "Its end, with").unwrap();
    press(&mut ed, (x + 60, y));
    assert!(under(&ed).starts_with('\n'), "at the line break: {:?}", under(&ed));
    assert_eq!(at(&ed).0, "Its end, with another needle.");
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
