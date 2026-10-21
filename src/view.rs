//! View mode — a cursor on what a surface shows (`target/sb/quote-plan.md` §3): the surface's rows as a read-only
//! document, Helix's moves, selections, search and counts on it (`COMMANDS`, the keymap's `[view]` tables), drawn
//! by the surface's own drawer with the selections painted over its rows (`term::view_paint`). The chat's: `esc`
//! from its input, `i` back; `esc` again to the code. The terminal's: `C-\` from its program, `i` back; `esc` or
//! `C-\` to the code (tarae's keys on its panel are its view mode). It holds still while it lasts — the chat
//! draws the rows it was made of, the terminal keeps its program's output back (`Terminal::hold`) — `G`
//! catches up.
//!
//! The document is in no pane and never the front buffer: each key's command runs with it swapped in as
//! `current` (`swap`) and put back before the key's handler returns — frames, jobs, servers and `context()` see
//! the pane's document only.

use std::sync::Arc;

use crate::chat::ChatRow;
use crate::document::Document;
use crate::editor::{CharFn, Editor, Mode};
use crate::key::{Code, Key};
use crate::keymap::Lookup;
use crate::movement as mv;
use crate::selection::{Range, Selection};
use crate::session::At;

/// What has a view mode: the chat's conversation, the terminal's scrollback and screen, and the bottom slot's run
/// panels — a task's output, a preflight's, the tests' detail (the failure picked, else the output).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Surface {
    Chat,
    Terminal,
    Task,
    Preflight,
    Tests,
}

/// The keymap's view tables over `[view]`: `[view.chat]`, `[view.terminal]`, and `[view.run]` the three run panels
/// share.
pub const TABLES: &[&str] = &["chat", "terminal", "run"];

impl Surface {
    /// Its name in the state (`view:tests`) and the status line (`● view · tests`).
    pub fn name(self) -> &'static str {
        match self {
            Surface::Chat => "chat",
            Surface::Terminal => "terminal",
            Surface::Task => "task",
            Surface::Preflight => "preflight",
            Surface::Tests => "tests",
        }
    }

    /// Its table in the keymap (`[view.chat]`; the run panels' `[view.run]`).
    pub fn table(self) -> &'static str {
        match self {
            Surface::Chat => "chat",
            Surface::Terminal => "terminal",
            Surface::Task | Surface::Preflight | Surface::Tests => "run",
        }
    }

    /// The bottom slot's panel it is.
    pub fn slot(self) -> Option<crate::sizes::Slot> {
        use crate::sizes::Slot;
        match self {
            Surface::Chat => None,
            Surface::Terminal => Some(Slot::Terminal),
            Surface::Task | Surface::Preflight => Some(Slot::Task),
            Surface::Tests => Some(Slot::Test),
        }
    }

    /// A run panel's: the task's, a preflight's, the tests'.
    pub fn run(self) -> bool {
        matches!(self, Surface::Task | Surface::Preflight | Surface::Tests)
    }
}

/// Commands of the editor's that view mode's `space` has (`[view.space]`): run once the key's command is done and
/// the view's document put back — they open a picker or go to the chat, never on the view's buffer.
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

/// A run panel's view, as it was made: its run, the tests' failure it shows (None: the output), and how many times
/// its run's output had been trimmed (`task::Run::trimmed` — once more, its lines aren't the view's lines).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RunMade {
    generation: u64,
    picked: Option<usize>,
    trimmed: u64,
}

/// How many times a run panel's output was trimmed (the tests' never is).
fn run_trimmed(ed: &Editor, s: Surface) -> u64 {
    match s {
        Surface::Task | Surface::Preflight => ed.session.task_run.as_ref().map_or(0, |r| r.trimmed()),
        _ => 0,
    }
}

/// The run's lines are still the view's, line for line: the same run showing the same thing, nothing trimmed out
/// of it since — only then is the view's top an index into its output (its `scroll`). Printed on, they still are.
fn same_lines(ed: &Editor, s: Surface, m: RunMade) -> bool {
    run_is(ed, s) == Some((m.generation, m.picked)) && run_trimmed(ed, s) == m.trimmed
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

/// The chat's rows as a frame laid them out, in order: each message's kept rows (with its index), and the
/// frame's own between them (air, a wait, the empty chat's words) — `term::draw_chat` records them
/// (`Screen.chat_laid`), a view's snapshot keeps them.
pub type Laid = Vec<(Option<usize>, Arc<[ChatRow]>)>;

/// What a command in view mode asks for once its key is done (it runs with the document swapped in — the view
/// can't end under it).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum After {
    /// `esc` with nothing selected: to the code.
    Leave,
    /// `i`, `a`, `enter`: the surface's input.
    Input,
    /// `G`: caught up, on the latest line.
    Latest,
    /// `enter` on a selection: quoted to the chat (`quote.rs`).
    Take,
    /// `enter` with nothing selected: the surface's own — the chat's input, the terminal's program, a run panel's
    /// run waiting behind (else the chat's input).
    Enter,
    /// `space >` with nothing selected, off the chat: the chat's view mode.
    ChatView,
    /// A run panel's `K`: where Claude's command ran.
    Where,
    /// A run panel's `o`: a preflight step's output in a buffer.
    Output,
    /// A command of `EDITORS` (`space l`, `space I`, `space ?`): run on the editor.
    Editor,
}

/// A surface's view mode: its document, the keys' state while they're its, and what it was made of.
pub struct View {
    pub surface: Surface,
    /// None only while a key's command has it swapped in (`swap`).
    doc: Option<Document>,
    /// The rows it was made of — the chat draws these while it lasts (held still).
    pub laid: Laid,
    /// What the surface held when it was made (`seen`) — another now: something came after.
    pub seen: u64,
    /// Its own mode (`v` selects), and a key sequence, count or awaited character under way — never the code's.
    mode: Mode,
    pending: Vec<Key>,
    count: Option<usize>,
    on_next_char: Option<(CharFn, Option<usize>)>,
    char_for: String,
    sticky: Option<Vec<usize>>,
    search_origin: Option<usize>,
    /// Its own search — what `n`/`N` repeat, whether its matches are shown, the matches on its rows: never the
    /// code's, and the code's never touched by it (`target/sb/panes-plan.md` §2.3).
    search: Option<String>,
    search_hl: bool,
    hits: Option<crate::search::SearchHits>,
    /// A prompt its `/`, `?` or `s` opened is up: its keys apply here.
    pub prompt: bool,
    /// Entered by the wheel: where the keys were.
    pub by_wheel: Option<Was>,
    after: Option<After>,
    /// The terminal's: what it was made of, and which of its lines wrapped into the next — with the blanks its line
    /// lost at its end — a quote joins them again.
    made: Option<Made>,
    wraps: Vec<Option<usize>>,
    /// The terminal's: a new panel size seen, not settled yet (`terminal_resized`).
    settling: Option<(u16, u16)>,
    /// A run panel's: what it was made of, and each line's stderr flag (its colour as the panel draws it) —
    /// `waiting`: nothing printed yet, its one line the command, as the panel shows it (faint).
    run: Option<RunMade>,
    errs: Vec<bool>,
    waiting: bool,
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

