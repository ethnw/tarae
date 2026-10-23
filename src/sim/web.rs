//! `:web` reading saved pages on a worker, simulated: a page asked (again: read anew, another part), a file that
//! isn't a page asked, the session's copy of a fetched page asked (read on a worker, no network), a page fetched from
//! a server of the test's own on this machine (its way a stand-in, its proxy made before the simulation: a proxy's
//! threads outlive a step), an address the fetch refuses, `:web-stop`, the buffer in front closed, the user taking
//! their time, a frame — the reads' and fetches' jobs landing among them in any order. After every step: one buffer at
//! most per page's title, none for what isn't a page, no fetch's step left once none is under way.

use std::time::Duration;

use super::{Choices, Sim};
use crate::editor::Editor;

const SEEDS: &[u64] = &[0, 1, 2];

const INPUTS: usize = 30;

const LIMIT: Duration = Duration::from_secs(60);

#[test]
fn a_page_read_twice_is_one_buffer() {
    super::seeds("a_page_read_twice_is_one_buffer", SEEDS, run);
}

fn run(choices: Choices) -> Vec<String> {
    let dir = crate::scratch::Scratch::new("sim-web");
    std::fs::write(
        dir.join("a.html"),
        "<html><head><title>A page</title></head><body><main><h1>A</h1><p>Its text.</p></main></body></html>",
    )
    .unwrap();
    std::fs::write(dir.join("b.pdf"), "%PDF-1.7\n").unwrap();
    use crate::web::fetch::tests::{ok, server, way, yours_rules};
    let (at, _) =
        server(false, |_| ok("<title>Fetched</title><main><h1>Fetched</h1><p>Its text.</p></main>"));
    let base = way(at, yours_rules());
    let mut sim = Sim::new("a_page_read_twice_is_one_buffer", choices, LIMIT);
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = dir.path().to_path_buf();
    ed.session.web_way =
        Some(std::sync::Arc::new(move |stop| crate::web::fetch::Way { stop, ..base.clone() }));
    let fetched = format!("web http://localhost:{}/", at.port());
    let kept = crate::web::fetch::Fetched {
        asked: "https://kept.test/".into(),
        url: "https://kept.test/".into(),
        status: 200,
        content_type: Some("text/html".into()),
        bytes: b"<title>Kept</title><main><h1>Kept</h1><p>Its text.</p></main>".to_vec(),
        from: Vec::new(),
        hops: 0,
        upgraded: false,
        said: None,
        cut: false,
        took: Duration::from_millis(30),
    };
    let _ = ed.session.pages.put(std::sync::Arc::new(kept), crate::clock::now());
    let input = move |sim: &mut Sim, ed: &mut Editor| input(sim, ed, &fetched);
    sim.walk(&mut ed, INPUTS, input, held, |ed| {
        let titles: Vec<&str> = ed.docs.iter().filter_map(|d| d.title.as_deref()).collect();
        format!("front {:?} · titled {titles:?}", ed.doc().title)
    });
    let trace = sim.trace().to_vec();
    drop(sim);
    drop(ed);
    trace
}

fn input(sim: &mut Sim, ed: &mut Editor, fetched: &str) -> String {
    let c = &mut sim.choices;
    // (by arm: the page 3, another part of it 2, the PDF 1, the session's copy 2, a fetch 2, a refused address 1, a
    // stop 1, a close 1, time 2, a frame 1)
    match c.weighted(&[3, 2, 1, 2, 2, 1, 1, 1, 2, 1]) {
        0 => typed(ed, "web a.html"),
        1 => typed(ed, "web a.html page"),
        2 => typed(ed, "web b.pdf"),
        3 => typed(ed, "web https://kept.test/"),
        // (this machine's page, never kept: every pick goes to the server — the kept copy is arm 3's and the fetch
        // tests')
        4 => typed(ed, fetched).replace(fetched, "web http://localhost/"),
        5 => typed(ed, "web https://u:p@kept.test/"),
        6 => typed(ed, "web-stop"),
        7 if ed.docs.len() > 1 => typed(ed, "buffer-close"),
        7 => "(one buffer: none closed)".into(),
        8 => {
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

fn typed(ed: &mut Editor, line: &str) -> String {
    match crate::typed::execute(ed, line) {
        Ok(()) => format!(":{line}"),
        Err(e) => format!(":{line} — {e}"),
    }
}

/// One buffer at most a page's title; none for the PDF; no fetch's step once none is under way.
fn held(ed: &Editor) -> Result<(), String> {
    if ed.session.web_fetch.is_none() && ed.toasts.iter().any(|t| t.step == Some("web")) {
        return Err("a fetch's step left with none under way".into());
    }
    let web: Vec<&str> =
        ed.docs.iter().filter_map(|d| d.title.as_deref()).filter(|t| t.starts_with("web ·")).collect();
    let mut seen = std::collections::HashSet::new();
    if let Some(twice) = web.iter().find(|t| !seen.insert(**t)) {
        return Err(format!("two buffers of {twice:?}"));
    }
    match web.iter().find(|t| t.contains("pdf")) {
        Some(t) => Err(format!("a buffer for what isn't a page: {t:?}")),
        None => Ok(()),
    }
}
