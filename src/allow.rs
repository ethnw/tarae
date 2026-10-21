//! What the user lets Claude do past the defaults: read outside the project (`reads.rs`), and — from what
//! the sandbox blocked (`blocked.rs`) — let its commands write in a folder or connect to a port on this
//! machine. Each answer on its card is for once, for this conversation (`Chat.allowed`), for this session
//! (`Session.allowed`, kept with it in `$state/sessions`), or always: kept per project (its real path) in
//! `$state/sandbox.json` — the user's state, not the project's files (those go into git, to other people, and
//! would need trust again). Each kept answer is an entry with its scope and when it was granted.
//!
//! How long one may last is `Caps::storable`'s (plan §2.3): a place that holds keys or is home itself (or above
//! it), a file named like a key, Docker's socket are only ever allowed once; another socket for the
//! conversation at most. A folder whose writing could run something later (`NEVER_WRITE`) isn't offered at all.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value as Json, json};

/// What one scope allows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Allowed {
    /// Files, and folders with everything in them, Claude's Read, Grep and Glob may read (real paths).
    pub read: Vec<PathBuf>,
    /// Folders (with everything in them) Claude's commands may write in, besides the sandbox's own.
    pub write: Vec<PathBuf>,
    /// Ports on this machine Claude's commands may connect to.
    pub ports: Vec<u16>,
    /// Unix sockets Claude's commands may connect to — each one file, by its real path (#119); this
    /// conversation's at most, never kept.
    pub sockets: Vec<PathBuf>,
    /// Hosts Claude's commands reach through tarae's proxy without asking, each at its port — kept for the
    /// project only (the card's `A`, `hosts.rs`).
    pub hosts: Vec<(String, u16)>,
    /// macOS: Claude's commands may open the terminals (`/dev/ttys*`) — every one (`Grant::Terminals`).
    pub terminals: bool,
    /// Files named like keys inside the project Claude's commands may read — each that file, by its real path
    /// (`Grant::Key`; macOS 7a, Linux 7b).
    pub keys: Vec<PathBuf>,
    /// Every port on this machine, the risky ones too (`Grant::Localhost` — as `llm.sandbox-localhost`).
    pub localhost: bool,
    /// Variables of the user's passed to Claude's commands, each by its whole name (`Grant::Env` — as an
    /// `llm.sandbox-env` entry).
    pub env: Vec<String>,
    /// When each was answered (seconds since the epoch) — what `:granted` says. One kept before times were (an
    /// old `sandbox.json`) has its file's time.
    pub granted: BTreeMap<Grant, u64>,
}

/// Now, in seconds since the epoch.
pub fn now_s() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

impl Allowed {
    /// Whether `real` (a real path) is one of the places allowed, or in one.
    pub fn reads(&self, real: &Path) -> bool {
        self.read.iter().any(|p| real.starts_with(p))
    }

    /// Adds a place (one already covered isn't added twice).
    pub fn add_read(&mut self, real: PathBuf) {
        if add_place(&mut self.read, real.clone()) {
            self.stamp(Grant::Read(real));
        }
    }

    pub fn add_write(&mut self, real: PathBuf) {
        if add_place(&mut self.write, real.clone()) {
            self.stamp(Grant::Write(real));
        }
    }

    pub fn add_port(&mut self, port: u16) {
        if !self.ports.contains(&port) {
            self.ports.push(port);
            self.stamp(Grant::Port(port));
        }
    }

    pub fn add_host(&mut self, host: &str, port: u16) {
        if !self.hosts.iter().any(|(h, p)| h == host && *p == port) {
            self.hosts.push((host.to_string(), port));
            self.stamp(Grant::Host(host.to_string(), port));
        }
    }

    pub fn add(&mut self, g: &Grant) {
        match g {
            Grant::Read(p) => self.add_read(p.clone()),
            Grant::Write(p) => self.add_write(p.clone()),
            Grant::Port(n) => self.add_port(*n),
            // (Docker's: one run's or one command's, never a chat's — #193)
            Grant::Socket(_) if g.as_outside() => {}
            Grant::Socket(s) => {
                if !self.sockets.contains(s) {
                    self.sockets.push(s.clone());
                    self.stamp(g.clone());
                }
            }
            Grant::Host(h, p) => self.add_host(h, *p),
            Grant::Terminals => {
                if !self.terminals {
                    self.terminals = true;
                    self.stamp(Grant::Terminals);
                }
            }
            Grant::Key(k) => {
                if !self.keys.contains(k) {
                    self.keys.push(k.clone());
                    self.stamp(g.clone());
                }
            }
            Grant::Localhost => {
                if !self.localhost {
                    self.localhost = true;
                    self.stamp(Grant::Localhost);
                }
            }
            Grant::Env(n) => {
                if !self.env.contains(n) {
                    self.env.push(n.clone());
                    self.stamp(g.clone());
                }
            }
        }
    }

    /// `g` answered now — unless a time is there already (read from where it was kept).
    fn stamp(&mut self, g: Grant) {
        self.granted.entry(g).or_insert_with(now_s);
    }

    /// Everything in it, each kind in its fields' order.
    pub fn grants(&self) -> Vec<Grant> {
        let mut out: Vec<Grant> = self.read.iter().cloned().map(Grant::Read).collect();
        out.extend(self.write.iter().cloned().map(Grant::Write));
        out.extend(self.ports.iter().copied().map(Grant::Port));
        out.extend(self.sockets.iter().cloned().map(Grant::Socket));
        out.extend(self.hosts.iter().cloned().map(|(h, p)| Grant::Host(h, p)));
        out.extend(self.terminals.then_some(Grant::Terminals));
        out.extend(self.keys.iter().cloned().map(Grant::Key));
        out.extend(self.localhost.then_some(Grant::Localhost));
        out.extend(self.env.iter().cloned().map(Grant::Env));
        out
    }

    /// `other`'s added, each with its own time.
    pub fn merge(&mut self, other: &Allowed) {
        for g in other.grants() {
            if let Some(at) = other.granted.get(&g) {
                self.granted.entry(g.clone()).or_insert(*at);
            }
            self.add(&g);
        }
    }

    /// What it holds as kept entries (`sandbox.json`'s, a session's in `$state/sessions`): kind, what, `scope`,
    /// `granted` — never a socket (a server's socket is the server: this conversation's at most).
    pub fn entries(&self, scope: &str) -> Vec<Json> {
        let mut out = Vec::new();
        for g in self.grants() {
            let Some((kind, what)) = g.entry() else { continue };
            let mut e = json!({ "kind": kind, "scope": scope });
            if let Some(what) = what {
                e["what"] = json!(what);
            }
            if let Some(at) = self.granted.get(&g) {
                e["granted"] = json!(at);
            }
            out.push(e);
        }
        out
    }

    /// Read back from `entries` (what `entries` wrote); one with no time takes `at` (its file's). One that
    /// doesn't read (a hand edit — a host the proxy can't read, a kind unknown) is left out, and said.
    pub fn from_entries(entries: &Json, at: u64) -> Allowed {
        let mut a = Allowed::default();
        for e in entries.as_array().into_iter().flatten() {
            let (kind, what) = (e["kind"].as_str().unwrap_or(""), e["what"].as_str());
            let Some(g) = Grant::of_entry(kind, what) else {
                let said = format!("sandbox: a kept entry that doesn't read: {e}");
                crate::log::line("sandbox", "warning", &said);
                continue;
            };
            a.granted.insert(g.clone(), e["granted"].as_u64().unwrap_or(at));
            a.add(&g);
        }
        a
    }

    /// Takes back a place, a port, a socket or the terminals (as `:granted` lists it).
    pub fn remove(&mut self, what: &str) {
        self.remove_only(what);
        let left = self.grants();
        self.granted.retain(|g, _| left.contains(g));
    }

