use super::*;
use crate::config::Config;
use crate::editor::Editor;
use crate::event::{Mouse, MouseKind};
use crate::key::{Code, Key};
use crate::session::At;

const W: u16 = 160;
const H: u16 = 40;

fn feed(ed: &mut Editor, keys: &str) {
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let key: Key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        };
        ed.handle_key(key);
        frame(ed);
    }
}

/// A frame, as the main loop draws one after an event.
fn frame(ed: &mut Editor) {
    ed.focus_sync();
    crate::term::render(ed, &mut Vec::new(), W, H).unwrap();
}

/// The code, the side panel, a finished test run in the slot and the chat — every surface of the default tree.
fn every_surface() -> Editor {
    let mut ed = Editor::new(Config::default());
    ed.docs[0].text = ropey::Rope::from_str("fn main() {}\n");
    let root = ed.session.root.clone();
    ed.session.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Files, root));
    ed.session.test_run = Some(crate::testing::TestRun::finished("t", Vec::new(), vec!["ok".into()]));
    ed.session.chat = Some(crate::chat::Chat::new());
    frame(&mut ed);
    ed
}

fn rect(ed: &Editor, shows: Shows) -> crate::split::Rect {
    let id = ed.session.leaf(shows).unwrap();
    ed.screen.leaves.iter().find(|l| l.0 == id).map(|l| l.1).unwrap_or_default()
}

/// §2.1c, P2b's decision: a floating pane with the keys is `Session.at` — the picker, a prompt, which-key, a card —
/// risen over the docked pane (`under`), which gets them back when it goes; the docked surface it acts on is still
/// `is_at`'s; the state lists the floating pane with the keys, no leaf. A hop never starts from a floating pane:
/// the picker and a prompt take `C-w` as text, a card holding the keys refuses it aloud, and a card taking only its
/// answers yields to the prefix (which-key is the hop's own) — the hop goes from the pane under it, the card waits.
#[test]
fn a_floating_pane_has_the_keys_and_gives_them_back_to_the_pane_under_it() {
    let mut ed = every_surface();
    let floated = |ed: &Editor| match ed.session.at {
        At::Float(id) => ed.float_of(id),
        _ => None,
    };
    // (the prompt over the side panel: the panel is what it acts on, and has the keys again after it)
    ed.session.at = At::Side;
    feed(&mut ed, ":");
    assert_eq!(floated(&ed), Some(Floating::Prompt));
    assert_eq!((ed.session.under, ed.session.is_at(At::Side)), (At::Side, true));
    let state = ed.ui_state();
    let panes = &state.sessions.iter().find(|s| s.front).unwrap().panes;
    assert_eq!(panes.iter().filter(|p| p.at).map(|p| p.id.as_str()).collect::<Vec<_>>(), ["prompt"]);
    feed(&mut ed, "<esc>");
    assert_eq!(ed.session.at, At::Side, "esc: back to the side panel");
    // (which-key while a sequence is under way, from the code; the hop it leads to is the code's)
    ed.session.at = At::Code;
    feed(&mut ed, "<C-w>");
    assert_eq!(floated(&ed), Some(Floating::WhichKey));
    feed(&mut ed, "l");
    assert_eq!(ed.session.at, At::Chat, "C-w l from the code, which-key over it");
    // (a card taking its answers over the code: its answer gives the keys back; `C-w l` goes from the code)
    ed.session.at = At::Code;
    ed.push_offer(crate::offer::Offer::new(
        "End the review?".into(),
        "2 files not viewed.",
        "",
        String::new(),
        crate::offer::What::ReviewEnd { session: 0, started: std::time::Instant::now() },
    ));
    frame(&mut ed);
    let card = ed.offers[0].id;
    assert_eq!(floated(&ed), Some(Floating::Card(card)));
    let id = ed.session.at;
    feed(&mut ed, "<C-w>");
    assert_eq!(floated(&ed), Some(Floating::WhichKey), "the card yields to the prefix");
    feed(&mut ed, "l");
    assert_eq!((ed.session.at, ed.offers.len()), (At::Chat, 1), "from the code; the card waits");
    feed(&mut ed, "<C-w>h");
    assert_eq!(ed.session.at, id, "back on the code: the card's again, the same pane");
    feed(&mut ed, "n");
    assert_eq!((ed.offers.len(), ed.session.at), (0, At::Code), "answered: the code's again");
    // (the picker opened from the code: a pane id of its own, the code's again after esc)
    feed(&mut ed, " b");
    assert_eq!(floated(&ed), Some(Floating::Picker));
    assert_ne!(ed.session.at, id, "a pane id of its own");
    feed(&mut ed, "<esc>");
    assert_eq!(ed.session.at, At::Code);
}

