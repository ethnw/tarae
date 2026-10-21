//! What a sandboxed run must be refused (`docs/roadmap.md` §2 item 3, `docs/threat-model.md` §1): one table of
//! 26 rows across the threat model's claims (§1.1–§1.7, §1.9), tried from inside a real sandboxed run against
//! stand-in files, listeners and processes in the test's own scratch folder (`Scratch`).
//!
//! Nothing of the user's or the host's is touched: home, `$XDG_*` and `$TMPDIR` are folders in the scratch
//! directory passed to a child test process (`in_stand_in_home`, without `env::set_var`); every key holds
//! `tarae-fake-token`; every listener is bound to `127.0.0.1:0` (or this machine's own address on port 0);
//! every process tried is a stand-in this test started; git runs only in the scratch checkouts with hooks and
//! signing off.
//!
//! Each row records `(claim, id, verdict, witness)` — deterministic across runs (no temp paths, PIDs, port
//! numbers or `errno` values in the snapshot; `errno` is kept in `detail` for failure diagnostics) — and checks
//! two properties:
//! - **Property 1 (Refusal):** every row's measured verdict matches what the tier promises (`refused`, `caught`
//!   by `snapshot::Before::since`, `shared` where the tier documents it, `rule` for pure rule checks, `not-here`
//!   for another OS's rows), and `tarae-fake-token` never appears in a run's environment or output.
//! - **Property 2 (Honesty):** what `policy.guarantees()` says is kept vs shared matches what the two-session
//!   rows found (`guarantees::said_as_found`), and any `shared` row is named in `Guard::In(policy).report(&[])`.

use std::collections::HashMap;
use std::ffi::{CString, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::os::unix::fs as unix_fs;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use super::guarantees::{Part, said_as_found};
use super::proxy::{FromProxy, Scope, ToProxy};
use super::*;

const FAKE_TOKEN: &str = "tarae-fake-token";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    Refused,
    Caught,
    Shared,
    Allowed,
    Rule,
    NotHere,
    /// Every part refused by the system, not by tarae (its baseline couldn't either): nothing of tarae's shown.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    System,
}

impl Verdict {
    fn as_str(self) -> &'static str {
        match self {
            Verdict::Refused => "refused",
            Verdict::Caught => "caught",
            Verdict::Shared => "shared",
            Verdict::Allowed => "allowed",
            Verdict::Rule => "rule",
            Verdict::NotHere => "not-here",
            Verdict::System => "system",
        }
    }
}

struct Row {
    claim: &'static str,
    id: &'static str,
    expected: Verdict,
    verdict: Verdict,
    witness: String,
    detail: String,
    part: Option<(Part, Option<bool>)>,
    shared_phrase: Option<&'static str>,
}

