//! A run of Claude's that looks stuck (#181: cargo hung under qemu — `no output for 68:11` at 0% CPU — and
//! nothing asked). Each look at the processes (`procs_sampled`, every `procs::QUIET_EVERY`) tells a running
//! run of Claude's its share of a core, all its processes told (`Waiting::looked`). Once it has printed
//! nothing and used next to no CPU for `llm.stuck-after` minutes, what each of its threads waits in is read
//! on a worker — unprivileged only: Linux's `/proc/<pid>/task/*/{stat,wchan}`, macOS's thread states — and
//! put in its output, so it's kept whatever comes next (stopped, its output is in Claude's report); then a
//! card asks: `y` stops it · `w` shows what it waits in (its panel, at those lines) · `n` keeps waiting
//! (asked again after as long again). The card goes once the run prints or ends.
//!
//! Quiet isn't always stuck (frictions15): a client whose work goes on elsewhere — a container engine in a VM
//! or on another host, a build server — prints nothing and uses no CPU here. What its tree is connected to
//! outside itself (`procs::sockets`, read beside its threads) and what it was let reach (`Guard::reaches_out`)
//! are said on the card — titled `Quiet:` when it's connected outside or was granted a daemon's socket (the
//! network or no sandbox alone say nothing of where it is: a note); it's asked all the same — a daemon gone
//! dead looks the same.

use std::time::{Duration, Instant};

use crate::editor::Editor;
use crate::event::SessionId;
use crate::procs::{Row, Thread};
use crate::sizes::Slot;

/// The processes of a run's tree read at most.
const PROCS_MAX: usize = 24;
/// A stopped run still there this long after its TERM gets a KILL.
const KILL_AFTER: Duration = Duration::from_secs(3);
/// A process's command line is cut past this many characters (a long classpath).
const COMMAND_MAX: usize = 100;

/// What the card asks about.
#[derive(Clone, Debug)]
pub struct Stuck {
    pub slot: Slot,
    /// Its run's trace id (`run#…` in the state).
    pub run: u64,
    pub pid: u32,
    /// When it was found stuck — a line printed after it means it wasn't.
    pub asked: Instant,
    /// How long it had printed nothing then, and its share of a core.
    pub quiet: Duration,
    pub cpu: f32,
    /// Where what its threads wait in starts in its output.
    pub at: usize,
    /// What it was let reach through which its work may go on elsewhere (`Guard::reaches_out`: a socket
    /// granted, the network, no sandbox) — None: neither.
    pub reach: Option<crate::sandbox::ReachOut>,
}

/// A run of Claude's running in session `l`'s `slot`: its pid, trace id, when it last printed, its waits and
/// what it was let reach (`Guard::reaches_out`).
type Running<'a> = (u32, u64, Instant, &'a mut crate::waits::Waiting, Option<crate::sandbox::ReachOut>);

fn running(l: &mut crate::session::Session, slot: Slot) -> Option<Running<'_>> {
    let reach = |g: &Option<crate::sandbox::Guard>| g.as_ref().and_then(crate::sandbox::Guard::reaches_out);
    match slot {
        Slot::Task => {
            let r = l.task_run.as_mut().filter(|r| r.by_claude && r.state == crate::task::State::Running)?;
            Some((r.pid()?, r.trace.id(), r.last_output, &mut r.wait, reach(&r.guard)))
        }
        Slot::Test => {
            let r = l
                .test_run
                .as_mut()
                .filter(|r| r.by_claude && r.state == crate::testing::RunState::Running)?;
            Some((r.pid()?, r.trace.id(), r.last_output, &mut r.wait, reach(&r.guard)))
        }
        _ => None,
    }
}

