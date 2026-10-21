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
    assert!(ed.session.view.is_some(), "its view mode");
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
