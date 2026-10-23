//! `:web https://… [main|article|page]`: a page fetched (`fetch.rs` — yours: no card, the address you typed is the
//! yes) on a worker into its buffer, made at once (`page.rs`: `C-c` on it stops the fetch); the session's copy
//! instead when it fetched that address within `fetch::KEEP`. `:web <path> [url] [main|article|page]`: a saved page
//! — the path as `:open` takes it (`~` spelled out, from the project's folder, an ignored folder's as any), the
//! buffer's own file without one; `url` where it came from, for its links (else the file's own address). The last word
//! which part to read (else the reader's guess). Read on a worker (a page's bytes are anyone's), shown as `web ·
//! docs.rs › Rope in ropey` in markdown, read-only (a page is something read: an edit put back and said; `:w <path>`
//! saves a copy), how it read on the status line; what isn't a page is said on its buffer's line.

use std::io::Read as _;
use std::path::Path;

use super::fetch::Fetched;
use super::{PAGE_MAX, Prefer};
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
        (None, Some(u)) => return super::page::open(ed, &u, None, prefer, again),
        (None, None) => (ed.doc().path.clone().ok_or(USAGE)?, None),
    };
    let url = url.unwrap_or_else(|| file_url(&path));
    super::page::open(ed, &url, Some(path), prefer, again)
}

/// Whether the session keeps `f` for the next `:web` of it: a whole 2xx page — never one cut at the limit or maybe
/// cut short, never an error's page — and never this machine's (a dev server changes under you).
pub(super) fn keeps(f: &Fetched) -> bool {
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
        ed.set_status("a web page is read-only — :w <path> saves a copy");
    }
}
