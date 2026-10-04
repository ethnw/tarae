//! `:restart` — a new tarae (after `cargo install`, or tarae building tarae). Build if
//! `editor.restart-build` says how, check the binary (`editor.restart-binary`, else the running one's
//! path) answers `--version`, then become it: `exec` keeps the pid, the terminal and the zellij pane, and
//! the new tarae comes back with what was on screen — the open files with their selections and scroll, the panes, the
//! side panel, the chat (its claude conversation resumed, the words being typed) and the `:set`s.
//!
//! The state goes through one file (`restart-<pid>.json` in the state dir → `--restart <file>`), written
//! once the loop has ended and read (and removed) at startup.
//!
//! Files are saved first, as before a task. What can't come along stops it the way `:q` is stopped — a
//! buffer with no file, an open review, a code review. A chat turn in flight is waited for (`turn_over`);
//! `:restart` again cancels.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::editor::{Editor, View};
use crate::selection::{Range, Selection};
use crate::sidebar::Mode;
use crate::split::{Dir, Split};

/// How long the new binary gets to answer `--version`.
const CHECK_WAIT: Duration = Duration::from_secs(5);

#[derive(Debug, PartialEq, Eq)]
pub enum Stage {
    /// Every session's chat turn ends first (#109 — another session's was cut, silently).
    Turn,
    /// Claude's accepted edits land on disk first (`claudes_writes`).
    Writes,
    /// The files are saved, and written (`saved_then`).
    Saving,
    /// The checkouts it could build in and their binaries are looked up (a worker).
    Finding,
    /// Its card says which checkout and commit — waiting for the answer (#108).
    Choosing,
    /// The build runs as a task (its generation).
    Building(u64),
    /// The binary didn't change though its checkout's HEAD is newer: its card says so.
    Same,
    /// The binary answers `--version` (a worker).
    Checking,
    /// The loop ends; main runs it.
    Ready(PathBuf),
}

pub struct Restart {
    /// Which `:restart` — a worker's answer for an earlier (canceled) one is dropped.
    id: u64,
    /// `:restart` builds, `:restart!` doesn't (nor after a wait that followed the build).
    build: bool,
    pub stage: Stage,
    /// What it builds in and runs, once the card's answered.
    target: Option<Target>,
}

/// A checkout a restart can build in and run (#108): whose it is (its session's label), where its build
/// runs (where `.tarae.toml` is), the binary it runs, its HEAD (short) and when that was committed, and when
/// the binary was last written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub label: String,
    pub root: PathBuf,
    pub bin: PathBuf,
    pub head: Option<String>,
    head_time: Option<i64>,
    bin_time: Option<std::time::SystemTime>,
}

impl Target {
    /// `main (1085db8)`.
    pub fn named(&self) -> String {
        match &self.head {
            Some(h) => format!("{} ({h})", self.label),
            None => self.label.clone(),
        }
    }

    /// Whether its binary was written before its HEAD was committed — it can't have HEAD in it.
    fn older_than_head(&self) -> bool {
        let secs = |t: std::time::SystemTime| {
            t.duration_since(std::time::UNIX_EPOCH).ok().map(|d| d.as_secs() as i64)
        };
        matches!((self.bin_time.and_then(secs), self.head_time), (Some(b), Some(h)) if b < h)
    }
}

/// What a restart's card asks: which of `targets` (the checkout tarae started in first) — `y` the first,
/// `f` the session in front's; and, after a build that left the binary as it was, whether to go on anyway.
#[derive(Clone, Debug)]
pub struct Choice {
    pub id: u64,
    pub targets: Vec<Target>,
}

impl Restart {
    /// The binary to run once the loop has ended.
    pub fn ready(&self) -> Option<&Path> {
        match &self.stage {
            Stage::Ready(bin) => Some(bin),
            _ => None,
        }
    }
}

// ── :restart ────────────────────────────────────────────────────────────────

/// `:restart` (`build`) / `:restart!`. Again while one is under way: cancels it.
pub fn start(ed: &mut Editor, build: bool) -> Result<(), String> {
    if ed.restart.take().is_some() {
        ed.set_status("restart canceled");
        return Ok(());
    }
    // (a turn's files still being read belong to it)
    let in_turn =
        ed.session.chat.as_ref().is_some_and(crate::chat::Chat::busy) || ed.session.checkpoints.gathering;
    blocker(ed, in_turn).map_or(Ok(()), |why| Err(format!("can't restart: {why}")))?;
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    ed.restart = Some(Restart { id, build, stage: Stage::Turn, target: None });
    // Every session's turn, not only the front's (#109)
    let turns = turns(ed);
    if !turns.is_empty() {
        ed.set_status(format!("tarae restarts when {} — :restart again cancels", waits_for(&turns)));
        return Ok(());
    }
    go(ed);
    Ok(())
}

/// The sessions whose Claude is in a turn, or whose turn's files are still being read — by name.
fn turns(ed: &Editor) -> Vec<String> {
    std::iter::once(&ed.session)
        .chain(&ed.sessions)
        .filter(|l| l.chat.as_ref().is_some_and(crate::chat::Chat::busy) || l.checkpoints.gathering)
        .map(|l| l.label().to_string())
        .collect()
}

/// What a restart waits for, said: `Claude's turn ends in main` · `Claude's turns end in main, fix-login`.
fn waits_for(turns: &[String]) -> String {
    match turns {
        [one] => format!("Claude's turn ends in {one}"),
        many => format!("Claude's turns end in {}", many.join(", ")),
    }
}

/// A chat's turn ended (or the chat closed, or the turn's files were read): a restart waiting for every
/// session's goes on — from the front (`sync`), not while a session behind is swapped in to apply it.
pub fn turn_over(ed: &mut Editor) {
    if !ed.background() {
        sync(ed);
    }
}

/// Every event: a restart waiting for turns goes on once none is under way in any session.
pub fn sync(ed: &mut Editor) {
    if ed.restart.as_ref().is_some_and(|r| r.stage == Stage::Turn) && turns(ed).is_empty() {
        go(ed);
    }
}

/// What keeps tarae from restarting now, in words. Files that can be saved don't — `go` saves them. While
/// Claude's turn is in flight (`in_turn`), what belongs to it — its edit's review, its command, its tests —
/// doesn't either: the turn is waited for. A shell open in the terminal doesn't: it's started again by hand.
fn blocker(ed: &Editor, in_turn: bool) -> Option<String> {
    let stuck: Vec<String> = ed
        .docs
        .iter()
        .filter(|d| d.is_modified() && !d.throwaway && (d.path.is_none() || d.disk_conflict))
        .map(|d| d.display_name(&ed.session.root))
        .collect();
    if !stuck.is_empty() {
        return Some(format!("unsaved changes in {} — save or close them first", stuck.join(", ")));
    }
    // A session's rebase under way: tarae's part of it (the review, the card, Claude's ask) wouldn't come back
    let rebasing = std::iter::once(&ed.session)
        .chain(&ed.sessions)
        .find_map(|l| Some(l.rebase.as_ref()?.why(l.label())));
    if rebasing.is_some() {
        return rebasing;
    }
    let claudes = |yes: bool| in_turn && yes;
    if ed.session.review.is_some()
        && !claudes(ed.session.chat.as_ref().is_some_and(|c| c.edits.review.is_some()))
    {
        return Some("a review is open — answer it first".into());
    }
    if crate::codereview::drafting(ed) {
        return Some("a code review is open — send it (space v s) or end it (space v q) first".into());
    }
    // (saved, it would come back as a plain file — `:wq` no longer committing)
    if ed.session.committing.is_some() {
        return Some("a commit message is open — :wq commits it, :q drops it".into());
    }
    let running = crate::task::State::Running;
    if let Some(r) = ed.session.task_run.as_ref().filter(|r| r.state == running && !claudes(r.by_claude)) {
        return Some(match r.install {
            true => format!("{} is installing — let it finish", r.task.name),
            false => format!("{} is running — let it finish, or stop it (space m c)", r.task.name),
        });
    }
    let testing = crate::testing::RunState::Running;
    if ed.session.test_run.as_ref().is_some_and(|r| r.state == testing && !claudes(r.chat.is_some())) {
        return Some("tests are running — let them finish, or stop them (space x c)".into());
    }
    if ed.dap.as_ref().is_some_and(|d| !matches!(d.state, crate::dap::State::Exited(_))) {
        return Some("a debug run is on — end it first (space G t)".into());
    }
    None
}

