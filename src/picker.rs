//! Picker — fuzzy list (files `space f`, buffers `space b`, global search results `space /`).
//!
//! The list is filled by a worker thread (file listing and search hit disk or external commands);
//! filtering is nucleo (Helix's matcher). Each keystroke refilters — only the last matches when the query
//! grew, a backspace from the last few queries kept, a big list (50k+) on a worker, scored on every core —
//! and match positions are computed only for visible lines.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use ropey::Rope;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use crate::document::DocId;
use crate::filetree::{Kind, Tree};
use crate::syntax::{self, Syntax};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Open(PathBuf),
    Buffer(DocId),
    /// Index in the code action list (the editor holds it).
    Code(usize),
    /// Command palette: named commands / `:` command line.
    Command(&'static str),
    Typed(String),
    /// Theme selection (previewed while choosing).
    Theme(String),
    /// Entry `index` of the pane's jump list — in open document `doc`, at `line` (for the preview).
    Jump {
        index: usize,
        doc: DocId,
        line: usize,
    },
    /// File `n` of the pending replace-across-files plan (replace.rs).
    ReplaceFile(usize),
    /// A folder of the file picker's tree (relative to its root) — Enter / → opens it.
    Dir(String),
    /// File `n` of the changed-files list (`space g f` — its diff is the preview).
    ChangedFile(usize),
    /// A commit (`space g l`) of the repository at `root` — its diff is the preview, Enter opens it.
    Commit {
        hash: String,
        root: PathBuf,
    },
    /// An earlier chat conversation (claude session `id`) kept in the `store` file — its rows are the preview.
    Conversation {
        id: String,
        store: PathBuf,
    },
    /// A notification of this session (`:messages`) — nothing to do; the list is for reading.
    Message,
    /// An item of a plugin's pick (the prompt token it answers, the item's id).
    Plugin {
        token: u64,
        id: String,
    },
    /// Line `line` (from 0) of the file, byte `col` within that line.
    Goto {
        path: PathBuf,
        line: usize,
        col: usize,
    },
}

/// Job that builds the list on a worker thread.
pub type Fill = Box<dyn FnOnce() -> Result<Vec<Item>, FillError> + Send>;

/// Why a list wasn't built. `Nothing` is news, not a failure ("no earlier conversations") — it's said
/// calmly and goes, where a failure is red and waits for a key.
#[derive(Debug)]
pub enum FillError {
    Nothing(String),
    Failed(String),
}

impl From<String> for FillError {
    fn from(e: String) -> Self {
        FillError::Failed(e)
    }
}

#[derive(Clone, Debug)]
pub struct Item {
    pub label: String,
    pub action: Action,
    /// Dimmed annotation on the right (keys in the command palette etc.) — not used for filtering.
    pub hint: String,
    /// Kind glyph before the name and its color's theme scope (symbol picker: `ƒ` function …).
    pub glyph: Option<(&'static str, &'static str)>,
}

/// One visible line.
#[derive(Clone, Debug)]
pub struct Row {
    pub label: String,
    /// Matched char positions in `label`.
    pub marks: Vec<u32>,
    pub selected: bool,
    /// Dimmed annotation on the right.
    pub hint: String,
    /// The annotation is what the row is for (a state): it keeps its room, the label is cut instead.
    pub hint_first: bool,
    pub glyph: Option<(&'static str, &'static str)>,
    /// A tree row's depth and kind (None = a plain list row).
    pub tree: Option<(usize, Kind)>,
}

/// Matches laid out as a tree while typing, best first — the rest only counted (a tree of all 200k files
/// that `e` matches takes 100 ms to build; nobody scrolls that far).
const TREE_MAX: usize = 2000;

/// Lists at least this long are filtered on a worker (a key never waits for 200k paths to be scored).
const ASYNC_MIN: usize = 50_000;

/// A filtering handed to a worker — `Editor::picker_filter` spawns it; `Picker::filtered` takes the result.
pub struct FilterJob {
    generation: u64,
    items: Arc<Vec<Item>>,
    candidates: Vec<u32>,
    query: String,
    tree: bool,
    config: Config,
}

/// What a filtering found: (generation, query, matches, whether only the best TREE_MAX are in order).
pub type Filtered = (u64, String, Vec<u32>, bool);

impl FilterJob {
    pub fn run(self) -> Filtered {
        let (matches, tail) =
            filter(&self.items, &self.candidates, &self.query, self.tree, &mut Matcher::new(self.config));
        (self.generation, self.query, matches, tail)
    }
}

/// `candidates` of `items` scored against `query`, best first — a tree needs only its best TREE_MAX in
/// order (the flag says so). Any thread; a big list is scored on every core.
fn filter(
    items: &[Item],
    candidates: &[u32],
    query: &str,
    tree: bool,
    matcher: &mut Matcher,
) -> (Vec<u32>, bool) {
    let pat = Pattern::parse(query, CaseMatching::Smart, Normalization::Smart);
    let score = |part: &[u32], matcher: &mut Matcher| -> Vec<(u32, u32)> {
        let mut buf = Vec::new();
        part.iter()
            .filter_map(|&i| {
                pat.score(Utf32Str::new(&items[i as usize].label, &mut buf), matcher).map(|s| (s, i))
            })
            .collect()
    };
    // 200k paths: ~8 ms on one core
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8);
    let mut scored = if candidates.len() < 20_000 || threads < 2 {
        score(candidates, matcher)
    } else {
        let config = matcher.config.clone();
        std::thread::scope(|sc| {
            let parts: Vec<_> = candidates
                .chunks(candidates.len().div_ceil(threads))
                .map(|part| {
                    let config = config.clone();
                    sc.spawn(move || score(part, &mut Matcher::new(config)))
                })
                .collect();
            parts.into_iter().flat_map(|h| h.join().unwrap_or_default()).collect()
        })
    };
    // Highest score first, ties in original order. A tree lays out only the best TREE_MAX — only those
    // need their order (sorting 140k matches is most of a key's time)
    let order = |a: &(u32, u32), b: &(u32, u32)| b.0.cmp(&a.0).then(a.1.cmp(&b.1));
    let tail = tree && scored.len() > TREE_MAX;
    if tail {
        scored.select_nth_unstable_by(TREE_MAX, order);
        scored[..TREE_MAX].sort_unstable_by(order);
    } else {
        scored.sort_unstable_by(order);
    }
    (scored.into_iter().map(|(_, i)| i).collect(), tail)
}

