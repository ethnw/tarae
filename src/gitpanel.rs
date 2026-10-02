//! Git in the editor — the side panel's git tab (`space g`, lazygit beside the code): what's staged and what
//! isn't, each as a tree with status letters and `+a −r`, the branch and how far it is from its upstream.
//! A changed file opens to its hunks and conflicts (rows under it — sidebar.rs `detail_rows`); the keys
//! (`sidebar::git_key`): `s`/`u` stage/unstage (a file, folder, section — or a hunk), `d` discards (a file
//! twice; a hunk: its index lines back), `o`/`t`/`a` on a conflict, `c` commit (a message buffer — `:w`
//! commits, `:q` cancels), `A` amend, `p`/`P` pull/push, `f` fetch, `B` branches, `L` log, `z`/`Z` stash,
//! `b` blame, `e`/`D` the file in the editor / against HEAD, `?` all of them. From the code the next key
//! after `space g` acts on what's under the cursor. Every git command runs on a worker thread, one at a
//! time; remotes never wait for a password prompt (`GIT_TERMINAL_PROMPT=0`).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::document::DocId;
use crate::editor::Editor;
use crate::gitmenu::git;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// M modified · A added · D deleted · R renamed · T type · ? untracked · U conflicted
    pub letter: char,
    /// From the repository's top.
    pub rel: String,
    /// Lines added / removed (None = binary or not counted).
    pub added: Option<usize>,
    pub removed: Option<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    pub top: PathBuf,
    pub branch: String,
    pub upstream: Option<String>,
    pub ahead: usize,
    pub behind: usize,
    pub staged: Vec<Entry>,
    pub unstaged: Vec<Entry>,
    /// Which read this is (the side panel takes its rows when it changes).
    pub generation: u64,
    /// The git tab's rows for it — built with it, on the worker (20k changed files: ~7 ms).
    pub rows: std::sync::Arc<[crate::sidebar::Line]>,
}

impl Status {
    /// With the git tab's rows built.
    pub fn with_rows(mut self) -> Self {
        self.rows = crate::sidebar::git_rows(&self).into();
        self
    }

    /// `main ↑1 ↓2` — for the panel's header.
    pub fn branch_line(&self) -> String {
        let mut s = self.branch.clone();
        if self.ahead > 0 {
            s += &format!(" ↑{}", self.ahead);
        }
        if self.behind > 0 {
            s += &format!(" ↓{}", self.behind);
        }
        s
    }
}

/// `git status --porcelain=v2 -z --branch` → branch, upstream, ahead/behind, staged and unstaged entries
/// (an entry changed in both shows in both; untracked and conflicted files are unstaged).
pub fn parse_v2(out: &str) -> Status {
    let mut st = Status::default();
    let mut it = out.split('\0').filter(|s| !s.is_empty());
    let entry = |letter: char, rel: &str| Entry { letter, rel: rel.to_string(), added: None, removed: None };
    while let Some(e) = it.next() {
        if let Some(h) = e.strip_prefix("# ") {
            match h.split_once(' ') {
                Some(("branch.head", b)) => {
                    st.branch = if b == "(detached)" { "detached".into() } else { b.into() }
                }
                Some(("branch.upstream", u)) => st.upstream = Some(u.into()),
                Some(("branch.ab", ab)) => {
                    let mut n = ab.split(' ').map(|x| x[1..].parse::<usize>().unwrap_or(0));
                    (st.ahead, st.behind) = (n.next().unwrap_or(0), n.next().unwrap_or(0));
                }
                _ => {}
            }
            continue;
        }
        let kind = e.as_bytes()[0];
        match kind {
            b'1' | b'2' => {
                let fields: Vec<&str> = e.splitn(if kind == b'1' { 9 } else { 10 }, ' ').collect();
                let (Some(xy), Some(path)) = (fields.get(1), fields.last()) else { continue };
                if kind == b'2' {
                    it.next(); // where it was renamed from
                }
                let (x, y) = (xy.as_bytes()[0] as char, xy.as_bytes()[1] as char);
                if x != '.' {
                    st.staged.push(entry(x, path));
                }
                if y != '.' {
                    st.unstaged.push(entry(y, path));
                }
            }
            b'u' => {
                if let Some(path) = e.splitn(11, ' ').nth(10) {
                    st.unstaged.push(entry('U', path));
                }
            }
            b'?' => st.unstaged.push(entry('?', &e[2..])),
            _ => {}
        }
    }
    st
}

