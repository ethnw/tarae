//! A page's links followed while pages are read, simulated (web P3): `:web A` (a server of the test's own on this
//! machine), `gd` on a link in the page in front (its own server's answer a job, another page's read another), `C-o` and
//! `C-i` between the pages, the buffer in front closed, `C-c` on it, the user taking their time, a frame — the reads,
//! the answers and their landings in any order. After every step: one buffer at most an address; a read landing never
//! moves the front (only your keys and a link's answer do — B lands in its buffer out of sight when you went back to A);
//! `C-o`/`C-i` start no fetch (asserted as they're pressed); at the end no more requests than fetches started, and a
//! read whose buffer went dropped and counted.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::Editor;

const SEEDS: &[u64] = &[0, 1, 2, 3];

const INPUTS: usize = 30;

const LIMIT: Duration = crate::test_results::PATIENCE;

#[test]
fn a_link_followed_mid_read_lands_in_its_own_buffer() {
    super::seeds("a_link_followed_mid_read_lands_in_its_own_buffer", SEEDS, run);
}

/// What the check keeps between steps: inputs taken, and after the last step the front buffer and whether the page's
/// own server had a request out (its answer may move the front: a link followed).
#[derive(Default)]
struct Seen {
    inputs: Cell<u64>,
    checked: Cell<u64>,
    front: Cell<u64>,
    asking: Cell<bool>,
}

fn run(choices: Choices) -> Vec<String> {
    use crate::web::fetch::tests::{ok, path, server, way, yours_rules};
    let (at, heads) = server(false, |head| match path(head) {
        "/b" => ok("<title>B</title><main><h1>B</h1><p>Back to <a href=/a>a</a>.</p></main>"),
        _ => ok("<title>A</title><main><h1>A</h1><p>See <a href=/b>bee</a>.</p></main>"),
    });
    let base = way(at, yours_rules());
    let mut sim = Sim::new("a_link_followed_mid_read_lands_in_its_own_buffer", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    ed.config.llm.sandbox_ports.push(at.port());
    ed.session.web_way =
        Some(std::sync::Arc::new(move |stop| crate::web::fetch::Way { stop, ..base.clone() }));
    let a = format!("web http://localhost:{}/a", at.port());
    let seen = Rc::new(Seen::default());
    let mine = seen.clone();
    let input = move |sim: &mut Sim, ed: &mut Editor| {
        mine.inputs.set(mine.inputs.get() + 1);
        input(sim, ed, &a)
    };
    sim.walk(&mut ed, INPUTS, input, |ed| held(ed, &seen), print);
    let (fetches, requests) = (ed.session.web_fetches, heads.try_iter().count());
    sim.at_end(|| match requests <= fetches {
        true => Ok(()),
        false => Err(format!("{requests} requests for {fetches} fetches started")),
    });
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

fn print(ed: &Editor) -> String {
    let page = |d: &crate::document::Document| {
        let p = crate::web::page::of(d)?;
        let path = d.virtual_uri.as_deref()?.rsplit('/').next()?.to_string();
        let state = match (p.fetching.is_some(), p.read.is_some()) {
            (true, _) => "reading",
            (false, true) => "read",
            (false, false) => "none",
        };
        Some(format!("{path}:{state}"))
    };
    let pages: Vec<String> = ed.docs.iter().filter_map(page).collect();
    format!("front {:?} · pages {pages:?} · dropped {}", page(ed.doc()), ed.session.web_dropped)
}

fn input(sim: &mut Sim, ed: &mut Editor, a: &str) -> String {
    let c = &mut sim.choices;
    // (by arm: A asked 2, a link followed 4, C-o 3, C-i 2, a close 1, C-c 1, time 2, a frame 1, a page still being read
    // closed wherever it is 2 — `space b`'s `C-x`)
    match c.weighted(&[2, 4, 3, 2, 1, 1, 2, 1, 2]) {
        8 => {
            let reading =
                ed.docs.iter().find(|d| crate::web::page::of(d).is_some_and(|p| p.fetching.is_some()));
            let Some(id) = reading.map(|d| d.id) else { return "(no page being read)".into() };
            let front = ed.doc().id == id;
            ed.close_doc(id);
            format!("a page being read closed (in front: {front})")
        }
        0 => typed(ed, a).replace(a, "web http://localhost/a"),
        1 => {
            let link = crate::web::page::of(ed.doc()).and_then(|p| p.facts.links.first().map(|l| l.from));
            let Some(at) = link else { return "(no link in front)".into() };
            ed.doc_mut().set_selection(crate::selection::Selection::point(at));
            keys(ed, &[('g', false), ('d', false)]);
            "gd on its link".into()
        }
        2 | 3 => {
            let back = *c.pick(&[true, false]) || c.below(2) == 0;
            let (key, said) = if back { ('o', "C-o") } else { ('i', "C-i") };
            let before = ed.session.web_fetches;
            keys(ed, &[(key, true)]);
            assert_eq!(ed.session.web_fetches, before, "{said} fetched");
            said.into()
        }
        4 if ed.docs.len() > 1 => typed(ed, "buffer-close"),
        4 => "(one buffer: none closed)".into(),
        5 => {
            let reading = crate::web::page::takes(ed);
            keys(ed, &[('c', true)]);
            format!("C-c (a read under way: {reading})")
        }
        6 => {
            let ms = *c.pick(&[1, 10, 50]);
            sim.wait(Duration::from_millis(ms));
            format!("{ms} ms pass")
        }
        _ => {
            crate::term::render(ed, &mut Vec::new(), 100, 30).unwrap();
            "a frame".into()
        }
    }
}

fn keys(ed: &mut Editor, keys: &[(char, bool)]) {
    for &(c, ctrl) in keys {
        ed.handle_key(crate::key::Key { code: crate::key::Code::Char(c), ctrl, alt: false, shift: false });
    }
}

fn typed(ed: &mut Editor, line: &str) -> String {
    match crate::typed::execute(ed, line) {
        Ok(()) => format!(":{line}"),
        Err(e) => format!(":{line} — {e}"),
    }
}

/// One buffer at most an address; the front moved only by an input or a link's answer.
fn held(ed: &Editor, seen: &Seen) -> Result<(), String> {
    let mut addresses = std::collections::HashSet::new();
    let pages = ed.docs.iter().filter(|d| crate::web::page::of(d).is_some());
    if let Some(twice) = pages.filter_map(|d| d.virtual_uri.as_deref()).find(|u| !addresses.insert(*u)) {
        return Err(format!("two buffers of {twice:?}"));
    }
    let front = ed.doc().id;
    let landing = seen.inputs.get() == seen.checked.get();
    let moved = seen.front.get() != front;
    let web = ed.lsp.web.as_ref().map(|(c, _)| *c);
    let asking = ed.lsp.pending.keys().any(|(c, _)| Some(*c) == web);
    let was_asking = seen.asking.replace(asking);
    seen.checked.set(seen.inputs.get());
    seen.front.set(front);
    match landing && moved && !was_asking {
        true => Err("a read's landing moved the front buffer".into()),
        false => Ok(()),
    }
}
