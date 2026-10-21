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
/// with (`llm.edit-tools`), whether what you use is counted (`features.count`) and whether your cards are kept
/// (`llm.card-record`) and whether pings spend your usage to keep a conversation warm (`llm.keep-warm*`) — a
/// project never does.
pub const YOURS_ONLY: [&str; 9] = [
    "llm.card-record",
    "llm.keep-warm",
    "llm.keep-warm-min-tokens",
    "llm.keep-warm-max",
    "llm.environment-key",
    "llm.billing",
    "llm.reviewer-billing",
    "llm.edit-tools",
    "features.count",
];

pub fn yours_only(setting: &str) -> bool {
    YOURS_ONLY.contains(&setting)
}

/// `:` commands only your own keys run (or you, typed): they switch who pays for Claude. A project's key binding
/// to one is left out (`config.rs`).
pub const YOURS_ONLY_COMMANDS: [&str; 1] = ["session-billing"];

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
    pub hash: Digest,
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

/// One place's answers: what they're about, and each version's (digest, trusted, when — seconds since the
/// epoch), the oldest first.
#[derive(Clone, Debug, PartialEq)]
struct Kept {
    kind: Kind,
    versions: Vec<(Digest, bool, u64)>,
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
}

impl Store {
    pub fn path() -> Option<PathBuf> {
        crate::config::state_dir().map(|d| d.join("trust.json"))
    }

    /// Read from `path` (missing or unreadable = nothing answered). An entry without its kind or a version without
    /// its time isn't read — that version asks again.
    pub fn read(path: Option<&Path>) -> Store {
        let Some(text) = path.and_then(|p| std::fs::read_to_string(p).ok()) else { return Store::default() };
        let Ok(json) = serde_json::from_str::<Json>(&text) else { return Store::default() };
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
            let versions = a["versions"].as_array().into_iter().flatten();
            let versions: Vec<(Digest, bool, u64)> =
                versions.filter_map(|v| Some((digest(&v[0])?, v[1].as_bool()?, v[2].as_u64()?))).collect();
            if !versions.is_empty() {
                store.answers.insert(place, Kept { kind, versions });
            }
        }
        store
    }

    /// The answer for `file` as it is now (`hash`) — Ask when this version was never answered.
    pub fn state(&self, file: &Path, hash: Digest) -> State {
        let found = self.answers.get(&place_of(file)).and_then(|k| k.versions.iter().rfind(|a| a.0 == hash));
        match found {
            Some((_, true, _)) => State::Trusted,
            Some((_, false, _)) => State::Ignored,
            None => State::Ask,
        }
    }

    /// `file`'s version `hash`, a `kind` of answer, answered at `at` (again: the new answer).
    pub fn answer(&mut self, kind: Kind, file: &Path, hash: Digest, trusted: bool, at: u64) {
        let k = self.answers.entry(place_of(file)).or_insert(Kept { kind, versions: Vec::new() });
        k.kind = kind;
        k.versions.retain(|a| a.0 != hash);
        k.versions.push((hash, trusted, at));
        let over = k.versions.len().saturating_sub(VERSIONS_KEPT);
        k.versions.drain(..over);
    }

    /// `place`'s trusted versions forgotten — each asks again; one held back stays so.
    pub fn forget(&mut self, place: &Place) {
        if let Some(k) = self.answers.get_mut(place) {
            k.versions.retain(|a| !a.1);
            if k.versions.is_empty() {
                self.answers.remove(place);
            }
        }
    }

    /// The places with a version trusted — the repository's whose git common folder is `common`, or (none:
    /// outside one) under `root` — by kind, then the newest answer first.
    pub fn trusted_in(&self, common: Option<&Path>, root: &Path) -> Vec<Trusted> {
        let mut out: Vec<Trusted> = self
            .answers
            .iter()
            .filter(|(p, _)| p.in_here(common, root))
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
                let kind = k.kind.word();
                match place {
                    Place::File(f) => json!({ "kind": kind, "file": f, "versions": versions }),
                    Place::Repo { common, file } => {
                        json!({ "kind": kind, "repository": common, "file": file, "versions": versions })
                    }
                }
            })
            .collect();
        serde_json::to_string_pretty(&json!({ "answers": answers })).unwrap_or_default()
    }
}

/// Keeps an answer of `kind` (on a worker — `change`).
pub fn record(path: PathBuf, kind: Kind, file: PathBuf, hash: Digest, trusted: bool) -> Result<(), String> {
    let at = crate::allow::now_s();
    change(&path, |s| s.answer(kind, &file, hash, trusted, at))
}

/// Takes back what's trusted at `place` (`:granted`'s `d`, on a worker — `change`): each version asks again.
pub fn forget(path: &Path, place: &Place) -> Result<(), String> {
    change(path, |s| s.forget(place))
}

/// The store changed by `f` (read, change, write; the file is tiny) — under the lock beside it: two answers
/// kept side by side (two cards answered at once, two taraes) each read what the other hadn't written yet, and
/// the later write lost the other's (`a_change_answered_before_restarts_claude_without_a_card` on Linux: the
/// card's `n` and the test's own answer).
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
    let mut store = Store::read(Some(path));
    f(&mut store);
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
        let (file, n) = (p.file.clone(), p.items.len());
        self.project_trust_apply();
        if let Some(path) = self.trust_path.clone() {
            self.events.jobs().spawn(move || {
                let r = record(path, Kind::Tarae, file, hash, trusted);
                move |ed: &mut crate::editor::Editor| {
                    if let Err(e) = r {
                        ed.set_error(format!("couldn't keep the answer: {e}"));
                    }
                }
            });
        }
        Some(n)
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
        let (hash, file) = (p.hash, p.file.clone());
        self.offers.retain(|o| !matches!(o.what, crate::offer::What::ProjectTrust { .. }));
        crate::repotrust::drop_part(self, |w| matches!(w, crate::offer::What::ProjectTrust { .. }), true);
        if let Some(p) = self.project_trust.as_mut() {
            p.state = State::Ignored;
        }
        self.project_trust_apply();
        if let Some(path) = self.trust_path.clone() {
            self.events.jobs().spawn(move || {
                let r = record(path, Kind::Tarae, file, hash, false);
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
        record(path.clone(), Kind::Tarae, file.clone(), v(0xab), true).unwrap();
        let s = Store::read(Some(&path));
        assert_eq!(s.state(&file, v(0xab)), State::Trusted);
        assert_eq!(s.state(&file, v(0xac)), State::Ask, "what it was about changed: asks again");
        assert_eq!(s.state(Path::new("/q/.tarae.toml"), v(0xab)), State::Ask, "another project");
        record(path.clone(), Kind::Tarae, file.clone(), v(0xab), false).unwrap();
        assert_eq!(Store::read(Some(&path)).state(&file, v(0xab)), State::Ignored);
        assert_eq!(Store::read(Some(&dir.join("none.json"))), Store::default());
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
        forget(&path, &listed[0].place).unwrap();
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
            assert_eq!(Store::read(Some(&path)), Store::default(), "nothing read of it: {text}");
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