/// `git diff --numstat -z` → path → (added, removed); binary files count `-`.
fn numstat(out: &str) -> HashMap<String, (Option<usize>, Option<usize>)> {
    let mut m = HashMap::new();
    let mut it = out.split('\0');
    while let Some(e) = it.next() {
        let mut f = e.splitn(3, '\t');
        let (Some(a), Some(r), Some(path)) = (f.next(), f.next(), f.next()) else { continue };
        // A rename: the path comes as two more fields, old then new
        let path = if path.is_empty() {
            it.next();
            it.next().unwrap_or_default()
        } else {
            path
        };
        m.insert(path.to_string(), (a.parse().ok(), r.parse().ok()));
    }
    m
}

/// The status of `cwd`'s repository with line counts (on a worker thread).
pub fn status(cwd: &Path) -> Result<Status, String> {
    let top = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"], None)?.trim());
    let top = std::fs::canonicalize(&top).unwrap_or(top);
    let out = git(&top, &["status", "--porcelain=v2", "-z", "--branch", "--untracked-files=all"], None)?;
    let mut st = parse_v2(&out);
    st.top = top.clone();
    let staged = numstat(&git(&top, &["diff", "--cached", "--numstat", "-z"], None).unwrap_or_default());
    // (`diff-files`, not `diff`: over a file whose stat changed, `diff` refreshes the index — taking its lock
    // whatever `GIT_OPTIONAL_LOCKS` says)
    let unstaged = numstat(&git(&top, &["diff-files", "--numstat", "-z"], None).unwrap_or_default());
    for (list, counts) in [(&mut st.staged, &staged), (&mut st.unstaged, &unstaged)] {
        for e in list.iter_mut() {
            if let Some(&(a, r)) = counts.get(&e.rel) {
                (e.added, e.removed) = (a, r);
            } else if e.letter == '?' {
                // Untracked: every line is new (counted for files under 512 KB)
                let path = top.join(&e.rel);
                let small = std::fs::metadata(&path).is_ok_and(|m| m.len() < 512 * 1024);
                let n =
                    small.then(|| std::fs::read(&path).ok()).flatten().filter(|b| !b.contains(&0)).map(|b| {
                        b.iter().filter(|&&c| c == b'\n').count()
                            + usize::from(!b.is_empty() && !b.ends_with(b"\n"))
                    });
                (e.added, e.removed) = (n, Some(0));
            }
        }
    }
    Ok(st)
}

/// Git with a time limit on its patience for remotes: no prompts (push/pull fail fast instead of waiting
/// for a password nobody will type).
fn git_remote(dir: &Path, args: &[&str]) -> Result<String, String> {
    #[cfg(test)]
    crate::gitmenu::not_in_this_repository(dir, args);
    let mut cmd = std::process::Command::new("git");
    crate::dap::new_session(&mut cmd);
    let out = cmd
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(std::process::Stdio::null())
        .output()
        .map_err(|e| format!("git: {e}"))?;
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    if !out.status.success() {
        let err = text(&out.stderr);
        return Err(err.lines().rfind(|l| !l.trim().is_empty()).unwrap_or("git failed").trim().to_string());
    }
    Ok(format!("{}{}", text(&out.stdout), text(&out.stderr)))
}

/// Queued work in the repository (on a worker, given its top) and what to say when it's done.
pub type GitWork = Box<dyn FnOnce(&Path) -> Result<String, String> + Send>;
pub type GitDone = Box<dyn FnOnce(&mut Editor, Result<String, String>) + Send>;

/// A commit being written: its message buffer, the repository, whether it amends.
pub struct Committing {
    pub doc: DocId,
    pub top: PathBuf,
    pub amend: bool,
}

impl Editor {
    /// Where git runs: the side panel's folder, else where tarae started.
    fn git_cwd(&self) -> PathBuf {
        self.sidebar
            .as_ref()
            .map_or_else(|| std::env::current_dir().unwrap_or_default(), |s| s.root().to_path_buf())
    }

