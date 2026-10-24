use std::time::Duration;

use ropey::Rope;

use super::{around, targets};
use crate::editor::Editor;
use crate::selection::{Range, Selection};

fn at(text: &str, mark: &str) -> Option<String> {
    around(&Rope::from_str(text), text.find(mark).unwrap())
}

/// The path or address around the cursor: punctuation at its end left out, a bracket or quote around it not taken,
/// one beside the cursor taken; none where the cursor is on a space between words.
#[test]
fn the_path_around_the_cursor() {
    assert_eq!(at("see src/a.rs.", "a.rs").as_deref(), Some("src/a.rs"));
    assert_eq!(at("at src/a.rs:12:5: error", "rs:").as_deref(), Some("src/a.rs:12:5"));
    assert_eq!(at("[bee](https://page.test/b)", "page").as_deref(), Some("https://page.test/b"));
    assert_eq!(at("open \"~/notes.md\" now", "\"~").as_deref(), Some("~/notes.md"));
    assert_eq!(at("a  b", "  "), None);
    assert_eq!(at("été/한글.md ", "한").as_deref(), Some("été/한글.md"));
    // (a long line: looked for around the cursor only)
    let long = format!("{} x/y.rs {}", "a".repeat(5000), "b".repeat(5000));
    assert_eq!(at(&long, "x/").as_deref(), Some("x/y.rs"));
}

/// Several selections: each one's text, trimmed; a wider one its own text, not the path around it.
#[test]
fn every_selection_names_one() {
    let text = Rope::from_str("a.rs b.rs  c.rs");
    let sel = Selection::new(vec![Range::new(0, 4), Range::new(5, 10)], 0);
    assert_eq!(targets(&text, &sel), ["a.rs", "b.rs"]);
    let one = Selection::new(vec![Range::new(0, 1)], 0);
    assert_eq!(targets(&text, &one), ["a.rs"]);
    let wide = Selection::new(vec![Range::new(0, 2)], 0);
    assert_eq!(targets(&text, &wide), ["a."]);
}

fn feed(ed: &mut Editor, keys: &str) {
    use crate::key::{Code, Key};
    let mut chars = keys.chars();
    while let Some(c) = chars.next() {
        let key = match c {
            '<' => chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap(),
            c => Key::plain(Code::Char(c)),
        };
        ed.handle_key(key);
    }
}

fn settle_until(ed: &mut Editor, what: &str, done: impl Fn(&Editor) -> bool) {
    let end = std::time::Instant::now() + crate::test_results::PATIENCE;
    while !done(ed) {
        let left = end.saturating_duration_since(std::time::Instant::now());
        let ev = ed.events.recv_timeout(left).unwrap_or_else(|| panic!("{what} before the timeout"));
        ed.handle_event(ev);
    }
}

fn put(ed: &mut Editor, text: &str, mark: &str) {
    let at = text.find(mark).unwrap();
    ed.doc_mut().set_selection(Selection::point(at));
}

fn said(ed: &Editor, words: &str) -> bool {
    ed.toasts.iter().any(|t| t.text == words)
}

/// `gf` in code: a path from the file's own folder, else the project's; `file:line:col` at its line; an http(s)
/// address as a page (the session's copy: nothing fetched); a `mailto:` and a missing file said; `C-o` back.
#[test]
fn gf_opens_the_path_or_page_named_here() {
    let root = crate::scratch::Scratch::new("gotofile");
    std::fs::create_dir_all(root.join("src")).unwrap();
    let text = "see b.rs and lib.rs:2:3, https://page.test/b, mailto:a@b.test, gone.rs\n";
    std::fs::write(root.join("src/a.rs"), text).unwrap();
    std::fs::write(root.join("src/b.rs"), "bee\n").unwrap();
    std::fs::write(root.join("lib.rs"), "one\ntwo three\n").unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = std::fs::canonicalize(root.path()).unwrap();
    let a = ed.session.root.join("src/a.rs");
    ed.open(&a).unwrap();
    let name = |ed: &Editor| {
        ed.doc().path.as_deref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned())
    };

    put(&mut ed, text, "b.rs");
    feed(&mut ed, "gf");
    settle_until(&mut ed, "src/b.rs", |ed| name(ed).as_deref() == Some("b.rs"));
    feed(&mut ed, "<C-o>");
    assert_eq!(name(&ed).as_deref(), Some("a.rs"), "C-o back");

    put(&mut ed, text, "lib.rs");
    feed(&mut ed, "gf");
    settle_until(&mut ed, "lib.rs at 2:3", |ed| name(ed).as_deref() == Some("lib.rs"));
    let d = ed.doc();
    assert_eq!(d.selection().primary().cursor(&d.text), "one\ntw".len(), "line 2, column 3");
    feed(&mut ed, "<C-o>");

    bee_kept(&mut ed);
    put(&mut ed, text, "page.test");
    feed(&mut ed, "gf");
    settle_until(&mut ed, "the page", |ed| ed.doc().title.as_deref() == Some("web · page.test › Bee"));
    feed(&mut ed, "<C-o>");
    assert_eq!(name(&ed).as_deref(), Some("a.rs"), "C-o back from the page");

    put(&mut ed, text, "mailto");
    feed(&mut ed, "gf");
    settle_until(&mut ed, "said", |ed| said(ed, "gf: mailto: — tarae follows only http(s)"));
    put(&mut ed, text, "gone");
    feed(&mut ed, "gf");
    settle_until(&mut ed, "said", |ed| said(ed, "gf: no file gone.rs"));
    assert_eq!(name(&ed).as_deref(), Some("a.rs"));
}

