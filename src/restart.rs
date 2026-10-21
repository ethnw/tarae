//! `:restart` — a new tarae (after `cargo install`, or tarae building tarae). Build if
//! `editor.restart-build` says how, check the binary (`editor.restart-binary`, else the running one's
//! path) answers `--version`, then become it: `exec` keeps the pid, the terminal and the zellij pane, and
//! the new tarae comes back as it was in every session — each one's open files with their selections and
//! scroll, its panes, side panel, chat (its claude conversation resumed, the words being typed) and code
//! review (its base, the file it was on) — the one in front in front, and the `:set`s. A session behind comes back as it would look had you switched away
//! a moment ago: its files' servers asleep, its chat without a claude until it's used.
//!
//! The state goes through one file (`restart-<pid>.json` in the state dir → `--restart <file>`), written
//! once the loop has ended and read (and removed) at startup. Each session has its block in it; the
//! top-level fields stay as earlier builds read them (`snapshot`).
//!
//! Files are saved first, as before a task. What can't come along stops it the way `:q` is stopped — a
//! buffer with no file, an edit's review, a commit message — in any session (`blocker`). What's under way — a
//! chat turn and its edits waiting, a command, tests — is named on its card: restart anyway (that's ended;
//! what begins after isn't, it's waited for) or wait (`turn_over`). A restart under way is on the status
//! line until it happens (`pending`); `:restart` again asks before calling it off.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::document::DocId;
use crate::editor::{Editor, View};
use crate::event::SessionId;
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
    /// It goes ahead: its dSYM is kept first (`dsyms::keep`, a worker).
    Keeping,
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
    /// The binary's dSYM kept (`dsyms::keep`): where, None when there was none — or why not (the restart goes
    /// on). Unset until it goes ahead (`keep_then`). Carried in the state: the next tarae logs it.
    kept: Option<Result<Option<crate::dsyms::Kept>, String>>,
    /// Its card was answered with work under way (`under_way`) — restart anyway: what the card named is
    /// ended, not waited for; what began after it is (it wasn't named).
    anyway: Vec<Work>,
}

/// One thing `under_way` names, told apart from what begins after its card: a session's Claude turn (and
/// whether an edit of it waits for an answer — ended with it), a command or tests by when they started.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Work {
    Turn { session: SessionId, edits: bool },
    Run(Instant),
}

impl Work {
    /// Named on a card that named `named` — a turn whose edit waits only if the card said so.
    fn named_in(&self, named: &[Work]) -> bool {
        named.iter().any(|n| match (self, n) {
            (Work::Turn { session, edits }, Work::Turn { session: s, edits: e }) => {
                session == s && (*e || !edits)
            }
            (w, n) => w == n,
        })
    }
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
/// `under_way`: what a restart now ends (`under_way`), said, and `work` the same told apart — `w` waits for it
/// instead.
#[derive(Clone, Debug)]
pub struct Choice {
    pub id: u64,
    pub targets: Vec<Target>,
    pub under_way: Vec<String>,
    work: Vec<Work>,
}

impl Restart {
    /// Tests: one gone ahead, the loop ending to run `bin`.
    #[cfg(test)]
    pub fn ready_to(bin: PathBuf) -> Restart {
        let stage = Stage::Ready(bin);
        Restart { id: 0, build: false, stage, target: None, kept: None, anyway: Vec::new() }
    }

    /// The binary to run once the loop has ended.
    pub fn ready(&self) -> Option<&Path> {
        match &self.stage {
            Stage::Ready(bin) => Some(bin),
            _ => None,
        }
    }
}

// ── :restart ────────────────────────────────────────────────────────────────

/// `:restart` (`build`) / `:restart!`. Again while one is under way: a card asks before calling it off
/// (#194: a second `:restart`, the first thought to have done nothing, canceled it) — one card: asked
/// again while it's up, that card comes to the front.
pub fn start(ed: &mut Editor, build: bool) -> Result<(), String> {
    if let Some(r) = ed.restart.as_ref() {
        let up = ed
            .offers
            .iter()
            .position(|o| matches!(o.what, crate::offer::What::RestartCancel { id } if id == r.id));
        if let Some(i) = up {
            ed.offer_to_front(i);
            ed.set_status("already asking — y calls the restart off, n keeps it going");
            return Ok(());
        }
        let (id, waits) = (r.id, pending(ed).unwrap_or_else(|| "tarae restarts".into()));
        let q = format!("{}.\n\ny calls it off · n keeps it going", said(ed, &waits));
        let what = crate::offer::What::RestartCancel { id };
        ed.push_offer(crate::offer::Offer::new("Call off the restart?".into(), &q, "", String::new(), what));
        return Ok(());
    }
    blocker(ed, true).map_or(Ok(()), |why| Err(format!("can't restart: {why}")))?;
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let stage = Stage::Turn;
    ed.restart = Some(Restart { id, build, stage, target: None, kept: None, anyway: Vec::new() });
    // What's under way in any session — Claude's turns (#109), commands, tests — is named on its card:
    // restart anyway (ended) or wait (frictions15: one session's turn and its long command held it, unsaid)
    go(ed);
    Ok(())
}

/// What runs now that a restart would end, in every session (#109): Claude in a turn (or its turn's files
/// being read), the task panel's run, the tests — named by session when there are several, with how long.
/// The restart's own build isn't one. As waited for — the status line, the cancel card.
fn under_way(ed: &Editor) -> Vec<String> {
    under_way_of(ed, false).into_iter().map(|(_, said)| said).collect()
}

/// `under_way`, each told apart (`Work`) — `ending`: as a restart now would end it (its card, the toast of
/// its yes), a turn naming its edits waiting for an answer, dropped with it (`edits_waiting`).
fn under_way_of(ed: &Editor, ending: bool) -> Vec<(Work, String)> {
    let several = !ed.sessions.is_empty();
    let build = match ed.restart.as_ref().map(|r| &r.stage) {
        Some(Stage::Building(g)) => Some(*g),
        _ => None,
    };
    let mut out = Vec::new();
    for l in ed.sessions_in_order() {
        let at = |what: String| if several { format!("{}: {what}", l.label()) } else { what };
        if l.chat.as_ref().is_some_and(crate::chat::Chat::busy) || l.checkpoints.gathering {
            let (edits, files) = edits_waiting(ed, l);
            let said = match edits {
                _ if !ending || edits == 0 => "Claude's turn".to_string(),
                1 => format!("Claude's turn — its unanswered edit to {} is dropped", files.join(", ")),
                n => format!("Claude's turn — its {n} unanswered edits to {} are dropped", files.join(", ")),
            };
            out.push((Work::Turn { session: l.id, edits: edits > 0 }, at(said)));
        }
        let running = crate::task::State::Running;
        if let Some(r) = l.task_run.as_ref().filter(|r| r.state == running && Some(r.generation()) != build) {
            let cmd = crate::task::traced(&r.task.cmd);
            let said = format!("{cmd} in the task panel, {}", crate::waits::clock(r.started.elapsed()));
            out.push((Work::Run(r.started), at(said)));
        }
        let testing = crate::testing::RunState::Running;
        if let Some(r) = l.test_run.as_ref().filter(|r| r.state == testing) {
            let said = format!("tests {}, {}", r.target.label, crate::waits::clock(r.started.elapsed()));
            out.push((Work::Run(r.started), at(said)));
        }
    }
    out
}

/// Claude's edits in session `l` waiting for an answer — the one in its review, those queued behind it — and
/// their files, each once. A restart drops them unanswered: the claude that asked is gone after it, and its
/// conversation resumed can't take an answer to a request of the old process's.
fn edits_waiting(ed: &Editor, l: &crate::session::Session) -> (usize, Vec<String>) {
    let Some(c) = l.chat.as_ref() else { return (0, Vec::new()) };
    let reviewed = c.edits.review.and(l.review.as_ref()).map(|r| r.doc_id);
    let shown = reviewed.and_then(|id| ed.docs.iter().find(|d| d.id == id)).and_then(|d| d.path.clone());
    let queued = c
        .edits
        .queue
        .iter()
        .map(|p| crate::chat::resolve(&l.root, p.input["file_path"].as_str().unwrap_or_default()));
    let mut files = Vec::new();
    for path in shown.into_iter().chain(queued) {
        let said = crate::chat::shown(&l.root, &path);
        if !files.contains(&said) {
            files.push(said);
        }
    }
    (c.edits.queue.len() + usize::from(c.edits.review.is_some()), files)
}

/// What runs now that the restart's answer didn't name (`Restart::anyway`; all of it before its card or after
/// `w`) — waited for.
fn unnamed(ed: &Editor) -> Vec<String> {
    let named = ed.restart.as_ref().map_or(&[][..], |r| &r.anyway);
    under_way_of(ed, false).into_iter().filter(|(w, _)| !w.named_in(named)).map(|(_, said)| said).collect()
}

/// A restart under way, in a few words for the status line — `restart waits for gitescape`, `restart
/// waits for its build` — until it happens (#194: said once in a toast, then nothing showed it pending). None
/// while none is, or while its card asks (it's in sight).
pub fn pending(ed: &Editor) -> Option<String> {
    Some(match &ed.restart.as_ref()?.stage {
        Stage::Turn => match under_way(ed).as_slice() {
            [] => "restart waits".into(),
            names => format!("restart waits for {}", names.join(", ")),
        },
        Stage::Writes => "restart waits for Claude's edits".into(),
        Stage::Building(_) => "restart waits for its build".into(),
        // (a worker cloning the binary's dSYM — a moment on APFS, a copy elsewhere)
        Stage::Keeping => "restarting — keeping its symbols".into(),
        Stage::Saving | Stage::Finding | Stage::Checking | Stage::Ready(_) => "restarting".into(),
        Stage::Choosing | Stage::Same => return None,
    })
}

/// What the cancel card says the restart waits for: what's under way by name, else the status line's words.
fn said(ed: &Editor, pending: &str) -> String {
    let under = under_way(ed);
    match ed.restart.as_ref().map(|r| &r.stage) {
        Some(Stage::Turn) if !under.is_empty() => {
            format!("tarae restarts once these end: {}", under.join(", "))
        }
        _ => pending.to_string(),
    }
}

/// The answer on the card `:restart` again raised: `y` calls restart `id` off (and its cards go), `n` keeps
/// it going. Gone meanwhile (it happened, or failed): said.
pub fn cancel_answer(ed: &mut Editor, card: u64, id: u64, yes: bool) {
    ed.offer_finished(card, Ok(()));
    if !ed.restart.as_ref().is_some_and(|r| r.id == id) {
        return ed.set_status("that restart isn't under way any more");
    }
    if !yes {
        let waits = pending(ed).unwrap_or_else(|| "the restart goes on".into());
        return ed.set_status(format!("{waits} — kept"));
    }
    ed.restart = None;
    let restarts = |w: &crate::offer::What| {
        matches!(w, crate::offer::What::Restart(_) | crate::offer::What::RestartSame { .. })
    };
    ed.drop_cards_for(restarts, "dropped — the restart was called off");
    ed.set_status("restart canceled");
}

/// A chat's turn ended (or the chat closed, or the turn's files were read): a restart waiting for every
/// session's goes on — from the front (`sync`), not while a session behind is swapped in to apply it.
pub fn turn_over(ed: &mut Editor) {
    if !ed.background() {
        sync(ed);
    }
}

/// Every event: a restart waiting (its card's `w`, or a turn asked after its answer) goes on once nothing is
/// under way in any session — but what it was told to restart anyway with (`unnamed`).
pub fn sync(ed: &mut Editor) {
    if ed.restart.as_ref().is_some_and(|r| r.stage == Stage::Turn) && unnamed(ed).is_empty() {
        go(ed);
    }
}

/// What keeps tarae from restarting now, in words — in any session, named when there are several. Files
/// that can be saved don't — `go` saves them. Work under way doesn't (frictions15): commands and tests are
/// named on the card (`under_way`) — restart anyway, or wait. While a session's Claude is in a turn
/// (`waiting`: before the card, or told to restart anyway), what belongs to the turn — its edit's review —
/// doesn't either. A shell open in the terminal doesn't: it's started again by hand.
fn blocker(ed: &Editor, waiting: bool) -> Option<String> {
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
    let several = !ed.sessions.is_empty();
    let in_session = ed.sessions_in_order().into_iter().find_map(|l| {
        let why = session_blocker(l, waiting)?;
        Some(if several { format!("{}: {why}", l.label()) } else { why })
    });
    if in_session.is_some() {
        return in_session;
    }
    if ed.dap.as_ref().is_some_and(|d| !matches!(d.state, crate::dap::State::Exited(_))) {
        return Some("a debug run is on — end it first (space G t)".into());
    }
    None
}

/// `blocker`'s part in session `l`, front or behind.
fn session_blocker(l: &crate::session::Session, waiting: bool) -> Option<String> {
    // (a turn's files still being read belong to it)
    let in_turn =
        waiting && (l.chat.as_ref().is_some_and(crate::chat::Chat::busy) || l.checkpoints.gathering);
    let claudes = |yes: bool| in_turn && yes;
    if l.review.is_some() && !claudes(l.chat.as_ref().is_some_and(|c| c.edits.review.is_some())) {
        return Some("a review is open — answer it first".into());
    }
    // (a code review comes along, its drafts with the notes — a turn's too, by its place from the newest turn
    // (frictions15: one session's open review held every session's restart); a rebase's stops it as the
    // rebase)
    // (saved, it would come back as a plain file — `:wq` no longer committing)
    if l.committing.is_some() {
        return Some("a commit message is open — :wq commits it, :q drops it".into());
    }
    // (an install stopped half-way can leave a tool broken; other runs are named on the card)
    let running = crate::task::State::Running;
    if let Some(r) = l.task_run.as_ref().filter(|r| r.state == running && r.install) {
        return Some(format!("{} is installing — let it finish", r.task.name));
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
            // (on: what began meanwhile is the card's to name, or `check`'s to wait for once it's answered)
            if at(ed, id, &Stage::Writes) {
                go(ed);
            }
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
    // (before its card, or told to restart anyway, a turn's own review isn't in the way: the turn is named,
    // its edits waiting with it — `under_way`)
    let turn_ok = ed.restart.as_ref().is_some_and(|r| !r.anyway.is_empty() || r.target.is_none());
    let why = match blocker(ed, turn_ok) {
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
    let said = crate::gitrepo::git(&root, &["log", "-1", "--format=%h %ct"], None).unwrap_or_default();
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
    // What runs now — Claude's turns, commands, tests, in any session — ended by a restart now; `w` waits for
    // them (frictions15: one session's turn and its long command held the restart, unsaid). In the warning
    // colour, as what won't come back is
    let (work, under): (Vec<Work>, Vec<String>) = under_way_of(ed, true).into_iter().unzip();
    let mut warn_from = None;
    if !under.is_empty() {
        warn_from = Some(q.split('\n').count() + 1);
        let lines: Vec<String> = under.iter().map(|u| format!("  {u}")).collect();
        q.push_str(&format!("\n\nUnder way — ended if it restarts now:\n{}", lines.join("\n")));
    }
    // The cards waiting that won't come back (a moment's decision about something live), by session — in
    // the warning colour, before the answer
    let lost: Vec<String> = ed
        .sessions_in_order()
        .into_iter()
        .flat_map(|l| {
            cards_of(ed, l)
                .filter(|o| o.what.over_restart() == crate::offer::OverRestart::Lost)
                .map(|o| format!("  {} ({})", o.title, l.label()))
        })
        .collect();
    if !lost.is_empty() {
        warn_from.get_or_insert(q.split('\n').count() + 1);
        q.push_str(&format!(
            "\n\nWon't come back — answer it first, or ask again after:\n{}",
            lost.join("\n")
        ));
    }
    let warned = warn_from.map(|from| from..q.split('\n').count());
    let what = crate::offer::What::Restart(Box::new(Choice { id, targets, under_way: under, work }));
    let mut o = crate::offer::Offer::new(title, &q, "Restarting…", "Not restarting".into(), what);
    if let Some(lines) = warned {
        o = o.warning(lines);
    }
    ed.push_offer(o);
}

/// The answer on its card: build in (or run) `targets[which]` — None calls it off.
pub fn chosen(ed: &mut Editor, card: u64, choice: Choice, which: Option<usize>) {
    ed.offer_finished(card, Ok(()));
    if !at(ed, choice.id, &Stage::Choosing) {
        return;
    }
    let Some(which) = which.filter(|&i| i < choice.targets.len()) else {
        ed.restart = None;
        return ed.set_status("not restarting");
    };
    // Something began while the card was up that it doesn't name (a run, Claude's edit waiting): the card
    // again, naming it — `y` would end it unsaid
    let now = under_way_of(ed, true);
    if now.iter().any(|(w, _)| !w.named_in(&choice.work)) {
        ed.set_status("something began since the card asked — it asks again");
        return ask(ed, choice.id, choice.targets);
    }
    let t = choice.targets[which].clone();
    let r = ed.restart.as_mut().expect("at its stage");
    let build = Some(ed.config.restart_build.trim().to_string()).filter(|c| r.build && !c.is_empty());
    r.target = Some(t.clone());
    // (with work under way: restart anyway — what the card named is ended, said; not waited for)
    if !now.is_empty() {
        r.anyway = choice.work;
        let names: Vec<String> = now.into_iter().map(|(_, said)| said).collect();
        ed.set_warning(format!("restarting anyway — it ends {}", names.join(", ")));
    }
    // (no build: its card said whether the binary is older than HEAD)
    let Some(cmd) = build else { return check(ed, t.bin, true) };
    let task = crate::task::Task { name: "restart build".into(), cmd, cwd: t.root.clone() };
    let Some(generation) = ed.task_start_restart(task) else {
        return cancel(ed, "the build didn't start".into());
    };
    if let Some(r) = ed.restart.as_mut() {
        r.stage = Stage::Building(generation);
    }
    ed.set_status(format!(
        "building {} — tarae restarts when it's done (:restart again asks to call it off)",
        t.named()
    ));
}

/// `w` on its card: wait for what's under way to end, in every session — then the card again (what it builds
/// and runs looked up afresh). `:restart` again asks to call it off.
pub fn wait_answer(ed: &mut Editor, card: u64, choice: Choice) {
    ed.offer_finished(card, Ok(()));
    if !at(ed, choice.id, &Stage::Choosing) {
        return;
    }
    if let Some(r) = ed.restart.as_mut() {
        r.stage = Stage::Turn;
    }
    // (as waited for: a turn's edits are answered as it goes on, not dropped)
    ed.set_status(format!(
        "tarae restarts once these end: {} — :restart again asks to call it off",
        under_way(ed).join(", ")
    ));
    turn_over(ed);
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
            // Something began since the card that it didn't name (Claude asked something, a run, an edit of
            // a named turn's), or Claude's edits are still landing: on from that wait (built already, the
            // card answered)
            let unnamed = unnamed(ed);
            if !unnamed.is_empty() || pending_writes(ed).is_some() {
                let r = ed.restart.as_mut().expect("checked above");
                let built = if r.build { "built — " } else { "" };
                (r.stage, r.build) = (Stage::Turn, false);
                if !unnamed.is_empty() {
                    ed.set_status(format!("{built}tarae restarts once these end: {}", unnamed.join(", ")));
                }
                return turn_over(ed);
            }
            keep_then(ed, id, bin);
        }
    });
}

