//! git branch — for the status line. Reads the `HEAD` file directly instead of spawning `git` (a few µs).
//! A watcher thread waits for HEAD to change (`fswatch`) and notifies the main loop (checkout in lazygit etc.).

use std::path::{Path, PathBuf};

use crate::editor::Editor;
use crate::event::Event;

/// The repo's HEAD file, walking up from `start`. Follows worktree/submodule `.git` files (`gitdir: …`) too.
pub fn find_head(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    loop {
        let dot = dir.join(".git");
        if dot.is_dir() {
            return Some(dot.join("HEAD"));
        }
        if dot.is_file() {
            let s = std::fs::read_to_string(&dot).ok()?;
            let gitdir = s.lines().find_map(|l| l.strip_prefix("gitdir:"))?.trim();
            let gitdir =
                if Path::new(gitdir).is_absolute() { PathBuf::from(gitdir) } else { dir.join(gitdir) };
            return Some(gitdir.join("HEAD"));
        }
        if !dir.pop() {
            return None;
        }
    }
}

/// HEAD content → branch name, or the first 7 chars of the commit if HEAD is detached.
pub fn parse_head(content: &str) -> Option<String> {
    let s = content.trim();
    match s.strip_prefix("ref:") {
        Some(r) => Some(r.trim().trim_start_matches("refs/heads/").to_string()),
        None if s.len() >= 7 && s.bytes().all(|b| b.is_ascii_hexdigit()) => Some(s[..7].to_string()),
        None => None,
    }
}

pub fn read_branch(head: &Path) -> Option<String> {
    parse_head(&std::fs::read_to_string(head).ok()?)
}

/// The loose ref file a symbolic HEAD points to (`ref: refs/heads/x`) — in a worktree's own git folder, or
/// the repository's common one (`commondir`). None: HEAD is detached (or unreadable).
fn head_ref(head: &Path) -> Option<PathBuf> {
    let name = std::fs::read_to_string(head).ok()?.strip_prefix("ref:")?.trim().to_string();
    let gitdir = head.parent()?;
    let common = std::fs::read_to_string(gitdir.join("commondir"))
        .ok()
        .map(|c| gitdir.join(c.trim()))
        .unwrap_or_else(|| gitdir.to_path_buf());
    let own = gitdir.join(&name);
    Some(if own.exists() { own } else { common.join(&name) })
}

/// Watcher thread for `dir`'s repository — reads once at first, then whenever HEAD moves: its file (a
/// checkout, a rebase) or the ref it points to (a commit, a rebase's end, a reset). Then the branch is read
/// again and the session's buffers take the index as it is now (#74: after a rebase in a session's
/// terminal the status line said +62 −62 over a clean tree).
pub fn watch(tx: crate::event::Tx, dir: PathBuf) {
    crate::event::thread("tarae-git-watch", move || {
        let Some(head) = find_head(&dir) else { return };
        let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
        let (mut last, mut first) = (None, true);
        loop {
            let mut watcher = crate::fswatch::Watcher::new(); // (armed before the stat — see `fswatch`)
            watcher.file(&head);
            let reference = head_ref(&head);
            if let Some(r) = &reference {
                watcher.file(r);
            }
            let now = mtime(&head);
            // (HEAD gone: the worktree was removed — a session ended — and so is its session)
            if now.is_none() && !first {
                break;
            }
            let now = (now, reference.as_deref().and_then(mtime));
            if first || Some(now) != last {
                let moved = !first;
                (last, first) = (Some(now), false);
                let branch = read_branch(&head);
                let dir = dir.clone();
                // (only the session it's for: one ended meanwhile sends it to the front)
                let apply = move |ed: &mut Editor| {
                    if ed.session.root == dir {
                        ed.session.git_branch = branch;
                        if moved {
                            ed.git_reload_bases_in(&dir);
                        }
                    }
                };
                if tx.send(Event::Job(Box::new(apply))).is_err() {
                    break;
                }
            }
            watcher.wait();
        }
    });
}

// ── Changed-line markers (bar next to line numbers) ──────────────────────────

