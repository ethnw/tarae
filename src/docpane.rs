//! A document in a pane that isn't the code's (panes P2c, `target/sb/panes-plan.md` §2.1e) — a run panel's output
//! (`rundoc.rs`), the chat's transcript (`chatdoc.rs`): read-only, with its own keys' state (`Keys` — its mode, a sequence,
//! count or awaited character under way, its search: never the code's, §2.3), normal mode on it with its surface's
//! table laid over.
//!
//! The code keeps `doc()`: each key's commands run with the pane's document swapped in as `current` (`swap`) and
//! put back before the key's handler returns — frames, jobs, servers and `context()` see the code's. What a key did
//! to the text is put back (`Document::put_back`) and insert mode ends there — the surface says so in its words; a
//! command that took the code to another buffer (`C-o`, `ga`, `gf`) gives the code the keys. While it has the keys
//! the pane holds still: the cursor is kept in sight only after a key or a size change (`in_sight`).

use crate::document::Document;
use crate::editor::{CharFn, Editor, Mode};
use crate::key::{Code, Key};
use crate::keymap::{Lookup, MappableCommand};
use crate::movement as mv;
use crate::selection::{Range, Selection};
use crate::session::At;
use crate::sizes::Slot;

/// Which document pane — where its `Pane` is kept.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Which {
    /// A run panel's output (`Session.outputs`): the task panel's, the tests'.
    Output(Slot),
    /// The chat's transcript (`Chat.transcript`).
    Chat,
    /// The chat's input (`Chat.input`, `chatinput.rs`) — the one that takes typing.
    Input,
}

/// The keys' state of a document that isn't the code's, traded with the editor's for each key (`swap`).
pub struct Keys {
    pub mode: Mode,
    pub pending: Vec<Key>,
    count: Option<usize>,
    on_next_char: Option<(CharFn, Option<usize>)>,
    char_for: String,
    sticky: Option<Vec<usize>>,
    search_origin: Option<usize>,
    pub search: Option<String>,
    pub search_hl: bool,
}

impl Default for Keys {
    fn default() -> Self {
        Keys {
            mode: Mode::Normal,
            pending: Vec::new(),
            count: None,
            on_next_char: None,
            char_for: String::new(),
            sticky: None,
            search_origin: None,
            search: None,
            search_hl: false,
        }
    }
}

impl Keys {
    /// Nothing under way: the next key starts something.
    pub fn idle(&self) -> bool {
        self.pending.is_empty() && self.on_next_char.is_none() && self.count.is_none()
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

    /// A count or an awaited character under way: the next key is its, whatever it is.
    pub fn waits(&self) -> bool {
        self.count.is_some() || self.on_next_char.is_some()
    }

    /// The keys gone from the pane: nothing under way, normal mode — its search kept (the pane's own).
    fn dropped(&mut self) {
        *self = Keys { search: self.search.take(), search_hl: self.search_hl, ..Keys::default() };
    }
}

/// What a key on a document pane asks for once it's done (its commands run with the document swapped in — the
/// pane can't change under them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum After {
    /// `esc` with nothing selected: to the code.
    Leave,
    /// `i`, `a`: the chat's input.
    Input,
    /// `G`: on the latest line, caught up.
    Latest,
    /// `enter` (or `space >`) on a selection: quoted to the chat (`quote.rs`).
    Take,
    /// `enter` with nothing selected: the surface's own (a run panel's: your run waiting behind, else the chat's
    /// input).
    Enter,
    /// `space >` with nothing selected: the chat's view mode.
    ChatView,
    /// Commands that act on the session, not the text in sight: run on the editor, the document put back.
    Editor,
}

