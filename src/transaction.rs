//! Transaction — a list of non-overlapping changes (positions in bytes). All multi-selection edits go
//! through this and apply at once; selection positions move to the new document via `map_pos`.

use ropey::Rope;
use tree_sitter::InputEdit;

use crate::selection::{Range, Selection};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub from: usize,
    pub to: usize,
    pub insert: String,
}

impl Change {
    pub fn insert(pos: usize, text: impl Into<String>) -> Self {
        Self { from: pos, to: pos, insert: text.into() }
    }

    pub fn delete(from: usize, to: usize) -> Self {
        Self { from, to, insert: String::new() }
    }
}

/// When a position meets an insertion exactly at it: stay in front (Before) or get pushed after (After).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Assoc {
    Before,
    After,
}

/// Record of one applied change — used by the syntax tree (`ts`) and LSP didChange (line/col + new text).
/// Columns are `ts` byte columns = LSP UTF-8 columns; `u16` is for UTF-16 servers (start, old end).
#[derive(Clone, Debug)]
pub struct Edit {
    pub ts: InputEdit,
    pub insert: String,
    pub u16: Option<(usize, usize)>,
}

fn u16_col(text: &Rope, byte: usize) -> usize {
    let ls = text.line_to_byte(text.byte_to_line(byte));
    crate::lsp::utf16_between(text, ls, byte)
}

#[derive(Clone, Debug, Default)]
pub struct Transaction {
    changes: Vec<Change>,
    /// Bytes gained (+) or lost (−) by the changes up to and including each one — `map_pos` finds its
    /// change by halving (a position per cursor, so thousands of cursors map in n log n, not n²).
    shift: Vec<isize>,
}