/// The restart goes ahead — checked, its card answered, no turn to wait for: the binary's symbols are kept
/// by its UUID before it runs, as the next build replaces the build-dir's dSYM (`dsyms::keep`, a worker; a
/// check that's turned down keeps nothing). Where they went — or why they weren't kept, and the restart goes
/// on — comes along in the state, for the next tarae's log (`symbols_said`): files saved, the loop ends.
fn keep_then(ed: &mut Editor, id: u64, bin: PathBuf) {
    let Some(r) = ed.restart.as_mut() else { return };
    r.stage = Stage::Keeping;
    ed.events.jobs().spawn(move || {
        let kept = crate::dsyms::keep(&bin);
        move |ed: &mut Editor| {
            if !at(ed, id, &Stage::Keeping) {
                return;
            }
            if let Some(r) = ed.restart.as_mut() {
                r.kept = Some(kept);
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
    // (std's are marked already, but not all are std's — one left open would keep its holder from ending)
    let marked = crate::spawn::close_on_exec();
    let signals = Signals::now();
    // (the new tarae starts from the descriptor limit this one started with, and raises it itself — else it
    // would take the raised one for the original and give it to what it starts: `nofile`)
    let limit = crate::nofile::for_exec();
    let err = std::process::Command::new(bin).arg("--restart").arg(file).exec();
    // Only a failed exec returns, and std's exec set this process up as a fork's child first: no signal
    // blocked, SIGPIPE at its default — a write to a closed pipe would end it now. Put back as they were
    signals.restore();
    limit.undo();
    for fd in marked {
        // SAFETY: clearing the flag set above on a descriptor number; one that's gone just fails.
        unsafe {
            libc::fcntl(fd, libc::F_SETFD, 0);
        }
    }
    let _ = std::fs::remove_file(file);
    format!("couldn't run {}: {err}", bin.display())
}

/// SIGPIPE's disposition and this thread's signal mask — what std's exec changes before it tries.
struct Signals {
    pipe: libc::sigaction,
    mask: libc::sigset_t,
}

impl Signals {
    fn now() -> Signals {
        // SAFETY: queries into zeroed out-structs of the right types.
        unsafe {
            let mut s = Signals { pipe: std::mem::zeroed(), mask: std::mem::zeroed() };
            libc::sigaction(libc::SIGPIPE, std::ptr::null(), &mut s.pipe);
            libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut s.mask);
            s
        }
    }

    fn restore(&self) {
        // SAFETY: putting back what `now` read, as it was.
        unsafe {
            libc::sigaction(libc::SIGPIPE, &self.pipe, std::ptr::null_mut());
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.mask, std::ptr::null_mut());
        }
    }
}

/// First thing after a restart: the last tarae's children that were stopped but not waited for are this
/// process's now (same pid) — waited for, so they don't linger as zombies. Those still running (#136: its
/// own build, told to stop a moment before the exec and not yet gone) are returned, for `wait_for` — every
/// child this process has now is the last one's: nothing else has been started yet.
pub fn reap() -> Vec<u32> {
    let mut status = 0;
    // SAFETY: waitpid with WNOHANG on any child; nothing else has been started yet.
    while unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) } > 0 {}
    crate::procs::children(std::process::id())
}

/// The last tarae's children still running at the restart, each waited for on a thread of its own as it
/// ends — nothing here started them, so nothing else would (once threads may start: `main`).
pub fn wait_for(pids: Vec<u32>) {
    for pid in pids {
        let _ =
            std::thread::Builder::new().name("tarae-reap".into()).spawn(move || crate::spawn::wait_once(pid));
    }
}

// ── What comes along ────────────────────────────────────────────────────────

/// What was on screen, as the next tarae reads it. Documents without a file (scratch, a review's deleted
/// file, `jdt://` classes) stay behind — panes showing one show the current file — and so do files outside
/// the project Claude's turn opened that you never took up (`Document::aside`, #37).
///
/// Each session's own block is in `sessions` (state v2: its files, panes, side panel, chat, shell — the
/// fields below, of its own, `session_json`). The top-level ones stay as earlier builds read them — every
/// file, the front's panes and chat — so a restart into one (`f` on the card) still comes back as it did.
pub fn snapshot(ed: &mut Editor) -> Value {
    ed.sync_focus_view();
    let docs: Vec<&crate::document::Document> = ed.docs.iter().filter(|d| carried(d)).collect();
    let mut state = session_json(&ed.session, &docs, ed.doc().id);
    let overrides: Vec<Value> = ed.overrides.iter().map(|(p, v)| json!([p, v.to_string()])).collect();
    // (every session, and which is in front — each with its block)
    let mut sessions = crate::worktree::snapshot(ed);
    let all = ed.sessions_in_order();
    for (block, l) in sessions["sessions"].as_array_mut().into_iter().flatten().zip(&all) {
        let current = if l.id == ed.session.id { ed.doc().id } else { l.current_doc };
        let own: Vec<&crate::document::Document> =
            docs.iter().filter(|d| l.docs.contains(&d.id)).copied().collect();
        if let (Some(block), Value::Object(fields)) = (block.as_object_mut(), session_json(l, &own, current))
        {
            block.extend(fields);
            // (its cards a command asks again — the rest come back by themselves, or were named on the card)
            let again: Vec<Value> = cards_of(ed, l)
                .filter_map(|o| match o.what.over_restart() {
                    crate::offer::OverRestart::Again(line) => Some(json!({ "again": line })),
                    _ => None,
                })
                .collect();
            if !again.is_empty() {
                block.insert("cards".into(), Value::Array(again));
            }
        }
    }
    if let Some(top) = state.as_object_mut() {
        top.extend([
            ("light".to_string(), json!(crate::theme::is_light())),
            ("overrides".into(), json!(overrides)),
            // (taken back until you quit, their store's write failed: `restore_quietly` takes them back again)
            // (not those a write that landed ended: its result wasn't applied — the loop had ended)
            ("taking".into(), {
                let landed = ed.allow_landed.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let taking = ed.allow_taking.iter().filter(|(n, _, _)| !landed.contains(n));
                json!(taking.map(|(n, r, w)| (r, w, n)).collect::<Vec<_>>())
            }),
            ("trust_taking".into(), crate::trust::taking_now(ed)),
            // (the stores' writes not landed when the exec came — its take-backs and no's, given again — and the
            // sequence they were given in, so what's given after the exec is later)
            ("store_writes".into(), json!({
                "trust": ed.trust_path.as_deref().map(crate::storewrite::pending_carried).unwrap_or_default(),
                "allow": ed.allow_path.as_deref().map(crate::storewrite::pending_carried).unwrap_or_default(),
                "sessions": ed.sessions_path().as_deref().map(crate::storewrite::pending_carried).unwrap_or_default(),
            })),
            ("given_last".into(), json!(crate::storewrite::last())),
            ("sessions".into(), sessions),
            // (the review's viewed marks, each session's)
            ("viewed".into(), crate::codereview::viewed_snapshot(ed)),
            ("symbols".into(), symbols(ed)),
        ]);
    }
    state
}

/// Where the binary about to run has its symbols kept (`dsyms::keep`), or why they weren't — None when it
/// had none beside it, or no restart went ahead.
fn symbols(ed: &Editor) -> Value {
    match ed.restart.as_ref().and_then(|r| r.kept.as_ref()) {
        Some(Ok(Some(k))) => json!({ "at": k.at, "pruned": k.pruned.as_ref().err() }),
        Some(Err(e)) => json!({ "not": e }),
        Some(Ok(None)) | None => Value::Null,
    }
}

/// The next tarae's log lines for its own symbols (`symbols` in the state): `(level, text)` — where they are,
/// with its pid (the same across the exec, as `sample` shows it), else why they weren't kept.
fn symbols_said(v: &Value, pid: u32) -> Vec<(&'static str, String)> {
    let s = &v["symbols"];
    let mut said = Vec::new();
    if let Some(at) = s["at"].as_str() {
        said.push(("info", format!("symbols of this tarae (pid {pid}): {at}")));
    }
    if let Some(e) = s["pruned"].as_str() {
        said.push(("warning", format!("older kept symbols not removed: {e}")));
    }
    if let Some(e) = s["not"].as_str() {
        said.push(("warning", format!("symbols of this tarae (pid {pid}) not kept: {e}")));
    }
    said
}

/// Session `l`'s cards waiting: those up (and stacked) when it's in front, those parked when it's behind.
fn cards_of<'a>(
    ed: &'a Editor,
    l: &'a crate::session::Session,
) -> impl Iterator<Item = &'a crate::offer::Offer> {
    let up = if l.id == ed.session.id { ed.offers.as_slice() } else { &[] };
    up.iter().chain(&l.parked)
}

