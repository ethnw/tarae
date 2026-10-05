//! Rebasing a session onto its base (roadmap #3): `:session-bring` when the base moved since the session
//! left it — a card says how far (`What::Rebase`), and `y` rebases in the session's checkout. tarae runs git
//! itself (Claude's sandbox can't write `.git`), in the task panel. Clean: the bring card (`What::Rebased` —
//! bring · preflight, then bring · not now). A conflict: the rebase stops, and the session's own Claude is
//! asked in its chat to resolve the files — no git, no commit. When its turn ends with no markers left in
//! them, a review shows them against the commit being replayed and a card (`What::RebaseResolved`) offers
//! continue (staged, `git rebase --continue` — it may stop again: the same loop) or abort. Markers left: the
//! files are named. While it's under way the session's tab, chip and row say `rebasing n/m`, and
//! `:session-end` and `:restart` wait for it; `:session-bring` puts its card up again. `:rebase-continue` ·
//! `:rebase-abort` do the card's work by hand, in the session rebasing whichever is in front — and take up a
//! rebase tarae didn't start (one from before a restart).

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::editor::Editor;
use crate::gitmenu::git;
use crate::offer::{Offer, What};

/// How often the step (n of m) is read while git runs.
const POLL: Duration = Duration::from_millis(150);
/// Most bytes of each side's diff Claude is sent (it reads the rest itself).
const SIDE_MAX: usize = 60_000;

/// git as tarae runs it for a rebase — outside the sandbox, so with no hooks: a `core.hooksPath` inside the
/// checkout (`.githooks`) is a folder the session's Claude can write, and pre-rebase, post-checkout,
/// post-rewrite and the commit hooks would run what it put there unsandboxed (review, 2026-10-04 — the same
/// flag as `gitmenu::NO_HOOKS`, here as a shell line for the task panel). And without
/// the conflict advice: its `hint:` lines tell you to `git add` and `--continue` yourself — tarae's step here —
/// and made the task panel tall enough to push the resolved card over the review (a shot).
const GIT: &str = "git -c core.hooksPath=/dev/null -c advice.mergeConflict=false";

/// A session whose base moved since it left it — what the rebase card says (`y` rebases), and once it's
/// rebased, what the bring card says (`moved` 0 then).
#[derive(Clone, Debug)]
pub struct Moved {
    pub root: PathBuf,
    /// The session's name, its branch and its base.
    pub name: String,
    pub branch: String,
    pub base: String,
    /// The base's commits the branch hasn't · the branch's the base hasn't.
    pub moved: usize,
    pub ahead: usize,
}

impl Moved {
    /// The card before the rebase.
    pub fn card(&self) -> Offer {
        let (base, branch) = (&self.base, &self.branch);
        let q = format!(
            "Rebase {branch} onto {base} first? {base} moved {} since {branch} left it; {branch} is {} ahead.\n\n\
             Its commits are replayed on {base}'s tip one by one, in its checkout. A conflict goes to its \
             Claude to resolve, and you review that before the rebase goes on.",
            crate::worktree::commits(self.moved),
            crate::worktree::commits(self.ahead),
        );
        Offer::new(
            format!("Bring {}: {base} moved", self.name),
            &q,
            "Rebasing…",
            format!("Couldn't rebase {branch}"),
            What::Rebase(Box::new(self.clone())),
        )
    }

    /// The card once it's rebased (`rebased`), or asked to bring one already on its base's tip (#155: `b` in
    /// the list brought it at once, no `p`): bring it now, or after the preflight — the quick set (`quick`:
    /// its steps, when the project has two sets), the full one being for before publishing.
    pub fn bring_card(&self, quick: Option<Vec<String>>, rebased: bool) -> Offer {
        let (base, them) = (&self.base, if self.ahead == 1 { "it" } else { "them" });
        let which = match quick {
            Some(steps) => format!(
                "the quick preflight passes?\n\nQuick: {} — :preflight-full runs the rest, before publishing.",
                steps.join(", ")
            ),
            None => "the preflight passes?".into(),
        };
        let q = format!(
            "{} on {base}'s tip now.\n\nBring {them} into {base} — now, or once {which}",
            crate::worktree::commits(self.ahead),
        );
        let title = match rebased {
            true => format!("Rebased {} onto {base}", self.branch),
            false => format!("Bring {} into {base}", self.branch),
        };
        Offer::new(
            title,
            &q,
            "Bringing…",
            format!("Couldn't bring {}", self.branch),
            What::Rebased(Box::new(self.clone())),
        )
    }

    /// The card for a session with nothing to bring, behind its base (`ahead` 0, #186): move it up to its
    /// base's tip — a fast-forward in its checkout.
    pub fn up_card(&self) -> Offer {
        let (base, branch) = (&self.base, &self.branch);
        let q = format!(
            "{branch} has no commits past {base} — nothing to bring. It's {} behind: move it up to {base}'s \
             tip?\n\nA fast-forward in its checkout: nothing of its own to replay, nothing lost.",
            crate::worktree::commits(self.moved),
        );
        Offer::new(
            format!("Move {} up to {base}", self.name),
            &q,
            "Moving up…",
            format!("Couldn't move {branch} up"),
            What::UpTo(Box::new(self.clone())),
        )
    }
}

/// Why a session's checkout can't be moved up to its base: changes in no commit — files git has changed
/// (new ones untracked don't count: git refuses only one in the way, and says so), or buffers unsaved there
/// (`unsaved`). None: nothing holds it (or git can't say — the move itself will).
pub fn held_up(root: &Path, unsaved: &[PathBuf]) -> Option<String> {
    let status = git(root, &["status", "--porcelain", "-z", "--untracked-files=no"], None).ok()?;
    let mut files: Vec<String> = status.split('\0').filter_map(|e| e.get(3..)).map(str::to_string).collect();
    for p in unsaved.iter().filter_map(|p| p.strip_prefix(root).ok()) {
        let p = p.to_string_lossy().into_owned();
        if !files.contains(&p) {
            files.push(p);
        }
    }
    files.retain(|f| !f.is_empty());
    let some: Vec<&str> = files.iter().take(3).map(String::as_str).collect();
    let more = if files.len() > 3 { ", …" } else { "" };
    (!files.is_empty())
        .then(|| format!("it has changes in {}{more} — commit or stash them first", some.join(", ")))
}

