//! Config file — TOML + tarae's own schema (one table in `settings.rs`).
//! Layers: defaults < user `~/.config/tarae/config.toml` < project `.tarae.toml` < your `.tarae.local.toml`
//! (that project's, read only while git ignores it — `personal`) < runtime `:set`.

use std::fs;
use std::io::{ErrorKind, Write as _};
use std::path::{Path, PathBuf};

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
    pub restore_files: bool,
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
    /// Blame at the cursor line's end (`b` in the git tab flips it until quit).
    pub git_blame: bool,
    /// Auto-save: "off" | "focus" (when the terminal loses focus) | "idle" (+ after 2 s without input).
    pub auto_save: String,
    /// The key that hands the keys back from the built-in terminal to the code (`editor.terminal-escape`).
    pub terminal_escape: String,
    /// On save, record the undo history; restore it on reopen (`undofile.rs`).
    pub persistent_undo: bool,
    /// Keep a copy of every buffer with unsaved edits, offered back after a crash (`recovery.rs`) · files
    /// never kept besides the ones that hold keys: names anywhere, paths from the project root.
    pub recovery: bool,
    pub recovery_skip: Vec<String>,
    /// `:restart` builds with this first ("" = no build) and runs this binary ("" = the one running) —
    /// a relative path is from the project root (`restart.rs`).
    pub restart_build: String,
    pub restart_binary: String,
    /// Ask whether to download when opening a file whose language has no grammar (`grammar.rs` notice).
    pub offer_grammars: bool,
    /// Offer to install a missing language server or debugger the way its pack says (`install.rs`).
    pub offer_installs: bool,
    pub scrollbar: bool,
    /// Where sessions' checkouts go ("" = `$XDG_DATA_HOME/tarae/worktrees`; `worktree.rs`).
    pub session_worktrees: String,
    /// Count which features get used, on this machine only (`features.rs`) — only your own config turns it on.
    pub features_count: bool,
    /// A command (its words) run once a session's checkout is removed (empty = none; `ended.rs`).
    pub session_ended: Vec<String>,
    pub theme: String,
    pub llm: LlmConfig,
    pub lsp: crate::lsp::LspConfig,
    /// Attach targets (`[[attach]]` — attach.rs). Same name: the later layer (project `.tarae.toml`) wins.
    pub attach: Vec<crate::attach::AttachTarget>,
    /// Build tasks (`[task.<name>] cmd = "…"`, optional `cwd`) — before the ones the project suggests.
    pub tasks: Vec<crate::task::Task>,
    /// `:preflight`'s steps (`[preflight] steps = [{ name, cmd, note }, …]` — `preflight.rs`). A layer that
    /// gives them replaces the list (and the packs' defaults).
    pub preflight: Vec<crate::preflight::Step>,
    /// …and `.tarae.local.toml`'s, added after them (or after the packs').
    pub preflight_local: Vec<crate::preflight::Step>,
    /// A code review's risk grades (`risk.rs`): paths whose every change is high (a trusted setting) · paths
    /// that are tests, docs or snapshots — names anywhere, paths from the root.
    pub review_risky: Vec<String>,
    pub review_quiet: Vec<String>,
    /// What a run card of Claude's warns of besides the packs' (`[[cards.warn]]`, `cardwarn.rs`) — said as the
    /// file's (`.tarae.toml`). Not held back for trust: it can only add words, and with `asks` a card — an id
    /// a pack or yours has is refused (`read_cards`).
    pub cards_warn: Vec<crate::cardwarn::Warn>,
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
            restore_files: false,
            inline_diagnostics: true,
            cursor_diagnostics: true,
            auto_pairs: true,
            soft_wrap: "always".into(),
            format_on_save: true,
            git_blame: true,
            auto_save: "focus".into(),
            terminal_escape: "C-\\".into(),
            persistent_undo: true,
            recovery: true,
            recovery_skip: Vec::new(),
            restart_build: String::new(),
            restart_binary: String::new(),
            offer_grammars: true,
            offer_installs: true,
            scrollbar: true,
            session_worktrees: String::new(),
            features_count: false,
            session_ended: Vec::new(),
            theme: "default".into(),
            llm: LlmConfig::default(),
            lsp: crate::lsp::LspConfig { enabled: true, ..Default::default() },
            attach: Vec::new(),
            tasks: Vec::new(),
            preflight: Vec::new(),
            preflight_local: Vec::new(),
            review_risky: Vec::new(),
            review_quiet: Vec::new(),
            cards_warn: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct Config {
    pub editor: EditorConfig,
    pub keymaps: Keymaps,
    /// Theme loaded by the `editor.theme` name (the config reader = watcher thread reads disk for us).
    pub theme: Theme,
    /// The project file's held-back part (`trust.rs`) — None when it has none.
    pub project: Option<crate::trust::Project>,
}

// ── Paths ────────────────────────────────────────────────────────────────────

pub fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("tarae").join("config.toml"))
}

/// `$XDG_STATE_HOME/tarae` (default `~/.local/state/tarae`) — files to restore, recent files, undo history.
pub fn state_dir() -> Option<PathBuf> {
    let state = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))?;
    Some(state.join("tarae"))
}

