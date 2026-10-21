use std::sync::mpsc::Receiver;

use super::*;
use crate::config::Config;
use crate::offer::{Offer, What};

fn editor() -> Editor {
    let mut ed = Editor::new(Config::default());
    ed.docs[0].text = ropey::Rope::from_str("one\ntwo\nthree\n");
    ed
}

/// A frame, as the main loop draws one after an event.
fn frame(ed: &mut Editor) {
    ed.focus_sync();
    crate::term::render(ed, &mut Vec::new(), 120, 40).unwrap();
}

/// Where the keys go now (`keys.to`).
fn to(ed: &Editor) -> String {
    ed.keys_to().to
}

/// Shuttle `id`'s step of `keys`, expecting `expect` — its answer, and whether any of it landed.
fn keys(ed: &mut Editor, id: u64, keys: &str, expect: Value) -> (Value, bool) {
    run(ed, Step::parse(id, &json!({ "keys": keys, "expect": expect })).unwrap())
}

/// …sent where the keys go now, `keys.to` and nothing else.
fn here(ed: &mut Editor, id: u64, k: &str) -> (Value, bool) {
    let to = to(ed);
    keys(ed, id, k, json!({ "keys": { "to": to } }))
}

fn text(ed: &mut Editor, id: u64, text: &str, expect: Value) -> (Value, bool) {
    run(ed, Step::parse(id, &json!({ "text": text, "expect": expect })).unwrap())
}

fn key(s: &str) -> Key {
    s.parse().unwrap()
}

/// A light card: `y` ends the code review (none here — nothing happens). `yours`: asked by the person's last
/// input — it comes in front of the others. Its id.
fn light_card(ed: &mut Editor, title: &str, yours: bool) -> u64 {
    let what = What::ReviewEnd { session: 0, started: Instant::now() };
    // (the person's command that asked for it came after the card in front: `offer_place`)
    ed.person_seq += u64::from(yours);
    let was = ed.events.set_asked(yours.then_some(ed.input_seq));
    ed.push_offer(Offer::new(title.into(), "2 files not viewed.", "", String::new(), what));
    ed.events.set_asked(was);
    ed.offer_seq
}

/// The front card in sight long enough for its answers to count.
/// The person answers the front card — drawn, and in sight long enough for a weighty one's answer to count.
fn answer_card(ed: &mut Editor, k: &str) {
    frame(ed);
    settle(ed);
    ed.handle_key(key(k));
}

fn settle(ed: &mut Editor) {
    if let Some(o) = ed.offers.first() {
        ed.offer_seen.set(Some((o.id, o.rev, Instant::now() - crate::offer::SETTLE * 2)));
    }
}

/// A hello from process `pid` (started at `started`), the peer being it — and where its answer comes.
fn say_hello(ed: &mut Editor, name: &str, started: u64) -> (For, Receiver<String>) {
    ed.shuttles.above = Some(vec![u32::MAX]);
    let of = For { pid: std::process::id(), started };
    let (tx, rx) = std::sync::mpsc::channel();
    hello(ed, hello_of(name, of, "cargo", vec![of.pid, 4000, 1]), tx);
    (of, rx)
}

/// A hello as the socket's thread makes one (`above`: the editor's, `Shuttles.above`).
fn hello_of(name: &str, of: For, what: &str, line: Vec<u32>) -> Hello {
    Hello {
        name: name.into(),
        of,
        what: what.into(),
        exe: "/usr/local/bin/cargo".into(),
        peer_what: "tarae".into(),
        peer_exe: "/usr/local/bin/tarae".into(),
        line,
        above: Vec::new(),
        config: Vec::new(),
    }
}

fn answered(rx: &Receiver<String>) -> Option<Value> {
    rx.try_recv().ok().map(|s| serde_json::from_str(&s).unwrap())
}

/// A step lands where a person's key would, says where and what took it, and counts as input — not as the
/// person's (`person_seq`: what only a person's look may do goes by it).
#[test]
fn a_step_lands_where_a_persons_key_would() {
    let mut ed = editor();
    let id = connect_for_test(&mut ed, "t", Level::Light);
    frame(&mut ed);
    let (input, person) = (ed.input_seq, ed.person_seq);
    let pane = to(&ed);
    let (said, landed) = here(&mut ed, id, "j");
    assert!(landed, "{said}");
    assert_eq!(
        (said["landed"].as_str(), said["took"].as_str()),
        (Some(pane.as_str()), Some("code")),
        "{said}"
    );
    assert_eq!(ed.cursor_line(), 1, "j moved the cursor");
    assert_eq!((ed.input_seq, ed.person_seq), (input + 1, person), "input, not the person's");
    assert_eq!(ed.shuttles.stepping, None, "the step is over");
    assert_eq!(lit(&ed), Some(format!("j → {pane}").as_str()), "its chip says it");
    assert_eq!(keys_said(&ed), Some(("t".into(), false)));
    assert!(
        crate::uistate::keys_json(&ed.keys_to())["shuttle"] == json!({ "name": "t", "paused": false }),
        "the state says one types"
    );
}

/// A card that waits for the keys to go quiet (friction 60) is freed before a step's check, never between the
/// check and its key: with the quiet passing right there (`AFTER_CHECK`), the shuttle's `y` checked as the code's
/// lands in the code — it never reaches a card it may not answer (a project's trust) unchecked. A step after the
/// keys went quiet meets the card, and its `y` is refused.
#[test]
fn a_card_freed_as_a_step_lands_never_takes_its_key_unchecked() {
    let mut ed = editor();
    ed.keys_hold = true;
    let id = connect_for_test(&mut ed, "t", Level::Light);
    // (keys coming in — the shuttle's last step, say — then the card from elsewhere)
    ed.last_input = Some(crate::clock::now());
    let trust = What::ProjectTrust { hash: [0; 32] };
    ed.push_offer(Offer::new("Trust this project?".into(), "x = 1", "", String::new(), trust));
    frame(&mut ed);
    settle(&mut ed);
    // (keys just came: the frame drawn never eats into the window, however slow the machine)
    ed.last_input = Some(crate::clock::now());
    assert!(ed.card_waits_quiet(), "it waits for the keys");
    let pane = to(&ed);
    AFTER_CHECK.set(Some(crate::offer::KEYS_QUIET * 2));
    let (said, landed) = here(&mut ed, id, "y");
    assert!(landed && said["landed"] == pane.as_str() && said["took"] == "code", "{said}");
    assert_eq!(ed.offers.len(), 1, "the card unanswered");
    // Quiet before the step: the card has the keys, and the shuttle's `y` on it is refused
    ed.last_input = ed.last_input.map(|t| t - crate::offer::KEYS_QUIET * 2);
    let card = to(&ed);
    assert!(card.starts_with("card#"), "the card's: {card}");
    let (said, landed) = here(&mut ed, id, "y");
    assert!(!landed && said["refused"] == "never", "{said}");
    assert_eq!(ed.offers.len(), 1);
}

/// Every step says where it goes — an empty `expect`, or none for the keys, is refused; so is one whose place
/// isn't where the keys go: nothing is typed.
#[test]
fn a_step_that_says_no_place_or_another_is_refused() {
    let mut ed = editor();
    let id = connect_for_test(&mut ed, "t", Level::Light);
    let input = ed.input_seq;
    for expect in [json!({}), json!(null), json!({ "keys": {} }), json!({ "keys": { "to": "chat" } })] {
        let (said, landed) = keys(&mut ed, id, "j", expect.clone());
        assert!(!landed && said["refused"] == "expect", "{expect}: {said}");
        assert_eq!(said["keys"]["to"], to(&ed), "it says where the keys go now");
    }
    let (said, _) = keys(&mut ed, id, "j", json!({ "keys": { "to": "chat" } }));
    assert_eq!(said["why"], format!("the keys go to {}, not chat", to(&ed)));
    assert_eq!((ed.input_seq, ed.cursor_line()), (input, 0), "nothing typed");
    // (a field the keys don't have, an element not in sight)
    let pane = to(&ed);
    let (said, _) = keys(&mut ed, id, "j", json!({ "keys": { "to": pane, "held": true } }));
    assert_eq!(said["why"], "keys.held is false, not true");
    let (said, _) = keys(&mut ed, id, "j", json!({ "keys": { "to": pane }, "card#9": { "rev": 0 } }));
    assert_eq!(said["why"], "no card#9 in sight");
}

