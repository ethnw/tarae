//! The list — places to go through one by one (`]q` `[q`), kept after the picker that found them closes:
//! a build task's errors (task.rs), or any picker of places pinned with `C-q` (search results, references,
//! diagnostics). `space q` shows it in the side panel.
//!
//! Items in open files sit on document marks, so they follow edits — fix the third error by adding lines and
//! the fourth is still where it was. A file opened later gets its marks then.

use std::path::{Path, PathBuf};

use crate::document::DocId;
use crate::editor::Editor;
use crate::picker::Action;
use crate::selection::Selection;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub path: PathBuf,
    /// Where (0-based line, byte column) — as found; the mark once the file is open.
    pub line: usize,
    pub col: usize,
    pub message: String,
    /// 1 error · 2 warning · 3 info (0 = just a place)
    pub severity: u8,
    pub mark: Option<(DocId, u64)>,
}

#[derive(Default)]
pub struct List {
    pub title: String,
    pub items: Vec<Item>,
    /// The item `]q` last went to.
    pub current: Option<usize>,
    /// What's in it already (path, line, message) — a streaming task repeats itself.
    keys: std::collections::HashSet<(PathBuf, usize, String)>,
    /// Bumped on every change (the side panel rebuilds its rows only then).
    pub generation: u64,
}

fn key(i: &Item) -> (PathBuf, usize, String) {
    (i.path.clone(), i.line, i.message.clone())
}

/// Marks the list keeps in documents (so the jump list's pruning leaves them).
pub fn marks(ed: &Editor) -> Vec<(DocId, u64)> {
    ed.session.qlist.iter().flat_map(|l| l.items.iter().filter_map(|i| i.mark)).collect()
}

impl Editor {
    /// A new list (the old one goes) — says how to walk it.
    pub fn qlist_set(&mut self, title: impl Into<String>, items: Vec<Item>) {
        let title = title.into();
        let n = items.len();
        self.qlist_put(title.clone(), items);
        if n > 0 {
            self.qlist_announce();
        }
    }

    /// How to walk the list, as a toast (`qlist_set` says it; a list set while something else has the
    /// screen says it later).
    pub fn qlist_announce(&mut self) {
        if let Some(l) = self.session.qlist.as_ref().filter(|l| !l.items.is_empty()) {
            let (title, n) = (l.title.clone(), l.items.len());
            self.set_status(format!("{title}: {n} in the list — ]q goes through them, space q shows them"));
        }
    }

    /// A new list, quietly.
    pub fn qlist_put(&mut self, title: impl Into<String>, items: Vec<Item>) {
        let title = title.into();
        let keys = items.iter().map(key).collect();
        let generation = self.session.qlist.as_ref().map_or(0, |l| l.generation) + 1;
        self.session.qlist = Some(List { title, items, current: None, keys, generation });
        self.qlist_mark_open();
    }

    /// One more item (a running task's) — false if it's there already.
    pub fn qlist_push(&mut self, mut item: Item) -> bool {
        let Some(l) = self.session.qlist.as_mut() else { return false };
        if !l.keys.insert(key(&item)) {
            return false;
        }
        if let Some(doc) =
            self.docs.iter_mut().find(|d| !d.loading && d.path.as_deref() == Some(item.path.as_path()))
        {
            let pos = byte_at(&doc.text, item.line, item.col);
            item.mark = Some((doc.id, doc.add_mark(Selection::point(pos))));
        }
        l.items.push(item);
        l.generation += 1;
        true
    }

    /// Items in open (loaded) documents without a mark get one — from their line and column as found. On
    /// making a list, opening a file, a file finishing loading.
    pub fn qlist_mark_open(&mut self) {
        let Some(l) = self.session.qlist.as_mut() else { return };
        let open: std::collections::HashMap<PathBuf, usize> = self
            .docs
            .iter()
            .enumerate()
            .filter(|(_, d)| !d.loading)
            .filter_map(|(i, d)| Some((d.path.clone()?, i)))
            .collect();
        for it in l.items.iter_mut().filter(|i| i.mark.is_none()) {
            let Some(&i) = open.get(&it.path) else { continue };
            let doc = &mut self.docs[i];
            let pos = byte_at(&doc.text, it.line, it.col);
            it.mark = Some((doc.id, doc.add_mark(Selection::point(pos))));
        }
    }

    /// Document `id` is closing: its items keep where their marks got to, as line and column (a mark again
    /// when it's reopened).
    pub fn qlist_doc_closing(&mut self, id: DocId) {
        let Some(l) = self.session.qlist.as_mut() else { return };
        let Some(doc) = self.docs.iter().find(|d| d.id == id) else { return };
        for it in l.items.iter_mut().filter(|i| i.mark.is_some_and(|(d, _)| d == id)) {
            if let Some(sel) = it.mark.and_then(|(_, m)| doc.mark(m)) {
                let head = sel.primary().head;
                it.line = doc.text.byte_to_line(head);
                it.col = head - doc.text.line_to_byte(it.line);
            }
            it.mark = None;
        }
    }

