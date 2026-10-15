use super::*;

/// 2026-10-08 21:17:03 +0900 — the stand-in's run's own container was made then.
const MADE: i64 = 1_791_461_823;

/// Another run's container (a parallel session's), made while the stand-in's run went.
const ANOTHER_RUNS: &str = "tarae-77-s2-r5-checks";

/// A stand-in `docker` in a folder of its own: it writes down what it's asked (`asked`, a line each) and with
/// what engine settings (`env`: its `DOCKER_HOST`, `DOCKER_CONTEXT`, `DOCKER_CONFIG` and the files in that
/// folder) and, asked to list, answers with the run's own container (made at `MADE`), one made a minute later
/// under another name (Claude's by hand), another run's made then too, and one made a week before (not the
/// run's business).
fn stand_in(name: &str, prefix: &str) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let dir = crate::scratch::Scratch::new(&format!("containers-{name}")).into_path();
    let log = dir.join("asked");
    let script = format!(
        "#!/bin/sh\necho \"$*\" >> '{log}'\n\
         held=$([ -n \"$DOCKER_CONFIG\" ] && cd \"$DOCKER_CONFIG\" && find . -type f | sort | tr '\\n' ' ')\n\
         echo \"DOCKER_HOST=${{DOCKER_HOST-unset}} DOCKER_CONTEXT=${{DOCKER_CONTEXT-unset}} \
         DOCKER_CONFIG=${{DOCKER_CONFIG-unset}} holding: $held\" >> '{env}'\n\
         case \"$1\" in\n\
         ps) printf '%s\\t%s\\n' '{prefix}-checks' '2026-10-08 21:17:03 +0900 KST' \
         'eager_turing' '2026-10-08 21:18:00.123456 +0900 KST' '{ANOTHER_RUNS}' '2026-10-08 21:18:30 +0900 KST' \
         'old_db' '2026-10-01 09:00:00 +0900 KST' ;;\n\
         stop) echo \"$2\" ;;\nesac\n",
        log = log.display(),
        env = dir.join("env").display()
    );
    let docker = dir.join("docker");
    std::fs::write(&docker, script).unwrap();
    std::fs::set_permissions(&docker, std::fs::Permissions::from_mode(0o755)).unwrap();
    (dir, log)
}

fn asked(log: &Path) -> Vec<String> {
    std::fs::read_to_string(log).unwrap_or_default().lines().map(str::to_string).collect()
}

/// tarae's packs say how docker's and podman's containers are listed and stopped — by `{name}`; an entry
/// that names no `{name}` to stop, or says what tarae doesn't read, is refused (it'd stop the wrong thing).
#[test]
fn the_packs_say_how_each_engine_lists_and_stops() {
    let e = engines();
    let docker = e.iter().find(|e| e.program == "docker").expect("docker's");
    assert!(docker.stop.iter().any(|a| a == "{name}"), "{docker:?}");
    assert!(docker.list.iter().any(|a| a.contains("{{.Names}}")), "{docker:?}");
    assert!(e.iter().any(|e| e.program == "podman"));
    let read = |s: &str| engines_of(&s.parse::<toml::Table>().unwrap()["containers"]);
    assert!(read("[[containers]]\nprogram = \"x\"\nlist = [\"ps\"]\nstop = [\"rm\", \"-f\"]").is_err());
    assert!(read("[[containers]]\nprogram = \"x\"\nlist = []\nstop = [\"{name}\"]\nkill = []").is_err());
}

