use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::*;
use crate::config::Config;
use crate::offer::{Offer, What};

fn editor() -> Editor {
    let mut ed = Editor::new(Config::default());
    ed.docs[0].text = ropey::Rope::from_str("one\ntwo\nthree\n");
    ed.session.root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    // (a message sent in a scene starts Claude: a stand-in, never the real one)
    ed.config.llm.command = "sh".into();
    ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
    ed
}

fn key(s: &str) -> Key {
    match s.chars().count() {
        1 => ch(s.chars().next().unwrap()),
        _ => s.trim_matches(['<', '>']).parse().unwrap(),
    }
}

fn feed(ed: &mut Editor, keys: &[&str]) {
    for k in keys {
        ed.handle_key(key(k));
    }
}

/// A frame, as the main loop draws one after an event (`focus_sync` ran in it).
fn frame(ed: &mut Editor) {
    ed.focus_sync();
    crate::term::render(ed, &mut Vec::new(), 120, 40).unwrap();
}

/// A light card: `y` ends the code review (none here — nothing happens).
fn light_card(ed: &mut Editor) {
    let o = Offer::new(
        "End the review?".into(),
        "2 files not viewed.",
        "",
        String::new(),
        What::ReviewEnd { session: 0, started: std::time::Instant::now() },
    );
    ed.push_offer(o);
}

