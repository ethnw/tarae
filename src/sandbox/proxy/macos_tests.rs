//! macOS's runs and tarae's proxy (step 5d, `target/sb/step5d-plan.md`), through the real path: the run's own
//! port and token (`RunPort`), its profile (`Policy::profile`), its environment (`sandbox::command`), its start
//! and end (`sandbox::spawn`, `sandbox::after_run`). The Seatbelt ones say so and skip inside a sandbox. Nothing
//! leaves this machine: the outside is the test's own loopback listener under an `example.invalid` name.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use super::RunPort;
use super::parse::{self, Auth};
use crate::sandbox::tests::{env, scratch};
use crate::sandbox::{Guard, Here, Policy, Watch, after_run, command, run_env_of, skipped_inside, spawn};

/// A policy for `root` with the network open and a port of its own to tarae's proxy, as `policy_in` makes it.
pub(super) fn open(root: &Path) -> Policy {
    let p = Policy::for_project(&crate::sandbox::Env { network: true, ..env(root) });
    Policy { proxy: Some(RunPort::new(p.run_id).unwrap()), ..p }
}

/// What `c` sets, by name.
fn env_of(c: &Command) -> HashMap<String, String> {
    let set = c.get_envs().filter_map(|(k, v)| Some((k.to_str()?.to_string(), v?.to_str()?.to_string())));
    set.collect()
}

/// A request head read from `s`, to its blank line (within 10 s).
pub(super) fn head(mut s: &TcpStream) -> Vec<u8> {
    s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        match s.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    buf
}

pub(super) use super::packs_tests::finish;

/// The tests' own time limit holds: a run past it is ended (`Deadline` — its process group, the run's own), so
/// the test fails instead of hanging. The stand-in run is a `sleep` this test started.
#[test]
fn a_run_past_its_time_is_ended() {
    let t = Instant::now();
    let (status, ..) = finish(crate::spawn::setsid(Command::new("/bin/sleep").arg("30")).unwrap(), 1);
    assert!(t.elapsed() < Duration::from_secs(10), "ended at its time: {:?}", t.elapsed());
    assert!(status.is_some_and(|s| !s.success()), "{status:?}");
}

