use crate::config::Config;
use crate::editor::Editor;
use crate::event::{Mouse, MouseKind};
use crate::key::{Code, Key};
use crate::pane::{Frame, GroupKind, Place, Shows, Side};
use crate::session::At;
use crate::sizes::Slot;
use crate::split::Rect;

const W: u16 = 160;
const H: u16 = 40;

fn feed(ed: &mut Editor, keys: &str) {
    feed_in(ed, keys, W, H);
}

/// Keys, each with a frame `w` × `h` after it.
fn feed_in(ed: &mut Editor, keys: &str, w: u16, h: u16) {
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let key: Key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        };
        ed.handle_key(key);
        frame_in(ed, w, h);
    }
}

fn frame_in(ed: &mut Editor, w: u16, h: u16) {
    ed.focus_sync();
    crate::term::render(ed, &mut Vec::new(), w, h).unwrap();
}

fn shown(ed: &mut Editor, w: u16, h: u16) -> String {
    frame_in(ed, w, h);
    crate::screen::text(ed.last_frame.as_ref().unwrap().0.as_slice(), w, h).unwrap()
}

fn mouse(ed: &mut Editor, kind: MouseKind, x: usize, y: usize) {
    ed.handle_mouse(Mouse { kind, x: x as u16, y: y as u16, alt: false, ctrl: false });
    frame_in(ed, W, H);
}

/// The code, a finished test run of two tests (one failed, at a place) and a finished task behind it in the slot, the
/// tests' list with the keys.
fn tests_and_a_task() -> (crate::scratch::Scratch, Editor) {
    use crate::test_results::{Case, Status};
    let dir = crate::scratch::Scratch::new("float");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(dir.join("src/lib.rs"), "a\nb\nc\nd\n").unwrap();
    let mut ed = Editor::new(Config::default());
    ed.session.root = dir.to_path_buf();
    ed.docs[0].text = ropey::Rope::from_str(&"fn main() {}\n".repeat(60));
    let t = crate::task::Task { name: "check".into(), cmd: "cargo check".into(), cwd: dir.to_path_buf() };
    let mut r = crate::task::Run::for_test(t);
    r.state = crate::task::State::Passed;
    ed.session.task_run = Some(r);
    let case = |name: &str, status, at: bool| Case {
        name: name.into(),
        status,
        secs: None,
        message: if at { vec!["left != right".into()] } else { Vec::new() },
        at: at.then(|| (dir.join("src/lib.rs"), 2)),
    };
    let cases = vec![case("adds", Status::Passed, false), case("subtracts", Status::Failed, true)];
    let mut run = crate::testing::TestRun::finished("math", cases, vec!["test result: FAILED".into()]);
    run.started = crate::clock::now();
    ed.session.test_run = Some(run);
    frame_in(&mut ed, W, H);
    ed.list_focus(GroupKind::Tests);
    frame_in(&mut ed, W, H);
    (dir, ed)
}

fn tests(ed: &Editor) -> crate::pane::Group {
    ed.session.group(GroupKind::Tests).unwrap()
}

fn leaf(ed: &Editor, id: crate::split::PaneId) -> Rect {
    ed.screen.leaves.iter().find(|l| l.0 == id).map(|l| l.1).unwrap_or_default()
}

fn drawn(ed: &Editor, kind: GroupKind) -> Rect {
    ed.screen.floating.borrow().iter().find(|f| f.0 == kind).map(|f| f.1).expect("drawn floating")
}

fn inside(f: Rect, r: Rect) -> bool {
    f.x <= r.x && r.x + r.w <= f.x + f.w && f.y <= r.y && r.y + r.h <= f.y + f.h
}

