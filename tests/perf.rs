//! Real-use performance: the built tarae on a pseudo-terminal, as someone uses it — startup to the first frame,
//! a key in a big file to its frame, scrolling, idle CPU, memory with this repository open. `cargo perf` (xtask
//! perf) runs these in the `perf` profile (release, with line tables) and keeps their numbers: each prints
//! `PERF <name> <value> budget=<value>`, past libtest's capture. At the test profile's opt-level 0 (`cargo
//! test`) they skip themselves, saying so — they'd measure a debug build.
#![cfg(unix)]
#![allow(clippy::disallowed_methods, reason = "tests: not in the release binary (clippy.toml)")]

mod harness;

use std::fs;
use std::time::{Duration, Instant};

use harness::*;

/// What a test that skipped itself writes (as `test_results::SELF_SKIP` in the editor reads it).
const SELF_SKIP: &str = "tarae-test: skipped ";
/// Runs of a scenario that's quick enough to repeat — its median is what's kept.
const REPEAT: usize = 5;

fn unoptimized(test: &str) -> bool {
    if cfg!(tarae_optimized) {
        return false;
    }
    let line = format!("{SELF_SKIP}{test} — built at opt-level 0 (the test profile); `cargo perf` runs it\n");
    let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    true
}

/// `PERF <name> <value> budget=<value> noise=<value>` for xtask perf, and the budget held. `noise`: what the
/// number moves between quiet runs, in its unit — a change under it is never called slower.
fn report(name: &str, value: f64, budget: f64, noise: f64) {
    let line = format!("PERF {name} {value:.3} budget={budget:.0} noise={noise}\n");
    let _ = std::io::Write::write_all(&mut std::io::stderr(), line.as_bytes());
    assert!(value < budget, "{name}: {value:.3} (budget {budget})");
}