/// Changed-line hunks against the index (what `git add` last recorded). `lines` = line range in the
/// current document (empty for a deletion — deleted before that line); `base` = the lines they replace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hunk {
    pub kind: HunkKind,
    pub lines: std::ops::Range<usize>,
    /// Lines missing on the index side (status line −N).
    pub removed: usize,
    pub base: std::ops::Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HunkKind {
    Added,
    Modified,
    Deleted,
}

/// Index content vs current content → hunks (histogram diff — same family as Helix and git).
pub fn hunks(base: &str, now: &str) -> Vec<Hunk> {
    use imara_diff::intern::InternedInput;
    use imara_diff::{Algorithm, diff};
    let input = InternedInput::new(base, now);
    let mut out = Vec::new();
    diff(Algorithm::Histogram, &input, |before: std::ops::Range<u32>, after: std::ops::Range<u32>| {
        let kind = match (before.is_empty(), after.is_empty()) {
            (true, _) => HunkKind::Added,
            (_, true) => HunkKind::Deleted,
            _ => HunkKind::Modified,
        };
        out.push(Hunk {
            kind,
            lines: after.start as usize..after.end as usize,
            removed: before.len(),
            base: before.start as usize..before.end as usize,
        });
    });
    out
}

/// Unchanged lines between two changes that still make them one hunk — as `git diff` shows them (three lines
/// of context on each side).
const HUNK_GAP: usize = 6;

/// `hunks` as `]g` walks them: changes `HUNK_GAP` or fewer unchanged lines apart are one (a rewritten
/// function, not each line of it).
pub fn regions(hunks: &[Hunk]) -> Vec<Hunk> {
    let mut out: Vec<Hunk> = Vec::new();
    for h in hunks {
        match out.last_mut() {
            Some(r) if h.lines.start.saturating_sub(r.lines.end) <= HUNK_GAP => {
                r.kind = if (r.kind, h.kind) == (HunkKind::Added, HunkKind::Added) {
                    HunkKind::Added
                } else {
                    HunkKind::Modified
                };
                r.lines.end = h.lines.end.max(r.lines.end);
                r.base.end = h.base.end.max(r.base.end);
                r.removed += h.removed;
            }
            _ => out.push(h.clone()),
        }
    }
    out
}

/// This file's content in the index of its repository as `places` finds it (`gitrepo::for_file_in` — never by
/// discovery from where a run can write); Ok(None): untracked. The gutter compares against it, so staged
/// changes leave the gutter. Runs on a worker thread.
pub fn index_blob(
    places: &crate::gitrepo::Places,
    path: &Path,
) -> Result<Option<String>, crate::gitrepo::NoGit> {
    let (repo, rel) = crate::gitrepo::for_file_in(places, path)?;
    Ok(repo.output(&["show", &format!(":{rel}")], None).ok().and_then(|b| String::from_utf8(b).ok()))
}

impl Editor {
    /// Where `path`'s git is looked for (`gitrepo::for_file_in`): the sessions' repositories — the one whose
    /// checkout holds it first, then the front one's — and, for a file in none, where a sandboxed run of any
    /// session may write (`sandbox::write_places`, with what's been granted: kept for its project, its chat's;
    /// worked out on the worker, only then). Runs not sandboxed (`llm.sandbox` off, no sandbox here): none —
    /// such a file's own repository is found from its folder, nothing held back.
    pub(crate) fn git_places(&self, path: &Path) -> crate::gitrepo::Places {
        use crate::sandbox::Availability;
        let id = self.session_of(path);
        let all = || std::iter::once(&self.session).chain(&self.sessions);
        let mut roots: Vec<PathBuf> = all().filter(|s| Some(s.id) == id).map(|s| s.root.clone()).collect();
        for s in all() {
            if !roots.contains(&s.root) {
                roots.push(s.root.clone());
            }
        }
        let sandboxed = self.config.llm.sandbox
            && !matches!(self.sandbox, Availability::Missing(_) | Availability::Bypass);
        let (llm, store) = (self.config.llm.clone(), self.allow_path.clone());
        let chats: Vec<(PathBuf, crate::allow::Allowed)> = all()
            .map(|s| (s.root.clone(), s.chat.as_ref().map(|c| c.allowed.clone()).unwrap_or_default()))
            .collect();
        let writes: crate::gitrepo::Writes = std::sync::Arc::new(move || {
            if !sandboxed {
                return None;
            }
            let kept = crate::allow::Store::read(store.as_deref());
            let mut all = Vec::new();
            for (root, chat) in &chats {
                let project = kept.get(&crate::sandbox::profile_path(root));
                let w = crate::tools::with_allowed(&llm, &[&project, chat], None);
                all.extend(crate::sandbox::write_places(root, &w.llm));
            }
            Some(all)
        });
        crate::gitrepo::Places { roots, writes }
    }

