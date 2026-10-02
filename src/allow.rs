//! What the user lets Claude do past the defaults — for now, read outside the project (`reads.rs`). Each
//! answer on its card is for once, for this conversation (`Chat.allowed`), or always: kept per project (its
//! real path) in `$state/sandbox.json` — the user's state, not the project's files (those go into git, to
//! other people, and would need trust again).
//!
//! A place that holds keys or is home itself (or above it) is only ever allowed once (`keepable`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value as Json, json};

/// What one scope allows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Allowed {
    /// Files, and folders with everything in them, Claude's Read, Grep and Glob may read (real paths).
    pub read: Vec<PathBuf>,
}

impl Allowed {
    /// Whether `real` (a real path) is one of the places allowed, or in one.
    pub fn reads(&self, real: &Path) -> bool {
        self.read.iter().any(|p| real.starts_with(p))
    }

    /// Adds a place (one already covered isn't added twice).
    pub fn add_read(&mut self, real: PathBuf) {
        if !self.reads(&real) {
            self.read.retain(|p| !p.starts_with(&real));
            self.read.push(real);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.read.is_empty()
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
            let read = a["read"].as_array().into_iter().flatten().filter_map(Json::as_str).map(PathBuf::from);
            projects.insert(PathBuf::from(project), Allowed { read: read.collect() });
        }
        Store { projects }
    }

    pub fn get(&self, project: &Path) -> Allowed {
        self.projects.get(project).cloned().unwrap_or_default()
    }

    pub fn to_json(&self) -> String {
        let projects: serde_json::Map<String, Json> = self
            .projects
            .iter()
            .filter(|(_, a)| !a.is_empty())
            .map(|(p, a)| {
                let read: Vec<String> = a.read.iter().map(|r| r.to_string_lossy().into_owned()).collect();
                (p.to_string_lossy().into_owned(), json!({ "read": read }))
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

/// Home, as the process sees it.
pub fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).filter(|h| h.is_absolute())
}

/// The project as the kept answers know it (its real path).
fn project() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_default();
    std::fs::canonicalize(&cwd).unwrap_or(cwd)
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
    if let Some(path) = ed.allow_path.clone() {
        let places = places.to_vec();
        ed.events.jobs().spawn(move || {
            let r = keep(&path, &project(), |a| places.into_iter().for_each(|p| a.add_read(p)));
            move |ed: &mut crate::editor::Editor| {
                if let Err(e) = r {
                    ed.set_error(format!("couldn't keep it: {e}"));
                }
            }
        });
    }
    ed.set_success(format!("Claude may read {shown} in this project — :sandbox-allowed lists it"));
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
            let row = |p: &PathBuf, hint: &str| Item {
                label: p.display().to_string(),
                action: Action::Typed(format!("sandbox-disallow {}", p.display())),
                hint: hint.into(),
                glyph: Some(("◦", "ui.virtual")),
            };
            let mut items: Vec<Item> = kept.read.iter().map(|p| row(p, "reads · always")).collect();
            items.extend(
                chat.read.iter().filter(|p| !kept.read.contains(p)).map(|p| row(p, "reads · this chat")),
            );
            match items.is_empty() {
                true => Err(FillError::Nothing("nothing allowed past the defaults in this project".into())),
                false => Ok(items),
            }
        });
        let p =
            crate::picker::Picker::new("allowed — enter takes it back", Vec::new(), false).without_preview();
        self.open_picker(p, Some(fill));
    }

    /// `:sandbox-disallow <path>` — Claude asks again before reading there (this conversation and kept).
    pub fn sandbox_disallow(&mut self, path: &str) -> Result<(), String> {
        let at = PathBuf::from(path.trim());
        if !at.is_absolute() {
            return Err("usage: :sandbox-disallow <path> — as :sandbox-allowed lists it".into());
        }
        if let Some(c) = self.chat.as_mut() {
            c.allowed.read.retain(|p| *p != at);
        }
        if let Some(store) = self.allow_path.clone() {
            let at = at.clone();
            self.events.jobs().spawn(move || {
                let r = keep(&store, &project(), |a| a.read.retain(|p| *p != at));
                move |ed: &mut crate::editor::Editor| {
                    if let Err(e) = r {
                        ed.set_error(format!("couldn't take it back: {e}"));
                    }
                }
            });
        }
        self.set_success(format!("Claude asks again before reading {}", at.display()));
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
    }

    #[test]
    fn kept_per_project_and_read_back() {
        let dir = std::env::temp_dir().join(format!("tarae-allow-{}", std::process::id()));
        let path = dir.join("state/sandbox.json");
        keep(&path, Path::new("/p/one"), |a| a.add_read("/t/a".into())).unwrap();
        keep(&path, Path::new("/p/two"), |a| a.add_read("/t/b".into())).unwrap();
        keep(&path, Path::new("/p/one"), |a| a.add_read("/t/c/f".into())).unwrap();
        let s = Store::read(Some(&path));
        assert_eq!(s.get(Path::new("/p/one")).read, [PathBuf::from("/t/a"), PathBuf::from("/t/c/f")]);
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
}