    fn remove_only(&mut self, what: &str) {
        if what == TERMINALS_LISTED {
            self.terminals = false;
        }
        if what == LOCALHOST_LISTED {
            self.localhost = false;
        }
        if let Some(n) = what.strip_prefix("env ") {
            self.env.retain(|e| e != n);
        }
        let at = Path::new(what);
        self.read.retain(|p| p != at);
        self.write.retain(|p| p != at);
        self.keys.retain(|p| p != at);
        if let Some(n) = what.strip_prefix("port ").and_then(|n| n.parse::<u16>().ok()) {
            self.ports.retain(|p| *p != n);
        }
        if let Some(s) = what.strip_prefix("socket ") {
            self.sockets.retain(|p| p != Path::new(s));
        }
        if let Some((host, port)) = what.strip_prefix("host ").and_then(host_port) {
            self.hosts.retain(|(h, p)| *h != host || *p != port);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.read.is_empty()
            && self.write.is_empty()
            && self.ports.is_empty()
            && self.sockets.is_empty()
            && self.hosts.is_empty()
            && !self.terminals
            && self.keys.is_empty()
            && !self.localhost
            && self.env.is_empty()
    }
}

/// How `:granted` lists `Grant::Terminals` for `:sandbox-disallow` to take it back.
pub const TERMINALS_LISTED: &str = "terminals";

/// How `:granted` lists `Grant::Localhost` for `:sandbox-disallow` to take it back.
pub const LOCALHOST_LISTED: &str = "localhost";

/// `host:port` as a kept answer or `:sandbox-disallow` writes it: one name (as the proxy reads names) at its
/// port — None for anything else (a pattern, an address, no port).
pub fn host_port(s: &str) -> Option<(String, u16)> {
    match crate::sandbox::proxy::egress::Pattern::parse(s.trim()).ok()? {
        crate::sandbox::proxy::egress::Pattern::Name(h, Some(p)) => Some((h, p)),
        _ => None,
    }
}

/// Whether `real` was added (not covered by a place there already).
fn add_place(list: &mut Vec<PathBuf>, real: PathBuf) -> bool {
    if list.iter().any(|p| real.starts_with(p)) {
        return false;
    }
    list.retain(|p| !p.starts_with(&real));
    list.push(real);
    true
}

/// What a command of Claude's may do more: read a place in home (its reads there are confined — card model
/// P2; Claude's Read tool reads it too), write in a folder, connect to a port on this machine, or to one
/// unix socket (#119 — colima's `docker.sock` for `cargo xtask linux`: the only way was the sandbox off).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Grant {
    Read(PathBuf),
    Write(PathBuf),
    Port(u16),
    /// That socket file only (its real path) — never a folder of them, never kept.
    Socket(PathBuf),
    /// A host at its port, through tarae's proxy (`hosts.rs`) — kept for the project, or this session's.
    Host(String, u16),
    /// macOS: open the terminals (`sandbox::TERMINALS`) — all of them, the user's own too: Seatbelt can't
    /// tell a pty a run made (expect, `script`, a pty test) from the user's shells.
    Terminals,
    /// Read that file in the project, named like a key (`sandbox::key_named_in`) — that one file, its real path
    /// (macOS 7a, Linux 7b).
    Key(PathBuf),
    /// Every port on this machine, `sandbox::RISKY_PORTS` too (as `llm.sandbox-localhost`: Seatbelt can't open
    /// `localhost:*` but some) — Gradle's client and daemon talk over ports chosen as they start.
    Localhost,
    /// Pass the user's variable of that name, whole (as one `llm.sandbox-env` entry) — never a name
    /// `readonly::env_allowed` keeps off; its value is on no card and in no record.
    Env(String),
}

impl Grant {
    /// As the card and the toasts say it.
    pub fn shown(&self) -> String {
        match self {
            Grant::Read(p) => format!("read {} and everything in it", tilde(p)),
            Grant::Write(p) => format!("write in {} and everything in it", tilde(p)),
            Grant::Port(n) => format!("connect to port {n} on this machine"),
            Grant::Socket(s) => format!("connect to the socket {} — that one file", tilde(s)),
            Grant::Host(h, p) => format!("reach {h}:{p} through tarae's proxy"),
            Grant::Terminals => "open terminals (/dev/ttys*) — every one, yours too".into(),
            Grant::Key(k) => format!("read {}, named like a key — that one file", tilde(k)),
            Grant::Localhost => "connect to every port on this machine".into(),
            Grant::Env(n) => format!("have your variable {n} (its value is never shown)"),
        }
    }

    /// Whether it can be kept for the project (`A`) — `Caps::storable`'s rule.
    pub fn keepable(&self) -> bool {
        crate::caps::Caps::storable(self) == Scope::Always
    }

    /// As a kept entry: its kind, and what (none for the terminals) — None for a socket, never kept.
    fn entry(&self) -> Option<(&'static str, Option<String>)> {
        let s = |p: &PathBuf| Some(p.to_string_lossy().into_owned());
        Some(match self {
            Grant::Read(p) => ("read", s(p)),
            Grant::Write(p) => ("write", s(p)),
            Grant::Port(n) => ("port", Some(n.to_string())),
            Grant::Socket(_) => return None,
            Grant::Host(h, p) => ("host", Some(format!("{h}:{p}"))),
            Grant::Terminals => ("terminals", None),
            Grant::Key(k) => ("key", s(k)),
            Grant::Localhost => ("localhost", None),
            Grant::Env(n) => ("env", Some(n.clone())),
        })
    }

    /// Back from a kept entry (`entry`) — a path absolute, a host one name at its port.
    fn of_entry(kind: &str, what: Option<&str>) -> Option<Grant> {
        let path = || what.map(PathBuf::from).filter(|p| p.is_absolute());
        Some(match kind {
            "read" => Grant::Read(path()?),
            "write" => Grant::Write(path()?),
            "port" => Grant::Port(what?.parse().ok()?),
            "host" => host_port(what?).map(|(h, p)| Grant::Host(h, p))?,
            "terminals" => Grant::Terminals,
            "key" => Grant::Key(path()?),
            "localhost" => Grant::Localhost,
            "env" => Some(what?).filter(|n| crate::readonly::env_allowed(n)).map(|n| Grant::Env(n.into()))?,
            _ => return None,
        })
    }

    /// Whether it's as much as running outside the sandbox (`warning` says why: Docker's socket) — never
    /// allowed for a chat, only for one run or one command (#193, `tools::RunAgain`).
    pub fn as_outside(&self) -> bool {
        self.warning().is_some()
    }

    /// What yes risks beyond what the grant says, on its card (`warning` in `warning`, the terminals').
    pub fn risk(&self) -> Option<String> {
        match self {
            Grant::Terminals => Some(
                "macOS's sandbox can't tell a terminal a command made from yours: its commands could read what \
                 you type in your shells, tarae's and other sessions', and type into them."
                    .into(),
            ),
            Grant::Key(_) => Some(
                "Its commands read what's in it — a key or a token, by its name — and what they print reaches \
                 Claude."
                    .into(),
            ),
            Grant::Localhost => {
                let risky: Vec<String> = crate::sandbox::RISKY_PORTS
                    .iter()
                    .map(|(p, why)| format!("{p} {}", why.split_once(" — ").map_or(*why, |(name, _)| name)))
                    .collect();
                Some(format!(
                    "The risky ones too, where what listens runs what it's asked to outside the sandbox: {}.",
                    risky.join(" · ")
                ))
            }
            Grant::Env(_) => Some("Its commands read its value, and what they print reaches Claude.".into()),
            _ => self.warning().map(String::from),
        }
    }

    /// For a socket that's a container engine's (the packs' `engine-sockets`: `docker.sock`, colima's, Docker
    /// Desktop's — the containers pack): what connecting to it means, said on its card.
    pub fn warning(&self) -> Option<&'static str> {
        let Grant::Socket(s) = self else { return None };
        engine_socket(s, &crate::sandboxpack::get().engine_sockets)
    }
}

/// The words of `engines`' entry `s` is a socket of — by its file's name, or a folder on its path.
fn engine_socket<'a>(s: &Path, engines: &'a [crate::sandboxpack::EngineSocket]) -> Option<&'a str> {
    let name = s.file_name().and_then(|n| n.to_str());
    let folders: Vec<&str> = s.components().filter_map(|c| c.as_os_str().to_str()).collect();
    engines
        .iter()
        .find(|e| {
            name.is_some_and(|n| e.names.iter().any(|x| x == n))
                || folders.iter().any(|f| e.folders.iter().any(|x| x == f))
        })
        .map(|e| e.say.as_str())
}

/// `p` with home spelled `~`.
pub fn tilde(p: &Path) -> String {
    match home().and_then(|h| p.strip_prefix(&h).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => p.display().to_string(),
    }
}

/// How long an answer lasts — shortest first (`Caps::storable` says the longest a grant may).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Scope {
    Once,
    ThisChat,
    /// This tarae session: kept with it (`Session.allowed`, `$state/sessions`) until it ends or the answer is
    /// taken back — a host's until you quit (`hosts.rs`).
    Session,
    Always,
}

/// `sandbox.json`'s form: each project a list of entries, each with its `scope` and when it was `granted`
/// (cardauto piece 6). A file without it is the old form: a list per kind, no times.
const STORE_VERSION: u64 = 2;

/// The "always" answers, per project (real path) — `$state/sandbox.json`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Store {
    projects: BTreeMap<PathBuf, Allowed>,
}

impl Store {
    pub fn path() -> Option<PathBuf> {
        crate::config::state_dir().map(|d| d.join("sandbox.json"))
    }

    /// Read from `path` (missing or unreadable = nothing allowed) — the old form too, each answer taking the
    /// file's time (written in the new form at the next change).
    pub fn read(path: Option<&Path>) -> Store {
        let Some(text) = path.and_then(|p| std::fs::read_to_string(p).ok()) else { return Store::default() };
        let Ok(json) = serde_json::from_str::<Json>(&text) else { return Store::default() };
        let at = path
            .and_then(|p| std::fs::metadata(p).ok()?.modified().ok())
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or(0, |d| d.as_secs());
        if json["version"].as_u64() == Some(STORE_VERSION) {
            let projects = json["projects"].as_object().into_iter().flatten();
            let projects = projects.map(|(p, e)| (PathBuf::from(p), Allowed::from_entries(e, at))).collect();
            return Store { projects };
        }
        let mut projects = BTreeMap::new();
        for (project, a) in json["projects"].as_object().into_iter().flatten() {
            let paths = |k: &str| -> Vec<PathBuf> {
                a[k].as_array().into_iter().flatten().filter_map(Json::as_str).map(PathBuf::from).collect()
            };
            let ports = a["ports"].as_array().into_iter().flatten().filter_map(Json::as_u64);
            let ports = ports.filter_map(|p| u16::try_from(p).ok()).collect();
            // (one the proxy can't read — a hand edit — is left out, and said)
            let mut hosts = Vec::new();
            for h in a["hosts"].as_array().into_iter().flatten().filter_map(Json::as_str) {
                match host_port(h) {
                    Some(hp) => hosts.push(hp),
                    None => {
                        crate::log::line("sandbox", "warning", &format!("sandbox.json: not a host:port: {h}"))
                    }
                }
            }
            let (read, write, sockets) = (paths("read"), paths("write"), Vec::new());
            let terminals = a["terminals"].as_bool().unwrap_or(false);
            let keys = paths("keys");
            let mut a = Allowed { read, write, ports, sockets, hosts, terminals, keys, ..Allowed::default() };
            a.granted = a.grants().into_iter().map(|g| (g, at)).collect();
            projects.insert(PathBuf::from(project), a);
        }
        Store { projects }
    }

