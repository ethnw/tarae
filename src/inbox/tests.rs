//! The inbox's order, `C-q` across sessions (#94's case too), the status line's count, `enter`.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use super::NOTHING;
use crate::editor::Editor;
use crate::event::SessionId;
use crate::key::{Code, Key};
use crate::offer::{Offer, What};

/// An editor in `front`, with sessions for `back` and `third` behind it — folders in the test's own temp
/// folder.
fn three(name: &str) -> (Editor, PathBuf, SessionId, SessionId) {
    let dir = crate::scratch::Scratch::new(&format!("inbox-{name}")).into_path();
    for p in ["front", "back", "third"] {
        std::fs::create_dir_all(dir.join(p)).unwrap();
    }
    let dir = std::fs::canonicalize(dir).unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    (ed.session.root, ed.session.name) = (dir.join("front"), "front".into());
    let back = ed.session_open(&dir.join("back")).unwrap();
    let third = ed.session_open(&dir.join("third")).unwrap();
    assert!(ed.session_switch(0));
    (ed, dir, back, third)
}

/// A card of Claude's (it asks to see the screen) that came `ago` seconds ago.
fn claudes(n: u64, ago: u64) -> Offer {
    let what = What::Screen { call: crate::tools::Call::test(n) };
    let mut o = Offer::new(format!("Claude asks {n}"), "to see the screen", "", String::new(), what);
    o.since = Instant::now() - Duration::from_secs(ago);
    o
}

/// A card of tarae's own (the project's settings ask for trust) that came `ago` seconds ago.
fn tarae(ago: u64) -> Offer {
    let what = What::ProjectTrust { hash: [7; 32] };
    let mut o =
        Offer::new("Trust this project's settings?".into(), "its .tarae.toml runs", "", String::new(), what);
    o.since = Instant::now() - Duration::from_secs(ago);
    o
}

fn session(ed: &mut Editor, id: SessionId) -> &mut crate::session::Session {
    std::iter::once(&mut ed.session).chain(&mut ed.sessions).find(|l| l.id == id).expect("the session")
}

/// Card `o` waiting in session `id` (behind), with an id of its own as `push_offer` gives one.
fn park(ed: &mut Editor, id: SessionId, mut o: Offer) {
    ed.offer_seq += 1;
    o.id = ed.offer_seq;
    session(ed, id).parked.push(o);
}

/// Card `o` up in front, keeping when it came (`push_offer` says now).
fn up(ed: &mut Editor, o: Offer) {
    let since = o.since;
    ed.push_offer(o);
    ed.offers.last_mut().unwrap().since = since;
}

fn feed(ed: &mut Editor, keys: &str) {
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        };
        ed.handle_key(key);
    }
}

fn screen(ed: &mut Editor, w: u16, h: u16) -> Vec<String> {
    let mut bytes = Vec::new();
    crate::term::render(ed, &mut bytes, w, h).unwrap();
    let mut vt = vt100::Parser::new(h, w, 0);
    vt.process(&bytes);
    vt.screen().rows(0, w).collect()
}

