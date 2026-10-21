//! What a session keeps when it ends (`session.keep`, frictions26). `git worktree remove` deletes
//! ignored files too: a session's notes, plans and measurements (`target/sb/`) went with its checkout, and
//! 52 sessions' were copied out by hand. Each folder named there moves to the main checkout's
//! `<folder>/sessions/<name>/` before the checkout goes:
//!
//! - **Through no link** (`beneath.rs`): descriptors walked from both roots, a folder opened `O_NOFOLLOW`
//!   from the one before, a file moved by `linkat` (a copy across volumes); links and protected names
//!   (`sandbox::protected_names` — what runs later) are left behind, and said.
//! - **Never into main's tracked tree**: only where git ignores the destination there (`check-ignore`).
//! - **Whole or not at all**: no main checkout, a destination git doesn't ignore, more than `KEEP_MAX` —
//!   nothing moves, and the end card says why (its `y` still ends).
//!
//! Its instruction files that differ from main's are named on the end card, and copied beside the first
//! folder kept (`instructions/`, flat names — `.claude` is a protected name).

use std::ffi::{OsStr, OsString};
use std::path::{Component, Path, PathBuf};

use crate::beneath::{Dir, Kind};

/// What a session keeps at most, all told.
pub const KEEP_MAX: u64 = 64 << 20;

/// Files a session keeps at most — past this it's not notes.
const FILES_MAX: usize = 20_000;

/// An entry of `session.keep` as a folder in the checkout — Err: why it's refused.
pub fn entry(s: &str) -> Result<PathBuf, String> {
    let p = Path::new(s.trim_end_matches('/'));
    if s.trim().is_empty() {
        return Err("\"\": an empty path".into());
    }
    if s.starts_with('~') || p.is_absolute() {
        return Err(format!("\"{s}\": a folder in the checkout, not from / or ~"));
    }
    for part in p.components() {
        match part {
            Component::Normal(n) if n.eq_ignore_ascii_case(".git") => {
                return Err(format!("\"{s}\": never .git or what's in it"));
            }
            Component::Normal(_) => {}
            _ => return Err(format!("\"{s}\": plain names only — no .. or .")),
        }
    }
    Ok(p.to_path_buf())
}

/// The entries of `session.keep` refused, said (`settings.rs`) — None: every one is a folder in the
/// checkout.
pub fn refused(entries: &[String]) -> Option<String> {
    let why: Vec<String> = entries.iter().filter_map(|e| entry(e).err()).collect();
    (!why.is_empty()).then(|| format!("left out — {}", why.join("; ")))
}

/// The entries kept of `session.keep`.
pub fn entries(list: &[String]) -> Vec<PathBuf> {
    list.iter().filter_map(|e| entry(e).ok()).collect()
}

/// A folder of the session's that's kept: where it is in the checkout, its files and bytes, where it goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Folder {
    pub rel: String,
    pub files: usize,
    pub bytes: u64,
    pub to: PathBuf,
}

/// What ending a session keeps (`plan`), for its end card and its end.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// Its folders there, and where each goes.
    pub folders: Vec<Folder>,
    /// Its instruction files that differ from the main checkout's (relative) — and where they're copied (None:
    /// nowhere, no folder kept).
    pub differ: Vec<String>,
    pub instructions_to: Option<PathBuf>,
    /// Why nothing is kept, none of it: no main checkout, a destination git doesn't ignore, too much.
    pub not: Option<String>,
    /// The main checkout it goes to (where the card's paths are from).
    pub main: Option<PathBuf>,
}

impl Plan {
    /// Something of its goes with the checkout for good: a folder not kept, an instruction file of its own
    /// copied nowhere.
    pub fn loses(&self) -> bool {
        let folders = !self.folders.is_empty() && self.not.is_some();
        let instructions = !self.differ.is_empty() && (self.not.is_some() || self.instructions_to.is_none());
        folders || instructions
    }