/// The latest end of a wait for Claude's write of an accepted edit (`Document::agent_will_write`) — those
/// files are left to Claude by auto-save, so until the watch sees the write they'd count as unsaved.
fn pending_writes(ed: &Editor) -> Option<Instant> {
    let now = Instant::now();
    ed.docs
        .iter()
        .filter(|d| d.is_modified())
        .filter_map(|d| Some(d.agent_writes?.0))
        .filter(|&u| u > now)
        .max()
}

/// Claude's edits are still landing: looked at again shortly (the wait ends on the write, or after
/// `AGENT_WRITE_WAIT` when auto-save writes them itself). True if the restart waits.
fn claudes_writes(ed: &mut Editor) -> bool {
    let Some(until) = pending_writes(ed) else { return false };
    let Some(r) = ed.restart.as_mut() else { return false };
    let first = r.stage != Stage::Writes;
    r.stage = Stage::Writes;
    let id = r.id;
    if first {
        ed.set_status("tarae restarts once Claude's edits are on disk");
    }
    let nap = until.saturating_duration_since(Instant::now()).min(Duration::from_millis(100));
    ed.events.jobs().spawn(move || {
        std::thread::sleep(nap);
        move |ed: &mut Editor| {
            if !at(ed, id, &Stage::Writes) {
                return;
            }
            // (asked something meanwhile: after that turn)
            if !turns(ed).is_empty() {
                if let Some(r) = ed.restart.as_mut() {
                    r.stage = Stage::Turn;
                }
                return;
            }
            go(ed);
        }
    });
    true
}

/// Files saved, then — once they're written (`after_saves`: the saver's event, not a wait; a slow disk says
/// so meanwhile) and nothing's in the way — `next`. Else the restart is called off, with why.
fn saved_then(ed: &mut Editor, id: u64, next: impl FnOnce(&mut Editor) + 'static) {
    let Some(r) = ed.restart.as_mut() else { return };
    r.stage = Stage::Saving;
    ed.auto_save(" · before restarting");
    // (`:restart` again meanwhile: canceled, or another one's)
    ed.after_saves(move |ed| {
        if at(ed, id, &Stage::Saving) && clear_to_go(ed) {
            next(ed);
        }
    });
}

/// Saved, nothing in the way? Else the restart is called off, with why.
fn clear_to_go(ed: &mut Editor) -> bool {
    let unsaved: Vec<String> = ed
        .docs
        .iter()
        .filter(|d| d.is_modified() && !d.throwaway)
        .map(|d| match d.path.as_deref().and_then(|p| crate::disk::unparsed_config(p, &d.text)) {
            // (held back by auto-save, #19)
            Some(_) => {
                format!("{} (it doesn't parse — :w writes it anyway)", d.display_name(&ed.session.root))
            }
            None => d.display_name(&ed.session.root),
        })
        .collect();
    let why = match blocker(ed, false) {
        Some(why) => why,
        None if !unsaved.is_empty() => format!("couldn't save {}", unsaved.join(", ")),
        None => return true,
    };
    ed.restart = None;
    ed.set_error(format!("not restarting: {why}"));
    false
}

/// On from the wait: saved, the checkouts it could build in looked up — the one tarae started in, and the
/// session in front's when that's another (#108: it built in the front's, an old session's, silently) —
/// then its card says which and at what commit.
fn go(ed: &mut Editor) {
    if claudes_writes(ed) {
        return;
    }
    let Some(id) = ed.restart.as_ref().map(|r| r.id) else { return };
    saved_then(ed, id, move |ed| find(ed, id));
}

/// The checkouts it could build in looked up (a worker), then its card — or, chosen and built already
/// before a wait, the check right away.
fn find(ed: &mut Editor, id: u64) {
    let Some(r) = ed.restart.as_mut() else { return };
    // (on after a wait that came once it was chosen and built: as answered)
    if let Some(t) = r.target.clone() {
        return check(ed, t.bin, true);
    }
    r.stage = Stage::Finding;
    let binary = ed.config.restart_binary.clone();
    let first = std::iter::once(&ed.session).chain(&ed.sessions).find(|l| l.id == 0).unwrap_or(&ed.session);
    let mut places = vec![(first.label().to_string(), first.root.clone())];
    if ed.session.root != first.root {
        places.push((ed.session.label().to_string(), ed.session.root.clone()));
    }
    ed.events.jobs().spawn(move || {
        let targets: Result<Vec<Target>, String> =
            places.into_iter().map(|(label, root)| target(label, &root, &binary)).collect();
        move |ed: &mut Editor| {
            if !at(ed, id, &Stage::Finding) {
                return;
            }
            match targets {
                Err(e) => cancel(ed, e),
                Ok(targets) => ask(ed, id, targets),
            }
        }
    });
}

/// What a restart in the checkout at `root` builds in and runs, and their commit and times (on a worker).
fn target(label: String, root: &Path, binary: &str) -> Result<Target, String> {
    let root = project_root(root);
    let bin = resolve(binary, &root)?;
    let said = crate::gitmenu::git(&root, &["log", "-1", "--format=%h %ct"], None).unwrap_or_default();
    let (head, head_time) = match said.trim().split_once(' ') {
        Some((h, t)) => (Some(h.to_string()), t.parse().ok()),
        None => (None, None),
    };
    let bin_time = std::fs::metadata(&bin).and_then(|m| m.modified()).ok();
    Ok(Target { label, root, bin, head, head_time, bin_time })
}

pub const TITLE: &str = "Restart into";

/// The card: which checkout it builds in, at what commit, and the binary it becomes — `y` the one tarae
/// started in, `f` the session in front's (when it's another), `n` none.
fn ask(ed: &mut Editor, id: u64, targets: Vec<Target>) {
    let Some(r) = ed.restart.as_mut() else { return };
    r.stage = Stage::Choosing;
    let build = Some(ed.config.restart_build.trim()).filter(|c| r.build && !c.is_empty());
    let tilde = |p: &Path| crate::allow::tilde(p);
    // (the folder once; the binary from it when it's inside)
    let bin = |t: &Target| match t.bin.strip_prefix(&t.root) {
        Ok(rel) => format!("{} there", rel.display()),
        Err(_) => tilde(&t.bin),
    };
    let says = |t: &Target| match build {
        Some(cmd) => format!("`{cmd}` in\n  {}\nthen runs {}", tilde(&t.root), bin(t)),
        None => format!("Runs {} as it is — no build (:restart builds)", tilde(&t.bin)),
    };
    let t = &targets[0];
    let what = if build.is_some() { "build" } else { "binary" };
    let title = format!(
        "{TITLE} {}'s {what}{}?",
        t.label,
        t.head.as_ref().map(|h| format!(" ({h})")).unwrap_or_default()
    );
    let mut q = says(t);
    if build.is_none() && t.older_than_head() {
        q.push_str(&format!("\n\nIt was written before {} was committed — it doesn't have it.", t.named()));
    }
    if let Some(o) = targets.get(1) {
        q.push_str(&format!(
            "\n\nf — {}'s {what} instead: {}",
            o.named(),
            says(o).replace("\n  ", " ").replace('\n', " ")
        ));
    }
    let what = crate::offer::What::Restart(Box::new(Choice { id, targets }));
    ed.push_offer(crate::offer::Offer::new(title, &q, "Restarting…", "Not restarting".into(), what));
}