    /// (Re)load the document's index content — on open, on regaining focus (may have staged outside), after
    /// staging a hunk.
    pub fn git_load_base(&mut self, id: crate::document::DocId) {
        if !self.git_auto {
            return;
        }
        // In a code review the base is the review's (codereview.rs), not the index — also before it lands
        let reviewed =
            move |ed: &Editor| ed.session.code_review.as_ref().is_some_and(|r| r.docs.contains(&id));
        let Some(path) =
            self.docs.iter().find(|d| d.id == id && d.review_base.is_none()).and_then(|d| d.path.clone())
        else {
            return;
        };
        if reviewed(self) {
            return;
        }
        let places = self.git_places(&path);
        self.events.jobs().spawn(move || {
            let read = index_blob(&places, &path);
            move |ed: &mut Editor| {
                ed.git_bases_read += 1;
                if read == Err(crate::gitrepo::NoGit::Writable { looks: true }) && ed.git_told.insert(path) {
                    ed.set_status(crate::gitrepo::NOT_READ);
                }
                let base = read.ok().flatten().map(std::sync::Arc::new);
                // A review or `:diff` that started meanwhile keeps its base
                if reviewed(ed) {
                    return;
                }
                if let Some(d) = ed.docs.iter_mut().find(|d| d.id == id && d.review_base.is_none())
                    && d.git_base.as_deref() != base.as_deref()
                {
                    d.git_base = base;
                    d.git_version = u64::MAX; // re-diff
                    if d.git_base.is_none() {
                        d.git_hunks.clear();
                    }
                }
            }
        });
    }

    pub fn git_reload_bases(&mut self) {
        let ids: Vec<_> = self.docs.iter().filter(|d| d.path.is_some()).map(|d| d.id).collect();
        for id in ids {
            self.git_load_base(id);
        }
    }

    /// The buffers of files under `dir` (a checkout whose HEAD moved) take the index as it is now — and the
    /// blame of the one in view is asked again (it's kept per text version, which a commit doesn't change: a
    /// rebased line said "not committed yet" — a shot, 2026-10-04).
    pub fn git_reload_bases_in(&mut self, dir: &Path) {
        let ids: Vec<_> = self
            .docs
            .iter()
            .filter(|d| d.path.as_deref().is_some_and(|p| p.starts_with(dir)))
            .map(|d| d.id)
            .collect();
        if self.blame.of.is_some_and(|(id, _)| ids.contains(&id)) {
            self.blame.of = None;
        }
        for id in ids {
            self.git_load_base(id);
        }
    }