/// `.tarae.toml` found by walking up from `root` (the editor's).
pub fn project_path(root: &Path) -> Option<PathBuf> {
    let mut dir = root.to_path_buf();
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

fn layers(root: &Path, user: Option<&Path>) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = user.map(Path::to_path_buf).into_iter().collect();
    if let Some(p) = project_path(root)
        && !v.contains(&p)
    {
        v.push(p);
    }
    v
}

/// The project layer, unless it's the user's own file (started in the config folder).
fn project_layer(root: &Path) -> Option<PathBuf> {
    project_path(root).filter(|p| config_path().as_ref() != Some(p))
}

/// Where a load reads what isn't the project's: your config file, and the answers about projects' trust —
/// the editor's own (`Editor::config_sources`: none in tests, so they never read yours).
#[derive(Clone, Debug, Default)]
pub struct Sources {
    pub user: Option<PathBuf>,
    pub trust: Option<PathBuf>,
}

impl Sources {
    /// This process's: `config_path`, `trust::Store::path`.
    pub fn own() -> Sources {
        Sources { user: config_path(), trust: crate::trust::Store::path() }
    }
}

/// `.tarae.local.toml` — yours, for one project: beside its `.tarae.toml`, or in the root (whether it's
/// there or not: the watch picks it up once made).
fn local_path(root: &Path) -> Option<PathBuf> {
    let dir = match project_path(root) {
        Some(p) => p.parent()?.to_path_buf(),
        None => root.to_path_buf(),
    };
    Some(dir.join(".tarae.local.toml"))
}

