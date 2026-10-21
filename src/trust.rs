//! Project trust. A project's `.tarae.toml` is the project's file — often a cloned repository's, written by
//! someone else. What in it runs commands or loosens how Claude's commands are guarded (`RISKY` settings,
//! `[task.<name>]`, a `[lsp.<name>]` command or args, an `[[attach]]` `before`, a key bound to a `:` command)
//! is held back until the user trusts the project on a card showing each of them as written; the rest of the
//! file applies as is. Each answer is kept (`$state/trust.json`) for the version of the file it was about (a
//! SHA-256 of the held-back part — `hash`), so a version never answered asks — and for the repository: its checkouts
//! (sessions) share every version answered, whichever each has (#185: sessions on different versions asked
//! again in turn). `:project-trust` shows the card again, `:project-untrust` takes it back.
//!
//! `config::load` applies the project layer both ways (`Project.with` / `without`), so answering switches
//! between them without reading anything.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value as Json, json};

use crate::config::EditorConfig;
use crate::keymap::Keymaps;

/// Settings a project file sets only once trusted: they run commands, or change what Claude's commands do
/// unasked (`llm.sandbox*` comes with the sandbox), or what Claude sees.
pub const RISKY: [&str; 23] = [
    // (a project turning on the language servers the user turned off — rust-analyzer runs its build scripts)
    "editor.lsp",
    "editor.restart-build",
    "editor.restart-binary",
    "llm.command",
    "llm.args",
    "llm.run-allow",
    "llm.review-ask",
    // (Claude's edits go in unseen, and the checks run them)
    "llm.review-at-end",
    "llm.chat-settings",
    "llm.sandbox",
    "llm.sandbox-network",
    // (a project's list takes the user's place — it could name more)
    "llm.sandbox-hosts",
    // (where the runs' traffic goes: around the user's own proxy, or through it)
    "llm.sandbox-upstream",
    "llm.sandbox-write",
    "llm.sandbox-protect",
    "llm.sandbox-ports",
    "llm.sandbox-localhost",
    // (a project passing `AWS_*` to every command)
    "llm.sandbox-env",
    // (a project turning it back on when the user turned it off)
    "llm.screen-terminal",
    // (where a checkout of the project's files is written — a launch agents folder would run them)
    "session.worktrees",
    // (a command run outside the sandbox at every session's end)
    "session.ended",
    // (files moved into the main checkout at every session's end)
    "session.keep",
    // (what a code review holds for a person's look before a bring — `risk.rs`)
    "review.risky",
];

/// Settings only your own config sets — refused from a project's `.tarae.toml` and `.tarae.local.toml` alike,
/// trusted or not (said): they decide whose credentials Claude uses and who pays, what Claude's chat edits
/// with (`llm.edit-tools`), whether what you use is counted (`features.count`), whether your cards and your
/// time on reviewed changes are kept (`llm.card-record`, `review.record`), whether pings spend your usage to keep
/// a conversation warm (`llm.keep-warm*`), what the board's Claudes are asked (`review.angles`) and how many of
/// them run on your credential and memory (`review.skeptics-max`, `review.board-at-once`) — a project never does.
pub const YOURS_ONLY: [&str; 13] = [
    "llm.card-record",
    "review.record",
    "llm.keep-warm",
    "llm.keep-warm-min-tokens",
    "llm.keep-warm-max",
    "llm.environment-key",
    "llm.billing",
    "llm.reviewer-billing",
    "llm.edit-tools",
    "features.count",
    // (what the board's Claudes are asked: a project's angle could talk its own reviewer into anything)
    "review.angles",
    // (how many of them: each is billed on your reviewer credential, ~200 MB while it reads)
    "review.skeptics-max",
    "review.board-at-once",
];

pub fn yours_only(setting: &str) -> bool {
    YOURS_ONLY.contains(&setting)
}

/// `:` commands only your own keys run (or you, typed): they switch who pays for Claude, or read a file you name
/// (`session-apply`'s patch). A project's key binding to one is left out (`config.rs`).
pub const YOURS_ONLY_COMMANDS: [&str; 2] = ["session-billing", "session-apply"];

/// Whether the `:` line `line` (no `:`) runs one of `YOURS_ONLY_COMMANDS`.
pub fn yours_only_command(line: &str) -> bool {
    let cmd = line.split_whitespace().next().unwrap_or_default();
    YOURS_ONLY_COMMANDS.contains(&cmd)
}

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
    /// Where its answers are kept (`place_of`, worked out as it's read).
    pub place: Place,
    pub hash: Digest,
    /// (key, value as written in TOML — marks that draw as something else spelled out)
    pub items: Vec<(String, String)>,
    pub state: State,
    pub with: Box<(EditorConfig, Keymaps)>,
    pub without: Box<(EditorConfig, Keymaps)>,
    /// The last trust answer of this tarae's whose write had gone when the store was read for it: a reload read
    /// before an answer given since, or while its write waits, doesn't put the earlier state back
    /// (`Editor::reload_config`).
    pub read_at: u64,
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

/// `visible` on one line, for text a card shows as a line of its own (a key, a value, a file's name): a line
/// break shown as `¶` — one more line could pass for a line of the card's own — a tab and the Unicode line
/// and paragraph separators spelled out too, and a `¶` of the text's own, so none reads as the other.
pub fn visible_line(s: &str) -> String {
    let spelled = |c: char| match c {
        '\n' => "¶".to_string(),
        '\t' | '¶' | '\u{2028}' | '\u{2029}' => format!("\\u{{{:x}}}", c as u32),
        c => c.to_string(),
    };
    visible(&s.chars().map(spelled).collect::<String>())
}

/// What an answer is about: a digest of the held-back items.
pub type Digest = [u8; 32];

/// The held-back items' SHA-256, in order, each key and value preceded by its length (so no two lists read
/// alike). A trust decision, not only "did it change": FNV-1a, before, could be matched by a crafted file
/// in a checkout of the repository — a command trusted with no card.
pub fn hash(items: &[(String, String)]) -> Digest {
    let mut bytes = Vec::new();
    for field in items.iter().flat_map(|(k, v)| [k, v]) {
        bytes.extend_from_slice(&(field.len() as u64).to_le_bytes());
        bytes.extend_from_slice(field.as_bytes());
    }
    crate::sha256::digest(&bytes)
}

/// A digest in hex, as kept.
fn hex(d: &Digest) -> String {
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// A digest kept in hex — None for anything else (an FNV-1a hash kept before: 16 digits).
fn unhex(s: &str) -> Option<Digest> {
    let mut d = [0u8; 32];
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    for (i, b) in d.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).ok()?;
    }
    Some(d)
}

/// Versions of one file answered at most (the oldest answer goes first).
const VERSIONS_KEPT: usize = 64;

/// What a kept answer is about — `:granted` lists them by it (`granted.rs`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// A project's `.tarae.toml` (this file's).
    Tarae,
    /// A repository's `.claude/settings*.json` (`claudetrust.rs`).
    Claude,
    /// A repository's own git commands, by its config file (`gittrust.rs`).
    Git,
    /// A hook in a hooks folder inside the checkout, by its content (`hooks.rs`).
    Hook,
}