/// Runs `in_stand_in_home` in a child process whose `HOME` and `XDG_*` point at a scratch directory outside
/// `/tmp` and `/var/folders`, so `config::state_dir()` and every path computed from the environment stay inside
/// the scratch directory without calling `env::set_var`.
#[test]
fn must_be_refused() {
    if skipped("sandbox::refused::must_be_refused") {
        return;
    }
    let scratch = crate::scratch::Scratch::in_dir(Path::new("/var/tmp"), "refused");
    let base = profile_path(&scratch);
    let home = base.join("h");
    for d in [".config", ".local/state", ".local/share", ".cache"] {
        fs::create_dir_all(home.join(d)).unwrap();
    }
    let exe = std::env::current_exe().unwrap();
    let mut cmd = Command::new(&exe);
    cmd.args(["--exact", "sandbox::refused::in_stand_in_home", "--ignored", "--nocapture"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("TARAE_REFUSED_BASE", &base)
        .stdin(Stdio::null());
    // Row 26: the child's controlling terminal, a pty of the test's own (held open here until the child is done:
    // its hangup would end it) — the baseline has one, so a run without one shows tarae's `setsid`, not the
    // container's lack of a terminal. The child only: its session and terminal, never this process's.
    #[cfg(target_os = "linux")]
    let ctty = PtyGuard::new();
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let slave = ctty.slave_fd;
        // SAFETY: the forked child, before its exec, calls only `setsid` and `ioctl` (async-signal-safe) on a
        // descriptor opened before the fork; `last_os_error` reads errno, no allocation.
        unsafe {
            cmd.pre_exec(move || {
                if libc::setsid() < 0 || libc::ioctl(slave, libc::TIOCSCTTY, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let out = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "in_stand_in_home failed:\nstdout:\n{stdout}\nstderr:\n{stderr}");
    // (a table not held against a snapshot, said where a passing test is seen: past libtest's capture)
    for line in stderr.lines().filter(|l| l.starts_with(UNCOMPARED)) {
        let _ = std::io::Write::write_all(&mut std::io::stderr(), format!("{line}\n").as_bytes());
    }
}

/// Whether the suite can't run here — then said past libtest's capture (`SELF_SKIP`), with why. macOS: inside a
/// sandbox already. Linux: inside one too (a run's own rules would be in the way, and its scratch folder
/// can't be made), outside `cargo xtask linux` (`TARAE_REFUSED_SUITE`: the snapshots are its tiers', a plain
/// `cargo test` on another kernel can't say which it holds), or below Landlock ABI 2 (where a tier is expected,
/// that fails instead).
fn skipped(test: &str) -> bool {
    #[cfg(target_os = "macos")]
    return skipped_inside(test);
    #[cfg(target_os = "linux")]
    {
        let why = if std::env::var_os(MARK).is_some() {
            "inside a sandboxed run: its rules would be in the way; run `cargo xtask linux test` outside one"
                .to_string()
        } else if std::env::var_os("TARAE_REFUSED_SUITE").is_none() {
            "its snapshots are `cargo xtask linux`'s tiers (TARAE_REFUSED_SUITE): run it there".to_string()
        } else {
            match crate::landlock::abi() {
                Ok(n) if n >= 2 => return false,
                abi => {
                    if std::env::var("TARAE_EXPECT_TIER").is_ok_and(|t| !t.trim().is_empty()) {
                        panic!("{test}: a tier was expected, Landlock: {abi:?}");
                    }
                    format!("needs Landlock ABI 2, this kernel: {abi:?}")
                }
            }
        };
        let line = format!("{}{test} — {why}\n", crate::test_results::SELF_SKIP);
        let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        true
    }
}

fn confined_policy_in(root: &Path, llm: &crate::llm::LlmConfig, here: &Here) -> Policy {
    let policy = run_policy_in(root, llm, &[], here).unwrap();
    #[cfg(target_os = "linux")]
    let policy = confined(policy);
    // (said here, before a stand-in's spawn would unwrap it deep down)
    #[cfg(target_os = "linux")]
    if let Some(Err(why)) = &policy.confine {
        panic!("the sandbox couldn't be made for {}: {why}", root.display());
    }
    policy
}

#[test]
#[ignore = "run by must_be_refused with a stand-in HOME and XDG_*"]
fn in_stand_in_home() {
    let Some(base) = std::env::var_os("TARAE_REFUSED_BASE").map(PathBuf::from) else {
        return;
    };
    let base = profile_path(&base);
    let home = base.join("h");
    let shared_tmp = base.join("tmp");
    let runs = base.join("runs");
    let outside = base.join("out");
    let root = base.join("p");
    let other_root = base.join("q");
    for d in [&shared_tmp, &runs, &outside, &root.join("src"), &other_root.join("src")] {
        fs::create_dir_all(d).unwrap();
    }

    // Stand-in files in home, state, config, outside and both sessions' checkouts
    write_file(&outside.join("existing"), "keep\n");
    write_file(&other_root.join("src/lib.rs"), "pub fn q() {}\n");
    write_file(&home.join(".config/tarae/config.toml"), "# user config\n");
    write_file(&home.join(".local/state/tarae/sandbox.json"), &format!("{{\"token\":\"{FAKE_TOKEN}\"}}\n"));
    write_file(&home.join(".local/state/tarae/trust.json"), "{}\n");
    fs::create_dir_all(home.join(".local/share/tarae/grammars")).unwrap();
    fs::create_dir_all(home.join(".local/share/tarae/plugins")).unwrap();
    write_file(&home.join(".ssh/id_ed25519"), &format!("{FAKE_TOKEN}\n"));
    write_file(&home.join(".aws/credentials"), &format!("[default]\naws_secret_access_key = {FAKE_TOKEN}\n"));
    write_file(&home.join("notes.txt"), &format!("{FAKE_TOKEN}\n"));
    write_file(
        &home.join(".gitconfig"),
        &format!("[user]\n\tname = t\n[http]\n\textraHeader = Authorization: Bearer {FAKE_TOKEN}\n"),
    );

    // Project files in `root` (a temporary git repository, hooks and signing disabled)
    write_file(
        &root.join("Cargo.toml"),
        "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    );
    write_file(&root.join("src/lib.rs"), "pub fn p() {}\n");
    write_file(&root.join("sub/build.rs"), "fn main() {}\n");
    write_file(&root.join(".env"), &format!("SECRET={FAKE_TOKEN}\n"));
    write_file(
        &root.join(".tarae.toml"),
        "[llm]\nsandbox = false\nsandbox-network = true\nsandbox-hosts = \"*\"\nsandbox-write = [\"..\"]\n\n\
         [session]\nended = [\"true\"]\n",
    );
    for d in ["new1", "new2", "new3", "crates"] {
        fs::create_dir_all(root.join(d)).unwrap();
        write_file(&root.join(d).join(".keep"), "");
    }
    init_git(&root);
    init_git(&other_root);
    write_file(&root.join(".git/hooks/pre-commit"), "#!/bin/sh\nexit 0\n");

    // Verify `.tarae.toml` risky settings are held back before project trust
    let sources = crate::config::Sources {
        user: Some(home.join(".config/tarae/config.toml")),
        trust: Some(home.join(".local/state/tarae/trust.json")),
    };
    let (loaded, _) = crate::config::load(&root, &sources);
    let untrusted_held = loaded.project.as_ref().is_some_and(|p| p.state == crate::trust::State::Ask)
        && loaded.editor.llm.sandbox
        && !loaded.editor.llm.sandbox_network
        && loaded.editor.llm.sandbox_hosts == crate::llm::Hosts::Registries
        && loaded.editor.llm.sandbox_write.is_empty()
        && loaded.editor.session_ended.is_empty();

    // Build `Here` with fake secret variables in tarae's environment (must be scrubbed from the run)
    let mut here = Here { tmp: Some(shared_tmp.clone()), runs: Some(runs.clone()), ..Here::now() };
    here.env.insert("HOME".into(), home.clone().into_os_string());
    here.env.insert("XDG_CONFIG_HOME".into(), home.join(".config").into_os_string());
    here.env.insert("XDG_STATE_HOME".into(), home.join(".local/state").into_os_string());
    here.env.insert("XDG_DATA_HOME".into(), home.join(".local/share").into_os_string());
    here.env.insert("XDG_CACHE_HOME".into(), home.join(".cache").into_os_string());
    for k in ["OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GITHUB_TOKEN", "AWS_SECRET_ACCESS_KEY", "TARAE_SECRET"]
    {
        here.env.insert(k.into(), OsString::from(FAKE_TOKEN));
    }

    toolchain_found_in(&root, ToolDirs::default(), &here);
    toolchain_found_in(&other_root, ToolDirs::default(), &here);
    let llm = crate::llm::LlmConfig::default();
    let policy = confined_policy_in(&root, &llm, &here);
    let _policy_cleanup = RunCleanupGuard { policy: policy.clone() };
    let other_policy = confined_policy_in(&other_root, &llm, &here);
    let _other_cleanup = RunCleanupGuard { policy: other_policy.clone() };
    if let Ok(want) = std::env::var("TARAE_EXPECT_TIER") {
        let want = want.trim();
        if !want.is_empty() {
            assert_eq!(policy.tier_said().as_deref(), Some(want), "unexpected sandbox tier");
        }
    }
    let own_run = policy.run.as_ref().unwrap().path().to_path_buf();
    let other_run = other_policy.run.as_ref().unwrap().path().to_path_buf();

    // Take baseline snapshot before creating transient `out-link` / `own.sock` so row 5 on Linux can verify
    // `Before::since()` catches newly created protected names in folders that had none at run start.
    let before_main = crate::snapshot::Before::take(&root);

    // Symlink inside `root` pointing outside (for row 1, removed before row 8's snapshot)
    let out_link = root.join("out-link");
    unix_fs::symlink(outside.join("via-link"), &out_link).unwrap();

    // Listeners for rows 10, 11, 12, 16
    let tcp_unopened = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    tcp_unopened.set_nonblocking(true).unwrap();
    let tcp_unopened_port = tcp_unopened.local_addr().unwrap().port();

    let non_loop_ip = super::proxy::egress::addresses()
        .unwrap_or_default()
        .into_iter()
        .find(|ip| !ip.is_loopback() && ip.is_ipv4())
        .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
    let tcp_non_loop = TcpListener::bind((non_loop_ip, 0)).unwrap();
    tcp_non_loop.set_nonblocking(true).unwrap();
    let tcp_non_loop_addr = tcp_non_loop.local_addr().unwrap();

    let udp_loop = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    udp_loop.set_nonblocking(true).unwrap();
    let udp_loop_port = udp_loop.local_addr().unwrap().port();

    let udp_dns = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    udp_dns.set_nonblocking(true).unwrap();
    let udp_dns_port = udp_dns.local_addr().unwrap().port();

    let sock_other_run = other_run.join("s.sock");
    let sock_state = home.join(".local/state/tarae/ui.sock");
    let sock_outside = outside.join("s.sock");
    let sock_tmp = shared_tmp.join("s.sock");
    let sock_own_root = root.join("own.sock");
    let sock_own_run = own_run.join("own.sock");
    let _unix_listeners: Vec<UnixListener> =
        [&sock_other_run, &sock_state, &sock_outside, &sock_tmp, &sock_own_root, &sock_own_run]
            .into_iter()
            .map(|p| UnixListener::bind(p).unwrap())
            .collect();

    // Stand-in processes for row 17: tarae stand-in (outside sandbox) and other session's sandboxed stand-in
    let exe = std::env::current_exe().unwrap();
    let tarae_guard = spawn_stand_in(&exe, None, &root, "tarae-stand-in");
    let other_guard = spawn_sandboxed_stand_in(&exe, &other_policy, "other-stand-in");

    // Shared memory objects for row 18 (created by this test process, standing in for another session), named
    // by the scratch folder's own `<pid>-<n>` (made new: two runs never meet, whatever their pid namespaces)
    let own = base
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("tarae-refused-"))
        .expect("the base is a `Scratch` named `refused`")
        .to_string();
    let posix_name = format!("/tarae-ref-{own}");
    let sysv_key =
        0x7900_0000 | (own.bytes().fold(0u32, |h, b| h.wrapping_mul(31) ^ u32::from(b)) & 0x00ff_ffff);
    let shm_guard = ShmGuard::new(&posix_name, sysv_key as libc::key_t);

    // Stand-in PTY on Linux for row 26 (held open outside the run)
    #[cfg(target_os = "linux")]
    let pty_guard = PtyGuard::new();
    #[cfg(target_os = "linux")]
    let stand_in_pty = pty_guard.slave_path.clone();
    #[cfg(not(target_os = "linux"))]
    let stand_in_pty = String::new();

    #[cfg(target_os = "linux")]
    let keeper_seen =
        keeper_seen(policy.confine.as_ref().unwrap().as_ref().unwrap().keeper.as_ref().unwrap());

    let cfg = [
        ("BASE", base.display().to_string()),
        ("HOME_DIR", home.display().to_string()),
        ("ROOT", root.display().to_string()),
        ("OTHER_ROOT", other_root.display().to_string()),
        ("OWN_RUN", own_run.display().to_string()),
        ("OTHER_RUN", other_run.display().to_string()),
        ("RUNS", runs.display().to_string()),
        ("OUTSIDE", outside.display().to_string()),
        ("SHARED_TMP", shared_tmp.display().to_string()),
        ("TCP_UNOPENED", tcp_unopened_port.to_string()),
        ("TCP_NON_LOOP", tcp_non_loop_addr.to_string()),
        ("UDP_LOOP", udp_loop_port.to_string()),
        ("UDP_DNS", udp_dns_port.to_string()),
        ("TARAE_PID", tarae_guard.pid.to_string()),
        ("OTHER_PID", other_guard.pid.to_string()),
        ("POSIX_SHM", posix_name.clone()),
        ("SYSV_KEY", sysv_key.to_string()),
        ("STAND_IN_PTY", stand_in_pty.clone()),
        #[cfg(target_os = "linux")]
        ("KEEPER_PID", keeper_seen.0.clone()),
        #[cfg(target_os = "linux")]
        ("KEEPER_PIDNS", keeper_seen.1.clone()),
    ];
    let cfg_file = own_run.join("refused.cfg");
    let cfg_text: String = cfg.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    write_file(&cfg_file, &cfg_text);

    #[cfg(target_os = "linux")]
    let base_linux = run_linux_baseline(&home, &root, &own_run, &stand_in_pty);

    // Run the main sandboxed helper covering rows 1–7, 9–13, 16–26 — the stand-ins running before and after
    // (one that ended on its own would read as a refused signal: ESRCH)
    assert!(tarae_guard.running() && other_guard.running(), "a stand-in ended before the run");
    let (main_out, main_leaked) = run_helper(&exe, &policy, "main", &cfg_file);
    let got = parse_kv(&main_out);
    assert!(tarae_guard.running() && other_guard.running(), "a stand-in ended during the run");
    #[cfg(target_os = "linux")]
    let refusing_got = refusing_userns_run(&exe, &policy, &cfg_file);

    // Clean up stand-ins and shm early (or on Drop if an earlier step panicked)
    #[cfg(target_os = "linux")]
    drop(pty_guard);
    drop(tarae_guard);
    drop(other_guard);
    drop(shm_guard);
    fs::remove_file(&out_link).ok();
    fs::remove_file(&sock_own_root).ok();
    if root.join("sub2").exists() && !root.join("sub").exists() {
        fs::rename(root.join("sub2"), root.join("sub")).unwrap();
    }
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    let since_main = before_main.since().unwrap();
    for p in ["new1/Cargo.toml", "new2/CARGO.TOML", "new3/Cargo.toml", "sub/Cargo.toml"] {
        fs::remove_file(root.join(p)).ok();
    }

    // Check listener accept counts for rows 10, 11, 12
    let tcp_unopened_accepted = usize::from(tcp_unopened.accept().is_ok());
    let tcp_non_loop_accepted = usize::from(tcp_non_loop.accept().is_ok());
    let mut udp_buf = [0u8; 16];
    let udp_loop_recv = usize::from(udp_loop.recv(&mut udp_buf).is_ok());
    let udp_dns_recv = usize::from(udp_dns.recv(&mut udp_buf).is_ok());

    // Row 8: move a folder holding `build.rs` from `$TMPDIR` into `root/crates/x` and check `Before::since()`
    let before = crate::snapshot::Before::take(&root);
    let (move_out, move_leaked) = run_helper(&exe, &policy, "move-in", &cfg_file);
    let move_got = parse_kv(&move_out);
    let since = before.since().unwrap();
    let flagged_move = since.protected.iter().any(|m| m.path == "crates/x/build.rs");
    fs::remove_dir_all(root.join("crates/x")).ok();
    drop(_policy_cleanup);
    drop(_other_cleanup);

    // Row 14: proxy unlisted host (`refused.invalid:443`), other session's proxy port, other session's token
    #[cfg_attr(not(target_os = "linux"), allow(unused_variables))]
    let (proxy_got, proxy_leaked, has_proxy) =
        run_proxy_row(&exe, &root, &other_root, &here, tcp_unopened_port);

    let g = policy.guarantees().expect("sandboxed policy has guarantees");
    let is_offline = g.tier() == Some(super::guarantees::Tier::Offline);

    // Assemble the 26 rows
    let mut rows = Vec::new();

    // 1. 1.1 | write-outside
    let r1_direct = got["r1_direct"] == "refused" && !outside.join("marker").exists();
    let r1_append =
        got["r1_append"] == "refused" && fs::read_to_string(outside.join("existing")).unwrap() == "keep\n";
    let r1_link = got["r1_link"] == "refused" && !outside.join("via-link").exists();
    let r1_ok = r1_direct && r1_append && r1_link;
    rows.push(Row {
        claim: "1.1",
        id: "write-outside",
        expected: Verdict::Refused,
        verdict: if r1_ok { Verdict::Refused } else { Verdict::Allowed },
        witness: format!(
            "direct={}, append={}, via-link={}",
            word(r1_direct),
            word(r1_append),
            word(r1_link)
        ),
        detail: got.get("r1_err").cloned().unwrap_or_default(),
        part: None,
        shared_phrase: None,
    });

    // 2. 1.1, 1.6 | write-other-session
    let r2_checkout = got["r2_checkout"] == "refused" && !other_root.join("src/pwn").exists();
    let r2_append = got["r2_append"] == "refused"
        && fs::read_to_string(other_root.join("src/lib.rs")).unwrap() == "pub fn q() {}\n";
    let r2_run = got["r2_run"] == "refused" && !other_run.join("pwn").exists();
    let r2_runs = got["r2_runs"] == "refused" && !runs.join("pwn").exists();
    let r2_ok = r2_checkout && r2_append && r2_run && r2_runs;
    rows.push(Row {
        claim: "1.1, 1.6",
        id: "write-other-session",
        expected: Verdict::Refused,
        verdict: if r2_ok { Verdict::Refused } else { Verdict::Allowed },
        witness: format!(
            "checkout={}, append={}, run-dir={}, runs-base={}",
            word(r2_checkout),
            word(r2_append),
            word(r2_run),
            word(r2_runs)
        ),
        detail: got.get("r2_err").cloned().unwrap_or_default(),
        part: Some((Part::Files, Some(!r2_ok))),
        shared_phrase: None,
    });

    // 3. 1.3, 1.9 | write-tarae-own
    let r3_cfg = got["r3_config"] == "refused";
    let r3_sb = got["r3_sandbox"] == "refused";
    let r3_tr = got["r3_trust"] == "refused";
    let r3_gr = got["r3_grammar"] == "refused" && !home.join(".local/share/tarae/grammars/x.dylib").exists();
    let r3_pl = got["r3_plugin"] == "refused" && !home.join(".local/share/tarae/plugins/x.wasm").exists();
    let r3_repo = got["r3_repo_cfg"] == "refused";
    let r3_ok = r3_cfg && r3_sb && r3_tr && r3_gr && r3_pl && r3_repo && untrusted_held;
    rows.push(Row {
        claim: "1.3, 1.9",
        id: "write-tarae-own",
        expected: Verdict::Refused,
        verdict: if r3_ok { Verdict::Refused } else { Verdict::Allowed },
        witness: format!(
            "config={}, sandbox-json={}, trust-json={}, grammar={}, plugin={}, repo-config={}, untrusted-held={}",
            word(r3_cfg),
            word(r3_sb),
            word(r3_tr),
            word(r3_gr),
            word(r3_pl),
            word(r3_repo),
            untrusted_held
        ),
        detail: got.get("r3_err").cloned().unwrap_or_default(),
        part: None,
        shared_phrase: None,
    });

    // 4. 1.2, 1.7 | write-protected
    let r4_cargo = got["r4_cargo"] == "refused" && root.join("Cargo.toml").is_file();
    let r4_build = got["r4_build"] == "refused" && root.join("sub/build.rs").is_file();
    let r4_git_cfg = got["r4_git_cfg"] == "refused" && root.join(".git/config").is_file();
    let r4_git_hook = got["r4_git_hook"] == "refused" && root.join(".git/hooks/pre-commit").is_file();
    let r4_ok = r4_cargo && r4_build && r4_git_cfg && r4_git_hook;
    rows.push(Row {
        claim: "1.2, 1.7",
        id: "write-protected",
        expected: Verdict::Refused,
        verdict: if r4_ok { Verdict::Refused } else { Verdict::Allowed },
        witness: format!(
            "cargo-toml={}, build-rs={}, git-config={}, git-hook={}",
            word(r4_cargo),
            word(r4_build),
            word(r4_git_cfg),
            word(r4_git_hook)
        ),
        detail: got.get("r4_err").cloned().unwrap_or_default(),
        part: None,
        shared_phrase: None,
    });

    // 5. 1.2 | make-protected
    #[cfg(target_os = "macos")]
    {
        let r5_new = got["r5_new"] == "refused" && !root.join("new1/Cargo.toml").exists();
        let r5_case = got["r5_case"] == "refused" && !root.join("new2/CARGO.TOML").exists();
        let r5_sym =
            got["r5_sym"] == "refused" && fs::symlink_metadata(root.join("new3/Cargo.toml")).is_err();
        let r5_ok = r5_new && r5_case && r5_sym;
        rows.push(Row {
            claim: "1.2",
            id: "make-protected",
            expected: Verdict::Refused,
            verdict: if r5_ok { Verdict::Refused } else { Verdict::Allowed },
            witness: format!("new={}, case={}, symlink={}", word(r5_new), word(r5_case), word(r5_sym)),
            detail: got.get("r5_err").cloned().unwrap_or_default(),
            part: None,
            shared_phrase: None,
        });
    }
    #[cfg(target_os = "linux")]
    {
        let r5_beside = got["r5_beside"] == "refused";
        let r5_new_caught =
            got["r5_new"] == "allowed" && since_main.protected.iter().any(|m| m.path == "new1/Cargo.toml");
        let r5_case_caught =
            got["r5_case"] == "allowed" && since_main.protected.iter().any(|m| m.path == "new2/CARGO.TOML");
        let r5_sym_caught =
            got["r5_sym"] == "allowed" && since_main.protected.iter().any(|m| m.path == "new3/Cargo.toml");
        let r5_ok = r5_beside && r5_new_caught && r5_case_caught && r5_sym_caught;
        let cw = |caught: bool| if caught { "caught" } else { "missed" };
        rows.push(Row {
            claim: "1.2",
            id: "make-protected",
            expected: Verdict::Caught,
            verdict: if r5_ok { Verdict::Caught } else { Verdict::Allowed },
            witness: format!(
                "beside-protected={}, new={}, case={}, symlink={}",
                word(r5_beside),
                cw(r5_new_caught),
                cw(r5_case_caught),
                cw(r5_sym_caught)
            ),
            detail: got.get("r5_err").cloned().unwrap_or_default(),
            part: None,
            shared_phrase: None,
        });
    }

    // 6. 1.2, 1.4 | hardlink-protected
    let r6_cargo = got["r6_cargo"] == "refused" && !root.join("src/link-cargo").exists();
    let r6_build = got["r6_build"] == "refused" && !root.join("src/link-build").exists();
    let r6_ssh = got["r6_ssh"] == "refused" && !root.join("src/link-ssh").exists();
    let r6_env = got["r6_env"] == "refused" && !root.join("src/link-env").exists();
    let r6_ok = r6_cargo && r6_build && r6_ssh && r6_env;
    rows.push(Row {
        claim: "1.2, 1.4",
        id: "hardlink-protected",
        expected: Verdict::Refused,
        verdict: if r6_ok { Verdict::Refused } else { Verdict::Allowed },
        witness: format!(
            "cargo-toml={}, build-rs={}, ssh-key={}, dot-env={}",
            word(r6_cargo),
            word(r6_build),
            word(r6_ssh),
            word(r6_env)
        ),
        detail: got.get("r6_err").cloned().unwrap_or_default(),
        part: None,
        shared_phrase: None,
    });

    // 7. 1.2 | move-protected-parent
    let r7_write = got["r7_write"] == "refused";
    let r7_unlink = got["r7_unlink"] == "refused";
    let r7_ok = r7_write && r7_unlink && root.join("sub/build.rs").is_file();
    rows.push(Row {
        claim: "1.2",
        id: "move-protected-parent",
        expected: Verdict::Refused,
        verdict: if r7_ok { Verdict::Refused } else { Verdict::Allowed },
        witness: format!("write-after-move={}, unlink-after-move={}", word(r7_write), word(r7_unlink)),
        detail: got.get("r7_err").cloned().unwrap_or_default(),
        part: None,
        shared_phrase: None,
    });

    // 8. 1.2 | move-in-from-temp
    let r8_write_refused = move_got["r8_write"] == "refused";
    #[cfg(target_os = "macos")]
    let r8_ok = r8_write_refused && flagged_move;
    #[cfg(target_os = "linux")]
    let r8_ok = flagged_move;
    rows.push(Row {
        claim: "1.2",
        id: "move-in-from-temp",
        expected: Verdict::Caught,
        verdict: if r8_ok { Verdict::Caught } else { Verdict::Allowed },
        witness: format!(
            "write-in-root={}, snapshot-flagged={}",
            word(r8_write_refused),
            if flagged_move { "crates/x/build.rs" } else { "none" }
        ),
        detail: move_got.get("r8_err").cloned().unwrap_or_default(),
        part: None,
        shared_phrase: None,
    });

    // 9. 1.3, 1.4 | read-kept
    let r9_ssh = got["r9_ssh"] == "refused";
    let r9_aws = got["r9_aws"] == "refused";
    let r9_env = got["r9_dotenv"] == "refused";
    let r9_sb = got["r9_sandbox"] == "refused";
    let r9_home = got["r9_home"] == "refused";
    let r9_git = got["r9_gitconfig"] == "scrubbed";
    let r9_vars = got["r9_vars"] == "scrubbed";
    let r9_ok = r9_ssh && r9_aws && r9_env && r9_sb && r9_home && r9_git && r9_vars;
    rows.push(Row {
        claim: "1.3, 1.4",
        id: "read-kept",
        expected: Verdict::Refused,
        verdict: if r9_ok { Verdict::Refused } else { Verdict::Allowed },
        witness: format!(
            "ssh-key={}, aws-creds={}, dot-env={}, sandbox-json={}, home-file={}, gitconfig={}, env={}",
            word(r9_ssh),
            word(r9_aws),
            word(r9_env),
            word(r9_sb),
            word(r9_home),
            got["r9_gitconfig"],
            got["r9_vars"]
        ),
        detail: got.get("r9_err").cloned().unwrap_or_default(),
        part: None,
        shared_phrase: None,
    });

    // 10. 1.5 | tcp-unopened-port
    let r10_refused = got["r10_connect"] == "refused" && tcp_unopened_accepted == 0;
    rows.push(Row {
        claim: "1.5",
        id: "tcp-unopened-port",
        expected: Verdict::Refused,
        verdict: if r10_refused { Verdict::Refused } else { Verdict::Shared },
        witness: format!("connect={}, accepted={tcp_unopened_accepted}", word(r10_refused)),
        detail: got.get("r10_err").cloned().unwrap_or_default(),
        part: Some((Part::Loopback, Some(!r10_refused))),
        shared_phrase: None,
    });

    // 11. 1.5 | tcp-own-non-loopback
    let r11_refused = got["r11_connect"] == "refused" && tcp_non_loop_accepted == 0;
    rows.push(Row {
        claim: "1.5",
        id: "tcp-own-non-loopback",
        expected: Verdict::Refused,
        verdict: if r11_refused { Verdict::Refused } else { Verdict::Allowed },
        witness: format!("connect={}, accepted={tcp_non_loop_accepted}", word(r11_refused)),
        detail: got.get("r11_err").cloned().unwrap_or_default(),
        part: None,
        shared_phrase: None,
    });

    // 12. 1.5 | udp-sendto
    #[cfg(target_os = "linux")]
    let is_isolated_net = policy.confine.as_ref().is_some_and(|c| {
        c.as_ref().is_ok_and(|c| {
            matches!(
                c.net,
                crate::sandbox::guarantees::RunNet::Own | crate::sandbox::guarantees::RunNet::Routed
            )
        })
    });
    #[cfg(not(target_os = "linux"))]
    let is_isolated_net = false;
    let r12_loop = (got["r12_loop"] == "refused" || is_isolated_net) && udp_loop_recv == 0;
    let r12_dns = (got["r12_dns"] == "refused" || is_isolated_net) && udp_dns_recv == 0;
    let r12_zero = got["r12_zero"] == "refused";
    let r12_ok = r12_loop && r12_dns && r12_zero;
    rows.push(Row {
        claim: "1.5",
        id: "udp-sendto",
        expected: Verdict::Refused,
        verdict: if r12_ok { Verdict::Refused } else { Verdict::Allowed },
        witness: format!("loopback={}, resolver={}, port0={}", word(r12_loop), word(r12_dns), word(r12_zero)),
        detail: got.get("r12_err").cloned().unwrap_or_default(),
        part: None,
        shared_phrase: None,
    });

    // 13. 1.5 | raw-and-other-sockets (socket creation only, nothing sent)
    let r13_icmp = got["r13_raw_icmp"] == "refused";
    let r13_tcp = got["r13_raw_tcp"] == "refused";
    let r13_dgram = got["r13_dgram_icmp"] == "refused";
    let r13_ok = r13_icmp && r13_tcp && r13_dgram;
    rows.push(Row {
        claim: "1.5",
        id: "raw-and-other-sockets",
        expected: Verdict::Refused,
        verdict: if r13_ok { Verdict::Refused } else { Verdict::Allowed },
        witness: format!(
            "raw-icmp={}, raw-tcp={}, dgram-icmp={}",
            word(r13_icmp),
            word(r13_tcp),
            word(r13_dgram)
        ),
        detail: got.get("r13_err").cloned().unwrap_or_default(),
        part: None,
        shared_phrase: None,
    });

    // 14. 1.5 | proxy-unlisted-and-other-token (a token only macOS's proxy takes: Linux's supervisor knows a run
    // by its socket pair, so there's none to try)
    #[cfg(target_os = "macos")]
    let (r14_ok, r14_witness) = (
        proxy_got["r14_unlisted"] == "403"
            && proxy_got["r14_other_port"] == "refused"
            && proxy_got["r14_other_token"] == "407",
        format!(
            "unlisted-host={}, other-proxy-port={}, other-proxy-token={}",
            proxy_got["r14_unlisted"], proxy_got["r14_other_port"], proxy_got["r14_other_token"]
        ),
    );
    #[cfg(target_os = "linux")]
    let (r14_ok, r14_witness) = (
        proxy_got["r14_unlisted"] == if has_proxy { "403" } else { "refused" }
            && proxy_got["r14_other_port"] == "refused",
        format!(
            "unlisted-host={}, other-proxy-port={}",
            proxy_got["r14_unlisted"], proxy_got["r14_other_port"]
        ),
    );
    rows.push(Row {
        claim: "1.5",
        id: "proxy-unlisted-and-other-token",
        expected: Verdict::Refused,
        verdict: if r14_ok { Verdict::Refused } else { Verdict::Allowed },
        witness: r14_witness,
        detail: ["r14_unlisted_err", "r14_err"]
            .iter()
            .filter_map(|k| proxy_got.get(*k).map(|v| format!("{k}={v}")))
            .collect::<Vec<_>>()
            .join(", "),
        part: None,
        shared_phrase: None,
    });

    // 15. 1.5 | link-local-metadata (rule check only — never attempted)
    let r15_v4 = route::never("169.254.169.254".parse().unwrap());
    let r15_v6 = route::never("fe80::1".parse().unwrap());
    let r15_mcast = route::never("ff02::1".parse().unwrap());
    let r15_gce = route::never_named("metadata.google.internal");
    let r15_ec2 = route::never_named("instance-data.ec2.internal");
    let r15_ok = r15_v4 && r15_v6 && r15_mcast && r15_gce && r15_ec2;
    rows.push(Row {
        claim: "1.5",
        id: "link-local-metadata",
        expected: Verdict::Rule,
        verdict: if r15_ok { Verdict::Rule } else { Verdict::Allowed },
        witness: format!(
            "ipv4-link-local={r15_v4}, ipv6-link-local={r15_v6}, multicast={r15_mcast}, \
             google-metadata={r15_gce}, ec2-metadata={r15_ec2}"
        ),
        detail: String::new(),
        part: None,
        shared_phrase: None,
    });

    // 16. 1.9 | unix-sockets-outside
    let r16_other = got["r16_other_run"] == "refused";
    let r16_state = got["r16_state"] == "refused";
    let r16_out = got["r16_outside"] == "refused";
    let r16_tmp = got["r16_tmp"] == "refused";
    let r16_own_want = if is_offline { "refused" } else { "ok" };
    let r16_own_root = got["r16_own_root"] == r16_own_want;
    let r16_own_run = got["r16_own_run"] == r16_own_want;
    let r16_ok = r16_other && r16_state && r16_out && r16_tmp && r16_own_root && r16_own_run;
    rows.push(Row {
        claim: "1.9",
        id: "unix-sockets-outside",
        expected: Verdict::Refused,
        verdict: if r16_ok { Verdict::Refused } else { Verdict::Allowed },
        witness: format!(
            "other-run={}, state-ui={}, outside={}, shared-tmp={}, own-root={}, own-run={}",
            word(r16_other),
            word(r16_state),
            word(r16_out),
            word(r16_tmp),
            got["r16_own_root"],
            got["r16_own_run"]
        ),
        detail: got.get("r16_err").cloned().unwrap_or_default(),
        part: None,
        shared_phrase: None,
    });

    // 17. 1.6, 1.9 | signal-trace-procargs (direct syscalls only — no lldb)
    let r17_signals =
        ["tarae_kill0", "tarae_cont", "other_kill0", "other_cont"].iter().all(|k| got[*k] == "refused");
    let r17_child = got["own_child"] == "ok";
    #[cfg(target_os = "macos")]
    let r17_procs = [
        "tarae_ptrace",
        "tarae_task",
        "tarae_info",
        "tarae_args",
        "tarae_prio",
        "other_ptrace",
        "other_task",
        "other_info",
        "other_args",
        "other_prio",
    ]
    .iter()
    .all(|k| got[*k] == "refused");
    #[cfg(target_os = "macos")]
    {
        let r17_ok = r17_signals && r17_procs && r17_child;
        rows.push(Row {
            claim: "1.6, 1.9",
            id: "signal-trace-procargs",
            expected: Verdict::Refused,
            verdict: if r17_ok { Verdict::Refused } else { Verdict::Shared },
            witness: format!(
                "signals={}, trace-and-procargs={}, own-child={}",
                word(r17_signals),
                word(r17_procs),
                got["own_child"]
            ),
            detail: got.get("r17_err").cloned().unwrap_or_default(),
            part: Some((Part::Signals, Some(!r17_signals))),
            shared_phrase: None,
        });
    }
    #[cfg(target_os = "linux")]
    let r17_procs =
        ["tarae_cmdline", "tarae_prio", "other_cmdline", "other_prio"].iter().all(|k| got[*k] == "refused");
    #[cfg(target_os = "linux")]
    {
        let r17_trace_env = ["tarae_ptrace", "tarae_environ", "other_ptrace", "other_environ"]
            .iter()
            .all(|k| got[*k] == "refused");
        let kept_procs = g.kept(Part::Processes) == Some(true);
        let kept_sigs = g.kept(Part::Signals) == Some(true);
        let r17_expected = if kept_procs && kept_sigs { Verdict::Refused } else { Verdict::Shared };
        let r17_verdict = if !r17_trace_env || !r17_child {
            Verdict::Allowed
        } else if r17_signals && r17_procs {
            Verdict::Refused
        } else {
            Verdict::Shared
        };
        let r17_phrase = if !kept_procs {
            Some("processes (command lines, priority)")
        } else if !kept_sigs {
            Some("can't keep a command's signals to its own processes")
        } else {
            None
        };
        rows.push(Row {
            claim: "1.6, 1.9",
            id: "signal-trace-procargs",
            expected: r17_expected,
            verdict: r17_verdict,
            witness: format!(
                "signals={}, ptrace-environ={}, cmdline-prio={}, own-child={}",
                if r17_signals { "refused" } else { "shared" },
                word(r17_trace_env),
                if r17_procs { "refused" } else { "shared" },
                got["own_child"]
            ),
            detail: got.get("r17_err").cloned().unwrap_or_default(),
            part: Some((Part::Signals, Some(!r17_signals))),
            shared_phrase: r17_phrase,
        });
    }

    // 18. 1.6 | sysv-and-posix-shm
    let r18_sysv = got["r18_sysv"] == "refused";
    let r18_posix = got["r18_posix"] == "shared";
    let kept_shm = g.kept(Part::Shm) == Some(true);
    let kept_ipc = g.kept(Part::Ipc) == Some(true);
    let r18_expected = if kept_shm && kept_ipc { Verdict::Refused } else { Verdict::Shared };
    let r18_verdict = match (r18_sysv, r18_posix) {
        (true, false) => Verdict::Refused,
        (true, true) if kept_ipc && !kept_shm => Verdict::Shared,
        (false, true) if !kept_ipc && !kept_shm => Verdict::Shared,
        _ => Verdict::Allowed,
    };
    let r18_phrase = if !kept_shm {
        #[cfg(target_os = "macos")]
        {
            Some("shares POSIX shared memory with other sessions' commands")
        }
        #[cfg(target_os = "linux")]
        {
            Some("/dev/shm")
        }
    } else {
        None
    };
    rows.push(Row {
        claim: "1.6",
        id: "sysv-and-posix-shm",
        expected: r18_expected,
        verdict: r18_verdict,
        witness: format!("sysv={}, posix={}", got["r18_sysv"], got["r18_posix"]),
        detail: got.get("r18_err").cloned().unwrap_or_default(),
        part: Some((Part::Shm, Some(r18_posix))),
        shared_phrase: r18_phrase,
    });

    // 19–20. macOS-only rows (`not-here` on Linux)
    #[cfg(target_os = "macos")]
    {
        // 19. 1.4, 1.9 | macos-apple-events-ls (`bootstrap_look_up` only, nothing sent)
        let r19_ls = got["r19_lsd"] == "refused" && got["r19_mapdb"] == "refused";
        let r19_ae = got["r19_ae"] == "refused";
        let r19_sec = got["r19_sec"] == "refused";
        let r19_pb = got["r19_pb"] == "refused";
        let r19_ctrl = got["r19_ctrl"] == "ok";
        let r19_ok = r19_ls && r19_ae && r19_sec && r19_pb && r19_ctrl;
        rows.push(Row {
            claim: "1.4, 1.9",
            id: "macos-apple-events-ls",
            expected: Verdict::Refused,
            verdict: if r19_ok { Verdict::Refused } else { Verdict::Allowed },
            witness: format!(
                "launchservices={}, appleevents={}, keychain={}, pasteboard={}, opendirectory-control={}",
                word(r19_ls),
                word(r19_ae),
                word(r19_sec),
                word(r19_pb),
                got["r19_ctrl"]
            ),
            detail: got.get("r19_err").cloned().unwrap_or_default(),
            part: None,
            shared_phrase: None,
        });

        // 20. 1.1 | macos-nested-sandbox-exec
        let r20_ok = got["r20_nested"] == "refused" && !outside.join("nested").exists();
        rows.push(Row {
            claim: "1.1",
            id: "macos-nested-sandbox-exec",
            expected: Verdict::Refused,
            verdict: if r20_ok { Verdict::Refused } else { Verdict::Allowed },
            witness: format!("sandbox-exec={}", word(r20_ok)),
            detail: got.get("r20_err").cloned().unwrap_or_default(),
            part: None,
            shared_phrase: None,
        });

        // 21–26. Linux-only rows (`not-here` on macOS)
        for (claim, id) in [
            ("1.4", "linux-key-mount-and-link"),
            ("1.9", "linux-userns-and-mount"),
            ("1.9", "linux-nested-seccomp-listener"),
            ("1.9", "linux-dangerous-syscalls"),
            ("1.6, 1.9", "linux-keeper-and-proc"),
            ("1.6, 1.9", "linux-tiocsti-and-ptys"),
        ] {
            rows.push(Row {
                claim,
                id,
                expected: Verdict::NotHere,
                verdict: Verdict::NotHere,
                witness: "linux-only".into(),
                detail: String::new(),
                part: None,
                shared_phrase: None,
            });
        }
    }

    #[cfg(target_os = "linux")]
    {
        for (claim, id) in [("1.4, 1.9", "macos-apple-events-ls"), ("1.1", "macos-nested-sandbox-exec")] {
            rows.push(Row {
                claim,
                id,
                expected: Verdict::NotHere,
                verdict: Verdict::NotHere,
                witness: "macos-only".into(),
                detail: String::new(),
                part: None,
                shared_phrase: None,
            });
        }

        // Rows 21, 22: each part the run's answer against the baseline's (`part`); the detail has both, errnos and all
        let ns_part = |n: &str| part(base_linux.ns[n], probe_said(&got, &format!("ns_{n}")));
        let ns_detail = |run: &HashMap<String, String>, prefix: &str, names: &[&str]| {
            let each = names.iter().map(|n| {
                format!(
                    "{n}: {} vs {}",
                    probe_said(run, &format!("{prefix}_{n}")).said(),
                    base_linux.ns[n].said()
                )
            });
            each.collect::<Vec<_>>().join(", ")
        };
        let witness =
            |parts: &[(&str, &str)]| parts.iter().map(|(w, p)| format!("{w}={p}")).collect::<Vec<_>>();

        // 21. 1.4 | linux-key-mount-and-link — the links are always the baseline's to make and read, so refused
        // is their only answer; a mount the baseline couldn't make either is the system's
        let r21 = [
            ("hardlink", "hardlink"),
            ("symlink-read", "symlink"),
            ("bind-mount", "bind"),
            ("open-tree", "open_tree"),
        ]
        .map(|(w, n)| (w, ns_part(n)));
        let r21_ok = r21[..2].iter().all(|(_, p)| *p == "refused")
            && r21[2..].iter().all(|(_, p)| matches!(*p, "refused" | "system" | "no-userns"));
        rows.push(Row {
            claim: "1.4",
            id: "linux-key-mount-and-link",
            expected: Verdict::Refused,
            verdict: if r21_ok { Verdict::Refused } else { Verdict::Allowed },
            witness: witness(&r21).join(", "),
            detail: ns_detail(&got, "ns", &["hardlink", "symlink", "bind", "open_tree"]),
            part: None,
            shared_phrase: None,
        });

        // 22. 1.9 | linux-userns-and-mount — user namespaces as this keeper's runs get them (made where tarae lets
        // them: `no_userns` off), a run that refuses them (`refusing_userns_run`), and mounts refused in a
        // namespace a run made. Where this system lets the baseline make no user namespace, every part is the
        // system's: the row is `system`, it shows nothing of tarae's.
        let no_userns = policy.confine.as_ref().and_then(|c| c.as_ref().ok()).is_some_and(|c| c.no_userns);
        let userns = ["unshare", "clone", "clone3", "setns"];
        let run_ns = userns.map(|n| (n, ns_part(n)));
        let mounts = [("mount+umount", ns_part("mount")), ("pivot-root", ns_part("pivot"))];
        let refusing =
            userns.map(|n| (n, part(base_linux.ns[n], probe_said(&refusing_got, &format!("nsu_{n}")))));
        let mut want_refused: Vec<&str> = mounts.iter().chain(&refusing).map(|(_, p)| *p).collect();
        let mut want_allowed = Vec::new();
        match no_userns {
            true => want_refused.extend(run_ns.map(|(_, p)| p)),
            false => want_allowed.extend(run_ns.map(|(_, p)| p)),
        }
        // (a part not tried shows nothing either way: the row fails, never reads it as tarae's refusal)
        let not_tried = want_refused.iter().chain(&want_allowed).any(|p| *p == "not-tried");
        let r22_verdict =
            if not_tried || want_refused.contains(&"allowed") || want_allowed.contains(&"refused") {
                Verdict::Allowed
            } else if want_refused.contains(&"refused") {
                Verdict::Refused
            } else {
                Verdict::System
            };
        let r22_expected = match userns.iter().any(|n| base_linux.ns[n].through()) {
            true => Verdict::Refused,
            false => Verdict::System,
        };
        let mut r22_witness = witness(&run_ns);
        r22_witness.extend(witness(&mounts));
        r22_witness.extend(refusing.iter().map(|(n, p)| format!("no-userns-run.{n}={p}")));
        rows.push(Row {
            claim: "1.9",
            id: "linux-userns-and-mount",
            expected: r22_expected,
            verdict: r22_verdict,
            witness: r22_witness.join(", "),
            detail: format!(
                "{}; no-userns run: {}",
                ns_detail(&got, "ns", &["unshare", "clone", "clone3", "setns", "mount", "pivot"]),
                ns_detail(&refusing_got, "nsu", &userns)
            ),
            part: None,
            shared_phrase: None,
        });

        let sys_run = |n: &str| probe_said(&got, &format!("sys_{n}"));
        let sys_detail = |names: &[&str]| {
            let each =
                names.iter().map(|n| format!("{n}: {} vs {}", sys_run(n).said(), base_linux.sys[n].said()));
            each.collect::<Vec<_>>().join(", ")
        };

        // 23. 1.9 | linux-nested-seccomp-listener — served, tarae's supervisor holds the chain's one listener: a
        // run's own is refused (EBUSY), and the precedence probe never starts. Offline a run may have its own (as
        // srt, Docker and Flatpak let it), and tarae's refusal still wins over what that listener's filter says
        // (`precedence`: tarae's EPERM where the baseline got the closed listener's ENOSYS). Plain filters stay
        // allowed. Where the baseline can't get a listener either, nothing here shows tarae's: `system`.
        let (base_listener, base_precedence) = (base_linux.sys["new_listener"], base_linux.sys["precedence"]);
        let listener = part(base_listener, sys_run("new_listener"));
        let precedence = match (base_precedence, sys_run("precedence")) {
            (_, Probe::NotTried) => "not-tried",
            (_, r) if r.through() => "allowed",
            (_, Probe::Refused(libc::ENOSYS)) => "listener-won",
            (Probe::Refused(libc::ENOSYS), Probe::Refused(libc::EPERM)) => "refused",
            (Probe::Refused(libc::ENOSYS), _) => "other",
            _ => "system",
        };
        let plain = part(base_linux.sys["plain_filter"], sys_run("plain_filter"));
        let r23_shows = base_listener.through() && base_precedence == Probe::Refused(libc::ENOSYS);
        let r23_ok = plain == "allowed"
            && match is_offline {
                false => listener == "refused" && precedence == "not-tried",
                true => listener == "allowed" && precedence == "refused",
            };
        rows.push(Row {
            claim: "1.9",
            id: "linux-nested-seccomp-listener",
            expected: if r23_shows { Verdict::Refused } else { Verdict::System },
            verdict: match (r23_shows, r23_ok) {
                (false, _) => Verdict::System,
                (true, true) => Verdict::Refused,
                (true, false) => Verdict::Allowed,
            },
            witness: format!("new-listener={listener}, precedence={precedence}, plain-filter={plain}"),
            detail: sys_detail(&["new_listener", "precedence", "plain_filter"]),
            part: None,
            shared_phrase: None,
        });

        // 24. 1.9 | linux-dangerous-syscalls — io_uring and another ABI's calls answered ENOSYS (tarae's
        // `Missing`, nothing else's), no capabilities and no-new-privs set where the baseline had them, packet
        // and socket-diagnostics sockets refused; a part the baseline was refused too is the system's. (Another
        // ABI only on x86-64: elsewhere it isn't tried, and the witness says so.)
        let mut r24 = vec![
            ("io-uring", missing_part(base_linux.sys["io_uring"], sys_run("io_uring"))),
            ("caps", part(base_linux.sys["caps"], sys_run("caps"))),
            ("no-new-privs", part(base_linux.sys["nnp_off"], sys_run("nnp_off"))),
            ("packet", part(base_linux.sys["packet"], sys_run("packet"))),
            ("netlink-diag", part(base_linux.sys["netlink_diag"], sys_run("netlink_diag"))),
        ];
        #[cfg(target_arch = "x86_64")]
        r24.push(("foreign-arch", missing_part(base_linux.sys["foreign_arch"], sys_run("foreign_arch"))));
        #[cfg(not(target_arch = "x86_64"))]
        r24.push(("foreign-arch", "not-here"));
        let r24_names = ["io_uring", "caps", "nnp_off", "packet", "netlink_diag"];
        let r24_bad = r24.iter().any(|(_, p)| matches!(*p, "allowed" | "other"));
        let r24_shows = base_linux
            .sys
            .iter()
            .any(|(n, b)| (r24_names.contains(n) || *n == "foreign_arch") && b.through());
        rows.push(Row {
            claim: "1.9",
            id: "linux-dangerous-syscalls",
            expected: if r24_shows { Verdict::Refused } else { Verdict::System },
            verdict: match (r24_bad, r24.iter().any(|(_, p)| *p == "refused")) {
                (true, _) => Verdict::Allowed,
                (false, true) => Verdict::Refused,
                (false, false) => Verdict::System,
            },
            witness: witness(&r24).join(", "),
            detail: sys_detail(&r24_names),
            part: None,
            shared_phrase: None,
        });

        // 25. 1.6, 1.9 | linux-keeper-and-proc — a run can't signal its keeper (Landlock's scope from ABI 6:
        // EPERM), trace it or take its descriptors, nor open its `/proc` files (it isn't dumpable). Inside the
        // session's PID namespace below ABI 6 the keeper is the run's PID 1, reached by `kill(1, 0)` (`as-init`):
        // what keeps it there is that PID 1 drops the signals it has no handler for — not sent here, never a
        // signal that ends or stops: `keeper::tests::a_command_cant_signal_the_keeper` sends them. Below ABI 6
        // without one, a run can signal it (shared; its runs fail closed). Each part against the baseline's
        // at the test's own process, with no capabilities as a run has none: where the system (Yama, Docker's
        // profile) refuses it there too, it's `system` (`keeper_row`).
        let c = policy.confine.as_ref().and_then(|c| c.as_ref().ok()).expect("a confined policy");
        let pidns = c.keeper.as_ref().expect("a keeper's policy").ns.pid;
        let r25_parent = got["r25_parent_is_keeper"] == "true";
        let r25_run: HashMap<&str, Probe> = std::iter::once("signals")
            .chain(KEEPER_PROBES)
            .map(|n| (n, probe_said(&got, &format!("r25_{n}"))))
            .collect();
        let (r25_expected, r25_verdict, r25_witness) =
            keeper_row(r25_parent, pidns, c.abi, &base_linux.keeper, &r25_run);
        let r25_detail = std::iter::once(format!("signals: {}", r25_run["signals"].said())).chain(
            KEEPER_PROBES.map(|n| format!("{n}: {} vs {}", r25_run[n].said(), base_linux.keeper[n].said())),
        );
        rows.push(Row {
            claim: "1.6, 1.9",
            id: "linux-keeper-and-proc",
            expected: r25_expected,
            verdict: r25_verdict,
            witness: r25_witness,
            detail: r25_detail.collect::<Vec<_>>().join(", "),
            part: None,
            shared_phrase: (r25_expected == Verdict::Shared)
                .then_some("can't keep a command's signals to its own processes"),
        });

        // 26. 1.6, 1.9 | linux-tiocsti-and-ptys — no controlling terminal in a run (tarae's `setsid`: the
        // baseline has the one `must_be_refused` gave it, so none in the run is tarae's doing — ENXIO), and
        // another session's pty refused where ptys are kept apart. A terminal the baseline lacks shows nothing of
        // tarae's (`system`): the setup failed, and so does the row.
        let ctty = match (base_linux.ctty, probe_said(&got, "r26_ctty")) {
            (_, r) if r.through() => "allowed",
            (b, _) if !b.through() => "system",
            (_, Probe::Refused(libc::ENXIO)) => "refused",
            _ => "other",
        };
        let other_pty = match part(base_linux.other_pty, probe_said(&got, "r26_other_pty")) {
            "allowed" => "shared",
            p => p,
        };
        let kept_ptys = g.kept(Part::Ptys) == Some(true);
        let r26_verdict = match (ctty, other_pty) {
            ("allowed" | "other", _) => Verdict::Allowed,
            ("system", _) | (_, "system") => Verdict::System,
            (_, "shared") => Verdict::Shared,
            _ => Verdict::Refused,
        };
        rows.push(Row {
            claim: "1.6, 1.9",
            id: "linux-tiocsti-and-ptys",
            expected: if kept_ptys { Verdict::Refused } else { Verdict::Shared },
            verdict: r26_verdict,
            witness: format!("ctty={ctty}, other-pty={other_pty}"),
            detail: format!(
                "ctty: {} vs {}, other-pty: {} vs {}",
                got["r26_ctty"],
                base_linux.ctty.said(),
                got["r26_other_pty"],
                base_linux.other_pty.said()
            ),
            part: (other_pty != "system").then_some((Part::Ptys, Some(other_pty == "shared"))),
            shared_phrase: (!kept_ptys).then_some("terminals (/dev/pts)"),
        });
    }

    // Property 1: every row's verdict matches its expected verdict, and no fake token leaked
    let mut wrong = Vec::new();
    if main_leaked || move_leaked || proxy_leaked {
        wrong.push(format!(
            "fake token leaked into helper output (main={main_leaked}, move={move_leaked}, proxy={proxy_leaked})"
        ));
    }
    for r in &rows {
        if r.verdict != r.expected {
            wrong.push(format!(
                "{} ({}): got {}, expected {} — witness: {} ({})",
                r.id,
                r.claim,
                r.verdict.as_str(),
                r.expected.as_str(),
                r.witness,
                r.detail
            ));
        }
    }

    // Property 2: `policy.guarantees()` and `Guard::In(policy).report(&[])` match what was found
    let mut found_parts: Vec<(Part, Option<bool>)> = rows.iter().filter_map(|r| r.part).collect();
    found_parts.push((Part::Processes, Some(!r17_procs)));
    found_parts.push((Part::Ipc, Some(!r18_sysv)));
    wrong.extend(said_as_found(&g, &found_parts));

    let report = Guard::In(Box::new(policy.clone())).report(&[]);
    let flat_report = report.split_whitespace().collect::<Vec<_>>().join(" ");
    for r in &rows {
        if r.verdict == Verdict::Shared
            && let Some(phrase) = r.shared_phrase
            && !flat_report.contains(phrase)
        {
            wrong.push(format!("{}: shared, but report did not contain {phrase:?}:\n{report}", r.id));
        }
    }

    // Format deterministic snapshot table
    #[cfg(target_os = "macos")]
    let (suite_label, snap_name) = ("macOS Seatbelt".to_string(), "refused_macos".to_string());
    #[cfg(target_os = "linux")]
    let (suite_label, snap_name) = {
        let tier = policy.tier_said().unwrap_or_default();
        assert!(!tier.is_empty(), "the run's policy says no tier: no keeper's");
        let c = policy.confine.as_ref().and_then(|c| c.as_ref().ok()).expect("a confined policy");
        let yama = fs::read_to_string("/proc/sys/kernel/yama/ptrace_scope").ok();
        let machine = Machine {
            tier: &tier,
            ns: c.keeper.as_ref().is_some_and(|k| k.ns.user),
            arch: std::env::consts::ARCH,
            abi: c.abi,
            yama: yama.and_then(|s| s.trim().parse().ok()),
        };
        (format!("Linux {tier}"), snapshot_for(&machine))
    };
    let mut table = format!(
        "# must-be-refused suite ({suite_label})\n\
         # claim | id | verdict | witness\n",
    );
    for r in &rows {
        table.push_str(&format!(
            "{:<8} | {:<30} | {:<8} | {}\n",
            r.claim,
            r.id,
            r.verdict.as_str(),
            r.witness
        ));
    }
    assert!(wrong.is_empty(), "must-be-refused failures:\n{}\n\n{table}", wrong.join("\n"));
    #[cfg(target_os = "macos")]
    compare_snapshot(&snap_name, &table);
    #[cfg(target_os = "linux")]
    match snap_name {
        Ok(name) => compare_snapshot(&name, &table),
        // (the properties above held; `must_be_refused` says it past libtest's capture)
        Err(why) => {
            let line = format!("{UNCOMPARED}{why}\n");
            let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
        }
    }
}

/// The start of the line saying a Linux table wasn't held against a snapshot (`snapshot_for`), forwarded by
/// `must_be_refused`.
const UNCOMPARED: &str = "tarae-test: must-be-refused snapshot not compared — ";

/// What a Linux table depends on besides tarae's code (`snapshot_for`).
#[derive(Clone, Copy)]
struct Machine<'a> {
    /// `Policy::tier_said`: own, host or offline, ` partial` below Landlock ABI 3.
    tier: &'a str,
    /// The keeper has the session's namespaces (offline has them where user namespaces work).
    ns: bool,
    arch: &'a str,
    /// The Landlock ABI.
    abi: u32,
    /// Yama's `ptrace_scope`, None without Yama.
    yama: Option<u32>,
}

/// The snapshot a Linux table is held against, or why none is: the snapshots are one machine's words, the test
/// VM's — x86-64 (row 24 tries another ABI's calls only there), Landlock ABI 6 or more (rows 17 and 25: a run's
/// signals scoped), Yama's `ptrace_scope` 1 (row 25's ptrace and `pidfd_getfd`, against a baseline Yama refuses
/// or not) — keyed by tier, offline with namespaces apart (it has a network of its own). Elsewhere the rows'
/// properties still hold, and the table is said, not compared.
fn snapshot_for(m: &Machine) -> Result<String, String> {
    if m.tier.ends_with("partial") {
        return Err(format!("Landlock ABI {}: partial", m.abi));
    }
    if m.arch != "x86_64" {
        return Err(format!("{}: the snapshots are x86-64's", m.arch));
    }
    if m.abi < 6 {
        return Err(format!("Landlock ABI {}: the snapshots are ABI 6's or later", m.abi));
    }
    if m.yama != Some(1) {
        let had = m.yama.map_or("no Yama".to_string(), |s| format!("Yama's ptrace_scope {s}"));
        return Err(format!("{had}: the snapshots are ptrace_scope 1's"));
    }
    let ns = if m.tier == "offline" && m.ns { "-ns" } else { "" };
    Ok(format!("refused_linux_{}{ns}", m.tier))
}

#[test]
fn a_linux_table_is_held_against_a_snapshot_only_where_its_words_are_the_snapshots() {
    let vm = Machine { tier: "host", ns: false, arch: "x86_64", abi: 7, yama: Some(1) };
    assert_eq!(snapshot_for(&vm).as_deref(), Ok("refused_linux_host"));
    let own = Machine { tier: "own", ns: true, ..vm };
    assert_eq!(snapshot_for(&own).as_deref(), Ok("refused_linux_own"));
    // (offline with namespaces has its own network, other calls refused in other ways: another table)
    for (ns, name) in [(false, "refused_linux_offline"), (true, "refused_linux_offline-ns")] {
        let offline = Machine { tier: "offline", ns, ..vm };
        assert_eq!(snapshot_for(&offline).as_deref(), Ok(name));
    }
    // Each of what the words depend on, off the VM's alone: no snapshot, and the why names it
    let off = [
        (Machine { tier: "host partial", abi: 2, ..vm }, "partial"),
        (Machine { arch: "aarch64", ..vm }, "aarch64"),
        (Machine { abi: 5, ..vm }, "ABI 5"),
        (Machine { yama: Some(0), ..vm }, "ptrace_scope 0"),
        (Machine { yama: None, ..vm }, "no Yama"),
    ];
    for (m, why) in off {
        let got = snapshot_for(&m);
        assert!(got.as_ref().is_err_and(|e| e.contains(why)), "{} {}: {got:?}", m.arch, m.abi);
    }
}

fn compare_snapshot(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/snapshots").join(format!("{name}.txt"));
    if std::env::var_os("TARAE_BLESS").is_some_and(|v| v == "1") {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, actual).unwrap();
        return;
    }
    let Ok(expected) = fs::read_to_string(&path) else {
        panic!(
            "no snapshot {} yet — create it with `TARAE_BLESS=1 cargo test sandbox::refused`\n\n{actual}",
            path.display()
        );
    };
    let expected = expected.replace("\r\n", "\n");
    if expected != actual {
        // (the lines that differ, then the whole table: a run in a container's clone blesses nothing here)
        let (was, now): (Vec<_>, Vec<_>) = (expected.lines().collect(), actual.lines().collect());
        let changed: String = (0..was.len().max(now.len()))
            .filter(|&i| was.get(i) != now.get(i))
            .map(|i| format!("- {}\n+ {}\n", was.get(i).unwrap_or(&""), now.get(i).unwrap_or(&"")))
            .collect();
        panic!(
            "snapshot {name} differs ({}) — if intended: `TARAE_BLESS=1 cargo test sandbox::refused`\n\
             {changed}\nnow:\n{actual}",
            path.display()
        );
    }
}

/// Row 25's `keeper_probes`, in their order: ptrace (seize), pidfd_getfd, then the keeper's `/proc` files.
const KEEPER_PROBES: [&str; 6] = ["ptrace", "pidfd", "mem", "environ", "fd", "root"];
/// Row 25's `/proc/<keeper>` files: `mem` and `environ` opened, `fd/0` read as a link, `root` followed.
const KEEPER_PROC: [&str; 4] = ["mem", "environ", "fd", "root"];

/// Row 25 judged — (expected, verdict, witness) — from whether the run's parent was proven the keeper
/// (`keeper_proven`; not proven, nothing was tried and the row fails), whether the keeper is the run's PID 1
/// (`pidns`), Landlock's `abi`, the run's `signals` (`kill(keeper, 0)`) and both sides' `KEEPER_PROBES` (the
/// baseline's at the test's own process). A part the baseline was refused too is the system's (`system`); a
/// `/proc` file's refusal is tarae's only as the kernel's access check gives it (EACCES, or an LSM's EPERM).
fn keeper_row(
    parent: bool,
    pidns: bool,
    abi: u32,
    base: &HashMap<&str, Probe>,
    run: &HashMap<&str, Probe>,
) -> (Verdict, Verdict, String) {
    let sig = match run["signals"] {
        _ if !parent => "not-tried",
        Probe::Refused(libc::EPERM) => "refused",
        r if r.through() && pidns && abi < 6 => "as-init",
        r if r.through() => "allowed",
        _ => "other",
    };
    let (ptrace, pidfd) = (part(base["ptrace"], run["ptrace"]), part(base["pidfd"], run["pidfd"]));
    let file = |f: &str| match (base[f], run[f]) {
        (_, r) if r.through() => "allowed",
        (_, Probe::NotTried) => "not-tried",
        (b, _) if !b.through() => "system",
        (_, Probe::Refused(libc::EACCES | libc::EPERM)) => "refused",
        _ => "other",
    };
    let proc = match KEEPER_PROC.map(file) {
        _ if !parent => "not-tried",
        f if f.contains(&"allowed") => "allowed",
        f if f.iter().any(|p| !matches!(*p, "refused" | "system")) => "other",
        f if f.contains(&"refused") => "refused",
        _ => "system",
    };
    let expected = if pidns || abi >= 6 { Verdict::Refused } else { Verdict::Shared };
    let verdict = if !parent
        || sig == "other"
        || [ptrace, pidfd, proc].iter().any(|p| matches!(*p, "allowed" | "other" | "not-tried"))
    {
        Verdict::Allowed
    } else if sig == "allowed" {
        Verdict::Shared
    } else {
        Verdict::Refused
    };
    let witness = format!(
        "parent-is-keeper={parent}, signals={sig}, ptrace={ptrace}, proc-files={proc}, pidfd-getfd={pidfd}"
    );
    (expected, verdict, witness)
}

/// The keeper's place from its `/proc/<pid>/status`, as the test reads it: `Some(true)` the PID 1 of a PID
/// namespace below the reader's (its `NSpid` ends with 1), `Some(false)` in the reader's own (one number),
/// `None` neither — another namespace's other process, or no `NSpid`: not a keeper a run sees as its parent.
fn keeper_in_pidns(status: &str) -> Option<bool> {
    let line = status.lines().find_map(|l| l.strip_prefix("NSpid:"))?;
    match line.split_whitespace().collect::<Vec<_>>().as_slice() {
        [_] => Some(false),
        [_, .., "1"] => Some(true),
        _ => None,
    }
}

/// Whether a run's parent `ppid` is the keeper proven the test's own: the number its runs see (`keeper`,
/// `keeper::seen_by_runs`), and with the session's PID namespace (`keeper_pidns`: the keeper's own, read from
/// the keeper by the test where `keeper_in_pidns` says it's that namespace's PID 1) the run in that very
/// namespace (`own_pidns`: its `/proc/self/ns/pid`). PID 1 is never taken by its number: in tier host the
/// test is root and PID 1 is the container's init. Not proven: nothing is tried at any process.
fn keeper_proven(ppid: i32, keeper: i32, keeper_pidns: Option<&str>, own_pidns: Option<&str>) -> bool {
    keeper > 0
        && ppid == keeper
        && match keeper_pidns {
            Some(ns) => ppid == 1 && own_pidns == Some(ns),
            None => ppid > 1,
        }
}

/// Row 25's every branch, here because the machines the suite runs on (Landlock ABI ≥ 6) reach only
/// `signals=refused`: `as-init` (the session's PID namespace) and `shared` (none) are ABI 5's and below.
#[test]
fn the_keeper_row_holds_what_each_machine_keeps() {
    let (eperm, eacces) = (Probe::Refused(libc::EPERM), Probe::Refused(libc::EACCES));
    let probes = |sig: Probe, each: Probe| -> HashMap<&'static str, Probe> {
        std::iter::once(("signals", sig)).chain(KEEPER_PROBES.map(|n| (n, each))).collect()
    };
    let base = probes(Probe::NotTried, Probe::Done);
    let with = |mut m: HashMap<&'static str, Probe>, parts: &[(&'static str, Probe)]| {
        m.extend(parts.iter().copied());
        m
    };
    let row = |parent, pidns, abi, base: &HashMap<_, _>, run: &HashMap<_, _>| {
        let (expected, verdict, witness) = keeper_row(parent, pidns, abi, base, run);
        (expected, verdict, witness)
    };
    let refused = probes(eperm, eacces);
    let through = probes(Probe::Done, eacces);

    // ABI 6: Landlock's signal scope, in the session's PID namespace or not
    for pidns in [false, true] {
        let (e, v, w) = row(true, pidns, 6, &base, &refused);
        assert_eq!((e, v), (Verdict::Refused, Verdict::Refused), "{w}");
        assert_eq!(
            w,
            "parent-is-keeper=true, signals=refused, ptrace=refused, proc-files=refused, pidfd-getfd=refused"
        );
    }
    // Below ABI 6 in the PID namespace `kill(1, 0)` gets through: the keeper is the run's PID 1 (as-init)
    let (e, v, w) = row(true, true, 5, &base, &through);
    assert_eq!((e, v), (Verdict::Refused, Verdict::Refused), "{w}");
    assert!(w.contains("signals=as-init"), "{w}");
    // ... without one a run can signal it: shared, and shared is what's expected there
    let (e, v, w) = row(true, false, 5, &base, &through);
    assert_eq!((e, v), (Verdict::Shared, Verdict::Shared), "{w}");
    assert!(w.contains("signals=allowed"), "{w}");
    // From ABI 6 a signal that gets through fails the row, with or without the PID namespace
    for pidns in [false, true] {
        let (e, v, w) = row(true, pidns, 6, &base, &through);
        assert_eq!((e, v), (Verdict::Refused, Verdict::Shared), "{w}");
    }

    // The system refusing the baseline too (Yama's scope 1 for ptrace, pidfd_getfd and `mem`): those parts
    // are `system`, the files the baseline opened still tarae's
    let yama = with(base.clone(), &[("ptrace", eperm), ("pidfd", eperm), ("mem", eacces)]);
    let (e, v, w) = row(true, false, 6, &yama, &refused);
    assert_eq!((e, v), (Verdict::Refused, Verdict::Refused), "{w}");
    assert!(w.contains("ptrace=system, proc-files=refused, pidfd-getfd=system"), "{w}");
    let none_opened = with(yama, &[("environ", eacces), ("fd", eacces), ("root", eacces)]);
    assert!(row(true, false, 6, &none_opened, &refused).2.contains("proc-files=system"));
    // ... but a run that got through is never the system's
    let traced = with(refused.clone(), &[("ptrace", Probe::Done)]);
    assert_eq!(row(true, false, 6, &none_opened, &traced).1, Verdict::Allowed);

    // A file opened, or refused by anything but the access check: the row fails
    for got in [Probe::Done, Probe::Refused(libc::ENOENT), Probe::NotTried] {
        let run = with(refused.clone(), &[("environ", got)]);
        let (_, v, w) = row(true, false, 6, &base, &run);
        assert_eq!(v, Verdict::Allowed, "environ {got:?}: {w}");
    }
    // A step before ptrace or pidfd_getfd failed in the run: not tried, never a refusal
    for n in ["ptrace", "pidfd"] {
        let run = with(refused.clone(), &[(n, Probe::NotTried)]);
        let (_, v, w) = row(true, false, 6, &base, &run);
        assert_eq!(v, Verdict::Allowed, "{w}");
        assert!(w.contains("=not-tried"), "{w}");
    }
    // The parent not proven the keeper: nothing tried, the row fails
    let untried = probes(Probe::NotTried, Probe::NotTried);
    let (_, v, w) = row(false, true, 6, &base, &untried);
    assert_eq!(v, Verdict::Allowed, "{w}");
    assert_eq!(
        w,
        "parent-is-keeper=false, signals=not-tried, ptrace=not-tried, proc-files=not-tried, pidfd-getfd=not-tried"
    );
}

