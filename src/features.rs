//! Which of tarae's features get used, counted on this machine only (`docs/feature-counts.md`) — off until your own
//! config turns it on (`features.count`, `trust::YOURS_ONLY`); off, nothing is counted and nothing written.
//! Counted, each with the day it was last used: the command a key ran (not the key), a command chosen in the
//! palette, a `:` command (its name — never what follows it), a plugin's command (its name), a card asked and
//! its answer, a side panel tab shown, a picker opened, Claude's tool calls. Never what they were about: every
//! name counted comes from a table in the code or a plugin's registered command — a `:` line naming nothing
//! known isn't counted at all, a tool name Claude made up is `other`.
//!
//! - **Who**: keys are one count (`By::Keys`) — a person's typing and a driver's keys (zellij's `write-chars`)
//!   reach tarae alike, as the terminal's input, so they aren't told apart, and no guess (timing, bursts) is
//!   passed off as telling. Claude's tool calls come over its stream: their own count, exact. A shuttle's steps
//!   (`shuttle.rs`) come over the UI socket: their own count too (`By::Shuttle`), where the socket hands them in.
//! - **Cost**: the key path adds a number at an index (`Counts::key`); the day is read once a batch of events
//!   (`Editor::features_turn`). Nothing wakes tarae for it: what's counted goes to disk on a worker with the
//!   first batch `SAVE_EVERY_S` after the last save, and when tarae ends (`Editor::features_flush`).
//! - **Where**: `$state/features.json`, one per user, 0600 — taraes running side by side each add what they
//!   counted since their last save, under a lock (`save`): none overwrites another's.
//! - **Nothing leaves**: no network here; Claude's level of the UI state never has them (they aren't part of
//!   `UiState`); the UI socket that answers `tarae state --features` refuses Claude's processes.

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::commands::StaticCommand;
use crate::editor::Editor;

/// The file in tarae's state folder.
pub const FILE: &str = "features.json";
const V: u64 = 1;
/// What's counted goes to disk with the first batch of events this long after the last save.
pub const SAVE_EVERY_S: u64 = 300;

/// Where a feature is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Area {
    /// A command a key ran (`commands::COMMANDS`).
    Command,
    /// A command chosen in the palette (`space ?`).
    Palette,
    /// A `:` command, typed or bound to a key — by its first name (`cmdline::COMMANDS`).
    Typed,
    /// A plugin's command, by the name it registered.
    Plugin,
    /// A card asked, by its kind (`Offer::kind`).
    Card,
    /// A card answered: its kind and the answer's letter.
    Answer,
    /// A side panel tab shown.
    Panel,
    /// A picker opened (`Picker::kind`).
    Picker,
    /// A tool Claude called in the chat.
    Claude,
    /// A shuttle's step that landed (`shuttle.rs`): keys or text.
    Shuttle,
}

impl Area {
    pub const ALL: [Area; 10] = [
        Area::Command,
        Area::Palette,
        Area::Typed,
        Area::Plugin,
        Area::Card,
        Area::Answer,
        Area::Panel,
        Area::Picker,
        Area::Claude,
        Area::Shuttle,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Area::Command => "command",
            Area::Palette => "palette",
            Area::Typed => "typed",
            Area::Plugin => "plugin",
            Area::Card => "card",
            Area::Answer => "answer",
            Area::Panel => "panel",
            Area::Picker => "picker",
            Area::Claude => "claude",
            Area::Shuttle => "shuttle",
        }
    }

    fn parse(s: &str) -> Option<Area> {
        Area::ALL.into_iter().find(|a| a.name() == s)
    }

    pub fn by(self) -> By {
        match self {
            Area::Claude => By::Claude,
            Area::Shuttle => By::Shuttle,
            _ => By::Keys,
        }
    }
}

/// Whose a count is. `Keys`: a person's or a driver's — both reach tarae as the terminal's input, and nothing in
/// a key says which. `Shuttle`: keys a program sent over the UI socket (`shuttle.rs`) — known to be a program's,
/// counted where the socket hands them in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum By {
    Keys,
    Claude,
    Shuttle,
}

