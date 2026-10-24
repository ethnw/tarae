use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::super::facts::{Facts, Known};
use super::super::page;
use super::super::{Prefer, read};
use crate::editor::Editor;

/// The server's answer as it gives it, ages said as everywhere.
fn answer(k: &Known, method: &str, params: &Value, now: std::time::Instant) -> Option<Value> {
    super::super::facts::answer(k, method, params, now, super::ago)
}

const URL: &str = "https://page.test/a";

/// A page with an anchor link, another page's, another host's, a `mailto:`, a heading linked to twice.
const HTML: &str = r##"<!doctype html><title>Doc</title><main>
<h1 id="top">Top</h1>
<p>See <a href="#usage">usage</a> and <a href="/b">bee</a> and <a href="https://other.test/c">sea</a> and
<a href="mailto:a@b.test">mail</a> and <a href="#nowhere">lost</a>.</p>
<h2 id="usage">Usage</h2>
<p>Back to <a href="#usage">here</a>, or <a href="/b">bee again</a>.</p>
<h3>Deep</h3>
<p>end</p>
</main>"##;

fn facts() -> (String, Facts) {
    let page = read(HTML.as_bytes(), URL, Some("text/html"), Prefer::Guess);
    let f = Facts::of(&page, &ropey::Rope::from_str(&page.text));
    (page.text, f)
}

/// The byte where link `words` (`[words](…)`) starts, a character in.
fn on(text: &str, words: &str) -> usize {
    text.find(&format!("[{words}]")).unwrap_or_else(|| panic!("[{words}] in {text}")) + 1
}

fn at(f: &Facts, byte: usize) -> Value {
    let (line, character) = f.position(byte);
    json!({ "textDocument": { "uri": URL }, "position": { "line": line, "character": character } })
}

fn known(f: &Arc<Facts>) -> Known {
    let mut k = Known { keep: super::super::fetch::KEEP, ..Known::default() };
    k.pages.insert(URL.into(), (Arc::downgrade(f), None));
    k
}

fn line_of(text: &str, starts: &str) -> usize {
    text.lines().position(|l| l.starts_with(starts)).unwrap_or_else(|| panic!("{starts:?} in {text}"))
}

/// Its answers are what the client's handlers read: a definition a location (`lsp::location_uris`) — another page's
/// address, or this page's line for its own anchor — and none for what isn't followed or isn't there; its
/// capabilities the four it answers, in UTF-8.
#[test]
fn a_definition_is_the_link_s_target() {
    let (text, f) = facts();
    let f = Arc::new(f);
    let k = known(&f);
    let now = crate::clock::now();
    let init = answer(&k, "initialize", &json!({}), now).unwrap();
    let caps = &init["capabilities"];
    assert_eq!(caps["positionEncoding"], "utf-8");
    for p in ["definitionProvider", "hoverProvider", "documentSymbolProvider", "referencesProvider"] {
        assert_eq!(caps[p], true, "{p}");
    }
    assert!(caps["completionProvider"].is_null() && caps["renameProvider"].is_null());

    let def = |words: &str| answer(&k, "textDocument/definition", &at(&f, on(&text, words)), now);
    let usage = line_of(&text, "## Usage");
    let places = |v: Option<Value>| -> Vec<(String, Value)> {
        let v = v.unwrap_or_default();
        crate::lsp::location_uris(&v).into_iter().map(|(u, p)| (u.to_string(), p)).collect()
    };
    assert_eq!(places(def("usage")), vec![(URL.into(), json!({ "line": usage, "character": 0 }))]);
    let b = ("https://page.test/b".to_string(), json!({ "line": 0, "character": 0 }));
    assert_eq!(places(def("bee")), vec![b]);
    assert_eq!(places(def("sea"))[0].0, "https://other.test/c");
    assert_eq!(def("mail"), None, "not followed");
    assert_eq!(def("lost"), None, "no such anchor");
    let words = answer(&k, "textDocument/definition", &at(&f, line_of(&text, "end")), now);
    assert_eq!(words, None, "not on a link");
    // (a page gone: nothing — its facts aren't kept past it)
    drop(f);
    assert_eq!(answer(&k, "textDocument/definition", &at(&facts().1, on(&text, "bee")), now), None);
}

