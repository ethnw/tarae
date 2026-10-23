//! A code view's rows: what a place that draws a file's code shows, screen row by screen row — a line of the
//! document (or one wrapped part of it), a folded doc comment's row, text that isn't the document's (a removed
//! line, a line Claude proposes), or the edit review's change header. Screen rows ≠ document lines: drawing,
//! the mouse and the moves go through these.

use std::borrow::Cow;
use std::ops::Range;

use ropey::Rope;

use crate::config::{CursorShape, LineNumber};
use crate::document::Document;
use crate::editor::{Editor, Mode};
use crate::git::{Hunk, HunkKind};
use crate::syntax::Syntax;

/// A folded doc comment row's text — shared: the edit review folds the whole file each frame from its cache.
pub type Spans = std::sync::Arc<[crate::markdown::Span]>;

/// One screen row of a code view.
#[derive(Clone, Debug)]
pub enum Row {
    /// A line of the document — `part` when it soft-wraps: which of its rows this is; `edit` when it's one of the
    /// lines a change of the edit review takes out or keeps.
    Text { line: usize, part: Option<Part>, edit: Option<Edit> },
    /// One row of a folded doc comment block (a rendered piece; `Editor::doc_block_folded` decides it).
    Doc {
        /// Document line range of the block [start, end].
        start: usize,
        end: usize,
        indent: usize,
        /// Which row within the block.
        index: usize,
        spans: Spans,
        /// Example code row (background one shade deeper).
        code: bool,
    },
    /// Text that isn't the document's, shown above line `at` (`part` when it soft-wraps, as a line does).
    Other { at: usize, text: String, kind: OtherKind, part: Option<Part> },
    /// The edit review's head of change `hunk` (its first line `at`) — drawn by the review (`draw_review`).
    Header { hunk: usize, at: usize },
}

/// What a row of text that isn't the document's is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OtherKind {
    /// A line the code review's base had here and the text no longer does (`base`: which, for the base's
    /// colours), or one a shown edit replaced.
    Gone { base: Option<usize> },
    /// A line of a change of the edit review: one it proposes — or, when the document no longer holds what the
    /// change replaces (edited while the review waits), one it takes out or keeps, from the text it was asked on.
    Edit(Edit),
    /// A line of an edit Claude is still writing (`writing.rs`) — drawn as the review will draw it, nothing
    /// applied; `head`: the line it's writing now, its last words bleeding.
    Writing { head: bool },
    /// A woven view's row that isn't a line (`weave.rs`).
    Woven(WovenRow),
}

/// A woven view's row that isn't a line: a conflict's head — a stacked part's (`part`: 0 the tip's), or the side by
/// side one's (None) —, row `index` of a side by side conflict's columns, or the head of a change that only takes
/// lines out (`Gone`: where its lines went — whose, and how many).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WovenRow {
    Head { conflict: usize, part: Option<usize> },
    Columns { conflict: usize, index: usize },
    Gone { change: usize },
}

/// A row of change `hunk` of the edit review, and what the change does to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Edit {
    pub hunk: usize,
    pub mark: Mark,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mark {
    /// A line it proposes — on the added wash while it's undecided.
    Added { washed: bool },
    /// A line it takes out.
    Taken,
    /// A line inside it that stays.
    Kept,
}

/// The rows of a line that isn't the document's at `width` cells (one, unwrapped, without a width).
pub fn other_parts(text: &str, width: Option<usize>, tab: usize) -> Vec<Option<Part>> {
    let Some(w) = width else { return vec![None] };
    parts(crate::wrap::rows(&Rope::from_str(text), 0, w, tab))
}

/// The rows of document line `line` at `width` cells (one, unwrapped, without a width).
pub fn text_parts(text: &Rope, line: usize, width: Option<usize>, tab: usize) -> Vec<Option<Part>> {
    let Some(w) = width else { return vec![None] };
    parts(crate::wrap::rows(text, line, w, tab))
}

fn parts(rows: Vec<crate::wrap::Row>) -> Vec<Option<Part>> {
    if rows.len() == 1 {
        return vec![None];
    }
    let n = rows.len();
    rows.into_iter().enumerate().map(|(index, row)| Some(Part { row, index, last: index + 1 == n })).collect()
}