/// frictions25 R8: a card whose yes is held (high-risk changes no one has looked at) says why on its state line
/// — not only in its text — and the JSON has it; a card stepped aside says so, and who answers it.
#[test]
fn a_held_cards_line_says_why_its_yes_is_missing() {
    let mut ed = editor();
    let mut o = Offer::new(
        "End the review?".into(),
        "1 high-risk change not looked at.",
        "",
        String::new(),
        What::ReviewEnd { session: 0, started: std::time::Instant::now() },
    );
    o.held = Some(crate::codereview::held_said(1));
    ed.push_offer(o);
    frame(&mut ed);
    let s = ed.ui_state();
    let line = s.text().lines().find(|l| l.starts_with("card#")).unwrap().to_string();
    let keys: Vec<String> = s.json()["cards"][0]["answers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["key"].as_str().unwrap().to_string())
        .collect();
    assert!(!keys.contains(&"y".to_string()), "no yes among its answers: {keys:?}");
    assert!(
        line.contains(" · y held: 1 high-risk change not looked at — space v h on each, in a review"),
        "{line}"
    );
    assert!(!line.contains("aside"), "{line}");
    assert_eq!(s.json()["cards"][0]["held"], crate::codereview::held_said(1));
    // Stepped aside for what its `h` went to: said
    ed.offers[0].aside = true;
    frame(&mut ed);
    let line = ed.ui_state().text().lines().find(|l| l.starts_with("card#")).unwrap().to_string();
    assert!(line.contains(" · aside, C-q answers it"), "{line}");
}

/// A light card up here, and an older one waiting in a session behind (`C-q` goes to that one first —
/// `inbox.rs`).
fn older_card_behind(ed: &mut Editor) {
    let dir = std::env::temp_dir().join(format!("tarae-uistate-behind-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let id = ed.session_open(&dir).unwrap();
    let mut o = Offer::new(
        "Restart anyway?".into(),
        "It's the binary it was.",
        "",
        String::new(),
        What::ReviewEnd { session: 0, started: std::time::Instant::now() },
    );
    o.since = Instant::now() - Duration::from_secs(60);
    ed.session.parked.push(o);
    assert!(ed.session_switch(0) && ed.session.id != id);
    light_card(ed);
}

/// A key, then a light card right after it: it waits for the keys to go quiet (friction 60).
fn card_as_keys_came(ed: &mut Editor) {
    ed.keys_hold = true;
    feed(ed, &["j"]);
    light_card(ed);
}

/// The state says a card waits for the keys to go quiet — the keys line names it, the card says what it waits
/// for, both forms — and stops saying it once they've been quiet, when the keys are its; given them by `C-q`, it
/// has them and waits for nothing.
#[test]
fn the_state_says_a_card_waits_for_the_keys_to_go_quiet() {
    let mut ed = editor();
    card_as_keys_came(&mut ed);
    frame(&mut ed);
    // (keys just came: the frame drawn never eats into the window, however slow the machine)
    ed.last_input = Some(crate::clock::now());
    let s = ed.ui_state();
    let id = format!("card#{}", ed.offers[0].id);
    assert_eq!(s.json()["keys"]["region"], "code", "the keys are the code's meanwhile");
    assert_eq!(s.json()["keys"]["waiting"], id.as_str());
    assert_eq!(s.json()["cards"][0]["waits"], "keys quiet");
    let text = s.text();
    assert!(text.contains(&format!("· {id} takes them once the keys are quiet")), "{text}");
    assert!(text.contains(" · waits for the keys quiet, C-q answers it"), "{text}");
    // Given the keys: its own, waiting for nothing
    feed(&mut ed, &["<C-q>"]);
    let s = ed.ui_state();
    assert_eq!(s.json()["keys"]["to"], id.as_str());
    assert!(s.json()["keys"]["waiting"].is_null() && s.json()["cards"][0]["waits"].is_null());
    // Let go, then quiet: the keys are its, nothing waits
    feed(&mut ed, &["<esc>"]);
    assert_eq!(ed.ui_state().json()["keys"]["waiting"], id.as_str(), "let go as keys came: waits again");
    ed.last_input = ed.last_input.map(|t| t - crate::offer::KEYS_QUIET);
    let s = ed.ui_state();
    assert_eq!(s.json()["keys"]["to"], id.as_str(), "quiet: the card's");
    assert!(s.json()["keys"]["waiting"].is_null() && s.json()["cards"][0]["waits"].is_null());
    assert!(!s.text().contains("quiet"), "{}", s.text());
}

/// frictions29 #69, #70, #76: a weighty card's `y` before it was seen whole is said in its state entry
/// (`refused`), `v` first among its answers while `y` waits; a bring card is about the session it brings
/// (`session`), `shown_in` where it stands; a session's `answer` says where its last answer leaves it.
#[test]
fn the_state_says_a_refused_y_whose_card_it_is_and_where_an_answer_leaves_a_session() {
    let mut ed = editor();
    // (drawn cut: more lines than the screen holds)
    let q = (0..30).map(|i| format!("file {i}")).collect::<Vec<_>>().join("\n");
    let what = What::ChangeSet { whose: crate::refactor::Whose::Claude(999), ops: false };
    ed.push_offer(Offer::new("Claude changes 30 files".into(), &q, "", String::new(), what));
    ed.focus_sync();
    crate::term::render(&mut ed, &mut Vec::new(), 100, 16).unwrap();
    settle(&mut ed);
    let front = |ed: &Editor| ed.ui_state().json()["cards"][0].clone();
    assert_eq!(front(&ed)["answers"][0]["key"], "v", "v first while y waits: {}", front(&ed));
    assert!(front(&ed)["refused"].is_null());
    feed(&mut ed, &["y"]);
    assert_eq!(ed.offers.len(), 1, "not taken");
    assert_eq!(front(&ed)["refused"], "y — see it whole first (v)", "{}", front(&ed));
    ed.offers.clear();
    // A bring card of another session's, standing in front
    let back = crate::scratch::Scratch::new("state-bring").into_path();
    let id = ed.session_add(back.clone());
    let moved = crate::rebase::Moved {
        root: back.clone(),
        name: "back".into(),
        branch: "b".into(),
        base: "main".into(),
        moved: 0,
        ahead: 1,
    };
    ed.push_offer(Offer::new(
        "Bring back into main".into(),
        "1 commit.",
        "",
        String::new(),
        What::UpTo(Box::new(moved)),
    ));
    let card = front(&ed);
    assert_eq!(card["session"], session_id(id), "{card}");
    assert_eq!(card["shown_in"], session_id(ed.session.id), "{card}");
    // A session's answer: none without a chat; ready once there is one, blocked while a card of its waits
    ed.offers.clear();
    let answer = |ed: &Editor| ed.ui_state().json()["sessions"][0]["answer"].clone();
    assert!(answer(&ed).is_null());
    chat(&mut ed, "");
    assert_eq!(answer(&ed), "ready");
    let call = crate::tools::Call::test(7);
    ed.push_offer(Offer::new(
        "Claude asks to see the screen".into(),
        "?",
        "",
        String::new(),
        What::Screen { call },
    ));
    assert_eq!(answer(&ed), "blocked: Claude asks to see the screen");
    std::fs::remove_dir_all(&back).ok();
}

/// The review of frictions36: a session's CLAUDE.md changes for the main checkout's are on their card — a long
/// one drawn cut, and its `y` waits until they've been read whole (`v`): never written unseen into what every
/// session's Claude reads.
#[test]
fn a_long_carry_out_card_waits_to_be_read() {
    let mut ed = editor();
    let here: String = (0..40).map(|i| format!("rule {i}\n")).collect();
    let b = crate::instructions::Both {
        root: "/stand-in/feat".into(),
        rel: "CLAUDE.md".into(),
        base: Some(String::new()),
        here,
        main: String::new(),
        left: 0,
        out: Some("/stand-in/main".into()),
        ended: false,
    };
    ed.push_offer(b.card());
    ed.focus_sync();
    crate::term::render(&mut ed, &mut Vec::new(), 100, 24).unwrap();
    settle(&mut ed);
    feed(&mut ed, &["y"]);
    assert_eq!(ed.offers.len(), 1, "not carried");
    assert_eq!(ed.ui_state().json()["cards"][0]["refused"], "y — see it whole first (v)");
}

/// A weighty card whose answers do nothing here: a change set of a chat process that isn't there.
fn weighty_card(ed: &mut Editor) {
    let q = "Claude renames `a` to `b` in 3 files.\n\nOne review for the set.";
    ed.push_offer(Offer::new(
        "Claude changes 3 files".into(),
        q,
        "",
        String::new(),
        What::ChangeSet { whose: crate::refactor::Whose::Claude(999), ops: false },
    ));
}

/// The project's Claude settings card, up beside the chat it doesn't hold (claude runs held back meanwhile) —
/// about no settings read here, so its answers find nothing to answer.
fn settings_card(ed: &mut Editor) {
    let q = "Claude started with these held back — y gives them to it, the conversation going on.\n\n  \
             .claude/settings.json: apiKeyHelper = printf stand-in";
    let what = What::ClaudeSettings { hash: [7; 32] };
    ed.push_offer(Offer::new(crate::claudetrust::TITLE.into(), q, "", String::new(), what));
}

/// The front card in sight long enough for its answers to count.
fn settle(ed: &mut Editor) {
    if let Some(o) = ed.offers.first() {
        ed.offer_seen.set(Some((o.id, o.rev, Instant::now() - crate::offer::SETTLE * 2)));
    }
}

fn chat(ed: &mut Editor, draft: &str) {
    let mut c = crate::chat::Chat::new();
    c.input = draft.into();
    c.cursor = draft.len();
    ed.session.chat = Some(c);
    ed.session.at = crate::session::At::Chat;
}

/// Minimized while Claude works (follow on): the panel is a small card, its input not on screen.
fn chat_minimized(ed: &mut Editor) {
    chat(ed, "");
    let c = ed.session.chat.as_mut().unwrap();
    c.state = crate::chat::State::Thinking;
    c.follow.on = true;
    c.unfolded = false;
}

fn edit_review(ed: &mut Editor) {
    let id = ed.doc().id;
    crate::llm::review_with(ed, id, "x", vec![(0, 3, "one".into(), "uno".into())], None);
}

fn task_panel(ed: &mut Editor) {
    let task = crate::task::Task { name: "check".into(), cmd: "true".into(), cwd: ed.session.root.clone() };
    ed.session.task_run = Some(crate::task::Run::for_test(task));
}

/// Whether `test` (its name in this module) skips itself, saying why: it starts a terminal (`terminal`, or a
/// scene of `SCENES` that does), which needs a real pty — and inside macOS's sandbox a pty's other end doesn't
/// open (EPERM — `sandbox::TERMINALS`), as `terminal::tests` skip for.
fn no_pty_here(test: &str) -> bool {
    crate::sandbox::no_pty_here(&format!("uistate::tests::{test}"))
}

/// A terminal whose program sits there (`sleep`) — it hangs up when the editor goes.
fn terminal(ed: &mut Editor) {
    ed.terminal_run(vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()], Vec::new());
    let end = Instant::now() + Duration::from_secs(10);
    while ed.session.terminal.is_none() {
        assert!(Instant::now() < end, "the terminal started");
        if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(20)) {
            ed.handle_event(ev);
        }
    }
    let t = ed.session.terminal.as_mut().unwrap();
    t.visible = true;
    ed.session.at = crate::session::At::Program;
    ed.session.slot_front = Some(Slot::Terminal);
}

fn rail(ed: &mut Editor) {
    let root = ed.session.root.clone();
    let r = crate::codereview::CodeReview::for_test(crate::codereview::Base::Head, root, Vec::new());
    ed.session.code_review = Some(r);
    ed.session.at = crate::session::At::Rail;
}

type Scene = (&'static str, fn(&mut Editor));

/// The states the routing test walks — the handlers of `handle_key`'s chain one by one, and the ways they
/// meet (a card over the chat, a prompt over the terminal, a review under a side panel …).
const SCENES: &[Scene] = &[
    ("the code", |_| {}),
    ("insert mode", |ed| feed(ed, &["i"])),
    ("select mode", |ed| feed(ed, &["v"])),
    ("a prefix under way", |ed| feed(ed, &["g"])),
    ("the space menu", |ed| feed(ed, &[" "])),
    ("a count", |ed| feed(ed, &["3"])),
    ("f waits for its character", |ed| feed(ed, &["f"])),
    ("the command line", |ed| feed(ed, &[":"])),
    ("a picker", |ed| {
        ed.set_status("one");
        ed.messages_picker();
    }),
    ("hover text", |ed| ed.popup = Some(Vec::new())),
    // (bgreview: the reviewer reads in the background, no review open — its segment on the status line takes no key)
    ("the reviewer reading, no review open", |ed| {
        let rv = crate::reviewer::Reviewer::for_test(
            ed.session.root.clone(),
            crate::reviewer::State::Reading,
            Vec::new(),
            "",
        );
        ed.session.reviewer = Some(rv);
    }),
    ("the chat, empty", |ed| chat(ed, "")),
    ("the chat, a draft", |ed| chat(ed, "hello")),
    ("the chat minimized while Claude works", chat_minimized),
    ("the chat minimized, a draft in it", |ed| {
        chat_minimized(ed);
        let c = ed.session.chat.as_mut().unwrap();
        (c.input, c.cursor) = ("half a thought".into(), 14);
    }),
    ("the chat, a prompt opened over it", |ed| {
        chat(ed, "hello");
        ed.open_prompt(crate::editor::PromptKind::Command, "");
    }),
    ("a light card", light_card),
    ("a weighty card, settled", |ed| {
        weighty_card(ed);
        frame(ed);
        settle(ed);
    }),
    ("a weighty card, just up", weighty_card),
    ("a card over the chat's draft", |ed| {
        chat(ed, "hello");
        light_card(ed);
    }),
    ("a card given the keys (C-q)", |ed| {
        light_card(ed);
        feed(ed, &["<C-q>"]);
    }),
    ("a card given the keys over the chat", |ed| {
        chat(ed, "hello");
        light_card(ed);
        feed(ed, &["<C-q>"]);
    }),
    // (sandbox UX piece A: the settings card comes up after the chat opened, its input empty — the chat keeps
    // the keys, a weighty card's or not; `C-q` gives them to it)
    ("the chat, empty, the Claude settings card beside it", |ed| {
        chat(ed, "");
        settings_card(ed);
        frame(ed);
        settle(ed);
    }),
    // (frictions26: a run card coming mid-turn leaves the empty input the keys — the `a` of a message typed next
    // was a grant; `C-q` hands them over)
    ("the chat, empty, a weighty card come mid-turn", |ed| {
        chat(ed, "");
        weighty_card(ed);
        frame(ed);
        settle(ed);
    }),
    ("a weighty card given the keys from the empty chat (C-q)", |ed| {
        chat(ed, "");
        weighty_card(ed);
        frame(ed);
        settle(ed);
        feed(ed, &["<C-q>"]);
    }),
    ("the Claude settings card given the keys from the chat (C-q)", |ed| {
        chat(ed, "");
        settings_card(ed);
        frame(ed);
        settle(ed);
        feed(ed, &["<C-q>"]);
    }),
    // (a held card's `h` went to what it waits on: its letters are the code's, `C-q` answers it)
    ("a card stepped aside for its h", |ed| {
        light_card(ed);
        ed.offers[0].aside = true;
    }),
    ("a card stepped aside, given the keys (C-q)", |ed| {
        light_card(ed);
        ed.offers[0].aside = true;
        feed(ed, &["<C-q>"]);
    }),
    // (it came right after a key: it waits for the keys to go quiet — friction 60)
    ("a card that came as keys came in", card_as_keys_came),
    ("a card that came as keys came in, given the keys (C-q)", |ed| {
        card_as_keys_came(ed);
        feed(ed, &["<C-q>"]);
    }),
    ("a card in insert mode", |ed| {
        light_card(ed);
        feed(ed, &["i"]);
    }),
    ("a card under the command line", |ed| {
        light_card(ed);
        feed(ed, &[":"]);
    }),
    ("two cards stacked", |ed| {
        light_card(ed);
        weighty_card(ed);
    }),
    ("a card up, an older one in a session behind", older_card_behind),
    ("nothing waits, a turn ended in a session behind", |ed| {
        let dir = std::env::temp_dir().join(format!("tarae-uistate-done-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let id = ed.session_open(&dir).unwrap();
        assert!(ed.session_switch(0) && ed.session.id != id);
        let l = ed.sessions.iter_mut().find(|l| l.id == id).unwrap();
        l.turn_end = Some((Instant::now(), Ok(())));
        assert_eq!(ed.next_up(), crate::inbox::Next::Done(id));
    }),
    ("a card up, an older one behind, the chat's draft", |ed| {
        older_card_behind(ed);
        chat(ed, "hello");
    }),
    ("the side panel", |ed| ed.sidebar_show(crate::sidebar::Mode::Outline)),
    ("the side panel in insert mode", |ed| {
        ed.sidebar_show(crate::sidebar::Mode::Outline);
        ed.mode = Mode::Insert;
    }),
    ("an edit's review", edit_review),
    ("an edit's review, the chat back", |ed| {
        edit_review(ed);
        chat(ed, "");
        ed.session.review.as_mut().unwrap().from_chat = true;
    }),
    ("an edit's review, left with Esc", |ed| {
        edit_review(ed);
        feed(ed, &["<esc>"]);
    }),
    // (Esc stops it — `llm::esc_stops`)
    ("a draft being written", |ed| {
        let id = ed.doc().id;
        let hunk = crate::llm::Hunk {
            from: 0,
            to: 0,
            old: String::new(),
            new: String::new(),
            decision: crate::llm::Decision::Undecided,
            conflict: false,
        };
        ed.session.review = Some(crate::llm::Review::new(1, id, "a commit message", vec![hunk]));
    }),
    ("a draft being written, the chat's draft", |ed| {
        chat(ed, "hello");
        let id = ed.doc().id;
        ed.session.review = Some(crate::llm::Review::new(1, id, "a commit message", Vec::new()));
    }),
    ("an edit's review under the side panel", |ed| {
        edit_review(ed);
        ed.sidebar_show(crate::sidebar::Mode::Outline);
    }),
    ("the review's rail", rail),
    ("the review's rail under the side panel", |ed| {
        rail(ed);
        ed.sidebar_show(crate::sidebar::Mode::Outline);
    }),
    (":procs", |ed| ed.procs_toggle()),
    (":procs behind another tab", |ed| {
        ed.procs_toggle();
        task_panel(ed);
        ed.session.slot_front = Some(Slot::Task);
    }),
    ("the task panel in view mode, by C-w j", |ed| {
        task_panel(ed);
        ed.bottom_take_keys();
    }),
    ("the task panel in view mode, a row selected", |ed| {
        task_panel(ed);
        ed.bottom_take_keys();
        feed(ed, &["x"]);
    }),
    ("the task panel in view mode, its search prompt", |ed| {
        task_panel(ed);
        ed.bottom_take_keys();
        feed(ed, &["/"]);
    }),
    ("the task panel in view mode, space under way", |ed| {
        task_panel(ed);
        ed.bottom_take_keys();
        feed(ed, &[" "]);
    }),
    // (`]` gone on to the editor: `]x`/`]q` are its, the rest the view says)
    ("the task panel in view mode, ] under way", |ed| {
        task_panel(ed);
        ed.bottom_take_keys();
        feed(ed, &["]"]);
    }),
    ("a preflight in view mode", |ed| {
        task_panel(ed);
        let steps = [crate::preflight::Step {
            name: "fmt".into(),
            cmd: String::new(),
            note: None,
            from: String::new(),
            full: false,
        }];
        let p = crate::preflight::Preflight::new(0, &steps, crate::preflight::Set::Full, "k".into());
        ed.session.preflight = Some(p);
        ed.bottom_take_keys();
    }),
    ("the test panel in view mode", |ed| {
        ed.session.test_run = Some(crate::testing::TestRun::finished("t", Vec::new(), vec!["ok".into()]));
        ed.bottom_take_keys();
    }),
    // (panes P2b: the tests' group — its list a pane of its own, beside the detail)
    ("the tests' list", |ed| {
        let case = crate::test_results::Case {
            name: "adds".into(),
            status: crate::test_results::Status::Passed,
            secs: None,
            message: Vec::new(),
            at: None,
        };
        let run = crate::testing::TestRun::finished("t", vec![case.clone(), case], vec!["ok".into()]);
        ed.session.test_run = Some(run);
        frame(ed);
        ed.tests_list_focus();
    }),
    ("a card over the task panel's view mode", |ed| {
        task_panel(ed);
        ed.bottom_take_keys();
        light_card(ed);
    }),
    ("the terminal's program", terminal),
    // (panes P2b: `C-w` is tarae's inside the program — its next key the window table's, `C-w` again the program's)
    ("the terminal's program, the window prefix taken from it", |ed| {
        terminal(ed);
        feed(ed, &["<C-w>"]);
    }),
    ("two code panes side by side, one sized", |ed| {
        ed.split_view(crate::split::Dir::Vertical);
        frame(ed);
        feed(ed, &["<C-w>", ">"]);
    }),
    ("the terminal in view mode, by C-w j", |ed| {
        terminal(ed);
        ed.bottom_take_keys();
    }),
    ("the terminal in view mode, a row selected", |ed| {
        terminal(ed);
        feed(ed, &["<C-\\>", "x"]);
    }),
    ("the terminal's program, a prompt over it", |ed| {
        terminal(ed);
        ed.open_prompt(crate::editor::PromptKind::Command, "");
    }),
    ("a card over the terminal's program", |ed| {
        terminal(ed);
        light_card(ed);
    }),
    ("pointing at something on screen, from the chat", |ed| {
        chat(ed, "a draft");
        light_card(ed);
        frame(ed);
        feed(ed, &["<C-o>"]);
    }),
    ("the chat's view mode", chat_view),
    ("the chat's view mode, a count under way", |ed| {
        chat_view(ed);
        feed(ed, &["3"]);
    }),
    ("the chat's view mode, a row selected", |ed| {
        chat_view(ed);
        feed(ed, &["x"]);
    }),
    ("the chat's view mode, a sequence under way", |ed| {
        chat_view(ed);
        feed(ed, &["g"]);
    }),
    ("the chat's view mode, its search prompt", |ed| {
        chat_view(ed);
        feed(ed, &["/"]);
    }),
    ("a card over the chat's view mode", |ed| {
        chat_view(ed);
        light_card(ed);
    }),
];

/// The chat's view mode (`esc` from its input, a draft in it) on a question and its answer.
fn chat_view(ed: &mut Editor) {
    use crate::chat::{Msg, Role};
    chat(ed, "a draft");
    let c = ed.session.chat.as_mut().unwrap();
    c.msgs.push(Msg::new(Role::User, "a question", ""));
    c.msgs.push(Msg::new(Role::Assistant, "An answer, in a few words.", ""));
    frame(ed);
    feed(ed, &["<esc>"]);
    frame(ed);
}

/// Keys a driver sends: answers, commands, text, the prefixes, `C-q`, Esc. (No Enter: in the chat it would
/// start a claude.)
const PROBES: &[&str] =
    &["y", "n", "a", "A", "v", "r", "j", "x", "i", "[", "3", " ", ":", "<C-q>", "<C-w>", "<esc>", "<C-d>"];

/// The keys that move, as every surface takes them (uxaudit D7, Z2, H1): a row, half the rows, a page, the ends,
/// the terminal's past, a picker's split, preview and keep-open keys — the routing test sends them too.
const MOVES: &[&str] =
    &["<C-e>", "<C-y>", "<C-f>", "<pagedown>", "<home>", "<S-pageup>", "<C-v>", "<C-t>", "<A-ret>"];

/// Every scene × every probe key and move: the handler that took the key (`took`) is the one `keys_to` said.
/// A model that's off in any scene shows here as `scene · key: model says X, handle_key took Y`.
fn mismatches(scenes: &[Scene], model: impl Fn(&Editor) -> KeysTo) -> Vec<String> {
    let mut off = Vec::new();
    for (name, setup) in scenes {
        for probe in PROBES.iter().chain(MOVES) {
            let mut ed = editor();
            setup(&mut ed);
            frame(&mut ed);
            // (a scene that settles its card did so after its frame — again, after this one)
            if name.contains("settled") {
                settle(&mut ed);
            }
            ed.focus_sync();
            let said = model(&ed).route(key(probe)).handler();
            TOOK.set(None);
            ed.handle_key(key(probe));
            let took = TOOK.get();
            if took != Some(said) {
                off.push(format!("{name} · {probe:?}: keys_to says {said:?}, handle_key took {took:?}"));
            }
        }
    }
    off
}

/// The routing test: `keys_to` is `handle_key`'s chain — in every state, for every probe key.
#[test]
fn keys_to_is_where_handle_key_sends_every_key() {
    if no_pty_here("keys_to_is_where_handle_key_sends_every_key") {
        return;
    }
    let off = mismatches(SCENES, Editor::keys_to);
    let all = SCENES.len() * (PROBES.len() + MOVES.len());
    assert!(off.is_empty(), "{} of {all}:\n{}", off.len(), off.join("\n"));
}

/// The routing test's second entrance (`shuttle.rs`): a shuttle isn't a handler of the chain — it comes in
/// before it. Every scene × every probe key sent as a shuttle's step: written for another place, it's refused and
/// nothing takes it; written for where `keys_to` says the keys go, the handler that takes it is the one that
/// takes the person's key — or, at a card, it's refused as an answer that stays the person's (`weighty`,
/// `never`), nothing taking it.
fn shuttle_mismatches(scenes: &[Scene]) -> Vec<String> {
    use crate::shuttle::{Input, Level, Step};
    let mut off = Vec::new();
    let ready = |setup: fn(&mut Editor), name: &str| {
        let mut ed = editor();
        setup(&mut ed);
        frame(&mut ed);
        if name.contains("settled") {
            settle(&mut ed);
        }
        ed
    };
    for (name, setup) in scenes {
        for probe in PROBES {
            let mut ed = ready(*setup, name);
            ed.focus_sync();
            TOOK.set(None);
            ed.handle_key(key(probe));
            let person = TOOK.get();
            let mut ed = ready(*setup, name);
            let id = crate::shuttle::connect_for_test(&mut ed, "t", Level::Weighty);
            ed.focus_sync();
            let to = ed.keys_to().to;
            let step = |to: &str| Step {
                id,
                input: Input::Keys(vec![key(probe)], probe.to_string()),
                expect: serde_json::json!({ "keys": { "to": to } }),
            };
            let elsewhere = if to == "chat" { "prompt" } else { "chat" };
            TOOK.set(None);
            let (said, _) = crate::shuttle::run(&mut ed, step(elsewhere));
            if said["refused"] != "expect" || TOOK.get().is_some() {
                off.push(format!("{name} · {probe:?} for {elsewhere}: went ({said})"));
                continue;
            }
            TOOK.set(None);
            let (said, _) = crate::shuttle::run(&mut ed, step(&to));
            let took = TOOK.get();
            let yours = matches!(said["refused"].as_str(), Some("weighty" | "never"));
            if yours && person == Some(crate::uistate::Handler::Card) && took.is_none() {
                continue;
            }
            if took != person {
                off.push(format!(
                    "{name} · {probe:?}: the person's took {person:?}, the shuttle's {took:?} ({said})"
                ));
            }
        }
    }
    off
}

/// Sequences the matrix sends: a prefix, the space menu, a count, insert mode, select mode.
const SEQUENCES: &[&[&str]] = &[&["g", "g"], &[" ", "v"], &["3", "j"], &["i", "x"], &["v", "j"]];

/// Every scene × every sequence: typed by a person, a key an event; sent by a shuttle as one step — the same
/// handler takes the last key, and the keys and the cursor end the same. (A sequence whose place moves partway a
/// shuttle stops on purpose — `moved`; at a card, an answer that stays the person's is refused: both skipped.)
fn shuttle_sequence_mismatches(scenes: &[Scene]) -> Vec<String> {
    use crate::shuttle::{Input, Level, Step};
    let ready = |setup: fn(&mut Editor), name: &str| {
        let mut ed = editor();
        setup(&mut ed);
        frame(&mut ed);
        if name.contains("settled") {
            settle(&mut ed);
        }
        ed.focus_sync();
        ed
    };
    let end = |ed: &Editor| {
        let k = ed.keys_to();
        let at = ed.doc().selection().primary();
        (k.to, k.region, k.takes, k.pending, k.held, ed.mode, at.anchor, at.head, ed.doc().text.to_string())
    };
    let mut off = Vec::new();
    for (name, setup) in scenes {
        for seq in SEQUENCES {
            let mut ed = ready(*setup, name);
            let to = ed.keys_to().to;
            let mut stays = true;
            for (i, k) in seq.iter().enumerate() {
                stays &= i == 0 || ed.keys_to().to == to;
                TOOK.set(None);
                ed.handle_event(crate::event::Event::Key(key(k)));
            }
            let person = (TOOK.get(), end(&ed));
            if !stays {
                continue;
            }
            let mut ed = ready(*setup, name);
            let id = crate::shuttle::connect_for_test(&mut ed, "t", Level::Weighty);
            ed.focus_sync();
            let keys = seq.iter().map(|k| key(k)).collect();
            let step = Step {
                id,
                input: Input::Keys(keys, seq.join(" ")),
                expect: serde_json::json!({ "keys": { "to": to } }),
            };
            TOOK.set(None);
            let (said, _) = crate::shuttle::run(&mut ed, step);
            if matches!(said["refused"].as_str(), Some("weighty" | "never")) {
                continue;
            }
            let shuttle = (TOOK.get(), end(&ed));
            if shuttle != person {
                off.push(format!(
                    "{name} · {seq:?}: the person's {person:?}, the shuttle's {shuttle:?} ({said})"
                ));
            }
        }
    }
    off
}

#[test]
fn a_shuttles_sequence_ends_where_a_persons_keys_would() {
    if no_pty_here("a_shuttles_sequence_ends_where_a_persons_keys_would") {
        return;
    }
    let off = shuttle_sequence_mismatches(SCENES);
    assert!(off.is_empty(), "{} of {}:\n{}", off.len(), SCENES.len() * SEQUENCES.len(), off.join("\n"));
}

#[test]
fn a_shuttles_step_lands_where_a_persons_key_would() {
    if no_pty_here("a_shuttles_step_lands_where_a_persons_key_would") {
        return;
    }
    let off = shuttle_mismatches(SCENES);
    assert!(off.is_empty(), "{} of {}:\n{}", off.len(), SCENES.len() * PROBES.len(), off.join("\n"));
}

/// …and it would see a model that's off. In every scene, each case broken once: what has the keys named
/// as another place; and where it leaves keys to the editor (`space`, `:`, `C-w`), it taking them all —
/// either disagrees with `handle_key` for some probe.
#[test]
fn the_routing_test_sees_each_case_broken() {
    if no_pty_here("the_routing_test_sees_each_case_broken") {
        return;
    }
    let main = |k: &KeysTo| k.chain.iter().position(|l| !l.shortcut).unwrap();
    let elsewhere = |ed: &Editor| {
        let mut k = ed.keys_to();
        let m = main(&k);
        k.chain[m].region = if k.chain[m].region == Region::Code { Region::Prompt } else { Region::Code };
        k
    };
    let greedy = |ed: &Editor| {
        let mut k = ed.keys_to();
        let m = main(&k);
        k.chain[m].filter = Filter::All;
        k
    };
    let mut leaves = 0;
    for scene @ (name, setup) in SCENES {
        let one = std::slice::from_ref(scene);
        assert!(
            !mismatches(one, elsewhere).is_empty(),
            "{name}: what has the keys named wrong, and the test passes"
        );
        let mut ed = editor();
        setup(&mut ed);
        frame(&mut ed);
        let k = ed.keys_to();
        if matches!(k.chain[main(&k)].filter, Filter::Except(_)) {
            leaves += 1;
            assert!(!mismatches(one, greedy).is_empty(), "{name}: taking every key, and the test passes");
        }
    }
    assert!(leaves >= 5, "scenes whose place leaves keys to the editor: {leaves}");
    // The shuttle's entrance, its check skipped: a step written for another place lands — and the test says so
    crate::shuttle::SKIP_EXPECT.set(true);
    let off = shuttle_mismatches(&SCENES[..2]);
    crate::shuttle::SKIP_EXPECT.set(false);
    assert!(!off.is_empty(), "the shuttle's check skipped, and the test passes");
}

/// The frame as text, as the user sees it.
fn shown(ed: &mut Editor) -> String {
    frame(ed);
    crate::screen::text(ed.last_frame.as_ref().unwrap().0.as_slice(), 120, 40).unwrap()
}

/// Every scene × every probe key where the key changed nothing on screen and nothing was said — a key
/// refused (or lost) in silence.
fn silent() -> Vec<String> {
    let mut out = Vec::new();
    for (name, setup) in SCENES {
        for probe in PROBES {
            let mut ed = editor();
            setup(&mut ed);
            if name.contains("settled") {
                frame(&mut ed);
                settle(&mut ed);
            }
            let (before, said) = (shown(&mut ed), ed.said);
            if name.contains("settled") {
                settle(&mut ed);
            }
            ed.handle_key(key(probe));
            if shown(&mut ed) == before && ed.said == said {
                out.push(format!("{name} · {probe:?}"));
            }
        }
    }
    out
}

/// No silent refusal (user decision 2026-10-04): in every scene of the routing matrix, a key either changes
/// what's on screen or is said. What's left is what does nothing on purpose — Esc with nothing to leave,
/// `C-w` with nothing to delete, `j`/`C-d` at a list's end, a space at the end of an input (there, but not
/// in sight as text) — and the terminal's program, whose echo comes later from its reader. Found and fixed
/// with this: the side panel, the review's list, `:procs`, the bottom slot and an edit's review dropped
/// keys they had no use for; `f`/`r` waited for a character unsaid; a card swallowed `y` as it came up;
/// `C-` keys in the chat and the command line went nowhere; `:procs`' `s`/`r` with no row.
#[test]
fn no_key_is_refused_in_silence() {
    if no_pty_here("no_key_is_refused_in_silence") {
        return;
    }
    let quiet = |k: &str, name: &str| match k {
        "<esc>" | "<C-w>" => true,
        // (view mode's copy: said once the clipboard has it — its job's toast)
        "y" => name.contains("view mode") && !name.contains("prompt"),
        // (`x` again on the conversation's last row: no row below to take — `j` at a list's end)
        "x" => name.contains("a row selected"),
        "j" | "<C-d>" => [
            "side panel",
            "rail",
            ":procs",
            "panel",
            "tarae's keys",
            "chat",
            "picker",
            "hover",
            "command line",
            "review",
            "view mode",
        ]
        .iter()
        .any(|p| name.contains(p)),
        " " => ["draft", "a prompt opened over it", "picker"].iter().any(|p| name.contains(p)),
        _ => false,
    };
    let off: Vec<String> = silent()
        .into_iter()
        // (the program's: its echo comes from its reader thread, after this frame)
        .filter(|l| !l.contains("the terminal's program ·"))
        .filter(|l| {
            let (name, k) = l.rsplit_once(" · ").unwrap();
            !quiet(k.trim_matches('"'), name)
        })
        .collect();
    assert!(off.is_empty(), "keys refused in silence:\n{}", off.join("\n"));
}

/// In every scene the keys go somewhere on screen — the minimized chat too, since its first key brings it
/// back (slip 3 was the one place they didn't).
#[test]
fn the_keys_always_go_somewhere_on_screen() {
    if no_pty_here("the_keys_always_go_somewhere_on_screen") {
        return;
    }
    let mut hidden = Vec::new();
    for (name, setup) in SCENES {
        let mut ed = editor();
        setup(&mut ed);
        frame(&mut ed);
        if !ed.keys_to().drawn {
            hidden.push(*name);
        }
    }
    assert!(hidden.is_empty(), "keys to what isn't on screen: {hidden:?}");
}

/// Found by the routing test, where the status line named one place and the keys went to another: a side
/// panel focused in insert mode (OUTLINE; the keys typed into the code), and a prompt over the terminal's
/// program (TERMINAL; the prompt took them).
#[test]
fn the_status_line_names_where_the_keys_go() {
    use crate::chrome::Keys;
    if no_pty_here("the_status_line_names_where_the_keys_go") {
        return;
    }
    let mut ed = editor();
    ed.sidebar_show(crate::sidebar::Mode::Outline);
    ed.mode = Mode::Insert;
    frame(&mut ed);
    assert_eq!(ed.keys_at(), Keys::Code, "insert mode: the keys type in the code");
    let mut ed = editor();
    terminal(&mut ed);
    ed.open_prompt(crate::editor::PromptKind::Command, "");
    frame(&mut ed);
    assert_eq!(ed.keys_to().region, Region::Prompt);
    assert_eq!(ed.keys_at(), Keys::Code, "the prompt's — not TERMINAL");
}

// ── The four slips of 2026-10-04 ─────────────────────────────────────────

/// Slip 1: a `y` typed into the chat input, a card up. The state says the keys are the chat's, as text —
/// the card's `y` isn't — and that the card waits (`C-q` reaches it).
#[test]
fn slip_1_a_y_meant_for_a_card_goes_to_the_chat_input() {
    let mut ed = editor();
    chat(&mut ed, "hello");
    light_card(&mut ed);
    frame(&mut ed);
    let k = ed.keys_to();
    assert_eq!((k.region, k.takes, k.to.as_str()), (Region::Chat, Takes::Text, "chat"));
    assert_eq!(k.route(key("y")), Region::Chat, "y is text");
    assert!(matches!(k.route(key("<C-q>")), Region::Card(_)), "C-q reaches the card");
    let s = ed.ui_state();
    assert_eq!(s.cards.len(), 1);
    assert!(s.text().contains("keys → chat (text)"), "{}", s.text());
    // And once `C-q` gave it the keys: the card's, held, the chat under it
    feed(&mut ed, &["<C-q>"]);
    frame(&mut ed);
    let k = ed.keys_to();
    assert!(matches!(k.region, Region::Card(_)) && k.held && k.takes == Takes::Answers);
    assert_eq!(k.under, Some(Region::Chat));
    assert_eq!(k.answers, vec!['y', 'n']);
    assert!(ed.ui_state().text().contains("under it: chat"));
}

/// Slip 2: a message meant for tarae typed into the terminal's shell. The state tells the program having
/// the keys from its view mode — and so does the status line (`● view · terminal  i type  esc code`).
#[test]
fn slip_2_the_terminal_program_has_the_keys_or_tarae_has_them_on_its_panel() {
    if no_pty_here("slip_2_the_terminal_program_has_the_keys_or_tarae_has_them_on_its_panel") {
        return;
    }
    let mut ed = editor();
    terminal(&mut ed);
    frame(&mut ed);
    let k = ed.keys_to();
    assert_eq!((k.region.name(), k.takes, k.drawn), ("terminal-program".to_string(), Takes::Program, true));
    assert_eq!(ed.keys_at(), crate::chrome::Keys::Program);
    assert_eq!(ed.keys_at().hint(), None);
    feed(&mut ed, &["<C-\\>"]);
    frame(&mut ed);
    let k = ed.keys_to();
    assert_eq!((k.region.name(), k.takes), ("view:terminal".to_string(), Takes::Commands));
    let shown = crate::screen::text(ed.last_frame.as_ref().unwrap().0.as_slice(), 120, 40).unwrap();
    let status = shown.lines().find(|l| l.contains("● view · terminal ")).unwrap_or_default();
    assert!(status.contains("● view · terminal   i type  esc code"), "{shown}");
}

/// Slip 3: ` W` typed into a chat input that wasn't on screen — minimized while Claude worked. Now the
/// first key that would type brings the panel back instead, isn't typed, and says so; the state said so
/// before it (`takes: unfold`). The chat's own keys (`C-c` stop, Esc) still act while it's minimized.
#[test]
fn slip_3_the_minimized_chat_comes_back_before_anything_is_typed() {
    let mut ed = editor();
    chat_minimized(&mut ed);
    frame(&mut ed);
    assert!(ed.screen.chat_mini.get().is_some(), "minimized: its card is drawn");
    let k = ed.keys_to();
    assert_eq!((k.region, k.takes, k.to.as_str(), k.drawn), (Region::Unfold, Takes::Unfold, "chat", true));
    assert_eq!(k.route(key(" ")), Region::Unfold, "space brings it back — no menu, no text");
    assert_eq!(k.route(key("<C-c>")), Region::Chat, "C-c still stops Claude");
    assert!(ed.ui_state().text().contains("keys → chat (unfold)"));
    let said = ed.said;
    feed(&mut ed, &[" ", "W"]);
    let c = ed.session.chat.as_ref().unwrap();
    assert!(!c.compact() && ed.session.is_at(crate::session::At::Chat), "back, with the keys");
    assert_eq!(c.input, "W", "the first key brought it back; the next typed");
    assert!(ed.said > said, "said");
    frame(&mut ed);
    assert!(ed.screen.chat_w > 0 && ed.keys_to().drawn && ed.keys_to().takes == Takes::Text);
}

/// #151: a follow-up typed into the minimized chat (follow mode, Claude working) keeps its first letter —
/// it brings the panel back and is typed there, in sight; Enter only brings it back (nothing is sent unseen).
#[test]
fn the_first_letter_into_the_minimized_chat_is_typed() {
    let mut ed = editor();
    chat_minimized(&mut ed);
    frame(&mut ed);
    feed(&mut ed, &["h", "i"]);
    let c = ed.session.chat.as_ref().unwrap();
    assert!(!c.compact() && ed.session.is_at(crate::session::At::Chat), "back, with the keys");
    assert_eq!(c.input, "hi", "the first letter isn't swallowed");
    frame(&mut ed);
    assert!(ed.screen.chat_w > 0, "in sight");
    // Minimized again: Enter brings it back, sends nothing
    chat_minimized(&mut ed);
    let c = ed.session.chat.as_mut().unwrap();
    (c.input, c.cursor) = ("hi".into(), 2);
    let queued = c.queued.clone();
    feed(&mut ed, &["<ret>"]);
    let c = ed.session.chat.as_ref().unwrap();
    assert!(!c.compact() && c.queued == queued && c.input == "hi", "not sent: {:?}", c.queued);
}

/// Slip 4: a card nobody noticed for minutes. Every card is in the state — up front, stacked behind it,
/// parked in a session behind — with how long it has waited and how long it has been in sight.
#[test]
fn slip_4_every_card_is_in_the_state_wherever_it_waits() {
    let mut ed = editor();
    light_card(&mut ed);
    weighty_card(&mut ed);
    let id = ed.session_add(std::env::temp_dir().join("tarae-uistate-behind"));
    let o =
        Offer::new("Review at the end?".into(), "Edits land unseen.", "", String::new(), What::ReviewAtEnd);
    park(&mut ed, id, o);
    frame(&mut ed);
    let s = ed.ui_state();
    let ups: Vec<(&str, &str)> = s.cards.iter().map(|c| (c.up, c.session.as_str())).collect();
    let behind = format!("s{id}");
    assert_eq!(ups, vec![("front", "s0"), ("stacked", "s0"), ("parked", behind.as_str())]);
    assert!(s.cards[0].in_sight_ms.is_some() && s.cards[1].in_sight_ms.is_none());
    let row = s.sessions.iter().find(|x| x.id == behind).unwrap();
    assert_eq!(row.cards, 1, "the session behind counts its card");
    let text = s.text();
    assert!(text.contains(&format!("in {behind} · parked")), "{text}");
}

// ── What it found ────────────────────────────────────────────────────────

/// Found by pulling the answers out (`Offer::answers`): closing a buffer under review asked `y Install ·
/// n Not now` — the download card's row — while its text said `y` declines Claude's change.
#[test]
fn the_close_reviewed_card_says_what_its_answers_do() {
    let o = Offer::new(
        "Close a.rs?".into(),
        "A change to a.rs waits for your answer.",
        "",
        String::new(),
        What::CloseReviewed { doc: 0, force: false },
    );
    assert_eq!(o.answers(false), vec![('y', "Decline and close"), ('n', "Keep both")]);
}

/// Found by the routing test: an edit's review under a focused side panel has the keys — the status line
/// said OUTLINE (the panel's) while `j` scrolled the review and `y` accepted Claude's change.
#[test]
fn an_edit_review_under_a_side_panel_is_where_the_keys_go() {
    let mut ed = editor();
    edit_review(&mut ed);
    ed.sidebar_show(crate::sidebar::Mode::Outline);
    frame(&mut ed);
    assert_eq!(ed.keys_to().region, Region::EditReview);
    assert_eq!(ed.keys_at(), crate::chrome::Keys::Change);
}

/// On a screen too narrow for the chat beside the code, the chat takes it all — while it has the keys. Once
/// they leave it (a card answered, Esc, an edit's review), the code they go to has the screen: they went to a
/// pane with no room (`keys → pane#29 (commands)`, nothing of it in sight — typing landed unseen). And the
/// state says the code is drawn only when its focused pane is.
#[test]
fn keys_out_of_a_chat_filling_a_narrow_screen_go_to_code_in_sight() {
    let narrow = |ed: &mut Editor| {
        ed.focus_sync();
        crate::term::render(ed, &mut Vec::new(), 60, 30).unwrap();
    };
    let mut ed = editor();
    chat(&mut ed, "");
    narrow(&mut ed);
    assert_eq!(ed.keys_to().region, Region::Chat);
    assert_eq!(ed.screen.chat_w, 60, "the chat fills the narrow screen while it has the keys");
    // (an empty conversation: nothing to view — `esc` is the code's)
    feed(&mut ed, &["<esc>"]);
    narrow(&mut ed);
    let k = ed.keys_to();
    assert_eq!(k.region, Region::Code);
    assert!(k.drawn, "the code the keys went to is on screen");
    assert!(ed.screen.panes.iter().any(|p| p.0 == ed.session.focus && p.1.w > 0), "its focused pane drawn");
    assert_eq!(ed.screen.chat_w, 0);
    // `space l`: the chat again, filling it
    feed(&mut ed, &[" ", "l"]);
    narrow(&mut ed);
    assert_eq!((ed.keys_to().region, ed.screen.chat_w), (Region::Chat, 60));
}

/// The Claude settings card is weighty, but it doesn't take the keys from the chat's empty input as others do:
/// claude runs held back beside it, and what's typed is a message. Each keeps its own — `C-q` gives the card
/// them, Esc gives them back, unanswered.
#[test]
fn the_claude_settings_card_and_the_chat_input_each_keep_their_keys() {
    let mut ed = editor();
    chat(&mut ed, "");
    settings_card(&mut ed);
    frame(&mut ed);
    settle(&mut ed);
    assert_eq!(ed.keys_to().region, Region::Chat);
    feed(&mut ed, &["n"]);
    assert_eq!(ed.session.chat.as_ref().unwrap().input, "n", "typed, not an answer");
    assert_eq!(ed.offers.len(), 1, "the card still up");
    feed(&mut ed, &["<C-q>"]);
    frame(&mut ed);
    assert!(matches!(ed.keys_to().region, Region::Card(_)), "{:?}", ed.keys_to().region);
    feed(&mut ed, &["<esc>"]);
    frame(&mut ed);
    assert_eq!(ed.keys_to().region, Region::Chat, "given back");
    assert_eq!(ed.offers.len(), 1, "unanswered");
}

/// frictions26: a card that runs something comes mid-turn while the chat's input is empty — the chat keeps
/// the keys. A message typed next is typed (its `a` was a grant for the chat when the card took them); `C-q`
/// hands them to the card, and once it's answered they're the chat's again.
#[test]
fn a_card_come_mid_turn_leaves_the_empty_chat_its_keys() {
    let mut ed = editor();
    chat(&mut ed, "");
    weighty_card(&mut ed);
    frame(&mut ed);
    settle(&mut ed);
    assert_eq!(ed.keys_to().region, Region::Chat);
    feed(&mut ed, &["a", "n", "d"]);
    frame(&mut ed);
    assert_eq!(ed.offers.len(), 1, "unanswered");
    assert_eq!(ed.session.chat.as_ref().unwrap().input, "and", "typed");
    assert!(shown(&mut ed).contains("C-q to answer it"), "the card says how");
    // `C-q`: the card's, `n` answers it, the chat has them again
    ed.session.chat.as_mut().unwrap().input.clear();
    ed.session.chat.as_mut().unwrap().cursor = 0;
    feed(&mut ed, &["<C-q>"]);
    assert!(matches!(ed.keys_to().region, Region::Card(_)), "{:?}", ed.keys_to().region);
    feed(&mut ed, &["n"]);
    frame(&mut ed);
    assert!(ed.offers.is_empty(), "answered");
    assert_eq!(ed.keys_to().region, Region::Chat, "back where they were");
}

/// Right after an edit's review is answered that took the keys from the chat (Claude asking mid-turn, its
/// input empty) — `tarae state` said `keys → pane#29 (commands)`: the code's normal-mode commands, while you'd
/// been in the chat. They go back to the chat, as its Esc already gave them.
#[test]
fn the_keys_go_back_to_the_chat_once_its_review_is_answered() {
    // (and where they go in the code, the state says it's the code — not a pane called "commands")
    let mut code = editor();
    code.focus_sync();
    let said = code.ui_state().text();
    assert!(said.contains(&format!("keys → pane#{}, the code (commands)", code.session.focus)), "{said}");
    // An edit's review that took them from the chat (`edits::show`): its `y` gives them back too
    let mut ed = editor();
    chat(&mut ed, "");
    edit_review(&mut ed);
    ed.session.leave(crate::session::At::Chat);
    ed.session.review.as_mut().unwrap().from_chat = true;
    frame(&mut ed);
    assert_eq!(ed.keys_to().region, Region::EditReview);
    feed(&mut ed, &["y"]);
    frame(&mut ed);
    assert!(ed.session.review.is_none(), "answered");
    assert_eq!(ed.keys_to().region, Region::Chat, "back where they were");
}

// ── The state ────────────────────────────────────────────────────────────

/// A card waiting in session `id`, behind — numbered as `push_offer` numbers them.
fn park(ed: &mut Editor, id: crate::event::SessionId, mut o: Offer) {
    ed.offer_seq += 1;
    o.id = ed.offer_seq;
    ed.session_by_id(id).unwrap().parked.push(o);
}

/// What doesn't stay put from run to run (pid, boot, times) — out of the golden JSON.
fn steady(mut v: Value) -> Value {
    v["tarae"]["pid"] = json!(0);
    v["tarae"]["boot"] = json!("-");
    v["tarae"]["version"] = json!("-");
    for c in v["cards"].as_array_mut().unwrap() {
        c["waited_ms"] = json!(0);
        if !c["in_sight_ms"].is_null() {
            c["in_sight_ms"] = json!(0);
        }
    }
    // (the inbox's entries wait as long as the test took to get there)
    for e in v["inbox"].as_array_mut().into_iter().flatten() {
        e["waited_ms"] = json!(0);
    }
    for s in v["sessions"].as_array_mut().unwrap() {
        s["root"] = json!(Path::new(s["root"].as_str().unwrap()).file_name().unwrap().to_string_lossy());
        if s["usage"]["seconds_left"].is_u64() {
            s["usage"]["seconds_left"] = json!(3600);
        }
    }
    // (run ids are the process's sequence — other tests take theirs)
    for r in v["runs"].as_array_mut().unwrap() {
        (r["id"], r["since_ms"]) = (json!("run#-"), json!(0));
    }
    for g in v["gone"].as_array_mut().unwrap() {
        g["ago_ms"] = json!(0);
    }
    // (the inbox's waits too — a loaded machine took 2 ms between the cards going up and the state)
    for e in v["inbox"].as_array_mut().unwrap() {
        e["waited_ms"] = json!(0);
    }
    v
}

/// A turn of one request (12 in, 3k written to the 1h cache, 9k read, 400 out) sent now, its result
/// saying $0.0213 so far of a fresh conversation in a 200k window.
fn usage_turn(u: &mut crate::usage::Usage) {
    use crate::usage::{Counts, Seen, Spent, Ttl};
    let usage = json!({ "input_tokens": 12, "cache_creation_input_tokens": 3000, "cache_read_input_tokens": 9000,
                        "output_tokens": 400, "cache_creation": { "ephemeral_1h_input_tokens": 3000 } });
    u.fresh("conv-1");
    u.requesting(crate::usage::now_ms());
    let model = "claude-opus-5-5".to_string();
    u.seen(Seen {
        id: Some("msg-1".into()),
        model: model.clone(),
        counts: Counts::of(&usage),
        ttl: Ttl::of(&usage),
        at_ms: 0,
    });
    u.spent(Spent {
        conversation: "conv-1".into(),
        counts: Counts::of(&usage),
        total_cost: Some(0.0213),
        models: vec![(model, Some(200_000))],
        alone: None,
        ms: Some(4200),
        at_ms: 1_791_100_000_000,
    });
}

/// The golden JSON: one fixed scene — two sessions, a card up and one parked behind, an edit's review in
/// hand, a warning on the card — pinned like the frames in `src/snapshots/` (`TARAE_BLESS=1` rewrites it).
/// A change of the shape shows as a diff in review; `v` goes up when a field goes or changes meaning.
#[test]
fn the_state_as_json_is_pinned() {
    let mut ed = editor();
    (ed.session.name, ed.session.root) = ("main".into(), std::env::temp_dir().join("tarae-uistate-main"));
    let id = ed.session_add(std::env::temp_dir().join("tarae-uistate-golden"));
    ed.session_by_id(id).unwrap().name = "fix-login".into();
    park(
        &mut ed,
        id,
        Offer::new(
            "Claude changes 2 files".into(),
            "Claude renames `x`.",
            "",
            String::new(),
            What::ChangeSet { whose: crate::refactor::Whose::Claude(7), ops: false },
        ),
    );
    // (its Claude had a turn: a request writing to the 1h cache, then the result — `usage.rs`)
    let u = &mut ed.session_by_id(id).unwrap().usage;
    usage_turn(u);
    // (its runs' sandboxes: in front a Linux keeper not served — pidfd_getfd refused —, behind macOS's with ports
    // open — 9d)
    let offline = landlock_run(crate::sandbox::guarantees::Ns::NONE, false);
    ed.session.sandbox = Some(offline);
    ed.session_by_id(id).unwrap().sandbox =
        Some(crate::sandbox::guarantees::Guarantees::seatbelt(true, false));
    edit_review(&mut ed);
    task_panel(&mut ed);
    light_card(&mut ed);
    ed.set_warning("a.rs changed on disk");
    frame(&mut ed);
    let actual = serde_json::to_string_pretty(&steady(ed.ui_state().json())).unwrap() + "\n";
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/snapshots/ui_state.json");
    if std::env::var_os("TARAE_BLESS").is_some_and(|v| v == "1") {
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        expected == actual,
        "{} differs — if intended: TARAE_BLESS=1 cargo test the_state_as_json_is_pinned\n\n{actual}",
        path.display()
    );
}

/// #204: a session's last turn, as its chat shows it, for a driver (`tarae state --chat`) — from the last
/// message sent on, the tool rows as drawn, then the answer — pinned like the state (`TARAE_BLESS=1`). By id
/// or name; the one in front when none is named; an unknown one says which there are.
#[test]
fn a_sessions_last_turn_is_pinned() {
    use crate::chat::{Chat, Look, Msg, Role};
    let mut ed = editor();
    (ed.session.name, ed.session.root) = ("main".into(), std::env::temp_dir().join("tarae-uistate-main"));
    let id = ed.session_add(std::env::temp_dir().join("tarae-uistate-chat"));
    let l = ed.session_by_id(id).unwrap();
    l.name = "fix-login".into();
    let mut c = Chat::new();
    c.msgs.push(Msg::new(Role::User, "an earlier question", ""));
    c.msgs.push(Msg::new(Role::Assistant, "an earlier answer", ""));
    c.msgs.push(Msg::new(Role::User, "why does login fail?", "login.rs · L12"));
    let look =
        Look {
            verb: "read", what: "src/login.rs".into(), pattern: false, range: "L1–40".into(), link: None
        };
    c.msgs.push(Msg::tool(look, "t1"));
    c.msgs.push(Msg::new(Role::Assistant, "The token check runs before the refresh: src/login.rs:12.", ""));
    l.chat = Some(c);
    l.at = crate::session::At::Chat;
    frame(&mut ed);
    let t = ed.chat_turn(&format!("s{id}")).unwrap();
    assert_eq!(ed.chat_turn("fix-login").unwrap().rows, t.rows, "by name too");
    assert!(ed.chat_turn("").unwrap().rows.is_empty(), "the one in front, no conversation");
    let e = ed.chat_turn("nope").err().unwrap();
    assert!(e.contains("s0 main") && e.contains("fix-login"), "{e}");
    let text = t.text();
    assert!(!text.contains("earlier") && text.contains("◦ read  src/login.rs  L1–40"), "{text}");
    let actual = serde_json::to_string_pretty(&t.json()).unwrap() + "\n";
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/snapshots/ui_chat.json");
    if std::env::var_os("TARAE_BLESS").is_some_and(|v| v == "1") {
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        expected == actual,
        "{} differs — if intended: TARAE_BLESS=1 cargo test a_sessions_last_turn_is_pinned\n\n{actual}",
        path.display()
    );
}

/// The text form says the same in a few lines; the JSON parses back with what drivers read first.
#[test]
fn the_text_form_and_the_json_agree() {
    let mut ed = editor();
    light_card(&mut ed);
    frame(&mut ed);
    let s = ed.ui_state();
    let v = s.json();
    assert_eq!(v["v"], V);
    assert_eq!(v["keys"]["to"], format!("card#{}", ed.offers[0].id));
    assert_eq!(v["keys"]["takes"], "answers");
    assert_eq!(v["keys"]["under"], "code");
    assert_eq!(v["cards"][0]["answers"][0], json!({ "key": "y", "does": "End it" }));
    let text = s.text();
    assert!(
        text.lines().nth(1).unwrap().starts_with(&format!("keys → card#{} (answers y n)", ed.offers[0].id)),
        "{text}"
    );
    assert!(text.contains("y end it · n keep reviewing"), "{text}");
}

/// A snapshot asked with `after_input` waits for those keys to be handled and drawn — then it's about
/// them; one asked plainly is answered by the next frame.
#[test]
fn a_snapshot_waits_for_its_input_and_a_frame() {
    let mut ed = editor();
    frame(&mut ed);
    let (tx, rx) = std::sync::mpsc::channel();
    let until = Instant::now() + Duration::from_secs(10);
    let input = ed.input_seq;
    ed.ui_asks.push(Ask {
        after_input: input + 1,
        json: true,
        reply: tx.clone(),
        until,
        what: crate::uistate::Asked::State,
    });
    ed.ui_asks.push(Ask {
        after_input: 0,
        json: false,
        reply: tx,
        until,
        what: crate::uistate::Asked::State,
    });
    frame(&mut ed);
    let first = rx.try_recv().expect("the plain one, with the frame");
    assert!(first.starts_with("tarae "), "{first}");
    assert!(rx.try_recv().is_err(), "the other waits for its key");
    feed(&mut ed, &[":"]);
    assert!(rx.try_recv().is_err(), "handled, not drawn yet");
    frame(&mut ed);
    let v: Value = serde_json::from_str(&rx.try_recv().expect("after the key's frame")).unwrap();
    assert_eq!((v["input"].as_u64(), v["keys"]["region"].as_str()), (Some(input + 1), Some("prompt")));
    assert!(ed.ui_asks.is_empty());
}

/// #209: the frame asked over the socket is the one the terminal got, whole — what `cargo xtask shot --live`
/// draws as a picture: after the keys sent, at its size, its bytes reading as the screen (the prompt opened by
/// the key is on it). Always JSON, whatever the format asked.
#[test]
fn the_frame_asked_is_the_one_drawn_after_the_keys() {
    let mut ed = editor();
    frame(&mut ed);
    let (tx, rx) = std::sync::mpsc::channel();
    let until = Instant::now() + Duration::from_secs(10);
    let input = ed.input_seq;
    ed.ui_asks.push(Ask { after_input: input + 1, json: true, reply: tx, until, what: Asked::Frame });
    frame(&mut ed);
    assert!(rx.try_recv().is_err(), "it waits for its key");
    feed(&mut ed, &[":"]);
    frame(&mut ed);
    let v: Value = serde_json::from_str(&rx.try_recv().expect("after the key's frame")).unwrap();
    let f = &v["frame"];
    assert_eq!(
        (f["cols"].as_u64(), f["rows"].as_u64(), f["input"].as_u64()),
        (Some(120), Some(40), Some(input + 1))
    );
    let bytes = f["bytes"].as_str().unwrap();
    assert_eq!(
        bytes.as_bytes(),
        ed.last_frame.as_ref().unwrap().0.as_slice(),
        "the frame as the terminal got it"
    );
    let text = crate::screen::text(bytes.as_bytes(), 120, 40).unwrap();
    // (the prompt's input inside its frame)
    let input = |l: &str| l.trim_start_matches([' ', '│']).starts_with(':');
    assert!(text.contains("two") && text.lines().any(input), "{text}");
}

// ── The socket ───────────────────────────────────────────────────────────

/// A short folder of the test's own (a socket's path has 104 bytes at most).
fn sock_dir(name: &str) -> PathBuf {
    crate::scratch::Scratch::new(&format!("tu-{name}")).into_path()
}

/// Pumps the editor's events, drawing after each batch as the main loop does, until `done`.
fn pump_until(ed: &mut Editor, done: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < end, "timed out");
        if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(10)) {
            ed.handle_event(ev);
            frame(ed);
        }
    }
}

/// Over the socket: the state as JSON and as text, `after_input`, an unknown request said so; and a peer
/// under one of Claude's processes refused (here the test itself, made Claude's for the test).
#[test]
fn the_socket_answers_snapshots_and_refuses_claudes_processes() {
    use std::io::{BufRead, BufReader, Write};
    let dir = sock_dir("serve");
    let listener = crate::uisocket::bind_in(&dir.join("ui"), 4242).unwrap();
    let path = dir.join("ui/4242.sock");
    let mut ed = editor();
    crate::uisocket::serve(listener, ed.events.shared_sender());
    let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let ask = |reqs: Vec<&'static str>| {
        let (path, asked) = (path.clone(), asked.clone());
        std::thread::spawn(move || {
            let mut s = std::os::unix::net::UnixStream::connect(&path).unwrap();
            let mut lines = BufReader::new(s.try_clone().unwrap()).lines();
            for r in reqs {
                writeln!(s, "{r}").unwrap();
                let got = lines.next().unwrap().unwrap();
                eprintln!("ASKED {r} → {}", &got[..got.len().min(120)]);
                asked.lock().unwrap().push(got);
            }
        })
    };
    let input = ed.input_seq;
    let after = Box::leak(format!("{{\"get\":\"state\",\"after_input\":{}}}", input + 2).into_boxed_str());
    let t = ask(vec![r#"{"get":"state"}"#, r#"{"get":"state","format":"text"}"#, r#"{"get":"what"}"#, after]);
    let n = |k: usize| {
        let asked = asked.clone();
        move || asked.lock().unwrap().len() >= k
    };
    pump_until(&mut ed, n(3));
    {
        let a = asked.lock().unwrap();
        let v: Value = serde_json::from_str(&a[0]).unwrap();
        assert_eq!((v["v"].as_u64(), v["keys"]["region"].as_str()), (Some(V), Some("code")), "{}", a[0]);
        let text: Value = serde_json::from_str(&a[1]).unwrap();
        assert!(
            text["text"].as_str().is_some_and(|t| t.contains("keys → pane#")),
            "{:?}",
            &a[1][..a[1].len().min(300)]
        );
        assert!(a[2].contains("unknown request"), "{}", a[2]);
    }
    // The fourth waits for two keys
    let end = Instant::now() + Duration::from_secs(10);
    while ed.ui_asks.is_empty() {
        assert!(Instant::now() < end, "the fourth request reached the editor");
        if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(10)) {
            ed.handle_event(ev);
        }
    }
    feed(&mut ed, &["g"]);
    frame(&mut ed);
    assert_eq!(asked.lock().unwrap().len(), 3, "one key isn't two: {:?}", asked.lock().unwrap().get(3));
    feed(&mut ed, &["g"]);
    frame(&mut ed);
    pump_until(&mut ed, n(4));
    let v: Value = serde_json::from_str(&asked.lock().unwrap()[3]).unwrap();
    assert_eq!(v["input"].as_u64(), Some(input + 2));
    t.join().unwrap();
    // Claude's: refused (its process tree holds the asker — here, this test)
    ed.llm_pid = Some(std::process::id());
    let t = ask(vec![r#"{"get":"state"}"#]);
    pump_until(&mut ed, n(5));
    t.join().unwrap();
    let refused = asked.lock().unwrap()[4].clone();
    assert!(refused.contains("refused: under a process of Claude's"), "{refused}");
    std::fs::remove_dir_all(&dir).ok();
}

/// A shuttle over the socket (`shuttle.rs`): refused while `ui.shuttle-keys` is off; on, its hello waits on the
/// card and `y` connects it; a step lands after the frame that draws it, saying where and what took it; one written
/// for another place is refused at once, nothing typed.
#[test]
fn a_shuttle_types_over_the_socket_once_you_said_yes() {
    use std::io::{BufRead, BufReader, Write};
    let dir = sock_dir("shuttle");
    let listener = crate::uisocket::bind_in(&dir.join("ui"), 4343).unwrap();
    let path = dir.join("ui/4343.sock");
    let mut ed = editor();
    // (its peer is this test, this process: as if another tarae's — a peer under tarae comes only from its terminal)
    ed.shuttles.above = Some(vec![u32::MAX]);
    crate::uisocket::serve(listener, ed.events.shared_sender());
    frame(&mut ed);
    let pane = ed.keys_to().to;
    let got = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Value>::new()));
    // (a connection saying hello as this test's process, then sending `reqs`: every answer kept)
    let shuttle = |reqs: Vec<Value>| {
        let (path, got) = (path.clone(), got.clone());
        std::thread::spawn(move || {
            let mut s = std::os::unix::net::UnixStream::connect(&path).unwrap();
            let mut lines = BufReader::new(s.try_clone().unwrap()).lines();
            // (no `for`: the grant is this connection's process alone — here, this test)
            let hello = serde_json::json!({ "shuttle": { "name": "t" } });
            for r in std::iter::once(hello).chain(reqs) {
                writeln!(s, "{r}").unwrap();
                let v: Value = serde_json::from_str(&lines.next().unwrap().unwrap()).unwrap();
                let done = v.get("refused").is_some() && r.get("shuttle").is_some();
                got.lock().unwrap().push(v);
                if done {
                    return;
                }
            }
        })
    };
    let n = |k: usize| {
        let got = got.clone();
        move || got.lock().unwrap().len() >= k
    };
    let t = shuttle(Vec::new());
    pump_until(&mut ed, n(1));
    t.join().unwrap();
    assert_eq!(got.lock().unwrap()[0]["refused"], "off");
    ed.config.shuttle_keys = crate::shuttle::Level::Light;
    let step = |to: &str| serde_json::json!({ "keys": "j", "expect": { "keys": { "to": to } } });
    let t = shuttle(vec![step(&pane), step("chat")]);
    let end = Instant::now() + Duration::from_secs(10);
    while !ed.offers.iter().any(|o| o.kind() == "shuttle") {
        assert!(Instant::now() < end, "the card came");
        if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(10)) {
            ed.handle_event(ev);
        }
    }
    frame(&mut ed);
    settle(&mut ed);
    assert_eq!(got.lock().unwrap().len(), 1, "the hello waits on you");
    let card = ed.offers.iter().find(|o| o.kind() == "shuttle").unwrap();
    let pid = std::process::id();
    assert!(card.question.contains(&format!("(pid {pid}, started")), "its own process: {}", card.question);
    assert!(!card.question.contains("Every program") && card.warning.is_none(), "{}", card.question);
    feed(&mut ed, &["y"]);
    let person = ed.person_seq;
    pump_until(&mut ed, n(4));
    t.join().unwrap();
    let got = got.lock().unwrap();
    assert_eq!(got[1]["shuttle"], "connected", "{}", got[1]);
    assert_eq!(
        (got[2]["landed"].as_str(), got[2]["took"].as_str()),
        (Some(pane.as_str()), Some("code")),
        "{}",
        got[2]
    );
    assert!(got[2]["frame"].as_u64().is_some(), "answered after a frame: {}", got[2]);
    assert_eq!(got[3]["refused"], "expect", "{}", got[3]);
    assert_eq!((ed.cursor_line(), ed.person_seq), (1, person), "one j, not the person's");
    std::fs::remove_dir_all(&dir).ok();
}