/// `space k`: where the link goes, whole; another host said; a page open or kept said with its copy's age.
#[test]
fn a_hover_says_where_a_link_goes() {
    let (text, f) = facts();
    let f = Arc::new(f);
    let mut k = known(&f);
    let now = crate::clock::now();
    let hover = |k: &Known, words: &str| {
        let v = answer(k, "textDocument/hover", &at(&f, on(&text, words)), now).unwrap();
        crate::lsp::hover_text(&v)
    };
    assert_eq!(hover(&k, "bee"), "`https://page.test/b`");
    assert_eq!(hover(&k, "sea"), "`https://other.test/c`\n\nanother host");
    assert!(hover(&k, "mail").ends_with("not followed — tarae follows only http(s)"));
    assert!(hover(&k, "usage").ends_with("this page"));
    let other = Arc::new(Facts::default());
    k.pages
        .insert("https://page.test/b".into(), (Arc::downgrade(&other), Some(now - Duration::from_secs(120))));
    assert!(hover(&k, "bee").ends_with("open · read 2m ago"), "{}", hover(&k, "bee"));
    k.kept.insert("https://other.test/c".into(), now - Duration::from_secs(300));
    assert!(hover(&k, "sea").ends_with("another host\n\nkept · read 5m ago"), "{}", hover(&k, "sea"));
    let off = answer(&k, "textDocument/hover", &at(&f, line_of(&text, "end")), now);
    assert_eq!(off, None);
}

/// `space s`: the headings as a tree by level, each a section (`§`); `gr` on a heading the links to its anchor, on a
/// link every link to the same address.
#[test]
fn symbols_are_the_headings_and_references_the_links_to_one() {
    let (text, f) = facts();
    let f = Arc::new(f);
    let k = known(&f);
    let now = crate::clock::now();
    let doc = json!({ "textDocument": { "uri": URL } });
    let s = answer(&k, "textDocument/documentSymbol", &doc, now).unwrap();
    assert_eq!(s[0]["name"], "Top");
    assert_eq!(s[0]["kind"], 2);
    assert_eq!(s[0]["children"][0]["name"], "Usage");
    assert_eq!(s[0]["children"][0]["children"][0]["name"], "Deep");
    assert_eq!(s.as_array().unwrap().len(), 1);

    let refs = |byte: usize| {
        let v = answer(&k, "textDocument/references", &at(&f, byte), now).unwrap_or_default();
        let lines: Vec<u64> =
            crate::lsp::location_uris(&v).iter().map(|(_, p)| p["line"].as_u64().unwrap()).collect();
        lines
    };
    let (see, back) = (line_of(&text, "See ") as u64, line_of(&text, "Back to") as u64);
    let usage = text.find("## Usage").unwrap() + 3;
    assert_eq!(refs(usage), vec![see, back], "the links to #usage");
    assert_eq!(refs(on(&text, "bee")), vec![see, back], "the links to /b");
    assert_eq!(refs(text.find("end").unwrap()), Vec::<u64>::new());
}