    /// `space g` — the side panel's git tab, with the keys; from the code, on the row for what's under the
    /// cursor (its hunk or conflict — the file opened to them — else its file). Again: closed. During a
    /// review the tab and the review's rail take turns in the column: again = the rail back, with the keys
    /// (so the next one doesn't land in the code — `d` there would delete the selected change).
    pub fn git_panel(&mut self) {
        if self.code_review.is_some() && !self.review_rail_up() {
            return crate::codereview::rail_focus(self);
        }
        let review = self.code_review.is_some();
        if let Some(r) = self.code_review.as_mut() {
            r.behind = true;
            r.focus = false;
            // (a tab focused behind the rail would close instead of coming to the front)
            if let Some(s) = self.sidebar.as_mut() {
                s.focus = false;
            }
        } else if self.sidebar.as_ref().is_some_and(|s| s.mode == crate::sidebar::Mode::Git && s.focus) {
            self.sidebar = None;
            return;
        }
        let aim = self.doc().path.clone().map(|p| (p, self.cursor_line()));
        self.sidebar_show(crate::sidebar::Mode::Git);
        if let Some(s) = self.sidebar.as_mut() {
            s.aim = aim;
            // (no quick `s`/`d` on the change under the cursor in a review: it's against the review's base)
            s.from_code = !review;
        }
        self.git_refresh();
    }

    /// `f` in the git tab — fetch every remote (pruned); it never waits on a password prompt.
    pub fn git_fetch(&mut self) {
        self.set_status("fetching…");
        self.git_do(
            Box::new(|top| git_remote(top, &["fetch", "--all", "--prune"])),
            Box::new(|ed, r| match r {
                Ok(_) => ed.set_success("Fetched"),
                Err(e) => ed.set_error(format!("fetch: {e}")),
            }),
        );
    }