impl By {
    pub fn name(self) -> &'static str {
        match self {
            By::Keys => "keys",
            By::Claude => "claude",
            By::Shuttle => "shuttle",
        }
    }
}

/// How often, and the last day (local, days since 1970-01-01).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Count {
    pub n: u32,
    pub last: u32,
}

impl Count {
    fn bump(&mut self, day: u32) {
        self.n = self.n.saturating_add(1);
        self.last = day;
    }

    fn add(&mut self, o: Count) {
        self.n = self.n.saturating_add(o.n);
        self.last = self.last.max(o.last);
    }
}

/// The side panel's tabs, as counted.
pub const PANELS: [&str; 5] = ["files", "outline", "list", "git", "plugin"];

pub fn panel(mode: crate::sidebar::Mode) -> &'static str {
    use crate::sidebar::Mode;
    match mode {
        Mode::Files => PANELS[0],
        Mode::Outline => PANELS[1],
        Mode::List => PANELS[2],
        Mode::Git => PANELS[3],
        Mode::Plugin => PANELS[4],
    }
}

/// Every picker's kind (`Picker::of`) — `every_picker_kind_is_listed` reads them from the code.
pub const PICKERS: &[&str] = &[
    "attach",
    "blocked",
    "branches",
    "buffers",
    "changed-files",
    "code-actions",
    "commands",
    "conversations",
    "diagnostics",
    "effort",
    "files",
    "granted",
    "inbox",
    "jumps",
    "log",
    "model",
    "notes",
    "notes-stale",
    "places",
    "plugin",
    "rebase-session",
    "replace",
    "search",
    "session-base",
    "symbols",
    "tasks",
    "text-matches",
    "themes",
    "watches",
    "workspace-diagnostics",
    "workspace-symbols",
];

/// The tools Claude has in the chat: tarae's (as the stream names them, `mcp__tarae__` off) and Claude Code's
/// own the chat may have (`llm.edit-tools`). Any other name is counted as `other` — a name is the model's
/// output, never kept as it came.
pub const CLAUDE_TOOLS: &[&str] = &[
    "commit",
    "definition",
    "diagnostics",
    "edit",
    "git",
    "hover",
    "note",
    "plan",
    "procs",
    "read",
    "references",
    "rename",
    "replace",
    "run",
    "run_tests",
    "screen",
    "search",
    "symbols",
    "ui",
    "Edit",
    "Read",
    "Write",
];

/// A tool name from Claude's stream, as counted.
pub fn claude_tool(name: &str) -> &'static str {
    let name = name.strip_prefix("mcp__tarae__").unwrap_or(name);
    CLAUDE_TOOLS.iter().find(|t| **t == name).copied().unwrap_or("other")
}

/// A `:` line's command, as counted: its first name — None for a line naming no command tarae has (a plugin's
/// is the caller's to find).
pub fn typed_name(line: &str) -> Option<&'static str> {
    let word = line.split_whitespace().next()?;
    crate::cmdline::COMMANDS.iter().find(|c| c.names.contains(&word)).map(|c| c.names[0])
}

/// `commands::COMMANDS`' index of `c` — its address in the table, no search.
fn index(c: &'static StaticCommand) -> Option<usize> {
    let table = crate::commands::COMMANDS;
    let at = (c as *const StaticCommand as usize).checked_sub(table.as_ptr() as usize)?;
    let i = at / std::mem::size_of::<StaticCommand>();
    table.get(i).filter(|t| std::ptr::eq(*t, c)).map(|_| i)
}

