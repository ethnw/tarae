//! Keymap trie — reads helix `[keys.<mode>]` TOML syntax as is.
//!
//! Values: `"command"` · `":typed command"` · `["command", ...]` (run in sequence) ·
//! table (submode, e.g. `g`, `space`).
//! Defaults are in `default_keys.toml` — read by the same parser, so that file doubles as the docs
//! and a parser test.

use std::collections::HashMap;

use crate::commands::{self, StaticCommand};
use crate::editor::Mode;
use crate::key::Key;

#[derive(Clone, Debug)]
pub enum MappableCommand {
    Static(&'static StaticCommand),
    /// The command line after `:` (e.g. `"w"`, `"sh zellij run -- lazygit"`).
    Typed(String),
}

impl MappableCommand {
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.strip_prefix(':') {
            Some(line) => Ok(Self::Typed(line.trim().to_string())),
            None => commands::find(s).map(Self::Static).ok_or_else(|| format!("unknown command: {s}")),
        }
    }

    /// What it does, in plain words — the command's doc, or the command line itself for `:cmd`.
    pub fn doc(&self) -> String {
        match self {
            Self::Static(c) => c.doc.to_string(),
            Self::Typed(line) => format!(":{line}"),
        }
    }
}

#[derive(Clone, Debug)]
pub enum KeyTrie {
    Leaf(Vec<MappableCommand>),
    Node(Node),
}

#[derive(Clone, Debug, Default)]
pub struct Node {
    map: HashMap<Key, KeyTrie>,
}

impl Node {
    /// Overrides with `other` — submodes merge recursively, everything else is replaced.
    pub fn merge(&mut self, other: Node) {
        for (key, trie) in other.map {
            match trie {
                KeyTrie::Node(sub) => match self.map.get_mut(&key) {
                    Some(KeyTrie::Node(mine)) => mine.merge(sub),
                    _ => {
                        self.map.insert(key, KeyTrie::Node(sub));
                    }
                },
                leaf => {
                    self.map.insert(key, leaf);
                }
            }
        }
    }

    /// Entries in display order (which-key, docs): single characters before named keys (`esc`, `C-w`),
    /// alphabetical, lowercase before uppercase. Total (`A-C`/`A-c` tie otherwise), so the order is stable.
    fn sorted(&self) -> Vec<(&Key, &KeyTrie)> {
        // (each key's text made once)
        let mut keyed: Vec<_> = self
            .map
            .iter()
            .map(|e| {
                let s = e.0.to_string();
                (
                    (
                        s.chars().count() > 1,
                        s.to_lowercase(),
                        s.chars().next().is_some_and(char::is_uppercase),
                        s,
                    ),
                    e,
                )
            })
            .collect();
        crate::sorting::by(&mut keyed, |a, b| a.0.cmp(&b.0));
        keyed.into_iter().map(|(_, e)| e).collect()
    }

    /// Only the bindings every command of which `keep` takes (view mode's: `view::COMMANDS`) — a submode kept
    /// while something in it is.
    fn filtered(&self, keep: &dyn Fn(&str) -> bool) -> Node {
        let mut node = Node::default();
        for (k, t) in &self.map {
            match t {
                KeyTrie::Node(n) => {
                    let sub = n.filtered(keep);
                    if !sub.map.is_empty() {
                        node.map.insert(*k, KeyTrie::Node(sub));
                    }
                }
                KeyTrie::Leaf(cmds) => {
                    let all = cmds.iter().all(|c| matches!(c, MappableCommand::Static(s) if keep(s.name)));
                    if all {
                        node.map.insert(*k, t.clone());
                    }
                }
            }
        }
        node
    }

    /// The keys bound to `name` here, shortest first (a hint names the key as bound — a remap moves it).
    pub fn keys_for(&self, name: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .map
            .iter()
            .filter(|(_, t)| {
                matches!(t, KeyTrie::Leaf(c) if matches!(c.as_slice(), [MappableCommand::Static(s)] if s.name == name))
            })
            .map(|(k, _)| k.to_string())
            .collect();
        crate::sorting::by(&mut out, |a, b| (a.len(), a).cmp(&(b.len(), b)));
        out
    }

    /// The shortest sequence here bound to `cmds` none of whose keys zellij takes — a taken key's twin
    /// (`zellij.rs`).
    fn twin(&self, cmds: &[MappableCommand], letters: &[char]) -> Option<String> {
        self.untaken(&cmd_names(cmds), letters)
    }

    /// The shortest sequence here bound to the commands named `want`, none of its keys zellij's — one walk, only
    /// the keys zellij leaves (no map of every binding: it's asked while a frame is drawn).
    fn untaken(&self, want: &[String], letters: &[char]) -> Option<String> {
        fn walk(node: &Node, prefix: &str, want: &[String], letters: &[char], best: &mut Option<String>) {
            for (k, t) in node.map.iter().filter(|(k, _)| !crate::zellij::takes(k, letters)) {
                let seq = if prefix.is_empty() { k.shown() } else { format!("{prefix} {}", k.shown()) };
                match t {
                    KeyTrie::Node(n) => walk(n, &seq, want, letters, best),
                    KeyTrie::Leaf(c) if cmd_names(c) == want => {
                        if best.as_ref().is_none_or(|b| (seq.len(), &seq) < (b.len(), b)) {
                            *best = Some(seq);
                        }
                    }
                    KeyTrie::Leaf(_) => {}
                }
            }
        }
        let mut best = None;
        walk(self, "", want, letters, &mut best);
        best
    }