/// The folder is this user's alone and the socket too (0700, 0600) — even under an open umask; a socket
/// left at this pid's path (a dead tarae's) is replaced, anything else there isn't touched.
#[test]
fn the_socket_is_this_users_alone() {
    use std::os::unix::fs::PermissionsExt;
    let dir = sock_dir("modes");
    let ui = dir.join("ui");
    std::fs::create_dir_all(&ui).unwrap();
    std::fs::set_permissions(&ui, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A dead tarae's socket at the pid's path
    drop(std::os::unix::net::UnixListener::bind(ui.join("77.sock")).unwrap());
    let l = crate::uisocket::bind_in(&ui, 77).unwrap();
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&ui), 0o700, "the folder closed");
    assert_eq!(mode(&ui.join("77.sock")), 0o600);
    drop(l);
    // Not a socket: left as it is, no socket
    std::fs::write(ui.join("78.sock"), "mine").unwrap();
    assert!(crate::uisocket::bind_in(&ui, 78).is_err());
    assert_eq!(std::fs::read_to_string(ui.join("78.sock")).unwrap(), "mine");
    std::fs::remove_dir_all(&dir).ok();
}

// ── No silent state change ───────────────────────────────────────────────

/// The run card of 2026-10-04: a card too tall for the screen, its whole text open in its buffer, the card
/// holding the keys (`C-q`). `y` before it's read to its end isn't taken — and the card's bar says so, in the
/// warning colour (it only flashed in the status line). The keys that read it (`g e`) move its buffer from
/// the card (they were refused, and nothing could be done without leaving it); read to its end, the bar says
/// `y` answers.
#[test]
fn a_card_says_why_y_waits_and_lets_its_text_be_read_from_it() {
    let mut ed = editor();
    let script: String = (1..=60).map(|i| format!("echo step {i}\n")).collect();
    let q = format!("{script}\nRuns the script Claude wrote this turn.");
    let o = Offer::new(
        "Claude wants to run a script".into(),
        &q,
        "",
        String::new(),
        What::Install {
            tool: "script".into(),
            run: vec!["true".into()],
            cwd: ed_root(),
            then: crate::install::Then::Lsp,
        },
    );
    ed.push_offer(o);
    let (w, h) = (100, 24);
    let shown = |ed: &mut Editor| {
        ed.focus_sync();
        crate::term::render(ed, &mut Vec::new(), w, h).unwrap();
        crate::screen::text(ed.last_frame.as_ref().unwrap().0.as_slice(), w, h).unwrap()
    };
    shown(&mut ed);
    settle(&mut ed);
    assert!(!ed.offer_seen_whole(), "cut to fit");
    feed(&mut ed, &["<C-q>", "v"]);
    shown(&mut ed);
    settle(&mut ed);
    assert!(ed.offer_bar(), "its text in its buffer, the card a bar over the status line");
    feed(&mut ed, &["y"]);
    let s = shown(&mut ed);
    assert!(!ed.offers.is_empty() && s.contains("ge to its end first — y waits"), "said on the bar:\n{s}");
    feed(&mut ed, &["g", "e"]);
    assert!(ed.card_keys, "the card still holds the keys");
    let s = shown(&mut ed);
    assert!(ed.offer_seen_whole(), "read to its end from the card:\n{s}");
    assert!(s.contains("echo step 60"), "{s}");
    // A key it has no use for: said there too
    feed(&mut ed, &["x"]);
    let s = shown(&mut ed);
    assert!(s.contains("x isn't an answer here"), "{s}");
    feed(&mut ed, &["n"]);
    assert!(ed.offers.is_empty());
}

