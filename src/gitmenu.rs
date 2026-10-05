//! `space g` — git: `f` changed files · `p` the hunk under the cursor as a diff card · `r` reset it (an
//! undoable edit) · `s` stage it (`git apply --cached` of that hunk alone) · `b` blame at the cursor line's
//! end, toggled. git runs on worker threads; the gutter compares against the index (git.rs).

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use ropey::Rope;

use crate::editor::Editor;
use crate::git::{Hunk, HunkKind};
use crate::movement as mv;

// ── Pure parts ────────────────────────────────────────────────────────────────

/// The hunk the cursor line is in — a deletion counts on the line above its gap (where its mark is).
pub fn hunk_at(hunks: &[Hunk], line: usize) -> Option<&Hunk> {
    hunks.iter().find(|h| match h.kind {
        HunkKind::Deleted => line + 1 == h.lines.start || (h.lines.start == 0 && line == 0),
        _ => h.lines.contains(&line),
    })
}

/// Byte span of whole lines `lines` in `text` (line breaks included).
fn line_bytes(text: &Rope, lines: &std::ops::Range<usize>) -> (usize, usize) {
    let at = |l: usize| if l >= text.len_lines() { text.len_bytes() } else { mv::line_start(text, l) };
    (at(lines.start), at(lines.end))
}

/// The index side of a hunk, as text.
pub fn base_text(base: &str, h: &Hunk) -> String {
    let rope = Rope::from_str(base);
    let (a, b) = line_bytes(&rope, &h.base);
    rope.byte_slice(a..b).to_string()
}

/// `r` — the change that puts the hunk's index lines back: (from, to, text) in the document.
pub fn reset_change(now: &Rope, base: &str, h: &Hunk) -> (usize, usize, String) {
    let (a, b) = line_bytes(now, &h.lines);
    (a, b, base_text(base, h))
}

/// `s` — a zero-context patch of just this hunk (`git apply --cached --unidiff-zero`), path from the
/// repository root.
pub fn patch(rel: &str, base: &str, now: &Rope, h: &Hunk) -> String {
    let old = base_text(base, h);
    let (a, b) = line_bytes(now, &h.lines);
    let new = now.byte_slice(a..b).to_string();
    // Zero-length sides name the line *before* the gap (git's convention)
    let side = |r: &std::ops::Range<usize>| {
        if r.is_empty() { format!("{},0", r.start) } else { format!("{},{}", r.start + 1, r.len()) }
    };
    let mut p = format!("diff --git a/{rel} b/{rel}\n--- a/{rel}\n+++ b/{rel}\n");
    p += &format!("@@ -{} +{} @@\n", side(&h.base), side(&h.lines));
    for (mark, chunk) in [('-', &old), ('+', &new)] {
        for l in chunk.split_inclusive('\n') {
            p.push(mark);
            p += l;
            if !l.ends_with('\n') {
                p += "\n\\ No newline at end of file\n";
            }
        }
    }
    p
}

/// `git status --porcelain=v1 -z` → (status letter, path from the repo root). Renames give the new path.
pub fn parse_status(out: &str) -> Vec<(char, String)> {
    let mut v = Vec::new();
    let mut it = out.split('\0').filter(|s| !s.is_empty());
    while let Some(e) = it.next() {
        if e.len() < 4 {
            continue;
        }
        let (xy, path) = e.split_at(3);
        let (x, y) = (xy.as_bytes()[0] as char, xy.as_bytes()[1] as char);
        if x == 'R' || x == 'C' {
            it.next(); // the old name
        }
        let letter = match (x, y) {
            ('?', _) => '?',
            (_, 'D') | ('D', _) => 'D',
            ('A', _) => 'A',
            ('R', _) => 'R',
            _ => 'M',
        };
        v.push((letter, path.to_string()));
    }
    v
}

/// A changed file for the changed-files picker (the palette's `changed_file_picker`): status letter, path, first changed line (from 0), diff against HEAD.
#[derive(Clone, Debug)]
pub struct Changed {
    pub letter: char,
    pub path: PathBuf,
    pub line: usize,
    pub diff: std::sync::Arc<Vec<crate::editdiff::FileDiff>>,
}

/// Files diffed at most (a huge untracked tree lists without previews past this).
const MAX_DIFFED: usize = 300;

