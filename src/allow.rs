//! What the user lets Claude do past the defaults: read outside the project (`reads.rs`), and — from what
//! the sandbox blocked (`blocked.rs`) — let its commands write in a folder or connect to a port on this
//! machine. Each answer on its card is for once, for this conversation (`Chat.allowed`), or always: kept per
//! project (its real path) in `$state/sandbox.json` — the user's state, not the project's files (those go
//! into git, to other people, and would need trust again).
//!
//! A place that holds keys or is home itself (or above it) is only ever allowed to be read once
//! (`keepable`); a folder whose writing could run something later (`NEVER_WRITE`) isn't offered at all.

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
}

impl Allowed {
    /// Whether `real` (a real path) is one of the places allowed, or in one.
    pub fn reads(&self, real: &Path) -> bool {
        self.read.iter().any(|p| real.starts_with(p))
    }

    /// Adds a place (one already covered isn't added twice).
    pub fn add_read(&mut self, real: PathBuf) {
        add_place(&mut self.read, real);
    }

    pub fn add_write(&mut self, real: PathBuf) {
        add_place(&mut self.write, real);
    }

    pub fn add_port(&mut self, port: u16) {
        if !self.ports.contains(&port) {
            self.ports.push(port);
        }
    }

    pub fn add(&mut self, g: &Grant) {
        match g {
            Grant::Write(p) => self.add_write(p.clone()),
            Grant::Port(n) => self.add_port(*n),
        }
    }

    /// Takes back a place or a port (as `:sandbox-allowed` lists it).
    pub fn remove(&mut self, what: &str) {
        let at = Path::new(what);
        self.read.retain(|p| p != at);
        self.write.retain(|p| p != at);
        if let Some(n) = what.strip_prefix("port ").and_then(|n| n.parse::<u16>().ok()) {
            self.ports.retain(|p| *p != n);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.read.is_empty() && self.write.is_empty() && self.ports.is_empty()
    }
}

fn add_place(list: &mut Vec<PathBuf>, real: PathBuf) {
    if !list.iter().any(|p| real.starts_with(p)) {
        list.retain(|p| !p.starts_with(&real));
        list.push(real);
    }
}

/// What a command of Claude's may do more: write in a folder, connect to a port on this machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Grant {
    Write(PathBuf),
    Port(u16),
}

impl Grant {
    /// As the card and the toasts say it.
    pub fn shown(&self) -> String {
        match self {
            Grant::Write(p) => format!("write in {} and everything in it", tilde(p)),
            Grant::Port(n) => format!("connect to port {n} on this machine"),
        }
    }
}

/// `p` with home spelled `~`.
pub fn tilde(p: &Path) -> String {
    match home().and_then(|h| p.strip_prefix(&h).ok().map(Path::to_path_buf)) {
        Some(rest) if rest.as_os_str().is_empty() => "~".into(),
        Some(rest) => format!("~/{}", rest.display()),
        None => p.display().to_string(),
    }
}

/// How long an answer lasts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Scope {
    Once,
    ThisChat,
    Always,
}

/// The "always" answers, per project (real path) — `$state/sandbox.json`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Store {
    projects: BTreeMap<PathBuf, Allowed>,
}

impl Store {
    pub fn path() -> Option<PathBuf> {
        crate::config::state_dir().map(|d| d.join("sandbox.json"))
    }

    /// Read from `path` (missing or unreadable = nothing allowed).
    pub fn read(path: Option<&Path>) -> Store {
        let Some(text) = path.and_then(|p| std::fs::read_to_string(p).ok()) else { return Store::default() };
        let Ok(json) = serde_json::from_str::<Json>(&text) else { return Store::default() };
        let mut projects = BTreeMap::new();
        for (project, a) in json["projects"].as_object().into_iter().flatten() {
            let paths = |k: &str| -> Vec<PathBuf> {
                a[k].as_array().into_iter().flatten().filter_map(Json::as_str).map(PathBuf::from).collect()
            };
            let ports = a["ports"].as_array().into_iter().flatten().filter_map(Json::as_u64);
            let ports = ports.filter_map(|p| u16::try_from(p).ok()).collect();
            projects.insert(
                PathBuf::from(project),
                Allowed { read: paths("read"), write: paths("write"), ports },
            );
        }
        Store { projects }
    }

    pub fn get(&self, project: &Path) -> Allowed {
        self.projects.get(project).cloned().unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        let strings = |v: &[PathBuf]| v.iter().map(|r| r.to_string_lossy().into_owned()).collect::<Vec<_>>();
        let projects: serde_json::Map<String, Json> = self
            .projects
            .iter()
            .filter(|(_, a)| !a.is_empty())
            .map(|(p, a)| {
                let body = json!({ "read": strings(&a.read), "write": strings(&a.write), "ports": a.ports });
                (p.to_string_lossy().into_owned(), body)
            })
            .collect();
        serde_json::to_string_pretty(&json!({ "projects": projects })).unwrap_or_default()
    }
}