    /// Its mode (the status line, the state).
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Its key sequence under way (which-key, the state).
    pub fn pending(&self) -> &[Key] {
        &self.pending
    }

    /// Its count and keys under way, as the status line says them (`Editor::pending_display`).
    pub fn pending_display(&self) -> String {
        let mut s = self.count.map(|c| c.to_string()).unwrap_or_default();
        for k in &self.pending {
            s.push_str(&k.to_string());
        }
        if self.on_next_char.is_some() {
            s.push_str(&self.char_for);
            s.push('…');
        }
        s
    }

    /// Its count and keys under way, each a word (the state's `keys.pending`).
    pub fn pending_keys(&self) -> Vec<String> {
        let mut v: Vec<String> = self.count.iter().map(|n| n.to_string()).collect();
        v.extend(self.pending.iter().map(Key::to_string));
        v
    }

    /// The next key is taken as a character (`f`, `t`).
    pub fn char_next(&self) -> bool {
        self.on_next_char.is_some()
    }

    /// A count or an awaited character (`f`) under way: the next key is its, whatever it is.
    pub fn waits(&self) -> bool {
        self.count.is_some() || self.on_next_char.is_some()
    }

    /// Rows it was made of.
    pub fn rows(&self) -> usize {
        match (self.made, self.run) {
            (Some(m), _) => m.lines,
            (None, Some(_)) => self.errs.len(),
            (None, None) => self.laid.iter().map(|(_, r)| r.len()).sum(),
        }
    }

    /// A run panel's line `l` came on stderr (drawn faint, as the panel's output is).
    pub fn err(&self, l: usize) -> bool {
        self.errs.get(l).copied().unwrap_or(false)
    }

    /// The tests' view shows a failure's message (None: the output).
    pub fn failure(&self) -> bool {
        self.run.is_some_and(|m| m.picked.is_some())
    }

    /// A run panel's view made before its run printed anything: its one line is the command (drawn faint).
    pub fn waiting(&self) -> bool {
        self.waiting
    }

    /// Something came since it was made (`● view · new text after`).
    pub fn came(&self, now: u64) -> bool {
        now != self.seen
    }

    /// More than one character selected, or several cursors — what `esc` collapses first, `enter` quotes.
    pub fn selects(&self) -> bool {
        selects(self.doc())
    }
}

/// A range of `doc`'s selection is wider than a character — what `enter` quotes (several plain cursors aren't).
pub fn wide(d: &Document) -> bool {
    d.selection().ranges().iter().any(|r| crate::graphemes::next_boundary(&d.text, r.from()) < r.to())
}

/// `doc` has more than one character selected, or several cursors.
pub fn selects(d: &Document) -> bool {
    let sel = d.selection();
    sel.ranges().len() > 1
        || sel.ranges().iter().any(|r| crate::graphemes::next_boundary(&d.text, r.from()) < r.to())
}

/// What the conversation holds now, as one number: what came changes it.
pub fn seen(c: &crate::chat::Chat) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    c.msgs.len().hash(&mut h);
    if let Some(m) = c.msgs.last() {
        (m.version(), m.text.len()).hash(&mut h);
    }
    (c.state as u8).hash(&mut h);
    h.finish()
}

/// A drawn row as a line of the document: its spans' text, line breaks and tabs as spaces (one byte each —
/// the painter splits the spans at the same offsets), blanks at its end off.
fn row_text(r: &ChatRow, out: &mut String) {
    let from = out.len();
    for s in &r.spans {
        out.extend(s.text.chars().map(|c| if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c }));
    }
    let kept = out[from..].trim_end().len();
    out.truncate(from + kept);
}

fn chat_doc(ed: &mut Editor, laid: &Laid) -> Document {
    let mut text = String::new();
    let mut first = true;
    for r in laid.iter().flat_map(|(_, rows)| rows.iter()) {
        if !first {
            text.push('\n');
        }
        first = false;
        row_text(r, &mut text);
    }
    let mut doc = Document::from_str(ed.alloc_id(), &text);
    doc.wrap = Some(false);
    doc.throwaway = true;
    doc
}

/// The document line of message `msg`'s row `row`.
fn line_at(laid: &Laid, msg: usize, row: usize) -> Option<usize> {
    let mut at = 0;
    for (m, rows) in laid {
        if *m == Some(msg) {
            return Some(at + row.min(rows.len().saturating_sub(1)));
        }
        at += rows.len();
    }
    None
}

/// The message and its row a document line is — a line of the frame's own is the message before it's last row.
fn place_at(laid: &Laid, line: usize) -> Option<(usize, usize)> {
    let mut at = 0;
    let mut last = None;
    for (m, rows) in laid {
        if let Some(m) = m {
            if line < at + rows.len() {
                return Some((*m, line - at));
            }
            last = Some((*m, rows.len().saturating_sub(1)));
        }
        at += rows.len();
        if line < at {
            return last;
        }
    }
    last
}