/// The first connection `l` takes within `secs` — None: none came (a test fails, never hangs).
pub(super) fn accept_within(l: &TcpListener, secs: u64) -> Option<TcpStream> {
    l.set_nonblocking(true).unwrap();
    let by = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < by {
        if let Ok((s, _)) = l.accept() {
            s.set_nonblocking(false).unwrap();
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    None
}

/// The outside, standing in: a loopback listener of the test's own under `name` (`TEST_NAMES`), taken for
/// beyond in the runs of `root`'s project (`TEST_BEYOND`). Its port, and its first request's head (answered
/// `200`, `ok`) with the listener after — None when none came within `secs`.
fn outside(
    name: &str,
    root: &Path,
    secs: u64,
) -> (u16, std::thread::JoinHandle<(Option<String>, TcpListener)>) {
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let at = l.local_addr().unwrap();
    super::TEST_NAMES.lock().unwrap().push((name.into(), at));
    super::TEST_BEYOND.lock().unwrap().push((root.to_path_buf(), at.ip()));
    let seen = crate::event::thread("tarae-5d-test", move || {
        let h = accept_within(&l, secs).map(|s| {
            let h = head(&s);
            let _ = (&s).write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            String::from_utf8_lossy(&h).into_owned()
        });
        (h, l)
    });
    (at.port(), seen)
}

/// `script` run by `sh` through the real path — `command`, `spawn`, its wait, `after_run` — in `p`: what it
/// printed.
fn run(p: &Policy, script: &str) -> String {
    let mut c = command(Some(p), "/bin/sh", &["-c".into(), script.into()]);
    c.current_dir(&p.root);
    let ended = after_run(Some(p));
    let (_, out, errs) = finish(spawn(Some(p), &c).unwrap(), 60);
    ended();
    if !errs.is_empty() {
        eprintln!("stderr: {errs}");
    }
    out
}

/// A run's token is its own — two runs, two tokens, 64 hex digits each (32 bytes from `getentropy`) — and the
/// proxy variables its environment holds are tarae's: each names the run's own port with its token as the
/// password (`run:` the user: git and pip send nothing with a user alone — M1), this machine's loopback left
/// direct, git told to send it up front after the settings the run already holds. The user's own `HTTPS_PROXY`
/// and `GIT_CONFIG_COUNT` are neither passed nor said as left out (§macOS flow: set on top, never a
/// pass-through). The network closed: none of them. A policy's `Debug` shows the port, never the token.
#[test]
fn a_runs_token_is_its_own_and_never_passed_through() {
    let here = Here {
        env: HashMap::from([
            ("HTTPS_PROXY".to_string(), std::ffi::OsString::from("http://users-proxy.example.invalid:3128")),
            ("GIT_CONFIG_COUNT".to_string(), "1".into()),
            ("PATH".to_string(), "/usr/bin:/bin".into()),
        ]),
        ..Here::default()
    };
    let any = crate::llm::Hosts::Any;
    let llm = crate::llm::LlmConfig { sandbox_network: true, sandbox_hosts: any, ..Default::default() };
    let run_env = run_env_of(&here, &llm);
    for n in ["HTTPS_PROXY", "GIT_CONFIG_COUNT"] {
        assert!(run_env.var(n).is_none(), "{n} passed");
        assert!(!run_env.left_out().any(|l| l == n), "{n} said as left out");
    }
    let root = Path::new("/Users/u/p");
    let (a, b) = (open(root), open(root));
    let (ta, tb) = (a.proxy.clone().unwrap(), b.proxy.clone().unwrap());
    assert_ne!(ta.token(), tb.token(), "two runs, one token");
    for t in [ta.token(), tb.token()] {
        assert!(t.len() == 64 && t.bytes().all(|c| c.is_ascii_hexdigit()), "{t}");
    }
    // (a policy's `Debug` is its key and what logs print: the port, never the token)
    assert!(!format!("{a:?}").contains(ta.token()) && format!("{a:?}").contains(&ta.port.to_string()));
    let a = Policy { run_env, ..a };
    let set = env_of(&command(Some(&a), "true", &[]));
    let url = format!("http://run:{}@127.0.0.1:{}", ta.token(), ta.port);
    for k in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"] {
        assert_eq!(set.get(k), Some(&url), "{k}");
    }
    assert_eq!(set.get("NO_PROXY").map(String::as_str), Some("localhost,127.0.0.1,::1"));
    let git = |set: &HashMap<String, String>, n: usize| {
        let at = |k: String| set.get(&k).cloned().unwrap_or_default();
        [
            at("GIT_CONFIG_COUNT".into()),
            at(format!("GIT_CONFIG_KEY_{n}")),
            at(format!("GIT_CONFIG_VALUE_{n}")),
        ]
    };
    assert_eq!(git(&set, 0), [(1).to_string(), "http.proxyAuthMethod".into(), "basic".into()]);
    // (git's own settings the user passed — `llm.sandbox-env` — stay: tarae's goes after them)
    let theirs = crate::llm::LlmConfig { sandbox_env: vec!["GIT_CONFIG_*".into()], ..llm };
    let passed = Policy { run_env: run_env_of(&here, &theirs), ..a.clone() };
    let set = env_of(&command(Some(&passed), "true", &[]));
    assert_eq!(git(&set, 1), [(2).to_string(), "http.proxyAuthMethod".into(), "basic".into()]);
    let closed = Policy { network: false, proxy: None, ..a };
    let set = env_of(&command(Some(&closed), "true", &[]));
    assert!(!set.contains_key("HTTPS_PROXY") && !set.contains_key("GIT_CONFIG_COUNT"), "{set:?}");
}

/// git through a run's proxy variables sends the token with its first CONNECT — no 407 first (M1: git's
/// default, `anyauth`, sends it only after one: a round trip more on every connection). Against a stand-in on
/// the run's own port (the proxy's listener, taken before any proxy has it), answered 403 so git ends; the
/// host is `example.invalid`, nothing leaves. git reads no one's settings: its home a folder of the test's own,
/// `GIT_CONFIG_GLOBAL=/dev/null`, no system file, run from a folder outside any repository.
#[test]
fn git_sends_its_password_up_front() {
    let p = open(Path::new("/Users/u/p"));
    let port = p.proxy.as_ref().unwrap();
    let stand_in = TcpListener::from(port.take().unwrap());
    let set = env_of(&command(Some(&p), "true", &[]));
    let home = std::env::temp_dir().join(format!("tarae-5d-git-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let home = std::fs::canonicalize(&home).unwrap();
    let url = "https://a-5d.example.invalid/x.git";
    crate::gitrepo::not_in_this_repository(&home, &["ls-remote", url]);
    let answered = crate::event::thread("tarae-5d-test", move || {
        let Some(s) = accept_within(&stand_in, 15) else { return Vec::new() };
        let h = head(&s);
        let _ = (&s).write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        h
    });
    let mut git = Command::new("git");
    // (nothing of this process's environment: each name removed — `spawn::setsid` doesn't see `env_clear`)
    for (k, _) in std::env::vars_os() {
        git.env_remove(k);
    }
    git.args(["ls-remote", url])
        .envs(set.iter().filter(|(k, _)| k.to_lowercase().ends_with("_proxy") || k.starts_with("GIT_CONFIG")))
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CEILING_DIRECTORIES", home.parent().unwrap())
        .env("GIT_TERMINAL_PROMPT", "0")
        .current_dir(&home);
    let (_, _, said) = finish(crate::spawn::setsid(&git).unwrap(), 30);
    let first = answered.join().unwrap();
    std::fs::remove_dir_all(&home).ok();
    let h = match parse::head(&first) {
        Ok(Some(h)) => h,
        r => panic!("not a request head: {r:?} — {:?}; git: {said}", String::from_utf8_lossy(&first)),
    };
    match h.auth {
        Auth::Basic { user, password } => {
            assert_eq!((user.as_slice(), password.as_slice()), (b"run".as_slice(), port.token().as_bytes()))
        }
        a => panic!("its first CONNECT carried no password: {a:?}; git: {said}"),
    }
}

/// A run's port closes with it (§macOS flow 4): once the run is reaped (`after_run`), a connect to its port is
/// refused, not left waiting in a listener's queue — what it left running loses its way out, closed. While it
/// runs, the port is the proxy's (a request without the token: 407). Through the real `spawn`, of a command
/// that ends at once (`/usr/bin/true` — no sandbox needed for the port's life).
#[test]
fn a_runs_port_closes_with_it() {
    let p = open(Path::new("/Users/u/p"));
    let port = p.proxy.as_ref().unwrap().port;
    let ended = after_run(Some(&p));
    let s = spawn(Some(&p), &Command::new("/usr/bin/true")).unwrap();
    let ask = || -> std::io::Result<String> {
        let mut c = TcpStream::connect((Ipv4Addr::LOCALHOST, port))?;
        c.write_all(b"GET http://a-5d.example.invalid/ HTTP/1.1\r\nHost: a-5d.example.invalid\r\n\r\n")?;
        Ok(String::from_utf8_lossy(&head(&c)).into_owned())
    };
    let served = ask().unwrap();
    assert!(served.starts_with("HTTP/1.1 407") && served.contains("Proxy-Authenticate: Basic"), "{served}");
    crate::spawn::wait(s.pid).unwrap();
    ended();
    // (`RunEnded` reaches the proxy's dispatcher on its own thread: waited for, at most 5 s)
    let by = Instant::now() + Duration::from_secs(5);
    let mut last = None;
    while Instant::now() < by {
        match TcpStream::connect((Ipv4Addr::LOCALHOST, port)) {
            Err(e) if e.kind() == std::io::ErrorKind::ConnectionRefused => return,
            r => last = Some(r.map(|_| "connected")),
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the run's port still takes connections after its end: {last:?} (the policy still held: {p:?})");
}

/// Two runs with the network open (§macOS flow 2, Q1): each reaches its own port to tarae's proxy; another
/// run's, and an ordinary port of this machine's, are denied — Seatbelt keeps another session's run off a
/// run's port, which its token alone couldn't (M3: any process reads a run's environment). A JVM's default
/// socket reaches its own too: the proxy's address is the IPv4 literal (M3d: Seatbelt's `localhost` doesn't
/// match `::ffff:127.0.0.1`). Harmless: the listeners are the test's own, on 127.0.0.1; `nc -z` and an empty
/// connect knock. The JVM gets the packs' variables as a run's does (below macOS 27 its IPv4 stack: the Java pack's
/// `JAVA_TOOL_OPTIONS`); its JDK is `JAVA_HOME`'s, as Gradle's and Maven's scripts take it, else macOS's own pick
/// (`/usr/libexec/java_home`: a CI's tool cache JDK is only the first). Without either it skips, said.
#[test]
fn a_run_reaches_only_its_own_proxy_port() {
    if skipped_inside("sandbox::proxy::macos_tests::a_run_reaches_only_its_own_proxy_port") {
        return;
    }
    let root = scratch("sb-5d-ports");
    let (a, b) = (open(&root), open(&root));
    let listener = |p: &Policy| {
        let l = TcpListener::from(p.proxy.as_ref().unwrap().take().unwrap());
        l.set_nonblocking(true).unwrap();
        l
    };
    let (la, lb) = (listener(&a), listener(&b));
    let ordinary = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    ordinary.set_nonblocking(true).unwrap();
    let port = |l: &TcpListener| l.local_addr().unwrap().port();
    let reached = |p: &Policy, l: &TcpListener, how: &str| {
        let script = format!("{how} {}", port(l));
        let mut c = command(Some(p), "/bin/sh", &["-c".into(), script]);
        c.current_dir(&root);
        finish(crate::spawn::setsid(&c).unwrap(), 30);
        l.accept().is_ok()
    };
    let nc = "/usr/bin/nc -z -w 2 127.0.0.1";
    assert!(reached(&a, &la, nc), "a run: its own port");
    assert!(reached(&b, &lb, nc), "the other run: its own port");
    assert!(!reached(&a, &lb, nc), "a run: the other run's port");
    assert!(!reached(&b, &la, nc), "the other run: this one's port");
    assert!(!reached(&a, &ordinary, nc), "a run: an ordinary port of this machine's");
    let given =
        std::env::var_os("JAVA_HOME").map(std::path::PathBuf::from).filter(|h| h.join("bin/java").is_file());
    let picked = || {
        let o = Command::new("/usr/libexec/java_home").output().ok().filter(|o| o.status.success())?;
        Some(std::path::PathBuf::from(String::from_utf8_lossy(&o.stdout).trim()))
    };
    match given.or_else(picked) {
        Some(home) => {
            let src = root.join("Knock.java");
            let knock = "public class Knock { public static void main(String[] a) throws Exception { \
                         try (var s = new java.net.Socket()) { s.connect(new java.net.InetSocketAddress(a[0], \
                         Integer.parseInt(a[1])), 2000); } } }";
            std::fs::write(&src, knock).unwrap();
            let java = format!("'{}/bin/java' '{}' 127.0.0.1", home.display(), src.display());
            // (the packs' variables, as `for_its_run` gives a run them)
            let here = Here::now();
            let vars = here.path("HOME").map(|h| crate::sandbox::vars_in(&h, &here));
            let env = vars.map(|v| crate::sandbox::packs_env(&v, &here, a.asks_hosts)).unwrap_or_default();
            let jvm = Policy { env, ..a.clone() };
            assert!(reached(&jvm, &la, &java), "a JVM's default socket: its own port");
            assert!(!reached(&jvm, &lb, &java), "a JVM's default socket: the other run's port");
        }
        None => eprintln!(
            "{}sandbox::proxy::macos_tests::a_run_reaches_only_its_own_proxy_port — its JVM part: no JDK (no \
             JAVA_HOME, none /usr/libexec/java_home knows)",
            crate::test_results::SELF_SKIP
        ),
    }
    std::fs::remove_dir_all(&root).ok();
}

/// A run's port is served with its policy's rules (§macOS flow 4, `RunPort::started`): any host — the address
/// checks still hold, this machine's own addresses beyond loopback refused, loopback left to `this_machine` —
/// so a request through it reaches the outside (a stand-in: `TEST_NAMES`, `TEST_BEYOND`), rebuilt as the
/// proxy rebuilds it (origin form, no `Proxy-*` header — the token never reaches the outside). Through the real
/// `spawn` and `after_run`, of a curl given the run's proxy variables — no sandbox needed for the proxy's side
/// (the Seatbelt half is `a_run_goes_out_only_through_its_proxy`).
#[test]
fn a_runs_port_takes_it_to_the_outside() {
    let root = Path::new("/Users/u/p-5d-rules");
    let name = "rules-5d.example.invalid";
    let (port, seen) = outside(name, root, 15);
    let p = open(root);
    let mut curl = Command::new("/usr/bin/curl");
    curl.args(["-s", "--max-time", "10", &format!("http://{name}:{port}/through")]);
    curl.envs(
        env_of(&command(Some(&p), "true", &[]))
            .into_iter()
            .filter(|(k, _)| k.to_lowercase().ends_with("_proxy")),
    );
    let ended = after_run(Some(&p));
    let (_, out, _) = finish(spawn(Some(&p), &curl).unwrap(), 30);
    ended();
    let (h, _) = seen.join().unwrap();
    assert_eq!(out, "ok", "through the proxy");
    let h = h.expect("the outside reached");
    assert!(h.starts_with("GET /through HTTP/1.1\r\n"), "as the proxy rebuilds it: {h:?}");
    assert!(h.contains(&format!("Host: {name}:{port}\r\n")), "{h:?}");
    assert!(!h.to_ascii_lowercase().contains("proxy-"), "no proxy header, no token outside: {h:?}");
}

/// Piece C: the network closed (`llm.sandbox-network` off), a run still goes out through its own port to tarae's
/// proxy — with an empty list, `llm.sandbox-hosts` aside — so a host waits on the main loop's answer: `y` lets this
/// run's request through alone; the next run is asked again, and a no refuses it. Through the real `spawn` (the
/// Seatbelt profile the closed policy makes), with a curl given the run's proxy variables.
#[test]
fn with_the_network_closed_a_host_waits_and_yes_lets_it_through() {
    use super::{FromProxy, Scope, ToProxy};
    if skipped_inside(
        "sandbox::proxy::macos_tests::with_the_network_closed_a_host_waits_and_yes_lets_it_through",
    ) {
        return;
    }
    let root = Path::new("/Users/u/p-c1-ask");
    let name = "ask-c1.example.invalid";
    let (port, seen) = outside(name, root, 30);
    let (asks, asked) = std::sync::mpsc::channel();
    let session = super::session_of(root);
    super::TEST_ASKS.lock().unwrap().push((session, asks));
    let closed = || {
        let listed = Some(vec![super::egress::Pattern::parse(name).unwrap()]);
        let env = crate::sandbox::Env { asks_hosts: true, hosts: listed, ..env(root) };
        let p = Policy::for_project(&env);
        assert!(p.asks_hosts && p.egress.hosts.is_empty(), "the list is the open network's");
        // (the run's port as a closed policy computed on macOS gets it — none, and its runs reach no proxy)
        let here = crate::sandbox::Here::now();
        let computed = crate::sandbox::policy_in(root, &crate::llm::LlmConfig::default(), &[], &here);
        assert!(computed.asks_hosts, "closed by default, each host asked");
        Policy { proxy: computed.proxy, run_id: computed.run_id, ..p }
    };
    let curl = |p: &Policy, path: &str| {
        let mut c = Command::new("/usr/bin/curl");
        c.args(["-s", "--max-time", "10", &format!("http://{name}:{port}/{path}")]);
        let set = env_of(&command(Some(p), "true", &[]));
        c.envs(set.into_iter().filter(|(k, _)| k.to_lowercase().ends_with("_proxy")));
        c
    };
    let answer = |run, scope| ToProxy::Answer { session, run, host: name.into(), port, scope };
    let p = closed();
    let ended = after_run(Some(&p));
    let s = spawn(Some(&p), &curl(&p, "first")).unwrap();
    let FromProxy::Ask { run, host, .. } = asked.recv_timeout(Duration::from_secs(10)).expect("asked");
    assert_eq!((run, host.as_str()), (p.run_id, name));
    super::get().send(answer(run, Scope::Run));
    let (_, out, _) = finish(s, 30);
    ended();
    assert_eq!(out, "ok", "`y`: the held request went on");
    assert!(seen.join().unwrap().0.is_some_and(|h| h.starts_with("GET /first ")));
    // The next run: `y` was the first one's alone — asked again; a no refuses it
    let p = closed();
    let ended = after_run(Some(&p));
    let s = spawn(Some(&p), &curl(&p, "second")).unwrap();
    let FromProxy::Ask { run, .. } = asked.recv_timeout(Duration::from_secs(10)).expect("asked again");
    assert_eq!(run, p.run_id);
    super::get().send(answer(run, Scope::Refused));
    let (_, out, _) = finish(s, 30);
    ended();
    assert_ne!(out, "ok", "refused");
}

/// A host a run's rules don't name (`llm.sandbox-hosts = []`) is held at the proxy, and the main loop is asked —
/// for that run, in its session, naming what asked; the session's answer lets the held request through, and the
/// session's next run reaches the host unasked though its own rules don't name it: the proxy keeps the session's
/// answers. Through the real `spawn` and `after_run` of a curl given the run's proxy variables (the Seatbelt half
/// of a run's way to its port is `a_run_goes_out_only_through_its_proxy`); the test stands in for the main loop
/// (`TEST_ASKS`).
#[test]
fn an_unknown_host_waits_for_the_answer() {
    use super::{FromProxy, Scope, ToProxy};
    let root = Path::new("/Users/u/p-5e-ask");
    let name = "ask-5e.example.invalid";
    let (port, seen) = outside(name, root, 30);
    let (asks, asked) = std::sync::mpsc::channel();
    let session = super::session_of(root);
    super::TEST_ASKS.lock().unwrap().push((session, asks));
    let listed = || {
        let env = crate::sandbox::Env { network: true, hosts: Some(Vec::new()), ..env(root) };
        let p = Policy::for_project(&env);
        Policy { proxy: Some(RunPort::new(p.run_id).unwrap()), ..p }
    };
    let curl = |p: &Policy, path: &str| {
        let mut c = Command::new("/usr/bin/curl");
        c.args(["-s", "--max-time", "10", &format!("http://{name}:{port}/{path}")]);
        let set = env_of(&command(Some(p), "true", &[]));
        c.envs(set.into_iter().filter(|(k, _)| k.to_lowercase().ends_with("_proxy")));
        c
    };
    let p = listed();
    let ended = after_run(Some(&p));
    let s = spawn(Some(&p), &curl(&p, "first")).unwrap();
    let ask = asked.recv_timeout(Duration::from_secs(10)).expect("the main loop asked");
    let FromProxy::Ask { run, host, port: at, process, .. } = ask;
    assert_eq!((run, host.as_str(), at, process.as_str()), (p.run_id, name, port, "curl"));
    super::get().send(ToProxy::Answer { session, run, host: name.into(), port, scope: Scope::Session });
    let (_, out, _) = finish(s, 30);
    ended();
    let (h, l) = seen.join().unwrap();
    assert_eq!(out, "ok", "the held request went on");
    assert!(h.is_some_and(|h| h.starts_with("GET /first ")));
    // The session's next run: its rules don't name it, the session's answer does
    let again = crate::event::thread("tarae-5e-test", move || {
        accept_within(&l, 15).map(|s| {
            let h = head(&s);
            let _ = (&s).write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            String::from_utf8_lossy(&h).into_owned()
        })
    });
    let p = listed();
    let ended = after_run(Some(&p));
    let (_, out, _) = finish(spawn(Some(&p), &curl(&p, "second")).unwrap(), 30);
    ended();
    assert_eq!(out, "ok", "the session's next run");
    assert!(again.join().unwrap().is_some_and(|h| h.starts_with("GET /second ")));
    assert!(asked.try_recv().is_err(), "not asked again");
}

/// A run whose policy couldn't read this machine's own addresses reaches no address through its port: the proxy
/// can't tell them from the network's, so it refuses every one (`Egress::refuses`), and the run's report says
/// nothing goes beyond this machine, and why (`Policy::loopback_only`). Through the real `spawn`, as
/// `a_runs_port_takes_it_to_the_outside` — the same stand-in outside, never reached here.
#[test]
fn a_run_whose_own_addresses_are_unread_reaches_nothing() {
    let root = Path::new("/Users/u/p-5d-unread");
    let name = "unread-5d.example.invalid";
    // (asked once curl has its answer: a connection to the outside would have come before it)
    let (port, seen) = outside(name, root, 1);
    let base = open(root);
    let mut r = RunPort::new(base.run_id).unwrap();
    std::sync::Arc::get_mut(&mut r).expect("a new run's port, ours alone").own_addresses = None;
    let p = Policy { proxy: Some(r), ..base };
    let report = Guard::In(Box::new(p.clone())).report(&[]);
    let report = report.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        report.contains(
            "connect to nothing but this machine's loopback, none of its ports (llm.sandbox-ports opens some, \
             llm.sandbox-localhost all) — tarae couldn't read this machine's own addresses, so its proxy can't tell \
             them from the network's and reaches no address, llm.sandbox-network notwithstanding"
        ),
        "{report}"
    );
    let mut curl = Command::new("/usr/bin/curl");
    curl.args(["-s", "--max-time", "10", &format!("http://{name}:{port}/through")]);
    curl.envs(
        env_of(&command(Some(&p), "true", &[]))
            .into_iter()
            .filter(|(k, _)| k.to_lowercase().ends_with("_proxy")),
    );
    let ended = after_run(Some(&p));
    let (_, out, _) = finish(spawn(Some(&p), &curl).unwrap(), 30);
    ended();
    assert!(
        out.contains("this machine's own addresses couldn't be read, so no address is reached"),
        "refused, saying why: {out:?}"
    );
    let (h, _) = seen.join().unwrap();
    assert!(h.is_none(), "the outside was reached: {h:?}");
}

/// Through the real path (`policy` → `command` → `spawn`), under Seatbelt, a run's plain `curl` goes out
/// through tarae's proxy alone: its proxy variables lead it to its own port, and the outside (the test's
/// stand-in, as in `a_runs_port_takes_it_to_the_outside`) gets the request. The same listener reached directly
/// (`--noproxy '*'`) is denied by Seatbelt: nothing but the proxy's port goes out.
#[test]
fn a_run_goes_out_only_through_its_proxy() {
    if skipped_inside("sandbox::proxy::macos_tests::a_run_goes_out_only_through_its_proxy") {
        return;
    }
    let root = scratch("sb-5d-out");
    let name = "a-5d.example.invalid";
    let (port, seen) = outside(name, &root, 15);
    let p = open(&root);
    let script = format!(
        "/usr/bin/curl -s --max-time 10 http://{name}:{port}/through; echo; \
         /usr/bin/curl -s --noproxy '*' --max-time 3 http://127.0.0.1:{port}/direct; echo \"direct=$?\""
    );
    let out = run(&p, &script);
    let (h, outside) = seen.join().unwrap();
    std::fs::remove_dir_all(&root).ok();
    assert!(out.starts_with("ok\n"), "through the proxy: {out:?}");
    assert!(h.is_some_and(|h| h.starts_with("GET /through HTTP/1.1\r\n")), "the outside reached through it");
    assert!(out.contains("direct=7"), "directly: curl couldn't connect — {out:?}");
    assert!(outside.accept().is_err(), "the direct connect reached the outside");
}

/// A run that names its own proxy password, a wrong one (its own `-x`, to its run's port): 407, and the report
/// says so — `network-proxy`, the host it was for. A request without credentials: 407 too, unsaid (§Decided:
/// git asks only after one — every git run's first request would be noise). Through the real path; the
/// report's line comes from the proxy over the run's own sender (`told`), into its watch.
#[test]
fn a_wrong_token_is_said_in_the_runs_report() {
    if skipped_inside("sandbox::proxy::macos_tests::a_wrong_token_is_said_in_the_runs_report") {
        return;
    }
    let root = scratch("sb-5d-token");
    let p = Policy { watch: Some(Watch::quiet("t5d.")), ..open(&root) };
    let script = "hp=${http_proxy##*@}; \
                  /usr/bin/curl -s -o /dev/null -w '%{http_code} ' -x \"http://run:tarae-fake-token-5d@$hp\" \
                  http://wrong-5d.example.invalid/; \
                  /usr/bin/curl -s -o /dev/null -w '%{http_code}' -x \"http://$hp\" http://none-5d.example.invalid/";
    let out = run(&p, script);
    std::fs::remove_dir_all(&root).ok();
    assert_eq!(out, "407 407");
    // (the proxy's line comes over its sender to a thread of the run's, then its watch: waited for, ≤ 5 s)
    let said = "reaching wrong-5d.example.invalid:80: a wrong proxy password — not its run's (HTTPS_PROXY) \
                (network-proxy, sh)";
    let g = Guard::In(Box::new(p));
    let by = Instant::now() + Duration::from_secs(5);
    while !g.report(&[]).contains(said) && Instant::now() < by {
        std::thread::sleep(Duration::from_millis(20));
    }
    let r = g.report(&[]);
    assert!(r.contains(said), "{r}");
    assert!(!r.contains("none-5d"), "without credentials: unsaid — {r}");
}