/// Session `m`'s branch moved up to its base's tip — checked again first (nothing of its own since, no
/// changes): the tip it's at now, short (on a worker).
fn move_up(m: &Moved, unsaved: &[PathBuf]) -> Result<String, String> {
    let base_tip = crate::worktree::sha(&m.root, &format!("refs/heads/{}", m.base))?;
    let head = crate::worktree::sha(&m.root, "HEAD")?;
    if crate::worktree::count(&m.root, &format!("{base_tip}..{head}")) > 0 {
        return Err(format!("{} has commits of its own since — :session-bring again", m.branch));
    }
    if let Some(why) = held_up(&m.root, unsaved) {
        return Err(why);
    }
    crate::gitmenu::git_no_hooks(&m.root, &["merge", "--ff-only", "-q", &base_tip])?;
    Ok(base_tip.chars().take(7).collect())
}

/// Where a rebase stopped (read on a worker).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stop {
    /// Commit n of m.
    pub step: Option<(usize, usize)>,
    /// Its conflicted files, relative to the checkout.
    pub files: Vec<String>,
    /// The commit being replayed: (full sha, short, subject).
    pub commit: Option<(String, String, String)>,
    /// The branch being rebased · what onto (short) — for one tarae didn't start.
    pub head: Option<String>,
    pub onto: Option<String>,
}

impl Stop {
    /// `commit 2 of 5 (a1b2c3d Add login)`.
    fn at(&self) -> String {
        let commit = self.commit.as_ref().map(|(_, short, subject)| format!("{short} {subject}"));
        match (self.step, commit) {
            (Some((n, m)), Some(c)) => format!("commit {n} of {m} ({c})"),
            (Some((n, m)), None) => format!("commit {n} of {m}"),
            (None, Some(c)) => c,
            (None, None) => "a commit".into(),
        }
    }
}

/// A rebase under way in a session's checkout (`Session.rebase`).
pub struct Rebase {
    pub branch: String,
    pub base: String,
    pub stage: Stage,
    pub step: Option<(usize, usize)>,
    pub stop: Stop,
    /// Its Claude was asked to resolve this stop (the end of its turns is looked at) · the ask, waiting for
    /// the turn under way to end.
    pub asked: bool,
    waiting: Option<Ask>,
    /// What the resolution changed besides the conflicted files (#129): in its review, staged with them.
    pub also: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// git runs in the task panel (run `generation`) — rebasing or continuing, or `aborting`.
    Running { generation: u64, aborting: bool },
    /// Stopped at a conflict.
    Stopped,
    /// Its files resolved: the review and the card are up.
    Resolved,
}

impl Rebase {
    /// `rebasing 2/5` — what the session's tab, chip and row say.
    pub fn said(&self) -> String {
        match self.step {
            Some((n, m)) => format!("rebasing {n}/{m}"),
            None => "rebasing".into(),
        }
    }

    /// Stopped for the user (said in the warning color), not git at work.
    pub fn stopped(&self) -> bool {
        !matches!(self.stage, Stage::Running { .. })
    }

    /// Why what would get in its way waits — `session` is its session's name.
    pub fn why(&self, session: &str) -> String {
        match self.stage {
            Stage::Running { .. } => format!("{session} is {} — let it finish", self.said()),
            _ => format!("{session} is {} — :rebase-continue or :rebase-abort first", self.said()),
        }
    }
}

/// What the session's Claude is asked: the message, its context (the files and both sides), the chip.
#[derive(Clone, Debug)]
pub struct Ask {
    pub question: String,
    about: String,
    chip: String,
}

// ── Git (workers) ────────────────────────────────────────────────────────────

fn git_dir(root: &Path) -> Option<PathBuf> {
    git(root, &["rev-parse", "--absolute-git-dir"], None).ok().map(|s| PathBuf::from(s.trim()))
}

/// The rebase's own folder in the git folder, while one is under way (`git am`'s isn't one).
fn state_dir(git_dir: &Path) -> Option<PathBuf> {
    let merge = git_dir.join("rebase-merge");
    let apply = git_dir.join("rebase-apply");
    if merge.is_dir() { Some(merge) } else { apply.join("rebasing").exists().then_some(apply) }
}

/// Commit n of m, as git counts in the rebase's folder.
fn step_of(state: &Path) -> Option<(usize, usize)> {
    let read = |f: &str| std::fs::read_to_string(state.join(f)).ok()?.trim().parse::<usize>().ok();
    match state.ends_with("rebase-merge") {
        true => Some((read("msgnum")?, read("end")?)),
        false => Some((read("next")?, read("last")?)),
    }
}

/// The rebase under way in the checkout at `root` — where it stopped — if there's one.
pub fn under_way(root: &Path) -> Option<Stop> {
    let state = state_dir(&git_dir(root)?)?;
    let read = |f: &str| std::fs::read_to_string(state.join(f)).ok().map(|s| s.trim().to_string());
    // (`ls-files -u`: a row per side, by path — a read that never takes the index lock)
    let mut files: Vec<String> = git(root, &["ls-files", "-u", "-z"], None)
        .unwrap_or_default()
        .split('\0')
        .filter_map(|e| e.split_once('\t').map(|(_, p)| p.to_string()))
        .collect();
    files.dedup();
    let commit =
        git(root, &["log", "-1", "--format=%H%x1f%h%x1f%s", "REBASE_HEAD"], None).ok().and_then(|l| {
            let mut p = l.trim().splitn(3, '\x1f').map(str::to_string);
            Some((p.next()?, p.next()?, p.next().unwrap_or_default()))
        });
    Some(Stop {
        step: step_of(&state),
        files,
        commit,
        head: read("head-name").map(|h| h.trim_start_matches("refs/heads/").to_string()),
        onto: read("onto").map(|o| o.chars().take(7).collect()),
    })
}