    pub fn get(&self, project: &Path) -> Allowed {
        self.projects.get(project).cloned().unwrap_or_default()
    }

    /// Every project's kept answers, by its real path.
    pub fn projects(&self) -> impl Iterator<Item = (&Path, &Allowed)> {
        self.projects.iter().map(|(p, a)| (p.as_path(), a))
    }

    pub fn to_json(&self) -> String {
        // Inserted, not collected: a Map's `collect` sorts — a copy of the sort for each (~6 KB)
        let mut projects = serde_json::Map::new();
        for (p, a) in self.projects.iter().filter(|(_, a)| !a.is_empty()) {
            projects.insert(p.to_string_lossy().into_owned(), json!(a.entries("always")));
        }
        serde_json::to_string_pretty(&json!({ "version": STORE_VERSION, "projects": projects }))
            .unwrap_or_default()
    }
}

/// `keep`'s turn: one at a time (a test holds it to see what goes on before a write lands).
pub(crate) static KEEP: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Changes what `project` always allows (on a worker — read, change, write; one at a time).
pub fn keep(path: &Path, project: &Path, change: impl FnOnce(&mut Allowed)) -> Result<(), String> {
    let _held = KEEP.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = Store::read(Some(path));
    change(store.projects.entry(project.to_path_buf()).or_default());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let body = store.to_json();
    crate::disk::write_atomic(path, |w| std::io::Write::write_all(w, body.as_bytes()))
        .map_err(|e| e.to_string())
}

/// Places under home that hold keys and tokens — allowed once at a time, never kept; Claude's commands never
/// read them (`sandbox::kept_reads`). Git's credential store keeps passwords in plain text in
/// `~/.git-credentials` and `~/.config/git/credentials` — the latter inside `~/.config/git`, which runs read.
pub(crate) const SECRETS: [&str; 13] = [
    ".git-credentials",
    ".config/git/credentials",
    ".ssh",
    ".aws",
    ".gnupg",
    ".config/gh",
    ".kube",
    ".docker",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".password-store",
    "Library/Keychains",
];

/// Whether an answer about `real` may last past this once: not home itself or a folder above it (that's
/// everything), and nothing in a place that holds keys.
pub fn keepable(real: &Path, home: Option<&Path>) -> bool {
    let Some(home) = home else { return real != Path::new("/") };
    if home.starts_with(real) {
        return false;
    }
    !secret(real, Some(home))
}

/// Whether `real` is in a place under home that holds keys (`SECRETS`).
pub fn secret(real: &Path, home: Option<&Path>) -> bool {
    home.is_some_and(|h| SECRETS.iter().any(|s| real.starts_with(h.join(s))))
}

/// Places under home that run something later when written — and so are never offered for writing (a
/// folder holding one isn't either): shells' and git's settings, launch agents, tarae's own config, cargo's
/// caches (every project's build uses the crates and checkouts there; with the network closed the profile
/// keeps them whatever is granted — `sandbox::Policy::caches`). Keys (`SECRETS`) neither. Nor are they ever
/// removed as a session's build folder (`builds::Builds::fate`).
pub(crate) const NEVER_WRITE: [&str; 8] = [
    ".config/fish",
    ".config/git",
    ".config/tarae",
    ".oh-my-zsh",
    "Library/LaunchAgents",
    ".zsh",
    ".cargo/registry",
    ".cargo/git",
];

/// Folders offered for one denied write, at most; and how deep one must be (`/opt`, `/private/var` aren't).
const WRITE_CHOICES: usize = 4;
const WRITE_DEPTH: usize = 3;

/// The folders a card may offer to let Claude's commands write in, for a write the sandbox denied at
/// `target`: the folder itself when it is one, else its nearest existing folder, up to just below home
/// (outside home, `WRITE_CHOICES` at most, none shallower than `WRITE_DEPTH`), widest last — none that
/// `write_refused`. Real paths.
pub fn write_choices(
    target: &Path,
    home: &Path,
    root: &Path,
    later: &crate::sandbox::RunsLater,
) -> Vec<PathBuf> {
    let mut at =
        if target.is_dir() { Some(target.to_path_buf()) } else { target.parent().map(Path::to_path_buf) };
    while let Some(p) = at.clone().filter(|p| !p.exists()) {
        at = p.parent().map(Path::to_path_buf);
    }
    let mut out = Vec::new();
    while let Some(dir) = at {
        at = dir.parent().map(Path::to_path_buf);
        let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
        if !write_refused(&dir, home, root, later) && !out.contains(&dir) {
            out.push(dir);
        }
    }
    out.truncate(WRITE_CHOICES);
    out
}

/// The folder a card may offer for a write Claude named itself (`also`): `target` (a real path) and nothing
/// wider — None when it isn't a folder or `write_refused`.
pub fn write_place(
    target: &Path,
    home: &Path,
    root: &Path,
    later: &crate::sandbox::RunsLater,
) -> Option<PathBuf> {
    (target.is_dir() && !write_refused(target, home, root, later)).then(|| target.to_path_buf())
}

/// Whether no card may open folder `dir` (real) to writes: home or above it, one that holds keys or a place
/// that runs something later (`NEVER_WRITE`; `later`: the folders of programs the user runs, the shared
/// caches, a toolchain's home and all in it), one in (or holding) the project, or one shallower than
/// `WRITE_DEPTH`.
fn write_refused(dir: &Path, home: &Path, root: &Path, later: &crate::sandbox::RunsLater) -> bool {
    let never = || SECRETS.iter().chain(NEVER_WRITE.iter()).map(|s| home.join(s));
    home.starts_with(dir)
        || never().any(|n| n.starts_with(dir) || dir.starts_with(n))
        || later.refuses(dir).is_some()
        || dir.starts_with(root)
        || root.starts_with(dir)
        || dir.components().count() <= WRITE_DEPTH
}

/// The places a card may offer to let Claude's commands read, for a read the sandbox denied at `target` (a
/// real path in home — reads elsewhere aren't confined): a folder itself, or a file right in home
/// (`~/.zshenv`), else the folder a file is in — then those above up to just below home, nearest first
/// (`WRITE_CHOICES` at most) — never home or above it, nor a place in, or holding, one kept from every run
/// (`kept`: keys, Claude's and tarae's own).
pub fn read_choices(target: &Path, home: &Path, kept: &[PathBuf]) -> Vec<PathBuf> {
    if !target.starts_with(home) || target == home {
        return Vec::new();
    }
    let mut out = Vec::new();
    let itself = target.is_dir() || target.parent() == Some(home);
    let mut at = if itself { Some(target) } else { target.parent() };
    while let Some(dir) = at.filter(|d| *d != home) {
        if read_open(dir, home, kept) {
            out.push(dir.to_path_buf());
        }
        at = dir.parent();
    }
    out.truncate(WRITE_CHOICES);
    out
}

/// The place a card may offer for a read Claude named itself (`also`): `target` (a real path in home, a file
/// or a folder) and nothing wider — None when it's home or above, outside home (reads there aren't
/// confined), or in or holding a place kept from every run.
pub fn read_place(target: &Path, home: &Path, kept: &[PathBuf]) -> Option<PathBuf> {
    let open = target.starts_with(home) && target != home && read_open(target, home, kept);
    open.then(|| target.to_path_buf())
}

/// Whether a card may open `p` (in home) to reads: not in or holding a place `kept` from every run, and an
/// answer about it may last (`keepable`).
fn read_open(p: &Path, home: &Path, kept: &[PathBuf]) -> bool {
    !kept.iter().any(|k| k.starts_with(p) || p.starts_with(k)) && keepable(p, Some(home))
}

/// What `a`/`A` on a read's card allow for `reads` (real paths) — each place itself, or its folder when
/// another read there asks too or was let through once (`others` — #102: twelve shots in one folder were
/// twelve cards): the folder then, with everything in it. Only a folder that can be kept (`keepable`), deep
/// enough not to be a whole area (`/private/tmp`, `/opt` — `WRITE_DEPTH`; a folder in a temp folder is,
/// as Linux's `/tmp/x` is one level above macOS's `/private/tmp/x`), not home's own folders
/// (`~/Downloads`), nor a temp folder itself.
pub fn grant_places(reads: &[PathBuf], others: &[PathBuf], home: Option<&Path>) -> Vec<PathBuf> {
    let temp = crate::sandbox::profile_path(&std::env::temp_dir());
    grant_places_in(reads, others, home, &temp, &crate::sandbox::profile_path(Path::new("/tmp")))
}

