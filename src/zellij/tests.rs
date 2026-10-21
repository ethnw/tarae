//! Inside zellij and out: the hints say a key zellij takes by its twin only inside it, and the twin does the
//! same — the keymap's (which-key, the palette, `:config-show`), the tutor's, the cards' and the chat's.

use super::*;
use crate::chat::{Chat, Msg, Role, State};
use crate::keymap::Keymaps;
use crate::offer::{Offer, What};

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

fn drawn(ed: &mut Editor, w: u16, h: u16) -> String {
    let mut bytes = Vec::new();
    crate::term::render(ed, &mut bytes, w, h).unwrap();
    let mut p = vt100::Parser::new(h, w, 0);
    p.process(&bytes);
    p.screen().rows(0, w).collect::<Vec<_>>().join("\n")
}

fn editor(zellij: bool) -> Editor {
    let mut ed = Editor::new(crate::config::Config::default());
    ed.zellij = zellij;
    ed
}

fn seq(s: &str) -> Vec<Key> {
    s.split(' ').map(|k| k.parse().unwrap()).collect()
}

/// A card of Claude's (it asks to see the screen).
fn card(ed: &mut Editor) {
    let what = What::Screen { call: crate::tools::Call::test(1) };
    ed.push_offer(Offer::new("Claude asks".into(), "to see the screen", "", String::new(), what));
}

/// A chat with a conversation, its input holding the keys.
fn chatting(ed: &mut Editor) {
    let mut c = Chat::new();
    c.msgs.push(Msg::new(Role::User, "hi", ""));
    c.msgs.push(Msg::new(Role::Assistant, "hello", ""));
    ed.session.chat = Some(c);
    ed.session.at = crate::session::At::Chat;
}

/// Keys, a frame after each (the chat's view mode is laid out of the last frame).
fn keys(ed: &mut Editor, keys: &str) {
    for k in keys.split(' ') {
        drawn(ed, 160, 40);
        match k {
            "space" => feed(ed, "<space>"),
            "esc" | "ret" => feed(ed, &format!("<{k}>")),
            k if k.starts_with('C') && k.len() > 1 && k.as_bytes()[1] == b'-' => feed(ed, &format!("<{k}>")),
            k => feed(ed, k),
        }
    }
    drawn(ed, 160, 40);
}

/// The chat's twins are its commands, typed in the code: from its input the way back first — and those keys run
/// the command, nothing typed to Claude (`:` in the input is a character). Nothing said yet: one `esc`.
#[test]
fn the_chats_command_twins_run_the_command() {
    for said in [false, true] {
        let mut ed = editor(true);
        match said {
            true => chatting(&mut ed),
            false => {
                let c = Chat::new();
                ed.session.chat = Some(c);
                ed.session.at = crate::session::At::Chat;
            }
        }
        drawn(&mut ed, 160, 40);
        let twin = ed.key_said("C-s");
        assert_eq!(twin, if said { "esc esc :chat-screen" } else { "esc :chat-screen" });
        keys(&mut ed, &format!("{twin} ret"));
        let c = ed.session.chat.as_ref().unwrap();
        assert!(
            c.screen.is_some(),
            "{said}: the screen goes with the next message: {:?}",
            ed.messages.last()
        );
        assert!(c.input.is_empty(), "nothing typed into the chat");
        assert_eq!(c.msgs.len(), if said { 2 } else { 0 }, "nothing sent");
    }
}

/// What a status line or toast says, its last one.
fn said(ed: &Editor) -> String {
    format!("{:?} {:?}", ed.status.as_ref().map(|s| &s.0), ed.messages.last())
}

/// The prompt's line for a key it doesn't take, a card up: its own `esc` is the way back — what reaches the card is
/// said from the code (inside zellij the card in front takes the keys there), never the prompt's `esc` again.
#[test]
fn the_prompts_line_says_the_card_from_the_code() {
    for zellij in [false, true] {
        let mut ed = editor(zellij);
        card(&mut ed);
        feed(&mut ed, ":<C-a>");
        let says = match zellij {
            true => "esc close — then the card takes the keys",
            false => "esc close — then C-q reaches the card",
        };
        assert!(said(&ed).contains(says), "{says}: {}", said(&ed));
        feed(&mut ed, "<esc>");
        assert!(ed.card_has_keys(), "as it says");
    }
}