/// §2.6 in P2b: `C-w v` from the chat splits the code pane last focused, the keys to the new one; from the
/// terminal's program it's said and nothing splits — a second terminal comes with the terminal work.
#[test]
fn c_w_v_off_the_code_splits_the_code_and_on_the_terminal_says_it_waits() {
    let mut ed = every_surface();
    let first = ed.session.focus;
    ed.session.at = At::Chat;
    feed(&mut ed, "<C-w>v");
    assert_eq!(ed.session.views.len(), 2);
    assert_eq!(ed.session.at, At::Code);
    assert_ne!(ed.session.focus, first, "the new pane has the keys");
    let _sent = crate::terminal::stand_in(&mut ed, 10, 80, 100);
    frame(&mut ed);
    assert!(ed.session.program_has_keys());
    feed(&mut ed, "<C-w>s");
    assert_eq!(ed.session.views.len(), 2, "nothing split");
    assert!(ed.toasts.iter().any(|t| t.text.starts_with("one terminal for now")), "said");
    assert!(ed.session.program_has_keys(), "the program keeps the keys");
}

/// §2.6: `C-w L` swaps this code pane with the one that way (the keys staying with it), `C-w t` turns the code's
/// split, and the panels keep their places (said); `C-w w` goes round every kind of pane on screen.
#[test]
fn c_w_swaps_and_turns_the_codes_panes_and_c_w_w_goes_round_every_kind() {
    let mut ed = every_surface();
    let a = ed.session.focus;
    feed(&mut ed, "<C-w>v");
    let b = ed.session.focus;
    let order = |ed: &Editor| ed.code_leaves();
    assert_eq!(order(&ed), [a, b]);
    feed(&mut ed, "<C-w>H");
    assert_eq!((order(&ed), ed.session.focus), (vec![b, a], b), "swapped, the keys with it");
    let (ra, rb) = (rect_of(&ed, a), rect_of(&ed, b));
    assert!(rb.x < ra.x && ra.y == rb.y, "b on the left now");
    feed(&mut ed, "<C-w>t");
    let (ra, rb) = (rect_of(&ed, a), rect_of(&ed, b));
    assert!(rb.y < ra.y && ra.x == rb.x, "stacked: {ra:?} {rb:?}");
    assert!(rect(&ed, Shows::Chat).w > 0 && rect(&ed, Shows::Side).w > 0, "the panels stay");
    // (`C-w o` then `C-w L` with no frame between: the pane it closed, still right of this one in the last frame, is
    // no pane to trade with — the simulation's seed 135 found the tree left holding it, and the next `C-w q` fell over)
    feed(&mut ed, "<C-w>t");
    assert!(rect_of(&ed, b).x < rect_of(&ed, a).x, "side by side again, a to the right");
    ed.handle_key("C-w".parse().unwrap());
    ed.handle_key("o".parse().unwrap());
    ed.handle_key("C-w".parse().unwrap());
    ed.handle_key("L".parse().unwrap());
    frame(&mut ed);
    assert_eq!(ed.code_leaves(), vec![ed.session.focus], "the tree holds the one pane left");
    feed(&mut ed, "<C-w>v<C-w>q");
    assert_eq!(ed.session.views.len(), 1);
    ed.session.at = At::Chat;
    feed(&mut ed, "<C-w>J");
    assert!(ed.toasts.iter().any(|t| t.text.starts_with("the panels keep their places")), "said");
    // (round every kind)
    let mut seen = Vec::new();
    for _ in 0..8 {
        feed(&mut ed, "<C-w>w");
        let id = ed.session.at_pane().unwrap();
        seen.push(ed.session.shows(id).unwrap());
    }
    for kind in [Shows::Side, Shows::Code, Shows::Slot, Shows::Chat] {
        assert!(seen.contains(&kind), "{kind:?} in {seen:?}");
    }
}