/// Slip 1: a script read `card#1`, and as its `y` was on the way `card#2` came in front. The step names the
/// card and its rev: refused, both cards still up — then, written for the card in front, it answers that one.
#[test]
fn a_card_come_in_front_between_the_read_and_the_y_is_not_answered() {
    let mut ed = editor();
    let id = connect_for_test(&mut ed, "t", Level::Light);
    let first = light_card(&mut ed, "End the review?", false);
    frame(&mut ed);
    assert_eq!(to(&ed), format!("card#{first}"));
    let read = json!({ "keys": { "to": format!("card#{first}") }, format!("card#{first}"): { "rev": 0 } });
    // (it comes between: the person's own command asked for it)
    let second = light_card(&mut ed, "End the other review?", true);
    frame(&mut ed);
    assert_eq!(to(&ed), format!("card#{second}"), "in front");
    let (said, landed) = keys(&mut ed, id, "y", read);
    assert!(!landed && said["refused"] == "expect", "{said}");
    assert_eq!(said["why"], format!("the keys go to card#{second}, not card#{first}"));
    assert_eq!(ed.offers.len(), 2, "neither answered");
    // Its rev moved on too (asked again, as it was): refused for that alone
    light_card(&mut ed, "End the other review?", true);
    let stale = json!({ "keys": { "to": format!("card#{second}") }, format!("card#{second}"): { "rev": 0 } });
    let (said, _) = keys(&mut ed, id, "y", stale);
    assert_eq!(said["why"], format!("card#{second}'s rev is 1 — you expected 0"));
    let now = json!({ "keys": { "to": format!("card#{second}") }, format!("card#{second}"): { "rev": 1 } });
    let (said, landed) = keys(&mut ed, id, "y", now);
    assert!(landed, "{said}");
    assert_eq!(said["took"], "card");
    assert_eq!(said["said"], format!("card#{second} went: answered y by shuttle t"), "{said}");
    assert!(ed.offers.iter().any(|o| o.id == first), "the first still up");
}

/// Slip 2: a paste meant for the commit message came before its buffer was open, and went to the pane that had
/// the keys. Naming the pane, its file and that it takes text, it's refused (nothing held for `p` either), then
/// lands once the message is there.
#[test]
fn text_before_the_commit_buffer_opened_waits_for_it() {
    let dir = crate::scratch::Scratch::new("shuttle-commit");
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    std::fs::write(dir.join("COMMIT_EDITMSG"), "").unwrap();
    let mut ed = Editor::with_root(Config::default(), dir.to_path_buf());
    ed.open(&dir.join("a.txt")).unwrap();
    let id = connect_for_test(&mut ed, "t", Level::Light);
    frame(&mut ed);
    let pane = to(&ed);
    let message =
        json!({ "keys": { "to": pane, "takes": "text" }, pane.clone(): { "path": "COMMIT_EDITMSG" } });
    let (said, landed) = text(&mut ed, id, "fix: the thing\n\nWhy.", message.clone());
    assert!(!landed && said["refused"] == "expect", "{said}");
    assert_eq!(said["why"], "keys.takes is commands, not text");
    assert!(!ed.registers.contains_key(&'"'), "nothing held for p");
    // (in insert mode, the wrong file: still refused)
    here(&mut ed, id, "i");
    frame(&mut ed);
    let (said, _) = text(&mut ed, id, "fix: the thing", message.clone());
    assert_eq!(said["why"], format!("{pane}'s path is a.txt — you expected COMMIT_EDITMSG"));
    // The message's buffer comes (the job that opens it landed), in insert mode, drawn: it lands there
    here(&mut ed, id, "esc");
    ed.open(&dir.join("COMMIT_EDITMSG")).unwrap();
    here(&mut ed, id, "i");
    frame(&mut ed);
    let (said, landed) = text(&mut ed, id, "fix: the thing\n\nWhy.", message);
    assert!(landed, "{said}");
    assert_eq!(ed.doc().text.to_string(), "fix: the thing\n\nWhy.");
    assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "one\n");
}

/// Slip 3: `]u` with every file viewed stays put, and a toggle sent next acted on the file in hand. Naming the
/// state it's written for (`viewed: false`), the step is refused — the marks as they were.
#[test]
fn a_review_key_goes_only_from_the_state_it_was_written_for() {
    use crate::codereview::{Base, CodeReview, File};
    let dir = crate::scratch::Scratch::new("shuttle-review");
    std::fs::write(dir.join("a.rs"), "one\ntwo\n").unwrap();
    let mut ed = Editor::with_root(Config::default(), dir.to_path_buf());
    ed.open(&dir.join("a.rs")).unwrap();
    ed.doc_mut().review_base = Some(ropey::Rope::from_str("one\n"));
    let file = File {
        rel: "a.rs".into(),
        path: dir.join("a.rs"),
        added: 1,
        removed: 0,
        new: false,
        gone: false,
        binary: false,
    };
    ed.session.code_review = Some(CodeReview::for_test(Base::Head, dir.to_path_buf(), vec![file]));
    for k in ["space", "v", "m"] {
        ed.handle_key(key(k));
    }
    let viewed = |ed: &Editor| {
        let r = ed.session.code_review.as_ref().unwrap();
        r.viewed(ed, &r.files[0])
    };
    assert!(viewed(&ed), "marked by the person");
    let id = connect_for_test(&mut ed, "t", Level::Light);
    frame(&mut ed);
    // `]u`: every file viewed — it stays
    let (said, landed) = here(&mut ed, id, "] u");
    assert!(landed && said["said"] == "all 1 file viewed", "{said}");
    frame(&mut ed);
    let pane = to(&ed);
    let unviewed =
        json!({ "keys": { "to": pane, "pending": "" }, "review:a.rs": { "viewed": false, "in_hand": true } });
    for k in ["space v m", "space v M"] {
        let (said, landed) = keys(&mut ed, id, k, unviewed.clone());
        assert!(!landed && said["refused"] == "expect", "{k}: {said}");
        assert_eq!(said["why"], "review:a.rs's viewed is true — you expected false");
        assert!(viewed(&ed), "{k}: the mark as it was");
    }
    // Written for the state it's in: it goes
    let marked = json!({ "keys": { "to": pane }, "review:a.rs": { "viewed": true, "in_hand": true } });
    let (said, landed) = keys(&mut ed, id, "space v M", marked);
    assert!(landed, "{said}");
    assert!(!viewed(&ed));
}

/// A sequence stays where it began: `:` opens the command line, so the `q` after it isn't typed there — it stops,
/// said, with what landed counted.
#[test]
fn a_sequence_stops_once_its_keys_would_go_elsewhere() {
    let mut ed = editor();
    let id = connect_for_test(&mut ed, "t", Level::Light);
    let (said, landed) = here(&mut ed, id, ": q");
    assert!(landed, "the : landed");
    assert_eq!((said["refused"].as_str(), said["landed_keys"].as_u64()), (Some("moved"), Some(1)), "{said}");
    assert!(
        said["why"].as_str().unwrap().starts_with("after 1 of 2 keys they go to prompt, not pane#"),
        "{said}"
    );
    assert_eq!(ed.prompt.as_ref().map(|p| p.text.as_str()), Some(""), "q not typed");
    // A prefix stays where it began: `space v` waits in the pane, as which-key shows it
    ed.prompt = None;
    let (said, landed) = here(&mut ed, id, "space v");
    assert!(landed && said["refused"].is_null(), "{said}");
    assert_eq!(ed.keys_to().pending, "space v");
}

/// A key a card takes and refuses (here: one with every key, `C-q`) is never reported as landed: not taken, its
/// refusal said.
#[test]
fn a_key_a_card_swallows_is_not_landed() {
    let mut ed = editor();
    let card = light_card(&mut ed, "End the review?", false);
    ed.handle_key(key("C-q"));
    let id = connect_for_test(&mut ed, "t", Level::Light);
    frame(&mut ed);
    let (said, _) = here(&mut ed, id, "x");
    assert_eq!(said["taken"], false, "{said}");
    assert_eq!(said["to"], format!("card#{card}"));
    assert_eq!(said["said"], "x isn't an answer here");
    assert!(said.get("landed").is_none());
}