/// Before drawing: the matches the view's search shows on its own rows — the pattern its `/` prompt is typing,
/// else its last search while shown. Its rows hold still while it lasts, so they're counted once a pattern.
pub fn refresh_search(ed: &mut Editor) {
    let typed = ed.prompt.as_ref().filter(|p| matches!(p.kind, crate::editor::PromptKind::Search { .. }));
    let typed = typed.map(|p| p.text.clone());
    let Some(v) = ed.session.view.as_mut().filter(|v| v.doc.is_some()) else { return };
    let pattern = match (v.prompt, typed) {
        (true, Some(t)) => Some(t).filter(|t| !t.is_empty()),
        _ => v.search.clone().filter(|_| v.search_hl),
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
        v.search_hl = false;
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
    let v = ed.session.view.as_ref()?;
    match v.surface {
        Surface::Chat => ed
            .session
            .chat
            .as_ref()
            .filter(|c| ed.session.is_at(At::Chat) && !c.compact())
            .map(|_| Surface::Chat),
        Surface::Terminal => terminal_holds(ed, v).then_some(Surface::Terminal),
        s => run_holds(ed, v).then_some(s),
    }
}

/// A run panel's view still has its surface: tarae's keys on its panel, in front, the same run showing the same
/// thing (the tests' failure picked — `]x` picks another: the view is made again).
fn run_holds(ed: &Editor, v: &View) -> bool {
    let Some(slot) = v.surface.slot() else { return false };
    ed.session.is_at(At::Slot)
        && ed.slot() == Some(slot)
        && run_surface(ed, slot) == Some(v.surface)
        && v.run.is_some_and(|m| run_is(ed, v.surface) == Some((m.generation, m.picked)))
}

/// The run panel `slot` is, as a view's surface: the task panel a preflight's while its run is one.
pub fn run_surface(ed: &Editor, slot: crate::sizes::Slot) -> Option<Surface> {
    use crate::sizes::Slot;
    match slot {
        Slot::Task => ed.session.task_run.as_ref().map(|r| {
            match ed.session.preflight.as_ref().is_some_and(|p| p.generation == r.generation()) {
                true => Surface::Preflight,
                false => Surface::Task,
            }
        }),
        Slot::Test => ed.session.test_run.as_ref().map(|_| Surface::Tests),
        _ => None,
    }
}

/// What a run panel shows now: its run, and the tests' failure picked.
fn run_is(ed: &Editor, s: Surface) -> Option<(u64, Option<usize>)> {
    match s {
        Surface::Task | Surface::Preflight => ed.session.task_run.as_ref().map(|r| (r.generation(), None)),
        Surface::Tests => {
            ed.session.test_run.as_ref().map(|r| (r.generation(), r.picked().map(|_| r.selected)))
        }
        Surface::Chat | Surface::Terminal => None,
    }
}

/// A run panel in its view mode (drawn from its document, the painter on it).
pub fn run_view_of(ed: &Editor, slot: crate::sizes::Slot) -> Option<&View> {
    ed.session.view.as_ref().filter(|v| v.surface.run() && v.surface.slot() == Some(slot) && v.doc.is_some())
}

/// The terminal's view still has its surface: tarae's keys on its panel, in front, the same run.
fn terminal_holds(ed: &Editor, v: &View) -> bool {
    let run = v.made.map(|m| m.generation);
    ed.session.is_at(At::Slot)
        && ed.slot() == Some(crate::sizes::Slot::Terminal)
        && ed.session.terminal.as_ref().is_some_and(|t| t.visible && Some(t.generation()) == run)
}

/// The terminal is in its view mode.
pub fn terminal_viewed(ed: &Editor) -> bool {
    ed.session.view.as_ref().is_some_and(|v| v.surface == Surface::Terminal)
}

/// The terminal in its view mode (drawn with the painter on its cells).
pub fn terminal_view_of(ed: &Editor) -> Option<&View> {
    ed.session.view.as_ref().filter(|v| v.surface == Surface::Terminal)
}

/// Where the keys are now, as the wheel would give them back.
fn was(ed: &Editor) -> Was {
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
fn give_back(ed: &mut Editor, was: Was) {
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
        Was::View(Surface::Chat) => chat_view(ed),
        Was::View(Surface::Terminal) if ed.session.terminal.as_ref().is_some_and(|t| t.visible) => {
            terminal_enter(ed, Land::InSight, None)
        }
        Was::View(s) if s.slot().is_some_and(|slot| run_surface(ed, slot) == Some(s)) => {
            run_enter(ed, s, Land::InSight, None)
        }
        _ => {}
    }
}

/// Another surface's view ends as this one's comes (one view at a time).
fn take_over(ed: &mut Editor, surface: Surface) {
    if ed.session.view.as_ref().is_some_and(|v| v.surface != surface) {
        leave(ed);
    }
}

/// The chat in its view mode (drawn held still, the painter on it).
pub fn chat_view_of(ed: &Editor) -> Option<&View> {
    ed.session.view.as_ref().filter(|v| v.surface == Surface::Chat)
}

/// `esc` from the chat's input, `space >` from anywhere: its view mode, landed where you were in this
/// conversation, else on the last answer's first row, else the last row. Nothing to read — an empty
/// conversation — the input's `esc` goes to the code (as it did), `space >` to the input, said. The minimized
/// chat keeps its `esc` (to the code); `space >` brings it back. A chat not drawn yet (closed, minimized) is
/// viewed once the frame lays its rows out (`wanted`).
pub fn chat_view(ed: &mut Editor) {
    let from_input = ed.session.is_at(At::Chat);
    let Some(c) = ed.session.chat.as_mut() else {
        crate::chat::open(ed);
        return want(ed);
    };
    if c.compact() {
        match from_input {
            true => ed.session.at = At::Code,
            false => {
                c.unfolded = true;
                want(ed);
            }
        }
        return;
    }
    if c.msgs.is_empty() {
        ed.session.at = if from_input { At::Code } else { At::Chat };
        if !from_input {
            ed.set_status("nothing to view yet — the conversation is empty: type a question");
        }
        return;
    }
    // (`space >` from elsewhere with the chat out of sight: once a frame lays it out; the input's `esc` has it in
    // sight — made of what the last frame laid)
    if ed.screen.chat_view.get().is_none() && !from_input {
        return want(ed);
    }
    let c = ed.session.chat.as_ref().expect("the chat, above");
    let laid = ed.screen.chat_laid.borrow().clone();
    let line = c.view_at.and_then(|(m, r)| line_at(&laid, m, r)).or_else(|| {
        let from = c.msgs.iter().rposition(|m| m.role == crate::chat::Role::User).map_or(0, |i| i + 1);
        let answer = (from..c.msgs.len())
            .find(|&i| c.msgs[i].role == crate::chat::Role::Assistant)
            .or_else(|| c.msgs.iter().rposition(|m| m.role == crate::chat::Role::Assistant))?;
        // (its first row is the air above it)
        line_at(&laid, answer, 1)
    });
    enter(ed, laid, line, None);
}

/// The chat's view mode asked for before its rows were laid out: made once a frame has (`wanted`).
fn want(ed: &mut Editor) {
    if let Some(c) = ed.session.chat.as_mut() {
        c.view_wanted = true;
        ed.session.at = At::Chat;
    }
}

/// After a frame (`term::render`): the view asked for before the chat was drawn, made now — true when it was
/// (the frame is drawn again with it).
pub fn wanted(ed: &mut Editor) -> bool {
    let chat_keys = ed.session.is_at(At::Chat);
    let Some(c) = ed.session.chat.as_mut().filter(|c| c.view_wanted) else { return false };
    c.view_wanted = false;
    if !chat_keys || ed.screen.chat_view.get().is_none() {
        return false;
    }
    // (opened on an empty conversation: the input keeps the keys, said)
    if c.msgs.is_empty() {
        ed.set_status("nothing to view yet — the conversation is empty: type a question");
        return true;
    }
    chat_view(ed);
    true
}

/// The wheel over the chat's conversation, up: its view mode at that point, the cursor on `line` (the row
/// under the pointer), the keys its — back where they were when the wheel takes it back down. An empty
/// conversation: nothing to read, nothing done.
pub fn chat_wheel(ed: &mut Editor, line: Option<usize>, up: bool) {
    if ed.session.chat.as_ref().is_none_or(|c| c.msgs.is_empty()) {
        return;
    }
    if chat_view_of(ed).is_none() {
        if !up {
            if let Some(c) = ed.session.chat.as_mut() {
                c.scroll = c.scroll.saturating_sub(3);
            }
            return;
        }
        let was = was(ed);
        let laid = ed.screen.chat_laid.borrow().clone();
        enter(ed, laid, line, Some(was));
    }
    ed.session.at = At::Chat;
    swap(ed, |ed| ed.scroll_view(if up { -3 } else { 3 }, false));
    // (back down to the end, by the wheel, nothing selected: done reading)
    let Some(v) = ed.session.view.as_ref() else { return };
    let at_end = v.doc().top + ed.screen.chat_view.get().map_or(1, |a| a.2) >= v.rows();
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

/// A view mode's state, its keys under way none yet.
fn made(
    surface: Surface,
    doc: Document,
    laid: Laid,
    seen: u64,
    by_wheel: Option<Was>,
    made: Option<Made>,
) -> View {
    View {
        surface,
        doc: Some(doc),
        laid,
        seen,
        mode: Mode::Normal,
        pending: Vec::new(),
        count: None,
        on_next_char: None,
        char_for: String::new(),
        sticky: None,
        search_origin: None,
        search: None,
        search_hl: false,
        hits: None,
        prompt: false,
        by_wheel,
        after: None,
        made,
        wraps: Vec::new(),
        settling: None,
        run: None,
        errs: Vec::new(),
        waiting: false,
        deferred: Vec::new(),
    }
}

/// A view of the chat made from `laid`, the cursor on `line` (else the last), what's on screen as it was.
fn enter(ed: &mut Editor, laid: Laid, line: Option<usize>, by_wheel: Option<Was>) {
    take_over(ed, Surface::Chat);
    let mut doc = chat_doc(ed, &laid);
    let last = mv::last_line(&doc.text);
    let line = line.unwrap_or(last).min(last);
    doc.set_selection(Selection::point(mv::line_start(&doc.text, line)));
    // (the rows in sight stay where they are: the window the chat showed)
    doc.set_top(ed.screen.chat_view.get().map_or(0, |a| a.3).min(last));
    let seen = ed.session.chat.as_ref().map_or(0, seen);
    ed.session.view = Some(made(Surface::Chat, doc, laid, seen, by_wheel, None));
    // (the keys are the chat's now — not the program's, nor tarae's on the slot)
    if ed.session.chat.is_some() {
        ed.session.at = At::Chat;
    }
    in_sight(ed);
}

/// The rows it shows (rows, width) — as the last frame drew the chat's, a run panel's; the terminal's screen.
fn viewport(ed: &Editor) -> (usize, usize) {
    let Some(v) = ed.session.view.as_ref() else { return (20, 60) };
    if let Some(m) = v.made {
        return (usize::from(m.size.0).max(1), usize::from(m.size.1).max(1));
    }
    if v.surface.run() {
        let s = &ed.screen;
        let w = s.width.map_or(80, |w| w.saturating_sub(s.slot_x + s.chat_w + 2 * crate::chrome::PAD));
        return (run_rows(ed, v.surface), w.max(1));
    }
    ed.screen.chat_view.get().map_or((20, 60), |a| (a.2.max(1), a.1.max(1)))
}

// ── The run panels ──────────────────────────────────────────────────────────────────────────────

/// The rows a run panel shows its output (the tests' detail) in, as the last frame drew it (not drawn yet: ten, till
/// the frame that draws it — `run_resized`).
pub(crate) fn run_rows(ed: &Editor, s: Surface) -> usize {
    let p = match s {
        Surface::Tests => ed.screen.test_panel.get(),
        _ => ed.screen.task_panel.get(),
    };
    p.map_or(10, |p| p.2).max(1)
}

/// A run panel's rows as a document — the task's output; the tests' detail: the failure picked's message, else
/// the output — a line each (a tab two spaces, as the panel draws it), each line's stderr flag, the line its panel
/// shows first in `rows`, and what the run had printed (`seen`). Nothing printed yet: the command, its one line, as
/// the panel shows it.
fn run_doc(ed: &mut Editor, s: Surface, rows: usize) -> Option<(Document, Vec<bool>, usize, u64)> {
    let mut text = String::new();
    let mut errs = Vec::new();
    let mut push = |err: bool, l: &str| {
        text.push_str(&l.replace('\t', "  "));
        text.push('\n');
        errs.push(err);
    };
    let (top, seen) = match s {
        Surface::Tests => {
            let r = ed.session.test_run.as_ref()?;
            match r.picked() {
                // (as its panel says it)
                Some(c) if c.message.is_empty() => push(false, "failed (no message)"),
                Some(c) => c.message.iter().for_each(|m| push(false, m)),
                None if r.output.is_empty() => push(false, &r.target.argv().join(" ")),
                None => r.output.iter().for_each(|(e, l)| push(*e, l)),
            }
            (r.detail_top(rows), (r.output.len() + r.cases.len()) as u64)
        }
        _ => {
            let r = ed.session.task_run.as_ref()?;
            if r.output.is_empty() {
                // (a command of several lines: its first, as the panel shows it)
                let mut lines = r.task.cmd.lines();
                let first = lines.next().unwrap_or_default();
                push(false, &if lines.next().is_some() { format!("{first} …") } else { first.to_string() });
            }
            r.output.iter().for_each(|(e, l)| push(*e, l));
            (r.top(rows), r.output_bytes as u64)
        }
    };
    let mut doc = Document::from_str(ed.alloc_id(), &text);
    doc.wrap = Some(false);
    doc.throwaway = true;
    Some((doc, errs, top, seen))
}

/// What a run panel's run has printed (its view's `seen` — another now: something came after).
fn run_seen(ed: &Editor, s: Surface) -> Option<u64> {
    match s {
        Surface::Tests => ed.session.test_run.as_ref().map(|r| (r.output.len() + r.cases.len()) as u64),
        _ => ed.session.task_run.as_ref().map(|r| r.output_bytes as u64),
    }
}

/// A run panel's view mode — held still from now (it draws its document while more comes), the cursor where `land`
/// says (in sight: a failure's first line, else the output's last row in sight), the rows in sight as they were —
/// tarae's keys on its panel.
fn run_enter(ed: &mut Editor, s: Surface, land: Land, by_wheel: Option<Was>) {
    let Some(slot) = s.slot() else { return };
    take_over(ed, s);
    let Some(is) = run_is(ed, s) else { return };
    let rows = run_rows(ed, s);
    let waiting = match s {
        Surface::Tests => {
            ed.session.test_run.as_ref().is_some_and(|r| r.picked().is_none() && r.output.is_empty())
        }
        _ => ed.session.task_run.as_ref().is_some_and(|r| r.output.is_empty()),
    };
    let Some((mut doc, errs, top, seen)) = run_doc(ed, s, rows) else { return };
    let last = mv::last_line(&doc.text);
    let so = ed.config.scrolloff.min((rows - 1) / 2);
    let line = match land {
        Land::InSight if is.1.is_some() => top,
        // (its last row in sight — inside the scrolloff margin, the view as it was, but at the output's end)
        Land::InSight if top + rows > last => last,
        Land::InSight => (top + rows - 1 - so).max(top),
        // (inside the scrolloff margin: the view stays as the wheel found it)
        Land::Row(r) => top + r.clamp(so, rows - 1 - so),
        Land::Latest => last,
    }
    .min(last);
    doc.set_selection(Selection::point(mv::line_start(&doc.text, line)));
    doc.set_top(top.min(last));
    let mut view = made(s, doc, Vec::new(), seen, by_wheel, None);
    view.run = Some(RunMade { generation: is.0, picked: is.1, trimmed: run_trimmed(ed, s) });
    view.errs = errs;
    view.waiting = waiting;
    ed.session.view = Some(view);
    ed.session.at = At::Slot;
    ed.session.slot_front = Some(slot);
    in_sight(ed);
}

/// After a frame (`term::render`): a run panel's view laid out for another number of rows than its panel just drew
/// (`C-w z`, the panel growing with its output) kept in sight at it — true when that moved it (drawn again).
pub fn run_resized(ed: &mut Editor) -> bool {
    let Some(top) =
        ed.session.view.as_ref().filter(|v| v.surface.run() && v.doc.is_some()).map(|v| v.doc().top)
    else {
        return false;
    };
    in_sight(ed);
    ed.session.view.as_ref().is_some_and(|v| v.doc().top != top)
}

/// The wheel over a run panel's rows (`row`: the one under the pointer): up, its view mode at that point, the keys
/// its; it scrolls the view; back down to the end, by the wheel, nothing selected, the keys go back where they were.
/// Down outside view mode, the panel scrolls on as it always did.
pub fn run_wheel(ed: &mut Editor, slot: crate::sizes::Slot, row: Option<usize>, up: bool) {
    let Some(s) = run_surface(ed, slot) else { return };
    if run_view_of(ed, slot).is_none() {
        if !up {
            let rows = run_rows(ed, s);
            match s {
                Surface::Tests => ed.session.test_run.as_mut().map(|r| r.scroll_by(3, rows)),
                _ => ed.session.task_run.as_mut().map(|r| r.scroll_by(3, rows)),
            };
            return;
        }
        let was = was(ed);
        run_enter(ed, s, row.map_or(Land::InSight, Land::Row), Some(was));
    }
    swap(ed, |ed| ed.scroll_view(if up { -3 } else { 3 }, false));
    let rows = run_rows(ed, s);
    let Some(v) = ed.session.view.as_ref() else { return };
    let at_end = v.doc().top + rows >= v.rows();
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

/// A press on a run panel's row `row` (of its output — the tests' detail) at column `col` of its text: the keys its
/// view mode's, the cursor on that character, as a click in the code puts it.
pub fn run_click(ed: &mut Editor, slot: crate::sizes::Slot, row: usize, col: usize) {
    ed.bottom_take_keys();
    let Some(v) = ed.session.view.as_mut().filter(|v| v.surface.slot() == Some(slot) && v.doc.is_some())
    else {
        return;
    };
    let d = v.doc_mut();
    let line = (d.top + row).min(mv::last_line(&d.text));
    let (ls, le) = (mv::line_start(&d.text, line), mv::line_end(&d.text, line));
    let text = d.text.byte_slice(ls..le).to_string();
    // (the character whose cells hold the column — display width, a wide one's second cell its own)
    let (mut w, mut at) = (0, 0);
    for g in unicode_segmentation::UnicodeSegmentation::graphemes(text.as_str(), true) {
        let gw = unicode_width::UnicodeWidthStr::width(g);
        if w + gw > col {
            break;
        }
        (w, at) = (w + gw, at + g.len());
    }
    // (in sight already: a press inside the scrolloff margin doesn't move the rows under the pointer)
    d.set_selection(Selection::point(crate::graphemes::snap(&d.text, (ls + at).min(le))));
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
    take_over(ed, Surface::Terminal);
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
    let mut view = made(Surface::Terminal, doc, Vec::new(), generation, by_wheel, Some(made_of));
    view.wraps = wraps;
    ed.session.view = Some(view);
    ed.session.at = At::Slot;
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
    let at_end = v.made.is_some_and(|m| v.doc().top >= m.back);
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
    let Some(v) = ed.session.view.as_mut().filter(|v| v.doc.is_some() && v.surface == Surface::Terminal)
    else {
        return;
    };
    let Some(m) = v.made else { return };
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
    v.made = Some(Made { size: now, back, lines: last + 1, ..m });
    v.settling = None;
    in_sight(ed);
}

/// The cursor's line in sight, the scrolloff margin kept (one line a row: nothing wraps).
fn in_sight(ed: &mut Editor) {
    let (rows, _) = viewport(ed);
    let so = ed.config.scrolloff.min(rows.saturating_sub(1) / 2);
    let Some(v) = ed.session.view.as_mut() else { return };
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
    if let Some(m) = v.made
        && let Some(t) = ed.session.terminal.as_mut().filter(|t| t.generation() == m.generation)
    {
        t.parser.screen_mut().set_scrollback(m.back.saturating_sub(top));
    }
    // (a run panel's scroll is the view's, while it shows what the view was made of — what `o` reads as the step in
    // view, where it's left)
    let (s, run) = (v.surface, v.run);
    if let Some(m) = run
        && same_lines(ed, s, m)
    {
        match s {
            Surface::Tests => ed.session.test_run.as_mut().map(|r| r.scroll_to(top, rows)),
            _ => ed.session.task_run.as_mut().map(|r| r.scroll_to(top, rows)),
        };
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
    let trade = |ed: &mut Editor, v: &mut View| {
        std::mem::swap(&mut ed.mode, &mut v.mode);
        std::mem::swap(&mut ed.pending, &mut v.pending);
        std::mem::swap(&mut ed.count, &mut v.count);
        std::mem::swap(&mut ed.on_next_char, &mut v.on_next_char);
        std::mem::swap(&mut ed.char_for, &mut v.char_for);
        std::mem::swap(&mut ed.sticky, &mut v.sticky);
        std::mem::swap(&mut ed.search_origin, &mut v.search_origin);
        std::mem::swap(&mut ed.search, &mut v.search);
        std::mem::swap(&mut ed.search_hl, &mut v.search_hl);
    };
    let mut v = ed.session.view.take()?;
    trade(ed, &mut v);
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
    trade(ed, &mut v);
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
    let sel = doc.selection();
    let r = sel.primary();
    let wide = sel.ranges().len() > 1 || crate::graphemes::next_boundary(&doc.text, r.from()) < r.to();
    if wide || select {
        let c = r.cursor(&doc.text);
        doc.set_selection(Selection::single(Range::point(c)));
        ed.mode = Mode::Normal;
        return;
    }
    ask(ed, After::Leave);
}

/// Ends the view: where it was kept for next time (the chat's `view_at`), the surface no longer held — the rows
/// you were reading still in sight, what came while it held below them — the document freed. The keys stay
/// where they are — the caller says where they go.
pub fn leave(ed: &mut Editor) {
    let (rows, _) = viewport(ed);
    let Some(v) = ed.session.view.take() else { return };
    if let Some(m) = v.run {
        // (a run panel: scrolled to the rows you read — its end, it follows its output again — while it still shows
        // what the view was made of)
        let top = v.doc().top;
        if same_lines(ed, v.surface, m) {
            let rows = run_rows(ed, v.surface);
            match v.surface {
                Surface::Tests => ed.session.test_run.as_mut().map(|r| r.scroll_to(top, rows)),
                _ => ed.session.task_run.as_mut().map(|r| r.scroll_to(top, rows)),
            };
        }
        return close_prompt(ed, &v);
    }
    if let Some(m) = v.made {
        // (the terminal: what its program wrote meanwhile onto its screen — scrolled back, it stays on the rows
        // you read)
        if let Some(t) = ed.session.terminal.as_mut().filter(|t| t.generation() == m.generation) {
            t.release();
        }
        return close_prompt(ed, &v);
    }
    let d = v.doc();
    let line = mv::line_of(&d.text, d.selection().primary().cursor(&d.text));
    // (the chat's scroll counts from the live bottom: what the last frame laid out, rows that came included —
    // the snapshot's count would shift the view by them)
    let live: usize = ed.screen.chat_laid.borrow().iter().map(|(_, r)| r.len()).sum();
    let total = if live > 0 { live } else { v.rows() };
    if let Some(c) = ed.session.chat.as_mut() {
        c.view_at = place_at(&v.laid, line);
        c.scroll = total.saturating_sub(d.top + rows);
    }
    close_prompt(ed, &v);
}

/// A prompt the view opened (`/`, `s`) goes with it.
fn close_prompt(ed: &mut Editor, v: &View) {
    if ed.prompt.is_some() && v.prompt {
        ed.prompt = None;
    }
}

/// Ends the view and keeps nothing of it: the conversation it was made of is gone (`C-l` a new one, `C-p` an
/// earlier one) — its rows and its place are another conversation's.
pub fn forget(ed: &mut Editor) {
    // (only the chat's: a terminal's view isn't the conversation's — taken here, its held output would never go out)
    if ed.session.view.as_ref().is_some_and(|v| v.surface == Surface::Chat)
        && let Some(v) = ed.session.view.take()
    {
        close_prompt(ed, &v);
    }
}

/// The view caught up: made again from what the last frame laid out (the terminal's: from its screen, what
/// came taken in), the cursor on the latest line.
fn catch_up(ed: &mut Editor) {
    let Some((by_wheel, surface)) = ed.session.view.as_ref().map(|v| (v.by_wheel, v.surface)) else { return };
    if surface != Surface::Chat {
        // (the view's own mode and keys under way come along: a `v`, a count before `G`)
        let Some(old) = ed.session.view.take() else { return };
        match surface {
            Surface::Terminal => {
                if let Some(t) = ed.session.terminal.as_mut() {
                    t.release();
                }
                terminal_enter(ed, Land::Latest, by_wheel);
            }
            s => run_enter(ed, s, Land::Latest, by_wheel),
        }
        if let Some(v) = ed.session.view.as_mut() {
            (v.mode, v.pending, v.count, v.on_next_char, v.char_for) =
                (old.mode, old.pending, old.count, old.on_next_char, old.char_for);
            let last = v.rows();
            v.doc_mut().set_top(last);
        }
        return in_sight(ed);
    }
    let laid = ed.screen.chat_laid.borrow().clone();
    enter(ed, laid, None, by_wheel);
    // (the last line at the bottom of the view)
    if let Some(v) = ed.session.view.as_mut() {
        let last = v.rows();
        v.doc_mut().set_top(last);
    }
    in_sight(ed);
}

/// What a command asked for, once its key is done.
fn after(ed: &mut Editor) {
    let Some(v) = ed.session.view.as_mut() else { return };
    let Some(a) = v.after.take() else { return };
    let surface = v.surface;
    let deferred = std::mem::take(&mut v.deferred);
    match (a, surface) {
        (After::Leave, Surface::Chat) => {
            leave(ed);
            ed.session.leave(At::Chat);
        }
        (After::Leave, _) => {
            leave(ed);
            ed.session.leave(At::Slot);
        }
        (After::Input | After::Enter, Surface::Chat) => leave(ed),
        (After::Input, s) if s.run() => {
            // (the way out as `said` names it, as bound)
            let out =
                bound(ed, s, "view_leave", "esc").map(|k| format!(" — {k} to the code")).unwrap_or_default();
            ed.set_status(format!("the {} panel takes no typing{out}", s.name()));
        }
        // (a run panel's: the user's own run waiting behind Claude's, now — Claude's stopping, `queued.rs`; none
        // waits: the chat's input, to ask about what's read)
        (After::Enter, s) if s.run() => {
            let which =
                if s == Surface::Tests { crate::queued::Slot::Tests } else { crate::queued::Slot::Task };
            if !crate::queued::run_now(ed, which) {
                leave(ed);
                ed.session.leave(At::Slot);
                crate::chat::open(ed);
            }
        }
        (After::Input | After::Enter, _) => {
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
        (After::Latest, _) => catch_up(ed),
        (After::Take, surface) => take(ed, surface),
        (After::ChatView, Surface::Chat) => {
            ed.set_status("this is the chat's view — select something, then enter or space > quotes it")
        }
        (After::ChatView, _) => chat_view(ed),
        (After::Where, s) => run_where(ed, s),
        (After::Output, s) => {
            let out = match s {
                Surface::Preflight => ed.preflight_output(""),
                _ => Err("o shows a preflight step's output — this run isn't one".into()),
            };
            if let Err(e) = out {
                ed.set_status(e);
            }
        }
        (After::Editor, _) => ed.run(&deferred, None),
    }
}

/// A run panel's `K`: where Claude's command in it ran, and why — from its run's record (cardburden 4b).
fn run_where(ed: &mut Editor, s: Surface) {
    let ran = match s {
        Surface::Task | Surface::Preflight => ed
            .session
            .task_run
            .as_ref()
            .and_then(|r| Some((crate::task::traced(&r.task.cmd), r.ran.clone()?))),
        _ => None,
    };
    match ran {
        Some((cmd, ran)) => crate::chat::say_ran(ed, &cmd, &ran),
        None => ed.set_status("K says where a command of Claude's ran — this run isn't one"),
    }
}

/// A command a view mode's key runs on the editor, its document put back (`After`): asked for from inside the
/// key's command — a view's document swapped in — else run now.
pub fn swapped(ed: &Editor) -> bool {
    ed.session.view.as_ref().is_some_and(|v| v.doc.is_none())
}

/// `enter` on a selection: each range a quote over the chat's input (`quote.rs`), the view left, the input with
/// the keys. The terminal's only where its rows go to Claude (`llm.screen-terminal`) — else said, nothing done.
fn take(ed: &mut Editor, surface: Surface) {
    if surface == Surface::Terminal && !ed.config.llm.screen_terminal {
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

/// The view's selections as quotes, one a range (blank ones left out): the chat's rows joined where the layout
/// wrapped them, a cut code row whole; the terminal's where the program's line wrapped, its lines of how many.
fn taken(ed: &Editor, v: &View) -> Vec<crate::quote::Taken> {
    use crate::chat::Role;
    use crate::quote::{Taken, lines, text_of};
    let d = v.doc();
    let rows: Vec<&ChatRow> = v.laid.iter().flat_map(|(_, r)| r.iter()).collect();
    let how = |l: usize| match v.surface {
        Surface::Chat => rows.get(l).map_or((0, None), |r| (r.skip, r.join.map(usize::from))),
        Surface::Terminal => (0, v.wraps.get(l.wrapping_sub(1)).copied().flatten()),
        // (an output's lines are its own: none wrapped)
        _ => (0, None),
    };
    let rest = |l: usize| rows.get(l).and_then(|r| r.rest.clone());
    let quote = |r: &Range| {
        let (text, l0, l1) = text_of(&d.text, (r.from(), r.to()), &how, &rest);
        // (blank rows around it — a screen's foot, the air between messages — aren't what was meant)
        let text = text.trim_start_matches('\n').trim_end().to_string();
        if text.trim().is_empty() {
            return None;
        }
        let n = lines(text.lines().count());
        Some(match v.surface {
            s @ (Surface::Task | Surface::Preflight | Surface::Tests) => {
                // (what ran: the task's name, the tests' target — and the failure the detail is of)
                let (from, title, case) = match s {
                    Surface::Tests => {
                        let r = ed.session.test_run.as_ref();
                        let case = r.and_then(|r| r.picked().map(|c| c.name.clone()));
                        ("test", r.map_or_else(String::new, |r| r.target.label.clone()), case)
                    }
                    _ => {
                        let name =
                            ed.session.task_run.as_ref().map_or_else(String::new, |r| r.task.name.clone());
                        (if s == Surface::Preflight { "preflight" } else { "task" }, name, None)
                    }
                };
                let at = format!("{}-{} of {}", l0 + 1, l1 + 1, v.rows());
                let mut attrs = vec![("from", from.to_string()), ("title", title)];
                attrs.extend(case.clone().map(|c| ("test", c)));
                attrs.push(("lines", at));
                let what = match (s, &case) {
                    (Surface::Tests, Some(_)) => "failure",
                    (Surface::Tests, None) => "test output",
                    (Surface::Preflight, _) => "preflight",
                    _ => "task output",
                };
                Taken { attrs, label: format!("{what} {n}"), text, redact: true, code: None }
            }
            Surface::Terminal => {
                // (the program's file name — `terminal::spawn`'s `argv[0]`, never a title the program sets)
                let title = ed.session.terminal.as_ref().map_or_else(String::new, |t| t.title.clone());
                let at = format!("{}-{} of {}", l0 + 1, l1 + 1, v.rows());
                Taken {
                    attrs: vec![("from", "terminal".into()), ("title", title), ("lines", at)],
                    label: format!("terminal {n}"),
                    text,
                    redact: true,
                    code: None,
                }
            }
            Surface::Chat => {
                // (whose words: one message's role, else the chat's)
                let msgs = ed.session.chat.as_ref().map(|c| &c.msgs);
                let role = |l: usize| place_at(&v.laid, l).and_then(|(m, _)| Some(msgs?.get(m)?.role));
                let (role, label) = match (role(l0), role(l1)) {
                    (Some(a), Some(b)) if a == b => match a {
                        Role::User => ("user", "your message"),
                        Role::Assistant => ("assistant", "Claude's answer"),
                        Role::Thought => ("thinking", "Claude's thinking"),
                        Role::Tool => ("tool calls", "Claude's tool calls"),
                        Role::Note => ("note", "the chat"),
                    },
                    _ => ("several", "the chat"),
                };
                Taken {
                    attrs: vec![("from", "chat".into()), ("role", role.into())],
                    label: format!("{label} {n}"),
                    text,
                    redact: false,
                    code: None,
                }
            }
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

/// The chat's chords — the screen, pointing, the last code block, a new or earlier conversation, models, the
/// turn's undo, stop, follow's hops: the chat's in its view mode too (§3.4), where view mode's table has no
/// use for them (`C-d` `C-u` `C-f` `C-b` page there).
pub const CHAT_CHORDS: &[char] = &['s', 'o', 'y', 'r', 'l', 'p', 't', 'z', 'c', ']', 'g'];

/// `key` is one of the chat's chords that its view mode leaves to it.
pub fn chat_chord(ed: &Editor, key: Key) -> bool {
    let Some(v) = chat_view_of(ed) else { return false };
    let chord = key.ctrl && !key.alt && matches!(key.code, Code::Char(c) if CHAT_CHORDS.contains(&c));
    chord && ed.keymaps.view(Surface::Chat, v.mode).search(&[key]).is_none()
}

/// The keys a view mode with nothing under way lets go on down the chain (`keys_to`'s layer): `C-w`, and the
/// chat's chords.
pub fn passes(ed: &Editor) -> Vec<Key> {
    let ctrl = |c: char| Key { code: Code::Char(c), ctrl: true, alt: false, shift: false };
    let mut v = vec![ctrl('w'), Key { alt: true, ..ctrl('w') }];
    v.extend(CHAT_CHORDS.iter().map(|&c| ctrl(c)).filter(|&k| chat_chord(ed, k)));
    v.extend(['[', ']'].map(|c| Key::plain(Code::Char(c))).into_iter().filter(|&k| run_pass(ed, k)));
    v
}

/// A key while a view mode has them: the view's prompt's (`/`), else its table's — a command of
/// `COMMANDS`, run on its document; a key it has no use for is said. `C-w` and a sequence of the editor's
/// under way go on. Returns the handler that took it.
pub fn key(ed: &mut Editor, key: Key) -> Option<crate::uistate::Handler> {
    use crate::uistate::Handler;
    // (a run panel's `[`/`]` went on: the key after it is the editor's only where it completes the panel's own)
    if let Some((s, goes)) = run_bracket(ed) {
        if goes.contains(&key) {
            return None;
        }
        let mut keys = std::mem::take(&mut ed.pending);
        keys.push(key);
        said(ed, s, &keys);
        return Some(Handler::RunView);
    }
    let surface = has_keys(ed)?;
    if !ed.pending.is_empty() {
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
    if v.pending.is_empty()
        && v.on_next_char.is_none()
        && v.count.is_none()
        && (windows(key) || chat_chord(ed, key) || run_pass(ed, key))
    {
        return None;
    }
    swap(ed, |ed| key_in(ed, surface, key));
    after(ed);
    Some(match surface {
        Surface::Chat => Handler::ChatView,
        Surface::Terminal => Handler::TerminalView,
        Surface::Task | Surface::Preflight | Surface::Tests => Handler::RunView,
    })
}

/// The editor's commands a run panel's view reaches through `[`/`]`, as the panel did: the failures' (the tests'
/// view is made again of the one picked) and the list's. The rest act on the code out of sight: said.
const BRACKETS: &[&str] =
    &["goto_next_test_failure", "goto_prev_test_failure", "goto_next_quickfix", "goto_prev_quickfix"];

/// A run panel's view whose `[`/`]` went on to the editor (`run_pass`): its surface, and the keys after it that are
/// the editor's too — those completing one of `BRACKETS` as bound, and `esc` (it lets the sequence go).
pub fn run_bracket(ed: &Editor) -> Option<(Surface, Vec<Key>)> {
    use crate::keymap::MappableCommand;
    let s = has_keys(ed).filter(|s| s.run())?;
    let [b] = ed.pending.as_slice() else { return None };
    if !matches!(b.code, Code::Char('[' | ']')) || b.ctrl || b.alt {
        return None;
    }
    let panels = |cmds: &[MappableCommand]| {
        cmds.iter().all(|c| matches!(c, MappableCommand::Static(st) if BRACKETS.contains(&st.name)))
    };
    let mut goes: Vec<Key> = (ed.keymaps.children(Mode::Normal, &[*b]).into_iter().map(|(k, ..)| k))
        .filter(|&k| matches!(ed.keymaps.lookup(Mode::Normal, &[*b, k]), Lookup::Matched(c) if panels(c)))
        .collect();
    goes.push(Key::plain(Code::Esc));
    Some((s, goes))
}

/// `[` and `]` on a run panel's view: on to the editor, as they were on the panel — for the keys of `BRACKETS`
/// after them (`run_bracket`) — where its table doesn't bind them.
fn run_pass(ed: &Editor, key: Key) -> bool {
    let Some(v) = ed.session.view.as_ref().filter(|v| v.surface.run()) else { return false };
    let bracket = matches!(key.code, Code::Char('[' | ']')) && !key.ctrl && !key.alt;
    bracket && ed.keymaps.view(v.surface, v.mode).search(&[key]).is_none()
}

/// `key` with the view's document swapped in: the awaited character (`f`), a count, the table's command.
fn key_in(ed: &mut Editor, surface: Surface, key: Key) {
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
    let cmds = match ed.keymaps.view_lookup(surface, ed.mode, &ed.pending) {
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
        None => said(ed, surface, &keys),
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

/// A key view mode has no use for: said where the user looks, with the way to type (§3.1).
fn said(ed: &mut Editor, surface: Surface, keys: &[Key]) {
    let seq: Vec<String> = keys.iter().map(Key::to_string).collect();
    // (the two ways on, as the status line says them — to type, to the code: its menus, `:`, its keys; an ended
    // program takes no typing)
    let types = match surface {
        Surface::Chat => Some("types"),
        Surface::Terminal if typeable(ed) => Some("types in the program"),
        _ => None,
    };
    let ways: Vec<String> = [("view_input", "i", types), ("view_leave", "esc", Some("to the code"))]
        .iter()
        .filter_map(|(cmd, like, what)| Some(format!("{} {}", bound(ed, surface, cmd, like)?, (*what)?)))
        .collect();
    let out = match ways.is_empty() {
        true => "its keys are its moves and selections".to_string(),
        false => ways.join(", "),
    };
    ed.set_status(format!("{} isn't in view mode — {out}", seq.join(" ")));
}

/// Whether `enter` quotes in the view mode now (`enter quote` said): something selected, on a surface whose rows go
/// to Claude — not the terminal's while `llm.screen-terminal` is off (there `enter` only says why).
pub fn quotes(ed: &Editor) -> bool {
    ed.session
        .view
        .as_ref()
        .is_some_and(|v| wide(v.doc()) && (v.surface != Surface::Terminal || ed.config.llm.screen_terminal))
}

/// Whether `i` has somewhere to type: the chat's input always, the terminal's program while it runs.
pub fn typeable(ed: &Editor) -> bool {
    match ed.session.view.as_ref().map(|v| v.surface) {
        Some(Surface::Terminal) => ed.session.terminal.as_ref().is_some_and(|t| t.exited.is_none()),
        Some(s) => !s.run(),
        None => true,
    }
}

/// Every key and event: a view whose surface lost the keys (a click elsewhere, the chat closed or minimized,
/// the terminal hidden, behind another tab or replaced) ends, kept for next time; tarae's keys on the terminal's
/// panel are its view mode (`C-w j`, a tab stepped to, its program ended while it fills the screen). Never
/// inside a key's command (its document swapped in): after it.
pub fn sync(ed: &mut Editor) {
    if ed.session.view.as_ref().is_some_and(|v| v.doc.is_none()) {
        return;
    }
    if let Some(v) = ed.session.view.as_ref() {
        let gone = match v.surface {
            Surface::Chat => {
                !(ed.session.is_at(At::Chat) && ed.session.chat.as_ref().is_some_and(|c| !c.compact()))
            }
            Surface::Terminal => !terminal_holds(ed, v),
            _ => !run_holds(ed, v),
        };
        if gone {
            leave(ed);
        } else if v.surface.run() {
            // (its panel another size since — `C-w z`, a border dragged: the cursor kept in sight, the top within it)
            in_sight(ed);
        }
    }
    if ed.session.view.is_some() || !ed.session.is_at(At::Slot) {
        return;
    }
    match ed.slot() {
        Some(crate::sizes::Slot::Terminal) if ed.session.terminal.as_ref().is_some_and(|t| t.visible) => {
            terminal_enter(ed, Land::InSight, None)
        }
        // (a run panel's: another run, another failure picked — made again, landed as at first)
        Some(slot) if let Some(s) = run_surface(ed, slot) => run_enter(ed, s, Land::InSight, None),
        _ => {}
    }
}

#[cfg(test)]
mod run_tests;
#[cfg(test)]
pub(crate) mod terminal_tests;
#[cfg(test)]
pub(crate) mod tests;

/// A run panel's view mode with the user's own run waiting behind there: `enter`, nothing selected, runs it now.
pub fn run_waits(ed: &Editor) -> bool {
    let Some(v) = ed.session.view.as_ref().filter(|v| v.surface.run()) else { return false };
    let which =
        if v.surface == Surface::Tests { crate::queued::Slot::Tests } else { crate::queued::Slot::Task };
    crate::queued::mine_waits(ed, which)
}

/// The surface whose view mode opened the prompt that's up (`/`, `?`, `s`): its word on the status line under it.
pub fn prompt_from(ed: &Editor) -> Option<Surface> {
    let s = has_keys(ed)?;
    (ed.prompt.is_some() && ed.session.view.as_ref().is_some_and(|v| v.prompt)).then_some(s)
}

/// A run panel's view: its run printed more since it was made (`● view · output after`).
pub fn run_came(ed: &Editor, v: &View, s: Surface) -> bool {
    run_seen(ed, s).is_some_and(|n| v.came(n))
}

/// The state's `view`: its surface, the cursor's line, selections and whether something came since — shapes,
/// never text.
pub fn state(ed: &Editor) -> Option<serde_json::Value> {
    let v = ed.session.view.as_ref()?;
    let d = v.doc();
    let line = mv::line_of(&d.text, d.selection().primary().cursor(&d.text));
    let held = match v.surface {
        Surface::Chat => v.came(ed.session.chat.as_ref().map_or(v.seen, seen)),
        Surface::Terminal => ed.session.terminal.as_ref().is_some_and(|t| t.came()),
        s => run_came(ed, v, s),
    };
    Some(serde_json::json!({
        "surface": v.surface.name(),
        "line": line + 1,
        "lines": v.rows(),
        "selections": d.selection().ranges().len(),
        "selects": v.selects(),
        "mode": if v.mode == Mode::Select { "select" } else { "normal" },
        "held": held,
    }))
}
