//! View mode — a cursor on what a surface shows (`target/sb/quote-plan.md` §3): the surface's rows as a read-only
//! document, Helix's moves, selections, search and counts on it (`COMMANDS`, the keymap's `[view]` tables), drawn
//! by the surface's own drawer with the selections painted over its rows (`term::view_paint`). The chat's: `esc`
//! from its input, `i` back; `esc` again to the code. It holds still while it lasts — the chat draws the rows it
//! was made of, `G` catches up.
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

/// What has a view mode (P1a: the chat's conversation; the terminal next).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Surface {
    Chat,
}

pub const SURFACES: &[Surface] = &[Surface::Chat];

impl Surface {
    /// Its name in the keymap (`[view.chat]`), the state and the status line.
    pub fn name(self) -> &'static str {
        match self {
            Surface::Chat => "chat",
        }
    }
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
    "view_input",
    "view_latest",
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
    /// A prompt its `/`, `?` or `s` opened is up: its keys apply here.
    pub prompt: bool,
    /// Entered by the wheel (where the keys were: back there when the wheel takes it back down, nothing
    /// selected — §9 Q9).
    pub by_wheel: Option<bool>,
    after: Option<After>,
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
        self.laid.iter().map(|(_, r)| r.len()).sum()
    }

    /// Something came since it was made (`● view · new text after`).
    pub fn came(&self, now: u64) -> bool {
        now != self.seen
    }

    /// More than one character selected, or several cursors — what `esc` collapses first.
    pub fn selects(&self) -> bool {
        let d = self.doc();
        let sel = d.selection();
        sel.ranges().len() > 1
            || sel.ranges().iter().any(|r| crate::graphemes::next_boundary(&d.text, r.from()) < r.to())
    }
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

/// The surface whose view mode has the keys now.
pub fn has_keys(ed: &Editor) -> Option<Surface> {
    let v = ed.session.view.as_ref()?;
    match v.surface {
        Surface::Chat => {
            ed.session.chat.as_ref().filter(|c| c.focused && !c.compact()).map(|_| Surface::Chat)
        }
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
    let Some(c) = ed.session.chat.as_mut() else {
        crate::chat::open(ed);
        return want(ed);
    };
    let from_input = c.focused;
    if c.compact() {
        match from_input {
            true => c.focused = false,
            false => {
                c.unfolded = true;
                want(ed);
            }
        }
        return;
    }
    if c.msgs.is_empty() {
        c.focused = !from_input;
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
        c.focused = true;
        c.view_wanted = true;
    }
}

/// After a frame (`term::render`): the view asked for before the chat was drawn, made now — true when it was
/// (the frame is drawn again with it).
pub fn wanted(ed: &mut Editor) -> bool {
    let Some(c) = ed.session.chat.as_mut().filter(|c| c.view_wanted) else { return false };
    c.view_wanted = false;
    if !c.focused || ed.screen.chat_view.get().is_none() {
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
        let was = ed.session.chat.as_ref().is_some_and(|c| c.focused);
        let laid = ed.screen.chat_laid.borrow().clone();
        enter(ed, laid, line, Some(was));
    }
    if let Some(c) = ed.session.chat.as_mut() {
        c.focused = true;
    }
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
        if let Some(c) = ed.session.chat.as_mut() {
            c.focused = was;
        }
    }
}

/// A view of the chat made from `laid`, the cursor on `line` (else the last), what's on screen as it was.
fn enter(ed: &mut Editor, laid: Laid, line: Option<usize>, by_wheel: Option<bool>) {
    let mut doc = chat_doc(ed, &laid);
    let last = mv::last_line(&doc.text);
    let line = line.unwrap_or(last).min(last);
    doc.set_selection(Selection::point(mv::line_start(&doc.text, line)));
    // (the rows in sight stay where they are: the window the chat showed)
    doc.set_top(ed.screen.chat_view.get().map_or(0, |a| a.3).min(last));
    let seen = ed.session.chat.as_ref().map_or(0, seen);
    ed.session.view = Some(View {
        surface: Surface::Chat,
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
        prompt: false,
        by_wheel,
        after: None,
    });
    if let Some(c) = ed.session.chat.as_mut() {
        c.focused = true;
    }
    in_sight(ed);
}