/// A weighty card's answers stay the person's for now, and so do the answers that decide what tarae may do from
/// now on (this card's own) — any card's `n` doesn't.
#[test]
fn weighty_and_lasting_answers_stay_yours() {
    let mut ed = editor();
    let id = connect_for_test(&mut ed, "t", Level::Weighty);
    let q = "Claude renames `a` to `b` in 3 files.\n\nOne review for the set.";
    let what = What::ChangeSet { whose: crate::refactor::Whose::Claude(999), ops: false };
    ed.push_offer(Offer::new("Claude changes 3 files".into(), q, "", String::new(), what));
    frame(&mut ed);
    settle(&mut ed);
    let (said, landed) = here(&mut ed, id, "y");
    assert!(!landed && said["refused"] == "weighty", "{said}");
    assert_eq!(ed.offers.len(), 1, "still up");
    let (said, landed) = here(&mut ed, id, "n");
    assert!(
        landed && said["said"].as_str().is_some_and(|s| s.ends_with("answered n by shuttle t")),
        "{said}"
    );
    // This card's own yes is never a shuttle's
    ed.push_offer(Offer::new(TITLE.into(), "x asks to send keys", "", String::new(), What::Shuttle));
    frame(&mut ed);
    let (said, landed) = here(&mut ed, id, "y");
    assert!(!landed && said["refused"] == "never", "{said}");
    // (its `n` is a shuttle's to give — once it's settled, as a person's)
    settle(&mut ed);
    here(&mut ed, id, "n");
    // Nor a lasting answer on a light card (`A` on Claude's screen card), nor keeping what a run made
    let call = crate::tools::Call::test(7);
    ed.push_offer(Offer::new(
        "Claude asks".into(),
        "to see the screen",
        "",
        String::new(),
        What::Screen { call },
    ));
    frame(&mut ed);
    assert!(!ed.offers[0].weighty() && ed.card_takes(&ed.offers[0]).contains(&'A'));
    let (said, landed) = here(&mut ed, id, "A");
    assert!(!landed && said["refused"] == "never", "{said}");
    assert!(said["why"].as_str().unwrap().starts_with("A on card#"), "{said}");
    here(&mut ed, id, "n");
    let then = Box::new(crate::tools::MadeThen::Servers);
    ed.push_offer(Offer::new(
        "Made".into(),
        "build.rs",
        "",
        String::new(),
        What::ProtectedMade { then, made: Vec::new() },
    ));
    frame(&mut ed);
    settle(&mut ed);
    let (said, landed) = here(&mut ed, id, "y");
    assert!(!landed && said["refused"] == "never", "{said}");
    assert_eq!(ed.offers.len(), 1, "still up");
}

/// The error read: a person's key after `ERROR_MIN` lets it fade — a shuttle's isn't someone reading it.
#[test]
fn a_shuttles_key_lets_no_error_go() {
    let mut ed = editor();
    let id = connect_for_test(&mut ed, "t", Level::Light);
    ed.set_error("it broke");
    let at = crate::clock::now() - crate::editor::ERROR_MIN * 2;
    let error = |ed: &mut Editor| ed.toasts.iter_mut().find(|t| t.text == "it broke").unwrap().clone();
    // (shown on its card, read for a while: its time runs)
    let t = ed.toasts.iter_mut().find(|t| t.text == "it broke").unwrap();
    (t.at, t.shown) = (at, true);
    let life = error(&mut ed).life;
    here(&mut ed, id, "j");
    assert_eq!(error(&mut ed).life, life, "still up as long");
    ed.handle_key(key("j"));
    assert!(error(&mut ed).life < life, "the person's key lets it fade");
}

/// Follow: a shuttle moving the cursor isn't you taking over — your own key is.
#[test]
fn a_shuttles_key_doesnt_pause_follow() {
    let mut ed = editor();
    let id = connect_for_test(&mut ed, "t", Level::Light);
    let mut c = crate::chat::Chat::new();
    (c.follow.on, c.focused) = (true, false);
    ed.session.chat = Some(c);
    assert!(to(&ed).starts_with("pane#"), "the code has the keys");
    crate::follow::watch(&mut ed);
    crate::follow::settle(&mut ed);
    let paused = |ed: &Editor| ed.session.chat.as_ref().unwrap().follow.paused;
    assert!(here(&mut ed, id, "j").1);
    crate::follow::watch(&mut ed);
    assert!(!paused(&ed), "still following");
    ed.handle_key(key("j"));
    crate::follow::watch(&mut ed);
    assert!(paused(&ed), "your key took over");
}

/// Claude's level knows a shuttle is connected — never which: not its name, its process's name nor its pid, on the
/// card that asks for it, in the keys, a toast or how a card went (the whole of its JSON and text searched).
#[test]
fn claudes_level_never_names_the_shuttle() {
    let mut ed = editor();
    ed.config.shuttle_keys = Level::Light;
    ed.shuttles.above = Some(vec![u32::MAX]);
    let asker = For { pid: 987_652, started: 5 };
    let (tx, _rx) = std::sync::mpsc::channel();
    hello(&mut ed, hello_of("zq-asker", asker, "zq-what", vec![asker.pid]), tx);
    frame(&mut ed);
    let full = ed.ui_state().json().to_string();
    assert!(full.contains("zq-asker") && full.contains("987652"), "its card names it");
    let claude = ed.ui_state_claude();
    let said = format!("{}\n{}", claude.json(), claude.text_claude(false));
    assert!(!said.contains("zq-") && !said.contains("987652") && said.contains(TITLE), "{said}");
    answer_card(&mut ed, "n");
    // (connected: its process `zq-proc`, pid 987651 — granted already)
    let of = For { pid: 987_651, started: 1 };
    ed.shuttles.grants.push(of);
    let (tx, rx) = std::sync::mpsc::channel();
    hello(&mut ed, hello_of("zq-shuttle", of, "zq-proc", vec![of.pid]), tx);
    let id = answered(&rx).unwrap()["id"].as_u64().unwrap();
    light_card(&mut ed, "End the review?", false);
    frame(&mut ed);
    assert!(here(&mut ed, id, "n").1);
    frame(&mut ed);
    frame(&mut ed);
    let full = ed.ui_state().json().to_string();
    assert!(
        full.contains("answered n by shuttle zq-shuttle") && full.contains("shuttle zq-shuttle connected"),
        "{full}"
    );
    // (its process and pid only in `:messages`: no toast says which program types)
    let toasts = ed.toasts.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join("\n");
    assert!(!toasts.contains("zq-proc") && !toasts.contains("987651"), "{toasts}");
    assert!(ed.messages.iter().any(|m| m.2 == "shuttle zq-shuttle connected: zq-proc, pid 987651"));
    let claude = ed.ui_state_claude();
    let said = format!("{}\n{}", claude.json(), claude.text_claude(false));
    for word in ["zq-shuttle", "zq-proc", "987651"] {
        assert!(!said.contains(word), "{word}: {said}");
    }
    assert!(said.contains("answered n by shuttle") && said.contains("connected"), "{said}");
}

/// The card shows a process's name with only the characters a shuttle's name may have (the program picks it),
/// cut; the executable's path on a line of its own, cut from the left (its end kept), control and reordering
/// marks out; and a start on another day with its date.
#[test]
fn the_grant_card_shows_what_the_program_cant_dress_up() {
    let mut ed = editor();
    ed.config.shuttle_keys = Level::Light;
    ed.shuttles.above = Some(vec![u32::MAX]);
    let of = For { pid: std::process::id(), started: 86_400 * 400 };
    let (tx, _rx) = std::sync::mpsc::channel();
    let mut h = hello_of("t", of, "cl\u{1b}[31maude\u{202e}", vec![of.pid]);
    h.exe = "/opt/x\u{1b}[2J/\u{202e}claude".into();
    hello(&mut ed, h, tx);
    let o = &ed.offers[0];
    let q = &o.question;
    assert!(q.starts_with("t — cl31maude (pid "), "{q}");
    assert!(q.contains(", started 1971-02-0"), "a date: {q}");
    assert_eq!(q.lines().nth(1), Some("program /opt/x[2J/claude"));
    assert_eq!((o.ends.as_slice(), o.decided.clone()), (&[1][..], Some(0..2)), "one row, its end kept");
    let long = "a".repeat(80);
    let (tx, _rx) = std::sync::mpsc::channel();
    let other = For { started: 7, ..of };
    hello(&mut ed, hello_of("t", other, &long, vec![of.pid]), tx);
    let q = &ed.offers.last().unwrap().question;
    assert!(q.starts_with(&format!("t — {}… (pid ", "a".repeat(31))), "cut: {q}");
}

/// A `:` line that changes what tarae may do isn't run from a shuttle's Enter — the line stays, yours to run;
/// another runs.
#[test]
fn a_colon_line_that_loosens_a_guard_is_not_run() {
    let mut ed = editor();
    let id = connect_for_test(&mut ed, "t", Level::Light);
    for line in ["set! editor.scrolloff 3", "set ui.shuttle-keys weighty", "set llm.sandbox false"] {
        here(&mut ed, id, ":");
        let (said, landed) = text(&mut ed, id, line, json!({ "keys": { "to": "prompt" } }));
        assert!(landed, "{said}");
        // (Enter with a modifier submits too)
        // (Enter with a modifier submits too — `editor::prompt_submits`)
        for enter in ["enter", "A-enter", "C-enter", "S-enter"] {
            let (said, landed) = here(&mut ed, id, enter);
            assert!(!landed && said["refused"] == "never", "{line} {enter}: {said}");
            assert_eq!(ed.prompt.as_ref().map(|p| p.text.as_str()), Some(line), "left for you");
        }
        ed.prompt = None;
    }
    assert_eq!(ed.config.shuttle_keys, Level::Light);
    here(&mut ed, id, ":");
    text(&mut ed, id, "set editor.scrolloff 3", json!({ "keys": { "to": "prompt" } }));
    let (said, landed) = here(&mut ed, id, "enter");
    assert!(landed, "{said}");
    assert_eq!(ed.config.scrolloff, 3);
}