/// One screen row of a soft-wrapped line (`index` from 0; `last` = the row the line ends on).
#[derive(Clone, Copy, Debug)]
pub struct Part {
    pub row: crate::wrap::Row,
    pub index: usize,
    pub last: bool,
}

/// What a place shows — the conditions its rows are built and drawn by, decided once where the place is drawn
/// (`Shows::pane`) rather than read from the editor here and there.
pub struct Shows {
    /// Has the code's keys: the cursor line's number, jump labels, the line-end diagnostic counted as shown; a
    /// pane without them is toned down.
    pub focused: bool,
    /// The document's selection is shown: its ranges and cursors, the cursor line's number and guides marked,
    /// jump labels — not in the edit review, which has no cursor.
    pub cursor: bool,
    /// `Editor::editing_in(focused)` — the fold rule's input.
    pub editing: bool,
    /// Sideways scroll (display columns).
    pub left: usize,
    /// Where the change marks come from.
    pub changes: Changes,
    pub numbers: LineNumber,
    /// The primary cursor painted as a cell (focused, not INSERT, a block cursor); without the keys it's painted
    /// as the others are.
    pub primary: bool,
    /// Its cursors are drawn at all: the code has the keys and this is its focused pane (`Editor::cursor_in` —
    /// one cursor on screen); else its selections only, and the cursor line's wash.
    pub cursors: bool,
    /// The search's hits — only on the document searched (`SearchHits` are its offsets).
    pub search: bool,
    /// The line on the cursor line's band: the cursor's (`editor.cursorline`, focused only), or a preview's item's.
    pub cursorline: Option<usize>,
    /// Line-end diagnostics (`editor.inline-diagnostics`).
    pub messages: bool,
    /// Inlay hints and indent guides — off while scrolled sideways.
    pub inlay: bool,
    pub guides: bool,
    /// The cursor line's blame at its end (focused, not INSERT).
    pub blame: bool,
    /// Claude's place, the line follow keeps in sight (`follow::place`): its line's bar — a mark, not a cursor
    /// (follow never moves yours).
    pub claude: Option<usize>,
    /// Where the sessions are in it (`presence::marks`): `▎` in the sign column on their lines, a name tag at the
    /// first line's end of where each is now — a pane's only.
    pub presence: Vec<crate::presence::Mark>,
}

/// Where a code view's change marks come from — the one source of its signs, washes, removed rows, the
/// scrollbar's marks and `]g`/`[g`: a view shows one diff.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Changes {
    /// The index's (the git gutter).
    Index,
    /// A code review's or `:diff`'s base: added lines washed, removed ones rows of their own.
    Base,
    /// An edit of Claude's that follow or replay shows: its lines washed, the ones it replaced above them.
    Shown,
    /// The edit review's: its rows carry their own marks (`Edit`) — nothing from the document's diffs.
    Edit,
    /// A woven view's (`weave.rs`): its changes and conflicts, each in its participant's colour — the conflicts'
    /// heads rows of their own.
    Woven,
}

impl Changes {
    /// Where `doc`'s marks come from: a review's or `:diff`'s base over a shown edit over the git tab's inline
    /// diff (its base the index's) over the index.
    pub fn of(doc: &Document) -> Changes {
        if doc.woven.is_some() {
            return Changes::Woven;
        }
        match (&doc.review_base, &doc.shown) {
            (Some(_), _) if doc.own_base() => Changes::Base,
            (_, Some(_)) => Changes::Shown,
            (Some(_), None) => Changes::Base,
            (None, None) => Changes::Index,
        }
    }

    /// Removed lines are rows of their own (not the index's `▁` under the line before), changed lines washed.
    pub fn rows(self) -> bool {
        !matches!(self, Changes::Index | Changes::Woven)
    }

