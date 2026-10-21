//! What a review run reads (bgreview, "tarae native" 2026-10-20): the snapshot it took (`snapshot::snap`'s tree)
//! as git holds it. Its members' tools are tarae's own `read` and `search` (`readtool::read_snapshot_seen`,
//! `nav::search_snapshot`) over this — each file's text from git's objects, never the checkout you go on
//! editing — so what a member reads is what its diff says, and every line it names is the snapshot's
//! (`notes::pin_finding` places it on the lines now). Nothing is written anywhere.
//!
//! - Listed once, on the worker that takes the snapshot (`ls-tree -r`, the sizes from `cat-file --batch-check`);
//!   texts read as a call asks for them, from one `cat-file --batch` a call, kept for the run's later calls up to
//!   `KEPT_MAX` bytes (the snapshot never changes: nothing kept goes stale).
//! - git runs with no hooks and never fetches a blob this clone doesn't have (`GIT_NO_LAZY_FETCH` — a partial
//!   clone's): such a file is left out, counted at the start and said when asked for.
//! - Left out, and said so when asked for: files named like keys (`sandbox::key_named_in`), links (a link's change
//!   is in the diff as its target text), submodules, files past `FILE_MAX`.
//! - A path is the checkout's (from its root, or absolute under it), resolved by its words alone — the snapshot has
//!   no links to follow; one outside the checkout is refused.

use std::collections::{BTreeMap, HashMap};
use std::io::{BufRead, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

/// A file past this isn't read: a blob comes whole into memory (there's no file to stream or map) and is kept for
/// the run's other calls — and its change is in the diff the reviewer is sent.
pub const FILE_MAX: u64 = 1 << 20;

/// The texts kept for a run's later calls at most (bytes): past it, the rest are read from git again each call.
/// (This repository's whole tree is ~29 MB: a run's searches of all of it read git once.)
const KEPT_MAX: usize = 64 << 20;

/// Why a file of the snapshot isn't read.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Left {
    Key,
    Link,
    Submodule,
    /// Past `FILE_MAX` (its size).
    Big(u64),
    /// Not in this clone.
    Missing,
}

impl Left {
    /// Why, in words — after the file's path.
    pub fn said(self) -> String {
        match self {
            Left::Key => "named like a key — the reviewer doesn't read it".into(),
            Left::Link => "a link — not read (the diff has its target)".into(),
            Left::Submodule => "a submodule — not read".into(),
            Left::Big(n) => format!(
                "{} KB, past the {} MB a file the reviewer reads — its change is in the diff",
                n.div_ceil(1024),
                FILE_MAX >> 20
            ),
            Left::Missing => "not in this clone (never fetched) — not read".into(),
        }
    }
}

/// What a path names in the snapshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Found {
    File,
    Folder,
    Left(Left),
    Nothing,
}

/// The snapshot a review run reads: its files (blob and size, by path from the root), those left out and why.
pub struct Snap {
    /// The checkout as the review names it, and its real path — a path under either is the checkout's.
    pub root: PathBuf,
    real: PathBuf,
    pub tree: String,
    files: BTreeMap<String, (String, u64)>,
    left: BTreeMap<String, Left>,
    kept: Mutex<Kept>,
    outlines: Mutex<HashMap<String, Arc<Vec<crate::nav::Sym>>>>,
}

#[derive(Default)]
struct Kept {
    bytes: usize,
    texts: HashMap<String, Arc<[u8]>>,
}

/// Its listing and kept texts are freed on a worker (`event::drop_later`) — a big tree's maps and up to `KEPT_MAX`
/// of texts, and the last hold may be the main loop's: a board card's `n` or a retake, a session's end, a card
/// dropped.
impl Drop for Snap {
    fn drop(&mut self) {
        let kept = std::mem::take(self.kept.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner));
        let outlines =
            std::mem::take(self.outlines.get_mut().unwrap_or_else(std::sync::PoisonError::into_inner));
        crate::event::drop_later((
            std::mem::take(&mut self.files),
            std::mem::take(&mut self.left),
            kept,
            outlines,
        ));
    }
}