/// Lists of paths shown as a folder tree (filetree.rs), relative to `root`.
struct TreeMode {
    root: PathBuf,
    /// The file picker: with nothing typed, every file, folded by folder.
    browse: bool,
    /// Browsing: the whole list's tree (once it's listed) and the open folders.
    all: Option<Tree>,
    open: HashSet<String>,
    /// A file to show opened up to once the list arrives.
    reveal: Option<String>,
    rows: Vec<crate::filetree::Row>,
    /// Browsing: the folder rows' items (`Action::Dir`) — a row's item past `items` is one of these.
    dirs: Vec<Item>,
}

pub struct Picker {
    /// Number assigned by the editor — so a late list doesn't land in a different (newer) picker.
    pub id: u64,
    pub title: String,
    pub query: String,
    items: Arc<Vec<Item>>,
    /// Filtered items (items indices, by score).
    matches: Vec<u32>,
    /// The query `matches` are for — typing on narrows them instead of scoring every item again.
    matched: Option<String>,
    /// Only the best TREE_MAX matches are in order (a tree shows no more).
    tail_unsorted: bool,
    /// The last few queries' matches — a backspace goes back to one without scoring everything again.
    earlier: Vec<(String, Vec<u32>, bool)>,
    /// A big list is filtered on a worker (the editor's pickers; tests filter in place unless they say) ·
    /// the filtering waiting to be spawned · its generation (a later key's makes an earlier one's moot) ·
    /// one is running (the rows are the last query's until it lands).
    pub asynchronous: bool,
    job: Option<FilterJob>,
    filter_gen: u64,
    pub filtering: bool,
    pub selected: usize,
    pub scroll: usize,
    /// Small window (top center) — for pickers that need the editor behind visible (theme preview).
    pub compact: bool,
    /// Picker that refetches the list when the query changes (workspace symbols — the server filters).
    pub requery: bool,
    /// The list is still being filled.
    pub loading: bool,
    /// Lines visible this frame (render fills them, draw draws them).
    pub view: Vec<Row>,
    /// Tree rows on the visible lines, top to bottom (pinned headings first) — for clicks.
    lines: Vec<usize>,
    /// Whether to use the right-hand preview pane (off when there's no file to show, e.g. code actions).
    pub preview: bool,
    /// Global search results: the pattern (`C-r` replaces the listed matches).
    pub grep: Option<String>,
    /// Preview scrolled this many lines from where it opens (back to 0 when the selection changes).
    pub preview_scroll: usize,
    /// Paths shown as a folder tree (filetree.rs) — `selected`/`scroll` then count its rows.
    tree: Option<TreeMode>,
    /// Put the selection mid-list on the next draw (the tree opening on the file being edited).
    center: bool,
    /// Preview cache (path → loading/content). Filled by a worker thread.
    pub previews: HashMap<PathBuf, Preview>,
    matcher: Matcher,
}

impl Picker {
    pub fn new(title: impl Into<String>, items: Vec<Item>, paths: bool) -> Self {
        let config = if paths { Config::DEFAULT.match_paths() } else { Config::DEFAULT };
        let mut p = Picker {
            id: 0,
            title: title.into(),
            query: String::new(),
            items: Arc::new(items),
            matches: Vec::new(),
            matched: None,
            tail_unsorted: false,
            earlier: Vec::new(),
            asynchronous: false,
            job: None,
            filter_gen: 0,
            filtering: false,
            selected: 0,
            scroll: 0,
            compact: false,
            requery: false,
            loading: false,
            view: Vec::new(),
            lines: Vec::new(),
            preview: true,
            grep: None,
            preview_scroll: 0,
            tree: None,
            center: false,
            previews: HashMap::new(),
            matcher: Matcher::new(config),
        };
        p.refilter();
        p
    }

    pub fn without_preview(mut self) -> Self {
        self.preview = false;
        self
    }

    pub fn set_items(&mut self, items: Vec<Item>) {
        self.set_items_with(items, None);
    }

    /// The file picker's folder to browse — its tree is built with the list, on the worker thread (a
    /// 200k-file tree takes 100 ms).
    pub fn browse_root(&self) -> Option<PathBuf> {
        self.tree.as_ref().filter(|t| t.browse).map(|t| t.root.clone())
    }

    /// `tree` = `browse_tree` of these items, when built already.
    pub fn set_items_with(&mut self, items: Vec<Item>, tree: Option<Tree>) {
        self.loading = false;
        self.items = Arc::new(items);
        self.matched = None;
        self.earlier.clear();
        let reveal = match self.tree.as_mut() {
            Some(tm) if tm.browse => {
                tm.all = Some(tree.unwrap_or_else(|| browse_tree(&tm.root, &self.items)));
                let reveal = tm.reveal.take();
                if let Some(r) = &reveal {
                    let mut up = crate::filetree::parent(r);
                    while let Some(d) = up {
                        tm.open.insert(d.to_string());
                        up = crate::filetree::parent(d);
                    }
                }
                reveal
            }
            _ => None,
        };
        self.sync();
        // Start on the file being edited
        if let Some(r) = reveal {
            let path = self.tree.as_ref().map(|t| t.root.join(&r));
            self.select_where(|a| matches!(a, Action::Open(p) if Some(p) == path.as_ref()));
            self.center = true;
        }
    }