/// `C-w s` from the default tree: the two code panes are a split of their own in the code's column (not the slot's
/// siblings), so `C-w t` turns them side by side — and back — the slot staying under them.
#[test]
fn c_w_s_then_c_w_t_turns_the_two_stacked_code_panes() {
    let mut ed = every_surface();
    let a = ed.session.focus;
    feed(&mut ed, "<C-w>s");
    let b = ed.session.focus;
    let (ra, rb) = (rect_of(&ed, a), rect_of(&ed, b));
    assert!(ra.y < rb.y && ra.x == rb.x, "stacked: {ra:?} {rb:?}");
    feed(&mut ed, "<C-w>t");
    let (ra, rb) = (rect_of(&ed, a), rect_of(&ed, b));
    assert!(ra.x < rb.x && ra.y == rb.y, "side by side: {ra:?} {rb:?}");
    assert!(rect(&ed, Shows::Slot).y > ra.y + ra.h - 1, "the slot under them both");
    assert!(!ed.toasts.iter().any(|t| t.text.starts_with("one code pane")), "nothing said");
    feed(&mut ed, "<C-w>t");
    let (ra, rb) = (rect_of(&ed, a), rect_of(&ed, b));
    assert!(ra.y < rb.y && ra.x == rb.x, "stacked again");
}

fn rect_of(ed: &Editor, id: crate::split::PaneId) -> crate::split::Rect {
    ed.screen.leaves.iter().find(|l| l.0 == id).map(|l| l.1).unwrap_or_default()
}

/// A finished run of two tests — one passed, one failed with a message and a place — in the slot.
fn with_results(ed: &mut Editor) {
    use crate::test_results::{Case, Status};
    let case = |name: &str, status, message: &[&str]| Case {
        name: name.into(),
        status,
        secs: None,
        message: message.iter().map(|m| m.to_string()).collect(),
        at: (status == Status::Failed).then(|| (ed.session.root.join("src/lib.rs"), 3)),
    };
    let cases =
        vec![case("adds", Status::Passed, &[]), case("subtracts", Status::Failed, &["left != right"])];
    ed.session.test_run =
        Some(crate::testing::TestRun::finished("math", cases, vec!["test result: FAILED".into()]));
    frame(ed);
}