/// Every session's cards — up in front, stacked, parked behind, Claude's and tarae's — and the edits waiting
/// for their review, in one list, oldest first, whatever session each is in. A card downloading waits for
/// nobody: not in it. The UI state lists the same, in the same order, and every card it lists that waits is
/// in it.
#[test]
fn the_inbox_is_every_sessions_waiting_things_oldest_first() {
    let (mut ed, dir, back, third) = three("order");
    // Up in front: two cards (the stacked one older); parked behind: one in each
    up(&mut ed, claudes(1, 30));
    up(&mut ed, claudes(2, 300));
    park(&mut ed, back, claudes(3, 120));
    park(&mut ed, third, tarae(600));
    // An edit of Claude's waiting for its review, behind
    let mut chat = crate::chat::Chat::new();
    chat.edits.queue.push_back(crate::edits::Pending {
        request_id: "e1".into(),
        tool: "Edit".into(),
        input: serde_json::json!({ "file_path": "src/a.rs" }),
        reason: String::new(),
        since: Instant::now() - Duration::from_secs(60),
        row: None,
        protected: Some(false),
    });
    session(&mut ed, back).chat = Some(chat);
    // One downloading: waits for nobody
    let mut busy = tarae(900);
    busy.state = crate::offer::OfferState::Installing(Instant::now());
    park(&mut ed, back, busy);
    let ids: Vec<String> = ed.inbox().into_iter().map(|e| e.id).collect();
    let card = |o: &Offer| format!("card#{}", o.id);
    let (front_new, front_old) = (card(&ed.offers[0]), card(&ed.offers[1]));
    let parked = |ed: &mut Editor, id| card(&session(ed, id).parked[0]);
    let (b, t) = (parked(&mut ed, back), parked(&mut ed, third));
    let edit = format!("edit#s{back}:e1");
    assert_eq!(ids, [t, front_old, b, edit.clone(), front_new], "oldest first, every session's");
    let e = ed.inbox().into_iter().find(|e| e.id == edit).unwrap();
    assert_eq!((e.kind, e.asks.as_str(), e.up), ("edit", "Claude's edit to src/a.rs", "queued"));
    // The state: the same list in the same order — and it's built on the state's cards
    let state = ed.ui_state();
    assert_eq!(state.inbox.iter().map(|e| &e.id).collect::<Vec<_>>(), ids.iter().collect::<Vec<_>>());
    for c in state.cards.iter().filter(|c| c.state != "installing") {
        assert!(ids.contains(&c.id), "{} waits, so it's in the inbox", c.id);
    }
    assert!(!state.cards.iter().filter(|c| c.state == "installing").any(|c| ids.contains(&c.id)));
    let json = state.json();
    assert_eq!(json["inbox"][0]["id"], ids[0].as_str(), "{json}");
    assert_eq!(json["inbox"][3]["kind"], "edit");
    assert!(
        state.text().contains(&format!("inbox, oldest first: {} (s{third})", ids[0])),
        "{}",
        state.text()
    );
    // The card's count says it, here and elsewhere
    assert_eq!(ed.cards_waiting(), (2, 3));
    std::fs::remove_dir_all(&dir).ok();
}

/// frictions15 #5: the card in front holds `C-q` (it takes this session's oldest first, #198), so what waits
/// behind it would be a silent queue — the card says how many more wait, where (here, or which session) and
/// the key that lists them (the keymap's, never a hardcoded one). Alone, it says nothing of the kind.
#[test]
fn the_front_card_says_how_many_wait_behind_it_and_where() {
    let (mut ed, dir, back, _) = three("behind");
    up(&mut ed, claudes(1, 60));
    let alone = screen(&mut ed, 120, 30).join("\n");
    assert!(!alone.contains("more wait"), "{alone}");
    up(&mut ed, claudes(2, 30));
    park(&mut ed, back, claudes(3, 10));
    let rows = screen(&mut ed, 120, 30).join("\n");
    assert!(rows.contains("2 more wait — 1 here, 1 in back · space I lists them"), "{rows}");
    std::fs::remove_dir_all(&dir).ok();
}

/// `C-q` with nothing waiting in front goes to the one that has waited longest, in any session, and its card
/// takes the keys there (frictions29 19: it took a second `C-q`, and from a chat the key meant for the card was
/// typed into it). #94's case: a card of tarae's parked in a session behind whose turn
/// ended (`done`, its Claude idle) — `C-q` said "no card waits for you" while it waited there; now it goes to
/// it. Nothing anywhere: said.
#[test]
fn c_q_goes_to_the_oldest_in_any_session() {
    let (mut ed, dir, back, third) = three("c-q");
    park(&mut ed, back, claudes(2, 60));
    // (#94: tarae's own card, parked in a session whose turn ended behind)
    park(&mut ed, third, tarae(30));
    session(&mut ed, third).turn_end = Some((Instant::now(), Ok(())));
    // The status line counts all of them, every session's
    let rows = screen(&mut ed, 160, 30);
    assert!(rows[29].contains("▲2 waiting"), "{}", rows[29]);
    // C-q: to back's (the oldest), its card up
    feed(&mut ed, "<C-q>");
    // (front's comes meanwhile, the newest: it waits behind)
    park(&mut ed, 0, claudes(1, 5));
    assert_eq!(ed.session.id, back, "the one that waited longest");
    assert_eq!(ed.offers.first().map(|o| o.title.as_str()), Some("Claude asks 2"));
    assert!(ed.card_keys, "the keys with it");
    assert_eq!(
        ed.keys_to().route(Key::plain(Code::Char('n'))),
        crate::uistate::Region::Card(ed.offers[0].id)
    );
    // `n` answers it there
    feed(&mut ed, "n");
    assert!(ed.offers.is_empty() && !ed.card_keys);
    // C-q: third's — tarae's own card, its turn over (#94) — before front's newer one
    assert!(ed.status.as_ref().is_none_or(|s| s.0 != NOTHING));
    feed(&mut ed, "<C-q>");
    assert_eq!(ed.session.id, third, "#94: found");
    assert_eq!(ed.offers.first().map(|o| o.title.as_str()), Some("Trust this project's settings?"));
    ed.offers.clear();
    // …then front's
    feed(&mut ed, "<C-q>");
    assert_eq!((ed.session.id, ed.offers.first().map(|o| o.title.as_str())), (0, Some("Claude asks 1")));
    assert!(ed.card_keys);
    // Nothing anywhere: said
    ed.offers.clear();
    feed(&mut ed, "<C-q>");
    assert!(ed.status.as_ref().is_some_and(|s| s.0 == NOTHING), "{:?}", ed.status);
    std::fs::remove_dir_all(&dir).ok();
}

