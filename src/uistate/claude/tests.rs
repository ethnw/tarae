use std::time::{Duration, Instant};

use super::*;
use crate::config::Config;
use crate::key::Key;
use crate::offer::{Offer, What};

fn editor() -> Editor {
    let mut ed = Editor::new(Config::default());
    ed.docs[0].text = ropey::Rope::from_str("one\ntwo\nthree\n");
    ed.session.root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    ed
}

/// A light card (`y` ends a code review — none here, so nothing happens); its id.
fn card(ed: &mut Editor, title: &str) -> u64 {
    let o = Offer::new(
        title.into(),
        "2 files not viewed.",
        "",
        String::new(),
        What::ReviewEnd { session: 0, started: std::time::Instant::now() },
    );
    let id = o.id;
    ed.push_offer(o);
    ed.offers.iter().find(|o| o.title == title).map_or(id, |o| o.id)
}

fn chat(ed: &mut Editor, draft: &str) {
    let mut c = crate::chat::Chat::new();
    c.input = draft.into();
    ed.session.chat = Some(c);
    ed.session.at = crate::session::At::Chat;
}

fn frame(ed: &mut Editor) -> String {
    ed.focus_sync();
    crate::term::render(ed, &mut Vec::new(), 120, 40).unwrap();
    crate::screen::text(ed.last_frame.as_ref().unwrap().0.as_slice(), 120, 40).unwrap()
}

fn key(s: &str) -> Key {
    match s.chars().count() {
        1 => Key::plain(crate::key::Code::Char(s.chars().next().unwrap())),
        _ => s.trim_matches(['<', '>']).parse().unwrap(),
    }
}

/// A session behind, named `name`, in folder `root` — its id.
fn behind(ed: &mut Editor, name: &str, root: std::path::PathBuf) -> crate::event::SessionId {
    let id = ed.session_add(root);
    ed.session_by_id(id).unwrap().name = name.into();
    id
}

fn ids(text: &str) -> Vec<String> {
    ids_in(text)
        .into_iter()
        .map(|(r, id)| {
            assert_eq!(&text[r], id);
            id
        })
        .collect()
}

// ── Ids in Claude's answers ──────────────────────────────────────────────

/// What an answer cites: numbered ids, rows by their content (a sentence's end isn't the row's), one of
/// another session — and none inside a word, a path, a URL, or with no number.
#[test]
fn ids_in_an_answer_are_the_elements_it_names() {
    let text = "card#12 asks it; run#7, pane#3 and toast#2. See git:src/a.rs, `review:src/b.rs`, \
                files:src/ and s2/git:x.rs. Then list:src/c.rs:12.";
    assert_eq!(
        ids(text),
        [
            "card#12",
            "run#7",
            "pane#3",
            "toast#2",
            "git:src/a.rs",
            "review:src/b.rs",
            "files:src/",
            "s2/git:x.rs",
            "list:src/c.rs:12"
        ]
    );
    let not = "discard#12 a/card#3 card# card#12x git://host/x https://h/git:y fixgit:src/a.rs v1.card#2 git: \
               run#";
    assert!(ids(not).is_empty(), "{:?}", ids(not));
}

// ── Claude's level ───────────────────────────────────────────────────────