/// `grant_places` with the temp folders given (`temp`: the user's, `tmp`: `/tmp`'s real path).
fn grant_places_in(
    reads: &[PathBuf],
    others: &[PathBuf],
    home: Option<&Path>,
    temp: &Path,
    tmp: &Path,
) -> Vec<PathBuf> {
    let roots =
        [tmp.to_path_buf(), temp.to_path_buf(), temp.parent().map(Path::to_path_buf).unwrap_or_default()];
    let tmp = [tmp.to_path_buf(), temp.to_path_buf()];
    let mut out: Vec<PathBuf> = Vec::new();
    for r in reads {
        let folder = r.parent().filter(|dir| {
            let sibling = reads.iter().chain(others).any(|o| o != r && o.parent() == Some(dir));
            let deep = dir.components().count() > WRITE_DEPTH || tmp.iter().any(|t| dir.parent() == Some(t));
            sibling
                && keepable(dir, home)
                && deep
                && home.is_none_or(|h| dir.parent() != Some(h))
                && !roots.iter().any(|t| t == dir)
        });
        let place = folder.map_or_else(|| r.clone(), Path::to_path_buf);
        if !out.contains(&place) {
            out.push(place);
        }
    }
    out
}

/// Folders of code read unasked, wherever they are (#112): the built-in packs' `[claude] code-trees` (the Rust
/// toolchains' own sources, `{rustup}/toolchains/*/lib/rustlib/src`, and the crates cargo downloaded,
/// `{cargo}/registry/src`), `vars` their places, a `*` each folder that's there — real paths, those that are
/// there. Never a tool's home itself (cargo's `credentials.toml`) nor anything else in it: a link in these
/// trees that leads out is judged where it leads.
pub fn code_trees(vars: &crate::sandboxpack::Vars) -> Vec<PathBuf> {
    let trees = crate::sandboxpack::get().code_trees.iter().filter_map(|t| vars.place(t));
    let mut out: Vec<PathBuf> = Vec::new();
    for t in trees {
        let mut at = vec![PathBuf::new()];
        for c in t.components() {
            at = match c.as_os_str() == "*" {
                true => at
                    .iter()
                    .flat_map(|a| std::fs::read_dir(a).into_iter().flatten().flatten().map(|e| e.path()))
                    .collect(),
                false => at.into_iter().map(|a| a.join(c)).collect(),
            };
        }
        out.extend(at);
    }
    out.into_iter().filter_map(|p| std::fs::canonicalize(p).ok()).filter(|p| p.is_dir()).collect()
}

/// What a read of Claude's may read unasked besides the project and what the user allowed, by kind
/// (`cards::class` sorts places by these). Real paths.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Defaults {
    /// The repository's other checkouts (`checkouts` — 8 of a day's 14 reads allowed for a chat were
    /// these: the main checkout's `target/`, a sibling session's design doc).
    pub checkouts: Vec<PathBuf>,
    /// The temp folders (`temp_folders_in` — what Claude's own shots and scripts left there in an earlier turn
    /// were the other 6).
    pub temp: Vec<PathBuf>,
    /// The code folders (`code_trees`) and the project's own build folders outside it, as far as the
    /// sandbox has found them (`sandbox::build_dirs` — its target and build dirs: Claude's runs write
    /// there already).
    pub code: Vec<PathBuf>,
    /// What runs never read (`sandbox::kept_places`: keys, tokens, Claude's and tarae's state, the toolchains'
    /// credentials) wherever the variables put it — outside home too: never a default, whatever its class.
    pub kept: Vec<PathBuf>,
}

/// (Tests: `defaults_in` for home and the variables as the process has them.)
#[cfg(test)]
pub fn defaults(root: &Path, temp: bool) -> Defaults {
    defaults_in(root, temp, home().as_deref(), &crate::sandbox::Here::now())
}

/// The places Claude reads unasked for the project at `root` (`Defaults`; `caps::Basis::build_in` makes them
/// the session's), for home `h` and what `here` says of the variables and the temp folder; the temp folders
/// only with `temp` (`Editor.read_temp` — tests' projects are in the temp folder, and theirs off). On a worker.
pub fn defaults_in(root: &Path, temp: bool, h: Option<&Path>, here: &crate::sandbox::Here) -> Defaults {
    let mut code = h.map(|h| code_trees(&crate::sandbox::vars_in(h, here))).unwrap_or_default();
    // (a cargo config can point them anywhere — and a checkout can be anywhere: never home or above it,
    // nor where keys are)
    code.extend(crate::sandbox::build_dirs(root, here).into_iter().filter(|d| keepable(d, h)));
    let checkouts = checkouts(root).into_iter().filter(|d| keepable(d, h)).collect();
    let temp = if temp { temp_folders_in(here) } else { Vec::new() };
    let kept = h.map(|h| crate::sandbox::kept_places(h, here)).unwrap_or_default();
    Defaults { checkouts, temp, code, kept }
}

/// `/tmp` (macOS's `/private/tmp`) and the user's temp folder as `here` says it (`$TMPDIR` itself — not its
/// parent, which holds apps' caches), real paths.
fn temp_folders_in(here: &crate::sandbox::Here) -> Vec<PathBuf> {
    let t = here.tmp.clone().unwrap_or_else(|| crate::sandbox::profile_path(&std::env::temp_dir()));
    let mut v = vec![crate::sandbox::profile_path(Path::new("/tmp"))];
    if !v.contains(&t) {
        v.push(t);
    }
    v
}

/// The other checkouts of the repository whose checkout is `root` (the folder holding its `.git`), as git's
/// own files say — no git run: the main checkout and each `git worktree add`ed one, real paths, `root` not
/// among them. Each is taken only when git's links go both ways (a checkout's `.git` file names its folder
/// under `<common>/worktrees/`, whose `gitdir` names that `.git` file back): a `.git/worktrees` entry that
/// came in a download of a repository can't make some other folder read as one. Nothing for a folder
/// inside a checkout, or not in one. On a worker.
pub fn checkouts(root: &Path) -> Vec<PathBuf> {
    let Ok(root) = std::fs::canonicalize(root) else { return Vec::new() };
    let Some(common) = common_dir(&root) else { return Vec::new() };
    let mut out = Vec::new();
    // (the main checkout: what holds the common folder, when that's its `.git`)
    if common.file_name().is_some_and(|n| n == ".git")
        && let Some(main) = common.parent()
    {
        out.push(main.to_path_buf());
    }
    for entry in std::fs::read_dir(common.join("worktrees")).into_iter().flatten().flatten() {
        let Some(dot) = linked_to(&entry.path()) else { continue };
        if let Some(checkout) = dot.parent()
            && worktree_dir(checkout).is_some_and(|d| same(&d, &entry.path()))
        {
            out.push(checkout.to_path_buf());
        }
    }
    let mut seen = Vec::new();
    for p in out {
        if let Ok(p) = std::fs::canonicalize(p)
            && p != root
            && p.is_dir()
            && !seen.contains(&p)
        {
            seen.push(p);
        }
    }
    seen
}

/// The repository's common git folder for the checkout at `root` (real): its `.git` folder, or — a linked
/// checkout — the `commondir` of the folder its `.git` file names, when that folder names it back.
pub(crate) fn common_dir(root: &Path) -> Option<PathBuf> {
    let dot = root.join(".git");
    if dot.is_dir() {
        return std::fs::canonicalize(dot).ok();
    }
    let dir = worktree_dir(root)?;
    if !linked_to(&dir).is_some_and(|d| same(&d, &dot)) {
        return None;
    }
    let common = std::fs::read_to_string(dir.join("commondir")).ok()?;
    std::fs::canonicalize(dir.join(common.trim())).ok()
}

/// The folder a checkout's `.git` file names (`gitdir: …`, relative to the checkout or absolute).
fn worktree_dir(checkout: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(checkout.join(".git")).ok()?;
    let at = text.lines().next()?.strip_prefix("gitdir:")?.trim();
    Some(checkout.join(at))
}

/// The `.git` file a folder under `<common>/worktrees/` names (its `gitdir`).
fn linked_to(dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(dir.join("gitdir")).ok()?;
    let at = text.lines().next()?.trim();
    (!at.is_empty()).then(|| dir.join(at))
}

/// Whether two paths are the same place (real paths).
fn same(a: &Path, b: &Path) -> bool {
    matches!((std::fs::canonicalize(a), std::fs::canonicalize(b)), (Ok(a), Ok(b)) if a == b)
}

/// The real folder each of the `$(…)` words in `cmd` gives where it runs (`cwd`; `readonly::expansions` — the
/// packs' `expand`): what its pack runs says it — `$(rustc --print sysroot)` as rustup says it (`rustup show
/// active-toolchain` reads the toolchain files; nothing of the project's runs) — one of the folders directly
/// in its `first-word-in` that's there; one it can't tell is left out. On a worker.
pub fn expanded(cmd: &str, cwd: &Path) -> Vec<(String, PathBuf)> {
    let t = crate::sandboxpack::get();
    let named = t.expand.iter().filter(|e| cmd.contains(&e.word));
    let vars = home().map(|h| crate::sandbox::vars_in(&h, &crate::sandbox::Here::now()));
    named
        .filter_map(|e| {
            let out = std::process::Command::new(e.run.first()?)
                .args(&e.run[1..])
                .current_dir(cwd)
                .envs(e.env.iter().map(|(k, v)| (k, v)))
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .ok()?;
            let folder = vars.as_ref()?.place(&e.first_word_in)?;
            Some((e.word.clone(), named_in(&String::from_utf8_lossy(&out.stdout), &folder)?))
        })
        .collect()
}

