//! A page is a buffer, as code is (web P3, the user's 2026-10-23): `:web` makes it at once — empty, its top line
//! `reading docs.rs…` with a timer and `C-c stop` — and fills it as the page lands, one transaction; read-only (an edit
//! put back, said: `open::sync`), its address its `virtual_uri` (kept out of `:restart`, recovery and `:wa` as a
//! server's document is), its links, anchors and headings kept beside its text (`Facts`: what the page's own server
//! answers from). A buffer is one copy: `:web` of an address open shows that buffer, `:web!` and `:reload` read it
//! anew into it. The fetch under way is its buffer's: closing the buffer stops it (`Fetching`'s drop), leaving it
//! doesn't — a result whose buffer went is dropped.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::fetch::{self, Fetched, Stop};
use super::{Page, Prefer, Read};
use crate::document::DocId;
use crate::editor::Editor;

/// A page's buffer: where it came from, how it read, the fetch under way.
pub struct PageOf {
    /// The address asked, its fragment taken off (`:web`'s, a link's).
    pub asked: String,
    pub prefer: Prefer,
    /// A saved page's file (`:web <path>`), read again by `:web` of it.
    pub saved: Option<PathBuf>,
    /// The copy its text was read from, and when it was fetched (a saved page has none).
    pub copy: Option<(Instant, Arc<Fetched>)>,
    /// How it read, once it landed.
    pub read: Option<Read>,
    pub chars: usize,
    pub facts: Arc<Facts>,
    pub fetching: Option<Fetching>,
    /// Why its last read failed (said on its line; the buffer says it where it has no text).
    pub failed: Option<String>,
    /// Where to go once it lands: the `#fragment` asked, the lines Claude read.
    want: Option<Want>,
    /// The copy Claude read (follow, replay): said so.
    pub claudes: bool,
}

pub use super::facts::{Facts, split_fragment};

/// A read under way: what stops it, since when, what it reads (a host, a file's name).
pub struct Fetching {
    pub stop: Stop,
    pub since: Instant,
    pub what: String,
}

impl Drop for Fetching {
    /// (its buffer closed, or another read in its place: its socket shut — a call that never waits)
    fn drop(&mut self) {
        self.stop.stop();
    }
}

enum Want {
    Fragment(String),
    Lines(std::ops::Range<usize>),
}

/// What a page is read from.
enum Source {
    /// Fetched (yours: no card).
    Fetch,
    /// A copy fetched before (the session's, the one Claude read): read on a worker, no network.
    Copy(Instant, Arc<Fetched>),
    /// A saved page.
    File(PathBuf),
}

/// A read landed: the page, its text as a rope and its facts (made on the worker), the copy it came from.
struct Got {
    page: Page,
    text: ropey::Rope,
    facts: Facts,
    copy: Option<(Instant, Arc<Fetched>)>,
    /// Read from a saved page's file: the buffer's source from now on (a copy's or a fetch's: none).
    saved: Option<PathBuf>,
    /// Fetched now (not a copy kept): kept for the session where it's one to keep, said with how long it took.
    fresh: bool,
}

/// The page `doc` shows, if it's one.
pub fn of(doc: &crate::document::Document) -> Option<&PageOf> {
    doc.page.as_deref()
}

fn page_mut(ed: &mut Editor, id: DocId) -> Option<&mut PageOf> {
    ed.docs.iter_mut().find(|d| d.id == id).and_then(|d| d.page.as_deref_mut())
}

/// The front session's buffer of the page at `url` (no fragment) but `but`: by the address it came from or the one
/// asked.
fn find(ed: &Editor, url: &str, but: Option<DocId>) -> Option<DocId> {
    ed.buffers().into_iter().map(|i| &ed.docs[i]).filter(|d| Some(d.id) != but).find_map(|d| {
        let p = of(d)?;
        (d.virtual_uri.as_deref() == Some(url) || p.asked == url).then_some(d.id)
    })
}

fn show(ed: &mut Editor, id: DocId) {
    if ed.doc().id == id {
        return;
    }
    if let Some(i) = ed.docs.iter().position(|d| d.id == id) {
        ed.push_jump();
        ed.current = i;
    }
}