/// A document pane: its document, its keys' state, its search's matches.
pub struct Pane {
    /// None only while a key's command has it swapped in (`swap`).
    doc: Option<Document>,
    pub keys: Keys,
    /// The matches its search shows (`refresh_search`).
    hits: Option<crate::search::SearchHits>,
    /// A prompt a key of its opened (`/`, `s`) is up: its keys apply here.
    pub prompt: bool,
    /// The rows and columns it was last kept in sight at (`resized`).
    pub size: Option<(usize, usize)>,
    after: Option<After>,
    deferred: Vec<MappableCommand>,
}

impl Pane {
    pub fn new(doc: Document) -> Pane {
        Pane {
            doc: Some(doc),
            keys: Keys::default(),
            hits: None,
            prompt: false,
            size: None,
            after: None,
            deferred: Vec::new(),
        }
    }

    pub fn doc(&self) -> &Document {
        self.doc.as_ref().expect("a pane's document is in it outside a key's command")
    }

    pub fn doc_mut(&mut self) -> &mut Document {
        self.doc.as_mut().expect("a pane's document is in it outside a key's command")
    }

    /// Its document, while it's in (not swapped in for a key).
    pub fn doc_in(&self) -> Option<&Document> {
        self.doc.as_ref()
    }

    pub fn doc_in_mut(&mut self) -> Option<&mut Document> {
        self.doc.as_mut()
    }

    /// Another document in its place — the old one back, to be freed (`event::drop_later`). Its search stays the
    /// pane's; its matches and its mode don't.
    pub fn replace(&mut self, doc: Document) -> Option<Document> {
        self.hits = None;
        self.keys.mode = Mode::Normal;
        self.doc.replace(doc)
    }

    /// Its document, the pane gone (to be freed on a worker).
    pub fn into_doc(self) -> Option<Document> {
        self.doc
    }

    /// The matches its search shows now.
    pub fn hits(&self) -> Option<&crate::search::SearchHits> {
        self.hits.as_ref()
    }

    /// Its lines (a last line break ends the last).
    pub fn lines(&self) -> usize {
        mv::last_line(&self.doc().text) + 1
    }

    /// More than one character selected, or several cursors — what `esc` collapses first.
    pub fn selects(&self) -> bool {
        selects(self.doc())
    }

    /// The keys went elsewhere: nothing under way, `v` ended, the selection down to its cursor — true when a prompt of
    /// its was up (the caller closes it).
    pub fn dropped(&mut self) -> bool {
        self.keys.dropped();
        if let Some(d) = self.doc.as_mut() {
            let c = d.selection().primary().cursor(&d.text);
            d.set_selection(Selection::point(c));
        }
        std::mem::take(&mut self.prompt)
    }

    /// Before drawing: the matches it shows — `typed`, the pattern its `/` prompt is typing, else its last search
    /// while shown.
    pub fn refresh_search(&mut self, typed: Option<String>) {
        let Some(d) = self.doc.as_ref() else { return };
        let pattern = match (self.prompt, typed) {
            (true, Some(t)) => Some(t).filter(|t| !t.is_empty()),
            _ => self.keys.search.clone().filter(|_| self.keys.search_hl),
        };
        self.hits = match pattern {
            Some(p) if self.hits.as_ref().is_some_and(|h| h.holds(d, &p)) => self.hits.take(),
            Some(p) => crate::search::SearchHits::of(d, &p),
            None => None,
        };
    }

    /// What its key asked for (`ask`), and the commands it left to the editor.
    pub fn take_after(&mut self) -> Option<(After, Vec<MappableCommand>)> {
        let a = self.after.take()?;
        Some((a, std::mem::take(&mut self.deferred)))
    }

    /// Commands its key left to the editor (`After::Editor`).
    pub fn deferred(&mut self, cmds: Vec<MappableCommand>) {
        self.deferred = cmds;
    }
}

/// The pane `which` names.
pub fn pane_mut(ed: &mut Editor, which: Which) -> Option<&mut Pane> {
    match which {
        Which::Output(slot) => ed.session.outputs.get_mut(slot).map(|o| &mut o.pane),
        Which::Chat => ed.session.chat.as_mut()?.transcript.as_mut().map(|t| &mut t.pane),
        Which::Input => ed.session.chat.as_mut().map(|c| &mut c.input.pane),
    }
}