/// How a run of git ended (read on a worker after it).
pub enum Ended {
    /// Stopped (at a conflict — Claude's ask made ready — or for a reason the panel says).
    Stopped(Box<Stop>, Option<Ask>),
    /// Done: the branch's commits on the base's tip, at `tip`.
    Done {
        ahead: usize,
        tip: String,
    },
    Aborted,
    /// Nothing under way and not done: git stopped before it began (the panel says why).
    Failed,
}

fn ended(root: &Path, branch: &str, base: &str, aborting: bool) -> Ended {
    if let Some(stop) = under_way(root) {
        let ask = (!stop.files.is_empty()).then(|| ask(root, branch, base, &stop));
        return Ended::Stopped(Box::new(stop), ask);
    }
    if aborting {
        return Ended::Aborted;
    }
    let base_tip = crate::worktree::sha(root, &format!("{base}^{{commit}}"));
    match (base_tip, crate::worktree::sha(root, "HEAD")) {
        (Ok(b), Ok(t)) if crate::worktree::is_ancestor(root, &b, &t) => {
            Ended::Done { ahead: crate::worktree::count(root, &format!("{b}..{t}")), tip: t[..7].to_string() }
        }
        _ => Ended::Failed,
    }
}

/// `s` cut to `max` bytes (on a character's edge), saying so.
fn cut(mut s: String, max: usize) -> String {
    if s.len() <= max {
        return s;
    }
    let mut at = max;
    while !s.is_char_boundary(at) {
        at -= 1;
    }
    let more = s.len() - at;
    s.truncate(at);
    s.push_str(&format!("\n… ({more} more bytes — read the files themselves)\n"));
    s
}

/// The session's Claude's ask for a stop with conflicts: the files and both sides — what the commit being
/// replayed changed in them, and what the base (and the commits replayed before it) changed since that
/// commit's parent.
fn ask(root: &Path, branch: &str, base: &str, stop: &Stop) -> Ask {
    let side = |to: &str| {
        let mut args = vec!["diff", "--no-color", "--no-ext-diff", "REBASE_HEAD^", to, "--"];
        args.extend(stop.files.iter().map(String::as_str));
        cut(git(root, &args, None).unwrap_or_default(), SIDE_MAX)
    };
    let n = stop.files.len();
    let (those, them) = if n == 1 { ("that file", "it's") } else { ("those files", "they're") };
    let question = format!(
        "Rebasing {branch} onto {base} stopped at {}: git couldn't merge {}. Resolve the conflicts in {those} \
         — keep what both sides meant, and leave no conflict markers. Don't run git and don't commit: tarae \
         stages them and goes on once I've looked. Tell me when {them} resolved.",
        stop.at(),
        stop.files.join(", "),
    );
    let short = stop.commit.as_ref().map_or("", |c| c.1.as_str());
    let about = format!(
        "<rebase branch=\"{branch}\" onto=\"{base}\" commit=\"{short}\">\n<conflicted>\n{}\n</conflicted>\n\
         <theirs note=\"what the commit being replayed changed in them\">\n{}</theirs>\n\
         <ours note=\"what {base}, and the commits replayed before this one, changed in them since that \
         commit's parent\">\n{}</ours>\n</rebase>",
        stop.files.join("\n"),
        side("REBASE_HEAD"),
        side("HEAD"),
    );
    let chip = format!("rebase · {n} conflicted file{}", if n == 1 { "" } else { "s" });
    Ask { question, about, chip }
}

/// Of `files` (relative, with an open buffer's text where there's one), those with conflict markers left.
fn marked(root: &Path, files: Vec<(String, Option<ropey::Rope>)>) -> Vec<String> {
    files
        .into_iter()
        .filter(|(rel, text)| {
            let text = match text {
                Some(t) => t.to_string(),
                None => std::fs::read_to_string(root.join(rel)).unwrap_or_default(),
            };
            crate::conflict::has_markers(&text)
        })
        .map(|(rel, _)| rel)
        .collect()
}

/// Every file `git add -A` would stage in the checkout (relative): changed against the index, unmerged,
/// untracked. `status` with optional locks off, as Claude's runs and the git tab read it — never a `git diff`
/// over the working tree (it refreshes the index).
fn unstaged(root: &Path) -> Vec<String> {
    let out = git(root, &["status", "--porcelain", "-z", "--untracked-files=all"], None).unwrap_or_default();
    let mut v = Vec::new();
    let mut entries = out.split('\0').filter(|e| !e.is_empty());
    while let Some(e) = entries.next() {
        let (xy, path) = (e.get(..2).unwrap_or_default(), e.get(3..).unwrap_or_default());
        // (a rename or copy in the index: its old path comes next)
        if xy.starts_with(['R', 'C']) {
            entries.next();
        }
        if !xy.ends_with(' ') && !path.is_empty() {
            v.push(path.to_string());
        }
    }
    v
}

/// `a.txt, b.rs` — the first few, the rest counted.
fn named(files: &[String]) -> String {
    let mut s = files.iter().take(4).cloned().collect::<Vec<_>>().join(", ");
    if files.len() > 4 {
        s.push_str(&format!(" and {} more", files.len() - 4));
    }
    s
}

// ── The flow ─────────────────────────────────────────────────────────────────

/// The task panel's run is closed or replaced (`space m c`, another task) while it runs: if it's the
/// rebase's git, what git left is read as at its end — else the session would say `rebasing` for good.
pub fn run_dropped(ed: &mut Editor) {
    let Some(t) = ed.session.task_run.as_ref().filter(|t| t.state == crate::task::State::Running) else {
        return;
    };
    let generation = t.generation();
    if ran(ed, generation) {
        ed.set_warning("the rebase's git was stopped — it's read as git left it");
    }
}