/// An instruction file there for the snapshot's reader (`Snap::rules`): its path, its lines, whether it's the
/// checkout's (git ignores it) rather than the snapshot's — and why it isn't read when it isn't (a link, past
/// `FILE_MAX`; its lines then 0).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    pub rel: String,
    pub lines: usize,
    pub from_checkout: bool,
    pub unread: Option<String>,
}

impl std::fmt::Debug for Snap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Snap").field("root", &self.root).field("tree", &self.tree).finish_non_exhaustive()
    }
}

/// git on the checkout at `root` for the snapshot: no hooks, and never a fetch for a blob this clone doesn't have.
fn git(root: &Path, args: &[&str]) -> Result<std::process::Command, String> {
    let all: Vec<&str> = crate::gitrepo::NO_HOOKS.iter().copied().chain(args.iter().copied()).collect();
    let mut cmd = crate::gitrepo::repo(root)?.command(&all);
    cmd.env("GIT_NO_LAZY_FETCH", "1");
    Ok(cmd)
}

impl Snap {
    /// Tree `tree` of the checkout at `root` listed (`ls-tree -r -z`, then the blobs' sizes from `cat-file
    /// --batch-check`). Blocks (a worker's).
    pub fn list(root: &Path, tree: &str) -> Result<Snap, String> {
        let out = crate::gitrepo::finish(&git(root, &["ls-tree", "-r", "-z", "--full-tree", tree])?, None)?;
        let out = String::from_utf8_lossy(&out);
        let mut left = BTreeMap::new();
        let mut wanted: Vec<(String, String)> = Vec::new();
        for rec in out.split('\0').filter(|r| !r.is_empty()) {
            let Some((meta, rel)) = rec.split_once('\t') else { continue };
            let mut f = meta.split_ascii_whitespace();
            let (Some(mode), Some(kind), Some(id)) = (f.next(), f.next(), f.next()) else { continue };
            // (a name git holds that isn't a plain path from the root is never one a tool's path could name)
            if !Path::new(rel).components().all(|c| matches!(c, Component::Normal(_))) {
                continue;
            }
            let why = match (mode, kind) {
                _ if crate::sandbox::key_named_in(root, &root.join(rel)) => Some(Left::Key),
                ("120000", _) => Some(Left::Link),
                (_, "commit") => Some(Left::Submodule),
                ("100644" | "100755", "blob") => None,
                _ => continue,
            };
            match why {
                Some(why) => drop(left.insert(rel.to_string(), why)),
                None => wanted.push((id.to_string(), rel.to_string())),
            }
        }
        let ids: String = wanted.iter().map(|(id, _)| format!("{id}\n")).collect();
        let sizes =
            crate::gitrepo::finish(&git(root, &["cat-file", "--batch-check"])?, Some(ids.as_bytes()))?;
        let sizes = String::from_utf8_lossy(&sizes);
        let mut files = BTreeMap::new();
        for ((id, rel), line) in wanted.into_iter().zip(sizes.lines()) {
            // `<id> blob <size>` — or `<id> missing`: not in this clone
            match line.split_ascii_whitespace().collect::<Vec<_>>()[..] {
                [_, "blob", size] => match size.parse::<u64>() {
                    Ok(size) if size <= FILE_MAX => drop(files.insert(rel, (id, size))),
                    Ok(size) => drop(left.insert(rel, Left::Big(size))),
                    Err(_) => drop(left.insert(rel, Left::Missing)),
                },
                _ => drop(left.insert(rel, Left::Missing)),
            }
        }
        Ok(Snap {
            root: root.to_path_buf(),
            real: crate::paths::real(root).unwrap_or_else(|| root.to_path_buf()),
            tree: tree.to_string(),
            files,
            left,
            kept: Mutex::default(),
            outlines: Mutex::default(),
        })
    }

    /// How many files it lists that this clone doesn't have (said at the start).
    pub fn missing(&self) -> usize {
        self.left.values().filter(|l| **l == Left::Missing).count()
    }