/// A tall run card up, drawn and settled, its whole text open in its buffer with `keys` (`v` — the card keeps
/// the keys while it's read; `C-q v`; `v <esc>`: given back).
fn reading_a_tall_card(keys: &[&str]) -> Editor {
    let mut ed = editor();
    let script: String = (1..=60).map(|i| format!("echo step {i}\n")).collect();
    let q = format!("{script}\nRuns the script Claude wrote this turn.");
    let what = What::Install {
        tool: "script".into(),
        run: vec!["true".into()],
        cwd: ed_root(),
        then: crate::install::Then::Lsp,
    };
    ed.push_offer(Offer::new("Claude wants to run a script".into(), &q, "", String::new(), what));
    bar_frame(&mut ed);
    settle(&mut ed);
    feed(&mut ed, keys);
    bar_frame(&mut ed);
    settle(&mut ed);
    assert!(ed.offer_reading(), "its text is the buffer in sight");
    ed
}

/// The frame as the user sees it (100 × 24) — the card a bar over the status line while its text is read.
fn bar_frame(ed: &mut Editor) -> String {
    ed.focus_sync();
    crate::term::render(ed, &mut Vec::new(), 100, 24).unwrap();
    crate::screen::text(ed.last_frame.as_ref().unwrap().0.as_slice(), 100, 24).unwrap()
}