/// A card holding the keys (`C-q` gave them — it swallows what isn't its answer): a twin lets go of it first, its
/// `esc` back to the chat, then the chat's way back — and the chat's row says no `answer card` while it holds them.
/// Its `C-q` again: keys back to the chat, said with the chat's twin.
#[test]
fn a_card_holding_the_keys_lets_go_first() {
    let mut ed = editor(true);
    chatting(&mut ed);
    // (Claude at work: the row where the chat says `answer card`)
    ed.session.chat.as_mut().unwrap().state = State::Thinking;
    card(&mut ed);
    assert!(drawn(&mut ed, 160, 40).contains("esc esc answer card"), "the card not holding them yet");
    keys(&mut ed, "C-q");
    assert!(ed.card_keys, "the card holds them");
    assert!(!drawn(&mut ed, 160, 40).contains("answer card"));
    let twin = ed.key_said("C-s");
    assert_eq!(twin, "esc esc esc :chat-screen");
    keys(&mut ed, &format!("{twin} ret"));
    let c = ed.session.chat.as_ref().unwrap();
    assert!(c.screen.is_some(), "it ran: {}", said(&ed));
    assert!(c.input.is_empty() && c.msgs.len() == 2, "nothing typed, nothing sent");
    assert_eq!(ed.offers.len(), 1, "the card waits, unanswered");
    // Given the keys again, then back: said the chat's way
    ed.session.at = crate::session::At::Chat;
    keys(&mut ed, "C-q C-q");
    assert!(!ed.card_keys);
    assert!(said(&ed).contains("keys back — the card waits, esc esc answers it"), "{}", said(&ed));
}

/// An edit's review in the buffer, one change of line 1 (`llm::review_with`), the keys in it.
fn reviewing(ed: &mut Editor) {
    ed.docs[0].text = ropey::Rope::from_str("a\n");
    let doc = ed.doc().id;
    crate::llm::review_with(ed, doc, "change it", vec![(0, 1, "a".into(), "b".into())], None);
    drawn(ed, 160, 40);
    assert_eq!(ed.keys_to().region, Region::EditReview);
}

/// The edit review's `esc`, back to the code: its line and its header say the inbox (`space I`) — and those keys
/// bring the change back with the keys, as `C-q` does.
#[test]
fn the_edit_review_left_for_the_code_says_the_inbox_and_it_brings_it_back() {
    let mut ed = editor(true);
    reviewing(&mut ed);
    keys(&mut ed, "esc");
    assert!(ed.session.review.as_ref().is_some_and(|r| r.away), "left, unanswered");
    assert!(
        said(&ed).contains("back to the code — the change waits unanswered, space I answers it"),
        "{}",
        said(&ed)
    );
    assert!(drawn(&mut ed, 160, 40).contains("space I answer it"), "the header says it too");
    keys(&mut ed, "space I ret");
    assert!(ed.session.review.as_ref().is_some_and(|r| !r.away), "back in it: {}", said(&ed));
    assert_eq!(ed.keys_to().region, Region::EditReview);
}

/// The edit review taken from the chat: its `esc` goes back to the chat, said with the chat's way back then the
/// inbox — and those keys bring it back.
#[test]
fn the_edit_review_left_for_the_chat_says_the_chats_way_and_it_brings_it_back() {
    let mut ed = editor(true);
    chatting(&mut ed);
    ed.session.leave(crate::session::At::Chat);
    reviewing(&mut ed);
    ed.session.review.as_mut().unwrap().from_chat = true;
    keys(&mut ed, "esc");
    assert_eq!(ed.keys_to().region, Region::Chat);
    let twin = ed.key_said("C-q");
    assert_eq!(twin, "esc esc space I");
    assert!(
        said(&ed).contains(&format!("back to the chat — the change waits unanswered, {twin} answers it"))
    );
    keys(&mut ed, &format!("{twin} ret"));
    assert_eq!(ed.keys_to().region, Region::EditReview, "back in it: {}", said(&ed));
    assert!(ed.session.chat.as_ref().unwrap().input.is_empty(), "nothing typed into the chat");
}