    /// `given` (from the root, or absolute) as a path of the snapshot (`/`-separated, "" the root) — None: outside
    /// the checkout. By its words: `..` steps back a name, never past the root.
    pub fn rel_of(&self, given: &str) -> Option<String> {
        let given = Path::new(given.trim());
        let rel = match given.is_absolute() {
            true => given.strip_prefix(&self.root).or_else(|_| given.strip_prefix(&self.real)).ok()?,
            false => given,
        };
        let mut parts: Vec<&str> = Vec::new();
        for c in rel.components() {
            match c {
                Component::Normal(s) => parts.push(s.to_str()?),
                Component::CurDir => {}
                Component::ParentDir => {
                    parts.pop()?;
                }
                Component::RootDir | Component::Prefix(_) => return None,
            }
        }
        Some(parts.join("/"))
    }

    /// An instruction file (`worktree::INSTRUCTIONS`, at the checkout's top) the snapshot hasn't — git ignores it,
    /// and the project's rules would be out of the review's sight: read from the checkout as it is now, through no
    /// link, a plain file up to `FILE_MAX`, never one named like a key. None: not one of those names, or the
    /// snapshot has it (then it's the snapshot's, as any file); Some(Err): why it isn't read. Blocks (a worker's).
    pub fn instruction(&self, rel: &str) -> Option<Result<Vec<u8>, String>> {
        if !crate::worktree::INSTRUCTIONS.contains(&rel) || self.found(rel) != Found::Nothing {
            return None;
        }
        if crate::sandbox::key_named_in(&self.root, &self.root.join(rel)) {
            return Some(Err(Left::Key.said()));
        }
        let size = std::fs::symlink_metadata(self.root.join(rel)).map(|m| m.len()).unwrap_or(0);
        if size > FILE_MAX {
            return Some(Err(Left::Big(size).said()));
        }
        let read = crate::beneath::Dir::root(&self.root).and_then(|d| d.read(rel.as_ref()));
        Some(match read {
            Ok(Some(bytes)) if bytes.len() as u64 <= FILE_MAX => Ok(bytes),
            Ok(Some(bytes)) => Err(Left::Big(bytes.len() as u64).said()),
            Ok(None) => Err("no such file in the snapshot or the checkout".into()),
            Err(e) if e.raw_os_error() == Some(libc::ELOOP) => Err(Left::Link.said()),
            Err(e) => Err(format!("not read from the checkout: {e}")),
        })
    }

    /// The checkout's instruction files (`worktree::INSTRUCTIONS`, at its top) `read` gives — the snapshot's, else
    /// one git ignores from the checkout (`instruction`) — with their lines: named in the reviewer's prompt, whose
    /// listing and search never show an ignored one (frictions 55: it concluded there was none). One there that
    /// isn't read is listed with why — never taken for none. Blocks (a worker's).
    pub fn rules(&self) -> Vec<Rule> {
        let lines = |b: &[u8]| b.iter().filter(|&&c| c == b'\n').count() + usize::from(!b.ends_with(b"\n"));
        let rule = |rel: &str, read: Result<Vec<u8>, String>, from_checkout| match read {
            Ok(b) => Rule { rel: rel.to_string(), lines: lines(&b), from_checkout, unread: None },
            Err(why) => Rule { rel: rel.to_string(), lines: 0, from_checkout, unread: Some(why) },
        };
        // (the checkout's, git ignoring it: there when its name is, a link too)
        let there = |rel: &str| std::fs::symlink_metadata(self.root.join(rel)).is_ok();
        (crate::worktree::INSTRUCTIONS.iter())
            .filter_map(|&rel| match (self.instruction(rel), self.found(rel)) {
                (Some(Err(_)), _) if !there(rel) => None,
                (Some(got), _) => Some(rule(rel, got, true)),
                (None, Found::File) => {
                    let text =
                        self.text(rel).map(|t| t.to_vec()).ok_or_else(|| "not read from git".to_string());
                    Some(rule(rel, text, false))
                }
                (None, Found::Left(why)) => Some(rule(rel, Err(why.said()), false)),
                (None, _) => None,
            })
            .collect()
    }

