//! Config file — TOML + tarae's own schema (one table in `settings.rs`).
//! Layers: defaults < user `~/.config/tarae/config.toml` < project `.tarae.toml` < runtime `:set`.

use std::fs;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::thread;

use toml::Value;
use toml_edit::DocumentMut;

use crate::editor::Editor;
use crate::event::Event;
use crate::keymap::Keymaps;
use crate::llm::LlmConfig;
use crate::settings;
use crate::theme::Theme;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineNumber {
    Absolute,
    Relative,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Bar,
    Underline,
}

#[derive(Clone, Debug)]
pub struct EditorConfig {
    pub line_number: LineNumber,
    pub scrolloff: usize,
    pub tab_width: usize,
    /// (normal, insert, select)
    pub cursor_shape: (CursorShape, CursorShape, CursorShape),
    pub color_modes: bool,
    pub cursorline: bool,
    /// Top line = path and definition location (breadcrumb) + open buffers.
    pub header: bool,
    /// Language server inlay hints (types and parameter names as dimmed text).
    pub inlay_hints: bool,
    /// Indent guides (│) · right-hand scrollbar (including diagnostic markers).
    pub indent_guides: bool,
    /// Render doc comments (`///` `//!` `/** */`) as markdown (raw text when the cursor enters).
    pub render_doc_comments: bool,
    /// When started without arguments, reopen the files last open in this folder where they were.
    pub restore_session: bool,
    /// Diagnostics at the end of their line (like Error Lens) + faint background on error/warning lines.
    pub inline_diagnostics: bool,
    /// Cursor on an underline whose message doesn't fit at the line end → the message in full in a card.
    pub cursor_diagnostics: bool,
    /// Insert mode: an opener gets its closer (`pairs.rs`).
    pub auto_pairs: bool,
    /// Long lines as several rows: "prose" (Markdown, commit messages, plain text) | "always" | "never".
    pub soft_wrap: String,
    /// `:w` asks the language server to format first (lsp_editor.rs `format_then_save`).
    pub format_on_save: bool,
    /// Blame at the cursor line's end (`b` in the git tab flips it for the session).
    pub git_blame: bool,
    /// Auto-save: "off" | "focus" (when the terminal loses focus) | "idle" (+ after 2 s without input).
    pub auto_save: String,
    /// The key that hands the keys back from the built-in terminal to the code (`editor.terminal-escape`).
    pub terminal_escape: String,
    /// On save, record the undo history; restore it on reopen (`undofile.rs`).
    pub persistent_undo: bool,
    /// `:restart` builds with this first ("" = no build) and runs this binary ("" = the one running) —
    /// a relative path is from the project root (`restart.rs`).
    pub restart_build: String,
    pub restart_binary: String,
    /// Open an IDE server so Claude Code can connect (`agent.rs`).
    pub agent_claude_code: bool,
    /// Ask whether to download when opening a file whose language has no grammar (`grammar.rs` notice).
    pub offer_grammars: bool,
    /// Offer to install a missing language server or debugger the way its pack says (`install.rs`).
    pub offer_installs: bool,
    pub scrollbar: bool,
    pub theme: String,
    pub llm: LlmConfig,
    pub lsp: crate::lsp::LspConfig,
    /// Attach targets (`[[attach]]` — attach.rs). Same name: the later layer (project `.tarae.toml`) wins.
    pub attach: Vec<crate::attach::AttachTarget>,
    /// Build tasks (`[task.<name>] cmd = "…"`, optional `cwd`) — before the ones the project suggests.
    pub tasks: Vec<crate::task::Task>,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            line_number: LineNumber::Relative,
            scrolloff: 5,
            tab_width: 4,
            // Modes distinct at a glance: insert = bar · select = underline, color per mode too
            // (theme ui.cursor.primary.<mode>)
            cursor_shape: (CursorShape::Block, CursorShape::Bar, CursorShape::Underline),
            color_modes: true,
            cursorline: true,
            header: true,
            inlay_hints: true,
            indent_guides: true,
            render_doc_comments: true,
            restore_session: false,
            inline_diagnostics: true,
            cursor_diagnostics: true,
            auto_pairs: true,
            soft_wrap: "always".into(),
            format_on_save: true,
            git_blame: true,
            auto_save: "focus".into(),
            terminal_escape: "C-\\".into(),
            persistent_undo: true,
            restart_build: String::new(),
            restart_binary: String::new(),
            agent_claude_code: true,
            offer_grammars: true,
            offer_installs: true,
            scrollbar: true,
            theme: "default".into(),
            llm: LlmConfig::default(),
            lsp: crate::lsp::LspConfig { enabled: true, ..Default::default() },
            attach: Vec::new(),
            tasks: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub editor: EditorConfig,
    pub keymaps: Keymaps,
    /// Theme loaded by the `editor.theme` name (the config reader = watcher thread reads disk for us).
    pub theme: Theme,
}