/// frictions15 #1: reading a tall card's text in its buffer, however the keys got where they are — the card
/// held them all along (`j`/`k` read through it), Esc gave them back, a view key pending — one `C-q` gives the
/// card the keys, never takes them away (it was a toggle: the `C-q` meant to answer let go of a card holding
/// them, unsaid, and the next key went to the code). Its answer then answers it.
#[test]
fn one_c_q_gives_a_card_being_read_its_keys_whatever_held_them() {
    let (given, plain, back) = (&["<C-q>", "v"][..], &["v"][..], &["v", "<esc>"][..]);
    for (opened, keys) in [
        (given, &["j", "j"][..]),
        (given, &["<esc>", "j"]),
        (plain, &["j"]),
        (back, &["j"]),
        (back, &["z"]),
        (given, &["g", "e"]),
    ] {
        let mut ed = reading_a_tall_card(opened);
        feed(&mut ed, keys);
        bar_frame(&mut ed);
        feed(&mut ed, &["<C-q>"]);
        assert!(ed.card_keys, "{keys:?}: one C-q, the card has the keys");
        let s = bar_frame(&mut ed);
        assert!(s.contains("esc goes back"), "{keys:?}: the bar says whose keys they are:\n{s}");
        feed(&mut ed, &["<C-q>"]);
        assert!(ed.card_keys, "{keys:?}: C-q again keeps them the card's");
        assert!(ed.status.as_ref().is_some_and(|s| s.0.contains("the card has the keys")), "said");
        feed(&mut ed, &["n"]);
        assert!(ed.offers.is_empty(), "{keys:?}: its answer answered it");
    }
}