/// `git status` from `cwd`'s repository, each file diffed HEAD → working tree (runs on a worker thread).
fn changed(cwd: &Path) -> Result<Vec<Changed>, String> {
    let top = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"], None)?.trim());
    let out = git(&top, &["status", "--porcelain=v1", "-z", "--untracked-files=all"], None)?;
    let files: Vec<(usize, (char, String))> = parse_status(&out).into_iter().enumerate().collect();
    // A `git show` and a diff per file, on every core (one after another, 300 files took seconds)
    let parts = std::thread::available_parallelism().map_or(4, |n| n.get()).min(8);
    let top = &top;
    Ok(std::thread::scope(|s| {
        let handles: Vec<_> = files
            .chunks(files.len().div_ceil(parts).max(1))
            .map(|c| s.spawn(move || c.iter().map(|(i, (letter, rel))| one(top, *i, *letter, rel)).collect()))
            .collect();
        handles.into_iter().flat_map(|h| h.join().unwrap_or_else(|_| Vec::new())).collect()
    }))
}

/// One changed file, diffed HEAD → working tree (the first `MAX_DIFFED` of them).
fn one(top: &Path, i: usize, letter: char, rel: &str) -> Changed {
    use crate::editdiff::{FileOp, LineKind, diff_file};
    let path = top.join(rel);
    let (before, after) = if i < MAX_DIFFED {
        let head = git(top, &["show", &format!("HEAD:{rel}")], None).ok();
        let now = std::fs::read(&path)
            .ok()
            .filter(|b| b.len() < 1 << 20)
            .map(|b| String::from_utf8_lossy(&b).into_owned());
        (head, now)
    } else {
        (None, None)
    };
    let op = match (&before, &after) {
        (None, Some(_)) => FileOp::Create,
        (Some(_), None) => FileOp::Delete,
        _ => FileOp::Edit,
    };
    let rope = |s: Option<String>| Rope::from_str(&s.unwrap_or_default());
    let diff = diff_file(path.clone(), op, rope(before), rope(after));
    // First changed line: the first added or removed line's number
    let line = diff
        .lines
        .iter()
        .find(|l| !matches!(l.kind, LineKind::Context | LineKind::Gap))
        .map_or(0, |l| l.number.saturating_sub(1));
    Changed { letter, path, line, diff: std::sync::Arc::new(vec![diff]) }
}

/// One line's blame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blame {
    pub author: String,
    /// Unix seconds.
    pub time: i64,
    pub summary: String,
    /// Not committed yet (changed in the working tree or the buffer).
    pub uncommitted: bool,
}

/// `git blame --porcelain` → per line (from 0).
pub fn parse_blame(out: &str) -> Vec<Option<Blame>> {
    use std::collections::HashMap;
    let mut commits: HashMap<String, Blame> = HashMap::new();
    let mut lines: Vec<Option<Blame>> = Vec::new();
    let mut cur: Option<(String, usize)> = None;
    for l in out.lines() {
        if let Some(_content) = l.strip_prefix('\t') {
            if let Some((sha, line)) = cur.take() {
                if lines.len() < line {
                    lines.resize(line, None);
                }
                lines[line - 1] = commits.get(&sha).cloned();
            }
            continue;
        }
        let mut words = l.split(' ');
        let first = words.next().unwrap_or_default();
        if first.len() == 40 && first.bytes().all(|b| b.is_ascii_hexdigit()) {
            let line = words.nth(1).and_then(|n| n.parse().ok()).unwrap_or(0);
            commits.entry(first.to_string()).or_insert_with(|| Blame {
                author: String::new(),
                time: 0,
                summary: String::new(),
                uncommitted: first.bytes().all(|b| b == b'0'),
            });
            cur = Some((first.to_string(), line));
            continue;
        }
        let Some((sha, _)) = &cur else { continue };
        let Some(b) = commits.get_mut(sha) else { continue };
        let rest = l.split_once(' ').map_or("", |x| x.1);
        match first {
            "author" => b.author = rest.to_string(),
            "author-time" => b.time = rest.parse().unwrap_or(0),
            "summary" => b.summary = rest.to_string(),
            _ => {}
        }
    }
    lines
}

/// `3 days ago` — the largest unit that fits.
pub fn ago(secs: i64) -> String {
    let s = secs.max(0);
    let (n, unit) = match s {
        0..60 => return "just now".into(),
        60..3600 => (s / 60, "minute"),
        3600..86_400 => (s / 3600, "hour"),
        86_400..2_592_000 => (s / 86_400, "day"),
        2_592_000..31_536_000 => (s / 2_592_000, "month"),
        _ => (s / 31_536_000, "year"),
    };
    format!("{n} {unit}{} ago", if n == 1 { "" } else { "s" })
}

