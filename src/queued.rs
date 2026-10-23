//! Runs that wait for their slot — phase 1 of docs/design/async.md, "wait, don't refuse". A session has two
//! slots, the task panel and the test pane, each running one run at a time. A run asked while its slot is
//! busy waits in the session's line (`Session.runs_next`) instead of being refused, and starts when the slot
//! frees (`run_next`, from a run's end, close, or a start that came to nothing) — Claude's call waits with it
//! and its answer says how long (`waits::Waiting::said`).
//!
//! - **The user's own run goes first** (`push`): their task (`space m m`) no longer stops Claude's command
//!   (nor their tests Claude's tests) — it waits first in line, and `enter` in the panel runs it now, stopping
//!   Claude's (`run_now`).
//! - **One run, every caller answered** (`join`, §3.4): Claude sending the same command again (the same
//!   checkout, folder, words and grants) joins the one waiting, or the one under way while nothing in that
//!   checkout was saved or written since it started (`Session.wrote`) and nothing there is unsaved.
//! - **Seen** (§3.6): a row per waiting run in its panel (`rows` — `queued · 2nd · behind cargo test ·
//!   0:12`), `Doing::Queued` on the session's tab and in the list (`position`), and a row in `:procs`
//!   (`Editor::procs_queued`).
//!
//! Phase 2's editor-wide lineup replaces this per-session shape.

use std::collections::VecDeque;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::editor::Editor;
use crate::session::Session;
use crate::tools::{Call, Extra, For, Job};

/// Where a run runs: the task panel, or the test pane.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Task,
    Tests,
}

/// A call that joined another's run of the same command: answered with its result.
#[derive(Clone, Debug)]
pub struct Joined {
    pub call: Call,
    pub asked: Instant,
    /// It joined the run under way — how long that had run by then (None: it joined while that waited).
    pub running: Option<Duration>,
}

/// How long a run waited in line, and behind which runs (each once, in the order they ran).
#[derive(Clone, Debug)]
pub struct Queued {
    pub since: Instant,
    pub behind: Vec<String>,
}

/// What goes along with Claude's run from its ask to its end: whether its card said there's no sandbox, what
/// the user allowed it besides, its dedupe key, the calls that joined it, and its time in line.
#[derive(Clone, Debug, Default)]
pub struct Ride {
    pub unprotected: bool,
    pub extra: Extra,
    pub key: Option<String>,
    pub joined: Vec<Joined>,
    pub queued: Option<Queued>,
    /// The user's run of Claude's again (`For::User`): its trace from its place in line (Claude's own rides on
    /// its call).
    pub trace: Option<crate::trace::RunTrace>,
    /// The checkout as Claude's command (not a check) found it, taken right before it starts (`tools::go`,
    /// `snapshot.rs`).
    pub snap: Option<crate::snapshot::Before>,
    /// Its card's answer, kept in the session with what it runs as that snapshot found it (`tools::go`,
    /// `sinceyes.rs`) — no second snapshot, and the run takes its place in line as the answer comes.
    pub answered: Option<Box<crate::sinceyes::Answered>>,
    /// The card it ran from (its id), Claude's reason there for going past the sandbox, and what it warned of
    /// (`cardwarn.rs`) — said with where it ran (`chat::Ran`). `warned` None: no card — judged as it starts.
    pub card: Option<u64>,
    pub reason: Option<String>,
    pub warned: Option<Vec<crate::cardwarn::Warned>>,
}

/// A run waiting for its slot.
pub enum What {
    /// Claude's command or tests — or the user's run of Claude's again (`For::User`, `blocked.rs`): through
    /// the sandbox (`tools::guard_then_go`).
    Guarded { who: For, job: Job, ride: Ride },
    /// The user's task.
    Task(crate::task::Task),
    /// The user's tests.
    Tests(crate::testing::Target),
    /// A session's rebase the user asked for (`rebase.rs`, #188): looked at again when its turn comes
    /// (`stash`: its changes go into git's autostash).
    Rebase { moved: Box<crate::rebase::Moved>, stash: bool },
}

impl What {
    pub fn slot(&self) -> Slot {
        match self {
            What::Guarded { job: Job::Command(..), .. } | What::Task(_) | What::Rebase { .. } => Slot::Task,
            What::Guarded { job: Job::Tests(_), .. } | What::Tests(_) => Slot::Tests,
        }
    }

    /// The user's own (first in line), not Claude's.
    fn users(&self) -> bool {
        !matches!(self, What::Guarded { who: For::Claude(_), .. })
    }

    /// Claude's calls waiting on it: the one that asked, and those that joined.
    fn calls(&self) -> Vec<&Call> {
        match self {
            What::Guarded { who: For::Claude(c), ride, .. } => {
                std::iter::once(c).chain(ride.joined.iter().map(|j| &j.call)).collect()
            }
            _ => Vec::new(),
        }
    }

    /// What its row calls it: `claude · cargo clippy`, `build` (a task), `tests editor::tests::undo`.
    pub fn label(&self) -> String {
        match self {
            What::Guarded { who, job, .. } => {
                let job = match job {
                    Job::Command(cmd, _) => crate::task::traced(cmd),
                    Job::Tests(t) => format!("tests {}", t.label),
                };
                match who {
                    For::Claude(_) => format!("claude · {job}"),
                    For::User(_) => format!("again · {job}"),
                }
            }
            What::Task(t) => crate::task::traced(&t.name),
            What::Tests(t) => format!("tests {}", t.label),
            What::Rebase { moved, .. } => format!("rebase {} onto {}", moved.branch, moved.base),
        }
    }
}