// ── Paths ────────────────────────────────────────────────────────────────────

pub fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("tarae").join("config.toml"))
}

/// `$XDG_STATE_HOME/tarae` (default `~/.local/state/tarae`) — session, recent files, undo history.
pub fn state_dir() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state.join("tarae"))
}

/// `.tarae.toml` found by walking up from the current directory.
pub fn project_path() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let p = dir.join(".tarae.toml");
        if p.is_file() {
            return Some(p);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn layers() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = config_path().into_iter().collect();
    if let Some(p) = project_path()
        && !v.contains(&p)
    {
        v.push(p);
    }
    v
}

fn short(p: &Path) -> String {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match home.as_deref().and_then(|h| p.strip_prefix(h).ok()) {
        Some(rel) => format!("~/{}", rel.display()),
        None => p.display().to_string(),
    }
}

// ── Reading ──────────────────────────────────────────────────────────────────

/// Read the layers in order. Problems come back as `file:line: message` warnings (startup isn't blocked).
pub fn load() -> (Config, Vec<String>) {
    let mut config = Config::default();
    let mut warnings = Vec::new();
    for path in layers() {
        match fs::read_to_string(&path) {
            Ok(src) => apply_source(&mut config, &src, &short(&path), &mut warnings),
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => warnings.push(format!("{}: {e}", short(&path))),
        }
    }
    config.theme = crate::theme::load(&config.editor.theme).unwrap_or_else(|e| {
        warnings.push(e);
        Theme::default()
    });
    warnings.extend(config.theme.warnings.iter().cloned());
    (config, warnings)
}

/// A single string on top of defaults (for tests).
#[cfg(test)]
pub fn parse(src: &str) -> (Config, Vec<String>) {
    let mut config = Config::default();
    let mut warnings = Vec::new();
    apply_source(&mut config, src, "config.toml", &mut warnings);
    (config, warnings)
}

fn apply_source(config: &mut Config, src: &str, file: &str, warnings: &mut Vec<String>) {
    let table: toml::Table = match toml::from_str(src) {
        Ok(t) => t,
        Err(e) => {
            let line = e.span().map(|s| src[..s.start.min(src.len())].lines().count().max(1));
            let at = line.map(|l| format!(":{l}")).unwrap_or_default();
            warnings.push(format!("{file}{at}: {}", e.message()));
            return;
        }
    };
    settings::apply_table(&mut config.editor, &table, src, file, warnings);
    // Server and language tables have free-form names, so they're read here, not via the settings table.
    if let Some(Value::Table(servers)) = table.get("lsp") {
        let strings = |v: &Value, k: &str| -> Option<Vec<String>> {
            v.get(k)?.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        };
        for (name, v) in servers {
            // A built-in server needs no command (`[lsp.ruff] except = ["format"]`)
            let base = crate::lsp::default_spec(name);
            let command = match (v.get("command").and_then(Value::as_str), &base) {
                (Some(c), _) => c.to_string(),
                (None, Some(b)) => b.command.clone(),
                (None, None) => {
                    warnings.push(format!("{file}: lsp.{name}: needs command = \"...\""));
                    continue;
                }
            };
            let args =
                strings(v, "args").or_else(|| base.as_ref().map(|b| b.args.clone())).unwrap_or_default();
            let except = strings(v, "except").unwrap_or_default();
            for f in except.iter().filter(|f| !crate::lsp::FEATURES.contains(&f.as_str())) {
                warnings.push(format!(
                    "{file}: lsp.{name}.except: no feature \"{f}\" — {}",
                    crate::lsp::FEATURES.join(", ")
                ));
            }
            let spec = crate::lsp::ServerSpec { name: name.clone(), command, args, except };
            config.editor.lsp.servers.insert(name.clone(), spec);
        }
    }
    if let Some(Value::Table(langs)) = table.get("lang") {
        for (lang, v) in langs {
            if let Some(list) = v.get("lsp").and_then(Value::as_array) {
                let names = list.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
                config.editor.lsp.languages.insert(lang.clone(), names);
            }
            if let Some(list) = v.get("also").and_then(Value::as_array) {
                let names: Vec<String> = list.iter().filter_map(|x| x.as_str().map(str::to_string)).collect();
                for n in &names {
                    if !config.editor.lsp.servers.contains_key(n) && crate::lsp::default_spec(n).is_none() {
                        warnings
                            .push(format!("{file}: lang.{lang}.also: no server \"{n}\" — define [lsp.{n}]"));
                    }
                }
                config.editor.lsp.also.insert(lang.clone(), names);
            }
        }
    }
    if let Some(Value::Table(tasks)) = table.get("task") {
        for (name, v) in tasks {
            let Some(cmd) = v.get("cmd").and_then(Value::as_str) else {
                warnings.push(format!("{file}: task.{name}: needs cmd = \"...\""));
                continue;
            };
            let cwd = std::env::current_dir().unwrap_or_default();
            let cwd = v.get("cwd").and_then(Value::as_str).map_or(cwd.clone(), |c| cwd.join(c));
            config.editor.tasks.retain(|t| t.name != *name);
            config.editor.tasks.push(crate::task::Task { name: name.clone(), cmd: cmd.to_string(), cwd });
        }
    }
    if let Some(v) = table.get("attach") {
        for t in crate::attach::parse_config(v, file, warnings) {
            config.editor.attach.retain(|x| x.name != t.name);
            config.editor.attach.push(t);
        }
    }
    if let Some(Value::Table(keys)) = table.get("keys") {
        config.keymaps.merge_user(keys, warnings);
    }
}