/// A code block keeps breaks the rope breaks a line at and `\n` doesn't (a lone `\r`, `\x0B`, `\x0C`, U+0085,
/// U+2028): after it the server's lines are still the buffer's — `space s`'s heading, `gd`'s anchor, `gr`'s link.
#[test]
fn lines_are_the_buffers_past_a_break_only_the_rope_makes() {
    let html = "<main><pre>a&#13;b\x0Bc\x0Cd\u{85}e\u{2028}f</pre><h2 id=after>After</h2>\
                <p>Back <a href=#after>up</a>.</p></main>";
    let page = read(html.as_bytes(), URL, Some("text/html"), Prefer::Page);
    assert!(page.text.contains("a\rb\x0Bc\x0Cd\u{85}e\u{2028}f"), "{:?}", page.text);
    let rope = ropey::Rope::from_str(&page.text);
    let f = Arc::new(Facts::of(&page, &rope));
    let line = |starts: &str| (0..rope.len_lines()).find(|&l| rope.line(l).to_string().starts_with(starts));
    let (after, back) = (line("## After").unwrap(), line("Back").unwrap());
    assert!(after > line_of(&page.text, "## After"), "the rope breaks more");
    let k = known(&f);
    let now = crate::clock::now();
    let doc = json!({ "textDocument": { "uri": URL } });
    let s = answer(&k, "textDocument/documentSymbol", &doc, now).unwrap();
    assert_eq!(s[0]["range"]["start"]["line"], after);
    let up = on(&page.text, "up");
    let def = answer(&k, "textDocument/definition", &at(&f, up), now).unwrap_or_default();
    assert_eq!(crate::lsp::location_uris(&def)[0].1, json!({ "line": after, "character": 0 }));
    let v = answer(&k, "textDocument/references", &at(&f, page.text.find("## After").unwrap() + 3), now);
    let refs = v.unwrap_or_default();
    assert_eq!(crate::lsp::location_uris(&refs)[0].1["line"], back);
    // (the reader's line — Claude's `read`'s — as the buffer's)
    assert_eq!(f.buffer_line(line_of(&page.text, "## After")), after);
}

/// The server's answers on docs.rs's Rope page (753 links): a `gd` and a `space k` on every link, `space s`, a `gr` on
/// every heading — the slowest one each. Numbers for `cargo perf`.
#[test]
fn perf_web_serve_answers() {
    if crate::test_results::unoptimized("web::serve::tests::perf_web_serve_answers") {
        return;
    }
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/web/pages/docsrs.html");
    let url = "https://docs.rs/ropey/latest/ropey/struct.Rope.html";
    let page = read(&std::fs::read(path).unwrap(), url, None, Prefer::Guess);
    let f = Arc::new(Facts::of(&page, &ropey::Rope::from_str(&page.text)));
    assert!(f.links.len() > 700, "{}", f.links.len());
    let mut k = Known::default();
    k.pages.insert(url.into(), (Arc::downgrade(&f), None));
    let now = crate::clock::now();
    let ask = |method: &str, byte: usize| {
        let (line, character) = f.position(byte);
        let p =
            json!({ "textDocument": { "uri": url }, "position": { "line": line, "character": character } });
        let t = std::time::Instant::now();
        let _ = answer(&k, method, &p, now);
        t.elapsed().as_secs_f64() * 1000.0
    };
    let worst =
        |method: &str, at: &mut dyn Iterator<Item = usize>| at.map(|b| ask(method, b)).fold(0.0, f64::max);
    let heads: Vec<usize> = f.headings.iter().map(|h| f.line_starts[h.line]).collect();
    for (name, ms, budget) in [
        ("definition", worst("textDocument/definition", &mut f.links.iter().map(|l| l.from)), 1.0),
        ("hover", worst("textDocument/hover", &mut f.links.iter().map(|l| l.from)), 1.0),
        ("symbols", ask("textDocument/documentSymbol", 0), 5.0),
        ("references", worst("textDocument/references", &mut heads.into_iter()), 2.0),
    ] {
        let line = format!("PERF web_serve_{name}_ms {ms:.3} budget={budget} noise=0.2\n");
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        assert!(ms < budget, "{name}: {ms:.2} ms (budget {budget})");
    }
}

// ── In the editor ──

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
    let end = std::time::Instant::now() + Duration::from_secs(10);
    while !done(ed) {
        let left = end.saturating_duration_since(std::time::Instant::now());
        let ev = ed.events.recv_timeout(left).unwrap_or_else(|| panic!("{what} before the timeout"));
        ed.handle_event(ev);
    }
}

fn cursor_line(ed: &Editor) -> usize {
    let d = ed.doc();
    d.text.byte_to_line(d.selection().primary().cursor(&d.text))
}