/// A run whose call was never made (a step before it failed: row 22's `setns` with no namespace file to
/// open) says `not-tried` against any baseline — never `refused`, nor the system's.
#[test]
fn a_part_not_tried_is_never_a_refusal() {
    for base in [Probe::Done, Probe::Reached, Probe::Refused(libc::EPERM), Probe::NotTried] {
        assert_eq!(part(base, Probe::NotTried), "not-tried", "against {base:?}");
    }
    assert_eq!(part(Probe::Reached, Probe::Refused(libc::EPERM)), "refused");
    assert_eq!(part(Probe::Refused(libc::EPERM), Probe::Refused(libc::EPERM)), "system");
}

/// The keeper a run's probes go to is proven by its PID namespace, never by the number 1: a run in another
/// namespace (the container's, whose PID 1 is its init) or one that couldn't read its own tries nothing.
#[test]
fn the_keeper_is_proven_by_its_namespace_never_by_pid_1_alone() {
    assert_eq!(keeper_in_pidns("Name:\tkeeper\nNSpid:\t4242\t1\nNSpgid:\t4242\t1\n"), Some(true));
    assert_eq!(keeper_in_pidns("NSpid:\t4242\t9\t1\n"), Some(true), "a namespace two below");
    assert_eq!(keeper_in_pidns("NSpid:\t4242\n"), Some(false));
    assert_eq!(keeper_in_pidns("NSpid:\t4242\t7\n"), None, "process 7 of another namespace");
    assert_eq!(keeper_in_pidns("Name:\tkeeper\n"), None, "no NSpid");

    let (keepers, others) = (Some("pid:[4026532001]"), Some("pid:[4026531836]"));
    assert!(keeper_proven(1, 1, keepers, keepers));
    assert!(!keeper_proven(1, 1, keepers, others), "PID 1 of another namespace");
    assert!(!keeper_proven(1, 1, keepers, None), "the run couldn't read its own namespace");
    assert!(!keeper_proven(1, 1, None, None), "PID 1 with no namespace of the keeper's to prove it");
    assert!(!keeper_proven(4242, 4242, keepers, keepers), "the keeper's outer number inside its namespace");
    assert!(keeper_proven(4242, 4242, None, None));
    assert!(!keeper_proven(4243, 4242, None, None), "another parent");
    assert!(!keeper_proven(0, 0, None, None), "no keeper");
}