/// A shuttle's keys never edit tarae's config: a key or a paste that would land in the buffer of a file in your
/// config folder (reached through a link too), a project's `.tarae.toml` or `.tarae.local.toml` is refused
/// (`config`), the buffer as it was; `:e`/`:o`/a split of one, `:config-open`, `:w` to one, `:wa` with one
/// changed are refused where typed lines run. Another file takes the same keys.
#[test]
fn a_shuttles_keys_never_edit_taraes_config() {
    let scratch = crate::scratch::Scratch::new("shuttle-config");
    let dir = std::fs::canonicalize(&scratch).unwrap();
    let (cfg, proj) = (dir.join("cfg"), dir.join("proj"));
    std::fs::create_dir_all(cfg.join("themes")).unwrap();
    std::fs::create_dir_all(&proj).unwrap();
    // (your config folder through a link, as dotfiles keep it: both forms are its)
    std::os::unix::fs::symlink(&cfg, dir.join("cfg-link")).unwrap();
    assert_eq!(dirs_of(dir.join("cfg-link")), vec![dir.join("cfg-link"), cfg.clone()]);
    let files = [
        cfg.join("config.toml"),
        cfg.join("themes/ink.toml"),
        proj.join(".tarae.toml"),
        proj.join(".tarae.local.toml"),
    ];
    for f in files.iter().chain([&proj.join("a.txt")]) {
        std::fs::write(f, "x = 1\n").unwrap();
    }
    let mut ed = editor();
    ed.session.root = proj.clone();
    // (your change to one, unsaved: `:wa` would write it)
    ed.open(&files[0]).unwrap();
    for k in ["i", "w", "esc"] {
        ed.handle_key(key(k));
    }
    assert!(ed.doc().is_modified());
    let id = connect_for_test(&mut ed, "t", Level::Light);
    ed.shuttles.on.as_mut().unwrap().config = dirs_of(dir.join("cfg-link"));
    for f in &files {
        ed.open(f).unwrap();
        frame(&mut ed);
        let before = ed.doc().text.to_string();
        let (said, landed) = here(&mut ed, id, "i z");
        assert_eq!((said["refused"].as_str(), landed), (Some("config"), false), "{said}");
        assert!(said["why"].as_str().unwrap().starts_with("config: yours to edit"), "{said}");
        let at = to(&ed);
        let (said, landed) = text(&mut ed, id, "z", json!({ "keys": { "to": at } }));
        assert_eq!((said["refused"].as_str(), landed), (Some("config"), false), "{said}");
        assert_eq!(ed.doc().text.to_string(), before, "{}", f.display());
    }
    ed.open(&proj.join("a.txt")).unwrap();
    frame(&mut ed);
    let (said, landed) = here(&mut ed, id, "i z");
    assert!(landed, "another file takes them: {said}");
    here(&mut ed, id, "esc");
    assert_eq!(ed.doc().text.to_string(), "zx = 1\n");
    let lines = [
        format!("e {}", cfg.join("config.toml").display()),
        format!("o {}:1", dir.join("cfg-link/keys.toml").display()),
        format!("vs {}", proj.join(".tarae.local.toml").display()),
        // (a place in it: the name read without `:1`)
        "e .tarae.toml:1".into(),
        "config-open".into(),
        format!("w {}", cfg.join("new.toml").display()),
        "wa".into(),
    ];
    for line in &lines {
        here(&mut ed, id, ":");
        text(&mut ed, id, line, json!({ "keys": { "to": "prompt" } }));
        let (said, landed) = here(&mut ed, id, "enter");
        assert_eq!((said["refused"].as_str(), landed), (Some("config"), false), "{line}: {said}");
        here(&mut ed, id, "esc");
    }
    assert!(!cfg.join("new.toml").exists() && !cfg.join("keys.toml").exists());
    assert_eq!(ed.doc().path.as_deref(), Some(proj.join("a.txt").as_path()), "none of them opened");
    // (a key bound to one is refused where it runs, the step stopped there)
    let keys: toml::Table = toml::from_str("[normal]\nF5 = \":config-open\"").unwrap();
    ed.keymaps.merge_user(&keys, &mut Vec::new());
    let (said, _) = here(&mut ed, id, "F5");
    assert_eq!(said["refused"], "config", "{said}");
    assert_eq!(ed.doc().path.as_deref(), Some(proj.join("a.txt").as_path()));
}

/// The `:` guard sits where typed lines run: a key you bound to a line that saves to your config (or changes a
/// setting a project can't) runs nothing from a shuttle — refused, said, the step stopped; a bound line that
/// changes nothing lasting runs.
#[test]
fn a_key_bound_to_a_lasting_line_runs_nothing_from_a_shuttle() {
    let mut ed = editor();
    let keys: toml::Table = toml::from_str(
        "[normal]\nF9 = \":set! editor.scrolloff 3\"\nF7 = \":set ui.shuttle-keys weighty\"\nF6 = \":session-billing api\"\nF8 = \":set editor.scrolloff 4\"",
    )
    .unwrap();
    let mut warnings = Vec::new();
    ed.keymaps.merge_user(&keys, &mut warnings);
    assert!(warnings.is_empty(), "{warnings:?}");
    let id = connect_for_test(&mut ed, "t", Level::Light);
    let before = ed.config.scrolloff;
    let billing = ed.session.billing;
    for (k, line) in [
        ("F9", "set! editor.scrolloff 3"),
        ("F7", "set ui.shuttle-keys weighty"),
        ("F6", "session-billing api"),
    ] {
        let (said, _) = here(&mut ed, id, &format!("{k} j"));
        assert_eq!(
            (said["refused"].as_str(), said["landed_keys"].as_u64()),
            (Some("never"), Some(1)),
            "{said}"
        );
        assert!(said["why"].as_str().unwrap().starts_with(&format!(":{line} — ")), "{said}");
        assert!(said["why"].as_str().unwrap().ends_with("— not from shuttle t"), "{said}");
    }
    assert_eq!((ed.config.scrolloff, ed.config.shuttle_keys), (before, Level::Light), "nothing ran");
    assert_eq!(ed.session.billing, billing, "who pays, as it was");
    assert!(ed.toasts.iter().any(|t| t.text.contains("a command only your own keys run")), "said");
    assert_eq!(ed.cursor_line(), 0, "the step stopped: no j after it");
    assert!(ed.toasts.iter().any(|t| t.text.contains("saves to your config: yours only")), "said");
    let (said, landed) = here(&mut ed, id, "F8");
    assert!(landed && said["refused"].is_null(), "{said}");
    assert_eq!(ed.config.scrolloff, 4);
}

thread_local!(static EVENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) });

/// A step's keys are an event each, as a person's are: what runs after every event (the focus, the views, the
/// syncs) runs between them, not once after the whole step — and the paste too.
#[test]
fn each_key_of_a_step_is_an_event_of_its_own() {
    let mut ed = editor();
    let id = connect_for_test(&mut ed, "t", Level::Light);
    crate::editor::AFTER_EVENT.set(Some(|_: &mut Editor| EVENTS.set(EVENTS.get() + 1)));
    EVENTS.set(0);
    assert!(here(&mut ed, id, "j j k").1);
    let keys = EVENTS.get();
    here(&mut ed, id, "i");
    EVENTS.set(0);
    let to = to(&ed);
    assert!(text(&mut ed, id, "x", json!({ "keys": { "to": to } })).1);
    let paste = EVENTS.get();
    crate::editor::AFTER_EVENT.set(None);
    assert_eq!((keys, paste), (3, 1));
    assert_eq!(ed.person_seq, 0, "none of them the person's");
}

/// Your key pauses it — never the other way round; it goes on once you've been idle `QUIET` and it asks.
#[test]
fn your_key_pauses_it_until_it_asks_after_a_quiet_while() {
    let mut ed = editor();
    let id = connect_for_test(&mut ed, "t", Level::Light);
    ed.handle_event(crate::event::Event::Key(key("j")));
    assert_eq!(ed.cursor_line(), 1, "your key, handled");
    assert_eq!(keys_said(&ed), Some(("t".into(), true)), "paused");
    let (said, landed) = here(&mut ed, id, "j");
    assert!(!landed && said["refused"] == "paused", "{said}");
    assert!(said["why"].as_str().unwrap().starts_with("you typed at "), "{said}");
    let said = resume(&mut ed, id);
    assert!(
        said["refused"] == "paused" && said["retry_after_ms"].as_u64().is_some_and(|ms| ms <= 5000),
        "{said}"
    );
    // (said once, in :messages)
    ed.handle_event(crate::event::Event::Key(key("k")));
    let paused = ed.messages.iter().filter(|m| m.2.contains("shuttle t paused")).count();
    assert_eq!(paused, 1);
    ed.shuttles.person_at = Some(crate::clock::now() - QUIET - Duration::from_millis(1));
    assert_eq!(resume(&mut ed, id)["shuttle"], "resumed");
    assert_eq!(keys_said(&ed), Some(("t".into(), false)));
    assert!(here(&mut ed, id, "j").1, "it types again");
    assert_eq!(resume(&mut ed, id)["shuttle"], "going");
}