impl Kind {
    const WORDS: [(Kind, &str); 4] =
        [(Kind::Tarae, "tarae"), (Kind::Claude, "claude"), (Kind::Git, "git"), (Kind::Hook, "hook")];

    fn word(self) -> &'static str {
        Kind::WORDS.iter().find(|(k, _)| *k == self).map_or("", |(_, w)| w)
    }

    fn of_word(w: &str) -> Option<Kind> {
        Kind::WORDS.iter().find(|(_, x)| *x == w).map(|(k, _)| *k)
    }
}

/// Where answers are kept: a repository's file — git's common folder, which its checkouts (sessions) share,
/// and the file's path in a checkout — or, outside a repository, the file itself (real path).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Place {
    File(PathBuf),
    Repo { common: PathBuf, file: PathBuf },
}

/// Where `file`'s answers are kept. A checkout counts as the repository's only when git's links go both
/// ways (`allow::common_dir`): a `.git` file naming another repository's folder doesn't get its answers.
pub fn place_of(file: &Path) -> Place {
    // (its deepest existing folder resolved: one answered before its folder was made — or seen through
    // another name of it, `/tmp` for `/private/tmp` — is kept and found under one name)
    let real = crate::paths::real(file).unwrap_or_else(|| file.to_path_buf());
    let checkout = real.parent().and_then(|d| d.ancestors().find(|a| a.join(".git").exists()));
    let repo = checkout.and_then(|c| Some((crate::allow::common_dir(c)?, real.strip_prefix(c).ok()?)));
    match repo {
        Some((common, rel)) => Place::Repo { common, file: rel.to_path_buf() },
        None => Place::File(real),
    }
}

impl Place {
    /// As `:granted` shows it: a repository's file from its checkout's top, else from `root` when it's there (or
    /// whole, `~` for home).
    pub fn shown(&self, root: &Path) -> String {
        match self {
            Place::Repo { file, .. } => file.display().to_string(),
            Place::File(f) => match f.strip_prefix(root) {
                Ok(rel) if !root.as_os_str().is_empty() => rel.display().to_string(),
                _ => crate::allow::tilde(f),
            },
        }
    }

    /// Whether it's `common`'s (a repository's git common folder), or — outside a repository — under `root`.
    fn in_here(&self, common: Option<&Path>, root: &Path) -> bool {
        match self {
            Place::Repo { common: c, .. } => Some(c.as_path()) == common,
            Place::File(f) => common.is_none() && f.starts_with(root),
        }
    }
}

/// One place's answers: what they're about, each version's (digest, trusted, when — seconds since the epoch),
/// the oldest first, its counter (`storewrite::Rev`, bumped by every write there) and when a take-back left it
/// with no version (seconds; the place stays, its counter with it — `TAKEN_KEPT`).
#[derive(Clone, Debug, PartialEq)]
struct Kept {
    kind: Kind,
    versions: Vec<(Digest, bool, u64)>,
    rev: crate::storewrite::Rev,
    taken_s: u64,
}

impl Kept {
    fn new(kind: Kind) -> Kept {
        Kept { kind, versions: Vec::new(), rev: crate::storewrite::Rev::default(), taken_s: 0 }
    }
}

/// How long a place taken back with no version left is kept: a yes given before it, in a tarae that hadn't seen
/// it, may land that late at most (one quit and start drops what's not written).
const TAKEN_KEPT: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 3600);

/// A place's key in `storewrite`'s memory of what this tarae saw and wrote.
pub(crate) fn key(place: &Place) -> String {
    format!("{place:?}")
}

/// A place with a version trusted, as `:granted` lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trusted {
    pub kind: Kind,
    pub place: Place,
    /// When the newest trusted version was answered.
    pub at: u64,
    /// How many of its versions are trusted.
    pub versions: usize,
}

/// The answers given, each version of a project file's (by its hash) — `$state/trust.json`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Store {
    answers: BTreeMap<Place, Kept>,
    /// `storewrite::last()` before it was read: an answer or take-back given after it may be missing
    /// (`Project.read_at`, `storewrite::since`).
    pub read_at: u64,
    /// The places being taken back as it was read (before): a mark that ended since ended at a write after the
    /// read — what was read still trusts it (`state`, `trusted_in`).
    taking: Vec<Place>,
}

impl Store {
    pub fn path() -> Option<PathBuf> {
        crate::config::state_dir().map(|d| d.join("trust.json"))
    }

    /// Read from `path` (missing or unreadable = nothing answered) — what it narrows is what this tarae has seen
    /// (`storewrite::saw`). An entry without its kind or a version without its time or stamp isn't read — that
    /// version asks again.
    pub fn read(path: Option<&Path>) -> Store {
        let read_at = crate::storewrite::last();
        let taking =
            TAKING.lock().unwrap_or_else(|e| e.into_inner()).iter().map(|(p, _)| p.clone()).collect();
        let Some(path) = path else { return Store { read_at, taking, ..Store::default() } };
        let store = Store { read_at, taking, ..Store::read_strict(path).unwrap_or_default() };
        store.seen(path);
        store
    }

    /// Each place's counter, told to `storewrite` as seen at `path`.
    fn seen(&self, path: &Path) {
        crate::storewrite::saw(path, self.answers.iter().map(|(p, k)| (key(p), k.rev)));
    }

    /// Whether `place` reads as being taken back: marked now, or as it was read.
    fn is_taking(&self, place: &Place) -> bool {
        self.taking.contains(place) || is_taking(place)
    }