/// P3 step 7 (§3.1–3.3): `C-w e` from the tests' list floats the group — centred, 3/5 × 1/2 of the screen, its own
/// pane out of the tree (`group:tests`), its members laid out inside its frame, named in the frame's top line; the slot
/// shows the task, the keys stay in the list, which moves as it did, `tab` to its detail. `C-w e` again docks it back
/// where it came from: the slot; from a docked side, that side.
#[test]
fn c_w_e_floats_the_tests_group_and_docks_it_back() {
    let (_dir, mut ed) = tests_and_a_task();
    feed(&mut ed, "<C-w>e");
    let g = tests(&ed);
    assert_eq!((g.place, g.back), (Place::Floating(Frame::CENTRED), Place::Slot));
    assert_eq!(ed.session.shows(g.host), Some(Shows::Group(GroupKind::Tests)), "a pane of its own");
    assert!(!ed.session.split.holds(g.host), "out of the tree");
    let f = drawn(&ed, GroupKind::Tests);
    // (160 × 39 rows above the status line: 96 × 20 from column 32, row 10)
    assert_eq!(f, Rect { x: 32, y: 10, w: 96, h: 20 });
    assert!(inside(f, leaf(&ed, g.list)) && leaf(&ed, g.list).w > 0, "its list in its frame");
    assert_eq!(ed.slot_shown(), Some(Slot::Task), "the slot shows the task");
    assert_eq!(ed.session.at, At::List(g.list), "the keys stay in the list");
    assert_eq!(ed.keys_to().region, crate::uistate::Region::List(GroupKind::Tests));
    let s = shown(&mut ed, W, H);
    let top = s.lines().nth(f.y).unwrap();
    assert!(top.contains("╭─ tests") && top.contains("results"), "named in its top line: {top}");
    let bottom = s.lines().nth(f.y + f.h - 1).unwrap();
    assert!(
        bottom.contains("╰─") && bottom.contains('┴'),
        "its bottom line, the members' line met: {bottom}"
    );
    feed(&mut ed, "j");
    assert_eq!(ed.session.test_run.as_ref().unwrap().selected, 1, "the list moves");
    feed(&mut ed, "<tab>");
    assert_eq!((ed.session.at, ed.session.hand), (At::Slot, Some(GroupKind::Tests)), "tab: its detail");
    assert_eq!(crate::rundoc::has_keys(&ed), Some(Slot::Test), "the tests' output, not the task's");
    // Back where it came from
    feed(&mut ed, "<C-w>e");
    assert_eq!(tests(&ed).place, Place::Slot);
    assert_eq!(ed.slot_shown(), Some(Slot::Test), "in front of the slot");
    assert!(ed.screen.floating.borrow().is_empty());
    assert!(!ed.session.panes.iter().any(|p| p.shows == Shows::Group(GroupKind::Tests)), "its pane gone");
    // From a docked side: floated, then back to that side
    feed(&mut ed, "<C-w>L<C-w>e");
    assert_eq!(tests(&ed).back, Place::Docked(Side::Right));
    assert!(!ed.session.split.holds(tests(&ed).host), "its leaf left the tree");
    feed(&mut ed, "<C-w>e");
    assert_eq!(tests(&ed).place, Place::Docked(Side::Right));
    assert!(ed.docked_in_sight(GroupKind::Tests));
    // From a float `C-w H J K L` dock it on that side, `J` the slot
    feed(&mut ed, "<C-w>e<C-w>K");
    assert_eq!(tests(&ed).place, Place::Docked(Side::Top));
    feed(&mut ed, "<C-w>e<C-w>J");
    assert_eq!(tests(&ed).place, Place::Slot);
    // One meaning (user, 2026-10-23): where nothing in hand floats — the code, the side panel — it says so and
    // stays; never the side panel's key
    feed(&mut ed, "<esc><C-w>e");
    assert_eq!((ed.session.at, ed.session.sidebar.is_none()), (At::Code, true), "the code: nothing moves");
    assert!(ed.messages.last().unwrap().2.contains("nothing here floats"), "said");
    feed(&mut ed, " e<C-w>e");
    assert_eq!(ed.session.at, At::Side, "the side panel keeps the keys");
    assert!(ed.messages.last().unwrap().2.contains("nothing here floats"), "said there too");
}