/// §2.1b: the tests panel is a linked group — the list and the detail two panes in the slot, under one header: `C-w
/// h/l` and `tab`/`S-tab` between them, the list's own keys moving its selection (which the detail follows), `esc`
/// to the code, one `q` closing the group; the state lists both members, the slot's keys the detail's.
#[test]
fn the_tests_panel_is_a_linked_group_of_two_panes() {
    let mut ed = every_surface();
    with_results(&mut ed);
    let g = ed.session.tests;
    let ids: Vec<_> = ed.screen.leaves.iter().map(|l| l.0).collect();
    assert!(ids.contains(&g.list) && ids.contains(&g.detail), "both members laid out: {ids:?}");
    assert!(!ids.contains(&ed.session.leaf(Shows::Slot).unwrap()), "the slot's leaf is its members");
    let list = ed.screen.leaves.iter().find(|l| l.0 == g.list).unwrap().1;
    let detail = ed.screen.leaves.iter().find(|l| l.0 == g.detail).unwrap().1;
    assert_eq!((list.y, list.h), (detail.y, detail.h), "side by side, one header over both");
    assert_eq!(detail.x, list.x + list.w + 1, "a line between them");
    // (the list, by a hop from the detail)
    ed.focus_pane(g.detail);
    frame(&mut ed);
    assert!(
        ed.session.is_at(At::Slot) && crate::rundoc::has_keys(&ed).is_some(),
        "the detail: the run's output"
    );
    feed(&mut ed, "<C-w>h");
    assert_eq!(ed.session.at, At::List(g.list));
    assert_eq!(ed.keys_to().region, crate::uistate::Region::List(crate::sizes::Slot::Test));
    assert_eq!(ed.keys_to().to, format!("pane#{}", g.list));
    // (its selection drives the detail: the failure picked, then the output again)
    feed(&mut ed, "j");
    assert_eq!(ed.session.test_run.as_ref().unwrap().selected, 1);
    assert!(shown(&mut ed).contains("left != right"), "the failure beside it");
    feed(&mut ed, "k");
    assert!(!shown(&mut ed).contains("left != right"), "a passed test: the output");
    // (tab to the detail, S-tab back; the one tab there is rounds its two members)
    feed(&mut ed, "<tab>");
    assert!(ed.session.is_at(At::Slot), "tab: the detail");
    feed(&mut ed, "<S-tab>");
    assert_eq!(ed.session.at, At::List(g.list), "S-tab: the list");
    // (the state: both members, the keys the list's)
    let state = ed.ui_state();
    let panes = &state.sessions.iter().find(|s| s.front).unwrap().panes;
    let at: Vec<_> = panes.iter().filter(|p| p.at).map(|p| p.shows.as_str()).collect();
    assert_eq!(at, ["tests:list"]);
    assert!(panes.iter().any(|p| p.shows == "tests:detail" && p.rect.is_some()));
    // (a click on a row picks it and gives the list the keys; on its title, the detail's; past the last result,
    // the list's, the selection where it was)
    let click = |ed: &mut Editor, x: usize, y: usize| {
        ed.handle_mouse(Mouse { kind: MouseKind::Down, x: x as u16, y: y as u16, alt: false });
        frame(ed);
    };
    let (y0, _, first) = ed.screen.tests_list.get().unwrap();
    let list = ed.screen.leaves.iter().find(|l| l.0 == g.list).unwrap().1;
    ed.session.at = At::Code;
    click(&mut ed, list.x + 3, y0 + 1 - first);
    assert_eq!((ed.session.at, ed.session.test_run.as_ref().unwrap().selected), (At::List(g.list), 1));
    click(&mut ed, list.x + 3, y0 - 1);
    assert!(ed.session.is_at(At::Slot), "the title row: the detail's keys");
    click(&mut ed, list.x + 3, y0 + 2);
    assert_eq!((ed.session.at, ed.session.test_run.as_ref().unwrap().selected), (At::List(g.list), 1));
    // (a word the list has no use for is said; esc to the code; back, and one q closes the group)
    feed(&mut ed, "x");
    assert!(ed.toasts.iter().any(|t| t.text.contains("the tests' list")), "said");
    feed(&mut ed, "<esc>");
    assert_eq!(ed.session.at, At::Code);
    ed.focus_pane(g.list);
    feed(&mut ed, "q");
    assert!(ed.session.test_run.is_none() || ed.slot() != Some(crate::sizes::Slot::Test), "closed");
    assert_eq!(ed.session.at, At::Code);
}

/// The frame as text.
fn shown(ed: &mut Editor) -> String {
    frame(ed);
    crate::screen::text(ed.last_frame.as_ref().unwrap().0.as_slice(), W, H).unwrap()
}

