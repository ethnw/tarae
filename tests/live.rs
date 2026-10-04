//! Live end-to-end runs: the real `tarae` on a pty with the real tools — rust-analyzer, cargo, git, and a
//! logged-in `claude`. What tests/e2e.rs can't reach (it has no network, language server or claude) and what
//! unit tests fake (`sh -c "cat > log"` servers, a scripted claude).
//!
//! Not part of `cargo test` — they take minutes and the Claude ones use your plan's usage:
//!
//! ```sh
//! cargo live                  # all of them
//! cargo live rust_analyzer    # by name
//! ```
//!
//! Each run has its own sandbox: tarae's config, state and the project live in a temp dir; PATH and HOME are
//! yours (so the tools and claude's login are found). A run that can't find a tool it needs says so and
//! passes — a missing rust-analyzer isn't a tarae bug.
#![cfg(unix)]

mod harness;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use harness::*;

/// Focus-out as the terminal reports it (tarae turns focus events on) — what leaving for Claude's pane sends.
const FOCUS_OUT: &str = "\x1b[O";

fn have(tool: &str) -> bool {
    let found = Command::new("sh").args(["-c", &format!("command -v {tool}")]).output();
    if !found.is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: {tool} isn't on PATH");
        return false;
    }
    // A rustup proxy whose component isn't installed is on PATH but isn't the tool (CI's runners: rust-analyzer)
    let runs = Command::new(tool).arg("--version").output();
    if let Ok(o) = runs
        && !o.status.success()
        && String::from_utf8_lossy(&o.stderr).contains("Unknown binary")
    {
        eprintln!("skipped: {tool} is rustup's, not installed (rustup component add {tool})");
        return false;
    }
    true
}