/// frictions15 #1, after its review: in a card's buffer Esc only ever gives the card's keys back (#25) — it
/// never hands them over. It did for a while, and Esc after Esc took turns, the toggle `C-q` had just lost, on
/// the key pressed out of habit. `C-q` is the one way to give them; the bar says so.
#[test]
fn esc_in_a_cards_buffer_only_gives_the_keys_back() {
    let mut ed = reading_a_tall_card(&["<C-q>", "v"]);
    feed(&mut ed, &["<esc>"]);
    assert!(!ed.card_keys, "held: Esc gives them back (#25)");
    // (a view key pending: the code's keys — the bar says how to answer)
    feed(&mut ed, &["z"]);
    let s = bar_frame(&mut ed);
    assert!(s.contains("C-q answers it"), "the code has them, said on the bar:\n{s}");
    for _ in 0..3 {
        feed(&mut ed, &["<esc>"]);
        assert!(!ed.card_keys, "Esc again: still the code's — no turns taken");
    }
    feed(&mut ed, &["<C-q>"]);
    assert!(ed.card_keys && bar_frame(&mut ed).contains("esc goes back"));
    feed(&mut ed, &["n"]);
    assert!(ed.offers.is_empty());
}

/// The review's case: Esc then `y` in a card's buffer — a yank, out of habit — doesn't answer the card, though
/// its text was read to its end and `y` would install. Opened with `v` or with `C-q v`, the card holds the
/// keys while it's read; after Esc only `C-q` gives them again — normal mode's free keys aren't the card's
/// in its buffer. The bar says `C-q answers it`.
#[test]
fn esc_then_y_in_a_cards_buffer_doesnt_answer_it() {
    for opened in [&["v"][..], &["<C-q>", "v"]] {
        let mut ed = reading_a_tall_card(opened);
        assert!(ed.card_keys, "{opened:?}: the card holds the keys while it's read");
        feed(&mut ed, &["g", "e"]);
        bar_frame(&mut ed);
        assert!(ed.offer_seen_whole(), "read to its end: `y` would answer it");
        let id = ed.offers[0].id;
        feed(&mut ed, &["<esc>", "y"]);
        assert!(!ed.card_keys, "{opened:?}: Esc gave them back");
        assert_eq!(ed.offers.first().map(|o| o.id), Some(id), "{opened:?}: still up, unanswered");
        assert!(matches!(ed.offers[0].state, crate::offer::OfferState::Asking), "not installing");
        let s = bar_frame(&mut ed);
        assert!(s.contains("C-q answers it"), "{s}");
        feed(&mut ed, &["<C-q>", "n"]);
        assert!(ed.offers.is_empty(), "{opened:?}: given with C-q, its answer answers it");
    }
}

/// Frictions 13, #4 and #5: a long card in the state. Its `asks` is whole in the JSON (scripts and the inbox
/// read all of it) and cut in the one-line text form; the JSON says how many lines its drawing left out and
/// whether it's been seen whole — not cut-and-unread, not after `v` alone (its buffer open, not read to its
/// end), yes once read to its end; the text line says the same.
#[test]
fn a_long_cards_state_is_whole_and_says_what_has_been_seen() {
    let mut ed = editor();
    let script: String = (1..=60).map(|i| format!("echo step {i}\n")).collect();
    let q = format!("{script}\nRuns the script Claude wrote this turn.");
    let o = Offer::new(
        "Claude wants to run a script".into(),
        &q,
        "",
        String::new(),
        What::Install {
            tool: "script".into(),
            run: vec!["true".into()],
            cwd: ed_root(),
            then: crate::install::Then::Lsp,
        },
    );
    ed.push_offer(o);
    let shown = |ed: &mut Editor| {
        ed.focus_sync();
        crate::term::render(ed, &mut Vec::new(), 100, 24).unwrap();
        let s = ed.ui_state();
        (s.json()["cards"][0].clone(), s.text().lines().find(|l| l.starts_with("card#")).unwrap().to_string())
    };
    let (card, line) = shown(&mut ed);
    // #4: whole in the JSON, its last decided line there (the text line too — frictions15 #2)
    let asks = card["asks"].as_str().unwrap();
    assert!(asks.ends_with("echo step 60") && !asks.ends_with('…'), "{asks}");
    assert!(asks.chars().count() > 300, "longer than Claude's level's cut");
    let inbox = &ed.ui_state().json()["inbox"][0];
    assert_eq!(inbox["asks"].as_str(), Some(asks), "the inbox has it whole too");
    // Claude's level doesn't grow with it: cut as the one-line forms cut it, its inbox entry too
    let claude = ed.ui_state_claude().json();
    let theirs = claude["cards"][0]["asks"].as_str().unwrap();
    assert!(theirs.ends_with('…') && theirs.chars().count() == super::ASKS_MAX, "{theirs}");
    assert_eq!(claude["inbox"][0]["asks"].as_str(), Some(theirs), "{claude}");
    assert!(!ed.ui_state_claude().text_claude(false).contains("echo step 60"));
    // The inbox picker's row: cut too (the whole is its card, one `enter` away)
    ed.inbox_open();
    let label = ed.picker.as_ref().expect("the inbox").items()[0].label.clone();
    assert_eq!(label, theirs, "one row, cut");
    ed.picker = None;
    // #5: cut, not seen whole
    let cut = card["cut"].as_u64().unwrap();
    assert!(cut > 0 && card["seen_whole"] == false && card["viewing"] == false, "{card}");
    assert!(line.ends_with(&format!(" · {cut} more lines, not seen whole")), "{line}");
    // `v`: its text open in its buffer — not yet read to its end
    settle(&mut ed);
    feed(&mut ed, &["<C-q>", "v"]);
    let (card, line) = shown(&mut ed);
    assert!(card["viewing"] == true && card["seen_whole"] == false && card["cut"].is_null(), "{card}");
    assert!(line.ends_with(" · open in its buffer, not read to its end"), "{line}");
    // Read to its end: seen whole
    feed(&mut ed, &["g", "e"]);
    let (card, line) = shown(&mut ed);
    assert!(card["seen_whole"] == true, "{card}");
    assert!(line.ends_with(" · seen whole"), "{line}");
    feed(&mut ed, &["n"]);
}

/// frictions15 #2: a driver asked to approve a long command reads all of it in every form the full level
/// has — the text line and a watcher's `card-up` (its `said`, and an `asks` of its own, so a driver never
/// parses `said`), not only the JSON. Still only what the card draws at full strength (the privacy rule):
/// the script's last decided line, never the faint lines after it. Claude's level keeps its cut (its tokens).
#[test]
fn a_long_cards_asks_is_whole_in_every_full_level_form() {
    let mut ed = editor();
    let script: String = (1..=60).map(|i| format!("echo step {i}\n")).collect();
    let q = format!("{script}\nRuns the script Claude wrote this turn.");
    let what = What::Install {
        tool: "script".into(),
        run: vec!["true".into()],
        cwd: ed_root(),
        then: crate::install::Then::Lsp,
    };
    frame(&mut ed);
    let (tx, rx) = std::sync::mpsc::sync_channel(watch::UNREAD);
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    ed.ui_watch_add(watch::Watcher { tx, kinds: vec!["cards".into()], overflowed: flag });
    ed.push_offer(Offer::new("Claude wants to run a script".into(), &q, "", String::new(), what));
    frame(&mut ed);
    let line = ed.ui_state().text().lines().find(|l| l.starts_with("card#")).unwrap().to_string();
    assert!(line.contains("echo step 60") && !line.contains('…'), "{line}");
    assert!(!line.contains("Runs the script"), "the faint line isn't in it: {line}");
    let up: Value = rx.try_iter().map(|l| serde_json::from_str(&l).unwrap()).next().expect("card-up");
    assert_eq!(up["event"], "card-up");
    let asks = up["asks"].as_str().expect("its own asks");
    assert!(asks.ends_with("echo step 60") && !asks.contains('…'), "{asks}");
    assert!(up["said"].as_str().unwrap().contains("echo step 60"), "{up}");
    let claude = ed.ui_state_claude().json();
    assert!(claude["cards"][0]["asks"].as_str().unwrap().ends_with('…'), "Claude's level is cut");
}

/// cardburden 5: a driver read only `asks` — the decided lines — so why a card asked, its warnings and the diff
/// since a `y` weren't in the state, and it read them off the screen. The full level has the card's text
/// whole, each line with what it is as drawn (warning over diff over decided; the rest `said`), and the
/// warnings' ids; the text form stays one line; Claude's level keeps its cut (the ids, no text).
#[test]
fn a_cards_text_is_whole_in_the_state_each_line_with_its_part() {
    let mut ed = editor();
    let q = "cargo test\n\nIt runs outside the sandbox.\nIt reaches beyond this machine.\n@@ -1 +1 @@\n+echo b\nfaint";
    let mut o = Offer::new(
        "Claude wants to run a command".into(),
        q,
        "",
        String::new(),
        What::Install {
            tool: "t".into(),
            run: vec!["true".into()],
            cwd: ed_root(),
            then: crate::install::Then::Lsp,
        },
    )
    .warning(2..3);
    (o.decided, o.diff) = (Some(0..1), Some(4..6));
    o.warnings.push(3..4);
    let why = crate::cards::Why::NotACheck("cargo".into());
    let run = crate::cards::Request::Run {
        past: crate::cards::Past::No,
        sandboxed: true,
        reads: Vec::new(),
        outward: false,
    };
    o.asked = Some(crate::cards::Asked::new(why, run).warned(vec!["outward".into()]));
    ed.push_offer(o);
    frame(&mut ed);
    let card = ed.ui_state().json()["cards"][0].clone();
    let text: Vec<(&str, &str)> = card["text"]
        .as_array()
        .expect("its text")
        .iter()
        .map(|l| (l["line"].as_str().unwrap(), l["role"].as_str().unwrap()))
        .collect();
    assert_eq!(
        text,
        [
            ("cargo test", "decided"),
            ("", "said"),
            ("It runs outside the sandbox.", "warning"),
            ("It reaches beyond this machine.", "warning"),
            ("@@ -1 +1 @@", "diff"),
            ("+echo b", "diff"),
            ("faint", "said"),
        ]
    );
    assert_eq!(card["warnings"], json!(["outward"]));
    let line = ed.ui_state().text().lines().find(|l| l.starts_with("card#")).unwrap().to_string();
    assert!(!line.contains("faint") && !line.contains("+echo b"), "the text form stays one line: {line}");
    let claude = ed.ui_state_claude().json();
    assert!(claude["cards"][0].get("text").is_none(), "{claude}");
    assert_eq!(claude["cards"][0]["warnings"], json!(["outward"]));
    feed(&mut ed, &["n"]);
}

