//! Step 5d's measurements (`target/sb/step5d-plan.md` §Measurements), `#[ignore]`d — run by hand outside a
//! sandbox (`target/sb/step5/d-outside.sh`), each writing its table to `target/sb/step5/d-*.txt`:
//! - Q4: what breaks when a run can't reach the system's resolver (mDNSResponder's socket — denied today by
//!   the unix-socket rule), each case under the run's real profile as it is and with that socket allowed;
//! - Q3b: whether the proxy can tell a connection to a run's port is the run's own — the peer's socket found
//!   among the run's processes (`proc_pidfdinfo`) — and what that costs.
//!
//! Every run's home is a folder of the measurement's own (no one's settings or credentials read); toolchains'
//! caches are in it too. Q4's fetches reach the real public registries through tarae's proxy only with
//! `TARAE_MEASURE_REACHES_NETWORK` set (decided 2026-10-07, as 5c's M4); without it they're left out, said.

use std::ffi::OsString;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::macos_tests::{accept_within, finish, head, open};
use crate::sandbox::tests::scratch;
use crate::sandbox::{Guard, Policy, RunDir, WATCH_LAST, after_run, command, spawn, watch_for, watch_wait};

/// Where a table goes.
fn out_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("target/sb/step5").join(name)
}

/// What every measured run gets on top: `PATH` (the run's environment here holds nothing of tarae's), a home of
/// its own, the toolchains' caches in the folder, rustup's toolchains read where they are (no credentials
/// there), no git or npm or pip settings of anyone's.
fn base_env(root: &Path) -> Vec<(String, String)> {
    let real_home = std::env::var("HOME").unwrap_or_default();
    let at = |rel: &str| root.join(rel).display().to_string();
    for d in ["home", "cargo-home", "npm", "pip", "gopath", "gocache", "target"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    [
        ("PATH", std::env::var("PATH").unwrap_or_default()),
        ("HOME", at("home")),
        ("RUSTUP_HOME", format!("{real_home}/.rustup")),
        ("CARGO_HOME", at("cargo-home")),
        ("CARGO_TARGET_DIR", at("target")),
        ("CARGO_NET_RETRY", "0".into()),
        ("GIT_CONFIG_GLOBAL", "/dev/null".into()),
        ("GIT_CONFIG_NOSYSTEM", "1".into()),
        ("GIT_TERMINAL_PROMPT", "0".into()),
        ("npm_config_cache", at("npm")),
        ("npm_config_userconfig", "/dev/null".into()),
        ("PIP_CONFIG_FILE", "/dev/null".into()),
        ("PIP_CACHE_DIR", at("pip")),
        ("GOPATH", at("gopath")),
        ("GOCACHE", at("gocache")),
        ("GOENV", "off".into()),
        ("GOFLAGS", "-modcacherw".into()),
        ("GOTOOLCHAIN", "local".into()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

/// A JDK's `java`, if there's one.
fn java() -> Option<String> {
    let o = Command::new("/usr/libexec/java_home").output().ok().filter(|o| o.status.success())?;
    Some(format!("{}/bin/java", String::from_utf8_lossy(&o.stdout).trim()))
}

/// `go build` of `src` into `root/name`, unsandboxed (stdlib only, nothing fetched), its home the folder's.
fn go_binary(root: &Path, name: &str, src: &str) -> Option<PathBuf> {
    let file = root.join(format!("{name}.go"));
    std::fs::write(&file, src).unwrap();
    let out = root.join(name);
    let mut go = Command::new("go");
    go.args(["build", "-o"]).arg(&out).arg(&file).envs(base_env(root)).current_dir(root);
    let (status, ..) = finish(crate::spawn::setsid(&go).ok()?, 300);
    status.is_some_and(|s| s.success()).then_some(out)
}

/// A policy for a measured run: the network open with a port of its own to the proxy, a run folder and tag,
/// its watch up (macOS's denials come from `log stream`).
fn measured(root: &Path) -> Policy {
    let run = RunDir::new().ok();
    let tag = run.as_ref().map(|r| r.tag());
    let mut p = Policy { run, tag, ..open(root) };
    p.watch = watch_for(&p);
    std::thread::sleep(watch_wait(&p));
    p
}

/// The command `command` makes for `script` in `p`, its profile with `extra` rules at its end (later rules win).
fn with_rules(p: &Policy, script: &str, extra: &str, env: &[(String, String)]) -> Command {
    let c = command(Some(p), "/bin/sh", &["-c".into(), script.into()]);
    let mut args: Vec<OsString> = c.get_args().map(OsString::from).collect();
    if !extra.is_empty() {
        let mut profile = args[1].clone();
        profile.push(extra);
        args[1] = profile;
    }
    let mut cmd = Command::new(c.get_program());
    cmd.args(&args);
    for (k, v) in c.get_envs() {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    cmd.envs(env.iter().map(|(k, v)| (k, v))).current_dir(&p.root);
    cmd
}

/// A measured run's end: its exit code, time, both streams.
struct Ended {
    code: Option<i32>,
    ms: u128,
    out: String,
    err: String,
}

/// `cmd` in `p` through `spawn`, its wait (within 300 s — a fetch on a slow network) and `after_run`; `started`
/// gets its pid (its session's id).
fn run_measured(p: &Policy, cmd: &Command, started: impl FnOnce(u32)) -> Ended {
    let ended = after_run(Some(p));
    let t = Instant::now();
    let s = spawn(Some(p), cmd).unwrap();
    if let Some(w) = &p.watch {
        w.run_started(s.pid);
    }
    started(s.pid);
    let (status, out, err) = finish(s, 300);
    let ms = t.elapsed().as_millis();
    ended();
    Ended { code: status.and_then(|s| s.code()), ms, out, err }
}

/// A line's worth of `s`: its last non-empty line, cut.
fn last_line(s: &str) -> String {
    let l = s.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    l.chars().take(90).collect()
}

const MDNS_SOCKET: &str = "/private/var/run/mDNSResponder";

/// A measured case: its name, its script, what it gets on top of `base_env`.
type Case = (String, String, Vec<(String, String)>);

fn case(name: &str, script: &str, env: &[(&str, &str)]) -> Case {
    (name.into(), script.into(), env.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect())
}

/// Q4: each case under the run's profile as it is (mDNSResponder's socket denied) and with it allowed — the
/// outcome, the time, and whether the run's report names a lookup refused. Lookups first (this machine only),
/// then fetches through the proxy (the network, only with `TARAE_MEASURE_REACHES_NETWORK`).
#[test]
#[ignore]
fn measure_mdns_under_the_proxy() {
    let root = scratch("sb-5d-q4");
    let env = base_env(&root);
    let net = std::env::var_os("TARAE_MEASURE_REACHES_NETWORK").is_some();
    // Fixtures: a crate whose test looks `localhost` up and binds it, one with a dependency, a Go module
    std::fs::create_dir_all(root.join("lk/src")).unwrap();
    std::fs::write(
        root.join("lk/Cargo.toml"),
        "[package]\nname = \"lk\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("lk/src/lib.rs"),
        "#[test] fn looks_up() { use std::net::ToSocketAddrs; assert!(\"localhost:0\".to_socket_addrs().unwrap().count() > 0); std::net::TcpListener::bind(\"localhost:0\").unwrap(); }\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("dep/src")).unwrap();
    std::fs::write(root.join("dep/Cargo.toml"), "[package]\nname = \"dep\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n[dependencies]\nitoa = \"1\"\n").unwrap();
    std::fs::write(root.join("dep/src/lib.rs"), "").unwrap();
    std::fs::create_dir_all(root.join("gm")).unwrap();
    std::fs::write(root.join("gm/go.mod"), "module m\n\ngo 1.21\n").unwrap();
    let lookup = go_binary(
        &root,
        "lookup",
        "package main\nimport (\"fmt\"; \"net\"; \"os\"; \"time\")\nfunc main() { t := time.Now(); h, _ := os.Hostname(); \
         for _, n := range []string{\"localhost\", h} { a, err := net.LookupHost(n); fmt.Println(n, a, err) }; \
         fmt.Println(time.Since(t).Milliseconds(), \"ms\") }\n",
    );
    let jv = java();
    if jv.is_some() {
        std::fs::write(
            root.join("Local.java"),
            "public class Local { public static void main(String[] a) throws Exception { long t = System.nanoTime(); \
             System.out.println(java.net.InetAddress.getLocalHost()); System.out.println((System.nanoTime() - t) / 1000000 + \" ms\"); } }",
        )
        .unwrap();
    }
    let py_lookups = "python3 -c \"import socket,time; t=time.time(); print(socket.getaddrinfo('localhost',80)[0][4]); \
                      print(socket.gethostbyname(socket.gethostname())); print(socket.getfqdn()); \
                      print(round((time.time()-t)*1000),'ms')\"";
    let node_lookups = "node -e \"const d=require('dns');d.lookup('localhost',(e,a)=>{console.log(e?e.code:a);\
                        d.lookup(require('os').hostname(),(e2,b)=>console.log(e2?e2.code:b))})\"";
    // (each with whether it needs the network)
    let mut cases: Vec<(Case, bool)> = vec![
        (case("python: localhost, hostname, fqdn", py_lookups, &[]), false),
        (case("node: dns.lookup localhost, hostname", node_lookups, &[]), false),
        (
            case(
                "curl: localhost by name (its proxy port)",
                "curl -s -o /dev/null -w '%{http_code}' --noproxy '*' \"http://localhost:${http_proxy##*:}/\"; echo",
                &[],
            ),
            false,
        ),
        (
            case(
                "cargo test: localhost:0 looked up and bound",
                "cd lk && cargo test --offline -q 2>&1 | tail -3",
                &[],
            ),
            false,
        ),
    ];
    if let Some(l) = &lookup {
        cases.push((case("go: LookupHost localhost, hostname", &format!("'{}'", l.display()), &[]), false));
    }
    if let Some(j) = &jv {
        cases.push((case("java: InetAddress.getLocalHost()", &format!("'{j}' Local.java"), &[]), false));
    }
    let node_fetch = "node -e \"fetch('https://example.com/').then(r=>console.log(r.status)).catch(e=>console.log('ERR',(e.cause&&e.cause.code)||e.message))\"";
    let fetches = [
        case(
            "curl https://example.com/",
            "curl -sS -o /dev/null -w '%{http_code}' https://example.com/; echo",
            &[],
        ),
        case(
            "git ls-remote github.com/git/git",
            "git ls-remote https://github.com/git/git HEAD | cut -c1-12",
            &[],
        ),
        case("cargo fetch (itoa, fresh CARGO_HOME)", "cd dep && cargo fetch 2>&1 | tail -2", &[]),
        case("npm view left-pad version", "npm view left-pad version 2>&1 | tail -2", &[]),
        case("pip download six", "python3 -m pip download six --no-deps -q -d dl 2>&1 | tail -2; ls dl", &[]),
        case(
            "go mod download golang.org/x/text",
            "cd gm && go mod download -x golang.org/x/text@v0.14.0 2>&1 | tail -1",
            &[],
        ),
        case(
            "python urllib https://example.com/",
            "python3 -c \"import urllib.request as u; print(u.urlopen('https://example.com/',timeout=20).status)\" 2>&1 | tail -1",
            &[],
        ),
        case("node fetch, no NODE_USE_ENV_PROXY", node_fetch, &[]),
        case("node fetch, NODE_USE_ENV_PROXY=1", node_fetch, &[("NODE_USE_ENV_PROXY", "1")]),
    ];
    cases.extend(fetches.into_iter().map(|c| (c, true)));
    let mdns_allowed =
        format!("(allow network-outbound (require-all (remote unix-socket) (literal \"{MDNS_SOCKET}\")))\n");
    let mut table = String::from(
        "# Q4: mDNSResponder denied (as it is) vs allowed, under the proxy (macOS, step 5d)\n\n\
         | Case | denied: exit · ms · lookup said · last line | allowed: exit · ms · lookup said · last line |\n|---|---|---|\n",
    );
    for ((name, script, extra_env), needs_net) in cases {
        if needs_net && !net {
            table.push_str(&format!("| {name} | — (no TARAE_MEASURE_REACHES_NETWORK) | — |\n"));
            continue;
        }
        let mut cells = Vec::new();
        for extra in ["", mdns_allowed.as_str()] {
            let p = measured(&root);
            let mut all = env.clone();
            all.extend(extra_env.iter().cloned());
            let cmd = with_rules(&p, &script, extra, &all);
            let e = run_measured(&p, &cmd, |_| {});
            std::thread::sleep(WATCH_LAST + Duration::from_millis(500));
            let report = Guard::In(Box::new(p)).report(&[]);
            let said = report.contains(MDNS_SOCKET);
            let shown = match last_line(&e.out) {
                l if l.is_empty() => last_line(&e.err),
                l => l,
            };
            cells.push(format!(
                "{} · {} · {} · `{}`",
                e.code.map_or("signal".into(), |c| c.to_string()),
                e.ms,
                if said { "yes" } else { "no" },
                shown.replace('|', "/")
            ));
        }
        let row = format!("| {name} | {} | {} |\n", cells[0], cells[1]);
        print!("{row}");
        table.push_str(&row);
    }
    std::fs::write(out_file("d-q4.txt"), &table).unwrap();
    std::fs::remove_dir_all(&root).ok();
}

// ── Q4 again: what a denied mDNSResponder costs a `localhost` lookup, fairly ─────────────────────────────

/// The rule that lets a run reach mDNSResponder's socket (the "allowed" column).
fn mdns_allowed() -> String {
    format!("(allow network-outbound (require-all (remote unix-socket) (literal \"{MDNS_SOCKET}\")))\n")
}

/// The lookup time a case printed of itself (`lookup_ms=…`, or curl's `lookup_s=…`), in ms.
fn lookup_ms(out: &str) -> Option<f64> {
    out.lines().find_map(|l| match l.trim().split_once('=') {
        Some(("lookup_ms", v)) => v.trim().parse().ok(),
        Some(("lookup_s", v)) => v.trim().parse::<f64>().ok().map(|s| s * 1000.0),
        _ => None,
    })
}

/// Median and p90 of `v` (sorted in place); None empty.
fn median_p90(v: &mut [f64]) -> Option<(f64, f64)> {
    v.sort_by(f64::total_cmp);
    let at = |q: f64| v[((v.len() as f64 - 1.0) * q).round() as usize];
    (!v.is_empty()).then(|| (at(0.5), at(0.9)))
}

/// A run's time and the lookup time it printed, in `p` with `extra` rules.
fn timed(p: &Policy, script: &str, extra: &str, env: &[(String, String)]) -> (f64, Option<f64>, String) {
    let e = run_measured(p, &with_rules(p, script, extra, env), |_| {});
    let shown = match last_line(&e.out) {
        l if l.is_empty() => last_line(&e.err),
        l => l,
    };
    (e.ms as f64, lookup_ms(&e.out).or_else(|| lookup_ms(&e.err)), shown)
}

/// The fixtures the lookup cases run: a Go binary (one lookup; `n` of them in a row), Java sources, a crate whose
/// test looks `localhost` up and says how long it took.
fn lookup_fixtures(root: &Path) -> (Option<PathBuf>, Option<PathBuf>, Option<String>) {
    let one = go_binary(
        root,
        "lookup1",
        "package main\nimport (\"fmt\"; \"net\"; \"time\")\nfunc main() { t := time.Now(); _, err := net.LookupHost(\"localhost\"); \
         fmt.Printf(\"lookup_ms=%.3f\\n\", float64(time.Since(t).Microseconds())/1000); if err != nil { fmt.Println(err) } }\n",
    );
    let many = go_binary(
        root,
        "lookupn",
        "package main\nimport (\"fmt\"; \"net\"; \"os\"; \"strconv\"; \"time\")\nfunc main() { n, _ := strconv.Atoi(os.Args[1]); \
         for i := 0; i < n; i++ { t := time.Now(); net.LookupHost(\"localhost\"); \
         fmt.Printf(\"%d: %.3f ms\\n\", i+1, float64(time.Since(t).Microseconds())/1000) } }\n",
    );
    std::fs::write(
        root.join("ByName.java"),
        "public class ByName { public static void main(String[] a) throws Exception { long t = System.nanoTime(); \
         java.net.InetAddress.getByName(\"localhost\"); System.out.println(\"lookup_ms=\" + (System.nanoTime() - t) / 1e6); } }",
    )
    .unwrap();
    // (what Gradle's daemon asks at its start: the host's name and address, its canonical name, the interfaces)
    std::fs::write(
        root.join("GradleLike.java"),
        "public class GradleLike { public static void main(String[] a) throws Exception { long t = System.nanoTime(); \
         var h = java.net.InetAddress.getLocalHost(); h.getCanonicalHostName(); \
         var e = java.net.NetworkInterface.getNetworkInterfaces(); while (e.hasMoreElements()) e.nextElement().getInetAddresses(); \
         System.out.println(\"lookup_ms=\" + (System.nanoTime() - t) / 1e6); } }",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("lt/src")).unwrap();
    std::fs::write(
        root.join("lt/Cargo.toml"),
        "[package]\nname = \"lt\"\nversion = \"0.1.0\"\nedition = \"2021\"\n[workspace]\n",
    )
    .unwrap();
    std::fs::write(
        root.join("lt/src/lib.rs"),
        "#[test] fn looks_up() { use std::net::ToSocketAddrs; let t = std::time::Instant::now(); \
         \"localhost:0\".to_socket_addrs().unwrap().count(); eprintln!(\"lookup_ms={:.3}\", t.elapsed().as_secs_f64() * 1000.0); \
         std::net::TcpListener::bind(\"localhost:0\").unwrap(); }\n",
    )
    .unwrap();
    (one, many, java())
}

/// Q4 again, fairly (the user, 2026-10-07: "denied looks too slow"): each `localhost` lookup case 7 times,
/// denied and allowed alternating (ABAB…), after one unmeasured run of each in each column (warm caches), each
/// run's wall time and the lookup time it printed of itself, medians and p90 per column. Then the options that keep
/// DNS closed, measured the same way: Go's own resolver, the JVM's hosts-file resolver. This machine only.
#[test]
#[ignore]
fn measure_localhost_lookups() {
    let root = scratch("sb-5d-q4b");
    let env = base_env(&root);
    let (one, _, jv) = lookup_fixtures(&root);
    // (curl's: a listener of the test's on an opened port, answering 204)
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = l.local_addr().unwrap().port();
    crate::event::thread("tarae-5d-test", move || {
        while let Some(s) = accept_within(&l, 120) {
            let _ = head(&s);
            let _ = (&s).write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n");
        }
    });
    let mut cases: Vec<Case> = vec![
        case(
            "python getaddrinfo('localhost')",
            "python3 -c \"import socket,time;t=time.perf_counter();socket.getaddrinfo('localhost',80);print('lookup_ms=%.3f'%((time.perf_counter()-t)*1000))\"",
            &[],
        ),
        case(
            "node dns.lookup('localhost')",
            "node -e \"const t=process.hrtime.bigint();require('dns').lookup('localhost',()=>console.log('lookup_ms='+Number(process.hrtime.bigint()-t)/1e6))\"",
            &[],
        ),
        case(
            "curl http://localhost:<opened port>/",
            &format!(
                "curl -s -o /dev/null --noproxy '*' -w 'lookup_s=%{{time_namelookup}}\\n' http://localhost:{port}/"
            ),
            &[],
        ),
        case(
            "cargo test: localhost:0 looked up and bound",
            "cd lt && cargo test --offline -q -- --nocapture 2>&1 | grep lookup_ms",
            &[],
        ),
    ];
    if let Some(g) = &one {
        cases.push(case("go LookupHost(\"localhost\")", &format!("'{}'", g.display()), &[]));
        cases.push(case(
            "  option: go, its own resolver (GODEBUG=netdns=go)",
            &format!("'{}'", g.display()),
            &[("GODEBUG", "netdns=go")],
        ));
    }
    if let Some(j) = &jv {
        let hosts = "-Djdk.net.hosts.file=/etc/hosts";
        cases.push(case("java getByName(\"localhost\")", &format!("'{j}' ByName.java"), &[]));
        cases.push(case(
            "java Gradle-like: getLocalHost, canonical name, interfaces",
            &format!("'{j}' GradleLike.java"),
            &[],
        ));
        cases.push(case(
            "  option: java getByName, the hosts file only (jdk.net.hosts.file)",
            &format!("'{j}' {hosts} ByName.java"),
            &[],
        ));
        cases.push(case(
            "  option: java Gradle-like, the hosts file only (jdk.net.hosts.file)",
            &format!("'{j}' {hosts} GradleLike.java"),
            &[],
        ));
    }
    let allowed = mdns_allowed();
    let mut table = String::from(
        "# Q4 again: a `localhost` lookup, mDNSResponder denied vs allowed (macOS, step 5d)\n\n\
         7 runs each, ABAB, warm (one unmeasured run per column first). ms: median / p90.\n\n\
         | Case | denied: lookup | allowed: lookup | denied: run | allowed: run | last line (denied) |\n|---|---|---|---|---|---|\n",
    );
    for (name, script, extra_env) in cases {
        let mut all = env.clone();
        all.extend(extra_env);
        let policy = || Policy { ports: vec![port], ..open(&root) };
        for extra in ["", allowed.as_str()] {
            let _ = timed(&policy(), &script, extra, &all);
        }
        let (mut look, mut wall) = ([Vec::new(), Vec::new()], [Vec::new(), Vec::new()]);
        let mut shown = String::new();
        for _ in 0..7 {
            for (i, extra) in ["", allowed.as_str()].into_iter().enumerate() {
                let (ms, lookup, last) = timed(&policy(), &script, extra, &all);
                wall[i].push(ms);
                look[i].extend(lookup);
                if i == 0 {
                    shown = last;
                }
            }
        }
        let cell = |v: &mut Vec<f64>| match median_p90(v) {
            Some((m, p)) => format!("{m:.1} / {p:.1}"),
            None => "—".into(),
        };
        let [ld, la] = &mut look;
        let [wd, wa] = &mut wall;
        let row = format!(
            "| {name} | {} | {} | {} | {} | `{}` |\n",
            cell(ld),
            cell(la),
            cell(wd),
            cell(wa),
            shown.replace('|', "/")
        );
        print!("{row}");
        table.push_str(&row);
    }
    std::fs::write(out_file("d-q4-lookups.txt"), &table).unwrap();
    std::fs::remove_dir_all(&root).ok();
}

/// Where a denied lookup's time goes: 5 lookups in one process, each timed (per lookup or once a process?),
/// denied and allowed; the run's denials by kind (is anything but the socket tried?); and `/usr/bin/sample` of a
/// process looking `localhost` up in a loop while denied — the stacks it waits in. This machine only.
#[test]
#[ignore]
fn measure_where_a_denied_lookup_waits() {
    let root = scratch("sb-5d-q4c");
    let env = base_env(&root);
    let (_, many, _) = lookup_fixtures(&root);
    let mut out = String::from("# Where a denied `localhost` lookup waits (macOS, step 5d)\n\n");
    let py5 = "python3 -c \"import socket,time\nfor i in range(5):\n  t=time.perf_counter(); socket.getaddrinfo('localhost',80); print('%d: %.3f ms'%(i+1,(time.perf_counter()-t)*1000))\"";
    let mut seqs = vec![("python, 5 lookups in one process", py5.to_string())];
    if let Some(g) = &many {
        seqs.push(("go, 5 lookups in one process", format!("'{}' 5", g.display())));
    }
    for (name, script) in &seqs {
        for (column, extra) in [("denied", String::new()), ("allowed", mdns_allowed())] {
            let p = measured(&root);
            let e = run_measured(&p, &with_rules(&p, script, &extra, &env), |_| {});
            std::thread::sleep(WATCH_LAST + Duration::from_millis(500));
            let report = Guard::In(Box::new(p)).report(&[]);
            let denials: Vec<&str> = report.lines().filter(|l| l.starts_with("  ")).collect();
            out.push_str(&format!(
                "## {name}, {column} ({} ms in all)\n\n```\n{}```\n\nIts report's denials: {}\n\n",
                e.ms,
                e.out,
                if denials.is_empty() { "none".to_string() } else { denials.join(" · ") }
            ));
        }
    }
    // The stacks: a looping lookup, denied, sampled for 2 s by the unsandboxed measurement
    let p = measured(&root);
    let looping = "python3 -c \"import socket,time\nt=time.time()\nwhile time.time()-t<6: socket.getaddrinfo('localhost',80)\"";
    let file = out_file("d-q4-sample.txt");
    let sample = |pid: u32| {
        let file = file.clone();
        crate::event::thread("tarae-5d-test", move || {
            std::thread::sleep(Duration::from_millis(1500));
            // (the run's `sh` exec'd python: the run's own pid is the process looking up)
            let mut s = Command::new("/usr/bin/sample");
            s.arg(pid.to_string()).arg("2").arg("-file").arg(&file);
            let _ = finish(crate::spawn::setsid(&s).unwrap(), 30);
        })
    };
    let mut sampler = None;
    let e = run_measured(&p, &with_rules(&p, looping, "", &env), |pid| sampler = Some(sample(pid)));
    if let Some(h) = sampler {
        let _ = h.join();
    }
    let lookups = std::fs::read_to_string(&file).unwrap_or_default();
    let heavy: Vec<&str> = lookups
        .lines()
        .filter(|l| {
            [
                "dnssd",
                "DNSService",
                "mdns",
                "si_",
                "getaddrinfo",
                "connect",
                "usleep",
                "nanosleep",
                "poll",
                "select",
                "kevent",
            ]
            .iter()
            .any(|k| l.contains(k))
        })
        .take(40)
        .collect();
    out.push_str(&format!(
        "## /usr/bin/sample of a denied lookup loop ({} ms run; whole sample: d-q4-sample.txt)\n\n```\n{}\n```\n",
        e.ms,
        heavy.join("\n")
    ));
    std::fs::write(out_file("d-q4-where.txt"), &out).unwrap();
    std::fs::remove_dir_all(&root).ok();
}

/// How a measured run is set up, as far as its denials go.
#[derive(Clone, Copy, PartialEq)]
enum Setup {
    /// A real run: a tag (every deny `(with message …)`) and a watch (`log stream` reading them).
    TagWatch,
    /// A tag, no watch.
    Tag,
    /// Neither.
    Neither,
}

/// A policy for `root` set up as `s` (the network open, a port of its own to the proxy).
fn set_up(root: &Path, s: Setup) -> Policy {
    let p = open(root);
    if s == Setup::Neither {
        return p;
    }
    let run = RunDir::new().ok();
    let tag = run.as_ref().map(|r| r.tag());
    let mut p = Policy { run, tag, ..p };
    if s == Setup::TagWatch {
        p.watch = watch_for(&p);
        std::thread::sleep(watch_wait(&p));
    }
    p
}

/// What a denial costs (the user, 2026-10-07): which part of a real run's denial — the tag's message, the
/// watch's `log stream` — makes a denied mDNSResponder slow some processes, and whether a deny that reports
/// nothing keeps the socket closed at no cost. Each case in 8 columns: tag + watch denied, allowed, a quiet deny
/// (no `with message`, after the tagged one), a `(with no-report)` deny; tag only denied, allowed; neither denied,
/// allowed. One unmeasured run per column first, then 7 rounds through every column, the round's first column
/// rotating. Per column: the run's wall time (median / p90), the lookup's own time (median), and — with a watch —
/// whether the run's report names the lookup (what a quiet deny takes from Claude). This machine only.
#[test]
#[ignore]
fn measure_what_a_denial_costs() {
    let root = scratch("sb-5d-q4d");
    let env = base_env(&root);
    let (one, _, jv) = lookup_fixtures(&root);
    let socket = format!("(require-all (remote unix-socket) (literal \"{MDNS_SOCKET}\"))");
    let (allowed, quiet) = (mdns_allowed(), format!("(deny network-outbound {socket})\n"));
    let no_report = format!("(deny network-outbound {socket} (with no-report))\n");
    let columns: [(&str, Setup, &str); 8] = [
        ("tag + watch, denied", Setup::TagWatch, ""),
        ("tag + watch, allowed", Setup::TagWatch, &allowed),
        ("tag + watch, quiet deny (no message)", Setup::TagWatch, &quiet),
        ("tag + watch, deny (with no-report)", Setup::TagWatch, &no_report),
        ("tag only, denied", Setup::Tag, ""),
        ("tag only, allowed", Setup::Tag, &allowed),
        ("neither, denied", Setup::Neither, ""),
        ("neither, allowed", Setup::Neither, &allowed),
    ];
    let mut cases = vec![
        case(
            "python getaddrinfo('localhost')",
            "python3 -c \"import socket,time;t=time.perf_counter();socket.getaddrinfo('localhost',80);print('lookup_ms=%.3f'%((time.perf_counter()-t)*1000))\"",
            &[],
        ),
        case(
            "cargo test: localhost:0 looked up and bound",
            "cd lt && cargo test --offline -q -- --nocapture 2>&1 | grep lookup_ms",
            &[],
        ),
    ];
    if let Some(g) = &one {
        cases.push(case("go LookupHost(\"localhost\")", &format!("'{}'", g.display()), &[]));
    }
    if let Some(j) = &jv {
        cases.push(case("java getByName(\"localhost\")", &format!("'{j}' ByName.java"), &[]));
    }
    // One run in a column: its wall time, its lookup time, whether its report named the lookup (a watch's only)
    let one_run = |setup: Setup, extra: &str, script: &str, env: &[(String, String)]| {
        let p = set_up(&root, setup);
        let (ms, lookup, _) = timed(&p, script, extra, env);
        let said = (setup == Setup::TagWatch).then(|| {
            std::thread::sleep(WATCH_LAST + Duration::from_millis(500));
            Guard::In(Box::new(p)).report(&[]).contains(MDNS_SOCKET)
        });
        (ms, lookup, said)
    };
    let mut out = String::from(
        "# What a denial costs (macOS, step 5d)\n\n7 rounds through 8 columns, the round's first column rotating, \
         after one unmeasured run per column. ms.\n\n",
    );
    for (name, script, extra_env) in cases {
        let mut all = env.clone();
        all.extend(extra_env);
        for (_, setup, extra) in &columns {
            let _ = one_run(*setup, extra, &script, &all);
        }
        let mut wall: Vec<Vec<f64>> = vec![Vec::new(); columns.len()];
        let mut look: Vec<Vec<f64>> = vec![Vec::new(); columns.len()];
        let mut said: Vec<Vec<bool>> = vec![Vec::new(); columns.len()];
        for round in 0..7 {
            for k in 0..columns.len() {
                let i = (round + k) % columns.len();
                let (_, setup, extra) = columns[i];
                let (ms, lookup, s) = one_run(setup, extra, &script, &all);
                wall[i].push(ms);
                look[i].extend(lookup);
                said[i].extend(s);
            }
        }
        let mut block = format!(
            "## {name}\n\n| Column | run: median / p90 | lookup: median | report names the lookup |\n|---|---|---|---|\n"
        );
        for (i, (column, ..)) in columns.iter().enumerate() {
            let w = median_p90(&mut wall[i]).map_or("—".into(), |(m, p)| format!("{m:.0} / {p:.0}"));
            let l = median_p90(&mut look[i]).map_or("—".into(), |(m, _)| format!("{m:.2}"));
            let s = match said[i].is_empty() {
                true => "—".to_string(),
                false => format!("{} of {}", said[i].iter().filter(|s| **s).count(), said[i].len()),
            };
            block.push_str(&format!("| {column} | {w} | {l} | {s} |\n"));
        }
        block.push('\n');
        print!("{block}");
        out.push_str(&block);
    }
    std::fs::write(out_file("d-q4-cost.txt"), &out).unwrap();
    std::fs::remove_dir_all(&root).ok();
}

// ── Q3b: who holds a connection's other end (`proc_pidfdinfo`) ──────────────────────────────────────────
// (the measurement's own reader; a check that goes in uses `procs::sockets` — frictions15's, on main soon)

/// `proc_pidfdinfo`'s flavor for a socket (`sys/proc_info.h`; not in the libc crate).
const PROC_PIDFDSOCKETINFO: i32 = 3;
/// `struct socket_fdinfo`'s offsets on arm64 and x86_64 (XNU `sys/proc_info.h`: `proc_fileinfo` 24 bytes, then
/// `socket_info` — `vinfo_stat` 136, …, `soi_family` at 160, `soi_kind` at 232, `soi_proto` at 240, whose
/// `in_sockinfo` holds the ports at 0/4 (network order, in an int) and the IPv4 addresses at 32+12 and 48+12).
/// Checked against a socket of the measurement's own before anything is measured (`the_reader_reads_itself`).
const FAMILY: usize = 24 + 160;
const KIND: usize = 24 + 232;
const IN: usize = 24 + 240;
/// `soi_kind`: `SOCKINFO_IN`, `SOCKINFO_TCP`.
const KINDS: [i32; 2] = [1, 2];

fn int_at(b: &[u8], at: usize) -> i32 {
    i32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
}

/// The IPv4 TCP sockets `pid` holds: (local, remote).
fn sockets_of(pid: i32) -> Vec<(SocketAddr, SocketAddr)> {
    let mut fds = vec![libc::proc_fdinfo { proc_fd: 0, proc_fdtype: 0 }; 4096];
    let size = (fds.len() * std::mem::size_of::<libc::proc_fdinfo>()) as i32;
    // SAFETY: a buffer of ours and its size in bytes.
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, fds.as_mut_ptr().cast(), size) };
    if n <= 0 {
        return Vec::new();
    }
    let count = n as usize / std::mem::size_of::<libc::proc_fdinfo>();
    let mut out = Vec::new();
    let mut buf = vec![0u64; 256];
    for fd in fds[..count].iter().filter(|f| f.proc_fdtype as i32 == libc::PROX_FDTYPE_SOCKET) {
        // SAFETY: a buffer of ours (2 KB, past `socket_fdinfo`) and its size in bytes.
        let got = unsafe {
            libc::proc_pidfdinfo(pid, fd.proc_fd, PROC_PIDFDSOCKETINFO, buf.as_mut_ptr().cast(), 2048)
        };
        if got <= 0 {
            continue;
        }
        // SAFETY: the u64 buffer seen as its bytes, within its length.
        let b = unsafe { std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), got as usize) };
        if b.len() < IN + 80 || int_at(b, FAMILY) != libc::AF_INET || !KINDS.contains(&int_at(b, KIND)) {
            continue;
        }
        let port = |at| u16::from_be(int_at(b, at) as u16);
        let v4 = |at: usize| Ipv4Addr::from(<[u8; 4]>::try_from(&b[at..at + 4]).unwrap());
        let local = SocketAddr::new(IpAddr::V4(v4(IN + 48 + 12)), port(IN + 4));
        let remote = SocketAddr::new(IpAddr::V4(v4(IN + 32 + 12)), port(IN));
        out.push((local, remote));
    }
    out
}

/// Every process in session `sid` (a run's: `spawn::setsid` makes its pid the session's id).
fn session(sid: i32) -> Vec<i32> {
    let mut pids = vec![0i32; 16384];
    let size = (pids.len() * 4) as i32;
    // SAFETY: a buffer of ours and its size in bytes.
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), size) };
    let n = (n.max(0) as usize).min(pids.len());
    // SAFETY: plain calls.
    pids[..n].iter().copied().filter(|&p| p > 0 && unsafe { libc::getsid(p) } == sid).collect()
}

