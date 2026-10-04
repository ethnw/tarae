//! The side panel, left of the code: the project's files as a tree (`space e`), the outline of the file
//! being edited (`space o`), the list `]q` goes through (`space q`, quickfix.rs), git, or a plugin's tab
//! (its rows as it sends them — a cluster's objects in the manifests open). Its rows are the pickers' tree
//! rows (filetree.rs), drawn the same way.
//!
//! Unfocused it follows the editor — the current file (its folders opened) or the definition around the
//! cursor. Focused: `j`/`k` move, `h`/`l` fold and unfold (or go up / in), Enter opens, Tab switches files ↔
//! outline, Esc goes back to the code, `q` closes; `space` and `:` still work. The file list and the
//! outline are worked out on worker threads.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::document::DocId;
use crate::editor::{Editor, Mode as EdMode};
use crate::filetree::{Kind, Tree};
use crate::key::{Code, Key};
use crate::syntax::Def;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Files,
    Outline,
    List,
    Git,
    /// A plugin's tab (the side it sends, `Plugins::side`).
    Plugin,
}

/// What a row leads to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Dir(String),
    File(PathBuf),
    /// A definition in the current file: where its name is.
    At(usize),
    /// Item `n` of the list.
    Item(usize),
    /// Something in the git tab: a file, a folder, or (`rel` empty) a whole section — staged or not.
    Change {
        rel: String,
        staged: bool,
        untracked: bool,
        file: bool,
    },
    /// A hunk of an open changed file (its gutter's, against the index) · a merge conflict in it.
    Hunk {
        rel: String,
        doc: DocId,
        index: usize,
    },
    Conflict {
        rel: String,
        doc: DocId,
        index: usize,
    },
    /// Row `n` of a plugin's tab.
    Plugin(usize),
    /// A heading (a folder or file over the list's items) — nothing to open.
    None,
}

/// One row as the panel shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub depth: usize,
    pub kind: Kind,
    pub label: String,
    pub glyph: Option<(&'static str, &'static str)>,
    pub hint: String,
    pub target: Target,
}

/// What the git tab's rows were built from: the status read, the current file, each opened file's
/// (hunks version, conflicts version).
type GitOf = (u64, Option<PathBuf>, Vec<(String, u64, u64)>);

pub struct Sidebar {
    pub mode: Mode,
    /// Keys go to the panel.
    pub focus: bool,
    /// Row selected (focused) / the editor's row (unfocused).
    pub selected: usize,
    pub scroll: usize,
    /// The selection scroll last followed — the wheel scrolls freely until it moves.
    pub shown: Option<usize>,
    /// Rows pinned over the list's top this frame (`fit`).
    pub pins: Vec<usize>,
    /// Files: the folder (real path — documents hold real paths), its files (relative) as a tree.
    root: PathBuf,
    rels: Vec<String>,
    tree: Option<Tree>,
    /// Unfolded folders — changed through `set_open`, which rebuilds `files`.
    open: HashSet<String>,
    /// The file tree's rows, built when the tree or `open` changes — not every time they're drawn.
    files: Vec<Line>,
    pub loading: bool,
    /// The file whose folders were last opened, following the editor.
    revealed: Option<PathBuf>,
    /// Outline: which document version it's of, and one being worked out.
    outline_of: Option<(DocId, u64)>,
    pending: Option<(DocId, u64)>,
    pub defs: Vec<Def>,
    /// The list's rows, rebuilt every event while it shows (`sidebar_sync`) · the row of its current item.
    list: std::sync::Arc<[Line]>,
    list_here: Option<usize>,
    /// The list's generation those rows are of (rebuilt only when it changes) · the git status's and the
    /// current file's.
    list_of: Option<u64>,
    git_of: Option<GitOf>,
    /// Changed files opened to their hunks and conflicts (rows under them) · `space g` from the code: the
    /// place to land on (file, line) once the rows are in.
    pub expanded: HashSet<String>,
    pub aim: Option<(PathBuf, usize)>,
    /// Opened by `space g` from the code and nothing pressed since: the next key is about the code under
    /// the cursor (`space g s` stages its hunk, `space g t` keeps theirs) — then the code has the keys.
    pub from_code: bool,
    /// A plugin tab's rows folded (by their ids).
    pub folded: HashSet<String>,
}

impl Sidebar {
    /// Showing `mode`, focused; files under `root` (listed by `sidebar_list_files`).
    pub fn new(mode: Mode, root: PathBuf) -> Self {
        Sidebar {
            mode,
            focus: true,
            selected: 0,
            scroll: 0,
            shown: None,
            pins: Vec::new(),
            root,
            rels: Vec::new(),
            tree: None,
            open: HashSet::new(),
            files: Vec::new(),
            loading: true,
            revealed: None,
            outline_of: None,
            pending: None,
            defs: Vec::new(),
            list: Default::default(),
            list_here: None,
            list_of: None,
            git_of: None,
            expanded: HashSet::new(),
            aim: None,
            from_code: false,
            folded: HashSet::new(),
        }
    }

    /// Files mode with these files listed already (tests — the real list comes from a worker).
    #[cfg(test)]
    pub fn with_files(root: PathBuf, rels: Vec<String>) -> Self {
        let mut s = Sidebar::new(Mode::Files, root);
        s.tree = Some(Tree::build(rels.iter().enumerate().map(|(i, r)| (i, r.as_str(), r.len())), true));
        s.rels = rels;
        s.loading = false;
        s.build_files();
        s
    }

