//! View mode — a cursor on what a surface shows (`target/sb/quote-plan.md` §3). The chat's is normal mode on its
//! transcript, a document of its own (`chatdoc.rs`, panes P2c); here the words they share (`Surface`, `Was`,
//! `has_keys`, `bound`) and the terminal's: its rows as a read-only document, Helix's moves, selections, search and
//! counts on it (`COMMANDS`, the keymap's `[view]` tables), drawn by its own drawer with the selections painted
//! over its cells (`term::view_marks`). `C-\` from its program, `i` back; `esc` or `C-\` to the code (tarae's keys
//! on its panel are its view mode). It holds still while it lasts — its program's output kept back
//! (`Terminal::hold`) — `G` catches up. (The terminal becomes a terminal buffer after the engine, §2.1e: this
//! module folds away then.)
//!
//! The document is in no pane and never the front buffer: each key's command runs with it swapped in as
//! `current` (`swap`) and put back before the key's handler returns — frames, jobs, servers and `context()` see
//! the pane's document only.

use crate::docpane::Keys;
use crate::document::Document;
use crate::editor::{Editor, Mode};
use crate::key::{Code, Key};
use crate::keymap::Lookup;
use crate::movement as mv;
use crate::selection::{Range, Selection};
use crate::session::At;

/// What has a view mode: the chat's conversation (normal mode on its transcript — `chatdoc.rs`), the terminal's
/// scrollback and screen. (The run panels' output is a document of its own with normal mode's keys — `rundoc.rs`.)
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Surface {
    Chat,
    Terminal,
}

/// The keymap's view tables over `[view]`: `[view.chat]`, `[view.terminal]`.
pub const TABLES: &[&str] = &["chat", "terminal"];

impl Surface {
    /// Its name in the state (`view:chat`) and the status line (`● normal · chat`, `● view · terminal`).
    pub fn name(self) -> &'static str {
        match self {
            Surface::Chat => "chat",
            Surface::Terminal => "terminal",
        }
    }

    /// Its table in the keymap (`[view.chat]`).
    pub fn table(self) -> &'static str {
        self.name()
    }
}

/// Commands of the editor's that view mode's `space` has (`[view.terminal.space]`; the chat's `space` is the
/// editor's menu): run once the key's command is done and the view's document put back — they open a picker or go
/// to the chat, never on the view's buffer.
pub const EDITORS: &[&str] = &["chat_open", "inbox", "command_palette"];

/// Where the keys were when the wheel brought a view mode — back there when the wheel takes it back down,
/// nothing selected (§9 Q9): the code, the chat's input, the terminal's program, or another surface's view mode
/// (made again: the chat's where you were, the terminal's at the rows in sight).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Was {
    Code,
    Chat,
    Program,
    View(Surface),
}

/// The terminal's view, as it was made: its program's run, its size, its rows of scrollback above the screen,
/// its document's lines.
#[derive(Clone, Copy, Debug)]
struct Made {
    generation: u64,
    size: (u16, u16),
    back: usize,
    lines: usize,
}

/// Where the terminal's view puts its cursor.
#[derive(Clone, Copy, Debug)]
enum Land {
    /// The program's cursor row when nothing is scrolled back (its prompt), else the last row in sight.
    InSight,
    /// The row `n` of what's in sight (under the wheel's pointer).
    Row(usize),
    /// The latest: the last row with text, never above the program's cursor.
    Latest,
}

/// Normal mode's commands view mode runs (the rest of a key there is said, not done): moves, selections,
/// search, the `z` views, counts, a copy — nothing that edits, opens or goes elsewhere. `K` stays out (a panel's
/// own, §9 Q4).
pub const COMMANDS: &[&str] = &[
    "move_char_left",
    "move_char_right",
    "move_line_up",
    "move_line_down",
    "move_visual_line_up",
    "move_visual_line_down",
    "extend_char_left",
    "extend_char_right",
    "extend_line_up",
    "extend_line_down",
    "extend_visual_line_up",
    "extend_visual_line_down",
    "move_next_word_start",
    "move_prev_word_start",
    "move_next_word_end",
    "extend_next_word_start",
    "extend_prev_word_start",
    "extend_next_word_end",
    "goto_line_start",
    "goto_line_end",
    "goto_first_nonwhitespace",
    "goto_file_start",
    "goto_last_line",
    "goto_line",
    "goto_window_top",
    "goto_window_center",
    "goto_window_bottom",
    "page_up",
    "page_down",
    "half_page_up",
    "half_page_down",
    "scroll_up",
    "scroll_down",
    "align_view_center",
    "align_view_top",
    "align_view_bottom",
    "repeat_last_motion",
    "select_mode",
    "normal_mode",
    "extend_line_below",
    "extend_to_line_bounds",
    "collapse_selection",
    "flip_selections",
    "keep_primary_selection",
    "select_all",
    "copy_selection_on_next_line",
    "copy_selection_on_prev_line",
    "find_next_char",
    "find_till_char",
    "find_prev_char",
    "till_prev_char",
    "search",
    "rsearch",
    "search_next",
    "search_prev",
    "search_selection",
    "select_regex",
    "split_selection",
    "split_selection_on_newline",
    "match_brackets",
    "select_textobject_inner",
    "select_textobject_around",
    "yank",
    "yank_to_clipboard",
    // (its own rows, `[view]`)
    "view_copy",
    "view_leave",
    "view_code",
    "view_input",
    "view_latest",
    "view_take",
];

