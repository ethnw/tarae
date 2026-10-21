//! The chat's input as a buffer (panes P2c, `target/sb/panes-plan.md` §2.1e): a small document in insert mode, the
//! one place in the chat that takes typing. Its keys are the editor's insert mode's (`[insert]`, yours too) with the
//! chat's `[chat]` over them (`esc` → its view mode, `C-a`/`C-e` a line's ends, `A-ret` a new line), run with the
//! document swapped in (`docpane::swap`, `Which::Input`) — what isn't bound is typed, without auto-pairs (it's
//! prose). The chat's own keys come first (`chat::key`): `enter` sends, its chords, the page keys, `↑`/`↓` and `tab`
//! from an empty input, backspace taking a chip off — as they were before it was a buffer.

use crate::docpane::{After, Pane, Which};
use crate::document::Document;
use crate::editor::{Editor, Mode};
use crate::key::{Code, Key};
use crate::keymap::{KeyTrie, Lookup, MappableCommand};
use crate::selection::Selection;
use crate::transaction::{Change, Transaction};

/// The input's rows at most (more scroll), as the chat draws it.
pub const ROWS: usize = 5;

/// The chat's input: its document pane, always in insert mode.
pub struct Input {
    pub pane: Pane,
}

impl Default for Input {
    fn default() -> Input {
        let mut doc = Document::from_str(crate::document::CHAT_INPUT, "");
        doc.throwaway = true;
        // (its lines as written: `↑`/`↓` move by them, the drawer wraps them)
        doc.wrap = Some(false);
        let mut pane = Pane::new(doc);
        pane.keys.mode = Mode::Insert;
        Input { pane }
    }
}

/// An input holding `text`, the cursor at its end.
impl From<&str> for Input {
    fn from(text: &str) -> Input {
        let mut i = Input::default();
        i.set(text);
        i
    }
}

impl From<String> for Input {
    fn from(text: String) -> Input {
        Input::from(text.as_str())
    }
}

impl std::fmt::Debug for Input {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self.text())
    }
}

impl PartialEq<&str> for Input {
    fn eq(&self, other: &&str) -> bool {
        self.pane.doc().text == *other
    }
}

impl Input {
    /// What's typed.
    pub fn text(&self) -> String {
        self.pane.doc().text.to_string()
    }

    pub fn is_empty(&self) -> bool {
        self.pane.doc().text.len_bytes() == 0
    }

    /// Nothing typed but blanks — nothing to send.
    pub fn blank(&self) -> bool {
        self.pane.doc().text.chars().all(char::is_whitespace)
    }

    /// Where the cursor is (bytes).
    pub fn cursor(&self) -> usize {
        self.pane.doc().selection().primary().head
    }

    /// `text` in its place, the cursor at `cursor` (on a character boundary, else its end). A sequence of
    /// `[insert]`'s under way goes with what it replaces.
    pub fn set_at(&mut self, text: &str, cursor: usize) {
        self.pane.keys.pending.clear();
        let d = self.pane.doc_mut();
        let tx = Transaction::new(vec![Change { from: 0, to: d.text.len_bytes(), insert: text.to_string() }]);
        d.apply(&tx);
        let at = if text.is_char_boundary(cursor) { cursor.min(text.len()) } else { text.len() };
        d.set_selection(Selection::point(at));
    }

    /// `text` in its place, the cursor at its end.
    pub fn set(&mut self, text: &str) {
        self.set_at(text, text.len());
    }

    pub fn clear(&mut self) {
        self.set("");
    }

    /// `text` typed at the cursor, the cursor after it (a paste, a key that brought the chat back) — after what a
    /// sequence under way had typed (`j` of `j k`: a paste breaks it off, as a key does).
    pub fn insert_str(&mut self, text: &str) {
        let text = self.held() + text;
        if text.is_empty() {
            return;
        }
        let d = self.pane.doc_mut();
        let at = d.selection().primary().head;
        d.apply(&Transaction::new(vec![Change::insert(at, text.clone())]));
        d.set_selection(Selection::point(at + text.len()));
    }

    /// What a sequence under way had typed, taken out of it (its keys that are characters).
    fn held(&mut self) -> String {
        std::mem::take(&mut self.pane.keys.pending).iter().filter_map(Key::plain_char).collect()
    }
}