/// Config file watcher thread — sleeps until the files may have changed (inotify/kqueue, see `fswatch`),
/// then compares mtimes. On change, it reads and parses here and sends only the result to the main loop.
pub fn watch(tx: Sender<Event>) {
    let mut paths = layers();
    if let Ok(cwd) = std::env::current_dir() {
        let p = cwd.join(".tarae.toml");
        if !paths.contains(&p) {
            paths.push(p); // even if missing yet — picked up as soon as it's created
        }
    }
    thread::spawn(move || {
        let stamps =
            || -> Vec<_> { paths.iter().map(|p| fs::metadata(p).and_then(|m| m.modified()).ok()).collect() };
        let mut last = stamps();
        // The plugins folder rides along: a few more stats, and no thread of its own
        let plugin_dir = crate::plugin::dir();
        let mut plugins = crate::plugin::Watch::new(plugin_dir.clone());
        loop {
            // Armed before looking, so a change made while we look still ends the wait below
            let mut watcher = crate::fswatch::Watcher::new();
            paths.iter().for_each(|p| watcher.file(p));
            if let Some(d) = &plugin_dir {
                watcher.folder(d);
            }
            if let Some(found) = plugins.changed() {
                let sync = move |ed: &mut Editor| ed.plugins_sync(found);
                if tx.send(Event::Job(Box::new(sync))).is_err() {
                    break;
                }
            }
            let now = stamps();
            if now != last {
                last = now;
                let (config, warnings) = load();
                let apply = move |ed: &mut Editor| ed.reload_config(config, warnings);
                if tx.send(Event::Job(Box::new(apply))).is_err() {
                    break;
                }
            }
            watcher.wait();
        }
    });
}

// ── Writing (`:set!`) ────────────────────────────────────────────────────────

/// Set `path` in the user config file to `v` — comments and formatting kept (toml_edit).
pub fn persist(path: &str, v: &Value) -> Result<PathBuf, String> {
    let file = config_path().ok_or("no config directory")?;
    let src = match fs::read_to_string(&file) {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.to_string()),
    };
    let mut doc: DocumentMut = src.parse().map_err(|e| format!("{}: {e}", short(&file)))?;
    set_path(&mut doc, path, v)?;
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let body = doc.to_string();
    crate::disk::write_atomic(&file, |w| w.write_all(body.as_bytes())).map_err(|e| e.to_string())?;
    Ok(file)
}