/// The whole screen is its frame (user, 2026-10-23): kept as a share of it, clamped onto it after a resize — moved to
/// the right edge at 160, it's still whole at 80; no smaller than 40 × 8 while the screen has room, the whole width
/// where it hasn't.
#[test]
fn the_frame_is_clamped_onto_the_screen_after_a_resize() {
    let (_dir, mut ed) = tests_and_a_task();
    feed(&mut ed, "<C-w>e<C-w>m");
    for _ in 0..20 {
        feed(&mut ed, "l");
    }
    feed(&mut ed, "j<ret>");
    let f = drawn(&ed, GroupKind::Tests);
    assert_eq!(f.x + f.w, W as usize, "at the right edge: {f:?}");
    for (w, h) in [(80u16, 24u16), (60, 12), (30, 10)] {
        frame_in(&mut ed, w, h);
        let f = drawn(&ed, GroupKind::Tests);
        assert!(f.x + f.w <= w as usize && f.y + f.h < h as usize, "{w}×{h}: {f:?}");
        assert!(f.w >= 40.min(w as usize) && f.h >= 8.min(h as usize - 1), "{w}×{h}: {f:?}");
    }
    assert_eq!(drawn(&ed, GroupKind::Tests).w, 30, "narrower than 40: the whole width");
    // (and back: its share again)
    frame_in(&mut ed, W, H);
    assert_eq!(drawn(&ed, GroupKind::Tests).x + drawn(&ed, GroupKind::Tests).w, W as usize);
}

/// `C-w m`: `h j k l` and the arrows move it 4 columns / 2 rows a press, its keys in its bottom line; any other key is
/// said, nothing moving; `enter`, `esc` or `C-w m` end it. Not on a floating pane: said.
#[test]
fn c_w_m_moves_a_floating_pane() {
    let (_dir, mut ed) = tests_and_a_task();
    feed(&mut ed, "<C-w>m");
    assert!(ed.float_moving.is_none() && said(&ed).contains("moves a floating pane"));
    feed(&mut ed, "<C-w>e");
    let was = drawn(&ed, GroupKind::Tests);
    feed(&mut ed, "<C-w>m");
    assert_eq!(ed.keys_to().region, crate::uistate::Region::Move);
    let s = shown(&mut ed, W, H);
    assert!(
        s.lines().nth(was.y + was.h - 1).unwrap().contains("h j k l move"),
        "its keys in its bottom line"
    );
    feed(&mut ed, "lj<left>l");
    let now = drawn(&ed, GroupKind::Tests);
    assert_eq!((now.x, now.y, now.w, now.h), (was.x + 4, was.y + 2, was.w, was.h), "l j ← l");
    feed(&mut ed, "x");
    assert!(said(&ed).contains("x does nothing"), "{}", said(&ed));
    assert_eq!(drawn(&ed, GroupKind::Tests), now, "nothing moved");
    feed(&mut ed, "<esc>");
    assert!(ed.float_moving.is_none());
    assert_eq!(ed.session.at, At::List(tests(&ed).list), "the keys back in its list");
    feed(&mut ed, "<C-w>mk<C-w>m");
    assert!(ed.float_moving.is_none(), "C-w m again ends it");
    assert_eq!(drawn(&ed, GroupKind::Tests).y, now.y - 2);
    // (the window prefix goes on while it moves: `C-w e` docks it back, the move over with it)
    feed(&mut ed, "<C-w>m<C-w>e");
    assert_eq!((tests(&ed).place, ed.float_moving), (Place::Slot, None));
    // The keys leaving it end the move (reviewer, 7a): a hop — `h j k l` and `esc` the code's again, said —…
    feed(&mut ed, "<C-w>e<C-w>m<C-w>h");
    assert_eq!((ed.session.at, ed.float_moving), (At::Code, None), "a hop out ends it");
    assert!(said(&ed).contains("moving ended"), "said: {}", said(&ed));
    let (frame, line) = (drawn(&ed, GroupKind::Tests), ed.cursor_line());
    feed(&mut ed, "j");
    assert_eq!((drawn(&ed, GroupKind::Tests), ed.cursor_line()), (frame, line + 1), "j: the code's");
    // …and a press elsewhere
    ed.list_focus(GroupKind::Tests);
    feed(&mut ed, "<C-w>m");
    assert!(ed.float_moving.is_some());
    mouse(&mut ed, MouseKind::Down, 10, 3);
    mouse(&mut ed, MouseKind::Up, 10, 3);
    assert_eq!((ed.session.at, ed.float_moving), (At::Code, None), "a press on the code ends it");
}