    /// `z` / `Z` in the git tab — put the changes away (new files too) / take the last ones back.
    pub fn git_stash(&mut self, pop: bool) {
        let args: &'static [&'static str] =
            if pop { &["stash", "pop"] } else { &["stash", "push", "--include-untracked"] };
        self.git_do(
            Box::new(move |top| git(top, args, None)),
            Box::new(move |ed, r| match r {
                Ok(_) if pop => ed.set_success("Took the stashed changes back"),
                Ok(out) if out.contains("No local changes") => ed.set_status("nothing to stash"),
                Ok(_) => ed.set_success("Stashed — Z takes them back"),
                Err(e) => ed.set_error(format!("stash: {e}")),
            }),
        );
    }

    /// `?` in the git tab — every key it takes, as a card.
    pub fn git_help(&mut self) {
        let md = "**git tab**\n\n\
            `enter` a changed file: its hunks and conflicts · a hunk: there in the code\n\
            `s` `u` stage · unstage (a file, folder, section — or the hunk)\n\
            `d` discard (twice for a file; a hunk: its index lines back)\n\
            `o` `t` `a` a conflict: keep ours · theirs · both\n\
            `e` open in the editor · `D` diff against HEAD\n\
            `c` commit · `C` commit, Claude drafting the message · `A` amend\n\
            `p` pull · `P` push · `f` fetch\n\
            `B` branches · `L` log · `z` stash · `Z` take it back\n\
            `b` blame on/off · `r` read again · `esc` the code";
        self.popup = Some(crate::markdown::render(md, &self.theme, None));
    }

    /// Read the status again (worker) — the panel follows. Only the latest read lands (a slow one started
    /// before a stage doesn't undo what the panel shows).
    pub fn git_refresh(&mut self) {
        let cwd = self.git_cwd();
        self.git_reads += 1;
        let read = self.git_reads;
        self.events.jobs().spawn(move || {
            let result = status(&cwd).map(Status::with_rows);
            move |ed: &mut Editor| {
                if read != ed.git_reads {
                    return;
                }
                match result {
                    Ok(mut st) => {
                        st.generation = read;
                        ed.git_status = Some(st);
                    }
                    Err(e) => {
                        ed.git_status = None;
                        if ed.sidebar.as_ref().is_some_and(|s| s.mode == crate::sidebar::Mode::Git) {
                            ed.set_warning(format!("git: {e}"));
                        }
                    }
                }
            }
        });
    }

    /// Queue work in the repository (on a worker, one at a time — two `git add`s at once fight over the
    /// index lock); `done` says how it went, then the status is read again.
    fn git_do(&mut self, work: GitWork, done: GitDone) {
        if self.git_status.is_none() {
            return self.set_warning("not a git repository");
        }
        self.git_queue.push_back((work, done));
        self.git_next();
    }

    fn git_next(&mut self) {
        if self.git_busy {
            return;
        }
        let (Some((work, done)), Some(top)) =
            (self.git_queue.pop_front(), self.git_status.as_ref().map(|s| s.top.clone()))
        else {
            return;
        };
        self.git_busy = true;
        self.events.jobs().spawn(move || {
            let result = work(&top);
            move |ed: &mut Editor| {
                ed.git_busy = false;
                done(ed, result);
                ed.git_next();
                if ed.git_queue.is_empty() {
                    ed.git_refresh();
                    ed.git_reload_bases();
                }
            }
        });
    }

    /// `s` / `u` on a row: stage / unstage it (a file, a folder, or `""` = the whole section).
    pub fn git_stage(&mut self, rel: &str, stage: bool) {
        let path = if rel.is_empty() { ".".to_string() } else { rel.to_string() };
        let args: Vec<String> = if stage {
            vec!["add".into(), "--all".into(), "--".into(), path]
        } else {
            vec!["restore".into(), "--staged".into(), "--".into(), path]
        };
        let what = if rel.is_empty() { "everything".to_string() } else { rel.to_string() };
        self.git_do(
            Box::new(move |top| {
                let a: Vec<&str> = args.iter().map(String::as_str).collect();
                git(top, &a, None)
            }),
            Box::new(move |ed, r| match r {
                Ok(_) => ed.set_status(format!("{} {what}", if stage { "Staged" } else { "Unstaged" })),
                Err(e) => ed.set_error(format!("git: {e}")),
            }),
        );
    }

    /// `d` on an unstaged row (a file, a folder, `""` = the whole section) — the first press says what goes,
    /// the second (within 3 s, same row) does it: the changed files it lists go back to what's staged (an open
    /// one reloads, its old text one `u` away), the untracked files it lists are deleted, file by file (and
    /// folders that end up empty) — never a folder whole, never the repository.
    pub fn git_discard(&mut self, rel: &str) {
        let Some(st) = self.git_status.as_ref() else { return };
        let under = |e: &&Entry| rel.is_empty() || e.rel == rel || e.rel.starts_with(&format!("{rel}/"));
        let tracked: Vec<String> =
            st.unstaged.iter().filter(under).filter(|e| e.letter != '?').map(|e| e.rel.clone()).collect();
        let untracked: Vec<String> =
            st.unstaged.iter().filter(under).filter(|e| e.letter == '?').map(|e| e.rel.clone()).collect();
        if tracked.is_empty() && untracked.is_empty() {
            return;
        }
        let fresh =
            self.git_discard_asked.as_ref().is_some_and(|(r, t)| r == rel && t.elapsed().as_secs() < 3);
        if !fresh {
            self.git_discard_asked = Some((rel.to_string(), Instant::now()));
            let what = if rel.is_empty() { "all changes".to_string() } else { rel.to_string() };
            let mut parts = Vec::new();
            if !tracked.is_empty() {
                parts.push(format!(
                    "throws away the changes in {} file{}",
                    tracked.len(),
                    if tracked.len() == 1 { "" } else { "s" }
                ));
            }
            if !untracked.is_empty() {
                parts.push(format!(
                    "deletes {} new file{}",
                    untracked.len(),
                    if untracked.len() == 1 { "" } else { "s" }
                ));
            }
            return self.set_warning(format!("d again {} — {what}", parts.join(" and ")));
        }
        self.git_discard_asked = None;
        let n = tracked.len() + untracked.len();
        self.git_do(
            Box::new(move |top| {
                if !tracked.is_empty() {
                    let mut args = vec!["restore", "--"];
                    args.extend(tracked.iter().map(String::as_str));
                    git(top, &args, None)?;
                }
                for rel in &untracked {
                    let path = top.join(rel);
                    std::fs::remove_file(&path).map_err(|e| format!("{rel}: {e}"))?;
                    // Folders it leaves empty go too — up to, never including, the repository's top
                    let mut dir = path.parent();
                    while let Some(d) = dir.filter(|d| *d != top && d.starts_with(top)) {
                        if std::fs::remove_dir(d).is_err() {
                            break; // not empty
                        }
                        dir = d.parent();
                    }
                }
                Ok(String::new())
            }),
            Box::new(move |ed, r| match r {
                Ok(_) => ed.set_status(format!("Discarded {n} file{}", if n == 1 { "" } else { "s" })),
                Err(e) => ed.set_error(format!("discard: {e}")),
            }),
        );
    }

    /// `c` / `A` — write the commit message in a buffer (`.git/COMMIT_EDITMSG`; amending starts from the
    /// last message). `:w` commits, `:q` cancels. `C`: Claude drafts it as the buffer opens (`draft`).
    pub fn git_commit_start(&mut self, amend: bool) {
        self.git_commit_open(amend, false);
    }

    pub fn git_commit_open(&mut self, amend: bool, draft: bool) {
        let Some(st) = self.git_status.as_ref() else { return self.set_warning("not a git repository") };
        let (top, branch) = (st.top.clone(), st.branch.clone());
        // Queued behind a stage still running, and what's staged read then — not the status shown, which
        // may be from before it
        self.git_do(
            Box::new(move |top| {
                let staged = git(top, &["diff", "--cached", "--name-status"], None)?;
                if staged.trim().is_empty() && !amend {
                    return Ok(String::new());
                }
                let dir = git(top, &["rev-parse", "--absolute-git-dir"], None)?;
                let file = PathBuf::from(dir.trim()).join("COMMIT_EDITMSG");
                let last = if amend { git(top, &["log", "-1", "--format=%B"], None)? } else { String::new() };
                let mut text = format!("{}\n", last.trim_end());
                text += "\n# :w commits · :q cancels · :commit-draft asks Claude · # lines are left out\n";
                text += &format!(
                    "# {} on {branch}\n",
                    if amend { "Amending the last commit" } else { "Committing" }
                );
                for l in staged.lines() {
                    let (letter, rel) = l.split_once('\t').unwrap_or(("M", l));
                    text += &format!("#   {} {}\n", &letter[..1], rel.replace('\t', " → "));
                }
                std::fs::write(&file, text).map_err(|e| e.to_string())?;
                Ok(file.to_string_lossy().into_owned())
            }),
            Box::new(move |ed, r| match r {
                Ok(file) if file.is_empty() => {
                    ed.set_warning("nothing staged — s stages a file (or a folder, or the whole section)")
                }
                Ok(file) => {
                    if let Err(e) = ed.open(std::path::Path::new(&file)) {
                        return ed.set_error(format!("{e:#}"));
                    }
                    let doc = ed.doc_mut();
                    doc.throwaway = true;
                    doc.set_selection(crate::selection::Selection::point(0));
                    let id = doc.id;
                    ed.committing = Some(Committing { doc: id, top, amend });
                    if let Some(s) = ed.sidebar.as_mut() {
                        s.focus = false;
                    }
                    // A new message: typed at once. Amending: the last message is there to change, not to
                    // type over the top of — normal mode
                    if draft {
                        ed.commit_draft();
                    } else if !amend {
                        ed.mode = crate::editor::Mode::Insert;
                    }
                }
                Err(e) => ed.set_error(format!("commit: {e}")),
            }),
        );
    }

    /// A buffer was saved: the commit message's commits (and its buffer closes); the panel reads the status
    /// again.
    pub fn git_saved(&mut self, id: DocId) {
        if let Some(c) = self.committing.take_if(|c| c.doc == id) {
            let Some(file) = self.docs.iter().find(|d| d.id == id).and_then(|d| d.path.clone()) else {
                return;
            };
            let (top, amend) = (c.top, c.amend);
            // In the git queue — after a stage still running, never beside another git that writes
            self.git_do(
                Box::new(move |top| {
                    let f = file.to_string_lossy().into_owned();
                    let mut args = vec!["commit", "--cleanup=strip", "-F", f.as_str()];
                    if amend {
                        args.push("--amend");
                    }
                    git(top, &args, None).and_then(|_| git(top, &["log", "-1", "--format=%h %s"], None))
                }),
                Box::new(move |ed, result| match result {
                    // Its buffer goes (wherever you are by now — a hook can take a while)
                    Ok(line) => {
                        ed.set_success(format!("Committed {}", line.trim()));
                        ed.close_doc(id);
                    }
                    // An empty message aborts — the buffer stays for another try
                    Err(e) => {
                        ed.committing = Some(Committing { doc: id, top, amend });
                        ed.set_error(format!("commit: {e}"));
                    }
                }),
            );
            return;
        }
        if self.sidebar.as_ref().is_some_and(|s| s.mode == crate::sidebar::Mode::Git) {
            self.git_refresh();
        }
    }

    /// `C` · `:commit-draft` — Claude drafts the commit message from what's staged (amending: the whole
    /// commit), in the style of the repository's recent messages. Read on a worker; the draft streams in and
    /// lands as the in-buffer review of the message part (above the `#` lines).
    pub fn commit_draft(&mut self) {
        let Some(c) = self.committing.as_ref().filter(|c| c.doc == self.doc().id) else {
            return self
                .set_status("open a commit message first — c in the git tab (or C: Claude drafts it)");
        };
        let (top, amend, id) = (c.top.clone(), c.amend, c.doc);
        self.set_status("Claude is drafting the commit message…");
        self.events.jobs().spawn(move || {
            let prompt = commit_prompt(&top, amend);
            move |ed: &mut Editor| {
                let prompt = match prompt {
                    Ok(p) => p,
                    Err(e) => return ed.set_error(format!("commit draft: {e}")),
                };
                let Some(doc) = ed.docs.iter().find(|d| d.id == id) else { return };
                // The message part: all before the first `#` line
                let text = doc.text.to_string();
                let end = text.match_indices('\n').map(|(i, _)| i + 1).find(|&i| text[i..].starts_with('#'));
                let end = end.unwrap_or(text.len());
                // Nothing written yet: the draft goes in at the top (a plain insertion to read, the blank
                // lines under it the gap before the `#` lines); amending: it's a new version of the message
                let end = if text[..end].trim().is_empty() { 0 } else { end };
                match crate::llm::draft(ed, id, "a commit message", &prompt, 0, end) {
                    // (its body at 72, like every message — Claude doesn't always: #21)
                    Ok(()) => {
                        if let Some(r) = ed.review.as_mut() {
                            r.wrap = Some(72);
                        }
                    }
                    Err(e) => ed.set_error(e),
                }
            }
        });
    }

    /// Whether the current buffer is a commit message being written.
    pub fn in_commit_message(&self) -> bool {
        self.committing.as_ref().is_some_and(|c| c.doc == self.doc().id)
    }

    /// `P` / `p` — push (setting the upstream the first time) / pull fast-forward only.
    pub fn git_push_pull(&mut self, push: bool) {
        let Some(st) = self.git_status.clone() else { return self.set_warning("not a git repository") };
        let args: Vec<String> = match (push, &st.upstream) {
            (true, Some(_)) => vec!["push".into()],
            (true, None) => vec!["push".into(), "-u".into(), "origin".into(), "HEAD".into()],
            (false, _) => vec!["pull".into(), "--ff-only".into()],
        };
        self.set_status(if push { "Pushing…" } else { "Pulling…" });
        let top = st.top.clone();
        self.events.jobs().spawn(move || {
            let a: Vec<&str> = args.iter().map(String::as_str).collect();
            let result = git_remote(&top, &a);
            move |ed: &mut Editor| {
                match result {
                    Ok(_) if push => ed.set_success(format!("Pushed {}", st.branch)),
                    Ok(out) if out.contains("Already up to date") => ed.set_status("Already up to date"),
                    Ok(_) => ed.set_success(format!("Pulled into {}", st.branch)),
                    Err(e) => ed.set_error(format!("{}: {e}", if push { "push" } else { "pull" })),
                }
                ed.git_refresh();
                ed.git_reload_bases();
            }
        });
    }

    /// `L` in the git tab — the log (500 commits), each commit's diff as the preview; Enter opens it in a buffer.
    pub fn git_log_picker(&mut self) {
        let cwd = self.git_cwd();
        let picker = crate::picker::Picker::new("log", Vec::new(), false);
        self.open_picker(
            picker,
            Some(Box::new(move || {
                let out = git(&cwd, &["log", "-n", "500", "--format=%h%x1f%s%x1f%an%x1f%ar%x1e"], None)?;
                Ok(out
                    .split('\x1e')
                    .filter_map(|r| {
                        let mut f = r.trim().split('\x1f');
                        let (h, s, a, when) = (f.next()?, f.next()?, f.next()?, f.next()?);
                        Some(crate::picker::Item {
                            label: format!("{h} {s}"),
                            action: crate::picker::Action::Commit { hash: h.to_string(), root: cwd.clone() },
                            hint: format!("{a} · {when}"),
                            glyph: None,
                        })
                    })
                    .collect())
            })),
        );
    }

    /// A commit from the log, in a buffer (`git show`, colored as a diff; closing it never asks) — the one
    /// already open if it is.
    pub fn git_show(&mut self, root: PathBuf, hash: &str) {
        let title = format!("commit {hash}");
        if let Some(i) =
            self.docs.iter().position(|d| d.path.is_none() && d.title.as_deref() == Some(title.as_str()))
        {
            self.current = i;
            return;
        }
        let hash = hash.to_string();
        self.events.jobs().spawn(move || {
            let result = show(&root, &hash);
            move |ed: &mut Editor| match result {
                Ok(text) => {
                    let id = ed.open_virtual(&title, &text);
                    if let Some(spec) = crate::syntax::spec("diff") {
                        ed.load_language(id, spec);
                    }
                }
                Err(e) => ed.set_error(format!("git show: {e}")),
            }
        });
    }

    /// `B` in the git tab — switch branch (local ones, the most recent first).
    pub fn git_branch_picker(&mut self) {
        let cwd = self.git_cwd();
        let picker = crate::picker::Picker::new("branches", Vec::new(), false).without_preview();
        self.open_picker(
            picker,
            Some(Box::new(move || {
                let out = git(
                    &cwd,
                    &[
                        "branch",
                        "--sort=-committerdate",
                        "--format=%(HEAD)%1f%(refname:short)%1f%(committerdate:relative)%1f%(contents:subject)",
                    ],
                    None,
                )?;
                Ok(out
                    .lines()
                    .filter_map(|l| {
                        let mut f = l.split('\x1f');
                        let (head, name, when, subject) = (f.next()?, f.next()?, f.next()?, f.next()?);
                        Some(crate::picker::Item {
                            label: name.to_string(),
                            action: crate::picker::Action::Typed(format!("git-switch {name}")),
                            hint: format!("{subject} · {when}"),
                            glyph: (head == "*").then_some(("●", "diff.plus")),
                        })
                    })
                    .collect())
            })),
        );
    }

    /// `:git-switch <branch>`.
    pub fn git_switch(&mut self, branch: &str) {
        let cwd = self.git_cwd();
        let branch = branch.to_string();
        self.set_status(format!("Switching to {branch}…"));
        self.events.jobs().spawn(move || {
            let result = git(&cwd, &["switch", &branch], None);
            move |ed: &mut Editor| {
                match result {
                    Ok(_) => ed.set_success(format!("On {branch}")),
                    Err(e) => ed.set_error(format!("switch: {e}")),
                }
                ed.git_refresh();
                ed.git_reload_bases();
            }
        });
    }
}

