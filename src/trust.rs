//! Project trust. A project's `.tarae.toml` is the project's file — often a cloned repository's, written by
//! someone else. What in it runs commands or loosens how Claude's commands are guarded (`RISKY` settings,
//! `[task.<name>]`, a `[lsp.<name>]` command or args, an `[[attach]]` `before`, a key bound to a `:` command)
//! is held back until the user trusts the project on a card showing each of them as written; the rest of the
//! file applies as is. The answer is kept per project file (`$state/trust.json`) with a hash of what it was
//! about, so a change asks again. `:project-trust` shows the card again, `:project-untrust` takes it back.
//!
//! `config::load` applies the project layer both ways (`Project.with` / `without`), so answering switches
//! between them without reading anything.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value as Json, json};

use crate::config::EditorConfig;
use crate::keymap::Keymaps;

/// Settings a project file sets only once trusted: they run commands, or change what Claude's commands do
/// unasked (`llm.sandbox*` comes with the sandbox).
pub const RISKY: [&str; 11] = [
    "editor.restart-build",
    "editor.restart-binary",
    "llm.command",
    "llm.args",
    "llm.run-allow",
    "llm.review-ask",
    "llm.chat-settings",
    "llm.sandbox",
    "llm.sandbox-network",
    "llm.sandbox-write",
    "llm.sandbox-protect",
];

/// A value longer than this (as written) isn't put on the card whole — so it's never trusted.
pub const SHOWN_MAX: usize = 600;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// Not answered for what the file says now — held back, and the card asks.
    Ask,
    Trusted,
    Ignored,
}

/// The project file's held-back part, and the config both ways.
#[derive(Clone, Debug)]
pub struct Project {
    pub file: PathBuf,
    pub hash: u64,
    /// (key, value as written in TOML — marks that draw as something else spelled out)
    pub items: Vec<(String, String)>,
    pub state: State,
    pub with: Box<(EditorConfig, Keymaps)>,
    pub without: Box<(EditorConfig, Keymaps)>,
}

/// While a project layer is applied: each risky thing is noted, and goes in only when trusted.
pub struct Gate {
    pub trusted: bool,
    pub held: Vec<(String, String)>,
}

impl Gate {
    pub fn new(trusted: bool) -> Gate {
        Gate { trusted, held: Vec::new() }
    }

    /// Whether `key` (set to `value`, as TOML) goes in — noted either way.
    pub fn pass(&mut self, key: String, value: &toml::Value) -> bool {
        let shown = visible(&value.to_string());
        let ok = self.trusted && shown.chars().count() <= SHOWN_MAX;
        self.held.push((key, shown));
        ok
    }
}

pub fn is_risky(setting: &str) -> bool {
    RISKY.contains(&setting)
}

/// Whether a key binding's value runs a `:` command (`":sh …"`, or one in a list).
pub fn typed_binding(v: &toml::Value) -> bool {
    match v {
        toml::Value::String(s) => s.trim_start().starts_with(':'),
        toml::Value::Array(a) => a.iter().any(typed_binding),
        _ => false,
    }
}

/// The text with what draws as something else — control characters, bidi and zero-width marks — spelled
/// out as `\u{…}`, so the card shows what's there.
pub fn visible(s: &str) -> String {
    let hidden = |c: char| {
        (c.is_control() && c != '\n' && c != '\t')
            || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
    };
    s.chars().map(|c| if hidden(c) { format!("\\u{{{:x}}}", c as u32) } else { c.to_string() }).collect()
}

/// What the answer is about: the held-back items, in order (FNV-1a).
pub fn hash(items: &[(String, String)]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for (k, v) in items {
        for b in k.bytes().chain([0]).chain(v.bytes()).chain([0]) {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0100_0000_01b3);
        }
    }
    h
}

/// The answers given, per project file (real path) — `$state/trust.json`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Store {
    answers: BTreeMap<PathBuf, (u64, bool)>,
}

impl Store {
    pub fn path() -> Option<PathBuf> {
        crate::config::state_dir().map(|d| d.join("trust.json"))
    }

    /// Read from `path` (missing or unreadable = nothing answered).
    pub fn read(path: Option<&Path>) -> Store {
        let Some(text) = path.and_then(|p| std::fs::read_to_string(p).ok()) else { return Store::default() };
        let Ok(json) = serde_json::from_str::<Json>(&text) else { return Store::default() };
        let mut answers = BTreeMap::new();
        for (file, a) in json["projects"].as_object().into_iter().flatten() {
            let hash = a["hash"].as_str().and_then(|h| u64::from_str_radix(h, 16).ok());
            if let (Some(hash), Some(trusted)) = (hash, a["trusted"].as_bool()) {
                answers.insert(PathBuf::from(file), (hash, trusted));
            }
        }
        Store { answers }
    }

