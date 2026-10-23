use super::*;
use crate::key::{Code, Key};
use crate::session::At;

fn frame(ed: &mut Editor) -> String {
    ed.focus_sync();
    crate::term::render(ed, &mut Vec::new(), 160, 40).unwrap();
    crate::screen::text(ed.last_frame.as_ref().unwrap().0.as_slice(), 160, 40).unwrap()
}

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

fn rect(ed: &Editor) -> crate::split::Rect {
    ed.screen.floating.borrow().iter().find(|f| f.0 == GroupKind::Messages).expect("drawn").1
}

fn here(ed: &Editor) -> Option<String> {
    let id = ed.float_list_here(GroupKind::Messages);
    rows(ed).into_iter().find(|r| Some(r.id) == id).map(|r| r.text)
}

/// `:messages` floats as a list in the session list's frame — 3/5 wide, as tall as its rows, its top at 1/4 — with
/// the keys (your key opened it), oldest first, at the newest; a row a message (`▲` before an error or a warning, its
/// lines as one, its age); `/` its own search; `esc code` keeps it in sight, `q` closes it; its rows in the state.
#[test]
fn messages_is_a_floating_list_at_the_newest() {
    let mut ed = Editor::new(crate::config::Config::default());
    feed(&mut ed, ":messages<ret>");
    assert!(ed.messages_pane.is_none(), "nothing said yet: said instead");
    ed.set_status("saved a.rs");
    ed.set_warning("rust-analyzer: 2 errors\nsee the log");
    ed.set_status("saved b.rs");
    assert!(!ed.toasts.is_empty(), "said as toasts");
    feed(&mut ed, ":messages<ret>");
    assert!(ed.float_list_keys(GroupKind::Messages), "focused at once");
    assert!(ed.toasts.is_empty(), "opened, the toasts go: the same words never twice");
    let s = frame(&mut ed);
    assert!(ed.screen.toast_card.get().is_none());
    let r = rect(&ed);
    // (the "no messages yet" said first is one of them: four rows)
    assert_eq!(
        r,
        crate::split::Rect { x: 32, y: 10, w: 96, h: 6 },
        "its top at 1/4, 3/5 wide, as tall as its rows"
    );
    let lines: Vec<&str> = s.lines().skip(r.y).take(r.h).collect();
    assert!(
        lines[0].contains("╭─ messages ── 4 messages · 1 warning") && lines[0].contains("y copy"),
        "{}",
        lines[0]
    );
    assert!(lines[1].contains("no messages yet"), "oldest first: {}", lines[1]);
    assert!(lines[3].contains("▲ rust-analyzer: 2 errors · see the log"), "{}", lines[3]);
    assert!(
        lines[4].contains("▎   saved b.rs") && lines[4].contains("just now"),
        "at the newest: {}",
        lines[4]
    );
    assert_eq!(here(&ed).as_deref(), Some("saved b.rs"));
    let listed = ed.ui_state().panels.lists;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].0, "messages");
    assert!(listed[0].1.iter().any(|r| r.selected && r.id.starts_with("messages:toast#")), "{listed:?}");
    // Its own search, never the code's
    feed(&mut ed, "/saved a<ret>");
    assert_eq!(here(&ed).as_deref(), Some("saved a.rs"));
    assert_eq!(ed.search, None);
    feed(&mut ed, "k");
    assert_eq!(here(&ed).as_deref(), Some("no messages yet"));
    feed(&mut ed, "<esc>");
    assert!(ed.messages_pane.is_some() && ed.session.at == At::Code, "esc code: it stays");
    feed(&mut ed, ":messages<ret>");
    assert_eq!(here(&ed).as_deref(), Some("no messages yet"), "up already: where it was");
    feed(&mut ed, "q");
    assert!(ed.messages_pane.is_none(), "q closes it");
}