/// `git show` of a commit: its message, the files it touched, the patch.
pub fn show(cwd: &Path, hash: &str) -> Result<String, String> {
    git(
        cwd,
        &[
            "show",
            "--no-color",
            "--stat",
            "--patch",
            "--format=commit %H%nAuthor: %an <%ae>%nDate:   %ad%n%n%w(0,4,4)%B",
            hash,
        ],
        None,
    )
}

/// Most bytes of the staged diff sent for a draft (the stat always goes).
const DRAFT_DIFF_MAX: usize = 80_000;

/// What Claude reads to draft a commit message: the repository's recent messages (their style), the stat and
/// the staged diff — amending, the whole commit's (from HEAD's parent).
fn commit_prompt(top: &Path, amend: bool) -> Result<String, String> {
    use crate::gitmenu::git;
    let base: Vec<&str> = if amend { vec!["--cached", "HEAD^"] } else { vec!["--cached"] };
    let diff_of = |extra: &[&str]| {
        let mut args = vec!["diff", "--no-color", "--no-ext-diff"];
        args.extend(&base);
        args.extend(extra);
        git(top, &args, None)
    };
    // (amending the first commit: no parent — the staged diff alone)
    let (stat, diff) = match (diff_of(&["--stat"]), diff_of(&[])) {
        (Ok(s), Ok(d)) => (s, d),
        _ => (
            git(top, &["diff", "--no-color", "--cached", "--stat"], None)?,
            git(top, &["diff", "--no-color", "--cached"], None)?,
        ),
    };
    if diff.trim().is_empty() {
        return Err("nothing staged".into());
    }
    let skip = if amend { "1" } else { "0" };
    let recent = git(top, &["log", "-8", "--skip", skip, "--format=%B%x00"], None).unwrap_or_default();
    let recent: Vec<&str> = recent.split('\0').map(str::trim).filter(|m| !m.is_empty()).collect();
    let mut cut = DRAFT_DIFF_MAX.min(diff.len());
    while !diff.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut p = String::from(
        "Write the git commit message for the change below. Match the style of this repository's recent \
         commit messages (their subject lines, how they explain, trailers they use). A subject line, a blank \
         line, then a body on what changed and why — the reasons a reader of the history needs, not a file \
         list. Wrap the body at 72 columns. Reply with only the message, no code fences, no commentary.\n\n",
    );
    if !recent.is_empty() {
        p.push_str("<recent-commits>\n");
        for m in &recent {
            p.push_str(m);
            p.push_str("\n---\n");
        }
        p.push_str("</recent-commits>\n\n");
    }
    p.push_str(&format!("<stat>\n{}</stat>\n\n<diff>\n{}", stat, &diff[..cut]));
    if cut < diff.len() {
        p.push_str(&format!("\n… ({} more bytes of diff not shown)\n", diff.len() - cut));
    }
    p.push_str("</diff>\n");
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_v2() {
        let out = "# branch.oid abc\0# branch.head main\0# branch.upstream origin/main\0# branch.ab +2 -1\0\
                   1 M. N... 100644 100644 100644 aaa bbb src/a.rs\0\
                   1 .M N... 100644 100644 100644 aaa bbb src/b.rs\0\
                   1 MM N... 100644 100644 100644 aaa bbb c.rs\0\
                   2 R. N... 100644 100644 100644 aaa bbb R100 new.rs\0old.rs\0\
                   u UU N... 100644 100644 100644 100644 aaa bbb ccc both.rs\0\
                   ? notes.md\0";
        let st = parse_v2(out);
        assert_eq!(
            (st.branch.as_str(), st.upstream.as_deref(), st.ahead, st.behind),
            ("main", Some("origin/main"), 2, 1)
        );
        let names = |l: &[Entry]| l.iter().map(|e| format!("{} {}", e.letter, e.rel)).collect::<Vec<_>>();
        assert_eq!(names(&st.staged), ["M src/a.rs", "M c.rs", "R new.rs"]);
        assert_eq!(names(&st.unstaged), ["M src/b.rs", "M c.rs", "U both.rs", "? notes.md"]);
        assert_eq!(st.branch_line(), "main ↑2 ↓1");
    }

    #[test]
    fn numstat_counts() {
        let m = numstat("3\t1\tsrc/a.rs\0-\t-\tlogo.png\0");
        assert_eq!(m["src/a.rs"], (Some(3), Some(1)));
        assert_eq!(m["logo.png"], (None, None), "binary");
    }

    /// Reading the status never writes the index: `git diff` over a file whose stat changed refreshes it
    /// (taking the lock, whatever `GIT_OPTIONAL_LOCKS` says) — a commit made in a terminal meanwhile would
    /// fail on the lock. (A repository of the test's own, in its temp folder.)
    #[test]
    fn reading_the_status_leaves_the_index() {
        let dir = std::env::temp_dir().join(format!("tarae-gitpanel-index-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let run = |args: &[&str]| {
            let id = ["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"];
            let ok = std::process::Command::new("git").args(id).args(args).current_dir(&dir).output();
            assert!(ok.unwrap().status.success(), "git {args:?}");
        };
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.join("b.txt"), "two\n").unwrap();
        run(&["init", "-q"]);
        run(&["add", "a.txt", "b.txt"]);
        run(&["commit", "-qm", "x"]);
        std::fs::write(dir.join("b.txt"), "two\nthree\n").unwrap();
        // The same text, an hour older: its stat no longer matches the index's (git counts whole seconds)
        let hour_ago = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options().write(true).open(dir.join("a.txt")).unwrap().set_modified(hour_ago).unwrap();
        let before = std::fs::read(dir.join(".git/index")).unwrap();
        let st = status(&dir).unwrap();
        let rows: Vec<_> = st.unstaged.iter().map(|e| (e.rel.as_str(), e.added, e.removed)).collect();
        assert_eq!(rows, [("b.txt", Some(1), Some(0))], "its counts still come");
        assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), before, "the index was rewritten");
        std::fs::remove_dir_all(&dir).ok();
    }
}