/// #198: `C-q` in a session with a card of its own takes that one first — an older one in another session
/// waits (answering here meant `n` to the other first). Then the rest by age. The list keeps its order: the
/// oldest first, whatever session is in front.
#[test]
fn c_q_takes_this_sessions_own_card_first() {
    let (mut ed, dir, back, _) = three("own");
    park(&mut ed, back, claudes(1, 600));
    up(&mut ed, claudes(2, 5));
    assert_eq!(ed.inbox()[0].session_id(), back, "the list: oldest first");
    feed(&mut ed, "<C-q>");
    assert_eq!(ed.session.id, 0, "stays here");
    assert!(ed.card_keys, "this session's card has the keys");
    assert_eq!(ed.offers.first().map(|o| o.title.as_str()), Some("Claude asks 2"));
    feed(&mut ed, "n");
    assert!(ed.offers.is_empty() && !ed.card_keys);
    // Answered: then the rest, by age
    feed(&mut ed, "<C-q>");
    assert_eq!((ed.session.id, ed.offers.first().map(|o| o.title.as_str())), (back, Some("Claude asks 1")));
    std::fs::remove_dir_all(&dir).ok();
}

/// The order is when each came, however it's read: two cards come within a millisecond, their waits read a
/// moment apart in whole milliseconds (`keys_to`, then `handle_key` — each reads every card's at its own
/// instant) — the newer one's read later, rounded up past the older one's. Read so, the order swapped: the
/// routing test's "two cards stacked" now and then (`C-q`: `keys_to` said NextCard, `handle_key` took Card).
#[test]
fn the_order_is_when_each_came_however_it_is_read() {
    let (mut ed, dir, _, _) = three("instant");
    let t = Instant::now() - Duration::from_secs(5);
    let (mut older, mut newer) = (claudes(1, 0), claudes(2, 0));
    (older.since, newer.since) = (t, t + Duration::from_micros(400));
    up(&mut ed, older);
    up(&mut ed, newer);
    let cards = ed.ui_cards();
    let ids = |cards: &[crate::uistate::CardState]| -> Vec<String> {
        ed.inbox_from(cards).into_iter().map(|e| e.id).collect()
    };
    let first = ids(&cards);
    assert_eq!(first[0], format!("card#{}", ed.offers.iter().find(|o| o.since == t).unwrap().id));
    let mut later = cards.clone();
    let most = later.iter().map(|c| c.waited_ms).max().unwrap();
    later.iter_mut().find(|c| c.since > t).unwrap().waited_ms = most + 1;
    assert_eq!(ids(&later), first, "the same order, read a moment later");
    std::fs::remove_dir_all(&dir).ok();
}