/// A git run of the rebase's ended (`task.rs`) — true if it was one (what's next is decided on a worker).
pub fn ran(ed: &mut Editor, generation: u64) -> bool {
    let Some(r) = ed.session.rebase.as_ref() else { return false };
    let Stage::Running { generation: g, aborting } = r.stage else { return false };
    if g != generation {
        return false;
    }
    let (root, branch, base) = (ed.session.root.clone(), r.branch.clone(), r.base.clone());
    // (git's exit says too little — a stop at conflicts exits 1 — so the run reads as under way until its end
    // is read, then as git left it: `rebase_ended`)
    let exit = match ed.session.task_run.as_mut().filter(|t| t.generation() == generation) {
        Some(t) => std::mem::replace(&mut t.state, crate::task::State::Running),
        None => crate::task::State::Failed(None),
    };
    ed.events.jobs().spawn(move || {
        let end = ended(&root, &branch, &base, aborting);
        move |ed: &mut Editor| ed.rebase_ended(generation, end, exit)
    });
    true
}

/// While git runs: the step it's at, read every `POLL` (nothing once it's done).
fn poll(ed: &Editor, root: PathBuf, generation: u64, dir: Option<PathBuf>) {
    ed.events.jobs().spawn(move || {
        std::thread::sleep(POLL);
        let dir = dir.or_else(|| git_dir(&root));
        let step = dir.as_deref().and_then(state_dir).as_deref().and_then(step_of);
        move |ed: &mut Editor| {
            let running = ed
                .session
                .task_run
                .as_ref()
                .is_some_and(|t| t.generation() == generation && t.state == crate::task::State::Running);
            let Some(r) = ed
                .session
                .rebase
                .as_mut()
                .filter(|r| matches!(r.stage, Stage::Running { generation: g, .. } if g == generation))
            else {
                return;
            };
            match step.filter(|s| r.step != Some(*s)) {
                Some(s) => r.step = Some(s),
                None => ed.unchanged = true,
            }
            if running {
                poll(ed, root, generation, dir);
            }
        }
    });
}

/// The session's Claude was asked, and its turn ended (`chat.rs`): an ask that waited for it goes now;
/// else, while the rebase is stopped at what it was asked about, its files are looked at for markers.
pub fn turn_over(ed: &mut Editor) {
    let Some(r) = ed.session.rebase.as_mut() else { return };
    if let Some(a) = r.waiting.take() {
        return ed.rebase_ask(a);
    }
    if r.stage != Stage::Stopped || !r.asked {
        return;
    }
    let conflicted = r.stop.files.clone();
    let files = ed.rebase_texts(&conflicted);
    let root = ed.session.root.clone();
    ed.events.jobs().spawn(move || {
        let left = marked(&root, files);
        // (what else the resolution changed — a snapshot blessed again: #129, left unstaged it stopped git)
        let also = unstaged(&root).into_iter().filter(|f| !conflicted.contains(f)).collect();
        move |ed: &mut Editor| ed.rebase_checked(left, also)
    });
}

impl Editor {
    /// A session by its checkout.
    fn session_at(&self, root: &Path) -> Option<crate::event::SessionId> {
        std::iter::once(&self.session).chain(&self.sessions).find(|l| l.root == root).map(|l| l.id)
    }

    /// What keeps git from running in the session in front now: its files unsaved, a task in the panel.
    fn rebase_free(&self) -> Result<(), String> {
        let root = &self.session.root;
        let unsaved: Vec<String> = self
            .docs
            .iter()
            .filter(|d| d.is_modified() && d.path.as_ref().is_some_and(|p| p.starts_with(root)))
            .map(|d| d.display_name(root))
            .collect();
        if !unsaved.is_empty() {
            return Err(format!("unsaved changes in {} — save them first", unsaved.join(", ")));
        }
        match self.session.task_run.as_ref().filter(|r| r.state == crate::task::State::Running) {
            Some(r) => Err(format!("{} is running — let it finish, or stop it (space m c)", r.task.name)),
            None => Ok(()),
        }
    }

    /// `files` (relative) with an unsaved buffer's text — what the user typed is what's checked. A saved one's
    /// is the disk's (read on the worker): its reload can lag the write — Claude's resolution was on disk while
    /// the buffer still showed the markers (a shot, 2026-10-04).
    fn rebase_texts(&self, files: &[String]) -> Vec<(String, Option<ropey::Rope>)> {
        let root = &self.session.root;
        files
            .iter()
            .map(|rel| {
                let p = root.join(rel);
                let unsaved =
                    self.docs.iter().find(|d| d.path.as_deref() == Some(p.as_path()) && d.is_modified());
                (rel.clone(), unsaved.map(|d| d.text.clone()))
            })
            .collect()
    }

    /// `cmd` (git) in the task panel for the rebase in front — its step read while it runs.
    fn rebase_git(&mut self, name: &str, cmd: String, aborting: bool) -> Result<(), String> {
        let root = self.session.root.clone();
        let task = crate::task::Task { name: name.into(), cmd, cwd: root.clone() };
        let generation = self.task_start_git(task).ok_or("git didn't start")?;
        if let Some(r) = self.session.rebase.as_mut() {
            r.stage = Stage::Running { generation, aborting };
        }
        poll(self, root, generation, None);
        Ok(())
    }

    /// `y` on the base-moved card: its session to the front, and its rebase in the task panel.
    pub fn rebase_start(&mut self, card: u64, m: Moved) {
        let Some(id) = self.session_at(&m.root) else {
            return self.offer_finished(card, Err(format!("{} isn't open any more", m.name)));
        };
        self.session_switch(id);
        if let Some(r) = self.session.rebase.as_ref() {
            return self.offer_finished(card, Err(r.why(&m.name)));
        }
        if let Err(why) = self.rebase_free() {
            return self.offer_finished(card, Err(why));
        }
        self.offer_finished(card, Ok(()));
        self.session.rebase = Some(Rebase {
            branch: m.branch.clone(),
            base: m.base.clone(),
            stage: Stage::Stopped,
            step: None,
            stop: Stop::default(),
            asked: false,
            waiting: None,
            also: Vec::new(),
        });
        let cmd = format!("{GIT} rebase {}", crate::packs::sh_quote(&m.base));
        if let Err(e) = self.rebase_git(&format!("rebase onto {}", m.base), cmd, false) {
            self.session.rebase = None;
            self.set_error(format!("rebase {}: {e}", m.branch));
        }
    }