    /// The changes, in order: the index's or the base's (`git_hunks` — the document's git base is the review's),
    /// or the shown edit's (`base` empty there: what it removed is the edit's own text).
    pub fn hunks(self, doc: &Document) -> Cow<'_, [Hunk]> {
        match (self, &doc.shown) {
            (Changes::Shown, Some(shown)) => Cow::Owned(
                shown
                    .iter()
                    .map(|h| Hunk {
                        kind: match (h.lines.is_empty(), h.removed.is_empty()) {
                            (true, _) => HunkKind::Deleted,
                            (false, true) => HunkKind::Added,
                            (false, false) => HunkKind::Modified,
                        },
                        lines: h.lines.clone(),
                        removed: h.removed.len(),
                        base: 0..0,
                    })
                    .collect(),
            ),
            (Changes::Shown, None) | (Changes::Edit, _) => Cow::Borrowed(&[]),
            (Changes::Woven, _) => {
                Cow::Owned(doc.woven.as_deref().map(crate::weave::hunks).unwrap_or_default())
            }
            _ => Cow::Borrowed(&doc.git_hunks),
        }
    }

    /// Whether `line` is on a change's wash (an added or changed line, where removed lines are rows).
    pub fn washed(self, hunks: &[Hunk], line: usize) -> bool {
        self.rows() && hunks.iter().any(|h| h.kind != HunkKind::Deleted && h.lines.contains(&line))
    }

    /// The removed lines that go above lines `from..=to`: (line they go above, text, the base's line it was).
    pub fn removed(self, doc: &Document, from: usize, to: usize) -> Vec<(usize, String, Option<usize>)> {
        let starts = |h: &Range<usize>| (from..=to).contains(&h.start);
        match (self, &doc.review_base, &doc.shown) {
            (Changes::Base, Some(base), _) => doc
                .git_hunks
                .iter()
                .filter(|h| h.removed > 0 && starts(&h.lines))
                .flat_map(|h| h.base.clone().filter(|&b| b < base.len_lines()).map(|b| (h.lines.start, b)))
                .map(|(at, b)| {
                    (at, base.line(b).to_string().trim_end_matches(['\n', '\r']).to_string(), Some(b))
                })
                .collect(),
            (Changes::Shown, _, Some(shown)) => shown
                .iter()
                .filter(|h| starts(&h.lines))
                .flat_map(|h| h.removed.iter().map(|t| (h.lines.start, t.clone(), None)))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// How many removed rows go above `line` — each soft-wrapped at `width`, as the lines are.
    pub fn rows_above(self, doc: &Document, line: usize, width: Option<usize>, tab: usize) -> usize {
        let n = match (self, &doc.shown) {
            (Changes::Base, _) => {
                doc.git_hunks.iter().filter(|h| h.lines.start == line).map(|h| h.removed).sum()
            }
            (Changes::Shown, Some(shown)) => {
                shown.iter().filter(|h| h.lines.start == line).map(|h| h.removed.len()).sum()
            }
            _ => 0,
        };
        match (n, width) {
            (0, _) | (_, None) => n,
            _ => self.removed(doc, line, line).iter().map(|r| other_parts(&r.1, width, tab).len()).sum(),
        }
    }

    /// What `near` reads: the lines of the changes it looks at (rows folded by it are kept while they're equal).
    pub fn near_key(self, doc: &Document) -> Vec<Range<usize>> {
        match (self, &doc.shown) {
            (Changes::Base, _) => doc.git_hunks.iter().map(|h| h.lines.clone()).collect(),
            (Changes::Shown, Some(shown)) => shown.iter().map(|h| h.lines.clone()).collect(),
            _ => Vec::new(),
        }
    }

    /// Whether a change shown as rows and washes is in or right next to lines `a..=b`: a changed or added line
    /// among them or on either side, or removed rows inside them or right against them.
    pub fn near(self, doc: &Document, a: usize, b: usize) -> bool {
        // (a removal's `lines` is empty: its rows go above `lines.start`)
        let near = |l: &Range<usize>| l.start <= b + 1 && l.end >= a;
        match (self, &doc.shown) {
            (Changes::Base, _) => doc.git_hunks.iter().any(|h| near(&h.lines)),
            (Changes::Shown, Some(shown)) => shown.iter().any(|h| near(&h.lines)),
            _ => false,
        }
    }
}

/// The screen rows a pane's lines take, as `term::rows_pane` lays them out — what scrolling counts
/// (`term::scroll_to`, `viewalign`): a line's wrapped rows, the removed rows above it, and for a doc
/// comment block that shows folded its rows on its first line and none on the rest (`j`/`k` step over it as one
/// line; a top inside it is its first line with rows skipped, as it's drawn).
pub struct Measure<'a> {
    editor: &'a Editor,
    doc: &'a Document,
    editing: bool,
    wrap: Option<usize>,
    cols: usize,
    tab: usize,
    changes: Changes,
    /// The doc comment blocks among `near` (none where they don't render), each with whether it shows folded once
    /// asked (`Editor::doc_block_closed` — its rows are laid out only when a line of it is counted).
    blocks: Vec<(crate::doccomment::Block, std::cell::OnceCell<bool>)>,
    /// The lines `blocks` were looked for in — elsewhere a line's block is looked for by itself.
    near: std::ops::RangeInclusive<usize>,
    /// The last block found outside `near`, with whether it shows folded: a long one (a big `//!` header) is
    /// looked for once, not for each of its lines a move counts past `near` (each ~400 lines scanned).
    far: std::cell::RefCell<Option<(crate::doccomment::Block, bool)>>,
    folding: bool,
    /// What Claude is writing here (`writing.rs`): each block's lines drawn as its rows, counted as a folded doc
    /// comment's are — its rows on its first line, none on the rest (counted once a width: `Block::counts`).
    writing: Option<&'a crate::writing::Shown>,
}