/// A card stepped aside (`Offer.aside`) isn't the one the way back reaches: the inbox is — and `space I` `enter`
/// gives it the keys again.
#[test]
fn an_aside_card_is_the_inboxs() {
    let mut ed = editor(true);
    card(&mut ed);
    ed.offers[0].aside = true;
    drawn(&mut ed, 160, 40);
    assert!(!ed.card_has_keys());
    assert_eq!(ed.key_said("C-q"), "space I");
    keys(&mut ed, "space I ret");
    assert!(ed.card_has_keys(), "{}", said(&ed));
}

/// The chat taking the whole screen (50 columns): the status line's `C-q shows its card` says the chat's way back
/// — and that key brings the card up, drawn.
#[test]
fn the_chat_taking_the_screen_says_its_way_back_to_the_card() {
    let mut ed = editor(true);
    let c = Chat::new();
    ed.session.chat = Some(c);
    ed.session.at = crate::session::At::Chat;
    let what = What::ChangeSet { whose: crate::refactor::Whose::Claude(7), ops: false };
    ed.push_offer(Offer::new(
        "Claude changes 3 files".into(),
        "One review for the set.",
        "",
        String::new(),
        what,
    ));
    let s = drawn(&mut ed, 50, 30);
    let status = s.lines().last().unwrap_or_default().to_string();
    assert!(status.contains("▲1 waiting · esc shows its card"), "{status}\n{s}");
    feed(&mut ed, "<esc>");
    drawn(&mut ed, 50, 30);
    assert!(ed.screen.offer_buttons.iter().any(|b| b.3 == 'y'), "drawn, with its answers");
}

/// Rebound, the twin names the keys as bound: the inbox on `A-q`, the session list on `A-w`.
#[test]
fn the_twins_say_the_keys_as_bound() {
    let mut ed = editor(true);
    let keys: toml::Table = toml::from_str("[normal]\nA-q = \"inbox\"\nA-w = \"session_list\"\n").unwrap();
    ed.keymaps.merge_user(&keys, &mut Vec::new());
    assert_eq!(ed.key_said("C-q"), "A-q", "nothing waits: the inbox, as bound");
    assert_eq!(ed.c_q_place(Reach::Sessions), "A-w");
    chatting(&mut ed);
    assert_eq!(super::c_q_twin(&ed, Reach::Inbox), "esc esc A-q");
}

/// The setting holds on every surface: `off` inside zellij says no twin anywhere, `on` outside says them all —
/// the palette, which-key, the tutor, `:config-show`.
#[test]
fn the_setting_holds_on_every_surface() {
    for (inside, setting, twins) in [(true, "off", false), (false, "on", true)] {
        let mut ed = editor(inside);
        crate::typed::execute(&mut ed, &format!("set editor.zellij-hints {setting}")).unwrap();
        let at = format!("{inside} {setting}");
        feed(&mut ed, "<space>?");
        let p = ed.picker.as_ref().expect("the palette");
        let up = p.items().iter().find(|i| i.action == crate::picker::Action::Command("page_up"));
        assert_eq!(up.map(|i| i.hint.as_str()), Some(if twins { "pageup" } else { "C-b" }), "{at}: palette");
        feed(&mut ed, "<esc>z");
        let s = drawn(&mut ed, 140, 40);
        let row = s.lines().find(|l| l.contains("C-b ")).unwrap_or_default();
        assert_eq!(row.contains("pageup"), twins, "{at}: which-key {row}");
        feed(&mut ed, "<esc>");
        ed.open_tutor();
        assert_eq!(ed.doc().text.to_string().contains("C-g locks zellij"), twins, "{at}: tutor");
        crate::typed::execute(&mut ed, "config-show").unwrap();
        assert_eq!(ed.doc().text.to_string().contains("# Inside zellij"), twins, "{at}: :config-show");
    }
}

/// A command bound only to keys zellij takes is said by its `:` command, never by such a key (`save_selection`: `C-s`
/// alone); one with a key zellij leaves gets that one (`page_up`: `pageup`, not `C-b`).
#[test]
fn a_twin_never_names_a_key_zellij_takes() {
    let ed = editor(true);
    assert_eq!(super::bound_key(&ed, "save_selection", ":its-command"), ":its-command");
    assert_eq!(super::bound_key(&ed, "page_up", ":its-command"), "pageup");
    assert_eq!(super::bound_key(&ed, "inbox", ":inbox"), "space I");
}