/// Claude's level is a strict subset: no unsent text (the chat's draft, a prompt's text), another session as
/// its name, what it's doing and how many cards — never its cards' titles, its folder, its runs.
#[test]
fn claudes_level_holds_back_unsent_text_and_other_sessions() {
    let mut ed = editor();
    chat(&mut ed, "a secret draft");
    ed.open_prompt(crate::editor::PromptKind::Command, "a secret prompt");
    let other = behind(&mut ed, "other", std::env::temp_dir().join("tarae-claude-level-other"));
    let mut parked = Offer::new(
        "Their own card".into(),
        "theirs",
        "",
        String::new(),
        What::ReviewEnd { session: 0, started: std::time::Instant::now() },
    );
    (ed.offer_seq, parked.id) = (ed.offer_seq + 1, ed.offer_seq + 1);
    let theirs = format!("card#{}", parked.id);
    let l = ed.sessions.iter_mut().find(|l| l.id == other).unwrap();
    l.parked.push(parked);
    // (its Claude's answer: the user's to read — `tarae state --chat`, #204 — never this Claude's)
    let mut c = crate::chat::Chat::new();
    c.msgs.push(crate::chat::Msg::new(crate::chat::Role::User, "q", ""));
    c.msgs.push(crate::chat::Msg::new(crate::chat::Role::Assistant, "their own answer", ""));
    l.chat = Some(c);
    l.at = crate::session::At::Chat;
    card(&mut ed, "End the review?");
    // (a toast on its card: how strong the frame drew it is the drivers', never Claude's)
    ed.set_warning("a toast on its card");
    frame(&mut ed);
    let strength = |s: &serde_json::Value| {
        s["toasts"].as_array().unwrap().iter().find_map(|t| t.get("strength").cloned())
    };
    assert!(
        strength(&ed.ui_state().json()).is_some(),
        "the full level has it: {}",
        ed.ui_state().json()["toasts"]
    );
    // (what drew the frames: a job's line — a shuttle's watch, say — and what was owed, a card's title: the
    // drivers', never Claude's)
    let owed = crate::editor::DrawnFor::Owed("card \"A secret owed card\"".into());
    ed.frames_for.push_back((7, vec![crate::editor::DrawnFor::Job(std::panic::Location::caller()), owed]));
    let drawn_for = ed.ui_state().json()["frames_for"].clone();
    assert!(drawn_for.to_string().contains("A secret owed card"), "the full level has it: {drawn_for}");
    let full = ed.ui_state().text();
    assert!(full.contains("a secret prompt"), "the full level has it — the filter matters:\n{full}");
    assert!(full.contains("Their own card") && full.contains(&theirs), "{full}");
    let turn = ed.chat_turn("other").unwrap().text();
    assert!(turn.contains("their own answer"), "the user reads it:\n{turn}");
    let s = ed.ui_state_claude();
    let text = s.text_claude(false);
    // (not even its card's id — the inbox lists every session's)
    for held in ["secret", "Their own card", "tarae-claude-level-other", theirs.as_str(), "their own answer"]
    {
        assert!(!text.contains(held), "{held:?} at Claude's level:\n{text}");
    }
    assert!(text.contains("other · waiting · 1 cards"), "the other session as counts:\n{text}");
    // (its `answer` too: blocked, never by what — the review of frictions36)
    let json = s.json().to_string();
    assert!(!json.contains("Their own card"), "{json}");
    assert!(s.json().get("frames_for").is_none(), "what drew the frames at Claude's level: {json}");
    assert_eq!(strength(&s.json()), None, "the card's strength at Claude's level: {}", s.json()["toasts"]);
    assert!(!json.contains("A secret owed card") && !json.contains("job src/"), "{json}");
    let theirs =
        s.json()["sessions"].as_array().unwrap().iter().find(|l| l["name"] == "other").cloned().unwrap();
    assert_eq!(theirs["answer"], "blocked", "{theirs}");
    assert!(full.contains("Their own card"), "the full level names it");
    assert!(text.contains("End the review?"), "its own session's card:\n{text}");
    assert!(!text.contains("frame"), "no pid, frame, input:\n{text}");
    assert!(s.panels.chat.as_ref().is_some_and(|c| c.draft.is_empty()));
}

