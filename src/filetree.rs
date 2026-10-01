//! Every list of files shows as the same folder tree — the file picker (browsing, and while typing), changed
//! files, search results, diagnostics, references, the replace preview, and the code review's rail. Pure.
//!
//! A row shows a piece of an item's own label — a folder's name is part of the path of a file under it, a
//! search hit is what follows its path — so match positions found on a label land on whichever row shows
//! that piece (`Row::source`, `Row::span`).
//!
//! Two orders. Browsing sorts each level by name, folders first, and folds. Ranked keeps the order the
//! entries came in (best match first, or the list's own order): a folder or file sits where its first entry
//! is, and every folder is open, as a heading. A folder holding nothing but one folder shares its row
//! (`snapshots/fixtures/demo/`).

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::Path;

use crate::picker::{Action, Item};

/// What a row is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A folder: `open` while browsing (it folds), None when it's a heading (ranked — always open).
    Dir { open: Option<bool> },
    /// A file — an entry itself, or the heading over its hits.
    File,
    /// One of a file's hits (a search match, a diagnostic, a reference): the label after the path.
    Hit,
    /// A section over trees (the git tab's Staged / Changes) — not made here, drawn strong.
    Section,
    /// A line of text under the row above (why it isn't fine), in its glyph's color, with no glyph — not
    /// made here either.
    Note,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub depth: usize,
    pub kind: Kind,
    /// The entry whose label this row shows a piece of, and that piece (chars of the label).
    pub source: usize,
    pub span: Range<usize>,
    /// The entry this row picks (files and hits); folders and file headings have none.
    pub item: Option<usize>,
    /// Folders: relative path of the deepest folder the row stands for (the one that folds).
    pub dir: Option<String>,
    /// Entries under it (folders, file headings).
    pub count: usize,
}

#[derive(Default)]
struct Dir {
    name: String,
    rel: String,
    dirs: Vec<Dir>,
    files: Vec<File>,
    dir_at: HashMap<String, usize>,
    file_at: HashMap<String, usize>,
    /// Order of the first entry under it, and where its name is in that entry's label.
    first: usize,
    source: usize,
    span: Range<usize>,
    count: usize,
}

struct File {
    name: String,
    first: usize,
    source: usize,
    span: Range<usize>,
    item: Option<usize>,
    hits: Vec<(usize, Range<usize>)>,
}

pub struct Tree {
    top: Dir,
}

impl Tree {
    /// `entries` = (index, label, bytes of the label that are the path), in rank order. `browse` sorts each
    /// level by name instead.
    pub fn build<'a>(entries: impl IntoIterator<Item = (usize, &'a str, usize)>, browse: bool) -> Tree {
        let mut top = Dir::default();
        for (order, (index, label, path_len)) in entries.into_iter().enumerate() {
            top.insert(order, index, label, path_len);
        }
        if browse {
            top.sort();
        }
        Tree { top }
    }

    /// Rows top to bottom. `open` = the unfolded folders while browsing; None = every folder open, as
    /// headings.
    pub fn rows(&self, open: Option<&HashSet<String>>) -> Vec<Row> {
        let mut out = Vec::new();
        emit(&self.top, 0, open, &mut out);
        out
    }
}

impl Dir {
    fn insert(&mut self, order: usize, index: usize, label: &str, path_len: usize) {
        let path = &label[..path_len];
        let segs = segments(path);
        let Some(((name, span), dirs)) = segs.split_last() else { return };
        let mut at = self;
        at.count += 1;
        for (seg, span) in dirs {
            let k = match at.dir_at.get(*seg) {
                Some(&k) => k,
                None => {
                    let rel = if at.rel.is_empty() { seg.to_string() } else { format!("{}/{seg}", at.rel) };
                    at.dir_at.insert(seg.to_string(), at.dirs.len());
                    at.dirs.push(Dir {
                        name: seg.to_string(),
                        rel,
                        first: order,
                        source: index,
                        span: span.clone(),
                        ..Dir::default()
                    });
                    at.dirs.len() - 1
                }
            };
            at = &mut at.dirs[k];
            at.count += 1;
        }
        let k = match at.file_at.get(*name) {
            Some(&k) => k,
            None => {
                at.file_at.insert(name.to_string(), at.files.len());
                at.files.push(File {
                    name: name.to_string(),
                    first: order,
                    source: index,
                    span: span.clone(),
                    item: None,
                    hits: Vec::new(),
                });
                at.files.len() - 1
            }
        };
        let file = &mut at.files[k];
        // What follows the path is a hit (`:12: text` → `12: text`); nothing = the file itself
        let rest = &label[path_len..];
        if rest.is_empty() {
            file.item = Some(index);
        } else {
            let from = span.end + usize::from(rest.starts_with(':'));
            file.hits.push((index, from..span.end + rest.chars().count()));
        }
    }