    /// A git run of the rebase ended — `end` as git left it, `exit` as its process did.
    fn rebase_ended(&mut self, generation: u64, end: Ended, exit: crate::task::State) {
        use crate::task::State;
        let running = |r: &Rebase| matches!(r.stage, Stage::Running { generation: g, .. } if g == generation);
        let Some((branch, base)) =
            self.session.rebase.as_ref().filter(|r| running(r)).map(|r| (r.branch.clone(), r.base.clone()))
        else {
            return;
        };
        // The panel says it as it is: stopped at its conflicts (not failed), done, aborted — or git's exit
        let state = match &end {
            Ended::Stopped(stop, _) if !stop.files.is_empty() => {
                let n = stop.files.len();
                State::Stopped(format!("{n} conflict{}", if n == 1 { "" } else { "s" }))
            }
            Ended::Done { .. } | Ended::Aborted => State::Passed,
            _ => exit,
        };
        if let Some(t) = self.session.task_run.as_mut().filter(|t| t.generation() == generation) {
            t.state = state;
        }
        let name = self.session.label().to_string();
        self.git_refresh();
        // (HEAD moved: its buffers' git bases and the blame, now — not when the watch next looks)
        let root = self.session.root.clone();
        self.git_reload_bases_in(&root);
        match end {
            Ended::Stopped(stop, ask) => {
                let at = stop.at();
                if let Some(r) = self.session.rebase.as_mut() {
                    (r.stage, r.step, r.asked) = (Stage::Stopped, stop.step, false);
                    r.stop = *stop;
                    r.also.clear();
                }
                match ask {
                    Some(a) => self.rebase_ask(a),
                    None => self.set_warning(format!(
                        "the rebase stopped at {at} — the task panel says why; :rebase-continue or :rebase-abort"
                    )),
                }
            }
            Ended::Done { ahead, tip } => {
                self.session.rebase = None;
                self.rebase_cards_gone();
                self.set_success(format!(
                    "Rebased {branch} onto {base} — {} on top, now at {tip}",
                    crate::worktree::commits(ahead)
                ));
                // (a session's: bring it now? The checkout tarae started in has no base to go into)
                if self.session.id != 0 {
                    let root = self.session.root.clone();
                    let quick = self.preflight_quick();
                    self.push_offer(
                        Moved { root, name, branch, base, moved: 0, ahead }.bring_card(quick, true),
                    );
                }
            }
            Ended::Aborted => {
                self.session.rebase = None;
                self.rebase_cards_gone();
                self.set_status(format!("Rebase aborted — {branch} is back where it was"));
            }
            Ended::Failed => {
                self.session.rebase = None;
                self.set_error(format!(
                    "rebase {branch}: git stopped before it began — the task panel says why"
                ));
            }
        }
    }

    /// The session's Claude asked to resolve the conflicts (its chat opened if it wasn't) — or, while it's
    /// answering something else, once that turn ends.
    fn rebase_ask(&mut self, a: Ask) {
        if self.session.chat.is_none() {
            crate::chat::open(self);
            if let Some(c) = &mut self.session.chat {
                (c.focused, c.unfolded) = (false, false);
            }
        }
        let Some(r) = self.session.rebase.as_mut() else { return };
        let files = named(&r.stop.files);
        if self.session.chat.as_ref().is_some_and(crate::chat::Chat::busy) {
            r.waiting = Some(a);
            return self.set_status(format!("conflicts in {files} — Claude is asked once its turn ends"));
        }
        r.asked = true;
        match crate::chat::submit(self, &a.question, Some(a.about), Some(a.chip)) {
            true => self.set_status(format!(
                "conflicts in {files} — Claude is resolving them; you review them before the rebase goes on"
            )),
            false => self.set_warning(format!(
                "conflicts in {files} — couldn't reach Claude (the chat says why); resolve them, then \
                 :rebase-continue"
            )),
        }
    }

    /// The markers looked for after Claude's turn: some left — named; none — the review of the resolved
    /// files, and of what else it changed (`also`, staged with them), against the commit being replayed, and
    /// the card.
    fn rebase_checked(&mut self, left: Vec<String>, also: Vec<String>) {
        let Some(r) = self.session.rebase.as_mut().filter(|r| r.stage == Stage::Stopped) else { return };
        if !left.is_empty() {
            return self.set_warning(format!(
                "conflict markers still in {} — ask Claude again, or fix them and :rebase-continue",
                named(&left)
            ));
        }
        r.stage = Stage::Resolved;
        r.also = also;
        let mut stop = r.stop.clone();
        stop.files.extend(r.also.iter().cloned());
        match stop.commit {
            // (the card once the review is open — it's about what the review shows)
            Some((sha, short, subject)) => {
                if let Some(c) = &mut self.session.chat {
                    c.focused = false;
                }
                let root = self.session.root.clone();
                let label = format!("{short} {subject}");
                crate::codereview::start_replay(
                    self,
                    root,
                    sha,
                    label,
                    stop.files,
                    Editor::rebase_resolved_card,
                );
            }
            None => self.rebase_resolved_card(),
        }
    }

    /// The resolved card: continue · abort · not now — while the rebase is still resolved and waiting.
    fn rebase_resolved_card(&mut self) {
        if self.session.rebase.as_ref().is_some_and(|r| r.stage == Stage::Resolved) {
            let o = self.rebase_card();
            self.push_offer(o);
        }
    }