/// The process of session `sid` holding the client's end of `accepted` — None: none of the run's does.
fn owner(sid: i32, accepted: &TcpStream) -> Option<i32> {
    let (peer, here) = (accepted.peer_addr().ok()?, accepted.local_addr().ok()?);
    session(sid).into_iter().find(|&pid| sockets_of(pid).iter().any(|&(l, r)| l == peer && r == here))
}

/// One connection's check: found or not, how long it took.
type Check = (bool, Duration);

/// A stand-in on a run's port (its listener): each connection checked against the run's session (given once
/// the run started), then answered 403 and closed. The checks so far, while it runs.
fn stand_in(listener: TcpListener, sid: Arc<OnceLock<i32>>) -> Arc<Mutex<Vec<Check>>> {
    let checks = Arc::new(Mutex::new(Vec::new()));
    let kept = checks.clone();
    crate::event::thread("tarae-5d-test", move || {
        // (none for 120 s: its run is long over — it ends)
        while let Some(s) = accept_within(&listener, 120) {
            let by = Instant::now() + Duration::from_secs(5);
            while sid.get().is_none() && Instant::now() < by {
                std::thread::sleep(Duration::from_millis(1));
            }
            let Some(&sid) = sid.get() else { continue };
            let t = Instant::now();
            let found = owner(sid, &s).is_some();
            kept.lock().unwrap().push((found, t.elapsed()));
            let _ = head(&s);
            let _ =
                (&s).write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });
    checks
}

/// Median and p99 of the checks' times, in µs.
fn times(c: &[Check]) -> (u128, u128) {
    let mut t: Vec<u128> = c.iter().map(|(_, d)| d.as_micros()).collect();
    t.sort_unstable();
    let at = |q: f64| t.get(((t.len() as f64 - 1.0) * q).round() as usize).copied().unwrap_or(0);
    (at(0.5), at(0.99))
}

/// Q3b: the peer check measured — (a) found for each client, sandboxed, many connections; (b) refused for a
/// process not the run's, holding the right token; (c) its cost with more of the run's processes; (d) its holes.
#[test]
#[ignore]
fn measure_peer_check() {
    let root = scratch("sb-5d-q3b");
    let env = base_env(&root);
    // The reader first, against a socket of this process's own: else every number below means nothing
    let l = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let c = TcpStream::connect(l.local_addr().unwrap()).unwrap();
    let mine = sockets_of(std::process::id() as i32);
    let want = (c.local_addr().unwrap(), l.local_addr().unwrap());
    assert!(mine.contains(&want), "the reader reads itself: {want:?} not in {mine:?}");
    drop((c, l));
    let mut table = String::from("# Q3b: the proc_pidfdinfo peer check (macOS, step 5d)\n\n");
    // SAFETY: plain call.
    let all = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    table.push_str(&format!("Processes on this machine: {all}\n\n| Case | found / connections | median µs | p99 µs | run |\n|---|---|---|---|---|\n"));
    let jv = java();
    if jv.is_some() {
        std::fs::write(
            root.join("Many.java"),
            "public class Many { public static void main(String[] a) throws Exception { int p = Integer.parseInt(a[0]); \
             for (int i = 0; i < 200; i++) { try (var s = new java.net.Socket(\"127.0.0.1\", p)) { \
             s.getOutputStream().write(\"GET http://a.example.invalid/ HTTP/1.1\\r\\nHost: a.example.invalid\\r\\n\\r\\n\".getBytes()); \
             s.getInputStream().readAllBytes(); } } } }",
        )
        .unwrap();
    }
    let many_go = go_binary(
        &root,
        "many",
        "package main\nimport \"net/http\"\nfunc main() { for i := 0; i < 200; i++ { r, err := http.Get(\"http://a.example.invalid/\"); if err == nil { r.Body.Close() } } }\n",
    );
    let curl = "curl -s -o /dev/null https://a.example.invalid/";
    let mut cases: Vec<Case> = vec![
        ("curl ×200 (CONNECT)".into(), format!("for i in $(seq 200); do {curl}; done"), vec![]),
        ("git ls-remote ×50 (its remote helper connects)".into(), "for i in $(seq 50); do git ls-remote https://a.example.invalid/x.git 2>/dev/null; done".into(), vec![]),
        ("python urllib ×200, one process".into(), "python3 -c \"import urllib.request as u\nfor i in range(200):\n  try: u.urlopen('http://a.example.invalid/',timeout=5)\n  except Exception: pass\"".into(), vec![]),
        ("node fetch ×200, one process".into(), "node -e \"(async()=>{for(let i=0;i<200;i++){try{await fetch('http://a.example.invalid/')}catch(e){}}})()\"".into(), vec![("NODE_USE_ENV_PROXY".into(), "1".into())]),
        ("cargo search ×10".into(), "for i in $(seq 10); do cargo search --limit 1 x >/dev/null 2>&1; done".into(), vec![]),
    ];
    if let Some(g) = &many_go {
        cases.push(("Go net/http ×200, one process".into(), format!("'{}'", g.display()), vec![]));
    }
    if let Some(j) = &jv {
        cases.push((
            "JVM sockets ×200, one process".into(),
            format!("'{j}' Many.java \"${{http_proxy##*:}}\""),
            vec![],
        ));
    }
    // (c): the run's own processes many — each `sleep` in its session, then 50 curls
    for n in [1, 50, 500] {
        cases.push((
            format!("cost: {n} more processes in the run, curl ×50"),
            format!("for i in $(seq {n}); do sleep 60 & done; for i in $(seq 50); do {curl}; done; kill $(jobs -p) 2>/dev/null"),
            vec![],
        ));
    }
    // (d): a process that left the run's session, and one whose parent went
    let raw = "import os,socket,time,sys\nport=int(os.environ['http_proxy'].rsplit(':',1)[1])\n";
    cases.push((
        "hole: a child that called setsid() itself".into(),
        format!("python3 -c \"{raw}os.setsid()\ns=socket.create_connection(('127.0.0.1',port)); s.sendall(b'GET http://a.example.invalid/ HTTP/1.1\\r\\n\\r\\n'); s.recv(100)\""),
        vec![],
    ));
    cases.push((
        "hole: a parent that exits, its child keeping the socket".into(),
        format!("python3 -c \"{raw}s=socket.create_connection(('127.0.0.1',port))\nif os.fork(): os._exit(0)\ntime.sleep(0.3); s.sendall(b'GET http://a.example.invalid/ HTTP/1.1\\r\\n\\r\\n'); s.recv(100)\"; sleep 1"),
        vec![],
    ));
    for (name, script, extra) in cases {
        let p = measured(&root);
        let listener = TcpListener::from(p.proxy.as_ref().unwrap().take().unwrap());
        let sid = Arc::new(OnceLock::new());
        let checks = stand_in(listener, sid.clone());
        let mut all = env.clone();
        all.extend(extra);
        let cmd = with_rules(&p, &script, "", &all);
        let e = run_measured(&p, &cmd, |pid| {
            let _ = sid.set(pid as i32);
        });
        std::thread::sleep(Duration::from_millis(300));
        let c = checks.lock().unwrap().clone();
        let found = c.iter().filter(|(f, _)| *f).count();
        let (median, p99) = times(&c);
        let ran = format!("exit {} · {} ms", e.code.map_or("signal".into(), |c| c.to_string()), e.ms);
        let row = format!("| {name} | {found} / {} | {median} | {p99} | {ran} |\n", c.len());
        print!("{row}");
        table.push_str(&row);
    }
    // (b): not the run's — this process itself, unsandboxed, with the right token; and another run's (its
    // profile `localhost:*`, as `llm.sandbox-localhost` makes it), each against the first run's session
    let p = measured(&root);
    let r = p.proxy.clone().unwrap();
    let listener = TcpListener::from(r.take().unwrap());
    let sid = Arc::new(OnceLock::new());
    let checks = stand_in(listener, sid.clone());
    let quiet = with_rules(&p, "sleep 3", "", &env);
    let first = std::thread::scope(|s| {
        let h = s.spawn(|| {
            run_measured(&p, &quiet, |pid| {
                let _ = sid.set(pid as i32);
            })
        });
        std::thread::sleep(Duration::from_millis(300));
        let request = format!(
            "GET http://a.example.invalid/ HTTP/1.1\r\nProxy-Authorization: Basic {}\r\n\r\n",
            basic(&format!("run:{}", r.token()))
        );
        for _ in 0..20 {
            if let Ok(mut c) = TcpStream::connect((Ipv4Addr::LOCALHOST, r.port)) {
                let _ = c.write_all(request.as_bytes());
                let _ = head(&c);
            }
        }
        let other = Policy { localhost: true, ..measured(&root) };
        let knock = format!("for i in $(seq 20); do /usr/bin/nc -w 1 127.0.0.1 {} </dev/null; done", r.port);
        let _ = run_measured(&other, &with_rules(&other, &knock, "", &env), |_| {});
        h.join().unwrap()
    });
    std::thread::sleep(Duration::from_millis(300));
    let c = checks.lock().unwrap().clone();
    let found = c.iter().filter(|(f, _)| *f).count();
    let (median, p99) = times(&c);
    let row = format!(
        "| not the run's: this process ×20 (right token) + another run (localhost:*) nc ×20 | {found} / {} | {median} | {p99} | exit {:?} |\n",
        c.len(),
        first.code
    );
    print!("{row}");
    table.push_str(&row);
    std::fs::write(out_file("d-q3b.txt"), &table).unwrap();
    std::fs::remove_dir_all(&root).ok();
}

/// Standard base64, padded.
fn basic(s: &str) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for c in s.as_bytes().chunks(3) {
        let n = c.iter().enumerate().fold(0u32, |n, (i, &b)| n | (u32::from(b) << (16 - 8 * i)));
        for i in 0..4 {
            match i <= c.len() {
                true => out.push(A[(n >> (18 - 6 * i) & 63) as usize] as char),
                false => out.push('='),
            }
        }
    }
    out
}

/// Tests: what the measurement's own base64 makes is what the proxy's parser reads (`parse::head`).
#[test]
fn the_measurements_base64_is_the_parsers() {
    for s in ["run:a", "run:ab", "run:abc", "run:tarae-fake-token-5d"] {
        let h = format!(
            "GET http://a.example.invalid/ HTTP/1.1\r\nProxy-Authorization: Basic {}\r\n\r\n",
            basic(s)
        );
        let got = super::parse::head(h.as_bytes()).unwrap().unwrap();
        let (user, password) = s.split_once(':').unwrap();
        assert_eq!(
            got.auth,
            super::parse::Auth::Basic {
                user: user.as_bytes().to_vec(),
                password: password.as_bytes().to_vec()
            }
        );
    }
}