    /// The end card's lines about it: (text, warned) — a heading, then what, a row each in two columns:
    ///
    /// ```text
    /// Kept in main's target/sb/sessions/fix-login/:        Not kept (over 64 MB) — goes with the checkout:
    ///   target/sb  14 files, 220 KB                          target/sb  2400 files, 310 MB
    ///   CLAUDE.md  differs from main's                       CLAUDE.md  differs from main's
    /// ```
    ///
    /// Kept, said plainly; going with the checkout, warned. Nothing there: nothing said.
    pub fn lines(&self) -> Vec<(String, bool)> {
        let count = |n: usize| if n == 1 { "1 file".to_string() } else { format!("{n} files") };
        // (a few notes are bytes, not `0 KB`)
        let size = |b: u64| if b < 1024 { format!("{b} B") } else { crate::procs::mem(b) };
        let mut rows: Vec<(String, String)> = (self.folders.iter())
            .map(|f| (f.rel.clone(), format!("{}, {}", count(f.files), size(f.bytes))))
            .collect();
        rows.extend(self.differ.iter().map(|d| (d.clone(), "differs from main's".to_string())));
        if rows.is_empty() {
            return Vec::new();
        }
        // (where it goes, from the main checkout — the user knows where that is)
        let from_main = |p: &Path| match self.main.as_deref().and_then(|m| p.strip_prefix(m).ok()) {
            Some(rel) => format!("main's {}/", rel.display()),
            None => crate::allow::tilde(p),
        };
        let to = self
            .folders
            .first()
            .map(|f| f.to.clone())
            .or_else(|| Some(self.instructions_to.as_deref()?.parent()?.into()));
        let (head, warned) = match (&self.not, to) {
            (None, Some(_)) if self.folders.len() > 1 => {
                ("Kept in the main checkout, each in its sessions/:".into(), false)
            }
            (None, Some(to)) if self.instructions_to.is_some() || self.differ.is_empty() => {
                (format!("Kept in {}:", from_main(&to)), false)
            }
            (Some(why), _) => (format!("Not kept ({why}) — goes with the checkout:"), true),
            _ => ("Not kept (session.keep names no folder) — goes with the checkout:".into(), true),
        };
        let w = rows.iter().map(|r| r.0.chars().count()).max().unwrap_or(0);
        let pad = |s: &str| format!("{s}{}", " ".repeat(w - s.chars().count()));
        std::iter::once((head, warned))
            .chain(rows.iter().map(|(a, b)| (format!("  {}  {b}", pad(a)), warned)))
            .collect()
    }
}

/// The folder name session `name`'s keepings go in (a branch's `/` is a `-`).
fn folder_name(name: &str) -> String {
    let n = name.replace(['/', '\\'], "-");
    match n.trim_matches('.').is_empty() {
        true => "session".into(),
        false => n,
    }
}

/// The first of `<name>`, `<name>-2`, … free in `dir`.
fn free_in(dir: &Path, name: &str) -> PathBuf {
    (1..)
        .map(|i| dir.join(if i == 1 { name.to_string() } else { format!("{name}-{i}") }))
        .find(|p| std::fs::symlink_metadata(p).is_err())
        .expect("a free name")
}

/// Files and bytes in folder `at`, links not followed — None past `FILES_MAX` files.
fn measure(at: &Path) -> Option<(usize, u64)> {
    let (mut files, mut bytes, mut todo) = (0, 0, vec![at.to_path_buf()]);
    while let Some(d) = todo.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let Ok(m) = std::fs::symlink_metadata(e.path()) else { continue };
            if m.is_dir() {
                todo.push(e.path());
            } else if m.is_file() {
                (files, bytes) = (files + 1, bytes + m.len());
                if files > FILES_MAX {
                    return None;
                }
            }
        }
    }
    Some((files, bytes))
}

