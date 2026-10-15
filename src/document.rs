//! Document = text (rope) + selection + path + undo history.
//!
//! Undo is snapshot-based. Cloning a ropey rope is O(1) structural sharing, so pushing the whole
//! (text, selection, version) per edit group is cheap. Only when writing to disk (to survive restarts)
//! are neighboring snapshots turned into diffs (`undofile.rs`).

use std::fs::File;
use std::io::{self, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result, bail};
use ropey::Rope;

use crate::lsp::{ClientId, Diagnostic};
use crate::selection::{Range, Selection};
use crate::syntax::Syntax;
use crate::transaction::{Assoc, Edit, Transaction};

/// One attached server's side of a document: what it hasn't been sent yet — each server has its own, since
/// each gets ready on its own time and may count columns differently.
pub struct Link {
    pub client: ClientId,
    /// Changes not yet sent (as didChange once the server is ready / at the end of an event batch).
    pub changes: Vec<Edit>,
    /// Send the full text next time instead of changes (after undo etc.).
    pub full_sync: bool,
    /// LSP document version — only increases, even on undo (separate from the editor version).
    pub version: i32,
    /// The server uses UTF-16 columns.
    pub u16: bool,
    /// Told the document and every change, asked nothing — a swap's new server until it takes over
    /// (`share.rs`).
    pub shadow: bool,
}

impl Link {
    /// Something is waiting to be sent.
    pub fn behind(&self) -> bool {
        !self.changes.is_empty() || self.full_sync
    }
}

/// LSP-side state of a document.
#[derive(Default)]
pub struct DocLsp {
    /// The main server — the one asked first (a feature it lacks or `except`s goes to one beside it).
    pub client: Option<ClientId>,
    /// Every attached server — the main one and those beside it (`[lang] also`, ruff next to pyright): every
    /// change goes to all, their diagnostics join one list.
    pub links: Vec<Link>,
    pub diagnostics: Vec<Diagnostic>,
    /// Inlay hints (by position) + the (doc version, line range) they're for — covers the visible range
    /// and the version matches → don't ask again.
    pub inlay: Vec<crate::lsp::InlayHint>,
    pub inlay_have: Option<(u64, (usize, usize))>,
}

impl DocLsp {
    /// Every server attached, the main one first (a shadow isn't — it's asked nothing).
    pub fn clients(&self) -> impl Iterator<Item = ClientId> + '_ {
        let main = self.client;
        let plain = self.links.iter().filter(|l| !l.shadow).map(|l| l.client);
        main.into_iter().chain(plain.filter(move |&c| Some(c) != main))
    }

    /// The shadows: servers told the document, asked nothing yet.
    pub fn shadows(&self) -> impl Iterator<Item = ClientId> + '_ {
        self.links.iter().filter(|l| l.shadow).map(|l| l.client)
    }

    /// Server `cid` is told the document and its changes, asked nothing (a swap's new server).
    pub fn attach_shadow(&mut self, cid: ClientId, u16: bool) {
        self.links.retain(|l| l.client != cid);
        self.links.push(Link {
            client: cid,
            changes: Vec::new(),
            full_sync: false,
            version: 0,
            u16,
            shadow: true,
        });
    }

    /// `to` takes over from `from`: its link a plain one (kept where it was — its versions go on), the main
    /// one if `from` was; `from`'s link gone.
    pub fn take_over(&mut self, from: ClientId, to: ClientId) {
        if let Some(l) = self.links.iter_mut().find(|l| l.client == to) {
            l.shadow = false;
        }
        self.links.retain(|l| l.client != from);
        if self.client == Some(from) {
            self.client = Some(to);
        }
    }

    /// The servers beside the main one.
    pub fn beside(&self) -> impl Iterator<Item = ClientId> + '_ {
        self.clients().filter(|&c| Some(c) != self.client)
    }

    pub fn link(&self, cid: ClientId) -> Option<&Link> {
        self.links.iter().find(|l| l.client == cid)
    }

    /// Server `cid` is attached (as the main one, or beside it) — its queue starts empty at version 0.
    pub fn attach(&mut self, cid: ClientId, u16: bool, main: bool) {
        self.links.retain(|l| l.client != cid);
        self.links.push(Link {
            client: cid,
            changes: Vec::new(),
            full_sync: false,
            version: 0,
            u16,
            shadow: false,
        });
        if main {
            self.client = Some(cid);
        }
    }

    /// Server `cid` is gone.
    pub fn detach(&mut self, cid: ClientId) {
        self.links.retain(|l| l.client != cid);
        if self.client == Some(cid) {
            self.client = None;
        }
    }
}