/// The answer on its card: build in (or run) `targets[which]` — None calls it off.
pub fn chosen(ed: &mut Editor, card: u64, choice: Choice, which: Option<usize>) {
    ed.offer_finished(card, Ok(()));
    if !at(ed, choice.id, &Stage::Choosing) {
        return;
    }
    let Some(t) = which.and_then(|i| choice.targets.into_iter().nth(i)) else {
        ed.restart = None;
        return ed.set_status("not restarting");
    };
    let r = ed.restart.as_mut().expect("at its stage");
    let build = Some(ed.config.restart_build.trim().to_string()).filter(|c| r.build && !c.is_empty());
    r.target = Some(t.clone());
    // (no build: its card said whether the binary is older than HEAD)
    let Some(cmd) = build else { return check(ed, t.bin, true) };
    let task = crate::task::Task { name: "restart build".into(), cmd, cwd: t.root.clone() };
    let Some(generation) = ed.task_start_restart(task) else {
        return cancel(ed, "the build didn't start".into());
    };
    if let Some(r) = ed.restart.as_mut() {
        r.stage = Stage::Building(generation);
    }
    ed.set_status(format!("building {} — tarae restarts when it's done (:restart again cancels)", t.named()));
}

/// The build task ended (`task.rs`) — true if it was this restart's (then the restart says what's next).
pub fn built(ed: &mut Editor, generation: u64, passed: bool, errors: usize) -> bool {
    let Some(r) = ed.restart.as_mut().filter(|r| r.stage == Stage::Building(generation)) else {
        return false;
    };
    match (passed, r.target.as_ref().map(|t| t.bin.clone())) {
        (true, Some(bin)) => check(ed, bin, false),
        _ => {
            ed.restart = None;
            let next = if errors > 0 { "]q goes to the first error" } else { "the panel has why" };
            ed.set_error(format!("build failed — not restarting; {next}"));
        }
    }
    true
}

/// The card for a binary the build left as it was though its checkout's HEAD is newer: `y` restarts onto
/// it anyway, `n` doesn't.
fn same(ed: &mut Editor, id: u64, t: &Target) {
    if let Some(r) = ed.restart.as_mut() {
        r.stage = Stage::Same;
    }
    let title = format!("{} is the binary it was", crate::allow::tilde(&t.bin));
    let q = format!(
        "It was written before {} was committed, and the build left it as it was — restarting would run the \
         same tarae.\n\neditor.restart-build and editor.restart-binary say what's built and run.",
        t.named()
    );
    let what = crate::offer::What::RestartSame { id };
    ed.push_offer(crate::offer::Offer::new(title, &q, "Restarting…", "Not restarting".into(), what));
}

/// The answer on that card: restart onto it anyway (`y`), or not.
pub fn same_answer(ed: &mut Editor, card: u64, id: u64, yes: bool) {
    ed.offer_finished(card, Ok(()));
    if !at(ed, id, &Stage::Same) {
        return;
    }
    let bin = ed.restart.as_ref().and_then(|r| r.target.as_ref()).map(|t| t.bin.clone());
    match (yes, bin) {
        (true, Some(bin)) => check(ed, bin, true),
        _ => {
            ed.restart = None;
            ed.set_status("not restarting");
        }
    }
}

/// The task panel's run was stopped or replaced: a build being waited for won't finish.
pub fn run_dropped(ed: &mut Editor) {
    if ed.restart.as_ref().is_some_and(|r| matches!(r.stage, Stage::Building(_))) {
        ed.restart = None;
        ed.set_warning("restart canceled — its build was stopped");
    }
}

/// The binary answers `--version` as a tarae (on a worker) — then the loop ends and main runs it. One the
/// build left as it was though its checkout's HEAD is newer asks first (`same`), unless `anyway`.
fn check(ed: &mut Editor, bin: PathBuf, anyway: bool) {
    let Some(r) = ed.restart.as_mut() else { return };
    r.stage = Stage::Checking;
    let id = r.id;
    ed.events.jobs().spawn(move || {
        let answer = answers(&bin);
        let written = std::fs::metadata(&bin).and_then(|m| m.modified()).ok();
        move |ed: &mut Editor| {
            if !at(ed, id, &Stage::Checking) {
                return;
            }
            if let Err(e) = answer {
                return cancel(ed, e);
            }
            // The same binary as before the build, and older than its HEAD: said, not run silently (#108)
            if let Some(t) = ed.restart.as_ref().and_then(|r| r.target.clone())
                && !anyway
                && written.is_some()
                && written == t.bin_time
                && t.older_than_head()
            {
                return same(ed, id, &t);
            }
            // Claude was asked something meanwhile, or its edits are still landing: on from that wait
            // (built already, the card answered)
            let turns = turns(ed);
            if !turns.is_empty() || pending_writes(ed).is_some() {
                let r = ed.restart.as_mut().expect("checked above");
                (r.stage, r.build) = (Stage::Turn, false);
                if !turns.is_empty() {
                    ed.set_status(format!("built — tarae restarts when {}", waits_for(&turns)));
                }
                return turn_over(ed);
            }
            saved_then(ed, id, move |ed| {
                if let Some(r) = ed.restart.as_mut() {
                    r.stage = Stage::Ready(bin);
                    ed.should_quit = true;
                }
            });
        }
    });
}

fn at(ed: &Editor, id: u64, stage: &Stage) -> bool {
    ed.restart.as_ref().is_some_and(|r| r.id == id && r.stage == *stage)
}

fn cancel(ed: &mut Editor, why: String) {
    ed.restart = None;
    ed.set_error(format!("not restarting: {why}"));
}

/// `bin --version` says it's a tarae, within `CHECK_WAIT` — a binary that won't run here (another
/// architecture, a broken link, the wrong file) is caught while this tarae is still whole.
fn answers(bin: &Path) -> Result<(), String> {
    use std::io::Read as _;
    use std::process::{Command, Stdio};
    let name = bin.display();
    let mut child = Command::new(bin)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("{name}: {e}"))?;
    let end = Instant::now() + CHECK_WAIT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) if Instant::now() < end => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("{name} didn't answer --version"));
            }
        }
    };
    let mut out = String::new();
    if let Some(mut o) = child.stdout.take() {
        let _ = o.read_to_string(&mut out);
    }
    if status.success() && out.starts_with("tarae") {
        Ok(())
    } else {
        Err(format!("{name} isn't a tarae that runs here"))
    }
}

/// The running binary as it was started — remembered first thing: once a build replaces it, Linux names
/// it "… (deleted)".
static EXE: OnceLock<Option<PathBuf>> = OnceLock::new();

pub fn remember_exe() {
    EXE.get_or_init(|| std::env::current_exe().ok());
}

/// `editor.restart-binary`: "" = the one running, `~/…` from home, relative from the project root.
fn resolve(binary: &str, root: &Path) -> Result<PathBuf, String> {
    let b = binary.trim();
    if b.is_empty() {
        let exe = EXE.get_or_init(|| std::env::current_exe().ok()).clone();
        let exe = exe.map(|p| match p.to_str().and_then(|s| s.strip_suffix(" (deleted)")) {
            Some(s) => PathBuf::from(s),
            None => p,
        });
        return exe.ok_or_else(|| "can't tell where this tarae is — set editor.restart-binary".into());
    }
    let p = match (b.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(b),
    };
    Ok(if p.is_absolute() { p } else { root.join(p) })
}

/// Where `.tarae.toml` is (its relative paths are from there), else the editor's root.
fn project_root(root: &Path) -> PathBuf {
    crate::config::project_path(root)
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| root.to_path_buf())
}

// ── Becoming the new one ────────────────────────────────────────────────────

/// After the loop (the terminal is back to normal, the restore file saved): the state written,
/// everything this tarae started stopped — `exec` runs no destructors, so the editor is dropped here — and
/// `bin` takes this process over. Returns only if that failed (the restore file still has the files).
pub fn exec(ed: Editor, bin: &Path) -> String {
    match crate::config::state_dir() {
        Some(dir) => exec_with(ed, bin, &dir.join(format!("restart-{}.json", std::process::id()))),
        None => "no state directory to carry what's open over".into(),
    }
}

