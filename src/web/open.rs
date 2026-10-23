//! `:web <path> [url] [main|article|page]`: a saved page read into a buffer — the path as `:open` takes it (`~`
//! spelled out, from the project's folder, an ignored folder's as any), the buffer's own file without one; `url`
//! where it came from, for its links (else the file's own address); the last word which part to read (else the
//! reader's guess). Read on a worker (a page's bytes are anyone's), shown as `web · docs.rs › Rope in ropey` in
//! markdown, read-only (a page is something read, never saved: an edit put back and said), how it read on the status
//! line; what isn't a page is said, no buffer made. Until tarae fetches pages (the next piece), an address alone is
//! said so.

use std::io::Read as _;
use std::path::Path;

use super::{PAGE_MAX, Page, Prefer, Read};
use crate::editor::Editor;

const USAGE: &str = "usage: :web <saved page> [its address] [main|article|page]";

pub fn saved(ed: &mut Editor, rest: &str) -> Result<(), String> {
    let (mut path, mut url, mut prefer) = (None, None, Prefer::Guess);
    for w in crate::typed::shell_words(rest) {
        match Prefer::named(&w) {
            Some(p) => prefer = p,
            None if w.contains("://") => url = Some(w),
            None => path = Some(crate::typed::typed_path(ed, &w)),
        }
    }
    let path = match (path, &url) {
        (Some(p), _) => p,
        (None, Some(u)) if ed.doc().path.is_none() => {
            let usage = USAGE.trim_start_matches("usage: ");
            return Err(format!("tarae doesn't fetch pages yet: {u} — save it, then {usage}"));
        }
        (None, _) => ed.doc().path.clone().ok_or(USAGE)?,
    };
    let url = url.unwrap_or_else(|| file_url(&path));
    ed.events.jobs().spawn(move || {
        let page = bytes_of(&path).map(|b| super::read(&b, &url, None, prefer));
        move |ed: &mut Editor| shown(ed, page, &url, &path)
    });
    Ok(())
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
/// read on the status line.
fn shown(ed: &mut Editor, page: Result<Page, String>, url: &str, path: &Path) {
    let page = match page {
        Ok(p) => p,
        Err(e) => return ed.set_error(format!("web: {e}")),
    };
    if page.read == Read::None {
        return ed.set_warning(format!("web: {}", page.said.unwrap_or_default()));
    }
    let place = super::url::host(url).unwrap_or_else(|| {
        path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned())
    });
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
    ed.set_status(status);
    if let Some(said) = page.said {
        ed.set_warning(format!("web: {said}"));
    }
}
