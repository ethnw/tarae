//! Jump list — spots to come back to (`C-o` back · `C-i` forward · `C-s` save this spot). One per pane,
//! as in helix. An entry names a mark kept by its document (`Document::marks`), so it follows edits.

use std::collections::VecDeque;

use crate::document::DocId;
use crate::editor::Editor;

/// Oldest entries drop off past this many.
const CAPACITY: usize = 30;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Jump {
    pub doc: DocId,
    pub mark: u64,
}

/// A preview the next one took the place of (`Editor::open_preview`): the spots the jump lists hold in it, kept by its
/// path — `C-o` back to one opens the file again, as the preview, and its entries point at that buffer
/// (`Session::jumps_parked`, `unpark`).
#[derive(Clone, Debug)]
pub struct Parked {
    pub path: std::path::PathBuf,
    pub marks: Vec<(u64, crate::selection::Selection)>,
    /// Each mark's line and that line's text as it was parked — what `space j` lists for it (`jumplist_picker`).
    pub lines: Vec<(u64, usize, String)>,
}

/// `current` = where `C-o` steps back from; `len` means "past the newest entry" (not travelling).
#[derive(Clone, Debug, Default)]
pub struct JumpList {
    jumps: VecDeque<Jump>,
    current: usize,
}

impl JumpList {
    /// Drop what's ahead of the current point — a new jump starts a new branch (as in a browser).
    pub fn truncate(&mut self) {
        self.jumps.truncate(self.current);
    }

    pub fn last(&self) -> Option<Jump> {
        self.jumps.back().copied()
    }

    pub fn push(&mut self, jump: Jump) {
        self.truncate();
        if self.jumps.len() >= CAPACITY {
            self.jumps.pop_front();
        }
        self.jumps.push_back(jump);
        self.current = self.jumps.len();
    }

    pub fn current(&self) -> usize {
        self.current
    }

    pub fn at_end(&self) -> bool {
        self.current >= self.jumps.len()
    }

    pub fn get(&self, i: usize) -> Option<Jump> {
        self.jumps.get(i).copied()
    }

    /// Move the current point to entry `i` and return it.
    pub fn go(&mut self, i: usize) -> Option<Jump> {
        let j = self.get(i)?;
        self.current = i;
        Some(j)
    }

    pub fn len(&self) -> usize {
        self.jumps.len()
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Jump> + DoubleEndedIterator {
        self.jumps.iter()
    }

    /// Entries of `old` become `new`'s, each at the mark `to` gives it — one it gives none goes (as `remove_doc`).
    pub fn rebind(&mut self, old: DocId, new: DocId, mut to: impl FnMut(u64) -> Option<u64>) {
        for j in self.jumps.iter_mut().filter(|j| j.doc == old) {
            if let Some(mark) = to(j.mark) {
                *j = Jump { doc: new, mark };
            }
        }
        self.remove_doc(old);
    }

    /// A closed document's entries go; the current point stays on the same entry.
    pub fn remove_doc(&mut self, doc: DocId) {
        let before = self.jumps.iter().take(self.current).filter(|j| j.doc == doc).count();
        self.jumps.retain(|j| j.doc != doc);
        self.current = (self.current - before).min(self.jumps.len());
    }
}

// ── On the editor (the focused pane's list) ─────────────────────────────────

impl Editor {
    fn focused_jumps(&mut self) -> Option<&mut JumpList> {
        let focus = self.session.focus;
        self.session.views.iter_mut().find(|v| v.id == focus).map(|v| &mut v.jumps)
    }

    /// Whether jump `j` is exactly where the cursor is now.
    fn is_here(&self, j: Jump) -> bool {
        let doc = self.doc();
        j.doc == doc.id && doc.mark_raw(j.mark) == Some(doc.selection())
    }

    /// Remember the current spot before moving away (go-to, search, `C-s`).
    pub fn push_jump(&mut self) {
        let focus = self.session.focus;
        let Some(v) = self.session.views.iter().position(|v| v.id == focus) else { return };
        self.record(v, self.doc().id);
    }

    /// Document `doc`'s current selection onto pane `v`'s list — not twice in a row.
    fn record(&mut self, v: usize, doc: DocId) {
        let Some(d) = self.docs.iter_mut().find(|d| d.id == doc) else { return };
        let list = &mut self.session.views[v].jumps;
        list.truncate();
        if list.last().is_some_and(|j| j.doc == doc && d.mark_raw(j.mark) == Some(d.selection())) {
            return;
        }
        let mark = d.add_mark(d.selection().clone());
        list.push(Jump { doc, mark });
        self.prune_marks();
    }