/// What ending session `name` at `root` keeps — its `entries` (checked: `entry`) into `main`, at most `max`;
/// `differ`: its instruction files that differ from main's. On a worker (git, the disk).
pub fn plan(
    root: &Path,
    name: &str,
    entries: &[PathBuf],
    main: Option<&Path>,
    differ: Vec<String>,
    max: u64,
) -> Plan {
    let mut plan = Plan { differ, main: main.map(Path::to_path_buf), ..Plan::default() };
    if entries.is_empty() {
        return plan;
    }
    let dir = folder_name(name);
    let mut first = None;
    for rel in entries {
        let r = rel.to_string_lossy().to_string();
        let to = main.map(|m| free_in(&m.join(rel).join("sessions"), &dir));
        first = first.or(to.clone());
        let at = root.join(rel);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => {
                plan.not.get_or_insert(format!("{r} isn't a folder — a link isn't followed"));
                plan.folders.push(Folder { rel: r, files: 0, bytes: 0, to: to.unwrap_or_default() });
                continue;
            }
            Err(_) => continue,
        }
        let Some((files, bytes)) = measure(&at) else {
            plan.not.get_or_insert(format!("over {FILES_MAX} files"));
            plan.folders.push(Folder { rel: r, files: FILES_MAX, bytes: 0, to: to.unwrap_or_default() });
            continue;
        };
        plan.folders.push(Folder { rel: r, files, bytes, to: to.unwrap_or_default() });
    }
    if plan.folders.is_empty() && plan.differ.is_empty() {
        return plan;
    }
    let Some(main) = main else {
        plan.not.get_or_insert("no main checkout to keep it in".into());
        return plan;
    };
    let total: u64 = plan.folders.iter().map(|f| f.bytes).sum();
    if total > max {
        plan.not.get_or_insert(format!("over {}", crate::procs::mem(max)));
    }
    // (where it goes is ignored there: keeping never puts a file in main's tracked tree)
    let goes: Vec<String> = (plan.folders.iter().map(|f| &f.to))
        .chain(first.iter().filter(|_| !plan.differ.is_empty()))
        .map(|f| f.strip_prefix(main).unwrap_or(f).to_string_lossy().to_string())
        .collect();
    for rel in goes {
        if crate::gitrepo::git(main, &["check-ignore", "-q", "--", &rel], None).is_err() {
            plan.not.get_or_insert(format!("git doesn't ignore {rel} in main"));
        }
    }
    plan.instructions_to = first.filter(|_| !plan.differ.is_empty()).map(|f| f.join("instructions"));
    plan
}

/// What was kept, for the end's toast: where to, how many files, what was left behind (relative).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Kept {
    pub to: Vec<PathBuf>,
    pub files: usize,
    pub left: Vec<String>,
}

impl Kept {
    /// ` · target/sb kept in ~/…/sessions/x (14 files) · not kept: build.rs` — empty when nothing was.
    pub fn said(&self, plan: &Plan) -> String {
        let mut s = String::new();
        if let Some(to) = self.to.first() {
            let what: Vec<&str> = plan.folders.iter().map(|f| f.rel.as_str()).collect();
            let what = if what.is_empty() { "its instruction files".to_string() } else { what.join(", ") };
            s += &format!(" · {what} kept in {} ({} files)", crate::allow::tilde(to), self.files);
        }
        let named: Vec<&str> = self.left.iter().take(3).map(String::as_str).collect();
        match self.left.len() {
            0 => {}
            n @ 4.. => s += &format!(" · not kept: {} and {} more", named.join(", "), n - 3),
            _ => s += &format!(" · not kept: {}", named.join(", ")),
        }
        s
    }
}

/// Whether `name` is a protected one (what runs later — any case; a name of several parts by its last).
fn protected(name: &OsStr) -> bool {
    let name = name.to_string_lossy();
    crate::sandbox::protected_names()
        .any(|p| p.rsplit('/').next().is_some_and(|l| l.eq_ignore_ascii_case(&name)))
}