/// `:web <url>` (`saved`: a saved page, `url` where it came from): its buffer shown — read again only where asked
/// (`again`: `:web!`; another part; a saved page, its file read anew), else made at once and read into.
pub fn open(
    ed: &mut Editor,
    url: &str,
    saved: Option<PathBuf>,
    prefer: Prefer,
    again: bool,
) -> Result<(), String> {
    let (key, fragment) = split_fragment(url.trim());
    if saved.is_none() {
        super::http::target(key)?;
    }
    let want = fragment.map(|f| Want::Fragment(f.to_string()));
    if let Some(id) = find(ed, key, None) {
        show(ed, id);
        let Some(p) = page_mut(ed, id) else { return Ok(()) };
        let other = prefer != Prefer::Guess && prefer != p.prefer;
        // (one that never landed — stopped, failed — is read again)
        let none = p.read.is_none() && p.fetching.is_none();
        if !(again || other || none || saved.is_some()) {
            match (&want, p.fetching.is_some()) {
                (Some(_), true) => p.want = want,
                (Some(Want::Fragment(f)), false) => go_to_fragment(ed, &f.clone()),
                _ => {}
            }
            return Ok(());
        }
        if prefer != Prefer::Guess {
            p.prefer = prefer;
        }
        p.want = want;
        // (its own source where none is given: its copy, else its saved file)
        let source = match (saved, p.copy.clone(), p.saved.clone()) {
            (Some(path), _, _) => Source::File(path),
            (None, Some((at, f)), _) if !again => Source::Copy(at, f),
            (None, None, Some(path)) if !again => Source::File(path),
            _ => Source::Fetch,
        };
        start(ed, id, source);
        return Ok(());
    }
    let kept = ed.session.pages.get(key, crate::clock::now()).filter(|_| !again && saved.is_none());
    let place =
        super::url::host(key).or_else(|| saved.as_deref().map(name_of)).unwrap_or_else(|| key.to_string());
    let id = made(ed, &place, key, PageOf::new(key, prefer, saved.clone(), want));
    let source = match (saved, kept) {
        (Some(path), _) => Source::File(path),
        (None, Some((at, f))) => Source::Copy(at, f),
        (None, None) => Source::Fetch,
    };
    start(ed, id, source);
    Ok(())
}

/// The copy Claude read (`at`), shown at `lines` (0-based) — follow and replay (`follow::page_read`, D14): its
/// buffer when one shows that very copy, else one made for it, read on a worker, never fetched again.
pub fn open_copy(ed: &mut Editor, f: Arc<Fetched>, at: Instant, lines: std::ops::Range<usize>) {
    let shown = ed.buffers().into_iter().map(|i| &ed.docs[i]).find_map(|d| {
        let copy = of(d)?.copy.as_ref()?;
        Arc::ptr_eq(&copy.1, &f).then_some(d.id)
    });
    if let Some(id) = shown {
        show(ed, id);
        return at_claudes_lines(ed, lines);
    }
    let url = split_fragment(&f.url).0.to_string();
    let place = super::url::host(&url).unwrap_or_else(|| url.clone());
    let mut page = PageOf::new(&url, Prefer::Guess, None, Some(Want::Lines(lines)));
    page.claudes = true;
    let id = made(ed, &place, &url, page);
    start(ed, id, Source::Copy(at, f));
}

impl PageOf {
    fn new(asked: &str, prefer: Prefer, saved: Option<PathBuf>, want: Option<Want>) -> PageOf {
        PageOf {
            asked: asked.to_string(),
            prefer,
            saved,
            copy: None,
            read: None,
            chars: 0,
            facts: Arc::default(),
            fetching: None,
            failed: None,
            want,
            claudes: false,
        }
    }
}