    /// The answer for `file` as it is now (`hash`) — Ask when there's none, or it was about something else.
    pub fn state(&self, file: &Path, hash: u64) -> State {
        match self.answers.get(file) {
            Some(&(h, true)) if h == hash => State::Trusted,
            Some(&(h, false)) if h == hash => State::Ignored,
            _ => State::Ask,
        }
    }

    pub fn answer(&mut self, file: &Path, hash: u64, trusted: bool) {
        self.answers.insert(file.to_path_buf(), (hash, trusted));
    }

    pub fn to_json(&self) -> String {
        let projects: serde_json::Map<String, Json> = self
            .answers
            .iter()
            .map(|(f, (h, t))| {
                (f.to_string_lossy().into_owned(), json!({ "hash": format!("{h:016x}"), "trusted": t }))
            })
            .collect();
        serde_json::to_string_pretty(&json!({ "projects": projects })).unwrap_or_default()
    }
}

/// Keeps an answer (on a worker — read, change, write; the file is tiny).
pub fn record(path: PathBuf, file: PathBuf, hash: u64, trusted: bool) -> Result<(), String> {
    let mut store = Store::read(Some(&path));
    store.answer(&file, hash, trusted);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let body = store.to_json();
    crate::disk::write_atomic(&path, |w| std::io::Write::write_all(w, body.as_bytes()))
        .map_err(|e| e.to_string())
}

/// The card's text: the file, then each held-back item as written.
pub fn question(p: &Project) -> String {
    let mut q = format!("{} sets what runs commands as you:\n", crate::chat::shown(&p.file));
    for (k, v) in &p.items {
        if v.chars().count() > SHOWN_MAX {
            q.push_str(&format!(
                "\n  {k} = ({} characters — too long to show, so never used)",
                v.chars().count()
            ));
        } else {
            q.push_str(&format!("\n  {k} = {v}"));
        }
    }
    q.push_str("\n\nIgnored, the rest of the file still applies.");
    q
}

pub const TITLE: &str = "Trust this project's settings?";

impl crate::editor::Editor {
    /// The project layer as config read it: its card if it asks, and the config the answer chose.
    pub fn project_trust_set(&mut self, p: Option<Project>) {
        let asks = p.as_ref().is_some_and(|p| p.state == State::Ask);
        self.offers.retain(|o| !matches!(o.what, crate::offer::What::ProjectTrust { .. }) || asks);
        if let Some(p) = p.as_ref().filter(|_| asks) {
            self.project_trust_offer(p);
        }
        self.project_trust = p;
    }

    fn project_trust_offer(&mut self, p: &Project) {
        let what = crate::offer::What::ProjectTrust { hash: p.hash };
        let o = crate::offer::Offer::new(TITLE.into(), &question(p), "", "Project settings".into(), what);
        self.push_offer(o);
    }

    /// The answer on the card (`hash` = what it showed): the config switches, and the answer is kept.
    pub fn project_trust_answer(&mut self, id: u64, hash: u64, trusted: bool) {
        self.offer_finished(id, Ok(()));
        let Some(p) = self.project_trust.as_mut().filter(|p| p.hash == hash) else { return };
        p.state = if trusted { State::Trusted } else { State::Ignored };
        let (file, n) = (p.file.clone(), p.items.len());
        self.project_trust_apply();
        if let Some(path) = self.trust_path.clone() {
            self.events.jobs().spawn(move || {
                let r = record(path, file, hash, trusted);
                move |ed: &mut crate::editor::Editor| {
                    if let Err(e) = r {
                        ed.set_error(format!("couldn't keep the answer: {e}"));
                    }
                }
            });
        }
        match trusted {
            true => self.set_success(format!("Trusted — the project's {n} held-back settings apply")),
            false => self.set_status("Ignoring them — :project-trust to look again"),
        }
    }

    /// The config as the project's state says (the user's `:set`s on top).
    fn project_trust_apply(&mut self) {
        let Some(p) = &self.project_trust else { return };
        let (editor, keymaps) = if p.state == State::Trusted { &*p.with } else { &*p.without };
        self.config = editor.clone();
        self.keymaps = keymaps.clone();
        for (path, v) in &self.overrides {
            let _ = crate::settings::apply(&mut self.config, path, v);
        }
    }

    /// `:project-trust` — the card again, for what the file says now.
    pub fn project_trust_ask(&mut self) -> Result<(), String> {
        let p = self.project_trust.clone().ok_or("nothing in this project's .tarae.toml needs trust")?;
        self.project_trust_offer(&p);
        Ok(())
    }