    /// The file picker: files under `root` as a folder tree — folded while nothing is typed, opened up to
    /// `reveal` (relative) — and, while typing, the matches under their folders.
    pub fn with_tree(mut self, root: PathBuf, reveal: Option<String>) -> Self {
        self.tree = Some(TreeMode {
            root,
            browse: true,
            all: None,
            open: HashSet::new(),
            reveal,
            rows: Vec::new(),
            dirs: Vec::new(),
        });
        self.sync();
        self
    }

    /// Items that are paths (or a path and what's at it — `src/a.rs:12: text`) listed under their folders,
    /// in the list's order (best match first while typing).
    pub fn grouped(mut self, root: PathBuf) -> Self {
        self.tree = Some(TreeMode {
            root,
            browse: false,
            all: None,
            open: HashSet::new(),
            reveal: None,
            rows: Vec::new(),
            dirs: Vec::new(),
        });
        self.sync();
        self
    }

    /// Refilter, and lay the matches out as tree rows when it's a tree (once they're in, if a worker filters).
    fn sync(&mut self) {
        if self.refilter() {
            self.layout();
        }
    }

    /// The matches as tree rows, when it's a tree.
    fn layout(&mut self) {
        let Some(tm) = self.tree.as_mut() else { return };
        tm.dirs.clear();
        tm.rows = if tm.browse && self.query.is_empty() {
            tm.all.as_ref().map(|t| t.rows(Some(&tm.open))).unwrap_or_default()
        } else {
            let root = &tm.root;
            let items = &self.items;
            let entries = self.matches.iter().take(TREE_MAX).map(|&i| {
                let it = &items[i as usize];
                (i as usize, it.label.as_str(), crate::filetree::path_len(root, it))
            });
            Tree::build(entries, false).rows(None)
        };
        // Browsing: folders are picked too (Enter / → opens one)
        if tm.browse && self.query.is_empty() {
            let n = self.items.len();
            for row in &mut tm.rows {
                if let Some(rel) = row.dir.clone() {
                    row.item = Some(n + tm.dirs.len());
                    tm.dirs.push(Item {
                        label: rel.rsplit('/').next().unwrap_or(&rel).to_string(),
                        action: Action::Dir(rel),
                        hint: row.count.to_string(),
                        glyph: None,
                    });
                }
            }
        }
        self.selected = self.selected.min(tm.rows.len().saturating_sub(1));
        self.settle(1);
    }

    /// Showing the browsing tree (file picker, nothing typed).
    pub fn in_tree(&self) -> bool {
        self.query.is_empty() && self.tree.as_ref().is_some_and(|t| t.browse && t.all.is_some())
    }

    /// Rows listed (tree rows — folders and headings included — or matches).
    pub fn len(&self) -> usize {
        match &self.tree {
            Some(tm) => tm.rows.len(),
            None => self.matches.len(),
        }
    }

    fn pickable(&self, row: usize) -> bool {
        match &self.tree {
            Some(tm) => tm.rows.get(row).is_some_and(|r| r.item.is_some()),
            None => row < self.matches.len(),
        }
    }

    /// Off a heading onto the nearest row that picks something — `dir` first, then the other way.
    fn settle(&mut self, dir: isize) {
        let n = self.len();
        if n == 0 || self.pickable(self.selected) {
            return;
        }
        let from = self.selected as isize;
        for d in [dir, -dir] {
            let mut k = from + d;
            while (0..n as isize).contains(&k) {
                if self.pickable(k as usize) {
                    self.selected = k as usize;
                    return;
                }
                k += d;
            }
        }
    }

    /// A click on row `row`: selects it if it picks something.
    pub fn select_row(&mut self, row: usize) -> bool {
        if !self.pickable(row) {
            return false;
        }
        self.selected = row;
        self.preview_scroll = 0;
        true
    }

    fn item(&self, i: usize) -> Option<&Item> {
        match self.items.get(i) {
            Some(it) => Some(it),
            None => self.tree.as_ref()?.dirs.get(i - self.items.len()),
        }
    }

    fn select_where(&mut self, f: impl Fn(&Action) -> bool) {
        let at = match &self.tree {
            Some(tm) => {
                tm.rows.iter().position(|r| r.item.and_then(|i| self.item(i)).is_some_and(|it| f(&it.action)))
            }
            None => self.matches.iter().position(|&i| f(&self.items[i as usize].action)),
        };
        if let Some(k) = at {
            self.selected = k;
        }
    }

    /// The selected row's folder (a folder row) / relative path (a file row).
    fn current_rel(&self) -> Option<(String, bool)> {
        let root = &self.tree.as_ref()?.root;
        match &self.current()?.action {
            Action::Dir(rel) => Some((rel.clone(), true)),
            Action::Open(p) => Some((p.strip_prefix(root).ok()?.to_string_lossy().replace('\\', "/"), false)),
            _ => None,
        }
    }

    fn is_open(&self, rel: &str) -> bool {
        self.tree.as_ref().is_some_and(|t| t.open.contains(rel))
    }

    /// Enter on a folder: open / fold it, staying on it.
    pub fn tree_toggle(&mut self) {
        let Some((rel, true)) = self.current_rel() else { return };
        if let Some(tm) = self.tree.as_mut()
            && !tm.open.remove(&rel)
        {
            tm.open.insert(rel.clone());
        }
        self.sync();
        self.select_where(|a| matches!(a, Action::Dir(d) if *d == rel));
    }

    /// → : open a folded folder; on an open one, step to its first entry.
    pub fn tree_right(&mut self) {
        match self.current_rel() {
            Some((rel, true)) if !self.is_open(&rel) => self.tree_toggle(),
            Some((_, true)) => self.move_by(1),
            _ => {}
        }
    }