    /// `read`, but a file that's there and can't be read or parsed is an error — a write never takes it for
    /// nothing answered and writes that over it (every no and take-back lost).
    fn read_strict(path: &Path) -> Result<Store, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Store::default()),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        };
        let json = serde_json::from_str::<Json>(&text).map_err(|e| format!("{}: {e}", path.display()))?;
        let mut store = Store::default();
        let digest = |h: &Json| h.as_str().and_then(unhex);
        for a in json["answers"].as_array().into_iter().flatten() {
            let Some(kind) = a["kind"].as_str().and_then(Kind::of_word) else { continue };
            let file = a["file"].as_str().map(PathBuf::from);
            let place = match (a["repository"].as_str(), file) {
                (Some(common), Some(file)) => Place::Repo { common: PathBuf::from(common), file },
                (None, Some(file)) => Place::File(file),
                _ => continue,
            };
            // (no counter: not read — it asks again)
            let Some(rev) = crate::storewrite::Rev::of(&a["rev"]) else { continue };
            let versions = a["versions"].as_array().into_iter().flatten();
            let versions: Vec<(Digest, bool, u64)> =
                versions.filter_map(|v| Some((digest(&v[0])?, v[1].as_bool()?, v[2].as_u64()?))).collect();
            let taken_s = a["taken"].as_u64().unwrap_or(0);
            store.answers.insert(place, Kept { kind, versions, rev, taken_s });
        }
        Ok(store)
    }

    /// The answer for `file` as it is now (`hash`) — Ask when this version was never answered, or its trust is
    /// being taken back (`taking`).
    pub fn state(&self, file: &Path, hash: Digest) -> State {
        let place = place_of(file);
        let found = self.answers.get(&place).and_then(|k| k.versions.iter().rfind(|a| a.0 == hash));
        match found {
            Some((_, true, ..)) if self.is_taking(&place) => State::Ask,
            Some((_, true, ..)) => State::Trusted,
            Some((_, false, ..)) => State::Ignored,
            None => State::Ask,
        }
    }

    /// `file`'s version `hash`, a `kind` of answer, answered at `at` (again: the new answer) — as a test's store
    /// in memory has it.
    #[cfg(test)]
    pub fn answer(&mut self, kind: Kind, file: &Path, hash: Digest, trusted: bool, at: u64) {
        self.answer_at(kind, &place_of(file), hash, trusted, at);
    }

    /// `place`'s version `hash` answered at `at`.
    fn answer_at(&mut self, kind: Kind, place: &Place, hash: Digest, trusted: bool, at: u64) -> &mut Kept {
        let k = self.answers.entry(place.clone()).or_insert_with(|| Kept::new(kind));
        k.kind = kind;
        k.versions.retain(|a| a.0 != hash);
        k.versions.push((hash, trusted, at));
        let over = k.versions.len().saturating_sub(VERSIONS_KEPT);
        k.versions.drain(..over);
        k
    }

    /// `place`'s trusted versions forgotten (a take-back) — each asks again; one held back stays so, and the place
    /// with its counter.
    fn forget_at(&mut self, kind: Kind, place: &Place) -> &mut Kept {
        let k = self.answers.entry(place.clone()).or_insert_with(|| Kept::new(kind));
        k.versions.retain(|a| !a.1);
        if k.versions.is_empty() {
            k.taken_s = crate::allow::now_s();
        }
        k
    }

    /// `place`'s change by a write of `path`'s store (under its lock): its counter bumped — the change returned.
    fn bump(&mut self, path: &Path, place: &Place) -> crate::storewrite::Rev {
        let Some(k) = self.answers.get_mut(place) else { return crate::storewrite::Rev::default() };
        k.rev = crate::storewrite::next(path, &key(place), k.rev);
        k.rev
    }

    /// Places taken back with no version left, older than `TAKEN_KEPT`, dropped (their counter with them: a yes
    /// that had seen it asks again — `storewrite::holds`).
    fn prune(&mut self) {
        let floor = crate::allow::now_s().saturating_sub(TAKEN_KEPT.as_secs());
        self.answers.retain(|_, k| !k.versions.is_empty() || k.taken_s >= floor);
    }

    /// The places with a version trusted — the repository's whose git common folder is `common`, or (none:
    /// outside one) under `root` — by kind, then the newest answer first.
    pub fn trusted_in(&self, common: Option<&Path>, root: &Path) -> Vec<Trusted> {
        let mut out: Vec<Trusted> = self
            .answers
            .iter()
            .filter(|(p, _)| p.in_here(common, root) && !self.is_taking(p))
            .filter_map(|(place, k)| {
                let trusted = k.versions.iter().filter(|v| v.1);
                let at = trusted.clone().map(|v| v.2).max()?;
                Some(Trusted { kind: k.kind, place: place.clone(), at, versions: trusted.count() })
            })
            .collect();
        crate::sorting::by(&mut out, |a, b| a.kind.cmp(&b.kind).then(b.at.cmp(&a.at)));
        out
    }

    pub fn to_json(&self) -> String {
        let answers: Vec<Json> = self
            .answers
            .iter()
            .map(|(place, k)| {
                let versions: Vec<Json> =
                    k.versions.iter().map(|(h, t, at)| json!([hex(h), t, at])).collect();
                let mut e = place_json(place);
                e["kind"] = json!(k.kind.word());
                e["versions"] = json!(versions);
                e["rev"] = k.rev.json();
                if k.taken_s > 0 {
                    e["taken"] = json!(k.taken_s);
                }
                e
            })
            .collect();
        serde_json::to_string_pretty(&json!({ "answers": answers })).unwrap_or_default()
    }
}

/// A place as kept (`trust.json`'s entries, `:restart`'s state).
fn place_json(p: &Place) -> Json {
    match p {
        Place::File(f) => json!({ "file": f }),
        Place::Repo { common, file } => json!({ "repository": common, "file": file }),
    }
}

fn place_from(p: &Json) -> Option<Place> {
    let file = p["file"].as_str().map(PathBuf::from)?;
    Some(match p["repository"].as_str() {
        Some(common) => Place::Repo { common: PathBuf::from(common), file },
        None => Place::File(file),
    })
}

/// Tests: an answer of `kind` given now, written (`write_now`).
#[cfg(test)]
pub fn record(path: PathBuf, kind: Kind, file: PathBuf, hash: Digest, trusted: bool) -> Result<(), String> {
    let seen = crate::storewrite::seen_now(&path);
    let w = Write::Answer { kind, place: None, file, hash, trusted, given: given(), seen };
    write_now(&path, &w, false).map(|_| ())
}

pub use crate::storewrite::given;

/// A trust answer or take-back, as the store's writer takes it (`queue`) and `:restart` carries it.
#[derive(Clone, Debug)]
pub enum Write {
    /// `file`'s version `hash` answered, given at `given` — at `place` when it was worked out where it was given
    /// (else once, before the store's lock); a yes with the changes this tarae had seen then (`seen`).
    Answer {
        kind: Kind,
        place: Option<Place>,
        file: PathBuf,
        hash: Digest,
        trusted: bool,
        given: u64,
        seen: BTreeMap<String, crate::storewrite::Rev>,
    },
    /// What's trusted at `place` taken back, given at `given` (`:granted`'s `d`).
    Forget { kind: Kind, place: Place, given: u64 },
}

/// How a write went: a yes after a change another tarae made there unseen isn't written (`Skipped`) — its card
/// asks again; one a take-back of this tarae's given after it will take back isn't either (`Superseded`, said by
/// that take-back).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Wrote {
    Written,
    Skipped,
    Superseded,
}

impl Write {
    /// As a quit that gave up waiting names it.
    fn said(&self) -> String {
        match self {
            Write::Answer { file, trusted: true, .. } => format!("trust kept for {}", file.display()),
            Write::Answer { file, .. } => format!("trust held back for {}", file.display()),
            Write::Forget { place, .. } => format!("trust taken back for {}", place.shown(Path::new(""))),
        }
    }

    /// What it is, as `:restart` carries it (a no or a take-back: `narrows` — a yes not written asks again).
    fn carried(&self) -> Json {
        match self {
            Write::Answer { kind, place, file, hash, given, trusted, .. } => json!({
                (if *trusted { "yes" } else { "no" }): kind.word(), "place": place.as_ref().map(place_json),
                "file": file, "hash": hex(hash), "given": given,
            }),
            Write::Forget { kind, place, given } => {
                json!({ "forget": kind.word(), "place": place_json(place), "given": given })
            }
        }
    }

    fn narrows(&self) -> bool {
        !matches!(self, Write::Answer { trusted: true, .. })
    }

