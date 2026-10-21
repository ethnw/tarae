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
    #[cfg(target_os = "macos")]
    if skipped_inside("sandbox::refused::must_be_refused") {
        return;
    }
    let scratch = crate::scratch::Scratch::in_dir(Path::new("/var/tmp"), "refused");
    let base = profile_path(&scratch);
    let home = base.join("h");
    for d in [".config", ".local/state", ".local/share", ".cache"] {
        fs::create_dir_all(home.join(d)).unwrap();
    }
    let exe = std::env::current_exe().unwrap();
    let out = Command::new(&exe)
        .args(["--exact", "sandbox::refused::in_stand_in_home", "--ignored", "--nocapture"])
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_STATE_HOME", home.join(".local/state"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("XDG_CACHE_HOME", home.join(".cache"))
        .env("TARAE_REFUSED_BASE", &base)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "in_stand_in_home failed:\nstdout:\n{stdout}\nstderr:\n{stderr}");
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
    let mut here = Here {
        tmp: Some(shared_tmp.clone()),
        runs: Some(runs.clone()),
        ..Here::now()
    };
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
    let policy = run_policy_in(&root, &llm, &[], &here).unwrap();
    let _policy_cleanup = RunCleanupGuard { policy: policy.clone() };
    let other_policy = run_policy_in(&other_root, &llm, &[], &here).unwrap();
    let own_run = policy.run.as_ref().unwrap().path().to_path_buf();
    let other_run = other_policy.run.as_ref().unwrap().path().to_path_buf();

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
    let _unix_listeners: Vec<UnixListener> = [
        &sock_other_run,
        &sock_state,
        &sock_outside,
        &sock_tmp,
        &sock_own_root,
        &sock_own_run,
    ]
    .into_iter()
    .map(|p| UnixListener::bind(p).unwrap())
    .collect();

    // Stand-in processes for row 17: tarae stand-in (outside sandbox) and other session's sandboxed stand-in
    let exe = std::env::current_exe().unwrap();
    let tarae_guard = spawn_stand_in(&exe, None, &root, "tarae-stand-in");
    let other_guard = spawn_sandboxed_stand_in(&exe, &other_policy, "other-stand-in");

    // Shared memory objects for row 18 (created by this test process, standing in for another session)
    let pid = std::process::id();
    let posix_name = format!("/tarae-ref-{pid}");
    let sysv_key = 0x7900_0000 | (pid as libc::key_t & 0x00ff_ffff);
    let shm_guard = ShmGuard::new(&posix_name, sysv_key);

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
    ];
    let cfg_file = own_run.join("refused.cfg");
    let cfg_text: String = cfg.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    write_file(&cfg_file, &cfg_text);

    // Run the main sandboxed helper covering rows 1–7, 9–13, 16–20
    let (main_out, main_leaked) = run_helper(&exe, &policy, "main", &cfg_file);
    let got = parse_kv(&main_out);

    // Clean up stand-ins and shm early (or on Drop if an earlier step panicked)
    drop(tarae_guard);
    drop(other_guard);
    drop(shm_guard);
    fs::remove_file(&out_link).ok();
    fs::remove_file(&sock_own_root).ok();
    if root.join("sub2").exists() && !root.join("sub").exists() {
        fs::rename(root.join("sub2"), root.join("sub")).unwrap();
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

    // Row 14: proxy unlisted host (`refused.invalid:443`), other session's proxy port, other session's token
    let (proxy_got, proxy_leaked) = run_proxy_row(&exe, &root, &other_root, &here);

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
        witness: format!(
            "write-after-move={}, unlink-after-move={}",
            word(r7_write),
            word(r7_unlink)
        ),
        detail: got.get("r7_err").cloned().unwrap_or_default(),
        part: None,
        shared_phrase: None,
    });

    // 8. 1.2 | move-in-from-temp
    let r8_write = move_got["r8_write"] == "refused";
    let r8_ok = r8_write && flagged_move;
    rows.push(Row {
        claim: "1.2",
        id: "move-in-from-temp",
        expected: Verdict::Caught,
        verdict: if r8_ok { Verdict::Caught } else { Verdict::Allowed },
        witness: format!(
            "write-in-root={}, snapshot-flagged={}",
            word(r8_write),
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
    let r12_loop = got["r12_loop"] == "refused" && udp_loop_recv == 0;
    let r12_dns = got["r12_dns"] == "refused" && udp_dns_recv == 0;
    let r12_zero = got["r12_zero"] == "refused";
    let r12_ok = r12_loop && r12_dns && r12_zero;
    rows.push(Row {
        claim: "1.5",
        id: "udp-sendto",
        expected: Verdict::Refused,
        verdict: if r12_ok { Verdict::Refused } else { Verdict::Allowed },
        witness: format!(
            "loopback={}, resolver={}, port0={}",
            word(r12_loop),
            word(r12_dns),
            word(r12_zero)
        ),
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

    // 14. 1.5 | proxy-unlisted-and-other-token
    let r14_unlisted = proxy_got["r14_unlisted"] == "403";
    let r14_other_port = proxy_got["r14_other_port"] == "refused";
    let r14_other_tok = proxy_got["r14_other_token"] == "407";
    let r14_ok = r14_unlisted && r14_other_port && r14_other_tok;
    rows.push(Row {
        claim: "1.5",
        id: "proxy-unlisted-and-other-token",
        expected: Verdict::Refused,
        verdict: if r14_ok { Verdict::Refused } else { Verdict::Allowed },
        witness: format!(
            "unlisted-host={}, other-proxy-port={}, other-proxy-token={}",
            proxy_got["r14_unlisted"],
            proxy_got["r14_other_port"],
            proxy_got["r14_other_token"]
        ),
        detail: proxy_got.get("r14_err").cloned().unwrap_or_default(),
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
    let r16_own_root = got["r16_own_root"] == "ok";
    let r16_own_run = got["r16_own_run"] == "ok";
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
    let r17_signals = ["tarae_kill0", "tarae_cont", "other_kill0", "other_cont"]
        .iter()
        .all(|k| got[*k] == "refused");
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
    let r17_child = got["own_child"] == "ok";
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

    // 18. 1.6 | sysv-and-posix-shm
    let r18_sysv = got["r18_sysv"] == "refused";
    let r18_posix = got["r18_posix"] == "shared";
    let r18_verdict = match (r18_sysv, r18_posix) {
        (true, true) => Verdict::Shared,
        (true, false) => Verdict::Refused,
        _ => Verdict::Allowed,
    };
    rows.push(Row {
        claim: "1.6",
        id: "sysv-and-posix-shm",
        expected: Verdict::Shared,
        verdict: r18_verdict,
        witness: format!("sysv={}, posix={}", got["r18_sysv"], got["r18_posix"]),
        detail: got.get("r18_err").cloned().unwrap_or_default(),
        part: Some((Part::Shm, Some(r18_posix))),
        shared_phrase: Some("shares POSIX shared memory with other sessions' commands"),
    });

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
    let g = policy.guarantees().expect("Seatbelt policy has guarantees");
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
    let mut table = String::from(
        "# must-be-refused suite (macOS Seatbelt)\n\
         # claim | id | verdict | witness\n",
    );
    for r in &rows {
        table.push_str(&format!("{:<8} | {:<30} | {:<8} | {}\n", r.claim, r.id, r.verdict.as_str(), r.witness));
    }
    assert!(wrong.is_empty(), "must-be-refused failures:\n{}\n\n{table}", wrong.join("\n"));
    compare_snapshot("refused_macos", &table);
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
    assert_eq!(
        expected,
        actual,
        "snapshot {name} differs ({}) — if intended: `TARAE_BLESS=1 cargo test sandbox::refused`",
        path.display()
    );
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

impl Drop for SandboxedStandInGuard {
    fn drop(&mut self) {
        // SAFETY: `self.pid` is the sandboxed stand-in spawned by this guard.
        unsafe { libc::kill(self.pid as libc::pid_t, libc::SIGKILL) };
        crate::spawn::wait(self.pid).ok();
        after_run(Some(&self.policy))();
    }
}

struct RunCleanupGuard {
    policy: Policy,
}

impl Drop for RunCleanupGuard {
    fn drop(&mut self) {
        after_run(Some(&self.policy))();
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
        // SAFETY: creating a POSIX shared memory object and a SysV segment owned by this test, cleaned up on Drop.
        let (posix_fd, sysv_id) = unsafe {
            let fd = libc::shm_open(
                posix_name.as_ptr(),
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                0o600 as libc::c_uint,
            );
            let id = libc::shmget(sysv_key, 4096, libc::IPC_CREAT | libc::IPC_EXCL | 0o600);
            (fd, id)
        };
        let err = io::Error::last_os_error();
        let guard = Self { posix_fd, posix_name, sysv_id };
        assert!(guard.posix_fd >= 0 && guard.sysv_id >= 0, "shm setup failed: {err}");
        guard
    }
}

impl Drop for ShmGuard {
    fn drop(&mut self) {
        // SAFETY: closing and unlinking the test's own POSIX shm and SysV segment if created.
        unsafe {
            if self.posix_fd >= 0 {
                libc::close(self.posix_fd);
            }
            libc::shm_unlink(self.posix_name.as_ptr());
            if self.sysv_id >= 0 {
                libc::shmctl(self.sysv_id, libc::IPC_RMID, std::ptr::null_mut());
            }
        }
    }
}

fn spawn_stand_in(_exe: &Path, policy: Option<&Policy>, cwd: &Path, marker: &str) -> StandInGuard {
    use std::os::unix::process::CommandExt;
    let args = ["-c".to_string(), "printf 'ready\\n'; sleep 60".to_string(), format!("{marker}-arg")];
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
    let args = ["-c".to_string(), "printf 'ready\\n'; sleep 60".to_string(), format!("{marker}-arg")];
    let mut cmd = command(Some(policy), "/bin/sh", &args);
    cmd.current_dir(&policy.root)
        .env("TARAE_REFUSED_STAND_IN", format!("{marker}-env"))
        .stdin(Stdio::null());
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
    let out = cmd.output().unwrap();
    let text =
        String::from_utf8_lossy(&out.stdout).into_owned() + &String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "in_sandbox ({mode}) failed:\n{text}");
    let leaked = text.contains(FAKE_TOKEN);
    (text, leaked)
}

fn run_proxy_row(
    exe: &Path,
    root: &Path,
    other_root: &Path,
    here: &Here,
) -> (HashMap<String, String>, bool) {
    let net_llm = crate::llm::LlmConfig { sandbox_network: true, ..Default::default() };
    let net_a = run_policy_in(root, &net_llm, &[], here).unwrap();
    let _net_a_cleanup = RunCleanupGuard { policy: net_a.clone() };
    let net_b = run_policy_in(other_root, &net_llm, &[], here).unwrap();
    let url_a = net_a.proxy_url().expect("net_a proxy url");
    let url_b = net_b.proxy_url().expect("net_b proxy url");

    // Start net_b's proxy listener by spawning a short stand-in in net_b (cleaned up on Drop)
    let _b_guard = spawn_sandboxed_stand_in(exe, &net_b, "net-b");

    // Register TEST_ASKS for net_a's session and pre-answer `refused.invalid:443` with Scope::Refused when asked
    let (asks, asked) = std::sync::mpsc::channel();
    let session = super::proxy::session_of(root);
    super::proxy::TEST_ASKS.lock().unwrap().push((session, asks));
    let answer_thread = std::thread::spawn(move || {
        if let Ok(FromProxy::Ask { run, host, port, .. }) = asked.recv_timeout(Duration::from_secs(10)) {
            super::proxy::get().send(ToProxy::Answer { session, run, host, port, scope: Scope::Refused });
        }
    });

    let cfg_file = net_a.run.as_ref().unwrap().path().join("proxy.cfg");
    let cfg_text = format!(
        "PORT_A={}\nTOKEN_A={}\nPORT_B={}\nTOKEN_B={}\n",
        url_a.port,
        url_a.password.as_deref().unwrap_or_default(),
        url_b.port,
        url_b.password.as_deref().unwrap_or_default(),
    );
    write_file(&cfg_file, &cfg_text);

    let args = [
        "--exact".to_string(),
        "sandbox::refused::in_sandbox".to_string(),
        "--ignored".to_string(),
        "--nocapture".to_string(),
    ];
    let mut cmd = command(Some(&net_a), &exe.to_string_lossy(), &args);
    cmd.current_dir(root)
        .env("TARAE_REFUSED_MODE", "proxy")
        .env("TARAE_REFUSED_CFG", &cfg_file)
        .stdin(Stdio::null());
    let s = spawn(Some(&net_a), &cmd).unwrap();
    let mut out = String::new();
    io::Read::read_to_string(&mut s.stdout.unwrap(), &mut out).unwrap();
    io::Read::read_to_string(&mut s.stderr.unwrap(), &mut out).unwrap();
    let status = crate::spawn::wait(s.pid).unwrap();
    drop(_net_a_cleanup);
    let _ = answer_thread.join();
    drop(_b_guard);

    assert!(status.success(), "in_sandbox (proxy) failed:\n{out}");
    let leaked = out.contains(FAKE_TOKEN);
    (parse_kv(&out), leaked)
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
    let cfg: HashMap<&str, &str> =
        cfg_text.lines().filter_map(|l| l.split_once('=')).collect();
    match mode.as_str() {
        "main" => sandbox_main(&cfg),
        "move-in" => sandbox_move_in(&cfg),
        "proxy" => sandbox_proxy(&cfg),
        other => panic!("unknown mode: {other}"),
    }
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

    // Row 5: make new protected names (including case variant and symlink)
    record_refused(
        "r5",
        &[
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
    record_refused(
        "r7",
        &[("write", try_write(&target)), ("unlink", fs::remove_file(&target))],
    );
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
    let copy_git = std::env::var_os("GIT_CONFIG_GLOBAL")
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default();
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
    emit(
        "r16_own_root",
        if UnixStream::connect(root.join("own.sock")).is_ok() { "ok" } else { "refused" },
    );
    emit(
        "r16_own_run",
        if UnixStream::connect(own_run.join("own.sock")).is_ok() { "ok" } else { "refused" },
    );

    // Row 17: process signals, ptrace, task_for_pid, proc_pidinfo, KERN_PROCARGS2, setpriority (no lldb)
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
        nested
            .map(|o| format!("code={:?}", o.status.code()))
            .unwrap_or_else(|e| format!("err={e}")),
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
    // Once inside the project root, writing `crates/x/build.rs` must be refused by Seatbelt
    match try_append(&dest.join("build.rs")) {
        Ok(()) => emit("r8_write", "allowed"),
        Err(e) => {
            emit("r8_write", "refused");
            emit("r8_err", format!("{:?}", e.raw_os_error()));
        }
    }
}

fn sandbox_proxy(cfg: &HashMap<&str, &str>) {
    let port_a: u16 = cfg["PORT_A"].parse().unwrap();
    let token_a = cfg["TOKEN_A"];
    let port_b: u16 = cfg["PORT_B"].parse().unwrap();
    let token_b = cfg["TOKEN_B"];

    // 1. Unlisted host `refused.invalid:443` through own proxy port with own token -> 403
    let auth_a = base64_basic(&format!("run:{token_a}"));
    let status_unlisted = proxy_connect(port_a, "refused.invalid:443", &auth_a)
        .unwrap_or_else(|e| format!("err:{:?}", e.raw_os_error()));
    emit("r14_unlisted", status_unlisted);

    // 2. Connect to other session's proxy port -> refused by Seatbelt
    let addr_b = SocketAddr::from(([127, 0, 0, 1], port_b));
    match TcpStream::connect_timeout(&addr_b, Duration::from_millis(500)) {
        Ok(_) => emit("r14_other_port", "allowed"),
        Err(e) => {
            emit("r14_other_port", "refused");
            emit("r14_err", format!("{:?}", e.raw_os_error()));
        }
    }

    // 3. Connect to own proxy port with other session's token -> 407
    let auth_b = base64_basic(&format!("run:{token_b}"));
    let status_bad_tok = proxy_connect(port_a, "refused.invalid:443", &auth_b)
        .unwrap_or_else(|e| format!("err:{:?}", e.raw_os_error()));
    emit("r14_other_token", status_bad_tok);
}

fn proxy_connect(port: u16, authority: &str, basic: &str) -> io::Result<String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
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

#[cfg(target_os = "macos")]
fn pid_kill0(pid: libc::pid_t) -> io::Result<()> {
    // SAFETY: signal 0 only probes whether the process can be signalled.
    if unsafe { libc::kill(pid, 0) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(target_os = "macos")]
fn pid_cont(pid: libc::pid_t) -> io::Result<()> {
    // SAFETY: SIGCONT is harmless to a sleeping stand-in process owned by this test.
    if unsafe { libc::kill(pid, libc::SIGCONT) } == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
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
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
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
fn pid_prio(pid: libc::pid_t) -> io::Result<()> {
    // SAFETY: `setpriority` on our own stand-in process.
    if unsafe { libc::setpriority(libc::PRIO_PROCESS, pid as libc::id_t, 7) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
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
        fn mach_port_deallocate(
            task: libc::mach_port_t,
            name: libc::mach_port_t,
        ) -> libc::kern_return_t;
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