/// What's live in `pid`'s heap — `heap`'s count by size (tree-sitter's subtrees are its 96- and 112-byte
/// blocks; a freed page malloc keeps is RSS, not here). With `TARAE_VMMAP`, the regions behind the footprint
/// too (malloc's, the binary's, stacks).
fn heap(pid: u32) -> String {
    let out = std::process::Command::new("/usr/bin/heap")
        .arg(pid.to_string())
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default();
    let mut s = out.lines().filter(|l| l.contains("nodes malloced")).collect::<Vec<_>>().join("\n");
    if std::env::var_os("TARAE_VMMAP").is_some() {
        let v = std::process::Command::new("/usr/bin/vmmap")
            .args(["--summary", &pid.to_string()])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
            .unwrap_or_default();
        s += &v[v.find("REGION TYPE").unwrap_or(0)..];
    }
    s
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// A Rust file of `lines` lines in the sandbox.
fn big_file(sb: &Sandbox, name: &str, lines: usize) {
    let text: String = (0..lines)
        .map(|i| format!("fn f{i}(x: u32) -> u32 {{ let y = x + {i}; y * 2 }} // 타래\n"))
        .collect();
    sb.write(name, &text);
}

/// From starting the process to its first frame drawn, with a file to open.
#[test]
fn perf_e2e_startup_to_first_frame() {
    if unoptimized("perf_e2e_startup_to_first_frame") {
        return;
    }
    let sb = Sandbox::new("perf-start");
    big_file(&sb, "a.rs", 2_000);
    let mut runs = Vec::new();
    for _ in 0..REPEAT {
        let t0 = Instant::now();
        let t = sb.spawn(&["a.rs"]);
        runs.push(ms(t.first_frame_at().expect("a frame").saturating_duration_since(t0)));
    }
    report("e2e_startup_to_first_frame_ms", median(runs), 500.0, 0.5);
}

/// What the descriptor-limit shim (`tarae --nofile N <path> <argv0> args…`, nofile.rs) adds to a child's start: `true`
/// started and waited for straight, and through the built tarae as the shim — alternating, 200 of each, medians;
/// the difference is the number. (Inside macOS's sandbox each exec costs more: measure outside it.)
#[test]
fn perf_e2e_nofile_shim_start() {
    if unoptimized("perf_e2e_nofile_shim_start") {
        return;
    }
    let program =
        ["/bin/true", "/usr/bin/true"].into_iter().find(|p| std::path::Path::new(p).exists()).unwrap();
    let time = |cmd: &mut std::process::Command| {
        let t0 = Instant::now();
        assert!(cmd.status().unwrap().success());
        ms(t0.elapsed())
    };
    let (mut straight, mut shimmed) = (Vec::new(), Vec::new());
    for _ in 0..200 {
        straight.push(time(&mut std::process::Command::new(program)));
        shimmed.push(time(std::process::Command::new(BIN).args(["--nofile", "256", program, "true"])));
    }
    let (s, w) = (median(straight), median(shimmed));
    eprintln!("nofile shim: {program} straight {s:.3} ms, through the shim {w:.3} ms");
    report("e2e_nofile_shim_added_ms", w - s, 10.0, 0.3);
}

/// A key in a 200k-line file to its frame on the terminal (the median of many moves and edits).
#[test]
fn perf_e2e_key_in_a_big_file() {
    if unoptimized("perf_e2e_key_in_a_big_file") {
        return;
    }
    let sb = Sandbox::new("perf-key");
    big_file(&sb, "big.rs", 200_000);
    let mut t = sb.spawn(&["big.rs"]);
    t.wait_text("fn f1(");
    std::thread::sleep(Duration::from_millis(600)); // (the whole-screen redraw after the first frames)
    let mut runs = Vec::new();
    for k in ["j", "j", "w", "w", "e", "b", "k", "l", "h", "j"].iter().cycle().take(40) {
        runs.extend(t.key_to_frame(k, Duration::from_secs(2)).map(ms));
    }
    assert!(runs.len() >= 30, "frames came for {} of 40 keys", runs.len());
    report("e2e_key_to_frame_big_file_ms", median(runs), 16.0, 0.05);
}

/// The UI state over its socket (`tarae state`): a request to its answer, the frame it waits for included —
/// straight over the socket, and as the `tarae state` command (its process start too). Medians of 30.
#[test]
fn perf_e2e_ui_state_round_trip() {
    use std::io::{BufRead, BufReader, Write};
    if unoptimized("perf_e2e_ui_state_round_trip") {
        return;
    }
    // (a short state folder: the test connects by the socket's path, 104 bytes at most)
    let short = std::env::temp_dir().join(format!("tps{}", std::process::id()));
    fs::create_dir_all(&short).unwrap();
    let sb = Sandbox::new("perf-ui").with_env("XDG_STATE_HOME", &short.to_string_lossy());
    big_file(&sb, "a.rs", 2_000);
    let mut t = sb.spawn(&["a.rs"]);
    t.wait_text("fn f1(");
    std::thread::sleep(Duration::from_millis(600));
    let sock = short.join(format!("tarae/ui/{}.sock", t.pid()));
    let mut s = std::os::unix::net::UnixStream::connect(&sock).expect("the socket");
    let mut lines = BufReader::new(s.try_clone().unwrap()).lines();
    let mut socket = Vec::new();
    let mut bytes = 0;
    for _ in 0..30 {
        let t0 = Instant::now();
        writeln!(s, r#"{{"get":"state"}}"#).unwrap();
        let line = lines.next().unwrap().unwrap();
        socket.push(ms(t0.elapsed()));
        bytes = line.len();
    }
    let cli: Vec<f64> = (0..30)
        .map(|_| {
            let t0 = Instant::now();
            t.state(&["--json"]);
            ms(t0.elapsed())
        })
        .collect();
    drop(t);
    fs::remove_dir_all(&short).ok();
    report("e2e_ui_state_socket_round_trip_ms", median(socket), 50.0, 0.1);
    report("e2e_ui_state_cli_round_trip_ms", median(cli), 200.0, 1.0);
    report("e2e_ui_state_json_bytes_plain_scene", bytes as f64, 16384.0, 1.0);
}

/// A key in a 200k-line file to its frame, with a watcher of the UI state connected (`tarae state --watch`):
/// the summary compared after each frame, the events written — next to `e2e_key_to_frame_big_file_ms`.
#[test]
fn perf_e2e_key_in_a_big_file_watched() {
    if unoptimized("perf_e2e_key_in_a_big_file_watched") {
        return;
    }
    let sb = Sandbox::new("perf-key-watched");
    big_file(&sb, "big.rs", 200_000);
    let mut t = sb.spawn(&["big.rs"]);
    t.wait_text("fn f1(");
    std::thread::sleep(Duration::from_millis(600));
    let w = t.watch("");
    let mut runs = Vec::new();
    for k in ["j", "j", "w", "w", "e", "b", "k", "l", "h", "j"].iter().cycle().take(40) {
        runs.extend(t.key_to_frame(k, Duration::from_secs(2)).map(ms));
    }
    assert!(runs.len() >= 30, "frames came for {} of 40 keys", runs.len());
    // (moving in the code changes nothing a watcher is told of)
    eprintln!("watched: {} events for 40 keys", w.events().len() - 1);
    report("e2e_key_to_frame_big_file_watched_ms", median(runs), 16.0, 0.05);
}

/// What a watcher is told while Claude streams an answer in front (a word every 50 ms, the chat open): the
/// summary leaves the chat's words out — events a second, over 3 s of it.
#[test]
fn perf_e2e_events_while_claude_streams() {
    if unoptimized("perf_e2e_events_while_claude_streams") {
        return;
    }
    let sb = Sandbox::new("perf-stream-events");
    big_file(&sb, "a.rs", 2_000);
    let word = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"word "}}}"#;
    let fake = format!(
        "while read -r line; do\n  case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac\n  i=0\n  \
         while [ $i -lt 400 ]; do printf '%s\\n' '{word}'; sleep 0.05; i=$((i+1)); done\n  \
         printf '%s\\n' '{{\"type\":\"result\",\"result\":\"done\"}}'\ndone\n"
    );
    let script = sb.home().join("fake-claude.sh");
    fs::write(&script, fake).unwrap();
    sb.write_config(&format!("{BASE_CONFIG}[llm]\ncommand = \"sh\"\nargs = [\"{}\"]\n", script.display()));
    let mut t = Term::spawn(&sb, &["a.rs"], 160, 50);
    t.wait_text("fn f1(");
    t.send(" l");
    t.send("go\r");
    t.wait_text("word word");
    let w = t.watch("");
    let (frames, secs) = (t.frames(), 3.0);
    std::thread::sleep(Duration::from_secs_f64(secs));
    let events = w.events().len() - 1;
    let kinds: Vec<String> =
        w.events().iter().skip(1).filter_map(|e| e["event"].as_str().map(String::from)).collect();
    eprintln!("while streaming: {events} events in {secs} s over {} frames: {kinds:?}", t.frames() - frames);
    report("e2e_events_while_streaming_per_s", events as f64 / secs, 5.0, 0.5);
}

/// Scrolling a 200k-line file by half pages to each frame.
#[test]
fn perf_e2e_scrolling() {
    if unoptimized("perf_e2e_scrolling") {
        return;
    }
    let sb = Sandbox::new("perf-scroll");
    big_file(&sb, "big.rs", 200_000);
    let mut t = sb.spawn(&["big.rs"]);
    t.wait_text("fn f1(");
    std::thread::sleep(Duration::from_millis(600));
    let mut runs = Vec::new();
    for i in 0..40 {
        let key = if i < 30 { "\x04" } else { "\x15" }; // C-d down, then C-u back
        runs.extend(t.key_to_frame(key, Duration::from_secs(2)).map(ms));
    }
    assert!(runs.len() >= 30, "frames came for {} of 40 scrolls", runs.len());
    report("e2e_scroll_to_frame_ms", median(runs), 16.0, 0.05);
}