/// `editor.zellij-hints`: `auto` says the twins inside zellij only, `on` always, `off` never (zellij's unlock-first
/// preset, keys of your own) — `:set` too.
#[test]
fn the_setting_turns_the_twins_on_and_off() {
    for (inside, setting, said) in
        [(true, "auto", "space I"), (false, "auto", "C-q"), (false, "on", "space I"), (true, "off", "C-q")]
    {
        let mut ed = editor(inside);
        crate::typed::execute(&mut ed, &format!("set editor.zellij-hints {setting}")).unwrap();
        assert_eq!(ed.key_said("C-q"), said, "{inside} {setting}");
        assert_eq!(ed.key_hint("C-s", "screen").1, if said == "C-q" { "screen" } else { "" });
    }
}

/// Nothing waits: `C-q`'s twin is the inbox, which says so as `C-q` does.
#[test]
fn nothing_waiting_the_twin_is_the_inbox_saying_so() {
    let mut ed = editor(true);
    assert_eq!(ed.next_up(), crate::inbox::Next::None);
    assert_eq!(ed.key_hint("C-q", "answer card"), ("space I".to_string(), "answer card"));
    feed(&mut ed, "<space>I");
    assert!(said(&ed).contains("nothing waits for you"), "{}", said(&ed));
}

/// The chat's view mode: one `esc` is the way back to the code, where the card in front takes the keys.
#[test]
fn from_the_chats_view_mode_the_twin_is_one_esc() {
    let mut ed = editor(true);
    chatting(&mut ed);
    keys(&mut ed, "esc");
    assert!(crate::view::chat_view_of(&ed).is_some(), "in its view mode");
    card(&mut ed);
    assert!(!ed.card_has_keys());
    assert_eq!(ed.key_said("C-q"), "esc");
    keys(&mut ed, "esc");
    assert!(ed.card_has_keys(), "{:?}", ed.messages.last());
}

/// The terminal's program holding the keys: `C-\ C-\` (its view mode, then the code) gives the card in front the
/// keys — on a stand-in terminal (no pty, no program).
#[test]
fn from_the_terminals_program_the_twin_is_its_round_to_the_code() {
    let mut ed = editor(true);
    let _sent = crate::terminal::stand_in(&mut ed, 10, 80, 100);
    drawn(&mut ed, 160, 40);
    assert_eq!(ed.keys_to().region, crate::uistate::Region::Program);
    card(&mut ed);
    assert!(!ed.card_has_keys());
    assert_eq!(ed.key_said("C-q"), "C-\\ C-\\");
    keys(&mut ed, "C-\\ C-\\");
    assert!(ed.card_has_keys(), "{:?} {:?}", ed.keys_to().region, ed.messages.last());
    feed(&mut ed, "n");
    assert!(ed.offers.is_empty(), "its n answered it");
}

/// `editor.zellij-keys` names the letters zellij takes: zellij's defaults (session on `C-o`) or a config of one's
/// own (session on `C-y`, `C-o` left alone) — every place reads it: the chat's `C-o point`, a twin that is a key
/// (the inbox bound to `C-y`), `:config-show`'s list, the tutor's.
#[test]
fn the_keys_zellij_takes_are_the_settings() {
    let yours = ["g", "q", "p", "n", "s", "y", "t", "h", "b"];
    for mine in [false, true] {
        let mut ed = editor(true);
        if mine {
            let setting = crate::settings::find("editor.zellij-keys").expect("its row");
            (setting.set)(&mut ed.config, &toml::Value::Array(yours.iter().map(|&k| k.into()).collect()));
            assert_eq!(ed.zellij_letters(), ['g', 'q', 'p', 'n', 's', 'y', 't', 'h', 'b']);
        }
        let at = if mine { "session on C-y" } else { "zellij's defaults" };
        // The chat's `C-o point`: zellij's own default takes it, a `C-y` config leaves it
        let c = Chat::new();
        ed.session.chat = Some(c);
        ed.session.at = crate::session::At::Chat;
        let s = drawn(&mut ed, 160, 40);
        let point = if mine { "C-o point" } else { "esc :chat-point" };
        assert!(s.contains(point), "{at}: {point}\n{s}");
        ed.session.chat = None;
        // A twin that is a key: the inbox bound to `C-y` too — said where zellij leaves it
        let keys: toml::Table = toml::from_str("[normal]\nC-y = \"inbox\"\n").unwrap();
        ed.keymaps.merge_user(&keys, &mut Vec::new());
        assert_eq!(ed.key_said("C-q"), if mine { "space I" } else { "C-y" }, "{at}");
        // The lists: `:config-show`'s and the tutor's
        crate::typed::execute(&mut ed, "config-show").unwrap();
        let shown = ed.doc().text.to_string();
        let list =
            if mine { "C-g C-q C-p C-n C-s C-y C-t C-h C-b" } else { "C-g C-q C-p C-n C-s C-t C-h C-o C-b" };
        assert!(shown.contains(&format!("takes {list} first (editor.zellij-keys)")), "{at}: {shown}");
        ed.open_tutor();
        assert!(ed.doc().text.to_string().contains(list), "{at}: the tutor's list");
    }
}