/// A document's folded doc comment blocks' row counts (by first line) for one version, code width and tab width
/// — what `Measure` counts, kept so a key doesn't lay them out again.
#[derive(Default)]
pub struct FoldCounts {
    key: (u64, usize, usize),
    rows: std::collections::HashMap<usize, usize>,
}

impl<'a> Measure<'a> {
    /// For a pane on `doc` whose code is `cols` wide (`wrap`: soft-wrapped at it), `rows` tall, its lines near
    /// `line` looked at.
    pub fn new(
        editor: &'a Editor,
        doc: &'a Document,
        editing: bool,
        (wrap, cols): (Option<usize>, usize),
        line: usize,
        rows: usize,
    ) -> Measure<'a> {
        let last = crate::movement::last_line(&doc.text);
        // (the lines the moves count: a screen up from the cursor's, one down from a top)
        let near = line.saturating_sub(rows + 2)..=(line + rows + 2).min(last);
        let folding = editor.doc_comments_render(doc.lang_name());
        // (from far enough above to join a `/** */` that starts there, as `rows_pane` looks)
        let blocks = match folding {
            true => crate::doccomment::blocks(
                &doc.text,
                near.start().saturating_sub(200)..=*near.end(),
                editor.config.tab_width,
            ),
            false => Vec::new(),
        };
        Measure {
            editor,
            doc,
            editing,
            wrap,
            cols,
            tab: editor.config.tab_width,
            changes: Changes::of(doc),
            blocks: blocks.into_iter().map(|b| (b, std::cell::OnceCell::new())).collect(),
            near,
            far: Default::default(),
            folding,
            writing: crate::writing::shown(doc),
        }
    }

    /// The block being written that holds `l`: (first line, last line, its rows — wrapped as the pane wraps).
    fn writing(&self, l: usize) -> Option<(usize, usize, usize)> {
        let b = self.writing?.at(l)?;
        let n = *b.counts(&self.doc.text, self.wrap, self.tab).last().unwrap_or(&0);
        Some((b.first, b.last, n))
    }

    /// The rows above a block's head (the line being written) inside it — where the row kept in sight is counted for a
    /// place on the block (`term::scroll_to`: follow keeps it in sight); once its text is whole, above the last row it
    /// wrote while follow leads the view there (it stays where the writing ended — never back up to its first row;
    /// your cursor's view is yours: its first row); 0 elsewhere.
    pub fn head(&self, l: usize) -> usize {
        let Some(b) = self.writing.and_then(|w| w.at(l)).filter(|b| b.first == l) else { return 0 };
        let leads = || self.doc.id == self.editor.doc().id && crate::follow::view_line(self.editor).is_some();
        let last = || b.rows.iter().rposition(|r| matches!(r, Row::Other { .. })).filter(|_| leads());
        b.head.or_else(last).map_or(0, |row| b.counts(&self.doc.text, self.wrap, self.tab)[row])
    }

    /// Every line near is one row: nothing wraps, no removed rows, no block shows folded (asked of the fold rule
    /// only — nothing laid out).
    pub fn uniform(&self) -> bool {
        let near = |b: &crate::doccomment::Block| b.end >= *self.near.start();
        self.wrap.is_none()
            && self.writing.is_none()
            && !self.changes.rows()
            // (a woven view's conflict heads and removal heads are rows of their own)
            && crate::weave::of(self.doc)
                .is_none_or(|m| m.merge.conflicts.is_empty() && m.merge.changes.iter().all(|c| !c.lines.is_empty()))
            && self
                .blocks
                .iter()
                .filter(|(b, _)| near(b))
                .all(|(b, closed)| !*closed.get_or_init(|| self.closed(b)))
    }

    fn closed(&self, b: &crate::doccomment::Block) -> bool {
        self.editor.doc_block_closed(self.doc, self.editing, (b.start, b.end))
    }

    /// A folded block's rows: laid out once for a version of the document and a width (`Document.fold_counts`),
    /// not on every key — counted without colouring its example code (a row each either way).
    fn laid(&self, b: &crate::doccomment::Block) -> usize {
        let mut counts = self.doc.fold_counts.borrow_mut();
        let key = (self.doc.version(), self.cols, self.tab);
        if counts.key != key {
            *counts = FoldCounts { key, rows: Default::default() };
        }
        *counts.rows.entry(b.start).or_insert_with(|| {
            let width = self.cols.saturating_sub(b.indent + 3).max(8);
            crate::term::fold_rows(self.editor, &crate::doccomment::parts(&b.bodies), width, None).len()
        })
    }

    /// The doc comment block holding `l` when it shows folded: (first line, last line, its rows).
    pub fn folded(&self, l: usize) -> Option<(usize, usize, usize)> {
        // (lines an edit being written replaces show as its rows, folded doc comment or not)
        if let Some(w) = self.writing(l) {
            return Some(w);
        }
        // (a woven view's conflict side by side: its head and its longest part's rows, on its first line)
        if let Some(m) = crate::weave::of(self.doc)
            && let Some(c) = m.merge.conflict_at(l).map(|i| &m.merge.conflicts[i])
            && crate::weave::columns(self.editor, self.doc, c, self.cols, self.editing)
        {
            let r = c.lines();
            return Some((r.start, r.end - 1, crate::weave::column_rows(c)));
        }
        if !self.folding {
            return None;
        }
        if self.near.contains(&l) {
            let i = self.blocks.partition_point(|(b, _)| b.end < l);
            let (b, closed) = self.blocks.get(i).filter(|(b, _)| b.start <= l)?;
            return closed.get_or_init(|| self.closed(b)).then(|| (b.start, b.end, self.laid(b)));
        }
        if let Some((b, closed)) = &*self.far.borrow()
            && (b.start..=b.end).contains(&l)
        {
            return closed.then(|| (b.start, b.end, self.laid(b)));
        }
        let (a, e) = crate::doccomment::block_at(&self.doc.text, l)?;
        let b = crate::doccomment::blocks(&self.doc.text, a..=e, self.tab).into_iter().next()?;
        let closed = self.closed(&b);
        let shown = closed.then(|| (a, e, self.laid(&b)));
        *self.far.borrow_mut() = Some((b, closed));
        shown
    }

    /// The line `l` is drawn as: a folded block's first line, else itself.
    pub fn shown_as(&self, l: usize) -> usize {
        self.folded(l).map_or(l, |(a, ..)| a)
    }

    /// `l`'s soft-wrapped rows (one whole row when it doesn't wrap).
    pub fn wrapped(&self, l: usize) -> Vec<crate::wrap::Row> {
        match self.wrap {
            Some(w) => crate::wrap::line_rows(&self.doc.text, self.doc.rows.as_deref(), l, w, self.tab),
            None => vec![crate::wrap::Row { start: 0, col: 0, end_col: usize::MAX, x0: 0 }],
        }
    }

    /// The removed rows above `l`.
    pub fn removed(&self, l: usize) -> usize {
        // (a woven view's stacked conflicts: a head above each part; a removal's head where its lines went)
        if let Some(m) = crate::weave::of(self.doc) {
            let stacked = |c: usize| {
                !crate::weave::columns(self.editor, self.doc, &m.merge.conflicts[c], self.cols, self.editing)
            };
            let gone = crate::weave::gone_at(&m.merge, l).count();
            return gone
                + crate::weave::heads_at(&m.merge, l).into_iter().filter(|&(c, _)| stacked(c)).count();
        }
        self.changes.rows_above(self.doc, l, self.wrap, self.tab)
    }

    /// The rows `l` takes: the removed ones above it, then its own.
    pub fn rows(&self, l: usize) -> usize {
        self.removed(l)
            + match self.folded(l) {
                Some((a, _, n)) if l == a => n,
                Some(_) => 0,
                None => self.wrapped(l).len(),
            }
    }

    /// The rows of `l` that can go above the screen while it's the top line: its own but the last (none under
    /// removed rows — those are shown whole).
    pub fn skippable(&self, l: usize) -> usize {
        if self.removed(l) > 0 {
            return 0;
        }
        match self.folded(l) {
            Some((a, _, n)) if l == a => n - 1,
            Some(_) => 0,
            None => self.wrapped(l).len() - 1,
        }
    }

    /// A top line as drawn: inside a folded block, its first line with the rows above `top` skipped.
    pub fn top(&self, top: usize, skip: usize) -> (usize, usize) {
        match self.folded(top) {
            Some((a, _, n)) if top > a => (a, (top - a).min(n - 1)),
            _ => (top, skip.min(self.skippable(top))),
        }
    }
}