    /// Its place's key, when it's known where it's given.
    fn key(&self) -> Option<String> {
        match self {
            Write::Answer { place, .. } => place.as_ref().map(key),
            Write::Forget { place, .. } => Some(key(place)),
        }
    }

    /// What it narrows, as `storewrite::Pending.over` has it: a take-back its place, a no its version there —
    /// nothing while its place isn't known.
    fn over(&self) -> Option<String> {
        match self {
            Write::Answer { trusted: true, .. } => None,
            Write::Answer { place, hash, .. } => place.as_ref().map(|p| part(p, hash)),
            Write::Forget { place, .. } => Some(key(place)),
        }
    }

    fn of_carried(v: &Json) -> Option<Write> {
        let given = v["given"].as_u64()?;
        if let Some(kind) = v["forget"].as_str().and_then(Kind::of_word) {
            return Some(Write::Forget { kind, place: place_from(&v["place"])?, given });
        }
        let kind = v["no"].as_str().and_then(Kind::of_word)?;
        let (place, file) = (place_from(&v["place"]), PathBuf::from(v["file"].as_str()?));
        let hash = v["hash"].as_str().and_then(unhex)?;
        Some(Write::Answer { kind, place, file, hash, trusted: false, given, seen: BTreeMap::new() })
    }
}

/// A version's part of `place`, as `storewrite::Pending.over` has it.
fn part(place: &Place, hash: &Digest) -> String {
    format!("{}\0{}", key(place), hex(hash))
}

/// `writes` queued at `path`'s store in the order given (`storewrite`), written on a worker one after another
/// (the first that fails ends them), and `then` on the main loop with how they went. A quit or `:restart`'s exec
/// waits for them; a take-back or a no not written by then `:restart` gives again.
pub fn queue(
    ed: &mut crate::editor::Editor,
    path: PathBuf,
    writes: Vec<Write>,
    then: impl FnOnce(&mut crate::editor::Editor, Result<Vec<Wrote>, String>) + Send + 'static,
) {
    let said = writes.iter().map(Write::said).collect::<Vec<_>>().join("; ");
    let again = writes.iter().any(Write::narrows);
    // (what `:restart` carries — what narrows, else all —, and what it narrows)
    let carry = move |writes: &[Write]| {
        Json::Array(writes.iter().filter(|w| !again || w.narrows()).map(Write::carried).collect())
    };
    let over = |writes: &[Write]| writes.iter().filter_map(Write::over).collect();
    let ticket = crate::storewrite::ticket(&path, said, carry(&writes), again, over(&writes));
    for w in &writes {
        let (Write::Answer { given, .. } | Write::Forget { given, .. }) = w;
        if let Some(k) = w.key() {
            crate::storewrite::asked(&ticket, &k, *given, matches!(w, Write::Forget { .. }));
        }
    }
    ed.trust_writing += 1;
    ed.events.jobs().spawn_kept(move || {
        // (a place worked out at the write — git's, Claude's settings', a hook's: here, before its turn, so what it
        // narrows is its own and no write at another place covers it; `:restart` carries it placed — N2, N8)
        let mut writes = writes;
        if writes.iter().any(|w| w.key().is_none()) {
            for w in &mut writes {
                if let Write::Answer { place: p @ None, file, .. } = w {
                    *p = Some(place_of(file));
                }
            }
            crate::storewrite::placed(&ticket, carry(&writes), over(&writes));
        }
        let r = crate::storewrite::turn(ticket).map(|turn| {
            let mut wrote = Vec::new();
            let r = (writes.iter().map(|w| write_over(&path, w, turn.fails, &mut wrote)))
                .collect::<Result<Vec<_>, _>>();
            turn.done(r.is_ok(), wrote);
            r
        });
        move |ed: &mut crate::editor::Editor| {
            // (none: cut off — a test's process that ended)
            if let Some(r) = r {
                ed.trust_writing -= 1;
                then(ed, r)
            }
        }
    });
}

/// `queue`, a failure said as `failed` says it.
pub fn queue_said(ed: &mut crate::editor::Editor, path: PathBuf, writes: Vec<Write>, failed: &'static str) {
    queue(ed, path, writes, move |ed, r| {
        if let Err(e) = r {
            ed.set_error(format!("couldn't keep the answer: {e}{failed}"));
        }
    });
}

/// `:restart`'s carried writes queued again (before any trust read).
pub fn queue_again(ed: &mut crate::editor::Editor, carried: &Json) {
    let Some(path) = ed.trust_path.clone() else { return };
    for item in carried.as_array().into_iter().flatten() {
        let writes: Vec<Write> =
            item.as_array().into_iter().flatten().filter_map(Write::of_carried).collect();
        if !writes.is_empty() {
            queue_said(ed, path.clone(), writes, " — held back until you quit");
        }
    }
}

/// One write, its turn taken (`fails`: a test's failing write), under the store's lock. Every write bumps its
/// place's counter; a no or a take-back is always written. A yes is a compare-and-set: written only when every
/// change there since the one it had seen is this tarae's (`storewrite::holds`) — else another tarae's came
/// first, unseen. A yes after this tarae's own take-backs there whose writes failed is written with them (they're
/// written whatever the yes does, and end); one with a take-back of this tarae's given after it isn't written
/// true at all — that take-back follows in the queue. A take-back or a no ends its own mark, never another's.
/// What it wrote over goes to `over` (`storewrite::Pending.over`'s form): a take-back — its own, or failed ones a
/// yes took along — the place, an answer written its version; a yes not written nothing of its own.
fn write_over(path: &Path, w: &Write, fails: bool, over: &mut Vec<String>) -> Result<Wrote, String> {
    if fails {
        return Err("the store's write failed (a test's)".into());
    }
    let mut wrote = Wrote::Written;
    // (the marks it ends, and the changes it makes)
    let mut ended: Vec<(Place, u64)> = Vec::new();
    let mut made: Vec<crate::storewrite::Rev> = Vec::new();
    let mut forgot = false;
    let (kind, place) = match w {
        Write::Answer { kind, place, file, hash, trusted, given, seen } => {
            // (worked out once: the mark and the entry are the same place — N10)
            let place = place.clone().unwrap_or_else(|| place_of(file));
            let k = key(&place);
            let at = crate::allow::now_s();
            change(path, |s| {
                if !*trusted {
                    s.answer_at(*kind, &place, *hash, false, at);
                    made.push(s.bump(path, &place));
                    ended.push((place.clone(), *given));
                    return;
                }
                // (a take-back given after it, behind it in the queue: another tarae never reads it trusted)
                if is_taking_after(&place, *given) {
                    wrote = Wrote::Superseded;
                    return;
                }
                // (its own take-backs before it whose writes failed: written with it)
                let failed = taking_below(&place, *given);
                if !failed.is_empty() {
                    s.forget_at(*kind, &place);
                    made.push(s.bump(path, &place));
                    (ended, forgot) = (failed, true);
                }
                let now = s.answers.get(&place).map_or_else(Default::default, |e| e.rev);
                let saw = seen.get(&k).copied().unwrap_or_default();
                if !crate::storewrite::holds(path, &k, saw, now, &made) {
                    // (the same yes another tarae gave: this version trusted there now — what it would write; a
                    // take-back or a no since left it otherwise)
                    let trusted_there =
                        s.answers.get(&place).and_then(|e| e.versions.iter().rfind(|a| a.0 == *hash));
                    if !trusted_there.is_some_and(|a| a.1) {
                        wrote = Wrote::Skipped;
                    }
                    return;
                }
                s.answer_at(*kind, &place, *hash, true, at);
                made.push(s.bump(path, &place));
            })?;
            if forgot {
                over.push(k);
            }
            if !*trusted || wrote == Wrote::Written {
                over.push(part(&place, hash));
            }
            (*kind, place)
        }
        Write::Forget { kind, place, given } => {
            change(path, |s| {
                s.forget_at(*kind, place);
                made.push(s.bump(path, place));
            })?;
            over.push(key(place));
            // (every trusted version gone: the take-backs of the place before it are done too — a failed one's)
            ended = taking_below(place, *given);
            ended.push((place.clone(), *given));
            forgot = true;
            (*kind, place.clone())
        }
    };
    for r in made {
        crate::storewrite::wrote(path, &key(&place), r);
    }
    if kind == Kind::Git {
        // (what this tarae answered since it started, forgotten before the mark ends — a resolve between can't
        // find it trusted; and what's kept resolved is resolved again: held, or no longer — F6, N6)
        if forgot {
            crate::gitrepo::forget_trust(&place);
        }
        crate::gitrepo::trust_changed();
    }
    for (p, id) in ended {
        taken(&p, id);
    }
    Ok(wrote)
}

