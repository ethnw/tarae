//! End-to-end smoke tests: the real `tarae` binary inside a pseudo-terminal.
//!
//! Each test gets its own sandbox (HOME, XDG_*, CLAUDE_CONFIG_DIR all point into a fresh temp dir, the
//! environment is cleared otherwise), starts tarae on a pty, types keys, and reads the screen back through a
//! `vt100` parser. Nothing here needs the network, claude, a language server or a debugger.
//!
//! Rules that keep these deterministic:
//! - Never sleep and hope — every key that matters is followed by `wait_for` on its visible effect.
//! - Esc goes out alone and is followed by a wait: crossterm reads `ESC x` arriving in one read as Alt-x.
//! - Keys are sent only after the first frame (raw mode is on by then — no line-discipline echo).
#![cfg(unix)]
#![allow(clippy::disallowed_methods, reason = "tests: not in the release binary (clippy.toml)")]

mod harness;

use std::fs;
use std::process::{Command, Stdio};
use std::time::Duration;

use harness::*;

// ── Tests ────────────────────────────────────────────────────────────────────

/// The shim (`tarae --nofile N <absolute path> <argv0> args…`, nofile.rs): the program gets soft limit N and the
/// `argv[0]` given, and runs as this process; anything else of that shape is refused with an exit and the usage —
/// never run as the editor — and a program that can't run is said, as a shell says it (127). (A guard ends one
/// that hangs, failing.)
#[test]
fn the_nofile_shim_gives_its_program_the_limit_and_refuses_any_other_shape() {
    let run = |args: &[&str]| {
        let mut child = Command::new(BIN)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let end = std::time::Instant::now() + TIMEOUT;
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() > end {
                child.kill().ok();
                panic!("tarae {args:?} didn't end — run as the editor?");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let out = child.wait_with_output().unwrap();
        let text = |b: &[u8]| String::from_utf8_lossy(b).trim().to_string();
        (out.status.code(), text(&out.stdout), text(&out.stderr))
    };
    let args = ["--nofile", "256", "/bin/sh", "sh", "-c", "echo $(ulimit -Sn) $0 $1", "one", "two"];
    let (code, out, _) = run(&args);
    assert_eq!(
        (code, out.as_str()),
        (Some(0), "256 one two"),
        "the limit, then the program with its arguments"
    );
    // (its argv[0] as the process has it: a shell reading its commands from stdin says it as `$0`)
    let mut sh = Command::new(BIN)
        .args(["--nofile", "256", "/bin/sh", "named-sh", "-s"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    std::io::Write::write_all(&mut sh.stdin.take().unwrap(), b"echo \"$0\"\n").unwrap();
    let out = sh.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "named-sh", "the argv[0] given, not the path");
    for shape in [
        &["--nofile"][..],
        &["--nofile", "256"],
        &["--nofile", "256", "/bin/sh"],
        &["--nofile", "many", "/bin/sh", "sh", "-c", "true"],
        &["--nofile", "-1", "/bin/sh", "sh", "-c", "true"],
        &["--nofile", "256", "sh", "sh", "-c", "true"],
    ] {
        let (code, out, err) = run(shape);
        assert_eq!(code, Some(2), "{shape:?} refused: {out} {err}");
        assert!(err.contains("usage: tarae --nofile N <absolute path> <argv0>"), "{shape:?}: {err}");
    }
    let (code, _, err) = run(&["--nofile", "256", "/nonexistent/tarae-shim-test", "tarae-shim-test"]);
    assert_eq!(code, Some(127), "{err}");
    assert!(err.contains("/nonexistent/tarae-shim-test"), "{err}");
}

/// Started under a low soft limit on open descriptors, tarae raises its own (said once in its log, from → to) and
/// what it starts gets the old one back — `:sh` here (through the shim, posix_spawn's).
#[test]
fn tarae_raises_its_descriptor_limit_and_sh_gets_the_old_one() {
    let sb = Sandbox::new("nofile").with_soft_nofile(256);
    sb.write("a.txt", "hi\n");
    let mut t = sb.spawn(&["a.txt"]);
    t.wait_text("hi");
    t.command("sh echo nofile=$(ulimit -Sn)");
    t.wait_text("nofile=256");
    let log = sb.state().join("tarae.log");
    let end = std::time::Instant::now() + sb.timeout();
    let raised = loop {
        let text = fs::read_to_string(&log).unwrap_or_default();
        if let Some(line) = text.lines().find(|l| l.contains("open descriptors:")) {
            break line.to_string();
        }
        assert!(std::time::Instant::now() < end, "never said in {}", log.display());
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(raised.contains("raised 256 →") && raised.contains("gets 256 back"), "{raised}");
    // (macOS: the editor pins its binary for the shim — what the subcommands below must not do)
    if cfg!(target_os = "macos") {
        assert!(sb.state().join(format!("shims/{}", t.pid())).is_file(), "the editor's pin");
    }
}

/// A subcommand that ends at once (`state`, `--version`, `grammar list`), started under a low soft limit, raises
/// nothing and pins nothing — only the editor does: a pin made by each `tarae state` a driver runs every few
/// seconds would pile up, the prune (the editor's, on a worker) never run. (Pins are macOS's: elsewhere there's
/// none to leave, and this holds trivially.)
#[test]
fn a_subcommand_under_a_low_limit_leaves_no_pin() {
    use std::os::unix::process::CommandExt as _;
    let sb = Sandbox::new("nofile-sub");
    let shims = sb.state().join("shims");
    for args in [&["state"][..], &["--version"], &["grammar", "list"]] {
        let mut c = Command::new(BIN);
        c.args(args)
            .env_clear()
            .envs(sb.env())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: getrlimit/setrlimit only, between fork and exec.
        unsafe {
            c.pre_exec(|| {
                let mut l = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
                cvt(libc::getrlimit(libc::RLIMIT_NOFILE, &mut l))?;
                l.rlim_cur = 256;
                cvt(libc::setrlimit(libc::RLIMIT_NOFILE, &l))?;
                Ok(())
            });
        }
        let mut child = c.spawn().unwrap();
        let end = std::time::Instant::now() + TIMEOUT;
        while child.try_wait().unwrap().is_none() {
            if std::time::Instant::now() > end {
                child.kill().ok();
                panic!("tarae {args:?} didn't end");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let left: Vec<_> =
            fs::read_dir(&shims).map(|d| d.flatten().map(|e| e.file_name()).collect()).unwrap_or_default();
        assert!(left.is_empty(), "tarae {args:?} left {left:?} in {}", shims.display());
    }
}

/// `tarae state … | head -3`: the reader closes its end early, and every way `tarae state` prints (the text,
/// `--json`, `--watch`, `--frame FILE`'s line, `--help`) ends quietly — exit 0, nothing on stderr — where `println!` panicked on the EPIPE
/// (#229). A stand-in tarae answers on a socket of the test's own (`TARAE_UI`); tarae's stdout is a pipe
/// whose read end is closed before it starts — made in the child (`pre_exec`): made here, a fork of another
/// test's between `pipe` and `close` kept the read end alive in that child, and `--watch` never ended.
#[test]
fn tarae_state_ends_quietly_when_its_reader_is_gone() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixListener;
    use std::os::unix::process::CommandExt;
    let sb = Sandbox::new("epipe");
    // (a path that fits `sun_path`, wherever the temp folder is)
    let sock = sb.socket("u.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    std::thread::spawn(move || {
        for mut s in listener.incoming().map_while(Result::ok) {
            let mut req = String::new();
            BufReader::new(&s).read_line(&mut req).ok();
            let watching = req.contains("\"watch\"");
            let answer = match (watching, req.contains(r#""get":"frame""#)) {
                (true, _) => r#"{"frame":1,"said":"a key"}"#,
                (false, true) => r#"{"frame":{"cols":2,"rows":1,"frames":1,"bytes":"ab"}}"#,
                (false, false) => r#"{"text":"one\ntwo\nthree\nfour\n","mode":"NOR"}"#,
            };
            // A watcher's lines keep coming until it hangs up; an answer is one line.
            while writeln!(s, "{answer}").is_ok() && watching {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    });
    let frame = sb.home().join("frame.txt");
    let frame_arg = frame.to_str().unwrap();
    for args in
        [&[][..], &["--json"], &["--watch"], &["--json", "--watch"], &["--frame", frame_arg], &["--help"]]
    {
        let mut cmd = Command::new(BIN);
        cmd.arg("state")
            .args(args)
            .env_clear()
            .envs(sb.env())
            .env("TARAE_UI", &sock)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // SAFETY: raw calls only, in the child between fork and exec: a pipe whose read end is closed, its
        // write end the child's stdout — no other process ever holds the read end.
        unsafe {
            cmd.pre_exec(|| {
                let mut fds = [0; 2];
                let made = libc::pipe(fds.as_mut_ptr()) == 0;
                if !made || libc::close(fds[0]) != 0 || libc::dup2(fds[1], 1) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                libc::close(fds[1]);
                Ok(())
            })
        };
        let mut child = cmd.spawn().unwrap();
        let end = std::time::Instant::now() + TIMEOUT;
        // (ended, not reaped yet: a signal's sender is read from it first. A diagnostic, not a fix — flaky80 saw a
        // SIGKILL 1 run in 10 and didn't find its sender; it can still come, and on Linux this names no one)
        while !ended(child.id()) {
            if std::time::Instant::now() > end {
                child.kill().ok();
                panic!("tarae state {args:?} didn't end with its reader gone");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let sender = signal_sender(child.id());
        let status = child.wait().unwrap();
        let mut said = String::new();
        std::io::Read::read_to_string(&mut child.stderr.take().unwrap(), &mut said).unwrap();
        assert!(status.success() && said.is_empty(), "tarae state {args:?}: {status}{sender}: {said}");
    }
    // (the frame was written before its line went nowhere)
    assert_eq!(fs::read_to_string(&frame).unwrap(), "ab");
}

/// Whether child `pid` has ended — left unreaped (`WNOWAIT`): `signal_sender` reads it before `wait` does.
fn ended(pid: u32) -> bool {
    // SAFETY: a zeroed siginfo_t is one to fill; waitid only looks at our own child, reaping nothing.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let flags = libc::WEXITED | libc::WNOHANG | libc::WNOWAIT;
    // SAFETY: as above.
    let r = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags) };
    #[cfg(target_os = "macos")]
    let from = info.si_pid;
    // SAFETY: filled by waitid (zero while it runs).
    #[cfg(not(target_os = "macos"))]
    let from = unsafe { info.si_pid() };
    r == 0 && from == pid as libc::pid_t
}

/// Who killed ended child `pid`, said after its status: macOS keeps a signal's sender — its pid and name — in
/// the exit reason of a process killed by kill(2), read from it until it's reaped (`PROC_PIDEXITREASONINFO`, its
/// reason's kcdata). Nothing for one that wasn't signalled, or elsewhere.
fn signal_sender(pid: u32) -> String {
    #[cfg(target_os = "macos")]
    {
        // (XNU's `proc_exitreasoninfo`, packed)
        #[repr(C, packed)]
        struct ExitReason {
            namespace: u32,
            code: u64,
            flags: u64,
            size: u32,
            buf: u64,
        }
        unsafe extern "C" {
            fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut libc::c_void, size: i32) -> i32;
        }
        const PROC_PIDEXITREASONINFO: i32 = 24;
        const OS_REASON_SIGNAL: u32 = 2;
        let (kcdata_pid, kcdata_procname) = (0x36, 0x37);
        let mut kcd = [0u8; 2048];
        let mut r = ExitReason {
            namespace: 0,
            code: 0,
            flags: 0,
            size: kcd.len() as u32,
            buf: kcd.as_mut_ptr() as u64,
        };
        let size = std::mem::size_of::<ExitReason>() as i32;
        // SAFETY: fills one ExitReason and up to `size` bytes of our own buffer it points at.
        let n = unsafe { proc_pidinfo(pid as i32, PROC_PIDEXITREASONINFO, 0, (&raw mut r).cast(), size) };
        let (namespace, kcd_size) = (r.namespace, r.size);
        if n <= 0 || namespace != OS_REASON_SIGNAL {
            return String::new();
        }
        // (kcdata: items of a type, a size and flags — 16 bytes — then their data, that size)
        let (mut at, len, mut from) = (0, (kcd_size as usize).min(kcd.len()), Vec::new());
        while at + 16 <= len {
            let word = |i: usize| u32::from_le_bytes(kcd[i..i + 4].try_into().unwrap());
            let (kind, size) = (word(at), word(at + 4) as usize);
            let data = &kcd[at + 16..(at + 16 + size).min(len)];
            if kind == kcdata_pid && data.len() >= 4 {
                from.push(format!("pid {}", i32::from_le_bytes(data[..4].try_into().unwrap())));
            } else if kind == kcdata_procname {
                from.push(
                    String::from_utf8_lossy(data.split(|b| *b == 0).next().unwrap_or_default()).into_owned(),
                );
            }
            at += 16 + size;
        }
        format!(" — sent by {}", if from.is_empty() { "no one named".to_string() } else { from.join(" ") })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = pid;
        String::new()
    }
}

#[test]
fn version_and_help_print_without_a_terminal() {
    let out = Command::new(BIN).arg("--version").output().unwrap();
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), format!("tarae {}", env!("CARGO_PKG_VERSION")));
    let out = Command::new(BIN).arg("--help").output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("usage: tarae [FILE]"));
}

/// Every thread tarae starts has a name of its own (`event::thread`: `tarae-input`, `tarae-job`, …), so `top
/// -H`, a profiler or `tarae-trace` tells them apart — unnamed, each is called `tarae` like the main thread.
/// Sampled from /proc while it starts on a file in a git repository, moves and saves. Linux only: elsewhere a
/// process's thread names aren't there for a test to read.
///
/// A thread names itself as it starts (Rust's `Builder::name`: from inside it, first thing), so for its first
/// moments it shows its creator's name — the main thread's `tarae`. One caught then and gone before the next look
/// (a short job; seen on Debian in tier own, where the start makes more of them) is no unnamed thread: one counts only
/// once two looks, 20 ms apart, find it still `tarae` — and past its start: it had a CPU since the first look
/// (its `schedstat` time grew) or waits in a call. On a busy VM a thread just made can wait for a CPU longer
/// than that, still in its first slice (deb12, frictions16: `schedstat 11680 18509 1` — 12 µs run, one
/// slice — on both looks, runnable, in no call): not yet named, not unnamed. A kernel without schedstat: the
/// two looks alone, as before.
#[cfg(target_os = "linux")]
#[test]
fn every_thread_tarae_starts_has_a_name() {
    use std::collections::BTreeMap;
    let sb = Sandbox::new("thread-names");
    sb.git(&["init", "-q"]);
    sb.write("a.rs", "fn main() {}\n");
    let mut t = sb.spawn(&["a.rs"]);
    t.wait_text("fn main");
    let pid = t.pid();
    // (each thread's latest name, how many looks found it `tarae`, whether one found it past its start, and its
    // time on a CPU at the first look that found it `tarae`)
    type Seen = BTreeMap<u32, (String, u32, bool, Option<u64>)>;
    let mut seen: Seen = BTreeMap::new();
    // (an unnamed one's whereabouts too, each look: where it waits, the call it's in, its time on a CPU)
    let mut whereabouts: BTreeMap<u32, String> = BTreeMap::new();
    let mut look = |seen: &mut Seen| {
        for e in fs::read_dir(format!("/proc/{pid}/task")).into_iter().flatten().flatten() {
            let tid: u32 = e.file_name().to_string_lossy().parse().unwrap_or(0);
            if let Ok(name) = fs::read_to_string(e.path().join("comm")) {
                let name = name.trim().to_string();
                let entry = seen.entry(tid).or_insert((String::new(), 0, false, None));
                if name == "tarae" && tid != pid {
                    let read = |f: &str| fs::read_to_string(e.path().join(f)).unwrap_or_default();
                    let (syscall, sched) = (read("syscall"), read("schedstat"));
                    // (schedstat: its time on a CPU so far, ns — the time waiting for one, ns — its slices)
                    let ran: Option<u64> = sched.split_whitespace().next().and_then(|n| n.parse().ok());
                    let at = format!(
                        "wchan {} · syscall {} · schedstat {}",
                        read("wchan").trim(),
                        syscall.trim(),
                        sched.trim()
                    );
                    whereabouts.entry(tid).or_default().push_str(&format!("[{at}] "));
                    let first = *entry.3.get_or_insert(ran.unwrap_or(0));
                    let in_a_call = syscall.trim() != "running";
                    entry.2 |= ran.is_none_or(|r| r > first) || in_a_call;
                    entry.1 += 1;
                }
                entry.0 = name;
            }
        }
    };
    look(&mut seen);
    t.send("j");
    t.command("w");
    // (threads come and go — a job's lives as long as its work: sampled for a second, not waited for)
    for _ in 0..50 {
        look(&mut seen);
        std::thread::sleep(Duration::from_millis(20));
    }
    let unnamed: Vec<_> = seen
        .iter()
        .filter(|(tid, (name, looks, past_start, _))| {
            **tid != pid && name.as_str() == "tarae" && *looks >= 2 && *past_start
        })
        .collect();
    assert!(
        unnamed.is_empty(),
        "threads without a name of their own: {unnamed:?} — {whereabouts:?}\nall: {seen:?}"
    );
    assert!(
        seen.values().any(|(n, ..)| n == "tarae-input") && seen.values().any(|(n, ..)| n == "tarae-signals"),
        "{seen:?}"
    );
}

#[test]
fn starts_shows_the_file_and_quits_restoring_the_terminal() {
    let sb = Sandbox::new("quit");
    sb.write("hello.txt", "first line\nsecond line\n");
    let mut t = sb.spawn(&["hello.txt"]);
    let s = t.wait_text("second line");
    assert!(s.contains("first line"), "{s}");
    assert!(s.contains("hello.txt"), "file name in the header\n{s}");

    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));

    // term::restore(): DisableFocusChange, DisableMouseCapture, SetCursorStyle::DefaultUserShape, Show,
    // LeaveAlternateScreen — all after the last time the alternate screen was entered.
    let leave = b"\x1b[?1049l";
    let raw = t.wait_raw("LeaveAlternateScreen", |r| contains(r, leave));
    let entered = rfind(&raw, b"\x1b[?1049h").expect("entered the alternate screen");
    let after = &raw[entered..];
    for (name, seq) in [
        ("focus reporting off", &b"\x1b[?1004l"[..]),
        ("mouse capture off", b"\x1b[?1000l"),
        ("default cursor shape", b"\x1b[0 q"),
        ("cursor shown", b"\x1b[?25h"),
        ("alternate screen left", leave),
    ] {
        assert!(contains(after, seq), "{name}: {seq:?} missing after entering the alternate screen");
    }
    // The alternate screen is left last — nothing draws over the user's shell afterwards.
    let left_at = rfind(&raw, leave).unwrap();
    assert!(left_at > entered);
    assert!(!contains(&raw[left_at..], b"\x1b[?1049h"));
    assert!(!t.screen().contains("● normal"), "back on the main screen:\n{}", t.screen());
}