    /// ← : fold an open folder; otherwise go to the folder row it's in (past folders sharing a row).
    pub fn tree_left(&mut self) {
        let Some((rel, dir)) = self.current_rel() else { return };
        if dir && self.is_open(&rel) {
            return self.tree_toggle();
        }
        let mut up = crate::filetree::parent(&rel);
        while let Some(d) = up {
            let before = self.selected;
            self.select_where(|a| matches!(a, Action::Dir(x) if x == d));
            if self.selected != before {
                return;
            }
            up = crate::filetree::parent(d);
        }
    }

    /// Filter the items by the query — false when that was handed to a worker (the rows stay the last
    /// query's until `filtered`).
    pub fn refilter(&mut self) -> bool {
        self.preview_scroll = 0;
        if self.query.is_empty() {
            self.cancel_filter();
            self.matched = None;
            self.tail_unsorted = false;
            self.matches = (0..self.items.len() as u32).collect();
            self.selected = self.selected.min(self.matches.len().saturating_sub(1));
            return true;
        }
        if let Some(i) = self.earlier.iter().position(|e| e.0 == self.query) {
            self.cancel_filter();
            let (_, matches, tail) = self.earlier.remove(i);
            self.remember();
            (self.matches, self.tail_unsorted) = (matches, tail);
            self.matched = Some(self.query.clone());
            self.selected = self.selected.min(self.matches.len().saturating_sub(1));
            return true;
        }
        // A longer query matches a subset of what the shorter one did — unless it has the pattern syntax
        // (`!` negates, a lone `'` `^` `$` is a literal until something follows)
        let plain = |q: &str| !q.contains(['!', '\'', '^', '$', '\\']);
        let narrow = self
            .matched
            .as_deref()
            .is_some_and(|m| !m.is_empty() && self.query.starts_with(m) && plain(m) && plain(&self.query));
        let candidates: Vec<u32> =
            if narrow { self.matches.clone() } else { (0..self.items.len() as u32).collect() };
        if self.asynchronous && candidates.len() >= ASYNC_MIN {
            self.filter_gen += 1;
            self.job = Some(FilterJob {
                generation: self.filter_gen,
                items: self.items.clone(),
                candidates,
                query: self.query.clone(),
                tree: self.tree.is_some(),
                config: self.matcher.config.clone(),
            });
            self.filtering = true;
            return false;
        }
        self.cancel_filter();
        self.remember();
        let (matches, tail) =
            filter(&self.items, &candidates, &self.query, self.tree.is_some(), &mut self.matcher);
        (self.matches, self.tail_unsorted) = (matches, tail);
        self.matched = Some(self.query.clone());
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
        true
    }

    /// A filtering to run on a worker, if a key asked for one.
    pub fn take_filter_job(&mut self) -> Option<FilterJob> {
        self.job.take()
    }

    /// A worker's filtering landed — taken if no later key asked for another.
    pub fn filtered(&mut self, (generation, query, matches, tail): Filtered) {
        if generation != self.filter_gen || !self.filtering {
            return;
        }
        self.filtering = false;
        self.remember();
        (self.matches, self.tail_unsorted) = (matches, tail);
        self.matched = Some(query);
        self.selected = self.selected.min(self.matches.len().saturating_sub(1));
        self.layout();
    }

    /// A key that acts on the list (Enter, a move, `C-q`) can't act on the last query's rows: a filtering
    /// still going is done here, now.
    pub fn filter_now(&mut self) {
        if self.filtering {
            let asynchronous = std::mem::replace(&mut self.asynchronous, false);
            self.sync();
            self.asynchronous = asynchronous;
        }
    }

    /// A filtering waiting or running isn't wanted any more.
    fn cancel_filter(&mut self) {
        if self.filtering || self.job.is_some() {
            self.filter_gen += 1;
            self.filtering = false;
            self.job = None;
        }
    }

    /// Keep the matches about to be replaced, for a backspace back to their query (the last 8).
    fn remember(&mut self) {
        if let Some(q) = self.matched.clone() {
            self.earlier.retain(|e| e.0 != q);
            self.earlier.push((q, self.matches.clone(), self.tail_unsorted));
            if self.earlier.len() > 8 {
                self.earlier.remove(0);
            }
        }
    }

    pub fn push(&mut self, c: char) {
        self.query.push(c);
        self.selected = 0;
        self.sync();
    }

    /// Pasted text joins the query — filtered once, not per character.
    pub fn push_str(&mut self, s: &str) {
        self.query.push_str(s);
        self.selected = 0;
        self.sync();
    }

    pub fn pop(&mut self) {
        self.query.pop();
        self.selected = 0;
        self.sync();
    }

    /// Moves `delta` rows that pick something (headings are stepped over), wrapping around.
    pub fn move_by(&mut self, delta: isize) {
        let n = self.len();
        if n == 0 {
            return;
        }
        self.preview_scroll = 0;
        let step = delta.signum();
        for _ in 0..delta.unsigned_abs() {
            let from = self.selected;
            for k in 1..=n as isize {
                let at = (from as isize + step * k).rem_euclid(n as isize) as usize;
                if self.pickable(at) {
                    self.selected = at;
                    break;
                }
            }
        }
    }