/// What a command in the terminal's view mode asks for once its key is done (it runs with the document swapped in
/// — the view can't end under it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum After {
    /// `esc` with nothing selected: to the code.
    Leave,
    /// `i`, `a`, `enter`: its program.
    Input,
    /// `G`: caught up, on the latest line.
    Latest,
    /// `enter` on a selection: quoted to the chat (`quote.rs`).
    Take,
    /// `enter` with nothing selected: its program.
    Enter,
    /// `space >` with nothing selected: the chat's view mode.
    ChatView,
    /// A command of `EDITORS` (`space l`, `space I`, `space ?`): run on the editor.
    Editor,
}

/// The terminal's view mode: its document, the keys' state while they're its, and what it was made of.
pub struct View {
    /// None only while a key's command has it swapped in (`swap`).
    doc: Option<Document>,
    /// Its own mode (`v` selects), a key sequence, count or awaited character under way, its own search — what
    /// `n`/`N` repeat, whether its matches are shown: never the code's, and the code's never touched by it
    /// (`target/sb/panes-plan.md` §2.3).
    pub keys: Keys,
    /// The matches on its rows (`refresh_search`).
    hits: Option<crate::search::SearchHits>,
    /// A prompt its `/`, `?` or `s` opened is up: its keys apply here.
    pub prompt: bool,
    /// Entered by the wheel: where the keys were.
    pub by_wheel: Option<Was>,
    after: Option<After>,
    /// What it was made of, and which of its lines wrapped into the next — with the blanks its line lost at its end
    /// — a quote joins them again.
    made: Made,
    wraps: Vec<Option<usize>>,
    /// A new panel size seen, not settled yet (`terminal_resized`).
    settling: Option<(u16, u16)>,
    /// `EDITORS`' commands its key asked for (`After::Editor`).
    deferred: Vec<crate::keymap::MappableCommand>,
}

impl View {
    pub fn doc(&self) -> &Document {
        self.doc.as_ref().expect("a view's document is in it outside a key's command")
    }

    fn doc_mut(&mut self) -> &mut Document {
        self.doc.as_mut().expect("a view's document is in it outside a key's command")
    }

    /// Rows it was made of.
    pub fn rows(&self) -> usize {
        self.made.lines
    }

    /// More than one character selected, or several cursors — what `esc` collapses first, `enter` quotes.
    pub fn selects(&self) -> bool {
        selects(self.doc())
    }
}

use crate::docpane::{selects, wide};

/// The view mode with the keys now and its keys' state — the chat's transcript's or the terminal's (the status
/// line, which-key, the state).
pub fn viewing(ed: &Editor) -> Option<(Surface, &Keys)> {
    match has_keys(ed)? {
        Surface::Chat => Some((Surface::Chat, &crate::chatdoc::transcript(ed)?.pane.keys)),
        Surface::Terminal => Some((Surface::Terminal, &ed.session.view.as_ref()?.keys)),
    }
}

/// Before drawing: the matches the view's search shows on its own rows — the pattern its `/` prompt is typing,
/// else its last search while shown. Its rows hold still while it lasts, so they're counted once a pattern.
pub fn refresh_search(ed: &mut Editor) {
    let typed = ed.prompt.as_ref().filter(|p| matches!(p.kind, crate::editor::PromptKind::Search { .. }));
    let typed = typed.map(|p| p.text.clone());
    let Some(v) = ed.session.view.as_mut().filter(|v| v.doc.is_some()) else { return };
    let pattern = match (v.prompt, typed) {
        (true, Some(t)) => Some(t).filter(|t| !t.is_empty()),
        _ => v.keys.search.clone().filter(|_| v.keys.search_hl),
    };
    v.hits = match pattern {
        Some(p) if v.hits.as_ref().is_some_and(|h| h.holds(v.doc(), &p)) => v.hits.take(),
        Some(p) => crate::search::SearchHits::of(v.doc(), &p),
        None => None,
    };
}

/// `esc` with the view's keys: its matches no longer shown (the code's stay as they are).
pub fn search_unshown(ed: &mut Editor) {
    if let Some(v) = ed.session.view.as_mut() {
        v.keys.search_hl = false;
    }
}

impl View {
    /// The matches its search shows now (`refresh_search`).
    pub fn hits(&self) -> Option<&crate::search::SearchHits> {
        self.hits.as_ref()
    }
}