    /// The folder it shows (the git tab's repository is the one it's in).
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }

    /// The project's name (files) — the header says it.
    pub fn root_name(&self) -> String {
        self.root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }

    /// Is folder `d` unfolded?
    pub fn is_open(&self, d: &str) -> bool {
        self.open.contains(d)
    }

    /// Fold or unfold the folders `dirs` — the file rows are built again when that changed anything.
    pub fn set_open<'a>(&mut self, dirs: impl IntoIterator<Item = &'a str>, open: bool) {
        let mut changed = false;
        for d in dirs {
            changed |= if open { self.open.insert(d.to_string()) } else { self.open.remove(d) };
        }
        if changed {
            self.build_files();
        }
    }

    fn build_files(&mut self) {
        let Some(t) = &self.tree else { return self.files.clear() };
        self.files = t
            .rows(Some(&self.open))
            .into_iter()
            .map(|r| {
                let label = self.rels[r.source].chars().skip(r.span.start).take(r.span.len()).collect();
                let target = match &r.dir {
                    Some(d) => Target::Dir(d.clone()),
                    None => Target::File(self.root.join(&self.rels[r.source])),
                };
                // A folded folder says how many files it holds
                let hint = match r.kind {
                    Kind::Dir { open: Some(false) } => r.count.to_string(),
                    _ => String::new(),
                };
                Line { depth: r.depth, kind: r.kind, label, glyph: None, hint, target }
            })
            .collect();
    }

    /// The rows (the files', the list's and the git tab's are kept, not copied).
    pub fn lines(&self) -> std::borrow::Cow<'_, [Line]> {
        use std::borrow::Cow;
        Cow::Owned(match self.mode {
            Mode::Files => return Cow::Borrowed(&self.files),
            Mode::Outline => self
                .defs
                .iter()
                .map(|d| Line {
                    depth: d.depth,
                    kind: Kind::File,
                    label: d.name.clone(),
                    glyph: Some(match d.kind {
                        'f' => ("ƒ", "function"),
                        'm' => ("§", "namespace"),
                        _ => ("τ", "type"),
                    }),
                    hint: String::new(),
                    target: Target::At(d.at),
                })
                .collect(),
            Mode::List | Mode::Git | Mode::Plugin => return Cow::Borrowed(&self.list),
        })
    }

    /// The row of what the editor is on: the current file, or the innermost definition around the cursor.
    pub fn here(&self, ed: &Editor) -> Option<usize> {
        let doc = ed.doc();
        match self.mode {
            Mode::Files => {
                let path = doc.path.as_ref()?;
                self.lines().iter().position(|l| matches!(&l.target, Target::File(p) if p == path))
            }
            Mode::Outline => {
                let head = doc.selection().primary().head;
                self.defs.iter().rposition(|d| d.range.contains(&head))
            }
            Mode::List | Mode::Git | Mode::Plugin => self.list_here,
        }
    }

    /// Before drawing `rows` list rows (`depths` = each line's): scroll to the selection when it moved —
    /// just into view when focused, centered when following the editor — and work out the folders pinned
    /// over the top, never over the selection.
    pub fn fit(&mut self, rows: usize, depths: &[usize]) {
        let n = depths.len();
        if n == 0 || rows == 0 {
            self.scroll = 0;
            self.pins.clear();
            return;
        }
        self.selected = self.selected.min(n - 1);
        let follow = self.shown != Some(self.selected);
        let center = self.shown.is_none(); // just opened or switched: the selection mid-list
        self.shown = Some(self.selected);
        if follow && (self.selected < self.scroll || self.selected >= self.scroll + rows) {
            self.scroll = match (self.focus && !center, self.selected < self.scroll) {
                (true, true) => self.selected,
                (true, false) => self.selected + 1 - rows,
                (false, _) => self.selected.saturating_sub(rows / 2),
            };
        }
        self.scroll = self.scroll.min(n.saturating_sub(rows));
        let mut pins: Vec<usize> = Vec::new();
        for _ in 0..4 {
            let at = (self.scroll + pins.len()).min(n - 1);
            let next = crate::filetree::enclosing(|i| depths[i], at, rows / 2);
            let settled = next.len() == pins.len();
            pins = next;
            if follow && self.selected < self.scroll + pins.len() {
                self.scroll = self.selected.saturating_sub(pins.len());
            } else if settled {
                break;
            }
        }
        self.pins = pins;
    }

    /// The rows on the list's lines, top to bottom — pinned folders first.
    pub fn shown_rows(&self, rows: usize, n: usize) -> Vec<usize> {
        let rest = (self.scroll + self.pins.len()..n).take(rows.saturating_sub(self.pins.len()));
        self.pins.iter().copied().chain(rest).collect()
    }

    /// Nothing to list: why (faint, in the list's place).
    pub fn empty_note(&self, ed: &Editor) -> &'static str {
        match self.mode {
            Mode::Files if self.loading => "listing files…",
            Mode::Files => "no files here",
            Mode::Outline if ed.doc().syntax.is_none() => "no grammar for this file",
            Mode::Outline if self.outline_of.is_none() => "reading…",
            Mode::Outline => "nothing defined here",
            Mode::List => "the list is empty",
            Mode::Git if ed.lane.git_status.is_none() => "not a git repository (yet — reading…)",
            Mode::Git => "nothing to commit — the tree is clean",
            Mode::Plugin => "nothing to show yet",
        }
    }

    /// `delta` rows on, stepping over headings (the list's folders and files), within the rows.
    fn move_by(&mut self, delta: isize, lines: &[Line]) {
        let n = lines.len() as isize;
        if n == 0 {
            return;
        }
        let mut at = (self.selected as isize + delta).clamp(0, n - 1);
        let step = if delta < 0 { -1 } else { 1 };
        while lines[at as usize].target == Target::None {
            let next = at + step;
            if !(0..n).contains(&next) {
                return; // nothing to land on that way
            }
            at = next;
        }
        self.selected = at as usize;
    }
}