impl Editor {
    /// Parses document `id`'s review base whole on a worker (`Document.base_syntax`) — a frame only when its
    /// removed rows are on screen. Not while its own trees are gone (`trees_follow_sight` asks again when it's
    /// shown), nor twice at once.
    pub fn base_parse(&mut self, id: crate::document::DocId) {
        let Some(d) = self.docs.iter_mut().find(|d| d.id == id) else { return };
        let syntax = d.syntax.as_ref().filter(|s| !s.dormant);
        let (Some(base), Some(lang)) = (d.review_base.clone(), syntax.map(|s| s.lang.clone())) else {
            return;
        };
        if d.base_parsing {
            return;
        }
        d.base_parsing = true;
        let seq = d.base_seq;
        self.events.jobs().spawn(move || {
            let syn = crate::syntax::Syntax::parsed(lang, &base);
            move |ed: &mut Editor| {
                let Some(d) = ed.docs.iter_mut().find(|d| d.id == id && d.base_seq == seq) else {
                    crate::event::drop_later(syn);
                    ed.unchanged = true;
                    return;
                };
                d.base_parsing = false;
                // (its trees went while it parsed: so does the base's — asked again when it's shown)
                if d.syntax.as_ref().is_none_or(|s| s.dormant) {
                    crate::event::drop_later(syn);
                    ed.unchanged = true;
                    return;
                }
                if let Some(old) = d.base_syntax.replace(syn) {
                    crate::event::drop_later(old);
                }
                // (the removed rows' lines: in a pane of the last frame?)
                let gone = |l: usize| d.git_hunks.iter().any(|h| h.removed > 0 && h.lines.start == l);
                let views = &ed.session.views;
                let seen = ed.screen.panes.iter().any(|(view, _, _, rows)| {
                    views.iter().any(|v| v.id == *view && v.doc == id) && rows.iter().any(|r| gone(r.0))
                });
                ed.unchanged = !seen;
            }
        });
    }
}