/// Review (detsim P1, note 8): a card's wait is on logic's clock — a simulation's here, moved on ten minutes —
/// and an edit's on the real one, a minute: the two are ordered by how long each has waited on its own clock,
/// never as instants of two clocks. The card first.
#[test]
fn the_order_is_by_each_ones_wait_on_its_own_clock() {
    let mut sim =
        crate::sim::Sim::new("inbox_clocks", crate::sim::Choices::seeded(1), Duration::from_secs(30));
    let (mut ed, dir, back, _) = three("clocks");
    let mut card = claudes(1, 0);
    card.since = crate::clock::now();
    up(&mut ed, card);
    let mut chat = crate::chat::Chat::new();
    chat.edits.queue.push_back(crate::edits::Pending {
        request_id: "e1".into(),
        tool: "Edit".into(),
        input: serde_json::json!({ "file_path": "src/a.rs" }),
        reason: String::new(),
        since: Instant::now() - Duration::from_secs(60),
        row: None,
        protected: Some(false),
    });
    session(&mut ed, back).chat = Some(chat);
    sim.wait(Duration::from_secs(600));
    let ids: Vec<String> = ed.inbox().into_iter().map(|e| e.id).collect();
    assert_eq!(ids, [format!("card#{}", ed.offers[0].id), format!("edit#s{back}:e1")]);
    drop(sim);
    std::fs::remove_dir_all(&dir).ok();
}

/// Inside zellij `C-q` is zellij's (uxaudit 7): the code's twin, `space I` `enter`, reaches a card waiting in a
/// session behind as `C-q` does, and the card takes the keys.
#[test]
fn inside_zellij_space_i_enter_reaches_the_card_as_c_q_does() {
    let (mut ed, dir, back, _) = three("zellij");
    ed.zellij = true;
    park(&mut ed, back, claudes(1, 20));
    assert_eq!(ed.key_said("C-q"), "space I");
    feed(&mut ed, "<space>I<ret>");
    assert_eq!(ed.session.id, back, "went to the card's session");
    assert_eq!(ed.offers.first().map(|o| o.title.as_str()), Some("Claude asks 1"));
    feed(&mut ed, "n");
    assert!(ed.offers.is_empty(), "its n answered it: {:?}", ed.messages.last());
    std::fs::remove_dir_all(&dir).ok();
}

/// Inside zellij, `C-q`'s twin is where it goes: a card behind is the inbox's even from the chat (its way back to
/// the code, then `space I`) — never the way back alone, which reaches only a card in front — and a turn that
/// ended, with nothing waiting, the session list's.
#[test]
fn inside_zellij_c_qs_twin_reaches_what_c_q_goes_to() {
    let (mut ed, dir, back, third) = three("zellij-twin");
    ed.zellij = true;
    let mut c = crate::chat::Chat::new();
    c.msgs.push(crate::chat::Msg::new(crate::chat::Role::User, "hi", ""));
    c.msgs.push(crate::chat::Msg::new(crate::chat::Role::Assistant, "hello", ""));
    ed.session.chat = Some(c);
    ed.session.at = crate::session::At::Chat;
    park(&mut ed, back, claudes(1, 20));
    assert_eq!(ed.key_said("C-q"), "esc esc space I");
    screen(&mut ed, 160, 30);
    feed(&mut ed, "<esc>");
    screen(&mut ed, 160, 30);
    feed(&mut ed, "<esc><space>I<ret>");
    assert_eq!(ed.session.id, back, "went to the card's session");
    feed(&mut ed, "n");
    assert!(ed.offers.is_empty(), "its n answered it: {:?}", ed.messages.last());
    // Nothing waits, third's turn ended: the session list
    session(&mut ed, third).turn_end = Some((Instant::now() - Duration::from_secs(30), Ok(())));
    assert_eq!(ed.next_up(), super::Next::Done(third));
    assert_eq!(ed.key_said("C-q"), "space W");
    assert_eq!(ed.c_q_place(crate::zellij::Reach::Sessions), "space W");
    feed(&mut ed, "<space>W");
    assert!(ed.sessions_list_keys(), "the session list");
    std::fs::remove_dir_all(&dir).ok();
}

