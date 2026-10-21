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
    /// Per surface: normal mode kept to `view::COMMANDS`, then `[view]`, then the surface's — and the same
    /// over `select` (view mode's `v`).
    views: Vec<(&'static str, Node, Node)>,
    /// The chat's input (`[chat]`): what its keys run before it types them — `esc` → its view mode.
    pub chat: Node,
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
        for s in crate::view::SURFACES {
            if let Some(toml::Value::Table(sub)) = rows.remove(s.name()) {
                on.push((s.name(), Node::from_toml(&sub, &format!("{path}.view.{}", s.name()), warnings)));
            }
        }
        (Node::from_toml(&rows, &format!("{path}.view"), warnings), on)
    }

    /// View mode's tables from `normal`/`select` and the view rows — again after the user's keys merge in.
    fn build_views(&mut self) {
        let keep = |name: &str| crate::view::COMMANDS.contains(&name);
        self.views = crate::view::SURFACES
            .iter()
            .map(|s| {
                let mut on = self.view.clone();
                if let Some((_, n)) = self.view_on.iter().find(|(name, _)| *name == s.name()) {
                    on.merge(n.clone());
                }
                // (the space menu is the editor's — in view mode its `space` is said, not a prefix)
                let space = Key::plain(crate::key::Code::Char(' '));
                let mut normal = self.normal.filtered(&keep);
                normal.map.remove(&space);
                normal.merge(on.clone());
                let mut select = self.select.filtered(&keep);
                select.map.remove(&space);
                select.merge(on);
                (s.name(), normal, select)
            })
            .collect();
    }

    /// Merges the user's `[keys]` table. As in helix, `keys.normal` does not spill into select. View mode is
    /// normal mode on another document: `keys.normal` spills into it (kept to `view::COMMANDS`), `keys.view`
    /// and `keys.view.<surface>` over it (decided 2026-10-20).
    pub fn merge_user(&mut self, keys: &toml::Table, warnings: &mut Vec<String>) {
        for (mode, node) in [
            ("normal", &mut self.normal),
            ("select", &mut self.select),
            ("insert", &mut self.insert),
            ("chat", &mut self.chat),
        ] {
            if let Some(toml::Value::Table(t)) = keys.get(mode) {
                node.merge(Node::from_toml(t, &format!("keys.{mode}"), warnings));
            }
        }
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
            self.views.iter().find(|(n, ..)| *n == surface.name()).expect("every surface has its table");
        if mode == Mode::Select { select } else { normal }
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

    /// Command name → shortest key bound to it (normal mode) — shown in the command palette.
    pub fn bindings(&self) -> HashMap<&'static str, String> {
        fn walk(node: &Node, prefix: &str, out: &mut HashMap<&'static str, String>) {
            for (k, t) in &node.map {
                let seq = if prefix.is_empty() { k.to_string() } else { format!("{prefix} {k}") };
                match t {
                    KeyTrie::Node(n) => walk(n, &seq, out),
                    KeyTrie::Leaf(cmds) => {
                        if let [MappableCommand::Static(c)] = cmds.as_slice() {
                            let better =
                                out.get(c.name).is_none_or(|old| (seq.len(), &seq) < (old.len(), old));
                            if better {
                                out.insert(c.name, seq);
                            }
                        }
                    }
                }
            }
        }
        let mut out = HashMap::new();
        walk(&self.normal, "", &mut out);
        out
    }

    pub fn lookup(&self, mode: Mode, keys: &[Key]) -> Lookup<'_> {
        let node = self.root(mode);
        match node.search(keys) {
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

    /// Command names of a binding (`:line` for typed commands) — several run in sequence.
    fn cmd_names(cmds: &[MappableCommand]) -> Vec<String> {
        cmds.iter()
            .map(|c| match c {
                MappableCommand::Static(c) => c.name.to_string(),
                MappableCommand::Typed(t) => format!(":{t}"),
            })
            .collect()
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
            "A preflight step's whole output in a buffer — the failed one's, else the one in view (`:preflight-output <step>`)",
        ),
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
            "Scroll the conversation — `C-u` and the arrows from an empty input (with text `C-u` clears to the cursor)",
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
            "\n## View mode\n\nA cursor on what a surface shows — the chat's conversation (`esc` from its input). Normal \
             mode's moves, selections, search and counts work there (yours too: `[keys.normal]` spills in), the \
             rest is said, not done. Its own keys go over them.\n",
        );
        node_tables(
            &mut out,
            &km.view,
            &mut Vec::new(),
            "### Its own keys (`[keys.view]`, `[keys.view.chat]`)",
        );
        node_tables(&mut out, &km.chat, &mut Vec::new(), "### The chat's input (`[keys.chat]`)");

        out.push_str(
            "\n## Cards, reviews and panels\n\nKeys of what's up on the screen — not rebindable, so not in \
             `default_keys.toml`:\n\n| Where | Key | What it does |\n|---|---|---|\n",
        );
        for (at, key, what) in ON_SCREEN {
            out.push_str(&format!("| {at} | {key} | {} |\n", md_cell(what)));
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
}