/// The context's `ui:` line: nothing up but the code and the chat, no line; a card, a toast, a run under
/// way — each by id, and the ids are what went to Claude.
#[test]
fn the_context_line_names_whats_up_and_only_then() {
    let mut ed = editor();
    chat(&mut ed, "");
    frame(&mut ed);
    assert_eq!(ed.ui_state_claude().context_line(), None, "nothing's up");
    let id = card(&mut ed, "End the review?");
    ed.set_warning("the disk is full");
    frame(&mut ed);
    let (line, given) = ed.ui_state_claude().context_line().expect("a line");
    let toast = format!("toast#{}", ed.toasts.last().unwrap().id);
    assert!(line.starts_with("ui: "), "{line}");
    assert!(line.contains(&format!("card#{id} review-end \"End the review?")), "{line}");
    assert!(line.contains(&format!("{toast} warning \"the disk is full\"")), "{line}");
    assert_eq!(given, [format!("card#{id}"), toast]);
}

/// The `ui` tool: answered at once with Claude's level as text; what it names one by one is given this
/// turn (a card, the row in hand) — a panel's other rows on screen, named as a list, aren't.
#[test]
fn the_ui_tool_answers_at_once_and_gives_what_it_names() {
    let mut ed = editor();
    chat(&mut ed, "");
    let id = card(&mut ed, "End the review?");
    let files = vec!["a.rs".into(), "b.rs".into()];
    ed.session.sidebar = Some(crate::sidebar::Sidebar::with_files(ed.session.root.clone(), files));
    ed.session.at = crate::session::At::Side;
    ed.session.leave(crate::session::At::Side);
    frame(&mut ed);
    let msg = serde_json::json!({ "jsonrpc": "2.0", "id": 9, "method": "tools/call",
        "params": { "name": "ui", "arguments": {} } });
    let answer = crate::tools::mcp(&mut ed, 0, "r", &msg).expect("answered at once");
    let text = answer["result"]["content"][0]["text"].as_str().unwrap().to_string();
    assert!(text.contains(&format!("card#{id} tarae · review-end")), "{text}");
    assert!(text.contains("rows files: files:a.rs files:b.rs"), "the rows on screen by id:\n{text}");
    let given = &ed.session.chat.as_ref().unwrap().given;
    assert!(given.contains(&format!("card#{id}")) && given.contains(&"files:a.rs".into()), "{given:?}");
    assert!(!given.contains(&"files:b.rs".into()), "rows named as a list aren't given: {given:?}");
}

// ── Links: into sight, a flash, the gone ring ────────────────────────────

/// A card stacked behind flashes where it is — the keys still answer the front one; one that went says how
/// (no flash); one that never was says so (no flash).
#[test]
fn a_link_to_a_card_flashes_it_where_it_is_or_says_how_it_went() {
    let mut ed = editor();
    let back = card(&mut ed, "First");
    let front = card(&mut ed, "Second");
    frame(&mut ed);
    let up_front = ed.offers.first().map(|o| o.id);
    let behind = if up_front == Some(front) { back } else { front };
    ed.ui_jump(&format!("card#{behind}"));
    assert!(ed.ui_flashing(&format!("card#{behind}")), "it flashes");
    assert_eq!(ed.offers.first().map(|o| o.id), up_front, "the keys answer what they did");
    // Gone: answered, then followed
    ed.ui_flash = None;
    ed.card_how(behind, "answered n");
    ed.offers.retain(|o| o.id != behind);
    frame(&mut ed);
    ed.ui_jump(&format!("card#{behind}"));
    assert!(ed.ui_flash.is_none(), "nothing to flash");
    let said = ed.messages.last().map(|m| m.2.clone()).unwrap_or_default();
    assert!(
        said.contains("went") && said.contains("answered n") && said.contains("First")
            || said.contains("Second"),
        "{said}"
    );
    // Made up
    ed.ui_jump("card#9999");
    assert!(ed.ui_flash.is_none());
    assert_eq!(ed.messages.last().map(|m| m.2.as_str()), Some("card#9999 — no such element"));
    assert!(!ed.ui_known("card#9999") && ed.ui_known(&format!("card#{behind}")), "the gone ring knows it");
}