/// The surface whose view mode has the keys now.
pub fn has_keys(ed: &Editor) -> Option<Surface> {
    if crate::chatdoc::has_keys(ed) {
        return Some(Surface::Chat);
    }
    let v = ed.session.view.as_ref()?;
    terminal_holds(ed, v).then_some(Surface::Terminal)
}

/// The terminal's view still has its surface: tarae's keys on its panel, in front, the same run.
fn terminal_holds(ed: &Editor, v: &View) -> bool {
    ed.session.is_at(At::Slot)
        && ed.slot() == Some(crate::sizes::Slot::Terminal)
        && ed.session.terminal.as_ref().is_some_and(|t| t.visible && t.generation() == v.made.generation)
}

/// The terminal is in its view mode.
pub fn terminal_viewed(ed: &Editor) -> bool {
    ed.session.view.is_some()
}

/// The terminal in its view mode (drawn with the painter on its cells).
pub fn terminal_view_of(ed: &Editor) -> Option<&View> {
    ed.session.view.as_ref()
}

/// Where the keys are now, as the wheel would give them back.
pub fn was(ed: &Editor) -> Was {
    if let Some(s) = has_keys(ed) {
        Was::View(s)
    } else if ed.session.program_has_keys() {
        Was::Program
    } else if ed.session.is_at(At::Chat) && ed.session.chat.is_some() {
        Was::Chat
    } else {
        Was::Code
    }
}

/// The keys back where the wheel found them.
pub fn give_back(ed: &mut Editor, was: Was) {
    let chat = ed.session.chat.is_some() && matches!(was, Was::Chat | Was::View(Surface::Chat));
    let program =
        was == Was::Program && ed.session.terminal.as_ref().is_some_and(|t| t.visible && t.exited.is_none());
    ed.session.at = match (chat, program) {
        (true, _) => At::Chat,
        (_, true) => At::Program,
        _ => At::Code,
    };
    if program && let Some(t) = ed.session.terminal.as_mut() {
        t.parser.screen_mut().set_scrollback(0);
    }
    match was {
        Was::View(Surface::Chat) => crate::chatdoc::view(ed),
        Was::View(Surface::Terminal) if ed.session.terminal.as_ref().is_some_and(|t| t.visible) => {
            terminal_enter(ed, Land::InSight, None)
        }
        _ => {}
    }
}

/// The terminal's view mode's state, its keys under way none yet.
fn made(doc: Document, by_wheel: Option<Was>, made: Made, wraps: Vec<Option<usize>>) -> View {
    View {
        doc: Some(doc),
        keys: Keys::default(),
        hits: None,
        prompt: false,
        by_wheel,
        after: None,
        made,
        wraps,
        settling: None,
        deferred: Vec::new(),
    }
}

/// The rows it shows (rows, width): the terminal's screen.
fn viewport(ed: &Editor) -> (usize, usize) {
    let Some(v) = ed.session.view.as_ref() else { return (20, 60) };
    (usize::from(v.made.size.0).max(1), usize::from(v.made.size.1).max(1))
}

// ── The terminal ────────────────────────────────────────────────────────────────────────────────

/// The bytes a cell of the terminal's is in its document's line: a blank one a space, a wide one its character
/// once (its second cell none), the program's control characters a space each — as `vt100::Screen::rows` gives a
/// row, which the document is read from; the painter counts the same way.
pub fn cell_len(contents: &str) -> usize {
    match contents {
        "" => 1,
        s => s.chars().map(|c| if c.is_control() { 1 } else { c.len_utf8() }).sum(),
    }
}

/// The terminal's rows — its scrollback, then its screen — as a document, a line each (`cell_len`, blanks at its
/// end off), how many rows of scrollback are above the screen, and which rows wrapped into the next. vt100 reaches
/// its rows only through the scrollback offset: read a page at a time by moving it, put back as it was.
fn terminal_doc(ed: &mut Editor) -> Option<(Document, usize, Vec<Option<usize>>)> {
    let t = ed.session.terminal.as_mut()?;
    let screen = t.parser.screen_mut();
    let (rows, cols) = screen.size();
    let was = screen.scrollback();
    screen.set_scrollback(usize::MAX);
    let back = screen.scrollback();
    let total = back + usize::from(rows);
    let mut text = String::new();
    let mut wraps = Vec::with_capacity(total);
    let mut line = 0;
    while line < total {
        // (the page whose first row is `line` — the screen's own, once past the scrollback)
        let off = back.saturating_sub(line);
        screen.set_scrollback(off);
        let first = back - off;
        let before = line;
        let skip = line - first;
        for (k, row) in screen.rows(0, cols).skip(skip).enumerate() {
            if line == total {
                break;
            }
            let from = text.len();
            text.extend(row.chars().map(|c| if c.is_control() { ' ' } else { c }));
            let kept = text[from..].trim_end().len();
            // (a wrapped row's blanks at its end are spaces the program printed — vt100 gives a row's cells up to
            // the last one written: a quote joins it to the next with them)
            let wrapped = screen.row_wrapped((skip + k) as u16);
            wraps.push(wrapped.then(|| text[from + kept..].chars().count()));
            text.truncate(from + kept);
            // (each row its line break: a blank last row is a line too — joined, it would be the ghost line
            // after a final newline, and the view one row short of the screen's bottom)
            text.push('\n');
            line += 1;
        }
        if line == before {
            break;
        }
    }
    screen.set_scrollback(was);
    let mut doc = Document::from_str(ed.alloc_id(), &text);
    doc.wrap = Some(false);
    doc.throwaway = true;
    Some((doc, back, wraps))
}