/// The row of the file `current` in the git tab's rows (among the changes first).
fn git_here(rows: &[Line], top: &std::path::Path, current: Option<&std::path::Path>) -> Option<usize> {
    let rel = current?.strip_prefix(top).ok()?.to_string_lossy().into_owned();
    rows.iter().position(|l| matches!(&l.target, Target::Change { rel: r, file: true, .. } if *r == rel))
}

/// Under an opened changed file: its hunks (the gutter's, against the index — the text where it starts,
/// `+a −r`) and merge conflicts, in line order. From its document (opened when it was expanded).
fn detail_rows(
    docs: &[crate::document::Document],
    top: &std::path::Path,
    rel: &str,
    depth: usize,
) -> Vec<Line> {
    let path = top.join(rel);
    let row = |label: String, glyph, hint: String, target| Line {
        depth,
        kind: Kind::Hit,
        label,
        glyph,
        hint,
        target,
    };
    let Some(doc) = docs.iter().find(|d| d.path.as_deref() == Some(path.as_path())) else {
        return vec![row("…".into(), None, String::new(), Target::None)];
    };
    let mut out: Vec<(usize, Line)> = Vec::new();
    // In a review its hunks are against the review's base, not the index — `s`/`d` would act on those
    if doc.review_base.is_some() {
        let note = "in the review";
        out.push((0, Line { kind: Kind::Note, ..row(note.into(), None, String::new(), Target::None) }));
    } else if doc.git_version == doc.version() {
        for (index, h) in doc.git_hunks.iter().enumerate() {
            // A conflict's markers are a change too — its own row says it better
            let end = h.lines.end.max(h.lines.start + 1);
            if doc.conflicts.iter().any(|c| h.lines.start <= c.end && c.start < end) {
                continue;
            }
            let at = h.lines.start.min(crate::movement::last_line(&doc.text));
            let said = if h.lines.is_empty() {
                format!("{} line{} removed", h.removed, if h.removed == 1 { "" } else { "s" })
            } else {
                doc.text.line(at).to_string().trim().to_string()
            };
            let scope = match h.kind {
                crate::git::HunkKind::Added => "diff.plus",
                crate::git::HunkKind::Modified => "diff.delta",
                crate::git::HunkKind::Deleted => "diff.minus",
            };
            let hint = match (h.lines.len(), h.removed) {
                (a, 0) => format!("+{a}"),
                (0, r) => format!("−{r}"),
                (a, r) => format!("+{a} −{r}"),
            };
            let target = Target::Hunk { rel: rel.to_string(), doc: doc.id, index };
            out.push((at, row(format!("{}: {said}", at + 1), Some(("▎", scope)), hint, target)));
        }
    } else if doc.git_base.is_some() || doc.loading {
        out.push((0, row("…".into(), None, String::new(), Target::None)));
    }
    for (index, c) in doc.conflicts.iter().enumerate() {
        let side = |s: &str, or: &str| if s.is_empty() { or.to_string() } else { s.to_string() };
        let label = format!("{}: {} ↔ {}", c.start + 1, side(&c.ours, "ours"), side(&c.theirs, "theirs"));
        let target = Target::Conflict { rel: rel.to_string(), doc: doc.id, index };
        out.push((c.start, row(label, Some(("●", "error")), String::new(), target)));
    }
    out.sort_by_key(|r| r.0);
    out.into_iter().map(|r| r.1).collect()
}

/// The git tab's rows: "Staged" and "Changes" sections, each a tree of its files (status letter in the
/// gutter's colors, `+a −r`) — built on the worker that reads the status.
pub fn git_rows(st: &crate::gitpanel::Status) -> Vec<Line> {
    let mut lines = Vec::new();
    for (staged, title, list) in [(false, "Changes", &st.unstaged), (true, "Staged", &st.staged)] {
        if list.is_empty() {
            continue;
        }
        let untracked_all = list.iter().all(|e| e.letter == '?');
        lines.push(Line {
            depth: 0,
            kind: Kind::Section,
            label: title.into(),
            glyph: None,
            hint: list.len().to_string(),
            target: Target::Change { rel: String::new(), staged, untracked: untracked_all, file: false },
        });
        let tree = Tree::build(list.iter().enumerate().map(|(i, e)| (i, e.rel.as_str(), e.rel.len())), false);
        for r in tree.rows(None) {
            let e = &list[r.source];
            let label: String = e.rel.chars().skip(r.span.start).take(r.span.len()).collect();
            let (target, glyph, hint) = match &r.dir {
                Some(d) => (
                    Target::Change { rel: d.clone(), staged, untracked: false, file: false },
                    None,
                    String::new(),
                ),
                None => {
                    let scope = match e.letter {
                        'A' | '?' => "diff.plus",
                        'D' => "diff.minus",
                        'U' => "error",
                        _ => "diff.delta",
                    };
                    let letter: &'static str = match e.letter {
                        'A' => "A",
                        '?' => "?",
                        'D' => "D",
                        'R' => "R",
                        'U' => "U",
                        'T' => "T",
                        _ => "M",
                    };
                    let hint = match (e.added, e.removed) {
                        (Some(a), Some(0)) => format!("+{a}"),
                        (Some(0), Some(r)) => format!("−{r}"),
                        (Some(a), Some(r)) => format!("+{a} −{r}"),
                        _ => String::new(),
                    };
                    let t =
                        Target::Change { rel: e.rel.clone(), staged, untracked: e.letter == '?', file: true };
                    (t, Some((letter, scope)), hint)
                }
            };
            lines.push(Line { depth: r.depth + 1, kind: r.kind, label, glyph, hint, target });
        }
    }
    lines
}