fn put(ed: &mut Editor, byte: usize) {
    ed.doc_mut().set_selection(crate::selection::Selection::point(byte));
}

/// A saved page read with its address, its server ready.
fn page_editor() -> (Editor, crate::scratch::Scratch) {
    page_editor_of(HTML)
}

/// `page_editor` on a page of `html`'s.
fn page_editor_of(html: &str) -> (Editor, crate::scratch::Scratch) {
    let root = crate::scratch::Scratch::new("web-serve");
    std::fs::write(root.join("a.html"), html).unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = std::fs::canonicalize(root.path()).unwrap();
    feed(&mut ed, &format!(":web a.html {URL}<ret>"));
    let ready = |ed: &Editor| ed.lsp.web.as_ref().and_then(|(c, _)| ed.client(*c)).is_some_and(|c| c.ready);
    settle_until(&mut ed, "landed, its server ready", |ed| {
        page::of(ed.doc()).is_some_and(|p| p.read.is_some()) && ready(ed)
    });
    (ed, root)
}

/// `ctrl`+click is `gd` on a page as in code (R11): an `#anchor` link followed, `C-o` back to its words; a plain click
/// puts the cursor on the character under it — on the words of a link `gd` follows said once a session (where the
/// terminal doesn't send `ctrl` with a click, nothing tells the two apart); a `mailto:` link's words never spend it.
#[test]
fn a_click_is_the_cursor_and_ctrl_click_follows_a_link() {
    use crate::event::{Mouse, MouseKind};
    let (mut ed, _root) = page_editor();
    let text = ed.doc().text.to_string();
    let mut bytes = Vec::new();
    crate::term::render(&mut ed, &mut bytes, 80, 24).unwrap();
    let mut p = vt100::Parser::new(24, 80, 0);
    p.process(&bytes);
    let rows = p.screen().rows(0, 80).collect::<Vec<String>>();
    // (the cell of `s`'s first character, on the row that shows it)
    let cell = |s: &str| {
        let (y, row) = rows.iter().enumerate().find(|(_, r)| r.contains(s)).unwrap_or_else(|| panic!("{s}"));
        (row[..row.find(s).unwrap()].chars().count() as u16, y as u16)
    };
    let link = |words: &str| {
        let (x, y) = cell(&format!("[{words}]"));
        (x + 1, y)
    };
    let click = |ed: &mut Editor, (x, y): (u16, u16), ctrl: bool| {
        for kind in [MouseKind::Down, MouseKind::Up] {
            ed.handle_event(crate::event::Event::Mouse(Mouse { kind, x, y, alt: false, ctrl }));
        }
    };
    let said = "gd follows a link · ctrl+click too, where the terminal sends it";
    let says = |ed: &Editor| ed.toasts.iter().filter(|t| t.text == said).count();
    click(&mut ed, cell("See "), false);
    assert_eq!(says(&ed), 0, "not a link's");
    click(&mut ed, link("mail"), false);
    assert_eq!(ed.doc().selection().primary().head, on(&text, "mail"), "the cursor on its words");
    assert_eq!(says(&ed), 0, "a mailto: link's `gd` goes nowhere: not spent on it");
    let (x, y) = link("bee");
    click(&mut ed, (x + 1, y), false);
    assert_eq!(ed.doc().selection().primary().head, on(&text, "bee") + 1, "the `e` under it");
    assert_eq!(says(&ed), 1);
    click(&mut ed, link("usage"), false);
    assert_eq!(says(&ed), 1, "once");
    let usage = line_of(&text, "## Usage");
    let from = cursor_line(&ed);
    click(&mut ed, link("usage"), true);
    settle_until(&mut ed, "at #usage", |ed| cursor_line(ed) == usage);
    feed(&mut ed, "<C-o>");
    assert_eq!(ed.doc().selection().primary().head, on(&text, "usage"), "C-o back to the link's words");
    assert_eq!(cursor_line(&ed), from);
}