/// The terminal's view mode: held still from now (its program's output kept back), its rows read, the cursor
/// where `land` says, the rows in sight as they were — tarae's keys on its panel.
fn terminal_enter(ed: &mut Editor, land: Land, by_wheel: Option<Was>) {
    let Some(t) = ed.session.terminal.as_mut() else { return };
    t.hold();
    t.visible = true;
    let screen = t.parser.screen();
    let (size, off, cursor) = (screen.size(), screen.scrollback(), usize::from(screen.cursor_position().0));
    let generation = t.generation();
    let Some((mut doc, back, wraps)) = terminal_doc(ed) else { return };
    let last = mv::last_line(&doc.text);
    let top = back.saturating_sub(off);
    let rows = usize::from(size.0).max(1);
    let latest = || {
        let blank = |l: usize| doc.text.line(l).chars().all(char::is_whitespace);
        (back + cursor..=last).rev().find(|&l| !blank(l)).unwrap_or(back + cursor)
    };
    let line = match land {
        Land::InSight if off == 0 => back + cursor,
        Land::InSight => top + rows - 1,
        // (inside the scrolloff margin: the view stays as the wheel found it)
        Land::Row(r) => {
            let so = ed.config.scrolloff.min((rows - 1) / 2);
            top + r.clamp(so, rows - 1 - so)
        }
        Land::Latest => latest(),
    }
    .min(last);
    doc.set_selection(Selection::point(mv::line_start(&doc.text, line)));
    doc.set_top(top.min(last));
    let made_of = Made { generation, size, back, lines: last + 1 };
    ed.session.view = Some(made(doc, by_wheel, made_of, wraps));
    ed.session.at = At::Slot;
    ed.session.hand = None;
    ed.session.slot_front = Some(crate::sizes::Slot::Terminal);
    in_sight(ed);
}

/// The terminal's `C-\` from its program (`[terminal]`): its view mode, at the row in sight. No terminal: one
/// started, the keys its program's (`C-\` from the code).
pub fn terminal_view(ed: &mut Editor) {
    match ed.session.terminal.is_some() {
        true => terminal_enter(ed, Land::InSight, None),
        false => ed.terminal_focus(),
    }
}

/// The wheel over the terminal (its program didn't ask for the mouse, not full-screen — or in its view mode):
/// up, its view mode at that point, the cursor on the row under the pointer, the keys its; it scrolls the view;
/// back down to the bottom, by the wheel, nothing selected, the keys go back where they were.
pub fn terminal_wheel(ed: &mut Editor, row: Option<usize>, up: bool) {
    if !terminal_viewed(ed) {
        if !up {
            // (left scrolled back: forward, as the wheel always did)
            if let Some(t) = ed.session.terminal.as_mut() {
                let s = t.parser.screen_mut();
                let n = s.scrollback();
                s.set_scrollback(n.saturating_sub(3));
            }
            return;
        }
        let was = was(ed);
        terminal_enter(ed, row.map_or(Land::InSight, Land::Row), Some(was));
    }
    swap(ed, |ed| ed.scroll_view(if up { -3 } else { 3 }, false));
    let Some(v) = ed.session.view.as_ref() else { return };
    let at_end = v.doc().top >= v.made.back;
    if !up
        && at_end
        && !v.selects()
        && !v.prompt
        && let Some(was) = v.by_wheel
    {
        leave(ed);
        give_back(ed, was);
    }
}

/// A press on its screen's cell (row, column) while viewed: the view's cursor on that cell's character, as a click
/// in the code puts it (a wide character's second cell: the character; past a row's text: its end) — nothing goes to
/// the program.
pub fn terminal_click(ed: &mut Editor, cell: Option<(u16, u16)>) {
    let Some((r, c)) = cell else { return };
    // (the bytes of the row's line before the cell's character, counted as the document was read — `cell_len`)
    let Some(t) = ed.session.terminal.as_ref() else { return };
    let s = t.parser.screen();
    let (mut at, mut here) = (0, 0);
    for x in 0..=c {
        let Some(cell) = s.cell(r, x) else { break };
        if cell.is_wide_continuation() {
            continue;
        }
        here = at;
        at += cell_len(cell.contents());
    }
    let Some(v) = ed.session.view.as_mut().filter(|v| v.doc.is_some()) else { return };
    let d = v.doc_mut();
    let line = (d.top + usize::from(r)).min(mv::last_line(&d.text));
    let (ls, le) = (mv::line_start(&d.text, line), mv::line_end(&d.text, line));
    d.set_selection(Selection::point((ls + here).min(le)));
    in_sight(ed);
}