/// Leaving a session whose card waits, its toast says how to get back to it: `C-q goes there` — inside zellij
/// `space I lists it` (the card is the inbox's from here, not the way back).
#[test]
fn inside_zellij_or_not_the_toast_for_a_session_left_waiting_says_its_key() {
    // (inside zellij by its setting, auto and off — and outside, on)
    let cases = [(false, "auto", false), (true, "auto", true), (true, "off", false), (false, "on", true)];
    for (zellij, setting, twins) in cases {
        let (mut ed, dir, back, _) = three(&format!("zellij-left-{zellij}-{setting}"));
        ed.zellij = zellij;
        ed.config.zellij_hints = setting.into();
        // (the session switched to has its chat holding the keys: a way back made into the toast would say
        // `esc esc space I` — read later, from anywhere, the toast names only the inbox)
        let mut c = crate::chat::Chat::new();
        c.msgs.push(crate::chat::Msg::new(crate::chat::Role::User, "hi", ""));
        session(&mut ed, back).chat = Some(c);
        session(&mut ed, back).at = crate::session::At::Chat;
        up(&mut ed, claudes(1, 20));
        assert!(ed.session_switch(back));
        let says = if twins {
            "front · waits for you — space I lists it"
        } else {
            "front · waits for you — C-q goes there"
        };
        let all: Vec<&str> = ed.toasts.iter().map(|t| t.text.as_str()).collect();
        assert!(all.iter().any(|t| t.ends_with(says)), "{says}: {all:?}");
        std::fs::remove_dir_all(&dir).ok();
    }
}

/// A panel holding the keys (the side panel): `esc` back to the code first, then `space I` — a bare `space I`
/// there could be the panel's own keys.
#[test]
fn inside_zellij_a_panels_twin_of_c_q_goes_back_first() {
    let (mut ed, dir, back, _) = three("zellij-panel");
    ed.zellij = true;
    // (`space e` shows the files panel and gives it the keys)
    feed(&mut ed, "<space>e");
    assert!(matches!(ed.keys_to().region, crate::uistate::Region::Side(_)), "{:?}", ed.keys_to().region);
    park(&mut ed, back, claudes(1, 20));
    assert_eq!(ed.key_said("C-q"), "esc space I");
    feed(&mut ed, "<esc>");
    assert!(matches!(ed.keys_to().region, crate::uistate::Region::Code), "esc: the code");
    feed(&mut ed, "<space>I<ret>");
    assert_eq!(ed.session.id, back, "went to the card's session");
    std::fs::remove_dir_all(&dir).ok();
}

/// frictions29 19: `C-q` from a chat that holds the keys reaches the card — one parked in a session behind (the
/// chat open and focused), one up in front (the chat minimized while Claude works, its input not on screen) —
/// and the card it reaches takes the keys: its `n` answers it, nothing typed into the chat.
#[test]
fn c_q_from_a_chat_holding_the_keys_reaches_the_card() {
    let (mut ed, dir, back, _) = three("chat-cq");
    let chat = |busy: bool| {
        let mut c = crate::chat::Chat::new();
        if busy {
            (c.state, c.follow.on, c.unfolded) = (crate::chat::State::Thinking, true, false);
        }
        c
    };
    // The chat open and focused here; the card parked behind
    ed.session.chat = Some(chat(false));
    ed.session.at = crate::session::At::Chat;
    park(&mut ed, back, claudes(1, 20));
    assert!(ed.ui_state().text().contains("keys → chat"), "{}", ed.ui_state().text());
    // (that session's own chat holding the keys too)
    session(&mut ed, back).chat = Some(chat(false));
    session(&mut ed, back).at = crate::session::At::Chat;
    feed(&mut ed, "<C-q>");
    assert_eq!(ed.session.id, back, "C-q went to the card's session");
    assert_eq!(ed.offers.first().map(|o| o.title.as_str()), Some("Claude asks 1"));
    // …and the card has the keys: one C-q, not one to get there and another to answer
    feed(&mut ed, "n");
    assert!(ed.offers.is_empty(), "its n answered it: {:?}", ed.messages.last());
    assert!(ed.session.chat.as_ref().unwrap().input.is_empty(), "nothing typed into the chat there");
    // Minimized while Claude works, holding the keys: a card up here
    ed.session.chat = Some(chat(true));
    ed.session.at = crate::session::At::Chat;
    assert!(ed.session.chat.as_ref().unwrap().compact());
    up(&mut ed, claudes(2, 5));
    feed(&mut ed, "<C-q>n");
    assert!(ed.offers.is_empty(), "its n answered it: {:?}", ed.messages.last());
    assert!(ed.session.chat.as_ref().unwrap().input.is_empty(), "nothing typed into the chat");
    std::fs::remove_dir_all(&dir).ok();
}