fn name_of(path: &std::path::Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// The page's buffer, made current: empty, read-only, `web · place`.
fn made(ed: &mut Editor, place: &str, url: &str, page: PageOf) -> DocId {
    ed.push_jump();
    let id = ed.open_virtual(&format!("web · {place}"), "");
    let doc = ed.doc_mut();
    doc.virtual_uri = Some(url.to_string());
    // (a paragraph is a line: read whole)
    doc.wrap = Some(true);
    doc.read_only = Some(doc.snapshot());
    doc.page = Some(Box::new(page));
    ed.attach_lsp(id);
    id
}

/// The page read into buffer `id` from `source` on a worker — the read under way before it stopped (its result
/// dropped as it lands).
fn start(ed: &mut Editor, id: DocId, source: Source) {
    let way = matches!(source, Source::Fetch).then(|| way_of(ed));
    #[cfg(test)]
    {
        ed.session.web_fetches += usize::from(way.is_some());
    }
    let Some(p) = page_mut(ed, id) else { return };
    let stop = Stop::default();
    let what = match &source {
        Source::File(path) => name_of(path),
        Source::Copy(_, f) => super::url::host(&f.url).unwrap_or_default(),
        Source::Fetch => super::url::host(&p.asked).unwrap_or_default(),
    };
    p.fetching = Some(Fetching { stop: stop.clone(), since: crate::clock::now(), what });
    p.failed = None;
    let (url, prefer, mine) = (p.asked.clone(), p.prefer, stop.clone());
    ed.events.jobs().spawn(move || {
        let got = match (source, way) {
            (Source::Fetch, Some(way)) => way(stop)
                .and_then(|way| fetch::page(&url, &way, prefer))
                .map(|(f, page)| got(page, Some((crate::clock::now(), Arc::new(f))), true)),
            (Source::Copy(at, f), _) => {
                let page = super::read(&f.bytes, &f.url, f.content_type.as_deref(), prefer);
                Ok(got(page, Some((at, f)), false))
            }
            // (`url`: where it came from, else its own `file://` address — `open::web`)
            (Source::File(path), _) => super::open::bytes_of(&path)
                .map(|b| Got { saved: Some(path), ..got(super::read(&b, &url, None, prefer), None, false) }),
            (Source::Fetch, None) => Err("no way to fetch".into()),
        };
        move |ed: &mut Editor| landed(ed, id, &mine, got)
    });
}

/// A page read: its text as a rope and its facts, made here on the worker.
fn got(page: Page, copy: Option<(Instant, Arc<Fetched>)>, fresh: bool) -> Got {
    let text = ropey::Rope::from_str(&page.text);
    let facts = Facts::of(&page, &text);
    Got { page, text, facts, copy, saved: None, fresh }
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

/// A read landed — into its buffer while that read is still the buffer's; else no one's (its buffer closed, read
/// anew or stopped since): freed off the loop, logged only.
fn landed(ed: &mut Editor, id: DocId, mine: &Stop, got: Result<Got, String>) {
    let ours = page_mut(ed, id).filter(|p| p.fetching.as_ref().is_some_and(|f| f.stop.same(mine)));
    let Some(p) = ours else {
        crate::event::drop_later(got);
        #[cfg(test)]
        {
            ed.session.web_dropped += 1;
        }
        return crate::log::line("web", "info", "a stopped read's result dropped");
    };
    p.fetching = None;
    match got {
        Err(e) => {
            ed.set_error(format!("web: {e}"));
            said_in(ed, id, e)
        }
        // (what isn't a page: said as the reader says it)
        Ok(g) if g.page.read == Read::None => {
            let said = g.page.said.clone().unwrap_or_else(|| "not a page".into());
            crate::event::drop_later(g);
            ed.set_warning(format!("web: {said}"));
            said_in(ed, id, said)
        }
        Ok(g) => filled(ed, id, g),
    }
}

/// A failure's why for the page's line, which names its host already: `<address>: <why> · <the OS's words>` (the
/// fetch's form) → `<why>`.
pub fn brief(why: &str) -> &str {
    let why = match why.split_once(": ") {
        Some((at, rest)) if at.contains("://") => rest,
        _ => why,
    };
    why.split(" · ").next().unwrap_or(why)
}

/// Why a read ended without a page (failed, stopped, not a page): on its line (`brief`), and whole on the first row of
/// a buffer that has no page yet, faint — one that has a copy keeps it.
fn said_in(ed: &mut Editor, id: DocId, why: String) {
    let Some(doc) = ed.docs.iter_mut().find(|d| d.id == id) else { return };
    if doc.text.len_bytes() == 0 || doc.page.as_ref().is_some_and(|p| p.read.is_none()) {
        crate::disk::reload_text(doc, &ropey::Rope::from_str(&why));
        // (at its start, as a first read's: the insert would carry the cursor to its end)
        doc.set_selection(crate::selection::Selection::point(0));
        doc.drop_history();
        doc.paint = vec![(0, why.len(), crate::syntax::capture_id("ui.linenr"))];
        doc.read_only = Some(doc.snapshot());
    }
    if let Some(p) = doc.page.as_deref_mut() {
        p.failed = Some(why);
    }
}

/// A page landed in buffer `id`: its text in with one change (the cursor kept where it was, a first read's at the
/// top), named by its title, its facts kept; a redirect's address names it — where a buffer of that address is open
/// already, that one is shown (this one goes if it never landed, else keeps what it showed). How it read on the status
/// line.
fn filled(ed: &mut Editor, id: DocId, g: Got) {
    let url = match &g.copy {
        Some((_, f)) => split_fragment(&f.url).0.to_string(),
        None => ed.docs.iter().find(|d| d.id == id).and_then(|d| d.virtual_uri.clone()).unwrap_or_default(),
    };
    // (one buffer an address: one that never landed goes; one read before keeps what it showed — closing yours for
    // a redirect would take its place in your buffers and jumps — and the other is shown; where to go goes along)
    if let Some(other) = find(ed, &url, Some(id)) {
        let front = ed.doc().id == id;
        let Some(p) = page_mut(ed, id) else { return crate::event::drop_later(g) };
        let (want, landed_before) = (p.want.take(), p.read.is_some());
        match landed_before {
            true => {
                ed.set_status(format!("web: it redirects to {url}, open already — this one kept as it was"))
            }
            false => ed.close_doc(id),
        }
        crate::event::drop_later(g);
        if front {
            show(ed, other);
            go_to(ed, want);
        } else if let Some(p) = page_mut(ed, other).filter(|p| p.fetching.is_some() && want.is_some()) {
            p.want = want;
        }
        return;
    }
    if g.fresh
        && let Some((_, f)) = &g.copy
        && super::open::keeps(f)
    {
        let now = crate::clock::now();
        let gone = ed.session.pages.put(f.clone(), now);
        crate::event::drop_later(gone);
        super::serve::kept(ed, split_fragment(&f.url).0, now);
    }
    let claudes = ed.docs.iter().find(|d| d.id == id).and_then(of).is_some_and(|p| p.claudes);
    let said = how(&g, claudes);
    let Got { page, text, facts, copy, saved, .. } = g;
    let Some(doc) = ed.docs.iter_mut().find(|d| d.id == id) else { return };
    // (a saved page's own address has no host: its file's name)
    let place = super::url::host(&url)
        .or_else(|| doc.page.as_ref().and_then(|p| p.saved.as_deref()).map(name_of))
        .unwrap_or_else(|| url.clone());
    doc.title = Some(match page.title.is_empty() {
        true => format!("web · {place}"),
        false => format!("web · {place} › {}", page.title),
    });
    doc.virtual_uri = Some(url);
    doc.paint.clear();
    // (a first read starts at the top — the text inserted into the empty buffer would carry the cursor to its end)
    let first = doc.page.as_ref().is_some_and(|p| p.read.is_none());
    crate::disk::reload_text(doc, &text);
    if first {
        doc.set_selection(crate::selection::Selection::point(0));
    }
    doc.drop_history();
    doc.read_only = Some(doc.snapshot());
    let p = doc.page.as_deref_mut().expect("a page's buffer");
    p.read = Some(page.read);
    p.chars = page.text.chars().count();
    p.facts = Arc::new(facts);
    // (what it was read from is its source now: `:reload`, `:web-reader` and `:w`'s first line go by it)
    let old = std::mem::replace(&mut p.copy, copy);
    crate::event::drop_later(old);
    p.saved = saved;
    let want = p.want.take();
    let lang = (doc.syntax.is_none()).then_some(page.language.as_deref()).flatten();
    if let Some(spec) = lang.and_then(crate::syntax::spec) {
        ed.load_language(id, spec);
    }
    super::serve::told(ed, id);
    ed.set_status(said);
    if let Some(s) = page.said {
        // (the reader's words are Claude's too: what only you can do is said here)
        let browser =
            if s.starts_with("needs JavaScript") { " — :web-open opens it in your browser" } else { "" };
        ed.set_warning(format!("web: {s}{browser}"));
    }
    if ed.doc().id != id {
        return;
    }
    go_to(ed, want);
}

/// The front page's cursor where `want` says — once it has landed; a page still reading keeps it for its landing.
fn go_to(ed: &mut Editor, want: Option<Want>) {
    let id = ed.doc().id;
    if let Some(p) = page_mut(ed, id).filter(|p| p.fetching.is_some()) {
        p.want = want.or(p.want.take());
        return;
    }
    match want {
        Some(Want::Fragment(f)) => go_to_fragment(ed, &f),
        Some(Want::Lines(lines)) => at_claudes_lines(ed, lines),
        None => {}
    }
}

/// How it read: the part, its size, links and anchors, its charset, a refresh; then where it isn't 2xx, how long it
/// took or how old the copy is, the hosts it came through, an upgrade to https; a fetch's own words.
fn how(g: &Got, claudes: bool) -> String {
    let page = &g.page;
    let mut s = format!(
        "{} · {}k chars · {} links · {} anchors · {}",
        page.read.word(),
        page.text.chars().count().div_ceil(1000),
        page.links.len(),
        page.anchors.len(),
        page.charset
    );
    if let Some(r) = &page.refresh {
        let after = crate::term::run_duration(Duration::from_secs(r.secs));
        s.push_str(&format!(" · refreshes to {} after {after}", r.url));
    }
    let Some((at, f)) = &g.copy else { return s };
    if !(200..300).contains(&f.status) {
        s.push_str(&format!(" · status {}", f.status));
    }
    let ago = crate::term::ago(crate::clock::since(*at), true);
    match (g.fresh, claudes) {
        (true, _) => s.push_str(&format!(" · {}", crate::term::run_duration(f.took))),
        (false, true) => s.push_str(&format!(" · the copy Claude read {ago}")),
        (false, false) => s.push_str(&format!(" · read {ago}")),
    }
    if !f.from.is_empty() {
        s.push_str(&format!(" · from {}", f.from.join(" › ")));
    }
    if f.upgraded {
        s.push_str(" · over https");
    }
    if let Some(said) = &f.said {
        s.push_str(&format!(" · {said}"));
    }
    s
}

/// The front page's cursor at the line a fragment names, said where none does.
fn go_to_fragment(ed: &mut Editor, fragment: &str) {
    let line = of(ed.doc()).and_then(|p| p.facts.line_of(fragment));
    match line {
        Some(l) => at_lines(ed, l..l + 1),
        None => ed.set_status(format!("web: no #{fragment} on this page")),
    }
}

/// The front buffer's cursor at the start of `lines` (0-based), those lines in sight.
fn at_lines(ed: &mut Editor, lines: std::ops::Range<usize>) {
    let doc = ed.doc_mut();
    let line = lines.start.min(crate::movement::last_line(&doc.text));
    let at = crate::movement::line_start(&doc.text, line);
    doc.set_selection(crate::selection::Selection::point(at));
    ed.show_range(lines);
}

/// `at_lines` at the lines Claude read: the reader's (counted at `\n`, as its `read` numbers them) as the buffer's.
fn at_claudes_lines(ed: &mut Editor, lines: std::ops::Range<usize>) {
    let lines = match of(ed.doc()) {
        Some(p) => p.facts.buffer_line(lines.start)..p.facts.buffer_line(lines.end),
        None => lines,
    };
    at_lines(ed, lines)
}

/// Whether `C-c` is the page's now (the layer `keys_chain` names `Region::Page`): normal mode on a page whose read is
/// under way, nothing under way of the keys'.
pub fn takes(ed: &Editor) -> bool {
    ed.mode == crate::editor::Mode::Normal
        && ed.pending.is_empty()
        && ed.prompt.is_none()
        && ed.on_next_char.is_none()
        && of(ed.doc()).is_some_and(|p| p.fetching.is_some())
}

/// The page's own key: `C-c` stops its read (`Fetching`'s drop shuts its socket; its result dropped as it lands).
/// false: not its key — with no read under way `C-c` is the code's (`toggle_comments`, refused read-only, said).
pub fn key(ed: &mut Editor, key: crate::key::Key) -> bool {
    let c = crate::key::Key { code: crate::key::Code::Char('c'), ctrl: true, alt: false, shift: false };
    if key != c || !takes(ed) {
        return false;
    }
    ed.count = None;
    let p = ed.doc_mut().page.as_deref_mut().expect("takes: a page");
    let what = p.fetching.take().map(|f| f.what.clone()).unwrap_or_default();
    let id = ed.doc().id;
    said_in(ed, id, "stopped".into());
    ed.set_status(format!("web: stopped reading {what}"));
    true
}

/// The page in sight, or why there's none.
fn in_sight(ed: &Editor) -> Result<&PageOf, String> {
    of(ed.doc()).ok_or_else(|| "not a web page — :web <url> reads one".to_string())
}

/// `:reload` on a page: read anew from where it came from — fetched again (`:web!`), a saved page's file read again.
pub fn reload(ed: &mut Editor) -> Result<(), String> {
    let p = in_sight(ed)?;
    let (url, saved, prefer) = (p.asked.clone(), p.saved.clone(), p.prefer);
    let id = ed.doc().id;
    if let Some(p) = page_mut(ed, id) {
        // (a fresh copy is no longer the one Claude read)
        p.claudes &= saved.is_some();
    }
    open(ed, &url, saved.clone(), prefer, saved.is_none())
}

/// `:web-reader main|article|page`: the page in sight read again as that part, from its copy (or its saved file) —
/// nothing fetched. With no word, the part it reads now.
pub fn reader(ed: &mut Editor, rest: &str) -> Result<(), String> {
    let p = in_sight(ed)?;
    let usage = "usage: :web-reader main|article|page";
    let Some(prefer) = (!rest.is_empty()).then(|| Prefer::named(rest.trim())).flatten() else {
        return match (rest.is_empty(), p.read) {
            (true, Some(r)) => Err(format!("{usage} — it reads {} now", r.word())),
            _ => Err(usage.into()),
        };
    };
    if p.fetching.is_some() {
        return Err("the page hasn't landed yet — C-c stops its read".into());
    }
    if p.copy.is_none() && p.saved.is_none() {
        return Err("no copy of this page to read again — :reload fetches it".into());
    }
    if p.prefer == prefer {
        let said = format!("web: already read as {}", p.read.map_or("it", |r| r.word()));
        ed.set_status(said);
        return Ok(());
    }
    let (url, saved) = (p.asked.clone(), p.saved.clone());
    let id = ed.doc().id;
    let p = page_mut(ed, id).expect("in sight");
    p.prefer = prefer;
    let source = match (saved, p.copy.clone()) {
        (Some(path), _) => Source::File(path),
        (None, Some((at, f))) => Source::Copy(at, f),
        (None, None) => return Err(format!("no copy of {url}")),
    };
    start(ed, id, source);
    Ok(())
}

/// `:web-open`: the page's own address — http(s) only, nothing else of it — handed to the system's opener (`open`,
/// `xdg-open`) on a worker; what it says when it fails said.
pub fn web_open(ed: &mut Editor) -> Result<(), String> {
    in_sight(ed)?;
    let url = ed.doc().virtual_uri.clone().unwrap_or_default();
    if !super::facts::followed(&url) {
        return Err("a saved page has no web address — :web-open opens only http(s)".into());
    }
    #[cfg(test)]
    {
        ed.session.web_opened.push(url.clone());
        ed.set_status(format!("web: {url} handed to your browser"));
        Ok(())
    }
    #[cfg(not(test))]
    {
        let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
        let said = url.clone();
        ed.events.jobs().spawn(move || {
            let ended = opened_by(program, &url);
            move |ed: &mut Editor| match ended {
                Ok(()) => ed.set_status(format!("web: {said} handed to your browser")),
                Err(e) => ed.set_error(format!("web: {program}: {e}")),
            }
        });
        Ok(())
    }
}

/// `program url`, in a session of its own (it never takes the terminal), waited for here on the worker: its last
/// line of errors where it fails.
#[cfg(not(test))]
fn opened_by(program: &str, url: &str) -> Result<(), String> {
    use std::io::Read as _;
    let mut cmd = std::process::Command::new(program);
    cmd.arg(url);
    let s = crate::spawn::setsid(&cmd).map_err(|e| e.to_string())?;
    let mut err = String::new();
    // (both pipes drained: one left full would hold it)
    std::thread::scope(|sc| {
        if let Some(mut o) = s.stdout {
            crate::event::scoped(sc, "tarae-web-open", move || std::io::copy(&mut o, &mut std::io::sink()));
        }
        if let Some(mut e) = s.stderr {
            let _ = e.read_to_string(&mut err);
        }
    });
    let status = crate::spawn::wait(s.pid).map_err(|e| e.to_string())?;
    match status.success() {
        true => Ok(()),
        false => Err(err
            .lines()
            .rfind(|l| !l.trim().is_empty())
            .map_or_else(|| status.to_string(), str::to_string)),
    }
}

/// Tests: a page's buffer in front holding `text`, being read (again, where it holds some) — no job behind it (nothing
/// lands).
#[cfg(test)]
pub fn stand_in(ed: &mut Editor, url: &str, text: &str) -> DocId {
    let place = super::url::host(url).unwrap_or_default();
    let id = made(ed, &place, url, PageOf::new(url, Prefer::Guess, None, None));
    let doc = ed.doc_mut();
    crate::disk::reload_text(doc, &ropey::Rope::from_str(text));
    doc.drop_history();
    doc.read_only = Some(doc.snapshot());
    let p = page_mut(ed, id).expect("made");
    p.fetching = Some(Fetching { stop: Stop::default(), since: crate::clock::now(), what: place });
    id
}

/// Whether a page in sight is being read: its line's timer moves (`tick_wanted`).
pub fn reading_in_sight(ed: &Editor) -> bool {
    let fetching =
        |id: DocId| ed.docs.iter().any(|d| d.id == id && of(d).is_some_and(|p| p.fetching.is_some()));
    fetching(ed.doc().id) || ed.session.views.iter().any(|v| fetching(v.doc))
}

/// `:w <path>` on a page: its text as shown, after a first line saying where it came from — an HTML comment, so the
/// markdown reads the same (`<!-- https://docs.rs/… · read 2026-10-23 14:02 -->`) — written to a file of yours by
/// path, as your `:w` writes (one there refused but with `!`), on a worker; said with `:open`. `:w` alone says so.
pub fn write(ed: &mut Editor, path: &str, force: bool) -> Result<(), String> {
    let Some(p) = crate::typed::shell_words(path).into_iter().next() else {
        return Err("a web page is read-only — :w <path> saves a copy".into());
    };
    let to = std::path::absolute(crate::typed::typed_path(ed, &p)).map_err(|e| e.to_string())?;
    if let Some(why) = crate::keepclose::refused(ed, [to.as_path()], "write it") {
        return Err(why);
    }
    let doc = ed.doc();
    let page = of(doc).ok_or("not a web page")?;
    if page.read.is_none() {
        return Err("the page hasn't landed yet".into());
    }
    let url = doc.virtual_uri.clone().unwrap_or_default();
    let read_at = match &page.copy {
        Some((at, _)) => crate::clock::system() - crate::clock::since(*at),
        None => crate::clock::system(),
    };
    let (text, shown) = (doc.text.clone(), ed.shown(&to).display().to_string());
    ed.events.jobs().spawn(move || {
        let (y, mo, d) = crate::log::day(read_at);
        let head = format!("<!-- {url} · read {y:04}-{mo:02}-{d:02} {} -->\n", crate::log::clock(read_at));
        let wrote = written(&to, &head, &text, force);
        move |ed: &mut Editor| match wrote {
            Ok(()) => ed.set_success(format!("saved a copy of the page to {shown} — :open {shown}")),
            Err(e) => ed.set_error(format!("web: {e}")),
        }
    });
    Ok(())
}

/// `head` then `text` into `to`: a new file, or with `force` one there written over.
fn written(to: &std::path::Path, head: &str, text: &ropey::Rope, force: bool) -> Result<(), String> {
    use std::io::Write as _;
    let said = |e: std::io::Error| match e.kind() {
        std::io::ErrorKind::AlreadyExists => format!("{} is there — :w! {0} writes over it", to.display()),
        _ => format!("{}: {e}", to.display()),
    };
    let mut o = std::fs::OpenOptions::new();
    o.write(true);
    match force {
        true => o.create(true).truncate(true),
        false => o.create_new(true),
    };
    let mut f = std::io::BufWriter::new(o.open(to).map_err(said)?);
    f.write_all(head.as_bytes()).map_err(said)?;
    for chunk in text.chunks() {
        f.write_all(chunk.as_bytes()).map_err(said)?;
    }
    f.flush().map_err(said)
}