/// A look at the processes (`rows`, taken at `now`): each run of Claude's told its CPU; one that looks stuck
/// has its threads read, then asked about — in its own session.
pub fn look(ed: &mut Editor, rows: &[Row], now: Instant) {
    let after = Duration::from_secs(ed.config.llm.stuck_after.saturating_mul(60));
    let mut due: Vec<(SessionId, Stuck)> = Vec::new();
    for l in std::iter::once(&mut ed.session).chain(&mut ed.sessions) {
        for slot in [Slot::Task, Slot::Test] {
            let Some((pid, run, last, wait, reach)) = running(l, slot) else { continue };
            if let Some((pct, _)) = crate::procs::summed(rows, &[pid]) {
                wait.looked(pct, now);
            }
            if let Some(quiet) = wait.stuck_for(last, now, after) {
                wait.stuck_asked = Some(now);
                let cpu = wait.cpu.unwrap_or(0.0);
                due.push((l.id, Stuck { slot, run, pid, asked: now, quiet, cpu, at: 0, reach }));
            }
        }
    }
    for (session, s) in due {
        ed.with_session(session, |ed| {
            ed.events.jobs().spawn(move || {
                let lines = waits_in(s.pid);
                let peers = connected_outside(s.pid);
                move |ed: &mut Editor| ask(ed, s, lines, peers)
            })
        });
    }
}

/// What `pid`'s tree is connected to outside itself (`procs::sockets`), as said — a daemon's socket, a host
/// and port: where its work may be going on while nothing here uses CPU.
pub fn connected_outside(pid: u32) -> Vec<String> {
    let socks: Vec<crate::procs::sockets::Sock> = crate::procs::tree(pid)
        .iter()
        .take(PROCS_MAX)
        .flat_map(|os| crate::procs::sockets::of(os.pid))
        .collect();
    crate::procs::sockets::outside(&socks)
}

/// What each thread of `pid`'s tree waits in, as its output says it: a process's line, then its threads
/// (alike ones counted on one line).
pub fn waits_in(pid: u32) -> Vec<String> {
    let tree = crate::procs::tree(pid);
    let mut out = Vec::new();
    for os in tree.iter().take(PROCS_MAX) {
        let threads = crate::procs::threads(os.pid);
        let command = crate::procs::command(os.pid).unwrap_or_else(|| os.name.clone());
        let n = threads.len();
        let many = if n == 1 { "1 thread".to_string() } else { format!("{n} threads") };
        let command = match command.chars().count() > COMMAND_MAX {
            true => format!("{}…", command.chars().take(COMMAND_MAX - 1).collect::<String>()),
            false => command,
        };
        out.push(format!("pid {} · {command} · {many}", os.pid));
        out.extend(grouped(&threads));
        if threads.is_empty() {
            out.push("  (its threads couldn't be read — it ended, or isn't ours to read)".into());
        }
    }
    if tree.len() > PROCS_MAX {
        out.push(format!("… {} more processes not read", tree.len() - PROCS_MAX));
    }
    if cfg!(target_os = "macos") && !tree.is_empty() {
        out.push(format!(
            "(macOS tells each thread's state, not where it waits — `sample {pid}` in a terminal shows its stacks)"
        ));
    }
    out
}

/// A process's threads, one line each — alike ones (same name, same wait) counted on one: `  3 × worker —
/// sleeping in futex_wait_queue · 0.12 s CPU`.
fn grouped(threads: &[Thread]) -> Vec<String> {
    let mut groups: Vec<(String, usize, Duration)> = Vec::new();
    for t in threads {
        let name = if t.name.is_empty() { "thread" } else { t.name.as_str() };
        let what = match &t.wchan {
            Some(w) => format!("{name} — {} in {w}", t.state),
            None => format!("{name} — {}", t.state),
        };
        match groups.iter_mut().find(|g| g.0 == what) {
            Some(g) => (g.1, g.2) = (g.1 + 1, g.2 + t.cpu),
            None => groups.push((what, 1, t.cpu)),
        }
    }
    groups
        .into_iter()
        .map(|(what, n, cpu)| {
            let times = if n > 1 { format!("{n} × ") } else { String::new() };
            format!("  {times}{what} · {:.2} s CPU", cpu.as_secs_f64())
        })
        .collect()
}

/// `no output for 12 min, 0% CPU`.
fn said(s: &Stuck) -> String {
    format!("no output for {} min, {:.0}% CPU", s.quiet.as_secs() / 60, s.cpu)
}

