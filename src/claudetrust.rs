//! A repository's Claude Code settings, held until trusted (F1 of the API-billing design, apibill 2026-10-10).
//! The chat starts claude with the project's settings files loaded (`--setting-sources user,project,local`,
//! `llm.chat-settings` — so CLAUDE.md loads), and under `-p` Claude Code shows no trust dialog: a repository's
//! `.claude/settings.json` has its `env` block, its helper commands (`apiKeyHelper` …) and the plugins it turns
//! on used as they are (Claude Code's docs, "What runs before you trust a folder"). A repository as cloned,
//! pulled or checked out could run a command as you each time the chat starts, move its billing to someone's
//! key, or send every prompt elsewhere (`ANTHROPIC_BASE_URL`). Claude's own runs can't write `.claude`
//! (`sandbox::protected_names`), and its edits there are always reviewed — it's the files that came with the
//! repository.
//!
//! So before the chat's claude starts, tarae reads those files itself (on a worker — `look`) and holds what runs
//! a command or sets claude's environment until you trust it on a card, as `trust.rs` does for `.tarae.toml`.
//! Answers go in its store (`$state/trust.json`), under `.claude/settings.json`'s place — a repository's
//! checkouts share them.
//! - Held: `COMMANDS` (shown whole), every `env` name (names only: a value may be a token — the digest still
//!   covers it), every plugin `enabledPlugins` turns on, `cleanupPeriodDays` (when Claude Code deletes your
//!   conversations — every project's; held back: yours, else `CLEANUP_DEFAULT`).
//! - Shown, not asked: `HELD`, which the chat's own `--settings` already turns off (`disableAllHooks` — it
//!   outranks the project's files).
//! - Held back (`n`), or a command too long to show (never trusted, as `trust.rs`): claude starts with each such
//!   key laid over in its `--settings` (which outranks the project's files) as it would be **without these
//!   files**, as far as that can be done with **no value of yours on the command line** but a few that claude
//!   needs and that aren't secret (`RESTORED`: `PATH`, `SHELL`, `HOME`, `TMPDIR`, `USER`, `LANG`, `TERM`, `LC_*`
//!   — back to tarae's value). The project's files pick the names: copying your value for any name they choose
//!   would let an untrusted repository publish a variable of yours (`DATABASE_URL`, `AWS_SESSION_TOKEN`) on a
//!   command line your other processes read — on macOS tarae's sandboxed runs too. So:
//!   - every other variable → "". "" reads as unset there (live check L6: `ANTHROPIC_BASE_URL` held to "" sent
//!     the request to Anthropic); for provider selection Claude Code says so ("treats the empty value as unset
//!     for provider selection"), and "Subprocesses still inherit the empty value" — the chat starts none of
//!     its own: no Bash (`chat::tools`), hooks off (`chat::CHAT_SETTINGS`), `.mcp.json` off
//!     (`--strict-mcp-config`, `chat::ensure_proc`), tarae's tools in-process (`sdkMcpServers`,
//!     `llm::initialize_message`). claude itself starts a few (git, `security`, `sh` for `uname`/`ps`/`ioreg`:
//!     measured, `docs/threat-model.md` §2): they inherit the empty values, never the project's;
//!   - a variable your own Claude settings set too → that file isn't loaded at all (its source left out of
//!     `--setting-sources`, so yours applies — for the project's file that's its CLAUDE.md too, Claude Code's
//!     SDK docs: "To load CLAUDE.md project instructions, include `project`");
//!   - a command key you set too → yours, unless it looks like a secret (`tokens::token_like`) — then that file
//!     isn't loaded; plugins → false.
//! - A file Claude Code may read some way tarae can't tell (not a JSON object, a key of another shape, too big)
//!   can't be held back key by key: it's never trusted, and isn't loaded at all.
//!
//! **Every command key is held up front** — those the files don't set yet too, unless you set it yourself (then
//! yours applies, and a project's one added later wins until the watch below stops claude): Claude Code reloads
//! an edited helper in a running session (Claude Code's settings docs), so a `git pull` adding one would run it
//! before tarae could look. An `env` name or a plugin the files don't set yet can't be named up front: for those
//! the window stays — from the write, until tarae's watch (an `fswatch` wake once the burst settles: ≥ 50 ms, at most
//! about 0.5 s) has read
//! the file again and stopped claude. Claude Code reapplies an `env` change to a running session at once (its env
//! reference), so a variable added by a pull can reach claude's next request in that window.
//!
//! A project skill's `allowed-tools` would pre-approve tools — "Workspace trust doesn't gate this field … even in
//! a `-p` run" (Claude Code's skills docs). Not in the chat: skills run through the `Skill` tool (its tools
//! reference), and the chat's `--tools` names only `chat::tools`, which "restricts the session's built-in tools
//! to the ones it names"; its `--settings` also denies `Skill` (a deny no level can allow). A skill typed as
//! `/name` would be your own message — tarae's messages start with the editor context, never `/`.
//!
//! `.claude/settings.local.json` is yours when git says it's untracked and ignored (`config::personal`, as for
//! `.tarae.local.toml`): not asked about then, and part of `Baseline`.
//!
//! The chat doesn't wait for the card (sandbox UX piece A): while it's up, claude runs held back — exactly as `n`
//! leaves it — and a message typed meanwhile goes to it. `y` gives claude the files at its next start: at once
//! between turns, else once its turn is over, the conversation resumed (`chat::settings_restart`).
//!
//! While the chat is open a watch reads both files again when they change: a change that needs the card or other
//! overrides restarts its claude at once with what holds now — held back while the card asks again.