/// A new panel size waits to settle (`terminal_resized`): a frame is wanted `SETTLE` on.
pub fn terminal_settling(ed: &Editor) -> bool {
    terminal_view_of(ed).is_some_and(|v| v.settling.is_some())
}

/// How long a new panel size has to hold before the view is read again at it (`terminal_resized`): a frame's
/// wake-up after the last change of a border drag.
pub const SETTLE: std::time::Duration = std::time::Duration::from_millis(100);

/// After each frame's resize (`term::render_frame`): the panel took another size — its view read again at it, the
/// cursor on the line it was on, once the size has settled: the same at two frames in a row (`settling` the one
/// seen first; `Editor::tick_wanted` asks for the second, `SETTLE` on). A border drag's frames don't each pay a
/// reading (3.7 ms at 5,000 rows); meanwhile the view draws as it was made — rows added at the screen's bottom
/// have no line yet, the scrollback's are unchanged.
pub fn terminal_resized(ed: &mut Editor) {
    let now = ed.session.terminal.as_ref().map(|t| t.parser.screen().size());
    let Some(v) = ed.session.view.as_mut().filter(|v| v.doc.is_some()) else { return };
    let m = v.made;
    let Some(now) = now else { return };
    // (back at the size it was made at counts as a change too: vt100 drops a shrunk screen's bottom rows and
    // brings them back blank — what it shows isn't what the view was made of)
    if v.settling.is_none() && now == m.size {
        return;
    }
    if v.settling != Some(now) {
        v.settling = Some(now);
        return;
    }
    // (read again into the view as it is — its mode, keys under way, count, selections and top kept: a row keeps its
    // line, a resize leaving vt100's scrollback as it was; a place past its line's new end at the end)
    let Some((mut doc, back, wraps)) = terminal_doc(ed) else { return };
    let Some(v) = ed.session.view.as_mut() else { return };
    v.wraps = wraps;
    let old = v.doc();
    let place = |pos: usize| {
        let l = mv::line_of(&old.text, pos);
        (l, pos - mv::line_start(&old.text, l))
    };
    let last = mv::last_line(&doc.text);
    let put = |(l, at): (usize, usize)| {
        let l = l.min(last);
        let p = (mv::line_start(&doc.text, l) + at).min(mv::line_end(&doc.text, l));
        crate::graphemes::snap(&doc.text, p)
    };
    let sel = old.selection().transform(|r| Range { anchor: put(place(r.anchor)), head: put(place(r.head)) });
    let top = old.top.min(last);
    doc.set_selection(sel);
    doc.set_top(top);
    v.doc = Some(doc);
    v.made = Made { size: now, back, lines: last + 1, ..m };
    v.settling = None;
    in_sight(ed);
}

/// The cursor's line in sight, the scrolloff margin kept (one line a row: nothing wraps).
fn in_sight(ed: &mut Editor) {
    let (rows, _) = viewport(ed);
    let so = ed.config.scrolloff.min(rows.saturating_sub(1) / 2);
    let Some(v) = ed.session.view.as_mut() else { return };
    let m = v.made;
    let d = v.doc_mut();
    let last = mv::last_line(&d.text);
    let line = mv::line_of(&d.text, d.selection().primary().cursor(&d.text));
    let mut top = d.top.min(last);
    if line < top + so {
        top = line.saturating_sub(so);
    } else if line + so >= top + rows {
        top = (line + so + 1).saturating_sub(rows);
    }
    // (no further down than the last line at the bottom)
    top = top.min((last + 1).saturating_sub(rows));
    d.set_top(top);
    // (the terminal shows what's in sight by its scrollback offset)
    if let Some(t) = ed.session.terminal.as_mut().filter(|t| t.generation() == m.generation) {
        t.parser.screen_mut().set_scrollback(m.back.saturating_sub(top));
    }
}