/// A look is a person's: `space v h` from a shuttle marks nothing looked at, and says why.
#[test]
fn a_look_is_a_persons_not_a_shuttles() {
    let mut ed = editor();
    let id = connect_for_test(&mut ed, "t", Level::Light);
    let (said, landed) = here(&mut ed, id, "space v h");
    assert!(landed, "{said}");
    assert_eq!(said["said"], "not looked at — a look is a person's, not shuttle t's keys");
}

/// Without `--for` a grant is the connecting `tarae keys`'s own, until it exits: the next one from the same shell
/// asks again. With `--for` (here its shell) it covers every program that one starts — the next connects with no
/// card — and the card names both programs, by their executables, its reach said in the warning colour.
#[test]
fn a_grant_is_the_connecting_programs_unless_for_widens_it() {
    let mut ed = editor();
    ed.config.shuttle_keys = Level::Light;
    ed.shuttles.above = Some(vec![u32::MAX]);
    let shell = 4100;
    let call = |ed: &mut Editor, peer: u32, of: u32| {
        let mut h = hello_of("claude-code", For { pid: of, started: 1 }, "sh", vec![peer, shell, 1]);
        h.exe = if of == shell { "/bin/sh".into() } else { "/usr/local/bin/tarae".into() };
        let (tx, rx) = std::sync::mpsc::channel();
        hello(ed, h, tx);
        rx
    };
    let cards = |ed: &Editor| ed.offers.iter().filter(|o| o.kind() == "shuttle").count();
    // Its own: the card names it alone, no warning; `y`, then it exits
    let first = call(&mut ed, 4201, 4201);
    assert_eq!(cards(&ed), 1);
    let o = &ed.offers[0];
    assert!(!o.question.contains("Every program") && o.warning.is_none(), "{}", o.question);
    assert!(o.question.contains("y lets it type until it exits"), "{}", o.question);
    answer_card(&mut ed, "y");
    assert_eq!(answered(&first).unwrap()["shuttle"], "connected");
    process_ended(&mut ed, For { pid: 4201, started: 1 });
    // The next `tarae keys` from the same shell: asked again
    let second = call(&mut ed, 4202, 4202);
    assert!(answered(&second).is_none() && cards(&ed) == 1, "asked again");
    answer_card(&mut ed, "n");
    // `--for` its shell: both named, the reach in the warning colour
    let third = call(&mut ed, 4203, shell);
    let o = ed.offers.iter().find(|o| o.kind() == "shuttle").unwrap();
    let lines: Vec<&str> = o.question.lines().collect();
    assert!(lines[0].starts_with("claude-code — tarae (pid 4203) for sh (pid 4100"), "{lines:?}");
    assert_eq!((lines[1], lines[2]), ("program /usr/local/bin/tarae", "for /bin/sh"));
    assert_eq!(lines[3], "Every program sh starts may type into tarae until it ends.");
    assert_eq!((o.warning.clone(), o.ends.as_slice()), (Some(3..4), &[1, 2][..]));
    answer_card(&mut ed, "y");
    let said = answered(&third).unwrap();
    assert_eq!(said["shuttle"], "connected");
    // (it types: its step lands, as the person's own grant meant)
    let id = said["id"].as_u64().unwrap();
    let (step, landed) = here(&mut ed, id, "j");
    assert!(landed && step["refused"].is_null(), "{step}");
    // The next one under it: no card
    let fourth = call(&mut ed, 4204, shell);
    assert_eq!(answered(&fourth).unwrap()["shuttle"], "connected");
    assert_eq!(cards(&ed), 0);
}

/// A step's keys each go in as an event, so the second sees where the first left the keys: `esc` gives the side
/// panel's keys back to the code, and the `j` written for the panel isn't sent there — the step stops, saying
/// where they went; sent for the code, it lands as the person's would.
#[test]
fn a_two_key_step_reads_where_its_first_key_moved_the_keys() {
    let mut ed = editor();
    let id = connect_for_test(&mut ed, "t", Level::Light);
    ed.sidebar_show(crate::sidebar::Mode::Outline);
    frame(&mut ed);
    assert_eq!(to(&ed), "side");
    let (said, _) = keys(&mut ed, id, "esc j", json!({ "keys": { "to": "side" } }));
    assert_eq!((said["refused"].as_str(), said["landed_keys"].as_u64()), (Some("moved"), Some(1)), "{said}");
    let pane = to(&ed);
    assert!(pane.starts_with("pane#"), "the code has them: {pane}");
    assert_eq!(said["why"], format!("after 1 of 2 keys they go to {pane}, not side — the rest not sent"));
    assert_eq!(ed.cursor_line(), 0, "no j in the code");
    assert!(here(&mut ed, id, "j").1);
    assert_eq!(ed.cursor_line(), 1);
}

/// The upkeep between a step's keys is a person's: a change set taken one file at a time puts the next file's
/// review up after the key that ends the one before (`refactor::next`, run after every event) — so `y y` answers
/// both reviews, as the person's two `y`s do, instead of the second finding no review and stopping.
#[test]
fn a_steps_second_key_finds_the_review_its_first_brought_up() {
    use crate::refactor::{FileChange, Op};
    let dir = crate::scratch::Scratch::new("shuttle-set");
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    std::fs::write(dir.join("b.txt"), "two\n").unwrap();
    let mut ed = Editor::with_root(Config::default(), dir.to_path_buf());
    ed.open(&dir.join("a.txt")).unwrap();
    let change = |rel: &str, before: &str, after: &str| FileChange {
        path: dir.join(rel),
        rel: rel.into(),
        before: ropey::Rope::from_str(before),
        after: ropey::Rope::from_str(after),
        places: 1,
        op: Op::Text,
        protected: false,
    };
    let files = vec![change("a.txt", "one\n", "uno\n"), change("b.txt", "two\n", "dos\n")];
    crate::refactor::land_yours(&mut ed, "p".into(), "apply p".into(), files, String::new(), String::new());
    // (the card's `r`, the person's: one file at a time)
    frame(&mut ed);
    settle(&mut ed);
    ed.handle_event(crate::event::Event::Key(key("r")));
    let id = connect_for_test(&mut ed, "t", Level::Light);
    frame(&mut ed);
    assert_eq!(to(&ed), "edit-review", "the first file's review");
    let (said, landed) = here(&mut ed, id, "y y");
    assert!(landed && said["refused"].is_null(), "both reviews answered: {said}");
    assert!(ed.session.review.is_none(), "none left");
}

/// The grant card is the program's to bring up — so a `y` you were typing as it came (a yank) isn't an answer: it
/// settles first, as a weighty card does; drawn and in sight, `y` grants. A card dropped unanswered (its session
/// gone, a restart) tells the hello waiting on it at once.
#[test]
fn the_grant_card_takes_no_y_typed_as_it_came() {
    let mut ed = editor();
    ed.config.shuttle_keys = Level::Light;
    let (_, rx) = say_hello(&mut ed, "claude-code", 41);
    frame(&mut ed);
    ed.handle_key(key("y"));
    assert!(answered(&rx).is_none(), "not answered");
    assert_eq!(ed.offer_said_now(), Some("y came as the card did — not taken; press it again"));
    answer_card(&mut ed, "y");
    assert_eq!(answered(&rx).unwrap()["shuttle"], "connected");
    frame(&mut ed);
    assert!(to(&ed).starts_with("pane#"), "answered, the card gone — the code has the keys: {}", to(&ed));
}