/// On a page the link comes first: ctrl+click on link words that are an address of their own
/// (`[https://a.example/](https://page.test/b)`) follows the link to its target — never the words' address — `C-o`
/// back to them.
#[test]
fn ctrl_click_on_words_that_are_an_address_follows_the_link() {
    use crate::event::{Mouse, MouseKind};
    let (mut ed, _root) = page_editor_of(
        "<!doctype html><title>Doc</title><main><p>Docs at <a href=\"/b\">https://a.example/</a>.</p></main>",
    );
    let a = ed.doc().id;
    let text = ed.doc().text.to_string();
    assert!(text.contains("[https://a.example/](https://page.test/b)"), "{text}");
    let b = crate::web::fetch::Fetched {
        asked: "https://page.test/b".into(),
        url: "https://page.test/b".into(),
        status: 200,
        content_type: Some("text/html".into()),
        bytes: b"<title>Bee</title><main><h1>Bee</h1><p>Its text.</p></main>".to_vec(),
        from: Vec::new(),
        hops: 0,
        upgraded: false,
        said: None,
        cut: false,
        took: Duration::from_millis(30),
        elsewhere: None,
    };
    let _ = ed.session.pages.put(Arc::new(b), crate::clock::now());
    let mut bytes = Vec::new();
    crate::term::render(&mut ed, &mut bytes, 80, 24).unwrap();
    let mut p = vt100::Parser::new(24, 80, 0);
    p.process(&bytes);
    let rows = p.screen().rows(0, 80).collect::<Vec<String>>();
    let (y, row) = rows.iter().enumerate().find(|(_, r)| r.contains("a.example")).expect("the link's row");
    let x = row[..row.find("a.example").unwrap()].chars().count() as u16;
    for kind in [MouseKind::Down, MouseKind::Up] {
        ed.handle_event(crate::event::Event::Mouse(Mouse { kind, x, y: y as u16, alt: false, ctrl: true }));
    }
    settle_until(&mut ed, "the link's target", |ed| {
        ed.doc().title.as_deref() == Some("web · page.test › Bee")
    });
    feed(&mut ed, "<C-o>");
    let d = ed.doc();
    assert_eq!(d.id, a, "C-o back to the page");
    assert_eq!(d.selection().primary().cursor(&d.text), text.find("a.example").unwrap(), "on the words");
}

