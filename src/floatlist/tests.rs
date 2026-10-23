use super::*;

fn feed(ed: &mut Editor, keys: &str) {
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let k: Key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        };
        ed.handle_key(k);
        ed.focus_sync();
        crate::term::render(ed, &mut Vec::new(), 160, 40).unwrap();
    }
}

/// The session list and `:messages` up at once: each its own pane, selection and search; the keys in one, the other
/// stays in sight; `q` closes only the one with the keys; the one with the keys drawn over the other.
#[test]
fn two_floating_lists_keep_their_own() {
    let dir = crate::scratch::Scratch::new("floatlist");
    let mut ed = Editor::new(crate::config::Config::default());
    (ed.session.root, ed.session.name) = (dir.to_path_buf(), "main".into());
    ed.set_status("saved a.rs");
    feed(&mut ed, " W");
    feed(&mut ed, ":messages<ret>");
    assert_eq!(ed.float_list_in_hand(), Some(GroupKind::Messages));
    let (s, m) = (ed.sessions_pane.as_ref().unwrap().list, ed.messages_pane.as_ref().unwrap().list);
    assert_ne!(s, m, "a pane each");
    assert_eq!(
        ed.screen.floating.borrow().last().map(|f| f.0),
        Some(GroupKind::Messages),
        "with the keys: on top"
    );
    feed(&mut ed, "/saved<ret>");
    assert_eq!(ed.messages_pane.as_ref().unwrap().search.as_deref(), Some("saved"));
    assert_eq!(ed.sessions_pane.as_ref().unwrap().search, None, "the other's search untouched");
    feed(&mut ed, "q");
    assert!(ed.messages_pane.is_none() && ed.sessions_pane.is_some(), "q: the one with the keys");
    ed.float_list_focus(GroupKind::Sessions);
    assert_eq!(ed.float_list_in_hand(), Some(GroupKind::Sessions));
}

/// Done once used (user, 2026-10-23): `enter` on the session list closes it — on the front's row too; `esc` and the
/// moves don't.
#[test]
fn a_floating_list_is_done_once_used() {
    let dir = crate::scratch::Scratch::new("floatlist");
    let mut ed = Editor::new(crate::config::Config::default());
    (ed.session.root, ed.session.name) = (dir.to_path_buf(), "main".into());
    feed(&mut ed, " Wj<esc>");
    assert!(ed.sessions_pane.is_some(), "moves and esc: it stays");
    feed(&mut ed, " W<ret>");
    assert!(ed.sessions_pane.is_none(), "enter on the front's row: used");
}
