//! Step 7 on macOS (linux7 piece 6): two sessions side by side under Seatbelt — from a run of one at what the
//! other's runs have: its processes, its sockets and shared memory, its files. Seatbelt has no keeper and no
//! namespaces: a session is its policy (its project, its run's own folder, its kept homes) and what one run is
//! kept from is what its profile names. The same three tests as Linux's (`keeper::tests::two_sessions_…`), each
//! with its expectations as a table; what macOS can't keep apart is pinned as shared, and the report must say
//! it. Terminals are 3c's (`tests::a_run_cant_open_another_terminal`). Seatbelt can't start inside a sandbox:
//! each test is `skipped_inside` there.

use super::tests::scratch;
use super::*;

/// A name of nobody's in the sessions' environment: left out of their runs, so the report has a first time to
/// say what's left out (`RunEnv::said`).
const LEFT_OUT: &str = "TARAE_APART_OTHER";

/// A session for these tests: the checkout at `root` with what a run of its gets to write but its project, as
/// Linux's `Apart::writing` has it — its own folder (`RunDir`) in a `runs` of `home`'s, where every session's
/// are, a kept run home of its project's and the home a daemon of its policy lives from (`with_kept`), each
/// made as a run that needs it would; reads confined to `home` (standing for one) but for the repository's
/// other checkouts. `ports`: the loopback ports its policy opens (`llm.sandbox-ports`).
struct Session {
    root: PathBuf,
    policy: Policy,
}

impl Session {
    fn new(root: &Path, home: &Path, ports: Vec<u16>) -> Session {
        std::fs::create_dir_all(root.join("src")).unwrap();
        let root = profile_path(root);
        // (`runs`, not `.cache/tarae/runs`: a socket's path in a run's folder stays under 104 bytes)
        let (runs, homes) = (home.join("runs"), home.join("run-homes"));
        let key = project_key(&root);
        let mut here = Here::now();
        here.env.insert(LEFT_OUT.into(), "x".into());
        let env = Env {
            root: root.clone(),
            tmp: Some(profile_path(&std::env::temp_dir())),
            runs: Some(runs.clone()),
            kept_homes: vec![("kept".into(), homes.join("kept").join(&key))],
            daemon_homes: vec![("daemon".into(), homes.join("daemon").join(&key))],
            ports,
            reads: Reads {
                home: Some(home.to_path_buf()),
                allow: crate::allow::checkouts(&root),
                ..Default::default()
            },
            run_env: run_env_of(&here, &crate::llm::LlmConfig::default()),
            ..Default::default()
        };
        let mut policy = Policy::for_project(&env);
        with_kept(&mut policy, &env);
        let run = RunDir::new_in(&runs).unwrap();
        // (its denials watched by its run's tag, as `for_its_run` gives it)
        policy.tag = Some(run.tag());
        policy.run = Some(run);
        for k in &policy.kept {
            std::fs::create_dir_all(&k.home).unwrap();
        }
        Session { root, policy }
    }

    /// The run's own folder (its `TMPDIR`).
    fn run_dir(&self) -> PathBuf {
        self.policy.run.as_ref().unwrap().path().to_path_buf()
    }

    /// `program args` as a run of this session's, in its project, with `env` set over its own.
    fn start(&self, program: &Path, args: &[String], env: &[(&str, &str)]) -> crate::spawn::Spawned {
        let mut cmd = command(Some(&self.policy), &program.to_string_lossy(), args);
        cmd.current_dir(&self.root).stdin(Stdio::null()).envs(env.iter().copied());
        spawn(Some(&self.policy), &cmd).unwrap()
    }

    /// `start`, waited for: what it wrote, stdout then stderr.
    fn run(&self, program: &Path, args: &[String], env: &[(&str, &str)]) -> String {
        let s = self.start(program, args, env);
        let mut out = String::new();
        io::Read::read_to_string(&mut s.stdout.unwrap(), &mut out).unwrap();
        io::Read::read_to_string(&mut s.stderr.unwrap(), &mut out).unwrap();
        crate::spawn::wait(s.pid).unwrap();
        after_run(Some(&self.policy))();
        out
    }