    /// Items the query leaves listed, best first (past what a tree shows: in the list's order).
    pub fn shown(&self) -> impl Iterator<Item = &Item> {
        let mut m = self.matches.clone();
        if self.tail_unsorted {
            m[TREE_MAX..].sort_unstable();
        }
        m.into_iter().map(|i| &self.items[i as usize])
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn current(&self) -> Option<&Item> {
        match &self.tree {
            Some(tm) => self.item(tm.rows.get(self.selected)?.item?),
            None => self.matches.get(self.selected).map(|&i| &self.items[i as usize]),
        }
    }

    /// (items matching, items) — a tree's folders and headings aren't counted.
    pub fn counts(&self) -> (usize, usize) {
        (self.matches.len(), self.items.len())
    }

    /// Visible lines. Scrolls so the selection shows.
    pub fn visible(&mut self, rows: usize) -> Vec<Row> {
        if std::mem::take(&mut self.center) {
            self.scroll = self.selected.saturating_sub(rows / 2);
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
            // …with the headings just above it, so a match never shows without its folder
            while self.scroll > 0 && !self.pickable(self.scroll - 1) && self.selected + 1 - self.scroll < rows
            {
                self.scroll -= 1;
            }
        } else if self.selected >= self.scroll + rows {
            self.scroll = self.selected + 1 - rows;
        }
        // Never leave rows empty at the bottom while there are rows above (after a list shrinks)
        self.scroll = self.scroll.min(self.len().saturating_sub(rows));
        let pat = Pattern::parse(&self.query, CaseMatching::Smart, Normalization::Smart);
        let mut buf = Vec::new();
        let (scroll, selected) = (self.scroll, self.selected);
        let mut marks = |label: &str, matcher: &mut Matcher| {
            let mut idx = Vec::new();
            pat.indices(Utf32Str::new(label, &mut buf), matcher, &mut idx);
            idx.sort_unstable();
            idx.dedup();
            idx
        };
        let Some(tm) = &self.tree else {
            return self
                .matches
                .iter()
                .enumerate()
                .skip(scroll)
                .take(rows)
                .map(|(row, &i)| {
                    let item = &self.items[i as usize];
                    Row {
                        label: item.label.clone(),
                        marks: marks(&item.label, &mut self.matcher),
                        selected: row == selected,
                        hint: item.hint.clone(),
                        hint_first: false,
                        glyph: item.glyph,
                        tree: None,
                    }
                })
                .collect();
        };
        // Folders (and a hit's file) that scrolled away stay pinned at the top, over the rows under them —
        // settled together with the scroll, so the selection never hides behind them
        let n = tm.rows.len();
        let mut pins: Vec<usize> = Vec::new();
        // (nothing listed — an empty folder, one still being listed, a query nothing matches: no headings)
        for _ in 0..if n == 0 { 0 } else { 4 } {
            let at = (self.scroll + pins.len()).min(n.saturating_sub(1));
            let next = crate::filetree::enclosing(|i| tm.rows[i].depth, at, rows / 2);
            let settled = next.len() == pins.len();
            pins = next;
            if self.selected < self.scroll + pins.len() {
                self.scroll = self.selected.saturating_sub(pins.len());
            } else if settled {
                break;
            }
        }
        let scroll = self.scroll;
        let lines: Vec<usize> = pins
            .iter()
            .copied()
            .chain((scroll + pins.len()..n).take(rows.saturating_sub(pins.len())))
            .collect();
        // A tree row shows a piece of its source's label: its marks are the label's, shifted
        let typed = !self.query.is_empty();
        let mut out = Vec::with_capacity(rows);
        for &k in &lines {
            let r = &tm.rows[k];
            let source = &self.items[r.source].label;
            let label: String = source.chars().skip(r.span.start).take(r.span.len()).collect();
            let marks = if typed {
                marks(source, &mut self.matcher)
                    .into_iter()
                    .filter(|&m| r.span.contains(&(m as usize)))
                    .map(|m| m - r.span.start as u32)
                    .collect()
            } else {
                Vec::new()
            };
            let item = r.item.and_then(|i| self.items.get(i));
            let hint = match (r.kind, item) {
                (Kind::File | Kind::Hit, Some(it)) => it.hint.clone(),
                // A heading folder's count is noise; a folded one's, and a file's hits, aren't
                (Kind::Dir { open: None }, _) => String::new(),
                _ if r.count > 0 => r.count.to_string(),
                _ => String::new(),
            };
            out.push(Row {
                label,
                marks,
                selected: k == selected,
                hint,
                hint_first: false,
                glyph: item.and_then(|it| it.glyph),
                tree: Some((r.depth, r.kind)),
            });
        }
        self.lines = lines;
        out
    }

    /// The row shown on visible line `line` (pinned headings included).
    pub fn row_at(&self, line: usize) -> Option<usize> {
        match &self.tree {
            Some(_) => self.lines.get(line).copied(),
            None => Some(self.scroll + line),
        }
    }
}

/// Every file under `root`, by folder, for browsing.
pub fn browse_tree(root: &Path, items: &[Item]) -> Tree {
    let entries =
        items.iter().enumerate().map(|(i, it)| (i, it.label.as_str(), crate::filetree::path_len(root, it)));
    Tree::build(entries, true)
}

// ── Preview ──────────────────────────────────────────────────────────────────

pub enum Preview {
    Loading,
    Ready { text: Rope, syntax: Option<Box<Syntax>> },
    Unavailable(&'static str),
}

impl Action {
    /// File to preview and the line to highlight.
    pub fn preview_target(&self) -> Option<(&Path, Option<usize>)> {
        match self {
            Action::Open(p) => Some((p, None)),
            Action::Goto { path, line, .. } => Some((path, Some(*line))),
            Action::Buffer(_)
            | Action::Jump { .. }
            | Action::ReplaceFile(_)
            | Action::Dir(_)
            | Action::ChangedFile(_)
            | Action::Commit { .. }
            | Action::Conversation { .. }
            | Action::Message
            | Action::Plugin { .. }
            | Action::Code(_)
            | Action::Command(_)
            | Action::Typed(_)
            | Action::Theme(_) => None,
        }
    }
}

impl Action {
    /// Where its preview is kept — the file's path, `commit:<hash>` for a commit.
    pub fn preview_key(&self) -> Option<PathBuf> {
        match self {
            Action::Commit { hash, root } => Some(PathBuf::from(format!("commit:{hash}@{}", root.display()))),
            Action::Conversation { id, store } => {
                Some(PathBuf::from(format!("chat:{id}@{}", store.display())))
            }
            a => a.preview_target().map(|(p, _)| p.to_path_buf()),
        }
    }
}

const PREVIEW_BYTES: usize = 512 * 1024;
pub const PREVIEW_CACHE: usize = 64;

/// On a worker thread — read just the head and, if the language is known, parse it too.
pub fn load_preview(path: &Path) -> Preview {
    if let Some((hash, root)) = path.to_str().and_then(|s| s.strip_prefix("commit:")?.split_once('@')) {
        return commit_preview(Path::new(root), hash);
    }
    if let Some((id, store)) = path.to_str().and_then(|s| s.strip_prefix("chat:")?.split_once('@')) {
        let Some(text) = crate::chats::transcript(Path::new(store), id) else {
            return Preview::Unavailable("not kept any more");
        };
        let text = Rope::from_str(&text);
        let syntax = syntax::spec("markdown")
            .and_then(|spec| syntax::Loader::global().load(spec).ok())
            .map(|lang| Box::new(Syntax::parsed(lang, &text)));
        return Preview::Ready { text, syntax };
    }
    let Ok(bytes) = std::fs::read(path) else { return Preview::Unavailable("can't read") };
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return Preview::Unavailable("binary file");
    }
    let mut end = bytes.len().min(PREVIEW_BYTES);
    while end < bytes.len() && (bytes[end] & 0xC0) == 0x80 {
        end -= 1; // don't cut mid-char
    }
    let Ok(s) = std::str::from_utf8(&bytes[..end]) else { return Preview::Unavailable("not UTF-8") };
    let text = Rope::from_str(s);
    let syntax = syntax::detect(path)
        .and_then(|spec| syntax::Loader::global().load(spec).ok())
        .map(|lang| Box::new(Syntax::parsed(lang, &text)));
    Preview::Ready { text, syntax }
}

/// A commit's `git show`, colored as a diff.
fn commit_preview(root: &Path, hash: &str) -> Preview {
    let Ok(text) = crate::gitpanel::show(root, hash) else { return Preview::Unavailable("git show failed") };
    let text = Rope::from_str(&text);
    let syntax = syntax::spec("diff")
        .and_then(|spec| syntax::Loader::global().load(spec).ok())
        .map(|lang| Box::new(Syntax::parsed(lang, &text)));
    Preview::Ready { text, syntax }
}

// ── Building lists (worker thread) ───────────────────────────────────────────

/// File list — `git ls-files` in a git repo (respects .gitignore, fast), else walk the tree directly.
pub fn list_files(root: &Path) -> Vec<PathBuf> {
    let git = Command::new("git")
        .args(["ls-files", "--cached", "--others", "--exclude-standard", "-z"])
        .current_dir(root)
        .output();
    if let Ok(out) = git
        && out.status.success()
    {
        let listed: Vec<PathBuf> = out
            .stdout
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| PathBuf::from(String::from_utf8_lossy(s).into_owned()))
            .collect();
        // Only what's there as a file (not deleted since the index, not a submodule's folder) — a stat
        // each, on every core: one after another that was most of listing a big repo
        let parts = std::thread::available_parallelism().map_or(4, |n| n.get()).min(16);
        let chunk = listed.len().div_ceil(parts).max(2048);
        let keep: Vec<Vec<bool>> = std::thread::scope(|s| {
            let handles: Vec<_> = listed
                .chunks(chunk)
                .map(|c| s.spawn(move || c.iter().map(|p| root.join(p).is_file()).collect::<Vec<bool>>()))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap_or_default()).collect()
        });
        return listed
            .into_iter()
            .zip(keep.into_iter().flatten())
            .filter(|(_, k)| *k)
            .map(|(p, _)| p)
            .collect();
    }
    let mut out = Vec::new();
    walk(root, Path::new(""), &mut out);
    out
}