/// The folder `said` names first (rustup's `1.99.0-aarch64-apple-darwin (overridden by …)`), as one directly in
/// `dir` that's there (real path) — a name that isn't one (a path, `..`) is None.
pub fn named_in(said: &str, dir: &Path) -> Option<PathBuf> {
    let name = said.split_whitespace().next()?;
    let plain = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || "-._".contains(c));
    if !plain || name.starts_with('.') {
        return None;
    }
    let dir = std::fs::canonicalize(dir).ok()?;
    let found = std::fs::canonicalize(dir.join(name)).ok()?;
    (found.parent() == Some(dir.as_path()) && found.is_dir()).then_some(found)
}

/// Home, as the process sees it.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_absolute())
}

/// Keeps `change` for the project (the session's root, resolved on the worker as a run's policy resolves it —
/// the real path the next runs read the kept answers by: `tools::guard_then_go`), on a worker (an error says so).
pub fn keep_later(ed: &mut crate::editor::Editor, change: impl FnOnce(&mut Allowed) + Send + 'static) {
    keep_later_then(ed, change, |_| {});
}

/// `keep_later`, and `then` once it's written or failed — at once when nothing is kept (tests).
fn keep_later_then(
    ed: &mut crate::editor::Editor,
    change: impl FnOnce(&mut Allowed) + Send + 'static,
    then: impl FnOnce(&mut crate::editor::Editor) + Send + 'static,
) {
    let Some(path) = ed.allow_path.clone() else { return then(ed) };
    let project = ed.session.root.clone();
    ed.events.jobs().spawn(move || {
        let r = keep(&path, &crate::sandbox::profile_path(&project), change);
        move |ed: &mut crate::editor::Editor| {
            then(ed);
            match r {
                Err(e) => ed.set_error(format!("couldn't keep it: {e}")),
                // (the chat's copy of the keys kept open — the message's context reads it, `reads::key_shut`)
                Ok(_) => {
                    crate::reads::kept_keys_load(ed);
                    ed.caps_sync();
                }
            }
        }
    });
}

/// `grants` for `scope` past once: this conversation's (`Chat.allowed`), this session's (`Session.allowed`,
/// kept with it), or the project's (kept on a worker, and the conversation's meanwhile) — the session's
/// `Caps` made again.
fn add_for(ed: &mut crate::editor::Editor, grants: &[Grant], scope: Scope) {
    match scope {
        Scope::Once => return,
        Scope::Session => {
            grants.iter().for_each(|g| ed.session.allowed.add(g));
            ed.sessions_keep();
        }
        Scope::ThisChat | Scope::Always => {
            if let Some(c) = ed.session.chat.as_mut() {
                grants.iter().for_each(|g| c.allowed.add(g));
            }
        }
    }
    if scope == Scope::Always {
        let grants = grants.to_vec();
        keep_later(ed, move |a| grants.iter().for_each(|g| a.add(g)));
    }
    ed.caps_sync();
}

/// How long `scope` lasts, as Claude hears it and as the toast ends.
fn lasting(scope: Scope) -> (&'static str, &'static str) {
    match scope {
        Scope::Always => ("in this project, until they take it back", " in this project — :granted lists it"),
        Scope::Session => {
            ("in this session, until it ends or they take it back", " in this session — :granted lists it")
        }
        Scope::ThisChat | Scope::Once => ("for the rest of this conversation", " in this conversation"),
    }
}

/// `a`/`s`/`A` on a card: Claude may read `places` (real paths) for the rest of this conversation, this
/// session, or always in this project — said in a toast.
pub fn grant_reads(ed: &mut crate::editor::Editor, places: &[PathBuf], scope: Scope) {
    if scope == Scope::Once || places.is_empty() {
        return;
    }
    let grants: Vec<Grant> = places.iter().cloned().map(Grant::Read).collect();
    add_for(ed, &grants, scope);
    let shown = places.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ");
    ed.set_success(format!("Claude may read {shown}{}", lasting(scope).1));
}

/// `a`/`A` on a read card naming files in the project named like keys (`reads.rs`): Claude may read `keys`
/// (real paths), and its commands too — `Grant::Key`, the one the run's panel gives — for the rest of this
/// conversation, or always in this project (kept on a worker) — said in a toast.
pub fn grant_keys(ed: &mut crate::editor::Editor, keys: &[PathBuf], scope: Scope) {
    if scope == Scope::Once || keys.is_empty() {
        return;
    }
    // (no longer than a key may be kept: `Caps::storable`)
    let grants: Vec<Grant> = keys.iter().map(|k| Grant::Key(k.clone())).collect();
    let scope = grants.iter().map(crate::caps::Caps::storable).fold(scope, Scope::min);
    if scope == Scope::Once {
        return;
    }
    add_for(ed, &grants, scope);
    let shown = keys.iter().map(|k| tilde(k)).collect::<Vec<_>>().join(", ");
    let (when, toast) = lasting(scope);
    // (Claude's read was answered; that its commands may read it too is news to it — as `grant` says)
    let tell = format!("The user let you and your commands read {shown}, named like a key — {when}.");
    ed.session.checkpoints.tell.push(tell);
    ed.set_success(format!("Claude and its commands may read {shown}{toast}"));
}

/// `a`/`s`/`A` on a card about what the sandbox blocked: Claude's commands may do `g` for the rest of this
/// conversation, this session, or always in this project — and Claude hears it with the next message.
pub fn grant(ed: &mut crate::editor::Editor, g: &Grant, scope: Scope) {
    // (no longer than it may be kept: Docker's socket is as much as the sandbox off — never past one run, #193;
    // another socket this conversation's at most)
    let scope = scope.min(crate::caps::Caps::storable(g));
    if scope == Scope::Once {
        return;
    }
    add_for(ed, std::slice::from_ref(g), scope);
    let (when, toast) = lasting(scope);
    ed.session.checkpoints.tell.push(format!("The user let your commands {} — {when}.", g.shown()));
    ed.set_success(format!("Claude's commands may {}{toast}", g.shown()));
}

impl crate::editor::Editor {
    /// `:sandbox-disallow <path | port N | socket path | host name:port>` — taken back (this conversation, this
    /// session and kept): Claude asks again before reading there, its commands can't write there or connect there, a
    /// host is asked again (its runs going on too — the proxy is told).
    pub fn sandbox_disallow(&mut self, what: &str) -> Result<(), String> {
        let what = what.trim().to_string();
        let host = crate::hosts::unlisted(&what);
        if !what.starts_with('/')
            && !what.starts_with("port ")
            && !what.starts_with("socket /")
            && what != TERMINALS_LISTED
            && what != LOCALHOST_LISTED
            && !what.starts_with("env ")
            && host.is_none()
        {
            return Err(
                "usage: :sandbox-disallow <path | port N | socket path | terminals | localhost | env NAME | host \
                        name:port> — as :granted lists it"
                    .into(),
            );
        }
        // (what goes, as Claude was told it: every answer `what` names, whichever scope — the session's `Caps` has
        // the kept ones too)
        let gone: Vec<String> = self.session.caps().map_or_else(Vec::new, |c| {
            let named = |g: &Grant| {
                let mut one = Allowed::default();
                one.add(g);
                one.remove(&what);
                one.is_empty()
            };
            let mut gone: Vec<String> = c.grants().iter().filter(|g| named(g)).map(Grant::shown).collect();
            gone.dedup();
            gone
        });
        if let Some(c) = self.session.chat.as_mut() {
            c.allowed.remove(&what);
        }
        let had = self.session.allowed.clone();
        self.session.allowed.remove(&what);
        if self.session.allowed != had {
            self.sessions_keep();
        }
        if let Some((h, p)) = host {
            self.session.hosts.retain(|(sh, sp)| (sh, *sp) != (&h, p));
            crate::hosts::taken(&self.session, &h, p);
        }
        // (a run's `Caps` made before the write lands leaves it out too: `caps::Basis.taking`)
        self.allow_taking.push(what.clone());
        let (taken, landed) = (what.clone(), what.clone());
        keep_later_then(self, move |a| a.remove(&taken), move |ed| ed.allow_taking.retain(|w| *w != landed));
        self.caps_sync();
        let said = if gone.is_empty() { what.clone() } else { gone.join("; ") };
        self.session.checkpoints.tell.push(format!(
            "The user took back what they had let you and your commands do: {said} — it's asked again from the \
             next run and the next read on."
        ));
        self.set_success(format!("Taken back: {said}"));
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// f22 f: a container engine's socket is the containers pack's to name — by its file's name or a folder on
    /// its path — and a grant of one warns in its words; any other socket doesn't.
    #[test]
    fn engine_sockets_are_the_containers_packs() {
        let engines = &crate::sandboxpack::get().engine_sockets;
        assert!(!engines.is_empty(), "the containers pack's");
        let said = |p: &str| Grant::Socket(PathBuf::from(p)).warning();
        for p in [
            "/run/docker.sock",
            "/u/.colima/default/x.sock",
            "/u/.docker/run/x.sock",
            "/u/.orbstack/run/x.sock",
        ] {
            assert!(said(p).is_some_and(|w| w.contains("mounts any folder")), "{p}");
            assert!(Grant::Socket(PathBuf::from(p)).as_outside(), "{p}");
        }
        assert_eq!(said("/u/p/.tarae/x.sock"), None);
        assert_eq!(said("/u/p/docker.sock.bak"), None);
        let own = [crate::sandboxpack::EngineSocket {
            names: vec!["podman.sock".into()],
            folders: vec![".lima".into()],
            say: "podman's".into(),
        }];
        assert_eq!(engine_socket(Path::new("/r/podman.sock"), &own), Some("podman's"));
        assert_eq!(engine_socket(Path::new("/u/.lima/a/b.sock"), &own), Some("podman's"));
        assert_eq!(engine_socket(Path::new("/run/docker.sock"), &own), None, "only what's named");
    }

    #[test]
    fn a_place_covers_what_is_in_it() {
        let mut a = Allowed::default();
        a.add_read("/t/x/f.txt".into());
        assert!(a.reads(Path::new("/t/x/f.txt")) && !a.reads(Path::new("/t/x/g.txt")));
        a.add_read("/t/x".into());
        assert_eq!(a.read, [PathBuf::from("/t/x")], "the folder takes the file's place");
        a.add_read("/t/x/y".into());
        assert_eq!(a.read.len(), 1, "covered already");
        assert!(a.reads(Path::new("/t/x/y/z")) && !a.reads(Path::new("/t/xy")));
        // Writes and ports alike, and taken back as listed
        a.add(&Grant::Write("/w/a".into()));
        a.add(&Grant::Write("/w/a/b".into()));
        a.add(&Grant::Port(5432));
        a.add(&Grant::Port(5432));
        assert_eq!((a.write.len(), a.ports.clone()), (1, vec![5432]));
        a.remove("port 5432");
        a.remove("/w/a");
        assert!(a.write.is_empty() && a.ports.is_empty() && !a.read.is_empty());
    }

    #[test]
    fn kept_per_project_and_read_back() {
        let dir = crate::scratch::Scratch::new("allow");
        let path = dir.join("state/sandbox.json");
        keep(&path, Path::new("/p/one"), |a| a.add_read("/t/a".into())).unwrap();
        keep(&path, Path::new("/p/two"), |a| a.add_read("/t/b".into())).unwrap();
        keep(&path, Path::new("/p/one"), |a| a.add_read("/t/c/f".into())).unwrap();
        keep(&path, Path::new("/p/one"), |a| a.add(&Grant::Write("/t/w".into()))).unwrap();
        keep(&path, Path::new("/p/one"), |a| a.add(&Grant::Port(3000))).unwrap();
        let s = Store::read(Some(&path));
        let one = s.get(Path::new("/p/one"));
        assert_eq!(one.read, [PathBuf::from("/t/a"), PathBuf::from("/t/c/f")]);
        assert_eq!((one.write, one.ports), (vec![PathBuf::from("/t/w")], vec![3000]));
        assert_eq!(s.get(Path::new("/p/two")).read, [PathBuf::from("/t/b")]);
        assert!(s.get(Path::new("/p/three")).is_empty());
        // Taken back: the project goes from the file
        keep(&path, Path::new("/p/two"), |a| a.read.clear()).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("/p/two"));
    }