/// Every file below `from` moved into `to` (made as it goes), as `at` (relative) names them — links, protected
/// names and what's neither a file nor a folder left, named in `kept.left`.
fn move_tree(from: &Dir, to: &Dir, at: &Path, kept: &mut Kept) -> Result<(), String> {
    let err = |e: std::io::Error| e.to_string();
    for (name, kind) in from.entries().map_err(err)? {
        let rel = at.join(&name);
        if protected(&name) || matches!(kind, Kind::Link | Kind::Other) {
            let what = if kind == Kind::Link { " (a link)" } else { "" };
            kept.left.push(format!("{}{what}", rel.display()));
            continue;
        }
        match kind {
            Kind::Folder => {
                let sub_from = from.folder(Path::new(&name), false).map_err(err)?;
                let sub_to = to.folder(Path::new(&name), true).map_err(err)?;
                move_tree(&sub_from, &sub_to, &rel, kept)?;
            }
            _ => {
                move_file(from, &name, to).map_err(err)?;
                kept.files += 1;
            }
        }
    }
    Ok(())
}

/// File `name` from `from` into `to`: linked (never over anything there), or across volumes copied, then
/// removed here.
fn move_file(from: &Dir, name: &OsStr, to: &Dir) -> std::io::Result<()> {
    match from.move_to(name, to, name) {
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            let bytes = from.read(name)?.ok_or_else(|| std::io::Error::other("gone meanwhile"))?;
            to.create(name, &bytes)?;
            from.remove(name)
        }
        r => r,
    }
}

/// A new folder for session `name` in `<main>/<rel>/sessions/` (made through no link) — the first of `<name>`,
/// `<name>-2`, … not there.
fn made_for(main: &Dir, rel: &Path, name: &str) -> Result<Dir, String> {
    let sessions = main.folder(&rel.join("sessions"), true).map_err(|e| e.to_string())?;
    for i in 1..1000 {
        let n = if i == 1 { name.to_string() } else { format!("{name}-{i}") };
        match sessions.make(&OsString::from(n)) {
            Ok(d) => return Ok(d),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.to_string()),
        }
    }
    Err(format!("no free name for {name} in {}", sessions.at.display()))
}