    /// This test binary on `helper` doing `what` as a run of this session's: the words of its `apart:` line.
    fn helper(&self, what: &str, env: &[(&str, &str)]) -> String {
        let exe = std::env::current_exe().unwrap();
        let mut env = env.to_vec();
        env.push(("TARAE_APART_DO", what));
        let out = self.run(&exe, &libtest("sandbox::apart::helper", &[]), &env);
        match out.lines().find_map(|l| l.strip_prefix("apart: ")) {
            Some(said) => said.to_string(),
            None => panic!("the helper said nothing:\n{out}"),
        }
    }

    /// What the report says of this session's runs (no denials: the rules).
    fn report(&self) -> String {
        let r = Guard::In(Box::new(self.policy.clone())).report(&[]);
        r.split_whitespace().collect::<Vec<_>>().join(" ")
    }

    fn end(self) {
        after_run(Some(&self.policy))();
    }
}

/// This test binary's arguments running only the ignored test `name` — `also` as more filters, which name
/// nothing: they mark its command line.
fn libtest(name: &str, also: &[&str]) -> Vec<String> {
    let mut args = vec!["--exact".to_string(), name.into()];
    args.extend(also.iter().map(|a| a.to_string()));
    args.extend(["--ignored", "--nocapture"].map(String::from));
    args
}

/// A stand-in's arguments and environment for its marker: `<marker>-arg` on its command line, `<marker>-env` in
/// its environment.
fn stand_in_of(marker: &str) -> (Vec<String>, String) {
    (libtest("sandbox::apart::stand_in", &[&format!("{marker}-arg")]), format!("{marker}-env"))
}

/// Reads lines of `out` until the stand-in says it's there.
fn ready(out: impl io::Read) -> impl io::Read {
    let mut out = io::BufReader::new(out);
    loop {
        let mut line = String::new();
        assert!(io::BufRead::read_line(&mut out, &mut line).unwrap() > 0, "the stand-in said nothing");
        if line.trim() == "ready" {
            return out;
        }
    }
}

/// `ok`, or the errno.
fn tried<T>(r: io::Result<T>) -> String {
    match r {
        Ok(_) => "ok".into(),
        Err(e) => e.raw_os_error().map_or_else(|| format!("{:?}", e.kind()), |n| n.to_string()),
    }
}

/// A libc call's `-1` as its errno.
fn io_result(r: libc::c_int) -> io::Result<()> {
    if r == -1 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

/// What one session's run got trying another session's (or tarae's).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reach {
    /// Refused: by Seatbelt, or by macOS itself.
    Refused,
    /// It went through: shared between sessions here — known, and the report must say it.
    Shared,
}