/// Over SSH a copy goes to the user's own clipboard through the terminal (OSC 52), written with tarae's own
/// output after a frame — and the one toast says so.
#[test]
fn a_copy_over_ssh_goes_through_the_terminal() {
    let sb = Sandbox::new("osc52").with_env("SSH_TTY", "/dev/pts/9");
    sb.write("a.txt", "hi there\n");
    let mut t = sb.spawn(&["a.txt"]);
    t.wait_text("hi there");
    t.send("e");
    t.send(" y");
    // "hi" in base64, ended by ST
    let osc = b"\x1b]52;c;aGk=\x1b\\";
    let raw = t.wait_raw("the OSC 52", |r| contains(r, osc));
    let at = rfind(&raw, osc).unwrap();
    let opened = rfind(&raw[..at], b"\x1b[?2026h").expect("a frame before it");
    assert!(contains(&raw[opened..at], b"\x1b[?2026l"), "not inside a frame's synchronized update");
    let s = t.wait_text("copied 1 selection");
    assert!(s.contains("OSC 52"), "says how\n{s}");
}

/// #184: the harness judges a screen only between frames — read up to a frame's start and not its end, the
/// parsed screen is half old, half new. A mark split across two reads is seen once its last byte comes.
#[test]
fn a_half_read_frame_is_not_judged() {
    use harness::in_frame_after;
    assert!(in_frame_after(b"old\x1b[?2026hthe first row", false), "started, not ended");
    assert!(!in_frame_after(b"the last row\x1b[?2026l", true), "ended");
    assert!(in_frame_after(b"\x1b[?2026l\x1b[?2026hnext", false), "one ended, the next begun");
    assert!(!in_frame_after(b"\x1b[?2026hall\x1b[?2026l", false), "a whole one");
    assert!(in_frame_after(b"rows, no mark", true) && !in_frame_after(b"plain", false), "no mark: as it was");
    // (split: the reader looks again from a mark's length less one before the new bytes)
    let (first, second) = (b"\x1b[?20".as_slice(), b"26hrow".as_slice());
    assert!(!in_frame_after(first, false), "not yet a mark");
    let mut seen = first[first.len().saturating_sub(harness::FRAME_START.len() - 1)..].to_vec();
    seen.extend_from_slice(second);
    assert!(in_frame_after(&seen, false), "its last byte come: a frame begun");
}

/// No claude on PATH (the sandbox's PATH has none): the chat says Claude Code isn't installed and how to get it
/// and log in — not an OS error (#163).
#[test]
fn a_chat_without_claude_says_how_to_get_it() {
    let sb = Sandbox::new("noclaude");
    sb.write("a.txt", "hi\n");
    let mut t = sb.spawn(&["a.txt"]);
    t.wait_text("hi");
    let input = t.state_json(0)["input"].as_u64().unwrap();
    t.send(" l");
    t.send("hello\r");
    // (the Enter handled and drawn too: it tries again)
    t.state_json(input + 8);
    // (the message whole — #184: under load its first line was read before the rest of its frame)
    let s = t.wait_for("the message, whole", |s| {
        s.contains("Claude Code isn't installed") && s.contains("install.sh") && s.contains("log in")
    });
    assert!(s.contains("install.sh"), "how to get it\n{s}");
    assert!(s.contains("log in"), "and log in\n{s}");
    assert!(!s.contains("os error"), "{s}");
    // (opening the chat tried, and so did sending: said once — a second would push the first's start up)
    let said = |line: &str| s.rows.iter().filter(|r| r.contains(line)).count();
    assert_eq!((said("Claude Code isn't installed"), said("After installing")), (1, 1), "{s}");
}

#[test]
fn edits_and_saves_an_existing_file() {
    let sb = Sandbox::new("save");
    let path = sb.write("note.txt", "world\n");
    let mut t = sb.spawn(&["note.txt"]);
    t.wait_text("world");
    t.insert("hello ");
    t.wait_text("hello world");
    t.command("w");
    wait_file(&path, "hello world\n");
    t.wait_text("Saved note.txt");
    // Saved, so a plain :q is allowed
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));
}

#[test]
fn saving_creates_a_file_that_did_not_exist() {
    let sb = Sandbox::new("create");
    let path = sb.file("fresh.txt");
    assert!(!path.exists());
    let mut t = sb.spawn(&["fresh.txt"]);
    t.wait_text("fresh.txt");
    t.insert("brand new");
    assert!(!path.exists(), "nothing written before :w");
    t.command("w");
    wait_file(&path, "brand new");
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));
}

#[test]
fn welcome_screen_without_a_file_then_scratch_save_as() {
    let sb = Sandbox::new("welcome");
    let mut t = sb.spawn(&[]);
    let s = t.wait_text("t a r a e");
    for hint in ["Open a file", "Find a command", "Learn the keys in 10 minutes"] {
        assert!(s.contains(hint), "{hint:?} missing\n{s}");
    }
    // Typing leaves the start screen; the scratch buffer saves under a new name
    t.insert("scratch text");
    let s = t.screen();
    assert!(!s.contains("t a r a e"), "{s}");
    t.command("w scratch.txt");
    wait_file(&sb.file("scratch.txt"), "scratch text");
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));
}

#[test]
fn survives_resizing_to_a_tiny_terminal_and_back() {
    let sb = Sandbox::new("resize");
    let body: String = (1..=60).map(|i| format!("line number {i}\n")).collect();
    sb.write("long.txt", &body);
    let mut t = sb.spawn(&["long.txt"]);
    t.wait_text("line number 1");
    // A processed key proves the input thread is up — crossterm installs its SIGWINCH handler there, and a
    // resize before that would go unnoticed.
    t.send("j");
    t.wait_for("cursor on line 2", |s| s.status().contains("2:1"));

    t.resize(20, 5);
    t.wait_for("a redraw at 20x5 with the status line", |s| {
        s.rows.len() == 5 && s.status().contains("● normal")
    });
    // Still responsive at that size
    t.send("j");
    t.wait_for("cursor on line 3 at 20x5", |s| s.status().contains("3:1"));

    // Degenerate sizes (a pane dragged shut, a minimized window) with keys arriving meanwhile. The screen
    // isn't checked here (vt100 itself overflows on one column) — only that no key is lost or panics.
    for (w, h) in [(1, 1), (0, 0), (100, 1)] {
        t.resize(w, h);
        t.send("j");
    }

    t.resize(COLS, ROWS);
    t.wait_for("a full redraw at 100x30, cursor on line 6", |s| {
        s.rows.len() == ROWS as usize && s.status().contains("6:1") && s.contains("line number 20")
    });
    // Still responsive after all that
    t.send("gg");
    t.wait_for("cursor back on line 1", |s| s.status().contains("1:1"));
    assert!(t.running());
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));
}

/// Config errors never block startup: tarae starts on defaults and reports `file:line: problem`.
fn starts_despite_config(config: &str, line: usize, problem: &str) {
    let sb = Sandbox::new("badconfig");
    sb.write_config(&format!("{BASE_CONFIG}{config}"));
    sb.write("a.txt", "still editable\n");
    let mut t = sb.spawn(&["a.txt"]);
    let at = format!("config.toml:{line}:");
    let s = t.wait_for(&format!("the config error at {at}"), |s| s.contains(&at) && s.contains(problem));
    assert!(s.contains("still editable"), "{s}");
    t.insert("ok ");
    t.wait_text("ok still editable");
    t.command("q!");
    assert_eq!(t.wait_exit().code(), Some(0));
}