/// A card parked in another session: that session comes to the front with its cards, and it flashes.
#[test]
fn a_link_to_another_sessions_card_brings_that_session_forward() {
    let mut ed = editor();
    let other = behind(&mut ed, "other", std::env::temp_dir().join("tarae-claude-link-other"));
    let parked = Offer::new(
        "Run it?".into(),
        "theirs",
        "",
        String::new(),
        What::ReviewEnd { session: 0, started: std::time::Instant::now() },
    );
    let id = parked.id;
    ed.sessions.iter_mut().find(|l| l.id == other).unwrap().parked.push(parked);
    ed.ui_jump(&format!("card#{id}"));
    assert_eq!(ed.session.id, other, "its session in front");
    assert!(ed.offers.iter().any(|o| o.id == id) && ed.ui_flashing(&format!("card#{id}")));
}

/// A row's link: the side panel shows its tab without taking the keys (the chat keeps them), scrolled to the
/// row, which flashes; a row not in its tab now says so.
#[test]
fn a_link_to_a_row_shows_its_tab_without_the_keys() {
    let mut ed = editor();
    chat(&mut ed, "");
    let files: Vec<String> = (0..80).map(|i| format!("f{i:02}.rs")).collect();
    let sb = crate::sidebar::Sidebar::with_files(ed.session.root.clone(), files);
    ed.session.sidebar = Some(sb);
    frame(&mut ed);
    ed.ui_jump("files:f70.rs");
    assert!(ed.ui_flashing("files:f70.rs"));
    let text = frame(&mut ed);
    assert!(text.contains("f70.rs"), "scrolled into sight:\n{text}");
    assert!(ed.session.is_at(crate::session::At::Chat), "the keys stay the chat's");
    assert_eq!(ed.keys_to().region, crate::uistate::Region::Chat);
    ed.ui_flash = None;
    ed.ui_jump("files:nope.rs");
    assert!(ed.ui_flash.is_none());
    assert_eq!(ed.messages.last().map(|m| m.2.as_str()), Some("files:nope.rs isn't in the files tab now"));
}

/// The flash goes by itself: a frame is asked for when it's over (no key needed), and none after.
#[test]
fn a_flash_asks_for_one_frame_at_its_end() {
    let mut ed = editor();
    let id = card(&mut ed, "First");
    assert_eq!(ed.tick_wanted(), None, "nothing moves: no frame");
    ed.ui_jump(&format!("card#{id}"));
    let wake = ed.tick_wanted().expect("a frame to take it away");
    assert!(wake <= FLASH + Duration::from_millis(5) && wake > Duration::from_millis(900), "{wake:?}");
    // Over: no more frames asked for, and the frame that drew it gone lets it go
    ed.ui_flash = Some((format!("card#{id}"), Instant::now() - FLASH * 2));
    assert!(!ed.ui_flashing(&format!("card#{id}")));
    assert_eq!(ed.tick_wanted(), None, "a flash over asks for nothing");
    frame(&mut ed);
    assert!(ed.ui_flash.is_none() && !ed.ui_lit());
}

/// An id in Claude's answer is a link in the chat: a click on it flashes the element, the keys where they
/// were; `C-]` walks it with the answer's places, in the order written.
#[test]
fn an_id_in_the_answer_is_a_link() {
    let mut ed = editor();
    let id = card(&mut ed, "End the review?");
    chat(&mut ed, "");
    let answer = format!("It came up for src/a.rs:3 — card#{id} asks whether to end it.");
    let c = ed.session.chat.as_mut().unwrap();
    c.msgs.push(crate::chat::Msg::new(crate::chat::Role::User, "why?", ""));
    c.msgs.push(crate::chat::Msg::new(crate::chat::Role::Assistant, answer, ""));
    let hops = c.hops(&ed.session.root.clone());
    assert!(
        matches!(&hops[..], [crate::chat::Hop::Place(..), crate::chat::Hop::Element(e)] if *e == format!("card#{id}")),
        "{hops:?}"
    );
    frame(&mut ed);
    let spot = ed.screen.chat_ids.borrow().first().cloned().expect("the id is a spot in the chat");
    assert_eq!(spot.3, format!("card#{id}"));
    ed.handle_mouse(crate::event::Mouse {
        kind: crate::event::MouseKind::Down,
        alt: false,
        ctrl: false,
        x: spot.1 as u16,
        y: spot.0 as u16,
    });
    assert!(ed.ui_flashing(&format!("card#{id}")), "clicked: it flashes");
    // `C-]`: the newest first — the card
    ed.ui_flash = None;
    ed.handle_key(key("<C-]>"));
    assert!(ed.ui_flashing(&format!("card#{id}")));
}