    /// The card of the front session's rebase, stopped (not git at work): `y` stages its files and goes on ·
    /// `X` aborts · `n` not now. Resolved, it's about the review up; stopped short of that, about where it
    /// stopped. Either way it says the files aren't staged yet — `y` stages them (#175: the card offered to
    /// continue while the git tab showed them unmerged, and nothing said `y` would stage them).
    fn rebase_card(&self) -> Offer {
        let r = self.session.rebase.as_ref().expect("a rebase");
        let (stop, branch, name) = (&r.stop, &r.branch, self.session.label());
        let n = stop.files.len() + r.also.len();
        let them = if n == 1 { "it" } else { "them" };
        // (short: the card shares the code's room with the review it's about and the task panel — what `y`
        // does, its button says)
        let also = match r.also.is_empty() {
            true => String::new(),
            false => format!(" It also changed {} — staged with them.", named(&r.also)),
        };
        let (title, first) = match (r.stage, stop.files.is_empty()) {
            (Stage::Resolved, _) => (
                format!("{name}: conflicts resolved"),
                format!(
                    "{} resolved for {}.{also} The review shows {them} against that commit — not staged yet: y \
                     stages {them} and goes on.",
                    named(&stop.files),
                    stop.at(),
                ),
            ),
            (_, false) => (
                format!("{name}: {}", r.said()),
                format!(
                    "Rebasing {branch} onto {} stopped at {}. {} {} unmerged: y stages {them} once no conflict \
                     markers are left, and goes on.",
                    r.base,
                    stop.at(),
                    named(&stop.files),
                    if stop.files.len() == 1 { "is" } else { "are" },
                ),
            ),
            (_, true) => (
                format!("{name}: {}", r.said()),
                format!(
                    "Rebasing {branch} onto {} stopped at {} — the task panel says why. y goes on.",
                    r.base,
                    stop.at(),
                ),
            ),
        };
        let q = format!("{first}\n\nX aborts the rebase: {branch} goes back as it was.");
        let root = self.session.root.clone();
        Offer::new(
            title,
            &q,
            "Going on with the rebase…",
            "Couldn't go on with the rebase".into(),
            What::RebaseResolved { root },
        )
        .warning(2..3)
    }

    /// Session `id`'s rebase met where it's in the way (`:session-bring`, #175): its card up in front — `y`
    /// goes on, `X` aborts — instead of a line saying what to type. Git at work there: Err says so.
    pub(crate) fn rebase_offer(&mut self, id: crate::event::SessionId) -> Result<(), String> {
        let mut card = Err(String::new());
        self.with_session(id, |ed| {
            let name = ed.session.label().to_string();
            card = match ed.session.rebase.as_ref() {
                Some(r) if !r.stopped() => Err(r.why(&name)),
                Some(_) => Ok((ed.session.root.clone(), ed.rebase_card())),
                None => Err(format!("no rebase under way in {name}")),
            };
        });
        let (root, o) = card?;
        // (one card for it: one left from before goes)
        let mine = |o: &Offer| matches!(&o.what, What::RebaseResolved { root: r } if *r == root);
        self.offers.retain(|o| !mine(o));
        for l in std::iter::once(&mut self.session).chain(&mut self.sessions) {
            l.parked.retain(|o| !mine(o));
        }
        self.push_offer(o);
        Ok(())
    }

    /// `:rebase-continue [<session>]` · `:rebase-abort [<session>]`: the rebase of the session named — else
    /// of the session rebasing: the one in front if it is, the only one if one is, a pick if several (#175:
    /// after the inbox brought another session to the front, it went to that one — "no rebase under way").
    /// None known: each session's checkout is looked at (one from before a restart, or by hand).
    pub fn rebase_by_hand(&mut self, which: &str, go_on: bool) -> Result<(), String> {
        if !which.trim().is_empty() {
            let (id, _) = self.session_named(which)?;
            return self.rebase_in(id, go_on);
        }
        let rebasing: Vec<_> = std::iter::once(&self.session)
            .chain(&self.sessions)
            .filter(|l| l.rebase.is_some())
            .map(|l| l.id)
            .collect();
        if !rebasing.is_empty() {
            return self.rebase_choose(&rebasing, go_on);
        }
        let roots: Vec<_> =
            std::iter::once(&self.session).chain(&self.sessions).map(|l| (l.id, l.root.clone())).collect();
        self.events.shared_jobs().spawn(move || {
            let found: Vec<_> =
                roots.into_iter().filter_map(|(id, r)| under_way(&r).map(|s| (id, s))).collect();
            move |ed: &mut Editor| {
                let ids: Vec<_> = found.iter().map(|f| f.0).collect();
                for (id, stop) in found {
                    ed.with_session(id, |ed| {
                        if ed.session.rebase.is_none() {
                            ed.rebase_adopt(stop);
                        }
                    });
                }
                if let Err(e) = ed.rebase_choose(&ids, go_on) {
                    ed.set_error(e);
                }
            }
        });
        Ok(())
    }

    /// Of the sessions rebasing (`ids`), the one to go on with or abort: the front one, the only one, or
    /// the user's pick.
    fn rebase_choose(&mut self, ids: &[crate::event::SessionId], go_on: bool) -> Result<(), String> {
        let front = self.session.id;
        match ids {
            [] if self.sessions.is_empty() => Err(format!("no rebase under way in {}", self.session.label())),
            [] => Err("no rebase under way in any session".into()),
            _ if ids.contains(&front) => self.rebase_in(front, go_on),
            [id] => self.rebase_in(*id, go_on),
            _ => {
                let cmd = if go_on { "rebase-continue" } else { "rebase-abort" };
                let items = self
                    .sessions_in_order()
                    .into_iter()
                    .filter(|l| ids.contains(&l.id))
                    .map(|l| crate::picker::Item {
                        label: l.label().to_string(),
                        hint: l
                            .rebase
                            .as_ref()
                            .map(|r| format!("{} onto {}", r.said(), r.base))
                            .unwrap_or_default(),
                        action: crate::picker::Action::Typed(format!("{cmd} {}", l.id)),
                        glyph: None,
                    })
                    .collect();
                let mut p = crate::picker::Picker::new(format!("{cmd}: which session?"), items, false)
                    .without_preview();
                p.compact = true;
                self.open_picker(p, None);
                Ok(())
            }
        }
    }

    /// Session `id`'s rebase gone on with (`go_on`) or aborted — in its own task panel; from behind, said.
    fn rebase_in(&mut self, id: crate::event::SessionId, go_on: bool) -> Result<(), String> {
        let mut done = Ok(());
        let mut name = String::new();
        self.with_session(id, |ed| {
            name = ed.session.label().to_string();
            done = if go_on { ed.rebase_continue() } else { ed.rebase_abort() };
        });
        if done.is_ok() && id != self.session.id {
            let what = if go_on { "goes on" } else { "is aborted" };
            self.set_status(format!("{name}'s rebase {what} — its task panel shows it (space W goes there)"));
        }
        done
    }