/// A session's first tree holds every surface, each once, the code pane in the middle over the slot.
#[test]
fn the_default_tree_holds_every_surface_once() {
    let (panes, tree) = default_layout(7);
    assert_eq!(tree.leaves(), [8, 9, 7, 10, 11]);
    assert_eq!(panes.iter().map(|p| p.shows).collect::<Vec<_>>(), SURFACES);
    for s in SURFACES {
        assert_eq!(Shows::of_name(s.name()), Some(s));
    }
    let ed = Editor::new(Config::default());
    assert_eq!(ed.session.split.leaves().len(), 1 + SURFACES.len(), "a new session's tree");
    assert!(ed.next_pane > ed.session.panes.iter().map(|p| p.id).max().unwrap(), "ids past its panes'");
}

/// §2.5: the screen is drawn from the tree's rects — the side panel on the left, the code over the slot between,
/// the chat on the right, each where the frame's measures say (as before the tree: the snapshots hold the cells).
#[test]
fn the_default_layout_is_drawn_from_the_trees_rects() {
    let ed = every_surface();
    let (side, slot, chat) = (rect(&ed, Shows::Side), rect(&ed, Shows::Slot), rect(&ed, Shows::Chat));
    let s = &ed.screen;
    assert_eq!((side.x, side.w), (0, s.side_w));
    assert_eq!((chat.x, chat.w), (W as usize - s.chat_w, s.chat_w));
    assert_eq!(s.chat_x, Some(chat.x));
    assert_eq!((slot.x, slot.h), (s.slot_x, s.bottom_h));
    let code = s.panes.iter().find(|p| p.0 == ed.session.focus).unwrap().1;
    assert_eq!(rect(&ed, Shows::Code), code, "the code pane where the tree put it");
    assert_eq!((code.x, code.x + code.w), (side.w, chat.x), "between them");
    assert_eq!(slot.y, code.y + code.h, "the slot right under the code");
    let body = s.text_top + s.body_h;
    assert_eq!((side.h, chat.h), (body, body), "the side columns from the top to the status line");
}

/// §2.6: `C-w h/j/k/l` go to the pane that way whatever it shows — the chat, the side panel, the slot (its view
/// mode), a code pane — and toward the code to the code pane last focused; nothing that way is said.
#[test]
fn c_w_hops_to_every_kind_of_pane_and_back() {
    let mut ed = every_surface();
    feed(&mut ed, "<C-w>v");
    let (left, right) = (ed.code_leaves()[0], ed.code_leaves()[1]);
    assert_eq!(ed.session.focus, right);
    feed(&mut ed, "<C-w>l");
    assert_eq!(ed.session.at, At::Chat, "right of the code: the chat");
    feed(&mut ed, "<C-w>h");
    assert_eq!((ed.session.at, ed.session.focus), (At::Code, right), "back: the pane it came from");
    feed(&mut ed, "<C-w>h");
    assert_eq!(ed.session.focus, left);
    feed(&mut ed, "<C-w>h");
    assert_eq!(ed.session.at, At::Side, "left of the code: the side panel");
    feed(&mut ed, "<C-w>h");
    assert_eq!(ed.session.at, At::Side, "nothing further");
    assert_eq!(ed.status.as_ref().map(|s| s.0.as_str()), Some("nothing to the left"));
    feed(&mut ed, "<C-w>l");
    assert_eq!((ed.session.at, ed.session.focus), (At::Code, left));
    feed(&mut ed, "<C-w>j");
    assert_eq!(ed.session.at, At::Slot, "below: the slot");
    assert!(crate::rundoc::has_keys(&ed).is_some(), "its output");
    feed(&mut ed, "<C-w>l");
    assert_eq!(ed.session.at, At::Chat, "right of the slot: the chat (it runs the body's height)");
    feed(&mut ed, "<C-w>h");
    assert_eq!(
        (ed.session.at, ed.session.focus),
        (At::Code, right),
        "the last focused isn't beside it: the nearest"
    );
    feed(&mut ed, "<C-w>j<C-w>h");
    assert_eq!(ed.session.at, At::Side, "left of the slot: the side panel");
    feed(&mut ed, "<C-w>l<C-w>j<C-w>k");
    assert_eq!((ed.session.at, ed.session.focus), (At::Code, left), "up from the slot");
}