/// A run in line: its id (`:procs`, the UI state's `run#<id>` — its trace's), what it is, its time there,
/// and its trace from the ask: the same run, the same id, once it starts.
pub struct Next {
    pub id: u64,
    pub what: What,
    pub queued: Queued,
    pub trace: crate::trace::RunTrace,
}

/// Whether `slot` is running a run now.
fn busy(l: &Session, slot: Slot) -> bool {
    match slot {
        Slot::Task => l.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running),
        Slot::Tests => l.test_run.as_ref().is_some_and(|r| r.state == crate::testing::RunState::Running),
    }
}

/// What runs in `slot` now, as a waiting row names it (`cargo test`, `tests editor::tests::undo`).
fn holder(l: &Session, slot: Slot) -> Option<String> {
    match slot {
        Slot::Task => l
            .task_run
            .as_ref()
            .filter(|r| r.state == crate::task::State::Running)
            .map(|r| crate::task::traced(&r.task.name)),
        Slot::Tests => l
            .test_run
            .as_ref()
            .filter(|r| r.state == crate::testing::RunState::Running)
            .map(|r| format!("tests {}", r.target.label)),
    }
}

/// Whether Claude's run in `slot` is what runs there now — what the user's own run waits behind rather
/// than stops.
pub fn claudes_running(l: &Session, slot: Slot) -> bool {
    match slot {
        Slot::Task => {
            l.task_run.as_ref().is_some_and(|r| r.by_claude && r.state == crate::task::State::Running)
        }
        Slot::Tests => {
            l.test_run.as_ref().is_some_and(|r| r.by_claude && r.state == crate::testing::RunState::Running)
        }
    }
}

/// The dedupe key of Claude's `job` in the checkout at `root` (§3.4): the folder it runs in and its words
/// (spaces collapsed) — or the test command — and what the user allowed it besides. Two checkouts never
/// share one.
pub fn key_of(root: &Path, job: &Job, extra: &Extra) -> String {
    let what = match job {
        Job::Command(cmd, cwd) => {
            format!("{}\0{}", cwd.display(), cmd.split_whitespace().collect::<Vec<_>>().join(" "))
        }
        Job::Tests(t) => format!("{}\0tests\0{}", t.cwd.display(), t.argv().join("\0")),
    };
    format!("{}\0{what}\0{extra:?}", root.display())
}

/// Claude's run, sandbox and all, asked (`tools::guard_and_go`): joined to the same run waiting or under way,
/// else into line when its slot is busy or others wait for it, else started.
pub fn admit(ed: &mut Editor, who: For, job: Job, mut ride: Ride) {
    let who = match who {
        For::Claude(call) => {
            let key = key_of(&ed.session.root, &job, &ride.extra);
            let call = match join(ed, &key, call) {
                Ok(()) => return,
                Err(call) => call,
            };
            ride.key = Some(key);
            For::Claude(call)
        }
        user => user,
    };
    let slot = What::Guarded { who: who.clone(), job: job.clone(), ride: Ride::default() }.slot();
    // (Claude's waits its turn behind what's in line; the user's goes first)
    let behind = matches!(who, For::Claude(_)) && ed.session.runs_next.iter().any(|n| n.what.slot() == slot);
    if busy(&ed.session, slot) || behind {
        return push(ed, What::Guarded { who, job, ride });
    }
    crate::tools::guard_then_go(ed, who, job, ride);
}

/// `call` joins Claude's run with this `key` — the one waiting, or the one under way when it still runs what
/// this would (nothing in the checkout saved or written since it started, nothing unsaved there). Err: none
/// to join, the call back.
pub fn join(ed: &mut Editor, key: &str, call: Call) -> Result<(), Call> {
    let now = Instant::now();
    let waiting = ed.session.runs_next.iter_mut().find_map(|n| match &mut n.what {
        What::Guarded { who: For::Claude(_), ride, .. } if ride.key.as_deref() == Some(key) => Some(ride),
        _ => None,
    });
    if let Some(ride) = waiting {
        call.step("joined the same run, waiting");
        ride.joined.push(Joined { call, asked: now, running: None });
        return Ok(());
    }
    let root = ed.session.root.clone();
    let unsaved =
        ed.docs.iter().any(|d| d.is_modified() && d.path.as_ref().is_some_and(|p| p.starts_with(&root)));
    let wrote = ed.session.wrote;
    let fresh = |started: Instant| !unsaved && wrote.is_none_or(|w| w < started);
    if let Some(r) = ed.session.task_run.as_mut().filter(|r| {
        r.state == crate::task::State::Running && r.key.as_deref() == Some(key) && fresh(r.started)
    }) {
        call.step("joined the same run, under way");
        r.joined.push(Joined { call, asked: now, running: Some(r.started.elapsed()) });
        return Ok(());
    }
    if let Some(r) = ed.session.test_run.as_mut().filter(|r| {
        r.state == crate::testing::RunState::Running && r.key.as_deref() == Some(key) && fresh(r.started)
    }) {
        call.step("joined the same run, under way");
        r.joined.push(Joined { call, asked: now, running: Some(r.started.elapsed()) });
        return Ok(());
    }
    Err(call)
}

/// Into line: Claude's at the end — or, `front`, back where it was (its slot was taken while it got ready);
/// the user's first, after their own already waiting (a task of theirs replaces the task waiting, tests the
/// tests).
pub fn push(ed: &mut Editor, what: What) {
    push_at(ed, what, false, None)
}

