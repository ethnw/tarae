//! `:web https://… [main|article|page]`: a page fetched (`fetch.rs` — yours: no card, the address you typed is the
//! yes) on a worker, said as a step while it runs (`:web-stop`, or the next `:web`, stops it); the session's copy
//! instead when it fetched that address within `fetch::KEEP`. `:web <path> [url] [main|article|page]`: a saved page
//! — the path as `:open` takes it (`~` spelled out, from the project's folder, an ignored folder's as any), the
//! buffer's own file without one; `url` where it came from, for its links (else the file's own address). The last word
//! which part to read (else the reader's guess). Read on a worker (a page's bytes are anyone's), shown as `web ·
//! docs.rs › Rope in ropey` in markdown, read-only (a page is something read, never saved: an edit put back and said),
//! how it read on the status line; what isn't a page is said, no buffer made.

use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::fetch::{self, Fetched, Stop};
use super::{PAGE_MAX, Page, Prefer, Read};
use crate::editor::Editor;

const USAGE: &str = "usage: :web <https://…> | <saved page> [its address] — then [main|article|page]";

/// `:web` (`again`: `:web!` — fetched anew whatever the session kept).
pub fn web(ed: &mut Editor, rest: &str, again: bool) -> Result<(), String> {
    let (mut path, mut url, mut prefer) = (None, None, Prefer::Guess);
    for w in crate::typed::shell_words(rest) {
        match Prefer::named(&w) {
            Some(p) => prefer = p,
            None if w.contains("://") => url = Some(w),
            None => path = Some(crate::typed::typed_path(ed, &w)),
        }
    }
    let (path, url) = match (path, url) {
        (Some(p), url) => (p, url),
        (None, Some(u)) => return fetch(ed, u, prefer, again),
        (None, None) => (ed.doc().path.clone().ok_or(USAGE)?, None),
    };
    let url = url.unwrap_or_else(|| file_url(&path));
    ed.events.jobs().spawn(move || {
        let page = bytes_of(&path).map(|b| super::read(&b, &url, None, prefer));
        let place =
            path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned());
        move |ed: &mut Editor| shown(ed, page, &url, &place, Vec::new())
    });
    Ok(())
}

/// The step a fetch is said by — by hand, not `Editor::step`: a fetch the next `:web` replaced ends after it began,
/// and mustn't take the newer one's step with it.
const STEP: &str = "web";

/// `:web https://…`: the fetch under way stopped first (whatever answers this one), then the session's copy within
/// `fetch::KEEP` (no network) but with `again`, else fetched on a worker.
fn fetch(ed: &mut Editor, url: String, prefer: Prefer, again: bool) -> Result<(), String> {
    let t = super::http::target(&url)?;
    if let Some(old) = ed.session.web_fetch.take() {
        old.stop();
        ed.step_done(STEP);
    }
    let kept = ed.session.pages.get(&url, crate::clock::now()).filter(|_| !again);
    if let Some((at, f)) = kept {
        let ago = crate::clock::since(at);
        ed.events.jobs().spawn(move || {
            let page = super::read(&f.bytes, &f.url, f.content_type.as_deref(), prefer);
            move |ed: &mut Editor| fetched(ed, Ok((f, page)), Some(ago))
        });
        return Ok(());
    }
    let stop = Stop::default();
    ed.session.web_fetch = Some(stop.clone());
    let (way, mine) = (way_of(ed), stop.clone());
    ed.set_step(STEP, format!("reading {}… — :web-stop stops it", t.host));
    ed.events.jobs().spawn(move || {
        let got =
            way(stop).and_then(|way| fetch::page(&url, &way, prefer)).map(|(f, page)| (Arc::new(f), page));
        move |ed: &mut Editor| {
            // (stopped, or another `:web` since: what it came to is no one's — freed off the loop, logged only)
            if !ed.session.web_fetch.as_ref().is_some_and(|s| s.same(&mine)) {
                crate::event::drop_later(got);
                #[cfg(test)]
                {
                    ed.session.web_dropped += 1;
                }
                return crate::log::line("web", "info", "a stopped fetch's result dropped");
            }
            ed.session.web_fetch = None;
            ed.step_done(STEP);
            fetched(ed, got, None)
        }
    });
    Ok(())
}

/// How the fetch's way is made on its worker: yours (`fetch::yours` — it reads the environment and may make the TLS
/// config: never on the main loop); a test's stand-in where it set one.
fn way_of(ed: &Editor) -> Box<dyn FnOnce(Stop) -> Result<fetch::Way, String> + Send> {
    #[cfg(test)]
    if let Some(make) = ed.session.web_way.clone() {
        return Box::new(move |stop| Ok(make(stop)));
    }
    let (root, upstream) = (ed.session.root.clone(), ed.config.llm.sandbox_upstream);
    Box::new(move |stop| fetch::yours(&root, crate::sandbox::upstream_now(upstream), stop))
}

/// `:web-stop`: the fetch under way stopped (its socket shut — its worker's read returns at once).
pub fn stop(ed: &mut Editor) -> Result<(), String> {
    let s = ed.session.web_fetch.take().ok_or("no page is being fetched")?;
    s.stop();
    ed.step_done(STEP);
    ed.set_status("web: stopped");
    Ok(())
}