/// `C-w z` fills the screen with a pane — `Zoom(PaneId)`, the chat's, a code pane's — and the hops see that one.
#[test]
fn the_zoom_is_a_pane() {
    let mut ed = every_surface();
    feed(&mut ed, "<C-w>l<C-w>z");
    let chat = ed.session.leaf(Shows::Chat).unwrap();
    assert_eq!(ed.zoomed(), Some(crate::sizes::Zoom(chat)));
    assert_eq!(ed.screen.leaves, [(chat, rect(&ed, Shows::Chat))], "that pane alone");
    assert_eq!(rect(&ed, Shows::Chat).w, W as usize);
    feed(&mut ed, "<C-w>h");
    assert_eq!(ed.session.at, At::Chat, "nothing beside it while it fills the screen");
    feed(&mut ed, "<C-w>z<C-w>h<C-w>z");
    assert_eq!(ed.zoomed(), Some(crate::sizes::Zoom(ed.session.focus)));
    assert_eq!(ed.screen.panes.len(), 1);
    assert_eq!(ed.screen.panes[0].1.w, W as usize, "the code pane the whole width");
}

/// §5.4: a split of the code's you made carries its own sizes — `C-w > <` from one of its panes, a drag of the
/// divider between them — in the tree (`Share::Cells`), said; the chat's width stays; `C-w =` even again. With no
/// code pane beside it, `C-w >` is the chat's as ever.
#[test]
fn your_split_is_sized_by_keys_and_by_its_divider() {
    let mut ed = every_surface();
    let chat = ed.screen.chat_w;
    feed(&mut ed, "<C-w>v");
    let (left, right) = (ed.code_leaves()[0], ed.code_leaves()[1]);
    let width = |ed: &Editor, id| ed.screen.leaves.iter().find(|l| l.0 == id).unwrap().1.w;
    let (l0, r0) = (width(&ed, left), width(&ed, right));
    assert!(l0.abs_diff(r0) <= 1, "even: {l0} {r0}");
    feed(&mut ed, "<C-w>>");
    assert_eq!(width(&ed, right), r0 + crate::sizes::STEP_COLS, "the pane in hand wider");
    assert_eq!(width(&ed, left) + width(&ed, right), l0 + r0, "its neighbour gives the room");
    assert_eq!(ed.screen.chat_w, chat, "the chat's width untouched");
    let said = ed.toasts.last().map(|t| t.text.clone()).unwrap_or_default();
    assert_eq!(said, format!("pane {} columns · C-w = automatic", r0 + crate::sizes::STEP_COLS));
    assert!(ed.session.split.sized());
    // The divider: the cell after the left pane, dragged 10 to the left
    let l = ed.screen.leaves.iter().find(|p| p.0 == left).unwrap().1;
    let (x, y) = (l.x + l.w, l.y + 3);
    assert_eq!(ed.border_at(x, y), Some(crate::sizes::Border::Divider(left)));
    let mouse = |ed: &mut Editor, kind, x: usize| {
        ed.handle_mouse(Mouse { kind, x: x as u16, y: y as u16, alt: false });
        frame(ed);
    };
    mouse(&mut ed, MouseKind::Down, x);
    mouse(&mut ed, MouseKind::Drag, x - 10);
    assert_eq!(width(&ed, left), l.w - 10, "follows the mouse");
    mouse(&mut ed, MouseKind::Up, x - 10);
    assert!(ed.toasts.last().is_some_and(|t| t.text.starts_with(&format!("pane {} columns", l.w - 10))));
    feed(&mut ed, "<C-w>=");
    assert!(!ed.session.split.sized(), "even again");
    assert!(width(&ed, left).abs_diff(width(&ed, right)) <= 1);
    // One code pane: the chat's
    feed(&mut ed, "<C-w>o<C-w>>");
    assert_eq!(ed.screen.chat_w, chat + crate::sizes::STEP_COLS, "no split of yours: the chat wider");
}

