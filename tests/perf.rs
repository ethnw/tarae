//! Real-use performance: the built tarae on a pseudo-terminal, as someone uses it — startup to the first frame,
//! a key in a big file to its frame, scrolling, idle CPU, memory with this repository open. `cargo perf` (xtask
//! perf) runs these in the `perf` profile (release, with line tables) and keeps their numbers: each prints
//! `PERF <name> <value> budget=<value>`, past libtest's capture. At the test profile's opt-level 0 (`cargo
//! test`) they skip themselves, saying so — they'd measure a debug build.
#![cfg(unix)]

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
}

/// Memory with this repository open: its files (a copy, in a git repository of the test's own) listed in
/// the file picker, the biggest source file open.
#[test]
fn perf_e2e_memory_in_this_repo() {
    if unoptimized("perf_e2e_memory_in_this_repo") {
        return;
    }
    let sb = Sandbox::new("perf-mem");
    let here = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    for d in ["src", "runtime", "docs", "tests", "xtask/src"] {
        copy_dir(&here.join(d), &sb.file(d));
    }
    for f in ["Cargo.toml", "README.md", "CONTRIBUTING.md"] {
        fs::copy(here.join(f), sb.file(f)).unwrap();
    }
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