/// A container is the run's by its prefix — the prefix, or it then `-…`: run 1's isn't run 12's.
#[test]
fn a_container_is_the_runs_by_its_prefix() {
    let p = "tarae-42-s0-r1";
    assert!(ours("tarae-42-s0-r1", p) && ours("tarae-42-s0-r1-checks", p));
    assert!(!ours("tarae-42-s0-r12-checks", p) && !ours("x-tarae-42-s0-r1", p) && !ours("eager_turing", p));
    let made = prefix(3, 7);
    assert!(made.starts_with(&format!("tarae-{}-s3-r7", std::process::id())));
    assert!(made.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'), "a name an engine takes");
}

/// When a container was made, as docker and podman print it (a fraction of a second too), in its own zone.
#[test]
fn when_a_container_was_made_is_read_in_its_zone() {
    assert_eq!(created("2026-10-08 21:17:03 +0900 KST"), Some(MADE));
    assert_eq!(created("2026-10-08 12:17:03 +0000 UTC"), Some(MADE));
    assert_eq!(created("2026-10-08 05:17:03.987 -0700 PDT"), Some(MADE));
    assert_eq!(created("1970-01-01 00:00:00 +0000 UTC"), Some(0));
    for not in ["", "2 hours ago", "2026-10-08 21:17 +0900", "2026-10-08 21:17:03 KST"] {
        assert_eq!(created(not), None, "{not:?}");
    }
}

/// A stopped run's look: its own (by prefix) to stop; others made since it started (a little before, for
/// the engine's clock) to ask about — but one carrying another run's prefix (a parallel session's `cargo
/// xtask linux`, still at work) is that run's: apart, never asked about; one made long before, or whose time
/// can't be read, none of them.
#[test]
fn a_look_sorts_the_runs_own_from_the_others_made_while_it_ran() {
    let p = "tarae-42-s0-r1";
    let listed = format!(
        "{p}-checks\t2026-10-01 09:00:00 +0900 KST\n\
         eager_turing\t2026-10-08 21:18:00 +0900 KST\n\
         {ANOTHER_RUNS}\t2026-10-08 21:18:30 +0900 KST\n\
         tarae-42-s0-r12\t2026-10-08 21:18:30 +0900 KST\n\
         old_db\t2026-10-01 09:00:00 +0900 KST\n\
         odd\tnot a time\n\n"
    );
    let l = look(&listed, p, MADE);
    assert_eq!(l.own, [format!("{p}-checks")], "its own whenever made");
    assert_eq!(l.meanwhile, ["eager_turing"]);
    assert_eq!(l.others, [ANOTHER_RUNS, "tarae-42-s0-r12"], "another session's, and this tarae's run 12");
    // (an engine whose clock is a little behind: still one made while it ran)
    let l = look("late\t2026-10-08 21:17:00 +0900 KST\n", p, MADE);
    assert_eq!(l.meanwhile, ["late"]);
}

/// A run's prefix, as `prefix` makes it, is known on any container — the prefix alone or with `-…` after it;
/// a name merely like it isn't one.
#[test]
fn a_runs_prefix_is_known_on_any_container() {
    assert!(a_runs(&prefix(3, 7)) && a_runs(&format!("{}-checks", prefix(3, 7))));
    assert!(a_runs(ANOTHER_RUNS) && a_runs("tarae-1-s0-r9"));
    for not in
        ["tarae-x-s0-r1", "tarae-1-s0", "tarae-1-s0-r", "tarae-1-s0-r1x", "my-tarae-1-s0-r1", "eager_turing"]
    {
        assert!(!a_runs(not), "{not}");
    }
}

/// Which engine a run reached: what its line sets in front of a command (`DOCKER_HOST=… cargo …`, `env
/// DOCKER_CONTEXT=… …`, after a `cd`, before a pipe) over its environment's; its environment's alone when its
/// line names none — or can't be read; each way it reached one apart. Only the engine's own variables, each
/// engine its own.
#[test]
fn a_runs_engine_is_what_its_line_sets_over_its_environment() {
    let e = engines();
    let engine = |p: &str| e.iter().find(|e| e.program == p).unwrap();
    let (docker, podman) = (engine("docker"), engine("podman"));
    let env = |n: &str| match n {
        "DOCKER_HOST" => Some("tcp://from-env:2375".to_string()),
        "CONTAINER_HOST" => Some("unix:///podman.sock".to_string()),
        _ => None,
    };
    let pair = |n: &str, v: &str| (n.to_string(), v.to_string());
    let line = "cd xtask && DOCKER_HOST=unix:///run.sock RUST_LOG=1 cargo xtask linux 2>&1 | tail -5";
    let r = Reach::of(Some(line), env);
    assert_eq!(r.sets(docker), [vec![pair("DOCKER_HOST", "unix:///run.sock")]], "its line's, RUST_LOG not");
    assert_eq!(r.sets(podman), [vec![pair("CONTAINER_HOST", "unix:///podman.sock")]], "its environment's");
    let r = Reach::of(Some("cargo xtask linux"), env);
    assert_eq!(r.sets(docker), [vec![pair("DOCKER_HOST", "tcp://from-env:2375")]]);
    let r = Reach::of(Some("env DOCKER_CONTEXT=colima-tarae-test cargo xtask linux"), env);
    let both = vec![pair("DOCKER_CONTEXT", "colima-tarae-test"), pair("DOCKER_HOST", "tcp://from-env:2375")];
    assert_eq!(r.sets(docker), [both]);
    let r = Reach::of(Some("DOCKER_HOST=unix:///a.sock x; DOCKER_HOST=unix:///b.sock y; true"), env);
    assert_eq!(
        r.sets(docker),
        [vec![pair("DOCKER_HOST", "unix:///a.sock")], vec![pair("DOCKER_HOST", "unix:///b.sock")]]
    );
    let r = Reach::of(Some("DOCKER_HOST=unix://$HOME/x.sock cargo xtask linux"), env);
    assert!(r.unread);
    assert_eq!(
        r.sets(docker),
        [vec![pair("DOCKER_HOST", "tcp://from-env:2375")]],
        "can't tell: its environment"
    );
    let r = Reach::of(None, |_| None);
    assert_eq!(r.sets(docker), [Vec::new()], "named none: the default engine");
}

/// A stopped run's calls reach the engine the run did — its `DOCKER_HOST`, its context — and never with the
/// user's config folder (its credential helpers would run): each gets an empty one of tarae's, gone after;
/// what a named context needs (its endpoint, `contexts/meta`) copied in, the config itself never. A run that
/// named none: the default engine, nothing copied.
#[test]
fn a_stopped_runs_calls_reach_its_engine_with_none_of_the_users_config() {
    let prefix = "tarae-1-s0-r3";
    let (dir, _) = stand_in("reach", prefix);
    // (the user's config folder, a stand-in: credentials and a context)
    let users = dir.join("home/.docker");
    std::fs::create_dir_all(users.join("contexts/meta/ab12")).unwrap();
    std::fs::write(users.join("config.json"), r#"{"credsStore":"tarae-fake-helper"}"#).unwrap();
    std::fs::write(users.join("contexts/meta/ab12/meta.json"), r#"{"Name":"colima-tarae-test"}"#).unwrap();
    let ed = Editor::new(crate::config::Config::default());
    let stop = |line: &str, env: &dyn Fn(&str) -> Option<String>| -> Vec<String> {
        let _ = std::fs::remove_file(dir.join("env"));
        let reach = Reach::of(Some(line), env);
        let run = Run::on(prefix, "Claude's command", ed.events.sender(), &dir)
            .reaching(reach, Some(&dir.join("home")));
        assert_eq!(run.stop_now(), [format!("{prefix}-checks")]);
        std::fs::read_to_string(dir.join("env")).unwrap().lines().map(str::to_string).collect()
    };
    let users_too = |n: &str| (n == "DOCKER_CONFIG").then(|| users.display().to_string());
    let tarae_s = |said: &str| {
        let folder = said.split("DOCKER_CONFIG=").nth(1).unwrap().split(' ').next().unwrap().to_string();
        assert!(
            folder.contains("tarae-engine-") && !Path::new(&folder).exists(),
            "tarae's, gone after: {said}"
        );
        said.replace(&folder, "<tarae's>")
    };
    // Its line's DOCKER_HOST, the user's DOCKER_CONFIG in its environment: the host passed, the config not
    let said = stop("DOCKER_HOST=unix:///run.sock cargo xtask linux test", &users_too);
    assert_eq!(said.len(), 2, "the list and the stop: {said:?}");
    for s in &said {
        assert_eq!(
            tarae_s(s),
            "DOCKER_HOST=unix:///run.sock DOCKER_CONTEXT=unset DOCKER_CONFIG=<tarae's> holding: "
        );
    }
    // A context named: its endpoint copied in, nothing else of the folder
    for s in &stop("DOCKER_CONTEXT=colima-tarae-test cargo xtask linux test", &users_too) {
        assert_eq!(
            tarae_s(s),
            "DOCKER_HOST=unset DOCKER_CONTEXT=colima-tarae-test DOCKER_CONFIG=<tarae's> \
             holding: ./contexts/meta/ab12/meta.json "
        );
    }
    // …from home's when the run named no folder of its own
    for s in &stop("DOCKER_CONTEXT=colima-tarae-test cargo xtask linux test", &|_| None) {
        assert!(tarae_s(s).ends_with("holding: ./contexts/meta/ab12/meta.json "), "{s}");
    }
    // Named none: the default engine, an empty folder
    for s in &stop("cargo xtask linux test", &|_| None) {
        assert_eq!(tarae_s(s), "DOCKER_HOST=unset DOCKER_CONTEXT=unset DOCKER_CONFIG=<tarae's> holding: ");
    }
    std::fs::remove_dir_all(&dir).ok();
}

/// Live (`--ignored`: a Docker at `TARAE_LIVE_DOCKER_HOST`, its image pulled; the test process itself with
/// no `DOCKER_HOST` or `DOCKER_CONTEXT`): Claude's command `DOCKER_HOST=… cargo xtask linux test …`, started
/// as tarae starts one the user let run outside the sandbox (`task_start_for`) — its engine named only in its
/// line. Stopped mid-way as tarae stops one that doesn't end on TERM — its process group killed
/// (`stuck::stop`'s `KILL_AFTER`), then the run closed (`space m c`: its containers stopped on a worker) — it
/// leaves no container. A killed client can't pass its stop on: its container ran on (the friction) — here it
/// must still be running before tarae's stop, and gone after.
#[test]
#[ignore]
fn live_a_stopped_xtask_linux_run_leaves_no_container() {
    use std::process::{Command, Stdio};
    for own in ["DOCKER_HOST", "DOCKER_CONTEXT"] {
        assert!(std::env::var_os(own).is_none(), "run it without {own}: the run's line names its engine");
    }
    let Ok(host) = std::env::var("TARAE_LIVE_DOCKER_HOST") else {
        return eprintln!(
            "live_a_stopped_xtask_linux_run_leaves_no_container: no TARAE_LIVE_DOCKER_HOST — skipped"
        );
    };
    // (what's running there — the test's own look, its engine named here, never through tarae's environment)
    let config = std::env::temp_dir().join(format!("tarae-live-dc-{}", std::process::id()));
    std::fs::create_dir_all(&config).unwrap();
    let names = || -> String {
        let mut ps = Command::new("docker");
        ps.args(["ps", "--format", "{{.Names}}"]).env("DOCKER_HOST", &host).stdin(Stdio::null());
        let out = ps.env("DOCKER_CONFIG", &config).output();
        out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
    };
    let mut ed = Editor::new(crate::config::Config::default());
    let line = format!(
        "DOCKER_HOST={host} DOCKER_CONFIG=/tmp/tarae-dc cargo xtask linux test --test e2e \
         every_thread_tarae_starts_has_a_name"
    );
    let task = crate::task::Task { name: "live".into(), cmd: line, cwd: env!("CARGO_MANIFEST_DIR").into() };
    let who = crate::tools::For::User("ran it outside the sandbox".into());
    assert!(ed.task_start_for(task, who, None, crate::queued::Ride::default()).is_ok());
    let r = ed.session.task_run.as_mut().unwrap();
    let path = std::env::var_os("PATH").unwrap_or_default();
    r.containers.as_mut().expect("Claude's").find_on(&path);
    let prefix = r.containers.as_ref().unwrap().prefix.clone();
    let checks = format!("{prefix}-checks");
    // (mid-way: the checkout in, its checks starting — the container's script says `toolchain: …` then. Before
    // that, a client cut off mid-stream takes the checkout's tar down with it, and the container ends itself)
    let end = std::time::Instant::now() + std::time::Duration::from_secs(600);
    while !ed.session.task_run.as_ref().unwrap().output.iter().any(|l| l.1.starts_with("toolchain: ")) {
        let r = ed.session.task_run.as_ref().unwrap();
        assert!(r.state == crate::task::State::Running, "it ended: {:?}", r.output);
        assert!(std::time::Instant::now() < end, "its checks never started");
        if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(100)) {
            ed.handle_event(ev);
        }
    }
    assert!(names().lines().any(|n| n == checks), "named from the prefix: {}", names());
    // Its process group killed, as a run that didn't end on TERM is: its container runs on. (Its end isn't
    // heard yet — no event handled — so the run is closed as a running one, as the stuck card's stop does.)
    let pid = ed.session.task_run.as_ref().unwrap().pid().expect("running");
    // SAFETY: a KILL to the process group of the run this test started (setsid: a group of its own).
    unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
    std::thread::sleep(std::time::Duration::from_secs(2));
    assert!(names().lines().any(|n| n == checks), "the killed client's container ran on: {}", names());
    // …until tarae's stop: on the engine its line named, by name — said in its session once done
    ed.task_close();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(60);
    let status = |ed: &Editor| ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
    while !status(&ed).contains(&checks) {
        assert!(std::time::Instant::now() < end, "said: {}", status(&ed));
        if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(100)) {
            ed.handle_event(ev);
        }
    }
    assert_eq!(status(&ed), format!("stopped with it, by name: {checks}"));
    // (gone by then, not some time later: a container whose checks end by themselves would be gone too — it's
    // `--rm` — and say nothing of tarae's stop)
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while names().lines().any(|n| ours(n, &prefix)) {
        assert!(std::time::Instant::now() < end, "no container left: {}", names());
        std::thread::sleep(std::time::Duration::from_millis(250));
    }
}

/// A stopped run: its own container stopped, by name; the one made by hand while it ran asked about on a
/// card — not stopped without `y` (`n` leaves it), stopped with it; another run's, made then too, left alone
/// and said, never on the card; the week-old one never touched; and no call names a pid — every one is the
/// engine's `ps` or `stop <name>`.
#[test]
fn a_stopped_runs_containers_are_stopped_by_name_and_others_asked_about() {
    let prefix = "tarae-1-s0-r9";
    let (dir, log) = stand_in("stop", prefix);
    let mut ed = Editor::new(crate::config::Config::default());
    let mut run = Run::on(prefix, "Claude's command `cargo xtask linux`", ed.events.sender(), &dir);
    run.started = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(MADE as u64);
    assert_eq!(run.stop_now(), [format!("{prefix}-checks")]);
    let card = |ed: &mut Editor| {
        let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !ed.offers.iter().any(|o| matches!(o.what, crate::offer::What::Containers(_))) {
            assert!(std::time::Instant::now() < end, "the card came");
            if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(50)) {
                ed.handle_event(ev);
            }
        }
    };
    card(&mut ed);
    let o = ed.offers.iter().find(|o| matches!(o.what, crate::offer::What::Containers(_))).unwrap();
    assert!(o.question.contains("    eager_turing") && !o.question.contains("old_db"), "{}", o.question);
    assert!(!o.question.contains(ANOTHER_RUNS), "another run's never on the card: {}", o.question);
    assert!(o.question.contains("Claude's command `cargo xtask linux`") && o.weighty(), "{}", o.question);
    // Said, beside what was stopped: another run's, left alone
    let status = |ed: &Editor| ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !status(&ed).starts_with("stopped with it") {
        assert!(std::time::Instant::now() < end, "said: {}", status(&ed));
        if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(50)) {
            ed.handle_event(ev);
        }
    }
    assert_eq!(
        status(&ed),
        format!("stopped with it, by name: {prefix}-checks · another run's, left alone: {ANOTHER_RUNS}")
    );
    assert_eq!(asked(&log), ["ps --format {{.Names}}\t{{.CreatedAt}}", &format!("stop {prefix}-checks")]);
    // n: left running, said — nothing sent to stop it (counted at the end: y's is the only one)
    ed.offer_dismiss();
    let said = ed.status.as_ref().map(|s| s.0.clone()).unwrap_or_default();
    assert_eq!(said, "left running on docker: eager_turing");
    // asked again, y: stopped by name
    let mut run = Run::on(prefix, "Claude's command `cargo xtask linux`", ed.events.sender(), &dir);
    run.started = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(MADE as u64);
    run.stop_now();
    card(&mut ed);
    ed.offer_accept();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !asked(&log).iter().any(|l| l == "stop eager_turing") {
        assert!(std::time::Instant::now() < end, "y stopped it: {:?}", asked(&log));
        if let Some(ev) = ed.events.recv_timeout(std::time::Duration::from_millis(50)) {
            ed.handle_event(ev);
        }
    }
    for l in asked(&log) {
        let (verb, rest) = l.split_once(' ').unwrap_or((&l, ""));
        assert!(verb == "ps" || (verb == "stop" && !rest.chars().all(|c| c.is_ascii_digit())), "{l}");
    }
    assert!(
        !asked(&log).iter().any(|l| l.contains("old_db") || l.contains(ANOTHER_RUNS)),
        "{:?}",
        asked(&log)
    );
    // (its `y` the only stop of it: `n` sent none — a stop sent by `n` lands before `y`'s, on the same worker
    // pool, and would be counted here)
    let stops = asked(&log).iter().filter(|l| *l == "stop eager_turing").count();
    assert_eq!(stops, 1, "{:?}", asked(&log));
    std::fs::remove_dir_all(&dir).ok();
}
