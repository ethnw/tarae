//! macdeny's measurements (`target/sb/macdeny-plan.md` §3), `#[ignore]`d — run by hand outside a sandbox
//! (`target/sb/macdeny/measure.sh`): what a command asks of macOS besides files and the network, run as Claude's
//! run is (`sandbox::policy` for this checkout), each its own `log stream` by the run's tag: what was refused.
//! (`BASE` was found with the profile's default allowed and reported — `(allow default (with report))`, the
//! default's denies put back — through a switch that lived only while measuring.) Tables go to
//! `$TARAE_MEASURE_OUT` (else `target/sb/macdeny`): `allow|deny  op  target  ×count  [processes]`, digits in
//! targets as `N`.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::sandbox::{Policy, after_run, command};

fn out_dir() -> PathBuf {
    let d = std::env::var_os("TARAE_MEASURE_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("target/sb/macdeny"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn var(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} unset"))
}

/// A run's policy for this checkout (`TARAE_MEASURE_ROOT` else this one), as Claude's runs get it, with these
/// grants: `terminals` (the project's grant), `localhost` (`llm.sandbox-localhost`), `network` (on, any host).
fn policy(grants: &[&str]) -> Policy {
    let root = std::env::var_os("TARAE_MEASURE_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| env!("CARGO_MANIFEST_DIR").into());
    let network = grants.contains(&"network");
    let llm = crate::llm::LlmConfig {
        sandbox_localhost: grants.contains(&"localhost"),
        sandbox_network: network,
        sandbox_hosts: if network { crate::llm::Hosts::Any } else { Default::default() },
        ..Default::default()
    };
    let mut p = crate::sandbox::policy(&root, &llm, &[]).unwrap();
    p.terminals = grants.contains(&"terminals");
    assert!(p.tag.is_some(), "a run's policy has its tag");
    p
}

/// One line of `log stream --style ndjson` from Seatbelt: (allow|deny, operation, target with digits as `N`,
/// process).
fn parse(line: &str) -> Option<(String, String, String, String)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let first = v["eventMessage"].as_str()?.lines().next()?;
    let after = &first[first.find("Sandbox: ")? + "Sandbox: ".len()..];
    let (process, after) = after.split_once('(')?;
    let (_, after) = after.split_once(") ")?;
    let (verdict, after) = after.split_once(' ')?;
    let verdict = match verdict {
        "allow" => "allow",
        v if v.starts_with("deny") => "deny",
        _ => return None,
    };
    let (op, target) = after.split_once(' ').unwrap_or((after, ""));
    let mut shaped = String::new();
    for c in target.trim().chars() {
        match c.is_ascii_digit() {
            true if shaped.ends_with('N') => {}
            true => shaped.push('N'),
            false => shaped.push(c),
        }
    }
    Some((verdict.into(), op.into(), shaped, process.into()))
}

/// The table of a log's lines.
fn table(log: &str) -> String {
    let mut seen: BTreeMap<(String, String, String), (usize, BTreeSet<String>)> = BTreeMap::new();
    for (v, op, target, process) in log.lines().filter_map(parse) {
        let e = seen.entry((v, op, target)).or_default();
        e.0 += 1;
        e.1.insert(process);
    }
    let mut s = String::new();
    for ((v, op, target), (n, ps)) in &seen {
        let ps: Vec<&str> = ps.iter().take(6).map(String::as_str).collect();
        s.push_str(&format!("{v:5} {op:30} {target}  ×{n}  [{}]\n", ps.join(" ")));
    }
    s
}

/// `script` in `p` (killed after `secs`), its `log stream` by the run's tag: `<name>.ndjson`, `.out` (both
/// streams) and `.txt` (what ran, how it ended, its output's last lines, the table) — the `.txt`'s text.
fn measured(p: &Policy, name: &str, script: &str, secs: u64) -> String {
    measured_as(p, name, script, secs, &|profile| profile)
}