/// A feature as the file names it: `area:name`.
type Key = (Area, Cow<'static, str>);

fn key_name((area, name): &Key) -> String {
    format!("{}:{name}", area.name())
}

/// What this tarae counted since its last save.
pub struct Counts {
    /// Commands a key ran, by their index in `commands::COMMANDS` — the key path's one add.
    keys: Vec<Count>,
    rest: HashMap<Key, Count>,
    /// Something counted since the last save.
    any: bool,
    /// Today (`day_of`) — read once a batch of events (`Editor::features_turn`).
    pub day: u32,
    /// The local time's offset from UTC, seconds — read again at each save.
    offset: i64,
    /// When the last save was asked for (seconds since 1970) — or tarae started.
    saved_s: u64,
    /// `$state/features.json` — None in tests: nothing read or written.
    pub path: Option<PathBuf>,
}

/// Days since 1970-01-01 in local time, `offset` seconds east of UTC.
pub fn day_of(secs: u64, offset: i64) -> u32 {
    (secs as i64 + offset).div_euclid(86_400).max(0) as u32
}

fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Counts {
    pub fn new(path: Option<PathBuf>) -> Counts {
        let now = std::time::SystemTime::now();
        let offset = crate::log::utc_offset(now);
        let secs = now_s();
        Counts {
            keys: vec![Count::default(); crate::commands::COMMANDS.len()],
            rest: HashMap::new(),
            any: false,
            day: day_of(secs, offset),
            offset,
            saved_s: secs,
            path,
        }
    }

    /// A command a key ran.
    #[inline]
    pub fn key(&mut self, c: &'static StaticCommand) {
        if let Some(i) = index(c) {
            self.keys[i].bump(self.day);
            self.any = true;
        }
    }

    /// `name` in `area` — a name from a table in the code.
    pub fn bump(&mut self, area: Area, name: &'static str) {
        self.bump_key((area, Cow::Borrowed(name)));
    }

    fn bump_key(&mut self, key: Key) {
        self.rest.entry(key).or_default().bump(self.day);
        self.any = true;
    }

    /// A card of kind `kind` answered `answer` (one of its letters).
    pub fn answer(&mut self, kind: &'static str, answer: char) {
        if answer.is_ascii_alphanumeric() {
            self.bump_key((Area::Answer, Cow::Owned(format!("{kind} {answer}"))));
        }
    }

    /// A plugin's command — `name` as the plugin registered it (never a line typed).
    fn plugin(&mut self, cmd: &crate::plugin::PluginCommand) {
        self.bump_key((Area::Plugin, Cow::Owned(cmd.name.clone())));
    }

    /// What's counted and not saved yet.
    pub fn pending(&self) -> Vec<(String, Count)> {
        let keys = crate::commands::COMMANDS
            .iter()
            .zip(&self.keys)
            .filter(|(_, c)| c.n > 0)
            .map(|(cmd, c)| (format!("{}:{}", Area::Command.name(), cmd.name), *c));
        keys.chain(self.rest.iter().map(|(k, c)| (key_name(k), *c))).collect()
    }

    /// What's counted, handed over to be saved — counted again from nothing.
    fn take(&mut self) -> Vec<(String, Count)> {
        let out = self.pending();
        self.keys.fill(Count::default());
        self.rest.clear();
        self.any = false;
        out
    }

    /// Forgets what's counted and not saved (counting turned off, a reset).
    pub fn clear(&mut self) {
        self.take();
    }
}

// ── The file ─────────────────────────────────────────────────────────────────

/// The file's counts: since which day, and each feature's (`area:name`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Saved {
    pub since: Option<u32>,
    pub counts: BTreeMap<String, Count>,
}

impl Saved {
    fn parse(text: &str) -> Option<Saved> {
        let v: Value = serde_json::from_str(text).ok()?;
        let counts = v["counts"].as_object()?;
        let mut out = Saved { since: v["since"].as_u64().map(|d| d as u32), counts: BTreeMap::new() };
        for (k, c) in counts {
            // (only names of a known area: a hand-edited line isn't shown as a feature)
            let known = k.split_once(':').is_some_and(|(a, n)| Area::parse(a).is_some() && !n.is_empty());
            let n = c[0].as_u64().and_then(|n| u32::try_from(n).ok());
            let last = c[1].as_u64().and_then(|n| u32::try_from(n).ok());
            if let (true, Some(n), Some(last)) = (known, n, last) {
                out.counts.insert(k.clone(), Count { n, last });
            }
        }
        Some(out)
    }

    fn to_json(&self) -> String {
        let counts: serde_json::Map<String, Value> =
            self.counts.iter().map(|(k, c)| (k.clone(), json!([c.n, c.last]))).collect();
        json!({ "v": V, "since": self.since, "counts": counts }).to_string()
    }