    /// A project's hosts (the card's `A`) are kept in `sandbox.json` as `host:port`, read back into the next run's
    /// rules (`with_allowed` → `Egress::answered`, named among what was granted), and taken back by
    /// `:sandbox-disallow` — from what's kept and the session's, the proxy told (`Taken`) so the runs going on ask
    /// again. A hand-edited entry the proxy can't read (a pattern, an address, no port) is left out.
    #[test]
    fn a_projects_hosts_are_kept_and_read_back() {
        let dir = crate::scratch::Scratch::new("allow-hosts");
        std::fs::create_dir_all(dir.join("p")).unwrap();
        let root = std::fs::canonicalize(dir.join("p")).unwrap();
        let path = dir.join("state/sandbox.json");
        keep(&path, &root, |a| a.add(&Grant::Host("static.example.invalid".into(), 443))).unwrap();
        keep(&path, &root, |a| a.add_host("static.example.invalid", 443)).unwrap();
        let kept = Store::read(Some(&path)).get(&root);
        assert_eq!(kept.hosts, [("static.example.invalid".to_string(), 443)], "once");
        let once = Grant::Host("once.example.invalid".into(), 8443);
        // (a project's kept host is a host list's: the network open — piece C)
        let w = crate::tools::with_allowed(
            &crate::llm::LlmConfig { sandbox_network: true, ..Default::default() },
            &crate::caps::Caps::answered(&[&kept]),
            Some(&once),
        );
        assert!(w.granted.contains(&"reach static.example.invalid:443 through tarae's proxy".to_string()));
        let mut p = crate::sandbox::Policy::for_project(&crate::sandbox::Env {
            root: root.clone(),
            network: true,
            hosts: Some(Vec::new()),
            ..Default::default()
        });
        w.onto(&mut p);
        let rules = &p.egress;
        assert!(
            rules.answered("static.example.invalid", 443) && rules.answered("once.example.invalid", 8443)
        );
        assert!(!rules.answered("static.example.invalid", 8443), "at its port");
        // A hand edit: only what the proxy reads
        let mut v: Json = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let entries = v["projects"][root.to_string_lossy().as_ref()].as_array_mut().unwrap();
        for what in ["*.example.invalid:443", "1.2.3.4:443", "bare.example.invalid"] {
            entries.push(json!({ "kind": "host", "what": what, "scope": "always", "granted": 1 }));
        }
        std::fs::write(&path, v.to_string()).unwrap();
        assert_eq!(Store::read(Some(&path)).get(&root).hosts.len(), 1);
        // Taken back
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        (ed.session.root, ed.allow_path) = (root.clone(), Some(path.clone()));
        ed.session.hosts.push(("static.example.invalid".into(), 443));
        crate::hosts::SENT.with(|s| s.borrow_mut().clear());
        ed.sandbox_disallow("host static.example.invalid:443").unwrap();
        assert!(ed.session.hosts.is_empty(), "the session's");
        let taken = crate::hosts::SENT.with(|s| {
            s.borrow().iter().any(|m| {
                matches!(m, crate::sandbox::proxy::ToProxy::Taken { session, host, port: 443 }
                    if *session == crate::sandbox::proxy::session_of(&root) && host == "static.example.invalid")
            })
        });
        assert!(taken, "the proxy told");
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !Store::read(Some(&path)).get(&root).hosts.is_empty() {
            assert!(std::time::Instant::now() < end, "what's kept, taken back");
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(50)) {
                ed.handle_event(ev);
            }
        }
        assert!(ed.sandbox_disallow("host *.example.invalid:443").is_err(), "a host:port, as listed");
    }

    #[test]
    fn home_above_it_and_key_folders_are_never_kept() {
        let home = Some(Path::new("/Users/u"));
        assert!(keepable(Path::new("/private/tmp/x/marker.txt"), home));
        assert!(keepable(Path::new("/Users/u/src/other"), home));
        assert!(!keepable(Path::new("/Users/u"), home), "home is everything");
        assert!(!keepable(Path::new("/Users"), home));
        assert!(!keepable(Path::new("/"), home));
        assert!(!keepable(Path::new("/Users/u/.ssh/id_ed25519"), home));
        assert!(!keepable(Path::new("/Users/u/.config/gh"), home));
        assert!(keepable(Path::new("/Users/u/.sshrc"), home), "by path parts, not by prefix");
    }

    /// Two reads in a folder offer the folder — on Linux too, where `/tmp/x` is a level above macOS's
    /// `/private/tmp/x`: a folder in a temp folder is deep enough; one in `/` (Linux's temp's parent) isn't,
    /// and the temp folders themselves never are.
    #[test]
    fn a_folder_in_tmp_is_offered_on_linux_too() {
        let p = |s: &str| PathBuf::from(s);
        let linux = |reads: &[PathBuf]| {
            grant_places_in(reads, &[], Some(Path::new("/home/u")), Path::new("/tmp"), Path::new("/tmp"))
        };
        assert_eq!(linux(&[p("/tmp/shots/a.png"), p("/tmp/shots/b.png")]), [p("/tmp/shots")]);
        let mac = grant_places_in(
            &[p("/private/tmp/shots/a.png"), p("/private/tmp/shots/b.png")],
            &[],
            Some(Path::new("/Users/u")),
            Path::new("/private/var/folders/xx/yyy/T"),
            Path::new("/private/tmp"),
        );
        assert_eq!(mac, [p("/private/tmp/shots")]);
        // A whole area stays each file: /etc's folders, /tmp itself
        assert_eq!(linux(&[p("/etc/x/a"), p("/etc/x/b")]), [p("/etc/x/a"), p("/etc/x/b")]);
        assert_eq!(linux(&[p("/tmp/a"), p("/tmp/b")]), [p("/tmp/a"), p("/tmp/b")]);
    }

    /// #112: the code folders are the toolchains' sources and cargo's downloaded crates — never cargo's own
    /// files beside them (its credentials) — and a read of one goes unasked while a link in them that
    /// leads out is judged where it leads. The sysroot rustup names is one of its toolchains, or none.
    #[test]
    fn code_folders_are_the_sources_and_nothing_beside_them() {
        let base = crate::scratch::Scratch::new("code");
        let (cargo, rustup) = (base.join("cargo"), base.join("rustup"));
        let krate = cargo.join("registry/src/index.crates.io-1949cf8c6b5b557f/libc-0.2.170/src");
        std::fs::create_dir_all(&krate).unwrap();
        std::fs::create_dir_all(rustup.join("toolchains/stable-x/lib/rustlib/src/rust")).unwrap();
        std::fs::create_dir_all(rustup.join("toolchains/bare-y/lib")).unwrap();
        std::fs::create_dir_all(base.join("p")).unwrap();
        std::fs::write(krate.join("lib.rs"), "x\n").unwrap();
        std::fs::write(cargo.join("credentials.toml"), "marker\n").unwrap();
        std::os::unix::fs::symlink(cargo.join("credentials.toml"), krate.join("leak.rs")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let (cargo, rustup, p) = (base.join("cargo"), base.join("rustup"), base.join("p"));
        // (the rust pack's code trees, its homes where `CARGO_HOME` and `RUSTUP_HOME` put them)
        let var = |n: &str| match n {
            "CARGO_HOME" => Some(cargo.clone()),
            "RUSTUP_HOME" => Some(rustup.clone()),
            _ => None,
        };
        let vars =
            crate::sandboxpack::Vars::new(crate::sandboxpack::get(), &base, &var, crate::sandbox::Os::HOST);
        let mut trees = code_trees(&vars);
        trees.sort();
        assert_eq!(
            trees,
            [cargo.join("registry/src"), rustup.join("toolchains/stable-x/lib/rustlib/src")],
            "a toolchain without its sources adds nothing"
        );
        let kept = Allowed { read: trees, ..Default::default() };
        let none = Allowed::default();
        let at = |c: &str| crate::readonly::outside(c, &p, &p, &none, &kept, &[]).unwrap();
        let lib = krate_at(&cargo).join("lib.rs");
        assert!(at(&format!("sed -n '1,20p' {}", lib.display())).is_empty());
        assert!(at(&format!("grep -rn fn {}", cargo.join("registry/src").display())).is_empty());
        let creds = cargo.join("credentials.toml");
        assert_eq!(at(&format!("cat {}", creds.display())), std::slice::from_ref(&creds));
        assert_eq!(
            at(&format!("cat {}", krate_at(&cargo).join("leak.rs").display())),
            std::slice::from_ref(&creds)
        );
        assert_eq!(
            at(&format!("ls {}", cargo.display())),
            std::slice::from_ref(&cargo),
            "the cargo home itself"
        );
        // The sysroot rustup names: one of its toolchains that's there (the rust pack's `first-word-in`)
        let tcs = vars.place(&crate::sandboxpack::get().expand[0].first_word_in).unwrap();
        assert_eq!(tcs, rustup.join("toolchains"));
        let tc = rustup.join("toolchains/stable-x");
        assert_eq!(named_in("stable-x (overridden by '/p/rust-toolchain.toml')\n", &tcs), Some(tc));
        for said in ["", "nope-z", "..", "../../cargo", "/tmp/tc (path)", "stable-x/../bare-y"] {
            assert_eq!(named_in(said, &tcs), None, "{said}");
        }
    }

    /// A real repository in `base`: `main` with a commit, and `wt` added as a linked checkout of it (git's
    /// own files, as `git worktree add` writes them). Real paths.
    pub(crate) fn repo_with_worktree(base: &Path) -> (PathBuf, PathBuf) {
        let git = |dir: &Path, args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "init.defaultBranch=main"])
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        std::fs::create_dir_all(base.join("main")).unwrap();
        let main = std::fs::canonicalize(base.join("main")).unwrap();
        git(&main, &["init", "-q"]);
        git(&main, &["commit", "-q", "--allow-empty", "-m", "first"]);
        git(&main, &["worktree", "add", "-q", "../wt"]);
        (main, std::fs::canonicalize(base.join("wt")).unwrap())
    }

    /// P0 of docs/design/cards.md: the repository's other checkouts are read unasked, from either one —
    /// as git's own files say, only where the links go both ways: an entry under `.git/worktrees` naming a
    /// folder whose `.git` doesn't name it back (one that came in a download) is no checkout; a folder inside
    /// a checkout, or in none, has none.
    #[test]
    fn the_repositorys_other_checkouts_both_ways_only() {
        let base = crate::scratch::Scratch::new("checkouts");
        let (main, wt) = repo_with_worktree(&base);
        assert_eq!(checkouts(&main), std::slice::from_ref(&wt));
        assert_eq!(checkouts(&wt), std::slice::from_ref(&main));
        // Planted: an entry naming a folder that has no `.git`, and one whose `.git` names somewhere else
        let (victim, other) = (base.join("victim"), base.join("other"));
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join(".git"), format!("gitdir: {}\n", base.join("nowhere").display())).unwrap();
        for (name, to) in [("evil", &victim), ("evil2", &other)] {
            let e = main.join(".git/worktrees").join(name);
            std::fs::create_dir_all(&e).unwrap();
            std::fs::write(e.join("gitdir"), format!("{}\n", to.join(".git").display())).unwrap();
            std::fs::write(e.join("commondir"), "../..\n").unwrap();
        }
        assert_eq!(checkouts(&main), std::slice::from_ref(&wt), "planted entries aren't checkouts");
        // A checkout whose `.git` file names an entry that doesn't name it back has no others
        std::fs::write(other.join(".git"), format!("gitdir: {}\n", main.join(".git/worktrees/wt").display()))
            .unwrap();
        assert!(checkouts(&other).is_empty(), "one-way: no repository");
        std::fs::create_dir_all(main.join("src")).unwrap();
        assert!(checkouts(&main.join("src")).is_empty(), "a folder inside a checkout");
        assert!(checkouts(&victim).is_empty(), "no repository");
        // The temp folders only when asked for
        assert!(defaults(&main, false).temp.is_empty());
        let t = defaults(&main, true).temp;
        assert!(t.contains(&crate::sandbox::profile_path(&std::env::temp_dir())), "{t:?}");
        assert!(t.contains(&crate::sandbox::profile_path(Path::new("/tmp"))), "{t:?}");
    }

    fn krate_at(cargo: &Path) -> PathBuf {
        cargo.join("registry/src/index.crates.io-1949cf8c6b5b557f/libc-0.2.170/src")
    }

    /// The places offered for a read the sandbox denied (card model P2): a file right in home itself, else its
    /// folder and those above it up to just below home, nearest first — never home, a place outside it, keys,
    /// nor a place kept from every run or a folder holding one. (Paths only; nothing is read.)
    #[test]
    fn which_places_a_denied_read_may_open() {
        let home = Path::new("/Users/u");
        let kept = [home.join(".ssh"), home.join(".config/gh"), home.join(".local/state/tarae")];
        let choices = |p: &str| read_choices(&home.join(p), home, &kept);
        assert_eq!(choices(".zshenv"), [home.join(".zshenv")], "a file right in home: itself");
        assert_eq!(
            choices("src/lib/a/b/c/d.rs"),
            ["src/lib/a/b/c", "src/lib/a/b", "src/lib/a", "src/lib"].map(|p| home.join(p)),
            "its folder and those above, nearest first, four at most"
        );
        assert_eq!(choices("src/x.rs"), [home.join("src")]);
        for p in
            [".ssh/id_ed25519", ".config/gh/hosts.yml", ".aws", ".aws/credentials", ".local/state/tarae/x"]
        {
            assert!(choices(p).is_empty(), "{p}: {:?}", choices(p));
        }
        // (not `.local` either: it holds tarae's state)
        assert_eq!(choices(".local/share/x/y"), [home.join(".local/share/x"), home.join(".local/share")]);
        assert!(
            read_choices(home, home, &kept).is_empty()
                && read_choices(Path::new("/opt/x/y"), home, &kept).is_empty()
        );
    }

    /// The folders offered for a denied write: its nearest existing folder up to just below home — never
    /// home or above, keys, a place that runs something later or a folder holding one, where programs the user
    /// runs are (the PATH's folders, `~/.local/bin`, the packs' `bins`) or what holds them, a toolchain's home
    /// or anything in it (at its default or moved) — npm's too: what `npx` fetched runs from its `_npx` — or
    /// the project. (A made-up home in the test's temp folder.)
    #[test]
    fn which_folders_a_denied_write_may_open() {
        let base = crate::scratch::Scratch::new("choices");
        let home = base.join("home");
        for d in [
            ".npm/_cacache",
            ".npm/_npx/0a1b/node_modules/x",
            "opt/npm/_npx/2c3d",
            "data/new",
            ".local/bin",
            ".config/tarae",
            "Library/LaunchAgents",
            "work/proj",
            ".cargo/registry/index/i/.cache",
            ".cargo/registry/cache/i",
            ".cargo/git/db/dep-1",
            ".cargo/git/checkouts/dep-1/abc",
            ".cargo/bin",
            "go/bin",
            ".gradle/caches/8.10/scripts",
            ".gradle/wrapper/dists/gradle-8.10-bin/x",
            ".rustup/toolchains/x/bin",
            "tools/bin",
            "opt/cargo/bin",
        ] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        let home = std::fs::canonicalize(&home).unwrap();
        let root = home.join("work/proj");
        // (cargo's home and npm's moved out of their default places)
        let cargo = home.join("opt/cargo");
        let here = crate::sandbox::Here {
            env: [
                ("HOME".to_string(), home.clone().into()),
                ("CARGO_HOME".to_string(), cargo.clone().into()),
                ("npm_config_cache".to_string(), home.join("opt/npm").into()),
            ]
            .into(),
            ..Default::default()
        };
        let path = [home.join("tools/bin")];
        let later = crate::sandbox::RunsLater::new(&home, &crate::sandbox::vars_in(&home, &here), &path);
        let at = |t: &str| write_choices(&home.join(t), &home, &root, &later);
        // (the file itself, and a folder not there yet, aren't offered — the nearest that is, and up)
        assert_eq!(at("data/new/x/y.json"), [home.join("data/new"), home.join("data")]);
        // (npm's home, at its default and moved: what `npx` fetched runs from there later)
        assert!(at(".npm/_cacache/new/x.json").is_empty(), "{:?}", at(".npm/_cacache/new/x.json"));
        assert!(at(".npm/_npx/0a1b/node_modules/x/index.js").is_empty(), "npx's packages");
        assert!(at("opt/npm/_npx/2c3d/x").is_empty(), "a moved npm home");
        assert!(at(".zshrc").is_empty(), "in home itself");
        assert!(at(".ssh/known_hosts").is_empty(), "keys");
        assert!(at(".local/bin/tool").is_empty(), "~/.local/bin — and .local holds it");
        assert!(at("tools/bin/tool").is_empty(), "on PATH — and tools holds it");
        assert!(at(".cargo/bin/tool").is_empty(), "cargo's programs");
        assert!(at("go/bin/tool").is_empty(), "go's programs — and go is its home");
        // (in a toolchain's home: what the user's own builds and proxies run later)
        assert!(at(".gradle/caches/8.10/scripts/x.class").is_empty(), "Gradle's compiled build scripts");
        assert!(at(".gradle/wrapper/dists/gradle-8.10-bin/x/g.zip").is_empty(), "what ./gradlew runs");
        assert!(at(".rustup/toolchains/x/bin/cargo").is_empty(), "what rustup's proxies run");
        assert!(at("opt/cargo/bin/tool").is_empty(), "a moved cargo home's programs — and its home");
        assert!(at("opt/cargo/x").is_empty(), "a moved cargo home");
        assert!(at(".config/tarae/config.toml").is_empty(), "tarae's own; .config holds it");
        assert!(at("Library/LaunchAgents/x.plist").is_empty(), "runs at login; Library holds it");
        assert!(at("work/proj/.git/index.lock").is_empty(), "the project");
        assert_eq!(at("work/other.txt"), Vec::<PathBuf>::new(), "work holds the project");
        // Cargo's caches: the profile keeps them while the network is closed, whatever is granted
        for t in [
            ".cargo/registry/index/i/.cache/se/rd/serde",
            ".cargo/registry/cache/i/serde-1.0.0.crate",
            ".cargo/registry/src/i/serde-1.0.0/build.rs",
            ".cargo/git/db/dep-1/HEAD",
            ".cargo/git/checkouts/dep-1/abc/src/lib.rs",
        ] {
            assert!(at(t).is_empty(), "{t}: {:?}", at(t));
        }
        // Outside home: a few, none shallow
        let deep = base.join("o/a/b/c/d");
        std::fs::create_dir_all(&deep).unwrap();
        let deep = std::fs::canonicalize(deep).unwrap();
        let got = write_choices(&deep.join("x"), &home, &root, &later);
        assert_eq!(got.len(), WRITE_CHOICES, "{got:?}");
        assert_eq!(got[0], deep);
        assert!(write_choices(Path::new("/opt/x/y"), &home, &root, &later).is_empty(), "{got:?}");
        // A folder refused itself: that folder first; named by `also`, that folder alone — never one above
        assert_eq!(at("data/new"), [home.join("data/new"), home.join("data")], "the folder itself first");
        let place = |t: &Path| write_place(t, &home, &root, &later);
        assert_eq!(place(&home.join("data/new")), Some(home.join("data/new")));
        assert_eq!(place(&home.join("data/new/x.json")), None, "not a folder");
        for t in [home.clone(), home.join(".local/bin"), home.join("work/proj"), home.join("opt/cargo/x")] {
            assert_eq!(place(&t), None, "{t:?}");
        }
    }

    /// A read refused at a folder (`space m s`) offers that folder first, then those above; named by `also`,
    /// `read_place` is that place alone, a folder or a file — never the folder above it, which may hold other
    /// projects and their tokens. (A made-up home in the test's temp folder.)
    #[test]
    fn a_folder_read_offers_the_folder_itself() {
        let base = crate::scratch::Scratch::new("read-place");
        std::fs::create_dir_all(base.join("home/Workspace/sandbox-runtime/src")).unwrap();
        std::fs::create_dir_all(base.join("home/.ssh")).unwrap();
        let home = std::fs::canonicalize(base.join("home")).unwrap();
        let kept = [home.join(".ssh")];
        let srt = home.join("Workspace/sandbox-runtime");
        assert_eq!(read_choices(&srt, &home, &kept), [srt.clone(), home.join("Workspace")]);
        assert_eq!(read_choices(&srt.join("README.md"), &home, &kept)[0], srt, "a file: its folder first");
        assert_eq!(read_choices(&home.join("Workspace"), &home, &kept), [home.join("Workspace")]);
        assert_eq!(read_place(&srt, &home, &kept), Some(srt.clone()));
        assert_eq!(read_place(&srt.join("README.md"), &home, &kept), Some(srt.join("README.md")));
        let above = home.parent().unwrap().to_path_buf();
        for t in
            [home.clone(), above, home.join(".ssh"), home.join(".ssh/id_ed25519"), PathBuf::from("/opt/x")]
        {
            assert_eq!(read_place(&t, &home, &kept), None, "{t:?}");
        }
    }

    /// cardauto 6: a `sandbox.json` of the old form (a list per kind, no times) is read with each answer at the
    /// file's time, and written in the new form at the next change — each entry with its scope and when it was
    /// granted, the old ones keeping the file's time, the new one its own.
    #[test]
    fn an_old_sandbox_json_reads_at_its_files_time_and_is_written_anew() {
        let dir = crate::scratch::Scratch::new("allow-old");
        let path = dir.join("sandbox.json");
        let old = json!({ "projects": { "/p": {
            "read": ["/t/r"], "write": ["/t/w"], "ports": [3000], "hosts": ["example.invalid:443"],
            "terminals": true, "keys": ["/p/.env"] } } });
        std::fs::write(&path, old.to_string()).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let at = modified.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let a = Store::read(Some(&path)).get(Path::new("/p"));
        let all = [
            Grant::Read("/t/r".into()),
            Grant::Write("/t/w".into()),
            Grant::Port(3000),
            Grant::Host("example.invalid".into(), 443),
            Grant::Terminals,
            Grant::Key("/p/.env".into()),
        ];
        assert_eq!(a.grants(), all);
        assert!(all.iter().all(|g| a.granted.get(g) == Some(&at)), "{:?}", a.granted);
        // Written anew at the next change
        std::thread::sleep(std::time::Duration::from_millis(1100));
        keep(&path, Path::new("/p"), |a| a.add_port(8080)).unwrap();
        let v: Json = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["version"], STORE_VERSION, "{v}");
        let entries = v["projects"]["/p"].as_array().unwrap();
        assert_eq!(entries.len(), 7, "{v}");
        assert!(entries.iter().all(|e| e["scope"] == "always" && e["granted"].is_u64()), "{v}");
        let b = Store::read(Some(&path)).get(Path::new("/p"));
        assert_eq!(b.grants()[..3], [all[0].clone(), all[1].clone(), Grant::Port(3000)]);
        assert_eq!(b.ports, [3000, 8080]);
        assert!(all.iter().all(|g| b.granted.get(g) == Some(&at)), "the file's time kept: {:?}", b.granted);
        assert!(b.granted[&Grant::Port(8080)] > at, "the new one's own");
        // Taken back: its time goes with it
        keep(&path, Path::new("/p"), |a| a.remove("port 8080")).unwrap();
        assert!(!Store::read(Some(&path)).get(Path::new("/p")).granted.contains_key(&Grant::Port(8080)));
    }

    /// cardauto 6: `s` — a grant for this session is the session's (not the conversation's, not the project's),
    /// cut to what it may be kept for, in the session's `Caps` and so in its runs' policy, and Claude hears it.
    #[test]
    fn a_session_answer_is_the_sessions_and_goes_no_further() {
        let mut ed = crate::editor::Editor::new(crate::config::Config::default());
        let g = Grant::Write("/t/w".into());
        grant(&mut ed, &g, Scope::Session);
        assert_eq!(ed.session.allowed.write, [PathBuf::from("/t/w")]);
        assert!(ed.session.allowed.granted.contains_key(&g), "with its time");
        assert!(ed.session.chat.as_ref().is_none_or(|c| c.allowed.is_empty()), "not the conversation's");
        let told = ed.session.checkpoints.tell.last().cloned().unwrap_or_default();
        assert!(told.contains("in this session, until it ends"), "{told}");
        let caps = ed.caps_basis().granted();
        assert!(caps.grants().contains(&g), "in the runs' policy");
        assert!(caps.write.iter().any(|e| e.source == crate::caps::Source::Grant(Scope::Session)));
        // Never past what it may be kept for: Docker's socket this run's, a key this run's, another socket the
        // conversation's
        grant(&mut ed, &Grant::Socket("/run/docker.sock".into()), Scope::Session);
        grant(&mut ed, &Grant::Key("/t/.env".into()), Scope::Session);
        grant(&mut ed, &Grant::Socket("/t/s.sock".into()), Scope::Session);
        assert_eq!(ed.session.allowed.grants(), [g], "nothing else the session's");
        // Taken back
        ed.sandbox_disallow("/t/w").unwrap();
        assert!(ed.session.allowed.is_empty() && ed.session.allowed.granted.is_empty());
    }
}