pub type DocId = u64;

/// File → rope. A missing file gives an empty rope (created on save).
pub fn read_file(path: &Path) -> Result<Rope> {
    match File::open(path) {
        Ok(f) => Rope::from_reader(BufReader::new(f)).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Rope::new()),
        Err(e) => Err(e).with_context(|| format!("opening {}", path.display())),
    }
}

/// An undo step: the text, selection and version as they were — no parse tree (#36). Snapshots used to keep
/// theirs so an undo was colored at once, and that cost an hour's work hundreds of MB: injected layers (every
/// comment, every macro's token tree as Rust) are parsed anew and share nothing between versions, and even
/// the main tree's reparse rebuilds the lists an edit sits in (a third of a 1,200-line file's nodes, every
/// edit). An undo now edits the current tree into the restored text instead (`restore`), as typing does.
#[derive(Clone)]
pub struct Snapshot {
    text: Rope,
    selection: Selection,
    version: u64,
}

pub struct Document {
    pub id: DocId,
    pub text: Rope,
    pub path: Option<PathBuf>,
    /// View scroll (line, screen column) — kept when switching buffers.
    pub top: usize,
    pub left: usize,
    /// `(line, n)`: while `top` is that line, its first `n` wrapped rows are above the screen — a wrapped
    /// line scrolls by rows, so one taller than half the screen isn't jumped past (#233). Another `top`: none.
    pub top_rows: Option<(usize, usize)>,
    /// Loading a big file in the background — text still empty. No saving (would overwrite with nothing).
    pub loading: bool,
    /// Syntax tree and language (None if the language is unknown).
    pub syntax: Option<Syntax>,
    /// Attached language servers, pending changes, diagnostics.
    pub lsp: DocLsp,
    selection: Selection,
    /// Version of the current text. A new edit always gets a new number; undo restores an old one
    /// — so `version == saved_version` means exactly "matches disk".
    version: u64,
    saved_version: u64,
    next_version: u64,
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// Fingerprint of the disk file last read or written — detects changes made elsewhere (disk.rs).
    pub disk: Option<crate::disk::Stamp>,
    /// Disk changed too while I was editing (resolve with `:reload` / `:w!`).
    pub disk_conflict: bool,
    /// Claude was told to write this buffer's text (an accepted edit): until the write lands or this time
    /// passes, auto-save leaves the file to Claude — saving first fails its edit ("changed since read").
    /// (until, the buffer's version Claude writes)
    pub agent_writes: Option<(std::time::Instant, u64)>,
    /// A new file of Claude's you accepted, and the project it was accepted in: tarae's own save of it makes
    /// its missing folders — inside that project only (`saver::make_folders`). Claude's write makes them
    /// too, but one that never comes (its turn stopped) left every save failing "No such file or directory".
    pub make_folders: Option<std::path::PathBuf>,
    /// Saves of this buffer the saver thread hasn't finished (`saver.rs`) — the disk changing meanwhile is
    /// that write, not someone else's.
    pub saving: u32,
    /// This file at HEAD (None outside a repo) · changed lines against it · version that diff is for.
    pub git_base: Option<std::sync::Arc<String>>,
    pub git_hunks: Vec<crate::git::Hunk>,
    /// In a code review: the base being compared against, for showing removed lines (`codereview.rs`).
    pub review_base: Option<ropey::Rope>,
    /// The file's indent unit, for (version, tab width) — `commands::indent_unit`.
    pub indent_unit: std::cell::Cell<Option<(u64, usize, crate::indent::Unit)>>,
    /// `:diff` — what it's compared with (diffview.rs; `review_base` holds that text).
    pub diff_of: Option<String>,
    /// Merge conflicts in the text (conflict.rs) · the version they were found for.
    pub conflicts: Vec<crate::conflict::Conflict>,
    pub conflicts_of: u64,
    /// A scan for this version is out (on a worker).
    pub conflicts_asked: Option<u64>,
    pub git_version: u64,
    /// Display name of an unnamed buffer (tutorial etc.) · disposable (quit isn't blocked as "unsaved").
    pub title: Option<String>,
    pub throwaway: bool,
    /// Soft wrap for this buffer alone (`z w`, `:soft-wrap`) — None: `editor.soft-wrap` decides (#156).
    pub wrap: Option<bool>,
    /// Address of non-file content (jdtls jdt:// class source) — reopening uses this buffer. Read-only.
    pub virtual_uri: Option<String>,
    /// Spots the jump lists point at (id → selection) — they follow edits like diagnostics do.
    marks: Vec<(u64, Selection)>,
    next_mark: u64,
    /// When it was last on screen (None: not yet) — only the recently seen keep their parse trees
    /// (`Editor::trees_follow_sight`).
    pub seen: Option<std::time::Instant>,
    /// Opened by Claude's turn outside the project (an edit's review) and not worked in by the user since:
    /// it closes when the turn ends, and `:restart` doesn't bring it back (#37 — such tabs stayed, each
    /// root keeping a language server).
    pub aside: bool,
    /// Opened by Claude's turn — follow or replay showing what it read or wrote — and not taken up by the user
    /// since (`edits::taken_up`): nobody asked to work in it, so no grammar or server is offered for it until
    /// then (#208: an install card for a `.sh` Claude wrote came up mid-turn, in front of the turn's own).
    pub claudes_only: bool,
    /// Its grammar's offer held back while `claudes_only` (`load_language`): the language — offered when the
    /// user takes it up (`edits::taken_up`).
    pub grammar_held: Option<&'static crate::syntax::LangSpec>,
    /// An edit of Claude's that follow or replay shows here (`follow::sync_shown` — only while the buffer is
    /// the version it was seen in): its lines on the review's wash, the ones they replaced above them.
    pub shown: Option<Vec<crate::replay::Hunk>>,
}