    /// `]q` / `[q` — the next / previous item (wrapping), its message as a toast.
    pub fn qlist_step(&mut self, forward: bool) {
        let Some(l) = self.session.qlist.as_ref().filter(|l| !l.items.is_empty()) else {
            return self
                .set_status("the list is empty — C-q in a picker keeps its places, space m runs a task");
        };
        let n = l.items.len();
        let i = match (l.current, forward) {
            (None, true) => 0,
            (None, false) => n - 1,
            (Some(c), true) => (c + 1) % n,
            (Some(c), false) => (c + n - 1) % n,
        };
        self.qlist_goto(i);
    }

    /// Go to item `i` — where its mark got to, or (a file not open, or still loading) its line and column.
    pub fn qlist_goto(&mut self, i: usize) {
        let Some(it) = self.session.qlist.as_ref().and_then(|l| l.items.get(i)).cloned() else { return };
        if let Some(l) = self.session.qlist.as_mut() {
            l.current = Some(i);
            l.generation += 1;
        }
        let marked = it
            .mark
            .and_then(|(d, m)| self.docs.iter().find(|x| x.id == d)?.mark(m))
            .map(|s| s.primary().head);
        let result = match marked {
            Some(pos) => {
                self.push_jump();
                self.open(&it.path).map(|_| self.doc_mut().set_selection(Selection::point(pos)))
            }
            None => self.open_at(&it.path, it.line, it.col),
        };
        if let Err(e) = result {
            return self.set_error(format!("{e:#}"));
        }
        let n = self.session.qlist.as_ref().map_or(0, |l| l.items.len());
        let msg = it.message.lines().next().unwrap_or_default().to_string();
        let text = if msg.is_empty() { format!("{}/{n}", i + 1) } else { format!("{}/{n} · {msg}", i + 1) };
        match it.severity {
            1 => self.set_error(text),
            2 => self.set_warning(text),
            _ => self.set_status(text),
        }
    }

    /// `C-q` in a picker of places — its listed rows become the list (their paths made real on a worker, so
    /// they match the documents').
    pub fn qlist_from_picker(&mut self) -> bool {
        let Some(p) = self.picker.as_ref() else { return false };
        let root = &self.session.root;
        let items: Vec<Item> = p
            .shown()
            .filter_map(|i| {
                let (path, line, col) = match &i.action {
                    Action::Goto { path, line, col } => (path.clone(), *line, *col),
                    Action::Open(path) => (path.clone(), 0, 0),
                    _ => return None,
                };
                // What the row says past its path (`src/a.rs:12: text` → `12: text` → `text`)
                let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy().into_owned();
                let rest = i.label.strip_prefix(rel.as_str()).unwrap_or(&i.label);
                let message =
                    rest.trim_start_matches(':').trim_start_matches(|c: char| c.is_ascii_digit() || c == ':');
                let severity = match i.glyph.map(|g| g.1) {
                    Some("error") => 1,
                    Some("warning") => 2,
                    Some(_) => 3,
                    None => 0,
                };
                Some(Item { path, line, col, message: message.trim().to_string(), severity, mark: None })
            })
            .collect();
        if items.is_empty() {
            self.set_status("no places here to keep");
            return false;
        }
        let title = p.title.clone();
        self.keep_last_picker();
        self.events.jobs().spawn(move || {
            let items: Vec<Item> = items
                .into_iter()
                .map(|mut i| {
                    i.path = std::fs::canonicalize(&i.path).unwrap_or(i.path);
                    i
                })
                .collect();
            move |ed: &mut Editor| ed.qlist_set(title, items)
        });
        true
    }
}

/// Byte at (line, byte column), clamped into the text and onto a character.
fn byte_at(text: &ropey::Rope, line: usize, col: usize) -> usize {
    let line = line.min(text.len_lines().saturating_sub(1));
    let start = text.line_to_byte(line);
    let end = crate::movement::line_end(text, line);
    crate::graphemes::snap(text, (start + col).min(end))
}

/// For the side panel: each item as `path:line: message` (the path relative to `root`) — the tree's hits.
pub fn labels(list: &List, root: &Path) -> Vec<(String, usize)> {
    list.items
        .iter()
        .map(|i| {
            let rel = i.path.strip_prefix(root).unwrap_or(&i.path).to_string_lossy().into_owned();
            let msg = i.message.lines().next().unwrap_or_default();
            (format!("{rel}:{}: {msg}", i.line + 1), rel.len())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn byte_positions_clamp_onto_the_text() {
        let t = ropey::Rope::from_str("ab\n타래\n");
        assert_eq!(byte_at(&t, 1, 3), 6);
        assert_eq!(byte_at(&t, 1, 99), 9, "past the line's end");
        assert_eq!(byte_at(&t, 1, 1), 3, "mid-character snaps back");
        assert_eq!(byte_at(&t, 9, 0), 10, "past the last line");
    }
}