/// `exec` with the state in `file`.
fn exec_with(mut ed: Editor, bin: &Path, file: &Path) -> String {
    use std::os::unix::process::CommandExt as _;
    crate::chat::keep_now(&ed);
    let state = snapshot(&mut ed);
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let body = state.to_string();
    if let Err(e) = crate::disk::write_atomic(file, |w| std::io::Write::write_all(w, body.as_bytes())) {
        return format!("{}: {e}", file.display());
    }
    drop(ed);
    close_on_exec();
    let err = std::process::Command::new(bin).arg("--restart").arg(file).exec();
    let _ = std::fs::remove_file(file);
    format!("couldn't run {}: {err}", bin.display())
}

/// Every descriptor past stdio closes on exec — std's are marked already, but not all are std's (a
/// terminal's pty from `openpty`); one left open would keep its shell from hanging up.
fn close_on_exec() {
    let fds: Vec<i32> = std::fs::read_dir("/dev/fd")
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.parse().ok())
        .filter(|&fd| fd > 2)
        .collect();
    for fd in fds {
        // SAFETY: setting a flag on a descriptor number; one that's gone meanwhile just fails.
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
        }
    }
}

/// First thing after a restart: the last tarae's children that were stopped but not waited for are this
/// process's now (same pid) — waited for, so they don't linger as zombies.
pub fn reap() {
    let mut status = 0;
    // SAFETY: waitpid with WNOHANG on any child; nothing else has been started yet.
    while unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) } > 0 {}
}

// ── What comes along ────────────────────────────────────────────────────────

/// What was on screen, as the next tarae reads it. Documents without a file (scratch, a review's deleted
/// file, `jdt://` classes) stay behind — panes showing one show the current file — and so do files outside
/// the project Claude's turn opened that you never took up (`Document::aside`, #37).
pub fn snapshot(ed: &mut Editor) -> Value {
    ed.sync_focus_view();
    let docs: Vec<&crate::document::Document> =
        ed.docs.iter().filter(|d| d.path.is_some() && d.virtual_uri.is_none() && !d.aside).collect();
    let index = |id| docs.iter().position(|d| d.id == id);
    let files: Vec<Value> = docs
        .iter()
        .map(|d| {
            let sel = d.selection();
            let ranges: Vec<[usize; 2]> = sel.ranges().iter().map(|r| [r.anchor, r.head]).collect();
            json!({ "path": d.path, "sel": ranges, "primary": sel.primary_index(), "top": d.top, "left": d.left })
        })
        .collect();
    let views: Vec<Value> = ed
        .session
        .views
        .iter()
        .map(|v| json!({ "id": v.id, "doc": index(v.doc), "top": v.top, "left": v.left }))
        .collect();
    let sidebar = ed
        .session
        .sidebar
        .as_ref()
        .and_then(|s| Some(json!({ "mode": mode_name(s.mode)?, "focus": s.focus })));
    let chat = ed.session.chat.as_ref().map(|c| {
        json!({ "conversation": c.conversation, "input": c.input, "cursor": c.cursor, "focused": c.focused,
                "scroll": c.scroll, "follow": c.follow.on, "at_end": c.edits.at_end })
    });
    let overrides: Vec<Value> = ed.overrides.iter().map(|(p, v)| json!([p, v.to_string()])).collect();
    json!({
        "light": crate::theme::is_light(),
        "docs": files,
        "current": index(ed.doc().id),
        "views": views,
        "split": split_json(&ed.session.split),
        "focus": ed.session.focus,
        "sidebar": sidebar,
        "chat": chat,
        "overrides": overrides,
        // (every session, and which is in front; its files are the ones above)
        "sessions": crate::worktree::snapshot(ed),
        // (the review's viewed marks, each session's)
        "viewed": crate::codereview::viewed_snapshot(ed),
        // (its shell can't come along — the next tarae says so)
        "terminal": ed.session.terminal.as_ref().is_some_and(|t| t.exited.is_none()),
    })
}

/// Back as it was (`snapshot`) — the files that are still there — and a toast that says so. Number of
/// files opened (none: the start screen, as a plain start would show).
pub fn restore(ed: &mut Editor, v: &Value) -> usize {
    let n = restore_quietly(ed, v);
    let mut said = match n {
        0 => "Restarted".to_string(),
        1 => "Restarted · 1 file as it was".to_string(),
        n => format!("Restarted · {n} files as they were"),
    };
    if v["terminal"].as_bool() == Some(true) {
        said.push_str(" · the terminal starts afresh (space T)");
    }
    ed.set_success(said);
    n
}

fn restore_quietly(ed: &mut Editor, v: &Value) -> usize {
    // `:set`s first — some decide how files open (soft wrap, language servers)
    let mut set = false;
    for o in v["overrides"].as_array().into_iter().flatten() {
        let (Some(path), Some(raw)) = (o[0].as_str(), o[1].as_str()) else { continue };
        let value = crate::settings::parse_value(raw);
        if crate::settings::apply(&mut ed.config, path, &value).is_ok() {
            ed.overrides.push((path.to_string(), value));
            set = true;
        }
    }
    if set {
        ed.ensure_theme();
    }
    // The sessions, the one in front in front — the files below open in it
    crate::worktree::restore(ed, &v["sessions"]);
    crate::codereview::viewed_restore(ed, &v["viewed"]);
    // Saved index → the document it is now
    let mut ids = Vec::new();
    for d in v["docs"].as_array().into_iter().flatten() {
        let opened =
            d["path"].as_str().filter(|p| Path::new(p).is_file()).and_then(|p| ed.open(Path::new(p)).ok());
        ids.push(opened.map(|()| {
            let doc = ed.doc_mut();
            // (a big file still loading gets its spot from the restore file when its text is in)
            if !doc.loading {
                let ranges: Vec<Range> = d["sel"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|r| {
                        let at =
                            |i: usize| r[i].as_u64().map(|n| crate::graphemes::snap(&doc.text, n as usize));
                        Some(Range::new(at(0)?, at(1)?))
                    })
                    .collect();
                if !ranges.is_empty() {
                    let primary = d["primary"].as_u64().unwrap_or(0) as usize;
                    doc.set_selection(Selection::new(ranges, primary));
                }
                doc.top = num(&d["top"]).min(doc.text.len_lines().saturating_sub(1));
                doc.left = num(&d["left"]);
            }
            doc.id
        }));
    }
    let opened = ids.iter().flatten().count();
    if opened > 0 {
        layout(ed, v, &ids);
    }
    let sidebar = &v["sidebar"];
    if let Some(mode) = sidebar["mode"].as_str().and_then(mode_of) {
        ed.sidebar_show(mode);
        if let Some(s) = ed.session.sidebar.as_mut() {
            s.focus = sidebar["focus"].as_bool().unwrap_or(false);
        }
    }
    let chat = &v["chat"];
    if chat.is_object() {
        crate::chat::restore(
            ed,
            crate::chat::Restored {
                // (an earlier build's state calls it "session")
                conversation: chat["conversation"].as_str().or(chat["session"].as_str()).map(str::to_string),
                input: chat["input"].as_str().unwrap_or_default().to_string(),
                cursor: num(&chat["cursor"]),
                focused: chat["focused"].as_bool().unwrap_or(false),
                scroll: num(&chat["scroll"]),
                follow: chat["follow"].as_bool().unwrap_or(ed.config.llm.follow),
                at_end: chat["at_end"].as_str().map(PathBuf::from),
            },
        );
    }
    opened
}