/// A grant card dropped unanswered tells the hello waiting on it at once, said in `:messages` — by every path a
/// card goes: dropped (`drop_cards_for`), parked in a session that ends (`session_close`), lost to a restart
/// (`restarting`, what `restart::exec` calls before the process is replaced).
#[test]
fn a_grant_card_dropped_unanswered_says_so_at_once() {
    let dir = crate::scratch::Scratch::new("shuttle-drop");
    let mut ed = editor();
    ed.config.shuttle_keys = Level::Light;
    let why = |rx: &Receiver<String>| answered(rx).map(|v| v["why"].as_str().unwrap_or_default().to_string());
    // Dropped
    let (_, rx) = say_hello(&mut ed, "one", 41);
    ed.drop_cards_for(|w| matches!(w, What::Shuttle), "dropped");
    ed.handle_event(crate::event::Event::Resize);
    assert_eq!(why(&rx).as_deref(), Some("its card went unanswered — say hello again"));
    assert!(ed.messages.iter().any(|m| m.2.starts_with("shuttle one: its card went unanswered (card#")));
    // Parked in a session that ends
    let (_, rx) = say_hello(&mut ed, "two", 42);
    let other = ed.session_open(dir.path()).unwrap();
    assert!(ed.session_switch(0) && ed.session.id != other);
    let card = ed.offers.iter().position(|o| o.kind() == "shuttle").unwrap();
    let card = ed.offers.remove(card);
    ed.sessions.iter_mut().find(|l| l.id == other).unwrap().parked.push(card);
    ed.handle_event(crate::event::Event::Resize);
    assert_eq!(why(&rx), None, "parked, still up");
    ed.session_close(dir.path());
    ed.handle_event(crate::event::Event::Resize);
    assert_eq!(why(&rx).as_deref(), Some("its card went unanswered — say hello again"));
    // Lost to a restart
    let (_, rx) = say_hello(&mut ed, "three", 43);
    restarting(&mut ed);
    assert_eq!(why(&rx).as_deref(), Some("its card went unanswered — tarae restarts — say hello again"));
}

/// A weighty card's answers stay the person's whatever letter reaches it — `Y` on a run card's group too, a
/// letter its hint row doesn't list (`offer_key` takes it in normal mode).
#[test]
fn a_weighty_cards_group_answer_is_not_a_shuttles() {
    let mut ed = editor();
    let id = connect_for_test(&mut ed, "t", Level::Weighty);
    let q = "Claude renames `a` to `b` in 3 files.";
    let what = What::ChangeSet { whose: crate::refactor::Whose::Claude(999), ops: false };
    ed.push_offer(Offer::new("Claude changes 3 files".into(), q, "", String::new(), what));
    ed.offers[0].like = vec![99];
    frame(&mut ed);
    settle(&mut ed);
    assert!(!ed.card_takes(&ed.offers[0]).contains(&'Y') && ed.offers[0].acts_on('Y'));
    let pane = to(&ed);
    let (said, landed) = keys(&mut ed, id, "Y", json!({ "keys": { "to": pane } }));
    assert!(!landed && said["refused"] == "weighty", "{said}");
    assert!(matches!(ed.offers[0].state, crate::offer::OfferState::Asking), "still asking");
    // (a letter the card doesn't act on goes on to the code)
    assert!(here(&mut ed, id, "j").1);
    assert_eq!(ed.cursor_line(), 1);
    // Another letter no hint row lists: `m` on a resolved rebase's card (its commit's message)
    ed.offers.clear();
    let what = What::RebaseResolved { root: "/nowhere".into(), message: true };
    ed.push_offer(Offer::new("Rebase resolved".into(), "Go on?", "", String::new(), what));
    frame(&mut ed);
    settle(&mut ed);
    assert!(!ed.card_takes(&ed.offers[0]).contains(&'m') && ed.offers[0].acts_on('m'));
    let pane = to(&ed);
    let (said, landed) = keys(&mut ed, id, "m", json!({ "keys": { "to": pane } }));
    assert!(!landed && said["refused"] == "weighty", "{said}");
    assert!(matches!(ed.offers[0].what, What::RebaseResolved { .. }), "still up");
}

/// Every card kind (`offer::KINDS`, whole), built — and again in the states that turn a letter of its on or off
/// (a grouped run, a held review, a branch to end, a target to restart in …). Nothing in them is real: no card
/// here is answered.
fn every_card() -> Vec<Offer> {
    use crate::offer::What as W;
    use std::path::PathBuf;
    let p = || PathBuf::from("/nowhere");
    let sha = || "0".repeat(40);
    let call = crate::tools::Call::test;
    let job = || crate::tools::Job::Command("true".into(), p());
    let run = |keep: bool| crate::tools::RunCard {
        cmd: "true".into(),
        cwd: p(),
        call: call(1),
        shown: Vec::new(),
        unprotected: false,
        reads: Vec::new(),
        grants: Vec::new(),
        keep,
        outside: false,
        socket: None,
        again: None,
        same: None,
        reason: None,
        warned: Vec::new(),
    };
    let blocked = || crate::blocked::BlockedRun {
        job: job(),
        denials: Vec::new(),
        from: None,
        claude: None,
        env: Vec::new(),
        report: None,
        key: None,
        keys: None,
    };
    let look = || crate::reads::Look {
        tool: "Read".into(),
        at: Some(p()),
        pattern: String::new(),
        filter: String::new(),
        given: "/nowhere".into(),
        file: true,
        keys: Vec::new(),
        cut: false,
    };
    let pending = || crate::reads::Pending {
        request_id: "r1".into(),
        generation: 1,
        input: Value::Null,
        look: look(),
        class: crate::cards::Class::Elsewhere,
        since: Instant::now(),
        found: Default::default(),
        repository: None,
        not_whole: None,
        own: None,
        then: None,
    };
    let moved = || crate::rebase::Moved {
        root: p(),
        name: "s".into(),
        branch: "b".into(),
        base: "main".into(),
        moved: 1,
        ahead: 1,
    };
    let held =
        |unsaved: Vec<String>| crate::rebase::Held { moved: moved(), changed: vec!["a".into()], unsaved };
    let making = |billing_ask: Option<String>, others: Vec<(String, String)>| crate::worktree::Making {
        top: p(),
        branch: "b".into(),
        base: "main".into(),
        base_sha: sha(),
        path: p(),
        files: (0, false),
        big: Vec::new(),
        builds: None,
        build_own: None,
        copies: Vec::new(),
        others,
        billing: Default::default(),
        billing_ask,
        billing_note: None,
    };
    let ending = |branch: Option<String>, changed: Vec<String>| crate::worktree::Ending {
        root: p(),
        common: p(),
        name: "s".into(),
        branch,
        tip: sha(),
        base: "main".into(),
        changed,
        ahead: Vec::new(),
        lost: Vec::new(),
        entries: Vec::new(),
        keep: crate::sessionkeep::Plan {
            folders: Vec::new(),
            differ: Vec::new(),
            instructions_to: None,
            not: None,
            main: None,
        },
    };
    let host = |open: bool| crate::hosts::Ask {
        session: 1,
        run: 1,
        host: "example.invalid".into(),
        port: 443,
        process: "curl".into(),
        label: "run#1".into(),
        users: false,
        left: false,
        open,
        hosts: Default::default(),
        also: Vec::new(),
        ended: false,
    };
    let way = crate::fetch::Download {
        url: "https://example.invalid/x".into(),
        version: crate::fetch::Version::Pinned("1".into()),
        file: crate::fetch::Name::Is("x".into()),
        sha256: crate::fetch::Sum::Pinned("0".repeat(64)),
        size: None,
        unpack: crate::fetch::Unpack::File,
        program: "x".into(),
        needs: Vec::new(),
        runtime: None,
        rename: Vec::new(),
    };
    let commit = crate::committool::Commit {
        call: call(2),
        ask: crate::committool::Ask {
            root: p(),
            top: p(),
            parent: None,
            branch: None,
            tree: sha(),
            message: "m".into(),
            paths: Vec::new(),
            files: Vec::new(),
            patch: String::new(),
            staged: Vec::new(),
            partial: Vec::new(),
            unsaved: Vec::new(),
        },
    };
    let stuck = crate::stuck::Stuck {
        slot: crate::sizes::Slot::Test,
        run: 1,
        pid: 1,
        asked: Instant::now(),
        quiet: Duration::from_secs(600),
        cpu: 0.0,
        at: 0,
        reach: None,
    };
    let limit = |resets_at_ms: Option<u64>| crate::billing::LimitAsk {
        session: 0,
        window: "weekly".into(),
        resets_at_ms,
    };
    let then = || crate::install::Then::Lsp;
    let whats = vec![
        W::Grammars { lang: "Lua".into(), names: Vec::new(), then: None },
        W::Fetch { tool: "x".into(), way: Box::new(way), dir: p(), then: then() },
        W::PluginPermissions { plugin: "p".into(), permissions: Vec::new() },
        W::PluginAsk { plugin: "p".into(), asks: Vec::new() },
        W::Install { tool: "x".into(), run: Vec::new(), cwd: p(), then: then() },
        W::Run(Box::new(run(true))),
        W::Run(Box::new(run(false))),
        W::NoSandbox {
            job: job(),
            call: call(3),
            ride: Box::default(),
            kind: crate::tools::Unsandboxed::Missing("none here".into()),
            first: true,
        },
        W::ProtectedMade { then: Box::new(crate::tools::MadeThen::Servers), made: Vec::new() },
        W::ReviewAtEnd,
        W::Read { pending: Box::new(pending()), keep: true, folder: None },
        W::Read { pending: Box::new(pending()), keep: false, folder: None },
        W::Grant { grant: crate::allow::Grant::Read(p()), run: Box::new(blocked()) },
        W::Outside { run: Box::new(blocked()) },
        W::ProjectTrust { hash: [0; 32] },
        W::ClaudeSettings { hash: [0; 32] },
        W::GitTrust(Box::new(crate::gittrust::Ask {
            config: p(),
            place: p(),
            digest: [0; 32],
            items: Vec::new(),
            files: Vec::new(),
            filters: false,
        })),
        W::RepoTrust { root: p(), parts: Vec::new() },
        W::Screen { call: call(4) },
        W::Preflight { root: p(), steps: Vec::new(), set: crate::preflight::Set::Quick },
        W::BranchDelete { top: p(), branch: "b".into(), tip: sha() },
        W::ForcePush(Box::new(crate::gitpanel::ForcePush {
            top: p(),
            branch: "b".into(),
            remote: "origin".into(),
            remote_ref: "refs/heads/b".into(),
            lease: sha(),
            lost: Vec::new(),
            yours: Vec::new(),
        })),
        W::SessionNew(Box::new(making(Some("weekly 84 %".into()), vec![("dev".into(), sha())]))),
        W::SessionNew(Box::new(making(None, Vec::new()))),
        W::SessionEnd(Box::new(ending(Some("b".into()), vec![" M a".into()]))),
        W::SessionEnd(Box::new(ending(None, Vec::new()))),
        W::CloseReviewed { doc: 0, force: false },
        W::ChangeSet { whose: crate::refactor::Whose::Claude(999), ops: false },
        W::Commit(Box::new(commit)),
        W::ReviewEnd { session: 0, started: Instant::now() },
        W::Rebase(Box::new(moved())),
        W::RebaseHeld(Box::new(held(Vec::new()))),
        W::RebaseHeld(Box::new(held(vec!["a".into()]))),
        W::Rebased(Box::new(moved())),
        W::RebaseResolved { root: p(), message: true },
        W::RebaseResolved { root: p(), message: false },
        W::Instructions(Box::new(crate::instructions::Both {
            root: p(),
            rel: "CLAUDE.md".into(),
            base: None,
            here: String::new(),
            main: String::new(),
            left: 0,
        })),
        W::UpTo(Box::new(moved())),
        W::BringQueue(Box::new(crate::bringqueue::Ask {
            ids: Vec::new(),
            names: vec!["s".into()],
            bases: vec!["main".into()],
        })),
        W::Board(Box::new(crate::board::Ask {
            session: 0,
            name: "s".into(),
            conversation: None,
            taken: Box::new(crate::codereview::Taken::for_test("/p".into())),
            angles: Vec::new(),
            estimate: None,
        })),
        W::Hooks(Box::new(crate::hooks::Ask {
            what: "commit".into(),
            base: "main".into(),
            hooks: Vec::new(),
            top: p(),
        })),
        W::Restart(Box::new(crate::restart::Choice::for_test(2, vec!["a test run".into()]))),
        W::Restart(Box::new(crate::restart::Choice::for_test(1, Vec::new()))),
        W::RestartSame { id: 1 },
        W::RestartCancel { id: 1 },
        W::NotesStale { ids: Vec::new() },
        W::NotesClose { by: None, ids: Vec::new() },
        W::Recover(Box::new(crate::recovery::Found::for_test(p()))),
        W::Stuck(Box::new(stuck)),
        W::Host(Box::new(host(true))),
        W::Host(Box::new(host(false))),
        W::Containers(Box::new(crate::containers::Ask::for_test(vec!["c".into()]))),
        W::BillingSwitch { session: 0, to: crate::billing::Billing::Subscription, money: false },
        W::EnvKey { session: 0, message: None },
        W::Limit(Box::new(limit(Some(1)))),
        W::Limit(Box::new(limit(None))),
        W::FeaturesReset,
        W::LeftRunning { runs: Vec::new() },
        W::Shuttle,
    ];
    let mut cards: Vec<Offer> =
        whats.into_iter().map(|w| Offer::new("Card".into(), "q", "", String::new(), w)).collect();
    // (a grouped card's `Y`; a review held for its high-risk changes, `h`)
    let grouped = cards.iter().position(|o| o.kind() == "change-set").expect("one");
    cards[grouped].like = vec![99];
    for kind in ["review-end", "rebased"] {
        let mut o = cards.iter().find(|o| o.kind() == kind).cloned().expect("one");
        o.held = Some("1 high-risk change not looked at".into());
        cards.push(o);
    }
    cards
}

