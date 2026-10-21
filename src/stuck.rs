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
//!
//! But not as soon (cardburden 5 — step 6c's breaks run, Docker's socket granted and its own `timeout 2700`,
//! asked at 10 minutes, ended by itself at 14): a run under a time limit its own command set over all it runs
//! (`limit`: the packs' `time-limits`, `timeout`) isn't asked before that limit is up, two hours at most
//! (`HOLD_MAX`) — the panel says `its timeout ends it in 35:00`; one granted a daemon's socket or run outside the sandbox on a card's `y`, or found connected outside
//! its processes, not before `llm.stuck-after-outside` (30 min) — no sandbox at all (none here, off) is every
//! run, and asked as before. `tarae state` says such a run is `quiet`.

use std::time::{Duration, Instant};

use crate::editor::Editor;
use crate::event::SessionId;
use crate::procs::{Row, Thread};
use crate::sizes::Slot;

/// The processes of a run's tree read at most.
const PROCS_MAX: usize = 24;
/// A stopped run still there this long after its TERM gets a KILL (tests: shorter — the stand-ins end at the TERM).
const KILL_AFTER: Duration = Duration::from_millis(if cfg!(test) { 500 } else { 3000 });
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

/// A run of Claude's running in session `l`'s `slot`: its pid, trace id, when it last printed, its waits,
/// what it was let reach (`Guard::reaches_out`) and whether its work may go on elsewhere (`away`).
type Running<'a> = (u32, u64, Instant, &'a mut crate::waits::Waiting, Option<crate::sandbox::ReachOut>, bool);

fn running(l: &mut crate::session::Session, slot: Slot) -> Option<Running<'_>> {
    let reach = |g: &Option<crate::sandbox::Guard>| g.as_ref().and_then(crate::sandbox::Guard::reaches_out);
    match slot {
        Slot::Task => {
            let r = l.task_run.as_mut().filter(|r| r.by_claude && r.state == crate::task::State::Running)?;
            Some((
                r.pid()?,
                r.trace.id(),
                r.last_output,
                &mut r.wait,
                reach(&r.guard),
                away(r.guard.as_ref()),
            ))
        }
        Slot::Test => {
            let r = l
                .test_run
                .as_mut()
                .filter(|r| r.by_claude && r.state == crate::testing::RunState::Running)?;
            Some((
                r.pid()?,
                r.trace.id(),
                r.last_output,
                &mut r.wait,
                reach(&r.guard),
                away(r.guard.as_ref()),
            ))
        }
        _ => None,
    }
}

