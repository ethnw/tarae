use super::*;
use crate::event::{Mouse, MouseKind};
use crate::key::Code;
use crate::offer::{Offer, What};
use crate::session::At;
use crate::uistate::Region;
use unicode_width::UnicodeWidthStr as _;

fn frame(ed: &mut Editor) -> String {
    ed.focus_sync();
    crate::term::render(ed, &mut Vec::new(), 160, 40).unwrap();
    crate::screen::text(ed.last_frame.as_ref().unwrap().0.as_slice(), 160, 40).unwrap()
}

/// Keys, a frame after each (`<C-w>`, `<esc>`, `<ret>` as written).
fn feed(ed: &mut Editor, keys: &str) {
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let k: Key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        };
        ed.handle_key(k);
        frame(ed);
    }
}

fn editor() -> Editor {
    let mut ed = Editor::new(crate::config::Config::default());
    ed.docs[0].text = ropey::Rope::from_str("one\ntwo\n");
    ed
}

/// The card's rows as drawn, top to bottom.
fn card(s: &str, ed: &Editor) -> Vec<String> {
    let (x, y, w, h) = ed.screen.toast_card.get().expect("the card drawn");
    s.lines().skip(y).take(h).map(|l| l.chars().skip(x).take(w).collect()).collect()
}

fn press(ed: &mut Editor, x: usize, y: usize) {
    ed.handle_mouse(Mouse { kind: MouseKind::Down, x: x as u16, y: y as u16, alt: false });
    frame(ed);
}

/// A toast takes no key until focused (P3 §3.9): `y` is the code's yank. `C-w w` focuses it — its frame lit with its
/// name and keys, the same rect; its time held; `y` copies it, never a yank; `q` lets it go, fading out over `LETGO`
/// from half strength; a message that comes meanwhile starts a card of its own.
#[test]
fn a_toast_takes_the_keys_only_once_focused() {
    let mut ed = editor();
    // (the flash drawn, so `y`'s words are a note, no toast of theirs on the card — `motion.rs`; the message is the
    // card's width, long enough for its keys)
    ed.motion_drawn = true;
    ed.set_status("saved a.rs, 3 lines written");
    frame(&mut ed);
    feed(&mut ed, "y");
    assert!(
        ed.toast_focus.is_none() && ed.messages.last().unwrap().2.starts_with("yanked"),
        "y: the code's yank"
    );
    let texts: Vec<&str> = ed.toasts.iter().map(|t| t.text.as_str()).collect();
    assert_eq!(texts, ["saved a.rs, 3 lines written"], "y's words a note: its flash drawn");
    let before = frame(&mut ed);
    let rect = ed.screen.toast_card.get();
    let rows = card(&before, &ed);
    assert!(rows[0].starts_with("╭─ messages"), "unfocused: framed, named: {rows:#?}");
    assert!(!rows.last().unwrap().contains("enter"), "no keys of its own until focused");
    feed(&mut ed, "<C-w>w");
    let f = ed.toast_focus.expect("C-w w: the toast");
    assert_eq!(ed.keys_to().region, Region::Toast(f.id));
    assert!(matches!(ed.session.at, At::Float(_)), "a floating pane with the keys");
    let s = frame(&mut ed);
    let rows = card(&s, &ed);
    assert!(
        rows[0].starts_with("╭─ messages") && rows.last().unwrap().contains("enter messages"),
        "{rows:#?}"
    );
    assert_eq!(ed.screen.toast_card.get(), rect, "the same rect focused");
    // Held: long past its reading time, still up
    let past = std::time::Instant::now() - std::time::Duration::from_secs(600);
    ed.toasts.iter_mut().for_each(|t| t.at = past);
    assert!(ed.toasts.iter().all(crate::editor::Toast::alive), "held while it has the keys");
    let yanks = ed.messages.iter().filter(|m| m.2.starts_with("yanked")).count();
    feed(&mut ed, "y");
    assert_eq!(
        ed.messages.iter().filter(|m| m.2.starts_with("yanked")).count(),
        yanks,
        "its y copies, never a yank"
    );
    assert!(ed.toast_focus.is_some(), "y: still focused");
    feed(&mut ed, "q");
    assert!(ed.toast_focus.is_none() && ed.toast_letgo.is_some(), "q: let go, fading");
    assert_eq!(ed.session.at, At::Code, "the keys back to the code");
    assert!(ed.toast_alpha() <= 0.5, "from half strength");
    assert!(ed.toasts.iter().all(|t| t.life <= t.age() + LETGO), "gone within LETGO");
    ed.set_status("next");
    assert_eq!(
        (ed.toasts.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(), ed.toast_letgo),
        (vec!["next"], None),
        "a message during the fade: a card of its own"
    );
}