/// `gf` on a folder: its files in the file picker's tree, rooted there; on several selections each one opened, the last
/// in front, the others in the buffers, `C-o` back through them in turn, and what opened nothing said beside them.
#[test]
fn gf_opens_a_folders_files_and_each_of_several_selections() {
    let root = crate::scratch::Scratch::new("gotofile-folder");
    std::fs::create_dir_all(root.join("src/web")).unwrap();
    let text = "see src/web/ and b.rs, c.rs, gone.rs\n";
    std::fs::write(root.join("a.rs"), text).unwrap();
    std::fs::write(root.join("src/web/page.rs"), "page\n").unwrap();
    std::fs::write(root.join("b.rs"), "bee\n").unwrap();
    std::fs::write(root.join("c.rs"), "sea\n").unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = std::fs::canonicalize(root.path()).unwrap();
    ed.open(&ed.session.root.join("a.rs")).unwrap();
    let name = |ed: &Editor| {
        ed.doc().path.as_deref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned())
    };

    put(&mut ed, text, "web/");
    feed(&mut ed, "gf");
    let listed = |ed: &Editor| ed.picker.as_ref().is_some_and(|p| !p.items().is_empty());
    settle_until(&mut ed, "the folder's files", listed);
    let p = ed.picker.as_ref().unwrap();
    assert_eq!(p.title, "files in src/web");
    let labels: Vec<String> = p.items().iter().map(|i| i.label.clone()).collect();
    assert!(
        labels.iter().any(|l| l.ends_with("page.rs")) && !labels.iter().any(|l| l.contains("b.rs")),
        "{labels:?}"
    );
    feed(&mut ed, "<esc>");
    assert_eq!(name(&ed).as_deref(), Some("a.rs"));

    let span = |w: &str| {
        let at = text.find(w).unwrap();
        Range::new(at, at + w.len())
    };
    ed.doc_mut().set_selection(Selection::new(vec![span("b.rs"), span("c.rs"), span("gone.rs")], 0));
    feed(&mut ed, "gf");
    settle_until(&mut ed, "each opened", |ed| said(ed, "gf: no file gone.rs"));
    assert_eq!(name(&ed).as_deref(), Some("c.rs"), "the last in front");
    let open: Vec<String> = ed
        .buffers()
        .into_iter()
        .filter_map(|i| ed.docs[i].path.clone())
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(["a.rs", "b.rs", "c.rs"].iter().all(|n| open.iter().any(|o| o == n)), "{open:?}");
    feed(&mut ed, "<C-o>");
    assert_eq!(name(&ed).as_deref(), Some("b.rs"), "C-o: the one before");
    feed(&mut ed, "<C-o>");
    assert_eq!(name(&ed).as_deref(), Some("a.rs"), "then where gf was");
}

/// On a page, `gf` on a link's address opens it (`gd` on its words is the link's definition).
#[test]
fn gf_on_a_page_opens_the_address_written_there() {
    let mut ed = Editor::new(crate::config::Config::default());
    let text = "See [bee](https://page.test/b).\n";
    crate::web::page::stand_in(&mut ed, "https://page.test/a", text);
    bee_kept(&mut ed);
    put(&mut ed, text, "page.test");
    feed(&mut ed, "gf");
    settle_until(&mut ed, "the page", |ed| ed.doc().title.as_deref() == Some("web · page.test › Bee"));
}

/// ctrl+click on an http(s) address written in code (a comment here) opens it as a page — `gf`'s act, inside tarae —
/// `C-o` back to the click; elsewhere in code it's `gd` (`lsp_editor`'s `ctrl_click_goes_to_the_definition`).
#[test]
fn ctrl_click_on_an_address_in_code_opens_the_page() {
    use crate::event::{Mouse, MouseKind};
    let root = crate::scratch::Scratch::new("gotofile-click");
    let text = "fn main() {}\n// see https://page.test/b for it\n";
    std::fs::write(root.join("a.rs"), text).unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = std::fs::canonicalize(root.path()).unwrap();
    ed.open(&ed.session.root.join("a.rs")).unwrap();
    bee_kept(&mut ed);
    let mut bytes = Vec::new();
    crate::term::render(&mut ed, &mut bytes, 80, 24).unwrap();
    let mut p = vt100::Parser::new(24, 80, 0);
    p.process(&bytes);
    let rows = p.screen().rows(0, 80).collect::<Vec<String>>();
    let (y, row) = rows.iter().enumerate().find(|(_, r)| r.contains("page.test")).expect("the comment's row");
    let x = row[..row.find("page.test").unwrap()].chars().count() as u16;
    for kind in [MouseKind::Down, MouseKind::Up] {
        ed.handle_event(crate::event::Event::Mouse(Mouse { kind, x, y: y as u16, alt: false, ctrl: true }));
    }
    settle_until(&mut ed, "the page", |ed| ed.doc().title.as_deref() == Some("web · page.test › Bee"));
    feed(&mut ed, "<C-o>");
    let d = ed.doc();
    assert_eq!(d.path.as_deref().and_then(|p| p.file_name()).and_then(|n| n.to_str()), Some("a.rs"));
    assert_eq!(
        d.selection().primary().cursor(&d.text),
        text.find("page.test").unwrap(),
        "C-o back to the click"
    );
}

/// `https://page.test/b` in the session's copies: opened with nothing fetched.
fn bee_kept(ed: &mut Editor) {
    let b = crate::web::fetch::Fetched {
        asked: "https://page.test/b".into(),
        url: "https://page.test/b".into(),
        status: 200,
        content_type: Some("text/html".into()),
        bytes: b"<title>Bee</title><main><h1>Bee</h1></main>".to_vec(),
        from: Vec::new(),
        hops: 0,
        upgraded: false,
        said: None,
        cut: false,
        took: Duration::from_millis(30),
        elsewhere: None,
    };
    let _ = ed.session.pages.put(std::sync::Arc::new(b), crate::clock::now());
}