impl Shows {
    /// A pane of the editor showing `doc`.
    pub fn pane(editor: &Editor, doc: &Document, focused: bool, left: usize) -> Shows {
        let not_insert = editor.mode != Mode::Insert;
        let config = &editor.config;
        let cursors = focused && editor.cursor_in() == Some(crate::chrome::CursorIn::Code);
        Shows {
            focused,
            cursor: true,
            editing: editor.editing_in(focused),
            left,
            changes: Changes::of(doc),
            numbers: config.line_number,
            primary: focused && not_insert && crate::term::cursor_shape(editor) == CursorShape::Block,
            cursors,
            search: editor.search_hits.as_ref().is_some_and(|h| h.doc == doc.id),
            cursorline: (focused && config.cursorline).then(|| {
                let primary = crate::term::cursor_idx(editor.mode, doc.selection().primary(), &doc.text);
                crate::movement::line_of(&doc.text, primary)
            }),
            messages: config.inline_diagnostics,
            inlay: config.inlay_hints && left == 0,
            guides: config.indent_guides && left == 0,
            blame: focused && not_insert,
            claude: focused.then(|| crate::follow::place(editor, doc)).flatten(),
            presence: crate::presence::marks(editor, doc),
        }
    }

    /// Claude's edit review: the whole file as in the editor — diagnostics, indent guides, inlay hints,
    /// breakpoints on its document's rows — its change marks the edit's own (`Changes::Edit`), scrolled sideways
    /// by `left` when it doesn't wrap. Kept apart from a pane: absolute numbers and no cursor line, no selection,
    /// no search's hits, no blame — the review has no cursor (its keys answer changes, `j`/`k` scroll), and a
    /// number relative to a cursor that isn't there would read as distances from nothing.
    pub fn edit_review(editor: &Editor, left: usize) -> Shows {
        let config = &editor.config;
        Shows {
            focused: true,
            cursor: false,
            editing: false,
            left,
            changes: Changes::Edit,
            numbers: LineNumber::Absolute,
            primary: false,
            cursors: false,
            search: false,
            cursorline: None,
            messages: config.inline_diagnostics,
            inlay: config.inlay_hints && left == 0,
            guides: config.indent_guides && left == 0,
            blame: false,
            claude: None,
            presence: Vec::new(),
        }
    }