    /// `:project-untrust` — held back again (and kept so).
    pub fn project_untrust(&mut self) -> Result<(), String> {
        let p = self.project_trust.as_ref().ok_or("nothing in this project's .tarae.toml needs trust")?;
        if p.state != State::Trusted {
            return Err("this project isn't trusted".into());
        }
        let (hash, file) = (p.hash, p.file.clone());
        self.offers.retain(|o| !matches!(o.what, crate::offer::What::ProjectTrust { .. }));
        if let Some(p) = self.project_trust.as_mut() {
            p.state = State::Ignored;
        }
        self.project_trust_apply();
        if let Some(path) = self.trust_path.clone() {
            self.events.jobs().spawn(move || {
                let r = record(path, file, hash, false);
                move |ed: &mut crate::editor::Editor| {
                    if let Err(e) = r {
                        ed.set_error(format!("couldn't keep the answer: {e}"));
                    }
                }
            });
        }
        self.set_status("No longer trusted — its commands are held back");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn answers_are_kept_per_file_and_hash() {
        let dir = std::env::temp_dir().join(format!("tarae-trust-{}", std::process::id()));
        let path = dir.join("trust.json");
        let file = PathBuf::from("/p/.tarae.toml");
        record(path.clone(), file.clone(), 0xabc, true).unwrap();
        let s = Store::read(Some(&path));
        assert_eq!(s.state(&file, 0xabc), State::Trusted);
        assert_eq!(s.state(&file, 0xabd), State::Ask, "what it was about changed: asks again");
        assert_eq!(s.state(Path::new("/q/.tarae.toml"), 0xabc), State::Ask, "another project");
        record(path.clone(), file.clone(), 0xabc, false).unwrap();
        assert_eq!(Store::read(Some(&path)).state(&file, 0xabc), State::Ignored);
        assert_eq!(Store::read(Some(&dir.join("none.json"))), Store::default());
    }

    #[test]
    fn the_card_switches_the_config_and_untrust_takes_it_back() {
        use crate::config::tests::{RISKY_PROJECT, project_file};
        use crate::offer::What;
        let path = project_file("card", RISKY_PROJECT);
        let (c, _) = crate::config::load_from(None, Some(path), &Store::default());
        let mut ed = crate::editor::Editor::new(c);
        assert!(matches!(ed.offers.first().map(|o| &o.what), Some(What::ProjectTrust { .. })));
        assert!(
            ed.offers[0].question.contains("\n  editor.restart-build = \"touch marker\""),
            "{}",
            ed.offers[0].question
        );
        assert!(ed.offers[0].weighty(), "y needs the card settled in sight");
        assert_eq!(ed.config.restart_build, "");
        ed.offer_accept();
        assert!(ed.offers.is_empty());
        assert_eq!(ed.config.restart_build, "touch marker");
        assert!(ed.config.tasks.iter().any(|t| t.name == "build"));
        // The user's `:set` stays on top whichever way
        crate::typed::execute(&mut ed, "set editor.restart-binary mine").unwrap();
        ed.project_untrust().unwrap();
        assert_eq!(ed.config.restart_build, "");
        assert_eq!(ed.config.restart_binary, "mine");
        assert!(ed.config.tasks.iter().all(|t| t.name != "build"));
        assert!(ed.project_untrust().is_err(), "not trusted any more");
        // Looked at again and ignored: still held back
        ed.project_trust_ask().unwrap();
        ed.offer_dismiss();
        assert!(ed.offers.is_empty());
        assert_eq!(ed.config.restart_build, "");
        assert_eq!(ed.project_trust.as_ref().unwrap().state, State::Ignored);
    }

    #[test]
    fn too_long_to_show_is_never_trusted() {
        use crate::config::tests::project_file;
        let long = "x".repeat(SHOWN_MAX + 1);
        let path = project_file("long", &format!("[editor]\nrestart-build = \"{long}\"\n"));
        let (c, _) = crate::config::load_from(None, Some(path.clone()), &Store::default());
        let p = c.project.unwrap();
        assert!(question(&p).contains("too long to show"));
        let mut store = Store::default();
        store.answer(&path, p.hash, true);
        let (t, _) = crate::config::load_from(None, Some(path), &store);
        assert_eq!(t.editor.restart_build, "");
    }

    #[test]
    fn hidden_marks_are_spelled_out() {
        assert_eq!(visible("a\u{202E}b\u{1b}c"), "a\\u{202e}b\\u{1b}c");
        assert_eq!(visible("cargo build"), "cargo build");
    }

    #[test]
    fn typed_bindings_are_the_risky_ones() {
        let v = |s: &str| toml::from_str::<toml::Table>(&format!("v = {s}")).unwrap()["v"].clone();
        assert!(typed_binding(&v("\":sh echo hi\"")));
        assert!(typed_binding(&v("[\"move_line_down\", \" :w\"]")));
        assert!(!typed_binding(&v("\"move_line_down\"")));
    }

    #[test]
    fn the_hash_follows_keys_and_values() {
        let a = vec![("task.x.cmd".to_string(), "\"make\"".to_string())];
        let b = vec![("task.x.cmd".to_string(), "\"make all\"".to_string())];
        assert_ne!(hash(&a), hash(&b));
        assert_eq!(hash(&a), hash(&a.clone()));
    }
}