/// The take-backs of trust under way: a place each, with its take-back's given — until that write lands, or never
/// (it failed: until you quit; `:restart` carries them), or a yes given after it is written with it. Their
/// trusted versions read as never answered (`Store::state`, `trusted_in`), so what's taken back is held back
/// before the store's write, and stays so when that fails.
static TAKING: std::sync::Mutex<Vec<(Place, u64)>> = std::sync::Mutex::new(Vec::new());

/// `place`'s trust is being taken back by the one given at `id`: read as never answered from now on.
pub fn taking(place: Place, id: u64) {
    TAKING.lock().unwrap_or_else(|e| e.into_inner()).push((place, id));
}

/// The take-back of `place` given at `id` is over: its store write landed (or a yes's after it, with it).
pub fn taken(place: &Place, id: u64) {
    TAKING.lock().unwrap_or_else(|e| e.into_inner()).retain(|(p, i)| p != place || *i != id);
}

/// The take-backs of `place` given before `given`.
fn taking_below(place: &Place, given: u64) -> Vec<(Place, u64)> {
    let t = TAKING.lock().unwrap_or_else(|e| e.into_inner());
    t.iter().filter(|(p, i)| p == place && *i < given).cloned().collect()
}

/// Whether a take-back of `place` given after `given` is under way.
fn is_taking_after(place: &Place, given: u64) -> bool {
    TAKING.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|(p, i)| p == place && *i > given)
}

/// Tests: a new process — every take-back of `place` marked here forgotten.
#[cfg(test)]
pub fn untake_all(place: &Place) {
    TAKING.lock().unwrap_or_else(|e| e.into_inner()).retain(|(p, _)| p != place);
}

pub(crate) fn is_taking(place: &Place) -> bool {
    TAKING.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|(p, _)| p == place)
}

impl crate::editor::Editor {
    /// `taking(place, id)`, this tarae's: `:restart` carries it while it lasts (`taking_now`).
    pub(crate) fn trust_taking_start(&mut self, place: Place, id: u64) {
        taking(place.clone(), id);
        self.trust_taking.retain(|(p, i)| is_marked(p, *i));
        self.trust_taking.push((place, id));
    }

    /// A take-back of `place` given now, marked (`trust_taking_start`) — its given, taken with the mark: a read
    /// between them never finds the one without the other (`Store::read`, N5).
    pub(crate) fn trust_taking_given(&mut self, place: Place) -> u64 {
        let id = {
            let mut t = TAKING.lock().unwrap_or_else(|e| e.into_inner());
            let id = given();
            t.push((place.clone(), id));
            id
        };
        self.trust_taking.retain(|(p, i)| is_marked(p, *i));
        self.trust_taking.push((place, id));
        id
    }
}

fn is_marked(place: &Place, id: u64) -> bool {
    TAKING.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|(p, i)| p == place && *i == id)
}

/// The take-backs `ed` has under way, as `:restart`'s state carries them — its own only: the process's marks are
/// every editor's (the tests' too).
pub fn taking_now(ed: &crate::editor::Editor) -> Json {
    let mut v: Vec<(Place, u64)> =
        ed.trust_taking.iter().filter(|(p, i)| is_marked(p, *i)).cloned().collect();
    crate::sorting::sort(&mut v);
    v.dedup();
    let json = |(p, id): &(Place, u64)| {
        let mut j = place_json(p);
        j["id"] = json!(id);
        j
    };
    Json::Array(v.iter().map(json).collect())
}

/// `taking_now`'s take-backs marked again, after `:restart` (their store still trusts them, or their write was
/// cut off: `queue_again` gives it again).
pub fn taking_again(ed: &mut crate::editor::Editor, v: &Json) {
    for p in v.as_array().into_iter().flatten() {
        if let (Some(place), Some(id)) = (place_from(p), p["id"].as_u64()) {
            ed.trust_taking_start(place, id);
        }
    }
}

/// `:restart`'s state `v`: what it carries that narrows marked again before the first trust read (the config's,
/// before the editor is made) — the take-backs, and each no not written yet: until its write lands its place reads
/// as never answered, not as the store still has it.
pub fn mark_carried(v: &Json) {
    for p in v["trust_taking"].as_array().into_iter().flatten() {
        if let (Some(place), Some(id)) = (place_from(p), p["id"].as_u64()) {
            taking(place, id);
        }
    }
    let writes = v["store_writes"]["trust"].as_array().into_iter().flatten();
    for w in writes.flat_map(|item| item.as_array().into_iter().flatten()) {
        // (one carried before its worker placed it — git's, Claude's settings': worked out here, before the first
        // read — N1, N7)
        let file = || w["file"].as_str().map(|f| place_of(Path::new(f)));
        if let (Some(_), Some(place), Some(given)) =
            (w["no"].as_str(), place_from(&w["place"]).or_else(file), w["given"].as_u64())
        {
            taking(place, given);
        }
    }
}

/// Tests: `w` written now (`write_over`).
#[cfg(test)]
pub fn write_now(path: &Path, w: &Write, fails: bool) -> Result<Wrote, String> {
    write_over(path, w, fails, &mut Vec::new())
}

/// Tests: takes back what's trusted at `place`, a `kind` of answer, given at `given` — written now.
#[cfg(test)]
pub fn forget(path: &Path, kind: Kind, place: &Place, given: u64) -> Result<(), String> {
    write_now(path, &Write::Forget { kind, place: place.clone(), given }, false).map(|_| ())
}