    /// A run panel's output (`rundoc.rs`): plain text on the panel's ground at full strength — its selections,
    /// search matches and cursors (painted, a cell each) only where it has the keys; no numbers, signs, guides,
    /// hints or blame. Scrolled sideways by `left` (its lines don't wrap).
    pub fn output(keys: bool, left: usize) -> Shows {
        Shows {
            focused: true,
            cursor: keys,
            editing: false,
            left,
            changes: Changes::Index,
            numbers: LineNumber::Absolute,
            primary: keys,
            cursors: keys,
            search: keys,
            cursorline: None,
            messages: false,
            inlay: false,
            guides: false,
            blame: false,
            claude: None,
            presence: Vec::new(),
        }
    }

    /// A picker's code preview: the file as the editor shows it — diagnostics, their line-end messages, signs,
    /// indent guides, its change marks as the index's are drawn (never removed lines as rows: the preview is
    /// laid out by lines from where the item opens). Kept apart from a pane: absolute numbers and no selection
    /// (no cursor there to count from); a band on `item`, the line the item opens at, even with
    /// `editor.cursorline` off — it marks where enter goes, not a cursor; no inlay hints — they're asked for
    /// the lines a pane shows, so a preview elsewhere in the file would have them on some lines and not others.
    /// Its lines always wrap (`term::rows_preview`): a preview is narrow and doesn't scroll sideways.
    pub fn preview(editor: &Editor, item: Option<usize>) -> Shows {
        let config = &editor.config;
        Shows {
            focused: true,
            cursor: false,
            editing: false,
            left: 0,
            changes: Changes::Index,
            numbers: LineNumber::Absolute,
            primary: false,
            cursors: false,
            search: false,
            cursorline: item,
            messages: config.inline_diagnostics,
            inlay: false,
            guides: config.indent_guides,
            blame: false,
            claude: None,
            presence: Vec::new(),
        }
    }
}

/// Everything `Editor::doc_block_closed` reads but the block (`Editor::closed_key`): rows it folded are kept
/// while this is equal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClosedKey {
    /// The document and its version (its text).
    pub doc: (crate::document::DocId, u64),
    pub editing: bool,
    /// Its selection's ranges, `from..to`.
    pub selection: Vec<(usize, usize)>,
    /// The changes shown (`Changes::near_key`) and the merge conflicts' lines.
    pub changes: Vec<Range<usize>>,
    pub conflicts: Vec<(usize, usize)>,
}

/// The edit review's rows, folded (`term::review_rows`), kept on its `Review` while what they're built from holds
/// — built afresh each frame they took ~200 µs on a 12k-line file (`perf_review_folded_on_a_big_file`).
pub struct Kept {
    pub key: KeptKey,
    pub rows: std::sync::Arc<Vec<Row>>,
}

/// What the edit review's rows are built from: the fold rule's inputs (the document's version among them), each
/// change's place and answer, the doc comments' layout.
#[derive(PartialEq)]
pub struct KeptKey {
    pub closed: ClosedKey,
    pub hunks: Vec<(usize, usize, crate::llm::Decision, bool)>,
    pub folds: Option<crate::doccomment::FoldsKey>,
}

