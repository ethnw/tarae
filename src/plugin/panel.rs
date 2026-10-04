//! What a plugin shows beyond the buffer: its panel (a table below the code), a line to type at, and
//! buffers it fills.
//!
//! - **The panel** takes the keys while it's focused — as a plugin command shows it, or with a click. tarae
//!   moves through its rows (`j` `k` `g` `G`, `C-d` `C-u`, the arrows and the wheel); `esc` gives the keys
//!   back to the code; `space` and `:` stay the editor's. Every other key goes to the plugin (`key`), with
//!   the row selected then — the plugin decides what `d` or `enter` does.
//!   With row ids, the selection stays on its row as rows come, go and move, and a key says which row it
//!   was on. A panel that's out of sight (a terminal or the debugger in its place) doesn't take keys.
//! - **Its line** (`prompt`): the command line, labelled with the plugin's name; `enter` answers, `esc`
//!   answers nothing. Only in answer to you (a command, a key in its panel, a line you typed) — a plugin
//!   can't pull you into typing while you're doing something else — and not over a line being typed.
//! - **Beside the code** (`hints`): text at the end of a buffer's lines — for the version of the buffer the
//!   plugin worked them out on; edited, they wait for the plugin's next ones.
//! - **Its side tab** (`side`): a tree beside files, outline and git (sidebar.rs draws it, folds it, and
//!   marks the row for the code under the cursor); keys it doesn't take itself go to the plugin.
//! - **Its badge** (`badge`): a short text in the status line, on every buffer.
//! - **Its pick** (`pick`): tarae's picker with its items; the choice is an answer. Like its line, only in
//!   answer to you.
//! - **Its buffers** (`show`, `append`): throwaway buffers titled by the plugin, one per id — shown again,
//!   replaced; appended to, their end grows (a log). One shown takes the keys from the panel. Once you
//!   close one the plugin is told (`failed`, with its id), and appending to it fails — so it can stop
//!   what it was following.

use std::cell::Cell;

use super::abi::{Hints, HostEvent, Panel, Pick, Side, Tone, View};
use super::shown;
use crate::editor::{Editor, Mode, PromptKind};
use crate::key::{Code, Key};

/// The most rows a panel holds (past that, what it sent is cut).
const MAX_ROWS: usize = 50_000;
/// The most hints for a buffer, the longest one, a card's rows, a pick's items.
const MAX_HINTS: usize = 10_000;
const MAX_HINT: usize = 160;
const MAX_SIDE_ROWS: usize = 5_000;
const MAX_PICK: usize = 50_000;

/// A plugin's text made one short line.
fn one_line(s: &str, max: usize) -> String {
    shown(s).replace(['\n', '\t'], " ").chars().take(max).collect()
}

/// A plugin's panel as the editor holds it.
pub struct PanelState {
    pub plugin: String,
    pub panel: Panel,
    pub selected: usize,
    /// The first row in sight (drawing keeps the selected one in sight).
    pub scroll: Cell<usize>,
    pub focus: bool,
    /// Each column's widest cell (as shown), worked out once per panel rather than every frame.
    pub widths: Vec<usize>,
}

/// Each column's widest cell, as shown.
fn widths(panel: &Panel) -> Vec<usize> {
    use unicode_width::UnicodeWidthStr;
    let cols = panel.columns.len().max(panel.rows.iter().map(Vec::len).max().unwrap_or(0));
    let mut w = vec![0; cols];
    for row in std::iter::once(&panel.columns).chain(&panel.rows) {
        for (i, c) in row.iter().enumerate() {
            w[i] = w[i].max(shown(c).replace(['\n', '\t'], " ").width());
        }
    }
    w
}

/// The panel's keys, while it's focused. true if taken.
pub fn key(ed: &mut Editor, key: Key) -> bool {
    if ed.mode != Mode::Normal || !ed.pending.is_empty() || ed.prompt.is_some() {
        return false;
    }
    if ed.lane.sidebar.as_ref().is_some_and(|s| s.focus) || !ed.plugin_panel_in_sight() {
        return false;
    }
    let Some(st) = ed.plugins.as_mut().and_then(|p| p.panel.as_mut()).filter(|s| s.focus) else {
        return false;
    };
    let n = st.panel.rows.len();
    let step = |st: &mut PanelState, by: isize| {
        st.selected = st.selected.saturating_add_signed(by).min(n.saturating_sub(1));
    };
    match (key.code, key.ctrl, key.alt) {
        // Still the editor's: the space menu, the command line
        (Code::Char(' ') | Code::Char(':'), false, false) => return false,
        (Code::Esc, ..) => st.focus = false,
        (Code::Char('j') | Code::Down, false, false) | (Code::Char('n'), true, false) => step(st, 1),
        (Code::Char('k') | Code::Up, false, false) | (Code::Char('p'), true, false) => step(st, -1),
        (Code::Char('d'), true, false) | (Code::PageDown, ..) => step(st, 10),
        (Code::Char('u'), true, false) | (Code::PageUp, ..) => step(st, -10),
        (Code::Char('g') | Code::Home, false, false) => st.selected = 0,
        (Code::Char('G') | Code::End, false, false) => st.selected = n.saturating_sub(1),
        _ => {
            let (plugin, row) = (st.plugin.clone(), (n > 0).then_some(st.selected as u64));
            let id = row.and_then(|_| st.panel.ids.get(st.selected).cloned());
            ed.plugin_events(&plugin, vec![HostEvent::Key(key.to_string(), row, id)]);
        }
    }
    true
}