/// `push`, back at the front of its line — it was its turn, and its slot was taken while it got ready.
/// `trace`: the user's run's, from its ask (Claude's rides on its call).
pub fn again(ed: &mut Editor, what: What, trace: Option<crate::trace::RunTrace>) {
    push_at(ed, what, true, trace)
}

fn push_at(ed: &mut Editor, mut what: What, front: bool, trace: Option<crate::trace::RunTrace>) {
    let slot = what.slot();
    let holder = holder(&ed.session, slot);
    // (back in line: its time and what it waited behind go on)
    let earlier = match &mut what {
        What::Guarded { ride, .. } => ride.queued.take(),
        _ => None,
    };
    let mut queued = earlier.unwrap_or_else(|| Queued { since: crate::clock::now(), behind: Vec::new() });
    if let Some(h) = &holder
        && queued.behind.last() != Some(h)
    {
        queued.behind.push(h.clone());
    }
    let label = what.label();
    let users = what.users();
    // (Claude's call carries its trace; the user's run comes with one, or begins it here)
    let trace = match &what {
        What::Guarded { who: For::Claude(call), .. } => call.trace.clone(),
        What::Guarded { ride, .. } => ride.trace.clone(),
        _ => None,
    }
    .or(trace)
    .unwrap_or_else(|| {
        crate::trace::RunTrace::new(match &what {
            What::Task(t) => format!("task: {}", t.name),
            What::Tests(t) => format!("tests: {}", t.label),
            What::Rebase { .. } => label.clone(),
            What::Guarded { .. } => format!("{label}: run again"),
        })
    });
    trace.step("queued");
    let next = Next { id: trace.id(), what, queued, trace };
    let line = &mut ed.session.runs_next;
    if users {
        line.retain(|n| !mine(n, slot));
    }
    let at = match (users, front) {
        (true, _) => line.iter().position(|n| !n.what.users()).unwrap_or(line.len()),
        (false, true) => {
            line.iter().position(|n| !n.what.users() && n.what.slot() == slot).unwrap_or(line.len())
        }
        (false, false) => line.len(),
    };
    line.insert(at, next);
    let behind = holder.map(|h| format!(" behind {h}")).unwrap_or_default();
    match users {
        // (they asked for it and nothing started: said, with how to have it now)
        true => ed.set_status(format!("{label} waits{behind} — first in line")),
        false => ed.note(format!("{label} waits{behind}")),
    }
}

/// A slot freed (a run ended, was closed, or a start came to nothing): the first in its line starts — a call
/// of Claude's whose turn has ended is dropped on the way.
pub fn run_next(ed: &mut Editor) {
    crate::tools::left_sync(ed);
    for slot in [Slot::Task, Slot::Tests] {
        if busy(&ed.session, slot) {
            continue;
        }
        while let Some(i) = ed.session.runs_next.iter().position(|n| n.what.slot() == slot) {
            let Some(next) = ed.session.runs_next.remove(i) else { break };
            // (as `tools.rs` checks a turn: another chat process, or one done, waits for nothing)
            let wanted = next.what.calls().first().is_none_or(|c| {
                !ed.session.chat.as_ref().is_some_and(|ch| ch.generation() != c.generation() || !ch.busy())
            });
            if wanted {
                start(ed, next);
                break;
            }
        }
    }
}

fn start(ed: &mut Editor, next: Next) {
    for c in next.what.calls() {
        c.step("its turn");
    }
    match next.what {
        What::Guarded { who, job, mut ride } => {
            ride.queued = Some(next.queued);
            ride.trace = Some(next.trace);
            crate::tools::guard_then_go(ed, who, job, ride)
        }
        What::Task(t) => ed.task_start_now(t, false, Some(next.trace)),
        What::Tests(t) => ed.test_start_now(t, false, Some(next.trace)),
        // (its git run carries its own trace)
        What::Rebase { moved, stash } => {
            next.trace.end("its turn: the checkout looked at again");
            ed.rebase_check(*moved, stash)
        }
    }
}

/// A run started in `slot` (`name`, as rows say it): what waits there waits behind it too.
pub fn started(ed: &mut Editor, slot: Slot, name: &str) {
    for n in ed.session.runs_next.iter_mut().filter(|n| n.what.slot() == slot) {
        if n.queued.behind.last().map(String::as_str) != Some(name) {
            n.queued.behind.push(name.to_string());
        }
    }
}

/// The user's own run waiting in `slot`'s line — not Claude's (what `enter` on its panel's view runs now).
fn mine(n: &Next, slot: Slot) -> bool {
    n.what.slot() == slot && matches!(n.what, What::Task(_) | What::Tests(_))
}

/// The user's own run waits in `slot`'s line (`enter run now` on its panel's view).
pub fn mine_waits(ed: &Editor, slot: Slot) -> bool {
    ed.session.runs_next.iter().any(|n| mine(n, slot))
}

/// `enter` in the panel: the user's own run waiting in `slot` starts now — Claude's under way stops. False:
/// none of theirs waits there.
pub fn run_now(ed: &mut Editor, slot: Slot) -> bool {
    let Some(i) = ed.session.runs_next.iter().position(|n| mine(n, slot)) else {
        return false;
    };
    match ed.session.runs_next.remove(i).map(|n| (n.what, n.trace)) {
        Some((What::Task(t), trace)) => ed.task_start_now(t, true, Some(trace)),
        Some((What::Tests(t), trace)) => ed.test_start_now(t, true, Some(trace)),
        _ => {}
    }
    true
}