/// `C-w < > - +` from its member size it (its left and top edges staying), `C-w =` puts it back in the middle — kept
/// with its place.
#[test]
fn c_w_sizes_a_floating_group_and_c_w_eq_centres_it() {
    let (_dir, mut ed) = tests_and_a_task();
    feed(&mut ed, "<C-w>e");
    let was = drawn(&ed, GroupKind::Tests);
    feed(&mut ed, "<C-w>><C-w>+");
    let now = drawn(&ed, GroupKind::Tests);
    let (cols, rows) = (crate::sizes::STEP_COLS, crate::sizes::STEP_ROWS);
    assert_eq!((now.x, now.y, now.w, now.h), (was.x, was.y, was.w + cols, was.h + rows));
    assert!(said(&ed).contains("tests"), "said: {}", said(&ed));
    feed(&mut ed, "<C-w><lt><C-w>-<C-w>-");
    assert_eq!(drawn(&ed, GroupKind::Tests).h, was.h - rows);
    feed(&mut ed, "<C-w>=");
    assert_eq!(drawn(&ed, GroupKind::Tests), was, "in the middle again");
    assert_eq!(tests(&ed).place, Place::Floating(Frame::CENTRED));
}

/// The mouse: its top line off the names moves it, its edges size it, a press inside gives it the keys and nothing
/// under it is pressed; the code's cursor under it is hidden.
#[test]
fn a_floating_frame_is_dragged_by_its_top_line_and_sized_by_its_edges() {
    let (_dir, mut ed) = tests_and_a_task();
    feed(&mut ed, "<C-w>e<esc>");
    assert_eq!(ed.session.at, At::Code);
    let f = drawn(&ed, GroupKind::Tests);
    // (its top line, past its names: the stretch of line before the right corner)
    let (x, y) = (f.x + f.w - 3, f.y);
    mouse(&mut ed, MouseKind::Down, x, y);
    mouse(&mut ed, MouseKind::Drag, x - 10, y + 3);
    mouse(&mut ed, MouseKind::Up, x - 10, y + 3);
    let moved = drawn(&ed, GroupKind::Tests);
    assert_eq!((moved.x, moved.y, moved.w, moved.h), (f.x - 10, f.y + 3, f.w, f.h));
    assert!(matches!(ed.session.at, At::List(_) | At::Slot), "a press on it gave it the keys");
    // Its right edge sizes it, the left staying
    let (x, y) = (moved.x + moved.w - 1, moved.y + 5);
    mouse(&mut ed, MouseKind::Down, x, y);
    mouse(&mut ed, MouseKind::Drag, x + 6, y);
    mouse(&mut ed, MouseKind::Up, x + 6, y);
    let wider = drawn(&ed, GroupKind::Tests);
    assert_eq!((wider.x, wider.w), (moved.x, moved.w + 6));
    // A press inside on a row of the code under it: the frame's, the code's cursor where it was
    ed.session.at = At::Code;
    let line = ed.cursor_line();
    mouse(&mut ed, MouseKind::Down, wider.x + wider.w - 4, wider.y + wider.h - 3);
    mouse(&mut ed, MouseKind::Up, wider.x + wider.w - 4, wider.y + wider.h - 3);
    assert_eq!(ed.cursor_line(), line, "nothing under it pressed");
    assert!(!ed.session.is_at(At::Code), "the keys to it");
}