impl Editor {
    /// A plugin's `panel`: shown (replacing what it showed — the selection stays on its row: the one with
    /// its id, or else where it was), or closed. A plugin command's answer focuses it.
    pub(super) fn plugin_panel(&mut self, plugin: &str, panel: Option<Panel>) {
        // (not while a card asks about a plugin: its keys are the card's)
        let asking = self.offers.first().is_some_and(|o| {
            matches!(
                o.what,
                crate::offer::What::PluginPermissions { .. } | crate::offer::What::PluginAsk { .. }
            )
        });
        let Some(p) = &mut self.plugins else { return };
        let focus = p.answering.as_deref() == Some(plugin) && !asking;
        match panel {
            None => {
                if p.panel.as_ref().is_some_and(|s| s.plugin == plugin) {
                    p.panel = None;
                }
            }
            Some(mut panel) => {
                panel.rows.truncate(MAX_ROWS);
                panel.ids.truncate(panel.rows.len());
                let widths = widths(&panel);
                let st = match p.panel.take() {
                    Some(mut st) if st.plugin == plugin => {
                        let was = st.panel.ids.get(st.selected);
                        let now = was.and_then(|id| panel.ids.iter().position(|i| i == id));
                        st.selected = now.unwrap_or(st.selected).min(panel.rows.len().saturating_sub(1));
                        st.panel = panel;
                        st.widths = widths;
                        st.focus |= focus;
                        st
                    }
                    _ => PanelState {
                        plugin: plugin.to_string(),
                        panel,
                        selected: 0,
                        scroll: Cell::new(0),
                        focus,
                        widths,
                    },
                };
                p.panel = Some(st);
                if focus {
                    self.plugin_panel_took_keys();
                }
            }
        }
    }

    /// The panel took the keys: nothing else keeps them.
    fn plugin_panel_took_keys(&mut self) {
        if let Some(s) = self.lane.sidebar.as_mut() {
            s.focus = false;
        }
        if let Some(c) = self.lane.chat.as_mut() {
            c.focused = false;
        }
    }

    /// A click on the panel's row `row` (None: its header): focused, that row selected.
    pub fn plugin_panel_click(&mut self, row: Option<usize>) {
        let Some(st) = self.plugins.as_mut().and_then(|p| p.panel.as_mut()) else { return };
        st.focus = true;
        if let Some(r) = row.filter(|&r| r < st.panel.rows.len()) {
            st.selected = r;
        }
        self.plugin_panel_took_keys();
    }

    /// The wheel over the panel: the selection moves.
    pub fn plugin_panel_wheel(&mut self, by: isize) {
        if let Some(st) = self.plugins.as_mut().and_then(|p| p.panel.as_mut()) {
            st.selected = st.selected.saturating_add_signed(by).min(st.panel.rows.len().saturating_sub(1));
        }
    }

    /// Whether the panel has the keys.
    pub fn plugin_panel_focused(&self) -> bool {
        self.plugins.as_ref().and_then(|p| p.panel.as_ref()).is_some_and(|s| s.focus)
            && self.plugin_panel_in_sight()
    }

    /// Whether the panel is drawn: it shares its place with the debugger and the terminal, which come first.
    pub fn plugin_panel_in_sight(&self) -> bool {
        self.plugins.as_ref().is_some_and(|p| p.panel.is_some())
            && !self.procs_up()
            && self.dap.is_none()
            && !self.lane.terminal.as_ref().is_some_and(|t| t.visible)
    }

    /// Code clicked: the panel gives the keys back.
    pub fn plugin_panel_unfocus(&mut self) {
        if let Some(st) = self.plugins.as_mut().and_then(|p| p.panel.as_mut()) {
            st.focus = false;
        }
    }