// ── Given and cited ──────────────────────────────────────────────────────

/// Marks: a card given this turn carries a faint `◦` on its title row, one the answer cites a `•`; a toast
/// none, given or not; a new question takes them away.
#[test]
fn given_and_cited_marks_last_one_turn_and_never_sit_on_toasts() {
    let mut ed = editor();
    let id = card(&mut ed, "End the review?");
    ed.set_success("saved a.rs");
    chat(&mut ed, "");
    let toast = format!("toast#{}", ed.toasts.last().unwrap().id);
    let title =
        |text: &str| text.lines().find(|l| l.contains("End the review?")).unwrap_or_default().to_string();
    assert!(!title(&frame(&mut ed)).contains('◦'), "nothing given yet");
    ed.session.chat.as_mut().unwrap().given = vec![format!("card#{id}"), toast];
    let text = frame(&mut ed);
    assert!(title(&text).contains('◦'), "given:\n{text}");
    let saved = text.lines().find(|l| l.contains("saved a.rs")).unwrap_or_default();
    assert!(!saved.contains('◦'), "never on a toast:\n{text}");
    let c = ed.session.chat.as_mut().unwrap();
    c.msgs.push(crate::chat::Msg::new(crate::chat::Role::Assistant, format!("card#{id} asks it"), ""));
    cite(c);
    assert!(title(&frame(&mut ed)).contains('•'), "cited");
}

// ── Pointing ─────────────────────────────────────────────────────────────

/// `C-o` in the chat: labels on what's on screen, every key theirs; a label's letters put its id in the
/// chat as a chip; Backspace on the empty input takes it off; Esc stops pointing.
#[test]
fn pointing_puts_an_elements_id_in_the_chat() {
    let mut ed = editor();
    let id = card(&mut ed, "End the review?");
    chat(&mut ed, "");
    frame(&mut ed);
    ed.handle_key(key("<C-o>"));
    let p = ed.pointing.clone().expect("labels up");
    assert_eq!(ed.keys_to().region, crate::uistate::Region::Point);
    let at = p.targets.iter().position(|t| t.id == format!("card#{id}")).expect("the card has a label");
    let (a, b) = crate::labels::Labels::label(at);
    let text = frame(&mut ed);
    let row = &p.targets[at];
    let shown: String =
        text.lines().nth(row.at.1).unwrap_or_default().chars().skip(row.at.0).take(2).collect();
    assert_eq!(shown, format!("{a}{b}"), "its label where it is:\n{text}");
    // A key that's no label: said, still pointing
    ed.handle_key(key("<C-x>"));
    assert!(ed.pointing.is_some() && ed.status.is_some());
    ed.handle_key(key(&a.to_string()));
    ed.handle_key(key(&b.to_string()));
    assert!(ed.pointing.is_none());
    assert_eq!(ed.session.chat.as_ref().unwrap().pointed, [format!("card#{id}")]);
    assert!(frame(&mut ed).contains(&format!("card#{id} goes with it")), "the chip over the input");
    ed.handle_key(key("<backspace>"));
    assert!(ed.session.chat.as_ref().unwrap().pointed.is_empty(), "taken off");
    ed.handle_key(key("<C-o>"));
    ed.handle_key(key("<esc>"));
    assert!(ed.pointing.is_none() && ed.session.is_at(crate::session::At::Chat));
}