/// `measured`, its profile as `edit` makes it (a rule added at its end wins: the later rule does).
fn measured_as(p: &Policy, name: &str, script: &str, secs: u64, edit: &dyn Fn(String) -> String) -> String {
    let dir = out_dir();
    let tag = p.tag.clone().unwrap();
    let predicate = format!("sender == \"Sandbox\" AND eventMessage CONTAINS \"{tag}\"");
    let log_file = dir.join(format!("{name}.ndjson"));
    let mut log = Command::new("/usr/bin/log")
        .args(["stream", "--style", "ndjson", "--predicate", &predicate])
        .stdout(std::fs::File::create(&log_file).unwrap())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    std::thread::sleep(Duration::from_secs(1));
    let out_file = dir.join(format!("{name}.out"));
    let out = std::fs::File::create(&out_file).unwrap();
    let mut c = edited(command(Some(p), "/bin/sh", &["-c".into(), script.into()]), edit);
    c.current_dir(&p.root);
    // (through the run's own spawn, as Claude's run: its port to tarae's proxy is handed over there)
    let ended = after_run(Some(p));
    let started = Instant::now();
    let spawned = crate::sandbox::spawn(Some(p), &c).unwrap();
    let pid = spawned.pid;
    let copied: Vec<_> = [spawned.stdout, spawned.stderr]
        .into_iter()
        .flatten()
        .map(|mut from| {
            let mut to = out.try_clone().unwrap();
            crate::event::thread("tarae-md-out", move || std::io::copy(&mut from, &mut to).ok())
        })
        .collect();
    let (tx, rx) = std::sync::mpsc::channel();
    crate::event::thread("tarae-md-wait", move || tx.send(crate::spawn::wait(pid)).ok());
    let code = match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(Ok(s)) => s.code().map_or("a signal".to_string(), |c| c.to_string()),
        Ok(Err(e)) => format!("unknown ({e})"),
        Err(_) => {
            // SAFETY: the run's own process group (it was started in a session of its own: `spawn::setsid`).
            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
            rx.recv().ok();
            format!("killed after {secs} s")
        }
    };
    for c in copied {
        c.join().ok();
    }
    let ms = started.elapsed().as_millis();
    ended();
    std::thread::sleep(Duration::from_millis(1500));
    log.kill().ok();
    log.wait().ok();
    let said = std::fs::read_to_string(&out_file).unwrap_or_default();
    let last: Vec<&str> = said.lines().rev().take(6).collect();
    let last: Vec<&str> = last.into_iter().rev().collect();
    let text = format!(
        "## {name}\n\n$ {script}\nexit {code} in {ms} ms\n{}\n\n{}\n",
        last.iter().map(|l| format!("  | {l}")).collect::<Vec<_>>().join("\n"),
        table(&std::fs::read_to_string(&log_file).unwrap_or_default())
    );
    std::fs::write(dir.join(format!("{name}.txt")), &text).unwrap();
    print!("{text}");
    text
}

/// `c` (`sandbox-exec -p <profile> …`) with its profile as `edit` makes it, its environment as it was.
fn edited(c: Command, edit: &dyn Fn(String) -> String) -> Command {
    let mut args: Vec<OsString> = c.get_args().map(OsString::from).collect();
    args[1] = edit(args[1].to_string_lossy().into_owned()).into();
    let mut cmd = Command::new(c.get_program());
    cmd.args(&args);
    for (k, v) in c.get_envs() {
        match v {
            Some(v) => cmd.env(k, v),
            None => cmd.env_remove(k),
        };
    }
    cmd
}

/// `TARAE_MEASURE_CMD` as Claude's run, named `TARAE_MEASURE_NAME`, with `TARAE_MEASURE_GRANTS` (`terminals`,
/// `localhost`, comma-separated) — within an hour.
#[test]
#[ignore]
fn measure_a_command() {
    let grants = std::env::var("TARAE_MEASURE_GRANTS").unwrap_or_default();
    let grants: Vec<&str> = grants.split(',').filter(|g| !g.is_empty()).collect();
    measured(&policy(&grants), &var("TARAE_MEASURE_NAME"), &var("TARAE_MEASURE_CMD"), 3600);
}

