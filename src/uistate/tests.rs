use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::*;
use crate::config::Config;
use crate::offer::{Offer, What};

fn editor() -> Editor {
    let mut ed = Editor::new(Config::default());
    ed.docs[0].text = ropey::Rope::from_str("one\ntwo\nthree\n");
    ed.session.root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
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
    let o = Offer::new("End the review?".into(), "2 files not viewed.", "", String::new(), What::ReviewEnd);
    ed.push_offer(o);
}

/// A weighty card whose answers do nothing here: a change set of a chat process that isn't there.
fn weighty_card(ed: &mut Editor) {
    let q = "Claude renames `a` to `b` in 3 files.\n\nOne review for the set.";
    ed.push_offer(Offer::new(
        "Claude changes 3 files".into(),
        q,
        "",
        String::new(),
        What::ChangeSet { generation: 999 },
    ));
}

/// The front card in sight long enough for its answers to count.
fn settle(ed: &mut Editor) {
    if let Some(o) = ed.offers.first() {
        ed.offer_seen.set(Some((o.id, o.rev, Instant::now() - crate::offer::SETTLE * 2)));
    }
}

fn chat(ed: &mut Editor, draft: &str) {
    let mut c = crate::chat::Chat::new();
    c.focused = true;
    c.input = draft.into();
    c.cursor = draft.len();
    ed.session.chat = Some(c);
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
    (t.visible, t.focus) = (true, true);
    ed.session.slot_front = Some(Slot::Terminal);
}

fn rail(ed: &mut Editor) {
    let root = ed.session.root.clone();
    let mut r = crate::codereview::CodeReview::for_test(crate::codereview::Base::Head, root, Vec::new());
    r.focus = true;
    ed.session.code_review = Some(r);
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
    ("the chat, empty", |ed| chat(ed, "")),
    ("the chat, a draft", |ed| chat(ed, "hello")),
    ("the chat minimized while Claude works", chat_minimized),
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
    ("the task panel with tarae's keys", |ed| {
        task_panel(ed);
        ed.bottom_take_keys();
    }),
    ("the terminal's program", terminal),
    ("the terminal with tarae's keys", |ed| {
        terminal(ed);
        ed.bottom_take_keys();
    }),
    ("the terminal's program, a prompt over it", |ed| {
        terminal(ed);
        ed.open_prompt(crate::editor::PromptKind::Command, "");
    }),
    ("a card over the terminal's program", |ed| {
        terminal(ed);
        light_card(ed);
    }),
];

/// Keys a driver sends: answers, commands, text, the prefixes, `C-q`, Esc. (No Enter: in the chat it would
/// start a claude.)
const PROBES: &[&str] =
    &["y", "n", "a", "A", "v", "r", "j", "x", "i", "[", "3", " ", ":", "<C-q>", "<C-w>", "<esc>", "<C-d>"];