/// A paste in normal mode: a `:` line opens the command line with it, not run (`:w` pasted into a commit
/// message became part of its subject, unsaid); any other text is held for `p`, not put in, and said
/// (#124); into the minimized chat, it comes back with it.
#[test]
fn a_paste_in_normal_mode_is_never_silent() {
    let mut ed = editor();
    ed.handle_paste(":w\n".into());
    let p = ed.prompt.as_ref().expect("the command line");
    assert_eq!((p.kind, p.text.as_str()), (crate::editor::PromptKind::Command, "w"));
    assert_eq!(ed.doc().text.to_string(), "one\ntwo\nthree\n", "nothing went in, nothing ran");
    feed(&mut ed, &["<esc>"]);
    let said = ed.said;
    ed.handle_paste("pasted text".into());
    assert!(!ed.doc().text.to_string().contains("pasted text") && ed.said > said, "held, and said");
    assert!(ed.messages.last().unwrap().2.starts_with("pasted a line — held, not in the file"));
    feed(&mut ed, &["p"]);
    assert!(ed.doc().text.to_string().contains("pasted text"), "p puts it in");
    // In insert mode it's typed where you type: nothing to say
    feed(&mut ed, &["i"]);
    let said = ed.said;
    ed.handle_paste(":not a command".into());
    assert!(ed.prompt.is_none() && ed.said == said && ed.doc().text.to_string().contains(":not a command"));
    feed(&mut ed, &["<esc>"]);
    chat_minimized(&mut ed);
    ed.handle_paste("for claude".into());
    let c = ed.session.chat.as_ref().unwrap();
    assert!(c.input == "for claude" && !c.compact(), "the chat back, with it");
}

// ── Phase 2: panels, runs, the gone ring, watching ───────────────────────

/// A folder of the test's own with a few files, the side panel's files tab on it, a picker, the review's
/// rail of two files, a task in the slot — every panel row the state lists is on the frame, as its label.
/// (The rows only ever come from what `term.rs` recorded drawing them.)
#[test]
fn every_row_the_state_lists_is_on_the_frame() {
    let dir = sock_dir("rows");
    let mut ed = editor();
    ed.session.root = dir.clone();
    let rels = vec!["Cargo.toml".to_string(), "src/main.rs".into(), "src/panels.rs".into()];
    let mut side = crate::sidebar::Sidebar::with_files(dir.clone(), rels);
    side.set_open(["src"], true);
    ed.session.sidebar = Some(side);
    ed.session.at = crate::session::At::Side;
    task_panel(&mut ed);
    let w = 160;
    let check = |ed: &mut Editor, what: &str| {
        ed.focus_sync();
        crate::term::render(ed, &mut Vec::new(), w, 40).unwrap();
        let shown = crate::screen::text(ed.last_frame.as_ref().unwrap().0.as_slice(), w, 40).unwrap();
        let p = ed.ui_state().panels;
        let mut rows: Vec<panels::Row> = Vec::new();
        rows.extend(p.side.iter().flat_map(|s| s.2.clone()));
        rows.extend(p.review.iter().flat_map(|r| r.rows.clone()));
        rows.extend(p.picker.iter().flat_map(|r| r.rows.clone()));
        assert!(!rows.is_empty(), "{what}: rows listed");
        for r in &rows {
            assert!(shown.contains(&r.label), "{what}: {} ({:?}) isn't on the frame\n{shown}", r.id, r.label);
        }
        rows
    };
    let side = check(&mut ed, "the side panel");
    let ids: Vec<&str> = side.iter().map(|r| r.id.as_str()).collect();
    assert!(ids.contains(&"files:src/") && ids.contains(&"files:src/panels.rs"), "content ids: {ids:?}");
    let slot = ed.ui_state().panels.slot.unwrap();
    assert_eq!((slot.tabs[0].0.as_str(), slot.front.as_deref()), ("check", Some("task")));
    // The review's rail takes the side panel's place
    let file = |rel: &str| crate::codereview::File {
        rel: rel.into(),
        path: dir.join(rel),
        added: 1,
        removed: 0,
        new: false,
        gone: false,
        binary: false,
    };
    let review = crate::codereview::CodeReview::for_test(
        crate::codereview::Base::Head,
        dir.clone(),
        vec![file("src/main.rs"), file("README.md")],
    );
    ed.session.code_review = Some(review);
    let rail = check(&mut ed, "the review's rail");
    assert!(rail.iter().any(|r| r.id == "review:src/main.rs" && r.label == "main.rs"), "{rail:?}");
    // A picker over it all
    ed.set_status("first message");
    ed.messages_picker();
    let picker = check(&mut ed, "a picker");
    assert!(picker.iter().any(|r| r.label.contains("first message")), "{picker:?}");
    // (found by this test: the rail's rows were drawn, then covered by the picker — and still listed)
    let p = ed.ui_state().panels;
    assert!(p.covered && p.review.unwrap().rows.is_empty(), "the rail under the picker lists no rows");
    std::fs::remove_dir_all(&dir).ok();
}

/// A run keeps one id from its ask to its end: the user's task waiting in line behind Claude's command is
/// `run#N` queued, and the same `run#N` once it runs.
#[test]
fn a_run_keeps_its_id_from_the_line_to_its_end() {
    let mut ed = editor();
    let task = |name: &str| crate::task::Task { name: name.into(), cmd: "true".into(), cwd: ed_root() };
    let mut claudes = crate::task::Run::for_test(task("claude's"));
    (claudes.state, claudes.by_claude) = (crate::task::State::Running, true);
    ed.session.task_run = Some(claudes);
    ed.task_start(task("check"));
    let runs = ed.ui_runs();
    let queued = runs.iter().find(|r| r.state == "queued").expect("waiting in line");
    assert_eq!((queued.of, queued.label.as_str(), queued.waiting.as_deref()), ("user", "check", Some("1st")));
    let id = queued.id.clone();
    // Claude's ends; the line moves
    ed.session.task_run.as_mut().unwrap().state = crate::task::State::Passed;
    crate::queued::run_next(&mut ed);
    let end = Instant::now() + Duration::from_secs(10);
    while !ed.session.task_run.as_ref().is_some_and(|r| r.task.name == "check") {
        assert!(Instant::now() < end, "the task started");
        if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(10)) {
            ed.handle_event(ev);
        }
    }
    let r = ed.ui_runs().into_iter().find(|r| r.label == "check").unwrap();
    assert_eq!(r.id, id, "the same run");
    assert_ne!(r.state, "queued");
}

fn ed_root() -> PathBuf {
    std::fs::canonicalize(std::env::temp_dir()).unwrap()
}

/// The gone ring: a card answered, one dropped as its turn ends, one that went without a word — each with
/// how; the newest first in the state.
#[test]
fn the_gone_ring_says_how_each_card_went() {
    let mut ed = editor();
    light_card(&mut ed);
    frame(&mut ed);
    feed(&mut ed, &["n"]);
    frame(&mut ed);
    weighty_card(&mut ed);
    frame(&mut ed);
    ed.drop_cards(|w| matches!(w, What::ChangeSet { .. }));
    frame(&mut ed);
    light_card(&mut ed);
    frame(&mut ed);
    ed.offers.clear();
    frame(&mut ed);
    let gone: Vec<(String, String)> =
        ed.ui_state().gone.iter().map(|g| (g.title.clone(), g.how.clone())).collect();
    assert_eq!(
        gone,
        vec![
            ("End the review?".into(), "gone".into()),
            ("Claude changes 3 files".into(), "dropped — its turn ended".into()),
            ("End the review?".into(), "answered n".into()),
        ]
    );
    assert!(ed.ui_state().text().contains("last gone: card#3 \"End the review?\" — gone"));
}

/// A watcher (as a socket connection is one) is told what changes, a frame at a time — where the keys
/// went, a card up and gone (and how), a toast, a run's steps — each with its frame, input and a `said`
/// line; nothing between frames, and nothing when nothing changed.
#[test]
fn a_watcher_is_told_what_changes() {
    let mut ed = editor();
    frame(&mut ed);
    let (tx, rx) = std::sync::mpsc::sync_channel(watch::UNREAD);
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    ed.ui_watch_add(watch::Watcher { tx, kinds: Vec::new(), overflowed: flag });
    let events = |rx: &std::sync::mpsc::Receiver<String>| -> Vec<Value> {
        rx.try_iter().map(|l| serde_json::from_str(&l).unwrap()).collect()
    };
    frame(&mut ed);
    assert!(events(&rx).is_empty(), "nothing changed, nothing said");
    feed(&mut ed, &[":"]);
    assert!(events(&rx).is_empty(), "nothing before its frame");
    frame(&mut ed);
    let e = events(&rx);
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!((e[0]["event"].as_str(), e[0]["to"].as_str()), (Some("keys"), Some("prompt")));
    assert_eq!((e[0]["frame"].as_u64(), e[0]["input"].as_u64()), (Some(ed.frames), Some(ed.input_seq)));
    assert_eq!(e[0]["said"], "keys → prompt (text)");
    feed(&mut ed, &["<esc>"]);
    light_card(&mut ed);
    let trace = crate::trace::RunTrace::new("task: check");
    trace.step("spawned");
    frame(&mut ed);
    let e = events(&rx);
    let kinds: Vec<&str> = e.iter().filter_map(|v| v["event"].as_str()).collect();
    assert_eq!(kinds, vec!["run", "keys", "card-up"], "{e:?}");
    assert_eq!(e[0]["said"], format!("run#{} task: check — spawned", trace.id()));
    assert!(e[2]["said"].as_str().unwrap().contains("End the review? — y end it · n keep reviewing"));
    feed(&mut ed, &["y"]);
    ed.set_warning("a.rs changed on disk");
    frame(&mut ed);
    let e = events(&rx);
    let gone = e.iter().find(|v| v["event"] == "card-gone").expect("card-gone");
    assert_eq!(gone["how"], "answered y");
    assert!(e.iter().any(|v| v["event"] == "toast" && v["said"] == "warning: a.rs changed on disk"), "{e:?}");
    // Kinds asked for only
    let (tx, only) = std::sync::mpsc::sync_channel(watch::UNREAD);
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    ed.ui_watch_add(watch::Watcher { tx, kinds: vec!["cards".into()], overflowed: flag });
    feed(&mut ed, &[":"]);
    light_card(&mut ed);
    frame(&mut ed);
    let e = events(&only);
    assert!(e.iter().all(|v| v["event"] == "card-up") && !e.is_empty(), "{e:?}");
}

/// What a session's Claude costs is a kind of its own (`usage`): said when a turn's result comes and when
/// the cache goes cold — with the numbers the state has — and not to a watcher of `sessions` only.
#[test]
fn a_watcher_of_usage_hears_a_turn_and_the_cache_going_cold() {
    let mut ed = editor();
    frame(&mut ed);
    let watcher = |ed: &mut Editor, kinds: &[&str]| {
        let (tx, rx) = std::sync::mpsc::sync_channel(watch::UNREAD);
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        ed.ui_watch_add(watch::Watcher {
            tx,
            kinds: kinds.iter().map(|k| k.to_string()).collect(),
            overflowed: flag,
        });
        rx
    };
    let (usage, sessions) = (watcher(&mut ed, &["usage"]), watcher(&mut ed, &["sessions"]));
    usage_turn(&mut ed.session.usage);
    watch::usage(&mut ed, "turn");
    frame(&mut ed);
    let e: Vec<Value> = usage.try_iter().map(|l| serde_json::from_str(&l).unwrap()).collect();
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(
        (&e[0]["event"], &e[0]["what"], &e[0]["session"]),
        (&json!("usage"), &json!("turn"), &json!("s0"))
    );
    assert_eq!(e[0]["last_turn"]["cost"], 0.0213);
    assert!(e[0]["said"].as_str().unwrap().contains("cache warm 60m (1h) · context 6% of 200k"), "{e:?}");
    // At its TTL: cold, said once
    let at = ed.session.usage.cold_at().unwrap();
    assert_eq!(crate::usage::woke_at(&mut ed.session.usage, at), crate::usage::Woke::Cold);
    watch::usage(&mut ed, "cold");
    frame(&mut ed);
    let e: Vec<Value> = usage.try_iter().map(|l| serde_json::from_str(&l).unwrap()).collect();
    assert_eq!(e.iter().map(|v| v["what"].as_str().unwrap()).collect::<Vec<_>>(), ["cold"]);
    assert!(sessions.try_iter().next().is_none(), "a watcher of sessions isn't told what they cost");
}

/// A Landlock run's sandbox (ABI 8) in a keeper born with `ns`, `served` — or not: `pidfd_getfd` refused.
fn landlock_run(ns: crate::sandbox::guarantees::Ns, served: bool) -> crate::sandbox::guarantees::Guarantees {
    use crate::sandbox::guarantees::PIDFD_REFUSED;
    crate::sandbox::guarantees::Guarantees::landlock(crate::sandbox::guarantees::Found {
        ns,
        served: if served { Ok(()) } else { Err(PIDFD_REFUSED.into()) },
        abi: 8,
        ptys: None,
        lo_missing: Vec::new(),
        short: None,
        brief: None,
        hint: None,
        network: false,
    })
}