const SKIP_DIRS: &[&str] = &["target", "node_modules", ".git", ".venv", "__pycache__"];
const MAX_FILES: usize = 200_000;

fn walk(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root.join(rel)) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if out.len() >= MAX_FILES {
            return;
        }
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || SKIP_DIRS.contains(&name.as_ref()) {
            continue;
        }
        let path = rel.join(name.as_ref());
        match e.file_type() {
            Ok(t) if t.is_dir() => walk(root, &path, out),
            Ok(t) if t.is_file() => out.push(path),
            _ => {}
        }
    }
}

pub fn file_items(root: &Path) -> Vec<Item> {
    list_files(root)
        .into_iter()
        .map(|rel| Item {
            label: rel.to_string_lossy().into_owned(),
            action: Action::Open(root.join(rel)),
            hint: String::new(),
            glyph: None,
        })
        .collect()
}

const MAX_RESULTS: usize = 10_000;

/// Global search — ripgrep if `rg` exists (smart case, respects .gitignore), else built-in regex scan.
pub fn grep(root: &Path, pattern: &str) -> Result<Vec<Item>, String> {
    use std::io::{BufRead, Read};
    let rg = Command::new("rg")
        .args([
            "--vimgrep",
            "--no-heading",
            "--color=never",
            "--smart-case",
            "--max-columns=240",
            "-e",
            pattern,
        ])
        .current_dir(root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();
    let Ok(mut rg) = rg else { return grep_builtin(root, pattern) };
    // Read as it comes and stopped at the limit — a common word in a big tree was held whole (and searched
    // to the end) before keeping the first 10k
    let err = rg.stderr.take().map(|mut e| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = e.read_to_string(&mut s);
            s
        })
    });
    let mut items = Vec::new();
    if let Some(out) = rg.stdout.take() {
        for line in std::io::BufReader::new(out).split(b'\n').map_while(Result::ok) {
            items.extend(parse_vimgrep(root, &String::from_utf8_lossy(&line)));
            if items.len() == MAX_RESULTS {
                let _ = rg.kill();
                let _ = rg.wait();
                return Ok(items);
            }
        }
    }
    let status = rg.wait().map_err(|e| e.to_string())?;
    let err = err.and_then(|h| h.join().ok()).unwrap_or_default();
    match status.code() {
        Some(0 | 1) => Ok(items),
        _ => Err(err.lines().next().unwrap_or("rg failed").to_string()),
    }
}