/// A framed card's label sits on its title's row — the frame's top line, where its name
/// is — at its left edge: the spot the state gives and the frame drawn agree.
#[test]
fn a_framed_cards_label_is_on_its_title_row() {
    let mut ed = editor();
    let id = card(&mut ed, "End the review?");
    chat(&mut ed, "");
    let before = frame(&mut ed);
    let title = before.lines().position(|l| l.contains("End the review?")).expect(&before);
    ed.handle_key(key("<C-o>"));
    let p = ed.pointing.clone().expect("labels up");
    let t = p.targets.iter().find(|t| t.id == format!("card#{id}")).expect("the card has a label");
    assert_eq!(t.at.1, title, "on the title's row:\n{before}");
    let row = before.lines().nth(title).unwrap();
    let x = row[..row.find("End the review?").unwrap()].chars().count();
    assert!(t.at.0 < x && x - t.at.0 <= 3, "at the card's left edge, before its name:\n{before}");
    let text = frame(&mut ed);
    let (a, b) = crate::labels::Labels::label(p.targets.iter().position(|u| u.id == t.id).unwrap());
    let row = text.lines().nth(title).unwrap_or_default();
    assert!(row.contains(&format!("{a}{b}")) && row.contains("End the review?"), "{text}");
}

/// A click while labels are up picks the element under it — here a side panel's row, by its content.
#[test]
fn pointing_by_a_click() {
    let mut ed = editor();
    chat(&mut ed, "");
    let sb = crate::sidebar::Sidebar::with_files(ed.session.root.clone(), vec!["a.rs".into(), "b.rs".into()]);
    ed.session.sidebar = Some(sb);
    frame(&mut ed);
    crate::uistate::point::start(&mut ed);
    let t = ed
        .pointing
        .as_ref()
        .unwrap()
        .targets
        .iter()
        .find(|t| t.id == "files:b.rs")
        .cloned()
        .expect("a label");
    ed.handle_mouse(crate::event::Mouse {
        kind: crate::event::MouseKind::Down,
        alt: false,
        ctrl: false,
        x: t.x as u16 + 3,
        y: t.y as u16,
    });
    assert_eq!(ed.session.chat.as_ref().unwrap().pointed, ["files:b.rs"]);
}

/// The message carries what was pointed at (`<pointed>`) and the `ui:` line; both are given to Claude for
/// the turn, the chips go.
#[test]
fn the_message_carries_what_was_pointed_at_and_the_ui_line() {
    let dir = std::env::temp_dir().join(format!("tarae-pointed-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let log = dir.join("stdin.log");
    let script = format!("read -r init; read -r line; printf '%s\\n' \"$line\" >> '{}'", log.display());
    let mut ed = editor();
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), script];
    let id = card(&mut ed, "End the review?");
    crate::chat::open(&mut ed);
    frame(&mut ed);
    let c = ed.session.chat.as_mut().unwrap();
    c.pointed = vec![format!("card#{id}")];
    c.input = "why did this come up?".into();
    crate::chat::send(&mut ed);
    let end = Instant::now() + Duration::from_secs(5);
    let sent = loop {
        let s = std::fs::read_to_string(&log).unwrap_or_default();
        if !s.is_empty() || Instant::now() > end {
            break s;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let msg: serde_json::Value = serde_json::from_str(sent.trim()).expect(&sent);
    let content = msg["message"]["content"].as_str().map(str::to_string).unwrap_or_else(|| msg.to_string());
    assert!(content.contains(&format!("ui: card#{id} review-end")), "{content}");
    assert!(
        content.contains(&format!("<pointed id=\"card#{id}\">card#{id} tarae · review-end")),
        "{content}"
    );
    let c = ed.session.chat.as_ref().unwrap();
    assert!(c.pointed.is_empty() && c.given == [format!("card#{id}")], "{:?}", c.given);
    std::fs::remove_dir_all(&dir).ok();
}