/// Changes what `project` always allows (on a worker — read, change, write; one at a time).
pub fn keep(path: &Path, project: &Path, change: impl FnOnce(&mut Allowed)) -> Result<(), String> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut store = Store::read(Some(path));
    change(store.projects.entry(project.to_path_buf()).or_default());
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let body = store.to_json();
    crate::disk::write_atomic(path, |w| std::io::Write::write_all(w, body.as_bytes()))
        .map_err(|e| e.to_string())
}

/// Places under home that hold keys and tokens — allowed once at a time, never kept.
const SECRETS: [&str; 11] = [
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
    !SECRETS.iter().any(|s| real.starts_with(home.join(s)))
}

/// Places under home that run something later when written — and so are never offered for writing (a
/// folder holding one isn't either): shells' and git's settings, launch agents, tarae's own config. Keys
/// (`SECRETS`) neither.
const NEVER_WRITE: [&str; 6] =
    [".config/fish", ".config/git", ".config/tarae", ".oh-my-zsh", "Library/LaunchAgents", ".zsh"];

/// Folders offered for one denied write, at most; and how deep one must be (`/opt`, `/private/var` aren't).
const WRITE_CHOICES: usize = 4;
const WRITE_DEPTH: usize = 3;

/// The folders a card may offer to let Claude's commands write in, for a write the sandbox denied at
/// `target`: from its nearest existing folder up to just below home (outside home, `WRITE_CHOICES` at most,
/// none shallower than `WRITE_DEPTH`), widest last — none that is home or above it, holds keys or a place
/// that runs something later (`NEVER_WRITE`, a folder on `path`), or is in (or holds) the project. Real
/// paths.
pub fn write_choices(target: &Path, home: &Path, root: &Path, path: &[PathBuf]) -> Vec<PathBuf> {
    let mut at = target.parent().map(Path::to_path_buf);
    while let Some(p) = at.clone().filter(|p| !p.exists()) {
        at = p.parent().map(Path::to_path_buf);
    }
    let never: Vec<PathBuf> =
        SECRETS.iter().chain(NEVER_WRITE.iter()).map(|s| home.join(s)).chain(path.iter().cloned()).collect();
    let mut out = Vec::new();
    while let Some(dir) = at {
        at = dir.parent().map(Path::to_path_buf);
        let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
        let refused = home.starts_with(&dir)
            || never.iter().any(|n| n.starts_with(&dir) || dir.starts_with(n))
            || dir.starts_with(root)
            || root.starts_with(&dir)
            || dir.components().count() <= WRITE_DEPTH;
        if !refused && !out.contains(&dir) {
            out.push(dir);
        }
    }
    out.truncate(WRITE_CHOICES);
    out
}

/// Home, as the process sees it.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_absolute())
}

/// The project as the kept answers know it (its real path).
pub fn project() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();
    std::fs::canonicalize(&cwd).unwrap_or(cwd)
}

/// Keeps `change` for the project, on a worker (an error says so).
fn keep_later(ed: &mut crate::editor::Editor, change: impl FnOnce(&mut Allowed) + Send + 'static) {
    let Some(path) = ed.allow_path.clone() else { return };
    ed.events.jobs().spawn(move || {
        let r = keep(&path, &project(), change);
        move |ed: &mut crate::editor::Editor| {
            if let Err(e) = r {
                ed.set_error(format!("couldn't keep it: {e}"));
            }
        }
    });
}

/// `a`/`A` on a card: Claude may read `places` (real paths) for the rest of this conversation, or always in
/// this project (kept on a worker) — said in a toast.
pub fn grant_reads(ed: &mut crate::editor::Editor, places: &[PathBuf], scope: Scope) {
    if scope == Scope::Once || places.is_empty() {
        return;
    }
    if let Some(c) = ed.chat.as_mut() {
        for p in places {
            c.allowed.add_read(p.clone());
        }
    }
    let shown = places.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ");
    if scope == Scope::ThisChat {
        return ed.set_success(format!("Claude may read {shown} in this conversation"));
    }
    let places = places.to_vec();
    keep_later(ed, move |a| places.into_iter().for_each(|p| a.add_read(p)));
    ed.set_success(format!("Claude may read {shown} in this project — :sandbox-allowed lists it"));
}

/// `a`/`A` on a card about what the sandbox blocked: Claude's commands may do `g` for the rest of this
/// conversation, or always in this project — and Claude hears it with the next message.
pub fn grant(ed: &mut crate::editor::Editor, g: &Grant, scope: Scope) {
    if scope == Scope::Once {
        return;
    }
    if let Some(c) = ed.chat.as_mut() {
        c.allowed.add(g);
    }
    let (when, toast) = match scope {
        Scope::Always => {
            ("in this project, until they take it back", " in this project — :sandbox-allowed lists it")
        }
        _ => ("for the rest of this conversation", " in this conversation"),
    };
    ed.checkpoints.tell.push(format!("The user let your commands {} — {when}.", g.shown()));
    if scope == Scope::Always {
        let g = g.clone();
        keep_later(ed, move |a| a.add(&g));
    }
    ed.set_success(format!("Claude's commands may {}{toast}", g.shown()));
}