/// `Offer::acts_on` — the letters a shuttle's keys are checked against, and the only ones `offer_key` takes — is
/// every letter `Offer::act` (what `offer_key` does) acts on, card kind by card kind, as the card may stand
/// (settled or not, cut or not): a letter added to one and not the other fails here.
#[test]
fn acts_on_is_every_letter_a_card_acts_on() {
    use crate::offer::Stands;
    let cards = every_card();
    let built: std::collections::BTreeSet<&str> = cards.iter().map(Offer::kind).collect();
    let kinds: std::collections::BTreeSet<&str> = crate::offer::KINDS.iter().copied().collect();
    assert_eq!(built, kinds, "every card kind, built (a new kind: add it to every_card)");
    let stands = [(true, true), (true, false), (false, true), (false, false)]
        .map(|(settled, whole)| Stands { drawn: true, settled, whole });
    let mut wrong = Vec::new();
    for o in &cards {
        for c in ' '..='~' {
            let acts = stands.iter().any(|&st| o.act(c, st).is_some());
            match (acts, o.acts_on(c)) {
                (true, false) => wrong.push(format!("{} · {c}: acted on, not in acts_on", o.kind())),
                (false, true) => wrong.push(format!("{} · {c}: in acts_on, acts on nothing", o.kind())),
                _ => {}
            }
        }
        // (not drawn, a weighty card takes nothing)
        let hidden = Stands { drawn: false, settled: true, whole: true };
        assert!(!o.weighty() || (' '..='~').all(|c| o.act(c, hidden).is_none()), "{}", o.kind());
    }
    assert!(wrong.is_empty(), "acts_on and act differ:\n{}", wrong.join("\n"));
}

/// The first hello from a process asks you on a card, and another hello from it waits on that one; `y` connects
/// both, until its process ends — then a hello asks again, and `n` refuses it.
#[test]
fn the_grant_card_asks_once_for_each_process() {
    let mut ed = editor();
    ed.config.shuttle_keys = Level::Light;
    let (of, first) = say_hello(&mut ed, "claude-code", 41);
    let (_, second) = say_hello(&mut ed, "claude-code", 41);
    let cards = |ed: &Editor| ed.offers.iter().filter(|o| o.kind() == "shuttle").count();
    assert_eq!(cards(&ed), 1, "one card for its process");
    assert!(answered(&first).is_none() && answered(&second).is_none(), "waiting on you");
    assert!(ed.offers[0].question.starts_with("claude-code — cargo (pid "), "{}", ed.offers[0].question);
    answer_card(&mut ed, "y");
    for rx in [&first, &second] {
        assert_eq!(answered(rx).unwrap()["shuttle"], "connected");
    }
    assert_eq!((ed.shuttles.grants.as_slice(), cards(&ed)), (&[of][..], 0));
    // Another connection of it: no card
    let (_, third) = say_hello(&mut ed, "claude-code", 41);
    assert_eq!(answered(&third).unwrap()["shuttle"], "connected");
    // Its process ends: the grant and the shuttle with it — a hello asks again; `n` refuses
    process_ended(&mut ed, of);
    assert!(ed.shuttles.on.is_none() && ed.shuttles.grants.is_empty());
    let (_, again) = say_hello(&mut ed, "claude-code", 41);
    assert_eq!(cards(&ed), 1);
    answer_card(&mut ed, "n");
    assert_eq!(answered(&again).unwrap()["refused"], "no");
    // The same pid started again isn't the process you said yes to
    ed.shuttles.grants.push(of);
    let (_, reused) = say_hello(&mut ed, "claude-code", 42);
    assert!(answered(&reused).is_none() && cards(&ed) == 1, "a card for it");
}

/// A peer's line goes up to pid 1 and keeps it — what tells a re-parented peer (`hello_refused`) — and stops at
/// a parent it can't read or one already in it.
#[test]
fn a_peers_line_keeps_pid_1() {
    let table = |pairs: &'static [(u32, u32)]| move |p: u32| pairs.iter().find(|x| x.0 == p).map(|x| x.1);
    assert_eq!(crate::uisocket::line_up(10, table(&[(10, 9), (9, 1)])), vec![10, 9, 1]);
    assert_eq!(crate::uisocket::line_up(10, table(&[(10, 1)])), vec![10, 1], "re-parented: its parent is 1");
    assert_eq!(crate::uisocket::line_up(10, table(&[(10, 9)])), vec![10, 9], "9's parent not read");
    assert_eq!(crate::uisocket::line_up(10, table(&[(10, 9), (9, 10)])), vec![10, 9]);
    // (`docker exec`'s: a parent outside the namespace reads as 0)
    assert_eq!(crate::uisocket::line_up(10, table(&[(10, 0)])), vec![10]);
}