    fn add(&mut self, counts: &[(String, Count)], today: u32) {
        for (k, c) in counts {
            self.counts.entry(k.clone()).or_default().add(*c);
        }
        let first = counts.iter().map(|(_, c)| c.last).min().unwrap_or(today);
        self.since = Some(self.since.map_or(first, |s| s.min(first)));
    }
}

/// The lock beside the file — the file itself is replaced by each save (a new inode), so it can't hold one.
fn lock(path: &Path) -> io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt as _;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let f = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .mode(0o600)
        .open(path.with_extension("lock"))?;
    f.lock()?;
    Ok(f)
}

/// The file as it is (missing: nothing counted yet; unreadable as counts: nothing either).
pub fn read(path: &Path) -> Saved {
    std::fs::read_to_string(path).ok().and_then(|t| Saved::parse(&t)).unwrap_or_default()
}

/// Adds `counts` to the file — read, added to and written while the lock is held, so taraes saving side by side
/// each add theirs. A file that isn't counts (broken, hand-edited) is set aside as `features.json.broken`, not
/// written over. On a worker.
pub fn save(path: &Path, counts: &[(String, Count)], today: u32) -> io::Result<()> {
    if counts.is_empty() {
        return Ok(());
    }
    let _held = lock(path)?;
    let mut saved = match std::fs::read_to_string(path) {
        Ok(t) => match Saved::parse(&t) {
            Some(s) => s,
            None => {
                set_aside(path)?;
                Saved::default()
            }
        },
        Err(e) if e.kind() == io::ErrorKind::NotFound => Saved::default(),
        Err(e) => return Err(e),
    };
    saved.add(counts, today);
    let body = saved.to_json();
    crate::disk::write_private(path, |w| io::Write::write_all(w, body.as_bytes()))
}