    /// Preview `id`, about to be closed for the next one: its spots the jump lists hold, kept by its path.
    pub(crate) fn park_jumps(&mut self, id: DocId) {
        let Some(d) = self.docs.iter().find(|d| d.id == id) else { return };
        let Some(path) = d.path.clone() else { return };
        let mut marks: Vec<(u64, crate::selection::Selection)> = Vec::new();
        let mut lines = Vec::new();
        for j in self.session.views.iter().flat_map(|v| v.jumps.iter()).filter(|j| j.doc == id) {
            if !marks.iter().any(|m| m.0 == j.mark)
                && let Some(sel) = d.mark(j.mark)
            {
                let line = d.text.byte_to_line(sel.primary().cursor(&d.text));
                let text: String = d.text.line(line).chars().take(200).collect();
                lines.push((j.mark, line, text.trim().to_string()));
                marks.push((j.mark, sel));
            }
        }
        if !marks.is_empty() {
            self.session.jumps_parked.insert(id, Parked { path, marks, lines });
        }
    }

    /// Jump `j` into a preview closed since (`park_jumps`): its file opened again, as the preview, and every entry
    /// of it pointing at that buffer — the jump as it is now. None: nothing kept of it, or the file is gone (its
    /// entries go, said).
    fn unpark(&mut self, j: Jump) -> Option<Jump> {
        let p = self.session.jumps_parked.remove(&j.doc)?;
        if let Err(e) = self.open_preview_travel(&p.path) {
            for v in self.session.views.iter_mut() {
                v.jumps.remove_doc(j.doc);
            }
            self.set_status(format!("that jump's file can't be opened again: {e:#}"));
            return None;
        }
        let new = self.doc().id;
        let d = self.doc_mut();
        let map: Vec<(u64, u64)> = p.marks.into_iter().map(|(m, sel)| (m, d.add_mark(sel))).collect();
        let to = |m: u64| map.iter().find(|x| x.0 == m).map(|x| x.1);
        for v in self.session.views.iter_mut() {
            v.jumps.rebind(j.doc, new, to);
        }
        let mark = to(j.mark);
        if mark.is_none() {
            self.set_status("that jump's spot wasn't kept — its file is open again, as the preview");
        }
        Some(Jump { doc: new, mark: mark? })
    }

    /// Documents keep only the marks some pane's list (or a note) still points at — every session's panes: a
    /// session behind keeps its jumps (a jump in the one in front pruned them, and `C-o` there went nowhere).
    fn prune_marks(&mut self) {
        let mut live: std::collections::HashSet<Jump> = std::iter::once(&self.session)
            .chain(&self.sessions)
            .flat_map(|l| l.views.iter().flat_map(|v| v.jumps.iter().copied()))
            .collect();
        // (a parked preview's spots, while one of this session's lists still names it — each session its own)
        let front: Vec<DocId> =
            self.session.views.iter().flat_map(|v| v.jumps.iter().map(|j| j.doc)).collect();
        self.session.jumps_parked.retain(|id, _| front.contains(id));
        // Notes sit on marks too
        live.extend(crate::notes::marks(self).into_iter().map(|(doc, mark)| Jump { doc, mark }));
        // …and the list's items
        live.extend(crate::quickfix::marks(self).into_iter().map(|(doc, mark)| Jump { doc, mark }));
        for d in &mut self.docs {
            let id = d.id;
            d.retain_marks(|mark| live.contains(&Jump { doc: id, mark }));
        }
    }

    /// The document changed under the focused pane (opening a file, a picker, `:b` …) — the spot left
    /// behind becomes a jump, and `ga` remembers the document.
    pub fn track_doc_switch(&mut self) {
        let now = self.doc().id;
        let focus = self.session.focus;
        let Some(i) = self.session.views.iter().position(|v| v.id == focus) else { return };
        let old = self.session.views[i].doc;
        if old == now {
            return;
        }
        (self.session.views[i].doc, self.session.views[i].last_doc) = (now, Some(old));
        self.record(i, old);
    }

    /// `C-o` — `n` spots back. Leaving the newest spot records it first, so `C-i` returns there.
    pub fn jump_backward(&mut self, n: usize) {
        self.track_doc_switch();
        let Some(list) = self.focused_jumps() else { return };
        let Some(mut target) = list.current().checked_sub(n) else {
            return self.set_status("no earlier jump");
        };
        if list.at_end() {
            self.push_jump();
        }
        // An entry that is exactly here would look like nothing happened — skip over it
        let list = self.focused_jumps().cloned().unwrap_or_default();
        while target > 0 && list.get(target).is_some_and(|j| self.is_here(j)) {
            target -= 1;
        }
        self.goto_jump(target);
    }

    /// `C-i` / Tab — `n` spots forward again (after `C-o`).
    pub fn jump_forward(&mut self, n: usize) {
        let Some(list) = self.focused_jumps() else { return };
        let target = list.current() + n;
        if target >= list.len() {
            return self.set_status("no later jump");
        }
        self.goto_jump(target);
    }