/// While `:messages` is up, what's said is a row of it, no toast; closed, a toast again. From a focused toast's `enter`
/// too, the card goes.
#[test]
fn while_the_list_is_up_a_message_is_its_row() {
    let mut ed = Editor::new(crate::config::Config::default());
    ed.set_status("saved a.rs");
    feed(&mut ed, ":messages<ret><esc>");
    assert!(ed.messages_pane.is_some(), "up, without the keys");
    ed.set_status("saved b.rs");
    frame(&mut ed);
    assert!(ed.toasts.is_empty() && ed.screen.toast_card.get().is_none(), "its row in sight: no toast");
    assert_eq!(rows(&ed).last().map(|r| r.text.as_str()), Some("saved b.rs"), "a row of it");
    feed(&mut ed, ":messages<ret>q");
    assert!(ed.messages_pane.is_none());
    ed.set_status("saved c.rs");
    assert_eq!(ed.toasts.len(), 1, "closed: a toast again");
    frame(&mut ed);
    feed(&mut ed, "<C-w>w<ret>");
    assert!(
        ed.float_list_keys(GroupKind::Messages) && ed.toasts.is_empty(),
        "the toast's enter: its home, the card gone"
    );
}

/// While the list is up a message is only its row where the frame shows that row: another pane zoomed (`C-w z` — the
/// list out of sight) or the selection holding the list off its newest row, it comes as a toast as usual, the selection
/// where it was — an error never only on the status line.
#[test]
fn a_message_whose_row_is_out_of_sight_comes_as_a_toast() {
    let mut ed = Editor::new(crate::config::Config::default());
    ed.set_status("saved a.rs");
    feed(&mut ed, ":messages<ret><esc><C-w>v<C-w>z");
    assert!(
        ed.messages_pane.is_some() && ed.screen.list(GroupKind::Messages).is_none(),
        "zoomed: up, not drawn"
    );
    ed.set_error("it broke");
    frame(&mut ed);
    assert!(ed.screen.toast_card.get().is_some(), "its row out of sight: a toast");
    assert_eq!(ed.toasts.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(), vec!["it broke"]);
    // The selection holding the list off its newest row
    feed(&mut ed, "<C-w>z:messages<ret>q");
    for i in 0..40 {
        ed.set_status(format!("saved {i}"));
    }
    feed(&mut ed, ":messages<ret>g");
    let first = here(&ed);
    assert_eq!(first.as_deref(), Some("saved a.rs"), "g: the first row");
    let (_, inner, top) = ed.screen.list(GroupKind::Messages).expect("drawn");
    assert!(top + inner < rows(&ed).len(), "its newest row out of sight");
    ed.set_warning("it broke again");
    frame(&mut ed);
    assert!(ed.toasts.iter().any(|t| t.text == "it broke again"), "a toast");
    assert!(ed.screen.toast_card.get().is_some(), "drawn as a card, never only on the status line");
    assert_eq!(here(&ed), first, "the selection where it was");
    assert_eq!(ed.screen.list(GroupKind::Messages).map(|l| l.2), Some(top), "the list not scrolled to it");
}

/// `n` messages said, then `:messages` up without the keys: the screen row the next message's row takes (the list as
/// tall as its rows, one more).
fn list_of(ed: &mut Editor, n: usize) -> usize {
    for i in 0..n {
        ed.set_status(format!("saved {i}"));
    }
    feed(ed, ":messages<ret><esc>");
    let r = rect(ed);
    r.y + r.h - 1
}

/// The next message's row `y` under `over` (x, y, w, h) — the cover is where the test means it to be.
fn covers(ed: &Editor, over: (usize, usize, usize, usize), y: usize) -> bool {
    let r = rect(ed);
    let (x, top, w, h) = over;
    (top..top + h).contains(&y) && x < r.x + r.w - 1 && r.x + 1 < x + w
}

fn toast_of(ed: &Editor, text: &str) -> bool {
    ed.screen.toast_card.get().is_some() && ed.toasts.iter().any(|t| t.text == text)
}

// What's drawn after the list over the next message's row hides it, and the message comes as a toast — a floating
// group with the keys (`:procs` floated), a card waiting, which-key: the row counts only where the finished frame shows
// its cells as it drew them (`term::shown_whole`, every later layer drawn). Without the cover, the row alone.

