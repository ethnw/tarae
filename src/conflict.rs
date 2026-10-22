//! Merge conflicts in a buffer — the `<<<<<<<` / `|||||||` / `=======` / `>>>>>>>` blocks a merge or rebase
//! leaves. Found per document version (a cheap search for the marker first), drawn as tinted sides (ours on
//! the green wash, theirs on the changed one, the base faint), with a card at the one under the cursor.
//! `space g o` / `space g t` / `space g a` keep ours / theirs / both (one undo step), `]m` / `[m` go
//! between them. Pure apart from the `Editor` part at the bottom.

use ropey::Rope;

use crate::editor::Editor;

/// One conflict, by line: the markers' lines and the labels after `<<<<<<<` / `>>>>>>>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conflict {
    pub start: usize,
    /// `|||||||` (diff3 style) — the common ancestor follows it.
    pub base: Option<usize>,
    pub mid: usize,
    pub end: usize,
    pub ours: String,
    pub theirs: String,
}

/// What a line of a conflict is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    Marker,
    Ours,
    Base,
    Theirs,
}

impl Conflict {
    pub fn part(&self, line: usize) -> Option<Part> {
        if line < self.start || line > self.end {
            return None;
        }
        Some(if line == self.start || line == self.mid || line == self.end || Some(line) == self.base {
            Part::Marker
        } else if line < self.base.unwrap_or(self.mid) {
            Part::Ours
        } else if line < self.mid {
            Part::Base
        } else {
            Part::Theirs
        })
    }
}

fn marker<'a>(line: &'a str, m: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(m)?;
    (rest.is_empty() || rest.starts_with([' ', '\n', '\r'])).then(|| rest.trim())
}

/// Whether any line of `text` is a conflict's `<<<<<<<`, `|||||||` or `>>>>>>>` marker — a conflict left
/// half-resolved counts (`=======` alone doesn't: it underlines a heading too).
pub fn has_markers(text: &str) -> bool {
    text.lines().any(|l| ["<<<<<<<", "|||||||", ">>>>>>>"].iter().any(|m| marker(l, m).is_some()))
}

/// Every conflict in the text (complete ones — an unfinished block is left alone). Only marker lines are
/// turned into strings.
pub fn find(text: &Rope) -> Vec<Conflict> {
    if !contains(text, "<<<<<<<") {
        return Vec::new();
    }
    // By characters — a line may start with a multi-byte one (a byte slice there would cut it)
    let starts = |l: &ropey::RopeSlice, m: &str| l.chars().take(7).eq(m.chars());
    let mut out = Vec::new();
    let mut open: Option<Conflict> = None;
    for (i, line) in text.lines().enumerate() {
        let is =
            |m: &str| starts(&line, m).then(|| marker(&line.to_string(), m).map(str::to_string)).flatten();
        if let Some(label) = is("<<<<<<<") {
            open =
                Some(Conflict { start: i, base: None, mid: 0, end: 0, ours: label, theirs: String::new() });
        } else if let Some(c) = open.as_mut() {
            if c.mid == 0 && c.base.is_none() && is("|||||||").is_some() {
                c.base = Some(i);
            } else if c.mid == 0 && is("=======").is_some() {
                c.mid = i;
            } else if c.mid > 0
                && let Some(label) = is(">>>>>>>")
            {
                c.end = i;
                c.theirs = label;
                out.extend(open.take());
            }
        }
    }
    out
}

/// Substring search over the rope's chunks (and across their seams) — on bytes, so a chunk's end inside a
/// character doesn't matter.
fn contains(text: &Rope, needle: &str) -> bool {
    let n = needle.as_bytes();
    let finder = memchr::memmem::Finder::new(n);
    let found = |h: &[u8]| finder.find(h).is_some();
    let mut tail: Vec<u8> = Vec::new();
    for chunk in text.chunks() {
        let b = chunk.as_bytes();
        let mut seam = std::mem::take(&mut tail);
        seam.extend_from_slice(&b[..b.len().min(n.len() - 1)]);
        if found(b) || found(&seam) {
            return true;
        }
        tail = b[b.len().saturating_sub(n.len() - 1)..].to_vec();
    }
    false
}

/// Which side(s) to keep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pick {
    Ours,
    Theirs,
    Both,
}

/// The replacement for conflict `c`: (from byte, to byte, what stays).
pub fn resolve(text: &Rope, c: &Conflict, pick: Pick) -> (usize, usize, String) {
    let lines = |a: usize, b: usize| -> String {
        if a >= b {
            return String::new();
        }
        text.slice(text.line_to_char(a)..text.line_to_char(b)).to_string()
    };
    let ours = lines(c.start + 1, c.base.unwrap_or(c.mid));
    let theirs = lines(c.mid + 1, c.end);
    let keep = match pick {
        Pick::Ours => ours,
        Pick::Theirs => theirs,
        Pick::Both => ours + &theirs,
    };
    let from = text.line_to_byte(c.start);
    let to = text.line_to_byte((c.end + 1).min(text.len_lines()));
    (from, to.max(from), keep)
}