/// `C-w w`'s round: the panes, then the card waiting, then the toast, then round again.
#[test]
fn the_round_goes_to_the_card_then_the_toast() {
    let mut ed = editor();
    let o = Offer::new(
        "End the review?".into(),
        "2 files not viewed.",
        "",
        String::new(),
        What::ReviewEnd { session: 0, started: std::time::Instant::now() },
    );
    ed.push_offer(o);
    // (the cursor off the top rows: the toast takes the top right, the card the bottom)
    ed.docs[0].text = ropey::Rope::from_str(&"line\n".repeat(30));
    feed(&mut ed, "jjjjjjjj");
    ed.set_status("saved a.rs");
    frame(&mut ed);
    assert!(ed.screen.offer_card.is_some() && ed.screen.toast_card.get().is_some(), "both in sight");
    feed(&mut ed, "<C-w>w");
    assert!(ed.card_has_keys(), "first the card (stepped aside for `C-w` in the frame between)");
    feed(&mut ed, "<C-w>w");
    assert!(!ed.card_has_keys() && ed.toast_focus.is_some(), "then the toast");
    feed(&mut ed, "<C-w>w");
    assert!(ed.toast_focus.is_none() && ed.session.at == At::Code, "then round to the code");
    // (the toast let go fades; the card still waits: the round starts over with it)
    feed(&mut ed, "<C-w>w");
    assert!(ed.card_has_keys(), "the card again");
}

/// Focused, a toast keeps its rect (user, 2026-10-23: nothing moves) — its keys whole, in their order (`enter messages`,
/// `esc close`, `y copy`, `j k move`), one that doesn't fit skipped for the next that does, never a wider card; the way
/// out always said (`esc close` — the card is never narrower than it): at 80 and 160 columns, one message, three, and
/// one too short for `enter messages`.
#[test]
fn a_focused_toast_keeps_its_rect() {
    let texts: &[&[&str]] = &[
        &["Saved src/main.rs · 15 lines"],
        &["Saved src/main.rs · 15 lines", "no such command: 'nope'", "no such command: 'wat'"],
        &["ok"],
    ];
    for w in [80, 160] {
        for said in texts {
            let mut ed = editor();
            for t in *said {
                ed.set_status(*t);
            }
            let draw = |ed: &mut Editor| {
                ed.focus_sync();
                crate::term::render(ed, &mut Vec::new(), w, 24).unwrap();
                crate::screen::text(ed.last_frame.as_ref().unwrap().0.as_slice(), w, 24).unwrap()
            };
            draw(&mut ed);
            let before = ed.screen.toast_card.get().expect("a card");
            ed.toast_focus_take();
            let s = draw(&mut ed);
            let (x, y, cw, h) = ed.screen.toast_card.get().expect("a card");
            assert_eq!((x, y, cw, h), before, "{w} columns, {} messages: the same rect", said.len());
            let bottom: String = s.lines().nth(y + h - 1).unwrap().chars().skip(x).take(cw).collect();
            assert!(bottom.starts_with('╰') && bottom.ends_with('╯'), "its line whole: {bottom:?}");
            assert!(!bottom.contains('…'), "whole pairs, never one cut: {bottom:?}");
            assert!(bottom.contains("esc close"), "the way out always said: {bottom:?}");
            // (in their order; one left out only where it wouldn't fit after the ones before it)
            let pair_w = |(k, d): &(&str, &str)| k.width() + 1 + d.width();
            let mut used = 0;
            let mut at = 0;
            for p in hints(&ed) {
                let said = format!("{} {}", p.0, p.1);
                match bottom[at..].find(&said) {
                    Some(i) => {
                        at += i + said.len();
                        used += pair_w(&p) + 3;
                    }
                    None => assert!(used + pair_w(&p) + 6 > cw, "{said} left out with room: {bottom:?}"),
                }
            }
            if said.len() == 1 && said[0] == "ok" {
                assert!(!bottom.contains("enter messages"), "too short for enter: {bottom:?}");
            }
        }
    }
}