/// Hops (§3.3): from the code `C-w l` never lands in a floating pane (they're over it, not beside it); from one, to its
/// frame's other member, then from its frame's centre to the nearest docked pane that way; `C-w w` goes round the
/// floating ones after the docked.
#[test]
fn hops_go_by_the_frames_centre_and_never_land_in_one_from_the_code() {
    let (_dir, mut ed) = tests_and_a_task();
    feed(&mut ed, "<C-w>e");
    let g = tests(&ed);
    feed(&mut ed, "<C-w>l");
    assert_eq!((ed.session.at, ed.session.hand), (At::Slot, Some(GroupKind::Tests)), "its own detail");
    feed(&mut ed, "<C-w>h<C-w>h");
    assert_eq!(ed.session.at, At::Code, "left of its centre: the code");
    feed(&mut ed, "<C-w>l");
    assert!(
        ed.session.at == At::Code || ed.session.at == At::Chat,
        "not into the float: {:?}",
        ed.session.at
    );
    ed.session.at = At::Code;
    let mut seen = Vec::new();
    for _ in 0..8 {
        feed(&mut ed, "<C-w>w");
        seen.push(ed.session.at);
    }
    assert!(seen.contains(&At::List(g.list)), "C-w w goes round it: {seen:?}");
}

/// Kept (§3.4, §5.3): the restart's file and `layouts.json` keep a floating group's place, frame and where it docks
/// back — its own pane made anew (the tree's panes only in the file); a run of its kind after it closed comes back
/// floating where it was.
#[test]
fn a_floating_group_is_kept_and_its_next_run_comes_back_floating() {
    let (_dir, mut ed) = tests_and_a_task();
    feed(&mut ed, "<C-w>e<C-w>m");
    feed(&mut ed, "hhk<ret>");
    let place = tests(&ed).place;
    let groups = crate::restart::groups_json(&ed.session);
    let panes = crate::restart::panes_json(&ed.session);
    assert!(!panes.iter().any(|p| p["shows"] == "group:tests"), "its pane isn't the tree's: {panes:?}");
    let kept = crate::restart::groups_of(&groups).expect("read back");
    let t = kept.iter().find(|g| g.0 == GroupKind::Tests).unwrap();
    assert_eq!((t.1, t.2, t.3), (ed.session.leaf(Shows::Slot).unwrap(), place, Place::Slot));
    // Closed (`q` on a run that ended), the place stays; the next run lands there
    feed(&mut ed, "q");
    assert!(ed.session.test_run.is_none() && ed.screen.floating.borrow().is_empty());
    assert_eq!(tests(&ed).place, place);
    let run = crate::testing::TestRun::finished("again", Vec::new(), vec!["ok".into()]);
    ed.session.test_run = Some(run);
    frame_in(&mut ed, W, H);
    assert!(ed.floating_in_sight(GroupKind::Tests), "floating again");
    assert_eq!(
        Frame::of_rect(drawn(&ed, GroupKind::Tests), W as usize, H as usize - 1),
        place.frame().unwrap()
    );
    // (as a restart brings it back: the slot's leaf its host, no pane of its own yet — made at the next event)
    let slot = ed.session.leaf(Shows::Slot).unwrap();
    ed.session.panes.retain(|p| p.shows != Shows::Group(GroupKind::Tests));
    ed.session.groups.iter_mut().filter(|g| g.kind == GroupKind::Tests).for_each(|g| g.host = slot);
    frame_in(&mut ed, W, H);
    let g = tests(&ed);
    assert_eq!(ed.session.shows(g.host), Some(Shows::Group(GroupKind::Tests)), "its own pane again");
    assert!(ed.floating_in_sight(GroupKind::Tests) && !ed.session.split.holds(g.host));
}

/// The state lists a floating group's own pane (`group:tests`, `place: floating`, its rect) and its members naming it.
#[test]
fn the_state_lists_a_floating_group() {
    let (_dir, mut ed) = tests_and_a_task();
    feed(&mut ed, "<C-w>e");
    let g = tests(&ed);
    let v = ed.ui_state().json();
    let panes = v["sessions"][0]["panes"].as_array().cloned().unwrap_or_default();
    let host = panes.iter().find(|p| p["id"] == format!("pane#{}", g.host)).expect("its pane listed");
    assert_eq!((host["shows"].as_str(), host["place"].as_str()), (Some("group:tests"), Some("floating")));
    let f = drawn(&ed, GroupKind::Tests);
    assert_eq!(host["rect"], serde_json::json!([f.x, f.y, f.w, f.h]));
    let list = panes.iter().find(|p| p["id"] == format!("pane#{}", g.list)).expect("its list");
    assert_eq!(
        (list["group"].as_str(), list["at"].as_bool()),
        (Some(&*format!("pane#{}", g.host)), Some(true))
    );
}