/// `path:line:col:text` (line and col from 1, col in bytes).
fn parse_vimgrep(root: &Path, l: &str) -> Option<Item> {
    let mut it = l.splitn(4, ':');
    let (path, line, col, text) = (it.next()?, it.next()?, it.next()?, it.next().unwrap_or(""));
    let (line, col): (usize, usize) = (line.parse().ok()?, col.parse().ok()?);
    Some(Item {
        label: format!("{path}:{line}: {}", text.trim()),
        action: Action::Goto { path: root.join(path), line: line - 1, col: col - 1 },
        hint: String::new(),
        glyph: None,
    })
}

fn grep_builtin(root: &Path, pattern: &str) -> Result<Vec<Item>, String> {
    let re = regex::RegexBuilder::new(pattern)
        .case_insensitive(!pattern.chars().any(char::is_uppercase))
        .build()
        .map_err(|e| e.to_string().lines().last().unwrap_or("invalid regex").trim().to_string())?;
    let mut out = Vec::new();
    for rel in list_files(root) {
        let Ok(src) = std::fs::read_to_string(root.join(&rel)) else { continue }; // skip binary and non-UTF-8
        for (i, line) in src.lines().enumerate() {
            if let Some(m) = re.find(line) {
                out.push(Item {
                    label: format!("{}:{}: {}", rel.display(), i + 1, line.trim()),
                    action: Action::Goto { path: root.join(&rel), line: i, col: m.start() },
                    hint: String::new(),
                    glyph: None,
                });
                if out.len() >= MAX_RESULTS {
                    return Ok(out);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn items(labels: &[&str]) -> Vec<Item> {
        labels
            .iter()
            .map(|l| Item {
                label: l.to_string(),
                action: Action::Open(PathBuf::from(l)),
                hint: String::new(),
                glyph: None,
            })
            .collect()
    }

    #[test]
    fn fuzzy_filters_and_ranks() {
        let mut p =
            Picker::new("files", items(&["src/main.rs", "src/editor.rs", "README.md", "src/term.rs"]), true);
        assert_eq!(p.counts(), (4, 4));
        for c in "edrs".chars() {
            p.push(c);
        }
        assert_eq!(p.current().map(|i| i.label.as_str()), Some("src/editor.rs"));
        let vis = p.visible(10);
        assert_eq!(vis[0].label, "src/editor.rs");
        assert!(!vis[0].marks.is_empty(), "highlighting needs match positions");
        p.pop();
        p.pop();
        p.pop();
        p.pop();
        assert_eq!(p.counts().0, 4);
        p.move_by(-1);
        assert_eq!(p.current().map(|i| i.label.as_str()), Some("src/term.rs"), "wraps around upward");
    }

    /// Hits (a path and what's at it) hang under their file, under its folders; headings aren't picked —
    /// not by moving, not by a click — and typing keeps the shape.
    #[test]
    fn grouped_hits_under_their_files() {
        let root = PathBuf::from("/r");
        let hit = |rel: &str, line: usize, text: &str| Item {
            label: format!("{rel}:{}: {text}", line + 1),
            action: Action::Goto { path: root.join(rel), line, col: 0 },
            hint: String::new(),
            glyph: None,
        };
        let mut p = Picker::new("grep", Vec::new(), false).grouped(root.clone());
        p.set_items(vec![hit("src/a.rs", 2, "let x"), hit("src/a.rs", 8, "x + 1"), hit("b.rs", 0, "y")]);
        let rows = |p: &mut Picker| -> Vec<String> {
            p.visible(10).iter().map(|r| format!("{}{}", "  ".repeat(r.tree.unwrap().0), r.label)).collect()
        };
        assert_eq!(rows(&mut p), ["src", "  a.rs", "    3: let x", "    9: x + 1", "b.rs", "  1: y"]);
        assert_eq!(p.current().map(|i| i.label.as_str()), Some("src/a.rs:3: let x"));
        assert!(!p.select_row(1), "a click on a heading picks nothing");
        p.move_by(1);
        p.move_by(1);
        assert_eq!(p.current().map(|i| i.label.as_str()), Some("b.rs:1: y"), "past the headings");
        p.push('y');
        assert_eq!(rows(&mut p), ["b.rs", "  1: y"]);
        assert_eq!(p.counts(), (1, 3));
        let marks = &p.visible(10)[1].marks;
        assert_eq!(marks, &[3], "the label's match lands on the hit row, shifted");
    }

    /// Scrolled deep into a folder, its heading (and a hit's file) stays pinned at the top; clicks on the
    /// lines under the pins land on the rows shown there.
    #[test]
    fn scrolled_away_headings_stay_pinned() {
        let root = PathBuf::from("/r");
        let hit = |line: usize| Item {
            label: format!("src/a.rs:{}: x", line + 1),
            action: Action::Goto { path: root.join("src/a.rs"), line, col: 0 },
            hint: String::new(),
            glyph: None,
        };
        let mut p = Picker::new("grep", (0..30).map(hit).collect(), false).grouped(root.clone());
        p.move_by(20);
        let rows: Vec<String> = p.visible(6).iter().map(|r| r.label.clone()).collect();
        assert_eq!(rows[..3], ["src", "a.rs", "18: x"]);
        assert_eq!(rows.last().map(String::as_str), Some("21: x"), "the selection shows, under the pins");
        assert_eq!(p.row_at(0), Some(0));
        assert!(p.row_at(5).is_some_and(|r| p.select_row(r)));
        assert_eq!(p.current().map(|i| i.label.as_str()), Some("src/a.rs:21: x"));
    }

    #[test]
    fn vimgrep_lines() {
        let it = parse_vimgrep(Path::new("/r"), "src/a.rs:12:5:    let 타래 = 1;").unwrap();
        assert_eq!(it.action, Action::Goto { path: PathBuf::from("/r/src/a.rs"), line: 11, col: 4 });
        assert_eq!(it.label, "src/a.rs:12: let 타래 = 1;");
    }

    #[test]
    fn builtin_grep_and_walk() {
        let dir = std::env::temp_dir().join(format!("tarae-grep-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("a.txt"), "hello\n타래 world\n").unwrap();
        std::fs::write(dir.join("sub/b.txt"), "no\nWorld\n").unwrap();
        std::fs::write(dir.join("target/skip.txt"), "world\n").unwrap();
        let hits = grep_builtin(&dir, "world").unwrap();
        let labels: Vec<_> = hits.iter().map(|i| i.label.clone()).collect();
        assert_eq!(
            labels,
            vec!["a.txt:2: 타래 world", "sub/b.txt:2: World"],
            "lowercase = case-insensitive, target skipped"
        );
        assert_eq!(hits[0].action, Action::Goto { path: dir.join("a.txt"), line: 1, col: 7 }, "col in bytes");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// A big list is filtered by a worker: the key only hands the job over, the result lands if no later
    /// key asked for another, and a key that acts on the list gets this query's rows at once.
    #[test]
    fn a_big_list_is_filtered_on_a_worker() {
        let items = || -> Vec<Item> {
            (0..60_000)
                .map(|i| Item {
                    label: format!("d{}/f{i}.rs", i % 50),
                    action: Action::Dir(i.to_string()),
                    hint: String::new(),
                    glyph: None,
                })
                .collect()
        };
        let mut fresh = Picker::new("f", items(), true).grouped(PathBuf::from("/x"));
        fresh.push('d');
        fresh.push('7');
        let want = fresh.matches.clone();
        let mut p = Picker::new("f", items(), true).grouped(PathBuf::from("/x"));
        p.asynchronous = true;
        p.push('d');
        assert!(p.filtering && p.matches.len() == 60_000, "the last query's rows meanwhile");
        let first = p.take_filter_job().unwrap();
        p.push('7');
        let second = p.take_filter_job().unwrap();
        p.filtered(first.run());
        assert!(p.filtering, "an earlier key's result isn't taken");
        p.filtered(second.run());
        assert!(!p.filtering);
        // (past the best TREE_MAX the order isn't kept — a tree shows no more)
        let head = |m: &[u32]| m[..TREE_MAX.min(m.len())].to_vec();
        let set = |m: &[u32]| m.iter().copied().collect::<HashSet<u32>>();
        assert_eq!(head(&p.matches), head(&want), "the same as filtering in place");
        assert_eq!(set(&p.matches), set(&want));
        // A key that acts: done now; the worker's late result is moot
        p.pop();
        p.pop();
        p.push('f'); // (all of them again — a worker's)
        let late = p.take_filter_job().unwrap();
        p.filter_now();
        assert!(!p.filtering);
        let now = p.matches.clone();
        p.filtered(late.run());
        assert_eq!(p.matches, now);
    }

    /// A tree with nothing in it draws (an empty folder, a folder still being listed, a query nothing
    /// matches) — the headings pinned over the top once read row 0 of an empty list.
    #[test]
    fn an_empty_tree_draws() {
        let tree = || Picker::new("files", Vec::new(), true).with_tree(PathBuf::from("/x"), None);
        assert!(tree().visible(10).is_empty(), "still listing");
        let mut p = tree();
        p.set_items(Vec::new());
        assert!(p.visible(10).is_empty(), "an empty folder");
        let item = Item {
            label: "a.rs".into(),
            action: Action::Open("/x/a.rs".into()),
            hint: String::new(),
            glyph: None,
        };
        let mut p = tree();
        p.set_items(vec![item]);
        p.push('z');
        assert!(p.visible(10).is_empty(), "nothing matches");
        let mut p = Picker::new("hits", Vec::new(), true).grouped(PathBuf::from("/x"));
        assert!(p.visible(10).is_empty(), "a grouped list with nothing yet");
    }

    /// Typing on narrows the last matches — the same list as filtering everything anew; a query with the
    /// pattern syntax (a negation) filters everything again.
    #[test]
    fn narrowing_gives_the_same_matches() {
        let item = |l: &str| Item {
            label: l.into(),
            action: Action::Dir(l.into()),
            hint: String::new(),
            glyph: None,
        };
        let labels =
            ["src/editor.rs", "src/term.rs", "src/terminal.rs", "tests/e2e.rs", "README.md", "docs/term.md"];
        let mut typed = Picker::new("t", labels.iter().map(|l| item(l)).collect(), true);
        for q in ["t", "te", "ter", "term", "term ", "term !", "term !r", "term !rs"] {
            typed.push(q.chars().last().unwrap());
            let mut fresh = Picker::new("t", labels.iter().map(|l| item(l)).collect(), true);
            fresh.query = q.into();
            fresh.refilter();
            assert_eq!(typed.matches, fresh.matches, "{q:?}");
        }
        assert_eq!(typed.matches.len(), 1, "term, without rs: {:?}", typed.matches);
        // Backspaces go back through the same lists (kept, not scored again)
        for q in ["term !r", "term !", "term ", "term", "ter", "te", "t"] {
            typed.pop();
            let mut fresh = Picker::new("t", labels.iter().map(|l| item(l)).collect(), true);
            fresh.query = q.into();
            fresh.refilter();
            assert_eq!(typed.matches, fresh.matches, "back to {q:?}");
        }
    }
}
