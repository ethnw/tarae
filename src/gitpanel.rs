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
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::document::DocId;
use crate::editor::Editor;
use crate::gitrepo::git;
use crate::hooks;

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
    /// A clean tree's history, for the tab with nothing to commit (#147).
    pub log: Option<Log>,
    /// Submodules whose own commands are held, kept out of every run (`gitrepo::Sub`): (from the top, its
    /// card up — else answered `n`). Their changes don't show; their rows say so.
    pub held: Vec<(String, bool)>,
    /// The repository and its submodules whose commands wait for an answer: their cards go up as it lands
    /// (one gone unanswered comes back) — so a row that says its card is up has one.
    pub asks: Vec<std::sync::Arc<crate::gitrepo::Repo>>,
}

/// What a clean tree's git tab says instead of nothing: the branch's commits past its base — what bring
/// takes (a session's base; else the upstream) — and the last commit.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Log {
    /// The base, its commits counted, the newest few (`a1b2c3d subject`).
    pub base: Option<String>,
    pub ahead: usize,
    pub commits: Vec<String>,
    /// `a1b2c3d subject · 2 hours ago`.
    pub last: Option<String>,
}

/// Commits a clean tab lists past its base at most.
const LOG_COMMITS: usize = 5;

/// `top`'s history against `base` (a branch, or the upstream when None) — on a worker.
fn log(top: &Path, base: Option<String>, upstream: Option<&str>) -> Log {
    let line = |l: &str| {
        let mut p = l.splitn(3, '\x1f');
        let (sha, subject, when) = (p.next().unwrap_or_default(), p.next().unwrap_or_default(), p.next());
        match when {
            Some(w) => format!("{sha} {subject} · {w}"),
            None => format!("{sha} {subject}"),
        }
    };
    let last = git(top, &["log", "-1", "--format=%h%x1f%s%x1f%cr"], None).ok().map(|l| line(l.trim()));
    let base = base.or_else(|| upstream.map(str::to_string));
    let Some(b) = base.clone() else { return Log { last, ..Log::default() } };
    let range = format!("{b}..HEAD");
    let ahead = git(top, &["rev-list", "--count", &range], None).ok().and_then(|n| n.trim().parse().ok());
    let commits = match ahead {
        Some(n) if n > 0 => git(top, &["log", "--format=%h%x1f%s", &format!("-{LOG_COMMITS}"), &range], None)
            .map(|out| out.lines().map(line).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    Log { base: ahead.is_some().then_some(b), ahead: ahead.unwrap_or(0), commits, last }
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

/// The status of `cwd`'s repository with line counts (on a worker thread); clean, its history against
/// `base` (a session's — else the upstream).
pub fn status(cwd: &Path, base: Option<String>) -> Result<Status, String> {
    let top = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"], None)?.trim());
    let top = std::fs::canonicalize(&top).unwrap_or(top);
    let out = git(&top, &["status", "--porcelain=v2", "-z", "--branch", "--untracked-files=all"], None)?;
    let mut st = parse_v2(&out);
    st.top = top.clone();
    if let Ok(r) = crate::gitrepo::repo(&top) {
        let held = r.subs.iter().filter(|s| s.repo.held.is_some());
        st.held = held.map(|s| (s.rel.clone(), !s.repo.asking().is_empty())).collect();
        st.asks = r.asking();
    }
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
    if st.staged.is_empty() && st.unstaged.is_empty() {
        st.log = Some(log(&top, base, st.upstream.as_deref()));
    }
    Ok(st)
}

/// Why a remote refused: in plain words, and git's own word when a ref was rejected (`fetch first`,
/// `non-fast-forward`, `stale info` …).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refused {
    pub why: String,
    pub rejected: Option<String>,
}

impl Refused {
    /// The remote has commits the branch doesn't — what a force (asked) is for.
    pub fn diverged(&self) -> bool {
        matches!(self.rejected.as_deref(), Some("fetch first" | "non-fast-forward"))
    }
}

/// Why git refused, from what it printed. A rejected ref (` ! [rejected] main -> main (fetch first)`) said in
/// plain words; else the first `fatal:`/`error:` line; else the last line that isn't a `hint:` — git's last
/// lines are its hints on what to do next, never what went wrong.
pub fn refusal(stderr: &str) -> Refused {
    let lines: Vec<&str> = stderr.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let rejected = lines.iter().find_map(|l| {
        let rest = l.strip_prefix("! [rejected]").or_else(|| l.strip_prefix("! [remote rejected]"))?;
        let reason = rest.rsplit_once('(')?.1.strip_suffix(')')?;
        Some((l.starts_with("! [remote"), reason.to_string()))
    });
    if let Some((by_remote, reason)) = rejected {
        // What the remote said itself (a hook's message, a forge's protected-branch rule) — its why
        let said: Vec<&str> = lines
            .iter()
            .filter_map(|l| l.strip_prefix("remote:"))
            .map(|l| l.trim().trim_start_matches("error:").trim())
            .filter(|l| !l.is_empty())
            .take(3)
            .collect();
        let why = match reason.as_str() {
            "fetch first" | "non-fast-forward" => {
                "rejected — the remote has commits this branch doesn't".into()
            }
            "stale info" => "rejected — the remote moved since it was read".into(),
            _ if by_remote && !said.is_empty() => format!("refused by the remote — {}", said.join(" ")),
            _ if by_remote => format!("refused by the remote ({reason})"),
            _ => format!("rejected ({reason})"),
        };
        return Refused { why, rejected: Some(reason) };
    }
    let said = |p: &str| lines.iter().find_map(|l| l.strip_prefix(p)).map(str::trim);
    let why = said("fatal:")
        .or_else(|| said("error:"))
        .or_else(|| lines.iter().rev().find(|l| !l.starts_with("hint:")).copied())
        .unwrap_or("git failed");
    Refused { why: why.to_string(), rejected: None }
}

/// Git with a time limit on its patience for remotes: no prompts (push/pull fail fast instead of waiting
/// for a password nobody will type). Its hooks the user's (pre-push, a pull's merge): `gitrepo`'s hooked form.
fn git_remote(dir: &Path, args: &[&str]) -> Result<String, Refused> {
    let refused = |why: String| Refused { why, rejected: None };
    let cmd = crate::gitrepo::repo(dir).map_err(refused)?.hooked_command(args).map_err(refused)?;
    let out = crate::gitrepo::ran(&cmd, None).map_err(refused)?;
    let text = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
    if !out.ok {
        return Err(refusal(&text(&out.stderr)));
    }
    Ok(format!("{}{}", text(&out.stdout), text(&out.stderr)))
}

/// Queued work in the repository (on a worker, given its top — or the folder git runs in, when no status
/// has been read) and what to say when it's done.
pub type GitWork = Box<dyn FnOnce(&Path) -> Result<String, String> + Send>;
pub type GitDone = Box<dyn FnOnce(&mut Editor, Result<String, String>) + Send>;
/// Either, as one: the work, giving back what to do on the main loop.
pub type GitJob = Box<dyn FnOnce(&Path) -> Box<dyn FnOnce(&mut Editor) + Send> + Send>;

/// What a push over the remote's commits would replace — read when its card is shown, the lease taken from it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForcePush {
    pub top: PathBuf,
    pub branch: String,
    pub remote: String,
    /// The branch on the remote (`refs/heads/main`).
    pub remote_ref: String,
    /// Where the remote's branch ended when the card was shown (full sha) — the push replaces only that.
    pub lease: String,
    /// The remote's commits the branch doesn't have (short sha, subject) — what goes.
    pub lost: Vec<(String, String)>,
    /// Which of them are earlier versions of the branch's own (`is_yours`) — the rest are someone else's
    /// work, lost if this goes.
    pub yours: Vec<bool>,
}

impl ForcePush {
    /// `origin/main`.
    pub fn upstream(&self) -> String {
        format!("{}/{}", self.remote, self.remote_ref.strip_prefix("refs/heads/").unwrap_or(&self.remote_ref))
    }

    /// The push: with a lease naming the sha the card showed — never a bare `--force` (nor a bare
    /// `--force-with-lease`, which any fetch in between defeats: it takes the remote-tracking ref as it is
    /// then, ours `f` included).
    pub fn args(&self) -> Vec<String> {
        vec![
            "push".into(),
            format!("--force-with-lease={}:{}", self.remote_ref, self.lease),
            self.remote.clone(),
            format!("refs/heads/{}:{}", self.branch, self.remote_ref),
        ]
    }

    /// Its card: (title, question, the lines decided, the lines warned). The remote's commits in two groups:
    /// those that aren't yours first — lost if you push, heading and rows in the warning color, counted in the
    /// title — then the earlier versions of yours (amended or rebased).
    pub fn card(&self) -> (String, String, Range<usize>, Option<Range<usize>>) {
        let (up, b, n) = (self.upstream(), &self.branch, self.lost.len());
        let group = |yours: bool| -> Vec<(String, String)> {
            let pairs = self.lost.iter().zip(&self.yours);
            pairs.filter(|(_, y)| **y == yours).map(|(c, _)| c.clone()).collect()
        };
        let (gone, kept) = (group(false), group(true));
        let title = match gone.len() {
            0 => format!("Push over {up}?"),
            1 => "Push over a commit that isn't yours?".to_string(),
            k => format!("Push over {k} commits that aren't yours?"),
        };
        let lines = |q: &str| q.split('\n').count();
        let mut q = format!("Pushing {b} replaces {n} commit{} on {up}.", if n == 1 { "" } else { "s" });
        let warned = (!gone.is_empty()).then(|| {
            let at = lines(&q);
            q += &format!("\nNot in yours — lost if you push:{}", commit_rows(&gone));
            at..lines(&q)
        });
        if !kept.is_empty() {
            let head =
                if kept.len() == 1 { "An earlier version of yours:" } else { "Earlier versions of yours:" };
            q += &format!("\n{head}{}", commit_rows(&kept));
        }
        let decided = 1..lines(&q);
        let short = &self.lease[..self.lease.len().min(7)];
        q += &format!(
            "\n\nOnly while {up} still ends at {short}: if anyone pushes meanwhile, nothing is replaced."
        );
        (title, q, decided, warned)
    }
}

/// Most commits listed on a card (the rest counted).
pub(crate) const COMMITS_SHOWN: usize = 10;

/// `sha subject` rows of a `log --format=%h%x1f%s` (cut to `COMMITS_SHOWN`, the rest counted).
pub(crate) fn commit_rows(lost: &[(String, String)]) -> String {
    let mut s: String = lost.iter().take(COMMITS_SHOWN).map(|(h, m)| format!("\n  {h} {m}")).collect();
    if lost.len() > COMMITS_SHOWN {
        s += &format!("\n  … and {} more", lost.len() - COMMITS_SHOWN);
    }
    s
}

pub(crate) fn parse_log(out: &str) -> Vec<(String, String)> {
    out.lines().filter_map(|l| l.split_once('\x1f')).map(|(h, s)| (h.to_string(), s.to_string())).collect()
}

/// The commits of `rev` that would be gone with it: on no other branch, remote-tracking branch or tag.
pub(crate) fn only_on(top: &Path, branch: &str) -> Result<Vec<(String, String)>, String> {
    let full = format!("refs/heads/{branch}");
    let exclude = format!("--exclude={branch}");
    let out = git(
        top,
        &["log", "--format=%h%x1f%s", &full, "--not", &exclude, "--branches", "--remotes", "--tags"],
        None,
    )?;
    Ok(parse_log(&out))
}

/// What the force card asks about: the branch, its upstream, the remote's commits it doesn't have (fetched
/// now — the card shows what's there) and the sha they end at.
fn force_ask(top: &Path) -> Result<ForcePush, String> {
    let branch = git(top, &["symbolic-ref", "--short", "-q", "HEAD"], None)
        .map_err(|_| "not on a branch".to_string())?
        .trim()
        .to_string();
    let up = git(
        top,
        &[
            "for-each-ref",
            "--format=%(upstream:remotename)%00%(upstream:remoteref)",
            &format!("refs/heads/{branch}"),
        ],
        None,
    )?;
    let (remote, remote_ref) = up.trim().split_once('\0').unwrap_or_default();
    if remote.is_empty() || remote_ref.is_empty() {
        return Err(format!("{branch} has no upstream"));
    }
    git_remote(top, &["fetch", remote, remote_ref]).map_err(|r| format!("fetch: {}", r.why))?;
    let lease =
        git(top, &["rev-parse", "--verify", &format!("{branch}@{{upstream}}")], None)?.trim().to_string();
    // (full sha, short, author, subject) of the remote's commits the branch lacks — and of ours it lacks
    let log = |range: String| -> Result<Vec<[String; 4]>, String> {
        let out = git(top, &["log", "--format=%H%x1f%h%x1f%ae%x1f%s", &range], None)?;
        Ok(out
            .lines()
            .filter_map(|l| {
                let mut f = l.splitn(4, '\x1f').map(str::to_string);
                Some([f.next()?, f.next()?, f.next()?, f.next()?])
            })
            .collect())
    };
    let (theirs, ours) = (log(format!("HEAD..{lease}"))?, log(format!("{lease}..HEAD"))?);
    if ours.is_empty() {
        return Err("the remote is ahead and this branch has nothing new — p pulls".into());
    }
    if theirs.is_empty() {
        return Err("nothing of the remote's would be replaced — P pushes as it is".into());
    }
    let same_change = same_changes(top, &lease)?;
    let yours = theirs.iter().map(|c| is_yours(c, &same_change, &ours)).collect();
    Ok(ForcePush {
        top: top.to_path_buf(),
        branch,
        remote: remote.to_string(),
        remote_ref: remote_ref.to_string(),
        lease,
        lost: theirs.into_iter().map(|[_, h, _, s]| (h, s)).collect(),
        yours,
    })
}

/// The remote's commits (up to `lease`) whose change the branch already has — git cherry's patch-id: a
/// rebased commit, or one picked across (full shas).
fn same_changes(top: &Path, lease: &str) -> Result<std::collections::HashSet<String>, String> {
    let out = git(top, &["cherry", "HEAD", lease], None)?;
    Ok(out.lines().filter_map(|l| l.strip_prefix("- ")).map(|s| s.trim().to_string()).collect())
}

/// Whether a remote commit (full sha, short, author, subject) is an earlier version of one of `ours`: its
/// change is in the branch (`same_change`), or one of ours has its author and subject — an amend keeps
/// both while changing the patch. (A colleague's commit with a subject like one of ours stays theirs: its
/// author differs.)
fn is_yours(c: &[String; 4], same_change: &std::collections::HashSet<String>, ours: &[[String; 4]]) -> bool {
    same_change.contains(&c[0]) || ours.iter().any(|o| o[2] == c[2] && o[3] == c[3])
}

/// Whether HEAD is already on its upstream (pushed) — amending it then means a force to push it again.
fn pushed(top: &Path) -> bool {
    git(top, &["merge-base", "--is-ancestor", "HEAD", "@{upstream}"], None).is_ok()
}

pub const PUSHED_WARNING: &str = "this commit is pushed — pushing it again will need a force (asked)";

/// The repository's top for `cwd`, real path.
pub(crate) fn top_of(cwd: &Path) -> Result<PathBuf, String> {
    let top = PathBuf::from(git(cwd, &["rev-parse", "--show-toplevel"], None)?.trim());
    Ok(std::fs::canonicalize(&top).unwrap_or(top))
}

/// `git worktree list --porcelain` → each worktree's path and the branch checked out there (None: detached
/// or bare).
pub fn parse_worktrees(out: &str) -> Vec<(PathBuf, Option<String>)> {
    let mut all = Vec::new();
    for block in out.split("\n\n") {
        let mut path = None;
        let mut branch = None;
        for l in block.lines() {
            if let Some(p) = l.strip_prefix("worktree ") {
                path = Some(PathBuf::from(p));
            } else if let Some(b) = l.strip_prefix("branch refs/heads/") {
                branch = Some(b.to_string());
            }
        }
        if let Some(p) = path {
            all.push((p, branch));
        }
    }
    all
}

/// The worktree other than `top`'s that has `branch` checked out — a branch is in one at a time.
fn held_elsewhere(top: &Path, branch: &str) -> Result<Option<PathBuf>, String> {
    let out = git(top, &["worktree", "list", "--porcelain"], None)?;
    Ok(parse_worktrees(&out)
        .into_iter()
        .filter(|(_, b)| b.as_deref() == Some(branch))
        .map(|(p, _)| std::fs::canonicalize(&p).unwrap_or(p))
        .find(|p| p != top))
}

/// Refused when `branch` is checked out in another worktree, naming it.
fn not_held(top: &Path, branch: &str, doing: &str) -> Result<(), String> {
    match held_elsewhere(top, branch)? {
        Some(p) => Err(format!(
            "can't {doing} {branch} — it's checked out in {} (a branch is in one worktree at a time)",
            worktree_shown(top, &p)
        )),
        None => Ok(()),
    }
}

/// Another worktree as said to a person: from the folder holding the repository (`../wt/fix` → `wt/fix`)
/// when it's in there, else from home (`~/…`).
pub fn worktree_shown(top: &Path, wt: &Path) -> String {
    match top.parent().and_then(|p| wt.strip_prefix(p).ok()) {
        Some(rel) if !rel.as_os_str().is_empty() => rel.display().to_string(),
        _ => crate::allow::tilde(wt),
    }
}

/// `name` as a new branch's name (`git check-ref-format --branch`), else why not.
pub(crate) fn branch_name(top: &Path, name: &str) -> Result<String, String> {
    if name.is_empty() || name.starts_with('-') {
        return Err(format!("{name:?} isn't a branch name"));
    }
    git(top, &["check-ref-format", "--branch", name], None)
        .map(|n| n.trim().to_string())
        .map_err(|_| format!("{name:?} isn't a branch name"))
}

/// The branch checked out in `top` (None: detached).
pub(crate) fn current_branch(top: &Path) -> Option<String> {
    git(top, &["symbolic-ref", "--short", "-q", "HEAD"], None).ok().map(|b| b.trim().to_string())
}

/// What deleting a branch found.
pub enum Deleting {
    /// Deleted — nothing was only on it (the sha it was at).
    Done(String),
    /// Commits only on it: the card asks (the sha it's at, those commits).
    Ask { tip: String, lost: Vec<(String, String)> },
}

/// A commit being written: its message buffer, the repository, whether it amends · whether its session
/// ends once it's in (`c` on a session's end card — `worktree.rs`).
pub struct Committing {
    pub doc: DocId,
    pub top: PathBuf,
    pub amend: bool,
    pub then_end: bool,
}

impl Editor {
    /// Where git runs: the side panel's folder, else the editor's root.
    pub(crate) fn git_cwd(&self) -> PathBuf {
        self.session.sidebar.as_ref().map_or_else(|| self.session.root.clone(), |s| s.root().to_path_buf())
    }

    /// `space g` — the side panel's git tab, with the keys; from the code, on the row for what's under the
    /// cursor (its hunk or conflict — the file opened to them — else its file). Again: closed. During a
    /// review the tab and the review's rail take turns in the column: again = the rail back, with the keys
    /// (so the next one doesn't land in the code — `d` there would delete the selected change).
    pub fn git_panel(&mut self) {
        if self.session.code_review.is_some() && !self.review_rail_up() {
            return crate::codereview::rail_focus(self);
        }
        let review = self.session.code_review.is_some();
        if let Some(r) = self.session.code_review.as_mut() {
            r.behind = true;
            r.focus = false;
            // (a tab focused behind the rail would close instead of coming to the front)
            if let Some(s) = self.session.sidebar.as_mut() {
                s.focus = false;
            }
        } else if self
            .session
            .sidebar
            .as_ref()
            .is_some_and(|s| s.mode == crate::sidebar::Mode::Git && s.focus)
        {
            self.session.sidebar = None;
            return;
        }
        let aim = self.doc().path.clone().map(|p| (p, self.cursor_line()));
        self.sidebar_show(crate::sidebar::Mode::Git);
        if let Some(s) = self.session.sidebar.as_mut() {
            s.aim = aim;
            // (no quick `s`/`d` on the change under the cursor in a review: it's against the review's base)
            s.from_code = !review;
        }
        self.git_refresh();
    }

    /// `f` in the git tab — fetch every remote (pruned); it never waits on a password prompt.
    pub fn git_fetch(&mut self) {
        self.git_hooked("fetch", hooks::FETCH, |ed, pre| {
            ed.set_step("git", "fetching…");
            ed.git_do(
                Box::new(move |top| {
                    git_remote(top, &hooks::with(pre, &["fetch", "--all", "--prune"])).map_err(|r| r.why)
                }),
                Box::new(|ed, r| {
                    ed.step_done("git");
                    match r {
                        Ok(_) => ed.set_success("Fetched"),
                        Err(e) => ed.set_error(format!("fetch: {e}")),
                    }
                }),
            );
        });
    }

    /// `z` / `Z` in the git tab — put the changes away (new files too) / take the last ones back.
    pub fn git_stash(&mut self, pop: bool) {
        let args: &'static [&'static str] =
            if pop { &["stash", "pop"] } else { &["stash", "push", "--include-untracked"] };
        self.git_hooked("stash", hooks::STASH, move |ed, pre| {
            ed.git_do(
                Box::new(move |top| crate::gitrepo::hooked(top, &hooks::with(pre, args))),
                Box::new(move |ed, r| match r {
                    Ok(_) if pop => ed.set_success("Took the stashed changes back"),
                    Ok(out) if out.contains("No local changes") => ed.set_status("nothing to stash"),
                    Ok(_) => ed.set_success("Stashed — Z takes them back"),
                    Err(e) => ed.set_error(format!("stash: {e}")),
                }),
            );
        });
    }

    /// `?` in the git tab — every key it takes, as a card.
    pub fn git_help(&mut self) {
        let md = "**git tab**\n\n\
            `enter` a changed file: its hunks and conflicts · a hunk: there in the code\n\
            `s` `u` stage · unstage (a file, folder, section — or the hunk)\n\
            `d` discard (twice for a file; a hunk: its index lines back)\n\
            `o` `t` `a` a conflict: keep ours · theirs · both\n\
            `e` open in the editor · `D` diff against HEAD\n\
            `c` commit · `C` commit, Claude drafting the message · `A` amend (`:git-amend`) — each opens the message: `:w` does it\n\
            `p` pull · `P` push · `f` fetch\n\
            `B` branches · `L` log · `z` stash · `Z` take it back\n\
            `b` blame on/off · `r` read again · `esc` the code";
        self.popup = Some(crate::markdown::render(md, &self.theme, None));
    }

    /// Something changed files outside the editor's own saves — Claude's edit, write or command, a turn's
    /// end (#29): the git tab, when it's shown, reads the status again.
    pub fn git_tab_follow(&mut self) {
        if self.session.sidebar.as_ref().is_some_and(|s| s.mode == crate::sidebar::Mode::Git) {
            self.git_refresh();
        }
    }

    /// Read the status again (worker) — the panel follows. Only the latest read lands (a slow one started
    /// before a stage doesn't undo what the panel shows).
    pub fn git_refresh(&mut self) {
        let cwd = self.git_cwd();
        self.session.git_reads += 1;
        let read = self.session.git_reads;
        // (a session's base, as bring takes it — in its own checkout only)
        let base = (cwd == self.session.root && self.session.id != 0)
            .then(|| self.session.base.clone().or_else(|| self.start_branch()))
            .flatten();
        self.events.jobs().spawn(move || {
            let result = status(&cwd, base).map(Status::with_rows);
            move |ed: &mut Editor| {
                if read != ed.session.git_reads {
                    return;
                }
                match result {
                    Ok(mut st) => {
                        st.generation = read;
                        for r in &st.asks {
                            ed.git_trust_check(r);
                        }
                        ed.session.git_status = Some(st);
                    }
                    Err(e) => {
                        ed.session.git_status = None;
                        if ed.session.sidebar.as_ref().is_some_and(|s| s.mode == crate::sidebar::Mode::Git) {
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
        if self.session.git_status.is_none() {
            return self.set_warning("not a git repository");
        }
        self.git_queued(work, done);
    }

    /// Queue work in the repository, the status read or not: `done` gets what `work` gave.
    fn git_queued<T: Send + 'static>(
        &mut self,
        work: impl FnOnce(&Path) -> T + Send + 'static,
        done: impl FnOnce(&mut Editor, T) + Send + 'static,
    ) {
        self.session.git_queue.push_back(Box::new(move |top| {
            let result = work(top);
            Box::new(move |ed: &mut Editor| done(ed, result))
        }));
        self.git_next();
    }

    fn git_next(&mut self) {
        if self.session.git_busy {
            return;
        }
        let top = self.session.git_status.as_ref().map_or_else(|| self.git_cwd(), |s| s.top.clone());
        let Some(job) = self.session.git_queue.pop_front() else { return };
        self.session.git_busy = true;
        self.events.jobs().spawn(move || {
            let done = job(&top);
            move |ed: &mut Editor| {
                ed.session.git_busy = false;
                done(ed);
                ed.git_next();
                if ed.session.git_queue.is_empty() {
                    ed.git_refresh();
                    ed.git_reload_bases();
                }
            }
        });
    }

    /// `s` / `u` on a row: stage / unstage it (a file, a folder, or `""` = the whole section). Staging waits
    /// for the saves asked before it — what's staged is what was saved, never a save's temp file.
    pub fn git_stage(&mut self, rel: &str, stage: bool) {
        let path = if rel.is_empty() { ".".to_string() } else { rel.to_string() };
        let args: Vec<String> = if stage {
            vec!["add".into(), "--all".into(), "--".into(), path, crate::disk::NOT_TEMP.into()]
        } else {
            vec!["restore".into(), "--staged".into(), "--".into(), path]
        };
        let what = if rel.is_empty() { "everything".to_string() } else { rel.to_string() };
        match stage {
            true => self.after_saves(move |ed| ed.git_stage_run(args, what, stage)),
            false => self.git_stage_run(args, what, stage),
        }
    }

    fn git_stage_run(&mut self, args: Vec<String>, what: String, stage: bool) {
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
        let Some(st) = self.session.git_status.as_ref() else { return };
        let under = |e: &&Entry| rel.is_empty() || e.rel == rel || e.rel.starts_with(&format!("{rel}/"));
        let tracked: Vec<String> =
            st.unstaged.iter().filter(under).filter(|e| e.letter != '?').map(|e| e.rel.clone()).collect();
        let untracked: Vec<String> =
            st.unstaged.iter().filter(under).filter(|e| e.letter == '?').map(|e| e.rel.clone()).collect();
        if tracked.is_empty() && untracked.is_empty() {
            return;
        }
        let fresh = self
            .session
            .git_discard_asked
            .as_ref()
            .is_some_and(|(r, t)| r == rel && t.elapsed().as_secs() < 3);
        if !fresh {
            self.session.git_discard_asked = Some((rel.to_string(), Instant::now()));
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
        self.session.git_discard_asked = None;
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
        let Some(st) = self.session.git_status.as_ref() else {
            return self.set_warning("not a git repository");
        };
        // A code review open keeps every buffer in its REVIEW keys: the message couldn't be typed, and a
        // draft landed as an edit waiting there — ended first, said
        if self.session.code_review.is_some() {
            let key = if draft { "C" } else { "c" };
            return self.set_warning(format!(
                "a code review is open — end it first (space v q), then {key} writes the commit message"
            ));
        }
        let (top, branch) = (st.top.clone(), st.branch.clone());
        if amend {
            // (read now, before the amend makes a new HEAD — said as the buffer opens)
            let top = top.clone();
            self.events.jobs().spawn(move || {
                let pushed = pushed(&top);
                move |ed: &mut Editor| {
                    if pushed {
                        ed.set_warning(PUSHED_WARNING);
                    }
                }
            });
        }
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
                text += &format!(
                    "\n# :w {} · :q cancels · :commit-draft asks Claude · # lines are left out\n",
                    if amend { "amends the last commit" } else { "commits" }
                );
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
                    ed.session.commit_then_end = false;
                    // (what's to stage, as the tab last read it — the commit's own read found nothing staged)
                    let changed = ed.session.git_status.as_ref().is_some_and(|s| !s.unstaged.is_empty());
                    let then = if draft { "C" } else { "c" };
                    ed.set_warning(match changed {
                        true => format!(
                            "nothing staged to commit — s on a file stages it, s on Changes stages them all, \
                             then {then}"
                        ),
                        false => "nothing staged to commit — no changes either".to_string(),
                    })
                }
                Ok(file) => {
                    let then_end = std::mem::take(&mut ed.session.commit_then_end);
                    if let Err(e) = ed.open(std::path::Path::new(&file)) {
                        return ed.set_error(format!("{e:#}"));
                    }
                    let doc = ed.doc_mut();
                    doc.throwaway = true;
                    doc.set_selection(crate::selection::Selection::point(0));
                    let id = doc.id;
                    ed.session.committing = Some(Committing { doc: id, top, amend, then_end });
                    if let Some(s) = ed.session.sidebar.as_mut() {
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
                Err(e) => {
                    ed.session.commit_then_end = false;
                    ed.set_error(format!("commit: {e}"))
                }
            }),
        );
    }

    /// A buffer was saved: the commit message's commits (and its buffer closes); the panel reads the status
    /// again.
    pub fn git_saved(&mut self, id: DocId) {
        if let Some(c) = self.session.committing.take_if(|c| c.doc == id) {
            let Some(file) = self.docs.iter().find(|d| d.id == id).and_then(|d| d.path.clone()) else {
                return;
            };
            let (top, amend, then_end) = (c.top, c.amend, c.then_end);
            let (what, names) = if amend { ("amend", hooks::AMEND) } else { ("commit", hooks::COMMIT) };
            self.git_hooked(what, names, move |ed, pre| {
                // In the git queue — after a stage still running, never beside another git that writes
                ed.git_do(
                    Box::new(move |top| {
                        let f = file.to_string_lossy().into_owned();
                        let mut args = vec!["commit", "--cleanup=strip", "-F", f.as_str()];
                        if amend {
                            args.push("--amend");
                        }
                        crate::gitrepo::hooked(top, &hooks::with(pre, &args))
                            .and_then(|_| git(top, &["log", "-1", "--format=%h %s"], None))
                    }),
                    Box::new(move |ed, result| match result {
                        // Its buffer goes (wherever you are by now — a hook can take a while)
                        Ok(line) => {
                            let did = if amend { "Amended" } else { "Committed" };
                            ed.set_success(format!("{did} {}", line.trim()));
                            ed.close_doc(id);
                            // (its session asked to end once its changes were in: asked again, now with none)
                            if then_end && let Err(e) = ed.session_end("") {
                                ed.set_error(e);
                            }
                        }
                        // An empty message aborts — the buffer stays for another try
                        Err(e) => {
                            ed.session.committing = Some(Committing { doc: id, top, amend, then_end });
                            ed.set_error(format!("commit: {e}"));
                        }
                    }),
                );
            });
            return;
        }
        if self.session.sidebar.as_ref().is_some_and(|s| s.mode == crate::sidebar::Mode::Git) {
            self.git_refresh();
        }
    }

    /// `C` · `:commit-draft` — Claude drafts the commit message from what's staged (amending: the whole
    /// commit), in the style of the repository's recent messages. Read on a worker; the draft streams in and
    /// lands as the in-buffer review of the message part (above the `#` lines).
    pub fn commit_draft(&mut self) {
        let Some(c) = self.session.committing.as_ref().filter(|c| c.doc == self.doc().id) else {
            return self
                .set_status("open a commit message first — c in the git tab (or C: Claude drafts it)");
        };
        let (top, amend, id) = (c.top.clone(), c.amend, c.doc);
        if self.session.code_review.is_some() {
            return self.set_warning("a code review is open — end it first (space v q), then :commit-draft");
        }
        let said = "Claude is drafting the commit message…";
        // (once it's read, the draft streams into its review)
        let work = move || commit_prompt(&top, amend);
        self.step(self.events.jobs(), "commit-draft", said, work, move |ed, prompt| {
            let prompt = match prompt {
                Ok(p) => p,
                Err(e) => return ed.set_error(format!("commit draft: {e}")),
            };
            let Some(doc) = ed.docs.iter().find(|d| d.id == id) else { return };
            // The message part: all before the first `#` line
            let text = doc.text.to_string();
            let end = text.match_indices('\n').map(|(i, _)| i + 1).find(|&i| text[i..].starts_with('#'));
            let end = end.unwrap_or(text.len());
            // Nothing written yet: the draft goes in at the top (a plain insertion to read, the blank lines
            // under it the gap before the `#` lines); amending: it's a new version of the message
            let end = if text[..end].trim().is_empty() { 0 } else { end };
            match crate::llm::draft(ed, id, "a commit message", &prompt, 0, end) {
                // (its body at 72, like every message — Claude doesn't always: #21)
                Ok(()) => {
                    if let Some(r) = ed.session.review.as_mut() {
                        r.wrap = Some(72);
                    }
                }
                Err(e) => ed.set_error(e),
            }
        });
    }

    /// Whether the current buffer is a commit message being written.
    pub fn in_commit_message(&self) -> bool {
        self.session.committing.as_ref().is_some_and(|c| c.doc == self.doc().id)
    }

    /// `P` / `p` — push (setting the upstream the first time) / pull fast-forward only. A branch gone a
    /// different way from its upstream (after an amend or a rebase) — or a push refused for it — gets the
    /// force card instead (`git_force_ask`).
    pub fn git_push_pull(&mut self, push: bool) {
        let Some(st) = self.session.git_status.clone() else {
            return self.set_warning("not a git repository");
        };
        if push && st.upstream.is_some() && st.ahead > 0 && st.behind > 0 {
            return self.git_force_ask(None);
        }
        let args: Vec<String> = match (push, &st.upstream) {
            (true, Some(_)) => vec!["push".into()],
            (true, None) => vec!["push".into(), "-u".into(), "origin".into(), "HEAD".into()],
            (false, _) => vec!["pull".into(), "--ff-only".into()],
        };
        let (what, names) = if push { ("push", hooks::PUSH) } else { ("pull", hooks::PULL) };
        self.git_hooked(what, names, move |ed, pre| {
            let top = st.top.clone();
            let work = move || {
                let a: Vec<&str> = args.iter().map(String::as_str).collect();
                git_remote(&top, &hooks::with(pre, &a))
            };
            let said = if push { "Pushing…" } else { "Pulling…" };
            ed.step(ed.events.jobs(), "git", said, work, move |ed, result| {
                match result {
                    Ok(_) if push => ed.set_success(format!("Pushed {}", st.branch)),
                    Ok(out) if out.contains("Already up to date") => ed.set_status("Already up to date"),
                    Ok(_) => ed.set_success(format!("Pulled into {}", st.branch)),
                    Err(r) if push && r.diverged() && st.upstream.is_some() => ed.git_force_ask(Some(r.why)),
                    Err(r) => ed.set_error(format!("{}: {}", if push { "push" } else { "pull" }, r.why)),
                }
                ed.git_refresh();
                ed.git_reload_bases();
            });
        });
    }

    /// The force card: what a push over the remote's commits would replace — fetched now, so it lists what's
    /// there — and `y` pushes with a lease on exactly that (`git_force_push`). `refused` = why the push
    /// before it was refused (said if the card can't be made).
    pub fn git_force_ask(&mut self, refused: Option<String>) {
        let Some(top) = self.session.git_status.as_ref().map(|s| s.top.clone()) else { return };
        let said = "reading what the remote has…";
        self.step(
            self.events.jobs(),
            "git",
            said,
            move || force_ask(&top),
            move |ed, ask| {
                let f = match ask {
                    Ok(f) => f,
                    Err(e) => {
                        return ed.set_error(match refused {
                            Some(why) => format!("push: {why} — {e}"),
                            None => format!("push: {e}"),
                        });
                    }
                };
                let (title, question, decided, warned) = f.card();
                let mut o = crate::offer::Offer::new(
                    title,
                    &question,
                    "Pushing…",
                    "Push".into(),
                    crate::offer::What::ForcePush(Box::new(f)),
                )
                .deciding(decided);
                if let Some(w) = warned {
                    o = o.warning(w);
                }
                ed.push_offer(o);
            },
        );
    }

    /// `y` on the force card: the push, with a lease on the sha the card showed. Refused if the remote
    /// moved since — said, nothing replaced.
    pub fn git_force_push(&mut self, id: u64, f: ForcePush) {
        self.offer_finished(id, Ok(()));
        self.git_hooked("push", hooks::PUSH, move |ed, pre| {
            let (args, top) = (f.args(), f.top.clone());
            let work = move || {
                let a: Vec<&str> = args.iter().map(String::as_str).collect();
                git_remote(&top, &hooks::with(pre, &a))
            };
            ed.step(ed.events.jobs(), "git", "Pushing…", work, move |ed, result| {
                let (up, n) = (f.upstream(), f.lost.len());
                match result {
                    Ok(_) => ed.set_success(format!(
                        "Pushed {} over {up} — {n} commit{} of it replaced",
                        f.branch,
                        if n == 1 { "" } else { "s" }
                    )),
                    Err(r) if r.rejected.as_deref() == Some("stale info") => ed.set_error(format!(
                        "push: {up} moved since the card was shown — nothing replaced; P looks again"
                    )),
                    Err(r) => ed.set_error(format!("push: {}", r.why)),
                }
                ed.git_refresh();
                ed.git_reload_bases();
            });
        });
    }

    /// `:git-amend` — `A` from anywhere: the last commit's message in a buffer, what's staged going in with it
    /// (`:w` amends, `:q` cancels; warned when that commit is pushed already) — as `c`/`C` open a new one.
    /// The repository's status is read first when the git tab hasn't.
    pub fn git_amend(&mut self) {
        if self.session.git_status.is_some() {
            return self.git_commit_start(true);
        }
        let cwd = self.git_cwd();
        self.events.jobs().spawn(move || {
            let read = status(&cwd, None).map(Status::with_rows);
            move |ed: &mut Editor| match read {
                Ok(st) => {
                    ed.session.git_status = Some(st);
                    ed.git_commit_start(true);
                }
                Err(e) => ed.set_warning(format!("amend: {e}")),
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

    /// `B` in the git tab — the local branches, the most recent first: Enter switches, `C-r` renames,
    /// `C-x` deletes; a name no branch has is a `+` row that makes it (`Picker::making`). One checked out in
    /// another worktree says where.
    pub fn git_branch_picker(&mut self) {
        let cwd = self.git_cwd();
        let picker = crate::picker::Picker::new("branches", Vec::new(), false)
            .without_preview()
            .making("branch", "git-branch-new");
        self.open_picker(
            picker,
            Some(Box::new(move || {
                let top = top_of(&cwd)?;
                let out = git(
                    &cwd,
                    &[
                        "branch",
                        "--sort=-committerdate",
                        "--format=%(HEAD)%1f%(refname:short)%1f%(committerdate:relative)%1f%(worktreepath)%1f%(contents:subject)",
                    ],
                    None,
                )?;
                Ok(out
                    .lines()
                    .filter_map(|l| {
                        let mut f = l.split('\x1f');
                        let (head, name, when, wt, subject) = (f.next()?, f.next()?, f.next()?, f.next()?, f.next()?);
                        let wt = Path::new(wt);
                        let elsewhere = (head != "*" && !wt.as_os_str().is_empty())
                            .then(|| std::fs::canonicalize(wt).unwrap_or(wt.to_path_buf()))
                            .filter(|w| *w != top);
                        let held = elsewhere.map(|w| worktree_shown(&top, &w));
                        let hint = match &held {
                            Some(at) => format!("in {at} · {subject} · {when}"),
                            None => format!("{subject} · {when}"),
                        };
                        Some(crate::picker::Item {
                            label: name.to_string(),
                            action: crate::picker::Action::Branch { name: name.to_string(), held },
                            hint,
                            glyph: (head == "*").then_some(("●", "diff.plus")),
                        })
                    })
                    .collect())
            })),
        );
    }

    /// Branch work on a worker in the folder git runs in, `step` said meanwhile; then `done`, and the status
    /// and the gutter's bases read again. The hooks it may run (`names`) asked about first (`hooks.rs`) —
    /// `work` gets the args to put before its git's own.
    fn git_branch_job<T: Send + 'static>(
        &mut self,
        step: String,
        names: &'static [&'static str],
        work: impl FnOnce(&Path, hooks::Pre) -> Result<T, String> + Send + 'static,
        done: impl FnOnce(&mut Editor, Result<T, String>) + Send + 'static,
    ) {
        let what = if names == hooks::SWITCH { "switch" } else { "branch change" };
        self.git_hooked(what, names, move |ed, pre| {
            let cwd = ed.git_cwd();
            let job = move || top_of(&cwd).and_then(|top| work(&top, pre));
            ed.step(ed.events.jobs(), "git", step, job, move |ed, result| {
                done(ed, result);
                ed.git_refresh();
                ed.git_reload_bases();
            });
        });
    }

    /// `:git-switch <branch>` — refused for one checked out in another worktree, naming it.
    pub fn git_switch(&mut self, branch: &str) {
        let branch = branch.to_string();
        let b = branch.clone();
        self.git_branch_job(
            format!("Switching to {branch}…"),
            hooks::SWITCH,
            move |top, pre| {
                if b.starts_with('-') {
                    return Err(format!("{b:?} isn't a branch"));
                }
                not_held(top, &b, "switch to")?;
                crate::gitrepo::hooked(top, &hooks::with(pre, &["switch", &b]))
            },
            move |ed, r| match r {
                Ok(_) => ed.set_success(format!("On {branch}")),
                Err(e) => ed.set_error(format!("switch: {e}")),
            },
        );
    }

    /// `:git-branch-new <name> [<from>]` · the branch picker's `+` row — a branch made from `from` (else
    /// where you are) and switched to; uncommitted changes come along.
    pub fn git_branch_new(&mut self, name: &str, from: Option<&str>) {
        let (name, from) = (name.to_string(), from.map(str::to_string));
        let n = name.clone();
        self.git_branch_job(
            format!("Making {name}…"),
            hooks::SWITCH,
            move |top, pre| {
                let name = branch_name(top, &n)?;
                let mut args = vec!["switch", "--no-guess", "-c", name.as_str()];
                if let Some(f) = from.as_deref() {
                    if f.starts_with('-') {
                        return Err(format!("{f:?} isn't a commit"));
                    }
                    args.push(f);
                }
                crate::gitrepo::hooked(top, &hooks::with(pre, &args)).map(|_| name)
            },
            move |ed, r| match r {
                Ok(name) => ed.set_success(format!("On {name}, a new branch")),
                Err(e) => ed.set_error(format!("new branch {name}: {e}")),
            },
        );
    }

    /// `:git-branch-rename [<old>] <new>` · `C-r` in the branch picker — the one you're on when `old` isn't
    /// given. Refused for one checked out in another worktree; its upstream keeps the old name (said).
    pub fn git_branch_rename(&mut self, old: Option<&str>, new: &str) {
        let (old, new) = (old.map(str::to_string), new.to_string());
        self.git_branch_job(
            format!("Renaming to {new}…"),
            hooks::REFS,
            move |top, pre| {
                let old = match old {
                    Some(o) => o,
                    None => current_branch(top).ok_or("not on a branch — name the one to rename")?,
                };
                let new = branch_name(top, &new)?;
                not_held(top, &old, "rename")?;
                crate::gitrepo::hooked(top, &hooks::with(pre, &["branch", "-m", "--", &old, &new]))?;
                let up = git(
                    top,
                    &["for-each-ref", "--format=%(upstream:short)", &format!("refs/heads/{new}")],
                    None,
                )
                .unwrap_or_default();
                Ok((old, new, up.trim().to_string()))
            },
            |ed, r| match r {
                Ok((old, new, up)) if up.is_empty() => ed.set_success(format!("Renamed {old} → {new}")),
                Ok((old, new, up)) => {
                    ed.set_success(format!("Renamed {old} → {new} (its upstream is still {up})"))
                }
                Err(e) => ed.set_error(format!("rename: {e}")),
            },
        );
    }

    /// `:git-branch-delete <name>` · `C-x` in the branch picker. Nothing only on it (its commits are on
    /// another branch, a remote's, or a tag): deleted at once. Else a card names the commits it would lose,
    /// and `y` deletes it if it hasn't moved since. Never the one you're on, nor one checked out in another
    /// worktree. `reopen`: the branch picker comes back after (it was open).
    pub fn git_branch_delete(&mut self, name: &str, reopen: bool) {
        self.git_branch_delete_asking(name, reopen, false);
    }

    /// `git_branch_delete` — `always`: the card even with nothing only on it (its delete asked again after
    /// `:restart` was never answered: asking again never acts).
    pub fn git_branch_delete_asking(&mut self, name: &str, reopen: bool, always: bool) {
        let name = name.to_string();
        let n = name.clone();
        self.git_branch_job(
            format!("Deleting {name}…"),
            hooks::REFS,
            move |top, pre| {
                if current_branch(top).as_deref() == Some(n.as_str()) {
                    return Err(format!("you're on {n} — switch to another branch first"));
                }
                not_held(top, &n, "delete")?;
                let tip = git(top, &["rev-parse", "--verify", "-q", &format!("refs/heads/{n}")], None)
                    .map_err(|_| format!("there's no branch {n}"))?
                    .trim()
                    .to_string();
                let lost = only_on(top, &n)?;
                if !lost.is_empty() || always {
                    return Ok((top.to_path_buf(), Deleting::Ask { tip, lost }));
                }
                crate::gitrepo::hooked(top, &hooks::with(pre, &["branch", "-D", "--", &n]))?;
                Ok((top.to_path_buf(), Deleting::Done(tip)))
            },
            move |ed, r| match r {
                Ok((_, Deleting::Done(tip))) => {
                    ed.set_success(format!(
                        "Deleted {name} (was {}) — its commits are kept elsewhere",
                        &tip[..7]
                    ));
                    if reopen && ed.picker.is_none() {
                        ed.git_branch_picker();
                    }
                }
                Ok((top, Deleting::Ask { tip, lost })) => {
                    let n = lost.len();
                    let question = match n {
                        // (asked again after `:restart` — nothing only on it by now, asked all the same)
                        0 => "Its commits are on another branch or tag too — nothing is lost with it.".into(),
                        _ => format!(
                            "{} on no other branch or tag:{}\nGone with it — git's reflog keeps {} a while.\nA \
                             squash merge on a forge looks unmerged here.",
                            if n == 1 { "This commit is".to_string() } else { format!("These {n} commits are") },
                            commit_rows(&lost),
                            if n == 1 { "it" } else { "them" },
                        ),
                    };
                    let rows = n.min(COMMITS_SHOWN + 1);
                    let o = crate::offer::Offer::new(
                        format!("Delete {name}?"),
                        &question,
                        "Deleting…",
                        "Delete".into(),
                        crate::offer::What::BranchDelete { top, branch: name, tip },
                    )
                    .deciding(if n == 0 { 0..1 } else { 1..1 + rows });
                    ed.push_offer(o);
                }
                Err(e) => ed.set_error(format!("delete: {e}")),
            },
        );
    }

    /// `y` on the delete card: deleted, if it's still where the card saw it.
    pub fn git_branch_delete_answer(&mut self, id: u64, top: PathBuf, branch: String, tip: String) {
        self.offer_finished(id, Ok(()));
        self.git_hooked("branch change", hooks::REFS, move |ed, pre| {
            let said = format!("Deleting {branch}…");
            let (b, t) = (branch.clone(), tip.clone());
            let work = move || {
                not_held(&top, &b, "delete")?;
                let now = git(&top, &["rev-parse", "--verify", "-q", &format!("refs/heads/{b}")], None)
                    .map_err(|_| format!("there's no branch {b} any more"))?;
                if now.trim() != t {
                    return Err(format!("{b} moved since the card was shown — nothing deleted"));
                }
                crate::gitrepo::hooked(&top, &hooks::with(pre, &["branch", "-D", "--", &b]))
            };
            ed.step(ed.events.jobs(), "git", said, work, move |ed, result| {
                match result {
                    Ok(_) => ed.set_success(format!(
                        "Deleted {branch} — `git branch {branch} {}` brings it back",
                        &tip[..7]
                    )),
                    Err(e) => ed.set_error(format!("delete: {e}")),
                }
                ed.git_refresh();
                ed.git_reload_bases();
            });
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
    use crate::gitrepo::git;
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

    /// What a refused push or pull says: why (the rejected ref's reason, a `fatal:` line) — never the `hint:`
    /// lines git ends with.
    #[test]
    fn refusals_say_why_not_the_hint() {
        let push = |reason: &str| {
            format!(
                "To /tmp/r/origin.git\n ! [rejected]        main -> main ({reason})\nerror: failed to push some \
                 refs to '/tmp/r/origin.git'\nhint: Updates were rejected because the tip of your current branch \
                 is behind\nhint: See the 'Note about fast-forwards' in 'git push --help' for details.\n"
            )
        };
        let r = refusal(&push("fetch first"));
        assert_eq!(r.why, "rejected — the remote has commits this branch doesn't");
        assert!(r.diverged());
        assert!(refusal(&push("non-fast-forward")).diverged());
        let stale = refusal(&push("stale info"));
        assert_eq!(
            (stale.why.as_str(), stale.diverged()),
            ("rejected — the remote moved since it was read", false)
        );
        let hook =
            refusal(" ! [remote rejected] main -> main (pre-receive hook declined)\nerror: failed to push\n");
        assert_eq!(hook.why, "refused by the remote (pre-receive hook declined)");
        // What the remote said is its why (a forge's protected branch)
        let forge = refusal(
            "remote: error: GH006: Protected branch update failed for refs/heads/main.\nremote: error: Changes \
             must be made through a pull request.\nTo github.com:o/r.git\n ! [remote rejected] main -> main \
             (protected branch hook declined)\nerror: failed to push some refs to 'github.com:o/r.git'\n",
        );
        assert_eq!(
            forge.why,
            "refused by the remote — GH006: Protected branch update failed for refs/heads/main. Changes must be \
             made through a pull request."
        );
        let gone = refusal(
            "fatal: '/tmp/missing.git' does not appear to be a git repository\nfatal: Could not read from remote \
             repository.\n\nPlease make sure you have the correct access rights\nand the repository exists.\n",
        );
        assert_eq!(gone.why, "'/tmp/missing.git' does not appear to be a git repository");
        assert_eq!(gone.rejected, None);
        let pull = refusal(
            "hint: Diverging branches can't be fast-forwarded, you need to either:\nhint:\nhint: \tgit merge \
             --no-ff\nfatal: Not possible to fast-forward, aborting.\n",
        );
        assert_eq!(pull.why, "Not possible to fast-forward, aborting.");
        assert_eq!(refusal("something odd\nhint: try this\n").why, "something odd");
    }

    /// The force push's argv: a lease naming the sha the card showed, the branch pushed by name — never a
    /// bare `--force`, `-f`, a `+` refspec, nor a lease without its sha.
    #[test]
    fn the_force_push_always_names_its_lease() {
        let f = ForcePush {
            top: PathBuf::from("/r"),
            branch: "fix".into(),
            remote: "origin".into(),
            remote_ref: "refs/heads/fix".into(),
            lease: "0123456789abcdef0123456789abcdef01234567".into(),
            lost: vec![("0123456".into(), "old".into())],
            yours: vec![true],
        };
        let args = f.args();
        assert_eq!(
            args,
            [
                "push",
                "--force-with-lease=refs/heads/fix:0123456789abcdef0123456789abcdef01234567",
                "origin",
                "refs/heads/fix:refs/heads/fix"
            ]
        );
        for a in &args {
            assert!(!matches!(a.as_str(), "--force" | "-f" | "--force-with-lease"), "{a}");
            assert!(!a.starts_with('+'), "{a}");
        }
        assert_eq!(f.upstream(), "origin/fix");
    }

    /// The force card: commits that aren't yours first — warned, counted in the title — then the earlier
    /// versions of yours; with none of anyone else's, no warning and the plain title.
    #[test]
    fn the_force_card_names_what_isnt_yours() {
        let card = |yours: Vec<bool>| {
            let lost =
                [("aaaaaaa", "Fix the crash"), ("bbbbbbb", "Bump the version"), ("ccccccc", "Ask once")];
            ForcePush {
                top: PathBuf::from("/r"),
                branch: "main".into(),
                remote: "origin".into(),
                remote_ref: "refs/heads/main".into(),
                lease: "aaaaaaa0123456789abcdef0123456789abcdef0".into(),
                lost: lost.iter().take(yours.len()).map(|(h, s)| (h.to_string(), s.to_string())).collect(),
                yours,
            }
            .card()
        };
        let (title, q, decided, warned) = card(vec![false, false, true]);
        assert_eq!(title, "Push over 2 commits that aren't yours?");
        let lines: Vec<&str> = q.split('\n').collect();
        assert_eq!(
            lines[..6],
            [
                "Pushing main replaces 3 commits on origin/main.",
                "Not in yours — lost if you push:",
                "  aaaaaaa Fix the crash",
                "  bbbbbbb Bump the version",
                "An earlier version of yours:",
                "  ccccccc Ask once",
            ]
        );
        assert_eq!((decided, warned), (1..6, Some(1..4)));
        assert!(q.ends_with(
            "Only while origin/main still ends at aaaaaaa: if anyone pushes meanwhile, nothing is replaced."
        ));
        let (title, q, _, warned) = card(vec![true, true]);
        assert_eq!((title.as_str(), warned), ("Push over origin/main?", None));
        assert!(q.contains("\nEarlier versions of yours:\n  aaaaaaa"), "{q}");
        let (title, q, _, warned) = card(vec![false]);
        assert_eq!((title.as_str(), warned), ("Push over a commit that isn't yours?", Some(1..3)));
        assert!(!q.contains("of yours:"), "{q}");
    }

    /// An earlier version of yours: its change is in yours (git cherry), or one of yours has its author and
    /// subject (an amend) — a colleague's commit with a subject like one of yours stays theirs.
    #[test]
    fn yours_by_change_or_by_author_and_subject() {
        let c = |sha: &str, who: &str, s: &str| {
            [sha.to_string(), sha[..3].to_string(), who.to_string(), s.to_string()]
        };
        let ours = [c("111111", "me@t", "Ask once"), c("222222", "me@t", "Picked, reworded")];
        let same: std::collections::HashSet<String> = ["999999".to_string()].into();
        assert!(is_yours(&c("888888", "me@t", "Ask once"), &same, &ours), "amended");
        assert!(is_yours(&c("999999", "kim@t", "Shared fix"), &same, &ours), "the same change");
        assert!(!is_yours(&c("777777", "kim@t", "Ask once"), &same, &ours), "a colleague's, same subject");
        assert!(!is_yours(&c("666666", "kim@t", "Bump"), &same, &ours));
    }

    /// Another worktree is named from the folder holding the repository, else from home.
    #[test]
    fn worktrees_are_named_from_beside_the_repository() {
        let top = Path::new("/p/app");
        assert_eq!(worktree_shown(top, Path::new("/p/wt/login")), "wt/login");
        assert_eq!(worktree_shown(top, Path::new("/p/app-fix")), "app-fix");
        let home = std::env::var("HOME").unwrap();
        let far = Path::new(&home).join(".local/share/tarae/worktrees/app/fix");
        assert_eq!(worktree_shown(top, &far), "~/.local/share/tarae/worktrees/app/fix");
    }

    #[test]
    fn worktrees_and_their_branches() {
        let out = "worktree /r\nHEAD aaa\nbranch refs/heads/main\n\nworktree /w/fix\nHEAD bbb\nbranch \
                   refs/heads/fix/login\n\nworktree /w/det\nHEAD ccc\ndetached\n\n";
        assert_eq!(
            parse_worktrees(out),
            [
                (PathBuf::from("/r"), Some("main".into())),
                (PathBuf::from("/w/fix"), Some("fix/login".into())),
                (PathBuf::from("/w/det"), None)
            ]
        );
    }

    #[test]
    fn numstat_counts() {
        let m = numstat("3\t1\tsrc/a.rs\0-\t-\tlogo.png\0");
        assert_eq!(m["src/a.rs"], (Some(3), Some(1)));
        assert_eq!(m["logo.png"], (None, None), "binary");
    }

    /// #147: a clean tree's git tab says more than "nothing to commit" — the branch's commits past its base
    /// (what bring takes; the newest few, the rest counted) and the last commit. (A repository of the test's
    /// own, in its temp folder.)
    #[test]
    fn a_clean_tree_shows_what_bring_takes_and_the_last_commit() {
        let dir = std::env::temp_dir().join(format!("tarae-gitpanel-clean-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let run = |args: &[&str]| {
            let id = ["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"];
            let ok = std::process::Command::new("git").args(id).args(args).current_dir(&dir).output();
            assert!(ok.unwrap().status.success(), "git {args:?}");
        };
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        run(&["init", "-q", "-b", "main"]);
        run(&["add", "a.txt"]);
        run(&["commit", "-qm", "base"]);
        run(&["checkout", "-qb", "feat"]);
        for i in 1..=7 {
            run(&["commit", "-q", "--allow-empty", "-m", &format!("step {i}")]);
        }
        let st = status(&dir, Some("main".into())).unwrap();
        let log = st.log.clone().expect("clean: its history");
        assert_eq!((log.base.as_deref(), log.ahead, log.commits.len()), (Some("main"), 7, LOG_COMMITS));
        assert!(
            log.commits[0].ends_with(" step 7")
                && log.last.as_deref().is_some_and(|l| l.contains(" step 7 · "))
        );
        // Drawn: the tab says it in the list's place
        let mut ed = Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        ed.session.sidebar = Some(crate::sidebar::Sidebar::new(crate::sidebar::Mode::Git, dir.clone()));
        ed.session.git_status = Some(st.with_rows());
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 120, 30).unwrap();
        let mut vt = vt100::Parser::new(30, 120, 0);
        vt.process(&bytes);
        let s = vt.screen().contents();
        for part in [
            "nothing to commit",
            "7 commits past main — what",
            "bring takes:",
            " step 7",
            " step 3",
            "… 2 more",
        ] {
            assert!(s.contains(part), "{part:?} in\n{s}");
        }
        assert!(!s.contains("last commit"), "the last commit is the first listed — not twice: {s}");
        // Nothing past the base: said, and the last commit under it
        let st = status(&dir, Some("feat".into())).unwrap();
        ed.session.git_status = Some(st.with_rows());
        let mut bytes = Vec::new();
        crate::term::render(&mut ed, &mut bytes, 120, 30).unwrap();
        vt.process(&bytes);
        let s = vt.screen().contents();
        assert!(s.contains("nothing past feat — nothing") && s.contains("last commit"), "{s}");
        // Changes to commit: no history — the list is the changes
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        assert_eq!(status(&dir, Some("main".into())).unwrap().log, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A save and a stage at once (`:w`, then `s` on everything while it's written — a slow disk): the stage
    /// waits for the save, so what's staged is what was saved; and a save's temp file — one asked after it
    /// waited, here a stray one beside — is never staged. (A repository of the test's own, in its temp folder.)
    #[test]
    fn staging_waits_for_saves_and_never_takes_a_temp_file() {
        let dir = std::env::temp_dir().join(format!("tarae-gitpanel-saves-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let dir = std::fs::canonicalize(&dir).unwrap();
        let git_out = |args: &[&str]| {
            let id = ["-c", "user.email=t@t", "-c", "user.name=t", "-c", "commit.gpgsign=false"];
            let out =
                std::process::Command::new("git").args(id).args(args).current_dir(&dir).output().unwrap();
            assert!(out.status.success(), "git {args:?}");
            String::from_utf8(out.stdout).unwrap()
        };
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git_out(&["init", "-q"]);
        git_out(&["add", "a.txt"]);
        git_out(&["commit", "-qm", "base"]);
        // (named as `disk::write_atomic` names them — at the top and in a folder)
        std::fs::write(dir.join(format!(".b.txt.tarae-{}-99.tmp", std::process::id())), "half").unwrap();
        std::fs::write(dir.join("sub/.c.rs.tarae-1-0.tmp"), "half").unwrap();
        let mut ed = Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        ed.open(&dir.join("a.txt")).unwrap();
        ed.session.git_status = Some(status(&dir, None).unwrap().with_rows());
        ed.saver.slow = std::time::Duration::from_millis(600);
        for k in ["c", "T", "W", "O", "esc", ":", "w", "ret"] {
            ed.handle_key(k.parse().unwrap());
        }
        ed.git_stage("", true);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !ed.toasts.iter().any(|t| t.text == "Staged everything") {
            let left = end.saturating_duration_since(std::time::Instant::now());
            let ev = ed.events.recv_timeout(left).expect("staged before the timeout");
            ed.handle_event(ev);
        }
        assert_eq!(git_out(&["show", ":a.txt"]), "TWOne\n", "staged what was saved");
        assert_eq!(git_out(&["ls-files"]), "a.txt\n", "no temp file staged");
        std::fs::remove_dir_all(&dir).ok();
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
        let st = status(&dir, None).unwrap();
        let rows: Vec<_> = st.unstaged.iter().map(|e| (e.rel.as_str(), e.added, e.removed)).collect();
        assert_eq!(rows, [("b.txt", Some(1), Some(0))], "its counts still come");
        assert_eq!(std::fs::read(dir.join(".git/index")).unwrap(), before, "the index was rewritten");
        std::fs::remove_dir_all(&dir).ok();
    }
}