    pub fn search(&self, keys: &[Key]) -> Option<&KeyTrie> {
        let mut node = self;
        for (i, key) in keys.iter().enumerate() {
            let trie = node.map.get(key)?;
            if i + 1 == keys.len() {
                return Some(trie);
            }
            match trie {
                KeyTrie::Node(n) => node = n,
                KeyTrie::Leaf(_) => return None,
            }
        }
        None
    }

    /// TOML table → node. Invalid entries are skipped and collected as warnings.
    pub fn from_toml(table: &toml::Table, path: &str, warnings: &mut Vec<String>) -> Node {
        let mut node = Node::default();
        for (k, v) in table {
            let here = format!("{path}.{k}");
            let key = match k.parse::<Key>() {
                Ok(key) => key,
                Err(e) => {
                    warnings.push(format!("{here}: {e}"));
                    continue;
                }
            };
            let names: Vec<&str> = match v {
                toml::Value::Table(t) => {
                    node.map.insert(key, KeyTrie::Node(Node::from_toml(t, &here, warnings)));
                    continue;
                }
                toml::Value::String(s) => vec![s.as_str()],
                toml::Value::Array(a) => a.iter().filter_map(|v| v.as_str()).collect(),
                _ => {
                    warnings.push(format!("{here}: expected command, list or table"));
                    continue;
                }
            };
            let parsed: Result<Vec<_>, _> = names.iter().map(|s| MappableCommand::parse(s)).collect();
            match parsed {
                Ok(cmds) if !cmds.is_empty() => {
                    node.map.insert(key, KeyTrie::Leaf(cmds));
                }
                Ok(_) => warnings.push(format!("{here}: empty command list")),
                Err(e) => warnings.push(format!("{here}: {e}")),
            }
        }
        node
    }
}

/// Command names of a binding (`:line` for typed commands) — several run in sequence.
fn cmd_names(cmds: &[MappableCommand]) -> Vec<String> {
    cmds.iter()
        .map(|c| match c {
            MappableCommand::Static(c) => c.name.to_string(),
            MappableCommand::Typed(t) => format!(":{t}"),
        })
        .collect()
}

/// A binding that gives its key back to the terminal's program (`to_program`, alone).
fn is_to_program(cmds: &[MappableCommand]) -> bool {
    matches!(cmds, [MappableCommand::Static(c)] if c.name == "to_program")
}

pub enum Lookup<'a> {
    Matched(&'a [MappableCommand]),
    Pending,
    NotFound,
}

#[derive(Clone, Debug)]
pub struct Keymaps {
    pub normal: Node,
    pub select: Node,
    pub insert: Node,
    /// View mode's own rows (`[view]`), and each surface's on top of them (`[view.chat]`) — what the view
    /// modes look keys up in is built from these and `normal`/`select` (`views`).
    view: Node,
    view_on: Vec<(&'static str, Node)>,
    /// Per surface: normal mode (the chat's whole, the terminal's kept to `view::COMMANDS`), then `[view]`, then
    /// the surface's — and the same over `select` (view mode's `v`).
    views: Vec<(&'static str, Node, Node)>,
    /// The chat's input (`[chat]`), over `insert` on its document (`chatinput.rs`) — `esc` → its view mode, `C-a`/`C-e`
    /// a line's ends, `A-ret` a new line.
    pub chat: Node,
    /// The keys taken from the terminal's program (`[terminal]`) — `C-\` → its view mode; every other key is the
    /// program's. A user's `[keys.terminal]` merges over it as every table does; `to_program` gives one back.
    pub terminal: Node,
    /// A run panel's own keys over normal mode on its output (`[run]`, `rundoc.rs`) — and what its keys are looked
    /// up in: `normal` and `select` with it on top (`runs`).
    run: Node,
    runs: (Node, Node),
}

const DEFAULT_KEYS: &str = include_str!("default_keys.toml");

impl Default for Keymaps {
    fn default() -> Self {
        let table: toml::Table = toml::from_str(DEFAULT_KEYS).expect("default_keys.toml parses");
        let mut warnings = Vec::new();
        let km = Self::from_toml_tables(&table, "default", &mut warnings);
        assert!(warnings.is_empty(), "default keymap: {warnings:?}");
        km
    }
}

/// Names of the built-in key groups (which-key shows them; groups from a user config show `…`).
pub fn group_name(path: &str) -> Option<&'static str> {
    Some(match path {
        "space g" => "Git",
        "space G" => "Debug",
        "space w" | "C-w" => "Window",
        "space x" => "Tests",
        "space m" => "Tasks",
        "space v" => "Review",
        "z" | "Z" => "View",
        "g" => "Goto",
        "m" => "Match",
        "[" => "Previous",
        "]" => "Next",
        _ => return None,
    })
}

impl Keymaps {
    /// A bundle of `{normal, select, insert}` tables. select = normal with overrides on top (as in helix).
    fn from_toml_tables(table: &toml::Table, path: &str, warnings: &mut Vec<String>) -> Self {
        let part = |mode: &str, warnings: &mut Vec<String>| match table.get(mode) {
            Some(toml::Value::Table(t)) => Node::from_toml(t, &format!("{path}.{mode}"), warnings),
            _ => Node::default(),
        };
        let normal = part("normal", warnings);
        let mut select = normal.clone();
        select.merge(part("select", warnings));
        let (view, view_on) = Self::view_tables(table, path, warnings);
        let mut km = Self {
            normal,
            select,
            insert: part("insert", warnings),
            view,
            view_on,
            views: Vec::new(),
            chat: part("chat", warnings),
            terminal: part("terminal", warnings),
            run: part("run", warnings),
            runs: Default::default(),
        };
        km.build_views();
        km
    }