/// `Frame`: its rect is its share, rounded, at least 40 × 8 where there's room, never past an edge; its rect read back
/// is the same frame.
#[test]
fn a_frames_rect_and_back() {
    let f = Frame::CENTRED;
    assert_eq!(f.rect(160, 39), Rect { x: 32, y: 10, w: 96, h: 20 });
    assert_eq!(f.rect(80, 23), Rect { x: 16, y: 6, w: 48, h: 12 });
    assert_eq!(Frame::of_rect(f.rect(160, 39), 160, 39).rect(160, 39), f.rect(160, 39));
    let edge = Frame { x: 900, y: 900, w: 300, h: 300 };
    assert_eq!(edge.rect(100, 30), Rect { x: 60, y: 21, w: 40, h: 9 }, "its least width, moved back onto it");
    assert_eq!(edge.rect(20, 5), Rect { x: 0, y: 0, w: 20, h: 5 }, "the whole of a tiny screen");
    assert_eq!(Frame::of_json(&f.json()), Some(f));
    assert_eq!(f.json()[1], serde_json::Value::Null, "a placed top kept as placed");
    let set = Frame { y: 300, ..f };
    assert_eq!(Frame::of_json(&set.json()), Some(set), "a top you set kept as set");
}

/// The placed top (user, 2026-10-23): `min(H/4, (H - h)/2)` — at 1/4 while the pane is at most half the screen tall,
/// centred with equal margins once it's taller (a screen too short for its least height, a placed height past half),
/// the two meeting at h = H/2, odd heights too: nothing jumps. A top you set stays where you put it.
#[test]
fn a_floating_pane_past_half_the_screen_is_centred() {
    // (80 × 12 rows: 3/5 × 1/2 is 6 rows, its least 8 — past half: 2 rows above it, 2 below)
    assert_eq!(Frame::CENTRED.rect(80, 12), Rect { x: 16, y: 2, w: 48, h: 8 });
    // (a placed one 4/5 tall on 40 rows: 32, 4 above, 4 below — not at 1/4, which would leave none)
    let tall = Frame { h: 800, ..Frame::CENTRED };
    assert_eq!(tall.rect(160, 40), Rect { x: 32, y: 4, w: 96, h: 32 });
    // (at 1/4 up to half, then one row up for every two taller: never a jump)
    for sh in 8..60 {
        let top = |h: usize| Frame { h: (h * 1000 / sh + 1) as u16, ..Frame::CENTRED }.rect(160, sh);
        let quarter = (250 * sh + 500) / 1000;
        let mut was = None;
        for h in crate::pane::FLOAT_MIN_H..=sh {
            let r = top(h);
            if r.h != h {
                continue;
            }
            if 2 * h <= sh {
                assert_eq!(r.y, quarter, "{sh} rows, {h} tall: at 1/4");
            } else {
                let below = sh - r.y - r.h;
                assert!(r.y.abs_diff(below) <= 1 && r.y <= quarter, "{sh} rows, {h} tall: centred — {r:?}");
            }
            if let Some(was) = was {
                assert!(was >= r.y && was - r.y <= 1, "{sh} rows, {h} tall: {was} → {}", r.y);
            }
            was = Some(r.y);
        }
    }
    // A top you set: where you put it, whatever the height
    assert_eq!(Frame { y: 100, h: 800, ..Frame::CENTRED }.rect(160, 40).y, 4, "set: 1/10 of 40");
    assert_eq!(Frame { y: 100, h: 200, ..Frame::CENTRED }.rect(160, 40).y, 4);
}

/// What was said last.
fn said(ed: &Editor) -> String {
    ed.messages.last().map(|m| m.2.clone()).unwrap_or_default()
}