use std::path::{Path, PathBuf};

use serde_json::{Map, Value as Json};

use crate::editor::Editor;
use crate::trust::{Digest, State};

/// The files, from the project's root, and the setting source each is (`--setting-sources`).
pub const FILES: [(&str, &str); 2] =
    [(".claude/settings.json", "project"), (".claude/settings.local.json", "local")];

/// The sources the chat loads when nothing is left out.
pub const SOURCES: &str = "user,project,local";

/// Keys that run a command as you, from any settings file (Claude Code's settings reference: each key typed as
/// a shell command line whose scope is "Any file").
pub const COMMANDS: [&str; 5] =
    ["apiKeyHelper", "awsAuthRefresh", "awsCredentialExport", "gcpAuthRefresh", "otelHeadersHelper"];

/// Keys that run commands but are off in the chat already: `disableAllHooks` in its `--settings` turns hooks
/// off and narrows the other three to managed settings (Claude Code's settings reference, `disableAllHooks`).
pub const HELD: [&str; 4] = ["hooks", "statusLine", "subagentStatusLine", "fileSuggestion"];

/// The setting for how many days Claude Code keeps your conversations: a project's would decide for every
/// project's (`0` deletes them all) — held.
const CLEANUP: &str = "cleanupPeriodDays";

/// Claude Code's own `cleanupPeriodDays` when no file sets it (https://code.claude.com/docs/en/settings,
/// `cleanupPeriodDays`: default 30) — what a project's is held back to when yours sets none.
pub const CLEANUP_DEFAULT: u64 = 30;

/// A file bigger than this isn't read: never trusted, left out.
const READ_MAX: u64 = 1 << 20;

pub const TITLE: &str = "Trust this project's Claude settings?";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Command,
    Env,
    Plugin,
    /// A `permissions.allow` rule: a tool unasked. Unused under `-p` in a folder never trusted, used in one you
    /// once trusted in Claude Code (Claude Code's permissions docs, "What runs before you trust a folder") —
    /// tarae can't tell which, so it's held.
    Allow,
    /// A `permissions.additionalDirectories` entry: a folder Claude reads as its own — held as `Allow`.
    Dir,
    /// `cleanupPeriodDays` (`CLEANUP`), its value as written.
    Cleanup,
}

/// One thing a file sets that's held until trusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// The file, from the root (`FILES`).
    pub file: &'static str,
    pub kind: Kind,
    /// The setting, the variable's name, the plugin's.
    pub key: String,
    /// As written — a command, a variable's value (never shown), a plugin's switch.
    pub value: String,
}

impl Item {
    /// Whether the card shows all of it: a command too long to show is never trusted (`trust::SHOWN_MAX`).
    fn shown_whole(&self) -> bool {
        self.kind != Kind::Command
            || crate::trust::visible_line(&self.value).chars().count() <= crate::trust::SHOWN_MAX
    }
}

/// What the project's Claude settings hold that needs an answer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Scan {
    pub items: Vec<Item>,
    /// What's off in the chat already (file, key) — shown on the card, not asked.
    pub held: Vec<(&'static str, String)>,
    /// Files that can't be read key by key (file, why) — never trusted, never loaded.
    pub unreadable: Vec<(&'static str, String)>,
    /// What an answer is about: every item with its whole value, and each unreadable file's bytes.
    pub hash: Digest,
}

/// What claude would have without the project's files, for what they set: your own Claude settings (your
/// `~/.claude/settings.json`, then your own local file over it), else tarae's environment — as far as a
/// hold-back may use it (`overrides`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Baseline {
    /// Each command key your settings set, and its command.
    pub commands: std::collections::BTreeMap<String, String>,
    /// Each name the files' `env` blocks set that your own Claude settings set too.
    pub yours: std::collections::BTreeSet<String>,
    /// Each name the files' `env` blocks set that's on `RESTORED`, and its value in tarae's environment.
    pub env: std::collections::BTreeMap<String, String>,
    /// Your `cleanupPeriodDays`, as written — None: you set none.
    pub cleanup: Option<String>,
}

/// The project's Claude settings as last read, and the answer for them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Seen {
    /// None: nothing in them needs an answer.
    pub scan: Option<Scan>,
    pub state: State,
    /// Where the answer is kept (`trust.rs`'s store): the project's `.claude/settings.json`.
    pub file: PathBuf,
    pub base: Baseline,
}