    /// A plugin's `prompt`: the command line with its name and label — if it's answering you, and no line
    /// is being typed (else it fails).
    pub(super) fn plugin_prompt(&mut self, plugin: &str, id: u64, label: &str) {
        let Some(p) = &mut self.plugins else { return };
        let why = if p.answering.as_deref() != Some(plugin) {
            Some("a line is asked for only in answer to you (a command, a key in its panel, a line typed)")
        } else if self.prompt.is_some() || self.mode != Mode::Normal {
            Some("you're typing something else")
        } else {
            None
        };
        if let Some(why) = why {
            return self.plugin_events(plugin, vec![HostEvent::Failed(id, why.into())]);
        }
        p.serial += 1;
        let token = p.serial;
        p.prompts.insert(token, (plugin.to_string(), id));
        let label: String = shown(label).replace('\n', " ").chars().take(40).collect();
        self.open_prompt(PromptKind::Plugin(token), "");
        if let Some(pr) = self.prompt.as_mut() {
            pr.label = Some(format!("{plugin} · {label}:"));
        }
    }

    /// What was typed at a plugin's line (None: esc).
    pub(crate) fn plugin_prompt_done(&mut self, token: u64, text: Option<String>) {
        let Some((plugin, id)) = self.plugins.as_mut().and_then(|p| p.prompts.remove(&token)) else { return };
        self.plugin_events(&plugin, vec![HostEvent::Answer(id, text)]);
    }

    /// A plugin's `hints` for a buffer (by path): they replace its earlier ones there; none clears them.
    pub(super) fn plugin_hints(&mut self, plugin: &str, h: Hints) {
        let Some(p) = &mut self.plugins else { return };
        let key = (plugin.to_string(), h.path);
        if h.hints.is_empty() {
            p.hints.remove(&key);
            return;
        }
        let hints = h
            .hints
            .into_iter()
            .take(MAX_HINTS)
            .map(|(l, t, tone)| (l, one_line(&t, MAX_HINT), tone))
            .collect();
        p.hints.insert(key, (h.version, hints));
    }