/// Scrolling on a terminal that reads slowly — 4 KiB every 10 ms, ≈ 400 KB/s (a remote terminal, a busy
/// multiplexer): 200 half-pages of a 200k-line file typed 2 ms apart, without waiting for their frames, on a
/// 200×60 screen. tarae writes faster than that is read, so the pty fills and its writes wait for room — the
/// number is how long until the last frame has been read; `cargo xtask trace pty` shows the writes' waits.
#[test]
fn perf_e2e_scrolling_slow_terminal() {
    if unoptimized("perf_e2e_scrolling_slow_terminal") {
        return;
    }
    let sb = Sandbox::new("perf-slow-terminal");
    big_file(&sb, "big.rs", 200_000);
    let pace = Pace { chunk: 4096, pause: Duration::from_millis(10) };
    let mut t = Term::spawn_paced(&sb, &["big.rs"], 200, 60, pace);
    t.wait_text("fn f1(");
    std::thread::sleep(Duration::from_millis(600));
    let (frames, written) = (t.frames(), t.written());
    let t0 = Instant::now();
    for _ in 0..200 {
        t.send("\x04"); // C-d
        std::thread::sleep(Duration::from_millis(2));
    }
    // Done when nothing more comes for a while: every frame written has been read
    let mut last = (t.written(), Instant::now());
    while last.1.elapsed() < Duration::from_millis(500) {
        assert!(t0.elapsed() < Duration::from_secs(60), "still writing after 60 s");
        std::thread::sleep(Duration::from_millis(20));
        if t.written() != last.0 {
            last = (t.written(), Instant::now());
        }
    }
    let took = t.last_frame_at().map_or(0.0, |at| ms(at.saturating_duration_since(t0)));
    eprintln!(
        "slow terminal: {} frames, {:.0} KB in {took:.0} ms",
        t.frames() - frames,
        (t.written() - written) as f64 / 1024.0
    );
    report("e2e_scrolling_slow_terminal_ms", took, 30_000.0, 250.0);
}

/// CPU an idle tarae uses, a file open in a git repository and settled.
#[test]
fn perf_e2e_idle_cpu() {
    if unoptimized("perf_e2e_idle_cpu") {
        return;
    }
    let sb = Sandbox::new("perf-idle");
    fs::write(sb.git_config(), "[user]\n\tname = t\n\temail = t@t\n[commit]\n\tgpgsign = false\n").unwrap();
    big_file(&sb, "a.rs", 2_000);
    sb.git(&["init", "-q", "-b", "main"]);
    sb.git(&["add", "-A"]);
    sb.git(&["commit", "-q", "-m", "base"]);
    let mut t = sb.spawn(&["a.rs"]);
    t.wait_text("fn f1(");
    std::thread::sleep(Duration::from_secs(1));
    let before = t.usage();
    // The meter sees work: starting up and drawing the file took CPU — so a 0 below means idle, not blind
    assert!(before.cpu > Duration::ZERO, "no CPU read for starting up: {before:?}");
    std::thread::sleep(Duration::from_secs(3));
    let used = t.usage() - before;
    report("e2e_idle_cpu_ms_per_s", ms(used.cpu) / 3.0, 10.0, 0.5);
    // …and with a watcher of the UI state connected: nothing changes, nothing is told, nothing wakes
    let w = t.watch("");
    std::thread::sleep(Duration::from_secs(1));
    let before = t.usage();
    std::thread::sleep(Duration::from_secs(3));
    let used = t.usage() - before;
    assert_eq!(w.events().len(), 1, "told nothing while idle: {:?}", w.events());
    report("e2e_idle_cpu_watched_ms_per_s", ms(used.cpu) / 3.0, 10.0, 0.5);
}

/// What `:procs` costs — its looks at tarae's processes (here tarae, the shell in its terminal): one look,
/// from tarae's CPU looking every 50 ms (`TARAE_PROCS_EVERY` standing in for its 30 s) against the same
/// tarae at its own pace; what that makes a second while `:procs` is closed; and while it's open (a look and
/// a frame a second).
#[test]
fn perf_e2e_procs_sampling() {
    if unoptimized("perf_e2e_procs_sampling") {
        return;
    }
    let secs = 3.0;
    let idle = |sb: Sandbox, open: bool| {
        sb.write("a.txt", "one\n");
        let mut t = sb.spawn(&["a.txt"]);
        t.wait_text("one");
        t.send(" T");
        t.wait_for("the terminal", |s| s.contains("● terminal") && (s.contains("$ ") || s.contains("# ")));
        t.send("\x1c");
        t.wait_for("the code has the keys", |s| s.contains("C-\\ type in it"));
        if open {
            t.send(":procs\r");
            t.wait_text("SELF-CHECKS");
        }
        std::thread::sleep(Duration::from_secs(1));
        let (before, frames) = (t.usage(), t.frames());
        std::thread::sleep(Duration::from_secs_f64(secs));
        let used = t.usage() - before;
        // Closed, a look draws nothing (measured 2026-10-03: 0 frames, ~2.5 thread wakes a look)
        if !open {
            assert_eq!(t.frames() - frames, 0, "a look drew a frame while :procs was closed");
        }
        ms(used.cpu) / secs
    };
    let every_ms = 50.0;
    let fast = idle(Sandbox::new("perf-procs-fast").with_env("TARAE_PROCS_EVERY", "50"), false);
    let own = idle(Sandbox::new("perf-procs-own"), false);
    let open = idle(Sandbox::new("perf-procs-open"), true);
    // (ms a second over 1000/every looks a second: µs a look — a cold wake of two threads, not the ~40 µs a
    // look takes warm)
    let look_us = (fast - own).max(0.0) * every_ms;
    let closed_us = look_us / 30.0;
    eprintln!("procs: {fast:.3} ms/s looking every {every_ms} ms, {own:.3} at its own pace, {open:.3} open");
    report("e2e_procs_look_us", look_us, 2000.0, 40.0);
    report("e2e_procs_closed_cpu_us_per_s", closed_us, 100.0, 2.0);
    report("e2e_procs_open_cpu_ms_per_s", open, 10.0, 2.0);
}