/// A document that comes along: a file — not a scratch, a review's deleted file, a `jdt://` class, or one
/// outside the project Claude's turn opened that you never took up (`Document::aside`, #37).
fn carried(d: &crate::document::Document) -> bool {
    d.path.is_some() && d.virtual_uri.is_none() && !d.aside
}

/// Session `l`'s part of the state: `docs` (its files, `current` the one it shows), its panes, side panel,
/// chat, and whether its shell was open (it can't come along — the next tarae says so).
fn session_json(l: &crate::session::Session, docs: &[&crate::document::Document], current: DocId) -> Value {
    let index = |id| docs.iter().position(|d| d.id == id);
    let files: Vec<Value> = docs
        .iter()
        .map(|d| {
            let sel = d.selection();
            let ranges: Vec<[usize; 2]> = sel.ranges().iter().map(|r| [r.anchor, r.head]).collect();
            json!({ "path": d.path, "sel": ranges, "primary": sel.primary_index(), "top": d.top, "left": d.left })
        })
        .collect();
    // (the focused pane shows the current document at its scroll — `sync_focus_view`, which a session
    // behind hasn't had since it went behind)
    let shown = docs.iter().find(|d| d.id == current).map(|d| (d.id, d.top, d.left));
    let views: Vec<Value> = l
        .views
        .iter()
        .map(|v| {
            let (doc, top, left) = shown.filter(|_| v.id == l.focus).unwrap_or((v.doc, v.top, v.left));
            json!({ "id": v.id, "doc": index(doc), "top": top, "left": left })
        })
        .collect();
    let sidebar =
        l.sidebar.as_ref().and_then(|s| Some(json!({ "mode": mode_name(s.mode)?, "focus": s.focus })));
    let chat = l.chat.as_ref().map(|c| {
        json!({ "conversation": c.conversation, "input": c.input, "cursor": c.cursor, "focused": c.focused,
                "scroll": c.scroll, "follow": c.follow.on, "at_end": c.edits.at_end,
                "at_end_yours": c.edits.at_end_yours })
    });
    let on = docs.iter().find(|d| d.id == current).and_then(|d| d.path.as_deref());
    let turns = l.checkpoints.list.len();
    let review = l.code_review.as_ref().and_then(|r| crate::codereview::carried(r, on, turns));
    json!({
        "docs": files,
        "current": index(current),
        "views": views,
        "split": split_json(&l.split),
        "focus": l.focus,
        "sidebar": sidebar,
        "chat": chat,
        "review": review,
        "terminal": l.terminal.as_ref().is_some_and(|t| t.exited.is_none()),
    })
}

/// What came back, for the toast.
#[derive(Default)]
struct Back {
    sessions: usize,
    files: std::collections::HashSet<DocId>,
    /// Files and checkouts gone since — named, never dropped in silence.
    gone: Vec<String>,
    gone_checkouts: Vec<String>,
    /// Whose shell starts afresh: None the front's, else a session behind's name.
    shells: Vec<Option<String>>,
}

/// Back as it was (`snapshot`) — every session, the files that are still there — and what came back said:
/// the counts, then (the newest toast, wrapped whole) by name what didn't — every gone checkout, the gone
/// files (the first few), whose shell starts afresh. Number of files opened (none: the start screen, as a
/// plain start would show).
pub fn restore(ed: &mut Editor, v: &Value) -> usize {
    for (level, text) in symbols_said(v, std::process::id()) {
        crate::log::line("restart", level, &text);
    }
    let back = restore_quietly(ed, v);
    let mut said = vec!["Restarted".to_string()];
    if back.sessions > 1 {
        said.push(format!("{} sessions", back.sessions));
    }
    match back.files.len() {
        0 => {}
        1 => said.push("1 file as it was".into()),
        n => said.push(format!("{n} files as they were")),
    }
    if back.shells.contains(&None) {
        said.push("the terminal starts afresh (space T)".into());
    }
    ed.set_success(said.join(" · "));
    // By name — after, so it's the toast that wraps (the one before is a row of its own); each name one
    // word, so a row never ends inside one
    let mut named = Vec::new();
    let list = |names: &[String]| names.join(", ");
    match back.gone_checkouts.as_slice() {
        [] => {}
        [one] => named.push(format!("checkout gone: {one}")),
        all => named.push(format!("checkouts gone: {}", list(all))),
    }
    let files: Vec<String> = back.gone.iter().take(GONE_SHOWN).cloned().collect();
    match (files.len(), back.gone.len().saturating_sub(GONE_SHOWN)) {
        (0, _) => {}
        (1, _) => named.push(format!("file gone: {}", list(&files))),
        (_, 0) => named.push(format!("files gone: {}", list(&files))),
        (_, more) => named.push(format!("files gone: {} +{more}", list(&files))),
    }
    let shells: Vec<String> = back.shells.iter().flatten().cloned().collect();
    match shells.len() {
        0 => {}
        1 => named.push(format!("terminal starts afresh: {}", list(&shells))),
        _ => named.push(format!("terminals start afresh: {}", list(&shells))),
    }
    let gone = !back.gone_checkouts.is_empty() || !back.gone.is_empty();
    match (named.is_empty(), gone) {
        (true, _) => {}
        (false, false) => ed.set_status(named.join(" · ")),
        (false, true) => ed.set_warning(named.join(" · ")),
    }
    back.files.len()
}

/// Files gone since named on the restart's toast (the rest counted; checkouts are all named).
const GONE_SHOWN: usize = 4;

/// `:restart`'s state `v`, before the config reads the trust store (the editor isn't made yet): the sequence goes
/// on past what was given, and what it carries that narrows is marked (`trust::mark_carried` — N10).
pub fn before_config(v: &Value) {
    crate::storewrite::last_again(v["given_last"].as_u64().unwrap_or(0));
    crate::trust::mark_carried(v);
}

fn restore_quietly(ed: &mut Editor, v: &Value) -> Back {
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
    // What was taken back "until you quit" whose store write failed: still so (the store keeps it) — before any
    // `Caps` is made or a trust read (the sequence went on in `before_config`)
    for t in v["taking"].as_array().into_iter().flatten() {
        if let (Some(root), Some(what), Some(id)) = (t[0].as_str(), t[1].as_str(), t[2].as_u64()) {
            ed.taking(PathBuf::from(root), what.to_string(), id);
        }
    }
    crate::trust::taking_again(ed, &v["trust_taking"]);
    // (each written again, in the order given: their marks above hold them back until then)
    crate::trust::queue_again(ed, &v["store_writes"]["trust"]);
    let allow = v["store_writes"]["allow"].as_array().cloned().unwrap_or_default();
    crate::allow::keep_again(ed, &allow);
    if ed.project_trust.as_ref().is_some_and(|p| crate::trust::is_taking(&p.place)) {
        ed.project_trust_forgotten();
    }
    // The sessions, the one in front in front — and their take-backs not written, written again (F2, F5)
    let gone = crate::worktree::restore(ed, &v["sessions"]);
    crate::worktree::taken_again(ed, &v["store_writes"]["sessions"]);
    crate::codereview::viewed_restore(ed, &v["viewed"]);
    let mut back = Back::default();
    let blocks: Vec<&Value> =
        v["sessions"]["sessions"].as_array().into_iter().flatten().filter(|b| b["docs"].is_array()).collect();
    if blocks.is_empty() {
        // (an earlier build's state: every file in the front, as it was then)
        restore_session(ed, v, false, &mut back);
        if v["terminal"].as_bool() == Some(true) {
            back.shells.push(None);
        }
    } else {
        // Each session in its own (`with_session`), the front's last — the one in sight
        let front = ed.session.id;
        let at = |ed: &Editor, b: &Value| {
            let root = b["root"].as_str().map(Path::new)?;
            ed.sessions_in_order().iter().find(|l| l.root == root).map(|l| (l.id, l.label().to_string()))
        };
        let mut found: Vec<(SessionId, String, &Value)> =
            blocks.into_iter().filter_map(|b| at(ed, b).map(|(id, name)| (id, name, b))).collect();
        crate::sorting::by_key(&mut found, |(id, ..)| *id == front);
        for (id, name, b) in &found {
            let behind = *id != front;
            ed.with_session(*id, |ed| restore_session(ed, b, behind, &mut back));
            if b["terminal"].as_bool() == Some(true) {
                back.shells.push(behind.then(|| name.clone()));
            }
        }
        // Its cards asked again — each by its command, in its session, once every session is back (fresh
        // checks; a card that comes is parked there as any card behind)
        for (id, name, b) in &found {
            for line in b["cards"].as_array().into_iter().flatten().filter_map(|c| c["again"].as_str()) {
                let mut failed = None;
                ed.with_session(*id, |ed| failed = ask_again(ed, line).err());
                if let Some(why) = failed {
                    ed.set_warning(format!("{name}: :{line} not asked again — {why}"));
                }
            }
        }
    }
    back.gone_checkouts = gone;
    back.sessions = 1 + ed.sessions.len();
    back
}

/// A card's command run again (`OverRestart::Again`) — it was never answered, so it only ever asks: an end
/// or a delete with nothing to lose by now is asked on its card all the same, not done at once.
fn ask_again(ed: &mut Editor, line: &str) -> Result<(), String> {
    match line.split_once(' ') {
        Some(("session-end", which)) => ed.session_end_asking(which, true),
        Some(("git-branch-delete", branch)) => {
            ed.git_branch_delete_asking(branch, false, true);
            Ok(())
        }
        None if line == "preflight" => ed.preflight_start_asking(crate::preflight::Set::Quick, true),
        None if line == "preflight-full" => ed.preflight_start_asking(crate::preflight::Set::Full, true),
        // (`:session-bring` asks on its card first, whatever it finds)
        _ => crate::typed::execute(ed, line),
    }
}