    /// Folders first, then files, each by name (case folded, so `Cargo.toml` sits with `build.rs`).
    fn sort(&mut self) {
        let key = |s: &str| (s.to_lowercase(), s.to_string());
        self.dirs.sort_by_cached_key(|d| key(&d.name));
        self.files.sort_by_cached_key(|f| key(&f.name));
        for d in &mut self.dirs {
            d.sort();
        }
    }
}

enum Kid<'a> {
    Dir(&'a Dir),
    File(&'a File),
}

fn emit(dir: &Dir, depth: usize, open: Option<&HashSet<String>>, out: &mut Vec<Row>) {
    let mut kids: Vec<Kid> = dir.dirs.iter().map(Kid::Dir).chain(dir.files.iter().map(Kid::File)).collect();
    if open.is_none() {
        kids.sort_by_key(|k| match k {
            Kid::Dir(d) => d.first,
            Kid::File(f) => f.first,
        });
    }
    for kid in kids {
        match kid {
            Kid::Dir(d) => {
                // A folder holding just one folder shares its row
                let (mut last, mut span) = (d, d.span.clone());
                while last.files.is_empty() && last.dirs.len() == 1 {
                    last = &last.dirs[0];
                    span.end = last.span.end;
                }
                let is_open = open.map(|s| s.contains(&last.rel));
                out.push(Row {
                    depth,
                    kind: Kind::Dir { open: is_open },
                    source: d.source,
                    span,
                    item: None,
                    dir: Some(last.rel.clone()),
                    count: last.count,
                });
                if is_open != Some(false) {
                    emit(last, depth + 1, open, out);
                }
            }
            Kid::File(f) => {
                out.push(Row {
                    depth,
                    kind: Kind::File,
                    source: f.source,
                    span: f.span.clone(),
                    item: f.item,
                    dir: None,
                    count: f.hits.len(),
                });
                out.extend(f.hits.iter().map(|(index, span)| Row {
                    depth: depth + 1,
                    kind: Kind::Hit,
                    source: *index,
                    span: span.clone(),
                    item: Some(*index),
                    dir: None,
                    count: 0,
                }));
            }
        }
    }
}

/// The rows enclosing row `at` (by `depth` of each row, top to bottom) — its folders, and its file when
/// it's a hit — outermost first; at most `max`, the nearest kept. What stays pinned when they scroll away.
pub fn enclosing(depth: impl Fn(usize) -> usize, at: usize, max: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut want = depth(at);
    for i in (0..at).rev() {
        if want == 0 {
            break;
        }
        if depth(i) < want {
            out.push(i);
            want = depth(i);
        }
    }
    out.truncate(max);
    out.reverse();
    out
}

/// A path's names and where each is (chars). A leading `/` stays with the first name (`/usr`).
fn segments(path: &str) -> Vec<(&str, Range<usize>)> {
    let mut out = Vec::new();
    let (mut from_b, mut from_c) = (0, 0);
    let mut chars = 0;
    for (c, (b, ch)) in path.char_indices().enumerate() {
        chars = c + 1;
        if ch == '/' && b > 0 {
            if b > from_b {
                out.push((&path[from_b..b], from_c..c));
            }
            (from_b, from_c) = (b + 1, c + 1);
        }
    }
    if from_b < path.len() {
        out.push((&path[from_b..], from_c..chars));
    }
    out
}

/// How much of an item's label is its path (relative to `root`): all of it, unless the label goes on
/// past the path of the file it opens (`src/a.rs:12: text`).
pub fn path_len(root: &Path, item: &Item) -> usize {
    let path = match &item.action {
        Action::Open(p) | Action::Goto { path: p, .. } => p,
        _ => return item.label.len(),
    };
    let rel = path.strip_prefix(root).unwrap_or(path).as_os_str().as_encoded_bytes();
    if item.label.as_bytes().starts_with(rel) { rel.len() } else { item.label.len() }
}

/// The parent folder of a row (`src/a/b.rs` → `src/a`, a top-level entry → None).
pub fn parent(rel: &str) -> Option<&str> {
    rel.rfind('/').map(|i| &rel[..i])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows as text: indent, `+`/`−` for folding folders, the piece of the label the row shows.
    fn show(labels: &[&str], rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| {
                let text: String = labels[r.source].chars().skip(r.span.start).take(r.span.len()).collect();
                let mark = match r.kind {
                    Kind::Dir { open: Some(true) } => "−",
                    Kind::Dir { open: Some(false) } => "+",
                    _ => "",
                };
                let slash = if matches!(r.kind, Kind::Dir { .. }) { "/" } else { "" };
                format!("{}{mark}{text}{slash}", "  ".repeat(r.depth))
            })
            .collect()
    }

    fn paths<'a>(labels: &'a [&'a str]) -> impl Iterator<Item = (usize, &'a str, usize)> {
        labels.iter().enumerate().map(|(i, l)| (i, *l, l.len()))
    }

    #[test]
    fn browsing_folds_and_sorts_folders_first() {
        let labels = ["b.txt", "src/main.rs", "src/ui/card.rs", "Cargo.toml", "docs/x.md"];
        let t = Tree::build(paths(&labels), true);
        let mut open = HashSet::new();
        assert_eq!(show(&labels, &t.rows(Some(&open))), ["+docs/", "+src/", "b.txt", "Cargo.toml"]);
        open.insert("src".to_string());
        let rows = t.rows(Some(&open));
        assert_eq!(show(&labels, &rows), ["+docs/", "−src/", "  +ui/", "  main.rs", "b.txt", "Cargo.toml"]);
        assert_eq!(rows[1].count, 2, "files inside");
        assert_eq!(rows[3].item, Some(1));
        assert_eq!(rows[1].item, None, "a folder picks nothing itself");
    }

    #[test]
    fn a_folder_holding_one_folder_shares_its_row() {
        let labels = ["src/snapshots/fixtures/demo/a.rs", "src/snapshots/fixtures/demo/b.rs", "src/x.rs"];
        let t = Tree::build(paths(&labels), true);
        let open: HashSet<String> =
            ["src", "src/snapshots/fixtures/demo"].into_iter().map(String::from).collect();
        let rows = t.rows(Some(&open));
        assert_eq!(
            show(&labels, &rows),
            ["−src/", "  −snapshots/fixtures/demo/", "    a.rs", "    b.rs", "  x.rs"]
        );
        assert_eq!(rows[1].dir.as_deref(), Some("src/snapshots/fixtures/demo"), "the deepest one folds");
    }

    #[test]
    fn ranked_keeps_the_entries_order_under_headings() {
        // Best match first: folders sit where their best file is
        let labels = ["src/editor.rs", "docs/edit.png", "src/snapshots/editing.txt", "src/edits.rs"];
        let rows = Tree::build(paths(&labels), false).rows(None);
        assert_eq!(
            show(&labels, &rows),
            ["src/", "  editor.rs", "  snapshots/", "    editing.txt", "  edits.rs", "docs/", "  edit.png"]
        );
        assert_eq!(rows[0].kind, Kind::Dir { open: None });
        assert_eq!(rows[0].source, 0, "a heading shows its best entry's piece — its match marks land there");
    }

    #[test]
    fn hits_hang_under_their_file() {
        let labels = ["src/a.rs:3: let x", "src/a.rs:9: x + 1", "b.rs:1: y", "/usr/lib/c.h:4"];
        let entries = labels.iter().enumerate().map(|(i, l)| {
            let path = l.find(':').unwrap();
            (i, *l, path)
        });
        let rows = Tree::build(entries, false).rows(None);
        assert_eq!(
            show(&labels, &rows),
            [
                "src/",
                "  a.rs",
                "    3: let x",
                "    9: x + 1",
                "b.rs",
                "  1: y",
                "/usr/lib/",
                "  c.h",
                "    4"
            ]
        );
        assert_eq!(rows[1].item, None, "the file heads its hits");
        assert_eq!(rows[1].count, 2);
        assert_eq!(rows[2].item, Some(0));
    }

    #[test]
    fn path_part_of_labels() {
        let root = Path::new("/r");
        let item =
            |label: &str, action| Item { label: label.into(), action, hint: String::new(), glyph: None };
        let goto = Action::Goto { path: "/r/src/a.rs".into(), line: 0, col: 0 };
        assert_eq!(path_len(root, &item("src/a.rs:1: x", goto.clone())), 8);
        assert_eq!(path_len(root, &item("src/a.rs", Action::Open("/r/src/a.rs".into()))), 8);
        assert_eq!(path_len(root, &item("1:3  unused", goto)), 11, "a label without its path");
        assert_eq!(segments("/usr/lib/c.h"), [("/usr", 0..4), ("lib", 5..8), ("c.h", 9..12)]);
        assert_eq!(parent("src/ui"), Some("src"));
        assert_eq!(parent("src"), None);
    }
}