fn set_path(doc: &mut DocumentMut, path: &str, v: &Value) -> Result<(), String> {
    let value: toml_edit::Value = v.to_string().parse().map_err(|e: toml_edit::TomlError| e.to_string())?;
    let segs: Vec<&str> = path.split('.').collect();
    let (last, parents) = segs.split_last().expect("non-empty path");
    // Missing tables become [sections], not inline. Marked implicit, so intermediate tables holding only
    // subsections don't print an empty header.
    let fresh = || {
        let mut t = toml_edit::Table::new();
        t.set_implicit(true);
        toml_edit::Item::Table(t)
    };
    let mut item = doc.as_item_mut();
    for seg in parents.iter().copied() {
        if item.is_none() {
            *item = fresh();
        }
        if !item.is_table_like() {
            return Err(format!("config: '{seg}' is not a table"));
        }
        item = &mut item[seg];
    }
    if item.is_none() {
        *item = fresh();
    }
    if !item.is_table_like() {
        return Err(format!("config: '{path}' parent is not a table"));
    }
    // Change only the value; the line's comment (`scrolloff = 5  # margin`) is inherited from the old value.
    let mut value = value;
    if let Some(old) = item.get(last).and_then(|i| i.as_value()) {
        *value.decor_mut() = old.decor().clone();
    }
    item[*last] = toml_edit::value(value);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ```toml blocks of a Markdown file.
    fn toml_blocks(md: &str) -> Vec<&str> {
        md.split("```toml\n").skip(1).map(|b| &b[..b.find("```").unwrap()]).collect()
    }

    #[test]
    fn configuration_guide_example_loads_cleanly() {
        // The first ```toml block under docs/configuration.md's "## Full example" — the canonical example.
        let guide = include_str!("../docs/configuration.md");
        let section = &guide[guide.find("## Full example").unwrap()..];
        let (c, w) = parse(toml_blocks(section)[0]);
        assert!(w.is_empty(), "{w:?}");
        assert!(c.editor.color_modes);
        assert_eq!(c.editor.line_number, LineNumber::Relative);
        assert_eq!(c.editor.llm.model.as_deref(), Some("haiku"));
    }

    #[test]
    fn documented_config_snippets_load_cleanly() {
        // Every TOML snippet in the docs is a config file (must load without warnings), a theme file, or a
        // language pack (says so in its first line).
        let docs = [
            ("README.md", include_str!("../README.md")),
            ("docs/configuration.md", include_str!("../docs/configuration.md")),
            ("docs/testing-and-debugging.md", include_str!("../docs/testing-and-debugging.md")),
            ("docs/claude-integration.md", include_str!("../docs/claude-integration.md")),
        ];
        for (file, md) in docs {
            for block in toml_blocks(md) {
                if block.starts_with("inherits =") {
                    let theme: Result<toml::Table, _> = toml::from_str(block);
                    assert!(theme.is_ok(), "{file}: theme example: {theme:?}");
                } else if block.lines().next().is_some_and(|l| l.contains("pack.toml")) {
                    let pack: Result<toml::Table, _> = toml::from_str(block);
                    assert!(
                        pack.is_ok_and(|p| ["test", "tasks", "install"].iter().any(|k| p.contains_key(*k))),
                        "{file}: language pack example\n{block}"
                    );
                } else {
                    let (_, w) = parse(block);
                    assert!(w.is_empty(), "{file}: {w:?}\n{block}");
                }
            }
        }
    }

    #[test]
    fn parses_tarae_schema() {
        let (c, w) = parse(
            r#"
            [editor]
            line-numbers = "relative"
            cursor = { insert = "bar", select = "underline" }
            [llm]
            model = "haiku"
            [keys.normal]
            "A-/" = "repeat_last_motion"
            "#,
        );
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.editor.line_number, LineNumber::Relative);
        assert_eq!(c.editor.cursor_shape, (CursorShape::Block, CursorShape::Bar, CursorShape::Underline));
        assert_eq!(c.editor.llm.model.as_deref(), Some("haiku"));
    }

    #[test]
    fn attach_tables_are_not_unknown_settings() {
        let (c, w) = parse("[editor]\nscrolloff = 3\n\n[[attach]]\nname = \"api\"\nport = 5005\n");
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.editor.attach.len(), 1);
        assert_eq!(c.editor.scrolloff, 3);
    }

    #[test]
    fn syntax_error_reports_line() {
        let (_, w) = parse("[editor]\nscrolloff = = 3\n");
        assert_eq!(w.len(), 1);
        assert!(w[0].starts_with("config.toml:2:"), "{w:?}");
    }

    #[test]
    fn set_path_preserves_comments() {
        let mut doc: DocumentMut = "# mine\n[editor]\nscrolloff = 5 # keep\n".parse().unwrap();
        set_path(&mut doc, "editor.scrolloff", &Value::Integer(8)).unwrap();
        set_path(&mut doc, "editor.cursor.insert", &Value::from("bar")).unwrap();
        let s = doc.to_string();
        assert!(s.starts_with("# mine\n[editor]\nscrolloff = 8 # keep\n"), "{s}");
        let (c, w) = parse(&s);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.editor.scrolloff, 8);
        assert_eq!(c.editor.cursor_shape.1, CursorShape::Bar);
    }
}
