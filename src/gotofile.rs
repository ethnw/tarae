//! `gf` (`goto_file`, as Helix has it): what the selections name, opened — each selection's text, or with one
//! selection of a character the path or address around the cursor (a quote or bracket beside it too). An http(s)
//! address opens as a page (`web::page::open`, as `:web`); `file://` and a path as `:open` takes them — `~` spelled
//! out, `file:12:5` at its line and column, a relative one from the document's folder, else from the project's; a
//! folder its file list. Whether each is there is asked on a worker (a stat is a disk wait), then opened on the loop.

use std::path::{Path, PathBuf};

use ropey::Rope;

use crate::editor::Editor;

/// How far around the cursor a path is looked for.
const AROUND: usize = 1000;

/// What a name in the text came to on the worker.
enum Found {
    Page(String),
    File(PathBuf, Option<(usize, usize)>),
    Folder(PathBuf),
    /// Why it opens nothing.
    Not(String),
}

/// Whether `c` can be part of a path or an address written in text.
fn in_path(c: char) -> bool {
    !c.is_whitespace() && !c.is_control() && !"\"'`()[]{}<>,;|".contains(c)
}

/// The path or address around byte `at` (the cursor) — or right after it, where the cursor is on a quote or bracket
/// before one — its trailing punctuation left out (`a.rs.`, `see https://x.test/:`).
pub fn around(text: &Rope, at: usize) -> Option<String> {
    let line = text.byte_to_line(at.min(text.len_bytes()));
    let (start, end) = (text.line_to_byte(line), text.line_to_byte(line + 1));
    let snap = |b: usize| text.char_to_byte(text.byte_to_char(b));
    let (from, to) = (snap(at.saturating_sub(AROUND).max(start)), snap((at + AROUND).min(end)));
    let s = text.byte_slice(from..to).to_string();
    let mut i = at - from;
    if !s[i..].chars().next().is_some_and(in_path) {
        i += s[i..].chars().next()?.len_utf8();
        s[i..].chars().next().filter(|&c| in_path(c))?;
    }
    let head = s[..i].char_indices().rev().take_while(|&(_, c)| in_path(c)).last().map_or(i, |(b, _)| b);
    let tail = s[i..].char_indices().find(|&(_, c)| !in_path(c)).map_or(s.len(), |(b, _)| i + b);
    let word = s[head..tail].trim_end_matches(['.', ',', ':', '!', '?']);
    (!word.is_empty()).then(|| word.to_string())
}

/// What the selections name: one selection of a character (or none) — the path around it, else that character;
/// otherwise each selection's text, trimmed (the empty left out).
pub fn targets(text: &Rope, sel: &crate::selection::Selection) -> Vec<String> {
    let one = sel.primary();
    let single = one.to() <= crate::graphemes::next_boundary(text, one.from());
    if sel.len() == 1 && single {
        let at = one.cursor(text);
        let word = around(text, at).or_else(|| {
            Some(text.byte_slice(one.from()..one.to()).to_string()).filter(|s| !s.trim().is_empty())
        });
        return word.into_iter().collect();
    }
    let words = sel.ranges().iter().map(|r| text.byte_slice(r.from()..r.to()).to_string());
    words.map(|w| w.trim().to_string()).filter(|w| !w.is_empty()).collect()
}

/// `gf`.
pub fn goto_file(ed: &mut Editor) {
    let doc = ed.doc();
    let names = targets(&doc.text, doc.selection());
    if names.is_empty() {
        return ed.set_status("gf: no path or address here");
    }
    // (where a relative path is looked for: the document's folder, then the project's)
    let mut places: Vec<PathBuf> =
        doc.path.as_deref().and_then(Path::parent).map(Path::to_path_buf).into_iter().collect();
    places.push(ed.session.root.clone());
    let home = crate::allow::home();
    ed.events.jobs().spawn(move || {
        let found: Vec<Found> = names.iter().map(|n| found(n, &places, home.as_deref())).collect();
        move |ed: &mut Editor| opened(ed, found)
    });
}

/// What `name` is: an address, or the first of `places` it's a file or folder in (as typed, then as `file:line:col`).
fn found(name: &str, places: &[PathBuf], home: Option<&Path>) -> Found {
    if crate::web::facts::followed(name) {
        return Found::Page(name.to_string());
    }
    let lower = name.to_ascii_lowercase();
    let path = match lower.strip_prefix("file://") {
        Some(_) => crate::web::facts::decoded(&name["file://".len()..]),
        None if name.contains("://")
            || ["mailto:", "javascript:", "data:", "tel:"].iter().any(|s| lower.starts_with(s)) =>
        {
            let scheme = name.split(':').next().unwrap_or_default();
            return Found::Not(format!("{scheme}: — tarae follows only http(s)"));
        }
        None => name.to_string(),
    };
    let path = crate::typed::expand_home(&path, home);
    let tries = match path.is_absolute() {
        true => vec![path],
        false => places.iter().map(|p| p.join(&path)).collect(),
    };
    for p in tries {
        if p.is_dir() {
            return Found::Folder(p);
        }
        if p.exists() {
            return Found::File(p, None);
        }
        if let Some((file, line, col)) = crate::typed::at_line(&p, |p| p.is_file()) {
            return Found::File(file, Some((line, col)));
        }
    }
    Found::Not(format!("no file {name}"))
}

/// Each found one opened in turn (the last in front); what opened nothing said.
fn opened(ed: &mut Editor, found: Vec<Found>) {
    let mut not = Vec::new();
    for f in found {
        let done = match f {
            Found::Page(url) => crate::web::page::open(ed, &url, None, crate::web::Prefer::Guess, false),
            Found::File(p, Some((line, col))) => ed.open_at(&p, line, col).map_err(|e| format!("{e:#}")),
            Found::File(p, None) => {
                ed.push_jump();
                ed.open(&p).map_err(|e| format!("{e:#}"))
            }
            Found::Folder(p) => {
                folder(ed, p);
                Ok(())
            }
            Found::Not(why) => Err(why),
        };
        // (the pane follows each in turn: else the one `gf` left is recorded after them all, and `C-o` skips back
        // over the ones before the last)
        ed.track_doc_switch();
        if let Err(e) = done {
            not.push(e);
        }
    }
    if !not.is_empty() {
        ed.set_status(format!("gf: {}", not.join(" · ")));
    }
}

/// A folder: its files, in the file picker's tree.
fn folder(ed: &mut Editor, root: PathBuf) {
    let title = format!("files in {}", ed.shown(&root).display());
    let picker = crate::picker::Picker::of("files", title, Vec::new(), true).with_tree(root.clone(), None);
    ed.open_picker(picker, Some(Box::new(move || Ok(crate::picker::file_items(&root)))));
}

#[cfg(test)]
mod tests;