impl Document {
    /// The view starts at `line`, from its first row.
    pub fn set_top(&mut self, line: usize) {
        self.top = line;
        self.top_rows = None;
    }

    /// The rows of the top line above the screen (`top_rows`).
    pub fn top_skip(&self) -> usize {
        self.top_rows.filter(|&(l, _)| l == self.top).map_or(0, |(_, n)| n)
    }

    pub fn from_str(id: DocId, text: &str) -> Self {
        Self {
            id,
            text: Rope::from_str(text),
            path: None,
            top: 0,
            left: 0,
            top_rows: None,
            loading: false,
            syntax: None,
            lsp: DocLsp::default(),
            selection: Selection::point(0),
            version: 0,
            saved_version: 0,
            next_version: 0,
            undo: Vec::new(),
            redo: Vec::new(),
            disk: None,
            disk_conflict: false,
            agent_writes: None,
            make_folders: None,
            saving: 0,
            git_base: None,
            git_hunks: Vec::new(),
            review_base: None,
            indent_unit: std::cell::Cell::new(None),
            diff_of: None,
            conflicts: Vec::new(),
            conflicts_of: u64::MAX,
            conflicts_asked: None,
            git_version: u64::MAX,
            title: None,
            throwaway: false,
            wrap: None,
            virtual_uri: None,
            marks: Vec::new(),
            next_mark: 0,
            seen: None,
            aside: false,
            claudes_only: false,
            grammar_held: None,
            shown: None,
        }
    }