    /// `[view]`'s rows, and its surfaces' tables (`[view.chat]`) apart — a surface's name isn't a key.
    fn view_tables(
        table: &toml::Table,
        path: &str,
        warnings: &mut Vec<String>,
    ) -> (Node, Vec<(&'static str, Node)>) {
        let Some(toml::Value::Table(t)) = table.get("view") else {
            return (Node::default(), Vec::new());
        };
        let mut rows = t.clone();
        let mut on = Vec::new();
        for &name in crate::view::TABLES {
            if let Some(toml::Value::Table(sub)) = rows.remove(name) {
                on.push((name, Node::from_toml(&sub, &format!("{path}.view.{name}"), warnings)));
            }
        }
        (Node::from_toml(&rows, &format!("{path}.view"), warnings), on)
    }

    /// View mode's tables from `normal`/`select` and the view rows — again after the user's keys merge in.
    fn build_views(&mut self) {
        let keep = |name: &str| crate::view::COMMANDS.contains(&name);
        self.views = crate::view::TABLES
            .iter()
            .map(|&table| {
                let mut on = self.view.clone();
                if let Some((_, n)) = self.view_on.iter().find(|(name, _)| *name == table) {
                    on.merge(n.clone());
                }
                let (mut normal, mut select) = match table {
                    // (the chat's conversation is normal mode on a read-only document: the whole of it, `space` the
                    // editor's menu — but the chat's chords stay the chat's where normal mode binds them to more
                    // than a move: `C-s` the screen, `C-o` pointing, `C-c`)
                    "chat" => {
                        let chords = |n: &Node| -> Node {
                            let mut n = n.clone();
                            n.map.retain(|k, t| {
                                !crate::chatdoc::chord(*k)
                                    || matches!(t, KeyTrie::Leaf(c) if c.iter().all(|c| matches!(c, MappableCommand::Static(s) if keep(s.name))))
                            });
                            n
                        };
                        (chords(&self.normal), chords(&self.select))
                    }
                    // (the terminal's: the space menu is the editor's — there `space` is view mode's own,
                    // `[view.terminal.space]`: what it has of the editor's)
                    _ => {
                        let space = Key::plain(crate::key::Code::Char(' '));
                        let mut normal = self.normal.filtered(&keep);
                        normal.map.remove(&space);
                        let mut select = self.select.filtered(&keep);
                        select.map.remove(&space);
                        (normal, select)
                    }
                };
                normal.merge(on.clone());
                select.merge(on);
                (table, normal, select)
            })
            .collect();
        // (an output is normal mode on its document: the whole of normal and select, the panel's rows on top)
        let (mut normal, mut select) = (self.normal.clone(), self.select.clone());
        normal.merge(self.run.clone());
        select.merge(self.run.clone());
        self.runs = (normal, select);
    }

    /// Merges the user's `[keys]` table. As in helix, `keys.normal` does not spill into select. View mode is
    /// normal mode on another document: `keys.normal` spills into it (the terminal's kept to `view::COMMANDS`), `keys.view`
    /// and `keys.view.<surface>` over it (decided 2026-10-20). `keys.terminal` merges over the keys taken from the
    /// program like the rest; a key bound to `to_program` there goes back to it.
    pub fn merge_user(&mut self, keys: &toml::Table, warnings: &mut Vec<String>) {
        for (mode, node) in [
            ("normal", &mut self.normal),
            ("select", &mut self.select),
            ("insert", &mut self.insert),
            ("chat", &mut self.chat),
            ("terminal", &mut self.terminal),
            ("run", &mut self.run),
        ] {
            if let Some(toml::Value::Table(t)) = keys.get(mode) {
                node.merge(Node::from_toml(t, &format!("keys.{mode}"), warnings));
            }
        }
        // (a key taken from the program is one key: a sequence's first key would go to the program and the rest
        // never fire — said, left out)
        self.terminal.map.retain(|k, t| {
            let one = matches!(t, KeyTrie::Leaf(_));
            if !one {
                warnings.push(format!(
                    "keys.terminal.{k}: a key taken from the program is one key, not a sequence — left out"
                ));
            }
            one
        });
        let (view, on) = Self::view_tables(keys, "keys", warnings);
        self.view.merge(view);
        for (name, node) in on {
            match self.view_on.iter_mut().find(|(n, _)| *n == name) {
                Some((_, mine)) => mine.merge(node),
                None => self.view_on.push((name, node)),
            }
        }
        self.build_views();
    }

    /// A view mode's table: the surface's, in normal or select (`v`).
    pub fn view(&self, surface: crate::view::Surface, mode: Mode) -> &Node {
        let (_, normal, select) =
            self.views.iter().find(|(n, ..)| *n == surface.table()).expect("every surface has its table");
        if mode == Mode::Select { select } else { normal }
    }

    /// The window table's prefix in normal mode: the key whose next key moves between panes (`C-w`, or where yours
    /// put the window keys) — what `window_keys` starts and the terminal's program gets once when it comes again.
    /// None: no such table.
    pub fn window_prefix(&self) -> Option<Key> {
        let mut keys: Vec<Key> = self
            .normal
            .map
            .iter()
            .filter(|(_, t)| matches!(t, KeyTrie::Node(n) if !n.keys_for("jump_view_left").is_empty()))
            .map(|(k, _)| *k)
            .collect();
        crate::sorting::by_key(&mut keys, |k| k.to_string());
        keys.first().copied()
    }

    /// The commands the terminal's program gives up `key` for (`[terminal]`) — None: it's the program's (not
    /// bound there, or bound to `to_program`).
    pub fn terminal_takes(&self, key: Key) -> Option<&[MappableCommand]> {
        match self.terminal.search(&[key]) {
            Some(KeyTrie::Leaf(cmds)) if !is_to_program(cmds) => Some(cmds),
            _ => None,
        }
    }

    /// An output's keys (`rundoc.rs`): normal mode's, or select's (its `v`), with `[run]` on top.
    fn run_table(&self, mode: Mode) -> &Node {
        if mode == Mode::Select { &self.runs.1 } else { &self.runs.0 }
    }

    /// `lookup` in an output's table.
    pub fn run_lookup(&self, mode: Mode, keys: &[Key]) -> Lookup<'_> {
        match self.run_table(mode).search(keys) {
            Some(KeyTrie::Leaf(cmds)) => Lookup::Matched(cmds),
            Some(KeyTrie::Node(_)) => Lookup::Pending,
            None => Lookup::NotFound,
        }
    }

