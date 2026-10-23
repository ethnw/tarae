//! A page's own language server (web P3: "a link is a definition"): tarae's, in-process, on the LSP routes — no
//! program (`lsp::Client::own`), one for every page, made as the first page's buffer is. Its answers are `facts.rs`'s,
//! from what the reader found (kept here by address while its page lives — a `Weak`: a page closed is gone from it),
//! computed on a worker as a server's come: `gd` a link's target (another page, or an `#anchor` a line of this one),
//! `space k` where it goes, `space s` the headings, `gr` the links to a heading or to the same address. Nothing else:
//! completion, formatting, renames and the rest are said as a server that can't do them is.

use std::sync::{Arc, LazyLock, Mutex};
use std::time::Instant;

use super::facts::{Known, answer, followed, split_fragment};
use super::page;
use crate::document::DocId;
use crate::editor::Editor;
use crate::lsp::{ClientId, ServerTraits, Virtual};

/// Its name, in the log, `:lsp-*` and the status line — and its settings' (`[lsp.web] except = ["hover"]`).
pub const NAME: &str = "web";

/// Its traits: its documents are pages at their http(s) addresses — a definition there opens a page
/// (`open_server_doc` → `follow`).
static TRAITS: LazyLock<ServerTraits> = LazyLock::new(|| ServerTraits {
    virtual_docs: Some(Virtual {
        scheme: "http".into(),
        fetch: String::new(),
        language: "markdown".into(),
        title_join: None,
    }),
    ..ServerTraits::default()
});

pub type Store = Arc<Mutex<Known>>;

/// An age as every place says it.
fn ago(d: std::time::Duration) -> String {
    crate::term::ago(d, true)
}

/// The server for pages (made again where it was stopped: `:lsp-stop`), its id.
fn ensure(ed: &mut Editor) -> ClientId {
    if let Some((cid, _)) = &ed.lsp.web
        && ed.client(*cid).is_some()
    {
        return *cid;
    }
    let store = Store::new(Mutex::new(Known { keep: super::fetch::KEEP, ..Known::default() }));
    {
        let mut k = store.lock().unwrap_or_else(|e| e.into_inner());
        for d in &ed.docs {
            if let (Some(p), Some(uri)) = (page::of(d), d.virtual_uri.as_ref()) {
                k.pages.insert(uri.clone(), (Arc::downgrade(&p.facts), p.copy.as_ref().map(|c| c.0)));
            }
        }
    }
    let mine = store.clone();
    let serve: crate::lsp::Serve = Box::new(move |method, params| {
        let k = mine.lock().unwrap_or_else(|e| e.into_inner());
        answer(&k, method, params, crate::clock::now(), ago)
    });
    let cid = ed.lsp_own(NAME, &TRAITS, serve);
    ed.lsp.web = Some((cid, store));
    cid
}

/// Page `id`'s buffer gets the server: its main and only one (pages have no path: no other attaches).
pub fn attach(ed: &mut Editor, id: DocId) {
    let cid = ensure(ed);
    if let Some(d) = ed.docs.iter_mut().find(|d| d.id == id) {
        d.lsp.attach(cid, false, true);
    }
    told(ed, id);
}

/// Page `id`'s facts as they are now (it landed, read anew) — what the server answers from at its address.
pub fn told(ed: &Editor, id: DocId) {
    let Some((_, store)) = &ed.lsp.web else { return };
    let Some(d) = ed.docs.iter().find(|d| d.id == id) else { return };
    let (Some(p), Some(uri)) = (page::of(d), d.virtual_uri.clone()) else { return };
    let mut k = store.lock().unwrap_or_else(|e| e.into_inner());
    k.pages.retain(|_, (f, _)| f.strong_count() > 0);
    k.pages.insert(uri, (Arc::downgrade(&p.facts), p.copy.as_ref().map(|c| c.0)));
}

/// A copy the session keeps, read `at` — a hover on a link to it says so.
pub fn kept(ed: &Editor, url: &str, at: Instant) {
    let Some((_, store)) = &ed.lsp.web else { return };
    let mut k = store.lock().unwrap_or_else(|e| e.into_inner());
    k.kept.retain(|_, t| crate::clock::since(*t) < super::fetch::KEEP);
    k.kept.insert(url.to_string(), at);
}

/// A link's definition, from tarae's own server: the page it names — its buffer, or fetched (yours) — at its
/// `#fragment`.
pub fn follow(ed: &mut Editor, url: &str) {
    if let Err(e) = page::open(ed, url, None, super::Prefer::Guess, false) {
        ed.set_status(format!("web: {e}"));
    }
}

/// Why `gd` on page `doc` at `head` found nothing, in the page's words: a link that isn't followed, an anchor it
/// hasn't, no link there.
pub fn why_none(ed: &Editor, doc: DocId, head: usize) -> Option<String> {
    let d = ed.docs.iter().find(|d| d.id == doc)?;
    let facts = &page::of(d)?.facts;
    let Some(link) = facts.link_at(head) else { return Some("not on a link".into()) };
    if !followed(&link.url) {
        let scheme = link.url.split(':').next().unwrap_or_default();
        return Some(format!("a {scheme}: link — tarae follows only http(s)"));
    }
    let fragment = split_fragment(&link.url).1.unwrap_or_default();
    Some(format!("no #{fragment} on this page"))
}

#[cfg(test)]
mod tests;