/// A file that isn't counts, kept beside as `features.json.broken`, `.broken-2`, … — linked to the first name
/// not taken (a link never replaces one), then let go of: no copy set aside before is written over.
fn set_aside(path: &Path) -> io::Result<()> {
    for n in 1..1000 {
        let to = match n {
            1 => path.with_extension("json.broken"),
            n => path.with_extension(format!("json.broken-{n}")),
        };
        match std::fs::hard_link(path, &to) {
            Ok(()) => return std::fs::remove_file(path),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::other("999 broken copies set aside already — left as it is"))
}

/// `:features-reset`'s yes: the file gone (under the lock). On a worker.
pub fn remove(path: &Path) -> io::Result<()> {
    let _held = lock(path)?;
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

// ── What there is to count ───────────────────────────────────────────────────

/// Every feature there is, so one never used shows as 0: (area, name, the key bound to it).
#[derive(Clone, Debug, Default)]
pub struct Universe {
    pub rows: Vec<(Area, String, Option<String>)>,
}

impl Universe {
    pub fn of(ed: &Editor) -> Universe {
        let bound = ed.keymaps.bindings();
        let mut rows: Vec<(Area, String, Option<String>)> = crate::commands::COMMANDS
            .iter()
            .map(|c| (Area::Command, c.name.to_string(), bound.get(c.name).cloned()))
            .collect();
        rows.extend(crate::cmdline::COMMANDS.iter().map(|c| (Area::Typed, c.names[0].to_string(), None)));
        if let Some(p) = &ed.plugins {
            rows.extend(p.commands.iter().map(|c| (Area::Plugin, c.name.clone(), None)));
        }
        rows.extend(crate::offer::KINDS.iter().map(|k| (Area::Card, k.to_string(), None)));
        rows.extend(PANELS.iter().map(|p| (Area::Panel, p.to_string(), None)));
        rows.extend(PICKERS.iter().map(|p| (Area::Picker, p.to_string(), None)));
        rows.extend(CLAUDE_TOOLS.iter().map(|t| (Area::Claude, t.to_string(), None)));
        rows.extend(crate::shuttle::STEPS.iter().map(|s| (Area::Shuttle, s.to_string(), None)));
        Universe { rows }
    }
}

// ── The report ───────────────────────────────────────────────────────────────

/// One feature as `:features` and `tarae state --features` show it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Row {
    pub area: Option<Area>,
    pub name: String,
    /// By a key (a command), typed (a `:` command), asked (a card), called (a tool), shown, opened.
    pub uses: u32,
    /// A command's: chosen in the palette.
    pub palette: u32,
    pub last: u32,
    pub bound: Option<String>,
    /// A card's answers: letter, how often.
    pub answers: Vec<(String, u32)>,
}

impl Row {
    pub fn total(&self) -> u32 {
        self.uses.saturating_add(self.palette)
    }
}

/// What `:features` and `tarae state --features` say.
pub struct Report {
    pub on: bool,
    pub since: Option<u32>,
    pub today: u32,
    /// The file, as the report names it.
    pub file: Option<String>,
    pub rows: Vec<Row>,
}

impl Report {
    /// Everything there is (`universe`) with the file's counts (`saved`) and what this tarae hasn't saved yet
    /// (`pending`) added up.
    pub fn new(
        on: bool,
        universe: &Universe,
        saved: &Saved,
        pending: &[(String, Count)],
        today: u32,
    ) -> Report {
        let mut rows: Vec<Row> = Vec::new();
        let mut at: HashMap<(Area, String), usize> = HashMap::new();
        let mut row = |rows: &mut Vec<Row>, area: Area, name: &str| -> usize {
            *at.entry((area, name.to_string())).or_insert_with(|| {
                rows.push(Row { area: Some(area), name: name.to_string(), ..Row::default() });
                rows.len() - 1
            })
        };
        for (area, name, bound) in &universe.rows {
            let i = row(&mut rows, *area, name);
            rows[i].bound.clone_from(bound);
        }
        let mut since = saved.since;
        let counts = saved.counts.iter().map(|(k, c)| (k.as_str(), *c));
        for (k, c) in counts.chain(pending.iter().map(|(k, c)| (k.as_str(), *c))) {
            let Some((area, name)) = k.split_once(':').and_then(|(a, n)| Some((Area::parse(a)?, n))) else {
                continue;
            };
            since = Some(since.map_or(c.last, |s| s.min(c.last)));
            let (i, add): (usize, fn(&mut Row, u32)) = match area {
                Area::Palette => (row(&mut rows, Area::Command, name), |r, n| r.palette += n),
                Area::Answer => {
                    let Some((kind, letter)) = name.rsplit_once(' ') else { continue };
                    let i = row(&mut rows, Area::Card, kind);
                    let r = &mut rows[i];
                    match r.answers.iter_mut().find(|(l, _)| l == letter) {
                        Some((_, n)) => *n += c.n,
                        None => r.answers.push((letter.to_string(), c.n)),
                    }
                    r.last = r.last.max(c.last);
                    continue;
                }
                _ => (row(&mut rows, area, name), |r, n| r.uses += n),
            };
            add(&mut rows[i], c.n);
            rows[i].last = rows[i].last.max(c.last);
        }
        for r in &mut rows {
            crate::sorting::by(&mut r.answers, |a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        }
        // Most used first, then by name — never used at the end, by name
        crate::sorting::by(&mut rows, |a, b| {
            let (ka, kb) = ((a.area, std::cmp::Reverse(a.total())), (b.area, std::cmp::Reverse(b.total())));
            ka.cmp(&kb).then_with(|| a.name.cmp(&b.name))
        });
        Report { on, since, today, file: None, rows }
    }

    fn used(&self, area: Area) -> impl Iterator<Item = &Row> {
        self.rows.iter().filter(move |r| r.area == Some(area) && r.total() > 0)
    }

    fn unused(&self, area: Area) -> impl Iterator<Item = &Row> {
        self.rows.iter().filter(move |r| r.area == Some(area) && r.total() == 0)
    }

    /// `today` · `yesterday` · `3d ago` (under two weeks, `term::ago`) · `2026-09-14`.
    pub fn when(&self, day: u32) -> String {
        match self.today.saturating_sub(day) {
            0 => "today".into(),
            1 => "yesterday".into(),
            n if n < 14 => crate::term::ago(std::time::Duration::from_secs(u64::from(n) * 86_400), false),
            _ => date(day),
        }
    }

    /// `:features`' text (Markdown) — also `tarae state --features`.
    pub fn markdown(&self) -> String {
        let mut out = "# Features\n\n".to_string();
        let file = self.file.as_deref().unwrap_or("features.json in tarae's state folder");
        if !self.on {
            out += "Counting is off — nothing is counted or written. To count which features you use, put this in \
                    your own config (a project's can't turn it on):\n\n```toml\n[features]\ncount = true\n```\n\n";
            out += &format!(
                "or `:set features.count true` until you quit. The counts stay in {file}; nothing is sent.\n"
            );
            if self.rows.iter().all(|r| r.total() == 0) {
                return out;
            }
            out += "\nCounted while it was on:\n";
        } else {
            let used = self.rows.iter().filter(|r| r.total() > 0).count();
            let never = self.rows.iter().filter(|r| r.total() == 0).count();
            let uses: u64 = self.rows.iter().map(|r| r.total() as u64).sum();
            let since = self.since.map_or_else(|| "today".to_string(), date);
            out += &format!(
                "Since {since} · {} uses of {used} features · {never} never used\n\n",
                thousands(uses)
            );
            out += &format!(
                "Keys are one count — a person's typing and a driver's keys (zellij, a script) reach tarae alike, so \
                 they aren't told apart. Claude's tool calls are its own; clicks aren't counted.\n\nKept in {file}, \
                 on this machine only · `:features-reset` empties it.\n"
            );
        }
        let n = |n: u32| if n == 0 { "·".to_string() } else { thousands(n as u64) };
        let bound = |r: &Row| r.bound.clone().unwrap_or_default();
        let name = |r: &Row| match r.area {
            Some(Area::Typed | Area::Plugin) => format!(":{}", r.name),
            _ => r.name.clone(),
        };
        let mut section = |head: &[&str], rows: Vec<Vec<String>>| {
            if !rows.is_empty() {
                out += "\n";
                out += &columns("##", head, &rows);
            }
        };
        let rows = self
            .used(Area::Command)
            .map(|r| vec![r.name.clone(), n(r.uses), n(r.palette), self.when(r.last), bound(r)]);
        section(&["Commands", "by key", "palette", "last", "key"], rows.collect());
        let plain = |area| self.used(area).map(|r| vec![name(r), n(r.uses), self.when(r.last)]).collect();
        section(&["Typed commands", "uses", "last"], plain(Area::Typed));
        section(&["Plugin commands", "uses", "last"], plain(Area::Plugin));
        let rows = self.used(Area::Card).map(|r| {
            let answers: Vec<String> = r.answers.iter().map(|(l, n)| format!("{l} {n}")).collect();
            vec![r.name.clone(), n(r.uses), self.when(r.last), answers.join(" · ")]
        });
        section(&["Cards", "asked", "last", "answered"], rows.collect());
        section(&["Side panel", "shown", "last"], plain(Area::Panel));
        section(&["Pickers", "opened", "last"], plain(Area::Picker));
        section(&["Claude's tools", "calls", "last"], plain(Area::Claude));
        if !self.on {
            return out;
        }
        if self.rows.iter().any(|r| r.total() == 0) {
            out += "\n##  Never used\n";
            for (title, area) in [
                ("Commands", Area::Command),
                ("Typed commands", Area::Typed),
                ("Plugin commands", Area::Plugin),
                ("Cards", Area::Card),
                ("Side panel tabs", Area::Panel),
                ("Pickers", Area::Picker),
                ("Claude's tools", Area::Claude),
            ] {
                let rows: Vec<Vec<String>> = self.unused(area).map(|r| vec![name(r), bound(r)]).collect();
                if !rows.is_empty() {
                    let title = format!("{title} · {}", rows.len());
                    let key = if area == Area::Command { "key" } else { "" };
                    out += "\n";
                    out += &columns("###", &[&title, key], &rows);
                }
            }
        }
        out
    }

    /// `tarae state --features --json`: every feature, used or not.
    pub fn json(&self) -> Value {
        let rows: Vec<Value> = self
            .rows
            .iter()
            .map(|r| {
                let area = r.area.unwrap_or(Area::Command);
                let mut v = json!({
                    "area": area.name(),
                    "name": r.name,
                    "by": area.by().name(),
                    "uses": r.uses,
                    "last": (r.total() > 0).then(|| date(r.last)),
                });
                if area == Area::Command {
                    v["palette"] = json!(r.palette);
                    v["key"] = json!(r.bound);
                }
                // (a card is asked by Claude or tarae, answered by keys: `by` is its answers')
                if area == Area::Card {
                    let answers: serde_json::Map<String, Value> =
                        r.answers.iter().map(|(l, n)| (l.clone(), json!(n))).collect();
                    v["answers"] = Value::Object(answers);
                    if let Some(o) = v.as_object_mut() {
                        o.remove("uses");
                    }
                    v["asked"] = json!(r.uses);
                }
                v
            })
            .collect();
        json!({
            "v": V,
            "on": self.on,
            "since": self.since.map(date),
            "today": date(self.today),
            "keys": "one count: a person's keys and a driver's reach tarae alike — not told apart",
            "features": rows,
        })
    }
}

/// A section: its heading (`heading` — `##`, `###` — then `head`: the title over the first column, each other
/// column's name over it) and its rows as aligned columns, four cells in, so Markdown reads them as code (a key
/// like `[ space` isn't a link's start) right under the heading's text; three cells between; a column of counts
/// right-aligned, text left, the first column at most `NAME_MAX` wide for the others' sake. The names read as a
/// heading, never as one more row.
fn columns(heading: &str, head: &[&str], rows: &[Vec<String>]) -> String {
    use unicode_width::UnicodeWidthStr as _;
    const NAME_MAX: usize = 32;
    let count = |s: &str| s == "·" || s.chars().all(|ch| ch.is_ascii_digit() || ch == ',');
    // (a column of days — `today`, an age, a date — right-aligned too: its ages end where the widest does)
    let day = |s: &str| {
        matches!(s, "today" | "yesterday")
            || s.ends_with(" ago")
            || s.chars().all(|ch| ch.is_ascii_digit() || ch == '-')
    };
    let numeric = |c: usize| c > 0 && (rows.iter().all(|r| count(&r[c])) || rows.iter().all(|r| day(&r[c])));
    let widths: Vec<usize> = (0..head.len())
        .map(|c| rows.iter().map(|r| r[c].width()).chain([head[c].width()]).max().unwrap_or(0))
        .map(|w| w.max(1))
        .enumerate()
        .map(|(c, w)| if c == 0 { w.min(NAME_MAX) } else { w })
        .collect();
    let line = |lead: &str, cells: Vec<&str>| {
        let padded: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(c, s)| {
                let gap = " ".repeat(widths[c].saturating_sub(s.width()));
                if numeric(c) { format!("{gap}{s}") } else { format!("{s}{gap}") }
            })
            .collect();
        format!("{lead:<4}{}\n", padded.join("   ").trim_end())
    };
    // (`##  Commands`: the heading's text starts where the rows do)
    let mut out = line(&format!("{heading} "), head.to_vec());
    for r in rows {
        out += &line("", r.iter().map(String::as_str).collect());
    }
    out
}

/// `1,204`.
pub(crate) fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `2026-10-13` for a day number (`day_of`) — the civil calendar from days (Howard Hinnant's algorithm).
pub fn date(day: u32) -> String {
    let z = day as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

// ── The editor's side ────────────────────────────────────────────────────────

impl Editor {
    /// Counting is on (`features.count`).
    pub fn counting(&self) -> bool {
        self.config.features_count
    }

    /// `name` in `area`, when counting.
    pub fn count(&mut self, area: Area, name: &'static str) {
        if self.counting() {
            self.features.bump(area, name);
        }
    }

    /// A `:` line run by a key or the command line: its command, or a plugin's — a line naming neither isn't
    /// counted.
    pub fn count_typed(&mut self, line: &str) {
        if !self.counting() {
            return;
        }
        match typed_name(line) {
            Some(name) => self.features.bump(Area::Typed, name),
            None => {
                let word = line.split_whitespace().next().unwrap_or_default();
                if let Some(cmd) = self.plugin_command(word).cloned() {
                    self.features.plugin(&cmd);
                }
            }
        }
    }

    /// Once a batch of events (`term::event_loop`), counting or not: today read, and what's counted saved once
    /// it's `SAVE_EVERY_S` old — on a worker, no result. Off, what's counted is forgotten.
    pub fn features_turn(&mut self) {
        if !self.counting() {
            if self.features.any {
                self.features.clear();
            }
            return;
        }
        let secs = now_s();
        self.features.day = day_of(secs, self.features.offset);
        if self.features.any && secs.saturating_sub(self.features.saved_s) >= SAVE_EVERY_S {
            self.features_save(secs, false);
        }
    }

    fn features_save(&mut self, secs: u64, now: bool) {
        self.features.saved_s = secs;
        self.features.offset = crate::log::utc_offset(std::time::SystemTime::now());
        let counts = self.features.take();
        let (Some(path), false) = (self.features.path.clone(), counts.is_empty()) else { return };
        let today = self.features.day;
        let work = move || {
            if let Err(e) = save(&path, &counts, today) {
                crate::log::line("features", "warn", &format!("{}: {e}", path.display()));
            }
        };
        match now {
            true => work(),
            false => self.events.jobs().background(work),
        }
    }

    /// When tarae ends (`:q`, `:restart`, a signal): what's counted written now.
    pub fn features_flush(&mut self) {
        if self.counting() && self.features.any {
            self.features_save(now_s(), true);
        }
    }

    /// The report, made on a worker (the file read there) and handed to `then` on the main loop.
    fn features_report(&mut self, then: impl FnOnce(&mut Editor, Report) + Send + 'static) {
        let universe = Universe::of(self);
        let pending = if self.counting() { self.features.pending() } else { Vec::new() };
        let (on, path, today) = (self.counting(), self.features.path.clone(), self.features.day);
        self.events.jobs().spawn(move || {
            let saved = path.as_deref().map(read).unwrap_or_default();
            let mut r = Report::new(on, &universe, &saved, &pending, today);
            r.file = path.map(|p| crate::allow::tilde(&p));
            move |ed: &mut Editor| then(ed, r)
        });
    }

    /// `:features` — the report in a buffer of its own (Markdown, nothing to save).
    pub fn features_open(&mut self) {
        self.features_report(|ed, r| {
            let text = r.markdown();
            match ed.docs.iter().position(|d| d.throwaway && d.title.as_deref() == Some("features")) {
                Some(i) => {
                    crate::disk::reload_text(&mut ed.docs[i], &ropey::Rope::from_str(&text));
                    ed.current = i;
                }
                None => {
                    let id = ed.open_virtual("features", &text);
                    if let Some(spec) = crate::syntax::spec("markdown") {
                        ed.load_language(id, spec);
                    }
                    ed.session_keeps(id);
                }
            }
        });
    }

    /// `tarae state --features`: the report as JSON or text, sent to `reply` once made.
    pub fn features_answer(&mut self, json: bool, reply: std::sync::mpsc::Sender<String>) {
        self.features_report(move |ed, r| {
            let _ = reply.send(if json { r.json().to_string() } else { r.markdown() });
            ed.unchanged = true;
        });
    }

    /// `:features-reset` — a card asks first.
    pub fn features_reset(&mut self) {
        let q = "Every feature's count goes, here and in the file — counting starts again from nothing. A tarae \
                 running beside this one adds what it counted since it last saved.";
        let what = crate::offer::What::FeaturesReset;
        self.push_offer(crate::offer::Offer::new(
            "Empty the feature counts?".into(),
            q,
            "",
            String::new(),
            what,
        ));
    }

    /// The card's answer: `y` empties them.
    pub fn features_reset_answer(&mut self, card: u64, yes: bool) {
        self.offer_finished(card, Ok(()));
        if !yes {
            return;
        }
        self.features.clear();
        if let Some(path) = self.features.path.clone() {
            self.events.jobs().background(move || {
                if let Err(e) = remove(&path) {
                    crate::log::line("features", "warn", &format!("{}: {e}", path.display()));
                }
            });
        }
        self.set_success("Feature counts emptied");
    }
}

#[cfg(test)]
mod tests;