    /// `children` in an output's table (which-key).
    pub fn run_children(&self, mode: Mode, keys: &[Key]) -> Vec<(Key, String, bool)> {
        Self::children_of(self.run_table(mode), keys)
    }

    /// The keys bound to `name` on an output, as a hint names them.
    pub fn run_keys_for(&self, name: &str) -> Vec<String> {
        self.runs.0.keys_for(name)
    }

    /// `name` is a command of the window table (`C-w`'s, wherever yours put it) — the editor's from any pane.
    pub fn in_window_table(&self, name: &str) -> bool {
        let Some(prefix) = self.window_prefix() else { return false };
        matches!(self.normal.map.get(&prefix), Some(KeyTrie::Node(n)) if !n.keys_for(name).is_empty())
    }

    /// `lookup` in a view mode's table.
    pub fn view_lookup(&self, surface: crate::view::Surface, mode: Mode, keys: &[Key]) -> Lookup<'_> {
        match self.view(surface, mode).search(keys) {
            Some(KeyTrie::Leaf(cmds)) => Lookup::Matched(cmds),
            Some(KeyTrie::Node(_)) => Lookup::Pending,
            None => Lookup::NotFound,
        }
    }

    fn root(&self, mode: Mode) -> &Node {
        match mode {
            Mode::Normal => &self.normal,
            Mode::Select => &self.select,
            Mode::Insert => &self.insert,
        }
    }

    /// Keys that can be pressed next in a submode (`space`, `g` …) — for the which-key card.
    /// (key, description, is submode). Description = command doc (the line for `:cmd`), lowercase first.
    pub fn children(&self, mode: Mode, keys: &[Key]) -> Vec<(Key, String, bool)> {
        Self::children_of(self.root(mode), keys)
    }

    /// `children` in a view mode's table.
    pub fn view_children(
        &self,
        surface: crate::view::Surface,
        mode: Mode,
        keys: &[Key],
    ) -> Vec<(Key, String, bool)> {
        Self::children_of(self.view(surface, mode), keys)
    }

    fn children_of(root: &Node, keys: &[Key]) -> Vec<(Key, String, bool)> {
        let Some(KeyTrie::Node(node)) = root.search(keys) else { return Vec::new() };
        node.sorted()
            .into_iter()
            .map(|(k, t)| match t {
                KeyTrie::Node(_) => {
                    let path: Vec<String> = keys.iter().chain([k]).map(Key::to_string).collect();
                    let name = group_name(&path.join(" ")).map_or("…".to_string(), |n| format!("{n} …"));
                    (*k, name, true)
                }
                KeyTrie::Leaf(cmds) => {
                    let desc = cmds.first().map(|c| c.doc().trim_end_matches(" (LSP)").to_string());
                    (*k, desc.unwrap_or_default(), false)
                }
            })
            .collect()
    }