#[test]
fn bad_config_value_is_reported_with_file_and_line() {
    // BASE_CONFIG is 4 lines; the bad value lands on line 5
    starts_despite_config("tab-width = 999\n", 5, "tab-width");
}

#[test]
fn broken_config_syntax_is_reported_with_file_and_line() {
    starts_despite_config("scrolloff = = 3\n", 5, "config.toml");
}

#[test]
fn hangul_and_emoji_move_and_delete_by_grapheme() {
    let sb = Sandbox::new("unicode");
    // 👍🏽 = thumbs up + skin tone modifier: two code points, one grapheme (8 bytes)
    let path = sb.write("u.txt", "한글👍🏽x\n");
    let mut t = sb.spawn(&["u.txt"]);
    t.wait_text("한글");
    // Two steps right = on the emoji (columns shown to humans count characters)
    t.send("ll");
    t.wait_for("cursor on the emoji", |s| s.status().contains("1:3"));
    // `d` deletes the whole grapheme, never half of it
    t.send("d");
    t.wait_for("emoji deleted", |s| s.contains("한글x"));
    t.insert("타래");
    t.wait_text("한글타래x");
    t.command("w");
    wait_file(&path, "한글타래x\n");
    assert_eq!(fs::read(&path).unwrap(), "한글타래x\n".as_bytes());
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));
}

#[test]
fn quit_with_unsaved_changes_is_refused_until_forced() {
    let sb = Sandbox::new("dirty");
    sb.write("keep.txt", "original\n");
    let mut t = sb.spawn(&["keep.txt"]);
    t.wait_text("original");
    t.insert("changed ");
    t.command("q");
    t.wait_text("unsaved changes");
    assert!(t.running(), "`:q` must not quit with unsaved changes");
    assert!(t.screen().contains("changed original"));

    t.command("q!");
    assert_eq!(t.wait_exit().code(), Some(0));
    assert_eq!(sb.read("keep.txt"), "original\n", "`:q!` discards, never writes");
}

#[test]
fn which_key_and_command_palette_open_and_close() {
    let sb = Sandbox::new("palette");
    sb.write("p.txt", "palette test\n");
    let mut t = sb.spawn(&["p.txt"]);
    t.wait_text("palette test");
    // `space` alone: which-key card lists what follows, described in plain words
    t.send(" ");
    t.wait_text("Find a command");
    // `?` opens the palette: its frame named `commands`, its input row `› …  n/n` under it
    t.send("?");
    t.wait_for("the command palette", |s| {
        let named = s.rows.iter().position(|r| r.contains("╭─ commands ─"));
        named.is_some_and(|y| s.rows.get(y + 1).is_some_and(|r| r.contains('›')))
    });
    t.send("save");
    t.wait_for("the query in the palette", |s| s.contains("› save"));
    let s = t.esc("the palette to close", |s| !s.contains("commands") && s.contains("palette test"));
    assert!(s.status().contains("● normal"), "{s}");
    assert!(t.running());
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));
}

/// A key that arrives together with a resize must not be lost. crossterm's default (mio, edge-triggered)
/// input source returned the resize and left the key unread until the next input — tarae uses the
/// `use-dev-tty` source (level-triggered poll) instead.
#[test]
fn keys_right_after_a_resize_are_not_lost() {
    let sb = Sandbox::new("resizekeys");
    let body: String = (1..=60).map(|i| format!("line number {i}\n")).collect();
    sb.write("long.txt", &body);
    let mut t = sb.spawn(&["long.txt"]);
    t.wait_text("line number 1");
    t.send("j");
    t.wait_for("cursor on line 2", |s| s.status().contains("2:1"));
    // Every step changes the size, so every key races a SIGWINCH
    for (i, (w, h)) in
        [(60, 12), (80, 20), (50, 10), (90, 25), (70, 15), (100, 0), (0, 30)].into_iter().enumerate()
    {
        t.resize(w, h);
        t.send("j");
        if h > 2 && w > 20 {
            let want = format!("{}:1", i + 3);
            t.wait_for(&format!("cursor on line {}", i + 3), |s| s.status().contains(&want));
        }
    }
    t.resize(COLS, ROWS);
    t.wait_for("cursor on line 9 after the degenerate sizes", |s| s.status().contains("9:1"));
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));
}

/// The side panel on a real screen: `space e` lists the project as a tree beside the code, the current
/// file's folder open; `j`/Enter opens another file (the code gets the keys back), `q` in it closes it.
#[test]
fn side_panel_lists_opens_and_closes() {
    let sb = Sandbox::new("sidepanel");
    fs::create_dir_all(sb.file("src")).unwrap();
    sb.write("src/alpha.rs", "fn alpha() {}\n");
    sb.write("src/beta.rs", "fn beta() {}\n");
    sb.write("notes.md", "# notes\n");
    // Wide enough for the panel and 80 columns of code
    let mut t = Term::spawn(&sb, &["src/alpha.rs"], 130, ROWS);
    t.wait_text("fn alpha");
    t.send(" e");
    let s = t.wait_for("the tree", |s| s.contains("src/") && s.contains("beta.rs") && s.contains("notes.md"));
    assert!(s.contains("files") && s.contains("outline"), "the tabs: {s}");
    // Selected: alpha.rs (the file being edited) → one down is beta.rs
    t.send("j\r");
    t.wait_for("beta.rs open", |s| s.contains("fn beta"));
    // The code has the keys again: `i` is insert mode
    t.send("i");
    t.wait_for("INSERT mode", |s| s.status().contains("● insert"));
    t.normal_mode();
    // Back in the panel, `q` closes it
    t.send(" e");
    t.wait_state("the panel has the keys", |s| s["keys"]["region"] == "side:files");
    t.send("q");
    t.wait_state("the panel gone", |s| s["panels"]["side"].is_null() && s["keys"]["region"] == "code");
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));
}

/// The built-in terminal through a real terminal: `space T` starts the shell with the keys, what's typed
/// runs there (not in the buffer), a real `C-\` byte (0x1c — crossterm reads it as Ctrl-4) gives the keys
/// back; `space T` from the code gives them to it again (#144), and from inside — `C-w j`, tarae's keys on
/// its panel — hides it.
#[test]
fn terminal_panel_runs_the_shell_and_gives_the_keys_back() {
    let sb = Sandbox::new("terminal");
    sb.write("t.txt", "the buffer\n");
    let mut t = sb.spawn(&["t.txt"]);
    t.wait_text("the buffer");
    t.send(" T");
    t.wait_text("● terminal");
    // The shell may still be starting — its line discipline keeps what's typed until it reads
    t.send("echo sum-$((40+2))\r");
    t.wait_text("sum-42");
    assert!(!t.screen().contains("the bufferecho"), "the buffer never saw the keys");
    // (`C-\` its view mode, `C-\` again the code)
    t.send("\x1c\x1c");
    t.send("i");
    t.wait_for("INSERT mode in the code", |s| s.status().contains("● insert"));
    t.normal_mode();
    t.send(" T");
    t.send("echo again-$((1+1))\r");
    t.wait_text("again-2");
    // (its view mode: `q` hides it, the shell running on)
    t.send("\x1c");
    t.send("q");
    t.wait_for("the terminal hidden", |s| !s.contains("● terminal") && !s.contains("sum-42"));
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));
    assert_eq!(sb.read("t.txt"), "the buffer\n");
}

/// A program flooding the terminal panel (`yes`) doesn't take the editor with it: the keys still land
/// right away — its output is taken in a batch at a time, the program waiting on a full pty meanwhile.
#[test]
fn a_flood_in_the_terminal_panel_leaves_the_keys_working() {
    let sb = Sandbox::new("flood");
    sb.write("t.txt", "the buffer\n");
    let mut t = sb.spawn(&["t.txt"]);
    t.wait_text("the buffer");
    t.send(" T");
    t.wait_text("● terminal");
    t.send("yes flood-line\r");
    t.wait_text("flood-line");
    std::thread::sleep(std::time::Duration::from_secs(1)); // a second of it piling up
    let asked = std::time::Instant::now();
    // (its view mode, then the code: what came meanwhile taken in as it's left)
    t.send("\x1c\x1c");
    t.send("i");
    // (an editor the flood takes never handles them — the bound only has to outlast a slow runner)
    let inserting = t.poll_for(TIMEOUT, |s| s.status().contains("● insert"));
    let took = asked.elapsed();
    eprintln!("flood: keys landed after {took:?}");
    assert!(inserting.is_some(), "keys still not handled after {took:?} of a flood");
    t.normal_mode();
    t.command("q!");
    assert_eq!(t.wait_exit().code(), Some(0));
}

/// tarae in `sb` with the terminal panel's shell running a program in front — (the shell's pid, the
/// program's), as each wrote it into the work folder.
fn a_shell_with_a_job(sb: &Sandbox) -> (Term, i32, i32) {
    let mut t = sb.spawn(&[]);
    t.send(" T");
    t.wait_text("● terminal");
    t.send("echo $$ > shell.pid\r");
    t.send("sh -c 'echo $$ > job.pid; exec sleep 120'\r");
    let pid = |name: &str| {
        let deadline = std::time::Instant::now() + TIMEOUT;
        loop {
            match fs::read_to_string(sb.file(name)).ok().and_then(|s| s.trim().parse().ok()) {
                Some(pid) => return pid,
                None if std::time::Instant::now() < deadline => std::thread::sleep(POLL),
                None => panic!("{name} never written\n{}", t.screen()),
            }
        }
    };
    let (shell, job) = (pid("shell.pid"), pid("job.pid"));
    assert!(alive(shell) && alive(job));
    (t, shell, job)
}

/// Whether `pid` is there (signal 0 only asks).
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 sends nothing.
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Waits (`TIMEOUT` at most) for both to be gone.
fn gone(what: &str, shell: i32, job: i32) {
    let deadline = std::time::Instant::now() + TIMEOUT;
    while (alive(shell) || alive(job)) && std::time::Instant::now() < deadline {
        std::thread::sleep(POLL);
    }
    assert!(!alive(shell), "{what}: the terminal panel's shell ({shell}) lives on");
    assert!(!alive(job), "{what}: the program in front of it ({job}) lives on");
}

/// A tarae that quits takes its terminal panel's shell and the program running there with it.
#[test]
fn a_quit_tarae_leaves_no_shell_behind() {
    let sb = Sandbox::new("quit-shell").with_env("HISTFILE", "/dev/null");
    let (mut t, shell, job) = a_shell_with_a_job(&sb);
    t.send("\x1c\x1c");
    t.command("q!");
    assert_eq!(t.wait_exit().code(), Some(0));
    gone(":q!", shell, job);
}

/// Killed (no destructor runs), tarae still leaves no shell: the pty's master was its own alone, so it
/// closes with it and the kernel hangs the shell up — which takes its program along.
#[test]
fn a_killed_tarae_leaves_no_shell_behind() {
    let sb = Sandbox::new("killed-shell").with_env("HISTFILE", "/dev/null");
    let (mut t, shell, job) = a_shell_with_a_job(&sb);
    t.child.kill().unwrap();
    t.wait_exit();
    gone("SIGKILL", shell, job);
}

/// SIGTERM and SIGHUP (its terminal gone — a window closed, the test binary that ran it dead) quit tarae
/// as `:q!` does — it exits rather than dying of the signal, and its shell goes with it.
#[test]
fn a_terminated_or_hung_up_tarae_quits_and_ends_its_shell() {
    use std::os::unix::process::ExitStatusExt;
    for sig in [libc::SIGTERM, libc::SIGHUP] {
        let sb = Sandbox::new("term-shell").with_env("HISTFILE", "/dev/null");
        let (mut t, shell, job) = a_shell_with_a_job(&sb);
        // SAFETY: a signal to the tarae this test started, still running.
        unsafe { libc::kill(t.pid() as i32, sig) };
        let status = t.wait_exit();
        assert_eq!(status.signal(), None, "signal {sig}: it quit, not killed by it ({status})");
        gone(&format!("signal {sig}"), shell, job);
    }
}