/// What compat's runs don't touch, one command each (§3.4): users, git, names, processes — its own and one of
/// the test's outside (`sleep`, holding a marker in its environment: G5) —, shared memory, a pty with and
/// without the grant, preferences, Xcode's tools, each runtime's start. Harmless: the outside process is the
/// test's own `sleep` (reniced, then killed), the keychain item looked up doesn't exist, the C program is
/// `return 0`. All in `probes.txt`.
#[test]
#[ignore]
fn measure_probes() {
    let mut outside =
        Command::new("/bin/sleep").arg("600").env("TARAE_G5", "tarae-g5-marker").spawn().unwrap();
    let pid = outside.id();
    let sysv = "perl -e 'use IPC::SysV qw(IPC_PRIVATE IPC_RMID S_IRUSR S_IWUSR); \
                my $id = shmget(IPC_PRIVATE, 4096, S_IRUSR|S_IWUSR); defined $id or die \"shmget: $!\\n\"; \
                shmctl($id, IPC_RMID, 0); print \"sysv ok\\n\"'";
    let probes: Vec<(&str, &[&str], String)> = vec![
        ("true", &[], "/usr/bin/true".into()),
        ("users", &[], "echo hi; id -un; id -Gn | wc -w".into()),
        (
            "git",
            &[],
            "cd \"$TMPDIR\" && git init -q g && cd g && git -c user.name=a -c user.email=a@b commit -q --allow-empty \
             -m x && git log --oneline | wc -l && git status --short | wc -l"
                .into(),
        ),
        ("localhost", &[], "python3 -c 'import socket; print(socket.getaddrinfo(\"localhost\", 80)[0][4])'".into()),
        ("ps-own", &[], "sleep 5 & ps -o pid=,command= -p $!; kill $!".into()),
        ("ps-other-env", &[], format!("ps -E -o command= -p {pid} | grep -c tarae-g5-marker")),
        ("pgrep-other", &[], format!("pgrep -f 'sleep 600' | grep -c {pid}")),
        ("lsof-own", &[], "lsof -p $$ | wc -l".into()),
        ("renice-other", &[], format!("renice +1 -p {pid}")),
        ("sysv-shm", &[], sysv.into()),
        (
            "posix-sem",
            &[],
            "python3 -c 'import multiprocessing as m; l = m.Lock(); l.acquire(); l.release(); print(\"sem ok\")'"
                .into(),
        ),
        ("pty", &[], "/usr/bin/script -q /dev/null /usr/bin/true".into()),
        ("pty-granted", &["terminals"], "/usr/bin/script -q /dev/null /usr/bin/true".into()),
        ("prefs", &[], "defaults read -g AppleLocale".into()),
        ("xcrun", &[], "xcrun --show-sdk-path".into()),
        (
            "cc",
            &[],
            "printf 'int main(void){return 0;}' > \"$TMPDIR/a.c\" && cc \"$TMPDIR/a.c\" -o \"$TMPDIR/a\" && \"$TMPDIR/a\""
                .into(),
        ),
        ("rustc", &[], "rustc --version".into()),
        ("node", &[], "node -e 'console.log(require(\"os\").cpus().length)'".into()),
        ("go", &[], "go version && go env GOOS".into()),
        ("java", &[], "java -version".into()),
        ("python", &[], "python3 -c 'import platform; print(platform.mac_ver()[0])'".into()),
        ("clipboard", &[], "pbpaste > /dev/null".into()),
        ("keychain", &[], "security find-generic-password -s tarae-macdeny-no-such-item > /dev/null".into()),
    ];
    let mut all = String::from("# macdeny probes\n\n");
    for (name, grants, script) in &probes {
        all.push_str(&measured(&policy(grants), name, script, 60));
        all.push('\n');
    }
    outside.kill().ok();
    outside.wait().ok();
    std::fs::write(out_dir().join("probes.txt"), all).unwrap();
}