    /// Command name → shortest key bound to it (normal mode).
    pub fn bindings(&self) -> HashMap<&'static str, String> {
        self.bindings_in(&[])
    }

    /// `bindings` as the command palette shows them — a key zellij takes (`letters` with `C-`; none outside it)
    /// gives way to its twin (`zellij.rs`), one with none stays.
    pub fn bindings_in(&self, letters: &[char]) -> HashMap<&'static str, String> {
        type Best = HashMap<&'static str, (bool, String)>;
        fn walk(node: &Node, prefix: &str, taken: bool, letters: &[char], out: &mut Best) {
            for (k, t) in &node.map {
                let seq = if prefix.is_empty() { k.shown() } else { format!("{prefix} {}", k.shown()) };
                let taken = taken || crate::zellij::takes(k, letters);
                match t {
                    KeyTrie::Node(n) => walk(n, &seq, taken, letters, out),
                    KeyTrie::Leaf(cmds) => {
                        if let [MappableCommand::Static(c)] = cmds.as_slice() {
                            let better = out
                                .get(c.name)
                                .is_none_or(|(t, old)| (taken, seq.len(), &seq) < (*t, old.len(), old));
                            if better {
                                out.insert(c.name, (taken, seq));
                            }
                        }
                    }
                }
            }
        }
        let mut out = HashMap::new();
        walk(&self.normal, "", false, letters, &mut out);
        out.into_iter().map(|(name, (_, seq))| (name, seq)).collect()
    }

    /// Command `name`'s shortest key in `mode` none of whose keys zellij takes (`letters` with `C-`) — None: it
    /// has none such.
    pub fn untaken(&self, mode: Mode, name: &str, letters: &[char]) -> Option<String> {
        self.root(mode).untaken(&[name.to_string()], letters)
    }

    /// Inside zellij, the twin of `keys` bound in `mode` when it takes one of them (`letters` with `C-`,
    /// `zellij.rs`): the shortest sequence of the mode's bound to the same, none of it zellij's.
    pub fn twin(&self, mode: Mode, keys: &[Key], letters: &[char]) -> Option<String> {
        if !keys.iter().any(|k| crate::zellij::takes(k, letters)) {
            return None;
        }
        let Some(KeyTrie::Leaf(cmds)) = self.root(mode).search(keys) else { return None };
        self.root(mode).twin(cmds, letters)
    }

    /// The bindings of the code's modes zellij takes (`letters` with `C-`) — (mode, keys, command, twin), select
    /// mode's only where they differ from normal's — for `:config-show` and the keymap reference.
    pub fn taken(&self, letters: &[char]) -> Vec<(&'static str, String, String, Option<String>)> {
        fn walk(node: &Node, prefix: &[Key], letters: &[char], out: &mut Vec<(Vec<Key>, Vec<String>)>) {
            for (k, t) in node.sorted() {
                let seq: Vec<Key> = prefix.iter().copied().chain([*k]).collect();
                match t {
                    KeyTrie::Node(n) => walk(n, &seq, letters, out),
                    KeyTrie::Leaf(c) if seq.iter().any(|k| crate::zellij::takes(k, letters)) => {
                        out.push((seq, cmd_names(c)))
                    }
                    KeyTrie::Leaf(_) => {}
                }
            }
        }
        let mut out = Vec::new();
        for (name, mode) in [("normal", Mode::Normal), ("select", Mode::Select), ("insert", Mode::Insert)] {
            let mut found = Vec::new();
            walk(self.root(mode), &[], letters, &mut found);
            for (seq, names) in found {
                let same =
                    matches!(self.lookup(Mode::Normal, &seq), Lookup::Matched(n) if cmd_names(n) == names);
                if mode == Mode::Select && same {
                    continue;
                }
                let keys = seq.iter().map(Key::shown).collect::<Vec<_>>().join(" ");
                out.push((name, keys, names.join(" + "), self.twin(mode, &seq, letters)));
            }
        }
        out
    }

    /// (a shifted page key no binding names is the page key: shift is carried only for the terminal's past —
    /// `Key.shift`)
    pub fn lookup(&self, mode: Mode, keys: &[Key]) -> Lookup<'_> {
        let node = self.root(mode);
        let found = node.search(keys).or_else(|| {
            let plain: Vec<Key> = keys.iter().map(|k| k.unshifted()).collect();
            keys.iter().any(|k| k.shift).then(|| node.search(&plain)).flatten()
        });
        match found {
            Some(KeyTrie::Leaf(cmds)) => Lookup::Matched(cmds),
            Some(KeyTrie::Node(_)) => Lookup::Pending,
            None => Lookup::NotFound,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(s: &str) -> Vec<Key> {
        s.split(' ').map(|k| k.parse().unwrap()).collect()
    }

    fn name(km: &Keymaps, mode: Mode, s: &str) -> Option<String> {
        match km.lookup(mode, &keys(s)) {
            Lookup::Matched(cmds) => Some(cmd_names(cmds).join(",")),
            _ => None,
        }
    }

    fn row(out: &mut String, keys: &str, cmds: &[MappableCommand]) {
        use crate::settings::{md_cell, md_code};
        let names: Vec<String> = cmd_names(cmds).iter().map(|n| md_code(n)).collect();
        // "Extend up, then extend selection to line bounds"
        let docs: Vec<String> = cmds
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let doc = c.doc();
                let mut chars = doc.chars();
                match (i, chars.next(), chars.next()) {
                    (1.., Some(first), Some(second)) if second.is_lowercase() => {
                        format!("{}{}", first.to_lowercase(), &doc[first.len_utf8()..])
                    }
                    _ => doc,
                }
            })
            .map(|d| md_cell(&d))
            .collect();
        out.push_str(&format!("| {} | {} | {} |\n", md_code(keys), names.join(" + "), docs.join(", then ")));
    }

    const TABLE_HEAD: &str = "| Key | Command | Description |\n|---|---|---|\n";

    /// A node's keys (submodes point below), then one table per submode, depth first.
    fn node_tables(out: &mut String, node: &Node, prefix: &mut Vec<String>, heading: &str) {
        out.push_str(&format!("\n{heading}\n\n{TABLE_HEAD}"));
        for (k, t) in node.sorted() {
            let seq = prefix.iter().cloned().chain([k.to_string()]).collect::<Vec<_>>().join(" ");
            match t {
                KeyTrie::Leaf(cmds) => row(out, &seq, cmds),
                KeyTrie::Node(_) => out.push_str(&format!(
                    "| {} | … | More keys — see {} below |\n",
                    crate::settings::md_code(&seq),
                    crate::settings::md_code(&seq)
                )),
            }
        }
        for (k, t) in node.sorted() {
            if let KeyTrie::Node(sub) = t {
                prefix.push(k.to_string());
                let title = format!("### {}", crate::settings::md_code(&prefix.join(" ")));
                node_tables(out, sub, prefix, &title);
                prefix.pop();
            }
        }
    }

    fn leaves<'a>(node: &'a Node, prefix: &[Key], out: &mut Vec<(Vec<Key>, &'a [MappableCommand])>) {
        for (k, t) in node.sorted() {
            let seq: Vec<Key> = prefix.iter().copied().chain([*k]).collect();
            match t {
                KeyTrie::Leaf(cmds) => out.push((seq, cmds)),
                KeyTrie::Node(sub) => leaves(sub, &seq, out),
            }
        }
    }

    /// The keys of cards, reviews and panels (`offer.rs`, `llm::review_key`, `codereview.rs`, the task
    /// panel) — where, which, what.
    const ON_SCREEN: &[(&str, &str, &str)] = &[
        (
            "A card",
            "`y` `n`",
            "Yes · no (some take `a` this chat, `s` this session, `A` always here); Esc never answers one",
        ),
        (
            "A card",
            "`C-q`",
            "Give it the keys from wherever they are (the status line says CARD); Esc goes back, unanswered",
        ),
        (
            "No card here",
            "`C-q`",
            "To the session whose Claude waits for you — its card comes up there; nothing waiting, to a \
             session whose turn ended (`done`)",
        ),
        ("A card cut to fit", "`v`", "All of it in a buffer — `y` waits until its end was on screen"),
        (
            "A run card listing others like it",
            "`Y`",
            "Run it and the cards it lists — the same run each, read whole first",
        ),
        (
            "The stale drafts card",
            "`y` `r` `n`",
            "Close them · list just them (`C-x` closes one) · keep them",
        ),
        (
            "Claude's change to several files",
            "`y` `r` `v` `n`",
            "Accept them all · review them one by one · every change in a buffer · decline them all",
        ),
        ("An edit's review", "`y` `n` `a` `tab`", "Accept this change · reject it · accept all left · next"),
        ("An edit's review", "Esc", "Back to the chat, the change unanswered — `C-q` answers it"),
        (
            "An edit's review",
            "`C-d` `C-u` `C-f` `C-b` `C-e` `C-y`, wheel",
            "Scroll it — `space`, `:` and `C-w` work over it too",
        ),
        ("Code review rail", "click on `◦`", "Mark the file viewed (a second click leaves it marked)"),
        ("Code review rail", "click on `↑ N more` / `↓ N more`, wheel", "Scroll the files"),
        ("Code review", "`space v q`", "End it — asked on a card while files aren't viewed"),
        ("Task panel", "click on a preflight step", "Its output; wheel, `C-u` `C-d` scroll"),
        (
            "Task panel",
            "`o`",
            "A preflight step's whole output in a buffer — the failed one's, else the one in view (`:preflight-output <step>`); beside the steps' list, the step in view selected there",
        ),
        (
            "A task's places, a preflight's steps (beside the output)",
            "`j` `k` and every list's moves · `enter` · `tab` `S-tab` · `q`",
            "Pick one — the output follows: the place's line, the step's heading at the top · the place in the code (a step's first place, else its output from its heading) · the output and back · close once the run has ended",
        ),
        ("A preflight's steps", "`o`", "The step picked, its whole output in a buffer"),
        (
            "Task panel",
            "`K`",
            "Where Claude's command in it ran, and why — its sandbox, the card it ran from, its warnings",
        ),
        (
            "The chat",
            "click on a command's row end (`ok · sandbox ▲1`)",
            "Why it ran there — its sandbox, the card it ran from, Claude's reason, its warnings",
        ),
        (
            "The chat",
            "`C-u` `C-d`, `PageUp` `PageDown`, `↑` `↓`, wheel",
            "Scroll the conversation — half the rows in sight, a page, a row; `C-u` and the arrows from an empty input (with text `C-u` clears to the cursor)",
        ),
        (
            "A list or output panel (side panel, review rail, `:procs`, test, task, terminal, a plugin's)",
            "`j` `k` `↓` `↑` `C-n` `C-p` · `C-e` `C-y` · `C-d` `C-u` · `C-f` `C-b` `PageDown` `PageUp` · `g` `G` `Home` `End`",
            "A row · the view a row, the selection staying unless it would leave it (the wheel too, three rows, while the panel has the keys) · half the rows in sight · all of them · the ends — the same keys everywhere, as the code's; a plugin's panel never gets them",
        ),
        ("Hover text, docs", "`C-d` `C-u` · `C-e` `C-y`", "Scroll it half its rows · a row"),
        (
            "The terminal, its program with the keys",
            "`S-pageup` `S-pagedown`",
            "Scroll back a page · forward (the program's next key brings it down; a full-screen program gets them)",
        ),
        (
            "A picker",
            "`C-n` `C-p` `↓` `↑` `Tab` · `C-d` `C-u` · `PageDown` `PageUp` · `Home` `End`",
            "A row · half the rows in sight · all of them · the first and last",
        ),
        (
            "A picker of files or places",
            "`C-v` `C-s` · `A-ret`",
            "Open it in a split, beside · under (Helix's keys) · open it, the list kept up",
        ),
        ("A picker", "`C-t` · `C-f` `C-b`, wheel over it", "The preview away and back · scroll it"),
    ];

    /// The twins of `ON_SCREEN`'s keys zellij takes (`zellij::twin`, `key::list_move`'s other keys).
    const ZELLIJ_ON_SCREEN: &[(&str, &str, &str)] = &[
        (
            "A card waiting",
            "`C-q`",
            "The card in front: the way back to the code, where it takes the keys — `esc` from insert mode, a \
             prompt, a picker, a panel, a view mode or a chat with nothing said yet · `esc esc` from the chat's input \
             · `C-\\ C-\\` from the terminal's program. A card in a session behind, an edit, the edit review: the way \
             back, then `space I` (the inbox, `enter` goes to it). A turn that ended: the way back, then `space W`",
        ),
        (
            "The chat's input",
            "`C-t` · `C-p` · `C-s` · `C-o`",
            "The way back to the code (`esc esc`, `esc` with nothing said yet), then `:model` · `:chat-resume` · \
             `:chat-screen` · `:chat-point` — in the input `:` is a character",
        ),
        ("A list or output panel", "`C-n` `C-p` · `C-b`", "`j` `k` · `PageUp`"),
        (
            "A picker",
            "`C-n` `C-p` · `C-b` · `C-t`",
            "`↓` `↑` · the wheel over the preview · none (the preview away)",
        ),
        (
            "A picker of files or places",
            "`C-s` · `C-q`",
            "none (`C-v` opens it beside) · none (the places kept as the list)",
        ),
    ];

    /// `docs/reference/keymap.md` — the default keymap per mode and submode (descriptions = command docs,
    /// the same text which-key and the palette show), then every `:` command from `cmdline::COMMANDS`.
    fn reference() -> String {
        use crate::settings::{md_cell, md_code};
        let km = Keymaps::default();
        let mut out = String::from(
            "<!-- Generated from src/default_keys.toml and the command registry — do not edit; regenerate \
             with `TARAE_BLESS=1 cargo test keymap_reference`. -->\n\
             # Default keymap\n\n\
             The keys tarae ships with, from [`src/default_keys.toml`](../../src/default_keys.toml). Command names \
             are the same as Helix's,\nso Helix key configs carry over. Rebind anything in `[keys.normal]`, \
             `[keys.select]`, or `[keys.insert]` —\nsee the [configuration guide](../configuration.md#keys).\n\n\
             In the editor you don't need this page: `space ?` finds any command by what it does, and after a \
             prefix key\n(`space`, `g`, `m`, `[`, `]`) a card shows what comes next.\n",
        );
        node_tables(&mut out, &km.normal, &mut Vec::new(), "## Normal mode");

        out.push_str(
            "\n## Select mode\n\n`v` enters select mode. Every normal-mode key works here too, except these — \
             movement extends the selection instead:\n\n",
        );
        out.push_str(TABLE_HEAD);
        let mut select = Vec::new();
        leaves(&km.select, &[], &mut select);
        for (seq, cmds) in select {
            let same = matches!(km.lookup(Mode::Normal, &seq), Lookup::Matched(n) if cmd_names(n) == cmd_names(cmds));
            if !same {
                let seq = seq.iter().map(Key::to_string).collect::<Vec<_>>().join(" ");
                row(&mut out, &seq, cmds);
            }
        }

        node_tables(&mut out, &km.insert, &mut Vec::new(), "## Insert mode");

        out.push_str(
            "\n## The run panels' output\n\nA task's output, a preflight's, the tests' failure or output (`C-w j` \
             to the panel, a click) is a read-only document with normal mode's keys — yours too: `[keys.normal]` \
             holds there. What would edit it is put back and said; `C-w` and `:` are the editor's. The panel's own \
             keys go over normal mode's:\n",
        );
        node_tables(&mut out, &km.run, &mut Vec::new(), "### On the output (`[keys.run]`)");

        out.push_str(
            "\n## View mode\n\nA cursor on what a surface shows. The chat's conversation (`esc` from its input) is \
             normal mode on a read-only document — all of it, yours too (`[keys.normal]` spills in), `space` the \
             editor's menu; an edit is put back and said. The terminal's scrollback and screen (`C-\\` from its \
             program, `C-w j` to its panel): normal mode's moves, selections, search and counts (yours too), the \
             rest said, not done; `space` its own menu, what it has of the editor's. `enter` quotes what's selected \
             to the chat. Its own keys go over them, and each surface's over those.\n",
        );
        node_tables(&mut out, &km.view, &mut Vec::new(), "### Its own keys (`[keys.view]`)");
        for (name, node) in &km.view_on {
            node_tables(&mut out, node, &mut Vec::new(), &format!("### The {name}'s (`[keys.view.{name}]`)"));
        }
        node_tables(&mut out, &km.chat, &mut Vec::new(), "### The chat's input (`[keys.chat]`)");
        node_tables(
            &mut out,
            &km.terminal,
            &mut Vec::new(),
            "### Taken from the terminal's program (`[keys.terminal]` — every other key is the program's; \
             `to_program` gives one of these back)",
        );

        out.push_str(
            "\n## Cards, reviews and panels\n\nKeys of what's up on the screen — not rebindable, so not in \
             `default_keys.toml`:\n\n| Where | Key | What it does |\n|---|---|---|\n",
        );
        for (at, key, what) in ON_SCREEN {
            out.push_str(&format!("| {at} | {key} | {} |\n", md_cell(what)));
        }

        let defaults = crate::zellij::DEFAULT_KEYS;
        out.push_str(&format!(
            "\n## Inside zellij\n\nzellij's default normal mode takes {} before tarae sees them — `editor.zellij-keys` \
             (yours only) names the letters zellij takes, these by default; a config of your own (session on `C-y`) \
             names its own, and `editor.zellij-hints = off` says no twins at all (the unlock-first preset). Inside \
             zellij (`ZELLIJ` set) the hints — which-key, the palette, the panels', the tutor — say the same thing \
             without them, the twins below (for the default keys); the bindings stay. A key with no twin works once \
             `C-g` has locked zellij (`C-g` again unlocks it).\n\n\
             | Where | Key | Inside zellij |\n|---|---|---|\n",
            crate::zellij::said(&defaults, true)
        ));
        for (mode, keys, cmds, twin) in km.taken(&defaults) {
            let twin = twin.map_or("none".to_string(), |t| md_code(&t));
            out.push_str(&format!("| {mode} mode | {} ({}) | {twin} |\n", md_code(&keys), md_code(&cmds)));
        }
        for (at, key, twin) in ZELLIJ_ON_SCREEN {
            out.push_str(&format!("| {at} | {key} | {} |\n", md_cell(twin)));
        }

        out.push_str(
            "\n## `:` commands\n\nType `:` in normal mode. `Tab` completes command names and their arguments \
             (paths, themes, settings, languages).\n\n| Command | Aliases | Description |\n|---|---|---|\n",
        );
        for c in crate::cmdline::COMMANDS {
            let aliases: Vec<String> = c.names[1..].iter().map(|a| md_code(&format!(":{a}"))).collect();
            out.push_str(&format!(
                "| {} | {} | {} |\n",
                md_code(&format!(":{}", c.names[0])),
                aliases.join(" "),
                md_cell(c.doc)
            ));
        }
        out
    }

    #[test]
    fn keymap_reference_is_up_to_date() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/reference/keymap.md");
        crate::settings::check_generated(path, &reference(), "keymap_reference");
    }

    #[test]
    fn defaults_load() {
        let km = Keymaps::default();
        assert_eq!(name(&km, Mode::Normal, "w").as_deref(), Some("move_next_word_start"));
        assert_eq!(name(&km, Mode::Select, "w").as_deref(), Some("extend_next_word_start"));
        assert_eq!(name(&km, Mode::Normal, "g g").as_deref(), Some("goto_file_start"));
        assert!(matches!(km.lookup(Mode::Normal, &keys("g")), Lookup::Pending));
        // select inherits the rest of normal
        assert_eq!(name(&km, Mode::Select, "d").as_deref(), Some("delete_selection"));
    }

    /// A shifted page key no binding names is the page key in the code — its shift is the terminal's past's only
    /// (`Key.shift`, uxaudit Z2); one bound by name is its own.
    #[test]
    fn a_shifted_page_key_is_the_page_key_unless_bound() {
        let km = Keymaps::default();
        assert_eq!(name(&km, Mode::Normal, "S-pageup").as_deref(), Some("page_up"));
        assert_eq!(name(&km, Mode::Insert, "S-pagedown").as_deref(), Some("page_down"));
        let mut km = km;
        let user: toml::Table = toml::from_str("[normal]\nS-pageup = \"goto_file_start\"").unwrap();
        km.merge_user(&user, &mut Vec::new());
        assert_eq!(name(&km, Mode::Normal, "S-pageup").as_deref(), Some("goto_file_start"));
        assert_eq!(name(&km, Mode::Normal, "pageup").as_deref(), Some("page_up"));
    }

    #[test]
    fn user_config_merges_like_helix() {
        // Part of the user's actual helix config
        let user: toml::Table = toml::from_str(
            r#"
            [normal]
            "A-/" = "repeat_last_motion"
            "X" = ["extend_line_up", "extend_to_line_bounds"]
            C-l = ":sh zellij run -- lazygit"
            g = { a = "goto_last_line" }
            "A-," = "no_such_command"
            "#,
        )
        .unwrap();
        let mut km = Keymaps::default();
        let mut warnings = Vec::new();
        km.merge_user(&user, &mut warnings);
        assert_eq!(name(&km, Mode::Normal, "A-/").as_deref(), Some("repeat_last_motion"));
        assert_eq!(name(&km, Mode::Normal, "X").as_deref(), Some("extend_line_up,extend_to_line_bounds"));
        assert_eq!(name(&km, Mode::Normal, "C-l").as_deref(), Some(":sh zellij run -- lazygit"));
        // Submodes merge — keeps the existing gg + adds ga
        assert_eq!(name(&km, Mode::Normal, "g a").as_deref(), Some("goto_last_line"));
        assert_eq!(name(&km, Mode::Normal, "g g").as_deref(), Some("goto_file_start"));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("no_such_command"));
    }

    /// Every line which-key shows for the default keys fits its column whole: ≤ 30 characters (CLAUDE.md's
    /// rule; uxaudit D11 — the `C-w` menu's were cut, "Taller: the panel at the bott…").
    #[test]
    fn every_which_key_line_fits_whole() {
        fn walk(km: &Keymaps, mode: Mode, node: &Node, path: &mut Vec<Key>, long: &mut Vec<String>) {
            for (k, t) in node.sorted() {
                if let KeyTrie::Node(n) = t {
                    path.push(*k);
                    for (key, desc, _) in km.children(mode, path) {
                        if desc.chars().count() > 30 {
                            let at: Vec<String> = path.iter().chain([&key]).map(Key::shown).collect();
                            long.push(format!("{mode:?} {}: {desc}", at.join(" ")));
                        }
                    }
                    walk(km, mode, n, path, long);
                    path.pop();
                }
            }
        }
        let km = Keymaps::default();
        let mut long = Vec::new();
        // (every mode's menus: a prefix bound only in select or insert has its card too)
        for (mode, root) in
            [(Mode::Normal, &km.normal), (Mode::Select, &km.select), (Mode::Insert, &km.insert)]
        {
            walk(&km, mode, root, &mut Vec::new(), &mut long);
        }
        assert!(long.is_empty(), "{long:#?}");
    }

    /// Keys are shown as they're typed: `C-w -`, not the config's `minus` (uxaudit D11).
    #[test]
    fn keys_are_shown_as_typed() {
        let km = Keymaps::default();
        assert_eq!(km.bindings()["bottom_shorter"], "C-w -");
        // (the config's name stays the config's: `minus` round-trips, `-` is what's shown — the drawn card is
        // term's `which_key_draws_keys_as_typed`)
        let minus = "C-minus".parse::<Key>().unwrap();
        assert_eq!((minus.to_string(), minus.shown()), ("C-minus".to_string(), "C--".to_string()));
        assert_eq!("A-minus".parse::<Key>().unwrap().shown(), "A--");
    }
}