    /// What `rel` (a path of the snapshot) names.
    pub fn found(&self, rel: &str) -> Found {
        if self.files.contains_key(rel) {
            return Found::File;
        }
        if let Some(l) = self.left.get(rel) {
            return Found::Left(*l);
        }
        let prefix = format!("{rel}/");
        let under = |k: Option<&String>| k.is_some_and(|k| k.starts_with(&prefix));
        match rel.is_empty()
            || under(self.files.range(prefix.clone()..).next().map(|e| e.0))
            || under(self.left.range(prefix.clone()..).next().map(|e| e.0))
        {
            true => Found::Folder,
            false => Found::Nothing,
        }
    }

    /// The files it reads in `scope` (a path of the snapshot: a file, a folder, "" all), by path.
    pub fn files_in<'a>(&'a self, scope: &'a str) -> Box<dyn Iterator<Item = &'a str> + 'a> {
        if scope.is_empty() {
            return Box::new(self.files.keys().map(String::as_str));
        }
        if self.files.contains_key(scope) {
            return Box::new(std::iter::once(scope));
        }
        let prefix = format!("{scope}/");
        Box::new(
            self.files
                .range(prefix.clone()..)
                .map(|e| e.0.as_str())
                .take_while(move |k| k.starts_with(&prefix)),
        )
    }

    /// The text of file `rel` as the snapshot has it (None: not one it reads). Blocks (a worker's).
    pub fn text(&self, rel: &str) -> Option<Arc<[u8]>> {
        let mut out = None;
        self.each_text(&[rel], &mut |_, t| {
            out = Some(t);
            false
        })
        .ok()?;
        out
    }

    /// The texts of `rels` (those it reads — others skipped) to `each` with their paths: those kept first, the
    /// rest from one `cat-file --batch` in their order, kept while there's room (false from `each`: enough —
    /// the rest unread). Blocks (a worker's).
    pub fn each_text(
        &self,
        rels: &[&str],
        each: &mut dyn FnMut(&str, Arc<[u8]>) -> bool,
    ) -> Result<(), String> {
        let mut todo: Vec<(&str, &str)> = Vec::new();
        for &rel in rels {
            let Some((id, _)) = self.files.get(rel) else { continue };
            let kept = self.kept.lock().unwrap_or_else(|p| p.into_inner()).texts.get(rel).cloned();
            match kept {
                Some(t) => {
                    if !each(rel, t) {
                        return Ok(());
                    }
                }
                None => todo.push((rel, id)),
            }
        }
        if todo.is_empty() {
            return Ok(());
        }
        let cmd = git(&self.root, &["cat-file", "--batch"])?;
        let said = |e: std::io::Error| format!("git cat-file: {e}");
        let (s, mut w) = crate::spawn::setsid_fed(&cmd).map_err(said)?;
        let ids: String = todo.iter().map(|(_, id)| format!("{id}\n")).collect();
        // (fed from a thread of its own, its errors read on another: neither pipe can hold the other up)
        let feed = crate::event::thread("tarae-snap-feed", move || {
            let _ = w.write_all(ids.as_bytes());
        });
        let errs = s.stderr.map(|mut e| {
            crate::event::thread("tarae-snap-err", move || {
                let mut v = Vec::new();
                let _ = e.read_to_end(&mut v);
                v
            })
        });
        // (its first failure kept, the loop left — never a return before the wait below: git reaped whatever came)
        let read = |out: std::io::PipeReader| -> Result<(), String> {
            let mut out = std::io::BufReader::new(out);
            for (rel, _) in &todo {
                let mut head = String::new();
                if out.read_line(&mut head).map_err(said)? == 0 {
                    return Err(format!("git cat-file ended before {rel}"));
                }
                // `<id> blob <size>` — or `<id> missing` (gone since it was listed: skipped)
                let size = match head.split_ascii_whitespace().collect::<Vec<_>>()[..] {
                    [_, "blob", size] => {
                        size.parse::<usize>().map_err(|_| format!("git cat-file: {head}"))?
                    }
                    _ => continue,
                };
                let mut bytes = vec![0; size + 1];
                out.read_exact(&mut bytes).map_err(said)?;
                bytes.truncate(size);
                let text: Arc<[u8]> = bytes.into();
                let mut kept = self.kept.lock().unwrap_or_else(|p| p.into_inner());
                if kept.bytes + size <= KEPT_MAX {
                    kept.bytes += size;
                    kept.texts.insert(rel.to_string(), text.clone());
                }
                drop(kept);
                if !each(rel, text) {
                    break;
                }
            }
            Ok(())
        };
        // (its output closed early, git's next write fails, it exits, the wait returns)
        let done = s.stdout.map_or(Ok(()), read);
        let _ = feed.join();
        let err = errs.and_then(|t| t.join().ok()).unwrap_or_default();
        let status = crate::spawn::wait(s.pid).map_err(said)?;
        if !status.success() && done.is_ok() && !err.is_empty() {
            let e = String::from_utf8_lossy(&err);
            return Err(format!("git cat-file: {}", e.lines().next().unwrap_or("failed")));
        }
        done
    }

    /// File `rel`'s outline from its text (`outlines::of_text`), kept for the run.
    pub fn outline(&self, rel: &str, text: &str) -> Result<Arc<Vec<crate::nav::Sym>>, String> {
        if let Some(s) = self.outlines.lock().unwrap_or_else(|p| p.into_inner()).get(rel) {
            return Ok(s.clone());
        }
        let syms = crate::outlines::of_text(&self.root.join(rel), None, text)?;
        self.outlines.lock().unwrap_or_else(|p| p.into_inner()).insert(rel.to_string(), syms.clone());
        Ok(syms)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// git in the test's own repository, none of the user's config read (no global or system file — no hooks of
    /// theirs, no signing), `input` on its stdin. Its output.
    pub(crate) fn git_in(dir: &Path, args: &[&str], input: &[u8]) -> String {
        let mut child = std::process::Command::new("git")
            .args(["-c", "core.hooksPath=/dev/null", "-c", "commit.gpgsign=false"])
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn git(dir: &Path, args: &[&str]) {
        git_in(dir, args, b"");
    }

    /// A checkout with a file, a key, a link and a file past the cap — its tree as `snapshot::snap` takes it.
    fn checkout(dir: &Path) -> String {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.join("id_rsa"), "tarae-fake-token\n").unwrap();
        std::os::unix::fs::symlink("src/a.rs", dir.join("link")).unwrap();
        std::fs::write(dir.join("big.txt"), vec![b'x'; FILE_MAX as usize + 1]).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
            &["add", "-A"],
            &["commit", "-q", "-m", "base"],
        ] {
            git(dir, args);
        }
        std::fs::write(dir.join("new.txt"), "untracked\n").unwrap();
        crate::snapshot::snap(dir).unwrap().id
    }

    fn text(s: &Snap, rel: &str) -> Option<String> {
        s.text(rel).map(|t| String::from_utf8_lossy(&t).into_owned())
    }

    /// What it reads is the snapshot's text, whatever the file holds now; a key, a link and a file past the cap
    /// are listed as left out, each with its reason; folders are told from files.
    #[test]
    fn the_snapshot_holds_its_texts_and_leaves_out_keys_links_and_big_files() {
        let s = crate::scratch::Scratch::new("snapread");
        let root = s.join("proj");
        let tree = checkout(&root);
        std::fs::write(root.join("src/a.rs"), "fn changed() {}\n").unwrap();
        let snap = Snap::list(&root, &tree).unwrap();
        assert_eq!(text(&snap, "src/a.rs").as_deref(), Some("fn a() {}\n"), "as taken");
        assert_eq!(text(&snap, "src/a.rs").as_deref(), Some("fn a() {}\n"), "kept: the same again");
        assert_eq!(text(&snap, "new.txt").as_deref(), Some("untracked\n"));
        assert_eq!(snap.found("id_rsa"), Found::Left(Left::Key));
        assert_eq!(snap.found("link"), Found::Left(Left::Link));
        assert_eq!(snap.found("big.txt"), Found::Left(Left::Big(FILE_MAX + 1)));
        assert_eq!(
            (snap.found("src"), snap.found(""), snap.found("sr")),
            (Found::Folder, Found::Folder, Found::Nothing)
        );
        for gone in ["id_rsa", "link", "big.txt", "src"] {
            assert_eq!(text(&snap, gone), None, "{gone}");
        }
        assert_eq!(snap.files_in("").collect::<Vec<_>>(), ["new.txt", "src/a.rs"]);
        assert_eq!(snap.files_in("src").collect::<Vec<_>>(), ["src/a.rs"]);
    }

    /// A snapshot let go here (the main loop, a card's `n`) frees its listing and texts on a worker.
    #[test]
    fn a_snapshot_is_freed_on_a_worker() {
        let s = crate::scratch::Scratch::new("snapread-drop");
        let root = s.join("proj");
        let tree = checkout(&root);
        let snap = Snap::list(&root, &tree).unwrap();
        assert!(snap.text("src/a.rs").is_some(), "a text kept");
        let before = crate::event::DROPPED_LATER.with(|n| n.get());
        drop(snap);
        assert_eq!(crate::event::DROPPED_LATER.with(|n| n.get()), before + 1);
    }

    /// A git-ignored CLAUDE.md is read from the checkout as it is now, said so in its header; a tracked one is the
    /// snapshot's; another ignored file, a deeper CLAUDE.md and a linked AGENTS.md aren't read.
    #[test]
    fn an_ignored_instruction_file_is_read_from_the_checkout_and_said_so() {
        let s = crate::scratch::Scratch::new("snapread-instructions");
        let root = s.join("proj");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join(".gitignore"), "CLAUDE.md\nAGENTS.md\nnotes.txt\nsrc/CLAUDE.md\n").unwrap();
        std::fs::write(root.join("CLAUDE.local.md"), "tracked rules\n").unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
            &["add", "-A"],
            &["commit", "-q", "-m", "base"],
        ] {
            git(&root, args);
        }
        std::fs::write(root.join("CLAUDE.md"), "the project's rules\n").unwrap();
        std::fs::write(root.join("notes.txt"), "ignored\n").unwrap();
        std::fs::write(root.join("src/CLAUDE.md"), "deeper\n").unwrap();
        std::os::unix::fs::symlink("notes.txt", root.join("AGENTS.md")).unwrap();
        let snap = Snap::list(&root, &crate::snapshot::snap(&root).unwrap().id).unwrap();
        std::fs::write(root.join("CLAUDE.local.md"), "changed since\n").unwrap();
        let read = |f: &str| match crate::readtool::read_snapshot(&snap, &serde_json::json!({ "files": [f] }))
        {
            Ok(s) | Err(s) => s,
        };
        let claude = read("CLAUDE.md");
        assert!(claude.contains("1: the project's rules"), "{claude}");
        assert!(
            claude.starts_with("== CLAUDE.md (of 1 line) — from the checkout: not in the snapshot"),
            "{claude}"
        );
        let tracked = read("CLAUDE.local.md");
        assert!(tracked.contains("1: tracked rules") && !tracked.contains("from the checkout"), "{tracked}");
        for refused in ["notes.txt", "src/CLAUDE.md", "AGENTS.md"] {
            let said = read(refused);
            assert!(!said.contains("1: "), "{refused}: {said}");
        }
        assert!(read("AGENTS.md").contains("a link"), "{}", read("AGENTS.md"));
        // (named for the reviewer's prompt: what read gives, the ignored one marked the checkout's)
        let rule = |rel: &str, from_checkout| Rule { rel: rel.into(), lines: 1, from_checkout, unread: None };
        let rules = snap.rules();
        assert_eq!(rules[..2], [rule("CLAUDE.md", true), rule("CLAUDE.local.md", false)]);
        // (one there that isn't read — the linked AGENTS.md — said with why, never taken for none: the review's
        // finding)
        assert_eq!((rules.len(), rules[2].rel.as_str(), rules[2].lines), (3, "AGENTS.md", 0), "{rules:?}");
        assert!(rules[2].unread.as_deref().is_some_and(|w| w.contains("a link")), "{rules:?}");
    }

    /// A path from the root or absolute under it (as given or as its real path) is the snapshot's; `..` past the
    /// root, or anywhere else, is outside.
    #[test]
    fn a_path_outside_the_checkout_is_none() {
        let s = crate::scratch::Scratch::new("snapread-paths");
        let root = s.join("proj");
        let tree = checkout(&root);
        let snap = Snap::list(&root, &tree).unwrap();
        let real = std::fs::canonicalize(&root).unwrap();
        assert_eq!(snap.rel_of("src/a.rs").as_deref(), Some("src/a.rs"));
        assert_eq!(snap.rel_of("./src/../new.txt").as_deref(), Some("new.txt"));
        assert_eq!(snap.rel_of(&root.join("src").display().to_string()).as_deref(), Some("src"));
        assert_eq!(snap.rel_of(&real.join("src/a.rs").display().to_string()).as_deref(), Some("src/a.rs"));
        for out in ["../x.txt", "src/../../x", "/etc/hosts"] {
            assert_eq!(snap.rel_of(out), None, "{out}");
        }
        assert_eq!(snap.rel_of(&s.join("x").display().to_string()), None);
    }

    /// A blob this clone doesn't have (as a partial clone's): never fetched, left out and counted.
    #[test]
    fn a_blob_this_clone_lacks_is_left_out_and_counted() {
        let s = crate::scratch::Scratch::new("snapread-missing");
        let root = s.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        git(&root, &["init", "-q"]);
        let here = git_in(&root, &["hash-object", "-w", "--stdin"], b"here\n");
        let gone = "0123456789abcdef0123456789abcdef01234567";
        let entries = format!("100644 blob {here}\there.txt\n100644 blob {gone}\tgone.txt\n");
        let tree = git_in(&root, &["mktree", "--missing"], entries.as_bytes());
        let snap = Snap::list(&root, &tree).unwrap();
        assert_eq!((snap.missing(), snap.found("gone.txt")), (1, Found::Left(Left::Missing)));
        assert_eq!(text(&snap, "here.txt").as_deref(), Some("here\n"));
    }

    /// A measurement, not a check (`cargo perf`): this repository's tree as of `tests/perf.rs`'s `CORPUS` (its
    /// paths) listed — what a review run waits for before its first member starts — and every text read once, as a
    /// first search of the whole tree does (later ones read what's kept).
    #[test]
    fn perf_snapshot_reads() {
        if crate::test_results::unoptimized("snapread::tests::perf_snapshot_reads") {
            return;
        }
        const CORPUS: &str = "53843b6";
        const PATHS: [&str; 8] =
            ["src", "runtime", "docs", "tests", "xtask/src", "Cargo.toml", "README.md", "CONTRIBUTING.md"];
        let s = crate::scratch::Scratch::new("snapread-perf");
        let root = s.join("proj");
        std::fs::create_dir_all(&root).unwrap();
        let here = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let archive = std::process::Command::new("git")
            .args(["archive", "--format=tar", CORPUS, "--"])
            .args(PATHS)
            .current_dir(&here)
            .output()
            .unwrap();
        if !archive.status.success() {
            let why = format!("snapread::tests::perf_snapshot_reads — no commit {CORPUS} here");
            return crate::test_results::said_skip(&why);
        }
        let mut tar = std::process::Command::new("tar")
            .args(["-xf", "-"])
            .current_dir(&root)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        tar.stdin.take().unwrap().write_all(&archive.stdout).unwrap();
        assert!(tar.wait().unwrap().success());
        for args in [&["init", "-q", "-b", "main"][..], &["add", "-A"]] {
            git(&root, args);
        }
        let tree = crate::snapshot::snap(&root).unwrap().id;
        let mut runs = Vec::new();
        for _ in 0..5 {
            let t = std::time::Instant::now();
            let snap = Snap::list(&root, &tree).unwrap();
            let listed = t.elapsed();
            let all: Vec<&str> = snap.files_in("").collect();
            let (mut n, mut bytes) = (0, 0);
            snap.each_text(&all, &mut |_, t| {
                (n, bytes) = (n + 1, bytes + t.len());
                true
            })
            .unwrap();
            runs.push((listed, t.elapsed() - listed, n, bytes));
        }
        runs.sort();
        let (listed, read, n, bytes) = runs[runs.len() / 2];
        let line = format!(
            "PERF snapshot_list_ms {:.1} noise=30\nPERF snapshot_read_all_ms {:.1} noise=30\n\
             (snapshot: {n} files, {} MB)\n",
            listed.as_secs_f64() * 1e3,
            read.as_secs_f64() * 1e3,
            bytes >> 20,
        );
        let _ = std::io::stderr().write_all(line.as_bytes());
    }
}