/// Runs `f` with the view's document as `current`, its mode, keys under way and size as the editor's — all
/// put back before it returns (the code's are the editor's again; the jump list never holds the view's spots).
fn swap<R>(ed: &mut Editor, f: impl FnOnce(&mut Editor) -> R) -> Option<R> {
    let (rows, cols) = viewport(ed);
    let v = ed.session.view.as_mut()?;
    let doc = v.doc.take()?;
    let id = doc.id;
    let current = ed.current;
    ed.docs.push(doc);
    ed.current = ed.docs.len() - 1;
    let mut v = ed.session.view.take()?;
    crate::docpane::trade(ed, &mut v.keys);
    ed.session.view = Some(v);
    let viewport = std::mem::replace(&mut ed.viewport, (rows, cols));
    let focus = ed.session.focus;
    let jumps = ed.session.views.iter().find(|w| w.id == focus).map(|w| w.jumps.clone());
    let prompt = ed.prompt.is_some();
    let r = f(ed);
    ed.viewport = viewport;
    if let (Some(jumps), Some(w)) = (jumps, ed.session.views.iter_mut().find(|w| w.id == focus)) {
        w.jumps = jumps;
    }
    let at = ed.docs.iter().rposition(|d| d.id == id).expect("the view's document is still in");
    let doc = ed.docs.remove(at);
    ed.current = current;
    let mut v = ed.session.view.take().expect("the view lasts through its key");
    crate::docpane::trade(ed, &mut v.keys);
    v.doc = Some(doc);
    // (a prompt it opened — `/`, `s` — is its until it closes)
    v.prompt = match ed.prompt.is_some() {
        true => v.prompt || !prompt,
        false => false,
    };
    ed.session.view = Some(v);
    in_sight(ed);
    Some(r)
}

/// A command in view mode asks for what's done after its key (`After`).
pub fn ask(ed: &mut Editor, after: After) {
    if let Some(v) = ed.session.view.as_mut() {
        v.after = Some(after);
    }
}

/// `esc` in view mode: a selection collapses to the primary cursor first (and `v` ends); nothing selected,
/// the view ends — to the code.
pub fn leave_key(ed: &mut Editor) {
    let select = ed.mode == Mode::Select;
    let doc = ed.doc_mut();
    if selects(doc) || select {
        let c = doc.selection().primary().cursor(&doc.text);
        doc.set_selection(Selection::single(Range::point(c)));
        ed.mode = Mode::Normal;
        return;
    }
    ask(ed, After::Leave);
}

/// Ends the view: the terminal no longer held — what its program wrote meanwhile onto its screen; scrolled back,
/// it stays on the rows you read — the document freed. The keys stay where they are — the caller says where they
/// go.
pub fn leave(ed: &mut Editor) {
    let Some(v) = ed.session.view.take() else { return };
    if let Some(t) = ed.session.terminal.as_mut().filter(|t| t.generation() == v.made.generation) {
        t.release();
    }
    if ed.prompt.is_some() && v.prompt {
        ed.prompt = None;
    }
}

/// The view caught up: made again from its screen, what came taken in, the cursor on the latest line — its own
/// mode, keys under way and search come along (a `v`, a count before `G`).
fn catch_up(ed: &mut Editor) {
    let Some(old) = ed.session.view.take() else { return };
    if let Some(t) = ed.session.terminal.as_mut() {
        t.release();
    }
    terminal_enter(ed, Land::Latest, old.by_wheel);
    if let Some(v) = ed.session.view.as_mut() {
        v.keys = old.keys;
        let last = v.rows();
        v.doc_mut().set_top(last);
    }
    in_sight(ed);
}

/// What a command asked for, once its key is done.
fn after(ed: &mut Editor) {
    let Some(v) = ed.session.view.as_mut() else { return };
    let Some(a) = v.after.take() else { return };
    let deferred = std::mem::take(&mut v.deferred);
    match a {
        After::Leave => {
            leave(ed);
            ed.session.leave(At::Slot);
        }
        After::Input | After::Enter => {
            if ed.session.terminal.as_ref().is_some_and(|t| t.exited.is_some()) {
                return ed.set_status("its program has ended — q close, esc code");
            }
            leave(ed);
            ed.session.leave(At::Slot);
            if let Some(t) = ed.session.terminal.as_mut() {
                ed.session.at = At::Program;
                t.parser.screen_mut().set_scrollback(0);
            }
        }
        After::Latest => catch_up(ed),
        After::Take => take(ed),
        After::ChatView => crate::chatdoc::view(ed),
        After::Editor => ed.run(&deferred, None),
    }
}

/// A command a view mode's key runs on the editor, its document put back (`After`): asked for from inside the
/// key's command — a view's document swapped in — else run now.
pub fn swapped(ed: &Editor) -> bool {
    ed.session.view.as_ref().is_some_and(|v| v.doc.is_none())
}

/// `enter` on a selection: each range a quote over the chat's input (`quote.rs`), the view left, the input with
/// the keys — only where its rows go to Claude (`llm.screen-terminal`), else said, nothing done.
fn take(ed: &mut Editor) {
    if !ed.config.llm.screen_terminal {
        return ed.set_status("the terminal's rows don't go to Claude (llm.screen-terminal) — y copies them");
    }
    let Some(v) = ed.session.view.as_ref() else { return };
    let taken = taken(ed, v);
    if taken.is_empty() {
        return ed.set_status("only blanks are selected — nothing to quote");
    }
    // (the keys the chat's input's: `quote::add` focuses it, the terminal's panel lets go)
    leave(ed);
    crate::quote::add(ed, taken);
}