/// The list as tree rows — its items under their files and folders (headings), severity glyphs, the
/// current item's row.
fn list_lines(list: Option<&crate::quickfix::List>, root: &std::path::Path) -> (Vec<Line>, Option<usize>) {
    let Some(list) = list else { return (Vec::new(), None) };
    let labels = crate::quickfix::labels(list, root);
    let tree = Tree::build(labels.iter().enumerate().map(|(i, (l, n))| (i, l.as_str(), *n)), false);
    let mut here = None;
    let lines = tree
        .rows(None)
        .into_iter()
        .enumerate()
        .map(|(k, r)| {
            let label: String = labels[r.source].0.chars().skip(r.span.start).take(r.span.len()).collect();
            let glyph = r.item.and_then(|i| match list.items[i].severity {
                1 => Some(("●", "error")),
                2 => Some(("▲", "warning")),
                3 => Some(("●", "info")),
                _ => None,
            });
            if r.item.is_some() && r.item == list.current {
                here = Some(k);
            }
            let hint = if r.kind == Kind::File && r.count > 0 { r.count.to_string() } else { String::new() };
            let target = r.item.map_or(Target::None, Target::Item);
            Line { depth: r.depth, kind: r.kind, label, glyph, hint, target }
        })
        .collect();
    (lines, here)
}

/// A plugin tab's rows as the panel shows them — under a folded row, none — and the row of what's under
/// the cursor (`line` of the file at `path`): the deepest one whose place holds it.
fn plugin_lines(
    side: Option<&crate::plugin::PluginSide>,
    folded: &HashSet<String>,
    path: Option<&std::path::Path>,
    line: usize,
) -> (Vec<Line>, Option<usize>) {
    use crate::plugin::PluginTone as T;
    let Some(side) = side else { return (Vec::new(), None) };
    let mut out = Vec::new();
    let mut here: Option<(usize, u64)> = None;
    let mut hidden_below: Option<u64> = None;
    let rows = &side.rows;
    for (i, r) in rows.iter().enumerate() {
        match hidden_below {
            Some(d) if r.depth > d => continue,
            _ => hidden_below = None,
        }
        let parent = rows.get(i + 1).is_some_and(|n| n.depth > r.depth);
        let shut = parent && folded.contains(&r.id);
        if shut {
            hidden_below = Some(r.depth);
        }
        let glyph = match r.tone {
            T::Ok => ("●", "diff.plus"),
            T::Info => ("·", "comment"),
            T::Warning => ("▲", "warning"),
            T::Error => ("✗", "error"),
        };
        let at_cursor = r.at.as_ref().is_some_and(|(p, from, to)| {
            path.is_some_and(|x| x == std::path::Path::new(p)) && (*from..=*to).contains(&(line as u64))
        });
        if at_cursor && here.is_none_or(|(_, d)| r.depth >= d) {
            here = Some((out.len(), r.depth));
        }
        // Its tone's glyph on every row (a folder's `+`/`−` would take its place): the top rows are
        // headings, and a row folded says so
        // (a note: text in its tone's color, nothing to select)
        out.push(Line {
            depth: r.depth as usize,
            kind: match () {
                _ if r.note => Kind::Note,
                _ if r.depth == 0 && parent => Kind::Section,
                _ => Kind::File,
            },
            label: if shut { format!("{} …", r.text) } else { r.text.clone() },
            glyph: Some(glyph),
            hint: r.detail.clone(),
            target: if r.note { Target::None } else { Target::Plugin(i) },
        });
    }
    (out, here.map(|h| h.0))
}

/// The folders above `rel` (`src/ui/card.rs` → `src`, `src/ui`).
fn ancestors(rel: &str) -> impl Iterator<Item = &str> {
    rel.match_indices('/').map(|(i, _)| &rel[..i])
}

impl Editor {
    /// `space e` / `space o` — show the panel in `mode` and focus it; on it already, close it.
    pub fn sidebar_show(&mut self, mode: Mode) {
        self.lane.side_last = mode;
        // A review's rail holds the panel's place — `space e` gives it the keys
        if mode == Mode::Files && self.lane.code_review.is_some() {
            return crate::codereview::rail_focus(self);
        }
        match self.lane.sidebar.as_mut() {
            Some(s) if s.mode == mode && s.focus => {
                self.lane.sidebar = None;
                return;
            }
            Some(s) => {
                s.mode = mode;
                s.focus = true;
                s.shown = None;
                s.list_of = None; // another view may have used the rows
                s.git_of = None;
            }
            None => self.lane.sidebar = Some(Sidebar::new(mode, self.lane.root.clone())),
        }
        if let Some(c) = self.lane.chat.as_mut() {
            c.focused = false;
        }
        if mode == Mode::Files {
            self.sidebar_list_files();
        }
        if self.side_folded() {
            self.set_warning(format!(
                "no room for the panel — the code keeps {} columns (C-w < narrows the chat)",
                crate::sizes::CODE_MIN
            ));
        }
        self.sidebar_sync();
        let here = self.lane.sidebar.as_ref().and_then(|s| s.here(self));
        let Some(s) = self.lane.sidebar.as_mut() else { return };
        match here {
            Some(h) => s.selected = h,
            // Not on a heading (the list's folders and files — Enter does nothing there)
            None => {
                let lines = s.lines().into_owned();
                s.move_by(0, &lines);
            }
        }
    }

    /// (Re)list the files — on a worker, the tree built there too.
    pub(crate) fn sidebar_list_files(&mut self) {
        let Some(s) = self.lane.sidebar.as_ref() else { return };
        let root = s.root.clone();
        self.events.jobs().spawn(move || {
            let rels: Vec<String> = crate::picker::list_files(&root)
                .into_iter()
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .collect();
            let tree = Tree::build(rels.iter().enumerate().map(|(i, r)| (i, r.as_str(), r.len())), true);
            move |ed: &mut Editor| {
                let Some(s) = ed.lane.sidebar.as_mut().filter(|s| s.root == root) else { return };
                s.rels = rels;
                s.tree = Some(tree);
                s.build_files();
                s.loading = false;
                s.revealed = None; // open the current file's folders in the new tree
                s.shown = None;
                ed.sidebar_sync();
                if let Some(h) = ed.lane.sidebar.as_ref().and_then(|s| s.here(ed)) {
                    ed.lane.sidebar.as_mut().unwrap().selected = h;
                }
            }
        });
    }