/// A chat turn's wait (N6): nothing on screen moves but the waiting animation (on the follow card, its edges
/// seated each frame) — tarae's CPU, its frames a second and the bytes a frame writes, over 4 s of it.
#[test]
fn perf_e2e_waiting_for_claude() {
    if unoptimized("perf_e2e_waiting_for_claude") {
        return;
    }
    let sb = Sandbox::new("perf-waiting");
    big_file(&sb, "a.rs", 2_000);
    let t = sb.waiting_turn(&["a.rs"], 160, 50, 30);
    // (past the first frames — the chat opening, its first whole redraw)
    std::thread::sleep(Duration::from_secs(1));
    let (before, frames, bytes) = (t.usage(), t.frames(), t.raw().len());
    let secs = 4.0;
    std::thread::sleep(Duration::from_secs_f64(secs));
    let used = t.usage() - before;
    let (frames, bytes) = ((t.frames() - frames) as f64, (t.raw().len() - bytes) as f64);
    report("e2e_waiting_cpu_ms_per_s", ms(used.cpu) / secs, 60.0, 2.0);
    report("e2e_waiting_frames_per_s", frames / secs, 12.0, 0.5);
    report("e2e_waiting_bytes_per_frame", bytes / frames.max(1.0), 400.0, 20.0);
}

/// Two sessions (`:session-open` on a worktree), Claude streaming a word every 50 ms in the one behind: tarae's
/// CPU and its frames a second in front, which shows nothing of it — the session's results are applied
/// (swapped in and out) but draw nothing.
#[test]
fn perf_e2e_busy_session_behind() {
    if unoptimized("perf_e2e_busy_session_behind") {
        return;
    }
    let sb = Sandbox::new("perf-sessions");
    fs::write(sb.git_config(), "[user]\n\tname = t\n\temail = t@t\n[commit]\n\tgpgsign = false\n").unwrap();
    big_file(&sb, "a.rs", 2_000);
    sb.git(&["init", "-q", "-b", "main"]);
    sb.git(&["add", "-A"]);
    sb.git(&["commit", "-q", "-m", "base"]);
    let wt = sb.work().with_file_name("wt");
    sb.git(&["worktree", "add", "-q", "-b", "fix", wt.to_str().unwrap()]);
    let word = r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"word "}}}"#;
    let fake = format!(
        "while read -r line; do\n  case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac\n  i=0\n  \
         while [ $i -lt 400 ]; do printf '%s\\n' '{word}'; sleep 0.05; i=$((i+1)); done\n  \
         printf '%s\\n' '{{\"type\":\"result\",\"result\":\"done\"}}'\ndone\n"
    );
    let script = sb.home().join("fake-claude.sh");
    fs::write(&script, fake).unwrap();
    sb.write_config(&format!("{BASE_CONFIG}[llm]\ncommand = \"sh\"\nargs = [\"{}\"]\n", script.display()));
    let mut t = Term::spawn(&sb, &["a.rs"], 160, 50);
    t.wait_text("fn f1(");
    t.send(&format!(":session-open {}\r", wt.display()));
    // (in front, the new session shows its blank scratch — whatever the toast says)
    t.wait_for("the new session in front", |s| !s.contains("fn f1("));
    t.send(" l");
    t.send("go\r");
    t.wait_text("word word");
    // (Esc alone — with a key right behind it, it reads as alt+that key: on a loaded machine the `:` went out
    // right behind it, both came in one read, alt+`:`, the switch typed into the chat, a timeout —
    // :preflight 2026-10-04. A screen grep for the follow card's words passed too early once. Now: the Esc
    // handled and drawn — `after_input` — and the state saying the code has the keys)
    t.esc_state("the chat let go of the keys", |s| s["keys"]["region"] == "code");
    t.send(":session-switch 0\r");
    t.wait_for("the first session in front", |s| !s.contains("word word") && s.contains("fn f1("));
    // (past the toasts' fade and the switch's frames)
    std::thread::sleep(Duration::from_secs(4));
    let (before, frames) = (t.usage(), t.frames());
    let secs = 3.0;
    std::thread::sleep(Duration::from_secs_f64(secs));
    let used = t.usage() - before;
    let frames = (t.frames() - frames) as f64;
    report("e2e_busy_session_behind_cpu_ms_per_s", ms(used.cpu) / secs, 10.0, 0.5);
    report("e2e_busy_session_behind_frames_per_s", frames / secs, 1.0, 0.2);
}

/// Memory with this repository open: its files (a copy, in a git repository of the test's own) listed in
/// the file picker, the biggest source file open.
#[test]
fn perf_e2e_memory_in_this_repo() {
    if unoptimized("perf_e2e_memory_in_this_repo") {
        return;
    }
    let sb = Sandbox::new("perf-mem");
    copy_repo(&sb);
    fs::write(sb.git_config(), "[user]\n\tname = t\n\temail = t@t\n[commit]\n\tgpgsign = false\n").unwrap();
    sb.git(&["init", "-q", "-b", "main"]);
    sb.git(&["add", "-A"]);
    sb.git(&["commit", "-q", "-m", "base"]);
    let mut t = Term::spawn(&sb, &["src/term.rs"], 160, 50);
    t.wait_text("use std::io");
    t.send(" f");
    t.wait_for("the file picker", |s| s.contains("files in"));
    t.esc("the picker closed", |s| !s.contains("files in"));
    std::thread::sleep(Duration::from_secs(1));
    report("e2e_rss_in_this_repo_mb", t.usage().rss as f64 / (1024.0 * 1024.0), 400.0, 2.0);
}

/// The live search's count on its input row (`› <typed> … search  <n>`): how many it found, and whether a
/// search is still under way (`…` after it). None while the row doesn't show `typed` yet.
fn search_count(s: &Screen, typed: &str) -> Option<(usize, bool)> {
    let row = s.rows.iter().find(|r| r.contains(&format!("› {typed} ")))?;
    // (the preview's title is further along the same screen row — the count is right after `search`)
    let after = row.split_once(" search ")?.1.trim_start();
    let digits: String = after.chars().take_while(char::is_ascii_digit).collect();
    let n = digits.parse().ok()?;
    Some((n, after[digits.len()..].starts_with('…')))
}