    /// The hints beside buffer `doc`'s lines, if they're for the text it has now: (line, text, tone).
    pub fn plugin_hints_for<'a>(
        &'a self,
        doc: &'a crate::document::Document,
    ) -> impl Iterator<Item = (usize, &'a str, Tone)> + 'a {
        let path = doc.path.as_ref().map(|p| p.display().to_string());
        let version = doc.version();
        self.plugins
            .iter()
            .flat_map(|p| p.hints.iter())
            .filter(move |((_, at), (v, _))| Some(at) == path.as_ref() && *v == version)
            .flat_map(|(_, (_, hs))| hs.iter().map(|(l, t, tone)| (*l as usize, t.as_str(), *tone)))
    }

    /// A plugin's `side`: its tab in the side panel (replacing any) — shown now if it asks, without the
    /// keys; or gone.
    pub(super) fn plugin_side(&mut self, plugin: &str, side: Option<Side>) {
        let Some(p) = &mut self.plugins else { return };
        let Some(mut side) = side else {
            if p.side.as_ref().is_some_and(|(q, _)| q == plugin) {
                p.side = None;
            }
            return;
        };
        side.title = one_line(&side.title, 20);
        side.note = one_line(&side.note, 60);
        side.rows.truncate(MAX_SIDE_ROWS);
        for r in &mut side.rows {
            r.text = one_line(&r.text, 120);
            r.detail = one_line(&r.detail, 80);
            r.depth = r.depth.min(12);
        }
        let (open, focus) = (std::mem::take(&mut side.open), std::mem::take(&mut side.focus));
        p.side = Some((plugin.to_string(), side));
        if open || focus {
            match self.lane.sidebar.as_mut() {
                Some(s) => {
                    s.mode = crate::sidebar::Mode::Plugin;
                    s.shown = None;
                    s.focus |= focus;
                }
                None => {
                    let mut s =
                        crate::sidebar::Sidebar::new(crate::sidebar::Mode::Plugin, self.lane.root.clone());
                    s.focus = focus;
                    self.lane.sidebar = Some(s);
                }
            }
            if focus && let Some(c) = self.lane.chat.as_mut() {
                c.focused = false;
            }
        }
        self.sidebar_sync();
        // Given the keys: on the row for the code under the cursor
        if focus
            && let Some(h) = self.lane.sidebar.as_ref().and_then(|s| s.here(self))
            && let Some(s) = self.lane.sidebar.as_mut()
        {
            s.selected = h;
        }
    }

    /// A key in a plugin's side tab, on its row `row`: to the plugin.
    pub(crate) fn plugin_side_key(&mut self, key: &str, row: usize) {
        let Some((plugin, side)) = self.plugins.as_ref().and_then(|p| p.side.as_ref()) else { return };
        let id = side.rows.get(row).map(|r| r.id.clone());
        let (plugin, ev) = (plugin.clone(), HostEvent::Key(key.to_string(), Some(row as u64), id));
        self.plugin_events(&plugin, vec![ev]);
    }

    /// A plugin's `badge`: its text in the status line (or none).
    pub(super) fn plugin_badge(&mut self, plugin: &str, badge: Option<(String, Tone)>) {
        let Some(p) = &mut self.plugins else { return };
        match badge {
            Some((text, tone)) => p.badge = Some((plugin.to_string(), one_line(&text, 40), tone)),
            None if p.badge.as_ref().is_some_and(|(q, ..)| q == plugin) => p.badge = None,
            None => {}
        }
    }

    /// A plugin's `pick`: tarae's picker with its items — if it's answering you, and nothing else asks.
    pub(super) fn plugin_pick(&mut self, plugin: &str, pick: Pick) {
        let Some(p) = &mut self.plugins else { return };
        let why = if p.answering.as_deref() != Some(plugin) {
            Some("a pick is offered only in answer to you (a command, a key, a click)")
        } else if self.prompt.is_some() || self.picker.is_some() || self.mode != Mode::Normal {
            Some("you're typing something else")
        } else {
            None
        };
        if let Some(why) = why {
            return self.plugin_events(plugin, vec![HostEvent::Failed(pick.id, why.into())]);
        }
        p.serial += 1;
        let token = p.serial;
        p.prompts.insert(token, (plugin.to_string(), pick.id));
        let items = pick
            .items
            .into_iter()
            .take(MAX_PICK)
            .map(|(id, label)| crate::picker::Item {
                label: one_line(&label, 200),
                action: crate::picker::Action::Plugin { token, id },
                hint: String::new(),
                glyph: None,
            })
            .collect();
        let title = format!("{plugin} · {}", one_line(&pick.title, 60));
        // (nothing to preview: the small picker, over the code)
        let mut picker = crate::picker::Picker::new(title, items, false);
        picker.compact = true;
        self.open_picker(picker, None);
    }

    /// A plugin's `show`: its buffer for that id, new or replaced, in front.
    pub(super) fn plugin_show(&mut self, plugin: &str, view: View) {
        let key = (plugin.to_string(), view.id);
        if let Some(old) = self.plugins.as_mut().and_then(|p| p.views.remove(&key)) {
            self.close_doc(old);
        }
        let title = format!("{} · {plugin}", shown(&view.title).replace('\n', " "));
        let id = self.open_virtual(&title, &view.text);
        if let Some(spec) = view.language.as_deref().and_then(crate::syntax::spec) {
            self.load_language(id, spec);
        }
        if let Some(p) = &mut self.plugins {
            p.views.insert(key, id);
        }
        // The buffer is what you're looking at now: its keys, not the panel's or the side tab's — not even
        // if this answer shows the panel again after it
        self.plugin_panel_unfocus();
        if let Some(s) = self.lane.sidebar.as_mut().filter(|s| s.mode == crate::sidebar::Mode::Plugin) {
            s.focus = false;
        }
        if let Some(p) = &mut self.plugins {
            p.answering = None;
        }
    }

    /// A plugin's `append`: to the end of the buffer it showed with that id — if it's still open (else it's
    /// told, and it's forgotten).
    pub(super) fn plugin_append(&mut self, plugin: &str, id: u64, text: &str) -> Result<(), String> {
        let key = (plugin.to_string(), id);
        let doc = self.plugins.as_ref().and_then(|p| p.views.get(&key)).copied();
        let Some(i) = doc.and_then(|d| self.docs.iter().position(|x| x.id == d)) else {
            if let Some(p) = &mut self.plugins {
                p.views.remove(&key);
            }
            self.plugin_events(plugin, vec![HostEvent::Failed(id, "its buffer is closed".into())]);
            return Ok(());
        };
        let doc = &mut self.docs[i];
        let end = doc.text.len_chars();
        // The cursor at the end follows it (a log being watched); anywhere else, it stays
        let following = doc.selection().primary().cursor(&doc.text) >= doc.text.len_bytes().saturating_sub(1);
        let at = doc.text.char_to_byte(end);
        let tx = crate::transaction::Transaction::new(vec![crate::transaction::Change {
            from: at,
            to: at,
            insert: shown(text),
        }]);
        doc.apply(&tx);
        // (the plugin's text, not an edit of yours: nothing to save)
        doc.mark_saved();
        if following {
            let end = doc.text.len_bytes();
            doc.set_selection(crate::selection::Selection::point(end));
        }
        Ok(())
    }
}