/// The store changed by `f` (read, change, write; the file is tiny) — under the lock beside it: two answers
/// kept side by side (two cards answered at once, two taraes) each read what the other hadn't written yet, and
/// the later write lost the other's (`a_change_answered_before_restarts_claude_without_a_card` on Linux: the
/// card's `n` and the test's own answer). One it can't read isn't written over (`Store::read_strict`).
fn change(path: &Path, f: impl FnOnce(&mut Store)) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    // (beside it: the file itself is replaced by each write, a new inode, so it can't hold one)
    let lock = {
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut o = std::fs::OpenOptions::new();
        o.create(true).truncate(false).write(true).mode(0o600);
        o.open(path.with_extension("lock")).map_err(|e| e.to_string())?
    };
    lock.lock().map_err(|e| e.to_string())?;
    let mut store = Store::read_strict(path)?;
    store.seen(path);
    f(&mut store);
    store.prune();
    let body = store.to_json();
    crate::disk::write_atomic(path, |w| std::io::Write::write_all(w, body.as_bytes()))
        .map_err(|e| e.to_string())
}

/// Tests: another tarae's write at `path` — a change of its own at the place (nothing of it this tarae's), a yes
/// it gives having seen what's there now.
#[cfg(test)]
pub fn write_as_other(path: &Path, w: &Write) -> Result<(), String> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path.with_extension("lock"));
    let lock = lock.map_err(|e| e.to_string())?;
    lock.lock().map_err(|e| e.to_string())?;
    let mut store = Store::read_strict(path)?;
    let at = crate::allow::now_s();
    let k = match w {
        Write::Answer { kind, place, file, hash, trusted, .. } => {
            let place = place.clone().unwrap_or_else(|| place_of(file));
            store.answer_at(*kind, &place, *hash, *trusted, at)
        }
        Write::Forget { kind, place, .. } => store.forget_at(*kind, place),
    };
    k.rev = crate::storewrite::Rev { n: k.rev.n + 1, tag: crate::storewrite::tag() };
    let body = store.to_json();
    crate::disk::write_atomic(path, |w| std::io::Write::write_all(w, body.as_bytes()))
        .map_err(|e| e.to_string())
}

/// The card's text: the file (from `root`, the editor's), then each held-back item as written.
pub fn question(root: &Path, p: &Project) -> String {
    let mut q = format!("{} sets what runs commands as you:\n", crate::chat::shown(root, &p.file));
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
    q.push_str("\n\nHeld back, the rest of the file still applies.");
    q
}

pub const TITLE: &str = "Trust this project's settings?";

impl crate::editor::Editor {
    /// The project layer as config read it: its card if it asks, and the config the answer chose.
    pub fn project_trust_set(&mut self, p: Option<Project>) {
        let asks = p.as_ref().is_some_and(|p| p.state == State::Ask);
        self.offers.retain(|o| !matches!(o.what, crate::offer::What::ProjectTrust { .. }) || asks);
        if !asks {
            crate::repotrust::drop_part(self, |w| matches!(w, crate::offer::What::ProjectTrust { .. }), true);
        }
        // (set first: the repository's card it may join names the file from it)
        self.project_trust = p;
        if let Some(p) = self.project_trust.clone().filter(|_| asks) {
            let o = self.project_trust_card(&p);
            self.trust_offer(o);
        }
    }

    /// Its own card — the repository's takes its lines when another part waits with it (`repotrust.rs`).
    fn project_trust_card(&self, p: &Project) -> crate::offer::Offer {
        let what = crate::offer::What::ProjectTrust { hash: p.hash };
        // (what's decided: the settings, under the line naming the file)
        crate::offer::Offer::new(
            TITLE.into(),
            &question(&self.session.root, p),
            "",
            "Project settings".into(),
            what,
        )
        .deciding(2..2 + p.items.len())
    }

    /// The answer on the card (`hash` = what it showed): the config switches, and the answer is kept.
    pub fn project_trust_answer(&mut self, id: u64, hash: Digest, trusted: bool) {
        self.offer_finished(id, Ok(()));
        let Some(n) = self.project_trust_answered(hash, trusted) else { return };
        match trusted {
            true => self.set_success(format!("Trusted — the project's {n} held-back settings apply")),
            false => self.set_status("Held back — :project-trust to look again"),
        }
    }

    /// The answer for what the file says (`hash`), on its card or the repository's: the config switches, the
    /// answer is kept — how many settings it was about (None: the file says something else now).
    pub(crate) fn project_trust_answered(&mut self, hash: Digest, trusted: bool) -> Option<usize> {
        let p = self.project_trust.as_mut().filter(|p| p.hash == hash)?;
        p.state = if trusted { State::Trusted } else { State::Ignored };
        let (file, place, n) = (p.file.clone(), p.place.clone(), p.items.len());
        self.project_trust_apply();
        if let Some(path) = self.trust_path.clone() {
            let seen = crate::storewrite::seen_now(&path);
            let place = Some(place);
            let w = Write::Answer { kind: Kind::Tarae, place, file, hash, trusted, given: given(), seen };
            queue(self, path, vec![w], move |ed, r| match r {
                Err(e) => ed.set_error(format!("couldn't keep the answer: {e}")),
                Ok(w) if w.contains(&Wrote::Skipped) => ed.project_trust_skipped(hash),
                Ok(_) => {}
            });
        }
        Some(n)
    }

    /// A yes on the project's settings not written: changed in another tarae before it, unseen — held back
    /// again, the card asking.
    fn project_trust_skipped(&mut self, hash: Digest) {
        let Some(p) = self.project_trust.as_mut().filter(|p| p.hash == hash && p.state == State::Trusted)
        else {
            return;
        };
        p.state = State::Ask;
        self.project_trust_apply();
        self.set_warning("Changed in another tarae before your yes was kept — held back, asked again");
        let _ = self.project_trust_ask();
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
        crate::edits::at_end_follow_config(self);
    }

    /// `:project-trust` — the card again, for what the file says now.
    pub fn project_trust_ask(&mut self) -> Result<(), String> {
        let p = self.project_trust.clone().ok_or("nothing in this project's .tarae.toml needs trust")?;
        // (asked alone: out of the repository's card, if it's there)
        crate::repotrust::drop_part(self, |w| matches!(w, crate::offer::What::ProjectTrust { .. }), true);
        let o = self.project_trust_card(&p);
        self.push_offer(o);
        Ok(())
    }

    /// `:granted`'s `d` on the project's trust answer (forgotten in the store by then): held back now, the card
    /// asking at the next look — not kept as held back, as `:project-untrust` keeps it.
    pub(crate) fn project_trust_forgotten(&mut self) {
        let Some(p) = self.project_trust.as_mut().filter(|p| p.state == State::Trusted) else { return };
        p.state = State::Ask;
        self.project_trust_apply();
    }