    /// Every event: follow the editor — open the current file's folders, keep the outline current (worked
    /// out on a worker per document version), and, unfocused, select what the editor is on.
    pub fn sidebar_sync(&mut self) {
        let Some(s) = self.lane.sidebar.as_mut() else { return };
        let doc = &self.docs[self.current];
        match s.mode {
            Mode::List => {
                let now = self.lane.qlist.as_ref().map(|l| l.generation);
                if s.list_of != now || now.is_none() {
                    let (list, here) = list_lines(self.lane.qlist.as_ref(), &s.root);
                    (s.list, s.list_here) = (list.into(), here);
                    s.list_of = now;
                    s.git_of = None; // the git tab's rows need building again
                }
            }
            Mode::Git => {
                let top = self.lane.git_status.as_ref().map(|st| st.top.clone());
                // `space g` from the code: its file opens to its hunks (when it's among the changes)
                if let (Some((p, _)), Some(top), Some(st)) = (&s.aim, &top, self.lane.git_status.as_ref())
                    && let Ok(rel) = p.strip_prefix(top)
                {
                    let rel = rel.to_string_lossy().into_owned();
                    if st.unstaged.iter().any(|e| e.rel == rel && e.letter != '?') {
                        s.expanded.insert(rel);
                    }
                }
                let mut opened: Vec<(String, u64, u64)> = s
                    .expanded
                    .iter()
                    .map(|rel| {
                        let d = top.as_ref().and_then(|t| {
                            let p = t.join(rel);
                            self.docs.iter().find(|d| d.path.as_deref() == Some(p.as_path()))
                        });
                        (rel.clone(), d.map_or(0, |d| d.git_version), d.map_or(0, |d| d.conflicts_of))
                    })
                    .collect();
                opened.sort();
                let now =
                    (self.lane.git_status.as_ref().map_or(0, |g| g.generation), doc.path.clone(), opened);
                if s.git_of.as_ref() != Some(&now) {
                    let first = s.list.is_empty();
                    (s.list, s.list_here) = match self.lane.git_status.as_ref() {
                        Some(st) if s.expanded.is_empty() => {
                            (st.rows.clone(), git_here(&st.rows, &st.top, doc.path.as_deref()))
                        }
                        Some(st) => {
                            let mut rows = Vec::with_capacity(st.rows.len());
                            for l in st.rows.iter() {
                                rows.push(l.clone());
                                if let Target::Change { rel, staged: false, file: true, .. } = &l.target
                                    && s.expanded.contains(rel)
                                {
                                    rows.extend(detail_rows(&self.docs, &st.top, rel, l.depth + 1));
                                }
                            }
                            let here = git_here(&rows, &st.top, doc.path.as_deref());
                            (rows.into(), here)
                        }
                        None => (Default::default(), None),
                    };
                    // The status just arrived (the tab opens before it's read): start on the current file
                    if first && s.focus {
                        s.selected = s.list_here.unwrap_or(0);
                    }
                    s.git_of = Some(now);
                    s.list_of = None; // the list's rows need building again
                }
                // `space g` from the code: on the row for what's under the cursor — its hunk or conflict,
                // else its file — settled once that file's hunks are in
                if let (Some((p, line)), Some(top)) = (s.aim.clone(), &top) {
                    let d = self.docs.iter().find(|d| d.path.as_deref() == Some(p.as_path()));
                    let under = |t: &Target| match (t, d) {
                        (Target::Hunk { doc, index, .. }, Some(d)) if *doc == d.id => {
                            crate::gitmenu::hunk_at(&d.git_hunks, line)
                                .is_some_and(|h| d.git_hunks.get(*index).is_some_and(|x| std::ptr::eq(h, x)))
                        }
                        (Target::Conflict { doc, index, .. }, Some(d)) if *doc == d.id => {
                            d.conflicts.get(*index).is_some_and(|c| (c.start..=c.end).contains(&line))
                        }
                        _ => false,
                    };
                    let row = s.list.iter().position(|l| under(&l.target));
                    let file = git_here(&s.list, top, Some(&p));
                    if let Some(r) = row.or(file) {
                        s.selected = r;
                    }
                    let settled = d.is_none_or(|d| d.git_base.is_none() || d.git_version == d.version());
                    if row.is_some() || (self.lane.git_status.is_some() && settled) {
                        s.aim = None;
                    }
                }
            }
            Mode::Plugin => {
                let side = self.plugins.as_ref().and_then(|p| p.side.as_ref()).map(|(_, side)| side);
                let head = doc.selection().primary().cursor(&doc.text);
                let line = doc.text.byte_to_line(head.min(doc.text.len_bytes()));
                let (list, here) = plugin_lines(side, &s.folded, doc.path.as_deref(), line);
                (s.list, s.list_here) = (list.into(), here);
                // (the list's and the git tab's rows need building again)
                (s.list_of, s.git_of) = (None, None);
            }
            Mode::Files => {
                if let Some(path) = doc.path.as_ref().filter(|p| s.revealed.as_ref() != Some(*p))
                    && s.tree.is_some()
                {
                    if let Ok(rel) = path.strip_prefix(&s.root) {
                        let rel = rel.to_string_lossy().replace('\\', "/");
                        s.set_open(ancestors(&rel), true);
                    }
                    s.revealed = Some(path.clone());
                }
            }
            Mode::Outline => {
                let want = (doc.id, doc.version());
                if s.outline_of.is_some_and(|o| o.0 != doc.id) {
                    s.defs.clear(); // another file's — gone at once
                    s.outline_of = None;
                }
                let tree = doc.syntax.as_ref().and_then(|syn| syn.tree.clone());
                if s.outline_of != Some(want)
                    && s.pending != Some(want)
                    && let Some(tree) = tree
                {
                    s.pending = Some(want);
                    let text = doc.text.clone();
                    self.events.jobs().spawn(move || {
                        let defs = crate::syntax::outline(&tree, &text);
                        move |ed: &mut Editor| {
                            let Some(s) = ed.lane.sidebar.as_mut() else { return };
                            if s.pending == Some(want) {
                                s.pending = None;
                            }
                            // A later version may have landed already; another file's is dropped
                            if ed.docs.get(ed.current).is_some_and(|d| d.id == want.0)
                                && s.outline_of.is_none_or(|o| o.0 != want.0 || o.1 <= want.1)
                            {
                                s.defs = defs;
                                s.outline_of = Some(want);
                            }
                        }
                    });
                }
            }
        }
        let Some(s) = self.lane.sidebar.as_ref() else { return };
        if !s.focus
            && let Some(h) = s.here(self)
        {
            self.lane.sidebar.as_mut().unwrap().selected = h;
        }
    }