/// A tarae whose terminal goes away (SIGHUP — quitting as `:q!` does) leaves the next start at least what a
/// killed one leaves: the files to continue with (a quit writes them, a kill keeps the last quit's), and the
/// files on disk as they were — unsaved text is never written to them (a copy of it may be kept for the next
/// start to offer back: `a_killed_taraes_unsaved_edits_come_back`).
#[test]
fn a_hung_up_tarae_leaves_the_next_start_at_least_what_a_killed_one_does() {
    let names = ["a.txt", "b.txt"];
    let among = |row: &str| -> Vec<&str> { names.iter().copied().filter(|n| row.contains(n)).collect() };
    // (what `r` reopens — the header's tabs — and the start screen's recent files)
    let offered = |how: libc::c_int| -> (Vec<&str>, Vec<&str>, String) {
        let sb = Sandbox::new("hangup-keeps");
        sb.write("a.txt", "alpha\n");
        sb.write("b.txt", "beta\n");
        // A quit before: a.txt kept to continue with
        let mut t = sb.spawn(&["a.txt"]);
        t.wait_text("alpha");
        t.command("q");
        assert_eq!(t.wait_exit().code(), Some(0));
        // Then b.txt and a.txt open, b.txt changed and not saved — and the end
        let mut t = sb.spawn(&["b.txt", "a.txt"]);
        t.wait_text("beta");
        t.insert("unsaved-edit ");
        // SAFETY: a signal to the tarae this test started, still running.
        unsafe { libc::kill(t.pid() as i32, how) };
        t.wait_exit();
        // What the next start offers, and what `r` brings back
        let mut t = sb.spawn(&[]);
        let s = t.wait_text("t a r a e");
        let at = s.rows.iter().position(|r| r.contains("Recent")).unwrap_or(s.rows.len());
        let recent = among(&s.rows[at..].join("\n"));
        let continued = if s.contains("Continue where you left off") {
            t.send("r");
            let s = t.wait_for("the files continued with", |s| s.contains("alpha") || s.contains("beta"));
            assert!(!s.contains("unsaved-edit"), "{s}");
            among(&s.rows[0])
        } else {
            Vec::new()
        };
        t.command("q");
        assert_eq!(t.wait_exit().code(), Some(0));
        (continued, recent, sb.read("b.txt"))
    };
    let (killed, killed_recent, killed_b) = offered(libc::SIGKILL);
    let (hung_up, hung_up_recent, hung_up_b) = offered(libc::SIGHUP);
    assert_eq!(killed, ["a.txt"], "a kill keeps the last quit's");
    for (n, list) in
        killed.iter().map(|n| (n, &hung_up)).chain(killed_recent.iter().map(|n| (n, &hung_up_recent)))
    {
        assert!(list.contains(n), "{n}: a kill kept it, a hangup didn't ({list:?})");
    }
    assert_eq!(hung_up, ["a.txt", "b.txt"], "a hangup keeps what was open, as a quit does");
    assert_eq!((killed_b.as_str(), hung_up_b.as_str()), ("beta\n", "beta\n"), "unsaved text never written");
}

/// The copies of unsaved buffers in a sandbox's state, any tarae's.
fn recovery_copies(sb: &Sandbox) -> Vec<std::path::PathBuf> {
    let folders = fs::read_dir(sb.state().join("recovery")).into_iter().flatten().flatten();
    let files = folders.flat_map(|d| fs::read_dir(d.path()).into_iter().flatten().flatten());
    files.map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "txt")).collect()
}

/// Killed (no destructor runs) with an edit not saved: the copy kept after the pause in typing brings it
/// back — the next start's card, `y`, and the text is in the buffer, unsaved; the file was never written. A
/// quit then leaves no copy behind (the answered one went, and its own with the quit).
#[test]
fn a_killed_taraes_unsaved_edits_come_back() {
    let sb = Sandbox::new("recover");
    sb.write("a.txt", "alpha\n");
    let mut t = sb.spawn(&["a.txt"]);
    t.wait_text("alpha");
    t.insert("unsaved-edit ");
    // (kept after a pause in typing — waited for where it's written)
    let deadline = std::time::Instant::now() + sb.timeout();
    while recovery_copies(&sb).is_empty() {
        assert!(std::time::Instant::now() < deadline, "no copy of the unsaved buffer was kept");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    t.child.kill().unwrap();
    t.wait_exit();
    assert_eq!(sb.read("a.txt"), "alpha\n");

    let mut t = sb.spawn(&["a.txt"]);
    focus_card(&mut t, "the recovery card", |c| c["kind"] == "recover");
    t.wait_text("Unsaved edits · a.txt");
    t.send("y");
    t.wait_text("unsaved-edit alpha");
    assert_eq!(sb.read("a.txt"), "alpha\n", "restored into the buffer, not written");
    t.command("q!");
    assert_eq!(t.wait_exit().code(), Some(0));
    assert!(recovery_copies(&sb).is_empty(), "{:?}", recovery_copies(&sb));
}

/// A build task from the config: its output below the code, the place it printed goes into the list, `]q`
/// goes there with the message as a toast, `space q` shows the list in the side panel.
#[test]
fn task_output_fills_the_list_and_q_walks_it() {
    let sb = Sandbox::new("task");
    sb.write("m.rs", "fn main() {\n    let x = oops;\n}\n");
    sb.write_config(&format!(
        "{BASE_CONFIG}[task.check]\ncmd = \"echo checking; echo 'm.rs:2:13: error: cannot find value oops'; exit 1\"\n"
    ));
    let mut t = Term::spawn(&sb, &["m.rs"], 130, ROWS);
    t.wait_text("fn main");
    t.command("task check");
    t.wait_for("the task failed", |s| s.contains("─ check ▲ ── failed") && s.contains("checking"));
    t.send("]q");
    t.wait_for("the cursor on the place", |s| s.status().contains("2:13"));
    t.wait_text("cannot find value oops");
    t.send(" q");
    t.wait_for("the list in the side panel", |s| s.contains("list") && s.contains("2: cannot find"));
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));
}

/// The git tab over a real repository: the changed file listed, `s` stages it, `c` opens the message
/// buffer, `:w` commits it — the tree is clean after.
#[test]
fn git_tab_stages_and_commits() {
    let sb = Sandbox::new("gittab");
    fs::write(
        sb.home().join(".gitconfig"),
        "[user]\n\tname = t\n\temail = t@t\n[commit]\n\tgpgsign = false\n",
    )
    .unwrap();
    let git = |args: &[&str]| {
        let st = Command::new("git")
            .args(args)
            .current_dir(sb.work())
            .env("HOME", sb.home())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git");
        assert!(st.success(), "git {args:?}");
    };
    sb.write("a.txt", "one\n");
    git(&["init", "-q", "-b", "main"]);
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "base"]);
    sb.write("a.txt", "one\ntwo\n");
    let mut t = Term::spawn(&sb, &["a.txt"], 130, ROWS);
    t.wait_text("two");
    t.send(" g");
    t.wait_for("the change listed", |s| s.contains("Changes") && s.contains("a.txt"));
    // The selection starts on the file being edited
    t.send("s");
    t.wait_text("Staged a.txt");
    // Right away — before the status after the stage is read back (as a quick hand would)
    t.send("c");
    t.wait_for("the message buffer, typing", |s| {
        s.contains("COMMIT_EDITMSG") && s.status().contains("● insert")
    });
    t.send("second line");
    t.wait_text("second line");
    t.normal_mode();
    t.command("w");
    t.wait_for("committed — the tree clean", |s| s.contains("nothing to commit"));
    let log = Command::new("git").args(["log", "-1", "--format=%s"]).current_dir(sb.work()).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&log.stdout).trim(), "second line");
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));
}

/// #187: in the real binary, the git tab's "Changes" header is reached with `k` from the files under it, and
/// `s` there stages the whole section at once (one `s` per folder before).
#[test]
fn git_tab_section_header_takes_k_and_stages_all() {
    let sb = Sandbox::new("gittab-section");
    fs::write(
        sb.home().join(".gitconfig"),
        "[user]\n\tname = t\n\temail = t@t\n[commit]\n\tgpgsign = false\n",
    )
    .unwrap();
    let git = |args: &[&str]| {
        let st = Command::new("git")
            .args(args)
            .current_dir(sb.work())
            .env("HOME", sb.home())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git");
        assert!(st.success(), "git {args:?}");
    };
    let files = ["docs/b.md", "src/a.rs", "tests/c.rs"];
    for f in files {
        fs::create_dir_all(sb.work().join(f).parent().unwrap()).unwrap();
        sb.write(f, "one\n");
    }
    git(&["init", "-q", "-b", "main"]);
    git(&["add", "-A"]);
    git(&["commit", "-q", "-m", "base"]);
    for f in files {
        sb.write(f, "one\ntwo\n");
    }
    let mut t = Term::spawn(&sb, &["tests/c.rs"], 130, ROWS);
    t.wait_text("two");
    t.send(" g");
    t.wait_for("the changes listed", |s| s.contains("Changes") && s.contains("c.rs"));
    // Up from the file being edited (the last one) to the top — at once, as a quick hand does, while the
    // file's hunks may still be read (they put the selection back on its row then)
    t.send("kkkkkk");
    t.send("s");
    t.wait_for("all of them staged", |s| s.contains("Staged everything"));
    let st = Command::new("git")
        .args(["diff", "--cached", "--name-only"])
        .current_dir(sb.work())
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&st.stdout).trim(), "docs/b.md\nsrc/a.rs\ntests/c.rs");
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));
}

/// N6: while Claude works the waiting animation wants its frames — in a terminal that never says where its
/// focus is, too. The terminal saying focus went elsewhere (`CSI O`, focus reporting being on — CSI ?1004)
/// rests it: no tick wanted, and once settled nothing goes out. Focus back (`CSI I`) and it goes on. Judged from
/// the state (`tick_in_ms`, `unfocused`) — frames only as a sign it draws, never a rate: a shared runner draws
/// fewer of them (CI's macOS: 2 a second).
#[test]
fn the_waiting_animation_rests_while_the_terminal_is_unfocused() {
    let sb = Sandbox::new("waiting-focus");
    sb.write("a.txt", "some text\n");
    let mut t = sb.waiting_turn(&["a.txt"], COLS, ROWS, 60);
    // (it draws: two frames past `from` — focus back's whole redraw is one —, waited for)
    let draws = |t: &mut Term, what: &str, from: usize| {
        let deadline = std::time::Instant::now() + sb.timeout();
        while t.frames() < from + 2 {
            assert!(std::time::Instant::now() < deadline, "{what}: {} frames, no more", t.frames() - from);
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    let s = t.wait_state("the animation's tick, no focus event yet", |s| s["tick_in_ms"].is_u64());
    assert!(s["tick_in_ms"].as_u64() <= Some(1000) && s.get("unfocused").is_none(), "{s:#}");
    let from = t.frames();
    draws(&mut t, "the animation, no focus event yet", from);
    t.send("\x1b[O");
    t.wait_state("unfocused: no tick", |s| s["unfocused"] == true && s.get("tick_in_ms").is_none());
    // Settled (the redraw that follows a frame sent in parts gone out, every frame read): then nothing goes out
    // (a tick asked while focused may land after `CSI O`: it draws nothing — `Editor::tick_landed`)
    t.settle("unfocused, settled", |s| s.get("tick_in_ms").is_none());
    let (from, raw_at) = (t.frames(), t.raw().len());
    std::thread::sleep(Duration::from_millis(1500));
    let raw = t.raw();
    assert_eq!(
        t.frames() - from,
        0,
        "unfocused: nothing drawn — it wrote {:?}\n{}",
        String::from_utf8_lossy(&raw[raw_at..]).chars().take(300).collect::<String>(),
        t.screen()
    );
    t.send("\x1b[I");
    t.wait_state("focused again: the tick", |s| s.get("unfocused").is_none() && s["tick_in_ms"].is_u64());
    draws(&mut t, "the animation after focus came back", from);
}

/// Counting which features get used (`features.count` on, `features.rs`) wakes nothing: after keys counted,
/// an idle tarae draws nothing and sleeps — what's counted is written once it ends, not on a timer of its own;
/// `tarae state --features` has them meanwhile.
#[test]
fn counting_features_wakes_nothing_and_is_written_at_the_end() {
    let sb = Sandbox::new("features");
    sb.write_config(&format!("{BASE_CONFIG}\n[features]\ncount = true\n"));
    sb.write("a.txt", "one\ntwo\nthree\n");
    let mut t = sb.spawn(&["a.txt"]);
    t.wait_text("three");
    t.send("jj");
    t.wait_state("two keys handled", |s| s["input"].as_u64().is_some_and(|n| n >= 2));
    // Settled (no motion, every frame written read, no redraw owed — `Term::settle`, never a quiet second), then two
    // seconds: no frame, next to no wakes
    t.settle("the keys' frames", |s| s["motions"].as_array().is_some_and(Vec::is_empty));
    let (frames, before, was, threads) = (t.frames(), t.usage(), t.screen(), t.threads());
    std::thread::sleep(std::time::Duration::from_secs(2));
    let (drawn, used) = (t.frames() - frames, t.usage() - before);
    // (on failure: what woke — each thread's share — and which rows a late frame changed)
    let now = t.screen();
    let changed: Vec<String> = was
        .rows
        .iter()
        .zip(&now.rows)
        .enumerate()
        .filter(|(_, (a, b))| a != b)
        .map(|(i, (a, b))| format!("row {i}: {a:?}\n    → {b:?}"))
        .collect();
    assert!(
        drawn == 0 && used.switches < 30,
        "counting: {drawn} frames, {used:?} in 2 s — woke:\n{}\nchanged:\n{}\n{now}",
        woke(&threads, &t.threads()),
        changed.join("\n")
    );
    let path = sb.state().join("features.json");
    assert!(!path.exists(), "nothing written yet — no timer of its own");
    let said: serde_json::Value = serde_json::from_str(&t.state(&["--features", "--json"])).unwrap();
    let row = said["features"].as_array().unwrap().iter().find(|r| r["uses"] == 2).cloned();
    assert!(row.is_some_and(|r| r["area"] == "command" && r["key"] == "j"), "{said}");
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));
    let saved = fs::read_to_string(&path).expect("written as it ended");
    assert!(saved.contains(":[2,"), "{saved}");
}