/// The current file and the panes — `ids` = the document each saved one is now.
fn layout(ed: &mut Editor, v: &Value, ids: &[Option<crate::document::DocId>]) {
    let doc_at = |i: &Value| i.as_u64().and_then(|i| *ids.get(i as usize)?);
    if let Some(id) = doc_at(&v["current"]).or_else(|| ids.iter().flatten().next().copied())
        && let Some(i) = ed.docs.iter().position(|d| d.id == id)
    {
        ed.current = i;
    }
    // The panes, if they still make a tree of the ones saved
    let now = ed.doc().id;
    let views: Vec<View> = v["views"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|w| {
            let mut view = View::new(w["id"].as_u64()?, doc_at(&w["doc"]).unwrap_or(now));
            (view.top, view.left) = (num(&w["top"]), num(&w["left"]));
            Some(view)
        })
        .collect();
    let split = split_of(&v["split"]);
    let focus = v["focus"].as_u64();
    if let (Some(split), Some(focus)) = (split, focus) {
        let mut leaves = split.leaves();
        leaves.sort_unstable();
        let mut ids: Vec<_> = views.iter().map(|w| w.id).collect();
        ids.sort_unstable();
        if leaves == ids
            && let Some(f) = views.iter().find(|w| w.id == focus)
            && let Some(i) = ed.docs.iter().position(|d| d.id == f.doc)
        {
            // The focused pane is the current document at its scroll
            ed.current = i;
            (ed.docs[i].top, ed.docs[i].left) = (f.top, f.left);
            ed.next_view = leaves.last().map_or(1, |l| l + 1);
            (ed.session.views, ed.session.split, ed.session.focus) = (views, split, focus);
        }
    }
}

fn num(v: &Value) -> usize {
    v.as_u64().unwrap_or(0) as usize
}

fn mode_name(m: Mode) -> Option<&'static str> {
    Some(match m {
        Mode::Files => "files",
        Mode::Outline => "outline",
        Mode::List => "list",
        Mode::Git => "git",
        // (a plugin's tab comes back when the plugin sends it again)
        Mode::Plugin => return None,
    })
}

fn mode_of(s: &str) -> Option<Mode> {
    Some(match s {
        "files" => Mode::Files,
        "outline" => Mode::Outline,
        "list" => Mode::List,
        "git" => Mode::Git,
        _ => return None,
    })
}

/// A leaf is its view id; a node is `{"v": [...]}` (side by side) or `{"h": [...]}` (stacked).
fn split_json(s: &Split) -> Value {
    match s {
        Split::Leaf(id) => json!(id),
        Split::Node(dir, kids) => {
            let key = if *dir == Dir::Vertical { "v" } else { "h" };
            json!({ key: kids.iter().map(split_json).collect::<Vec<_>>() })
        }
    }
}

fn split_of(v: &Value) -> Option<Split> {
    if let Some(id) = v.as_u64() {
        return Some(Split::Leaf(id));
    }
    let (dir, kids) = match (v.get("v"), v.get("h")) {
        (Some(k), _) => (Dir::Vertical, k),
        (_, Some(k)) => (Dir::Horizontal, k),
        _ => return None,
    };
    let kids: Option<Vec<Split>> = kids.as_array()?.iter().map(split_of).collect();
    Some(Split::Node(dir, kids?)).filter(|s| matches!(s, Split::Node(_, k) if k.len() >= 2))
}