/// A press on it focuses it; focused, a press on a message takes it in hand, `j`/`k` too; `enter` its home —
/// `:messages` with that message selected — the toast let go; a press elsewhere lets it go.
#[test]
fn a_focused_toasts_keys_and_presses() {
    let mut ed = editor();
    ed.set_status("one");
    ed.set_status("two");
    frame(&mut ed);
    let (x, y, ..) = ed.screen.toast_card.get().unwrap();
    press(&mut ed, x + 3, y + 1);
    let f = ed.toast_focus.expect("a press focuses it");
    let two = ed.toasts.last().unwrap().id;
    assert_eq!(f.selected, two, "the newest in hand");
    feed(&mut ed, "j");
    let one = two - 1;
    assert_eq!(ed.toast_focus.map(|f| f.selected), Some(one), "j: the one before");
    feed(&mut ed, "k");
    assert_eq!(ed.toast_focus.map(|f| f.selected), Some(two));
    let at_one = ed.screen.toast_rows.borrow().iter().find(|r| r.1 == one).map(|r| r.0).unwrap();
    press(&mut ed, x + 3, at_one);
    assert_eq!(ed.toast_focus.map(|f| f.selected), Some(one), "a press on it");
    feed(&mut ed, "x");
    assert!(ed.messages.last().unwrap().2.starts_with("x does nothing in the message"), "another key: said");
    feed(&mut ed, "<ret>");
    assert!(ed.toast_focus.is_none(), "enter: let go");
    assert!(ed.float_list_keys(crate::pane::GroupKind::Messages), "its home, with the keys");
    assert_eq!(ed.float_list_here(crate::pane::GroupKind::Messages), Some(one), "at that message");
    // A press elsewhere lets a focused one go
    feed(&mut ed, "q");
    ed.set_status("three");
    frame(&mut ed);
    feed(&mut ed, "<C-w>w");
    assert!(ed.toast_focus.is_some());
    press(&mut ed, 2, 2);
    assert!(ed.toast_focus.is_none() && ed.toast_letgo.is_some(), "a press on the code: let go");
}

/// An error past `ERROR_MIN` goes with a person's key — but focused, its keys are its own: held until it's let go.
#[test]
fn a_focused_error_is_held_until_let_go() {
    let mut ed = editor();
    ed.set_error("broke");
    frame(&mut ed);
    ed.toasts[0].at = std::time::Instant::now() - std::time::Duration::from_secs(5);
    feed(&mut ed, "<C-w>w");
    assert!(ed.toast_focus.is_some());
    feed(&mut ed, "l");
    let t = ed.toasts.iter().find(|t| t.text == "broke").unwrap();
    assert!(t.life > t.age() + std::time::Duration::from_secs(60), "held, not the key's fade");
    feed(&mut ed, "<esc>");
    assert!(ed.toast_focus.is_none());
    assert!(ed.toasts.iter().all(|t| t.life <= t.age() + LETGO), "esc: let go, fading");
}

/// A press on a card fading after a let-go focuses it again — you pressed it: it comes back held, the fade stopped. With
/// a prompt open the press is the prompt's: said, the toast not focused (never a focus the prompt takes back that frame).
#[test]
fn a_press_on_a_fading_toast_takes_it_back() {
    let mut ed = editor();
    ed.set_status("one");
    frame(&mut ed);
    feed(&mut ed, "<C-w>wq");
    assert!(ed.toast_focus.is_none() && ed.toast_letgo.is_some(), "q: let go, fading");
    let (x, y, ..) = ed.screen.toast_card.get().expect("drawn as it fades");
    press(&mut ed, x + 3, y + 1);
    assert!(ed.toast_focus.is_some(), "pressed: focused again");
    assert_eq!(ed.toast_letgo, None, "the fade stopped");
    let day = std::time::Duration::from_secs(3600);
    assert!(ed.toasts.iter().all(|t| t.life > t.age() + day), "held again");
    assert_eq!(ed.keys_to().region, Region::Toast(ed.toast_focus.unwrap().id));
    // A prompt open: the press is its, said
    feed(&mut ed, "q:");
    assert!(ed.prompt.is_some() && ed.toast_letgo.is_some());
    for what in ["fading", "up"] {
        let (x, y, ..) = ed.screen.toast_card.get().expect("drawn");
        press(&mut ed, x + 3, y + 1);
        assert!(ed.toast_focus.is_none(), "{what}, under a prompt: not focused");
        assert!(ed.prompt.is_some(), "the prompt stays");
        assert!(ed.messages.last().unwrap().2.starts_with("the prompt has the keys"), "{what}: said");
    }
}

/// A picker opening (`space f`'s) takes the keys from a focused toast — it lets go; a session switch too.
#[test]
fn what_takes_the_keys_lets_a_focused_toast_go() {
    let mut ed = editor();
    ed.set_status("one");
    frame(&mut ed);
    feed(&mut ed, "<C-w>w");
    assert!(ed.toast_focus.is_some());
    ed.open_picker(crate::picker::Picker::of("buffers", "things", Vec::new(), false), None);
    ed.focus_sync();
    assert!(ed.toast_focus.is_none() && ed.toast_letgo.is_some(), "a picker: let go");
}