/// The view's selections as quotes, one a range (blank ones left out): joined where the program's line wrapped,
/// its lines of how many.
fn taken(ed: &Editor, v: &View) -> Vec<crate::quote::Taken> {
    use crate::quote::{Taken, lines, text_of};
    let d = v.doc();
    let how = |l: usize| (0, v.wraps.get(l.wrapping_sub(1)).copied().flatten());
    let quote = |r: &Range| {
        let (text, l0, l1) = text_of(&d.text, (r.from(), r.to()), &how, &|_| None);
        // (blank rows around it — a screen's foot — aren't what was meant)
        let text = text.trim_start_matches('\n').trim_end().to_string();
        if text.trim().is_empty() {
            return None;
        }
        let n = lines(text.lines().count());
        // (the program's file name — `terminal::spawn`'s `argv[0]`, never a title the program sets)
        let title = ed.session.terminal.as_ref().map_or_else(String::new, |t| t.title.clone());
        let at = format!("{}-{} of {}", l0 + 1, l1 + 1, v.rows());
        Some(Taken {
            attrs: vec![("from", "terminal".into()), ("title", title), ("lines", at)],
            label: format!("terminal {n}"),
            text,
            redact: true,
            code: None,
        })
    };
    // (only the ranges wider than a character: a plain cursor beside them quotes nothing)
    let wide = |r: &&Range| crate::graphemes::next_boundary(&d.text, r.from()) < r.to();
    d.selection().ranges().iter().filter(wide).filter_map(quote).collect()
}

/// Whether `key` is `C-w` (or `A-C-w`): the windows' keys, the editor's from a view mode too.
fn windows(key: Key) -> bool {
    key.ctrl && key.code == Code::Char('w')
}

/// The keys the terminal's view mode with nothing under way lets go on down the chain (`keys_to`'s layer): `C-w`.
pub fn passes() -> Vec<Key> {
    let ctrl = Key { code: Code::Char('w'), ctrl: true, alt: false, shift: false };
    vec![ctrl, Key { alt: true, ..ctrl }]
}

/// A key while the terminal's view mode has them: the view's prompt's (`/`), else its table's — a command of
/// `COMMANDS`, run on its document; a key it has no use for is said. `C-w` and a sequence of the editor's under
/// way go on. Returns the handler that took it.
pub fn key(ed: &mut Editor, key: Key) -> Option<crate::uistate::Handler> {
    use crate::uistate::Handler;
    if has_keys(ed) != Some(Surface::Terminal) || !ed.pending.is_empty() {
        return None;
    }
    let v = ed.session.view.as_ref()?;
    if ed.prompt.is_some() {
        if !v.prompt {
            return None;
        }
        swap(ed, |ed| ed.handle_prompt_key(key));
        return Some(Handler::Prompt);
    }
    if v.keys.idle() && windows(key) {
        return None;
    }
    swap(ed, |ed| key_in(ed, key));
    after(ed);
    Some(Handler::TerminalView)
}

/// `key` with the view's document swapped in: the awaited character (`f`), a count, the table's command.
fn key_in(ed: &mut Editor, key: Key) {
    if let Some((f, count)) = ed.on_next_char.take() {
        let ch = match (key.code, key.ctrl || key.alt) {
            (Code::Char(c), false) => Some(c),
            (Code::Enter, _) => Some('\n'),
            (Code::Tab, _) => Some('\t'),
            _ => None,
        };
        if let Some(ch) = ch {
            ed.with_group_count(count, |cx| f(cx, ch));
        }
        return;
    }
    if ed.pending.is_empty()
        && let Some(c @ '0'..='9') = key.plain_char()
        && (c != '0' || ed.count.is_some())
    {
        let digit = c.to_digit(10).unwrap_or(0) as usize;
        ed.count = Some(ed.count.unwrap_or(0).saturating_mul(10).saturating_add(digit));
        return;
    }
    ed.pending.push(key);
    let cmds = match ed.keymaps.view_lookup(Surface::Terminal, ed.mode, &ed.pending) {
        Lookup::Pending => return,
        Lookup::Matched(cmds) => Some(cmds.to_vec()),
        Lookup::NotFound => None,
    };
    let keys = std::mem::take(&mut ed.pending);
    let count = ed.count.take();
    let editors = |c: &crate::keymap::MappableCommand| matches!(c, crate::keymap::MappableCommand::Static(s) if EDITORS.contains(&s.name));
    match cmds {
        // (the editor's own, once the view's document is put back — `space ?`, `space I`, `space l`)
        Some(cmds) if cmds.iter().any(editors) => {
            if let Some(v) = ed.session.view.as_mut() {
                v.deferred = cmds;
            }
            ask(ed, After::Editor);
        }
        Some(cmds) => {
            ed.run(&cmds, count);
            if ed.on_next_char.is_some() {
                ed.char_for = keys.iter().map(Key::to_string).collect();
            }
        }
        None => said(ed, &keys),
    }
}