    /// Enter / a click: a folder folds or unfolds, a file opens, a definition is jumped to — the code gets
    /// the keys back.
    pub fn sidebar_activate(&mut self, row: usize) {
        let Some(s) = self.lane.sidebar.as_mut() else { return };
        let Some(line) = s.lines().get(row).cloned() else { return };
        s.selected = row;
        match line.target {
            Target::Dir(d) => {
                let open = !s.is_open(&d);
                s.set_open([d.as_str()], open);
            }
            Target::File(p) => {
                s.focus = false;
                if let Err(e) = self.open(&p) {
                    self.set_error(format!("{e:#}"));
                }
            }
            Target::Item(i) => {
                s.focus = false;
                self.qlist_goto(i);
            }
            // A changed file (not new): open to its hunks and conflicts — it shows in the editor, the
            // panel keeps the keys; closed again the same way
            Target::Change { rel, file: true, staged: false, untracked: false } if s.focus => {
                if s.expanded.remove(&rel) {
                    return;
                }
                let Some(top) = self.lane.git_status.as_ref().map(|s| s.top.clone()) else { return };
                let path = top.join(&rel);
                if !path.exists() {
                    return self.set_warning(format!("{rel} is deleted — nothing to open"));
                }
                s.expanded.insert(rel);
                if let Err(e) = self.open(&path) {
                    self.set_error(format!("{e:#}"));
                }
            }
            Target::Change { rel, file: true, .. } => {
                let Some(top) = self.lane.git_status.as_ref().map(|s| s.top.clone()) else { return };
                let path = top.join(&rel);
                if !path.exists() {
                    return self.set_warning(format!("{rel} is deleted — nothing to open"));
                }
                s.focus = false;
                if let Err(e) = self.open(&path) {
                    self.set_error(format!("{e:#}"));
                }
            }
            // A hunk or conflict: there in the code, which gets the keys
            Target::Hunk { .. } | Target::Conflict { .. } => {
                s.focus = false;
                self.git_reveal(&line.target);
            }
            // A plugin's row: where it's defined (there, in the code), or the plugin's to act on
            Target::Plugin(i) => {
                let at = self
                    .plugins
                    .as_ref()
                    .and_then(|p| p.side.as_ref())
                    .and_then(|(_, side)| side.rows.get(i)?.at.clone());
                match at {
                    Some((path, line, _)) => {
                        s.focus = false;
                        let goto =
                            crate::picker::Action::Goto { path: path.into(), line: line as usize, col: 0 };
                        self.run_picker_action(goto);
                    }
                    None => self.plugin_side_key("ret", i),
                }
            }
            Target::None | Target::Change { .. } => {}
            Target::At(at) => {
                s.focus = false;
                let fresh = s.outline_of.is_some_and(|o| o == (self.doc().id, self.doc().version()));
                if !fresh {
                    return self.set_status("the outline is catching up — try again");
                }
                self.push_jump();
                let doc = self.doc_mut();
                doc.set_selection(crate::selection::Selection::point(at.min(doc.text.len_bytes())));
            }
        }
    }
}

impl Editor {
    /// The code at a git row's hunk (selected) or conflict (the cursor on it), centered — its file shown.
    /// False when it's gone.
    pub(crate) fn git_reveal(&mut self, t: &Target) -> bool {
        let (Target::Hunk { doc, .. } | Target::Conflict { doc, .. }) = t else { return false };
        let Some(path) = self.docs.iter().find(|d| d.id == *doc).and_then(|d| d.path.clone()) else {
            return false;
        };
        if self.doc().id != *doc && self.open(&path).is_err() {
            return false;
        }
        let d = self.doc();
        match t {
            Target::Hunk { index, .. } => {
                let Some(h) = d.git_hunks.get(*index).cloned() else { return false };
                self.select_hunk(&h);
            }
            Target::Conflict { index, .. } => {
                let Some(c) = d.conflicts.get(*index) else { return false };
                let at = d.text.line_to_byte(c.start.min(crate::movement::last_line(&d.text)));
                self.doc_mut().set_selection(crate::selection::Selection::point(at));
            }
            _ => return false,
        }
        self.align_view(crate::viewalign::Place::Center);
        true
    }
}