/// Whether `.tarae.local.toml` at `path` is yours to read as you'd read your own config: git says it's
/// untracked and ignored. Err (why not) otherwise — a committed one came with the repository (the
/// project's, which `.tarae.toml` is for), an untracked one git doesn't ignore may have come with a
/// download of it, and outside a repository git can't tell. Asked only when the file is there, of the
/// repository `root` (the editor's) resolves — not of what the file's folder may pass for (`gitrepo`).
pub(crate) fn personal(root: &Path, path: &Path) -> Result<(), String> {
    let Some((repo, rel)) = crate::gitrepo::for_file(root, path) else {
        return Err("not in a git repository, so git can't say it's yours".into());
    };
    if repo.run(&["ls-files", "--error-unmatch", "--", &rel], None).is_ok() {
        return Err("git tracks it — a committed one is the project's: put that in .tarae.toml".into());
    }
    if repo.run(&["check-ignore", "-q", "--", &rel], None).is_err() {
        return Err("git doesn't ignore it — add it to .gitignore or .git/info/exclude to use it".into());
    }
    Ok(())
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
/// The project's file (found from `root`, the editor's) goes in through `trust.rs`: what runs commands only
/// once the project is trusted.
pub fn load(root: &Path, from: &Sources) -> (Config, Vec<String>) {
    let store = crate::trust::Store::read(from.trust.as_deref());
    let local = local_path(root).filter(|p| p.is_file()).map(|p| (root.to_path_buf(), p));
    let project = project_layer(root).filter(|p| from.user.as_ref() != Some(p));
    load_layers(from.user.clone(), project, local, &store)
}

/// (tests: the layers without a local one)
#[cfg(test)]
pub(crate) fn load_from(
    user: Option<PathBuf>,
    project: Option<PathBuf>,
    store: &crate::trust::Store,
) -> (Config, Vec<String>) {
    load_layers(user, project, None, store)
}

/// The layers in order: yours, the project's (through `trust.rs`), then `.tarae.local.toml` — read as
/// yours when git ignores it (`personal`), else not at all, said so. Its `[preflight]` steps go after the
/// project's (`EditorConfig::preflight_local`), not in their place. `local` = (the editor's root, the file).
pub(crate) fn load_layers(
    user: Option<PathBuf>,
    project: Option<PathBuf>,
    local: Option<(PathBuf, PathBuf)>,
    store: &crate::trust::Store,
) -> (Config, Vec<String>) {
    let read = |path: &Path, warnings: &mut Vec<String>| match fs::read_to_string(path) {
        Ok(src) => Some(src),
        Err(e) if e.kind() == ErrorKind::NotFound => None,
        Err(e) => {
            warnings.push(format!("{}: {e}", short(path)));
            None
        }
    };
    let mut config = Config::default();
    let mut warnings = Vec::new();
    if let Some(path) = &user
        && let Some(src) = read(path, &mut warnings)
    {
        apply_source(&mut config, &src, &short(path), &mut warnings, None, true);
    }
    if let Some(path) = &project
        && let Some(src) = read(path, &mut warnings)
    {
        let file = short(path);
        let (mut without, mut w_without) = (config.clone(), Vec::new());
        let mut gate = crate::trust::Gate::new(false);
        apply_source(&mut without, &src, &file, &mut w_without, Some(&mut gate), false);
        if gate.held.is_empty() {
            config = without;
            warnings.extend(w_without);
        } else {
            let (mut with, mut w_with) = (config.clone(), Vec::new());
            apply_source(
                &mut with,
                &src,
                &file,
                &mut w_with,
                Some(&mut crate::trust::Gate::new(true)),
                false,
            );
            let real = fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            let hash = crate::trust::hash(&gate.held);
            let state = store.state(&real, hash);
            let trusted = state == crate::trust::State::Trusted;
            config = if trusted { with.clone() } else { without.clone() };
            warnings.extend(if trusted { w_with } else { w_without });
            config.project = Some(crate::trust::Project {
                file: real,
                hash,
                items: gate.held,
                state,
                with: Box::new((with.editor, with.keymaps)),
                without: Box::new((without.editor, without.keymaps)),
            });
        }
    }
    if let Some((root, path)) = &local
        && let Some(src) = read(path, &mut warnings)
    {
        match personal(root, path) {
            Ok(()) => {
                let file = short(path);
                let apply = |c: &mut Config, warnings: &mut Vec<String>| {
                    let project_steps = std::mem::take(&mut c.editor.preflight);
                    apply_source(c, &src, &file, warnings, None, false);
                    // (its steps were read in place of the project's: they go after them instead)
                    c.editor.preflight_local = std::mem::replace(&mut c.editor.preflight, project_steps);
                };
                apply(&mut config, &mut warnings);
                // …and on both ways the project's trust can go (the card switches between them)
                if let Some(p) = &mut config.project {
                    for half in [&mut p.with, &mut p.without] {
                        let (editor, keymaps) = (half.0.clone(), half.1.clone());
                        let mut c = Config { editor, keymaps, ..Config::default() };
                        apply(&mut c, &mut Vec::new());
                        **half = (c.editor, c.keymaps);
                    }
                }
            }
            Err(why) => warnings.push(format!("{}: not read — {why}", short(path))),
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
    apply_source(&mut config, src, "config.toml", &mut warnings, None, true);
    (config, warnings)
}

/// One layer on top of `config`. `gate` = a project's file: each thing that runs commands goes through it.
/// `your_config`: your config file — the one layer that sets `trust::YOURS_ONLY` (a project's two files are
/// refused them, said).
fn apply_source(
    config: &mut Config,
    src: &str,
    file: &str,
    warnings: &mut Vec<String>,
    mut gate: Option<&mut crate::trust::Gate>,
    your_config: bool,
) {
    // (a layer with no gate is yours: your config, `.tarae.local.toml` — the project's goes through `trust.rs`)
    let yours = gate.is_none();
    let mut pass = |key: String, v: &Value| gate.as_deref_mut().is_none_or(|g| g.pass(key, v));
    let table: toml::Table = match toml::from_str(src) {
        Ok(t) => t,
        Err(e) => {
            let line = e.span().map(|s| src[..s.start.min(src.len())].lines().count().max(1));
            let at = line.map(|l| format!(":{l}")).unwrap_or_default();
            warnings.push(format!("{file}{at}: {}", e.message()));
            return;
        }
    };
    let mut refused = Vec::new();
    let mut risky = |path: &str, v: &Value| {
        if !your_config && crate::trust::yours_only(path) {
            refused.push(path.to_string());
            return false;
        }
        !crate::trust::is_risky(path) || pass(path.to_string(), v)
    };
    settings::apply_table_gated(&mut config.editor, &table, src, file, warnings, &mut risky);
    for path in refused {
        let at = settings::locate(src, &path).map(|l| format!(":{l}")).unwrap_or_default();
        warnings.push(format!("{file}{at}: {path} is set only in your own config — not a project's file"));
    }
    // Server and language tables have free-form names, so they're read here, not via the settings table.
    if let Some(Value::Table(servers)) = table.get("lsp") {
        let strings = |v: &Value, k: &str| -> Option<Vec<String>> {
            v.get(k)?.as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect())
        };
        for (name, v) in servers {
            // A server's own command runs only once the project is trusted (`except` alone doesn't run one)
            if (v.get("command").is_some() || v.get("args").is_some()) && !pass(format!("lsp.{name}"), v) {
                continue;
            }
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
            if !pass(format!("task.{name}"), v) {
                continue;
            }
            let Some(cmd) = v.get("cmd").and_then(Value::as_str) else {
                warnings.push(format!("{file}: task.{name}: needs cmd = \"...\""));
                continue;
            };
            // (from the editor's root — resolved when it starts, `task_spawn`)
            let cwd = v.get("cwd").and_then(Value::as_str).map_or_else(PathBuf::new, PathBuf::from);
            config.editor.tasks.retain(|t| t.name != *name);
            config.editor.tasks.push(crate::task::Task { name: name.clone(), cmd: cmd.to_string(), cwd });
        }
    }
    if let Some(v) = table.get("preflight") {
        // Each step's command runs — each goes in only once the project is trusted (`preflight.<name>`)
        let steps = crate::preflight::parse_config(v, file, warnings);
        let n = steps.len();
        let passed: Vec<_> = steps
            .into_iter()
            .filter(|s| pass(format!("preflight.{}", s.name), &Value::from(s.cmd.as_str())))
            .collect();
        if n > 0 && passed.len() == n {
            config.editor.preflight = passed;
        }
    }
    if let Some(v) = table.get("attach") {
        for t in crate::attach::parse_config(v, file, warnings) {
            // `before` is a command run first (a port-forward)
            if let Some(b) = &t.before
                && !pass(format!("attach.{}.before", t.name), &Value::from(b.as_str()))
            {
                continue;
            }
            config.editor.attach.retain(|x| x.name != t.name);
            config.editor.attach.push(t);
        }
    }
    if let Some(cards) = table.get("cards") {
        read_cards(config, cards, file, warnings, yours);
    }
    if let Some(Value::Table(keys)) = table.get("keys") {
        // A key bound to a `:` command (`:sh …`) runs it when pressed
        let mut keys = keys.clone();
        drop_typed(&mut keys, "keys", &mut pass);
        // …and who pays for Claude is switched only by your own keys or a command you type: a project's file
        // (trusted or not, `.tarae.local.toml` too — as `trust::YOURS_ONLY`) never binds one, said
        if !your_config {
            let mut theirs = Vec::new();
            drop_typed(&mut keys, "keys", &mut |at, v| {
                let bills = typed_lines(v).any(|l| crate::trust::yours_only_command(&l));
                if bills {
                    theirs.push(at);
                }
                !bills
            });
            for at in theirs {
                warnings.push(format!("{file}: {at} switches billing — only your own keys do; left out"));
            }
        }
        config.keymaps.merge_user(&keys, warnings);
    }
}

/// The `:` lines a key binding's value runs (`":a"`, or those in a list), without their `:`.
fn typed_lines(v: &Value) -> Box<dyn Iterator<Item = String> + '_> {
    match v {
        Value::String(s) => Box::new(s.trim_start().strip_prefix(':').map(str::to_string).into_iter()),
        Value::Array(a) => Box::new(a.iter().flat_map(typed_lines)),
        _ => Box::new(std::iter::empty()),
    }
}

/// `[cards]`: `[[cards.warn]]`, what a run card warns of besides the packs' (`cardwarn.rs`) — said as this
/// file's (`.tarae.toml`). It only adds: an id a pack has, or an earlier layer, is left out and said — a cloned
/// project's file, read with no trust asked, can't drop or weaken a pack's warning nor yours (an `asks` made
/// one that never fires). Only yours (`yours`: your config, `.tarae.local.toml`) takes a project's id.
fn read_cards(config: &mut Config, cards: &Value, file: &str, warnings: &mut Vec<String>, yours: bool) {
    let Some(t) = cards.as_table() else {
        return warnings.push(format!("{file}: [cards] isn't a table"));
    };
    for k in t.keys().filter(|k| *k != "warn") {
        warnings.push(format!("{file}: unknown setting 'cards.{k}'"));
    }
    let Some(v) = t.get("warn") else { return };
    let whose = Path::new(file).file_name().map_or(file.to_string(), |n| n.to_string_lossy().into_owned());
    let mut problems = Vec::new();
    let new = crate::cardwarn::read(v, &whose, &mut problems);
    warnings.extend(problems.into_iter().map(|p| format!("{file}: cards.warn: {p}")));
    let packs = &crate::sandboxpack::get().warn;
    let mine = &mut config.editor.cards_warn;
    for w in new {
        let left_out = |whose: &str| format!("{file}: cards.warn: left out: id `{}` is the {whose}'s", w.id);
        if let Some(p) = packs.iter().find(|p| p.id == w.id) {
            warnings.push(left_out(&p.whose));
            continue;
        }
        match mine.iter().position(|o| o.id == w.id) {
            Some(i) if yours && !mine[i].yours => mine[i] = crate::cardwarn::Warn { yours, ..w },
            Some(i) => warnings.push(left_out(&mine[i].whose)),
            None => mine.push(crate::cardwarn::Warn { yours, ..w }),
        }
    }
}

/// Takes the bindings to `:` commands that `pass` doesn't let through out of a `[keys]` table (submodes too).
fn drop_typed(t: &mut toml::Table, path: &str, pass: &mut impl FnMut(String, &Value) -> bool) {
    let mut out = Vec::new();
    for (k, v) in t.iter_mut() {
        let at = format!("{path}.{k}");
        match v {
            Value::Table(sub) => drop_typed(sub, &at, pass),
            v if crate::trust::typed_binding(v) && !pass(at, v) => out.push(k.clone()),
            _ => {}
        }
    }
    for k in out {
        t.remove(&k);
    }
}

/// Config file watcher thread — sleeps until the files may have changed (inotify/kqueue, see `fswatch`),
/// then compares mtimes. On change, it reads and parses here and sends only the result to the main loop.
/// The project's files are `root`'s (the editor's); yours and the trust answers are `from`'s.
pub fn watch(tx: crate::event::Tx, root: PathBuf, from: Sources) {
    let mut paths = layers(&root, from.user.as_deref());
    let p = root.join(".tarae.toml");
    if !paths.contains(&p) {
        paths.push(p); // even if missing yet — picked up as soon as it's created
    }
    if let Some(p) = local_path(&root).filter(|p| !paths.contains(p)) {
        paths.push(p);
    }
    crate::event::thread("tarae-config", move || {
        let stamps =
            || -> Vec<_> { paths.iter().map(|p| fs::metadata(p).and_then(|m| m.modified()).ok()).collect() };
        let mut last = stamps();
        // The plugins folder rides along: a few more stats, and no thread of its own (beside your config —
        // `plugin::dir`, from `from`'s: none in tests)
        let plugin_dir = from.user.as_deref().and_then(Path::parent).map(|d| d.join("plugins"));
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
                let (config, warnings) = load(&root, &from);
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
    persist_to(&file, path, v)?;
    Ok(file)
}

/// Set `path` in the config file `file` to `v` — comments and formatting kept.
pub fn persist_to(file: &Path, path: &str, v: &Value) -> Result<(), String> {
    let src = match fs::read_to_string(file) {
        Ok(s) => s,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.to_string()),
    };
    let mut doc: DocumentMut = src.parse().map_err(|e| format!("{}: {e}", short(file)))?;
    set_path(&mut doc, path, v)?;
    if let Some(dir) = file.parent() {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let body = doc.to_string();
    crate::disk::write_atomic(file, |w| w.write_all(body.as_bytes())).map_err(|e| e.to_string())
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
pub(crate) mod tests {
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
                        pack.is_ok_and(|p| {
                            !p.is_empty() && p.keys().all(|k| crate::packs::reach(k).is_some())
                        }),
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

    /// A project file in a scratch folder of its own (its real path; the folder is the caller's to remove) —
    /// the values are never run.
    pub(crate) fn project_file(name: &str, src: &str) -> PathBuf {
        let dir = crate::scratch::Scratch::new(&format!("config-{name}")).into_path();
        let path = dir.join(".tarae.toml");
        fs::write(&path, src).unwrap();
        path
    }

    pub(crate) const RISKY_PROJECT: &str = r#"
"editor.restart-binary" = "bin/marker"
[editor]
scrolloff = 9
restart-build = "touch marker"
[llm]
run-allow = ["make"]
review-at-end = true
[session]
ended = ["touch", "marker"]
[task.build]
cmd = "touch marker"
[preflight]
steps = [{ name = "check", cmd = "touch marker" }]
[lsp.mine]
command = "my-server"
[lsp.ruff]
except = ["format"]
[keys.normal]
X = ":sh touch marker"
Y = "move_line_down"
[keys.normal.space]
Z = ["move_line_down", ":sh touch marker"]
"#;

    /// Whose credentials Claude uses, who pays for it, and what its chat edits with are yours to say
    /// (`trust::YOURS_ONLY`): a project's `.tarae.toml` — trusted or not — and its `.tarae.local.toml` are refused
    /// `llm.environment-key`, `llm.billing`, `llm.reviewer-billing`, `llm.edit-tools` and `llm.claude-sandbox`
    /// (claude itself unconfined), and say so at each line; your own config sets them.
    #[test]
    fn a_setting_only_yours_sets_is_refused_from_both_project_files() {
        use crate::billing::Billing;
        use crate::trust::Store;
        let scratch = crate::scratch::Scratch::new("config-yours-only");
        let dir = scratch.to_path_buf();
        let ok = std::process::Command::new("git").args(["init", "-q"]).current_dir(&dir).output().unwrap();
        assert!(ok.status.success());
        let (user, project, local) =
            (dir.join("user.toml"), dir.join(".tarae.toml"), dir.join(".tarae.local.toml"));
        let yours_only = "environment-key = \"keep\"\nbilling = \"api\"\nreviewer-billing = \"api\"\nedit-tools = \"native\"\nclaude-sandbox = false\n";
        // (a risky setting beside them, so the project has a trust card to answer)
        fs::write(&project, format!("[llm]\nrun-allow = [\"make\"]\n{yours_only}")).unwrap();
        fs::write(&local, format!("[llm]\n{yours_only}")).unwrap();
        fs::write(dir.join(".gitignore"), ".tarae.local.toml\n").unwrap();
        let load = |user: Option<PathBuf>, store: &Store| {
            load_layers(user, Some(project.clone()), Some((dir.clone(), local.clone())), store)
        };
        let none_set = |c: &Config| {
            let l = &c.editor.llm;
            !l.keep_env_key
                && l.billing == Billing::Subscription
                && l.reviewer_billing.is_none()
                && l.edit_tools == crate::edittool::Arm::Tarae
                && l.claude_sandbox
        };
        let (c, w) = load(None, &Store::default());
        assert!(none_set(&c), "neither project file sets them");
        for (file, first) in [(".tarae.toml", 3), (".tarae.local.toml", 2)] {
            for (i, key) in ["environment-key", "billing", "reviewer-billing", "edit-tools", "claude-sandbox"]
                .into_iter()
                .enumerate()
            {
                let said = format!(
                    "{file}:{}: llm.{key} is set only in your own config — not a project's file",
                    first + i
                );
                assert!(w.iter().any(|w| w.ends_with(&said)), "{said}: {w:?}");
            }
        }
        let mut trusted = Store::default();
        trusted.answer(&project, c.project.as_ref().unwrap().hash, true);
        let (t, _) = load(None, &trusted);
        assert!(t.editor.llm.run_allow == ["make"] && none_set(&t), "trusted: still refused");
        fs::write(&user, format!("[llm]\n{yours_only}")).unwrap();
        let (mine, w) = load(Some(user.clone()), &Store::default());
        let l = &mine.editor.llm;
        assert!(
            l.keep_env_key
                && l.billing == Billing::Api
                && l.reviewer_billing == Some(Billing::Api)
                && l.edit_tools == crate::edittool::Arm::Native
                && !l.claude_sandbox,
            "{w:?}"
        );
    }

    /// Who pays for Claude is switched only by your own keys or a command you type: a key a project's file binds
    /// to `:session-billing` — `.tarae.toml` trusted or not, `.tarae.local.toml` — is left out, said at its key
    /// (a trusted project's other `:` bindings run as before); your config's binding stays.
    #[test]
    fn a_projects_key_binding_never_switches_billing() {
        use crate::keymap::{Lookup, MappableCommand};
        use crate::trust::Store;
        let scratch = crate::scratch::Scratch::new("config-bill-keys");
        let dir = scratch.to_path_buf();
        let ok = std::process::Command::new("git").args(["init", "-q"]).current_dir(&dir).output().unwrap();
        assert!(ok.status.success());
        let (user, project, local) =
            (dir.join("user.toml"), dir.join(".tarae.toml"), dir.join(".tarae.local.toml"));
        let keys = "[keys.normal]\nX = \":session-billing api\"\nY = \":sh true\"\n\
                    [keys.normal.space]\nZ = [\"move_line_down\", \":session-billing subscription\"]\n";
        fs::write(&project, keys).unwrap();
        fs::write(&local, "[keys.normal]\nW = \":session-billing api\"\n").unwrap();
        fs::write(dir.join(".gitignore"), ".tarae.local.toml\n").unwrap();
        let load = |user: Option<PathBuf>, store: &Store| {
            load_layers(user, Some(project.clone()), Some((dir.clone(), local.clone())), store)
        };
        let typed = |c: &Config, seq: &str| -> Vec<String> {
            let keys: Vec<crate::key::Key> = seq
                .split(' ')
                .map(|k| match k {
                    "space" => crate::key::Key::plain(crate::key::Code::Char(' ')),
                    k => crate::key::Key::plain(crate::key::Code::Char(k.chars().next().unwrap())),
                })
                .collect();
            match c.keymaps.lookup(crate::editor::Mode::Normal, &keys) {
                Lookup::Matched(cmds) => cmds
                    .iter()
                    .filter_map(|c| match c {
                        MappableCommand::Typed(t) => Some(t.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            }
        };
        let (c, _) = load(None, &Store::default());
        let mut trusted = Store::default();
        trusted.answer(&project, c.project.as_ref().unwrap().hash, true);
        let (t, w) = load(None, &trusted);
        assert_eq!(typed(&t, "Y"), ["sh true"], "a trusted project's other `:` binding runs");
        for seq in ["X", "space Z", "W"] {
            assert!(typed(&t, seq).is_empty(), "{seq}: no billing switch from a project's file");
        }
        for (file, at) in [
            (".tarae.toml", "keys.normal.X"),
            (".tarae.toml", "keys.normal.space.Z"),
            (".tarae.local.toml", "keys.normal.W"),
        ] {
            let said = format!("{file}: {at} switches billing — only your own keys do; left out");
            assert!(w.iter().any(|w| w.ends_with(&said)), "{said}: {w:?}");
        }
        fs::write(&user, "[keys.normal]\nV = \":session-billing api\"\n").unwrap();
        let (mine, _) = load(Some(user.clone()), &trusted);
        assert_eq!(typed(&mine, "V"), ["session-billing api"], "your own key does");
    }

    /// `.tarae.local.toml` is read as yours only while git says it's untracked and ignored: its settings over
    /// the project's, its preflight steps after the project's — on both ways the project's trust can go.
    /// Not ignored, committed, or outside a repository: not read, and why is said.
    #[test]
    fn a_local_layer_is_read_only_while_git_ignores_it() {
        use crate::trust::{State, Store};
        let scratch = crate::scratch::Scratch::new("config-local");
        let dir = scratch.to_path_buf();
        let git = |args: &[&str]| {
            let ok = std::process::Command::new("git").args(args).current_dir(&dir).output().unwrap();
            assert!(ok.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&ok.stderr));
        };
        let (project, local) = (dir.join(".tarae.toml"), dir.join(".tarae.local.toml"));
        fs::write(
            &project,
            "[editor]\nscrolloff = 9\n[preflight]\nsteps = [{ name = \"check\", cmd = \"cargo check\" }]\n",
        )
        .unwrap();
        fs::write(
            &local,
            "[editor]\nscrolloff = 4\n[preflight]\nsteps = [{ name = \"mine\", cmd = \"make lint\" }]\n",
        )
        .unwrap();
        fs::write(dir.join(".gitignore"), ".tarae.local.toml\n").unwrap();
        let load = |store: &Store| {
            load_layers(None, Some(project.clone()), Some((dir.clone(), local.clone())), store)
        };
        let names = |s: &[crate::preflight::Step]| s.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        // Outside a repository: git can't say it's yours
        let (c, w) = load(&Store::default());
        assert_eq!(c.editor.scrolloff, 9);
        assert!(
            w.iter().any(|w| w.ends_with("not read — not in a git repository, so git can't say it's yours")),
            "{w:?}"
        );
        // Ignored, untracked: yours — over the project's settings, its steps after (the project not trusted
        // yet: its own steps held, yours there either way)
        git(&["init", "-q"]);
        let (c, w) = load(&Store::default());
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(c.editor.scrolloff, 4);
        assert!(c.editor.preflight.is_empty(), "the project's steps wait for trust");
        assert_eq!(names(&c.editor.preflight_local), ["mine"]);
        assert_eq!(c.editor.preflight_local[0].from, ".tarae.local.toml");
        let p = c.project.as_ref().unwrap();
        assert_eq!(p.state, State::Ask);
        assert!(
            p.items.iter().all(|(k, _)| !k.contains("mine")),
            "yours isn't on the trust card: {:?}",
            p.items
        );
        let mut store = Store::default();
        store.answer(&project, p.hash, true);
        let (t, _) = load(&store);
        assert_eq!(
            (names(&t.editor.preflight), names(&t.editor.preflight_local)),
            (vec!["check".into()], vec!["mine".into()])
        );
        assert_eq!(
            (t.editor.scrolloff, &p.with.0.preflight_local[0].name),
            (4, &"mine".to_string()),
            "both ways"
        );
        // Not ignored: not read
        fs::write(dir.join(".gitignore"), "").unwrap();
        let (c, w) = load(&Store::default());
        assert_eq!((c.editor.scrolloff, c.editor.preflight_local.len()), (9, 0));
        assert!(
            w.iter().any(|w| w.contains("not read — git doesn't ignore it — add it to .gitignore")),
            "{w:?}"
        );
        // Committed (ignored or not): the project's — not read as yours
        fs::write(dir.join(".gitignore"), ".tarae.local.toml\n").unwrap();
        git(&["add", "-f", ".tarae.local.toml"]);
        let (c, w) = load(&Store::default());
        assert_eq!(c.editor.scrolloff, 9);
        assert!(w.iter().any(|w| w.contains("not read — git tracks it")), "{w:?}");
    }

    fn bound(c: &Config, keys: &str) -> Option<String> {
        let keys: Vec<crate::key::Key> = keys.split(' ').map(|k| k.parse().unwrap()).collect();
        match c.keymaps.lookup(crate::editor::Mode::Normal, &keys) {
            crate::keymap::Lookup::Matched(cmds) => {
                Some(cmds.iter().map(|c| c.doc()).collect::<Vec<_>>().join(","))
            }
            _ => None,
        }
    }

    #[test]
    fn a_projects_commands_wait_for_trust() {
        use crate::trust::{State, Store};
        let path = project_file("trust", RISKY_PROJECT);
        let (c, w) = load_from(None, Some(path.clone()), &Store::default());
        assert!(w.is_empty(), "{w:?}");
        // The harmless rest applies
        assert_eq!(c.editor.scrolloff, 9);
        assert_eq!(c.editor.lsp.servers["ruff"].except, ["format"]);
        assert_eq!(bound(&c, "Y").as_deref(), Some(crate::commands::find("move_line_down").unwrap().doc));
        // What runs commands doesn't — however it's written
        assert_eq!(c.editor.restart_build, "");
        assert_eq!(c.editor.restart_binary, "");
        assert!(c.editor.llm.run_allow.is_empty());
        assert!(!c.editor.llm.review_at_end, "edits going in unseen, their checks run: not unasked");
        assert!(c.editor.session_ended.is_empty(), "nothing run at a session's end");
        assert!(c.editor.tasks.iter().all(|t| t.name != "build"));
        assert!(c.editor.preflight.is_empty(), "no preflight steps");
        assert!(!c.editor.lsp.servers.contains_key("mine"));
        assert_ne!(bound(&c, "X").as_deref(), Some(":sh touch marker"), "the default binding stays");
        assert!(bound(&c, "space Z").is_none_or(|b| !b.contains(":sh")));
        let p = c.project.as_ref().expect("held back");
        assert_eq!(p.state, State::Ask);
        let keys: Vec<&str> = p.items.iter().map(|(k, _)| k.as_str()).collect();
        for k in [
            "editor.restart-binary",
            "editor.restart-build",
            "llm.run-allow",
            "llm.review-at-end",
            "session.ended",
            "task.build",
            "preflight.check",
            "lsp.mine",
            "keys.normal.X",
            "keys.normal.space.Z",
        ] {
            assert!(keys.contains(&k), "{k} in {keys:?}");
        }
        assert!(p.items.iter().any(|(k, v)| k == "editor.restart-build" && v == "\"touch marker\""));
        // Trusted for what it says now: all of it applies
        let mut store = Store::default();
        store.answer(&path, p.hash, true);
        let (t, _) = load_from(None, Some(path.clone()), &store);
        assert_eq!(t.project.as_ref().unwrap().state, State::Trusted);
        assert_eq!(t.editor.restart_build, "touch marker");
        assert_eq!(t.editor.restart_binary, "bin/marker");
        assert_eq!(t.editor.llm.run_allow, ["make"]);
        assert_eq!(t.editor.session_ended, ["touch", "marker"]);
        assert!(t.editor.llm.review_at_end);
        assert!(t.editor.tasks.iter().any(|t| t.name == "build"));
        assert_eq!(t.editor.preflight.iter().map(|s| s.cmd.as_str()).collect::<Vec<_>>(), ["touch marker"]);
        assert!(t.editor.lsp.servers.contains_key("mine"));
        assert_eq!(bound(&t, "X").as_deref(), Some(":sh touch marker"));
        // A change asks again, and holds it back meanwhile
        fs::write(&path, RISKY_PROJECT.replace("bin/marker", "bin/other")).unwrap();
        let (a, _) = load_from(None, Some(path.clone()), &store);
        assert_eq!(a.project.as_ref().unwrap().state, State::Ask);
        assert_eq!(a.editor.restart_binary, "");
        // Ignored: held back, not asked
        store.answer(&path, a.project.as_ref().unwrap().hash, false);
        let (i, _) = load_from(None, Some(path.clone()), &store);
        assert_eq!(i.project.as_ref().unwrap().state, State::Ignored);
        assert_eq!(i.editor.restart_build, "");
    }

    /// `llm.sandbox-upstream` from a project file, all the way to a run's policy: held back until the project is
    /// trusted (else an untrusted file could send the runs around the user's proxy), then `"none"` reaches
    /// `LlmConfig` and the policy made from it — the proxy named in tarae's environment isn't chained to; with
    /// `"env"` it is.
    #[test]
    fn a_projects_upstream_setting_waits_for_trust_then_reaches_the_policy() {
        use crate::sandbox::{Here, Os, policy_in};
        use crate::trust::{State, Store};
        let path = project_file("upstream", "[llm]\nsandbox-upstream = \"none\"\n");
        let (c, _) = load_from(None, Some(path.clone()), &Store::default());
        let p = c.project.as_ref().expect("held back");
        assert_eq!(p.state, State::Ask);
        assert!(p.items.iter().any(|(k, v)| k == "llm.sandbox-upstream" && v == "\"none\""), "{:?}", p.items);
        let mut store = Store::default();
        store.answer(&path, p.hash, true);
        let (t, _) = load_from(None, Some(path.clone()), &store);
        // The policy each config makes, the network on, tarae's environment naming a proxy
        let root = path.parent().unwrap();
        let here = Here {
            env: std::collections::HashMap::from([(
                "HTTPS_PROXY".to_string(),
                std::ffi::OsString::from("http://up.example.invalid:3128"),
            )]),
            tmp: Some(root.to_path_buf()),
            uid: 501,
            os: Os::HOST,
            ..Here::default()
        };
        let through = |c: &Config| {
            let llm = crate::llm::LlmConfig { sandbox_network: true, ..c.editor.llm.clone() };
            policy_in(root, &llm, &[], &here).egress.upstream.through()
        };
        assert!(through(&c), "untrusted: the user's own choice (\"env\") holds");
        assert!(!through(&t), "trusted: \"none\" — straight out");
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn the_users_own_file_is_never_held_back() {
        let user = project_file("user", RISKY_PROJECT);
        let (c, _) = load_from(Some(user), None, &crate::trust::Store::default());
        assert!(c.project.is_none());
        assert_eq!(c.editor.restart_build, "touch marker");
        // and a project file that sets nothing that runs has nothing to ask
        let plain = project_file("plain", "[editor]\nscrolloff = 3\n");
        let (p, _) = load_from(None, Some(plain), &crate::trust::Store::default());
        assert!(p.project.is_none());
        assert_eq!(p.editor.scrolloff, 3);
    }

    /// The project's files are the editor's root's — found walking up from it, not from the folder tarae
    /// runs in (the tests run in this repository, which has a `.tarae.toml` of its own); a task's folder
    /// stays as written, for the root to resolve when it starts.
    #[test]
    fn the_project_layer_is_the_roots() {
        let scratch = crate::scratch::Scratch::new("config-root");
        let root = scratch.to_path_buf();
        std::fs::create_dir_all(root.join("deep/er")).unwrap();
        assert_eq!(local_path(&root), Some(root.join(".tarae.local.toml")), "none yet: beside the root");
        fs::write(root.join(".tarae.toml"), "[task.t]\ncmd = \"true\"\ncwd = \"sub\"\n").unwrap();
        assert_eq!(project_path(&root.join("deep/er")), Some(root.join(".tarae.toml")), "walking up");
        assert_eq!(project_layer(&root), Some(root.join(".tarae.toml")));
        assert_eq!(local_path(&root.join("deep")), Some(root.join(".tarae.local.toml")));
        // (read as your own file: a project's tasks wait for trust)
        let (c, _) = load_from(project_layer(&root), None, &crate::trust::Store::default());
        let t = c.editor.tasks.iter().find(|t| t.name == "t").map(|t| t.cwd.clone());
        assert_eq!(t, Some(PathBuf::from("sub")), "as written");
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