/// The pane a key's command has swapped in as `current` (`commands.rs` asks before it acts on a surface).
pub fn swapped(ed: &Editor) -> Option<Which> {
    ed.session.swapped
}

/// A range of `doc`'s selection is wider than a character — what `enter` quotes (several plain cursors aren't).
pub fn wide(d: &Document) -> bool {
    d.selection().ranges().iter().any(|r| crate::graphemes::next_boundary(&d.text, r.from()) < r.to())
}

/// `doc` has more than one character selected, or several cursors.
pub fn selects(d: &Document) -> bool {
    d.selection().ranges().len() > 1 || wide(d)
}

/// The editor's mode, keys under way and search, and the pane's, change places (the terminal's view mode's too).
pub fn trade(ed: &mut Editor, k: &mut Keys) {
    std::mem::swap(&mut ed.mode, &mut k.mode);
    std::mem::swap(&mut ed.pending, &mut k.pending);
    std::mem::swap(&mut ed.count, &mut k.count);
    std::mem::swap(&mut ed.on_next_char, &mut k.on_next_char);
    std::mem::swap(&mut ed.char_for, &mut k.char_for);
    std::mem::swap(&mut ed.sticky, &mut k.sticky);
    std::mem::swap(&mut ed.search_origin, &mut k.search_origin);
    std::mem::swap(&mut ed.search, &mut k.search);
    std::mem::swap(&mut ed.search_hl, &mut k.search_hl);
}

/// What a key's commands did that a read-only pane takes back (`swap`).
#[derive(Clone, Copy, Debug, Default)]
pub struct Swapped {
    /// Its text changed: put back.
    pub edited: bool,
    /// It went into insert mode: back to normal.
    pub typing: bool,
}

impl Swapped {
    /// Refused, to be said: an edit, or typing.
    pub fn refused(self) -> bool {
        self.edited || self.typing
    }
}

/// Runs `f` with `which`'s document as `current`, its mode, keys under way and `size` (rows, columns) as the
/// editor's — all put back before it returns. What it did to the text is put back too, and insert mode ends there
/// (the caller says so); a command that took the code to another buffer gives the code the keys (the code's jumps
/// are its own then). The chat's input takes typing: its edits stay, its insert mode too.
pub fn swap<R>(
    ed: &mut Editor,
    which: Which,
    size: (usize, usize),
    f: impl FnOnce(&mut Editor) -> R,
) -> Option<(R, Swapped)> {
    let p = pane_mut(ed, which)?;
    let doc = p.doc.take()?;
    let mut keys = std::mem::take(&mut p.keys);
    let (id, version, before) = (doc.id, doc.version(), doc.snapshot());
    let code = ed.doc().id;
    ed.docs.push(doc);
    ed.current = ed.docs.len() - 1;
    trade(ed, &mut keys);
    ed.session.swapped = Some(which);
    let viewport = std::mem::replace(&mut ed.viewport, size);
    let focus = ed.session.focus;
    let jumps = ed.session.views.iter().find(|w| w.id == focus).map(|w| w.jumps.clone());
    let prompt = ed.prompt.is_some();
    let input = which == Which::Input;
    let r = f(ed);
    ed.viewport = viewport;
    ed.session.swapped = None;
    let at = ed.docs.iter().rposition(|d| d.id == id).expect("the pane's document is still in");
    // (a command that showed the code another buffer — `C-o`, `ga`, `gf` — left it current)
    let moved = ed.current != at;
    let now = ed.docs[ed.current].id;
    let mut doc = ed.docs.remove(at);
    let back = if moved { now } else { code };
    let index = |id| ed.docs.iter().position(|d| d.id == id);
    ed.current = index(back).or_else(|| index(code)).unwrap_or(0);
    if !moved && let (Some(jumps), Some(w)) = (jumps, ed.session.views.iter_mut().find(|w| w.id == focus)) {
        w.jumps = jumps;
    }
    trade(ed, &mut keys);
    // Read-only: an edit put back, insert mode left (the input: insert mode whatever its key did)
    let edited = !input && doc.version() != version;
    if edited {
        doc.put_back(before);
    }
    let typing = !input && keys.mode == Mode::Insert;
    if typing {
        keys.mode = Mode::Normal;
        ed.insert_rec = None;
    }
    if input {
        keys.mode = Mode::Insert;
        doc.drop_history();
    }
    let up = ed.prompt.is_some();
    let p = pane_mut(ed, which).expect("a pane lasts through its key");
    p.doc = Some(doc);
    p.keys = keys;
    // (a prompt it opened — `/`, `s` — is its until it closes)
    p.prompt = match up {
        true => p.prompt || !prompt,
        false => false,
    };
    if moved {
        ed.session.at = At::Code;
    }
    Some((r, Swapped { edited, typing }))
}