/// The keymap's twin: the shortest key of the mode bound to the same, none of it zellij's — none when it has
/// no other; the palette says it inside zellij only.
#[test]
fn the_keymaps_twin_is_the_same_command_without_zellijs_keys() {
    let km = Keymaps::default();
    let d = &DEFAULT_KEYS;
    assert_eq!(km.twin(Mode::Normal, &seq("z C-b"), d).as_deref(), Some("pageup"));
    assert_eq!(km.twin(Mode::Normal, &seq("C-b"), d).as_deref(), Some("pageup"));
    assert_eq!(km.twin(Mode::Insert, &seq("C-h"), d).as_deref(), Some("backspace"));
    assert_eq!(km.twin(Mode::Normal, &seq("C-s"), d), None, "save_selection has no other key");
    assert_eq!(km.twin(Mode::Normal, &seq("C-f"), d), None, "zellij doesn't take C-f");
    assert_eq!(km.twin(Mode::Normal, &seq("C-o"), d), None, "jump_backward: no other key");
    assert_eq!(km.bindings_in(&[])["page_up"], "C-b");
    assert_eq!(km.bindings_in(d)["page_up"], "pageup");
    assert_eq!(km.bindings_in(d)["save_selection"], "C-s", "no twin: said as it is");
    let taken = km.taken(d);
    let row = |keys: &str| taken.iter().find(|t| t.1 == keys).map(|t| t.3.clone());
    assert_eq!(row("z C-b"), Some(Some("pageup".into())));
    assert_eq!(row("C-s"), Some(None));
    assert_eq!(row("C-o"), Some(None), "zellij's default takes C-o");
    assert!(taken.iter().all(|t| t.1.split(' ').any(|k| takes(&k.parse().unwrap(), d))), "{taken:?}");
    for zellij in [false, true] {
        let mut ed = editor(zellij);
        feed(&mut ed, "<space>?");
        let p = ed.picker.as_ref().expect("the palette");
        let up = p.items().iter().find(|i| i.action == crate::picker::Action::Command("page_up"));
        assert_eq!(up.map(|i| i.hint.as_str()), Some(if zellij { "pageup" } else { "C-b" }));
    }
}

/// Which-key under `z`: inside zellij `C-b`'s row says `pageup` after what it does; outside, nothing more.
#[test]
fn which_key_says_the_twin_inside_zellij() {
    for zellij in [false, true] {
        let mut ed = editor(zellij);
        feed(&mut ed, "z");
        let s = drawn(&mut ed, 140, 40);
        let row = s.lines().find(|l| l.contains("C-b ")).unwrap_or_else(|| panic!("drawn:\n{s}"));
        assert_eq!(row.contains("pageup"), zellij, "{row}");
        assert!(!s.contains('…'), "nothing cut:\n{s}");
    }
}

/// On a narrow screen the twin goes before the description is cut under its floor — every width draws, none
/// panics (a cut `dw` under the twin's width underflowed); wide, it's there.
#[test]
fn which_key_drops_the_twin_where_it_doesnt_fit() {
    let mut seen = Vec::new();
    // (short screens too: the keys go into columns and the descriptions are cut to share the width)
    for h in [10, 12, 14, 40] {
        for w in (24..=140).step_by(4) {
            let mut ed = editor(true);
            feed(&mut ed, "z");
            let s = drawn(&mut ed, w, h);
            if let Some(row) = s.lines().find(|l| l.contains("C-b ")) {
                seen.push((w, h, row.contains("pageup")));
            }
        }
    }
    assert!(seen.iter().any(|&(.., twin)| !twin), "narrow: drawn without it {seen:?}");
    assert!(seen.contains(&(140, 40, true)), "{seen:?}");
}