/// Run `id` leaves the line (`s` on its row in `:procs`): Claude's calls on it are told it didn't run, and
/// `why`.
pub fn leave(ed: &mut Editor, id: u64, why: &str) {
    let Some(i) = ed.session.runs_next.iter().position(|n| n.id == id) else { return };
    let Some(n) = ed.session.runs_next.remove(i) else { return };
    let label = n.what.label();
    if let What::Guarded { who: For::Claude(call), ride, .. } = n.what {
        for c in std::iter::once(call).chain(ride.joined.into_iter().map(|j| j.call)) {
            crate::tools::answer(ed, c, Err(format!("not run — {why}")));
        }
    }
    ed.set_status(format!("{label} — out of the line, not run"));
}

/// Claude's turn ended: its runs waiting go (nobody waits for them); the user's stay.
pub fn turn_over(ed: &mut Editor) {
    ed.session.runs_next.retain(|n| n.what.users());
}

/// A waiting run's row: what it is, where it is in line, behind what, for how long (`said`: `queued · 2nd ·
/// behind cargo test · 0:12`), and — the user's own — what `enter` stops to run it now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub label: String,
    pub pos: usize,
    pub behind: Option<String>,
    pub waited: Duration,
    pub stops: Option<String>,
}

impl Row {
    /// `queued · 2nd · behind cargo test · 12.3s` — without `behind` (a hint beside it names it already):
    /// `queued · 2nd · 12.3s`. The time a running one (`term::timer`, `calm` its whole seconds): it doesn't slide as it
    /// counts.
    pub fn said(&self, behind: bool, calm: bool) -> String {
        let holder =
            self.behind.as_ref().filter(|_| behind).map(|h| format!(" · behind {h}")).unwrap_or_default();
        format!("queued · {}{holder} · {}", ordinal(self.pos), crate::term::timer(self.waited, calm))
    }
}

/// The rows of what waits in session `l`'s `slot`, first in line first.
pub fn rows(l: &Session, slot: Slot, now: Instant) -> Vec<Row> {
    let holder = holder(l, slot);
    line(&l.runs_next, slot)
        .enumerate()
        .map(|(i, n)| Row {
            label: n.what.label(),
            pos: i + 1,
            behind: holder.clone(),
            waited: now.saturating_duration_since(n.queued.since),
            stops: matches!(n.what, What::Task(_) | What::Tests(_)).then(|| holder.clone()).flatten(),
        })
        .collect()
}

fn line(q: &VecDeque<Next>, slot: Slot) -> impl Iterator<Item = &Next> {
    q.iter().filter(move |n| n.what.slot() == slot)
}

/// Where session `l`'s Claude's first waiting call is in its line (1 = next) — its tab and its row in the
/// list say `queued 2nd` (`session::Doing::Queued`), not working nor stuck.
pub fn position(l: &Session) -> Option<usize> {
    let chat = l.chat.as_ref().filter(|c| c.busy())?;
    [Slot::Task, Slot::Tests]
        .into_iter()
        .filter_map(|slot| {
            line(&l.runs_next, slot)
                .position(|n| n.what.calls().iter().any(|c| c.generation() == chat.generation()))
                .map(|i| i + 1)
        })
        .min()
}

/// What Claude's call waiting in line waits behind, as the chat's live row says it (`chat_said`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChatWait {
    /// `queued 1st · behind cargo test · 0:12` — behind a run a stopped turn left, `queued 1st, behind a
    /// stopped turn's run`.
    pub said: String,
    /// That run: what it is, how long it has run, the key that stops it (`space m c`).
    pub left: Option<(String, String, &'static str)>,
}

/// Where session `l`'s Claude's first waiting call is in line and behind what, for the chat's live row
/// (frictions26: a run a stopped turn left going held the next turn's, and the chat said nothing). One no turn
/// waits on any more is said apart, with how it stops. Its times running ones (`term::timer`, `calm` its whole
/// seconds): the key after them doesn't slide as they count.
pub fn chat_said(l: &Session, now: Instant, calm: bool) -> Option<ChatWait> {
    let chat = l.chat.as_ref().filter(|c| c.busy())?;
    let (slot, pos, n) = [Slot::Task, Slot::Tests].into_iter().find_map(|slot| {
        line(&l.runs_next, slot)
            .enumerate()
            .find(|(_, n)| n.what.calls().iter().any(|c| c.generation() == chat.generation()))
            .map(|(i, n)| (slot, i + 1, n))
    })?;
    let waited = crate::term::timer(now.saturating_duration_since(n.queued.since), calm);
    let said = |s: String| ChatWait { said: s, left: None };
    let Some(holder) = holder(l, slot) else {
        return Some(said(format!("queued {} · {waited}", ordinal(pos))));
    };
    // (Claude's, its call gone: a turn that ended left it — `tools::turn_over`)
    let orphan = match slot {
        Slot::Task => l.task_run.as_ref().filter(|r| r.by_claude && r.chat.is_none()).map(|r| r.started),
        Slot::Tests => l.test_run.as_ref().filter(|r| r.by_claude && r.chat.is_none()).map(|r| r.started),
    };
    Some(match orphan {
        Some(started) => {
            let ran = crate::term::timer(now.saturating_duration_since(started), calm);
            let stop = if slot == Slot::Task { "space m c" } else { "space x c" };
            ChatWait {
                said: format!("queued {}, behind a stopped turn's run", ordinal(pos)),
                left: Some((holder, ran, stop)),
            }
        }
        None => said(format!("queued {} · behind {holder} · {waited}", ordinal(pos))),
    })
}

/// `1st` · `2nd` · `3rd` · `11th` …
pub fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

