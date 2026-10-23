use std::time::{Duration, Instant};

use super::*;
use crate::key::{Code, Key};
use crate::procs::{Os, Sample};
use crate::waits::Waiting;

const MIN: Duration = Duration::from_secs(60);

/// #181: a run is stuck once it has printed nothing *and* used next to no CPU for the setting's minutes —
/// asked once, again only after as long again; never while it says what it waits on, nor with the setting
/// at 0. Working (a look over `IDLE_PCT`) starts its idle time over.
#[test]
fn quiet_and_idle_for_its_minutes_is_stuck() {
    let t0 = Instant::now();
    let at = |m: u64| t0 + MIN * m as u32;
    let after = 10 * MIN;
    let mut w = Waiting::default();
    assert_eq!(w.stuck_for(t0, at(30), after), None, "never looked at: not known to be idle");
    w.looked(0.4, at(0));
    assert_eq!(w.idle, Some(at(0)));
    w.looked(0.0, at(5));
    assert_eq!(w.idle, Some(at(0)), "idle since the first idle look");
    assert_eq!(w.stuck_for(t0, at(9), after), None, "9 minutes");
    assert_eq!(w.stuck_for(t0, at(12), after), Some(12 * MIN));
    assert_eq!(w.stuck_for(at(4), at(12), after), None, "it printed 8 minutes ago");
    assert_eq!(w.stuck_for(t0, at(12), Duration::ZERO), None, "0 = never");
    // Working starts it over
    w.looked(35.0, at(12));
    assert_eq!((w.idle, w.cpu), (None, Some(35.0)));
    assert_eq!(w.stuck_for(t0, at(13), after), None);
    w.looked(0.2, at(13));
    assert_eq!(w.stuck_for(t0, at(22), after), None, "idle 9 minutes only");
    assert_eq!(w.stuck_for(t0, at(23), after), Some(23 * MIN));
    // Asked: not again until as long again
    w.stuck_asked = Some(at(23));
    assert_eq!(w.stuck_for(t0, at(30), after), None);
    assert_eq!(w.stuck_for(t0, at(33), after), Some(33 * MIN));
}

/// cardburden 5: step 6c's breaks run — Docker's socket granted, `timeout 2700` its own — was asked "stuck?" at
/// 10 minutes and ended by itself at 14. The time limit a command line sets itself is read from the packs'
/// `time-limits` (built-in: `timeout`, `gtimeout`) after variables and other wrappers, options skipped, a
/// grace option's value added. A word that only names `timeout` sets nothing.
#[test]
fn a_commands_own_time_limit_is_read_from_its_words() {
    let s = Duration::from_secs;
    for (cmd, set) in [
        ("DOCKER_HOST=unix:///x.sock timeout 2700 sh breaks.sh", Some(s(2700))),
        ("timeout -k 30 45m make check", Some(s(45 * 60 + 30))),
        ("timeout --kill-after=1m -s KILL 2h cargo test", Some(s(2 * 3600 + 60))),
        ("timeout --signal=INT --preserve-status 90s go test ./...", Some(s(90))),
        ("nice timeout 1.5d ./soak", Some(s(129_600))),
        ("gtimeout 600 docker build .", Some(s(600))),
        ("sh -c 'timeout 60 cargo test'", Some(s(60))),
        // (none: `timeout` only a word, or no duration after it)
        ("echo timeout 60", None),
        ("cargo test timeout", None),
        ("timeout sh x.sh", None),
        ("timeout 10q x", None),
        ("timeout -1 x", None),
        ("make check", None),
    ] {
        assert_eq!(limit(cmd), set, "{cmd}");
    }
}

/// The review of cardburden 5: the largest limit anywhere in the line held the card while the part that hangs
/// had none — `cargo build && timeout 60 cargo test`, `hang; timeout 1d true`. A line is limited only when all
/// it runs is: one after another (`;` `&&` `||`) the limits add up; a pipeline counts when its first command is
/// limited and the rest only read, ending with their input (`| tail -20`, not `| tail -f` nor `| sh`). Any part
/// with no limit — a `cd` too — or what the reader can't read (a `&`, `$(…)`) is no hold.
#[test]
fn a_line_is_limited_only_when_every_part_of_it_is() {
    let s = Duration::from_secs;
    for (cmd, set) in [
        ("timeout 60 a; timeout 10m b", Some(s(660))),
        ("timeout 60 a && timeout 2m b || timeout 30 c", Some(s(210))),
        ("timeout 600 cargo test 2>&1 | tail -20", Some(s(600))),
        ("timeout 600 cargo test 2>&1 | grep -v warning | head -50", Some(s(600))),
        ("cargo build && timeout 60 cargo test", None),
        ("hang; timeout 1d true", None),
        ("timeout 1d true; hang", None),
        ("cd sub && timeout 60 cargo test", None),
        ("timeout 600 cargo test | tail -f", None),
        ("timeout 600 cargo test | sh", None),
        ("cat log | timeout 60 grep x", None),
        ("timeout 60 a & wait", None),
        ("timeout 60 $(which cargo) test", None),
    ] {
        assert_eq!(limit(cmd), set, "{cmd}");
    }
}