/// frictions29 9: a run quiet only because a filter holds its output (`| tail -60` over a long build) is working,
/// not waiting on anyone — it stays out of the inbox while its processes use the CPU, comes in once they've been
/// idle for `SILENT` too (or half the time its own `timeout` gives it is gone), and says the filter holds it.
#[test]
fn a_quiet_run_still_working_stays_out_of_the_inbox() {
    use crate::task::{Run, State, Task};
    let (mut ed, dir, _, _) = three("filtered");
    let Some(then) = Instant::now().checked_sub(Duration::from_secs(400)) else { return };
    let run = |cmd: &str| {
        let mut r = Run::for_test(Task { name: cmd.into(), cmd: cmd.into(), cwd: dir.join("front") });
        (r.state, r.by_claude, r.last_output, r.started) = (State::Running, true, then, then);
        r
    };
    let said = |ed: &Editor| ed.inbox().into_iter().find(|e| e.kind == "silent-run").map(|e| e.asks);
    // Working: out
    let mut r = run("cargo test 2>&1 | grep -E 'result' | tail -60");
    r.wait.looked(240.0, Instant::now());
    ed.session.task_run = Some(r);
    assert_eq!(said(&ed), None, "a run using the CPU behind its filter");
    // Its own time limit half gone: in, working or not
    let r = ed.session.task_run.as_mut().unwrap();
    r.wait.limited(then, Some(Duration::from_secs(600)));
    assert!(said(&ed).is_some_and(|l| l.contains("its output goes through grep")), "{:?}", said(&ed));
    // Idle for minutes as well: in, the filter said
    let r = ed.session.task_run.as_mut().unwrap();
    r.wait.limited(then, None);
    r.wait.looked(0.0, then);
    let l = said(&ed).expect("idle and quiet: in the inbox");
    assert!(l.contains("no output for") && l.contains("its output goes through grep"), "{l}");
    // No filter: said as before
    let mut r = run("cargo test");
    r.wait.looked(0.0, then);
    ed.session.task_run = Some(r);
    assert!(said(&ed).is_some_and(|l| !l.contains("goes through")), "{:?}", said(&ed));
    // `C-q` takes the user to its panel, as for any quiet run
    feed(&mut ed, "<C-q>");
    assert_eq!(ed.slot(), Some(crate::sizes::Slot::Task));
    std::fs::remove_dir_all(&dir).ok();
}

/// #171: a run of Claude's quiet for minutes (a slow `apt-get` in docker) is in the inbox, but after a card
/// that waits for an answer — even one that came long after it went quiet, in another session. `C-q` goes to
/// the card first; the quiet run stays reachable after it.
#[test]
fn a_card_waiting_comes_before_a_run_only_quiet() {
    use crate::task::{Run, State, Task};
    let (mut ed, dir, back, _) = three("quiet");
    let cmd = "docker run --rm img sh -c 'apt-get install -y x'";
    let mut r = Run::for_test(Task { name: cmd.into(), cmd: cmd.into(), cwd: dir.join("front") });
    (r.state, r.by_claude) = (State::Running, true);
    let Some(then) = Instant::now().checked_sub(Duration::from_secs(228)) else { return };
    r.last_output = then;
    ed.session.task_run = Some(r);
    park(&mut ed, back, claudes(1, 20));
    let inbox = ed.inbox();
    let kinds: Vec<&str> = inbox.iter().map(|e| e.kind).collect();
    assert_eq!(kinds, ["screen", "silent-run"], "{inbox:?}");
    assert!(inbox[1].waited_ms > inbox[0].waited_ms, "quiet for longer than the card has waited");
    assert_eq!(ed.ui_state().inbox.iter().map(|e| e.kind).collect::<Vec<_>>(), kinds);
    // C-q: to back's card, not the quiet run's panel here
    feed(&mut ed, "<C-q>");
    assert_eq!(ed.session.id, back);
    assert_eq!(ed.offers.first().map(|o| o.title.as_str()), Some("Claude asks 1"));
    feed(&mut ed, "<C-q>n");
    assert!(ed.offers.is_empty());
    // Answered: C-q now goes to the quiet run, its panel with the keys
    feed(&mut ed, "<C-q>");
    assert_eq!(ed.session.id, 0);
    assert_eq!(ed.slot(), Some(crate::sizes::Slot::Task));
    assert!(ed.bottom_has_keys());
    std::fs::remove_dir_all(&dir).ok();
}