/// What `plan` keeps of the checkout at `root`, kept in `main` (the plan made again just before — a `not`
/// there keeps nothing). Err: why it stopped (what moved before it stays moved, said).
pub fn keep(root: &Path, name: &str, plan: &Plan, main: &Path) -> Result<Kept, String> {
    let mut kept = Kept::default();
    if plan.not.is_some() || (plan.folders.is_empty() && plan.differ.is_empty()) {
        return Ok(kept);
    }
    let dir = folder_name(name);
    let (from_root, to_root) = (Dir::root(root), Dir::root(main));
    let (from_root, to_root) = (from_root.map_err(|e| e.to_string())?, to_root.map_err(|e| e.to_string())?);
    let stopped = |kept: &Kept, why: String| match kept.files {
        0 => why,
        n => format!("{why} ({n} files moved already)"),
    };
    let mut first: Option<Dir> = None;
    for f in &plan.folders {
        let rel = Path::new(&f.rel);
        let from = from_root.folder(rel, false).map_err(|e| stopped(&kept, e.to_string()))?;
        let to = made_for(&to_root, rel, &dir).map_err(|e| stopped(&kept, e))?;
        kept.to.push(to.at.clone());
        move_tree(&from, &to, rel, &mut kept).map_err(|e| stopped(&kept, e))?;
        first.get_or_insert(to);
    }
    crate::sorting::sort(&mut kept.left);
    if plan.differ.is_empty() {
        return Ok(kept);
    }
    // (beside the first folder kept — made for them when the checkout had none of it)
    let first = match first {
        Some(d) => d,
        None => {
            let Some(rel) = plan.instructions_to.as_ref().and_then(|p| p.parent()?.parent()?.parent()) else {
                return Ok(kept);
            };
            let rel = rel.strip_prefix(main).unwrap_or(rel);
            let d = made_for(&to_root, rel, &dir).map_err(|e| stopped(&kept, e))?;
            kept.to.push(d.at.clone());
            d
        }
    };
    let into = first.make(OsStr::new("instructions")).map_err(|e| stopped(&kept, e.to_string()))?;
    for rel in &plan.differ {
        let p = Path::new(rel);
        let (Some(parent), Some(file)) = (p.parent(), p.file_name()) else { continue };
        let bytes = from_root.folder(parent, false).and_then(|d| d.read(file));
        let Ok(Some(bytes)) = bytes else {
            kept.left.push(rel.clone());
            continue;
        };
        // (flat: `.claude/settings.json` → `claude-settings.json` — `.claude` is a protected name)
        let flat = rel.trim_start_matches('.').replace('/', "-");
        into.create(OsStr::new(&flat), &bytes).map_err(|e| stopped(&kept, e.to_string()))?;
        kept.files += 1;
    }
    Ok(kept)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;

    /// A main checkout (git, `target/` ignored) and a session's folder beside it, in the test's own folder.
    fn two(name: &str, ignore: &str) -> (crate::scratch::Scratch, PathBuf, PathBuf) {
        let s = crate::scratch::Scratch::new(&format!("keep-{name}"));
        let dir = std::fs::canonicalize(&s).unwrap();
        let (main, session) = (dir.join("main"), dir.join("session"));
        std::fs::create_dir_all(&main).unwrap();
        std::fs::create_dir_all(session.join("target/sb/sub")).unwrap();
        let ok = std::process::Command::new("git").args(["init", "-q"]).current_dir(&main).status().unwrap();
        assert!(ok.success());
        std::fs::write(main.join(".gitignore"), ignore).unwrap();
        std::fs::write(session.join("target/sb/plan.md"), "the plan\n").unwrap();
        std::fs::write(session.join("target/sb/sub/numbers.txt"), "1 2 3\n").unwrap();
        (s, main, session)
    }

    fn sb() -> Vec<PathBuf> {
        vec![PathBuf::from("target/sb")]
    }

    #[test]
    fn an_entry_is_a_folder_in_the_checkout() {
        assert_eq!(entry("target/sb/"), Ok(PathBuf::from("target/sb")));
        for bad in ["/tmp/x", "~/notes", "../main/target", "./target", ".git", "a/.GIT/hooks", ""] {
            assert!(entry(bad).is_err(), "{bad}");
        }
        let said = refused(&["target/sb".into(), "../x".into(), ".git/info".into()]).unwrap();
        assert!(said.contains("\"../x\": plain names only") && said.contains("never .git"), "{said}");
        assert_eq!(entries(&["target/sb".into(), "/abs".into()]), sb());
        // As a setting: the folders kept, the rest said with the config's warnings
        let mut cfg = crate::config::EditorConfig::default();
        let v = toml::Value::Array(vec!["target/sb".into(), "../x".into()]);
        let said = crate::settings::apply(&mut cfg, "session.keep", &v).unwrap_err();
        assert!(said.starts_with("session.keep: left out — \"../x\""), "{said}");
        assert_eq!(cfg.session_keep, ["target/sb"]);
    }

    /// Its folder moves to main's `target/sb/sessions/<name>/` — a link and a protected name stay behind,
    /// said; a second session of the name gets `<name>-2`; an instruction file of its own is copied beside it,
    /// flat.
    #[test]
    fn a_sessions_folder_moves_into_the_main_checkout() {
        let (_s, main, session) = two("moves", "/target/\n");
        std::os::unix::fs::symlink("/etc/hosts", session.join("target/sb/hosts")).unwrap();
        std::fs::write(session.join("target/sb/build.rs"), "fn main() {}\n").unwrap();
        std::fs::write(session.join("CLAUDE.md"), "mine\n").unwrap();
        let differ = vec!["CLAUDE.md".to_string()];
        let plan = plan(&session, "fix/login", &sb(), Some(&main), differ, KEEP_MAX);
        let to = main.join("target/sb/sessions/fix-login");
        assert_eq!(plan.not, None);
        assert_eq!(plan.folders, [Folder { rel: "target/sb".into(), files: 3, bytes: 28, to: to.clone() }]);
        assert!(!plan.loses());
        let kept = keep(&session, "fix/login", &plan, &main).unwrap();
        assert_eq!(std::fs::read_to_string(to.join("plan.md")).unwrap(), "the plan\n");
        assert_eq!(std::fs::read_to_string(to.join("sub/numbers.txt")).unwrap(), "1 2 3\n");
        assert_eq!(std::fs::read_to_string(to.join("instructions/CLAUDE.md")).unwrap(), "mine\n");
        assert!(!session.join("target/sb/plan.md").exists(), "moved, not copied");
        assert!(std::fs::symlink_metadata(to.join("hosts")).is_err() && !to.join("build.rs").exists());
        assert_eq!(kept.files, 3);
        assert_eq!(kept.left, ["target/sb/build.rs", "target/sb/hosts (a link)"].map(String::from).to_vec());
        assert!(kept.said(&plan).contains("target/sb kept in "), "{}", kept.said(&plan));
        // The name taken: the next one
        std::fs::write(session.join("target/sb/plan.md"), "again\n").unwrap();
        let plan = super::plan(&session, "fix/login", &sb(), Some(&main), Vec::new(), KEEP_MAX);
        assert_eq!(plan.folders[0].to, main.join("target/sb/sessions/fix-login-2"));
        keep(&session, "fix/login", &plan, &main).unwrap();
        assert_eq!(
            std::fs::read_to_string(main.join("target/sb/sessions/fix-login-2/plan.md")).unwrap(),
            "again\n"
        );
    }

    /// Nothing moves where git doesn't ignore the destination, past the size, or with no main checkout — the
    /// card says why, and that it goes with the checkout.
    #[test]
    fn nothing_is_kept_where_it_would_be_tracked_or_too_much() {
        let (_s, main, session) = two("refused", "");
        let plan = plan(&session, "s", &sb(), Some(&main), Vec::new(), KEEP_MAX);
        assert!(plan.not.as_deref().is_some_and(|w| w.contains("git doesn't ignore target/sb/sessions/s")));
        assert!(plan.loses());
        let lines = plan.lines();
        assert!(lines.iter().all(|l| l.1), "warned: {lines:?}");
        let head = "Not kept (git doesn't ignore target/sb/sessions/s in main) — goes with the checkout:";
        assert_eq!(lines[0].0, head);
        assert!(lines[1].0.starts_with("  target/sb  2 files, "), "{lines:?}");
        assert_eq!(keep(&session, "s", &plan, &main).unwrap(), Kept::default());
        assert!(!main.join("target").exists() && session.join("target/sb/plan.md").exists());
        std::fs::write(main.join(".gitignore"), "/target/\n").unwrap();
        let plan = super::plan(&session, "s", &sb(), Some(&main), Vec::new(), 4);
        assert!(plan.not.as_deref().is_some_and(|w| w.starts_with("over ")), "{plan:?}");
        let plan = super::plan(&session, "s", &sb(), None, vec!["CLAUDE.md".into()], KEEP_MAX);
        assert!(plan.not.as_deref().is_some_and(|w| w.contains("no main checkout")));
        let lines = plan.lines();
        let head = "Not kept (no main checkout to keep it in) — goes with the checkout:";
        assert_eq!(lines[0], (head.to_string(), true));
        assert_eq!(lines[1], ("  target/sb  2 files, 15 B".to_string(), true));
        assert_eq!(lines[2], ("  CLAUDE.md  differs from main's".to_string(), true));
        // Nothing to keep: nothing said, nothing lost
        let none = super::plan(&session, "s", &[PathBuf::from("notes")], Some(&main), Vec::new(), KEEP_MAX);
        assert!(none.folders.is_empty() && none.not.is_none() && none.lines().is_empty() && !none.loses());
        assert!(Path::new(&session).join("target/sb").exists());
    }
}