/// A fetched page (or the session's copy, `ago`) shown, kept for the session where it's one to keep (`keeps`), said:
/// its status where it isn't 2xx, how long it took or how old the copy is, the hosts it came through, an upgrade to
/// https.
fn fetched(ed: &mut Editor, got: Result<(Arc<Fetched>, Page), String>, ago: Option<Duration>) {
    let (f, page) = match got {
        Ok(got) => got,
        Err(e) => return ed.set_error(format!("web: {e}")),
    };
    let mut how = Vec::new();
    if !(200..300).contains(&f.status) {
        how.push(format!("status {}", f.status));
    }
    match ago {
        Some(a) => how.push(format!("read {}", crate::term::ago(a, true))),
        None => {
            how.push(crate::term::run_duration(f.took));
            if keeps(&f) {
                let gone = ed.session.pages.put(f.clone(), crate::clock::now());
                crate::event::drop_later(gone);
            }
        }
    }
    if !f.from.is_empty() {
        how.push(format!("from {}", f.from.join(" › ")));
    }
    if f.upgraded {
        how.push("over https".into());
    }
    shown(ed, Ok(page), &f.url, &f.url, how);
    if let Some(said) = &f.said {
        ed.set_warning(format!("web: {said}"));
    }
}

/// Whether the session keeps `f` for the next `:web` of it: a whole 2xx page — never one cut at the limit or maybe
/// cut short, never an error's page — and never this machine's (a dev server changes under you).
fn keeps(f: &Fetched) -> bool {
    let here = super::http::target(&f.url).is_ok_and(|t| t.loopback);
    (200..300).contains(&f.status) && f.said.is_none() && !f.cut && !here
}

/// A path's `file://` address: every byte but RFC 3986's unreserved and `/` percent-encoded, so a name holding
/// `#`, `?` or `%` stays the path's (a saved page's relative links resolve against its own folder).
pub(super) fn file_url(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt;
    let mut url = String::from("file://");
    for &b in path.as_os_str().as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                url.push(char::from(b))
            }
            _ => url.push_str(&format!("%{b:02X}")),
        }
    }
    url
}

/// The file's first `PAGE_MAX` bytes and one more (so the reader says it was cut) — a plain file only, judged by
/// what was opened: opened without waiting (a FIFO with no writer would hold the worker inside open(2)), and a FIFO
/// or a device refused, said.
pub(super) fn bytes_of(path: &Path) -> Result<Vec<u8>, String> {
    let said = |e: std::io::Error| format!("{}: {e}", path.display());
    let opened = crate::paths::open_read(path).map_err(said)?;
    if !opened.meta.is_file() {
        return Err(format!("{}: not a file", path.display()));
    }
    let mut bytes = Vec::new();
    opened.file.take(PAGE_MAX as u64 + 1).read_to_end(&mut bytes).map_err(said)?;
    Ok(bytes)
}

/// Every event: a page's buffer as it was read — an edit put back (it's read-only — said where you look), insert
/// mode left.
pub fn sync(ed: &mut Editor) {
    let current = ed.doc().id;
    let mut said = false;
    for d in ed.docs.iter_mut() {
        said |= d.hold_read_only() && d.id == current;
    }
    let typing = ed.mode == crate::editor::Mode::Insert && ed.doc().read_only.is_some();
    if typing {
        ed.mode = crate::editor::Mode::Normal;
        ed.insert_rec = None;
    }
    if said || typing {
        ed.set_status("a web page is read-only — :web reads it again");
    }
}

/// The page in a buffer of its own, read-only (one of that title before it goes: nothing in it was yours), how it
/// read on the status line — `place` naming it where its address has no host, `how` said after.
fn shown(ed: &mut Editor, page: Result<Page, String>, url: &str, place: &str, how: Vec<String>) {
    let page = match page {
        Ok(p) => p,
        Err(e) => return ed.set_error(format!("web: {e}")),
    };
    if page.read == Read::None {
        return ed.set_warning(format!("web: {}", page.said.unwrap_or_default()));
    }
    let place = super::url::host(url).unwrap_or_else(|| place.to_string());
    let title = match page.title.is_empty() {
        true => format!("web · {place}"),
        false => format!("web · {place} › {}", page.title),
    };
    let old: Vec<_> =
        ed.docs.iter().filter(|d| d.throwaway && d.title.as_deref() == Some(&title)).map(|d| d.id).collect();
    for d in old {
        ed.close_doc(d);
    }
    let id = ed.open_virtual(&title, &page.text);
    if let Some(d) = ed.docs.iter_mut().find(|d| d.id == id) {
        // (a paragraph is a line: read whole)
        d.wrap = Some(true);
        d.read_only = Some(d.snapshot());
    }
    if let Some(spec) = page.language.as_deref().and_then(crate::syntax::spec) {
        ed.load_language(id, spec);
    }
    let mut status = format!(
        "{} · {}k chars · {} links · {} anchors · {}",
        page.read.word(),
        page.text.chars().count().div_ceil(1000),
        page.links.len(),
        page.anchors.len(),
        page.charset
    );
    if let Some(r) = &page.refresh {
        let after = crate::term::run_duration(std::time::Duration::from_secs(r.secs));
        status.push_str(&format!(" · refreshes to {} after {after}", r.url));
    }
    for h in how {
        status.push_str(&format!(" · {h}"));
    }
    ed.set_status(status);
    if let Some(said) = page.said {
        ed.set_warning(format!("web: {said}"));
    }
}