impl Editor {
    /// A file at `path` was saved or written (Claude's edit, a change set): its session's runs under way no
    /// longer run what's in the checkout — the same command asked from now on isn't joined to them.
    pub(crate) fn wrote(&mut self, path: &Path) {
        let Some(id) = self.session_of(path) else { return };
        let now = Instant::now();
        for l in std::iter::once(&mut self.session).chain(&mut self.sessions).filter(|l| l.id == id) {
            l.wrote = Some(now);
            // (a run under way whose changes are taken: this one isn't its)
            let snaps = [
                l.task_run.as_mut().and_then(|r| r.snap.as_mut()),
                l.test_run.as_mut().and_then(|r| r.snap.as_mut()),
            ];
            for s in snaps.into_iter().flatten() {
                s.wrote.push(path.to_path_buf());
            }
        }
    }

    /// What waits in every session's line, as `:procs` lists it: (its session, its id, what it is, where it
    /// is in line — `1st · behind cargo test · 0:12`; its age column says `queued`).
    pub fn procs_queued(&self) -> Vec<(crate::event::SessionId, u64, String, String)> {
        let now = Instant::now();
        let mut out = Vec::new();
        for l in std::iter::once(&self.session).chain(&self.sessions) {
            for slot in [Slot::Task, Slot::Tests] {
                let ids = line(&l.runs_next, slot).map(|n| n.id);
                for (id, r) in ids.zip(rows(l, slot, now)) {
                    // (whole seconds: `:procs` looks again each second, a tenth would stand still between)
                    let said = r.said(true, true).trim_start_matches("queued · ").to_string();
                    out.push((l.id, id, r.label, said));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use serde_json::json;

    use crate::editor::Editor;

    /// An editor in a fresh temp folder of its own, its chat at work (a stand-in claude that reads and says
    /// nothing) — `sh …` runs unasked, as an allowed check would — and the chat process's generation.
    fn at_work(name: &str) -> (Editor, PathBuf, u64) {
        let dir = crate::scratch::Scratch::new(&format!("queued-{name}")).into_path();
        let mut ed = Editor::new(crate::config::Config::default());
        ed.session.root = dir.clone();
        ed.config.llm.command = "sh".into();
        ed.config.llm.args = vec!["-c".into(), "cat > /dev/null".into()];
        ed.config.llm.run_allow = vec!["sh".into()];
        crate::chat::open(&mut ed);
        let c = ed.session.chat.as_mut().unwrap();
        c.state = crate::chat::State::Thinking;
        ed.session.at = crate::session::At::Code;
        let generation = c.generation();
        (ed, dir, generation)
    }

    /// Claude's `run` of `cmd` as call `id`, the way claude sends it — it waits (no answer now).
    fn run(ed: &mut Editor, generation: u64, id: u64, cmd: &str) {
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call",
                          "params": { "name": "run", "arguments": { "command": cmd } } });
        let now = crate::tools::mcp(ed, generation, &format!("r{id}"), &msg);
        assert!(now.is_none(), "{cmd}: answered at once — {now:?}");
    }

    /// Call `id`'s answer, once it came.
    fn answered(id: u64) -> Option<Result<String, String>> {
        crate::tools::ANSWERED
            .with(|a| a.borrow().iter().find(|(i, _)| *i == json!(id)).map(|(_, r)| r.clone()))
    }

    fn pump(ed: &mut Editor, what: &str, until: impl Fn(&Editor) -> bool) {
        let end = Instant::now() + Duration::from_secs(15);
        while !until(ed) {
            assert!(Instant::now() < end, "timed out: {what}");
            if let Some(ev) = ed.events.recv_timeout(Duration::from_millis(10)) {
                ed.handle_event(ev);
            }
        }
    }

    fn screen(ed: &mut Editor) -> String {
        let mut bytes = Vec::new();
        crate::term::render(ed, &mut bytes, 140, 36).unwrap();
        let mut vt = vt100::Parser::new(36, 140, 0);
        vt.process(&bytes);
        vt.screen().contents()
    }

    /// The lines of `order.txt` — each stand-in run writes its name there when it's done.
    fn order(dir: &Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("order.txt")).unwrap_or_default().lines().map(String::from).collect()
    }

    /// A stand-in script `name.sh`: waits `secs`, then writes its name in `order.txt`.
    fn script(dir: &Path, name: &str, secs: f32) {
        std::fs::write(dir.join(format!("{name}.sh")), format!("sleep {secs}\necho {name} >> order.txt\n"))
            .unwrap();
    }

    fn running(ed: &Editor) -> bool {
        ed.session.task_run.as_ref().is_some_and(|r| r.state == crate::task::State::Running)
    }

    /// Phase 1 (docs/design/async.md): a command that runs unasked while another runs waits in line — not
    /// refused — and starts when that one ends; meanwhile it has a row in the panel, its session reads as
    /// queued (not stuck, not plain working), `:procs` lists it, and Claude's answer says how long it waited
    /// and behind what.
    #[test]
    fn an_unasked_run_waits_its_turn_and_says_so() {
        let (mut ed, dir, generation) = at_work("unasked");
        script(&dir, "slow", 0.8);
        script(&dir, "fast", 0.0);
        run(&mut ed, generation, 1, "sh slow.sh");
        pump(&mut ed, "the first started", running);
        run(&mut ed, generation, 2, "sh fast.sh");
        assert_eq!(answered(2), None, "not refused");
        assert_eq!(ed.session.runs_next.len(), 1, "in line");
        assert_eq!(ed.doing(&ed.session), crate::session::Doing::Queued { pos: 1 });
        let s = screen(&mut ed);
        assert!(s.contains("claude · sh fast.sh   queued · 1st · behind sh slow.sh · "), "{s}");
        let procs = ed.procs_queued();
        assert!(
            matches!(&procs[..], [(_, _, l, w)] if l == "claude · sh fast.sh" && w.starts_with("1st · behind sh slow.sh"))
        );
        let mut rows = Vec::new();
        crate::procs::with_queued(&mut rows, &procs, &[]);
        assert!(matches!(rows[0].kind, crate::procs::Kind::Queued(_)) && rows[0].pid == 0);
        pump(&mut ed, "the second ran", |_| answered(2).is_some());
        assert_eq!(order(&dir), ["slow", "fast"], "one after the other");
        let text = answered(2).unwrap().unwrap();
        let second = text.lines().nth(1).unwrap_or_default();
        assert!(text.starts_with("exit 0"), "{text}");
        assert!(
            second.starts_with("Waited ") && second.contains(" queued behind sh slow.sh. Ran "),
            "{text}"
        );
        let waited = second["Waited ".len()..].split(" queued").next().and_then(crate::term::time_read);
        assert!(waited.unwrap() > std::time::Duration::from_millis(400), "{text}");
        // The first's answer has no wait in it
        assert!(!answered(1).unwrap().unwrap().contains("Waited"));
        assert!(ed.session.runs_next.is_empty() && ed.doing(&ed.session) == crate::session::Doing::Working);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A grant riding with a run that waits its turn goes with a take-back of it made meanwhile (verify3 F15): the
    /// run starts with what's allowed then.
    #[test]
    fn a_take_back_strips_the_grant_riding_with_a_run_in_line() {
        let (mut ed, dir, generation) = at_work("riding");
        script(&dir, "slow", 0.8);
        script(&dir, "fast", 0.0);
        run(&mut ed, generation, 1, "sh slow.sh");
        pump(&mut ed, "the first started", running);
        run(&mut ed, generation, 2, "sh fast.sh");
        fn riding(ed: &mut Editor) -> Option<&mut Option<crate::allow::Grant>> {
            match ed.session.runs_next.front_mut().map(|n| &mut n.what) {
                Some(super::What::Guarded { ride, .. }) => Some(&mut ride.extra.grant),
                _ => None,
            }
        }
        *riding(&mut ed).expect("in line") = Some(crate::allow::Grant::Port(8080));
        ed.sandbox_disallow("port 8080").unwrap();
        assert_eq!(riding(&mut ed).cloned(), Some(None), "the grant rides on past its take-back");
        pump(&mut ed, "the second ran", |_| answered(2).is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A take-back in one session strips the grant riding with another checkout's run in line too: the take-back
    /// reaches every session (piece J — a kept answer may be every checkout's, each conversation's copy goes).
    #[test]
    fn a_take_back_strips_another_sessions_riding_grant() {
        let (mut ed, dir, generation) = at_work("riding-other");
        script(&dir, "slow", 0.8);
        script(&dir, "fast", 0.0);
        run(&mut ed, generation, 1, "sh slow.sh");
        pump(&mut ed, "the first started", running);
        run(&mut ed, generation, 2, "sh fast.sh");
        let mut next = ed.session.runs_next.pop_front().expect("in line");
        if let super::What::Guarded { ride, .. } = &mut next.what {
            ride.extra.grant = Some(crate::allow::Grant::Port(8080));
        }
        // (in line in another checkout's session)
        let other = dir.join("other");
        std::fs::create_dir_all(&other).unwrap();
        let id = ed.session_add(other);
        let s = ed.sessions.iter_mut().find(|s| s.id == id).unwrap();
        s.runs_next.push_back(next);
        ed.sandbox_disallow("port 8080").unwrap();
        let s = ed.sessions.iter().find(|s| s.id == id).unwrap();
        let kept = s.runs_next.front().map(|n| match &n.what {
            super::What::Guarded { ride, .. } => ride.extra.grant.clone(),
            _ => None,
        });
        assert_eq!(kept, Some(None), "rides on in another session past its take-back");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Its turn came, and while it got ready (its sandbox, the saves) the panel was taken — it's back at the
    /// front of the line, not refused, and runs when the panel frees.
    #[test]
    fn a_run_whose_slot_was_taken_meanwhile_goes_back_in_line() {
        let (mut ed, dir, generation) = at_work("taken");
        script(&dir, "mine", 0.6);
        script(&dir, "claudes", 0.0);
        let call = crate::tools::Call::of(generation, "r1", json!(1));
        // (past the door already — `admit` saw the panel free)
        let job = crate::tools::Job::Command("sh claudes.sh".into(), dir.clone());
        ed.task_start(crate::task::Task { name: "mine".into(), cmd: "sh mine.sh".into(), cwd: dir.clone() });
        pump(&mut ed, "the user's task started", running);
        crate::tools::guard_then_go(&mut ed, crate::tools::For::Claude(call), job, Default::default());
        assert_eq!(answered(1), None, "not refused");
        assert_eq!(ed.session.runs_next.len(), 1);
        pump(&mut ed, "Claude's ran", |_| answered(1).is_some());
        assert_eq!(order(&dir), ["mine", "claudes"]);
        assert!(
            answered(1).unwrap().is_ok_and(|t| t.starts_with("exit 0") && t.contains("queued behind mine"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A test target that runs `sh -c script` (no parser reads it: the answer comes all the same).
    fn target(dir: &Path, label: &str, script: &str) -> crate::testing::Target {
        crate::testing::Target {
            label: label.into(),
            cwd: dir.to_path_buf(),
            how: crate::testing::How::Pack {
                lang: "python".into(),
                argv: vec!["sh".into(), "-c".into(), script.into()],
                debug: None,
                lines: crate::test_results::Rules::builtin("python"),
                build: None,
                debug_run: None,
                junit: None,
            },
        }
    }

    /// `run_tests` while tests run: the call waits for the pane, not refused ("tests are already running"),
    /// and its answer says it waited.
    #[test]
    fn tests_wait_for_the_tests_running() {
        let (mut ed, dir, generation) = at_work("tests");
        let go = |ed: &mut Editor, id: u64, t: crate::testing::Target| {
            let call = crate::tools::Call::of(generation, &format!("r{id}"), json!(id));
            let job = crate::tools::Job::Tests(Box::new(t));
            crate::tools::guard_and_go(ed, crate::tools::For::Claude(call), job, false, Default::default());
        };
        go(&mut ed, 1, target(&dir, "first", "sleep 0.6; echo first >> order.txt"));
        let testing = |ed: &Editor| {
            ed.session.test_run.as_ref().is_some_and(|r| r.state == crate::testing::RunState::Running)
        };
        pump(&mut ed, "the first started", testing);
        go(&mut ed, 2, target(&dir, "second", "echo second >> order.txt"));
        assert_eq!(answered(2), None, "not refused");
        assert_eq!(ed.doing(&ed.session), crate::session::Doing::Queued { pos: 1 });
        let s = screen(&mut ed);
        assert!(s.contains("claude · tests second   queued · 1st · behind tests first"), "{s}");
        pump(&mut ed, "the second ran", |_| answered(2).is_some());
        assert_eq!(order(&dir), ["first", "second"]);
        let text = answered(2).unwrap().unwrap();
        assert!(text.contains("queued behind tests first"), "{text}");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// §3.4: the same command sent again joins the one waiting (spaces aside) — one run, both answered; and the
    /// one under way, while nothing in the checkout was saved since it started. After a save, it's a run of
    /// its own.
    #[test]
    fn the_same_run_asked_again_is_one_run() {
        let (mut ed, dir, generation) = at_work("dedupe");
        script(&dir, "slow", 0.8);
        script(&dir, "fast", 0.0);
        run(&mut ed, generation, 1, "sh slow.sh");
        pump(&mut ed, "the first started", running);
        run(&mut ed, generation, 2, "sh fast.sh");
        run(&mut ed, generation, 3, "sh   fast.sh");
        assert_eq!(ed.session.runs_next.len(), 1, "one waits, the other joined it");
        // Under way: joined too
        run(&mut ed, generation, 4, "sh slow.sh");
        pump(&mut ed, "all answered", |_| (1..=4).all(|i| answered(i).is_some()));
        assert_eq!(order(&dir), ["slow", "fast"], "each ran once");
        for i in 1..=4 {
            assert!(answered(i).unwrap().is_ok_and(|t| t.starts_with("exit 0")), "{i}: {:?}", answered(i));
        }
        assert!(answered(3).unwrap().unwrap().contains("queued behind sh slow.sh"));
        assert!(answered(4).unwrap().unwrap().contains("Joined a run of the same command already under way"));
        // A file saved while it runs: the same command again is a run of its own
        std::fs::write(dir.join("order.txt"), "").unwrap();
        let file = dir.join("a.txt");
        std::fs::write(&file, "one\n").unwrap();
        ed.open(&file).unwrap();
        run(&mut ed, generation, 5, "sh slow.sh");
        pump(&mut ed, "it started", running);
        ed.handle_key("i".parse().unwrap());
        ed.handle_key("x".parse().unwrap());
        ed.handle_key("esc".parse().unwrap());
        for k in [":", "w", "ret"] {
            ed.handle_key(k.parse().unwrap());
        }
        assert!(ed.flush_saves());
        run(&mut ed, generation, 6, "sh slow.sh");
        assert_eq!(ed.session.runs_next.len(), 1, "not joined: it waits to run again");
        pump(&mut ed, "both answered", |_| answered(5).is_some() && answered(6).is_some());
        assert_eq!(order(&dir), ["slow", "slow"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// The user's task while Claude's command runs: Claude's isn't stopped — the task waits, first in line
    /// (ahead of Claude's next), its row offering to run it now; `enter` in the panel does, stopping Claude's.
    #[test]
    fn the_users_task_waits_first_in_line_and_can_run_now() {
        let (mut ed, dir, generation) = at_work("users");
        script(&dir, "slow", 0.8);
        script(&dir, "fast", 0.0);
        script(&dir, "mine", 0.0);
        let mine = crate::task::Task { name: "mine".into(), cmd: "sh mine.sh".into(), cwd: dir.clone() };
        run(&mut ed, generation, 1, "sh slow.sh");
        pump(&mut ed, "Claude's started", running);
        run(&mut ed, generation, 2, "sh fast.sh");
        ed.task_start(mine.clone());
        assert!(
            ed.session
                .task_run
                .as_ref()
                .is_some_and(|r| r.by_claude && r.state == crate::task::State::Running)
        );
        assert!(
            matches!(ed.session.runs_next.front().map(|n| &n.what), Some(super::What::Task(_))),
            "first in line"
        );
        // (its time read at a pinned clock: how long it has waited, as a running time says it)
        let held = crate::clock::hold();
        let waited = held.saturating_duration_since(ed.session.runs_next[0].queued.since);
        let s = screen(&mut ed);
        crate::clock::release();
        let row = s.lines().find(|l| l.trim_start().starts_with("mine ")).unwrap_or_default();
        let said = format!("   queued · 1st · {}", crate::term::timer(waited, false));
        assert!(row.contains(&said), "{said:?} in {s}");
        let time = row.split("queued · 1st · ").nth(1).and_then(|t| t.trim_start().split("  ").next());
        assert!(time.is_some_and(crate::term::reads_as_time), "{row}");
        assert!(s.contains("C-w j  enter run now — stops sh slow.sh"), "{s}");
        assert!(s.contains("claude · sh fast.sh   queued · 2nd"), "{s}");
        pump(&mut ed, "all ran", |ed| order(&dir).len() == 3 && !running(ed));
        assert_eq!(order(&dir), ["slow", "mine", "fast"]);
        assert!(answered(1).unwrap().is_ok_and(|t| t.starts_with("exit 0")), "Claude's ran to its end");
        // Run now: Claude's stops, the user's runs
        std::fs::write(dir.join("order.txt"), "").unwrap();
        run(&mut ed, generation, 3, "sh slow.sh");
        pump(&mut ed, "Claude's started", running);
        ed.task_start(mine);
        ed.session.at = crate::session::At::Slot;
        assert!(screen(&mut ed).contains("enter run now — stops sh slow.sh"));
        ed.handle_key("ret".parse().unwrap());
        pump(&mut ed, "the user's ran", |ed| order(&dir) == ["mine"] && !running(ed));
        assert!(answered(3).unwrap().is_err_and(|e| e.contains("stopped")), "{:?}", answered(3));
        assert!(ed.session.runs_next.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `s` on its row in `:procs`: a run waiting leaves the line — Claude's call is told it didn't run, and
    /// nothing runs.
    #[test]
    fn a_run_taken_out_of_line_is_told_it_did_not_run() {
        let (mut ed, dir, generation) = at_work("leave");
        script(&dir, "slow", 0.5);
        script(&dir, "fast", 0.0);
        run(&mut ed, generation, 1, "sh slow.sh");
        pump(&mut ed, "the first started", running);
        run(&mut ed, generation, 2, "sh fast.sh");
        let id = ed.session.runs_next.front().map(|n| n.id).unwrap();
        super::leave(&mut ed, id, "the user took it out of the line in :procs");
        assert!(
            answered(2).is_some_and(|r| r.is_err_and(|e| e.starts_with("not run — the user took it out")))
        );
        pump(&mut ed, "the first ended", |_| answered(1).is_some());
        assert_eq!(order(&dir), ["slow"], "the one taken out didn't run");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// frictions26: the user stops a turn while its run goes on (a run under way isn't cut short) — one light
    /// card offers to stop it; the next turn's run waits behind it, and the chat says behind what and how it
    /// stops; `y` stops it and the waiting one runs. A card whose run ends by itself goes.
    #[test]
    fn a_stopped_turns_run_is_offered_to_stop() {
        let (mut ed, dir, generation) = at_work("left");
        script(&dir, "slow", 5.0);
        script(&dir, "fast", 0.0);
        run(&mut ed, generation, 1, "sh slow.sh");
        pump(&mut ed, "it started", running);
        crate::chat::turn_ended(&mut ed, generation, true);
        let o = ed.offers.first().expect("a card");
        assert_eq!(o.kind(), "left-running");
        assert!(o.question.contains("sh slow.sh — the task panel, running "), "{}", o.question);
        assert!(!o.weighty(), "light: no settling");
        assert!(running(&ed), "not cut short");
        // The next turn's run waits behind it — the chat says so
        ed.session.chat.as_mut().unwrap().state = crate::chat::State::Thinking;
        run(&mut ed, generation, 2, "sh fast.sh");
        let said = super::chat_said(&ed.session, Instant::now(), false).unwrap();
        assert_eq!(said.said, "queued 1st, behind a stopped turn's run");
        assert!(
            said.left.as_ref().is_some_and(|(l, ran, k)| l == "sh slow.sh"
                && crate::term::reads_as_time(ran.trim_start())
                && *k == "space m c"),
            "{said:?}"
        );
        // `y`: stopped, and the one in line runs
        ed.handle_key("y".parse().unwrap());
        assert!(ed.offers.is_empty());
        pump(&mut ed, "the waiting one ran", |_| answered(2).is_some());
        assert_eq!(order(&dir), ["fast"], "the stopped turn's never finished");
        // A stopped turn's run that ends by itself takes its card with it
        script(&dir, "short", 0.3);
        run(&mut ed, generation, 3, "sh short.sh");
        pump(&mut ed, "it started", running);
        crate::chat::turn_ended(&mut ed, generation, true);
        assert_eq!(ed.offers.len(), 1);
        pump(&mut ed, "it ended", |ed| !running(ed));
        assert!(ed.offers.is_empty(), "gone with its run");
        // A turn that ends on its own offers nothing
        ed.session.chat.as_mut().unwrap().state = crate::chat::State::Thinking;
        script(&dir, "long", 2.0);
        run(&mut ed, generation, 4, "sh long.sh");
        pump(&mut ed, "it started", running);
        crate::chat::turn_ended(&mut ed, generation, false);
        assert!(ed.offers.is_empty(), "not stopped: no card");
        ed.task_close();
    }

    #[test]
    fn ordinals_read_as_said() {
        let o = super::ordinal;
        assert_eq!(
            [o(1), o(2), o(3), o(4), o(11), o(12), o(13), o(21), o(22), o(103)],
            ["1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd", "103rd"]
        );
        // (as a session's tab and its row in the list say where its Claude's run is)
        assert_eq!(crate::session::Doing::Queued { pos: 2 }.said(), "queued 2nd");
    }
}