/// The rows it shows (rows, width) — as the last frame drew them.
fn viewport(ed: &Editor) -> (usize, usize) {
    ed.screen.chat_view.get().map_or((20, 60), |a| (a.2.max(1), a.1.max(1)))
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

/// Ends the view: where it was kept for next time (the chat's `view_at`), the chat no longer held — the rows
/// you were reading still in sight, what came while it held below them — the document freed. The keys stay
/// where they are — the caller says where they go.
pub fn leave(ed: &mut Editor) {
    let Some(v) = ed.session.view.take() else { return };
    let (rows, _) = viewport(ed);
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
    if let Some(v) = ed.session.view.take().filter(|v| v.surface == Surface::Chat) {
        close_prompt(ed, &v);
    }
}

/// The view caught up: made again from what the last frame laid out, the cursor on the latest line.
fn catch_up(ed: &mut Editor) {
    let laid = ed.screen.chat_laid.borrow().clone();
    let Some(by_wheel) = ed.session.view.as_ref().map(|v| v.by_wheel) else { return };
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
    let Some(a) = ed.session.view.as_mut().and_then(|v| v.after.take()) else { return };
    match a {
        After::Leave => {
            leave(ed);
            if let Some(c) = ed.session.chat.as_mut() {
                c.focused = false;
            }
        }
        After::Input => leave(ed),
        After::Latest => catch_up(ed),
    }
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
    v
}

/// A key while a view mode has them: the view's prompt's (`/`), else its table's — a command of
/// `COMMANDS`, run on its document; a key it has no use for is said. `C-w` and a sequence of the editor's
/// under way go on. Returns the handler that took it.
pub fn key(ed: &mut Editor, key: Key) -> Option<crate::uistate::Handler> {
    use crate::uistate::Handler;
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
        && (windows(key) || chat_chord(ed, key))
    {
        return None;
    }
    swap(ed, |ed| key_in(ed, surface, key));
    after(ed);
    Some(Handler::ChatView)
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
    match cmds {
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
    keys.iter().find(|k| k.as_str() == like).or(keys.first()).cloned()
}

/// A key view mode has no use for: said where the user looks, with the way to type (§3.1).
fn said(ed: &mut Editor, surface: Surface, keys: &[Key]) {
    let seq: Vec<String> = keys.iter().map(Key::to_string).collect();
    // (the two ways on, as the status line says them — to type, to the code: its menus, `:`, its keys)
    let ways: Vec<String> = [("view_input", "i", "types"), ("view_leave", "esc", "to the code")]
        .iter()
        .filter_map(|(cmd, like, what)| Some(format!("{} {what}", bound(ed, surface, cmd, like)?)))
        .collect();
    let out = match ways.is_empty() {
        true => "its keys are its moves and selections".to_string(),
        false => ways.join(", "),
    };
    ed.set_status(format!("{} isn't in view mode — {out}", seq.join(" ")));
}

/// Every key and event: a view whose surface lost the keys (a click elsewhere, the chat closed or minimized)
/// ends, kept for next time.
pub fn sync(ed: &mut Editor) {
    let Some(v) = ed.session.view.as_ref() else { return };
    let gone = match v.surface {
        Surface::Chat => !ed.session.chat.as_ref().is_some_and(|c| c.focused && !c.compact()),
    };
    if gone {
        leave(ed);
    }
}

#[cfg(test)]
pub(crate) mod tests;

/// The state's `view`: its surface, the cursor's line, selections and whether something came since — shapes,
/// never text.
pub fn state(ed: &Editor) -> Option<serde_json::Value> {
    let v = ed.session.view.as_ref()?;
    let d = v.doc();
    let line = mv::line_of(&d.text, d.selection().primary().cursor(&d.text));
    let now = match v.surface {
        Surface::Chat => ed.session.chat.as_ref().map_or(v.seen, seen),
    };
    Some(serde_json::json!({
        "surface": v.surface.name(),
        "line": line + 1,
        "lines": v.rows(),
        "selections": d.selection().ranges().len(),
        "selects": v.selects(),
        "mode": if v.mode == Mode::Select { "select" } else { "normal" },
        "held": v.came(now),
    }))
}