    /// A missing file gives an empty document at that path (created on save).
    pub fn open(id: DocId, path: &Path) -> Result<Self> {
        let mut doc = Self::placeholder(id, path);
        doc.disk = crate::disk::stamp(path);
        doc.text = read_file(path)?;
        doc.loading = false;
        Ok(doc)
    }

    /// Mark the current content as matching disk (after reloading).
    pub fn mark_saved(&mut self) {
        self.saved_version = self.version;
    }

    /// Empty placeholder for background loading — `finish_loading` fills it.
    pub fn placeholder(id: DocId, path: &Path) -> Self {
        let mut doc = Self::from_str(id, "");
        doc.path = Some(path.to_path_buf());
        doc.loading = true;
        doc
    }

    /// `disk` is set by the loader to the stamp taken *before* reading — a change made during the read
    /// then shows up on the next watch round.
    pub fn finish_loading(&mut self, text: Rope) {
        self.text = text;
        self.loading = false;
        if let Some(s) = &mut self.syntax {
            s.reset();
        }
        let sel = self.selection.clone();
        self.set_selection(sel);
    }

    pub fn selection(&self) -> &Selection {
        &self.selection
    }

    pub fn set_selection(&mut self, sel: Selection) {
        self.selection = sel.clamp(self.text.len_bytes());
    }

    /// Change the text and move selections along with the changes.
    pub fn apply(&mut self, tx: &Transaction) {
        let sel = tx.map_selection(&self.selection);
        self.apply_with(tx, sel);
    }

    /// Change the text; the selection is whatever the caller decided.
    pub fn apply_with(&mut self, tx: &Transaction, sel: Selection) {
        if tx.is_empty() {
            self.set_selection(sel);
            return;
        }
        let u16 = self.lsp.links.iter().any(|l| l.u16);
        let edits = tx.apply(&mut self.text, u16);
        if let Some(s) = &mut self.syntax {
            s.edit(&edits.iter().map(|e| e.ts).collect::<Vec<_>>());
        }
        for l in &mut self.lsp.links {
            l.changes.extend(edits.iter().cloned());
        }
        // Diagnostic ranges follow edits too — stay in place until the server sends new ones.
        for d in &mut self.lsp.diagnostics {
            d.from = tx.map_pos(d.from, Assoc::Before);
            d.to = tx.map_pos(d.to, Assoc::Before).max(d.from);
        }
        // Hints stick to the following character (typing before it pushes them along)
        for h in &mut self.lsp.inlay {
            h.pos = tx.map_pos(h.pos, Assoc::After);
        }
        for (_, m) in &mut self.marks {
            *m = tx.map_selection(m);
        }
        self.set_selection(sel);
        self.next_version += 1;
        self.version = self.next_version;
    }

    pub fn version(&self) -> u64 {
        self.version
    }

    /// Keep `sel` as a mark (it follows edits) — returns its id.
    pub fn add_mark(&mut self, sel: Selection) -> u64 {
        self.next_mark += 1;
        self.marks.push((self.next_mark, sel));
        self.next_mark
    }

    /// Mark `id` as stored (compare with the current selection).
    pub fn mark_raw(&self, id: u64) -> Option<&Selection> {
        self.marks.iter().find(|(m, _)| *m == id).map(|(_, s)| s)
    }

    /// Mark `id` on the current text (undo replaces the text wholesale — ends snap to a char start).
    pub fn mark(&self, id: u64) -> Option<Selection> {
        let snap = |p| crate::graphemes::snap(&self.text, p);
        self.mark_raw(id).map(|s| s.transform(|r| Range::new(snap(r.anchor), snap(r.head))))
    }