/// A small Cargo library: a call into another module (for go-to-definition), one passing test and one
/// failing test.
fn cargo_project(sb: &Sandbox) {
    std::fs::create_dir_all(sb.file("src")).unwrap();
    sb.write("Cargo.toml", "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    sb.write("src/util.rs", "pub fn same(x: i32) -> i32 {\n    x\n}\n");
    sb.write(
        "src/lib.rs",
        "mod util;\n\npub fn add(a: i32, b: i32) -> i32 {\n    util::same(a) + b\n}\n\n#[cfg(test)]\n\
         mod tests {\n    use super::*;\n\n    #[test]\n    fn adds() {\n        assert_eq!(add(1, 2), 3);\n    }\n\n\
         \x20   #[test]\n    fn fails() {\n        assert_eq!(add(2, 2), 5);\n    }\n}\n",
    );
}

/// Polls a file until `ok` holds, within `within`.
fn wait_file_until(path: &Path, what: &str, within: Duration, ok: impl Fn(&str) -> bool) -> String {
    let end = Instant::now() + within;
    loop {
        let got = std::fs::read_to_string(path).unwrap_or_default();
        if ok(&got) {
            return got;
        }
        assert!(Instant::now() < end, "{} never {what}; last read: {got:?}", path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The chat's turn is over: its footer offers "enter send" again (while Claude works it offers "C-c stop").
/// Only a focused chat says — after a review, `space l` first.
fn chat_idle(s: &Screen) -> bool {
    s.contains("enter send") && !s.contains("C-c stop")
}

/// One chat turn: focus the chat if it isn't, send `msg`, and wait until Claude has answered.
fn chat_turn(t: &mut Term, msg: &str) -> Screen {
    if !t.screen().contains("enter send") {
        t.send(" l");
        t.wait_for("the chat, focused", |s| s.contains("enter send"));
    }
    t.send(&format!("{msg}\r"));
    // It starts (the footer offers "C-c stop") — a very quick turn may be over before the first look
    let _ = t.poll_for(Duration::from_secs(15), |s| s.contains("C-c stop"));
    t.wait_for("Claude's answer", chat_idle)
}

/// Lets a caller that needs a tool the machine may lack skip — with the reason in the output.
fn skip(why: &str) -> bool {
    eprintln!("skipped: {why}");
    true
}

/// Presses a key again until the screen shows `done` — for questions a server still loading can't answer yet.
fn until(t: &mut Term, keys: &str, what: &str, done: impl Fn(&Screen) -> bool) -> Screen {
    let end = Instant::now() + LIVE_TIMEOUT;
    loop {
        t.send(keys);
        if let Some(s) = t.poll_for(Duration::from_secs(3), &done) {
            return s;
        }
        assert!(Instant::now() < end, "never {what}\n{}", t.screen());
        t.send("\x1b");
        std::thread::sleep(Duration::from_millis(200));
    }
}

fn raw_text(t: &Term) -> String {
    String::from_utf8_lossy(&t.raw()).into_owned()
}

// ── Language server, tests ───────────────────────────────────────────────────

#[test]
#[ignore = "live: run with `cargo live`"]
fn rust_analyzer_diagnoses_completes_and_jumps() {
    if !have("rust-analyzer") || !have("cargo") {
        return;
    }
    let sb = Sandbox::live("ra");
    cargo_project(&sb);
    let mut t = sb.spawn(&["src/lib.rs"]);
    t.wait_text("rust-analyzer ready");

    // A type error, saved → cargo check's diagnostic at the line end
    t.send("ge");
    t.send("o");
    t.wait_for("INSERT mode", |s| s.status().contains("INSERT"));
    t.send("pub fn bad() -> i32 { \"x\" }");
    t.normal_mode();
    t.command("w");
    t.wait_for("the type error's diagnostic", |s| s.contains("mismatched types") || s.contains("expected"));

    // Completion from the server: `util::` offers `same`
    t.send("o");
    t.wait_for("INSERT mode", |s| s.status().contains("INSERT"));
    t.send("fn c() -> i32 { util::sa");
    t.wait_for("rust-analyzer's completion list", |s| s.rows.iter().any(|r| r.contains("same(")));
    t.normal_mode();
    t.command("q!");

    // Go to definition across files
    let mut t = sb.spawn(&["src/lib.rs"]);
    t.wait_text("rust-analyzer ready");
    t.send("/same\r");
    t.wait_text("/same");
    // Right after startup the server is still indexing: one gd is enough — tarae says it's busy and asks
    // again when the indexing ends (measured: the notice shows in this fresh project)
    t.send("gd");
    t.wait_for("util.rs after gd", |s| s.rows[0].contains("util.rs") && s.contains("pub fn same"));
}

#[test]
#[ignore = "live: run with `cargo live`"]
fn cargo_test_at_the_cursor_marks_the_failure() {
    if !have("cargo") {
        return;
    }
    let sb = Sandbox::live("tests");
    sb.write_config(&format!("{LIVE_CONFIG}lsp = false\n"));
    cargo_project(&sb);
    let mut t = sb.spawn(&["src/lib.rs"]);
    t.send("/fn fails\r");
    t.send(" xx");
    t.wait_for("the failing assertion in the test panel", |s| s.contains("left == right"));
    t.wait_for("the ▲ at the failing line", |s| {
        s.rows.iter().any(|r| r.contains("assert_eq!(add(2, 2), 5)") && r.contains('▲'))
    });
    // The whole file: one passes, one fails
    t.send(" xf");
    t.wait_for("● 1 and ▲ 1 in the summary", |s| s.contains("● 1") && s.contains("▲ 1"));
}

// ── Claude ───────────────────────────────────────────────────────────────────

/// `space i` on a selection → Claude's answer as a review in the buffer → accept → save.
#[test]
#[ignore = "live: run with `cargo live` (uses Claude)"]
fn claude_edits_the_selection() {
    if !have("claude") {
        return;
    }
    let sb = Sandbox::live("ask");
    sb.write_config(&format!("{LIVE_CONFIG}lsp = false\n"));
    let path = sb.write("notes.txt", "alpha\nbeta\n");
    let mut t = sb.spawn(&["notes.txt"]);
    t.send("jx");
    t.send(" i");
    t.wait_text("ask ");
    t.send("Uppercase this line. Change nothing else.\r");
    t.wait_for("the review in the buffer", |s| s.contains("accept") && s.contains("reject"));
    t.send("a");
    t.wait_text("BETA");
    t.command("w");
    wait_file_until(&path, "held the accepted edit", Duration::from_secs(5), |s| s == "alpha\nBETA\n");
}

/// The chat edits a file; you accept in the buffer and leave for another pane at once — before and after
/// Claude has written the file. Measured: "auto-save failed" toasts after Claude's edits (fixed 2026-09-29 —
/// a save finding my own text on disk isn't a conflict).
#[test]
#[ignore = "live: run with `cargo live` (uses Claude)"]
fn claude_chat_edit_accepted_then_focus_leaves() {
    if !have("claude") {
        return;
    }
    let sb = Sandbox::live("chat");
    sb.write_config(&format!("{LIVE_CONFIG}lsp = false\n"));
    let path = sb.write("notes.txt", "alpha\nbeta\n");
    let mut t = sb.spawn(&["notes.txt"]);
    t.send(" l");
    t.wait_text("Ask about notes.txt");

    // 1) Leave right after accepting — tarae's auto-save may beat Claude's write
    t.send("In notes.txt replace the word beta with gamma, using your Edit tool. Do nothing else.\r");
    t.wait_for("the review of Claude's edit", |s| s.contains("accept") && s.contains("reject"));
    // One write: tarae takes both in the same batch, before claude can have written — the race, every time
    let from = t.raw().len();
    t.send(&format!("a{FOCUS_OUT}"));
    wait_file_until(&path, "held gamma", LIVE_TIMEOUT, |s| s == "alpha\ngamma\n");
    t.send("\x1b[I"); // focus back
    t.send(" l");
    t.wait_for("Claude's turn to end", chat_idle);
    // Auto-save left the file to Claude (its toast would say "Saved notes.txt"): saving first can make
    // Claude's Edit fail — "it failed because the file had changed since I read it" (measured)
    assert!(
        !String::from_utf8_lossy(&t.raw()[from..]).contains("Saved notes.txt"),
        "auto-save wrote before Claude\n{}",
        t.screen()
    );
    let said = t.screen().rows.join("\n");
    assert!(
        !said.contains("changed since") && !said.contains("failed"),
        "Claude's edit failed\n{}",
        t.screen()
    );

    // 2) Leave the moment Claude has written — before tarae's disk watch (0.5 s) has seen it
    t.send("Now replace gamma with delta the same way.\r");
    t.wait_for("the second review", |s| s.contains("accept") && s.contains("reject"));
    t.send("a");
    wait_file_until(&path, "held delta", LIVE_TIMEOUT, |s| s.contains("delta"));
    t.send(FOCUS_OUT);
    t.send(" l"); // back to the chat (a review takes the keys from it) — its footer tells a turn's end
    t.wait_for("Claude's turn to end", chat_idle);
    std::thread::sleep(Duration::from_secs(1)); // the watch, a newline write-back, toasts

    assert_eq!(std::fs::read_to_string(&path).unwrap(), "alpha\ndelta\n");
    let raw = raw_text(&t);
    assert!(!raw.contains("auto-save failed"), "an auto-save failure was shown\n{}", t.screen());
    assert!(!raw.contains("changed on disk"), "a false conflict was shown\n{}", t.screen());
    assert!(!t.screen().rows[0].contains('●'), "the buffer is left modified\n{}", t.screen());
}

// ── One rust-analyzer for a repository's sessions (share.rs) ────────────────

/// A clone of this repository in /private/tmp, its sessions' worktrees beside it, and their build dirs
/// (a `.cargo/config.toml` above them — never `~/.cargo/build`). Dropped — passed or not — the run's tarae
/// group is killed (its servers, proc-macro servers and cargo are in it: the harness `setsid`s tarae) and
/// the folder removed.
#[cfg(target_os = "macos")]
struct Scratch {
    base: std::path::PathBuf,
    group: Option<i32>,
}

#[cfg(target_os = "macos")]
impl Drop for Scratch {
    fn drop(&mut self) {
        if let Some(g) = self.group {
            // SAFETY: a signal to the process group the harness made for this run's tarae.
            unsafe { libc::killpg(g, libc::SIGKILL) };
            std::thread::sleep(Duration::from_millis(300));
        }
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// The processes under `pid` (it included), and what each is: (pid, name, footprint bytes, command line).
#[cfg(target_os = "macos")]
fn tree(pid: u32) -> Vec<(u32, String, u64, String)> {
    let mut out = Vec::new();
    let mut todo = vec![pid];
    while let Some(p) = todo.pop() {
        // SAFETY: plain out-structs and buffers of the sizes given; the calls only fill them in.
        unsafe {
            let mut bsd: libc::proc_bsdinfo = std::mem::zeroed();
            let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
            if libc::proc_pidinfo(p as i32, libc::PROC_PIDTBSDINFO, 0, (&raw mut bsd).cast(), size) != size {
                continue;
            }
            let name: String =
                bsd.pbi_name.iter().take_while(|&&c| c != 0).map(|&c| c as u8 as char).collect();
            let mut ri: libc::rusage_info_v2 = std::mem::zeroed();
            let fp = if libc::proc_pid_rusage(p as i32, libc::RUSAGE_INFO_V2, (&raw mut ri).cast()) == 0 {
                ri.ri_phys_footprint
            } else {
                0
            };
            let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, p as libc::c_int];
            let mut len: libc::size_t = 0;
            let mut args = String::new();
            if libc::sysctl(mib.as_mut_ptr(), 3, std::ptr::null_mut(), &mut len, std::ptr::null_mut(), 0) == 0
            {
                let mut buf = vec![0u8; len];
                if libc::sysctl(
                    mib.as_mut_ptr(),
                    3,
                    buf.as_mut_ptr().cast(),
                    &mut len,
                    std::ptr::null_mut(),
                    0,
                ) == 0
                {
                    args = String::from_utf8_lossy(&buf[4..len]).replace('\0', " ");
                }
            }
            out.push((p, name, fp, args));
            let mut kids: Vec<libc::pid_t> = vec![0; 256];
            let n =
                libc::proc_listchildpids(p as i32, kids.as_mut_ptr().cast(), (kids.len() * 4) as libc::c_int);
            todo.extend(kids.iter().take(n.max(0) as usize).filter(|&&k| k > 0).map(|&k| k as u32));
        }
    }
    out
}

/// Every 200 ms: the rust-analyzers' footprint (with their proc-macro servers), and each cargo started under
/// tarae (when it was first seen, its command line).
#[cfg(target_os = "macos")]
#[derive(Default)]
struct Watched {
    samples: Vec<(Instant, u64, usize)>,
    cargo: Vec<(u32, Instant, String)>,
}

/// The right answer to `gd` on `self.lsp_stopped(cid)` in `src/procs.rs`: its definition on screen.
#[cfg(target_os = "macos")]
const RIGHT: &str = "fn lsp_stopped(&mut self";

/// The cursor on the call (`src/procs.rs` of the session in front), then `gd`: when it was sent, and when the
/// definition showed (None: not within 2.5 s — loading, or nothing).
#[cfg(target_os = "macos")]
fn ask(t: &mut Term) -> (Instant, Option<Instant>) {
    esc(t);
    t.command("o src/procs.rs");
    // (a regex; the cursor ends at the match's end — on the method's name)
    t.send("gg/self\\.lsp_stopped\r");
    let _ = t.poll_for(Duration::from_secs(5), |s| s.contains("self.lsp_stopped(cid)") && !s.contains(RIGHT));
    let sent = Instant::now();
    t.send("gd");
    let got = t.poll_for(Duration::from_millis(2500), |s| s.contains(RIGHT)).map(|_| Instant::now());
    (sent, got)
}

/// `ask` again and again until `done` says (given every answer so far), or `within`: every answer.
#[cfg(target_os = "macos")]
fn asking(
    t: &mut Term,
    within: Duration,
    done: impl Fn(&[(Instant, Option<Instant>)]) -> bool,
) -> Vec<(Instant, Option<Instant>)> {
    let end = Instant::now() + within;
    let mut all = Vec::new();
    while Instant::now() < end && !done(&all) {
        all.push(ask(t));
    }
    all
}

/// The status line hasn't shown rust-analyzer at work for `secs` in a row (at most `LIVE_TIMEOUT`).
#[cfg(target_os = "macos")]
fn quiet(t: &mut Term, secs: u64) {
    let end = Instant::now() + LIVE_TIMEOUT;
    let mut since = Instant::now();
    while Instant::now() < end && since.elapsed() < Duration::from_secs(secs) {
        if t.screen().contains("rust-analyzer:") {
            since = Instant::now();
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The project's `.tarae.toml` card (this repository's risky settings), answered `n` if it comes within
/// `within` — its settings left out.
#[cfg(target_os = "macos")]
fn untrusted(t: &mut Term, within: Duration) {
    esc(t);
    if t.poll_for(within, |s| s.contains("Trust this project")).is_some() {
        // (a card takes keys once it's settled in sight — `offer::SETTLE`)
        let end = Instant::now() + Duration::from_secs(15);
        while t.screen().contains("Trust this project") && Instant::now() < end {
            t.send("n");
            let _ = t.poll_for(Duration::from_millis(500), |s| !s.contains("Trust this project"));
        }
        t.wait_for("the trust card gone", |s| !s.contains("Trust this project"));
    }
}

/// A new session `name` (`:session-new`, its card's `y`), its chat closed again: when `y` was pressed.
/// Esc on its own — a key right after it would read as Alt+key.
#[cfg(target_os = "macos")]
fn esc(t: &mut Term) {
    t.send("\x1b");
    std::thread::sleep(Duration::from_millis(100));
}

#[cfg(target_os = "macos")]
fn new_session(t: &mut Term, name: &str) -> Instant {
    esc(t);
    t.command(&format!("session-new {name}"));
    let card = format!("Branch {name}");
    t.wait_for("the new session's card", |s| s.contains(&card));
    // (taken once it's settled in sight — `offer::SETTLE`; a `y` after it went is a yank, harmless)
    let mut at = Instant::now();
    while t.screen().contains(&card) {
        at = Instant::now();
        t.send("y");
        let _ = t.poll_for(Duration::from_millis(300), |s| !s.contains(&card));
    }
    t.wait_for("the session made", |s| s.contains(&format!("Session {name}")));
    // (its chat has the keys — back to the code, the chat closed)
    t.send("\x17h");
    t.command("chat-close");
    untrusted(t, Duration::from_secs(2));
    at
}

/// Processes anywhere on the machine whose command line names `base` (the run's leftovers).
#[cfg(target_os = "macos")]
fn leftovers(base: &Path) -> Vec<(u32, String)> {
    let mut pids: Vec<libc::pid_t> = vec![0; 16384];
    // SAFETY: a buffer of the size given.
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), (pids.len() * 4) as libc::c_int) };
    let needle = base.display().to_string();
    pids.iter()
        .take(n.max(0) as usize)
        .filter(|&&p| p > 0)
        .filter_map(|&p| tree(p as u32).into_iter().next())
        .filter(|p| p.3.contains(&needle))
        .map(|p| (p.0, p.3))
        .collect()
}

/// What the asking saw from its first question on: answers, right ones, the slowest right one, the longest
/// stretch without one.
#[cfg(target_os = "macos")]
fn said(all: &[(Instant, Option<Instant>)]) -> String {
    let Some(from) = all.first().map(|a| a.0) else { return "no asks".into() };
    let right: Vec<Instant> = all.iter().filter_map(|a| a.1).collect();
    let slowest =
        all.iter().filter_map(|(s, g)| Some(g.as_ref()?.duration_since(*s))).max().unwrap_or_default();
    let end = all.last().map_or(from, |a| a.1.unwrap_or(a.0));
    let mut pts = vec![from];
    pts.extend(right.iter().copied());
    pts.push(end);
    let gap = pts.windows(2).map(|w| w[1].saturating_duration_since(w[0])).max().unwrap_or_default();
    format!(
        "{} asks, {} right, slowest right {:.0} ms, longest without a right answer {:.2} s",
        all.len(),
        right.len(),
        slowest.as_secs_f64() * 1000.0,
        gap.as_secs_f64()
    )
}

/// Live: a clone of this repository and sessions of it (worktrees) share one rust-analyzer 1.99. Numbers
/// (printed): a new session's first right `gd` from when it's made (its first file opened 5 s later, as a
/// person or Claude gets to it); the session in front asking `gd` all through a second session's swap,
/// a switch, and an ended session's swap; the servers' peak footprint; and saves with typing right after
/// them (each check must still start). `TARAE_LIVE_SHORT=1`: only the new session's first answer (to compare
/// builds, `TARAE_BIN`).
#[test]
#[ignore = "live: run with `cargo live`"]
#[cfg(target_os = "macos")]
fn rust_analyzer_shared_by_sessions() {
    if !have("rust-analyzer") || !have("cargo") {
        return;
    }
    let short = std::env::var_os("TARAE_LIVE_SHORT").is_some();
    let base = std::path::PathBuf::from(format!("/private/tmp/ra-live-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let mut scratch = Scratch { base: base.clone(), group: None };
    std::fs::create_dir_all(base.join(".cargo")).unwrap();
    std::fs::write(
        base.join(".cargo/config.toml"),
        format!("[build]\nbuild-dir = \"{}/build/{{workspace-path-hash}}\"\n", base.display()),
    )
    .unwrap();
    let main = base.join("main");
    let st = Command::new("git")
        .args(["clone", "-q", "--no-hardlinks", env!("CARGO_MANIFEST_DIR")])
        .arg(&main)
        .status()
        .unwrap();
    assert!(st.success(), "clone");
    // (warm, as a session's checkout that has been built is)
    let t0 = Instant::now();
    let st =
        Command::new("cargo").args(["check", "--all-targets", "-q", "--offline"]).current_dir(&main).status();
    assert!(st.is_ok_and(|s| s.success()), "cargo check of the clone");
    eprintln!("LIVE clone checked in {:.1} s", t0.elapsed().as_secs_f64());

    let sb = Sandbox::live("ra-share");
    sb.write_config(&format!(
        "{LIVE_CONFIG}auto-save = \"off\"\n[session]\nworktrees = \"{}/wt\"\n\
         [llm]\ncommand = \"sh\"\nargs = [\"-c\", \"exit 0\"]\n",
        base.display()
    ));
    let log = sb.home().join(".local/state/tarae/tarae.log");
    let logged = |what: &str| std::fs::read_to_string(&log).unwrap_or_default().matches(what).count();
    let mut t = Term::spawn_in(&sb, &main, &["src/procs.rs"], 160, 40);
    let tarae = t.pid();
    scratch.group = Some(tarae as i32);
    untrusted(&mut t, Duration::from_secs(5));

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let watched = std::sync::Arc::new(std::sync::Mutex::new(Watched::default()));
    let sampler = {
        let (stop, watched) = (stop.clone(), watched.clone());
        std::thread::spawn(move || {
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                let procs = tree(tarae);
                let ra: Vec<_> = procs.iter().filter(|p| p.1.starts_with("rust-analyzer")).collect();
                let servers = ra.iter().filter(|p| p.1 == "rust-analyzer").count();
                let mut w = watched.lock().unwrap();
                w.samples.push((Instant::now(), ra.iter().map(|p| p.2).sum(), servers));
                for p in procs.iter().filter(|p| p.1 == "cargo") {
                    if !w.cargo.iter().any(|c| c.0 == p.0) {
                        w.cargo.push((p.0, Instant::now(), p.3.clone()));
                    }
                }
                drop(w);
                std::thread::sleep(Duration::from_millis(200));
            }
        })
    };
    let peak = |from: Instant, to: Instant| {
        let w = watched.lock().unwrap();
        let s = w.samples.iter().filter(|s| s.0 >= from && s.0 <= to).max_by_key(|s| s.1).copied();
        s.map_or("—".into(), |s| format!("{:.2} GB ({} servers)", s.1 as f64 / 1e9, s.2))
    };
    let start = Instant::now();
    let right_once = |all: &[(Instant, Option<Instant>)]| all.last().is_some_and(|a| a.1.is_some());

    // The first session's server, loaded and quiet
    let first = asking(&mut t, LIVE_TIMEOUT, right_once);
    let Some(at) = first.last().and_then(|a| a.1) else {
        let text = std::fs::read_to_string(&log).unwrap_or_default();
        let tail: Vec<&str> = text.lines().rev().take(40).collect();
        panic!("main's gd never right\n{}\nlog (newest first):\n{}", t.screen(), tail.join("\n"));
    };
    eprintln!("LIVE main: first right gd {:.1} s after the start", at.duration_since(start).as_secs_f64());
    quiet(&mut t, 5);

    // A. A new session: its first right answer, from when it's made (its file opened 5 s later)
    let n = logged("switched to the new");
    let made = new_session(&mut t, "s1");
    std::thread::sleep(Duration::from_secs(5).saturating_sub(made.elapsed()));
    let all = asking(&mut t, LIVE_TIMEOUT, right_once);
    let got = all.last().and_then(|a| a.1).expect("s1's gd never right");
    eprintln!(
        "LIVE A. new session s1: first right gd {:.1} s after it was made ({} asks) · peak so far {}",
        got.duration_since(made).as_secs_f64(),
        all.len(),
        peak(start, Instant::now())
    );
    if short {
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = sampler.join();
        return ended(t, &scratch, &log);
    }
    // (its swap over)
    let end = Instant::now() + LIVE_TIMEOUT;
    while logged("switched to the new") <= n && logged("restart: ") == 0 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(200));
    }
    quiet(&mut t, 5);

    // B. Another new session; the first one in front asks all through its swap
    let n = logged("switched to the new");
    let made = new_session(&mut t, "s2");
    t.command("session-switch 0");
    let all = asking(&mut t, LIVE_TIMEOUT, |_| logged("switched to the new") > n);
    let more = asking(&mut t, Duration::from_secs(5), |_| false);
    let all: Vec<_> = all.into_iter().chain(more).collect();
    eprintln!(
        "LIVE B. main while s2's swap ran ({:.1} s): {} · peak {}",
        made.elapsed().as_secs_f64(),
        said(&all),
        peak(made, Instant::now())
    );
    quiet(&mut t, 5);

    // C. A switch to s2 (its server warm since the swap)
    t.command("session-switch 2");
    let at = Instant::now();
    let all = asking(&mut t, LIVE_TIMEOUT, right_once);
    let got = all.last().and_then(|a| a.1).expect("s2's gd never right");
    eprintln!(
        "LIVE C. switch to s2: first right gd {:.2} s after the switch ({} asks)",
        got.duration_since(at).as_secs_f64(),
        all.len()
    );

    // D. s1 ended: the swap with the shorter list; main asks all through it
    t.command("session-switch 0");
    let n = logged("switched to the new");
    let at = Instant::now();
    t.command("session-end s1");
    // (its checkout removed — a card if it had anything to lose; it hasn't)
    let gone = |s: &Screen| s.contains("Ended s1") || s.contains("s1 ended") || s.contains("removed");
    if t.poll_for(Duration::from_secs(10), gone).is_none() {
        eprintln!("LIVE D. after :session-end s1 —\n{}", t.screen());
    }
    let all = asking(&mut t, LIVE_TIMEOUT, |_| logged("switched to the new") > n);
    let more = asking(&mut t, Duration::from_secs(5), |_| false);
    let all: Vec<_> = all.into_iter().chain(more).collect();
    eprintln!(
        "LIVE D. main while s1's end swapped ({:.1} s): {} · peak {}",
        at.elapsed().as_secs_f64(),
        said(&all),
        peak(at, Instant::now())
    );
    quiet(&mut t, 5);

    // E. Saves with typing right after them: each one's check still starts
    esc(&mut t);
    t.command("o src/log.rs");
    t.send("Go// live");
    esc(&mut t);
    std::thread::sleep(Duration::from_millis(300));
    let manifest = main.display().to_string();
    let again0 = logged("sent again");
    let mut rows = Vec::new();
    for _ in 0..6 {
        let saved = Instant::now();
        t.command("w");
        t.send("A");
        for _ in 0..25 {
            t.send("x");
            std::thread::sleep(Duration::from_millis(8));
        }
        let last = Instant::now();
        t.send("\x1b");
        std::thread::sleep(Duration::from_secs(4));
        let w = watched.lock().unwrap();
        let checks = |from: Instant, to: Instant| {
            w.cargo
                .iter()
                .filter(|c| c.1 >= from && c.1 < to && c.2.contains(" check") && c.2.contains(&manifest))
                .count()
        };
        rows.push((checks(saved, last), checks(last, Instant::now())));
    }
    eprintln!(
        "LIVE E. 6 saves, each with 25 keys typed over 200 ms right after: saves sent again {} · checks started (during the typing, after it) {rows:?}",
        logged("sent again") - again0
    );

    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let _ = sampler.join();
    eprintln!("LIVE peak over the run: {}", peak(start, Instant::now()));
    ended(t, &scratch, &log);
}

/// tarae quits, its group is killed (what quitting left), and what of the run is still running is said —
/// nothing, or the test fails.
#[cfg(target_os = "macos")]
fn ended(mut t: Term, scratch: &Scratch, log: &Path) {
    t.command("qa!");
    let _ = t.wait_exit();
    if let Some(g) = scratch.group {
        // SAFETY: a signal to the process group the harness made for this run's tarae.
        unsafe { libc::killpg(g, libc::SIGKILL) };
    }
    std::thread::sleep(Duration::from_millis(500));
    eprintln!("LIVE log:\n{}", share_lines(log));
    let left = leftovers(&scratch.base);
    assert!(left.is_empty(), "left running: {left:?}");
    eprintln!("LIVE nothing of the run left running");
}

/// The run's swap, restart and share lines in tarae's log.
#[cfg(target_os = "macos")]
fn share_lines(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let keep = ["swap", "restart", "share", "stop", "save", "health"];
    text.lines()
        .filter(|l| l.contains("rust-analyzer") && keep.iter().any(|k| l.contains(k)))
        .map(|l| l.chars().take(260).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Language server: code actions, rename, format ────────────────────────────

/// `space a` lists rust-analyzer's fixes with a preview and applies the picked one; `space r` renames across
/// files; `:w` formats first.
#[test]
#[ignore = "live: run with `cargo live`"]
fn rust_analyzer_code_action_rename_and_format() {
    if !have("rust-analyzer") || !have("cargo") {
        return;
    }
    let sb = Sandbox::live("ra-edit");
    cargo_project(&sb);
    let lib = sb.file("src/lib.rs");
    let text = std::fs::read_to_string(&lib).unwrap();
    std::fs::write(&lib, format!("{text}\npub fn pick(x: Option<i32>) -> i32 {{\n    match x {{}}\n}}\n\npub fn ugly( )->i32{{1}}\n"))
        .unwrap();
    let mut t = sb.spawn(&["src/lib.rs"]);
    t.wait_text("rust-analyzer ready");

    // A code action: fill the empty match
    t.send("/match x\r");
    t.wait_text("/match x");
    until(&mut t, " a", "offered the missing match arms", |s| s.contains("missing match arms"));
    t.send("missing match arms");
    t.wait_for("its preview", |s| s.contains("None =>"));
    t.send("\r");
    t.wait_for("the arms in the buffer", |s| s.contains("None =>") && !s.contains("missing match arms"));

    // Rename across files (util.rs isn't open)
    t.send("gg/same\r");
    t.wait_text("/same");
    t.send(" r");
    t.wait_for("the rename prompt", |s| {
        s.status().contains("same") || s.rows.iter().any(|r| r.contains("rename"))
    });
    t.send("\x7f\x7f\x7f\x7fident\r");
    t.wait_text("util::ident(a)");
    t.command("wa");
    let util = sb.file("src/util.rs");
    wait_file_until(&util, "renamed", Duration::from_secs(10), |s| s.contains("pub fn ident(x: i32)"));

    // Saved formatted
    wait_file_until(&lib, "formatted on save", Duration::from_secs(30), |s| {
        s.contains("pub fn ugly() -> i32 {\n    1\n}") && s.contains("None => todo!()")
    });
}

// ── Debugger ─────────────────────────────────────────────────────────────────

/// A breakpoint, `space x d` on a test: it stops there, with the values inline and in the panel.
fn debug_test_stops_at_breakpoint(sb: &Sandbox, open: &str, bp: &str, test_file: &str, test: &str) {
    let mut t = sb.spawn(&[open]);
    t.send(&format!("/{bp}\r"));
    t.wait_text(&format!("/{bp}"));
    t.send(" Gb");
    t.wait_for("the breakpoint in the gutter", |s| s.contains("●"));
    if open != test_file {
        t.command(&format!("open {test_file}"));
        t.wait_text(test);
    }
    t.send(&format!("/{test}\r"));
    t.wait_text(&format!("/{test}"));
    t.send(" xd");
    let s = t.wait_for("stopped at the breakpoint", |s| s.contains("F10 over") && s.contains("a = 1"));
    assert!(s.contains("VARIABLES"), "{s}");
    t.send(" Gt");
    t.wait_for("the debug run over", |s| !s.contains("F10 over"));
}

#[test]
#[ignore = "live: run with `cargo live`"]
fn rust_debug_test_stops_at_breakpoint() {
    let lldb = Command::new("xcrun").args(["-f", "lldb-dap"]).output().is_ok_and(|o| o.status.success());
    if !have("cargo") || (!lldb && !have("lldb-dap") && skip("no lldb-dap")) {
        return;
    }
    let sb = Sandbox::live("dbg-rust");
    sb.write_config(&format!("{LIVE_CONFIG}lsp = false\n"));
    cargo_project(&sb);
    debug_test_stops_at_breakpoint(&sb, "src/lib.rs", "util::same", "src/lib.rs", "fn adds");
}

#[test]
#[ignore = "live: run with `cargo live`"]
fn go_debug_test_stops_at_breakpoint() {
    if !have("go") || !have("dlv") {
        return;
    }
    let sb = Sandbox::live("dbg-go");
    sb.write_config(&format!("{LIVE_CONFIG}lsp = false\n"));
    sb.write("go.mod", "module demo\n\ngo 1.21\n");
    sb.write("add.go", "package demo\n\nfunc Add(a, b int) int {\n\treturn a + b\n}\n");
    sb.write(
        "add_test.go",
        "package demo\n\nimport \"testing\"\n\nfunc TestAdd(t *testing.T) {\n\tif Add(1, 2) != 3 {\n\t\tt.Fatal(\"bad\")\n\t}\n}\n",
    );
    debug_test_stops_at_breakpoint(&sb, "add.go", "return a", "add_test.go", "func TestAdd");
}

#[test]
#[ignore = "live: run with `cargo live`"]
fn python_debug_test_stops_at_breakpoint() {
    let debugpy =
        Command::new("python3").args(["-c", "import debugpy"]).output().is_ok_and(|o| o.status.success());
    if !debugpy && skip("python3 has no debugpy (pip install debugpy)") {
        return;
    }
    let sb = Sandbox::live("dbg-py");
    sb.write_config(&format!("{LIVE_CONFIG}lsp = false\n"));
    sb.write("calc.py", "def add(a, b):\n    return a + b\n");
    sb.write("test_calc.py", "from calc import add\n\n\ndef test_add():\n    assert add(1, 2) == 3\n");
    debug_test_stops_at_breakpoint(&sb, "calc.py", "return a", "test_calc.py", "def test_add");
}

// ── The chat (space l) ───────────────────────────────────────────────────────

/// Claude checks with tarae's run_tests tool: the tests run in the test panel, and the answer names the failure.
#[test]
#[ignore = "live: run with `cargo live` (uses Claude)"]
fn chat_runs_the_tests() {
    if !have("claude") || !have("cargo") {
        return;
    }
    let sb = Sandbox::live("chat-tests");
    sb.write_config(&format!("{LIVE_CONFIG}lsp = false\n"));
    cargo_project(&sb);
    let mut t = sb.spawn(&["src/lib.rs"]);
    let s = chat_turn(
        &mut t,
        "Use your run_tests tool to run the tests in src/lib.rs. Then answer in one line: which test failed? \
         Don't edit anything.",
    );
    assert!(raw_text(&t).contains("left == right"), "the tests ran in tarae's test panel\n{s}");
}

/// Claude pins a note on the code with its note tool; `space N` lists it.
#[test]
#[ignore = "live: run with `cargo live` (uses Claude)"]
fn chat_pins_a_note() {
    if !have("claude") {
        return;
    }
    let sb = Sandbox::live("chat-note");
    sb.write_config(&format!("{LIVE_CONFIG}lsp = false\n"));
    cargo_project(&sb);
    let mut t = sb.spawn(&["src/lib.rs"]);
    chat_turn(
        &mut t,
        "Pin exactly one note with your note tool on the add function in src/lib.rs, saying in a few words what \
         it does. Do nothing else.",
    );
    t.send("\x1b");
    t.wait_for("back in the editor", |s| !s.contains("enter send"));
    t.send(" N");
    t.wait_for("the note in the notes picker", |s| s.contains("lib.rs"));
}

/// The chat's claude with its stream-json copied to `stream.jsonl` in the project as it goes (a `tee` in
/// between) — what each of its requests cost is in that stream (`usage`).
fn teed_chat(name: &str) -> (Sandbox, std::path::PathBuf) {
    let sb = Sandbox::live(name);
    let out = sb.file("stream.jsonl");
    let script =
        sb.write("claude-tee.sh", &format!("#!/bin/sh\nclaude \"$@\" | tee -a '{}'\n", out.display()));
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    sb.write_config(&format!("{LIVE_CONFIG}lsp = false\n[llm]\ncommand = \"{}\"\n", script.display()));
    (sb, out)
}

/// Each request's prompt as claude's `usage` counts it — input + cache written + cache read, the whole
/// prompt whatever the cache did — in the order they went, one per message id.
fn prompts(stream: &str) -> Vec<u64> {
    let mut seen: Vec<String> = Vec::new();
    let mut out = Vec::new();
    for line in stream.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else { continue };
        let m = &v["message"];
        if v["type"] != "assistant" || m["usage"].is_null() {
            continue;
        }
        let id = m["id"].as_str().unwrap_or_default().to_string();
        if seen.contains(&id) {
            continue;
        }
        seen.push(id);
        let u = &m["usage"];
        let n = |k: &str| u[k].as_u64().unwrap_or(0);
        out.push(n("input_tokens") + n("cache_creation_input_tokens") + n("cache_read_input_tokens"));
    }
    out
}

/// Token cost: the fixed part every request carries (system prompt, tools' definitions, Claude Code's own) —
/// the first request's prompt for a one-word question. Run against two builds (`TARAE_BIN`) to see what a
/// change adds; the question and the project are the same each time.
#[test]
#[ignore = "live: run with `cargo live` (uses Claude)"]
fn chat_prompt_tokens() {
    if !have("claude") {
        return;
    }
    let (sb, out) = teed_chat("chat-tokens");
    sb.write("notes.txt", "alpha\n");
    let mut t = sb.spawn(&["notes.txt"]);
    chat_turn(&mut t, "Reply with the single word ok. Use no tools.");
    let p = prompts(&std::fs::read_to_string(&out).unwrap_or_default());
    eprintln!("TOKENS first request's prompt: {:?} (all requests: {p:?})", p.first());
    assert!(!p.is_empty(), "claude's usage in its stream");
}

/// A turn whose context carries the `ui:` line (a toast up): its first request's prompt, beside
/// `chat_prompt_tokens`' — the difference is the line.
#[test]
#[ignore = "live: run with `cargo live` (uses Claude)"]
fn chat_prompt_tokens_with_a_toast_up() {
    if !have("claude") {
        return;
    }
    let (sb, out) = teed_chat("chat-tokens-toast");
    sb.write("notes.txt", "alpha\n");
    let mut t = sb.spawn(&["notes.txt"]);
    // (`:w` says it saved — a toast up for a few seconds, the message sent under it)
    t.command("w");
    t.send(" l");
    t.wait_for("the chat, focused", |s| s.contains("enter send"));
    chat_turn(&mut t, "Reply with the single word ok. Use no tools.");
    let p = prompts(&std::fs::read_to_string(&out).unwrap_or_default());
    eprintln!("TOKENS first request's prompt: {:?}", p.first());
}

/// Token cost of one `ui` call: the second request's prompt less the first's is the call and its answer.
#[test]
#[ignore = "live: run with `cargo live` (uses Claude)"]
fn chat_ui_call_tokens() {
    if !have("claude") {
        return;
    }
    let (sb, out) = teed_chat("chat-ui-tokens");
    sb.write("notes.txt", "alpha\n");
    let mut t = sb.spawn(&["notes.txt"]);
    chat_turn(&mut t, "Call your ui tool once, then reply with the single word ok. Nothing else.");
    let stream = std::fs::read_to_string(&out).unwrap_or_default();
    let p = prompts(&stream);
    let answer = stream
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .filter(|v| v["type"] == "user")
        .find_map(|v| {
            let c = v["message"]["content"].as_array()?.first()?.clone();
            (c["type"] == "tool_result").then(|| c["content"].to_string())
        })
        .unwrap_or_default();
    eprintln!("TOKENS requests' prompts: {p:?}; the ui answer: {} bytes\n{answer}", answer.len());
    assert!(p.len() >= 2, "a request after the call: {p:?}");
}

/// A turn's edit, undone with `space u` (on disk too); after a restart the conversation goes on, and Claude
/// remembers it.
#[test]
#[ignore = "live: run with `cargo live` (uses Claude)"]
fn chat_turn_undo_and_resume() {
    if !have("claude") {
        return;
    }
    let sb = Sandbox::live("chat-resume");
    sb.write_config(&format!("{LIVE_CONFIG}lsp = false\n"));
    let path = sb.write("notes.txt", "alpha\nbeta\n");
    let mut t = sb.spawn(&["notes.txt"]);
    t.send(" l");
    t.wait_text("Ask about notes.txt");
    t.send("In notes.txt replace the word beta with gamma, using your Edit tool. Do nothing else.\r");
    t.wait_for("the review of Claude's edit", |s| s.contains("accept") && s.contains("reject"));
    t.send("a");
    wait_file_until(&path, "held gamma", LIVE_TIMEOUT, |s| s == "alpha\ngamma\n");
    t.send(" l");
    t.wait_for("the turn to end", chat_idle);
    t.send("\x1b");
    t.wait_for("back in the editor", |s| !s.contains("enter send"));
    t.send(" u");
    t.wait_text("space U brings it back");
    wait_file_until(&path, "back to beta", Duration::from_secs(10), |s| s == "alpha\nbeta\n");
    t.command("q!");
    t.wait_exit();

    let mut t = sb.spawn(&["notes.txt"]);
    t.send(" l");
    t.wait_for("the earlier conversation back", |s| {
        s.contains("continued from") && s.contains("replace the word")
    });
    let s = chat_turn(&mut t, "Which word did I first ask you to replace? Answer with that one word only.");
    let said = s.rows.iter().map(|r| r.rsplit('▐').next().unwrap_or("")).collect::<Vec<_>>().join("\n");
    assert!(said.matches("beta").count() >= 2, "Claude remembers (the question and its answer)\n{s}");
}

/// `C-t` in the chat lists claude's own models; picking one switches it live — the title says so and the
/// next answer comes from it.
#[test]
#[ignore = "live: run with `cargo live` (uses Claude)"]
fn chat_switches_the_model() {
    if !have("claude") {
        return;
    }
    let sb = Sandbox::live("chat-model");
    sb.write_config(&format!("{LIVE_CONFIG}lsp = false\n"));
    sb.write("notes.txt", "alpha\n");
    let mut t = sb.spawn(&["notes.txt"]);
    t.send(" l");
    t.wait_text("Ask about notes.txt");
    t.send("\x14");
    let s = t.wait_for("claude's models listed", |s| s.contains("Haiku") || s.contains("Sonnet"));
    let pick = if s.contains("Haiku") { "Haiku" } else { "Sonnet" };
    t.send(&pick.to_lowercase());
    t.send("\r");
    t.wait_for("the model in the chat's title", |s| {
        s.rows.iter().any(|r| r.contains("claude ›") && r.contains(pick))
    });
    chat_turn(&mut t, "Reply with just the word: ok");
}

/// `space v c`: Claude reviews the uncommitted change — notes on the lines, a verdict in the chat.
#[test]
#[ignore = "live: run with `cargo live` (uses Claude)"]
fn claude_reviews_the_changes() {
    if !have("claude") || !have("git") {
        return;
    }
    let sb = Sandbox::live("review");
    sb.write_config(&format!("{LIVE_CONFIG}lsp = false\n"));
    cargo_project(&sb);
    sb.git(&["init", "-q", "-b", "main"]);
    sb.git(&["add", "-A"]);
    sb.git(&["commit", "-q", "-m", "base"]);
    let lib = sb.file("src/lib.rs");
    let text = std::fs::read_to_string(&lib).unwrap().replace("util::same(a) + b", "util::same(a) - b");
    std::fs::write(&lib, text).unwrap();
    let mut t = sb.spawn(&["src/lib.rs"]);
    t.send(" vc");
    t.wait_for("the review asked for", |s| s.contains("C-c stop") || s.contains("claude ›"));
    t.send(" l");
    t.wait_for("Claude's verdict", chat_idle);
    t.send("\x1b");
    t.wait_for("back in the editor", |s| !s.contains("enter send"));
    t.send(" N");
    t.wait_for("a note on the changed file", |s| s.contains("lib.rs"));
}

/// The git tab's `C`: Claude drafts the commit message from the staged diff, reviewed in the message buffer;
/// `:w` commits it.
#[test]
#[ignore = "live: run with `cargo live` (uses Claude)"]
fn claude_drafts_the_commit_message() {
    if !have("claude") || !have("git") {
        return;
    }
    let sb = Sandbox::live("commit");
    sb.write_config(&format!("{LIVE_CONFIG}lsp = false\n"));
    sb.write("notes.txt", "alpha\n");
    sb.git(&["init", "-q", "-b", "main"]);
    sb.git(&["add", "-A"]);
    sb.git(&["commit", "-q", "-m", "base"]);
    sb.write("notes.txt", "alpha\nbeta\n");
    let mut t = Term::spawn(&sb, &["notes.txt"], 130, ROWS);
    t.send(" gg");
    t.wait_for("the change listed", |s| s.contains("Changes") && s.contains("notes.txt"));
    t.send("s");
    t.wait_text("Staged notes.txt");
    t.send("C");
    t.wait_for("the drafted message as a review", |s| {
        s.contains("COMMIT_EDITMSG") && s.contains("accept") && s.contains("reject")
    });
    t.send("a");
    t.wait_for("the review done", |s| !s.contains("reject"));
    t.command("w");
    t.wait_for("committed — the tree clean", |s| s.contains("nothing to commit"));
    let log = Command::new("git").args(["log", "-1", "--format=%s"]).current_dir(sb.work()).output().unwrap();
    let subject = String::from_utf8_lossy(&log.stdout).trim().to_string();
    assert!(!subject.is_empty() && subject != "base", "a drafted subject: {subject:?}");
}