/// The stand-in: says it's there, then sleeps — a minute at most, ended by its test before.
#[test]
#[ignore = "run by two_sessions_cant_reach_each_others_processes in a process of its own"]
fn stand_in() {
    if std::env::var_os("TARAE_APART_ENV").is_none() {
        return;
    }
    println!("ready");
    for _ in 0..60 {
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// The helper, as a run of a session's: `$TARAE_APART_DO` — `processes` (`processes_tried`) or `sockets`
/// (`sockets_tried`) — said on one `apart:` line of `what=result` words.
#[test]
#[ignore = "run by the two_sessions_ tests as a run of one session's"]
fn helper() {
    let Ok(what) = std::env::var("TARAE_APART_DO") else { return };
    let said = match what.as_str() {
        "processes" => processes_tried(),
        "sockets" => sockets_tried(),
        other => panic!("{other}?"),
    };
    println!("apart: {said}");
}

/// `processes`: its own child first (a stand-in it starts: the control — a try that reaches nothing would
/// pass), then each of `$TARAE_TO` (`who:pid:marker` words), every attempt on each (`reach`) — `who-what=…`.
fn processes_tried() -> String {
    use std::os::unix::process::CommandExt;
    let marker = format!("own-{}", std::process::id());
    let (args, env) = stand_in_of(&marker);
    let mut own = Command::new(std::env::current_exe().unwrap());
    own.args(&args).env("TARAE_APART_ENV", &env).stdout(Stdio::piped()).process_group(0);
    let mut own = own.spawn().unwrap();
    let _out = ready(own.stdout.take().unwrap());
    let mut said = vec![reach("own", own.id() as libc::pid_t, &marker)];
    for to in std::env::var("TARAE_TO").unwrap().split_whitespace() {
        let mut parts = to.splitn(3, ':');
        let (who, pid, marker) = (parts.next().unwrap(), parts.next().unwrap(), parts.next().unwrap());
        said.push(reach(who, pid.parse().unwrap(), marker));
    }
    own.kill().ok();
    own.wait().ok();
    said.join(" ")
}

/// Every attempt of one process on another of the same user's, `who-what=ok` or its errno (`lldb`: `ok`,
/// `refused`, `timeout` or `unclear`): signal 0 and CONT; its details (`proc_pidinfo`, as `lsof -p` reads
/// them); its command line and environment (`KERN_PROCARGS2`, as `ps -wwE` reads them — `ok` only with
/// `<marker>-arg` / `<marker>-env` in them); `setpriority` to 7; a debugger attached and detached (`lldb -p`:
/// Apple's debugserver takes the task port — a bare `task_for_pid` of this binary's is refused by macOS even on
/// its own child, so it would pin nothing).
fn reach(who: &str, pid: libc::pid_t, marker: &str) -> String {
    // SAFETY: plain calls on a pid; signal 0 only asks, CONT wakes nothing that sleeps on a timer.
    let (kill0, cont) = unsafe { (libc::kill(pid, 0), libc::kill(pid, libc::SIGCONT)) };
    let info = {
        // SAFETY: a plain out-struct of the size given; the call only fills it in.
        let mut bi: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // SAFETY: as above.
        match unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut bi).cast(), size) } {
            n if n == size => Ok(()),
            _ => Err(io::Error::last_os_error()),
        }
    };
    let (args, env) = match procargs(pid) {
        Ok((args, env)) => {
            let has = |words: &[String], m: &str| match words.iter().any(|w| w.contains(m)) {
                true => "ok".to_string(),
                false => "unmarked".to_string(),
            };
            (has(&args, &format!("{marker}-arg")), has(&env, &format!("{marker}-env")))
        }
        Err(e) => (tried::<()>(Err(e)), "-".to_string()),
    };
    // SAFETY: a plain call on a pid.
    let prio = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, 7) };
    format!(
        "{who}-kill0={} {who}-cont={} {who}-info={} {who}-args={args} {who}-env={env} {who}-prio={} \
         {who}-lldb={}",
        tried(io_result(kill0)),
        tried(io_result(cont)),
        tried(info),
        tried(io_result(prio)),
        attached(pid),
    )
}

/// Process `pid`'s command line and starting environment (`KERN_PROCARGS2`: argc, the executable's path, NULs,
/// the words, then the environment).
fn procargs(pid: libc::pid_t) -> io::Result<(Vec<String>, Vec<String>)> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    let null = std::ptr::null_mut();
    // SAFETY: a size query (no buffer), then a read into a buffer of that size; both only fill in.
    let buf = unsafe {
        io_result(libc::sysctl(mib.as_mut_ptr(), 3, null, &mut size, null, 0))?;
        let mut buf = vec![0u8; size];
        io_result(libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, null, 0))?;
        buf.truncate(size);
        buf
    };
    let bad = || io::Error::other("not KERN_PROCARGS2's form");
    let argc = i32::from_ne_bytes(buf.get(..4).ok_or_else(bad)?.try_into().unwrap()).max(0) as usize;
    let rest = &buf[4..];
    let rest = &rest[rest.iter().position(|&b| b == 0).ok_or_else(bad)?..];
    let rest = &rest[rest.iter().position(|&b| b != 0).ok_or_else(bad)?..];
    let mut words = rest.split(|&b| b == 0).map(|w| String::from_utf8_lossy(w).into_owned());
    let args: Vec<String> = words.by_ref().take(argc).collect();
    let env: Vec<String> = words.take_while(|w| !w.is_empty()).collect();
    Ok((args, env))
}