/// One session's files, panes, side panel and chat back (`session_json`, or an earlier build's top-level
/// fields), in the session in front — swapped in for one `behind`, whose files' servers and claude wait
/// until it's used (`LspState.attach_later` → `Session.asleep`; a lazy chat).
fn restore_session(ed: &mut Editor, v: &Value, behind: bool, back: &mut Back) {
    // Saved index → the document it is now (outside the checkouts, a file's server waits until it's
    // shown — #141)
    let mut ids = Vec::new();
    (ed.lsp.unseen_later, ed.lsp.attach_later) = (true, behind);
    for d in v["docs"].as_array().into_iter().flatten() {
        let path = d["path"].as_str();
        let opened = path.filter(|p| Path::new(p).is_file()).and_then(|p| ed.open(Path::new(p)).ok());
        if opened.is_none()
            && let Some(name) = path.and_then(|p| Path::new(p).file_name()).map(|n| n.to_string_lossy())
            && !back.gone.iter().any(|g| *g == name)
        {
            back.gone.push(name.into_owned());
        }
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
    (ed.lsp.unseen_later, ed.lsp.attach_later) = (false, false);
    back.files.extend(ids.iter().flatten());
    if ids.iter().any(Option::is_some) {
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
                at_end_yours: chat["at_end_yours"].as_bool().unwrap_or(false),
                lazy: behind,
            },
        );
    }
    // Its code review (read again on a worker, in this session)
    if v["review"].is_object() {
        crate::codereview::reopen(ed, &v["review"]);
    }
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
        crate::sorting::sort(&mut leaves);
        let mut ids: Vec<_> = views.iter().map(|w| w.id).collect();
        crate::sorting::sort(&mut ids);
        if leaves == ids
            && let Some(f) = views.iter().find(|w| w.id == focus)
            && let Some(i) = ed.docs.iter().position(|d| d.id == f.doc)
        {
            // The focused pane is the current document at its scroll
            ed.current = i;
            (ed.docs[i].top, ed.docs[i].left) = (f.top, f.left);
            // Each pane an id of this tarae's own counter — the state's only tie its panes to its split (two
            // sessions' may be the same)
            let first = ed.next_view;
            ed.next_view += views.len() as u64;
            let new = |old| first + ids.iter().position(|&i| i == old).unwrap_or(0) as u64;
            let mut views = views;
            for v in &mut views {
                v.id = new(v.id);
            }
            let mut split = split;
            split.renumber(&new);
            (ed.session.views, ed.session.split, ed.session.focus) = (views, split, new(focus));
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
        crate::scratch::Scratch::new(&format!("restart-{name}")).into_path()
    }

    /// #136: a child of the last tarae still running at the restart (nothing here holds it) is waited for
    /// as it ends — not left a zombie. (The test's own `sleep`, its handle dropped as an exec drops it;
    /// `reap` itself waits for any child, so it isn't run in the test binary.)
    #[test]
    fn a_child_still_running_at_the_restart_is_reaped_when_it_ends() {
        let child = std::process::Command::new("sleep").arg("0.3").spawn().unwrap();
        let pid = child.id();
        drop(child);
        wait_for(vec![pid]);
        // SAFETY: signal 0 only asks whether the process is there (a zombie still is).
        let gone = || unsafe { libc::kill(pid as i32, 0) } != 0;
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !gone() && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(gone(), "reaped once it ended");
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

    /// With `TARAE_FRAMES_DIR` set, the screen as it is (100 × 30) in both built-in themes there,
    /// `<name>-meok.bin` / `<name>-hanji.bin` — for screenshots (`cargo xtask shot`: `TARAE_SHOT_REPLAY`).
    fn frames_both(ed: &mut Editor, name: &str) {
        let Some(dir) = std::env::var_os("TARAE_FRAMES_DIR") else { return };
        std::fs::create_dir_all(&dir).unwrap();
        // (crossterm drops every color under NO_COLOR, which tarae's test runner sets)
        crossterm::style::force_color_output(true);
        let (popups, was) = (std::mem::replace(&mut ed.offer_popups, true), ed.theme.name.clone());
        for theme in ["meok", "hanji"] {
            ed.theme = crate::theme::load(theme).unwrap();
            let mut buf = Vec::new();
            crate::term::render(ed, &mut buf, 100, 30).unwrap();
            let edges = ed.screen.edges.borrow();
            let bytes = crate::screen::Screen::new().present(&buf, 100, 30, &edges).to_vec();
            std::fs::write(Path::new(&dir).join(format!("{name}-{theme}.bin")), bytes).unwrap();
        }
        (ed.offer_popups, ed.theme) = (popups, crate::theme::load(&was).unwrap());
    }

    /// The restart's card up, naming what's under way (`what`), answered `w`: it waits for that to end.
    fn wait_on_card(ed: &mut Editor, what: &str) {
        let card =
            |ed: &Editor| ed.offers.iter().position(|o| matches!(o.what, crate::offer::What::Restart(_)));
        settle_until(ed, |ed| card(ed).is_some());
        let i = card(ed).expect("its card");
        let q = &ed.offers[i].question;
        assert!(q.contains(&format!("Under way — ended if it restarts now:\n  {what}")), "{q}");
        ed.offer_to_front(i);
        ed.offer_restart_wait();
        assert_eq!(ed.restart.as_ref().map(|r| &r.stage), Some(&Stage::Turn), "{}", toasts(ed));
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
                usage: None,
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

    /// A conversation of `rows` kept in `store` for the checkout in `root`, as claude's id `id`.
    fn keep_conversation(store: &Path, root: &Path, id: &str, said: &str) {
        use crate::chat::{Msg, Role};
        let msgs = [Msg::new(Role::User, "hello", ""), Msg::new(Role::Assistant, said, "")];
        let saved = crate::chats::Saved {
            id: id.into(),
            title: "hello".into(),
            updated: crate::chats::now(),
            rows: crate::chats::rows_of(&msgs),
            plan: None,
            usage: None,
            seq: crate::chats::seq(),
        };
        crate::chats::save(store, root, saved);
    }

    /// Two checkouts (`front`, `back`) in the test's folder, files in each; an editor started in `front`
    /// with a stand-in claude (its command lines go to `argv.log`) and conversations kept in `chats`.
    fn two_sessions(name: &str) -> (PathBuf, PathBuf, PathBuf, Editor) {
        let dir = temp(name);
        let (front, back) = (dir.join("front"), dir.join("back"));
        for (d, files) in [(&front, &["f.txt"][..]), (&back, &["a.txt", "b.txt", "c.md"])] {
            std::fs::create_dir_all(d).unwrap();
            for f in files {
                std::fs::write(d.join(f), "one\ntwo\nthree\n").unwrap();
            }
        }
        (front.clone(), back, dir.clone(), started_in(&dir, &front))
    }

    /// A new editor as `:restart`'s binary starts: in `front`, the test's stand-ins.
    fn started_in(dir: &Path, front: &Path) -> Editor {
        let mut ed = Editor::new(Config::default());
        (ed.session.root, ed.session.name) = (front.to_path_buf(), "front".into());
        ed.chats_dir = Some(dir.join("chats"));
        ed.config.llm.command = "sh".into();
        let argv = dir.join("argv.log");
        ed.config.llm.args =
            vec!["-c".into(), format!("printf '%s\\n' \"$*\" >> '{}'; cat > /dev/null", argv.display())];
        ed
    }

    fn session_at<'a>(ed: &'a Editor, root: &Path) -> &'a crate::session::Session {
        ed.sessions_in_order().into_iter().find(|l| l.root == root).expect("the session")
    }

    fn paths_of(ed: &Editor, l: &crate::session::Session) -> Vec<String> {
        let mut v: Vec<String> = ed
            .docs
            .iter()
            .filter(|d| l.docs.contains(&d.id))
            .filter_map(|d| Some(d.path.as_ref()?.file_name()?.to_string_lossy().into_owned()))
            .collect();
        v.sort();
        v
    }

    /// frictions14: restarted with another session in front — a checkout whose `.tarae.toml` is another
    /// version, without `[preflight]`. The watcher went by the session in front, so the next reload (here: a
    /// change to your config, as editing it does) read that checkout's file: the project's quick set gone, the
    /// bring card's `p` ran the packs' defaults. Now the config stays the checkout tarae started in's. Your
    /// config and the trust answers are stand-ins in the test's folder (`config_sources`), never yours.
    #[test]
    fn restarted_with_another_session_in_front_the_config_stays_the_started_checkouts() {
        use crate::config::{Sources, load};
        let (front, back, dir, mut ed) = two_sessions("config-root");
        let quick = "[preflight]\nsteps = [\n  { name = \"fmt\", cmd = \"true\" },\n  \
                     { name = \"perf\", cmd = \"true\", full = true },\n]\n";
        std::fs::write(front.join(".tarae.toml"), quick).unwrap();
        std::fs::write(back.join(".tarae.toml"), "[editor]\ntab-width = 3\n").unwrap();
        let (user, trust) = (dir.join("config.toml"), dir.join("trust.json"));
        std::fs::write(&user, "[editor]\nscrolloff = 1\n").unwrap();
        let sources = || Sources { user: Some(user.clone()), trust: Some(trust.clone()) };
        // The project trusted, as its card's `y` does
        let p = load(&front, &sources()).0.project.expect("its steps wait for trust");
        crate::trust::record(trust.clone(), crate::trust::Kind::Tarae, p.file, p.hash, true).unwrap();
        let started = |ed: &mut Editor| {
            (ed.config_file, ed.trust_path) = (Some(user.clone()), Some(trust.clone()));
            let (c, w) = load(&front, &ed.config_sources());
            ed.reload_config(c, w);
        };
        started(&mut ed);
        assert_eq!(ed.preflight_quick(), Some(vec!["fmt".to_string()]), "before: the project's quick set");
        // The other checkout in front, then `:restart`
        let id = ed.session_add(back.clone());
        assert!(ed.session_switch(id));
        let state = snapshot(&mut ed);
        drop(ed);
        let mut ed = started_in(&dir, &front);
        started(&mut ed);
        restore(&mut ed, &state);
        assert_eq!(ed.session.root, back, "the other checkout in front");
        // As the event loop starts: then your config changes (again, until the watch has seen it)
        ed.watches_start();
        let mut n = 1;
        while ed.config.scrolloff == 1 && n < 50 {
            n += 1;
            std::fs::write(&user, format!("[editor]\nscrolloff = {n}\n")).unwrap();
            let end = std::time::Instant::now() + Duration::from_millis(200);
            while ed.config.scrolloff == 1
                && let Some(ev) =
                    ed.events.recv_timeout(end.saturating_duration_since(std::time::Instant::now()))
            {
                ed.handle_event(ev);
            }
        }
        assert_ne!(ed.config.scrolloff, 1, "your config's change read");
        assert_eq!(ed.config.tab_width, 4, "not the checkout in front's .tarae.toml");
        assert_eq!(
            ed.preflight_quick(),
            Some(vec!["fmt".to_string()]),
            "after: still the project's quick set"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Wave 3, points 1–2: every session comes back as it was — not only the front's. Behind: its two files
    /// (in its own buffer list, none in the front's), the selection, its two panes, its side panel, its chat
    /// with its conversation and draft; the one in front in front.
    #[test]
    fn every_session_comes_back_as_it_was() {
        let (front, back, dir, mut ed) = two_sessions("every");
        keep_conversation(&dir.join("chats"), &back, "c-back", "Hello from back.");
        ed.open(&front.join("f.txt")).unwrap();
        let id = ed.session_add(back.clone());
        ed.with_session(id, |ed| {
            ed.open(&back.join("a.txt")).unwrap();
            ed.doc_mut().set_selection(Selection::single(Range::new(0, 3)));
            ed.split_view(Dir::Vertical);
            ed.open(&back.join("b.txt")).unwrap();
            ed.sidebar_show(Mode::Files);
            let mut c = crate::chat::Chat::new();
            (c.conversation, c.input, c.cursor) = (Some("c-back".into()), "half a thought".into(), 4);
            ed.session.chat = Some(c);
        });
        let state = snapshot(&mut ed);
        drop(ed);
        let mut ed = started_in(&dir, &front);
        restore(&mut ed, &state);
        assert_eq!(ed.session.root, front, "the front in front");
        assert_eq!(paths_of(&ed, &ed.session), ["f.txt"], "none of back's in the front's buffers");
        let l = session_at(&ed, &back);
        assert_eq!(paths_of(&ed, l), ["a.txt", "b.txt"]);
        let current = ed.docs.iter().find(|d| d.id == l.current_doc).unwrap();
        assert_eq!(current.path.as_deref(), Some(back.join("b.txt").as_path()), "its current file");
        let a = ed.docs.iter().find(|d| d.path.as_deref() == Some(back.join("a.txt").as_path())).unwrap();
        assert_eq!(a.selection().primary(), Range::new(0, 3));
        assert_eq!(l.split.leaves().len(), 2, "its two panes");
        assert_eq!(l.sidebar.as_ref().map(|s| s.mode), Some(Mode::Files), "its side panel");
        let c = l.chat.as_ref().expect("its chat");
        assert_eq!((c.input.as_str(), c.cursor), ("half a thought", 4), "its draft");
        settle_until(&mut ed, |ed| {
            let c = session_at(ed, &back).chat.as_ref().unwrap();
            c.msgs.iter().any(|m| m.text == "Hello from back.")
        });
        let c = session_at(&ed, &back).chat.as_ref().unwrap();
        assert_eq!(c.conversation.as_deref(), Some("c-back"), "its own conversation");
        assert!(ed.session.chat.is_none(), "none in front — there was none");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Wave 3, points 3–4: a session behind comes back with no language server and no claude — they start
    /// when it's used: its files' server when it comes to the front, its claude (resuming its conversation)
    /// with its first message.
    #[test]
    fn sessions_behind_start_no_server_and_no_claude() {
        let (front, back, dir, mut ed) = two_sessions("lazy");
        let cfg = "[lsp.mdfake]\ncommand = \"sh\"\nargs = [\"-c\", \"cat > /dev/null\"]\n\
                   [lang.markdown]\nlsp = [\"mdfake\"]\n";
        let (config, w) = crate::config::parse(cfg);
        assert!(w.is_empty(), "{w:?}");
        keep_conversation(&dir.join("chats"), &back, "c-back", "Hello from back.");
        let id = ed.session_add(back.clone());
        ed.with_session(id, |ed| {
            ed.open(&back.join("c.md")).unwrap();
            let mut c = crate::chat::Chat::new();
            c.conversation = Some("c-back".into());
            ed.session.chat = Some(c);
        });
        let state = snapshot(&mut ed);
        drop(ed);
        let mut ed = started_in(&dir, &front);
        ed.config.lsp = config.editor.lsp.clone();
        restore(&mut ed, &state);
        settle_until(&mut ed, |ed| {
            let c = session_at(ed, &back).chat.as_ref();
            c.is_some_and(|c| c.msgs.iter().any(|m| m.text == "Hello from back."))
        });
        let md = |ed: &Editor| {
            ed.docs
                .iter()
                .find(|d| d.path.as_deref() == Some(back.join("c.md").as_path()))
                .unwrap()
                .lsp
                .client
        };
        assert_eq!(md(&ed), None, "no server for a session behind");
        let c = session_at(&ed, &back).chat.as_ref().unwrap();
        assert!(!c.running(), "no claude for a chat behind");
        let argv = || std::fs::read_to_string(dir.join("argv.log")).unwrap_or_default();
        // To the front: its server; a message: its claude, resuming its conversation
        let id = session_at(&ed, &back).id;
        ed.session_switch(id);
        assert!(md(&ed).is_some(), "its server, now it's in front");
        assert!(!ed.session.chat.as_ref().unwrap().running(), "its claude waits for a message");
        let c = ed.session.chat.as_mut().unwrap();
        (c.input, c.cursor) = ("go on".into(), 5);
        crate::chat::send(&mut ed);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !argv().contains("--resume c-back") {
            assert!(std::time::Instant::now() < end, "resumed: {}", argv());
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(argv().lines().count(), 1, "one claude");
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Wave 3, point 5: before the exec, every session's conversation is kept on this thread — not only the
    /// front's (a worker's save would go with the process).
    #[test]
    fn every_sessions_conversation_is_kept_before_the_exec() {
        use crate::chat::{Msg, Role};
        let (front, back, dir, mut ed) = two_sessions("keep-all");
        let chat = |id: &str, said: &str| {
            let mut c = crate::chat::Chat::new();
            c.conversation = Some(id.into());
            c.msgs = vec![Msg::new(Role::User, said, ""), Msg::new(Role::Assistant, "ok", "")];
            c
        };
        ed.session.chat = Some(chat("c-front", "in front"));
        let id = ed.session_add(back.clone());
        ed.session_by_id(id).unwrap().chat = Some(chat("c-back", "behind"));
        crate::chat::keep_now(&ed);
        let store = dir.join("chats");
        let ids =
            |root: &Path| crate::chats::load(&store, root).into_iter().map(|s| s.id).collect::<Vec<_>>();
        assert_eq!(ids(&front), ["c-front"]);
        assert_eq!(ids(&back), ["c-back"], "the one behind too");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The state file goes both ways between builds: one an earlier build wrote (no session blocks) brings
    /// the files back in the front, as then; and the one written now still has today's top-level fields —
    /// what an earlier build reads after `f` on the restart card — with the front's files and panes.
    #[test]
    fn state_files_of_earlier_and_later_builds_restore() {
        let (front, back, dir, mut ed) = two_sessions("compat");
        ed.open(&front.join("f.txt")).unwrap();
        let id = ed.session_add(back.clone());
        ed.with_session(id, |ed| ed.open(&back.join("a.txt")).unwrap());
        let state = snapshot(&mut ed);
        drop(ed);
        // Read as an earlier build reads it: the top-level fields
        assert_eq!(state["docs"].as_array().map(Vec::len), Some(2), "every file, as before");
        assert_eq!(state["views"].as_array().map(Vec::len), Some(1), "the front's panes");
        assert_eq!(state["sessions"]["front"], json!(front));
        // An earlier build's file: no blocks — the files open in the front, as then
        let mut old = state.clone();
        for s in old["sessions"]["sessions"].as_array_mut().unwrap() {
            *s = json!({ "root": s["root"], "name": s["name"], "branch": s["branch"], "base": s["base"] });
        }
        let mut ed = started_in(&dir, &front);
        assert_eq!(restore(&mut ed, &old), 2);
        assert_eq!(paths_of(&ed, &ed.session), ["a.txt", "f.txt"], "an earlier build's: all in front");
        assert_eq!(ed.sessions.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Wave 3, point 11: the toast says what came back — the sessions, the files — and names what didn't: a
    /// file gone, a checkout gone, each session's shell that starts afresh.
    #[test]
    fn the_restart_toast_says_what_came_back() {
        let (front, back, dir, mut ed) = two_sessions("toast");
        let old = dir.join("old");
        std::fs::create_dir_all(&old).unwrap();
        ed.open(&front.join("f.txt")).unwrap();
        let id = ed.session_add(back.clone());
        ed.with_session(id, |ed| {
            ed.open(&back.join("a.txt")).unwrap();
            ed.open(&back.join("b.txt")).unwrap();
        });
        ed.session_add(old.clone());
        let mut state = snapshot(&mut ed);
        drop(ed);
        let blocks = state["sessions"]["sessions"].as_array_mut().unwrap();
        let back_block = blocks.iter_mut().find(|b| b["root"] == json!(back)).unwrap();
        back_block["terminal"] = json!(true);
        std::fs::remove_file(back.join("b.txt")).unwrap();
        std::fs::remove_dir_all(&old).unwrap();
        let mut ed = started_in(&dir, &front);
        restore(&mut ed, &state);
        let said: Vec<&str> = ed.messages.iter().map(|m| m.2.as_str()).collect();
        let at = said.iter().position(|m| m.starts_with("Restarted")).expect("said");
        assert_eq!(
            &said[at..],
            [
                "Restarted · 2 sessions · 2 files as they were",
                "checkout gone: old · file gone: b.txt · terminal starts afresh: back"
            ],
            "the counts, then what didn't come back by name"
        );
        assert!(!said.iter().any(|m| m.contains("removed outside tarae")), "said once, there: {said:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Panes come back with ids of this tarae's own counter — two sessions whose panes had the same ids in
    /// the state (an earlier build's, or a hand-edited file) don't share one; no two views anywhere do.
    #[test]
    fn no_two_views_share_an_id_after_a_restart() {
        let (front, back, dir, mut ed) = two_sessions("view-ids");
        ed.open(&front.join("f.txt")).unwrap();
        ed.split_view(Dir::Vertical);
        let id = ed.session_add(back.clone());
        ed.with_session(id, |ed| {
            ed.open(&back.join("a.txt")).unwrap();
            ed.split_view(Dir::Horizontal);
            ed.open(&back.join("b.txt")).unwrap();
        });
        let mut state = snapshot(&mut ed);
        drop(ed);
        // The same ids in both sessions' blocks
        let blocks = state["sessions"]["sessions"].as_array_mut().unwrap();
        let ids_of =
            |b: &Value| b["views"].as_array().unwrap().iter().map(|v| v["id"].clone()).collect::<Vec<_>>();
        let front_ids = ids_of(&blocks[0]);
        let behind = blocks.iter_mut().find(|b| b["root"] == json!(back)).unwrap();
        for (v, id) in behind["views"].as_array_mut().unwrap().iter_mut().zip(&front_ids) {
            v["id"] = id.clone();
        }
        behind["split"] = json!({ "h": front_ids });
        behind["focus"] = front_ids[1].clone();
        let mut ed = started_in(&dir, &front);
        restore(&mut ed, &state);
        let all: Vec<u64> =
            ed.sessions_in_order().iter().flat_map(|l| l.views.iter().map(|v| v.id)).collect();
        let mut distinct = all.clone();
        distinct.sort_unstable();
        distinct.dedup();
        assert_eq!(all.len(), distinct.len(), "every view's own id: {all:?}");
        for l in ed.sessions_in_order() {
            let mut leaves = l.split.leaves();
            leaves.sort_unstable();
            let mut ids: Vec<u64> = l.views.iter().map(|v| v.id).collect();
            ids.sort_unstable();
            assert_eq!((leaves.len(), &leaves), (2, &ids), "{}: its two panes, its split over them", l.name);
            assert!(ids.contains(&l.focus), "{}: its focus one of them", l.name);
        }
        assert!(ed.next_view > *distinct.last().unwrap(), "the next one past them all");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The restart's toast on a narrow screen (60 columns): what came back first, then what didn't — every
    /// gone checkout said, no name cut across rows or by an ellipsis.
    #[test]
    fn the_restart_toast_reads_whole_on_a_narrow_screen() {
        let (front, back, dir, mut ed) = two_sessions("toast-narrow");
        let gone: Vec<PathBuf> = ["login-fix", "old-parser"].iter().map(|n| dir.join(n)).collect();
        for g in &gone {
            std::fs::create_dir_all(g).unwrap();
        }
        ed.open(&front.join("f.txt")).unwrap();
        let id = ed.session_add(back.clone());
        ed.with_session(id, |ed| {
            ed.open(&back.join("a.txt")).unwrap();
            ed.open(&back.join("b.txt")).unwrap();
        });
        for g in &gone {
            ed.session_add(g.clone());
        }
        let mut state = snapshot(&mut ed);
        drop(ed);
        let blocks = state["sessions"]["sessions"].as_array_mut().unwrap();
        blocks.iter_mut().find(|b| b["root"] == json!(back)).unwrap()["terminal"] = json!(true);
        std::fs::remove_file(back.join("b.txt")).unwrap();
        for g in &gone {
            std::fs::remove_dir_all(g).unwrap();
        }
        let mut ed = started_in(&dir, &front);
        restore(&mut ed, &state);
        let (w, h) = (60, 24);
        let mut buf = Vec::new();
        crate::term::render(&mut ed, &mut buf, w, h).unwrap();
        let mut vt = vt100::Parser::new(h, w, 0);
        vt.process(&buf);
        let rows: Vec<String> = vt.screen().rows(0, w).collect();
        let (x, y, cw, ch) = ed.screen.toast_card.get().expect("the toast is up");
        let card: Vec<String> =
            rows[y..y + ch].iter().map(|r| r.chars().skip(x).take(cw).collect::<String>()).collect();
        let shown = card.join("\n");
        // The newest toast: from its bar to the next one's
        let first = card.iter().position(|r| r.contains('▎')).expect("a toast");
        let end =
            card[first + 1..].iter().position(|r| r.contains('▎')).map_or(card.len(), |i| first + 1 + i);
        let newest = &card[first..end];
        assert!(!newest.iter().any(|r| r.contains('…')), "nothing of it cut:\n{shown}");
        for name in ["login-fix", "old-parser", "b.txt", "back"] {
            assert!(newest.iter().any(|r| r.contains(name)), "{name:?} whole on a row:\n{shown}");
        }
        assert!(shown.contains("Restarted"), "the counts too:\n{shown}");
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

    /// #205, end to end: `:restart` typed while Claude's card is up — each key through `handle_event`, which
    /// marks what it starts as asked by it — and its card, brought back by the jobs that key started (saved,
    /// the checkouts looked up on a worker), comes in front of Claude's and is the inbox's first. Claude's stays,
    /// unanswered. (The build is `true`, the binary a stand-in in the test's own folder; nothing is answered.)
    #[test]
    fn restarts_card_typed_while_claudes_is_up_comes_in_front_of_it() {
        use crate::offer::{Offer, What};
        let dir = temp("in-front");
        let bin = fake_tarae(&dir, "tarae 9.9.9");
        let mut ed = editing(&dir, "true", &bin);
        let what = What::Screen { call: crate::tools::Call::test(1) };
        ed.push_offer(Offer::new("Claude asks".into(), "to see the screen", "", String::new(), what));
        let claudes = ed.offers[0].id;
        for k in [":", "r", "e", "s", "t", "a", "r", "t", "ret"] {
            ed.handle_event(crate::event::Event::Key(k.parse().unwrap()));
        }
        let mine = |ed: &Editor| ed.offers.iter().find(|o| matches!(o.what, What::Restart(_))).map(|o| o.id);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while mine(&ed).is_none() {
            let ev = ed.events.recv_timeout(end.saturating_duration_since(std::time::Instant::now()));
            ed.handle_event(ev.unwrap_or_else(|| panic!("no restart card: {}", toasts(&ed))));
        }
        let ids: Vec<u64> = ed.offers.iter().map(|o| o.id).collect();
        assert_eq!(ids, [mine(&ed).unwrap(), claudes], "yours in front, Claude's stacked");
        assert_eq!(ed.inbox().first().and_then(|e| e.card()), mine(&ed), "and the inbox's first");
        ed.restart = None;
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Before it becomes the binary it built, the restart keeps that binary's dSYM (`dsyms::keep`, on a
    /// worker) — one it can't keep (here: the stand-in, in a cargo build's folder, is a script, not a Mach-O)
    /// doesn't stop the restart. No dSYM beside the binary: nothing to keep. Either way the state carries it
    /// and the next tarae's log says it — where they went, with its pid, or why not.
    #[test]
    fn the_binarys_symbols_are_kept_and_one_that_cant_be_doesnt_stop_it() {
        let dir = temp("dsym");
        let release = dir.join("target/release");
        std::fs::create_dir_all(&release).unwrap();
        std::fs::write(dir.join("target/CACHEDIR.TAG"), "").unwrap();
        std::fs::write(release.join(".cargo-lock"), "").unwrap();
        let bin = fake_tarae(&release, "tarae 9.9.9");
        std::fs::create_dir_all(release.join("tarae.dSYM/Contents/Resources/DWARF")).unwrap();
        let mut ed = editing(&dir, "true", &bin);
        feed(&mut ed, ":restart<ret>");
        settle_answering(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        let r = ed.restart.as_ref().expect("still restarting");
        assert_eq!(r.ready(), Some(bin.as_path()), "{}", toasts(&ed));
        assert!(
            matches!(&r.kept, Some(Err(e)) if e.contains(&bin.display().to_string()) && e.contains("not a 64-bit Mach-O")),
            "{:?}",
            r.kept
        );
        let said = symbols_said(&snapshot(&mut ed), 42);
        assert!(
            matches!(said.as_slice(), [("warning", s)] if s.starts_with("symbols of this tarae (pid 42) not kept: ")
                && s.contains("not a 64-bit Mach-O")),
            "{said:?}"
        );
        // Kept, its older ones not all removed: where, then why not
        let at = dir.join("target/dsyms/A.dSYM");
        ed.restart.as_mut().unwrap().kept =
            Some(Ok(Some(crate::dsyms::Kept { at: at.clone(), pruned: Err("X.dSYM: not a folder".into()) })));
        assert_eq!(
            symbols_said(&snapshot(&mut ed), 42),
            [
                ("info", format!("symbols of this tarae (pid 42): {}", at.display())),
                ("warning", "older kept symbols not removed: X.dSYM: not a folder".into()),
            ]
        );

        std::fs::remove_dir_all(release.join("tarae.dSYM")).unwrap();
        let mut ed = editing(&dir, "true", &bin);
        feed(&mut ed, ":restart<ret>");
        settle_answering(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        let r = ed.restart.as_ref().expect("still restarting");
        assert_eq!((r.ready(), &r.kept), (Some(bin.as_path()), &Some(Ok(None))), "{}", toasts(&ed));
        let state = snapshot(&mut ed);
        assert_eq!((&state["symbols"], symbols_said(&state, 42)), (&Value::Null, vec![]), "nothing to say");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A slow disk doesn't hold the screen: `:restart` saves and goes on once the files are written (the
    /// saver's event, not a wait) — and `:restart` again meanwhile, `y` on its card, cancels it.
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
        feed(&mut ed, ":restart!<ret>:restart!<ret>y");
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

    /// What's half written stops it with why, as `:q` would: a commit message, a debug run. A task running
    /// doesn't (frictions15: its card names it — `a_sessions_review_and_running_command_dont_hold_a_restart`),
    /// nor a shell in the terminal — the next tarae says it starts afresh.
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
        assert_eq!(blocker(&ed, true), None, "a run is named on its card, not a refusal");
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

    /// Wave 3, gap 3: only the front session's were looked at — a task or tests running behind were killed by
    /// the exec without a word, a commit message or a review open there lost. Every session's half-written
    /// work stops it now, named; with nothing in the way anywhere, it goes on. Runs and a turn's review don't
    /// stop it (frictions15): runs are named on its card, reviews come along.
    #[test]
    fn whats_running_in_a_session_behind_stops_the_restart_by_name() {
        let dir = temp("blocked-behind");
        let bin = fake_tarae(&dir, "tarae 9.9.9");
        let mut ed = editing(&dir, "", &bin);
        let back = dir.join("fix-login");
        std::fs::create_dir_all(&back).unwrap();
        std::fs::write(back.join("b.txt"), "one\n").unwrap();
        let id = ed.session_add(back.clone());
        let try_it = |ed: &mut Editor, why: &str| {
            let err = crate::typed::execute(ed, "restart!").unwrap_err();
            assert!(err.contains(&format!("fix-login: {why}")), "{err}");
            assert!(ed.restart.is_none());
        };
        // An install (stopped half-way it can leave a tool broken)
        let task = crate::task::Task { name: "rust-analyzer".into(), cmd: "true".into(), cwd: back.clone() };
        let mut run = crate::task::Run::for_test(task);
        (run.state, run.install) = (crate::task::State::Running, true);
        ed.session_by_id(id).unwrap().task_run = Some(run);
        try_it(&mut ed, "rust-analyzer is installing — let it finish");
        ed.session_by_id(id).unwrap().task_run = None;
        // A commit message, an edit's review
        let doc = ed.doc().id;
        ed.session_by_id(id).unwrap().committing =
            Some(crate::gitpanel::Committing { doc, top: back.clone(), amend: false, then_end: false });
        try_it(&mut ed, "a commit message is open");
        ed.session_by_id(id).unwrap().committing = None;
        ed.with_session(id, |ed| {
            ed.open(&back.join("b.txt")).unwrap();
            let doc = ed.doc().id;
            crate::llm::review_with(ed, doc, "x", vec![(0, 3, "one".into(), "uno".into())], None);
        });
        assert!(ed.session_by_id(id).unwrap().review.is_some());
        try_it(&mut ed, "a review is open");
        ed.session_by_id(id).unwrap().review = None;
        // Nothing anywhere: on its way
        crate::typed::execute(&mut ed, "restart!").unwrap();
        assert!(ed.restart.is_some());
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

    /// A binary that can't be run: exec returns why, the state it wrote for it is gone again, and the process
    /// is as it was — std's exec first sets the process up as a fork's child would be (SIGPIPE at its default,
    /// no signal blocked), so a write to a closed pipe after a failed exec killed it. In the test binary that
    /// was every run that came after: a fake claude gone, its writer's EPIPE a SIGPIPE, the run dead
    /// ("signal: 13"). Run in a process of its own (`failed_exec_in_its_own_process`), so this one's signals
    /// are never touched.
    #[test]
    fn a_failed_exec_says_why_and_leaves_the_process_as_it_was() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "restart::tests::failed_exec_in_its_own_process", "--ignored", "--nocapture"])
            .env("TARAE_FAILED_EXEC", "1")
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&out.stdout).to_string() + &String::from_utf8_lossy(&out.stderr);
        use std::os::unix::process::ExitStatusExt as _;
        assert_eq!(out.status.signal(), None, "it died of a signal (13 = SIGPIPE): {said}");
        assert!(out.status.success(), "{said}");
        assert!(said.contains("1 passed"), "the child ran it: {said}");
    }

    /// `a_failed_exec_says_why_and_leaves_the_process_as_it_was`'s child.
    #[test]
    #[ignore]
    fn failed_exec_in_its_own_process() {
        if std::env::var_os("TARAE_FAILED_EXEC").is_none() {
            return;
        }
        let pipe_disposition = || {
            // SAFETY: a query only.
            unsafe {
                let mut now: libc::sigaction = std::mem::zeroed();
                libc::sigaction(libc::SIGPIPE, std::ptr::null(), &mut now);
                now.sa_sigaction
            }
        };
        let blocked = || {
            // SAFETY: a query of this thread's mask only.
            unsafe {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut set);
                libc::sigismember(&set, libc::SIGUSR2)
            }
        };
        // As tarae runs: SIGPIPE ignored (std's main does it); and one signal blocked, and a descriptor
        // made without close-on-exec (a pty's, say), to see them come back
        assert_eq!(pipe_disposition(), libc::SIG_IGN);
        let mut fds = [0; 2];
        // SAFETY: blocking one signal on this thread; a pipe(2) into a two-int array, no close-on-exec.
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();
            libc::sigemptyset(&mut set);
            libc::sigaddset(&mut set, libc::SIGUSR2);
            libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
            assert_eq!(libc::pipe(fds.as_mut_ptr()), 0);
        }
        let dir = temp("exec");
        let file = dir.join("restart-state.json");
        let said = exec_with(Editor::new(Config::default()), &dir.join("no-such-tarae"), &file);
        assert!(said.starts_with("couldn't run ") && said.contains("no-such-tarae"), "{said}");
        assert!(!file.exists());
        std::fs::remove_dir_all(&dir).ok();
        // A write into a pipe whose reader is gone: an error, not the end of this process
        // SAFETY: closing the reading end made above, then writing a byte into the other.
        let wrote = unsafe {
            libc::close(fds[0]);
            libc::write(fds[1], b"x".as_ptr().cast(), 1)
        };
        assert_eq!(wrote, -1);
        assert_eq!(std::io::Error::last_os_error().raw_os_error(), Some(libc::EPIPE));
        assert_eq!(pipe_disposition(), libc::SIG_IGN, "SIGPIPE is ignored again");
        assert_eq!(blocked(), 1, "the signal mask is back");
        // SAFETY: reading a descriptor's flags.
        let flags = unsafe { libc::fcntl(fds[1], libc::F_GETFD) };
        assert_eq!(flags & libc::FD_CLOEXEC, 0, "a descriptor's close-on-exec is as it was");
    }

    /// Claude's edit waiting in its review is part of the turn — `:restart` names the turn on its card instead
    /// of refusing, and `w` waits. Once accepted, the restart goes on only after Claude's write has landed
    /// (until then auto-save leaves the file to Claude, and it would count as unsaved).
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
        wait_on_card(&mut ed, "Claude's turn");
        feed(&mut ed, "y");
        settle_answering(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert_eq!(ed.restart.as_ref().and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        assert!(toasts(&ed).contains("once Claude's edits are on disk"), "it waited: {}", toasts(&ed));
        assert!(!ed.doc().is_modified(), "Claude's write was the save");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "TWO\n");
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A turn's files still being read (review at the end) are the turn's — named, and `w` waits for them; then
    /// its review doesn't open over the restart — the chat says `space v t`, which works after it.
    #[test]
    fn a_turn_being_gathered_is_waited_for() {
        let dir = temp("gather");
        let bin = fake_tarae(&dir, "tarae 9.9.9");
        let mut ed = Editor::new(Config::default());
        ed.session.root = dir.clone();
        ed.config.restart_binary = bin.display().to_string();
        ed.session.checkpoints.gathering = true;
        crate::typed::execute(&mut ed, "restart!").unwrap();
        wait_on_card(&mut ed, "Claude's turn");
        turn_over(&mut ed);
        assert_eq!(ed.restart.as_ref().unwrap().stage, Stage::Turn, "still reading");
        let file = dir.join("a.txt");
        std::fs::write(&file, "two\n").unwrap();
        ed.session.checkpoints.gathering = false;
        ed.session.checkpoints.list.push(crate::checkpoint::Checkpoint::new(
            "q".into(),
            vec![crate::checkpoint::Touched {
                path: file,
                before: Some(ropey::Rope::from_str("one\n")),
                after: ropey::Rope::from_str("two\n"),
                unseen: true,
                ..Default::default()
            }],
        ));
        crate::chat::turn_gathered(&mut ed, Some("a.txt".into()));
        assert!(ed.session.code_review.is_none(), "not over the restart");
        settle_answering(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert_eq!(ed.restart.as_ref().and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A chat turn in flight is named on the card; `w` waits for it — the restart goes on when it ends (its
    /// card again); `:restart` again, `y` on its card, cancels.
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
        wait_on_card(&mut ed, "Claude's turn");
        crate::typed::execute(&mut ed, "restart!").unwrap();
        // (the chat has the keys: the card's are given)
        feed(&mut ed, "<C-q>y");
        assert!(ed.restart.is_none(), "again, then y = canceled");
        crate::typed::execute(&mut ed, "restart!").unwrap();
        wait_on_card(&mut ed, "Claude's turn");
        assert!(toasts(&ed).contains("tarae restarts once these end: Claude's turn"), "{}", toasts(&ed));
        settle_answering(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert!(!ed.session.chat.as_ref().unwrap().busy(), "only after the turn");
        assert_eq!(ed.restart.as_ref().and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// An editor in `dir` restarting onto a stand-in tarae, its chat open with a stand-in claude that never
    /// answers — and `restart!`'s card, nothing under way unless `busy` (Claude in a turn).
    fn restart_card_with_chat(dir: &Path, busy: bool) -> (Editor, PathBuf) {
        let bin = fake_tarae(dir, "tarae 9.9.9");
        let mut ed = Editor::new(Config::default());
        ed.session.root = dir.to_path_buf();
        ed.config.restart_binary = bin.display().to_string();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        crate::chat::open(&mut ed);
        if busy {
            ed.session.chat.as_mut().unwrap().state = crate::chat::State::Thinking;
        }
        crate::typed::execute(&mut ed, "restart!").unwrap();
        settle_until(&mut ed, |ed| restart_card(ed).is_some());
        (ed, bin)
    }

    /// The restart's card up, if it is — its question.
    fn restart_card(ed: &Editor) -> Option<String> {
        let o = ed.offers.iter().find(|o| matches!(o.what, crate::offer::What::Restart(_)))?;
        Some(o.question.clone())
    }

    /// Restart anyway with Claude in a turn: the turn is named on the card and `y` doesn't wait for it —
    /// and its edits waiting for an answer, which the restart drops with it (the claude that asked is gone
    /// after it), are named too, by file (the reviewer: `y` dropped one, the card saying only "Claude's
    /// turn"). Ones that came while the card was up aren't on it: `y` asks again, naming them.
    #[test]
    fn restart_anyway_ends_claudes_turn_and_names_its_edit() {
        let dir = temp("anyway-turn");
        let file = dir.join("a.txt");
        std::fs::write(&file, "one\n").unwrap();
        std::fs::write(dir.join("b.txt"), "one\n").unwrap();
        let (mut ed, bin) = restart_card_with_chat(&dir, true);
        let q = restart_card(&ed).unwrap();
        assert!(q.lines().any(|l| l == "  Claude's turn"), "{q}");
        // Claude asks to write two files while the card is up
        for (id, name) in [("e1", "a.txt"), ("e2", "b.txt")] {
            ed.session.chat.as_mut().unwrap().edits.queue.push_back(crate::edits::Pending {
                request_id: id.into(),
                tool: "Write".into(),
                input: json!({ "file_path": dir.join(name), "content": "TWO\n" }),
                reason: String::new(),
                since: Instant::now(),
                protected: Some(false),
            });
        }
        ed.offer_accept();
        assert!(
            toasts(&ed).contains("something began since the card asked — it asks again"),
            "{}",
            toasts(&ed)
        );
        let q = restart_card(&ed).expect("asked again");
        let dropped = "Claude's turn — its 2 unanswered edits to a.txt, b.txt are dropped";
        assert!(q.lines().any(|l| l == format!("  {dropped}")), "{q}");
        // The first opens in its review meanwhile: named all the same
        ed.handle_event(crate::event::Event::Resize);
        assert!(ed.session.review.is_some(), "a.txt's edit in its review");
        settle_answering(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert_eq!(ed.restart.as_ref().and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        assert!(ed.session.chat.as_ref().unwrap().busy(), "the turn wasn't waited for");
        assert!(toasts(&ed).contains(&format!("restarting anyway — it ends {dropped}")), "{}", toasts(&ed));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "one\n", "the edit wasn't made");
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Restart anyway with Claude in a turn whose accepted edit is still being written: the write is waited
    /// for (until then the file would count as unsaved), and on the way the turn the card named isn't (the
    /// wait's `sync` lets what it was told to end go on). The wait is the real one (`agent_will_write`), ended
    /// as its job ends it (`agent_write_missed`) once the restart waits on it — never by the clock: a 300 ms
    /// window ran out while the card's check ran the stand-in binary, and the restart went on unwaiting (1 in 5).
    #[test]
    fn restart_anyway_waits_for_claudes_write_not_its_turn() {
        let dir = temp("anyway-write");
        let file = dir.join("a.txt");
        std::fs::write(&file, "one\n").unwrap();
        let (mut ed, bin) = restart_card_with_chat(&dir, true);
        // Claude's accepted edit, its write on the way
        ed.open(&file).unwrap();
        let edit = crate::transaction::Transaction::new(vec![crate::transaction::Change::insert(0, "TWO\n")]);
        ed.doc_mut().apply(&edit);
        ed.doc_mut().agent_will_write();
        let (id, v) = (ed.doc().id, ed.doc().version());
        let i = ed.offers.iter().position(|o| matches!(o.what, crate::offer::What::Restart(_))).unwrap();
        ed.offer_to_front(i);
        ed.offer_accept();
        settle_until(&mut ed, |ed| ed.restart.as_ref().is_some_and(|r| r.stage == Stage::Writes));
        assert!(toasts(&ed).contains("once Claude's edits are on disk"), "it waited: {}", toasts(&ed));
        // (the write never came: the wait's end saves it as accepted)
        ed.agent_write_missed(id, v);
        settle_until(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert_eq!(ed.restart.as_ref().and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        assert!(toasts(&ed).contains("once Claude's edits are on disk"), "it waited: {}", toasts(&ed));
        assert!(ed.session.chat.as_ref().unwrap().busy(), "the turn wasn't waited for");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "TWO\none\n", "written before it restarts");
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A turn asked after the card was answered wasn't on it: it's waited for, said — not ended with the
    /// restart — and the restart goes on once it ends.
    #[test]
    fn a_turn_asked_after_the_answer_is_waited_for() {
        let dir = temp("turn-after");
        let (mut ed, bin) = restart_card_with_chat(&dir, false);
        assert!(!restart_card(&ed).unwrap().contains("Under way"));
        ed.offer_accept();
        // (the binary's check is on a worker: asked before its answer lands)
        ed.session.chat.as_mut().unwrap().state = crate::chat::State::Thinking;
        settle_until(&mut ed, |ed| ed.restart.as_ref().is_some_and(|r| r.stage == Stage::Turn));
        assert!(toasts(&ed).contains("tarae restarts once these end: Claude's turn"), "{}", toasts(&ed));
        ed.handle_event(crate::event::Event::Resize);
        assert_eq!(ed.restart.as_ref().unwrap().stage, Stage::Turn, "still at work");
        ed.session.chat.as_mut().unwrap().state = crate::chat::State::Idle;
        ed.handle_event(crate::event::Event::Resize);
        settle_answering(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert_eq!(ed.restart.as_ref().and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A run that starts while the card is up isn't on it (its list is taken when it's drawn): `y` asks
    /// again, naming it, instead of ending it unsaid; `y` then restarts anyway, said.
    #[test]
    fn a_run_begun_while_the_card_is_up_asks_again() {
        let dir = temp("run-began");
        let (mut ed, bin) = restart_card_with_chat(&dir, false);
        assert!(!restart_card(&ed).unwrap().contains("Under way"));
        let task = crate::task::Task { name: "long".into(), cmd: "sleep 30".into(), cwd: dir.clone() };
        let who = crate::tools::For::User("long".into());
        assert!(ed.task_start_for(task, who, None, Default::default()).is_ok());
        ed.offer_accept();
        assert!(
            toasts(&ed).contains("something began since the card asked — it asks again"),
            "{}",
            toasts(&ed)
        );
        let q = restart_card(&ed).expect("asked again");
        assert!(
            q.contains("Under way — ended if it restarts now:\n  sleep 30 in the task panel, 0:0"),
            "{q}"
        );
        settle_answering(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert_eq!(ed.restart.as_ref().and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        assert!(
            toasts(&ed).contains("restarting anyway — it ends sleep 30 in the task panel"),
            "{}",
            toasts(&ed)
        );
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
            // (plain git: there's no repository for tarae's — `gitrepo` — before the `init`)
            let out = std::process::Command::new("git").args(args).current_dir(dir).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        }
        crate::gitrepo::git(dir, &["rev-parse", "--short", "HEAD"], None).unwrap().trim().to_string()
    }

    /// Wave 3, point 8: a code review open in a session — of the uncommitted changes in front, since a
    /// branch behind — and its drafts don't stop a restart any more: it comes back in its session, against
    /// the same base (the branch's merge base as it was), on the file it was on; behind, its files' servers
    /// wait. A review of Claude's turn still stops it, named (its turns load later and would shift it).
    #[test]
    fn a_code_review_comes_back_with_its_base_and_file() {
        use crate::codereview::{Base, CodeReview};
        let (front, back, dir, mut ed) = two_sessions("review");
        let cfg = "[lsp.mdfake]\ncommand = \"sh\"\nargs = [\"-c\", \"cat > /dev/null\"]\n\
                   [lang.markdown]\nlsp = [\"mdfake\"]\n";
        let (config, w) = crate::config::parse(cfg);
        assert!(w.is_empty(), "{w:?}");
        let git = |d: &Path, args: &[&str]| crate::gitrepo::git(d, args, None).unwrap();
        std::fs::write(front.join("g.txt"), "g\n").unwrap();
        repo(&front);
        repo(&back);
        // In front: f.txt and g.txt changed, not committed · behind: a branch off main that changed c.md
        std::fs::write(front.join("f.txt"), "ONE\ntwo\nthree\n").unwrap();
        std::fs::write(front.join("g.txt"), "G\n").unwrap();
        git(&back, &["checkout", "-q", "-b", "feat"]);
        std::fs::write(back.join("c.md"), "# c\n").unwrap();
        git(&back, &["commit", "-q", "-am", "feat"]);
        let merge_base = git(&back, &["rev-parse", "main"]).trim().to_string();
        let reviewing = |ed: &Editor, root: &Path| {
            session_at(ed, root).code_review.as_ref().is_some_and(|r| !r.docs.is_empty())
        };
        crate::codereview::start(&mut ed, None);
        settle_until(&mut ed, |ed| reviewing(ed, &front));
        // In front, on its second file
        crate::codereview::show(&mut ed, 1, false);
        let id = ed.session_add(back.clone());
        ed.with_session(id, |ed| crate::codereview::start(ed, Some("main".into())));
        settle_until(&mut ed, |ed| reviewing(ed, &back));
        // Behind: its review's file closed meanwhile, another one shown (the review opens it again)
        ed.with_session(id, |ed| {
            let md = ed.doc().id;
            ed.open(&back.join("b.txt")).unwrap();
            ed.close_doc(md);
        });
        // A draft in front
        crate::notes::add(&mut ed, &front.join("f.txt"), 0..1, crate::notes::By::You, "why?");
        ed.session.notes.list[0].thread[0].draft = true;
        let blocked = |ed: &Editor| blocker(ed, true);
        assert_eq!(blocked(&ed), None, "a review and its drafts come along");
        let state = snapshot(&mut ed);
        drop(ed);
        let mut ed = started_in(&dir, &front);
        ed.config.lsp = config.editor.lsp.clone();
        restore(&mut ed, &state);
        settle_until(&mut ed, |ed| reviewing(ed, &front) && reviewing(ed, &back));
        let r = ed.session.code_review.as_ref().unwrap();
        assert_eq!((&r.base, r.files.len()), (&Base::Head, 2), "in front: the uncommitted changes");
        assert_eq!(ed.doc().path.as_deref(), Some(front.join("g.txt").as_path()), "on the file it was on");
        let l = session_at(&ed, &back);
        let r = l.code_review.as_ref().unwrap();
        assert_eq!(r.base, Base::Branch { name: "main".into(), commit: merge_base }, "behind: since main");
        let current = ed.docs.iter().find(|d| d.id == l.current_doc).unwrap();
        assert_eq!(current.path.as_deref(), Some(back.join("c.md").as_path()), "its file, opened again");
        assert!(r.docs.contains(&current.id), "in the review");
        assert_eq!(current.lsp.client, None, "behind: its server waits");
        // Nothing said for them over the restart's toasts
        let said: Vec<&str> = ed.messages.iter().map(|m| m.2.as_str()).collect();
        assert!(!said.iter().any(|m| m.contains("review:")), "{said:?}");
        // A turn's review comes along too (frictions15 — `a_turns_review_comes_back_after_a_restart`)
        let turn = Base::Turn { at: 0, question: "do it".into() };
        ed.session.code_review = Some(CodeReview::for_test(turn, front.clone(), Vec::new()));
        assert_eq!(blocked(&ed), None);
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Wave 3, point 10: a card of tarae's a command raised comes back after the restart by being asked
    /// again — the command run anew in its session (fresh checks), its card parked there as any card behind.
    /// Here the git tab's delete of a branch with commits on no other, asked in a session behind.
    #[test]
    fn a_cards_command_asks_again_after_restart() {
        use crate::offer::What;
        let (front, back, dir, mut ed) = two_sessions("again");
        let git = |args: &[&str]| crate::gitrepo::git(&back, args, None).unwrap();
        repo(&back);
        git(&["checkout", "-q", "-b", "side"]);
        std::fs::write(back.join("a.txt"), "only on side\n").unwrap();
        git(&["commit", "-q", "-am", "side"]);
        git(&["checkout", "-q", "main"]);
        let id = ed.session_add(back.clone());
        ed.with_session(id, |ed| crate::typed::execute(ed, "git-branch-delete side").unwrap());
        let asked = |ed: &Editor| {
            session_at(ed, &back).parked.iter().any(|o| matches!(o.what, What::BranchDelete { .. }))
        };
        settle_until(&mut ed, asked);
        let state = snapshot(&mut ed);
        let block =
            state["sessions"]["sessions"].as_array().unwrap().iter().find(|b| b["root"] == json!(back));
        assert_eq!(
            block.unwrap()["cards"],
            json!([{ "again": "git-branch-delete side" }]),
            "how to ask again"
        );
        drop(ed);
        let mut ed = started_in(&dir, &front);
        restore(&mut ed, &state);
        settle_until(&mut ed, asked);
        assert!(
            ed.offers.iter().all(|o| !matches!(o.what, What::BranchDelete { .. })),
            "behind, not in front"
        );
        assert_eq!(git(&["branch", "--list", "side"]).trim(), "side", "asked, nothing deleted");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A card asked again was never answered: asking again never acts. The branch merged meanwhile — its
    /// commits on another now, so `:git-branch-delete` would delete it at once — still gets its card.
    #[test]
    fn a_branch_delete_asked_again_asks_even_when_nothing_would_be_lost() {
        use crate::offer::What;
        let (front, back, dir, mut ed) = two_sessions("again-merged");
        let git = |args: &[&str]| crate::gitrepo::git(&back, args, None).unwrap();
        repo(&back);
        git(&["checkout", "-q", "-b", "side"]);
        std::fs::write(back.join("a.txt"), "only on side\n").unwrap();
        git(&["commit", "-q", "-am", "side"]);
        git(&["checkout", "-q", "main"]);
        let id = ed.session_add(back.clone());
        ed.with_session(id, |ed| crate::typed::execute(ed, "git-branch-delete side").unwrap());
        let asked = |ed: &Editor| {
            session_at(ed, &back).parked.iter().any(|o| matches!(o.what, What::BranchDelete { .. }))
        };
        settle_until(&mut ed, asked);
        let state = snapshot(&mut ed);
        drop(ed);
        // Meanwhile: merged — nothing would be lost now
        git(&["merge", "-q", "--ff-only", "side"]);
        let mut ed = started_in(&dir, &front);
        restore(&mut ed, &state);
        let gone = || git(&["branch", "--list", "side"]).trim().is_empty();
        settle_until(&mut ed, |ed| asked(ed) || gone());
        assert!(!gone(), "deleted without its card");
        assert!(asked(&ed));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// …and a session whose end was asked, with nothing to lose by the time the restart lands (its change
    /// gone meanwhile — `:session-end` would end it at once), still gets its card.
    #[test]
    fn a_session_end_asked_again_asks_even_when_nothing_would_be_lost() {
        use crate::offer::What;
        let (front, _, dir, mut ed) = two_sessions("again-end");
        let git = |d: &Path, args: &[&str]| crate::gitrepo::git(d, args, None).unwrap();
        repo(&front);
        let wt = dir.join("wt");
        git(&front, &["worktree", "add", "-q", "-b", "wt", wt.to_str().unwrap()]);
        let id = ed.session_add(wt.clone());
        ed.session_by_id(id).unwrap().base = Some("main".into());
        // Something to lose: a new file
        std::fs::write(wt.join("new.txt"), "mine\n").unwrap();
        crate::typed::execute(&mut ed, "session-end wt").unwrap();
        let asked = |ed: &Editor| ed.offers.iter().any(|o| matches!(o.what, What::SessionEnd(_)));
        settle_until(&mut ed, asked);
        let state = snapshot(&mut ed);
        drop(ed);
        // Meanwhile: gone — nothing to lose now
        std::fs::remove_file(wt.join("new.txt")).unwrap();
        let mut ed = started_in(&dir, &front);
        restore(&mut ed, &state);
        let ended =
            |ed: &Editor| ed.messages.iter().any(|m| m.2.contains("Ended wt") || m.2.contains("end wt:"));
        settle_until(&mut ed, |ed| asked(ed) || ended(ed));
        assert!(wt.exists() && !ended(&ed), "ended without its card");
        assert!(asked(&ed));
        drop(ed);
        git(&front, &["worktree", "remove", "--force", wt.to_str().unwrap()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// …and the preflight's first-run card, with the project given trusted steps of its own meanwhile —
    /// `:preflight` would run them at once — is asked on its card all the same, nothing run.
    #[test]
    fn a_preflight_asked_again_asks_even_when_it_would_just_run() {
        use crate::offer::{Offer, What};
        use crate::preflight::{Set, Step};
        let (front, _, dir, mut ed) = two_sessions("again-preflight");
        let step = |name: &str, cmd: &str, from: &str| Step {
            name: name.into(),
            cmd: cmd.into(),
            note: None,
            from: from.into(),
            full: false,
        };
        let steps = vec![step("check", "true", "rust pack")];
        let what = What::Preflight { root: front.clone(), steps, set: Set::Quick };
        ed.push_offer(Offer::new("Run these checks?".into(), "…", "", String::new(), what));
        let state = snapshot(&mut ed);
        drop(ed);
        // Meanwhile: the project's own steps, trusted (a stand-in: a marker in the test's folder)
        let marker = dir.join("ran");
        let mut ed = started_in(&dir, &front);
        ed.config.preflight = vec![step("mine", &format!("touch '{}'", marker.display()), ".tarae.toml")];
        restore(&mut ed, &state);
        let asked = |ed: &Editor| ed.offers.iter().any(|o| matches!(o.what, What::Preflight { .. }));
        settle_until(&mut ed, |ed| asked(ed) || ed.session.task_run.is_some());
        assert!(ed.session.task_run.is_none() && !marker.exists(), "ran without its card");
        let o = ed.offers.iter().find(|o| matches!(o.what, What::Preflight { .. })).unwrap();
        assert!(o.question.contains("touch"), "the steps it would run: {}", o.question);
        // `y` there runs them
        ed.offer_accept();
        settle_until(&mut ed, |ed| ed.session.task_run.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// …and a card that can't come back (a moment's decision about something live) is named on the restart's
    /// card, with its session, before it's answered — those asked again by their command and those that come
    /// back by themselves aren't.
    #[test]
    fn the_restart_card_names_what_wont_come_back() {
        use crate::offer::{Offer, What};
        let (front, back, dir, mut ed) = two_sessions("lost");
        let id = ed.session_add(back.clone());
        let card = |title: &str, what| Offer::new(title.into(), "…", "", String::new(), what);
        ed.push_offer(card("Close a.txt?", What::CloseReviewed { doc: 0, force: false }));
        let tip = "0".repeat(40);
        ed.push_offer(card(
            "Delete side?",
            What::BranchDelete { top: front.clone(), branch: "side".into(), tip },
        ));
        let ended = What::ReviewEnd { session: id, started: std::time::Instant::now() };
        ed.session_by_id(id).unwrap().parked.push(card("End the review?", ended));
        // (a run of Claude's held after its last made a protected name: its job isn't run again blind)
        let made = What::ProtectedMade {
            then: Box::new(crate::tools::MadeThen::Claude {
                job: crate::tools::Job::Command("make".into(), back.clone()),
                call: crate::tools::Call::test(1),
                ride: Box::default(),
            }),
            made: vec![crate::snapshot::Made { path: "Makefile".into(), hash: String::new() }],
        };
        ed.session_by_id(id).unwrap().parked.push(card("Run it after it made Makefile?", made));
        // (a rebase held back by its checkout's changes: `:session-bring` raises it again, looking afresh)
        let moved = crate::rebase::Moved {
            root: back.clone(),
            name: "feat".into(),
            branch: "feat".into(),
            base: "main".into(),
            moved: 1,
            ahead: 1,
        };
        let held = crate::rebase::Held { moved, changed: vec!["a.txt".into()], unsaved: Vec::new() };
        ed.session_by_id(id).unwrap().parked.push(held.card());
        ed.restart = Some(Restart {
            id: 7,
            build: false,
            stage: Stage::Finding,
            target: None,
            kept: None,
            anyway: Vec::new(),
        });
        let t = Target {
            label: "front".into(),
            root: front.clone(),
            bin: dir.join("tarae"),
            head: None,
            head_time: None,
            bin_time: None,
        };
        ask(&mut ed, 7, vec![t]);
        let o = ed.offers.iter().find(|o| matches!(o.what, What::Restart(_))).expect("the restart's card");
        let q = &o.question;
        assert!(
            q.ends_with(
                "Won't come back — answer it first, or ask again after:\n  Close a.txt? (front)\n  End the \
                 review? (back)\n  Run it after it made Makefile? (back)\n  Rebase feat: changes in no commit \
                 (back)"
            ),
            "{q}"
        );
        assert!(!q.contains("Delete side?"), "asked again by its command: {q}");
        let lines: Vec<&str> = q.split('\n').collect();
        let warned = o.warning.clone().expect("in the warning colour");
        assert!(lines[warned].iter().all(|l| l.contains("come back") || l.ends_with(')')), "{q}");
        std::fs::remove_dir_all(&dir).ok();
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
    /// straight on. Its symbols are kept only once it goes ahead: not while the card asks, nor after `n`.
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
        assert_eq!(ed.restart.as_ref().map(|r| &r.kept), Some(&None), "not kept while the card asks");
        ed.offer_dismiss();
        assert!(ed.restart.is_none() && !ed.should_quit, "n: not restarting");
        // `y`: anyway
        crate::typed::execute(&mut ed, "restart").unwrap();
        settle_answering(&mut ed, |ed| same(ed) || ed.restart.is_none());
        ed.offer_accept();
        settle_until(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        let r = ed.restart.as_ref();
        assert_eq!(r.and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        assert_eq!(
            r.and_then(|r| r.kept.clone()),
            Some(Ok(None)),
            "kept once it went ahead (none beside it)"
        );
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

    /// frictions15 #7: one session's open review or side work doesn't hold the restart. `:restart` was refused
    /// for a turn's review open in another session ("end it first"), then waited, unsaid, on another session's
    /// running command. Now a running command (here a session behind's, the test's own `sleep`) is named on
    /// the card: `w` waits for it (said; once it ends the card asks again, nothing under way), `y` restarts
    /// anyway — said, and the run is ended (its process gone once the editor is). A turn's review behind
    /// doesn't stop it (`a_turns_review_comes_back_after_a_restart`).
    #[test]
    fn a_sessions_review_and_running_command_dont_hold_a_restart() {
        let (_, back, dir, mut ed) = two_sessions("not-held");
        let bin = fake_tarae(&dir, "tarae 9.9.9");
        ed.config.restart_binary = bin.display().to_string();
        use crate::codereview::{Base, CodeReview};
        let id = ed.session_add(back.clone());
        let turn = Base::Turn { at: 0, question: "do it".into() };
        ed.session_by_id(id).unwrap().code_review =
            Some(CodeReview::for_test(turn, back.clone(), Vec::new()));
        let long = |ed: &mut Editor| -> u32 {
            let mut pid = 0;
            ed.with_session(id, |ed| {
                let task =
                    crate::task::Task { name: "long".into(), cmd: "sleep 30".into(), cwd: back.clone() };
                let who = crate::tools::For::User("long".into());
                assert!(ed.task_start_for(task, who, None, Default::default()).is_ok());
                pid = ed.session.task_run.as_ref().unwrap().pid().unwrap();
            });
            pid
        };
        long(&mut ed);
        // Not refused; named on its card — `w` waits, said
        crate::typed::execute(&mut ed, "restart!").expect("not refused");
        let up = |ed: &Editor| ed.offers.iter().any(|o| matches!(o.what, crate::offer::What::Restart(_)));
        settle_until(&mut ed, up);
        frames_both(&mut ed, "restart_under_way");
        wait_on_card(&mut ed, "back: sleep 30 in the task panel, 0:0");
        let o = |ed: &Editor| {
            ed.offers.iter().find(|o| matches!(o.what, crate::offer::What::Restart(_))).cloned()
        };
        assert!(toasts(&ed).contains("tarae restarts once these end: back: sleep 30"), "{}", toasts(&ed));
        // It ends: the card again, nothing under way
        ed.with_session(id, |ed| ed.session.task_run = None);
        ed.handle_event(crate::event::Event::Resize);
        settle_until(&mut ed, |ed| o(ed).is_some());
        let card = o(&ed).unwrap();
        assert!(!card.question.contains("Under way"), "{}", card.question);
        assert_eq!(card.answers(false).iter().map(|a| a.0).collect::<String>(), "yn");
        ed.offer_dismiss();
        assert!(ed.restart.is_none());
        // Restart anyway: said, it goes ahead — and the run is ended
        let pid = long(&mut ed);
        crate::typed::execute(&mut ed, "restart!").unwrap();
        settle_until(&mut ed, |ed| o(ed).is_some());
        assert_eq!(o(&ed).unwrap().answers(false)[0], ('y', "Restart anyway"));
        settle_answering(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert_eq!(ed.restart.as_ref().and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        assert!(
            toasts(&ed).contains("restarting anyway — it ends back: sleep 30 in the task panel"),
            "{}",
            toasts(&ed)
        );
        drop(ed);
        let end = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while crate::procs::read(pid).is_some_and(|p| !p.zombie) && std::time::Instant::now() < end {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(crate::procs::read(pid).is_none_or(|p| p.zombie), "its run ended");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// frictions15 #7: a turn's review, open in a session behind, comes back after the restart as a code
    /// review does — the same turn (counted from the newest kept: the kept turns load after the restart,
    /// before any made since, and `at` alone would point at another), on the file it was on. It held the
    /// restart ("end it first"). A turn the restart ended mid-way (restart anyway, Claude at work) comes back
    /// after the kept ones as one more — not counted: found by its question and its place among the kept
    /// (the reviewer: counted from the list's end, it said "that turn isn't kept").
    #[test]
    fn a_turns_review_comes_back_after_a_restart() {
        let (front, back, dir, mut ed) = two_sessions("turn-back");
        let turns = dir.join("turns");
        ed.turns_dir = Some(turns.clone());
        let id = ed.session_add(back.clone());
        settle_until(&mut ed, |ed| session_at(ed, &back).checkpoints.dir.is_some());
        // Its two turns: the review is of the first ("loud"), on b.txt
        let touched = |name: &str, after: &str| crate::checkpoint::Touched {
            path: back.join(name),
            before: Some(ropey::Rope::from_str("one\ntwo\nthree\n")),
            after: ropey::Rope::from_str(after),
            ..Default::default()
        };
        std::fs::write(back.join("a.txt"), "ONE\ntwo\nthree\n").unwrap();
        std::fs::write(back.join("b.txt"), "one\nTWO\nthree\n").unwrap();
        std::fs::write(back.join("c.md"), "one\ntwo\nTHREE\n").unwrap();
        let cp = |q: &str, files| crate::checkpoint::Checkpoint::new(q.into(), files);
        ed.session_by_id(id).unwrap().checkpoints.list = vec![
            cp(
                "make it loud",
                vec![touched("a.txt", "ONE\ntwo\nthree\n"), touched("b.txt", "one\nTWO\nthree\n")],
            ),
            cp("and the notes", vec![touched("c.md", "one\ntwo\nTHREE\n")]),
        ];
        let reviewing =
            |ed: &Editor| session_at(ed, &back).code_review.as_ref().is_some_and(|r| !r.docs.is_empty());
        ed.with_session(id, |ed| crate::codereview::start_turn(ed, 0));
        settle_until(&mut ed, reviewing);
        ed.with_session(id, |ed| crate::codereview::show(ed, 1, false));
        // A third turn under way, cut by the restart: c.md changed since it began
        ed.session_by_id(id).unwrap().checkpoints.turn = vec![crate::checkpoint::Touched {
            path: back.join("c.md"),
            before: Some(ropey::Rope::from_str("one\ntwo\nTHREE\n")),
            after: ropey::Rope::from_str("one\ntwo\nTHREE\n"),
            ..Default::default()
        }];
        std::fs::write(back.join("c.md"), "one\ntwo\nTHREE, cut\n").unwrap();
        // (kept as its edits land — `keep_of` writes on a worker)
        crate::checkpoint::keep_now(session_at(&ed, &back));
        assert_eq!(blocker(&ed, true), None, "a turn's review doesn't stop it");
        // back's Claude still at work: restart anyway — its turn is cut
        let bin = fake_tarae(&dir, "tarae 9.9.9");
        ed.config.restart_binary = bin.display().to_string();
        ed.with_session(id, |ed| {
            crate::chat::open(ed);
            ed.session.chat.as_mut().unwrap().state = crate::chat::State::Thinking;
        });
        crate::typed::execute(&mut ed, "restart!").unwrap();
        settle_until(&mut ed, |ed| restart_card(ed).is_some());
        let q = restart_card(&ed).unwrap();
        assert!(q.lines().any(|l| l == "  back: Claude's turn"), "{q}");
        settle_answering(&mut ed, |ed| ed.should_quit || ed.restart.is_none());
        assert_eq!(ed.restart.as_ref().and_then(Restart::ready), Some(bin.as_path()), "{}", toasts(&ed));
        assert!(toasts(&ed).contains("restarting anyway — it ends back: Claude's turn"), "{}", toasts(&ed));
        let state = snapshot(&mut ed);
        drop(ed);
        let mut ed = started_in(&dir, &front);
        ed.turns_dir = Some(turns);
        restore(&mut ed, &state);
        settle_until(&mut ed, reviewing);
        let l = session_at(&ed, &back);
        let r = l.code_review.as_ref().unwrap();
        let crate::codereview::Base::Turn { at, question } = &r.base else { panic!("{:?}", r.base) };
        assert_eq!((*at, question.as_str()), (0, "make it loud"), "the same turn");
        assert_eq!(l.checkpoints.list[*at].question, "make it loud");
        let questions: Vec<&str> = l.checkpoints.list.iter().map(|c| c.question.as_str()).collect();
        assert_eq!(questions, ["make it loud", "and the notes", "a turn cut short"], "the cut one after");
        let current = ed.docs.iter().find(|d| d.id == l.current_doc).unwrap();
        assert_eq!(current.path.as_deref(), Some(back.join("b.txt").as_path()), "on the file it was on");
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #109: every session's turn is under way, not only the front's — named on the card, whose; `w` waits,
    /// and it goes on once the last one ends (it ended another session's turn, silently).
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
        wait_on_card(&mut ed, "back: Claude's turn");
        assert!(
            toasts(&ed).contains("tarae restarts once these end: back: Claude's turn"),
            "{}",
            toasts(&ed)
        );
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

    /// #194: a restart waiting for a turn stays in sight — the status line says whose turn it waits for —
    /// until it happens; `:restart` again (thinking the first did nothing) asks before calling it off: `n`
    /// keeps it waiting, `y` calls it off and the status line says nothing of it any more.
    #[test]
    fn a_waiting_restart_is_in_sight_and_again_asks_first() {
        let dir = temp("in-sight");
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
        let status = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 160, 30).unwrap();
            let mut vt = vt100::Parser::new(30, 160, 0);
            vt.process(&bytes);
            vt.screen().rows(0, 160).nth(29).unwrap()
        };
        assert!(!status(&mut ed).contains("restart"));
        crate::typed::execute(&mut ed, "restart!").unwrap();
        wait_on_card(&mut ed, "back: Claude's turn");
        ed.toasts.clear();
        // Long after its toast: still said
        let line = status(&mut ed);
        assert!(line.contains("restart waits for back"), "{line}");
        // Again: a card asks — nothing called off yet
        crate::typed::execute(&mut ed, "restart!").unwrap();
        assert_eq!(ed.restart.as_ref().map(|r| &r.stage), Some(&Stage::Turn), "asks first");
        let card = ed.offers.first().expect("a card asks");
        assert!(card.title.contains("Call off the restart"), "{}", card.title);
        assert!(card.question.contains("once these end: back: Claude's turn"), "{}", card.question);
        // (again while it asks: that card, not a second)
        crate::typed::execute(&mut ed, "restart!").unwrap();
        crate::typed::execute(&mut ed, "restart!").unwrap();
        assert_eq!(ed.offers.len(), 1, "one card");
        assert!(ed.status.as_ref().is_some_and(|s| s.0.contains("already asking")), "{:?}", ed.status);
        feed(&mut ed, "n");
        assert!(ed.offers.is_empty());
        assert_eq!(ed.restart.as_ref().map(|r| &r.stage), Some(&Stage::Turn), "n: still waits");
        assert!(status(&mut ed).contains("restart waits for back"));
        crate::typed::execute(&mut ed, "restart!").unwrap();
        feed(&mut ed, "y");
        assert!(ed.restart.is_none(), "y: called off");
        assert!(toasts(&ed).contains("restart canceled"), "{}", toasts(&ed));
        assert!(!status(&mut ed).contains("restart"));
        // (back's turn ends: nothing restarts)
        let back_chat = ed.sessions.iter_mut().find(|l| l.id == id).and_then(|l| l.chat.as_mut()).unwrap();
        back_chat.state = crate::chat::State::Idle;
        ed.handle_event(crate::event::Event::Resize);
        assert!(ed.restart.is_none() && !ed.should_quit);
        drop(ed);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// While its symbols are being kept (`Stage::Keeping`, a worker), the status line says so — and so does
    /// the card `:restart` again raises; `y` there calls it off.
    #[test]
    fn the_status_line_says_the_symbols_are_being_kept() {
        let mut ed = Editor::new(Config::default());
        let status = |ed: &mut Editor| {
            let mut bytes = Vec::new();
            crate::term::render(ed, &mut bytes, 160, 30).unwrap();
            let mut vt = vt100::Parser::new(30, 160, 0);
            vt.process(&bytes);
            vt.screen().rows(0, 160).nth(29).unwrap()
        };
        let stage = Stage::Keeping;
        ed.restart =
            Some(Restart { id: 7, build: false, stage, target: None, kept: None, anyway: Vec::new() });
        let line = status(&mut ed);
        assert!(line.contains("restarting — keeping its symbols"), "{line}");
        crate::typed::execute(&mut ed, "restart!").unwrap();
        let card = ed.offers.first().expect("a card asks");
        assert!(card.question.starts_with("restarting — keeping its symbols."), "{}", card.question);
        feed(&mut ed, "y");
        assert!(ed.restart.is_none(), "y: called off");
        assert!(!status(&mut ed).contains("restart"));
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