/// The user's decision (2026-10-21, plan §2.6): `C-w` is tarae's inside the terminal's program too — its next key
/// is the window table's (which-key up, `C-w k` to the code), `C-w C-w` sends the program one `C-w`, `esc` cancels
/// it with nothing sent; a `[keys.terminal]` row hands it back whole.
#[test]
fn c_w_is_tarae_s_in_the_terminal_and_c_w_c_w_the_program_s() {
    let mut ed = Editor::new(Config::default());
    let sent = crate::terminal::stand_in(&mut ed, 10, 80, 100);
    frame(&mut ed);
    let drained = |sent: &std::sync::mpsc::Receiver<Vec<u8>>| sent.try_iter().flatten().collect::<Vec<u8>>();
    drained(&sent);
    feed(&mut ed, "<C-w>");
    assert!(drained(&sent).is_empty(), "the prefix isn't the program's");
    assert_eq!(ed.keys_to().region.name(), "code", "its next key the window table's");
    assert!(ed.floating().contains(&Floating::WhichKey), "which-key up over the program");
    feed(&mut ed, "<C-w>");
    assert_eq!(drained(&sent), b"\x17", "C-w C-w: one C-w to the program");
    assert!(ed.pending.is_empty() && ed.session.program_has_keys());
    feed(&mut ed, "<C-w><esc>");
    assert!(drained(&sent).is_empty(), "esc: cancelled, nothing sent");
    assert!(ed.pending.is_empty() && ed.session.program_has_keys(), "the program keeps the keys");
    feed(&mut ed, "<C-w>k");
    assert_eq!(ed.session.at, At::Code, "C-w k: up to the code");
    feed(&mut ed, "<C-w>j");
    assert_eq!(ed.session.at, At::Slot, "and back down: its view mode");
    // Handed back: every `C-w` the program's
    let (config, warnings) = crate::config::parse("[keys.terminal]\n\"C-w\" = \"to_program\"\n");
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut ed = Editor::new(config);
    let sent = crate::terminal::stand_in(&mut ed, 10, 80, 100);
    frame(&mut ed);
    drained(&sent);
    feed(&mut ed, "<C-w>");
    assert_eq!(drained(&sent), b"\x17");
    assert!(ed.pending.is_empty());
    // The window keys moved to `C-a` (your `[keys.normal]`): the program's `C-w` starts that table, `C-a` again
    // is the program's one; `C-w` after it is no stray key to the program
    let toml = "[keys.normal.\"C-a\"]\nh = \"jump_view_left\"\nk = \"jump_view_up\"\n\n[keys.normal]\n\"C-w\" = \"normal_mode\"\n";
    let (config, warnings) = crate::config::parse(toml);
    assert!(warnings.is_empty(), "{warnings:?}");
    let mut ed = Editor::new(config);
    let a: Key = "C-a".parse().unwrap();
    assert_eq!(ed.keymaps.window_prefix(), Some(a));
    let sent = crate::terminal::stand_in(&mut ed, 10, 80, 100);
    frame(&mut ed);
    drained(&sent);
    feed(&mut ed, "<C-w>");
    assert_eq!(ed.pending, [a], "the window table where yours put it");
    feed(&mut ed, "<C-a>");
    assert_eq!(drained(&sent), b"\x01", "the prefix again: one to the program");
    feed(&mut ed, "<C-w><C-w>");
    assert!(drained(&sent).is_empty(), "no stray C-w: the window table had none");
    feed(&mut ed, "<C-w>k");
    assert_eq!(ed.session.at, At::Code);
}