    fn goto_jump(&mut self, i: usize) {
        let Some(j) = self.focused_jumps().and_then(|l| l.go(i)) else { return };
        let old = self.doc().id;
        // (into a preview the next one took the place of: opened again, as the preview)
        // (one that can't go says so: a jump that does nothing in silence looks like a broken key)
        let j = match self.docs.iter().any(|d| d.id == j.doc) {
            true => j,
            false if !self.session.jumps_parked.contains_key(&j.doc) => {
                for v in self.session.views.iter_mut() {
                    v.jumps.remove_doc(j.doc);
                }
                return self.set_status("that jump's file was closed — its jumps went with it");
            }
            false => match self.unpark(j) {
                Some(j) => j,
                None => return,
            },
        };
        let Some(di) = self.docs.iter().position(|d| d.id == j.doc) else { return };
        let Some(sel) = self.docs[di].mark(j.mark) else {
            return self.set_status("that jump's spot isn't kept any more");
        };
        self.current = di;
        self.doc_mut().set_selection(sel);
        // Switching documents here is travel, not a new jump
        let focus = self.session.focus;
        if let Some(v) = self.session.views.iter_mut().find(|v| v.id == focus)
            && v.doc != j.doc
        {
            (v.doc, v.last_doc) = (j.doc, Some(old));
        }
    }

    /// Picked in the jump list: travel there (the current spot is kept first, like `C-o` does).
    pub fn jump_to_entry(&mut self, i: usize) {
        if self.focused_jumps().is_some_and(|l| l.at_end()) {
            self.push_jump();
        }
        self.goto_jump(i);
    }

    /// `space j` — this pane's jumps, newest first: `file:line` and the line's text — a preview's the next one took
    /// the place of too, by its path as it was parked (`C-o` reaches them; picking one opens it again as the preview).
    pub fn jumplist_picker(&mut self) {
        let Some(list) = self.focused_jumps().cloned() else { return };
        let items: Vec<crate::picker::Item> = list
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(index, j)| {
                if let Some(p) = self.session.jumps_parked.get(&j.doc) {
                    let (_, line, text) = p.lines.iter().find(|l| l.0 == j.mark)?;
                    return Some(crate::picker::Item {
                        label: format!("{}:{}", self.shown(&p.path).display(), line + 1),
                        action: crate::picker::Action::JumpParked {
                            index,
                            path: p.path.clone(),
                            line: *line,
                        },
                        hint: text.clone(),
                        glyph: None,
                    });
                }
                let doc = self.docs.iter().find(|d| d.id == j.doc)?;
                let sel = doc.mark(j.mark)?;
                let line = doc.text.byte_to_line(sel.primary().cursor(&doc.text));
                let text: String = doc.text.line(line).chars().take(200).collect();
                Some(crate::picker::Item {
                    label: format!("{}:{}", doc.display_name(&self.session.root), line + 1),
                    action: crate::picker::Action::Jump { index, doc: j.doc, line },
                    hint: text.trim().to_string(),
                    glyph: None,
                })
            })
            .collect();
        if items.is_empty() {
            return self.set_status("no jumps yet (go-tos, searches, gg/ge and file switches add them)");
        }
        self.open_picker(crate::picker::Picker::of("jumps", "jumps", items, true), None);
    }

    /// `ga` — back to the document shown before this one in this pane.
    pub fn goto_last_accessed(&mut self) {
        let focus = self.session.focus;
        let last = self.session.views.iter().find(|v| v.id == focus).and_then(|v| v.last_doc);
        match last.and_then(|d| self.docs.iter().position(|x| x.id == d)) {
            Some(i) => self.current = i,
            None => self.set_status("no other file yet"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(doc: DocId, mark: u64) -> Jump {
        Jump { doc, mark }
    }

    #[test]
    fn push_truncates_the_forward_branch() {
        let mut l = JumpList::default();
        l.push(j(1, 1));
        l.push(j(1, 2));
        l.push(j(1, 3));
        // Standing on entry 1 (came back to it): a new jump replaces it and everything after
        assert_eq!(l.go(1), Some(j(1, 2)));
        l.push(j(2, 4));
        assert_eq!(l.iter().copied().collect::<Vec<_>>(), [j(1, 1), j(2, 4)]);
        assert!(l.at_end());
    }

    #[test]
    fn capacity_drops_the_oldest() {
        let mut l = JumpList::default();
        for m in 0..40 {
            l.push(j(1, m));
        }
        assert_eq!(l.len(), CAPACITY);
        assert_eq!(l.get(0), Some(j(1, 10)));
        assert_eq!(l.current(), CAPACITY);
    }

    #[test]
    fn removing_a_doc_keeps_the_current_entry() {
        let mut l = JumpList::default();
        for (d, m) in [(1, 1), (2, 2), (1, 3), (2, 4)] {
            l.push(j(d, m));
        }
        l.go(2);
        l.remove_doc(2);
        assert_eq!(l.iter().copied().collect::<Vec<_>>(), [j(1, 1), j(1, 3)]);
        assert_eq!(l.get(l.current()), Some(j(1, 3)));
        l.remove_doc(1);
        assert_eq!((l.len(), l.current()), (0, 0));
    }
}