/// `:config-show` lists the taken keys and their twins inside zellij only; the tutor says its twins there.
#[test]
fn config_show_and_the_tutor_say_the_twins_inside_zellij() {
    for zellij in [false, true] {
        let mut ed = editor(zellij);
        crate::typed::execute(&mut ed, "config-show").unwrap();
        let text = ed.doc().text.to_string();
        assert_eq!(text.contains("# Inside zellij"), zellij);
        assert_eq!(text.contains("# normal z C-b (page_up): pageup"), zellij, "{text}");
        ed.open_tutor();
        let text = ed.doc().text.to_string();
        assert_eq!(text.contains("C-g locks zellij"), zellij);
        assert_eq!(text.contains("C-q hands it your keys"), !zellij);
    }
}

/// A busy chat holding the keys, a card up: its hint row says `C-q answer card` — inside zellij `esc esc`, and
/// `esc esc` gives the card the keys (its `n` answers it, nothing typed).
#[test]
fn the_chats_twin_of_c_q_gives_the_card_the_keys() {
    for zellij in [false, true] {
        let mut ed = editor(zellij);
        let mut c = Chat::new();
        c.state = State::Thinking;
        c.msgs.push(Msg::new(Role::User, "hi", ""));
        c.msgs.push(Msg::new(Role::Assistant, "hello", ""));
        ed.session.chat = Some(c);
        ed.session.at = crate::session::At::Chat;
        card(&mut ed);
        let s = drawn(&mut ed, 160, 40);
        let says = if zellij { "esc esc answer card" } else { "C-q answer card" };
        assert!(s.contains(says), "{says}:\n{s}");
        assert!(!ed.card_has_keys());
        feed(&mut ed, if zellij { "<esc>" } else { "<C-q>" });
        if zellij {
            drawn(&mut ed, 160, 40);
            feed(&mut ed, "<esc>");
        }
        assert!(ed.card_has_keys(), "the card has the keys: {:?}", ed.messages.last());
        if !zellij {
            // (outside zellij too: the card holding them, the row no longer offers to give it them)
            assert!(ed.card_keys);
            assert!(!drawn(&mut ed, 160, 40).contains("answer card"), "held: no `answer card`");
        }
        feed(&mut ed, "n");
        assert!(ed.offers.is_empty(), "its n answered it: {:?}", ed.messages.last());
        assert!(ed.session.chat.as_ref().unwrap().input.is_empty(), "nothing typed into the chat");
    }
}

/// A card that came as keys were coming in waits for them to go quiet (`Offer.typing`): the status line's
/// `▲1 waiting · C-q` says the twin — and `space I` `enter` gives it the keys at once, as `C-q` does.
#[test]
fn a_card_waiting_for_the_keys_to_go_quiet_says_the_twin() {
    for zellij in [false, true] {
        let mut ed = editor(zellij);
        ed.keys_hold = true;
        feed(&mut ed, "j");
        card(&mut ed);
        assert!(ed.card_waits_quiet(), "it waits for the keys to go quiet");
        let s = drawn(&mut ed, 160, 40);
        let status = s.lines().last().unwrap_or_default().to_string();
        let says = if zellij { "▲1 waiting · space I" } else { "▲1 waiting · C-q" };
        assert!(status.contains(says), "{says}: {status}");
        feed(&mut ed, if zellij { "<space>I<ret>" } else { "<C-q>" });
        assert!(ed.card_has_keys(), "{zellij}: {}", said(&ed));
    }
}

/// The chat's message waits for the card about a file named like a key it points at (`reads::ask_own`): `enter`
/// says so with `C-q` — inside zellij its twin from the chat (nothing said yet: `esc`), and `esc` gives the card the
/// keys.
#[test]
fn the_chats_message_waiting_on_its_card_says_the_way_to_it() {
    for zellij in [false, true] {
        let mut ed = editor(zellij);
        let mut c = Chat::new();
        c.input = "look at this".into();
        c.cursor = c.input.len();
        ed.session.chat = Some(c);
        ed.session.at = crate::session::At::Chat;
        let key_file = ed.session.root.join(".env");
        crate::reads::ask_own(&mut ed, vec![key_file], crate::reads::Own::Chip("pane#1".into()));
        assert!(crate::reads::pointing_asks(&ed), "its card");
        drawn(&mut ed, 160, 40);
        feed(&mut ed, "<ret>");
        let key = if zellij { "esc" } else { "C-q" };
        let says = format!("not sent yet — answer the card about what you pointed at first ({key})");
        assert!(said(&ed).contains(&says), "{says}: {}", said(&ed));
        feed(&mut ed, &format!("<{key}>"));
        assert!(ed.card_has_keys(), "{zellij}: {}", said(&ed));
    }
}