    /// On every event: if the current document's diff is stale, redo it on a worker thread shortly after
    /// (once input stops).
    pub fn git_schedule(&mut self) {
        if self.git_diffing {
            return;
        }
        let doc = self.doc();
        let Some(base) = doc.git_base.clone() else { return };
        if doc.git_version == doc.version() || doc.loading {
            return;
        }
        let (id, version, text) = (doc.id, doc.version(), doc.text.clone());
        self.git_diffing = true;
        self.events.jobs().spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(if cfg!(test) { 0 } else { 80 }));
            let hunks = hunks(&base, &text.to_string());
            move |ed: &mut Editor| {
                ed.git_diffing = false;
                // Against the base it still has — a review's or `:diff`'s base may have come in meanwhile
                if let Some(d) = ed.docs.iter_mut().find(|d| d.id == id)
                    && d.version() == version
                    && d.git_base.as_ref().is_some_and(|b| std::sync::Arc::ptr_eq(b, &base))
                {
                    d.git_hunks = hunks;
                    d.git_version = version;
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_contents() {
        assert_eq!(parse_head("ref: refs/heads/master\n").as_deref(), Some("master"));
        assert_eq!(parse_head("ref: refs/heads/feat/타래\n").as_deref(), Some("feat/타래"));
        assert_eq!(parse_head("0123456789abcdef0123\n").as_deref(), Some("0123456"));
        assert_eq!(parse_head("garbage"), None);
    }

    #[test]
    fn hunks_added_modified_deleted() {
        let base = "a\nb\nc\nd\n";
        let now = "a\nB\nc\nnew\n";
        let h = hunks(base, now);
        assert_eq!(h[0], Hunk { kind: HunkKind::Modified, lines: 1..2, removed: 1, base: 1..2 });
        assert_eq!(h[1], Hunk { kind: HunkKind::Modified, lines: 3..4, removed: 1, base: 3..4 });
        let h = hunks("a\nb\n", "a\n");
        assert_eq!(h, [Hunk { kind: HunkKind::Deleted, lines: 1..1, removed: 1, base: 1..2 }]);
        let h = hunks("a\n", "a\nb\nc\n");
        assert_eq!(h, [Hunk { kind: HunkKind::Added, lines: 1..3, removed: 0, base: 1..1 }]);
    }

    /// The gutter's base for a large file (`index_blob`, one git run each) — 100k lines (5 MB) in a
    /// repository of 3,000 tracked files: the first (the repository resolved then, once per root), and the
    /// median of 21 after (`target/sb/gitlauncher.md`: the same as the discovery form it replaced).
    #[test]
    fn perf_gutter_base_of_a_large_file() {
        if crate::test_results::unoptimized("git::tests::perf_gutter_base_of_a_large_file") {
            return;
        }
        let dir = std::env::temp_dir().join(format!("tarae-gutter-perf-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        for i in 0..3000 {
            std::fs::write(dir.join(format!("src/f{i}.rs")), format!("fn f{i}() {{}}\n").repeat(20)).unwrap();
        }
        let big = dir.join("src/big.rs");
        std::fs::write(&big, "fn line() { let x = 1 + 2 + 3; } // a line of a large file\n".repeat(100_000))
            .unwrap();
        for args in [&["init", "-q"][..], &["add", "-A"]] {
            let ok = std::process::Command::new("git").args(args).current_dir(&dir).output().unwrap();
            assert!(ok.status.success(), "git {args:?}");
        }
        let ms = |t: std::time::Instant| t.elapsed().as_secs_f64() * 1000.0;
        let places = crate::gitrepo::Places::only(&dir);
        let t = std::time::Instant::now();
        assert!(index_blob(&places, &big).is_ok_and(|b| b.is_some_and(|b| b.len() > 5_000_000)));
        let first = ms(t);
        let mut again: Vec<f64> = (0..21)
            .map(|_| {
                let t = std::time::Instant::now();
                assert!(index_blob(&places, &big).is_ok_and(|b| b.is_some()));
                ms(t)
            })
            .collect();
        crate::sorting::by(&mut again, f64::total_cmp);
        std::fs::remove_dir_all(&dir).ok();
        let lines = [
            format!("PERF gutter_base_large_file_ms {:.2} budget=200 noise=1", again[again.len() / 2]),
            format!("(the first: {first:.2} ms · fastest {:.2} · slowest {:.2})", again[0], again[20]),
        ];
        let _ = std::io::Write::write_all(&mut std::io::stderr(), (lines.join("\n") + "\n").as_bytes());
    }

    #[test]
    fn finds_this_repo() {
        let head = find_head(Path::new(env!("CARGO_MANIFEST_DIR")).join("src").as_path()).unwrap();
        assert!(read_branch(&head).is_some());
    }
}