/// What the cursor line's blame says at the line end.
pub fn blame_text(b: &Blame, now: i64) -> String {
    if b.uncommitted {
        return "not committed yet".into();
    }
    format!("{}, {} · {}", b.author, ago(now - b.time), b.summary)
}

/// git's hooks turned off — for what tarae itself runs in a checkout, outside the sandbox (session making,
/// ending and bringing, a review's fetch, the rebase — `rebase::GIT`): a `core.hooksPath` inside a checkout
/// is a folder Claude can write (review, 2026-10-04). The user's own git-tab actions keep their hooks.
pub(crate) const NO_HOOKS: [&str; 2] = ["-c", "core.hooksPath=/dev/null"];

/// `git` with `NO_HOOKS`.
pub(crate) fn git_no_hooks(dir: &Path, args: &[&str]) -> Result<String, String> {
    let all: Vec<&str> = NO_HOOKS.iter().chain(args).copied().collect();
    git(dir, &all, None)
}

pub(crate) fn git(dir: &Path, args: &[&str], input: Option<&str>) -> Result<String, String> {
    #[cfg(test)]
    not_in_this_repository(dir, args);
    let mut cmd = Command::new("git");
    // No terminal of its own (`setsid`): ssh or gpg asking for a passphrase can't take ours (it fails instead)
    crate::dap::setsid(&mut cmd);
    let mut child = cmd
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        // A read (status, diff) never takes the index lock — a commit or stage running then would fail on it
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("git: {e}"))?;
    if let (Some(s), Some(mut stdin)) = (input, child.stdin.take()) {
        let s = s.to_string();
        crate::event::thread("tarae-git-feed", move || {
            use std::io::Write as _;
            let _ = stdin.write_all(s.as_bytes());
        });
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(err.lines().find(|l| !l.trim().is_empty()).unwrap_or("git failed").to_string());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Tests only: whether git run in `dir` would work on tarae's own repository (the nearest `.git` up from it
/// is this crate's).
#[cfg(test)]
fn in_this_repository(dir: &Path) -> bool {
    let own = std::fs::canonicalize(env!("CARGO_MANIFEST_DIR")).unwrap();
    let dir = dir.ancestors().find_map(|d| std::fs::canonicalize(d).ok()).unwrap_or_default();
    dir.ancestors().find(|d| d.join(".git").exists()) == Some(own.as_path())
}

/// Tests only: no test runs git in tarae's own repository — even a read can write there (`git diff`
/// refreshes the index), and a sandboxed run reports it as a denial. One that does stops the whole run,
/// naming the command and where it came from (past libtest's capture: an abort loses what it captured).
#[cfg(test)]
pub(crate) fn not_in_this_repository(dir: &Path, args: &[&str]) {
    if in_this_repository(dir) {
        // (a job's worker is named after the test it ran for — `event::run`)
        let test = std::thread::current().name().unwrap_or("a test").to_string();
        let line = format!(
            "{test} ran `git {}` in tarae's own repository ({}) — give it a repository of its own in its \
             temp folder\n{}\n",
            args.join(" "),
            dir.display(),
            std::backtrace::Backtrace::force_capture()
        );
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        std::process::abort();
    }
}

/// Blame state for the current document (on/off is `editor.git-blame`).
#[derive(Default)]
pub struct BlameState {
    /// (doc, version) the lines are for · a run is going.
    pub of: Option<(crate::document::DocId, u64)>,
    pub lines: Vec<Option<Blame>>,
    pub running: bool,
}

// ── On the editor ─────────────────────────────────────────────────────────────

impl Editor {
    /// What the quick keys after `space g` could act on in the code: (a hunk under the cursor, a conflict
    /// there or below — `conflict_take`'s reach).
    pub(crate) fn git_code_here(&mut self) -> (bool, bool) {
        self.conflicts_now();
        let line = self.cursor_line();
        let doc = self.doc();
        (hunk_at(&doc.git_hunks, line).is_some(), doc.conflicts.iter().any(|c| c.end >= line))
    }

    /// The current document's hunk under the cursor, with its index text.
    fn cursor_hunk(&mut self) -> Option<(Hunk, std::sync::Arc<String>)> {
        let doc = self.doc();
        let Some(base) = doc.git_base.clone() else {
            self.set_status("not tracked by git (or outside a repository)");
            return None;
        };
        let line = self.cursor_line();
        match hunk_at(&doc.git_hunks, line) {
            Some(h) => Some((h.clone(), base)),
            None => {
                self.set_status("no change here (]g jumps to the next)");
                None
            }
        }
    }

    /// The hunk as a card (the palette's `git_preview_hunk`): removed lines, then added ones, in the gutter's colors.
    /// Selects a hunk's lines (a deletion: the line start at its spot).
    pub fn select_hunk(&mut self, h: &Hunk) {
        let text = &self.doc().text;
        let last = mv::last_line(text);
        let from = mv::line_start(text, h.lines.start.min(last));
        let to = if h.lines.is_empty() { from } else { mv::line_full_end(text, (h.lines.end - 1).min(last)) };
        let sel = if to > from {
            crate::selection::Selection::single(crate::selection::Range::new(from, to))
        } else {
            crate::selection::Selection::point(from)
        };
        self.doc_mut().set_selection(sel);
    }

    pub fn git_preview_hunk(&mut self) {
        let Some((h, base)) = self.cursor_hunk() else { return };
        let t = &self.theme;
        let color =
            |k: &str| crate::theme::Style { fg: t.try_get(k).and_then(|s| s.fg), ..Default::default() };
        let (minus, plus) = (color("diff.minus"), color("diff.plus"));
        let old = base_text(&base, &h);
        let doc = self.doc();
        let (a, b) = line_bytes(&doc.text, &h.lines);
        let new = doc.text.byte_slice(a..b).to_string();
        let mut lines = Vec::new();
        for (mark, chunk, style) in [("- ", &old, minus), ("+ ", &new, plus)] {
            for l in chunk.lines() {
                let text = l.replace('\t', "    ");
                // Long lines wrap in the card, continuing under the code (past the marker and indent)
                let indent = 2 + text.chars().take_while(|c| *c == ' ').count();
                let spans = vec![
                    crate::markdown::Span { text: mark.to_string(), style },
                    crate::markdown::Span { text, style },
                ];
                lines.push(crate::markdown::Line::Text { spans, indent });
            }
        }
        self.popup = Some(lines);
    }

    /// `d` on a hunk in the git tab (`space g d` from the code) — put its index lines back (one undo step).
    pub fn git_reset_hunk(&mut self) {
        if let Some(of) = self.doc().diff_of.clone() {
            return self.set_warning(format!("these are changes against {of} — :diff-off first"));
        }
        let Some((h, base)) = self.cursor_hunk() else { return };
        let (from, to, text) = reset_change(&self.doc().text, &base, &h);
        self.with_group(|cx| {
            let doc = cx.editor.doc_mut();
            let tx = crate::transaction::Transaction::new(vec![crate::transaction::Change {
                from,
                to,
                insert: text,
            }]);
            let sel = crate::selection::Selection::point(tx.map_pos(from, crate::transaction::Assoc::Before));
            doc.apply_with(&tx, sel);
        });
        self.note("hunk reset");
    }

    /// `space g s` — stage just this hunk (what the buffer has, saved or not).
    pub fn git_stage_hunk(&mut self) {
        if let Some(of) = self.doc().diff_of.clone() {
            return self
                .set_warning(format!("these are changes against {of}, not the index — :diff-off first"));
        }
        let Some((h, base)) = self.cursor_hunk() else { return };
        let doc = self.doc();
        let Some(path) = doc.path.clone() else { return };
        let (id, text) = (doc.id, doc.text.clone());
        self.note("staging…");
        self.events.jobs().spawn(move || {
            let result = (|| {
                let dir = path.parent().ok_or("no folder")?;
                let top = PathBuf::from(git(dir, &["rev-parse", "--show-toplevel"], None)?.trim());
                let top = std::fs::canonicalize(&top).unwrap_or(top);
                let rel = path
                    .strip_prefix(&top)
                    .map_err(|_| "outside the repository")?
                    .to_string_lossy()
                    .into_owned();
                let p = patch(&rel, &base, &text, &h);
                git(&top, &["apply", "--cached", "--unidiff-zero", "--whitespace=nowarn", "-"], Some(&p))
                    .map(|_| ())
            })();
            move |ed: &mut Editor| match result {
                Ok(()) => {
                    ed.set_success("Staged the hunk");
                    ed.git_load_base(id);
                }
                Err(e) => ed.set_error(format!("stage: {e}")),
            }
        });
    }

    /// Changed and untracked files (`git status`), status letters in the gutter's colors, each
    /// file's diff against HEAD as the preview (computed on a worker thread), `+N −M` on the right.
    pub fn git_changed_files(&mut self) {
        self.git_changed_files_in(self.session.root.clone());
    }

    /// Changed files of `cwd`'s repository, labels relative to it.
    pub fn git_changed_files_in(&mut self, cwd: PathBuf) {
        self.note("reading git status…");
        self.events.jobs().spawn(move || {
            let result = changed(&cwd);
            move |ed: &mut Editor| {
                ed.status = None;
                let list = match result {
                    Ok(l) if l.is_empty() => return ed.set_status("nothing changed"),
                    Ok(l) => l,
                    Err(e) => return ed.set_error(e),
                };
                let items = list
                    .iter()
                    .enumerate()
                    .map(|(n, c)| {
                        let (glyph, scope) = match c.letter {
                            'A' => ("A", "diff.plus"),
                            '?' => ("?", "diff.plus"),
                            'D' => ("D", "diff.minus"),
                            'R' => ("R", "diff.delta"),
                            _ => ("M", "diff.delta"),
                        };
                        let d = &c.diff[0];
                        crate::picker::Item {
                            label: c.path.strip_prefix(&cwd).unwrap_or(&c.path).display().to_string(),
                            action: crate::picker::Action::ChangedFile(n),
                            hint: format!("+{} −{}", d.added, d.removed),
                            glyph: Some((glyph, scope)),
                        }
                    })
                    .collect();
                ed.session.changed_files = list;
                ed.open_picker(crate::picker::Picker::new("changed files", items, true).grouped(cwd), None);
            }
        });
    }

    /// `b` in the git tab — blame at the cursor line's end, on/off.
    pub fn git_toggle_blame(&mut self) {
        self.config.git_blame = !self.config.git_blame;
        self.blame.of = None;
        self.blame.lines.clear();
        self.note(if self.config.git_blame { "blame on (b in the git tab hides it)" } else { "blame off" });
    }

    /// Every event: blame the current document's text if it changed (shortly after input stops).
    pub fn blame_schedule(&mut self) {
        if !self.config.git_blame || self.blame.running || !self.git_auto {
            return;
        }
        let doc = self.doc();
        let Some(path) = doc.path.clone() else { return };
        if doc.loading || doc.git_base.is_none() || self.blame.of == Some((doc.id, doc.version())) {
            return;
        }
        let (id, version) = (doc.id, doc.version());
        self.blame.running = true;
        // Once the text has stood still that long: typing puts it off (it used to run back to back while
        // typing — hidden then anyway — and git blame over a long history can take seconds of a core)
        self.events.jobs().spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(250));
            move |ed: &mut Editor| {
                ed.unchanged = true; // (nothing to draw yet)
                if ed.doc().id == id && ed.doc().version() == version {
                    ed.blame_run(path);
                } else {
                    ed.blame.running = false; // (this event's own `blame_schedule` waits again)
                }
            }
        });
    }

    fn blame_run(&mut self, path: PathBuf) {
        // (a rope clone — the text is written out on the worker, not on every version here)
        let doc = self.doc();
        let (id, version, text) = (doc.id, doc.version(), doc.text.clone());
        self.events.jobs().spawn(move || {
            let text = text.to_string();
            let dir = path.parent().unwrap_or(Path::new("."));
            let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let out = git(dir, &["blame", "--porcelain", "--contents", "-", "--", &name], Some(&text));
            move |ed: &mut Editor| {
                ed.blame.running = false;
                if let Ok(out) = out {
                    ed.blame.lines = parse_blame(&out);
                }
                // Stale (typed meanwhile) → the next event asks again
                ed.blame.of = Some((id, version));
            }
        });
    }

    /// The cursor line's blame, if showing.
    pub fn blame_here(&self, line: usize) -> Option<String> {
        if !self.config.git_blame || self.blame.of.map(|o| o.0) != Some(self.doc().id) {
            return None;
        }
        let b = self.blame.lines.get(line)?.as_ref()?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs() as i64);
        Some(blame_text(b, now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::hunks;

    /// The tripwire's question: this crate's folders (a subfolder too, and one that isn't there yet) are
    /// this repository; a repository of a test's own, even one made under `target/`, is not.
    #[test]
    fn tests_know_this_repository() {
        let own = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert!(in_this_repository(own));
        assert!(in_this_repository(&own.join("src")));
        assert!(in_this_repository(&own.join("target/not-there-yet")));
        let dir = own.join(format!("target/tarae-own-repo-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let ok = std::process::Command::new("git").args(["init", "-q"]).current_dir(&dir).output().unwrap();
        assert!(ok.status.success());
        assert!(!in_this_repository(&dir), "a repository of its own");
        assert!(!in_this_repository(&std::env::temp_dir()));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Changed files come diffed and in `git status` order, however many workers took them.
    #[test]
    fn changed_files_are_diffed_in_order() {
        let dir = std::env::temp_dir().join(format!("tarae-changed-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let ok = std::process::Command::new("git").args(args).current_dir(&dir).output().unwrap();
            assert!(ok.status.success(), "git {args:?}");
        };
        let names: Vec<String> = (0..40).map(|i| format!("f{i:02}.txt")).collect();
        for n in &names {
            std::fs::write(dir.join(n), "a\nb\nc\n").unwrap();
        }
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
            &["config", "commit.gpgsign", "false"],
            &["add", "-A"],
            &["commit", "-q", "-m", "base"],
        ] {
            run(args);
        }
        for (i, n) in names.iter().enumerate() {
            std::fs::write(dir.join(n), format!("a\nb{i}\nc\n")).unwrap();
        }
        std::fs::write(dir.join("new.txt"), "fresh\n").unwrap();
        let got = changed(&dir).unwrap();
        let rels: Vec<String> =
            got.iter().map(|c| c.path.file_name().unwrap().to_string_lossy().into_owned()).collect();
        let mut want = names.clone();
        want.push("new.txt".into());
        assert_eq!(rels, want);
        assert!(got[..40].iter().all(|c| c.letter == 'M' && c.line == 1), "the changed line is b");
        assert_eq!(got[40].letter, '?');
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hunk_under_the_cursor_and_reset() {
        let base = "a\nb\nc\nd\n";
        let now = Rope::from_str("a\nB\nc\n");
        let hs = hunks(base, &now.to_string());
        assert_eq!(hunk_at(&hs, 1).map(|h| h.kind), Some(HunkKind::Modified));
        assert_eq!(hunk_at(&hs, 2).map(|h| h.kind), Some(HunkKind::Deleted), "the deletion's mark line");
        assert!(hunk_at(&hs, 0).is_none());
        let (a, b, t) = reset_change(&now, base, hunk_at(&hs, 1).unwrap());
        assert_eq!((a, b, t.as_str()), (2, 4, "b\n"));
    }

    #[test]
    fn zero_context_patch() {
        let base = "a\nb\nc\n";
        let now = Rope::from_str("a\nB\nc\nd\n");
        let hs = hunks(base, &now.to_string());
        assert_eq!(
            patch("x.txt", base, &now, &hs[0]),
            "diff --git a/x.txt b/x.txt\n--- a/x.txt\n+++ b/x.txt\n@@ -2,1 +2,1 @@\n-b\n+B\n"
        );
        assert!(
            patch("x.txt", base, &now, &hs[1]).contains("@@ -3,0 +4,1 @@\n+d\n"),
            "insertion after line 3"
        );
        let now = Rope::from_str("a\nb\nC");
        let hs = hunks(base, &now.to_string());
        assert!(
            patch("x", base, &now, &hs[0]).ends_with("+C\n\\ No newline at end of file\n"),
            "{}",
            patch("x", base, &now, &hs[0])
        );
    }

    #[test]
    fn status_and_blame_parsing() {
        let st = "M  src/a.rs\0?? new.txt\0R  b.rs\0old.rs\0 D gone.rs\0";
        assert_eq!(
            parse_status(st),
            [
                ('M', "src/a.rs".into()),
                ('?', "new.txt".into()),
                ('R', "b.rs".into()),
                ('D', "gone.rs".into())
            ]
        );
        let sha = "a".repeat(40);
        let zero = "0".repeat(40);
        let out = format!(
            "{sha} 1 1 2\nauthor Kim\nauthor-time 1000\nsummary Add a\n\tline one\n{sha} 2 2\n\tline two\n{zero} 3 3 1\nauthor Not Committed Yet\n\tnew\n"
        );
        let b = parse_blame(&out);
        assert_eq!(b.len(), 3);
        assert_eq!(b[1].as_ref().map(|b| b.author.as_str()), Some("Kim"), "metadata only on the first line");
        assert_eq!(blame_text(b[0].as_ref().unwrap(), 1000 + 3 * 86_400), "Kim, 3 days ago · Add a");
        assert_eq!(blame_text(b[2].as_ref().unwrap(), 0), "not committed yet");
        assert_eq!(ago(90), "1 minute ago");
    }
}