/// The git tab's keys on its selected row (`at`) — false: not one of them.
fn git_key(ed: &mut Editor, key: Key, at: Option<Line>) -> bool {
    use crate::conflict::Pick;
    let t = at.map(|l| l.target);
    let change = match &t {
        Some(Target::Change { rel, staged, untracked, file }) => {
            Some((rel.clone(), *staged, *untracked, *file))
        }
        _ => None,
    };
    let from_code = ed.lane.sidebar.as_mut().is_some_and(|s| std::mem::take(&mut s.from_code));
    let Code::Char(c) = key.code else { return false };
    // Right after `space g` in the code: a hunk's and a conflict's keys act there. The tab keeps the keys
    // either way (#18, #50: given back to the code, the next `j`/`k`/`s` went there)
    let (on_hunk, on_conflict) = if from_code { ed.git_code_here() } else { (false, false) };
    if (on_hunk && matches!(c, 's' | 'd')) || (on_conflict && matches!(c, 'o' | 't' | 'a')) {
        match c {
            's' => ed.git_stage_hunk(),
            'd' => ed.git_reset_hunk(),
            'o' => ed.conflict_take(Pick::Ours),
            't' => ed.conflict_take(Pick::Theirs),
            _ => ed.conflict_take(Pick::Both),
        }
        return true;
    }
    match (c, &t, change) {
        // A hunk: stage it · its index lines back
        ('s', Some(t @ Target::Hunk { .. }), _) => {
            if ed.git_reveal(t) {
                ed.git_stage_hunk();
            }
        }
        ('d', Some(t @ Target::Hunk { .. }), _) => {
            if ed.git_reveal(t) {
                ed.git_reset_hunk();
            }
        }
        ('u', Some(Target::Hunk { .. }), _) => ed.set_status("a hunk isn't staged yet — s stages it"),
        // A conflict: keep ours · theirs · both
        ('o' | 't' | 'a', Some(t @ Target::Conflict { .. }), _) => {
            let pick = match c {
                'o' => Pick::Ours,
                't' => Pick::Theirs,
                _ => Pick::Both,
            };
            if ed.git_reveal(t) {
                ed.conflict_take(pick);
            }
        }
        ('o' | 't' | 'a', ..) => ed.set_status("o t a keep ours / theirs / both — on a conflict's row"),
        ('s', _, Some((rel, false, ..))) => ed.git_stage(&rel, true),
        ('u', _, Some((rel, true, ..))) => ed.git_stage(&rel, false),
        ('s' | 'u', _, Some(_)) => {
            ed.set_status("already there — s stages what's in Changes, u unstages what's Staged")
        }
        ('d', _, Some((rel, false, ..))) => ed.git_discard(&rel),
        ('d', _, Some(_)) => ed.set_status("staged — u unstages it first"),
        // Into the editor with the keys: the file · the hunk or conflict · the file against HEAD
        ('e' | 'D', _, Some((rel, _, _, true))) => {
            let Some(top) = ed.lane.git_status.as_ref().map(|s| s.top.clone()) else { return true };
            let path = top.join(&rel);
            if !path.exists() {
                ed.set_warning(format!("{rel} is deleted — nothing to open"));
                return true;
            }
            if let Err(e) = ed.open(&path) {
                ed.set_error(format!("{e:#}"));
                return true;
            }
            if let Some(s) = ed.lane.sidebar.as_mut() {
                s.focus = false;
            }
            if c == 'D'
                && let Err(e) = ed.diff_against("")
            {
                ed.set_error(e);
            }
        }
        ('e', Some(t @ (Target::Hunk { .. } | Target::Conflict { .. })), _) => {
            if let Some(s) = ed.lane.sidebar.as_mut() {
                s.focus = false;
            }
            ed.git_reveal(t);
        }
        // `l` / `h` open and close a changed file's hunks, like a folder
        ('l' | 'h', _, Some((rel, false, false, true))) => {
            let open = ed.lane.sidebar.as_ref().is_some_and(|s| s.expanded.contains(&rel));
            if open == (c == 'l') {
                return false; // (already so: the tree's own l / h)
            }
            let row = ed.lane.sidebar.as_ref().map_or(0, |s| s.selected);
            ed.sidebar_activate(row);
        }
        ('c', ..) => ed.git_commit_start(false),
        ('A', ..) => ed.git_commit_start(true),
        ('C', ..) => ed.git_commit_open(false, true),
        ('P', ..) => ed.git_push_pull(true),
        ('p', ..) => ed.git_push_pull(false),
        ('f', ..) => ed.git_fetch(),
        ('r', ..) => ed.git_refresh(),
        ('B', ..) => ed.git_branch_picker(),
        ('L', ..) => ed.git_log_picker(),
        ('z', ..) => ed.git_stash(false),
        ('Z', ..) => ed.git_stash(true),
        ('b', ..) => ed.git_toggle_blame(),
        ('?', ..) => ed.git_help(),
        _ => return false,
    }
    true
}