/// Refused before any card: the setting off, a name that isn't plain, a `for` not among the peer's line, one under
/// what runs the project's code (a task — `:preflight`'s build would find the socket), a second shuttle.
#[test]
fn who_may_not_type() {
    let mut ed = editor();
    ed.shuttles.above = Some(vec![u32::MAX]);
    let hello_as = |ed: &mut Editor, name: &str, of: For, line: Vec<u32>| {
        let (tx, rx) = std::sync::mpsc::channel();
        hello(ed, hello_of(name, of, "sh", line), tx);
        answered(&rx)
    };
    let me = For { pid: std::process::id(), started: 1 };
    let said = hello_as(&mut ed, "t", me, vec![me.pid]).unwrap();
    assert_eq!(
        (said["refused"].as_str(), said["why"].as_str()),
        (Some("off"), Some("ui.shuttle-keys is off — your own config turns it on"))
    );
    ed.config.shuttle_keys = Level::Light;
    let said = hello_as(&mut ed, "t\u{1b}[31m", me, vec![me.pid]).unwrap();
    assert_eq!(said["refused"], "name");
    assert_eq!(hello_as(&mut ed, &"x".repeat(33), me, vec![me.pid]).unwrap()["refused"], "name");
    let said = hello_as(&mut ed, "t", For { pid: 1, started: 1 }, vec![me.pid]).unwrap();
    assert_eq!(said["refused"], "peer", "its for isn't its own line");
    // Re-parented to launchd/init (it daemonized, or forked twice to leave Claude's line): refused, whatever its
    // `for` — its own process too
    for of in [me, For { pid: 1, started: 1 }] {
        let said = hello_as(&mut ed, "t", of, vec![me.pid, 1]).unwrap();
        assert_eq!(
            said["why"],
            "not started from a shell or a program you run — its parent is pid 1: it daemonized or forked twice"
        );
    }
    // (a parent of its own, under pid 1 further up: not refused for it — a card asks)
    assert!(hello_as(&mut ed, "t", me, vec![me.pid, 4243, 1]).is_none());
    answer_card(&mut ed, "n");
    let task = crate::task::Task { name: "check".into(), cmd: "true".into(), cwd: ed.session.root.clone() };
    ed.session.task_run = Some(crate::task::Run::for_test(task).with_pid(4242));
    let said = hello_as(&mut ed, "t", me, vec![me.pid, 4242, 1]).unwrap();
    assert_eq!(said["why"], "under a process that runs the project's code (pid 4242)");
    ed.session.task_run = None;
    // This tarae is 500, under its shell 501, under the terminal 900. Never a `for`: tarae itself, its shell,
    // its terminal — the last from a peer outside tarae too (one the terminal also runs)
    ed.shuttles.above = Some(vec![500, 501, 900]);
    let under = vec![me.pid, 500, 501, 900, 1];
    let said = hello_as(&mut ed, "t", For { pid: 500, started: 1 }, under.clone()).unwrap();
    assert_eq!(
        (said["refused"].as_str(), said["why"].as_str()),
        (Some("for"), Some("for: pid 500 is this tarae"))
    );
    for pid in [501, 900] {
        let said = hello_as(&mut ed, "t", For { pid, started: 1 }, under.clone()).unwrap();
        let why = format!("for: pid {pid} runs this tarae (its shell, its terminal, the login session)");
        assert_eq!(said["why"], why, "{said}");
    }
    let outside = vec![me.pid, 777, 900, 1];
    let said = hello_as(&mut ed, "t", For { pid: 900, started: 1 }, outside.clone()).unwrap();
    assert_eq!(said["refused"], "for", "a peer outside tarae, its for tarae's terminal: {said}");
    // …and a peer under tarae but not from its terminal panel (a hook, a formatter — no list knows them)
    let said = hello_as(&mut ed, "t", me, under).unwrap();
    assert_eq!(said["why"], "under this tarae, but not from its terminal panel", "{said}");
    // (from outside, its own process: nothing refuses it — a card asks)
    let said = hello_as(&mut ed, "t", For { pid: 777, started: 1 }, outside);
    assert!(said.is_none(), "a card asks: {said:?}");
    answer_card(&mut ed, "n");
    ed.shuttles.above = Some(vec![u32::MAX]);
    assert!(ed.offers.is_empty(), "no card for any of them");
    // One at a time: a second, from another process, is refused naming the first
    connect_for_test(&mut ed, "first", Level::Light);
    let other = For { pid: 2, started: 9 };
    let said = hello_as(&mut ed, "second", other, vec![me.pid, 2, 1]).unwrap();
    assert_eq!(said["refused"], "another");
    assert!(said["why"].as_str().unwrap().starts_with("first (pid "), "{said}");
}

/// `:shuttle-end` cuts it off with its grant: its steps are refused as ended, and a hello asks again.
#[test]
fn shuttle_end_ends_it_and_its_grant() {
    let mut ed = editor();
    let id = connect_for_test(&mut ed, "t", Level::Light);
    for k in [":", "s", "h", "u", "t", "t", "l", "e", "-", "e", "n", "d", "enter"] {
        ed.handle_key(key(k));
    }
    assert!(ed.shuttles.on.is_none() && ed.shuttles.grants.is_empty());
    assert!(ed.toasts.iter().any(|t| t.text.starts_with("shuttle t ended")));
    let (said, _) = here(&mut ed, id, "j");
    assert_eq!(said["refused"], "ended");
    // (the setting turned off: its steps refused)
    let id = connect_for_test(&mut ed, "t", Level::Light);
    ed.config.shuttle_keys = Level::Off;
    assert_eq!(here(&mut ed, id, "j").0["refused"], "off");
    assert_eq!(keys_said(&ed), None, "no chip");
}

/// Connected, a shuttle asks for no frame of its own; its step lit on the chip asks for one, when it goes faint.
#[test]
fn a_shuttle_wakes_nothing_but_its_chip_going_faint() {
    let mut ed = editor();
    let id = connect_for_test(&mut ed, "t", Level::Light);
    assert_eq!(lit_wake(&ed), None);
    here(&mut ed, id, "j");
    assert!(lit_wake(&ed).is_some_and(|d| d <= LIT + Duration::from_millis(2)));
    if let Some(s) = ed.shuttles.on.as_mut() {
        s.last.as_mut().unwrap().1 = crate::clock::now() - LIT;
    }
    assert_eq!((lit_wake(&ed), lit(&ed)), (None, None));
}

/// The status line: `◦ shuttle <name>` whether a step is lit or not; the lit step comes after everything else on
/// the left, so lighting it, or its going out, moves nothing before it.
#[test]
fn a_lit_step_moves_nothing_on_the_status_line() {
    let mut ed = editor();
    let id = connect_for_test(&mut ed, "claude-code", Level::Light);
    ed.toasts.clear();
    // (a segment after the chip: the branch)
    ed.session.git_branch = Some("main".into());
    let status = |ed: &mut Editor| {
        frame(ed);
        let text = crate::screen::text(ed.last_frame.as_ref().unwrap().0.as_slice(), 120, 40).unwrap();
        text.lines().last().unwrap_or_default().to_string()
    };
    let unlit = status(&mut ed);
    assert!(unlit.contains("◦ shuttle claude-code"), "{unlit:?}");
    let pane = to(&ed);
    assert!(here(&mut ed, id, "l").1);
    ed.toasts.clear();
    let lit = status(&mut ed);
    let at = lit.find(&format!("l → {pane}")).unwrap_or_else(|| panic!("lit: {lit:?}"));
    // (what's left of it is the line's left as it was — the chip, the branch after it)
    let left = &unlit[..unlit.find("main").expect("the branch") + "main".len()];
    assert!(lit[..at].starts_with(left), "moved:\n{unlit:?}\n{lit:?}");
}

/// A step's request: keys in tarae's notation, or text — one of them.
#[test]
fn a_step_is_keys_or_text() {
    let s =
        Step::parse(3, &json!({ "keys": " space  v m ", "expect": { "keys": { "to": "pane#1" } } })).unwrap();
    assert_eq!(s.input, Input::Keys(vec![key("space"), key("v"), key("m")], "space v m".into()));
    assert!(Step::parse(3, &json!({ "keys": "C-x-y" })).is_err());
    assert!(Step::parse(3, &json!({ "keys": "" })).is_err());
    assert!(Step::parse(3, &json!({ "keys": "y", "text": "y" })).is_err());
    assert_eq!(Step::parse(3, &json!({ "text": "a\nb" })).unwrap().input, Input::Text("a\nb".into()));
}