/// Nothing waits, but turns ended in sessions behind (their tabs say `done`): `C-q` goes to them, the one
/// that ended first first, and says when — after whatever waits: a card that comes meanwhile goes first, and
/// it isn't the inbox's (its count stays what waits for an answer). None left: "no card waits".
#[test]
fn c_q_goes_to_a_finished_session_when_nothing_waits() {
    let (mut ed, dir, back, third) = three("done");
    let ago = |s: u64| Some((Instant::now() - Duration::from_secs(s), Ok(())));
    session(&mut ed, back).turn_end = ago(30);
    session(&mut ed, third).turn_end = ago(200);
    assert!(ed.inbox().is_empty(), "a turn's end asks nothing");
    assert_eq!(ed.next_up(), super::Next::Done(third));
    // A card comes meanwhile: it goes first
    park(&mut ed, back, claudes(1, 1));
    feed(&mut ed, "<C-q>");
    assert_eq!((ed.session.id, ed.offers.first().map(|o| o.title.as_str())), (back, Some("Claude asks 1")));
    ed.offers.clear();
    // (back was looked at with it: its `done` went)
    assert!(session(&mut ed, back).turn_end.is_none());
    // Then third's turn end, said with how long ago
    feed(&mut ed, "<C-q>");
    assert_eq!(ed.session.id, third);
    assert!(ed.status.as_ref().is_some_and(|s| s.0 == "third · its turn ended 3m ago"), "{:?}", ed.status);
    // Nothing left
    feed(&mut ed, "<C-q>");
    assert_eq!(ed.session.id, third);
    assert!(ed.status.as_ref().is_some_and(|s| s.0 == NOTHING), "{:?}", ed.status);
    std::fs::remove_dir_all(&dir).ok();
}

/// The list: `space I` (or a click on the status line's count) — a row each, its session named, what it
/// asks, its kind and how long it has waited, the oldest in hand. `enter` goes to it — its session in front,
/// its card up with the keys — and it's answered there. Nothing waiting: said, no list.
#[test]
fn the_list_goes_to_the_one_picked_and_its_card_takes_the_keys() {
    let (mut ed, dir, back, third) = three("list");
    park(&mut ed, back, claudes(1, 60));
    park(&mut ed, third, tarae(400));
    // A click on the count opens it
    let rows = screen(&mut ed, 160, 30);
    let x = rows[29].find("▲2 waiting").map(|b| rows[29][..b].chars().count()).expect("the count");
    let click = crate::event::Mouse { kind: crate::event::MouseKind::Down, x: x as u16, y: 29, alt: false };
    ed.handle_mouse(click);
    let p = ed.picker.as_ref().expect("the inbox");
    assert_eq!(p.title, super::INBOX);
    let labels: Vec<&str> = p.items().iter().map(|i| i.label.as_str()).collect();
    assert_eq!(labels.len(), 2);
    assert!(labels[0].starts_with("third · Trust this project's settings?"), "{labels:?}");
    assert!(labels[1].starts_with("back · Claude asks 1"), "{labels:?}");
    // (its time a column, `RUN_TIME_W` wide)
    assert!(p.items()[0].hint.starts_with("project-trust ·  6m 40s"), "{}", p.items()[0].hint);
    let rows = screen(&mut ed, 160, 30);
    assert!(rows.iter().any(|r| r.contains("enter go to it")), "its keys: {rows:#?}");
    // The second row, enter: back in front, its card up with the keys — `y` answers it there
    feed(&mut ed, "<down><ret>");
    assert!(ed.picker.is_none());
    assert_eq!(ed.session.id, back);
    assert_eq!(ed.offers.first().map(|o| o.title.as_str()), Some("Claude asks 1"));
    assert!(ed.card_keys, "answered there");
    // Answered meanwhile: the list follows; empty, it closes and says so
    ed.inbox_open();
    session(&mut ed, third).parked.clear();
    ed.inbox_sync();
    assert_eq!(ed.picker.as_ref().map(|p| p.items().len()), Some(1));
    ed.offers.clear();
    ed.inbox_sync();
    assert!(ed.picker.is_none());
    feed(&mut ed, "<space>I");
    assert!(ed.picker.is_none());
    assert!(ed.status.as_ref().is_some_and(|s| s.0.contains("nothing waits for you")), "{:?}", ed.status);
    std::fs::remove_dir_all(&dir).ok();
}