/// The page's keys are the code's: `gd` on an `#anchor` a move in the page (`C-o` back), on another page's link that
/// page in a buffer of its own (the session's copy: nothing fetched) — `C-o` back and `C-i` forward across both; a
/// `mailto:` link's `gd` says why it goes nowhere; `space k` where a link goes; `space s` the headings; `gr` on a
/// heading the links to it. Its server is the page's only one, never asked by a code file, and never in `:procs`.
#[test]
fn a_page_s_language_keys_follow_its_links() {
    let (mut ed, _root) = page_editor();
    let a = ed.doc().id;
    let text = ed.doc().text.to_string();
    let (cid, _) = ed.lsp.web.clone().unwrap();
    assert_eq!(ed.doc().lsp.client, Some(cid));
    assert!(ed.client(cid).is_some_and(|c| c.own), "tarae's own");

    // `#usage`: a move within
    put(&mut ed, on(&text, "usage"));
    let from = cursor_line(&ed);
    let usage = line_of(&text, "## Usage");
    feed(&mut ed, "gd");
    settle_until(&mut ed, "at #usage", |ed| cursor_line(ed) == usage);
    feed(&mut ed, "<C-o>");
    assert_eq!((ed.doc().id, cursor_line(&ed)), (a, from), "C-o back");

    // `mailto:`: said
    put(&mut ed, on(&text, "mail"));
    feed(&mut ed, "gd");
    settle_until(&mut ed, "said", |ed| {
        ed.toasts.iter().any(|t| t.text == "a mailto: link — tarae follows only http(s)")
    });

    // another page: its buffer, from the session's copy
    let b = crate::web::fetch::Fetched {
        asked: "https://page.test/b".into(),
        url: "https://page.test/b".into(),
        status: 200,
        content_type: Some("text/html".into()),
        bytes: b"<title>Bee</title><main><h1>Bee</h1><p>Its text.</p></main>".to_vec(),
        from: Vec::new(),
        hops: 0,
        upgraded: false,
        said: None,
        cut: false,
        took: Duration::from_millis(30),
        elsewhere: None,
    };
    let _ = ed.session.pages.put(Arc::new(b), crate::clock::now());
    put(&mut ed, on(&text, "bee"));
    feed(&mut ed, "gd");
    settle_until(&mut ed, "the other page", |ed| ed.doc().title.as_deref() == Some("web · page.test › Bee"));
    let bee = ed.doc().id;
    feed(&mut ed, "<C-o>");
    assert_eq!(ed.doc().id, a, "C-o back to the page it came from");
    feed(&mut ed, "<C-i>");
    assert_eq!(ed.doc().id, bee, "C-i forward");
    feed(&mut ed, "<C-o>");

    // `space k`
    put(&mut ed, on(&text, "sea"));
    feed(&mut ed, "<space>k");
    settle_until(&mut ed, "its hover", |ed| ed.popup.is_some());

    // `space s`
    feed(&mut ed, "<esc><space>s");
    settle_until(&mut ed, "its symbols", |ed| ed.picker.is_some());
    let labels: Vec<String> = ed.picker.as_ref().unwrap().shown().map(|i| i.label.clone()).collect();
    assert_eq!(labels, ["Top", "Usage", "Deep"]);
    feed(&mut ed, "<esc>");

    // `gr` on the heading
    put(&mut ed, text.find("## Usage").unwrap() + 3);
    feed(&mut ed, "gr");
    settle_until(&mut ed, "its references", |ed| ed.picker.as_ref().is_some_and(|p| p.shown().count() == 2));
    feed(&mut ed, "<esc>");

    // tarae's own only for pages; never a process
    let code = ed.docs.iter().filter(|d| d.page.is_none()).all(|d| d.lsp.clients().all(|c| c != cid));
    assert!(code, "no code file has it");
    assert!(ed.procs_known().iter().all(|k| k.title != super::NAME), "not in :procs");
}

/// A late answer is dropped as a server's is (the page read anew since: its version moved); `[lsp.web] except` is
/// honoured; out of sight it's never stopped as a program-less server.
#[test]
fn its_answers_take_a_server_s_path() {
    let (mut ed, _root) = page_editor();
    let text = ed.doc().text.to_string();
    let (cid, _) = ed.lsp.web.clone().unwrap();

    // late: the cursor moved before the answer came — nothing happens
    put(&mut ed, on(&text, "usage"));
    feed(&mut ed, "gd");
    put(&mut ed, 0);
    let pending = |ed: &Editor| ed.lsp.pending.keys().any(|(c, _)| *c == cid);
    settle_until(&mut ed, "its answer", |ed| !pending(ed));
    assert_eq!(cursor_line(&ed), 0, "the late answer moved nothing");

    // `[lsp.web] except`
    let spec = crate::lsp::ServerSpec {
        name: "web".into(),
        command: String::new(),
        args: Vec::new(),
        except: vec!["hover".into()],
    };
    ed.config.lsp.servers.insert("web".into(), spec);
    assert_eq!(ed.server_for(&crate::lsp_editor::Kind::Hover), None);
    assert!(ed.server_for(&crate::lsp_editor::Kind::DocumentSymbols).is_some());

    // out of sight past `sight_after`: kept
    ed.lsp.sight_after = Some(Duration::ZERO);
    ed.new_scratch();
    ed.servers_follow_sight(crate::clock::now());
    std::thread::sleep(Duration::from_millis(20));
    while let Some(ev) = ed.events.recv_timeout(Duration::from_millis(50)) {
        ed.handle_event(ev);
    }
    assert!(ed.client(cid).is_some(), "not stopped");
}