    /// Every mark's head (as stored) — for looking many up at once.
    pub fn mark_heads(&self) -> impl Iterator<Item = (u64, usize)> + '_ {
        self.marks.iter().map(|(m, s)| (*m, s.primary().head))
    }

    /// Drop marks nothing points at any more.
    pub fn retain_marks(&mut self, keep: impl Fn(u64) -> bool) {
        self.marks.retain(|(m, _)| keep(*m));
    }

    /// [error, warning] counts — for the status line.
    pub fn diagnostic_counts(&self) -> [usize; 2] {
        let n = |s: u8| self.lsp.diagnostics.iter().filter(|d| d.severity == s).count();
        [n(1), n(2)]
    }

    pub fn is_modified(&self) -> bool {
        self.version != self.saved_version
    }

    /// The text as on disk (last read or saved), while the undo history still holds it — None once it
    /// doesn't (a rope clone, sharing what didn't change).
    pub fn saved_text(&self) -> Option<Rope> {
        if !self.is_modified() {
            return Some(self.text.clone());
        }
        self.undo.iter().chain(&self.redo).find(|s| s.version == self.saved_version).map(|s| s.text.clone())
    }

    /// The parse trees this document keeps alive, as distinct subtrees: (main tree's, injected layers'). Its
    /// undo history keeps none (#36) — this is its current trees, for tests that hold memory to a bound.
    #[cfg(test)]
    pub fn tree_nodes(&self) -> (usize, usize) {
        use crate::syntax::count_subtrees;
        let (mut main, mut layers) = (Default::default(), Default::default());
        // (all held through the count — `count_subtrees`)
        let trees: Vec<crate::syntax::Trees> =
            self.syntax.as_ref().and_then(|s| s.trees()).into_iter().collect();
        for (tree, ls) in &trees {
            count_subtrees(tree, &mut main);
            for l in ls.iter() {
                count_subtrees(&l.tree, &mut layers);
            }
        }
        (main.len(), layers.len())
    }

    pub fn snapshot(&self) -> Snapshot {
        Snapshot { text: self.text.clone(), selection: self.selection.clone(), version: self.version }
    }

    /// Close the edit group — if now differs from the group-start snapshot, push onto the undo stack.
    pub fn commit_undo(&mut self, before: Snapshot) {
        if before.version != self.version {
            self.undo.push(before);
            self.redo.clear();
        }
    }

    pub fn undo(&mut self) -> bool {
        let Some(snap) = self.undo.pop() else { return false };
        let cur = self.snapshot();
        self.redo.push(cur);
        self.restore(snap);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(snap) = self.redo.pop() else { return false };
        let cur = self.snapshot();
        self.undo.push(cur);
        self.restore(snap);
        true
    }

    /// Persistent undo: (text, selection) of (undo stack, redo stack) — in stack order (last = nearest).
    pub fn history(&self) -> crate::undofile::Stacks {
        let f = |v: &Vec<Snapshot>| v.iter().map(|s| (s.text.clone(), s.selection.clone())).collect();
        (f(&self.undo), f(&self.redo))
    }

    /// Install restored history right after opening (current text = saved state). Versions renumbered.
    pub fn install_history(&mut self, (undo, redo): crate::undofile::Stacks) {
        let mut snap = |(text, selection): (Rope, Selection)| {
            self.next_version += 1;
            Snapshot { text, selection, version: self.next_version }
        };
        let undo: Vec<Snapshot> = undo.into_iter().map(&mut snap).collect();
        let redo: Vec<Snapshot> = redo.into_iter().map(&mut snap).collect();
        (self.undo, self.redo) = (undo, redo);
    }

    pub fn has_history(&self) -> bool {
        !self.undo.is_empty() || !self.redo.is_empty()
    }

    fn restore(&mut self, snap: Snapshot) {
        // The trees follow the text as one edit (the span between what's common at its ends) — what didn't
        // change keeps its colors at once, the rest within a reparse, from the tree as it is: nothing kept
        // per undo step (#36). (A parse under way gets the edit too, as for a key.)
        let edit = crate::disk::minimal_change(&self.text, &snap.text)
            .map(|c| crate::syntax::input_edit(&self.text, c.from, c.to, &c.insert));
        self.text = snap.text;
        self.selection = snap.selection;
        self.version = snap.version;
        if let (Some(s), Some(e)) = (&mut self.syntax, edit) {
            s.edit(&[e]);
        }
        // Text replaced wholesale → resend everything to the server.
        for l in &mut self.lsp.links {
            l.changes.clear();
            l.full_sync = true;
        }
        self.lsp.diagnostics.clear();
        self.lsp.inlay.clear();
        self.lsp.inlay_have = None;
    }

    pub fn save(&mut self) -> Result<()> {
        self.save_as(false)
    }

    /// Unless `force`, don't overwrite a file changed on disk since read (don't clobber others' changes).
    /// Writes here, on this thread — `Editor::save_in_background` is the usual way; callers of this one wait
    /// for background saves first (`Editor::flush_saves`) so an older one can't land after it.
    pub fn save_as(&mut self, force: bool) -> Result<()> {
        let path = self.check_save(force)?;
        crate::disk::write_atomic(&path, |w| self.text.write_to(w))
            .with_context(|| format!("writing {}", path.display()))?;
        let (version, stamp) = (self.version, crate::disk::stamp(&path));
        self.saved_at(version, stamp);
        Ok(())
    }

    /// Whether this buffer may be written to its file now — the path to write. Unless `force`, not over a
    /// file changed on disk since read (don't clobber others' changes).
    pub fn check_save(&mut self, force: bool) -> Result<std::path::PathBuf> {
        if self.loading {
            bail!("still loading — not saved");
        }
        let Some(path) = &self.path else { bail!("no file name — use :w <path>") };
        let now = crate::disk::stamp(path);
        // (a save of ours still being written changes the file too — that's not someone else)
        let changed = self.saving == 0 && self.disk.is_some() && now.is_some() && now != self.disk;
        // Written outside with my buffer's text (Claude saving an edit I accepted, maybe without the last
        // newline) — before the watch has seen it. Not a conflict: this save just finishes it.
        let mine = || {
            let Ok(body) = std::fs::read_to_string(path) else { return false };
            self.text == body.as_str()
                || crate::disk::is_plus_newline(&self.text, ropey::RopeSlice::from(body.as_str()))
        };
        if !force && (self.disk_conflict || (changed && !mine())) {
            self.disk_conflict = true;
            bail!(
                "{} changed on disk since it was read — :w! to overwrite, :reload to take it",
                self.display_name_short()
            );
        }
        Ok(path.clone())
    }

    /// Version `version` of the text is on disk, now with fingerprint `stamp`.
    pub fn saved_at(&mut self, version: u64, stamp: Option<crate::disk::Stamp>) {
        self.saved_version = version;
        self.disk = stamp;
        self.disk_conflict = false;
    }

    /// Its language's name — by its grammar, else by its path (also when the grammar isn't there).
    pub fn lang_name(&self) -> Option<&str> {
        self.syntax
            .as_ref()
            .map(|s| s.lang.name.as_str())
            .or_else(|| self.path.as_deref().and_then(crate::syntax::detect).map(|s| s.name.as_str()))
    }

    /// Its path relative to `root` (the editor's), else its title.
    pub fn display_name(&self, root: &Path) -> String {
        match &self.path {
            Some(p) => p.strip_prefix(root).unwrap_or(p).display().to_string(),
            None => self.title.clone().unwrap_or_else(|| "[scratch]".to_string()),
        }
    }

    /// File name only (for header and buffer list).
    pub fn display_name_short(&self) -> String {
        match self.path.as_ref().and_then(|p| p.file_name()) {
            Some(n) => n.to_string_lossy().into_owned(),
            None => self.title.clone().unwrap_or_else(|| "[scratch]".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::selection::Range;
    use crate::transaction::Change;

    fn edit(doc: &mut Document, change: Change) {
        let before = doc.snapshot();
        doc.apply(&Transaction::new(vec![change]));
        doc.commit_undo(before);
    }

    fn temp_dir(name: &str) -> PathBuf {
        crate::scratch::Scratch::new(&format!("doc-{name}")).into_path()
    }

    #[test]
    fn undo_redo_walk_snapshots_and_a_new_edit_drops_redo() {
        let mut doc = Document::from_str(0, "ab");
        edit(&mut doc, Change::insert(2, "c"));
        edit(&mut doc, Change::insert(3, "d"));
        assert_eq!(doc.text.to_string(), "abcd");
        assert!(doc.undo() && doc.undo());
        assert_eq!(doc.text.to_string(), "ab");
        assert!(!doc.undo(), "nothing left to undo");
        assert!(doc.redo());
        assert_eq!(doc.text.to_string(), "abc");
        // A new edit after undo forgets what could be redone
        edit(&mut doc, Change::insert(0, "x"));
        assert!(!doc.redo());
        assert_eq!(doc.text.to_string(), "xabc");
    }

    #[test]
    fn a_group_that_changed_nothing_is_not_an_undo_step() {
        let mut doc = Document::from_str(0, "ab");
        let before = doc.snapshot();
        doc.apply(&Transaction::new(vec![]));
        doc.set_selection(Selection::point(1));
        doc.commit_undo(before);
        assert!(!doc.has_history());
        assert_eq!(doc.selection().primary().head, 1, "an empty transaction still sets the selection");
    }

    #[test]
    fn modified_means_differs_from_the_saved_version() {
        let dir = temp_dir("modified");
        let mut doc = Document::open(0, &dir.join("a.txt")).unwrap();
        edit(&mut doc, Change::insert(0, "one"));
        doc.save().unwrap();
        assert!(!doc.is_modified());
        doc.undo();
        assert!(doc.is_modified(), "undo past the save differs from disk");
        doc.redo();
        assert!(!doc.is_modified(), "redo lands exactly on the saved version");
        // Undo, then a different edit: never mistaken for the saved version, even at the same depth
        doc.undo();
        edit(&mut doc, Change::insert(0, "two"));
        assert!(doc.is_modified());
        assert_ne!(doc.version(), 0);
    }

    #[test]
    fn edits_move_diagnostics_hints_and_selection_and_queue_lsp_changes() {
        let mut doc = Document::from_str(0, "let x = 1;\n");
        doc.set_selection(Selection::point(4));
        doc.lsp.diagnostics.push(crate::lsp::Diagnostic {
            from: 4,
            to: 5,
            severity: 1,
            message: "unused".into(),
            raw: serde_json::Value::Null,
            source: 0,
        });
        doc.lsp.inlay.push(crate::lsp::InlayHint { pos: 5, text: ": i32".into() });
        // Not attached: nothing is queued for a server
        edit(&mut doc, Change::insert(0, "  "));
        assert!(doc.lsp.links.is_empty());
        assert_eq!((doc.lsp.diagnostics[0].from, doc.lsp.diagnostics[0].to), (6, 7));
        assert_eq!(doc.lsp.inlay[0].pos, 7);
        assert_eq!(doc.selection().primary().head, 6);
        // Attached: the edit is queued; typing right at a hint pushes it along (it sticks to what follows)
        doc.lsp.attach(1, false, true);
        doc.lsp.attach(2, false, false);
        edit(&mut doc, Change::insert(7, "y"));
        assert!(doc.lsp.links.iter().all(|l| l.changes.len() == 1), "queued for each server");
        assert_eq!(doc.lsp.inlay[0].pos, 8);
        // A diagnostic whose whole range is deleted collapses instead of inverting
        edit(&mut doc, Change::delete(6, 8));
        assert!(doc.lsp.diagnostics[0].from <= doc.lsp.diagnostics[0].to);
    }

    #[test]
    fn undo_resets_server_state_for_a_full_resync() {
        let mut doc = Document::from_str(0, "a");
        doc.lsp.attach(1, false, true);
        edit(&mut doc, Change::insert(1, "b"));
        doc.lsp.inlay.push(crate::lsp::InlayHint { pos: 0, text: "h".into() });
        doc.lsp.inlay_have = Some((doc.version(), (0, 1)));
        doc.undo();
        assert!(doc.lsp.links.iter().all(|l| l.full_sync && l.changes.is_empty()));
        assert!(doc.lsp.inlay.is_empty() && doc.lsp.inlay_have.is_none());
    }

    #[test]
    fn selection_is_clamped_to_the_text() {
        let mut doc = Document::from_str(0, "abc");
        doc.set_selection(Selection::single(Range::new(1, 99)));
        assert_eq!(doc.selection().primary().to(), 3);
        edit(&mut doc, Change::delete(0, 3));
        assert_eq!(doc.selection().primary().to(), 0);
    }

    #[test]
    fn save_refuses_outside_changes_unless_forced() {
        let dir = temp_dir("conflict");
        let path = dir.join("a.txt");
        std::fs::write(&path, "disk\n").unwrap();
        let mut doc = Document::open(0, &path).unwrap();
        edit(&mut doc, Change::insert(0, "mine "));
        // Someone else writes (a different length, so the stamp differs even with coarse mtimes)
        std::fs::write(&path, "someone else\n").unwrap();
        let err = doc.save().unwrap_err().to_string();
        assert!(err.contains("changed on disk"), "{err}");
        assert!(doc.disk_conflict && doc.is_modified());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "someone else\n", "their file is untouched");
        // Still refused while the conflict stands, even if the stamps happened to line up again
        assert!(doc.save().is_err());
        doc.save_as(true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mine disk\n");
        assert!(!doc.disk_conflict && !doc.is_modified());
        doc.save().unwrap();
    }

    #[test]
    fn save_needs_a_path_and_a_finished_load() {
        let mut scratch = Document::from_str(0, "x");
        assert!(scratch.save().unwrap_err().to_string().contains("no file name"));
        let dir = temp_dir("loading");
        let path = dir.join("big.txt");
        std::fs::write(&path, "content\n").unwrap();
        let mut doc = Document::placeholder(0, &path);
        assert!(doc.save().unwrap_err().to_string().contains("loading"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "content\n", "never overwritten with nothing");
        doc.set_selection(Selection::point(0));
        doc.finish_loading(Rope::from_str("content\n"));
        assert!(!doc.loading);
        doc.save().unwrap();
    }

    #[test]
    fn opening_a_missing_file_gives_an_empty_document() {
        let dir = temp_dir("missing");
        let doc = Document::open(0, &dir.join("new.txt")).unwrap();
        assert_eq!(doc.text.len_bytes(), 0);
        assert!(doc.disk.is_none() && !doc.is_modified());
        assert_eq!(doc.display_name_short(), "new.txt");
        assert_eq!(Document::from_str(1, "").display_name(&dir), "[scratch]");
        assert_eq!(doc.display_name(&dir), "new.txt", "relative to the root");
        assert_eq!(doc.display_name(Path::new("/elsewhere")), dir.join("new.txt").display().to_string());
    }

    #[test]
    fn installed_history_renumbers_versions_past_the_current_one() {
        let mut doc = Document::from_str(0, "b");
        let history = (
            vec![(Rope::from_str(""), Selection::point(0))],
            vec![(Rope::from_str("bc"), Selection::point(2))],
        );
        doc.install_history(history);
        let (undo, redo) = doc.history();
        assert_eq!((undo.len(), redo.len()), (1, 1));
        assert!(doc.undo());
        assert_eq!(doc.text.to_string(), "");
        assert!(doc.is_modified(), "a restored step is not the saved version");
        assert!(doc.redo() && doc.redo());
        assert_eq!(doc.text.to_string(), "bc");
        assert_eq!(doc.selection().primary().head, 2);
    }
}