/// `:web` in the real binary against a server of this test's own on this machine (held until the test lets it
/// answer): the page's buffer at once, its line saying it's reading with `C-c stop`; then the page — its title on the
/// line, its first heading in the text — and once it landed, nothing drawn and nothing woken.
#[test]
fn a_page_reads_into_its_buffer_and_then_sleeps() {
    use std::io::{Read as _, Write as _};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (go, held) = std::sync::mpsc::channel::<()>();
    std::thread::spawn(move || {
        let Ok((mut s, _)) = listener.accept() else { return };
        let mut head = Vec::new();
        let mut b = [0u8; 1024];
        while !head.windows(4).any(|w| w == b"\r\n\r\n") {
            match s.read(&mut b) {
                Ok(0) | Err(_) => return,
                Ok(n) => head.extend_from_slice(&b[..n]),
            }
        }
        let _ = held.recv();
        let body = "<title>Held</title><main><h1>Held page</h1><p>Its text, at last.</p></main>";
        let reply = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = s.write_all(reply.as_bytes());
    });
    let sb = Sandbox::new("web-page");
    let mut t = sb.spawn(&[]);
    t.wait_for("started", |s| s.status().contains("● normal"));
    t.command(&format!("web http://localhost:{port}/a"));
    t.wait_for("reading, C-c stop", |s| s.contains("reading localhost") && s.contains("C-c stop"));
    go.send(()).unwrap();
    let s = t.wait_text("# Held page");
    assert!(s.contains("localhost › Held") && !s.contains("reading localhost"), "{s}");
    // Landed: its toast gone, settled (`Term::settle`, never a quiet second), then two seconds with no frame and
    // nothing woken (the line's age is drawn when a frame comes, never a frame of its own)
    t.wait_for("its toast gone", |s| !s.status().contains(" chars · "));
    t.settle("the page landed", |s| s["motions"].as_array().is_some_and(Vec::is_empty));
    let (frames, before) = (t.frames(), t.usage());
    std::thread::sleep(std::time::Duration::from_secs(2));
    let (drawn, used) = (t.frames() - frames, t.usage() - before);
    assert!(drawn == 0 && used.switches < 30, "{drawn} frames, {used:?} in 2 s\n{}", t.screen());
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));
}

/// `space f` in the real binary: the tree picker's first frame (the list still coming from its worker —
/// no rows yet) draws, then the files show; a query nothing matches draws too.
/// An idle tarae sleeps — a file open in a git repository, then the side panel, the git tab, the terminal,
/// the chat, and a picker over an error waiting for its card: once settled, nothing goes out for seconds
/// and nothing wakes it (CPU time and context switches stay near nothing). The error under the picker
/// woke the main loop 14 times a second (U7 restarted its time every frame): 146 switches in 3 s.
#[test]
fn an_idle_editor_draws_nothing_and_sleeps() {
    let sb = Sandbox::new("idle");
    fs::write(sb.git_config(), "[user]\n\tname = t\n\temail = t@t\n[commit]\n\tgpgsign = false\n").unwrap();
    sb.write("a.txt", "one\n");
    sb.git(&["init", "-q", "-b", "main"]);
    sb.git(&["add", "-A"]);
    sb.git(&["commit", "-q", "-m", "base"]);
    sb.write("a.txt", "one\ntwo\n");
    // (a program may type into it: a shuttle connected, paused by the person's key, wakes nothing either)
    sb.write_config(&format!("{BASE_CONFIG}\n[ui]\nshuttle-keys = \"light\"\n"));
    let mut t = sb.spawn(&["a.txt"]);
    t.wait_text("two");
    // (the line's blame and the gutter's mark land late — git on workers; a loaded machine drew them inside the
    // measured window)
    // (the blame is the cursor's line's: line 1 at the first start, line 2 — restored there — at the second)
    // (its age is whatever the run's speed made it: `just now` within the commit's second, else `Ns ago` — ci5: the
    // VM's full suite read `t, just now · base`, never redrawn, and waited it out)
    let git_landed =
        |s: &Screen| (s.contains(" · base") || s.contains("not committed yet")) && s.contains("▎two");
    t.wait_for("the line's blame and its change's mark", git_landed);
    let idle = |t: &mut Term, what: &str| {
        // Settled on what lands late, as the state says it — never a quiet second (CI's macOS: a frame that
        // changed no row came after one): the git watch's first round (the branch), the blame (waited for
        // above), no motion under way, the whole-screen redraw that follows a frame sent in parts
        // (`RESTING_REDRAW`) gone out, and every frame written read here (`Term::settle`). Each scene waited
        // for its own text before.
        let s = t.settle(what, |s| {
            s["sessions"][0]["branch"] == "main" && s["motions"].as_array().is_some_and(Vec::is_empty)
        });
        let (frames, before, was, threads) = (t.frames(), t.usage(), t.screen(), t.threads());
        let settled = format!(
            "settled at {frames} frames read: {} written, redraw due {}, tick {}",
            s["frames_written"], s["redraw_due"], s["tick_in_ms"]
        );
        std::thread::sleep(std::time::Duration::from_secs(2));
        let (drawn, used) = (t.frames() - frames, t.usage() - before);
        let woke = woke(&threads, &t.threads());
        // (a frame drawn: which rows it changed — what landed late)
        let now = t.screen();
        let changed: Vec<String> = was
            .rows
            .iter()
            .zip(&now.rows)
            .enumerate()
            .filter(|(_, (a, b))| a != b)
            .map(|(i, (a, b))| format!("row {i}: {a:?}\n    → {b:?}"))
            .collect();
        // (a wake is a few switches: 14 a second came to ~100 in 2 s; at rest it's under 10)
        if drawn > 0 || used.switches >= 30 {
            // (what drew each frame since the settle, as tarae says it — `frames_for`: an event's kind, a job's
            // result by where it was sent from or its own name, `owed: …`, the resting redraw)
            let written = frames as u64;
            let late: Vec<String> = t.state_json(0)["frames_for"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|f| f["frame"].as_u64().is_some_and(|n| n > written))
                .map(|f| format!("frame {}: {}", f["frame"], f["for"]))
                .collect();
            panic!(
                "{what}: {drawn} frames, {used:?} in 2 s ({settled}) — drawn for:\n{}\nwoke:\n{woke}\nchanged:\n{}\n{now}",
                late.join("\n"),
                changed.join("\n")
            );
        }
    };
    idle(&mut t, "a file");
    // The UI state's socket listens all along (on by default); a snapshot asked is one frame that writes
    // nothing — and a driver connected, asking nothing, wakes nothing
    let sock = sb.state().join(format!("ui/{}.sock", t.pid()));
    assert!(sock.exists(), "{} listens", sock.display());
    assert!(t.state(&[]).contains("keys → pane#"));
    let held = connected(&sock);
    idle(&mut t, "a file, a driver connected");
    drop(held);
    // …and one watching: told only of changes, at the end of batches of events handled anyway (drawn or not)
    // — none while idle (`procs` too: a look's changes since one from before it came aren't its — an earlier
    // scene's gits ending were, 4 runs in 5, once the fix sent them at once: watchlate)
    let watching = t.watch("");
    idle(&mut t, "a file, a watcher connected");
    assert_eq!(watching.events().len(), 1, "only its `watching` line: {:#?}", watching.events());
    drop(watching);
    // A shuttle (`tarae keys`): its hello waits on the grant card, the `y` is the person's; its step lands where
    // it said; the person's key pauses it — connected and paused, nothing wakes
    // (its two calls under one lasting shell, the grant's `for`; the second waits for a line on its stdin)
    let pane = t.state_json(0)["keys"]["to"].as_str().unwrap().to_string();
    let script = "k() { \"$TARAE\" keys --pid \"$TARAE_PID\" --for $$ --name e2e --expect-keys \"$1\" k; echo \"exit $?\"; }\n\
                  k \"$1\"; read go; k \"$1\"";
    let mut shuttle = t.shuttle_shell(script, &[&pane]);
    let mut said = std::io::BufRead::lines(std::io::BufReader::new(shuttle.stdout.take().unwrap()));
    let mut answer = || -> Vec<String> {
        let mut lines = Vec::new();
        for l in said.by_ref().map_while(Result::ok) {
            let end = l.starts_with("exit ");
            lines.push(l);
            if end {
                break;
            }
        }
        lines
    };
    // (focused: a card that came on its own takes no key before — a `y` typed as it came is a yank)
    focus_card(&mut t, "the grant card", |c| c["kind"] == "shuttle");
    t.send("y");
    let first = answer();
    assert_eq!(first.last().map(String::as_str), Some("exit 0"), "{first:?}");
    assert!(first[0].starts_with(&format!("landed on {pane} (code)")), "{first:?}");
    t.wait_for("its chip", |s| s.status().contains("◦ shuttle"));
    t.send("j");
    t.wait_state("paused by the person's key", |s| s["keys"]["shuttle"]["paused"] == true);
    t.wait_for("its chip says so", |s| s.status().contains("◦ shuttle e2e paused"));
    // (its `shuttle connected` toast fades on its own time — gone first, so what's measured is the shuttle)
    t.wait_state("its toast gone", |s| s["toasts"].as_array().is_some_and(Vec::is_empty));
    idle(&mut t, "a shuttle connected, paused");
    use std::io::Write as _;
    writeln!(shuttle.stdin.as_mut().unwrap(), "go").unwrap();
    let second = answer();
    assert_eq!(second.last().map(String::as_str), Some("exit 1"), "paused: refused — {second:?}");
    assert!(second[0].starts_with("refused (paused): you typed at "), "{second:?}");
    shuttle.wait().unwrap();
    // (its process's end is heard by a thread of tarae's waiting on it, and the chip goes then — CI's macOS drew
    // that just after the next scene's settle: a frame, and the whole-screen redraw after it, in the window)
    t.wait_state("its process's end heard", |s| s["keys"]["shuttle"].is_null());
    t.send(" e");
    t.wait_state("the side panel has the keys", |s| s["keys"]["region"] == "side:files");
    idle(&mut t, "the side panel");
    t.esc_state("the code has the keys", |s| s["keys"]["region"] == "code");
    t.send(" g");
    t.wait_for("the git tab", |s| s.contains("Changes"));
    idle(&mut t, "the git tab");
    t.esc_state("the code has the keys", |s| s["keys"]["region"] == "code");
    t.send(" T");
    // (sh's prompt: `$`, or `#` as root — a container)
    t.wait_for("the terminal", |s| s.contains("● terminal") && (s.contains("$ ") || s.contains("# ")));
    idle(&mut t, "the terminal");
    t.send("\x1c\x1c");
    t.wait_state("the code has the keys", |s| s["keys"]["region"] == "code");
    t.send(" l");
    t.wait_text("Ask about");
    idle(&mut t, "the chat");
    // (watchlate: watchers are told after batches that drew nothing too — with the most going on, nothing
    // changing still sends no line and draws no frame)
    let watching = t.watch("");
    idle(&mut t, "the chat, a watcher connected");
    assert_eq!(watching.events().len(), 1, "only its `watching` line: {:#?}", watching.events());
    drop(watching);
    drop(t);
    let mut t = sb.spawn(&["a.txt"]);
    t.wait_text("two");
    // (as at the first start: the gutter's mark under the picker landed in the window — its `+1`, its `▎`)
    t.wait_for("the line's blame and its change's mark", git_landed);
    t.send(" f");
    t.wait_for("the picker", |s| s.contains("files in work"));
    sb.write_config(&format!("{BASE_CONFIG}tab-width = \"wide\"\n"));
    // (f22 n: on Linux this never came, every run — so a timeout says what tarae did say: its toasts, and its
    // log's last lines — whether the config was read again, what it said, what came after it)
    if t.poll_for(sb.timeout(), |s| s.status().contains("config:")).is_none() {
        let state: serde_json::Value = serde_json::from_str(&t.state(&["--json"])).unwrap_or_default();
        let log = fs::read_to_string(sb.state().join("tarae.log")).unwrap_or_default();
        let tail: Vec<&str> = log.lines().rev().take(40).collect();
        let tail: Vec<&str> = tail.into_iter().rev().collect();
        panic!(
            "no config error on the status line after {:?} — toasts: {:#}\nthe log's end:\n{}\n{}",
            sb.timeout(),
            state["toasts"],
            tail.join("\n"),
            t.screen()
        );
    }
    idle(&mut t, "a picker over an error");
    // The picker gone, the card comes up — first drawn faint, its fade-in's own ticks must bring it to full
    // strength (left as first drawn, it would stay transparent). Judged by what drew the frame that drew it
    // whole, never by frames counted (CI's macOS, ci round 5: a slow runner's next frame came after the fade's
    // whole 150 ms — one frame, already whole): nothing asked meanwhile — an ask is a frame, drawing the fade
    // forward itself — a watcher hears when the card is first drawn whole (`toast-whole`, which frame); that
    // frame was drawn for a tick (`frames_for`), not the resting redraw nor an ask.
    let watching = t.watch("toasts");
    watching.wait("its watching line", |_| true);
    t.esc("the card", |s| !s.contains("files in work") && s.contains("expected integer"));
    let whole = watching.wait("the card drawn whole", |e| e["event"] == "toast-whole");
    drop(watching);
    let s = t.settle("the card's fade over", |s| s["toasts"].as_array().is_some_and(|ts| !ts.is_empty()));
    let by =
        s["frames_for"].as_array().and_then(|fs| fs.iter().find(|f| f["frame"] == whole["written"])).cloned();
    assert!(
        by.as_ref().and_then(|f| f["for"].as_array()).is_some_and(|f| f.iter().any(|w| w == "a tick")),
        "the frame that drew the card whole ({}) wasn't its fade's tick: {by:?}\nthe frames: {:#}",
        whole["written"],
        s["frames_for"]
    );
}