impl crate::editor::Editor {
    /// `:sandbox-allowed` — what Claude may do past the defaults here, always (kept) and in this
    /// conversation; Enter takes one back.
    pub fn sandbox_allowed(&mut self) {
        use crate::picker::{Action, FillError, Item};
        let chat = self.chat.as_ref().map(|c| c.allowed.clone()).unwrap_or_default();
        let path = self.allow_path.clone();
        let fill: crate::picker::Fill = Box::new(move || {
            let kept = Store::read(path.as_deref()).get(&project());
            let row = |label: String, hint: String| Item {
                action: Action::Typed(format!("sandbox-disallow {label}")),
                label,
                hint,
                glyph: Some(("◦", "ui.virtual")),
            };
            let mut items = Vec::new();
            for (a, when) in [(&kept, "always"), (&chat, "this chat")] {
                let not_kept = |also: bool| when == "always" || !also;
                for p in a.read.iter().filter(|p| not_kept(kept.read.contains(p))) {
                    items.push(row(p.display().to_string(), format!("reads · {when}")));
                }
                for p in a.write.iter().filter(|p| not_kept(kept.write.contains(p))) {
                    items.push(row(p.display().to_string(), format!("writes · {when}")));
                }
                for n in a.ports.iter().filter(|n| not_kept(kept.ports.contains(n))) {
                    items.push(row(format!("port {n}"), format!("connects · {when}")));
                }
            }
            match items.is_empty() {
                true => Err(FillError::Nothing("nothing allowed past the defaults in this project".into())),
                false => Ok(items),
            }
        });
        let p =
            crate::picker::Picker::new("allowed — enter takes it back", Vec::new(), false).without_preview();
        self.open_picker(p, Some(fill));
    }

    /// `:sandbox-disallow <path | port N>` — taken back (this conversation and kept): Claude asks again before
    /// reading there, its commands can't write there or connect there.
    pub fn sandbox_disallow(&mut self, what: &str) -> Result<(), String> {
        let what = what.trim().to_string();
        if !what.starts_with('/') && !what.starts_with("port ") {
            return Err("usage: :sandbox-disallow <path | port N> — as :sandbox-allowed lists it".into());
        }
        if let Some(c) = self.chat.as_mut() {
            c.allowed.remove(&what);
        }
        let taken = what.clone();
        keep_later(self, move |a| a.remove(&taken));
        self.set_success(format!("Taken back: {what}"));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let dir = std::env::temp_dir().join(format!("tarae-allow-{}", std::process::id()));
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
        std::fs::remove_dir_all(&dir).ok();
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

    /// The folders offered for a denied write: its nearest existing folder up to just below home — never
    /// home or above, keys, a place that runs something later or a folder holding one, the PATH's folders
    /// (or what holds them), or the project. (A made-up home in the test's temp folder.)
    #[test]
    fn which_folders_a_denied_write_may_open() {
        let base = std::env::temp_dir().join(format!("tarae-choices-{}", std::process::id()));
        std::fs::remove_dir_all(&base).ok();
        let home = base.join("home");
        for d in [".npm/_cacache", ".local/bin", ".config/tarae", "Library/LaunchAgents", "work/proj"] {
            std::fs::create_dir_all(home.join(d)).unwrap();
        }
        let home = std::fs::canonicalize(&home).unwrap();
        let root = home.join("work/proj");
        let path = [home.join(".local/bin")];
        let at = |t: &str| write_choices(&home.join(t), &home, &root, &path);
        // (the file itself, and a folder not there yet, aren't offered — the nearest that is, and up)
        assert_eq!(at(".npm/_cacache/new/x.json"), [home.join(".npm/_cacache"), home.join(".npm")]);
        assert!(at(".zshrc").is_empty(), "in home itself");
        assert!(at(".ssh/known_hosts").is_empty(), "keys");
        assert!(at(".local/bin/tool").is_empty(), "on PATH — and .local holds it");
        assert!(at(".config/tarae/config.toml").is_empty(), "tarae's own; .config holds it");
        assert!(at("Library/LaunchAgents/x.plist").is_empty(), "runs at login; Library holds it");
        assert!(at("work/proj/.git/index.lock").is_empty(), "the project");
        assert_eq!(at("work/other.txt"), Vec::<PathBuf>::new(), "work holds the project");
        // Outside home: a few, none shallow
        let deep = base.join("o/a/b/c/d");
        std::fs::create_dir_all(&deep).unwrap();
        let deep = std::fs::canonicalize(deep).unwrap();
        let got = write_choices(&deep.join("x"), &home, &root, &path);
        assert_eq!(got.len(), WRITE_CHOICES, "{got:?}");
        assert_eq!(got[0], deep);
        assert!(write_choices(Path::new("/opt/x/y"), &home, &root, &path).is_empty(), "{got:?}");
        std::fs::remove_dir_all(&base).ok();
    }
}