impl Transaction {
    pub fn new(mut changes: Vec<Change>) -> Self {
        #[expect(
            clippy::disallowed_methods,
            reason = "every edit's changes (one a cursor): std's sort, in place"
        )]
        changes.sort_by_key(|c| (c.from, c.to));
        let mut out: Vec<Change> = Vec::with_capacity(changes.len());
        for c in changes {
            // Overlapping changes are dropped — can't occur normally with normalized selections.
            if out.last().is_some_and(|l| c.from < l.to) {
                continue;
            }
            out.push(c);
        }
        let shift = out
            .iter()
            .scan(0, |sum, c| {
                *sum += c.insert.len() as isize - (c.to - c.from) as isize;
                Some(*sum)
            })
            .collect();
        Self { changes: out, shift }
    }

    pub fn change_by_selection(sel: &Selection, f: impl FnMut(Range) -> Option<Change>) -> Self {
        Self::new(sel.ranges().iter().copied().filter_map(f).collect())
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    /// Apply and return the edit records (each relative to the text just before it — applied back to
    /// front so earlier positions don't shift; feed them in this order to the syntax tree or LSP).
    /// `want_u16`: also count UTF-16 columns, only with a UTF-16 server attached (costs a line scan).
    pub fn apply(&self, text: &mut Rope, want_u16: bool) -> Vec<Edit> {
        // Applying back to front keeps earlier indices stable.
        // ropey 1's edit API uses char indices — the only byte↔char conversion in the whole editor.
        let mut edits = Vec::with_capacity(self.changes.len());
        for c in self.changes.iter().rev() {
            edits.push(Edit {
                ts: crate::syntax::input_edit(text, c.from, c.to, &c.insert),
                insert: c.insert.clone(),
                u16: want_u16.then(|| (u16_col(text, c.from), u16_col(text, c.to))),
            });
            let from = text.byte_to_char(c.from);
            if c.from < c.to {
                text.remove(from..text.byte_to_char(c.to));
            }
            if !c.insert.is_empty() {
                text.insert(from, &c.insert);
            }
        }
        edits
    }

    /// What each change wrote, in the text after it: byte ranges (empty where it only deleted).
    pub fn changed(&self) -> impl Iterator<Item = std::ops::Range<usize>> + '_ {
        self.changes.iter().enumerate().map(|(k, c)| {
            let start = (c.from as isize + if k == 0 { 0 } else { self.shift[k - 1] }) as usize;
            start..start + c.insert.len()
        })
    }

    pub fn map_pos(&self, pos: usize, assoc: Assoc) -> usize {
        // The changes wholly before `pos` are a prefix (sorted, apart): they shift it. An insertion right at
        // it counts as before only with `After`; one that ends at it after deleting counts always.
        let before = |c: &Change| c.to < pos || (c.to == pos && (c.from < c.to || assoc == Assoc::After));
        let k = self.changes.partition_point(before);
        let delta = if k == 0 { 0 } else { self.shift[k - 1] };
        match self.changes.get(k) {
            // pos inside the deleted range — collapses to the range start.
            Some(c) if c.from < pos => {
                let base = (c.from as isize + delta) as usize;
                if assoc == Assoc::After { base + c.insert.len() } else { base }
            }
            _ => (pos as isize + delta) as usize,
        }
    }

    pub fn map_selection(&self, sel: &Selection) -> Selection {
        sel.transform(|r| {
            Range::new(self.map_pos(r.anchor, Assoc::After), self.map_pos(r.head, Assoc::After))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_multiple_changes() {
        let mut rope = Rope::from_str("hello world");
        let tx =
            Transaction::new(vec![Change::insert(0, ">"), Change::delete(5, 6), Change::insert(11, "!")]);
        tx.apply(&mut rope, false);
        assert_eq!(rope.to_string(), ">helloworld!");
    }

    #[test]
    fn map_pos_through_insert_and_delete() {
        let tx = Transaction::new(vec![Change::insert(2, "abc"), Change::delete(5, 8)]);
        assert_eq!(tx.map_pos(1, Assoc::After), 1);
        assert_eq!(tx.map_pos(2, Assoc::Before), 2);
        assert_eq!(tx.map_pos(2, Assoc::After), 5);
        assert_eq!(tx.map_pos(4, Assoc::After), 7);
        // Inside the deleted range → range start (shifted position)
        assert_eq!(tx.map_pos(6, Assoc::After), 8);
        assert_eq!(tx.map_pos(8, Assoc::After), 8);
        assert_eq!(tx.map_pos(10, Assoc::After), 10);
    }

    /// Found by halving, mapped as walking the changes one by one would: inserts, deletes, replacements,
    /// several inserts at one spot, every position and both sides.
    #[test]
    fn map_pos_agrees_with_a_walk() {
        let walk = |tx: &Transaction, pos: usize, assoc: Assoc| {
            let mut delta: isize = 0;
            for c in &tx.changes {
                let (ins, del) = (c.insert.len() as isize, (c.to - c.from) as isize);
                if c.to < pos || (c.to == pos && (c.from < c.to || assoc == Assoc::After)) {
                    delta += ins - del;
                } else if c.from < pos {
                    let base = (c.from as isize + delta) as usize;
                    return if assoc == Assoc::After { base + ins as usize } else { base };
                } else {
                    break;
                }
            }
            (pos as isize + delta) as usize
        };
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = |n: usize| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed % n as u64) as usize
        };
        for _ in 0..300 {
            let mut changes = Vec::new();
            let mut at = 0;
            for _ in 0..next(8) {
                at += next(4);
                let del = next(3);
                changes.push(Change { from: at, to: at + del, insert: "xy"[..next(3)].into() });
                at += del;
            }
            let tx = Transaction::new(changes);
            for pos in 0..at + 3 {
                for assoc in [Assoc::Before, Assoc::After] {
                    assert_eq!(tx.map_pos(pos, assoc), walk(&tx, pos, assoc), "{pos} {assoc:?} {tx:?}");
                }
            }
        }
    }

    #[test]
    fn overlapping_changes_are_dropped() {
        let tx = Transaction::new(vec![Change::delete(0, 4), Change::delete(2, 6)]);
        assert_eq!(tx.changes.len(), 1);
    }
}