/// A client connected to `sock` that asks nothing (`nc -U`, from the socket's folder — its path may be past
/// `sun_path`'s 104 bytes); gone when dropped. None where there's no `nc` with `-U`.
fn connected(sock: &std::path::Path) -> Option<std::process::Child> {
    let child = std::process::Command::new("/bin/sh")
        .args(["-c", "exec nc -U \"$0\"", &sock.file_name().unwrap().to_string_lossy()])
        .current_dir(sock.parent().unwrap())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    std::thread::sleep(std::time::Duration::from_millis(200));
    let mut child = child;
    // (macOS has `nc -U`: still there = connected, and asking nothing)
    if cfg!(target_os = "macos") {
        assert!(matches!(child.try_wait(), Ok(None)), "nc couldn't connect to {}", sock.display());
    }
    Some(child)
}

/// A driver reads where the keys go before sending — the four slips of 2026-10-04 through the real binary
/// and its socket (`tarae state`): the code's keys; the terminal's program having every key, then its view
/// mode (the status line says `● view · terminal  i type`); a card the chat's draft keeps from its `y`;
/// `--after-input` answering about the keys just sent. The terminal's shell finds this tarae (`TARAE_UI`).
#[test]
fn a_driver_reads_where_the_keys_go() {
    let sb = Sandbox::new("uistate");
    sb.write("a.txt", "one\n");
    let mut t = sb.spawn(&["a.txt"]);
    t.wait_text("one");
    let s = t.state_json(0);
    assert_eq!(s["v"], 14, "the schema's version (`uistate::V`)");
    assert_eq!((s["keys"]["region"].as_str(), s["keys"]["takes"].as_str()), (Some("code"), Some("commands")));
    assert_eq!(s["tarae"]["pid"], t.pid());
    let input = s["input"].as_u64().unwrap();
    // The terminal: every key the program's
    t.send(" T");
    let s = t.state_json(input + 2);
    assert_eq!(s["input"].as_u64(), Some(input + 2), "about the two keys sent");
    t.wait_for("the shell", |s| s.contains("$ ") || s.contains("# "));
    let s = t.state_json(input + 2);
    assert_eq!(
        (s["keys"]["region"].as_str(), s["keys"]["takes"].as_str()),
        (Some("terminal-program"), Some("program"))
    );
    t.send("echo ui=${TARAE_UI##*/}\r");
    t.wait_text(&format!("ui={}.sock", t.pid()));
    // `C-\` from the program: its view mode — tarae's keys on its panel
    let input = t.state_json(0)["input"].as_u64().unwrap();
    t.send("\x1c");
    let s = t.state_json(input + 1);
    assert_eq!(
        (s["keys"]["region"].as_str(), s["keys"]["takes"].as_str()),
        (Some("view:terminal"), Some("commands"))
    );
    t.wait_for("the status line says so", |s| {
        s.status().contains("● view · terminal") && s.status().contains("i type")
    });
    assert!(t.state(&[]).lines().nth(1).unwrap().starts_with("keys → terminal (commands)"));
    drop(t);
}