    /// The resolved card's, a session's rebase (wherever it is): `y` continue · `X` abort.
    pub fn rebase_answer(&mut self, root: &Path, go_on: bool) {
        let Some(id) = self.session_at(root) else { return };
        self.with_session(id, |ed| {
            let done = if go_on { ed.rebase_continue() } else { ed.rebase_abort() };
            if let Err(e) = done {
                ed.set_error(e);
            }
        });
    }

    /// `:rebase-continue` — the stopped rebase's files, with no markers left, staged with what else the
    /// resolution changed (its review showed them — `Rebase.also`) and `git rebase --continue` (it may stop
    /// again). Anything else changed in the checkout isn't staged unseen: named, and the next
    /// `:rebase-continue` stages it (#129: left unstaged, git refused to go on). A rebase tarae didn't start
    /// is taken up first.
    pub fn rebase_continue(&mut self) -> Result<(), String> {
        let name = self.session.label().to_string();
        let Some(r) = self.session.rebase.as_ref() else {
            return self.rebase_find(true);
        };
        if let Stage::Running { .. } = r.stage {
            return Err(r.why(&name));
        }
        self.rebase_free()?;
        let mut files = r.stop.files.clone();
        files.extend(r.also.iter().cloned());
        let texts = self.rebase_texts(&r.stop.files);
        let root = self.session.root.clone();
        self.events.jobs().spawn(move || {
            let left = marked(&root, texts);
            let unseen: Vec<String> = unstaged(&root).into_iter().filter(|f| !files.contains(f)).collect();
            move |ed: &mut Editor| {
                let Some(r) = ed.session.rebase.as_mut().filter(|r| r.stopped()) else { return };
                if !left.is_empty() {
                    return ed
                        .set_warning(format!("conflict markers still in {} — not going on", named(&left)));
                }
                if !unseen.is_empty() {
                    let msg = format!(
                        "not going on — {} changed too and {} unstaged: look at {} (space g), then \
                         :rebase-continue stages {} with the rest",
                        named(&unseen),
                        if unseen.len() == 1 { "is" } else { "are" },
                        if unseen.len() == 1 { "it" } else { "them" },
                        if unseen.len() == 1 { "it" } else { "them" },
                    );
                    r.also.extend(unseen);
                    return ed.set_warning(msg);
                }
                ed.rebase_review_gone();
                ed.rebase_cards_gone();
                let quoted: Vec<String> = files.iter().map(|f| crate::packs::sh_quote(f)).collect();
                let add = match quoted.is_empty() {
                    true => String::new(),
                    false => format!("{GIT} add -A -- {} && ", quoted.join(" ")),
                };
                // (no editor for the replayed commit's message: it's kept as it was)
                let cmd = format!("{add}GIT_EDITOR=true {GIT} rebase --continue");
                if let Err(e) = ed.rebase_git("rebase --continue", cmd, false) {
                    ed.set_error(format!("rebase: {e}"));
                }
            }
        });
        Ok(())
    }

    /// `:rebase-abort` — `git rebase --abort`: the branch back where it was before the rebase.
    pub fn rebase_abort(&mut self) -> Result<(), String> {
        let name = self.session.label().to_string();
        let Some(r) = self.session.rebase.as_ref() else {
            return self.rebase_find(false);
        };
        if let Stage::Running { .. } = r.stage {
            return Err(r.why(&name));
        }
        self.rebase_free()?;
        self.rebase_review_gone();
        self.rebase_cards_gone();
        self.rebase_git("rebase --abort", format!("{GIT} rebase --abort"), true)
    }

    /// `:rebase-continue`/`:rebase-abort` with no rebase of tarae's here: one under way in the checkout
    /// (from before a restart, or started by hand) is taken up, then gone on with (`go_on`) or aborted.
    fn rebase_find(&mut self, go_on: bool) -> Result<(), String> {
        let root = self.session.root.clone();
        self.events.jobs().spawn(move || {
            let stop = under_way(&root);
            move |ed: &mut Editor| {
                let Some(stop) = stop else {
                    let name = ed.session.label().to_string();
                    return ed.set_error(format!("no rebase under way in {name}"));
                };
                if ed.session.rebase.is_none() {
                    ed.rebase_adopt(stop);
                }
                let done = if go_on { ed.rebase_continue() } else { ed.rebase_abort() };
                if let Err(e) = done {
                    ed.set_error(e);
                }
            }
        });
        Ok(())
    }

    /// A rebase found under way in the front session's checkout, taken up as stopped there.
    pub(crate) fn rebase_adopt(&mut self, stop: Stop) {
        let s = &self.session;
        let branch = stop.head.clone().or_else(|| s.branch.clone()).unwrap_or_else(|| "HEAD".into());
        let base = s.base.clone().or_else(|| stop.onto.clone()).unwrap_or_default();
        self.session.rebase = Some(Rebase {
            branch,
            base,
            stage: Stage::Stopped,
            step: stop.step,
            stop,
            asked: false,
            waiting: None,
            also: Vec::new(),
        });
    }

    /// The review of a stop's resolution ends (the rebase goes on, or is aborted).
    fn rebase_review_gone(&mut self) {
        let replay = self
            .session
            .code_review
            .as_ref()
            .is_some_and(|r| matches!(r.base, crate::codereview::Base::Replay { .. }));
        if replay {
            crate::codereview::close(self);
        }
    }

    /// The resolved card of the front session's rebase goes (answered by hand).
    fn rebase_cards_gone(&mut self) {
        let root = self.session.root.clone();
        let mine = |o: &Offer| matches!(&o.what, What::RebaseResolved { root: r } if *r == root);
        self.offers.retain(|o| !mine(o));
        self.session.parked.retain(|o| !mine(o));
    }