/// A key for the input's document (`chat::key`, once the chat's own keys didn't take it): `[chat]` then
/// `[insert]`, run with it swapped in; what neither binds, typed — a `C-`/`A-` key nothing binds is said. Always
/// taken.
pub fn key(ed: &mut Editor, key: Key) -> bool {
    let cols = ed.screen.chat_view.get().map_or(60, |a| a.1).saturating_sub(2).max(1);
    crate::docpane::swap(ed, Which::Input, (ROWS, cols), |ed| typed(ed, key));
    let Some((a, deferred)) = ed.session.chat.as_mut().and_then(|c| c.input.pane.take_after()) else {
        return true;
    };
    if a == After::Editor {
        ed.run(&deferred, None);
    }
    true
}

/// A sequence of the input's under way (the `j` of `[keys.insert] j = { k = … }`): whether `key` goes on with it —
/// then it's the input's (`key`). Else what was typed of it is text, put in first, as a sequence broken off is in
/// the code: the chat's own key after it (`enter` sends) takes the input with it, never leaves it for the next.
pub fn under_way(ed: &mut Editor, key: Key) -> bool {
    let Some(pending) =
        ed.session.chat.as_ref().map(|c| &c.input.pane.keys.pending).filter(|p| !p.is_empty())
    else {
        return false;
    };
    let mut keys = pending.clone();
    keys.push(key);
    if !matches!(lookup(ed, &keys), Lookup::NotFound) {
        return true;
    }
    let Some(c) = ed.session.chat.as_mut() else { return false };
    // (what it typed so far goes in first: `insert_str` puts it before what it's given — here nothing)
    c.input.insert_str("");
    false
}

/// Where `esc` goes from the input, as its hint and its notices say it: normal mode on the conversation (`esc
/// normal`, its key as bound) — nothing to read yet, the code.
pub fn esc_said(ed: &Editor) -> (String, &'static str) {
    let to_view = ed.keymaps.chat.keys_for("chat_view").into_iter().next();
    match to_view {
        Some(k) if ed.session.chat.as_ref().is_some_and(|c| !c.msgs.is_empty()) => (k, "normal"),
        // (`esc code` where the surface stays, `esc close` where it goes — the user, 2026-10-20)
        _ => ("esc".into(), "code"),
    }
}

/// `key` with the input swapped in.
fn typed(ed: &mut Editor, key: Key) {
    ed.pending.push(key);
    let cmds = match lookup(ed, &ed.pending) {
        Lookup::Pending => return,
        Lookup::Matched(cmds) => Some(cmds.to_vec()),
        Lookup::NotFound => None,
    };
    let keys = std::mem::take(&mut ed.pending);
    match cmds {
        // (the editor's own — `esc` to its view mode, the window table's, a `:` line — once it's put back)
        Some(cmds) if cmds.iter().all(|c| editors(ed, c)) => {
            if let Some(p) = ed.session.chat.as_mut().map(|c| &mut c.input.pane) {
                p.deferred(cmds);
            }
            crate::docpane::ask(ed, After::Editor);
        }
        Some(cmds) => ed.run(&cmds, None),
        // (a sequence broken off — `j x` under `j k` — is text, as in the code; a last key that isn't a
        // character, said)
        None => {
            let chars: Vec<char> = keys.iter().filter_map(Key::plain_char).collect();
            if !chars.is_empty() {
                ed.with_group(|cx| chars.into_iter().for_each(|c| crate::commands::insert_char(cx, c)));
            }
            if let Some(&last) =
                keys.last().filter(|k| k.plain_char().is_none() && matches!(k.code, Code::Char(_)))
            {
                let (k, to) = esc_said(ed);
                ed.key_unused(last, "the chat", &format!("{k} {to}"));
            }
        }
    }
}

/// The keys under way in `[chat]`, else in `[insert]`.
fn lookup<'a>(ed: &'a Editor, keys: &[Key]) -> Lookup<'a> {
    for table in [&ed.keymaps.chat, &ed.keymaps.insert] {
        match table.search(keys) {
            Some(KeyTrie::Leaf(cmds)) => return Lookup::Matched(cmds),
            Some(KeyTrie::Node(_)) => return Lookup::Pending,
            None => {}
        }
    }
    Lookup::NotFound
}

/// A command a key of the input runs on the editor, the input put back first: its view mode, the window table's,
/// `view::EDITORS`, a `:` line bound to a key.
fn editors(ed: &Editor, c: &MappableCommand) -> bool {
    match c {
        MappableCommand::Static(s) => {
            s.name == "chat_view"
                || crate::view::EDITORS.contains(&s.name)
                || ed.keymaps.in_window_table(s.name)
        }
        MappableCommand::Typed(_) => true,
    }
}

#[cfg(test)]
mod tests;