/// Its threads read: into its output (from `s.at`) — then the card (`card`), if it's still running and still
/// quiet.
fn ask(ed: &mut Editor, mut s: Stuck, lines: Vec<String>, peers: Vec<String>) {
    let head = format!("── tarae: {} — what each of its threads waits in ──", said(&s));
    let still = |at: Instant| at <= s.asked;
    let label = match s.slot {
        Slot::Task => {
            let Some(r) = ed.session.task_run.as_mut().filter(|r| {
                r.state == crate::task::State::Running && r.trace.id() == s.run && still(r.last_output)
            }) else {
                return;
            };
            s.at = r.output.len();
            for line in std::iter::once(head).chain(lines.iter().cloned()) {
                r.push_line(false, line, false);
            }
            format!("Claude's command {} in the task panel", crate::task::traced(&r.task.cmd))
        }
        _ => {
            let Some(r) = ed.session.test_run.as_mut().filter(|r| {
                r.state == crate::testing::RunState::Running && r.trace.id() == s.run && still(r.last_output)
            }) else {
                return;
            };
            s.at = r.output.len();
            r.output.extend(std::iter::once(head).chain(lines.iter().cloned()).map(|l| (false, l)));
            format!("Claude's tests {} in the test pane", r.target.label)
        }
    };
    let o = card(s, &label, &lines, &peers);
    crate::log::line("stuck", "ask", &format!("{label}: {}", o.question.lines().next().unwrap_or_default()));
    ed.push_offer(o);
}

/// The card about run `s` (`label`: which it is and where), its threads' `lines`, what its tree is connected
/// to outside itself (`peers`). Connected, or granted a daemon's socket (`s.reach`), it may be working there —
/// said, and `Quiet:` rather than `Stuck?`. Let reach only the network, or no sandbox around it, with nothing
/// connected: still `Stuck?` (#181 — cargo hung under qemu, outside a sandbox too), the reach a note.
pub fn card(s: Stuck, label: &str, lines: &[String], peers: &[String]) -> crate::offer::Offer {
    use crate::sandbox::ReachOut;
    let shown: Vec<&str> = lines.iter().take(6).map(String::as_str).collect();
    let more = if lines.len() > shown.len() { "\n    …" } else { "" };
    let there = " — the work may be going on there: next to no CPU here doesn't mean it's stuck.";
    let (quiet, elsewhere) = match (peers, &s.reach) {
        ([], None) => (false, String::new()),
        ([], Some(ReachOut::Sockets(socks))) => (true, format!("\n\nIt can reach {socks}{there}")),
        ([], Some(reach)) => {
            let seen = "but no connection outside its processes was seen";
            (false, format!("\n\nIt can reach {}, {seen}.", reach.said()))
        }
        (peers, _) => (true, format!("\n\nIts processes are connected to {}{there}", peers.join(", "))),
    };
    let question = format!(
        "N{} — {label}. Claude's turn waits on it, and only you can stop it.{elsewhere}\n\nWhat each of its \
         threads waits in is in its output now (w), kept if you stop it:\n    {}{more}",
        &said(&s)[1..],
        shown.join("\n    "),
    );
    let title = match (s.slot, quiet) {
        (Slot::Task, false) => "Stuck? Claude's command",
        (Slot::Task, true) => "Quiet: Claude's command",
        (_, false) => "Stuck? Claude's tests",
        (_, true) => "Quiet: Claude's tests",
    };
    let o = crate::offer::Offer::new(
        title.into(),
        &question,
        "Stopping…",
        String::new(),
        crate::offer::What::Stuck(Box::new(s)),
    );
    // (where the work may be going on is what tells this card from `Stuck?`: at full strength, with the run)
    match quiet {
        false => o,
        true => o.deciding(0..3),
    }
}