/// Linux: tarae is the last parent of what its programs leave running (`reaper::at_start`), and what a run of
/// Claude's left ends when tarae quits (`sandbox::end_leftovers`). The stand-in: a `sleep` the terminal's
/// shell leaves behind (a subshell gone, its own session) carrying a run tag of this tarae's — one tarae no
/// longer knows, as after a `:restart`.
#[cfg(target_os = "linux")]
#[test]
fn what_a_run_left_running_is_taraes_and_ends_when_it_quits() {
    let sb = Sandbox::new("left");
    sb.write("a.txt", "one\n");
    let mut t = sb.spawn(&["a.txt"]);
    t.wait_text("one");
    t.send(" T");
    t.wait_for("the shell", |s| s.contains("$ ") || s.contains("# "));
    let file = sb.file("left.pid");
    t.send(&format!(
        "(TARAE_SANDBOXED=tarae-{}-999999. setsid sleep 97 & echo $! > '{}')\r",
        t.pid(),
        file.display()
    ));
    let end = std::time::Instant::now() + sb.timeout();
    let pid: u32 = loop {
        if let Some(p) = std::fs::read_to_string(&file).ok().and_then(|s| s.trim().parse().ok()) {
            break p;
        }
        assert!(std::time::Instant::now() < end, "the shell left its sleep");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    let parent = |pid: u32| {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        stat[stat.rfind(')')? + 2..].split(' ').nth(1)?.parse::<u32>().ok()
    };
    // (its subshell gone, it's re-parented — to tarae)
    let end = std::time::Instant::now() + sb.timeout();
    while parent(pid) != Some(t.pid()) && std::time::Instant::now() < end {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let adopted = parent(pid);
    // Quit: `C-\` gives the keys back to tarae (its view mode), `C-\` again to the code
    t.send("\x1c\x1c:q!\r");
    t.wait_exit();
    let end = std::time::Instant::now() + TIMEOUT;
    while std::path::Path::new(&format!("/proc/{pid}")).exists() && std::time::Instant::now() < end {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let left = std::path::Path::new(&format!("/proc/{pid}")).exists();
    if left {
        // SAFETY: the test's own stand-in.
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };
    }
    assert_eq!(adopted, Some(t.pid()), "tarae is its parent");
    assert!(!left, "ended with tarae");
}

/// Watching over the socket (`tarae state --watch`): the trust card up from the start goes with `n` — told
/// as `card-gone`, `answered n`; the keys going to the terminal's program — told as `keys`; each event with
/// the frame that showed it and the input it followed, in plain words too.
#[test]
fn a_watcher_hears_what_changes() {
    let sb = Sandbox::new("watch");
    sb.write(".tarae.toml", "[editor]\nrestart-build = \"true\"\n");
    sb.write("a.txt", "one\n");
    let mut t = sb.spawn(&["a.txt"]);
    let card = focus_card(&mut t, "the trust card", |c| c["kind"] == "project-trust");
    let id = card["cards"][0]["id"].as_str().unwrap().to_string();
    let w = t.watch("");
    t.send("n");
    let gone = w.wait("the card gone", |e| e["event"] == "card-gone");
    assert_eq!((gone["id"].as_str(), gone["how"].as_str()), (Some(id.as_str()), Some("answered n")));
    assert!(gone["said"].as_str().unwrap().contains("answered n — Trust this project"), "{gone}");
    t.send(" T");
    let keys = w.wait("the keys to the terminal's program", |e| {
        e["event"] == "keys" && e["region"] == "terminal-program"
    });
    assert_eq!(keys["said"], "keys → terminal (program)");
    let s = t.state_json(0);
    assert!(keys["frame"].as_u64() <= s["frame"].as_u64() && keys["input"].as_u64() <= s["input"].as_u64());
    // The gone ring remembers it
    assert!(s["gone"].as_array().unwrap().iter().any(|g| g["id"] == id.as_str() && g["how"] == "answered n"));
}

/// A card that comes on its own waits to be focused (P3 §3.8), and a driver reads it before it sends:
/// `keys.waiting` names it and the card `waits: focus`; a `y` sent meanwhile is the code's (a yank), the card stays
/// up, the status line says `▲1 waiting · C-q`; `C-q`, and the next state has the keys the card's. The card is the
/// project's trust, asked as a `.tarae.toml` written mid-session is read — no key asked for it.
#[test]
fn a_card_that_comes_on_its_own_waits_and_the_state_says_so() {
    let sb = Sandbox::new("cardwait");
    sb.write("a.txt", "one\ntwo\nthree\n");
    let mut t = sb.spawn(&["a.txt"]);
    t.wait_text("one");
    sb.write(".tarae.toml", "[editor]\nrestart-build = \"true\"\n");
    let s = t.wait_state("the trust card", |s| {
        s["cards"].as_array().into_iter().flatten().any(|c| c["kind"] == "project-trust")
    });
    let id = s["cards"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(
        (s["keys"]["region"].as_str(), s["keys"]["waiting"].as_str()),
        (Some("code"), Some(id.as_str()))
    );
    assert_eq!(
        (s["cards"][0]["waits"].as_str(), s["cards"][0]["focused"].as_bool()),
        (Some("focus"), Some(false))
    );
    // A `y` while it waits: the code's
    let input = s["input"].as_u64().unwrap();
    t.send("y");
    let s = t.state_json(input + 1);
    assert!(s["cards"].as_array().is_some_and(|c| c.iter().any(|c| c["id"] == id.as_str())), "still up");
    assert_eq!(s["keys"]["waiting"], id.as_str(), "still waiting");
    t.wait_for("the status line says it waits", |s| s.status().contains("▲1 waiting · C-q"));
    // `C-q`: the keys the card's, nothing waits
    t.send("\x11");
    let s = t.state_json(input + 2);
    assert_eq!(s["keys"]["to"], id.as_str(), "{s}");
    assert!(s["keys"]["waiting"].is_null() && s["cards"][0]["waits"].is_null(), "{s}");
    t.wait_for("the status line says the card has them", |s| !s.status().contains("waiting"));
}

/// The card in front that `pick` matches, focused: `C-q` unless it has the keys already (your own key's card is
/// focused as it comes — P3 §3.8). The state once it has them.
fn focus_card(t: &mut Term, what: &str, pick: impl Fn(&serde_json::Value) -> bool) -> serde_json::Value {
    let front = |s: &serde_json::Value| -> Option<bool> {
        let c = s["cards"].as_array().into_iter().flatten().find(|c| c["up"] == "front" && pick(c))?;
        Some(c["focused"] == true)
    };
    let s = t.wait_state(what, |s| front(s).is_some());
    if front(&s) == Some(false) {
        t.send("\x11");
    }
    t.wait_state(&format!("{what}, focused"), |s| front(s) == Some(true))
}

/// A chat turn as a watcher hears it: `turn started` as the message goes, `turn ended` with its time when
/// the answer's in (a stand-in claude answering after a second).
#[test]
fn a_watcher_hears_a_turn_begin_and_end() {
    let sb = Sandbox::new("watch-turn");
    let fake = "while read -r line; do\n  case \"$line\" in '{\"message\"'*) ;; *) continue;; esac\n  sleep 1\n  \
                printf '%s\\n' '{\"type\":\"result\",\"result\":\"done\"}'\ndone\n";
    let script = sb.write("fake-claude.sh", fake);
    sb.write_config(&format!("{BASE_CONFIG}[llm]\ncommand = \"sh\"\nargs = [\"{}\"]\n", script.display()));
    sb.write("a.txt", "one\n");
    let mut t = sb.spawn(&["a.txt"]);
    t.send(" l");
    t.wait_state("the chat has the keys", |s| s["keys"]["region"] == "chat");
    let w = t.watch("turns");
    t.send("hello\r");
    let started = w.wait("the turn started", |e| e["event"] == "turn" && e["what"] == "started");
    assert_eq!(started["session"], "s0");
    let ended = w.wait("the turn ended", |e| e["event"] == "turn" && e["what"] == "ended");
    assert!(ended["took_ms"].as_u64().is_some_and(|ms| ms >= 900), "{ended}");
    assert!(ended["said"].as_str().unwrap().contains("Claude's turn ended — 0 calls"), "{ended}");
    assert!(
        w.events().iter().skip(1).all(|e| e["event"] == "turn"),
        "only turns asked for: {:#?}",
        w.events()
    );
}

/// 번짐 asks no frame of its own: an answer streamed in parts, its head bleeding as it comes (on in the binary: the
/// head in other ink than the words before it while the answer waits mid-way), then the turn's end — the head
/// in the ink of the rest, and after that frame nothing is drawn.
#[test]
fn bleeding_ink_draws_no_frame_after_the_turn() {
    let sb = Sandbox::new("bleeding-ink");
    let delta = |t: &str| {
        format!(
            r#"{{"type":"stream_event","event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"{t}"}}}}}}"#
        )
    };
    let lines = [
        r#"{"type":"system","subtype":"init","session_id":"c1","model":"claude-opus-5-5"}"#.to_string(),
        delta("The name is cloned "),
        delta("because insert takes "),
        delta("the key by value."),
        r#"{"type":"result","subtype":"success","result":"done","session_id":"c1"}"#.to_string(),
    ];
    // (each part a moment after the one before: frames drawn with the head bleeding; mid-way it waits a while)
    let body: Vec<String> = lines
        .iter()
        .enumerate()
        .map(|(i, l)| format!("  printf '%s\\n' '{l}'; sleep {}\n", if i == 2 { "3" } else { "0.2" }))
        .collect();
    let fake = format!(
        "while read -r line; do\n  case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac\n{}done\n",
        body.concat()
    );
    let script = sb.write("fake-claude.sh", &fake);
    sb.write_config(&format!("{BASE_CONFIG}[llm]\ncommand = \"sh\"\nargs = [\"{}\"]\n", script.display()));
    sb.write("a.txt", "one\n");
    let mut t = sb.spawn(&["a.txt"]);
    t.send(" l");
    t.wait_state("the chat has the keys", |s| s["keys"]["region"] == "chat");
    t.send("why?\r");
    // (single words: the follow card or the panel may wrap the line anywhere)
    t.wait_for("the answer mid-way", |s| s.contains("takes"));
    let (dry, bleeding) = (t.fg_of("cloned"), t.fg_of("takes"));
    assert!(
        dry.is_some() && bleeding != dry,
        "mid-way, the head in other ink: {bleeding:?} after {dry:?}\n{}",
        t.screen()
    );
    t.wait_for("the answer, whole", |s| s.contains("the key by value."));
    t.wait_state("the turn ended", |s| s["sessions"][0]["doing"] == "idle");
    t.wait_for("the answer in the panel", |s| s.contains("value.") && !s.contains("C-c stop"));
    let (head, rest) = (t.fg_of("value."), t.fg_of("cloned"));
    assert_eq!(head, rest, "the turn ended: the head in the ink of the rest\n{}", t.screen());
    // Settled (the frame that draws it dry, every frame read, no redraw owed — `Term::settle`) — then nothing: no
    // tick for the ink drying
    t.settle("the turn's last frame", |s| s["motions"].as_array().is_some_and(Vec::is_empty));
    // (nor a wake: a tick that draws nothing new is no frame, but it wakes the loop — at rest it's a few
    // switches in 2 s, a 100 ms tick ~100, as `an_idle_editor_draws_nothing_and_sleeps` measures)
    let (frames, before) = (t.frames(), t.usage());
    std::thread::sleep(Duration::from_secs(2));
    let (drawn, used) = (t.frames() - frames, t.usage() - before);
    assert!(
        drawn == 0 && used.switches < 30,
        "after the turn's end: {drawn} frames, {used:?}\n{}",
        t.screen()
    );
}

/// A flash on its own clock, then sleep: `u` flashes what it took back — its key's frame, a step as it dries, one as it
/// goes (`calm` under Reduce Motion: the key's and the going's) — and once it's gone (the state's `motions` empty)
/// nothing is drawn and nothing wakes the loop.
#[test]
fn a_motion_ends_and_tarae_sleeps() {
    let sb = Sandbox::new("motion");
    sb.write("a.txt", "one two\n");
    let mut t = sb.spawn(&["a.txt"]);
    // (what undo brings back flashes: `one ` again, the cursor on one of its cells)
    t.send("wd");
    t.wait_for("the word gone", |s| !s.contains("one two"));
    let before = t.frames();
    t.send("u");
    t.wait_for("undone", |s| s.contains("one two"));
    t.wait_state("the flash gone", |s| s["motions"].as_array().is_some_and(Vec::is_empty));
    let drawn = t.frames() - before;
    assert!(drawn >= 2, "the key's frame and the flash's going: {drawn} frames\n{}", t.screen());
    // Settled (every frame read, no redraw owed — `Term::settle`, never a quiet second) — then nothing: no tick for
    // the flash
    t.settle("the flash's last frame", |s| s["motions"].as_array().is_some_and(Vec::is_empty));
    let (frames, before) = (t.frames(), t.usage());
    std::thread::sleep(Duration::from_secs(2));
    let (drawn, used) = (t.frames() - frames, t.usage() - before);
    assert!(drawn == 0 && used.switches < 30, "after the flash: {drawn} frames, {used:?}\n{}", t.screen());
}

/// A chat turn's usage on screen: a stand-in claude streaming what claude does (a request writing 4k to
/// the 5m cache, its result with the conversation's cost and the model's window) — the line under the chat
/// says until when the cache stays warm, the context's share, the turn's cost; `:usage` lists the turn; the
/// state says the same (with seconds left).
#[test]
fn a_turns_usage_shows_under_the_chat_and_in_usage() {
    let sb = Sandbox::new("usage");
    let lines = [
        r#"{"type":"system","subtype":"init","session_id":"c1","model":"claude-opus-5-5"}"#,
        r#"{"type":"system","subtype":"status","status":"requesting"}"#,
        r#"{"type":"stream_event","event":{"type":"message_start","message":{"id":"m1","model":"claude-opus-5-5","usage":{"input_tokens":6,"cache_creation_input_tokens":4000,"cache_read_input_tokens":20000,"cache_creation":{"ephemeral_5m_input_tokens":4000,"ephemeral_1h_input_tokens":0},"output_tokens":1}}}}"#,
        r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello."}}}"#,
        r#"{"type":"stream_event","event":{"type":"message_delta","usage":{"input_tokens":6,"cache_creation_input_tokens":4000,"cache_read_input_tokens":20000,"output_tokens":120}}}"#,
        r#"{"type":"result","subtype":"success","result":"Hello.","session_id":"c1","duration_ms":900,"total_cost_usd":0.0412,"usage":{"input_tokens":6,"cache_creation_input_tokens":4000,"cache_read_input_tokens":20000,"output_tokens":120},"modelUsage":{"claude-opus-5-5":{"costUSD":0.0412,"contextWindow":1000000}}}"#,
    ];
    let body: Vec<String> = lines.iter().map(|l| format!("  printf '%s\\n' '{l}'\n")).collect();
    let fake = format!(
        "while read -r line; do\n  case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac\n{}done\n",
        body.concat()
    );
    let script = sb.write("fake-claude.sh", &fake);
    sb.write_config(&format!("{BASE_CONFIG}[llm]\ncommand = \"sh\"\nargs = [\"{}\"]\n", script.display()));
    sb.write("a.txt", "one\n");
    let mut t = sb.spawn(&["a.txt"]);
    t.send(" l");
    t.wait_state("the chat has the keys", |s| s["keys"]["region"] == "chat");
    t.send("hello\r");
    // (the chat is 40 wide here: the footer's short form; warm until five minutes on, by the clock)
    t.wait_for("the usage line", |s| {
        s.rows.iter().any(|r| r.contains("● warm until ") && r.contains("ctx 2%  ≈$0.041"))
    });
    let s = t.wait_state("the session's usage", |s| s["sessions"][0]["usage"]["cache"] == "warm");
    let u = &s["sessions"][0]["usage"];
    assert_eq!((u["ttl"].as_str(), u["context"]["tokens"].as_u64()), (Some("5m"), Some(24_006)), "{u}");
    assert_eq!(u["last_turn"]["cost"], 0.0412);
    // (`esc` the chat's view mode — the conversation under a cursor — `esc` again the code)
    t.esc_state("the chat's view mode", |s| s["keys"]["region"] == "view:chat");
    t.esc_state("the code has the keys", |s| s["keys"]["region"] == "code");
    t.command("usage");
    // (the costs said once to be the API price — #161; today's total at the end, below the screen)
    t.wait_for("the usage view", |s| {
        s.contains("# Claude usage")
            && s.contains("Costs are the API price")
            && s.contains("| total | 1 turn")
    });
    t.send("G");
    t.wait_for("today's total", |s| s.contains("Every session: $0.041 in 1 turn"));
}

/// A state folder past `sun_path`'s 104 bytes: the socket is bound from its folder, and `tarae state`
/// reaches it the same way.
#[test]
fn the_socket_lives_in_a_long_state_folder() {
    let sb = Sandbox::new("uistate-long");
    let deep = sb.home().join(format!(".local/{}/state", "very-long-folder-name/".repeat(4)));
    std::fs::create_dir_all(&deep).unwrap();
    let sb = sb.with_env("XDG_STATE_HOME", &deep.to_string_lossy());
    let t = sb.spawn(&[]);
    let sock = deep.join(format!("tarae/ui/{}.sock", t.pid()));
    assert!(sock.as_os_str().len() > 104, "{}", sock.display());
    assert!(sock.exists(), "{} listens", sock.display());
    assert_eq!(t.state_json(0)["keys"]["region"], "code");
}

#[test]
fn file_picker_opens_lists_and_survives_no_matches() {
    let sb = Sandbox::new("picker");
    fs::create_dir_all(sb.file("src")).unwrap();
    sb.write("src/alpha.rs", "fn alpha() {}\n");
    sb.write("notes.md", "# notes\n");
    let mut t = sb.spawn(&["notes.md"]);
    t.wait_text("# notes");
    t.send(" f");
    t.wait_for("the tree", |s| s.contains("src/") && s.contains("notes.md"));
    t.send("zzqx");
    t.wait_for("no matches", |s| s.contains("› zzqx"));
    assert!(t.running(), "still running");
    t.esc("the picker closed", |s| !s.contains("› zzqx"));
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));
}

/// The project search (`space /`) with no rg anywhere — the harness's PATH is `/usr/bin:/bin`: ripgrep's
/// crates search in-process, hidden files too, and Enter opens the hit at its line.
#[test]
fn the_project_search_needs_no_rg() {
    let sb = Sandbox::new("search");
    fs::create_dir_all(sb.file(".github")).unwrap();
    sb.write("notes.md", "# notes\n");
    sb.write("src.rs", "fn one() {}\n\nfn needle_here() {}\n");
    sb.write(".github/ci.yml", "needle: ci\n");
    let mut t = sb.spawn(&["notes.md"]);
    t.wait_text("# notes");
    t.send(" /");
    t.wait_for("the search", |s| s.contains("type to search"));
    t.send("needle");
    t.wait_for("both hits", |s| s.contains("needle: ci") && s.contains("fn needle_here() {}"));
    t.send("\x1b[B\r");
    t.wait_for("src.rs at the hit", |s| s.rows[0].contains("› src.rs") && s.status().contains(" 3:4 "));
}

/// `tarae <folder>`: tarae works there on the start screen — even with files kept to reopen there — and the
/// file picker lists its files; `editor.restore-files` reopens them. A quit from the start screen keeps
/// them; closing every file clears them. A file given beside it is read from where the command ran.
#[test]
fn a_folder_argument_opens_there_on_the_start_screen() {
    let sb = Sandbox::new("folder");
    fs::create_dir_all(sb.file("app/src")).unwrap();
    sb.write("app/src/lib.rs", "pub fn lib() {}\n");
    sb.write("outside.txt", "from outside\n");
    let mut t = sb.spawn(&["app"]);
    t.wait_text("t a r a e");
    t.send(" f");
    t.wait_for("the picker on app's files", |s| s.contains("src/") && s.contains("files in app"));
    t.send("lib\r");
    t.wait_text("pub fn lib");
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));

    // lib.rs was open on quit — the next start is the start screen all the same, and quitting from it
    // leaves them alone
    let mut t = sb.spawn(&["app"]);
    let s = t.wait_text("t a r a e");
    assert!(!s.contains("pub fn lib"), "{s}");
    assert!(s.contains("Continue where you left off") && s.contains("lib.rs"), "offered: {s}");
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));

    // With the setting on it comes back; closing it before quitting clears what's kept
    sb.write_config(&format!("{BASE_CONFIG}restore-files = true\n"));
    let mut t = sb.spawn(&["app"]);
    t.wait_text("pub fn lib");
    t.command("bc");
    t.wait_text("t a r a e");
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));

    let mut t = sb.spawn(&["app"]);
    t.wait_text("t a r a e");
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));

    let mut t = sb.spawn(&["app", "outside.txt"]);
    t.wait_text("from outside");
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));
}

/// `:restart`'s card (which checkout it builds in, #108) answered `y` once it's in front with the keys — yours, it
/// comes in front of the project's trust card (up since the start: its `.tarae.toml` sets the binary), focused.
fn yes_to_restart(t: &mut Term) {
    focus_card(t, "the restart card", |c| c["kind"] == "restart");
    t.send("y");
}