/// A look at the processes (`rows`, taken at `now`): each run of Claude's told its CPU; one that looks stuck
/// has its threads read, then asked about — in its own session.
pub fn look(ed: &mut Editor, rows: &[Row], now: Instant) {
    let (after, outside) = afters(&ed.config.llm);
    let mut due: Vec<(SessionId, Stuck)> = Vec::new();
    for l in std::iter::once(&mut ed.session).chain(&mut ed.sessions) {
        for slot in [Slot::Task, Slot::Test] {
            let Some((pid, run, last, wait, reach, away)) = running(l, slot) else { continue };
            if let Some((pct, _)) = crate::procs::summed(rows, &[pid]) {
                wait.looked(pct, now);
            }
            // (granted a daemon's socket, outside the sandbox, or found connected outside: its work may go on
            // there — asked later)
            let after = if away || wait.connected { outside } else { after };
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

/// How long a run prints nothing at next to no CPU before the card asks: `llm.stuck-after` — and, its work
/// maybe going on elsewhere (`away`, or connected outside its processes), `llm.stuck-after-outside`,
/// never sooner (0: as the first).
pub fn afters(llm: &crate::llm::LlmConfig) -> (Duration, Duration) {
    let min = |m: u64| Duration::from_secs(m.saturating_mul(60));
    let after = min(llm.stuck_after);
    let outside = match llm.stuck_after_outside {
        0 => after,
        m => min(m).max(after),
    };
    (after, outside)
}

/// Whether a run was let reach where its work may go on while nothing here uses CPU: a daemon's socket granted
/// (Docker's), or run outside the sandbox on a card's `y` — not the network alone (frictions15: that says
/// nothing of where it is), nor no sandbox at all (none here, off: every run so — #181's hang, cargo under
/// qemu, is asked at `llm.stuck-after` all the same).
pub fn away(guard: Option<&crate::sandbox::Guard>) -> bool {
    use crate::sandbox::Guard;
    match guard {
        Some(Guard::In(p)) => !p.sockets.is_empty(),
        Some(Guard::Outside) => true,
        _ => false,
    }
}

/// Whether a running run that printed nothing since `last` is held from the stuck card now — under its own
/// time limit still to come, or its work maybe elsewhere (`away`, or found connected outside) and not quiet
/// for `llm.stuck-after-outside` yet: `tarae state` says `quiet`, not `running`.
pub fn held(
    wait: &crate::waits::Waiting,
    last: Instant,
    now: Instant,
    away: bool,
    llm: &crate::llm::LlmConfig,
) -> bool {
    let quiet = now.saturating_duration_since(last);
    let silent = wait.silent_for(last, now).is_some();
    let limited = wait.hold_until.is_some_and(|at| at > now);
    silent && (limited || ((away || wait.connected) && quiet < afters(llm).1))
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

/// The longest a run holds the stuck card under its own time limit, whatever the limit says (the user's call,
/// review of cardburden 5): `timeout 30d sh x.sh` hung at 0% CPU is still a hang to ask about — two hours
/// covers a long test suite or a container build, and past them the card asks as it would have.
pub const HOLD_MAX: Duration = Duration::from_secs(2 * 3600);

/// The time limit command line `cmd` sets itself over all it runs (cardburden 5: `DOCKER_HOST=… timeout 2700
/// sh breaks.sh` asked "stuck?" at 10 minutes, ended by itself at 14) — None unless every part of it is under
/// one (the review: `cargo build && timeout 60 cargo test` or `hang; timeout 1d true` was held while the part
/// that hangs had none). The line as `readonly::parse_env` reads it (a `&`, a subshell, a `$(…)`: None);
/// commands one after another (`;` `&&` `||`) add up; a pipeline counts when its first command is limited and
/// the rest only read (`readonly::only_reads`: they end with their input — `| tail -20`, not `| tail -f`). A
/// command is limited by a program of the packs' `time-limits` leading it — after `NAME=value` and other
/// wrappers — its duration the first word that isn't one of its options (`1800`, `45m`, `2h`, `1.5d`; a grace
/// option's value added, `-k 30`).
pub fn limit(cmd: &str) -> Option<Duration> {
    let packs = crate::sandboxpack::get();
    limit_in(cmd, &packs.time_limits, &packs.wrappers)
}

fn limit_in(cmd: &str, limits: &[crate::sandboxpack::TimeLimit], wrappers: &[String]) -> Option<Duration> {
    use crate::readonly::Part;
    let (parts, piped) = crate::readonly::parse_env(cmd)?;
    let mut total = Duration::ZERO;
    let mut i = 0;
    while i < parts.len() {
        // (this pipeline: parts i..=end, joined by single `|`s)
        let end = (i..parts.len()).find(|&j| !piped.get(j).copied().unwrap_or(false)).unwrap_or(i);
        let Part::Cmd(first) = &parts[i] else { return None };
        let words: Vec<String> = first.iter().map(|w| w.text.clone()).collect();
        let t = limited(&words, limits, wrappers)?;
        let readers =
            parts[i + 1..=end].iter().all(|p| matches!(p, Part::Cmd(w) if crate::readonly::only_reads(w)));
        if !readers {
            return None;
        }
        total = total.checked_add(t)?;
        i = end + 1;
    }
    (!parts.is_empty()).then_some(total)
}

/// The limit one command's `words` set (`limit`): None when no program of `limits` leads it.
fn limited(
    words: &[String],
    limits: &[crate::sandboxpack::TimeLimit],
    wrappers: &[String],
) -> Option<Duration> {
    let words = &words[crate::cardwarn::lead(words)..];
    // (the limit among the leading wrappers: `nice timeout 60 …`, `timeout 60 nice …`)
    let (k, l) = words
        .iter()
        .enumerate()
        .take_while(|(_, w)| wrappers.contains(w))
        .find_map(|(k, w)| limits.iter().find(|l| l.run == *w).map(|l| (k, l)))?;
    let (mut i, mut grace) = (k + 1, Duration::ZERO);
    while let Some(w) = words.get(i) {
        if let Some((name, value)) = w.split_once('=').filter(|(n, _)| n.starts_with("--")) {
            if l.grace.iter().any(|g| g == name) {
                grace = duration(value).unwrap_or(grace);
            }
            i += 1;
        } else if l.option_values.contains(w) {
            if l.grace.contains(w) {
                grace = words.get(i + 1).and_then(|v| duration(v)).unwrap_or(grace);
            }
            i += 2;
        } else if w.starts_with('-') {
            i += 1;
        } else {
            return duration(w)?.checked_add(grace);
        }
    }
    None
}

/// A duration as `timeout` takes one: a number, a fraction too, and `s` `m` `h` `d` (none: seconds).
fn duration(w: &str) -> Option<Duration> {
    let (n, unit) = match w.char_indices().last()? {
        (i, c) if c.is_ascii_alphabetic() => (&w[..i], c),
        _ => (w, 's'),
    };
    let secs: f64 = n.parse().ok().filter(|s: &f64| s.is_finite() && *s >= 0.0)?;
    let times = match unit {
        's' => 1.0,
        'm' => 60.0,
        'h' => 3600.0,
        'd' => 86400.0,
        _ => return None,
    };
    Duration::try_from_secs_f64(secs * times).ok()
}

/// `no output for 12 min, 0% CPU`.
fn said(s: &Stuck) -> String {
    format!("no output for {} min, {:.0}% CPU", s.quiet.as_secs() / 60, s.cpu)
}

/// Its threads read: into its output (from `s.at`) — then the card (`card`), if it's still running and still
/// quiet.
fn ask(ed: &mut Editor, mut s: Stuck, lines: Vec<String>, peers: Vec<String>) {
    // (connected outside its processes — a container engine, another host: its work may go on there, and it
    // isn't asked before `llm.stuck-after-outside`; marked so, the next looks wait that long)
    let (_, outside) = afters(&ed.config.llm);
    if !peers.is_empty() && s.quiet < outside {
        ed.session.log().line(
            "stuck",
            "held",
            &format!("pid {}: {} — connected to {}", s.pid, said(&s), peers.join(", ")),
        );
        if let Some(w) = run_wait(ed, &s) {
            (w.connected, w.stuck_asked) = (true, None);
        }
        return;
    }
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
    let asks = format!("{label}: {}", o.question.lines().next().unwrap_or_default());
    ed.session.log().line("stuck", "ask", &asks);
    ed.push_offer(o);
}

/// The waits of the run `s` is about, while it's still that run.
fn run_wait<'a>(ed: &'a mut Editor, s: &Stuck) -> Option<&'a mut crate::waits::Waiting> {
    match s.slot {
        Slot::Task => ed.session.task_run.as_mut().filter(|r| r.trace.id() == s.run).map(|r| &mut r.wait),
        _ => ed.session.test_run.as_mut().filter(|r| r.trace.id() == s.run).map(|r| &mut r.wait),
    }
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

/// `y` on the card: the run stopped with every process under it (`procs::stop_tree`: TERM, KILL to what's still
/// there `KILL_AFTER` later — what left its group with `setsid` too, frictions29 35) — its panel and output stay,
/// and Claude's report has them. What outlived the stop is said once it's known (`stopped`).
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
    ed.session.log().line("stuck", "stop", &format!("pid {pid}: {}", said(s)));
    // (what it started in a container engine — a quiet `docker run`'s work goes on there — with it, by name:
    // `containers.rs`)
    if let Some(c) = containers {
        c.stopped();
    }
    // (its tree read before the first signal, on a worker — the stop's timer with it; a quit doesn't wait)
    let (slot, run) = (s.slot, s.run);
    ed.events.jobs().spawn(move || {
        let done = crate::procs::stop_tree(pid, KILL_AFTER);
        move |ed: &mut Editor| stopped(ed, slot, run, pid, done)
    });
    ed.set_status("stopping it — its output stays in the panel, and Claude hears how it ended");
}

/// How many of run `run`'s processes a stop shows at most, of those that outlived it.
const OUTLIVED_SHOWN: usize = 3;

/// The stop of run `run` (its root `pid`) is over (`done`): what left its group and was stopped with it goes in
/// its output; what outlived it is said — there, as a warning, and in the log — with `:procs` to see it.
fn stopped(ed: &mut Editor, slot: Slot, run: u64, pid: u32, done: crate::procs::Stopped) {
    let n = done.outlived.len();
    let apart = match done.apart {
        0 => None,
        1 => Some("── stopped with it: 1 process it started in a session of its own ──".to_string()),
        k => Some(format!("── stopped with it: {k} processes it started in sessions of their own ──")),
    };
    let named: Vec<String> = done
        .outlived
        .iter()
        .take(OUTLIVED_SHOWN)
        .map(|(p, cmd)| format!("pid {p} {}", crate::cut::fit_ellipsis(cmd, COMMAND_MAX)))
        .collect();
    let more = if n > named.len() { format!(" and {} more", n - named.len()) } else { String::new() };
    let outlived = (n > 0).then(|| {
        let them = if n == 1 { "1 process".to_string() } else { format!("{n} processes") };
        format!(
            "{them} outlived the stop: {}{more} — :procs shows {}",
            named.join(", "),
            if n == 1 { "it" } else { "them" }
        )
    });
    let lines: Vec<String> = apart.into_iter().chain(outlived.iter().map(|o| format!("── {o} ──"))).collect();
    match slot {
        Slot::Task => {
            if let Some(r) = ed.session.task_run.as_mut().filter(|r| r.trace.id() == run) {
                lines.into_iter().for_each(|l| r.push_line(false, l, false));
            }
        }
        _ => {
            if let Some(r) = ed.session.test_run.as_mut().filter(|r| r.trace.id() == run) {
                r.output.extend(lines.into_iter().map(|l| (false, l)));
            }
        }
    }
    let log = format!("pid {pid}: {} apart, {n} outlived {:?}", done.apart, done.outlived);
    ed.session.log().line("stuck", "stopped", &log);
    match outlived {
        Some(o) => ed.set_warning(format!("Stopped Claude's run — but {o}")),
        // (nothing apart, nothing left: nothing new on screen)
        None if done.apart == 0 => ed.unchanged = true,
        None => {}
    }
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