/// `--restart <file>`: the state the last tarae left — the file goes once read.
pub fn read(file: &Path) -> Option<Value> {
    let body = std::fs::read_to_string(file).ok();
    let _ = std::fs::remove_file(file);
    serde_json::from_str(&body?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn temp(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("tarae-restart-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    /// Files, multi-cursor selections, scroll, panes and `:set`s go through the state as they were; a
    /// file gone meanwhile is skipped and panes showing it show the current file.
    #[test]
    fn what_was_on_screen_comes_back() {
        let dir = temp("roundtrip");
        let (a, b, c) = (dir.join("a.txt"), dir.join("b.txt"), dir.join("c.txt"));
        std::fs::write(&a, "one\ntwo\nthree\n").unwrap();
        std::fs::write(&b, "한글 b\n").unwrap();
        std::fs::write(&c, "gone soon\n").unwrap();
        let mut ed = Editor::new(Config::default());
        ed.open(&a).unwrap();
        ed.doc_mut().set_selection(Selection::new(vec![Range::new(0, 2), Range::new(4, 7)], 1));
        ed.doc_mut().top = 1;
        ed.open(&c).unwrap();
        ed.split_view(Dir::Vertical); // c | c
        ed.open(&b).unwrap(); // c | b
        ed.doc_mut().set_selection(Selection::point(3)); // after 한
        crate::typed::execute(&mut ed, "set editor.scrolloff 9").unwrap();
        let state = snapshot(&mut ed);
        drop(ed);
        std::fs::remove_file(&c).unwrap();

        let mut ed = Editor::new(Config::default());
        assert_eq!(restore(&mut ed, &state), 2, "a and b — c is gone");
        assert_eq!(ed.config.scrolloff, 9, "the :set came along");
        assert_eq!(ed.overrides.len(), 1, "and stays a value until quit");
        let doc_a = ed.docs.iter().find(|d| d.path.as_deref() == Some(a.as_path())).unwrap();
        assert_eq!(doc_a.selection(), &Selection::new(vec![Range::new(0, 2), Range::new(4, 7)], 1));
        assert_eq!(doc_a.top, 1);
        assert_eq!(ed.doc().path.as_deref(), Some(b.as_path()), "the focused pane's file is current");
        assert_eq!(ed.doc().selection().primary().head, 3);
        assert_eq!(ed.session.split.leaves().len(), 2, "both panes");
        let other = ed.session.views.iter().find(|v| v.id != ed.session.focus).unwrap();
        assert_eq!(other.doc, ed.doc().id, "the pane that showed the gone file shows the current one");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A position past a file shortened meanwhile, or inside a character, lands on a cluster boundary.
    #[test]
    fn selections_snap_to_what_the_file_is_now() {
        let dir = temp("snap");
        let a = dir.join("a.txt");
        std::fs::write(&a, "e\u{301}e\u{301} long line\n").unwrap();
        let mut ed = Editor::new(Config::default());
        ed.open(&a).unwrap();
        ed.doc_mut().set_selection(Selection::single(Range::new(1, 30)));
        let state = snapshot(&mut ed);
        std::fs::write(&a, "e\u{301}\n").unwrap();
        let mut ed = Editor::new(Config::default());
        restore(&mut ed, &state);
        let r = ed.doc().selection().primary();
        assert_eq!((r.anchor, r.head), (0, 4), "cluster start · the end of the shorter text");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn split_trees_read_back() {
        let s = Split::Node(
            Dir::Vertical,
            vec![Split::Leaf(1), Split::Node(Dir::Horizontal, vec![Split::Leaf(3), Split::Leaf(2)])],
        );
        assert_eq!(split_of(&split_json(&s)), Some(s));
        assert_eq!(split_of(&json!({"v": [1]})), None, "a node of one isn't a split");
        assert_eq!(split_of(&json!("x")), None);
    }

    fn settle_until(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done(ed) {
            let left = end.saturating_duration_since(std::time::Instant::now());
            let ev = ed.events.recv_timeout(left).expect("event before timeout");
            ed.handle_event(ev);
        }
    }

    /// `settle_until`, a restart's card answered `y` (the checkout tarae started in) as it comes.
    fn settle_answering(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done(ed) {
            if ed.offers.first().is_some_and(|o| matches!(o.what, crate::offer::What::Restart(_))) {
                ed.offer_accept();
                continue;
            }
            let left = end.saturating_duration_since(std::time::Instant::now());
            let ev = ed.events.recv_timeout(left).expect("event before timeout");
            ed.handle_event(ev);
        }
    }

    /// The chat comes back with its own conversation (not the latest one kept — that's a guess), resumed
    /// in claude, with the words being typed, the scroll and follow as they were; one note says so.
    #[test]
    fn the_chat_comes_back_with_its_conversation_and_draft() {
        use crate::chat::{Msg, Role};
        let dir = temp("chat");
        // (the editor's root is the test's folder — its conversations are kept under it)
        let (store, cwd) = (dir.join("chats"), dir.clone());
        let keep = |id: &str, ago: u64, said: &str| {
            let msgs = [Msg::new(Role::User, "hello", ""), Msg::new(Role::Assistant, said, "")];
            let saved = crate::chats::Saved {
                id: id.into(),
                title: "hello".into(),
                updated: crate::chats::now() - ago,
                rows: crate::chats::rows_of(&msgs),
                plan: None,
                seq: crate::chats::seq(),
            };
            crate::chats::save(&store, &cwd, saved);
        };
        keep("s-1", 60, "Hello back.");
        keep("s-2", 0, "Another one.");
        let argv = dir.join("argv.log");
        let mut ed = Editor::new(Config::default());
        ed.session.root = cwd;
        ed.chats_dir = Some(store);
        ed.config.llm.command = "sh".into();
        ed.config.llm.args =
            vec!["-c".into(), format!("printf '%s\\n' \"$*\" >> '{}'; cat > /dev/null", argv.display())];
        // (as an earlier build wrote it — "session", not "conversation": a `:restart` into a new build)
        let state = json!({ "chat": { "session": "s-1", "input": "half 한", "cursor": 5, "focused": true,
                                      "scroll": 2, "follow": false, "at_end": dir } });
        assert_eq!(restore(&mut ed, &state), 0, "no files — the chat comes back all the same");
        settle_until(&mut ed, |ed| {
            ed.session.chat.as_ref().unwrap().msgs.iter().any(|m| m.text == "Hello back.")
        });
        let c = ed.session.chat.as_ref().unwrap();
        assert_eq!((c.input.as_str(), c.cursor, c.focused, c.scroll), ("half 한", 5, true, 2));
        assert!(!c.follow.on);
        assert_eq!(c.edits.at_end.as_deref(), Some(dir.as_path()), "review at the end stays on");
        assert_eq!(c.conversation.as_deref(), Some("s-1"));
        assert_eq!(c.msgs.last().unwrap().text, crate::chat::RESTARTED);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let started = || std::fs::read_to_string(&argv).unwrap_or_default();
        while !started().lines().last().unwrap_or_default().contains("--resume s-1") {
            assert!(std::time::Instant::now() < end, "resumed: {}", started());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(started().lines().count(), 1, "one claude — none prewarmed only to be killed");
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Review at the end (the folder it was agreed for) and claude's conversation go into the state with the
    /// chat.
    #[test]
    fn review_at_the_end_is_in_the_state() {
        let mut ed = Editor::new(Config::default());
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        crate::chat::open(&mut ed);
        assert_eq!(snapshot(&mut ed)["chat"]["at_end"], Value::Null);
        ed.session.chat.as_mut().unwrap().edits.at_end = Some("/p".into());
        ed.session.chat.as_mut().unwrap().conversation = Some("c-1".into());
        let state = snapshot(&mut ed);
        assert_eq!((&state["chat"]["at_end"], &state["chat"]["conversation"]), (&json!("/p"), &json!("c-1")));
    }

    fn feed(ed: &mut Editor, keys: &str) {
        let mut chars = keys.chars();
        while let Some(c) = chars.next() {
            let key: crate::key::Key = if c == '<' {
                chars.by_ref().take_while(|&c| c != '>').collect::<String>().parse().unwrap()
            } else {
                c.to_string().parse().unwrap()
            };
            ed.handle_key(key);
        }
    }

    fn toasts(ed: &Editor) -> String {
        ed.toasts.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join(" | ")
    }

    /// A stand-in for the built binary: answers `--version` with `said`.
    fn fake_tarae(dir: &Path, said: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let bin = dir.join("tarae");
        std::fs::write(&bin, format!("#!/bin/sh\necho '{said}'\n")).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    /// An editor on a modified file in `dir`, set to build with `build` and run `bin`.
    fn editing(dir: &Path, build: &str, bin: &Path) -> Editor {
        let file = dir.join("a.txt");
        std::fs::write(&file, "one\n").unwrap();
        let mut ed = Editor::new(Config::default());
        ed.session.root = dir.to_path_buf();
        ed.open(&file).unwrap();
        feed(&mut ed, "iX<esc>");
        ed.config.restart_build = build.into();
        ed.config.restart_binary = bin.display().to_string();
        ed
    }

    /// `:restart` saves, builds as a task, checks the new binary is a tarae, and ends the loop with it
    /// ready for main to run.
    #[test]
    fn restart_saves_builds_checks_and_ends_the_loop() {
        let dir = temp("go");
        let bin = fake_tarae(&dir, "tarae 9.9.9");
        let mut ed = editing(&dir, "true", &bin);
        feed(&mut ed, ":restart<ret>");
        settle_answering(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert_eq!(ed.restart.as_ref().and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "Xone\n", "saved first");
        let run = ed.session.task_run.as_ref().expect("the build ran as a task");
        assert_eq!((run.task.cmd.as_str(), &run.state), ("true", &crate::task::State::Passed));
        assert!(ed.session.task_last.is_none(), "not the task space m m runs again");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A slow disk doesn't hold the screen: `:restart` saves and goes on once the files are written (the
    /// saver's event, not a wait) — and `:restart` again meanwhile cancels it.
    #[test]
    fn a_slow_save_is_waited_for_off_the_main_loop() {
        let dir = temp("slow");
        let bin = fake_tarae(&dir, "tarae 9.9.9");
        let mut ed = editing(&dir, "", &bin);
        ed.saver.slow = Duration::from_millis(500);
        let t = Instant::now();
        feed(&mut ed, ":restart!<ret>");
        let took = t.elapsed();
        assert!(took < Duration::from_millis(300), "the main loop waited {took:?} for the save");
        assert_eq!(ed.restart.as_ref().unwrap().stage, Stage::Saving, "{}", toasts(&ed));
        settle_answering(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert_eq!(ed.restart.as_ref().and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        assert_eq!(std::fs::read_to_string(dir.join("a.txt")).unwrap(), "Xone\n", "saved first");

        let mut ed = editing(&dir, "", &bin);
        ed.saver.slow = Duration::from_millis(300);
        feed(&mut ed, ":restart!<ret>:restart!<ret>");
        assert!(ed.restart.is_none());
        settle_until(&mut ed, |ed| !ed.doc().is_modified());
        assert!(ed.restart.is_none() && !ed.should_quit, "{}", toasts(&ed));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A failed build, or a binary that isn't a tarae, calls it off — tarae stays, and says why.
    #[test]
    fn a_failed_build_or_a_wrong_binary_calls_it_off() {
        let dir = temp("off");
        let bin = fake_tarae(&dir, "tarae 9.9.9");
        let mut ed = editing(&dir, "exit 3", &bin);
        feed(&mut ed, ":restart<ret>");
        settle_answering(&mut ed, |ed| ed.restart.is_none());
        assert!(!ed.should_quit);
        assert!(toasts(&ed).contains("build failed — not restarting"), "{}", toasts(&ed));

        let not_tarae = fake_tarae(&dir, "something else");
        ed.config.restart_binary = not_tarae.display().to_string();
        feed(&mut ed, ":restart!<ret>");
        settle_answering(&mut ed, |ed| ed.restart.is_none());
        assert!(!ed.should_quit);
        assert!(toasts(&ed).contains("isn't a tarae that runs here"), "{}", toasts(&ed));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// What can't come along stops it before anything happens: text in a buffer with no file.
    #[test]
    fn a_buffer_with_no_file_stops_it() {
        let mut ed = Editor::new(Config::default());
        feed(&mut ed, "iscratch<esc>:restart!<ret>");
        assert!(ed.restart.is_none());
        assert!(!ed.should_quit);
        assert!(toasts(&ed).contains("can't restart: unsaved changes in"), "{}", toasts(&ed));
    }

    /// What's running or half written stops it with why, as `:q` would: a commit message, a task, a debug
    /// run. A shell in the terminal doesn't — the next tarae says it starts afresh.
    #[test]
    fn whats_running_or_half_written_stops_it() {
        let dir = temp("blocked");
        let bin = fake_tarae(&dir, "tarae 9.9.9");
        let mut ed = editing(&dir, "", &bin);
        let try_it = |ed: &mut Editor, why: &str| {
            let err = crate::typed::execute(ed, "restart!").unwrap_err();
            assert!(err.contains(why), "{err}");
            assert!(ed.restart.is_none());
        };
        ed.session.committing = Some(crate::gitpanel::Committing {
            doc: ed.doc().id,
            top: dir.clone(),
            amend: false,
            then_end: false,
        });
        try_it(&mut ed, "a commit message is open");
        ed.session.committing = None;
        let task = crate::task::Task { name: "slow".into(), cmd: "sleep 5".into(), cwd: dir.clone() };
        ed.task_start(task);
        // (it starts once the modified file is saved)
        settle_until(&mut ed, |ed| ed.session.task_run.is_some());
        try_it(&mut ed, "slow is running — let it finish, or stop it (space m c)");
        ed.task_close();
        ed.dap = Some(crate::dap::Dap::placeholder(crate::dap::State::Starting, "app".into()));
        try_it(&mut ed, "a debug run is on");
        ed.dap = Some(crate::dap::Dap::placeholder(crate::dap::State::Exited(Some(0)), "app".into()));
        crate::typed::execute(&mut ed, "restart!").unwrap();
        settle_answering(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert!(ed.should_quit, "an ended debug run doesn't count: {}", toasts(&ed));

        let mut ed = Editor::new(Config::default());
        restore(&mut ed, &json!({ "terminal": true }));
        assert!(toasts(&ed).contains("Restarted · the terminal starts afresh (space T)"), "{}", toasts(&ed));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Stopping the build (space m c) calls the restart off — it would never finish.
    #[test]
    fn stopping_the_build_cancels_the_restart() {
        let dir = temp("stopped");
        let bin = fake_tarae(&dir, "tarae 9.9.9");
        let mut ed = editing(&dir, "sleep 5", &bin);
        feed(&mut ed, ":restart<ret>");
        settle_answering(&mut ed, |ed| {
            ed.restart.as_ref().is_some_and(|r| matches!(r.stage, Stage::Building(_)))
        });
        feed(&mut ed, "<space>mc");
        assert!(ed.restart.is_none());
        assert!(toasts(&ed).contains("restart canceled — its build was stopped"), "{}", toasts(&ed));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A binary that can't be run: exec returns why, and the state it wrote for it is gone again.
    #[test]
    fn a_failed_exec_says_why_and_leaves_no_state() {
        let dir = temp("exec");
        let file = dir.join("restart-state.json");
        let said = exec_with(Editor::new(Config::default()), &dir.join("no-such-tarae"), &file);
        assert!(said.starts_with("couldn't run ") && said.contains("no-such-tarae"), "{said}");
        assert!(!file.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Claude's edit waiting in its review is part of the turn — `:restart` waits instead of refusing. Once
    /// accepted, the restart goes on only after Claude's write has landed (until then auto-save leaves the
    /// file to Claude, and it would count as unsaved).
    #[test]
    fn claudes_edit_and_its_write_are_waited_for() {
        let dir = temp("claude-edit");
        let bin = fake_tarae(&dir, "tarae 9.9.9");
        let file = dir.join("a.txt");
        std::fs::write(&file, "one\n").unwrap();
        // What Claude will write, and when: after the turn has ended — the watch sees it late
        let prepared = dir.join("claude.txt");
        std::fs::write(&prepared, "TWO\n").unwrap();
        let ask = json!({"type": "control_request", "request_id": "p1", "request": {"subtype": "can_use_tool",
            "tool_name": "Write", "input": {"file_path": file, "content": "TWO\n"}}});
        let result = json!({"type": "result", "result": ""});
        let mut ed = Editor::new(Config::default());
        ed.session.root = dir.clone();
        ed.open(&file).unwrap();
        ed.disk_watch();
        ed.config.restart_binary = bin.display().to_string();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do case \"$line\" in '{{\"message\"'*) printf '%s\\n' '{ask}';; \
                 '{{\"response\"'*) printf '%s\\n' '{result}'; sleep 0.3; cp '{}' '{}';; esac; done",
                prepared.display(),
                file.display()
            ),
        ];
        feed(&mut ed, "<space>lwrite it<ret>");
        settle_until(&mut ed, |ed| ed.session.review.is_some());
        crate::typed::execute(&mut ed, "restart!").unwrap();
        assert_eq!(ed.restart.as_ref().unwrap().stage, Stage::Turn, "{}", toasts(&ed));
        feed(&mut ed, "y");
        settle_answering(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert_eq!(ed.restart.as_ref().and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        assert!(toasts(&ed).contains("once Claude's edits are on disk"), "it waited: {}", toasts(&ed));
        assert!(!ed.doc().is_modified(), "Claude's write was the save");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "TWO\n");
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A turn's files still being read (review at the end) are waited for like the turn; then its review
    /// doesn't open over the restart — the chat says `space v t`, which works after it.
    #[test]
    fn a_turn_being_gathered_is_waited_for() {
        let dir = temp("gather");
        let bin = fake_tarae(&dir, "tarae 9.9.9");
        let mut ed = Editor::new(Config::default());
        ed.session.root = dir.clone();
        ed.config.restart_binary = bin.display().to_string();
        ed.session.checkpoints.gathering = true;
        crate::typed::execute(&mut ed, "restart!").unwrap();
        assert_eq!(ed.restart.as_ref().unwrap().stage, Stage::Turn, "{}", toasts(&ed));
        turn_over(&mut ed);
        assert_eq!(ed.restart.as_ref().unwrap().stage, Stage::Turn, "still reading");
        let file = dir.join("a.txt");
        std::fs::write(&file, "two\n").unwrap();
        ed.session.checkpoints.gathering = false;
        ed.session.checkpoints.list.push(crate::checkpoint::Checkpoint {
            question: "q".into(),
            files: vec![crate::checkpoint::Touched {
                path: file,
                before: Some(ropey::Rope::from_str("one\n")),
                after: ropey::Rope::from_str("two\n"),
                unseen: true,
            }],
            undone: false,
            seen: false,
        });
        crate::chat::turn_gathered(&mut ed, Some("a.txt".into()));
        assert!(ed.session.code_review.is_none(), "not over the restart");
        settle_answering(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert_eq!(ed.restart.as_ref().and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A chat turn in flight is waited for — the restart goes on when it ends; `:restart` again cancels.
    #[test]
    fn a_turn_in_flight_is_waited_for() {
        let dir = temp("turn");
        let bin = fake_tarae(&dir, "tarae 9.9.9");
        let mut ed = Editor::new(Config::default());
        ed.session.root = dir.clone();
        ed.config.restart_binary = bin.display().to_string();
        let init = json!({"type": "system", "subtype": "init", "session_id": "s-1"});
        let result = json!({"type": "result", "result": "done"});
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec![
            "-c".into(),
            format!(
                "while read -r line; do case \"$line\" in '{{\"message\"'*) sleep 0.3; \
                 printf '%s\\n' '{init}' '{result}';; esac; done"
            ),
        ];
        feed(&mut ed, "<space>lhello<ret>");
        assert!(ed.session.chat.as_ref().unwrap().busy());
        crate::typed::execute(&mut ed, "restart!").unwrap();
        crate::typed::execute(&mut ed, "restart!").unwrap();
        assert!(ed.restart.is_none(), "again = canceled");
        crate::typed::execute(&mut ed, "restart!").unwrap();
        assert_eq!(ed.restart.as_ref().unwrap().stage, Stage::Turn);
        assert!(toasts(&ed).contains("when Claude's turn ends"), "{}", toasts(&ed));
        settle_answering(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert!(!ed.session.chat.as_ref().unwrap().busy(), "only after the turn");
        assert_eq!(ed.restart.as_ref().and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A repository in `dir` with what's there in one commit — its short HEAD.
    fn repo(dir: &Path) -> String {
        for args in [
            &["init", "-q", "-b", "main"][..],
            &["config", "user.email", "t@t"],
            &["config", "user.name", "t"],
            &["config", "commit.gpgsign", "false"],
            &["add", "-A"],
            &["commit", "-q", "-m", "base"],
        ] {
            crate::gitmenu::git(dir, args, None).unwrap();
        }
        crate::gitmenu::git(dir, &["rev-parse", "--short", "HEAD"], None).unwrap().trim().to_string()
    }

    /// #108: the card says which checkout `:restart` builds in and at what commit — the one tarae started
    /// in, and `f` for the session in front's when that's another (it built the front's, an old session's,
    /// silently). `y` builds where tarae started; `f` there.
    #[test]
    fn the_restart_card_says_which_checkout_it_builds() {
        let dir = temp("which");
        let (main, other) = (dir.join("main"), dir.join("other"));
        let mut heads = Vec::new();
        for d in [&main, &other] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::write(d.join("a.txt"), "x\n").unwrap();
            heads.push(repo(d));
        }
        let bin = fake_tarae(&dir, "tarae 9.9.9");
        let mut ed = Editor::new(Config::default());
        (ed.session.root, ed.session.name) = (main.clone(), "main".into());
        ed.config.restart_build = "pwd > built-here".into();
        ed.config.restart_binary = bin.display().to_string();
        let card = |ed: &mut Editor| {
            settle_until(ed, |ed| !ed.offers.is_empty() || ed.restart.is_none());
            let o = ed.offers.first().unwrap_or_else(|| panic!("a card: {}", toasts(ed)));
            (o.title.clone(), o.question.clone(), o.restart_other())
        };
        // One session: where tarae started, nothing else to pick
        crate::typed::execute(&mut ed, "restart").unwrap();
        let (title, q, other_too) = card(&mut ed);
        assert_eq!(title, format!("Restart into main's build ({})?", heads[0]));
        assert!(
            q.starts_with("`pwd > built-here` in\n  ") && q.contains("\nthen runs ") && !other_too,
            "{q}"
        );
        ed.offer_dismiss();
        assert!(ed.restart.is_none() && !main.join("built-here").exists(), "n: nothing built");
        // Another session in front: the card still starts where tarae started, the front's is `f`
        ed.session_open(&other).unwrap();
        crate::typed::execute(&mut ed, "restart").unwrap();
        let (title, q, other_too) = card(&mut ed);
        assert_eq!(title, format!("Restart into main's build ({})?", heads[0]), "{q}");
        assert!(other_too && q.contains(&format!("f — other ({})'s build instead", heads[1])), "{q}");
        ed.offer_restart_other();
        settle_until(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert_eq!(ed.restart.as_ref().and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        assert!(
            other.join("built-here").exists() && !main.join("built-here").exists(),
            "built in the front's"
        );
        drop(ed);
        // `y`: where tarae started, the front another
        let mut ed = Editor::new(Config::default());
        (ed.session.root, ed.session.name) = (main.clone(), "main".into());
        ed.config.restart_build = "pwd > built-here".into();
        ed.config.restart_binary = bin.display().to_string();
        ed.session_open(&other).unwrap();
        std::fs::remove_file(other.join("built-here")).unwrap();
        crate::typed::execute(&mut ed, "restart").unwrap();
        card(&mut ed);
        settle_answering(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert!(
            main.join("built-here").exists() && !other.join("built-here").exists(),
            "built where it started"
        );
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #108: a binary the build left as it was, written before its checkout's HEAD was committed, is said on
    /// a card — not run as if it were new; `y` restarts onto it anyway, `n` doesn't. One the build wrote goes
    /// straight on.
    #[test]
    fn the_same_old_binary_is_said_not_run() {
        let dir = temp("same");
        let main = dir.join("main");
        std::fs::create_dir_all(&main).unwrap();
        std::fs::write(main.join("a.txt"), "x\n").unwrap();
        let head = repo(&main);
        let bin = fake_tarae(&dir, "tarae 9.9.9");
        let an_hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        let age =
            |bin: &Path| std::fs::File::options().write(true).open(bin).unwrap().set_modified(an_hour_ago);
        age(&bin).unwrap();
        let mut ed = Editor::new(Config::default());
        (ed.session.root, ed.session.name) = (main.clone(), "main".into());
        ed.config.restart_build = "true".into();
        ed.config.restart_binary = bin.display().to_string();
        let same = |ed: &Editor| {
            ed.offers.first().is_some_and(|o| matches!(o.what, crate::offer::What::RestartSame { .. }))
        };
        crate::typed::execute(&mut ed, "restart").unwrap();
        settle_answering(&mut ed, |ed| same(ed) || ed.restart.is_none());
        let o = &ed.offers[0];
        assert!(o.title.ends_with("tarae is the binary it was"), "{}", o.title);
        assert!(
            o.question.contains(&format!("written before main ({head}) was committed")),
            "{}",
            o.question
        );
        ed.offer_dismiss();
        assert!(ed.restart.is_none() && !ed.should_quit, "n: not restarting");
        // `y`: anyway
        crate::typed::execute(&mut ed, "restart").unwrap();
        settle_answering(&mut ed, |ed| same(ed) || ed.restart.is_none());
        ed.offer_accept();
        settle_until(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert_eq!(ed.restart.as_ref().and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        // A build that writes it: on at once
        let mut ed = Editor::new(Config::default());
        (ed.session.root, ed.session.name) = (main.clone(), "main".into());
        ed.config.restart_build = format!("touch '{}'", bin.display());
        ed.config.restart_binary = bin.display().to_string();
        crate::typed::execute(&mut ed, "restart").unwrap();
        settle_answering(&mut ed, |ed| same(ed) || ed.should_quit || ed.restart.is_none());
        assert_eq!(ed.restart.as_ref().and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #109: a restart waits for every session's turn, not only the front's — saying whose — and goes on
    /// once the last one ends (it ended another session's turn, silently).
    #[test]
    fn a_restart_waits_for_every_sessions_turn() {
        let dir = temp("every-turn");
        let (main, back) = (dir.join("main"), dir.join("back"));
        for d in [&main, &back] {
            std::fs::create_dir_all(d).unwrap();
        }
        let bin = fake_tarae(&dir, "tarae 9.9.9");
        let mut ed = Editor::new(Config::default());
        (ed.session.root, ed.session.name) = (main.clone(), "main".into());
        ed.config.restart_binary = bin.display().to_string();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        let id = ed.session_open(&back).unwrap();
        crate::chat::open(&mut ed);
        ed.session.chat.as_mut().unwrap().state = crate::chat::State::Thinking;
        assert!(ed.session_switch(0));
        crate::typed::execute(&mut ed, "restart!").unwrap();
        assert_eq!(ed.restart.as_ref().unwrap().stage, Stage::Turn);
        assert!(toasts(&ed).contains("tarae restarts when Claude's turn ends in back"), "{}", toasts(&ed));
        ed.handle_event(crate::event::Event::Resize);
        assert_eq!(ed.restart.as_ref().unwrap().stage, Stage::Turn, "back's still at work");
        let back_chat = ed.sessions.iter_mut().find(|l| l.id == id).and_then(|l| l.chat.as_mut()).unwrap();
        back_chat.state = crate::chat::State::Idle;
        ed.handle_event(crate::event::Event::Resize);
        settle_answering(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert_eq!(ed.restart.as_ref().and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_binary_is_found_from_the_project_root() {
        let root = Path::new("/proj");
        assert_eq!(resolve("target/release/tarae", root).unwrap(), root.join("target/release/tarae"));
        assert_eq!(resolve("/opt/tarae", root).unwrap(), PathBuf::from("/opt/tarae"));
        assert!(resolve("", root).unwrap().is_absolute(), "the running one");
    }

    /// Nothing that was open is still there: the start screen, as a plain start shows it.
    #[test]
    fn nothing_left_means_a_plain_start() {
        let mut ed = Editor::new(Config::default());
        let state = json!({ "docs": [{ "path": "/nowhere/at/all.txt" }], "current": 0 });
        assert_eq!(restore(&mut ed, &state), 0);
        assert_eq!(ed.docs.len(), 1);
        assert!(ed.doc().path.is_none());
    }
}