/// A card stepped aside: its own line says `aside while you look — C-q answers it`, inside zellij the inbox.
#[test]
fn an_aside_cards_line_says_the_twin() {
    for zellij in [false, true] {
        let mut ed = editor(zellij);
        card(&mut ed);
        ed.offers[0].aside = true;
        let s = drawn(&mut ed, 160, 40);
        let says = if zellij {
            "aside while you look — space I answers it"
        } else {
            "aside while you look — C-q answers it"
        };
        assert!(s.contains(says), "{says}:\n{s}");
    }
}

/// In insert mode a card waits, its line says `C-q to answer it` — inside zellij `esc to answer it`, and `esc`
/// gives it the keys.
#[test]
fn insert_modes_twin_of_c_q_is_esc() {
    for zellij in [false, true] {
        let mut ed = editor(zellij);
        feed(&mut ed, "i");
        card(&mut ed);
        assert_eq!(ed.key_said("C-q"), if zellij { "esc" } else { "C-q" });
        let s = drawn(&mut ed, 160, 40);
        let says = format!("{} to answer it", ed.key_said("C-q"));
        assert!(s.contains(&says), "{says}:\n{s}");
        feed(&mut ed, if zellij { "<esc>" } else { "<C-q>" });
        assert!(ed.card_has_keys());
        feed(&mut ed, "n");
        assert!(ed.offers.is_empty(), "its n answered it");
        // A prompt open: `esc` too — `space I` would be typed into it
        feed(&mut ed, ":");
        card(&mut ed);
        assert_eq!(ed.key_said("C-q"), if zellij { "esc" } else { "C-q" });
        feed(&mut ed, if zellij { "<esc>" } else { "<C-q>" });
        assert!(ed.prompt.is_none(), "the prompt closed, as C-q closes it");
        assert!(ed.card_has_keys(), "{zellij}: {:?} {:?}", ed.offers.len(), ed.messages.last());
    }
}

/// The empty chat's first keys: `C-t` another model, `C-p` an earlier conversation — inside zellij their
/// commands, which do the same.
#[test]
fn the_empty_chat_says_its_commands_inside_zellij() {
    for zellij in [false, true] {
        let mut ed = editor(zellij);
        let c = Chat::new();
        ed.session.chat = Some(c);
        ed.session.at = crate::session::At::Chat;
        let s = drawn(&mut ed, 160, 40);
        // (a `:` command says what it does: alone, never `:model for another model`; from the chat's input the
        // way back to the code first — nothing said yet, one `esc`)
        let model = if zellij { "esc :model" } else { "C-t for another model" };
        assert!(s.contains(model), "{model}:\n{s}");
        let screen = if zellij { "esc :chat-screen  esc :chat-point" } else { "C-s screen  C-o point" };
        assert!(s.contains(screen), "{screen}:\n{s}");
        assert_eq!(s.contains("for another model") || s.contains(":chat-screen screen"), !zellij, "{s}");
    }
    let pair = |zellij: bool, k, w| editor(zellij).key_hint(k, w);
    assert_eq!(pair(true, "C-s", "screen"), (":chat-screen".to_string(), ""));
    assert_eq!(pair(true, "C-b", "page up"), ("C-b".to_string(), "page up"), "not a `:` twin: kept whole");
    assert_eq!(pair(false, "C-s", "screen"), ("C-s".to_string(), "screen"));
    for (key, twin) in [("C-t", ":model"), ("C-p", ":chat-resume"), ("C-s", ":chat-screen")] {
        assert_eq!(editor(true).key_said(key), twin);
        assert!(crate::cmdline::COMMANDS.iter().any(|c| c.names[0] == &twin[1..]), "{twin} is a command");
    }
}