fn word(refused: bool) -> &'static str {
    if refused { "refused" } else { "allowed" }
}

fn write_file(path: &Path, text: &str) {
    if let Some(p) = path.parent() {
        fs::create_dir_all(p).unwrap();
    }
    fs::write(path, text).unwrap();
}

fn init_git(dir: &Path) {
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false"])
            .args(["-c", "core.hooksPath=/dev/null", "-c", "init.defaultBranch=main"])
            .args(args)
            .current_dir(dir)
            .env("GIT_DIR", dir.join(".git"))
            .env("GIT_WORK_TREE", dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);
}

struct StandInGuard {
    child: std::process::Child,
    pid: u32,
}

impl Drop for StandInGuard {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

struct SandboxedStandInGuard {
    _spawned: crate::spawn::Spawned,
    pid: u32,
    policy: Policy,
}

impl StandInGuard {
    fn running(&self) -> bool {
        running(self.child.stdout.as_ref().unwrap())
    }
}

impl SandboxedStandInGuard {
    fn running(&self) -> bool {
        running(self._spawned.stdout.as_ref().unwrap())
    }
}

/// Whether a stand-in still runs: its stdout, held by it alone (`exec sleep`), isn't at its end — nothing
/// signalled to ask.
fn running(out: &impl std::os::fd::AsRawFd) -> bool {
    let mut p = libc::pollfd { fd: out.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    // SAFETY: one pollfd of ours, its descriptor open for the call; no wait (timeout 0).
    unsafe { libc::poll(&mut p, 1, 0) == 0 }
}

impl Drop for SandboxedStandInGuard {
    fn drop(&mut self) {
        // On Linux a run is its keeper's to signal, as `dap::term_group` does: the keeper reaps it, so its pid
        // could be another process's by now — and a false (not a keeper's run any more) means it's gone:
        // nothing is signalled. On macOS it's this test's own child, unreaped until `wait` below.
        #[cfg(target_os = "linux")]
        let _ = super::keeper::signal(self.pid, libc::SIGKILL);
        // SAFETY: the test's own child, not reaped yet (`spawn::wait` below): the pid is still its.
        #[cfg(not(target_os = "linux"))]
        unsafe {
            libc::kill(self.pid as libc::pid_t, libc::SIGKILL)
        };
        crate::spawn::wait(self.pid).ok();
        after_run(Some(&self.policy))();
        #[cfg(target_os = "linux")]
        end_leftovers(Some(&self.policy.root));
    }
}

struct RunCleanupGuard {
    policy: Policy,
}

impl Drop for RunCleanupGuard {
    fn drop(&mut self) {
        after_run(Some(&self.policy))();
        #[cfg(target_os = "linux")]
        end_leftovers(Some(&self.policy.root));
    }
}

struct ShmGuard {
    posix_fd: libc::c_int,
    posix_name: CString,
    sysv_id: libc::c_int,
}

impl ShmGuard {
    fn new(name: &str, sysv_key: libc::key_t) -> Self {
        let posix_name = CString::new(name).unwrap();
        // SAFETY: a new POSIX shm object (`O_EXCL`: never one that's there, another's), named by a C string of
        // ours; removed on Drop only when made here.
        let posix_fd = unsafe {
            libc::shm_open(
                posix_name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                0o600 as libc::c_uint,
            )
        };
        let posix_err = io::Error::last_os_error();
        // SAFETY: a new SysV segment (`IPC_EXCL`: never one that's there); removed on Drop only when made here.
        let sysv_id = unsafe { libc::shmget(sysv_key, 4096, libc::IPC_CREAT | libc::IPC_EXCL | 0o600) };
        let sysv_err = io::Error::last_os_error();
        let guard = Self { posix_fd, posix_name, sysv_id };
        assert!(guard.posix_fd >= 0, "shm_open {name}: {posix_err}");
        assert!(guard.sysv_id >= 0, "shmget {sysv_key:#x}: {sysv_err}");
        guard
    }
}

impl Drop for ShmGuard {
    fn drop(&mut self) {
        // SAFETY: closing and removing only what `new` made (a name it didn't make is another's).
        unsafe {
            if self.posix_fd >= 0 {
                libc::close(self.posix_fd);
                libc::shm_unlink(self.posix_name.as_ptr());
            }
            if self.sysv_id >= 0 {
                libc::shmctl(self.sysv_id, libc::IPC_RMID, std::ptr::null_mut());
            }
        }
    }
}

#[cfg(target_os = "linux")]
struct PtyGuard {
    master_fd: libc::c_int,
    slave_fd: libc::c_int,
    slave_path: String,
}

#[cfg(target_os = "linux")]
impl PtyGuard {
    fn new() -> Self {
        // SAFETY: opens a fresh master/slave pty pair from `/dev/ptmx` held open by this test and closed on Drop.
        unsafe {
            let master_fd =
                libc::open(c"/dev/ptmx".as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
            assert!(master_fd >= 0, "open /dev/ptmx: {}", io::Error::last_os_error());
            let mut unlock: libc::c_int = 0;
            assert_eq!(libc::ioctl(master_fd, libc::TIOCSPTLCK, &mut unlock), 0);
            let mut ptn: libc::c_uint = 0;
            assert_eq!(libc::ioctl(master_fd, libc::TIOCGPTN, &mut ptn), 0);
            let slave_path = format!("/dev/pts/{ptn}");
            let c_slave = CString::new(slave_path.as_str()).unwrap();
            let slave_fd = libc::open(c_slave.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC);
            assert!(slave_fd >= 0, "open {slave_path}: {}", io::Error::last_os_error());
            Self { master_fd, slave_fd, slave_path }
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for PtyGuard {
    fn drop(&mut self) {
        // SAFETY: closing the pty descriptors owned by this guard.
        unsafe {
            if self.slave_fd >= 0 {
                libc::close(self.slave_fd);
            }
            if self.master_fd >= 0 {
                libc::close(self.master_fd);
            }
        }
    }
}

/// A stand-in's script: alive until its guard ends it (never on a timer — one that ended mid-run would read as
/// a refusal), holding its stdout for `running`.
const STAND_IN: &str = "printf 'ready\\n'; exec sleep 86400";

fn spawn_stand_in(_exe: &Path, policy: Option<&Policy>, cwd: &Path, marker: &str) -> StandInGuard {
    use std::os::unix::process::CommandExt;
    let args = ["-c".to_string(), STAND_IN.to_string(), format!("{marker}-arg")];
    let mut cmd = command(policy, "/bin/sh", &args);
    cmd.current_dir(cwd)
        .env("TARAE_REFUSED_STAND_IN", format!("{marker}-env"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let child = cmd.spawn().unwrap();
    let pid = child.id();
    let mut guard = StandInGuard { child, pid };
    let mut line = String::new();
    io::BufReader::new(guard.child.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "ready");
    guard
}

fn spawn_sandboxed_stand_in(_exe: &Path, policy: &Policy, marker: &str) -> SandboxedStandInGuard {
    let args = ["-c".to_string(), STAND_IN.to_string(), format!("{marker}-arg")];
    let mut cmd = command(Some(policy), "/bin/sh", &args);
    cmd.current_dir(&policy.root).env("TARAE_REFUSED_STAND_IN", format!("{marker}-env")).stdin(Stdio::null());
    let s = spawn(Some(policy), &cmd).unwrap();
    let pid = s.pid;
    let mut guard = SandboxedStandInGuard { _spawned: s, pid, policy: policy.clone() };
    let mut line = String::new();
    io::BufReader::new(guard._spawned.stdout.as_mut().unwrap()).read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "ready");
    guard
}

fn run_helper(exe: &Path, policy: &Policy, mode: &str, cfg_file: &Path) -> (String, bool) {
    let args = [
        "--exact".to_string(),
        "sandbox::refused::in_sandbox".to_string(),
        "--ignored".to_string(),
        "--nocapture".to_string(),
    ];
    let mut cmd = command(Some(policy), &exe.to_string_lossy(), &args);
    cmd.current_dir(&policy.root)
        .env("TARAE_REFUSED_MODE", mode)
        .env("TARAE_REFUSED_CFG", cfg_file)
        .stdin(Stdio::null());
    let s = spawn(Some(policy), &cmd).unwrap();
    let mut text = String::new();
    io::Read::read_to_string(&mut s.stdout.unwrap(), &mut text).unwrap();
    io::Read::read_to_string(&mut s.stderr.unwrap(), &mut text).unwrap();
    let status = crate::spawn::wait(s.pid).unwrap();
    assert!(status.success(), "in_sandbox ({mode}) failed:\n{text}");
    let leaked = text.contains(FAKE_TOKEN);
    (text, leaked)
}

fn run_proxy_row(
    exe: &Path,
    root: &Path,
    other_root: &Path,
    here: &Here,
    fallback_port_b: u16,
) -> (HashMap<String, String>, bool, bool) {
    let net_llm = crate::llm::LlmConfig { sandbox_network: true, ..Default::default() };
    let net_a = confined_policy_in(root, &net_llm, here);
    let _net_a_cleanup = RunCleanupGuard { policy: net_a.clone() };
    let net_b = confined_policy_in(other_root, &net_llm, here);
    let _net_b_cleanup = RunCleanupGuard { policy: net_b.clone() };

    let url_a = net_a.proxy_url();
    let url_b = net_b.proxy_url();
    let has_proxy =
        net_a.guarantees().is_some_and(|g| g.way == super::guarantees::Way::Proxy) && url_a.is_some();

    let (addr_a, token_a) = match &url_a {
        Some(u) => (format!("{}:{}", u.host, u.port), u.password.clone().unwrap_or_default()),
        None => (route::PROXY.to_string(), String::new()),
    };

    // On macOS, session B has its own proxy port (`url_b.port`) and token (`url_b.password`).
    // On Linux served tiers (`own`, `host`), session B binds a loopback listener inside its sandbox (`listen-b`)
    // and we verify session A cannot connect to session B's port.
    let (_b_guard, port_b, token_b) = match (&url_b, has_proxy) {
        (Some(u), true) if u.password.is_some() => {
            let g = spawn_sandboxed_stand_in(exe, &net_b, "net-b");
            (Some(g), u.port, u.password.clone().unwrap_or_default())
        }
        (Some(_), true) => {
            let (g, p) = spawn_sandboxed_listener(exe, &net_b);
            (Some(g), p, "other-session-token".to_string())
        }
        _ => (None, fallback_port_b, "other-session-token".to_string()),
    };

    // Register TEST_ASKS for net_a's session and pre-answer `refused.invalid:443` with Scope::Refused when asked
    let answer_thread = if has_proxy {
        let (asks, asked) = std::sync::mpsc::channel();
        let session = super::proxy::session_of(root);
        super::proxy::TEST_ASKS.lock().unwrap().push((session, asks));
        Some(
            std::thread::Builder::new()
                .name("tarae-refused-answer".into())
                .spawn(move || {
                    if let Ok(FromProxy::Ask { run, host, port, .. }) =
                        asked.recv_timeout(Duration::from_secs(10))
                    {
                        super::proxy::get().send(ToProxy::Answer {
                            session,
                            run,
                            host,
                            port,
                            scope: Scope::Refused,
                        });
                    }
                })
                .unwrap(),
        )
    } else {
        None
    };

    let cfg_file = net_a.run.as_ref().unwrap().path().join("proxy.cfg");
    let offline = u8::from(!has_proxy);
    let cfg_text = format!(
        "ADDR_A={addr_a}\nTOKEN_A={token_a}\nPORT_B={port_b}\nTOKEN_B={token_b}\nOFFLINE={offline}\n"
    );
    write_file(&cfg_file, &cfg_text);

    let (out, leaked) = run_helper(exe, &net_a, "proxy", &cfg_file);
    // (B's stand-in still there: a refused connect to a listener that had ended would prove nothing)
    assert!(_b_guard.as_ref().is_none_or(|g| g.running()), "session B's stand-in ended during the run");
    drop(_net_a_cleanup);
    if let Some(t) = answer_thread {
        let _ = t.join();
    }
    drop(_b_guard);
    drop(_net_b_cleanup);

    (parse_kv(&out), leaked, has_proxy)
}

fn spawn_sandboxed_listener(exe: &Path, policy: &Policy) -> (SandboxedStandInGuard, u16) {
    let cfg_file = policy.run.as_ref().unwrap().path().join("listen.cfg");
    write_file(&cfg_file, "");
    let args = [
        "--exact".to_string(),
        "sandbox::refused::in_sandbox".to_string(),
        "--ignored".to_string(),
        "--nocapture".to_string(),
    ];
    let mut cmd = command(Some(policy), &exe.to_string_lossy(), &args);
    cmd.current_dir(&policy.root)
        .env("TARAE_REFUSED_MODE", "listen-b")
        .env("TARAE_REFUSED_CFG", &cfg_file)
        .stdin(Stdio::null());
    let s = spawn(Some(policy), &cmd).unwrap();
    let pid = s.pid;
    let mut guard = SandboxedStandInGuard { _spawned: s, pid, policy: policy.clone() };
    let mut reader = io::BufReader::new(guard._spawned.stdout.as_mut().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).unwrap();
        assert!(n > 0, "listen-b exited before printing port");
        if let Some(port_str) = line.trim().strip_prefix("listen-b-port=") {
            let port: u16 = port_str.parse().unwrap();
            return (guard, port);
        }
    }
}

fn parse_kv(text: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("refused-kv: ")
            && let Some((k, v)) = rest.split_once('=')
        {
            map.insert(k.to_string(), v.to_string());
        }
    }
    map
}

fn emit(k: &str, v: impl std::fmt::Display) {
    println!("refused-kv: {k}={v}");
}

#[test]
#[ignore = "run inside the sandbox by in_stand_in_home"]
fn in_sandbox() {
    let Ok(mode) = std::env::var("TARAE_REFUSED_MODE") else {
        return;
    };
    let cfg_path = PathBuf::from(std::env::var_os("TARAE_REFUSED_CFG").unwrap());
    let cfg_text = fs::read_to_string(&cfg_path).unwrap();
    let cfg: HashMap<&str, &str> = cfg_text.lines().filter_map(|l| l.split_once('=')).collect();
    match mode.as_str() {
        "main" => sandbox_main(&cfg),
        "move-in" => sandbox_move_in(&cfg),
        "proxy" => sandbox_proxy(&cfg),
        "listen-b" => sandbox_listen_b(),
        #[cfg(target_os = "linux")]
        "userns" => {
            for (name, got) in userns_probes() {
                emit(&format!("nsu_{name}"), got.said());
            }
        }
        other => panic!("unknown mode: {other}"),
    }
}

fn sandbox_listen_b() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    println!("listen-b-port={port}");
    let _ = io::stdout().flush();
    // (until its guard ends it, as `STAND_IN`)
    std::thread::sleep(Duration::from_secs(86400));
}

fn try_write(path: &Path) -> Result<(), io::Error> {
    fs::write(path, "pwn\n")
}

fn try_append(path: &Path) -> Result<(), io::Error> {
    let mut f = OpenOptions::new().append(true).open(path)?;
    f.write_all(b"pwn\n")
}

fn try_all_mutations(path: &Path) -> Result<(), io::Error> {
    let mut errs = Vec::new();
    for r in [
        try_write(path),
        try_append(path),
        fs::remove_file(path),
        fs::rename(path, path.with_extension("bak")),
    ] {
        match r {
            Ok(()) => return Ok(()),
            Err(e) => errs.push(e),
        }
    }
    Err(errs.remove(0))
}

fn record_refused(prefix: &str, items: &[(&str, Result<(), io::Error>)]) {
    let mut errs = Vec::new();
    for (name, r) in items {
        match r {
            Ok(()) => emit(&format!("{prefix}_{name}"), "allowed"),
            Err(e) => {
                emit(&format!("{prefix}_{name}"), "refused");
                errs.push(format!("{name}:{:?}", e.raw_os_error()));
            }
        }
    }
    emit(&format!("{prefix}_err"), errs.join(","));
}

fn sandbox_main(cfg: &HashMap<&str, &str>) {
    let home = Path::new(cfg["HOME_DIR"]);
    let root = Path::new(cfg["ROOT"]);
    let other_root = Path::new(cfg["OTHER_ROOT"]);
    let own_run = Path::new(cfg["OWN_RUN"]);
    let other_run = Path::new(cfg["OTHER_RUN"]);
    let runs = Path::new(cfg["RUNS"]);
    let outside = Path::new(cfg["OUTSIDE"]);
    let shared_tmp = Path::new(cfg["SHARED_TMP"]);

    // Row 1: write outside project
    record_refused(
        "r1",
        &[
            ("direct", try_write(&outside.join("marker"))),
            ("append", try_append(&outside.join("existing"))),
            ("link", try_write(&root.join("out-link"))),
        ],
    );

    // Row 2: write other session's checkout and RunDir
    record_refused(
        "r2",
        &[
            ("checkout", try_write(&other_root.join("src/pwn"))),
            ("append", try_append(&other_root.join("src/lib.rs"))),
            ("run", try_write(&other_run.join("pwn"))),
            ("runs", try_write(&runs.join("pwn"))),
        ],
    );

    // Row 3: write tarae's own config/state/data and `.tarae.toml`
    record_refused(
        "r3",
        &[
            ("config", try_append(&home.join(".config/tarae/config.toml"))),
            ("sandbox", try_write(&home.join(".local/state/tarae/sandbox.json"))),
            ("trust", try_write(&home.join(".local/state/tarae/trust.json"))),
            ("grammar", try_write(&home.join(".local/share/tarae/grammars/x.dylib"))),
            ("plugin", try_write(&home.join(".local/share/tarae/plugins/x.wasm"))),
            ("repo_cfg", try_append(&root.join(".tarae.toml"))),
        ],
    );

    // Row 4: write/append/unlink/rename existing protected names
    record_refused(
        "r4",
        &[
            ("cargo", try_all_mutations(&root.join("Cargo.toml"))),
            ("build", try_all_mutations(&root.join("sub/build.rs"))),
            ("git_cfg", try_all_mutations(&root.join(".git/config"))),
            ("git_hook", try_all_mutations(&root.join(".git/hooks/pre-commit"))),
        ],
    );

    // Row 5: make new protected names (including case variant, symlink, and beside an existing protected name)
    record_refused(
        "r5",
        &[
            ("beside", try_write(&root.join("sub/Cargo.toml"))),
            ("new", try_write(&root.join("new1/Cargo.toml"))),
            ("case", try_write(&root.join("new2/CARGO.TOML"))),
            ("sym", unix_fs::symlink("../src/lib.rs", root.join("new3/Cargo.toml"))),
        ],
    );

    // Row 6: hard-link protected files and keys into writable `src/`
    record_refused(
        "r6",
        &[
            ("cargo", fs::hard_link(root.join("Cargo.toml"), root.join("src/link-cargo"))),
            ("build", fs::hard_link(root.join("sub/build.rs"), root.join("src/link-build"))),
            ("ssh", fs::hard_link(home.join(".ssh/id_ed25519"), root.join("src/link-ssh"))),
            ("env", fs::hard_link(root.join(".env"), root.join("src/link-env"))),
        ],
    );

    // Row 7: move parent folder `sub` holding `sub/build.rs` -> `sub2`, then try writing/unlinking `build.rs`
    let moved = fs::rename(root.join("sub"), root.join("sub2")).is_ok();
    let target = if moved { root.join("sub2/build.rs") } else { root.join("sub/build.rs") };
    record_refused("r7", &[("write", try_write(&target)), ("unlink", fs::remove_file(&target))]);
    if moved {
        let _ = fs::rename(root.join("sub2"), root.join("sub"));
    }

    // Row 9: read kept files, keys, unlisted home files, scrubbed `.gitconfig`, and scrubbed env
    record_refused(
        "r9",
        &[
            ("ssh", fs::read_to_string(home.join(".ssh/id_ed25519")).map(drop)),
            ("aws", fs::read_to_string(home.join(".aws/credentials")).map(drop)),
            ("dotenv", fs::read_to_string(root.join(".env")).map(drop)),
            ("sandbox", fs::read_to_string(home.join(".local/state/tarae/sandbox.json")).map(drop)),
            ("home", fs::read_to_string(home.join("notes.txt")).map(drop)),
        ],
    );
    let direct_git = fs::read_to_string(home.join(".gitconfig"));
    let copy_git =
        std::env::var_os("GIT_CONFIG_GLOBAL").and_then(|p| fs::read_to_string(p).ok()).unwrap_or_default();
    let git_scrubbed = direct_git.is_err() && !copy_git.is_empty() && !copy_git.contains(FAKE_TOKEN);
    emit("r9_gitconfig", if git_scrubbed { "scrubbed" } else { "leaked" });
    let env_leaked = std::env::vars().any(|(k, v)| k.contains(FAKE_TOKEN) || v.contains(FAKE_TOKEN));
    emit("r9_vars", if env_leaked { "leaked" } else { "scrubbed" });

    // Row 10: TCP connect to unopened loopback port
    let port10: u16 = cfg["TCP_UNOPENED"].parse().unwrap();
    let addr10 = SocketAddr::from(([127, 0, 0, 1], port10));
    record_refused(
        "r10",
        &[("connect", TcpStream::connect_timeout(&addr10, Duration::from_millis(500)).map(drop))],
    );

    // Row 11: TCP connect to this machine's non-loopback address
    let addr11: SocketAddr = cfg["TCP_NON_LOOP"].parse().unwrap();
    record_refused(
        "r11",
        &[("connect", TcpStream::connect_timeout(&addr11, Duration::from_millis(500)).map(drop))],
    );

    // Row 12: UDP sendto loopback, resolver stand-in, and port 0
    let port12_loop: u16 = cfg["UDP_LOOP"].parse().unwrap();
    let port12_dns: u16 = cfg["UDP_DNS"].parse().unwrap();
    record_refused(
        "r12",
        &[
            ("loop", udp_send(SocketAddr::from(([127, 0, 0, 1], port12_loop)))),
            ("dns", udp_send(SocketAddr::from(([127, 0, 0, 1], port12_dns)))),
            ("zero", udp_send(SocketAddr::from(([127, 0, 0, 1], 0)))),
        ],
    );

    // Row 13: raw socket creation (nothing sent) and dgram-icmp to own loopback only
    record_refused(
        "r13",
        &[
            ("raw_icmp", try_socket(libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_ICMP)),
            ("raw_tcp", try_socket(libc::AF_INET, libc::SOCK_RAW, libc::IPPROTO_TCP)),
            ("dgram_icmp", try_dgram_icmp_loopback()),
        ],
    );

    // Row 16: unix sockets outside vs inside project and own RunDir
    record_refused(
        "r16",
        &[
            ("other_run", UnixStream::connect(other_run.join("s.sock")).map(drop)),
            ("state", UnixStream::connect(home.join(".local/state/tarae/ui.sock")).map(drop)),
            ("outside", UnixStream::connect(outside.join("s.sock")).map(drop)),
            ("tmp", UnixStream::connect(shared_tmp.join("s.sock")).map(drop)),
        ],
    );
    emit("r16_own_root", if UnixStream::connect(root.join("own.sock")).is_ok() { "ok" } else { "refused" });
    emit("r16_own_run", if UnixStream::connect(own_run.join("own.sock")).is_ok() { "ok" } else { "refused" });

    // Row 17: process signals, ptrace, and process inspection (no lldb)
    let tarae_pid: libc::pid_t = cfg["TARAE_PID"].parse().unwrap();
    let other_pid: libc::pid_t = cfg["OTHER_PID"].parse().unwrap();
    check_processes(tarae_pid, other_pid);

    // Row 18: SysV and POSIX shared memory
    let sysv_key: libc::key_t = cfg["SYSV_KEY"].parse().unwrap();
    let posix_name = cfg["POSIX_SHM"];
    let sysv_r = sysv_attached(sysv_key);
    let posix_r = posix_opened(posix_name);
    emit("r18_sysv", if sysv_r.is_err() { "refused" } else { "shared" });
    emit("r18_posix", if posix_r.is_ok() { "shared" } else { "refused" });
    emit("r18_err", format!("sysv={:?},posix={:?}", sysv_r.err(), posix_r.err()));

    #[cfg(target_os = "macos")]
    {
        // Row 19: macOS LaunchServices, Apple Events, Keychain, Pasteboard (`bootstrap_look_up` only, nothing sent)
        record_refused(
            "r19",
            &[
                ("lsd", mach_lookup("com.apple.coreservices.launchservicesd")),
                ("mapdb", mach_lookup("com.apple.lsd.mapdb")),
                ("ae", mach_lookup("com.apple.coreservices.appleevents")),
                ("sec", mach_lookup("com.apple.SecurityServer")),
                ("pb", mach_lookup("com.apple.pasteboard.1")),
            ],
        );
        emit(
            "r19_ctrl",
            if mach_lookup("com.apple.system.opendirectoryd.libinfo").is_ok() { "ok" } else { "refused" },
        );

        // Row 20: nested `sandbox-exec` attempting to replace profile with `(allow default)`
        let nested_target = outside.join("nested");
        let nested = Command::new(SANDBOX_EXEC)
            .args([
                "-p",
                "(version 1)(allow default)",
                "/bin/sh",
                "-c",
                &format!("echo pwn > '{}'", nested_target.display()),
            ])
            .stdin(Stdio::null())
            .output();
        let nested_refused = match &nested {
            Ok(o) => !o.status.success() && !nested_target.exists(),
            Err(_) => !nested_target.exists(),
        };
        emit("r20_nested", if nested_refused { "refused" } else { "allowed" });
        emit(
            "r20_err",
            nested.map(|o| format!("code={:?}", o.status.code())).unwrap_or_else(|e| format!("err={e}")),
        );
    }

    #[cfg(target_os = "linux")]
    check_linux_rows(
        home,
        root,
        own_run,
        cfg["STAND_IN_PTY"],
        cfg.get("KEEPER_PID").copied().unwrap_or("0"),
        cfg.get("KEEPER_PIDNS").copied().unwrap_or(""),
    );
}

fn sandbox_move_in(cfg: &HashMap<&str, &str>) {
    let root = Path::new(cfg["ROOT"]);
    let own_run = Path::new(cfg["OWN_RUN"]);
    let stage = own_run.join("x");
    fs::create_dir_all(&stage).unwrap();
    fs::write(stage.join("build.rs"), "fn main() {}\n").unwrap();
    let dest = root.join("crates/x");
    fs::rename(&stage, &dest).unwrap();
    // Once inside the project root, writing `crates/x/build.rs` is refused by Seatbelt on macOS and caught by
    // `Before::since()` on both macOS and Linux.
    match try_append(&dest.join("build.rs")) {
        Ok(()) => emit("r8_write", "allowed"),
        Err(e) => {
            emit("r8_write", "refused");
            emit("r8_err", format!("{:?}", e.raw_os_error()));
        }
    }
}

fn sandbox_proxy(cfg: &HashMap<&str, &str>) {
    let addr_a: SocketAddr = cfg["ADDR_A"].parse().unwrap();
    let token_a = cfg["TOKEN_A"];
    let port_b: u16 = cfg["PORT_B"].parse().unwrap();

    // 1. Unlisted host `refused.invalid:443` through own proxy port with own token -> 403. Offline (no proxy)
    // the socket's own refusal is the answer and nothing connects: were the rule broken, a connect to
    // `route::PROXY` would leave by the default route.
    let status_unlisted = if cfg["OFFLINE"] == "1" {
        match try_socket(libc::AF_INET, libc::SOCK_STREAM, 0) {
            Ok(()) => "allowed".to_string(),
            Err(e) if matches!(e.raw_os_error(), Some(libc::EPERM | libc::EACCES)) => "refused".to_string(),
            Err(e) => {
                emit("r14_unlisted_err", format!("{:?}", e.raw_os_error()));
                "error".to_string()
            }
        }
    } else {
        let auth_a = base64_basic(&format!("run:{token_a}"));
        proxy_connect(addr_a, "refused.invalid:443", &auth_a).unwrap_or_else(|e| {
            emit("r14_unlisted_err", format!("{:?}", e.raw_os_error()));
            "error".to_string()
        })
    };
    emit("r14_unlisted", status_unlisted);

    // 2. Connect to other session's proxy/listener port -> refused
    let addr_b = SocketAddr::from(([127, 0, 0, 1], port_b));
    match TcpStream::connect_timeout(&addr_b, Duration::from_millis(500)) {
        Ok(_) => emit("r14_other_port", "allowed"),
        Err(e) => {
            emit("r14_other_port", "refused");
            emit("r14_err", format!("{:?}", e.raw_os_error()));
        }
    }

    // 3. Connect to own proxy port with other session's token -> 407 on macOS; on Linux supervisor identifies run by socketpair
    #[cfg(target_os = "macos")]
    {
        let auth_b = base64_basic(&format!("run:{}", cfg["TOKEN_B"]));
        let status_bad_tok =
            proxy_connect(addr_a, "refused.invalid:443", &auth_b).unwrap_or_else(|_| "refused".to_string());
        emit("r14_other_token", status_bad_tok);
    }
}

fn proxy_connect(addr: SocketAddr, authority: &str, basic: &str) -> io::Result<String> {
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(5))?;
    s.set_read_timeout(Some(Duration::from_secs(5)))?;
    s.set_write_timeout(Some(Duration::from_secs(5)))?;
    let req = format!(
        "CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\nProxy-Authorization: Basic {basic}\r\n\r\n"
    );
    s.write_all(req.as_bytes())?;
    let mut line = String::new();
    io::BufReader::new(&s).read_line(&mut line)?;
    let code = line.split_whitespace().nth(1).unwrap_or("none").to_string();
    Ok(code)
}

fn base64_basic(input: &str) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let bytes = input.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(if chunk.len() > 1 { TABLE[((n >> 6) & 0x3f) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[(n & 0x3f) as usize] as char } else { '=' });
    }
    out
}

fn udp_send(to: SocketAddr) -> io::Result<()> {
    let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
    sock.send_to(b"x", to).map(drop)
}

fn try_socket(domain: libc::c_int, ty: libc::c_int, proto: libc::c_int) -> io::Result<()> {
    // SAFETY: `socket` with standard constants; if a descriptor is returned, it is closed at once without sending.
    unsafe {
        let fd = libc::socket(domain, ty, proto);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        libc::close(fd);
        Ok(())
    }
}

fn try_dgram_icmp_loopback() -> io::Result<()> {
    // SAFETY: creates an unprivileged `SOCK_DGRAM` `IPPROTO_ICMP` socket and tries `sendto` to `127.0.0.1` only
    // (Seatbelt checks `network-outbound` on `sendto`/`connect`), closing the descriptor at once.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, libc::IPPROTO_ICMP);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        let mut sin: libc::sockaddr_in = std::mem::zeroed();
        #[cfg(target_os = "macos")]
        {
            sin.sin_len = std::mem::size_of::<libc::sockaddr_in>() as u8;
        }
        sin.sin_family = libc::AF_INET as libc::sa_family_t;
        sin.sin_addr.s_addr = u32::from_ne_bytes([127, 0, 0, 1]);
        let echo: [u8; 8] = [8, 0, 0xf7, 0xff, 0, 0, 0, 0];
        let n = libc::sendto(
            fd,
            echo.as_ptr().cast(),
            echo.len(),
            0,
            (&raw const sin).cast(),
            std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
        );
        let err = io::Error::last_os_error();
        libc::close(fd);
        if n < 0 { Err(err) } else { Ok(()) }
    }
}

#[cfg(target_os = "macos")]
fn check_processes(tarae_pid: libc::pid_t, other_pid: libc::pid_t) {
    use std::os::unix::process::CommandExt;
    let child = Command::new("/bin/sh")
        .args(["-c", "printf 'ready\\n'; sleep 60", "own-child-arg"])
        .env("TARAE_REFUSED_STAND_IN", "own-child-env")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id();
    let mut own = StandInGuard { child, pid };
    let mut ready = String::new();
    io::BufReader::new(own.child.stdout.as_mut().unwrap()).read_line(&mut ready).unwrap();
    let own_pid = own.pid as libc::pid_t;
    let ctrl_ok = pid_kill0(own_pid).is_ok()
        && pid_cont(own_pid).is_ok()
        && pid_info(own_pid).is_ok()
        && pid_procargs(own_pid, "own-child").is_ok();
    emit("own_child", if ctrl_ok { "ok" } else { "refused" });
    drop(own);

    let mut errs = Vec::new();
    for (who, pid, marker) in [("tarae", tarae_pid, "tarae-stand-in"), ("other", other_pid, "other-stand-in")]
    {
        for (name, res) in [
            ("kill0", pid_kill0(pid)),
            ("cont", pid_cont(pid)),
            ("ptrace", pid_ptrace(pid)),
            ("task", pid_task_port(pid)),
            ("info", pid_info(pid)),
            ("args", pid_procargs(pid, marker)),
            ("prio", pid_prio(pid)),
        ] {
            match res {
                Ok(()) => emit(&format!("{who}_{name}"), "allowed"),
                Err(e) => {
                    emit(&format!("{who}_{name}"), "refused");
                    errs.push(format!("{who}_{name}:{:?}", e.raw_os_error()));
                }
            }
        }
    }
    emit("r17_err", errs.join(","));
}

#[cfg(target_os = "linux")]
fn check_processes(tarae_pid: libc::pid_t, other_pid: libc::pid_t) {
    use std::os::unix::process::CommandExt;
    let child = Command::new("/bin/sh")
        .args(["-c", "printf 'ready\\n'; read _line", "own-child-arg"])
        .env("TARAE_REFUSED_STAND_IN", "own-child-env")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .unwrap();
    let pid = child.id();
    let mut own = StandInGuard { child, pid };
    let mut ready = String::new();
    io::BufReader::new(own.child.stdout.as_mut().unwrap()).read_line(&mut ready).unwrap();
    let own_pid = own.pid as libc::pid_t;
    let k0 = pid_kill0(own_pid);
    let cont = pid_cont(own_pid);
    let prio = pid_prio(own_pid);
    let cmd = proc_contains(own_pid, "cmdline", "own-child");
    let env = proc_contains(own_pid, "environ", "own-child");
    let ctrl_ok = k0.is_ok() && cont.is_ok() && (prio.is_ok() || cmd.is_ok());
    emit("own_child", if ctrl_ok { "ok" } else { "refused" });
    drop(own);

    let mut errs = Vec::new();
    errs.push(format!(
        "own(k0={:?},cont={:?},prio={:?},cmd={:?},env={:?})",
        k0.err(),
        cont.err(),
        prio.err(),
        cmd.err(),
        env.err()
    ));
    for (who, pid, marker) in [("tarae", tarae_pid, "tarae-stand-in"), ("other", other_pid, "other-stand-in")]
    {
        for (name, res) in [
            ("kill0", pid_kill0(pid)),
            ("cont", pid_cont(pid)),
            ("ptrace", pid_ptrace_linux(pid)),
            ("environ", proc_contains(pid, "environ", marker)),
            ("cmdline", proc_contains(pid, "cmdline", marker)),
            ("prio", pid_prio(pid)),
        ] {
            match res {
                Ok(()) => emit(&format!("{who}_{name}"), "allowed"),
                Err(e) => {
                    emit(&format!("{who}_{name}"), "refused");
                    errs.push(format!("{who}_{name}:{:?}", e.raw_os_error()));
                }
            }
        }
    }
    emit("r17_err", errs.join(","));
}

fn pid_kill0(pid: libc::pid_t) -> io::Result<()> {
    // SAFETY: signal 0 only probes whether the process can be signalled.
    if unsafe { libc::kill(pid, 0) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

fn pid_cont(pid: libc::pid_t) -> io::Result<()> {
    // SAFETY: SIGCONT is harmless to a sleeping stand-in process owned by this test.
    if unsafe { libc::kill(pid, libc::SIGCONT) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

fn pid_prio(pid: libc::pid_t) -> io::Result<()> {
    // SAFETY: `setpriority` on our own stand-in process.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, 7) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
fn pid_ptrace_linux(pid: libc::pid_t) -> io::Result<()> {
    // SAFETY: attempting `PTRACE_SEIZE` on our own stand-in process; if it ever succeeded, detach at once.
    unsafe {
        let null = std::ptr::null_mut::<libc::c_void>();
        if libc::ptrace(libc::PTRACE_SEIZE, pid, null, null) == 0 {
            libc::ptrace(libc::PTRACE_INTERRUPT, pid, null, null);
            let mut status: libc::c_int = 0;
            libc::waitpid(pid, &mut status, 0);
            libc::ptrace(libc::PTRACE_DETACH, pid, null, null);
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(target_os = "linux")]
fn proc_contains(pid: libc::pid_t, entry: &str, marker: &str) -> io::Result<()> {
    let bytes = fs::read(format!("/proc/{pid}/{entry}"))?;
    let text = String::from_utf8_lossy(&bytes);
    if text.contains(marker) { Ok(()) } else { Err(io::Error::from_raw_os_error(libc::EACCES)) }
}

#[cfg(target_os = "macos")]
fn pid_ptrace(pid: libc::pid_t) -> io::Result<()> {
    // SAFETY: attempting `PT_ATTACHEXC` on our own stand-in process; if it ever succeeded, detach at once.
    unsafe {
        if libc::ptrace(libc::PT_ATTACHEXC, pid, std::ptr::null_mut(), 0) == 0 {
            libc::ptrace(libc::PT_DETACH, pid, std::ptr::null_mut(), 0);
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(target_os = "macos")]
fn pid_task_port(pid: libc::pid_t) -> io::Result<()> {
    unsafe extern "C" {
        fn mach_task_self() -> libc::mach_port_t;
        fn mach_port_deallocate(task: libc::mach_port_t, name: libc::mach_port_t) -> libc::kern_return_t;
        fn task_for_pid(
            target_tport: libc::mach_port_t,
            pid: libc::c_int,
            t: *mut libc::mach_port_t,
        ) -> libc::kern_return_t;
    }
    let mut port: libc::mach_port_t = 0;
    // SAFETY: querying the task port of our own stand-in process; deallocated at once if returned.
    unsafe {
        let kr = task_for_pid(mach_task_self(), pid, &mut port);
        if kr == 0 && port != 0 {
            mach_port_deallocate(mach_task_self(), port);
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(libc::EPERM))
        }
    }
}

#[cfg(target_os = "macos")]
fn pid_info(pid: libc::pid_t) -> io::Result<()> {
    // SAFETY: zeroed `proc_bsdinfo` struct filled in by `proc_pidinfo`.
    let mut bi: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // SAFETY: passing the buffer and its exact size.
    let n = unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut bi).cast(), size) };
    if n == size { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(target_os = "macos")]
fn pid_procargs(pid: libc::pid_t, marker: &str) -> io::Result<()> {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 1 << 16;
    let mut buf = vec![0u8; size];
    let null = std::ptr::null_mut();
    // SAFETY: `sysctl` with `KERN_PROCARGS2` reading into `buf` of `size` bytes.
    let r = unsafe { libc::sysctl(mib.as_mut_ptr(), 3, buf.as_mut_ptr().cast(), &mut size, null, 0) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    buf.truncate(size);
    let text = String::from_utf8_lossy(&buf);
    if text.contains(marker) {
        Ok(())
    } else {
        Err(io::Error::new(io::ErrorKind::PermissionDenied, "marker not in KERN_PROCARGS2"))
    }
}

#[cfg(target_os = "macos")]
fn mach_lookup(name: &str) -> io::Result<()> {
    unsafe extern "C" {
        static bootstrap_port: libc::mach_port_t;
        fn bootstrap_look_up(
            bp: libc::mach_port_t,
            service_name: *const libc::c_char,
            sp: *mut libc::mach_port_t,
        ) -> libc::kern_return_t;
        fn mach_task_self() -> libc::mach_port_t;
        fn mach_port_deallocate(task: libc::mach_port_t, name: libc::mach_port_t) -> libc::kern_return_t;
    }
    let c = CString::new(name).unwrap();
    let mut port: libc::mach_port_t = 0;
    // SAFETY: `bootstrap_look_up` only queries the service port; if returned, it is deallocated at once with
    // nothing sent.
    unsafe {
        let kr = bootstrap_look_up(bootstrap_port, c.as_ptr(), &mut port);
        if kr == 0 && port != 0 {
            mach_port_deallocate(mach_task_self(), port);
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(libc::EPERM))
        }
    }
}

fn sysv_attached(key: libc::key_t) -> io::Result<()> {
    // SAFETY: `shmget` and `shmat` on the test's own segment key; detached at once if attached.
    unsafe {
        let id = libc::shmget(key, 0, 0o600);
        if id < 0 {
            return Err(io::Error::last_os_error());
        }
        let p = libc::shmat(id, std::ptr::null(), libc::SHM_RDONLY);
        if p == (-1isize) as *mut libc::c_void {
            return Err(io::Error::last_os_error());
        }
        libc::shmdt(p);
        Ok(())
    }
}

fn posix_opened(name: &str) -> io::Result<()> {
    let c = CString::new(name).unwrap();
    // SAFETY: `shm_open` read-only on the test's own POSIX shm name; closed at once if opened.
    unsafe {
        let fd = libc::shm_open(c.as_ptr(), libc::O_RDONLY, 0 as libc::c_uint);
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        libc::close(fd);
        Ok(())
    }
}

/// What one of rows 21–26's calls got (`forked`, `keeper_probes`) — the same calls in the baseline and in the
/// run (`ns_probes`), told apart by whose answer it is (`part`). Every OS: the judging (`part`, `keeper_row`) is
/// tested everywhere, the calls are Linux's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
enum Probe {
    Done,
    /// Past every filter, turned down by the kernel's own check after them (`setns` into the caller's own user
    /// namespace, `pivot_root` of folders that aren't mounts: EINVAL) — what tarae's refusal never is.
    Reached,
    /// Refused, with its errno.
    Refused(i32),
    /// No user namespace to try it in (`in_own_ns` couldn't make one).
    NoUserNs,
    /// A step before the call was refused: it was never made.
    NotTried,
    /// The child ended by this signal.
    Signal(i32),
}

/// `forked`'s exit codes past an errno's.
#[cfg(target_os = "linux")]
const NOT_TRIED: i32 = 252;
#[cfg(target_os = "linux")]
const REACHED: i32 = 253;
#[cfg(target_os = "linux")]
const NO_USERNS: i32 = 254;

impl Probe {
    /// Got through the filters: done, or the kernel's own answer past them.
    fn through(self) -> bool {
        matches!(self, Probe::Done | Probe::Reached)
    }
}

#[cfg(target_os = "linux")]
impl Probe {
    fn of(code: i32) -> Probe {
        match code {
            0 => Probe::Done,
            NOT_TRIED => Probe::NotTried,
            REACHED => Probe::Reached,
            NO_USERNS => Probe::NoUserNs,
            e => Probe::Refused(e),
        }
    }

    /// What the run sends back — its errno for `detail`, never a witness.
    fn said(self) -> String {
        match self {
            Probe::Done => "done".into(),
            Probe::Reached => "reached".into(),
            Probe::Refused(e) => format!("refused:{e}"),
            Probe::NoUserNs => "no-userns".into(),
            Probe::NotTried => "not-tried".into(),
            Probe::Signal(s) => format!("signal:{s}"),
        }
    }

    fn parse(s: &str) -> Probe {
        let n = |v: &str| v.parse().unwrap_or_else(|_| panic!("a probe's word: {s}"));
        match s.split_once(':') {
            None if s == "done" => Probe::Done,
            None if s == "reached" => Probe::Reached,
            None if s == "no-userns" => Probe::NoUserNs,
            None if s == "not-tried" => Probe::NotTried,
            Some(("refused", e)) => Probe::Refused(n(e)),
            Some(("signal", e)) => Probe::Signal(n(e)),
            _ => panic!("a probe's word: {s}"),
        }
    }
}

/// The probe `key` a run said (`emit`).
#[cfg(target_os = "linux")]
fn probe_said(got: &HashMap<String, String>, key: &str) -> Probe {
    Probe::parse(got.get(key).unwrap_or_else(|| panic!("the run said no {key}")))
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
struct LinuxBaseline {
    /// `ns_probes`, by name.
    ns: HashMap<&'static str, Probe>,
    /// `sys_probes`, by name.
    sys: HashMap<&'static str, Probe>,
    /// Row 25's `keeper_probes` at the test's own process, by name.
    keeper: HashMap<&'static str, Probe>,
    /// Row 26's opens: `/dev/tty` (the terminal `must_be_refused` gave this process) and the stand-in pty.
    ctty: Probe,
    other_pty: Probe,
}

/// A part's word, the run's answer against the baseline's: `allowed` (it got through), `not-tried` (a step
/// before the call failed in the run: nothing to tell, and never a refusal), `system` (the baseline didn't get
/// through either: nothing of tarae's to tell), `no-userns` (the run never reached the call: no user namespace to
/// try it in), `refused` (refused where the baseline got through: tarae's).
fn part(base: Probe, run: Probe) -> &'static str {
    match run {
        r if r.through() => "allowed",
        Probe::NotTried => "not-tried",
        _ if !base.through() => "system",
        Probe::NoUserNs => "no-userns",
        _ => "refused",
    }
}

/// The calling thread's errno — async-signal-safe, for a forked child.
#[cfg(target_os = "linux")]
fn errno() -> i32 {
    // SAFETY: the thread's own errno slot, read.
    unsafe { *libc::__errno_location() }
}

/// `f` in a forked child, its exit code its answer (an errno, 0 done, or `REACHED`/`NO_USERNS`).
///
/// # Safety
/// `f` runs in a fork of a multithreaded process: async-signal-safe calls only — no allocation, no locks, no
/// Rust stdio —, what it needs (C strings, buffers) made before.
#[cfg(target_os = "linux")]
unsafe fn forked(f: impl FnOnce() -> i32) -> Probe {
    // SAFETY: the child runs `f` (the caller's promise) and `_exit`s; the parent reaps its own child.
    unsafe {
        let pid = libc::fork();
        assert!(pid >= 0, "fork: {}", io::Error::last_os_error());
        if pid == 0 {
            libc::_exit(f().clamp(0, 255));
        }
        let mut status: libc::c_int = 0;
        assert!(libc::waitpid(pid, &mut status, 0) == pid, "waitpid: {}", io::Error::last_os_error());
        match libc::WIFEXITED(status) {
            true => Probe::of(libc::WEXITSTATUS(status)),
            false => Probe::Signal(libc::WTERMSIG(status)),
        }
    }
}

#[cfg(target_os = "linux")]
fn fmt_id_map(buf: &mut [u8; 64], id: libc::uid_t) -> usize {
    buf[0] = b'0';
    buf[1] = b' ';
    let mut n = id;
    let mut digits = [0u8; 20];
    let mut dlen = 0;
    if n == 0 {
        digits[0] = b'0';
        dlen = 1;
    } else {
        while n > 0 {
            digits[dlen] = b'0' + (n % 10) as u8;
            n /= 10;
            dlen += 1;
        }
    }
    let mut at = 2;
    for i in (0..dlen).rev() {
        buf[at] = digits[i];
        at += 1;
    }
    buf[at] = b' ';
    buf[at + 1] = b'1';
    buf[at + 2] = b'\n';
    at + 3
}

/// `forked`, the child first in a user and mount namespace of its own, its IDs mapped to the caller's (as far as
/// the maps can be written — a mount that needs them fails in the baseline too: `system`): what `f` mounts,
/// unmounts or pivots is in that namespace alone, never the test's, the container's or the host's. `NoUserNs`
/// where none can be made.
///
/// # Safety
/// As `forked`.
#[cfg(target_os = "linux")]
unsafe fn in_own_ns(f: impl FnOnce() -> i32) -> Probe {
    // SAFETY: plain getters.
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let (mut ubuf, mut gbuf) = ([0u8; 64], [0u8; 64]);
    let (ulen, glen) = (fmt_id_map(&mut ubuf, uid), fmt_id_map(&mut gbuf, gid));
    // SAFETY: the child makes raw calls on the buffers made above, then `f` (the caller's promise).
    unsafe {
        forked(|| {
            if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0 {
                return NO_USERNS;
            }
            // (a run is undumpable: its /proc/self files would be root's, the maps unwritable)
            libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0);
            let maps = [
                (c"/proc/self/setgroups", &b"deny"[..]),
                (c"/proc/self/uid_map", &ubuf[..ulen]),
                (c"/proc/self/gid_map", &gbuf[..glen]),
            ];
            for (path, buf) in maps {
                let fd = libc::open(path.as_ptr(), libc::O_WRONLY);
                if fd >= 0 {
                    libc::write(fd, buf.as_ptr().cast(), buf.len());
                    libc::close(fd);
                }
            }
            f()
        })
    }
}

/// Rows 21 and 22's calls, one set for both sides — the baseline's are the run's: `run_linux_baseline` (the
/// test's own process, unsandboxed) and `check_linux_rows` (the run). The links in the caller's own process (in
/// `dir`); the rest each in a forked child, the mounts in namespaces of the child's own (`in_own_ns`). `dir`: a
/// new folder of the caller's own (the links, the bind target, the mount points), removed after.
#[cfg(target_os = "linux")]
fn ns_probes(env_file: &Path, ssh_file: &Path, dir: &Path) -> Vec<(&'static str, Probe)> {
    fs::create_dir_all(dir.join("mnt/old")).unwrap();
    let refused = |e: io::Error| Probe::Refused(e.raw_os_error().unwrap_or(0));
    let hard = dir.join("hard");
    let hardlink = fs::hard_link(env_file, &hard)
        .or_else(|_| fs::hard_link(ssh_file, &hard))
        .map_or_else(refused, |()| Probe::Done);
    let sym = dir.join("sym");
    unix_fs::symlink(env_file, &sym).unwrap();
    let symlink = fs::read_to_string(&sym).map_or_else(refused, |_| Probe::Done);

    let bind_target = dir.join("bind");
    write_file(&bind_target, "");
    let c = |p: &Path| CString::new(p.to_string_lossy().as_bytes()).unwrap();
    let (c_env, c_bind) = (c(env_file), c(&bind_target));
    let (c_mnt, c_old) = (c(&dir.join("mnt")), c(&dir.join("mnt/old")));
    let null = std::ptr::null::<libc::c_void>;
    // SAFETY (the four below): the child makes raw calls only, on C strings made before the fork; what it
    // mounts, unmounts or pivots is in the namespaces `in_own_ns` made for it alone.
    let bind = unsafe {
        in_own_ns(|| {
            if libc::mount(c_env.as_ptr(), c_bind.as_ptr(), null().cast(), libc::MS_BIND, null()) != 0 {
                return errno();
            }
            libc::umount2(c_bind.as_ptr(), libc::MNT_DETACH);
            0
        })
    };
    let open_tree = unsafe {
        in_own_ns(|| {
            // (OPEN_TREE_CLONE: a detached copy, then moved onto the target — MOVE_MOUNT_F_EMPTY_PATH)
            let fd = libc::syscall(libc::SYS_open_tree, libc::AT_FDCWD, c_env.as_ptr(), 1usize);
            if fd < 0 {
                return errno();
            }
            let r = libc::syscall(
                libc::SYS_move_mount,
                fd,
                c"".as_ptr(),
                libc::AT_FDCWD,
                c_bind.as_ptr(),
                4usize,
            );
            let e = errno();
            libc::close(fd as libc::c_int);
            if r != 0 {
                return e;
            }
            libc::umount2(c_bind.as_ptr(), libc::MNT_DETACH);
            0
        })
    };
    // (a mount and its unmount, one part: Landlock refuses the mount first, and an unmount can't be tried
    // without a mount of its own — the kernel's EINVAL on a folder that isn't one comes before the LSM's)
    let mount = unsafe {
        in_own_ns(|| {
            if libc::mount(c"tmpfs".as_ptr(), c_mnt.as_ptr(), c"tmpfs".as_ptr(), 0, null()) != 0 {
                return errno();
            }
            if libc::umount2(c_mnt.as_ptr(), libc::MNT_DETACH) != 0 {
                return errno();
            }
            0
        })
    };
    // (two folders, no mount first: the LSM's check comes right after the paths' lookup — Landlock: EPERM —,
    // then the kernel's own, EINVAL for a folder that isn't a mount: `Reached`, what a run let through gets)
    let pivot = unsafe {
        in_own_ns(|| match libc::syscall(libc::SYS_pivot_root, c_mnt.as_ptr(), c_old.as_ptr()) {
            0 => 0,
            _ => match errno() {
                libc::EINVAL | libc::EBUSY => REACHED,
                e => e,
            },
        })
    };
    let _ = fs::remove_dir_all(dir);
    let mut all =
        vec![("hardlink", hardlink), ("symlink", symlink), ("bind", bind), ("open_tree", open_tree)];
    all.extend(userns_probes());
    all.extend([("mount", mount), ("pivot", pivot)]);
    all
}

/// Row 22's user namespace calls: `unshare`, `clone` and `clone3` with `CLONE_NEWUSER`, and `setns` into the
/// caller's own user namespace (the kernel's EINVAL past the filters: `Reached`; tarae's EPERM comes first; its
/// namespace file not opened: `NotTried`, no setns made) — each in a forked child: what one makes is that
/// child's alone, and joining one's own changes nothing.
#[cfg(target_os = "linux")]
fn userns_probes() -> Vec<(&'static str, Probe)> {
    #[repr(C)]
    #[derive(Default)]
    struct CloneArgs {
        flags: u64,
        pidfd: u64,
        child_tid: u64,
        parent_tid: u64,
        exit_signal: u64,
        stack: u64,
        stack_size: u64,
        tls: u64,
    }
    let args = CloneArgs {
        flags: libc::CLONE_NEWUSER as u64,
        exit_signal: libc::SIGCHLD as u64,
        ..Default::default()
    };
    // A clone's answer in the forked child: its own child (on a copy of the stack, as a fork's) ends at once and
    // is reaped.
    // SAFETY: called in a forked child only, with a clone's return.
    let reaped = |pid: libc::c_long| unsafe {
        match pid {
            0 => libc::_exit(0),
            p if p > 0 => {
                libc::waitpid(p as libc::pid_t, std::ptr::null_mut(), 0);
                0
            }
            _ => errno(),
        }
    };
    // SAFETY (the four below): the child makes raw calls only, on what was made before the fork.
    let unshare = unsafe { forked(|| if libc::unshare(libc::CLONE_NEWUSER) == 0 { 0 } else { errno() }) };
    let clone = unsafe {
        forked(|| {
            let flags = (libc::CLONE_NEWUSER | libc::SIGCHLD) as libc::c_ulong;
            reaped(libc::syscall(libc::SYS_clone, flags, 0usize, 0usize, 0usize, 0usize))
        })
    };
    let clone3 = unsafe {
        forked(|| reaped(libc::syscall(libc::SYS_clone3, &raw const args, std::mem::size_of::<CloneArgs>())))
    };
    let setns = unsafe {
        forked(|| {
            let fd = libc::open(c"/proc/self/ns/user".as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC);
            if fd < 0 {
                return NOT_TRIED;
            }
            let r = libc::setns(fd, libc::CLONE_NEWUSER);
            let e = errno();
            libc::close(fd);
            match (r, e) {
                (0, _) => 0,
                (_, libc::EINVAL) => REACHED,
                (_, e) => e,
            }
        })
    };
    vec![("unshare", unshare), ("clone", clone), ("clone3", clone3), ("setns", setns)]
}

/// Row 22's run that refuses new user namespaces (`no_userns`, what tarae's AppArmor profile brings, off on the
/// machines the suite runs on): the session's keeper starts it so, asked to (`Keeper::start_userns`), with the
/// session's rules — without their mounts over key files: it makes user namespaces and reads nothing.
#[cfg(target_os = "linux")]
fn refusing_userns_run(exe: &Path, policy: &Policy, cfg_file: &Path) -> HashMap<String, String> {
    let c = policy.confine.as_ref().unwrap().as_ref().unwrap();
    let k = c.keeper.as_ref().expect("a keeper's policy");
    let mut cmd = Command::new(exe);
    cmd.args(["--exact", "sandbox::refused::in_sandbox", "--ignored", "--nocapture"])
        .env("TARAE_REFUSED_MODE", "userns")
        .env("TARAE_REFUSED_CFG", cfg_file)
        .current_dir(&policy.root);
    let comm = crate::landlock::thread_name();
    let rules = c.rules_and_covers().0;
    let s = k.start_userns(&cmd, &comm, rules, super::guarantees::RunNet::Plain, true).unwrap().spawned;
    let mut text = String::new();
    io::Read::read_to_string(&mut s.stdout.unwrap(), &mut text).unwrap();
    io::Read::read_to_string(&mut s.stderr.unwrap(), &mut text).unwrap();
    let status = crate::spawn::wait(s.pid).unwrap();
    assert!(status.success(), "in_sandbox (userns) failed:\n{text}");
    assert!(!text.contains(FAKE_TOKEN), "the userns run said the token:\n{text}");
    parse_kv(&text)
}

#[cfg(target_os = "linux")]
fn run_linux_baseline(home: &Path, root: &Path, own_run: &Path, stand_in_pty: &str) -> LinuxBaseline {
    let ns = ns_probes(&root.join(".env"), &home.join(".ssh/id_ed25519"), &own_run.join("base-ns"));

    // (row 25 at a bare child of the test's own, gone before any run; row 26 the terminal `must_be_refused`
    // gave this process)
    let keeper = {
        let target = BareTarget::new();
        let got = keeper_probes(Some(target.pid), true);
        assert!(target.alive(), "the baseline's target ended before its probes did");
        got.into_iter().collect()
    };
    let ctty = open_probe(c"/dev/tty");
    let other_pty = open_probe(&CString::new(stand_in_pty).unwrap());

    LinuxBaseline {
        ns: ns.into_iter().collect(),
        sys: sys_probes().into_iter().collect(),
        keeper,
        ctty,
        other_pty,
    }
}

/// Rows 23 and 24's calls, one set for both sides as `ns_probes`. The filters, sockets and rings each in a
/// forked child (a filter it installs is that child's alone); the capabilities and no-new-privs read in the
/// caller's own process. `foreign_arch` only on x86-64 (i386's `int 0x80`: the other ABI a run could reach).
#[cfg(target_os = "linux")]
fn sys_probes() -> Vec<(&'static str, Probe)> {
    let ins = |code: u32, k: u32, jt: u8, jf: u8| libc::sock_filter { code: code as u16, jt, jf, k };
    let allow = [ins(libc::BPF_RET | libc::BPF_K, libc::SECCOMP_RET_ALLOW, 0, 0)];
    // (`socket` to the filter's own listener, the rest allowed)
    let socket_notified = [
        ins(libc::BPF_LD | libc::BPF_W | libc::BPF_ABS, 0, 0, 0), // seccomp_data.nr
        ins(libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K, libc::SYS_socket as u32, 0, 1),
        ins(libc::BPF_RET | libc::BPF_K, libc::SECCOMP_RET_USER_NOTIF, 0, 0),
        ins(libc::BPF_RET | libc::BPF_K, libc::SECCOMP_RET_ALLOW, 0, 0),
    ];
    // A filter installed on the calling thread (no-new-privs first, as an unprivileged one needs): its
    // listener's descriptor with `NEW_LISTENER`, else 0; -1 refused.
    // SAFETY: called in a forked child only, on a filter of the caller's that outlives the call.
    let install = |filter: &[libc::sock_filter], flags: libc::c_ulong| unsafe {
        let prog = libc::sock_fprog { len: filter.len() as u16, filter: filter.as_ptr().cast_mut() };
        libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        libc::syscall(libc::SYS_seccomp, libc::SECCOMP_SET_MODE_FILTER, flags, &raw const prog)
    };
    let listening = libc::SECCOMP_FILTER_FLAG_NEW_LISTENER as libc::c_ulong;
    let mut params = [0u8; 120]; // (io_uring_params)
    // SAFETY (the five below): the child makes raw calls only, on what was made before the fork; a filter it
    // installs, a listener it gets, a ring it makes are its own and end with it.
    let new_listener = unsafe {
        forked(|| match install(&allow, listening) {
            fd if fd >= 0 => {
                libc::close(fd as libc::c_int);
                0
            }
            _ => errno(),
        })
    };
    // Precedence: the run's own listener's filter in front of `socket`, the listener closed (a call it would
    // get is answered ENOSYS), then a socket tarae refuses in every mode (AF_UNIX but a stream: EPERM) —
    // EPERM says tarae's answer won, ENOSYS the run's filter's
    let precedence = unsafe {
        forked(|| {
            let fd = install(&socket_notified, listening);
            if fd < 0 {
                return NOT_TRIED;
            }
            libc::close(fd as libc::c_int);
            match libc::socket(libc::AF_UNIX, libc::SOCK_DGRAM, 0) {
                s if s >= 0 => {
                    libc::close(s);
                    0
                }
                _ => errno(),
            }
        })
    };
    let plain_filter = unsafe { forked(|| if install(&allow, 0) == 0 { 0 } else { errno() }) };
    let io_uring = unsafe {
        forked(|| match libc::syscall(libc::SYS_io_uring_setup, 1u32, params.as_mut_ptr()) {
            fd if fd >= 0 => {
                libc::close(fd as libc::c_int);
                0
            }
            _ => errno(),
        })
    };
    #[cfg(target_arch = "x86_64")]
    let foreign_arch = unsafe {
        forked(|| {
            // (i386's getpid, 20: a pid, or -errno)
            let ret: i32;
            std::arch::asm!("int 0x80", inout("eax") 20i32 => ret, options(nostack, preserves_flags));
            if ret >= 0 { 0 } else { -ret }
        })
    };

    let mut header = CapHeader { version: CAP_VERSION_3, pid: 0 };
    let mut data = [CapData::default(); 2];
    // SAFETY: the header and the two data slots version 3 fills, ours.
    let got = unsafe { libc::syscall(libc::SYS_capget, &mut header, data.as_mut_ptr()) };
    assert_eq!(got, 0, "capget: {}", io::Error::last_os_error());
    // (`Done`: it has some — what a run must not)
    let caps = match data[0].effective | data[1].effective {
        0 => Probe::Refused(0),
        _ => Probe::Done,
    };
    // SAFETY: a getter.
    let nnp_off = match unsafe { libc::prctl(libc::PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0) } {
        0 => Probe::Done,
        _ => Probe::Refused(0),
    };
    let socket = |domain, ty, proto| match try_socket(domain, ty, proto) {
        Ok(()) => Probe::Done,
        Err(e) => Probe::Refused(e.raw_os_error().unwrap_or(0)),
    };
    let all = vec![
        ("new_listener", new_listener),
        ("precedence", precedence),
        ("plain_filter", plain_filter),
        ("io_uring", io_uring),
        ("caps", caps),
        ("nnp_off", nnp_off),
        ("packet", socket(libc::AF_PACKET, libc::SOCK_RAW, 0)),
        ("netlink_diag", socket(libc::AF_NETLINK, libc::SOCK_RAW, libc::NETLINK_SOCK_DIAG)),
    ];
    // (another ABI's calls only where there is one: x86_64's i386)
    #[cfg(target_arch = "x86_64")]
    let all = [all, vec![("foreign_arch", foreign_arch)]].concat();
    all
}

/// A part tarae answers `ENOSYS` (`To::Missing`: io_uring, another ABI's calls): `refused` only for that answer
/// where the baseline got through — another refusal there (`EPERM`) isn't tarae's: `other`.
#[cfg(target_os = "linux")]
fn missing_part(base: Probe, run: Probe) -> &'static str {
    match run {
        r if r.through() => "allowed",
        _ if !base.through() => "system",
        Probe::Refused(libc::ENOSYS) => "refused",
        _ => "other",
    }
}

#[cfg(target_os = "linux")]
fn check_linux_rows(
    home: &Path,
    root: &Path,
    own_run: &Path,
    stand_in_pty: &str,
    keeper_pid_str: &str,
    keeper_pidns: &str,
) {
    // Rows 21, 22: the baseline's calls (`ns_probes`), each said with its errno for the parent to judge
    for (name, got) in ns_probes(&root.join(".env"), &home.join(".ssh/id_ed25519"), &own_run.join("ns")) {
        emit(&format!("ns_{name}"), got.said());
    }

    // Rows 23, 24: the baseline's calls (`sys_probes`)
    for (name, got) in sys_probes() {
        emit(&format!("sys_{name}"), got.said());
    }

    // Row 25: the keeper — only once proven the test's own (`keeper_proven`: the run's parent by the number
    // runs see, and with the session's PID namespace this run in the keeper's own, where a fresh `/proc` is
    // mounted); else nothing is tried at any process. Signals: `kill(pid, 0)` only, never one that ends or stops.
    // SAFETY: a getter.
    let ppid = unsafe { libc::getppid() };
    let keeper: libc::pid_t = keeper_pid_str.parse().unwrap_or(0);
    let own_pidns = fs::read_link("/proc/self/ns/pid").ok().map(|l| l.display().to_string());
    let keeper_pidns = Some(keeper_pidns).filter(|ns| !ns.is_empty());
    let parent_is_keeper = keeper_proven(ppid, keeper, keeper_pidns, own_pidns.as_deref());
    emit("r25_parent_is_keeper", parent_is_keeper);
    let signals = match parent_is_keeper {
        // SAFETY: signal 0 — a permission check, nothing sent — at the keeper proven the test's own.
        true => match unsafe { libc::kill(ppid, 0) } {
            0 => Probe::Done,
            _ => Probe::Refused(errno()),
        },
        false => Probe::NotTried,
    };
    emit("r25_signals", signals.said());
    for (name, got) in keeper_probes(parent_is_keeper.then_some(ppid), false) {
        emit(&format!("r25_{name}"), got.said());
    }

    // Row 26: a terminal of the test's own is this process's ctty unless tarae's `setsid` dropped it
    // (`must_be_refused`), and another session's pty (`PtyGuard`)
    emit("r26_ctty", open_probe(c"/dev/tty").said());
    emit("r26_other_pty", open_probe(&CString::new(stand_in_pty).unwrap()).said());
}

/// `capget`/`capset`'s header and data (`_LINUX_CAPABILITY_VERSION_3`: two data slots).
#[cfg(target_os = "linux")]
#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Default, Copy, Clone)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}
#[cfg(target_os = "linux")]
const CAP_VERSION_3: u32 = 0x2008_0522;

/// Row 25's `KEEPER_PROBES` at `target`, one set for both sides: the baseline's at a `BareTarget`, the run's at
/// its keeper, proven the test's own first — never at any other process; the caller is never the target's
/// ancestor either way (what Yama's scope 1 asks). `None`: not tried. Each in a forked child: a seize that got through ends
/// with that child (the kernel detaches it — nothing stopped, nothing to resume); the `/proc` files are opened
/// where the kernel checks, nothing read (`mem` at 0 is never mapped anyway). `bare`: the child first drops
/// every capability, as a run has none — the baseline's: the test container's `CAP_SYS_PTRACE` (`cargo xtask
/// linux`) takes a call past Yama, the capability rule and the dumpable check alike. Not dropped: `NotTried`.
#[cfg(target_os = "linux")]
fn keeper_probes(target: Option<libc::pid_t>, bare: bool) -> [(&'static str, Probe); 6] {
    let Some(target) = target else {
        return KEEPER_PROBES.map(|n| (n, Probe::NotTried));
    };
    let at = |f: &str| CString::new(format!("/proc/{target}/{f}")).unwrap();
    let (mem, environ, fd0, root) = (at("mem"), at("environ"), at("fd/0"), at("root"));
    let header = CapHeader { version: CAP_VERSION_3, pid: 0 };
    let none = [CapData::default(); 2];
    // SAFETY: called in a forked child only: a capset of the child's own on what was made before the fork.
    let dropped =
        || !bare || unsafe { libc::syscall(libc::SYS_capset, &raw const header, none.as_ptr()) } == 0;
    let ok = |r: libc::c_long| if r >= 0 { 0 } else { errno() };
    // SAFETY (the six below): the child makes raw calls only, at `target` (the caller's proven own process), on
    // what was made before the fork; what it gets is closed before it ends.
    let probe = |f: &dyn Fn() -> i32| unsafe { forked(|| if dropped() { f() } else { NOT_TRIED }) };
    let open = |path: &CString, flags: libc::c_int| unsafe {
        match libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) {
            fd if fd >= 0 => {
                libc::close(fd);
                0
            }
            _ => errno(),
        }
    };
    let ptrace = probe(&|| unsafe { ok(libc::ptrace(libc::PTRACE_SEIZE, target, 0usize, 0usize)) });
    let pidfd = probe(&|| unsafe {
        let pfd = libc::syscall(libc::SYS_pidfd_open, target, 0usize);
        if pfd < 0 {
            return errno();
        }
        let fd = libc::syscall(libc::SYS_pidfd_getfd, pfd as libc::c_int, 0usize, 0usize);
        let e = errno();
        if fd >= 0 {
            libc::close(fd as libc::c_int);
        }
        libc::close(pfd as libc::c_int);
        if fd < 0 { e } else { 0 }
    });
    let mem = probe(&|| open(&mem, libc::O_RDONLY));
    let environ = probe(&|| open(&environ, libc::O_RDONLY));
    let fd = probe(&|| {
        let mut buf = [0u8; 256];
        unsafe { ok(libc::readlink(fd0.as_ptr(), buf.as_mut_ptr().cast(), buf.len()) as libc::c_long) }
    });
    // (followed — the kernel's check is there —, nothing listed)
    let root = probe(&|| open(&root, libc::O_RDONLY | libc::O_DIRECTORY));
    [("ptrace", ptrace), ("pidfd", pidfd), ("mem", mem), ("environ", environ), ("fd", fd), ("root", root)]
}

/// Row 26's opens, one for both sides: `path` read-write without taking it as a ctty (`O_NOCTTY`), closed at once.
#[cfg(target_os = "linux")]
fn open_probe(path: &std::ffi::CStr) -> Probe {
    // SAFETY: an open of a C string and the close of what it gave.
    unsafe {
        match libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY | libc::O_CLOEXEC) {
            fd if fd >= 0 => {
                libc::close(fd);
                Probe::Done
            }
            _ => Probe::Refused(errno()),
        }
    }
}

/// Row 25's keeper as a run must find it (`keeper_proven`), read from the keeper itself: the number its runs
/// see (`keeper::seen_by_runs`) and, with the session's PID namespace, that namespace (its `/proc/<pid>/ns/pid`)
/// — only where its `NSpid` agrees with the policy (`keeper_in_pidns`: that namespace's PID 1, or one number in
/// the test's own). Else `("0", "")`: not proven, nothing is tried, and the row fails.
#[cfg(target_os = "linux")]
fn keeper_seen(k: &super::keeper::Keeper) -> (String, String) {
    let status = fs::read_to_string(format!("/proc/{}/status", k.pid)).unwrap_or_default();
    let seen = super::keeper::seen_by_runs(k).to_string();
    match (keeper_in_pidns(&status), k.ns.pid) {
        (Some(false), false) => (seen, String::new()),
        (Some(true), true) => match fs::read_link(format!("/proc/{}/ns/pid", k.pid)) {
            Ok(ns) => (seen, ns.display().to_string()),
            Err(_) => ("0".into(), String::new()),
        },
        _ => ("0".into(), String::new()),
    }
}

/// Row 25's baseline target: a forked child of the test's own with no capabilities, as the keeper has none
/// (`keeper::main` drops them) — a prober with none is refused any target that has one, whatever else holds —
/// but dumpable, in no sandbox: what a run is refused at its keeper past that is tarae's. It waits on a pipe
/// of its own (close-on-exec: no run ever holds it) and ends when `Drop` closes it; never signalled.
#[cfg(target_os = "linux")]
struct BareTarget {
    pid: libc::pid_t,
    hold: libc::c_int,
}

#[cfg(target_os = "linux")]
impl BareTarget {
    /// Still there: probes at a gone target would read as the system's refusal.
    fn alive(&self) -> bool {
        // SAFETY: its own child, not reaped while running.
        unsafe { libc::waitpid(self.pid, std::ptr::null_mut(), libc::WNOHANG) == 0 }
    }

    fn new() -> BareTarget {
        let (mut hold, mut ready) = ([0; 2], [0; 2]);
        let header = CapHeader { version: CAP_VERSION_3, pid: 0 };
        let none = [CapData::default(); 2];
        // SAFETY: two pipes of this process's own; the child makes raw calls only (a capset of its own, its
        // pipes' ends), on what was made before the fork, and `_exit`s.
        unsafe {
            for p in [&mut hold, &mut ready] {
                assert_eq!(
                    libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC),
                    0,
                    "pipe2: {}",
                    io::Error::last_os_error()
                );
            }
            let pid = libc::fork();
            assert!(pid >= 0, "fork: {}", io::Error::last_os_error());
            if pid == 0 {
                libc::close(hold[1]);
                libc::close(ready[0]);
                let dropped =
                    u8::from(libc::syscall(libc::SYS_capset, &raw const header, none.as_ptr()) == 0);
                libc::write(ready[1], (&raw const dropped).cast(), 1);
                libc::close(ready[1]);
                let mut b = 0u8;
                // (until the pipe is closed: a signal's EINTR waits on)
                loop {
                    match libc::read(hold[0], (&raw mut b).cast(), 1) {
                        n if n > 0 => {}
                        -1 if errno() == libc::EINTR => {}
                        _ => break,
                    }
                }
                libc::_exit(0);
            }
            libc::close(hold[0]);
            libc::close(ready[1]);
            let mut dropped = 0u8;
            let got = libc::read(ready[0], (&raw mut dropped).cast(), 1);
            libc::close(ready[0]);
            let target = BareTarget { pid, hold: hold[1] };
            assert!(got == 1 && dropped == 1, "the baseline's target couldn't drop its capabilities");
            target
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for BareTarget {
    fn drop(&mut self) {
        // SAFETY: the pipe's end this holds, closed once; then its own child, reaped.
        unsafe {
            libc::close(self.hold);
            libc::waitpid(self.pid, std::ptr::null_mut(), 0);
        }
    }
}