    /// `y` on the bring card.
    pub fn rebase_bring(&mut self, m: Moved) {
        let Some(id) = self.session_at(&m.root) else { return };
        if let Err(e) = self.session_bring_now(&id.to_string()) {
            self.set_error(e);
        }
    }

    /// `y` on the move-up card: its branch fast-forwarded to its base's tip, in its checkout (a worker).
    pub fn rebase_move_up(&mut self, id: u64, m: Moved) {
        self.offer_finished(id, Ok(()));
        let unsaved: Vec<PathBuf> =
            self.docs.iter().filter(|d| d.is_modified()).filter_map(|d| d.path.clone()).collect();
        let said = format!("Moving {} up to {}…", m.name, m.base);
        let work = move || {
            let r = move_up(&m, &unsaved);
            (m, r)
        };
        self.step(self.events.shared_jobs(), "session", said, work, |ed, (m, r)| {
            match r {
                Ok(tip) => ed.set_success(format!("Moved {} up to {} — now at {tip}", m.name, m.base)),
                Err(e) => ed.set_error(format!("move {} up: {e}", m.name)),
            }
            ed.git_refresh();
            ed.git_reload_bases();
        });
    }

    /// `p` on the bring card: the quick preflight in its session, and the bring once it passes (the full one
    /// is for before publishing).
    pub fn rebase_preflight_then_bring(&mut self, m: Moved) {
        let Some(id) = self.session_at(&m.root) else { return };
        self.with_session(id, |ed| {
            ed.session.bring_after_preflight = true;
            if let Err(e) = ed.preflight_start(crate::preflight::Set::Quick) {
                ed.session.bring_after_preflight = false;
                ed.set_error(e);
            }
        });
    }

    /// The preflight (`title`: which set) ended, `passed` or not: a bring waiting for it goes on, or doesn't.
    pub(crate) fn rebase_preflight_done(&mut self, passed: bool, title: &str) {
        if !std::mem::take(&mut self.session.bring_after_preflight) {
            return;
        }
        let (id, name) = (self.session.id, self.session.label().to_string());
        if !passed {
            return self.set_warning(format!("{name} not brought — the {title} didn't pass"));
        }
        if let Err(e) = self.session_bring_now(&id.to_string()) {
            self.set_error(e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stop_says_where_it_is() {
        let mut s = Stop { step: Some((2, 5)), ..Default::default() };
        assert_eq!(s.at(), "commit 2 of 5");
        s.commit = Some(("f".repeat(40), "fffffff".into(), "Add login".into()));
        assert_eq!(s.at(), "commit 2 of 5 (fffffff Add login)");
        s.step = None;
        assert_eq!(s.at(), "fffffff Add login");
        let files: Vec<String> = (1..=6).map(|i| format!("f{i}")).collect();
        assert_eq!(named(&files[..2]), "f1, f2");
        assert_eq!(named(&files), "f1, f2, f3, f4 and 2 more");
        let r = Rebase {
            branch: "b".into(),
            base: "main".into(),
            stage: Stage::Stopped,
            step: Some((2, 5)),
            stop: Stop::default(),
            asked: false,
            waiting: None,
            also: Vec::new(),
        };
        assert_eq!(r.said(), "rebasing 2/5");
        assert_eq!(r.why("feat"), "feat is rebasing 2/5 — :rebase-continue or :rebase-abort first");
        assert!(cut("é".repeat(10), 5).starts_with("éé\n… (16 more bytes"));
    }

    /// #175: `:rebase-continue` and `:rebase-abort` go to the session rebasing, not the one in front — the
    /// front one when it is, the only one, or the user's pick of several; a session named goes first.
    #[test]
    fn by_hand_it_goes_to_the_session_rebasing() {
        let dir = std::env::temp_dir().join(format!("tarae-rebase-which-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        for p in ["front", "a", "b"] {
            std::fs::create_dir_all(dir.join(p)).unwrap();
        }
        let dir = std::fs::canonicalize(dir).unwrap();
        let mut ed = Editor::new(crate::config::Config::default());
        (ed.session.root, ed.session.name) = (dir.join("front"), "front".into());
        let a = ed.session_open(&dir.join("a")).unwrap();
        let b = ed.session_open(&dir.join("b")).unwrap();
        assert!(ed.session_switch(0));
        // (git at work in each: the answer says so — nothing runs)
        let running = |base: &str| Rebase {
            branch: "x".into(),
            base: base.into(),
            stage: Stage::Running { generation: 0, aborting: false },
            step: Some((1, 3)),
            stop: Stop::default(),
            asked: false,
            waiting: None,
            also: Vec::new(),
        };
        ed.with_session(a, |ed| ed.session.rebase = Some(running("main")));
        let label = |ed: &mut Editor, id| {
            let mut n = String::new();
            ed.with_session(id, |ed| n = ed.session.label().to_string());
            n
        };
        let name_a = label(&mut ed, a);
        // One, behind: that one
        assert_eq!(ed.rebase_by_hand("", true), Err(format!("{name_a} is rebasing 1/3 — let it finish")));
        // Two, behind: asked which
        ed.with_session(b, |ed| ed.session.rebase = Some(running("dev")));
        assert_eq!(ed.rebase_by_hand("", false), Ok(()));
        let p = ed.picker.take().expect("a pick");
        assert_eq!(p.title, "rebase-abort: which session?");
        let rows: Vec<_> = p.items().iter().map(|i| (i.hint.as_str(), i.action.clone())).collect();
        assert_eq!(
            rows,
            [
                ("rebasing 1/3 onto main", crate::picker::Action::Typed(format!("rebase-abort {a}"))),
                ("rebasing 1/3 onto dev", crate::picker::Action::Typed(format!("rebase-abort {b}"))),
            ]
        );
        // Named: that one
        let name_b = label(&mut ed, b);
        assert_eq!(
            ed.rebase_by_hand(&b.to_string(), true),
            Err(format!("{name_b} is rebasing 1/3 — let it finish"))
        );
        // The front one rebasing too: the front's
        ed.session.rebase = Some(running("main"));
        assert_eq!(ed.rebase_by_hand("", true), Err("front is rebasing 1/3 — let it finish".to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }
}