/// The edit review's rows (`Review::rows`, the whole file) with each doc comment block laid out in `folds`
/// shown folded — the block's lines one after another as plain rows of their own, the rows on either side
/// plain too (a change's head or line inside it or right next to it keeps it raw: a diff is read against the
/// text it changes), and `closed` (the one rule, `Editor::doc_block_closed`) saying so.
pub fn fold(
    rows: Vec<Row>,
    folds: &crate::doccomment::Folds,
    closed: impl Fn(usize, usize) -> bool,
) -> Vec<Row> {
    let plain = |r: Option<&Row>| r.is_none_or(|r| matches!(r, Row::Text { edit: None, .. }));
    let line = |r: &Row| match r {
        Row::Text { line, edit: None, part: None } => Some(*line),
        _ => None,
    };
    let mut out = Vec::with_capacity(rows.len());
    // (rows' lines climb: the blocks are walked beside them, not looked up for each row)
    let mut next = folds.blocks.iter().peekable();
    let mut i = 0;
    while i < rows.len() {
        let block = line(&rows[i]).and_then(|a| {
            while next.next_if(|b| b.start < a).is_some() {}
            next.peek().copied().filter(|b| b.start == a)
        });
        let Some(&crate::doccomment::Folded { start: a, end: b, indent, rows: ref laid }) = block else {
            out.push(rows[i].clone());
            i += 1;
            continue;
        };
        // The block's lines, as long as they're rows of their own one after another (a change breaks them)
        let mut j = i + 1;
        while j < rows.len() && a + j - i <= b && line(&rows[j]) == Some(a + j - i) {
            j += 1;
        }
        let whole = a + j - i - 1 == b;
        if !whole
            || !plain(i.checked_sub(1).and_then(|k| rows.get(k)))
            || !plain(rows.get(j))
            || !closed(a, b)
        {
            out.extend_from_slice(&rows[i..j]);
            i = j;
            continue;
        }
        out.extend(laid.iter().enumerate().map(|(index, (spans, code))| Row::Doc {
            start: a,
            end: b,
            indent,
            index,
            spans: spans.clone(),
            code: *code,
        }));
        i = j;
    }
    out
}

/// What the drawer reads text and colours from.
pub struct Source<'a> {
    pub doc: &'a Document,
    pub text: &'a Rope,
    /// The parse to colour by — only one with a tree.
    pub syntax: Option<&'a Syntax>,
    /// The search's matches on it when they're its own (a run panel's output — `rundoc.rs`), else the editor's.
    pub hits: Option<&'a crate::search::SearchHits>,
}

impl<'a> Source<'a> {
    pub fn of(doc: &'a Document) -> Source<'a> {
        Source { doc, text: &doc.text, syntax: doc.syntax.as_ref().filter(|s| s.tree.is_some()), hits: None }
    }
}

impl Row {
    /// Document line this screen row points to (first line if folded — clicking goes there and unfolds it).
    pub fn line(&self) -> usize {
        match self {
            Row::Text { line, .. } => *line,
            Row::Doc { start, .. } => *start,
            Row::Other { at, .. } | Row::Header { at, .. } => *at,
        }
    }

    /// This row as its soft-wrapped part `p` (a line's or a text's that isn't the document's).
    pub fn with_part(&self, p: Option<Part>) -> Row {
        let mut row = self.clone();
        if let Row::Text { part, .. } | Row::Other { part, .. } = &mut row {
            *part = p;
        }
        row
    }

    /// The edit review's change this row is part of.
    pub fn hunk(&self) -> Option<usize> {
        match self {
            Row::Header { hunk, .. } => Some(*hunk),
            Row::Text { edit: Some(e), .. } | Row::Other { kind: OtherKind::Edit(e), .. } => Some(e.hunk),
            _ => None,
        }
    }

    pub fn last_line(&self) -> usize {
        match self {
            Row::Text { line, .. } => *line,
            Row::Doc { end, .. } => *end,
            // Not a line of the text: it doesn't widen the per-line tables
            Row::Other { at, .. } | Row::Header { at, .. } => at.saturating_sub(1),
        }
    }

    /// For the mouse: (doc line, display column at the row's first text cell, cells before the text,
    /// column the row stops before). A row whose text isn't the line's is one place of it, wherever it's
    /// clicked: a removed line the start of the line it went above, a folded doc comment its first line where
    /// the comment starts (it doesn't scroll sideways).
    pub fn origin(&self, left: usize) -> (usize, usize, usize, usize) {
        match self {
            Row::Text { line, part: Some(p), .. } => (*line, p.row.col, p.row.x0, p.row.end_col),
            Row::Other { at, .. } => (*at, 0, 0, 1),
            Row::Doc { start, indent, .. } => (*start, *indent, 0, indent + 1),
            _ => (self.line(), left, 0, usize::MAX),
        }
    }
}