    /// `:project-untrust` — held back again (and kept so).
    pub fn project_untrust(&mut self) -> Result<(), String> {
        let p = self.project_trust.as_ref().ok_or("nothing in this project's .tarae.toml needs trust")?;
        if p.state != State::Trusted {
            return Err("this project isn't trusted".into());
        }
        let (hash, file, place) = (p.hash, p.file.clone(), p.place.clone());
        self.offers.retain(|o| !matches!(o.what, crate::offer::What::ProjectTrust { .. }));
        crate::repotrust::drop_part(self, |w| matches!(w, crate::offer::What::ProjectTrust { .. }), true);
        if let Some(p) = self.project_trust.as_mut() {
            p.state = State::Ignored;
        }
        self.project_trust_apply();
        // (held back in the store first: a reload before the write lands — or after it failed — reads it so; its
        // own write ends the mark)
        let given = self.trust_taking_given(place.clone());
        match self.trust_path.clone() {
            Some(path) => {
                let place = Some(place);
                let w = Write::Answer {
                    kind: Kind::Tarae,
                    place,
                    file,
                    hash,
                    trusted: false,
                    given,
                    seen: BTreeMap::new(),
                };
                queue_said(self, path, vec![w], " — held back until you quit");
            }
            None => taken(&place, given),
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
        record(path.clone(), Kind::Tarae, file.clone(), v(0xab), true).unwrap();
        let s = Store::read(Some(&path));
        assert_eq!(s.state(&file, v(0xab)), State::Trusted);
        assert_eq!(s.state(&file, v(0xac)), State::Ask, "what it was about changed: asks again");
        assert_eq!(s.state(Path::new("/q/.tarae.toml"), v(0xab)), State::Ask, "another project");
        record(path.clone(), Kind::Tarae, file.clone(), v(0xab), false).unwrap();
        assert_eq!(Store::read(Some(&path)).state(&file, v(0xab)), State::Ignored);
        assert!(Store::read(Some(&dir.join("none.json"))).answers.is_empty());
    }

    /// Piece E: each answer is kept with its kind and time, as `:granted` lists them; `forget` takes back the
    /// trusted versions only (a held-back one stays so); an entry without its kind isn't read — it asks again.
    #[test]
    fn answers_keep_their_kind_and_time_and_forget_takes_back_the_trusted_ones() {
        let scratch = crate::scratch::Scratch::new("trust-kinds");
        let path = scratch.join("trust.json");
        let root = scratch.join("p");
        let (toml, hook) = (root.join(".tarae.toml"), root.join(".githooks/pre-commit"));
        record(path.clone(), Kind::Tarae, toml.clone(), v(1), true).unwrap();
        record(path.clone(), Kind::Tarae, toml.clone(), v(2), false).unwrap();
        record(path.clone(), Kind::Hook, hook.clone(), v(3), true).unwrap();
        record(path.clone(), Kind::Tarae, scratch.join("q/.tarae.toml"), v(4), true).unwrap();
        let s = Store::read(Some(&path));
        let listed = s.trusted_in(None, &std::fs::canonicalize(&*scratch).unwrap().join("p"));
        let kinds: Vec<(Kind, usize)> = listed.iter().map(|t| (t.kind, t.versions)).collect();
        assert_eq!(kinds, [(Kind::Tarae, 1), (Kind::Hook, 1)], "this project's, by kind — not q's");
        assert!(listed.iter().all(|t| t.at + 60 > crate::allow::now_s()), "each with its time");
        forget(&path, Kind::Tarae, &listed[0].place, given()).unwrap();
        let s = Store::read(Some(&path));
        assert_eq!(s.state(&toml, v(1)), State::Ask, "taken back: asks again");
        assert_eq!(s.state(&toml, v(2)), State::Ignored, "a held-back version stays so");
        assert_eq!(s.state(&hook, v(3)), State::Trusted, "another place untouched");
        // Without its kind: not read
        let text = std::fs::read_to_string(&path).unwrap().replace("\"kind\": \"hook\",", "");
        std::fs::write(&path, text).unwrap();
        assert_eq!(Store::read(Some(&path)).state(&hook, v(3)), State::Ask);
    }

    /// Answers kept side by side are all kept: each read-change-write under the lock beside the file — without
    /// it, one read what another hadn't written yet and the later write lost the other's answer.
    #[test]
    fn answers_kept_at_once_are_all_kept() {
        let scratch = crate::scratch::Scratch::new("trust-at-once");
        let path = scratch.join("trust.json");
        let file = |t: u8, i: u8| PathBuf::from(format!("/tarae-test-{t}-{i}/.tarae.toml"));
        let threads: Vec<_> = (0..4u8)
            .map(|t| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for i in 0..25u8 {
                        record(path.clone(), Kind::Tarae, file(t, i), v(i), true).unwrap();
                    }
                })
            })
            .collect();
        threads.into_iter().for_each(|t| t.join().unwrap());
        let s = Store::read(Some(&path));
        let lost: Vec<PathBuf> = (0..4u8)
            .flat_map(|t| (0..25u8).map(move |i| (t, i)))
            .filter(|&(t, i)| s.state(&file(t, i), v(i)) != State::Trusted)
            .map(|(t, i)| file(t, i))
            .collect();
        assert!(lost.is_empty(), "{} of 100 answers lost: {lost:?}", lost.len());
    }

    /// A file answered before its folder is made, named through a link (as `/tmp` is `/private/tmp`), is
    /// found by either name once the folder is there — kept under one: its deepest existing folder resolved
    /// (the bring's preflight: `$TMPDIR` a link, an answer kept and looked up under two names).
    #[test]
    fn an_answer_is_found_whatever_name_its_folder_had() {
        let scratch = crate::scratch::Scratch::new("trust-names");
        let base = scratch.to_path_buf();
        std::fs::create_dir_all(base.join("real")).unwrap();
        std::os::unix::fs::symlink(base.join("real"), base.join("link")).unwrap();
        let path = base.join("trust.json");
        record(path.clone(), Kind::Tarae, base.join("link/new/.tarae.toml"), v(1), true).unwrap();
        std::fs::create_dir_all(base.join("real/new")).unwrap();
        let s = Store::read(Some(&path));
        assert_eq!(s.state(&base.join("real/new/.tarae.toml"), v(1)), State::Trusted);
        assert_eq!(s.state(&base.join("link/new/.tarae.toml"), v(1)), State::Trusted);
        std::fs::remove_dir_all(&base).ok();
    }

    /// A digest of `n`s, standing in for a version's.
    fn v(n: u8) -> Digest {
        [n; 32]
    }

    /// FNV-1a, as answers were kept before.
    fn fnv(items: &[(String, String)]) -> u64 {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in items.iter().flat_map(|(k, v)| k.bytes().chain([0]).chain(v.bytes()).chain([0])) {
            h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3);
        }
        h
    }

    /// An answer is about the held-back items' SHA-256 — FNV-1a could be matched by a crafted file, and a
    /// command trusted with no card: what's kept is that digest, each key and value preceded by its length; an
    /// answer kept by FNV before (one per file, or a version of 16 hex digits) asks again.
    #[test]
    fn answers_are_kept_by_the_items_sha256_and_old_ones_ask_again() {
        let dir = std::env::temp_dir().join(format!("tarae-trust-sha-{}", std::process::id()));
        let (path, file) = (dir.join("trust.json"), dir.join(".tarae.toml"));
        let items = vec![
            ("editor.restart-build".to_string(), "\"cargo build\"".to_string()),
            ("task.x.cmd".to_string(), "\"make\"".to_string()),
        ];
        let mut bytes = Vec::new();
        for f in ["editor.restart-build", "\"cargo build\"", "task.x.cmd", "\"make\""] {
            bytes.extend_from_slice(&(f.len() as u64).to_le_bytes());
            bytes.extend_from_slice(f.as_bytes());
        }
        assert_eq!(hash(&items), crate::sha256::digest(&bytes));
        // (the same characters split another way: another digest)
        let moved = [("editor.restart-build\"".to_string(), "cargo build\"".to_string())];
        assert_ne!(hash(&moved), hash(&items[..1]));
        record(path.clone(), Kind::Tarae, file.clone(), hash(&items), true).unwrap();
        let kept = std::fs::read_to_string(&path).unwrap();
        assert!(kept.contains(&format!("\"{}\"", crate::sha256::hex(&bytes))), "kept as the digest: {kept}");
        assert_eq!(Store::read(Some(&path)).state(&file, hash(&items)), State::Trusted);
        // Kept by FNV before: asks once again
        let real = std::fs::canonicalize(&dir).unwrap().join(".tarae.toml");
        let old = fnv(&items);
        let per_file = format!(
            "{{\"projects\": {{\"{}\": {{\"hash\": \"{old:016x}\", \"trusted\": true}}}}}}",
            real.display()
        );
        let versions = format!(
            "{{\"answers\": [{{\"file\": \"{}\", \"versions\": [[\"{old:016x}\", true]]}}]}}",
            real.display()
        );
        for text in [per_file, versions] {
            std::fs::write(&path, &text).unwrap();
            assert_eq!(Store::read(Some(&path)).state(&file, hash(&items)), State::Ask, "{text}");
            assert!(Store::read(Some(&path)).answers.is_empty(), "nothing read of it: {text}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #185: every version answered is kept, for the repository — its checkouts (sessions) share what was
    /// trusted, whichever version each has; a version never answered still asks. Another repository, a
    /// planted link to this one, another file of it: none of its answers. (Git's files made by hand in the
    /// test's temp folder — nothing run.)
    #[test]
    fn each_version_answered_is_kept_for_the_repository() {
        let base = std::fs::canonicalize(std::env::temp_dir())
            .unwrap()
            .join(format!("tarae-trust-versions-{}", std::process::id()));
        let (main, wt, other, planted) =
            (base.join("main"), base.join("wt"), base.join("other"), base.join("planted"));
        let link = main.join(".git/worktrees/wt");
        for d in [&link, &wt, &other.join(".git"), &planted, &main.join("sub")] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(link.join("gitdir"), format!("{}\n", wt.join(".git").display())).unwrap();
        std::fs::write(link.join("commondir"), "../..\n").unwrap();
        std::fs::write(wt.join(".git"), format!("gitdir: {}\n", link.display())).unwrap();
        // (names the repository's worktree folder, which doesn't name it back)
        std::fs::write(planted.join(".git"), format!("gitdir: {}\n", link.display())).unwrap();
        let toml = |d: &Path| d.join(".tarae.toml");
        let path = base.join("trust.json");
        record(path.clone(), Kind::Tarae, toml(&main), v(0xa), true).unwrap();
        record(path.clone(), Kind::Tarae, toml(&main), v(0xb), true).unwrap();
        record(path.clone(), Kind::Tarae, toml(&wt), v(0xc), true).unwrap();
        record(path.clone(), Kind::Tarae, toml(&wt), v(0xd), false).unwrap();
        let s = Store::read(Some(&path));
        assert_eq!(s.state(&toml(&main), v(0xa)), State::Trusted, "an earlier version stays trusted");
        assert_eq!(s.state(&toml(&main), v(0xb)), State::Trusted);
        assert_eq!(s.state(&toml(&main), v(0xc)), State::Trusted, "trusted in another checkout of it");
        assert_eq!(s.state(&toml(&wt), v(0xa)), State::Trusted);
        assert_eq!(s.state(&toml(&main), v(0xd)), State::Ignored, "ignored, per version too");
        assert_eq!(s.state(&toml(&main), v(0xe)), State::Ask, "never seen: asks");
        assert_eq!(s.state(&toml(&other), v(0xa)), State::Ask, "another repository");
        let planted_said = "a link one way only isn't a checkout of it";
        assert_eq!(s.state(&toml(&planted), v(0xa)), State::Ask, "{planted_said}");
        assert_eq!(s.state(&main.join("sub/.tarae.toml"), v(0xa)), State::Ask, "another file of it");
        // Answered again: the new answer
        record(path.clone(), Kind::Tarae, toml(&wt), v(0xa), false).unwrap();
        assert_eq!(Store::read(Some(&path)).state(&toml(&main), v(0xa)), State::Ignored);
        std::fs::remove_dir_all(&base).ok();
    }

    /// Every `llm.sandbox*` setting changes what Claude's commands may do unasked — where they write, what they
    /// reach and by which way (`llm.sandbox-upstream`: around the user's own proxy, or through it) — so a
    /// project's waits for trust. A new one is risky unless it's named here as tarae's own housekeeping.
    #[test]
    fn every_sandbox_setting_waits_for_trust() {
        // (when tarae removes the kept run homes no run has used: nothing a run can do changes)
        const HOUSEKEEPING: [&str; 1] = ["llm.sandbox-kept-days"];
        let sandbox = crate::settings::SETTINGS.iter().filter(|s| s.path.starts_with("llm.sandbox"));
        let mut n = 0;
        for s in sandbox {
            assert_eq!(is_risky(s.path), !HOUSEKEEPING.contains(&s.path), "{}", s.path);
            n += 1;
        }
        assert!(n >= 10, "the sandbox's settings found: {n}");
    }

    /// Beyond `llm.sandbox*`: the settings that start a program, or pick which — a language server (G13), a
    /// build, Claude — wait for trust too (`[lang.*] lsp`/`also`: `config::tests::what_starts_a_language_server_waits_for_trust`).
    #[test]
    fn every_setting_that_starts_a_program_waits_for_trust() {
        let starts = [
            "editor.lsp",
            "editor.restart-build",
            "editor.restart-binary",
            "llm.command",
            "llm.args",
            "session.ended",
        ];
        for s in starts {
            assert!(crate::settings::SETTINGS.iter().any(|x| x.path == s), "{s} is a setting");
            assert!(is_risky(s), "{s}");
        }
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
        // What's decided — drawn at full strength — is exactly the settings, not the line naming the file
        let lines: Vec<&str> = ed.offers[0].question.split('\n').collect();
        let decided = ed.offers[0].decided_lines();
        let items: Vec<usize> = (0..lines.len()).filter(|&i| lines[i].starts_with("  ")).collect();
        assert_eq!(decided.collect::<Vec<_>>(), items, "{lines:?}");
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
        assert!(question(Path::new("/"), &p).contains("too long to show"));
        let mut store = Store::default();
        store.answer(crate::trust::Kind::Tarae, &path, p.hash, true, 0);
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