/// Every scene × every probe key: the handler that took the key (`took`) is the one `keys_to` said.
/// A model that's off in any scene shows here as `scene · key: model says X, handle_key took Y`.
fn mismatches(scenes: &[Scene], model: impl Fn(&Editor) -> KeysTo) -> Vec<String> {
    let mut off = Vec::new();
    for (name, setup) in scenes {
        for probe in PROBES {
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
    let off = mismatches(SCENES, Editor::keys_to);
    assert!(off.is_empty(), "{} of {}:\n{}", off.len(), SCENES.len() * PROBES.len(), off.join("\n"));
}

/// …and it would see a model that's off. In every scene, each case broken once: what has the keys named
/// as another place; and where it leaves keys to the editor (`space`, `:`, `C-w`), it taking them all —
/// either disagrees with `handle_key` for some probe.
#[test]
fn the_routing_test_sees_each_case_broken() {
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
}

/// In every scene the keys go somewhere on screen — but one: the chat minimized while Claude works (its
/// input isn't drawn, and it has the keys — slip 3).
#[test]
fn the_keys_go_somewhere_on_screen_but_the_minimized_chat() {
    let mut hidden = Vec::new();
    for (name, setup) in SCENES {
        let mut ed = editor();
        setup(&mut ed);
        frame(&mut ed);
        if !ed.keys_to().drawn {
            hidden.push(*name);
        }
    }
    assert_eq!(hidden, vec!["the chat minimized while Claude works"]);
}

/// Found by the routing test, where the status line named one place and the keys went to another: a side
/// panel focused in insert mode (OUTLINE; the keys typed into the code), and a prompt over the terminal's
/// program (TERMINAL; the prompt took them).
#[test]
fn the_status_line_names_where_the_keys_go() {
    use crate::chrome::Keys;
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
/// the keys from tarae's keys on its panel — and so does the status line (`i type in it`).
#[test]
fn slip_2_the_terminal_program_has_the_keys_or_tarae_has_them_on_its_panel() {
    let mut ed = editor();
    terminal(&mut ed);
    frame(&mut ed);
    let k = ed.keys_to();
    assert_eq!((k.region.name(), k.takes, k.drawn), ("terminal-program".to_string(), Takes::Program, true));
    assert_eq!(ed.keys_at(), crate::chrome::Keys::Program);
    assert_eq!(ed.keys_at().hint(), None);
    feed(&mut ed, &["<C-\\>"]);
    ed.bottom_take_keys();
    frame(&mut ed);
    let k = ed.keys_to();
    assert_eq!((k.region.name(), k.takes), ("slot:terminal".to_string(), Takes::Commands));
    assert_eq!(ed.keys_at().hint(), Some(("i", "type in it")));
    let shown = crate::screen::text(ed.last_frame.as_ref().unwrap().0.as_slice(), 120, 40).unwrap();
    let status = shown.lines().find(|l| l.contains(" TERMINAL ")).unwrap_or_default();
    assert!(status.contains("TERMINAL ▌  i type in it"), "{shown}");
}

/// Slip 3: ` W` typed into a chat input that wasn't on screen — minimized while Claude worked. The keys
/// are the chat's, and the state says it isn't drawn.
#[test]
fn slip_3_keys_to_a_chat_input_not_on_screen() {
    let mut ed = editor();
    chat_minimized(&mut ed);
    frame(&mut ed);
    let k = ed.keys_to();
    assert_eq!((k.region, k.takes, k.drawn), (Region::Chat, Takes::Text, false));
    assert_eq!(k.route(key(" ")), Region::Chat, "space is text there, not the menu");
    assert!(ed.ui_state().text().contains("NOT ON SCREEN"));
    // Unfolded: on screen
    ed.session.chat.as_mut().unwrap().unfolded = true;
    frame(&mut ed);
    assert!(ed.keys_to().drawn);
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
    assert_eq!(ed.keys_at(), crate::chrome::Keys::Review);
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
    for s in v["sessions"].as_array_mut().unwrap() {
        s["root"] = json!(Path::new(s["root"].as_str().unwrap()).file_name().unwrap().to_string_lossy());
    }
    v
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
            What::ChangeSet { generation: 7 },
        ),
    );
    edit_review(&mut ed);
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
    ed.ui_asks.push(Ask { after_input: input + 1, json: true, reply: tx.clone(), until });
    ed.ui_asks.push(Ask { after_input: 0, json: false, reply: tx, until });
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

// ── The socket ───────────────────────────────────────────────────────────

/// A short folder of the test's own (a socket's path has 104 bytes at most).
fn sock_dir(name: &str) -> PathBuf {
    let d = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("tu-{}-{name}", std::process::id()));
    std::fs::remove_dir_all(&d).ok();
    std::fs::create_dir_all(&d).unwrap();
    d
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

// ── Measured ─────────────────────────────────────────────────────────────

/// The state on a busy scene — 3 sessions, 3 cards (2 up, 1 parked), an edit's review open, a run in the
/// slot, toasts — built, as JSON and as text: how long (median of 200), how big, and the text's tokens
/// (estimated: a token per 4 bytes of English/JSON — no tokenizer here). For `cargo perf`.
#[test]
fn perf_ui_state_busy_scene() {
    if !cfg!(tarae_optimized) {
        let line = format!(
            "{}uistate::tests::perf_ui_state_busy_scene — built at opt-level 0 (the test profile); `cargo perf` runs it\n",
            crate::test_results::SELF_SKIP
        );
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
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
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    let lines = [
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
    ed.set_warning("src/term.rs changed on disk — reloaded");
    ed.set_status("check passed");
    frame(&mut ed);
    ed
}