impl Seen {
    /// What it's about — None when nothing needs an answer.
    pub fn digest(&self) -> Option<Digest> {
        self.scan.as_ref().map(|s| s.hash)
    }
}

/// A chat's gate: claude starts once the project's settings are read — held back while their card waits.
pub struct Gate {
    pub seen: Option<Seen>,
    /// A read under way.
    reading: bool,
    /// This gate's — a closed chat's late read isn't another chat's.
    id: u64,
    /// The `--settings` and `--setting-sources` its claude was started with: a change that needs other ones
    /// stops it.
    started: Option<(String, String)>,
    /// Ends the watch when the chat goes.
    watch: Option<crate::fswatch::Waker>,
    /// Its claude starts again, the conversation resumed, once its turn is over — `y` mid-turn, or a change that
    /// stopped it mid-turn (then its exit ends the turn) — `chat::settings_restart`.
    pub(crate) again: bool,
}

impl Default for Gate {
    fn default() -> Gate {
        static IDS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        let id = IDS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Gate { seen: None, reading: false, id, started: None, watch: None, again: false }
    }
}

impl Drop for Gate {
    fn drop(&mut self) {
        if let Some(w) = &self.watch {
            w.wake();
        }
    }
}

impl Gate {
    /// claude can't start yet: the settings are being read (milliseconds — a card waiting doesn't hold it).
    pub fn waiting(&self) -> bool {
        self.reading
    }
}

// ── Reading ─────────────────────────────────────────────────────────────────

/// One file as read: None — not there, or yours; Err — why it wasn't read.
pub type FileRead = Option<Result<Vec<u8>, String>>;