impl Editor {
    /// Every event: the conflicts of documents whose text changed, found on a worker (a big file's scan
    /// never holds a key); a result for an older version is dropped.
    pub fn conflicts_sync(&mut self) {
        for d in self.docs.iter_mut().filter(|d| !d.loading && d.conflicts_of != d.version()) {
            if d.conflicts_asked == Some(d.version()) {
                continue;
            }
            // Every file is looked at once; past 1 MB, again on edits only if it had conflicts (typing in a
            // huge file doesn't rescan it per key)
            if d.conflicts_of != u64::MAX && d.text.len_bytes() > 1 << 20 && d.conflicts.is_empty() {
                d.conflicts_of = d.version();
                continue;
            }
            let (id, version, text) = (d.id, d.version(), d.text.clone());
            // No marker (almost every file, every key): settled right here — a megabyte is looked through
            // in well under a ms, where a worker's answer was a second pass of the hooks and a second frame
            if d.conflicts.is_empty() && !contains(&text, "<<<<<<<") {
                d.conflicts_of = version;
                continue;
            }
            d.conflicts_asked = Some(version);
            // Tests wait for particular events — no extra ones from here
            if cfg!(test) {
                d.conflicts = find(&text);
                d.conflicts_of = version;
                continue;
            }
            self.events.jobs().spawn(move || {
                let found = find(&text);
                move |ed: &mut Editor| {
                    if let Some(d) = ed.docs.iter_mut().find(|d| d.id == id && d.version() == version) {
                        d.conflicts = found;
                        d.conflicts_of = version;
                    }
                }
            });
        }
    }

    /// The current document's conflicts found now (a key that acts on them can't wait for the worker).
    pub(crate) fn conflicts_now(&mut self) {
        let d = self.doc_mut();
        if d.conflicts_of != d.version() && !d.loading {
            d.conflicts = find(&d.text);
            d.conflicts_of = d.version();
        }
    }

    /// The conflict holding the cursor (index, conflict).
    pub fn conflict_here(&self) -> Option<(usize, &Conflict)> {
        let line = self.cursor_line();
        self.doc().conflicts.iter().enumerate().find(|(_, c)| (c.start..=c.end).contains(&line))
    }

    /// `space g o` / `t` / `a` — keep ours / theirs / both in the conflict under the cursor (else the next
    /// one below — never one out of sight above).
    pub fn conflict_take(&mut self, pick: Pick) {
        self.conflicts_now();
        let line = self.cursor_line();
        let doc = self.doc();
        let Some(c) = doc.conflicts.iter().find(|c| c.end >= line).cloned() else {
            let msg = if doc.conflicts.is_empty() {
                "no conflicts in this file"
            } else {
                "no conflict here or below — [m goes back to one"
            };
            return self.set_status(msg);
        };
        let (from, to, keep) = resolve(&doc.text, &c, pick);
        let n = doc.conflicts.len() - 1;
        self.with_group(|cx| {
            let tx = crate::transaction::Transaction::new(vec![crate::transaction::Change {
                from,
                to,
                insert: keep,
            }]);
            cx.editor.doc_mut().apply_with(&tx, crate::selection::Selection::point(from));
        });
        self.conflicts_now();
        let kept = match pick {
            Pick::Ours => format!("kept {}", if c.ours.is_empty() { "ours" } else { &c.ours }),
            Pick::Theirs => format!("kept {}", if c.theirs.is_empty() { "theirs" } else { &c.theirs }),
            Pick::Both => "kept both".to_string(),
        };
        if n == 0 {
            self.set_success(format!(
                "{kept} — no conflicts left here; s in the git tab (space g) marks it resolved"
            ));
        } else {
            self.set_status(format!("{kept} — {n} more (]m)"));
        }
    }

    /// `]m` / `[m` — the next / previous conflict (wrapping).
    pub fn conflict_step(&mut self, forward: bool) {
        // (a merged view's conflicts are its merge's — `merged.rs`)
        if crate::merged::step(self, forward).is_some() {
            return;
        }
        self.conflicts_now();
        let line = self.cursor_line();
        let list = &self.doc().conflicts;
        if list.is_empty() {
            return self.set_status("no conflicts in this file");
        }
        let c = if forward {
            list.iter().find(|c| c.start > line).or(list.first())
        } else {
            list.iter().rev().find(|c| c.start < line).or(list.last())
        };
        let start = c.expect("non-empty").start;
        self.push_jump();
        let doc = self.doc_mut();
        let pos = doc.text.line_to_byte(start);
        doc.set_selection(crate::selection::Selection::point(pos));
        self.align_view(crate::viewalign::Place::Center);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "a\n<<<<<<< HEAD\nours\n||||||| base\nold\n=======\ntheirs\n>>>>>>> feature\nz\n";

    #[test]
    fn finds_and_resolves() {
        let t = Rope::from_str(SRC);
        let c = find(&t);
        assert_eq!(
            c,
            [Conflict {
                start: 1,
                base: Some(3),
                mid: 5,
                end: 7,
                ours: "HEAD".into(),
                theirs: "feature".into()
            }]
        );
        let parts: Vec<_> = (0..9).map(|l| c[0].part(l)).collect();
        assert_eq!(parts[2], Some(Part::Ours));
        assert_eq!(parts[4], Some(Part::Base));
        assert_eq!(parts[6], Some(Part::Theirs));
        assert_eq!((parts[1], parts[8]), (Some(Part::Marker), None));
        let apply = |p| {
            let (a, b, s) = resolve(&t, &c[0], p);
            let mut x = SRC.to_string();
            x.replace_range(a..b, &s);
            x
        };
        assert_eq!(apply(Pick::Ours), "a\nours\nz\n");
        assert_eq!(apply(Pick::Theirs), "a\ntheirs\nz\n");
        assert_eq!(apply(Pick::Both), "a\nours\ntheirs\nz\n");
        assert!(find(&Rope::from_str("<<<<<<< x\nunfinished\n")).is_empty());
        assert!(find(&Rope::from_str("no markers\n")).is_empty());
        // Emoji where a byte count would cut (it once panicked: "abcdef" + 👍 at bytes 6..10)
        assert!(find(&Rope::from_str("abcdef👍 x\n")).is_empty());
        let long: String = "👍".repeat(3000) + "\n<<<<<<< a\nx\n=======\ny\n>>>>>>> b\n";
        assert_eq!(find(&Rope::from_str(&long)).len(), 1, "a marker after many chunks of emoji");
    }
}