/// `:restart` on a real terminal: the build runs as a task, then the new binary (this same one here) takes
/// the process over — same pid, the pty still its terminal — and comes back as it was: both panes, each
/// file's cursor, the focused one current. Twice, so a restarted tarae restarts too; then it quits cleanly.
#[test]
fn restart_becomes_the_new_binary_as_you_were() {
    let sb = Sandbox::new("restart");
    sb.write(
        ".tarae.toml",
        &format!("[editor]\nrestart-build = \"echo built\"\nrestart-binary = \"{BIN}\"\n"),
    );
    let body: String = (1..=40).map(|i| format!("line {i}\n")).collect();
    sb.write("a.txt", &body);
    sb.write("b.txt", "bee one\nbee two\n");
    let mut t = Term::spawn(&sb, &["a.txt"], 120, ROWS);
    t.wait_text("line 1");
    t.send("5j");
    t.wait_for("cursor on line 6 of a", |s| s.status().contains("6:1"));
    t.command("vsplit b.txt");
    t.wait_text("bee two");
    t.send("j");
    t.wait_for("cursor on line 2 of b", |s| s.status().contains("2:1"));

    let entered = |raw: &[u8]| raw.windows(8).filter(|w| *w == b"\x1b[?1049h").count();
    t.command("restart");
    yes_to_restart(&mut t);
    t.wait_raw("a second tarae entering the alternate screen", |r| entered(r) >= 2);
    let s = t.wait_text("Restarted · 2 files as they were");
    assert!(t.running(), "the same process, still running");
    assert!(s.contains("bee two") && s.contains("line 6"), "both panes:\n{s}");
    assert!(s.status().contains("2:1"), "b focused, its cursor kept:\n{s}");
    // (`C-w h`: `C-w w`'s round from the last pane goes on to the trust card waiting — P3 §3.8)
    t.send("\x17h");
    t.wait_for("the other pane: a at line 6", |s| s.status().contains("6:1"));

    t.command("restart!");
    yes_to_restart(&mut t);
    t.wait_raw("a third tarae", |r| entered(r) >= 3);
    t.wait_for("a back at line 6, focused", |s| s.status().contains("6:1") && s.contains("bee two"));
    let state = sb.home().join(".local/state/tarae");
    let left: Vec<_> = fs::read_dir(&state)
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("restart-"))
        .collect();
    assert!(left.is_empty(), "the state file goes once read: {left:?}");
    t.command("q");
    t.command("q");
    assert_eq!(t.wait_exit().code(), Some(0));
}

/// A file just under the size that loads in the background, given 1000 undo steps (`C-s` in insert mode,
/// the most kept) and a last one typing ` END ` — enough that writing its undo history takes a while: the
/// fingerprint and every step are worked out over the whole text.
fn edit_big_file(sb: &Sandbox, t: &mut Term) {
    t.wait_text("line number 1");
    t.send("i");
    t.wait_for("INSERT mode", |s| s.status().contains("● insert"));
    t.send(&format!("{} END ", "x\x13".repeat(1000)));
    t.wait_text("END");
    t.normal_mode();
    assert!(fs::metadata(sb.file("big.txt")).unwrap().len() > 800_000);
}

fn big_file(sb: &Sandbox) {
    sb.write("big.txt", &(1..=50_000).map(|i| format!("line number {i}\n")).collect::<String>());
}

/// `u` in a tarae started afresh takes back the last step (the history came from disk): tried until the
/// history read on a worker is in.
fn undo_reaches_back(t: &mut Term) {
    let undone = |s: &Screen| !s.contains("END");
    let end = std::time::Instant::now() + TIMEOUT;
    while std::time::Instant::now() < end {
        t.send("u");
        if t.poll_for(std::time::Duration::from_millis(250), undone).is_some() {
            return;
        }
    }
    panic!("u didn't take the last step back — no undo history\n{}", t.screen());
}

/// `:wq` right after editing: the undo history is written (on a worker) before tarae exits — exiting used to
/// cut that write off — so the next tarae's `u` still takes the edit back.
#[test]
fn undo_history_is_on_disk_before_quitting() {
    let sb = Sandbox::new("undoquit");
    big_file(&sb);
    let mut t = sb.spawn(&["big.txt"]);
    edit_big_file(&sb, &mut t);
    t.command("wq");
    assert_eq!(t.wait_exit().code(), Some(0));
    // (cut off, the write leaves only its temp file — `.<name>.json.tarae-….tmp`)
    let undo = sb.home().join(".local/state/tarae/undo");
    let names: Vec<String> = fs::read_dir(&undo)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| n.ends_with(".json") && !n.starts_with('.')),
        "the undo history wasn't written before tarae exited: {names:?}"
    );
    assert!(!names.iter().any(|n| n.ends_with(".tmp")), "a write cut off halfway: {names:?}");

    let mut t = sb.spawn(&["big.txt"]);
    t.wait_text("END");
    undo_reaches_back(&mut t);
    t.command("q!");
    assert_eq!(t.wait_exit().code(), Some(0));
}

/// `:restart` saves first, and the new tarae has the undo history of that save: the write isn't cut off by
/// the exec.
#[test]
fn undo_history_comes_along_on_restart() {
    let sb = Sandbox::new("undorestart");
    sb.write(".tarae.toml", &format!("[editor]\nrestart-binary = \"{BIN}\"\n"));
    big_file(&sb);
    let mut t = sb.spawn(&["big.txt"]);
    edit_big_file(&sb, &mut t);
    t.command("restart");
    yes_to_restart(&mut t);
    t.wait_text("Restarted · 1 file as it was");
    undo_reaches_back(&mut t);
    t.command("q!");
    assert_eq!(t.wait_exit().code(), Some(0));
}

/// Follow mode the way it failed when watched live: review at the end on, a stand-in claude reads, edits a
/// file four times (each asked for, allowed unseen, written by claude), searches and reads again — a stop
/// every 0.4 s. Each reaches the screen in order within a bound, nothing skipped: an edit with its new lines
/// and the replaced ones above them (`−`), the card naming it; the search with its count. Then the turn
/// replays stop by stop with `[r` / `]r`.
#[test]
fn follow_shows_each_edit_and_replays_the_turn() {
    use serde_json::json;
    let sb = Sandbox::new("follow-replay");
    let lines = |n: usize| (1..=n).map(|i| format!("line {i}\n")).collect::<String>();
    let main = sb.write("main.txt", &lines(3));
    let lib = sb.write("lib.txt", &lines(10));
    let file = sb.write("edit.txt", &lines(30));
    let edits = [
        ("line 3\n", "three\n"),
        ("line 10\n", "line 10\nadded a\nadded b\n"),
        ("line 20\n", ""),
        ("line 1\n", "zero\nline 1\n"),
    ];
    let mut text = lines(30);
    let tool = |id: &str, name: &str, input: serde_json::Value| json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": name, "input": input}]}});
    let result = |id: &str, text: &str| json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": id, "content": text}]}});
    let say = |v: serde_json::Value| format!("  printf '%s\\n' '{v}'\n");
    let mut turn = say(tool("r1", "Read", json!({"file_path": main, "offset": 1, "limit": 3})));
    turn += &say(result("r1", "1\tline 1"));
    turn += "  sleep 0.4\n";
    for (k, (old, new)) in edits.iter().enumerate() {
        text = text.replacen(old, new, 1);
        let input = json!({"file_path": file, "old_string": old, "new_string": new});
        turn += &say(tool(&format!("e{k}"), "Edit", input.clone()));
        turn += &say(json!({"type": "control_request", "request_id": format!("q{k}"),
            "request": {"subtype": "can_use_tool", "tool_name": "Edit", "input": input}}));
        // (claude waits for tarae's answer, writes the file, then says it's done)
        turn += &format!(
            "  while read -r l; do case \"$l\" in *'\"request_id\":\"q{k}\"'*) break;; esac; done\n  printf '%s' '{text}' > '{}'\n",
            file.display()
        );
        turn += &say(result(&format!("e{k}"), "The file has been updated."));
        turn += "  sleep 0.4\n";
    }
    turn += &say(tool("s1", "mcp__tarae__search", json!({"pattern": "line 5"})));
    turn += &say(result("s1", "1 matching line in 1 file:\nlib.txt\n    5: line 5\n"));
    turn += "  sleep 0.4\n";
    turn += &say(tool("r2", "Read", json!({"file_path": lib, "offset": 8, "limit": 2})));
    turn += &say(result("r2", "8\tline 8"));
    turn += "  sleep 0.4\n";
    turn += &say(json!({"type": "result", "result": "done"}));
    let fake = format!(
        "while read -r line; do\n  case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac\n{turn}done\n"
    );
    let script = sb.write("fake-claude.sh", &fake);
    // (its fake writes the file as Claude Code's Edit does — the arm with Edit)
    sb.write_config(&format!(
        "{BASE_CONFIG}[llm]\ncommand = \"sh\"\nargs = [\"{}\"]\nreview-at-end = true\nedit-tools = \"native\"\n",
        script.display()
    ));
    let mut t = Term::spawn(&sb, &["main.txt"], 120, 40);
    t.send(" l");
    t.send("change it\r");
    // Live: each stop in order, each within a bound of the one before (they come 0.4 s apart) — the follow
    // card's knot rows (글타래 P6): `●` the stop in hand, `▲` every edit; a search's count cut to the row's room
    // (`1 matching l…` at 120 columns)
    let removed = |s: &Screen, gone: &str| s.rows.iter().any(|r| r.contains('−') && r.contains(gone));
    type Seen = Box<dyn Fn(&Screen) -> bool>;
    let stops: [(&str, Seen); 7] = [
        ("the read", Box::new(|s| s.contains("● read   main.txt  L1–3"))),
        (
            "edit 1, its line and the one it replaced",
            Box::new(move |s| {
                s.contains("▲ edit   edit.txt  L3 ") && s.contains("three") && removed(s, "line 3")
            }),
        ),
        ("edit 2", Box::new(|s| s.contains("▲ edit   edit.txt  L11–12") && s.contains("added b"))),
        (
            "edit 3, a line gone",
            Box::new(move |s| s.contains("▲ edit   edit.txt  L22") && removed(s, "line 20")),
        ),
        ("edit 4", Box::new(|s| s.contains("▲ edit   edit.txt  L1 ") && s.contains("zero"))),
        (
            "the search, named, with its count",
            Box::new(|s| s.contains("● search (project) line 5  1 matching")),
        ),
        ("the last read", Box::new(|s| s.contains("● read   lib.txt  L8–9"))),
    ];
    // (a stop skipped is never seen whatever the bound — it only has to outlast a slow runner's lag)
    for (what, seen) in &stops {
        if t.poll_for(TIMEOUT, |s| seen(s)).is_none() {
            panic!("{what} never reached the screen in time\n{}", t.screen());
        }
    }
    // The turn over: its review opens (review at the end) and takes the keys from the chat, so the chat's
    // row says how to replay the turn from the code — then stop by stop, back from the newest. (`C-]`, said
    // while the chat still has the keys, shows only until the turn's files are gathered on a worker.)
    t.wait_for("the review, with the keys, and the replay's hint", |s| {
        s.contains("reviewing them now") && s.contains("[r ]r replay its 7 stops")
    });
    let step = |t: &mut Term, keys: &str, what: &str, seen: &dyn Fn(&Screen) -> bool| {
        t.send(keys);
        t.wait_for(what, seen);
    };
    // (the card's knot in hand reads its stop as it is now: edit 1's `L4`, its chat row the `L3` it was made at)
    step(&mut t, "[r", "the newest stop", &|s| s.contains("● read   lib.txt  L8–9") && s.contains("7/7"));
    step(&mut t, "[r", "the search", &|s| s.contains("● search (project)") && s.contains("6/7"));
    step(&mut t, "[r", "edit 4", &|s| s.contains("▲ edit   edit.txt  L1 ") && s.contains("5/7"));
    step(&mut t, "[r[r[r", "edit 1, a line down since edit 4", &|s| {
        s.contains("▲ edit   edit.txt  L4") && s.contains("2/7") && removed(s, "line 3")
    });
    step(&mut t, "]r]r]r]r]r]r", "past the newest", &|s| s.contains("that was the newest stop"));
}

/// Frictions 13, #3: a click on a file row of the review's rail, as the terminal sends it (SGR mouse bytes,
/// press then release, 1-based), opens that file — with the chat open and focused beside it, on a 188×57
/// screen, its folders three deep.
#[test]
fn a_click_on_the_review_rail_opens_the_file() {
    let sb = Sandbox::new("rail-click");
    fs::write(sb.git_config(), "[user]\n\tname = t\n\temail = t@t\n[commit]\n\tgpgsign = false\n").unwrap();
    let rels = ["runtime/packs/c-sharp/pack.toml", "runtime/packs/go/pack.toml", "src/a.rs", "src/b.rs"];
    for rel in rels {
        fs::create_dir_all(sb.file(rel).parent().unwrap()).unwrap();
        sb.write(rel, "one\n");
    }
    sb.git(&["init", "-q", "-b", "main"]);
    sb.git(&["add", "-A"]);
    sb.git(&["commit", "-q", "-m", "base"]);
    for rel in rels {
        sb.write(rel, "one\ntwo\n");
    }
    let mut t = Term::spawn(&sb, &["src/b.rs"], 188, 57);
    t.wait_text("two");
    t.command("review");
    let s = t.wait_text("0 of 4 viewed");
    let click = |t: &mut Term, s: &Screen, name: &str, nth: usize| {
        let (y, row) =
            s.rows.iter().enumerate().filter(|(_, r)| r.contains(&format!("◦ {name}"))).nth(nth).unwrap();
        let x = row[..row.find(&format!("◦ {name}")).unwrap()].chars().count() + 2;
        t.send(&format!("\x1b[<0;{};{}M\x1b[<0;{};{}m", x + 1, y + 1, x + 1, y + 1));
    };
    click(&mut t, &s, "pack.toml", 0);
    t.wait_for("the first pack.toml opened", |s| {
        s.rows[0].contains("c-sharp") && s.rows[0].contains("pack.toml")
    });
    // The chat open and focused beside it: the same
    t.send(" l");
    let s = t.wait_text("claude");
    click(&mut t, &s, "a.rs", 0);
    t.wait_for("a.rs opened", |s| s.rows[0].contains("a.rs"));
}