/// `w` on the card: its run's panel in sight, at what its threads wait in — the card stays, to answer.
pub fn show(ed: &mut Editor, s: &Stuck) {
    match s.slot {
        Slot::Task => {
            if let Some(r) = ed.session.task_run.as_mut().filter(|r| r.trace.id() == s.run) {
                r.scroll = Some(s.at);
            }
        }
        _ => {
            if let Some(r) = ed.session.test_run.as_mut().filter(|r| r.trace.id() == s.run) {
                r.scroll = Some(s.at);
            }
        }
    }
    ed.slot_show(s.slot);
    ed.set_status("what its threads wait in, in its output — y on the card stops it, n keeps waiting");
}

/// `y` on the card: the run stopped (TERM to its group, KILL if it's still there `KILL_AFTER` later) — its
/// panel and output stay, and Claude's report has them.
pub fn stop(ed: &mut Editor, s: &Stuck) {
    let line = format!("── stopped from tarae's card: {} ──", said(s));
    let (pid, containers) = match s.slot {
        Slot::Task => ed.session.task_run.as_mut().filter(|r| r.trace.id() == s.run).map(|r| {
            r.push_line(false, line, false);
            (r.pid(), r.containers.take())
        }),
        _ => ed.session.test_run.as_mut().filter(|r| r.trace.id() == s.run).map(|r| {
            r.output.push((false, line));
            (r.pid(), r.containers.take())
        }),
    }
    .unwrap_or((None, None));
    let Some(pid) = pid else { return ed.set_status("that run ended already") };
    crate::log::line("stuck", "stop", &format!("pid {pid}: {}", said(s)));
    let started = crate::procs::read(pid).map(|o| o.started);
    crate::dap::term_group(pid);
    // (what it started in a container engine — a quiet `docker run`'s work goes on there — with it, by name:
    // `containers.rs`)
    if let Some(c) = containers {
        c.stopped();
    }
    // (the same process still — a pid isn't given out again that soon, and its start says it's the one)
    ed.events.jobs().background(move || {
        std::thread::sleep(KILL_AFTER);
        if crate::procs::read(pid).is_some_and(|o| !o.zombie && Some(o.started) == started) {
            // SAFETY: a KILL to the process group of Claude's run, started with setsid, still the same.
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
        }
    });
    ed.set_status("stopping it — its output stays in the panel, and Claude hears how it ended");
}

/// `n` on the card: asked again only after as long again with nothing.
pub fn keep_waiting(ed: &mut Editor, s: &Stuck) {
    let now = Instant::now();
    let found = match s.slot {
        Slot::Task => ed.session.task_run.as_mut().filter(|r| r.trace.id() == s.run).map(|r| &mut r.wait),
        _ => ed.session.test_run.as_mut().filter(|r| r.trace.id() == s.run).map(|r| &mut r.wait),
    };
    if let Some(w) = found {
        w.stuck_asked = Some(now);
    }
    let after = ed.config.llm.stuck_after;
    let stop = if s.slot == Slot::Task { "space m c" } else { "space x c" };
    ed.set_status(format!("kept waiting — asked again after {after} more min of nothing; {stop} stops it"));
}

/// Every event: a stuck card whose run printed since, or ended, goes — in every session.
pub fn sync(ed: &mut Editor) {
    let any = |o: &crate::offer::Offer| o.stuck().is_some();
    let behind: Vec<SessionId> =
        ed.sessions.iter().filter(|l| l.parked.iter().any(any)).map(|l| l.id).collect();
    if ed.offers.iter().any(any) || ed.session.parked.iter().any(any) {
        drop_gone(ed);
    }
    for id in behind {
        ed.with_session(id, drop_gone);
    }
}

/// The session in `ed.session`'s stuck cards whose run isn't stuck any more.
fn drop_gone(ed: &mut Editor) {
    let mut live = Vec::new();
    for slot in [Slot::Task, Slot::Test] {
        if let Some((_, run, last, ..)) = running(&mut ed.session, slot) {
            live.push((run, last));
        }
    }
    let gone = |w: &crate::offer::What| match w {
        crate::offer::What::Stuck(s) => !live.iter().any(|(run, last)| *run == s.run && *last <= s.asked),
        _ => false,
    };
    ed.drop_cards_for(gone, "dropped — its run printed, or ended");
}

#[cfg(test)]
mod tests;