/// What a rule changes (§3, after the confirm pass): each case under the profile as built, then with a rule
/// added or taken out — which refusal stops `/usr/bin/java` finding its runtime; whether preferences come through
/// cfprefsd though the plists are closed; whether another process's or its own child's arguments and
/// environment are read (`kern.procargs2`: G5); System V shared memory of another's (G6), call by call; Go's TLS
/// through tarae's proxy with and without trust's service (the network on, any host: proxy.golang.org's front
/// page, read only). Harmless: the outside processes are the test's own `sleep` and segment, removed after; the
/// preferences read are Finder's, counted not shown. All in `variants.txt`.
#[test]
#[ignore]
fn measure_variants() {
    let dir = out_dir();
    let add = |rule: &'static str| move |p: String| format!("{p}{rule}\n");
    let none = |p: String| p;
    let mut outside =
        Command::new("/bin/sleep").arg("600").env("TARAE_G5", "tarae-g5-marker").spawn().unwrap();
    let pid = outside.id();
    // (the segment another program keeps: a key of the test's own, a marker in it)
    let key = "0x7a4d6430";
    let made = Command::new("perl")
        .arg("-e")
        .arg(format!(
            "use IPC::SysV qw(IPC_CREAT S_IRUSR S_IWUSR); my $id = shmget({key}, 4096, IPC_CREAT|S_IRUSR|S_IWUSR) \
             // die \"$!\"; shmwrite($id, 'tarae-g6-marker', 0, 15) or die \"$!\"; print $id"
        ))
        .output()
        .unwrap();
    let segment = String::from_utf8_lossy(&made.stdout).trim().to_string();
    assert!(!segment.is_empty(), "the test's segment: {}", String::from_utf8_lossy(&made.stderr));
    // (Go's TLS: a client built here, outside, with the toolchain as it is)
    std::fs::write(
        dir.join("tlsget.go"),
        "package main\nimport (\"fmt\"; \"net/http\"; \"os\")\nfunc main() { r, err := http.Get(\"https://proxy.golang.org/\"); \
         if err != nil { fmt.Println(\"error:\", err); os.Exit(1) }; fmt.Println(\"status\", r.StatusCode) }\n",
    )
    .unwrap();
    let tlsget = dir.join("tlsget");
    let built = Command::new("go")
        .args(["build", "-o"])
        .arg(&tlsget)
        .arg(dir.join("tlsget.go"))
        .env("GOTOOLCHAIN", "local")
        .status()
        .unwrap();
    assert!(built.success());
    let procargs = |pid: &str| {
        format!(
            "python3 -c 'import ctypes, sys; libc = ctypes.CDLL(None, use_errno=True); \
             mib = (ctypes.c_int * 3)(1, 49, int(sys.argv[1])); size = ctypes.c_size_t(1 << 16); \
             buf = ctypes.create_string_buffer(1 << 16); r = libc.sysctl(mib, 3, buf, ctypes.byref(size), None, 0); \
             got = buf.raw[:size.value]; words = [w for w in got[4:].split(b\"\\0\") if w][:3]; \
             print(\"sysctl\", r, \"errno\", ctypes.get_errno(), \"argv\", words, \"env marker\", b\"tarae-g5-marker\" in got)' {pid}"
        )
    };
    // (the same read from outside the sandbox: what there is to read at all)
    let outside_read = Command::new("/bin/sh").arg("-c").arg(procargs(&pid.to_string())).output().unwrap();
    let outside_read = String::from_utf8_lossy(&outside_read.stdout).trim().to_string();
    let java = "java -version 2>&1 | head -2";
    let prefs = "defaults read com.apple.finder 2>&1 | wc -l";
    let cfprefsd = "(allow mach-lookup (global-name \"com.apple.cfprefsd.agent\") (global-name \"com.apple.cfprefsd.daemon\"))\n\
                    (allow ipc-posix-shm-read-data (ipc-posix-name-prefix \"apple.cfprefs.\"))";
    let attach = format!(
        "perl -e 'my $id = shmget({key}, 0, 0); defined $id or die \"get: $!\\n\"; my $b; \
         shmread($id, $b, 0, 15) or die \"read: $!\\n\"; print \"read $b\\n\"'"
    );
    let trustd = format!("(allow {})\n", super::TRUSTD);
    let own_child = format!("TARAE_G5=tarae-g5-marker sleep 30 & {}; kill $!", procargs("$!"));
    type Case<'a> = (&'a str, &'a [&'a str], String, Box<dyn Fn(String) -> String>);
    // Round 2 (`TARAE_MEASURE_ROUND`, else 2): with what round 1 found in the profile
    let second: Vec<Case> = vec![
        ("java", &[], java.into(), Box::new(none)),
        ("prefs-global", &[], "defaults read -g AppleLocale".into(), Box::new(none)),
        ("procargs-other", &[], procargs(&pid.to_string()), Box::new(none)),
        ("procargs-own-child", &[], own_child.clone(), Box::new(none)),
        ("tls", &["network"], format!("'{}'", tlsget.display()), Box::new(none)),
        (
            "tls-trustd",
            &["network"],
            format!("'{}'", tlsget.display()),
            Box::new(move |p: String| p.replace(&trustd, "")),
        ),
    ];
    // Round 3: why the proxy answered 502, and trust's service on its own — Go's TLS straight out (no proxy
    // variables, its own resolver: UDP 53 and 443 opened for the case), with and without it
    let direct = format!(
        "env -u HTTPS_PROXY -u https_proxy -u HTTP_PROXY -u http_proxy -u ALL_PROXY -u all_proxy GODEBUG=netdns=go '{}'",
        tlsget.display()
    );
    let out = "(allow network-outbound (remote ip \"*:443\") (remote ip \"*:53\"))\n";
    let without = format!("(allow {})\n", super::TRUSTD);
    let third: Vec<Case> = vec![
        (
            "curl-proxied",
            &["network"],
            "curl -sv -o /dev/null https://proxy.golang.org/ 2>&1 | grep -E '^(< |\\* )' | head -12".into(),
            Box::new(none),
        ),
        ("tls-direct", &["network"], direct.clone(), Box::new(move |p: String| format!("{p}{out}"))),
        (
            "tls-direct-trustd",
            &["network"],
            direct,
            Box::new(move |p: String| format!("{}{out}", p.replace(&without, ""))),
        ),
    ];
    // Round 4 (G27, srt's same-sandbox rows in): what a debugger on the run's own child needs besides — a C sleeper
    // the run builds (a system binary can't be debugged at all), attached and detached
    let lldb = "printf '#include <unistd.h>\\nint main(void){sleep(30);return 0;}' > \"$TMPDIR/s.c\" && \
                cc \"$TMPDIR/s.c\" -o \"$TMPDIR/s\" && { \"$TMPDIR/s\" & p=$!; \
                lldb --batch -p $p -o 'process detach' 2>&1 | tail -2; kill $p; }";
    let debug = "(allow system-debug (target same-sandbox))\n(allow sysctl-read (sysctl-name \"hw.cputype\") \
                 (sysctl-name \"hw.cpusubtype\") (sysctl-name \"machdep.virtual_address_size\") \
                 (sysctl-name \"security.mac.amfi.developer_mode_status\") (sysctl-name \"sysctl.proc_translated\"))\n";
    let listed = format!("{debug}(allow sysctl-read (sysctl-name \"kern.proc.all\"))\n");
    let fourth: Vec<Case> = vec![
        ("lldb-own+debug", &[], lldb.into(), Box::new(move |p: String| format!("{p}{debug}"))),
        ("lldb-own+debug+proc.all", &[], lldb.into(), Box::new(move |p: String| format!("{p}{listed}"))),
    ];
    // Round 1: after the confirm pass, before the profile took `kern.grade_cputype` and the global preferences
    let first: Vec<Case> = vec![
        ("java", &[], java.into(), Box::new(none)),
        (
            "java+coreservicesd",
            &[],
            java.into(),
            Box::new(add("(allow mach-lookup (global-name \"com.apple.CoreServices.coreservicesd\"))")),
        ),
        (
            "java+diskarbitrationd",
            &[],
            java.into(),
            Box::new(add("(allow mach-lookup (global-name \"com.apple.DiskArbitration.diskarbitrationd\"))")),
        ),
        (
            "java+grade_cputype",
            &[],
            java.into(),
            Box::new(add("(allow sysctl-write (sysctl-name \"kern.grade_cputype\"))")),
        ),
        (
            "java+globalprefs",
            &[],
            java.into(),
            Box::new(add(
                "(allow file-read-data (regex #\"/Library/Preferences/(ByHost/)?\\.GlobalPreferences[^/]*\\.plist$\"))",
            )),
        ),
        ("prefs", &[], prefs.into(), Box::new(none)),
        ("prefs+cfprefsd", &[], prefs.into(), Box::new(add(cfprefsd))),
        (
            "prefs+cfprefsd+user-preference-read",
            &[],
            prefs.into(),
            Box::new(add(
                "(allow mach-lookup (global-name \"com.apple.cfprefsd.agent\") (global-name \"com.apple.cfprefsd.daemon\"))\n(allow ipc-posix-shm-read-data (ipc-posix-name-prefix \"apple.cfprefs.\"))\n(allow user-preference-read)",
            )),
        ),
        ("procargs-other", &[], procargs(&pid.to_string()), Box::new(none)),
        (
            "procargs-other+allowed",
            &[],
            procargs(&pid.to_string()),
            Box::new(add("(allow sysctl-read (sysctl-name-prefix \"kern.procargs2.\"))")),
        ),
        ("procargs-own-child", &[], own_child.clone(), Box::new(none)),
        ("sysv-attach-other", &[], attach.clone(), Box::new(none)),
        ("sysv-attach-other+ipc-sysv", &[], attach.clone(), Box::new(add("(allow ipc-sysv*)"))),
    ];
    let round = std::env::var("TARAE_MEASURE_ROUND").unwrap_or_else(|_| "2".into());
    let cases = match round.as_str() {
        "1" => first,
        "3" => third,
        "4" => fourth,
        _ => second,
    };
    let mut all = format!(
        "# macdeny variants, round {round}\n\nprocargs-other, read outside the sandbox: {outside_read}\n\n"
    );
    for (name, grants, script, edit) in &cases {
        all.push_str(&measured_as(&policy(grants), name, script, 60, edit.as_ref()));
        all.push('\n');
    }
    outside.kill().ok();
    outside.wait().ok();
    let removed = Command::new("perl")
        .arg("-e")
        .arg(format!("use IPC::SysV qw(IPC_RMID); shmctl({segment}, IPC_RMID, 0) or die \"$!\""))
        .status();
    all.push_str(&format!("the test's segment {segment} removed: {removed:?}\n"));
    std::fs::write(dir.join(format!("variants-{round}.txt")), all).unwrap();
}