/// Polls the screen every 2 ms (the harness's 20 ms would be most of what's measured) until `done`.
fn until(t: &mut Term, within: Duration, done: impl Fn(&Screen) -> bool) -> Option<Duration> {
    let t0 = Instant::now();
    while t0.elapsed() < within {
        if done(&t.screen()) {
            return Some(t0.elapsed());
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    None
}

/// The live search (`space /`, N4) in a copy of this repository: each key of a pattern, typed one by one,
/// to its results on screen (the search done — the count without `…`). Includes the pause it waits for
/// typing to stop (`LIVE_PAUSE`, 50 ms).
#[test]
fn perf_e2e_live_search_in_this_repo() {
    if unoptimized("perf_e2e_live_search_in_this_repo") {
        return;
    }
    let sb = Sandbox::new("perf-search");
    copy_repo(&sb);
    let mut t = Term::spawn(&sb, &["src/term.rs"], 160, 50);
    t.wait_text("use std::io");
    t.send(" /");
    t.wait_for("the search", |s| s.contains("type to search"));
    let mut runs = Vec::new();
    let mut typed = String::new();
    for k in "fn draw_".chars() {
        typed.push(k);
        let q = typed.clone();
        t.send(&k.to_string());
        let took = until(&mut t, Duration::from_secs(5), |s| search_count(s, &q).is_some_and(|c| !c.1));
        runs.push(ms(took.unwrap_or_else(|| panic!("no results for {q:?}\n{}", t.screen()))));
    }
    report("e2e_live_search_repo_ms", median(runs), 250.0, 5.0);
}

/// A tree of 200k small files, made once and kept (in the temp folder — the next run reuses it; it's
/// remade when gone). Every 1000th file has `needle_<n>` in it.
fn tree_200k() -> std::path::PathBuf {
    let root = std::path::Path::new("/tmp/tarae-perf-200k").to_path_buf();
    if root.join(".complete").exists() {
        return root;
    }
    let _ = fs::remove_dir_all(&root);
    std::thread::scope(|s| {
        for a in 0..200 {
            let root = &root;
            s.spawn(move || {
                for b in 0..10 {
                    let dir = root.join(format!("d{a}/e{b}"));
                    fs::create_dir_all(&dir).unwrap();
                    for c in 0..100 {
                        let n = (a * 10 + b) * 100 + c;
                        let rare = if n % 1000 == 0 {
                            format!("let needle_{} = 1;\n", n / 1000)
                        } else {
                            String::new()
                        };
                        let text = format!("// file {n}\nfn f{c}(x: u32) -> u32 {{ x + {n} }}\n{rare}");
                        fs::write(dir.join(format!("f{c}.rs")), text).unwrap();
                    }
                }
            });
        }
    });
    fs::write(root.join(".complete"), "").unwrap();
    root
}

/// The live search in a 200k-file tree: a key to the first results on screen, and to the search done —
/// medians over a few patterns (`needle_1`, `needle_2` … each a search of every file).
#[test]
fn perf_e2e_live_search_in_200k_files() {
    if unoptimized("perf_e2e_live_search_in_200k_files") {
        return;
    }
    let tree = tree_200k();
    let sb = Sandbox::new("perf-search-200k");
    let mut t = Term::spawn_in(&sb, &tree, &[], 160, 50);
    // (the start screen — a folder with no file named)
    t.wait_for("the start screen", |s| s.rows.iter().any(|r| !r.trim().is_empty()));
    t.send(" /");
    t.wait_for("the search", |s| s.contains("type to search"));
    t.send("needle_");
    t.wait_for("the first search", |s| search_count(s, "needle_").is_some_and(|c| !c.1));
    let (mut first, mut done) = (Vec::new(), Vec::new());
    for d in ["1", "2", "3", "4", "5"] {
        let q = format!("needle_{d}");
        // (the last search's 200 stay up until this one's first results take their place)
        let before = search_count(&t.screen(), "needle_").map_or(0, |c| c.0);
        let t0 = Instant::now();
        t.send(d);
        let shown = until(&mut t, Duration::from_secs(20), |s| {
            search_count(s, &q).is_some_and(|c| c.0 > 0 && c.0 != before)
        });
        first.push(ms(shown.unwrap_or_else(|| panic!("no results for {q:?}\n{}", t.screen()))));
        until(&mut t, Duration::from_secs(20), |s| search_count(s, &q).is_some_and(|c| !c.1))
            .unwrap_or_else(|| panic!("the search for {q:?} never ended\n{}", t.screen()));
        done.push(ms(t0.elapsed()));
        t.send("\x7f");
        t.wait_for("back to needle_", |s| search_count(s, "needle_").is_some_and(|c| !c.1));
    }
    report("e2e_live_search_200k_first_ms", median(first), 1000.0, 20.0);
    report("e2e_live_search_200k_done_ms", median(done), 4000.0, 150.0);
}

/// Memory over long editing (#36): a copy of this repository, its two biggest files open, rounds of edits
/// (each reparsed — every doc comment is a layer of its own), files opened and closed, a code review
/// started and ended, a chat answered with code. RSS at the end, and how much it grew past the first round
/// — what a leak moves (an hour's work went 214 → 556 MB: every undo step kept a whole set of layers).
#[test]
fn perf_e2e_memory_over_long_editing() {
    if unoptimized("perf_e2e_memory_over_long_editing") {
        return;
    }
    let sb = Sandbox::new("perf-long");
    copy_repo(&sb);
    fs::write(sb.git_config(), "[user]\n\tname = t\n\temail = t@t\n[commit]\n\tgpgsign = false\n").unwrap();
    sb.git(&["init", "-q", "-b", "main"]);
    sb.git(&["add", "-A"]);
    sb.git(&["commit", "-q", "-m", "base"]);
    // A stand-in claude: each message answered with some prose and code, ending `fin<n>`
    let block = "Here is how it fits together.\\n\\n```rust\\nfn walk(tree: &Tree) -> usize {\\n    let mut n = 0;\\n    for c in tree.children() { n += walk(c); }\\n    n + 1\\n}\\n```\\n\\nThe rest follows.\\n\\n";
    let fake = format!(
        "n=0\nwhile read -r line; do\n  case \"$line\" in '{{\"message\"'*) ;; *) continue;; esac\n  n=$((n+1))\n  \
         for i in 1 2 3 4 5 6; do printf '%s\\n' '{{\"type\":\"stream_event\",\"event\":{{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"{block}\"}}}}}}'; done\n  \
         printf '{{\"type\":\"stream_event\",\"event\":{{\"type\":\"content_block_delta\",\"delta\":{{\"type\":\"text_delta\",\"text\":\"fin%s\"}}}}}}\\n' \"$n\"\n  \
         printf '%s\\n' '{{\"type\":\"result\",\"result\":\"done\"}}'\ndone\n"
    );
    let script = sb.write("fake-claude.sh", &fake);
    sb.write_config(&format!("{BASE_CONFIG}[llm]\ncommand = \"sh\"\nargs = [\"{}\"]\n", script.display()));
    let mut t = Term::spawn(&sb, &["src/term.rs", "src/editor.rs"], 160, 50);
    t.wait_text("use std::io");
    let mut asked = 0;
    let mut round = |t: &mut Term, r: usize| {
        for file in 0..2 {
            for k in 0..40 {
                // An edit somewhere in the file — its own undo step — and time for its reparse
                t.send(&format!("{}gg", 50 + (k * 97 + r * 13) % 4000));
                t.send("o// x");
                // (Esc alone: one with keys right after it reads as Alt + key)
                t.send("\x1b");
                std::thread::sleep(Duration::from_millis(60));
            }
            // The other big file, a small one opened and closed
            t.send(if file == 0 { "ga" } else { ":open src/picker.rs\r" });
            std::thread::sleep(Duration::from_millis(300));
            if file == 1 {
                t.send(":bc\r");
            }
        }
        // (TARAE_LONG_SKIP=review,chat leaves those parts out — to see what a number comes from)
        let skip = std::env::var("TARAE_LONG_SKIP").unwrap_or_default();
        // A code review of what changed, started and ended
        if !skip.contains("review") {
            t.send(" vv");
            std::thread::sleep(Duration::from_millis(800));
            t.send(" vq");
            std::thread::sleep(Duration::from_millis(300));
        }
        if skip.contains("chat") {
            return;
        }
        // A few turns of the chat
        t.send(" l");
        for _ in 0..3 {
            asked += 1;
            t.send(&format!("tell me more {asked}\r"));
            let fin = format!("fin{asked}");
            t.wait_for("the answer", |s| s.contains(&fin));
        }
        t.send("\x1b");
        std::thread::sleep(Duration::from_millis(300));
    };
    // Two rounds to warm up: the kept undo steps fill, and malloc's high-water mark settles (each parse
    // makes a fresh layer set and frees the last — RSS keeps the pages: +34 MB in the second round, with the
    // live heap flat from then on, measured with `heap`)
    round(&mut t, 0);
    round(&mut t, 1);
    std::thread::sleep(Duration::from_secs(1));
    let first = t.usage().rss;
    let rounds: usize = std::env::var("TARAE_LONG_ROUNDS").ok().and_then(|n| n.parse().ok()).unwrap_or(4);
    for r in 2..2 + rounds {
        round(&mut t, r);
        // (TARAE_LONG_HEAP: what's live in the heap, beside the RSS — a freed page malloc keeps is RSS)
        let heap = if std::env::var_os("TARAE_LONG_HEAP").is_some() { heap(t.pid()) } else { String::new() };
        eprintln!("round {r}: {:.1} MB  {heap}", t.usage().rss as f64 / (1024.0 * 1024.0));
    }
    std::thread::sleep(Duration::from_secs(1));
    let last = t.usage().rss;
    let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
    eprintln!("long editing: {:.1} MB warmed up, {:.1} MB {rounds} rounds later", mb(first), mb(last));
    // (86 MB since undo keeps no trees — 138 MB when it kept 100, 1.4 GB when it kept them all)
    report("e2e_rss_long_editing_mb", mb(last), 150.0, 5.0);
    // (before #36's fix this grew ~240 MB a round; bounded it's under a MB — the undo texts, the chat)
    report("e2e_rss_long_editing_growth_mb", mb(last.saturating_sub(first)), 20.0, 3.0);
}

/// Memory at the start of a working day (#36): a copy of this repository with the files a day of
/// tarae's development keeps open (its biggest sources among them), each looked at once, then left a while
/// — only the five seen last keep their parse trees (footprint, as Activity Monitor shows it). Next to it,
/// no file open. With `TARAE_HEAP` set, what's live at each step (`heap`'s count by size: tree-sitter's
/// subtrees are its 96- and 112-byte blocks).
#[test]
fn perf_e2e_memory_with_a_working_set_open() {
    if unoptimized("perf_e2e_memory_with_a_working_set_open") {
        return;
    }
    // (files unseen for 2 s let their trees go, not 3 minutes — the most recent five keep theirs)
    let sb = Sandbox::new("perf-working-set").with_env("TARAE_TREES_UNSEEN", "2");
    // (TARAE_MSL: malloc's stacks kept, and at the end what allocated what's live — `malloc_history -callTree`
    // into target/tarae-msl.txt)
    let msl = std::env::var_os("TARAE_MSL").is_some();
    let sb = if msl { sb.with_env("MallocStackLogging", "lite") } else { sb };
    copy_repo(&sb);
    let files = [
        "src/editor.rs",
        "src/term.rs",
        "src/lsp_editor.rs",
        "src/picker.rs",
        "src/tools.rs",
        "src/lsp.rs",
        "src/readonly.rs",
        "src/packs.rs",
        "tests/e2e.rs",
        "tests/harness/mod.rs",
        "Cargo.toml",
        "CONTRIBUTING.md",
        "README.md",
    ];
    let heap =
        |t: &Term| if std::env::var_os("TARAE_HEAP").is_some() { heap(t.pid()) } else { String::new() };
    let settled = |t: &mut Term| {
        // (every file read and parsed — the frames stop)
        let mut last = t.frames();
        loop {
            std::thread::sleep(Duration::from_millis(1500));
            if t.frames() == last {
                break;
            }
            last = t.frames();
        }
    };
    let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
    let mut t = Term::spawn(&sb, &[], 160, 50);
    t.wait_for("the start screen", |s| s.rows.iter().any(|r| !r.trim().is_empty()));
    settled(&mut t);
    let empty = t.usage().rss;
    eprintln!("no file open: {:.1} MB\n{}", mb(empty), heap(&t));
    drop(t);
    let mut t = Term::spawn(&sb, &files, 160, 50);
    t.wait_text("● normal");
    settled(&mut t);
    let say = |what: &str, u: Usage, t: &Term| {
        eprintln!("{what}: RSS {:.1} MB, footprint {:.1} MB\n{}", mb(u.rss), mb(u.footprint), heap(t));
    };
    // Opened: only the file on screen is parsed — the rest when they're first shown
    let opened = t.usage();
    say(&format!("{} files opened", files.len()), opened, &t);
    // Each looked at once (`gn` round the buffers, each parsed as it's shown)…
    for _ in 0..files.len() {
        t.send("gn");
        std::thread::sleep(Duration::from_millis(400));
    }
    settled(&mut t);
    let seen = t.usage();
    say("each looked at", seen, &t);
    // …then a while later (2 s here, 3 minutes for real), at the next key: the five seen last keep theirs
    std::thread::sleep(Duration::from_secs(3));
    t.send("\x1b");
    settled(&mut t);
    let later = t.usage();
    say("a while later", later, &t);
    if msl {
        let out = std::process::Command::new("/usr/bin/malloc_history")
            .args([&t.pid().to_string(), "-callTree", "-invert"])
            .output()
            .unwrap();
        let to = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/tarae-msl.txt");
        fs::write(&to, &out.stdout).unwrap();
    }
    // (measured 2026-10-03: 96.5 MB when every open file kept its trees — 815k live blocks; 30 MB with the
    // five seen last — 338k. Just opened, the footprint is mostly a parse's working memory not yet given
    // back: not a number to keep)
    report("e2e_footprint_working_set_mb", mb(later.footprint), 60.0, 5.0);
}

/// What a restored idle session costs (#218: after a `:restart` with 26 sessions the status line said "▲ tarae
/// 620 MB", 313 MB moments later). A copy of this repository; `n` sessions made (`:session-new`, its card
/// answered), four files opened in each, every session's checkout given kept turns as `space u` keeps them
/// (`TARAE_TURN_FILES` files of `TARAE_TURN_KB` KB each — 3 × 150 KB — over `KEEP` turns, in the format an
/// earlier tarae wrote; `TARAE_TURN_FILES=0`: none), then `:restart` — tarae's footprint before it, a second
/// after it came back, and settled (frames stopped, then 20 s more); then `:restart` once more, settled (what
/// the turns cost once they're kept as this tarae keeps them). The same with no session but the first is the
/// baseline: what's past it, per session, is what one costs. `TARAE_HEAP`: what's live in the heap at the end.
/// `TARAE_RESTORED_N` sets `n` (26, as it happened).
#[test]
fn perf_e2e_memory_restored_sessions() {
    if unoptimized("perf_e2e_memory_restored_sessions") {
        return;
    }
    let n: usize = std::env::var("TARAE_RESTORED_N").ok().and_then(|v| v.parse().ok()).unwrap_or(26);
    let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
    let base = restored_sessions(0);
    let got = restored_sessions(n);
    let each = (got.settled - base.settled) / n.max(1) as f64;
    let again_each = (got.again - base.again) / n.max(1) as f64;
    eprintln!(
        "restored sessions: {n} — live before :restart {:.1} MB; after it: first {:.1} MB, settled {:.1} MB, \
         restarted again {:.1} MB; with none: first {:.1} MB, settled {:.1} MB, again {:.1} MB — {each:.1} MB a \
         session ({again_each:.1} MB the second time)",
        mb(got.live),
        got.first,
        got.settled,
        got.again,
        base.first,
        base.settled,
        base.again
    );
    report("e2e_restored_session_each_mb", each, 40.0, 1.0);
    report("e2e_restored_sessions_first_mb", got.first, 1000.0, 20.0);
}

/// `restored_sessions`' footprints: before `:restart` (bytes), after it (MB) a second after it came back and
/// settled, and settled after a second `:restart`.
struct Restored {
    live: u64,
    first: f64,
    settled: f64,
    again: f64,
}

/// Turns kept for every checkout in tarae's state folder (`turns/`), as an earlier tarae wrote them — one file
/// per checkout, every turn's texts in it. The checkouts are those tarae keeps turns for already (a file each,
/// named by the checkout's hash: `turns/<hash>.json`, or `turns/<hash>/index.json`), written over with a `seq`
/// past any save of tarae's own.
fn seed_turns(sb: &Sandbox, checkouts: usize) {
    let env = |k: &str, d: usize| std::env::var(k).ok().and_then(|v| v.parse().ok()).unwrap_or(d);
    let (files, kb) = (env("TARAE_TURN_FILES", 3), env("TARAE_TURN_KB", 150));
    if files == 0 {
        return;
    }
    let dir = sb.state().join("turns");
    // (hash, root) of each kept checkout
    let kept = || -> Vec<(String, String)> {
        let Ok(rd) = fs::read_dir(&dir) else { return Vec::new() };
        let mut v: Vec<(String, String)> = rd
            .flatten()
            .filter_map(|e| {
                let p = e.path();
                let (hash, file) = match p.extension() {
                    Some(x) if x == "json" => (p.file_stem()?.to_string_lossy().into_owned(), p.clone()),
                    None if p.is_dir() => {
                        (p.file_name()?.to_string_lossy().into_owned(), p.join("index.json"))
                    }
                    _ => return None,
                };
                let v: serde_json::Value = serde_json::from_str(&fs::read_to_string(file).ok()?).ok()?;
                Some((hash, v["root"].as_str()?.to_string()))
            })
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let end = Instant::now() + Duration::from_secs(30);
    while kept().len() < checkouts {
        assert!(Instant::now() < end, "turns kept for {} checkouts of {checkouts}", kept().len());
        std::thread::sleep(Duration::from_millis(100));
    }
    for (hash, root) in kept() {
        let text = |seed: usize| {
            let line =
                format!("    let x{seed} = compute(a, b, c); // a line of a source file, as long as most\n");
            line.repeat(kb * 1024 / line.len())
        };
        let list: Vec<serde_json::Value> = (0..10)
            .map(|t| {
                let touched: Vec<serde_json::Value> = (0..files)
                    .map(|f| {
                        serde_json::json!({ "path": format!("{root}/src/f{f}.rs"), "before": text(t * 10 + f),
                                            "after": text(t * 10 + f + 1), "unseen": false, "gone": false })
                    })
                    .collect();
                serde_json::json!({ "question": format!("turn {t}"), "files": touched, "undone": false,
                                    "seen": true })
            })
            .collect();
        let body = serde_json::json!({ "root": root, "seq": u64::MAX / 2, "list": list, "turn": [],
                                       "question": "" });
        fs::write(dir.join(format!("{hash}.json")), body.to_string()).unwrap();
    }
}

/// `restored_sessions`' scene with `n` sessions besides the first.
fn restored_sessions(n: usize) -> Restored {
    let mb = |b: u64| b as f64 / (1024.0 * 1024.0);
    let sb = Sandbox::new(&format!("perf-restored-{n}"));
    copy_repo(&sb);
    fs::write(sb.git_config(), "[user]\n\tname = t\n\temail = t@t\n[commit]\n\tgpgsign = false\n").unwrap();
    sb.write(".tarae.toml", &format!("[editor]\nrestart-build = \"true\"\nrestart-binary = \"{BIN}\"\n"));
    sb.git(&["init", "-q", "-b", "main"]);
    sb.git(&["add", "-A"]);
    sb.git(&["commit", "-q", "-m", "base"]);
    let files = ["src/picker.rs", "src/lsp.rs", "src/readonly.rs", "README.md"];
    // (a card settled in front: of `kind`, or — `kind` empty — any but the project's trust card)
    let front_card = |t: &mut Term, what: &str, kind: &str| {
        t.wait_state(what, |s| {
            s["cards"].as_array().into_iter().flatten().any(|c| {
                let of = if kind.is_empty() { c["kind"] != "project-trust" } else { c["kind"] == kind };
                c["up"] == "front" && c["settled"] == true && of
            })
        })
    };
    let sessions = |s: &serde_json::Value| s["sessions"].as_array().map_or(0, Vec::len);
    let settled = |t: &mut Term| {
        let mut last = t.frames();
        loop {
            std::thread::sleep(Duration::from_millis(1500));
            if t.frames() == last {
                break;
            }
            last = t.frames();
        }
    };
    let mut t = Term::spawn(&sb, &files, 160, 50);
    // (the project's trust card — its `.tarae.toml` names the binary `:restart` runs: set aside, as
    // `yes_to_restart` in e2e does; the restart takes the binary all the same)
    front_card(&mut t, "the trust card", "project-trust");
    t.send("n");
    for i in 1..=n {
        t.command(&format!("session-new s{i}"));
        front_card(&mut t, "the new session's card", "");
        t.send("y");
        t.wait_state("the session made, in front", |s| sessions(s) == i + 1 && s["front"] == format!("s{i}"));
        // (a new session opens with its chat focused: the keys back to the code first)
        t.send("\x1b");
        t.wait_state("the keys back in the code", |s| s["keys"]["to"] != "chat");
        for f in files {
            t.command(&format!("o {f}"));
        }
        t.wait_text("README");
    }
    settled(&mut t);
    seed_turns(&sb, n + 1);
    let live = t.usage().footprint;
    // (the `k`th tarae: its screen entered once more)
    let restart = |t: &mut Term, k: usize| {
        t.command("restart");
        front_card(t, "the restart card", "restart");
        t.send("y");
        t.wait_raw("the new tarae", |r| r.windows(8).filter(|w| *w == b"\x1b[?1049h").count() >= k);
        t.wait_text("Restarted");
    };
    restart(&mut t, 2);
    std::thread::sleep(Duration::from_secs(1));
    let first = t.usage().footprint;
    settled(&mut t);
    std::thread::sleep(Duration::from_secs(20));
    let after = t.usage().footprint;
    if std::env::var_os("TARAE_HEAP").is_some() {
        eprintln!("{n} sessions restored, settled:\n{}", heap(t.pid()));
    }
    restart(&mut t, 3);
    settled(&mut t);
    std::thread::sleep(Duration::from_secs(20));
    let again = t.usage().footprint;
    if std::env::var_os("TARAE_HEAP").is_some() {
        eprintln!("{n} sessions restored again, settled:\n{}", heap(t.pid()));
    }
    Restored { live, first: mb(first), settled: mb(after), again: mb(again) }
}

/// The repository's files as of this commit — what the scenes that copy "this repository" open and search.
/// Pinned: the files keep growing, and a scene's number moved with them (the working set's footprint went
/// 31.7 → 50.3 MB from Oct 2 to 4 with tarae's own part +0.5 MB — `editor.rs` grew 477 → 643 KB). Moving it
/// is a new baseline for those numbers.
const CORPUS: &str = "53843b6";
const CORPUS_PATHS: [&str; 8] =
    ["src", "runtime", "docs", "tests", "xtask/src", "Cargo.toml", "README.md", "CONTRIBUTING.md"];

fn copy_repo(sb: &Sandbox) {
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    // (TARAE_PERF_REPO: another tree's files instead — to tell the code's growth from the files')
    if let Some(from) = std::env::var_os("TARAE_PERF_REPO") {
        return copy_tree(std::path::Path::new(&from), sb);
    }
    if !(sb.copy_commit(CORPUS, &CORPUS_PATHS) && sb.file("src/editor.rs").exists()) {
        // (a clone without that commit: today's files — a number not to compare with the history)
        eprintln!("perf: no commit {CORPUS} here — copying the working tree's files");
        copy_tree(here, sb);
    }
}

fn copy_tree(here: &std::path::Path, sb: &Sandbox) {
    for d in ["src", "runtime", "docs", "tests", "xtask/src"] {
        copy_dir(&here.join(d), &sb.file(d));
    }
    for f in ["Cargo.toml", "README.md", "CONTRIBUTING.md"] {
        fs::copy(here.join(f), sb.file(f)).unwrap();
    }
}

fn copy_dir(from: &std::path::Path, to: &std::path::Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap().flatten() {
        let (src, dst) = (e.path(), to.join(e.file_name()));
        match e.file_type() {
            Ok(t) if t.is_dir() => copy_dir(&src, &dst),
            Ok(t) if t.is_file() => {
                fs::copy(&src, &dst).unwrap();
            }
            _ => {}
        }
    }
}