/// The key a view mode's command is bound to, as a hint names it: `like` when that's one of them (`i` before
/// `a` and `ret`), else the shortest — None when nothing is (a remap took it away).
pub fn bound(ed: &Editor, surface: Surface, name: &str, like: &str) -> Option<String> {
    let keys = ed.keymaps.view(surface, Mode::Normal).keys_for(name);
    let k = keys.iter().find(|k| k.as_str() == like).or(keys.first())?;
    // (as every hint names it)
    Some(if k == "ret" { "enter".into() } else { k.clone() })
}

/// A key the terminal's view mode has no use for: said where the user looks, with the way to type (§3.1).
fn said(ed: &mut Editor, keys: &[Key]) {
    let seq: Vec<String> = keys.iter().map(Key::to_string).collect();
    // (the two ways on, as the status line says them — to type, to the code: its menus, `:`, its keys; an ended
    // program takes no typing)
    let types = typeable(ed).then_some("types in the program");
    let ways: Vec<String> = [("view_input", "i", types), ("view_leave", "esc", Some("to the code"))]
        .iter()
        .filter_map(|(cmd, like, what)| {
            Some(format!("{} {}", bound(ed, Surface::Terminal, cmd, like)?, (*what)?))
        })
        .collect();
    let out = match ways.is_empty() {
        true => "its keys are its moves and selections".to_string(),
        false => ways.join(", "),
    };
    ed.set_status(format!("{} isn't in view mode — {out}", seq.join(" ")));
}

/// Whether `enter` quotes in the view mode with the keys now (`enter quote` said): something selected, on a
/// surface whose rows go to Claude — not the terminal's while `llm.screen-terminal` is off (there `enter` only says
/// why).
pub fn quotes(ed: &Editor) -> bool {
    match has_keys(ed) {
        Some(Surface::Chat) => crate::chatdoc::quotes(ed),
        Some(Surface::Terminal) => {
            ed.session.view.as_ref().is_some_and(|v| wide(v.doc()) && ed.config.llm.screen_terminal)
        }
        None => false,
    }
}

/// Whether `i` has somewhere to type: the chat's input always, the terminal's program while it runs.
pub fn typeable(ed: &Editor) -> bool {
    match has_keys(ed) {
        Some(Surface::Terminal) => ed.session.terminal.as_ref().is_some_and(|t| t.exited.is_none()),
        Some(Surface::Chat) | None => true,
    }
}

/// Every key and event: a view whose surface lost the keys (a click elsewhere, the chat closed or minimized,
/// the terminal hidden, behind another tab or replaced) ends; tarae's keys on the terminal's panel are its view
/// mode (`C-w j`, a tab stepped to, its program ended while it fills the screen). Never inside a key's command (its
/// document swapped in): after it. The chat's transcript takes the keys coming and going too (`chatdoc::sync_keys`).
pub fn sync(ed: &mut Editor) {
    crate::chatdoc::sync_keys(ed);
    if ed.session.view.as_ref().is_some_and(|v| v.doc.is_none()) {
        return;
    }
    if let Some(v) = ed.session.view.as_ref()
        && !terminal_holds(ed, v)
    {
        leave(ed);
    }
    if ed.session.view.is_some() || !ed.session.is_at(At::Slot) {
        return;
    }
    match ed.slot() {
        Some(crate::sizes::Slot::Terminal) if ed.session.terminal.as_ref().is_some_and(|t| t.visible) => {
            terminal_enter(ed, Land::InSight, None)
        }
        _ => {}
    }
}

#[cfg(test)]
pub(crate) mod terminal_tests;

/// The surface whose view mode opened the prompt that's up (`/`, `?`, `s`): its word on the status line under it.
pub fn prompt_from(ed: &Editor) -> Option<Surface> {
    let s = has_keys(ed)?;
    let up = match s {
        Surface::Chat => crate::chatdoc::transcript(ed).is_some_and(|t| t.pane.prompt),
        Surface::Terminal => ed.session.view.as_ref().is_some_and(|v| v.prompt),
    };
    (ed.prompt.is_some() && up).then_some(s)
}

/// The state's `view`: its surface, the cursor's line, selections and whether something came since — shapes,
/// never text.
pub fn state(ed: &Editor) -> Option<serde_json::Value> {
    if has_keys(ed) == Some(Surface::Chat) {
        return crate::chatdoc::state(ed);
    }
    let v = ed.session.view.as_ref()?;
    let d = v.doc();
    let line = mv::line_of(&d.text, d.selection().primary().cursor(&d.text));
    Some(serde_json::json!({
        "surface": Surface::Terminal.name(),
        "line": line + 1,
        "lines": v.rows(),
        "selections": d.selection().ranges().len(),
        "selects": v.selects(),
        "mode": if v.keys.mode == Mode::Select { "select" } else { "normal" },
        "held": ed.session.terminal.as_ref().is_some_and(|t| t.came()),
    }))
}