/// `d`'s cursor line in sight in `rows` × `cols`, the scrolloff margin `so` kept (one line a row: nothing wraps),
/// and its column.
pub fn in_sight(d: &mut Document, (rows, cols): (usize, usize), so: usize, tab: usize) {
    let so = so.min(rows.saturating_sub(1) / 2);
    let last = mv::last_line(&d.text);
    let cursor = d.selection().primary().cursor(&d.text);
    let line = mv::line_of(&d.text, cursor);
    let mut top = d.top.min(last);
    if line < top + so {
        top = line.saturating_sub(so);
    } else if line + so >= top + rows {
        top = (line + so + 1).saturating_sub(rows);
    }
    // (no further down than the last line at the bottom)
    let top = top.min((last + 1).saturating_sub(rows));
    d.set_top(top);
    let col = mv::visual_col(&d.text, cursor, tab);
    if col < d.left {
        d.left = col;
    } else if col >= d.left + cols {
        d.left = col + 1 - cols;
    }
}

/// A command on a document pane asks for what's done after its key (`After`).
pub fn ask(ed: &mut Editor, after: After) {
    if let Some(p) = swapped(ed).and_then(|w| pane_mut(ed, w)) {
        p.after = Some(after);
    }
}

/// `esc` on a document pane: a selection collapses to the primary cursor first (and `v` ends); nothing selected, to
/// the code.
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

/// `key` with a pane's document swapped in: the awaited character (`f`), a count, the command `lookup` finds for
/// the keys under way — one that's the editor's (`editors`: acts on the session, not the text) left for after the
/// key (`After::Editor`). A key bound to nothing does nothing, as in the code.
pub fn key_in(
    ed: &mut Editor,
    key: Key,
    lookup: impl for<'a> Fn(&'a Editor, Mode, &[Key]) -> Lookup<'a>,
    editors: impl Fn(&Editor, &MappableCommand) -> bool,
) {
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
    let cmds = match lookup(ed, ed.mode, &ed.pending) {
        Lookup::Pending => return,
        Lookup::Matched(cmds) => Some(cmds.to_vec()),
        Lookup::NotFound => None,
    };
    let keys = std::mem::take(&mut ed.pending);
    let count = ed.count.take();
    match cmds {
        // (the editor's own, once the document is put back — `tab`, `]x`, `space ?`)
        Some(cmds) if cmds.iter().all(|c| editors(ed, c)) => {
            if let Some(p) = swapped(ed).and_then(|w| pane_mut(ed, w)) {
                p.deferred = cmds;
            }
            ask(ed, After::Editor);
        }
        Some(cmds) => {
            ed.run(&cmds, count);
            if ed.on_next_char.is_some() {
                ed.char_for = keys.iter().map(Key::to_string).collect();
            }
        }
        None => {}
    }
}