#[test]
fn a_message_under_a_floating_group_comes_as_a_toast() {
    let mut ed = Editor::new(crate::config::Config::default());
    let y = list_of(&mut ed, 3);
    feed(&mut ed, ":procs<ret><C-w>e");
    let over = ed.screen.floating.borrow().iter().find(|f| f.0 == GroupKind::Procs).map(|f| f.1);
    let over = over.expect(":procs floating");
    assert!(covers(&ed, (over.x, over.y, over.w, over.h), y), "{over:?} over row {y} of {:?}", rect(&ed));
    ed.set_error("it broke");
    frame(&mut ed);
    assert!(toast_of(&ed, "it broke"), "under :procs: a toast");
}

#[test]
fn a_message_under_a_waiting_card_comes_as_a_toast() {
    let mut ed = Editor::new(crate::config::Config::default());
    ed.docs[0].text = ropey::Rope::from_str(&"line\n".repeat(60));
    // (the cursor off the top rows: the toast takes the top right, the card the bottom)
    feed(&mut ed, "jjjjjjjj");
    let y = list_of(&mut ed, 16);
    let q = (1..=12).map(|i| format!("file {i} not viewed")).collect::<Vec<_>>().join("\n");
    let what = crate::offer::What::ReviewEnd { session: 0, started: std::time::Instant::now() };
    ed.push_offer(crate::offer::Offer::new("End the review?".into(), &q, "", String::new(), what));
    frame(&mut ed);
    let card = ed.screen.offer_card.expect("the card drawn");
    assert!(!ed.card_has_keys(), "waiting");
    assert!(covers(&ed, card, y), "{card:?} over row {y} of {:?}", rect(&ed));
    ed.set_error("it broke");
    frame(&mut ed);
    assert!(
        toast_of(&ed, "it broke"),
        "under the card: a toast — {:?} {:?}",
        ed.toasts.iter().map(|t| (&t.text, t.row)).collect::<Vec<_>>(),
        ed.screen.toast_card.get()
    );
}

#[test]
fn a_message_under_which_key_comes_as_a_toast() {
    let mut ed = Editor::new(crate::config::Config::default());
    list_of(&mut ed, 16);
    feed(&mut ed, " ");
    ed.set_error("it broke");
    frame(&mut ed);
    assert!(toast_of(&ed, "it broke"), "under which-key: a toast");
    // Which-key gone, the same list: the row alone (it had room — the cover was which-key)
    let mut ed = Editor::new(crate::config::Config::default());
    list_of(&mut ed, 16);
    feed(&mut ed, " <esc>");
    ed.set_error("it broke");
    frame(&mut ed);
    assert!(!ed.toasts.iter().any(|t| t.text == "it broke"), "in sight: only its row");
}

/// `enter` on one its row can't show whole opens it in a buffer — the list used, it closes (user, 2026-10-23); on
/// one shown whole it says so and stays. `y` copies the row's message.
#[test]
fn enter_opens_a_long_one_whole_and_closes_the_list() {
    let mut ed = Editor::new(crate::config::Config::default());
    ed.set_status("short");
    ed.set_warning("first line\nsecond line");
    feed(&mut ed, ":messages<ret>k<ret>");
    assert!(ed.messages_pane.is_some(), "shown whole: it stays");
    assert!(ed.messages.last().unwrap().2.starts_with("shown whole"));
    // (the newest is that message now: back to the warning)
    let warning = rows(&ed).into_iter().find(|r| r.text.starts_with("first line")).unwrap().id;
    ed.messages_pane.as_mut().unwrap().selected = warning;
    feed(&mut ed, "<ret>");
    assert!(ed.messages_pane.is_none(), "used: closed");
    assert_eq!(ed.doc().title.as_deref(), Some(format!("message #{warning}").as_str()));
    assert_eq!(ed.doc().text.to_string(), "first line\nsecond line");
}