/// What one file holds: what's held until trusted, and what's off already (file, key).
type Holds = (Vec<Item>, Vec<(&'static str, String)>);

/// What one file holds — Err: why it can't be read key by key.
fn read_one(file: &'static str, bytes: &[u8]) -> Result<Holds, String> {
    let json: Json = serde_json::from_slice(bytes).map_err(|_| "not JSON".to_string())?;
    let obj = json.as_object().ok_or("not a JSON object")?;
    let mut items = Vec::new();
    for key in COMMANDS {
        if let Some(v) = obj.get(key) {
            let value = v.as_str().ok_or(format!("{key} isn't a string"))?;
            items.push(Item { file, kind: Kind::Command, key: key.into(), value: value.into() });
        }
    }
    if let Some(env) = obj.get("env") {
        let env = env.as_object().ok_or("env isn't an object")?;
        for (name, v) in env {
            let value = v.as_str().map_or_else(|| v.to_string(), String::from);
            items.push(Item { file, kind: Kind::Env, key: name.clone(), value });
        }
    }
    if let Some(plugins) = obj.get("enabledPlugins") {
        let plugins = plugins.as_object().ok_or("enabledPlugins isn't an object")?;
        // (anything but `false` may turn it on)
        for (name, on) in plugins.iter().filter(|(_, on)| on.as_bool() != Some(false)) {
            items.push(Item { file, kind: Kind::Plugin, key: name.clone(), value: on.to_string() });
        }
    }
    if let Some(days) = obj.get(CLEANUP) {
        items.push(Item { file, kind: Kind::Cleanup, key: CLEANUP.into(), value: days.to_string() });
    }
    if let Some(p) = obj.get("permissions") {
        let p = p.as_object().ok_or("permissions isn't an object")?;
        for (key, kind) in [("allow", Kind::Allow), ("additionalDirectories", Kind::Dir)] {
            let Some(list) = p.get(key) else { continue };
            for v in list.as_array().ok_or(format!("permissions.{key} isn't a list"))? {
                let rule = v.as_str().ok_or(format!("permissions.{key} holds a non-string"))?;
                items.push(Item { file, kind, key: rule.into(), value: String::new() });
            }
        }
    }
    let held = HELD.iter().filter(|k| obj.contains_key(**k)).map(|k| (file, k.to_string())).collect();
    Ok((items, held))
}

/// What the files hold (each file and its bytes; None: not there, or yours) — None when nothing needs an answer.
pub fn scan(files: &[(&'static str, FileRead)]) -> Option<Scan> {
    let (mut items, mut held, mut unreadable) = (Vec::new(), Vec::new(), Vec::new());
    let mut digested: Vec<(String, String)> = Vec::new();
    for (file, bytes) in files {
        // (an unreadable file is about its bytes — or, not read at all, why)
        let (read, about) = match bytes {
            None => continue,
            Some(Err(why)) => (Err(why.clone()), why.clone()),
            Some(Ok(b)) => (read_one(file, b), crate::sha256::hex(b)),
        };
        match read {
            Ok((i, h)) => {
                items.extend(i);
                held.extend(h);
            }
            Err(why) => {
                digested.push((format!("{file}\0unreadable"), about));
                unreadable.push((*file, why));
            }
        }
    }
    if items.is_empty() && unreadable.is_empty() {
        return None;
    }
    digested.extend(items.iter().map(|i| (format!("{}\0{:?}\0{}", i.file, i.kind, i.key), i.value.clone())));
    let hash = crate::trust::hash(&digested);
    Some(Scan { items, held, unreadable, hash })
}

/// A settings file as read — a regular file only (a FIFO there would hold the read), at most `READ_MAX`.
fn read_file(path: &Path) -> FileRead {
    match std::fs::metadata(path) {
        Err(_) => None,
        Ok(m) if !m.is_file() => Some(Err("not a regular file".to_string())),
        Ok(m) if m.len() > READ_MAX => Some(Err(format!("{} bytes — too big to read", m.len()))),
        Ok(_) => Some(std::fs::read(path).map_err(|e| e.to_string())),
    }
}

/// What your own settings file holds, as items (none when it can't be read key by key).
fn yours_of(label: &'static str, read: FileRead) -> Vec<Item> {
    match read {
        Some(Ok(b)) => read_one(label, &b).map(|(items, _)| items).unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// The project's Claude settings now, the answer kept for them (`store`: `trust.rs`'s; None: none kept), and
/// what claude would have without them — on a worker. `user`: your own Claude settings file (None: none read);
/// `yours`: whether the local file is the user's own (git says untracked and ignored); `env`: a variable of
/// the environment claude gets.
pub fn look(
    root: &Path,
    store: Option<&Path>,
    user: Option<&Path>,
    yours: impl Fn(&Path) -> bool,
    env: impl Fn(&str) -> Option<String>,
) -> Seen {
    // Yours: your settings file, then your own local file over it (as Claude Code ranks them)
    let mut mine = yours_of("your settings", user.and_then(read_file));
    let files: Vec<(&'static str, FileRead)> = FILES
        .iter()
        .map(|(name, source)| {
            let path = root.join(name);
            let read = read_file(&path);
            if *source == "local" && matches!(read, Some(Ok(_))) && yours(&path) {
                mine.extend(yours_of(name, read));
                return (*name, None);
            }
            (*name, read)
        })
        .collect();
    let file = root.join(FILES[0].0);
    let scan = scan(&files);
    let state = match &scan {
        None => State::Trusted,
        Some(s) => crate::trust::Store::read(store).state(&file, s.hash),
    };
    let base = baseline(scan.as_ref(), &mine, env);
    Seen { scan, state, file, base }
}

/// What claude would have without the project's files: each command key yours set, and for each name their
/// `env` blocks set, yours (`mine` — the later winning) else `env`'s.
pub fn baseline(scan: Option<&Scan>, mine: &[Item], env: impl Fn(&str) -> Option<String>) -> Baseline {
    let mut b = Baseline::default();
    for i in mine.iter().filter(|i| i.kind == Kind::Command) {
        b.commands.insert(i.key.clone(), i.value.clone());
    }
    b.cleanup = mine.iter().rfind(|i| i.kind == Kind::Cleanup).map(|i| i.value.clone());
    let named = scan.into_iter().flat_map(|s| &s.items).filter(|i| i.kind == Kind::Env);
    for name in named.map(|i| &i.key) {
        if mine.iter().any(|i| i.kind == Kind::Env && &i.key == name) {
            b.yours.insert(name.clone());
        } else if let Some(v) = env(name).filter(|_| restored(name)) {
            b.env.insert(name.clone(), v);
        }
    }
    b
}

// ── Starting claude ─────────────────────────────────────────────────────────

/// The variables a held-back name goes back to tarae's value for — on claude's command line, which your other
/// processes can read, so only these: none a secret, and claude needs them to work. `PATH` and `SHELL`: claude
/// finds its shell by them (Claude Code's env reference, `CLAUDE_CODE_SHELL`: "Auto-detection uses your
/// `$SHELL` … on your `PATH`"). `HOME`, `TMPDIR`: a project's `env` can't set those at all (its settings
/// reference, "Variables Claude Code ignores in `env`") — listed in case that changes. `USER`, `LANG`, `TERM`,
/// `LC_*`: who runs, how text reads (runenv's floor) — no Claude Code page names them [tarae's choice].
pub const RESTORED: [&str; 7] = ["PATH", "HOME", "SHELL", "TMPDIR", "USER", "LANG", "TERM"];
const RESTORED_FAMILIES: [&str; 1] = ["LC_"];

fn restored(name: &str) -> bool {
    RESTORED.contains(&name) || RESTORED_FAMILIES.iter().any(|f| name.starts_with(f))
}

/// Whether `name=value` looks like a secret (`tokens::token_like`, as runenv) — a command of yours that does
/// never goes on the command line.
fn secret(name: &str, value: &str) -> bool {
    crate::tokens::token_like(&format!("{name}={value}"))
}

/// What `--settings` lays over the files, and the sources left out. A variable held back: `RESTORED` ones to
/// tarae's value, every other one to "" — never a value of yours the files picked by naming it; one your own
/// Claude settings set too: its file left out (yours applies, off the command line). A command key held back:
/// "", or yours where you set it too — unless yours looks like a secret, then its file is left out. Every
/// command key is held, set by the files or not, unless they set it and it's trusted, or only you set it.
pub fn overrides(seen: &Seen) -> (Map<String, Json>, Vec<&'static str>) {
    let trusted = seen.state == State::Trusted;
    let items: Vec<&Item> = seen.scan.iter().flat_map(|s| &s.items).collect();
    let held = |i: &Item| !trusted || !i.shown_whole();
    let source = |file: &str| FILES.iter().find(|(f, _)| *f == file).map(|(_, s)| *s);
    let (mut out, mut env, mut plugins, mut left_out) = (Map::new(), Map::new(), Map::new(), Vec::new());
    let mut ask: Vec<Json> = Vec::new();
    for key in COMMANDS {
        let theirs: Vec<&&Item> = items.iter().filter(|i| i.kind == Kind::Command && i.key == key).collect();
        if !theirs.is_empty() && theirs.iter().all(|i| !held(i)) {
            continue;
        }
        match seen.base.commands.get(key) {
            // (yours, and the files don't set it: yours applies as it is)
            Some(_) if theirs.is_empty() => {}
            // (yours and theirs: yours — off the command line, by theirs not loading, when it looks like a secret)
            Some(v) if secret(key, v) => left_out.extend(theirs.iter().filter_map(|i| source(i.file))),
            Some(v) => {
                out.insert(key.into(), Json::String(v.clone()));
            }
            None => {
                out.insert(key.into(), Json::String(String::new()));
            }
        }
    }
    for i in items.into_iter().filter(|i| held(i)) {
        match i.kind {
            Kind::Command => {}
            Kind::Env if seen.base.yours.contains(&i.key) => left_out.extend(source(i.file)),
            Kind::Env => {
                let v = seen.base.env.get(&i.key).cloned().unwrap_or_default();
                env.insert(i.key.clone(), Json::String(v));
            }
            Kind::Plugin => {
                plugins.insert(i.key.clone(), Json::Bool(false));
            }
            // (an `ask` rule outranks an `allow` rule from any file — Claude Code's settings docs — so each asks,
            // and tarae answers it: `reads.rs`, the edit review)
            Kind::Allow => ask.push(Json::String(i.key.clone())),
            // (a folder can't be taken back by an override: its file isn't loaded)
            Kind::Dir => left_out.extend(source(i.file)),
            Kind::Cleanup => {
                let yours = seen.base.cleanup.as_deref().and_then(|v| serde_json::from_str(v).ok());
                out.insert(CLEANUP.into(), yours.unwrap_or(Json::from(CLEANUP_DEFAULT)));
            }
        }
    }
    if !ask.is_empty() {
        out.insert("permissions".into(), serde_json::json!({ "ask": ask }));
    }
    if !env.is_empty() {
        out.insert("env".into(), Json::Object(env));
    }
    if !plugins.is_empty() {
        out.insert("enabledPlugins".into(), Json::Object(plugins));
    }
    crate::sorting::sort(&mut left_out);
    left_out.dedup();
    (out, left_out)
}

/// The chat's `--settings` (`base`, the chat's own, with the overrides laid in) and `--setting-sources` (a
/// source left out by the overrides, or for a file it can't read key by key) for what was seen. A card still
/// waiting holds back as `n` does (`overrides`: only trusted lets a key through).
pub fn settings(base: &str, seen: &Seen) -> (String, String) {
    let mut json: Map<String, Json> = serde_json::from_str(base).unwrap_or_default();
    let (over, mut left_out) = overrides(seen);
    for (k, v) in over {
        match (json.get_mut(&k), v) {
            // (lists add up — the chat's own `ask` rules stay beside the held allow rules)
            (Some(Json::Object(mine)), Json::Object(theirs)) => {
                for (k, v) in theirs {
                    match (mine.get_mut(&k), v) {
                        (Some(Json::Array(a)), Json::Array(b)) => a.extend(b),
                        (_, v) => {
                            mine.insert(k, v);
                        }
                    }
                }
            }
            (_, v) => {
                json.insert(k, v);
            }
        }
    }
    let unreadable = seen.scan.iter().flat_map(|s| &s.unreadable);
    left_out.extend(unreadable.filter_map(|(u, _)| FILES.iter().find(|(f, _)| f == u).map(|(_, s)| *s)));
    let sources: Vec<&str> = SOURCES.split(',').filter(|s| !left_out.contains(s)).collect();
    (Json::Object(json).to_string(), sources.join(","))
}

/// `:granted`'s `d` on the project's Claude settings (forgotten in the store by then), in the session whose chat
/// read them: held back now — a claude running with them started again without (mid-turn stopped, as `n` does);
/// the card asks when a chat reads them next.
pub(crate) fn forgotten(ed: &mut Editor) {
    let Some(c) = ed.session.chat.as_mut() else { return };
    let Some(seen) = c.settings.seen.as_mut().filter(|s| s.state == State::Trusted) else { return };
    seen.state = State::Ask;
    let now = settings(crate::chat::CHAT_SETTINGS, seen);
    if c.running() && Some(&now) != c.settings.started.as_ref() {
        crate::chat::settings_restart(
            ed,
            "Taken back — Claude started again without the project's settings, the conversation going on",
            true,
        );
    }
}

/// The chat's claude may start (`chat::ensure_proc`): its `--settings` and `--setting-sources` — None while
/// the project's settings are read (it starts once they are).
pub fn ready(ed: &mut Editor, base: &str) -> Option<(String, String)> {
    if !ed.claude_settings_gate {
        return Some((base.into(), SOURCES.into()));
    }
    let c = ed.session.chat.as_mut()?;
    let start = match &c.settings.seen {
        None => {
            if !c.settings.reading {
                read(ed);
            }
            return None;
        }
        // The card still waiting: held back, as `n` holds it — F1's safe state (`docs/threat-model.md` §2): none
        // of the repository's commands, environment, plugins, allow rules or folders reach claude before `y`, so
        // it needn't wait for the answer. Only trusted lets them through (`overrides`)
        Some(s) => settings(base, s),
    };
    c.settings.started = Some(start.clone());
    Some(start)
}

/// Before a ping forks the chat's claude (`keepwarm.rs`): the project's settings read again now, on a worker, and
/// `then` told whether they're what the gate last saw and the chat's claude was started as they hold now — a fork
/// is a new claude reading the files as they are, and a key added since isn't held by the chat's `--settings`.
/// The gate off (no project settings read, or tests'): they hold. None: no chat (closed, or closed meanwhile).
pub fn still_holds(ed: &mut Editor, then: impl FnOnce(&mut Editor, Option<bool>) + Send + 'static) {
    if ed.session.chat.is_none() {
        return then(ed, None);
    }
    if !ed.claude_settings_gate || !ed.config.llm.chat_settings {
        return then(ed, Some(true));
    }
    let Some(id) = ed.session.chat.as_ref().map(|c| c.settings.id) else { return then(ed, None) };
    let places = Places::of(ed);
    ed.events.jobs().spawn(move || {
        let seen = places.look();
        move |ed: &mut Editor| {
            let c = ed.session.chat.as_ref().filter(|c| c.settings.id == id);
            let holds = c.map(|c| {
                let now = settings(crate::chat::CHAT_SETTINGS, &seen);
                c.settings.seen.as_ref() == Some(&seen) && c.settings.started.as_ref() == Some(&now)
            });
            then(ed, holds)
        }
    });
}

/// Reads the project's settings on a worker — the chat's gate gets them.
fn read(ed: &mut Editor) {
    let Some(c) = ed.session.chat.as_mut() else { return };
    c.settings.reading = true;
    let id = c.settings.id;
    let places = Places::of(ed);
    ed.events.jobs().spawn(move || {
        let seen = places.look();
        move |ed: &mut Editor| seen_now(ed, id, seen)
    });
}

/// What a read needs, taken to a worker: the project's root, the answers' store, your Claude settings file.
#[derive(Clone)]
struct Places {
    root: PathBuf,
    store: Option<PathBuf>,
    user: Option<PathBuf>,
    /// `Editor::claude_env`: None — tarae's own.
    env: Option<std::collections::HashMap<String, String>>,
}

impl Places {
    fn of(ed: &Editor) -> Places {
        Places {
            root: ed.session.root.clone(),
            store: ed.trust_path.clone(),
            user: ed.claude_user_settings.clone(),
            env: ed.claude_env.clone(),
        }
    }

    /// `look` as tarae reads: git says whether the local file is yours, and claude gets tarae's environment
    /// (`llm::spawn` passes it on) — or the one `Editor::claude_env` gives.
    fn look(&self) -> Seen {
        let yours = |p: &Path| crate::config::personal(&self.root, p).is_ok();
        let env = |n: &str| match &self.env {
            Some(given) => given.get(n).cloned(),
            None => std::env::var(n).ok(),
        };
        look(&self.root, self.store.as_deref(), self.user.as_deref(), yours, env)
    }
}

/// Your own Claude settings file — `$CLAUDE_CONFIG_DIR/settings.json`, else `~/.claude/settings.json` (Claude
/// Code's settings docs). Only its command keys and `env` are read, for `Baseline` — not where claude's folder is
/// tarae's (`Places::of`).
pub fn user_settings() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        Some(d) => Some(PathBuf::from(d).join("settings.json")),
        None => std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".claude/settings.json")),
    }
}

/// The settings as just read (the first time, or after a change): the card when they need one; claude started —
/// held back while it's up — or, running, started again at once when what it runs with no longer holds or the
/// card asks again.
fn seen_now(ed: &mut Editor, id: u64, seen: Seen) {
    let gate_on = ed.claude_settings_gate;
    let Some(c) = ed.session.chat.as_mut().filter(|c| c.settings.id == id) else { return };
    c.settings.reading = false;
    let first = c.settings.seen.is_none();
    // The same content: the answer in hand holds — the store may not have it yet (kept on a worker)
    if c.settings.seen.as_ref().is_some_and(|s| s.digest() == seen.digest() && s.file == seen.file) {
        return;
    }
    let ask = seen.state == State::Ask;
    let now = settings(crate::chat::CHAT_SETTINGS, &seen);
    // (asked again: what the files say now is held back whatever claude ran with — a held name's value it reads
    // live, a new one isn't named yet — so it starts again, never only on what's different)
    let restart = c.running() && (ask || Some(&now) != c.settings.started.as_ref());
    c.settings.seen = Some(seen.clone());
    if first && gate_on {
        watch(ed, id);
    }
    if ask {
        offer(ed, &seen);
    } else {
        // (this session's card only — up in front, or parked while it's behind; its part of the repository's)
        ed.drop_cards(|w| matches!(w, crate::offer::What::ClaudeSettings { .. }));
        crate::repotrust::drop_part(ed, |w| matches!(w, crate::offer::What::ClaudeSettings { .. }), false);
    }
    match (restart, ask) {
        (true, true) => crate::chat::settings_restart(
            ed,
            "The project's Claude settings changed — Claude started again with them held back; the card asks",
            true,
        ),
        (true, false) => crate::chat::settings_restart(
            ed,
            "The project's Claude settings changed — Claude started again with them as answered",
            true,
        ),
        (false, ask) => {
            if ask && let Some(c) = ed.session.chat.as_mut() {
                c.note(
                    "The project's Claude settings run commands as you — Claude started with them held back",
                );
            }
            crate::chat::gate_opened(ed);
        }
    }
}

/// Reads the settings again whenever either file changes, while the chat is open (its gate's waker ends it).
fn watch(ed: &mut Editor, id: u64) {
    let waker = crate::fswatch::Waker::new();
    let Some(c) = ed.session.chat.as_mut() else { return };
    c.settings.watch = Some(waker.clone());
    let tx = ed.events.sender();
    let places = Places::of(ed);
    let paths: Vec<PathBuf> = FILES.iter().map(|(f, _)| places.root.join(f)).collect();
    crate::event::thread("tarae-claudeset", move || {
        let stamps = || -> Vec<_> {
            paths.iter().map(|p| std::fs::metadata(p).ok().map(|m| (m.modified().ok(), m.len()))).collect()
        };
        // (read once more at the start: a change made since the first read isn't missed)
        let mut last = Vec::new();
        loop {
            // Armed before looking, so a change made while we look still ends the wait below
            let mut watcher = crate::fswatch::Watcher::with_waker(waker.clone());
            paths.iter().for_each(|p| watcher.file(p));
            let now = stamps();
            if now != last {
                last = now;
                let seen = places.look();
                let apply = move |ed: &mut Editor| seen_now(ed, id, seen);
                if tx.send(crate::event::Event::Job(Box::new(apply))).is_err() {
                    break;
                }
            }
            if watcher.wait() == crate::fswatch::Woke::Cancelled {
                break;
            }
        }
    });
}

// ── The card ────────────────────────────────────────────────────────────────

/// The card's text, and the lines being decided: where claude stands (held back unless trusted — it doesn't wait
/// for the card), then what's held, each on a line of its own, as written (an env value never — names only).
/// Short: it stands beside the work while claude runs; the rest is `v`'s (`detail`).
pub fn question(seen: &Seen) -> (String, std::ops::Range<usize>) {
    let Some(scan) = &seen.scan else { return (String::new(), 0..0) };
    let first = match seen.state {
        State::Trusted => "Claude has these — n holds them back, starting it again.",
        _ => "Claude started with these held back — y gives them to it, the conversation going on.",
    };
    let mut lines = vec![first.to_string(), String::new()];
    let line = crate::trust::visible_line;
    for i in &scan.items {
        lines.push(match i.kind {
            Kind::Command if !i.shown_whole() => format!(
                "  {}: {} = ({} characters — too long to show, so never used)",
                i.file,
                i.key,
                i.value.chars().count()
            ),
            Kind::Command => format!("  {}: {} = {}", i.file, i.key, line(&i.value)),
            Kind::Env => format!("  {}: env {} (its value not shown)", i.file, line(&i.key)),
            Kind::Plugin => format!("  {}: turns on the plugin {}", i.file, line(&i.key)),
            Kind::Allow => format!("  {}: lets {} run unasked", i.file, line(&i.key)),
            Kind::Dir => format!("  {}: adds the folder {} for Claude", i.file, line(&i.key)),
            Kind::Cleanup => format!(
                "  {}: {CLEANUP} = {} (Claude Code deletes your conversations older than that, every project's)",
                i.file,
                line(&i.value)
            ),
        });
    }
    for (file, why) in &scan.unreadable {
        lines.push(format!("  {file}: {why} — never used"));
    }
    let decided = 2..lines.len();
    (lines.join("\n"), decided)
}

/// What the card leaves to `v`, after its text: why it asks, what's off already, what holding back does.
pub fn detail(seen: &Seen) -> String {
    let mut lines = vec![
        "The project's Claude Code settings run commands as you or set Claude's environment — read in the chat \
         with no trust dialog of Claude Code's."
            .to_string(),
    ];
    let held = seen.scan.iter().flat_map(|s| &s.held);
    let held: Vec<String> = held.map(|(f, k)| format!("{f} {k}")).collect();
    if !held.is_empty() {
        lines.push(String::new());
        lines.push(format!("Already off in tarae's chat: {}.", held.join(", ")));
    }
    lines.push(String::new());
    lines.push(
        "Held back, Claude runs with these emptied — PATH, SHELL and the locale your own — the plugins off, \
         cleanupPeriodDays yours (else Claude Code's default, 30 days); the rest of the files still apply. No value of yours goes on Claude's command line for a name they chose; a \
         file that sets a variable your own Claude settings set too isn't loaded at all, so yours applies."
            .to_string(),
    );
    lines.join("\n")
}

/// Its card, up — or joining the repository's, when another part waits with it (`repotrust.rs`).
fn offer(ed: &mut Editor, seen: &Seen) {
    if let Some(o) = card(seen) {
        ed.trust_offer(o);
    }
}

fn card(seen: &Seen) -> Option<crate::offer::Offer> {
    let hash = seen.digest()?;
    let (q, decided) = question(seen);
    let asked = crate::cards::Asked::new(crate::cards::Why::Own, crate::cards::Request::Own);
    let what = crate::offer::What::ClaudeSettings { hash };
    let o = crate::offer::Offer::new(TITLE.into(), &q, "", "Claude settings".into(), what).deciding(decided);
    Some(o.asked(asked))
}

impl Editor {
    /// The answer on the card (`hash` = what it showed): kept, and the chat's claude starts as answered — or,
    /// already running with other settings, starts again on the same conversation: trusted, between turns (at
    /// once, else at its turn's end); held back, at once.
    pub fn claude_settings_answer(&mut self, id: u64, hash: Digest, trusted: bool) {
        self.offer_finished(id, Ok(()));
        let Some(n) = self.claude_settings_answered(hash, trusted) else { return };
        match trusted {
            true => {
                self.set_success(format!("Trusted — the project's {n} Claude settings apply in the chat"))
            }
            false => self.set_status("Held back — :claude-settings to look again"),
        }
    }

    /// The answer for what the settings say (`hash`), on their card or the repository's: kept, claude started as
    /// answered — how many settings it was about (None: no chat, or they say something else now).
    pub(crate) fn claude_settings_answered(&mut self, hash: Digest, trusted: bool) -> Option<usize> {
        let c = self.session.chat.as_mut()?;
        let seen = c.settings.seen.as_mut().filter(|s| s.digest() == Some(hash))?;
        seen.state = if trusted { State::Trusted } else { State::Ignored };
        let (seen, n) = (seen.clone(), seen.scan.as_ref().map_or(0, |s| s.items.len()));
        let now = settings(crate::chat::CHAT_SETTINGS, &seen);
        if c.running() && Some(&now) != c.settings.started.as_ref() {
            match trusted {
                // (held back while it answers is more than asked — it gets them once its turn is over)
                true => crate::chat::settings_restart(
                    self,
                    "Trusted — Claude starts again with these settings, the conversation going on",
                    false,
                ),
                false => crate::chat::settings_restart(
                    self,
                    "Held back — Claude started again without them, the conversation going on",
                    true,
                ),
            }
        }
        if let Some(path) = self.trust_path.clone() {
            let file = seen.file.clone();
            self.events.jobs().spawn(move || {
                let r = crate::trust::record(path, crate::trust::Kind::Claude, file, hash, trusted);
                move |ed: &mut Editor| {
                    if let Err(e) = r {
                        ed.set_error(format!("couldn't keep the answer: {e}"));
                    }
                }
            });
        }
        crate::chat::gate_opened(self);
        Some(n)
    }

    /// `v` on the card: its text and `detail` in its buffer — for what the card shows (`hash`), else (read again
    /// since) its text alone.
    pub fn claude_settings_view(&mut self, hash: Digest) {
        let seen = self.session.chat.as_ref().and_then(|c| c.settings.seen.as_ref());
        let more = seen.filter(|s| s.digest() == Some(hash)).map(detail).unwrap_or_default();
        self.offer_view_with(more);
    }

    /// `:claude-settings` — the card again, for what the project's Claude settings held when last read.
    pub fn claude_settings_ask(&mut self) -> Result<(), String> {
        let c = self.session.chat.as_ref().ok_or("the chat reads them when it opens — space l")?;
        let seen = match &c.settings.seen {
            Some(s) => s.clone(),
            None if c.settings.reading => return Err("still reading them — a moment".into()),
            None => return Err("not read yet — they're read when Claude starts".into()),
        };
        if seen.scan.is_none() {
            return Err("nothing in this project's Claude settings needs trust".into());
        }
        // (asked alone: out of the repository's card, if it's there)
        crate::repotrust::drop_part(self, |w| matches!(w, crate::offer::What::ClaudeSettings { .. }), false);
        if let Some(o) = card(&seen) {
            self.push_offer(o);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