/// Keeper 9d: a session's `sandbox` is its last sandboxed run's — null before one, then the tier, ABI, the keeper's
/// namespaces, the network's way, what's kept and shared, why not the top tier and 9f's `hint` (null) — and a
/// watcher of `sandbox` hears it once each time it changes (the first run, a keeper born with other namespaces, a
/// policy opening ports), never for a run like the last, nor a watcher of `sessions`. Claude reads its own
/// session's in its `ui` answer, why included, and no other's; the context's `ui:` line doesn't carry it.
#[test]
fn tarae_state_says_the_sandbox() {
    use crate::sandbox::guarantees::{Guarantees, Ns};
    let mut ed = editor();
    frame(&mut ed);
    assert_eq!(ed.ui_state().json()["sessions"][0]["sandbox"], Value::Null, "no sandboxed run yet");
    let watcher = |ed: &mut Editor, kind: &str| {
        let (tx, rx) = std::sync::mpsc::sync_channel(watch::UNREAD);
        let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        ed.ui_watch_add(watch::Watcher { tx, kinds: vec![kind.into()], overflowed: flag });
        rx
    };
    let (sandbox, sessions) = (watcher(&mut ed, "sandbox"), watcher(&mut ed, "sessions"));
    let events = |ed: &mut Editor| -> Vec<Value> {
        frame(ed);
        sandbox.try_iter().map(|l| serde_json::from_str(&l).unwrap()).collect()
    };
    // The first run: its keeper has every namespace (its network by tarae's proxy)
    let own = landlock_run(Ns::session(true, true), true);
    let line = "tier own · Landlock ABI 8 · loopback its own · network closed · shared nothing";
    watch::sandbox(&mut ed, own.clone());
    let e = events(&mut ed);
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(
        (&e[0]["event"], &e[0]["session"], &e[0]["was"]),
        (&json!("sandbox"), &json!("s0"), &Value::Null)
    );
    assert_eq!(e[0]["said"], format!("s0 {}: sandbox {line}", ed.session.label()));
    let kept = ["files", "processes", "signals", "ipc", "shm", "ptys", "loopback"].map(|p| match p {
        "files" => json!({ "part": p, "how": "but the temp folders" }),
        "loopback" => json!({ "part": p, "how": "its own" }),
        _ => json!({ "part": p }),
    });
    assert_eq!(
        ed.ui_state().json()["sessions"][0]["sandbox"],
        json!({
            "by": "landlock", "tier": "own", "abi": 8, "partial": false,
            "namespaces": { "user": true, "net": true, "pid": true, "shm": true, "netns": true },
            "network": { "open": false, "asked": false, "listed": false, "way": "proxy" }, "kept": kept, "shared": [],
            "attributes": { "mode_times": "where-writes", "xattrs": "where-writes", "owner_flags": "never" },
            "why": null, "hint": null, "line": line,
        })
    );
    // A run like the last: nothing said
    watch::sandbox(&mut ed, own.clone());
    assert_eq!(events(&mut ed), Vec::<Value>::new());
    // A keeper born with no namespaces and not served — pidfd_getfd refused: one event, offline, said why
    watch::sandbox(&mut ed, landlock_run(Ns::NONE, false));
    let e = events(&mut ed);
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(
        (&e[0]["was"], &e[0]["tier"], &e[0]["namespaces"]["netns"]),
        (&json!(line), &json!("offline"), &json!(false))
    );
    assert_eq!(e[0]["network"]["way"], "no-socket");
    let why = format!("tarae can't serve its calls: {}", crate::sandbox::guarantees::PIDFD_REFUSED);
    assert_eq!(e[0]["why"], why.as_str());
    // macOS: Seatbelt's (no tier, ABI or namespaces); a policy that opens ports, one event — its loopback shared
    watch::sandbox(&mut ed, Guarantees::seatbelt(false, false));
    assert_eq!(events(&mut ed).len(), 1);
    watch::sandbox(&mut ed, Guarantees::seatbelt(false, false));
    assert_eq!(events(&mut ed), Vec::<Value>::new());
    watch::sandbox(&mut ed, Guarantees::seatbelt(true, false));
    let e = events(&mut ed);
    assert_eq!(e.len(), 1, "{e:?}");
    assert_eq!(
        (&e[0]["by"], &e[0]["tier"], &e[0]["abi"], &e[0]["namespaces"]),
        (&json!("seatbelt"), &Value::Null, &Value::Null, &Value::Null)
    );
    assert!(
        e[0]["shared"]
            .as_array()
            .unwrap()
            .contains(&json!({ "part": "loopback", "how": "the ports it opens, whoever's" })),
        "{e:?}"
    );
    assert!(sessions.try_iter().next().is_none(), "a watcher of sessions isn't told the sandbox");
    // The text forms; Claude's level: its own session's, why included — not another's, not in the `ui:` line
    let id = ed.session_add(std::env::temp_dir().join("tarae-uistate-sandbox"));
    ed.session_by_id(id).unwrap().sandbox = Some(own);
    ed.session.sandbox = Some(landlock_run(Ns::NONE, false));
    let mine = ed.session.sandbox.clone().unwrap();
    frame(&mut ed);
    ed.session_by_id(id).unwrap().sandbox = Some(Guarantees::seatbelt(false, false));
    let text = ed.ui_state().text();
    let seatbelt = "sandbox s1: Seatbelt · loopback none of its ports open · network closed · shared POSIX shared \
                    memory\n";
    assert!(text.contains(seatbelt), "{text}");
    ed.session_by_id(id).unwrap().sandbox = Some(landlock_run(Ns::session(true, true), true));
    let text = ed.ui_state().text();
    assert!(
        text.contains(&format!("sandbox s0: {}\n", mine.line()))
            && text.contains(&format!("sandbox s1: {line}\n")),
        "{text}"
    );
    let c = ed.ui_state_claude();
    let text = c.text_claude(false);
    let said = format!("sandbox: {} — what's missing: {}\n", mine.line(), mine.why.as_ref().unwrap());
    assert!(text.contains(&said) && !text.contains(line), "{text}");
    assert_eq!(c.json()["sessions"][1]["sandbox"], Value::Null, "another session's: not Claude's");
    assert!(c.context_line().is_none_or(|(l, _)| !l.contains("sandbox")), "{:?}", c.context_line());
}

/// A watcher that doesn't read is let go once `UNREAD` events wait for it — its flag says why (its
/// connection's last line); the others go on, and with none left, nothing more is kept.
#[test]
fn a_watcher_that_doesnt_read_is_let_go() {
    let mut ed = editor();
    frame(&mut ed);
    let (tx, _unread) = std::sync::mpsc::sync_channel(watch::UNREAD);
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    ed.ui_watch_add(watch::Watcher { tx, kinds: Vec::new(), overflowed: flag.clone() });
    for i in 0..watch::UNREAD + 10 {
        ed.set_status(format!("message {i}"));
        frame(&mut ed);
        if !ed.ui_watch.watching() {
            break;
        }
    }
    assert!(!ed.ui_watch.watching() && flag.load(std::sync::atomic::Ordering::Relaxed), "let go, and why");
    assert!(
        crate::trace::steps_taken().is_empty() && {
            crate::trace::RunTrace::new("x").step("y");
            true
        }
    );
    assert!(crate::trace::steps_taken().is_empty(), "nobody watching: a run's steps aren't kept");
}

// ── Measured ─────────────────────────────────────────────────────────────

/// The state on a busy scene — 3 sessions, 3 cards (2 up, 1 parked), an edit's review open, a run in the
/// slot, toasts — built, as JSON and as text: how long (median of 200), how big, and the text's tokens
/// (estimated: a token per 4 bytes of English/JSON — no tokenizer here). For `cargo perf`.
#[test]
fn perf_ui_state_busy_scene() {
    if crate::test_results::unoptimized("uistate::tests::perf_ui_state_busy_scene") {
        return;
    }
    let mut ed = busy_scene();
    let times = |f: &dyn Fn(&Editor) -> usize| {
        let mut v: Vec<Duration> = (0..200)
            .map(|_| {
                let t = Instant::now();
                std::hint::black_box(f(&ed));
                t.elapsed()
            })
            .collect();
        v.sort();
        v[v.len() / 2]
    };
    let build = times(&|ed| ed.ui_state().cards.len());
    let json = times(&|ed| ed.ui_state().json().to_string().len());
    let text = times(&|ed| ed.ui_state().text().len());
    let s = ed.ui_state();
    let (jb, tb) = (s.json().to_string().len(), s.text().len());
    // Claude's level: its `ui` answer, and the context's `ui:` line
    let claude = times(&|ed| ed.ui_state_claude().text_claude(false).len());
    let c = ed.ui_state_claude();
    let (cb, lb) = (c.text_claude(false).len(), c.context_line().map_or(0, |l| l.0.len()));
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    let lines = [
        format!("PERF ui_state_claude_text_us {:.1} budget=1000 noise=5", us(claude)),
        format!("PERF ui_state_claude_text_bytes {cb} budget=1536 noise=1"),
        format!("PERF ui_state_context_line_bytes {lb} budget=512 noise=1"),
        format!("PERF ui_state_build_us {:.1} budget=1000 noise=5", us(build)),
        format!("PERF ui_state_json_us {:.1} budget=1000 noise=5", us(json)),
        format!("PERF ui_state_text_us {:.1} budget=1000 noise=5", us(text)),
        format!("PERF ui_state_json_bytes {jb} budget=16384 noise=1"),
        format!("PERF ui_state_text_bytes {tb} budget=1536 noise=1"),
        format!("PERF ui_state_json_tokens_est {} budget=4096 noise=1", jb.div_ceil(4)),
        format!("PERF ui_state_text_tokens_est {} budget=384 noise=1", tb.div_ceil(4)),
    ];
    let _ = std::io::Write::write_all(&mut std::io::stderr(), (lines.join("\n") + "\n").as_bytes());
    frame(&mut ed);
}

/// A session behind streaming its answer while someone watches (watchlate: watchers are told after a batch that
/// draws nothing too, the whole state built and diffed): what one quiet batch costs — a word of the answer, as
/// the main loop takes it (`Event::For` → the chat's stream → `term::batch_done`), ~40 a second at
/// `llm::pump`'s 25 ms — with no watcher, with one of every kind, and a frame of the same scene beside them.
/// The busy scene, median of 200. For `cargo perf`. (2026-10-19, M-series, perf profile: 1.8 µs alone, 10.1
/// watched, a frame 85.6.) Its budget is a frame's worth: past it, build only what the watchers' kinds need.
#[test]
fn perf_ui_state_quiet_batch_watched() {
    if crate::test_results::unoptimized("uistate::tests::perf_ui_state_quiet_batch_watched") {
        return;
    }
    let mut ed = busy_scene();
    let behind = ed.sessions.iter().find(|l| l.name == "fix-login").unwrap().id;
    let generation = {
        let mut c = crate::chat::Chat::new();
        // (a turn under way: its words come in)
        c.state = crate::chat::State::Thinking;
        let g = c.generation();
        ed.session_by_id(behind).unwrap().chat = Some(c);
        g
    };
    frame(&mut ed);
    let batch = move |ed: &mut Editor| {
        let word = move |ed: &mut Editor| {
            crate::chat::stream_event(ed, generation, crate::llm::StreamEv::Text("word ".into()))
        };
        let unchanged = crate::term::handle_noting(ed, crate::event::Event::For(behind, Box::new(word)));
        assert!(!crate::term::batch_done(ed, unchanged), "a word behind draws nothing");
    };
    let median = |ed: &mut Editor, f: &dyn Fn(&mut Editor)| {
        f(ed);
        let mut v: Vec<Duration> = (0..200)
            .map(|_| {
                let t = Instant::now();
                f(ed);
                t.elapsed()
            })
            .collect();
        v.sort();
        v[v.len() / 2].as_secs_f64() * 1e6
    };
    let alone = median(&mut ed, &batch);
    let (tx, rx) = std::sync::mpsc::sync_channel(watch::UNREAD);
    let flag = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    ed.ui_watch_add(watch::Watcher { tx, kinds: Vec::new(), overflowed: flag });
    let watched = median(&mut ed, &batch);
    let told = rx.try_iter().count();
    assert_eq!(told, 0, "a word behind changes nothing a watcher hears");
    let drawn = median(&mut ed, &|ed: &mut Editor| frame(ed));
    let lines = [
        format!("PERF ui_state_quiet_batch_us {alone:.1} budget=20 noise=5"),
        format!("PERF ui_state_quiet_batch_watched_us {watched:.1} budget=85 noise=5"),
        format!("PERF ui_state_busy_scene_frame_us {drawn:.1} budget=5000 noise=5"),
    ];
    let _ = std::io::Write::write_all(&mut std::io::stderr(), (lines.join("\n") + "\n").as_bytes());
}

/// What a `ui` call costs Claude on the busy scene (3 sessions, 3 cards, an edit's review, a task, toasts):
/// its answer within the design's ~1.5 KB, the context's line a fraction of it — and nothing of the other
/// sessions' cards in either.
#[test]
fn claudes_answer_stays_small_on_a_busy_scene() {
    let ed = busy_scene();
    let c = ed.ui_state_claude();
    let text = c.text_claude(false);
    let (line, _) = c.context_line().expect("cards and toasts are up");
    assert!(text.len() <= 1536, "{} bytes:\n{text}", text.len());
    assert!(line.len() <= 512, "{} bytes: {line}", line.len());
    assert!(!text.contains("cargo test -p tarae") && !line.contains("cargo test"), "{text}\n{line}");
    // (its sandbox: one line of the answer, nothing in the context's line every request reads — 9d)
    let mut bare = busy_scene();
    bare.session.sandbox = None;
    let b = bare.ui_state_claude();
    let sandbox = text.lines().find(|l| l.starts_with("sandbox: ")).expect("its sandbox said");
    assert_eq!(b.text_claude(false).len() + sandbox.len() + 1, text.len(), "{text}");
    assert_eq!(b.context_line().map(|l| l.0), Some(line));
}

fn busy_scene() -> Editor {
    let mut ed = editor();
    for name in ["fix-login", "uistate"] {
        let id = ed.session_add(std::env::temp_dir().join(format!("tarae-uistate-busy-{name}")));
        ed.session_by_id(id).unwrap().name = name.into();
        if name == "uistate" {
            let q = "cargo test -p tarae\n\nIn the project, sandboxed.";
            park(
                &mut ed,
                id,
                Offer::new("Claude wants to run a command".into(), q, "", String::new(), What::ReviewAtEnd),
            );
        }
    }
    edit_review(&mut ed);
    task_panel(&mut ed);
    light_card(&mut ed);
    weighty_card(&mut ed);
    // (its runs' sandbox: a keeper not served — the longest line Claude's answer gets, a fallback's why with it)
    ed.session.sandbox = Some(landlock_run(crate::sandbox::guarantees::Ns::NONE, false));
    ed.set_warning("src/term.rs changed on disk — reloaded");
    ed.set_status("check passed");
    frame(&mut ed);
    ed
}