/// `lldb -p pid`, detached at once: `ok` (it stopped the process — attached), `refused` (attach failed),
/// `timeout` (30 s), `unclear` (neither said), `absent` (no lldb here).
fn attached(pid: libc::pid_t) -> String {
    let lldb = Path::new("/usr/bin/lldb");
    if !lldb.exists() {
        return "absent".into();
    }
    let mut c = Command::new(lldb);
    c.args(["--batch", "--no-lldbinit", "-p", &pid.to_string(), "-o", "process detach"]);
    let mut child = c.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
    let end = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > end {
            child.kill().ok();
            child.wait().ok();
            return "timeout".into();
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let out = child.wait_with_output().unwrap();
    let said = String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    if said.contains(&format!("Process {pid} stopped")) {
        "ok".into()
    } else if said.contains("attach failed") {
        "refused".into()
    } else {
        eprintln!("lldb on {pid}: {said}");
        "unclear".into()
    }
}

/// Step 7 on Seatbelt: two sessions' policies side by side, a run of each — from one, everything a process
/// can do to another of the same user's, on its own child (the control), on the other session's run (its own
/// `sandbox-exec`) and on tarae's stand-in (outside the sandbox, a process group of its own). Seatbelt's
/// `same-sandbox` keeps signals, details and a debugger's attach in (`OWN_PROCESSES`, F2; the attach since this
/// test found `lldb -p` reaching tarae's stand-in — a way to run anything outside the sandbox); the command line
/// and the starting environment (`KERN_PROCARGS2`), and `setpriority`, no rule of Seatbelt's keeps — pinned as
/// shared, and the report must say it (`RunEnv::said`'s first time; `MAC_SHARED`). The stand-ins are this test
/// binary (no platform binary: what macOS refuses every debugger of a system program says nothing of the
/// sandbox); they're signalled CONT only, attached and detached at once, and what a `setpriority` they were let
/// do is checked on them from here. A refused attach the watch saw names a stand-in, never one of its own.
#[test]
fn two_sessions_cant_reach_each_others_processes() {
    use Reach::*;
    let test = "sandbox::apart::two_sessions_cant_reach_each_others_processes";
    if skipped_inside(test) {
        return;
    }
    use std::os::unix::process::CommandExt;
    let t = scratch("ap-procs");
    let (mine, theirs) = (Session::new(&t.join("p"), &t, vec![]), Session::new(&t.join("q"), &t, vec![]));
    let exe = std::env::current_exe().unwrap();
    let pid = std::process::id();
    let (tarae_marker, their_marker) = (format!("tarae-{pid}"), format!("their-{pid}"));
    // tarae's stand-in, outside the sandbox, in a process group of its own
    let (args, env) = stand_in_of(&tarae_marker);
    let mut tarae = Command::new(&exe);
    tarae.args(&args).env("TARAE_APART_ENV", &env).stdout(Stdio::piped()).process_group(0);
    let mut tarae = tarae.spawn().unwrap();
    let tarae_pid = tarae.id();
    let _tarae_out = ready(tarae.stdout.take().unwrap());
    // Another session's run: its own policy, its own sandbox-exec
    let (args, env) = stand_in_of(&their_marker);
    let mut their = theirs.start(&exe, &args, &[("TARAE_APART_ENV", &env)]);
    let _their_out = ready(their.stdout.take().unwrap());
    let to = format!("tarae:{}:{tarae_marker} their:{}:{their_marker}", tarae.id(), their.pid);
    // (its denials, as its report says them)
    let w = Watch::start(mine.policy.tag.as_deref().expect("a run's tag"));
    std::thread::sleep(WATCH_READY);
    let said = mine.helper("processes", &[("TARAE_TO", &to)]);
    std::thread::sleep(WATCH_LAST * 2);
    let blocked = w.blocked();
    w.finish();
    let attaches: Vec<String> =
        blocked.seen.iter().filter(|d| d.op == "mach-priv-task-port").map(|d| d.plain()).collect();
    // (what a `setpriority` did, seen from here; the stand-ins still there — never ended by the run)
    let prio = |p: u32| {
        // SAFETY: a plain query on a pid (-1 is a priority too: errno says which).
        unsafe { libc::getpriority(libc::PRIO_PROCESS, p as libc::id_t) }
    };
    // SAFETY: signal 0 only asks whether the process is there.
    let alive = |p: u32| unsafe { libc::kill(p as libc::pid_t, 0) } == 0;
    let (tarae_prio, their_prio) = (prio(tarae.id()), prio(their.pid));
    let (tarae_alive, their_alive) = (alive(tarae.id()), alive(their.pid));
    tarae.kill().ok();
    tarae.wait().ok();
    // SAFETY: the other run's stand-in, started above and not waited on yet.
    unsafe { libc::kill(their.pid as libc::pid_t, libc::SIGKILL) };
    crate::spawn::wait(their.pid).ok();
    // What the report says of what's left out of a run's environment, the first time
    let env_said = mine.policy.run_env.said(&mut Vec::new(), Os::Mac).unwrap_or_default();
    let report = mine.report();
    mine.end();
    theirs.end();
    std::fs::remove_dir_all(&t).ok();
    let got: HashMap<&str, &str> = said.split_whitespace().filter_map(|w| w.split_once('=')).collect();
    let line = format!("tried: {said}\n  attaches refused, as said: {attaches:?}\n");
    let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
    assert!(tarae_alive && their_alive, "the stand-ins outlived the run: {line}");
    // (attempt, on tarae's stand-in and the other session's run)
    let table = [
        ("kill0", Refused),
        ("cont", Refused),
        ("info", Refused),
        ("args", Shared),
        ("env", Shared),
        ("prio", Shared),
        ("lldb", Refused),
    ];
    let said_of = |what: &str| match what {
        "args" | "env" => env_said.ends_with(runenv::OTHERS_READ),
        "prio" => report.contains("can lower their priority"),
        _ => false,
    };
    let mut wrong = Vec::new();
    for (what, want) in table {
        // The control: its own child, every attempt through
        if got[&*format!("own-{what}")] != "ok" {
            wrong.push(format!("its own child: {what} not ok — the try reaches nothing"));
        }
        for who in ["their", "tarae"] {
            let verdict = if got[&*format!("{who}-{what}")] == "ok" { Shared } else { Refused };
            if verdict != want {
                wrong.push(format!("{who}: {what} {verdict:?}, not {want:?}"));
            }
            if verdict == Shared && !said_of(what) {
                wrong.push(format!(
                    "{who}: {what} is shared between sessions here, and the report doesn't say so"
                ));
            }
        }
    }
    // What a `setpriority` went through did, and only then
    for (who, p) in [("their", their_prio), ("tarae", tarae_prio)] {
        if (got[&*format!("{who}-prio")] == "ok") != (p == 7) {
            wrong.push(format!(
                "{who}: priority {p} after the run, its setpriority said {}",
                got[&*format!("{who}-prio")]
            ));
        }
    }
    // A refused attach the watch saw is a stand-in's (by pid: the name is this binary's for all three)
    let stand_ins = [tarae_pid, their.pid].map(|pid| format!(" ({pid}) — outside the run's own processes"));
    for a in &attaches {
        if !stand_ins.iter().any(|s| a.ends_with(s)) && !a.starts_with("attaching to a process") {
            wrong.push(format!("an attach refused that isn't a stand-in's: {a}"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}\n{line}");
}

/// `sockets`: each of `$TARAE_OWN` (the control), then of `$TARAE_TO` (`what=place` lines) — a unix socket
/// connected to (`checkout`, `run`), a loopback port (`tcp-…`), a POSIX shared memory name opened (`posix`), a
/// System V segment attached by key (`sysv`); its own POSIX name and System V segment made and opened —
/// `[own-]what=ok` or its errno.
fn sockets_tried() -> String {
    let try_one = |what: &str, place: &str| -> String {
        let r = match what {
            "checkout" | "run" => std::os::unix::net::UnixStream::connect(place).map(drop),
            w if w.starts_with("tcp") => {
                let at = std::net::SocketAddr::from(([127, 0, 0, 1], place.parse::<u16>().unwrap()));
                std::net::TcpStream::connect_timeout(&at, std::time::Duration::from_secs(2)).map(drop)
            }
            "posix" => posix_opened(place, false),
            "sysv" => sysv_attached(place.parse().unwrap(), false),
            other => panic!("{other}?"),
        };
        tried(r)
    };
    let list = |var: &str| -> Vec<(String, String)> {
        let v = std::env::var(var).unwrap_or_default();
        v.lines().filter_map(|l| l.split_once('=')).map(|(w, p)| (w.into(), p.into())).collect()
    };
    let mut said = Vec::new();
    for (what, place) in list("TARAE_OWN") {
        said.push(format!("own-{what}={}", try_one(&what, &place)));
    }
    let own = format!("/tarae-own-{}", std::process::id());
    said.push(format!("own-posix={}", tried(posix_opened(&own, true))));
    said.push(format!("own-sysv={}", tried(sysv_attached(libc::IPC_PRIVATE, true))));
    for (what, place) in list("TARAE_TO") {
        said.push(format!("{what}={}", try_one(&what, &place)));
    }
    said.join(" ")
}

/// POSIX shared memory `name` opened read-write — `made`: made first (then removed again).
fn posix_opened(name: &str, made: bool) -> io::Result<()> {
    let c = std::ffi::CString::new(name).unwrap();
    let flags = if made { libc::O_RDWR | libc::O_CREAT | libc::O_EXCL } else { libc::O_RDWR };
    // SAFETY: a NUL-terminated name; the descriptor is closed at once (and the name made here removed).
    unsafe {
        let fd = libc::shm_open(c.as_ptr(), flags, 0o600 as libc::c_uint);
        io_result(fd)?;
        libc::close(fd);
        if made {
            libc::shm_unlink(c.as_ptr());
        }
    }
    Ok(())
}

/// The System V segment of `key` attached — `made`: a new private one (then removed again).
fn sysv_attached(key: libc::key_t, made: bool) -> io::Result<()> {
    let flags = if made { libc::IPC_CREAT | 0o600 } else { 0 };
    // SAFETY: a segment found or made by key, attached and detached at once (one made here removed).
    unsafe {
        let id = libc::shmget(key, if made { 4096 } else { 0 }, flags);
        io_result(id)?;
        let at = libc::shmat(id, std::ptr::null(), 0);
        let r = if at as isize == -1 { Err(io::Error::last_os_error()) } else { Ok(()) };
        if r.is_ok() {
            libc::shmdt(at);
        }
        if made {
            libc::shmctl(id, libc::IPC_RMID, std::ptr::null_mut());
        }
        r
    }
}

/// Step 7 on Seatbelt: two sessions side by side — from a run of one at what a run of the other holds: a unix
/// socket in its checkout and in its run's own folder, a loopback listener on a port this session's policy
/// opens (`llm.sandbox-ports`) and on one it doesn't, a POSIX shared memory name and a System V segment.
/// Each is made by this test where a run of the other session's would make it (Seatbelt's rules are on
/// places and names, never on who made one). The trying run reaches its own of each first: a try that
/// reaches nothing would pass — the opened port is the closed one's control. A port the user opens is this
/// machine's (no loopback of a session's own on macOS) — shared, and the report says so; shared memory, POSIX
/// and System V, no rule of Seatbelt's keeps apart — shared, and said (`MAC_SHARED`). Harmless: every
/// listener and segment is this test's own, and nothing is sent. (The sessions in `/private/tmp`: a socket's
/// path has 104 bytes, and a run's folder under a checkout's `target/` is past it. A unix socket there is still
/// refused but in the run's own folder and project — `unix_sockets_only_in_the_run_and_the_project`.)
#[test]
fn two_sessions_cant_reach_each_others_sockets() {
    use Reach::*;
    use std::os::unix::net::UnixListener;
    let test = "sandbox::apart::two_sessions_cant_reach_each_others_sockets";
    if skipped_inside(test) {
        return;
    }
    let t = PathBuf::from(format!("/private/tmp/tarae-ap-{}", std::process::id()));
    std::fs::remove_dir_all(&t).ok();
    std::fs::create_dir_all(&t).unwrap();
    let theirs = Session::new(&t.join("q"), &t, vec![]);
    let (open, closed) = (
        std::net::TcpListener::bind("127.0.0.1:0").unwrap(),
        std::net::TcpListener::bind("127.0.0.1:0").unwrap(),
    );
    let port = |l: &std::net::TcpListener| l.local_addr().unwrap().port();
    let mine = Session::new(&t.join("p"), &t, vec![port(&open)]);
    let socket = |dir: &Path| {
        let at = dir.join("s");
        assert!(at.as_os_str().len() < 104, "{}: past a socket's path", at.display());
        (UnixListener::bind(&at).unwrap(), at)
    };
    let (_mine_checkout, own_checkout) = socket(&mine.root.join("src"));
    let (_mine_run, own_run) = socket(&mine.run_dir());
    let (_their_checkout, their_checkout) = socket(&theirs.root.join("src"));
    let (_their_run, their_run) = socket(&theirs.run_dir());
    // The other session's shared memory: a POSIX name and a System V segment
    let pid = std::process::id();
    let posix = format!("/tarae-ap-{pid}");
    let name = std::ffi::CString::new(posix.clone()).unwrap();
    let key = 0x7a00_0000 | (pid as libc::key_t & 0x00ff_ffff);
    // SAFETY: a name and a segment of this test's own, made new (removed below).
    let (fd, seg) = unsafe {
        let fd =
            libc::shm_open(name.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600 as libc::c_uint);
        (fd, libc::shmget(key, 4096, libc::IPC_CREAT | libc::IPC_EXCL | 0o600))
    };
    assert!(fd >= 0 && seg >= 0, "{}", io::Error::last_os_error());
    let own = format!("checkout={}\nrun={}\n", own_checkout.display(), own_run.display());
    let to = format!(
        "checkout={}\nrun={}\ntcp-open={}\ntcp-closed={}\nposix={posix}\nsysv={key}\n",
        their_checkout.display(),
        their_run.display(),
        port(&open),
        port(&closed),
    );
    let said = mine.helper("sockets", &[("TARAE_OWN", &own), ("TARAE_TO", &to)]);
    // SAFETY: this test's own name and segment, made above.
    unsafe {
        libc::close(fd);
        libc::shm_unlink(name.as_ptr());
        libc::shmctl(seg, libc::IPC_RMID, std::ptr::null_mut());
    }
    let report = mine.report();
    mine.end();
    theirs.end();
    std::fs::remove_dir_all(&t).ok();
    let got: HashMap<&str, &str> = said.split_whitespace().filter_map(|w| w.split_once('=')).collect();
    let line = format!("tried: {said}\n  report: {report}\n");
    let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
    let mut wrong = Vec::new();
    for what in ["checkout", "run", "posix", "sysv"] {
        if got[&*format!("own-{what}")] != "ok" {
            wrong.push(format!("its own {what}: not ok — the try reaches nothing"));
        }
    }
    let table = [
        ("checkout", Refused),
        ("run", Refused),
        ("tcp-open", Shared),
        ("tcp-closed", Refused),
        ("posix", Shared),
        ("sysv", Shared),
    ];
    let said_of = |what: &str| match what {
        // (step 8, decision 9: another session's listener on an opened port is reached here, and said)
        "tcp-open" => {
            report.contains(&format!("this machine's ports {}", port(&open)))
                && report.contains("another tarae session's commands' too")
        }
        "posix" | "sysv" => {
            report.contains("shares System V and POSIX shared memory with other sessions' commands")
        }
        _ => false,
    };
    for (what, want) in table {
        let verdict = if got[what] == "ok" { Shared } else { Refused };
        if verdict != want {
            wrong.push(format!("{what}: {verdict:?}, not {want:?}"));
        }
        if verdict == Shared && !said_of(what) {
            wrong.push(format!("{what} is shared between sessions here, and the report doesn't say so"));
        }
    }
    assert!(wrong.is_empty(), "{wrong:#?}\n{line}");
}

/// Step 7 on Seatbelt: two sessions side by side, files — from a run of one into each place a run of the other
/// writes but the temp folders (shared, D16): its checkout, its run's own folder (in the same `runs` as this
/// one's, as every session's are), its project's kept run home, the home a daemon of its policy lives from — in
/// each a file made and the one there written. The profile names only its own session's places, so each is
/// refused. The trying run writes its own of each first: a try that writes nothing would pass. The
/// repository's other checkouts are read by design (D16) — pinned as shared, said by the report, with a folder
/// beside them in home refused: reads are confined there, so the read is the policy's grant, not an open home.
#[test]
fn two_sessions_cant_write_each_others_files() {
    let test = "sandbox::apart::two_sessions_cant_write_each_others_files";
    if skipped_inside(test) {
        return;
    }
    // (under this crate's `target/`, standing for home: outside the temp folders, where every run writes)
    let home = scratch("ap-files");
    let (main, wt) = crate::allow::tests::repo_with_worktree(&home);
    let elsewhere = home.join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let (mine, theirs) = (Session::new(&main, &home, vec![]), Session::new(&wt, &home, vec![]));
    let writes = |s: &Session| -> Vec<(&'static str, PathBuf)> {
        let kept = |daemon| s.policy.kept.iter().find(|k| k.daemon == daemon).unwrap().home.clone();
        vec![
            ("checkout", s.root.join("src")),
            ("run", s.run_dir()),
            ("kept", kept(false)),
            ("daemon", kept(true)),
        ]
    };
    let (own, others) = (writes(&mine), writes(&theirs));
    for (_, dir) in own.iter().chain(&others) {
        std::fs::write(dir.join("there"), "").unwrap();
    }
    std::fs::write(elsewhere.join("there"), "").unwrap();
    // One `w what 'attempt'` line each, its place in a variable of its own
    let mut script = String::from(
        "w() { if ( eval \"$2\" ) 2>/dev/null; then printf '%s=ok ' \"$1\"; else printf '%s=no ' \"$1\"; fi; }\n",
    );
    let mut vars: Vec<(String, String)> = Vec::new();
    for (pre, list) in [("own-", &own), ("", &others)] {
        for (what, dir) in list {
            let v = format!("D{}", vars.len());
            script.push_str(&format!("w {pre}{what}-make 'printf x > \"${v}/made\"'\n"));
            script.push_str(&format!("w {pre}{what}-write 'printf x >> \"${v}/there\"'\n"));
            vars.push((v, dir.display().to_string()));
        }
    }
    // (a file of its own: `there` in the other checkout is what must stay empty)
    for (what, file) in [("checkout", wt.join("src/read")), ("elsewhere", elsewhere.join("read"))] {
        std::fs::write(&file, "x").unwrap();
        let v = format!("D{}", vars.len());
        script.push_str(&format!("w {what}-read 'cat \"${v}\" > /dev/null'\n"));
        vars.push((v, file.display().to_string()));
    }
    let env: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let said = mine.run(Path::new("/bin/sh"), &["-c".into(), script], &env);
    // What landed in the other session's places, seen from here
    let landed: Vec<PathBuf> = others
        .iter()
        .flat_map(|(_, d)| std::fs::read_dir(d).into_iter().flatten().flatten())
        .map(|e| e.path())
        .filter(|f| match f.file_name().and_then(|n| n.to_str()) {
            Some("there") => std::fs::metadata(f).is_ok_and(|m| m.len() > 0),
            Some("read") => std::fs::read(f).is_ok_and(|c| c != b"x"),
            _ => true,
        })
        .collect();
    let report = mine.report();
    mine.end();
    theirs.end();
    std::fs::remove_dir_all(&home).ok();
    let got: HashMap<&str, &str> = said.split_whitespace().filter_map(|w| w.split_once('=')).collect();
    let line = format!("tried: {said}\n");
    let _ = io::Write::write_all(&mut io::stderr(), line.as_bytes());
    let mut wrong = Vec::new();
    for (what, _) in &own {
        for how in ["make", "write"] {
            if got[&*format!("own-{what}-{how}")] != "ok" {
                wrong.push(format!("its own {what}: {how} not ok — the try reaches nothing"));
            }
            if got[&*format!("{what}-{how}")] != "no" {
                wrong.push(format!("the other session's {what}: {how} not refused"));
            }
        }
    }
    if got["checkout-read"] != "ok" {
        wrong.push("the repository's other checkout isn't read (D16: shared)".into());
    }
    if !report.contains("the repository's other checkouts") {
        wrong.push(format!("the other checkout is read, and the report doesn't say so: {report}"));
    }
    if got["elsewhere-read"] != "no" {
        wrong.push("a folder in home that's no checkout is read: the checkout's read pins nothing".into());
    }
    assert!(wrong.is_empty() && landed.is_empty(), "{wrong:#?}\nlanded: {landed:?}\n{line}");
}