/// The limit is taken when the run starts, from its start — said as it is, held two hours at most (`HOLD_MAX`).
/// (The run is the test's own `sleep` under `timeout`, stopped at the end.)
#[test]
fn a_run_started_under_a_time_limit_knows_when_it_is_up() {
    let dir = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("tarae-stuck-limit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = dir.clone();
    for (cmd, limit, hold) in [
        ("timeout -k 5 30 sleep 30", Some(35), Some(35)),
        ("timeout 3h sleep 30", Some(10_800), Some(7200)),
        ("sleep 30", None, None),
    ] {
        let task = crate::task::Task { name: "t".into(), cmd: cmd.into(), cwd: dir.clone() };
        assert!(
            ed.task_start_for(task, crate::tools::For::User("t".into()), None, Default::default()).is_ok()
        );
        let r = ed.session.task_run.as_ref().unwrap();
        let secs = |at: Option<Instant>| at.map(|at| at.duration_since(r.started).as_secs());
        assert_eq!((secs(r.wait.limit_at), secs(r.wait.hold_until)), (limit, hold), "{cmd}");
        ed.session.task_run = None;
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// cardburden 5 (a): under its own time limit still to come, a run quiet and idle past `llm.stuck-after` isn't
/// asked — it ends by then, stuck or not; the panel says when. Past its limit (it didn't end: something
/// ignored the signal), asked as before.
#[test]
fn a_run_under_its_own_time_limit_is_not_asked_before_it_is_up() {
    let t0 = Instant::now();
    let at = |m: u64| t0 + MIN * m as u32;
    let after = 10 * MIN;
    let mut w = Waiting::default();
    w.looked(0.0, at(0));
    w.limited(t0, Some(45 * MIN));
    assert_eq!(w.stuck_for(t0, at(12), after), None, "its timeout 33 min away");
    assert_eq!(w.stuck_for(t0, at(44), after), None);
    assert_eq!(w.stuck_for(t0, at(46), after), Some(46 * MIN), "past it");
    assert_eq!(
        w.silent(t0, at(10)).as_deref(),
        Some("no output for 10m 00s · its timeout ends it in 35m 00s")
    );
    assert_eq!(w.silent(t0, at(46)).as_deref(), Some("no output for 46m 00s"), "nothing to come");
    let llm = crate::llm::LlmConfig::default();
    assert!(held(&w, t0, at(10), false, &llm), "`quiet` in the state");
    assert!(!held(&w, t0, at(46), false, &llm));
    assert!(!held(&w, at(10), at(10), false, &llm), "printing: not quiet");
    // The review: `timeout 30d sh x.sh` hung at 0% CPU is still asked — after two hours (`HOLD_MAX`), its
    // timeout said as it is
    w.limited(t0, Some(30 * 24 * 60 * MIN));
    assert_eq!(w.stuck_for(t0, at(119), after), None);
    assert!(held(&w, t0, at(119), false, &llm));
    assert_eq!(w.stuck_for(t0, at(121), after), Some(121 * MIN), "two hours at most");
    assert!(!held(&w, t0, at(121), false, &llm));
    let said = w.silent(t0, at(121)).unwrap();
    assert!(said.starts_with("no output for 2h 01m · its timeout ends it in 29d 21h"), "{said}");
}

/// cardburden 5 (b), D4: one whose work may go on elsewhere — a daemon's socket granted, run outside the
/// sandbox on a card's `y` — or found connected outside its processes waits `llm.stuck-after-outside` (30 min)
/// before the card, never less than `llm.stuck-after`; 0 = as that. The network alone says nothing of where it
/// is, and no sandbox at all (none here, off) is every run: #181's hang is still asked at 10.
#[test]
fn work_maybe_elsewhere_is_asked_at_its_own_later_minutes() {
    use crate::sandbox::Guard;
    let mut llm = crate::llm::LlmConfig::default();
    assert_eq!((llm.stuck_after, llm.stuck_after_outside), (10, 30), "the defaults");
    assert_eq!(afters(&llm), (10 * MIN, 30 * MIN));
    llm.stuck_after_outside = 0;
    assert_eq!(afters(&llm), (10 * MIN, 10 * MIN), "0 = as llm.stuck-after");
    llm.stuck_after_outside = 5;
    assert_eq!(afters(&llm), (10 * MIN, 10 * MIN), "never sooner");
    let llm = crate::llm::LlmConfig::default();
    let root = std::fs::canonicalize(std::env::temp_dir()).unwrap();
    let policy = || crate::sandbox::policy(&root, &llm, &[]).unwrap();
    let mut socket = policy();
    socket.sockets = vec![root.join("engine.sock")];
    let mut network = policy();
    network.network = true;
    let socket = Guard::In(Box::new(socket));
    assert!(away(Some(&socket)) && away(Some(&Guard::Outside)));
    for g in
        [Guard::In(Box::new(network)), Guard::In(Box::new(policy())), Guard::Off, Guard::Without("x".into())]
    {
        assert!(!away(Some(&g)), "{:?}", g.reaches_out());
    }
    assert!(!away(None));
    let t0 = Instant::now();
    let at = |m: u64| t0 + MIN * m as u32;
    let mut w = Waiting::default();
    w.looked(0.0, at(0));
    assert!(held(&w, t0, at(12), true, &llm));
    assert!(!held(&w, t0, at(31), true, &llm), "asked then");
    assert!(!held(&w, t0, at(12), false, &llm), "#181: still stuck at 10");
    w.connected = true;
    assert!(held(&w, t0, at(12), false, &llm), "connected outside");
}
#[test]
fn alike_threads_are_counted_on_one_line() {
    let t = |name: &str, state: &str, wchan: Option<&str>, ms: u64| Thread {
        name: name.into(),
        state: state.into(),
        wchan: wchan.map(str::to_string),
        cpu: Duration::from_millis(ms),
    };
    let lines = grouped(&[
        t("cargo", "sleeping", Some("do_wait"), 1200),
        t("worker", "sleeping", Some("futex_wait_queue"), 10),
        t("worker", "sleeping", Some("futex_wait_queue"), 20),
        t("", "waiting", None, 0),
    ]);
    assert_eq!(
        lines,
        [
            "  cargo — sleeping in do_wait · 1.2s CPU",
            "  2 × worker — sleeping in futex_wait_queue · 30ms CPU",
            "  thread — waiting · 0ms CPU",
        ]
    );
}

/// What a process's threads wait in, read without privileges: this test's own process (a stand-in for a
/// run — read, nothing done to it).
#[test]
fn a_processs_threads_are_read_without_privileges() {
    if !cfg!(any(target_os = "macos", target_os = "linux")) {
        return;
    }
    // (some CPU of its own first: Linux counts it in 10 ms ticks, and a test run alone had used none)
    let busy = Instant::now();
    let mut n = 0u64;
    while busy.elapsed() < Duration::from_millis(60) {
        n = std::hint::black_box(n.wrapping_add(1));
    }
    let me = std::process::id();
    let threads = crate::procs::threads(me);
    assert!(!threads.is_empty(), "this process has threads");
    assert!(threads.iter().all(|t| !t.state.is_empty()), "{threads:?}");
    assert!(threads.iter().any(|t| t.cpu > Duration::ZERO), "some CPU time: {threads:?}");
    let lines = waits_in(me);
    assert!(lines[0].starts_with(&format!("pid {me} · ")), "{lines:?}");
    assert!(lines[1].starts_with("  ") && lines[1].contains(" — "), "{lines:?}");
    assert!(crate::procs::threads(u32::MAX - 1).is_empty(), "no such process: nothing made up");
}

/// Linux: a sleeping process's thread is read from `/proc` — its state and the kernel function it sleeps in
/// (`wchan`), as the run's output says it. (The sleeper is the test's own child, stopped at its end.)
#[cfg(target_os = "linux")]
#[test]
fn a_sleeping_threads_wait_channel_is_read_on_linux() {
    let mut child = std::process::Command::new("sleep").arg("30").spawn().expect("sleep");
    let pid = child.id();
    // (until it's past its exec and asleep — a second at most)
    let end = Instant::now() + Duration::from_secs(2);
    let mut threads = crate::procs::threads(pid);
    while !threads.first().is_some_and(|t| t.name == "sleep" && t.state == "sleeping" && t.wchan.is_some()) {
        if Instant::now() > end {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
        threads = crate::procs::threads(pid);
    }
    let lines = waits_in(pid);
    let _ = child.kill();
    let _ = child.wait();
    let t = threads.first().unwrap_or_else(|| panic!("its thread read: {threads:?}"));
    assert_eq!((t.name.as_str(), t.state.as_str()), ("sleep", "sleeping"), "{threads:?}");
    let wchan = t.wchan.as_deref().unwrap_or_else(|| panic!("where it sleeps: {threads:?}"));
    assert!(wchan.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'), "a kernel function: {wchan}");
    assert!(lines[0].starts_with(&format!("pid {pid} · sleep 30 · 1 thread")), "{lines:?}");
    assert_eq!(lines[1], format!("  sleep — sleeping in {wchan} · 0.00 s CPU"), "{lines:?}");
}

/// A look at the processes as the sampling thread sends one: tarae (the test) and the run at `pct`.
fn sample(pid: u32, pct: f32) -> Sample {
    let me = std::process::id();
    let os = |pid, ppid, name: &str| Os { pid, ppid, name: name.into(), started: 1, ..Default::default() };
    Sample::of(vec![(os(me, 1, "tarae"), Some(1.0)), (os(pid, me, "sh"), Some(pct))])
}

fn settle_until(ed: &mut Editor, done: impl Fn(&Editor) -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while !done(ed) {
        let left = end.saturating_duration_since(Instant::now());
        let ev = ed.events.recv_timeout(left).expect("event before timeout");
        ed.handle_event(ev);
    }
}

/// #181 as met: a run of Claude's printing nothing at 0% CPU for the setting's minutes has what its threads
/// wait in put in its output, then a card asks — once; `w` shows those lines in its panel (the card stays),
/// `y` stops it, its output kept. (The run is the test's own `sleep`, stopped by the card.)
#[test]
fn a_stuck_run_is_captured_then_asked_about() {
    let dir = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("tarae-stuck-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = dir.clone();
    let task = crate::task::Task { name: "hangs".into(), cmd: "sleep 30".into(), cwd: dir.clone() };
    let user = crate::tools::For::User("hangs".into());
    assert!(ed.task_start_for(task, user, None, Default::default()).is_ok());
    let r = ed.session.task_run.as_mut().unwrap();
    r.by_claude = true;
    let pid = r.pid().unwrap();
    let ago = |m: u32| Instant::now().checked_sub(MIN * m).unwrap();
    r.last_output = ago(11);
    // Idle at the first look — and since 11 minutes, as the looks before saw it
    ed.procs_sampled(sample(pid, 0.0));
    assert!(ed.offers.is_empty(), "idle only from this look");
    ed.session.task_run.as_mut().unwrap().wait.idle = Some(ago(11));
    ed.procs_sampled(sample(pid, 0.0));
    settle_until(&mut ed, |ed| ed.offers.iter().any(|o| o.stuck().is_some()));
    let o = &ed.offers[0];
    assert_eq!(o.title, "Stuck? Claude's command");
    assert!(
        o.question.starts_with("No output for 11m 00s, 0% CPU — Claude's command sleep 30"),
        "{}",
        o.question
    );
    assert!(o.question.contains(&format!("pid {pid} · ")), "its threads on the card: {}", o.question);
    let answers = o.answers(false);
    assert_eq!(answers, [('y', "Stop it"), ('w', "What it waits on"), ('n', "Keep waiting")]);
    let out: Vec<String> =
        ed.session.task_run.as_ref().unwrap().output.iter().map(|l| l.1.into_owned()).collect();
    let head = "── tarae: no output for 11m 00s, 0% CPU — what each of its threads waits in ──";
    let at = out.iter().position(|l| *l == head).unwrap_or_else(|| panic!("{out:?}"));
    assert!(out[at + 1].starts_with(&format!("pid {pid} · ")), "{out:?}");
    assert!(out[at + 2].starts_with("  ") && out[at + 2].contains(" — "), "a thread: {out:?}");
    // Asked once: the next look asks nothing more
    ed.procs_sampled(sample(pid, 0.0));
    assert_eq!(ed.offers.iter().filter(|o| o.stuck().is_some()).count(), 1);
    // `w`: its panel, at those lines — the card stays
    crate::term::render(&mut ed, &mut Vec::new(), 120, 40).unwrap();
    let o = (ed.offers[0].id, ed.offers[0].rev);
    ed.offer_seen.set(Some((o.0, o.1, Instant::now() - crate::offer::SETTLE)));
    ed.handle_key(Key::plain(Code::Char('w')));
    assert_eq!(ed.slot(), Some(Slot::Task));
    assert_eq!(ed.session.task_run.as_ref().unwrap().scroll, Some(at));
    assert!(ed.offers.iter().any(|o| o.stuck().is_some()), "still to answer");
    // (a quiet `docker run`'s work goes on in its container: a stand-in `docker` lists the run's — named from its
    // prefix — and writes down what it's asked)
    let prefix = ed.session.task_run.as_ref().unwrap().containers.as_ref().expect("Claude's").prefix.clone();
    let asked = dir.join("asked");
    std::fs::create_dir_all(dir.join("bin")).unwrap();
    let script = format!(
        "#!/bin/sh\necho \"$*\" >> '{}'\n[ \"$1\" = ps ] && printf '%s\\t%s\\n' '{prefix}-checks' \
         '2026-10-08 21:17:03 +0900 KST'\nexit 0\n",
        asked.display()
    );
    std::fs::write(dir.join("bin/docker"), script).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dir.join("bin/docker"), std::fs::Permissions::from_mode(0o755)).unwrap();
    let tx = ed.events.sender();
    ed.session.task_run.as_mut().unwrap().containers =
        Some(crate::containers::Run::on(&prefix, "Claude's command", tx, &dir.join("bin")));
    // `y`: stopped — its output stays, and says so — and its container with it, by name
    ed.handle_key(Key::plain(Code::Char('y')));
    assert!(ed.offers.iter().all(|o| o.stuck().is_none()));
    settle_until(&mut ed, |ed| {
        ed.session.task_run.as_ref().is_some_and(|r| r.state != crate::task::State::Running)
    });
    let end = Instant::now() + Duration::from_secs(5);
    while !std::fs::read_to_string(&asked).unwrap_or_default().contains(&format!("stop {prefix}-checks")) {
        assert!(Instant::now() < end, "its container stopped by name");
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = &ed.session.task_run.as_ref().unwrap().output;
    assert!(out.iter().any(|l| l.1 == head), "kept");
    assert!(
        out.iter().any(|l| l.1 == "── stopped from tarae's card: no output for 11m 00s, 0% CPU ──"),
        "{out:?}"
    );
    ed.session.task_run = None;
    std::fs::remove_dir_all(&dir).ok();
}

/// frictions29 35: `y` on the card stops what the run started in a session of its own too — a test's helper that
/// called `setsid` to take a pty was left running, blocked in a read. The run is the test's own `sh` starting
/// the stand-in (`procs::tests::setsid_helper`, in the test's `Scratch`): after `y` it's gone, and the run's output
/// says it was stopped with it.
///
/// (frictions29 90's ninth and tenth falls: the card says the run's command line and each of its threads, so its
/// height was the place it ran in's — the scratch folder under `$TMPDIR` and the test binary's path in that line,
/// a thread's state word under load — and at 120×40 it fitted with no row to spare: a longer `$TMPDIR` cut it, and
/// `y` on a cut card asks for it to be read first. The line is in a script of the run's own, `sh run.sh`; the
/// screen leaves the card room; and drawn whole is checked before the `y`, said if not.)
#[test]
fn a_stop_from_the_card_ends_what_left_the_runs_group() {
    let dir = crate::scratch::Scratch::new("stuck-apart");
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = dir.path().to_path_buf();
    let exe = std::env::current_exe().unwrap();
    let line = format!(
        "{}='{}' '{}' --exact procs::tests::setsid_helper --ignored --test-threads=1 >/dev/null 2>&1 & wait\n",
        crate::procs::tests::SETSID_HELPER,
        dir.path().display(),
        exe.display()
    );
    std::fs::write(dir.join("run.sh"), &line).unwrap();
    let cmd = "sh run.sh".to_string();
    let task = crate::task::Task { name: "apart".into(), cmd, cwd: dir.path().to_path_buf() };
    let user = crate::tools::For::User("apart".into());
    assert!(ed.task_start_for(task, user, None, Default::default()).is_ok());
    let file = dir.join("pid");
    let by = Instant::now() + Duration::from_secs(20);
    while !file.exists() {
        assert!(Instant::now() < by, "the stand-in never said its pid");
        std::thread::sleep(Duration::from_millis(10));
    }
    let helper: u32 = std::fs::read_to_string(&file).unwrap().trim().parse().unwrap();
    let started = crate::procs::read(helper).expect("the stand-in runs").started;
    let _end = crate::procs::tests::EndStandIn(helper, started);
    let r = ed.session.task_run.as_mut().unwrap();
    r.by_claude = true;
    let pid = r.pid().unwrap();
    let ago = |m: u32| Instant::now().checked_sub(MIN * m).unwrap();
    r.last_output = ago(11);
    r.wait.idle = Some(ago(11));
    ed.procs_sampled(sample(pid, 0.0));
    settle_until(&mut ed, |ed| ed.offers.iter().any(|o| o.stuck().is_some()));
    crate::term::render(&mut ed, &mut Vec::new(), 140, 60).unwrap();
    let card = &ed.offers[0];
    assert!(
        ed.offer_seen_whole(),
        "the card cut ({:?}) — its `y` would ask it read first:\n{}",
        ed.screen.offer_card,
        card.question
    );
    let o = (card.id, card.rev);
    ed.offer_seen.set(Some((o.0, o.1, Instant::now() - crate::offer::SETTLE)));
    ed.handle_key(Key::plain(Code::Char('y')));
    assert!(ed.offers.iter().all(|o| o.stuck().is_none()));
    let said = "── stopped with it: 1 process it started in a session of its own ──";
    let out = |ed: &Editor| ed.session.task_run.as_ref().map(|r| r.output.clone()).unwrap_or_default();
    settle_until(&mut ed, |ed| out(ed).iter().any(|l| l.1 == said || l.1.contains("outlived the stop")));
    let gone = !crate::procs::read(helper).is_some_and(|o| !o.zombie && o.started == started);
    assert!(gone, "the stand-in in a session of its own outlived the stop: {:?}", out(&ed));
    assert!(out(&ed).iter().any(|l| l.1 == said), "{:?}", out(&ed));
    assert!(!out(&ed).iter().any(|l| l.1.contains("outlived")), "{:?}", out(&ed));
    ed.session.task_run = None;
}

/// frictions15 #3 (a): a quiet run of Claude's that was granted a daemon's socket may be working there — a
/// build on a remote container engine printed nothing for minutes at 0% CPU here and was called stuck. Its
/// card is `Quiet:` and says what it can reach; one let reach nothing is still `Stuck?` (#181 — cargo hung
/// under qemu). Not at 12 minutes (cardburden 5: `llm.stuck-after-outside`, 30) — `quiet` in the state then.
/// The run is the test's own `sleep`; the socket a name only.
#[test]
fn a_quiet_run_let_out_to_a_socket_is_asked_as_maybe_working_elsewhere() {
    let dir = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("tarae-stuck-reach-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = dir.clone();
    let task = crate::task::Task { name: "builds".into(), cmd: "sleep 30".into(), cwd: dir.clone() };
    assert!(ed.task_start_for(task, crate::tools::For::User("b".into()), None, Default::default()).is_ok());
    let mut policy = crate::sandbox::policy(&dir, &ed.config.llm, &[]).unwrap();
    policy.sockets = vec![dir.join("engine.sock")];
    let ago = |m: u32| Instant::now().checked_sub(MIN * m).unwrap();
    let r = ed.session.task_run.as_mut().unwrap();
    (r.by_claude, r.last_output, r.wait.idle) = (true, ago(12), Some(ago(12)));
    r.guard = Some(crate::sandbox::Guard::In(Box::new(policy)));
    let pid = r.pid().unwrap();
    ed.procs_sampled(sample(pid, 0.0));
    assert_eq!(ed.session.task_run.as_ref().unwrap().wait.stuck_asked, None, "12 min: not asked");
    let runs = ed.ui_runs();
    assert_eq!(runs.iter().map(|r| r.state.as_str()).collect::<Vec<_>>(), ["quiet"]);
    let r = ed.session.task_run.as_mut().unwrap();
    (r.last_output, r.wait.idle) = (ago(31), Some(ago(31)));
    ed.procs_sampled(sample(pid, 0.0));
    settle_until(&mut ed, |ed| ed.offers.iter().any(|o| o.stuck().is_some()));
    assert_eq!(ed.ui_runs()[0].state, "running", "asked: no longer held");
    let o = &ed.offers[0];
    assert_eq!(o.title, "Quiet: Claude's command");
    let reach = format!(
        "It can reach {} — the work may be going on there: next to no CPU here doesn't mean it's stuck.",
        crate::allow::tilde(&dir.join("engine.sock"))
    );
    assert!(o.question.contains(&reach), "{}", o.question);
    assert_eq!(o.answers(false), [('y', "Stop it"), ('w', "What it waits on"), ('n', "Keep waiting")]);
    ed.session.task_run = None;
    std::fs::remove_dir_all(&dir).ok();
}

/// frictions15 #3 (b): what the run's processes are connected to outside its tree names where the work may
/// be — before what it was let reach (a run outside the sandbox can reach anything; its connection says
/// which). Neither: `Stuck?`, as #181.
#[test]
fn a_peer_outside_the_tree_is_named_on_the_card() {
    use crate::sandbox::ReachOut;
    let s = |reach: Option<ReachOut>| Stuck {
        slot: Slot::Task,
        run: 1,
        pid: 4242,
        asked: Instant::now(),
        quiet: 12 * MIN,
        cpu: 0.0,
        at: 0,
        reach,
    };
    let lines = ["pid 4242 · docker build . · 9 threads".to_string()];
    let label = "Claude's command docker build . in the task panel";
    let peers = ["~/.colima/default/docker.sock".to_string()];
    let o = card(s(Some(ReachOut::Anything)), label, &lines, &peers);
    assert_eq!(o.title, "Quiet: Claude's command");
    assert!(
        o.question
            .contains("\n\nIts processes are connected to ~/.colima/default/docker.sock — the work may"),
        "{}",
        o.question
    );
    assert!(!o.question.contains("It can reach"), "the connection says which: {}", o.question);
    let o = card(s(None), label, &lines, &[]);
    assert_eq!(o.title, "Stuck? Claude's command");
    assert!(!o.question.contains("may be going on there"), "{}", o.question);
}

/// The reviewer: a run with no sandbox around it, or the network open, was always `Quiet:` — "doesn't mean
/// it's stuck" — so #181's hang (cargo under qemu, no connection anywhere) was never called stuck for those
/// runs. Only a connection seen, or a daemon's socket granted, says where the work may be; the network or no
/// sandbox alone, nothing connected, is `Stuck?` with the reach a note.
#[test]
fn reaching_anything_with_nothing_connected_is_still_stuck() {
    use crate::sandbox::ReachOut;
    let s = |reach| Stuck {
        slot: Slot::Test,
        run: 1,
        pid: 4242,
        asked: Instant::now(),
        quiet: 12 * MIN,
        cpu: 0.0,
        at: 0,
        reach: Some(reach),
    };
    let lines = ["pid 4242 · cargo test · 3 threads".to_string()];
    let label = "Claude's tests in the test pane";
    for (reach, said) in
        [(ReachOut::Anything, "anything — no sandbox around it"), (ReachOut::Network, "the network")]
    {
        let o = card(s(reach), label, &lines, &[]);
        assert_eq!(o.title, "Stuck? Claude's tests", "{said}");
        let note = format!("It can reach {said}, but no connection outside its processes was seen.");
        assert!(o.question.contains(&note), "{}", o.question);
        assert!(!o.question.contains("doesn't mean it's stuck"), "{}", o.question);
    }
    // (connected, the same reach: where the work may be is known)
    let peers = ["10.0.0.7:2376".to_string()];
    assert_eq!(card(s(ReachOut::Network), label, &lines, &peers).title, "Quiet: Claude's tests");
}

/// cardburden 5 (b): a run found connected outside its processes (read at the stuck look, on a worker) when
/// it was let reach nothing in particular is held as one granted a socket: no card before
/// `llm.stuck-after-outside`, marked so the next looks wait as long, `quiet` in the state; then asked, `Quiet:`.
/// (The run is the test's own `sleep`; the peer a name only.)
#[test]
fn a_run_found_connected_outside_is_held_to_the_later_minutes() {
    let dir = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("tarae-stuck-peer-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = dir.clone();
    let task = crate::task::Task { name: "builds".into(), cmd: "sleep 30".into(), cwd: dir.clone() };
    assert!(ed.task_start_for(task, crate::tools::For::User("b".into()), None, Default::default()).is_ok());
    let ago = |m: u32| Instant::now().checked_sub(MIN * m).unwrap();
    let r = ed.session.task_run.as_mut().unwrap();
    (r.by_claude, r.last_output, r.wait.idle) = (true, ago(12), Some(ago(12)));
    let (pid, run) = (r.pid().unwrap(), r.trace.id());
    let s = |quiet| Stuck {
        slot: Slot::Task,
        run,
        pid,
        asked: Instant::now(),
        quiet,
        cpu: 0.0,
        at: 0,
        reach: None,
    };
    let peers = || vec!["10.0.0.7:2376".to_string()];
    // (the look at 12 minutes found it due, as one let reach nothing — then read its connection)
    ed.session.task_run.as_mut().unwrap().wait.stuck_asked = Some(Instant::now());
    ask(&mut ed, s(12 * MIN), Vec::new(), peers());
    assert!(ed.offers.is_empty(), "connected: not at 12 min");
    let w = &ed.session.task_run.as_ref().unwrap().wait;
    assert!(w.connected && w.stuck_asked.is_none(), "marked; its next look not put off by an ask");
    ed.procs_sampled(sample(pid, 0.0));
    assert_eq!(ed.session.task_run.as_ref().unwrap().wait.stuck_asked, None, "the next look waits 30 min");
    assert_eq!(ed.ui_runs()[0].state, "quiet");
    ask(&mut ed, s(31 * MIN), Vec::new(), peers());
    assert_eq!(ed.offers.len(), 1);
    assert_eq!(ed.offers[0].title, "Quiet: Claude's command");
    ed.session.task_run = None;
    std::fs::remove_dir_all(&dir).ok();
}

/// frictions15 #3 (b), for real: a child of the test connected to the test's own socket (a stand-in daemon in
/// its temp folder) — its tree's connection outside it is read and named by the socket's path.
#[cfg(target_os = "macos")]
#[test]
fn a_runs_connection_to_a_daemon_is_read_from_its_tree() {
    let dir = crate::scratch::Scratch::new("sd");
    let path = dir.join("d.sock");
    let daemon = std::os::unix::net::UnixListener::bind(&path).unwrap();
    // (`nc -U` connects and waits on its input, which is held open: a quiet client)
    let mut child = std::process::Command::new("nc")
        .arg("-U")
        .arg(&path)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn()
        .expect("nc");
    let _conn = daemon.accept().unwrap();
    let end = Instant::now() + Duration::from_secs(3);
    let mut said = connected_outside(child.id());
    while !said.iter().any(|s| s.ends_with("d.sock")) && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
        said = connected_outside(child.id());
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(said.iter().any(|s| s.ends_with("d.sock")), "{said:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// #200: a stuck card parked in a session behind, its run still silent, is looked at after every event
/// (`sync`) — and that look must not say the event changed nothing. It did: every key, resize and job came
/// back unchanged, nothing was drawn for 30 minutes (no card settled, no toast aged) until the run ended.
/// (The run is the test's own `sleep`, stopped at the end.)
#[test]
fn a_stuck_card_behind_leaves_the_front_drawn() {
    use crate::event::Event;
    let dir = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("tarae-stuck-behind-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("back")).unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = dir.clone();
    let back = ed.session_add(dir.join("back"));
    let ago = |m: u32| Instant::now().checked_sub(MIN * m).unwrap();
    ed.with_session(back, |ed| {
        let task = crate::task::Task { name: "hangs".into(), cmd: "sleep 30".into(), cwd: dir.join("back") };
        let user = crate::tools::For::User("h".into());
        assert!(ed.task_start_for(task, user, None, Default::default()).is_ok());
        let r = ed.session.task_run.as_mut().unwrap();
        (r.by_claude, r.last_output) = (true, ago(11));
        let s = Stuck {
            slot: Slot::Task,
            run: r.trace.id(),
            pid: r.pid().unwrap(),
            asked: Instant::now(),
            quiet: 11 * MIN,
            cpu: 0.0,
            at: 0,
            reach: None,
        };
        let o = crate::offer::Offer::new(
            "Stuck? Claude's command".into(),
            "No output for 11 min",
            "Stopping…",
            String::new(),
            crate::offer::What::Stuck(Box::new(s)),
        );
        ed.push_offer(o);
    });
    let parked = |ed: &Editor| ed.sessions.iter().any(|l| l.parked.iter().any(|o| o.stuck().is_some()));
    assert!(parked(&ed), "parked behind");
    // (its "waits for you" said once — what follows is the steady state that froze the screen)
    ed.handle_event(Event::Resize);
    let changed = |ed: &mut Editor, ev: Event| {
        ed.unchanged = false;
        ed.handle_event(ev);
        !std::mem::take(&mut ed.unchanged)
    };
    assert!(changed(&mut ed, Event::Key(Key::plain(Code::Char(':')))), "a key is drawn");
    assert!(ed.prompt.is_some(), "the command line opened");
    assert!(changed(&mut ed, Event::Resize), "a resize is drawn");
    assert!(changed(&mut ed, Event::Job(Box::new(|_: &mut Editor| {}))), "a job is drawn");
    assert!(parked(&ed), "its run still silent: the card stays");
    // (its `sleep` stopped as a dropped run is)
    ed.with_session(back, |ed| ed.session.task_run = None);
    std::fs::remove_dir_all(&dir).ok();
}

/// The card goes once its run prints — it wasn't stuck after all; `n` keeps waiting, said.
#[test]
fn a_stuck_card_goes_when_its_run_prints() {
    let dir = std::fs::canonicalize(std::env::temp_dir())
        .unwrap()
        .join(format!("tarae-stuck-prints-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = dir.clone();
    let task = crate::task::Task { name: "hangs".into(), cmd: "sleep 30".into(), cwd: dir.clone() };
    assert!(ed.task_start_for(task, crate::tools::For::User("h".into()), None, Default::default()).is_ok());
    let ago = |m: u32| Instant::now().checked_sub(MIN * m).unwrap();
    let r = ed.session.task_run.as_mut().unwrap();
    (r.by_claude, r.last_output, r.wait.idle) = (true, ago(12), Some(ago(12)));
    let pid = r.pid().unwrap();
    ed.procs_sampled(sample(pid, 0.0));
    settle_until(&mut ed, |ed| ed.offers.iter().any(|o| o.stuck().is_some()));
    // `n`: kept waiting, said — not asked again at the next look
    crate::term::render(&mut ed, &mut Vec::new(), 120, 40).unwrap();
    let o = (ed.offers[0].id, ed.offers[0].rev);
    ed.offer_seen.set(Some((o.0, o.1, Instant::now() - crate::offer::SETTLE)));
    ed.handle_key(Key::plain(Code::Char('n')));
    assert!(ed.offers.is_empty());
    let said = ed.messages.last().map(|m| m.2.clone()).unwrap_or_default();
    assert!(said.contains("kept waiting — asked again after 10 more min of nothing"), "{said}");
    ed.procs_sampled(sample(pid, 0.0));
    assert!(ed.offers.is_empty(), "not again so soon");
    // Asked again later; it prints — the card goes
    ed.session.task_run.as_mut().unwrap().wait.stuck_asked = Some(ago(11));
    ed.procs_sampled(sample(pid, 0.0));
    settle_until(&mut ed, |ed| ed.offers.iter().any(|o| o.stuck().is_some()));
    ed.session.task_run.as_mut().unwrap().last_output = Instant::now();
    ed.handle_event(crate::event::Event::Resize);
    assert!(ed.offers.is_empty(), "it printed: not stuck");
    ed.session.task_run = None;
    std::fs::remove_dir_all(&dir).ok();
}

/// `sample`, with a child `name` of the run's `pid` that started at `started` (epoch seconds; the rest started at
/// 1) and has used `worked` of CPU time.
fn sample_with_child(pid: u32, name: &str, started: u64, worked: Duration) -> Sample {
    sample_with_children(pid, &[(name, started, worked)])
}

/// `sample`, with children of the run's `pid` (pids `pid + 100_000` on): name, start, CPU time.
fn sample_with_children(pid: u32, kids: &[(&str, u64, Duration)]) -> Sample {
    let me = std::process::id();
    let os = |pid, ppid, name: &str, started, cpu| Os {
        pid,
        ppid,
        name: name.into(),
        started,
        cpu,
        ..Default::default()
    };
    let none = Duration::ZERO;
    let mut procs = vec![(os(me, 1, "tarae", 1, none), Some(1.0)), (os(pid, me, "sh", 1, none), Some(0.0))];
    for (i, (name, started, worked)) in kids.iter().enumerate() {
        procs.push((os(pid + 100_000 + i as u32, pid, name, *started, *worked), Some(0.0)));
    }
    Sample::of(procs)
}

/// frictions 53: a run that prints nothing at full CPU (a `grep` with a runaway pattern) is never stuck — the card
/// asks only at next to no CPU — but past `waits::BUSY` with no output it's said busy where the user looks: its
/// rows say its CPU, the inbox lists it with the key that stops it, and still no card. Under that, under a time
/// limit of its own still to come, below `BUSY_PCT` or with a process new in its tree lately, it isn't.
#[test]
fn a_run_busy_at_nothing_it_shows_is_said_not_asked() {
    let dir = crate::scratch::Scratch::new("stuck-busy");
    let (mut ed, pid) = quiet_run(dir.path(), "grep -a -o -E '.{0,220}x' big.bin");
    let me = std::process::id();
    let os = |pid, ppid, name: &str| Os { pid, ppid, name: name.into(), started: 1, ..Default::default() };
    let busy = |pct| Sample::of(vec![(os(me, 1, "tarae"), Some(1.0)), (os(pid, me, "grep"), Some(pct))]);
    let silent = |ed: &Editor| {
        let (now, r) = (Instant::now(), ed.session.task_run.as_ref().unwrap());
        (
            r.wait.silent(r.last_output, now).unwrap_or_default(),
            ed.inbox().into_iter().find(|e| e.kind == "silent-run"),
        )
    };
    // 11 minutes quiet at full CPU: said, listed with its stop key — no card
    ed.procs_sampled(busy(100.0));
    ed.handle_event(crate::event::Event::Resize);
    assert!(ed.offers.iter().all(|o| o.stuck().is_none()), "busy isn't stuck: {:?}", ed.offers);
    let (said, listed) = silent(&ed);
    assert!(said.starts_with("no output for 11m ") && said.ends_with(" · 100% CPU"), "{said}");
    let listed = listed.expect("in the inbox").asks;
    assert!(listed.contains("100% CPU — space m c stops it"), "{listed}");
    // Under BUSY, or at a share of a core below BUSY_PCT: neither
    ed.session.task_run.as_mut().unwrap().last_output = Instant::now().checked_sub(MIN * 9).unwrap();
    assert!(!silent(&ed).0.contains("CPU") && silent(&ed).1.is_none(), "{:?}", silent(&ed));
    ed.session.task_run.as_mut().unwrap().last_output = Instant::now().checked_sub(MIN * 11).unwrap();
    ed.procs_sampled(busy(30.0));
    assert!(!silent(&ed).0.contains("CPU") && silent(&ed).1.is_none(), "{:?}", silent(&ed));
    // A time limit of its own still to come, or a new process in its tree lately: neither
    ed.procs_sampled(busy(100.0));
    let r = ed.session.task_run.as_mut().unwrap();
    r.wait.limit_at = Instant::now().checked_add(MIN);
    assert!(!silent(&ed).0.contains("CPU"), "{:?}", silent(&ed));
    let r = ed.session.task_run.as_mut().unwrap();
    (r.wait.limit_at, r.wait.moved) = (None, Instant::now().checked_sub(MIN));
    assert!(!silent(&ed).0.contains("CPU") && silent(&ed).1.is_none(), "{:?}", silent(&ed));
    ed.session.task_run = None;
}

/// A run of Claude's started as `cmd`, quiet and idle for 11 minutes, as the looks before saw it.
fn quiet_run(dir: &std::path::Path, cmd: &str) -> (Editor, u32) {
    let mut ed = Editor::new(crate::config::Config::default());
    ed.session.root = dir.to_path_buf();
    let task = crate::task::Task { name: "scenes".into(), cmd: cmd.into(), cwd: dir.to_path_buf() };
    assert!(ed.task_start_for(task, crate::tools::For::User("s".into()), None, Default::default()).is_ok());
    let ago = Instant::now().checked_sub(MIN * 11).unwrap();
    let r = ed.session.task_run.as_mut().unwrap();
    (r.by_claude, r.last_output, r.wait.idle) = (true, ago, Some(ago));
    let pid = r.pid().unwrap();
    (ed, pid)
}

/// frictions34 39 a: a run whose work is a child after another (a scene's tarae each) prints nothing behind a
/// filter and reads 0% CPU between them — a process new in its tree since the last look that has done some work
/// is progress, as a line is: no card. Once none has come for the setting's minutes, it's asked about as before —
/// the card saying how long since its last line (20 min), not since that process.
#[test]
fn a_new_process_in_the_runs_tree_is_progress() {
    let dir = crate::scratch::Scratch::new("stuck-progress");
    let (mut ed, pid) = quiet_run(dir.path(), "sleep 30 2>&1 | tail -15");
    ed.session.task_run.as_mut().unwrap().last_output = Instant::now().checked_sub(MIN * 20).unwrap();
    ed.procs_sampled(sample_with_child(pid, "tarae", 2, Duration::from_secs(3)));
    let w = &ed.session.task_run.as_ref().unwrap().wait;
    assert!(w.moved.is_some() && w.at_work == [(pid + 100_000, 2)], "a new child seen");
    assert!(!w.worth_a_look(Instant::now(), Instant::now(), Instant::now()), "not for the inbox either");
    // (the stuck card would come on a job — none was started)
    ed.handle_event(crate::event::Event::Resize);
    assert!(ed.offers.iter().all(|o| o.stuck().is_none()), "working: not asked");
    // Nothing new for 11 minutes: asked
    let r = ed.session.task_run.as_mut().unwrap();
    r.wait.moved = Instant::now().checked_sub(MIN * 11);
    ed.procs_sampled(sample_with_child(pid, "tarae", 2, Duration::from_secs(3)));
    settle_until(&mut ed, |ed| ed.offers.iter().any(|o| o.stuck().is_some()));
    let q = &ed.offers.iter().find(|o| o.stuck().is_some()).unwrap().question;
    assert!(q.starts_with("No output for 20m 00s, 0% CPU"), "since its last line: {q}");
    // New work while the card is up (the next scene's tarae): the card goes, as it does on a line
    ed.procs_sampled(sample_with_child(pid, "tarae", 3, Duration::from_secs(1)));
    ed.handle_event(crate::event::Event::Resize);
    assert!(ed.offers.iter().all(|o| o.stuck().is_none()), "at work again: {:?}", ed.offers);
    ed.session.task_run = None;
}

/// frictions34 39 a's second review: a process at work the last look didn't see is progress whenever it started —
/// one older than the newest seen that does its work only now, one started in the same second as another (starts
/// are whole seconds) — and one already seen isn't, again.
#[test]
fn a_late_or_same_second_worker_is_progress() {
    let dir = crate::scratch::Scratch::new("stuck-late");
    let (mut ed, pid) = quiet_run(dir.path(), "sleep 30 | tail -5");
    let (busy, idle) = (Duration::from_secs(3), Duration::from_millis(5));
    let moved = |ed: &mut Editor| ed.session.task_run.as_mut().unwrap().wait.moved.take().is_some();
    ed.procs_sampled(sample_with_children(pid, &[("cargo", 5, busy), ("rustc", 2, idle)]));
    assert!(moved(&mut ed), "the first at work");
    ed.procs_sampled(sample_with_children(pid, &[("cargo", 5, busy), ("rustc", 2, idle)]));
    assert!(!moved(&mut ed), "nothing new");
    ed.procs_sampled(sample_with_children(pid, &[("cargo", 5, busy), ("rustc", 2, busy)]));
    assert!(moved(&mut ed), "started earlier, at work only now");
    let three = [("cargo", 5, busy), ("rustc", 2, busy), ("rustc", 5, busy)];
    ed.procs_sampled(sample_with_children(pid, &three));
    assert!(moved(&mut ed), "started in the same second as cargo");
    ed.session.task_run = None;
}

/// frictions34 39 a's review: a run hung in a polling loop (`until nc -z db 5432; do sleep 2; done`) starts a new
/// `sleep` before every look — behind a `tail` too, a process that has done next to no work isn't progress, and
/// it's asked about.
#[test]
fn a_polling_loops_new_sleeps_are_no_progress() {
    let dir = crate::scratch::Scratch::new("stuck-polling");
    let (mut ed, pid) = quiet_run(dir.path(), "sleep 30 | tail -5");
    ed.procs_sampled(sample_with_child(pid, "sleep", 2, Duration::from_millis(1)));
    assert!(ed.session.task_run.as_ref().unwrap().wait.moved.is_none(), "a sleep is no work");
    settle_until(&mut ed, |ed| ed.offers.iter().any(|o| o.stuck().is_some()));
    ed.session.task_run = None;
}

/// frictions34 39 a's third review: a polling loop around something heavier (`until kubectl get pod x; do sleep
/// 5; done`) has a step at work at every look. Its output not held, lines are the only sign of work, as before
/// 39 a: its hung wait is asked about. Behind a `| tail`, a new process at work is progress — lines can't show
/// it — and it isn't (the residual: a heavy poll loop behind a holding filter goes unasked).
#[test]
fn a_heavy_poller_is_asked_about_unless_its_output_is_held() {
    let dir = crate::scratch::Scratch::new("stuck-heavy-poller");
    let (mut ed, pid) = quiet_run(dir.path(), "sleep 30");
    ed.procs_sampled(sample_with_child(pid, "kubectl", 2, Duration::from_millis(800)));
    assert!(ed.session.task_run.as_ref().unwrap().wait.moved.is_none(), "lines are the only sign");
    settle_until(&mut ed, |ed| ed.offers.iter().any(|o| o.stuck().is_some()));
    ed.session.task_run = None;

    let (mut ed, pid) = quiet_run(dir.path(), "sleep 30 2>&1 | tail -15");
    ed.procs_sampled(sample_with_child(pid, "kubectl", 2, Duration::from_millis(800)));
    assert!(
        ed.session.task_run.as_ref().unwrap().wait.moved.is_some(),
        "held: a process at work is progress"
    );
    ed.handle_event(crate::event::Event::Resize);
    assert!(ed.offers.iter().all(|o| o.stuck().is_none()), "not asked");
    ed.session.task_run = None;
}

/// frictions34 39 a: what the card can't see, it says — a command whose output goes through a filter that holds
/// what it writes into the run's pipe (`tail`, `grep` alike), one wording with the filter's name; one asked for
/// its lines as they come (`grep --line-buffered`) lets only part of the output through.
#[test]
fn a_filtered_runs_card_says_why_nothing_shows() {
    let dir = crate::scratch::Scratch::new("stuck-filtered");
    let card = |cmd: &str| {
        let (mut ed, pid) = quiet_run(dir.path(), cmd);
        ed.procs_sampled(sample(pid, 0.0));
        settle_until(&mut ed, |ed| ed.offers.iter().any(|o| o.stuck().is_some()));
        let q = ed.offers.iter().find(|o| o.stuck().is_some()).unwrap().question.clone();
        ed.session.task_run = None;
        q
    };
    let q = card("sleep 30 2>&1 | tail -15");
    assert!(
        q.starts_with(
            "No output for 11m 00s, 0% CPU — Claude's command sleep 30 2>&1 | tail -15 in the task panel. Its \
             output goes through tail: what it lets through may show only when the run ends. Claude's turn \
             waits on it"
        ),
        "{q}"
    );
    let q = card("sleep 30 2>&1 | grep FAIL");
    assert!(
        q.contains("Its output goes through grep: what it lets through may show only when the run ends."),
        "{q}"
    );
    let q = card("sleep 30 2>&1 | grep --line-buffered FAIL");
    assert!(q.contains("Its output goes through grep: only what it lets through shows."), "{q}");
}