/// The table of `TARAE_MEASURE_LOG` (a `log stream` the script ran around compat's runs), as
/// `TARAE_MEASURE_NAME.txt`.
#[test]
#[ignore]
fn summarize_a_log() {
    let log = std::fs::read_to_string(var("TARAE_MEASURE_LOG")).unwrap();
    let text = table(&log);
    std::fs::write(out_dir().join(format!("{}.txt", var("TARAE_MEASURE_NAME"))), &text).unwrap();
    print!("{text}");
}

/// A run's start under the profile: `/usr/bin/true` 50 times after 5 unmeasured, ms median / p90, and the
/// profile's size — `cost-<TARAE_MEASURE_NAME>.txt` (else `cost-deny-default.txt`).
#[test]
#[ignore]
fn measure_start_cost() {
    let which = std::env::var("TARAE_MEASURE_NAME").unwrap_or_else(|_| "deny-default".into());
    let p = policy(&[]);
    let mut ms: Vec<f64> = Vec::new();
    for i in 0..55 {
        let mut c = command(Some(&p), "/usr/bin/true", &[]);
        c.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        let t = Instant::now();
        assert!(c.status().unwrap().success());
        if i >= 5 {
            ms.push(t.elapsed().as_secs_f64() * 1000.0);
        }
    }
    ms.sort_by(f64::total_cmp);
    let profile = p.profile();
    let text = format!(
        "{which}: median {:.2} ms, p90 {:.2} ms over {} runs; profile {} bytes, {} lines\n",
        ms[ms.len() / 2],
        ms[ms.len() * 9 / 10],
        ms.len(),
        profile.len(),
        profile.lines().count()
    );
    std::fs::write(out_dir().join(format!("cost-{which}.txt")), &text).unwrap();
    print!("{text}");
}