/// Keys while the panel has focus (normal mode, nothing pending). False = not the panel's.
pub fn key(ed: &mut Editor, key: Key) -> bool {
    if ed.mode != EdMode::Normal || !ed.pending.is_empty() || ed.prompt.is_some() {
        return false;
    }
    // Folded away (the terminal is too narrow for it and the code): the keys are the code's
    let folded = ed.side_folded();
    let Some(s) = ed.lane.sidebar.as_mut().filter(|s| s.focus) else { return false };
    if folded {
        s.focus = false;
        return false;
    }
    let lines = s.lines().into_owned();
    let n = lines.len();
    let at = lines.get(s.selected);
    let mode = s.mode;
    // The git tab's own keys (gitpanel.rs)
    if mode == Mode::Git && !key.ctrl && !key.alt && git_key(ed, key, at.cloned()) {
        return true;
    }
    // A plugin's tab: its rows fold (h / l), and keys the panel doesn't take are the plugin's
    if mode == Mode::Plugin
        && !key.alt
        && let Some(Target::Plugin(i)) = at.map(|l| l.target.clone())
    {
        let rows = ed.plugins.as_ref().and_then(|p| p.side.as_ref()).map(|(_, side)| &side.rows);
        let parent =
            rows.is_some_and(|rs| rs.get(i + 1).zip(rs.get(i)).is_some_and(|(n, r)| n.depth > r.depth));
        let id = rows.and_then(|rs| rs.get(i)).map(|r| r.id.clone());
        let s = ed.lane.sidebar.as_mut().expect("focused above");
        match (key.code, key.ctrl) {
            (Code::Char('l') | Code::Right, false)
                if parent && id.as_ref().is_some_and(|id| s.folded.remove(id)) =>
            {
                ed.sidebar_sync();
                return true;
            }
            (Code::Char('h') | Code::Left, false)
                if parent && id.as_ref().is_some_and(|id| !s.folded.contains(id)) =>
            {
                s.folded.extend(id);
                ed.sidebar_sync();
                return true;
            }
            (Code::Char(c), false) if !"jkgGhlq :".contains(c) => {
                ed.plugin_side_key(&key.to_string(), i);
                return true;
            }
            _ => {}
        }
    }
    let before = ed.lane.sidebar.as_ref().map(|s| s.selected);
    let Some(s) = ed.lane.sidebar.as_mut() else { return false };
    match (key.code, key.ctrl) {
        // Still the editor's: the space menu, the command line, the windows (`C-w e` back to the code)
        (Code::Char(' ') | Code::Char(':'), false) | (Code::Char('w'), true) => return false,
        (Code::Char('j') | Code::Down, false) | (Code::Char('n'), true) => s.move_by(1, &lines),
        (Code::Char('k') | Code::Up, false) | (Code::Char('p'), true) => s.move_by(-1, &lines),
        (Code::Char('d'), true) | (Code::PageDown, _) => s.move_by(10, &lines),
        (Code::Char('u'), true) | (Code::PageUp, _) => s.move_by(-10, &lines),
        (Code::Char('g') | Code::Home, false) => {
            s.selected = 0;
            s.move_by(0, &lines);
        }
        (Code::Char('G') | Code::End, false) => {
            s.selected = n.saturating_sub(1);
            s.move_by(0, &lines);
        }
        // l / → : unfold a folder, or step into it
        (Code::Char('l') | Code::Right, false) => match at.map(|l| &l.target) {
            Some(Target::Dir(d)) if !s.is_open(d) => s.set_open([d.as_str()], true),
            _ if at.is_some_and(|l| lines.get(s.selected + 1).is_some_and(|x| x.depth > l.depth)) => {
                s.move_by(1, &lines)
            }
            _ => {}
        },
        // h / ← : fold a folder, or go to the row it's in
        (Code::Char('h') | Code::Left, false) => match at {
            Some(Line { target: Target::Dir(d), .. }) if s.is_open(d) => s.set_open([d.as_str()], false),
            Some(l) => {
                if let Some(up) = lines[..s.selected].iter().rposition(|x| x.depth < l.depth) {
                    s.selected = up;
                }
            }
            None => {}
        },
        (Code::Enter, _) => {
            let row = s.selected;
            ed.sidebar_activate(row);
        }
        // files → outline → the list (when there is one) → git → a plugin's tab (when there is one) → files
        (Code::Tab, _) => {
            let list = ed.lane.qlist.as_ref().is_some_and(|l| !l.items.is_empty());
            let plugin = ed.plugins.as_ref().is_some_and(|p| p.side.is_some());
            let mode = match s.mode {
                Mode::Files => Mode::Outline,
                Mode::Outline if list => Mode::List,
                Mode::Outline | Mode::List => Mode::Git,
                Mode::Git if plugin => Mode::Plugin,
                Mode::Git | Mode::Plugin => Mode::Files,
            };
            s.focus = false; // so sidebar_show switches instead of closing
            ed.sidebar_show(mode);
            if mode == Mode::Git {
                ed.git_refresh();
            }
        }
        // (in front of a review's rail: the rail back, with the keys)
        (Code::Esc, _) if ed.lane.code_review.as_ref().is_some_and(|r| r.behind) => {
            crate::codereview::rail_focus(ed)
        }
        (Code::Esc, _) => s.focus = false,
        (Code::Char('q'), false) => ed.lane.sidebar = None,
        _ => {}
    }
    // Moving onto a hunk or conflict shows it in the code
    if let Some(s) =
        ed.lane.sidebar.as_ref().filter(|s| s.mode == Mode::Git && s.focus && Some(s.selected) != before)
        && let Some(t) = s.lines().get(s.selected).map(|l| l.target.clone())
        && matches!(t, Target::Hunk { .. } | Target::Conflict { .. })
    {
        ed.git_reveal(&t);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file in a review: the git tab lists no hunks for it — they're against the review's base, not the
    /// index, so `s`/`d` there would stage or put back the wrong lines — a note says where they are.
    #[test]
    fn a_reviewed_files_hunks_stay_the_reviews() {
        let top = std::path::Path::new("/r");
        let mut doc = crate::document::Document::from_str(1, "one\nTWO\n");
        doc.path = Some(top.join("a.txt"));
        doc.git_version = doc.version();
        doc.git_hunks = vec![crate::git::Hunk {
            kind: crate::git::HunkKind::Modified,
            lines: 1..2,
            removed: 1,
            base: 1..2,
        }];
        let hunks = |rows: &[Line]| rows.iter().filter(|l| matches!(l.target, Target::Hunk { .. })).count();
        assert_eq!(hunks(&detail_rows(std::slice::from_ref(&doc), top, "a.txt", 1)), 1, "against the index");
        doc.review_base = Some(ropey::Rope::from_str("one\ntwo\n"));
        let rows = detail_rows(std::slice::from_ref(&doc), top, "a.txt", 1);
        assert_eq!(hunks(&rows), 0, "in a review");
        assert!(rows.iter().any(|l| l.kind == Kind::Note && l.label == "in the review"), "{:?}", rows.len());
    }
}
